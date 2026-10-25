//! VM: the bytecode dispatch loop (`step`).
//! Split out of `src/vm/exec.rs`. `super::*` is the parent module.

use super::*;

impl Vm {
    pub(super) fn step(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        match op {
            Op::ConstInt(n) => {
                // A source literal may exceed ±2^62 (still within i64) → box it.
                let v = self.make_int(*n);
                self.push(v);
            }
            Op::ConstFloat(x) => {
                let v = self.box_float(*x);
                self.push(v);
            }
            Op::ConstStr(s) => {
                // M19 Phase 3 — intern by data pointer (stable for the program's lifetime, since the
                // literal lives in the immutable `Arc<Program>`). First push of this op allocs the
                // heap `Obj::Str`; every later push reuses the cached, GC-rooted handle — no clone,
                // no alloc. Sound because strings are immutable and there is no identity operator.
                let key = s.as_ptr() as usize;
                let h = match self.str_intern.get(&key) {
                    Some(&h) => h,
                    None => {
                        let h = self.heap.alloc(Obj::Str(s.as_str().into()));
                        self.str_intern.insert(key, h);
                        h
                    }
                };
                self.push(Value::obj(h));
            }
            Op::ConstBytes(b) => {
                // Not interned in v1 (unlike `ConstStr`): allocate a fresh `bytes` heap object per
                // push, like a list literal. Bytes literals are not a hot path.
                let h = self.heap.alloc(Obj::Bytes(b.clone()));
                self.push(Value::obj(h));
            }
            Op::True => self.push(Value::bool(true)),
            Op::False => self.push(Value::bool(false)),
            Op::Nil => self.push(Value::nil()),
            Op::Pop => {
                self.pop();
            }
            Op::StampGen(m) => {
                if let Some(h) = self.stack.last().and_then(|v| v.as_obj())
                    && let Obj::Generator(g) = self.heap.get_mut(h)
                    && matches!(
                        g.parked.get_mut(),
                        Some(Parked {
                            state: GenState::Pending(_),
                            ..
                        })
                    )
                {
                    let p = &self.program.protos[g.proto];
                    g.stamp |= m & crate::base::crossing::param_stamp(
                        p.private_slots,
                        p.deep_slots,
                        p.arity,
                    );
                }
            }
            Op::Assert { has_msg, cmp } => {
                // Reached only on the failing path: the compiler emits `Op::Assert` after a
                // `JumpIfFalse` that already consumed (and tested) `cond`, so this op always faults.
                // `msg` (if present) was evaluated lazily just before us — `msg` is only
                // evaluated when the assertion fails.
                let extra = if *has_msg { 1 } else { 0 };
                let n = self.stack.len();
                let msg = if *has_msg {
                    let m = self.stack[n - 1];
                    self.val_str(m)
                } else {
                    None
                };
                // For a comparison, the two operand VALUES sit on the stack under `msg` (duplicated
                // by the compiler before the comparison consumed the originals). Render them around
                // `cmp`'s symbol; keep them rooted on the operand stack until the last render
                // finishes, since a `str` method it runs could otherwise collect them. If either
                // render faults, drop the operand text but still return `is_assert: true` below — a
                // faulting operand must never reclassify a failed assertion as an error.
                let cmp_text = if let Some(c) = cmp {
                    let l = self.stack[n - 2 - extra];
                    let r = self.stack[n - 1 - extra];
                    let mut s = String::new();
                    let rendered = self
                        .stringify_nested_into(&mut s, l, span, 0)
                        .and_then(|()| {
                            s.push(' ');
                            s.push_str(c.symbol());
                            s.push(' ');
                            self.stringify_nested_into(&mut s, r, span, 0)
                        });
                    rendered.ok().map(|()| s)
                } else {
                    None
                };
                self.stack
                    .truncate(n - extra - if cmp.is_some() { 2 } else { 0 });
                let message = match (msg, cmp_text) {
                    (Some(m), Some(c)) => format!("assertion failed: {m} ({c})"),
                    (Some(m), None) => format!("assertion failed: {m}"),
                    (None, Some(c)) => format!("assertion failed: {c}"),
                    (None, None) => "assertion failed".to_string(),
                };
                // The ONE site that flags an assert failure — the `test fn` runner buckets this as
                // FAIL, every other (`is_assert: false`) fault as ERROR.
                return Err(RuntimeError {
                    message,
                    span,
                    is_assert: true,
                    is_over_memory: false,
                    is_timed_out: false,
                    is_deadlock: false,
                    is_panic: false,
                });
            }
            Op::GetLocal(slot) => {
                let v = self.stack[self.base() + slot];
                self.push(v);
            }
            Op::SetLocal(slot) => {
                let v = self.pop();
                let at = self.base() + slot;
                self.stack[at] = v;
            }
            Op::GetGlobalSlot(slot) => {
                let home = self.frames.last().unwrap().home;
                // TICKET-051: one module global denotes one object inside one task. A crossing
                // closure's free globals are installed into the RECEIVING view's own module copy at
                // the airlock (`from_wire_memo`'s `WireValue::Closure` arm), so a plain live read
                // always agrees with a closure's read of the same slot.
                // TICKET-246: inside a box closure the global is the caller's, through the one door.
                let door = if self.box_chain.is_empty() {
                    None
                } else {
                    self.box_global(home, Some(*slot), span)?
                };
                let v = match door {
                    Some(v) => v,
                    None => {
                        // D1: lazily reconstruct the worker's home module
                        self.ensure_module_faulted(home);
                        self.read_slot(home, *slot, span)?
                    }
                };
                self.push(v);
            }
            Op::DefineGlobalSlot(slot) => {
                let v = self.pop();
                let home = self.frames.last().unwrap().home;
                self.set_global_slot(home, *slot, v);
            }
            Op::SetGlobalSlot(slot) => {
                let v = self.pop();
                let home = self.frames.last().unwrap().home;
                // D4 layer C (TICKET-169): a spawned task's write to its module-global copy is a
                // fault, never a silent lost write (owner decision D4 supersedes DEC-137's write
                // half). The root task (not a task view) writes its globals freely.
                if self.is_task_view() {
                    self.ensure_module_faulted(home);
                    let name = self.global_slot_name(home, *slot);
                    return Err(self.copied_write_err(name.as_deref(), span));
                }
                self.set_global_slot(home, *slot, v);
            }
            Op::GetCaptured(slot) => {
                // Lever #3: hot path is a pure `captured[slot]` index — no string hash. The slot is
                // always in range (one capture per snapshot entry, populated at MakeClosure), and a
                // nested missing parent capture is stored as `Value::nil()` (byte-identical to the old
                // `get(name) -> Some(Nil)`).
                let frame = self.frames.last().unwrap();
                let (clo, home) = (frame.closure, frame.home);
                let v = clo.and_then(|h| match self.heap.get(h) {
                    Obj::Closure { captured, .. } => captured.try_get(*slot as usize),
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
                });
                match v {
                    Some(v) => self.push(v),
                    None => {
                        // Cold path: not a closure frame, or slot out of range. Recover the name from
                        // the proto's capture_names and fall back to a home global (D1 lazy fault).
                        self.ensure_module_faulted(home);
                        let proto = self.frames.last().unwrap().proto;
                        let name = self.program.protos[proto]
                            .capture_names
                            .get(*slot as usize)
                            .cloned();
                        let idx = name
                            .as_deref()
                            .and_then(|n| self.slot_index(home, n))
                            .ok_or_else(|| {
                                let label =
                                    name.clone().unwrap_or_else(|| format!("capture#{slot}"));
                                self.err(format!("undefined name '{label}'"), span)
                            })?;
                        let v = self.read_slot(home, idx, span)?;
                        self.push(v);
                    }
                }
            }
            Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod => self.arith(op, span)?,
            Op::Neg => {
                let v = self.pop();
                let r = self.neg_value(v, span)?;
                self.push(r);
            }
            Op::Not => {
                let v = self.pop();
                match v.as_bool() {
                    Some(b) => self.push(Value::bool(!b)),
                    None => {
                        return Err(
                            self.err(format!("cannot apply Not to {}", self.type_name(v)), span)
                        );
                    }
                }
            }
            Op::Lt | Op::LtEq | Op::Gt | Op::GtEq => self.compare_op(op, span)?,
            // `return`, like `Op::Contains` below: `eq_operator` may re-enter user code (a struct/enum
            // `eq`), so keeping its String/Vec temporaries out of `step`'s frame matters on the deep
            // `step → run_proto → run_until → step` path.
            Op::Eq => return self.eq_operator(false, span),
            Op::NotEq => return self.eq_operator(true, span),
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr => self.bitwise(op, span)?,
            // `return` (not `?`): keeps `step`'s frame from materializing an extra `RuntimeError`
            // temporary, which would bloat the deep re-entrant recursion path (`str(self)`-style
            // infinite recursion must hit the 10_000 call-depth limit before exhausting the host
            // stack — `self_referential_stringable_hits_depth_limit` guards exactly this).
            Op::Contains => return self.op_contains(span),
            Op::AsBool => {
                let v = *self.stack.last().unwrap();
                if v.as_bool().is_none() {
                    return Err(
                        self.err(format!("expected bool, found {}", self.type_name(v)), span)
                    );
                }
            }
            Op::AsInt => {
                let v = *self.stack.last().unwrap();
                if !self.is_integral(v) {
                    return Err(
                        self.err(format!("expected int, found {}", self.type_name(v)), span)
                    );
                }
            }
            // ----- M19 superinstructions. Bodies live in `#[inline(never)]` helpers so `step`'s own
            // stack frame stays lean. Plain calls no longer recurse the host stack (call-flattening:
            // `Op::Call` pushes a frame and the running `run_until` loop executes it), but the
            // HOF/method/deferred re-entrant path still cycles `step → run_proto → run_until → step`,
            // so a fat `step` frame would still bloat that recursion. -----
            Op::BinLocalLocal { a, b, kind } => self.op_bin_local_local(*a, *b, *kind, span)?,
            Op::BinLocalConst { slot, val, kind } => {
                self.op_bin_local_const(*slot, *val, *kind, span)?
            }
            Op::IncLocal { slot, delta } => self.op_inc_local(*slot, *delta, span)?,
            Op::PushHandler(target) => self.handlers.push(Handler {
                stack_len: self.stack.len(),
                frame_len: self.frames.len(),
                call_depth: self.call_depth,
                ip: *target,
                defer_len: self.frames.last().map(|f| f.deferred.len()).unwrap_or(0),
                markers_len: self
                    .frames
                    .last()
                    .map(|f| f.defer_markers.len())
                    .unwrap_or(0),
                nursery_len: self.nurseries.len(),
            }),
            Op::PopHandler => {
                self.handlers.pop();
            }
            // Cancellation checkpoint — lock-step with the inlined arm in `run_until`.
            Op::Jump(t) => self.jump_checked(*t, span)?,
            Op::JumpIfFalse(t) => {
                if self.pop().as_bool() == Some(false) {
                    self.jump(*t);
                }
            }
            Op::JumpIfFalseKeep(t) => {
                if self.stack.last().unwrap().as_bool() == Some(false) {
                    self.jump(*t);
                }
            }
            Op::JumpIfTrueKeep(t) => {
                if self.stack.last().unwrap().as_bool() == Some(true) {
                    self.jump(*t);
                }
            }
            Op::Call(argc) => self.do_call(*argc, span)?,
            Op::CallMethod { name, argc, ic } => self.do_method_call(name, *argc, *ic, span)?,
            Op::CallHook {
                hook,
                name,
                argc,
                ic,
            } => self.call_hook(*hook, name, *argc, *ic, span)?,
            Op::CallStatic {
                type_key,
                method,
                argc,
            } => self.do_static_call(type_key, method, *argc, span)?,
            // M24 static witness: the type key rides on top of the args as a `str`. Pop it, then
            // dispatch exactly like `CallStatic`. The compiler only emits this reading a `$w:T`
            // local it itself initialized from a `ConstStr`, so a non-`str` here is an internal
            // invariant break — surfaced as a clear runtime error, never a panic.
            Op::CallStaticDyn { method, argc } => {
                let w = self.pop();
                let Some(key) = self.val_str(w) else {
                    return Err(self.err(
                        format!("internal: static witness for '{method}' is not a type key"),
                        span,
                    ));
                };
                self.do_static_call(&key, method, *argc, span)?
            }
            Op::CallBuiltin(b, argc) => self.do_builtin(*b, *argc, span)?,
            Op::LoadBuiltin(b) => {
                let v = self.fn_value(FnKey::Builtin(*b), || Obj::Builtin(*b));
                self.push(v);
            }
            Op::CallPrint(argc) => {
                // The `Arc` clone ends the borrow of `self` the defaults would hold.
                let program = Arc::clone(&self.program);
                let [sep, end] = &program.print_defaults;
                self.do_print_sep(*argc, sep, end, span)?
            }
            // Stack on entry: `[args…, sep, end]`. Both are copied out as `str`, so they are no
            // longer GC roots while the args stringify.
            Op::CallPrintSep { argc } => {
                let end = self.pop();
                let sep = self.pop();
                let end = self.val_str(end).unwrap_or_default();
                let sep = self.val_str(sep).unwrap_or_default();
                self.do_print_sep(*argc, &sep, &end, span)?
            }
            Op::Return => self.do_return(false)?,
            Op::DeferCall(argc) => self.do_defer(None, *argc, span),
            Op::DeferMethod(name, argc) => self.do_defer(Some(name.clone()), *argc, span),
            Op::EnterDeferScope => {
                let frame = self.frames.last_mut().unwrap();
                let marker = frame.deferred.len();
                frame.defer_markers.push(marker);
            }
            Op::LeaveDeferScope => {
                if let Some(e) = self.leave_defer_scope() {
                    return Err(e);
                }
            }
            Op::DrainHandlerDefers => {
                // The live recover handler is still installed (its `PopHandler` follows). Drain the
                // block's defers down to its marker; a fault propagates and is caught by that same
                // handler (becoming the recover's `Err`).
                if let Some(marker) = self.handlers.last().map(|h| h.defer_len)
                    && let Some(e) = self.drain_frame_to(marker)
                {
                    return Err(e);
                }
            }
            Op::Try => self.do_try(span)?,
            Op::JsonDecode(desc) => {
                let desc = desc.clone();
                self.json_decode(&desc, span)?;
            }
            Op::JsonToValue => {
                self.json_to_value(span)?;
            }
            Op::NewList(n) => {
                let at = self.stack.len() - *n;
                let items = RVec::from_slice(&self.stack[at..]);
                self.stack.truncate(at);
                let h = self.heap.alloc(Obj::List(items));
                self.push(Value::obj(h));
            }
            Op::NewTuple(n) => {
                let at = self.stack.len() - *n;
                let items: Vec<Value> = self.stack.split_off(at);
                let h = self.heap.alloc(Obj::Tuple(items));
                self.push(Value::obj(h));
            }
            Op::NewMap(n) => {
                // Build an insertion-ordered hash map with last-key-wins upsert. Phase 1 hashes
                // every key while ALL operands are still rooted on the stack (a struct key's hash()
                // re-enters the VM and can GC); phase 2 dedups against the half-built LOCAL map,
                // which since M23 may dispatch a user `eq` (another re-entry) — both phases read
                // keys/values from the still-rooted stack, so the operands survive either collection.
                let count = *n;
                let at = self.stack.len() - 2 * count;
                let mut hashes = Vec::with_capacity(count);
                for j in 0..count {
                    let k = self.stack[at + 2 * j];
                    hashes.push(self.hash_value(k, span)?);
                    // Snapshot a struct/enum key (Go value-key model) and overwrite its
                    // still-rooted stack slot, so phase 2 stores the snapshot (a later mutation of
                    // the caller's original can't corrupt the map). Landing it in the rooted slot
                    // keeps it alive across the NEXT element's re-entrant `hash_value` GC; values
                    // (odd slots) are left by-reference.
                    let snap = self.snapshot_key(k);
                    self.stack[at + 2 * j] = snap;
                }
                let map = MapData::default();
                for (j, &hk) in hashes.iter().enumerate() {
                    let (k, v) = (self.stack[at + 2 * j], self.stack[at + 2 * j + 1]);
                    let pos = self.map_slot(&map.entries, map.candidates(hk), k, span)?;
                    if pos.and_then(|p| map.set_value(p, v)).is_none() {
                        map.push(&self.heap, hk, k, v);
                    }
                }
                self.stack.truncate(at);
                let h = self.heap.alloc(Obj::Map(map));
                self.push(Value::obj(h));
            }
            Op::NewSet(n) => {
                // Insertion-ordered hash set, dedup keeping first occurrence. Same two-phase rooting
                // as NewMap (elements stay rooted on the stack across BOTH the re-entrant `hash`
                // and the dedup's possibly-re-entrant `eq`).
                let count = *n;
                let at = self.stack.len() - count;
                let mut hashes = Vec::with_capacity(count);
                for j in 0..count {
                    hashes.push(self.hash_value(self.stack[at + j], span)?);
                    // Snapshot a struct/enum element (Go value-key model) into its still-
                    // rooted stack slot, so phase 2 stores the snapshot.
                    let snap = self.snapshot_key(self.stack[at + j]);
                    self.stack[at + j] = snap;
                }
                let set = SetData::default();
                for (j, &he) in hashes.iter().enumerate() {
                    let e = self.stack[at + j];
                    if self
                        .set_slot(&set.entries, set.candidates(he), e, span)?
                        .is_none()
                    {
                        set.push(&self.heap, he, e);
                    }
                }
                self.stack.truncate(at);
                let h = self.heap.alloc(Obj::Set(set));
                self.push(Value::obj(h));
            }
            Op::NewStruct(name, argc) => self.new_struct(name, *argc, span)?,
            // ----- cells (uniform by-reference capture, Task A — unwired) -----
            Op::NewCell => {
                // `v` moves straight into the `Obj` (no rooting window — alloc never GCs mid-op).
                let v = self.pop();
                let h = self.heap.alloc(Obj::Cell(v));
                self.push(Value::obj(h));
            }
            Op::CellLoad => {
                let ch = self.pop();
                let Some(h) = ch.as_obj() else {
                    unreachable!("CellLoad on a non-handle value");
                };
                let Obj::Cell(v) = self.heap.get(h) else {
                    unreachable!("CellLoad on a non-cell object");
                };
                let v = *v;
                self.push(v);
            }
            Op::CellStore => {
                // HARD contract: pop the HANDLE first, then the value (operands are `[val, handle]`).
                let ch = self.pop();
                let v = self.pop();
                let Some(h) = ch.as_obj() else {
                    unreachable!("CellStore on a non-handle value");
                };
                // D4 layer C (TICKET-169): a captured local reassigned inside a spawned task is a
                // fault, never a silent lost write.
                if self.heap.is_copied(h) {
                    return Err(self.copied_write_err(None, span));
                }
                if let Obj::Cell(slot) = self.heap.get_mut(h) {
                    *slot = v;
                } else {
                    unreachable!("CellStore on a non-cell object");
                }
            }
            Op::NewEnum {
                variant,
                variant_id,
                argc,
            } => self.new_enum(variant, *variant_id, *argc, span)?,
            Op::MakeFunc(proto) => {
                let home = self.frames.last().unwrap().home;
                let proto = *proto;
                let v = self.fn_value(FnKey::Proto(proto), || Obj::Func { proto, home });
                self.push(v);
            }
            // A default-argument provider declared in a module this one does not import — see
            // [`Op::MakeFuncIn`]. `home` comes from the DEFINER's module index, not this frame's, so
            // the default resolves in its own namespace with no import edge.
            //
            // `.get()` rather than `[]`: `module_objs` grows as modules RUN (and a worker's copy holds
            // only the modules that had run when it was snapshotted), so a not-yet-initialized definer
            // must be a clean `RuntimeError`, never an index panic on a pool thread.
            // The module's globals themselves fault in lazily — the provider's own `GetGlobalSlot`
            // calls `ensure_module_faulted` — so nothing is forced here.
            // Callee-side default fill: the argument WAS supplied, so skip the compiled default.
            // Falling through runs it and stores it into the slot.
            Op::JumpIfProvided(slot, target) => {
                if self.frames.last().unwrap().argc > *slot as usize {
                    self.jump(*target);
                }
            }
            Op::MakeFuncIn(id) => {
                let (proto, midx) = self.program.providers[*id as usize];
                let Some(&home) = self.module_objs.get(midx) else {
                    return Err(self.err(
                        format!(
                            "the module that declares {} has not been initialized yet",
                            crate::base::provider_name::display_fn_name(
                                &self.program.protos[proto].name
                            )
                        ),
                        span,
                    ));
                };
                let v = self.fn_value(FnKey::Proto(proto), || Obj::Func { proto, home });
                self.push(v);
            }
            // A type path's method read as a value (TICKET-204): the proto and home `CallStatic`
            // would use, resolved at run time because the type's module may compile after this one.
            Op::MakeMethodFunc { type_key, method } => {
                let Some((proto, midx)) = self.type_method_proto(type_key, method) else {
                    return Err(self.err(
                        format!(
                            "internal: no method '{method}' on type {}",
                            crate::base::words::bare_display(type_key)
                        ),
                        span,
                    ));
                };
                let Some(&home) = self.module_objs.get(midx) else {
                    return Err(self.err(
                        format!(
                            "the module that declares {} has not been initialized yet",
                            crate::base::words::bare_display(type_key)
                        ),
                        span,
                    ));
                };
                let v = self.fn_value(FnKey::Proto(proto), || Obj::Func { proto, home });
                self.push(v);
            }
            // Body in an `#[inline(never)]` helper so `step`'s frame stays small (the deep-recursion
            // depth-guard test overflows in debug if `step` grows — same discipline as `ToStrFmt`).
            Op::MakeCffi(id) => self.op_make_cffi(*id, span)?,
            Op::MakeClosure(proto, entries) => {
                // Lever #3: build the captured env *positionally* — slot i is the i-th entry (the
                // snapshot order the child proto's `capture_names` mirrors). A nested capture reads
                // the enclosing closure's value by its positional `parent_slot`.
                let frame = self.frames.last().unwrap();
                let (base, home, enclosing) = (frame.base, frame.home, frame.closure);
                let captured: RVec<Value> = entries
                    .iter()
                    .map(|e| match e.src {
                        CapSrc::Slot(i) => self.stack[base + i],
                        CapSrc::Captured(parent_slot) => enclosing
                            .and_then(|h| match self.heap.get(h) {
                                Obj::Closure { captured, .. } => {
                                    captured.try_get(parent_slot as usize)
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
                                | Obj::Struct { .. }
                                | Obj::Enum { .. }
                                | Obj::Cell(_)
                                | Obj::Func { .. }
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
                            })
                            .unwrap_or(Value::nil()),
                    })
                    .collect();
                let h = self.heap.alloc(Obj::Closure {
                    proto: *proto,
                    captured,
                    home,
                });
                self.push(Value::obj(h));
            }
            Op::GetField { name, ic } => self.get_field(name, *ic, span)?,
            Op::GetIndex => self.get_index(span)?,
            Op::SliceRange => self.slice_range(span)?,
            Op::SetField { name, ic } => self.set_field(name, *ic, span)?,
            Op::SetIndex => self.set_index(span)?,
            Op::Dup => {
                let top = *self.stack.last().expect("Dup on empty stack");
                self.push(top);
            }
            Op::Dup2 => {
                let n = self.stack.len();
                let a = self.stack[n - 2];
                let b = self.stack[n - 1];
                self.push(a);
                self.push(b);
            }
            Op::ToStr => {
                let v = self.stack[self.stack.len() - 1]; // leave rooted; stringify may run user code
                let s = self.stringify(v, span, 0)?;
                self.pop();
                let h = self.heap.alloc(Obj::Str(s.into()));
                self.push(Value::obj(h));
            }
            // Body in an `#[inline(never)]` helper so `step`'s frame stays small (the deep-recursion
            // depth-guard test overflows in debug if `step` grows — see commit 1450077).
            Op::ToStrFmt(spec) => self.op_to_str_fmt(spec, span)?,
            Op::BuildStr(n) => {
                let at = self.stack.len() - *n;
                // Stringify in place so each interpolated part stays rooted while a `str` method runs.
                let mut s = String::new();
                for i in 0..*n {
                    let p = self.stack[at + i];
                    self.stringify_into(&mut s, p, span, 0)?; // one buffer, no per-part String
                }
                self.stack.truncate(at);
                let h = self.heap.alloc(Obj::Str(s.into()));
                self.push(Value::obj(h));
            }
            Op::ListClone => {
                // Snapshot a `for` iterand into an index-iterable list (a body that mutates the
                // source does not disturb iteration). `builtin_items` holds the rows.
                let v = self.pop();
                let Some(items) = v
                    .as_obj()
                    .and_then(|h| self.builtin_items::<RVec<Value>>(h))
                else {
                    return Err(
                        self.err(format!("cannot iterate over {}", self.type_name(v)), span)
                    );
                };
                let nh = self.heap.alloc(Obj::List(items));
                self.push(Value::obj(nh));
            }
            Op::ArrLen => {
                let v = self.pop();
                let len = match v.view() {
                    ValueView::Obj(h) => match self.heap.get(h).as_list() {
                        Some(items) => items.len() as i64,
                        None => unreachable!("ArrLen on non-list"),
                    },
                    ValueView::Int(_) | ValueView::Bool(_) | ValueView::Nil => {
                        unreachable!("ArrLen on non-list")
                    }
                };
                self.push(Value::int(len));
            }
            Op::HasNextHook => {
                let v = self.pop();
                let has = self.hook_of(v, crate::base::hooks::Hook::Next);
                self.push(Value::bool(has));
            }
            Op::IsGenerator => {
                let v = self.pop();
                let is_gen =
                    matches!(v.as_obj(), Some(h) if matches!(self.heap.get(h), Obj::Generator(_)));
                self.push(Value::bool(is_gen));
            }
            Op::IsCursor => {
                let v = self.pop();
                let is_cursor =
                    matches!(v.as_obj(), Some(h) if matches!(self.heap.get(h), Obj::Iter { .. }));
                self.push(Value::bool(is_cursor));
            }
            Op::IterableToCursor => {
                // One-time `for`-entry conversion: a PURE-`Iterable` struct (has `iter`, lacks `next`)
                // becomes its cursor (so the seq path drains it); everything else passes through.
                // Shared with `drain_iterable` (`List()`/`Set()`/`Map()`/`.iter()`) so every consumer
                // of the checker's `Iterable` admission set accepts the same witnesses.
                let v = self.pop();
                let cursor = self.iterable_to_cursor(v, span)?;
                self.push(cursor);
            }
            Op::Yield => {
                // Experimental generator suspend. The yielded value is already on the stack top; flag
                // the request and let `run_until` return to the host `.next()` after this op (the
                // frame `ip` has already advanced past the `Yield`, so resume continues after it).
                self.gen_yielding = true;
            }
            Op::IsChannel => {
                let v = self.pop();
                let is_chan =
                    matches!(v.as_obj(), Some(h) if matches!(self.heap.get(h), Obj::Channel(_)));
                self.push(Value::bool(is_chan));
            }
            Op::ChanRecvOrClosed => {
                self.sched_seed_point();
                // `for v in ch:` step: pop a value (parking on empty-open exactly like `recv`) and push
                // `Some(v)`, or push `None` once the channel is closed-and-drained (the loop's clean
                // exit). It DOES run inside a native re-entry when the loop sits in a generator body,
                // a callback or a `defer` (TICKET-136, W14-16), so it goes through
                // `recv_step_or_demote`: an M:N fiber there demotes instead of faulting a false
                // deadlock. Outside a re-entry it takes the snapshot-park / block-in-place path.
                let v = self.pop();
                let Some(h) = v.as_obj() else {
                    return Err(self.err("`for` over a non-channel value".to_string(), span));
                };
                match self.recv_step_or_demote(h, span)? {
                    // `recv_step_or_demote` woke a sender for `Got`; a filled slot committed none.
                    RecvStep::Got(w) | RecvStep::Filled(w) => {
                        let val = self.from_wire(w);
                        let opt = self.alloc_enum("Option", "Some", &[val]);
                        self.push(opt);
                    }
                    RecvStep::ClosedEmpty => {
                        let opt = self.alloc_enum("Option", "None", &[]);
                        self.push(opt);
                    }
                    // `chan_recv_step` re-rooted the handle + set `suspend`; `run_until`'s `paused()`
                    // gate returns to the scheduler, and the op re-runs (rewound `ip`) on resume.
                    RecvStep::Parked => {}
                }
            }
            Op::EnsureEnum(slot) => {
                let v = self.stack[self.base() + *slot];
                if !matches!(v.as_obj(), Some(h) if matches!(self.heap.get(h), Obj::Enum { .. })) {
                    return Err(self.err(format!("cannot match on {}", self.type_name(v)), span));
                }
            }
            Op::MatchArm {
                scrut,
                variant,
                variant_id,
                enum_name,
                nbind,
                bind_start,
                next,
            } => self.match_arm(
                *scrut,
                variant,
                *variant_id,
                enum_name.as_deref(),
                *nbind,
                *bind_start,
                *next,
                span,
            )?,
            Op::MatchNoArm(slot) => {
                let v = self.stack[self.base() + *slot];
                let id = match v.view() {
                    ValueView::Obj(h) => self.heap.get(h).as_enum().map(|(id, _)| id),
                    ValueView::Int(_) | ValueView::Bool(_) | ValueView::Nil => None,
                };
                // A carrier is named by the pattern that would match it, a user variant by name.
                let message = match id.and_then(Self::carrier_pattern_text) {
                    Some(pattern) => format!("no match arm for pattern '{pattern}'"),
                    None => {
                        let variant = id.map_or("", |id| self.enum_names(id).1);
                        format!("no match arm for variant '{variant}'")
                    }
                };
                return Err(self.err(message, span));
            }
            Op::EnterNursery => {
                self.sched_seed_point();
                self.op_enter_nursery(span)
            }
            Op::JoinNursery => {
                self.sched_seed_point();
                self.join_nursery_released()?;
                self.cancel_at_join(span)?;
            }
            // TASK B — `break`/`continue` leaving a `parallel:` scope: cancel its
            // tasks and pop exactly that one level (the compiler emits one per escaped scope).
            Op::ReclaimNursery => {
                let from = self.nurseries.len().saturating_sub(1);
                // TICKET-132 — park the owner instead of waiting inline if `from`'s level is a
                // fiber-owned escape; the rewound op re-runs once the cancelled family settles.
                if !self.park_escaped_abort(from)
                    && let Some(e) = self.drain_escaped_nursery(from)
                {
                    // TICKET-147 — the aborted nursery's child fault is a real fault.
                    return Err(e);
                }
            }
            Op::SpawnCall(argc, fresh, entry) => {
                self.do_spawn(super::sched::SpawnHead::Callee(*entry), *argc, *fresh, span)?
            }
            Op::SpawnRecv(argc, fresh, entry) => self.do_spawn(
                super::sched::SpawnHead::Receiver(*entry),
                *argc,
                *fresh,
                span,
            )?,
            Op::SpawnBlock(proto, entries) => self.do_spawn_block(*proto, entries, span)?,
            Op::WaitPoll(meta) => {
                self.sched_seed_point();
                self.op_wait_poll(meta, span)?
            }
            Op::NewChannel(has_cap) => {
                let cap = if *has_cap {
                    let cap_v = self.pop();
                    let n = self.int_of(cap_v);
                    if n < 0 {
                        return Err(self.err(
                            "Channel capacity must be >= 0 (0 = a rendezvous channel; omit the argument for an unbounded channel)"
                                .to_string(),
                            span,
                        ));
                    }
                    Some(n as usize)
                } else {
                    None
                };
                let h = self.heap.alloc(Obj::Channel(Arc::new(ChannelCore {
                    cap,
                    ..Default::default()
                })));
                self.push(Value::obj(h));
            }
            Op::NewShared => {
                let init = self.pop();
                // The box holds the wire form (single serialization == the old deep_clone-in). A
                // non-sendable init (a frame-holding generator / module/native/FFI handle) faults
                // gracefully with this Op's span — the box is a shared cross-thread cell.
                let init = self.to_wire_crossable(init, span)?;
                // TICKET-246: the value is decoded into the box's own heap and lives there.
                let core = self.new_shared_core(init);
                let h = self.heap.alloc(Obj::Shared(core));
                self.push(Value::obj(h));
            }
            Op::NewRwShared => {
                let init = self.pop();
                // The box holds the wire form (single serialization == the old deep_clone-in). A
                // non-sendable init (a frame-holding generator / module/native/FFI handle) faults
                // gracefully with this Op's span — the box is a shared cross-thread cell.
                // TICKET-154: an `RwShared` store serializes like every other cross-heap store (one object per alias).
                let init = self.to_wire_crossable(init, span)?;
                let core = self.new_rwshared_core(init);
                let h = self.heap.alloc(Obj::RwShared(core));
                self.push(Value::obj(h));
            }
            // `NewAtomic`/`NewAtomicInt` delegate to `#[inline(never)]` helpers so their locals do NOT
            // inflate `step`'s stack frame — `step` is on the per-op
            // recursion path, so a fatter frame here multiplies across a deep call chain (debug builds
            // don't reuse match-arm stack slots) and can overflow the host stack before the
            // `MAX_CALL_DEPTH` guard fires. Keep these cold constructors out of line.
            Op::NewAtomic => {
                let v = self.new_atomic(span)?;
                self.push(v);
            }
            Op::NewAtomicInt => {
                let v = self.new_atomic_int(span)?;
                self.push(v);
            }
            Op::NewExecutor => {
                let v = self.new_executor(0, span);
                self.push(v);
            }
            Op::NewExecutorN => {
                let v = self.pop();
                let n = self.int_val(v).unwrap_or(0);
                if n < 1 {
                    return Err(self.err("Executor(n) needs n >= 1".to_string(), span));
                }
                let v = self.new_executor(n as usize, span);
                self.push(v);
            }
        }
        Ok(())
    }
}
