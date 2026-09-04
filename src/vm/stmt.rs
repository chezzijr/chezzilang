// vm::stmt — split out of vm/mod.rs. `super::*` == the `vm` module.
// Statement exec: try/defer unwind, struct/enum ctor, index/field, print, builtins, display/stringify.

use super::*;

impl Vm {
    /// Drain the current (top) frame's deferred calls, LIFO, popping one at a time from the frame's
    /// own list so the not-yet-run records stay GC-rooted in the frame. Skipped on a hard
    /// `std.os.exit` (Go: `os.Exit` does not run deferred calls). Returns the latest fault, if any.
    pub(super) fn drain_top_frame_deferred(&mut self) -> Option<RuntimeError> {
        if self.pending_exit.is_some() {
            return None;
        }
        let fi = self.frames.len() - 1;
        let mut err = None;
        while let Some(d) = self.frames[fi].deferred.pop() {
            if let Err(e) = self.run_one_deferred(d) {
                err = Some(e);
                if self.pending_exit.is_some() {
                    break;
                }
            }
        }
        err
    }

    /// Leave a lexical defer scope (`LeaveDeferScope`): pop the top marker and run the current
    /// frame's defers registered since it, LIFO. This is the block-scoped analogue of
    /// `drain_top_frame_deferred` — it drains down to a marker, not to the bottom of the frame.
    /// Skipped on a hard `std.os.exit`. Returns the latest fault from a deferred call, if any.
    pub(super) fn leave_defer_scope(&mut self) -> Option<RuntimeError> {
        let fi = self.frames.len() - 1;
        debug_assert!(
            !self.frames[fi].defer_markers.is_empty(),
            "LeaveDeferScope without a matching EnterDeferScope (compiler scope-count desync)"
        );
        let marker = self.frames[fi].defer_markers.pop().unwrap_or(0);
        self.drain_frame_to(marker)
    }

    /// Drain the current (top) frame's pending defers down to `marker` (the count to leave behind),
    /// LIFO. The block-scoped analogue of `drain_top_frame_deferred` for an explicit marker — used
    /// by `LeaveDeferScope` and by every `recover:` boundary path. Skipped on a hard `std.os.exit`.
    /// Returns the latest fault from a deferred call, if any.
    pub(super) fn drain_frame_to(&mut self, marker: usize) -> Option<RuntimeError> {
        if self.pending_exit.is_some() {
            return None;
        }
        let fi = self.frames.len() - 1;
        let mut err = None;
        while self.frames[fi].deferred.len() > marker {
            let d = self.frames[fi].deferred.pop().unwrap();
            if let Err(e) = self.run_one_deferred(d) {
                err = Some(e);
                if self.pending_exit.is_some() {
                    break;
                }
            }
        }
        err
    }

    /// Unwind frames from the current depth down to `target_frame_len`, running each discarded
    /// frame's deferred calls (innermost first) before dropping it. Used on a fault: deferred
    /// cleanup runs as the stack unwinds, before a `recover:` boundary regains control (or before
    /// the program exits on an uncaught fault). A fault in a deferred call supersedes the original.
    ///
    /// `report_escaped` — a genuine fault (not a B3.4 cancel / `std.os.exit`) cancels
    /// each discarded frame's escaped nurseries (its implicit nursery + any inner `parallel:` the
    /// fault unwound past) BEFORE that frame's `defer`s run — matching the interp oracle, which
    /// reports in `exec_parallel` / `leave_implicit_nursery` as the body unwinds and only then runs
    /// `finish_frame`'s defers. The MODULE top-level nursery is preserved (it joins only on a clean
    /// run to program end; an uncaught top-level fault leaves it silent, as in the interp).
    pub(super) fn unwind_deferred(
        &mut self,
        target_frame_len: usize,
        report_escaped: bool,
    ) -> Option<RuntimeError> {
        let mut err = None;
        while self.frames.len() > target_frame_len {
            let fi = self.frames.len() - 1;
            // Report this frame's escaped nurseries BEFORE its defers (drain pops innermost-first, so
            // inner `parallel:` levels report before the frame's implicit one — the interp's order).
            if report_escaped {
                let f = &self.frames[fi];
                let floor = if f.is_toplevel && f.has_implicit_nursery {
                    f.nursery_len + 1 // preserve the module nursery
                } else {
                    f.nursery_len
                };
                self.drain_escaped_nursery(floor.min(self.nurseries.len()));
            }
            if self.pending_exit.is_none() {
                while let Some(d) = self.frames[fi].deferred.pop() {
                    if let Err(e) = self.run_one_deferred(d) {
                        err = Some(e);
                        if self.pending_exit.is_some() {
                            break;
                        }
                    }
                }
            }
            let frame = self.frames.pop().unwrap();
            if frame.counted {
                self.call_depth -= 1;
            }
            self.stack.truncate(frame.base);
            self.cur_base = self.frames.last().map(|f| f.base).unwrap_or(0);
            while self
                .handlers
                .last()
                .is_some_and(|h| h.frame_len > self.frames.len())
            {
                self.handlers.pop();
            }
        }
        err
    }

    pub(super) fn do_try(&mut self, span: Span) -> Result<(), RuntimeError> {
        let v = self.pop();
        // Extract (variant_id, payload-arity, first-payload) up front so the heap borrow is released
        // before we mutate the stack / unwind a frame.
        let info = match v.view() {
            ValueView::Obj(h) => match self.heap.get(h) {
                Obj::Enum {
                    variant_id,
                    payload,
                } => Some((*variant_id, payload.len(), payload.first().copied())),
                _ => None,
            },
            _ => None,
        };
        // M19 lever #2 — gate on the fixed native variant ids (`VID_OK`/`VID_SOME` unwrap, `VID_ERR`/
        // `VID_NONE_VARIANT` propagate), NOT a name compare. A user enum shadowing `Ok`/`Err`/`Some`/
        // `None` gets distinct ids, so it is correctly NOT treated as a Result/Option by `?`.
        use crate::vm::op::{VID_ERR, VID_NONE_VARIANT, VID_OK, VID_SOME};
        if let Some((variant_id, n, first)) = info {
            if (variant_id == VID_OK || variant_id == VID_SOME) && n == 1 {
                self.push(first.unwrap());
                return Ok(());
            }
            if variant_id == VID_ERR || variant_id == VID_NONE_VARIANT {
                // A `?` directly inside a `recover:` block (a handler installed in THIS frame)
                // short-circuits to that boundary (try-block style): the `Err`/`None` value becomes
                // the recover's result. Function-scoped `?` (no same-frame handler) falls through.
                let frame_len = self.frames.len();
                if let Some(h) = self.handlers.pop_if(|h| h.frame_len == frame_len) {
                    self.stack.truncate(h.stack_len);
                    self.call_depth = h.call_depth;
                    // Drop scope markers of defer scopes opened inside the recover block — the `?`
                    // jumps past their `LeaveDeferScope`s, so they would otherwise leak.
                    self.frames
                        .last_mut()
                        .unwrap()
                        .defer_markers
                        .truncate(h.markers_len);
                    // TASK B — a recover-scoped `?` jumps past the `JoinNursery` of any `parallel:`
                    // opened inside the recover block: cancel its tasks HERE
                    // (before the handler binds its result and execution continues), so a recover-caught
                    // `?` reports IDENTICALLY-AND-AS-EARLY as an uncaught one — matching the interp,
                    // whose `exec_parallel` reports during the `?` unwind, before the recover's value is
                    // produced. Without this the nursery lingered until the whole frame returned (the
                    // report then trailed `print("recovered")`, an interp/VM divergence).
                    //
                    // ORDERING (matches the interp oracle): the escaped `parallel:` BODY's own defers
                    // must run BEFORE the nursery reclaim, and the recover block's defers AFTER it —
                    // because in the interp the body is its own `exec_scoped_block` whose defers drain
                    // as the `?` unwinds out of the body, and only then does `exec_parallel` report;
                    // the recover block's defers run later, at the recover boundary. So: drain the
                    // body defers down to the outermost escaped nursery's floor, report, then drain the
                    // remaining (recover-block) defers down to the handler's install-time floor. A body
                    // defer fault is held and superseded by any later recover-block defer fault.
                    let mut body_defer_err = if self.nurseries.len() > h.nursery_len {
                        let floor = self.nursery_defer_floors[h.nursery_len];
                        self.drain_frame_to(floor)
                    } else {
                        None
                    };
                    if self.pending_exit.is_some()
                        && let Some(e) = body_defer_err.take()
                    {
                        return Err(e);
                    }
                    self.drain_escaped_nursery(h.nursery_len);
                    // Drain the recover block's own defers before binding the result. A fault in one
                    // supersedes the propagated value (becomes the recover's `Err`); a recover-block
                    // defer fault in turn supersedes a body defer fault (it unwinds later).
                    match self.drain_frame_to(h.defer_len) {
                        Some(e) if self.pending_exit.is_some() => return Err(e),
                        Some(e) => {
                            let sp = e.span;
                            let msg = self.alloc_str(e.message);
                            self.stamp_err_span(msg, sp);
                            let err = self.alloc_enum("Result", "Err", vec![msg]);
                            self.push(err);
                        }
                        None => match body_defer_err {
                            // No recover-block defer fault, but a parallel-body defer faulted: that
                            // becomes the recover's `Err` (Go semantics — a defer fault supersedes).
                            Some(e) => {
                                let sp = e.span;
                                let msg = self.alloc_str(e.message);
                                self.stamp_err_span(msg, sp);
                                let err = self.alloc_enum("Result", "Err", vec![msg]);
                                self.push(err);
                            }
                            None => self.push(v), // the propagated Result/Option value IS the result
                        },
                    }
                    self.jump(h.ip);
                    return Ok(());
                }
                // A `?` at the top level (no enclosing function) is an unhandled error → exit. Use
                // the `?` op's own `span` so the reported location matches the interp (which threads
                // the `?`'s `expr.span` through its propagation marker).
                if self.frames.last().unwrap().is_toplevel {
                    return Err(self.top_level_error(v, span).unwrap_or_else(|| {
                        self.err(format!("unhandled error: {}", self.display(v)), span)
                    }));
                }
                // Otherwise early-return this value from the enclosing function (running its
                // deferred calls first; a fault in one propagates as a fault).
                self.push(v);
                self.do_return(true)?;
                return Ok(());
            }
        }
        Err(self.err(
            format!("'?' expects Result or Option, found {}", self.type_name(v)),
            span,
        ))
    }

    // ----- construction / access -----

    pub(super) fn new_struct(
        &mut self,
        name: &str,
        argc: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let def = self
            .program
            .structs
            .get(name)
            .cloned()
            .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        if argc != def.fields.len() {
            return Err(self.err(
                format!(
                    "struct '{}' expects {} field(s), got {argc}",
                    def.display_name,
                    def.fields.len()
                ),
                span,
            ));
        }
        let at = self.stack.len() - argc;
        // Positional layout: the args already arrive in declaration order (desugar reorders any
        // named-field constructor before codegen), so split them straight in — no per-field name
        // strings, no zip with `def.fields`. `argc == def.fields.len()` is checked above.
        let fields: Vec<Value> = self.stack.split_off(at);
        let h = self.heap.alloc(Obj::Struct {
            tid: def.tid,
            fields: Fields::from_vec(fields),
        });
        self.push(Value::obj(h));
        Ok(())
    }

    /// The dense layout id for a struct type `name`, or [`TID_NONE`] if it isn't a registered type
    /// (native/ad-hoc structs) — such a struct never IC-caches, so it stays sound on the probe path.
    pub(super) fn struct_tid(&self, name: &str) -> u32 {
        self.program.structs.get(name).map_or(TID_NONE, |d| d.tid)
    }

    /// M19 memory-layout lever — resolve a struct `tid` back to its IDENTITY KEY (the `structs` map
    /// key, e.g. `<main>::Point`) on the cold path (method dispatch / Display / arith / hash / wire /
    /// snap), where `Obj::Struct` no longer carries a per-instance `name`. O(1) via `struct_names`.
    /// Returns `"?"` for [`crate::vm::op::TID_NONE`] / an out-of-range id (defensive — every
    /// source-constructed struct has a registered `def.tid`, so this is unreachable for Display).
    pub(super) fn struct_name_of_tid(&self, tid: u32) -> &str {
        self.program
            .struct_names
            .get(tid as usize)
            .map_or("?", |s| s)
    }

    /// M19 lever #2 — resolve a `variant_id` back to its `(enum-type, variant)` names on the COLD path
    /// (Display / stringify / error / wire / snap), where the instance no longer carries the strings.
    /// O(1) via `Program::variants_by_id`. Returns `("?", "?")` for [`crate::vm::op::VID_NONE`] / an
    /// out-of-range id (defensive — a registered enum always resolves).
    pub(super) fn enum_names(&self, variant_id: u32) -> (&str, &str) {
        self.program
            .variants_by_id
            .get(variant_id as usize)
            .map_or(("?", "?"), |d| (d.enum_name.as_str(), d.name.as_str()))
    }

    /// The index of the module that declared the enum keyed by `enum_key` (its method bodies resolve
    /// top-level names against that module's globals). Defaults to module 0 if unrecorded.
    pub(super) fn enum_home_module(&self, enum_key: &str) -> usize {
        self.program.enum_home.get(enum_key).copied().unwrap_or(0)
    }

    /// The index of the module that declared the newtype keyed by `key` (home-globals for its
    /// methods). Mirrors [`enum_home_module`]. Defaults to module 0 if unrecorded.
    pub(super) fn newtype_home_module(&self, key: &str) -> usize {
        self.program.newtype_home.get(key).copied().unwrap_or(0)
    }

    /// Construct an enum from `Op::NewEnum`. M19 lever #2 — the dense `variant_id` is baked into the op
    /// at compile time (no runtime hash lookup); it is stamped onto the instance instead of the two
    /// per-instance type/variant `Box<str>`s. `variant` is used only for the arity-mismatch message.
    pub(super) fn new_enum(
        &mut self,
        variant: &str,
        variant_id: u32,
        argc: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if let Some(def) = self.program.variants_by_id.get(variant_id as usize)
            && argc != def.arity
        {
            return Err(self.err(
                format!(
                    "variant '{variant}' expects {} value(s), got {argc}",
                    def.arity
                ),
                span,
            ));
        }
        let at = self.stack.len() - argc;
        let payload: Vec<Value> = self.stack.split_off(at);
        let h = self.heap.alloc(Obj::Enum {
            variant_id,
            payload,
        });
        self.push(Value::obj(h));
        Ok(())
    }

    pub(super) fn get_field(
        &mut self,
        name: &str,
        ic: u32,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let obj = self.pop();
        let Some(h) = obj.as_obj() else {
            return Err(self.err(
                format!("cannot read field '{name}' of {}", self.type_name(obj)),
                span,
            ));
        };
        self.ensure_module_faulted(h); // D1: `module.member` on a not-yet-faulted worker module
        // M19 Phase 5b — inline-cache fast path: a hit collapses the struct name-probe to one pure-int
        // `tid` compare (the struct's layout id). Same `tid` ⇒ same field order ⇒ the cached `idx` is
        // the right slot, so the field-name re-verify is unnecessary. `cell.tid == TID_NONE` (empty or
        // an unregistered struct) never matches, forcing the probe below. `fields.get` stays bounds-
        // safe (defensive; same `tid` guarantees in-range). Worst case on a miss: a re-probe + refill.
        if ic != NO_IC {
            let cell = self.field_ic[ic as usize];
            if cell.tid != TID_NONE
                && let Obj::Struct { tid, fields, .. } = self.heap.get(h)
                && *tid == cell.tid
                && let Some(v) = fields.get(cell.idx as usize)
            {
                let v = *v;
                self.push(v);
                return Ok(());
            }
        }
        match self.heap.get(h) {
            // `t.0`, `t.1`, … — tuple element access. The field name is the element index.
            Obj::Tuple(items) => {
                let v = name
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| items.get(i).copied());
                match v {
                    Some(v) => {
                        self.push(v);
                        Ok(())
                    }
                    None => Err(self.err(
                        format!("tuple has no element '.{name}' (len {})", items.len()),
                        span,
                    )),
                }
            }
            Obj::Struct { tid, fields, .. } => {
                // Positional layout: the field name->index map lives in the StructDef (declaration
                // order), not the instance. Resolve the slot there, then index the flat `fields`
                // Vec. Capture both index + layout `tid` so the IC can cache them (Value is Copy,
                // `tid` is a `u32`, so the heap borrow ends here, freeing `self` for the write).
                let tid = *tid;
                let sname = self.struct_name_of_tid(tid);
                let idx = self
                    .program
                    .structs
                    .get(sname)
                    .and_then(|d| d.fields.iter().position(|f| f == name));
                let found = idx.and_then(|i| fields.get(i).map(|v| (i, *v)));
                match found {
                    Some((i, v)) => {
                        if ic != NO_IC {
                            self.field_ic[ic as usize] = IcCell { idx: i as u32, tid };
                        }
                        self.push(v);
                        Ok(())
                    }
                    None => {
                        let shown = self.display(obj);
                        Err(self.err(format!("no field '{name}' on {shown}"), span))
                    }
                }
            }
            Obj::Module(m) => match m.index.get(name).map(|&i| m.slots[i as usize]) {
                Some(v) => {
                    self.push(v);
                    Ok(())
                }
                None => Err(self.err(format!("module '{}' has no member '{name}'", m.name), span)),
            },
            _ => Err(self.err(
                format!("cannot read field '{name}' of {}", self.type_name(obj)),
                span,
            )),
        }
    }

    /// `obj[start:end:step]` — Python-style slice copy of a list/str, or a struct's `slice`. Each
    /// component arrives as `Nil` (omitted → `None`) or `Int`; the shared `slice::slice_indices`
    /// resolver owns all the clamp/step/reverse math.
    pub(super) fn get_slice(&mut self, span: Span) -> Result<(), RuntimeError> {
        let step = self.pop();
        let end = self.pop();
        let start = self.pop();
        let obj = self.pop();
        // Each component is `Nil` (omitted) → `None`, or an `Int` → `Some`. Anything else faults.
        let comp = |vm: &Vm, v: Value| -> Result<Option<i64>, RuntimeError> {
            if v.is_nil() {
                Ok(None)
            } else if let Some(n) = vm.int_val(v) {
                Ok(Some(n))
            } else {
                Err(vm.err(format!("expected int, found {}", vm.type_name(v)), span))
            }
        };
        let s = comp(self, start)?;
        let e = comp(self, end)?;
        let st = comp(self, step)?;
        let Some(h) = obj.as_obj() else {
            return Err(self.err(format!("cannot slice {}", self.type_name(obj)), span));
        };
        // Snapshot the result kind without holding the heap borrow across the alloc / method call.
        enum Sliced {
            List(Vec<Value>),
            Str(String),
            Bytes(Vec<u8>),
            ByteArray(Vec<u8>),
            Struct,
        }
        let sliced = match self.heap.get(h) {
            Obj::List(items) => {
                let idxs = crate::slice::slice_indices(s, e, st, items.len())
                    .map_err(|m| self.err(m.to_string(), span))?;
                Sliced::List(idxs.iter().map(|&i| items[i]).collect())
            }
            Obj::Str(string) => {
                let chars: Vec<char> = string.chars().collect();
                let idxs = crate::slice::slice_indices(s, e, st, chars.len())
                    .map_err(|m| self.err(m.to_string(), span))?;
                Sliced::Str(idxs.iter().map(|&i| chars[i]).collect())
            }
            // `bytes[a:b:c]` slices over BYTE offsets and yields a new `bytes` (open bounds / step /
            // reverse / negative all via the shared `slice_indices`, exactly like list/str).
            Obj::Bytes(b) => {
                let idxs = crate::slice::slice_indices(s, e, st, b.len())
                    .map_err(|m| self.err(m.to_string(), span))?;
                Sliced::Bytes(idxs.iter().map(|&i| b[i]).collect())
            }
            // `bytearray[a:b:c]` slices over BYTE offsets and yields a NEW `bytearray` (mutable copy).
            Obj::ByteArray(b) => {
                let idxs = crate::slice::slice_indices(s, e, st, b.len())
                    .map_err(|m| self.err(m.to_string(), span))?;
                Sliced::ByteArray(idxs.iter().map(|&i| b[i]).collect())
            }
            Obj::Struct { .. } => Sliced::Struct,
            _ => return Err(self.err(format!("cannot slice {}", self.type_name(obj)), span)),
        };
        match sliced {
            Sliced::List(slice) => {
                // Root the source across the alloc: the new list shares its element handles, which
                // are otherwise unreachable (the source was popped) and could be collected by a GC.
                self.push(obj);
                let nh = self.heap.alloc(Obj::List(slice));
                self.pop();
                self.push(Value::obj(nh));
            }
            Sliced::Str(sub) => {
                let nh = self.heap.alloc(Obj::Str(sub.into()));
                self.push(Value::obj(nh));
            }
            Sliced::Bytes(sub) => {
                let nh = self.heap.alloc(Obj::Bytes(sub.into_boxed_slice()));
                self.push(Value::obj(nh));
            }
            Sliced::ByteArray(sub) => {
                let nh = self.heap.alloc(Obj::ByteArray(sub));
                self.push(Value::obj(nh));
            }
            Sliced::Struct => {
                // The `slice` protocol takes three `Option[int]` components — pass real `Option`
                // values (`None`/`Some(n)`) so the user body can `match`/`??` them. Root `obj`
                // across the enum allocs (it's the only reference keeping the receiver alive).
                self.push(obj);
                let opt = |vm: &mut Vm, c: Option<i64>| match c {
                    None => vm.alloc_enum("Option", "None", Vec::new()),
                    Some(n) => {
                        // `n` comes from `int_val` (may be a boxed BigInt) → box if wide.
                        let nv = vm.make_int(n);
                        vm.alloc_enum("Option", "Some", vec![nv])
                    }
                };
                let s_v = opt(self, s);
                let e_v = opt(self, e);
                let st_v = opt(self, st);
                self.pop();
                let v = self.dispatch_index_method(h, "slice", vec![obj, s_v, e_v, st_v], span)?;
                self.push(v);
            }
        }
        Ok(())
    }

    /// Dispatch an `Index`/`IndexSet`/`Slice` protocol method (`index`/`set_index`/`slice`) on a
    /// struct heap object. `args` already includes the receiver as its first element (bound to
    /// `self`). Mirrors `struct_arith`'s frame dispatch; the args are rooted as the new frame's locals.
    pub(super) fn dispatch_index_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let Obj::Struct { tid, .. } = self.heap.get(h) else {
            unreachable!()
        };
        let name = self.struct_name_of_tid(*tid);
        let def = self
            .program
            .structs
            .get(name)
            .cloned()
            .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        let proto = *def
            .methods
            .get(method)
            .ok_or_else(|| self.err(format!("struct '{name}' has no method '{method}'"), span))?;
        let home = self.module_objs[def.module_idx];
        // Guarded (B1): `index`/`slice`/`set_index` overloads run from native opcode handlers whose
        // operand state is on the host stack, so a blocking `recv` inside one cannot park — it faults
        // `deadlock` instead of suspending (matches `struct_arith`/`compare`/`hash`).
        self.guarded(|vm| vm.run_proto(proto, home, None, args, true, false, span))
    }

    pub(super) fn get_index(&mut self, span: Span) -> Result<(), RuntimeError> {
        // The index is NOT pre-validated as int (the `AsInt` was removed so map keys can be
        // str/bool): pop it as a Value and validate per object kind.
        let key = self.pop();
        let obj = self.pop();
        let Some(h) = obj.as_obj() else {
            return Err(self.err(format!("cannot index {}", self.type_name(obj)), span));
        };
        // M19 Tier-2 — Int-key fast path. A `List` or `Map` indexed by an `Int` needs no rooting:
        // `scalar_hash` on an int allocates nothing, can't GC, can't re-enter user code, so the
        // `hash_key_rooted` push/pop the general Map arm does is pure waste. The `candidates` +
        // `values_equal` probe is unchanged, so an Int key still matches a `values_equal` `Float`
        // key. `Str`/`Struct` (and any non-Int key) fall through to the general match below.
        if let Some(n) = key.as_int_inline() {
            match self.heap.get(h) {
                Obj::List(items) => {
                    return match crate::slice::norm_index(n, items.len()).map(|i| items[i]) {
                        Some(v) => {
                            self.push(v);
                            Ok(())
                        }
                        None => Err(self.err(
                            format!("index {n} out of bounds (len {})", items.len()),
                            span,
                        )),
                    };
                }
                // `bytes[i]`/`bytearray[i]` → `int` (0–255); out-of-range faults recoverably.
                Obj::Bytes(b) => {
                    return match crate::slice::norm_index(n, b.len()).map(|i| b[i] as i64) {
                        Some(v) => {
                            self.push(Value::int(v));
                            Ok(())
                        }
                        None => {
                            Err(self
                                .err(format!("index {n} out of bounds (len {})", b.len()), span))
                        }
                    };
                }
                Obj::ByteArray(b) => {
                    return match crate::slice::norm_index(n, b.len()).map(|i| b[i] as i64) {
                        Some(v) => {
                            self.push(Value::int(v));
                            Ok(())
                        }
                        None => {
                            Err(self
                                .err(format!("index {n} out of bounds (len {})", b.len()), span))
                        }
                    };
                }
                Obj::Map(_) => {
                    let hk = self.scalar_hash(key);
                    return match self.map_probe(h, hk, key, span)? {
                        Some(p) => {
                            let Obj::Map(m) = self.heap.get(h) else {
                                unreachable!()
                            };
                            let v = m.entries[p].2;
                            self.push(v);
                            Ok(())
                        }
                        // Int fast path: `key` is already the plain `n` popped above, so no
                        // rendering call (and no rooting) is needed to name it.
                        None => Err(self.err(format!("key not found: {n}"), span)),
                    };
                }
                _ => {}
            }
        }
        // Require an int index for list/str (the message matches the old `AsInt` exactly, for parity).
        let int_idx = |vm: &Vm| -> Result<i64, RuntimeError> {
            match vm.int_val(key) {
                Some(n) => Ok(n),
                None => Err(vm.err(format!("expected int, found {}", vm.type_name(key)), span)),
            }
        };
        match self.heap.get(h) {
            Obj::List(items) => {
                let idx = int_idx(self)?;
                let v = crate::slice::norm_index(idx, items.len()).map(|i| items[i]);
                match v {
                    Some(v) => {
                        self.push(v);
                        Ok(())
                    }
                    None => Err(self.err(
                        format!("index {idx} out of bounds (len {})", items.len()),
                        span,
                    )),
                }
            }
            Obj::Str(s) => {
                let idx = int_idx(self)?;
                let chars: Vec<char> = s.chars().collect();
                match crate::slice::norm_index(idx, chars.len()).map(|i| chars[i]) {
                    Some(c) => {
                        let nh = self.alloc_char(c);
                        self.push(nh);
                        Ok(())
                    }
                    None => Err(self.err(
                        format!("index {idx} out of bounds (len {})", chars.len()),
                        span,
                    )),
                }
            }
            Obj::Bytes(b) => {
                let idx = int_idx(self)?;
                match crate::slice::norm_index(idx, b.len()).map(|i| b[i] as i64) {
                    Some(v) => {
                        self.push(Value::int(v));
                        Ok(())
                    }
                    None => {
                        Err(self.err(format!("index {idx} out of bounds (len {})", b.len()), span))
                    }
                }
            }
            Obj::ByteArray(b) => {
                let idx = int_idx(self)?;
                match crate::slice::norm_index(idx, b.len()).map(|i| b[i] as i64) {
                    Some(v) => {
                        self.push(Value::int(v));
                        Ok(())
                    }
                    None => {
                        Err(self.err(format!("index {idx} out of bounds (len {})", b.len()), span))
                    }
                }
            }
            Obj::Map(_) => {
                let hk = self.hash_key_rooted(key, &[obj, key], span)?;
                match self.map_probe(h, hk, key, span)? {
                    Some(p) => {
                        let Obj::Map(m) = self.heap.get(h) else {
                            unreachable!()
                        };
                        let v = m.entries[p].2;
                        self.push(v);
                        Ok(())
                    }
                    None => {
                        // Render the key the same way a nested container element renders (quoted
                        // `str`, bare `int`/etc — `stringify_nested_into`), rooted across the call
                        // since a struct/enum key's `str()` display hook can re-enter the VM and GC.
                        self.push(key);
                        let mut ks = String::new();
                        let r = self.stringify_nested_into(&mut ks, key, span, 0);
                        self.pop();
                        r?;
                        Err(self.err(format!("key not found: {ks}"), span))
                    }
                }
            }
            // A struct satisfying `Index` dispatches `obj[k]` to `index(self, k)`.
            Obj::Struct { .. } => {
                let v = self.dispatch_index_method(h, "index", vec![obj, key], span)?;
                self.push(v);
                Ok(())
            }
            _ => Err(self.err(format!("cannot index {}", self.type_name(obj)), span)),
        }
    }

    pub(super) fn set_field(
        &mut self,
        name: &str,
        ic: u32,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let val = self.pop();
        let obj = self.pop();
        let Some(h) = obj.as_obj() else {
            return Err(self.err(
                format!("cannot assign field '{name}' of {}", self.type_name(obj)),
                span,
            ));
        };
        // M19 Phase 5b — IC fast path (see [`Vm::get_field`]): a hit on the `tid` guard writes straight
        // to the cached index (no field-name re-verify); a miss falls through to the probe + cache-fill.
        if ic != NO_IC {
            let cell = self.field_ic[ic as usize];
            if cell.tid != TID_NONE
                && let Obj::Struct { tid, fields, .. } = self.heap.get_mut(h)
                && *tid == cell.tid
                && let Some(slot) = fields.get_mut(cell.idx as usize)
            {
                *slot = val;
                return Ok(());
            }
        }
        // Positional layout: resolve the field name->index from the StructDef (declaration order)
        // BEFORE the mutable heap borrow, then write the flat `fields` slot by index.
        let stid = match self.heap.get(h) {
            Obj::Struct { tid, .. } => Some(*tid),
            _ => None,
        };
        let idx = stid
            .map(|t| self.struct_name_of_tid(t))
            .and_then(|n| self.program.structs.get(n))
            .and_then(|d| d.fields.iter().position(|f| f == name));
        let found;
        match self.heap.get_mut(h) {
            Obj::Struct { tid, fields, .. } => {
                let tid = *tid;
                match idx.and_then(|i| fields.get_mut(i).map(|slot| (i, slot))) {
                    Some((i, slot)) => {
                        *slot = val;
                        found = (i as u32, tid);
                    }
                    None => {
                        let shown = self.display(obj);
                        return Err(self.err(format!("no field '{name}' on {shown}"), span));
                    }
                }
            }
            _ => {
                return Err(self.err(
                    format!("cannot assign field '{name}' of {}", self.type_name(obj)),
                    span,
                ));
            }
        }
        if ic != NO_IC {
            self.field_ic[ic as usize] = IcCell {
                idx: found.0,
                tid: found.1,
            };
        }
        Ok(())
    }

    pub(super) fn set_index(&mut self, span: Span) -> Result<(), RuntimeError> {
        let val = self.pop();
        // The index is NOT pre-validated as int (AsInt removed for map keys): pop as a Value.
        let key = self.pop();
        let obj = self.pop();
        let Some(h) = obj.as_obj() else {
            return Err(self.err(format!("cannot index {}", self.type_name(obj)), span));
        };
        // M19 Tier-2 — Int-key fast path for a Map write: `scalar_hash` on an int needs no rooting
        // (it can't GC or re-enter, unlike a struct key's `hash()`), so skip `hash_key_rooted`. Same
        // `candidates`/`values_equal`/`push` as the general Map arm → byte-identical behavior. A
        // `Struct` with an Int key still falls through to its `set_index` protocol dispatch below.
        if self.is_integral(key) && matches!(self.heap.get(h), Obj::Map(_)) {
            let hk = self.scalar_hash(key);
            // `val` is an in-flight Rust local: `map_probe` roots the map + key itself, but a user
            // `eq` on a stored key could otherwise collect the value being written.
            let pos = self.with_roots(&[val], |vm| vm.map_probe(h, hk, key, span))?;
            let Obj::Map(m) = self.heap.get_mut(h) else {
                unreachable!()
            };
            match pos {
                Some(i) => m.entries[i].2 = val,
                None => m.push(hk, key, val),
            }
            return Ok(());
        }
        // For a map, hash the key (rooting the map/key/value across a struct key's re-entrant
        // hash()), locate the entry, then mutate — updating the side index on insert.
        if matches!(self.heap.get(h), Obj::Map(_)) {
            let hk = self.hash_key_rooted(key, &[obj, key, val], span)?;
            let pos = self.with_roots(&[val], |vm| vm.map_probe(h, hk, key, span))?;
            // On INSERT only, snapshot a struct/enum/newtype key so a later mutation of the
            // caller's live value can't corrupt the map (Go value-key model). An UPDATE reuses the
            // stored key and pays no clone. `snapshot_key` is pure alloc (no GC), so no rooting.
            match pos {
                Some(i) => {
                    let Obj::Map(m) = self.heap.get_mut(h) else {
                        unreachable!()
                    };
                    m.entries[i].2 = val;
                }
                None => {
                    // Snapshot a struct/enum/newtype key on INSERT only (Go value-key model); pure
                    // alloc (no GC), so `h`/`val` stay valid across it and no rooting is needed.
                    let key = self.snapshot_key(key);
                    let Obj::Map(m) = self.heap.get_mut(h) else {
                        unreachable!()
                    };
                    m.push(hk, key, val);
                }
            }
            return Ok(());
        }
        // A struct satisfying `IndexSet` dispatches `obj[k] = v` to `set_index(self, k, v)`.
        if matches!(self.heap.get(h), Obj::Struct { .. }) {
            self.dispatch_index_method(h, "set_index", vec![obj, key, val], span)?;
            return Ok(());
        }
        let idx = match self.int_val(key) {
            Some(n) => n,
            None => {
                return Err(self.err(format!("expected int, found {}", self.type_name(key)), span));
            }
        };
        // `bytearray[i] = x` — the NEW mutable capability `bytes` lacks. The value must be an `int`
        // in 0..=255 (validated BEFORE the in-place write); the index must be in range. Both are
        // distinct recoverable faults. Mutation flows through the heap slot, so two bindings to the
        // same `bytearray` observe it (like `list`). Validate the value up front (`&self` borrow)
        // before the `&mut self` `get_mut` below.
        if matches!(self.heap.get(h), Obj::ByteArray(_)) {
            let byte = match self.int_val(val) {
                Some(n) if (0..=255).contains(&n) => n as u8,
                Some(n) => {
                    return Err(self.err(
                        format!("byte value {n} out of range (must be 0..=255)"),
                        span,
                    ));
                }
                None => {
                    return Err(
                        self.err(format!("expected int, found {}", self.type_name(val)), span)
                    );
                }
            };
            let Obj::ByteArray(b) = self.heap.get_mut(h) else {
                unreachable!()
            };
            return match crate::slice::norm_index(idx, b.len()) {
                Some(i) => {
                    b[i] = byte;
                    Ok(())
                }
                None => {
                    let len = b.len();
                    Err(self.err(format!("index {idx} out of bounds (len {len})"), span))
                }
            };
        }
        match self.heap.get_mut(h) {
            Obj::List(items) => match crate::slice::norm_index(idx, items.len()) {
                Some(i) => {
                    items[i] = val;
                    Ok(())
                }
                None => {
                    let len = items.len();
                    Err(self.err(format!("index {idx} out of bounds (len {len})"), span))
                }
            },
            _ => Err(self.err(format!("cannot index {}", self.type_name(obj)), span)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn match_arm(
        &mut self,
        scrut: usize,
        variant: &str,
        variant_id: u32,
        enum_name: Option<&str>,
        nbind: usize,
        bind_start: usize,
        next: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let v = self.stack[self.base() + scrut];
        let h = match v.as_obj() {
            Some(h) => h,
            None => unreachable!("scrutinee ensured to be an enum"),
        };
        // M19 lever #2 — dispatch is a pure-int compare of the instance's stamped `variant_id` against
        // the arm's compile-time id (no variant-name string compare). `variant` is only the cold error.
        let (mut matches, vid, payload) = match self.heap.get(h) {
            Obj::Enum {
                variant_id: vid,
                payload,
            } => (*vid == variant_id, *vid, payload.clone()),
            _ => unreachable!("scrutinee ensured to be an enum"),
        };
        // SCRUTINEE-DRIVEN fallback on an id MISS. The compile-time `variant_id` can be the WRONG
        // module's id when a bare match-pattern enum qualifier (`Color.Red`) is reachable only via a
        // whole-module import and TWO whole-imported modules declare the same-named enum — the
        // construction side baked the scrutinee's correct id, but the pattern side may have guessed
        // the other module's id. Resolve from the SCRUTINEE's own `(enum_key, variant)` identity:
        // it matches iff the arm names that variant in the same (bare) enum. Built-in arms carry
        // `enum_name: None` and never enter this branch (pure-int dispatch — zero behavior change).
        if !matches && let Some(en) = enum_name {
            let (ekey, vname) = self.enum_names(vid);
            // Compare the bare display name without allocating (hot match path): strip the
            // `<module-key>::` prefix in place rather than via `bare_display`'s owned String.
            matches = vname == variant && ekey.rsplit("::").next().unwrap_or(ekey) == en;
        }
        if !matches {
            self.jump(next);
            return Ok(());
        }
        if payload.len() != nbind {
            return Err(self.err(
                format!(
                    "pattern '{variant}' binds {nbind} value(s) but variant carries {}",
                    payload.len()
                ),
                span,
            ));
        }
        let base = self.base();
        for (k, pv) in payload.into_iter().enumerate() {
            self.stack[base + bind_start + k] = pv;
        }
        Ok(())
    }

    // ----- builtins / print -----

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
            return Err(self.err(message, sp));
        }
        let result = match name {
            "range" => self.builtin_range(&args, span)?,
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

    /// One-time `Iterable` → cursor conversion: a PURE-`Iterable` struct (has `iter`, lacks `next`)
    /// becomes its cursor by running `iter(self)`; EVERYTHING else passes through unchanged (so it is
    /// safe to call on any value). Shared by `Op::IterableToCursor` (the `for` lowering) and
    /// [`drain_iterable`](Self::drain_iterable) (`List()`/`Set()`/`Map()`/`.iter()`), which must accept
    /// the same witnesses — the checker's `iterable_elem` admits an `iter`-only struct, and an
    /// `Iterable[T]` ANNOTATION hands one to every consumer, not only to `for`.
    ///
    /// The `next`-lacks test is by NAME, and the checker mirrors it: `struct_iterable_elem` refuses any
    /// struct that DECLARES a `next`, so a struct whose `next` is malformed is rejected at check time
    /// rather than admitted via `iter` here and then driven through that `next` by `drain_iterable`.
    pub(super) fn iterable_to_cursor(
        &mut self,
        v: Value,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let convert = if let Some(h) = v.as_obj()
            && let Obj::Struct { tid, .. } = self.heap.get(h)
        {
            let name = self.struct_name_of_tid(*tid);
            self.program
                .structs
                .get(name)
                .filter(|d| !d.methods.contains_key("next"))
                .and_then(|d| d.methods.get("iter").map(|p| (*p, d.module_idx)))
        } else {
            None
        };
        let Some((proto, module_idx)) = convert else {
            return Ok(v);
        };
        let home = self.module_objs[module_idx];
        // Re-enter the VM to run `iter(self)`; it returns the cursor (the body calls `self.xs.iter()`).
        // Root the receiver across the call (guarded GC).
        self.push(v);
        let cursor =
            self.guarded(|vm| vm.run_proto(proto, home, None, vec![v], true, false, span))?;
        self.pop(); // unroot receiver
        Ok(cursor)
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
                Obj::Map(m) => return Ok(m.entries.iter().map(|(_, k, _)| *k).collect()),
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
                _ => {}
            }
        }
        // Re-entrant paths (generator / user struct iterator): run user `.next()` until `None`, rooting
        // the source + a heap accumulator list on the operand stack across each call.
        let Some(h) = v.as_obj() else {
            return Err(self.err(format!("cannot iterate over {}", self.type_name(v)), span));
        };
        // Resolve the iteration step: a generator resumes; a user struct dispatches its `next` proto.
        enum Step {
            Generator,
            StructNext { proto: ProtoId, home: GcRef },
        }
        let step = match self.heap.get(h) {
            Obj::Generator(_) => Step::Generator,
            Obj::Struct { tid, .. } => {
                let name = self.struct_name_of_tid(*tid);
                let def = self
                    .program
                    .structs
                    .get(name)
                    .cloned()
                    .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
                let proto = *def.methods.get("next").ok_or_else(|| {
                    self.err(
                        format!(
                            "cannot iterate over {} (no `next` method)",
                            self.type_name(v)
                        ),
                        span,
                    )
                })?;
                Step::StructNext {
                    proto,
                    home: self.module_objs[def.module_idx],
                }
            }
            _ => return Err(self.err(format!("cannot iterate over {}", self.type_name(v)), span)),
        };
        // Root the source + the growing accumulator list across the re-entrant calls.
        let acc = self.heap.alloc(Obj::List(Vec::new()));
        self.push(v);
        self.push(Value::obj(acc));
        let result = (|| {
            loop {
                let res = match step {
                    Step::Generator => self.generator_next(h, span)?,
                    Step::StructNext { proto, home } => self.guarded(|vm| {
                        vm.run_proto(proto, home, None, vec![v], true, false, span)
                    })?,
                };
                let Some(rh) = res.as_obj() else {
                    return Err(self.err(
                        format!(
                            "iterator next() must return Option, found {}",
                            self.type_name(res)
                        ),
                        span,
                    ));
                };
                let Obj::Enum {
                    variant_id,
                    payload,
                } = self.heap.get(rh)
                else {
                    return Err(self.err(
                        format!(
                            "iterator next() must return Option, found {}",
                            self.type_name(res)
                        ),
                        span,
                    ));
                };
                match *variant_id {
                    crate::vm::op::VID_SOME => {
                        let item = *payload.first().ok_or_else(|| {
                            self.err(
                                "iterator next() returned Some with no payload".to_string(),
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
                        return Err(self.err(
                            format!(
                                "iterator next() must return Option, found {}",
                                self.type_name(res)
                            ),
                            span,
                        ));
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
            [one] => {
                let it = self.unwrap_newtype_value(*one);
                self.drain_iterable(it, span)?
            }
            _ => {
                return Err(self.err(
                    format!("Set() expects 0 or 1 argument(s), got {}", args.len()),
                    span,
                ));
            }
        };
        // Root the source elements (as a fresh PRIVATE heap list) so they survive a struct
        // element's re-entrant hash() GC. Phase 0 snapshots each struct/enum/newtype element IN
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

    /// Cast-unwrap a generic aggregate newtype to its inner value for `List(s)`/`Set(s)`/`Map(s)`: a
    /// `Obj::NewType` (e.g. a `Stack[T] = List[T]`) peels to the wrapped collection. A non-newtype
    /// value passes through. Type args are erased at runtime; the checker verified the underlying.
    pub(super) fn unwrap_newtype_value(&self, v: Value) -> Value {
        if let Some(h) = v.as_obj()
            && let Obj::NewType { inner, .. } = self.heap.get(h)
        {
            return *inner;
        }
        v
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
            [one] => {
                let it = self.unwrap_newtype_value(*one);
                self.drain_iterable(it, span)?
            }
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
        let it = self.unwrap_newtype_value(*one);
        // Cast-unwrapping a generic newtype over `Map[K, V]` (`Tally[T] = Map[T, int]`) yields the
        // inner map DIRECTLY — a copy, not a re-iteration as 2-tuples (iterating a map gives keys).
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
                // Snapshot a struct/enum/newtype KEY on insert (Go value-key model); root it on the
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

    pub(super) fn builtin_range(
        &mut self,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let ints: Option<Vec<i64>> = args.iter().map(|v| self.int_val(*v)).collect();
        let (start, end, step) = match ints.as_deref() {
            Some([n]) => (0, *n, 1),
            Some([a, b]) => (*a, *b, 1),
            Some([a, b, s]) => (*a, *b, *s),
            _ => {
                return Err(self.err(
                    "range() expects range(end), range(start, end), or range(start, end, step) of ints"
                        .to_string(),
                    span,
                ));
            }
        };
        let raw = crate::slice::range_values(start, end, step)
            .map_err(|message| self.err(message, span))?;
        let items: Vec<Value> = raw.into_iter().map(|n| self.make_int(n)).collect();
        Ok(Value::obj(self.heap.alloc(Obj::List(items))))
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
        if let Some(h) = v.as_obj() {
            match self.heap.get(h) {
                Obj::Str(s) => {
                    let s = s.to_string();
                    return match s.trim().parse::<i64>() {
                        Ok(n) => Ok(self.make_int(n)),
                        Err(_) => {
                            Err(self.err(format!("int(): cannot parse '{s}' as an integer"), span))
                        }
                    };
                }
                // `int(newtype)` unwraps the inner value (the cast-unwrap path). The checker has
                // already verified the underlying is `int`, so recursing yields the inner `Int`.
                Obj::NewType { inner, .. } => {
                    let inner = *inner;
                    return self.builtin_int(&[inner], span);
                }
                _ => {}
            }
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
        if let Some(h) = v.as_obj() {
            match self.heap.get(h) {
                Obj::Str(s) => {
                    let s = s.to_string();
                    return match s.trim().parse::<f64>() {
                        Ok(f) => Ok(self.box_float(f)),
                        Err(_) => {
                            Err(self.err(format!("float(): cannot parse '{s}' as a float"), span))
                        }
                    };
                }
                // `float(newtype)` unwraps the inner (checker verified the underlying is float).
                Obj::NewType { inner, .. } => {
                    let inner = *inner;
                    return self.builtin_float(&[inner], span);
                }
                _ => {}
            }
        }
        Err(self.err(
            format!("float() cannot convert {}", self.type_name(v)),
            span,
        ))
    }

    /// `bool(x)` — total truthiness cast over the scalars. Never faults on int/float/bool/str
    /// (+ scalar newtype-unwrap): int 0 -> false else true; float 0.0/-0.0 -> false, NaN -> true
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
        if let Some(h) = v.as_obj() {
            match self.heap.get(h) {
                Obj::Str(s) => return Ok(Value::bool(!s.is_empty())),
                // `bool(newtype)` unwraps the inner scalar (mirrors int/float's cast-unwrap).
                Obj::NewType { inner, .. } => {
                    let inner = *inner;
                    return self.builtin_bool(&[inner], span);
                }
                _ => {}
            }
        }
        Err(self.err(format!("bool() cannot convert {}", self.type_name(v)), span))
    }

    pub(super) fn builtin_str(
        &mut self,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        self.arity_err("str", args, 1, span)?;
        // `str` is dual: a `newtype N = str` with NO `str(self)` override UNWRAPS to its inner str
        // (the cast-unwrap). A `str(self)` override OR any other underlying goes through `stringify`
        // (the display cast — which itself honors the override). Mirrors the interp.
        if let Some(h) = args[0].as_obj()
            && let Obj::NewType { type_key, inner } = self.heap.get(h)
            && let Some(ih) = inner.as_obj()
            && matches!(self.heap.get(ih), Obj::Str(_))
            && !self
                .program
                .newtype_methods
                .get(type_key.as_ref())
                .is_some_and(|m| m.contains_key("str"))
        {
            return Ok(Value::obj(ih));
        }
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

    /// Read a module global. **D1 invariant:** on a `--parallel` worker VM a module's globals are
    /// faulted in lazily, so any NEW caller that reads globals on a worker must call
    /// [`Vm::ensure_module_faulted`] for `module` first (the existing op/field/method read sites do);
    /// otherwise it may observe an empty, not-yet-faulted module and spuriously fail to resolve.
    pub(super) fn module_global(&self, module: GcRef, name: &str) -> Option<Value> {
        match self.heap.get(module) {
            Obj::Module(m) => m.index.get(name).map(|&i| m.slots[i as usize]),
            _ => None,
        }
    }

    /// M19 Phase 2b — read a module global by compile-time slot. The home module is always pre-sized
    /// before any `GetGlobalSlot`: the top-level engine sizes it from `global_slots` in `run_module`,
    /// and a worker faults it fully in (`fault_module`) before reading. So the index is always valid.
    pub(super) fn global_slot(&self, module: GcRef, slot: u32) -> Value {
        match self.heap.get(module) {
            Obj::Module(m) => m.slots[slot as usize],
            _ => Value::nil(),
        }
    }

    /// M19 Phase 2b — write a module global by compile-time slot (`DefineGlobalSlot`/`SetGlobalSlot`).
    pub(super) fn set_global_slot(&mut self, module: GcRef, slot: u32, value: Value) {
        // W6-19 — a WRITE can be a task's FIRST module-global access (`fn worker(): g = 99`), and on a
        // worker the module's slots fault in LAZILY: without this the write indexed an empty `slots` vec
        // and PANICKED the pool thread (`index out of bounds: the len is 0`) while the now-removed
        // `--serial` engine printed the right answer. Rooted here (the sole slot-write helper) so both write ops and any future caller
        // are covered; a free no-op wherever no snapshot is installed (top-level `main`).
        self.ensure_module_faulted(module);
        // W6-2 — a module-slot write invalidates the snapshot CACHE: the next `spawn` must snapshot the
        // NEW value instead of replaying an earlier copy. One of exactly two module-slot mutators (with
        // `module_define`), so this pair is the whole invalidation surface for slot rebinding. Already-
        // queued tasks are unaffected — each carries the snapshot pinned at its own `spawn`
        // (`register_task`), so a later assignment never time-travels into a task that predates it.
        // One store; no scan of anything.
        self.snapshot_memo = None;
        // W7-4c — the cell registry describes that exact snapshot's numbering, so it dies with it.
        self.snapshot_cells = std::sync::Arc::new(super::fxhash::FxHashMap::default());
        if let Obj::Module(m) = self.heap.get_mut(module) {
            m.slots[slot as usize] = value;
        }
    }

    /// TICKET-051 — the write path for a task's OWN explicit assignment to a module global
    /// (`Op::SetGlobalSlot`). Marks `assigned` AND `carried` for `slot` on top of the plain write,
    /// so `install_global_slot` on this or a later view knows this write must never be clobbered
    /// by an arriving closure's stale value.
    pub(super) fn assign_global_slot(&mut self, module: GcRef, slot: u32, value: Value) {
        self.set_global_slot(module, slot, value);
        if let Obj::Module(m) = self.heap.get_mut(module) {
            let i = slot as usize;
            if m.assigned.len() <= i {
                m.assigned.resize(i + 1, false);
            }
            if m.carried.len() <= i {
                m.carried.resize(i + 1, false);
            }
            m.assigned[i] = true;
            m.carried[i] = true;
        }
    }

    /// TICKET-051 — the airlock's write path: installs an arriving closure's value for `slot` into
    /// the RECEIVING view, unless that view already assigned the slot itself. Returns `false` (and
    /// writes nothing) when `module` has no such slot (a hand-built fixture with an empty module) or
    /// the receiving view already assigned it; the receiving view's own write always wins. On a
    /// successful install marks `carried` (never `assigned`: this view did not write it itself),
    /// which is what lets a forwarding task's `closure_global_snapshot` carry the value onward.
    pub(super) fn install_global_slot(&mut self, module: GcRef, slot: u32, value: Value) -> bool {
        self.ensure_module_faulted(module);
        let ok = matches!(
            self.heap.get(module),
            Obj::Module(m) if (slot as usize) < m.slots.len()
                && !m.assigned.get(slot as usize).copied().unwrap_or(false)
        );
        if !ok {
            return false;
        }
        self.set_global_slot(module, slot, value);
        if let Obj::Module(m) = self.heap.get_mut(module) {
            let i = slot as usize;
            if m.carried.len() <= i {
                m.carried.resize(i + 1, false);
            }
            m.carried[i] = true;
        }
        true
    }

    /// Define (or overwrite) a global by name. M19 Phase 2b — if `name` already has a slot (the
    /// common case: the run driver pre-sized + indexed the module from `global_slots`, so imports
    /// and `DefineGlobalSlot` targets are already present) the value lands in that slot; otherwise a
    /// fresh slot is appended (native-module population + worker fault replay both build up modules
    /// this way, growing slots in the same order the parent assigned them).
    pub(super) fn module_define(&mut self, module: GcRef, name: &str, value: Value) {
        // W6-2 — the by-name twin of `set_global_slot`: invalidate the snapshot cache (import binding,
        // native-module population, worker fault replay). `fault_module` take/restores the memo around
        // its replay loop, since a replay REPRODUCES the snapshot rather than mutating the view.
        self.snapshot_memo = None;
        // W7-4c — the cell registry describes that exact snapshot's numbering, so it dies with it.
        self.snapshot_cells = std::sync::Arc::new(super::fxhash::FxHashMap::default());
        if let Obj::Module(m) = self.heap.get_mut(module) {
            match m.index.get(name) {
                Some(&i) => m.slots[i as usize] = value,
                None => {
                    m.index.insert(name.into(), m.slots.len() as u32);
                    m.slots.push(value);
                }
            }
        }
    }

    pub(super) fn module_name(&self, module: GcRef) -> String {
        match self.heap.get(module) {
            Obj::Module(m) => m.name.to_string(),
            _ => String::new(),
        }
    }

    // ----- display / type names -----

    pub(super) fn type_name(&self, v: Value) -> &'static str {
        // A boxed float is Float-tagged → `view` yields `Obj(h)` → the `Obj::FloatBox => "float"` arm
        // below names it (a boxed `BigInt` likewise → "int").
        match v.view() {
            ValueView::Int(_) => "int",
            ValueView::Bool(_) => "bool",
            ValueView::Nil => "nil",
            ValueView::Obj(h) => match self.heap.get(h) {
                Obj::Str(_) => "str",
                Obj::Bytes(_) => "bytes",
                Obj::ByteArray(_) => "bytearray",
                // Boxed scalars report the same type name as the inline `Int`/`Float`.
                Obj::BigInt(_) => "int",
                Obj::FloatBox(_) => "float",
                Obj::List(_) => "List",
                Obj::Tuple(_) => "tuple",
                Obj::Map(_) => "Map",
                Obj::Set(_) => "Set",
                Obj::Struct { .. } => "struct",
                Obj::Enum { .. } => "enum",
                Obj::NewType { .. } => "newtype",
                Obj::Func { .. } | Obj::Closure { .. } => "function",
                Obj::Module(_) => "module",
                Obj::Native { .. } => "function",
                Obj::Builtin(_) => "function",
                Obj::Cffi(_) => "function",
                Obj::Ptr(_) => "ptr",
                Obj::Channel(_) => "Channel",
                Obj::Shared(_) => "Shared",
                Obj::RwShared(_) => "RwShared",
                Obj::Atomic(_) => "Atomic",
                Obj::AtomicInt(_) => "AtomicInt",
                Obj::Executor(_) => "Executor",
                Obj::Socket(_) => "Socket",
                Obj::Listener(_) => "Listener",
                Obj::Writer(_) => "Writer",
                Obj::Reader(_) => "Reader",
                Obj::Generator(_) => "generator",
                // A cell is a transparent by-reference box (never a user-visible operand — reads
                // `CellLoad` first); defensively report its inner value's type.
                Obj::Cell(v) => self.type_name(*v),
                Obj::Iter { .. } => "iterator",
            },
        }
    }

    /// `Display` form. Thin wrapper over the depth-guarded worker — kept infallible so every
    /// error-message / `display_wire` caller is unchanged; a cyclic structure renders as `<...>` here
    /// (the print path surfaces the error).
    pub(super) fn display(&self, v: Value) -> String {
        self.display_guarded(v, 0)
            .unwrap_or_else(|_| "<...>".to_string())
    }

    /// Depth-guarded structural display. Returns `Err` (recoverable) once recursion exceeds
    /// [`MAX_STRUCTURAL_DEPTH`] — guarding cyclic data from overflowing the host stack.
    pub(super) fn display_guarded(&self, v: Value, depth: usize) -> Result<String, RuntimeError> {
        if self.walk_base + depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.depth_exceeded_err(Span::RUNTIME));
        }
        match v.view() {
            ValueView::Int(n) => Ok(n.to_string()),
            ValueView::Bool(b) => Ok(b.to_string()),
            ValueView::Nil => Ok("nil".to_string()),
            // A boxed float/big-int is heap-tagged → the `Obj::FloatBox`/`Obj::BigInt` arms below.
            ValueView::Obj(h) => match self.heap.get(h) {
                // NESTED (`depth > 0`) means this string sits inside a container / field / payload,
                // so it renders as its `repr` — same rule as `stringify_nested_into`, applied here
                // by depth alone because this renderer is `&self` and has no display-hook path that
                // preserves depth. A top-level `display` of a `str` stays its bare characters.
                Obj::Str(s) if depth > 0 => Ok(crate::slice::str_repr(s)),
                Obj::Str(s) => Ok(s.to_string()),
                // Boxed scalars render identically to the inline `Int`/`Float`.
                Obj::BigInt(n) => Ok(n.to_string()),
                Obj::FloatBox(f) => Ok(format_float(*f)),
                // Python `bytes` repr `b'...'` — shared with the interp via `slice::bytes_repr`.
                Obj::Bytes(b) => Ok(crate::slice::bytes_repr(b)),
                // Python `bytearray` repr `bytearray(b'...')` — shared via `slice::bytearray_repr`.
                Obj::ByteArray(b) => Ok(crate::slice::bytearray_repr(b)),
                Obj::List(items) => {
                    let mut parts = Vec::with_capacity(items.len());
                    for v in items {
                        parts.push(self.display_guarded(*v, depth + 1)?);
                    }
                    Ok(format!("[{}]", parts.join(", ")))
                }
                Obj::Tuple(items) => {
                    let mut parts = Vec::with_capacity(items.len());
                    for v in items {
                        parts.push(self.display_guarded(*v, depth + 1)?);
                    }
                    Ok(format!("({})", parts.join(", ")))
                }
                Obj::Map(m) => {
                    let mut parts = Vec::with_capacity(m.entries.len());
                    for (_, k, v) in &m.entries {
                        parts.push(format!(
                            "{}: {}",
                            self.display_guarded(*k, depth + 1)?,
                            self.display_guarded(*v, depth + 1)?
                        ));
                    }
                    Ok(format!("{{{}}}", parts.join(", ")))
                }
                Obj::Set(s) => {
                    if s.entries.is_empty() {
                        Ok("Set()".to_string())
                    } else {
                        let mut parts = Vec::with_capacity(s.entries.len());
                        for (_, v) in &s.entries {
                            parts.push(self.display_guarded(*v, depth + 1)?);
                        }
                        Ok(format!("{{{}}}", parts.join(", ")))
                    }
                }
                Obj::Struct { tid, fields, .. } => {
                    // Positional layout: recover declaration-order field names from the StructDef
                    // (cold display path). Resolve the type key from `tid`; snapshot values to drop
                    // the heap borrow.
                    let name = self.struct_name_of_tid(*tid);
                    let vals: Vec<Value> = fields.as_slice().to_vec();
                    // ROOT REDESIGN — render the BARE display name (not the qualified identity key);
                    // `name` is the key the StructDef is stored under. Fall back to stripping the key.
                    let (display, names): (String, Vec<String>) = self
                        .program
                        .structs
                        .get(name)
                        .map(|d| (d.display_name.clone(), d.fields.clone()))
                        .unwrap_or_else(|| (crate::compiler::bare_display(name), Vec::new()));
                    let mut parts = Vec::with_capacity(vals.len());
                    for (i, v) in vals.iter().enumerate() {
                        let k = names.get(i).cloned().unwrap_or_else(|| i.to_string());
                        parts.push(format!("{k}={}", self.display_guarded(*v, depth + 1)?));
                    }
                    Ok(format!("{display}({})", parts.join(", ")))
                }
                Obj::Enum {
                    variant_id,
                    payload,
                } => {
                    // M19 lever #2 — recover the variant name from the id (cold display path).
                    let variant = self.enum_names(*variant_id).1.to_string();
                    let payload: Vec<Value> = payload.clone();
                    if payload.is_empty() {
                        Ok(variant)
                    } else {
                        let mut parts = Vec::with_capacity(payload.len());
                        for v in &payload {
                            parts.push(self.display_guarded(*v, depth + 1)?);
                        }
                        Ok(format!("{variant}({})", parts.join(", ")))
                    }
                }
                // Raw display fallback (no method dispatch here): `Name(inner)`. The `str(self)`
                // override is honored by `stringify` (the path print/`str()` actually use).
                Obj::NewType { type_key, inner } => {
                    let display = crate::compiler::bare_display(type_key.as_ref());
                    let inner = *inner;
                    Ok(format!(
                        "{display}({})",
                        self.display_guarded(inner, depth + 1)?
                    ))
                }
                Obj::Func { proto, .. } => Ok(format!("<fn {}>", self.program.protos[*proto].name)),
                Obj::Closure { .. } => Ok("<closure>".to_string()),
                Obj::Module(m) => Ok(format!("<module {}>", m.name)),
                Obj::Native { name, .. } => Ok(format!("<native fn {name}>")),
                Obj::Builtin(name) => Ok(format!("<builtin fn {name}>")),
                Obj::Cffi(c) => Ok(format!("<extern fn {}>", c.name())),
                // A raw address is non-deterministic (differs per run), so never render it — a
                // printed pointer's value would not be reproducible. Only null vs live (a
                // deterministic distinction) is observable.
                Obj::Ptr(a) => Ok(if *a == 0 {
                    "<ptr null>".to_string()
                } else {
                    "<ptr>".to_string()
                }),
                // TICKET-042a — `msg_len`, not the raw queue length: a parked rendezvous sender's
                // deposit stays invisible here too, matching `Channel.len()`.
                Obj::Channel(core) => {
                    Ok(format!("Channel(len={})", core.q.lock().unwrap().msg_len()))
                }
                // B3.1: the box holds the wire form; render it directly (`display` is `&self` and
                // cannot `from_wire`, which allocates — `display_wire` is the read-only equivalent).
                Obj::Shared(core) => Ok(format!(
                    "Shared({})",
                    self.display_wire(&core.v.lock().unwrap())
                )),
                Obj::RwShared(core) => Ok(format!(
                    "RwShared({})",
                    self.display_wire(&core.v.read().unwrap())
                )),
                Obj::Atomic(core) => Ok(format!(
                    "Atomic({})",
                    self.display_wire(&core.v.lock().unwrap())
                )),
                Obj::AtomicInt(core) => Ok(format!(
                    "AtomicInt({})",
                    core.v.load(std::sync::atomic::Ordering::SeqCst)
                )),
                // Work not yet finished, counted across BOTH halves: the lazy `inner` queue (which
                // only the since-removed cooperative engine ever filled) and the eager outstanding
                // count. Summing both is what keeps this honest — reading only the queue would report
                // `pending=0` while jobs are running. Exactly one term is ever non-zero today.
                Obj::Executor(core) => Ok(format!(
                    "Executor(pending={})",
                    core.inner.lock().unwrap().len()
                        + core
                            .eager
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .outstanding()
                )),
                // D6: render open/closed without exposing the fd; matches no interp counterpart (net
                // is VM-only) but mirrors the core handles' structural `Display`.
                Obj::Socket(core) => Ok(format!(
                    "Socket({})",
                    if core.stream.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )),
                Obj::Listener(core) => Ok(format!(
                    "Listener({})",
                    if core.listener.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )),
                // R2: render open/closed without exposing the fd (mirrors the socket handles).
                Obj::Writer(core) => Ok(format!(
                    "Writer({})",
                    if core.inner.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )),
                // R2b: render open/closed without exposing the fd (mirrors the Writer handle).
                Obj::Reader(core) => Ok(format!(
                    "Reader({})",
                    if core.inner.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )),
                Obj::Generator(_) => Ok("<generator>".to_string()),
                // A cell is a transparent by-reference box (never a user-visible operand — reads
                // `CellLoad` first); defensively display its inner value.
                Obj::Cell(v) => self.display_guarded(*v, depth),
                Obj::Iter { .. } => Ok("<iterator>".to_string()),
            },
        }
    }

    /// `Display` form of a [`WireValue`] — the read-only (`&self`) counterpart of [`display`] for
    /// values that live in a core (only `Shared` renders its contents). Mirrors `display` arm-for-arm;
    /// a `Handle(GcRef)` resolves back through the heap via `display`, a nested core renders like its
    /// heap counterpart. B3.1: total over the sendable set.
    pub(super) fn display_wire(&self, w: &WireValue) -> String {
        match w {
            WireValue::Int(n) => n.to_string(),
            WireValue::Float(x) => format_float(*x),
            WireValue::Bool(b) => b.to_string(),
            WireValue::Nil => "nil".to_string(),
            // Every `display_wire` caller renders a NESTED position — inside `Shared(…)`/`Atomic(…)`,
            // a container, or a struct field — so a wire string is always quoted (`Shared(['a'])`).
            WireValue::Str(s) => crate::slice::str_repr(s),
            WireValue::Bytes(b) => crate::slice::bytes_repr(b),
            WireValue::ByteArray(b) => crate::slice::bytearray_repr(b),
            WireValue::Handle(h) => self.display(Value::obj(*h)),
            WireValue::List { items, .. } => {
                let inner = items
                    .iter()
                    .map(|v| self.display_wire(v))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("[{inner}]")
            }
            WireValue::Tuple { items, .. } => {
                let inner = items
                    .iter()
                    .map(|v| self.display_wire(v))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({inner})")
            }
            WireValue::Map { entries, .. } => {
                let inner = entries
                    .iter()
                    .map(|(_, k, v)| format!("{}: {}", self.display_wire(k), self.display_wire(v)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{{inner}}}")
            }
            WireValue::Set { entries, .. } => {
                if entries.is_empty() {
                    "Set()".to_string()
                } else {
                    let inner = entries
                        .iter()
                        .map(|(_, v)| self.display_wire(v))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{{{inner}}}")
                }
            }
            WireValue::Struct { name, fields, .. } => {
                let inner = fields
                    .iter()
                    .map(|(k, v)| format!("{k}={}", self.display_wire(v)))
                    .collect::<Vec<_>>()
                    .join(", ");
                // ROOT REDESIGN — `name` is the qualified identity key; render the bare display name.
                let display = self
                    .program
                    .structs
                    .get(name.as_ref())
                    .map(|d| d.display_name.clone())
                    .unwrap_or_else(|| crate::compiler::bare_display(name.as_ref()));
                format!("{display}({inner})")
            }
            WireValue::Enum {
                variant_id,
                payload,
                ..
            } => {
                // M19 lever #2 — the wire form carries the id; resolve the variant name on this cold
                // display path via the shared program's `variants_by_id`.
                let variant = self.enum_names(*variant_id).1;
                if payload.is_empty() {
                    variant.to_string()
                } else {
                    let inner = payload
                        .iter()
                        .map(|v| self.display_wire(v))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{variant}({inner})")
                }
            }
            WireValue::NewType {
                type_key, inner, ..
            } => {
                let display = crate::compiler::bare_display(type_key.as_ref());
                format!("{display}({})", self.display_wire(inner))
            }
            WireValue::Channel(core) => {
                // TICKET-042a — see the `Obj::Channel` arm above.
                format!("Channel(len={})", core.q.lock().unwrap().msg_len())
            }
            WireValue::Shared(core) => {
                format!("Shared({})", self.display_wire(&core.v.lock().unwrap()))
            }
            WireValue::RwShared(core) => {
                format!("RwShared({})", self.display_wire(&core.v.read().unwrap()))
            }
            WireValue::Atomic(core) => {
                format!("Atomic({})", self.display_wire(&core.v.lock().unwrap()))
            }
            WireValue::AtomicInt(core) => {
                format!(
                    "AtomicInt({})",
                    core.v.load(std::sync::atomic::Ordering::SeqCst)
                )
            }
            WireValue::Executor(core) => {
                format!("Executor(pending={})", core.inner.lock().unwrap().len())
            }
            // D6: render open/closed without exposing the fd (mirrors the heap `Display`).
            WireValue::Socket(core) => {
                format!(
                    "Socket({})",
                    if core.stream.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )
            }
            WireValue::Listener(core) => {
                format!(
                    "Listener({})",
                    if core.listener.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )
            }
            // R2: render open/closed without exposing the fd (mirrors the heap `Display`).
            WireValue::Writer(core) => {
                format!(
                    "Writer({})",
                    if core.inner.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )
            }
            // R2b: render open/closed without exposing the fd (mirrors the heap `Display`).
            WireValue::Reader(core) => {
                format!(
                    "Reader({})",
                    if core.inner.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )
            }
            // An opaque `ptr` renders like its heap counterpart (`Obj::Ptr` → "<ptr null>"/"<ptr>");
            // never the raw address (non-deterministic across engines).
            WireValue::Ptr(a) => {
                if *a == 0 {
                    "<ptr null>".to_string()
                } else {
                    "<ptr>".to_string()
                }
            }
            // A wired first-class builtin fn renders like its heap counterpart (`<builtin fn name>`).
            WireValue::Builtin(name) => format!("<builtin fn {name}>"),
            // A wired native/FFI fn renders like its heap counterpart (`Obj::Native`/`Obj::Cffi`).
            WireValue::Native { name, .. } => format!("<native fn {name}>"),
            WireValue::Cffi(c) => format!("<extern fn {}>", c.name()),
            // B3.6: a wired closure renders like its heap counterpart (`Obj::Closure` → "<closure>").
            WireValue::Closure { .. } => "<closure>".to_string(),
            // B3.3: a wired bare fn renders like its heap counterpart (`Obj::Func` → "<fn NAME>"),
            // DISTINCT from a closure — this is why `WireValue::Func` is kept separate from `Closure`.
            WireValue::Func { proto, .. } => format!("<fn {}>", self.program.protos[*proto].name),
            // A wired cursor renders like its heap counterpart (`Obj::Iter` → "<iterator>").
            WireValue::Iter { .. } => "<iterator>".to_string(),
            // A cell is a transparent box — render its inner value (never user-visible in practice).
            WireValue::Cell { inner, .. } => self.display_wire(inner),
            // A back-reference closes a Cell/Closure cycle; render a stable marker rather than recurse
            // (never user-visible in practice — a wire value is only rendered on the error path).
            WireValue::Backref(_) => "<cycle>".to_string(),
            // F3 path C: a wired generator renders like its heap counterpart (`Obj::Generator` →
            // "<generator>").
            WireValue::Generator { .. } => "<generator>".to_string(),
        }
    }

    /// Protocol-aware render for `print` / `str()` / interpolation: a struct with a self-only
    /// `str(self) -> str` method (the `Stringable` protocol) dispatches to it; everything else uses
    /// the default structural repr, recursing through `stringify` so nested structs honour the
    /// protocol too. Distinct from the `&self` `display` above, which stays the pure structural form
    /// for error/debug text.
    pub(super) fn stringify(
        &mut self,
        v: Value,
        span: Span,
        depth: usize,
    ) -> Result<String, RuntimeError> {
        let mut s = String::new();
        self.stringify_into(&mut s, v, span, depth)?;
        Ok(s)
    }

    /// Render `v` by appending into `out` — the allocation-free core shared by `stringify` (which
    /// wraps it in a fresh `String`) and `BuildStr` (which reuses one buffer across all interpolation
    /// parts). Byte-identical output to the old return-a-`String` form; only the intermediate
    /// per-part / per-element `String`s are gone.
    /// `Op::ToStrFmt` — render the top-of-stack value per the parsed format spec. Scalars map
    /// straight to a [`crate::fmtspec::FmtArg`]; non-scalars are rendered via the normal
    /// `stringify_into` first (rooted on the operand stack, so a `str` method's nested frames see a
    /// live object), then formatted as a plain string. The spec's width is already capped at compile
    /// time, so no pathological allocation is possible here. Lives in its own `#[inline(never)]`
    /// helper to keep `step`'s frame small (commit 1450077).
    #[inline(never)]
    pub(super) fn op_to_str_fmt(
        &mut self,
        spec: &crate::fmtspec::FormatSpec,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let v = self.stack[self.stack.len() - 1]; // leave rooted; rendering may run user code
        let mut out = String::new();
        if let Some(n) = self.int_val(v) {
            crate::fmtspec::apply(spec, crate::fmtspec::FmtArg::Int(n), &mut out)
                .map_err(|m| self.err(m, span))?;
        } else if v.is_float() {
            let x = self.float_of(v);
            crate::fmtspec::apply(spec, crate::fmtspec::FmtArg::Float(x), &mut out)
                .map_err(|m| self.err(m, span))?;
        } else if let Some(h) = v.as_obj()
            && matches!(self.heap.get(h), Obj::Str(_))
        {
            let s = match self.heap.get(h) {
                Obj::Str(s) => s.clone(),
                _ => unreachable!(),
            };
            crate::fmtspec::apply(spec, crate::fmtspec::FmtArg::Str(&s), &mut out)
                .map_err(|m| self.err(m, span))?;
        } else {
            // Bool/Nil/containers/structs: render with the normal stringify, then treat as a
            // string for fill/align/width (type chars/precision error via `apply`).
            let mut rendered = String::new();
            self.stringify_into(&mut rendered, v, span, 0)?;
            crate::fmtspec::apply(spec, crate::fmtspec::FmtArg::Other(&rendered), &mut out)
                .map_err(|m| self.err(m, span))?;
        }
        self.pop();
        let h = self.heap.alloc(Obj::Str(out.into()));
        self.push(Value::obj(h));
        Ok(())
    }

    pub(super) fn stringify_into(
        &mut self,
        out: &mut String,
        v: Value,
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        use std::fmt::Write as _;
        // Guard against cyclic data overflowing the host stack — turns SIGABRT into a recoverable
        // `RuntimeError`. Tested against `walk_base + depth`: a `str` hook that stringifies from
        // inside its BODY continues on the same shared budget rather than restarting at 0 (see
        // `Vm::walk_base`) — without that, hook-nesting × per-hook depth is unbounded and the
        // process dies uncatchably. The hook's RESULT is re-stringified at the *same* `depth` after
        // `guarded_walk` restored, so a non-recursive protocol hook still doesn't burn the budget.
        if self.walk_base + depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.depth_exceeded_err(span));
        }
        if let Some(n) = self.int_val(v) {
            let _ = write!(out, "{n}");
        } else if v.is_float() {
            out.push_str(&format_float(self.float_of(v)));
        } else if let Some(b) = v.as_bool() {
            out.push_str(if b { "true" } else { "false" });
        } else if v.is_nil() {
            out.push_str("nil");
        } else if let Some(h) = v.as_obj() {
            // ROOT the object on the operand stack: a `str` method runs nested frames that GC at
            // instruction boundaries, and the container keeps its transitive contents reachable.
            self.push(v);
            let r = self.stringify_obj_into(out, h, span, depth);
            self.pop();
            return r;
        }
        Ok(())
    }

    /// True iff `v` is a `str` at runtime (a heap [`Obj::Str`]). `str` is the only string
    /// representation — [`Value`] carries no inline string — so a user `str(self)` display hook's
    /// result "conforms to `Stringable`" exactly when this holds. Used by the display-hook arms to
    /// decide, AFTER invoking the hook, whether to use its result or fall back to the default repr
    /// (Bug B). Checking the runtime VALUE (not the declared syntax) transparently covers an
    /// annotated `-> str`, an inferred (un-annotated) str, and a str type-alias return alike.
    pub(super) fn is_str_value(&self, v: Value) -> bool {
        matches!(v.as_obj(), Some(h) if matches!(self.heap.get(h), Obj::Str(_)))
    }

    /// Render a value that is NESTED inside something else — a list/tuple/map/set element, a struct
    /// field, an enum payload. Identical to [`Self::stringify_into`] except that a `str` renders as
    /// its Python `repr` (quoted + escaped) instead of its bare characters, so `["a", "b"]` and
    /// `["a, b"]` no longer print the same text and `[""]` no longer prints `[]` (`docs/gaps.md`
    /// §W7-25). CPython's `str` vs `repr` split, same rule.
    ///
    /// NOT used for a `str(self)` display hook's RESULT: that string is the object's own rendering,
    /// not a nested value, so it must never be quoted (those sites re-enter `stringify_into` at the
    /// same depth deliberately).
    pub(super) fn stringify_nested_into(
        &mut self,
        out: &mut String,
        v: Value,
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        if let Some(h) = v.as_obj()
            && let Obj::Str(s) = self.heap.get(h)
        {
            out.push_str(&crate::slice::str_repr(s));
            return Ok(());
        }
        self.stringify_into(out, v, span, depth)
    }

    pub(super) fn stringify_obj_into(
        &mut self,
        out: &mut String,
        h: GcRef,
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        use std::fmt::Write as _;
        // Clone the object's shape out so no heap borrow is held across the nested `&mut self` calls.
        match self.heap.get(h).clone() {
            Obj::Str(s) => out.push_str(&s),
            // Boxed scalars stringify identically to the inline `Int`/`Float`.
            Obj::BigInt(n) => out.push_str(&n.to_string()),
            Obj::FloatBox(f) => out.push_str(&format_float(f)),
            // `bytes` interpolates/prints as its Python `b'...'` repr (shared helper).
            Obj::Bytes(b) => out.push_str(&crate::slice::bytes_repr(&b)),
            // `bytearray` interpolates/prints as `bytearray(b'...')` (shared helper).
            Obj::ByteArray(b) => out.push_str(&crate::slice::bytearray_repr(&b)),
            Obj::List(items) => {
                out.push('[');
                self.stringify_seq_into(out, &items, span, depth + 1)?;
                out.push(']');
            }
            Obj::Tuple(items) => {
                out.push('(');
                self.stringify_seq_into(out, &items, span, depth + 1)?;
                out.push(')');
            }
            Obj::Map(m) => {
                out.push('{');
                for (i, (_, k, mv)) in m.entries.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    self.stringify_nested_into(out, *k, span, depth + 1)?;
                    out.push_str(": ");
                    self.stringify_nested_into(out, *mv, span, depth + 1)?;
                }
                out.push('}');
            }
            Obj::Set(s) => {
                if s.entries.is_empty() {
                    out.push_str("Set()");
                } else {
                    out.push('{');
                    for (i, (_, e)) in s.entries.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        self.stringify_nested_into(out, *e, span, depth + 1)?;
                    }
                    out.push('}');
                }
            }
            Obj::Struct { tid, mut fields } => {
                // Resolve the type key from `tid` (owned — the `str` hook below takes `&mut self`,
                // so a `&self` borrow of the name can't live across it). Cold stringify path.
                let name = self.struct_name_of_tid(tid).to_string();
                // A self-only `str(self)` overrides the default repr — but ONLY when it actually
                // conforms to `Stringable`, i.e. it returns a `str`. Bug B: `str` is a normal user
                // method that may legitimately return anything (e.g. `-> S` returning the struct
                // itself, or an inferred/aliased str), and its return type is not reliably reachable
                // at the bytecode level. So — mirroring arith.rs `struct_hash`/`enum_hash`, which
                // invoke the hook then inspect the returned value — invoke `str` and use its result
                // ONLY when it is a `str`. A non-`str` result must NOT be re-stringified: that
                // re-enters this arm, re-invokes the hook, and recurses forever (uncatchable native
                // stack overflow). It falls through to the default repr, like a wrong-arity `str`.
                let def = self.program.structs.get(name.as_str()).cloned();
                if let Some(def) = &def
                    && let Some(&proto) = def.methods.get("str")
                    && self.program.protos[proto].arity == 1
                {
                    let home = self.module_objs[def.module_idx];
                    // The hook re-enters the VM and may start its own structural walk, so hand it
                    // the depth this walk has already consumed — one shared budget (see
                    // [`Vm::walk_base`]), not a fresh 10 000 per nesting level. The
                    // `stringify_into(out, res, ...)` below runs AFTER `guarded_walk` restored, at
                    // the outer `depth` — correct, it is the hook's RESULT, not a nested walk.
                    let base = self.walk_base + depth;
                    let res = self.guarded_walk(base, |vm| {
                        vm.run_proto(proto, home, None, vec![Value::obj(h)], true, false, span)
                    })?;
                    if self.is_str_value(res) {
                        return self.stringify_into(out, res, span, depth);
                    }
                    // Non-`str`: fall through to the default repr. The hook may have mutated self's
                    // fields (and triggered GC, freeing the pre-hook `fields` clone taken at fn
                    // entry), so re-read the live, rooted struct — otherwise the render loop below
                    // could dereference a swept GcRef and panic uncatchably (GC-safety).
                    if let Obj::Struct { fields: cur, .. } = self.heap.get(h) {
                        fields = cur.clone();
                    }
                }
                // Positional layout: recover declaration-order field names from the StructDef (the
                // same `def` already cloned for the `str` hook) — no per-instance name strings.
                // ROOT REDESIGN — render the BARE display name, not the qualified identity key.
                let display = def
                    .as_ref()
                    .map(|d| d.display_name.clone())
                    .unwrap_or_else(|| crate::compiler::bare_display(&name));
                let _ = write!(out, "{display}(");
                for (i, fv) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    match def.as_ref().and_then(|d| d.fields.get(i)) {
                        Some(k) => {
                            let _ = write!(out, "{k}=");
                        }
                        None => {
                            let _ = write!(out, "{i}=");
                        }
                    }
                    self.stringify_nested_into(out, *fv, span, depth + 1)?;
                }
                out.push(')');
            }
            Obj::Enum {
                variant_id,
                mut payload,
            } => {
                // A self-only `str(self)` overrides the default `Variant(payload)` repr, but only
                // when it returns a `str` — checked on the returned value, not the declared syntax
                // (mirrors the struct arm; see its Bug B note).
                let key = self.enum_names(variant_id).0.to_string();
                if let Some(&proto) = self
                    .program
                    .enum_methods
                    .get(&key)
                    .and_then(|m| m.get("str"))
                    && self.program.protos[proto].arity == 1
                {
                    let home = self.module_objs[self.enum_home_module(&key)];
                    // The hook re-enters the VM and may start its own structural walk, so hand it
                    // the depth this walk has already consumed — one shared budget (see
                    // [`Vm::walk_base`]), not a fresh 10 000 per nesting level. The
                    // `stringify_into(out, res, ...)` below runs AFTER `guarded_walk` restored, at
                    // the outer `depth` — correct, it is the hook's RESULT, not a nested walk.
                    let base = self.walk_base + depth;
                    let res = self.guarded_walk(base, |vm| {
                        vm.run_proto(proto, home, None, vec![Value::obj(h)], true, false, span)
                    })?;
                    if self.is_str_value(res) {
                        return self.stringify_into(out, res, span, depth);
                    }
                    // Non-`str`: re-read the live rooted enum before the default render (GC-safety;
                    // see the struct arm).
                    if let Obj::Enum { payload: cur, .. } = self.heap.get(h) {
                        payload = cur.clone();
                    }
                }
                // M19 lever #2 — recover the variant name from the id (cold stringify path).
                out.push_str(self.enum_names(variant_id).1);
                if !payload.is_empty() {
                    out.push('(');
                    self.stringify_seq_into(out, &payload, span, depth + 1)?;
                    out.push(')');
                }
            }
            // A newtype honors a `str(self) -> str` override (Stringable) exactly like enum/struct;
            // else it renders `Name(inner)` (its raw `Display`).
            Obj::NewType {
                type_key,
                mut inner,
            } => {
                if let Some(&proto) = self
                    .program
                    .newtype_methods
                    .get(type_key.as_ref())
                    .and_then(|m| m.get("str"))
                    && self.program.protos[proto].arity == 1
                {
                    let home = self.module_objs[self.newtype_home_module(&type_key)];
                    // The hook re-enters the VM and may start its own structural walk, so hand it
                    // the depth this walk has already consumed — one shared budget (see
                    // [`Vm::walk_base`]), not a fresh 10 000 per nesting level. The
                    // `stringify_into(out, res, ...)` below runs AFTER `guarded_walk` restored, at
                    // the outer `depth` — correct, it is the hook's RESULT, not a nested walk.
                    let base = self.walk_base + depth;
                    let res = self.guarded_walk(base, |vm| {
                        vm.run_proto(proto, home, None, vec![Value::obj(h)], true, false, span)
                    })?;
                    if self.is_str_value(res) {
                        return self.stringify_into(out, res, span, depth);
                    }
                    // Non-`str`: re-read the live rooted newtype before the default render
                    // (GC-safety; see the struct arm).
                    if let Obj::NewType { inner: cur, .. } = self.heap.get(h) {
                        inner = *cur;
                    }
                }
                let display = crate::compiler::bare_display(type_key.as_ref());
                let _ = write!(out, "{display}(");
                self.stringify_nested_into(out, inner, span, depth + 1)?;
                out.push(')');
            }
            Obj::Func { proto, .. } => {
                let _ = write!(out, "<fn {}>", self.program.protos[proto].name);
            }
            Obj::Closure { .. } => out.push_str("<closure>"),
            Obj::Module(m) => {
                let _ = write!(out, "<module {}>", m.name);
            }
            Obj::Native { name, .. } => {
                let _ = write!(out, "<native fn {name}>");
            }
            Obj::Builtin(name) => {
                let _ = write!(out, "<builtin fn {name}>");
            }
            Obj::Cffi(c) => {
                let _ = write!(out, "<extern fn {}>", c.name());
            }
            // Channel / Shared / Executor have no protocol hook — reuse the structural `Display`
            // (`stringify`'s catch-all falls back to `Display` too).
            Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Reader(_)
            | Obj::Ptr(_) => {
                out.push_str(&self.display_guarded(Value::obj(h), depth)?);
            }
            // Experimental generators stringify opaquely (no protocol hook).
            Obj::Generator(_) => out.push_str("<generator>"),
            // A cell is a transparent by-reference box (never a user-visible operand — reads
            // `CellLoad` first); defensively stringify its inner value.
            Obj::Cell(v) => self.stringify_into(out, v, span, depth)?,
            Obj::Iter { .. } => out.push_str("<iterator>"),
        }
        Ok(())
    }

    pub(super) fn stringify_seq_into(
        &mut self,
        out: &mut String,
        elems: &[Value],
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        for (i, e) in elems.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            self.stringify_nested_into(out, *e, span, depth)?;
        }
        Ok(())
    }
}
