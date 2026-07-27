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
#[derive(Default)]
struct WireMemo {
    /// GcRef of an identity-preserved node (`Cell`/`Closure`/container) currently on the serialize DFS
    /// stack → the `id` assigned on its first visit. A revisit while still in `path` is a true back-edge
    /// → `Backref(id)`; removed on DFS exit so an off-stack alias is deep-copied independently.
    path: super::fxhash::FxHashMap<GcRef, u32>,
    next_id: u32,
    /// GcRefs of `Obj::Generator`s currently on the serialize DFS stack. A generator carries no id (its
    /// parked frame can't be a `Backref` target), so re-entering one still on the stack is a cycle
    /// through a non-preservable node → reject (never duplicate). Removed on DFS exit, so a generator
    /// revisited off-stack (an acyclic DAG alias) is deep-copied independently, like the containers.
    gens_on_stack: super::fxhash::FxHashSet<GcRef>,
}

impl Vm {
    /// `spawn f(args)` / `spawn recv.m(args)` — pop `argc(+1)` operands, deep-copy the args (and, for
    /// the method form, the receiver) across the airlock, and register the task on the innermost
    /// nursery. The callee passes by handle (like `defer`); only data crosses the airlock. Mirrors
    /// the serial-VM oracle's `exec_spawn`.
    pub(super) fn do_spawn(
        &mut self,
        method: Option<String>,
        argc: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let raw_args: Vec<Value> = self.stack.split_off(at);
        let head = self.pop();
        let mut args: Vec<Value> = Vec::with_capacity(raw_args.len());
        for a in raw_args {
            args.push(self.deep_clone(a, span)?);
        }
        let task = match method {
            Some(name) => {
                let recv = self.deep_clone(head, span)?;
                PendingCall::Method {
                    recv,
                    name,
                    args,
                    span,
                }
            }
            None => PendingCall::Call {
                callee: self.cross_spawn_callee(head, span)?,
                args,
                span,
            },
        };
        self.register_task(task, span)
    }

    /// Cross a `spawn f()` **callee** over the task boundary. A closure that captures locals holds them
    /// (uniformly) as `Obj::Cell` handles; sharing that closure by handle would let the task alias the
    /// parent's cells — a serial-vs-M:N divergence, since the M:N engine's `prepare_worker`/`to_snap`
    /// path already deep-copies a task closure's captures into fresh cells. So a *capture-bearing*
    /// closure crosses by DEEP value here too (via the existing `wire_callable` → `from_wire`
    /// round-trip, the same serializer M:N uses), snapshotting its cells at spawn time on BOTH engines.
    /// A capture-free callable (plain `Obj::Func`, a builtin, or a closure with no captures) keeps the
    /// cheap shared handle — it holds no mutable captured state, so sharing is observationally identical
    /// to copying (and preserves the closure hot path). `Shared`/`RwShared`/`Atomic`/`Channel` captures
    /// still cross by reference (their `Arc` core is deep-copied as the same `Arc`), so `Shared`-based
    /// cross-task sharing is unaffected.
    fn cross_spawn_callee(&mut self, callee: Value, span: Span) -> Result<Value, RuntimeError> {
        let deep = match callee.as_obj() {
            Some(h) => {
                matches!(self.heap.get(h), Obj::Closure { captured, .. } if !captured.is_empty())
            }
            None => false,
        };
        if deep {
            let w = self.wire_callable(callee, span)?;
            Ok(self.from_wire(w))
        } else {
            Ok(callee)
        }
    }

    /// `spawn:` block — snapshot the captured bindings from the current frame (like `MakeClosure`),
    /// deep-copy each captured value across the airlock, build a zero-arg closure over the synthetic
    /// block proto, and register it as a `Call` task. Mirrors the serial-VM oracle's `Task::Block`
    /// (captured locals deep-copied; home globals by handle).
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
            // Deep-copy across the airlock: the task can't share mutable state with the parent.
            // Positional (lever #3): slot order matches the synthetic block proto's `capture_names`.
            captured.push(self.deep_clone(v, span)?);
        }
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
        )
    }

    /// Register a spawned task on the innermost nursery. Per-connection spawn: if that nursery is
    /// EAGER, build the handler into a live [`Fiber`] (serializing its args out of THIS fiber's heap,
    /// the same airlock copy `do_spawn`'s `deep_clone` does) and [`MnSched::inject`] it straight into
    /// the running sched — it runs concurrently with the rest of the body. The `task_index` is the
    /// scope's monotonic `next_index` (spawn order), so Decision-F output stays deterministic.
    /// Otherwise (lazy/top-level) push the [`QueuedTask`] for the join to drain. The checker guarantees
    /// a `parallel:` is open, but we guard for parity with the serial-VM oracle's runtime error.
    ///
    /// W6-2 — THE PIN INSTANT: the task's module view is snapshotted HERE, at its own `spawn`, on both
    /// engines and on both the eager and the lazy path. `ensure_snapshot`'s cache makes consecutive
    /// spawns cheap; a build failure is CARRIED on the task and raised where it is prepared.
    pub(super) fn register_task(
        &mut self,
        task: PendingCall,
        span: Span,
    ) -> Result<(), RuntimeError> {
        // The innermost open nursery (`nurseries`, `mn_scopes` and `eager_scheds` are lockstep).
        let Some(i) = self.nurseries.len().checked_sub(1) else {
            return Err(self.err("spawn must be inside a parallel: block".to_string(), span));
        };
        let snap = self.ensure_snapshot(span);
        // Eager innermost nursery → inject a live fiber. Clone the sched Arc, drop the borrow so
        // `prepare_worker` can take `&mut self`; `inject` assigns the real slot index under its lock
        // (the `0` placeholder is overwritten), so no caller-side index bookkeeping is needed.
        if let Some(Some(scope)) = self.eager_scheds.last() {
            let sched = Arc::clone(&scope.sched);
            // The eager nursery owns its OWN sched (a single scope 0 — see `activate_eager_nursery`);
            // `inject` overwrites the `0` placeholder `task_index` under its lock. This path PREPARES the
            // task right here, so a snapshot build failure surfaces right here too (prepare instant).
            let fiber = self.prepare_worker(task, Some(snap?))?.into_fiber(0, 0);
            sched.inject(fiber, 0);
            return Ok(());
        }
        self.nurseries[i].push(QueuedTask { call: task, snap });
        Ok(())
    }

    /// `parallel:` dedent — run the nursery's spawned tasks as cooperative fibers (B1/B2). The
    /// joining (parent) fiber is parked while the children run; a child that blocks on an empty
    /// `recv` suspends and the scheduler switches to a runnable sibling, resuming it once a sibling
    /// `send`s. A child that never blocks runs to completion before the next starts — identical to
    /// the old FIFO run-to-completion drain, so non-blocking programs are byte-for-byte unchanged.
    /// The first child fault (or `std.os.exit`) aborts the remaining siblings and propagates; on that
    /// path the parent's restored `run_until` handles `recover:`/unwind in its own context.
    /// TASK B — cancel-and-report when a `parallel:` body escapes its `JoinNursery` early (`?` /
    /// `return` / `break` / `continue`) or when a fault unwinds past it. Pop every nursery entry ABOVE
    /// `from_len` (the level the escaping construct should restore to); for each lazy nursery that
    /// holds unstarted [`PendingCall`]s, write ONE report line to stdout (`out`, the stream the parity
    /// harnesses read) — emitting PER-NURSERY, innermost-first, byte-identical to the serial-VM oracle,
    /// whose `exec_parallel` / `leave_implicit_nursery` report once per frame/block as it unwinds (two
    /// stacked nurseries → two lines, not one combined `2 pending`). The tasks are then DROPPED: they
    /// never started, so there is no fiber to cancel and no buffered output to flush. This preserves
    /// the old `truncate`'s no-leak behavior (depth returns to `from_len`) and adds the observable
    /// report. Replaces the bare `self.nurseries.truncate(from_len)` at every reclaim site.
    pub(super) fn drain_escaped_nursery(&mut self, from_len: usize) {
        if self.nurseries.len() <= from_len {
            return; // nothing escaped past the join (e.g. normal fall-through already popped it)
        }
        while self.nurseries.len() > from_len {
            self.nursery_defer_floors.pop(); // lockstep with `nurseries`
            let mn_scope = self.mn_scopes.pop().flatten(); // lockstep — Some if early-enlisted
            let nursery = self.nurseries.pop().unwrap_or_default();
            // Cross-nursery flat scheduler — an EARLY-ENLISTED nursery's tasks are LIVE fibers already
            // seeded into the global sched (its `tasks` vec was drained), so an escape past its join must
            // CANCEL + drain them (trip the scope cancel, requeue parked, settle), exactly like an eager
            // nursery's `abort_eager_nursery`. No pending-task report (the tasks DID start).
            if let Some(scope_id) = mn_scope {
                self.abort_enlisted_scope(scope_id);
                // A `spawn:` issued after the enlist refilled `nursery` with unstarted late tasks; on an
                // escape they never started → report them cancelled (parity with the lazy arm below and
                // with coop), rather than silently dropping them. (Cross-nursery flat scheduler — #3.)
                if !nursery.is_empty() {
                    self.emit_out(&crate::runtime::pending_cancel_report(nursery.len()));
                }
                continue;
            }
            // Per-connection spawn — pop the eager scope in lockstep. An eager nursery's handlers are
            // already-started live fibers (no unstarted `PendingCall`s to count): cancel + drain + flush
            // them. A lazy nursery's entries are unstarted tasks → report one line per such nursery.
            match self.eager_scheds.pop().flatten() {
                Some(scope) => self.abort_eager_nursery(scope),
                None => {
                    if !nursery.is_empty() {
                        self.emit_out(&crate::runtime::pending_cancel_report(nursery.len()));
                    }
                }
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
        // order → parity). See `run_mn_nursery_nested`.
        if let Some(scope_id) = mn_scope {
            self.join_enlisted_scope(scope_id)?;
            // A `spawn:` issued AFTER this nursery was enlisted refilled the drained `tasks` vec (the
            // enlist `take()` emptied it, but `mn_scopes[i]` stayed `Some`). Those late tasks were NOT
            // part of the enlisted scope — run them now, at the join, exactly as the lazy path below
            // (coop runs nursery tasks at the join too; late spawns post-date the nested `inner()` join,
            // so they have no live inner peer → parity holds). Falls through to the normal task path:
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
        // D2b: under `--parallel`, run the tasks as lightweight M:N fibers on the OS-thread pool
        // (park-on-`recv`), instead of cooperative fibers (decision A keeps the cooperative path the
        // default below).
        if self.parallel {
            return self.run_mn_nursery(tasks);
        }
        // Task 1 — deep-copy the module globals into each child's OWN `module_objs` view (in the shared
        // heap) via `prepare_serial_child`, exactly as M:N does per worker. A cooperative child mutates
        // its private copy → invisible to the parent → `serial == M:N` by construction.
        // W6-2 — each task replays the snapshot PINNED at its own `spawn` (`register_task`), the one
        // instant both engines share. Snapshotting HERE instead would diverge: M:N may prepare a
        // nursery's tasks earlier, at a nested nursery's join (`early_enlist_outer`) or — for an eager
        // per-connection nursery — at the spawn itself. A task whose pin FAILED to build raises that
        // error here, at its preparation (a module-global generator with a non-sendable parked slot or
        // reference cycle, an over-deep global), already stamped with the spawn-site span.
        let mut children: Vec<Fiber> = Vec::with_capacity(tasks.len());
        for (i, t) in tasks.into_iter().enumerate() {
            let span = t.span();
            let (pending, module_objs, module_faulted) =
                self.prepare_serial_child(t.call, t.snap?)?;
            children.push(Fiber {
                span,
                ctx: FiberCtx {
                    module_objs,
                    module_faulted,
                    ..FiberCtx::default()
                },
                state: FiberState::Pending(pending),
                task_index: i,
                scope_id: 0,
                resume_native: None,
            });
        }
        // D0: every child starts `Pending` ⇒ runnable, so seed `ready` with all indices in order.
        let ready = (0..children.len()).collect();
        // Park the parent: move its live context into the nursery, leaving `self.*` as the fresh,
        // empty arena the children execute in. The nursery (parent + children) is GC-rooted while on
        // `scheduler_stack`.
        let mut nursery = Nursery {
            parent: FiberCtx::default(),
            children,
            ready,
            blocked_on: std::collections::HashMap::new(),
        };
        self.swap_ctx(&mut nursery.parent);
        self.scheduler_stack.push(nursery);
        let result = self.run_scheduler();
        // Tear the level down and restore the parent context on every path (normal / fault / exit).
        let mut nursery = self.scheduler_stack.pop().expect("scheduler level present");
        self.swap_ctx(&mut nursery.parent);
        result
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
    /// here), so the per-nursery-join flush ORDER — and three-engine parity for non-blocking nested
    /// spawns — is preserved. `self.mn_enlisted` counts those deferred scopes; `self.mn` stays installed
    /// until the LAST of them joins (`join_enlisted_scope` tears it down).
    pub(super) fn run_mn_nursery_outermost(
        &mut self,
        tasks: Vec<QueuedTask>,
    ) -> Result<(), RuntimeError> {
        let total = tasks.len();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut fibers = Vec::with_capacity(total);
        for (i, t) in tasks.into_iter().enumerate() {
            // W6-2 — each task replays the snapshot pinned at its own spawn (the serial engine replays
            // the same one). scope 0 — the outermost nursery.
            fibers.push(self.prepare_worker(t.call, Some(t.snap?))?.into_fiber(i, 0));
        }
        let deadlock_err = self.err(DEADLOCK_MSG.to_string(), Span { line: 1, col: 1 });
        // Worker count must account for the early-enlisted OUTER scopes' tasks too (case-A: `main`'s `O`),
        // so a multi-task inner nursery + outer siblings still gets real parallelism. We don't yet know
        // the outer totals here, so size to a reasonable upper bound (core count) capped by total work
        // after enlisting is impossible to know pre-register; use core count (the inline owner alone
        // still guarantees completion, helpers only accelerate).
        let nworkers = worker_count();
        let sched = Arc::new(MnSched::new(
            total,
            nworkers,
            Arc::clone(&cancel),
            deadlock_err,
        ));
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
        shell.mn_worker_loop(&sched, 0, 0); // owner of scope 0
        // The owner returned on scope 0; reduce scope 0's sub-range. The sched is released only when no
        // early-enlisted outer scope is still pending (else those scopes' slots must survive until their
        // own joins reduce them — `join_enlisted_scope` releases it at the last).
        sched.wait_for_scope(0);
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
            workers.push(self.prepare_worker(t.call, Some(t.snap?))?);
        }
        let scope_id =
            sched.register_scope_seeded(Arc::clone(&cancel), self.scope_ancestors(), workers);
        let wid = self.wid;
        let mut shell = self.spawn_shell(sched, &cancel);
        shell.mn_worker_loop(sched, wid, scope_id);
        sched.wait_for_scope(scope_id);
        let slots = sched.take_scope_slots(scope_id);
        self.reduce_task_slots(slots)
    }

    /// Cross-nursery flat scheduler — EARLY-ENLIST every OUTER still-pending nursery (those above the
    /// current one on `self.nurseries`) into `sched` as its own scope: seed its sibling tasks as live
    /// fibers (so a nested owner draining the GLOBAL queue can run them — the cross-nursery wake), drain
    /// its `tasks` vec, and record the scope on `self.mn_scopes` + bump `self.mn_enlisted` so its OWN
    /// `JoinNursery` reduces it (deferred — preserving per-nursery flush order and three-engine parity).
    /// Idempotent per nursery (skips any already-enlisted `Some(_)` and any empty one).
    pub(super) fn early_enlist_outer(&mut self, sched: &Arc<MnSched>) -> Result<(), RuntimeError> {
        // Independent/normal multi-level nesting is fully supported: every still-pending OUTER nursery is
        // enlisted as its own scope here, and the genuinely-CONTENDED case (2+ live receivers racing ONE
        // channel across nested nurseries) is NOT gated — it is concurrent-divergent by design (delivery
        // order may differ from the cooperative engine, or it may deadlock-fault; suspendable concurrency
        // is VM-only / divergent under `--parallel`, see PROGRESS.md). It must only never PANIC.
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
            // and `mn_scopes`/`mn_enlisted` are unbumped — the fault propagates cleanly, matching coop.
            let clones: Vec<QueuedTask> = self.nurseries[i].clone();
            // W6-2 — each OUTER task replays the pin taken at ITS OWN spawn, never one taken here:
            // enlisting happens at a NESTED nursery's join, an instant the serial engine never reaches,
            // so snapshotting here would diverge for a global mutated in between.
            let mut prepared = Vec::with_capacity(total);
            for t in clones {
                prepared.push(self.prepare_worker(t.call, Some(t.snap?))?);
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
        shell.mn_worker_loop(&sched, wid, scope_id);
        sched.wait_for_scope(scope_id);
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
        shell.mn_worker_loop(&sched, wid, scope_id);
        sched.wait_for_scope(scope_id);
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
    /// thread serves many); multi-core handler parallelism is future work.
    pub(super) fn activate_eager_nursery(&mut self) -> EagerScope {
        let cancel = Arc::new(AtomicBool::new(false));
        debug_assert!(
            self.mn.is_some(),
            "an eager nursery only activates on a worker shell (gated by mn.is_some())"
        );
        let deadlock_err = self.err(DEADLOCK_MSG.to_string(), Span { line: 1, col: 1 });
        // wid 0 = inline join worker; wid 1 = the dedicated raw drainer below.
        let mut inner = MnSched::new(0, 2, Arc::clone(&cancel), deadlock_err);
        // gaps.md B5 — this eager sched is PRIVATE (no link to the parent). A `send`/`close` inside its
        // body only scans its OWN parked set, so a receiver parked in the PARENT nursery on a shared
        // channel is never woken → the parent spuriously faults `deadlock`. Point `parent_wake` at the
        // sched the activating worker fiber is running on (its parent nursery — held in `self.mn`, or
        // `mn_enlist_sched` on the inline outermost builder) so `send_wake`/`close_wake` route the wake
        // up to it. Strictly upward: no cycle, and it wakes a receiver on the parent's HOME sched (its
        // outcome slot / JoinScope stay put).
        inner.parent_wake = self.mn.clone().or_else(|| self.mn_enlist_sched.clone());
        let sched = Arc::new(inner);
        // Structured concurrency — an eager nursery is a nested scope: its handlers must observe the
        // enclosing scopes' cancel too (`JoinScope::ancestors`).
        sched.lock().scopes[0].ancestors = self.scope_ancestors();
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
            .ok();
        EagerScope {
            sched,
            cancel,
            drainer,
        }
    }

    /// Per-connection spawn — `JoinNursery` for an eager nursery (the normal fall-through path). Close
    /// the body (no more injections → the sched may terminate once every handler is done), then run
    /// the inline join worker (`wid` 0) to help drain remaining handlers, wait for every slot to fill,
    /// and reduce (Decision-F output flush in spawn order; a handler fault propagates as the
    /// acceptor's body fault, which the outer nursery then sees). Mirrors `run_mn_nursery`'s tail.
    pub(super) fn join_eager_nursery(&mut self, scope: EagerScope) -> Result<(), RuntimeError> {
        let EagerScope {
            sched,
            cancel,
            drainer,
            ..
        } = scope;
        sched.close_body(0);
        let mut shell = self.spawn_shell(&sched, &cancel);
        shell.mn_worker_loop(&sched, 0, 0);
        sched.wait_for_completion();
        if let Some(h) = drainer {
            let _ = h.join();
        }
        let slots = sched.take_slots();
        self.reduce_task_slots(slots)
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
            ..
        } = scope;
        // N4 — trip the cancel UNDER the core lock (`trip_scope_cancel`, scope 0 = the eager scope, whose
        // `JoinScope::cancel` IS this `cancel` Arc) and BEFORE `close_body` clears the `any_body_open`
        // veto: gapless veto handoff, and the store is published to any worker that takes the core lock
        // to evaluate the deadlock predicate (a bare `Relaxed` store outside the lock has no
        // synchronizes-with edge, so a worker could read a stale `false` and reap this scope's parked
        // handlers as `Deadlocked` — dropping them without `unwind_deferred`, skipping their `defer`s).
        sched.trip_scope_cancel(0);
        sched.close_body(0);
        sched.cancel_drain(0);
        poller::drain_sched(&sched);
        let mut shell = self.spawn_shell(&sched, &cancel);
        shell.mn_worker_loop(&sched, 0, 0);
        sched.wait_for_completion();
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
                .expect("demote_recv_block on the cooperative engine"),
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
                // tripped flag and truncates the defer mid-body — on M:N only, since serial runs the same
                // call inline. The predicate's `deferring == 0` term is what keeps cleanup atomic, and it
                // also folds in `cancel_outer` (an ENCLOSING scope's cancel), which a raw read misses.
                if self.cancel_requested() {
                    self.cancelled = true;
                    c.running += 1;
                    sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
                    c.unregister_demoted(ptr);
                    c.unwatch_demoted_cancel(tok);
                    drop(c);
                    return Err(self.err("cancelled".to_string(), span));
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
                .expect("demote_wait_block on the cooperative engine"),
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
            if q.is_empty() {
                // Clamp the backoff to the timer deadline so the loop re-polls and fires the timer arm
                // by its deadline (saturating, so a deadline that already passed yields ~zero wait).
                let backoff = match timer {
                    Some((_, d)) => DEMOTE_POLL_BACKOFF
                        .min(d.saturating_duration_since(std::time::Instant::now())),
                    None => DEMOTE_POLL_BACKOFF,
                };
                let _ = first.cv.wait_timeout(q, backoff);
            }
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
        let sched = Arc::clone(
            self.mn
                .as_ref()
                .expect("demote_block_sleep on the cooperative engine"),
        );
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
        // 3. Sleep in place (the worker is covered by the replacement).
        std::thread::sleep(std::time::Duration::from_millis(ms));
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
        Ok(Value::nil())
    }

    /// D5 owe #3 Path C (#3 socket half) — a socket `read`/`write`/`accept` that `WouldBlock`s INSIDE a
    /// native callback (`native_reentry > 0`) on the M:N engine. [`Vm::park_on_fd`] only parks on the
    /// netpoller when `native_reentry == 0` (the callback's `for`-loop state lives on the un-snapshottable
    /// Rust host stack), so without this the op surfaces a misleading `--parallel`-engine error even
    /// though we *are* on `--parallel`. Instead DEMOTE like [`Vm::demote_block_sleep`]: spin a replacement
    /// worker once + backoff-poll the **non-blocking** op in place until it's ready, then resume.
    ///
    /// Accounted as `inflight` (NOT `blocked_native`): a socket op is woken by external OS readiness, so
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
        let sched = Arc::clone(
            self.mn
                .as_ref()
                .expect("demote_block_socket on the cooperative engine"),
        );
        self.demote_socket_enter(span)?;
        let out = loop {
            // Observe teardown/cancel BEFORE doing more work each iteration. Cancel (a sibling faulted):
            // set `cancelled` so the outcome is SWALLOWED (a cancelled task is dropped, not reported).
            if self.cancel_requested() {
                self.cancelled = true;
                break Err(self.err("cancelled".to_string(), span));
            }
            // Nursery torn down (deadlock elsewhere / `os.exit`): fault in place. An `inflight` socket op
            // never *self*-fires the predicate (it vetoes it), so a genuine quiesce is surfaced by another
            // worker setting `terminate`, observed here within the backoff.
            if sched.lock().terminate {
                break Err(sched.deadlock_err.clone());
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
        self.demote_socket_exit();
        out
    }

    /// D5 owe #3 Path C (#3 socket half) — enter the in-callback socket demote: account `running → inflight`
    /// under core lock A + notify idle pullers (a worker in an untimed `cv.wait` re-evaluates now that this
    /// fiber left `running`), then spin a replacement worker ONCE (reuse the `self.demoted` coverage the
    /// recv/sleep demote also uses — one spawn + one eventual exit per demoted thread). On OS-refuse, un-roll
    /// the accounting and fault cleanly so the join still completes. Mirrors [`Vm::demote_block_sleep`] 1–2.
    pub(super) fn demote_socket_enter(&mut self, span: Span) -> Result<(), RuntimeError> {
        let sched = Arc::clone(
            self.mn
                .as_ref()
                .expect("demote_socket_enter on the cooperative engine"),
        );
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
        let sched = Arc::clone(
            self.mn
                .as_ref()
                .expect("demote_socket_exit on the cooperative engine"),
        );
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
    /// repeat until the scheduler terminates. Generalizes the cooperative [`Vm::run_child`] to a
    /// shared run queue + park set across threads.
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
                    let aborts = matches!(
                        outcome,
                        TaskOutcome::Fault { .. } | TaskOutcome::Exit { .. }
                    );
                    sched.finish(task_index, scope_id, outcome);
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
            if self.demoted {
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
        // No-op for the cooperative engine / a non-fiber run (`mn` is `None` there).
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
                    Err(rte) => return Disp::Finish(self.classify_mn_outcome(Err(rte))),
                }
            }
            // D6b — a fiber resumed from a non-blocking `connect` park carries the connecting socket in
            // `pending_connect` (swapped in with its ctx). Complete the handshake (read `SO_ERROR`) and
            // push the `Result[Socket]` the `net.connect` call site is waiting for, then continue past
            // it. `finish_pending_connect` never faults (it yields a `Result` *value*). Mutually
            // exclusive with `resume_native` (a fiber is offload-parked OR connect-parked, never both).
            if let Some(cip) = self.pending_connect.take() {
                let v = self.finish_pending_connect(cip);
                self.push(v);
            }
            let res = match state {
                FiberState::Pending(task) => self.start_task(task),
                FiberState::Ready | FiberState::Blocked => self.run_until(0),
                FiberState::Done => unreachable!("mn_worker_loop scheduled a Done fiber"),
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
            out: String::new(),
            stderr: String::new(),
        }
    }

    /// D2b — classify a finished fiber's run into a [`TaskOutcome`] (the M:N analogue of
    /// [`ReadyWorker::run_outcome`]). Unlike the legacy path it uses `start_task`/`run_until` and
    /// **discards the task's return value**, matching the cooperative parity oracle (which never
    /// inspects a task's return). Trips the shared cancel flag on a fault/exit so siblings abort. The
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

    /// B3.3-threads / B3.6 — the engine-agnostic farm/join/flush core: run a vector of already-prepared
    /// [`ReadyWorker`]s on the bounded pool and reduce their outcomes. The caller wires each worker's
    /// `cancel` / `deadlock` token first ([`run_parallel_nursery`] sets both; the `Executor` pool drain
    /// sets `cancel` only — an `Executor`-spanning deadlock is an accepted hang, decision D). Farms
    /// `ready[1..]` to the pool, runs `ready[0]` inline (parent participates — decision B), joins on the
    /// `DoneSignal` counter, flushes `Done`/`Exit` output in **task order** (decision F), and applies the
    /// `Exit`-over-`Fault` precedence (an `os.exit` hard-halts the parent; a fault unwinds for an outer
    /// `recover:`). A `Cancelled` outcome is swallowed.
    pub(super) fn run_workers_on_pool(
        &mut self,
        ready: Vec<ReadyWorker>,
    ) -> Result<(), RuntimeError> {
        let n = ready.len();
        // Per-task outcome slots (task order) + a finished-count condvar the pool bumps.
        let results: TaskSlots = Arc::new(Mutex::new((0..n).map(|_| None).collect()));
        let done: Arc<(Mutex<usize>, std::sync::Condvar)> =
            Arc::new((Mutex::new(0), std::sync::Condvar::new()));

        // 2. Farm tasks[1..] to the pool; keep tasks[0] to run inline. Every farmed job runs under a
        //    `DoneSignal` guard whose `Drop` bumps the completion counter + wakes the joiner on EVERY
        //    exit path — including a Rust panic unwinding through `rw.run()` (a worker-VM `unwrap` /
        //    poisoned core lock). Without it a panicking task would leave the counter short and hang
        //    the join forever; with it the panic is caught, converted to a fault slot, and joined like
        //    any other error. (Review: panic→hang was the one blocking defect.)
        let mut iter = ready.into_iter().enumerate();
        let first = iter.next();
        for (i, rw) in iter {
            let results = Arc::clone(&results);
            let done = Arc::clone(&done);
            let span = rw.span;
            pool::submit(Box::new(move || {
                // Drop runs LAST (declared first), so the slot is committed before the counter bumps.
                let _signal = DoneSignal(done);
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rw.run_outcome()))
                    .unwrap_or_else(|p| TaskOutcome::Fault {
                        err: panic_to_fault(p, span),
                        out: String::new(),
                        stderr: String::new(),
                    });
                results.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(r);
            }));
        }
        // 3. Parent participates: run task[0] on this thread (it may block on `recv`, woken by a pool
        //    sibling's `send`). Caught the same way so an inline-task panic still joins the pool tasks
        //    and reports rather than unwinding past the still-pending wait.
        if let Some((i, rw)) = first {
            let span = rw.span;
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rw.run_outcome()))
                .unwrap_or_else(|p| TaskOutcome::Fault {
                    err: panic_to_fault(p, span),
                    out: String::new(),
                    stderr: String::new(),
                });
            results.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(r);
        }
        // 4a. Wait for the farmed tasks (n-1) to finish (the `DoneSignal` guard guarantees the counter
        //     reaches `pool_count` even if some tasks panicked).
        let pool_count = n.saturating_sub(1);
        {
            let (lock, cv) = &*done;
            let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
            while *g < pool_count {
                g = cv.wait(g).unwrap_or_else(|e| e.into_inner());
            }
        }
        // 4b. Flush worker output in task order (decision F) and select the terminal outcome.
        //     `Done`/`Exit` output is flushed; the terminal (lowest-index propagating) `Fault` flushes
        //     its buffered output at its slot too (oracle parity — a faulting task's partial output is
        //     emitted before the fault unwinds); higher-index racy `Fault`s + `Cancelled` still drop
        //     (no deterministic slot). The fault-free goldens only ever hit `Done`, so byte-identical.
        //
        //     Precedence: an `os.exit` is an UNCONDITIONAL hard halt, so the lowest-index `Exit`
        //     wins over any `Fault` regardless of index — otherwise a recoverable sibling fault at a
        //     lower index could demote a child's `os.exit` to a catchable error (a `recover:` around
        //     the `parallel:` would swallow it and the process would not exit). Within a kind, the
        //     lowest index wins (scan order + `is_none()` guard), matching the cooperative engine's
        //     first-fault rule.
        // Take the slots out under the lock rather than `Arc::try_unwrap`: a just-finished pool
        // thread bumps the `done` counter (in `DoneSignal::drop`) *before* its closure environment —
        // which still owns a `results` `Arc` clone — is dropped, so the joiner can wake with
        // `strong_count > 1` and `try_unwrap` would spuriously fail. `mem::take` needs only the lock.
        let slots = std::mem::take(&mut *results.lock().unwrap_or_else(|e| e.into_inner()));
        self.reduce_task_slots(slots)
    }

    /// B3.3-threads / D2b — reduce a nursery's per-task outcome slots (task order) into the join's
    /// result, flushing output and applying `Exit`-over-`Fault` precedence. Shared by the legacy pool
    /// engine ([`run_workers_on_pool`]) and the M:N engine ([`run_mn_nursery`]).
    ///
    /// `Done`/`Exit` output is flushed in task order (decision F). The terminal (lowest-index
    /// propagating) `Fault` ALSO flushes its buffered output at its slot — matching the cooperative/
    /// interp oracle, which writes a faulting task's partial output before the fault unwinds. Higher-
    /// index racy `Fault`s and `Cancelled` still drop (no deterministic slot — the work is incomplete /
    /// ran past the terminal fault's cancel). The fault-free goldens only ever hit `Done`, so they stay
    /// byte-identical. A `Deadlocked` slot (the M:N deadlock-abort synthetic outcome — every parked
    /// fiber gets one; a real `Fault`/`Exit` normally trips `terminate` first, and the precedence below
    /// resolves any mix deterministically) is different: ALL parked
    /// buffers flush in task order (not just the lowest-index one), matching serial's live prints, and
    /// ONE deadlock error propagates. Precedence: an `os.exit` is an UNCONDITIONAL hard halt, so the lowest-index
    /// `Exit` wins over any `Fault` regardless of index — otherwise a lower-index recoverable fault
    /// could demote a child's `os.exit` to a catchable error. Within a kind, the lowest index wins
    /// (scan order + `is_none()`).
    pub(super) fn reduce_task_slots(
        &mut self,
        slots: Vec<Option<TaskOutcome>>,
    ) -> Result<(), RuntimeError> {
        let mut first_exit: Option<i32> = None;
        let mut first_fault: Option<RuntimeError> = None;
        let mut deadlock_err: Option<RuntimeError> = None;
        for slot in slots {
            match slot.expect("every task slot was filled before join returned") {
                TaskOutcome::Done(wr) => {
                    self.out.push_str(&wr.out);
                    self.stderr.push_str(&wr.stderr);
                }
                TaskOutcome::Exit { code, out, stderr } => {
                    self.out.push_str(&out);
                    self.stderr.push_str(&stderr);
                    if first_exit.is_none() {
                        first_exit = Some(code);
                    }
                }
                TaskOutcome::Fault { err, out, stderr } => {
                    // The terminal (lowest-index propagating) fault flushes its buffered output at its
                    // task-order slot — after lower-index Done/Exit, before the fault propagates —
                    // so a faulting task's partial output is no longer silently dropped. Higher-index
                    // racy faults still drop (they ran concurrently past the terminal fault's cancel;
                    // no deterministic slot position).
                    //
                    // RESIDUAL RACE (intentionally not chased here — applies ONLY to a genuine
                    // multi-printer REAL-fault reduce; the multi-parked DEADLOCK case is handled by
                    // the `Deadlocked` arm below, which flushes ALL parked buffers): this matches the
                    // cooperative/interp oracle byte-for-byte only when the faulting task is the
                    // nursery's SOLE output-producer. With additional output-producing siblings the M:N result can
                    // still diverge from serial's strict stop-at-first-fault order — a sibling that
                    // reaches `Done` before the faulter's cancel-trip keeps its output (serial would
                    // never have run it), and whether a lower-index sibling ends `Fault` vs `Cancelled`
                    // (which selects the propagating fault) is itself a scheduler race. The
                    // buffer-and-flush-per-task model cannot reconcile concurrency with serial's
                    // sequential stop-at-fault, so multi-task-with-fault output ordering is a separate,
                    // pre-existing nondeterminism, not asserted as parity (see the single-task test
                    // `parallel_faulting_task_flushes_partial_output_3engine`).
                    if first_fault.is_none() {
                        self.out.push_str(&out);
                        self.stderr.push_str(&stderr);
                        first_fault = Some(err);
                    }
                }
                TaskOutcome::Deadlocked { err, out, stderr } => {
                    // The M:N deadlock detector recorded EVERY still-parked fiber with this synthetic
                    // outcome (`flag_deadlock`). Unlike a real `Fault`, ALL parked buffers flush at
                    // their task-order slot (no `is_none()` gate) — so with two-or-more parked fibers
                    // a higher-index printer's output is preserved, matching the serial engine which
                    // printed those lines live before the deadlock returned. Only ONE deadlock error
                    // propagates (the first, i.e. lowest task-order); a real fault/exit normally trips
                    // `terminate` before the detector fires, but the terminal `match` below applies a
                    // strict `Exit` > `Fault` > `Deadlocked` precedence so a mixed vector (were one to
                    // arise under a race) still resolves deterministically.
                    self.out.push_str(&out);
                    self.stderr.push_str(&stderr);
                    if deadlock_err.is_none() {
                        deadlock_err = Some(err);
                    }
                }
                TaskOutcome::Cancelled { out, stderr } => {
                    // A cancelled task's buffered output flushes at its task-order slot (it really
                    // printed those bytes — with cancellation points a started task always completes
                    // its prologue), matching serial, which prints live and cannot un-print. Cross-task
                    // ORDER stays nondeterministic on both engines; the line SET is the contract.
                    self.out.push_str(&out);
                    self.stderr.push_str(&stderr);
                }
            }
        }
        match (first_exit, first_fault, deadlock_err) {
            // A child `os.exit` hard-halts the parent: set `pending_exit` and return the exit
            // sentinel. The op→`step`→`run_until` chain sees `pending_exit` and unwinds past every
            // `recover:` to the driver, which reports `code` as the process exit status (decision C).
            // It wins over any sibling fault — a hard halt is never demoted to a catchable error.
            (Some(code), _, _) => {
                self.pending_exit = Some(code);
                Err(self.err("exit".to_string(), Span { line: 1, col: 1 }))
            }
            // A real fault propagates normally so an outer `recover:` can still catch it.
            (None, Some(e), _) => Err(e),
            // Deadlock abort: all parked buffers already flushed above; propagate ONE deadlock error.
            (None, None, Some(e)) => Err(e),
            (None, None, None) => Ok(()),
        }
    }

    /// Cooperatively drive the children of the innermost scheduler level until all are `Done`. D0:
    /// pops the lowest-index runnable child from the level's `ready` set each turn (O(log N), vs the
    /// old O(N) `pick_runnable` linear scan — same lowest-index order, so byte-identical). A child
    /// that blocks on an empty channel leaves `ready` and is re-added by a sibling `send`
    /// ([`Vm::wake_on_send`]). When `ready` empties: all children `Done` ⇒ the nursery is finished;
    /// otherwise every remaining child is parked on an empty channel no sibling can fill — a deadlock.
    pub(super) fn run_scheduler(&mut self) -> Result<(), RuntimeError> {
        // A nursery scope's cancel state is per-scope on M:N (a fresh flag cloned into each worker,
        // `cancelled` reset at every fiber swap-in). Serial keeps both in VM-globals, so save/restore
        // them around the level. What the level STARTS with:
        //
        // * The enclosing scope's cancel flag is INHERITED (structured concurrency: cancelling a scope
        //   cancels its descendants). Severing it would make a nested `parallel:` inside a cancelled
        //   task uncancellable — its children's back-edge checkpoints would have no flag to read and a
        //   spinning grandchild would hang the drain forever.
        // * …EXCEPT inside a `defer` (`deferring > 0`), where the enclosing cancel must NOT reach:
        //   a `parallel:` in a cancelled task's cleanup gets a clean slate and runs to completion (and
        //   `deferring` itself resets, so THAT nursery can still cancel its OWN children on a fault).
        // * `cancelled` starts false — this level's fibers have not observed anything yet.
        //
        // On the way out `cancelled` is restored, EXCEPT when the level propagates an error while the
        // enclosing scope is itself cancelled: the parent fiber is dying of that cancel, so the latch
        // stays set and it unwinds as a cancel (defers run, `recover:` bypassed — exec.rs) rather than
        // as a catchable fault. Conversely a leaked `cancelled == true` from an ordinary child fault
        // would bypass the PARENT's `recover:`, which is what the restore prevents.
        let in_defer = self.deferring > 0;
        let saved_cancel = if in_defer {
            self.cancel.take()
        } else {
            self.cancel.clone()
        };
        let saved_cancelled = std::mem::replace(&mut self.cancelled, false);
        let saved_deferring = std::mem::replace(&mut self.deferring, 0);
        let r = self.run_scheduler_level();
        self.deferring = saved_deferring;
        self.cancel = saved_cancel;
        self.cancelled = saved_cancelled;
        if r.is_err() && self.cancel_requested() {
            self.cancelled = true;
        }
        r
    }

    fn run_scheduler_level(&mut self) -> Result<(), RuntimeError> {
        loop {
            let next = self
                .scheduler_stack
                .last_mut()
                .expect("scheduler level present")
                .ready
                .pop_first();
            match next {
                Some(i) => {
                    if let Err(e) = self.run_child(i) {
                        // N6 — the child faulted / `os.exit`ed. Its siblings must not be abandoned
                        // where they sit: cancel them and re-drive each one so it unwinds through
                        // its `defer`s (Go runs a cancelled goroutine's deferred functions; `defer`
                        // is Chezzi's only cleanup mechanism). M:N already did this (`cancel_drain`
                        // + the fibers' cancel-recheck-on-park); serial used to propagate this `Err`
                        // straight out with a bare `?`. THIS is serial's ONLY child-error
                        // propagation point — every other nursery path is `self.parallel`-gated.
                        return Err(self.drain_cancelled_children(i, e));
                    }
                }
                None => {
                    if self.all_children_done() {
                        return Ok(());
                    }
                    return Err(self.err(DEADLOCK_MSG.to_string(), Span { line: 1, col: 1 }));
                }
            }
        }
    }

    /// N6 — serial's cancel teardown, the cooperative twin of M:N's `cancel_drain`: after child
    /// `faulted` of the innermost level faults or `os.exit`s, drive every still-PARKED sibling to a
    /// real end so each runs its `defer`s, then return the error the nursery propagates.
    ///
    /// Every sibling that is not already `Done` is driven, in task order, with the cancel flag
    /// tripped (serial has no worker race, so this is ordering, not locking) — INCLUDING a `Pending`
    /// (never-started) one. That is not a nicety: M:N is structurally forced to start every spawned
    /// fiber (a scope only completes at `done == total`, and `take_runnable` never consults the scope
    /// cancel), so a cancelled-scope M:N sibling still runs its prologue, prints, and runs any `defer`
    /// it registers. Serial must do the same or the two engines disagree on the LINE SET — the very
    /// thing this contract declares parity (measured before this drain: `spawn boom(); spawn talker()`
    /// → serial `{"0"}`, M:N `{"hi", "42"}`, 20/20). Cancellation points make the two orders converge:
    /// a started task always runs its straight-line prologue, then dies at its first checkpoint.
    ///
    /// `cancelled` is a per-VM latch, so it is cleared before EACH child (else only the first sibling
    /// unwinds). `pending_exit` too (`unwind_deferred`, stmt.rs, refuses to run ANY defer while an
    /// exit is pending — one task's exiting `defer` would otherwise poison every later sibling's
    /// cleanup). Exits and faults are then REDUCED exactly like M:N's `reduce_task_slots`: `Exit`
    /// beats `Fault` (a hard halt is never demoted to a catchable error), and within each, the lowest
    /// task index wins — so a drained sibling that faults *ahead* of `faulted` propagates ITS error,
    /// as M:N's slot reduction does. A drained child that dies of the cancel itself is not a fault.
    ///
    /// Output: a drained child's bytes STAY (serial prints live into the shared `out` and cannot
    /// un-print; M:N now flushes a `Cancelled` fiber's buffer at its task slot for the same reason).
    /// Cross-task ORDER is nondeterministic on both engines and is not part of the parity contract.
    ///
    /// Genuine DEADLOCK of THIS level (all parked, nothing cancelled) is reported from
    /// `run_scheduler_level`'s `None` arm, which never reaches here — so a deadlocked level still
    /// tears its parked fibers down without running their defers, identically on both engines
    /// (docs/gaps.md §N5, unchanged).
    fn drain_cancelled_children(&mut self, faulted: usize, err: RuntimeError) -> RuntimeError {
        // Exit-over-fault, lowest task index wins — M:N's `reduce_task_slots` precedence.
        let mut first_exit: Option<(usize, i32)> = self.pending_exit.take().map(|c| (faulted, c));
        let mut first_fault: (usize, RuntimeError) = (faulted, err);
        let cancel = Arc::new(AtomicBool::new(true));
        let n = self
            .scheduler_stack
            .last()
            .expect("scheduler level present")
            .children
            .len();
        for i in 0..n {
            if i == faulted || matches!(self.child_state(i), FiberState::Done) {
                continue;
            }
            self.cancel = Some(Arc::clone(&cancel));
            self.cancelled = false;
            self.pending_exit = None;
            let r = self.run_child(i);
            if let Err(e) = r
                && !self.cancelled
                && i < first_fault.0
            {
                // A real fault (not the cancel unwind) from a sibling AHEAD of the faulter: M:N's
                // slot reduction reports the lowest-index fault, so serial must too.
                first_fault = (i, e);
            }
            if let Some(code) = self.pending_exit.take() {
                // The child's `defer` (or its cancelled-but-still-running body) called `os.exit` —
                // carry the code (M:N's `Exit` arm does the same); never discard it.
                if first_exit.is_none_or(|(j, _)| i < j) {
                    first_exit = Some((i, code));
                }
            }
        }
        self.cancel = None; // `run_scheduler` restores the enclosing scope's flag on the way out
        match first_exit {
            Some((_, code)) => {
                self.pending_exit = Some(code);
                self.err("exit".to_string(), Span { line: 1, col: 1 })
            }
            None => first_fault.1,
        }
    }

    fn child_state(&self, i: usize) -> &FiberState {
        &self
            .scheduler_stack
            .last()
            .expect("scheduler level present")
            .children[i]
            .state
    }

    pub(super) fn all_children_done(&self) -> bool {
        self.scheduler_stack
            .last()
            .expect("scheduler level present")
            .children
            .iter()
            .all(|c| matches!(c.state, FiberState::Done))
    }

    /// Run (start or resume) child `i` of the top scheduler level until it completes or blocks. The
    /// child is taken out of the level (replaced by a `Done` placeholder) so its context can be
    /// swapped into `self.*` without holding a `scheduler_stack` borrow across the run — a nested
    /// `parallel:` pushes/pops its own level meanwhile. On return the child's context is parked back
    /// and its new state recorded.
    pub(super) fn run_child(&mut self, i: usize) -> Result<(), RuntimeError> {
        let mut child = {
            let level = self
                .scheduler_stack
                .last_mut()
                .expect("scheduler level present");
            std::mem::replace(
                &mut level.children[i],
                Fiber {
                    ctx: FiberCtx::default(),
                    state: FiberState::Done,
                    task_index: i,
                    scope_id: 0,
                    span: Span { line: 1, col: 1 },
                    resume_native: None,
                },
            )
        };
        self.swap_ctx(&mut child.ctx); // self.* = child's execution context
        self.suspend = None; // clear any prior wait before (re)running
        self.wait_suspend = None;
        self.send_suspend = None;
        // `wait` (§6d) multi-channel park: this fiber may be filed under SEVERAL `blocked_on` keys.
        // A sibling `send` to ONE of them woke it (draining only that bucket); sweep the index out of
        // every other bucket here, before it re-runs, so a later `send` to one of those channels can
        // never re-wake a fiber that already moved on (the doc's "swept out of the other buckets").
        // A no-op for an ordinary single-`recv` park (already removed from its one bucket by the wake).
        if let Some(level) = self.scheduler_stack.last_mut() {
            for bucket in level.blocked_on.values_mut() {
                bucket.retain(|&x| x != i);
            }
            level.blocked_on.retain(|_, v| !v.is_empty());
        }
        let outcome = match std::mem::replace(&mut child.state, FiberState::Ready) {
            FiberState::Pending(task) => self.start_task(task),
            // Resume: the saved frames replay via the rewound `recv` op and ordinary `Return`s — no
            // host-stack nesting is rebuilt (run_until is frame-count driven).
            FiberState::Ready | FiberState::Blocked => self.run_until(0),
            FiberState::Done => unreachable!("run_child on a Done fiber"),
        };
        self.swap_ctx(&mut child.ctx); // park the (possibly-suspended) context back into the child
        // D0: a run always ends `Done` or `Blocked` (never left `Ready`), so a finished child is
        // simply dropped from scheduling; a blocked child registers in `blocked_on` under its
        // channel's core pointer so a sibling `send` can re-add it to `ready` ([`wake_on_send`]).
        let result = match outcome {
            Ok(()) => {
                child.state = if let Some(handles) = self.wait_suspend.take() {
                    // `wait` blocking park: file this child under EVERY live arm-channel key, so a
                    // sibling that frees the gap re-runs the `WaitPoll` (which re-polls source order).
                    // Kind-agnostic here — a recv waiter wakes on a `send`, a send waiter on a `recv`
                    // (both drain `blocked_on[key]` via `wake_on_send`/`wake_senders`); the re-poll
                    // sorts out which arm is actually ready.
                    for (h, _is_send) in handles {
                        let key = self.channel_core_ptr(h);
                        self.scheduler_stack
                            .last_mut()
                            .expect("scheduler level present")
                            .blocked_on
                            .entry(key)
                            .or_default()
                            .push(i);
                    }
                    FiberState::Blocked
                } else {
                    // A `recv` park (`suspend`) OR a full-bounded-`send` park (`send_suspend`): both
                    // file under the channel key so a sibling that frees the gap (`send` for a recv
                    // waiter, `recv` for a send waiter — both drain `blocked_on[key]` via
                    // `wake_on_send`/`wake_senders`) re-runs this fiber, which re-checks + proceeds.
                    match self.suspend.take().or_else(|| self.send_suspend.take()) {
                        Some(h) => {
                            let key = self.channel_core_ptr(h);
                            self.scheduler_stack
                                .last_mut()
                                .expect("scheduler level present")
                                .blocked_on
                                .entry(key)
                                .or_default()
                                .push(i);
                            FiberState::Blocked
                        }
                        None => FiberState::Done,
                    }
                };
                Ok(())
            }
            Err(e) => {
                child.state = FiberState::Done;
                Err(e)
            }
        };
        self.scheduler_stack
            .last_mut()
            .expect("scheduler level present")
            .children[i] = child;
        result
    }

    /// D0 — the `ChannelCore` identity (`Arc::as_ptr as usize`) behind a channel handle, the stable
    /// key for [`Nursery::blocked_on`]. Stable across the distinct `GcRef`s sibling fibers hold for
    /// the same channel (cooperative `spawn` deep-clones the handle onto the shared `Arc`).
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

    /// D0 — a `send` into channel `h` may unblock siblings parked on its `recv`. Drain the matching
    /// `blocked_on` bucket back onto `ready` for **every** scheduler level (not just the innermost):
    /// a fiber nested in an inner `parallel:` can `send` to a channel an outer-level sibling parked
    /// on, and that outer fiber must become runnable once control unwinds back to its level. No-op
    /// under `--parallel` (workers never push `scheduler_stack`) and when no sibling is parked.
    pub(super) fn wake_on_send(&mut self, h: GcRef) {
        if self.scheduler_stack.is_empty() {
            return;
        }
        let key = self.channel_core_ptr(h);
        for level in &mut self.scheduler_stack {
            if let Some(woken) = level.blocked_on.remove(&key) {
                level.ready.extend(woken);
            }
        }
    }

    /// Launch a fiber's initial task in the (already swapped-in) child context. Mirrors the old
    /// `run_pending`, but a blocking `recv` may park the fiber mid-flight: the `do_method_call` /
    /// `invoke_value` paths leave `self.suspend` set and the frames live, so the discard-pop is
    /// skipped (there is no result yet) and the scheduler resumes the fiber later.
    pub(super) fn start_task(&mut self, task: PendingCall) -> Result<(), RuntimeError> {
        match task {
            PendingCall::Call { callee, args, span } => {
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
    /// and `Channel` / `Shared` handles pass by reference (the handle is what crosses). Mirrors
    /// `interp::deep_clone` exactly. Allocates, but only at the instruction boundary that called it
    /// (no GC runs mid-clone), so intermediate handles can't be collected.
    ///
    /// Implemented as a [`WireValue`] round-trip — `to_wire` (read-only serialize) then `from_wire`
    /// (reconstruct into this heap). Byte-identical to the old direct deep-copy; the wire form is what
    /// de-risks the cores-as-`Arc` and real-OS-thread-boundary crossings. By-reference objects cross
    /// as `Handle`. Since F3 path-C a live generator crosses BY VALUE (its parked frame serialized,
    /// rebuilt as an independent copy on `from_wire`), so this is **fallible** only for the generator
    /// HARD ARMS `to_wire` rejects — a generator suspended mid-`recover:` (live handler) or with a
    /// pending `defer`, or one whose parked slot is itself non-sendable — each re-stamped with the real
    /// spawn-site `span` (the caller has it) via `to_wire_at`, a catchable error instead of a panic.
    pub(super) fn deep_clone(&mut self, v: Value, span: Span) -> Result<Value, RuntimeError> {
        let w = self.to_wire_at(v, span)?;
        Ok(self.from_wire(w))
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
    pub(super) fn to_wire_crossable(
        &self,
        v: Value,
        span: Span,
    ) -> Result<WireValue, RuntimeError> {
        let w = self.to_wire_at(v, span)?;
        self.ensure_crossable(&w, span)?;
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
        // Fresh memo per root: a cell sent as two separate spawn args stays independent (each is its
        // own serialization). The memo is back-edge-only (see [`WireMemo`]), so within one root a
        // Cell/Closure cycle round-trips while an off-path alias is deep-copied.
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
            return Err(self.depth_exceeded_err(Span { line: 0, col: 0 }));
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
                // builtins are name-compared, so that is observationally identical. Works on the M:N
                // engine (this path) and the serial engine (`SnapValue::Builtin`) alike.
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
                Obj::Native { name, func } => WireValue::Native {
                    name: name.clone(),
                    func: *func,
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
                            Span { line: 0, col: 0 },
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
                            // CHECKER-UNREACHABLE HARD ARMS — reject cleanly (same code path on both
                            // engines → byte-identical error) rather than silently mis-serialize.
                            // Neither shape can arise from checker-valid source; these are defensive
                            // guards against the type-blind compiler path (see the parity tests).
                            //  - multi-frame (a): `yield` only fires in the generator's own body frame
                            //    (`in_generator` resets at every fn/closure boundary), so a suspended
                            //    generator always has exactly one frame.
                            //  - pending `defer` (c): `defer` is banned inside a generator body.
                            // A mid-`recover:` suspension (arm b) is NO LONGER rejected — a live
                            // handler stack is pure plain-data and now serialized below.
                            if g.ctx.frames.len() != 1 {
                                return Err(self.err(
                                    "a generator suspended across more than one call frame cannot be sent across tasks".to_string(),
                                    Span { line: 0, col: 0 },
                                ));
                            }
                            let frame = &g.ctx.frames[0];
                            if !frame.deferred.is_empty() {
                                return Err(self.err(
                                    "a generator suspended with a pending `defer` cannot be sent across tasks".to_string(),
                                    Span { line: 0, col: 0 },
                                ));
                            }
                            // The sole body frame's home/closure equal the core's by construction
                            // (`push_frame(proto, g.home, g.closure, …)`); assert defensively and
                            // reject rather than wire a mismatched frame.
                            if frame.home != g.home || frame.closure != g.closure {
                                return Err(self.err(
                                    "a generator with an inconsistent parked frame cannot be sent across tasks".to_string(),
                                    Span { line: 0, col: 0 },
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
                // plain captured local sent into a task is an isolated copy (design §4 F1).
                //
                // Identity preservation (see the `Obj::Closure` arm): assign `h` an `id` and record it
                // in `memo.path` BEFORE recursing `inner`, so a nested back-edge to this cell (the
                // letrec self-cell a recursive local `fn`, or a mutual-recursion loop, closes) emits
                // `WireValue::Backref(id)` and stops. Removed from `path` on exit, so an off-path alias
                // is deep-copied independently (the deep-copy-independence contract holds).
                Obj::Cell(v) => {
                    if let Some(&id) = memo.path.get(&h) {
                        WireValue::Backref(id)
                    } else {
                        let id = memo.next_id;
                        memo.next_id += 1;
                        memo.path.insert(h, id);
                        let inner = self.to_wire_depth(*v, depth + 1, memo)?;
                        memo.path.remove(&h);
                        WireValue::Cell {
                            id,
                            inner: Box::new(inner),
                        }
                    }
                }
                // A cursor crosses by value as a DEEP COPY (like `List`): wire each snapshot item and
                // carry `pos`. It is plain data (a `Vec` + index), so — unlike a generator — it is
                // genuinely sendable, and `from_wire` rebuilds an independent cursor on the other side.
                // This matches the serial-VM parity oracle, whose `deep_clone` already deep-copies a cursor across
                // the airlock; gating it here (the old behavior) diverged VM from interp. Recursing
                // through items means a cursor over a non-sendable element faults recoverably, like a
                // `list` of that element would.
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
        self.from_wire_memo(w, &mut rebuild)
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
    fn from_wire_memo(
        &mut self,
        w: WireValue,
        rebuild: &mut super::fxhash::FxHashMap<u32, GcRef>,
    ) -> Value {
        match w {
            // Re-create on the DESTINATION heap: `make_int` re-inlines or re-boxes (wide) and
            // `box_float` re-boxes — identically on both engines, so the airlock round-trip is
            // representation-stable across serial/M:N.
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
            WireValue::Native { name, func } => {
                Value::obj(self.heap.alloc(Obj::Native { name, func }))
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
                let cloned: Vec<Value> = items
                    .into_iter()
                    .map(|x| self.from_wire_memo(x, rebuild))
                    .collect();
                *self.heap.get_mut(h) = Obj::List(cloned);
                Value::obj(h)
            }
            WireValue::Tuple { id, items } => {
                let h = self.heap.alloc(Obj::Tuple(Vec::new()));
                rebuild.insert(id, h);
                let cloned: Vec<Value> = items
                    .into_iter()
                    .map(|x| self.from_wire_memo(x, rebuild))
                    .collect();
                *self.heap.get_mut(h) = Obj::Tuple(cloned);
                Value::obj(h)
            }
            WireValue::Iter { id, items, pos } => {
                let h = self.heap.alloc(Obj::Iter {
                    items: Vec::new(),
                    pos,
                });
                rebuild.insert(id, h);
                let cloned: Vec<Value> = items
                    .into_iter()
                    .map(|x| self.from_wire_memo(x, rebuild))
                    .collect();
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
                let cloned: Vec<Value> = fields
                    .into_iter()
                    .map(|(_, val)| self.from_wire_memo(val, rebuild))
                    .collect();
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
                let cloned: Vec<Value> = payload
                    .into_iter()
                    .map(|x| self.from_wire_memo(x, rebuild))
                    .collect();
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
            // A back-reference closes a serialized value cycle: resolve it to the placeholder registered
            // under this `id` (always present — `to_wire` assigns the id and inserts it into the
            // serialize memo BEFORE emitting any back-edge to it, and `from_wire_memo` registers the
            // placeholder BEFORE recursing children, so the target is alloc'd by the time we get here).
            WireValue::Backref(id) => Value::obj(
                *rebuild
                    .get(&id)
                    .expect("a wire Backref always targets an already-reconstructed node id"),
            ),
            // Rebuild a FRESH, independent `Obj::Cell` on this side (deep copy, never a shared box) —
            // the receiving task owns its own cell (design §4 F1). TIE THE KNOT: alloc a placeholder
            // `Cell(Nil)` and register its `id` BEFORE recursing `inner`, so a nested `Backref(id)`
            // (the self-cell a recursive local `fn` closes) resolves to this exact handle; then patch
            // the placeholder with the reconstructed inner. `Heap::alloc` never collects, so no GC runs
            // between the placeholder and the patch.
            WireValue::Cell { id, inner } => {
                let h = self.heap.alloc(Obj::Cell(Value::nil()));
                rebuild.insert(id, h);
                let inner = self.from_wire_memo(*inner, rebuild);
                *self.heap.get_mut(h) = Obj::Cell(inner);
                Value::obj(h)
            }
            // B3.6: rebuild a submitted closure by value over the worker's reconstructed home module
            // (the `proto` is shared via `Arc<Program>`; captures reconstruct bottom-up into this heap).
            // `worker_home` resolves the home index against this VM's `module_objs` (the rebuilt graph
            // in a pool worker, or the live graph in a cooperative same-heap drain). TIE THE KNOT like
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
                let cap: Vec<Value> = captured
                    .into_iter()
                    .map(|(_k, w)| self.from_wire_memo(w, rebuild))
                    .collect();
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
                        let args: Vec<Value> = wargs
                            .into_iter()
                            .map(|w| self.from_wire_memo(w, rebuild))
                            .collect();
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
                        let stack: Vec<Value> = stack
                            .into_iter()
                            .map(|w| self.from_wire_memo(w, rebuild))
                            .collect();
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
        // real runaway trips on whichever heap runs it. It does NOT make the trip point identical for a
        // *near-boundary* concurrent test — the cooperative engine shares one heap (parent-baseline + Σ
        // tasks) while each M:N worker's heap is measured alone, so a task allocating just under the cap
        // (plus a parent baseline) trips on serial but not M:N. That per-heap divergence is inherent and
        // documented (`docs/future.md §3b`); a cross-engine aggregate would need non-deterministic global
        // RSS. `0` when the cap is off, so the common path is untouched.
        worker.set_max_heap(self.heap.mem_cap());
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
        worker.parallel = self.parallel;
        // B3.3-threads: thread the parent's host state (process args + env) through so a
        // `--parallel` task reading `std.os.args` / an env var sees the same values instead of inert
        // defaults (the B3.2 silent-divergence owe). `args` is read-only (deep-cloned). `env` is
        // SHARED (its `Arc::clone` hands over the same `Mutex`-guarded map, not a copy) so a task's
        // `std.os.setenv` is visible to the parent + siblings — process-global env, matching the
        // serial engine (one Vm, one map) and Python/Go. `stdin` is SHARED, not copied: `Stdin`'s clone
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
        self.prepare_worker(task, None)?.run()
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
    ) -> Result<ReadyWorker, RuntimeError> {
        // 1. Lower the task to a `Send` description in THIS (parent) heap (read-only serialize),
        //    rejecting any value that can't cross a heap boundary as-is.
        let lowered = self.lower_task(task)?;
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
        let (call, span) = worker.rebuild_ready(lowered);
        Ok(ReadyWorker { worker, call, span })
    }

    /// PHASE 1 of task preparation — lower a [`PendingCall`] to a heap-independent [`Lowered`] against
    /// the CURRENT (parent/shell) heap: home indices resolve against `self.module_objs`, and every
    /// crossing value goes through `to_wire`/`ensure_crossable` (rejecting a non-isolable callee). Split
    /// out of [`Vm::prepare_worker`] so the serial engine can reuse the exact M:N lowering
    /// ([`Vm::prepare_serial_child`]) — a behavior-preserving extraction.
    pub(super) fn lower_task(&mut self, task: PendingCall) -> Result<Lowered, RuntimeError> {
        let lowered = match task {
            PendingCall::Call { callee, args, span } => {
                let wargs = self.wire_args(args, span)?;
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
                                let w = self.to_wire_at(v, span)?;
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
                let wrecv = self.to_wire_at(recv, span)?;
                self.ensure_crossable(&wrecv, span)?;
                let wargs = self.wire_args(args, span)?;
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
    /// Split out of [`Vm::prepare_worker`] so both the M:N worker (into its own heap) and the serial
    /// child (into the shared heap, under a per-child module view — [`Vm::prepare_serial_child`]) reuse
    /// the identical reconstruction. Infallible (all crossing checks happened in [`Vm::lower_task`]).
    pub(super) fn rebuild_ready(&mut self, lowered: Lowered) -> (ReadyCall, Span) {
        match lowered {
            Lowered::Closure {
                proto,
                captured,
                args,
                home,
                span,
            } => {
                let home = self.worker_home(home);
                // Lever #3: rebuild positionally (slot order), discarding the carried names.
                let cap: Vec<Value> = captured
                    .into_iter()
                    .map(|(_k, w)| self.from_wire(w))
                    .collect();
                let callee = Value::obj(self.heap.alloc(Obj::Closure {
                    proto,
                    captured: cap,
                    home,
                }));
                let args = args.into_iter().map(|w| self.from_wire(w)).collect();
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
                let args = args.into_iter().map(|w| self.from_wire(w)).collect();
                (ReadyCall::Invoke { callee, args }, span)
            }
            Lowered::Builtin { name, args, span } => {
                let callee = Value::obj(self.heap.alloc(Obj::Builtin(name)));
                let args = args.into_iter().map(|w| self.from_wire(w)).collect();
                (ReadyCall::Invoke { callee, args }, span)
            }
            Lowered::Method {
                recv,
                name,
                args,
                span,
            } => {
                let recv = self.from_wire(recv);
                let args = args.into_iter().map(|w| self.from_wire(w)).collect();
                (ReadyCall::Method { recv, name, args }, span)
            }
        }
    }

    /// Task 1 — the SERIAL analogue of [`Vm::prepare_worker`]: deep-copy this module graph into a fresh
    /// per-child `module_objs` view **in the SHARED heap** (not a separate worker heap), so a cooperative
    /// child mutates its OWN copy of every module global — making `serial == M:N` by construction. Reuses
    /// the exact `to_snap` lowering (so `Shared`/`Atomic`/`Channel`/`Cffi` still cross by shared `Arc`,
    /// NOT by deep copy — the escape hatch) and the eager `fault_module` replay.
    ///
    /// Returns the re-homed [`PendingCall`] (callee/receiver/args now point at the child's module copy)
    /// plus the child's `module_objs` + `module_faulted`, which the caller stores in the child's
    /// [`FiberCtx`] so they swap in per-fiber via [`Vm::swap_ctx`].
    ///
    /// GC-safe: no dispatch safepoint runs between install and restore (allocation never collects
    /// inline — only [`Vm::run_until`] does), so the shell's real modules parked in `saved_objs` cannot
    /// be swept during the window. The child modules are reachable via `self.module_objs` while being
    /// built; once installed in a `FiberCtx`, `root_ctx` roots them.
    pub(super) fn prepare_serial_child(
        &mut self,
        task: PendingCall,
        snap: Arc<ModuleSnapshot>,
    ) -> Result<(PendingCall, Vec<GcRef>, Vec<bool>), RuntimeError> {
        // Phase 1: lower against the SHELL's live module_objs (home indices resolve to the parent).
        let lowered = self.lower_task(task)?;
        // Phase 2: install a fresh child module view into the SHARED heap and eager-fault every global,
        // materializing the deep copy. The child's `FiberCtx` carries NO snapshot (W6-2: `module_snapshot`
        // swaps per fiber, and a cooperative child's is `None`) — else `ensure_module_faulted` would try
        // to lazy-fault the shell's REAL modules. Eager fault + `None` sidesteps all lazy machinery; the
        // take/restore below is what keeps THIS (parent) VM's own snapshot out of the child's window.
        let saved_objs = std::mem::take(&mut self.module_objs);
        let saved_faulted = std::mem::take(&mut self.module_faulted);
        let saved_snap = self.module_snapshot.take();
        // W6-2 — the cache describes the view, so it rides the same take/restore: the child's view must
        // not leave its snapshot cached on the parent (and vice versa).
        let saved_memo = self.snapshot_memo.take();
        self.install_snapshot(snap);
        for i in 0..self.module_objs.len() {
            self.fault_module(i);
        }
        self.module_snapshot = None;
        // Phase 3: rebuild callee/receiver + args — home indices now resolve to the CHILD copy.
        let (call, span) = self.rebuild_ready(lowered);
        // Capture the child view, restore the shell's real modules/snapshot.
        let child_objs = std::mem::replace(&mut self.module_objs, saved_objs);
        let child_faulted = std::mem::replace(&mut self.module_faulted, saved_faulted);
        self.module_snapshot = saved_snap;
        self.snapshot_memo = saved_memo;
        let pending = match call {
            ReadyCall::Invoke { callee, args } => PendingCall::Call { callee, args, span },
            ReadyCall::Method { recv, name, args } => PendingCall::Method {
                recv,
                name,
                args,
                span,
            },
        };
        Ok((pending, child_objs, child_faulted))
    }

    /// Task 1 (Executor path) — run `body` with a fresh per-task child `module_objs` view installed in
    /// the SHARED heap (a deep copy of every module global via `snap`), then restore the shell's real
    /// modules on every path. The SERIAL cooperative analogue of the M:N `prepare_worker_from_wire` →
    /// own-heap snapshot: an `Executor` task drained inline on the cooperative engine mutates its OWN
    /// module-global copy — so `serial == M:N` for a module global mutated from a submitted closure,
    /// exactly like the nursery path ([`Vm::prepare_serial_child`]). Both the `from_wire` rebuild of the
    /// task closure AND its `invoke_value` must run inside `body` so its `home` resolves to — and its
    /// mutations land on — the child copy. Reuses `to_snap`/`install_snapshot`, so `Shared`/`Atomic`/
    /// `Channel`/`Cffi` module globals still cross by shared `Arc` (the escape hatch), never deep-copied.
    ///
    /// GC-safe: `self.module_objs` (swapped to the child view for the duration) is a GC root, so the
    /// child copy survives an alloc-triggered collection during the task's `invoke_value`; the shell's
    /// real modules parked in `saved_objs` are only swept if unrooted, but they are restored before any
    /// safepoint runs on the shell again.
    pub(super) fn with_serial_child_modules<R>(
        &mut self,
        snap: Arc<ModuleSnapshot>,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        // Install a fresh child module view into the SHARED heap and eager-fault every global (the same
        // dance as `prepare_serial_child`). The task body runs on THIS VM (no fiber swap), so clear
        // `module_snapshot` after eager-faulting — else `ensure_module_faulted` would try to lazy-fault the
        // shell's REAL modules once we restore them below.
        let saved_objs = std::mem::take(&mut self.module_objs);
        // GC PIN (Task 1 fix): this window can run with EMPTY frames (the serial `Executor` exit-drain,
        // after `run()` pops the top-level frame), so the shell's real modules — now only in `saved_objs`
        // — are rooted by nothing. `install_snapshot`/`fault_module`/`body` all allocate on the SHARED
        // heap and can trip a safepoint GC; without this pin those modules get swept and the restore
        // below reinstalls dangling `GcRef`s (use-after-free). `collect()` scans `pinned_module_roots`.
        // NOT panic-safe: a raw Rust unwind through `body` skips the `truncate`/restore below, leaving
        // `pinned_module_roots` + `module_objs` corrupt. Safe today — `guarded` re-raises panics and the
        // serial VM is discarded on unwind (no catch-and-reuse); recoverable faults return via `Err`,
        // which restores cleanly. Revisit if serial-VM panic recovery is ever added.
        // Append (not assign) so a NESTED serial drain — an Executor job that drains another Executor —
        // keeps every outer level's shell modules pinned too; truncate back to the base on exit.
        let pin_base = self.pinned_module_roots.len();
        self.pinned_module_roots.extend_from_slice(&saved_objs);
        let saved_faulted = std::mem::take(&mut self.module_faulted);
        let saved_snap = self.module_snapshot.take();
        // W6-2 — the snapshot cache describes the view, so it rides the same take/restore dance.
        let saved_memo = self.snapshot_memo.take();
        self.install_snapshot(snap);
        for i in 0..self.module_objs.len() {
            self.fault_module(i);
        }
        self.module_snapshot = None;
        let r = body(self);
        // Restore the shell's real modules/snapshot (the child copy is dropped — GC reclaims it).
        self.pinned_module_roots.truncate(pin_base);
        self.module_objs = saved_objs;
        self.module_faulted = saved_faulted;
        self.module_snapshot = saved_snap;
        self.snapshot_memo = saved_memo;
        r
    }

    /// B3.6 — the `Executor`-drain analogue of [`prepare_worker`]: build a worker, install the shared
    /// read-only [`ModuleSnapshot`] (D1 — modules fault in lazily on first global access), and rebuild
    /// a submitted closure (a [`WireValue::Closure`] drained from the executor queue) into that heap as
    /// a zero-arg call. The submitted closure already crossed `to_wire`/`ensure_crossable` at `submit`,
    /// but `ensure_snapshot` can fault if a module global is a frame-holding generator — so this
    /// forwards that snapshot fault (re-stamped with `span`) rather than panicking. `--parallel` only.
    ///
    /// W6-2 — an `Executor` has no nursery, so there is no pin: the snapshot is taken at the DRAIN, which
    /// is the instant the job actually runs. Both engines drain at the same program point (an explicit
    /// `shutdown` / `drain_live_executors` at clean exit), so the instant is parity-identical.
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

    /// B3.6 — drain a shut `Executor`'s pending tasks onto the bounded pool under `--parallel`. Each
    /// queued closure becomes a [`ReadyWorker`] sharing a fresh per-drain cancel flag (first fault
    /// aborts siblings, matching the cooperative inline `r?`); **no** deadlock watch (decision D — an
    /// `Executor`-spanning deadlock hangs, as documented). Output is flushed in submission (queue) order
    /// by [`run_workers_on_pool`] (decision F).
    pub(super) fn drain_executor_on_pool(
        &mut self,
        tasks: Vec<WireValue>,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if tasks.is_empty() {
            return Ok(());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let mut ready = Vec::with_capacity(tasks.len());
        for t in tasks {
            let mut rw = self.prepare_worker_from_wire(t, span)?;
            rw.worker.cancel = Some(Arc::clone(&cancel));
            ready.push(rw);
        }
        self.run_workers_on_pool(ready)
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
    /// argument that can't cross a heap boundary as-is (see [`Vm::ensure_crossable`]).
    pub(super) fn wire_args(
        &self,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Vec<WireValue>, RuntimeError> {
        args.into_iter()
            .map(|a| {
                // `to_wire_at` re-stamps a generator's placeholder span with this call site's `span`.
                let w = self.to_wire_at(a, span)?;
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
    /// that a re-snapshot would read EMPTY slots and recreate W6-2 one level down. Free on the top-level
    /// / cooperative engine (nothing is lazy there — `module_snapshot` is `None`).
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
        let snap = Arc::new(
            self.snapshot_modules()
                .map_err(|e| self.err(e.message, span))?,
        );
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
    pub(super) fn snapshot_modules(&self) -> Result<ModuleSnapshot, RuntimeError> {
        let mut modules = Vec::with_capacity(self.module_objs.len());
        // W6-2 — computed inside the walk that already visits every global (no extra traversal).
        let mut reusable = true;
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
            for (k, v) in globals {
                reusable &= self.slot_snapshot_reusable(v);
                snapped.push((k, self.to_snap(v)?));
            }
            modules.push(ModuleSnap {
                name,
                globals: snapped,
            });
        }
        Ok(ModuleSnapshot { modules, reusable })
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
    pub(super) fn to_snap(&self, v: Value) -> Result<SnapValue, RuntimeError> {
        self.to_snap_depth(v, 0)
    }

    /// Depth-counted worker behind [`Vm::to_snap`] — the serial engine never snapshots, so this is the
    /// M:N module-global crossing path. Shares [`Vm::to_wire_depth`]'s cyclic-data guard: the same
    /// `MAX_STRUCTURAL_DEPTH` bound turns a cyclic module global into a recoverable `RuntimeError`
    /// (re-stamped with the real nursery span by `ensure_snapshot`) rather than a host `SIGABRT`. The
    /// fast path threads `depth` into `to_wire_depth` and every slow arm recurses at `depth + 1`, so
    /// the shared budget keeps `to_snap` and `to_wire` in lockstep.
    fn to_snap_depth(&self, v: Value, depth: usize) -> Result<SnapValue, RuntimeError> {
        if depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.depth_exceeded_err(Span { line: 0, col: 0 }));
        }
        let h = match v.as_obj() {
            Some(h) => h,
            // A scalar (inline Int/Bool/Nil) or a boxed float (Float tag) is always sendable — never
            // `Obj::Generator`, so this `.expect` is unreachable for a generator.
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
        // memory-safe AND `serial == M:N` by construction (each task already gets its own frozen
        // per-task module-global snapshot — F1). A non-sendable parked slot / reference cycle makes
        // `to_wire` Err → we fall to the slow arm, which re-raises that real reject.
        if let Ok(w) = self.to_wire_depth(v, depth, &mut WireMemo::default())
            && !w.has_handle()
        {
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
                // `Module`/`Native`/`Cffi` handle (else `to_wire` succeeds with no handle and it rides
                // the SnapValue::Wire fast path with the Backref cycle encoding). In that residual-handle
                // case the recursion below walks the self-cell to the shared `MAX_STRUCTURAL_DEPTH` cap
                // and rejects cleanly (bounded, host-stack-safe) — identity preservation is a wire-only
                // concern; the SnapValue slow arm carries no Backref encoding.
                let names = &self.program.protos[proto].capture_names;
                let mut snapped = Vec::with_capacity(captured.len());
                for (i, cv) in captured.iter().enumerate() {
                    snapped.push((names.get(i).cloned().unwrap_or_default(), self.to_snap_depth(*cv, depth + 1)?));
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
                        globals.push((k, self.to_snap_depth(mv, depth + 1)?));
                    }
                    SnapValue::ModuleInline { name, globals }
                }
            },
            Obj::Native { name, func } => SnapValue::Native { name, func },
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
                    out.push(self.to_snap_depth(*x, depth + 1)?);
                }
                SnapValue::List(out)
            }
            Obj::Tuple(items) => {
                let mut out = Vec::with_capacity(items.len());
                for x in &items {
                    out.push(self.to_snap_depth(*x, depth + 1)?);
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
                    out.push((k, self.to_snap_depth(*fv, depth + 1)?));
                }
                SnapValue::Struct { name, fields: out }
            }
            Obj::Enum { variant_id, payload } => {
                let mut out = Vec::with_capacity(payload.len());
                for x in &payload {
                    out.push(self.to_snap_depth(*x, depth + 1)?);
                }
                // M19 lever #2 — carry the dense `variant_id` directly on the cold snap path (mirrors
                // `to_wire`); replay reuses it as-is against the shared program.
                SnapValue::Enum { variant_id, payload: out }
            }
            Obj::NewType { type_key, inner } => SnapValue::NewType {
                type_key,
                inner: Box::new(self.to_snap_depth(inner, depth + 1)?),
            },
            Obj::Map(m) => {
                let mut out = Vec::with_capacity(m.entries.len());
                for (hash, k, val) in &m.entries {
                    out.push((
                        *hash,
                        self.to_snap_depth(*k, depth + 1)?,
                        self.to_snap_depth(*val, depth + 1)?,
                    ));
                }
                SnapValue::Map(out)
            }
            Obj::Set(s) => {
                let mut out = Vec::with_capacity(s.entries.len());
                for (hash, e) in &s.entries {
                    out.push((*hash, self.to_snap_depth(*e, depth + 1)?));
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
            //     `Nil` is not iterable) — "fault only when reached", `serial == M:N` by construction
            //     (both engines snapshot from the same memoized frozen copy).
            Obj::Generator(_) => match self.to_wire_depth(v, depth, &mut WireMemo::default()) {
                Ok(w) if !w.has_handle() => SnapValue::Wire(w),
                _ => SnapValue::Wire(WireValue::Nil),
            },
            // A `Cell` embedding a handle snaps like a 1-field box (its inner recursively snapped) —
            // replayed as a FRESH independent cell (design §4 F1). A pure-data cell took the `to_wire`
            // fast path above (`WireValue::Cell`).
            Obj::Cell(v) => SnapValue::Cell(Box::new(self.to_snap_depth(v, depth + 1)?)),
            // A cursor snapshots like a `List`: its items (recursively snapped) + `pos`. Only a
            // handle-bearing cursor reaches here; a pure-data cursor took the `to_wire` fast path.
            Obj::Iter { items, pos } => {
                let mut out = Vec::with_capacity(items.len());
                for x in &items {
                    out.push(self.to_snap_depth(*x, depth + 1)?);
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
        for (name, sv) in &snap.modules[idx].globals {
            let val = self.replay_snap(sv);
            self.module_define(module, name, val);
        }
        self.snapshot_memo = memo;
    }

    /// D1 — if this is a worker VM (a snapshot is installed), ensure the module that owns `home` has
    /// been faulted in before its globals are read. No-op on the top-level / cooperative VM (no
    /// snapshot — `module_objs` are the real, already-populated modules), so those engines are
    /// untouched. Called at every module-global read site (`GetGlobal`, the `GetCaptured` home
    /// fallback, module member access, and a `module.fn(...)` call).
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
    pub(super) fn replay_snap(&mut self, snap: &SnapValue) -> Value {
        match snap {
            SnapValue::Wire(w) => self.from_wire(w.clone()),
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
                    .map(|(_k, cv)| self.replay_snap(cv))
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
                    let val = self.replay_snap(gv);
                    self.module_define(wm, k, val);
                }
                Value::obj(wm)
            }
            SnapValue::Native { name, func } => Value::obj(self.heap.alloc(Obj::Native {
                name: name.clone(),
                func: *func,
            })),
            // Re-alloc a fresh `Obj::Builtin` from the carried name (pure code, no state to share).
            SnapValue::Builtin(name) => Value::obj(self.heap.alloc(Obj::Builtin(name.clone()))),
            // Re-alloc from the SAME shared `Arc<Cffi>` — no re-dlopen (shared address space).
            SnapValue::Cffi(c) => Value::obj(self.heap.alloc(Obj::Cffi(Arc::clone(c)))),
            SnapValue::List(xs) => {
                let v = xs.iter().map(|x| self.replay_snap(x)).collect();
                Value::obj(self.heap.alloc(Obj::List(v)))
            }
            SnapValue::Iter { items, pos } => {
                let v = items.iter().map(|x| self.replay_snap(x)).collect();
                Value::obj(self.heap.alloc(Obj::Iter {
                    items: v,
                    pos: *pos,
                }))
            }
            SnapValue::Tuple(xs) => {
                let v = xs.iter().map(|x| self.replay_snap(x)).collect();
                Value::obj(self.heap.alloc(Obj::Tuple(v)))
            }
            SnapValue::Struct { name, fields } => {
                // Positional layout: the snap fields are in declaration order (to_snap emits them
                // so), so rebuild positionally — the carried names are discarded.
                let f: Vec<Value> = fields.iter().map(|(_, fv)| self.replay_snap(fv)).collect();
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
                let p = payload.iter().map(|x| self.replay_snap(x)).collect();
                // M19 lever #2 — the dense `variant_id` was carried directly (mirrors `from_wire`);
                // replay it as-is against the shared program — no lossy name re-resolution.
                Value::obj(self.heap.alloc(Obj::Enum {
                    variant_id: *variant_id,
                    payload: p,
                }))
            }
            SnapValue::NewType { type_key, inner } => {
                let inner = self.replay_snap(inner);
                Value::obj(self.heap.alloc(Obj::NewType {
                    type_key: type_key.clone(),
                    inner,
                }))
            }
            // Rebuild a FRESH independent cell on the worker (deep copy, never shared) — design §4 F1.
            SnapValue::Cell(inner) => {
                let inner = self.replay_snap(inner);
                Value::obj(self.heap.alloc(Obj::Cell(inner)))
            }
            SnapValue::Map(entries) => {
                let mut out = MapData::default();
                for (hash, k, val) in entries {
                    let (ck, cv) = (self.replay_snap(k), self.replay_snap(val));
                    out.push(*hash, ck, cv);
                }
                Value::obj(self.heap.alloc(Obj::Map(out)))
            }
            SnapValue::Set(entries) => {
                let mut out = SetData::default();
                for (hash, e) in entries {
                    let ce = self.replay_snap(e);
                    out.push(*hash, ce);
                }
                Value::obj(self.heap.alloc(Obj::Set(out)))
            }
        }
    }
}
