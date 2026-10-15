//! VM: arity checks, native-bodied methods, operator hooks, `do_method_call`, static calls.
//! Split out of `src/vm/call.rs`. `super::*` is the parent module.

use super::call::is_mutating_native;
use super::*;
use crate::base::hooks::Hook;

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
                Some((proto, self.module_objs[def.module_idx]))
            }
            Obj::Enum { variant_id, .. } => {
                let key = self.enum_names(*variant_id).0;
                let proto = *self.program.enum_methods.get(key)?.get(hook.method())?;
                Some((proto, self.module_objs[self.enum_home_module(key)]))
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
        name: &str,
        argc: usize,
        ic: u32,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let recv = self.stack[self.stack.len() - argc - 1];
        let declared = recv
            .as_obj()
            .is_some_and(|h| matches!(self.heap.get(h), Obj::Struct { .. } | Obj::Enum { .. }));
        if !declared || self.hook_of(recv, hook) {
            return self.do_method_call(name, argc, ic, span);
        }
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        self.pop();
        if let Some(v) = self.intrinsic_proto_method(recv, name, &args, span)? {
            self.push(v);
            return Ok(());
        }
        if hook == Hook::Iter && args.is_empty() && self.hook_of(recv, Hook::Next) {
            self.push(recv);
            return Ok(());
        }
        Err(self.err(
            format!("type {} has no method '{name}'", self.type_name(recv)),
            span,
        ))
    }

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
        // D4 layer C (TICKET-169): a mutating native call on an airlock copy is a lost write.
        if self.heap.is_copied(h) && is_mutating_native(self.heap.get(h), method) {
            return Err(self.copied_write_err(None, span));
        }
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
        // TICKET-257: the handles are `Eq` by identity (`Eq` grant rows for `Channel`, `Shared`,
        // `RwShared`, `Atomic`): `x.eq(y)` is `x == y`. No handle declares an `eq` of its own.
        if ((method == "eq" && args.len() == 1) || (method == "str" && args.is_empty()))
            && matches!(
                self.heap.get(h),
                Obj::Channel(_)
                    | Obj::Shared(_)
                    | Obj::RwShared(_)
                    | Obj::Atomic(_)
                    | Obj::AtomicInt(_)
                    | Obj::Iter { .. }
                    | Obj::Generator(_)
            )
            && let Some(v) = self.intrinsic_proto_method(recv, method, &args, span)?
        {
            self.push(v);
            return Ok(());
        }
        // Concurrency C4: `Channel` / `Shared` methods mutate the heap object in place (and `update`
        // re-enters the VM), so dispatch them directly off the handle, like the core-type methods.
        if matches!(self.heap.get(h), Obj::Channel(_)) && method != "iter" {
            self.sched_seed_point();
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
            | Obj::Channel(_)
            | Obj::Generator(_) => None,
        };
        if let Some(key) = handle_key {
            if self.try_native_bodied_method(key, method, recv, &args, argc, ic, span)? {
                return Ok(());
            }
            // Native op body per handle — identical to the retired per-arm dispatch, tails included.
            self.sched_seed_point();
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
        // map → (key, value) pairs, str → per-char str, bytes/bytearray → per-byte int). Reuses `drain_iterable`
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
                    | Obj::Channel(_)
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
            | Obj::Generator(_) => None,
        };
        if let Some(container) = container {
            let dispatched = match container {
                Container::Core => self.core_method(h, method, &args, span),
                Container::Bytes => self.bytes_method(h, method, &args, span),
                Container::ByteArray => self.bytearray_method(h, method, &args, span),
            };
            // A name miss inside a dispatcher falls to the intrinsic protocol methods through
            // `container_method_miss`; any other error is the dispatcher's own and stays as it is.
            let result = dispatched?;
            self.push(result);
            return Ok(());
        }
        self.ensure_module_faulted(h); // D1: `module.fn(...)` on a not-yet-faulted worker module
        match self.heap.get(h).clone() {
            // `module.fn(args)` — plain call on the looked-up member, no `self`.
            Obj::Module(m) => {
                let idx = self.slot_index(h, method).ok_or_else(|| {
                    self.err(
                        format!("module '{}' has no member '{method}'", m.name),
                        span,
                    )
                })?;
                let member = self.read_slot(h, idx, span)?;
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
                // TICKET-030 — every struct gets an intrinsic, MISS-ONLY `copy()`: it is reached only
                // after the declared-method lookup and the fn-typed-field fallback above both failed,
                // for the same precedence reason as the checker's arm. `fields` is already the OWNED
                // clone taken at the top of this match (`self.heap.get(h).clone()`), so allocating a
                // new `Obj::Struct` from the same `tid` and `fields` copies each field's VALUE while a
                // heap field (a `List`/`Map`/`Channel`/nested struct) is SHARED — a shallow copy.
                // `Heap::alloc` never collects (the only safepoint is the top of `run_until`'s dispatch
                // loop), so no rooting is needed between the `alloc` and the `push`.
                if method == "copy" && args.is_empty() {
                    let dup = self.heap.alloc(Obj::Struct { tid, fields });
                    self.push(Value::obj(dup));
                    return Ok(());
                }
                // ROOT REDESIGN — render the BARE display name (not the identity key) in the error.
                // Resolved BEFORE the W6-3 fallback below so the `name` borrow of `self` ends here.
                let display = self
                    .program
                    .structs
                    .get(name)
                    .map(|d| d.display_name.clone())
                    .unwrap_or_else(|| crate::base::words::bare_display(name));
                // W6-3 — a zero-field struct with no `hash` hook is intrinsically `Hashable`
                // (`proto.rs`), so `x.hash()` in an erased `[T: Hashable]` body must answer with the
                // SAME constant `declared_hash` gives it as a Map/Set key.
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
                let who = Self::enum_subject(variant_id, &enum_key, |n| format!("type {n}"));
                Err(self.err(format!("{who} has no method '{method}'"), span))
            }
            // W6-3 — a BOXED scalar (`Obj::BigInt`, and any other Obj-tagged scalar) is Obj-tagged, so
            // it never reaches the inline-scalar miss above and lands here instead: it must answer the
            // same intrinsic arith/`Hashable`/`Comparable` methods its inline twin does.
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
            | Obj::Generator(_) => {
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

    /// The proto of method `method` on the struct or enum `type_key`, and the index of
    /// the type's declaring module (its home). The one lookup `CallStatic` and `MakeMethodFunc`
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
        let ms = prog.enum_methods.get(type_key)?;
        ms.get(method)
            .map(|&p| (p, self.enum_home_module(type_key)))
    }

    /// The one fn object for `key` in this heap (TICKET-215), and the only allocator of `Obj::Func`,
    /// `Obj::Native`, `Obj::Cffi` and `Obj::Builtin`: a fn read twice, in two modules, or after a
    /// crossing is one value. A proto is keyed alone: one heap has one module view, so a user
    /// proto's home is fixed, and a synthesized proto's home is fixed too (it reads no global, or,
    /// for `json.decode[T]`, is homed in std.json by `Op::MakeFuncIn`, TICKET-226).
    pub(super) fn fn_value(&mut self, key: FnKey, make: impl FnOnce() -> Obj) -> Value {
        if let Some(&h) = self.fn_values.get(&key) {
            return Value::obj(h);
        }
        let h = self.heap.alloc(make());
        self.fn_values.insert(key, h);
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
}
