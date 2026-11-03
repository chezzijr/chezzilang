//! VM: print, builtin dispatch and arity errors, iterables, the conversion builtins.
//! Split out of `src/vm/stmt.rs`. `super::*` is the parent module.

use super::*;
use crate::base::builtin::Builtin;
use crate::base::builtin_method::NativeStatic;
use crate::base::hooks::Hook;

impl Vm {
    /// `print` joins the `argc` args on the stack top with `sep` and appends `end`: the one print
    /// routine. The args stay rooted on the operand stack across `stringify`, which can run user
    /// code (a `Stringable` `str` method) and GC; its pushes sit above `at + argc`, so these indices
    /// stay valid across the loop. Pushes `nil`.
    pub(super) fn do_print_sep(
        &mut self,
        argc: usize,
        sep: &str,
        end: &str,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let mut parts = Vec::with_capacity(argc);
        for i in 0..argc {
            let v = self.stack[at + i];
            parts.push(self.stringify(v, span, 0)?);
        }
        self.stack.truncate(at);
        // ONE write (body + end joined): in stream mode that is ONE locked write, so a `print` is
        // line-atomic across tasks. Byte-identical in buffered mode.
        let mut line = parts.join(sep);
        line.push_str(end);
        self.emit_out(&line);
        if let Some(halt) = self.stream_halt(span) {
            return Err(halt); // stdout died (closed reader / unwritable): halt like `os.exit`
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

    /// The error `panic(msg)` raises: the SAME recoverable `RuntimeError` (`self.err`) the runtime
    /// uses for overflow/OOB/decode, so it unwinds (running `defer`s) to the nearest `recover:` as
    /// `Err(e)` with `e.message() == msg`, else aborts. The checker guarantees a single `str` arg;
    /// a non-str falls back to the value's type name. Shared by the direct call and the value call.
    pub(super) fn builtin_panic(&mut self, args: &[Value], span: Span) -> RuntimeError {
        let message = match args.first().copied() {
            Some(v) => match v.as_obj().map(|h| self.heap.get(h)) {
                Some(Obj::Str(s)) => s.to_string(),
                _ => self.type_name(v).to_string(),
            },
            None => String::new(),
        };
        let sp = self.panic_origin(args.first().copied(), span);
        self.err(message, sp).raised_by_panic()
    }

    pub(super) fn do_builtin(
        &mut self,
        b: Builtin,
        argc: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let result = match b {
            // Unwinds instead of pushing a value.
            Builtin::Panic => return Err(self.builtin_panic(&args, span)),
            Builtin::Ord => self.builtin_ord(&args, span)?,
            Builtin::Chr => self.builtin_chr(&args, span)?,
            // A call on a builtin type's name makes the empty or zero value; the checker admits no
            // argument.
            Builtin::Int => Value::int(0),
            Builtin::Float => self.box_float(0.0),
            Builtin::Bool => Value::bool(false),
            Builtin::Str => self.alloc_str(String::new()),
            Builtin::Bytes => Value::obj(self.heap.alloc(Obj::Bytes(Box::default()))),
            Builtin::Set => Value::obj(self.heap.alloc(Obj::Set(SetData::default()))),
            Builtin::List => Value::obj(self.heap.alloc(Obj::List(Vec::new().into()))),
            Builtin::Map => Value::obj(self.heap.alloc(Obj::Map(MapData::default()))),
            Builtin::ByteArray => Value::obj(self.heap.alloc(Obj::ByteArray(RVec::new()))),
            // `Lowering::Print` and `Lowering::Opcode` builtins have opcodes of their own.
            Builtin::Print
            | Builtin::Channel
            | Builtin::Shared
            | Builtin::RwShared
            | Builtin::Atomic
            | Builtin::AtomicInt
            | Builtin::Executor => unreachable!("{} has its own opcode", b.name()),
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

    /// D6c — the `timeout_ms: int? = None` argument (index `idx`) of a net socket op. The call site
    /// fills an omitted `None` from the decl, so the argument is always there. `Ok(None)` for `None`
    /// (park until done). `Ok(Some(Timeout))` for an int: `poll_once` is true iff `ms <= 0` (`0`
    /// polls once and never parks; a negative saturates to it), and `deadline` is `now + ms`,
    /// saturated to a far-future deadline for a pathological `ms` (centuries) rather than panicking
    /// the worker on `Instant` overflow (mirrors `sleep_ms`). `Err` for a missing or non-`int?`
    /// argument (the checker rejects both; this is the runtime backstop).
    pub(super) fn parse_timeout_ms(
        &self,
        args: &[Value],
        idx: usize,
        span: Span,
    ) -> Result<Option<SockTimeout>, RuntimeError> {
        let bad = || self.err("timeout_ms expects an int? (milliseconds)".into(), span);
        let Some(&v) = args.get(idx) else {
            return Err(bad());
        };
        let Some(ms) = self.copy_arg(v).opt_int(idx).map_err(|_| bad())? else {
            return Ok(None);
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

    /// What a builtin collection yields per step, as a snapshot: list and set elements, map
    /// `(key, value)` pairs, a str's 1-char strs, a bytes' or bytearray's ints (0-255), and a
    /// cursor's remaining items (not advanced). `None` for every other object. The one runtime row
    /// table: the `for` lowering (`Op::ListClone`), [`drain_iterable`](Self::drain_iterable) and the
    /// builtin `.iter()` all read it. Collected straight into the caller's container `C` (a
    /// `Vec` or a heap object's `RVec`), so a big list is copied once, not through a temporary.
    pub(super) fn builtin_items<C: FromIterator<Value>>(&mut self, h: GcRef) -> Option<C> {
        match self.heap.get(h) {
            Obj::List(items) => Some(items.iter().collect()),
            Obj::Set(s) => Some(s.entries.iter().map(|(_, e)| e).collect()),
            Obj::Map(_) => Some(self.map_pairs(h).into_iter().collect()),
            Obj::Bytes(b) => Some(b.iter().map(|&x| Value::int(x as i64)).collect()),
            Obj::ByteArray(b) => Some(b.iter().map(|x| Value::int(x as i64)).collect()),
            Obj::Str(s) => {
                let chars: Vec<char> = s.chars().collect();
                Some(chars.into_iter().map(|c| self.alloc_char(c)).collect())
            }
            Obj::Iter { items, pos } => Some(items.iter().skip(pos.get()).collect()),
            Obj::BigInt(_)
            | Obj::FloatBox(_)
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
            | Obj::Generator(_) => None,
        }
    }

    /// Drain ANY for-iterable into a `Vec<Value>` of its elements — the runtime peer of the checker's
    /// `iterable_elem` (the owner of "what `for x in X` yields"). A built-in collection reads its
    /// [`builtin_items`](Self::builtin_items) row; a `Generator` is driven via `generator_next`
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
        // OUT of the `match self.heap.get(h)` below so the write door arms the `pos` store.
        if let Some(h) = v.as_obj()
            && matches!(self.heap.get(h), Obj::Iter { .. })
        {
            let Obj::Iter { items, pos } = self.heap.write(h).get(h) else {
                unreachable!()
            };
            let drained = items.iter().skip(pos.get()).collect();
            pos.set(items.len());
            return Ok(drained);
        }
        // Built-in collections: copy directly, no re-entry.
        if let Some(items) = v.as_obj().and_then(|h| self.builtin_items(h)) {
            return Ok(items);
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
        let acc = self.heap.alloc(Obj::List(RVec::new()));
        self.push(v);
        self.push(Value::obj(acc));
        let result = (|| {
            loop {
                let res = match step {
                    // One value per receive, through the same entry `recv` and `for v in ch` use:
                    // under `guarded` an M:N fiber demotes (blocks in place) instead of parking,
                    // so no `ip` rewind replays the surrounding `List(...)` call (DEC-181).
                    Step::Channel => {
                        let item = match self.guarded(|vm| vm.recv_step_or_demote(h, span))? {
                            // `recv_step_or_demote` woke a sender for `Got`; a filled slot committed none.
                            RecvStep::Got(v) | RecvStep::Filled(v) => v,
                            RecvStep::ClosedEmpty => break,
                            RecvStep::Parked => unreachable!("a re-entered recv never parks"),
                        };
                        let heap = self.heap.write(acc);
                        let Obj::List(buf) = heap.get(acc) else {
                            unreachable!()
                        };
                        buf.push(heap, item);
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
                    crate::base::op::VID_SOME => {
                        let item = payload.try_get(0).ok_or_else(|| {
                            self.err(
                                "iterator next() returned a present value with no payload"
                                    .to_string(),
                                span,
                            )
                        })?;
                        let heap = self.heap.write(acc);
                        let Obj::List(buf) = heap.get(acc) else {
                            unreachable!()
                        };
                        buf.push(heap, item);
                    }
                    crate::base::op::VID_NONE_VARIANT => break,
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
        let out = buf.to_vec();
        self.pop(); // unroot accumulator
        self.pop(); // unroot source
        Ok(out)
    }

    /// Run a builtin type's static method (`List.from_iter(xs)`) on the `argc` args on the stack
    /// top, which the checker matched to the decl. Pushes the result.
    pub(super) fn do_native_static(
        &mut self,
        which: NativeStatic,
        argc: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let (ty, method) = which.key();
        // A fault names the call that faulted (`bytes.zeroed() size -1 must be non-negative`).
        let what = format!("{ty}.{method}");
        // Every argument is there: a direct call fills the decl's defaults at the call site, and the
        // static's value fills them in its prologue (`build_native_static_fn`).
        if args.len() != which.arity() {
            return Err(self.err(
                format!(
                    "{ty}.{method}() expects {} argument(s), got {}",
                    which.arity(),
                    args.len()
                ),
                span,
            ));
        }
        let arg = args[0];
        let result = match which {
            NativeStatic::ListFromIter => {
                let items = self.drain_iterable(arg, span)?;
                Value::obj(self.heap.alloc(Obj::List(items.into())))
            }
            NativeStatic::SetFromIter => self.set_from_iter(arg, span)?,
            NativeStatic::MapFromIter => self.map_from_iter(arg, span)?,
            NativeStatic::BytesFromIter => {
                let bytes = self.bytes_from_iter(&what, arg, span)?;
                Value::obj(self.heap.alloc(Obj::Bytes(bytes.into_boxed_slice())))
            }
            NativeStatic::BytearrayFromIter => {
                let bytes = self.bytes_from_iter(&what, arg, span)?;
                Value::obj(self.heap.alloc(Obj::ByteArray(bytes.into())))
            }
            NativeStatic::BytesZeroed => {
                let n = self.int_val(arg).unwrap_or_default();
                let bytes = self.zero_filled(&what, n, span, super::rvec::try_zeroed_bytes)?;
                Value::obj(self.heap.alloc(Obj::Bytes(bytes)))
            }
            NativeStatic::BytearrayZeroed => {
                let n = self.int_val(arg).unwrap_or_default();
                let bytes = self.zero_filled(&what, n, span, RVec::try_zeroed)?;
                Value::obj(self.heap.alloc(Obj::ByteArray(bytes)))
            }
            NativeStatic::IntFromBeBytes | NativeStatic::IntFromLeBytes => {
                let Some(Obj::Bytes(b)) = arg.as_obj().map(|h| self.heap.get(h)) else {
                    return Err(self.err(format!("{what}() expects bytes"), span));
                };
                let Some(signed) = args[1].as_bool() else {
                    return Err(self.err(format!("{what}() expects a bool signed"), span));
                };
                let big = matches!(which, NativeStatic::IntFromBeBytes);
                match super::core_method::int_from_bytes(method, b, signed, big) {
                    Ok(n) => {
                        let nv = self.make_int(n);
                        self.alloc_enum("Result", "Ok", &[nv])
                    }
                    Err(text) => {
                        let msg = self.alloc_str(text);
                        self.alloc_enum("Result", "Err", &[msg])
                    }
                }
            }
        };
        self.push(result);
        Ok(())
    }

    /// `Set.from_iter(xs)` → a deduped hash set drained from ANY for-iterable.
    fn set_from_iter(&mut self, it: Value, span: Span) -> Result<Value, RuntimeError> {
        let src: Vec<Value> = self.drain_iterable(it, span)?;
        // Root the source elements (as a fresh PRIVATE heap list) so they survive a struct
        // element's re-entrant hash() GC. Phase 0 snapshots each struct/enum element IN
        // PLACE in that rooted list (Go value-key model — a later mutation of the caller's original
        // can't corrupt the set); the list is ours, so overwriting is safe, and rooting the
        // snapshots there keeps them alive across the phase-1 re-entrant hashes. Then hash (phase 1)
        // and build GC-free (phase 2), reading the snapshots from the list.
        let lh = self.heap.alloc(Obj::List(src.clone().into()));
        self.push(Value::obj(lh));
        let n = src.len();
        let built = (|| {
            for i in 0..n {
                let orig = {
                    let Obj::List(b) = self.heap.get(lh) else {
                        unreachable!()
                    };
                    b.get(i)
                };
                let snap = self.snapshot_key(orig);
                if let Obj::List(b) = self.heap.write(lh).get(lh) {
                    b.set(i, snap);
                }
            }
            let mut hashes = Vec::with_capacity(n);
            for i in 0..n {
                let v = {
                    let Obj::List(b) = self.heap.get(lh) else {
                        unreachable!()
                    };
                    b.get(i)
                };
                hashes.push(self.hash_value(v, span)?);
            }
            let set = SetData::default();
            for (i, &he) in hashes.iter().enumerate() {
                let v = {
                    let Obj::List(b) = self.heap.get(lh) else {
                        unreachable!()
                    };
                    b.get(i)
                };
                if self
                    .set_slot(&set.entries, set.candidates(he), v, span)?
                    .is_none()
                {
                    set.push(&self.heap, he, v);
                }
            }
            Ok(set)
        })();
        self.pop(); // unroot the source list
        Ok(Value::obj(self.heap.alloc(Obj::Set(built?))))
    }

    /// `Map.from_iter(it)` → a map from an iterable of 2-tuples `(k, v)` (last-wins on dup keys,
    /// like the `{k: v}` literal). A struct key's `hash()` re-enters the VM, so the in-flight
    /// key/value are rooted via `hash_key_rooted` while the building map is rooted.
    fn map_from_iter(&mut self, it: Value, span: Span) -> Result<Value, RuntimeError> {
        // `Map.from_iter(m)` over a map yields a COPY of it (a map iterates (K, V) pairs, but the copy
        // does not rebuild from them).
        if let Some(h) = it.as_obj()
            && let Obj::Map(inner) = self.heap.get(h)
        {
            let copy = inner.clone();
            return Ok(Value::obj(self.heap.alloc(Obj::Map(copy))));
        }
        let drained = self.drain_iterable(it, span)?;
        // Root the drained elements (as a fresh heap list) across the re-entrant hash() calls.
        let src_obj = Value::obj(self.heap.alloc(Obj::List(drained.clone().into())));
        self.push(src_obj);
        // Snapshot keys are rooted by pushing them onto the operand stack (the drained tuples may
        // ALIAS caller-held tuples, so they cannot be mutated in place). Everything above this base
        // is popped after the build, on both the Ok and Err path.
        let stack_base = self.stack.len();
        let built = (|| {
            let map = MapData::default();
            for elem in &drained {
                let (k, v) = match elem.as_obj().map(|eh| self.heap.get(eh)) {
                    Some(Obj::Tuple(parts)) if parts.len() == 2 => (parts[0], parts[1]),
                    _ => {
                        return Err(self.err(
                            format!(
                                "Map.from_iter() expects an iterable of (key, value) 2-tuples, got {}",
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
                if pos.and_then(|p| map.set_value(p, v)).is_none() {
                    map.push(&self.heap, hk, k, v);
                }
            }
            Ok(map)
        })();
        self.stack.truncate(stack_base); // unroot the snapshot keys (Ok or Err)
        self.pop(); // unroot the source list
        Ok(Value::obj(self.heap.alloc(Obj::Map(built?))))
    }

    /// Collect raw bytes from a byte-sequence-shaped argument: a `bytes`, a `bytearray` (copy), or a
    /// `List[int]` (each element 0..=255, else a recoverable fault). The `what` label names the call
    /// in error messages (`bytes.from_iter`, `extend`, `write_bytes`).
    pub(super) fn collect_bytes_arg(
        &self,
        what: &str,
        v: Value,
        span: Span,
    ) -> Result<Vec<u8>, RuntimeError> {
        match v.as_obj().map(|h| self.heap.get(h)) {
            Some(Obj::Bytes(b)) => Ok(b.to_vec()),
            Some(Obj::ByteArray(b)) => Ok(b.to_vec()),
            Some(Obj::List(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for e in items {
                    match self.int_val(e) {
                        Some(n) if (0..=255).contains(&n) => out.push(n as u8),
                        Some(n) => {
                            return Err(self.err(
                                format!("{what}() element {n} out of range (must be 0..=255)"),
                                span,
                            ));
                        }
                        None => {
                            return Err(self.err(
                                format!(
                                    "{what}() expects a list of int, got an element of type {}",
                                    self.type_name(e)
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

    /// The bytes of `bytes.from_iter(xs)` / `bytearray.from_iter(xs)`: a byte sequence or a list
    /// is read in place, any other iterable is drained to a list first. `what` names the type in
    /// a fault.
    fn bytes_from_iter(
        &mut self,
        what: &str,
        it: Value,
        span: Span,
    ) -> Result<Vec<u8>, RuntimeError> {
        let src = match it.as_obj().map(|h| self.heap.get(h)) {
            Some(Obj::Bytes(_) | Obj::ByteArray(_) | Obj::List(_)) => it,
            _ => {
                let items = self.drain_iterable(it, span)?;
                Value::obj(self.heap.alloc(Obj::List(items.into())))
            }
        };
        self.collect_bytes_arg(what, src, span)
    }

    /// The `n` zero bytes of `bytes.zeroed(n)` and `bytearray.zeroed(n)` (CPython's `bytes(n)` rule). A negative `n`
    /// faults. A block that cannot be allocated is a recoverable fault, not an abort, with no hard
    /// cap, so a large buffer still works; `alloc` zeroes the block, so no page is touched here.
    fn zero_filled<B>(
        &self,
        what: &str,
        n: i64,
        span: Span,
        alloc: impl FnOnce(usize) -> Result<B, AllocFailed>,
    ) -> Result<B, RuntimeError> {
        if n < 0 {
            return Err(self.err(format!("{what}() size {n} must be non-negative"), span));
        }
        alloc(n as usize).map_err(|AllocFailed| {
            self.err(format!("{what}() size {n} is too large to allocate"), span)
        })
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
