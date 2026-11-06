//! VM: `SchedCore` verdicts, jobs and families, `panic_to_fault`, the ready worker.
//! Split out of `src/vm/mod.rs`. `super::*` is the parent module.

use super::*;

impl SchedCore {
    /// Fault every still-parked fiber (across ALL scopes) with the deadlock error, cut every
    /// blocked-in-place waiter (`verdict_cut`) and latch `terminate` unless a demoted fiber is still
    /// registered (called under the lock). An aborting family is cut first, alone
    /// ([`SchedCore::cut_aborting_families`]). Returns whether `terminate` latched. Correct because the
    /// global predicate firing means nothing can progress anywhere. A `Wait` token sits in N buckets
    /// but is ONE fiber — claim-once (the wake CAS) dedups it so the flat slot is faulted and the fiber's SCOPE's `done` bumped exactly once.
    /// Each parked fiber's OWN buffered stdout/stderr (moved into `f.ctx.out`/`stderr` by `swap_task`
    /// when it parked) is carried into its `Deadlocked` slot, so `reduce_task_slots` flushes EVERY
    /// parked buffer at its task-order slot — matching what a strictly sequential run would have
    /// printed live before the deadlock returned. A distinct `Deadlocked` outcome (not `Fault`) is what lets
    /// the reduce flush ALL parked buffers here without disturbing the real-fault multi-fault path.
    pub(super) fn flag_deadlock(&mut self, err: &RuntimeError) -> bool {
        if self.cut_aborting_families() {
            return false;
        }
        let buckets: Vec<Vec<ParkedEntry>> = self.parked.drain().map(|(_, v)| v).collect();
        for v in buckets {
            for entry in v {
                let fiber = match entry {
                    ParkedEntry::Recv(f) => Some(f),
                    ParkedEntry::Send(f) => Some(f),
                    ParkedEntry::Wait(wp) => {
                        if wp
                            .claimed
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                            .is_err()
                        {
                            None
                        } else {
                            Some(
                                wp.fiber
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .take()
                                    .expect("WaitPark fiber claimed twice"),
                            )
                        }
                    }
                };
                if let Some(f) = fiber {
                    self.record_deadlocked(f, err);
                }
            }
        }
        // TICKET-103 — `terminate` stops every worker before its next global pop, so a join-parked
        // owner requeued here would never run: record it `Deadlocked` instead.
        for (origin, f) in std::mem::take(&mut self.join_parked) {
            self.scopes[origin].joins_blocked -= 1;
            self.scopes[f.scope_id].owners_blocked -= 1;
            self.record_deadlocked(f, err);
        }
        self.parked_n = 0;
        // TICKET-296: a demoted fiber reads the cut, unwinds and can requeue itself, so the workers
        // stay until it settles. Returns whether `terminate` latched.
        self.verdict_cut = true;
        self.terminate = !self.any_fiber_waiter();
        self.terminate
    }

    /// The verdict's cut of an aborting family: a demoted fiber of a cancelled scope (its owner's
    /// own fault, or a child's fault, tripped the scope's cancel, before or after the owner reached
    /// its join) waits in a `defer` that nothing can finish, and the owner waits for that family
    /// alone. The verdict cuts those waiters as the scope's cancel ([`Vm::in_place_cut`]) and
    /// nothing else, and latches nothing: the cause wins (DEC-147), the owner unwinds, and a later
    /// wait is judged again. Returns whether such a waiter exists; without one the verdict is
    /// sched-wide.
    fn cut_aborting_families(&mut self) -> bool {
        let cancelled: Vec<usize> = self
            .scopes
            .iter()
            .filter(|(_, s)| s.done < s.total)
            .map(|(i, _)| i)
            .filter(|&i| self.scope_cancel_tripped(i))
            .collect();
        let mut any = false;
        for w in self.waiters.values_mut() {
            if w.fiber && w.scope.is_some_and(|s| cancelled.contains(&s)) {
                w.cut = true;
                any = true;
            }
        }
        any
    }

    /// TICKET-296: the sched is torn down, or the sched-wide verdict cut every blocked-in-place
    /// waiter. Read through [`Vm::in_place_cut`].
    pub(super) fn cut_in_place(&self) -> bool {
        self.terminate || self.verdict_cut
    }

    /// Did the verdict cut the waiter `tok` as the stuck cleanup of a cancelled scope
    /// ([`SchedCore::cut_aborting_families`])? Read through [`Vm::in_place_cut`].
    pub(super) fn waiter_cut(&self, tok: Option<u64>) -> bool {
        tok.is_some_and(|t| self.waiters.get(&t).is_some_and(|w| w.cut))
    }

    /// Store one claimed parked fiber's `Deadlocked` outcome and bump its scope's `done`. Shared by
    /// `flag_deadlock` and `flag_deadlock_leaves`.
    fn record_deadlocked(&mut self, f: Fiber, err: &RuntimeError) {
        // Carry the parked fiber's OWN buffered stdout/stderr into its Deadlocked slot (not an empty
        // buffer). `swap_task` moved this fiber's live prints into `f.ctx.out` when it parked; the
        // downstream `reduce_task_slots` flushes EVERY Deadlocked slot's buffer at its task-order slot
        // (not just the lowest-index one, as with a real Fault), so with two-or-more parked fibers a
        // higher-index printer's output is preserved byte-identically to a strictly sequential run.
        // `task_index`/`scope_id` are Copy, read before `f.ctx` moves out.
        let (ti, sid) = (f.task_index, f.scope_id);
        // A scope with its own `deadlock_err` (the owner-fiber arm of `run_mn_nursery_nested`)
        // reports ITS span instead of the sched-wide `err` — see `JoinScope::deadlock_err` and
        // DEC-048.
        // TICKET-208 — else, on an `Executor`'s sched, the fiber's own blocking op: the text and
        // span the in-place path raises there. Else the sched-wide `err`.
        // The listing names each task's own wait: its park site, else its scope's own fault
        // (an owner parked at a join names no site), else the sched-wide `err`.
        let scope_err = match (f.park_site, &self.scopes[sid].deadlock_err) {
            (Some((msg, span)), _) => err.at_site(msg, span),
            (None, Some(e)) => e.clone(),
            (None, None) => err.clone(),
        };
        // M1.4 — take the context out of the registry before any field moves: this thread may not
        // be attached to the GC (see `RootRegistry::roots`).
        let ctx = f.ctx.into_inner();
        let frames = ctx
            .frames
            .iter()
            .rev()
            .filter(|f| !f.is_toplevel)
            .map(|f| (f.proto, f.call_span))
            .collect();
        self.slots[ti] = Some(TaskOutcome::Deadlocked {
            err: scope_err,
            out: ctx.out,
            stderr: ctx.stderr,
            frames,
        });
        self.task_done(sid);
        self.job_event(
            JobEvent::Reaped {
                task_index: ti,
                scope_id: sid,
            },
            RunHalt::Running,
        );
    }

    /// Whether `scope_id` is one of the Executor's own scopes, so its fiber is a job and not a
    /// job's nursery child. `leaf_site` is true for an Executor's sched alone.
    pub(super) fn is_job_scope(&self, scope_id: usize) -> bool {
        self.leaf_site && self.scope_family(0).contains(&scope_id)
    }

    /// TICKET-219 — THE job-state transition: the only reader and writer of `exec_held`,
    /// `exec_active` (except the no-runner pre-count in `spawn_into`) and `job_settle`. `halt` is
    /// [`quiesce::QuiesceState::run_halt`], read by the caller under this lock; `cancelled` is
    /// scope 0's own flag (`cancel`) or an ancestor's (creator cancel). The rule, read before
    /// anything is released:
    /// 1. `halt` is `Exit` or `Deadlock`: drop every held job, settle nothing; neither runs a
    ///    `defer`. A sealed value is a ready receive (DEC-194), so a seal would let a reader print
    ///    after it ([`RunHalt::settles`], TICKET-223).
    /// 2. else `halt` is `Fault`, or `cancelled`: drop every held job and settle each with its
    ///    cancel value. A `defer` still runs whole after a `--max-heap` abort, and it may read the handle.
    /// 3. else: release held jobs while `exec_active < exec_limit`.
    ///
    /// TICKET-232 — a `Submit` under the cap reads the halt alone, never `cancelled`: the submit
    /// was accepted at the slot reservation, and `spawn_into` starts runner threads between that
    /// lock hold and this one, so a job's `cancel()` can land there. A cancel read here
    /// dropped the job with exit status zero. A `Submit` over the cap is held and falls through to
    /// the tail, the one cancel read.
    ///
    /// A `--max-heap` abort's cell is stored before any sched's flag trips, so the halt read closes that window.
    pub(super) fn job_event(&mut self, ev: JobEvent, halt: RunHalt) -> JobStep {
        let mut step = JobStep::default();
        match ev {
            JobEvent::Submit(fiber) => {
                let fiber = *fiber;
                if halt != RunHalt::Running {
                    self.drop_job(fiber, halt.settles(), &mut step);
                    return step;
                }
                if self.exec_limit == 0 || self.exec_active < self.exec_limit {
                    self.exec_active += 1;
                    step.start.push(fiber);
                    return step;
                }
                self.exec_held.push_back(fiber);
            }
            JobEvent::Ended {
                task_index,
                scope_id,
                seal,
            } => {
                if !self.is_job_scope(scope_id) {
                    return step;
                }
                self.exec_active -= 1;
                self.settle_job(task_index, seal, &mut step);
            }
            JobEvent::Reaped {
                task_index,
                scope_id,
            } => {
                // `flag_deadlock` latches `terminate` (or leaves a demoted fiber to settle), so a
                // released job would never run and its `None` slot would hang the join; the parked job's `Deadlocked` is the report.
                if self.is_job_scope(scope_id) {
                    self.exec_active -= 1;
                    self.settle_job(task_index, JobSeal::Nothing, &mut step);
                    self.drop_held(false, &mut step);
                }
                return step;
            }
            JobEvent::DropHeld => {
                self.drop_held(halt.settles(), &mut step);
                return step;
            }
            JobEvent::CancelOne(ch) => {
                self.cancel_one(&ch, halt, &mut step);
                return step;
            }
        }
        if self.exec_held.is_empty() {
            return step;
        }
        if halt != RunHalt::Running || self.job_cancelled() {
            self.drop_held(halt.settles(), &mut step);
            return step;
        }
        // The loop condition, not a bare pop, keeps the cap: the no-runner path of `spawn_into`
        // admits a slot without testing the cap, and its `finish` must start nothing while
        // `exec_active` is still at the cap.
        while self.exec_active < self.exec_limit {
            let Some(f) = self.exec_held.pop_front() else {
                break;
            };
            self.exec_active += 1;
            step.start.push(f);
        }
        step
    }

    /// Scope 0's own cancel flag (`cancel`) or an ancestor's (creator cancel) is tripped.
    fn job_cancelled(&self) -> bool {
        self.scopes.get(0).is_some() && self.scope_cancel_tripped(0)
    }

    fn drop_held(&mut self, settle: bool, step: &mut JobStep) {
        for f in std::mem::take(&mut self.exec_held) {
            self.drop_job(f, settle, step);
        }
    }

    /// A job that never starts: its slot is `Cancelled`, and its handle settles with the cancel
    /// value when `settle` is set.
    fn drop_job(&mut self, f: Fiber, settle: bool, step: &mut JobStep) {
        let seal = if settle {
            JobSeal::Cancel
        } else {
            JobSeal::Nothing
        };
        self.slots[f.task_index] = Some(TaskOutcome::Cancelled {
            out: Vec::new(),
            stderr: Vec::new(),
        });
        self.task_done(f.scope_id);
        self.settle_job(f.task_index, seal, step);
    }

    /// TICKET-289 — `Task.cancel()`: the job whose Task channel is `ch`. A held job never starts:
    /// it is dropped, and its Task settles with the cancel value. A running job gets its own flag
    /// tripped here, under this lock, and the caller drains what that flag wakes
    /// ([`JobStep::cut_flag`]). A job that already ended has no record, so nothing happens.
    fn cancel_one(&mut self, ch: &Arc<ChannelCore>, halt: RunHalt, step: &mut JobStep) {
        let Some(idx) = self
            .job_settle
            .iter()
            .find(|(_, s)| Arc::ptr_eq(&s.ch, ch))
            .map(|(i, _)| *i)
        else {
            return;
        };
        if let Some(pos) = self.exec_held.iter().position(|f| f.task_index == idx) {
            if let Some(f) = self.exec_held.remove(pos) {
                self.drop_job(f, halt.settles(), step);
            }
            return;
        }
        let flag = Arc::clone(&self.job_settle[&idx].flag);
        crate::vm::trip_cancel_flag(&flag);
        step.cut_flag = Some(flag);
    }

    /// TICKET-232 — THE writer of a Task's non-body outcome: forget job `task_index`'s settle
    /// record and seal its channel as `seal` says, here, in the lock hold that ends the job. The
    /// join returns on the slot, so a seal written after the unlock leaves a reader on an empty
    /// channel with no counted party: a false deadlock. The first writer wins: a job body that
    /// already sealed its value keeps it, and nothing is woken. [`JobSeal::Nothing`] seals
    /// nothing and counts no leaver. Lock order `SchedCore` then `ChannelCore::q`, as `send_wake`.
    pub(super) fn settle_job(&mut self, task_index: usize, seal: JobSeal, step: &mut JobStep) {
        let Some(s) = self.job_settle.remove(&task_index) else {
            return;
        };
        let sealed = match seal {
            JobSeal::Nothing => return,
            JobSeal::Cancel => s.seal_err(crate::vm::EXEC_CANCELLED_MSG),
            JobSeal::Fault(msg) => s.seal_err(&msg),
        };
        if sealed {
            step.sealed.push(s.ch);
        }
        // TICKET-236 — LAST: the seal above is this leaver's final channel effect, and the caller
        // still holds the lock that ends the job. Moved above the seal, a judge could open its
        // bracket after the bump, read the unsealed handle, then read the finished sched.
        self.note_left();
    }

    /// TICKET-103 — every scope sharing `scope_id`'s cancel token: a nursery's origin scope plus its
    /// continuation scopes (`MnSched::inject_or_extend`). Every other scope owns a distinct Arc, so
    /// its family is itself. TICKET-199 — a retired `scope_id` has an EMPTY family: a worker that
    /// still holds the id after dropping the lock (`cancel_drain` after `finish`) touches nothing.
    pub(super) fn scope_family(&self, scope_id: usize) -> Vec<usize> {
        let Some(origin) = self.scopes.get(scope_id) else {
            return Vec::new();
        };
        let tok = &origin.cancel;
        self.scopes
            .iter()
            .filter(|(_, s)| Arc::ptr_eq(&s.cancel, tok))
            .map(|(i, _)| i)
            .collect()
    }

    /// TICKET-103 — every scope of `scope_id`'s family has finished every task it holds.
    pub(super) fn family_done(&self, scope_id: usize) -> bool {
        self.scope_family(scope_id)
            .iter()
            .all(|&j| self.scopes[j].done == self.scopes[j].total)
    }

    /// TICKET-128 (W13-25) — a scope-scoped owner may stop only when its WHOLE family (the origin
    /// scope plus any TICKET-103 continuation scopes sharing its cancel token) is done, not the
    /// origin alone. A drainer scope-scoped to the origin used to stop while a continuation scope —
    /// opened by `inject_or_extend` when a spawn landed after the origin was no longer the sched's
    /// last scope — still held a parked fiber; at `CHEZZI_THREADS=1` no worker was left to run it and
    /// the joiner slept on an untimed `cv.wait` forever.
    pub(super) fn owner_scope_done(&self, scope_id: usize) -> bool {
        !self.scopes[scope_id].body_open && self.family_done(scope_id)
    }

    /// TICKET-103 — some owner blocked at a same-sched join is joining a family whose every scope is
    /// complete. That owner resumes on its next pass (see `MnSched::quiesced_core`).
    pub(super) fn any_joined_family_done(&self) -> bool {
        self.scopes
            .iter()
            .any(|(i, s)| s.joins_blocked > 0 && self.family_done(i))
    }

    /// TICKET-103 (W12-4) — the deadlock flag, faulting only the parked fibers of JOINED LEAF
    /// families. Returns `true` when it fell back to the sched-wide [`SchedCore::flag_deadlock`]
    /// (which latches `terminate` unless a demoted fiber is left), `false` when it faulted leaves
    /// only and the sched keeps running.
    ///
    /// A join-blocked owner feeds by CHANNEL, not by family, so a parked fiber outside the family it
    /// joins may be its next receiver once its `recover:` returns (`cousin_fed`). A *joined* family
    /// (one with `joins_blocked > 0`) is a *leaf* when none of its scopes holds a join-blocked owner
    /// (`owners_blocked`). A joined leaf's fibers have no feeder left: its owner waits on them, and
    /// no member waits on a join. With no parked fiber in ANY joined leaf, the sched keeps today's
    /// sched-wide flag.
    ///
    /// TICKET-125 (W13-5) — with two OR MORE parked leaves, faulting all of them at once can be
    /// wrong: one leaf's owner may feed a COUSIN leaf once its `recover:` returns
    /// (`leaf_cousin`/`d2a`), so faulting the cousin too drops a live receiver. So it faults the
    /// single lowest-index parked leaf alone, and ONLY when `unproven_ok` licenses it (TICKET-129 —
    /// [`MnSched::may_fault_unproven`] is that license: a private sched judging its own leaf in
    /// isolation cannot tell "genuinely stuck" from "a cousin on another sched is about to feed
    /// me", so a fault waits for that peer to either move or itself go quiet, matching Go's
    /// all-goroutines-parked rule instead of a per-subgroup guess). The next verdict judges the next
    /// leaf.
    ///
    /// M3.2 fix 1 — there is no holder-count proof any more: a channel slot can be shared by two
    /// tasks (a module global, a value received by handle), and one slot holds one `Arc`, so no
    /// count of `Arc` holders shows that no other task holds the channel.
    ///
    /// Returns `None` when `unproven_ok` is `false` — the caller must decline (wait, never spin)
    /// rather than fault. Otherwise returns `Some(true)` when it fell back to the sched-wide
    /// [`SchedCore::flag_deadlock`] and that latched `terminate`, `Some(false)` when it faulted
    /// leaves only and the sched keeps running.
    pub(super) fn flag_deadlock_leaves(
        &mut self,
        err: &RuntimeError,
        unproven_ok: bool,
    ) -> Option<bool> {
        if !unproven_ok {
            return None;
        }
        let candidates = self.leaf_candidates();
        if candidates.is_empty() {
            return Some(self.flag_deadlock(err));
        }
        let target: std::collections::HashSet<usize> = candidates[0].iter().copied().collect();
        let buckets: Vec<(usize, Vec<ParkedEntry>)> = self.parked.drain().collect();
        let mut flagged = 0usize;
        for (key, v) in buckets {
            let mut keep: Vec<ParkedEntry> = Vec::new();
            for entry in v {
                let fiber = match entry {
                    ParkedEntry::Recv(f) | ParkedEntry::Send(f) if target.contains(&f.scope_id) => {
                        Some(f)
                    }
                    ParkedEntry::Wait(wp) => {
                        if wp.claimed.load(Ordering::Acquire) {
                            continue; // a stale copy of a claimed token: drop it
                        }
                        match Self::peek_wait_scope(&wp) {
                            None => continue, // already taken: drop the stale copy
                            Some(s) if !target.contains(&s) => {
                                keep.push(ParkedEntry::Wait(wp));
                                continue;
                            }
                            Some(_) => {
                                if wp
                                    .claimed
                                    .compare_exchange(
                                        false,
                                        true,
                                        Ordering::AcqRel,
                                        Ordering::Acquire,
                                    )
                                    .is_err()
                                {
                                    continue;
                                }
                                wp.fiber.lock().unwrap_or_else(|e| e.into_inner()).take()
                            }
                        }
                    }
                    other @ ParkedEntry::Recv(_) | other @ ParkedEntry::Send(_) => {
                        keep.push(other); // outside every joined leaf: stays parked
                        continue;
                    }
                };
                if let Some(f) = fiber {
                    self.record_deadlocked(f, err);
                    flagged += 1;
                }
            }
            if !keep.is_empty() {
                self.parked.insert(key, keep);
            }
        }
        self.parked_n -= flagged;
        Some(false)
    }

    /// Peek a `Wait` fiber's scope under its mutex without claiming, as `cancel_drain` does. Shared by
    /// [`SchedCore::leaf_candidates`] and the flagging loop in [`SchedCore::flag_deadlock_leaves`].
    fn peek_wait_scope(wp: &Arc<WaitPark>) -> Option<usize> {
        wp.fiber
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|f| f.scope_id)
    }

    /// TICKET-129 (W13-5 residual) — the distinct joined-leaf families with at least one parked
    /// member, ordered by each family's lowest scope index ([`SchedCore::flag_deadlock_leaves`]).
    fn leaf_candidates(&self) -> Vec<Vec<usize>> {
        // Distinct joined-leaf families, keyed (and so de-duplicated + ordered) by each family's
        // lowest scope index.
        let mut families: std::collections::BTreeMap<usize, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (i, si) in self.scopes.iter() {
            if si.joins_blocked == 0 {
                continue;
            }
            let fam = self.scope_family(i);
            if fam.iter().any(|&j| self.scopes[j].owners_blocked > 0) {
                continue; // interior: a member's owner may feed it once its own join returns
            }
            // TICKET-125 (W13-4) — a member fiber that owns a still-incomplete NESTED nursery is
            // also interior: faulting that member drops it without unwinding its child scope, which
            // orphans the child and hangs the run. `owners_blocked` alone misses this: it only
            // counts JOIN-parked owners, not one parked on a channel inside the child's own body.
            if self.scopes.iter().any(|(k, s)| {
                !fam.contains(&k)
                    && s.done < s.total
                    && s.parent_scope.is_some_and(|p| fam.contains(&p))
            }) {
                continue;
            }
            let lowest = *fam.iter().min().expect("scope_family never returns empty");
            families.entry(lowest).or_insert(fam);
        }
        let parked_scopes: std::collections::HashSet<usize> = self
            .parked
            .values()
            .flatten()
            .filter_map(|e| match e {
                ParkedEntry::Recv(f) | ParkedEntry::Send(f) => Some(f.scope_id),
                ParkedEntry::Wait(wp) => (!wp.claimed.load(Ordering::Acquire))
                    .then(|| Self::peek_wait_scope(wp))
                    .flatten(),
            })
            .collect();
        families
            .into_values()
            .filter(|fam| fam.iter().any(|s| parked_scopes.contains(s)))
            .collect()
    }
}

/// B3.3-threads — turn a caught panic payload (from `catch_unwind` around a worker task) into a
/// `RuntimeError` so a Rust panic inside a `--parallel` task surfaces as an ordinary task fault
/// (joined + reported) instead of hanging the join or aborting the process opaquely.
pub(super) fn panic_to_fault(payload: Box<dyn std::any::Any + Send>, span: Span) -> RuntimeError {
    let msg = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    RuntimeError {
        message: format!("internal error: a parallel task panicked: {msg}"),
        span,
        is_assert: false,
        is_over_memory: false,
        is_timed_out: false,
        is_deadlock: false,
        is_panic: false,
        blocked: None,
    }
}

/// B3.3-threads — a worker `Vm` with its task's callee and args (the spawner's values, by handle,
/// M3.4), ready to run on whatever thread owns it. [`Vm::prepare_worker`] builds this (the
/// spawner-thread half of `run_task_isolated`); [`ReadyWorker::run`] is the thread-side half
/// (invoke and hand the result back, test only). Splitting the two lets the bounded pool prepare a task on the parent thread, then **move**
/// the whole `ReadyWorker` (it is `Send` — `Vm` is `Send`, `Value`/`String` are `Send`) onto a pool
/// thread to execute. Single-thread `run_task_isolated` = `prepare_worker(task)?.run()`.
pub(super) struct ReadyWorker {
    pub(super) worker: Vm,
    pub(super) call: ReadyCall,
    pub(super) span: Span,
    /// M1.7 — the callee and args are in no root set until `into_fiber` (`vm/gc_door.rs`).
    pub(super) payload: super::gc_door::PayloadInFlight,
}

/// What [`ReadyWorker::run`] hands a worker unit test: the task's return value (a handle in the
/// run's one heap, unrooted: read it before the next safepoint) and its captured output. A real
/// task returns no value to its join ([`WorkerResult`]).
#[cfg(test)]
pub(super) struct IsolatedRun {
    pub(super) value: Value,
    pub(super) out: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

/// How a [`ReadyWorker`]'s task is invoked: the spawner's values, by handle (M3.4).
pub(super) enum ReadyCall {
    /// A `spawn f(x)` / `spawn:` block — invoke the callable with its args.
    Invoke { callee: Value, args: Vec<Value> },
}

impl ReadyWorker {
    /// Run the prepared task to completion **on the current thread** and hand back its return value
    /// plus the worker's captured `out`/`stderr` (decision F). A method
    /// task that blocks on `recv` would leave no result on the stack; under `--parallel` the blocking
    /// `recv` instead waits on the channel condvar (it never returns the suspend sentinel), so the
    /// `suspend` guard is a safety net against a future regression, not a live path.
    #[cfg(test)]
    pub(super) fn run(mut self) -> Result<IsolatedRun, RuntimeError> {
        let span = self.span;
        let value = Self::invoke(&mut self.worker, self.call, span)?;
        Ok(IsolatedRun {
            value,
            out: self.worker.out,
            stderr: self.worker.stderr,
        })
    }

    #[cfg(test)]
    /// Invoke the prepared task on the worker VM, leaving its return value on the stack popped into
    /// `ret`. Borrows `worker` and consumes `call` (disjoint fields), so the caller keeps `worker`
    /// afterward to inspect `pending_exit`/`cancelled` (B3.4).
    fn invoke(worker: &mut Vm, call: ReadyCall, span: Span) -> Result<Value, RuntimeError> {
        match call {
            ReadyCall::Invoke { callee, args } => worker.invoke_value(callee, args, span),
        }
    }

    /// D2b — deconstruct a prepared worker into a lightweight [`Fiber`] for the M:N engine: its
    /// module roots and executors become the fiber's `FiberCtx` (state that travels
    /// together — see [`FiberCtx`]), and its `ReadyCall` becomes a `Pending` task `start_task`
    /// launches on first schedule. The worker `Vm` is discarded with its heap handle (its objects
    /// are in the run's one heap, rooted through the registered context from here on): under M:N a
    /// fiber runs on a shared host shell, not its own `Vm`.
    pub(super) fn into_fiber(self, task_index: usize, scope_id: usize) -> Fiber {
        let ReadyWorker {
            mut worker,
            call,
            span,
            payload,
        } = self;
        let task = match call {
            ReadyCall::Invoke { callee, args } => PendingCall::Call { callee, args, span },
        };
        let registry = worker.root_registry.clone();
        let ctx = FiberCtx {
            // M1.4 — the task waits in the context so the context alone roots it.
            start: Some(task),
            owns_side: true,
            module_objs: worker.module_objs,
            executors: worker.executors,
            // M19 Phase 3 — the intern cache holds the worker's own strings; they are this fiber's.
            str_intern: worker.str_intern,
            ..FiberCtx::default()
        };
        debug_assert!(ctx.start.is_some(), "a Pending fiber holds its task");
        let ctx = registry.register(ctx);
        // The payload is rooted by the registered context from here on. Settle the payload `Vm`'s
        // `write` record (its fill of a rebuilt container) while the spawner is attached.
        worker.heap.settle_pending_mut();
        drop(payload);
        Fiber {
            ctx,
            state: FiberState::Pending,
            task_index,
            scope_id,
            span,
            resume_native: None,
            pending: None,
            fault_park: None,
            park_site: None,
            park_deadline: None,
            job_cancel: None,
        }
    }
}
