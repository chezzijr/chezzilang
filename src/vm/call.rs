// vm::call — split out of vm/mod.rs. `super::*` == the `vm` module.
// Calls: value/native/method/static dispatch, builtins, defer/return.

use super::*;
use crate::base::builtin::Builtin;

/// Why `coerce_json` stopped: a decode mismatch (the `Err` of the decode's `Result`), or a fault
/// raised by a field default it ran, which propagates as a runtime fault (TICKET-198).
pub(super) enum DecodeFail {
    Msg(String),
    Fault(RuntimeError),
}

impl From<String> for DecodeFail {
    fn from(msg: String) -> Self {
        DecodeFail::Msg(msg)
    }
}

/// CPython's `str.isspace()` is true for 29 codepoints; Rust's `char::is_whitespace` covers 25. The
/// four extras are U+001C..U+001F (the C0 "information separator" controls), measured 2026-09-08.
pub(super) fn py_blank(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

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
            };
            if let Some((proto, home, clo)) = kind {
                // Arity check BEFORE disturbing the stack — identical messages to `invoke_value`, and
                // the error path leaves `[callee, args…]` intact for the trace / `recover:`.
                let arity = self.program.protos[proto].arity;
                // A closure proto with defaults (a nested `fn`) may be entered short too; a lambda
                // cannot declare a default, so it keeps the exact-arity check and its message.
                if clo.is_none() || self.program.protos[proto].min_arity < arity {
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

    /// The arity check of a call to a fn value (`closure`: a closure, whose lambda form cannot
    /// declare a default and keeps the exact count and its message).
    fn callee_arity(
        &self,
        proto: ProtoId,
        closure: bool,
        argc: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let p = &self.program.protos[proto];
        if !closure || p.min_arity < p.arity {
            return self.check_proto_arity(proto, argc, span);
        }
        if argc != p.arity {
            return Err(self.err(
                format!("closure expects {} argument(s), got {argc}", p.arity),
                span,
            ));
        }
        Ok(())
    }

    /// [`Vm::invoke_value`] for a loop continuation on the dispatch loop: push `f`'s frame over
    /// `args` with `ret` and return `true`; the loop runs it and its return applies `ret`, so a
    /// wait in `f` parks like any call. `false`, with nothing pushed, for a callee that runs no
    /// frame of its own (a native, a builtin, an extern, a generator fn): the caller runs it with
    /// `invoke_value`.
    pub(super) fn push_callback_frame(
        &mut self,
        f: Value,
        args: &[Value],
        ret: RetAction,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        let (proto, home, closure) = match self.callable(f) {
            Some(Callee::Func { proto, home }) => (proto, home, None),
            Some(Callee::Closure { proto, home }) => (proto, home, f.as_obj()),
            Some(Callee::Native { .. } | Callee::Builtin(_) | Callee::Cffi(_)) | None => {
                return Ok(false);
            }
        };
        if self.program.protos[proto].is_generator {
            return Ok(false);
        }
        self.callee_arity(proto, closure.is_some(), args.len(), span)?;
        self.frame_depth_guard(true, span)?;
        let base = self.stack.len();
        self.stack.extend_from_slice(args);
        self.finish_frame(proto, home, closure, base, true, false, span, ret);
        Ok(true)
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
        let Some(callee_kind) = self.callable(callee) else {
            return Err(self.err(
                format!("'{}' is not callable", self.type_name(callee)),
                span,
            ));
        };
        match callee_kind {
            Callee::Func { proto, home } => {
                self.callee_arity(proto, false, argc, span)?;
                // Experimental generators — allocate, don't run (see `do_call`'s fast path).
                if self.program.protos[proto].is_generator {
                    return Ok(self.alloc_generator(proto, home, None, args));
                }
                self.run_proto(proto, home, None, args, true, false, span)
            }
            Callee::Closure { proto, home } => {
                let h = callee.as_obj().expect("callable is an object");
                self.callee_arity(proto, true, argc, span)?;
                if self.program.protos[proto].is_generator {
                    return Ok(self.alloc_generator(proto, home, Some(h), args));
                }
                self.run_proto(proto, home, Some(h), args, true, false, span)
            }
            Callee::Native {
                func,
                name,
                kind,
                defaults,
            } => {
                let args = self.fill_decl_defaults(args, defaults);
                self.invoke_native(func, &name, kind, args, span)
            }
            // A first-class universe builtin fn value (`print`/`ord`/`chr`/`panic`) — route
            // back into the SAME logic direct calls use. `panic` returns `Err` (mirrors
            // `do_builtin`'s panic arm) so defers still unwind. `ord`/`chr` reuse
            // `builtin_ord`/`builtin_chr` directly.
            Callee::Builtin(b) => match b {
                // `print`'s declared slots are `(...args, sep, end)`, so the value takes
                // `(List, sep?, end?)`; an absent `sep`/`end` is the declaration's default.
                Builtin::Print => {
                    let program = Arc::clone(&self.program);
                    let text = |vm: &Self, i: usize, default: &String| {
                        args.get(i)
                            .and_then(|v| vm.val_str(*v))
                            .unwrap_or_else(|| default.clone())
                    };
                    let sep = text(self, 1, &program.print_defaults[0]);
                    let end = text(self, 2, &program.print_defaults[1]);
                    let items: Vec<Value> = args
                        .first()
                        .and_then(|v| v.as_obj())
                        .and_then(|h| self.heap.get(h).as_list().map(RVec::to_vec))
                        .unwrap_or_default();
                    // ROOT the items on the operand stack while they stringify. `args` was
                    // `split_off` the stack (do_call slow path), so it is NOT a GC root; a
                    // `Stringable` `str` method runs user code that can `collect()` at a safepoint.
                    for v in &items {
                        self.push(*v);
                    }
                    self.do_print_sep(items.len(), false, &sep, &end, false, span)?;
                    Ok(self.pop())
                }
                Builtin::Ord => self.builtin_ord(&args, span),
                Builtin::Chr => self.builtin_chr(&args, span),
                Builtin::Panic => Err(self.builtin_panic(&args, span)),
                // Only a first-class builtin is a value (`Builtin::first_class`).
                Builtin::Int
                | Builtin::Float
                | Builtin::Bool
                | Builtin::Str
                | Builtin::Bytes
                | Builtin::ByteArray
                | Builtin::List
                | Builtin::Map
                | Builtin::Set
                | Builtin::Channel
                | Builtin::Shared
                | Builtin::RwShared
                | Builtin::Atomic
                | Builtin::AtomicInt
                | Builtin::Executor => {
                    unreachable!("non-first-class builtin {} reached invoke_value", b.name())
                }
            },
            Callee::Cffi(cffi) => {
                // Arity is checker-guaranteed, but guard defensively (a hand-built program
                // could bypass the checker) so a wrong arg count never indexes out of bounds.
                if !cffi.is_c_variadic() {
                    self.check_arity("function", cffi.name(), cffi.param_count(), argc, span)?;
                } else if argc < cffi.param_count() {
                    return Err(self.err(
                        format!(
                            "function '{}' expects at least {} argument(s), got {argc}",
                            cffi.name(),
                            cffi.param_count()
                        ),
                        span,
                    ));
                }
                // Go `cgocall`: a C call runs detached and returns an off-heap `NativeRet`; nothing
                // allocates. It reads the args copied out before the door opens. A callback that C
                // calls meanwhile runs attached (`DoorHost::invoke_callback`, Go `cgocallback`) on
                // the arg values, which the door pins.
                let pins: Vec<GcRef> = args.iter().filter_map(|v| v.child_gcref()).collect();
                let copies = self.copy_args(&args);
                let ret = self.wait_call(
                    Some(WaitSpec::Extern),
                    "an extern call",
                    span,
                    &pins,
                    |door| {
                        cffi.call(&mut DoorHost {
                            door,
                            args: copies,
                            vals: args,
                        })
                    },
                )?;
                let ret = ret.map_err(|e| RuntimeError {
                    message: e.message,
                    span,
                    is_assert: false,
                    is_over_memory: false,
                    is_timed_out: false,
                    is_deadlock: false,
                    is_panic: false,
                    blocked: None,
                })?;
                Ok(self.lower_native(ret))
            }
        }
    }

    /// TICKET-226 — THE answer to "is `v` callable, and how": call, `prepare_worker`,
    /// `Executor.submit` and the entrypoint all read it. Borrows the heap object only long enough to copy its
    /// `Copy`/cheap fields, so the hot fn/closure paths allocate nothing. The `match` names every
    /// `Obj` variant and has no `_` arm: a new `Obj` kind does not compile until it is classified.
    /// Always inlined: as a call, its `Option<Callee>` result went back through the stack in
    /// pieces, and the caller's wide reload stalled on every HOF callback (M2.8 profile).
    #[inline(always)]
    pub(super) fn callable(&self, v: Value) -> Option<Callee> {
        let h = v.as_obj()?;
        match self.heap.get(h) {
            Obj::Func { proto, home } => Some(Callee::Func {
                proto: *proto,
                home: *home,
            }),
            Obj::Closure { proto, home, .. } => Some(Callee::Closure {
                proto: *proto,
                home: *home,
            }),
            Obj::Native {
                func,
                name,
                kind,
                defaults,
            } => Some(Callee::Native {
                func: *func,
                name: name.clone(),
                kind: *kind,
                defaults,
            }),
            Obj::Builtin(b) => Some(Callee::Builtin(*b)),
            Obj::Cffi(c) => Some(Callee::Cffi(std::sync::Arc::clone(c))),
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
            | Obj::Module(_)
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

    /// The one filler of a short native call: append the decl's default of each trailing
    /// parameter the call left out. Only a call through a fn value is short (a direct call has its
    /// defaults filled at the call site by the checker's binder), as a user fn value's prologue fills
    /// its own (DEC-182).
    pub(super) fn fill_decl_defaults(
        &mut self,
        mut args: Vec<Value>,
        defaults: &crate::base::builtin_method::DeclDefaults,
    ) -> Vec<Value> {
        use crate::base::builtin_method::DeclDefault;
        let arity = defaults.min + defaults.values.len();
        if args.len() < defaults.min {
            return args;
        }
        for d in &defaults.values[args.len().min(arity) - defaults.min..] {
            let v = match *d {
                DeclDefault::Int(n) => self.make_int(n),
                DeclDefault::Float(x) => self.box_float(x),
                DeclDefault::Bool(b) => Value::bool(b),
                DeclDefault::Str(s) => self.alloc_str(s.to_string()),
                DeclDefault::None => self.alloc_enum("Option", "None", &[]),
            };
            args.push(v);
        }
        args
    }

    /// Invoke a native (Rust) function value (M6c). Builds a [`VmHost`] over the evaluated args,
    /// runs the binding, then lowers its engine-neutral [`NativeRet`] into a heap-allocated `Value`
    /// and pushes it. Lowering (the only allocation) happens here — at an instruction boundary,
    /// after the call returns — so the "collect only at instruction boundaries" GC invariant holds.
    ///
    /// `kind` is the native's [`crate::native::Kind`], carried from its registry entry on the
    /// `Obj::Native` value — it decides EVERYTHING this fn branches on (intercept / offload / timed
    /// wait / inline). Nothing here compares a native's name to a string literal: that scatter is what
    /// `docs/design/roadmap.md` §3c removed, because a new blocking native that forgot to join the old
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
            Kind::InterceptNet => {
                self.sched_seed_point();
                return self.net_connect_or_listen(name, args, span);
            }
            Kind::InterceptIo => return self.io_native(name, args, span),
            Kind::InterceptTimer => return self.new_timer(&args, span),
            Kind::Inline | Kind::Blocking | Kind::TimedWait | Kind::HostWait => {}
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
        if kind.blocks() {
            self.wait_halt(span)?;
        }
        if WaitSpec::of_native(kind).is_some_and(|s| matches!(self.block_mode(s), BlockMode::Park))
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
                                wake: self.wake_set(),
                                run_deadline: self.deadline,
                                timeout_ms: self.timeout_ms,
                            }),
                        }
                    })
                }
                Kind::Inline
                | Kind::Blocking
                | Kind::InterceptIo
                | Kind::InterceptNet
                | Kind::HostWait
                | Kind::InterceptTimer => Some(OffloadReq {
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
        if kind == Kind::TimedWait
            && self.block_mode(WaitSpec::Sleep) == BlockMode::Demote
            && let Some(ms) = args.first().and_then(|v| self.int_val(*v))
            && ms > 0
        {
            let now = std::time::Instant::now();
            let deadline = now
                .checked_add(std::time::Duration::from_millis(ms as u64))
                .unwrap_or_else(|| now + std::time::Duration::from_secs(86_400 * 365));
            return self.demote_block_until(deadline, WaitSpec::Sleep, "sleep_ms", span);
        }
        // W7-16 — every OTHER timed wait with `ms > 0`: an eager `Executor` job (`mn == None`), the
        // top-level `main` thread, the inline outermost-`parallel:` builder VM, or a `mn == None`
        // native callback.
        // All of them used to reach `native::time::sleep_ms`'s bare `std::thread::sleep`, which is a
        // hole in every halt the loop it replaces would have checked: `cancel()` at 50 ms against
        // a `sleep_ms(3000)` waited the full 3012 ms AND ran the job's post-sleep code, and
        // `chezzi test --timeout=200` did not abort it either (measured: PASS, 3 s). The deadline is
        // OURS, so it stays a checkpoint for its whole duration — see [`Vm::block_until_deadline`].
        //
        // Deliberately NOT gated on `native_reentry == 0`: a native callback loop is already a
        // documented cancellation checkpoint, `demote_block_until` above already faults from inside
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
            self.block_until_deadline(WaitSpec::Sleep, deadline, span)?;
            return Ok(Value::nil());
        }
        let writes_before = self.stdout_writes;
        // A native that holds its host thread (`Blocking` run inline, `HostWait`) waits through
        // one bracket whose steps come from its `WaitSpec` row (`block::wait_steps`): stdin demotes
        // in every context, so a sibling runs at one worker too, and its wait vetoes the deadlock
        // verdict. Gated on `holds_host_thread`, so `sleep_ms(<=0)` never demotes for a no-op.
        let in_place = kind.holds_host_thread();
        let raw = if in_place {
            let what = if kind == Kind::HostWait {
                "a stdin read"
            } else {
                "a blocking call"
            };
            // It waits detached and returns an off-heap `NativeRet`: it allocates nothing on the
            // heap. It reads the args copied out before the door opens, so the door pins nothing.
            let copies = self.copy_args(&args);
            self.wait_call(WaitSpec::of_native(kind), what, span, &[], |door| {
                func(&mut DoorHost {
                    door,
                    args: copies,
                    vals: Vec::new(),
                })
            })?
        } else {
            func(&mut VmHost { vm: self, args })
        };
        let ret = raw.map_err(|e| RuntimeError {
            message: e.message,
            span,
            is_assert: false,
            is_over_memory: false,
            is_timed_out: false,
            is_deadlock: false,
            is_panic: false,
            blocked: None,
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
    /// the dirty-pool thread can run the call without the heap ([`ArgCopy::offload_form`]). Returns
    /// `None` if any arg is not a primitive (int / float / bool / str / bytes / a str list or map) —
    /// the scoped blocking fns only ever take primitives, so a non-primitive means "don't offload,
    /// run inline" (a safe fallback, never a fault).
    pub(super) fn extract_native_args(
        &self,
        args: &[Value],
    ) -> Option<Vec<crate::native::NativeArg>> {
        args.iter()
            .map(|&v| self.copy_arg(v).offload_form())
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
                Value::obj(self.heap.alloc(Obj::List(vs.into())))
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
                let map = MapData::default();
                for (k, v) in entries {
                    let lk = self.lower_native(k);
                    let lv = self.lower_native(v);
                    let hk = self.scalar_hash(lk);
                    map.push(&self.heap, hk, lk, lv);
                }
                Value::obj(self.heap.alloc(Obj::Map(map)))
            }
            N::Ok(inner) => {
                let p = self.lower_native(*inner);
                self.alloc_enum("Result", "Ok", &[p])
            }
            N::Err(msg) => {
                let p = self.alloc_str(msg);
                self.alloc_enum("Result", "Err", &[p])
            }
            N::Some(inner) => {
                let p = self.lower_native(*inner);
                self.alloc_enum("Option", "Some", &[p])
            }
            N::None => self.alloc_enum("Option", "None", &[]),
        }
    }

    /// Return from the current frame. `propagated` true ⇒ the value came from `?` (no observable
    /// difference here; the caller treats it as the function's result).
    ///
    /// Deferred calls (`defer`) run LIFO first, while the frame is still live so the GC keeps their
    /// values — and the return value — rooted. A fault in a deferred call supersedes the frame's
    /// result (Go: a panic in a defer wins): it returns `Err` and the frame is still torn down.
    pub(super) fn do_return(&mut self, _propagated: bool) -> Result<(), RuntimeError> {
        // TICKET-132 — an escaped fiber-owned level PARKS the owner (rewinds `ip`); the op re-runs
        // with the drained defers consumed, and takes back the fault the park carried.
        let err = self.take_escape_park();
        self.return_levels(err)
    }

    /// `do_return` after the escaped `parallel:` levels (`return_levels`), with their fault
    /// `defer_err`.
    pub(super) fn return_join(
        &mut self,
        defer_err: Option<RuntimeError>,
    ) -> Result<(), RuntimeError> {
        // W14-4 -- escaped `parallel:` levels unwind FIRST, innermost-first: each level's body defers,
        // then its cancel + wait (its children run their defers), THEN the implicit-nursery join, THEN
        // the frame's remaining defers. So the enclosing defers run after the cancelled children have
        // unwound (Go cancel + `wg.Wait()`, asyncio `TaskGroup`). All of this runs while the frame is
        // still current and the return value (if any) still sits on the operand stack.
        //
        // M-C implicit nurseries: if this frame opened one, JOIN it (run its spawned tasks to
        // completion) BEFORE the frame unwinds — `return`/`?`/fall-through is the join barrier.
        // `join_nursery` swaps the whole `FiberCtx`, never the operand value, so the value survives.
        // A task that faults during the join propagates as this function's error (the frame is
        // intact, so the normal unwind machinery runs its defers). NB: an uncaught *body* fault never
        // reaches here — it unwinds via the handler path, which cancels (not joins) the implicit
        // nursery.
        let frame_top = self.frames.last().unwrap();
        let nursery_floor = frame_top.nursery_len;
        let implicit = frame_top.has_implicit_nursery;
        if implicit && self.nurseries.len() > nursery_floor {
            // A body defer faulted above the implicit nursery: return `Err` with the frame intact,
            // so the fault path cancels the implicit nursery and runs the remaining defers, as for a
            // join fault. Joining could park and lose the fault.
            if let Some(e) = defer_err {
                return Err(e);
            }
            self.join_nursery_released()?; // join the implicit nursery (runs its tasks)
            // TICKET-103 — `join_nursery` parked this fiber (`Disp::JoinPark`) and rewound `ip`
            // to the op that called us: `Op::Return`, or `Op::Try` for a `?`. Leave the frame and
            // the return value on the stack; the op re-executes after the wake and completes the
            // return. `do_try` pushed the propagated value back before calling us, so the
            // rewound `Op::Try` re-pops the same value.
            if self.join_suspend.is_some() {
                return Ok(());
            }
            // TICKET-147 (W14-15) — the join is a cancellation point: a cancelled owner returns `Err`
            // with the frame intact, so the cancel unwind runs its defers.
            self.cancel_at_join(Span::RUNTIME)?;
        }
        // Drain with the return value still on top of the stack (rooted) and the frame still on
        // `self.frames` (so `collect` roots the pending records). The frame's remaining defers run
        // AFTER the escaped levels and the implicit-nursery join above (children complete, then
        // cleanup). `return_levels` leaves no level above `nursery_floor`, so no park can
        // follow the defers. The rest is `return_finish`.
        self.drain_then(0, DrainCont::Return { err: defer_err })
    }

    /// The end of `do_return`, after the frame's defers ran: `defer_err` is their latest fault, else
    /// the escaped levels'. The return value is on top of the stack.
    pub(super) fn return_finish(
        &mut self,
        defer_err: Option<RuntimeError>,
    ) -> Result<(), RuntimeError> {
        let ret = self.pop();
        let frame = self.frames.pop().unwrap();
        if frame.counted {
            self.call_depth -= 1;
        }
        self.stack.truncate(frame.base);
        self.cur_base = self.frames.last().map(|f| f.base).unwrap_or(0);
        self.push(ret); // rooted across the reclaim below, which can wait and so collect
        // Reclaim any `parallel:` nursery this frame opened but whose `JoinNursery` was skipped by a
        // `?`/return escape (no-op on the normal fall-through path — `JoinNursery` already popped it,
        // so `nurseries.len() == frame.nursery_len`; also a no-op when the implicit nursery above was
        // just joined). Mirrors the `recover:` catch path; a nested frame keeps the parent's
        // nursery (it captured the parent depth
        // at entry). TASK B: route through `drain_escaped_nursery` so the unstarted tasks are
        // cancelled-and-reported (not silently dropped). NB: within-frame `break`/`continue` out of a
        // `parallel:` no longer rely on this — the compiler emits a `ReclaimNursery` before their
        // loop-exit `Jump` (see `compile_parallel`/`emit_loop_escape_drain`), reclaiming block-scoped.
        let escaped = self.drain_escaped_nursery(frame.nursery_len);
        // Drop any `recover:` handlers installed in the frame we just left (e.g. a `?` early-return
        // out of a recover block) — they must not survive to catch a later, unrelated fault.
        while self
            .handlers
            .last()
            .is_some_and(|h| h.frame_len > self.frames.len())
        {
            self.handlers.pop();
        }
        // TICKET-147 — a defer fault stays the root cause; an aborted nursery's child fault fills the gap.
        if let Some(e) = defer_err.or(escaped) {
            self.pop();
            // A verdict's fault (the defer was cut) releases a box guard as a cut one.
            self.drop_ret(frame.ret, frame.base, e.is_deadlock);
            return Err(e);
        }
        // A hook frame's operator post-step, at the operator's own span.
        match frame.ret {
            RetAction::Push => Ok(()),
            ret @ (RetAction::Discard
            | RetAction::ToBool
            | RetAction::EqBool { .. }
            | RetAction::CmpOp(_)
            | RetAction::Stringify
            | RetAction::StringifyFmt
            | RetAction::Hof(_)
            | RetAction::Drain(_)
            | RetAction::Print { .. }
            | RetAction::Sort(_)
            | RetAction::MinMax { .. }
            | RetAction::Box(_)
            | RetAction::Gen(_)) => self.apply_ret(ret, frame.call_span),
        }
    }

    /// `defer f(args)` / `defer recv.m(args)` — pop the callee/receiver + `argc` args off the stack
    /// and record a deferred call on the current frame (drained LIFO at frame exit). The values were
    /// evaluated now (Go semantics); the call runs at exit.
    pub(super) fn do_defer(&mut self, method: Option<(Arc<str>, u32)>, argc: usize, span: Span) {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let head = self.pop();
        let d = match method {
            Some((name, ic)) => Deferred::Method {
                recv: head,
                name,
                ic,
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

    /// Run frame `fi`'s next deferred call to completion on the Rust stack (the result is
    /// discarded): the synchronous drain ([`Vm::drain_deferred`]). The record is called as the
    /// trampoline calls it ([`Vm::run_loaded`]) and stays in the frame, rooted, until it returns.
    pub(super) fn run_one_deferred(&mut self, fi: usize) -> Result<(), RuntimeError> {
        // A defer is the cleanup a cancel exists to run, so NO cancellation checkpoint fires inside
        // it: `deferring > 0` ⇒ `in_defer()` ⇒ `halt_requested()` is `None` (frames.rs). It must be
        // raised BEFORE `guarded` (whose own checkpoint would otherwise eat this very call) and
        // lowered on every exit path, including a fault thrown by the deferred body itself.
        // Panic-safe, same reasoning as `guarded`'s `native_reentry` (exec.rs): this fn
        // calls `guarded`, which CATCHES a re-entered FFI callback's Rust panic and `resume_unwind`s it,
        // so a plain `-= 1` after the call would be skipped on that unwind and leak `deferring` at +1 for
        // the VM's lifetime. Since W7-3 that leak is worse than "the task stops hitting checkpoints": it
        // would also permanently disable the cancel bypass, letting an ordinary outer `recover:` defeat a
        // nursery cancel. A `Drop` guard can't be used (it would alias `self` across the call).
        self.deferring += 1;
        // Guarded: a synchronous drain is Rust-stack state, so a blocking `recv` inside it cannot
        // park — it demotes or is judged (B1).
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.guarded(|vm| vm.run_loaded(fi))
        }));
        self.deferring -= 1;
        match r {
            Ok(v) => v,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    // ----- concurrency C4: sequential, run-to-completion executor -----
}
