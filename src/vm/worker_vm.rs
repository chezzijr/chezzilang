//! VM: worker VMs, Executor joins and the worker home (a worker shares the run's module objects).
//! Split out of `src/vm/sched.rs`. `super::*` is the parent module.

use super::airlock_out::WireMemo;
use super::airlock_out::ensure_crossable_wire;
use super::sched::poke_live_scheds;
use super::*;
use crate::base::crossing::{self, Route};

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
    ///      (`spawn blob.len()`, W6-10s residual (a)). Removing it needs its own measurement
    ///      against `over_memory_trips_on_an_all_native_task_body`.
    ///   2. The byte pacing: the payload a task's arguments rebuild into is charged by `alloc` to
    ///      the worker's handle, and the handle adds its counts to the run's trigger when it drops
    ///      (`Heap`'s `Drop`). The per-worker `request_collect` that was here forced a collection at
    ///      the task's first instruction, the same point as (1); with one heap (1) samples the run
    ///      there already, so it went.
    ///
    /// WHAT NEITHER COVERS: growth AFTER the sample is paced, not bounded (`Heap::write`'s settle
    /// charges it). Do not restate any of the above as an unconditional guarantee.
    pub(super) fn spawn_worker(&self) -> Vm {
        // SAFETY: the gc_coord protocol (`vm/gc_coord.rs`): the worker joins this run's `GcCoord`
        // (below) and attaches before it runs code on its own thread (`Vm::worker_loop_held`); a
        // payload `Vm` allocates only on the spawner's attached thread between two of its
        // safepoints. M3.1: the worker shares the run's module objects (`prepare_worker`), so it
        // writes a module global in place, as a goroutine writes a Go `var`.
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

    /// B3.2 — run a spawned task in an isolated worker (`spawn_worker`): its args/captures cross IN
    /// as [`WireValue`] (serialized in *this* parent heap, reconstructed in the worker heap) and
    /// its return value + captured `out`/`stderr` cross back OUT. The worker runs **synchronously**
    /// on the calling thread (no OS thread until B3.3), proving the `Arc<Program>` + heap-handoff
    /// plumbing in isolation.
    ///
    /// The callee is **not** crossed as a parent-heap `Handle` (a `GcRef` is meaningless in another
    /// heap); instead the task is lowered to its `ProtoId` + wire'd captures (the proto lives in
    /// the shared `Arc<Program>`) and the worker rebuilds the closure over its own heap.
    ///
    /// **Cross-heap safety (enforced, not just documented).** A `WireValue` that still carries a
    /// by-reference [`Handle`](WireValue::has_handle) — a `str`, a closure/func value, a module —
    /// is a parent-heap `GcRef` that means nothing in the worker heap, so every crossed value
    /// (captures, args, and the returned result) is checked with [`Vm::ensure_crossable`] and a
    /// clean `RuntimeError` is raised rather than silently reconstructing a dangling handle. Plain
    /// data and `Channel`/`Shared`/`Executor` handles (which cross as a shared `Arc`, not a
    /// `GcRef`) pass. `str`/closure crossing **by value** lands in B3.3.
    ///
    /// M3.1: the worker's modules are the spawner's module objects ([`Vm::prepare_worker`]), so a
    /// task reads and writes the live module globals and calls sibling/imported fns through them.
    /// The synchronous single-thread driver: prepare + run on the calling thread. The `--parallel`
    /// engine calls `prepare_worker` and `ReadyWorker::run` separately (across the pool boundary),
    /// so this convenience wrapper is now only used by the B3.2–B3.3d worker unit tests.
    #[cfg(test)]
    pub(super) fn run_task_isolated(
        &mut self,
        task: PendingCall,
    ) -> Result<WorkerResult, RuntimeError> {
        self.prepare_worker(task, 0)?.run()
    }

    /// B3.3-threads — the parent-thread half of [`Vm::run_task_isolated`]: lower the task to a
    /// `Send` description against THIS heap, build the worker over the spawner's module objects,
    /// and rebuild the callee/receiver + args **into the worker heap**, yielding a [`ReadyWorker`]
    /// that can be moved to a pool thread and `run()`. Everything that reads the parent heap
    /// happens here; nothing in `ReadyWorker::run` touches `self`.
    pub(super) fn prepare_worker(
        &mut self,
        task: PendingCall,
        fresh: u32,
    ) -> Result<ReadyWorker, RuntimeError> {
        // 1. Lower the task to a `Send` description in THIS (parent) heap (read-only serialize),
        //    rejecting any value that can't cross a heap boundary as-is.
        let lowered = self.lower_task(task)?;
        // 2. Build the worker over the run's one module table (M3.1, `Vm::spawn_worker`), so every
        //    task of the run reads and writes one copy of each module global, as in Go, a module
        //    that initializes after this spawn included. 3. Rebuild the callable/receiver + args
        //    into the worker heap. The actual invoke is `ReadyWorker::run`.
        let payload = super::gc_door::PayloadInFlight::new();
        let mut worker = self.spawn_worker();
        let (call, span) = worker.rebuild_ready(lowered, fresh);
        Ok(ReadyWorker {
            worker,
            call,
            span,
            payload,
        })
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
    pub(super) fn lower_task(&mut self, task: PendingCall) -> Result<Lowered, RuntimeError> {
        let mut memo = WireMemo::default();
        let lowered = match task {
            PendingCall::Call { callee, args, span } => {
                let wargs = self.wire_args(&args, span, &mut memo)?;
                match self.callable(callee) {
                    Some(Callee::Closure { proto, home }) => {
                        let h = callee.as_obj().expect("a closure is an object");
                        let Obj::Closure { captured, .. } = self.heap.get(h) else {
                            unreachable!("callable classified a closure")
                        };
                        let captured = captured.clone();
                        // Lever #3: captures are positional; carry names from the proto in slot
                        // order so the wire format (Vec<(name, value)>) is unchanged. Its globals
                        // are the run's: the worker shares the spawner's module objects.
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
                    // TICKET-226: every other callable kind crosses by value, AFTER the args (the
                    // order `rebuild_ready` rebuilds in); `from_wire_memo` lands it in the
                    // worker heap's one `fn_value` slot.
                    Some(_) => {
                        let w = self.to_wire_memo_at(callee, span, &mut memo)?;
                        self.ensure_crossable(&w, span)?;
                        Lowered::Value {
                            callee: w,
                            args: wargs,
                            span,
                        }
                    }
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
        };
        Ok(lowered)
    }

    /// PHASE 3 of task preparation — rebuild a [`Lowered`] task's callable/receiver + args INTO
    /// THIS VM's heap, resolving home indices against `self.module_objs` (the run's module
    /// table). Split out of [`Vm::prepare_worker`]. Infallible (all crossing checks happened in
    /// [`Vm::lower_task`]).
    ///
    /// W7-4: ONE rebuild map spans the whole `Lowered`, mirroring `lower_task`'s single
    /// [`WireMemo`] and reconstructing in the SAME order it serialized (a `Call`'s args before the
    /// callee's captures), so a cell shared between an arg and a capture is rebuilt once and both
    /// references tie to it — and no `Backref` is ever reached before the `WireValue::Cell` that
    /// defines it.
    ///
    /// D4 (TICKET-240): `fresh` names the checker's shape for each argument ([`crossing::operand`];
    /// a receiver is arg 0 of its entry thunk, TICKET-235). After the rebuild [`Vm::unmark_fresh`]
    /// unmarks exactly the objects each shape says the call site built.
    pub(super) fn rebuild_ready(&mut self, lowered: Lowered, fresh: u32) -> (ReadyCall, Span) {
        // D4 layer C (TICKET-169): everything this call rebuilds — a spawned callee's captures, a
        // `spawn f(args)`'s arguments, a `spawn obj.m()`'s receiver — follows `Route::Spawn`.
        let saved_copy_mark = self.copy_mark;
        self.copy_mark = crossing::marks(Route::Spawn, saved_copy_mark);
        let rb = &mut super::fxhash::FxHashMap::default();
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
                    captured: cap.into(),
                    home,
                }));
                (ReadyCall::Invoke { callee, args }, span)
            }
            Lowered::Value { callee, args, span } => {
                // ARGS FIRST — `lower_task` serializes them before the callee.
                let args = self.rebuild_items(args, rb, |w| w);
                let callee = self.from_wire_memo(callee, rb);
                (ReadyCall::Invoke { callee, args }, span)
            }
        };
        self.copy_mark = saved_copy_mark;
        if fresh != 0 {
            let program = self.program.clone();
            let ReadyCall::Invoke { args, .. } = &out.0;
            for (i, &arg) in args.iter().enumerate() {
                self.unmark_fresh(arg, crossing::operand(&program.fresh_calls, fresh, i));
            }
        }
        out
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
        self.width_release();
        let reg = if self.block_mode(WaitSpec::Join) == BlockMode::Demote {
            let wait = PartyWait::Join(Arc::clone(&sched), slack);
            match self.block_enter(WaitSpec::Join, Some(wait), "an Executor join", span) {
                Ok(reg) => Some(reg),
                Err(e) => {
                    self.width_acquire();
                    return Err(e);
                }
            }
        } else {
            None
        };
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
            self.gc_blocking(&[], |_| sched.join_tick(slack, DEMOTE_POLL_BACKOFF));
        };
        if let (Some(reg), Some(m)) = (reg, self.mn.clone()) {
            reg.release(&m, &mut m.lock());
        }
        drop(party);
        self.width_acquire();
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
                self.gc_blocking(&[], |_| {
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
                    self.gc_blocking(&[], |_| {
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
        let (out, stderr, exits) = self.gc_blocking(&[], |_| sched.take_finished_streams());
        self.out.extend_from_slice(&out);
        self.stderr.extend_from_slice(&stderr);
        self.adopt_exit_output(exits);
    }

    /// Reject a wired value that still carries a by-reference [`Handle`](WireValue::has_handle) — a
    /// heap-local `GcRef` that cannot cross into another heap as-is. As of B3.3 plain data, `str`/bytes,
    /// closures/functions, native/FFI fns, and the `Channel`/`Shared`/`Executor`/socket handles all
    /// cross by value (or as a shared `Arc` core), so the only remaining non-crossable value is a
    /// module handle — a module's mutable globals can't be shared across an OS-thread heap boundary.
    /// (Source-unreachable: `module` is not a nameable type, so this is a defensive-only guard.)
    pub(super) fn ensure_crossable(&self, w: &WireValue, span: Span) -> Result<(), RuntimeError> {
        ensure_crossable_wire(w, span)
    }

    /// Serialize a task's argument list across the airlock (read-only walk of this heap), rejecting any
    /// argument that can't cross a heap boundary as-is (see [`Vm::ensure_crossable`]). W7-4: the caller
    /// supplies the [`WireMemo`], because the args share one serialization with the callee's captures
    /// (and the matching rebuild map in [`rebuild_ready`](Vm::rebuild_ready)) — see
    /// [`deep_clone_all`](Vm::deep_clone_all) for the scope invariant.
    pub(super) fn wire_args(
        &self,
        args: &[Value],
        span: Span,
        memo: &mut WireMemo,
    ) -> Result<Vec<WireValue>, RuntimeError> {
        args.iter()
            .map(|&a| {
                // `to_wire_memo_at` re-stamps a generator's placeholder span with this call site's.
                let w = self.to_wire_memo_at(a, span, memo)?;
                self.ensure_crossable(&w, span)?;
                Ok(w)
            })
            .collect()
    }

    /// A fresh empty module to serve as a worker closure's `home`. The parent's `home` `GcRef`
    /// can't cross heaps; used as the fallback when the task's home is not a real `module_objs`
    /// entry (the hand-built unit-test fixtures) — real spawns resolve a run's module (see
    /// `worker_home`).
    pub(super) fn fresh_worker_home(&mut self) -> GcRef {
        self.heap.alloc(Obj::Module(Box::new(ModuleData::new(
            "<worker>".into(),
            RVec::new(),
            Default::default(),
        ))))
    }

    /// B3.3c — the index of a `home` module `GcRef` in this VM's `module_objs`, so the worker can
    /// resolve the same module. `None` for a home not in the table (test fixtures).
    pub(super) fn home_index(&self, home: GcRef) -> Option<usize> {
        self.walk_src().home_index(home)
    }

    /// B3.3c — resolve a lowered home index to this (worker) VM's module obj, falling
    /// back to a fresh empty home when the parent home was not a real module (test fixtures).
    pub(super) fn worker_home(&mut self, idx: Option<usize>) -> GcRef {
        match idx {
            Some(i) => self.module_obj(i),
            None => self.fresh_worker_home(),
        }
    }
}
