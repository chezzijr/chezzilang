// vm::call — split out of vm/mod.rs. `super::*` == the `vm` module.
// Calls: value/native/method/static dispatch, builtins, defer/return.

use super::*;

impl Vm {
    pub(super) fn do_call(&mut self, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        // Fast path — a plain `Func`/`Closure` runs directly over the args already contiguous on the
        // stack, skipping the `split_off` `Vec` alloc + the re-push in `push_frame`. The callee sits
        // one slot below the args; we drop it (shifting the args down one) so they become the new
        // frame's parameter slots in place. Native / not-callable callees fall through to the `Vec`
        // path (`invoke_native` needs an owned `Vec`, HOFs build args off-stack).
        let callee = self.stack[at - 1];
        if let Some(h) = callee.as_obj() {
            let kind = match self.heap.get(h) {
                Obj::Func { proto, home } => Some((*proto, *home, None)),
                Obj::Closure { proto, home, .. } => Some((*proto, *home, Some(h))),
                _ => None,
            };
            if let Some((proto, home, clo)) = kind {
                // Arity check BEFORE disturbing the stack — identical messages to `invoke_value`, and
                // the error path leaves `[callee, args…]` intact for the trace / `recover:`.
                let arity = self.program.protos[proto].arity;
                if clo.is_none() {
                    self.check_proto_arity(proto, argc, span)?;
                } else if argc != arity {
                    return Err(self.err(
                        format!("closure expects {arity} argument(s), got {argc}"),
                        span,
                    ));
                }
                // Experimental generators — calling a generator function does NOT run its body; it
                // allocates a suspendable generator object over the args. (Arity was just checked.)
                if self.program.protos[proto].is_generator {
                    let args: Vec<Value> = self.stack.split_off(at); // the argc args
                    self.stack.pop(); // drop the callee left beneath them
                    let g = self.alloc_generator(proto, home, clo, args);
                    self.push(g);
                    return Ok(());
                }
                // Drop the callee from beneath the args (argc-element memmove; `Value: Copy`).
                self.stack.copy_within(at.., at - 1);
                self.stack.pop();
                // M19 call-flattening: push the callee frame and let the *running* `run_until` loop
                // execute it, instead of recursing into a fresh `run_until` (which cost a native Rust
                // stack frame + an `Arc::clone(&self.program)` per call). The frame lands at
                // `frames.len()-1` with `ip = 0`; the loop already advanced the caller's `ip` past
                // this `Call` (on the captured caller index, before `step`), so the next iteration
                // runs the callee from its start. The callee's eventual `Op::Return` → `do_return`
                // pushes the result onto the caller's stack and pops the frame, and the loop resumes
                // the caller — no synchronous result to push here. Pause/`recover:`/`defer` are caught
                // by the loop body's own checks (they operate on `self.frames`, not the Rust stack).
                self.push_frame_in_place(proto, home, clo, at - 1, span)?;
                return Ok(());
            }
        }
        // Slow path — native, struct, or not-callable.
        let args: Vec<Value> = self.stack.split_off(at);
        let callee = self.pop();
        let v = self.invoke_value(callee, args, span)?;
        if self.paused() {
            return Ok(()); // B1/D3: callee parked on `recv` or yielded; don't push a sentinel result.
        }
        self.push(v);
        Ok(())
    }

    /// Dispatch an already-evaluated callable `Value` on evaluated args, *returning* the result
    /// instead of pushing it. Shared by `do_call` (which pushes) and the higher-order list methods
    /// (which call it per element while keeping their source/result lists rooted on the stack).
    /// `args.len()` is the explicit arg count for arity checks.
    pub(super) fn invoke_value(
        &mut self,
        callee: Value,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let argc = args.len();
        match callee.view() {
            ValueView::Obj(h) => {
                // Borrow the heap object only long enough to read its `Copy` fields. The old code
                // `self.heap.get(h).clone()` deep-cloned the whole `Obj` on *every* call — for a
                // closure that meant cloning its captured-environment `HashMap` each time — just to
                // read `proto`/`home`. `Native` still clones its (small) name `String`, but the hot
                // user-function/closure paths now copy three scalars and allocate nothing.
                enum Callee {
                    Func {
                        proto: ProtoId,
                        home: GcRef,
                    },
                    Closure {
                        proto: ProtoId,
                        home: GcRef,
                    },
                    Native {
                        func: crate::native::NativeFn,
                        name: Box<str>,
                        kind: crate::native::Kind,
                    },
                    Builtin(Box<str>),
                    Cffi(std::sync::Arc<crate::native::cffi::Cffi>),
                    NotCallable,
                }
                let callee_kind = match self.heap.get(h) {
                    Obj::Func { proto, home } => Callee::Func {
                        proto: *proto,
                        home: *home,
                    },
                    Obj::Closure { proto, home, .. } => Callee::Closure {
                        proto: *proto,
                        home: *home,
                    },
                    Obj::Native { func, name, kind } => Callee::Native {
                        func: *func,
                        name: name.clone(),
                        kind: *kind,
                    },
                    Obj::Builtin(name) => Callee::Builtin(name.clone()),
                    Obj::Cffi(c) => Callee::Cffi(std::sync::Arc::clone(c)),
                    _ => Callee::NotCallable,
                };
                match callee_kind {
                    Callee::Func { proto, home } => {
                        self.check_proto_arity(proto, argc, span)?;
                        // Experimental generators — allocate, don't run (see `do_call`'s fast path).
                        if self.program.protos[proto].is_generator {
                            return Ok(self.alloc_generator(proto, home, None, args));
                        }
                        self.run_proto(proto, home, None, args, true, false, span)
                    }
                    Callee::Closure { proto, home } => {
                        if argc != self.program.protos[proto].arity {
                            return Err(self.err(
                                format!(
                                    "closure expects {} argument(s), got {argc}",
                                    self.program.protos[proto].arity
                                ),
                                span,
                            ));
                        }
                        if self.program.protos[proto].is_generator {
                            return Ok(self.alloc_generator(proto, home, Some(h), args));
                        }
                        self.run_proto(proto, home, Some(h), args, true, false, span)
                    }
                    Callee::Native { func, name, kind } => {
                        self.invoke_native(func, &name, kind, args, span)
                    }
                    // A first-class universe builtin fn value (`print`/`ord`/`chr`/`panic`) — route
                    // back into the SAME logic direct calls use. `print` replicates `do_print`'s
                    // value-form defaults (space-join + trailing '\n'; sep=/end= are direct-call-only
                    // via `CallPrintSep`). `panic` returns `Err` (mirrors `do_builtin`'s panic arm) so
                    // defers still unwind. `ord`/`chr` reuse `builtin_ord`/`builtin_chr` directly.
                    Callee::Builtin(name) => match name.as_ref() {
                        "print" => {
                            // ROOT the args on the operand stack while stringifying. `args` was
                            // `split_off` the stack (do_call slow path), so it is NOT a GC root; a
                            // `Stringable` `str` method runs user code that can `collect()` at a
                            // safepoint and would sweep the LATER (still-unrendered) args — a
                            // use-after-free. `do_print` guards this exact hazard by keeping the args
                            // on the operand stack across the whole stringify loop; mirror it here:
                            // push them back, render from the rooted slots, then truncate.
                            let at = self.stack.len();
                            for v in &args {
                                self.push(*v);
                            }
                            let mut parts = Vec::with_capacity(args.len());
                            for i in 0..args.len() {
                                let v = self.stack[at + i];
                                parts.push(self.stringify(v, span, 0)?);
                            }
                            self.stack.truncate(at);
                            let mut line = parts.join(" ");
                            line.push('\n');
                            self.emit_out(&line);
                            match self.stream_halt(span) {
                                Some(halt) => Err(halt), // stdout died — halt like `os.exit`
                                None => Ok(Value::nil()),
                            }
                        }
                        "ord" => self.builtin_ord(&args, span),
                        "chr" => self.builtin_chr(&args, span),
                        "panic" => {
                            let message = match args.first().copied() {
                                Some(v) => match v.as_obj().map(|h| self.heap.get(h)) {
                                    Some(Obj::Str(s)) => s.to_string(),
                                    _ => self.type_name(v).to_string(),
                                },
                                None => String::new(),
                            };
                            Err(self.err(message, span))
                        }
                        _ => unreachable!("non-first-class builtin {name} reached invoke_value"),
                    },
                    Callee::Cffi(cffi) => {
                        // Arity is checker-guaranteed, but guard defensively (a hand-built program
                        // could bypass the checker) so a wrong arg count never indexes out of bounds.
                        self.check_arity("function", cffi.name(), cffi.param_count(), argc, span)?;
                        let mut host = VmHost { vm: self, args };
                        let ret = cffi.call(&mut host).map_err(|e| RuntimeError {
                            message: e.message,
                            span,
                            is_assert: false,
                            is_over_memory: false,
                            is_timed_out: false,
                        })?;
                        Ok(self.lower_native(ret))
                    }
                    Callee::NotCallable => Err(self.err(
                        format!("'{}' is not callable", self.type_name(callee)),
                        span,
                    )),
                }
            }
            _ => Err(self.err(
                format!("'{}' is not callable", self.type_name(callee)),
                span,
            )),
        }
    }

    /// Invoke a native (Rust) function value (M6c). Builds a [`VmHost`] over the evaluated args,
    /// runs the binding, then lowers its engine-neutral [`NativeRet`] into a heap-allocated `Value`
    /// and pushes it. Lowering (the only allocation) happens here — at an instruction boundary,
    /// after the call returns — so the "collect only at instruction boundaries" GC invariant holds.
    ///
    /// `kind` is the native's [`crate::native::Kind`], carried from its registry entry on the
    /// `Obj::Native` value — it decides EVERYTHING this fn branches on (intercept / offload / timed
    /// wait / inline). Nothing here compares a native's name to a string literal: that scatter is what
    /// `docs/future.md` §3c removed, because a new blocking native that forgot to join the old
    /// `is_blocking` list failed silently.
    pub(super) fn invoke_native(
        &mut self,
        func: crate::native::NativeFn,
        name: &str,
        kind: crate::native::Kind,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        use crate::native::Kind;
        // D6 / R2 — an INTERCEPTED native is run by the engine itself: `std.net.connect`/`listen` and
        // `std.io`'s Writer/Reader openers all allocate a handle (a heap object over an `Arc`'d core),
        // which a pure off-heap native cannot do, so their registered placeholder fns never execute.
        // Both arms must stay AHEAD of the offload gate below. The kind is what distinguishes them:
        // `std.io::_append` (an opener) and `std.fs::_append` (a syscall) share a bare name, and used
        // to be told apart only by check ORDER plus a func-pointer identity test.
        //
        // EXHAUSTIVE on purpose (no `_` arm): a future `Kind` must be routed here deliberately, or it
        // does not compile — a catch-all would silently run a new variant inline.
        match kind {
            Kind::InterceptNet => return self.net_connect_or_listen(name, args, span),
            Kind::InterceptIo => return self.io_native(name, args, span),
            Kind::Inline | Kind::Blocking | Kind::TimedWait => {}
        }
        // D5 — under the M:N engine, a blocking native call (`read_file` / `sleep_ms` / `fs.*`) is
        // OFFLOADED to the dirty pool rather than run inline, so it can't pin a core worker (the G3
        // starvation). Gated on `native_reentry == 0`: a blocking native reached inside a native
        // callback can't park the fiber (its caller's loop state is on the Rust host stack), so it
        // falls through to inline. Record the call + extracted primitive args; the worker loop hands
        // it to the pool ([`Disp::Offload`]) and `paused()` skips the (missing) result-push here. The
        // result is lowered + pushed by the worker that resumes the fiber after completion.
        // CANCELLATION CHECKPOINT — a blocking op is a cancel-delivery point regardless of whether an
        // M:N sched is in scope: it sits OUTSIDE the `mn.is_some()` offload gate below. A `mn == None`
        // context (top-level `main`, the inline outermost-`parallel:` builder VM, an eager `Executor`
        // job) needs it every bit as much as an M:N worker — it is the only checkpoint a `sleep_ms` /
        // `io` / `fs` / `request` / `process` call offers, and without it a cancelled fiber (one the
        // cancel drain re-drove, say) would run the blocking call to completion, stalling the whole
        // teardown for its full duration, and then keep executing the straight-line statements after
        // it. On an M:N worker it also stops a post-cancel `sleep_ms` from delaying the teardown by
        // the full sleep.
        if self.native_reentry == 0 && kind.blocks() && self.cancel_requested() {
            self.cancelled = true;
            return Err(self.err("cancelled".to_string(), span));
        }
        if self.mn.is_some()
            && self.native_reentry == 0
            && kind.blocks()
            && let Some(nargs) = self.extract_native_args(&args)
        {
            // D5 owe #2 — a TIMED WAIT (`sleep_ms`) rides the timer thread (park + deadline-wake), not
            // a pool thread (`timer_ms = Some(ms)`). A non-positive (or non-int) duration has nothing
            // to wait for, so it is NOT offloaded — `offload` stays `None` and execution falls through
            // to the inline path below (which returns `Nil` instantly). Every other blocking native
            // (the `io`/`fs`/`request`/`process` set) keeps `timer_ms = None` → the dirty pool.
            let offload = match kind {
                Kind::TimedWait => {
                    // Copy the duration out first (ends the `nargs` borrow before the move below).
                    let ms = match nargs.first() {
                        Some(crate::native::NativeArg::Int(ms)) if *ms > 0 => Some(*ms as u64),
                        _ => None, // sleep_ms(<=0) / non-int: inline no-op
                    };
                    // W7-16 — the halt inputs ride WITH the sleep, so the timer thread can end it on a
                    // cancel or a `--timeout` instead of only at its deadline. `checked_add` saturates
                    // a pathological `ms` (centuries) to a far-future deadline rather than panicking on
                    // `Instant` overflow — a panic in `offload` escapes *before* `complete_offload` and
                    // would pin `inflight` forever (hang). Matches the old inline-sleep path's
                    // "effectively infinite sleep" rather than a crash.
                    ms.map(|ms| {
                        let now = std::time::Instant::now();
                        let deadline = now
                            .checked_add(std::time::Duration::from_millis(ms))
                            .unwrap_or_else(|| now + std::time::Duration::from_secs(86_400 * 365));
                        OffloadReq {
                            func,
                            args: nargs,
                            span,
                            timer: Some(crate::vm::TimerSleep {
                                deadline,
                                cancel: self.demote_cancel_flags(),
                                run_deadline: self.deadline,
                                timeout_ms: self.timeout_ms,
                            }),
                        }
                    })
                }
                _ => Some(OffloadReq {
                    func,
                    args: nargs,
                    span,
                    timer: None,
                }),
            };
            if let Some(req) = offload {
                self.offload = Some(req);
                return Ok(Value::nil()); // sentinel; never pushed (the `paused()` gate at the call site)
            }
        }
        // D5 owe #3 Path C (#3) — a timed wait (`ms > 0`) reached INSIDE a native callback (the offload
        // gate above is skipped here because it requires `native_reentry == 0`). Rather than run inline
        // and pin the worker for `ms`, DEMOTE the worker: spawn a replacement + sleep in place +
        // resume. A non-positive / non-int arg has nothing to wait for → falls through to the inline
        // no-op.
        if self.mn.is_some()
            && self.native_reentry > 0
            && kind == Kind::TimedWait
            && let Some(ms) = args.first().and_then(|v| self.int_val(*v))
            && ms > 0
        {
            return self.demote_block_sleep(ms as u64, span);
        }
        // W7-16 — every OTHER timed wait with `ms > 0`: an eager `Executor` job (`mn == None`), the
        // top-level `main` thread, the inline outermost-`parallel:` builder VM, or a `mn == None`
        // native callback.
        // All of them used to reach `native::time::sleep_ms`'s bare `std::thread::sleep`, which is a
        // hole in every halt the loop it replaces would have checked: `shutdown_now()` at 50 ms against
        // a `sleep_ms(3000)` waited the full 3012 ms AND ran the job's post-sleep code, and
        // `chezzi test --timeout=200` did not abort it either (measured: PASS, 3 s). The deadline is
        // OURS, so it stays a checkpoint for its whole duration — see [`Vm::block_until_deadline`].
        //
        // Deliberately NOT gated on `native_reentry == 0`: a native callback loop is already a
        // documented cancellation checkpoint, `demote_block_sleep` above already faults from inside
        // one, and `block_halt_check` only ever returns `Err` — it never unwinds VM state.
        //
        // `checked_add` saturates a pathological `ms` (centuries) to a far-future deadline rather than
        // panicking on `Instant` overflow, matching the offload path's own saturation above.
        if kind == Kind::TimedWait
            && let Some(ms) = args.first().and_then(|v| self.int_val(*v))
            && ms > 0
        {
            let now = std::time::Instant::now();
            let deadline = now
                .checked_add(std::time::Duration::from_millis(ms as u64))
                .unwrap_or_else(|| now + std::time::Duration::from_secs(86_400 * 365));
            self.block_until_deadline(deadline, span)?;
            return Ok(Value::nil());
        }
        let writes_before = self.stdout_writes;
        let mut host = VmHost { vm: self, args };
        let ret = func(&mut host).map_err(|e| RuntimeError {
            message: e.message,
            span,
            is_assert: false,
            is_over_memory: false,
            is_timed_out: false,
        })?;
        // A streamed `io.print`/`io.flush` whose stdout died emitted into a dead sink
        // ([`Vm::emit_out`], a no-op there) and still returned `Ok` — so the deterministic
        // broken-pipe halt is raised HERE, at the call site, exactly as at `print` (line ~186) and
        // the `Writer` arm (line ~1121).
        //
        // W7-5d — the gate is "did THIS native emit to stdout", NOT the bare
        // `stream_halt`. `stream_halt` reads the process-GLOBAL `out_dead_reason()`, so unguarded it
        // fired after EVERY native once stdout died anywhere: a job doing three `fs.atomic_write`s
        // completed only the first, and how many completed varied with the thread count and across
        // runs. The old comment here claimed "this only ever fires for the print natives" — it did
        // not, and nothing made it so. The counter delta does. Re-entrancy is covered: a native that
        // re-enters Chezzi and prints there bumps the same counter (see [`Vm::stdout_writes`]).
        if self.stdout_writes != writes_before
            && let Some(halt) = self.stream_halt(span)
        {
            return Err(halt);
        }
        Ok(self.lower_native(ret))
    }

    /// D5 — materialize a blocking native's already-evaluated `Value` args into `Send` primitives so
    /// the dirty-pool thread can run the call without the heap. Returns `None` if any arg is not a
    /// primitive (int / float / bool / str) — the scoped blocking fns only ever take primitives, so a
    /// non-primitive means "don't offload, run inline" (a safe fallback, never a fault).
    pub(super) fn extract_native_args(
        &self,
        args: &[Value],
    ) -> Option<Vec<crate::native::NativeArg>> {
        use crate::native::NativeArg as A;
        args.iter()
            .map(|&v| {
                if let Some(n) = self.int_val(v) {
                    return Some(A::Int(n));
                }
                if v.is_float() {
                    return Some(A::Float(self.float_of(v)));
                }
                if let Some(b) = v.as_bool() {
                    return Some(A::Bool(b));
                }
                let h = v.as_obj()?;
                match self.heap.get(h) {
                    Obj::Str(s) => Some(A::Str(s.to_string())),
                    // R1 — a binary arg (today `io.write_bytes`): copied out so it survives the
                    // off-heap handoff. `bytes` only — the checker types every seam param `bytes`
                    // and a `bytearray` is not assignable to it (`bytes(ba)` converts).
                    Obj::Bytes(b) => Some(A::Bytes(b.to_vec())),
                    // A `Map[str, str]` arg (today only `request`'s headers) is snapshotted into
                    // owned pairs so it survives the off-heap handoff. Any non-str key/value reverts
                    // to `None` → run inline (safe fallback; the checker guarantees str/str for
                    // typed code, so this is unreachable from a well-typed program).
                    Obj::Map(m) => {
                        let mut pairs = Vec::with_capacity(m.entries.len());
                        for (_, k, mv) in &m.entries {
                            let (Some(kh), Some(vh)) = (k.as_obj(), mv.as_obj()) else {
                                return None;
                            };
                            let (Obj::Str(ks), Obj::Str(vs)) =
                                (self.heap.get(kh), self.heap.get(vh))
                            else {
                                return None;
                            };
                            pairs.push((ks.to_string(), vs.to_string()));
                        }
                        Some(A::Map(pairs))
                    }
                    // A `List[str]` arg (today only `run_args`'s argv) is snapshotted into owned
                    // strings so it survives the off-heap handoff. Any non-str element reverts to
                    // `None` → run inline (the checker guarantees str for typed code).
                    Obj::List(items) => {
                        let mut out = Vec::with_capacity(items.len());
                        for e in items {
                            let eh = e.as_obj()?;
                            let Obj::Str(s) = self.heap.get(eh) else {
                                return None;
                            };
                            out.push(s.to_string());
                        }
                        Some(A::List(out))
                    }
                    _ => None,
                }
            })
            .collect()
    }

    /// Lower a native fn's engine-neutral [`crate::native::NativeRet`] into a VM `Value`, allocating
    /// heap objects for the reference kinds. `Ok`/`Err`/`Some`/`None` become the built-in
    /// `Result` / `Option` enum objects.
    /// Map a SCALAR callback result [`Value`] back to an engine-neutral [`crate::native::NativeRet`]
    /// for the FFI trampoline to write into C's return slot (the reverse of `lower_native`, scalar-
    /// only: a callback return is checker-restricted to int/float/bool/ptr). A non-scalar is a
    /// checker-prevented case; default to `Int(0)` defensively (the trampoline then writes a zeroed
    /// register, never UB).
    ///
    /// R1: deliberately NO `bytes` arm. This maps a value into C's *return register* — there is no C
    /// return repr for a byte buffer, and the checker rejects a non-scalar callback return, so a
    /// `Bytes` arm here would be unreachable dead code.
    pub(super) fn value_to_native_ret(&self, v: Value) -> crate::native::NativeRet {
        use crate::native::NativeRet as N;
        if let Some(n) = self.int_val(v) {
            return N::Int(n);
        }
        if v.is_float() {
            return N::Float(self.float_of(v));
        }
        if let Some(b) = v.as_bool() {
            return N::Bool(b);
        }
        if v.is_nil() {
            return N::Nil;
        }
        if let Some(h) = v.as_obj()
            && let Obj::Ptr(a) = self.heap.get(h)
        {
            return N::Ptr(*a);
        }
        N::Int(0)
    }

    pub(super) fn lower_native(&mut self, ret: crate::native::NativeRet) -> Value {
        use crate::native::NativeRet as N;
        match ret {
            N::Int(n) => self.make_int(n),
            N::Float(f) => self.box_float(f),
            N::Bool(b) => Value::bool(b),
            N::Nil => Value::nil(),
            N::Ptr(a) => Value::obj(self.heap.alloc(Obj::Ptr(a))),
            N::Str(s) => self.alloc_str(s),
            N::Bytes(b) => Value::obj(self.heap.alloc(Obj::Bytes(b.into_boxed_slice()))),
            N::List(items) => {
                let mut vs = Vec::with_capacity(items.len());
                for x in items {
                    vs.push(self.lower_native(x));
                }
                Value::obj(self.heap.alloc(Obj::List(vs)))
            }
            N::Struct { name, fields } => {
                // Positional layout: lower the named native fields, then place them into a flat Vec
                // at the StructDef's declaration-order index (native emit order already matches, but
                // resolving by name keeps it robust to drift). Lower first (each may allocate), then
                // allocate the struct — keeps every allocation at this boundary (GC invariant).
                let tid = self.struct_tid(&name);
                let order: Option<Vec<String>> =
                    self.program.structs.get(&name).map(|d| d.fields.clone());
                let mut lowered: Vec<(Box<str>, Value)> = Vec::with_capacity(fields.len());
                for (k, v) in fields {
                    let lv = self.lower_native(v);
                    lowered.push((k.into_boxed_str(), lv));
                }
                let fs: Vec<Value> = match order {
                    // Registered type: place each lowered value at its declaration-order slot.
                    Some(order) => order
                        .iter()
                        .map(|fname| {
                            lowered
                                .iter()
                                .find(|(k, _)| k.as_ref() == fname.as_str())
                                .map(|(_, v)| *v)
                                .unwrap_or(Value::nil())
                        })
                        .collect(),
                    // Ad-hoc / unregistered (TID_NONE): keep native emit order positionally.
                    None => lowered.into_iter().map(|(_, v)| v).collect(),
                };
                Value::obj(self.heap.alloc(Obj::Struct {
                    tid,
                    fields: Fields::from_vec(fs),
                }))
            }
            N::Map(entries) => {
                // Native maps have unique scalar (str) keys — hash them directly (no re-entry, no
                // dedup needed).
                let mut map = MapData::default();
                for (k, v) in entries {
                    let lk = self.lower_native(k);
                    let lv = self.lower_native(v);
                    let hk = self.scalar_hash(lk);
                    map.push(hk, lk, lv);
                }
                Value::obj(self.heap.alloc(Obj::Map(map)))
            }
            N::Ok(inner) => {
                let p = self.lower_native(*inner);
                self.alloc_enum("Result", "Ok", vec![p])
            }
            N::Err(msg) => {
                let p = self.alloc_str(msg);
                self.alloc_enum("Result", "Err", vec![p])
            }
            N::Some(inner) => {
                let p = self.lower_native(*inner);
                self.alloc_enum("Option", "Some", vec![p])
            }
            N::None => self.alloc_enum("Option", "None", Vec::new()),
        }
    }

    /// Build a NATIVE `Result`/`Option` enum instance (the native / std construction path: `Ok`/`Err`/
    /// `Some`/`None`, list `pop`, regex/request/json/fs returns). M19 lever #2 — stamps the FIXED native
    /// `VID_OK`/`VID_ERR`/`VID_SOME`/`VID_NONE_VARIANT` constant DIRECTLY, never a name lookup through
    /// `Program::variants`: a user enum declaring a variant named `Ok`/`Err`/`Some`/`None` SHADOWS that
    /// name in the `variants` map (its own dense id at `4..`), so resolving by name here would stamp the
    /// user's id onto a genuine native value — collapsing native-vs-user identity (broken `==`) and
    /// missing `?`'s `variant_id == VID_SOME`/`VID_OK` gate. The reserved 0..=3 ids are disjoint from
    /// every user id, so stamping the constant keeps native and user variants distinguishable. `ty` is
    /// retained in the signature so call sites read self-documentingly, but it is not stored.
    pub(super) fn alloc_enum(&mut self, ty: &str, variant: &str, payload: Vec<Value>) -> Value {
        let _ = ty;
        use crate::vm::op::{VID_ERR, VID_NONE, VID_NONE_VARIANT, VID_OK, VID_SOME};
        let variant_id = match variant {
            "Ok" => VID_OK,
            "Err" => VID_ERR,
            "Some" => VID_SOME,
            "None" => VID_NONE_VARIANT,
            // `alloc_enum` is the NATIVE construction path; it is only ever called with the four
            // reserved names above. The fallback is defensive only.
            _ => VID_NONE,
        };
        Value::obj(self.heap.alloc(Obj::Enum {
            variant_id,
            payload,
        }))
    }

    /// `Op::JsonDecode`: pop the `Result[Json]` from `parse`, coerce its `Ok` payload against the
    /// descriptor (passing through an `Err`), push the resulting `Result[T]`.
    pub(super) fn json_decode(
        &mut self,
        desc: &crate::json_decode::TypeDescriptor,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let res = self.pop();
        let bad = "decode: parse did not return a Result".to_string();
        let (rty, variant, payload) = self
            .enum_parts(res)
            .ok_or_else(|| self.err(bad.clone(), span))?;
        if rty != "Result" {
            return Err(self.err(bad, span));
        }
        match variant.as_str() {
            "Err" => {
                self.push(res); // a Result Err(str) is already a valid Result[T]
                Ok(())
            }
            "Ok" if payload.len() == 1 => {
                let jv = payload[0];
                match self.coerce_json(jv, desc, "$") {
                    Ok(v) => {
                        let r = self.alloc_enum("Result", "Ok", vec![v]);
                        self.push(r);
                    }
                    Err(msg) => {
                        let s = self.alloc_str(msg);
                        let r = self.alloc_enum("Result", "Err", vec![s]);
                        self.push(r);
                    }
                }
                Ok(())
            }
            _ => Err(self.err(bad, span)),
        }
    }

    /// The enum type, variant name, and (copied) payload of an enum value; `None` if not an enum.
    pub(super) fn enum_parts(&self, v: Value) -> Option<(String, String, Vec<Value>)> {
        let h = v.as_obj()?;
        match self.heap.get(h) {
            Obj::Enum {
                variant_id,
                payload,
            } => {
                // M19 lever #2 — cold path: resolve the type + variant names from the id.
                let (ty, variant) = self.enum_names(*variant_id);
                Some((ty.to_string(), variant.to_string(), payload.clone()))
            }
            _ => None,
        }
    }

    /// Coerce a parsed `Json` value into a concrete value of the descriptor's type. `path` is a
    /// JSON-pointer-ish breadcrumb for error messages.
    pub(super) fn coerce_json(
        &mut self,
        jv: Value,
        desc: &crate::json_decode::TypeDescriptor,
        path: &str,
    ) -> Result<Value, String> {
        use crate::json_decode::TypeDescriptor as D;
        let (_jty, variant, payload) = self
            .enum_parts(jv)
            .ok_or_else(|| format!("decode: expected a JSON value at {path}"))?;
        let mismatch = |want: &str| {
            format!(
                "decode: expected {want} at {path}, found {}",
                crate::json_decode::json_kind(&variant)
            )
        };
        match desc {
            D::Int => {
                // `Json.Int` carries its exact i64 payload (TICKET-013, W8-35) — take it directly,
                // never through f64, so a 19-digit id decodes byte-exact.
                if variant == "Int" {
                    let n = self.int_val(payload[0]).ok_or_else(|| mismatch("int"))?;
                    return Ok(self.make_int(n));
                }
                let f = self
                    .json_num(&variant, &payload)
                    .ok_or_else(|| mismatch("int"))?;
                if f.fract() != 0.0 || !f.is_finite() {
                    return Err(format!("decode: expected an integer at {path}, found {f}"));
                }
                // `f as i64` saturates, so range-check first. Use a strict `> 2^63` upper bound so
                // i64::MAX (which f64-rounds to exactly 2^63) still round-trips via the saturating
                // cast, while everything strictly above 2^63 / below -2^63 is rejected.
                if f < i64::MIN as f64 || f > 9_223_372_036_854_775_808.0 {
                    return Err(format!(
                        "decode: integer {f} at {path} is out of range for int"
                    ));
                }
                Ok(self.make_int(f as i64))
            }
            D::Float => {
                let f = self
                    .json_num(&variant, &payload)
                    .ok_or_else(|| mismatch("float"))?;
                Ok(self.box_float(f))
            }
            D::Bool => match (variant.as_str(), payload.first().and_then(|v| v.as_bool())) {
                ("Bool", Some(b)) => Ok(Value::bool(b)),
                _ => Err(mismatch("bool")),
            },
            D::Str => {
                if variant == "Str" {
                    let s = self.val_str(payload[0]).unwrap_or_default();
                    Ok(self.alloc_str(s))
                } else {
                    Err(mismatch("str"))
                }
            }
            D::Option(inner) => {
                if variant == "Null" {
                    Ok(self.alloc_enum("Option", "None", Vec::new()))
                } else {
                    let v = self.coerce_json(jv, inner, path)?;
                    Ok(self.alloc_enum("Option", "Some", vec![v]))
                }
            }
            D::List(inner) => {
                if variant != "Arr" {
                    return Err(mismatch("array"));
                }
                let items = match self.heap.get(self.as_obj(payload[0])) {
                    Obj::List(items) => items.clone(),
                    _ => return Err(mismatch("array")),
                };
                let mut out = Vec::with_capacity(items.len());
                for (i, it) in items.into_iter().enumerate() {
                    out.push(self.coerce_json(it, inner, &format!("{path}[{i}]"))?);
                }
                Ok(Value::obj(self.heap.alloc(Obj::List(out))))
            }
            D::Map(inner) => {
                if variant != "Obj" {
                    return Err(mismatch("object"));
                }
                let entries = match self.heap.get(self.as_obj(payload[0])) {
                    Obj::Map(m) => m.entries.clone(),
                    _ => return Err(mismatch("object")),
                };
                let mut out = MapData::default();
                for (hk, k, v) in entries {
                    let key = self.val_str(k).unwrap_or_default();
                    let coerced = self.coerce_json(v, inner, &format!("{path}.{key}"))?;
                    out.push(hk, k, coerced); // str keys unchanged → reuse the cached hash
                }
                Ok(Value::obj(self.heap.alloc(Obj::Map(out))))
            }
            D::Struct {
                key,
                display,
                fields,
            } => {
                if variant != "Obj" {
                    // ROOT REDESIGN — error text shows the BARE display name, never the identity key.
                    return Err(mismatch(&format!("object for {display}")));
                }
                let entries = match self.heap.get(self.as_obj(payload[0])) {
                    Obj::Map(m) => m.entries.clone(),
                    _ => return Err(mismatch("object")),
                };
                // Positional layout: `fields` (the type descriptor) is already in the struct's
                // declaration order (see `json_decode::struct_descriptor`), so push values in order.
                let mut field_vals: Vec<Value> = Vec::with_capacity(fields.len());
                for (fname, fdesc) in fields {
                    let found = entries
                        .iter()
                        .find(|(_, k, _)| self.val_str(*k).as_deref() == Some(fname.as_str()));
                    let fpath = format!("{path}.{fname}");
                    let v = match found {
                        Some((_, _, jval)) => self.coerce_json(*jval, fdesc, &fpath)?,
                        None => match fdesc {
                            // A missing Option field decodes to None; anything else is an error.
                            D::Option(_) => self.alloc_enum("Option", "None", Vec::new()),
                            _ => return Err(format!("decode: missing key '{fname}' at {path}")),
                        },
                    };
                    field_vals.push(v);
                }
                // ROOT REDESIGN — tag the value with the qualified IDENTITY KEY (so downstream
                // field/method lookups + `struct_tid` hit the right layout); display renders bare.
                let tid = self.struct_tid(key);
                let h = self.heap.alloc(Obj::Struct {
                    tid,
                    fields: Fields::from_vec(field_vals),
                });
                Ok(Value::obj(h))
            }
        }
    }

    /// The `f64` of a JSON `Num` or `Int`, else `None`.
    pub(super) fn json_num(&self, variant: &str, payload: &[Value]) -> Option<f64> {
        if variant == "Num" || variant == "Int" {
            payload.first().and_then(|&v| {
                if v.is_float() {
                    Some(self.float_of(v))
                } else {
                    self.int_val(v).map(|n| n as f64)
                }
            })
        } else {
            None
        }
    }

    /// Depth cap for `json.encode`'s runtime walk (`json_of`) — independent of `stringify`'s own
    /// cap in `std/json.chz`, which only guards a `Json` tree that already exists. A Chezzi struct
    /// is a reference value and may be cyclic, so this is what stops `json.encode` on one from
    /// recursing forever. Tied to `std/json.chz`'s `MAX_NEST_DEPTH`.
    const JSON_ENCODE_MAX_DEPTH: usize = 2000;

    /// `Op::JsonToValue`: pop one runtime value, push the `Json` tree that `std.json`'s own
    /// `stringify` then renders — `json.encode(x)` is `stringify(_to_json(x))` in Chezzi.
    pub(super) fn json_to_value(&mut self, span: Span) -> Result<(), RuntimeError> {
        let v = self.pop();
        match self.json_of(v, 0) {
            Ok(jv) => {
                self.push(jv);
                Ok(())
            }
            Err(msg) => Err(self.err(msg, span)),
        }
    }

    /// Build a `Json.<variant>` enum value, keyed by `Program::variants[("Json", variant)]` (the
    /// runtime key `std.json` registers itself under — bare, since it stays a non-native module).
    /// A missing key means `std.json` was never loaded, e.g. `json._to_json` reached through some
    /// other module.
    fn json_variant(&mut self, variant: &str, payload: Vec<Value>) -> Result<Value, String> {
        let variant_id = self
            .program
            .variants
            .get(&("Json".to_string(), variant.to_string()))
            .map(|d| d.variant_id)
            .ok_or_else(|| "json.encode: std.json is not loaded".to_string())?;
        Ok(Value::obj(self.heap.alloc(Obj::Enum {
            variant_id,
            payload,
        })))
    }

    /// The runtime walk behind `json.encode`: a `Value` -> the `Json` tree that mirrors its shape.
    /// Accumulates children into a plain `Vec<Value>`/`MapData`, same allocation idiom as
    /// `coerce_json` (no new rooting scheme).
    fn json_of(&mut self, v: Value, depth: usize) -> Result<Value, String> {
        use crate::vm::op::{VID_ERR, VID_NONE_VARIANT, VID_OK, VID_SOME};
        if depth > Self::JSON_ENCODE_MAX_DEPTH {
            return Err("json.encode: exceeded max depth".to_string());
        }
        match v.view() {
            ValueView::Nil => self.json_variant("Null", Vec::new()),
            ValueView::Bool(b) => self.json_variant("Bool", vec![Value::bool(b)]),
            ValueView::Int(n) => self.json_variant("Int", vec![Value::int(n)]),
            ValueView::Obj(h) => match self.heap.get(h).clone() {
                Obj::FloatBox(f) => {
                    let fv = self.box_float(f);
                    self.json_variant("Num", vec![fv])
                }
                Obj::BigInt(n) => {
                    let iv = self.make_int(n);
                    self.json_variant("Int", vec![iv])
                }
                Obj::Str(s) => {
                    let sv = self.alloc_str(s.to_string());
                    self.json_variant("Str", vec![sv])
                }
                Obj::List(items) | Obj::Tuple(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for it in items {
                        out.push(self.json_of(it, depth + 1)?);
                    }
                    let lv = Value::obj(self.heap.alloc(Obj::List(out)));
                    self.json_variant("Arr", vec![lv])
                }
                Obj::Map(m) => {
                    let mut out = MapData::default();
                    for (hk, k, val) in m.entries {
                        if self.val_str(k).is_none() {
                            return Err(format!(
                                "json.encode: object keys must be str, found {}",
                                self.type_name(k)
                            ));
                        }
                        let jv = self.json_of(val, depth + 1)?;
                        out.push(hk, k, jv);
                    }
                    let mv = Value::obj(self.heap.alloc(Obj::Map(out)));
                    self.json_variant("Obj", vec![mv])
                }
                Obj::Struct { tid, fields } => {
                    let key = self.struct_name_of_tid(tid).to_string();
                    let field_names = self
                        .program
                        .structs
                        .get(&key)
                        .map(|d| d.fields.clone())
                        .unwrap_or_default();
                    let field_vals: Vec<Value> = fields.as_slice().to_vec();
                    let mut out = MapData::default();
                    for (fname, fval) in field_names.into_iter().zip(field_vals) {
                        let key_v = self.alloc_str(fname);
                        let hk = self.scalar_hash(key_v);
                        let jv = self.json_of(fval, depth + 1)?;
                        out.push(hk, key_v, jv);
                    }
                    let mv = Value::obj(self.heap.alloc(Obj::Map(out)));
                    self.json_variant("Obj", vec![mv])
                }
                Obj::Enum {
                    variant_id,
                    payload,
                } => {
                    let (ty, _) = self.enum_names(variant_id);
                    if ty == "Json" {
                        return Ok(v);
                    }
                    match variant_id {
                        VID_SOME => self.json_of(payload[0], depth + 1),
                        VID_NONE_VARIANT => self.json_variant("Null", Vec::new()),
                        VID_OK | VID_ERR => Err("json.encode: cannot encode a Result".to_string()),
                        _ => Err(format!("json.encode: cannot encode enum {ty}")),
                    }
                }
                _ => Err(format!("json.encode: cannot encode {}", self.type_name(v))),
            },
        }
    }

    /// The owned text of a str value, else `None`.
    pub(super) fn val_str(&self, v: Value) -> Option<String> {
        let h = v.as_obj()?;
        match self.heap.get(h) {
            Obj::Str(s) => Some(s.to_string()),
            _ => None,
        }
    }

    /// The heap handle of an `Obj` value (caller guarantees it is one).
    pub(super) fn as_obj(&self, v: Value) -> GcRef {
        v.as_obj().expect("as_obj on non-object")
    }

    /// Arity check for a call that enters a compiled PROTO, accepting `min_arity..=arity`.
    ///
    /// A proto whose trailing parameters carry defaults may be entered short — the callee's own
    /// prologue fills the omitted slots (`Op::JumpIfProvided`). Everything else keeps
    /// `min_arity == arity`, so this is the exact old check for every function without defaults.
    ///
    /// Every proto-entering site routes through here rather than inlining its own compare, so the
    /// relaxation cannot reach some call paths and miss others — the failure mode where a method
    /// call accepts a short arity that the same function rejects through a value.
    pub(super) fn check_proto_arity(
        &self,
        proto: ProtoId,
        got: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let p = &self.program.protos[proto];
        if (p.min_arity..=p.arity).contains(&got) {
            return Ok(());
        }
        let want = if p.min_arity == p.arity {
            format!("{}", p.arity)
        } else {
            format!("{}-{}", p.min_arity, p.arity)
        };
        Err(self.err(
            format!(
                "function '{}' expects {want} argument(s), got {got}",
                p.name
            ),
            span,
        ))
    }

    pub(super) fn check_arity(
        &self,
        _kind: &str,
        name: &str,
        want: usize,
        got: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if want != got {
            return Err(self.err(
                format!("function '{name}' expects {want} argument(s), got {got}"),
                span,
            ));
        }
        Ok(())
    }

    /// Try to dispatch a BODIED Chezzi method on a native handle (`native struct` bare name `key`, e.g.
    /// `"Reader"`). Returns `Ok(true)` if `method` resolved to a compiled proto and was dispatched (a
    /// result/generator pushed, or a frame flattened into `run_until`); `Ok(false)` if the native
    /// struct carries no such bodied method — the caller then falls through to the native name-keyed
    /// dispatch (`reader_method` etc.), leaving those byte-identical. Mirrors the enum-method arm
    /// (`Obj::Enum` in this file): type-erased (no `StructDef`/`tid`), home-globals from `native_home`,
    /// generator-aware (a `yield`-bearing method allocates rather than running).
    #[allow(clippy::too_many_arguments)] // handle key + method + recv/args + argc + ic + span, like the enum arm
    fn try_native_bodied_method(
        &mut self,
        key: &str,
        method: &str,
        recv: Value,
        args: &[Value],
        argc: usize,
        ic: u32,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        let prog = Arc::clone(&self.program);
        let Some(proto) = prog
            .native_methods
            .get(key)
            .and_then(|ms| ms.get(method).copied())
        else {
            return Ok(false);
        };
        self.check_proto_arity(proto, argc + 1, span)?;
        let home = self.module_objs[prog.native_home[key]];
        if self.program.protos[proto].is_generator {
            let mut gen_args = Vec::with_capacity(argc + 1);
            gen_args.push(recv);
            gen_args.extend_from_slice(args);
            let g = self.alloc_generator(proto, home, None, gen_args);
            self.push(g);
            return Ok(true);
        }
        // Flatten only on the real dispatch-loop path (`ic != NO_IC`); a re-entrant caller passes
        // `NO_IC` and uses the synchronous `run_proto` path (mirrors the struct/enum arms).
        if ic != NO_IC {
            let base = self.stack.len();
            self.stack.push(recv);
            self.stack.extend_from_slice(args);
            self.push_frame_in_place(proto, home, None, base, span)?;
            return Ok(true);
        }
        let mut call_args = Vec::with_capacity(argc + 1);
        call_args.push(recv);
        call_args.extend_from_slice(args);
        let v = self.run_proto(proto, home, None, call_args, true, false, span)?;
        if self.paused() {
            return Ok(true);
        }
        self.push(v);
        Ok(true)
    }

    /// `ic`: the per-call-site method inline-cache id from the `CallMethod` op, or [`NO_IC`] for the
    /// native-re-entry callers (`spawn`/`defer` method tasks) that need a *synchronous* result and so
    /// must take the re-entrant `run_proto` path (never the in-place frame flatten). A real `ic` ⟺ the
    /// caller is the running dispatch loop (the sole emit path), so a real `ic` is exactly the
    /// "flatten-safe" signal: the pushed frame is executed by the `run_until` that called us.
    pub(super) fn do_method_call(
        &mut self,
        method: &str,
        argc: usize,
        ic: u32,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let recv = self.pop();
        // `compare` on a primitive (int/float/str): they intrinsically satisfy `Comparable`, so an
        // erased generic body may call `.compare()` on a concrete primitive. Return the sign of the
        // ordering. Structs with their own `compare` fall through to the normal dispatch below.
        if method == "compare" && args.len() == 1 {
            let is_prim = self.is_numeric(recv)
                || matches!(recv.as_obj(), Some(h) if matches!(self.heap.get(h), Obj::Str(_)));
            if is_prim && let Some(ord) = self.compare(recv, args[0]) {
                self.push(Value::int(ord as i64));
                return Ok(());
            }
        }
        // `str` on a scalar: int/float/bool/str intrinsically satisfy `Stringable`, so an erased
        // generic body may call `.str()` on a concrete scalar. Render via `stringify` (the same path
        // `str(x)`/interpolation use) and alloc a fresh `Obj::Str`. The already-`Str` receiver (T=str)
        // MUST be intercepted here too — otherwise it would fall to struct-method dispatch and fault;
        // `stringify` returns its raw unquoted contents. GC-safe: `recv` is already popped, and for a
        // scalar receiver `stringify` runs no user code, so the owned `String` exists before the alloc.
        // Mirrors the `compare` scalar branch above. Structs with their own `str` fall through below.
        if method == "str" && args.is_empty() {
            let is_scalar = self.is_numeric(recv)
                || recv.as_bool().is_some()
                || recv.is_nil()
                || matches!(recv.as_obj(), Some(h) if matches!(self.heap.get(h), Obj::Str(_)));
            if is_scalar {
                let s = self.stringify(recv, span, 0)?;
                let h = self.heap.alloc(Obj::Str(s.into()));
                self.push(Value::obj(h));
                return Ok(());
            }
        }
        let Some(h) = recv.as_obj() else {
            // W6-3 — an inline scalar (int/float/bool/nil) receiver: the checker grants int/float the
            // arith operator protocols and int/bool `Hashable` intrinsically, so answer those here.
            // Miss-only, so this costs nothing for any resolvable call.
            if let Some(v) = self.intrinsic_proto_method(recv, method, &args, span)? {
                self.push(v);
                return Ok(());
            }
            return Err(self.err(
                format!("type {} has no method '{method}'", self.type_name(recv)),
                span,
            ));
        };
        // M19 Phase 6 / N-way poly — method-call inline-cache fast path (struct methods only). Scan
        // the site's ways for a way whose cached `tid` matches the receiver layout: a hit collapses the
        // `program.structs` clone + name-keyed `def.methods` probe to a short int-compare scan AND
        // flattens the call: `[recv, args…]` go on the stack and the method frame is installed in place,
        // so the running `run_until` executes the body and its `Return` pushes the result — no re-entrant
        // `run_proto`. A megamorphic-but-bounded site (≤4 distinct receiver types) hits a way for each,
        // so it never thrashes the monomorphic refill. Only the dispatch loop reaches here (real `ic`);
        // the arity guard re-runs per hit (cheap) so a hit can never enter a frame with the wrong slot
        // count, and the tid re-compare on every probe bars a wrong body. A `sticky` site (overflowed
        // past 4 types) skips the probe and falls straight through to the slow path.
        if ic != NO_IC {
            let site = self.method_ic[ic as usize];
            if !site.sticky
                && let Obj::Struct { tid, .. } = self.heap.get(h)
            {
                let recv_tid = *tid;
                let mut hit: Option<MethodIcCell> = None;
                for way in &site.ways {
                    if way.tid != TID_NONE && way.tid == recv_tid {
                        hit = Some(*way);
                        break;
                    }
                }
                if let Some(cell) = hit {
                    let proto = cell.proto;
                    self.check_proto_arity(proto, argc + 1, span)?;
                    let home = self.module_objs[cell.module_idx as usize];
                    // Experimental generators — a generator method allocates rather than running (else its
                    // `Op::Yield` would poison the host run with no `generator_next` to drive it).
                    if self.program.protos[proto].is_generator {
                        let mut gen_args = Vec::with_capacity(argc + 1);
                        gen_args.push(recv);
                        gen_args.extend(args);
                        let g = self.alloc_generator(proto, home, None, gen_args);
                        self.push(g);
                        return Ok(());
                    }
                    let base = self.stack.len();
                    self.stack.push(recv);
                    self.stack.extend(args);
                    return self.push_frame_in_place(proto, home, None, base, span);
                }
            }
        }
        // Higher-order list methods (`map`/`filter`/`fold`) call a closure per element, which runs
        // nested VM frames that may GC at instruction boundaries. They keep the source + result
        // (and fold's accumulator) rooted on the operand stack across the loop — see `list_hof`.
        if matches!(self.heap.get(h), Obj::List(_))
            && matches!(
                method,
                "map" | "filter" | "fold" | "take_while" | "drop_while" | "count" | "position"
            )
        {
            let result = self.list_hof(h, method, args, span)?;
            self.push(result);
            return Ok(());
        }
        // `sort_by` also runs a closure per comparison, but sorts in place and returns nil.
        if matches!(self.heap.get(h), Obj::List(_)) && method == "sort_by" {
            let result = self.list_sort_by(h, args, span)?;
            self.push(result);
            return Ok(());
        }
        // `sort_by_key` calls a key extractor once per element, then sorts in place by key.
        if matches!(self.heap.get(h), Obj::List(_)) && method == "sort_by_key" {
            let result = self.list_sort_by_key(h, args, span)?;
            self.push(result);
            return Ok(());
        }
        // `min_by`/`max_by` call a key extractor once per element (re-entrant, may GC), then return the
        // ELEMENT with the extremal key — same rooting discipline as `sort_by_key`.
        if matches!(self.heap.get(h), Obj::List(_)) && matches!(method, "min_by" | "max_by") {
            let result = self.list_min_max_by(h, args, method == "max_by", span)?;
            self.push(result);
            return Ok(());
        }
        // Concurrency C4: `Channel` / `Shared` methods mutate the heap object in place (and `update`
        // re-enters the VM), so dispatch them directly off the handle, like the core-type methods.
        if matches!(self.heap.get(h), Obj::Channel(_)) {
            let result = self.channel_method(h, method, &args, span)?;
            if self.suspend.is_some() || self.send_suspend.is_some() {
                // B1: `recv` parked this fiber (re-rooted the receiver itself); or a bounded `send`
                // parked it on a full channel (re-rooted receiver + value). Either way, no result push.
                return Ok(());
            }
            self.push(result);
            return Ok(());
        }
        // A cursor (`Obj::Iter`, the `Iterable` `.iter()` result) exposes `.next()` (advance the
        // snapshot, idempotent `None` past the end) and `.iter()` (returns self — every Iterator IS
        // Iterable, idempotently). Intrinsic, like the generator arm just below.
        if matches!(self.heap.get(h), Obj::Iter { .. }) {
            if !args.is_empty() {
                return Err(self.err(format!("a cursor's '{method}' takes no arguments"), span));
            }
            match method {
                "iter" => self.push(recv), // idempotent: iter() on a cursor returns self
                "next" => {
                    let Obj::Iter { items, pos } = self.heap.get_mut(h) else {
                        unreachable!()
                    };
                    let item = if *pos < items.len() {
                        let item = items[*pos];
                        *pos += 1;
                        Some(item)
                    } else {
                        None
                    };
                    let result = match item {
                        Some(v) => self.alloc_enum("Option", "Some", vec![v]),
                        None => self.alloc_enum("Option", "None", Vec::new()),
                    };
                    self.push(result);
                }
                _ => {
                    return Err(self.err(
                        format!("a cursor has no method '{method}' (only `next()`/`iter()`)"),
                        span,
                    ));
                }
            }
            return Ok(());
        }
        // Experimental generators — `.next()` is intrinsic (resumes the coroutine), so a generator
        // result drives `for x in g():` through the same lazy `next()` step as a struct iterator.
        // `.iter()` on a generator returns self (a generator IS an Iterator, hence Iterable).
        if matches!(self.heap.get(h), Obj::Generator(_)) {
            if method == "iter" && args.is_empty() {
                self.push(recv); // idempotent: a generator's iter() is itself
                return Ok(());
            }
            if method != "next" || !args.is_empty() {
                return Err(self.err(
                    format!("a generator has no method '{method}' (only `next()`)"),
                    span,
                ));
            }
            let result = self.generator_next(h, span)?;
            self.push(result);
            return Ok(());
        }
        // UNIFIED NATIVE-HANDLE DISPATCH for every reserved handle (concurrency + io/net). ONE match
        // maps the receiver to its `&'static str` key; a `None` short-circuits straight to the hot
        // collection arms below (this REPLACES the eight per-handle `if matches!` probes that used to
        // sit on that path). For a handle, a BODIED Chezzi method compiled to bytecode (generic like
        // `Executor.submit_result`, or plain like `Reader.lines`) dispatches FIRST; a miss (`Ok(false)`)
        // falls through to the name-keyed native op body below, BYTE-IDENTICALLY — including each tail
        // (Socket/Listener `poll_park`, Writer `stream_halt`). The single match makes it structurally
        // impossible to add a handle and forget bodied dispatch (the check-OK/run-fault class): adding a
        // variant here auto-enables it. EXCLUDES List/Map/Set — they harvest no bodied methods and their
        // hot `core_method` arm is on the M19 hot path; keep this match precise to the 8 handles.
        let handle_key = match self.heap.get(h) {
            Obj::Shared(_) => Some("Shared"),
            Obj::RwShared(_) => Some("RwShared"),
            Obj::Atomic(_) => Some("Atomic"),
            Obj::AtomicInt(_) => Some("AtomicInt"),
            Obj::Executor(_) => Some("Executor"),
            Obj::Socket(_) => Some("Socket"),
            Obj::Listener(_) => Some("Listener"),
            Obj::Writer(_) => Some("Writer"),
            Obj::Reader(_) => Some("Reader"),
            _ => None,
        };
        if let Some(key) = handle_key {
            if self.try_native_bodied_method(key, method, recv, &args, argc, ic, span)? {
                return Ok(());
            }
            // Native op body per handle — identical to the retired per-arm dispatch, tails included.
            let result = match key {
                "Shared" => self.shared_method(h, method, &args, span)?,
                "RwShared" => self.rwshared_method(h, method, &args, span)?,
                "Atomic" => self.atomic_method(h, method, &args, span)?,
                "AtomicInt" => self.atomic_int_method(h, method, &args, span)?,
                "Executor" => self.executor_method(h, method, &args, span)?,
                // D6: `Socket` / `Listener` ops operate on the fd in the `Arc`'d core and may park the
                // fiber on the netpoller (a would-block `read`/`write`/`accept`); gate the result-push
                // on `poll_park` (mirrors the channel `recv` park gate, but routed to the poller).
                "Socket" => {
                    let r = self.socket_method(h, method, &args, span)?;
                    if self.poll_park.is_some() {
                        return Ok(()); // D6: the op `WouldBlock`ed and re-rooted the receiver itself.
                    }
                    r
                }
                "Listener" => {
                    let r = self.listener_method(h, method, &args, span)?;
                    if self.poll_park.is_some() {
                        return Ok(());
                    }
                    r
                }
                // R2 / N1: a `stdout()`-backed `write` routes through `emit_out`, a NO-OP once the
                // streamed reader has died. `writer_method` then returns `Ok`, so — exactly like
                // `print` (line ~186) and `invoke_native` (line ~339) — the deterministic broken-pipe
                // halt must be raised HERE at the call site, or a `loop: w.write(...)` into a dead
                // pipe spins forever, growing the unbounded stream queue without bound (6f8bb5c).
                // `stream_halt` is inert off the streaming CLI path.
                //
                // W7-5d — gated on THIS call having emitted to stdout, for the reason spelled out at
                // `invoke_native`: unguarded, a dead stdout also faulted a write to a FILE-backed or
                // `stderr()`-backed `Writer`, which never touched the broken pipe.
                "Writer" => {
                    let writes_before = self.stdout_writes;
                    let r = self.writer_method(h, method, &args, span)?;
                    if self.stdout_writes != writes_before
                        && let Some(halt) = self.stream_halt(span)
                    {
                        return Err(halt);
                    }
                    r
                }
                // R2b: `Reader` ops are synchronous blocking file reads — no netpoller/`poll_park`, and
                // NO `stream_halt` check (a Reader never emits to stdout/stderr).
                "Reader" => self.reader_method(h, method, &args, span)?,
                _ => unreachable!("handle_key yields only the 8 handles above"),
            };
            self.push(result);
            return Ok(());
        }
        // `.iter()` on a built-in collection (str/list/map/set/bytes/bytearray) → a FRESH cursor that
        // SNAPSHOTS the current contents in the SAME order/elements as `for x in X` (list/set elems,
        // map → keys, str → per-char str, bytes/bytearray → per-byte int). Reuses `drain_iterable`
        // (the for-loop's single source of truth), then wraps the snapshot in an `Obj::Iter`. Placed
        // BEFORE the per-type dispatch so it intercepts `iter` for every collection in one spot; a
        // collection has no user-defined `iter`, so there is no precedence concern.
        if method == "iter"
            && args.is_empty()
            && matches!(
                self.heap.get(h),
                Obj::Str(_)
                    | Obj::List(_)
                    | Obj::Map(_)
                    | Obj::Set(_)
                    | Obj::Bytes(_)
                    | Obj::ByteArray(_)
            )
        {
            // `drain_iterable` may alloc (str per-char); root the receiver across the call.
            self.push(recv);
            let items = self.drain_iterable(recv, span)?;
            self.pop(); // unroot receiver
            let cursor = self.heap.alloc(Obj::Iter { items, pos: 0 });
            self.push(Value::obj(cursor));
            return Ok(());
        }
        // Built-in container methods, dispatched off the handle BEFORE the clone-match below so
        // `list.push`/`bytearray.push` mutate the heap object in place (the match clones the Obj):
        //   `str`/`list`/`map`/`set` → `core_method` (M6),
        //   `bytes`                  → `bytes_method` (immutable: only `decode() -> str`),
        //   `bytearray`              → `bytearray_method` (`len`/`push`/`pop`/`extend`/`decode`;
        //                              separate from `core_method`, same in-place `get_mut` discipline).
        enum Container {
            Core,
            Bytes,
            ByteArray,
        }
        let container = match self.heap.get(h) {
            Obj::Str(_) | Obj::List(_) | Obj::Map(_) | Obj::Set(_) => Some(Container::Core),
            Obj::Bytes(_) => Some(Container::Bytes),
            Obj::ByteArray(_) => Some(Container::ByteArray),
            _ => None,
        };
        if let Some(container) = container {
            let dispatched = match container {
                Container::Core => self.core_method(h, method, &args, span),
                Container::Bytes => self.bytes_method(h, method, &args, span),
                Container::ByteArray => self.bytearray_method(h, method, &args, span),
            };
            let result = match dispatched {
                Ok(v) => v,
                // W6-3 — MISS-only fallback to the intrinsic `Index`/`IndexSet`/`Slice`/`Hashable`
                // methods the checker grants these built-ins. NAME-GATED because a dispatcher error
                // is not necessarily a name miss (`Set.add` on a cyclic key, `list.pop` on empty…)
                // and rewriting an existing fault message would be a regression; none of the three
                // dispatchers owns any of the gated names, so the gate is exact. Zero cost unless the
                // dispatch already failed.
                Err(e) => {
                    // W7-8 adds `as_path` (the `PathLike` grant on str/bytes/bytearray) to the gate;
                    // M23 adds `eq` (the `Eq` grant on `str` — the one scalar that is heap-backed and
                    // so lands in this container dispatcher rather than the inline-scalar path).
                    if !matches!(
                        method,
                        "index" | "set_index" | "slice" | "hash" | "as_path" | "eq"
                    ) {
                        return Err(e);
                    }
                    match self.intrinsic_proto_method(recv, method, &args, span)? {
                        Some(v) => v,
                        None => return Err(e),
                    }
                }
            };
            self.push(result);
            return Ok(());
        }
        self.ensure_module_faulted(h); // D1: `module.fn(...)` on a not-yet-faulted worker module
        match self.heap.get(h).clone() {
            // `module.fn(args)` — plain call on the looked-up member, no `self`.
            Obj::Module(m) => {
                let member = m
                    .index
                    .get(method)
                    .map(|&i| m.slots[i as usize])
                    .ok_or_else(|| {
                        self.err(
                            format!("module '{}' has no member '{method}'", m.name),
                            span,
                        )
                    })?;
                // W7-8 — a module member can now be a BODIED Chezzi fn (`std.fs`'s `PathLike`
                // wrappers), not only an `Obj::Native`. `do_call` FLATTENS such a callee (installs the
                // frame for the *running* `run_until` to execute), which is only correct on the real
                // dispatch-loop path. A re-entrant caller passes `NO_IC` (`defer fs.remove_file(p)`
                // runs during frame teardown, with no loop to hand the frame to) — it must use the
                // synchronous `invoke_value`, exactly like the struct/enum arms below. Before this the
                // arm was unconditional and safe only because every module member was a native.
                if ic != NO_IC {
                    self.stack.push(member);
                    self.stack.extend(args);
                    self.do_call(argc, span)
                } else {
                    let v = self.invoke_value(member, args, span)?;
                    self.push(v);
                    Ok(())
                }
            }
            Obj::Struct { tid, fields, .. } => {
                // Resolve the type IDENTITY KEY from the instance's dense `tid` (O(1) index) — the
                // instance no longer carries a per-instance `name`. Warm method-dispatch path.
                let name = self.struct_name_of_tid(tid);
                // Fix A — resolve `(proto, module_idx)` WITHOUT cloning the whole StructDef (its
                // `fields` Vec + `methods` HashMap). On a megamorphic / sticky-generic site this slow
                // path runs per call, so the per-miss StructDef clone dwarfed the dispatch itself. We
                // bump the cheap `Arc<Program>` refcount (read-only, never alias-mutated) so the
                // immutable `structs` borrow is released before the later `&mut self` calls.
                let prog = Arc::clone(&self.program);
                let def = prog
                    .structs
                    .get(name)
                    .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
                let resolved = def.methods.get(method).copied();
                let def_module_idx = def.module_idx;
                if let Some(proto) = resolved {
                    let home = self.module_objs[def_module_idx];
                    self.check_proto_arity(proto, argc + 1, span)?;
                    // Experimental generators — a generator method allocates rather than running (else
                    // its `Op::Yield` would poison the host run). Covers both the IC-flatten and the
                    // re-entrant `run_proto` paths below; never IC-cached (it returns, not push-frame).
                    if self.program.protos[proto].is_generator {
                        let mut gen_args = Vec::with_capacity(argc + 1);
                        gen_args.push(recv);
                        gen_args.extend(args);
                        let g = self.alloc_generator(proto, home, None, gen_args);
                        self.push(g);
                        return Ok(());
                    }
                    // M19 N-way poly — fill the next free way so the next call at this site hits the fast
                    // path above (only for the dispatch-loop path: a real `ic`, a registered layout
                    // `tid`). When all ways are occupied AND this `tid` is new, latch `sticky` so the
                    // site stops probing the (full) ways and goes straight here, mirroring the binop
                    // quickening's one-way `Q_GENERIC` deopt — a megamorphic site never thrashes.
                    if ic != NO_IC && tid != TID_NONE {
                        let site = &mut self.method_ic[ic as usize];
                        if !site.sticky {
                            let cell = MethodIcCell {
                                tid,
                                proto,
                                module_idx: def_module_idx as u32,
                            };
                            if let Some(free) = site.ways.iter_mut().find(|w| w.tid == TID_NONE) {
                                *free = cell;
                            } else {
                                // All ways occupied and `tid` is distinct from every one of them (else
                                // the fast path would have hit) — the site is megamorphic; go sticky.
                                site.sticky = true;
                            }
                        }
                        // Flatten: install the frame in place and let the running `run_until` execute it
                        // (mirrors the IC fast path + the `Op::Call` flatten). The re-entrant callers
                        // pass NO_IC and so keep the synchronous `run_proto` path below.
                        let base = self.stack.len();
                        self.stack.push(recv);
                        self.stack.extend(args);
                        return self.push_frame_in_place(proto, home, None, base, span);
                    }
                    let mut call_args = Vec::with_capacity(argc + 1);
                    call_args.push(recv);
                    call_args.extend(args);
                    let v = self.run_proto(proto, home, None, call_args, true, false, span)?;
                    if self.paused() {
                        return Ok(()); // B1/D3: the method parked on a blocking `recv` or yielded.
                    }
                    self.push(v);
                    return Ok(());
                }
                // No method named `method`: fall back to a function-typed *field* — `recv.f(args)`
                // where `f` holds a function value (the checker verified `f: fn(...) -> ...`).
                // Invoked as a value (no `self` bound — it's not a method). Positional layout:
                // resolve the field name->index from the StructDef, then index the flat `fields`.
                let fidx = self
                    .program
                    .structs
                    .get(name)
                    .and_then(|d| d.fields.iter().position(|f| f == method));
                if let Some(fval) = fidx.and_then(|i| fields.get(i).copied()) {
                    let v = self.invoke_value(fval, args, span)?;
                    if self.paused() {
                        return Ok(()); // B1/D3: the function-field call parked on `recv` or yielded.
                    }
                    self.push(v);
                    return Ok(());
                }
                // A user iterator struct (`next`, no explicit `iter`) IS Iterable — `.iter()` returns
                // self (idempotent), letting it flow into an `[S: Iterable[T]]` body. Mirrors interp.
                if method == "iter"
                    && args.is_empty()
                    && self
                        .program
                        .structs
                        .get(name)
                        .is_some_and(|d| d.methods.contains_key("next"))
                {
                    self.push(recv);
                    return Ok(());
                }
                // ROOT REDESIGN — render the BARE display name (not the identity key) in the error.
                // Resolved BEFORE the W6-3 fallback below so the `name` borrow of `self` ends here.
                let display = self
                    .program
                    .structs
                    .get(name)
                    .map(|d| d.display_name.clone())
                    .unwrap_or_else(|| crate::compiler::bare_display(name));
                // W6-3 — a ZERO-FIELD struct with no `hash` method is intrinsically `Hashable`
                // (`proto.rs`), so `x.hash()` in an erased `[T: Hashable]` body must answer with the
                // SAME constant `struct_hash` gives it as a Map/Set key. Miss-only, so a struct that
                // DEFINES `hash` (or any other protocol method) already dispatched above.
                if let Some(v) = self.intrinsic_proto_method(recv, method, &args, span)? {
                    self.push(v);
                    return Ok(());
                }
                Err(self.err(format!("struct '{display}' has no method '{method}'"), span))
            }
            // Enum method dispatch (name-resolved, like structs). Enums are type-erased — no `tid`,
            // so the method IC is skipped (follow-up lever); we resolve `enum_methods[key][method]`
            // off the variant's `enum_name` and dispatch with the same `self`-binding path structs use.
            Obj::Enum { variant_id, .. } => {
                let prog = Arc::clone(&self.program);
                let enum_key = self.enum_names(variant_id).0.to_string();
                let resolved = prog
                    .enum_methods
                    .get(&enum_key)
                    .and_then(|ms| ms.get(method).copied());
                if let Some(proto) = resolved {
                    // An enum method's home module is the enum's declaring module (recorded in
                    // `enum_home`), so its body resolves top-level names against the right globals.
                    let home = self.module_objs[self.enum_home_module(&enum_key)];
                    self.check_proto_arity(proto, argc + 1, span)?;
                    if self.program.protos[proto].is_generator {
                        let mut gen_args = Vec::with_capacity(argc + 1);
                        gen_args.push(recv);
                        gen_args.extend(args);
                        let g = self.alloc_generator(proto, home, None, gen_args);
                        self.push(g);
                        return Ok(());
                    }
                    // Flatten only on the real dispatch-loop path (`ic != NO_IC`); re-entrant callers
                    // pass `NO_IC` and use the synchronous `run_proto` path (mirrors the struct arm).
                    if ic != NO_IC {
                        let base = self.stack.len();
                        self.stack.push(recv);
                        self.stack.extend(args);
                        return self.push_frame_in_place(proto, home, None, base, span);
                    }
                    let mut call_args = Vec::with_capacity(argc + 1);
                    call_args.push(recv);
                    call_args.extend(args);
                    let v = self.run_proto(proto, home, None, call_args, true, false, span)?;
                    if self.paused() {
                        return Ok(());
                    }
                    self.push(v);
                    return Ok(());
                }
                // D1 (W6-3's rule, applied to the one dispatch arm that lacked it) — the checker
                // grants `Eq` intrinsically to an `enum`, and `Option`/`Result` ARE `Obj::Enum` at
                // runtime, so an erased `[T: Eq]` body's `a.eq(b)` must answer here. Miss-only, so
                // an enum that DEFINES the method already dispatched above. Without this the grant
                // is check-OK-then-`has no method 'eq'`, the class this whole milestone closes.
                if let Some(v) = self.intrinsic_proto_method(recv, method, &args, span)? {
                    self.push(v);
                    return Ok(());
                }
                let display = crate::compiler::bare_display(&enum_key);
                Err(self.err(format!("type {display} has no method '{method}'"), span))
            }
            // Newtype method dispatch (name-resolved, like enums). Resolves `newtype_methods[key]
            // [method]` off the wrapper's `type_key`. The underlying's methods are NOT inherited.
            Obj::NewType { type_key, .. } => {
                let prog = Arc::clone(&self.program);
                let nt_key = type_key.to_string();
                let resolved = prog
                    .newtype_methods
                    .get(&nt_key)
                    .and_then(|ms| ms.get(method).copied());
                if let Some(proto) = resolved {
                    let home = self.module_objs[self.newtype_home_module(&nt_key)];
                    self.check_proto_arity(proto, argc + 1, span)?;
                    if self.program.protos[proto].is_generator {
                        let mut gen_args = Vec::with_capacity(argc + 1);
                        gen_args.push(recv);
                        gen_args.extend(args);
                        let g = self.alloc_generator(proto, home, None, gen_args);
                        self.push(g);
                        return Ok(());
                    }
                    if ic != NO_IC {
                        let base = self.stack.len();
                        self.stack.push(recv);
                        self.stack.extend(args);
                        return self.push_frame_in_place(proto, home, None, base, span);
                    }
                    let mut call_args = Vec::with_capacity(argc + 1);
                    call_args.push(recv);
                    call_args.extend(args);
                    let v = self.run_proto(proto, home, None, call_args, true, false, span)?;
                    if self.paused() {
                        return Ok(());
                    }
                    self.push(v);
                    return Ok(());
                }
                // W6-3 — a SCALAR-underlying newtype intrinsically satisfies `Add`/`Sub`/`Mul`/`Div`/
                // `Mod`/`Comparable` (`proto.rs`: its same-type `+`/`<` auto-flow to the underlying's
                // native op, with no user method), so `a.add(b)`/`a.compare(b)` in an erased body
                // answers with what `a + b` / `a < b` produce. Miss-only, so a newtype that DEFINES
                // one of those methods got ITS method above (never shadowed) — and for that receiver
                // the method and operator forms DIVERGE, because the operator always auto-flows to the
                // underlying's native op. Known, out of scope to reconcile here: `docs/gaps.md` W6-3d.
                if let Some(v) = self.intrinsic_proto_method(recv, method, &args, span)? {
                    self.push(v);
                    return Ok(());
                }
                let display = crate::compiler::bare_display(&nt_key);
                Err(self.err(format!("type {display} has no method '{method}'"), span))
            }
            // W6-3 — a BOXED scalar (`Obj::BigInt`, and any other Obj-tagged scalar) is Obj-tagged, so
            // it never reaches the inline-scalar miss above and lands here instead: it must answer the
            // same intrinsic arith/`Hashable`/`Comparable` methods its inline twin does.
            _ => {
                if let Some(v) = self.intrinsic_proto_method(recv, method, &args, span)? {
                    self.push(v);
                    return Ok(());
                }
                Err(self.err(
                    format!("type {} has no method '{method}'", self.type_name(recv)),
                    span,
                ))
            }
        }
    }

    /// `Type.method(args)` — STATIC (associated) method dispatch (the "no self ⇒ static" rule).
    /// Stack: `[arg0, …]` — exactly `argc` values, NO receiver. Resolves `method` in the named
    /// struct's (`program.structs[key].methods`) or enum's (`program.enum_methods[key]`) method
    /// table by `type_key`; the body's home module is the type's declaring module. Pushes a frame
    /// holding just the args (arity == argc, no `self` slot) and runs it via `push_frame_in_place`
    /// (the dispatch-loop path) — structurally identical to enum-method dispatch minus the receiver.
    /// A static generator allocates rather than running, mirroring the instance arms.
    pub(super) fn do_static_call(
        &mut self,
        type_key: &str,
        method: &str,
        argc: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let prog = Arc::clone(&self.program);
        // Resolve the proto + home module from the struct table first, then the enum table. The
        // compiler only emits `CallStatic` for a static method that exists on a known struct/enum,
        // so a miss here is an internal invariant break — surface it as a clear runtime error.
        let (proto, home_idx) = if let Some(def) = prog.structs.get(type_key) {
            match def.methods.get(method).copied() {
                Some(p) => (p, def.module_idx),
                None => {
                    return Err(self.err(
                        format!(
                            "type '{}' has no static method '{method}'",
                            def.display_name
                        ),
                        span,
                    ));
                }
            }
        } else if let Some(ms) = prog.enum_methods.get(type_key) {
            match ms.get(method).copied() {
                Some(p) => (p, self.enum_home_module(type_key)),
                None => {
                    let display = crate::compiler::bare_display(type_key);
                    return Err(self.err(
                        format!("type {display} has no static method '{method}'"),
                        span,
                    ));
                }
            }
        } else {
            let display = crate::compiler::bare_display(type_key);
            return Err(self.err(
                format!("type {display} has no static method '{method}'"),
                span,
            ));
        };
        // A static method has NO receiver, so its argument count is `argc` exactly (no `+ 1`).
        self.check_proto_arity(proto, argc, span)?;
        let home = self.module_objs[home_idx];
        if self.program.protos[proto].is_generator {
            let at = self.stack.len() - argc;
            let gen_args: Vec<Value> = self.stack.split_off(at);
            let g = self.alloc_generator(proto, home, None, gen_args);
            self.push(g);
            return Ok(());
        }
        // The `argc` args are already contiguous on the operand stack (pushed by the compiler in
        // order, no receiver). Install the frame in place over them — the running `run_until`
        // executes the body and its `Return` pushes the result.
        let base = self.stack.len() - argc;
        self.push_frame_in_place(proto, home, None, base, span)
    }

    /// Higher-order list methods `map` / `filter` / `fold`. `src_h` is the receiver list.
    ///
    /// SNAPSHOT semantics: iteration walks a copy of the receiver's elements taken at call time, so
    /// a callback that MUTATES the receiver (e.g. `xs.pop()`/`xs.push(..)`) does NOT perturb the
    /// iteration sequence — it always visits exactly the elements present when the HOF was invoked.
    /// This (a) matches comprehensions/for-loops (`Op::ListClone`) and `list_sort_by`/`sort_by_key`
    /// (which snapshot), (b) matches Python `map`/`filter`, and (c) is OOB-safe: indexing the
    /// original live list while a callback shrinks it would panic (regression:
    /// `map_shrinking_callback_no_panic`).
    ///
    /// GC discipline: each element is fed to a closure via `invoke_value`, which runs nested VM
    /// frames that can trigger GC at instruction boundaries. To keep the GC from collecting in-flight
    /// heap values, the source list, the snapshot list, the partially-built result list (map/filter),
    /// and the fold accumulator are all kept rooted on the operand stack across the iteration. Returns
    /// the result (caller pushes it).
    pub(super) fn list_hof(
        &mut self,
        src_h: GcRef,
        method: &str,
        mut args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // ROOT the source list on the operand stack: a method receiver is popped before dispatch, so
        // an inline temporary (`make().map(..)`) is otherwise unrooted and the callback's GC could
        // collect it before we snapshot.
        self.push(Value::obj(src_h));
        // Take a SNAPSHOT now: iterate the receiver's elements as of call
        // time so a callback that shrinks/grows the receiver mid-iteration neither perturbs the
        // sequence nor indexes past the live (now-shorter) Vec. The snapshot is heap-allocated and
        // rooted on the operand stack so its elements survive the callback's collections.
        let snap_h = {
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!("list_hof on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::obj(snap_h)); // ROOT the snapshot
        let n = match self.heap.get(snap_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!(),
        };
        match method {
            "map" | "filter" => {
                if args.len() != 1 {
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(self.err(
                        format!("'{method}' expects 1 argument(s), got {}", args.len()),
                        span,
                    ));
                }
                let f = args.swap_remove(0);
                let is_filter = method == "filter";
                // ROOT the result list too.
                let res_h = self.heap.alloc(Obj::List(Vec::new()));
                self.push(Value::obj(res_h));
                for i in 0..n {
                    // Read from the rooted SNAPSHOT, not the live receiver: a callback that shrinks
                    // the receiver must not affect this index (stays valid, no OOB).
                    let elem = match self.heap.get(snap_h) {
                        Obj::List(v) => v[i],
                        _ => unreachable!(),
                    };
                    // May GC; source, snapshot, and result lists are rooted, so elements survive.
                    let out = self.guarded(|vm| vm.invoke_value(f, vec![elem], span))?;
                    if is_filter {
                        match out.as_bool() {
                            Some(true) => {
                                if let Obj::List(items) = self.heap.get_mut(res_h) {
                                    items.push(elem);
                                }
                            }
                            Some(false) => {}
                            None => {
                                self.pop(); // unroot result
                                self.pop(); // unroot snapshot
                                self.pop(); // unroot source
                                return Err(self.err(
                                    format!(
                                        "filter predicate must return bool, got {}",
                                        self.type_name(out)
                                    ),
                                    span,
                                ));
                            }
                        }
                    } else if let Obj::List(items) = self.heap.get_mut(res_h) {
                        items.push(out);
                    }
                }
                self.pop(); // unroot result
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                Ok(Value::obj(res_h))
            }
            "fold" => {
                if args.len() != 2 {
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(self.err(
                        format!("'fold' expects 2 argument(s), got {}", args.len()),
                        span,
                    ));
                }
                let f = args.swap_remove(1);
                let init = args.swap_remove(0);
                // ROOT the accumulator: push init, remember its slot, and replace in place each step.
                // `acc_slot` sits below every nested frame's base (frames push above the current
                // stack top and pop back to it), so the index stays valid across `invoke_value`.
                self.push(init);
                let acc_slot = self.stack.len() - 1;
                for i in 0..n {
                    // Read from the rooted SNAPSHOT (see map/filter): OOB-safe under a shrinking
                    // callback.
                    let elem = match self.heap.get(snap_h) {
                        Obj::List(v) => v[i],
                        _ => unreachable!(),
                    };
                    let acc = self.stack[acc_slot];
                    let new = self.guarded(|vm| vm.invoke_value(f, vec![acc, elem], span))?;
                    self.stack[acc_slot] = new;
                }
                let acc = self.pop(); // unroot accumulator
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                Ok(acc)
            }
            // take_while: NEW list of the prefix while `pred` holds; stops (pred not called) at the
            // first false. Reads the rooted SNAPSHOT — a pred that shrinks the receiver can't OOB.
            "take_while" | "drop_while" => {
                if args.len() != 1 {
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(self.err(
                        format!("'{method}' expects 1 argument(s), got {}", args.len()),
                        span,
                    ));
                }
                let f = args.swap_remove(0);
                let is_drop = method == "drop_while";
                let res_h = self.heap.alloc(Obj::List(Vec::new()));
                self.push(Value::obj(res_h)); // ROOT the result list
                let mut cut = n; // first index where pred is false (n = all-true)
                for i in 0..n {
                    let elem = match self.heap.get(snap_h) {
                        Obj::List(v) => v[i],
                        _ => unreachable!(),
                    };
                    let out = self.guarded(|vm| vm.invoke_value(f, vec![elem], span))?;
                    match out.as_bool() {
                        Some(true) => {
                            if !is_drop && let Obj::List(items) = self.heap.get_mut(res_h) {
                                items.push(elem);
                            }
                        }
                        Some(false) => {
                            cut = i;
                            break;
                        }
                        None => {
                            self.pop(); // unroot result
                            self.pop(); // unroot snapshot
                            self.pop(); // unroot source
                            return Err(self.err(
                                format!(
                                    "{method} predicate must return bool, got {}",
                                    self.type_name(out)
                                ),
                                span,
                            ));
                        }
                    }
                }
                if is_drop {
                    // Collect the snapshot suffix from the first non-matching index.
                    for i in cut..n {
                        let elem = match self.heap.get(snap_h) {
                            Obj::List(v) => v[i],
                            _ => unreachable!(),
                        };
                        if let Obj::List(items) = self.heap.get_mut(res_h) {
                            items.push(elem);
                        }
                    }
                }
                self.pop(); // unroot result
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                Ok(Value::obj(res_h))
            }
            // count: number of snapshot elements satisfying `pred`. position: index of the FIRST
            // match as Option[int] (short-circuits), else None.
            "count" | "position" => {
                if args.len() != 1 {
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(self.err(
                        format!("'{method}' expects 1 argument(s), got {}", args.len()),
                        span,
                    ));
                }
                let f = args.swap_remove(0);
                let is_pos = method == "position";
                let mut count = 0_i64;
                let mut found: Option<usize> = None;
                for i in 0..n {
                    let elem = match self.heap.get(snap_h) {
                        Obj::List(v) => v[i],
                        _ => unreachable!(),
                    };
                    let out = self.guarded(|vm| vm.invoke_value(f, vec![elem], span))?;
                    match out.as_bool() {
                        Some(true) => {
                            if is_pos {
                                found = Some(i);
                                break;
                            }
                            count += 1;
                        }
                        Some(false) => {}
                        None => {
                            self.pop(); // unroot snapshot
                            self.pop(); // unroot source
                            return Err(self.err(
                                format!(
                                    "{method} predicate must return bool, got {}",
                                    self.type_name(out)
                                ),
                                span,
                            ));
                        }
                    }
                }
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                if is_pos {
                    Ok(match found {
                        Some(i) => self.alloc_enum("Option", "Some", vec![Value::int(i as i64)]),
                        None => self.alloc_enum("Option", "None", vec![]),
                    })
                } else {
                    Ok(Value::int(count))
                }
            }
            _ => unreachable!("list_hof called with non-HOF method {method}"),
        }
    }

    /// A sort callback must not mutate the list being sorted (W8-4): the list is a `Result`-less
    /// snapshot-and-write-back, so a mutation the write-back would erase is a contract violation,
    /// not something to discard silently. Compares the live list at `src_h` to the rooted snapshot
    /// at `snap_h` by raw `Value` word (`Value`'s derived `PartialEq` is bit equality — NaN-safe,
    /// and it catches a length change AND a same-length element write). Call this immediately
    /// before the write-back, at all three sort call sites.
    pub(super) fn sort_mutation_check(
        &mut self,
        src_h: GcRef,
        snap_h: GcRef,
        method: &str,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let changed = match (self.heap.get(src_h), self.heap.get(snap_h)) {
            (Obj::List(cur), Obj::List(snap)) => cur != snap,
            _ => true,
        };
        if changed {
            return Err(self.err(
                format!(
                    "list modified during '{method}' -- a sort callback must not mutate the list being sorted"
                ),
                span,
            ));
        }
        Ok(())
    }

    /// `xs.sort_by(cmp)` — stable in-place sort driven by a Chezzi comparator `fn(T, T) -> int`
    /// (negative = a before b, positive = a after b, zero = equal). The comparator re-enters the VM
    /// and may GC, so we never hold the elements in an unrooted Rust `Vec`: the source list stays
    /// rooted on the operand stack, and the merge sort permutes plain `usize` **indices**, re-reading
    /// elements from the rooted heap object on each comparison. The final permutation is materialised
    /// only after all comparator calls finish (no GC in between). Returns `nil`.
    pub(super) fn list_sort_by(
        &mut self,
        src_h: GcRef,
        mut args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if args.len() != 1 {
            return Err(self.err(
                format!("'sort_by' expects 1 argument(s), got {}", args.len()),
                span,
            ));
        }
        let cmp = args.swap_remove(0);
        // Root the source list itself: a method receiver is popped before dispatch, so an inline
        // temporary (`make().sort_by(...)`) is otherwise unrooted and the comparator's GC could
        // collect it before the write-back.
        self.push(Value::obj(src_h));
        // Sort a SNAPSHOT taken now: a comparator that mutates the source
        // list mid-sort must not perturb the ordering, and its mutations are discarded by the final
        // write-back. The snapshot list is itself heap-allocated and rooted on the operand stack so
        // its elements survive the comparator's collections.
        let snap_h = {
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!("list_sort_by on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::obj(snap_h)); // ROOT the snapshot
        let n = match self.heap.get(snap_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!(),
        };
        let order = match self.msort_indices(snap_h, (0..n).collect(), cmp, span) {
            Ok(o) => o,
            Err(e) => {
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                return Err(e);
            }
        };
        if let Err(e) = self.sort_mutation_check(src_h, snap_h, "sort_by", span) {
            self.pop(); // unroot snapshot
            self.pop(); // unroot source
            return Err(e);
        }
        // No comparator calls remain, so no GC: read the rooted snapshot and write the result back.
        let reordered: Vec<Value> = match self.heap.get(snap_h) {
            Obj::List(v) => order.iter().map(|&i| v[i]).collect(),
            _ => unreachable!(),
        };
        if let Obj::List(v) = self.heap.get_mut(src_h) {
            *v = reordered;
        }
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(Value::nil())
    }

    /// Stable top-down merge sort over `idx` (positions into the rooted list `src_h`), comparing
    /// elements via the Chezzi comparator `cmp`.
    pub(super) fn msort_indices(
        &mut self,
        src_h: GcRef,
        idx: Vec<usize>,
        cmp: Value,
        span: Span,
    ) -> Result<Vec<usize>, RuntimeError> {
        let n = idx.len();
        if n <= 1 {
            return Ok(idx);
        }
        let mut idx = idx;
        let right = idx.split_off(n / 2);
        let left = self.msort_indices(src_h, idx, cmp, span)?;
        let right = self.msort_indices(src_h, right, cmp, span)?;
        let mut out = Vec::with_capacity(n);
        let (mut li, mut ri) = (0, 0);
        while li < left.len() && ri < right.len() {
            let a = match self.heap.get(src_h) {
                Obj::List(v) => v[left[li]],
                _ => unreachable!(),
            };
            let b = match self.heap.get(src_h) {
                Obj::List(v) => v[right[ri]],
                _ => unreachable!(),
            };
            // `<= 0` keeps the left element first on ties → stable.
            if self.compare_with(cmp, a, b, span)? <= 0 {
                out.push(left[li]);
                li += 1;
            } else {
                out.push(right[ri]);
                ri += 1;
            }
        }
        out.extend_from_slice(&left[li..]);
        out.extend_from_slice(&right[ri..]);
        Ok(out)
    }

    /// Run the comparator on `(a, b)` and return its int result (errors if it returns non-int).
    pub(super) fn compare_with(
        &mut self,
        cmp: Value,
        a: Value,
        b: Value,
        span: Span,
    ) -> Result<i64, RuntimeError> {
        let res = self.guarded(|vm| vm.invoke_value(cmp, vec![a, b], span))?;
        match self.int_val(res) {
            Some(n) => Ok(n),
            None => Err(self.err(
                format!(
                    "sort_by comparator must return int, got {}",
                    self.type_name(res)
                ),
                span,
            )),
        }
    }

    /// `xs.sort_by_key(f)` — stable in-place sort by a derived key `f: fn(T) -> K`. Mirrors
    /// `list_sort_by`'s GC discipline: the source list, an element snapshot, AND a parallel **keys**
    /// list are all rooted on the operand stack so the re-entrant extractor (and a Comparable-struct
    /// key's `compare`) can GC freely. Keys are computed once per element; the merge sort permutes
    /// `usize` indices, re-reading keys from the rooted keys list per comparison. Returns `nil`.
    pub(super) fn list_sort_by_key(
        &mut self,
        src_h: GcRef,
        mut args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if args.len() != 1 {
            return Err(self.err(
                format!("'sort_by_key' expects 1 argument(s), got {}", args.len()),
                span,
            ));
        }
        let f = args.swap_remove(0);
        self.push(Value::obj(src_h)); // ROOT the source list
        let snap_h = {
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!("list_sort_by_key on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::obj(snap_h)); // ROOT the snapshot
        let n = match self.heap.get(snap_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!(),
        };
        // Compute keys once per element into a rooted list. Each `invoke_value` may GC; already-pushed
        // keys survive because `keys_h` is rooted (a `Vec::push` into it does not itself GC).
        let keys_h = self.heap.alloc(Obj::List(Vec::with_capacity(n)));
        self.push(Value::obj(keys_h)); // ROOT the keys
        for i in 0..n {
            let e = match self.heap.get(snap_h) {
                Obj::List(v) => v[i],
                _ => unreachable!(),
            };
            match self.guarded(|vm| vm.invoke_value(f, vec![e], span)) {
                Ok(k) => {
                    if let Obj::List(v) = self.heap.get_mut(keys_h) {
                        v.push(k);
                    }
                }
                Err(err) => {
                    self.pop(); // unroot keys
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(err);
                }
            }
        }
        let order = match self.msort_indices_by_key(keys_h, (0..n).collect(), span) {
            Ok(o) => o,
            Err(e) => {
                self.pop(); // unroot keys
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                return Err(e);
            }
        };
        if let Err(e) = self.sort_mutation_check(src_h, snap_h, "sort_by_key", span) {
            self.pop(); // unroot keys
            self.pop(); // unroot snapshot
            self.pop(); // unroot source
            return Err(e);
        }
        // No extractor/compare calls remain, so no GC: reorder the snapshot and write back.
        let reordered: Vec<Value> = match self.heap.get(snap_h) {
            Obj::List(v) => order.iter().map(|&i| v[i]).collect(),
            _ => unreachable!(),
        };
        if let Obj::List(v) = self.heap.get_mut(src_h) {
            *v = reordered;
        }
        self.pop(); // unroot keys
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(Value::nil())
    }

    /// `xs.min()` / `xs.max()` — the extremal element by natural order. A Comparable-struct element's
    /// `compare` re-enters the VM (may GC AND may mutate the source list), so the scan runs over a
    /// GC-rooted SNAPSHOT (mirrors [`Vm::list_sort_by_key`]/[`Vm::list_min_max_by`]) — never the live
    /// source, whose length can change under a re-entrant comparator. Returns `Option[T]` — `None` on
    /// an empty list (Rust's `Iterator::min`/`max`, matching the sibling `first`/`last`/`pop`), NOT a
    /// fault. First-seen tie-break (an equal element never displaces the earlier best), matching
    /// Python's `min`/`max`.
    pub(super) fn list_reduce_extreme(
        &mut self,
        src_h: GcRef,
        is_max: bool,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let n = match self.heap.get(src_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!("list_reduce_extreme on non-list"),
        };
        if n == 0 {
            return Ok(self.alloc_enum("Option", "None", vec![]));
        }
        // A Comparable-struct element's `compare` re-enters user code, which may MUTATE (shrink) the
        // source list, so scan an immutable SNAPSHOT (mirrors `list_sort_by_key`/`list_min_max_by`) —
        // indexing the live source post-user-code would panic out of bounds. GC-rooted so its elements
        // survive `order_key`'s struct-compare allocations.
        self.push(Value::obj(src_h)); // ROOT the source list
        let snap_h = {
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!(),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::obj(snap_h)); // ROOT the snapshot
        let mut best = 0usize;
        let mut err = None;
        for i in 1..n {
            let (cur, best_v) = match self.heap.get(snap_h) {
                Obj::List(v) => (v[i], v[best]),
                _ => unreachable!(),
            };
            match self.order_key(cur, best_v, span) {
                // min: strict `<` displaces; max: strict `>` displaces. Equal never displaces → first-seen.
                Ok(ord) => {
                    if (is_max && ord.is_gt()) || (!is_max && ord.is_lt()) {
                        best = i;
                    }
                }
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let result = match self.heap.get(snap_h) {
            Obj::List(v) => v[best],
            _ => unreachable!(),
        };
        if let Some(e) = err {
            self.pop(); // unroot snapshot
            self.pop(); // unroot source
            return Err(e);
        }
        // Wrap while the snapshot is still rooted. Belt-and-braces, NOT load-bearing today:
        // `Heap::alloc` never collects, and the only collection boundary THIS PATH can reach is
        // `run_until`'s instruction boundary — the scan's last re-entry is already over, and the
        // other collection site (`Vm::sample_mem_cap`, once per task start, `sched.rs`) runs before
        // this method call, not during it. Kept because a shrinking comparator can leave `result`
        // reachable ONLY through the snapshot, so if anything between here and the caller's
        // `push` ever re-enters the VM, this is the order that is already correct.
        let some = self.alloc_enum("Option", "Some", vec![result]);
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(some)
    }

    /// `xs.min_by(f)` / `xs.max_by(f)` — the ELEMENT whose derived key `f: fn(T) -> K` is extremal.
    /// Same GC discipline as [`Vm::list_sort_by_key`]: source, an element snapshot, AND a parallel
    /// keys list are rooted so the re-entrant extractor (and a Comparable-struct key's `compare`) can
    /// GC freely. Keys are computed once per element; a linear argmin/argmax over the rooted keys picks
    /// the extremal index (first-seen tie), then the element at that index is read from the rooted
    /// snapshot. Returns `Option[T]` — `None` on an empty list (same lineage as `min`/`max`), NOT a
    /// fault.
    pub(super) fn list_min_max_by(
        &mut self,
        src_h: GcRef,
        mut args: Vec<Value>,
        is_max: bool,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let name = if is_max { "max_by" } else { "min_by" };
        if args.len() != 1 {
            return Err(self.err(
                format!("'{name}' expects 1 argument(s), got {}", args.len()),
                span,
            ));
        }
        let f = args.swap_remove(0);
        let n = match self.heap.get(src_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!("list_min_max_by on non-list"),
        };
        if n == 0 {
            return Ok(self.alloc_enum("Option", "None", vec![]));
        }
        self.push(Value::obj(src_h)); // ROOT the source list
        let snap_h = {
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!(),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::obj(snap_h)); // ROOT the snapshot
        let keys_h = self.heap.alloc(Obj::List(Vec::with_capacity(n)));
        self.push(Value::obj(keys_h)); // ROOT the keys
        for i in 0..n {
            let e = match self.heap.get(snap_h) {
                Obj::List(v) => v[i],
                _ => unreachable!(),
            };
            match self.guarded(|vm| vm.invoke_value(f, vec![e], span)) {
                Ok(k) => {
                    if let Obj::List(v) = self.heap.get_mut(keys_h) {
                        v.push(k);
                    }
                }
                Err(err) => {
                    self.pop(); // unroot keys
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(err);
                }
            }
        }
        // Linear argmin/argmax over the rooted keys (first-seen tie-break).
        let mut best = 0usize;
        let mut err = None;
        for i in 1..n {
            let (cur, best_k) = match self.heap.get(keys_h) {
                Obj::List(v) => (v[i], v[best]),
                _ => unreachable!(),
            };
            match self.order_key(cur, best_k, span) {
                Ok(ord) => {
                    if (is_max && ord.is_gt()) || (!is_max && ord.is_lt()) {
                        best = i;
                    }
                }
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let result = match self.heap.get(snap_h) {
            Obj::List(v) => v[best],
            _ => unreachable!(),
        };
        if let Some(e) = err {
            self.pop(); // unroot keys
            self.pop(); // unroot snapshot
            self.pop(); // unroot source
            return Err(e);
        }
        // Wrap while the snapshot is still rooted — see `list_reduce_extreme` for why that is
        // belt-and-braces rather than load-bearing.
        let some = self.alloc_enum("Option", "Some", vec![result]);
        self.pop(); // unroot keys
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(some)
    }

    /// Stable top-down merge sort over `idx` (positions into the rooted keys list `keys_h`), ordering
    /// by each key's natural order via [`order_key`].
    pub(super) fn msort_indices_by_key(
        &mut self,
        keys_h: GcRef,
        idx: Vec<usize>,
        span: Span,
    ) -> Result<Vec<usize>, RuntimeError> {
        let n = idx.len();
        if n <= 1 {
            return Ok(idx);
        }
        let mut idx = idx;
        let right = idx.split_off(n / 2);
        let left = self.msort_indices_by_key(keys_h, idx, span)?;
        let right = self.msort_indices_by_key(keys_h, right, span)?;
        let mut out = Vec::with_capacity(n);
        let (mut li, mut ri) = (0, 0);
        while li < left.len() && ri < right.len() {
            let a = match self.heap.get(keys_h) {
                Obj::List(v) => v[left[li]],
                _ => unreachable!(),
            };
            let b = match self.heap.get(keys_h) {
                Obj::List(v) => v[right[ri]],
                _ => unreachable!(),
            };
            // `<= Equal` keeps the left element first on ties → stable.
            if self.order_key(a, b, span)?.is_le() {
                out.push(left[li]);
                li += 1;
            } else {
                out.push(right[ri]);
                ri += 1;
            }
        }
        out.extend_from_slice(&left[li..]);
        out.extend_from_slice(&right[ri..]);
        Ok(out)
    }

    /// Natural order over two `sort_by_key` keys: a Comparable struct key dispatches to its
    /// `compare`; scalar keys (int/float/str) use the built-in [`Vm::compare`]. The checker has
    /// verified the key type is orderable.
    pub(super) fn order_key(
        &mut self,
        a: Value,
        b: Value,
        span: Span,
    ) -> Result<std::cmp::Ordering, RuntimeError> {
        if let (Some(ha), Some(hb)) = (a.as_obj(), b.as_obj())
            && matches!(self.heap.get(ha), Obj::Struct { .. })
            && matches!(self.heap.get(hb), Obj::Struct { .. })
        {
            return self.struct_compare(a, b, span);
        }
        // Numeric-newtype (`Comparable`) unwrap — mirrors `value_order` (arith.rs) so `sort_by_key`/
        // `.min()`/`.max()`/`.min_by`/`.max_by` on a `List[newtype=int/float]` order by the wrapped
        // scalar's NATIVE order (never a user `compare`). MUST precede the `is_float` fast-path: a
        // wrapper is `Obj`-tagged so `is_float()`/`is_numeric()` miss it, and a NaN newtype-float key
        // would fault. Copy `*inner` to a local first to release the immutable `heap.get` borrow
        // before the `&mut self` recursion (the sole shape difference from `&self` `value_order`).
        if let Some(ha) = a.as_obj()
            && let Obj::NewType { inner, .. } = self.heap.get(ha)
        {
            let inner = *inner;
            return self.order_key(inner, b, span);
        }
        if let Some(hb) = b.as_obj()
            && let Obj::NewType { inner, .. } = self.heap.get(hb)
        {
            let inner = *inner;
            return self.order_key(a, inner, span);
        }
        // Float keys order by `total_cmp` for the WHOLE comparison (not just the NaN case), exactly
        // mirroring `sort()`'s `value_order` Float arm — so `sort_by_key` and `sort()` agree on every
        // float pair, including `-0.0`/`+0.0` (which `partial_cmp` ranks Equal but `total_cmp` orders
        // `-0.0 < +0.0`) and NaN (deterministic, to one end). Int keys deliberately stay on the int
        // path below (`Int.cmp`): routing them through `as_f64` would lose precision past 2^53.
        if a.is_float() && b.is_float() {
            return Ok(self.float_of(a).total_cmp(&self.float_of(b)));
        }
        match self.compare(a, b) {
            Some(ord) => Ok(ord),
            // Numeric `None` means a NaN float — handled above for the Float/Float case; this arm
            // only catches a mixed int/float key pair (not reachable for a single key type K), kept
            // deterministic via `total_cmp` for safety.
            None if self.is_numeric(a) && self.is_numeric(b) => {
                Ok(self.as_f64(a).total_cmp(&self.as_f64(b)))
            }
            // Genuinely-incomparable types: unreachable from well-typed source; kept for safety.
            None => Err(self.err(
                format!(
                    "sort_by_key keys are not comparable: {} vs {}",
                    self.type_name(a),
                    self.type_name(b)
                ),
                span,
            )),
        }
    }

    /// Built-in methods on `str` / `list` (M6). The result is returned (not pushed) so the caller
    /// owns stack discipline. Multi-allocation paths (`split`) are safe: the GC only collects at
    /// instruction boundaries, never mid-opcode, so all `alloc`s here complete uninterrupted.
    /// Clone the elements of a `list`-typed argument for `concat`/`extend`. The checker guarantees
    /// the type; a non-list here is an internal invariant break, reported for safety.
    pub(super) fn expect_list_obj(
        &self,
        method: &str,
        arg: Value,
        span: Span,
    ) -> Result<Vec<Value>, RuntimeError> {
        match arg.as_obj().map(|ah| self.heap.get(ah)) {
            Some(Obj::List(items)) => Ok(items.clone()),
            _ => Err(self.err(
                format!(
                    "{method}() expects a list argument, got {}",
                    self.type_name(arg)
                ),
                span,
            )),
        }
    }

    /// Insert-or-overwrite `(hk, key, val)` into the heap map at `h` (last write wins). Used by
    /// `map.update`. On INSERT it snapshots a struct/enum/newtype key (Go value-key model) so the
    /// target map does NOT alias the source map's stored key object — otherwise mutating one map's
    /// key (e.g. via `keys()`) would silently corrupt the other. `snapshot_key` is pure alloc (no
    /// GC), so `h` stays valid across it.
    pub(super) fn map_upsert_in_heap(
        &mut self,
        h: GcRef,
        hk: u64,
        key: Value,
        val: Value,
        span: Span,
    ) -> Result<(), RuntimeError> {
        // `key`/`val` are in-flight Rust locals here (the source map's entries were cloned out), so
        // root `val` across the probe — `map_probe` roots the target map and the key itself.
        let pos = self.with_roots(&[val], |vm| vm.map_probe(h, hk, key, span))?;
        match pos {
            Some(i) => {
                let Obj::Map(m) = self.heap.get_mut(h) else {
                    unreachable!()
                };
                m.entries[i].2 = val;
            }
            None => {
                let key = self.snapshot_key(key);
                let Obj::Map(m) = self.heap.get_mut(h) else {
                    unreachable!()
                };
                m.push(hk, key, val);
            }
        }
        Ok(())
    }

    /// An `int` method-argument, with a uniform type error matching the interp.
    pub(super) fn int_arg(&self, method: &str, v: &Value, span: Span) -> Result<i64, RuntimeError> {
        match self.int_val(*v) {
            Some(n) => Ok(n),
            None => Err(self.err(
                format!(
                    "{method}() expects an int argument, got {}",
                    self.type_name(*v)
                ),
                span,
            )),
        }
    }

    /// W6-3 — dispatch an **intrinsic** protocol method: one the checker grants a BUILT-IN with no
    /// user method behind it (`src/checker/proto.rs::satisfies_args_d`'s `grant_intrinsic` early-outs
    /// — `Add`/`Sub`/`Mul`/`Div`/`Mod`/`Neg` + `Comparable` on int/float/numeric-newtype, `Hashable`
    /// on int/str/bytes/bool/zero-field-struct, `Index`/`IndexSet`/`Slice` on list/map/str/bytes/
    /// bytearray). An erased `[T: Add]` body may write `a.add(b)`, so the VM must answer it.
    ///
    /// EVERY arm **delegates** to the exact primitive the operator form uses — `arith` for
    /// `add`..`mod`, `neg_value` for `neg`, `hash_value` (the Map/Set key hash) for `hash`,
    /// `compare` for `compare`, `get_index`/`set_index`/`get_slice` for the indexing trio — so
    /// `a.add(b)` ≡ `a + b` and `c.index(k)` ≡ `c[k]` by CONSTRUCTION: same value, same int/float
    /// coercion, same overflow / divide-by-zero / out-of-bounds fault text. Nothing is
    /// reimplemented here.
    ///
    /// Returns `Ok(None)` when `(method, argc)` is not an intrinsic pair, so the caller re-raises its
    /// original `has no method` error unchanged. Called ONLY from a method-resolution MISS, so a
    /// struct/enum/newtype that DEFINES `add`/`hash`/`index`/… always gets ITS method (it resolves
    /// first) and this costs nothing on any successful dispatch.
    ///
    /// `compare` on a NaN operand is the one arm that delegates somewhere ELSE than the operator: it
    /// answers the total order `sort()`/`.min()`/`.max()` use (via [`Vm::order_key`]) rather than
    /// faulting, so `compare`/`sort`/`min`/`max` share ONE order while `<`/`<=`/`>`/`>=` stay IEEE
    /// (false for every NaN comparison). See the arm below.
    ///
    /// **One documented limit on the "≡ the operator form" claim** (`docs/gaps.md`):
    /// * a numeric `newtype` that DEFINES `add`/`sub`/`mul`/`div`/`mod`/`compare` — being miss-only,
    ///   the method form correctly dispatches the USER method while `+`/`<` still auto-flow to the
    ///   underlying's native op, so for THAT receiver the two spellings differ (W6-3d). Never
    ///   shadowing a user method wins over the equivalence; the divergence pre-dates this function and
    ///   its real fix is a checker/grant decision.
    ///
    /// There is currently NO unpairable grant — `INTRINSIC_UNPAIRED` in `src/checker/proto.rs` is empty
    /// (W6-3b narrowed `Iterator` so a raw collection no longer claims `next`).
    pub(super) fn intrinsic_proto_method(
        &mut self,
        recv: Value,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Option<Value>, RuntimeError> {
        // `Add`/`Sub`/`Mul`/`Div`/`Mod` → the binary-arith primitive (which itself routes a
        // same-newtype pair through `newtype_arith`, so the numeric-newtype grant lands here too).
        let bin = match method {
            "add" => Some(Op::Add),
            "sub" => Some(Op::Sub),
            "mul" => Some(Op::Mul),
            "div" => Some(Op::Div),
            "mod" => Some(Op::Mod),
            _ => None,
        };
        if let Some(bin) = bin
            && args.len() == 1
        {
            self.push(recv);
            self.push(args[0]);
            self.arith(&bin, span)?;
            return Ok(Some(self.pop()));
        }
        match (method, args.len()) {
            ("neg", 0) => Ok(Some(self.neg_value(recv, span)?)),
            // `Hashable` → the SAME hash a Map/Set key gets, so `x.hash()` can never disagree with
            // `m[x]` / `s.has(x)` membership. Routes a zero-field struct through `struct_hash`'s
            // `fields.is_empty() && !methods.contains_key("hash")` guard, which is the runtime mirror
            // of the checker's zero-field `Hashable` grant. `recv` stays rooted across the (possibly
            // re-entrant, possibly allocating) hash + the result box.
            ("hash", 0) => {
                self.push(recv);
                let hv = self.hash_value(recv, span)?;
                let n = self.make_int(hv as i64);
                self.pop();
                Ok(Some(n))
            }
            // `Comparable` — needed for the numeric-NEWTYPE grant AND for a NaN operand on a scalar
            // (the pre-dispatch scalar arm only answers when `compare` returns `Some`). `compare`
            // unwraps a newtype to the UNDERLYING's native ordering, which is exactly what `<` uses
            // (`compare_op`'s same-newtype fast path).
            //
            // NaN is TOTAL here, and by the SAME order the rest of the language sorts by: route the
            // pair through [`Vm::order_key`] — the one ordering site behind `sort()` / `sort_by_key` /
            // `.min()` / `.max()` (`f64::total_cmp`, NaN deterministically at one end, numeric-newtype
            // layers unwrapped first). So there is exactly ONE total order shared by
            // `compare`/`sort`/`min`/`max`, and exactly ONE documented divergence left: that total
            // order (the method) vs IEEE (the operators — `ordered_bool` answers `false` for every NaN
            // comparison, IEEE-754/Python/Rust parity, arith.rs; untouched by design). Not two
            // orderings plus a fault, as W6-3c originally shipped.
            //
            // The NaN END is not fixed by spec: the signbit of `0.0/0.0` is target-dependent (negative
            // on x86 SSE2 ⇒ NaN ranks below `-inf` ⇒ sorts FIRST, `compare < 0`). The guarantee is
            // "same end `sort()` puts it", which is why this delegates instead of re-deriving.
            //
            // Reached only from the `None` (NaN) branch, never unconditionally: `order_key`'s struct
            // branch calls `struct_compare` (a USER `compare`), and intrinsic dispatch must stay
            // miss-only so a user method always wins. A `±0.0` pair therefore still answers via
            // `self.compare` (IEEE-Equal) exactly as before — only NaN comes through here.
            // `order_key`'s terminal `Err` is unreachable behind the `numeric_unwrapped` gate.
            ("compare", 1) => match self.compare(recv, args[0]) {
                Some(ord) => Ok(Some(Value::int(ord as i64))),
                None if self.numeric_unwrapped(recv) && self.numeric_unwrapped(args[0]) => {
                    let other = args[0];
                    Ok(Some(Value::int(self.order_key(recv, other, span)? as i64)))
                }
                // Genuinely incomparable TYPES (unreachable from well-typed source): leave the
                // caller's `has no method` error standing, exactly as before.
                None => Ok(None),
            },
            // `Eq` (M23) → the SAME structural equality `==` uses, so `x.eq(y)` can never disagree
            // with `x == y`. Serves the four scalar grants and the numeric-newtype grant (whose `==`
            // unwraps to the underlying, which is what the worker below does). Miss-only like the rest, so
            // a user type's own `eq` method is dispatched before this ever runs.
            // (`values_equal_guarded` is the operator's OWN worker — not the fault-swallowing
            // `values_equal` test wrapper — so a cyclic operand raises the same recoverable
            // depth fault `==` raises instead of silently answering "not equal".)
            ("eq", 1) => {
                let other = args[0];
                Ok(Some(Value::bool(
                    self.with_roots(&[recv, other], |vm| {
                        vm.values_equal_guarded(recv, other, 0, span)
                    })?,
                )))
            }
            // W7-8 `PathLike` → the RAW OS bytes of a path spelled as a `str`/`bytes`/`bytearray`.
            // `str` hands back its UTF-8 encoding (exactly what `str.encode()` yields), `bytes` IS the
            // answer (returned unchanged — no copy), and a `bytearray` is COPIED into a fresh immutable
            // `bytes` (the explicit `bytes(ba)` semantics: a later mutation of the buffer must not
            // change a path already handed over). Miss-only dispatch, so a user type with its own
            // `as_path` — including `path.Path` — never reaches here.
            ("as_path", 0) => {
                let Some(h) = recv.as_obj() else {
                    return Ok(None);
                };
                let raw: Vec<u8> = match self.heap.get(h) {
                    Obj::Str(s) => s.as_bytes().to_vec(),
                    Obj::Bytes(_) => return Ok(Some(recv)),
                    Obj::ByteArray(b) => b.clone(),
                    _ => return Ok(None),
                };
                let r = self.heap.alloc(Obj::Bytes(raw.into()));
                Ok(Some(Value::obj(r)))
            }
            ("index", 1) => {
                self.push(recv);
                self.push(args[0]);
                self.get_index(span)?;
                Ok(Some(self.pop()))
            }
            // `set_index` returns `nil` per the protocol; `Vm::set_index` pushes nothing.
            ("set_index", 2) => {
                self.push(recv);
                self.push(args[0]);
                self.push(args[1]);
                self.set_index(span)?;
                Ok(Some(Value::nil()))
            }
            // `Slice[R]`'s components are `int?` (`Option[int]`) VALUES, while `get_slice` reads the
            // raw `Nil`/`Int` form pushed for `c[a:b:c]` — so unwrap each `Some(n)`/`None`
            // (by the FIXED native variant ids, never a name compare, mirroring `stmt.rs`'s `opt`
            // closure inverted). A non-Option component is left as-is so `get_slice` raises its own
            // `expected int, found X`.
            ("slice", 3) => {
                self.push(recv);
                for a in args {
                    let c = self.unwrap_opt_int(*a);
                    self.push(c);
                }
                self.get_slice(span)?;
                Ok(Some(self.pop()))
            }
            _ => Ok(None),
        }
    }

    /// Is `v` numeric AFTER unwrapping any `newtype` layers? The predicate `Vm::ordered_bool` applies
    /// to decide "a `None` from `compare` means NaN → answer the total order, not incomparable types →
    /// leave the caller's `has no method` standing" — but applied to the
    /// values as `compare` sees them (it recurses through `Obj::NewType`, and `compare_op` unwraps a
    /// same-newtype pair before calling `ordered_bool`), so `newtype M = float` answers like `float`.
    fn numeric_unwrapped(&self, v: Value) -> bool {
        if let Some(h) = v.as_obj()
            && let Obj::NewType { inner, .. } = self.heap.get(h)
        {
            return self.numeric_unwrapped(*inner);
        }
        self.is_numeric(v)
    }

    /// `Option[int]` → the raw `Nil`/`Int` slice component `Vm::get_slice` expects. Gated on the
    /// fixed `VID_SOME`/`VID_NONE_VARIANT` ids (a user enum shadowing `Some`/`None` gets its own
    /// ids), mirroring `src/vm/stmt.rs`'s `opt` closure in reverse.
    fn unwrap_opt_int(&self, v: Value) -> Value {
        use crate::vm::op::{VID_NONE_VARIANT, VID_SOME};
        if let Some(h) = v.as_obj()
            && let Obj::Enum {
                variant_id,
                payload,
            } = self.heap.get(h)
        {
            if *variant_id == VID_SOME && payload.len() == 1 {
                return payload[0];
            }
            if *variant_id == VID_NONE_VARIANT {
                return Value::nil();
            }
        }
        v
    }

    pub(super) fn core_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // A str argument's owned text, with a uniform type error matching the interp.
        let str_arg = |vm: &Vm, i: usize| -> Result<String, RuntimeError> {
            match args[i].as_obj().map(|ah| vm.heap.get(ah)) {
                Some(Obj::Str(a)) => Ok(a.to_string()),
                _ => Err(vm.err(
                    format!(
                        "{method}() expects a str argument, got {}",
                        vm.type_name(args[i])
                    ),
                    span,
                )),
            }
        };
        match self.heap.get(h) {
            Obj::Str(s) => {
                let s = s.to_string();
                match method {
                    "len" => {
                        self.arity_err("len", args, 0, span)?;
                        Ok(Value::int(s.chars().count() as i64))
                    }
                    "upper" => {
                        self.arity_err("upper", args, 0, span)?;
                        Ok(self.alloc_str(s.to_uppercase()))
                    }
                    "lower" => {
                        self.arity_err("lower", args, 0, span)?;
                        Ok(self.alloc_str(s.to_lowercase()))
                    }
                    "trim" => {
                        self.arity_err("trim", args, 0, span)?;
                        Ok(self.alloc_str(s.trim().to_string()))
                    }
                    // `str` conforms to `Error`: `message()` returns the string itself.
                    "message" => {
                        self.arity_err("message", args, 0, span)?;
                        Ok(self.alloc_str(s.to_string()))
                    }
                    "split" => {
                        self.arity_err("split", args, 1, span)?;
                        let sep = str_arg(self, 0)?;
                        // std.string.split faults on an empty sep (Python model); the native
                        // method must too — Rust's `split("")` leaks empty edges. Recoverable.
                        if sep.is_empty() {
                            return Err(self.err("split: sep must not be empty".to_string(), span));
                        }
                        let parts: Vec<Value> = s
                            .split(sep.as_str())
                            .map(|p| self.alloc_str(p.to_string()))
                            .collect();
                        Ok(Value::obj(self.heap.alloc(Obj::List(parts))))
                    }
                    "chars" => {
                        self.arity_err("chars", args, 0, span)?;
                        let cs: Vec<Value> = s.chars().map(|c| self.alloc_char(c)).collect();
                        Ok(Value::obj(self.heap.alloc(Obj::List(cs))))
                    }
                    "starts_with" => {
                        self.arity_err("starts_with", args, 1, span)?;
                        Ok(Value::bool(s.starts_with(str_arg(self, 0)?.as_str())))
                    }
                    "contains" => {
                        self.arity_err("contains", args, 1, span)?;
                        Ok(Value::bool(s.contains(str_arg(self, 0)?.as_str())))
                    }
                    // `encode() -> bytes`: UTF-8 encode (str is UTF-8 internally; copy the bytes out
                    // into a new immutable `bytes`). Always succeeds — no fault path. UTF-8 only.
                    "encode" => {
                        self.arity_err("encode", args, 0, span)?;
                        let bytes = s.as_bytes().to_vec().into_boxed_slice();
                        Ok(Value::obj(self.heap.alloc(Obj::Bytes(bytes))))
                    }
                    "join" => {
                        self.arity_err("join", args, 1, span)?;
                        let Some(lh) = args[0].as_obj() else {
                            return Err(self.err(
                                format!(
                                    "join() expects a list of str, got {}",
                                    self.type_name(args[0])
                                ),
                                span,
                            ));
                        };
                        let Obj::List(items) = self.heap.get(lh) else {
                            return Err(self.err(
                                format!(
                                    "join() expects a list of str, got {}",
                                    self.type_name(args[0])
                                ),
                                span,
                            ));
                        };
                        let mut out = String::new();
                        for (i, item) in items.clone().iter().enumerate() {
                            let Some(ih) = item.as_obj() else {
                                return Err(self.err(
                                    format!(
                                        "join() expects a list of str, got an element of type {}",
                                        self.type_name(*item)
                                    ),
                                    span,
                                ));
                            };
                            let Obj::Str(part) = self.heap.get(ih) else {
                                return Err(self.err(
                                    format!(
                                        "join() expects a list of str, got an element of type {}",
                                        self.type_name(*item)
                                    ),
                                    span,
                                ));
                            };
                            if i > 0 {
                                out.push_str(&s);
                            }
                            out.push_str(part);
                        }
                        Ok(self.alloc_str(out))
                    }
                    // gap #1 (minimal subset): receiver methods forwarding to the `std.string` free
                    // fns. Pure native Rust, byte-identical to the std.string codepoint-loop oracle
                    // (see std/string.chz) and to the interp arms.
                    "ends_with" => {
                        self.arity_err("ends_with", args, 1, span)?;
                        Ok(Value::bool(s.ends_with(str_arg(self, 0)?.as_str())))
                    }
                    "replace" => {
                        self.arity_err("replace", args, 2, span)?;
                        let old = str_arg(self, 0)?;
                        let new = str_arg(self, 1)?;
                        // std.string returns `s` unchanged for an empty `old`.
                        if old.is_empty() {
                            Ok(self.alloc_str(s))
                        } else {
                            Ok(self.alloc_str(s.replace(old.as_str(), new.as_str())))
                        }
                    }
                    "repeat" => {
                        self.arity_err("repeat", args, 1, span)?;
                        let n = self.int_arg("repeat", &args[0], span)?;
                        // std.string: n <= 0 yields "".
                        if n <= 0 {
                            Ok(self.alloc_str(String::new()))
                        } else {
                            // Guard the allocation: `str::repeat` hard-panics on capacity overflow.
                            // Raise a recoverable fault instead (repo convention for overflow).
                            match s
                                .len()
                                .checked_mul(n as usize)
                                .filter(|&t| t <= isize::MAX as usize)
                            {
                                Some(total) => {
                                    // The byte-size guard passes huge-but-representable totals that
                                    // `str::repeat` would still abort on. Probe allocation
                                    // feasibility with `try_reserve_exact` (uninitialized capacity,
                                    // freed immediately) so an infeasible request is a recoverable
                                    // fault, then fall through to the optimized `str::repeat` — which
                                    // also short-circuits to "" for an empty receiver (`total == 0`)
                                    // instead of looping `n` times.
                                    let mut probe = String::new();
                                    if probe.try_reserve_exact(total).is_err() {
                                        return Err(self.err(
                                            "string repeat capacity overflow".to_string(),
                                            span,
                                        ));
                                    }
                                    drop(probe);
                                    Ok(self.alloc_str(s.repeat(n as usize)))
                                }
                                None => {
                                    Err(self
                                        .err("string repeat capacity overflow".to_string(), span))
                                }
                            }
                        }
                    }
                    "reverse" => {
                        self.arity_err("reverse", args, 0, span)?;
                        Ok(self.alloc_str(s.chars().rev().collect::<String>()))
                    }
                    "pad_left" => {
                        self.arity_err("pad_left", args, 2, span)?;
                        let width = self.int_arg("pad_left", &args[0], span)?;
                        let fill = str_arg(self, 1)?;
                        // An empty `fill` can never reach `width` — the old prepend loop spun
                        // forever. Fault EAGERLY (before the width check) so the diagnostic does
                        // not depend on whether padding was actually needed.
                        if fill.is_empty() {
                            return Err(
                                self.err("pad_left: fill must not be empty".to_string(), span)
                            );
                        }
                        // std.string: pad to `width` CODEPOINTS; never shrinks (a `width` at or below
                        // the current length — including `i64::MIN` — returns `s` unchanged). The
                        // early-out comes BEFORE the subtraction, so `need` can never overflow i64
                        // (a `width - len` on a negative `width` would panic in debug and wrap in
                        // release) nor wrap into a colossal `take` at the `as usize` cast.
                        let len = s.chars().count() as i64;
                        if width <= len {
                            return Ok(self.alloc_str(s));
                        }
                        let need = (width - len) as usize;
                        // Guard the allocation the same way `repeat` does — a huge `width` must be
                        // a recoverable fault, not an OOM/abort. Size the pad in BYTES EXACTLY:
                        // whole cycles of `fill` plus the byte length of its first `rem` chars (an
                        // over-estimate would spuriously fault a pad that actually fits).
                        let fill_cp = fill.chars().count(); // non-zero (empty `fill` rejected above)
                        let rem: usize =
                            fill.chars().take(need % fill_cp).map(char::len_utf8).sum();
                        match (need / fill_cp)
                            .checked_mul(fill.len())
                            .and_then(|pad| pad.checked_add(rem))
                            .and_then(|pad| pad.checked_add(s.len()))
                            .filter(|&t| t <= isize::MAX as usize)
                        {
                            Some(total) => {
                                let mut out = String::new();
                                if out.try_reserve_exact(total).is_err() {
                                    return Err(
                                        self.err("string pad capacity overflow".to_string(), span)
                                    );
                                }
                                // The fill is a repeating cycle TRUNCATED to fit, so the result is
                                // EXACTLY `width` codepoints (`"a".pad_left(4, "xy")` -> `"xyxa"`).
                                out.extend(fill.chars().cycle().take(need));
                                out.push_str(&s);
                                Ok(self.alloc_str(out))
                            }
                            None => Err(self.err("string pad capacity overflow".to_string(), span)),
                        }
                    }
                    "index_of" => {
                        self.arity_err("index_of", args, 1, span)?;
                        let sub = str_arg(self, 0)?;
                        // std.string: empty -> 0; otherwise the CODEPOINT index (not byte offset).
                        if sub.is_empty() {
                            Ok(Value::int(0))
                        } else {
                            match s.find(sub.as_str()) {
                                Some(byte) => Ok(Value::int(s[..byte].chars().count() as i64)),
                                None => Ok(Value::int(-1)),
                            }
                        }
                    }
                    "count" => {
                        self.arity_err("count", args, 1, span)?;
                        let sub = str_arg(self, 0)?;
                        // std.string / Python / Go: empty -> codepoint-len + 1; otherwise
                        // non-overlapping count.
                        if sub.is_empty() {
                            Ok(Value::int(s.chars().count() as i64 + 1))
                        } else {
                            Ok(Value::int(s.matches(sub.as_str()).count() as i64))
                        }
                    }
                    "strip_prefix" => {
                        self.arity_err("strip_prefix", args, 1, span)?;
                        let p = str_arg(self, 0)?;
                        let out = s.strip_prefix(p.as_str()).unwrap_or(&s).to_string();
                        Ok(self.alloc_str(out))
                    }
                    "strip_suffix" => {
                        self.arity_err("strip_suffix", args, 1, span)?;
                        let p = str_arg(self, 0)?;
                        let out = s.strip_suffix(p.as_str()).unwrap_or(&s).to_string();
                        Ok(self.alloc_str(out))
                    }
                    "split_lines" => {
                        self.arity_err("split_lines", args, 0, span)?;
                        let parts: Vec<Value> = s
                            .split('\n')
                            .map(|p| self.alloc_str(p.to_string()))
                            .collect();
                        Ok(Value::obj(self.heap.alloc(Obj::List(parts))))
                    }
                    // `strip` is a trim alias.
                    "strip" => {
                        self.arity_err("strip", args, 0, span)?;
                        Ok(self.alloc_str(s.trim().to_string()))
                    }
                    // gap #7: safe numeric parse — None on bad input (trims like int()/float()).
                    "to_int" => {
                        self.arity_err("to_int", args, 0, span)?;
                        match s.trim().parse::<i64>() {
                            Ok(n) => {
                                let nv = self.make_int(n);
                                Ok(self.alloc_enum("Option", "Some", vec![nv]))
                            }
                            Err(_) => Ok(self.alloc_enum("Option", "None", vec![])),
                        }
                    }
                    "to_float" => {
                        self.arity_err("to_float", args, 0, span)?;
                        match s.trim().parse::<f64>() {
                            Ok(f) => {
                                let fv = self.box_float(f);
                                Ok(self.alloc_enum("Option", "Some", vec![fv]))
                            }
                            Err(_) => Ok(self.alloc_enum("Option", "None", vec![])),
                        }
                    }
                    // Result-returning parse siblings of to_int/to_float — carry a human-readable
                    // Err(msg) instead of None (trims first, like int()/float()/to_int/to_float).
                    "parse_int" => {
                        self.arity_err("parse_int", args, 0, span)?;
                        match s.trim().parse::<i64>() {
                            Ok(n) => {
                                let nv = self.make_int(n);
                                Ok(self.alloc_enum("Result", "Ok", vec![nv]))
                            }
                            Err(_) => {
                                let msg =
                                    self.alloc_str(format!("cannot parse '{s}' as an integer"));
                                Ok(self.alloc_enum("Result", "Err", vec![msg]))
                            }
                        }
                    }
                    "parse_float" => {
                        self.arity_err("parse_float", args, 0, span)?;
                        match s.trim().parse::<f64>() {
                            Ok(f) => {
                                let fv = self.box_float(f);
                                Ok(self.alloc_enum("Result", "Ok", vec![fv]))
                            }
                            Err(_) => {
                                let msg = self.alloc_str(format!("cannot parse '{s}' as a float"));
                                Ok(self.alloc_enum("Result", "Err", vec![msg]))
                            }
                        }
                    }
                    _ => Err(self.err(format!("type str has no method '{method}'"), span)),
                }
            }
            Obj::List(items) => {
                match method {
                    "len" => {
                        self.arity_err("len", args, 0, span)?;
                        Ok(Value::int(items.len() as i64))
                    }
                    "push" => {
                        self.arity_err("push", args, 1, span)?;
                        let v = args[0];
                        let Obj::List(items) = self.heap.get_mut(h) else {
                            unreachable!()
                        };
                        items.push(v);
                        Ok(Value::nil())
                    }
                    "pop" => {
                        self.arity_err("pop", args, 0, span)?;
                        let Obj::List(items) = self.heap.get_mut(h) else {
                            unreachable!()
                        };
                        let popped = items.pop();
                        // M19 lever #2 — route through `alloc_enum` so the dense `variant_id` is stamped
                        // (replacing the two ad-hoc per-instance `Box<str>` builds).
                        Ok(match popped {
                            Some(v) => self.alloc_enum("Option", "Some", vec![v]),
                            None => self.alloc_enum("Option", "None", vec![]),
                        })
                    }
                    "reverse" => {
                        self.arity_err("reverse", args, 0, span)?;
                        let Obj::List(items) = self.heap.get_mut(h) else {
                            unreachable!()
                        };
                        items.reverse();
                        Ok(Value::nil())
                    }
                    "sort" => {
                        self.arity_err("sort", args, 0, span)?;
                        // In place, ascending. Checker guarantees a homogeneous orderable element type.
                        // A list of Comparable structs orders via each struct's `compare` (engine
                        // re-entry, so a merge sort that holds `&mut self`); primitives use the faster
                        // `value_order`. Str elements live on the heap, so `value_order` needs
                        // `&self.heap` — clone the elements out, sort (no alloc/closure → no GC for the
                        // primitive path), then write back.
                        let is_struct = matches!(items.first().and_then(|v| v.as_obj()), Some(hh) if matches!(self.heap.get(hh), Obj::Struct { .. }));
                        if is_struct {
                            // Struct compare re-enters the VM (may GC) → rooted, index-based sort.
                            return self.list_sort_structs(h, span);
                        }
                        let mut elems = items.clone();
                        elems.sort_by(|a, b| self.value_order(*a, *b));
                        let Obj::List(items) = self.heap.get_mut(h) else {
                            unreachable!()
                        };
                        *items = elems;
                        Ok(Value::nil())
                    }
                    // `elems` is a Rust-local clone and `target` came off the stack at dispatch, so
                    // both the receiver and the target are rooted across the (re-entrant) `eq`.
                    "contains" => {
                        self.arity_err("contains", args, 1, span)?;
                        let target = args[0];
                        let elems = items.clone();
                        let found = self
                            .with_roots(&[Value::obj(h), target], |vm| {
                                vm.seq_slot(&elems, target, span)
                            })?
                            .is_some();
                        Ok(Value::bool(found))
                    }
                    "index_of" => {
                        self.arity_err("index_of", args, 1, span)?;
                        let target = args[0];
                        let elems = items.clone();
                        let idx = self.with_roots(&[Value::obj(h), target], |vm| {
                            vm.seq_slot(&elems, target, span)
                        })?;
                        Ok(Value::int(idx.map(|i| i as i64).unwrap_or(-1)))
                    }
                    "concat" => {
                        self.arity_err("concat", args, 1, span)?;
                        let mut out = items.clone();
                        out.extend(self.expect_list_obj("concat", args[0], span)?);
                        // `out` is fully built and moved into the new Obj before any GC can run.
                        Ok(Value::obj(self.heap.alloc(Obj::List(out))))
                    }
                    "extend" => {
                        self.arity_err("extend", args, 1, span)?;
                        // Snapshot the other side first so `xs.extend(xs)` (self-extend) terminates.
                        let appended = self.expect_list_obj("extend", args[0], span)?;
                        let Obj::List(items) = self.heap.get_mut(h) else {
                            unreachable!()
                        };
                        items.extend(appended);
                        Ok(Value::nil())
                    }
                    "sum" => {
                        // A SCALAR NUMERIC NEWTYPE list arrives with one hidden argument: the `T(0)`
                        // seed the compiler minted from the checker's `NewtypeSumTable` (a user cannot
                        // spell `.sum(x)` — the harvested sig takes no parameters). Fold from it
                        // through `newtype_arith`, the same unwrap→native-op→rewrap path `Cents +
                        // Cents` takes, so overflow faults identically and the result is `T`. The seed
                        // alone is the answer for an EMPTY list.
                        if args.len() == 1 {
                            let seed = args[0];
                            let elems = items.clone();
                            let mut acc = seed;
                            for &v in &elems {
                                // BELT-AND-BRACES, not load-bearing today. `acc` is a heap value held
                                // only in a Rust local across `newtype_arith`'s `heap.alloc`, which is
                                // the shape `with_roots` exists for — but no collection can land here:
                                // the only two `collect()` sites are `run_until`'s instruction boundary
                                // (`exec.rs`) and `sample_mem_cap` (per task dispatch, `sched.rs`);
                                // `Heap::alloc` merely bumps counters. Unlike the `values_equal_guarded`
                                // / `hash_value` `with_roots` sites nearby, `newtype_arith` is pure
                                // native and cannot re-enter the VM — the admitted set is exactly the
                                // INTRINSIC-`Add` set, so there is no user `add` hook to dispatch. Kept
                                // so the fold stays correct if a collect trigger ever moves.
                                acc = self.with_roots(&[Value::obj(h), acc, v], |vm| {
                                    match (acc.as_obj(), v.as_obj()) {
                                        (Some(ha), Some(hb)) if vm.same_newtype_keys(ha, hb) => {
                                            vm.newtype_arith(&Op::Add, ha, hb, "Add", span)
                                        }
                                        _ => Err(vm.err(
                                            format!(
                                                "sum() expects a numeric list, got an element of type {}",
                                                vm.type_name(v)
                                            ),
                                            span,
                                        )),
                                    }
                                })?;
                            }
                            return Ok(acc);
                        }
                        self.arity_err("sum", args, 0, span)?;
                        // Clone out so `make_int`/`box_float` (which mutate the heap) don't collide with
                        // the `items` heap borrow.
                        let elems = items.clone();
                        let any_float = elems.iter().any(|v| v.is_float());
                        if any_float {
                            let mut acc = 0.0_f64;
                            for &v in &elems {
                                if v.is_float() {
                                    acc += self.float_of(v);
                                } else if let Some(n) = self.int_val(v) {
                                    acc += n as f64;
                                } else {
                                    return Err(self.err(format!("sum() expects a numeric list, got an element of type {}", self.type_name(v)), span));
                                }
                            }
                            Ok(self.box_float(acc))
                        } else {
                            let mut acc = 0_i64;
                            for &v in &elems {
                                if let Some(n) = self.int_val(v) {
                                    acc = acc.checked_add(n).ok_or_else(|| {
                                        self.err("integer overflow in Add".to_string(), span)
                                    })?;
                                } else {
                                    return Err(self.err(format!("sum() expects a numeric list, got an element of type {}", self.type_name(v)), span));
                                }
                            }
                            Ok(self.make_int(acc))
                        }
                    }
                    "first" | "last" => {
                        self.arity_err(method, args, 0, span)?;
                        let Obj::List(items) = self.heap.get(h) else {
                            unreachable!()
                        };
                        let picked = if method == "first" {
                            items.first().copied()
                        } else {
                            items.last().copied()
                        };
                        Ok(match picked {
                            Some(v) => self.alloc_enum("Option", "Some", vec![v]),
                            None => self.alloc_enum("Option", "None", vec![]),
                        })
                    }
                    "reversed" => {
                        // A NEW list (clone then reverse) — never mutate the receiver (that is `reverse`).
                        self.arity_err("reversed", args, 0, span)?;
                        let Obj::List(items) = self.heap.get(h) else {
                            unreachable!()
                        };
                        let mut out = items.clone();
                        out.reverse();
                        Ok(Value::obj(self.heap.alloc(Obj::List(out))))
                    }
                    "insert" => {
                        // Python-clamp: i>len appends, negatives are len-relative and clamp to 0. Never faults.
                        self.arity_err("insert", args, 2, span)?;
                        let Some(i) = self.int_val(args[0]) else {
                            unreachable!()
                        };
                        let x = args[1];
                        let Obj::List(items) = self.heap.get_mut(h) else {
                            unreachable!()
                        };
                        let len = items.len() as i64;
                        let idx = if i < 0 { (i + len).max(0) } else { i.min(len) } as usize;
                        items.insert(idx, x);
                        Ok(Value::nil())
                    }
                    "remove_at" => {
                        // Returns the removed element; Python-relative negatives; true OOB faults.
                        self.arity_err("remove_at", args, 1, span)?;
                        let Some(i) = self.int_val(args[0]) else {
                            unreachable!()
                        };
                        let Obj::List(items) = self.heap.get_mut(h) else {
                            unreachable!()
                        };
                        let len = items.len();
                        match crate::slice::norm_index(i, len) {
                            Some(idx) => Ok(items.remove(idx)),
                            None => {
                                Err(self.err(format!("index {i} out of bounds (len {len})"), span))
                            }
                        }
                    }
                    // min/max may re-enter the VM (a Comparable-struct `compare` → GC), which invalidates
                    // the `items` borrow — route to a rooted, index-based helper (mirrors `sort`'s struct path).
                    "min" | "max" => {
                        self.arity_err(method, args, 0, span)?;
                        self.list_reduce_extreme(h, method == "max", span)
                    }
                    // unique/dedup: NEW list, never mutate the receiver. Equality is the same
                    // `elem_equal` `contains` uses (works on floats) — which since M23 can dispatch a
                    // user `eq` and so re-enter the VM. Root the snapshotted ELEMENTS, not just the
                    // receiver: an `eq` that clears the receiver mid-walk orphans every element
                    // `items`/`out` still hold (`out` ⊆ `items`, so `items` covers both).
                    "unique" | "dedup" => {
                        self.arity_err(method, args, 0, span)?;
                        let Obj::List(items) = self.heap.get(h) else {
                            unreachable!()
                        };
                        let items = items.clone();
                        let out = self.with_elem_roots(&[Value::obj(h)], &items, |vm| {
                            let mut out: Vec<Value> = Vec::new();
                            if method == "dedup" {
                                // Collapse only CONSECUTIVE runs (Rust `Vec::dedup`): keep an element
                                // iff it differs from the previously-kept one.
                                for &v in &items {
                                    let keep = match out.last() {
                                        Some(&p) => !vm.elem_equal(p, v, 0, span)?,
                                        None => true,
                                    };
                                    if keep {
                                        out.push(v);
                                    }
                                }
                            } else if items.iter().all(|&v| vm.is_flat_hash_key(v)) {
                                // W8-34: every element is a flat scalar key, so dedupe through a hash
                                // index in one pass instead of a linear scan per element. The gate is
                                // all-or-nothing over the WHOLE list, never per-element: `bytes ==
                                // bytearray` is content-equal, so mixing would let an indexed `bytes`
                                // and a scanned `bytearray` of equal content both survive. A bucket hit
                                // is still confirmed by `elem_equal` — the hash only picks candidates,
                                // the compare remains the verdict (DEC-014).
                                let mut seen = SetData::default();
                                for &v in &items {
                                    let hv = vm.scalar_hash(v);
                                    let mut dup = false;
                                    for &p in seen.candidates(hv) {
                                        if vm.elem_equal(seen.entries[p].1, v, 0, span)? {
                                            dup = true;
                                            break;
                                        }
                                    }
                                    if !dup {
                                        seen.push(hv, v);
                                    }
                                }
                                out = seen.entries.iter().map(|&(_, v)| v).collect();
                            } else {
                                // Remove ALL duplicates, first-occurrence order (Python `dict.fromkeys`).
                                // Fallback for any non-flat element (container, Struct/Enum/NewType,
                                // ByteArray) — same O(n^2) scan as before the hash-index fast path above.
                                for &v in &items {
                                    if vm.seq_slot(&out, v, span)?.is_none() {
                                        out.push(v);
                                    }
                                }
                            }
                            Ok(out)
                        })?;
                        Ok(Value::obj(self.heap.alloc(Obj::List(out))))
                    }
                    // chunk/windows build a `List[List[T]]`. Each inner-list alloc may GC, so the outer
                    // list is ROOTED on the operand stack and every inner handle is pushed into it
                    // IMMEDIATELY after allocation (no intervening alloc), keeping prior inners reachable.
                    "chunk" | "windows" => {
                        self.arity_err(method, args, 1, span)?;
                        let Some(n) = self.int_val(args[0]) else {
                            unreachable!()
                        };
                        if n <= 0 {
                            let what = if method == "chunk" { "chunk" } else { "window" };
                            return Err(
                                self.err(format!("{what} size must be positive, got {n}"), span)
                            );
                        }
                        let n = n as usize;
                        let Obj::List(items) = self.heap.get(h) else {
                            unreachable!()
                        };
                        let items = items.clone();
                        let len = items.len();
                        let outer = self.heap.alloc(Obj::List(Vec::new()));
                        self.push(Value::obj(outer)); // ROOT the outer list across inner allocs
                        if method == "chunk" {
                            let mut start = 0;
                            while start < len {
                                let end = (start + n).min(len);
                                let inner = self.heap.alloc(Obj::List(items[start..end].to_vec()));
                                if let Obj::List(o) = self.heap.get_mut(outer) {
                                    o.push(Value::obj(inner));
                                }
                                start = end;
                            }
                        } else if n <= len {
                            // windows: `n > len` → no windows → empty outer list (loop skipped).
                            for start in 0..=len - n {
                                let inner =
                                    self.heap.alloc(Obj::List(items[start..start + n].to_vec()));
                                if let Obj::List(o) = self.heap.get_mut(outer) {
                                    o.push(Value::obj(inner));
                                }
                            }
                        }
                        self.pop(); // unroot outer
                        Ok(Value::obj(outer))
                    }
                    _ => Err(self.err(format!("type list has no method '{method}'"), span)),
                }
            }
            Obj::Map(m) => match method {
                "len" => {
                    self.arity_err("len", args, 0, span)?;
                    Ok(Value::int(m.entries.len() as i64))
                }
                "has" => {
                    self.arity_err("has", args, 1, span)?;
                    let key = args[0];
                    let hk = self.hash_key_rooted(key, &[Value::obj(h), key], span)?;
                    let found = self.map_probe(h, hk, key, span)?.is_some();
                    Ok(Value::bool(found))
                }
                "get" => {
                    self.arity_err("get", args, 1, span)?;
                    let key = args[0];
                    let hk = self.hash_key_rooted(key, &[Value::obj(h), key], span)?;
                    let found = self.map_probe(h, hk, key, span)?.map(|p| {
                        let Obj::Map(m) = self.heap.get(h) else {
                            unreachable!()
                        };
                        m.entries[p].2
                    });
                    match found {
                        Some(v) => Ok(self.alloc_enum("Option", "Some", vec![v])),
                        None => Ok(self.alloc_enum("Option", "None", vec![])),
                    }
                }
                "keys" => {
                    self.arity_err("keys", args, 0, span)?;
                    let keys: Vec<Value> = m.entries.iter().map(|(_, k, _)| *k).collect();
                    Ok(Value::obj(self.heap.alloc(Obj::List(keys))))
                }
                "values" => {
                    self.arity_err("values", args, 0, span)?;
                    let vals: Vec<Value> = m.entries.iter().map(|(_, _, v)| *v).collect();
                    Ok(Value::obj(self.heap.alloc(Obj::List(vals))))
                }
                "remove" => {
                    self.arity_err("remove", args, 1, span)?;
                    let key = args[0];
                    let hk = self.hash_key_rooted(key, &[Value::obj(h), key], span)?;
                    let pos = self.map_probe(h, hk, key, span)?;
                    match pos {
                        Some(i) => {
                            let Obj::Map(m) = self.heap.get_mut(h) else {
                                unreachable!()
                            };
                            let (_, _, v) = m.remove_at(i);
                            Ok(self.alloc_enum("Option", "Some", vec![v]))
                        }
                        None => Ok(self.alloc_enum("Option", "None", vec![])),
                    }
                }
                "merge" | "update" => {
                    self.arity_err(method, args, 1, span)?;
                    // Snapshot the incoming entries (with their cached hashes — engine-wide
                    // consistent, so reuse is sound) first; handles `m.merge(m)`/`m.update(m)`.
                    let incoming = match args[0].as_obj().map(|oh| self.heap.get(oh)) {
                        Some(Obj::Map(o)) => o.entries.clone(),
                        _ => {
                            return Err(self.err(
                                format!(
                                    "{method}() expects a map argument, got {}",
                                    self.type_name(args[0])
                                ),
                                span,
                            ));
                        }
                    };
                    if method == "merge" {
                        // Build the result fresh, snapshotting EVERY struct/enum/newtype key (Go
                        // value-key model) so the new map aliases neither the receiver's nor the
                        // argument's stored keys — `m.clone()` would carry the receiver's key handles
                        // by reference. The receiver's own keys are already unique, so they need no
                        // dedup; the argument's entries upsert (last-wins). `snapshot_key` is pure
                        // alloc (no GC), so the local `out`'s contents stay valid across it.
                        let mine = match self.heap.get(h) {
                            Obj::Map(m) => m.entries.clone(),
                            _ => unreachable!(),
                        };
                        // Root BOTH source maps: `out` is a Rust local and its snapshot keys are
                        // reachable only from it, so the receiver + argument (whose entries every
                        // value in `out` came from) must stay alive across a re-entrant user `eq`.
                        let out = self.with_roots(&[Value::obj(h), args[0]], |vm| {
                            let mut out = MapData::default();
                            for (hk, key, val) in mine {
                                // A snapshot key is a FRESH object reachable only from the local
                                // `out`, so root it on the operand stack too (`with_roots` truncates
                                // back past these on the way out).
                                let key = vm.snapshot_key(key);
                                vm.push(key);
                                out.push(hk, key, val);
                            }
                            for (hk, key, val) in incoming {
                                let pos =
                                    vm.map_slot(&out.entries, out.candidates(hk), key, span)?;
                                match pos {
                                    Some(i) => out.entries[i].2 = val,
                                    None => {
                                        let key = vm.snapshot_key(key);
                                        vm.push(key);
                                        out.push(hk, key, val);
                                    }
                                }
                            }
                            Ok(out)
                        })?;
                        Ok(Value::obj(self.heap.alloc(Obj::Map(out))))
                    } else {
                        // Root both maps for the WHOLE loop: the not-yet-upserted tail of `incoming`
                        // is a Rust local reachable only through the argument map, which is unrooted
                        // when it is an inline temporary (`m.update(make_map())`) — and each upsert
                        // now probes with a possibly re-entrant user `eq`.
                        self.with_roots(&[Value::obj(h), args[0]], |vm| {
                            for (hk, key, val) in incoming {
                                vm.map_upsert_in_heap(h, hk, key, val, span)?;
                            }
                            Ok(())
                        })?;
                        Ok(Value::nil())
                    }
                }
                _ => Err(self.err(format!("type map has no method '{method}'"), span)),
            },
            Obj::Set(s) => match method {
                "len" => {
                    self.arity_err("len", args, 0, span)?;
                    Ok(Value::int(s.entries.len() as i64))
                }
                "has" => {
                    self.arity_err("has", args, 1, span)?;
                    let x = args[0];
                    let hx = self.hash_key_rooted(x, &[Value::obj(h), x], span)?;
                    Ok(Value::bool(self.set_probe(h, hx, x, span)?.is_some()))
                }
                "add" => {
                    self.arity_err("add", args, 1, span)?;
                    let x = args[0];
                    let hx = self.hash_key_rooted(x, &[Value::obj(h), x], span)?;
                    let present = self.set_probe(h, hx, x, span)?.is_some();
                    if !present {
                        // Snapshot a struct/enum/newtype element on insert (Go value-key model) so a
                        // later mutation of the caller's live value can't corrupt the set. Pure alloc
                        // (no GC), so no rooting needed before the immediately-following push.
                        let x = self.snapshot_key(x);
                        let Obj::Set(s) = self.heap.get_mut(h) else {
                            unreachable!()
                        };
                        s.push(hx, x);
                    }
                    Ok(Value::nil())
                }
                "remove" => {
                    self.arity_err("remove", args, 1, span)?;
                    let x = args[0];
                    let hx = self.hash_key_rooted(x, &[Value::obj(h), x], span)?;
                    let pos = self.set_probe(h, hx, x, span)?;
                    match pos {
                        Some(i) => {
                            let Obj::Set(s) = self.heap.get_mut(h) else {
                                unreachable!()
                            };
                            s.remove_at(i);
                            Ok(Value::bool(true))
                        }
                        None => Ok(Value::bool(false)),
                    }
                }
                "union" | "intersection" | "difference" => {
                    self.arity_err(method, args, 1, span)?;
                    // Both operands already carry per-element cached hashes, so set algebra needs no
                    // re-hashing — purely build a fresh hash set, deduping and membership-testing via
                    // the cached hashes confirmed by `elem_equal` (which since M23 may dispatch a user
                    // `eq` and re-enter the VM; `mine`/`other`/`out` are Rust locals holding elements
                    // reachable only through the two source sets, so both are rooted).
                    let mine = match self.heap.get(h) {
                        Obj::Set(s) => s.entries.clone(),
                        _ => unreachable!(),
                    };
                    let other = self.set_arg(args[0], method, span)?;
                    let add = |vm: &mut Vm,
                               set: &mut SetData,
                               he: u64,
                               e: Value|
                     -> Result<(), RuntimeError> {
                        if vm
                            .set_slot(&set.entries, set.candidates(he), e, span)?
                            .is_none()
                        {
                            set.push(he, e);
                        }
                        Ok(())
                    };
                    let out = self.with_roots(&[Value::obj(h), args[0]], |vm| {
                        let mut out = SetData::default();
                        match method {
                            "union" => {
                                for (he, e) in mine.iter().chain(other.entries.iter()) {
                                    add(vm, &mut out, *he, *e)?;
                                }
                            }
                            // intersection keeps mine's elements present in other; difference drops them.
                            m => {
                                let keep_when_present = m == "intersection";
                                for (he, e) in &mine {
                                    let in_other = vm
                                        .set_slot(&other.entries, other.candidates(*he), *e, span)?
                                        .is_some();
                                    if in_other == keep_when_present {
                                        add(vm, &mut out, *he, *e)?;
                                    }
                                }
                            }
                        }
                        Ok(out)
                    })?;
                    Ok(Value::obj(self.heap.alloc(Obj::Set(out))))
                }
                _ => Err(self.err(format!("type set has no method '{method}'"), span)),
            },
            _ => unreachable!("core_method dispatched a non-str/list/map/set receiver"),
        }
    }

    /// Built-in methods on a `bytearray` receiver (`h` is the heap handle): `len`, `push(int 0..=255)`,
    /// `pop() -> Option[int]`, `extend(bytes|bytearray|List[int])`. Mirrors the checker's file-backed
    /// `native struct bytearray` method table in `std/prelude.chz` (the retired `bytearray_method_sig`) —
    /// keep them in lockstep.
    /// Mutators write IN PLACE through the heap slot (`get_mut`), exactly like the `list` methods, so a
    /// second binding to the same `bytearray` observes the change.
    pub(super) fn bytearray_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "len" => {
                self.arity_err("len", args, 0, span)?;
                let Obj::ByteArray(b) = self.heap.get(h) else {
                    unreachable!()
                };
                Ok(Value::int(b.len() as i64))
            }
            "push" => {
                self.arity_err("push", args, 1, span)?;
                let byte = match self.int_val(args[0]) {
                    Some(n) if (0..=255).contains(&n) => n as u8,
                    Some(n) => {
                        return Err(self.err(
                            format!("byte value {n} out of range (must be 0..=255)"),
                            span,
                        ));
                    }
                    None => {
                        return Err(self.err(
                            format!("push() expects an int, got {}", self.type_name(args[0])),
                            span,
                        ));
                    }
                };
                let Obj::ByteArray(b) = self.heap.get_mut(h) else {
                    unreachable!()
                };
                b.push(byte);
                Ok(Value::nil())
            }
            "pop" => {
                self.arity_err("pop", args, 0, span)?;
                let Obj::ByteArray(b) = self.heap.get_mut(h) else {
                    unreachable!()
                };
                let popped = b.pop();
                Ok(match popped {
                    Some(x) => self.alloc_enum("Option", "Some", vec![Value::int(x as i64)]),
                    None => self.alloc_enum("Option", "None", vec![]),
                })
            }
            "extend" => {
                self.arity_err("extend", args, 1, span)?;
                // Snapshot the other side first (so `ba.extend(ba)` terminates) — also validates
                // ints 0..=255 / element types up front, mirroring the constructor.
                let appended = self.collect_bytes_arg("extend", args[0], span)?;
                let Obj::ByteArray(b) = self.heap.get_mut(h) else {
                    unreachable!()
                };
                b.extend_from_slice(&appended);
                Ok(Value::nil())
            }
            // `decode() -> str`: UTF-8 decode the current buffer. Invalid UTF-8 is a RECOVERABLE
            // fault (catchable by `recover:`), never a panic — mirrors the bytes path + the interp.
            "decode" => {
                self.arity_err("decode", args, 0, span)?;
                let Obj::ByteArray(b) = self.heap.get(h) else {
                    unreachable!()
                };
                let bytes = b.clone();
                self.decode_utf8(&bytes, span)
            }
            _ => Err(self.err(format!("type bytearray has no method '{method}'"), span)),
        }
    }

    /// `bytes` methods (immutable byte sequence): only `decode() -> str` (UTF-8). Mirrors the checker's
    /// file-backed `native struct bytes` method table in `std/prelude.chz` (the retired `bytes_method_sig`)
    /// — keep them in lockstep.
    pub(super) fn bytes_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "decode" => {
                self.arity_err("decode", args, 0, span)?;
                let Obj::Bytes(b) = self.heap.get(h) else {
                    unreachable!()
                };
                let bytes = b.clone();
                self.decode_utf8(&bytes, span)
            }
            // W7-8 `decode_lossy() -> str`: UTF-8 decode with each maximal invalid subsequence
            // replaced by U+FFFD (Python's `b.decode(errors="replace")`, Rust's
            // `String::from_utf8_lossy`). NEVER faults — it is the DISPLAY twin of `decode()`, and it
            // is what `path.Path.str()` (a `Stringable`, which cannot fail) is built on.
            "decode_lossy" => {
                self.arity_err("decode_lossy", args, 0, span)?;
                let Obj::Bytes(b) = self.heap.get(h) else {
                    unreachable!()
                };
                let s = String::from_utf8_lossy(b).into_owned();
                Ok(self.alloc_str(s))
            }
            "len" => {
                self.arity_err("len", args, 0, span)?;
                let Obj::Bytes(b) = self.heap.get(h) else {
                    unreachable!()
                };
                Ok(Value::int(b.len() as i64))
            }
            _ => Err(self.err(format!("type bytes has no method '{method}'"), span)),
        }
    }

    /// UTF-8 decode a byte slice into a new heap `str`. Invalid UTF-8 maps to a RECOVERABLE
    /// RuntimeError (catchable by `recover:`), not a panic.
    pub(super) fn decode_utf8(&mut self, bytes: &[u8], span: Span) -> Result<Value, RuntimeError> {
        match std::str::from_utf8(bytes) {
            Ok(s) => Ok(self.alloc_str(s.to_string())),
            Err(_) => Err(self.err("invalid UTF-8 in decode()".to_string(), span)),
        }
    }

    /// Read a set argument (for set algebra), erroring if it isn't a set. Returns a clone of its
    /// [`SetData`] (entries + index) so membership tests reuse the cached hashes.
    pub(super) fn set_arg(
        &self,
        v: Value,
        method: &str,
        span: Span,
    ) -> Result<SetData, RuntimeError> {
        match v.as_obj().map(|h| self.heap.get(h)) {
            Some(Obj::Set(s)) => Ok(s.clone()),
            _ => Err(self.err(
                format!(
                    "{method}() expects a set argument, got {}",
                    self.type_name(v)
                ),
                span,
            )),
        }
    }

    /// Allocate a heap string and return its handle as a `Value`.
    pub(super) fn alloc_str(&mut self, s: String) -> Value {
        Value::obj(self.heap.alloc(Obj::Str(s.into())))
    }

    /// M19 Phase 3 — the 1-char `str` value for `c`, in a single allocation. `c.to_string()` +
    /// `into_boxed_str` is two allocs (a `String`, then a shrink-to-fit realloc); encoding straight
    /// into a stack buffer and boxing the `&str` is one. Used by string indexing/iteration/`chr`.
    pub(super) fn alloc_char(&mut self, c: char) -> Value {
        let mut buf = [0u8; 4];
        Value::obj(
            self.heap
                .alloc(Obj::Str((&*c.encode_utf8(&mut buf)).into())),
        )
    }

    /// Return from the current frame. `propagated` true ⇒ the value came from `?` (no observable
    /// difference here; the caller treats it as the function's result, exactly like the interp).
    ///
    /// Deferred calls (`defer`) run LIFO first, while the frame is still live so the GC keeps their
    /// values — and the return value — rooted. A fault in a deferred call supersedes the frame's
    /// result (Go: a panic in a defer wins): it returns `Err` and the frame is still torn down.
    pub(super) fn do_return(&mut self, _propagated: bool) -> Result<(), RuntimeError> {
        // M-C implicit nurseries: if this frame opened one, JOIN it here (run its spawned tasks to
        // completion) BEFORE the frame unwinds — `return`/`?`/fall-through is the join barrier. This
        // runs while the frame is still current and the return value (if any) still sits on the
        // operand stack; `join_nursery` swaps the whole `FiberCtx`, never the operand value, so the
        // value survives. Any *inner* `parallel:` this return/`?` escaped sits ABOVE the implicit
        // nursery and is cancelled-and-reported first (existing escape semantics). A task that faults
        // during the join propagates as this function's error (the frame is intact, so the normal
        // unwind machinery runs its defers). NB: an uncaught *body* fault never reaches here — it
        // unwinds via the handler path, which cancels (not joins) the implicit nursery.
        let frame_top = self.frames.last().unwrap();
        let nursery_floor = frame_top.nursery_len;
        if frame_top.has_implicit_nursery {
            self.drain_escaped_nursery(nursery_floor + 1); // cancel inner escaped `parallel:` levels
            if self.nurseries.len() > nursery_floor {
                self.join_nursery()?; // join the implicit nursery (runs its tasks)
            }
        }
        // Drain with the return value still on top of the stack (rooted) and the frame still on
        // `self.frames` (so `collect` roots the pending records). Defers run AFTER the implicit-nursery
        // join above (tasks complete, then cleanup).
        let defer_err = self.drain_top_frame_deferred();
        let ret = self.pop();
        let frame = self.frames.pop().unwrap();
        if frame.counted {
            self.call_depth -= 1;
        }
        self.stack.truncate(frame.base);
        self.cur_base = self.frames.last().map(|f| f.base).unwrap_or(0);
        // Reclaim any `parallel:` nursery this frame opened but whose `JoinNursery` was skipped by a
        // `?`/return escape (no-op on the normal fall-through path — `JoinNursery` already popped it,
        // so `nurseries.len() == frame.nursery_len`; also a no-op when the implicit nursery above was
        // just joined). Mirrors the `recover:` catch path and the interp's unconditional
        // `exec_parallel` pop; a nested frame keeps the parent's nursery (it captured the parent depth
        // at entry). TASK B: route through `drain_escaped_nursery` so the unstarted tasks are
        // cancelled-and-reported (not silently dropped). NB: within-frame `break`/`continue` out of a
        // `parallel:` no longer rely on this — the compiler emits a `ReclaimNursery` before their
        // loop-exit `Jump` (see `compile_parallel`/`emit_loop_body_drain`), reclaiming block-scoped.
        self.drain_escaped_nursery(frame.nursery_len);
        // Drop any `recover:` handlers installed in the frame we just left (e.g. a `?` early-return
        // out of a recover block) — they must not survive to catch a later, unrelated fault.
        while self
            .handlers
            .last()
            .is_some_and(|h| h.frame_len > self.frames.len())
        {
            self.handlers.pop();
        }
        if let Some(e) = defer_err {
            return Err(e);
        }
        self.push(ret);
        Ok(())
    }

    /// `defer f(args)` / `defer recv.m(args)` — pop the callee/receiver + `argc` args off the stack
    /// and record a deferred call on the current frame (drained LIFO at frame exit). The values were
    /// evaluated now (Go semantics); the call runs at exit.
    pub(super) fn do_defer(&mut self, method: Option<String>, argc: usize, span: Span) {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let head = self.pop();
        let d = match method {
            Some(name) => Deferred::Method {
                recv: head,
                name,
                args,
                span,
            },
            None => Deferred::Call {
                callee: head,
                args,
                span,
            },
        };
        self.frames.last_mut().unwrap().deferred.push(d);
    }

    /// Run one deferred call to completion (the result is discarded). `Call` rides `invoke_value`;
    /// `Method` re-uses the normal method dispatch by pushing the receiver + args and popping the
    /// discarded result. The pop→push window has no instruction boundary, so the moved-out values
    /// can't be collected before they're re-rooted.
    pub(super) fn run_one_deferred(&mut self, d: Deferred) -> Result<(), RuntimeError> {
        // A defer is the cleanup a cancel exists to run, so NO cancellation checkpoint fires inside
        // it: `deferring > 0` ⇒ `cancel_requested()` is false (exec.rs). It must be raised BEFORE
        // `guarded` (whose own checkpoint would otherwise eat this very call) and lowered on every
        // exit path, including a fault thrown by the deferred body itself.
        // Panic-safe, same reasoning as `guarded`'s `native_reentry` (exec.rs): `run_one_deferred_inner`
        // calls `guarded`, which CATCHES a re-entered FFI callback's Rust panic and `resume_unwind`s it,
        // so a plain `-= 1` after the call would be skipped on that unwind and leak `deferring` at +1 for
        // the VM's lifetime. Since W7-3 that leak is worse than "the task stops hitting checkpoints": it
        // would also permanently disable the cancel bypass, letting an ordinary outer `recover:` defeat a
        // nursery cancel. A `Drop` guard can't be used (it would alias `self` across the call).
        self.deferring += 1;
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.run_one_deferred_inner(d)
        }));
        self.deferring -= 1;
        match r {
            Ok(v) => v,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn run_one_deferred_inner(&mut self, d: Deferred) -> Result<(), RuntimeError> {
        // Guarded: a deferred call runs during frame teardown (the LIFO drain loop is Rust-stack
        // state), so a blocking `recv` inside it cannot park — it faults `deadlock` (B1).
        self.guarded(|vm| match d {
            Deferred::Call { callee, args, span } => {
                vm.invoke_value(callee, args, span)?;
                Ok(())
            }
            Deferred::Method {
                recv,
                name,
                args,
                span,
            } => {
                let argc = args.len();
                vm.push(recv);
                for a in args {
                    vm.push(a);
                }
                vm.do_method_call(&name, argc, NO_IC, span)?;
                vm.pop(); // discard the deferred call's result
                Ok(())
            }
        })
    }

    // ----- concurrency C4: sequential, run-to-completion executor -----
}
