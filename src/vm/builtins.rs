//! VM: print, builtin dispatch and arity errors, iterables, the conversion builtins.
//! Split out of `src/vm/stmt.rs`. `super::*` is the parent module.

use super::*;
use crate::checker::Hook;

impl Vm {
    pub(super) fn do_print(&mut self, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        // Keep the args rooted on the operand stack while stringifying — a `Stringable` `str` method
        // runs user code that can GC. `stringify` pushes/pops above `at + argc`, so these indices
        // stay valid across the loop.
        let mut parts = Vec::with_capacity(argc);
        for i in 0..argc {
            let v = self.stack[at + i];
            parts.push(self.stringify(v, span, 0)?);
        }
        self.stack.truncate(at);
        // ONE write (body + newline joined): in stream mode that is ONE locked write → a `print` is
        // line-atomic across tasks. Byte-identical in buffered mode.
        let mut line = parts.join(" ");
        line.push('\n');
        self.emit_out(&line);
        if let Some(halt) = self.stream_halt(span) {
            return Err(halt); // stdout died (closed reader / unwritable) — halt like `os.exit`
        }
        self.push(Value::nil());
        Ok(())
    }

    /// `print(args…, sep=, end=)`. Stack layout on entry: `[args… , sep, end]`. Pops `end` then
    /// `sep` (both `str`, copied out so they're no longer GC roots), stringifies the `argc` user
    /// args (kept rooted on the stack across `stringify`, which can run user code + GC), joins with
    /// `sep` and appends `end`.
    pub(super) fn do_print_sep(&mut self, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let end = self.pop();
        let sep = self.pop();
        let end = self.val_str(end).unwrap_or_default();
        let sep = self.val_str(sep).unwrap_or_default();
        let at = self.stack.len() - argc;
        let mut parts = Vec::with_capacity(argc);
        for i in 0..argc {
            let v = self.stack[at + i];
            parts.push(self.stringify(v, span, 0)?);
        }
        self.stack.truncate(at);
        let mut line = parts.join(&sep);
        line.push_str(&end);
        self.emit_out(&line);
        if let Some(halt) = self.stream_halt(span) {
            return Err(halt);
        }
        self.push(Value::nil());
        Ok(())
    }

    /// Stamps a caught fault's origin onto its message string, unless the fault was raised inside
    /// the stdlib itself (a coordinate the user cannot act on).
    pub(super) fn stamp_err_span(&mut self, msg: Value, sp: Span) {
        if let Some(mh) = msg.as_obj()
            && !self.program.file_is_std(sp.file)
        {
            self.heap.set_err_span(mh, sp);
        }
    }

    /// The origin a `panic(msg)` re-raise should use: the stamp `msg` carries, if any, else `span`
    /// (the `panic` call site itself).
    pub(super) fn panic_origin(&self, arg: Option<Value>, span: Span) -> Span {
        arg.and_then(|v| v.as_obj())
            .and_then(|h| self.heap.err_span(h))
            .unwrap_or(span)
    }

    pub(super) fn do_builtin(
        &mut self,
        name: &str,
        argc: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        // `panic(msg)` raises the SAME recoverable `RuntimeError` (`self.err`) the runtime uses for
        // overflow/OOB/decode, instead of pushing a value — it unwinds (running `defer`s) to the
        // nearest `recover:` as `Err(e)` with `e.message() == msg`, else aborts. Early-return before
        // the value-returning match so nothing is pushed on the (unwinding) path. The checker
        // guarantees a single `str` arg; fall back to the value's type name for a non-str (matches
        // the interp's defensive guard, keeping messages byte-identical across engines).
        if name == "panic" {
            let message = match args.first().copied() {
                Some(v) => match v.as_obj().map(|h| self.heap.get(h)) {
                    Some(Obj::Str(s)) => s.to_string(),
                    _ => self.type_name(v).to_string(),
                },
                None => String::new(),
            };
            let sp = self.panic_origin(args.first().copied(), span);
            return Err(self.err(message, sp).raised_by_panic());
        }
        let result = match name {
            "int" => self.builtin_int(&args, span)?,
            "float" => self.builtin_float(&args, span)?,
            "bool" => self.builtin_bool(&args, span)?,
            "str" => self.builtin_str(&args, span)?,
            "ord" => self.builtin_ord(&args, span)?,
            "chr" => self.builtin_chr(&args, span)?,
            "Set" => self.builtin_set(&args, span)?,
            "List" => self.builtin_list(&args, span)?,
            "Map" => self.builtin_map(&args, span)?,
            "bytearray" => self.builtin_bytearray(&args, span)?,
            "bytes" => self.builtin_bytes(&args, span)?,
            _ => unreachable!("unknown builtin {name}"),
        };
        self.push(result);
        Ok(())
    }

    pub(super) fn arity_err(
        &self,
        name: &str,
        args: &[Value],
        n: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if args.len() == n {
            Ok(())
        } else {
            Err(self.err(
                format!("{name}() expects {n} argument(s), got {}", args.len()),
                span,
            ))
        }
    }

    /// D6c — arity check for a method that accepts an inclusive `min..=max` argument range (the net
    /// socket ops: `read`/`write` take 1–2, `accept` 0–1 — the optional trailing `timeout_ms`).
    pub(super) fn arity_range_err(
        &self,
        name: &str,
        args: &[Value],
        min: usize,
        max: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if (min..=max).contains(&args.len()) {
            Ok(())
        } else {
            Err(self.err(
                format!(
                    "{name}() expects {min}–{max} argument(s), got {}",
                    args.len()
                ),
                span,
            ))
        }
    }

    /// D6c — parse the optional trailing `timeout_ms` int arg of a net socket op. `Ok(None)` if no
    /// timeout arg was passed (park forever — the existing behavior). `Ok(Some(Timeout))` otherwise:
    /// `poll_once` is true iff `ms <= 0` (`0` polls once and never parks; a negative saturates to it),
    /// and `deadline` is `now + ms`, saturated to a far-future deadline for a pathological `ms`
    /// (centuries) rather than panicking the worker on `Instant` overflow (mirrors `sleep_ms`). `Err`
    /// for a non-int timeout arg (the checker also rejects this; this is the runtime backstop).
    pub(super) fn parse_timeout_ms(
        &self,
        arg: Option<&Value>,
        span: Span,
    ) -> Result<Option<SockTimeout>, RuntimeError> {
        let Some(v) = arg else {
            return Ok(None);
        };
        let Some(ms) = self.int_val(*v) else {
            return Err(self.err("timeout_ms expects an int (milliseconds)".into(), span));
        };
        let poll_once = ms <= 0;
        let ms = ms.max(0) as u64;
        let dur = std::time::Duration::from_millis(ms);
        let deadline = std::time::Instant::now()
            .checked_add(dur)
            .unwrap_or_else(|| {
                std::time::Instant::now() + std::time::Duration::from_secs(86_400 * 365)
            });
        Ok(Some(SockTimeout {
            poll_once,
            deadline,
        }))
    }

    /// One-time `Iterable` → cursor conversion: a struct or enum with the `iter` hook becomes its
    /// cursor by running `iter(self)`; EVERYTHING else passes through unchanged (so it is safe to
    /// call on any value). Shared by `Op::IterableToCursor` (the `for` lowering) and
    /// [`drain_iterable`](Self::drain_iterable) (`List()`/`Set()`/`Map()`/`.iter()`). `iter` wins
    /// over `next`: a type with both hooks is iterated through `iter`.
    pub(super) fn iterable_to_cursor(
        &mut self,
        v: Value,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        Ok(self
            .reentered(|vm| vm.invoke_hook(Hook::Iter, v, &[], span))?
            .unwrap_or(v))
    }

    /// Drain ANY for-iterable into a `Vec<Value>` of its elements — the runtime peer of the checker's
    /// `iter_elem` (the single source of truth for "what `for x in X` accepts"). Built-in collections
    /// copy their elements directly (list/set elems, str→per-char str, bytes/bytearray→per-byte int,
    /// map→keys, range is already materialized to a list); a `Generator` is driven via `generator_next`
    /// until `None`; a user struct with `next(self) -> Option[T]` re-enters the VM (`run_proto`) per
    /// step until `None`. Both re-entrant paths run user code that can GC, so the growing accumulator
    /// is built into a heap `Obj::List` ROOTED on the operand stack across every `.next()` (mirrors
    /// `builtin_set`/`list_hof`/`struct_hash`). The source handle is rooted too. Returns the collected
    /// elements (cloned out of the rooted list after the loop, GC-safe).
    pub(super) fn drain_iterable(
        &mut self,
        v: Value,
        span: Span,
    ) -> Result<Vec<Value>, RuntimeError> {
        // A pure-`Iterable` struct (`iter`, no `next`) is converted ONCE to its cursor first — the same
        // step the `for` lowering emits as `Op::IterableToCursor`. Without it `List(xs)`/`Set(xs)`/
        // `Map(xs)` on an `Iterable[T]`-annotated param would type-check and then fault on that witness.
        let v = self.iterable_to_cursor(v, span)?;
        // A cursor (`Obj::Iter`) is CONSUMED in place: clone its REMAINING items (`items[pos..]`),
        // then advance `pos` to the end so the same cursor yields nothing on a second drain — keeping
        // `List(it)`/`Set(it)`/`for` consistent with `.next()` (which also advances the shared cursor)
        // and with the docs ("reusing one exhausted cursor yields nothing on a second pass"). Lifted
        // OUT of the immutable-borrow `match self.heap.get(h)` below so `get_mut` can advance `pos`.
        if let Some(h) = v.as_obj()
            && matches!(self.heap.get(h), Obj::Iter { .. })
        {
            let Obj::Iter { items, pos } = self.heap.get_mut(h) else {
                unreachable!()
            };
            let start = (*pos).min(items.len());
            let drained = items[start..].to_vec();
            *pos = items.len();
            return Ok(drained);
        }
        // Built-in collections: copy directly, no re-entry.
        if let Some(h) = v.as_obj() {
            match self.heap.get(h) {
                Obj::List(items) => return Ok(items.clone()),
                Obj::Set(s) => return Ok(s.entries.iter().map(|(_, e)| *e).collect()),
                Obj::Map(_) => return Ok(self.map_pairs(h)),
                Obj::Bytes(b) => {
                    let bytes = b.clone();
                    return Ok(bytes.iter().map(|&x| Value::int(x as i64)).collect());
                }
                Obj::ByteArray(b) => {
                    let bytes = b.clone();
                    return Ok(bytes.iter().map(|&x| Value::int(x as i64)).collect());
                }
                Obj::Str(s) => {
                    let chars: Vec<char> = s.chars().collect();
                    return Ok(chars.into_iter().map(|c| self.alloc_char(c)).collect());
                }
                // (`Obj::Iter` cursors are handled above by the consume-in-place guard.)
                Obj::BigInt(_)
                | Obj::FloatBox(_)
                | Obj::Iter { .. }
                | Obj::Tuple(_)
                | Obj::Struct { .. }
                | Obj::Enum { .. }
                | Obj::Cell(_)
                | Obj::Func { .. }
                | Obj::Closure { .. }
                | Obj::Module(_)
                | Obj::Native { .. }
                | Obj::Builtin(_)
                | Obj::Cffi(_)
                | Obj::Ptr(_)
                | Obj::Channel(_)
                | Obj::Shared(_)
                | Obj::RwShared(_)
                | Obj::Atomic(_)
                | Obj::AtomicInt(_)
                | Obj::Executor(_)
                | Obj::Socket(_)
                | Obj::Listener(_)
                | Obj::Writer(_)
                | Obj::Reader(_)
                | Obj::Generator(_) => {}
            }
        }
        // Re-entrant paths (generator / user struct iterator): run user `.next()` until `None`, rooting
        // the source + a heap accumulator list on the operand stack across each call.
        let Some(h) = v.as_obj() else {
            return Err(self.err(format!("cannot iterate over {}", self.type_name(v)), span));
        };
        // Resolve the iteration step: a generator resumes; a struct or enum runs its `next` hook.
        enum Step {
            Generator,
            NextHook,
            Channel,
        }
        let step = match self.heap.get(h) {
            Obj::Generator(_) => Step::Generator,
            Obj::Struct { .. } | Obj::Enum { .. } if self.hook_of(v, Hook::Next) => Step::NextHook,
            // A channel drains until it is closed and empty (Rust `rx.iter().collect()`).
            Obj::Channel(_) => Step::Channel,
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::List(_)
            | Obj::Tuple(_)
            | Obj::Map(_)
            | Obj::Set(_)
            | Obj::Struct { .. }
            | Obj::Enum { .. }
            | Obj::Cell(_)
            | Obj::Func { .. }
            | Obj::Closure { .. }
            | Obj::Module(_)
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Reader(_) => {
                return Err(self.err(format!("cannot iterate over {}", self.type_name(v)), span));
            }
        };
        // Root the source + the growing accumulator list across the re-entrant calls.
        let acc = self.heap.alloc(Obj::List(Vec::new()));
        self.push(v);
        self.push(Value::obj(acc));
        let result = (|| {
            loop {
                let res = match step {
                    // One value per receive, through the same entry `recv` and `for v in ch` use:
                    // under `guarded` an M:N fiber demotes (blocks in place) instead of parking,
                    // so no `ip` rewind replays the surrounding `List(...)` call (DEC-181).
                    Step::Channel => {
                        let w = match self.guarded(|vm| vm.recv_step_or_demote(h, span))? {
                            RecvStep::Got(w) => {
                                self.wake_senders(h); // freed a slot — wake a parked bounded sender
                                w
                            }
                            RecvStep::Filled(w) => w,
                            RecvStep::ClosedEmpty => break,
                            RecvStep::Parked => unreachable!("a re-entered recv never parks"),
                        };
                        let item = self.from_wire(w);
                        let Obj::List(buf) = self.heap.get_mut(acc) else {
                            unreachable!()
                        };
                        buf.push(item);
                        continue;
                    }
                    Step::Generator => self.generator_next(h, span)?,
                    Step::NextHook => self
                        .guarded(|vm| vm.invoke_hook(Hook::Next, v, &[], span))?
                        .unwrap_or(Value::nil()),
                };
                let Some(rh) = res.as_obj() else {
                    return Err(self.bad_next_value(res, span));
                };
                let Obj::Enum {
                    variant_id,
                    payload,
                } = self.heap.get(rh)
                else {
                    return Err(self.bad_next_value(res, span));
                };
                match *variant_id {
                    crate::vm::op::VID_SOME => {
                        let item = *payload.first().ok_or_else(|| {
                            self.err(
                                "iterator next() returned a present value with no payload"
                                    .to_string(),
                                span,
                            )
                        })?;
                        let Obj::List(buf) = self.heap.get_mut(acc) else {
                            unreachable!()
                        };
                        buf.push(item);
                    }
                    crate::vm::op::VID_NONE_VARIANT => break,
                    _ => {
                        return Err(self.bad_next_value(res, span));
                    }
                }
            }
            Ok(())
        })();
        result?;
        // Clone the collected elements out of the rooted list before unrooting.
        let Obj::List(buf) = self.heap.get(acc) else {
            unreachable!()
        };
        let out = buf.clone();
        self.pop(); // unroot accumulator
        self.pop(); // unroot source
        Ok(out)
    }

    /// `Set()` → empty set; `Set(it)` → a deduped hash set drained from ANY for-iterable.
    pub(super) fn builtin_set(
        &mut self,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let src: Vec<Value> = match args {
            [] => Vec::new(),
            [one] => self.drain_iterable(*one, span)?,
            _ => {
                return Err(self.err(
                    format!("Set() expects 0 or 1 argument(s), got {}", args.len()),
                    span,
                ));
            }
        };
        // Root the source elements (as a fresh PRIVATE heap list) so they survive a struct
        // element's re-entrant hash() GC. Phase 0 snapshots each struct/enum element IN
        // PLACE in that rooted list (Go value-key model — a later mutation of the caller's original
        // can't corrupt the set); the list is ours, so overwriting is safe, and rooting the
        // snapshots there keeps them alive across the phase-1 re-entrant hashes. Then hash (phase 1)
        // and build GC-free (phase 2), reading the snapshots from the list.
        let lh = self.heap.alloc(Obj::List(src.clone()));
        self.push(Value::obj(lh));
        let n = src.len();
        let built = (|| {
            for i in 0..n {
                let orig = {
                    let Obj::List(b) = self.heap.get(lh) else {
                        unreachable!()
                    };
                    b[i]
                };
                let snap = self.snapshot_key(orig);
                if let Obj::List(b) = self.heap.get_mut(lh) {
                    b[i] = snap;
                }
            }
            let mut hashes = Vec::with_capacity(n);
            for i in 0..n {
                let v = {
                    let Obj::List(b) = self.heap.get(lh) else {
                        unreachable!()
                    };
                    b[i]
                };
                hashes.push(self.hash_value(v, span)?);
            }
            let mut set = SetData::default();
            for (i, &he) in hashes.iter().enumerate() {
                let v = {
                    let Obj::List(b) = self.heap.get(lh) else {
                        unreachable!()
                    };
                    b[i]
                };
                if self
                    .set_slot(&set.entries, set.candidates(he), v, span)?
                    .is_none()
                {
                    set.push(he, v);
                }
            }
            Ok(set)
        })();
        self.pop(); // unroot the source list
        Ok(Value::obj(self.heap.alloc(Obj::Set(built?))))
    }

    /// `List()` → a fresh empty list (the `List[T]()` turbofish form; mirrors `Set()`); `List(it)` →
    /// a list drained from ANY for-iterable.
    pub(super) fn builtin_list(
        &mut self,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let items = match args {
            [] => Vec::new(),
            [one] => self.drain_iterable(*one, span)?,
            _ => {
                return Err(self.err(
                    format!("List() expects 0 or 1 argument(s), got {}", args.len()),
                    span,
                ));
            }
        };
        Ok(Value::obj(self.heap.alloc(Obj::List(items))))
    }

    /// `Map(it)` → a map from an iterable of 2-tuples `(k, v)` (last-wins on dup keys, like the
    /// `{k: v}` literal). A struct key's `hash()` re-enters the
    /// VM, so the in-flight key/value are rooted via `hash_key_rooted` while the building map is rooted.
    pub(super) fn builtin_map(
        &mut self,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let one = match args {
            // `Map()` → a fresh empty map (the `Map[K, V]()` turbofish form; mirrors `Set()`).
            [] => return Ok(Value::obj(self.heap.alloc(Obj::Map(MapData::default())))),
            [one] => one,
            _ => {
                return Err(self.err(
                    format!("Map() expects 0 or 1 argument(s), got {}", args.len()),
                    span,
                ));
            }
        };
        let it = *one;
        // `Map(m)` over a map yields a COPY of it (a map iterates (K, V) pairs, but the copy
        // does not rebuild from them).
        if let Some(h) = it.as_obj()
            && let Obj::Map(inner) = self.heap.get(h)
        {
            let copy = inner.clone();
            return Ok(Value::obj(self.heap.alloc(Obj::Map(copy))));
        }
        let drained = self.drain_iterable(it, span)?;
        // Root the drained elements (as a fresh heap list) across the re-entrant hash() calls.
        let src_obj = Value::obj(self.heap.alloc(Obj::List(drained.clone())));
        self.push(src_obj);
        // Snapshot keys are rooted by pushing them onto the operand stack (the drained tuples may
        // ALIAS caller-held tuples, so they cannot be mutated in place). Everything above this base
        // is popped after the build, on both the Ok and Err path.
        let stack_base = self.stack.len();
        let built = (|| {
            let mut map = MapData::default();
            for elem in &drained {
                let (k, v) = match elem.as_obj().map(|eh| self.heap.get(eh)) {
                    Some(Obj::Tuple(parts)) if parts.len() == 2 => (parts[0], parts[1]),
                    _ => {
                        return Err(self.err(
                            format!(
                                "Map() expects an iterable of (key, value) 2-tuples, got {}",
                                self.type_name(*elem)
                            ),
                            span,
                        ));
                    }
                };
                // Snapshot a struct/enum KEY on insert (Go value-key model); root it on the
                // operand stack for the rest of the build (the later `hash_key_rooted` re-enters the
                // VM → GC). Values stay by-reference (rooted via src_obj). The snapshot is `==` the
                // original, so its hash is unchanged.
                let k = self.snapshot_key(k);
                self.push(k);
                let hk = self.hash_key_rooted(k, &[v], span)?;
                // last-wins upsert (mirrors the map literal + interp `map_upsert`).
                let pos = self.map_slot(&map.entries, map.candidates(hk), k, span)?;
                match pos {
                    Some(p) => map.entries[p].2 = v,
                    None => map.push(hk, k, v),
                }
            }
            Ok(map)
        })();
        self.stack.truncate(stack_base); // unroot the snapshot keys (Ok or Err)
        self.pop(); // unroot the source list
        Ok(Value::obj(self.heap.alloc(Obj::Map(built?))))
    }

    /// Collect raw bytes from a byte-sequence-shaped argument for the `bytes`/`bytearray`
    /// constructors: a `bytes`, a `bytearray` (copy), or a `List[int]` (each element 0..=255, else a
    /// recoverable fault). The `what` label names the constructor in error messages.
    pub(super) fn collect_bytes_arg(
        &self,
        what: &str,
        v: Value,
        span: Span,
    ) -> Result<Vec<u8>, RuntimeError> {
        match v.as_obj().map(|h| self.heap.get(h)) {
            Some(Obj::Bytes(b)) => Ok(b.to_vec()),
            Some(Obj::ByteArray(b)) => Ok(b.clone()),
            Some(Obj::List(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for e in items {
                    match self.int_val(*e) {
                        Some(n) if (0..=255).contains(&n) => out.push(n as u8),
                        Some(n) => {
                            return Err(self.err(
                                format!("{what}() list element {n} out of range (must be 0..=255)"),
                                span,
                            ));
                        }
                        None => {
                            return Err(self.err(
                                format!(
                                    "{what}() expects a list of int, got an element of type {}",
                                    self.type_name(*e)
                                ),
                                span,
                            ));
                        }
                    }
                }
                Ok(out)
            }
            _ => Err(self.err(
                format!(
                    "{what}() expects a bytes, a bytearray, or a List[int], got {}",
                    self.type_name(v)
                ),
                span,
            )),
        }
    }

    /// `bytearray()` → empty; `bytearray(N)` → N zero bytes (Python); `bytearray(b)` → mutable copy of
    /// a `bytes`; `bytearray(ba)` → copy of another `bytearray`; `bytearray([ints])` → from a list of
    /// ints (each 0..=255). The MUTABLE buffer (`Obj::ByteArray`, in-place-mutated via the heap slot).
    pub(super) fn builtin_bytearray(
        &mut self,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let bytes: Vec<u8> = match args {
            [] => Vec::new(),
            [one] if self.int_val(*one).is_some() => {
                let n = self.int_val(*one).unwrap();
                if n < 0 {
                    return Err(
                        self.err(format!("bytearray() size {n} must be non-negative"), span)
                    );
                }
                // Bound the eager zero-fill: an unguarded `vec![0u8; n]` for a huge n aborts the
                // process (SIGABRT), uncatchable by `recover:`. `try_reserve` turns OOM into a
                // recoverable fault, matching range()/format-width's "never a giant abort" rule —
                // without a hard cap, so legitimately large buffers still work.
                let n = n as usize;
                let mut buf: Vec<u8> = Vec::new();
                if buf.try_reserve_exact(n).is_err() {
                    return Err(self.err(
                        format!("bytearray() size {n} is too large to allocate"),
                        span,
                    ));
                }
                buf.resize(n, 0u8);
                buf
            }
            [one] => self.collect_bytes_arg("bytearray", *one, span)?,
            _ => {
                return Err(self.err(
                    format!("bytearray() expects 0 or 1 argument(s), got {}", args.len()),
                    span,
                ));
            }
        };
        Ok(Value::obj(self.heap.alloc(Obj::ByteArray(bytes))))
    }

    /// `bytes(b)` → copy; `bytes(ba)` → immutable SNAPSHOT of a `bytearray`; `bytes([ints])` → from a
    /// list of ints. The conversion bridge to the IMMUTABLE form (the other being the `b"..."` literal).
    pub(super) fn builtin_bytes(
        &mut self,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let bytes: Vec<u8> = match args {
            [one] => self.collect_bytes_arg("bytes", *one, span)?,
            _ => {
                return Err(self.err(
                    format!("bytes() expects 1 argument, got {}", args.len()),
                    span,
                ));
            }
        };
        Ok(Value::obj(
            self.heap.alloc(Obj::Bytes(bytes.into_boxed_slice())),
        ))
    }

    pub(super) fn builtin_int(
        &mut self,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        self.arity_err("int", args, 1, span)?;
        let v = args[0];
        if self.is_integral(v) {
            // `int(int)` is identity — inline or boxed big-int, returned unchanged.
            return Ok(v);
        }
        if v.is_float() {
            let f = self.float_of(v);
            if !f.is_finite() || f < i64::MIN as f64 || f >= 9_223_372_036_854_775_808.0 {
                return Err(self.err(format!("int(): {f} is out of integer range"), span));
            }
            return Ok(self.make_int(f as i64));
        }
        if let Some(b) = v.as_bool() {
            return Ok(Value::int(i64::from(b)));
        }
        if let Some(h) = v.as_obj()
            && let Obj::Str(s) = self.heap.get(h)
        {
            let s = s.to_string();
            return match parse_i64_pep515(s.trim()) {
                Ok(n) => Ok(self.make_int(n)),
                Err(IntParseErr::Overflow) => Err(self.err(
                    format!(
                        "int(): '{s}' overflows i64 (range -9223372036854775808..=9223372036854775807)"
                    ),
                    span,
                )),
                Err(IntParseErr::Malformed) => {
                    Err(self.err(format!("int(): cannot parse '{s}' as an integer"), span))
                }
            };
        }
        Err(self.err(format!("int() cannot convert {}", self.type_name(v)), span))
    }

    pub(super) fn builtin_float(
        &mut self,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        self.arity_err("float", args, 1, span)?;
        let v = args[0];
        if v.is_float() {
            return Ok(v);
        }
        if self.is_integral(v) {
            let f = self.int_of(v) as f64;
            return Ok(self.box_float(f));
        }
        if let Some(b) = v.as_bool() {
            return Ok(self.box_float(f64::from(b)));
        }
        if let Some(h) = v.as_obj()
            && let Obj::Str(s) = self.heap.get(h)
        {
            let s = s.to_string();
            return match strip_num_underscores(s.trim()).and_then(|t| t.parse::<f64>().ok()) {
                Some(f) => Ok(self.box_float(f)),
                None => Err(self.err(format!("float(): cannot parse '{s}' as a float"), span)),
            };
        }
        Err(self.err(
            format!("float() cannot convert {}", self.type_name(v)),
            span,
        ))
    }

    /// `bool(x)` — total truthiness cast over the scalars. Never faults on int/float/bool/str:
    /// int 0 -> false else true; float 0.0/-0.0 -> false, NaN -> true
    /// (Rust `f != 0.0` is already false for both zeros and true for NaN — matches Python), else
    /// true; bool -> identity; str "" -> false else true (non-empty is truthy — NOT a parse, so
    /// `bool(" ")` is true). A non-scalar arg faults exactly like `int()`/`float()`.
    pub(super) fn builtin_bool(
        &mut self,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        self.arity_err("bool", args, 1, span)?;
        let v = args[0];
        if let Some(n) = self.int_val(v) {
            return Ok(Value::bool(n != 0));
        }
        if v.is_float() {
            return Ok(Value::bool(self.float_of(v) != 0.0));
        }
        if let Some(b) = v.as_bool() {
            return Ok(Value::bool(b));
        }
        if let Some(h) = v.as_obj()
            && let Obj::Str(s) = self.heap.get(h)
        {
            return Ok(Value::bool(!s.is_empty()));
        }
        Err(self.err(format!("bool() cannot convert {}", self.type_name(v)), span))
    }

    pub(super) fn builtin_str(
        &mut self,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        self.arity_err("str", args, 1, span)?;
        let s = self.stringify(args[0], span, 0)?;
        Ok(Value::obj(self.heap.alloc(Obj::Str(s.into()))))
    }

    /// `ord(s)` — codepoint of the ONE character of `s`.
    ///
    /// The length that matters is CHARACTERS, not bytes: `ord("é")` is 233 (one char, two bytes) and
    /// `ord("éa")` faults. A multi-char string used to return the first char's codepoint silently
    /// (measured: `ord("ab")` → 97), where CPython 3.14 raises `TypeError: ord() expected a
    /// character, but string of length 2 found` — a plausible wrong value with rc=0, so it faulted
    /// nothing and reported nothing. Now it faults like the ancestor (recoverably, via `recover:`).
    pub(super) fn builtin_ord(
        &mut self,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        self.arity_err("ord", args, 1, span)?;
        let v = args[0];
        if let Some(h) = v.as_obj()
            && let Obj::Str(s) = self.heap.get(h)
        {
            let mut it = s.chars();
            return match (it.next(), it.next()) {
                (Some(c), None) => Ok(Value::int(c as i64)),
                (None, _) => Err(self.err("ord() of an empty string".to_string(), span)),
                (Some(_), Some(_)) => {
                    let n = s.chars().count();
                    Err(self.err(
                        format!("ord() expects a 1-character str, got {n} characters"),
                        span,
                    ))
                }
            };
        }
        Err(self.err(
            format!("ord() expects a str, got {}", self.type_name(v)),
            span,
        ))
    }

    /// `chr(n)` — the 1-char str for codepoint `n`.
    pub(super) fn builtin_chr(
        &mut self,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        self.arity_err("chr", args, 1, span)?;
        let v = args[0];
        match self.int_val(v) {
            Some(n) => u32::try_from(n)
                .ok()
                .and_then(char::from_u32)
                .map(|c| self.alloc_char(c))
                .ok_or_else(|| {
                    self.err(format!("chr(): {n} is not a valid Unicode codepoint"), span)
                }),
            None => Err(self.err(
                format!("chr() expects an int, got {}", self.type_name(v)),
                span,
            )),
        }
    }

    // ----- module namespace helpers -----
}
