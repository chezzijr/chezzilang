//! VM: spawn and the nursery lifecycle: `do_spawn`, task entry and registration, join and escape,
//! M:N and eager nurseries.
//! Split out of `src/vm/sched.rs`. `super::*` is the parent module.

use super::sched::JoinCover;
use super::sched::NestedDrainerSlot;
use super::sched::SpawnHead;
use super::sched::SpawnTarget;
use super::sched::cancel_fiber_owned_family;
use super::sched::join_helpers;
use super::sched::spawn_worker_thread;
use super::*;
use crate::base::crossing::{self};

impl Vm {
    /// `spawn f(args)` / `spawn recv.m(args)` — pop `argc(+1)` operands, deep-copy the args (and, for
    /// the method form, the receiver) across the airlock, and register the task on the innermost
    /// nursery. The callee passes by handle (like `defer`); only data crosses the airlock. `fresh`
    /// names the checker's operand shapes (see [`crate::base::op::Op::SpawnRecv`]), carried to
    /// [`Vm::rebuild_ready`].
    pub(super) fn do_spawn(
        &mut self,
        kind: SpawnHead,
        argc: usize,
        fresh: u32,
        span: Span,
    ) -> Result<(), RuntimeError> {
        self.sched_seed_point();
        let at = self.stack.len() - argc;
        let raw_args: Vec<Value> = self.stack.split_off(at);
        let head = self.pop();
        if self.nurseries.is_empty() {
            return Err(self.err("spawn must be inside a parallel: block".to_string(), span));
        }
        self.spawn_into(SpawnTarget::Nursery, kind, head, raw_args, fresh, span)
    }

    /// TICKET-235 — does calling `v` push a Chezzi frame that the task then runs in? True for a
    /// non-generator fn or closure. A native fn, a builtin, a ctor and a generator fn push none:
    /// a park inside one would have no `ip` to rewind ([`Vm::rewind_op`]).
    pub(super) fn enters_frame(&self, v: Value) -> bool {
        matches!(
            self.callable(v),
            Some(Callee::Func { proto, .. } | Callee::Closure { proto, .. })
                if !self.program.protos[proto].is_generator
        )
    }

    /// TICKET-235 — THE decision of how a spawned call becomes a task body; every task it yields
    /// starts in a frame. A head that pushes its own frame runs directly, unchanged. Any other
    /// head (a receiver, a native fn value, a builtin, a ctor, a generator fn) becomes argument 0
    /// of the site's entry thunk, and `fresh` says so ([`crossing::behind_entry`]). A
    /// non-callable callee passes through, so `lower_task` reports it as it always did.
    fn task_entry(
        &mut self,
        kind: SpawnHead,
        head: Value,
        mut args: Vec<Value>,
        fresh: u32,
        span: Span,
    ) -> Result<(Value, Vec<Value>, u32), RuntimeError> {
        let proto = match kind {
            SpawnHead::Callee(_) if self.enters_frame(head) || self.callable(head).is_none() => {
                return Ok((head, args, fresh));
            }
            SpawnHead::Callee(None) => {
                return Err(self.err(
                    "internal error: a spawned call has no entry thunk".to_string(),
                    span,
                ));
            }
            SpawnHead::Callee(Some(p)) | SpawnHead::Receiver(p) => p,
        };
        // The thunk value is built on the PARENT side, the way `Op::MakeFunc` builds any fn value.
        let home = self.frames.last().expect("a spawn runs in a frame").home;
        let entry = self.fn_value(FnKey::Proto(proto), || Obj::Func { proto, home });
        args.insert(0, head);
        Ok((entry, args, crossing::behind_entry(fresh)))
    }

    /// Start one task: the ONE function behind `spawn` and `Executor.submit` (TICKET-208). It pins
    /// the task's module view ([`Vm::fresh_view`]), crosses the callee and its arguments in one
    /// serialization, and hands the task to `target`. Everything a new party inherits from its
    /// starter is decided here or below, once, whichever door started it.
    pub(super) fn spawn_into(
        &mut self,
        target: SpawnTarget,
        kind: SpawnHead,
        head: Value,
        raw_args: Vec<Value>,
        fresh: u32,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let (head, raw_args, fresh) = self.task_entry(kind, head, raw_args, fresh, span)?;
        // W7-4: everything that crosses here crosses in ONE serialization, so two sibling closures
        // over the SAME captured local (`spawn work(c.inc, c.get)`) still share their one binding on
        // the far side. Root-by-root `deep_clone` gave each its own [`WireMemo`] → one cell per
        // REFERENCE, silently splitting the binding. A capture-free `spawn f(…)` callee still keeps
        // the cheap shared handle (see [`spawn_callee_crosses_deep`](Vm::spawn_callee_crosses_deep)),
        // so it stays out of the batch.
        //
        // ARGS FIRST, callee LAST — the pre-batch order (every arg was `deep_clone`d, then
        // `cross_spawn_callee`). Serialization order is observable when two of them are
        // non-crossable in DIFFERENT ways (a depth-cap arg vs a reference-cycle callee): the first
        // failure is the reported fault. `lower_task` keeps the same args-before-captures order.
        // A head behind an entry thunk is argument 0 (TICKET-235), so a receiver is serialized
        // before the arguments.
        //
        // W7-4c — PIN THE SNAPSHOT FIRST. The clone below mints fresh cells, and they can only be tied
        // to the module snapshot's ids if those ids already exist; pinning inside `register_task`
        // (below) is too late for the first spawn of a view. No user code runs between here and there,
        // so the pinned VALUES are identical — only which fault wins changes when BOTH the snapshot
        // build and the crossing are non-viable, and the snapshot's fault is the more fundamental one.
        let pin = self.fresh_view(span);
        let cross_head = self.spawn_callee_crosses_deep(head);
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
        let task = PendingCall::Call {
            callee: head,
            args,
            span,
        };
        match target {
            SpawnTarget::Nursery => self.register_task(task, span, pin, cell_ids, fresh),
            SpawnTarget::Scope { sched, settle } => {
                let rw = self.prepare_worker(task, pin?, &cell_ids, fresh)?;
                // W7-26r — the worker heap this start just built is owned by the starter until the
                // task FINISHES (DEC-205). `own_bytes`, never `live_bytes`: the full walk counts
                // `Arc`-shared payloads the starter already counts. Under a live cap only.
                let charge = if self.heap.mem_cap() != 0 {
                    rw.worker.heap.own_bytes()
                } else {
                    0
                };
                let mut fiber = rw.into_fiber(0, 0);
                // Reserve the slot and claim the runners it needs in ONE core-lock hold. A scope
                // whose join already closed it takes no new task: that is what makes a submit
                // racing a `shutdown()` either rejected or counted by that join.
                let wids = {
                    let mut c = sched.lock();
                    if !c.scopes.get(0).is_some_and(|s| s.body_open) {
                        drop(c);
                        return Err(self.err(
                            "submit on a shut-down Executor (it no longer accepts work)"
                                .to_string(),
                            span,
                        ));
                    }
                    sched.reserve_job_slot(&mut c, &mut fiber, charge, settle);
                    sched.claim_runners(&mut c)
                };
                // TICKET-230 — once another runner exists, main hands its permit over at slice
                // ends, at every worker count (one runner budget of N, main included).
                if self.mn.is_none() && !wids.is_empty() {
                    self.slice_in_place = true;
                }
                let runner = self.start_runners(&sched, wids);
                if !runner {
                    // No thread can run it: fill the slot so no join waits for it.
                    {
                        let mut c = sched.lock();
                        c.running += 1;
                        c.exec_active += 1;
                    }
                    sched.finish(
                        fiber.task_index,
                        fiber.scope_id,
                        TaskOutcome::Cancelled {
                            out: Vec::new(),
                            stderr: Vec::new(),
                        },
                    );
                    return Err(self.err(super::EXEC_NO_RUNNER_MSG.to_string(), span));
                }
                sched.submit_job(fiber);
                // W7-26, the sampling half — charge what this Executor retains, and the task just
                // started, against this heap's GC pacing counter, so a live cap gets sampled.
                if self.heap.mem_cap() != 0 {
                    self.heap.charge_bytes(sched.take_charge() + charge);
                }
                Ok(())
            }
        }
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
            };
            captured.push(v);
        }
        // Deep-copy across the airlock: the task can't share mutable state with the parent.
        // Positional (lever #3): slot order matches the synthetic block proto's `capture_names`.
        // W7-4: ONE serialization for all captures — two captured closures over the same local
        // (`gi := c.inc; gg := c.get`) must reach the task sharing their single binding.
        // W7-4c: and the snapshot is pinned BEFORE the clone, so a capture over a cell a module global
        // also holds crosses under that global's id — see `do_spawn`.
        if self.nurseries.is_empty() {
            return Err(self.err("spawn must be inside a parallel: block".to_string(), span));
        }
        let pin = self.fresh_view(span);
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
            0,
        )
    }

    /// The module view a task started NOW inherits: the one door every `spawn` and every
    /// `Executor.submit` reads its globals through (TICKET-208).
    ///
    /// A cached snapshot that is not `reusable` holds a mutable aggregate, which an in-place
    /// mutation (`q.push(1)`, `m[k] = v`, `p.x = 1`) changes with no module-slot write for
    /// `set_global_slot` / `module_define` to see. It is dropped when its `view` stamp is no longer
    /// this heap's `(id, view_epoch)`: something a global reaches was handed out mutably since the
    /// build (`Heap::get_mut`), or the snapshot was built from another heap. So a task sees its
    /// globals as of its own start (Go: a goroutine sees the heap as of `go`), and a spawn storm
    /// that mutates nothing shares ONE snapshot (TICKET-213: one per spawn held 2000 copies of a
    /// 100000-item global at one worker). An all-immutable view keeps its one snapshot for the
    /// whole run. This is the ONE site that decides it.
    ///
    /// A `spawn` calls this BEFORE `deep_clone_all` mints the task's cells, and CARRIES the
    /// `Result` (W6-2): a snapshot that cannot be built — a module global holding a frame-holding
    /// generator — faults where the task is PREPARED, so a nursery whose tasks are all cancelled
    /// before preparation stays faultless.
    pub(super) fn fresh_view(&mut self, span: Span) -> Result<Arc<ModuleSnapshot>, RuntimeError> {
        // TICKET-246 — a box context seats its OWN view from its caller before it snapshots.
        self.box_seat_view(span)?;
        let now = (self.heap.id(), self.heap.view_epoch());
        if self
            .snapshot_memo
            .as_ref()
            .is_some_and(|s| !s.reusable && s.view != now)
        {
            self.snapshot_memo = None;
            // W7-4c — the registry numbers that snapshot; drop it with the cache.
            self.snapshot_cells = Arc::new(super::fxhash::FxHashMap::default());
            // TICKET-111 — the node registry numbers the same snapshot; drop it too.
            self.snapshot_nodes = Arc::new(super::fxhash::FxHashMap::default());
        }
        self.ensure_snapshot(span)
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
        fresh: u32,
    ) -> Result<(), RuntimeError> {
        // The innermost open nursery (`nurseries`, `mn_scopes` and `eager_scheds` are lockstep).
        // W7-4c — `snap` was pinned by `pin_snapshot`, which already ran this guard BEFORE the
        // caller's `deep_clone_all`; re-checking here keeps the invariant local and costs one compare.
        let Some(i) = self.nurseries.len().checked_sub(1) else {
            return Err(self.err("spawn must be inside a parallel: block".to_string(), span));
        };
        // Eager innermost nursery → inject a live fiber. Clone the sched Arc, drop the borrow so
        // `prepare_worker` can take `&mut self`; `inject_or_extend` assigns the real slot index under
        // its lock (the `0` placeholder is overwritten), so no caller-side index bookkeeping is needed.
        if let Some(Some(scope)) = self.eager_scheds.last() {
            let sched = Arc::clone(&scope.sched);
            // TICKET-103 — every eager scope may have a later scope registered on its sched by a
            // fiber (`activate_fiber_owned_nursery`), so a spawn goes to the nursery's TAIL scope, or
            // opens a continuation scope when the tail is no longer the sched's last. This path
            // PREPARES the task right here, so a snapshot build failure surfaces right here too
            // (prepare instant).
            let tail = scope.more_scopes.last().copied().unwrap_or(scope.scope);
            let fiber = self
                .prepare_worker(task, snap?, &cell_ids, fresh)?
                .into_fiber(0, tail);
            let (opened, wids) = sched.inject_or_extend(fiber, tail);
            if let Some(n) = opened
                && let Some(Some(scope)) = self.eager_scheds.last_mut()
            {
                scope.more_scopes.push(n);
            }
            self.start_runners(&sched, wids);
            return Ok(());
        }
        self.nurseries[i].push(QueuedTask {
            call: task,
            snap,
            cell_ids,
            fresh,
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
    /// (`docs/contributing/gaps.md` W7-12: an uncertain verdict must decline). trio and `asyncio.TaskGroup` print
    /// nothing here either. The eager path never reported, so this also removes a pre-existing
    /// eager-vs-lazy observable split rather than creating one.
    ///
    /// TICKET-147 (W14-12) — returns the FIRST fault the aborted levels' children ended with (`None`
    /// when none faulted, or on an `os.exit`, which keeps its `pending_exit` route). A caller whose
    /// escape carries its own fault keeps that as the root cause and drops this; a cancel funnel, whose
    /// escape is only the `cancelled` sentinel, replaces the sentinel with it.
    pub(super) fn drain_escaped_nursery(&mut self, from_len: usize) -> Option<RuntimeError> {
        if self.nurseries.len() <= from_len {
            return None; // nothing escaped past the join (e.g. normal fall-through already popped it)
        }
        let mut child_fault: Option<RuntimeError> = None;
        // TICKET-141 — every inline abort below waits in place: hold no width permit across it.
        self.width_release();
        while self.nurseries.len() > from_len {
            // All five stacks pop TOGETHER, unconditionally — nurseries, nursery_defer_floors,
            // nursery_spans, mn_scopes and eager_scheds are lockstep, and the enlisted arm below
            // continues.
            self.nursery_defer_floors.pop();
            self.nursery_spans.pop();
            let mn_scope = self.mn_scopes.pop().flatten(); // Some if early-enlisted
            let eager = self.eager_scheds.pop().flatten(); // Some if this nursery is eager
            self.nurseries.pop(); // unstarted tasks — dropped, never run (see the doc above)
            // Cross-nursery flat scheduler — an EARLY-ENLISTED nursery's tasks are LIVE fibers already
            // seeded into the global sched (its `tasks` vec was drained), so an escape past its join must
            // CANCEL + drain them (trip the scope cancel, requeue parked, settle), exactly like an eager
            // nursery's `abort_eager_nursery`. (A nursery is never both: the enlist only happens on the
            // lazy path, so `eager` is `None` here.)
            if let Some(scope_id) = mn_scope {
                let e = self.abort_enlisted_scope(scope_id);
                child_fault = child_fault.or(e);
                continue;
            }
            // Per-connection spawn / §2c1 — an eager nursery's tasks are already-started live fibers:
            // cancel + drain + flush them.
            if let Some(scope) = eager {
                let e = self.abort_eager_nursery(scope);
                child_fault = child_fault.or(e);
            }
        }
        self.width_acquire();
        child_fault
    }

    /// TICKET-132 — before `drain_escaped_nursery` pops anything, check whether the TOP levels
    /// (from `from_len`) are fiber-owned and PARK the owner instead of letting their abort wait
    /// inline. Declines (`false`, no pop) at the first level that is not fiber-owned — that level and
    /// every level below it still drain inline, unchanged — or when `native_reentry > 0` (the Rust
    /// stack holds the native frame, so this fiber cannot park). On park, the rewound op re-runs once
    /// `MnSched::park_join` requeues the fiber; `drain_escaped_nursery`'s re-run then finds the
    /// family settled and pops normally.
    pub(super) fn park_escaped_abort(&mut self, from_len: usize) -> bool {
        let Some(origin) = self.park_abort_origin(from_len) else {
            return false;
        };
        self.rewind_op();
        self.join_suspend = Some(origin);
        true
    }

    /// TICKET-132/TICKET-287 — THE park test of an abort: cancel the fiber-owned levels above
    /// `from_len`, top-down, and return the origin scope of the first one whose family is not done.
    /// `None` (decline) under the same conditions [`Vm::park_escaped_abort`] lists. The caller sets
    /// `join_suspend` and stores what its re-entry needs: a rewind for an escape, a
    /// [`FaultPark`](super::frames::FaultPark) for a fault unwind.
    pub(super) fn park_abort_origin(&mut self, from_len: usize) -> Option<usize> {
        if !matches!(self.block_mode(WaitSpec::Nursery), BlockMode::Park) {
            return None;
        }
        for i in (from_len..self.nurseries.len()).rev() {
            if matches!(self.mn_scopes.get(i), Some(Some(_))) {
                return None;
            }
            let Some(Some(scope)) = self.eager_scheds.get(i) else {
                return None;
            };
            if !scope.fiber_owned {
                return None;
            }
            cancel_fiber_owned_family(scope);
            if !scope.sched.lock().family_done(scope.scope) {
                return Some(scope.scope);
            }
        }
        None
    }

    /// W14-4 -- escape every `parallel:` level above `from_len`, innermost-first: run the level's
    /// body defers (down to its `nursery_defer_floors` entry), then cancel it and wait for its
    /// children. Enclosing defers therefore run after the cancelled children unwind (Go cancel +
    /// `wg.Wait()`, asyncio `TaskGroup`). Returns `true` when it parked (TICKET-132); the rewound op
    /// re-runs with the drained defers consumed. TICKET-287 — a fault already in hand parks with
    /// the fiber ([`Vm::store_escape_park`]) and comes back to `err` on the re-run.
    pub(super) fn unwind_escaped_levels(
        &mut self,
        from_len: usize,
        err: &mut Option<RuntimeError>,
    ) -> bool {
        if let Some(e) = self.take_escape_park() {
            *err = Some(e);
        }
        while self.nurseries.len() > from_len {
            let top = self.nurseries.len() - 1;
            let floor = self.nursery_defer_floors[top];
            if let Some(e) = self.drain_frame_to(floor) {
                *err = Some(e);
            }
            if self.park_escaped_abort(top) {
                if let Some(e) = err.take() {
                    self.store_escape_park(e);
                }
                return true;
            }
            // TICKET-147 — a body defer fault stays the root cause; a child fault only fills the gap.
            let child = self.drain_escaped_nursery(top);
            if err.is_none() {
                *err = child;
            }
        }
        false
    }

    pub(super) fn join_nursery(&mut self) -> Result<(), RuntimeError> {
        // TICKET-103 — a fiber-owned nursery's owner PARKS here while its family runs, and re-runs
        // this op once `MnSched::park_join`'s wake requeues it. Checked before the first pop below,
        // which the re-run must still find. Under `native_reentry > 0` the fiber cannot park (its
        // Rust stack holds the native frame), so `join_fiber_owned_nursery` runs the loop inline.
        // Both callers stop after a park: `Op::JoinNursery` has nothing after the call, and
        // `do_return` returns early on `join_suspend`.
        if matches!(self.block_mode(WaitSpec::Nursery), BlockMode::Park)
            && let Some(Some(scope)) = self.eager_scheds.last()
            && scope.fiber_owned
            && !scope.sched.lock().family_done(scope.scope)
        {
            self.join_suspend = Some(scope.scope);
            self.rewind_op();
            return Ok(());
        }
        // Consume this nursery's tasks (FIFO). Popping the entry now (as the old drain did at the
        // end) keeps the parent's `Handler::nursery_len` accounting correct on a later fault.
        self.nursery_defer_floors.pop(); // keep the parallel floor stack in lockstep with `nurseries`
        // TICKET-048 — lockstep with `nurseries`: the span of the parallel: construct this join is
        // about.
        let nursery_span = self.nursery_spans.pop().unwrap_or(Span::RUNTIME);
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
        // `--serial` (`docs/design/roadmap.md` §2b).
        self.run_mn_nursery(tasks, nursery_span)
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
    pub(super) fn run_mn_nursery(
        &mut self,
        tasks: Vec<QueuedTask>,
        nursery_span: Span,
    ) -> Result<(), RuntimeError> {
        // Cross-nursery flat scheduler — the OUTERMOST nursery (`self.mn.is_none()`) builds the ONE
        // global sched + farms helpers; a NESTED nursery (`self.mn.is_some()`) REUSES it (register a
        // scope, enlist into the same global run queue, run its inline owner scope-scoped). Because the
        // inline owner drains the GLOBAL queue, it naturally runs cross-nursery siblings — the case-A
        // fix (`docs/history/designs/cross-nursery-flat-scheduler.md`). Nested owners farm NO helpers (they run inline
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
            (Some(sched), _) => self.run_mn_nursery_nested(&sched, tasks, nursery_span),
            (None, Some(held)) => self.run_mn_nursery_nested(&held, tasks, nursery_span),
            (None, None) => self.run_mn_nursery_outermost(tasks, nursery_span),
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
        nursery_span: Span,
    ) -> Result<(), RuntimeError> {
        let total = tasks.len();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut fibers = Vec::with_capacity(total);
        for (i, t) in tasks.into_iter().enumerate() {
            // W6-2 — each task replays the snapshot pinned at its own spawn. scope 0 — the outermost
            // nursery.
            fibers.push(
                self.prepare_worker(t.call, t.snap?, &t.cell_ids, t.fresh)?
                    .into_fiber(i, 0),
            );
        }
        let deadlock_err = self.err(DEADLOCK_MSG.to_string(), nursery_span).deadlock();
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
        // TICKET-099 — so a send/close on this sched can wake a receiver parked on any other live
        // sched of this run (sibling or descendant), not just an ancestor.
        inner.sched_registry = Arc::clone(&self.sched_registry);
        // gaps.md W7-56 — the deadlock predicate must see this run's outstanding eager `Executor`
        // jobs (uncounted senders). Assigned here, not through `new`, so the predicate's unit
        // fixtures keep an empty registry.
        inner.exec_registry = Arc::clone(&self.exec_registry);
        // gaps.md W7-58 — so an idle worker of this sched can JUDGE the process-wide verdict on
        // behalf of a nursery owner, which never reaches `block_halt_check`.
        inner.join_run(&self.quiesce);
        let sched = Arc::new(inner);
        sched.mark_all_runners_claimed();
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
        {
            // gaps.md W7-58 — this thread is now blocked in a nursery join; make it visible to the
            // process-wide verdict for exactly that span (RAII, dropped before the reduce).
            let _party = self.nursery_party_guard(&sched);
            if let Some(mut shell) = self.joiner_shell(&sched, &cancel, 0, JoinCover::None) {
                shell.mn_worker_loop(&sched, 0, 0); // owner of scope 0
            }
            // The owner returned on scope 0; reduce scope 0's sub-range. The sched is released only when
            // no early-enlisted outer scope is still pending (else those scopes' slots must survive
            // until their own joins reduce them — `join_enlisted_scope` releases it at the last).
            sched.wait_for_scope(0);
        }
        // TICKET-164 — publish this run's pick count on the joining thread (read by
        // run_capture_counting_picks).
        #[cfg(test)]
        super::RUN_PICKS.with(|p| p.set(p.get() + sched.picks.load(Ordering::Relaxed)));
        #[cfg(test)]
        self.publish_module_replays();
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
        nursery_span: Span,
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
            workers.push(self.prepare_worker(t.call, t.snap?, &t.cell_ids, t.fresh)?);
        }
        let scope_id =
            sched.register_scope_seeded(Arc::clone(&cancel), self.scope_ancestors(), workers);
        // TICKET-095 — the owner-fiber arm (`self.owns_nested_sched(sched)`) gets its OWN deadlock
        // error, naming THIS nursery's span, so a T=1 fault reports the inner nursery like every other
        // worker count does. The `(None, Some(held))` late-spawn arm gets none: it has no
        // `blocked_owners` bracket either (see `blocked_owner_guard`), and DEC-048 makes the sched's
        // builder-span error correct for it.
        if self.owns_nested_sched(sched) {
            sched.lock().scopes[scope_id].deadlock_err =
                Some(self.err(DEADLOCK_MSG.to_string(), nursery_span).deadlock());
        }
        let wid = self.wid;
        {
            // gaps.md W7-58 — a no-op on the `(Some(sched), _)` path (a fiber's nested nursery runs on
            // a worker shell, `mn.is_some()`); it registers only on the `(None, Some(held))` late-spawn
            // path, where `self` really is the top-level builder blocked in this join.
            let _party = self.nursery_party_guard(sched);
            // TICKET-095 — counts this fiber's OS thread as a `blocked_owner`, on ITS OWN sched
            // (`self.mn`), for the span it sits inline in this join, so `is_deadlocked_ignoring_jobs`
            // can tell it apart from a genuinely running worker. `None` on the late-spawn arm (see
            // `blocked_owner_guard`). TICKET-099 widened this to the cross-sched case too: `sched`
            // (the child nursery this fiber is joining) may be a private eager sched of its own, not
            // `self.mn` — the fiber still counts as blocked on `self.mn`, because a peer veto asks
            // `self.mn` "can you still feed me?" and a fiber waiting on this join can feed nobody.
            // TICKET-103 — in the owner arm the joined scope is on the owner's own sched, so it is
            // recorded as this join's target for `flag_deadlock_leaves`.
            let _owner =
                self.blocked_owner_guard(sched, self.owns_nested_sched(sched).then_some(scope_id));
            if let Some(mut shell) = self.joiner_shell(sched, &cancel, wid, JoinCover::None) {
                shell.mn_worker_loop(sched, wid, scope_id);
            }
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
                prepared.push(self.prepare_worker(t.call, t.snap?, &t.cell_ids, t.fresh)?);
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
        sched.lock().builder_joined(scope_id);
        let cancel = Arc::clone(&sched.lock().scopes[scope_id].cancel);
        let wid = self.wid;
        // W6-2 — a shell needs no snapshot of its own: it runs no code, and every fiber it schedules in
        // carries its own module view + snapshot (`FiberCtx`).
        {
            // gaps.md W7-58 — the enlisted-scope join blocks this (inline builder) thread too.
            let _party = self.nursery_party_guard(&sched);
            if let Some(mut shell) = self.joiner_shell(&sched, &cancel, wid, JoinCover::None) {
                shell.mn_worker_loop(&sched, wid, scope_id);
            }
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
    ///
    /// TICKET-147 (W14-12) — returns the fault its children ended with, if any (an `os.exit` keeps its
    /// `pending_exit` route and returns `None`). The escape's own fault stays the root cause: callers
    /// use this only when the escape carries none (see [`Vm::drain_escaped_nursery`]).
    pub(super) fn abort_enlisted_scope(&mut self, scope_id: usize) -> Option<RuntimeError> {
        let sched = self.mn_enlist_sched.clone()?;
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
            c.builder_joined(scope_id);
            Arc::clone(&c.scopes[scope_id].cancel)
        };
        sched.drain_family(scope_id);
        let wid = self.wid;
        // W6-2 — a shell needs no snapshot (see `join_enlisted_scope`), which also retires the
        // `.expect("no fault possible")` this teardown path used to carry.
        {
            // gaps.md W7-58 — the cancel-teardown drain blocks this thread just as the normal join does.
            let _party = self.nursery_party_guard(&sched);
            if let Some(mut shell) = self.joiner_shell(&sched, &cancel, wid, JoinCover::None) {
                shell.mn_worker_loop(&sched, wid, scope_id);
            }
            sched.wait_for_scope(scope_id);
        }
        let slots = sched.take_scope_slots(scope_id);
        self.mn_enlisted -= 1;
        if self.mn_enlisted == 0 {
            self.mn_enlist_sched = None;
        }
        self.escape_child_fault(slots)
    }

    /// TICKET-147 — reduce an aborted (escaped) nursery's slots into the fault its children ended
    /// with. An `os.exit` sets `pending_exit` inside the reduce and is not reported here: the catch
    /// sites honor it through `pending_exit`.
    /// TICKET-195 — it reduces through [`Vm::reduce_slots`], never [`Vm::reduce_task_slots`]: this
    /// runs inside the party's OWN fault's unwind, so the child's fault must not become a cut.
    fn escape_child_fault(&mut self, slots: Vec<Option<TaskOutcome>>) -> Option<RuntimeError> {
        self.reduce_slots(slots)
            .0
            .err()
            .filter(|_| self.pending_exit.is_none())
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
    pub(super) fn activate_eager_nursery(&mut self, nursery_span: Span) -> Option<EagerScope> {
        // §2c1 — a NESTED eager nursery on THIS thread joins the enclosing scope's sched as a new
        // SCOPE instead of building a private sibling sched, so the two share one predicate and one
        // wake fan-out from the start — see `EagerScope::scope`. (TICKET-099 — a private sibling sched
        // would still be reachable through `wake_run_wide`'s run-wide registry walk, but joining as a
        // scope keeps one fault predicate rather than two that must agree.)
        //
        // Only when `mn.is_none()`. On a WORKER SHELL the enclosing eager scope belongs to a
        // different nursery generation and a private eager sched sharing the run's `sched_registry` is
        // the per-connection-spawn design; that path is unchanged.
        if self.mn.is_none()
            && let Some(outer) = self.owned_eager_scopes().next_back()
        {
            let sched = Arc::clone(&outer.sched);
            let cancel = Arc::new(AtomicBool::new(false));
            let scope = sched.register_scope_seeded(
                Arc::clone(&cancel),
                self.scope_ancestors(),
                Vec::new(),
            );
            sched.open_body(scope);
            return Some(EagerScope {
                sched,
                cancel,
                drainer: None, // the OWNER's drainer serves this scope too — it drains the global queue
                drainer_slot: None,
                scope,
                fiber_owned: false,
                more_scopes: Vec::new(),
            });
        }
        // TICKET-073 — a NESTED nursery's private drainer costs one OS thread per OPEN nursery, not
        // per nesting level, so it spends from the process-wide `NestedDrainerSlot` budget; a denied
        // slot falls back to the lazy queue-at-join path via the `?` below, same as a failed thread
        // spawn. The OUTERMOST nursery (`self.mn.is_none()`) never needs a slot: there is exactly one
        // per thread.
        let drainer_slot = if self.mn.is_some() {
            Some(NestedDrainerSlot::acquire()?)
        } else {
            None
        };
        let ancestors = self.scope_ancestors();
        let mut scope = self.new_detached_sched(nursery_span, ancestors, None)?;
        scope.drainer_slot = drainer_slot;
        Some(scope)
    }

    /// A private [`MnSched`] with one open scope (scope 0) whose cancel parents are `ancestors`,
    /// registered with the run and served by its own raw drainer thread. The one builder of a
    /// sched that is not a scope of an enclosing one: an eager `parallel:` passes its own
    /// [`Vm::scope_ancestors`] (TICKET-208).
    pub(super) fn new_detached_sched(
        &mut self,
        span: Span,
        ancestors: Vec<Arc<AtomicBool>>,
        detached: Option<&Arc<ExecutorCore>>,
    ) -> Option<EagerScope> {
        let cancel = match detached {
            Some(core) => Arc::clone(&core.cancel),
            None => Arc::new(AtomicBool::new(false)),
        };
        let deadlock_err = self.err(DEADLOCK_MSG.to_string(), span).deadlock();
        // wid 0 = the body's thread (body, then inline joiner), wid 1 = the dedicated raw drainer
        // below, wids 2..=n = the raw runners `Vm::start_runners` starts on a claim (TICKET-211).
        // An Executor claims wids 1..=n. `MnSched::new` allocates the per-worker local queues up front (`locals: (0..nworkers)`), so the count must
        // be sized here even though most of those workers are farmed later; an unused local queue is
        // inert.
        let nworkers = worker_count().max(1) + 1;
        let mut inner = MnSched::new(
            0,
            nworkers,
            Arc::clone(&cancel),
            deadlock_err,
            self.heap.mem_cap(),
        );
        // TICKET-099 — this eager sched is PRIVATE (no link to the parent). A `send`/`close` inside its
        // body must be able to wake a receiver parked on ANY other live sched of this run — the parent
        // nursery, a sibling, or a descendant — not just an ancestor, so it shares the run's registry
        // and `wake_run_wide` walks it directly rather than through a hierarchical chain.
        inner.sched_registry = Arc::clone(&self.sched_registry);
        // TICKET-112 — this eager sched is NESTED: its body runs on the fiber of `self`'s own sched
        // (`self.mn.is_some()`), not on a dedicated drainer thread. See `MnSched::body_is_fiber`.
        inner.body_is_fiber = self.mn.is_some();
        // gaps.md W7-56 — see `run_mn_nursery_outermost`.
        inner.exec_registry = Arc::clone(&self.exec_registry);
        // gaps.md W7-58 — so an idle worker of this sched can JUDGE the process-wide verdict on
        // behalf of a nursery owner, which never reaches `block_halt_check`.
        inner.join_run(&self.quiesce);
        inner.detached = detached.map(Arc::downgrade);
        if detached.is_some() {
            inner.body_is_fiber = false; // no body at all: its submitters are other parties
        }
        let sched = Arc::new(inner);
        self.register_sched(&sched);
        // Structured concurrency — an eager nursery is a nested scope: its handlers must observe the
        // enclosing scopes' cancel too (`JoinScope::ancestors`).
        sched.lock().scopes[0].ancestors = ancestors;
        if let Some(core) = detached {
            // TICKET-208 — an `Executor`'s scope: no body thread and no drainer. Its submitters are
            // other parties, which the process-wide verdict counts, so scope 0 is open (it takes
            // submits until a join closes it) and never "injecting". `submit` starts the runners.
            {
                let mut c = sched.lock();
                c.scopes[0].body_open = true;
                c.scopes[0].body_blocked = true;
                c.leaf_site = true;
                c.exec_limit = core.limit;
            }
            self.quiesce.register_eager_body(&sched);
            return Some(EagerScope {
                sched,
                cancel,
                drainer: None,
                drainer_slot: None,
                scope: 0,
                fiber_owned: false,
                more_scopes: Vec::new(),
            });
        }
        sched.open_body(0);
        {
            // TICKET-211 — the body's own thread (wid 0) and the drainer below (wid 1) are runners.
            let mut c = sched.lock();
            c.runner_wids[0] = true;
            c.runner_wids[1] = true;
        }
        let shell = self.spawn_shell(&sched, &cancel);
        let drainer = spawn_worker_thread(shell, &sched, "chezzi-eager", 1, 0).ok()?; // no drainer ⇒ no worker during the body ⇒ fall back to lazy (see the doc above)
        // TICKET-230 — the body shares the runner budget with its drainer at every worker count.
        if self.mn.is_none() {
            self.slice_in_place = true;
        }
        // §2c1 — an eager nursery publishes itself so the process-wide verdict counts its undone
        // fibers as uncounted senders. Without it, top-level `main` blocked on `ch.recv()` while a
        // live sibling is about to `send` is `parties.len() >= live` with nothing satisfiable — a
        // false deadlock on a live program.
        //
        // TICKET-112 — NESTED eager nurseries are registered too, not just OUTERMOST ones: a NESTED
        // sched's fibers are also live senders the process-wide verdict must count (`recursive`
        // false-faulted past the granted-slot path at the default worker count without this). Sound
        // only together with `MnSched::body_is_fiber` and `live_eager_bodies`' `can_still_move`
        // read with `require_parked` false — see `## Decisions` bullet 3.
        self.quiesce.register_eager_body(&sched);
        Some(EagerScope {
            sched,
            cancel,
            drainer: Some(drainer),
            drainer_slot: None,
            scope: 0,
            fiber_owned: false,
            more_scopes: Vec::new(),
        })
    }

    /// TICKET-103 (W12-1) — the fallback when a nursery entered inside a spawned fiber gets no
    /// private eager sched (`worker_count() == 1`, a denied `NestedDrainerSlot`, or a failed drainer
    /// thread): register its scope on the fiber's OWN sched (`self.mn`). Each `spawn` then injects a
    /// live fiber that runs on that sched's existing workers, so the task starts at the `spawn` as
    /// Go's `go` does, and no thread is added (W8-8, the pool bound). The queue-at-join fallback
    /// this replaces starved any owner body that blocks on its own child before the join.
    ///
    /// No `open_body`: the owner is a counted fiber (`running`/`parked`), so `body_open` would veto
    /// the predicate while it is parked and a genuine deadlock would hang. The scope names this
    /// nursery in its deadlock fault (DEC-095). `None` at the top level (`self.mn` is `None`).
    pub(super) fn activate_fiber_owned_nursery(
        &mut self,
        nursery_span: Span,
    ) -> Option<EagerScope> {
        let sched = self.mn.clone()?;
        let cancel = Arc::new(AtomicBool::new(false));
        let scope =
            sched.register_scope_seeded(Arc::clone(&cancel), self.scope_ancestors(), Vec::new());
        {
            let mut c = sched.lock();
            c.scopes[scope].deadlock_err =
                Some(self.err(DEADLOCK_MSG.to_string(), nursery_span).deadlock());
            // TICKET-125 — this scope's fibers can only be fed once the OWNING fiber's own join
            // returns; a family faulting that fiber (`flag_deadlock_leaves`) must treat this scope
            // as interior, not a leaf, or the owner is dropped without unwinding it (W13-4).
            c.scopes[scope].parent_scope = self.fiber_scope;
        }
        Some(EagerScope {
            sched,
            cancel,
            drainer: None,
            drainer_slot: None,
            scope,
            fiber_owned: true,
            more_scopes: Vec::new(),
        })
    }

    /// Per-connection spawn — `JoinNursery` for an eager nursery (the normal fall-through path). Close
    /// the body (no more injections → the sched may terminate once every handler is done), then run
    /// the inline join worker (`wid` 0) to help drain remaining handlers, wait for every slot to fill,
    /// and reduce (Decision-F output flush in spawn order; a handler fault propagates as the
    /// acceptor's body fault, which the outer nursery then sees). Mirrors `run_mn_nursery`'s tail.
    ///
    /// §2c1 — two additions once the OUTERMOST nursery takes this path:
    /// - **The joiner claims its slot.** Runners are started at the spawn, never here
    ///   (TICKET-211: `MnSched::claim_runners` in `inject_or_extend`, `Vm::start_runners`).
    ///   `close_body` gives the body's runner bit back, and the inline joiner runs fibers only if
    ///   `MnSched::claim_joiner` finds a slot left under the worker count.
    /// - **The W7-58 party guard.** The joiner used to be a worker shell by construction, so it was
    ///   never a counted party; a top-level joiner IS one, and without registering, `main` sitting in
    ///   `mn_worker_loop` is invisible to the process-wide verdict (`parties.len() < live` vetoes
    ///   forever). `nursery_party_guard` self-gates on `mn.is_none()`,
    ///   so it stays a no-op on a shell.
    pub(super) fn join_eager_nursery(&mut self, scope: EagerScope) -> Result<(), RuntimeError> {
        if scope.fiber_owned {
            return self.join_fiber_owned_nursery(scope);
        }
        let sids = scope.sids();
        let EagerScope {
            sched,
            cancel,
            drainer,
            drainer_slot: _held,
            scope: sid,
            fiber_owned: _,
            more_scopes: _,
        } = scope;
        sched.close_body(sid);
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
                // TICKET-099 — this thread's OS-level block on `wait_for_scope` below counts as a
                // `blocked_owner` on `self.mn` (see `blocked_owner_guard`'s doc): `None` here since
                // `drainer.is_none()` only happens when `self.mn` was already `None` at nursery entry
                // (`activate_eager_nursery`'s reused-scope branch requires it), so there is no sched
                // above this one to report to.
                let _owner = self.blocked_owner_guard(&sched, None);
                // TICKET-118 (W13-7) — only the top-level `Vm` (`mn.is_none()`) may hand its pool
                // slot over while it waits here (DEC-052).
                let _joiner = self.mn.is_none().then(|| sched.pool_joiner_guard());
                // TICKET-211 — the body's block above handed its runner slot to the claim; this
                // thread runs fibers only if the claim still has a slot for it.
                let mut shell = self.joiner_shell(&sched, &cancel, 0, JoinCover::Runners);
                // TICKET-103 — over the nursery's family: its continuation scopes hold the tasks
                // spawned after a fiber registered a later scope on this sched.
                for &s in &sids {
                    if let Some(shell) = shell.as_mut() {
                        shell.mn_worker_loop(&sched, 0, s);
                    }
                    sched.wait_for_scope(s);
                }
            }
            let slots: Vec<_> = sids
                .iter()
                .flat_map(|&s| sched.take_scope_slots(s))
                .collect();
            for &s in sids.iter().rev() {
                sched.retire_scope(s);
            }
            return self.reduce_task_slots(slots);
        }
        {
            let _party = self.nursery_party_guard(&sched);
            // TICKET-099 — this thread's OS-level block on `wait_for_completion` below counts as a
            // `blocked_owner` on `self.mn`, whether `sched` (this join's PRIVATE eager sched) is the
            // outermost nursery (`self.mn` is `None`, the guard is a no-op) or a nursery entered
            // inside a spawned task (`self.mn` is `Some(outer)`, so this fiber counts as blocked on
            // `outer` — the fiber that is join-blocked here can feed `outer`'s peer veto nothing).
            let _owner = self.blocked_owner_guard(&sched, None);
            // TICKET-118 (W13-7) — only the top-level `Vm` (`mn.is_none()`) may hand its pool slot
            // over while it waits here (DEC-052).
            let _joiner = self.mn.is_none().then(|| sched.pool_joiner_guard());
            // TICKET-211 — `close_body` gave the body's runner slot back; the inline joiner runs
            // fibers only if the claim has a slot left (started runners may already fill it).
            if let Some(mut shell) = self.joiner_shell(&sched, &cancel, 0, JoinCover::Runners) {
                shell.mn_worker_loop(&sched, 0, 0);
            }
            sched.wait_for_completion();
        }
        if let Some(h) = drainer {
            let _ = h.join();
        }
        join_helpers(&sched);
        // TICKET-164 — publish this run's pick count on the joining thread (read by
        // run_capture_counting_picks).
        #[cfg(test)]
        super::RUN_PICKS.with(|p| p.set(p.get() + sched.picks.load(Ordering::Relaxed)));
        #[cfg(test)]
        self.publish_module_replays();
        let slots = sched.take_slots();
        self.reduce_task_slots(slots)
    }

    /// TICKET-211 — THE starter of runner threads: one runner per wid [`MnSched::claim_runners`]
    /// claimed, all `SENTINEL_SCOPE` (they drain the global queue until `terminate`). An Executor's
    /// are new `chezzi-exec` threads, counted in `runner_starts` and never joined. Any other sched's
    /// are leases handed to `runner_cache`, which runs each on a parked raw thread if one is free and
    /// on a new `chezzi-eager-helper` thread otherwise (never the pool, DEC-103). Each lease holds a
    /// [`NestedDrainerSlot`] and is counted on the sched for `join_helpers`. A denied slot or a
    /// failed spawn gives its wid back. Returns whether any runner remains, as
    /// [`MnSched::unclaim_runner`] reports.
    pub(super) fn start_runners(&self, sched: &Arc<MnSched>, wids: Vec<usize>) -> bool {
        if wids.is_empty() {
            return true;
        }
        let Some(cancel) = sched.scope_cancel(0) else {
            for wid in wids {
                sched.unclaim_runner(wid);
            }
            return false;
        };
        let exec = sched.detached.is_some();
        if let Some(core) = sched.detached.as_ref().and_then(|w| w.upgrade()) {
            core.runner_starts.fetch_add(wids.len(), Ordering::Relaxed);
        }
        let mut runner = true;
        for wid in wids {
            if exec {
                let shell = self.spawn_shell(sched, &cancel);
                if spawn_worker_thread(shell, sched, "chezzi-exec", wid, SENTINEL_SCOPE).is_err() {
                    runner = sched.unclaim_runner(wid);
                }
                continue;
            }
            let Some(slot) = NestedDrainerSlot::acquire() else {
                #[cfg(test)]
                sched
                    .quiesce
                    .runner_slot_denials
                    .fetch_add(1, Ordering::SeqCst);
                runner = sched.unclaim_runner(wid);
                continue;
            };
            sched.helper_started();
            let shell = self.spawn_shell(sched, &cancel);
            let lease = super::runner_cache::Lease {
                sched: Arc::clone(sched),
                shell,
                wid,
                slot,
            };
            if super::runner_cache::start(lease).is_err() {
                sched.helper_done();
                runner = sched.unclaim_runner(wid);
            }
        }
        runner
    }

    /// Per-connection spawn — reclaim an eager nursery whose body ESCAPED early (`?`/`return`/`break`/
    /// `continue` or a `recover:` catch jumped past its `JoinNursery`). The injected handlers are live
    /// fibers, so (unlike a lazy nursery's unstarted `PendingCall`s) they must be cancelled, not just
    /// dropped: trip the inner cancel, drain channel- and socket-parked handlers (D6b
    /// `drain_family`), run the inline worker to settle them, then flush their output
    /// (Decision F). The body's own escape error is what propagates, so a handler fault here is
    /// swallowed (only its buffered output + any `os.exit` are honored via `reduce_task_slots`).
    ///
    /// TICKET-147 (W14-12) — returns the fault its children ended with (see
    /// [`Vm::abort_enlisted_scope`]); the callers keep the escape's own fault first.
    pub(super) fn abort_eager_nursery(&mut self, scope: EagerScope) -> Option<RuntimeError> {
        if scope.fiber_owned {
            return self.abort_fiber_owned_nursery(scope);
        }
        let sids = scope.sids();
        let EagerScope {
            sched,
            cancel,
            drainer,
            drainer_slot: _held,
            scope: sid,
            fiber_owned: _,
            more_scopes: _,
        } = scope;
        // N4 — trip the cancel UNDER the core lock (`trip_scope_cancel`, scope 0 = the eager scope, whose
        // `JoinScope::cancel` IS this `cancel` Arc) and BEFORE `close_body` clears the `any_body_open`
        // veto: gapless veto handoff, and the store is published to any worker that takes the core lock
        // to evaluate the deadlock predicate (a bare `Relaxed` store outside the lock has no
        // synchronizes-with edge, so a worker could read a stale `false` and reap this scope's parked
        // handlers as `Deadlocked` — dropping them without `unwind_deferred`, skipping their `defer`s).
        sched.trip_scope_cancel(sid);
        sched.close_body(sid);
        // §2c1 — family-scoped (`drain_family`): a nested eager nursery shares the enclosing scope's
        // sched, so draining by sched alone would unpark the OUTER scope's socket-parked fibers too.
        sched.drain_family(sid);
        // §2c1 — a NESTED scope settles and reduces only ITSELF, then retires (see
        // `join_eager_nursery`'s nested arm); the owner's drainer and sibling scopes are untouched.
        if drainer.is_none() {
            {
                let _bodies = self.blocked_bodies_guard(true); // see `join_eager_nursery`'s nested arm
                let _party = self.nursery_party_guard(&sched);
                // T1-fix (W8-8, nested arm) — same gate as `join_eager_nursery`'s nested arm above,
                // over the same family (TICKET-103).
                let mut shell = self.joiner_shell(&sched, &cancel, 0, JoinCover::RunnersAbort);
                for &s in &sids {
                    if let Some(shell) = shell.as_mut() {
                        shell.mn_worker_loop(&sched, 0, s);
                    }
                    sched.wait_for_scope(s);
                }
            }
            let slots: Vec<_> = sids
                .iter()
                .flat_map(|&s| sched.take_scope_slots(s))
                .collect();
            for &s in sids.iter().rev() {
                sched.retire_scope(s);
            }
            return self.escape_child_fault(slots);
        }
        {
            // §2c1 — same reason as `join_eager_nursery`: a top-level escape settles its handlers on
            // `main`, which must be a counted party for that span or the process-wide verdict vetoes
            // forever. No-op on a worker shell.
            let _party = self.nursery_party_guard(&sched);
            if let Some(mut shell) = self.joiner_shell(&sched, &cancel, 0, JoinCover::RunnersAbort)
            {
                shell.mn_worker_loop(&sched, 0, 0);
            }
            sched.wait_for_completion();
        }
        if let Some(h) = drainer {
            let _ = h.join();
        }
        join_helpers(&sched);
        let slots = sched.take_slots();
        // `reduce_task_slots` sets `self.pending_exit` for a handler `os.exit` (decision C — a hard
        // halt wins), which the catch site honors after the drain — so it is NOT lost. A handler
        // fault is returned; the callers keep the escape's own fault first.
        self.escape_child_fault(slots)
    }

    /// TICKET-103 — `JoinNursery` for a fiber-owned nursery (`EagerScope::fiber_owned`). Its scopes
    /// live on this fiber's own sched, so the owner's OS thread runs them inline, scope by scope over
    /// the family, under `blocked_owner_guard` with the origin scope as its join target. Mirrors
    /// `run_mn_nursery_nested`'s owner arm.
    ///
    /// [`Vm::joiner_shell`] decides whether this thread runs the family's fibers (TICKET-245).
    /// Under `native_reentry > 0` the owner hands its runner slot to its demote replacement and
    /// only waits; the inline loop remains there only for a refused OS thread. It is not gated on
    /// `eager_joiner_runs_fibers` (the main-thread nested arm differs because a drainer exists
    /// there). No `nursery_party_guard`: the owner is a fiber, never a counted party. No
    /// `blocked_bodies_guard`: the body is a counted fiber, and `awaiting_builder` on its scope would
    /// veto a genuine deadlock.
    ///
    /// With `native_reentry == 0` the family is complete here, because `join_nursery` parked the
    /// fiber otherwise. Under `native_reentry > 0` the fiber cannot park (its Rust stack holds the
    /// native frame, the rule `park_recv`'s caller follows), so it waits here in place.
    fn join_fiber_owned_nursery(&mut self, scope: EagerScope) -> Result<(), RuntimeError> {
        let sids = scope.sids();
        if !scope.sched.lock().family_done(scope.scope) {
            let wid = self.wid;
            let _owner = self.blocked_owner_guard(&scope.sched, Some(scope.scope));
            let mut shell = self.joiner_shell(&scope.sched, &scope.cancel, wid, JoinCover::None);
            for &sid in &sids {
                if let Some(shell) = shell.as_mut() {
                    shell.mn_worker_loop(&scope.sched, wid, sid);
                }
                scope.sched.wait_for_scope(sid);
            }
        }
        let slots: Vec<_> = sids
            .iter()
            .flat_map(|&s| scope.sched.take_scope_slots(s))
            .collect();
        for &s in sids.iter().rev() {
            scope.sched.retire_scope(s);
        }
        self.reduce_task_slots(slots)
    }

    /// TICKET-103/TICKET-132 — `abort_eager_nursery` for a fiber-owned nursery: trip the family's
    /// one cancel token, requeue its parked and socket-parked fibers, then settle them. It waits
    /// inline ONLY where [`Vm::park_abort_origin`] declines: `native_reentry > 0` (the Rust stack holds
    /// the native frame), a nested dispatch loop, or a lower level that is not fiber-owned. A fault
    /// unwind parks through [`FaultPark`](super::frames::FaultPark) (TICKET-287). The body's escape error is what propagates, so a task fault here is swallowed.
    fn abort_fiber_owned_nursery(&mut self, scope: EagerScope) -> Option<RuntimeError> {
        let sids = scope.sids();
        cancel_fiber_owned_family(&scope);
        if !scope.sched.lock().family_done(scope.scope) {
            let wid = self.wid;
            let _owner = self.blocked_owner_guard(&scope.sched, Some(scope.scope));
            let mut shell = self.joiner_shell(&scope.sched, &scope.cancel, wid, JoinCover::None);
            for &sid in &sids {
                if let Some(shell) = shell.as_mut() {
                    shell.mn_worker_loop(&scope.sched, wid, sid);
                }
                scope.sched.wait_for_scope(sid);
            }
        }
        let slots: Vec<_> = sids
            .iter()
            .flat_map(|&s| scope.sched.take_scope_slots(s))
            .collect();
        for &s in sids.iter().rev() {
            scope.sched.retire_scope(s);
        }
        self.escape_child_fault(slots)
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
        // TICKET-141 (W14-14) — the replacement worker, an inline join shell and a farmed helper of a
        // gated thread are gated too; each starts without a permit (see `src/vm/width.rs`).
        shell
    }
}
