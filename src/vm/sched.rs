// vm::sched — split out of vm/mod.rs. `super::*` == the `vm` module.
// Concurrency: spawn/nursery/fibers, MN scheduler, wire (airlock), module snapshots.

use super::*;

/// Identity-preservation state threaded through [`Vm::to_wire_depth`] — every identity-preserved node
/// kind (`Obj::Cell`/`Obj::Closure` AND the container arms `List`/`Tuple`/`Map`/`Set`/`Struct`/`Enum`/
/// `NewType`/`Iter`) records its GcRef here so ANY value cycle round-trips: a recursive local `fn`'s
/// letrec self-cell, a mutually-recursive closure pair, a self-referential struct/list/map, or a mixed
/// struct+closure cycle. `path` maps each such GcRef **currently on the serialize DFS stack** to the
/// `id` assigned on its first visit; `next_id` is the monotonic id counter. On a REVISIT of a node still
/// in `path` (a true back-edge) the arm emits `WireValue::Backref(id)` and stops; the node is REMOVED
/// from `path` on DFS exit, so an acyclic DAG alias revisited off-stack is re-serialized as an
/// independent deep copy (preserving the deep-copy-independence contract for both closures and data).
/// A `Generator` is the sole remaining unpreserved container (its parked frame holds no id). It is NOT
/// identity-preserved (its parked frame can't back-reference), so a cycle re-entering the SAME generator
/// on the DFS stack is REJECTED cleanly (`gens_on_stack`): with the containers now back-referencing, the
/// depth cap no longer trips on such a cycle, and re-serializing the generator would silently DUPLICATE
/// it — the e8dcad7 wrong-result class. The documented backstop is thus two-pronged: an acyclic parked
/// slot too deep trips the depth cap; a generator inside a value cycle trips `gens_on_stack`.
///
/// **W7-4 — `Obj::Cell` is the ONE exception to back-edge-only.** A cell is not a data node, it is a
/// *binding's identity*: the language rule (docs/syntax.md) is that a write through a capture is
/// visible "across sibling closures", and a crossing snapshot-copies the binding into ONE independent
/// per-task cell — one per BINDING, not one per reference. Under the pop-on-exit `path` discipline two
/// sibling closures over the same local (`Ctr(inc, get)`) reach that cell off each other's DFS stack,
/// so it was re-serialized twice and the shared binding silently SPLIT on the far side. So cells live
/// in the separate `cells` map, which is NEVER popped: one `WireValue::Cell` per cell per
/// serialization *scope*, every later reach emitting `Backref`. Everything else keeps `path` exactly as
/// it was — the documented DATA rule (an acyclic DAG alias is two independent deep copies, never
/// collapsed) is deliberate and unchanged.
///
/// **`elem_split` — cell scope for a value STORED in a cross-heap box.** `RwShared`'s zero-copy read
/// views drain ONE stored wire through MANY independent `from_wire` rebuilds, so a depth-1 element
/// carrying a `Backref` to a cell defined in a SIBLING element would hit `from_wire_memo`'s `.expect`
/// (a host panic) — the id is not in that piece's rebuild map. So every cross-heap STORE
/// ([`Vm::to_wire_crossable`]) serializes with `elem_split`: `gen` bumps on entry to each depth-1
/// node, and a cell is re-emitted as a FULL `WireValue::Cell` (same id) the first time each depth-1
/// subtree reaches it. Every depth-1 subtree is then self-contained, and `from_wire_memo` DEDUPES by
/// id (second definition resolves to the first rebuild), so a whole-value rebuild (`Channel.recv`,
/// `Shared.get`, `RwShared.get`) still ties every reference to ONE cell.
///
/// ponytail: the ceiling is WIRE SIZE — a cell reached from k depth-1 subtrees is serialized k times
/// (its inner graph re-expands, though only once per subtree, so it stays linear in k). Only a stored
/// value whose top-level elements share a binding pays it. Upgrade path if it ever matters: hoist cell
/// definitions into a side table on the stored wire so a piece can resolve ids without carrying them.
/// A piece whose cycle closes through the ROOT container still cannot be self-contained (the node it
/// needs IS the container). That used to `.expect`-abort the host in the copy-out views (W7-11); it is
/// now handled on the REBUILD side by [`Vm::from_wire_piece`], which rebuilds the whole container for
/// that one case. Do NOT try to fix it here by re-emitting container definitions the way `elem_split`
/// re-emits cells: a container re-emitted into every depth-1 subtree is O(n²) wire size, which is the
/// cliff `rwshared_view_over_shared_bindings_is_not_quadratic` exists to catch.
#[derive(Default)]
struct WireMemo {
    /// GcRef of an identity-preserved node (`Closure`/container) currently on the serialize DFS
    /// stack → the `id` assigned on its first visit. A revisit while still in `path` is a true back-edge
    /// → `Backref(id)`; removed on DFS exit so an off-stack alias is deep-copied independently.
    path: super::fxhash::FxHashMap<GcRef, u32>,
    /// W7-4 — GcRef of every `Obj::Cell` seen ANYWHERE in this serialization → (its `id`, the `gen` it
    /// was last EMITTED under). Never removed: a cell is a binding, so reaching it again under the same
    /// `gen` (off-stack sibling closure, or on-stack letrec back-edge) emits `Backref` and the far side
    /// rebuilds exactly one cell per binding. Scope discipline: a serialize memo's lifetime must equal
    /// its `from_wire_memo` rebuild map's, or a `Backref` minted under one memo hits the other's
    /// `.expect` — see [`Vm::to_wire_memo_at`].
    cells: super::fxhash::FxHashMap<GcRef, u32>,
    /// W7-4c — a READ-ONLY base consulted on a `cells` miss: this view's snapshot cell registry
    /// ([`Vm::snapshot_cells`]), shared by `Arc` rather than copied. Every `spawn` seeds a memo from
    /// it, so copying would put an O(module-global cells) clone on the spawn path — the same O(M·K)
    /// shape W7-4 already rejected for `to_snap`'s speculative rollback. Never written: new ids go to
    /// `cells`, which shadows it, so `try_wire_speculative`'s rollback stays exact.
    base_cells: Option<Arc<super::fxhash::FxHashMap<GcRef, u32>>>,
    /// Ids from `cells` already EMITTED (as a full `WireValue::Cell`) under the current `gen`. Equal to
    /// `cells`' id set unless `elem_split` is on.
    emitted: super::fxhash::FxHashMap<u32, u32>,
    /// W7-4a — undo journal for [`emitted`](WireMemo::emitted) while a SPECULATIVE attempt is in
    /// flight: `(id, the entry it replaced)`. `try_wire_speculative`'s rollback used to be complete
    /// with `emitted.retain(|id, _| *id < mint_from)`, because every id in the memo had been minted by
    /// the current module's own walk — so "id below the watermark" meant "really emitted". Once the
    /// memo spans MODULES (`snapshot_modules`) that stopped holding: a discarded attempt can mark an
    /// id minted in an EARLIER module, `retain` keeps it as if it were real, and the module's kept
    /// encoding then emits a `Backref` whose definition it never wrote — a dangling ref that rebuilds
    /// a closure over `nil` and trips `CellLoad on a non-handle value`. Recorded only while
    /// `speculating`, so the non-speculative paths pay nothing.
    emit_undo: Vec<(u32, Option<u32>)>,
    /// True for the duration of one [`Vm::try_wire_speculative`] attempt. Never nests — the only
    /// callers are `to_snap_depth`'s two speculative sites, and the attempt runs `to_wire_depth`,
    /// which never re-enters `to_snap_depth` (asserted by the empty-`path` `debug_assert` at entry).
    speculating: bool,
    /// Bumped on entry to each depth-1 node when `elem_split` — see the type doc.
    elem_gen: u32,
    /// Re-emit a cell's full definition once per depth-1 subtree (cross-heap stores only).
    elem_split: bool,
    next_id: u32,
    /// GcRefs of `Obj::Generator`s currently on the serialize DFS stack. A generator carries no id (its
    /// parked frame can't be a `Backref` target), so re-entering one still on the stack is a cycle
    /// through a non-preservable node → reject (never duplicate). Removed on DFS exit, so a generator
    /// revisited off-stack (an acyclic DAG alias) is deep-copied independently, like the containers.
    gens_on_stack: super::fxhash::FxHashSet<GcRef>,
}

impl WireMemo {
    /// W7-4c — the id this cell already crosses under, from the overlay or the shared base.
    fn cell_id(&self, h: GcRef) -> Option<u32> {
        self.cells.get(&h).copied().or_else(|| {
            self.base_cells
                .as_ref()
                .and_then(|base| base.get(&h).copied())
        })
    }
}

impl Vm {
    /// `spawn f(args)` / `spawn recv.m(args)` — pop `argc(+1)` operands, deep-copy the args (and, for
    /// the method form, the receiver) across the airlock, and register the task on the innermost
    /// nursery. The callee passes by handle (like `defer`); only data crosses the airlock.
    pub(super) fn do_spawn(
        &mut self,
        method: Option<String>,
        argc: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let raw_args: Vec<Value> = self.stack.split_off(at);
        let head = self.pop();
        // W7-4: everything that crosses here crosses in ONE serialization, so two sibling closures
        // over the SAME captured local (`spawn work(c.inc, c.get)`) still share their one binding on
        // the far side. Root-by-root `deep_clone` gave each its own [`WireMemo`] → one cell per
        // REFERENCE, silently splitting the binding. A capture-free `spawn f(…)` callee still keeps
        // the cheap shared handle (see [`spawn_callee_crosses_deep`](Vm::spawn_callee_crosses_deep)),
        // so it stays out of the batch.
        //
        // ARGS FIRST, receiver/callee LAST — the pre-batch order (every arg was `deep_clone`d, then the
        // receiver / `cross_spawn_callee`). Serialization order is observable when two of them are
        // non-crossable in DIFFERENT ways (a depth-cap arg vs a reference-cycle callee): the first
        // failure is the reported fault. `lower_task` keeps the same args-before-captures order.
        //
        // W7-4c — PIN THE SNAPSHOT FIRST. The clone below mints fresh cells, and they can only be tied
        // to the module snapshot's ids if those ids already exist; pinning inside `register_task`
        // (below) is too late for the first spawn of a view. No user code runs between here and there,
        // so the pinned VALUES are identical — only which fault wins changes when BOTH the snapshot
        // build and the crossing are non-viable, and the snapshot's fault is the more fundamental one.
        let pin = self.pin_snapshot(span)?;
        let cross_head = method.is_some() || self.spawn_callee_crosses_deep(head);
        let mut batch = raw_args;
        if cross_head {
            batch.push(head);
        }
        let (mut crossed, cell_ids) = self.deep_clone_all(batch, span)?;
        let head = if cross_head {
            crossed.pop().expect("the head was pushed last")
        } else {
            head
        };
        let args = crossed;
        let task = match method {
            Some(name) => PendingCall::Method {
                recv: head,
                name,
                args,
                span,
            },
            None => PendingCall::Call {
                callee: head,
                args,
                span,
            },
        };
        self.register_task(task, span, pin, cell_ids)
    }

    /// Does a `spawn f()` **callee** have to cross the task boundary by DEEP value? A closure that
    /// captures locals holds them (uniformly) as `Obj::Cell` handles; sharing that closure by handle
    /// would let the task alias the parent's cells — a serial-vs-M:N divergence, since the M:N engine's
    /// `prepare_worker`/`to_snap` path already deep-copies a task closure's captures into fresh cells.
    /// So a *capture-bearing* closure joins the [`deep_clone_all`](Vm::deep_clone_all) batch in
    /// [`do_spawn`](Vm::do_spawn), snapshotting its cells at spawn time on BOTH engines. A capture-free
    /// callable (plain `Obj::Func`, a builtin, or a closure with no captures) keeps the cheap shared
    /// handle — it holds no mutable captured state, so sharing is observationally identical to copying
    /// (and preserves the closure hot path). `Shared`/`RwShared`/`Atomic`/`Channel` captures still cross
    /// by reference (their `Arc` core is deep-copied as the same `Arc`), so `Shared`-based cross-task
    /// sharing is unaffected.
    ///
    /// W7-4 folded the old `cross_spawn_callee` (a SEPARATE `wire_callable` → `from_wire` round-trip)
    /// into that batch so a callee and an arg closing over the same local keep one cell. The
    /// `wire_callable` step's only extra was `ensure_crossable`; [`lower_task`](Vm::lower_task) applies
    /// it to every one of this closure's captures with the same span, and a `Closure` wire's
    /// `has_handle` is exactly the OR over its captures' — so nothing is dropped (and the guard is
    /// source-unreachable anyway: `module` is not a nameable type).
    fn spawn_callee_crosses_deep(&self, callee: Value) -> bool {
        match callee.as_obj() {
            Some(h) => {
                matches!(self.heap.get(h), Obj::Closure { captured, .. } if !captured.is_empty())
            }
            None => false,
        }
    }

    /// `spawn:` block — snapshot the captured bindings from the current frame (like `MakeClosure`),
    /// deep-copy each captured value across the airlock, build a zero-arg closure over the synthetic
    /// block proto, and register it as a `Call` task (captured locals deep-copied; home globals by
    /// handle).
    pub(super) fn do_spawn_block(
        &mut self,
        proto: ProtoId,
        entries: &[CapEntry],
        span: Span,
    ) -> Result<(), RuntimeError> {
        let frame = self.frames.last().unwrap();
        let (base, home, enclosing) = (frame.base, frame.home, frame.closure);
        let mut captured = Vec::with_capacity(entries.len());
        for e in entries {
            let v = match e.src {
                CapSrc::Slot(i) => self.stack[base + i],
                CapSrc::Captured(parent_slot) => enclosing
                    .and_then(|h| match self.heap.get(h) {
                        Obj::Closure { captured, .. } => {
                            captured.get(parent_slot as usize).copied()
                        }
                        _ => None,
                    })
                    .unwrap_or(Value::nil()),
            };
            captured.push(v);
        }
        // Deep-copy across the airlock: the task can't share mutable state with the parent.
        // Positional (lever #3): slot order matches the synthetic block proto's `capture_names`.
        // W7-4: ONE serialization for all captures — two captured closures over the same local
        // (`gi := c.inc; gg := c.get`) must reach the task sharing their single binding.
        // W7-4c: and the snapshot is pinned BEFORE the clone, so a capture over a cell a module global
        // also holds crosses under that global's id — see `do_spawn`.
        let pin = self.pin_snapshot(span)?;
        let (captured, cell_ids) = self.deep_clone_all(captured, span)?;
        let h = self.heap.alloc(Obj::Closure {
            proto,
            captured,
            home,
        });
        self.register_task(
            PendingCall::Call {
                callee: Value::obj(h),
                args: Vec::new(),
                span,
            },
            span,
            pin,
            cell_ids,
        )
    }

    /// W7-4c — the nursery guard + [`Vm::ensure_snapshot`], split out of [`Vm::register_task`] so a
    /// `spawn` can pin its module view BEFORE `deep_clone_all` mints the task's cells. The guard stays
    /// FIRST: `spawn` outside a `parallel:` must still report that, not a snapshot fault.
    ///
    /// The `Result` is CARRIED, not raised (W6-2): a snapshot that cannot be built — a module global
    /// holding a frame-holding generator — faults where the task is PREPARED, so a nursery whose tasks
    /// are all cancelled before preparation stays faultless.
    fn pin_snapshot(
        &mut self,
        span: Span,
    ) -> Result<Result<Arc<ModuleSnapshot>, RuntimeError>, RuntimeError> {
        if self.nurseries.is_empty() {
            return Err(self.err("spawn must be inside a parallel: block".to_string(), span));
        }
        Ok(self.ensure_snapshot(span))
    }

    /// Register a spawned task on the innermost nursery. Per-connection spawn: if that nursery is
    /// EAGER, build the handler into a live [`Fiber`] (serializing its args out of THIS fiber's heap,
    /// the same airlock copy `do_spawn`'s `deep_clone` does) and [`MnSched::inject`] it straight into
    /// the running sched — it runs concurrently with the rest of the body. The `task_index` is the
    /// scope's monotonic `next_index` (spawn order), so Decision-F output stays deterministic.
    /// Otherwise (lazy/top-level) push the [`QueuedTask`] for the join to drain. The checker guarantees
    /// a `parallel:` is open, but we guard here anyway and return a runtime error instead of panicking.
    ///
    /// W6-2 — THE PIN INSTANT: the task's module view is snapshotted HERE, at its own `spawn`, on
    /// both the eager and the lazy path. `ensure_snapshot`'s cache makes consecutive
    /// spawns cheap; a build failure is CARRIED on the task and raised where it is prepared.
    pub(super) fn register_task(
        &mut self,
        task: PendingCall,
        span: Span,
        snap: Result<Arc<ModuleSnapshot>, RuntimeError>,
        cell_ids: CellIds,
    ) -> Result<(), RuntimeError> {
        // The innermost open nursery (`nurseries`, `mn_scopes` and `eager_scheds` are lockstep).
        // W7-4c — `snap` was pinned by `pin_snapshot`, which already ran this guard BEFORE the
        // caller's `deep_clone_all`; re-checking here keeps the invariant local and costs one compare.
        let Some(i) = self.nurseries.len().checked_sub(1) else {
            return Err(self.err("spawn must be inside a parallel: block".to_string(), span));
        };
        // Eager innermost nursery → inject a live fiber. Clone the sched Arc, drop the borrow so
        // `prepare_worker` can take `&mut self`; `inject` assigns the real slot index under its lock
        // (the `0` placeholder is overwritten), so no caller-side index bookkeeping is needed.
        if let Some(Some(scope)) = self.eager_scheds.last() {
            let sched = Arc::clone(&scope.sched);
            // The eager nursery owns its OWN sched (a single scope 0 — see `activate_eager_nursery`);
            // `inject` overwrites the `0` placeholder `task_index` under its lock. This path PREPARES the
            // task right here, so a snapshot build failure surfaces right here too (prepare instant).
            let sid = scope.scope;
            let fiber = self
                .prepare_worker(task, Some(snap?), &cell_ids)?
                .into_fiber(0, sid);
            sched.inject(fiber, sid);
            return Ok(());
        }
        self.nurseries[i].push(QueuedTask {
            call: task,
            snap,
            cell_ids,
        });
        Ok(())
    }

    /// `parallel:` dedent — run the nursery's spawned tasks as cooperative fibers (B1/B2). The
    /// joining (parent) fiber is parked while the children run; a child that blocks on an empty
    /// `recv` suspends and the scheduler switches to a runnable sibling, resuming it once a sibling
    /// `send`s. A child that never blocks runs to completion before the next starts — identical to
    /// the old FIFO run-to-completion drain, so non-blocking programs are byte-for-byte unchanged.
    /// The first child fault (or `std.os.exit`) aborts the remaining siblings and propagates; on that
    /// path the parent's restored `run_until` handles `recover:`/unwind in its own context.
    /// TASK B — cancel a `parallel:` body's tasks when it escapes its `JoinNursery` early (`?` /
    /// `return` / `break` / `continue`) or when a fault unwinds past it. Pop every nursery entry ABOVE
    /// `from_len` (the level the escaping construct should restore to), innermost-first: an eager or
    /// early-enlisted nursery's tasks are LIVE fibers, so they are cancelled + drained + flushed; a
    /// lazy nursery's entries never started, so they are simply DROPPED (no fiber to cancel, no
    /// buffered output to flush). Depth returns to `from_len` — the old `truncate`'s no-leak behavior
    /// — at every reclaim site.
    ///
    /// §2c1 — this used to write ONE observable line per lazy nursery
    /// (`"{n} pending task(s) cancelled on early exit from parallel:"`). **That report is deleted.**
    /// A task now starts at its `spawn`, so on the M:N engine there are no unstarted tasks to count,
    /// and any residual count would be racy — a task injected into the run queue may or may not have
    /// been picked up before the escape. A confident wrong number on stdout is worse than no line
    /// (`docs/gaps.md` W7-12: an uncertain verdict must decline). trio and `asyncio.TaskGroup` print
    /// nothing here either. The eager path never reported, so this also removes a pre-existing
    /// eager-vs-lazy observable split rather than creating one.
    pub(super) fn drain_escaped_nursery(&mut self, from_len: usize) {
        if self.nurseries.len() <= from_len {
            return; // nothing escaped past the join (e.g. normal fall-through already popped it)
        }
        while self.nurseries.len() > from_len {
            // All four stacks pop TOGETHER, unconditionally — `nurseries`, `nursery_defer_floors`,
            // `mn_scopes` and `eager_scheds` are lockstep, and the enlisted arm below `continue`s.
            self.nursery_defer_floors.pop();
            let mn_scope = self.mn_scopes.pop().flatten(); // Some if early-enlisted
            let eager = self.eager_scheds.pop().flatten(); // Some if this nursery is eager
            self.nurseries.pop(); // unstarted tasks — dropped, never run (see the doc above)
            // Cross-nursery flat scheduler — an EARLY-ENLISTED nursery's tasks are LIVE fibers already
            // seeded into the global sched (its `tasks` vec was drained), so an escape past its join must
            // CANCEL + drain them (trip the scope cancel, requeue parked, settle), exactly like an eager
            // nursery's `abort_eager_nursery`. (A nursery is never both: the enlist only happens on the
            // lazy path, so `eager` is `None` here.)
            if let Some(scope_id) = mn_scope {
                self.abort_enlisted_scope(scope_id);
                continue;
            }
            // Per-connection spawn / §2c1 — an eager nursery's tasks are already-started live fibers:
            // cancel + drain + flush them.
            if let Some(scope) = eager {
                self.abort_eager_nursery(scope);
            }
        }
    }

    pub(super) fn join_nursery(&mut self) -> Result<(), RuntimeError> {
        // Consume this nursery's tasks (FIFO). Popping the entry now (as the old drain did at the
        // end) keeps the parent's `Handler::nursery_len` accounting correct on a later fault.
        self.nursery_defer_floors.pop(); // keep the parallel floor stack in lockstep with `nurseries`
        let mn_scope = self.mn_scopes.pop().flatten(); // lockstep — Some if early-enlisted
        let tasks = self.nurseries.pop().unwrap_or_default();
        // Per-connection spawn — pop the eager scope in lockstep. An eager nursery injected its tasks
        // live (so `tasks` is empty); its join drains the handlers it spawned, not a queued list.
        if let Some(Some(scope)) = self.eager_scheds.pop() {
            return self.join_eager_nursery(scope);
        }
        // Cross-nursery flat scheduler — this nursery was EARLY-ENLISTED into the global sched (its
        // sibling tasks already seeded as a scope so a nested nursery's owner could run them). Its
        // `tasks` were drained, so join = run the inline owner of that scope (drain any still-parked
        // siblings), wait, and reduce THAT scope's slot sub-range (preserving per-nursery-join flush
        // order). See `run_mn_nursery_nested`.
        if let Some(scope_id) = mn_scope {
            self.join_enlisted_scope(scope_id)?;
            // A `spawn:` issued AFTER this nursery was enlisted refilled the drained `tasks` vec (the
            // enlist `take()` emptied it, but `mn_scopes[i]` stayed `Some`). Those late tasks were NOT
            // part of the enlisted scope — run them now, at the join, exactly as the lazy path below
            // (late spawns post-date the nested `inner()` join, so they have no live inner peer to
            // race with). Falls through to the normal task path:
            // `run_mn_nursery` routes them to the HELD sched (if an outer scope is still enlisted) as a
            // fresh TRAILING scope — `register_scope_seeded` is append-only so the flat slots stay contiguous,
            // and it un-latches a stale `terminate` so the inline owner runs the late task instead of
            // stopping on the prior-scopes-all-done flag — else to a fresh outermost sched once no sched
            // is held. No clobber of the held sched, no panic, no drop. (Cross-nursery flat scheduler — #3.)
            if tasks.is_empty() {
                return Ok(());
            }
        }
        if tasks.is_empty() {
            return Ok(());
        }
        // D2b: run the tasks as lightweight M:N fibers on the OS-thread pool (park-on-`recv`). This is
        // the only engine — the cooperative scheduler that used to live here was deleted with
        // `--serial` (`docs/future.md` §2b).
        self.run_mn_nursery(tasks)
    }

    /// D2b — the `--parallel` M:N engine: run a nursery's tasks as **lightweight fibers parked on
    /// `recv`** multiplexed over the bounded pool, the replacement for the legacy "one OS thread per
    /// task, block the thread on `recv`" model. The core M:N win: an empty `recv` parks the fiber and
    /// frees its worker instead of pinning the thread, so `#fibers ≫ #threads` producer/consumer
    /// workloads complete instead of starving.
    ///
    /// 1. **Prepare every task into a lightweight [`Fiber`]** ([`Vm::prepare_worker`] →
    ///    [`ReadyWorker::into_fiber`], serial, against the parent heap): each carries its own heap +
    ///    lazy-module roots + a `Pending` task.
    /// 2. **Seed the shared [`MnSched`]** (run queue + park set + per-task slots) and enlist workers:
    ///    the joining thread runs one shell loop inline (decision B — parent participates) and up to
    ///    `available_parallelism()-1` more shells are farmed to the pool. A shell is a thin host `Vm`
    ///    (shared module snapshot installed, sched/cancel wired); fibers swap their own heaps in/out.
    ///    Bounded by core count, so a nested `parallel:` never becomes thread-per-task.
    /// 3. **Park/wake**: an empty `recv` parks the fiber in the channel's wait set ([`MnSched::park`])
    ///    and the worker grabs the next fiber; a `send` drains that set back onto the run queue and
    ///    wakes a worker ([`MnSched::send_wake`]). Over-notify is correct (a spuriously-woken fiber replays
    ///    its rewound-ip `recv` and re-parks); targeted wake + the StoreLoad barrier are D4.
    /// 4. **Reduce** the per-task slots in task order ([`Vm::reduce_task_slots`]) — decision F output
    ///    flush + `Exit`-over-`Fault` precedence.
    ///
    /// B3.4 — **cancellation**: the shared `cancel: Arc<AtomicBool>` (cloned onto every shell) aborts
    /// running fibers at a dispatch back-edge and parked fibers via [`MnSched::cancel_drain`] (they are
    /// requeued and observe the flag on resume). B3.5 — **deadlock** is redefined as the exact
    /// predicate `running == 0 && runnable == 0 && parked > 0 && done < total`, evaluated atomically by
    /// [`MnSched::take_runnable`] (no barrier-confirm needed under a single coordinator). Residual
    /// hangs (decision D): deadlocks spanning nurseries or involving `Executor` work — `MnSched.parked`
    /// is per-nursery, so a cross-nursery `send` delivers the message but does not wake across scheds.
    pub(super) fn run_mn_nursery(&mut self, tasks: Vec<QueuedTask>) -> Result<(), RuntimeError> {
        // Cross-nursery flat scheduler — the OUTERMOST nursery (`self.mn.is_none()`) builds the ONE
        // global sched + farms helpers; a NESTED nursery (`self.mn.is_some()`) REUSES it (register a
        // scope, enlist into the same global run queue, run its inline owner scope-scoped). Because the
        // inline owner drains the GLOBAL queue, it naturally runs cross-nursery siblings — the case-A
        // fix (`docs/cross-nursery-flat-scheduler.md`). Nested owners farm NO helpers (they run inline
        // on the worker thread that called them).
        //
        // A THIRD case: `self.mn.is_none()` (no nested sched installed — the body runs `mn == None`) yet a
        // flat sched is HELD in `mn_enlist_sched` because an OUTER nursery was early-enlisted and has not
        // joined yet. This is a late `spawn:` into a non-outermost nursery (charge #3): the leftover late
        // tasks reach `JoinNursery` after the enlist drained the original vec. They must run on the HELD
        // sched as a fresh TRAILING scope (`register_scope_seeded` is append-only, so the flat slot ranges stay
        // contiguous) — NOT via `run_mn_nursery_outermost`, which would build a fresh sched and CLOBBER
        // the held one, leaving a later `join_enlisted_scope(scope_id)` to index a stale scope_id into the
        // fresh (len-1) sched → `index out of bounds` panic. Reusing the nested path is correct: the late
        // trailing scope reduces inline at its own join (it is NOT counted in `mn_enlisted`), exactly like
        // a nested nursery. Only when no sched is held do we build a fresh outermost sched.
        match (self.mn.clone(), self.mn_enlist_sched.clone()) {
            (Some(sched), _) => self.run_mn_nursery_nested(&sched, tasks),
            (None, Some(held)) => self.run_mn_nursery_nested(&held, tasks),
            (None, None) => self.run_mn_nursery_outermost(tasks),
        }
    }

    /// Cross-nursery flat scheduler — the OUTERMOST `parallel:` nursery (`self.mn.is_none()`): build the
    /// one global `MnSched`, farm helper shells, run the inline owner of scope 0, reduce, tear down.
    ///
    /// Case-A FIX: a `parallel:` body runs INLINE before its `JoinNursery`, so when *this* nursery is
    /// reached via a NESTED nursery's join (e.g. `inner()`'s implicit nursery joins while `main`'s outer
    /// `parallel:` still has an un-run sibling `O` queued), the outer sibling `O` is not yet in any
    /// scheduler. So the builder EARLY-ENLISTS every still-pending OUTER nursery (from `self.nurseries`)
    /// as its own scope — seeding `O` so the nested owner, draining the GLOBAL queue, can RUN it (the
    /// cross-nursery wake). But each enlisted scope's OUTPUT is reduced at ITS OWN `JoinNursery` (NOT
    /// here), so the per-nursery-join flush ORDER for non-blocking nested spawns stays deterministic.
    /// `self.mn_enlisted` counts those deferred scopes; `self.mn` stays installed
    /// until the LAST of them joins (`join_enlisted_scope` tears it down).
    pub(super) fn run_mn_nursery_outermost(
        &mut self,
        tasks: Vec<QueuedTask>,
    ) -> Result<(), RuntimeError> {
        let total = tasks.len();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut fibers = Vec::with_capacity(total);
        for (i, t) in tasks.into_iter().enumerate() {
            // W6-2 — each task replays the snapshot pinned at its own spawn. scope 0 — the outermost
            // nursery.
            fibers.push(
                self.prepare_worker(t.call, Some(t.snap?), &t.cell_ids)?
                    .into_fiber(i, 0),
            );
        }
        let deadlock_err = self.err(DEADLOCK_MSG.to_string(), Span::RUNTIME);
        // Worker count must account for the early-enlisted OUTER scopes' tasks too (case-A: `main`'s `O`),
        // so a multi-task inner nursery + outer siblings still gets real parallelism. We don't yet know
        // the outer totals here, so size to a reasonable upper bound (core count) capped by total work
        // after enlisting is impossible to know pre-register; use core count (the inline owner alone
        // still guarantees completion, helpers only accelerate).
        let nworkers = worker_count();
        let mut inner = MnSched::new(
            total,
            nworkers,
            Arc::clone(&cancel),
            deadlock_err,
            self.heap.mem_cap(),
        );
        // gaps.md W7-56 — the deadlock predicate must see this run's outstanding eager `Executor`
        // jobs (uncounted senders). Assigned here, not through `new`, so the predicate's unit
        // fixtures keep an empty registry.
        inner.exec_registry = Arc::clone(&self.exec_registry);
        // gaps.md W7-58 — so an idle worker of this sched can JUDGE the process-wide verdict on
        // behalf of a nursery owner, which never reaches `block_halt_check`.
        inner.quiesce = Arc::clone(&self.quiesce);
        let sched = Arc::new(inner);
        // W7-56 — publish the sched so an eager job's `send`/`close` (which runs with no sched of its
        // own) can wake a fiber parked here: `Vm::wake_on_send`.
        self.register_sched(&sched);
        sched.lock().scopes[0].ancestors = self.scope_ancestors();
        sched.seed(fibers);
        // Early-enlist OUTER still-pending nurseries (case-A: `main`'s sibling `O` when the builder is a
        // nested join) — BEFORE farming any helper or starting the owner, so EVERY scope's fibers are
        // seeded (runnable-accounted) before any worker can run scope 0 to a park: else a helper could
        // run scope 0's fiber to a park and trip the global deadlock predicate before `O` is enlisted (a
        // multi-task inner nursery race). The sched is held in `mn_enlist_sched` (NOT `self.mn` — the
        // inline body must run with `mn == None` so it does not take the worker-only yield/park paths).
        // Each enlisted scope reduces at its OWN `JoinNursery` (deferred — preserves per-nursery order).
        // Install `mn_enlist_sched` only AFTER a SUCCESSFUL enlist that actually deferred a scope: an
        // early `?` (a `prepare_worker` backstop on a non-crossable spawn) must leave NO stale sched
        // behind for a later `join_enlisted_scope`/inline-send to pick up; and if nothing was enlisted
        // there are no deferred joins, so it stays `None` (clean teardown at this owner's reduce).
        self.early_enlist_outer(&sched)?;
        if self.mn_enlisted > 0 {
            self.mn_enlist_sched = Some(Arc::clone(&sched));
        }
        // NOW farm helper shells — SENTINEL (drain the global queue across all scopes until global
        // terminate). Farming AFTER the enlist closes the deadlock-predicate race above.
        for wid in 1..nworkers {
            let mut shell = self.spawn_shell(&sched, &cancel);
            let sched = Arc::clone(&sched);
            pool::submit(Box::new(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    shell.mn_worker_loop(&sched, wid, SENTINEL_SCOPE)
                }));
            }));
        }
        let mut shell = self.spawn_shell(&sched, &cancel);
        {
            // gaps.md W7-58 — this thread is now blocked in a nursery join; make it visible to the
            // process-wide verdict for exactly that span (RAII, dropped before the reduce).
            let _party = self.nursery_party_guard(&sched);
            shell.mn_worker_loop(&sched, 0, 0); // owner of scope 0
            // The owner returned on scope 0; reduce scope 0's sub-range. The sched is released only when
            // no early-enlisted outer scope is still pending (else those scopes' slots must survive
            // until their own joins reduce them — `join_enlisted_scope` releases it at the last).
            sched.wait_for_scope(0);
        }
        let slots = sched.take_scope_slots(0);
        if self.mn_enlisted == 0 {
            self.mn_enlist_sched = None;
        }
        self.reduce_task_slots(slots)
    }

    /// Cross-nursery flat scheduler — a NESTED `parallel:` nursery reusing the ONE global sched. EARLY-
    /// ENLIST every still-pending OUTER nursery (so a cross-nursery sibling like case-A's `O` is seeded
    /// and the inline owner — draining the GLOBAL queue — can run it), recording each enlisted scope on
    /// `self.mn_scopes` so its OWN `JoinNursery` reduces it (deferred — preserves per-nursery flush
    /// order). Then register + seed this nursery's OWN scope, run its inline owner SCOPE-SCOPED (returns
    /// the instant ITS scope is done, having drained the global queue meanwhile), wait, reduce its
    /// sub-range. Farms NO helpers (runs inline on the worker thread that called it, reusing `self.wid`).
    pub(super) fn run_mn_nursery_nested(
        &mut self,
        sched: &Arc<MnSched>,
        tasks: Vec<QueuedTask>,
    ) -> Result<(), RuntimeError> {
        // W6-2 — each task replays the snapshot pinned at its own spawn: on a worker fiber that is a
        // FRESH snapshot of the TASK's own (possibly mutated) view, not the parent module's frozen copy.
        // This nursery's OWN scope. Prepare every worker FIRST (the fallible/heap-heavy step — touches no
        // scheduler state), THEN register the scope and seed its fibers atomically. Doing the prepare
        // BEFORE registration is what makes `register_scope_seeded` race-free: there is no window where the
        // scope exists with `runnable == 0` (the old `register_scope` → prepare_worker → `seed` ordering
        // left exactly that gap, which on the late-spawn-into-middle path — inline builder not counted in
        // `running` — let a SENTINEL helper fault an innocent parked outer sibling). On a `prepare_worker`
        // Err no scope is registered (clean unwind).
        let cancel = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::with_capacity(tasks.len());
        for t in tasks {
            workers.push(self.prepare_worker(t.call, Some(t.snap?), &t.cell_ids)?);
        }
        let scope_id =
            sched.register_scope_seeded(Arc::clone(&cancel), self.scope_ancestors(), workers);
        let wid = self.wid;
        let mut shell = self.spawn_shell(sched, &cancel);
        {
            // gaps.md W7-58 — a no-op on the `(Some(sched), _)` path (a fiber's nested nursery runs on
            // a worker shell, `mn.is_some()`); it registers only on the `(None, Some(held))` late-spawn
            // path, where `self` really is the top-level builder blocked in this join.
            let _party = self.nursery_party_guard(sched);
            shell.mn_worker_loop(sched, wid, scope_id);
            sched.wait_for_scope(scope_id);
        }
        let slots = sched.take_scope_slots(scope_id);
        self.reduce_task_slots(slots)
    }

    /// Cross-nursery flat scheduler — EARLY-ENLIST every OUTER still-pending nursery (those above the
    /// current one on `self.nurseries`) into `sched` as its own scope: seed its sibling tasks as live
    /// fibers (so a nested owner draining the GLOBAL queue can run them — the cross-nursery wake), drain
    /// its `tasks` vec, and record the scope on `self.mn_scopes` + bump `self.mn_enlisted` so its OWN
    /// `JoinNursery` reduces it (deferred — preserving per-nursery flush order).
    /// Idempotent per nursery (skips any already-enlisted `Some(_)` and any empty one).
    pub(super) fn early_enlist_outer(&mut self, sched: &Arc<MnSched>) -> Result<(), RuntimeError> {
        // Independent/normal multi-level nesting is fully supported: every still-pending OUTER nursery is
        // enlisted as its own scope here, and the genuinely-CONTENDED case (2+ live receivers racing ONE
        // channel across nested nurseries) is NOT gated — it is concurrent-divergent by design (delivery
        // order may vary between runs, or it may deadlock-fault; suspendable concurrency is VM-only,
        // see PROGRESS.md). It must only never PANIC.
        for i in 0..self.nurseries.len() {
            if self.mn_scopes[i].is_some() || self.nurseries[i].is_empty() {
                continue;
            }
            let total = self.nurseries[i].len();
            // VALIDATE THEN COMMIT (atomic enlist — charge #4). Prepare every task from a CLONE first,
            // BEFORE any irreversible mutation. `prepare_worker` is the only fallible step (the checker
            // gates non-crossable spawns, so this backstop normally never fires — but if it did, an early
            // `?` here must not leave a half-state). On `Err` the nursery is untouched (originals still in
            // `self.nurseries[i]`), no scope is registered (so no unseeded scope can hang `wait_for_scope`)
            // and `mn_scopes`/`mn_enlisted` are unbumped — the fault propagates cleanly.
            let clones: Vec<QueuedTask> = self.nurseries[i].clone();
            // W6-2 — each OUTER task replays the pin taken at ITS OWN spawn, never one taken here:
            // enlisting happens at a NESTED nursery's join, so snapshotting here would diverge for a
            // global mutated in between.
            let mut prepared = Vec::with_capacity(total);
            for t in clones {
                prepared.push(self.prepare_worker(t.call, Some(t.snap?), &t.cell_ids)?);
            }
            // COMMIT — nothing fallible remains. Discard the originals (the clones became the fibers),
            // register + seed the scope, and record it for its OWN `JoinNursery` to reduce.
            let _ = std::mem::take(&mut self.nurseries[i]);
            let cancel = Arc::new(AtomicBool::new(false));
            let scope_id = sched.register_scope(total, Arc::clone(&cancel), self.scope_ancestors());
            // Mark the scope as awaiting the builder's own join: its parked fibers have the live builder
            // body as a feeder, so the deadlock predicate must not fault them until the builder reaches
            // this scope's `JoinNursery` (which clears the flag). (Cross-nursery flat scheduler — #1/#2.)
            let base = {
                let mut c = sched.lock();
                c.scopes[scope_id].awaiting_builder = true;
                c.scopes[scope_id].base_index
            };
            let fibers: Vec<Fiber> = prepared
                .into_iter()
                .enumerate()
                .map(|(j, p)| p.into_fiber(base + j, scope_id))
                .collect();
            sched.seed(fibers);
            self.mn_scopes[i] = Some(scope_id);
            self.mn_enlisted += 1;
        }
        Ok(())
    }

    /// Cross-nursery flat scheduler — `JoinNursery` for a nursery that was EARLY-ENLISTED: its tasks are
    /// already live fibers in `sched` (seeded by `early_enlist_outer`). Run the inline owner of that
    /// scope to drain any still-parked siblings, wait for the scope, reduce its slot sub-range (deferred
    /// flush — preserves per-nursery order), and release the held sched once the last enlisted scope
    /// joins. Runs on the INLINE builder VM (`self.mn == None`), so the owner loop is on a SHELL.
    pub(super) fn join_enlisted_scope(&mut self, scope_id: usize) -> Result<(), RuntimeError> {
        let sched = self
            .mn_enlist_sched
            .clone()
            .expect("join_enlisted_scope without a held sched");
        // The builder has reached THIS scope's join — it is no longer feeding it from body code, it is now
        // blocked draining it. Clear `awaiting_builder` so a genuine post-body deadlock (this scope parked
        // with no live sender) faults instead of being vetoed. (Cross-nursery flat scheduler — #1/#2.)
        sched.lock().scopes[scope_id].awaiting_builder = false;
        let cancel = Arc::clone(&sched.lock().scopes[scope_id].cancel);
        let wid = self.wid;
        // W6-2 — a shell needs no snapshot of its own: it runs no code, and every fiber it schedules in
        // carries its own module view + snapshot (`FiberCtx`).
        let mut shell = self.spawn_shell(&sched, &cancel);
        {
            // gaps.md W7-58 — the enlisted-scope join blocks this (inline builder) thread too.
            let _party = self.nursery_party_guard(&sched);
            shell.mn_worker_loop(&sched, wid, scope_id);
            sched.wait_for_scope(scope_id);
        }
        let slots = sched.take_scope_slots(scope_id);
        self.mn_enlisted -= 1;
        if self.mn_enlisted == 0 {
            self.mn_enlist_sched = None;
        }
        self.reduce_task_slots(slots)
    }

    /// Cross-nursery flat scheduler — reclaim an EARLY-ENLISTED scope whose nursery ESCAPED past its
    /// join (`?`/`return`/`break`/`continue`/caught fault). Its tasks are live fibers, so cancel them
    /// (trip the scope cancel, drain, settle — like `abort_eager_nursery`), reduce (only `os.exit`
    /// honored — the escape error is what propagates), and release the held sched at the last scope.
    pub(super) fn abort_enlisted_scope(&mut self, scope_id: usize) {
        let Some(sched) = self.mn_enlist_sched.clone() else {
            return;
        };
        // N4 — ARM the cancel-teardown veto BEFORE clearing `awaiting_builder` (which is the veto that
        // has been holding the deadlock predicate off this scope): a GAPLESS handoff. Clearing first —
        // as this did — leaves a window in which the scope has NEITHER veto, and an idle worker's
        // `take_runnable` landing in it sees the quiesce (`running == 0` — the inline builder is not
        // counted — `runnable == 0`, `parked_n > 0`) and declares a spurious DEADLOCK, whose
        // `flag_deadlock` DROPS this scope's parked fibers without `unwind_deferred` (their `defer`s
        // never run) and reaps every OTHER scope's parked fibers too (it sets global `terminate`).
        // `trip_scope_cancel` stores under the core lock, so the flag is published to any worker that
        // takes that lock to evaluate the predicate. THEN clear `awaiting_builder` so the cancel quiesce
        // is observed promptly rather than vetoed. (Cross-nursery flat scheduler — #1/#2.)
        sched.trip_scope_cancel(scope_id);
        let cancel = {
            let mut c = sched.lock();
            c.scopes[scope_id].awaiting_builder = false;
            Arc::clone(&c.scopes[scope_id].cancel)
        };
        sched.cancel_drain(scope_id);
        poller::drain_sched(&sched);
        let wid = self.wid;
        // W6-2 — a shell needs no snapshot (see `join_enlisted_scope`), which also retires the
        // `.expect("no fault possible")` this teardown path used to carry.
        let mut shell = self.spawn_shell(&sched, &cancel);
        {
            // gaps.md W7-58 — the cancel-teardown drain blocks this thread just as the normal join does.
            let _party = self.nursery_party_guard(&sched);
            shell.mn_worker_loop(&sched, wid, scope_id);
            sched.wait_for_scope(scope_id);
        }
        let slots = sched.take_scope_slots(scope_id);
        self.mn_enlisted -= 1;
        if self.mn_enlisted == 0 {
            self.mn_enlist_sched = None;
        }
        let _ = self.reduce_task_slots(slots); // escape error propagates; only os.exit honored here
    }

    /// Per-connection spawn — the EAGER counterpart to [`Vm::run_mn_nursery`], split across the
    /// `parallel:` body. Activate at `EnterNursery`: build an empty live [`MnSched`] (`total` grows as
    /// the body `inject`s handlers), flag its body open (so a transient `done == total` does not
    /// terminate it), and spawn ONE dedicated **raw OS thread** (`wid` 1) that drains injected handlers
    /// concurrently with the accept loop. `wid` 0 is the inline join worker ([`Vm::join_eager_nursery`]).
    ///
    /// Why a raw thread, not the bounded pool: the eager body has NO inline worker between
    /// `EnterNursery` and `JoinNursery`, so liveness during the body depends entirely on this drainer.
    /// A bounded-pool helper (the lazy path's accelerator) is the WRONG tool here — `available_parallelism()`
    /// can be 1 (no helper farmed at all → the body never drains → the sequential-client pattern
    /// deadlocks), and a long-running pool job per eager nursery exhausts the fixed pool under nesting
    /// (an undetectable hang, since `body_open` vetoes the deadlock predicate). A raw thread (like the
    /// D5-owe-#3 demote replacement) is unconditional and pool-independent — exactly one extra OS thread
    /// per open eager nursery, joined when the nursery completes. Handlers within one eager nursery
    /// multiplex over this one drainer + the join worker (M:N — handlers park on socket ops, so one
    /// thread serves many); multi-core handler parallelism arrives at the JOIN — see
    /// [`Vm::join_eager_nursery`], which farms the bounded pool once the body is closed.
    ///
    /// §2c1 — this now runs for the OUTERMOST nursery too (`mn.is_none()`, top-level `main`), which is
    /// what makes a `spawn` start at the `spawn`. Returns `None` when the drainer thread cannot be
    /// created, so the caller falls back to the lazy queue-at-join path: an eager scope with no
    /// drainer has NO worker at all between `EnterNursery` and `JoinNursery`, so a body that blocks
    /// (an accept loop) would hang outright.
    pub(super) fn activate_eager_nursery(&mut self) -> Option<EagerScope> {
        // §2c1 — a NESTED eager nursery on THIS thread joins the enclosing scope's sched as a new
        // SCOPE instead of building a private sibling sched. Two private scheds cannot wake each
        // other (`send_wake` scans its own sched then `wake_parent_chain`, strictly upward), which is
        // the cross-nursery deadlock the flat scheduler exists to prevent — see `EagerScope::scope`.
        //
        // Only when `mn.is_none()`. On a WORKER SHELL the enclosing eager scope belongs to a
        // different nursery generation and the private-sched-plus-`parent_wake` shape is the
        // per-connection-spawn design; that path is unchanged.
        if self.mn.is_none()
            && let Some(outer) = self.eager_scheds.iter().flatten().next_back()
        {
            let sched = Arc::clone(&outer.sched);
            let cancel = Arc::new(AtomicBool::new(false));
            let scope = sched.register_scope_seeded(
                Arc::clone(&cancel),
                self.nursery_ancestors(),
                Vec::new(),
            );
            sched.open_body(scope);
            return Some(EagerScope {
                sched,
                cancel,
                drainer: None, // the OWNER's drainer serves this scope too — it drains the global queue
                scope,
            });
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let deadlock_err = self.err(DEADLOCK_MSG.to_string(), Span::RUNTIME);
        // wid 0 = inline join worker, wid 1 = the dedicated raw drainer below, wids 2.. = the pool
        // helpers `join_eager_nursery` farms for an OUTERMOST scope. `MnSched::new` allocates the
        // per-worker local queues up front (`locals: (0..nworkers)`), so the count must be sized here
        // even though most of those workers are farmed later; an unused local queue is inert.
        let nworkers = worker_count().max(2);
        let mut inner = MnSched::new(
            0,
            nworkers,
            Arc::clone(&cancel),
            deadlock_err,
            self.heap.mem_cap(),
        );
        // gaps.md B5 — this eager sched is PRIVATE (no link to the parent). A `send`/`close` inside its
        // body only scans its OWN parked set, so a receiver parked in the PARENT nursery on a shared
        // channel is never woken → the parent spuriously faults `deadlock`. Point `parent_wake` at the
        // sched the activating worker fiber is running on (its parent nursery — held in `self.mn`, or
        // `mn_enlist_sched` on the inline outermost builder) so `send_wake`/`close_wake` route the wake
        // up to it. Strictly upward: no cycle, and it wakes a receiver on the parent's HOME sched (its
        // outcome slot / JoinScope stay put).
        //
        // §2c1 — at the TOP LEVEL both are `None`, and that is CORRECT rather than merely convenient:
        // a top-level eager sched IS the outermost scheduler, so there is no parked receiver above it
        // for a wake to reach. (The wake still reaches SIBLING scheds through the run's
        // `sched_registry` — `Vm::wake_on_send` — which is a different, non-hierarchical path.)
        inner.parent_wake = self.mn.clone().or_else(|| self.mn_enlist_sched.clone());
        // gaps.md W7-56 — see `run_mn_nursery_outermost`.
        inner.exec_registry = Arc::clone(&self.exec_registry);
        // gaps.md W7-58 — so an idle worker of this sched can JUDGE the process-wide verdict on
        // behalf of a nursery owner, which never reaches `block_halt_check`.
        inner.quiesce = Arc::clone(&self.quiesce);
        let sched = Arc::new(inner);
        self.register_sched(&sched);
        // Structured concurrency — an eager nursery is a nested scope: its handlers must observe the
        // enclosing scopes' cancel too (`JoinScope::ancestors`).
        sched.lock().scopes[0].ancestors = self.nursery_ancestors();
        sched.open_body(0);
        let mut shell = self.spawn_shell(&sched, &cancel);
        let drain_sched = Arc::clone(&sched);
        let drainer = std::thread::Builder::new()
            .stack_size(VM_STACK_BYTES)
            .name("chezzi-eager".into())
            .spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    shell.mn_worker_loop(&drain_sched, 1, 0)
                }));
            })
            .ok()?; // no drainer ⇒ no worker during the body ⇒ fall back to lazy (see the doc above)
        // §2c1 — an OUTERMOST eager nursery publishes itself so the process-wide verdict counts its
        // undone fibers as uncounted senders. Without it, top-level `main` blocked on `ch.recv()`
        // while a live sibling is about to `send` is `parties.len() >= live` with nothing satisfiable
        // — a false deadlock on a live program. NESTED eager nurseries are not registered: their body
        // runs on a worker shell, which is never a counted party, so the invariant was never broken
        // there. See `quiesce::QuiesceState::eager_bodies`.
        if self.mn.is_none() {
            self.quiesce.register_eager_body(&sched);
        }
        Some(EagerScope {
            sched,
            cancel,
            drainer: Some(drainer),
            scope: 0,
        })
    }

    /// §2c1 — the cancel chain a new nursery scope must observe: this fiber's own chain
    /// ([`Vm::scope_ancestors`]) PLUS every eager nursery scope still open on this thread, outermost
    /// first.
    ///
    /// `scope_ancestors` alone is not enough once the top level is eager: on `main` there is no fiber,
    /// so `cancel`/`cancel_outer` are empty and a nested scope would observe no ancestor at all —
    /// an outer escape could then leave its inner scope's fibers uncancellable, which is the
    /// structured-concurrency invariant.
    pub(super) fn nursery_ancestors(&self) -> Vec<Arc<AtomicBool>> {
        let mut a = self.scope_ancestors();
        if self.deferring == 0 {
            a.extend(
                self.eager_scheds
                    .iter()
                    .flatten()
                    .map(|s| Arc::clone(&s.cancel)),
            );
        }
        a
    }

    /// Per-connection spawn — `JoinNursery` for an eager nursery (the normal fall-through path). Close
    /// the body (no more injections → the sched may terminate once every handler is done), then run
    /// the inline join worker (`wid` 0) to help drain remaining handlers, wait for every slot to fill,
    /// and reduce (Decision-F output flush in spawn order; a handler fault propagates as the
    /// acceptor's body fault, which the outer nursery then sees). Mirrors `run_mn_nursery`'s tail.
    ///
    /// §2c1 — two additions once the OUTERMOST nursery takes this path:
    /// - **Pool helpers.** The body ran on one raw drainer, which is right for a server whose
    ///   handlers park on sockets but would cost a CPU-bound fan-out (`examples/primes_parallel.chz`)
    ///   every core but two. Once the body is CLOSED there is no acceptor left to starve, so farm the
    ///   bounded pool exactly as `run_mn_nursery_outermost` does — same helper count, same
    ///   `SENTINEL_SCOPE`, and the same thread-hold window (join → completion) as before this change.
    ///   Only for an outermost scope: a NESTED eager join already runs on a worker thread whose pool
    ///   siblings are busy, and farming there is what the old `worker_count() >= 2` gate was about.
    /// - **The W7-58 party guard.** The joiner used to be a worker shell by construction, so it was
    ///   never a counted party; a top-level joiner IS one, and without registering, `main` sitting in
    ///   `mn_worker_loop` is invisible to the process-wide verdict (`parties.len() < live` vetoes
    ///   forever). `nursery_party_guard` self-gates on `mn.is_none()`,
    ///   so it stays a no-op on a shell.
    pub(super) fn join_eager_nursery(&mut self, scope: EagerScope) -> Result<(), RuntimeError> {
        let EagerScope {
            sched,
            cancel,
            drainer,
            scope: sid,
        } = scope;
        sched.close_body(sid);
        let mut shell = self.spawn_shell(&sched, &cancel);
        // §2c1 — a NESTED scope shares the owner's sched: run the inline owner SCOPE-SCOPED (it
        // returns the instant ITS scope is done, having drained the GLOBAL queue meanwhile — that
        // drain is what runs a sibling scope's fiber), reduce only its sub-range, and retire it so the
        // enclosing scope is the last scope again. It must NOT touch the sched's drainer or its other
        // scopes' slots — those belong to the owner's join. Mirrors `run_mn_nursery_nested`.
        if drainer.is_none() {
            {
                // §2c1 — the ENCLOSING body is parked here for the duration, so it cannot inject:
                // clear its `body_open` veto or a genuine nested deadlock hangs.
                let _bodies = self.blocked_bodies_guard(true);
                let _party = self.nursery_party_guard(&sched);
                // T1-fix (W8-8, nested arm) — this scope has NO drainer of its own; the OUTER scope's
                // `chezzi-eager` drainer already drains the shared global queue (scope-blind) and
                // cannot self-stop while `main` sits here (`scopes[0].body_open` stays true), so at a
                // budget of one that drainer alone is the whole CPU allowance. Running the inline
                // owner too would be a second runner. Same gate as the outermost arms below.
                if eager_joiner_runs_fibers(worker_count()) {
                    shell.mn_worker_loop(&sched, 0, sid);
                }
                sched.wait_for_scope(sid);
            }
            let slots = sched.take_scope_slots(sid);
            sched.retire_last_scope(sid);
            return self.reduce_task_slots(slots);
        }
        self.farm_outermost_eager_helpers(&sched, &cancel);
        {
            let _party = self.nursery_party_guard(&sched);
            if eager_joiner_runs_fibers(worker_count()) {
                shell.mn_worker_loop(&sched, 0, 0);
            }
            sched.wait_for_completion();
        }
        if let Some(h) = drainer {
            let _ = h.join();
        }
        let slots = sched.take_slots();
        self.reduce_task_slots(slots)
    }

    /// §2c1 — farm the bounded pool onto an OUTERMOST eager sched whose body has just closed, so a
    /// CPU-bound fan-out gets every core instead of the drainer + the inline joiner. `wid`s 2.. —
    /// `0` is the inline joiner and `1` is the raw drainer; `activate_eager_nursery` sized `locals`
    /// for all of them. SENTINEL, as in `run_mn_nursery_outermost`: drain the whole queue until
    /// global terminate.
    ///
    /// Two guards, both measured:
    /// - **nested scope** (`mn.is_some()`) — a nested eager join already runs on a worker thread
    ///   whose pool siblings are busy; farming there is what the old `worker_count() >= 2` gate at
    ///   `EnterNursery` was about.
    /// - **fewer than 2 tasks left** — the inline joiner alone finishes those, so every submission
    ///   would be pure overhead. That matters because a `parallel:` INSIDE A LOOP reaches this once
    ///   per iteration: unguarded, a 20 000-iteration loop submitted ~200 000 pool jobs for nurseries
    ///   holding a single task each.
    fn farm_outermost_eager_helpers(&mut self, sched: &Arc<MnSched>, cancel: &Arc<AtomicBool>) {
        if self.mn.is_some() || sched.outstanding_tasks() < 2 {
            return;
        }
        for wid in eager_helper_wids(worker_count()) {
            let mut shell = self.spawn_shell(sched, cancel);
            let sched = Arc::clone(sched);
            pool::submit(Box::new(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    shell.mn_worker_loop(&sched, wid, SENTINEL_SCOPE)
                }));
            }));
        }
    }

    /// Per-connection spawn — reclaim an eager nursery whose body ESCAPED early (`?`/`return`/`break`/
    /// `continue` or a `recover:` catch jumped past its `JoinNursery`). The injected handlers are live
    /// fibers, so (unlike a lazy nursery's unstarted `PendingCall`s) they must be cancelled, not just
    /// dropped: trip the inner cancel, drain channel- and socket-parked handlers (D6b
    /// `cancel_drain` + `drain_sched`), run the inline worker to settle them, then flush their output
    /// (Decision F). The body's own escape error is what propagates, so a handler fault here is
    /// swallowed (only its buffered output + any `os.exit` are honored via `reduce_task_slots`).
    pub(super) fn abort_eager_nursery(&mut self, scope: EagerScope) {
        let EagerScope {
            sched,
            cancel,
            drainer,
            scope: sid,
        } = scope;
        // N4 — trip the cancel UNDER the core lock (`trip_scope_cancel`, scope 0 = the eager scope, whose
        // `JoinScope::cancel` IS this `cancel` Arc) and BEFORE `close_body` clears the `any_body_open`
        // veto: gapless veto handoff, and the store is published to any worker that takes the core lock
        // to evaluate the deadlock predicate (a bare `Relaxed` store outside the lock has no
        // synchronizes-with edge, so a worker could read a stale `false` and reap this scope's parked
        // handlers as `Deadlocked` — dropping them without `unwind_deferred`, skipping their `defer`s).
        sched.trip_scope_cancel(sid);
        sched.close_body(sid);
        sched.cancel_drain(sid);
        // §2c1 — scope-selective: a nested eager nursery shares the enclosing scope's sched, so
        // draining by sched alone would unpark the OUTER scope's socket-parked fibers too.
        if drainer.is_some() {
            poller::drain_sched(&sched);
        } else {
            poller::drain_scope(&sched, sid);
        }
        let mut shell = self.spawn_shell(&sched, &cancel);
        // §2c1 — a NESTED scope settles and reduces only ITSELF, then retires (see
        // `join_eager_nursery`'s nested arm); the owner's drainer and sibling scopes are untouched.
        if drainer.is_none() {
            {
                let _bodies = self.blocked_bodies_guard(true); // see `join_eager_nursery`'s nested arm
                let _party = self.nursery_party_guard(&sched);
                // T1-fix (W8-8, nested arm) — same gate as `join_eager_nursery`'s nested arm above.
                if eager_joiner_runs_fibers(worker_count()) {
                    shell.mn_worker_loop(&sched, 0, sid);
                }
                sched.wait_for_scope(sid);
            }
            let slots = sched.take_scope_slots(sid);
            sched.retire_last_scope(sid);
            let _ = self.reduce_task_slots(slots);
            return;
        }
        {
            // §2c1 — same reason as `join_eager_nursery`: a top-level escape settles its handlers on
            // `main`, which must be a counted party for that span or the process-wide verdict vetoes
            // forever. No-op on a worker shell.
            let _party = self.nursery_party_guard(&sched);
            if eager_joiner_runs_fibers(worker_count()) {
                shell.mn_worker_loop(&sched, 0, 0);
            }
            sched.wait_for_completion();
        }
        if let Some(h) = drainer {
            let _ = h.join();
        }
        let slots = sched.take_slots();
        // The body's escape error is what propagates; a handler fault here is swallowed. But
        // `reduce_task_slots` still sets `self.pending_exit` for a handler `os.exit` (decision C —
        // a hard halt wins), which the catch site honors after the drain — so it is NOT lost.
        let _ = self.reduce_task_slots(slots);
    }

    /// D2b — build a thin host **shell** `Vm` for the M:N engine: a worker `Vm` with the nursery
    /// scheduler and the cancel token wired in. It runs no code itself — fibers swap their own heap +
    /// module roots + snapshot into it ([`Vm::swap_ctx`]); the shell only provides the dispatch engine
    /// and the `mn`/`cancel` flags the `recv`/`send`/back-edge paths read.
    ///
    /// W6-2 — a shell carries NO `module_snapshot` of its own (it used to be handed one). Snapshots are
    /// per-nursery now and a shell drains the GLOBAL run queue across scopes, so a shell-level snapshot
    /// would be the WRONG one for a fiber from another scope; each fiber brings its own.
    pub(super) fn spawn_shell(&self, sched: &Arc<MnSched>, cancel: &Arc<AtomicBool>) -> Vm {
        let mut shell = self.spawn_worker();
        shell.mn = Some(Arc::clone(sched));
        // Cancel inheritance: if this shell serves a NESTED scope (its `cancel` is not the flag this VM
        // runs under), the enclosing scopes' flags go on the chain. `run_one_fiber` re-points both per
        // fiber swap-in; this only makes the shell coherent before the first swap-in.
        shell.cancel_outer = match &self.cancel {
            Some(mine) if !Arc::ptr_eq(mine, cancel) => self.scope_ancestors(),
            _ => self.cancel_outer.clone(),
        };
        shell.cancel = Some(Arc::clone(cancel));
        shell
    }

    /// D5 owe #3 (Path C) — a blocking `recv` reached inside a native callback (the host-stack loop
    /// frame of `xs.map(f)` / a sort comparator / `Shared.update(f)`, so `native_reentry > 0`) cannot
    /// snapshot-park. Instead of faulting `deadlock`, this worker thread **demotes**: it blocks in place
    /// on the channel's own condvar and resumes in place once a sibling `send`s — Go's `handoffp`. A
    /// fresh replacement worker is spun up ONCE (covering this thread's `wid`) so the live runnable-worker
    /// count stays at N; after this fiber settles, [`Vm::mn_worker_loop`] sees `self.demoted` and exits,
    /// so steady-state live workers = N + (fibers currently blocked in a callback) — Go's exact cost.
    ///
    /// Returns [`RecvStep::Got`] (the native callback continues on this thread with the value),
    /// [`RecvStep::ClosedEmpty`] (the channel was `close()`d while demoted — the caller faults
    /// "receive on a closed channel"), or a `cancelled` / `deadlock` fault (which unwinds the callback
    /// → the fiber faults). Never [`RecvStep::Parked`] — a demoted recv blocks in place, it never
    /// snapshot-parks. Only ever called on the M:N engine inside a native callback (the recv site gates
    /// on `mn.is_some() && native_reentry > 0`).
    pub(super) fn demote_recv_block(
        &mut self,
        h: GcRef,
        span: Span,
    ) -> Result<RecvStep, RuntimeError> {
        let sched = Arc::clone(
            self.mn
                .as_ref()
                .expect("demote_recv_block called with no active M:N scheduler (self.mn is None)"),
        );
        let core = self.channel_core(h);
        let ptr = self.channel_core_ptr(h);
        // 1. Account running → blocked_native AND register the channel (#1 fix), under core lock A, then
        //    notify so an idle puller sitting in an untimed `take_runnable` `cv.wait` re-evaluates the
        //    deadlock predicate now that this fiber left `running` (without this notify a genuine
        //    all-blocked quiesce would never be detected — a hang). The registration lets
        //    `is_deadlocked` peek this fiber's queue so a value a sibling races in isn't misread as a
        //    deadlock (the #1 false-positive against an innocent parked sibling).
        let tok = {
            let mut c = sched.lock();
            c.running -= 1;
            sched.blocked_native.fetch_add(1, Ordering::Relaxed);
            c.register_demoted(ptr, &core);
            // N4 — a cancel can still WAKE this fiber (the `cancel_requested()` check below ranks above
            // `terminate` / the self-detect), and that is progress the deadlock predicate's counters
            // cannot see: it is `blocked_native`, not `parked`. Watch the flags it would honour so
            // `is_deadlocked` vetoes while one of them is tripped. Empty (⇒ no veto) when a cancel could
            // NOT wake it — already unwinding, or blocked inside its own `defer` — which is exactly the
            // fiber that IS a genuine deadlock, and must be reported rather than hang.
            let tok = c.watch_demoted_cancel(self.demote_cancel_flags());
            drop(c);
            sched.cv.notify_all();
            tok
        };
        // 2. Spin up a replacement worker ONCE per demoted thread (covers this `wid` while we block).
        //    Subsequent re-entries of this loop on the SAME thread (a callback that recvs repeatedly)
        //    reuse the already-spawned coverage — one spawn + one eventual exit per demoted thread.
        //    If the OS refuses the thread (a real mode for this raw-thread-per-demotion design under
        //    `RLIMIT_NPROC`/ENOMEM with many fibers blocked-in-callback), DON'T panic mid-accounting:
        //    un-roll step 1 (account + registry) and fault this fiber cleanly so the join still completes.
        if !self.demoted {
            if !self.spawn_replacement_worker(&sched, self.wid) {
                let mut c = sched.lock();
                c.running += 1;
                sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
                c.unregister_demoted(ptr);
                c.unwatch_demoted_cancel(tok);
                drop(c);
                return Err(self.err(
                    "recv inside a native callback could not demote the worker (OS thread limit \
                     reached) — reduce concurrent in-callback blocking or raise the thread limit"
                        .to_string(),
                    span,
                ));
            }
            self.demoted = true;
        }
        // 3. Block in place. The pop + un-account (blocked_native--/running++) + un-register are ATOMIC
        //    under core lock A (A-then-q — the order `send_wake` uses → no ABBA), so the deadlock checker
        //    never observes an emptied-but-still-counted/registered demoted fiber (the #1 window). The
        //    QUEUE is checked FIRST so a genuinely-sent value always wins over a spuriously-set
        //    `terminate`. Each exit path un-accounts under A and returns directly (no separate "step 4");
        //    lost condvar wakeups are bounded by `DEMOTE_POLL_BACKOFF` (≤ latency, never a hang).
        loop {
            // --- settle under core lock A: pop wins over cancel / terminate / deadlock ---
            {
                let mut c = sched.lock();
                let mut qg = core.q.lock().unwrap_or_else(|e| e.into_inner());
                let popped = qg.pop();
                if let Some(w) = popped {
                    c.running += 1;
                    sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
                    c.unregister_demoted(ptr);
                    c.unwatch_demoted_cancel(tok);
                    drop(qg);
                    drop(c);
                    return Ok(RecvStep::Got(w));
                }
                // A tripped latch (`trip()`) delivers `true` like a passed timer — ranks below a real
                // queued value, above closed/terminate/deadlock (a `done().recv()` on a cancelled token
                // reached inside a native callback must not false-deadlock).
                if core.done_latch.load(Ordering::Relaxed) {
                    c.running += 1;
                    sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
                    c.unregister_demoted(ptr);
                    c.unwatch_demoted_cancel(tok);
                    drop(qg);
                    drop(c);
                    return Ok(RecvStep::Got(WireValue::Bool(true)));
                }
                // Closed-and-drained: the queue is empty here (pop-first) and the channel is closed, so
                // no value will ever arrive — signal `ClosedEmpty` (the caller faults "receive on a
                // closed channel"). Read while still holding the queue lock so it is atomic with the
                // pop above. Ranks below a delivered value, above terminate/deadlock.
                let closed = qg.closed;
                drop(qg);
                // Cancel (a sibling faulted): set `cancelled` BEFORE returning the Err so the outcome is
                // SWALLOWED (a cancelled task is dropped, not reported) instead of surfacing as a Fault —
                // mirrors the snapshot-park recv's cancel branch.
                //
                // MUST go through `cancel_requested()`, never a raw `self.cancel` load: a `defer` body
                // runs under `guarded` (native_reentry > 0), so a blocking op INSIDE cleanup (a
                // `sock.close()`, a `ch.send()`, a `sleep`) lands here. A raw read fires on the already-
                // tripped flag and truncates the defer mid-body. The predicate's `deferring == 0` term
                // is what keeps cleanup atomic, and it also folds in `cancel_outer` (an ENCLOSING
                // scope's cancel), which a raw read misses.
                if self.cancel_requested() {
                    self.cancelled = true;
                    c.running += 1;
                    sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
                    c.unregister_demoted(ptr);
                    c.unwatch_demoted_cancel(tok);
                    drop(c);
                    return Err(self.err("cancelled".to_string(), span));
                }
                // W7-47 — a run-wide `os.exit` from another party, below cancel like every other site.
                // Un-account first (same bookkeeping as the cancel arm above), or the counters leak.
                if let Some(e) = self.run_exit_err(span) {
                    c.running += 1;
                    sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
                    c.unregister_demoted(ptr);
                    c.unwatch_demoted_cancel(tok);
                    drop(c);
                    return Err(e);
                }
                if closed {
                    c.running += 1;
                    sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
                    c.unregister_demoted(ptr);
                    c.unwatch_demoted_cancel(tok);
                    drop(c);
                    return Ok(RecvStep::ClosedEmpty);
                }
                // Terminate without a delivered value (genuine deadlock / nursery torn down): fault in
                // place. Path C self-sufficient deadlock detection: evaluate the predicate HERE rather
                // than depending on a separate idle puller being alive to fire it (the replacement could
                // be the last worker and itself demoted → otherwise a hang). The queue-first pop above
                // means OUR channel is empty here, and `is_deadlocked` now also peeks OTHER demoted
                // channels (#1), so firing can never strand a value destined for any demoted fiber.
                if c.terminate {
                    c.running += 1;
                    sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
                    c.unregister_demoted(ptr);
                    c.unwatch_demoted_cancel(tok);
                    drop(c);
                    return Err(sched.deadlock_err.clone());
                }
                if sched.is_deadlocked(&c) {
                    c.flag_deadlock(&sched.deadlock_err);
                    c.running += 1;
                    sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
                    c.unregister_demoted(ptr);
                    c.unwatch_demoted_cancel(tok);
                    drop(c);
                    sched.cv.notify_all();
                    return Err(sched.deadlock_err.clone());
                }
            }
            // --- wait on the channel's OWN condvar (q-only; core lock A released) ---
            let q = core.q.lock().unwrap_or_else(|e| e.into_inner());
            if q.is_empty() {
                let _ = core.cv.wait_timeout(q, DEMOTE_POLL_BACKOFF);
            }
        }
    }

    /// §6d M:N — a blocking multi-channel `wait` reached INSIDE a native callback (`native_reentry > 0`).
    /// A host-stack loop frame sits between the worker loop and the `wait`, so the fiber CANNOT
    /// snapshot-park (`park_wait`); it demotes — blocks this worker in place, polling all N arm queues
    /// in **source order** on a bounded `DEMOTE_POLL_BACKOFF` backoff. The N-arm analogue of
    /// [`Vm::demote_recv_block`]: account `running → blocked_native`, register EVERY arm channel in
    /// `demoted_chans` (so `is_deadlocked` peeks them all and a value racing onto any arm vetoes a false
    /// fire), spin a replacement worker once, then loop. Because there are N channel condvars (no single
    /// one to block on), the wait is a bounded poll rather than a targeted condvar wait — lower
    /// throughput but sound (the documented v1 limitation, same shape as the timer-in-callback note).
    /// Returns `(arm_index, value)` for the first source-order arm to deliver. A per-arm `close`+empty
    /// is SKIPPED; once EVERY arm is closed+empty it returns "all channels closed". Cancel/terminate/
    /// self-detected-deadlock fault in place. Never parks. Only called on the M:N engine inside a
    /// callback (gated `mn.is_some() && native_reentry > 0`).
    pub(super) fn demote_wait_block(
        &mut self,
        arms: Vec<(usize, Arc<ChannelCore>)>,
        timer: Option<(usize, std::time::Instant)>,
        span: Span,
    ) -> Result<(usize, WireValue), RuntimeError> {
        let sched = Arc::clone(
            self.mn
                .as_ref()
                .expect("demote_wait_block called with no active M:N scheduler (self.mn is None)"),
        );
        // 1. Account running → blocked_native AND register EVERY arm channel, under core lock A, then
        //    notify so an idle puller re-evaluates the deadlock predicate now this fiber left `running`.
        // (`watch_demoted_cancel`: see `demote_recv_block` — a cancel can still wake this fiber, and
        // that is progress `is_deadlocked`'s counters cannot see.)
        let tok = {
            let mut c = sched.lock();
            c.running -= 1;
            sched.blocked_native.fetch_add(1, Ordering::Relaxed);
            for (ptr, core) in &arms {
                c.register_demoted(*ptr, core);
            }
            let tok = c.watch_demoted_cancel(self.demote_cancel_flags());
            drop(c);
            sched.cv.notify_all();
            tok
        };
        // Un-account helper: reverse step 1 (called on every exit path), caller holds core lock A.
        let un_account = |c: &mut SchedCore| {
            c.running += 1;
            sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
            for (ptr, _) in &arms {
                c.unregister_demoted(*ptr);
            }
            c.unwatch_demoted_cancel(tok);
        };
        // 2. Spin a replacement worker ONCE per demoted thread (covers this `wid` while we block). If the
        //    OS refuses the thread, un-roll step 1 and fault cleanly so the join still completes.
        if !self.demoted {
            if !self.spawn_replacement_worker(&sched, self.wid) {
                let mut c = sched.lock();
                un_account(&mut c);
                drop(c);
                return Err(self.err(
                    "wait inside a native callback could not demote the worker (OS thread limit \
                     reached) — reduce concurrent in-callback blocking or raise the thread limit"
                        .to_string(),
                    span,
                ));
            }
            self.demoted = true;
        }
        // 3. Block in place. Each poll: under core lock A, scan all N arms in source order — the first
        //    with a queued value wins (un-account + return). Then rank cancel > all-closed > terminate
        //    > self-detected-deadlock, exactly like `demote_recv_block`, but generalized over N arms.
        loop {
            {
                let mut c = sched.lock();
                // Source-order poll: pop the first arm with a queued value (atomic with the A hold).
                let mut all_closed = true;
                for (idx, (_, core)) in arms.iter().enumerate() {
                    let mut qg = core.q.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(w) = qg.pop() {
                        drop(qg);
                        un_account(&mut c);
                        drop(c);
                        return Ok((idx, w));
                    }
                    // A tripped latch (`trip()`) makes this arm ready with `true` (after the value scan).
                    if core.done_latch.load(Ordering::Relaxed) {
                        drop(qg);
                        un_account(&mut c);
                        drop(c);
                        return Ok((idx, WireValue::Bool(true)));
                    }
                    if !qg.closed {
                        all_closed = false;
                    }
                }
                // Cancel (a sibling faulted): swallow the outcome (mirror the snapshot-park cancel arm).
                if self.cancel_requested() {
                    self.cancelled = true;
                    un_account(&mut c);
                    drop(c);
                    return Err(self.err("cancelled".to_string(), span));
                }
                // W7-47 — a run-wide `os.exit` from another party, below cancel like every other site.
                if let Some(e) = self.run_exit_err(span) {
                    un_account(&mut c);
                    drop(c);
                    return Err(e);
                }
                // WAIT-1 (demote path) — a live timer arm fires only AFTER the source-order channel scan
                // failed (so a real `send` to any arm beats the timer on a tie). Once `now >= deadline`,
                // take the timer arm with `true`. A still-pending timer vetoes the deadlock fault below
                // (a value WILL arrive at the deadline — like an `inflight` job on the snapshot path).
                if let Some((idx, deadline)) = timer
                    && std::time::Instant::now() >= deadline
                {
                    un_account(&mut c);
                    drop(c);
                    return Ok((idx, WireValue::Bool(true)));
                }
                // Every arm closed+empty: no value can ever arrive — the all-closed `wait` fault. (A live
                // timer arm keeps `all_closed` false, so this fires only with no timer pending.)
                if all_closed {
                    un_account(&mut c);
                    drop(c);
                    return Err(self.err("wait: all channels closed".to_string(), span));
                }
                if c.terminate {
                    un_account(&mut c);
                    drop(c);
                    return Err(sched.deadlock_err.clone());
                }
                // A pending timer guarantees future progress (its deadline send), so it vetoes the
                // self-detected deadlock just like an `inflight` job does on the snapshot-park path.
                if timer.is_none() && sched.is_deadlocked(&c) {
                    c.flag_deadlock(&sched.deadlock_err);
                    un_account(&mut c);
                    drop(c);
                    sched.cv.notify_all();
                    return Err(sched.deadlock_err.clone());
                }
            }
            // No single condvar to wait on (N arms, N condvars) → bounded backoff poll. Sleep on the
            // FIRST arm's condvar with a timeout so a `send`/`close` to arm 0 wakes promptly, and any
            // other arm is observed within `DEMOTE_POLL_BACKOFF` (the documented lower-throughput path).
            let first = &arms[0].1;
            let q = first.q.lock().unwrap_or_else(|e| e.into_inner());
            // Clamp the backoff to the timer deadline so the loop re-polls and fires the timer arm
            // by its deadline (saturating, so a deadline that already passed yields ~zero wait).
            let backoff = match timer {
                Some((_, d)) => {
                    DEMOTE_POLL_BACKOFF.min(d.saturating_duration_since(std::time::Instant::now()))
                }
                None => DEMOTE_POLL_BACKOFF,
            };
            // W7-13r(b) — arm 0 is ready on a queued value OR a `trip()` latch, and the latch belongs
            // here for the same reason it belongs in the eager `wait:` predicate: the poll above
            // SETTLES on `done_latch`, so sleeping through one costs a full tick on a channel that is
            // permanently ready. The old form tested `q.is_empty()` only, which is why `trip()` moving
            // under `core.q` did not by itself make every waiter prompt.
            //
            // `closed` is deliberately NOT a ready condition: the poll SKIPS a closed+empty recv arm,
            // so reporting ready would return instantly, re-poll, skip, and spin — the parity-perf-0
            // live-lock, which the eager `wait:` predicate reintroduced once already. (Send arms never
            // reach this function: an in-callback `wait:` with a send arm faults before the demote.)
            let _ = first.cv.wait_timeout_while(q, backoff, |g| {
                !(!g.is_empty() || first.done_latch.load(Ordering::Relaxed))
            });
        }
    }

    /// D5 owe #3 Path C (#3) — a `sleep_ms(ms>0)` reached INSIDE a native callback (`native_reentry > 0`)
    /// on the M:N engine. The offload gate (`running → timer thread`) requires `native_reentry == 0`
    /// (the callback's `for`-loop state lives on the un-snapshottable Rust host stack), so without this
    /// the sleep runs INLINE and pins the worker for `ms`. Instead DEMOTE like a recv-in-callback: spin a
    /// replacement worker + sleep in place + resume, freeing the worker for `ms` (Go's `handoffp`).
    /// Accounted as `inflight` (NOT `blocked_native`): a sleeper returns unconditionally, so it must VETO
    /// the deadlock predicate (like an offloaded blocking native — `is_deadlocked` already treats
    /// `inflight>0` as "external progress guaranteed"). A `blocked_native` fiber is the opposite (it
    /// returns only via a sibling `send`). Returns `Ok(Nil)` (`sleep_ms` yields nothing). Residual: the
    /// `thread::sleep` is uninterruptible, so a cancel during the sleep is observed only after it returns
    /// (no worse than the inline pin it replaces — the worker is now freed).
    pub(super) fn demote_block_sleep(
        &mut self,
        ms: u64,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let sched =
            Arc::clone(self.mn.as_ref().expect(
                "demote_block_sleep called with no active M:N scheduler (self.mn is None)",
            ));
        // 1. Account running → inflight under the core lock, then notify idle pullers (a worker sitting in
        //    an untimed `cv.wait` re-evaluates now that this fiber left `running`).
        {
            let mut c = sched.lock();
            c.running -= 1;
            sched.inflight.fetch_add(1, Ordering::Relaxed);
            drop(c);
            sched.cv.notify_all();
        }
        // 2. Spin up a replacement worker ONCE per demoted thread (reuse the `self.demoted` coverage the
        //    recv demote sets — one spawn + one eventual exit per demoted thread regardless of how many
        //    times it blocks). Un-roll the accounting + fault cleanly if the OS refuses the thread.
        if !self.demoted {
            if !self.spawn_replacement_worker(&sched, self.wid) {
                let mut c = sched.lock();
                c.running += 1;
                sched.inflight.fetch_sub(1, Ordering::Relaxed);
                drop(c);
                return Err(self.err(
                    "sleep_ms inside a native callback could not demote the worker (OS thread limit \
                     reached) — reduce concurrent in-callback blocking or raise the thread limit"
                        .to_string(),
                    span,
                ));
            }
            self.demoted = true;
        }
        // 3. Sleep in place (the worker is covered by the replacement), CHUNKED at `DEMOTE_POLL_BACKOFF`
        //    exactly like `Vm::block_until_deadline` — W7-57. One uninterruptible `thread::sleep(ms)`
        //    made the cancel / `os.exit` rungs below reachable only AFTER the full sleep (measured: an
        //    `xs.map(fn(x): sleep_ms(3000))` in a nursery task survived a sibling job's `os.exit(3)` for
        //    3012 ms). The loop decides only WHEN to stop sleeping; classification stays entirely with
        //    the two arms below, in their existing order, so no state is touched here.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            std::thread::sleep(DEMOTE_POLL_BACKOFF.min(deadline - now));
            // `deferring == 0` mirrors the suppression `run_exit_err` (and `cancel_requested`) apply
            // below: inside a `defer` neither arm will fire, so cutting the sleep short here would
            // silently SHORTEN a deferred `sleep_ms` instead of halting anything.
            if self.cancel_requested() || (self.deferring == 0 && self.quiesce.pending().is_some())
            {
                break;
            }
        }
        // 4. Un-account inflight → running (the `+1` is essential — the fiber's next dispatch does
        //    `running -= 1`, which would underflow without this restore).
        {
            let mut c = sched.lock();
            c.running += 1;
            sched.inflight.fetch_sub(1, Ordering::Relaxed);
        }
        // Cancel observed during/after the sleep (a sibling faulted): set `cancelled` and fault so the
        // outcome is SWALLOWED (a cancelled task is dropped, not reported), mirroring `demote_recv_block`
        // + the snapshot-park recv. Without this, a cancelled task would sleep through every remaining
        // callback element and then fault NORMALLY at a later back-edge — wrong classification (a
        // cancelled-task Fault masking the real sibling error) and wasted in-callback sleeps. Faulting
        // here aborts the native callback loop immediately, so no further elements sleep.
        if self.cancel_requested() {
            self.cancelled = true;
            return Err(self.err("cancelled".to_string(), span));
        }
        // W7-47 — a run-wide `os.exit` observed during/after the sleep, below cancel like every other
        // site. Since W7-57 chunked step 3 this aborts the sleep ITSELF within one
        // `DEMOTE_POLL_BACKOFF`, not merely the rest of the callback loop (measured 3012 ms → 63 ms).
        if let Some(e) = self.run_exit_err(span) {
            return Err(e);
        }
        Ok(Value::nil())
    }

    /// **Block a would-block socket op IN PLACE when the fiber cannot park.** [`Vm::park_on_fd`] parks on
    /// the netpoller only when this Vm is an M:N worker shell AND `native_reentry == 0`. Callers do NOT
    /// route every other context here — the precondition is exactly [`Vm::may_block_socket_in_place`],
    /// i.e. the two contexts whose thread runs nothing else:
    ///
    /// - **In a native callback on an M:N worker** (`native_reentry > 0` — the callback's `for`-loop state
    ///   lives on the un-snapshottable Rust host stack). This is the original D5 owe #3 Path C case:
    ///   DEMOTE like [`Vm::demote_block_sleep`] — spin a replacement worker once so the pool keeps its
    ///   width, then backoff-poll in place.
    /// - **Top-level `main` on the DEFAULT engine** (`parallel`, `mn == None`, no `eager_core`, no
    ///   scheduler, `native_reentry == 0`). There is no pool to demote FROM, so the scheduler bookkeeping
    ///   is skipped entirely — hence the `Option` sched — and only the wait loop runs. Until 2026-08-10
    ///   the four callers returned `Err("… requires the --parallel engine")` here too, on the DEFAULT
    ///   engine, because `mn.is_some()` means "worker shell", not "parallel is on" (`Vm::parallel`) —
    ///   which made the hello-world TCP server unwritable. `connect` had had a fallback all along — its
    ///   own private sleep-spin, deleted by `W7-59`, which now routes it here like the other four; they
    ///   were simply left behind (the repo's recurring fixed-some-arms-of-an-N-way-set finding, W7-22).
    ///
    /// **Not, deliberately: an eager `Executor` job.** It does not own its thread — it runs on the
    /// bounded process-wide [`crate::vm::pool`] — so blocking in place there starves the peer that would
    /// make the fd ready. Measured as a permanent hang when a first attempt at this widening let it in;
    /// it keeps the loud `Err`. (The since-removed cooperative engine was excluded for the same reason:
    /// it ran every fiber on one thread.)
    ///
    /// **`connect` is deliberately admitted even though it is a wait a non-thread-owning party can
    /// reach** (`W7-59`): the starvation argument above is about waiting on a CHEZZI peer fiber,
    /// which only `accept`/`read`/`write` do. A `connect` handshake is completed by the KERNEL, so no
    /// chezzi party is starved by waiting for it — and both ancestors block (measured: CPython
    /// `socket.connect` 0.1 ms, Go `net.Dial` 314 µs, each from the sole/main thread). `connect`
    /// therefore gates on `eager_core.is_some()` alone, not on [`Vm::may_block_socket_in_place`].
    ///
    /// **Consequence, deliberate: an fd that never becomes ready is now a HANG, not an immediate `Err`.**
    /// That is Go-identical (`ln.Accept()` on the main goroutine with nobody dialing blocks forever) and
    /// it is the ancestor's contract, not an oversight. Three escapes are checked at the top of every
    /// loop iteration — the op's own `timeout_ms` (`deadline`, below), the run's `--timeout` via
    /// [`Vm::deadline_halt`], and cancellation — but **which of them EXIST depends on the context**, and
    /// for top-level `main` under `chezzi run` two of the three do not:
    ///
    /// - `--timeout` is a **`chezzi test` flag only**; `chezzi run --timeout=500 f.chz` is rejected by
    ///   the CLI with `chezzi run: unknown flag '--timeout=500'` (`src/main.rs:243`), so `self.deadline`
    ///   is `None` and [`Vm::deadline_halt`] is a no-op on this path.
    /// - `main` has no scope — no `cancel`, no `cancel_outer` — so `cancel_requested()` reads an EMPTY
    ///   flag set and nothing can ever trip it.
    ///
    /// So an `accept`/`read`/`write` reached on `main` with **no explicit `timeout_ms`** has no escape at
    /// all short of SIGINT. Pass a `timeout_ms` if you need one. (The in-callback M:N demote — the other
    /// context routed here — does have all three: it runs under `chezzi test` where `--timeout` is legal,
    /// and its fiber has a scope cancel.)
    ///
    /// **Second-order, also deliberate: a socket-blocked `main` is NOT a quiescence party.** It cannot
    /// register as one — there is no scheduler here to register with — which matches the precedent on
    /// both sides: `time.sleep_ms` deliberately stays out of the predicate as "a false-deadlock
    /// generator" (`netio.rs`), and the M:N socket demote is accounted `inflight` specifically to VETO
    /// the predicate (Go-identical). It therefore never FABRICATES a verdict — the safe direction
    /// (`parked-is-not-stuck`: a wrong verdict is worse than a late one).
    ///
    /// It does, however, **SUPPRESS** the process-wide verdict for as long as the op is outstanding —
    /// which for an fd that never becomes ready is FOREVER, not merely "delayed" (an earlier version of
    /// this note claimed the latter; measured, it is wrong). Repro: `main` blocks in `accept()` while an
    /// eager `Executor` job blocks on a `Channel` nothing can ever send. Chezzi prints `listening` and
    /// hangs (rc=124 under `timeout 12`).
    ///
    /// **That is the ancestor's answer — do not "fix" it in code.** Go behaves the same way: an open
    /// socket is a runnable party, so `all goroutines are asleep` does not fire (measured — `main` in
    /// `ln.Accept()` plus a goroutine on an empty `chan int`: prints `listening`, hangs, rc=124). The
    /// since-removed cooperative engine printed `Err('accept would block: …')` and then the full
    /// `recv on an empty channel: deadlock`, exit 1 — but only because it REFUSED the socket op
    /// outright, which is what left its channel party alone in the predicate. It was the engine that
    /// did NOT match the ancestor.
    ///
    /// On an M:N worker it is accounted `inflight` (NOT `blocked_native`): a socket op is woken by external OS readiness, so
    /// it must VETO the deadlock predicate — exactly the netpoller-park accounting (a lone in-callback
    /// `accept` with no client correctly never self-terminates, Go-identical), hence **no** `is_deadlocked`
    /// self-fire here. The flip side: this op exits the wait ONLY via fd readiness, `cancel`, or another
    /// worker setting `terminate` — so a nursery where *every* remaining fiber is an in-callback socket
    /// demote on an fd that never becomes ready, with no faulting sibling, hangs silently (no `deadlock`
    /// fault). That is the same Go-identical "all goroutines waiting on a never-ready socket" case the
    /// netpoller park already has; the rule is unchanged (don't await an fd that nothing will signal).
    ///
    /// Between attempts the worker kernel-BLOCKS on the fd via [`wait_fd_ready`] (woken immediately on
    /// readiness, no busy-poll, no wasted syscalls — close to the epoll path it can't use here) with a
    /// `DEMOTE_POLL_BACKOFF` timeout so a sibling-fault `cancel` is still observed within that window.
    /// `cancel`/`terminate` are re-checked at the TOP of every iteration (before re-attempting), so a
    /// cancelled task stops issuing socket work promptly and its outcome is SWALLOWED (mirrors the
    /// recv/sleep demote). `attempt` re-runs one non-blocking op (it owns a cloned `Arc<…Core>`) and
    /// returns `SockPoll::Ready` with the op's `Result`-shaped `Value`, or `SockPoll::WouldBlock`.
    ///
    /// `deadline` is the caller's `timeout_ms` (`None` = untimed): it bounds the WHOLE op here exactly
    /// as it does on the netpoller-park path, and expiring yields `Err("timeout")` — or, for a str `read`
    /// whose demote closure took a partial codepoint off the wire (`poll_partial` latched, N3a), the
    /// `Err("incomplete utf-8: …")` classification instead. It is the ONLY escape from the "never-ready
    /// fd" case above — an in-callback socket op is `inflight`, so it can never self-fire the deadlock
    /// predicate.
    pub(super) fn demote_block_socket(
        &mut self,
        fd: std::os::fd::RawFd,
        interest: poller::Interest,
        deadline: Option<std::time::Instant>,
        span: Span,
        mut attempt: impl FnMut(&mut Vm) -> SockPoll,
    ) -> Result<Value, RuntimeError> {
        // `None` when there is no worker pool at all (top-level `main`, an eager `Executor` job): the
        // calling thread is its own, so there is nothing to demote FROM and the scheduler bookkeeping
        // below is skipped. The wait loop itself does not care either way.
        let sched = self.mn.as_ref().map(Arc::clone);
        if sched.is_some() {
            self.demote_socket_enter(span)?;
        }
        let out = loop {
            // W7-18 — the run's `--timeout` deadline, ABOVE the cancel check (it outranks a cancel,
            // W7-17's ordering). An in-callback socket op is accounted `inflight`, so it VETOES the
            // deadlock predicate: without this an untimed `accept` here hangs exactly like the
            // netpoller-park shape. `break`, NOT `?` — this loop is bracketed by
            // `demote_socket_enter`/`demote_socket_exit`, and returning past the exit would leak
            // `running -= 1` / `inflight += 1` for the rest of the process (which is why every other
            // exit below is a `break` too).
            if let Err(e) = self.deadline_halt(span) {
                break Err(e);
            }
            // Observe teardown/cancel BEFORE doing more work each iteration. Cancel (a sibling faulted):
            // set `cancelled` so the outcome is SWALLOWED (a cancelled task is dropped, not reported).
            if self.cancel_requested() {
                self.cancelled = true;
                break Err(self.err("cancelled".to_string(), span));
            }
            // W7-47 — a run-wide `os.exit` from another party (an eager `Executor` job). This is the one
            // blocking wait not routed through `block_halt_check`, so it needs the rung explicitly, in
            // the same relative order (below cancel). `break`, NOT `?`, for the bracketing reason above.
            if let Some(e) = self.run_exit_err(span) {
                break Err(e);
            }
            // Nursery torn down (deadlock elsewhere / `os.exit`): fault in place. An `inflight` socket op
            // never *self*-fires the predicate (it vetoes it), so a genuine quiesce is surfaced by another
            // worker setting `terminate`, observed here within the backoff.
            if let Some(s) = &sched
                && s.lock().terminate
            {
                break Err(s.deadlock_err.clone());
            }
            match attempt(self) {
                SockPoll::Ready(r) => break r,
                SockPoll::WouldBlock => {
                    // The caller's `timeout_ms` bounds the WHOLE op here too, exactly as it does on the
                    // netpoller-park path — the demote loop used to wait on fd readiness with no
                    // deadline at all, and an in-callback op is accounted `inflight` (it VETOES the
                    // deadlock predicate), so a peer that never sends hung the program with no fault and
                    // no `Err("timeout")`. Cap the kernel wait by the remaining budget so the deadline is
                    // observed within it, not one full backoff late.
                    let left = match deadline {
                        None => DEMOTE_POLL_BACKOFF,
                        Some(dl) => {
                            let left = dl.saturating_duration_since(std::time::Instant::now());
                            if left.is_zero() {
                                // N3(a) — a str `read` that took a partial codepoint off the wire (its
                                // demote closure latched `poll_partial`) reports the incomplete-utf-8
                                // classification, not `timeout` (nothing arrived). `read_bytes`/`write`/
                                // `accept` never latch it, so their demote timeout stays `timeout`.
                                break Ok(match self.poll_partial {
                                    Some(owed) => self.sock_incomplete_err(owed),
                                    None => self.sock_err("timeout"),
                                });
                            }
                            left.min(DEMOTE_POLL_BACKOFF)
                        }
                    };
                    wait_fd_ready(fd, interest, left);
                }
            }
        };
        if sched.is_some() {
            self.demote_socket_exit();
        }
        out
    }

    /// D5 owe #3 Path C (#3 socket half) — enter the in-callback socket demote: account `running → inflight`
    /// under core lock A + notify idle pullers (a worker in an untimed `cv.wait` re-evaluates now that this
    /// fiber left `running`), then spin a replacement worker ONCE (reuse the `self.demoted` coverage the
    /// recv/sleep demote also uses — one spawn + one eventual exit per demoted thread). On OS-refuse, un-roll
    /// the accounting and fault cleanly so the join still completes. Mirrors [`Vm::demote_block_sleep`] 1–2.
    pub(super) fn demote_socket_enter(&mut self, span: Span) -> Result<(), RuntimeError> {
        let sched =
            Arc::clone(self.mn.as_ref().expect(
                "demote_socket_enter called with no active M:N scheduler (self.mn is None)",
            ));
        {
            let mut c = sched.lock();
            c.running -= 1;
            sched.inflight.fetch_add(1, Ordering::Relaxed);
            drop(c);
            sched.cv.notify_all();
        }
        if !self.demoted {
            if !self.spawn_replacement_worker(&sched, self.wid) {
                let mut c = sched.lock();
                c.running += 1;
                sched.inflight.fetch_sub(1, Ordering::Relaxed);
                drop(c);
                return Err(self.err(
                    "a socket op inside a native callback could not demote the worker (OS thread limit \
                     reached) — reduce concurrent in-callback blocking or raise the thread limit"
                        .to_string(),
                    span,
                ));
            }
            self.demoted = true;
        }
        Ok(())
    }

    /// D5 owe #3 Path C (#3 socket half) — exit the in-callback socket demote: un-account `inflight →
    /// running` (the `+1` is essential — the fiber's next dispatch does `running -= 1`, which would
    /// underflow without this restore). Mirrors [`Vm::demote_block_sleep`] step 4.
    pub(super) fn demote_socket_exit(&mut self) {
        let sched =
            Arc::clone(self.mn.as_ref().expect(
                "demote_socket_exit called with no active M:N scheduler (self.mn is None)",
            ));
        let mut c = sched.lock();
        c.running += 1;
        sched.inflight.fetch_sub(1, Ordering::Relaxed);
    }

    /// D5 owe #3 (Path C) — spawn a fresh OS thread running a replacement M:N worker shell over the
    /// same scheduler, reusing the demoting worker's `wid`. A RAW thread ([`VM_STACK_BYTES`] stack,
    /// like the pool), NOT a `pool::submit` job: the bounded pool is fixed-size, so a blocked-in-callback
    /// pool thread would shrink it — the demoted thread is "off the pool" (Go grows `m` under
    /// cgo/syscalls). The replacement drains the shared run queue until `terminate` (`Take::Stop`), then
    /// exits — detached + reaped at nursery/process end (it holds only `Arc`s, so the joining thread can
    /// return without any use-after-free). Panic-guarded like the farmed shells. Returns `false` iff the
    /// OS refused the thread (caller faults the fiber rather than blocking with no coverage); the
    /// cancel `.expect` is a true invariant (only reachable on the M:N engine in a nursery).
    pub(super) fn spawn_replacement_worker(&self, sched: &Arc<MnSched>, wid: usize) -> bool {
        let cancel = self
            .cancel
            .as_ref()
            .expect("Path C replacement worker without a cancel token");
        let mut shell = self.spawn_shell(sched, cancel);
        let sched = Arc::clone(sched);
        std::thread::Builder::new()
            .stack_size(VM_STACK_BYTES)
            .name("chezzi-mn-repl".into())
            .spawn(move || {
                // SENTINEL — the replacement covers the demoted thread's `wid` and drains the GLOBAL
                // queue (across all scopes) until global terminate; the demoted owner returns on its own
                // (its fiber settles → `self.demoted` exits its loop), so the replacement must not stop
                // early on any single scope.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    shell.mn_worker_loop(&sched, wid, SENTINEL_SCOPE)
                }));
            })
            .is_ok()
    }

    /// D2b — a worker shell's lifetime: pull a runnable fiber, run it to its next park/finish, settle,
    /// repeat until the scheduler terminates, over a shared run queue + park set across threads.
    /// Cross-nursery flat scheduler — `owner_scope` is the scope this worker is the INLINE OWNER of
    /// (it returns when that scope completes — scope-scoped owner stop), or [`SENTINEL_SCOPE`] for a
    /// FARMED helper / drainer (which never self-stops, only on global terminate). The fiber it runs may
    /// belong to ANY scope (the queue is global): `finish`/`cancel_drain` use the FIBER's `scope_id`,
    /// while `take_runnable`'s stop check uses `owner_scope`.
    pub(super) fn mn_worker_loop(&mut self, sched: &Arc<MnSched>, wid: usize, owner_scope: usize) {
        self.wid = wid; // D5 owe #3 (Path C) — `demote_recv_block` reuses this for the replacement worker
        let mut tick: u64 = 0;
        loop {
            tick = tick.wrapping_add(1);
            let mut fiber = match sched.take_runnable(wid, tick, owner_scope) {
                Take::Run(f) => f,
                Take::Stop => return,
            };
            let task_index = fiber.task_index;
            let scope_id = fiber.scope_id;
            let span = fiber.span;
            match self.run_one_fiber(&mut fiber, span) {
                Disp::Park(key, core) => sched.park(key, &core, fiber),
                // Bounded backpressure — the send-side park (gap re-check = space, not a message).
                Disp::SendPark(key, core) => sched.park_send(key, &core, fiber),
                // §6d — multi-channel `wait` park: file ONE shared token in every arm's bucket.
                Disp::WaitPark(arms) => sched.park_wait(arms, fiber),
                Disp::Yield => sched.yield_fiber(fiber),
                // D5 — the fiber hit a blocking native; hand it + the call to the dirty pool (frees
                // this worker). The pool re-enqueues it on completion via `complete_offload`.
                Disp::Offload(req) => sched.offload(fiber, req),
                // D6 — the fiber's socket op `WouldBlock`ed; hand it + the fd to the netpoller (frees
                // this worker). The poller re-enqueues it via `complete_offload` on OS readiness.
                Disp::PollPark(pp) => sched.poll_park_offload(fiber, pp),
                Disp::Finish(outcome) => {
                    // `finish` reports whether the STORED outcome aborts: it may itself turn a `Done`
                    // into a hard-halt over-memory `Fault` (W7-26r), which needs the same sibling
                    // drain a task's own fault does.
                    let aborts = sched.finish(task_index, scope_id, outcome);
                    // A fault/exit tripped the FIBER's SCOPE cancel (in `classify_mn_outcome`, via the
                    // re-pointed `self.cancel`); requeue THAT scope's parked siblings so they observe it
                    // and unwind (running ones see it at a back-edge). `cancel_drain(scope_id)` reaches
                    // channel-`recv`-parked fibers in this scope ONLY (never outer siblings — structured
                    // concurrency); `drain_sched` reaches the netpoller-parked ones. Together they cover
                    // every parked fiber of the faulting scope, so a net server sharing a nursery with a
                    // faulting sibling now unwinds instead of hanging (D6b — the production-ready gate).
                    if aborts {
                        sched.cancel_drain(scope_id);
                        poller::drain_sched(sched);
                    }
                }
            }
            // D5 owe #3 (Path C) — this worker DEMOTED mid-fiber (blocked in place on a callback `recv`)
            // and a replacement now covers its `wid`. The fiber it was running has just settled
            // (finished, or re-parked for another worker to resume), so this thread exits to keep the
            // net live-worker count at N. The joining thread's `wait_for_completion` holds the reduce
            // until the replacements fill every slot.
            //
            // gaps.md W8-7 hang regression — notify HERE, at the departure, not at `yield_fiber`'s
            // requeue. `yield_fiber`'s no-notify argument #1 ("the yielding worker loops straight back
            // into `take_runnable`") is false on exactly this exit: this worker does NOT loop back, it
            // returns. The demoted fiber's replacement, spun up by `demote_recv_block` et al. at
            // demote time, typically parked into `take_runnable`'s untimed `cv.wait` well before this
            // point (`runnable == 0` while the demoted fiber ran) — so if the fiber the demoted thread
            // was just running preempted on its way out (`Disp::Yield` -> `yield_fiber`, no notify),
            // the requeued fiber has ZERO live consumers: not this thread (leaving), not the
            // replacement (asleep, untimed). `wait_for_completion` is ALSO an untimed `cv.wait`, so the
            // joiner never wakes either — a hang, not a slow path. All four demote entry points route
            // here (`demote_recv_block`, `demote_wait_block`, `demote_block_sleep`,
            // `demote_socket_enter`), so the notify must be unconditional on this exit, not shaped to
            // any one of them. Cost: once per demoted-thread exit, not once per `CONTEXT_REDS`
            // dispatched ops — off the hot path W8-7 is about.
            //
            // THE ENUMERATION `yield_fiber` CROSS-REFERENCES — every way this loop is left, and who
            // consumes a fiber `yield_fiber` requeued without a wake. It lives here, in the code that
            // claims to hold it, because the W8-7 hang was caused by a comment asserting a case it had
            // not enumerated:
            //   (a) `Take::Stop => return` (the `match` at the top). `Stop` is RETURNED BY
            //       `take_runnable`, so a worker taking it has by definition re-entered and evaluated
            //       the queue — it cannot be holding an unconsumed yield. Of its four sites, the
            //       owner-stop, deadlock and W7-58 branches all `notify_all` before returning; the
            //       `c.terminate` branch does not, which is benign because both writers of
            //       `terminate` (`MnSched::finish`, `flag_deadlock`) `notify_all` right after setting
            //       it, and the run is ending regardless.
            //   (b) this `self.demoted` return — the hole, closed by the notify below.
            //   (c) a panic is NOT an exit: `run_one_fiber` wraps its whole body in `catch_unwind` and
            //       converts a panic into `Disp::Finish(panic_outcome(..))`, so the loop continues. The
            //       outer `catch_unwind`s in the shells only see a panic from `take_runnable`/`park`/
            //       `finish` themselves, which is a pre-existing scheduler-bug path (see
            //       `eager_joiner_runs_fibers`' own hazard note).
            if self.demoted {
                sched.cv.notify_all();
                return;
            }
        }
    }

    /// D2b — run a single fiber on this shell: swap its context in, start/resume it until it parks or
    /// finishes, decide its disposition WHILE its heap is live (the park key and outcome are heap-keyed
    /// reads), then swap the context back out. The run is panic-guarded so a worker-VM panic becomes a
    /// task `Fault` (keeps the loop alive + the slot filled — the join can't hang).
    pub(super) fn run_one_fiber(&mut self, fiber: &mut Fiber, span: Span) -> Disp {
        self.swap_ctx(&mut fiber.ctx);
        // Cross-nursery flat scheduler — RE-POINT the shell's `self.cancel` to THIS fiber's SCOPE cancel
        // on every swap-in. One shell runs fibers from MULTIPLE scopes off the global queue; the
        // back-edge cancel check (`run_until`), `trip_cancel` (on this fiber's fault/exit), the demote
        // loops, and the netpoller `register` all read `self.cancel`, so it MUST track the running
        // fiber's scope — else an inner fault would trip the wrong scope and cancel an outer sibling.
        // `self.mn` is always `Some` here in practice — the only caller, `mn_worker_loop`, only ever
        // runs a shell built by `spawn_shell`, which unconditionally sets `mn`. The `if let` is
        // defensive, not a reachable no-op branch.
        if let Some(sched) = self.mn.clone() {
            let (scope_cancel, ancestors) = {
                let c = sched.lock();
                let s = &c.scopes[fiber.scope_id];
                (Arc::clone(&s.cancel), s.ancestors.clone())
            };
            self.cancel = Some(scope_cancel);
            // …and the ENCLOSING scopes' flags with it: an outer cancel must reach a nested scope's
            // fibers (structured concurrency), or a nested nursery inside a cancelled task never dies.
            self.cancel_outer = ancestors;
        }
        self.suspend = None;
        self.wait_suspend = None; // set by `op_wait_poll`'s M:N snapshot-park (→ `Disp::WaitPark`)
        self.send_suspend = None; // set by a full bounded `send` (→ `Disp::SendPark`)
        self.offload = None;
        self.poll_park = None;
        self.cancelled = false;
        self.pending_exit = None;
        self.reds = CONTEXT_REDS; // D3 — fresh reduction budget on every schedule-in (BEAM semantics)
        self.yield_now = false;
        let state = std::mem::replace(&mut fiber.state, FiberState::Ready);
        // D5 — a fiber resumed after a blocking-native offload carries the pool's result. Take it now
        // (heap is swapped in) and push it below so the suspended `Call` completes and dispatch
        // continues past it.
        let resume_native = fiber.resume_native.take();
        let disp = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // D5 — lower + push the offloaded native's result before resuming, so the operand stack
            // holds what the `Call` would have pushed and `run_until` continues correctly. The `Err`
            // arm carries a fault from the pool job: either the native PANICKED (caught in the job,
            // surfaced here — faulting the task without running its defers, exactly as an inline
            // native panic does via `run_one_fiber`'s outer `catch_unwind`), or it returned a
            // `HostError` (unreachable for the scoped fns — fs/io surface I/O failures as `Result`
            // *values* and arg types are checker-guaranteed). Either way the task faults.
            if let Some(result) = resume_native {
                match result {
                    Ok(nr) => {
                        let v = self.lower_native(nr);
                        self.push(v);
                    }
                    Err(rte) => {
                        // W7-16 — …or the offloaded `sleep_ms` was ENDED by a cancel (the timer job
                        // observes the scope flags mid-sleep now). `self.cancelled` was just reset
                        // above, so without this the cancelled sleeper classifies as a **Fault**,
                        // trips its siblings and MASKS the real error that cancelled it. The
                        // `executor_hard_halt` guard keeps the other direction honest: a `--timeout`
                        // / over-memory abort must never be swallowed into a silent `Cancelled`.
                        if !executor_hard_halt(&rte) && self.cancel_requested() {
                            self.cancelled = true;
                        }
                        // …and it must UNWIND, not merely finish. This arm returns WITHOUT re-entering
                        // `run_until`, so nothing else would run the task's `defer`s — a cancel
                        // delivered mid-sleep would silently skip every registered cleanup, while the
                        // same cancel arriving 50 ms earlier (the entry checkpoint, which faults inside
                        // the VM) runs them. `docs/concurrency.md`: "a cancelled task then unwinds
                        // through its `defer`s… 'does my cleanup run?' no longer depends on scheduler
                        // timing". Same shape as `run_until`'s `cancel_bypass` funnel — including
                        // re-stamping the hard-halt markers a mid-unwind `defer` fault would strip, so
                        // `--timeout`/`--max-heap` stay un-catchable. A native PANIC keeps its
                        // documented no-defer behavior: it sets neither the cancel latch nor a marker.
                        let rte = if self.cancelled || rte.is_over_memory || rte.is_timed_out {
                            let (over_mem, timed) = (rte.is_over_memory, rte.is_timed_out);
                            let r = self.unwind_deferred(0, false).unwrap_or(rte);
                            let r = if over_mem { r.over_memory() } else { r };
                            if timed { r.timed_out() } else { r }
                        } else {
                            rte
                        };
                        return Disp::Finish(self.classify_mn_outcome(Err(rte)));
                    }
                }
            }
            // D6b — a fiber resumed from a non-blocking `connect` park carries the connecting socket in
            // `pending_connect` (swapped in with its ctx). Complete the handshake (read `SO_ERROR`) and
            // push the `Result[Socket]` the `net.connect` call site is waiting for, then continue past
            // it. `finish_pending_connect` never faults (it yields a `Result` *value*). Mutually
            // exclusive with `resume_native` (a fiber is offload-parked OR connect-parked, never both).
            if let Some(cip) = self.pending_connect.take() {
                // W7-18 — a connect park carries no `timeout_ms` of its own, so only the run's
                // `--timeout` clamp can have set this: RAISE the hard halt here rather than clearing
                // the flag and trusting a later checkpoint. There is no later checkpoint on this path
                // — `net.connect` is followed by a straight-line `match` with no back-edge and no
                // blocking op, so the fiber would finish NORMALLY, the nursery would join, and the
                // test body's `assert` would run: W7-17's original SWALLOWED symptom, re-created on
                // the connect path by the fix meant to close it. Raising also avoids handing back the
                // `Ok(Socket)` `finish_pending_connect` would produce for a handshake still in flight
                // (it reports on `SO_ERROR` alone; the blocking arm's `attempt` closure additionally
                // checks `peer_addr`). The unwind + re-stamp is the `resume_native` arm's, verbatim: without
                // it the aborted task skips every `defer` (W7-16's bug, W7-17 lesson 2).
                if std::mem::take(&mut self.poll_timed_out) {
                    let rte = self
                        .err(
                            format!("test exceeded --timeout ({}ms)", self.timeout_ms),
                            cip.span,
                        )
                        .timed_out();
                    let rte = self.unwind_deferred(0, false).unwrap_or(rte).timed_out();
                    return Disp::Finish(self.classify_mn_outcome(Err(rte)));
                }
                let v = self.finish_pending_connect(cip);
                self.push(v);
            }
            let res = match state {
                FiberState::Pending(task) => self.start_task(task),
                FiberState::Ready | FiberState::Blocked => self.run_until(0),
            };
            if res.is_ok() && self.offload.is_some() {
                // D5 — the fiber hit a blocking native; hand it to the dirty pool. Mutually exclusive
                // with `suspend`/`yield_now` (offload returns up via the `paused()` gate before any
                // `recv` runs or the budget is re-checked).
                Disp::Offload(self.offload.take().unwrap())
            } else if res.is_ok() && self.poll_park.is_some() {
                // D6 — the fiber's socket op `WouldBlock`ed; hand it to the netpoller (frees this
                // worker). Mutually exclusive with `offload`/`suspend`/`yield_now` — the socket op
                // returns up via the `paused()` gate before any other safepoint runs.
                Disp::PollPark(self.poll_park.take().unwrap())
            } else if res.is_ok() && self.suspend.is_some() {
                let h = self.suspend.take().unwrap();
                // Capture the park key + the channel `Arc` WHILE the fiber heap is live (`h` is a
                // GcRef into it); `park` re-checks the queue through this `Arc` under the sched lock.
                Disp::Park(self.channel_core_ptr(h), self.channel_core(h))
            } else if res.is_ok() && self.send_suspend.is_some() {
                // Bounded backpressure — the fiber blocked on a full `send`. Capture the key + core
                // WHILE the fiber heap is live (like `Disp::Park`); `park_send` re-checks SPACE.
                let h = self.send_suspend.take().unwrap();
                Disp::SendPark(self.channel_core_ptr(h), self.channel_core(h))
            } else if res.is_ok() && self.wait_suspend.is_some() {
                // §6d — the fiber blocked on a multi-channel `wait`. Capture each arm's (key, core)
                // WHILE the fiber heap is live (the `GcRef`s index into it), exactly as `Disp::Park`
                // captures the single recv key; `park_wait` re-checks every arm under the sched lock.
                let handles = self.wait_suspend.take().unwrap();
                let arms: Vec<(usize, Arc<ChannelCore>, bool)> = handles
                    .iter()
                    .map(|&(h, is_send)| (self.channel_core_ptr(h), self.channel_core(h), is_send))
                    .collect();
                Disp::WaitPark(arms)
            } else if res.is_ok() && self.yield_now {
                // D3 — budget exhausted (mutually exclusive with `suspend`: the safepoint returns
                // before dispatching, so no `recv` ran this slice). Frames stay intact; resume
                // re-enters `run_until(0)`.
                Disp::Yield
            } else {
                Disp::Finish(self.classify_mn_outcome(res))
            }
        }))
        .unwrap_or_else(|p| Disp::Finish(self.panic_outcome(p, span)));
        self.swap_ctx(&mut fiber.ctx);
        disp
    }

    /// D2b — a worker-VM PANIC (a VM `unwrap`/index bug, a panicking native/FFI callback) became this
    /// fiber's outcome. It never reached [`Vm::classify_mn_outcome`], so this is the ONLY place that can
    /// trip the fiber's SCOPE cancel on that path — and it MUST: `mn_worker_loop` treats a `Fault` as an
    /// abort and calls `cancel_drain`, but a requeued sibling with `cancel == false` just re-runs `recv`
    /// and PARKS AGAIN; the scope then quiesces uncancelled, `is_deadlocked` fires (correctly, by its own
    /// rules), and `flag_deadlock` drops those siblings without `unwind_deferred` — their `defer`s never
    /// run, hidden behind the panic-fault (`reduce_task_slots` ranks Fault > Deadlocked). The trip is
    /// program-ordered before `finish`'s lock release, which publishes it (see `trip_scope_cancel`).
    /// `self.cancel` is the RUNNING fiber's scope cancel (re-pointed at every `swap_ctx` in).
    pub(super) fn panic_outcome(
        &mut self,
        p: Box<dyn std::any::Any + Send>,
        span: Span,
    ) -> TaskOutcome {
        self.trip_cancel();
        TaskOutcome::Fault {
            err: panic_to_fault(p, span),
            out: Vec::new(),
            stderr: Vec::new(),
        }
    }

    /// D2b — classify a finished fiber's run into a [`TaskOutcome`] (the M:N analogue of
    /// [`ReadyWorker::run_outcome`]). Unlike the legacy path it uses `start_task`/`run_until` and
    /// **discards the task's return value** (a spawned task's return is never observable to its
    /// caller). Trips the shared cancel flag on a fault/exit so siblings abort. The
    /// fiber's `out`/`stderr` are taken from the live (swapped-in) shell buffers.
    pub(super) fn classify_mn_outcome(&mut self, res: Result<(), RuntimeError>) -> TaskOutcome {
        if let Some(code) = self.pending_exit {
            self.trip_cancel();
            TaskOutcome::Exit {
                code,
                out: std::mem::take(&mut self.out),
                stderr: std::mem::take(&mut self.stderr),
            }
        } else if self.cancelled {
            TaskOutcome::Cancelled {
                out: std::mem::take(&mut self.out),
                stderr: std::mem::take(&mut self.stderr),
            }
        } else {
            match res {
                Err(e) => {
                    self.trip_cancel();
                    TaskOutcome::Fault {
                        err: e,
                        out: std::mem::take(&mut self.out),
                        stderr: std::mem::take(&mut self.stderr),
                    }
                }
                Ok(()) => TaskOutcome::Done(WorkerResult {
                    value: WireValue::Nil,
                    out: std::mem::take(&mut self.out),
                    stderr: std::mem::take(&mut self.stderr),
                }),
            }
        }
    }

    /// B3.3-threads / D2b — reduce a nursery's per-task outcome slots (task order) into the join's
    /// result, flushing output and applying `Exit`-over-`Fault` precedence. Called from the M:N
    /// nursery join paths ([`run_mn_nursery`] and friends).
    ///
    /// Every slot flushes its buffered output at its task-order slot, unconditionally, regardless of
    /// outcome kind: `Done`, `Exit` (decision F), `Fault` (W7-5c — EVERY faulting task's output flushes,
    /// not just the lowest-index one), `Cancelled`, and `Deadlocked` all write their bytes — so a
    /// task's already-printed partial output is preserved rather than dropped when it
    /// unwinds/cancels/aborts. The outcome kinds differ only in which error (if
    /// any) they contribute to the terminal result — `Cancelled` never contributes one; see the
    /// precedence below for the rest. The fault-free goldens only ever hit `Done`, so they stay
    /// byte-identical. A `Deadlocked` slot (the M:N deadlock-abort synthetic outcome — every parked
    /// fiber gets one; a real `Fault`/`Exit` normally trips `terminate` first, and the precedence below
    /// resolves any mix deterministically) reports one deadlock error per parked fiber, of which only
    /// the lowest-index one propagates. Precedence: an `os.exit` is an UNCONDITIONAL hard halt, so the
    /// lowest-index `Exit` wins over any `Fault` regardless of index — otherwise a lower-index
    /// recoverable fault could demote a child's `os.exit` to a catchable error. W7-5 review Fix 1: a
    /// lowest-index [`executor_hard_halt`]-marked `Fault` (over-memory/timeout) likewise
    /// wins over any ordinary `Fault` regardless of index, for the same reason — an Executor drain's
    /// hard halt must never be demoted to a catchable error by an earlier sibling's plain fault. Full
    /// precedence: `Exit` > hard-halt `Fault` > ordinary `Fault` > `Deadlocked`, lowest index winning
    /// within each kind (scan order + `is_none()`).
    pub(super) fn reduce_task_slots(
        &mut self,
        slots: Vec<Option<TaskOutcome>>,
    ) -> Result<(), RuntimeError> {
        let mut first_exit: Option<i32> = None;
        let mut first_fault: Option<RuntimeError> = None;
        // W7-5 review Fix 1: the lowest-index HARD-HALT fault (`executor_hard_halt` —
        // over-memory/timeout), tracked separately from `first_fault` above. It must win
        // final propagation over an earlier ordinary fault, or a later `--max-heap`/`--timeout` abort
        // gets demoted to a catchable error by an earlier sibling's plain fault (letting `recover:`
        // swallow a hard halt it must never be able to catch — `exec.rs`'s cancel-bypass check is
        // keyed on these markers). This does NOT change flush behavior — every fault flushes its
        // buffered output regardless of index (W7-5c) — only which error `reduce_task_slots` returns.
        let mut first_hard_fault: Option<RuntimeError> = None;
        let mut deadlock_err: Option<RuntimeError> = None;
        for slot in slots {
            // W7-60 — a `None` here means the slot was already drained by `EagerState::take_finished`
            // on a `join_eager_jobs` bail-out (its output is flushed, its outcome consumed), which is
            // the one way a reduce can legitimately see an empty slot. The invariant the old
            // `.expect("every task slot was filled before join returned")` guarded — a job that never
            // filled its slot — is still asserted, at the place that actually knows: the join only
            // reaches `take_slots` with `outstanding() == 0`.
            let Some(outcome) = slot else { continue };
            match outcome {
                TaskOutcome::Done(wr) => {
                    self.out.extend_from_slice(&wr.out);
                    self.stderr.extend_from_slice(&wr.stderr);
                }
                TaskOutcome::Exit { code, out, stderr } => {
                    self.out.extend_from_slice(&out);
                    self.stderr.extend_from_slice(&stderr);
                    if first_exit.is_none() {
                        first_exit = Some(code);
                    }
                }
                TaskOutcome::Fault { err, out, stderr } => {
                    // EVERY faulting task flushes its buffered output at its task-order slot,
                    // unconditionally (W7-5c) — after lower-index Done/Exit, before the fault
                    // propagates — like the `Deadlocked` arm below. Under the W7-5 run-all drain a
                    // second fault is ordinary, not a race artifact, so gating the flush on
                    // `first_fault.is_none()` would delete real output that serial printed live. Only
                    // the LOWEST-index error still propagates (subject to the hard-halt-over-ordinary
                    // precedence in `reduce_task_slots`'s doc comment).
                    //
                    // RESIDUAL RACE (intentionally not chased here — applies ONLY to a genuine
                    // multi-printer REAL-fault reduce; the multi-parked DEADLOCK case is handled by
                    // the `Deadlocked` arm below, which flushes ALL parked buffers): this matches a
                    // strictly sequential, stop-at-first-fault reference order byte-for-byte only when
                    // the faulting task is the nursery's SOLE output-producer. With additional
                    // output-producing siblings the M:N result can still diverge from that sequential
                    // order — a sibling that reaches `Done` before the faulter's cancel-trip keeps its
                    // output (a strictly sequential run would never have reached it), and whether a
                    // lower-index sibling ends `Fault` vs `Cancelled` (which selects the propagating
                    // fault) is itself a scheduler race. The buffer-and-flush-per-task model cannot
                    // reconcile concurrency with a sequential stop-at-fault order, so multi-task-with-
                    // fault output ordering is a separate, pre-existing nondeterminism (see the
                    // single-producer case covered by the test
                    // `parallel_faulting_task_flushes_partial_output_3engine`).
                    if first_hard_fault.is_none() && executor_hard_halt(&err) {
                        first_hard_fault = Some(err.clone());
                    }
                    self.out.extend_from_slice(&out);
                    self.stderr.extend_from_slice(&stderr);
                    if first_fault.is_none() {
                        first_fault = Some(err);
                    }
                }
                TaskOutcome::Deadlocked { err, out, stderr } => {
                    // The M:N deadlock detector recorded EVERY still-parked fiber with this synthetic
                    // outcome (`flag_deadlock`). Unlike a real `Fault`, ALL parked buffers flush at
                    // their task-order slot (no `is_none()` gate) — so with two-or-more parked fibers
                    // a higher-index printer's output is preserved, matching what a strictly sequential
                    // run would have printed live before the deadlock returned. Only ONE deadlock error
                    // propagates (the first, i.e. lowest task-order); a real fault/exit normally trips
                    // `terminate` before the detector fires, but the terminal `match` below applies a
                    // strict `Exit` > `Fault` > `Deadlocked` precedence so a mixed vector (were one to
                    // arise under a race) still resolves deterministically.
                    self.out.extend_from_slice(&out);
                    self.stderr.extend_from_slice(&stderr);
                    if deadlock_err.is_none() {
                        deadlock_err = Some(err);
                    }
                }
                TaskOutcome::Cancelled { out, stderr } => {
                    // A cancelled task's buffered output flushes at its task-order slot (it really
                    // printed those bytes — with cancellation points a started task always completes
                    // its prologue), matching serial, which prints live and cannot un-print. Cross-task
                    // ORDER stays nondeterministic; the line SET is the contract.
                    self.out.extend_from_slice(&out);
                    self.stderr.extend_from_slice(&stderr);
                }
            }
        }
        // W7-5 review Fix 1: `first_hard_fault` (if any) wins over `first_fault` here — the
        // precedence is `Exit` > hard-halt-marked `Fault` > ordinary `Fault` > `Deadlocked`, lowest
        // index winning within each kind. This changes ONLY which error propagates; it does not
        // touch the `Exit`-over-`Fault` rule above or the nursery's abort semantics (every fault
        // still trips the shared cancel flag the same way it always did).
        // W7-47 — `first_exit` only ever comes from a SLOT, so an `os.exit` issued by an eager
        // `Executor` job (which owns no slot) is invisible here, and a nursery whose tasks are all
        // blocked reported a `deadlock` — a confident WRONG verdict about the user's program, the
        // `parked-is-not-stuck` class. The run-scoped cell carries that exit, folded in as an ordinary
        // `first_exit` so there is ONE precedence table (Go's rule: the first `os.Exit` wins).
        let first_exit = first_exit.or_else(|| self.quiesce.pending());
        match (first_exit, first_hard_fault.or(first_fault), deadlock_err) {
            // A child `os.exit` hard-halts the parent: set `pending_exit` and return the exit
            // sentinel. The op→`step`→`run_until` chain sees `pending_exit` and unwinds past every
            // `recover:` to the driver, which reports `code` as the process exit status (decision C).
            // It wins over any sibling fault — a hard halt is never demoted to a catchable error.
            (Some(code), _, _) => {
                self.pending_exit = Some(code);
                Err(self.err("exit".to_string(), Span::RUNTIME))
            }
            // A real fault propagates normally so an outer `recover:` can still catch it (unless it
            // carries a hard-halt marker, in which case `first_hard_fault` already selected it above
            // and `exec.rs`'s cancel-bypass check keeps `recover:` from swallowing it anyway).
            (None, Some(e), _) => Err(e),
            // Deadlock abort: all parked buffers already flushed above; propagate ONE deadlock error.
            (None, None, Some(e)) => Err(e),
            (None, None, None) => Ok(()),
        }
    }

    /// D0 — the `ChannelCore` identity (`Arc::as_ptr as usize`) behind a channel handle, the stable
    /// key for `SchedCore::parked`. Stable across the distinct `GcRef`s sibling fibers hold for
    /// the same channel (`spawn` deep-clones the handle onto the shared `Arc`).
    pub(super) fn channel_core_ptr(&self, h: GcRef) -> usize {
        match self.heap.get(h) {
            Obj::Channel(core) => Arc::as_ptr(core) as usize,
            // A fiber only ever parks via a `recv` on a `Channel`, so `suspend` always holds a
            // channel handle. Fail loud (matching `channel_core` above) rather than silently filing
            // the park under a sentinel key `wake_on_send` would never match — a silent mis-key
            // would mis-report a `deadlock` (review: Incident Response Commander).
            _ => unreachable!("channel_core_ptr on a non-channel park handle"),
        }
    }

    /// D0 — a `send` into channel `h` may unblock fibers parked on its `recv`. Wake the matching
    /// bucket on every live `MnSched`.
    pub(super) fn wake_on_send(&mut self, h: GcRef) {
        let key = self.channel_core_ptr(h);
        // W7-56 — this VM has no sched in scope (that is the only branch that calls here), but the
        // RUN may: an eager `Executor` job's `Vm` gets neither `mn` nor `mn_enlist_sched` from
        // `spawn_worker`, so its `send`/`close` notified the channel condvar and nothing else, while
        // the fiber it just fed sat in some nursery's `SchedCore::parked` — which only that sched's
        // `wake_bucket` can drain, and whose idle workers `cv.wait` untimed. Wake every live sched's
        // bucket for this key; `wake_bucket` drains the whole bucket, so this serves `send` and
        // `close` alike, and an over-wake (woken fiber finds the queue empty and re-parks) is the
        // already-tolerated pattern. Dead entries are pruned as we walk (`Weak` — no deregistration).
        //
        // Here rather than in `channel_send_wire` because this fn is the shared tail of ALL FIVE
        // no-sched wake sites (`close`, `trip`, `channel_send_wire`, the bounded enqueue, and the
        // bounded slot-free wake); patching one would leave the other four broken. Every one of them
        // has dropped `ChannelCore::q` before calling, so taking the sched lock here is q-free.
        {
            let mut g = self
                .sched_registry
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !g.is_empty() {
                let live: Vec<_> = g.iter().filter_map(|w| w.upgrade()).collect();
                g.retain(|w| w.strong_count() > 0);
                drop(g);
                for s in live {
                    s.wake_key(key);
                }
            }
        }
    }

    /// gaps.md W7-57 — a run-wide `os.exit` must tear every live nursery down NOW, exactly as an
    /// intra-nursery fault/exit already does for its OWN scope (`mn_worker_loop`'s `if aborts { … }`).
    /// Without it the exit is observed only by the POLLING waits, so a party that is spinning, parked on
    /// a `recv`, or asleep outlives it — Go's `os.Exit` kills all three within a few ms.
    ///
    /// The registry walk is [`Vm::wake_on_send`]'s, verbatim: upgrade the `Weak`s, prune the dead, DROP
    /// the registry lock, then touch each sched. Lock order stays `sched_registry → SchedCore`, and
    /// `request_exit` runs from an `Inline` native with no sched core lock held.
    pub(super) fn halt_all_scheds(&self) {
        let mut g = self
            .sched_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if g.is_empty() {
            return;
        }
        let live: Vec<_> = g.iter().filter_map(|w| w.upgrade()).collect();
        g.retain(|w| w.strong_count() > 0);
        drop(g);
        for s in live {
            s.cancel_all();
        }
    }

    /// W7-56 — publish a freshly built `MnSched` on the run's registry (see [`Vm::sched_registry`]).
    pub(super) fn register_sched(&self, sched: &Arc<MnSched>) {
        self.sched_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::downgrade(sched));
    }

    /// gaps.md W7-58 — register this thread as a party blocked in a `parallel:` nursery JOIN, for as
    /// long as the returned guard lives. Without it a `main` sitting in `mn_worker_loop` is invisible
    /// to the process-wide verdict, so `parties.len() < live` vetoes forever and a genuinely stuck
    /// job + stuck nursery hangs instead of faulting.
    ///
    /// **The gate is deliberately NOT [`Vm::is_counted_party`].** `owns_os_thread` additionally
    /// requires `mn_enlist_sched.is_none()`, which is false for the very builder that owns an
    /// early-enlisted scope — the commonest owner there is. What `quiesce`'s `live` actually counts is
    /// "threads with no scheduler UNDER them": top-level `main` (the `1 +`) and an eager `Executor`
    /// job (the `Σ outstanding`). That is exactly `mn.is_none()`.
    ///
    /// **A worker SHELL (`mn.is_some()`) must never register.** It is not in `live`, so registering it
    /// would let `parties.len()` exceed `live` — the one error direction that faults a live program.
    /// The gate excludes it.
    ///
    /// §2c1 — `join_eager_nursery`/`abort_eager_nursery` DO call this now. They used to be exempt
    /// because `activate_eager_nursery` `debug_assert!`ed `mn.is_some()`, so their joiner was always a
    /// shell; the outermost nursery is eager now, so its joiner is top-level `main`. The gate keeps
    /// those calls no-ops on the nested (shell) path.
    ///
    /// Called with NO sched core lock held — the total order is `parties` → `SchedCore`.
    pub(super) fn nursery_party_guard(
        &self,
        sched: &Arc<MnSched>,
    ) -> Option<crate::vm::quiesce::PartyGuard> {
        self.mn.is_none().then(|| {
            self.quiesce
                .block(crate::vm::quiesce::PartyWait::Nursery(Arc::clone(sched)))
        })
    }

    /// `chezzi test --max-heap` — collect and read the cap, at a point that is NOT an instruction
    /// boundary. Callers must have every live value of the pending call already on `self.stack`.
    ///
    /// W6-10s residual (a), closed: `over_cap` is assigned only in `Heap::sweep()`, `sweep()` runs
    /// only when `should_collect()` fires, and the only non-test caller of `should_collect()` is the
    /// top of [`run_until`](Vm::run_until)'s dispatch loop — whose guard is `self.frames.len() >
    /// base_level`. **A task whose entire body is one native call pushes no frame, so it never enters
    /// that loop and its heap is never sampled at all.** Measured on the release binary: `spawn
    /// xs.len()` over a 300 K-element `List[str]` PASSED a 8 MB cap at 170.9 MB (21×), while the
    /// byte-identical `spawn use(xs)` (`use` = `return xs.len()`, one bytecode frame) correctly
    /// reported OVER-MEMORY at the same RSS — the verdict tracked who ran bytecode, not who held
    /// bytes. The worker's copy is the BIGGER one, too: `from_wire` mints a fresh `Obj::Str` per
    /// element and does not re-intern, so N aliases of one interned literal become N objects.
    ///
    /// SCOPE — this owns the FIBER door only: [`start_task`](Vm::start_task), which every `spawn` /
    /// `parallel:` task is dispatched through. It does NOT cover eager `Executor` jobs, which reach
    /// the VM by `ReadyWorker::invoke` instead; those keep their own forced sample
    /// (`Heap::request_collect`, set in [`spawn_worker`](Vm::spawn_worker)). Neither mechanism
    /// subsumes the other and the two-door split is written out at that call site — read it before
    /// deleting either.
    ///
    /// COST CEILING: this is a FULL mark-sweep of the heap the fiber runs on, once per task start.
    /// The heap is the M:N worker's own and freshly born, so it is small. (An engine where every fiber
    /// shared ONE heap would make a capped run O(live heap × tasks); the cooperative engine that did
    /// that has been removed, and `--max-heap` was refused with it at the CLI anyway. If a
    /// shared-heap execution mode is ever reintroduced, this is the line that needs a cheaper
    /// trigger.)
    ///
    /// **Collecting with `self.frames` EMPTY is sound.** Empty frames stop `run_until`'s *loop*; they
    /// do not make a direct [`collect`](Vm::collect) unsafe. `collect` roots the operand stack first
    /// (`for v in &self.stack`), plus the swapped-in child's module objects, intern cache and
    /// snapshot registry — the same root set the task's first instruction boundary would see, minus
    /// frame slots that do not exist yet. That is why the contract above is "operands on the stack".
    ///
    /// No `unwind_deferred`: there are no frames and no `defer` registered yet. The `is_over_memory`
    /// marker alone is what buckets the verdict `OverMemory` and makes it uncatchable by `recover:`.
    fn sample_mem_cap(&mut self, span: Span) -> Result<(), RuntimeError> {
        self.collect();
        if self.heap.over_cap() {
            return Err(self
                .err(
                    format!("test exceeded --max-heap ({} bytes)", self.heap.mem_cap()),
                    span,
                )
                .over_memory());
        }
        Ok(())
    }

    /// Launch a fiber's initial task in the (already swapped-in) child context. Mirrors the old
    /// `run_pending`, but a blocking `recv` may park the fiber mid-flight: the `do_method_call` /
    /// `invoke_value` paths leave `self.suspend` set and the frames live, so the discard-pop is
    /// skipped (there is no result yet) and the scheduler resumes the fiber later.
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
                self.invoke_value(callee, args, span)?;
                Ok(())
            }
            PendingCall::Method {
                recv,
                name,
                args,
                span,
            } => {
                let argc = args.len();
                self.push(recv);
                for a in args {
                    self.push(a);
                }
                // Receiver + args are already on the operand stack, i.e. rooted — sample here.
                if self.heap.mem_cap() != 0 {
                    self.sample_mem_cap(span)?;
                }
                self.do_method_call(&name, argc, NO_IC, span)?;
                if !self.paused() {
                    self.pop(); // discard the completed task's result (none pending if paused/yielded)
                }
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
    /// [`from_wire_piece`](Vm::from_wire_piece) instead.
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
        let mut memo = WireMemo {
            base_cells,
            next_id: seed_ceiling,
            ..WireMemo::default()
        };
        let mut ws = Vec::with_capacity(vs.len());
        for v in vs {
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
        let cell_ids: super::CellIds = if self.snapshot_cells.is_empty() {
            Vec::new()
        } else {
            rebuild
                .iter()
                .filter(|&(&id, &h)| id < seed_ceiling && matches!(self.heap.get(h), Obj::Cell(_)))
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
    /// W7-4: serializes with [`WireMemo::elem_split`], because a STORED wire is the one wire that gets
    /// drained PIECEWISE (`RwShared`'s zero-copy read views take one depth-1 element at a time, each
    /// with its own rebuild map). Each depth-1 subtree therefore carries its own full definition of
    /// every cell it reaches, and a whole-value rebuild dedupes them back to one cell — so
    /// `Channel.recv`/`Shared.get`/`RwShared.get` keep the shared binding while a per-element view is
    /// (as always) an independent copy. `elem_split` covers CELLS only; a piece back-referencing the
    /// ROOT container is handled on the rebuild side by [`from_wire_piece`](Vm::from_wire_piece)
    /// (W7-11).
    pub(super) fn to_wire_crossable(
        &self,
        v: Value,
        span: Span,
    ) -> Result<WireValue, RuntimeError> {
        let mut memo = WireMemo {
            elem_split: true,
            ..Default::default()
        };
        let w = self.to_wire_memo_at(v, span, &mut memo)?;
        self.ensure_crossable(&w, span)?;
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
        if self.heap.mem_cap() != 0 {
            self.heap.charge_bytes(crate::vm::core::wire_summary(&w).0);
        }
        Ok(w)
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
        // sibling closures over the same local arrive as two cells (W7-4). Within a memo: containers +
        // closures are back-edge-only (an off-path alias is deep-copied), cells are preserved
        // throughout (see [`WireMemo`]).
        let mut memo = WireMemo::default();
        self.to_wire_depth(v, 0, &mut memo)
    }

    /// Depth-counted worker behind [`Vm::to_wire`]. A **self-referential** sendable (e.g. a struct
    /// whose field points back at itself) now ROUND-TRIPS via the identity-preserving id + `Backref`
    /// machinery (every container arm + `Cell`/`Closure`), so it never recurses unbounded. This depth
    /// bound remains as the backstop for a genuinely-unbounded ACYCLIC nest (which would otherwise
    /// recurse on the HOST stack and `SIGABRT`, uncatchable) — the SAME `MAX_STRUCTURAL_DEPTH` limit
    /// and message the display / `==` paths use (`stmt.rs` / `arith.rs`), turned into a recoverable
    /// `RuntimeError` (placeholder `Span{0,0}`, re-stamped with the real airlock site by `to_wire_at`).
    /// Kept in lockstep with [`Vm::to_snap`] (which shares this budget on its fast path) so the serial
    /// and M:N engines trip at the identical depth.
    fn to_wire_depth(
        &self,
        v: Value,
        depth: usize,
        memo: &mut WireMemo,
    ) -> Result<WireValue, RuntimeError> {
        if depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.depth_exceeded_err(Span::default()));
        }
        // W7-4: a cross-heap STORE re-emits each cell's full definition once per depth-1 subtree, so
        // every piece an `RwShared` read view drains alone is self-contained (see [`WireMemo`]).
        if memo.elem_split && depth == 1 {
            memo.elem_gen += 1;
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
                // `id` and record it in `memo.path` BEFORE recursing captures; a nested revisit of `h`
                // (the back-edge) emits `WireValue::Backref(id)` and stops — `from_wire` ties the knot
                // back. `h` is removed from `path` on exit, so an off-path alias is deep-copied.
                Obj::Closure {
                    proto,
                    captured,
                    home,
                } => {
                    if let Some(&id) = memo.path.get(&h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.next_id;
                        memo.next_id += 1;
                        memo.path.insert(h, id);
                        let names = &self.program.protos[*proto].capture_names;
                        let mut wcap = Vec::with_capacity(captured.len());
                        for (i, cv) in captured.iter().enumerate() {
                            let w = self.to_wire_depth(*cv, depth + 1, memo)?;
                            let name = names.get(i).cloned().unwrap_or_default();
                            wcap.push((name.into_boxed_str(), w));
                        }
                        memo.path.remove(&h);
                        WireValue::Closure {
                            id,
                            proto: *proto,
                            captured: wcap,
                            home: self.home_index(*home),
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
                // `id` and record it in `memo.path` BEFORE recursing, so a self-referential list
                // (`xs.push(xs)`) or any cycle passing through it back-references instead of overflowing
                // the depth cap. Removed from `path` on DFS exit, so an off-stack alias is deep-copied.
                Obj::List(items) => {
                    if let Some(&id) = memo.path.get(&h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.next_id;
                        memo.next_id += 1;
                        memo.path.insert(h, id);
                        let mut out = Vec::with_capacity(items.len());
                        for x in items {
                            out.push(self.to_wire_depth(*x, depth + 1, memo)?);
                        }
                        memo.path.remove(&h);
                        WireValue::List { id, items: out }
                    }
                }
                Obj::Tuple(items) => {
                    if let Some(&id) = memo.path.get(&h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.next_id;
                        memo.next_id += 1;
                        memo.path.insert(h, id);
                        let mut out = Vec::with_capacity(items.len());
                        for x in items {
                            out.push(self.to_wire_depth(*x, depth + 1, memo)?);
                        }
                        memo.path.remove(&h);
                        WireValue::Tuple { id, items: out }
                    }
                }
                Obj::Map(m) => {
                    if let Some(&id) = memo.path.get(&h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.next_id;
                        memo.next_id += 1;
                        memo.path.insert(h, id);
                        let mut out = Vec::with_capacity(m.entries.len());
                        for (hash, k, val) in &m.entries {
                            out.push((
                                *hash,
                                self.to_wire_depth(*k, depth + 1, memo)?,
                                self.to_wire_depth(*val, depth + 1, memo)?,
                            ));
                        }
                        memo.path.remove(&h);
                        WireValue::Map { id, entries: out }
                    }
                }
                Obj::Set(s) => {
                    if let Some(&id) = memo.path.get(&h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.next_id;
                        memo.next_id += 1;
                        memo.path.insert(h, id);
                        let mut out = Vec::with_capacity(s.entries.len());
                        for (hash, e) in &s.entries {
                            out.push((*hash, self.to_wire_depth(*e, depth + 1, memo)?));
                        }
                        memo.path.remove(&h);
                        WireValue::Set { id, entries: out }
                    }
                }
                // Identity-preserved container (see `Obj::List`): a self-referential struct
                // (`a.next = b; b.next = a`) back-references instead of overflowing the cap.
                Obj::Struct { tid, fields, .. } => {
                    if let Some(&id) = memo.path.get(&h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.next_id;
                        memo.next_id += 1;
                        memo.path.insert(h, id);
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
                        memo.path.remove(&h);
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
                    if let Some(&id) = memo.path.get(&h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.next_id;
                        memo.next_id += 1;
                        memo.path.insert(h, id);
                        let mut out = Vec::with_capacity(payload.len());
                        for x in payload {
                            out.push(self.to_wire_depth(*x, depth + 1, memo)?);
                        }
                        memo.path.remove(&h);
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
                // A newtype crosses by value (deep copy), like a 1-field struct: carry its key + the
                // wired inner. Sendable iff its inner is (the checker's `sendable_rec` agrees).
                Obj::NewType { type_key, inner } => {
                    if let Some(&id) = memo.path.get(&h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.next_id;
                        memo.next_id += 1;
                        memo.path.insert(h, id);
                        let winner = self.to_wire_depth(*inner, depth + 1, memo)?;
                        memo.path.remove(&h);
                        WireValue::NewType {
                            id,
                            type_key: type_key.clone(),
                            inner: Box::new(winner),
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
                    // The SOLE remaining non-identity-preserved container: a generator's parked frame
                    // holds no `WireValue` id, so it can't be a `Backref` target. With the containers now
                    // identity-preserved, a cycle re-entering THIS generator no longer trips the depth cap
                    // (a container back-edge cuts the recursion first) — so re-serializing it would
                    // silently DUPLICATE the generator (two independent copies sharing one container), the
                    // e8dcad7 wrong-result class. Guard it directly: if `h` is already on the DFS stack we
                    // are closing a value cycle through a non-preservable node → reject cleanly (both
                    // engines run this identical path → byte-identical fault). Insert BEFORE recursing;
                    // remove on exit, so a generator revisited OFF the stack (an acyclic DAG alias) is
                    // deep-copied independently, like the containers. A recursive closure PARKED in the
                    // generator is still fine: its self-cell cycle is identity-preserved and back-refs.
                    if memo.gens_on_stack.contains(&h) {
                        return Err(self.err(
                            "a generator cannot be sent across tasks as part of a reference cycle"
                                .to_string(),
                            Span::default(),
                        ));
                    }
                    memo.gens_on_stack.insert(h);
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
                            WireGenState::Pending(wargs)
                        }
                        GenState::Done => WireGenState::Done,
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
                                frame: WireCallFrame {
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
                                },
                                stack: wstack,
                                call_depth: g.ctx.call_depth,
                                cur_base: g.ctx.cur_base,
                                handlers: g.ctx.handlers.clone(),
                            }
                        }
                    };
                    // Off the DFS stack: a later off-stack revisit is an acyclic alias → deep-copied.
                    memo.gens_on_stack.remove(&h);
                    WireValue::Generator {
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
                // Data containers keep the pop-on-exit `path` discipline above, so the documented
                // DAG-alias-is-two-independent-copies contract is untouched. The `id` is stable per
                // cell for the whole serialization; `emitted` (which `elem_split` scopes per depth-1
                // subtree — see [`WireMemo`]) decides definition-vs-`Backref`, and a repeated
                // definition DEDUPES on rebuild, so identity is unchanged either way.
                Obj::Cell(v) => {
                    let id = match memo.cell_id(h) {
                        Some(id) => id,
                        None => {
                            let id = memo.next_id;
                            memo.next_id += 1;
                            memo.cells.insert(h, id);
                            id
                        }
                    };
                    if memo.emitted.get(&id) == Some(&memo.elem_gen) {
                        WireValue::Backref(id)
                    } else {
                        // W7-4a — journal the entry we are about to overwrite so a DISCARDED
                        // speculative attempt restores it exactly (see `try_wire_speculative`).
                        if memo.speculating {
                            let prev = memo.emitted.get(&id).copied();
                            memo.emit_undo.push((id, prev));
                        }
                        memo.emitted.insert(id, memo.elem_gen);
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
                    if let Some(&id) = memo.path.get(&h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.next_id;
                        memo.next_id += 1;
                        memo.path.insert(h, id);
                        let pos = *pos;
                        let items = items.clone();
                        let mut out = Vec::with_capacity(items.len());
                        for x in items {
                            out.push(self.to_wire_depth(x, depth + 1, memo)?);
                        }
                        memo.path.remove(&h);
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

    /// B3.0 — reconstruct a [`WireValue`] into a heap [`Value`] (the airlock's inbound half). Data
    /// arms `alloc` fresh objects into *this* `Vm`'s heap (mirroring the old deep_clone allocation);
    /// [`WireValue::Handle`] returns the same handle (by-reference preserved). `Map`/`Set` rebuild
    /// via `push(hash, …)` with the carried hash, so iteration order + index are identical. Every
    /// identity-preserved arm (`Cell`/`Closure` AND every container) ties the knot (placeholder-alloc →
    /// register id → recurse → patch), so a self-referential value round-trips; `alloc` never collects,
    /// so no intermediate is lost mid-reconstruction.
    // `&mut self` is intentional: `from_wire` reconstructs *into* this VM's heap (it allocates),
    // so it is not the usual ownership-less `from_*` constructor the lint expects.
    #[allow(clippy::wrong_self_convention)]
    pub(super) fn from_wire(&mut self, w: WireValue) -> Value {
        // Fresh rebuild memo per root: it maps each identity-preserved wire `id` (a `Cell`/`Closure` or
        // a container) to the heap `GcRef` of its placeholder, so a `WireValue::Backref(id)` nested
        // inside resolves to the already-alloc'd (and about-to-be-patched) node — tying a serialized
        // value cycle back together.
        let mut rebuild = super::fxhash::FxHashMap::<u32, GcRef>::default();
        let v = self.from_wire_memo(w, &mut rebuild);
        // W7-11 — every caller of `from_wire` rebuilds a WHOLE crossing, so its rebuild map spans the
        // same scope the serialize memo did and a `Backref` can never dangle. A piecewise drain goes
        // through [`from_wire_piece`](Vm::from_wire_piece) instead. Keep the invariant loud where it is
        // genuinely a bug; in release a dangling ref degrades to `nil` rather than aborting the host.
        debug_assert!(
            !self.wire_backref_missing,
            "from_wire: a whole-value rebuild hit a dangling Backref — memo scope != rebuild scope \
             (a piecewise drain must call from_wire_piece)"
        );
        v
    }

    /// W7-11 — rebuild ONE depth-1 piece of a stored wire (`RwShared`'s copy-out read views).
    ///
    /// A piece is normally self-contained: [`to_wire_crossable`](Vm::to_wire_crossable) serializes
    /// stores with [`WireMemo::elem_split`], which re-emits every `Obj::Cell` definition the piece
    /// reaches. But `elem_split` only covers CELLS — a piece whose cycle closes through the ROOT
    /// container (`a.next = xs; RwShared(xs).at(0)`) carries a `Backref` to the container itself, which
    /// the piece by definition does not contain. That used to abort the host.
    ///
    /// When the piece cannot stand alone, this rebuilds the WHOLE `root` **into the caller's map** and
    /// returns the piece out of it by its wire id — the node it wanted is now defined and the cycle is
    /// tied. That is CPython's answer, measured: `copy.deepcopy(xs[0])` on the same shape follows the
    /// cycle and copies the container too (`b.next[0] is b` → `True`); `pickle` agrees across a process
    /// boundary.
    ///
    /// **The check is a PRE-check, and that is load-bearing** — attempting the rebuild and reacting to
    /// the miss afterwards is wrong, and shipped once. A half-finished attempt has already written its
    /// partial nodes into `rebuild`, including an `Obj::Cell` still holding the inert placeholder;
    /// `slice`, which deliberately shares ONE map across its elements, then served the NEXT element out
    /// of that poisoned cell via the `Cell` first-wins dedupe and handed user code a `nil` — a WRONG
    /// VALUE where the bug being fixed had only crashed. Nothing may be allocated until the piece is
    /// known to be rebuildable.
    ///
    /// Rebuilding the whole container into the CALLER's map (not a private one) is what keeps `slice`'s
    /// "one call, one container, shares within itself" contract on cyclic data: every later piece finds
    /// its id already there and is served from the same container.
    ///
    /// `root` is borrowed from the CALLER'S read guard on purpose. Re-acquiring `core.v` here would be
    /// a SECOND guard, and the window between the two let a concurrent `set` swap in an unrelated
    /// serialization whose ids mean different nodes — the torn read that sank the first attempt at this
    /// (`docs/gaps.md` W7-4 round 2: `.expect` abort or a wrong-node `CellLoad` abort, M:N-only, so
    /// parity-blind). Holding one guard across the rebuild is safe because `Heap::alloc` never
    /// collects, so no GC (which would re-lock `core.v` to mark `Obj::RwShared`) can run underneath.
    ///
    /// ponytail: the ceiling is O(root) per view call ON CYCLIC DATA ONLY. A non-cyclic piece pays one
    /// extra non-allocating walk (`backrefs_resolvable`), which is what keeps
    /// `rwshared_view_over_shared_bindings_is_not_quadratic` green.
    ///
    /// State that ceiling precisely, because an earlier draft of this comment overclaimed and review
    /// caught it. For a SINGLE piece (`at`, `get_key`) the cost is CPython's: `copy.deepcopy` of one
    /// cyclic element copies the container too. For a WHOLE-CONTAINER WALK it is not — `for_each`/
    /// `fold` over a container where many elements back-reference the root rebuild it once per element,
    /// so they are O(n²) where CPython's `for x in deepcopy(xs)` is O(n) (measured: n = 500 / 1000 /
    /// 2000 → 0.068 / 0.28 / 1.17 s). `slice` is exempt — it decides once per call and shares one map.
    /// Upgrade path when that matters: memoize the whole rebuild per (core, store generation) across
    /// one walk, which is exactly what `slice` now does by hand.
    #[allow(clippy::wrong_self_convention)]
    pub(super) fn from_wire_piece(
        &mut self,
        root: &WireValue,
        piece: WireValue,
        rebuild: &mut super::fxhash::FxHashMap<u32, GcRef>,
    ) -> Value {
        let id = piece.node_id();
        // Already materialized — by an earlier piece of THIS call that took the fallback below and
        // rebuilt the whole container. Only reachable through that path: distinct depth-1 elements
        // carry distinct ids (`to_wire` pops `path` on DFS exit, so an off-stack alias is re-serialized
        // with a fresh id), so a piece's own id is never in the map for any other reason.
        if let Some(&h) = id.and_then(|i| rebuild.get(&i)) {
            return Value::obj(h);
        }
        if piece.backrefs_resolvable(rebuild) {
            let v = self.from_wire_memo(piece, rebuild);
            debug_assert!(
                !self.wire_backref_missing,
                "from_wire_piece: backrefs_resolvable said yes and the rebuild disagreed — the \
                 pre-check has drifted from from_wire_memo's arms"
            );
            return v;
        }
        self.wire_backref_missing = false;
        let _root_v = self.from_wire_memo(root.clone(), rebuild);
        debug_assert!(
            !self.wire_backref_missing,
            "from_wire_piece: the WHOLE stored wire has a dangling Backref — the store side is broken"
        );
        self.wire_backref_missing = false; // never leave it set for the next caller's assert
        match id.and_then(|i| rebuild.get(&i)) {
            Some(&h) => Value::obj(h),
            // An id-LESS piece — only a `Generator`, which carries no wire id because its parked frame
            // can never be a `Backref` TARGET. It still had to wait for the whole rebuild, because its
            // closure/parked slots can back-reference the container. Rebuilding it now resolves against
            // the complete map; it is a fresh node, which is right — it has no identity to preserve.
            None => self.from_wire_memo(piece, rebuild),
        }
    }

    /// Rebuild a wire item list into heap `Value`s **at exact capacity**. `pick` pulls the
    /// [`WireValue`] out of each element, so a keyed list (`Vec<(Box<str>, WireValue)>` — struct
    /// fields, closure captures) shares the one path with a plain `Vec<WireValue>`.
    ///
    /// NOT `items.into_iter().map(…).collect()`, which is the shape this replaced. Rust specializes
    /// that into an **in-place** collect when the destination element is no larger than the source's
    /// AND their alignments are equal (`align_of::<SRC>() != align_of::<DEST>()` is what makes std
    /// bail): the source `Vec`'s allocation is reused and the result inherits its capacity, scaled by
    /// the size ratio. Both conditions hold at every airlock rebuild — everything here maps into
    /// `Value` (8 B, align 8) from an align-8 source — so the inflation was **22×** from a plain
    /// `Vec<WireValue>` (176 B) and **24×** from a keyed `Vec<(Box<str>, WireValue)>` (192 B).
    ///
    /// Every container crossing the airlock (`spawn` args, `Channel.recv`, `Shared.get`, closure
    /// captures) therefore kept the whole wire buffer alive for the rebuilt object's entire lifetime:
    /// a 200 000-int `List` measured `capacity = 4 400 000` — a **35.2 MB** `Obj::List` holding
    /// 1.6 MB of data — and 50 such spawns peaked at 3.45 GB (203 MB after). It is invisible to
    /// `len()` and to every value-level test; only `Vec::capacity` shows it.
    ///
    /// **Use this at EVERY wire→`Value` list rebuild, not just the ones in `from_wire_memo`.** The
    /// first cut of this fix converted that function's eight container arms and left the identical
    /// shape in `deep_clone_all` and in `rebuild_ready`'s five `Lowered` arms — two of which
    /// (`deep_clone_all`'s result and the `Lowered::Closure` captures) land in a durable
    /// `Obj::Closure { captured }`, so `spawn` with a capturing closure still leaked 22–24×, under a
    /// doc comment claiming captures were fixed. Adversarial review caught it; the suite did not.
    ///
    /// Pre-sizing and pushing keeps the exact-capacity guarantee at the cost of one extra live
    /// buffer while the source drains, which the wire copy was already paying.
    fn rebuild_items<T>(
        &mut self,
        items: Vec<T>,
        rebuild: &mut super::fxhash::FxHashMap<u32, GcRef>,
        pick: impl Fn(T) -> WireValue,
    ) -> Vec<Value> {
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            let v = self.from_wire_memo(pick(it), rebuild);
            out.push(v);
        }
        out
    }

    /// Worker behind [`Vm::from_wire`] — reconstructs into this heap, threading `rebuild` (wire `id` →
    /// placeholder `GcRef`) so any value cycle (a recursive local `fn`, a self-referential struct/list/
    /// map, a mixed struct+closure cycle) round-trips. Every identity-preserved arm (`Cell`/`Closure`
    /// AND every container) TIES THE KNOT: alloc a placeholder object FIRST, register its `id` in
    /// `rebuild`, recurse the children (a nested `Backref(id)` resolves to the placeholder), then
    /// `heap.get_mut`-patch the placeholder with the reconstructed children. Memory-safe: `Heap::alloc`
    /// never collects (heap.rs), so no GC fires between placeholder-alloc and the patch, and `GcRef` is
    /// a GC-traced index (never a raw pointer) — the placeholder can never dangle or alias.
    #[allow(clippy::wrong_self_convention)]
    pub(super) fn from_wire_memo(
        &mut self,
        w: WireValue,
        rebuild: &mut super::fxhash::FxHashMap<u32, GcRef>,
    ) -> Value {
        match w {
            // Re-create on the DESTINATION heap: `make_int` re-inlines or re-boxes (wide) and
            // `box_float` re-boxes, so the airlock round-trip is representation-stable.
            WireValue::Int(n) => self.make_int(n),
            WireValue::Float(f) => self.box_float(f),
            WireValue::Bool(b) => Value::bool(b),
            WireValue::Nil => Value::nil(),
            // B3.3a: rebuild a fresh heap `str` from the owned bytes (by value, not the old handle).
            WireValue::Str(s) => Value::obj(self.heap.alloc(Obj::Str(s.into()))),
            // Rebuild a fresh heap `bytes` from the owned raw bytes (by value, like `str`).
            WireValue::Bytes(b) => Value::obj(self.heap.alloc(Obj::Bytes(b))),
            // Rebuild a FRESH, independent heap `bytearray` from the owned raw bytes (deep copy across
            // the airlock, like `list`) — the other side never shares this VM's buffer.
            WireValue::ByteArray(b) => Value::obj(self.heap.alloc(Obj::ByteArray(b.into_vec()))),
            WireValue::Handle(h) => Value::obj(h),
            // B3.1: rebuild a fresh heap handle onto the SAME shared core (`Arc` already cloned in
            // `to_wire`). Not registered in `self.executors` — the original `NewExecutor` handle there
            // drives the program-exit auto-drain and shares this core, so the alias needs no entry.
            WireValue::Channel(core) => Value::obj(self.heap.alloc(Obj::Channel(core))),
            WireValue::Shared(core) => Value::obj(self.heap.alloc(Obj::Shared(core))),
            WireValue::RwShared(core) => Value::obj(self.heap.alloc(Obj::RwShared(core))),
            WireValue::Atomic(core) => Value::obj(self.heap.alloc(Obj::Atomic(core))),
            WireValue::AtomicInt(core) => Value::obj(self.heap.alloc(Obj::AtomicInt(core))),
            WireValue::Executor(core) => Value::obj(self.heap.alloc(Obj::Executor(core))),
            // D6: rebuild a fresh heap handle onto the SAME shared socket/listener core (`Arc` cloned
            // in `to_wire`) — two fibers reach one fd.
            WireValue::Socket(core) => Value::obj(self.heap.alloc(Obj::Socket(core))),
            WireValue::Listener(core) => Value::obj(self.heap.alloc(Obj::Listener(core))),
            // R2: rebuild a fresh heap handle onto the SAME shared writer core (`Arc` cloned in
            // `to_wire`) — two fibers reach one output.
            WireValue::Writer(core) => Value::obj(self.heap.alloc(Obj::Writer(core))),
            // R2b: rebuild a fresh heap handle onto the SAME shared reader core (`Arc` cloned in
            // `to_wire`) — two fibers reach one fd.
            WireValue::Reader(core) => Value::obj(self.heap.alloc(Obj::Reader(core))),
            // Rebuild a fresh `Obj::Ptr` from the raw address carried by value (heap-independent).
            WireValue::Ptr(a) => Value::obj(self.heap.alloc(Obj::Ptr(a))),
            // Re-alloc a fresh `Obj::Builtin` from the name carried by value (pure code, no state).
            WireValue::Builtin(name) => Value::obj(self.heap.alloc(Obj::Builtin(name))),
            // Re-alloc a fresh `Obj::Native` from the name + fn pointer carried by value (pure code) —
            // same as the `SnapValue::Native` rebuild.
            WireValue::Native { name, func, kind } => {
                Value::obj(self.heap.alloc(Obj::Native { name, func, kind }))
            }
            // Re-alloc a fresh `Obj::Cffi` sharing the SAME `Arc<Cffi>` (no re-dlopen) — same as the
            // `SnapValue::Cffi` rebuild.
            WireValue::Cffi(c) => Value::obj(self.heap.alloc(Obj::Cffi(c))),
            // TIE THE KNOT (see the `Cell`/`Closure` arms): alloc an empty placeholder container
            // FIRST, register its `id` in `rebuild` BEFORE recursing children, so a nested
            // `Backref(id)` (a self-referential list/struct/map) resolves to this exact handle; then
            // `heap.get_mut`-patch the placeholder with the reconstructed children. `Heap::alloc` never
            // collects, so no GC runs between the placeholder and the patch. (Nothing READS the
            // placeholder's contents mid-reconstruction — only its handle identity is observed.)
            WireValue::List { id, items } => {
                let h = self.heap.alloc(Obj::List(Vec::new()));
                rebuild.insert(id, h);
                let cloned = self.rebuild_items(items, rebuild, |x| x);
                *self.heap.get_mut(h) = Obj::List(cloned);
                Value::obj(h)
            }
            WireValue::Tuple { id, items } => {
                let h = self.heap.alloc(Obj::Tuple(Vec::new()));
                rebuild.insert(id, h);
                let cloned = self.rebuild_items(items, rebuild, |x| x);
                *self.heap.get_mut(h) = Obj::Tuple(cloned);
                Value::obj(h)
            }
            WireValue::Iter { id, items, pos } => {
                let h = self.heap.alloc(Obj::Iter {
                    items: Vec::new(),
                    pos,
                });
                rebuild.insert(id, h);
                let cloned = self.rebuild_items(items, rebuild, |x| x);
                match self.heap.get_mut(h) {
                    Obj::Iter { items, .. } => *items = cloned,
                    _ => unreachable!("placeholder was alloc'd as Obj::Iter"),
                }
                Value::obj(h)
            }
            WireValue::Map { id, entries } => {
                let h = self.heap.alloc(Obj::Map(MapData::default()));
                rebuild.insert(id, h);
                // Reconstruction reuses the CARRIED hash (`push(hash, …)`) — never re-hashes a
                // (possibly cyclic) key, and keeps iteration order + index byte-identical.
                let mut out = MapData::default();
                for (hash, k, val) in entries {
                    let ck = self.from_wire_memo(k, rebuild);
                    let cv = self.from_wire_memo(val, rebuild);
                    out.push(hash, ck, cv);
                }
                *self.heap.get_mut(h) = Obj::Map(out);
                Value::obj(h)
            }
            WireValue::Set { id, entries } => {
                let h = self.heap.alloc(Obj::Set(SetData::default()));
                rebuild.insert(id, h);
                let mut out = SetData::default();
                for (hash, e) in entries {
                    let ce = self.from_wire_memo(e, rebuild);
                    out.push(hash, ce);
                }
                *self.heap.get_mut(h) = Obj::Set(out);
                Value::obj(h)
            }
            WireValue::Struct { id, name, fields } => {
                // Positional layout: the wire fields arrive in declaration order (to_wire emits
                // them so), so rebuild positionally — the carried names are discarded.
                let tid = self.struct_tid(&name);
                let h = self.heap.alloc(Obj::Struct {
                    tid,
                    fields: Fields::from_vec(Vec::new()),
                });
                rebuild.insert(id, h);
                let cloned = self.rebuild_items(fields, rebuild, |(_, val)| val);
                match self.heap.get_mut(h) {
                    Obj::Struct { fields, .. } => *fields = Fields::from_vec(cloned),
                    _ => unreachable!("placeholder was alloc'd as Obj::Struct"),
                }
                Value::obj(h)
            }
            WireValue::Enum {
                id,
                variant_id,
                payload,
            } => {
                let h = self.heap.alloc(Obj::Enum {
                    variant_id,
                    payload: Vec::new(),
                });
                rebuild.insert(id, h);
                let cloned = self.rebuild_items(payload, rebuild, |x| x);
                // M19 lever #2 — the dense `variant_id` crossed the airlock directly (shared
                // `Arc<Program>`), so it is replayed as-is — no lossy name re-resolution.
                match self.heap.get_mut(h) {
                    Obj::Enum { payload, .. } => *payload = cloned,
                    _ => unreachable!("placeholder was alloc'd as Obj::Enum"),
                }
                Value::obj(h)
            }
            WireValue::NewType {
                id,
                type_key,
                inner,
            } => {
                let h = self.heap.alloc(Obj::NewType {
                    type_key,
                    inner: Value::nil(),
                });
                rebuild.insert(id, h);
                let inner_v = self.from_wire_memo(*inner, rebuild);
                match self.heap.get_mut(h) {
                    Obj::NewType { inner, .. } => *inner = inner_v,
                    _ => unreachable!("placeholder was alloc'd as Obj::NewType"),
                }
                Value::obj(h)
            }
            // A back-reference closes a serialized value cycle (or, for a `Cell`, ties a second
            // reference to the one binding — W7-4): resolve it to the placeholder registered under this
            // `id`. Present whenever the rebuild map spans the SAME scope the serialize memo did —
            // `to_wire` emits the defining node before any `Backref` to it, and `from_wire_memo`
            // registers the placeholder BEFORE recursing children. Keeping those two scopes equal is
            // the caller's job (see [`deep_clone_all`](Vm::deep_clone_all)); a wire drained PIECEWISE
            // (`RwShared`'s read views) is served by [`to_wire_crossable`](Vm::to_wire_crossable)'s
            // `elem_split`, which makes every depth-1 piece self-contained — EXCEPT for a piece whose
            // cycle closes through the ROOT container, which no per-piece re-emission can make
            // self-contained (the node it needs IS the container).
            //
            // W7-11 — that last case used to `.expect` here and ABORT THE HOST on a legal program
            // (`a.next = xs; RwShared(xs).at(0)`). It now flags the miss and hands back an inert
            // placeholder; [`from_wire_piece`](Vm::from_wire_piece) sees the flag and re-does the
            // rebuild over the WHOLE container, which defines the id. The placeholder is never
            // observable: the only caller that can legitimately trip the flag discards this whole
            // result, and every other caller is `debug_assert`ed in [`from_wire`](Vm::from_wire).
            WireValue::Backref(id) => match rebuild.get(&id) {
                Some(&h) => Value::obj(h),
                None => {
                    self.wire_backref_missing = true;
                    Value::nil()
                }
            },
            // Rebuild a FRESH, independent `Obj::Cell` on this side (deep copy, never a shared box) —
            // the receiving task owns its own cell (design §4 F1). TIE THE KNOT: alloc a placeholder
            // `Cell(Nil)` and register its `id` BEFORE recursing `inner`, so a nested `Backref(id)`
            // (the self-cell a recursive local `fn` closes) resolves to this exact handle; then patch
            // the placeholder with the reconstructed inner. `Heap::alloc` never collects, so no GC runs
            // between the placeholder and the patch.
            // W7-4: a wire may carry the SAME cell definition more than once (`elem_split` re-emits it
            // per depth-1 subtree so each is self-contained) — the first rebuild wins and every later
            // definition of that id resolves to it, exactly like a `Backref`. That is what keeps
            // `Channel.recv`/`Shared.get`/`RwShared.get` on ONE cell per binding.
            WireValue::Cell { id, inner } => {
                if let Some(&prev) = rebuild.get(&id) {
                    return Value::obj(prev);
                }
                let h = self.heap.alloc(Obj::Cell(Value::nil()));
                rebuild.insert(id, h);
                let inner = self.from_wire_memo(*inner, rebuild);
                *self.heap.get_mut(h) = Obj::Cell(inner);
                Value::obj(h)
            }
            // B3.6: rebuild a submitted closure by value over the worker's reconstructed home module
            // (the `proto` is shared via `Arc<Program>`; captures reconstruct bottom-up into this heap).
            // `worker_home` resolves the home index against this VM's `module_objs` (the rebuilt graph
            // in a pool worker, or the live graph directly when running with no heap of its own).
            // TIE THE KNOT like
            // the `Cell` arm: resolve `home` and alloc a placeholder `Closure` with `Nil` captures,
            // register its `id`, THEN recurse the captures (a nested `Backref(id)` resolves to this
            // handle), and patch the captured slots. `worker_home`/`from_wire_memo` may `alloc` but never
            // collect, so the placeholder cannot be lost mid-reconstruction.
            WireValue::Closure {
                id,
                proto,
                captured,
                home,
            } => {
                let home = self.worker_home(home);
                let n = captured.len();
                let h = self.heap.alloc(Obj::Closure {
                    proto,
                    captured: vec![Value::nil(); n],
                    home,
                });
                rebuild.insert(id, h);
                // Lever #3: rebuild positionally — push values in wire (slot) order, discard the
                // carried names (they live in `proto.capture_names`). `to_wire` emits in slot order.
                let cap = self.rebuild_items(captured, rebuild, |(_k, w)| w);
                match self.heap.get_mut(h) {
                    Obj::Closure { captured, .. } => *captured = cap,
                    _ => unreachable!("placeholder was alloc'd as Obj::Closure"),
                }
                Value::obj(h)
            }
            // B3.3: rebuild a bare fn by value over the worker's reconstructed home module (the `proto`
            // is shared via `Arc<Program>`; `worker_home` resolves the home index like the Closure arm).
            WireValue::Func { proto, home } => {
                let home = self.worker_home(home);
                Value::obj(self.heap.alloc(Obj::Func { proto, home }))
            }
            // F3 path C: rebuild a FRESH, independent `GeneratorCore` on this heap (deep-copy
            // independence, like `Cell`/`Iter`). `worker_home` resolves the home index; the backing
            // closure + parked slots reconstruct bottom-up into this heap. `Pending` reuses
            // `alloc_generator`; `Done`/`Suspended` build the core directly (this method is in-module,
            // so it can construct the private `CallFrame`/`GenCtx`/`GeneratorCore`). The rebuilt frame's
            // `home`/`closure` reuse the core's rebuilt `GcRef`s (they were equal at serialize time).
            WireValue::Generator {
                proto,
                home,
                closure,
                state,
            } => {
                let home = self.worker_home(home);
                let closure = closure.map(|c| {
                    self.from_wire_memo(*c, rebuild)
                        .as_obj()
                        .expect("a generator's backing closure wire rebuilds to a heap object")
                });
                match state {
                    WireGenState::Pending(wargs) => {
                        let args = self.rebuild_items(wargs, rebuild, |w| w);
                        self.alloc_generator(proto, home, closure, args)
                    }
                    WireGenState::Done => {
                        let core = GeneratorCore {
                            proto,
                            home,
                            closure,
                            state: GenState::Done,
                            ctx: GenCtx::default(),
                        };
                        Value::obj(self.heap.alloc(Obj::Generator(Box::new(core))))
                    }
                    WireGenState::Suspended {
                        frame,
                        stack,
                        call_depth,
                        cur_base,
                        handlers,
                    } => {
                        let stack = self.rebuild_items(stack, rebuild, |w| w);
                        let rebuilt = CallFrame {
                            proto: frame.proto,
                            ip: frame.ip,
                            base: frame.base,
                            home,
                            closure,
                            counted: frame.counted,
                            is_toplevel: frame.is_toplevel,
                            deferred: Vec::new(),
                            defer_markers: frame.defer_markers,
                            nursery_len: frame.nursery_len,
                            has_implicit_nursery: frame.has_implicit_nursery,
                            call_span: frame.call_span,
                            argc: frame.argc,
                        };
                        // Backlog arm (b): the live `recover:` handlers cross as plain-data (`Copy`,
                        // `GcRef`-free), so they need no reconstruction — their `usize` indices
                        // address the frame/stack rebuilt just above and stay valid on this heap.
                        // `generator_next` rebases each handler's `nursery_len` to the resuming
                        // driver's floor, so the (now stale, cross-heap) sender value is inert.
                        let ctx = GenCtx {
                            frames: vec![rebuilt],
                            stack,
                            call_depth,
                            cur_base,
                            handlers,
                        };
                        let core = GeneratorCore {
                            proto,
                            home,
                            closure,
                            state: GenState::Suspended,
                            ctx,
                        };
                        Value::obj(self.heap.alloc(Obj::Generator(Box::new(core))))
                    }
                }
            }
        }
    }

    /// B3.2 — construct a fresh worker `Vm` that shares this VM's compiled program by `Arc`
    /// (read-only) but owns its own empty heap. Execution-shaping flags (`gc_stress`) carry over so a
    /// worker is exercised under the same GC pressure as the parent; `host` is left inert (B3.2's
    /// isolation tasks don't touch host I/O — B3.3 threads it through when real workers run user I/O).
    /// No OS thread yet; the caller drives the returned worker synchronously.
    pub(super) fn spawn_worker(&self) -> Vm {
        let mut worker = Vm::new(Arc::clone(&self.program));
        worker.gc_stress = self.gc_stress;
        // `chezzi test --max-heap` — thread the per-test live-heap cap onto the worker's OWN heap, so a
        // RUNAWAY alloc in a `spawn`/`parallel:` task trips the guard on the M:N engine too (a fresh
        // `Vm::new` heap defaults to cap-off). This is what gives the cap its cross-engine guarantee: a
        // real runaway trips on whichever heap runs it. It does NOT make the trip point precisely
        // predictable for a *near-boundary* concurrent test — each M:N worker's heap is measured alone
        // (not the whole run's aggregate), so a task's near-cap allocation trips independent of any
        // sibling's. That per-heap accounting is inherent and documented (`docs/future.md §3b`); a
        // cross-worker aggregate would need non-deterministic global RSS. `0` when the cap is off, so
        // the common path is untouched.
        worker.set_max_heap(self.heap.mem_cap());
        // …and something actually LOOKS at it, but NOT from here. A worker heap is BORN with its
        // task's payload already rebuilt into it, and `should_collect` counts OBJECTS: a `List[int]`
        // of any size rebuilds to ONE `Obj`, so a heap holding megabytes after ~7 allocations never
        // reaches `next_gc`, never sweeps, and `over_cap` — assigned only in `sweep()` — is never
        // evaluated. The guarantee the comment above states ("a real runaway trips on whichever heap
        // runs it") was therefore false even for the most ordinary shape there is.
        //
        // TWO DOORS run a worker built here, and each has its OWN sampling mechanism. Neither
        // subsumes the other; state which one you mean before claiming coverage.
        //
        //   1. THE FIBER DOOR — `spawn` / `parallel:`. `into_fiber` turns this worker's `ReadyCall`
        //      into a `PendingCall` and [`Vm::start_task`] dispatches it. Sampled THERE, by
        //      [`Vm::sample_mem_cap`], before dispatch. That placement is what covers a task body
        //      running NO bytecode (`spawn blob.len()` → `do_method_call` → `invoke_native`, no
        //      frame, no boundary) — a shape the flag below structurally cannot reach, because
        //      nothing ever consumes it. That was W6-10s residual (a).
        //
        //   2. THE JOB DOOR — eager `Executor` jobs. `prepare_worker_from_wire` →
        //      `dispatch_eager_job` → `ReadyWorker::run_outcome` → `ReadyWorker::invoke`, which does
        //      NOT route through `start_task` and so is NOT covered by the sample above. An
        //      `Executor` job body is always a closure, i.e. always bytecode, so it always reaches an
        //      instruction boundary and the `request_collect` flag below is always consumed — which
        //      is exactly the condition under which the flag works. It is the only forced sample this
        //      door has.
        //
        // What the flag uniquely covers on door 2: a job heap BORN over the cap in few objects and
        // few *shallow* bytes — an `Arc`-core capture (`Shared`/`Channel`/`Executor`), where
        // `obj_bytes_shallow` charges 0 by design and `since_gc` stays under `MIN_GC_THRESHOLD`
        // (256), but `live_bytes`'s deep walk would report over-cap if a sweep ever ran. No in-tree
        // witness exists for that class — the producer holds the same core and `live_bytes` charges a
        // core once per heap by reachability, so the producer trips first — but "no witness built" is
        // not "unreachable", and three lines is not a price worth arguing over.
        //
        // Here, not at a run site, because this is the one door every worker heap is born through
        // (both `ReadyWorker` constructors call it) and the flag survives `into_fiber`. Setting it
        // BEFORE the payload is rebuilt is fine: `Heap::alloc` never collects, so nothing can consume
        // it before the first instruction boundary, by which point the payload is fully installed.
        // Gated on a live cap, so an uncapped run forces no GC at all.
        //
        // WHAT NEITHER COVERS: growth AFTER the sample. `xs.push(i)` into an existing list allocates
        // no `Obj` and rode 77× past the cap until W7-28 charged it in `get_mut`. Bytes are paced,
        // not bounded — do not restate any of the above as an unconditional guarantee.
        if self.heap.mem_cap() != 0 {
            worker.heap.request_collect();
        }

        // `chezzi test --timeout` — thread the SAME absolute deadline onto the worker so a `spawn`/
        // `parallel:` task's loop trips the wall-clock cap on the M:N engine too (a fresh `Vm::new`
        // starts with `deadline = None`). `None` when the cap is off, so the common path is untouched.
        worker.set_deadline(self.deadline);
        // …and the raw `timeout_ms` too — the deadline drives the TRIP, but the abort MESSAGE
        // interpolates `timeout_ms`, so a worker left at the `Vm::new` default `0` would render a
        // spawned-task timeout as "(0ms)" (reads as "off"). Mirror `set_max_heap` above.
        worker.set_timeout(self.timeout_ms);
        // Workers run on the pool too, so a nested `parallel:` inside a task recurses onto threads
        // (and a worker's `recv` blocks on the condvar, not a fiber). B3.3-threads.
        // W7-5b — SHARE (not copy) the run's executor registry, so an `Executor` constructed inside a
        // task is still reachable by the program-exit join after this worker's heap is gone. This is
        // the whole fix: the parallel `Vm.executors` list is heap-keyed and dies with the task.
        worker.exec_registry = Arc::clone(&self.exec_registry);
        // …and the blocked-party registry with it, for the same reason: the process-wide deadlock
        // verdict must see every party of THIS run and none of any other's (`future.md` §2d step 0).
        worker.quiesce = Arc::clone(&self.quiesce);
        // W7-56 — and the sched registry, so a wake from a worker that holds NO sched of its own
        // (every eager `Executor` job: this fn sets neither `mn` nor `mn_enlist_sched`) still reaches
        // a fiber parked on one of the run's nurseries. See `Vm::sched_registry`.
        worker.sched_registry = Arc::clone(&self.sched_registry);
        // B3.3-threads: thread the parent's host state (process args + env) through so a
        // `--parallel` task reading `std.os.args` / an env var sees the same values instead of inert
        // defaults (the B3.2 silent-divergence owe). `args` is read-only (deep-cloned). `env` is
        // SHARED (its `Arc::clone` hands over the same `Mutex`-guarded map, not a copy) so a task's
        // `std.os.setenv` is visible to the parent + siblings — process-global env, matching
        // Python/Go. `stdin` is SHARED, not copied: `Stdin`'s clone
        // hands over the same source (an `Arc` queue / the process-global locked handle), so a task's
        // `read_line` reads the one stream — a line goes to exactly ONE task, and no task is ever
        // handed a false EOF (Go's `os.Stdin` / Python's `sys.stdin`; which task gets a given line is
        // nondeterministic BY DESIGN). `HostConfig` isn't `Clone`, so build it field-wise.
        worker.host = crate::native::HostConfig {
            args: self.host.args.clone(),
            env: self.host.env.clone(),
            stdin: self.host.stdin.clone(),
            // Streaming CLI: an M:N worker writes its task's output straight to the process stdout
            // as it prints (line-atomic), instead of buffering it until the nursery joins — which for
            // a server's nursery is never. In buffered (test/embedder) mode this is false and the
            // per-task buffer + task-order flush is untouched.
            stream: self.host.stream,
        };
        worker
    }

    /// B3.2 — run a spawned task in an isolated worker (`spawn_worker`): its args/captures cross IN as
    /// [`WireValue`] (serialized in *this* parent heap, reconstructed in the worker heap) and its
    /// return value + captured `out`/`stderr` cross back OUT. The worker runs **synchronously** on the
    /// calling thread (no OS thread until B3.3), proving the `Arc<Program>` + heap-handoff plumbing in
    /// isolation.
    ///
    /// The callee is **not** crossed as a parent-heap `Handle` (a `GcRef` is meaningless in another
    /// heap); instead the task is lowered to its `ProtoId` + wire'd captures (the proto lives in the
    /// shared `Arc<Program>`) and the worker rebuilds the closure over its own heap.
    ///
    /// **Cross-heap safety (enforced, not just documented).** A `WireValue` that still carries a
    /// by-reference [`Handle`](WireValue::has_handle) — a `str`, a closure/func value, a module — is a
    /// parent-heap `GcRef` that means nothing in the worker heap, so every crossed value (captures,
    /// args, and the returned result) is checked with [`Vm::ensure_crossable`] and a clean
    /// `RuntimeError` is raised rather than silently reconstructing a dangling handle. Plain data and
    /// `Channel`/`Shared`/`Executor` handles (which cross as a shared `Arc`, not a `GcRef`) pass.
    /// `str`/closure crossing **by value** lands in B3.3.
    ///
    /// B3.3c/d: the worker's `home` is a **read-only snapshot** of the parent's module graph
    /// ([`Vm::build_worker_modules`]) — top-level fns resolve via the rebuilt home globals, imports via
    /// the rebuilt `module_objs` — so a task may read post-init globals and call sibling/imported fns
    /// (a task WRITE lands on its own per-nursery copy; the old read-only G1 rule is retired). **Method tasks**
    /// (`spawn recv.m()`) dispatch against that rebuilt graph. Still deferred to B3.3-threads: real OS
    /// threads + a condvar `recv` (a method that blocks on `recv` faults here, no scheduler yet).
    /// The whole graph is reconstructed per task (correctness-first; pooling is a B3.3-threads concern).
    /// The synchronous single-thread driver: prepare + run on the calling thread. The `--parallel`
    /// engine calls `prepare_worker` and `ReadyWorker::run` separately (across the pool boundary), so
    /// this convenience wrapper is now only used by the B3.2–B3.3d worker unit tests.
    #[cfg(test)]
    pub(super) fn run_task_isolated(
        &mut self,
        task: PendingCall,
    ) -> Result<WorkerResult, RuntimeError> {
        self.prepare_worker(task, None, &[])?.run()
    }

    /// B3.3-threads — the parent-thread half of [`Vm::run_task_isolated`]: lower the task to a `Send`
    /// description against THIS heap, build the worker + reconstruct the module graph in its heap, and
    /// rebuild the callee/receiver + args **into the worker heap**, yielding a [`ReadyWorker`] that
    /// can be moved to a pool thread and `run()`. Everything that reads the parent heap happens here;
    /// nothing in `ReadyWorker::run` touches `self`.
    ///
    /// W6-2 — `snap` is the task's PIN, the module view snapshotted at its `spawn` ([`QueuedTask`]), which
    /// every nursery path (lazy, early-enlisted, eager per-connection) now passes in. `None` = "snapshot
    /// the current view here", left for the nursery-less callers whose prepare instant IS now:
    /// `run_task_isolated` (B3.3) and the `Executor` drain (`prepare_worker_from_wire`).
    pub(super) fn prepare_worker(
        &mut self,
        task: PendingCall,
        snap: Option<Arc<ModuleSnapshot>>,
        cell_ids: &[(GcRef, u32)],
    ) -> Result<ReadyWorker, RuntimeError> {
        // 1. Lower the task to a `Send` description in THIS (parent) heap (read-only serialize),
        //    rejecting any value that can't cross a heap boundary as-is.
        let lowered = self.lower_task(task, cell_ids)?;
        // 2. Build the worker + install the shared read-only module snapshot (D1): pre-alloc empty
        //    module objs (indices line up with the parent), faulting each module's globals into the
        //    worker heap lazily on first access — instead of eagerly reconstructing the whole graph
        //    per task. 3. rebuild the callable/receiver + args into the worker heap (a `home` index
        //    resolves to a pre-alloced empty module that faults on first global read). The actual
        //    invoke is `ReadyWorker::run`.
        let snap = match snap {
            Some(s) => s,
            None => self.ensure_snapshot(lowered.span())?,
        };
        let mut worker = self.spawn_worker();
        worker.install_snapshot(snap);
        let (call, span) = worker.rebuild_ready(lowered, !cell_ids.is_empty());
        Ok(ReadyWorker { worker, call, span })
    }

    /// PHASE 1 of task preparation — lower a [`PendingCall`] to a heap-independent [`Lowered`] against
    /// the CURRENT (parent/shell) heap: home indices resolve against `self.module_objs`, and every
    /// crossing value goes through `to_wire`/`ensure_crossable` (rejecting a non-isolable callee). Split
    /// out of [`Vm::prepare_worker`] as its own phase (historically so the since-removed `--serial`
    /// engine could reuse the exact M:N lowering — a behavior-preserving extraction; today it has the
    /// one caller).
    ///
    /// W7-4: ONE [`WireMemo`] spans the whole lowering (closure captures + args, or receiver + args),
    /// matched by ONE rebuild map in [`rebuild_ready`](Vm::rebuild_ready). Per-value memos re-split a
    /// shared binding here even after `do_spawn` unified it. See [`deep_clone_all`](Vm::deep_clone_all) for the scope invariant.
    /// **Serialize order must equal `rebuild_ready`'s reconstruct order**: whichever walk reaches a
    /// shared cell first emits its `WireValue::Cell` and the later one emits a `Backref`, so rebuilding
    /// in the other order would hit `from_wire_memo`'s `.expect` with an unregistered id. Here that
    /// order is ARGS, then the callee's captures — `wire_args` stays where it always was, at the top of
    /// the `Call` arm, because it is the only site applying `ensure_crossable` to spawn arguments and
    /// moving it below the callee classification would (a) skip argument validation entirely for a
    /// non-callable callee and (b) let a capture fault pre-empt an argument fault. `rebuild_ready`
    /// matches by reconstructing a `Closure`'s args before its captures.
    pub(super) fn lower_task(
        &mut self,
        task: PendingCall,
        cell_ids: &[(GcRef, u32)],
    ) -> Result<Lowered, RuntimeError> {
        // W7-4c — seed with the ids `deep_clone_all` gave this task's CLONE cells, so the binding a
        // module global also holds crosses under the snapshot's id and `rebuild_ready` + `fault_module`
        // (one shared map) tie both to ONE cell. `emitted` stays empty, so this walk writes the full
        // definition and never a `Backref` into the snapshot's separate, lazily-replayed serialization.
        //
        // `next_id` must clear EVERY seeded id, not just the snapshot's high-water mark: the clone
        // ids came from `deep_clone_all`, which itself minted from `snapshot_next_id` upward, so a
        // clone cell's id is typically `>= snapshot_next_id`. Starting the walk at `snapshot_next_id`
        // would re-mint an id already seeded here, and since `rebuild_ready` and `fault_module` share
        // ONE rebuild map that merges a container with a cell — `CellLoad on a non-cell object`.
        // Fenced by `spawn_named_args_keep_one_binding` (a spawn over a plain LOCAL, which is exactly
        // the case whose clone ids sit above the snapshot's).
        let next_id = cell_ids
            .iter()
            .map(|&(_, id)| id + 1)
            .max()
            .unwrap_or(0)
            .max(self.snapshot_next_id);
        let mut memo = WireMemo {
            cells: cell_ids.iter().copied().collect(),
            next_id,
            ..WireMemo::default()
        };
        let lowered = match task {
            PendingCall::Call { callee, args, span } => {
                let wargs = self.wire_args(args, span, &mut memo)?;
                match callee.as_obj() {
                    Some(h) => match self.heap.get(h).clone() {
                        Obj::Closure {
                            proto,
                            captured,
                            home,
                        } => {
                            // Lever #3: captures are positional; carry names from the proto in slot
                            // order so the wire format (Vec<(name, value)>) is unchanged.
                            let names = self.program.protos[proto].capture_names.clone();
                            let mut wcap = Vec::with_capacity(captured.len());
                            for (i, v) in captured.into_iter().enumerate() {
                                let w = self.to_wire_memo_at(v, span, &mut memo)?;
                                self.ensure_crossable(&w, span)?;
                                let name = names.get(i).cloned().unwrap_or_default();
                                wcap.push((name, w));
                            }
                            Lowered::Closure {
                                proto,
                                captured: wcap,
                                args: wargs,
                                home: self.home_index(home),
                                span,
                            }
                        }
                        Obj::Func { proto, home } => Lowered::Func {
                            proto,
                            args: wargs,
                            home: self.home_index(home),
                            span,
                        },
                        // A first-class builtin fn value (`f := ord; spawn f(x)`) is pure code — cross
                        // it by name; the worker re-allocs a fresh `Obj::Builtin`. Mirrors `Func`.
                        Obj::Builtin(name) => Lowered::Builtin {
                            name,
                            args: wargs,
                            span,
                        },
                        _ => {
                            return Err(self.err(
                                format!(
                                    "spawn: '{}' is not an isolable task",
                                    self.type_name(callee)
                                ),
                                span,
                            ));
                        }
                    },
                    None => {
                        return Err(self.err(
                            format!(
                                "spawn: '{}' is not an isolable task",
                                self.type_name(callee)
                            ),
                            span,
                        ));
                    }
                }
            }
            // B3.3d: the receiver + args cross by wire; dispatch resolves against the worker's
            // reconstructed `module_objs` (built below). `ensure_crossable` keeps a non-sendable
            // receiver (e.g. a closure) from silently dangling.
            PendingCall::Method {
                recv,
                name,
                args,
                span,
            } => {
                let wrecv = self.to_wire_memo_at(recv, span, &mut memo)?;
                self.ensure_crossable(&wrecv, span)?;
                let wargs = self.wire_args(args, span, &mut memo)?;
                Lowered::Method {
                    recv: wrecv,
                    name,
                    args: wargs,
                    span,
                }
            }
        };
        Ok(lowered)
    }

    /// PHASE 3 of task preparation — rebuild a [`Lowered`] task's callable/receiver + args INTO THIS
    /// VM's heap, resolving home indices against `self.module_objs` (the just-installed module view).
    /// Split out of [`Vm::prepare_worker`]. Infallible (all crossing checks happened in
    /// [`Vm::lower_task`]).
    ///
    /// W7-4: ONE rebuild map spans the whole `Lowered`, mirroring `lower_task`'s single [`WireMemo`]
    /// and reconstructing in the SAME order it serialized (a `Call`'s args before the callee's
    /// captures; a `Method`'s receiver before its args), so a cell shared between an arg and a capture
    /// is rebuilt once and both references tie to it — and no `Backref` is ever reached before the
    /// `WireValue::Cell` that defines it.
    pub(super) fn rebuild_ready(&mut self, lowered: Lowered, share: bool) -> (ReadyCall, Span) {
        // W7-4c — when this task carries snapshot-numbered cells, rebuild into the SAME map
        // `fault_module` drains, so a cell its captures rebuild is the one the module snapshot's later,
        // lazy replay ties to (both sides emit full definitions under the shared id; `from_wire_memo`
        // dedupes first-wins, so the order between them is free). Taken out of `self` for the borrow.
        //
        // When it carries NONE — no module global shares a binding with it, the common case — use a
        // throwaway map exactly as before: joining would make every `spawn` pay a scan of its whole
        // rebuilt object graph for a merge that can never happen. Measured: without this a 20k-spawn
        // storm ran 18% slower.
        let mut owned = if share {
            std::mem::take(&mut self.snapshot_rebuild)
        } else {
            super::fxhash::FxHashMap::default()
        };
        let rb = &mut owned;
        let out = match lowered {
            Lowered::Closure {
                proto,
                captured,
                args,
                home,
                span,
            } => {
                let home = self.worker_home(home);
                // ARGS FIRST — `lower_task` serializes them first (see its doc), so the defining
                // `WireValue::Cell` of a cell shared with a capture lives here.
                let args = self.rebuild_items(args, rb, |w| w);
                // Lever #3: rebuild positionally (slot order), discarding the carried names.
                let cap = self.rebuild_items(captured, rb, |(_k, w)| w);
                let callee = Value::obj(self.heap.alloc(Obj::Closure {
                    proto,
                    captured: cap,
                    home,
                }));
                (ReadyCall::Invoke { callee, args }, span)
            }
            Lowered::Func {
                proto,
                args,
                home,
                span,
            } => {
                let home = self.worker_home(home);
                let callee = Value::obj(self.heap.alloc(Obj::Func { proto, home }));
                let args = self.rebuild_items(args, rb, |w| w);
                (ReadyCall::Invoke { callee, args }, span)
            }
            Lowered::Builtin { name, args, span } => {
                let callee = Value::obj(self.heap.alloc(Obj::Builtin(name)));
                let args = self.rebuild_items(args, rb, |w| w);
                (ReadyCall::Invoke { callee, args }, span)
            }
            Lowered::Method {
                recv,
                name,
                args,
                span,
            } => {
                let recv = self.from_wire_memo(recv, rb);
                let args = self.rebuild_items(args, rb, |w| w);
                (ReadyCall::Method { recv, name, args }, span)
            }
        };
        // W7-4c — prune to cells, for the same reason `fault_module` does: only a cell can be
        // back-referenced by a LATER, separate serialization (the module snapshot's), and keeping the
        // task's whole rebuilt object graph in a `Vm`-lived GC root would make it immortal.
        if share {
            owned.retain(|_, &mut h| matches!(self.heap.get(h), Obj::Cell(_)));
            self.snapshot_rebuild = owned;
        }
        out
    }

    /// B3.6 — the `Executor`-drain analogue of [`prepare_worker`]: build a worker, install the shared
    /// read-only [`ModuleSnapshot`] (D1 — modules fault in lazily on first global access), and rebuild
    /// a submitted closure (a [`WireValue::Closure`] drained from the executor queue) into that heap as
    /// a zero-arg call. The submitted closure already crossed `to_wire`/`ensure_crossable` at `submit`,
    /// but `ensure_snapshot` can fault if a module global is a frame-holding generator — so this
    /// forwards that snapshot fault (re-stamped with `span`) rather than panicking. `--parallel` only.
    ///
    /// W6-2 — an `Executor` has no nursery, so there is no pin: the snapshot is taken where this is
    /// called, which is the instant the job actually starts. Under EAGER execution that is the
    /// `submit` (a job observes the globals as of its submission), where the pre-eager queueing model
    /// took it at the drain. The difference is observable only by a program that inspects a job's
    /// effect BETWEEN `submit` and `shutdown()` — which is exactly the shape the `Executor` docs tell
    /// you not to write.
    pub(super) fn prepare_worker_from_wire(
        &mut self,
        task: WireValue,
        span: Span,
    ) -> Result<ReadyWorker, RuntimeError> {
        let snap = self.ensure_snapshot(span)?;
        let mut worker = self.spawn_worker();
        worker.install_snapshot(snap);
        let callee = worker.from_wire(task);
        Ok(ReadyWorker {
            worker,
            call: ReadyCall::Invoke {
                callee,
                args: Vec::new(),
            },
            span,
        })
    }

    /// EAGER `submit` (M:N), the fallible half — build the worker for ONE submitted closure. Paired
    /// with the free [`dispatch_eager_job`], which is the part that runs under the executor lock.
    ///
    /// This is the ancestor model (Python `ThreadPoolExecutor.submit`, Java `ExecutorService.submit`):
    /// work starts at once and `shutdown()` waits for it, rather than nothing running until the reap
    /// point.
    ///
    /// The job takes the executor's PER-CORE cancel flag (not a per-drain one — there was no such
    /// thing to share when jobs only ran inside one drain call), so `shutdown_now` can trip work that
    /// is already running (decision D4, cooperative). W7-5 is untouched: an ordinary job fault still
    /// does NOT trip that flag ([`ReadyWorker::run_outcome`]), only `os.exit` / [`executor_hard_halt`]
    /// do, so siblings keep running and `shutdown` raises the lowest SUBMISSION-INDEX fault.
    ///
    /// The worker also carries the executor CORE, which owns the job's outcome slot and its cancel
    /// flag. Whether a blocked job is DEADLOCKED is not asked of that core: it is the process-wide
    /// question in [`crate::vm::quiesce`] (`future.md` §2d step 0), which counts this job through
    /// `outstanding` and sees it park through the blocked-party registry. No SCHEDULER predicate reads
    /// eager-job state — `is_deadlocked` is untouched. What eager execution DOES change is that a
    /// blocking op inside a job can no longer assume its submitter is stuck in the drain — see
    /// [`Vm::block_recv`].
    pub(super) fn prepare_eager_job(
        &mut self,
        core: &Arc<ExecutorCore>,
        task: WireValue,
        span: Span,
    ) -> Result<ReadyWorker, RuntimeError> {
        // MUST run with no executor lock held — see the call site in `executor_method`: this rebuilds
        // the closure into the worker's heap, which can GC, and the GC's `Obj::Executor` mark arm
        // takes `core.inner`. It is also the fallible half (`ensure_snapshot` on a frame-holding
        // generator global), and that fault must surface out of `submit` before any slot is reserved.
        let mut rw = self.prepare_worker_from_wire(task, span)?;
        rw.worker.eager_core = Some(Arc::clone(core));
        rw.worker.cancel = Some(Arc::clone(&core.cancel));
        // …and the ENCLOSING EXECUTOR's flag with it, when this executor was CREATED inside an eager
        // job (`ExecutorCore::creator_cancel`, captured at `Op::NewExecutor`). Without it this was the
        // ONE seam in the tree installing a cancel token without the `scope_ancestors()` half every
        // nursery seam pairs it with (`spawn_shell`, `run_one_fiber`), so an inner executor's job had
        // the chain `[inner.cancel]` and never observed an outer `shutdown_now` — the outer job's own
        // `sleep_ms(8000)` died at 50 ms while the IDENTICAL sleep one executor deeper ran to
        // completion and the program paid the full 8 s at the exit drain. That inconsistency is
        // Chezzi-vs-Chezzi; the ancestors are split (CPython's nested `ThreadPoolExecutor` does NOT
        // propagate — 8.04 s and the job's line printed, for both `shutdown(wait=False)` and
        // `wait=True`; Go's derived `context.WithCancel(parent)` DOES — child cancelled at 50 ms), so
        // this follows W7-16's ruling one level down: an executor that disagrees with the nursery
        // beside it is the defect.
        //
        // Read from the CORE, not from `self`: an `Executor` crosses the airlock by `Arc`, so keying
        // this on the submitter let a job of an unrelated executor donate ITS cancel chain to a job of
        // `main`'s executor — an outer `shutdown_now()` then killed an already-started job that was
        // none of its business, and `main`'s own graceful `shutdown()` returned with the work dropped.
        //
        // Empty for an executor created by `main` or by a `parallel:`/`spawn` fiber, so decision A2's
        // "an `Executor` is DETACHED" survives intact: detached from NURSERIES, not from an enclosing
        // executor job. `scope_ancestors()` already severs inside a `defer`, so an executor created by
        // a cancelled job's cleanup still runs uncancelled work.
        rw.worker.cancel_outer = core.creator_cancel.clone();
        Ok(rw)
    }

    /// EAGER `shutdown`/`shutdown_now` (M:N) — wait for every in-flight job, then reduce the
    /// submission-ordered outcome slots (decision D1: the executor is detached, and this is the join).
    /// Reuses [`Vm::reduce_task_slots`] verbatim, so the W7-5 fault contract, W7-5c's unconditional
    /// per-slot output flush and decision F's task-order flush all carry over unchanged.
    ///
    /// The wait is a bounded [`DEMOTE_POLL_BACKOFF`] poll rather than a plain condvar wait. It used to
    /// be the latter — `finish` always runs (see `dispatch_eager_job`'s panic note), so no wakeup is
    /// ever missed — but a party that only REGISTERS and never ASKS leaves a whole family of genuine
    /// deadlocks silent: a run whose every counted party sits in one of these joins has nobody to
    /// evaluate the verdict at all. Measured (`gaps.md` W7-58, the residual hunt): three executors
    /// whose jobs each `shutdown()` the next, plus `main` joining the first, HUNG forever — 4 parties,
    /// `live == 4`, every `Join` unsatisfiable, and not one of them ever called `quiesced`. The
    /// sibling shapes fault in milliseconds only because SOME party in them happens to be channel-
    /// blocked (`two_executors_deadlocking_each_other_fault`), i.e. the fault was an accident of the
    /// shape, not a property of the join. So this now polls at exactly the cadence every other
    /// blocking-in-place site pays ([`Vm::block_wait_tick`], `demote_recv_block`).
    ///
    /// **And only when every registered party is a `Join`** ([`quiesce::QuiesceState::quiesced_only_joins`]):
    /// any other kind of party has a judge of its own whose fault names the real blocking site, so the
    /// joiner would only be racing it for a worse message.
    ///
    /// **W7-60 — it also observes `--timeout` and cancel**, in [`Vm::block_halt_check`]'s order
    /// (`--timeout` > cancel > the verdict). Before this it observed neither, so a job blocked in an
    /// inner wait made its joiner both uncancellable and immune to the wall-clock cap: measured, an
    /// outer `shutdown_now()` at 200 ms did not end a run until **10 009 ms**, against
    /// `docs/stdlib.md`'s own promise that "a scope cancel or an `Executor.shutdown_now()` ends the
    /// wait within ~5 ms". Neither rung is gated on [`Vm::is_counted_party`] — they are facts about
    /// the RUN, not about who may judge it — which is exactly how `block_halt_check` gates its own
    /// three (only the verdict at the bottom carries that test).
    ///
    /// **There is deliberately NO `os.exit` rung, and the reason is narrower than it first looks.**
    /// W7-47 routes a run-wide exit through each JOB's own blocking wait, so for any job that HAS a
    /// cancellation checkpoint `outstanding()` drops and this join releases through the mechanism it
    /// already has — a rung here would be redundant. That argument does **not** extend to a job with
    /// no checkpoint at all: measured (W7-60 review, charge A3), `os.exit(3)` at 200 ms beside a job
    /// running `process.run("sleep 5")` exits after **5.014 s**, not promptly. An exit rung would not
    /// fix that either — it would unblock the WAITER while the uninterruptible child kept running,
    /// which is the documented ceiling of a blocking native (`docs/stdlib.md` §"blocking calls cannot
    /// be interrupted"), not something a join can lift. What the bail-out CAN do about an abandoned
    /// job, it now does unconditionally: it trips `core.cancel` (see the store below), so every job
    /// that owns a checkpoint stops at it.
    ///
    /// **Both rungs are evaluated while `eager` (G) is HELD, deliberately.** `deadline_halt` takes no
    /// lock and `cancel_requested` takes none either, so there is no inversion to avoid — and holding
    /// G means there is no window in which a job could finish between the check and the decision.
    /// That matters because the cancel rung LATCHES (`self.cancelled = true`): a halt observed in
    /// such a window and then discarded as stale would leave this fiber permanently
    /// `cancel_suppressed`, no-opping every later checkpoint. Only the verdict needs the drop, and
    /// only because `quiesced_only_joins` takes P and then G.
    ///
    /// **Collateral of the cancel rung, accepted:** unwinding here leaves the joined executor marked
    /// `shut` with jobs still outstanding, so the exit drain skips it. That is not a new class — the
    /// pre-existing deadlock bail-out below leaves exactly the same state — and it is what
    /// `shutdown_now`'s documented "ask running jobs to stop cooperatively" means for a job that is
    /// itself parked in a join. The cancelled joiner's own outcome is SWALLOWED, as every cancelled
    /// task's is.
    ///
    /// **Lock order.** `core.eager` (G) is DROPPED before `quiesced` is called: the one total order is
    /// `parties` (P) → … → `ExecutorCore::eager` (G), and `quiesced` takes G under P (both through
    /// `outstanding_jobs` and through the `Join` arm's own satisfiability). Holding G across that call
    /// is a lock inversion, i.e. a real hang.
    ///
    /// Gated on [`Vm::is_counted_party`] for the same reason the registration below is: a joiner
    /// running inside a nursery task is not in `live`, so it must neither register nor judge.
    ///
    /// **A joiner is a blocked party** (`future.md` §2d step 0), and registering it here is what makes
    /// the process-wide verdict able to see `main` inside `shutdown()` — the node whose absence left
    /// W7-12's arms unable to tell "my submitter may still send" from "my submitter is waiting for
    /// me". It is registered for EVERY join, the explicit `shutdown()` and the program-exit drain
    /// alike, which is what closes W7-12r's residual (c); the old `JoinGuard` could not be armed at
    /// the drain because the verdict was per-executor and registry ORDER would then have decided
    /// whose job faulted. A `Join` wait is never satisfiable on its own — the jobs it waits for fault
    /// themselves, which fills their slots and releases this wait normally.
    ///
    /// A joiner running inside a nursery task is NOT a counted party and so does not register: a
    /// sibling task may be the very producer the blocked job needs (pinned by
    /// `executor_job_keeps_waiting_when_shutdown_runs_beside_a_live_producer`).
    pub(super) fn join_eager_jobs(&mut self, core: &Arc<ExecutorCore>) -> Result<(), RuntimeError> {
        // A JOIN NEVER WAITS FOR ITSELF. When this thread is an eager job OF THIS CORE — a job that
        // calls `ex.shutdown()`/`ex.shutdown_now()` on the executor it is running under — its own
        // slot is one of the `outstanding` below and stays so until it returns from here. Waiting
        // for `0` is then waiting for an event this thread is itself the only obstacle to, and it
        // showed up as the one unacceptable outcome: the party registered just below answered
        // "never satisfiable", every other party in the run was a `Join` too, and the verdict
        // declared a program in which EVERY JOB HAD ALREADY RUN to be deadlocked — measured 8/60
        // debug runs for `shutdown_now` (the other 52 escaped only because `shutdown_now` trips
        // `core.cancel`, so the self-joining job usually reached the cancel rung below before
        // `main`'s poll reached the verdict — a coin flip between two 5 ms pollers) and 8/8 for the
        // graceful `shutdown()`, which has no cancel to escape through.
        //
        // The ancestor refuses the self-join rather than hanging on it: CPython 3.14.6, measured,
        // raises `RuntimeError: cannot join current thread` from `shutdown(wait=True)` inside its
        // own worker and returns in 0.000 s from `shutdown(wait=False)`; in neither case is the RUN
        // declared dead. Chezzi's join is over a COUNT rather than over thread handles, so it can
        // do better than refuse: it waits for everything the executor owes EXCEPT this job, which
        // is the same wait with the impossible term removed. `slack` is that term.
        //
        // Discounting can only ever RELEASE a wait or VETO a verdict, never manufacture a fault —
        // the safe direction of `quiesce`'s error table. Sibling shapes are untouched: two jobs of
        // DIFFERENT executors joining each other get `slack == 0` on both sides and still fault
        // (`two_executors_deadlocking_each_other_fault`), and a self-joiner whose sibling is
        // genuinely stuck on a channel is still unsatisfiable at `slack` and is judged by that
        // sibling's own blocking site, which names the real line.
        let slack = usize::from(
            self.eager_core
                .as_ref()
                .is_some_and(|mine| Arc::ptr_eq(mine, core)),
        );
        // A self-join reduces NOTHING (see the slot rule at the end of this fn), so it owes the core
        // to a LATER join — and the caller marked the core `shut` a moment ago, which is what used to
        // make `drain_live_executors` skip it and drop every sibling's buffered output and fault.
        // Marked HERE, before the wait, not after it: the wait is unbounded (it waits for the
        // siblings), and for that whole time the core is already `shut` and already unreduced, so a
        // mark placed after the wait would leave the exit drain blind for exactly as long as the job
        // takes. Cleared again below on the two paths that discharge the debt.
        //
        // ponytail: the remaining window is the few instructions between the caller's `shut = true`
        // and this store — the flag is not under `inner`'s lock, because taking `inner` here would
        // invert the fixed inner → eager order. A drain landing inside that window skips the core, but
        // it would also be exiting while a job is mid-`shutdown()`, which is the pre-existing
        // exit-mid-job hazard `Executor.shutdown_now` already documents, not something this adds.
        // Move the flag into `ExecState` (beside `shut`, under the same lock) if that ever bites.
        if slack > 0 {
            core.unreduced.store(true, Ordering::Release);
        }
        let _party =
            self.block_party_guard(crate::vm::quiesce::PartyWait::Join(Arc::clone(core), slack));
        // W7-60 — carried out of the loop rather than `?`-ed, so the slot rule below can read it: on
        // EVERY bail-out the jobs that own the remaining slots are still outstanding, so this thread must
        // not empty the vec they will `finish` into (the FINISHED ones are flushed instead).
        let mut bail: Option<RuntimeError> = None;
        let slots = {
            let mut g = core.eager.lock().unwrap_or_else(|e| e.into_inner());
            while g.outstanding() > slack {
                let (next, timed) = core
                    .eager_cv
                    .wait_timeout(g, DEMOTE_POLL_BACKOFF)
                    .unwrap_or_else(|e| e.into_inner());
                g = next;
                if g.outstanding() <= slack {
                    continue; // progress — leave through the loop head and reduce normally
                }
                // W7-60 — the run-wide halts, in `block_halt_check`'s order. Ungated (see the doc
                // above) and evaluated under G, which is what keeps the cancel latch from being set
                // on a verdict this thread might then discard as stale. Checked on every wake, not
                // only a timed-out one: they are free, and a notified wake is as good a moment as
                // any to notice the run is over.
                if let Err(e) = self.deadline_halt(Span::RUNTIME) {
                    bail = Some(e);
                    break;
                }
                if self.cancel_requested() {
                    self.cancelled = true;
                    bail = Some(self.err("cancelled".to_string(), Span::RUNTIME));
                    break;
                }
                // The deadlock verdict keeps BOTH of its old gates: only on a timed-out wait (a
                // notified wake means something just moved, so the party set is least trustworthy
                // right then), and only from a party the count can judge.
                if !timed.timed_out() || !self.is_counted_party() {
                    continue;
                }
                // W7-58 residual — DROP `eager` (G) before taking `parties` (P). See the doc above:
                // the order is P → G, and `quiesced` takes G beneath P.
                drop(g);
                let verdict = self.quiesce.quiesced_only_joins(&self.exec_registry);
                g = core.eager.lock().unwrap_or_else(|e| e.into_inner());
                // Re-check under the re-taken lock: a job may have finished in the gap, which is
                // progress and makes the verdict stale.
                if verdict && g.outstanding() > slack {
                    bail = Some(self.err(JOIN_DEADLOCK_MSG.to_string(), Span::RUNTIME));
                    break;
                }
            }
            // Do NOT steal the WHOLE slot vec on a bail-out: the jobs that own the remaining slots are
            // still outstanding and would `finish` into a vec this thread had emptied. But the jobs
            // that ALREADY finished own buffered output, and dropping it is a silent loss — a `print`
            // that ran to completion and never reached stdout. `take_finished` is the length-preserving
            // half: it flushes those and leaves the outstanding indices intact (W7-60 review, charge
            // A2 — reproduced as a missing `QUICK DONE` under `--timeout`).
            // …and a SELF-JOIN must not steal it either, for the same reason plus a sharper one: this
            // thread's OWN slot is still reserved and it will `finish` into that index the moment it
            // returns from here, so `take_slots`' `mem::take` would leave `finish` writing past the
            // end (the `debug_assert` in `EagerState::finish`, an out-of-bounds panic on a pool
            // thread in release). It takes NOTHING at all, not even the finished outcomes, so that a
            // LATER join reduces the whole vector in SUBMISSION order — which keeps every sibling's
            // output at its own slot position (W7-5c) and lets a sibling's fault surface from the
            // executor that owns it rather than being re-raised inside an unrelated job.
            //
            // That later join has to be guaranteed, and it was not. This join marks the core `shut`,
            // and `drain_live_executors` used to read `shut` as "already handled" and skip it — so a
            // job that shut down its own executor with no enclosing `shutdown()` left the whole
            // vector unreduced: every sibling's buffered output dropped, every sibling's fault
            // swallowed, the run exiting 0. (Under `chezzi run` the output half is invisible, since a
            // streamed `print` already reached fd 1 at the moment it ran; on the buffered sink — every
            // embedder, `run_capture` — the slot IS the only copy.) `ExecutorCore::unreduced`, set
            // above, is the hand-off: the exit drain picks such a core up exactly once.
            if bail.is_some() {
                // Debt discharged the only way an ORDINARY (non-self) bail can: flush what finished,
                // and CLEAR the mark — this thread was never going to `take_slots` on success either,
                // so a bail truly has no successor to promise, and re-joining at exit a core whose
                // join just reported a deadlock would undo the "last chance to ask" reasoning below.
                // Gated on `slack == 0`: a SELF-join (`slack > 0`) never discharges the debt, bail or
                // not — clearing here regardless of `slack` would drop a mark this call did not set
                // (an earlier self-join left it true, promising a LATER join; this bail is not that
                // join) and the exit drain would then skip a core still owed a reduce. This branch is
                // why the mark cannot be inferred from "the slot vector is non-empty": `take_finished`
                // is length-preserving, so it leaves one behind.
                if slack == 0 {
                    core.unreduced.store(false, Ordering::Release);
                }
                let done = g.take_finished();
                drop(g);
                for o in done {
                    let (out, stderr) = o.streams();
                    self.out.extend_from_slice(out);
                    self.stderr.extend_from_slice(stderr);
                }
                Vec::new()
            } else if slack > 0 {
                Vec::new()
            } else {
                // The vector is reduced here and cannot refill (`shut` is set before every join, and
                // `submit` refuses a shut core), so the hand-off is discharged for good — which is
                // what stops `drain_live_executors` re-picking this core forever.
                core.unreduced.store(false, Ordering::Release);
                g.take_slots()
            }
        };
        drop(_party);
        if let Some(e) = bail {
            // W7-60 review, charge A1 — ASK THE WORK TO STOP, don't just stop waiting for it. Every
            // other `--timeout`/cancel observation happens INSIDE a job, where `run_outcome` trips the
            // executor's cancel for us; this one is on the JOINER, and without this store the abandoned
            // jobs never learn. That is not merely untidy: `Vm::do_call`'s blocking-native offload gates
            // on `cancel_requested()`, so a job part-way through a sequence of blocking calls would
            // launch the NEXT one after the run had already reported TIMED-OUT (measured: a fresh
            // subprocess spawned at ~1.3 s under `--timeout=300`). The executor is already `shut`, and
            // for an ORDINARY (non-self, `slack == 0`) join this path deliberately leaves `unreduced`
            // clear, so `drain_live_executors` will never revisit it — this is the last chance to ask.
            // A self-join's bail (`slack > 0`) leaves the mark exactly as it found it instead — see
            // the `slack == 0` gate above the `unreduced.store(false, …)` a few lines up.
            //
            // It is a REQUEST, not a kill — a job with no cancellation checkpoint (an in-flight
            // `process.run` child, `docs/stdlib.md` §"blocking calls cannot be interrupted") still runs
            // to completion. That ceiling is the documented one, unchanged here.
            core.cancel.store(true, Ordering::Relaxed);
            return Err(e);
        }
        self.reduce_task_slots(slots)
    }

    /// Reject a wired value that still carries a by-reference [`Handle`](WireValue::has_handle) — a
    /// heap-local `GcRef` that cannot cross into another heap as-is. As of B3.3 plain data, `str`/bytes,
    /// closures/functions, native/FFI fns, and the `Channel`/`Shared`/`Executor`/socket handles all
    /// cross by value (or as a shared `Arc` core), so the only remaining non-crossable value is a
    /// module handle — a module's mutable globals can't be shared across an OS-thread heap boundary.
    /// (Source-unreachable: `module` is not a nameable type, so this is a defensive-only guard.)
    pub(super) fn ensure_crossable(&self, w: &WireValue, span: Span) -> Result<(), RuntimeError> {
        if w.has_handle() {
            return Err(self.err(
                "this task value can't cross a worker boundary — plain data, closures/functions, \
                 native/FFI fns, and Channel/Shared/Executor handles are sendable, but a module handle cannot cross"
                    .to_string(),
                span,
            ));
        }
        Ok(())
    }

    /// Serialize a task's argument list across the airlock (read-only walk of this heap), rejecting any
    /// argument that can't cross a heap boundary as-is (see [`Vm::ensure_crossable`]). W7-4: the caller
    /// supplies the [`WireMemo`], because the args share one serialization with the callee's captures
    /// (and the matching rebuild map in [`rebuild_ready`](Vm::rebuild_ready)) — see
    /// [`deep_clone_all`](Vm::deep_clone_all) for the scope invariant.
    pub(super) fn wire_args(
        &self,
        args: Vec<Value>,
        span: Span,
        memo: &mut WireMemo,
    ) -> Result<Vec<WireValue>, RuntimeError> {
        args.into_iter()
            .map(|a| {
                // `to_wire_memo_at` re-stamps a generator's placeholder span with this call site's.
                let w = self.to_wire_memo_at(a, span, memo)?;
                self.ensure_crossable(&w, span)?;
                Ok(w)
            })
            .collect()
    }

    /// B3.6 — serialize a callable (`Executor.submit`'s argument) across the airlock **by value**, so a
    /// submitted task can be reconstructed and run on a pool thread. As of B3.3 the GENERIC `to_wire`
    /// already lowers a closure/bare-fn by value (a [`WireValue::Closure`]/[`WireValue::Func`], proto +
    /// wired captures + home index — no heap-local `GcRef`), so this is now a thin delegate that reuses
    /// that path and applies the [`ensure_crossable`] backstop. It still rejects a non-callable
    /// argument with the `submit`-specific message (a plain value / handle would `ensure_crossable`-pass
    /// or produce a less specific fault otherwise). A captured `Channel`/`Shared` crosses as its shared
    /// `Arc` (via `to_wire`), unchanged.
    pub(super) fn wire_callable(&self, v: Value, span: Span) -> Result<WireValue, RuntimeError> {
        if let Some(h) = v.as_obj()
            && matches!(self.heap.get(h), Obj::Func { .. } | Obj::Closure { .. })
        {
            // `to_wire_at` re-stamps a generator capture's placeholder span with `span`.
            let w = self.to_wire_at(v, span)?;
            self.ensure_crossable(&w, span)?;
            return Ok(w);
        }
        Err(self.err("submit requires a function or closure".to_string(), span))
    }

    /// A fresh empty module to serve as a worker closure's `home`. The parent's `home` `GcRef` can't
    /// cross heaps; used as the fallback when the task's home is not a real `module_objs` entry (the
    /// hand-built unit-test fixtures) — real spawns resolve a reconstructed module (see `worker_home`).
    pub(super) fn fresh_worker_home(&mut self) -> GcRef {
        self.heap.alloc(Obj::Module(Box::new(ModuleData {
            name: "<worker>".into(),
            slots: Vec::new(),
            index: Default::default(),
        })))
    }

    /// B3.3c — the index of a `home` module `GcRef` in this VM's `module_objs`, so the worker can
    /// resolve the corresponding rebuilt module. `None` for a home not in the table (test fixtures).
    pub(super) fn home_index(&self, home: GcRef) -> Option<usize> {
        self.module_objs.iter().position(|&m| m == home)
    }

    /// B3.3c — resolve a lowered home index to this (worker) VM's reconstructed module obj, falling
    /// back to a fresh empty home when the parent home was not a real module (test fixtures).
    pub(super) fn worker_home(&mut self, idx: Option<usize>) -> GcRef {
        match idx {
            Some(i) if i < self.module_objs.len() => self.module_objs[i],
            _ => self.fresh_worker_home(),
        }
    }

    /// D1 — the read-only [`ModuleSnapshot`] of the module graph THIS view currently sees, replayed
    /// into each worker/child it prepares.
    ///
    /// W6-2 — this is resolved per task at its `spawn` ([`Vm::register_task`]), not frozen for the run.
    /// `snapshot_memo` is a CACHE with two invalidation rules, not a forever-memo: a module-slot write
    /// drops it (`set_global_slot` / `module_define`), and `Op::EnterNursery` drops it when the cached
    /// snapshot is not `reusable` (some global holds a mutable aggregate, which in-place mutation changes
    /// with no slot write). So a global initialized or mutated after an earlier nursery is SEEN by later
    /// tasks (it used to replay as the frozen first-nursery copy, or as `nil` if it had not been
    /// initialized yet), while a program whose globals are only scalars / `Channel` / `Shared` / `Atomic`
    /// still builds exactly one snapshot for the run, and a spawn storm inside one nursery builds one.
    ///
    /// On a WORKER/fiber view the globals fault in lazily, so materialize the whole view first: without
    /// that a re-snapshot would read EMPTY slots and recreate W6-2 one level down. Free on the
    /// top-level VM, and on a fiber with no heap of its own (nothing is lazy there — `module_snapshot`
    /// is `None`).
    ///
    /// Fallible: a module global that is a frame-holding generator cannot be snapshotted (its parked
    /// frames reference the parent heap). `to_wire`/`to_snap` stamp the airlock fault with a
    /// placeholder span, so this choke point RE-STAMPS it with the real nursery/spawn-site `span`
    /// (the caller has it) — a graceful, catchable error instead of a panic. The build path caches
    /// ONLY on success, so a deterministic failure is never cached as a stale error.
    pub(super) fn ensure_snapshot(
        &mut self,
        span: Span,
    ) -> Result<Arc<ModuleSnapshot>, RuntimeError> {
        if let Some(s) = &self.snapshot_memo {
            return Ok(Arc::clone(s));
        }
        if self.module_snapshot.is_some() {
            for i in 0..self.module_faulted.len().min(self.module_objs.len()) {
                self.fault_module(i);
            }
        }
        // W7-4c — seed the build from the MONOTONIC counter, and keep the cell registry it produces
        // so a later `deep_clone_all`/`lower_task` can serialize the same binding under the same id.
        // On failure nothing is stored (the build path caches only on success), so a faulted snapshot
        // never leaves a half-registry behind.
        let (built, cells, next_id) = self
            .snapshot_modules(self.snapshot_next_id)
            .map_err(|e| self.err(e.message, span))?;
        let snap = Arc::new(built);
        self.snapshot_cells = cells;
        self.snapshot_next_id = next_id;
        self.snapshot_builds += 1;
        // Cache unconditionally: consecutive `spawn`s into one nursery must not each rebuild the whole
        // view (that is O(all module globals) per spawn — measured 84× on a spawn storm with a big
        // aggregate global). Freshness comes from the two invalidation rules, not from refusing to cache.
        self.snapshot_memo = Some(Arc::clone(&snap));
        Ok(snap)
    }

    /// D1 — read this VM's initialized module graph (read-only) into a heap-independent
    /// [`ModuleSnapshot`]: one [`ModuleSnap`] per module in `module_objs` order (so a callable's home
    /// index lines up with a worker's pre-alloced modules), each global lowered by [`Vm::to_snap`].
    /// Replaces the eager per-task `build_worker_modules` reconstruction — built once, replayed lazily.
    pub(super) fn snapshot_modules(
        &self,
        next_id: u32,
    ) -> Result<super::SnapshotBuild, RuntimeError> {
        let mut modules = Vec::with_capacity(self.module_objs.len());
        // W6-2 — computed inside the walk that already visits every global (no extra traversal).
        let mut reusable = true;
        // W7-4a — ONE [`WireMemo`] spans EVERY module, matched by the one `Vm`-lived rebuild map
        // `fault_module` drains ([`Vm::snapshot_rebuild`]) — the scope invariant of `deep_clone_all`,
        // now at snapshot scope. A memo per module gave a cell reached from globals in two DIFFERENT
        // modules a fresh id each (`l.GI := k.C.inc` / `main.GG := k.C.get`), so the task rebuilt two
        // cells and its write to one was invisible to the other: `0`, where CPython and Go both
        // measure `2`. `cells`/`next_id` therefore persist across the loop.
        let mut memo = WireMemo {
            // W7-4c — MONOTONIC across builds, so an id from a superseded snapshot can never collide
            // with one from this build (a stale seed then simply misses; see `Vm::snapshot_next_id`).
            next_id,
            ..WireMemo::default()
        };
        for &pm in &self.module_objs {
            // M19 Phase 2b — collect globals in *slot order* (not HashMap iteration order) so a
            // worker replays them into matching slots; the shared `Arc<Program>` slot map makes
            // parent and worker agree on slot↔name regardless of any hash ordering.
            let (name, globals): (Box<str>, Vec<(String, Value)>) = match self.heap.get(pm) {
                Obj::Module(m) => (m.name.clone(), module_slot_pairs(&m.slots, &m.index)),
                _ => ("<worker>".into(), Vec::new()),
            };
            // Fallible: a module global that is a frame-holding generator faults here (graceful,
            // re-stamped with the nursery span by `ensure_snapshot`) instead of panicking in `to_snap`.
            let mut snapped = Vec::with_capacity(globals.len());
            // W7-4a — each module must be SELF-CONTAINED: modules fault in lazily, in whatever order
            // the task touches them (and a module it never touches never faults at all), so a
            // `Backref` pointing into a module that has not been replayed yet would resolve to
            // nothing. Clearing `emitted` (not `cells`) makes this module re-emit the FULL
            // `WireValue::Cell` definition under the SAME id, and `from_wire_memo`'s first-wins dedupe
            // ties the second definition to the cell the first one built. The cost is wire size, and
            // only for a cell reached from 2+ modules — the same trade `elem_split` already makes for
            // `RwShared` stores. WITHIN a module the pop-on-DFS-exit `path` discipline is untouched,
            // so the data-DAG contract is unchanged.
            memo.emitted.clear();
            for (k, v) in globals {
                reusable &= self.slot_snapshot_reusable(v);
                snapped.push((k, self.to_snap(v, &mut memo)?));
            }
            modules.push(ModuleSnap {
                name,
                globals: snapped,
            });
        }
        Ok((
            ModuleSnapshot { modules, reusable },
            Arc::new(memo.cells),
            memo.next_id,
        ))
    }

    /// W6-2 — may a snapshot containing this module-global value be CACHED and replayed by a later
    /// nursery? Only if the value's own CONTENTS cannot change without a module-slot write (which
    /// `set_global_slot`/`module_define` hook): an immutable leaf (`str`/`bytes`/`int`/`float`/`ptr`), a
    /// code value with no captured state (`Func`/`Native`/`Builtin`/`Cffi`), an `Arc`-shared core that
    /// crosses by HANDLE rather than by copy (`Channel`/`Shared`/`RwShared`/`Atomic`/`Executor`/socket/
    /// `Writer`/`Reader` — the sharing escape hatch, so no snapshot can be stale about them), or an
    /// import-alias `Module` whose own globals the same walk covers.
    ///
    /// Everything else — `List`/`Map`/`Set`/`Tuple`/`Struct`/`Enum`/`NewType`/`Cell`/`Closure`/
    /// `Generator`/`Iter`/`ByteArray`/an inline `Module` — is mutated IN PLACE (`q.push(1)`, `m[k] = v`,
    /// `p.x = 1`, a captured cell) with no slot write to invalidate on, so a snapshot holding one is
    /// never cached: the nursery rebuilds. A WHITELIST on purpose — a new `Obj` variant defaults to
    /// "rebuild" (slower, never stale).
    ///
    /// (A precise per-mutation invalidation — hooking the mutating intrinsics themselves — is the
    /// recorded follow-up; it needs `src/vm/call.rs`, fenced while W6-3 is in flight.)
    fn slot_snapshot_reusable(&self, v: Value) -> bool {
        let Some(h) = v.as_obj() else {
            return true; // inline scalar (int/bool/nil) — immutable
        };
        match self.heap.get(h) {
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
            | Obj::Reader(_) => true,
            // An import alias: reusable iff it is one of the modules this same walk snapshots (so its
            // own globals were vetted too). A module NOT in `module_objs` is encoded INLINE — its
            // globals are unvetted here, so play it safe and rebuild.
            Obj::Module(_) => self.module_objs.contains(&h),
            _ => false,
        }
    }

    /// D1 — lower one parent-heap global value into a heap-independent [`SnapValue`]. The snapshot
    /// analogue of the old `map_global_value`: a `GcRef` is heap-local, so a callable's home/captures,
    /// an import-alias module ref, and any container embedding one of those must be encoded
    /// structurally, never by reference.
    ///
    /// - **Fast path:** a value whose wire form has no by-reference `Handle` is pure data (`str` by
    ///   value) or a `Channel`/`Shared`/`Executor` core (Arc-shared) — encode the exact wire form.
    /// - `Func`/`Closure` → record the home as a `module_objs` index (captures recursed); import-alias
    ///   `Module` → `ModuleAlias(idx)`; `Native` → fn pointer; containers → element-wise (map/set
    ///   hashes are value-derived, carried unchanged).
    ///
    /// A MODULE-GLOBAL live generator crosses BY VALUE (backlog item B): `to_wire` deep-copies it (its
    /// proto, backing closure, and parked slots) and `from_wire` rebuilds a FRESH independent
    /// `GeneratorCore` on the worker heap, so each task drives its own copy (consistent with the F1
    /// per-task snapshot). It rides the fast path when handle-free; the slow `Obj::Generator` arm
    /// snapshots a sendable generator BY VALUE and a non-sendable one (parked host handle / >depth-cap
    /// nest / reference cycle) as an inert `Nil` placeholder — so a module that merely *holds* a
    /// non-sendable generator it never sends still spawns cleanly (fault only when a task REACHES it,
    /// at the use site). The old Option-B reach-gate is gone; safety is by-value deep copy or `Nil`.
    pub(super) fn to_snap(&self, v: Value, memo: &mut WireMemo) -> Result<SnapValue, RuntimeError> {
        self.to_snap_depth(v, 0, memo)
    }

    /// W7-4 — run a SPECULATIVE `to_wire_depth` that the caller may discard, ROLLING BACK `memo` when
    /// it is. Load-bearing for two separate invariants:
    ///  1. **id hygiene** — a discarded attempt that left cell ids in the shared memo would make a later
    ///     global emit `Backref(id)` for an id no emitted `WireValue::Cell` ever defines, and
    ///     `from_wire_memo`'s `.expect` would PANIC on replay.
    ///  2. **handle detection** — a discarded attempt that left a `Backref` shortcut could hide a
    ///     residual `Module`/`Native`/`Cffi` handle from a LATER global's `has_handle()` check, letting a
    ///     parent `GcRef` replay on a worker heap.
    ///
    /// `keep` decides; on `false` (or `Err`) the caller's memo is restored to its entry state.
    ///
    /// Rollback, not a clone: `to_snap_depth` calls this at EVERY node, so cloning the caller's memo
    /// (which for the module snapshot spans every module and grows with every cell already emitted)
    /// would make snapshotting a module with K cell-bearing globals O(M·K) instead of O(M).
    ///
    /// The undo is exact on all four pieces, but they need DIFFERENT undos, and W7-4a is why:
    /// - `cells` / `next_id` — every id this attempt MINTED is `>= mint_from`, and an attempt never
    ///   rewrites an existing entry, so the watermark `retain` is complete.
    /// - `emitted` — the watermark is NOT enough. This attempt can mark an id minted by an EARLIER
    ///   MODULE (ids persist across modules now, `emitted` does not), and such an id is *below* the
    ///   watermark, so `retain` would keep a marking that the discarded encoding made up. The module's
    ///   kept encoding would then emit `Backref(id)` with no definition anywhere in it → a rebuild
    ///   miss → a closure over `nil` → `CellLoad on a non-handle value`. Replayed exactly from
    ///   [`emit_undo`](WireMemo::emit_undo) instead. Fenced by
    ///   `airlock_discarded_wire_attempt_does_not_forge_a_backref`.
    /// - `path` / `gens_on_stack` — empty on entry (only `to_wire_depth` touches them and it pops on
    ///   every `Ok` arm; an `Err` can leave residue, which is what the clears remove).
    ///
    /// Cost is O(cells touched) on the DISCARD path only — a handle-bearing or generator global — and
    /// O(1) on the kept path.
    fn try_wire_speculative(
        &self,
        v: Value,
        depth: usize,
        memo: &mut WireMemo,
        keep: impl Fn(&WireValue) -> bool,
    ) -> Option<WireValue> {
        debug_assert!(memo.path.is_empty() && memo.gens_on_stack.is_empty());
        debug_assert!(!memo.speculating, "try_wire_speculative must not nest");
        let mint_from = memo.next_id;
        memo.emit_undo.clear();
        memo.speculating = true;
        let attempt = self.to_wire_depth(v, depth, memo);
        memo.speculating = false;
        match attempt {
            Ok(w) if keep(&w) => {
                memo.emit_undo.clear();
                Some(w)
            }
            _ => {
                memo.cells.retain(|_, id| *id < mint_from);
                // Newest-first, so an id marked twice in one attempt lands back on its ORIGINAL entry.
                for (id, prev) in memo.emit_undo.drain(..).rev() {
                    match prev {
                        Some(g) => memo.emitted.insert(id, g),
                        None => memo.emitted.remove(&id),
                    };
                }
                memo.next_id = mint_from;
                memo.path.clear();
                memo.gens_on_stack.clear();
                None
            }
        }
    }

    /// Depth-counted worker behind [`Vm::to_snap`] — the M:N module-global crossing path. Shares
    /// [`Vm::to_wire_depth`]'s cyclic-data guard: the same
    /// `MAX_STRUCTURAL_DEPTH` bound turns a cyclic module global into a recoverable `RuntimeError`
    /// (re-stamped with the real nursery span by `ensure_snapshot`) rather than a host `SIGABRT`. The
    /// fast path threads `depth` into `to_wire_depth` and every slow arm recurses at `depth + 1`, so
    /// the shared budget keeps `to_snap` and `to_wire` in lockstep.
    fn to_snap_depth(
        &self,
        v: Value,
        depth: usize,
        memo: &mut WireMemo,
    ) -> Result<SnapValue, RuntimeError> {
        if depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.depth_exceeded_err(Span::default()));
        }
        let h = match v.as_obj() {
            Some(h) => h,
            // A scalar (inline Int/Bool/Nil) or a boxed float (Float tag) is always sendable — never
            // `Obj::Generator`, so this `.expect` is unreachable for a generator. It mints no id, so a
            // throwaway memo is fine here.
            None => {
                return Ok(SnapValue::Wire(
                    self.to_wire_depth(v, depth, &mut WireMemo::default())
                        .expect("scalar is always sendable"),
                ));
            }
        };
        // Fast path: no embedded callable/module → the wire form is exact and cheap. Threads the
        // SHARED `depth` so a cyclic pure-data global trips `to_wire_depth`'s guard at the same budget
        // as the M:N spawn-arg path. A value EMBEDDING A GENERATOR now rides this fast lane too
        // (backlog item B): `to_wire` serializes a generator BY VALUE (F3 path C — proto + backing
        // closure + deep-copied parked slots), and `from_wire` rebuilds a FRESH independent
        // `GeneratorCore` on the WORKER heap (no shared parent `GcRef`), so a per-task by-value copy is
        // memory-safe by construction (each task already gets its own frozen per-task module-global
        // snapshot — F1). A non-sendable parked slot / reference cycle makes
        // `to_wire` Err → we fall to the slow arm, which re-raises that real reject.
        // W7-4: SPECULATIVE — the attempt is discarded when the value carries a handle, so the memo
        // must be rolled back on that branch (`try_wire_speculative`).
        if let Some(w) = self.try_wire_speculative(v, depth, memo, |w| !w.has_handle()) {
            return Ok(SnapValue::Wire(w));
        }
        Ok(match self.heap.get(h).clone() {
            // Boxed scalars: in practice `to_wire_depth` above already handled these (no handle), so
            // this arm is a defensive mirror keeping the match exhaustive — same value as inline.
            Obj::BigInt(n) => SnapValue::Wire(WireValue::Int(n)),
            Obj::FloatBox(f) => SnapValue::Wire(WireValue::Float(f)),
            Obj::Func { proto, home } => SnapValue::Func { proto, home: self.home_index(home) },
            Obj::Closure { proto, captured, home } => {
                // Lever #3: positional captures — carry names from the proto in slot order. A recursive
                // local `fn` (self-cell cycle) reaches this slow arm ONLY if it ALSO embeds a residual
                // `Module` handle (else `to_wire` succeeds with no handle and it rides the
                // SnapValue::Wire fast path with the Backref cycle encoding; `Native`/`Cffi`/`Builtin`
                // cross BY VALUE and never force this arm).
                //
                // W7-4b — that case used to walk the self-cell to `MAX_STRUCTURAL_DEPTH` and "reject
                // cleanly", i.e. FAULT a program CPython runs (`m := k` + a recursive `down`, `41`).
                // The `Obj::Cell` arm below now carries an id and emits `SnapValue::Backref` on the
                // second reach, so the cycle terminates here exactly as it does on the wire path.
                // Fenced by `airlock_handle_bearing_recursive_local_fn_round_trips`.
                let names = &self.program.protos[proto].capture_names;
                let mut snapped = Vec::with_capacity(captured.len());
                for (i, cv) in captured.iter().enumerate() {
                    snapped.push((names.get(i).cloned().unwrap_or_default(), self.to_snap_depth(*cv, depth + 1, memo)?));
                }
                SnapValue::Closure { proto, captured: snapped, home: self.home_index(home) }
            }
            // An import alias bound to another module obj.
            Obj::Module(m) => match self.home_index(h) {
                Some(idx) => SnapValue::ModuleAlias(idx),
                // A module not in `module_objs` (shouldn't occur for a bound import) — encode inline,
                // in slot order so replay rebuilds matching slots.
                None => {
                    let ModuleData { name, slots, index } = *m;
                    let mut globals = Vec::new();
                    for (k, mv) in module_slot_pairs(&slots, &index) {
                        globals.push((k, self.to_snap_depth(mv, depth + 1, memo)?));
                    }
                    SnapValue::ModuleInline { name, globals }
                }
            },
            Obj::Native { name, func, kind } => SnapValue::Native { name, func, kind },
            // A first-class builtin fn is pure code — SENDABLE. Carry the name; the worker re-allocs
            // a fresh `Obj::Builtin` on replay (like `Native`, but with no fn pointer to share).
            Obj::Builtin(name) => SnapValue::Builtin(name),
            // A Cffi shares its `Arc` to the worker (which shares the parent address space): the
            // worker re-allocs `Obj::Cffi` from the SAME Arc — no re-dlopen, no symbol re-resolution.
            Obj::Cffi(c) => SnapValue::Cffi(Arc::clone(&c)),
            // Containers embedding a callable: encode each element. (Pure-data containers took the fast
            // path above.) A generator embedded in any of these faults via the recursive `?`.
            Obj::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for x in &items {
                    out.push(self.to_snap_depth(*x, depth + 1, memo)?);
                }
                SnapValue::List(out)
            }
            Obj::Tuple(items) => {
                let mut out = Vec::with_capacity(items.len());
                for x in &items {
                    out.push(self.to_snap_depth(*x, depth + 1, memo)?);
                }
                SnapValue::Tuple(out)
            }
            Obj::Struct { tid, fields, .. } => {
                // Positional layout: recover declaration-order field names from the StructDef so
                // the SnapValue encoding (which carries names) is unchanged (cold cross-task path).
                // The instance carries only `tid`; resolve the type key from it (the snap format
                // still carries the name string, replay re-derives its tid).
                let name: Box<str> = self.struct_name_of_tid(tid).into();
                let names: Vec<Box<str>> = self
                    .program
                    .structs
                    .get(name.as_ref())
                    .map(|d| d.fields.iter().map(|f| f.clone().into_boxed_str()).collect())
                    .unwrap_or_default();
                let mut out = Vec::with_capacity(fields.len());
                for (i, fv) in fields.iter().enumerate() {
                    let k = names.get(i).cloned().unwrap_or_else(|| i.to_string().into_boxed_str());
                    out.push((k, self.to_snap_depth(*fv, depth + 1, memo)?));
                }
                SnapValue::Struct { name, fields: out }
            }
            Obj::Enum { variant_id, payload } => {
                let mut out = Vec::with_capacity(payload.len());
                for x in &payload {
                    out.push(self.to_snap_depth(*x, depth + 1, memo)?);
                }
                // M19 lever #2 — carry the dense `variant_id` directly on the cold snap path (mirrors
                // `to_wire`); replay reuses it as-is against the shared program.
                SnapValue::Enum { variant_id, payload: out }
            }
            Obj::NewType { type_key, inner } => SnapValue::NewType {
                type_key,
                inner: Box::new(self.to_snap_depth(inner, depth + 1, memo)?),
            },
            Obj::Map(m) => {
                let mut out = Vec::with_capacity(m.entries.len());
                for (hash, k, val) in &m.entries {
                    out.push((
                        *hash,
                        self.to_snap_depth(*k, depth + 1, memo)?,
                        self.to_snap_depth(*val, depth + 1, memo)?,
                    ));
                }
                SnapValue::Map(out)
            }
            Obj::Set(s) => {
                let mut out = Vec::with_capacity(s.entries.len());
                for (hash, e) in &s.entries {
                    out.push((*hash, self.to_snap_depth(*e, depth + 1, memo)?));
                }
                SnapValue::Set(out)
            }
            // Leaf data / cores are handled by the fast path; if `to_wire` ever errored above we land
            // here for a `str`/core (always sendable) — encode its wire form. A generator is
            // `Obj::Generator` (handled below), never one of these, so this `.expect` is unreachable
            // for a generator.
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            // R2: a `Writer` handle is always sendable (crosses by value as `WireValue::Writer` — an
            // `Arc`'d core, no `GcRef`), like `Socket`.
            | Obj::Writer(_)
            // R2b: a `Reader` handle is always sendable (crosses by value as `WireValue::Reader`).
            | Obj::Reader(_)
            // An opaque `ptr` is always sendable (crosses by value as `WireValue::Ptr`); normally the
            // fast path catches it, but it is a valid leaf here too.
            | Obj::Ptr(_) => {
                SnapValue::Wire(self.to_wire(v).expect("str / bytes / bytearray / channel / shared / atomic / executor / socket / writer / reader / ptr is always sendable"))
            }
            // Backlog item B — a module-global live generator crosses BY VALUE, exactly like a
            // frame-local one. A generator reaches this SLOW arm only because the fast-path `to_wire`
            // either errored (a non-sendable parked slot / a reference cycle) OR the value had a
            // `has_handle()` sibling (a container embedding both a handle-bearing value AND this
            // generator: the whole value fails the fast lane, but the generator itself may still be
            // sendable). Re-run `to_wire` for this node:
            //   - Ok + no handle  → encode the by-value wire copy; `from_wire` rebuilds a fresh
            //     independent `GeneratorCore` on the worker heap (the feature).
            //   - otherwise (non-sendable parked slot / reference cycle / a parked module/native/FFI
            //     handle) → snapshot an inert `Nil` placeholder, NOT the `?`-propagated reject. Emitting
            //     `Wire(w)` for a handle-bearing generator would replay a parent `GcRef` on the worker
            //     (the memory-safety hole), so it must not cross — but eager-faulting the WHOLE snapshot
            //     here would abort every `spawn` in a module that merely *holds* a non-sendable generator
            //     it never sends (a regression: `snapshot_modules` walks EVERY global once, reached or
            //     not). `Nil` keeps the snapshot infallible and inert: a task that never touches the
            //     generator runs clean, and one that DOES reach it faults at the use site (iterating a
            //     `Nil` is not iterable) — "fault only when reached" by construction (every task
            //     snapshots from the same memoized frozen copy).
            // W7-4: also SPECULATIVE (the `Nil` branch discards it) — same rollback discipline, and
            // it must use the SHARED memo on the kept branch or its ids would collide with the ones the
            // module's other globals minted, aliasing unrelated nodes on replay.
            Obj::Generator(_) => SnapValue::Wire(
                self.try_wire_speculative(v, depth, memo, |w| !w.has_handle())
                    .unwrap_or(WireValue::Nil),
            ),
            // A `Cell` embedding a handle snaps like a 1-field box (its inner recursively snapped) —
            // replayed as ONE independent cell per BINDING (design §4 F1). A pure-data cell took the
            // `to_wire` fast path above (`WireValue::Cell`).
            //
            // W7-4b — the id/`Backref` dance is the SAME as `to_wire_depth`'s `Obj::Cell` arm and
            // shares its memo, so a cell reached twice (two sibling closures, or a letrec back-edge)
            // rebuilds once whichever path each reference travelled. Only `Obj::Module` forces this
            // slow arm today (`has_handle`; `Native`/`Cffi`/`Builtin` all cross by value), and it is
            // source-reachable: `p := [k]` captured by two closures over one binding read `1` where
            // CPython measures `3`.
            Obj::Cell(v) => {
                let id = match memo.cell_id(h) {
                    Some(id) => id,
                    None => {
                        let id = memo.next_id;
                        memo.next_id += 1;
                        memo.cells.insert(h, id);
                        id
                    }
                };
                if memo.emitted.get(&id) == Some(&memo.elem_gen) {
                    SnapValue::Backref(id)
                } else {
                    // No `emit_undo` journal here, unlike `to_wire_depth`'s twin: this arm is only
                    // ever reached AFTER `try_wire_speculative` has already returned (it runs
                    // `to_wire_depth`, never `to_snap_depth`), so `speculating` is false and this
                    // marking is never part of an attempt that can be thrown away.
                    memo.emitted.insert(id, memo.elem_gen);
                    SnapValue::Cell {
                        id,
                        inner: Box::new(self.to_snap_depth(v, depth + 1, memo)?),
                    }
                }
            }
            // A cursor snapshots like a `List`: its items (recursively snapped) + `pos`. Only a
            // handle-bearing cursor reaches here; a pure-data cursor took the `to_wire` fast path.
            Obj::Iter { items, pos } => {
                let mut out = Vec::with_capacity(items.len());
                for x in &items {
                    out.push(self.to_snap_depth(*x, depth + 1, memo)?);
                }
                SnapValue::Iter { items: out, pos }
            }
        })
    }

    /// D1 — install a shared [`ModuleSnapshot`] into a freshly-built worker: pre-alloc one **empty**
    /// `Module` per snapshot entry (index order preserved so a callable's home index lines up), seed
    /// the per-module faulted flags, and keep the `Arc` so each module's globals fault in lazily on
    /// first access ([`Vm::fault_module`]). The cheap replacement for eager `build_worker_modules`.
    pub(super) fn install_snapshot(&mut self, snap: Arc<ModuleSnapshot>) {
        debug_assert!(
            self.module_objs.is_empty(),
            "install_snapshot expects a fresh worker"
        );
        for m in &snap.modules {
            let wm = self.heap.alloc(Obj::Module(Box::new(ModuleData {
                name: m.name.clone(),
                slots: Vec::new(),
                index: std::collections::HashMap::new(),
            })));
            self.module_objs.push(wm);
        }
        self.module_faulted = vec![false; snap.modules.len()];
        // W7-4a — a fresh view rebuilds from scratch: ids are minted per snapshot, so carrying a
        // previous view's map in would tie this view's cells to another heap's `GcRef`s.
        self.snapshot_rebuild.clear();
        // W7-4c — likewise the registry: its keys are `GcRef`s in the heap the snapshot was BUILT
        // from, which is not this worker's heap. A worker therefore starts with no registry, so a
        // NESTED nursery's tasks fall back to pre-W7-4c behavior (two bindings) until this worker
        // builds a snapshot of its own — a missed optimisation, never a wrong merge.
        self.snapshot_cells = Arc::new(super::fxhash::FxHashMap::default());
        // W6-2 — seed the cache from the snapshot being installed: this view IS a faithful replay of
        // `snap`, so a nested `spawn` that changed nothing reuses it for free instead of materializing
        // + re-walking every global. The two invalidation rules still apply to it: a slot write drops it,
        // and a nested `parallel:` drops it unless `reusable` — which is what makes a task that mutated
        // its own copy in place re-snapshot for its children (`slot_snapshot_reusable`).
        self.snapshot_memo = Some(Arc::clone(&snap));
        self.module_snapshot = Some(snap);
    }

    /// D1 — fault module `idx`'s globals into this worker's heap from the snapshot, the first time any
    /// global of that module is read. Idempotent (guarded by `module_faulted`); the flag is set
    /// *before* replaying so a self-referential global (e.g. a [`SnapValue::ModuleAlias`] back to this
    /// same module) resolves to the already-alloced module obj without re-entering. No-op once faulted.
    pub(super) fn fault_module(&mut self, idx: usize) {
        if self.module_faulted[idx] {
            return;
        }
        self.module_faulted[idx] = true;
        let snap = Arc::clone(
            self.module_snapshot
                .as_ref()
                .expect("worker has a snapshot"),
        );
        let module = self.module_objs[idx];
        // W6-2 — a replay REPRODUCES the snapshot, it does not mutate the view, so its `module_define`s
        // must not drop the cache this view was seeded with (`install_snapshot`).
        let memo = self.snapshot_memo.take();
        // W7-4c — `module_define` clears the registry (it is a slot mutator); a replay REPRODUCES the
        // snapshot rather than mutating the view, so the registry rides the same take/restore the
        // cache does.
        let saved_cells = std::mem::take(&mut self.snapshot_cells);
        // W7-4a: ONE rebuild map for the WHOLE view's replay, mirroring the one `WireMemo`
        // `snapshot_modules` now spans every module with (scope invariant — see `deep_clone_all`), so
        // two globals over one captured local rebuild ONE cell whether they live in the same module or
        // not. Each module re-emits a shared cell's full definition under the same id and
        // `from_wire_memo` dedupes first-wins, so lazy fault ORDER does not matter and a module that
        // never faults costs nothing — PROVIDED the serialize side really did write a definition in
        // every module that references the cell, which is what `try_wire_speculative`'s `emit_undo`
        // journal guarantees (a discarded attempt must not leave a forged `emitted` marking).
        // Taken out of `self` for the borrow and put back below; it lives
        // on the `Vm` (rooted by `collect`) because a cell built by this fault can sit in it
        // across a safepoint before a later module's fault ties to it.
        let mut rb = std::mem::take(&mut self.snapshot_rebuild);
        // W7-4b — this replay is a WHOLE crossing (the module's globals under the one memo that
        // serialized them), so every `Backref` must resolve. Own the flag like `from_wire`/
        // `from_wire_piece` do: clear it going in, assert it going out, clear it again so a snapshot
        // miss is never charged to the next unrelated `from_wire` caller's assert. Loud in debug; in
        // release the miss still degrades to `nil` rather than aborting the host.
        self.wire_backref_missing = false;
        for (name, sv) in &snap.modules[idx].globals {
            let val = self.replay_snap(sv, &mut rb);
            self.module_define(module, name, val);
        }
        debug_assert!(
            !self.wire_backref_missing,
            "fault_module: a module global's replay hit a dangling Backref — a discarded speculative \
             attempt forged one, or the serialize memo's scope no longer matches this rebuild map"
        );
        self.wire_backref_missing = false;
        debug_assert!(
            self.snapshot_rebuild.is_empty(),
            "fault_module re-entered while its rebuild map was taken — the nested fault built cells \
             against a map that is about to be dropped"
        );
        // W7-4a — keep ONLY the cells past this module. `from_wire_memo` registers EVERY
        // identity-preserved node it rebuilds (List/Map/Set/Struct/Tuple/Closure too), but only a cell
        // can be back-referenced from a LATER module: containers live in the `path` map, which pops on
        // DFS exit, so a container reached again in another module is serialized fresh under a NEW id
        // and can never resolve against this one. Retaining them would make the whole module-global
        // object graph immortal for the fiber's life (the map is `Vm`-lived AND a GC root, so a task
        // that reassigns a big global would keep the original rooted — a `--max-heap` regression).
        rb.retain(|_, &mut h| matches!(self.heap.get(h), Obj::Cell(_)));
        self.snapshot_rebuild = rb;
        self.snapshot_memo = memo;
        self.snapshot_cells = saved_cells;
    }

    /// D1 — if this is a worker VM (a snapshot is installed), ensure the module that owns `home` has
    /// been faulted in before its globals are read. No-op on the top-level VM, and on a fiber with no
    /// heap of its own (no snapshot — `module_objs` are the real, already-populated modules), so those
    /// views are untouched. Called at every module-global read site (`GetGlobal`, the `GetCaptured`
    /// home fallback, module member access, and a `module.fn(...)` call).
    pub(super) fn ensure_module_faulted(&mut self, home: GcRef) {
        if self.module_snapshot.is_none() {
            return;
        }
        if let Some(idx) = self.module_objs.iter().position(|&m| m == home) {
            self.fault_module(idx);
        }
    }

    /// D1 — replay a [`SnapValue`] into this worker's heap (the inverse of [`Vm::to_snap`]): the
    /// snapshot is shared behind an `Arc`, so this borrows and clones leaf data (`WireValue`, fn
    /// pointer) rather than moving. `ModuleAlias(idx)` resolves to the pre-alloced `module_objs[idx]`
    /// — which faults its own globals lazily on first access, so no eager cascade.
    /// W7-4: `rb` is the module-scoped wire-`id` → placeholder `GcRef` rebuild map (see
    /// [`fault_module`](Vm::fault_module)); it must span exactly the globals whose serialization shared
    /// one [`WireMemo`], or a `Backref` hits `from_wire_memo`'s `.expect`.
    pub(super) fn replay_snap(
        &mut self,
        snap: &SnapValue,
        rb: &mut super::fxhash::FxHashMap<u32, GcRef>,
    ) -> Value {
        match snap {
            SnapValue::Wire(w) => self.from_wire_memo(w.clone(), rb),
            SnapValue::Func { proto, home } => {
                let whome = self.worker_home(*home);
                Value::obj(self.heap.alloc(Obj::Func {
                    proto: *proto,
                    home: whome,
                }))
            }
            SnapValue::Closure {
                proto,
                captured,
                home,
            } => {
                let whome = self.worker_home(*home);
                // Lever #3: rebuild positionally (slot order), discarding the carried names.
                let cap: Vec<Value> = captured
                    .iter()
                    .map(|(_k, cv)| self.replay_snap(cv, rb))
                    .collect();
                Value::obj(self.heap.alloc(Obj::Closure {
                    proto: *proto,
                    captured: cap,
                    home: whome,
                }))
            }
            SnapValue::ModuleAlias(idx) => Value::obj(self.module_objs[*idx]),
            SnapValue::ModuleInline { name, globals } => {
                let wm = self.heap.alloc(Obj::Module(Box::new(ModuleData {
                    name: name.clone(),
                    slots: Vec::new(),
                    index: std::collections::HashMap::new(),
                })));
                for (k, gv) in globals {
                    let val = self.replay_snap(gv, rb);
                    self.module_define(wm, k, val);
                }
                Value::obj(wm)
            }
            SnapValue::Native { name, func, kind } => Value::obj(self.heap.alloc(Obj::Native {
                name: name.clone(),
                func: *func,
                kind: *kind,
            })),
            // Re-alloc a fresh `Obj::Builtin` from the carried name (pure code, no state to share).
            SnapValue::Builtin(name) => Value::obj(self.heap.alloc(Obj::Builtin(name.clone()))),
            // Re-alloc from the SAME shared `Arc<Cffi>` — no re-dlopen (shared address space).
            SnapValue::Cffi(c) => Value::obj(self.heap.alloc(Obj::Cffi(Arc::clone(c)))),
            SnapValue::List(xs) => {
                let v = xs.iter().map(|x| self.replay_snap(x, rb)).collect();
                Value::obj(self.heap.alloc(Obj::List(v)))
            }
            SnapValue::Iter { items, pos } => {
                let v = items.iter().map(|x| self.replay_snap(x, rb)).collect();
                Value::obj(self.heap.alloc(Obj::Iter {
                    items: v,
                    pos: *pos,
                }))
            }
            SnapValue::Tuple(xs) => {
                let v = xs.iter().map(|x| self.replay_snap(x, rb)).collect();
                Value::obj(self.heap.alloc(Obj::Tuple(v)))
            }
            SnapValue::Struct { name, fields } => {
                // Positional layout: the snap fields are in declaration order (to_snap emits them
                // so), so rebuild positionally — the carried names are discarded.
                let f: Vec<Value> = fields
                    .iter()
                    .map(|(_, fv)| self.replay_snap(fv, rb))
                    .collect();
                let tid = self.struct_tid(name);
                Value::obj(self.heap.alloc(Obj::Struct {
                    tid,
                    fields: Fields::from_vec(f),
                }))
            }
            SnapValue::Enum {
                variant_id,
                payload,
            } => {
                let p = payload.iter().map(|x| self.replay_snap(x, rb)).collect();
                // M19 lever #2 — the dense `variant_id` was carried directly (mirrors `from_wire`);
                // replay it as-is against the shared program — no lossy name re-resolution.
                Value::obj(self.heap.alloc(Obj::Enum {
                    variant_id: *variant_id,
                    payload: p,
                }))
            }
            SnapValue::NewType { type_key, inner } => {
                let inner = self.replay_snap(inner, rb);
                Value::obj(self.heap.alloc(Obj::NewType {
                    type_key: type_key.clone(),
                    inner,
                }))
            }
            // Rebuild ONE independent cell per binding on the worker (deep copy, never shared with the
            // parent) — design §4 F1. W7-4b: mirrors `from_wire_memo`'s `Cell` arm exactly — first-wins
            // dedupe by id (so a repeated definition from another module ties to the cell already
            // built), and the placeholder is registered BEFORE recursing so a cycle through this cell
            // resolves to it instead of recursing forever.
            SnapValue::Cell { id, inner } => {
                if let Some(&prev) = rb.get(id) {
                    return Value::obj(prev);
                }
                let h = self.heap.alloc(Obj::Cell(Value::nil()));
                rb.insert(*id, h);
                let inner = self.replay_snap(inner, rb);
                *self.heap.get_mut(h) = Obj::Cell(inner);
                Value::obj(h)
            }
            // W7-4b — the far side of a second reach. A miss is a memo-scope bug, not a program error:
            // degrade to `nil` and flag it (W7-11) rather than aborting the host.
            SnapValue::Backref(id) => match rb.get(id) {
                Some(&h) => Value::obj(h),
                None => {
                    self.wire_backref_missing = true;
                    Value::nil()
                }
            },
            SnapValue::Map(entries) => {
                let mut out = MapData::default();
                for (hash, k, val) in entries {
                    let (ck, cv) = (self.replay_snap(k, rb), self.replay_snap(val, rb));
                    out.push(*hash, ck, cv);
                }
                Value::obj(self.heap.alloc(Obj::Map(out)))
            }
            SnapValue::Set(entries) => {
                let mut out = SetData::default();
                for (hash, e) in entries {
                    let ce = self.replay_snap(e, rb);
                    out.push(*hash, ce);
                }
                Value::obj(self.heap.alloc(Obj::Set(out)))
            }
        }
    }
}

/// W8-8 — the wid range of the pool helpers an outermost eager nursery farms. wid 0 is the inline
/// joiner and wid 1 the raw `chezzi-eager` drainer, both unconditional threads, so helpers start at
/// 2 and the range end is the whole runner budget.
pub(super) fn eager_helper_wids(n: usize) -> std::ops::Range<usize> {
    2..n.max(2)
}

/// W8-8 — the inline joiner runs fibers only when the budget has a slot left after the drainer.
/// At `n == 1` the drainer already holds the only slot, so the joiner just waits for completion —
/// otherwise `--threads=1` runs two CPU runners and does not serialize.
///
/// T1-fix — a hazard this gate introduces, not fixed here: at `n == 1` the joiner no longer runs any
/// fiber loop of its own, so it is now purely a spectator waiting on the drainer thread. A panic
/// inside `take_runnable`/`park`/`finish` itself (outside `run_one_fiber`'s own inner
/// `catch_unwind`) is swallowed by the drainer thread's OUTER `catch_unwind`
/// (`activate_eager_nursery`'s `spawn(move || { catch_unwind(...) })`); the thread then exits with
/// its scope's slots unfilled, and at `n == 1` there is nothing else left to fill them, so the
/// joiner's `wait_for_completion`/`wait_for_scope` blocks forever. Pre-W8-8 the joiner's own fiber
/// loop covered that. At `n >= 2` the cover differs BY ARM and only one of them has a fallback: the
/// OUTERMOST arms farm pool helpers (`farm_outermost_eager_helpers`, called from
/// `join_eager_nursery` alone) so a dead drainer still leaves runners behind, while the NESTED arms
/// farm nothing at all — there the joiner's own loop IS the only cover, so the window is closed at
/// `n >= 2` purely because the gate lets that loop run. Requires a pre-existing scheduler bug to
/// reach, so no code change here — recorded so the next reader sees the trade.
pub(super) fn eager_joiner_runs_fibers(n: usize) -> bool {
    n >= 2
}

/// EAGER `submit` (M:N), the atomic half — reserve this job's submission slot and hand it to the
/// bounded pool. Called with the executor's `inner` lock HELD (see `Vm::executor_method`), so it must
/// stay allocation-free and must not re-enter the VM: it takes only the separate `eager` lock, giving
/// a fixed inner → eager order that a finishing job (which takes `eager` alone) can never invert.
///
/// The slot is reserved BEFORE the job reaches the pool, so [`EagerState`]'s slots are in SUBMISSION
/// order regardless of completion order — that is what lets `shutdown` reduce them with the shared
/// [`Vm::reduce_task_slots`] and inherit the whole W7-5/W7-5c contract (decision F output order,
/// lowest-index fault, hard-halt precedence, per-slot flush) instead of re-deriving it.
pub(super) fn dispatch_eager_job(
    core: &Arc<ExecutorCore>,
    rw: ReadyWorker,
    mem_cap: usize,
    pending: usize,
    sched_registry: &crate::vm::SchedRegistry,
) {
    let span = rw.span;
    // W7-26r sibling — this job's rebuilt worker heap belongs to the SUBMITTER until a pool thread
    // picks it up (see `ExecutorCore::pending`). `0` when the cap is off, so this is a no-op then.
    core.pending
        .fetch_add(pending, std::sync::atomic::Ordering::Relaxed);
    let idx = core
        .eager
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .reserve();
    let core = Arc::clone(core);
    let sched_registry = Arc::clone(sched_registry);
    pool::submit(Box::new(move || {
        // W7-26r sibling — the job leaves the queue HERE: from this point its bytes are the worker
        // heap's own (charged against the worker's copy of the cap by its own sweeps), so the
        // submitter must stop counting them or the two double-count. Before `catch_unwind`, so a
        // panicking job cannot leave the charge stranded on the core forever.
        core.pending
            .fetch_sub(pending, std::sync::atomic::Ordering::Relaxed);
        // A Rust panic in the worker VM becomes a `Fault` slot rather than unwinding into the pool
        // thread and leaving `outstanding` short — which would hang `shutdown`'s condvar wait forever.
        // Everything after the `catch_unwind` is panic-free (an in-range `Vec` index; the lock is
        // poison-tolerant), so the outcome is always recorded and `outstanding` always reaches 0.
        // This is the invariant B3.3-threads' retired `DoneSignal` guard used to carry for the batch
        // join; `executor_faulting_job_does_not_hang_shutdown` covers it.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rw.run_outcome()))
            .unwrap_or_else(|p| TaskOutcome::Fault {
                err: panic_to_fault(p, span),
                out: Vec::new(),
                stderr: Vec::new(),
            });
        // W7-26 — summarised for the `--max-heap` byte walk BEFORE the lock is taken: the walk is
        // O(result) and this lock is contended by every `submit` (`reserve`, below, runs while the
        // submitter holds `inner`) and by every `live_bytes`. Same hoist as `SharedCore::store`.
        let sum = crate::vm::core::outcome_summary(&outcome);
        // W7-26r — the finishing job is the only party that can observe the cap while the submitter
        // sits in `shutdown()`'s join (see `core::halt_over_backlog`). `bytes` is this executor's
        // whole retained backlog INCLUDING this outcome, so the trip means the results alone are
        // over the cap; the replacement outcome is the same size, leaving `sum` accurate.
        let over = {
            let mut g = core.eager.lock().unwrap_or_else(|e| e.into_inner());
            let backlog = g.summary().0 + sum.0;
            let (outcome, over) = crate::vm::core::halt_over_backlog(outcome, backlog, mem_cap);
            g.finish(idx, sum, outcome);
            over
        };
        if over {
            // Stop the siblings still feeding the backlog — the `shutdown_now` idiom (D4: cooperative,
            // a job with no cancellation point still runs to completion). Without it the remaining
            // jobs keep allocating while the join drains, which is what the abort exists to prevent.
            core.cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        core.eager_cv.notify_all();
        // W7-56 — this job's `outstanding` just dropped, so it no longer vetoes any nursery's
        // deadlock predicate (`MnSched::is_deadlocked`). Poke every live sched so an idle worker
        // re-evaluates: without this, a job that ends WITHOUT sending leaves the veto's consumers
        // asleep forever (idle workers `cv.wait` untimed) — turning a program that correctly faults
        // `deadlock` today into a silent hang.
        //
        // The lock is taken and dropped rather than a bare `notify_all`, and that is what makes it
        // reliable: a worker that read `outstanding == 1` inside `is_deadlocked` did so under this
        // same core lock, so acquiring it here happens-after that read completes — the worker is
        // either already on the condvar (and gets the notify) or has not yet taken the lock (and
        // will read the new count). A bare notify could land in the gap and be lost.
        {
            let mut g = sched_registry.lock().unwrap_or_else(|e| e.into_inner());
            let live: Vec<_> = g.iter().filter_map(|w| w.upgrade()).collect();
            g.retain(|w| w.strong_count() > 0);
            drop(g);
            for s in live {
                drop(s.lock());
                s.cv.notify_all();
            }
        }
    }));
}
