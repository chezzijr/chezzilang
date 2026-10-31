//! VM: worker VMs, Executor joins and the worker home (a worker shares the run's module objects).
//! Split out of `src/vm/sched.rs`. `super::*` is the parent module.

use super::sched::poke_live_scheds;
use super::*;

impl Vm {
    /// B3.2 — construct a fresh worker `Vm` that shares this VM's compiled program by `Arc`
    /// (read-only). M1.7: it holds a handle on this run's ONE heap core (`Heap::attach`), shared
    /// the way `exec_registry` is shared below, so every `Vm` of a run (a task's payload `Vm`, a
    /// runner or joiner shell, a box context) allocates into one heap that one collector sweeps.
    /// Execution-shaping flags (`gc_stress`) carry over so a worker is exercised under the same GC
    /// pressure as the parent. No OS thread yet; the caller drives the returned worker
    /// synchronously.
    ///
    /// `chezzi test --max-heap` needs nothing here: the cap and its verdict live in the shared
    /// core, so a task's growth is part of the run's `live_bytes`, and every sweep, whichever
    /// thread runs it, measures the whole run. Two mechanisms sample the cap for a task:
    ///
    ///   1. [`Vm::sample_mem_cap`], in [`Vm::start_task`], before dispatch: a full collection of
    ///      the run under a live cap. It was added for a task body running NO bytecode
    ///      (`spawn blob.len()`, W6-10s residual (a)), whose heap held a rebuilt copy of its
    ///      payload. M3.4 removed that copy (`a_spawned_payload_is_the_producers_and_costs_no_heap`);
    ///      removing the sample still needs its own measurement.
    ///   2. The byte pacing: what a task allocates is charged by `alloc` to the handle of the `Vm`
    ///      that runs it, and the handle adds its counts to the run's trigger when it drops
    ///      (`Heap`'s `Drop`). The per-worker `request_collect` that was here forced a collection at
    ///      the task's first instruction, the same point as (1); with one heap (1) samples the run
    ///      there already, so it went.
    ///
    /// WHAT NEITHER COVERS: growth AFTER the sample is paced, not bounded (`Heap::write`'s settle
    /// charges the growth of the object the writing handle armed; since M3.4 another task's
    /// handle can grow that same object, so the charge lands on whichever handle armed it). Do not
    /// restate any of the above as an unconditional guarantee.
    pub(super) fn spawn_worker(&self) -> Vm {
        // SAFETY: the gc_coord protocol (`vm/gc_coord.rs`): the worker joins this run's `GcCoord`
        // (below) and attaches before it runs code on its own thread (`Vm::worker_loop_held`).
        // Since M3.4 the worker writes objects other tasks hold (the spawner's args and captured
        // cells, the run's module globals since M3.1), so "a worker writes only what it built" no
        // longer holds. Two facts keep a write sound: every heap slot write is atomic and no
        // `&mut Obj` exists across a safepoint (M3.0), so two tasks racing on one object is a
        // user-level race, never UB; and a container buffer a write outgrows is retired, freed
        // only by the stop-the-world collector (M2), so a reader on another thread never sees a
        // freed buffer.
        let heap = unsafe { self.heap.attach() };
        let mut worker = Vm::with_heap(Arc::clone(&self.program), heap);
        worker.gc_stress = self.gc_stress;

        // `chezzi test --timeout` — thread the SAME absolute deadline onto the worker so a `spawn`/
        // `parallel:` task's loop trips the wall-clock cap on the M:N engine too (a fresh `Vm::new`
        // starts with `deadline = None`). `None` when the cap is off, so the common path is untouched.
        worker.set_deadline(self.deadline);
        // …and the raw `timeout_ms` too — the deadline drives the TRIP, but the abort MESSAGE
        // interpolates `timeout_ms`, so a worker left at the `Vm::new` default `0` would render a
        // spawned-task timeout as "(0ms)" (reads as "off").
        worker.set_timeout(self.timeout_ms);
        // Workers run on the pool too, so a nested `parallel:` inside a task recurses onto threads
        // (and a worker's `recv` blocks on the condvar, not a fiber). B3.3-threads.
        // W7-5b — SHARE (not copy) the run's executor registry, so an `Executor` constructed inside a
        // task is still reachable by the program-exit join after this worker's heap is gone. This is
        // the whole fix: the parallel `Vm.executors` list is heap-keyed and dies with the task.
        worker.exec_registry = Arc::clone(&self.exec_registry);
        // M3.1 — and the run's one module table and fn-value table, so the worker reads the
        // run's module globals, a module that initializes later included.
        worker.module_objs = Arc::clone(&self.module_objs);
        worker.fn_values = Arc::clone(&self.fn_values);
        // …and the blocked-party registry with it, for the same reason: the process-wide deadlock
        // verdict must see every party of THIS run and none of any other's (`future.md` §2d step 0).
        worker.quiesce = Arc::clone(&self.quiesce);
        // W7-56 — and the sched registry, so a wake from a worker that holds NO sched of its own
        // (every eager `Executor` job: this fn sets neither `mn` nor `mn_enlist_sched`) still reaches
        // a fiber parked on one of the run's nurseries. See `Vm::sched_registry`.
        worker.sched_registry = Arc::clone(&self.sched_registry);
        // M1.4 — and the root registry, so every fiber built from this worker is listed in the run's.
        worker.root_registry = self.root_registry.clone();
        // M1.5 — and the stop-the-world coordinator: a thread that drives this worker joins it.
        worker.gc_coord = Arc::clone(&self.gc_coord);
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

    /// B3.2 — run a spawned task in a worker (`spawn_worker`) synchronously on the calling thread:
    /// [`Vm::prepare_worker`] then [`ReadyWorker::run`]. Only the worker unit tests use it; the
    /// engine calls the two halves separately (across the pool boundary).
    #[cfg(test)]
    pub(super) fn run_task_isolated(
        &mut self,
        task: PendingCall,
    ) -> Result<super::sched_core::IsolatedRun, RuntimeError> {
        self.prepare_worker(task)?.run()
    }

    /// B3.3-threads — the spawner-thread half of a task start: build the worker over the run's
    /// module objects and the run's one heap, and hand it the task's callee and args by handle
    /// (M3.4: a task shares the parent's values, as `go f(x)` does in Go), yielding a
    /// [`ReadyWorker`] that can be moved to a pool thread and `run()`. A non-callable callee is the
    /// one fault.
    pub(super) fn prepare_worker(
        &mut self,
        task: PendingCall,
    ) -> Result<ReadyWorker, RuntimeError> {
        let PendingCall::Call { callee, args, span } = task;
        if self.callable(callee).is_none() {
            return Err(self.err(
                format!("spawn: '{}' is not callable", self.type_name(callee)),
                span,
            ));
        }
        // The callee and args are in no root set from here to `into_fiber` (`vm/gc_door.rs`).
        let payload = super::gc_door::PayloadInFlight::new();
        Ok(ReadyWorker {
            worker: self.spawn_worker(),
            call: ReadyCall::Invoke { callee, args },
            span,
            payload,
        })
    }

    /// TICKET-208 — THE Executor join, behind `shutdown`, the exit drain and the
    /// per-test reap: wait until the Executor's sched has no unfinished task but the joiner's own
    /// (`slack`: a job that joins its own Executor cannot wait for itself), then reduce the
    /// submission-ordered slots with [`Vm::reduce_task_slots`].
    ///
    /// The joiner is not an ancestor of the jobs, so its wait reads its halts itself, through
    /// [`Vm::block_halts`] and nowhere else. It never declares a deadlock: the joined sched's idle
    /// runner is the one judge, and a fiber joiner is a registered waiter that judge can read.
    ///
    /// How it leaves:
    /// - done: the scope is taken out of the core and its slots are reduced (`slack == 0` only);
    /// - `--timeout`: the run stopped wanting the jobs, so they are cancelled, the finished ones'
    ///   output is flushed, and the scope is taken so nothing joins it again;
    /// - this party's own `os.exit`: the finished jobs' output is flushed;
    /// - any other cut (a cancel, a delivered child fault): nothing is touched. The jobs keep
    ///   running and the scope stays for a later `shutdown()` or the exit drain.
    pub(super) fn join_executor(
        &mut self,
        core: &Arc<ExecutorCore>,
        span: Span,
    ) -> Result<(), RuntimeError> {
        use crate::vm::quiesce::PartyWait;
        let Some(sched) = core.sched() else {
            return Ok(());
        };
        sched.close_body(0);
        sched.latch_terminate_if_done();
        let slack = usize::from(self.owns_nested_sched(&sched));
        let mut reg = self.wait_begin(
            WaitSpec::Join,
            Some(PartyWait::Join(Arc::clone(&sched), slack)),
            "an Executor join",
            span,
        )?;
        let party = self.block_party_guard(PartyWait::Join(Arc::clone(&sched), slack), None);
        let leave = loop {
            if sched.lock().join_over(slack) {
                break None;
            }
            if let Err(e) = self.block_halts(span) {
                break Some(e);
            }
            if let Some(m) = self.mn.clone()
                && m.lock().cut_in_place()
            {
                break Some(m.deadlock_err.clone());
            }
            self.wait_door(&[], |_| sched.join_tick(slack, DEMOTE_POLL_BACKOFF));
        };
        if let Some(m) = self.mn.clone() {
            reg.release(&m, &mut m.lock());
        }
        drop(party);
        self.wait_end(reg, &[]);
        let take_scope =
            |core: &ExecutorCore| core.scope.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(e) = leave else {
            if slack > 0 {
                return Ok(());
            }
            // `None`: another join reduced it already.
            let Some(scope) = take_scope(core) else {
                return Ok(());
            };
            // `shutdown()` raises no job fault (TICKET-289): an ordinary job fault, and the `defer`
            // fault of a cancelled job, are the job's `Task.get()` outcome, so the slot reduces as
            // a cancelled job. A hard halt (`--max-heap`, `--timeout`), a deadlock and an exit still
            // raise: a hard abort is never a Task outcome.
            let cancelled = core.cancel.load(Ordering::Relaxed);
            let slots = scope
                .sched
                .take_slots()
                .into_iter()
                .map(|s| match s {
                    Some(
                        TaskOutcome::Fault {
                            err, out, stderr, ..
                        }
                        | TaskOutcome::CancelledFault {
                            err, out, stderr, ..
                        },
                    ) if !executor_hard_halt(&err) && !err.is_deadlock => {
                        Some(TaskOutcome::Cancelled { out, stderr })
                    }
                    // Cancel outranks a deadlock verdict (DEC-135): a job the verdict ended after
                    // `cancel()` cancelled it is a cancelled job. It is parked in its own
                    // nursery join there, behind a child whose `defer` blocks, so no cut reaches it.
                    Some(TaskOutcome::Deadlocked { out, stderr, .. }) if cancelled => {
                        Some(TaskOutcome::Cancelled { out, stderr })
                    }
                    // Every other slot keeps its outcome: an unfilled slot, a value, a hard-halt or
                    // deadlock fault, a cancel, an exit, a deadlock with no cancel.
                    s @ (None
                    | Some(
                        TaskOutcome::Done(_)
                        | TaskOutcome::Fault { .. }
                        | TaskOutcome::CancelledFault { .. }
                        | TaskOutcome::Cancelled { .. }
                        | TaskOutcome::Exit { .. }
                        | TaskOutcome::Deadlocked { .. },
                    )) => s,
                })
                .collect();
            return self.reduce_task_slots(slots);
        };
        let flush = self.join_leave_flushes(&e);
        if e.is_timed_out {
            crate::vm::trip_cancel_flag(&core.cancel);
            sched.drain_family(0);
            poke_live_scheds(&self.sched_registry);
        }
        if flush {
            // TICKET-284 — test-only: a job takes the joined Executor's output after the run-end pass
            // read the job's own Executor (see `HOLD_RUN_END_PASS`).
            #[cfg(test)]
            if crate::vm::HOLD_RUN_END_PASS.load(Ordering::Relaxed) && self.fiber_scope.is_some() {
                self.wait_door(&[], |_| {
                    std::thread::sleep(std::time::Duration::from_millis(900))
                });
            }
            self.take_job_streams(&sched);
        }
        if e.is_timed_out && slack == 0 {
            drop(take_scope(core));
        }
        Err(e)
    }

    /// Does a join that left with `e` take the finished jobs' output? A `--timeout`, this party's
    /// own `os.exit` and the run-wide job fault (`Cut::RunFault`) end the run, so finished output
    /// is the last it prints. A cancel or a delivered child fault leaves the jobs running and takes
    /// nothing (DEC-208). `join_executor` and the exit drain both read this one rule.
    pub(super) fn join_leave_flushes(&self, e: &RuntimeError) -> bool {
        e.is_timed_out
            || self.pending_exit.is_some()
            || matches!(self.cut, Some(crate::vm::block::Cut::RunFault))
    }

    /// TICKET-270 — the run ended: take every live executor's finished output, in creation order
    /// from registry index `from`. A job that ended the run in a later executor than the one being
    /// joined holds its output only in its slot. `take_finished_streams` waits for every exiting
    /// fiber's slot, then takes the core lock, under which `MnSched::publish_job_fault` stored a job
    /// fault's cell before the slot; so the slot of the job that ended the run is filled here.
    /// TICKET-284 — a job of an Executor this pass already read can take a later Executor's exit
    /// output and store it in its own slot. So a pass is valid only when no exit count began during
    /// it ([`QuiesceState::exit_origins_begun`]); else it repeats. A pass that drains exit output
    /// begins counts itself, so one more pass follows and drains nothing. Each pass re-reads the
    /// registry, which only grows (DEC-067).
    pub(super) fn flush_finished_jobs_from(&mut self, from: usize) {
        loop {
            let begun = self.quiesce.exit_origins_begun();
            let live: Vec<_> = self
                .exec_registry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .skip(from)
                .filter_map(|c| c.sched())
                .collect();
            for sched in live {
                self.take_job_streams(&sched);
                // TICKET-284 — test-only: `main` reads the next Executor late (see `HOLD_RUN_END_PASS`).
                #[cfg(test)]
                if crate::vm::HOLD_RUN_END_PASS.load(Ordering::Relaxed)
                    && self.fiber_scope.is_none()
                {
                    self.wait_door(&[], |_| {
                        std::thread::sleep(std::time::Duration::from_millis(600))
                    });
                }
            }
            if self.quiesce.exit_origins_begun() == begun {
                break;
            }
        }
    }

    /// TICKET-284 — move one sched's finished output into this party's buffers, and take over the
    /// count of the exit output it moved ([`Vm::adopt_exit_output`]).
    fn take_job_streams(&mut self, sched: &MnSched) {
        let (out, stderr, exits) = self.wait_door(&[], |_| sched.take_finished_streams());
        self.out.extend_from_slice(&out);
        self.stderr.extend_from_slice(&stderr);
        self.adopt_exit_output(exits);
    }
}
