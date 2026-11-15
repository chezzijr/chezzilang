//! VM: arity checks, native-bodied methods, operator hooks, `do_method_call`, static calls.
//! Split out of `src/vm/call.rs`. `super::*` is the parent module.

use super::shared_methods::RwCall;
use super::*;
use crate::base::hooks::Hook;
use crate::base::method_table::{Intrinsic, MethodEntry, MethodId, MethodRow, NativeArm};

impl Vm {
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
                crate::base::provider_name::display_fn_name(&p.name)
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

    /// TICKET-250 — does the declared type of `v` have `hook`? The checker decided it
    /// (`Checker::hook_method`); this reads `Program::hooks_struct` / `hooks_enum`. `false` for
    /// every value that is not a struct or an enum.
    pub(super) fn hook_of(&self, v: Value, hook: Hook) -> bool {
        let Some(h) = v.as_obj() else {
            return false;
        };
        let set = match self.heap.get(h) {
            Obj::Struct { tid, .. } => self.program.hooks_struct.get(*tid as usize),
            Obj::Enum { variant_id, .. } => self.program.hooks_enum.get(*variant_id as usize),
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
        };
        set.is_some_and(|bits| bits & hook.bit() != 0)
    }

    /// The proto and home module of `recv`'s `hook`, when its type has that hook.
    fn hook_proto(&self, recv: Value, hook: Hook) -> Option<(ProtoId, GcRef)> {
        if !self.hook_of(recv, hook) {
            return None;
        }
        match self.heap.get(recv.as_gcref()) {
            Obj::Struct { tid, .. } => {
                let def = self.program.structs.get(self.struct_name_of_tid(*tid))?;
                let proto = *def.methods.get(hook.method())?;
                Some((proto, self.module_obj(def.module_idx)))
            }
            Obj::Enum { variant_id, .. } => {
                let key = self.enum_names(*variant_id).0;
                let proto = *self.program.enum_methods.get(key)?.get(hook.method())?;
                Some((proto, self.module_obj(self.enum_home_module(key))))
            }
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

    /// TICKET-250 — the one entry an operator's handler calls a hook through. `Ok(None)` when
    /// `recv` has no such hook; else the hook's result. A generator hook allocates a generator,
    /// as every method call does. It enters the proto directly, not through `do_method_call`:
    /// that frame is large, and a hook that re-enters itself (`fn str(self): str(self)`) must
    /// reach the call-depth fault before the native stack ends. The receiver and `args` are
    /// rooted as the new frame's locals. The caller keeps its own re-entry wrapper
    /// (`reentered`, `guarded`, `guarded_walk`).
    pub(super) fn invoke_hook(
        &mut self,
        hook: Hook,
        recv: Value,
        args: &[Value],
        span: Span,
    ) -> Result<Option<Value>, RuntimeError> {
        let Some((proto, home)) = self.hook_proto(recv, hook) else {
            return Ok(None);
        };
        self.check_proto_arity(proto, args.len() + 1, span)?;
        let mut call_args = Vec::with_capacity(args.len() + 1);
        call_args.push(recv);
        call_args.extend_from_slice(args);
        if self.program.protos[proto].is_generator {
            return Ok(Some(self.alloc_generator(proto, home, None, call_args)));
        }
        self.run_proto(proto, home, None, call_args, true, false, span)
            .map(Some)
    }

    /// [`Vm::invoke_hook`] for an operator handler on the dispatch loop: push the hook's frame with
    /// `ret` and return; the loop runs the frame and its return applies `ret`, so a wait in the hook
    /// parks like any call. `Ok(false)` when `recv` has no such hook. A generator hook allocates its
    /// generator, and `ret` applies to it at once. Legal only where the caller's remaining work is
    /// `ret` (the handler returns without pushing); a synchronous native caller uses `invoke_hook`.
    pub(super) fn push_hook_frame(
        &mut self,
        hook: Hook,
        recv: Value,
        args: &[Value],
        ret: RetAction,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        let Some((proto, home)) = self.hook_proto(recv, hook) else {
            return Ok(false);
        };
        self.check_proto_arity(proto, args.len() + 1, span)?;
        if self.program.protos[proto].is_generator {
            let mut call_args = Vec::with_capacity(args.len() + 1);
            call_args.push(recv);
            call_args.extend_from_slice(args);
            let g = self.alloc_generator(proto, home, None, call_args);
            self.push(g);
            self.apply_ret(ret, span)?;
            return Ok(true);
        }
        self.frame_depth_guard(true, span)?;
        let base = self.stack.len();
        self.stack.push(recv);
        self.stack.extend_from_slice(args);
        self.finish_frame(proto, home, None, base, true, false, span, ret);
        Ok(true)
    }

    /// The fault an operator reports on a struct or enum that has no `hook`.
    pub(super) fn missing_hook(&self, recv: Value, hook: Hook, span: Span) -> RuntimeError {
        let method = hook.method();
        let who = match self.heap.get(recv.as_gcref()) {
            Obj::Struct { tid, .. } => {
                let name = self.struct_name_of_tid(*tid);
                let display = self
                    .program
                    .structs
                    .get(name)
                    .map_or(name, |d| d.display_name.as_str());
                format!("struct '{display}'")
            }
            Obj::Enum { variant_id, .. } => {
                let key = self.enum_names(*variant_id).0.to_string();
                Self::enum_subject(*variant_id, &key, |n| format!("enum '{n}'"))
            }
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
            | Obj::Generator(_) => self.type_name(recv).to_string(),
        };
        self.err(format!("{who} has no '{method}' method"), span)
    }

    /// `Op::CallHook` — a method call typed through a reserved protocol's requirement. A receiver
    /// that is not a struct or enum, or whose type has the hook, takes the ordinary dispatch. A
    /// struct or enum without the hook never reaches a declared method of that name: the builtin
    /// grant answers, or a `next` owner is its own cursor for `iter`.
    pub(super) fn call_hook(
        &mut self,
        hook: Hook,
        method: MethodId,
        argc: usize,
        ic: u32,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let recv = self.stack[self.stack.len() - argc - 1];
        let declared = recv
            .as_obj()
            .is_some_and(|h| matches!(self.heap.get(h), Obj::Struct { .. } | Obj::Enum { .. }));
        if !declared || self.hook_of(recv, hook) {
            return self.do_method_call(method, argc, ic, span);
        }
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        self.pop();
        if let Some(v) = self.intrinsic_proto_method(recv, Intrinsic::Hook(hook), &args, span)? {
            self.push(v);
            return Ok(());
        }
        if hook == Hook::Iter && args.is_empty() && self.hook_of(recv, Hook::Next) {
            self.push(recv);
            return Ok(());
        }
        let name = self.program.methods.name(method);
        Err(self.err(
            format!("type {} has no method '{name}'", self.type_name(recv)),
            span,
        ))
    }

    /// `recv.method(args)` from a running dispatch loop: a `CallMethod` op, or a `defer` record
    /// (`Deferred::Method`; the synchronous drain `Vm::run_loaded` runs the pushed frame in its own
    /// `run_until`). A user body is installed as a frame for that loop to run. `ic` is the site's
    /// method inline-cache id, always a real one. The struct IC answers first; every other call
    /// reads the receiver's entry of the method table (`Program::methods`) and runs it.
    pub(super) fn do_method_call(
        &mut self,
        method: MethodId,
        argc: usize,
        ic: u32,
        span: Span,
    ) -> Result<(), RuntimeError> {
        debug_assert!(
            (ic as usize) < self.method_ic.len(),
            "do_method_call needs a real method IC site, got {ic}"
        );
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let recv = self.pop();
        // M19 Phase 6 / N-way poly — method-call inline-cache fast path (struct methods only). Scan
        // the site's ways for a way whose cached `tid` matches the receiver layout: a hit skips the
        // table read AND flattens the call: `[recv, args…]` go on the stack and the method frame is
        // installed in place, so the running `run_until` executes the body and its `Return` pushes
        // the result — no re-entrant `run_proto`. A megamorphic-but-bounded site (≤4 distinct
        // receiver types) hits a way for each, so it never thrashes the monomorphic refill. The
        // arity guard re-runs per hit (cheap) so a hit can never enter a frame with the wrong slot
        // count, and the tid re-compare on every probe bars a wrong body. A `sticky` site
        // (overflowed past 4 types) skips the probe and reads the table.
        let site = self.method_ic[ic as usize];
        if !site.sticky
            && let Some(h) = recv.as_obj()
            && let Obj::Struct { tid, .. } = self.heap.get(h)
        {
            let recv_tid = *tid;
            let hit = site
                .ways
                .iter()
                .find(|way| way.tid != TID_NONE && way.tid == recv_tid)
                .copied();
            if let Some(cell) = hit {
                let proto = cell.proto;
                self.check_proto_arity(proto, argc + 1, span)?;
                let home = self.module_obj(cell.module_idx as usize);
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
        match self.method_entry(recv, method) {
            MethodEntry::Proto { proto, home } => {
                self.enter_method(proto as ProtoId, home, recv, args, ic, span)
            }
            // A fn-typed field: `recv.f(args)` where `f` holds a function value (the checker
            // verified `f: fn(...) -> ...`), invoked as a value (no `self` bound).
            MethodEntry::Field(i) => {
                let fval = if let Obj::Struct { fields, .. } = self.heap.get(recv.as_gcref()) {
                    fields.get(i as usize)
                } else {
                    None
                };
                let Some(fval) = fval else {
                    return Err(self.method_miss(recv, method, &args, span));
                };
                let v = self.invoke_value(fval, args, span)?;
                if self.paused() {
                    return Ok(()); // B1/D3: the function-field call parked on `recv` or yielded.
                }
                self.push(v);
                Ok(())
            }
            MethodEntry::Native(arm) => self.native_method(arm, recv, args, span),
            MethodEntry::Intrinsic(which) => self.intrinsic_method(which, recv, method, args, span),
            MethodEntry::Miss => Err(self.method_miss(recv, method, &args, span)),
        }
    }

    /// The method-table entry of `method` on `recv`'s row: a fixed row for every kind but a
    /// struct (its `tid`'s row) and an enum (its variant's enum's row).
    fn method_entry(&self, recv: Value, method: MethodId) -> MethodEntry {
        let table = &self.program.methods;
        let row = match recv.view() {
            ValueView::Int(_) => MethodRow::Int,
            ValueView::Bool(_) => MethodRow::Bool,
            ValueView::Nil => MethodRow::Nil,
            ValueView::Obj(_) if recv.is_float() => MethodRow::Float,
            ValueView::Obj(h) => match self.heap.get(h) {
                Obj::Struct { tid, .. } => return table.of_struct(*tid, method),
                Obj::Enum { variant_id, .. } => return table.of_enum(*variant_id, method),
                Obj::Str(_) => MethodRow::Str,
                Obj::Bytes(_) => MethodRow::Bytes,
                Obj::ByteArray(_) => MethodRow::ByteArray,
                Obj::BigInt(_) => MethodRow::Int,
                Obj::FloatBox(_) => MethodRow::Float,
                Obj::Iter { .. } => MethodRow::Cursor,
                Obj::List(_) => MethodRow::List,
                Obj::Tuple(_) => MethodRow::Tuple,
                Obj::Map(_) => MethodRow::Map,
                Obj::Set(_) => MethodRow::Set,
                Obj::Cell(_) => MethodRow::Cell,
                Obj::Func { .. }
                | Obj::Closure { .. }
                | Obj::Native { .. }
                | Obj::Builtin(_)
                | Obj::Cffi(_) => MethodRow::Func,
                Obj::Module(_) => MethodRow::Module,
                Obj::Ptr(_) => MethodRow::Ptr,
                Obj::Channel(_) => MethodRow::Channel,
                Obj::Shared(_) => MethodRow::Shared,
                Obj::RwShared(_) => MethodRow::RwShared,
                Obj::Atomic(_) => MethodRow::Atomic,
                Obj::AtomicInt(_) => MethodRow::AtomicInt,
                Obj::Executor(_) => MethodRow::Executor,
                Obj::Socket(_) => MethodRow::Socket,
                Obj::Listener(_) => MethodRow::Listener,
                Obj::Writer(_) => MethodRow::Writer,
                Obj::Reader(_) => MethodRow::Reader,
                Obj::Generator(_) => MethodRow::Generator,
            },
        };
        table.fixed(row, method)
    }

    /// Run a compiled method body on `recv`: a generator method allocates rather than running (else
    /// its `Op::Yield` would poison the host run); any other body is installed as a frame in place
    /// for the running `run_until`. A struct receiver fills the site's IC first.
    fn enter_method(
        &mut self,
        proto: ProtoId,
        home_idx: u32,
        recv: Value,
        args: Vec<Value>,
        ic: u32,
        span: Span,
    ) -> Result<(), RuntimeError> {
        self.check_proto_arity(proto, args.len() + 1, span)?;
        let home = self.module_obj(home_idx as usize);
        if self.program.protos[proto].is_generator {
            let mut gen_args = Vec::with_capacity(args.len() + 1);
            gen_args.push(recv);
            gen_args.extend(args);
            let g = self.alloc_generator(proto, home, None, gen_args);
            self.push(g);
            return Ok(());
        }
        // Fill the next free way so the next call at this site hits the fast path (a registered
        // layout `tid` only). When all ways are occupied AND this `tid` is new, latch `sticky` so
        // the site stops probing the (full) ways — a megamorphic site never thrashes.
        if let Some(h) = recv.as_obj()
            && let Obj::Struct { tid, .. } = self.heap.get(h)
            && *tid != TID_NONE
        {
            let cell = MethodIcCell {
                tid: *tid,
                proto,
                module_idx: home_idx,
            };
            let site = &mut self.method_ic[ic as usize];
            if !site.sticky {
                if let Some(free) = site.ways.iter_mut().find(|w| w.tid == TID_NONE) {
                    *free = cell;
                } else {
                    site.sticky = true;
                }
            }
        }
        let base = self.stack.len();
        self.stack.push(recv);
        self.stack.extend(args);
        self.push_frame_in_place(proto, home, None, base, span)
    }

    /// Run the bodyless std method `arm` on `recv` and push its result, unless the method parked
    /// the fiber or ran a frame whose return pushes it. Each kind's tail is here: a channel's park,
    /// a box closure's frame, an Executor's join, a socket's poller park, a Writer's broken pipe.
    fn native_method(
        &mut self,
        arm: NativeArm,
        recv: Value,
        args: Vec<Value>,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let result = match arm {
            NativeArm::Int(m) => self.int_method(recv, m, &args, span)?,
            NativeArm::Float(m) => self.float_method(recv, m, &args, span)?,
            NativeArm::Str(_) | NativeArm::Map(_) | NativeArm::Set(_) => {
                self.core_method_as(recv.as_gcref(), arm, &args, span)?
            }
            // A list HOF runs its loop here: each callback is a frame, so a wait in it parks. So
            // does a sort or a min / max whose comparator may be user code.
            NativeArm::List(m) => {
                let h = recv.as_gcref();
                if let Some(kind) = HofKind::of(m) {
                    return self.list_hof(h, kind, args, true, span);
                }
                if let Some(c) = CmpMethod::of(m)
                    && self.list_compare_method(h, c, &args, true, span)?
                {
                    return Ok(());
                }
                self.core_method_as(h, arm, &args, span)?
            }
            NativeArm::Bytes(m) => self.bytes_method(recv.as_gcref(), m, &args, span)?,
            NativeArm::ByteArray(m) => self.bytearray_method(recv.as_gcref(), m, &args, span)?,
            // Concurrency C4: `Channel` / `Shared` methods mutate the heap object in place (and
            // `update` re-enters the VM), so they run directly off the handle.
            NativeArm::Channel(m) => {
                self.sched_seed_point();
                let result = self.channel_method(recv.as_gcref(), m, &args, span)?;
                if self.suspend.is_some() || self.send_suspend.is_some() {
                    // B1: `recv` parked this fiber (re-rooted the receiver itself); or a bounded
                    // `send` parked it on a full channel (re-rooted receiver + value). No push.
                    return Ok(());
                }
                result
            }
            // A box closure on the dispatch loop is a frame: its return pushes the result.
            NativeArm::Shared(m) => {
                self.sched_seed_point();
                match self.shared_method(recv.as_gcref(), m, &args, span)? {
                    Some(v) => v,
                    None => return Ok(()),
                }
            }
            NativeArm::RwShared(m) => {
                self.sched_seed_point();
                match self.rwshared_method(recv.as_gcref(), RwCall::Method(m), &args, span)? {
                    Some(v) => v,
                    None => return Ok(()),
                }
            }
            NativeArm::RwSharedPrim(p) => {
                self.sched_seed_point();
                match self.rwshared_method(recv.as_gcref(), RwCall::Prim(p), &args, span)? {
                    Some(v) => v,
                    None => return Ok(()),
                }
            }
            NativeArm::Atomic(m) => {
                self.sched_seed_point();
                self.atomic_method(recv.as_gcref(), m, &args, span)?
            }
            NativeArm::AtomicInt(m) => {
                self.sched_seed_point();
                self.atomic_int_method(recv.as_gcref(), m, &args, span)?
            }
            NativeArm::Executor(m) => {
                self.sched_seed_point();
                let r = self.executor_method(recv.as_gcref(), m, &args, span)?;
                if self.exec_join_suspend.is_some() {
                    return Ok(()); // a parked `shutdown` re-pushed its receiver and rewound.
                }
                r
            }
            // D6: `Socket` / `Listener` ops operate on the fd in the `Arc`'d core and may park the
            // fiber on the netpoller (a would-block `read`/`write`/`accept`); gate the result-push
            // on `poll_park` (as the channel `recv` park gate, but routed to the poller).
            NativeArm::Socket(m) => {
                self.sched_seed_point();
                let r = self.socket_method(recv.as_gcref(), m, &args, span)?;
                if self.poll_park.is_some() {
                    return Ok(()); // D6: the op `WouldBlock`ed and re-rooted the receiver itself.
                }
                r
            }
            NativeArm::Listener(m) => {
                self.sched_seed_point();
                let r = self.listener_method(recv.as_gcref(), m, &args, span)?;
                if self.poll_park.is_some() {
                    return Ok(());
                }
                r
            }
            // R2 / N1: a `stdout()`-backed `write` routes through `emit_out`, a NO-OP once the
            // streamed reader has died. `writer_method` then returns `Ok`, so — exactly like
            // `print` and `invoke_native` — the deterministic broken-pipe halt must be raised HERE
            // at the call site, or a `loop: w.write(...)` into a dead pipe spins forever, growing
            // the unbounded stream queue without bound (6f8bb5c). `stream_halt` is inert off the
            // streaming CLI path.
            //
            // W7-5d — gated on THIS call having emitted to stdout, for the reason spelled out at
            // `invoke_native`: unguarded, a dead stdout also faulted a write to a FILE-backed or
            // `stderr()`-backed `Writer`, which never touched the broken pipe.
            NativeArm::Writer(m) => {
                self.sched_seed_point();
                let writes_before = self.stdout_writes;
                let r = self.writer_method(recv.as_gcref(), m, &args, span)?;
                if self.stdout_writes != writes_before
                    && let Some(halt) = self.stream_halt(span)
                {
                    return Err(halt);
                }
                r
            }
            // R2b: `Reader` ops are synchronous blocking file reads — no netpoller/`poll_park`, and
            // NO `stream_halt` check (a Reader never emits to stdout/stderr).
            NativeArm::Reader(m) => {
                self.sched_seed_point();
                self.reader_method(recv.as_gcref(), m, &args, span)?
            }
        };
        self.push(result);
        Ok(())
    }

    /// Run the intrinsic method `which` on `recv`: a kind's own cursor and module entries here,
    /// the protocol methods through [`Vm::intrinsic_proto_method`]. One that declines (a wrong
    /// argument count) faults with the kind's miss text.
    fn intrinsic_method(
        &mut self,
        which: Intrinsic,
        recv: Value,
        method: MethodId,
        args: Vec<Value>,
        span: Span,
    ) -> Result<(), RuntimeError> {
        match which {
            // `.iter()` on a built-in collection → a FRESH cursor over a snapshot of what
            // `for x in X` yields: the [`builtin_items`](Self::builtin_items) row, or a channel's
            // drained values.
            Intrinsic::CollectionIter if args.is_empty() => {
                let h = recv.as_gcref();
                // Both readers may alloc (str per-char); root the receiver across the call.
                self.push(recv);
                let items: Option<RVec<Value>> = if matches!(self.heap.get(h), Obj::Channel(_)) {
                    Some(self.drain_iterable(recv, span)?.into())
                } else {
                    self.builtin_items(h)
                };
                self.pop(); // unroot receiver
                if let Some(items) = items {
                    let cursor = self.heap.alloc(Obj::Iter {
                        items,
                        pos: RCell::new(0),
                    });
                    self.push(Value::obj(cursor));
                    return Ok(());
                }
            }
            // A cursor IS Iterable and a generator IS an Iterator: `iter()` is itself.
            Intrinsic::SelfIter if args.is_empty() => {
                self.push(recv);
                return Ok(());
            }
            // A cursor's `next()` advances its snapshot (idempotent `None` past the end).
            Intrinsic::CursorNext if args.is_empty() => {
                let h = recv.as_gcref();
                let Obj::Iter { items, pos } = self.heap.write(h).get(h) else {
                    unreachable!("the cursor row holds cursors")
                };
                // M3.0a — one read answers the bound and the element: two threads can step
                // one shared cursor (a racy step can repeat or skip an item, as in Go).
                let at = pos.get();
                let item = items.try_get(at);
                if item.is_some() {
                    pos.set(at + 1);
                }
                let result = match item {
                    Some(v) => self.alloc_enum("Option", "Some", &[v]),
                    None => self.alloc_enum("Option", "None", &[]),
                };
                self.push(result);
                return Ok(());
            }
            // Experimental generators — `.next()` resumes the coroutine. The body runs as a frame
            // on this stack, so a wait in it parks.
            Intrinsic::GenNext if args.is_empty() => {
                return self.gen_resume(recv.as_gcref(), GenThen::Push, span);
            }
            // `module.fn(args)` — plain call on the looked-up member, no `self`. Read in place:
            // every task of the run shares the module (M3.1), so a clone would copy its slots per
            // call. A member can be a bodied Chezzi fn (`std.fs`'s `PathLike` wrappers): `do_call`
            // installs it as a frame for the running loop.
            Intrinsic::ModuleMember => {
                let h = recv.as_gcref();
                let name = self.program.methods.name(method);
                let Some(idx) = self.slot_index(h, name) else {
                    return Err(self.method_miss(recv, method, &args, span));
                };
                let member = self.read_slot(h, idx, span)?;
                let argc = args.len();
                self.stack.push(member);
                self.stack.extend(args);
                return self.do_call(argc, span);
            }
            Intrinsic::Hook(_) | Intrinsic::AsPath | Intrinsic::Error(_) => {
                if let Some(v) = self.intrinsic_proto_method(recv, which, &args, span)? {
                    self.push(v);
                    return Ok(());
                }
            }
            Intrinsic::CollectionIter
            | Intrinsic::SelfIter
            | Intrinsic::CursorNext
            | Intrinsic::GenNext => {}
        }
        Err(self.method_miss(recv, method, &args, span))
    }

    /// The fault of a method `recv`'s kind does not have, in the kind's own words.
    fn method_miss(
        &self,
        recv: Value,
        method: MethodId,
        args: &[Value],
        span: Span,
    ) -> RuntimeError {
        let m = self.program.methods.name(method);
        let Some(h) = recv.as_obj() else {
            return self.err(
                format!("type {} has no method '{m}'", self.type_name(recv)),
                span,
            );
        };
        let text = match self.heap.get(h) {
            Obj::Str(_) => format!("type str has no method '{m}'"),
            Obj::List(_) => format!("type list has no method '{m}'"),
            Obj::Map(_) => format!("type map has no method '{m}'"),
            Obj::Set(_) => format!("type set has no method '{m}'"),
            Obj::Bytes(_) => format!("type bytes has no method '{m}'"),
            Obj::ByteArray(_) => format!("type bytearray has no method '{m}'"),
            // ROOT REDESIGN — the BARE display name (not the identity key).
            Obj::Struct { tid, .. } => {
                let name = self.struct_name_of_tid(*tid);
                let display = self
                    .program
                    .structs
                    .get(name)
                    .map(|d| d.display_name.clone())
                    .unwrap_or_else(|| crate::base::words::bare_display(name));
                format!("struct '{display}' has no method '{m}'")
            }
            Obj::Enum { variant_id, .. } => {
                let key = self.enum_names(*variant_id).0;
                let who = Self::enum_subject(*variant_id, key, |n| format!("type {n}"));
                format!("{who} has no method '{m}'")
            }
            Obj::Module(_) => format!("module '{}' has no member '{m}'", self.module_name(h)),
            Obj::Iter { .. } if !args.is_empty() => {
                format!("a cursor's '{m}' takes no arguments")
            }
            Obj::Iter { .. } => format!("a cursor has no method '{m}' (only `next()`/`iter()`)"),
            Obj::Generator(_) => format!("a generator has no method '{m}' (only `next()`)"),
            Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Tuple(_)
            | Obj::Cell(_)
            | Obj::Func { .. }
            | Obj::Closure { .. }
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
            | Obj::Reader(_) => format!("type {} has no method '{m}'", self.type_name(recv)),
        };
        self.err(text, span)
    }

    /// The proto of method `method` on the struct, the enum or the builtin `type_key`, and the
    /// index of the type's declaring module (its home). The one lookup `CallStatic` and `MakeMethodFunc`
    /// share (TICKET-204).
    pub(super) fn type_method_proto(
        &self,
        type_key: &str,
        method: &str,
    ) -> Option<(ProtoId, usize)> {
        let prog = &self.program;
        if let Some(def) = prog.structs.get(type_key) {
            return def.methods.get(method).map(|&p| (p, def.module_idx));
        }
        if let Some(ms) = prog.enum_methods.get(type_key) {
            return ms
                .get(method)
                .map(|&p| (p, self.enum_home_module(type_key)));
        }
        // A builtin's bodied static (`int.from`), compiled under the builtin's bare name.
        let ms = prog.native_methods.get(type_key)?;
        ms.get(method).map(|&p| (p, prog.native_home[type_key]))
    }

    /// The one fn object for `key` in this run (TICKET-215, M3.1), and the only allocator of
    /// `Obj::Func`, `Obj::Native`, `Obj::Cffi` and `Obj::Builtin`: a fn read twice, in two modules,
    /// in two tasks, or after a crossing is one value. A proto is keyed alone: a run has one module
    /// view, so a user proto's home is fixed, and a synthesized proto's home is fixed too (it reads
    /// no global, or, for `json.decode[T]`, is homed in std.json by `Op::MakeFuncIn`, TICKET-226).
    /// The miss allocates under the table lock, so two tasks never make two objects for one key;
    /// `Heap::alloc` never collects, so no safepoint is reached while the lock is held.
    pub(super) fn fn_value(&mut self, key: FnKey, make: impl FnOnce() -> Obj) -> Value {
        let mut table = self.fn_values.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&h) = table.get(&key) {
            return Value::obj(h);
        }
        let h = self.heap.alloc(make());
        table.insert(key, h);
        Value::obj(h)
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
        // The compiler only emits `CallStatic` for a static method that exists on a known
        // struct/enum, so a miss here is an internal invariant break — surface it as a clear
        // runtime error.
        let Some((proto, home_idx)) = self.type_method_proto(type_key, method) else {
            let display = match self.program.structs.get(type_key) {
                Some(def) => format!("'{}'", def.display_name),
                None => crate::base::words::bare_display(type_key),
            };
            let method = crate::base::provider_name::display_fn_name(method);
            return Err(self.err(
                format!("type {display} has no static method '{method}'"),
                span,
            ));
        };
        // A static method has NO receiver, so its argument count is `argc` exactly (no `+ 1`).
        self.check_proto_arity(proto, argc, span)?;
        let home = self.module_obj(home_idx);
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
}
