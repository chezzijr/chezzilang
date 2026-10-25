//! VM: frame and module slots, globals, module definitions.
//! Split out of `src/vm/stmt.rs`. `super::*` is the parent module.

use super::*;

impl Vm {
    /// The slot index of module global `name`, for [`Vm::read_slot`]. **D1 invariant:** on a
    /// `--parallel` worker VM a module's globals are faulted in lazily, so any NEW caller that
    /// reads globals on a worker must call [`Vm::ensure_module_faulted`] for `module` first (the
    /// existing op/field/method read sites do); otherwise it may observe an empty, not-yet-faulted
    /// module and spuriously fail to resolve.
    pub(super) fn slot_index(&self, module: GcRef, name: &str) -> Option<u32> {
        match self.heap.get(module) {
            Obj::Module(m) => m.names().index.get(name).copied(),
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

    /// TICKET-186 — the one read of a module slot. It faults on the TICKET-183 uninit marker (a
    /// let that has not run yet); a raw `slots[..]` read elsewhere leaks the marker, which once
    /// reached the heap as an index (K10). The home module is always pre-sized before any read:
    /// the top-level engine sizes it from `global_slots` in `run_module`, and a worker faults it
    /// fully in (`fault_module`) before reading. So the index is always valid.
    pub(super) fn read_slot(
        &self,
        module: GcRef,
        slot: u32,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // TICKET-246 — a view the box could not seat for this read (see `ensure_module_faulted`).
        if !self.box_chain.is_empty()
            && let Some(e) = &self.box_view_err
        {
            return Err(self.err(e.message.clone(), span));
        }
        let v = match self.heap.get(module) {
            Obj::Module(m) => m.slots.get(slot as usize),
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
            | Obj::Generator(_) => Value::nil(),
        };
        if v.is_uninit() {
            return Err(self.uninit_read_err(module, slot, v, span));
        }
        Ok(v)
    }

    /// The name the module `index` gives compile-time slot `slot`, if any.
    pub(super) fn global_slot_name(&self, module: GcRef, slot: u32) -> Option<String> {
        match self.heap.get(module) {
            Obj::Module(m) => m
                .names()
                .index
                .iter()
                .find(|&(_, &i)| i == slot)
                .map(|(n, _)| n.to_string()),
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

    /// TICKET-183 — the fault for a read of a module global whose let has not run yet (`v` is the
    /// slot's `Value::uninit` marker, which carries the let's line). CPython's `NameError`.
    pub(super) fn uninit_read_err(
        &self,
        home: GcRef,
        slot: u32,
        v: Value,
        span: Span,
    ) -> RuntimeError {
        let name = self
            .global_slot_name(home, slot)
            .unwrap_or_else(|| format!("global#{slot}"));
        let line = v.uninit_line();
        self.err(
            format!("'{name}' is read before its initialization at line {line}"),
            span,
        )
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
        // TICKET-111 — the node registry describes the same snapshot; drop it too.
        self.snapshot_nodes = std::sync::Arc::new(super::fxhash::FxHashMap::default());
        if let Obj::Module(m) = self.heap.write(module).get(module) {
            m.slots.set(slot as usize, value);
        }
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
        // TICKET-111 — the node registry describes the same snapshot; drop it too.
        self.snapshot_nodes = std::sync::Arc::new(super::fxhash::FxHashMap::default());
        // A new name appends a slot: a push that outgrows the block retires it (`vm/rvec.rs`),
        // so a racy reader of the old block never reads freed memory. The index insert and the
        // push run under one write lock, so a locked reader sees them in lockstep.
        let heap = self.heap.write(module);
        if let Obj::Module(m) = heap.get(module) {
            let mut names = m.names_mut();
            match names.index.get(name) {
                Some(&i) => m.slots.set(i as usize, value),
                None => {
                    names.index.insert(name.into(), m.slots.len() as u32);
                    m.slots.push(heap, value);
                }
            }
        }
    }

    pub(super) fn module_name(&self, module: GcRef) -> String {
        match self.heap.get(module) {
            Obj::Module(m) => m.names().name.to_string(),
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
            | Obj::Generator(_) => String::new(),
        }
    }

    // ----- display / type names -----

    /// The text of the void value, in every display and in a fault's `type None`.
    pub(super) const NONE_TEXT: &str = "None";

    /// The fault for an iterator whose `next()` gave something that is not a `T?` value.
    pub(super) fn bad_next_value(&self, res: Value, span: Span) -> RuntimeError {
        self.err(
            format!(
                "iterator next() must return a `T?` value, found {}",
                self.type_name(res)
            ),
            span,
        )
    }
}
