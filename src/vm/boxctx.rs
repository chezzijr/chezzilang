//! TICKET-246 — a `Shared` / `RwShared` box keeps its value in a private heap. This module owns
//! the three things that make a closure run inside that heap:
//!
//! - [`Vm::with_box`], the ONE swap into a box context (and its undo, on return and on unwind);
//! - `Vm::box_chain` with [`Vm::as_box_caller`], the ONE definition of "the caller of this box
//!   call", at any nesting depth;
//! - [`Vm::box_global`], the ONE door a box closure reads a module global through.

use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

/// One open box call. The LAST frame of `Vm::box_chain` describes the call now running; frame 0
/// parks the root task. A box call made from inside a box closure pushes one more frame.
pub(super) struct BoxFrame {
    /// The parked caller while the call runs. Inside [`Vm::swap_frame`]'s second half (the `Vm`
    /// acts as the caller) it holds the box context instead.
    caller: Box<FiberCtx>,
    /// The caller's running generators and their parked hosts. They index the caller's heap and
    /// `FiberCtx` does not carry them, so they park here.
    gens: Vec<GcRef>,
    gen_hosts: Vec<GenCtx>,
    /// This call already asked its caller for a fresh view (see [`Vm::box_seat_view`]).
    view_checked: bool,
    /// Globals this call read through the slot door. The values index the LIVE (box) heap, so the
    /// last frame's are GC roots; a frame below the last indexes a parked heap and is not.
    slots: fxhash::FxHashMap<(u32, u32), Value>,
}

impl BoxFrame {
    /// GC roots of the call now running.
    pub(super) fn roots(&self) -> impl Iterator<Item = GcRef> + '_ {
        self.slots.values().filter_map(|v| v.child_gcref())
    }
}

/// The values a module slot can hand to another heap alone, with no view: an immutable leaf, a
/// `fn` item or an `Arc` core. `Vm::slot_snapshot_reusable` reads the same list, so "safe to
/// reuse without a rebuild" and "safe to copy without a view" cannot drift.
pub(super) fn slot_is_leaf(heap: &Heap, v: Value) -> bool {
    let Some(h) = v.as_obj() else {
        return true; // inline scalar (int/bool/nil) — immutable
    };
    matches!(
        heap.get(h),
        Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Ptr(_)
            | Obj::Func { .. }
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
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
    )
}

impl Vm {
    /// A fresh box context: its own heap, an EMPTY module view (seated lazily, per call, from the
    /// caller — see [`Vm::box_global`]) and the state slot `stack[0]`, which that heap's own GC
    /// roots.
    pub(super) fn box_new(&mut self) -> Box<FiberCtx> {
        let worker = self.spawn_worker();
        Box::new(FiberCtx {
            heap: Some(worker.heap),
            stack: vec![Value::nil()],
            module_objs: worker.module_objs,
            module_faulted: worker.module_faulted,
            module_snapshot: worker.module_snapshot,
            snapshot_memo: worker.snapshot_memo,
            snapshot_rebuild: worker.snapshot_rebuild,
            snapshot_cells: worker.snapshot_cells,
            snapshot_nodes: worker.snapshot_nodes,
            snapshot_adopt: worker.snapshot_adopt,
            snapshot_next_id: worker.snapshot_next_id,
            executors: worker.executors,
            str_intern: worker.str_intern,
            fn_values: worker.fn_values,
            ..Default::default()
        })
    }

    /// Swap the live context with `frame.caller`, in either direction. `swap_ctx` moves what
    /// `FiberCtx` holds; this adds the rest of the rule "swap what indexes a heap": the running
    /// generators travel with their heap, and `out` / `stderr` swap BACK, because a `print`
    /// inside a box closure is the caller's output.
    fn swap_frame(&mut self, frame: &mut BoxFrame) {
        self.swap_ctx(&mut frame.caller);
        std::mem::swap(&mut self.out, &mut frame.caller.out);
        std::mem::swap(&mut self.stderr, &mut frame.caller.stderr);
        std::mem::swap(&mut self.active_generators, &mut frame.gens);
        std::mem::swap(&mut self.gen_host_ctx, &mut frame.gen_hosts);
    }

    /// Run `f` inside the box context in `slot`, with the caller parked on `box_chain`. The ONLY
    /// swap into a box. Everything is undone on return AND on unwind: a Rust panic inside `f`
    /// leaves the `Vm` on the caller's heap, the context back in `slot` and the chain as it was.
    pub(super) fn with_box<R>(
        &mut self,
        slot: &mut Option<Box<FiberCtx>>,
        f: impl FnOnce(&mut Vm) -> R,
    ) -> R {
        let ctx = slot
            .take()
            .expect("a box context is in its slot under the box lock");
        let n_mods = self.module_objs.len();
        let mut frame = BoxFrame {
            caller: ctx,
            gens: Vec::new(),
            gen_hosts: Vec::new(),
            view_checked: false,
            slots: fxhash::FxHashMap::default(),
        };
        self.swap_frame(&mut frame);
        // The box addresses the caller's modules by index, so its table is as long as theirs. A
        // module object stays for the box's life: the state can hold a closure whose home it is.
        while self.module_objs.len() < n_mods {
            let wm = self.heap.alloc(Obj::Module(Box::new(ModuleData {
                name: "".into(),
                slots: Vec::new(),
                index: std::collections::HashMap::new(),
            })));
            self.module_objs.push(wm);
            self.module_faulted.push(false);
        }
        let depth = self.box_chain.len();
        let entry = (
            self.frames.len(),
            self.stack.len(),
            self.call_depth,
            self.cur_base,
            self.handlers.len(),
        );
        self.box_chain.push(frame);
        let r = catch_unwind(AssertUnwindSafe(|| f(self)));
        self.frames.truncate(entry.0);
        self.stack.truncate(entry.1);
        self.call_depth = entry.2;
        self.cur_base = entry.3;
        self.handlers.truncate(entry.4);
        self.box_view_err = None;
        self.box_chain.truncate(depth + 1);
        let mut frame = self.box_chain.pop().expect("this call's frame");
        self.swap_frame(&mut frame);
        *slot = Some(frame.caller);
        match r {
            Ok(v) => v,
            Err(p) => resume_unwind(p),
        }
    }

    /// Run `f` AS the caller of the box call now running: the `Vm` is the caller's context and
    /// `box_chain` is the caller's own chain while `f` runs. The ONE way to reach the caller; a
    /// caller that is itself a box context answers through its own chain, so a nested call has no
    /// second code path. Undone on return and on unwind.
    pub(super) fn as_box_caller<R>(&mut self, f: impl FnOnce(&mut Vm) -> R) -> R {
        let mut frame = self.box_chain.pop().expect("a box call is running");
        let depth = self.box_chain.len();
        self.swap_frame(&mut frame);
        let r = catch_unwind(AssertUnwindSafe(|| f(self)));
        self.box_chain.truncate(depth);
        self.swap_frame(&mut frame);
        self.box_chain.push(frame);
        match r {
            Ok(v) => v,
            Err(p) => resume_unwind(p),
        }
    }

    /// A new `Shared` core holding `init`, decoded into the box's own heap.
    pub(super) fn new_shared_core(&mut self, init: WireValue) -> Arc<SharedCore> {
        let mut slot = Some(self.box_new());
        let bytes = self.box_store(&mut slot, init);
        Arc::new(SharedCore {
            state: core::SharedState(Mutex::new(slot)),
            bytes: std::sync::atomic::AtomicUsize::new(bytes),
        })
    }

    /// Replace the state of the box in `slot` with `w` (construction, `set`). Returns the box
    /// heap's byte estimate for the core's `bytes`.
    pub(super) fn box_store(&mut self, slot: &mut Option<Box<FiberCtx>>, w: WireValue) -> usize {
        self.with_box(slot, |vm| {
            // A value stored into the box is handed to it: the box owns the copy, so the marks
            // the rebuild makes for a closure from another heap are cleared at once.
            vm.heap.begin_copy_log();
            let v = vm.from_wire(w);
            vm.heap.unmark_copy_log();
            vm.stack[0] = v;
            vm.box_settle()
        })
    }

    /// The heap id of the caller of the box call now running (its heap is parked on the chain).
    fn box_caller_heap_id(&self) -> u64 {
        self.box_chain
            .last()
            .and_then(|f| f.caller.heap.as_ref())
            .map_or(self.heap.id(), |h| h.id())
    }

    /// Runs INSIDE a box, at the end of a write: collect when the box heap is due (a box whose
    /// users only `set` runs no bytecode of its own, so nothing else would), then report its
    /// bytes. The box heap is collected only here and in the closure's own dispatch loop, both
    /// under the exclusive lock.
    fn box_settle(&mut self) -> usize {
        if self.heap.should_collect() {
            self.collect();
        }
        self.heap.approx_bytes()
    }

    /// Run the encoded closure `fw` inside the box in `slot`, on the live state. With
    /// `write_back` the closure's result becomes the state (`update` / `write`) and `Ok(None)`
    /// returns; without it the result is encoded for the caller (`read`). A fault leaves the state
    /// as the closure left it.
    ///
    /// The closure's captures are copies marked as such (DEC-189), so a write to one is the D4
    /// fault. Those marks are cleared when the call ends: a capture the closure stored into the
    /// state is the box's own value from then on.
    pub(super) fn box_call(
        &mut self,
        slot: &mut Option<Box<FiberCtx>>,
        fw: WireValue,
        write_back: bool,
        span: Span,
    ) -> (Result<Option<WireValue>, RuntimeError>, usize) {
        self.with_box(slot, |vm| {
            vm.heap.begin_copy_log();
            let f = vm.from_wire(fw);
            vm.heap.end_copy_log();
            vm.stack.push(f);
            let state = vm.stack[0];
            let r = vm.reentered(|vm| vm.invoke_value(f, vec![state], span));
            // A view that could not be seated at a site with no fault channel surfaces here at
            // the latest.
            let r = match (r, vm.box_view_err.take()) {
                (Ok(_), Some(e)) => Err(e),
                (r, _) => r,
            };
            let r = r.and_then(|v| {
                if write_back {
                    vm.stack[0] = v;
                    Ok(None)
                } else {
                    // The result is handed to the caller, so it carries the CALLER's stamp.
                    let mut src = vm.walk_src();
                    src.origin = vm.box_caller_heap_id();
                    src.to_wire_crossable_memo(v, span, &mut sched::WireMemo::default())
                        .map(Some)
                }
            });
            vm.heap.unmark_copy_log();
            (r, vm.box_settle())
        })
    }

    /// Copy the state out of a parked box context, for this `Vm`'s heap. The walk only borrows
    /// the box heap; the reader charges its OWN heap.
    pub(super) fn box_copy_out(
        &self,
        ctx: &FiberCtx,
        span: Span,
    ) -> Result<WireValue, RuntimeError> {
        let src = sched::WalkSrc::of_ctx(ctx, &self.program, self.walk_base, self.heap.id());
        let w = src.to_wire_crossable_memo(ctx.stack[0], span, &mut sched::WireMemo::default())?;
        self.charge_wire(&w);
        Ok(w)
    }

    /// The whole value of a `Shared`, encoded: update guard, state lock, copy, unlock. The task
    /// that holds the guard (inside the box's own `update` / `read` closure) gets the
    /// `already holds the update guard` fault instead of a self-deadlock.
    pub(super) fn shared_copy_out(
        &mut self,
        core: &Arc<SharedCore>,
        span: Span,
    ) -> Result<WireValue, RuntimeError> {
        let key = Arc::as_ptr(core) as usize;
        let _guard = self.take_update_guard(key, "a Shared update guard", span)?;
        let st = core.state.lock();
        let ctx = st
            .as_ref()
            .expect("a Shared state is in its slot under the guard");
        self.box_copy_out(ctx, span)
    }

    /// "Is this view a task's copy?" — a write to a module global faults there (D4). True for a
    /// spawned task and inside a box closure.
    pub(super) fn is_task_view(&self) -> bool {
        self.module_snapshot.is_some() || !self.box_chain.is_empty()
    }

    /// The one door a box closure reads a module global through. A no-op (`Ok(None)`) outside a
    /// box call and for a `home` that is not a module of this view.
    ///
    /// Slot half (`slot` given): a slot holding a [`slot_is_leaf`] value is copied alone from the
    /// caller, with no view, and returned. View half (otherwise): the caller's `fresh_view` is
    /// seated once per call and `Ok(None)` tells the reader to fault the module in as usual.
    pub(super) fn box_global(
        &mut self,
        home: GcRef,
        slot: Option<u32>,
        span: Span,
    ) -> Result<Option<Value>, RuntimeError> {
        if self.box_chain.is_empty() {
            return Ok(None);
        }
        let Some(idx) = self.home_index(home) else {
            return Ok(None);
        };
        if let Some(slot) = slot
            && let Some(v) = self.box_slot(idx, slot, span)
        {
            return Ok(Some(v));
        }
        self.box_seat_view(span)?;
        Ok(None)
    }

    /// The slot half of [`box_global`](Vm::box_global), cached per call.
    fn box_slot(&mut self, idx: usize, slot: u32, span: Span) -> Option<Value> {
        let key = (idx as u32, slot);
        let frame = self.box_chain.last()?;
        if let Some(&v) = frame.slots.get(&key) {
            return Some(v);
        }
        let w = self.as_box_caller(|vm| vm.box_slot_here(idx, slot, span))?;
        let v = self.from_wire(w);
        if let Some(frame) = self.box_chain.last_mut() {
            frame.slots.insert(key, v);
        }
        Some(v)
    }

    /// Runs IN the caller: global `(idx, slot)` encoded for the box, when it is a leaf. A caller
    /// that is itself a box context trusts its own module copy only once THIS call of it seated
    /// its view; before that the copy can be an earlier call's, so the read resolves through that
    /// context's own caller first, at any depth, and ends at the root task's live slot.
    fn box_slot_here(&mut self, idx: usize, slot: u32, span: Span) -> Option<WireValue> {
        let m = *self.module_objs.get(idx)?;
        let own_is_current = self.box_chain.last().is_none_or(|f| f.view_checked);
        let direct = match self.heap.get(m) {
            Obj::Module(md) if own_is_current => md
                .slots
                .get(slot as usize)
                .copied()
                .filter(|v| !v.is_uninit()),
            _ => None,
        };
        let v = match direct {
            Some(v) => v,
            None => self.box_slot(idx, slot, span)?,
        };
        if !slot_is_leaf(&self.heap, v) {
            return None;
        }
        self.to_wire_crossable(v, span).ok()
    }

    /// The view half of [`box_global`](Vm::box_global): once per box call, ask the caller for its
    /// `fresh_view` and, when it is not the view this box already holds, re-seat it IN PLACE. A
    /// no-op outside a box call. `fresh_view` itself begins with this, so a caller that is a box
    /// context seats its OWN view from ITS caller before it snapshots.
    pub(super) fn box_seat_view(&mut self, span: Span) -> Result<(), RuntimeError> {
        match self.box_chain.last_mut() {
            Some(frame) if !frame.view_checked => frame.view_checked = true,
            _ => return Ok(()),
        }
        let snap = match self.as_box_caller(|vm| vm.fresh_view(span)) {
            Ok(snap) => snap,
            Err(e) => {
                // Not seated: the next read asks again and raises the same fault.
                if let Some(frame) = self.box_chain.last_mut() {
                    frame.view_checked = false;
                }
                return Err(e);
            }
        };
        if self
            .module_snapshot
            .as_ref()
            .is_some_and(|m| Arc::ptr_eq(m, &snap))
        {
            return Ok(());
        }
        for i in 0..self.module_objs.len() {
            let m = self.module_objs[i];
            if let Obj::Module(md) = self.heap.get_mut(m) {
                md.slots.clear();
                md.index.clear();
                if let Some(sm) = snap.modules.get(i) {
                    md.name = sm.name.clone();
                }
            }
        }
        self.module_faulted = vec![false; self.module_objs.len()];
        self.snapshot_rebuild.clear();
        self.snapshot_cells = Arc::new(fxhash::FxHashMap::default());
        self.snapshot_nodes = Arc::new(fxhash::FxHashMap::default());
        self.snapshot_adopt.clear();
        // A cached `Func` has the old view's home; a slot-door value may be one.
        self.fn_values.clear();
        if let Some(frame) = self.box_chain.last_mut() {
            frame.slots.clear();
        }
        self.snapshot_memo = Some(Arc::clone(&snap));
        self.module_snapshot = Some(snap);
        Ok(())
    }
}
