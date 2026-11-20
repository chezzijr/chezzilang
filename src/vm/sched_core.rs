//! VM: `SchedCore` verdicts, jobs and families, `panic_to_fault`, the ready worker.
//! Split out of `src/vm/mod.rs`. `super::*` is the parent module.

use super::*;

impl SchedCore {
    /// The run's deadlock verdict on this sched, step 1 ([`MnSched::on_deadlock`]): every parked
    /// and join-parked fiber leaves its wait, each counted awake again. Every sched of the run
    /// takes its fibers out before any is ended ([`SchedCore::end_asleep`]), so the end of one
    /// (a job a fiber joins) wakes no fiber the verdict ends.
    pub(super) fn take_asleep(&mut self) -> Vec<Fiber> {
        let mut out = Vec::new();
        for (_, v) in self.parked.drain() {
            for entry in v {
                match entry {
                    ParkedEntry::Recv(f)
                    | ParkedEntry::Send(f)
                    | ParkedEntry::Guard(f)
                    | ParkedEntry::ExecJoin(f, ..) => out.push(f),
                    // One fiber in N buckets: the claim takes it once.
                    ParkedEntry::Wait(wp) => {
                        if wp
                            .claimed
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                            && let Some(f) =
                                wp.fiber.lock().unwrap_or_else(|e| e.into_inner()).take()
                        {
                            out.push(f);
                        }
                    }
                }
            }
        }
        for (_, f) in std::mem::take(&mut self.join_parked) {
            out.push(f);
        }
        self.parked_n = 0;
        self.awake.woken(out.len());
        out
    }

    /// Step 2 of the run's verdict on this sched: each fiber [`SchedCore::take_asleep`] took ends
    /// `Deadlocked` with `err`, listed at its wait, its guards cut, no `defer` run
    /// ([`SchedCore::record_deadlocked`]). Latches `terminate` unless a demoted fiber is still
    /// registered.
    pub(super) fn end_asleep(&mut self, fibers: Vec<Fiber>, err: &RuntimeError, verdict: u64) {
        for f in fibers {
            self.record_deadlocked(f, err, verdict);
        }
        self.verdict_cut = true;
        self.terminate = !self.any_fiber_waiter();
    }

    /// TICKET-296: the sched is torn down, or the run's verdict ended its tasks
    /// ([`SchedCore::end_asleep`]). Read through [`Vm::in_place_cut`].
    pub(super) fn cut_in_place(&self) -> bool {
        self.terminate || self.verdict_cut
    }

    /// Store one claimed parked fiber's `Deadlocked` outcome and bump its scope's `done`
    /// ([`SchedCore::end_asleep`]).
    fn record_deadlocked(&mut self, f: Fiber, err: &RuntimeError, verdict: u64) {
        // Carry the parked fiber's OWN buffered stdout/stderr into its Deadlocked slot (not an empty
        // buffer). `swap_task` moved this fiber's live prints into `f.ctx.out` when it parked; the
        // downstream `reduce_task_slots` flushes EVERY Deadlocked slot's buffer at its task-order slot
        // (not just the lowest-index one, as with a real Fault), so with two-or-more parked fibers a
        // higher-index printer's output is preserved byte-identically to a strictly sequential run.
        // `task_index`/`scope_id` are Copy, read before `f.ctx` moves out.
        let (ti, sid) = (f.task_index, f.scope_id);
        // A scope with its own `deadlock_err` (a fiber-owned nursery's, set by
        // `Vm::activate_fiber_owned_nursery`) reports ITS span instead of the sched-wide `err` —
        // see `JoinScope::deadlock_err` and DEC-048.
        // TICKET-208 — else, on an `Executor`'s sched, the fiber's own blocking op: the text and
        // span the in-place path raises there. Else the sched-wide `err`.
        // Each task names its own wait: its park site, else its scope's own fault (an owner
        // parked at a join names no site), else the sched-wide `err`.
        // M1.4 — take the context out of the registry before any field moves: this thread may not
        // be attached to the GC (see `RootRegistry::roots`).
        let mut ctx = f.ctx.into_inner();
        // The verdict cut this holder: the guards its frames hold (innermost last) are freed as
        // cut, and no acquire takes the box again (`core::UpdateGuard::mark_cut`).
        super::core::cut_all(&mut ctx.held_guards);
        ctx.held_guards.clear();
        // A fiber parked at a wait is one entry of the run's listing, with its frames; an owner
        // parked at a join is not (its blocked children stand in its place).
        let scope_err = match (f.park_site, &self.scopes[sid].deadlock_err) {
            (Some((msg, span)), _) => {
                let frames = super::frames::raw_frames(&ctx.frames);
                self.listing.cut(err, &ctx.task, msg, span, frames)
            }
            (None, Some(e)) => e.clone(),
            (None, None) => err.clone(),
        };
        // A task stuck in a `defer` of its `--max-heap` abort stays over memory.
        let scope_err = if ctx.drains.iter().any(super::Drain::over_memory) {
            scope_err.over_memory()
        } else {
            scope_err
        };
        self.slots[ti] = Some(TaskOutcome::Deadlocked {
            err: scope_err,
            out: ctx.out,
            stderr: ctx.stderr,
            verdict,
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
                // A held job is alive but asleep: only a job's end releases it. The submitter is
                // awake, so this never reads as every task asleep.
                self.exec_held.push_back(fiber);
                self.awake.asleep();
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
                // `end_asleep` latches `terminate` (or leaves a demoted fiber to settle), so a
                // released job would never run and its `None` slot would hang the join; the parked
                // job's `Deadlocked` is the report.
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
            self.awake.woken(1);
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
            self.awake.woken(1); // a held job wakes to end
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
                self.awake.woken(1); // a held job wakes to end
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
        is_panic: false,
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
            task: worker.task,
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
            timer_wait: None,
            job_cancel: None,
            cut: None,
            cancel_unwind_faulted: false,
        }
    }
}
