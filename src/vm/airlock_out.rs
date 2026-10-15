//! VM: the airlock outbound: `start_task`, deep clone, `to_wire` and the `WalkSrc` walker.
//! Split out of `src/vm/sched.rs`. `super::*` is the parent module.

use super::wire::{WireMap, WireSet};
use super::*;

impl Vm {
    /// The copy walk's source for this `Vm`'s own heap.
    pub(super) fn walk_src(&self) -> WalkSrc<'_> {
        WalkSrc {
            heap: &self.heap,
            module_objs: &self.module_objs,
            active_generators: &self.active_generators,
            program: &self.program,
            walk_base: self.walk_base,
            origin: self.heap.id(),
        }
    }

    /// Launch a fiber's initial task in the (already swapped-in) child context: push the callee's
    /// frame and return; the scheduler then runs it. The callee always pushes a frame (TICKET-235,
    /// [`Vm::task_entry`]), so a call that waits inside the task has an `ip` to rewind.
    pub(super) fn start_task(&mut self, task: PendingCall) -> Result<(), RuntimeError> {
        match task {
            PendingCall::Call { callee, args, span } => {
                // Sample the `--max-heap` cap before dispatch (see `sample_mem_cap`). The callee and
                // args are plain Rust locals here, so they are NOT rooted — park them on the operand
                // stack (which `collect` traces) across the sample and take them back after. Under a
                // live cap only, so the uncapped path is byte-for-byte what it was.
                let (callee, args) = if self.heap.mem_cap() != 0 {
                    let argc = args.len();
                    self.push(callee);
                    for a in args {
                        self.push(a);
                    }
                    self.sample_mem_cap(span)?;
                    let args = self.stack.split_off(self.stack.len() - argc);
                    (self.pop(), args)
                } else {
                    (callee, args)
                };
                // TICKET-235: `Vm::task_entry` gave every callable head that pushes no frame an
                // entry thunk. A task queued any other way stops here, not at a park's rewind.
                // (A non-callable callee falls through to `invoke_value`'s own fault.)
                if self.callable(callee).is_some() && !self.enters_frame(callee) {
                    return Err(self.err(
                        "internal error: a task must start in a Chezzi frame".to_string(),
                        span,
                    ));
                }
                self.invoke_value(callee, args, span)?;
                Ok(())
            }
        }
    }

    /// Deep-copy a value across a task airlock (`spawn` / `Channel.send` / `Shared` get-set):
    /// data — scalars, collections, structs, enums — is recursively cloned into fresh heap objects
    /// so a task can't share mutable state with the spawner. `str` (immutable), callables, modules,
    /// and `Channel` / `Shared` handles pass by reference (the handle is what crosses). Allocates, but
    /// only at the instruction boundary that called it (no GC runs mid-clone), so intermediate handles
    /// can't be collected.
    ///
    /// Implemented as a [`WireValue`] round-trip — `to_wire` (read-only serialize) then `from_wire`
    /// (reconstruct into this heap). Byte-identical to the old direct deep-copy; the wire form is what
    /// de-risks the cores-as-`Arc` and real-OS-thread-boundary crossings. By-reference objects cross
    /// as `Handle`. Since F3 path-C a live generator crosses BY VALUE (its parked frame serialized,
    /// rebuilt as an independent copy on `from_wire`), so this is **fallible** only for the generator
    /// HARD ARMS `to_wire` rejects — a generator suspended mid-`recover:` (live handler) or with a
    /// pending `defer`, or one whose parked slot is itself non-sendable — each re-stamped with the real
    /// spawn-site `span` (the caller has it) via `to_wire_at`, a catchable error instead of a panic.
    ///
    /// W7-4 — takes the whole LIST of values that form ONE logical crossing (a `spawn`'s
    /// callee/receiver + args, a `spawn:` block's captures), because cloning them one at a time gave
    /// each its own [`WireMemo`]: two sibling closures over the SAME captured local then arrived as two
    /// cells and the shared binding silently split. One memo spans the whole serialize pass and ONE
    /// rebuild map spans the whole reconstruct pass. (The old single-value `deep_clone` had no callers
    /// left once every crossing became a batch, so it is gone rather than kept as a trap.)
    ///
    /// **Scope invariant** (the loud failure mode): a serialize memo's lifetime must equal its rebuild
    /// map's. A `Backref` minted under memo A but reconstructed under a fresh rebuild map resolves to
    /// nothing — caught by the `debug_assert` in [`from_wire`](Vm::from_wire) (it used to be an
    /// `.expect` that aborted the host; W7-11). Both passes here are local to this call, so they match
    /// by construction. A caller that genuinely CANNOT match them (a piecewise drain) must use
    /// [`from_wire_pieces`](Vm::from_wire_pieces) instead.
    pub(super) fn deep_clone_all(
        &mut self,
        vs: Vec<Value>,
        span: Span,
    ) -> Result<(Vec<Value>, super::CellIds), RuntimeError> {
        // W7-4c — SEED from this view's snapshot registry, so a cell that a module global also reaches
        // serializes under the id the snapshot already gave it. `emitted` stays empty, so this walk
        // writes the cell's FULL definition rather than a `Backref` into a serialization the far side
        // has not replayed (and may replay later, or never — modules fault lazily); `from_wire_memo`
        // dedupes a repeated definition by id.
        let seed_ceiling = self.snapshot_next_id;
        // W7-4c — the whole "a stale id MISSES, never collides" guarantee rests on this. It holds only
        // while the counter travels with the registry it numbered (`FiberCtx` swaps both); split them
        // and a fiber resuming on a fresher shell re-mints ids its own registry already uses.
        debug_assert!(
            self.snapshot_cells.values().all(|&id| id < seed_ceiling),
            "snapshot registry holds an id at or above the mint counter — the counter and the \
             registry have been separated (see FiberCtx::snapshot_cells)"
        );
        // No registry ⇒ no snapshot id to agree with, so skip the `Arc` bump and the base lookup
        // entirely — that is every program whose module globals hold no closure over a captured local.
        let base_cells =
            (!self.snapshot_cells.is_empty()).then(|| Arc::clone(&self.snapshot_cells));
        // TICKET-111 — same idea for data nodes: a spawn-crossed alias of a module global mints the
        // registry's id, so `rebuild_ready` can later adopt it as the global's own object.
        let base_nodes =
            (!self.snapshot_nodes.is_empty()).then(|| Arc::clone(&self.snapshot_nodes));
        let mut memo = WireMemo {
            base_cells,
            base_nodes,
            next_id: seed_ceiling,
            ..WireMemo::default()
        };
        let mut ws = Vec::with_capacity(vs.len());
        for &v in &vs {
            ws.push(self.to_wire_memo_at(v, span, &mut memo)?);
        }
        let mut rebuild = super::fxhash::FxHashMap::<u32, GcRef>::default();
        let out = self.rebuild_items(ws, &mut rebuild, |w| w);
        // W7-4c — hand the CLONE cells' ids back so `lower_task` can serialize them under the same id
        // (the clone is a different `GcRef` than the original the registry knows). Cells only: a
        // container's id lives in the memo's `path`, which pops on DFS exit, so it can never be
        // back-referenced from the separate `lower_task` walk. Never folded into `self.snapshot_cells`
        // — that would grow it once per spawn and pin every clone for the view's life.
        //
        // FAST PATH: with no registry there is no snapshot id to agree with, so there is nothing to
        // report and the scan is skipped outright — that is the overwhelmingly common shape (no module
        // global holds a closure over a captured local), and it keeps `spawn` off this path entirely.
        // Otherwise report only ids BELOW the seed ceiling: those are the registry's, the only ones the
        // snapshot can also use. Ids this walk minted are above every snapshot id (the counter is
        // monotonic), so they can never collide and carrying them would just cost.
        // TICKET-111 — containers and generators now report too, alongside cells: `lower_task` splits
        // this list back apart by kind, so a data-node id also ties this crossing to the module
        // snapshot's replay (adoption), the same way a cell id always has.
        let cell_ids: super::CellIds = if self.snapshot_cells.is_empty()
            && self.snapshot_nodes.is_empty()
        {
            Vec::new()
        } else {
            rebuild
                .iter()
                .filter(|&(&id, &h)| {
                    id < seed_ceiling
                        && (matches!(self.heap.get(h), Obj::Cell(_)) || self.is_adoptable_node(h))
                })
                .map(|(&id, &h)| (h, id))
                .collect()
        };
        Ok((out, cell_ids))
    }

    /// [`to_wire_at`](Vm::to_wire_at) against a CALLER-OWNED [`WireMemo`], so several roots that cross
    /// together share one serialization (see [`deep_clone_all`](Vm::deep_clone_all) for the
    /// memo-scope == rebuild-scope invariant this obliges the caller to keep).
    pub(super) fn to_wire_memo_at(
        &self,
        v: Value,
        span: Span,
        memo: &mut WireMemo,
    ) -> Result<WireValue, RuntimeError> {
        self.to_wire_depth(v, 0, memo)
            .map_err(|e| self.err(e.message, span))
    }

    /// `to_wire` re-stamped with a real call-site `span`. `to_wire`'s `Err`s (a generator HARD ARM —
    /// suspended mid-`recover:`, a parked `defer`, a non-sendable parked slot — or a recursive local
    /// `fn`) carry a placeholder `Span{0,0}`; every method-level airlock site (`Channel.send`/
    /// `Shared.set`/`Atomic.store`/…) has a real span, so route through this so the catchable error
    /// reports the operation's location rather than line 0.
    #[cfg(test)]
    pub(super) fn to_wire_at(&self, v: Value, span: Span) -> Result<WireValue, RuntimeError> {
        self.to_wire(v).map_err(|e| self.err(e.message, span))
    }

    /// `to_wire_at` PLUS the [`ensure_crossable`](Vm::ensure_crossable) handle-reject backstop — the
    /// serialize step used at every cross-heap VALUE-STORE site (`Channel.send`/`try_send`,
    /// `Shared`/`RwShared`/`Atomic` construct/set/update/store/CAS/…). The spawn-arg / capture /
    /// `submit` paths pair `to_wire_at` with `ensure_crossable` by hand; the value-store paths route
    /// through this single helper so a NEW store path physically can't forget the guard (a module /
    /// native / FFI handle silently crossing a channel was serial≠M:N + cross-heap corruption). Legit
    /// `Channel`/`Shared`/`Executor`/socket handles map to shared-`Arc` wire arms (`has_handle()` ==
    /// false), so they still cross unchanged.
    ///
    /// TICKET-154: one serialize rule for EVERY cross-heap store, the three `RwShared` stores
    /// included. An off-stack DAG alias (container or cell) crosses as ONE object; an `RwShared`
    /// read view that drains the stored wire piecewise takes ONE shared rebuild map wherever a piece
    /// is not self-contained (see `Vm::box_copy_pieces`, W11-15). Includes the three
    /// `RwShared` stores.
    pub(super) fn to_wire_crossable(
        &self,
        v: Value,
        span: Span,
    ) -> Result<WireValue, RuntimeError> {
        self.to_wire_crossable_memo(v, span, &mut WireMemo::default())
    }

    /// [`to_wire_crossable`](Self::to_wire_crossable) through the caller's memo (TICKET-192: a
    /// [`WireMemo::seeded`] one for an entry spliced into a stored map).
    pub(super) fn to_wire_crossable_memo(
        &self,
        v: Value,
        span: Span,
        memo: &mut WireMemo,
    ) -> Result<WireValue, RuntimeError> {
        let w = self.walk_src().to_wire_crossable_memo(v, span, memo)?;
        // W6-10 (sampling half) — charge the payload's off-heap bytes against the GC trigger so a
        // live `--max-heap` cap actually gets SAMPLED. `over_cap` is only evaluated in `sweep()`,
        // and `sweep()` only runs when `should_collect()` fires; a store loop that pushes megabytes
        // across the airlock while allocating ~2 `Obj`s per iteration never reached the object-count
        // threshold, so the cap failed OPEN. This is the one helper every cross-heap value store
        // routes through, so a new store path can't forget the charge (same argument as the
        // `ensure_crossable` guard above). GATED on a live cap: a cap-off run (every `chezzi run`,
        // every bench, the two-worker-count `tests/chz` gate) pays one `!= 0` branch and zero extra walks — the
        // walk here is a SECOND `wire_summary` pass (the send path walks again when it caches the
        // core's summary), accepted rather than threading a precomputed summary through
        // `MnSched::send_wake`'s signature for a debug/CI guard. Monotonic pacing HINT, not
        // accounting (`live_bytes()` stays the sole measure): a replacing store charges too and a
        // `recv` never decrements, because under-triggering means a guard that fails open.
        self.charge_wire(&w);
        Ok(w)
    }

    /// Charge an encoded payload's bytes against THIS heap's GC trigger, under a live cap only.
    pub(super) fn charge_wire(&self, w: &WireValue) {
        if self.heap.mem_cap() != 0 {
            self.heap.charge_bytes(crate::vm::core::wire_summary(w).0);
        }
    }

    /// TICKET-111 — true iff `h` is a data node kind eligible for the snapshot node registry / adoption:
    /// every identity-preserved container plus `Generator`, EXCLUDING `Cell` (tied separately via
    /// `snapshot_cells`, W7-4c) and `Closure` (never adopted — a closure's identity is its own binding,
    /// not a spawn-crossed alias target).
    pub(super) fn is_adoptable_node(&self, h: GcRef) -> bool {
        matches!(
            self.heap.get(h),
            Obj::List(_)
                | Obj::Tuple(_)
                | Obj::Map(_)
                | Obj::Set(_)
                | Obj::Struct { .. }
                | Obj::Enum { .. }
                | Obj::Iter { .. }
                | Obj::Generator(_)
        )
    }

    /// B3.0 — serialize a value into its [`WireValue`] form (the airlock's outbound half). A
    /// read-only walk of the heap, structurally identical to `deep_clone`'s old recursion but
    /// allocating nothing. Data (list/tuple/map/set/struct/enum) recurses; immutable / by-reference
    /// objects (`Str`, callables, modules, `Channel`/`Shared`/`Executor`) cross as
    /// [`WireValue::Handle`] (the existing handle, same heap in B3.0). `Map`/`Set` carry their cached
    /// hashes through so reconstruction never re-hashes.
    ///
    /// Every `Value` and every `Obj` variant maps to a wire arm — by-reference objects (callables,
    /// modules, `Channel`/`Shared`/`Executor`) cross as `Handle`. The ONE fallible arm is a
    /// frame-holding **generator** (`Obj::Generator`): its parked frames reference this heap, so it
    /// is not sendable and returns a graceful `a generator cannot be sent across tasks` error here
    /// (carrying a placeholder `Span{0,0}` that airlock callers re-stamp with the real site via
    /// `to_wire_at`/`deep_clone`/`ensure_snapshot`). Every other arm is infallible (the `?` only
    /// forwards the generator error up through container recursion).
    pub(super) fn to_wire(&self, v: Value) -> Result<WireValue, RuntimeError> {
        // Fresh memo per root — correct for a SINGLE-root crossing (`Channel.send`, a `Shared` store).
        // A crossing whose roots belong together (a `spawn`'s args, a `spawn:` block's captures, one
        // module's globals) must instead share one memo via `to_wire_memo_at`/`deep_clone_all`, or two
        // sibling closures over the same local arrive as two cells (W7-4). TICKET-100: within a memo,
        // containers, closures and cells are ALL preserved for the whole crossing — one source object
        // produces one copy per crossing (see [`WireMemo`]).
        let mut memo = WireMemo::default();
        self.to_wire_depth(v, 0, &mut memo)
    }

    /// [`WalkSrc::to_wire_depth`] over this `Vm`'s own heap.
    #[inline]
    pub(super) fn to_wire_depth(
        &self,
        v: Value,
        depth: usize,
        memo: &mut WireMemo,
    ) -> Result<WireValue, RuntimeError> {
        self.walk_src().to_wire_depth(v, depth, memo)
    }
}

/// Identity-preservation state threaded through [`Vm::to_wire_depth`] — every identity-preserved node
/// kind (`Obj::Cell`/`Obj::Closure` AND the container arms `List`/`Tuple`/`Map`/`Set`/`Struct`/`Enum`/
/// `Iter`) records its GcRef here so ANY value cycle round-trips: a recursive local `fn`'s
/// letrec self-cell, a mutually-recursive closure pair, a self-referential struct/list/map, or a mixed
/// struct+closure cycle. `path` maps each such GcRef **currently on the serialize DFS stack** to the
/// `id` assigned on its first visit; `next_id` is the monotonic id counter. On a REVISIT of a node still
/// in `path` (a true back-edge) the arm emits `WireValue::Backref(id)` and stops.
///
/// TICKET-100 — one source object now produces ONE copy per crossing, for data, closures and cells
/// alike. `path` is popped on DFS exit, but `nodes` (never popped) also records every identity-preserved
/// node reached anywhere in this crossing, so an OFF-stack DAG alias back-references too, exactly like
/// an on-stack cycle: `b := a` inside a task means the same thing it means outside one, and CPython's
/// `copy.deepcopy` agrees (it memoizes by source identity). A `Generator` is the sole remaining
/// unpreserved container on the DFS stack (its parked frame is mid-serialization, so it cannot be a
/// `Backref` target while it is being written). A cycle re-entering the SAME generator on the DFS stack
/// is REJECTED cleanly (`gens_on_stack`): with the containers now back-referencing, the depth cap no
/// longer trips on such a cycle, and re-serializing the generator would silently DUPLICATE it — the
/// e8dcad7 wrong-result class. TICKET-137: a later OFF-stack reach (two module globals or two list
/// slots holding the same generator) is a `Backref` to the id its first reach minted in `nodes`, so
/// the crossing holds ONE copy with the alias intact. An acyclic parked slot too deep still trips the
/// depth cap.
///
/// **TICKET-154 — the three `RwShared` stores serialize by this same rule.** An `RwShared` read view
/// drains ONE stored wire through independent `from_wire` rebuilds, so a depth-1 element that
/// back-references a node defined in a SIBLING element (an aliased container or cell) or in the root
/// itself (a cycle) is not self-contained. The rebuild side handles that, never the store: the four
/// looping views (`for_each`/`fold`/`for_each_entry`/`fold_entries`) rebuild the whole wire ONCE into
/// one map and materialize every piece from it (`Vm::box_copy_pieces`), and the single-piece
/// views fall back to a whole-root rebuild per call ([`Vm::from_wire_pieces`]). Two separate single-piece
/// calls stay two crossings, as two separate `copy.deepcopy`s are. Do NOT re-emit container or cell
/// definitions per depth-1 subtree to make pieces self-contained: that is O(n²) wire size, the cliff
/// `rwshared_view_over_shared_bindings_is_not_quadratic` exists to catch.
#[derive(Default)]
pub(super) struct WireMemo {
    /// GcRef of an identity-preserved node (`Closure`/container) currently on the serialize DFS
    /// stack → the `id` assigned on its first visit. A revisit while still in `path` is a true back-edge
    /// → `Backref(id)`. Popped on DFS exit (`WireMemo::exit`) because `try_wire_speculative` asserts
    /// it is empty at entry — the off-stack case is now covered by `nodes` below, not by `path` itself.
    pub(super) path: super::fxhash::FxHashMap<GcRef, u32>,
    /// TICKET-100 — GcRef of every identity-preserved node (`Closure`/container) seen ANYWHERE in this
    /// crossing → its `id`. NEVER popped, so an off-stack DAG alias also back-references: one source
    /// object now produces one copy per crossing, not one copy per DFS path to it. See
    /// `WireMemo::seen`/`enter`.
    pub(super) nodes: super::fxhash::FxHashMap<GcRef, u32>,
    /// W7-4 — GcRef of every `Obj::Cell` seen ANYWHERE in this serialization → its `id`. Never
    /// removed: a cell is a binding, so reaching it again (off-stack sibling closure, or on-stack
    /// letrec back-edge) emits `Backref` and the far side
    /// rebuilds exactly one cell per binding. Scope discipline: a serialize memo's lifetime must equal
    /// its `from_wire_memo` rebuild map's, or a `Backref` minted under one memo hits the other's
    /// `.expect` — see [`Vm::to_wire_memo_at`].
    pub(super) cells: super::fxhash::FxHashMap<GcRef, u32>,
    /// W7-4c — a READ-ONLY base consulted on a `cells` miss: this view's snapshot cell registry
    /// ([`Vm::snapshot_cells`]), shared by `Arc` rather than copied. Every `spawn` seeds a memo from
    /// it, so copying would put an O(module-global cells) clone on the spawn path — the same O(M·K)
    /// shape W7-4 already rejected for `to_snap`'s speculative rollback. Never written: new ids go to
    /// `cells`, which shadows it, so `try_wire_speculative`'s rollback stays exact.
    pub(super) base_cells: Option<Arc<super::fxhash::FxHashMap<GcRef, u32>>>,
    /// TICKET-111 — read-only snapshot node registry consulted when a data node is first reached;
    /// never written, never set on a memo that speculates.
    pub(super) base_nodes: Option<Arc<super::fxhash::FxHashMap<GcRef, u32>>>,
    /// Ids from `cells` already EMITTED as a full `WireValue::Cell`. `memo.emitted.clear()` at the
    /// per-module re-emit (`snapshot_modules`) is what keeps this a set apart from `cells`' id set.
    pub(super) emitted: super::fxhash::FxHashSet<u32>,
    /// W7-4a — undo journal for [`emitted`](WireMemo::emitted) while a SPECULATIVE attempt is in
    /// flight: the ids it newly inserted. `try_wire_speculative`'s rollback used to be complete
    /// with `emitted.retain(|id, _| *id < mint_from)`, because every id in the memo had been minted by
    /// the current module's own walk — so "id below the watermark" meant "really emitted". Once the
    /// memo spans MODULES (`snapshot_modules`) that stopped holding: a discarded attempt can mark an
    /// id minted in an EARLIER module, `retain` keeps it as if it were real, and the module's kept
    /// encoding then emits a `Backref` whose definition it never wrote — a dangling ref that rebuilds
    /// a closure over `nil` and trips `CellLoad on a non-handle value`. Recorded only while
    /// `speculating`, so the non-speculative paths pay nothing.
    pub(super) emit_undo: Vec<u32>,
    /// True for the duration of one [`Vm::try_wire_speculative`] attempt. Never nests — the only
    /// callers are `to_snap_depth`'s two speculative sites, and the attempt runs `to_wire_depth`,
    /// which never re-enters `to_snap_depth` (asserted by the empty-`path` `debug_assert` at entry).
    pub(super) speculating: bool,
    pub(super) next_id: u32,
    /// GcRefs of `Obj::Generator`s currently on the serialize DFS stack. Re-entering one still on the
    /// stack is a cycle through a node whose parked frame is mid-serialization → reject (never
    /// duplicate). Removed on DFS exit. An OFF-stack revisit is a `Backref` through `nodes`
    /// (TICKET-137), not a reject.
    pub(super) gens_on_stack: super::fxhash::FxHashSet<GcRef>,
    /// TICKET-119 — set by `to_wire_depth_inner`'s depth guard, cleared at each speculative attempt's
    /// entry. No arm swallows an error, so it classifies the one `Err` that leaves the attempt.
    pub(super) depth_tripped: bool,
    /// TICKET-119 — `next_id` at the in-flight attempt's latest SHORTCUT (a `Backref` return, or a
    /// cell whose id already existed). A frame entered at or before it records nothing.
    pub(super) shortcut_at: Option<u32>,
    /// TICKET-119 — `(node, enter stamp, depth)` of each untainted frame a depth-tripped attempt
    /// unwound through; moved into `doom` when that attempt is discarded.
    pub(super) doom_pending: Vec<(GcRef, u32, usize)>,
    /// TICKET-119 — the LATEST depth-tripped attempt's records: node -> (enter stamp, depth). While a
    /// record is valid, an attempt rooted at that node at `depth >= d0` replays the recorded walk
    /// exactly and trips no later, so `to_snap_depth` skips it. See `docs/concurrency.md`'s
    /// depth-guard paragraph and `WireMemo::doomed`.
    pub(super) doom: super::fxhash::FxHashMap<GcRef, (u32, usize)>,
    /// TICKET-119 — every node/cell that attempt minted -> its id there, which is its enter stamp.
    pub(super) doom_ids: super::fxhash::FxHashMap<GcRef, u32>,
    /// TICKET-119 — highest `doom_ids` stamp touched since the recording; a record is valid only
    /// while its enter stamp is above it.
    pub(super) doom_invalid_upto: Option<u32>,
}

impl WireMemo {
    /// TICKET-119 — `h` may be about to become a `Backref` target: invalidate every `doom` record
    /// whose recorded sub-walk contains it.
    pub(super) fn doom_touch(&mut self, h: GcRef) {
        if self.doom_ids.is_empty() {
            return;
        }
        if let Some(&s) = self.doom_ids.get(&h) {
            self.doom_invalid_upto = Some(self.doom_invalid_upto.map_or(s, |u| u.max(s)));
        }
    }

    /// TICKET-119 — true iff a depth-tripped attempt already proved, by an exact-replay argument,
    /// that a walk from `h` at `depth` or deeper trips no later than it did: a `doom` record exists
    /// for `h`, `depth` is at or below the recorded depth's cap-ward direction (`depth >= d0`), and
    /// nothing has invalidated the record since (`enter > doom_invalid_upto`). `to_snap_depth` skips
    /// the speculative attempt when this is true. See `docs/concurrency.md`'s depth-guard paragraph.
    pub(super) fn doomed(&self, h: GcRef, depth: usize) -> bool {
        self.doom.get(&h).is_some_and(|&(enter, d0)| {
            depth >= d0 && self.doom_invalid_upto.is_none_or(|u| enter > u)
        })
    }

    /// W7-4c — the id this cell already crosses under, from the overlay or the shared base.
    pub(super) fn cell_id(&self, h: GcRef) -> Option<u32> {
        self.cells.get(&h).copied().or_else(|| {
            self.base_cells
                .as_ref()
                .and_then(|base| base.get(&h).copied())
        })
    }

    /// TICKET-100 — the id `h` already crosses under: an on-stack back-edge (`path`) first, else an
    /// off-stack DAG alias already seen this crossing (`nodes`).
    fn seen(&self, h: GcRef) -> Option<u32> {
        self.path
            .get(&h)
            .copied()
            .or_else(|| self.nodes.get(&h).copied())
    }

    /// TICKET-100 — record `h`'s first visit: on the DFS stack, and in `nodes` so a later off-stack
    /// reach still back-references.
    fn enter(&mut self, h: GcRef, id: u32) {
        self.path.insert(h, id);
        self.nodes.insert(h, id);
    }

    /// TICKET-100 — pop `h` off the DFS stack on exit. `nodes` is never popped.
    fn exit(&mut self, h: GcRef) {
        self.path.remove(&h);
    }

    /// TICKET-111 — the id to mint for a data node's FIRST reach this crossing: the snapshot
    /// registry's id for `h` if it holds one (ties this crossing to the module snapshot's
    /// replay, W12-5 adoption), else a fresh id.
    fn mint_node(&mut self, h: GcRef) -> u32 {
        if let Some(id) = self
            .base_nodes
            .as_ref()
            .and_then(|base| base.get(&h).copied())
        {
            return id;
        }
        let id = self.next_id;
        self.next_id += 1;
        id
    }
}

/// TICKET-246 — everything the copy walk reads, borrowed. The walk is `&self` over these five
/// fields and nothing else, so one body serves a `Vm` encoding out of its own heap
/// ([`Vm::walk_src`]) and a `RwShared` reader encoding out of a box heap it only borrows
/// (TICKET-246 step 5). It never charges a heap: the caller charges its OWN.
pub(super) struct WalkSrc<'a> {
    pub(super) heap: &'a Heap,
    pub(super) module_objs: &'a [GcRef],
    pub(super) active_generators: &'a [GcRef],
    pub(super) program: &'a Program,
    pub(super) walk_base: usize,
    /// The heap id a closure is stamped with (`WireValue::Closure::origin_heap`). A receiver
    /// marks a closure whose stamp is not its own heap as a task copy (D4 layer C). A `Vm` stamps
    /// its own heap. A copy handed OUT of a box is stamped with the READER's heap: the reader
    /// owns that copy, so a write through it is not a lost write (DEC-189, a hand-off never marks).
    pub(super) origin: u64,
}

impl<'a> WalkSrc<'a> {
    /// The source for a parked context (a box state): its heap and its module table. Nothing runs
    /// in a parked context, so no generator of its heap is mid-resume.
    pub(super) fn of_ctx(
        ctx: &'a FiberCtx,
        program: &'a Program,
        walk_base: usize,
        origin: u64,
    ) -> Self {
        WalkSrc {
            heap: ctx.heap.as_ref().expect("a box context owns its heap"),
            module_objs: &ctx.module_objs,
            active_generators: &[],
            program,
            walk_base,
            origin,
        }
    }

    fn err(&self, message: String, span: Span) -> RuntimeError {
        super::exec::plain_err(message, span)
    }

    fn depth_exceeded_err(&self, span: Span) -> RuntimeError {
        super::exec::depth_exceeded(span)
    }

    /// B3.3c — the index of a `home` module `GcRef` in the source's `module_objs`, so the receiver
    /// can resolve the corresponding rebuilt module. `None` for a home not in the table.
    pub(super) fn home_index(&self, home: GcRef) -> Option<usize> {
        self.module_objs.iter().position(|&m| m == home)
    }

    /// Resolve a struct instance's `tid` to its type key. Returns `"?"` for
    /// [`crate::vm::op::TID_NONE`] / an out-of-range id.
    pub(super) fn struct_name_of_tid(&self, tid: u32) -> &'a str {
        self.program
            .struct_names
            .get(tid as usize)
            .map_or("?", |s| s)
    }

    /// Encode `v` for another heap, with the [`ensure_crossable`](Vm::ensure_crossable)
    /// handle-reject backstop. A fault carries `span`.
    pub(super) fn to_wire_crossable_memo(
        &self,
        v: Value,
        span: Span,
        memo: &mut WireMemo,
    ) -> Result<WireValue, RuntimeError> {
        let w = self
            .to_wire_depth(v, 0, memo)
            .map_err(|e| self.err(e.message, span))?;
        ensure_crossable_wire(&w, span)?;
        Ok(w)
    }
}

/// The handle-reject backstop behind [`Vm::ensure_crossable`] and [`WalkSrc`].
pub(super) fn ensure_crossable_wire(w: &WireValue, span: Span) -> Result<(), RuntimeError> {
    if w.has_handle() {
        return Err(super::exec::plain_err(
            "this task value can't cross a worker boundary — plain data, closures/functions, \
             native/FFI fns, and Channel/Shared/Executor handles are sendable, but a module handle cannot cross"
                .to_string(),
            span,
        ));
    }
    Ok(())
}

impl WalkSrc<'_> {
    /// Depth-counted worker behind [`Vm::to_wire`]. A **self-referential** sendable (e.g. a struct
    /// whose field points back at itself) now ROUND-TRIPS via the identity-preserving id + `Backref`
    /// machinery (every container arm + `Cell`/`Closure`), so it never recurses unbounded. This depth
    /// bound remains as the backstop for a genuinely-unbounded ACYCLIC nest (which would otherwise
    /// recurse on the HOST stack and `SIGABRT`, uncatchable) — the SAME `MAX_STRUCTURAL_DEPTH` limit
    /// and message the display / `==` paths use (`stmt.rs` / `arith.rs`), turned into a recoverable
    /// `RuntimeError` (placeholder `Span{0,0}`, re-stamped with the real airlock site by `to_wire_at`).
    /// Kept in lockstep with [`Vm::to_snap`] (which shares this budget on its fast path) so the serial
    /// and M:N engines trip at the identical depth.
    #[inline(always)]
    fn to_wire_depth(
        &self,
        v: Value,
        depth: usize,
        memo: &mut WireMemo,
    ) -> Result<WireValue, RuntimeError> {
        #[cfg(test)]
        super::WIRE_ENCODE_NODES.with(|c| c.set(c.get() + 1));
        if !memo.speculating {
            return self.to_wire_depth_inner(v, depth, memo);
        }
        let enter = memo.next_id;
        if let Some(h) = v.as_obj() {
            memo.doom_touch(h);
        }
        let r = self.to_wire_depth_inner(v, depth, memo);
        match &r {
            Ok(WireValue::Backref(_)) => memo.shortcut_at = Some(memo.next_id),
            Err(_) if memo.depth_tripped => {
                if let Some(h) = v.as_obj()
                    && memo.shortcut_at.is_none_or(|s| enter > s)
                {
                    memo.doom_pending.push((h, enter, depth));
                }
            }
            _ => {}
        }
        r
    }

    /// TICKET-119 — the walk itself, wrapped by `to_wire_depth` for doomed-attempt bookkeeping.
    fn to_wire_depth_inner(
        &self,
        v: Value,
        depth: usize,
        memo: &mut WireMemo,
    ) -> Result<WireValue, RuntimeError> {
        if self.walk_base + depth > MAX_STRUCTURAL_DEPTH {
            memo.depth_tripped = true;
            return Err(self.depth_exceeded_err(Span::default()));
        }
        Ok(match v.view() {
            ValueView::Int(n) => WireValue::Int(n),
            ValueView::Bool(b) => WireValue::Bool(b),
            ValueView::Nil => WireValue::Nil,
            // A boxed float is Float-tagged → `view` yields `Obj(h)` → the `Obj::FloatBox` arm below
            // maps it to `WireValue::Float` (a boxed `BigInt` likewise → `WireValue::Int`).
            ValueView::Obj(h) => match self.heap.get(h) {
                // B3.3a: `str` crosses by value (owned bytes) so it can survive an OS-thread heap
                // boundary; immutable + value-compared, so a fresh handle on reconstruction is
                // observationally identical to sharing this one.
                Obj::Str(s) => WireValue::Str(s.as_str().into()),
                // Boxed scalars cross by value, exactly like the inline `Int`/`Float` arms above —
                // `from_wire` re-inlines (or re-boxes, once Phase 1 is live) on the far heap.
                Obj::BigInt(n) => WireValue::Int(*n),
                Obj::FloatBox(f) => WireValue::Float(*f),
                // `bytes` crosses by value (owned raw bytes), exactly like `str` — immutable +
                // value-compared, so a fresh handle on reconstruction is observationally identical.
                Obj::Bytes(b) => WireValue::Bytes(b.clone()),
                // `bytearray` crosses by value as a DEEP COPY (a fresh independent buffer on the other
                // side, like `list`) — never a shared mutable view. `from_wire` rebuilds a new heap
                // `bytearray`, so cross-thread mutation never aliases.
                Obj::ByteArray(b) => WireValue::ByteArray(b.clone().into_boxed_slice()),
                // A first-class builtin fn crosses the airlock BY VALUE (its name) — pure code, no
                // `GcRef` and no captured heap state, so it genuinely crosses an OS-thread boundary
                // (unlike a `Func`/`Closure` handle). `from_wire` re-allocs a fresh `Obj::Builtin`;
                // builtins are name-compared, so that is observationally identical. Works the same way
                // on this airlock-cross path and on the separate module-snapshot replay path
                // (`SnapValue::Builtin`).
                Obj::Builtin(name) => WireValue::Builtin(name.clone()),
                // B3.3: a closure crosses the airlock BY VALUE — its `proto` (shared via `Arc<Program>`),
                // its captures wired recursively in slot order (paired with the proto's `capture_names`),
                // and its `home` as an index into `module_objs` — never a heap-local `GcRef`. Mirrors the
                // slot-order/home logic in `to_snap_depth`'s Closure arm and the old `wire_callable`.
                //
                // Identity preservation: a recursive local `fn`'s letrec self-cell makes this closure's
                // capture graph cyclic (`Closure h -> captured[Cell] -> Cell.inner = h`, or the
                // `Closure_f -> Cell_g -> Closure_g -> Cell_f` mutual-recursion loop). We assign `h` an
                // `id` and record it via `memo.enter` BEFORE recursing captures; a nested revisit of `h`
                // (the back-edge) emits `WireValue::Backref(id)` and stops — `from_wire` ties the knot
                // back. TICKET-100: `h` is popped off `path` on exit but kept in `nodes`, so an off-stack
                // alias also back-references.
                Obj::Closure {
                    proto,
                    captured,
                    home,
                } => {
                    if let Some(id) = memo.seen(h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.next_id;
                        memo.next_id += 1;
                        memo.enter(h, id);
                        let names = &self.program.protos[*proto].capture_names;
                        let mut wcap = Vec::with_capacity(captured.len());
                        for (i, cv) in captured.iter().enumerate() {
                            let w = self.to_wire_depth(*cv, depth + 1, memo)?;
                            let name = names.get(i).cloned().unwrap_or_default();
                            wcap.push((name.into_boxed_str(), w));
                        }
                        memo.exit(h);
                        WireValue::Closure {
                            id,
                            proto: *proto,
                            captured: wcap,
                            home: self.home_index(*home),
                            origin_heap: self.origin,
                        }
                    }
                }
                // B3.3: a bare fn crosses BY VALUE as its own distinct `Func` arm (NOT an empty-capture
                // `Closure`), so it keeps rendering `<fn NAME>` after a round-trip (a `Closure` renders
                // `<closure>` — collapsing them would diverge the M:N snapshot rebuild from serial).
                Obj::Func { proto, home } => WireValue::Func {
                    proto: *proto,
                    home: self.home_index(*home),
                },
                // A module's mutable globals genuinely can't cross an OS-thread heap boundary — its
                // `GcRef` is meaningless on the receiver heap — so it stays a by-reference `Handle`
                // (the worker airlock rejects it; source-unreachable, defensive only).
                Obj::Module(_) => WireValue::Handle(h),
                // A native fn is pure code (fn ptr + name, no `GcRef`) and a Cffi is a shared `Arc` —
                // both cross BY VALUE like `Builtin`, exactly as the SNAPSHOT path
                // (`SnapValue::Native`/`Cffi`) already ships them across M:N workers. `has_handle`
                // leaves both `false`, so the worker airlock accepts them.
                Obj::Native { name, func, kind } => WireValue::Native {
                    name: name.clone(),
                    func: *func,
                    kind: *kind,
                },
                Obj::Cffi(c) => WireValue::Cffi(Arc::clone(c)),
                // B3.1: the shared cores cross as the `Arc` itself (clone = refcount bump), so a
                // `from_wire` in any heap reaches the same mailbox/box/queue.
                Obj::Channel(core) => WireValue::Channel(Arc::clone(core)),
                Obj::Shared(core) => WireValue::Shared(Arc::clone(core)),
                Obj::RwShared(core) => WireValue::RwShared(Arc::clone(core)),
                Obj::Atomic(core) => WireValue::Atomic(Arc::clone(core)),
                Obj::AtomicInt(core) => WireValue::AtomicInt(Arc::clone(core)),
                Obj::Executor(core) => WireValue::Executor(Arc::clone(core)),
                // D6: a socket/listener handle crosses as its shared `Arc` core (a spawned fiber
                // reaches the same fd) — same shape as `Channel`/`Shared`/`Executor`.
                Obj::Socket(core) => WireValue::Socket(Arc::clone(core)),
                Obj::Listener(core) => WireValue::Listener(Arc::clone(core)),
                // R2: a `Writer` handle crosses as its shared `Arc` core (a spawned fiber reaches the
                // same output) — same shape as `Socket`. Cross-task write ordering is unspecified.
                Obj::Writer(core) => WireValue::Writer(Arc::clone(core)),
                // R2b: a `Reader` handle crosses as its shared `Arc` core (a spawned fiber reaches the
                // same fd) — same shape as `Writer`. Cross-task read ordering is unspecified.
                Obj::Reader(core) => WireValue::Reader(Arc::clone(core)),
                // An opaque `ptr` handle crosses by value — its raw address is heap-independent, so a
                // fresh `Obj::Ptr` on the other side is observationally identical (immutable +
                // value-compared). Cross-safe for both the serial and M:N engines.
                Obj::Ptr(a) => WireValue::Ptr(*a),
                // Identity-preserved container (see the `Obj::Cell`/`Obj::Closure` arms): assign `h` an
                // `id` and record it via `memo.enter` BEFORE recursing, so a self-referential list
                // (`xs.push(xs)`) or any cycle passing through it back-references instead of overflowing
                // the depth cap. TICKET-100: removed from `path` on DFS exit but kept in `nodes`, so an
                // off-stack alias back-references too.
                Obj::List(items) => {
                    if let Some(id) = memo.seen(h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.mint_node(h);
                        memo.enter(h, id);
                        let mut out = Vec::with_capacity(items.len());
                        for x in items {
                            out.push(self.to_wire_depth(*x, depth + 1, memo)?);
                        }
                        memo.exit(h);
                        WireValue::List { id, items: out }
                    }
                }
                Obj::Tuple(items) => {
                    if let Some(id) = memo.seen(h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.mint_node(h);
                        memo.enter(h, id);
                        let mut out = Vec::with_capacity(items.len());
                        for x in items {
                            out.push(self.to_wire_depth(*x, depth + 1, memo)?);
                        }
                        memo.exit(h);
                        WireValue::Tuple { id, items: out }
                    }
                }
                Obj::Map(m) => {
                    if let Some(id) = memo.seen(h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.mint_node(h);
                        memo.enter(h, id);
                        let mut out = Vec::with_capacity(m.entries.len());
                        for (hash, k, val) in &m.entries {
                            out.push((
                                *hash,
                                self.to_wire_depth(*k, depth + 1, memo)?,
                                self.to_wire_depth(*val, depth + 1, memo)?,
                            ));
                        }
                        memo.exit(h);
                        WireValue::Map {
                            id,
                            entries: WireMap::new(out),
                        }
                    }
                }
                Obj::Set(s) => {
                    if let Some(id) = memo.seen(h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.mint_node(h);
                        memo.enter(h, id);
                        let mut out = Vec::with_capacity(s.entries.len());
                        for (hash, e) in &s.entries {
                            out.push((*hash, self.to_wire_depth(*e, depth + 1, memo)?));
                        }
                        memo.exit(h);
                        WireValue::Set {
                            id,
                            entries: WireSet::new(out),
                        }
                    }
                }
                // Identity-preserved container (see `Obj::List`): a self-referential struct
                // (`a.next = b; b.next = a`) back-references instead of overflowing the cap.
                Obj::Struct { tid, fields, .. } => {
                    if let Some(id) = memo.seen(h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.mint_node(h);
                        memo.enter(h, id);
                        // Positional layout: recover the declaration-order field names from the
                        // StructDef (cold cross-task path) so the WireValue encoding is unchanged.
                        // The instance carries only `tid`; resolve the type key from it (owned — the
                        // wire format still carries the name string, receiver re-derives its tid).
                        let name: Box<str> = self.struct_name_of_tid(*tid).into();
                        let names: Vec<Box<str>> = self
                            .program
                            .structs
                            .get(name.as_ref())
                            .map(|d| {
                                d.fields
                                    .iter()
                                    .map(|f| f.clone().into_boxed_str())
                                    .collect()
                            })
                            .unwrap_or_default();
                        let vals: Vec<Value> = fields.as_slice().to_vec();
                        let mut out = Vec::with_capacity(vals.len());
                        for (i, val) in vals.iter().enumerate() {
                            let k = names
                                .get(i)
                                .cloned()
                                .unwrap_or_else(|| i.to_string().into_boxed_str());
                            out.push((k, self.to_wire_depth(*val, depth + 1, memo)?));
                        }
                        memo.exit(h);
                        WireValue::Struct {
                            id,
                            name,
                            fields: out,
                        }
                    }
                }
                Obj::Enum {
                    variant_id,
                    payload,
                } => {
                    if let Some(id) = memo.seen(h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.mint_node(h);
                        memo.enter(h, id);
                        let mut out = Vec::with_capacity(payload.len());
                        for x in payload {
                            out.push(self.to_wire_depth(*x, depth + 1, memo)?);
                        }
                        memo.exit(h);
                        // M19 lever #2 — carry the dense `variant_id` directly on the COLD wire path. All
                        // workers share one `Arc<Program>`, so the id is meaningful on both sides;
                        // carrying it (not the name) preserves native-vs-user identity under shadowing.
                        WireValue::Enum {
                            id,
                            variant_id: *variant_id,
                            payload: out,
                        }
                    }
                }
                // A frame-holding generator crosses the airlock BY VALUE as a DEEP COPY: its `proto`
                // (shared via `Arc<Program>`), `home` index, backing closure, and lifecycle state, with
                // every parked slot wired recursively so a non-sendable slot rejects AT SERIALIZE TIME.
                // Both a FRAME-LOCAL generator (F3 path C — direct `spawn`/`Channel.send` crossing) and a
                // MODULE-GLOBAL generator (backlog item B — via `to_snap`'s slow arm, which re-enters
                // here) take this same by-value path; the old reach-gate + Option-B poison→Nil model is
                // retired. The single-frame Suspended invariant is load-bearing (the checker resets `in_generator`
                // at every fn/closure boundary, so a suspension always parks exactly the generator body
                // frame); a mid-`recover:` handler, a pending `defer`, or >1 parked frame is a HARD ARM
                // rejected cleanly below (never mis-serialized).
                Obj::Generator(g) => {
                    // TICKET-041(a) — `generator_next` parks `GenState::Done` as a placeholder in the
                    // heap object for the DURATION of a resume (`src/vm/exec.rs:510`), so a generator
                    // crossed WHILE it is running would otherwise serialize that placeholder verbatim
                    // and hand the receiver a copy that reports itself as already exhausted. Reject at
                    // the serializer instead: the same oracle `generator_next`'s own re-entrancy guard
                    // already applies (`src/vm/exec.rs:497`, "generator already running"). CPython
                    // agrees — `copy.deepcopy(gen)` on a running generator raises
                    // `TypeError: cannot pickle 'generator' object` (measured 2026-09-03).
                    if self.active_generators.contains(&h) {
                        return Err(self.err(
                            "a running generator cannot be sent across tasks".to_string(),
                            Span::default(),
                        ));
                    }
                    // A cycle re-entering THIS generator on the DFS stack closes a value cycle through a
                    // node whose parked frame cannot be a mid-rebuild `Backref` target → reject cleanly
                    // (re-serializing it would silently DUPLICATE the generator, the e8dcad7
                    // wrong-result class). Insert BEFORE recursing; remove on exit. A recursive closure
                    // PARKED in the generator is still fine: its self-cell cycle is identity-preserved
                    // and back-refs.
                    if memo.gens_on_stack.contains(&h) {
                        return Err(self.err(
                            "a generator cannot be sent across tasks as part of a reference cycle"
                                .to_string(),
                            Span::default(),
                        ));
                    }
                    // TICKET-137 — a generator reached a second time OFF the stack (two module globals,
                    // or two list slots, aliasing one generator) is ONE copy per crossing: `nodes` holds
                    // the id its first reach minted and `from_wire_memo` registered, so the alias is a
                    // `Backref`.
                    if let Some(id) = memo.nodes.get(&h).copied() {
                        return Ok(WireValue::Backref(id));
                    }
                    memo.gens_on_stack.insert(h);
                    let id = memo.mint_node(h);
                    memo.nodes.insert(h, id);
                    let home = self.home_index(g.home);
                    let closure = match g.closure {
                        Some(c) => Some(Box::new(self.to_wire_depth(
                            Value::obj(c),
                            depth + 1,
                            memo,
                        )?)),
                        None => None,
                    };
                    let state = match &g.state {
                        GenState::Pending(args) => {
                            let mut wargs = Vec::with_capacity(args.len());
                            for a in args {
                                wargs.push(self.to_wire_depth(*a, depth + 1, memo)?);
                            }
                            // TICKET-190: the creation stamp rides along; the rebuild arm reads it.
                            WireGenState::Pending(wargs, g.stamp)
                        }
                        GenState::Done => WireGenState::Done,
                        GenState::Unsendable(m) => WireGenState::Unsendable(m.clone()),
                        GenState::Suspended => {
                            // CHECKER-UNREACHABLE HARD ARMS — reject cleanly rather than silently
                            // mis-serialize. Neither shape can arise from checker-valid source; these
                            // are defensive guards against the type-blind compiler path.
                            //  - multi-frame (a): `yield` only fires in the generator's own body frame
                            //    (`in_generator` resets at every fn/closure boundary), so a suspended
                            //    generator always has exactly one frame.
                            //  - pending `defer` (c): `defer` is banned inside a generator body.
                            // A mid-`recover:` suspension (arm b) is NO LONGER rejected — a live
                            // handler stack is pure plain-data and now serialized below.
                            if g.ctx.frames.len() != 1 {
                                return Err(self.err(
                                    "a generator suspended across more than one call frame cannot be sent across tasks".to_string(),
                                    Span::default(),
                                ));
                            }
                            let frame = &g.ctx.frames[0];
                            if !frame.deferred.is_empty() {
                                return Err(self.err(
                                    "a generator suspended with a pending `defer` cannot be sent across tasks".to_string(),
                                    Span::default(),
                                ));
                            }
                            // The sole body frame's home/closure equal the core's by construction
                            // (`push_frame(proto, g.home, g.closure, …)`); assert defensively and
                            // reject rather than wire a mismatched frame.
                            if frame.home != g.home || frame.closure != g.closure {
                                return Err(self.err(
                                    "a generator with an inconsistent parked frame cannot be sent across tasks".to_string(),
                                    Span::default(),
                                ));
                            }
                            let mut wstack = Vec::with_capacity(g.ctx.stack.len());
                            for v in &g.ctx.stack {
                                wstack.push(self.to_wire_depth(*v, depth + 1, memo)?);
                            }
                            // Backlog arm (b): carry the live `recover:` handlers. Each `Handler` is
                            // `Copy` plain-data (all `usize`, no `GcRef`/`Value`), so it crosses as-is
                            // with no value recursion; its indices address the frame/stack serialized
                            // above and are reconstructed coherently in `from_wire`.
                            WireGenState::Suspended {
                                frame: Box::new(WireCallFrame {
                                    proto: frame.proto,
                                    ip: frame.ip,
                                    base: frame.base,
                                    counted: frame.counted,
                                    is_toplevel: frame.is_toplevel,
                                    defer_markers: frame.defer_markers.clone(),
                                    nursery_len: frame.nursery_len,
                                    has_implicit_nursery: frame.has_implicit_nursery,
                                    call_span: frame.call_span,
                                    argc: frame.argc,
                                }),
                                stack: wstack,
                                call_depth: g.ctx.call_depth,
                                cur_base: g.ctx.cur_base,
                                handlers: g.ctx.handlers.clone(),
                                stamp: g.stamp,
                            }
                        }
                    };
                    // Off the DFS stack; `nodes` keeps `h`, so a later reach is a `Backref`.
                    memo.gens_on_stack.remove(&h);
                    WireValue::Generator {
                        id,
                        proto: g.proto,
                        home,
                        closure,
                        state,
                    }
                }
                // A `Cell` (a by-reference-captured local's box) crosses the airlock by value as a
                // DEEP COPY: wire its inner value; `from_wire` rebuilds a FRESH independent cell, so a
                // plain captured local sent into a task is an isolated copy (design §4 F1) — ONE fresh
                // cell per BINDING, not per reference.
                //
                // W7-4: unlike every other arm this uses `memo.cells`, which is never popped. A cell is
                // a binding's identity, so ANY later reach (an off-stack sibling closure — `Ctr(inc,
                // get)` over one local `n` — as well as an on-stack letrec/mutual-recursion back-edge)
                // emits `Backref(id)` and the far side ties both references to the one rebuilt cell.
                // TICKET-100: data containers now keep identity the same way, via the separate `nodes`
                // map (see [`WireMemo`]) — a cell and a container are two cases of one rule. The `id`
                // is stable per cell for the whole serialization; `emitted` decides
                // definition-vs-`Backref`.
                Obj::Cell(v) => {
                    let id = match memo.cell_id(h) {
                        Some(id) => {
                            if memo.speculating {
                                memo.shortcut_at = Some(memo.next_id);
                            }
                            id
                        }
                        None => {
                            let id = memo.next_id;
                            memo.next_id += 1;
                            memo.cells.insert(h, id);
                            id
                        }
                    };
                    if memo.emitted.contains(&id) {
                        WireValue::Backref(id)
                    } else {
                        // W7-4a — journal the id we are about to insert so a DISCARDED speculative
                        // attempt removes it again (see `try_wire_speculative`).
                        if memo.speculating {
                            memo.emit_undo.push(id);
                        }
                        memo.emitted.insert(id);
                        let inner = self.to_wire_depth(*v, depth + 1, memo)?;
                        WireValue::Cell {
                            id,
                            inner: Box::new(inner),
                        }
                    }
                }
                // A cursor crosses by value as a DEEP COPY (like `List`): wire each snapshot item and
                // carry `pos`. It is plain data (a `Vec` + index), so — unlike a generator — it is
                // genuinely sendable, and `from_wire` rebuilds an independent cursor on the other side.
                // `deep_clone` already deep-copies a cursor the same way across the airlock for
                // `spawn`, so there is no separate gate here. Recursing through items means a cursor
                // over a non-sendable element faults recoverably, like a `list` of that element would.
                Obj::Iter { items, pos } => {
                    if let Some(id) = memo.seen(h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.mint_node(h);
                        memo.enter(h, id);
                        let pos = *pos;
                        let items = items.clone();
                        let mut out = Vec::with_capacity(items.len());
                        for x in items {
                            out.push(self.to_wire_depth(x, depth + 1, memo)?);
                        }
                        memo.exit(h);
                        WireValue::Iter {
                            id,
                            items: out,
                            pos,
                        }
                    }
                }
            },
        })
    }
}

#[cfg(test)]
mod doomed_tests {
    use super::*;

    /// TICKET-119 — the program-level lock
    /// (`airlock_module_global_skip_honours_the_depth_the_doom_was_recorded_at` in `src/vm/tests.rs`)
    /// stayed green with `depth >= d0` deleted from `WireMemo::doomed`, because that program's
    /// invalidation masked the term's effect. This proves the term directly, against the predicate
    /// itself, so the mutation cannot hide behind a program-level side effect.
    #[test]
    fn doomed_requires_depth_at_or_above_the_recorded_depth() {
        let h = GcRef(1);
        let mut memo = WireMemo {
            doom: [(h, (7, 10))].into_iter().collect(),
            ..Default::default()
        };
        assert!(!memo.doomed(h, 9), "depth below d0 must not be doomed");
        assert!(memo.doomed(h, 10), "depth at d0 must be doomed");
        assert!(memo.doomed(h, 11), "depth above d0 must be doomed");

        memo.doom.clear();
        assert!(!memo.doomed(h, 10), "no record at all must not be doomed");
    }
}
