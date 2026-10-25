//! TICKET-246 — a `Shared` / `RwShared` box keeps its value in its own context (`stack[0]` of a
//! `FiberCtx`, with its own module view). M1.7: the context's objects live in the run's one heap.
//! The collector reaches them through the box, as Go reaches a value through the struct that holds
//! it: marking an `Obj::Shared` (or a `WireValue::Shared` in a core's payload) walks the parked
//! context (`core::SharedState::roots_into`). The caller a box call parks is rooted by the `Vm`
//! that runs the call (`BoxFrame::roots`). A box crossing still copies, into that one heap. This
//! module owns the three things that make a closure run inside a box context:
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
    /// acts as the caller) it holds the box context instead. `BoxFrame::roots` roots whichever of
    /// the two it holds.
    caller: Box<FiberCtx>,
    /// The caller's running generators and their parked hosts. `FiberCtx` does not carry them,
    /// so they park here, and `Vm::gc_roots` roots them.
    gens: Vec<GcRef>,
    gen_hosts: Vec<GenCtx>,
    /// This call already asked its caller for a fresh view (see [`Vm::box_seat_view`]).
    view_checked: bool,
    /// Globals this call read through the slot door: GC roots (`Vm::gc_roots`).
    slots: fxhash::FxHashMap<(u32, u32), Value>,
}

impl BoxFrame {
    /// Append the GC roots this frame holds: the parked context, the slot-door cache and the
    /// parked generators.
    pub(super) fn roots(&self, out: &mut Vec<GcRef>) {
        self.caller.root_view().walk(out);
        out.extend(self.slots.values().filter_map(|v| v.child_gcref()));
        out.extend(self.gens.iter().copied());
        for host in &self.gen_hosts {
            host.root_view().walk(out);
        }
    }
}

/// The values a module slot can hand to another heap alone, with no view: an immutable leaf
/// (`Fixed::Leaf`), a `fn` item or an `Arc` core. `Vm::slot_snapshot_reusable` reads the same
/// list, so "safe to reuse without a rebuild" and "safe to copy without a view" cannot drift.
pub(super) fn slot_is_leaf(heap: &Heap, v: Value) -> bool {
    let Some(h) = v.as_obj() else {
        return true; // inline scalar (int/bool/nil) — immutable
    };
    let o = heap.get(h);
    fixed_after_build(o) == Some(Fixed::Leaf) || is_arc_core(o)
}

/// The two kinds of object no write changes in place after it is built (TICKET-295).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Fixed {
    /// Holds no value: a `str`, bytes, a big int, a boxed float, a pointer, a `fn` item.
    Leaf,
    /// Holds values that can still be mutable: a tuple, an enum value.
    Holder,
}

/// The ONE list of immutable kinds (TICKET-295): `Some` when no write changes `o` in place after
/// it is built. A `Holder`'s children can still be mutable, so a caller that asks "can a write
/// reach it" walks them too. `slot_is_leaf`, `WireMemo::sole_captures` and `Vm::unmark_graph`
/// read it.
pub(super) fn fixed_after_build(o: &Obj) -> Option<Fixed> {
    match o {
        Obj::Str(_)
        | Obj::Bytes(_)
        | Obj::BigInt(_)
        | Obj::FloatBox(_)
        | Obj::Ptr(_)
        | Obj::Func { .. }
        | Obj::Native { .. }
        | Obj::Builtin(_) => Some(Fixed::Leaf),
        Obj::Tuple(_) | Obj::Enum { .. } => Some(Fixed::Holder),
        Obj::ByteArray(_)
        | Obj::Iter { .. }
        | Obj::List(_)
        | Obj::Map(_)
        | Obj::Set(_)
        | Obj::Struct { .. }
        | Obj::Cell(_)
        | Obj::Closure { .. }
        | Obj::Module(_)
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
        | Obj::Generator(_) => None,
    }
}

/// Whether `o` is a handle on an `Arc` core: its state lives outside every heap, so a copy of
/// the handle shares it.
pub(super) fn is_arc_core(o: &Obj) -> bool {
    match o {
        Obj::Cffi(_)
        | Obj::Channel(_)
        | Obj::Shared(_)
        | Obj::RwShared(_)
        | Obj::Atomic(_)
        | Obj::AtomicInt(_)
        | Obj::Executor(_)
        | Obj::Socket(_)
        | Obj::Listener(_)
        | Obj::Writer(_)
        | Obj::Reader(_) => true,
        Obj::Str(_)
        | Obj::Bytes(_)
        | Obj::BigInt(_)
        | Obj::FloatBox(_)
        | Obj::Ptr(_)
        | Obj::Func { .. }
        | Obj::Native { .. }
        | Obj::Builtin(_)
        | Obj::Tuple(_)
        | Obj::Enum { .. }
        | Obj::ByteArray(_)
        | Obj::Iter { .. }
        | Obj::List(_)
        | Obj::Map(_)
        | Obj::Set(_)
        | Obj::Struct { .. }
        | Obj::Cell(_)
        | Obj::Closure { .. }
        | Obj::Module(_)
        | Obj::Generator(_) => false,
    }
}

/// The pieces of one crossing out of (or into) a box, encoded with one memo. With `prelude` the
/// first wire is the whole state, which the pieces back-reference; the decoder drops it.
pub(super) struct BoxPieces {
    prelude: bool,
    ws: Vec<WireValue>,
}

/// The state object of a parked box context, when the state is a heap value. `heap` is any
/// handle of the run: the state lives in the run's one heap.
pub(super) fn state_obj<'a>(heap: &'a Heap, ctx: &FiberCtx) -> Option<&'a Obj> {
    ctx.stack[0].as_obj().map(|h| heap.get(h))
}

impl Vm {
    /// A fresh box context: an EMPTY module view (seated lazily, per call, from the caller — see
    /// [`Vm::box_global`]), its own view stamp, and the state slot `stack[0]`, which the collector
    /// reaches through the box.
    pub(super) fn box_new(&mut self) -> Box<FiberCtx> {
        let mut worker = self.spawn_worker();
        Box::new(FiberCtx {
            view: worker.heap.take_view(),
            owns_side: true,
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
    /// `FiberCtx` holds; this adds the running generators, which belong to the context that runs
    /// them. `out` / `stderr` do not move (`swap_ctx` leaves them to
    /// [`Vm::swap_task`]): a box context owns no output stream, so a `print` inside a box closure
    /// is the caller's output.
    fn swap_frame(&mut self, frame: &mut BoxFrame) {
        self.swap_ctx(&mut frame.caller);
        std::mem::swap(&mut self.active_generators, &mut frame.gens);
        std::mem::swap(&mut self.gen_host_ctx, &mut frame.gen_hosts);
    }

    /// Run `f` inside the box context in `slot`, with the caller parked on `box_chain`. The ONLY
    /// swap into a box. Everything is undone on return AND on unwind: a Rust panic inside `f`
    /// leaves the `Vm` on the caller's heap, the context back in `slot` and the chain as it was.
    pub(super) fn with_box<R>(
        &mut self,
        slot: &mut core::BoxSlot,
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
            let wm = self.heap.alloc(Obj::Module(Box::new(ModuleData::new(
                "".into(),
                RVec::new(),
                std::collections::HashMap::new(),
            ))));
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
    /// While `f` runs, the frame waits on `box_lifted`, where `Vm::gc_roots` still roots it.
    pub(super) fn as_box_caller<R>(&mut self, f: impl FnOnce(&mut Vm) -> R) -> R {
        let mut frame = self.box_chain.pop().expect("a box call is running");
        let depth = self.box_chain.len();
        self.swap_frame(&mut frame);
        let lifted = self.box_lifted.len();
        self.box_lifted.push(frame);
        let r = catch_unwind(AssertUnwindSafe(|| f(self)));
        self.box_chain.truncate(depth);
        self.box_lifted.truncate(lifted + 1);
        let mut frame = self.box_lifted.pop().expect("this call's lifted frame");
        self.swap_frame(&mut frame);
        self.box_chain.push(frame);
        match r {
            Ok(v) => v,
            Err(p) => resume_unwind(p),
        }
    }

    /// A new `Shared` core holding `init`, decoded into the box's context.
    pub(super) fn new_shared_core(&mut self, init: WireValue) -> Arc<SharedCore> {
        let mut slot = Some(self.box_new());
        self.box_store(&mut slot, init);
        Arc::new(SharedCore {
            state: core::SharedState(Mutex::new(slot)),
        })
    }

    /// Replace the state of the box in `slot` with `w` (construction, `set`).
    pub(super) fn box_store(&mut self, slot: &mut core::BoxSlot, w: WireValue) {
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

    /// Runs INSIDE a box, at the end of a write: collect when the run is due (a box whose users
    /// only `set` run no bytecode of the box, so its dispatch loop would not check). M1.7: the
    /// collection is the run's, with the world stopped; the box state is live in this `Vm`, and
    /// its objects count in the run's `live_bytes`.
    fn box_settle(&mut self) {
        if self.heap.should_collect() {
            self.gc_collect();
        }
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
        slot: &mut core::BoxSlot,
        fw: WireValue,
        write_back: bool,
        span: Span,
    ) -> Result<Option<WireValue>, RuntimeError> {
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
                    // What the box keeps after the call decides which captures of a closure in
                    // the result are the only live copy (`WireMemo::sole_captures`).
                    let mut memo = match v.as_obj() {
                        Some(_) => airlock_out::WireMemo::box_result(vm.gc_roots()),
                        None => airlock_out::WireMemo::default(),
                    };
                    vm.walk_src()
                        .to_wire_crossable_memo(v, span, &mut memo)
                        .map(Some)
                }
            });
            vm.heap.unmark_copy_log();
            vm.box_settle();
            r
        })
    }

    /// Copy the state out of a parked box context, for this `Vm`'s handle. The walk only reads
    /// the box's objects; the reader charges its OWN handle.
    pub(super) fn box_copy_out(
        &self,
        ctx: &FiberCtx,
        span: Span,
    ) -> Result<WireValue, RuntimeError> {
        let src = airlock_out::WalkSrc::of_ctx(ctx, &self.heap, &self.program, self.walk_base);
        let w =
            src.to_wire_crossable_memo(ctx.stack[0], span, &mut airlock_out::WireMemo::default())?;
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
        // Fast path, as `rwshared_read`: when nobody is in the box the state lock is free, and a
        // copy needs no registry lock (measured: a small `get` with the guard cost 3.3 us against
        // 1.5-1.9 us on the encoded store). A busy lock means a closure runs in the box, or
        // another copy is being taken; only then the wait is registered through the guard.
        if let Ok(st) = core.state.0.try_lock()
            && let Some(ctx) = st.as_ref()
        {
            return self.box_copy_out(ctx, span);
        }
        let key = Arc::as_ptr(core) as usize;
        let _guard = self.take_update_guard(key, "a Shared update guard", span)?;
        let st = core.state.lock();
        let ctx = st
            .as_ref()
            .expect("a Shared state is in its slot under the guard");
        self.box_copy_out(ctx, span)
    }

    /// Encode the closure argument `f` of the native method `owner.method` for another heap. The
    /// ONE function a native arm calls to cross a closure: it reads
    /// [`CROSSING_CLOSURE_METHODS`], the list the checker's capture floor reads, so a native arm
    /// that crosses a closure with no row panics in a debug build the first time it runs.
    pub(super) fn cross_closure(
        &mut self,
        owner: &'static str,
        method: &'static str,
        f: Value,
        span: Span,
    ) -> Result<WireValue, RuntimeError> {
        debug_assert!(
            CROSSING_CLOSURE_METHODS.contains(&(owner, method)),
            "{owner}.{method} crosses a closure but has no row in CROSSING_CLOSURE_METHODS"
        );
        self.to_wire_crossable(f, span)
    }

    /// A new `RwShared` core holding `init`, decoded into the box's context.
    pub(super) fn new_rwshared_core(&mut self, init: WireValue) -> Arc<RwSharedCore> {
        let mut slot = Some(self.box_new());
        self.box_store(&mut slot, init);
        Arc::new(RwSharedCore {
            state: core::RwSharedState(std::sync::RwLock::new(slot)),
        })
    }

    /// The ONE reader of a `RwShared` state: run `pick` on the parked box context under the read
    /// lock. `pick` only copies (`&Vm`, `&FiberCtx`): it runs no user code and writes nothing
    /// into the box heap, which is what makes many readers at once sound.
    ///
    /// Fast path: `try_read`, with no registry lock (the 8-reader cell). It fails only when a
    /// writer holds or waits for the state; the reader then takes the update guard, so its wait
    /// is a registered guard wait (deadlock detection, cancellation) and a reader inside the
    /// box's OWN `write` closure faults with `already holds the update guard`. Under the guard no
    /// writer can hold the lock, so the blocking `read` waits at most for another reader's copy.
    pub(super) fn rwshared_read<R>(
        &mut self,
        core: &Arc<RwSharedCore>,
        span: Span,
        pick: impl FnOnce(&Vm, &FiberCtx) -> Result<R, RuntimeError>,
    ) -> Result<R, RuntimeError> {
        if let Some(st) = core.state.try_read() {
            let ctx = st.as_ref().expect("a RwShared state is in its slot");
            return pick(self, ctx);
        }
        let key = Arc::as_ptr(core) as usize;
        let _guard = self.take_update_guard(key, "a RwShared update guard", span)?;
        let st = core.state.read();
        let ctx = st.as_ref().expect("a RwShared state is in its slot");
        pick(self, ctx)
    }

    /// The ONE single-entry writer of a `RwShared` container: update guard, write lock, then
    /// `op` runs INSIDE the box on the live state with `args` decoded into the box heap (one
    /// memo, so a value that aliases its key stays one object). `op` is the plain `Map` / `Set`
    /// / `List` operation; a value it returns is encoded for the caller.
    pub(super) fn rwshared_mutate(
        &mut self,
        core: &Arc<RwSharedCore>,
        args: &[Value],
        span: Span,
        op: impl FnOnce(&mut Vm, Value, &[Value]) -> Result<Option<Value>, RuntimeError>,
    ) -> Result<Option<WireValue>, RuntimeError> {
        let mut memo = airlock_out::WireMemo::default();
        let mut ws = Vec::with_capacity(args.len());
        for &a in args {
            ws.push(self.to_wire_crossable_memo(a, span, &mut memo)?);
        }
        let key = Arc::as_ptr(core) as usize;
        let _guard = self.take_update_guard(key, "a RwShared update guard", span)?;
        let mut st = core.state.write();
        self.with_box(&mut st, |vm| {
            // Handed to the box: it owns these copies (see `box_store`).
            vm.heap.begin_copy_log();
            let vals = vm.from_wire_pieces(BoxPieces { prelude: false, ws });
            vm.heap.unmark_copy_log();
            // Rooted across a key's `eq` hook; `with_box` truncates the stack on the way out.
            vm.stack.extend_from_slice(&vals);
            let state = vm.stack[0];
            let r = op(vm, state, &vals).and_then(|out| {
                out.map(|v| {
                    vm.walk_src().to_wire_crossable_memo(
                        v,
                        span,
                        &mut airlock_out::WireMemo::default(),
                    )
                })
                .transpose()
            });
            vm.box_settle();
            r
        })
    }

    /// Copy `vals` (values of the parked box context `ctx`) out for this `Vm`'s heap, as ONE
    /// crossing: one memo, so two pieces that alias one node decode as one object.
    ///
    /// A piece that cannot be encoded alone (a generator whose closure reaches the root
    /// container, W7-11) is encoded against the WHOLE state instead: the state goes first, as a
    /// prelude the decoder rebuilds and drops, and the pieces resolve into it. That is CPython's
    /// `copy.deepcopy(xs[0])`, which copies the container too.
    pub(super) fn box_copy_pieces(
        &self,
        ctx: &FiberCtx,
        vals: &[Value],
        span: Span,
    ) -> Result<BoxPieces, RuntimeError> {
        let src = airlock_out::WalkSrc::of_ctx(ctx, &self.heap, &self.program, self.walk_base);
        let encode = |prelude: Option<Value>| -> Result<BoxPieces, RuntimeError> {
            let mut memo = airlock_out::WireMemo::default();
            let mut ws = Vec::with_capacity(vals.len() + 1);
            for &v in prelude.iter().chain(vals) {
                ws.push(src.to_wire_crossable_memo(v, span, &mut memo)?);
            }
            Ok(BoxPieces {
                prelude: prelude.is_some(),
                ws,
            })
        };
        let out = encode(None).or_else(|_| encode(Some(ctx.stack[0])))?;
        for w in &out.ws {
            self.charge_wire(w);
        }
        Ok(out)
    }

    /// Decode the pieces of one crossing ([`box_copy_pieces`](Vm::box_copy_pieces)) with ONE
    /// rebuild map, in order. A hand-off: it never marks.
    #[allow(clippy::wrong_self_convention)]
    pub(super) fn from_wire_pieces(&mut self, pieces: BoxPieces) -> Vec<Value> {
        let mut rebuild = fxhash::FxHashMap::<u32, GcRef>::default();
        let saved = self.copy_mark;
        self.copy_mark = crossing::marks(crossing::Route::Handoff, saved);
        let mut vals: Vec<Value> = Vec::with_capacity(pieces.ws.len());
        for w in pieces.ws {
            vals.push(self.from_wire_memo(w, &mut rebuild));
        }
        self.copy_mark = saved;
        if pieces.prelude {
            vals.remove(0);
        }
        vals
    }

    /// `"list"` / `"map"` / `"set"` for a container value of the live heap, else `""`.
    pub(super) fn elem_kind(&self, v: Value) -> &'static str {
        match v.as_obj().map(|h| self.heap.get(h)) {
            Some(Obj::List(_)) => "list",
            Some(Obj::Map(_)) => "map",
            Some(Obj::Set(_)) => "set",
            _ => "",
        }
    }

    /// How many nurseries this task has open BELOW the context now running: the parked callers'
    /// stacks, summed over `box_chain`. `0` outside a box call. A nursery index a halt carries is
    /// this base plus the index in its own context's `nurseries`, so one index names one nursery
    /// of the task at every box depth.
    pub(super) fn nursery_base(&self) -> usize {
        self.box_chain
            .iter()
            .map(|f| f.caller.nurseries.len())
            .sum()
    }

    /// THE eager-scope stacks of this party, outermost first: each parked caller's on
    /// `box_chain`, then this context's, each with its task-wide nursery base (see
    /// [`Vm::nursery_base`]). A task's nurseries belong to the task, not to the heap its code runs
    /// in. Every `Vm` reader that asks "which nurseries does this task have open" reads this
    /// through [`Vm::owned_nurseries`], [`Vm::owned_eager_scopes`] or [`Vm::owns_nursery`]. The
    /// other uses of `eager_scheds` push, pop or index THIS context's stack in lockstep with
    /// `nurseries` (enter, join, abort, `spawn` into the innermost nursery).
    fn eager_stacks(
        &self,
    ) -> impl DoubleEndedIterator<Item = (usize, &[Option<super::EagerScope>])> {
        let below = |i: usize| -> usize {
            self.box_chain[..i]
                .iter()
                .map(|f| f.caller.nurseries.len())
                .sum()
        };
        self.box_chain
            .iter()
            .enumerate()
            .map(move |(i, f)| (below(i), f.caller.eager_scheds.as_slice()))
            .chain(std::iter::once((
                self.nursery_base(),
                self.eager_scheds.as_slice(),
            )))
    }

    /// TICKET-188 — does this party own an open nursery? The gate of the owned halt read.
    #[inline]
    pub(super) fn owns_nursery(&self) -> bool {
        !self.eager_scheds.is_empty() || self.eager_stacks().any(|(_, e)| !e.is_empty())
    }

    /// TICKET-188 — THE open nurseries of this party, outermost first. Inside a box call the
    /// callers' nurseries are still this party's, so a child's fault cuts an owner that waits
    /// inside a box closure. `n` is task-wide (see [`Vm::nursery_base`]).
    pub(super) fn owned_nurseries(&self) -> impl DoubleEndedIterator<Item = block::OwnedRef<'_>> {
        self.eager_stacks().flat_map(|(base, eager)| {
            super::open_nurseries(eager).map(move |mut o| {
                o.n += base;
                o
            })
        })
    }

    /// The eager scopes behind [`Vm::owned_nurseries`], outermost first: the bodies a block of
    /// this thread marks blocked, and the enclosing sched a nested `parallel:` joins.
    pub(super) fn owned_eager_scopes(&self) -> impl DoubleEndedIterator<Item = &super::EagerScope> {
        self.eager_stacks()
            .flat_map(|(_, eager)| eager.iter().flatten())
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
            Obj::Module(md) if own_is_current => {
                md.slots.try_get(slot as usize).filter(|v| !v.is_uninit())
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
            if let Obj::Module(md) = self.heap.write(m).get(m) {
                let mut names = md.names_mut();
                md.slots.clear();
                names.index.clear();
                if let Some(sm) = snap.modules.get(i) {
                    names.name = sm.name.clone();
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
