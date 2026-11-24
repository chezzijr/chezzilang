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
        let ended = self.finish_with(task_index, scope_id, outcome, None);
        // No party here runs `halt_run`, so no report may wait for one.
        if ended.1 {
            self.quiesce.end_job_halt();
        }
        ended
    }

    /// Record a finished fiber's outcome in its FLAT slot, bump its SCOPE's done, drop it from
    /// `running`. Sets GLOBAL `terminate` only when EVERY scope is done (and no eager body is open) —
    /// because farmed helpers (sentinel scope_id) drain until global terminate, and the scope-scoped
    /// owner stop returns each owner the instant its OWN scope completes. The per-scope `done` drives
    /// that owner stop; the global all-done drives helper/sentinel termination.
    /// Returns whether the stored outcome ABORTS its scope (a `Fault`/`Exit`), so the caller runs the
    /// [`cancel`] its parked siblings need. It is computed here, not by the caller, because the
    /// W7-26r backlog verdict below can turn a `Done` into a hard-halt `Fault` — a caller-side
    /// `matches!` on the outcome it handed in would miss that and leave the scope's parked fibers
    /// with nobody to wake them. Also returns whether it published a run-halting fault
    /// ([`MnSched::publish_job_fault`], for the task's own abort or the scope's first backlog
    /// verdict), so the caller runs [`Vm::halt_run`].
    /// TICKET-297 — `trip` is the cancel flag the fiber ran under (`Vm::cancel`: its scope's flag,
    /// or an Executor job's own). An aborting outcome ([`TaskOutcome::aborts`]) trips it HERE, in
    /// the core-lock hold that stores the slot: THE one trip of a task's aborting end. A party that
    /// reads the tripped flag and then asks [`MnSched::scope_fault`] (`block::halt_of`) waits for
    /// this lock, so it finds the fault. A trip before the lock let an owner parked at an inner join
    /// read the flag with an empty slot, take no halt, and run the line after its join. Under the
    /// lock the trip also reaches the deadlock verdict (as [`cancel`]'s scope trip).
    pub(super) fn finish_with(
        &self,
        task_index: usize,
        scope_id: usize,
        outcome: TaskOutcome,
        trip: Option<&AtomicBool>,
    ) -> (bool, bool) {
        let mut c = self.lock();
        c.leave_running();
        // A task the latched verdict cut in place cancels no sibling: the verdict ended every
        // task of the run at once ([`MnSched::on_deadlock`]), and a trip would turn a listed
        // sibling into a cancelled task.
        let verdict_end = matches!(&outcome, TaskOutcome::Fault { .. })
            && self.quiesce.run_halt() == RunHalt::Deadlock;
        let cut_end = verdict_end && c.cut_in_place();
        let job_fault = self.publish_job_fault(&c, scope_id, &outcome);
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
        let (outcome, first_over) = if self.mem_cap != 0 {
            let s = &mut c.scopes[scope_id];
            s.bytes += super::core::outcome_summary(&outcome);
            let (outcome, over) = super::core::halt_over_backlog(outcome, s.bytes, self.mem_cap);
            // The scope's bytes stay over the cap, so every later end converts too. Only the first
            // halts the run: a second publish would report the same abort as a second fault.
            let first_over = over && !s.cancel.load(Ordering::Relaxed);
            if over {
                // Under the core lock, exactly as [`cancel`]'s scope trip — the release
                // publishes the flag to every worker that later takes the lock. Only THIS
                // scope: an inner nursery's backlog must not cancel an outer sibling (structured
                // concurrency); the fault propagates outward through the join instead.
                crate::vm::trip_cancel_flag(&s.cancel);
            }
            (outcome, first_over)
        } else {
            (outcome, false)
        };
        let job_fault = job_fault || (first_over && self.publish_job_fault(&c, scope_id, &outcome));
        let aborts = outcome.aborts() && !cut_end;
        // A job the deadlock verdict ended: the verdict cut the sched (it latched `terminate`, or a
        // demoted fiber is left to settle), so a released held job would never run and its `None`
        // slot would hang the join. It reads as a latched verdict.
        let seal = match &outcome {
            // The body returned: `_run_outcome` sealed its value, and the first writer wins.
            TaskOutcome::Done(_) => JobSeal::Nothing,
            // A hard abort is never a Task outcome (TICKET-289): the run halts instead. A fault
            // `publish_job_fault` published ends the run the same way.
            TaskOutcome::Fault { err, .. } | TaskOutcome::CancelledFault { err, .. }
                if job_fault || executor_hard_halt(err) =>
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
        // The task's end wakes its joiners and the readers of its handle below: a token task keeps
        // the count awake until those wakes are made, so the end counts after them.
        c.awake.task_started(1);
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
        if self.quiesce.awake.task_ended() {
            self.on_deadlock();
        }
        (aborts, job_fault)
    }

    /// TICKET-270 — THE publisher of the task ends that halt the run: an `Executor` job's
    /// `--max-heap` abort (TICKET-289) or the root task's. A job's is never a Task outcome, so the
    /// run halts and the Task seals nothing ([`JobSeal::Nothing`]). The root's halts the run so a
    /// job blocked in place on the driver thread stops waiting. Every other job end stays in its Task: an
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
                if (c.is_job_scope(scope_id) || (c.root && scope_id == 0))
                    && err.is_over_memory =>
            {
                self.quiesce.request_job_fault(err.clone(), trace.clone())
            }
            TaskOutcome::Fault { .. }
            | TaskOutcome::Exit { .. }
            | TaskOutcome::Done(_)
            | TaskOutcome::Cancelled { .. }
            | TaskOutcome::CancelledFault { .. }
            | TaskOutcome::Deadlocked { .. } => false,
        }
    }

    /// [`cancel`]'s `All` on this sched (gaps.md W7-57, the run-wide `os.exit` analogue of the
    /// intra-nursery abort teardown): trip **every** scope's cancel and drain every parked fiber,
    /// so a nursery that is not the exiting party's own still dies now instead of at its natural end.
    ///
    /// **The cancel stores go under the core lock** — the release edge [`cancel`]'s scope trip
    /// documents. A bare `Relaxed` store outside it has no synchronizes-with edge, so a worker
    /// already holding the lock could legally still read `false`.
    ///
    /// The lock is DROPPED before `drain_family`, which takes it itself (its poller half keeps the
    /// registry leaf-level and calls `complete_offload`, which locks this core).
    ///
    /// The live scope ids are snapshotted: a scope registered after this instant is a nursery created
    /// after the exit was published, whose fibers are covered by `jump_checked`'s back-edge exit rung
    /// and by every blocking wait's `run_exit_err` — this drain is a promptness lever, not the only one.
    /// A snapshotted id retired before its drain reads as absent (TICKET-199), so its drain is a no-op.
    fn cancel_all(&self) {
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
    /// requeue every fiber of its family that is parked, in both halves ([`MnSched::drain_where`]).
    /// A socket-parked fiber is counted `inflight`, not `parked`, so a drain of one half strands it
    /// — the `cancel` hang of a job's nursery fiber parked in `accept`/`read`/`write`/`connect`.
    /// A retired id has an empty family (TICKET-199, DEC-199), so both halves are no-ops.
    ///
    /// B3.4 — scope-scoped: an inner fault must drain ONLY its own scope's parked fibers, never drag
    /// an OUTER sibling out of its legitimate park (structured concurrency). TICKET-103 — "its own
    /// scope" is its FAMILY (`SchedCore::scope_family`): a nursery's continuation scopes share its
    /// one cancel token, so a faulting child must requeue a parked sibling in a continuation too.
    fn drain_family(&self, scope_id: usize) {
        let family = self.lock().scope_family(scope_id);
        self.drain_where(|f| f.woken_by(&family));
    }

    /// TICKET-289 — the cancel drain of the fibers under `flag` on this sched: a fiber of the
    /// Executor job whose flag it is (`Task.cancel()`), and the fibers of every scope under it.
    /// The scope ids are read once, under the lock: a scope registered after the trip starts with
    /// the flag already tripped and unwinds on its own.
    fn drain_job(&self, flag: &Arc<AtomicBool>) {
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

    /// The `parked` half of [`MnSched::drain_where`]: move every parked fiber `wakes` selects back
    /// onto the global queue so a worker resumes it and it observes the cancel (at the recv
    /// re-check / a dispatch back-edge) and unwinds. Entries of other fibers are re-filed.
    fn cancel_drain_where(&self, wakes: &dyn Fn(&Fiber) -> bool) {
        let c = self.lock();
        if c.parked_n == 0 {
            return;
        }
        // A join-parked owner of this family stays filed (see `SchedCore::join_parked`): its
        // children see the tripped token, finish, and `finish` requeues it.
        let _ = self.drain_parked(c, wakes);
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
                    // A verdict that latched first lists the fibers this would requeue.
                    let _gate = s.quiesce.verdict_gate();
                    if s.quiesce.run_halt() != crate::vm::quiesce::RunHalt::Deadlock {
                        s.deadline_drain();
                    }
                }
            }),
        );
    }

    /// TICKET-286 — requeue every parked fiber whose `--timeout` deadline passed, so it re-runs its
    /// op and returns the hard halt at the op's deadline checkpoint. A stale job (a test that ended
    /// before its deadline) finds no due fiber: a later test's fibers park under a later deadline.
    /// Returns whether it requeued any.
    pub(super) fn deadline_drain(&self) -> bool {
        let c = self.lock();
        if c.parked_n == 0 {
            return false;
        }
        self.drain_parked(c, Fiber::deadline_due) > 0
    }

    /// The one parked-set drain: requeue every parked fiber `wake` selects, in every bucket shape.
    /// A selected sleep ([`SleepPark`]) is claimed under `c` and settled after `c` is released
    /// ([`settle_sleep`] reads the halts, which take other scheds' locks). Returns how many it
    /// requeued.
    fn drain_parked(
        &self,
        mut c: std::sync::MutexGuard<'_, SchedCore>,
        wake: impl Fn(&Fiber) -> bool,
    ) -> usize {
        let buckets: Vec<(usize, Vec<ParkedEntry>)> = c.parked.drain().collect();
        let mut drained = 0usize;
        let mut slept: Vec<(Fiber, Arc<SleepPark>)> = Vec::new();
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
                    ParkedEntry::Sleep(sp) => {
                        // Not a verdict's task (a sleeper is awake), so never listed. The claim
                        // and the removal share this hold: a filed sleep is unclaimed.
                        debug_assert!(
                            !sp.claimed.load(Ordering::Acquire),
                            "a filed sleep is claimed"
                        );
                        let f = {
                            let mut g = sp.fiber.lock().unwrap_or_else(|e| e.into_inner());
                            if g.as_ref().is_some_and(&wake)
                                && sp
                                    .claimed
                                    .compare_exchange(
                                        false,
                                        true,
                                        Ordering::AcqRel,
                                        Ordering::Acquire,
                                    )
                                    .is_ok()
                            {
                                g.take()
                            } else {
                                None
                            }
                        };
                        match f {
                            Some(f) => slept.push((f, sp)),
                            None => keep.push(ParkedEntry::Sleep(sp)),
                        }
                    }
                }
            }
            if !keep.is_empty() {
                c.parked.insert(key, keep);
            }
        }
        c.parked_n -= drained + slept.len();
        c.awake.woken(drained);
        self.runnable.fetch_add(drained, Ordering::Relaxed); // parked → ready
        self.notify_waiters();
        drop(c);
        let woke = drained + slept.len();
        for (f, sp) in slept {
            let sched = sp.sched.upgrade().expect("a filed sleep's sched is alive");
            settle_sleep(&sched, f, &sp);
        }
        woke
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
    /// `drain_scopes`, `take_finished_streams`) calls it while it holds the sched lock, so a
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

    #[must_use]
    /// Every slot still held, of every scope, in slot order: after the run's deadlock verdict
    /// ended a root sched's tasks ([`Vm::run_root`]), for the output they buffered.
    pub(super) fn take_rest_slots(&self) -> Vec<Option<TaskOutcome>> {
        let mut c = self.lock();
        for s in c.scopes.values_mut() {
            s.bytes = 0;
        }
        let slots: Vec<_> = c.slots.iter_mut().map(Option::take).collect();
        self.hold_exit_output(slots.iter().flatten());
        slots
    }

    #[cfg(test)]
    pub(super) fn take_scope_slots(&self, scope_id: usize) -> Vec<Option<TaskOutcome>> {
        self.drain_scopes(&[scope_id]).collect()
    }

    /// Cross-nursery flat scheduler — drain each scope's contiguous slot sub-range
    /// (`base_index..base_index+total`) in task order, leaving other scopes' slots intact. The reducer
    /// flushes each scope's `Done`/`Exit` output in order (Decision F) — the slots are contiguous
    /// because `register_scope`/`inject` only ever grow the flat vec / the LAST scope. Replaces each
    /// taken slot with `None` so a re-take is a no-op (and the flat vec keeps its length, so other
    /// scopes' `base_index` stays valid). Scopes go in `sids` order. The iterator holds the sched
    /// lock until it drops and takes each slot where it lies, so a reduce that consumes it builds
    /// no copy of the slots (a 100k-task join held two).
    pub(super) fn drain_scopes<'a>(
        &'a self,
        sids: &[usize],
    ) -> impl Iterator<Item = Option<TaskOutcome>> + 'a {
        let mut c = self.lock();
        // W7-26r — the retained bytes leave with the slots, so the backlog total returns to zero with
        // them (`the Executor's sched::take_slots` does the same for the executor half). Today every caller
        // takes only after `wait_for_scope`, so no fiber of this scope can still be running; this
        // keeps the counter honest if that ever stops being true, instead of leaving a stale
        // watermark that would fault the NEXT program to reach this scope id.
        let ranges: Vec<_> = sids
            .iter()
            .map(|&sid| {
                let s = &mut c.scopes[sid];
                s.bytes = 0;
                s.base_index..s.base_index + s.total
            })
            .collect();
        ranges.into_iter().flatten().map(move |i| {
            let slot = c.slots[i].take();
            self.hold_exit_output(&slot);
            slot
        })
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
    /// still settling this scope's last fiber. Poison-tolerant. Returns `false` when a run halt
    /// ended the wait first ([`RunHalt::ends_join`]): `halt_run` publishes the halt, then takes
    /// this lock and notifies `cv`, so the read below cannot miss it.
    pub(super) fn wait_for_scope(&self, scope_id: usize) -> bool {
        let mut c = self.lock();
        while c.scopes[scope_id].done < c.scopes[scope_id].total {
            if self.quiesce.run_halt().ends_join() {
                return false;
            }
            c = self.cv.wait(c).unwrap_or_else(|e| e.into_inner());
        }
        true
    }

    /// Block until every scope of `scope_id`'s family is done. Unlike [`MnSched::wait_for_scope`],
    /// a run halt does not end it: the caller started a runner that settles the family.
    pub(super) fn wait_for_family(&self, scope_id: usize) {
        let mut c = self.lock();
        while !c.family_done(scope_id) {
            c = self.cv.wait(c).unwrap_or_else(|e| e.into_inner());
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
        while c.any_scope_incomplete() {
            c = self.cv.wait(c).unwrap_or_else(|e| e.into_inner());
        }
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
            // D5 owe #2 — a `sleep_ms`: park the fiber with one timer (no pool thread, no work),
            // waking it at the deadline or at a cancel. `sleep_ms` returns nothing, so the fiber
            // resumes with `Ok(Nil)` and the native is never run (there is nothing to compute). Same
            // inflight→runnable + `notify` accounting as the pool path (`complete_offload`); the
            // sleeping fiber stays awake in the run's count: it WILL come back.
            arm_timer_sleep(sched, fiber, Arc::new(t), span);
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
                    is_panic: false,
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

/// What a [`cancel`] covers.
pub(super) enum CancelTarget<'a> {
    /// Scope `.1` of sched `.0` and its family: a child fault, an owner's abort of the nursery it
    /// owns, an Executor's `cancel()` and its join `--timeout` (scope 0 of the Executor's sched).
    Scope(&'a MnSched, usize),
    /// A flag that is no scope's own: an Executor job's (`Task.cancel()`), an Executor's before it
    /// has a sched, the `--timeout` gap wake's `Vm::cancel`. `None` trips nothing (a held job was
    /// dropped, or the job had ended): the poke alone.
    Flag(Option<&'a Arc<AtomicBool>>),
    /// Every scope of every live sched: `os.exit` and a job fault ([`Vm::halt_run`]).
    All,
}

/// THE cancel: every source trips and wakes here, in this order. 1. Trip the flag
/// ([`crate::vm::trip_cancel_flag`]); a scope's under its sched's core lock, whose release
/// publishes it to every worker that later takes the lock, and which lets an abort seam trip
/// before it clears `body_open` (N4). 2. Drain the target's parked and socket-parked fibers on its
/// own sched. 3. Drain the fibers under the flag on every live sched of `registry`: a nested
/// Executor a task or job created holds its jobs on a sched of its own (it inherits the flag as an
/// ancestor). This drain is their only wake, and it runs before the canceller can park and read
/// every task asleep. 4. Poke every live sched (TICKET-118,
/// W13-8): take its core lock and drop it, then notify. A worker that read the stale state did so
/// under that lock, so it is either on the condvar already (and gets the notify) or reads the new
/// state; a bare notify could land in the gap and be lost. Its demoted waits are kicked
/// ([`MnSched::wake_demoted`]): a thread blocked in place sleeps on a condvar of its own wait.
/// The caller holds no sched core lock.
pub(super) fn cancel(registry: &crate::vm::SchedRegistry, target: CancelTarget<'_>) {
    let flag = match target {
        CancelTarget::Scope(sched, scope_id) => {
            let flag = {
                let c = sched.lock();
                // A retired id reads as absent (TICKET-199): nothing to trip or drain.
                let flag = c.scopes.get(scope_id).map(|s| Arc::clone(&s.cancel));
                if let Some(f) = &flag {
                    crate::vm::trip_cancel_flag(f);
                }
                flag
            };
            sched.drain_family(scope_id);
            flag
        }
        CancelTarget::Flag(flag) => {
            if let Some(f) = flag {
                crate::vm::trip_cancel_flag(f);
            }
            flag.cloned()
        }
        CancelTarget::All => {
            for s in live_scheds(registry) {
                s.cancel_all();
            }
            None
        }
    };
    let live = live_scheds(registry);
    if let Some(flag) = &flag {
        for s in &live {
            s.drain_job(flag);
        }
    }
    for s in live {
        drop(s.lock());
        s.notify_waiters();
        s.wake_demoted();
    }
}

/// The live scheds of `registry`, pruning the dead. The registry lock is dropped before the caller
/// touches a sched: lock order stays `sched_registry → SchedCore`.
fn live_scheds(registry: &crate::vm::SchedRegistry) -> Vec<Arc<MnSched>> {
    let mut g = registry.lock().unwrap_or_else(|e| e.into_inner());
    let live = g.iter().filter_map(|w| w.upgrade()).collect();
    g.retain(|w| w.strong_count() > 0);
    live
}
