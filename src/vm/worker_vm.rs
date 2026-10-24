//! VM: isolated worker VMs, Executor joins, the worker home, module snapshots and replay.
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
    /// WHAT NEITHER COVERS: growth AFTER the sample is paced, not bounded (`Heap::get_mut`'s settle
    /// charges it). Do not restate any of the above as an unconditional guarantee.
    pub(super) fn spawn_worker(&self) -> Vm {
        // SAFETY: the gc_coord protocol (`vm/gc_coord.rs`): the worker joins this run's `GcCoord`
        // (below) and attaches before it runs code on its own thread (`Vm::worker_loop_held`); a
        // payload `Vm` allocates only on the spawner's attached thread between two of its
        // safepoints. Every crossing copies, so the worker writes only objects it built.
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
        let PendingCall::Call { span, .. } = &task;
        let snap = self.fresh_view(*span)?;
        self.prepare_worker(task, snap, &[], 0)?.run()
    }

    /// B3.3-threads — the parent-thread half of [`Vm::run_task_isolated`]: lower the task to a `Send`
    /// description against THIS heap, build the worker + reconstruct the module graph in its heap, and
    /// rebuild the callee/receiver + args **into the worker heap**, yielding a [`ReadyWorker`] that
    /// can be moved to a pool thread and `run()`. Everything that reads the parent heap happens here;
    /// nothing in `ReadyWorker::run` touches `self`.
    ///
    /// W6-2 — `snap` is the task's PIN, the module view snapshotted at its `spawn` ([`QueuedTask`]), which
    /// every caller passes in; [`Vm::fresh_view`] is the one door that builds it.
    pub(super) fn prepare_worker(
        &mut self,
        task: PendingCall,
        snap: Arc<ModuleSnapshot>,
        cell_ids: &[(GcRef, u32)],
        fresh: u32,
    ) -> Result<ReadyWorker, RuntimeError> {
        // TICKET-111 — `cell_ids` now mixes cells and adoptable data-node ids (see `deep_clone_all`'s
        // report filter); split them so `share` (the cell-sharing prune below) only fires on an actual
        // cell, and the data-node ids can seed `snapshot_adopt` in `rebuild_ready`.
        let share = cell_ids
            .iter()
            .any(|&(h, _)| matches!(self.heap.get(h), Obj::Cell(_)));
        let adopt_ids: Vec<u32> = cell_ids
            .iter()
            .filter(|&&(h, _)| !matches!(self.heap.get(h), Obj::Cell(_)))
            .map(|&(_, id)| id)
            .collect();
        // 1. Lower the task to a `Send` description in THIS (parent) heap (read-only serialize),
        //    rejecting any value that can't cross a heap boundary as-is.
        let lowered = self.lower_task(task, cell_ids)?;
        // 2. Build the worker + install the shared read-only module snapshot (D1): pre-alloc empty
        //    module objs (indices line up with the parent), faulting each module's globals into the
        //    worker heap lazily on first access — instead of eagerly reconstructing the whole graph
        //    per task. 3. rebuild the callable/receiver + args into the worker heap (a `home` index
        //    resolves to a pre-alloced empty module that faults on first global read). The actual
        //    invoke is `ReadyWorker::run`.
        let payload = super::gc_door::PayloadInFlight::new();
        let mut worker = self.spawn_worker();
        worker.install_snapshot(snap);
        let (call, span) = worker.rebuild_ready(lowered, share, &adopt_ids, fresh);
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
        // TICKET-111 — `cell_ids` now also carries `deep_clone_all`'s adoptable data-node ids
        // (see that fn's report filter): split them back apart by heap kind so a data node ties to
        // the snapshot's node registry (`base_nodes`) rather than being mistaken for a cell.
        let (cells, node_ids): (Vec<_>, Vec<_>) = cell_ids
            .iter()
            .copied()
            .partition(|&(h, _)| matches!(self.heap.get(h), Obj::Cell(_)));
        let base_nodes = (!node_ids.is_empty()).then(|| {
            Arc::new(
                node_ids
                    .into_iter()
                    .collect::<super::fxhash::FxHashMap<_, _>>(),
            )
        });
        let mut memo = WireMemo {
            cells: cells.into_iter().collect(),
            base_nodes,
            next_id,
            ..WireMemo::default()
        };
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
                        // order so the wire format (Vec<(name, value)>) is unchanged. TICKET-016
                        // (W8-25): this is the DIRECT spawn callee — no `globals` snapshot here,
                        // see the `Lowered::Closure` doc comment (its home module is already
                        // covered by `pin_snapshot`). A nested closure captured here still gets
                        // its own globals installed through `to_wire_memo_at`'s `WireValue::Closure`
                        // arm.
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

    /// PHASE 3 of task preparation — rebuild a [`Lowered`] task's callable/receiver + args INTO THIS
    /// VM's heap, resolving home indices against `self.module_objs` (the just-installed module view).
    /// Split out of [`Vm::prepare_worker`]. Infallible (all crossing checks happened in
    /// [`Vm::lower_task`]).
    ///
    /// W7-4: ONE rebuild map spans the whole `Lowered`, mirroring `lower_task`'s single [`WireMemo`]
    /// and reconstructing in the SAME order it serialized (a `Call`'s args before the callee's
    /// captures), so a cell shared between an arg and a capture is rebuilt once and both references
    /// tie to it — and no `Backref` is ever reached before the `WireValue::Cell` that defines it.
    ///
    /// D4 (TICKET-240): `fresh` names the checker's shape for each argument
    /// ([`crossing::operand`]; a receiver is arg 0 of its entry thunk, TICKET-235). After the
    /// rebuild [`Vm::unmark_fresh`] unmarks exactly the objects each shape says the call site
    /// built.
    pub(super) fn rebuild_ready(
        &mut self,
        lowered: Lowered,
        share: bool,
        adopt_ids: &[u32],
        fresh: u32,
    ) -> (ReadyCall, Span) {
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
        // D4 layer C (TICKET-169): everything this call rebuilds — a spawned callee's captures, a
        // `spawn f(args)`'s arguments, a `spawn obj.m()`'s receiver — follows `Route::Spawn`.
        let saved_copy_mark = self.copy_mark;
        self.copy_mark = crossing::marks(Route::Spawn, saved_copy_mark);
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
                // TICKET-016 (W8-25): no `globals` snapshot for the direct spawn callee — see the
                // `Lowered::Closure` doc comment. A crossed captured closure's own globals were
                // already installed inside `cap` via `from_wire_memo`'s `WireValue::Closure` arm.
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
        // TICKET-111 — copy each adopt id's just-rebuilt handle into `snapshot_adopt`, so this task's
        // OWN module-global fault (`fault_module`) can adopt it as the global's object instead of
        // rebuilding a second copy. Must happen before the cell-only prune below (`owned` still holds
        // both cells and data nodes here).
        for &id in adopt_ids {
            if let Some(&h) = owned.get(&id) {
                self.snapshot_adopt.insert(id, h);
            }
        }
        // W7-4c — prune to cells, for the same reason `fault_module` does: only a cell can be
        // back-referenced by a LATER, separate serialization (the module snapshot's), and keeping the
        // task's whole rebuilt object graph in a `Vm`-lived GC root would make it immortal.
        if share {
            owned.retain(|_, &mut h| matches!(self.heap.get(h), Obj::Cell(_)));
            self.snapshot_rebuild = owned;
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

    /// A fresh empty module to serve as a worker closure's `home`. The parent's `home` `GcRef` can't
    /// cross heaps; used as the fallback when the task's home is not a real `module_objs` entry (the
    /// hand-built unit-test fixtures) — real spawns resolve a reconstructed module (see `worker_home`).
    pub(super) fn fresh_worker_home(&mut self) -> GcRef {
        self.heap.alloc(Obj::Module(Box::new(ModuleData {
            name: "<worker>".into(),
            slots: RVec::new(),
            index: Default::default(),
        })))
    }

    /// B3.3c — the index of a `home` module `GcRef` in this VM's `module_objs`, so the worker can
    /// resolve the corresponding rebuilt module. `None` for a home not in the table (test fixtures).
    pub(super) fn home_index(&self, home: GcRef) -> Option<usize> {
        self.walk_src().home_index(home)
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
    /// drops it (`set_global_slot` / `module_define`), and [`Vm::fresh_view`] drops it when the cached
    /// snapshot is not `reusable` (some global holds a mutable aggregate, which in-place mutation changes
    /// with no slot write) and its `view` stamp is stale. So a global initialized or mutated after an earlier nursery is SEEN by later
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
        if self.is_task_view() {
            for i in 0..self.module_faulted.len().min(self.module_objs.len()) {
                self.fault_module(i);
            }
        }
        self.mark_view();
        // W7-4c — seed the build from the MONOTONIC counter, and keep the cell registry it produces
        // so a later `deep_clone_all`/`lower_task` can serialize the same binding under the same id.
        // On failure nothing is stored (the build path caches only on success), so a faulted snapshot
        // never leaves a half-registry behind.
        let (built, cells, nodes, next_id) = self
            .snapshot_modules(self.snapshot_next_id)
            .map_err(|e| self.err(e.message, span))?;
        let snap = Arc::new(built);
        self.snapshot_cells = cells;
        self.snapshot_nodes = nodes;
        self.snapshot_next_id = next_id;
        self.snapshot_builds += 1;
        // Cache unconditionally: consecutive `spawn`s into one nursery must not each rebuild the whole
        // view (that is O(all module globals) per spawn — measured 84× on a spawn storm with a big
        // aggregate global). Freshness comes from the two invalidation rules, not from refusing to cache.
        self.snapshot_memo = Some(Arc::clone(&snap));
        Ok(snap)
    }

    /// TICKET-213 — mark every object this view's module globals reach as viewed, so
    /// `Heap::get_mut` moves the view epoch when one of them is mutated in place. Runs right before
    /// the build that stamps the snapshot; nothing mutates the heap between the two. The module
    /// objects themselves stay unmarked: a slot write already drops the cache
    /// (`set_global_slot` / `module_define`), and a worker's lazy fault replay writes slots.
    fn mark_view(&mut self) {
        self.heap.begin_view();
        let mut stack: Vec<GcRef> = Vec::new();
        for &pm in &self.module_objs {
            if let Obj::Module(m) = self.heap.get(pm) {
                stack.extend(m.slots.iter().filter_map(|v| v.as_obj()));
            }
        }
        while let Some(h) = stack.pop() {
            if self.module_objs.contains(&h) || !self.heap.set_viewed(h) {
                continue;
            }
            stack.extend(self.heap.children(h));
        }
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
        // TICKET-111 — the snapshot node registry, harvested from `memo.nodes` after EACH module's
        // globals loop (before the next module clears it): first module wins, mirroring `snapshot_cells`.
        // Filtered to `is_adoptable_node` — this registry never carries a `Cell` (that is
        // `snapshot_cells`'s job) or a `Closure` (never adopted).
        let mut nodes = super::fxhash::FxHashMap::default();
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
                | Obj::Closure { .. }
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
                | Obj::Generator(_) => ("<worker>".into(), Vec::new()),
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
            // only for a cell reached from 2+ modules. TICKET-100: WITHIN a module the `nodes` clear just below
            // give containers and generators the analogous per-module self-containment.
            memo.emitted.clear();
            // TICKET-100 — a container/generator is not re-emitted per module the way a cell is (that
            // would be O(size) wire per module, not O(1)), so `nodes` is CLEARED per
            // module instead: this module's encoding must stay self-contained because modules fault in
            // lazily and in the task's own order, so a `Backref`/reject built against an earlier
            // module's node would be meaningless here. A container aliased by globals in two DIFFERENT
            // modules is therefore still two independent copies in the task.
            memo.nodes.clear();
            for (k, v) in globals {
                // TICKET-119 — a `doom` record only describes the global whose walk made it: clear
                // before each global's own walk, so no global's skip decision depends on another's.
                memo.doom.clear();
                memo.doom_ids.clear();
                memo.doom_invalid_upto = None;
                reusable &= self.slot_snapshot_reusable(v);
                snapped.push((k, self.to_snap(v, &mut memo)?));
            }
            // TICKET-111 — harvest THIS module's data-node ids before the next iteration clears
            // `memo.nodes`; first module wins (`or_insert`), same tie-break as `snapshot_cells`.
            for (&h, &id) in &memo.nodes {
                if self.is_adoptable_node(h) {
                    nodes.entry(h).or_insert(id);
                }
            }
            modules.push(ModuleSnap {
                name,
                globals: snapped,
            });
        }
        Ok((
            ModuleSnapshot {
                modules,
                reusable,
                view: self.heap.view_stamp(),
                #[cfg(test)]
                replays: std::sync::atomic::AtomicUsize::new(0),
            },
            Arc::new(memo.cells),
            Arc::new(nodes),
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
    /// Everything else — `List`/`Map`/`Set`/`Tuple`/`Struct`/`Enum`/`Cell`/`Closure`/
    /// `Generator`/`Iter`/`ByteArray`/an inline `Module` — is mutated IN PLACE (`q.push(1)`, `m[k] = v`,
    /// `p.x = 1`, a captured cell) with no slot write to invalidate on, so a snapshot holding one is
    /// never cached: the nursery rebuilds. A WHITELIST on purpose — a new `Obj` variant defaults to
    /// "rebuild" (slower, never stale).
    ///
    /// TICKET-213 — a non-reusable snapshot is still cached while nothing it reaches was mutated:
    /// `Heap::get_mut`, the sole `&mut Obj` door, moves the view epoch the snapshot is stamped with
    /// (`ModuleSnapshot::view`, read by [`Vm::fresh_view`]). No mutating intrinsic is hooked.
    fn slot_snapshot_reusable(&self, v: Value) -> bool {
        // The leaf list has one owner (`boxctx::slot_is_leaf`); the box slot door reads it too.
        if super::boxctx::slot_is_leaf(&self.heap, v) {
            return true;
        }
        // An import alias: reusable iff it is one of the modules this same walk snapshots (so its
        // own globals were vetted too). A module NOT in `module_objs` is encoded INLINE — its
        // globals are unvetted here, so play it safe and rebuild.
        v.as_obj().is_some_and(|h| {
            matches!(self.heap.get(h), Obj::Module(_)) && self.module_objs.contains(&h)
        })
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
        debug_assert!(
            memo.base_nodes.is_none(),
            "a speculating memo must not carry base_nodes: the watermark rollback cannot undo a base id"
        );
        let mint_from = memo.next_id;
        memo.emit_undo.clear();
        memo.depth_tripped = false;
        memo.shortcut_at = None;
        memo.doom_pending.clear();
        memo.speculating = true;
        let attempt = self.to_wire_depth(v, depth, memo);
        memo.speculating = false;
        match attempt {
            Ok(w) if keep(&w) => {
                memo.emit_undo.clear();
                Some(w)
            }
            _ => {
                let record = memo.depth_tripped;
                if record {
                    memo.doom.clear();
                    memo.doom_ids.clear();
                    memo.doom_invalid_upto = None;
                    for (h, e, d) in memo.doom_pending.drain(..) {
                        memo.doom.insert(h, (e, d));
                    }
                }
                let doom_ids = &mut memo.doom_ids;
                memo.cells.retain(|h, id| {
                    if record && *id >= mint_from {
                        doom_ids.insert(*h, *id);
                    }
                    *id < mint_from
                });
                memo.nodes.retain(|h, id| {
                    if record && *id >= mint_from {
                        doom_ids.insert(*h, *id);
                    }
                    *id < mint_from
                });
                // Every journalled id was NEWLY inserted (a present id emits a `Backref` and is not
                // journalled), so removing it restores the set exactly.
                for id in memo.emit_undo.drain(..) {
                    memo.emitted.remove(&id);
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
    /// (re-stamped with the real nursery span by `ensure_snapshot`) rather than a host `SIGABRT`.
    /// The fast path passes `depth` to `to_wire_depth` and every slow arm recurses at `depth + 1`,
    /// so one budget, `MAX_STRUCTURAL_DEPTH`, bounds both walks.
    fn to_snap_depth(
        &self,
        v: Value,
        depth: usize,
        memo: &mut WireMemo,
    ) -> Result<SnapValue, RuntimeError> {
        if self.walk_base + depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.depth_exceeded_err(Span::default()));
        }
        if v.is_uninit() {
            return Ok(SnapValue::Uninit(v.uninit_line()));
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
        // TICKET-119: skip the attempt entirely when an EARLIER depth-tripped attempt already proved,
        // by an exact replay argument, that a walk from `h` at this depth or deeper trips no later —
        // see `docs/reference/concurrency/`'s depth-guard paragraph and `WireMemo::doomed`.
        if !memo.doomed(h, depth)
            && let Some(w) = self.try_wire_speculative(v, depth, memo, |w| !w.has_handle())
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
                    snapped.push((names.get(i).cloned().unwrap_or_default(), self.to_snap_depth(cv, depth + 1, memo)?));
                }
                // TICKET-016 (W8-25) review finding — this `SnapValue` form is the PER-TASK whole
                // module snapshot (`ensure_snapshot`/`fault_module`): `home`'s globals move into the
                // new task's own module copy in the SAME replay as this closure, so `f` must keep
                // doing a live `Op::GetGlobalSlot` read against that copy rather than installing a
                // frozen value at the snapshot's pre-write state — the whole snapshot predates every
                // spawned task's own writes, unlike an actual airlock crossing (`WireValue::Closure`,
                // `Lowered::Closure`), where the sender's write already happened before it sends.
                SnapValue::Closure { proto, captured: snapped, home: self.home_index(home) }
            }
            // An import alias bound to another module obj.
            Obj::Module(m) => match self.home_index(h) {
                Some(idx) => SnapValue::ModuleAlias(idx),
                // A module not in `module_objs` (shouldn't occur for a bound import) — encode inline,
                // in slot order so replay rebuilds matching slots.
                None => {
                    let ModuleData {
                        name,
                        slots,
                        index,
                    } = *m;
                    let mut globals = Vec::new();
                    for (k, mv) in module_slot_pairs(&slots, &index) {
                        globals.push((k, self.to_snap_depth(mv, depth + 1, memo)?));
                    }
                    SnapValue::ModuleInline { name, globals }
                }
            },
            Obj::Native { name, func, kind } => SnapValue::Native { name, func, kind },
            // A first-class builtin fn is pure code — SENDABLE. Carry the id; the worker re-allocs
            // a fresh `Obj::Builtin` on replay (like `Native`, but with no fn pointer to share).
            Obj::Builtin(b) => SnapValue::Builtin(b),
            // A Cffi shares its `Arc` to the worker (which shares the parent address space): the
            // worker re-allocs `Obj::Cffi` from the SAME Arc — no re-dlopen, no symbol re-resolution.
            Obj::Cffi(c) => SnapValue::Cffi(Arc::clone(&c)),
            // Containers embedding a callable: encode each element. (Pure-data containers took the fast
            // path above.) A generator embedded in any of these faults via the recursive `?`.
            Obj::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for x in &items {
                    out.push(self.to_snap_depth(x, depth + 1, memo)?);
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
                    out.push((k, self.to_snap_depth(fv, depth + 1, memo)?));
                }
                SnapValue::Struct { name, fields: out }
            }
            Obj::Enum { variant_id, payload } => {
                let mut out = Vec::with_capacity(payload.len());
                for x in &payload {
                    out.push(self.to_snap_depth(x, depth + 1, memo)?);
                }
                // M19 lever #2 — carry the dense `variant_id` directly on the cold snap path (mirrors
                // `to_wire`); replay reuses it as-is against the shared program.
                SnapValue::Enum { variant_id, payload: out }
            }
            Obj::Map(m) => {
                let mut out = Vec::with_capacity(m.entries.len());
                for (hash, k, val) in &m.entries {
                    out.push((
                        hash,
                        self.to_snap_depth(k, depth + 1, memo)?,
                        self.to_snap_depth(val, depth + 1, memo)?,
                    ));
                }
                SnapValue::Map(out)
            }
            Obj::Set(s) => {
                let mut out = Vec::with_capacity(s.entries.len());
                for (hash, e) in &s.entries {
                    out.push((hash, self.to_snap_depth(e, depth + 1, memo)?));
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
            //     handle / a generator running at the spawn) → snapshot an inert `Unsendable` generator,
            //     NOT the `?`-propagated reject. Emitting `Wire(w)` for a handle-bearing generator would
            //     replay a parent `GcRef` on the worker (the memory-safety hole), so it must not cross —
            //     but eager-faulting the WHOLE snapshot here would abort every `spawn` in a module that
            //     merely *holds* a non-sendable generator it never sends (a regression:
            //     `snapshot_modules` walks EVERY global once, reached or not). The inert copy keeps the
            //     snapshot infallible: a task that never touches the generator runs clean, and one that
            //     DOES drive it faults at the use site with a message naming the cause — "fault only
            //     when reached" by construction (every task snapshots from the same memoized frozen
            //     copy).
            // W7-4: also SPECULATIVE (the fallback branch discards it) — same rollback discipline, and
            // it must use the SHARED memo on the kept branch or its ids would collide with the ones the
            // module's other globals minted, aliasing unrelated nodes on replay.
            // TICKET-137 — the discarded attempt no longer degrades to `Nil` (an aliased running
            // generator read as `type nil has no method 'next'`). It becomes an inert
            // `GenState::Unsendable` generator: a task that never drives it runs clean, and one that
            // does gets a fault naming the cause.
            Obj::Generator(g) => SnapValue::Wire(
                match self.try_wire_speculative(v, depth, memo, |w| !w.has_handle()) {
                    Some(w) => w,
                    None => {
                        let msg = if self.active_generators.contains(&h) {
                            "a module-global generator that was running when this task was spawned has no copy in the task; send its values through a Channel"
                        } else {
                            "a module-global generator holding a value that cannot cross tasks has no copy in the task; send its values through a Channel"
                        };
                        let id = memo.next_id;
                        memo.next_id += 1;
                        WireValue::Generator {
                            id,
                            proto: g.proto,
                            home: self.home_index(g.home),
                            closure: None,
                            state: WireGenState::Unsendable(msg.into()),
                        }
                    }
                },
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
                // TICKET-119: this arm mints a Backref target directly (bypassing the speculative
                // wrapper), so a doom record whose recorded sub-walk contains `h` is now invalid.
                memo.doom_touch(h);
                let id = match memo.cell_id(h) {
                    Some(id) => id,
                    None => {
                        let id = memo.next_id;
                        memo.next_id += 1;
                        memo.cells.insert(h, id);
                        id
                    }
                };
                if memo.emitted.contains(&id) {
                    SnapValue::Backref(id)
                } else {
                    // No `emit_undo` journal here, unlike `to_wire_depth`'s twin: this arm is only
                    // ever reached AFTER `try_wire_speculative` has already returned (it runs
                    // `to_wire_depth`, never `to_snap_depth`), so `speculating` is false and this
                    // marking is never part of an attempt that can be thrown away.
                    memo.emitted.insert(id);
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
                    out.push(self.to_snap_depth(x, depth + 1, memo)?);
                }
                SnapValue::Iter { items: out, pos }
            }
        })
    }

    /// TICKET-233 -- drain the snapshot's replay count into [`super::MODULE_REPLAYS`].
    #[cfg(test)]
    pub(super) fn publish_module_replays(&self) {
        if let Some(s) = &self.snapshot_memo {
            let n = s.replays.swap(0, Ordering::Relaxed);
            super::MODULE_REPLAYS.with(|c| c.set(c.get() + n));
        }
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
                slots: RVec::new(),
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
        // TICKET-111 — same reasoning: a fresh worker starts with no node registry either.
        self.snapshot_nodes = Arc::new(super::fxhash::FxHashMap::default());
        // TICKET-111 — a fresh worker has no pending adoption captures of its own yet either (they are
        // populated per-task by `rebuild_ready`, after this runs).
        self.snapshot_adopt.clear();
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
        #[cfg(test)]
        snap.replays.fetch_add(1, Ordering::Relaxed);
        let module = self.module_objs[idx];
        // W6-2 — a replay REPRODUCES the snapshot, it does not mutate the view, so its `module_define`s
        // must not drop the cache this view was seeded with (`install_snapshot`).
        let memo = self.snapshot_memo.take();
        // W7-4c — `module_define` clears the registry (it is a slot mutator); a replay REPRODUCES the
        // snapshot rather than mutating the view, so the registry rides the same take/restore the
        // cache does.
        let saved_cells = std::mem::take(&mut self.snapshot_cells);
        // TICKET-111 — the node registry rides the same take/restore, for the same reason.
        let saved_nodes = std::mem::take(&mut self.snapshot_nodes);
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
        // `from_wire_pieces` do: clear it going in, assert it going out, clear it again so a snapshot
        // miss is never charged to the next unrelated `from_wire` caller's assert. Loud in debug; in
        // release the miss still degrades to `nil` rather than aborting the host.
        self.wire_backref_missing = false;
        // TICKET-111 — adoption is consulted ONLY across this module's own replay: `RwShared.slice`
        // (`src/vm/netio.rs`) relies on `from_wire_memo`'s container/generator arms NOT deduping on
        // any other path, and a `Channel` message's ids are a separate id space that must never be
        // checked against `snapshot_adopt` (gotcha 1).
        self.adopt_active = !self.snapshot_adopt.is_empty();
        // D4 layer C (TICKET-169): this replay builds the task's OWN copy of the module's globals —
        // `Route::ModuleSnapshot` marks every object it allocates, so a later write to it faults.
        let saved_copy_mark = self.copy_mark;
        self.copy_mark = crossing::marks(Route::ModuleSnapshot, saved_copy_mark);
        for (name, sv) in &snap.modules[idx].globals {
            let val = self.replay_snap(sv, &mut rb);
            self.module_define(module, name, val);
        }
        self.copy_mark = saved_copy_mark;
        self.adopt_active = false;
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
        self.snapshot_nodes = saved_nodes;
    }

    /// D1 — if this is a worker VM (a snapshot is installed), ensure the module that owns `home` has
    /// been faulted in before its globals are read. No-op on the top-level VM, and on a fiber with no
    /// heap of its own (no snapshot — `module_objs` are the real, already-populated modules), so those
    /// views are untouched. Called at every module-global read site (`GetGlobal`, the `GetCaptured`
    /// home fallback, module member access, and a `module.fn(...)` call).
    pub(super) fn ensure_module_faulted(&mut self, home: GcRef) {
        // TICKET-246 — inside a box closure this read seats the caller's view first. This site
        // returns no fault, so one is kept for the `read_slot` that follows.
        if !self.box_chain.is_empty()
            && let Err(e) = self.box_global(home, None, Span::default())
        {
            self.box_view_err = Some(e);
            return;
        }
        if !self.is_task_view() {
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
                let (proto, whome) = (*proto, self.worker_home(*home));
                self.fn_value(FnKey::Proto(proto), || Obj::Func { proto, home: whome })
            }
            SnapValue::Closure {
                proto,
                captured,
                home,
            } => {
                let whome = self.worker_home(*home);
                // Lever #3: rebuild positionally (slot order), discarding the carried names.
                // D4 layer C: a snapshot closure never replays into the heap it left, so its
                // captures are marked (`Route::ClosureCaptures`).
                let saved_copy_mark = self.copy_mark;
                self.copy_mark =
                    crossing::marks(Route::ClosureCaptures { sole: false }, saved_copy_mark);
                let cap: Vec<Value> = captured
                    .iter()
                    .map(|(_k, cv)| self.replay_snap(cv, rb))
                    .collect();
                self.copy_mark = saved_copy_mark;
                Value::obj(self.heap.alloc(Obj::Closure {
                    proto: *proto,
                    captured: cap.into(),
                    home: whome,
                }))
            }
            SnapValue::ModuleAlias(idx) => Value::obj(self.module_objs[*idx]),
            SnapValue::ModuleInline { name, globals } => {
                let wm = self.heap.alloc(Obj::Module(Box::new(ModuleData {
                    name: name.clone(),
                    slots: RVec::new(),
                    index: std::collections::HashMap::new(),
                })));
                for (k, gv) in globals {
                    let val = self.replay_snap(gv, rb);
                    self.module_define(wm, k, val);
                }
                Value::obj(wm)
            }
            SnapValue::Native { name, func, kind } => {
                self.fn_value(FnKey::Native(name.clone(), *func as usize), || {
                    Obj::Native {
                        name: name.clone(),
                        func: *func,
                        kind: *kind,
                    }
                })
            }
            // The heap's one `Obj::Builtin` for this id ([`Vm::fn_value`], TICKET-226).
            SnapValue::Builtin(b) => self.fn_value(FnKey::Builtin(*b), || Obj::Builtin(*b)),
            SnapValue::Uninit(line) => Value::uninit(*line),
            // Re-alloc from the SAME shared `Arc<Cffi>` — no re-dlopen (shared address space).
            SnapValue::Cffi(c) => self.fn_value(FnKey::Cffi(Arc::as_ptr(c) as usize), || {
                Obj::Cffi(Arc::clone(c))
            }),
            SnapValue::List(xs) => {
                let v = xs.iter().map(|x| self.replay_snap(x, rb)).collect();
                let h = self.heap.alloc(Obj::List(v));
                if self.copy_mark {
                    self.heap.set_copied(h);
                }
                Value::obj(h)
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
                let h = self.heap.alloc(Obj::Struct {
                    tid,
                    fields: Fields::from_vec(f),
                });
                if self.copy_mark {
                    self.heap.set_copied(h);
                }
                Value::obj(h)
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
                if self.copy_mark {
                    self.heap.set_copied(h);
                }
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
                let mut out = Vec::with_capacity(entries.len());
                for (hash, k, val) in entries {
                    let (ck, cv) = (self.replay_snap(k, rb), self.replay_snap(val, rb));
                    out.push((*hash, ck, cv));
                }
                let h = self.heap.alloc(Obj::Map(MapData::from_entries(out)));
                if self.copy_mark {
                    self.heap.set_copied(h);
                }
                Value::obj(h)
            }
            SnapValue::Set(entries) => {
                let mut out = Vec::with_capacity(entries.len());
                for (hash, e) in entries {
                    let ce = self.replay_snap(e, rb);
                    out.push((*hash, ce));
                }
                let h = self.heap.alloc(Obj::Set(SetData::from_entries(out)));
                if self.copy_mark {
                    self.heap.set_copied(h);
                }
                Value::obj(h)
            }
        }
    }
}
