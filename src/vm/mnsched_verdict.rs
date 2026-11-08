//! VM: finishing, cancel and drain, scope slots and faults, waits, deadlock and quiescence
//! verdicts, offload.
//! Split out of `src/vm/mnsched.rs`. `super::*` is the parent module.

use super::*;

impl MnSched {
    /// [`MnSched::finish_with`] for a task that never ran (`spawn_into`'s no-runner path, the unit
    /// tests): there is no flag it ran under, so nothing trips.
    pub(super) fn finish(
        &self,
        task_index: usize,
        scope_id: usize,
        outcome: TaskOutcome,
    ) -> (bool, bool) {
        self.finish_with(task_index, scope_id, outcome, None)
    }

    /// Record a finished fiber's outcome in its FLAT slot, bump its SCOPE's done, drop it from
    /// `running`. Sets GLOBAL `terminate` only when EVERY scope is done (and no eager body is open) —
    /// because farmed helpers (sentinel scope_id) drain until global terminate, and the scope-scoped
    /// owner stop returns each owner the instant its OWN scope completes. The per-scope `done` drives
    /// that owner stop; the global all-done drives helper/sentinel termination.
    /// Returns whether the stored outcome ABORTS its scope (a `Fault`/`Exit`), so the caller runs the
    /// `cancel_drain` its parked siblings need. It is computed here, not by the caller, because the
    /// W7-26r backlog verdict below can turn a `Done` into a hard-halt `Fault` — a caller-side
    /// `matches!` on the outcome it handed in would miss that and leave the scope's parked fibers
    /// with nobody to wake them. Also returns whether it published a job fault
    /// ([`MnSched::publish_job_fault`]), so the caller runs [`Vm::halt_run`].
    /// TICKET-297 — `trip` is the cancel flag the fiber ran under (`Vm::cancel`: its scope's flag,
    /// or an Executor job's own). An aborting outcome ([`TaskOutcome::aborts`]) trips it HERE, in
    /// the core-lock hold that stores the slot: THE one trip of a task's aborting end. A party that
    /// reads the tripped flag and then asks [`MnSched::scope_fault`] (`block::halt_of`) waits for
    /// this lock, so it finds the fault. A trip before the lock let an owner parked at an inner join
    /// read the flag with an empty slot, take no halt, and run the line after its join. Under the
    /// lock the trip also reaches the deadlock verdict (`trip_scope_cancel`).
    pub(super) fn finish_with(
        &self,
        task_index: usize,
        scope_id: usize,
        outcome: TaskOutcome,
        trip: Option<&AtomicBool>,
    ) -> (bool, bool) {
        let mut c = self.lock();
        debug_assert!(Arc::ptr_eq(&c.left, &self.quiesce.epoch()));
        let job_fault = self.publish_job_fault(&c, scope_id, &outcome);
        c.leave_running();
        // A task that ends with a deadlock verdict's fault cancels no sibling when every other
        // party of this sched is stuck too: the sched-wide verdict cut them all at once, or (a
        // verdict on one leaf family, ended through its owner) nothing else here can move. A trip
        // turned such a sibling into a cancelled task, so the report missed it, and which task was
        // reported depended on which woke first. Each stuck sibling is then judged by the next
        // verdict, and the join lists every one in task order.
        let cut_end = matches!(&outcome, TaskOutcome::Fault { err, .. } if err.is_deadlock)
            && (c.cut_in_place() || self.is_deadlocked(&c));
        // The outcome the worker handed in, before the W7-26r conversion below: that conversion
        // trips the scope's own flag, for a backlog only.
        if let Some(flag) = trip
            && outcome.aborts()
            && !cut_end
        {
            crate::vm::trip_cancel_flag(flag);
        }
        // W7-26r — this thread is the only party that can observe the cap for a nursery whose parent
        // is blocked in the join (see `core::halt_over_backlog`). These slots live outside every
        // `Heap`, so `live_bytes` never counted them either: this is the accounting AND the
        // observation. Cheap: the summary is O(1) (buffered output capacities; a task keeps no
        // return value, W7-27) and the whole block is skipped when no cap is set.
        let outcome = if self.mem_cap != 0 {
            let s = &mut c.scopes[scope_id];
            s.bytes += super::core::outcome_summary(&outcome);
            let (outcome, over) = super::core::halt_over_backlog(outcome, s.bytes, self.mem_cap);
            if over {
                // Under the core lock, exactly as `trip_scope_cancel` requires — the release
                // publishes the flag to every worker that later evaluates `is_deadlocked`. Only THIS
                // scope: an inner nursery's backlog must not cancel an outer sibling (structured
                // concurrency); the fault propagates outward through the join instead.
                crate::vm::trip_cancel_flag(&s.cancel);
            }
            outcome
        } else {
            outcome
        };
        let aborts = outcome.aborts() && !cut_end;
        // A job the deadlock verdict ended: the verdict cut the sched (it latched `terminate`, or a
        // demoted fiber is left to settle), so a released held job would never run and its `None`
        // slot would hang the join. It reads as a latched verdict.
        let verdict_end = matches!(&outcome, TaskOutcome::Fault { err, .. } if err.is_deadlock);
        let seal = match &outcome {
            // The body returned: `_run_outcome` sealed its value, and the first writer wins.
            TaskOutcome::Done(_) => JobSeal::Nothing,
            // A hard abort is never a Task outcome (TICKET-289): the run halts instead.
            TaskOutcome::Fault { err, .. } | TaskOutcome::CancelledFault { err, .. }
                if executor_hard_halt(err) =>
            {
                JobSeal::Nothing
            }
            // A deadlock-verdict `Fault` also lands here, but `halt` below is `Deadlock` for it,
            // so `Ended` seals nothing: the deadlock ends the run and is never a Task outcome.
            TaskOutcome::Fault { err, .. } | TaskOutcome::CancelledFault { err, .. } => {
                JobSeal::Fault(err.message.as_str().into())
            }
            TaskOutcome::Cancelled { .. }
            | TaskOutcome::Exit { .. }
            | TaskOutcome::Deadlocked { .. } => JobSeal::Cancel,
        };
        c.slots[task_index] = Some(outcome);
        c.task_done(scope_id);
        // TICKET-219 — only an Executor's sched (`leaf_site`) holds jobs, so a nursery's finish
        // reads no halt cell.
        let halt = if verdict_end && c.is_job_scope(scope_id) {
            RunHalt::Deadlock
        } else if c.leaf_site {
            self.quiesce.run_halt()
        } else {
            RunHalt::Running
        };
        let step = c.job_event(
            JobEvent::Ended {
                task_index,
                scope_id,
                seal: if halt.settles() {
                    seal
                } else {
                    JobSeal::Nothing
                },
            },
            halt,
        );
        for f in step.start {
            self.enqueue_locked(&mut c, f);
        }
        // TICKET-103 — a join-parked owner of the family this task completed resumes. Its own slot
        // is still `None`, so the `terminate` latch below cannot fire while one is requeued.
        self.wake_completed_joins(&mut c);
        // Per-connection spawn — do NOT latch `terminate` while ANY eager body is still injecting: a
        // transient all-done (every handler SO FAR finished) is not completion — the acceptor may
        // inject more. `close_body` at `JoinNursery` clears `body_open`, and the next run-out-of-work
        // `take_runnable` terminates. Always `false` on the lazy path → unchanged D2b behavior.
        if c.all_scopes_done() && !c.any_body_open() {
            c.terminate = true;
        }
        self.notify_waiters();
        drop(c);
        self.wake_sealed(step.sealed);
        (aborts, job_fault)
    }

    /// TICKET-270 — THE publisher of the one job end that halts the run: an `Executor` job's
    /// `--max-heap` abort (TICKET-289). A hard abort is never a Task outcome, so the run halts and
    /// the Task seals nothing ([`JobSeal::Nothing`]). Every other job end stays in its Task: an
    /// ordinary fault, a deadlock verdict and a `--timeout` cut (each party reads the deadline)
    /// publish nothing. It stores the cell under `finish`'s core-lock hold, before the slot: a
    /// joiner that reads the halt takes this lock to read finished output
    /// (`take_finished_streams`), so it finds the job's slot filled, and `job_event` reads the cell
    /// before it releases a held job (DEC-219). An `os.exit` is not published here: the host
    /// `request_exit` stores its code at the call (W7-57, DEC-213). The caller runs
    /// [`Vm::halt_run`] after `finish` returns: `halt_all_scheds` takes every core lock.
    fn publish_job_fault(&self, c: &SchedCore, scope_id: usize, outcome: &TaskOutcome) -> bool {
        match outcome {
            TaskOutcome::Fault { err, trace, .. }
                if c.is_job_scope(scope_id) && err.is_over_memory =>
            {
                self.quiesce.request_job_fault(err.clone(), trace.clone());
                true
            }
            TaskOutcome::Fault { .. }
            | TaskOutcome::Exit { .. }
            | TaskOutcome::Done(_)
            | TaskOutcome::Cancelled { .. }
            | TaskOutcome::CancelledFault { .. }
            | TaskOutcome::Deadlocked { .. } => false,
        }
    }

    /// N4 — trip scope `scope_id`'s cancel **under the core lock**. Two reasons the lock is not
    /// optional: (1) the mutex release PUBLISHES the flag to every worker that later takes the lock to
    /// evaluate `is_deadlocked` (a bare `Relaxed` store outside it has no synchronizes-with edge, so a
    /// lock-holder could legally still read `false` and reap the scope's parked fibers as `Deadlocked`);
    /// (2) it lets the abort seams arm this veto BEFORE they clear their own (`awaiting_builder` /
    /// `body_open`) — a gapless veto handoff. Idempotent (the fault path trips its flag in
    /// [`MnSched::finish_with`], under `finish`'s own lock hold).
    pub(super) fn trip_scope_cancel(&self, scope_id: usize) {
        let c = self.lock();
        crate::vm::trip_cancel_flag(&c.scopes[scope_id].cancel);
    }

    /// gaps.md W7-57 — the run-wide `os.exit` analogue of the intra-nursery abort teardown
    /// (`mn_worker_loop`'s `if aborts { drain_family }`): trip **every** scope's cancel and
    /// drain every parked fiber, so a nursery that is not the exiting party's own still dies now instead
    /// of at its natural end. Called for each live sched by [`Vm::halt_all_scheds`].
    ///
    /// **The cancel stores go under the core lock** — the release edge [`trip_scope_cancel`] documents.
    /// A bare `Relaxed` store outside it has no synchronizes-with edge, so a worker already holding the
    /// lock to evaluate `is_deadlocked` could legally still read `false` and reap the scope's parked
    /// fibers as `Deadlocked`, dropping their `defer`s.
    ///
    /// The lock is DROPPED before `drain_family`, which takes it itself (its poller half keeps the
    /// registry leaf-level and calls `complete_offload`, which locks this core).
    ///
    /// The live scope ids are snapshotted: a scope registered after this instant is a nursery created
    /// after the exit was published, whose fibers are covered by `jump_checked`'s back-edge exit rung
    /// and by every blocking wait's `run_exit_err` — this drain is a promptness lever, not the only one.
    /// A snapshotted id retired before its drain reads as absent (TICKET-199), so its drain is a no-op.
    pub(super) fn cancel_all(self: &Arc<Self>) {
        let ids = {
            let c = self.lock();
            for s in c.scopes.values() {
                crate::vm::trip_cancel_flag(&s.cancel);
            }
            c.scopes.ids()
        };
        for scope_id in ids {
            self.drain_family(scope_id);
        }
        self.notify_waiters();
    }

    /// TICKET-200 — THE cancel drain: after scope `scope_id`'s cancel (or an ancestor's) is tripped,
    /// requeue every fiber of its family that is parked, in both halves: `cancel_drain` (the `parked`
    /// buckets) and `poller::drain_family` (the netpoller registry). A socket-parked fiber is counted
    /// `inflight`, not `parked`, so a drain of one half strands it — the `cancel` hang of a
    /// job's nursery fiber parked in `accept`/`read`/`write`/`connect`. Every cancel site calls this;
    /// never call either half on its own. A retired id has an empty family (TICKET-199, DEC-199), so
    /// both halves are no-ops.
    pub(super) fn drain_family(&self, scope_id: usize) {
        let family = self.lock().scope_family(scope_id);
        self.cancel_drain(scope_id);
        poller::drain_family(self, &family);
    }

    /// TICKET-289 — the cancel drain of one Executor job whose flag is `flag` (`Task.cancel()`):
    /// requeue the job's own parked fiber and the fibers of every scope under it, in both halves.
    /// The scope ids are read once, under the lock: a scope registered after the trip starts with
    /// the flag already tripped and unwinds on its own.
    pub(super) fn drain_job(&self, flag: &Arc<AtomicBool>) {
        let under: Vec<usize> = {
            let c = self.lock();
            c.scopes
                .iter()
                .filter(|(_, s)| s.ancestors.iter().any(|a| Arc::ptr_eq(a, flag)))
                .map(|(i, _)| i)
                .collect()
        };
        self.drain_where(|f| {
            f.honours_cancel()
                && (f.job_cancel.as_ref().is_some_and(|j| Arc::ptr_eq(j, flag))
                    || under.contains(&f.scope_id)
                    || open_nurseries(&f.ctx.eager_scheds).any(|o| {
                        under.contains(&o.scope) || o.more.iter().any(|s| under.contains(s))
                    }))
        });
    }

    /// TICKET-200 — THE cancel drain's two halves, selecting the fibers `wakes` names:
    /// `cancel_drain_where` (the `parked` buckets) and the netpoller registry.
    fn drain_where(&self, wakes: impl Fn(&Fiber) -> bool) {
        self.cancel_drain_where(&wakes);
        poller::drain_where(self, &wakes);
    }

    /// B3.4 — after a scope's cancel is tripped, move every parked fiber **belonging to that scope**
    /// back onto the global queue so a worker resumes it and it observes the cancel flag (at the recv
    /// re-check / a dispatch back-edge) and unwinds. Cross-nursery flat scheduler: with one global
    /// parked set shared across scopes, this MUST be scope-scoped — an inner fault must drain ONLY its
    /// own scope's parked fibers, never drag an OUTER sibling out of its legitimate park (that would
    /// break structured concurrency). Parked entries whose fiber is in a different scope are kept parked
    /// (re-filed into their buckets). A `Recv` entry's scope is read by reference; a `Wait` token's is
    /// PEEKED under its fiber lock before claiming (so a non-matching wait fiber is left intact).
    ///
    /// (An `os.exit` reaches every scope by calling this in a loop — [`MnSched::cancel_all`] — rather
    /// than by relaxing the scope-scoping here, which the structured-concurrency invariant forbids.)
    ///
    /// TICKET-103 — "that scope" is its FAMILY (`SchedCore::scope_family`): a nursery's continuation
    /// scopes share its one cancel token, so a faulting child must requeue a parked sibling in a
    /// continuation too, or `any_cancelled_scope_awaiting_drain` vetoes forever. Every other scope
    /// owns a distinct Arc, so the result is unchanged for them.
    pub(super) fn cancel_drain(&self, scope_id: usize) {
        let family = self.lock().scope_family(scope_id);
        self.cancel_drain_where(&|f: &Fiber| f.woken_by(&family));
    }

    /// [`MnSched::cancel_drain`] for any matcher ([`MnSched::drain_job`]).
    fn cancel_drain_where(&self, wakes: &dyn Fn(&Fiber) -> bool) {
        let c = self.lock();
        if c.parked_n == 0 {
            return;
        }
        // A join-parked owner of this family stays filed (see `SchedCore::join_parked`): its
        // children see the tripped token, finish, and `finish` requeues it.
        self.drain_parked(c, wakes);
    }

    /// TICKET-286 — arm the one [`MnSched::deadline_drain`] job for the run deadline `dl`, once per
    /// deadline per sched. Called when a fiber parks on a channel under `--timeout`.
    pub(super) fn arm_deadline_wake(self: &Arc<Self>, dl: std::time::Instant) {
        {
            let mut c = self.lock();
            if c.deadline_armed == Some(dl) {
                return;
            }
            c.deadline_armed = Some(dl);
        }
        let sched = Arc::downgrade(self);
        timer::submit_at(
            dl,
            Box::new(move || {
                if let Some(s) = sched.upgrade() {
                    s.deadline_drain();
                }
            }),
        );
    }

    /// TICKET-286 — requeue every parked fiber whose `--timeout` deadline passed, so it re-runs its
    /// op and returns the hard halt at the op's deadline checkpoint. A stale job (a test that ended
    /// before its deadline) finds no due fiber: a later test's fibers park under a later deadline.
    pub(super) fn deadline_drain(&self) {
        let c = self.lock();
        if c.parked_n == 0 {
            return;
        }
        self.drain_parked(c, Fiber::deadline_due);
    }

    /// The one parked-set drain: requeue every parked fiber `wake` selects, in every bucket shape.
    fn drain_parked(
        &self,
        mut c: std::sync::MutexGuard<'_, SchedCore>,
        wake: impl Fn(&Fiber) -> bool,
    ) {
        let buckets: Vec<(usize, Vec<ParkedEntry>)> = c.parked.drain().collect();
        let mut drained = 0usize;
        // A fiber requeued while the run's deadlock verdict is latched (a cut owner's abort
        // cancelled its scope) is one of the verdict's blocked tasks: it is parked at a wait that
        // nothing can satisfy, so it joins the run's listing here, as the verdict found it.
        let listing = (self.quiesce.run_halt() == crate::vm::quiesce::RunHalt::Deadlock)
            .then(|| Arc::clone(&c.listing));
        let list = |f: &Fiber| {
            if let (Some(l), Some((wait, span))) = (&listing, f.park_site) {
                l.add(
                    &f.ctx.task,
                    wait,
                    span,
                    super::frames::raw_frames(&f.ctx.frames),
                );
            }
        };
        for (key, v) in buckets {
            let mut keep: Vec<ParkedEntry> = Vec::new();
            for entry in v {
                match entry {
                    ParkedEntry::Recv(mut f) => {
                        if wake(&f) {
                            list(&f);
                            drained += 1;
                            f.state = FiberState::Ready;
                            c.global.push_back(f);
                        } else {
                            keep.push(ParkedEntry::Recv(f)); // a sibling scope's park — leave it.
                        }
                    }
                    ParkedEntry::Send(mut f) => {
                        if wake(&f) {
                            list(&f);
                            drained += 1;
                            f.state = FiberState::Ready;
                            c.global.push_back(f);
                        } else {
                            keep.push(ParkedEntry::Send(f)); // a sibling scope's park — leave it.
                        }
                    }
                    ParkedEntry::Guard(mut f) => {
                        if wake(&f) {
                            list(&f);
                            drained += 1;
                            f.state = FiberState::Ready;
                            c.global.push_back(f);
                        } else {
                            keep.push(ParkedEntry::Guard(f)); // a sibling scope's park — leave it.
                        }
                    }
                    ParkedEntry::ExecJoin(mut f, s, slack) => {
                        if wake(&f) {
                            list(&f);
                            drained += 1;
                            f.state = FiberState::Ready;
                            c.global.push_back(f);
                        } else {
                            // A sibling scope's park — leave it.
                            keep.push(ParkedEntry::ExecJoin(f, s, slack));
                        }
                    }
                    ParkedEntry::Wait(wp) => {
                        // Peek the fiber's scope WITHOUT claiming (so a non-matching wait fiber is left
                        // intact in every bucket). If already claimed (a prior bucket of THIS token in
                        // this same drain, or a concurrent waker), the fiber is `None` → drop the stale
                        // token copy. Only a matching-scope, still-present fiber is claimed + requeued.
                        let in_scope = {
                            let g = wp.fiber.lock().unwrap_or_else(|e| e.into_inner());
                            g.as_ref().is_some_and(&wake)
                        };
                        if !in_scope {
                            // Either a different scope (keep parked) OR already claimed (drop). Keep the
                            // token only if the fiber is still present (i.e. a different scope), else the
                            // stale token must be dropped so it can't double-wake.
                            let still_present =
                                wp.fiber.lock().unwrap_or_else(|e| e.into_inner()).is_some();
                            if still_present {
                                keep.push(ParkedEntry::Wait(wp));
                            }
                            continue;
                        }
                        if wp
                            .claimed
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                            .is_err()
                        {
                            continue; // raced to claimed by another bucket of this token — skip.
                        }
                        let mut f = wp
                            .fiber
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .take()
                            .expect("WaitPark fiber claimed twice");
                        list(&f);
                        drained += 1;
                        f.state = FiberState::Ready;
                        c.global.push_back(f);
                    }
                }
            }
            if !keep.is_empty() {
                c.parked.insert(key, keep);
            }
        }
        c.parked_n -= drained;
        self.runnable.fetch_add(drained, Ordering::Relaxed); // parked → ready
        self.notify_waiters();
    }

    /// Drain the per-task outcome slots after the nursery terminates (joining thread, post-loop).
    #[must_use]
    pub(super) fn take_slots(&self) -> Vec<Option<TaskOutcome>> {
        let mut c = self.lock();
        let slots = std::mem::take(&mut c.slots);
        self.hold_exit_output(slots.iter().flatten());
        slots
    }

    /// TICKET-284 — the one count of exit output that leaves its slot. Every taker (`take_slots`,
    /// `take_scope_slots`, `take_finished_streams`) calls it while it holds the sched lock, so a
    /// reader that reads the count under that lock sees the output in a slot or counted, never in
    /// neither. The receiver ends the count ([`Vm::adopt_exit_output`]).
    pub(super) fn hold_exit_output<'a>(
        &self,
        taken: impl IntoIterator<Item = &'a TaskOutcome>,
    ) -> usize {
        let n = taken.into_iter().filter(|o| o.holds_exit_output()).count();
        for _ in 0..n {
            self.quiesce.begin_exit_origin();
        }
        n
    }

    /// Cross-nursery flat scheduler — drain ONE scope's contiguous slot sub-range
    /// (`base_index..base_index+total`) in task order, leaving other scopes' slots intact. The reducer
    /// flushes this scope's `Done`/`Exit` output in order (Decision F) — the slots are contiguous
    /// because `register_scope`/`inject` only ever grow the flat vec / the LAST scope. Replaces each
    /// taken slot with `None` so a re-take is a no-op (and the flat vec keeps its length, so other
    /// scopes' `base_index` stays valid).
    #[must_use]
    pub(super) fn take_scope_slots(&self, scope_id: usize) -> Vec<Option<TaskOutcome>> {
        let mut c = self.lock();
        let (base, total) = {
            let s = &c.scopes[scope_id];
            (s.base_index, s.total)
        };
        // W7-26r — the retained bytes leave with the slots, so the backlog total returns to zero with
        // them (`the Executor's sched::take_slots` does the same for the executor half). Today every caller
        // takes only after `wait_for_scope`, so no fiber of this scope can still be running; this
        // keeps the counter honest if that ever stops being true, instead of leaving a stale
        // watermark that would fault the NEXT program to reach this scope id.
        c.scopes[scope_id].bytes = 0;
        let slots: Vec<_> = (base..base + total).map(|i| c.slots[i].take()).collect();
        self.hold_exit_output(slots.iter().flatten());
        slots
    }

    /// TICKET-062 (W10-16) — peek at ONE scope's slots for the lowest-index recorded `Fault`, without
    /// taking any slot. `MnSched::finish` writes a slot and bumps `scopes[sid].done` under this SAME
    /// lock, so a fault this call sees is final: the join still reduces every slot afterward exactly
    /// as if this peek never ran.
    pub(super) fn scope_fault(&self, scope_id: usize) -> Option<(RuntimeError, Vec<TraceFrame>)> {
        let c = self.lock();
        let (base, total) = {
            let s = &c.scopes[scope_id];
            (s.base_index, s.total)
        };
        c.slots[base..base + total].iter().find_map(|s| match s {
            Some(TaskOutcome::Fault { err, trace, .. }) => Some((err.clone(), trace.clone())),
            _ => None,
        })
    }

    /// Cross-nursery flat scheduler — block until ONE scope's slots are all filled (`done == total` for
    /// that scope). The inline owner returns from `mn_worker_loop` the instant its scope completes, so
    /// this is a non-blocking re-check in the common case; it parks only if a demoted replacement is
    /// still settling this scope's last fiber. Poison-tolerant.
    pub(super) fn wait_for_scope(&self, scope_id: usize) {
        let mut c = self.lock();
        let mut idle_since = None;
        while c.scopes[scope_id].done < c.scopes[scope_id].total {
            c = self.joiner_wait(c, &mut idle_since);
        }
    }

    /// D5 owe #3 (Path C) — block until every task slot is filled (`done == total`), then return. The
    /// joining thread calls this AFTER its `mn_worker_loop` returns and BEFORE `take_slots`, because
    /// under Path C the loop can return in two ways that race slot-completion: (a) the joining thread
    /// itself demoted and early-exited (its replacement is still draining the nursery), or (b) a
    /// `terminate` (deadlock) was set before the demoted threads finished faulting in place.
    /// In the common case `mn_worker_loop` already returned because `done == total`, so this is a
    /// non-blocking re-check. Poison-tolerant (a panicked worker must not wedge the join). Liveness
    /// rests on the invariant that EVERY demote-loop exit settles its fiber (value → resume → finish;
    /// cancel/terminate/self-detected-deadlock → fault → finish; spawn-failure → fault → finish), so
    /// `done` strictly advances to `total`.
    pub(super) fn wait_for_completion(&self) {
        let mut c = self.lock();
        let mut idle_since = None;
        while c.any_scope_incomplete() {
            c = self.joiner_wait(c, &mut idle_since);
        }
    }

    /// The B3.5 deadlock predicate, now GLOBAL across every nursery scope (cross-nursery flat
    /// scheduler): SOME scope still has unfinished tasks (`any_scope_incomplete`), every not-done fiber
    /// is parked, with none running, none queued anywhere (`runnable`), and **none in flight in the
    /// blocking pool** (`inflight`) — so no `send` and no blocking-pool completion can ever arrive to
    /// wake a parked fiber, ANYWHERE. Because the park/wake set is one global set, a quiesce here means
    /// nothing can progress in ANY scope, so faulting every parked fiber is correct. Called under the
    /// core lock (the caller holds `c`); `running == 0` excludes the only out-of-lock `runnable`
    /// mutator, and `inflight` is mutated only under the core lock, so both reads are sound.
    pub(super) fn is_deadlocked(&self, c: &SchedCore) -> bool {
        self.is_deadlocked_given(c, None)
    }

    /// TICKET-232 — THE entry for this sched's own deadlock verdict. Every site that saw
    /// [`MnSched::is_deadlocked`] calls it with NO sched lock held (`parties` is taken before
    /// `SchedCore`). `licence` is the caller's own condition for cutting a victim.
    ///
    /// TICKET-236 — the order, inside ONE epoch bracket (`e0`):
    /// 1. the verdict and its licence, under the sched lock. If either fails, or a waker left
    ///    the counts since `e0`, nothing is latched. A latch never un-latches (DEC-232), so a
    ///    verdict latched ahead of a licence that then declined halted a healthy run.
    /// 2. [`quiesce::QuiesceState::decide_since`], which latches the first party's site: the verdict is
    ///    a run halt before any victim is cut (DEC-232). A flagged leaf unwinds and frees what it
    ///    holds, and a party that then takes it (a ready wait outranks a halt, DEC-194) is
    ///    stopped by the halt.
    /// 3. the verdict, the licence and the epoch again under a fresh lock; the guard is handed
    ///    back when all three still hold.
    ///
    /// `Declined` is not read: a party inside a callback is never registered (`BlockCtx::judged`),
    /// so a strict judge would hang its deadlock. `Held` returns `None`: the test pin gives the
    /// first decision to the party judge, and a cut here would free a guard before any verdict is
    /// latched (TICKET-256).
    pub(super) fn latch_own_verdict(
        &self,
        licence: impl Fn(&SchedCore) -> bool,
    ) -> Option<std::sync::MutexGuard<'_, SchedCore>> {
        let e0 = self.quiesce.left();
        let holds =
            |c: &SchedCore| self.is_deadlocked(c) && licence(c) && self.quiesce.left() == e0;
        {
            let c = self.lock();
            if !holds(&c) {
                return None;
            }
        }
        match self
            .quiesce
            .decide_since(e0, crate::vm::quiesce::Judge::Sched)
        {
            crate::vm::quiesce::Decision::Held => return None,
            crate::vm::quiesce::Decision::Stands | crate::vm::quiesce::Decision::Declined => {}
        }
        let c = self.lock();
        holds(&c).then_some(c)
    }

    /// The `runnable` count of the parked fibers a verdict's cut requeued
    /// ([`SchedCore::cut_aborting_families`]); every caller of a verdict flag calls it under the
    /// same lock hold, then notifies.
    pub(super) fn take_cut_requeued(&self, c: &mut SchedCore) {
        let n = std::mem::take(&mut c.cut_requeued);
        self.runnable.fetch_add(n, Ordering::Relaxed); // parked → ready
    }

    /// TICKET-256 — the fault this sched writes into a victim: the latched report when a verdict
    /// is latched (the first blocked party's own site), else this sched's `deadlock_err`. Takes the
    /// `deadlock` cell under the caller's `SchedCore` lock, the order `run_halt` already uses.
    pub(super) fn verdict_err(&self) -> RuntimeError {
        match self.quiesce.deadlock_report() {
            Some((msg, span)) => self.deadlock_err.at_site(&msg, span),
            None => self.deadlock_err.clone(),
        }
    }

    /// TICKET-232 — a wait that detects this sched's deadlock in place: latch the verdict
    /// ([`MnSched::latch_own_verdict`]), then fault every parked fiber. The caller holds no sched
    /// lock, and its next pass returns through the run halt or `terminate`. A new self-detecting
    /// wait calls this, never `SchedCore::flag_deadlock`.
    pub(super) fn fault_own_deadlock(&self) {
        if let Some(mut c) = self.latch_own_verdict(|_| true) {
            let err = self.verdict_err();
            c.flag_deadlock(&err);
            self.take_cut_requeued(&mut c);
            drop(c);
            self.notify_waiters();
        }
    }

    /// TICKET-118 (W13-8) — [`MnSched::is_deadlocked`], parameterised on an already-computed
    /// `cancelled_scope_awaiting_drain` read (see [`MnSched::quiesced_core_given`]). `None` reads
    /// live, matching `is_deadlocked`'s old behaviour exactly.
    pub(super) fn is_deadlocked_given(&self, c: &SchedCore, awaiting_drain: Option<bool>) -> bool {
        // A detached sched (an `Executor`'s, TICKET-208) declines: it never judges itself. Every
        // other sched answers [`MnSched::is_deadlocked_ignoring_jobs_given`]. No outstanding-job
        // veto stands here any more; a peer sched that can still move declines the verdict
        // inside that function (`any_peer_can_move`).
        if self.detached.is_some() {
            return false;
        }
        self.is_deadlocked_ignoring_jobs_given(c, awaiting_drain)
    }

    /// [`MnSched::is_deadlocked`] minus its W7-56 outstanding-job veto — "can THIS sched still move on
    /// its own?", asked without reference to any executor OR any peer sched.
    ///
    /// Three callers:
    /// * [`quiesce::PartyWait::Nursery::satisfiable`] — an owner parked in a nursery join is
    ///   satisfiable exactly when its nursery can still move. The full predicate would be useless
    ///   there: it vetoes on `outstanding > 0`, which is precisely the W7-58 shape (a stuck job).
    ///   Sound because that job is then a registered party of the process-wide verdict in its own
    ///   right — an *unregistered* job is a running one, and `parties.len() < live` already vetoes.
    ///   TICKET-099 — never sees the peer veto either: the process-wide verdict already does its own
    ///   cross-sched accounting, and a second veto underneath it double-counts (see
    ///   `any_peer_can_move`'s own doc).
    /// * [`quiesce::QuiesceState::live_eager_bodies`] — same reason: it feeds the process-wide verdict,
    ///   so it must not see the peer veto either (TICKET-099).
    /// * the W7-58 judge in [`MnSched::take_runnable`], which escalates to the process-wide verdict
    ///   when only this sched's own predicate holds — reached through [`MnSched::is_deadlocked_ignoring_jobs`],
    ///   which DOES see the peer veto, and through [`MnSched::is_deadlocked`]'s four `flag_deadlock`
    ///   callers.
    ///
    /// **Why it is not circular, stated precisely.** It reads no party state and no `outstanding` —
    /// so the process-wide verdict never appears on its own right-hand side. It is NOT lock-free
    /// though: besides `SchedCore` (held by the caller) and this sched's own atomics, its last gate
    /// peeks `ChannelCore::q` for every demoted fiber. Evaluated from `PartyWait::Nursery` that makes
    /// the chain **P → A → Q**, which is the established total order (`parties` → `SchedCore` →
    /// `ChannelCore::q`); `A → Q` is the order `send_wake` and the demoted peek already use.
    // TICKET-125 — production call sites now route through `quiesced_core` directly so they can pass
    // `only_blocked_owners`; only the unit tests below call this wrapper.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn local_quiesced(&self, c: &SchedCore) -> bool {
        self.local_quiesced_given(c, None)
    }

    /// TICKET-118 (W13-8) — [`MnSched::local_quiesced`], parameterised on an already-computed
    /// `cancelled_scope_awaiting_drain` read.
    pub(super) fn local_quiesced_given(&self, c: &SchedCore, awaiting_drain: Option<bool>) -> bool {
        self.quiesced_core_given(c, true, awaiting_drain)
    }

    /// TICKET-101 — `local_quiesced`'s body, parameterised on whether the D5 Path-C clause (below)
    /// demands a visible parked-or-demoted victim. `require_parked = true` is "may THIS sched
    /// fault?" — the fault path and the two process-wide-verdict inputs
    /// (`quiesce::PartyWait::Nursery::satisfiable`, `quiesce::QuiesceState::live_eager_bodies`) all want
    /// a victim in view before declaring deadlock, per DEC-099. `require_parked = false` is
    /// [`MnSched::peer_can_move`]'s question, "can THIS PEER still feed me?" — a peer whose only fiber
    /// is an owner blocked at a nested join has `parked_n == 0` (it counts only in `blocked_owners`), so
    /// demanding a parked fiber there vetoes forever even though that fiber can send nothing. Do not
    /// collapse the two calls: passing `true` at the peer site re-hangs the genuine nested deadlock this
    /// ticket fixes; passing `false` at either fault-path site hands the verdict a sched with no victim.
    fn quiesced_core(&self, c: &SchedCore, require_parked: bool) -> bool {
        self.quiesced_core_given(c, require_parked, None)
    }

    /// TICKET-118 (W13-8) — [`MnSched::quiesced_core`], parameterised on an already-computed
    /// `cancelled_scope_awaiting_drain` read: `None` reads live (every caller except the
    /// `MnSched::take_runnable` W13-8 drain trigger), `Some(_)` reuses that ONE read so the N4 veto
    /// below and the trigger's own drain decision can never disagree inside the same lock hold (a
    /// trip landing between two reads would otherwise let one say `false` and the other `true`).
    fn quiesced_core_given(
        &self,
        c: &SchedCore,
        require_parked: bool,
        awaiting_drain: Option<bool>,
    ) -> bool {
        // The `done < total` half is now explicit (the owner-stop replaced the preceding scalar
        // `done == total` terminate check). If EVERY scope is done there is no deadlock — `finish` will
        // have (or is about to) set global `terminate`; the owner-stop returns each owner already.
        if !c.any_scope_incomplete() {
            return false;
        }
        // TICKET-103 — a join-blocked owner whose joined family is complete resumes on its next pass,
        // and its `BlockedOwnerGuard` drop notifies `cv`. Judging in that window finds no parked leaf,
        // so `flag_deadlock_leaves` falls back to the sched-wide flag and faults the owner's live
        // siblings (W12-4 at T>=2 with a denied slot). Above the `require_parked` split (DEC-101), so
        // both callers get it; it lasts one owner pass.
        if c.any_joined_family_done() {
            return false;
        }
        // Per-connection spawn — an eager nursery whose body is still running is live work the sched
        // can't account (the acceptor runs inline and may `inject` a handler that wakes a parked
        // sibling). Never declare deadlock while ANY body can still inject; `close_body` at
        // `JoinNursery` re-enables the predicate so a genuine post-join deadlock still fires. Always
        // `false` on the lazy path — unchanged.
        //
        // §2c1 — `any_body_injecting`, not `any_body_open`: a body BLOCKED in place (or parked in a
        // NESTED nursery's join) cannot reach another `spawn`, so it is not the live feeder this veto
        // exists for. Without the distinction an eager top-level nursery — whose body spans
        // essentially the whole program — vetoes forever, and both a genuine `main`-plus-sibling
        // deadlock and a genuine nested deadlock HANG instead of faulting.
        //
        // **This is only safe because nesting shares ONE sched** (`EagerScope::scope`). With a private
        // sched per nursery the sibling that would feed the blocked body was invisible here, and this
        // exact relaxation false-faulted `examples/parallel_cross_nursery_{circular,fanout}.chz`. Now
        // that sibling is another SCOPE on this same sched, so it shows up in `running`/`runnable` and
        // vetoes on its own merits.
        // TICKET-112 — a private nested sched's open body is run by a fiber of ANOTHER sched, which
        // counts and judges that fiber itself (`MnSched::body_is_fiber`). Do not veto here on it.
        if c.any_body_injecting() && !self.body_is_fiber {
            return false;
        }
        // Cross-nursery flat scheduler — if every still-incomplete scope is an early-enlisted outer
        // nursery awaiting the inline builder's join, the builder has finished all nested service and
        // will return to the body to feed those parked siblings (`send`/`close`/`spawn`). That is a live
        // external feeder the counter-only predicate can't see, so it is NOT a deadlock. The flag clears
        // as the builder begins draining each enlisted scope, so a genuine post-body deadlock still fires
        // (and a stuck NESTED scope keeps a non-`awaiting_builder` scope incomplete → fires). (#1/#2.)
        if c.all_incomplete_awaiting_builder() {
            return false;
        }
        // `blocked_owners` — of `running`, fibers whose OS thread is blocked in a NESTED nursery join
        // on THIS sched (§ `SchedCore::blocked_owners`). Such a fiber cannot send, yield or spawn until
        // its child scope moves, so it is stuck by construction, not merely idle — `running == 0` alone
        // could never hold for a nursery nested under a spawned task at `worker_count() == 1`, where the
        // child runs as a scope on the OWNER FIBER's own thread (`op_enter_nursery`'s lazy fallback).
        if !(c.running == c.blocked_owners
            && self.runnable.load(Ordering::Relaxed) == 0
            && self.inflight.load(Ordering::Relaxed) == 0
            // D5 owe #3 (Path C) — a demoted fiber waiter (waiting in place on a channel
            // condvar) comes back only via a sibling `send`; if nothing is running/runnable/inflight,
            // no send can ever arrive, so an all-parked-or-demoted quiesce IS a deadlock. The
            // demoted thread observes the resulting `terminate` (via its bounded condvar poll) and
            // faults in place. (registering a waiter notifies `cv` so an idle puller re-evaluates this.)
            && (!require_parked
                || c.parked_n > 0
                || c.any_fiber_waiter()))
        {
            return false;
        }
        // N4 — CANCEL is a wakeup source the counters above do not model, and a cancelled scope
        // mid-teardown is not a deadlock. Two shapes, both live progress:
        //
        // * an UNDRAINED PARKED fiber of a cancelled scope (`any_cancelled_scope_awaiting_drain`): the
        //   cancel trip and its `cancel_drain` are two core-lock acquisitions apart (three seams), and
        //   an idle worker landing in that gap sees the pre-drain quiesce. `cancel_drain` is about to
        //   requeue those fibers so they unwind their `defer`s;
        // * a DEMOTED fiber whose cancel flag is tripped (`any_blocked_satisfiable`): it is
        //   a registered waiter, not `parked`, so the first scan cannot see it — but `demote_recv_block`
        //   ranks `halt_requested()` above `terminate`/self-detect, so it resumes within one
        //   `DEMOTE_POLL_BACKOFF`, unwinds and runs its `defer`s (which can `send`).
        //
        // Either way `flag_deadlock` would drop every parked fiber WITHOUT `unwind_deferred` (silently
        // skipping their `defer`s — and `reduce_task_slots` ranks Fault > Deadlocked, so the spurious
        // deadlock never even surfaced; the lost `defer` was the only symptom) and LATCH `terminate`,
        // truncating the cleanup of anything demoted inside its own `defer`. BOUNDED to real progress:
        // a cancelled scope with no undrained park whose last fiber is demoted-blocked or parked
        // forever INSIDE an uncancellable `defer` (no cancel watch — `halt_requested()` is `None`
        // while `in_defer()`) IS a genuine deadlock and is reported, not hung. Evaluated only at the
        // quiesce (after the counter gate above), so the scan is off the idle/steal hot path. A GENUINE
        // deadlock (nothing cancelled anywhere) is untouched.
        if awaiting_drain.unwrap_or_else(|| c.any_cancelled_scope_awaiting_drain(self)) {
            return false;
        }
        // TICKET-181, TICKET-236 — the ONE veto of every blocked thing. Each registered waiter (a
        // demoted recv or `wait:`, an M:N guard wait, a blocked body of this thread) and each
        // parked fiber is asked whether it could already resume: a value, a latch, a close or a
        // seal landed on its channels (DEC-176: a `wait:` settles on a close of EVERY arm), its
        // guard came free, or a cancel it would honour tripped (N4: it unwinds and runs its
        // `defer`s, which can `send`). A parked fiber stays in `parked_n` until its wake runs, and
        // that wake follows the waker's effect, so the count alone says nothing about its
        // channel. Declaring deadlock would drop every parked fiber without its `defer`s and
        // latch `terminate`. Each vetoing state resumes the asked thing (a waiter on its next
        // poll, a parked fiber on the wake that follows the effect), so the veto cannot pin a
        // hang. Lock order A → Q, A → G (a leaf), as `send_wake`.
        if c.any_blocked_satisfiable(self) {
            return false;
        }
        true
    }

    /// TICKET-099 — "can a PEER sched still move?", asked from inside a fault decision that already
    /// holds this sched's own core lock. `try_lock`s the peer's core rather than blocking: a failed
    /// `try_lock` reads as "a peer can move" (the predicate only ever DECLINES to fault), and a
    /// `try_lock` can never close a cycle against `wake_run_wide`'s registry-then-core (R then A)
    /// order, because this is core-then-core (A then A). Calls [`MnSched::local_quiesced`], never
    /// [`MnSched::is_deadlocked_ignoring_jobs`] — the recursive spelling would come back to `try_lock`
    /// a core this thread already holds if `self` ever appeared in its own peer list (it never does —
    /// `any_peer_can_move` excludes `self` by `std::ptr::eq`).
    ///
    /// A peer whose only non-parked fiber is an owner blocked at a nested join (`SchedCore::running
    /// == SchedCore::blocked_owners`, TICKET-095's clause, widened cross-sched by TICKET-099's
    /// `blocked_owner_guard`) reads as [`MnSched::quiesced_core`]`(&c, false)` and therefore does NOT
    /// veto (TICKET-101 — `local_quiesced`'s own D5 Path-C parked-victim demand read this same peer as
    /// "not quiesced" and vetoed forever, which hung the genuine nested deadlock this fn exists to let
    /// through). Dropping the cross-sched `blocked_owner_guard` bracket entirely makes every
    /// nested-nursery genuine deadlock hang instead of fault: the outer sched's join-blocked fiber would
    /// count in `running` with `blocked_owners` at zero, so it would never read quiesced, and this fn
    /// would answer "can move" forever.
    fn peer_can_move(&self) -> bool {
        let c = match self.core.try_lock() {
            Ok(c) => c,
            Err(std::sync::TryLockError::WouldBlock) => return true,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        };
        self.can_still_move(&c, false)
    }

    /// TICKET-236 — THE "undone and able to move" read of one sched, under its core lock `c`:
    /// it still holds an unfinished task and its own counts do not read as quiesced. Every
    /// caller that asks whether a sched is still a waker reads this; `require_parked` is each
    /// caller's own DEC-101 choice.
    pub(super) fn can_still_move(&self, c: &SchedCore, require_parked: bool) -> bool {
        c.any_scope_incomplete() && !self.quiesced_core(c, require_parked)
    }

    /// TICKET-099 — "can ANY OTHER live sched of this run still move?" `try_lock`s the registry too
    /// (this thread already holds its own `SchedCore` lock, so the registry acquisition must not
    /// block): contention reads as "a peer can move", same discipline as [`MnSched::peer_can_move`].
    /// An empty registry answers `false` — no peers, no veto, which is the unit fixtures' default
    /// (see `sched_registry`'s own doc).
    pub(super) fn any_peer_can_move(&self) -> bool {
        let g = match self.sched_registry.try_lock() {
            Ok(g) => g,
            Err(std::sync::TryLockError::WouldBlock) => return true,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        };
        if g.is_empty() {
            return false;
        }
        let live: Vec<_> = g.iter().filter_map(|w| w.upgrade()).collect();
        drop(g);
        live.iter()
            .filter(|s| !std::ptr::eq(Arc::as_ptr(s), self as *const MnSched))
            .any(|s| s.peer_can_move())
    }

    /// TICKET-099 — [`MnSched::local_quiesced`] plus a peer veto, applied ONLY on a FAULT decision
    /// (`is_deadlocked`'s four `flag_deadlock` callers and the W7-58 judge in `take_runnable`), NEVER
    /// on an input to the process-wide verdict (`quiesce::PartyWait::Nursery::satisfiable` and
    /// `quiesce::QuiesceState::live_eager_bodies` both call `local_quiesced` directly — see its own
    /// doc). The process-wide verdict already does its own cross-sched accounting through `parties`/
    /// `live`/`outstanding_jobs`; a second cross-sched veto underneath it would over-count `live` and
    /// hang a genuinely deadlocked run (measured on three `*_still_fault` tests — see `quiesce.rs`).
    ///
    /// A THIRD veto, `c.cross_sched_blocked_owners == 0` — this sched must not conclude ITS OWN
    /// deadlock while one of its fibers is blocked on a CHILD sched it cannot see into (see
    /// `SchedCore::cross_sched_blocked_owners`'s own doc). Without it, a nested-nursery genuine
    /// deadlock double-fires: the CHILD sched correctly detects it (its own `is_deadlocked_ignoring_jobs`
    /// is unaffected — this veto only applies to fibers blocked ON this sched, not the sched a peer's
    /// fiber is blocked into), but the PARENT sched, now reading `local_quiesced` too (the very fix that
    /// lets the child's peer veto lift), independently reaches the same false conclusion about itself and
    /// faults its OWN parked siblings directly — with `flag_deadlock` dropping their `defer`s, instead of
    /// letting the child's fault propagate up through the blocked fiber's own return and trip the
    /// parent's scope cancel the ordinary way (`MnSched::finish_with`).
    pub(super) fn is_deadlocked_ignoring_jobs(&self, c: &SchedCore) -> bool {
        self.is_deadlocked_ignoring_jobs_given(c, None)
    }

    /// TICKET-118 (W13-8) — [`MnSched::is_deadlocked_ignoring_jobs`], parameterised on an
    /// already-computed `cancelled_scope_awaiting_drain` read.
    fn is_deadlocked_ignoring_jobs_given(
        &self,
        c: &SchedCore,
        awaiting_drain: Option<bool>,
    ) -> bool {
        c.cross_sched_blocked_owners == 0
            && !self.body_held_by_fiber(c)
            && self.local_quiesced_given(c, awaiting_drain)
            && !self.any_peer_can_move()
    }

    /// TICKET-112 — this sched's own body is open and run by a fiber of another sched
    /// (`body_is_fiber`): that fiber may still feed this sched's parked fibers, so this sched must not
    /// fault itself while its body is open. `close_body`'s `cv.notify_all()` re-judges once it joins.
    pub(super) fn body_held_by_fiber(&self, c: &SchedCore) -> bool {
        self.body_is_fiber && c.any_body_open()
    }

    /// D5 — hand a fiber that hit a blocking native call to the dirty/blocking pool, freeing this
    /// core worker. Transitions running→inflight under the core lock (so the deadlock predicate is
    /// immediately sound: the fiber is accounted as in-flight, not vanished), then submits the call.
    /// The pool thread runs the native off-heap ([`run_offload`]) and `complete_offload`s the fiber
    /// back onto the run queue. Takes `&Arc<Self>` so the completion closure can hold the scheduler.
    pub(super) fn offload(self: &Arc<Self>, fiber: Fiber, req: OffloadReq) {
        {
            let mut c = self.lock();
            c.leave_running();
            self.inflight.fetch_add(1, Ordering::Relaxed); // running → inflight
        }
        let sched = Arc::clone(self);
        // On the timer path `func`/`args` are intentionally unused (a sleep computes nothing — the
        // fiber resumes with `Nil`); they are consumed only by the pool branch below.
        let OffloadReq {
            func,
            args,
            span,
            timer,
        } = req;
        if let Some(t) = timer {
            // D5 owe #2 — a `sleep_ms`: park the fiber on the timer thread (no pool thread, no work),
            // waking it at the deadline. `sleep_ms` returns nothing, so the fiber resumes with
            // `Ok(Nil)` and the native is never run (there is nothing to compute). Same
            // inflight→runnable + `notify` accounting as the pool path (`complete_offload`), so the
            // deadlock predicate stays sound: the sleeping fiber is `inflight` and WILL come back.
            arm_timer_sleep(sched, fiber, t, span);
            return;
        }
        blocking_pool::submit(Box::new(move || {
            // `complete_offload` MUST run on every path — if it didn't (e.g. the native panicked and
            // unwound), `inflight` would stay pinned forever, vetoing the deadlock predicate and
            // hanging the nursery with the fiber lost. So catch a panic here and surface it as a
            // fault on the fiber (matching an inline native panic, which `run_one_fiber`'s
            // `catch_unwind` also turns into a task fault) rather than letting it escape into the
            // pool's belt-and-suspenders `catch_unwind`.
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_offload(func, args)));
            let result = match outcome {
                Ok(Ok(nr)) => Ok(nr),
                Ok(Err(e)) => Err(RuntimeError {
                    message: e.message,
                    span,
                    is_assert: false,
                    is_over_memory: false,
                    is_timed_out: false,
                    is_deadlock: false,
                    is_panic: false,
                    blocked: None,
                }),
                Err(p) => Err(panic_to_fault(p, span)),
            };
            let mut fiber = fiber;
            fiber.resume_native = Some(result);
            sched.complete_offload(fiber);
        }));
    }

    /// D6 — hand a fiber whose socket op returned `WouldBlock` to the netpoller, freeing this core
    /// worker. Transitions running→inflight under the core lock (so the deadlock predicate is
    /// immediately sound — a socket-parked fiber is accounted as in-flight, vetoing a false deadlock;
    /// it WILL be woken by the OS), then registers fd interest. The poller `complete_offload`s the
    /// fiber back onto the run queue on readiness (inflight→runnable), exactly like the blocking pool.
    /// Takes `&Arc<Self>` so the poller can hold the scheduler for that completion.
    pub(super) fn poll_park_offload(self: &Arc<Self>, fiber: Fiber, pp: PollPark) {
        // Cross-nursery flat scheduler — clone THIS fiber's SCOPE cancel and its ancestors' flags
        // (TICKET-200, `ScopeCancel`; not the sched's legacy global `cancel`, which is only the
        // OUTERMOST nursery's) while we hold the core lock anyway: they are the flags `register` must
        // gate the park on, exactly like `park`/`park_wait`'s gap re-check. Reading
        // the global one here would let a fiber of a CANCELLED INNER scope park on a poller that
        // `drain_family` had already swept — stranding it, and (N4) holding the cancel-teardown veto
        // forever.
        let cancel = {
            let mut c = self.lock();
            c.leave_running();
            self.inflight.fetch_add(1, Ordering::Relaxed); // running → inflight
            c.scope_cancel(fiber.scope_id)
        };
        // `register` rejects (returns the fiber) iff that scope's cancel was tripped before it could
        // park — a sibling faulted in the park-vs-cancel gap. Re-inject so the fiber resumes and unwinds
        // on the cancel flag, rather than parking on a poller a past `drain_family` already swept (→ a
        // hang).
        if let Some(fiber) = poller::register(
            pp.key,
            pp.fd,
            pp.interest,
            fiber,
            Arc::clone(self),
            cancel,
            Arc::clone(&pp.in_flight),
            Arc::clone(&pp.closed),
            pp.deadline,
        ) {
            pp.in_flight.store(false, Ordering::Release);
            self.complete_offload(fiber);
        }
    }

    /// D5 — the blocking pool finished an offloaded native: re-enqueue the fiber (with its result
    /// stashed in `resume_native`) as `Ready` on the global queue and wake a worker. Transitions
    /// inflight→runnable under the core lock; the `notify_all` is the wakep (reusing the D4 wake
    /// path). The `runnable.fetch_add` here happens under the core lock and is followed by `notify`,
    /// so D4e's runnable-gated park is lost-wakeup-free: a worker parking concurrently either sees
    /// `runnable > 0` and does not sleep, or is already in `cv.wait` and is reached by this `notify`.
    /// D6 reuses this verbatim as the netpoller's inject path (a socket op re-runs, so it stashes no
    /// `resume_native` — the fiber's stays `None`).
    pub(super) fn complete_offload(&self, mut fiber: Fiber) {
        let mut c = self.lock();
        self.leave_inflight(); // inflight → runnable
        fiber.state = FiberState::Ready;
        c.global.push_back(fiber);
        self.runnable.fetch_add(1, Ordering::Relaxed);
        drop(c);
        self.notify_waiters();
    }
}
