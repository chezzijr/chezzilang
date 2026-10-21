//! VM: pool and width slots, seed perturbation, the joiner, the worker loop, `run_one_fiber`,
//! outcome classification, wakes, the sched registry.
//! Split out of `src/vm/sched.rs`. `super::*` is the parent module.

use super::sched::BlockedOwnerGuard;
use super::sched::GatedReg;
use super::sched::JoinCover;
#[cfg(test)]
use super::sched::RunnerCount;
use super::sched::eager_joiner_runs_fibers;
use super::sched::poke_live_scheds;
use super::sched::spawn_worker_thread;
use super::*;

impl Vm {
    /// TICKET-016 (W8-3) generalisation of the demote bracket: "I am about to block in place" for any
    /// in-callback blocking op, not just a socket op. `what` names the op in the OS-thread-limit fault
    /// message. A no-op when `self.mn` is `None` (a top-level `Shared.set`/`update` with no nursery),
    /// so a guard wait outside any nursery neither starves a pool nor panics on the `.expect` the
    /// socket-only version used to carry.
    /// TICKET-052 — hand this thread's pool slot to a replacement worker before blocking in place.
    /// A no-op on an M:N worker shell (`self.mn.is_some()`): those already compensate through
    /// [`Vm::block_enter`], and yielding here too would spawn two replacements for one block. This
    /// is the POOL-side half only: which bracket a wait takes is `block::mode`'s decision.
    /// TICKET-118 (W13-7) — a job's top-level `Vm` joining a nursery hands its slot over in
    /// `MnSched::joiner_wait` and `MnSched::take_runnable` instead, marked by
    /// `MnSched::pool_joiner_guard` and only while that sched has no running or runnable fiber; an
    /// M:N shell still never yields.
    pub(super) fn yield_pool_slot(&self, budget: Option<std::time::Duration>) {
        if self.mn.is_some() {
            return;
        }
        crate::vm::pool::yield_slot(budget);
    }

    /// TICKET-141 (W14-14) — give this thread's width permit back. A no-op unless it holds one.
    pub(super) fn width_release(&mut self) {
        width::release();
    }

    /// TICKET-141 — take a width permit in FIFO order. A no-op for an ungated shell or one that
    /// already holds a permit.
    pub(super) fn width_acquire(&mut self) {
        width::acquire();
    }

    /// TICKET-205 — list this gated thread as an in-place waiter of `cores` for as long as the
    /// guard lives, so the waker (`ChannelCore::wake_all`) queues it for the permit in the waker's
    /// own order. Called while the permit is held: the list order is the order the waits began.
    pub(super) fn gated_register(&self, cores: &[&Arc<ChannelCore>]) -> GatedReg {
        let me = width::my_slot();
        for (i, c) in cores.iter().enumerate() {
            let sleeps_on = (i > 0).then(|| Arc::clone(cores[0]));
            c.gated
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((Arc::clone(&me), sleeps_on));
        }
        GatedReg {
            cores: cores.iter().map(|c| Arc::clone(c)).collect(),
            me,
        }
    }

    /// TICKET-205 — THE bracket of a one-tick in-place wait on channels: register, release the
    /// permit, wait, re-take it.
    pub(super) fn wait_released_on<R>(
        &mut self,
        cores: &[&Arc<ChannelCore>],
        wait: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let _gated = self.gated_register(cores);
        width::release();
        let r = wait(self);
        width::acquire();
        r
    }

    /// TICKET-205 — a pool thread about to keep its slot past a slice end makes room for a queued
    /// job: it starts one pool thread for the queue (gated like every runner, TICKET-230).
    pub(super) fn offer_pool_slot(&self) {
        if crate::vm::pool::job_waits_for_my_slot() {
            crate::vm::pool::yield_slot(None);
        }
    }

    /// TICKET-167 — seeded-mode reduction budget: `CONTEXT_REDS` unseeded, else a random budget so
    /// forced preemption fires at varying points instead of the same fixed op count every schedule.
    /// 1-in-4 draws a short budget (1..=64) to bias toward tighter preemption windows.
    fn fresh_reds(&self) -> u32 {
        if !sched_seed::on() {
            return CONTEXT_REDS;
        }
        let Some(sched) = self.mn.clone() else {
            return CONTEXT_REDS;
        };
        let r = sched.rng.next();
        if r % 4 == 0 {
            1 + ((r >> 8) % 64) as u32
        } else {
            1 + ((r >> 8) % u64::from(CONTEXT_REDS)) as u32
        }
    }

    /// TICKET-167 — a seeded-mode sync point: no-op unless `sched_seed::on()`.
    #[inline]
    pub(super) fn sched_seed_point(&mut self) {
        if sched_seed::on() {
            self.sched_seed_perturb();
        }
    }

    /// TICKET-167 — part 2, perturbation at T >= 2: a random yield, short spin or delay, drawn from
    /// this fiber's sched RNG. Runs only at `worker_count() >= 2` (see `## Decisions`: at T=1 these
    /// arms would move only OS timing, which replay cannot control).
    #[cold]
    fn sched_seed_perturb(&mut self) {
        let Some(sched) = self.mn.clone() else {
            return;
        };
        let r = sched.rng.next();
        match r % 8 {
            0 | 1 => self.reds = 0,
            2 if worker_count() >= 2 => std::thread::yield_now(),
            3 if worker_count() >= 2 => {
                for _ in 0..((r >> 8) % 4096) {
                    std::hint::spin_loop();
                }
            }
            4 if worker_count() >= 2 => {
                std::thread::sleep(std::time::Duration::from_micros(1 + (r >> 8) % 50))
            }
            _ => {}
        }
    }

    /// TICKET-141 (W14-14) — the D3 budget ran out inside a native callback. The fiber cannot leave
    /// the thread, so the THREAD hands its runner slot to one replacement and takes one back in FIFO
    /// order, keeping `--threads=N` at N runners. Preempts only when someone can use the slot.
    pub(super) fn slice_end_in_place(&mut self) {
        self.reds = CONTEXT_REDS;
        let fiber_waits = self
            .mn
            .as_ref()
            .is_some_and(|s| s.runnable.load(Ordering::Relaxed) > 0);
        let job_waits = crate::vm::pool::job_waits_for_my_slot();
        let due = width::handover_due(true);
        if !fiber_waits && !job_waits && !due {
            return;
        }
        // A refused replacement thread returns without handing off: the thread keeps running.
        if let Some(sched) = self.mn.clone()
            && !self.demote_once(&sched)
        {
            return;
        }
        self.offer_pool_slot();
        // TICKET-206 — the thread queues its own next turn while it still holds the permit: the
        // release wakes the queue head, which could otherwise queue a third thread first.
        // TICKET-230 — with no thread queued this round trip took the thread's own ticket back at
        // once, so it is skipped; with one queued it runs once per `HANDOVER_QUANTUM`.
        if due {
            width::reserve(&width::my_slot());
            self.width_release();
            self.width_acquire();
        }
    }

    /// TICKET-147 (W14-15) — a nursery join is a cancellation point for its OWNER. `join_nursery`
    /// reduces an all-`Cancelled` slot vector to `Ok(())`, so a cancelled owner would otherwise run the
    /// straight-line code after its join (asyncio never runs code after a cancelled `async with`).
    /// Runs after the join returns; it reads [`Vm::take_halt`]: the flags the owner holds from an
    /// ENCLOSING scope and the nurseries it still owns — never the one just joined (DEC-096: the
    /// owner learns that nursery's fault from the join itself), and never inside a `defer`. A join that parked
    /// (`join_suspend`) re-runs after the wake, so it is not checked here.
    pub(super) fn cancel_at_join(&mut self, span: Span) -> Result<(), RuntimeError> {
        if self.join_suspend.is_none() {
            self.resume_halts(span)?;
        }
        Ok(())
    }

    /// TICKET-141 — `join_nursery` with this thread's width permit released for the whole join.
    pub(super) fn join_nursery_released(&mut self) -> Result<(), RuntimeError> {
        self.width_release();
        let r = self.join_nursery();
        self.width_acquire();
        r
    }

    /// TICKET-063 — enter a `Shared`/`RwShared` update-guard wait through the one demote bracket
    /// ([`Vm::block_enter`]). A guard wait has no promised external progress — it returns only
    /// when the owner (one of the parties the verdict already judges) releases — so it registers
    /// a `PartyWait::Guard` waiter, never `inflight` (DEC-063). With `self.mn` `None` the bracket
    /// keeps DEC-052's immediate `yield_pool_slot(None)`.
    pub(super) fn guard_wait_enter(
        &mut self,
        key: usize,
        what: &str,
        span: Span,
    ) -> Result<DemoteReg, RuntimeError> {
        self.block_enter(
            WaitSpec::Guard,
            Some(quiesce::PartyWait::Guard(key, self.guard_token)),
            what,
            span,
        )
    }

    /// TICKET-063 — exit a guard wait entered via [`Vm::guard_wait_enter`]: unregister its waiter
    /// under core lock A. With `self.mn` `None` there is nothing to undo.
    pub(super) fn guard_wait_exit(&mut self, reg: DemoteReg) {
        if let Some(sched) = self.mn.as_ref().map(Arc::clone) {
            reg.release(&sched, &mut sched.lock());
        }
    }

    /// TICKET-245 — THE shell an inline nursery joiner runs fibers on, or `None` when the joiner
    /// only waits. Every join and abort site calls this and decides nothing itself. The decision is
    /// [`Vm::joiner_helps`] (the `Nursery` row of `block::mode`). A joiner that may not help and
    /// has no cover hands its runner slot over ([`Vm::hand_joiner_slot`]); it keeps the inline
    /// loop only when the OS refused that thread. Call it after the site's guards.
    pub(super) fn joiner_shell(
        &mut self,
        sched: &Arc<MnSched>,
        cancel: &Arc<AtomicBool>,
        wid: usize,
        cover: JoinCover,
    ) -> Option<Vm> {
        let helps = self.joiner_helps();
        let runs = match cover {
            JoinCover::Runners => {
                helps && eager_joiner_runs_fibers(worker_count()) && sched.claim_joiner()
            }
            JoinCover::None => helps || !self.hand_joiner_slot(sched, cancel, wid),
        };
        runs.then(|| self.spawn_shell(sched, cancel))
    }

    /// TICKET-245 — give the joined `sched` one SENTINEL runner in place of a joiner that may not
    /// run fibers on its own stack: this worker's demote replacement on the fiber's own sched, a
    /// `chezzi-mn-repl` thread on any other. `false` iff the OS refused the thread.
    fn hand_joiner_slot(
        &mut self,
        sched: &Arc<MnSched>,
        cancel: &Arc<AtomicBool>,
        wid: usize,
    ) -> bool {
        if self.owns_nested_sched(sched) {
            return self.demote_once(sched);
        }
        let shell = self.spawn_shell(sched, cancel);
        spawn_worker_thread(shell, sched, "chezzi-mn-repl", wid, SENTINEL_SCOPE).is_ok()
    }

    /// THE one-time handover of this worker's runner slot: spawn its replacement worker unless it
    /// already has one. `false` iff the OS refused the thread; the worker is then not demoted.
    pub(super) fn demote_once(&mut self, sched: &Arc<MnSched>) -> bool {
        if !self.demoted {
            if !self.spawn_replacement_worker(sched, self.wid) {
                return false;
            }
            self.demoted = true;
        }
        true
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
        let shell = self.spawn_shell(sched, cancel);
        // SENTINEL — the replacement covers the demoted thread's `wid` and drains the GLOBAL queue
        // (across all scopes) until global terminate; the demoted owner returns on its own (its
        // fiber settles → `self.demoted` exits its loop), so the replacement must not stop early on
        // any single scope.
        spawn_worker_thread(shell, sched, "chezzi-mn-repl", wid, SENTINEL_SCOPE).is_ok()
    }

    /// D2b — a worker shell's lifetime: pull a runnable fiber, run it to its next park/finish, settle,
    /// repeat until the scheduler terminates, over a shared run queue + park set across threads.
    /// Cross-nursery flat scheduler — `owner_scope` is the scope this worker is the INLINE OWNER of
    /// (it returns when that scope completes — scope-scoped owner stop), or [`SENTINEL_SCOPE`] for a
    /// FARMED helper / drainer (which never self-stops, only on global terminate). The fiber it runs may
    /// belong to ANY scope (the queue is global): `finish`/`cancel_drain` use the FIBER's `scope_id`,
    /// while `take_runnable`'s stop check uses `owner_scope`.
    ///
    /// TICKET-230 — the loop releases the permit before it returns, so a caller that held one (main
    /// as joiner, a fiber's inline nested-nursery owner) takes it back here before its own Chezzi
    /// code runs again.
    pub(super) fn mn_worker_loop(&mut self, sched: &Arc<MnSched>, wid: usize, owner_scope: usize) {
        let held = width::holds();
        self.mn_worker_loop_inner(sched, wid, owner_scope);
        if held {
            width::acquire();
        }
    }

    fn mn_worker_loop_inner(&mut self, sched: &Arc<MnSched>, wid: usize, owner_scope: usize) {
        #[cfg(test)]
        let _runner = RunnerCount::enter(&sched.quiesce);
        self.wid = wid; // D5 owe #3 (Path C) — `demote_recv_block` reuses this for the replacement worker
        // TICKET-206 — withdraws the turn this loop queued for itself when it exits without taking it.
        struct OwnTurn;
        impl Drop for OwnTurn {
            fn drop(&mut self) {
                width::cancel(&width::my_slot());
            }
        }
        let _turn = OwnTurn;
        let mut tick: u64 = 0;
        let mut kept: Option<Fiber> = None;
        loop {
            // TICKET-199 — `tick` counts FRESH slices (Go's `schedtick`): an inherited `runnext`
            // pick runs on the current slice and leaves it unchanged.
            let next = tick.wrapping_add(1);
            let (mut fiber, slice) = match kept.take() {
                Some(f) => (f, Slice::Fresh),
                None => match sched.take_runnable(wid, next, owner_scope) {
                    Take::Run(f, slice) => (f, slice),
                    Take::Stop => {
                        debug_assert!(!width::holds(), "TICKET-141: exit holding a permit");
                        return;
                    }
                },
            };
            if slice == Slice::Fresh {
                tick = next;
            }
            self.width_acquire();
            let task_index = fiber.task_index;
            let scope_id = fiber.scope_id;
            let span = fiber.span;
            let disp = self.run_one_fiber(&mut fiber, span, slice);
            self.offer_pool_slot();
            // TICKET-230 — a worker keeps its permit across fibers while no thread queues for one.
            // Releasing and re-taking it per fiber was a gate round trip per park (measured: the
            // head-of-line wait made `send_one_channel` T=0 29.2x base). When a thread queues, the
            // worker hands the permit over at this fiber boundary: a preempted fiber's worker once
            // per `width::HANDOVER_QUANTUM`, any other boundary at once.
            if width::handover_due(matches!(disp, Disp::Yield)) {
                // TICKET-206 — a worker queues its own next turn while it still holds the permit,
                // so its place does not depend on how long the bookkeeping below takes. The reserve
                // stays above the release (DEC-206).
                if width::holds()
                    && (sched.runnable.load(Ordering::Relaxed) > 0 || matches!(disp, Disp::Yield))
                {
                    width::reserve(&width::my_slot());
                }
                // Also runs on the panic path: `run_one_fiber` catches the panic and returns
                // `Disp::Finish`.
                self.width_release();
            }
            match disp {
                Disp::Park(key, core) => sched.park(key, core, fiber),
                // Bounded backpressure — the send-side park (gap re-check = space, not a message).
                Disp::SendPark(key) => sched.park_send(key, fiber),
                // §6d — multi-channel `wait` park: file ONE shared token in every arm's bucket.
                Disp::WaitPark(arms) => sched.park_wait(arms, fiber),
                // TICKET-230 — nothing else is runnable, so the next pick would return this fiber
                // from `global`. Queueing it let an idle worker's look take it in the
                // push-to-re-pop window and run it on a CPU that was idle (`primes_parallel` T=0:
                // per-slice time 1.24x, wall 1.36x). The fiber stays counted in `running`. A
                // demoted worker leaves right after this match, so it yields as before. Seeded mode
                // keeps its replayed yield order (DEC-209). A worker that handed its permit over
                // (`waiting() > 0`) no longer holds one and yields as before.
                Disp::Yield
                    if !self.demoted
                        && !sched_seed::on()
                        && width::holds()
                        && sched.runnable.load(Ordering::Relaxed) == 0 =>
                {
                    kept = Some(fiber);
                }
                Disp::Yield => sched.yield_fiber(fiber),
                // D5 — the fiber hit a blocking native; hand it + the call to the dirty pool (frees
                // this worker). The pool re-enqueues it on completion via `complete_offload`.
                Disp::Offload(req) => sched.offload(fiber, req),
                // D6 — the fiber's socket op `WouldBlock`ed; hand it + the fd to the netpoller (frees
                // this worker). The poller re-enqueues it via `complete_offload` on OS readiness.
                Disp::PollPark(pp) => sched.poll_park_offload(fiber, pp),
                // TICKET-103 — file the owner until its nursery's family completes.
                Disp::JoinPark(origin) => sched.park_join(fiber, origin),
                Disp::Finish(outcome) => {
                    // `finish` reports whether the STORED outcome aborts: it may itself turn a `Done`
                    // into a hard-halt over-memory `Fault` (W7-26r), which needs the same sibling
                    // drain a task's own fault does.
                    // TICKET-270 — `finish` also reports whether it published a job fault (the cell is
                    // stored before the slot); the teardown runs here, outside its lock.
                    // TICKET-284 — test-only: a joiner that adopts a child's `os.exit` stores its own slot
                    // late (a job is scope 0), so the reader of finished output is reachable with no host load.
                    #[cfg(test)]
                    if crate::vm::HOLD_ADOPTED_EXIT.load(Ordering::Relaxed)
                        && scope_id == 0
                        && matches!(&outcome, TaskOutcome::Exit { code: 3, .. })
                    {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                    }
                    let (aborts, job_fault) = sched.finish(task_index, scope_id, outcome);
                    if job_fault {
                        self.halt_run();
                    }
                    // TICKET-270 — this fiber called `os.exit`: its slot now holds its output, so a
                    // flush waiting in `take_finished_streams` may read it.
                    if std::mem::take(&mut self.exit_origin) {
                        self.quiesce.end_exit_origin();
                    }
                    // A fault/exit tripped the FIBER's SCOPE cancel (in `classify_mn_outcome`, via the
                    // re-pointed `self.cancel`); requeue THAT scope's parked siblings so they observe it
                    // and unwind (running ones see it at a back-edge). `drain_family(scope_id)` reaches
                    // the channel-parked and the netpoller-parked fibers of this scope's family ONLY
                    // (never outer siblings — structured concurrency), so a net server sharing a
                    // nursery with a faulting sibling unwinds instead of hanging (D6b — the
                    // production-ready gate).
                    if aborts {
                        sched.drain_family(scope_id);
                    }
                    // A finished Executor job is one fewer live sender for every other sched: an
                    // idle worker elsewhere must ask its verdict again.
                    if sched.detached.is_some() {
                        poke_live_scheds(&self.sched_registry);
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
            // here (`demote_recv_block`, `demote_wait_block`, `demote_block_until`,
            // `block_enter`), so the notify must be unconditional on this exit, not shaped to
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
                // TICKET-230 — a worker keeps its permit across fibers while no thread queues, so
                // it returns the permit here, on its way out.
                self.width_release();
                sched.notify_waiters();
                debug_assert!(!width::holds(), "TICKET-141: exit holding a permit");
                return;
            }
        }
    }

    /// D2b — run a single fiber on this shell: swap its context in, start/resume it until it parks or
    /// finishes, decide its disposition WHILE its heap is live (the park key and outcome are heap-keyed
    /// reads), then swap the context back out. The run is panic-guarded so a worker-VM panic becomes a
    /// task `Fault` (keeps the loop alive + the slot filled — the join can't hang).
    pub(super) fn run_one_fiber(&mut self, fiber: &mut Fiber, span: Span, slice: Slice) -> Disp {
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
            // TICKET-103 — `blocked_owner_guard` counts this fiber on its own scope.
            self.fiber_scope = Some(fiber.scope_id);
        }
        self.suspend = None;
        self.wait_suspend = None; // set by `op_wait_poll`'s M:N snapshot-park (→ `Disp::WaitPark`)
        self.send_suspend = None; // set by a full bounded `send` (→ `Disp::SendPark`)
        self.offload = None;
        self.poll_park = None;
        // TICKET-096 review fix — a delivered cut's floor indexes THIS fiber's `nurseries` (swapped
        // by `swap_ctx`, not carried in `FiberCtx`), so a cut left by the fiber that just parked/died
        // on this shell must not survive into the next fiber scheduled in here, or an unrelated
        // `recover:` in that fiber is wrongly bypassed (`run_until`'s `owner_bypass`).
        self.cut = None;
        self.cancel_unwind_faulted = false;
        self.pending_exit = None;
        // D3 — a fresh reduction budget on a fresh slice (BEAM semantics). TICKET-199 — a `runnext`
        // pick (`Slice::Inherit`, Go `inheritTime`) keeps the budget the previous fiber left, so a
        // rendezvous pair shares one slice and the local ring is served when it runs out.
        if slice == Slice::Fresh {
            self.reds = self.fresh_reds();
        }
        self.yield_now = false;
        let state = std::mem::replace(&mut fiber.state, FiberState::Ready);
        // D5 — a fiber resumed after a blocking-native offload carries the pool's result. Take it now
        // (heap is swapped in) and push it below so the suspended `Call` completes and dispatch
        // continues past it.
        let resume_native = fiber.resume_native.take();
        self.pending = fiber.pending.take();
        let disp = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // D5 — lower + push the offloaded native's result before resuming, so the operand stack
            // holds what the `Call` would have pushed and `run_until` continues correctly. The `Err`
            // arm carries a fault from the pool job: either the native PANICKED (caught in the job,
            // surfaced here — faulting the task without running its defers, exactly as an inline
            // native panic does via `run_one_fiber`'s outer `catch_unwind`), or it returned a
            // `HostError` (unreachable for the scoped fns — fs/io surface I/O failures as `Result`
            // *values* and arg types are checker-guaranteed). Either way the task faults.
            // TICKET-188 — a halt observed at the resume (a cancel, or a child fault of a nursery this
            // fiber owns — the timer job ends a sleep on either) goes through `run_until`'s own fault
            // arm, so defers run and a `recover:` outside the faulting nursery still catches a child
            // fault (DEC-096). A value that completed together with the halt is dropped. A hard halt
            // (`--timeout` / over-memory) keeps its own funnel below.
            let resume_native = match resume_native {
                Some(r) if !matches!(&r, Err(e) if executor_hard_halt(e)) => {
                    let span = self.frames.last().map(|f| f.call_span).unwrap_or_default();
                    match self.take_halt(span) {
                        Some(e) => {
                            if let Err(e) = self.on_step_fault(0, e) {
                                return Disp::Finish(self.classify_mn_outcome(Err(e)));
                            }
                            None
                        }
                        None => Some(r),
                    }
                }
                other => other,
            };
            if let Some(result) = resume_native {
                match result {
                    Ok(nr) => {
                        let v = self.lower_native(nr);
                        self.push(v);
                    }
                    Err(rte) => {
                        // W7-16 — an offloaded `sleep_ms` ENDED by a cancel is delivered by the halt
                        // branch above (TICKET-188), so `Cut::Cancelled` is never set here: this arm
                        // is a native fault or a hard halt (`--timeout` / over-memory), which must
                        // never be swallowed into a silent `Cancelled`.
                        // …and it must UNWIND, not merely finish. This arm returns WITHOUT re-entering
                        // `run_until`, so nothing else would run the task's `defer`s — a cancel
                        // delivered mid-sleep would silently skip every registered cleanup, while the
                        // same cancel arriving 50 ms earlier (the entry checkpoint, which faults inside
                        // the VM) runs them. `docs/reference/concurrency/`: "a cancelled task then unwinds
                        // through its `defer`s… 'does my cleanup run?' no longer depends on scheduler
                        // timing". Same shape as `run_until`'s `cancel_bypass` funnel — including
                        // re-stamping the hard-halt markers a mid-unwind `defer` fault would strip, so
                        // `--timeout`/`--max-heap` stay un-catchable. A native PANIC keeps its
                        // documented no-defer behavior: it sets neither the cancel latch nor a marker.
                        let rte = if self.is_cancelled() || rte.is_over_memory || rte.is_timed_out {
                            let (over_mem, timed) = (rte.is_over_memory, rte.is_timed_out);
                            // TICKET-135 (W14-39): as in `run_until`'s cancel bypass, abort the
                            // escaped nurseries of a task whose offloaded sleep a cancel ended.
                            let r = self.unwind_cancelled(0, self.is_cancelled(), rte);
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
                    let d = self.unwind_deferred(0, false);
                    let rte = self.unwind_result(rte, d, false).timed_out();
                    return Disp::Finish(self.classify_mn_outcome(Err(rte)));
                }
                // TICKET-188 — a halt that woke the connect park (a cancel, or a child fault of a
                // nursery this fiber owns) is delivered like the offload resume's, before
                // `finish_pending_connect` could report `Ok` for a handshake still in flight.
                if let Some(e) = self.take_halt(cip.span) {
                    drop(cip);
                    if let Err(e) = self.on_step_fault(0, e) {
                        return Disp::Finish(self.classify_mn_outcome(Err(e)));
                    }
                } else {
                    let v = self.finish_pending_connect(cip);
                    self.push(v);
                }
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
            } else if res.is_ok() && self.join_suspend.is_some() {
                // TICKET-103 — the fiber parked at a fiber-owned nursery's `JoinNursery`.
                Disp::JoinPark(self.join_suspend.take().unwrap())
            } else if res.is_ok() && self.suspend.is_some() {
                let h = self.suspend.take().unwrap();
                // Capture the park key + the channel `Arc` WHILE the fiber heap is live (`h` is a
                // GcRef into it); `park` re-checks the queue through this `Arc` under the sched lock.
                Disp::Park(self.channel_core_ptr(h), self.channel_core(h))
            } else if res.is_ok() && self.send_suspend.is_some() {
                // Bounded backpressure — the fiber blocked on a full `send`. Capture the key + core
                // WHILE the fiber heap is live (like `Disp::Park`); `park_send` re-checks SPACE.
                let h = self.send_suspend.take().unwrap();
                Disp::SendPark(self.channel_core_ptr(h))
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
        fiber.pending = self.pending.take();
        // A fiber that parks carries its site; one that runs on carries `None`.
        fiber.park_site = self.park_site.take();
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
            trace: Vec::new(),
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
        } else if self.is_cancelled() || matches!(self.cut, Some(crate::vm::block::Cut::RunFault)) {
            // TICKET-147 (W14-12) — the cancel funnel replaced the `cancelled` sentinel with a real
            // fault from the task's own `defer` / an aborted nested nursery: report it, ranked below
            // every ordinary fault. No `trip_cancel`: the scope is already cancelled.
            match res {
                Err(err) if std::mem::take(&mut self.cancel_unwind_faulted) => {
                    TaskOutcome::CancelledFault {
                        err,
                        out: std::mem::take(&mut self.out),
                        stderr: std::mem::take(&mut self.stderr),
                    }
                }
                _ => TaskOutcome::Cancelled {
                    out: std::mem::take(&mut self.out),
                    stderr: std::mem::take(&mut self.stderr),
                },
            }
        } else {
            match res {
                Err(e) => {
                    self.trip_cancel();
                    TaskOutcome::Fault {
                        err: e,
                        out: std::mem::take(&mut self.out),
                        stderr: std::mem::take(&mut self.stderr),
                        trace: self.take_fault_trace(),
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
    /// precedence: `Exit` > hard-halt `Fault` > ordinary `Fault` > `CancelledFault` > `Deadlocked`,
    /// lowest index winning within each kind (scan order + `is_none()`). TICKET-147: a
    /// `CancelledFault` (a cancelled task's own `defer` fault) ranks below every ordinary fault —
    /// the cancel's root cause is the more useful report.
    ///
    /// TICKET-195 — a JOIN's reduce: a fault it returns was raised by another party, so this party
    /// adopts it as a delivered cut and reports that party's trace ([`Vm::adopt_child_fault`]).
    pub(super) fn reduce_task_slots(
        &mut self,
        slots: Vec<Option<TaskOutcome>>,
    ) -> Result<(), RuntimeError> {
        let (res, trace) = self.reduce_slots(slots);
        if let (Err(_), Some(trace)) = (&res, trace) {
            self.adopt_child_fault(None, trace);
        }
        res
    }

    /// The reduce itself (see [`Vm::reduce_task_slots`]). Also returns the trace of the slot whose
    /// fault it returns: a `Fault`'s own trace, an empty one for a `CancelledFault`, and `None` for
    /// an exit or a synthesized deadlock.
    pub(super) fn reduce_slots(
        &mut self,
        slots: Vec<Option<TaskOutcome>>,
    ) -> (Result<(), RuntimeError>, Option<Vec<TraceFrame>>) {
        let mut first_exit: Option<i32> = None;
        let mut first_fault: Option<(RuntimeError, Vec<TraceFrame>)> = None;
        // W7-5 review Fix 1: the lowest-index HARD-HALT fault (`executor_hard_halt` —
        // over-memory/timeout), tracked separately from `first_fault` above. It must win
        // final propagation over an earlier ordinary fault, or a later `--max-heap`/`--timeout` abort
        // gets demoted to a catchable error by an earlier sibling's plain fault (letting `recover:`
        // swallow a hard halt it must never be able to catch — `exec.rs`'s cancel-bypass check is
        // keyed on these markers). This does NOT change flush behavior — every fault flushes its
        // buffered output regardless of index (W7-5c) — only which error `reduce_task_slots` returns.
        let mut first_hard_fault: Option<(RuntimeError, Vec<TraceFrame>)> = None;
        let mut deadlock_err: Option<RuntimeError> = None;
        // TICKET-147 — the lowest-index `CancelledFault`; used only when no ordinary fault exists.
        let mut first_cancel_fault: Option<(RuntimeError, Vec<TraceFrame>)> = None;
        for slot in slots {
            // W7-60 — a `None` here means the slot was already drained by `MnSched::take_finished_streams`
            // on a `join_executor` bail-out (its output is flushed, its outcome consumed), which is
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
                TaskOutcome::Fault {
                    err,
                    out,
                    stderr,
                    trace,
                } => {
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
                    if first_hard_fault.is_none() && (executor_hard_halt(&err) || err.is_deadlock) {
                        first_hard_fault = Some((err.clone(), trace.clone()));
                    }
                    self.out.extend_from_slice(&out);
                    self.stderr.extend_from_slice(&stderr);
                    if first_fault.is_none() {
                        first_fault = Some((err, trace));
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
                TaskOutcome::CancelledFault { err, out, stderr } => {
                    self.out.extend_from_slice(&out);
                    self.stderr.extend_from_slice(&stderr);
                    if first_cancel_fault.is_none() {
                        first_cancel_fault = Some((err, Vec::new()));
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
        // index winning within each kind. A deadlock-marked `Fault` (a party's own verdict, e.g. an
        // eager job) ranks with the hard halts (TICKET-135); a synthesized `Deadlocked` slot still
        // ranks last (TICKET-062). This changes ONLY which error propagates; it does not
        // touch the `Exit`-over-`Fault` rule above or the nursery's abort semantics (every fault
        // still trips the shared cancel flag the same way it always did).
        // W7-47 — `first_exit` only ever comes from a SLOT, so an `os.exit` issued by an `Executor`
        // job of another sched is invisible in it, and a nursery whose tasks are all blocked would
        // report a `deadlock` — a confident WRONG verdict, the `parked-is-not-stuck` class. The
        // `run_exit_err` guard below delivers that run-wide exit, inside a `defer` too (TICKET-213).
        // TICKET-208 — the run-wide halt, read through its one funnel after a slot's own exit and
        // before any slot fault: a `recover:` around this nursery must not catch a sibling fault
        // and outlive a job fault that ends the run. Every slot's output is already flushed.
        if first_exit.is_none()
            && let Some(e) = self.run_exit_err(Span::RUNTIME)
        {
            return (Err(e), None);
        }
        let fault = first_hard_fault.or(first_fault).or(first_cancel_fault);
        match (first_exit, fault, deadlock_err) {
            // A child `os.exit` hard-halts the parent: set `pending_exit` and return the exit
            // sentinel. The op→`step`→`run_until` chain sees `pending_exit` and unwinds past every
            // `recover:` to the driver, which reports `code` as the process exit status (decision C).
            // It wins over any sibling fault — a hard halt is never demoted to a catchable error.
            (Some(code), _, _) => {
                self.pending_exit = Some(code);
                (Err(self.err("exit".to_string(), Span::RUNTIME)), None)
            }
            // A real fault propagates normally so an outer `recover:` can still catch it (unless it
            // carries a hard-halt marker, in which case `first_hard_fault` already selected it above
            // and `exec.rs`'s cancel-bypass check keeps `recover:` from swallowing it anyway).
            (None, Some((e, trace)), _) => (Err(e), Some(trace)),
            // Deadlock abort: all parked buffers already flushed above; propagate ONE deadlock error.
            (None, None, Some(e)) => (Err(e), None),
            (None, None, None) => (Ok(()), None),
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
            | Obj::Module(_)
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Reader(_)
            | Obj::Generator(_) => unreachable!("channel_core_ptr on a non-channel park handle"),
        }
    }

    /// D0 — a `send` into channel `h` may unblock fibers parked on its `recv`. Wake the matching
    /// bucket on every live `MnSched`.
    pub(super) fn wake_on_send(&mut self, h: GcRef) {
        let key = self.channel_core_ptr(h);
        self.wake_on_send_key(key);
    }

    /// Same as [`Vm::wake_on_send`], keyed directly (TICKET-028 — a caller holding only the
    /// `Arc<ChannelCore>`, never a `GcRef`, still needs this registry walk).
    pub(super) fn wake_on_send_key(&mut self, key: usize) {
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
        self.wake_on_send_key_kind(key, WakeKind::All);
    }

    /// Same as [`Vm::wake_on_send_key`] but with the wake kind carried by the caller (TICKET-117,
    /// TICKET-185 — the receiver-side wake passes `WakeKind::Settled` on every cap; every
    /// sender/close/trip caller keeps going through `wake_on_send_key` above, which stays
    /// `WakeKind::All`).
    pub(super) fn wake_on_send_key_kind(&mut self, key: usize, kind: WakeKind) {
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
                    s.wake_key(key, kind);
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

    /// W7-57 — the teardown half of a run end, after its cell is stored: halt every live sched,
    /// then latch the hint, so `true` publishes the teardown to whoever reads it. The host
    /// `request_exit` and the `Disp::Finish` arm (a job fault, `MnSched::publish_job_fault`) both
    /// run it.
    pub(super) fn halt_run(&self) {
        self.halt_all_scheds();
        self.quiesce.mark_run_halt();
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
    /// **The gate is deliberately NOT `BlockCtx::judged`.** That is false for
    /// `BlockCtx::Builder` (`mn_enlist_sched.is_some()`), which is the very builder that owns an
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
            self.quiesce.block(
                crate::vm::quiesce::PartyWait::Nursery(Arc::clone(sched)),
                self.wake_set(),
            )
        })
    }

    /// TICKET-095 — true on the exact complement of `nursery_party_guard`'s gate: `self` is a fiber
    /// running ON `sched` itself (the `(Some(sched), _)` arm of `run_mn_nursery`), not the top-level
    /// late-spawn builder (the `(None, Some(held))` arm, which `nursery_party_guard` covers instead).
    pub(super) fn owns_nested_sched(&self, sched: &Arc<MnSched>) -> bool {
        self.mn.as_ref().is_some_and(|m| Arc::ptr_eq(m, sched))
    }

    /// TICKET-095 — counts a fiber whose OS thread is about to block in a NESTED nursery join as a
    /// `SchedCore::blocked_owners` ON ITS OWN SCHED, so `is_deadlocked_ignoring_jobs`'s `running ==
    /// blocked_owners` clause can tell "stuck by construction" from "genuinely running". `None` on
    /// the `(None, Some(held))` late-spawn arm — that owner is not counted in `running` either, which
    /// is exactly the `self.mn.is_none()` case.
    ///
    /// TICKET-099 widened this from the same-sched case only: a fiber blocked at a nested join counts
    /// as a `blocked_owner` on ITS OWN sched (`self.mn`) whether the child nursery it is waiting on
    /// (`sched`) runs on that same sched (`owns_nested_sched`) or on a private eager one of its own.
    /// TICKET-099's peer veto (`MnSched::peer_can_move`) asks a PEER sched "can you still feed me?",
    /// and a fiber waiting on a join can feed nobody, whichever sched its child runs on — the same
    /// distinction `local_quiesced`'s §2c1 comment already draws between `any_body_injecting` and
    /// `any_body_open`. Without this widening the outer sched of a nested nursery never counts its
    /// join-blocked fiber as a `blocked_owner`, so it never reads `local_quiesced`, so a peer veto
    /// against it never lifts and a genuine nested deadlock hangs forever instead of faulting
    /// (measured: `parity_nested_deadlock_cancels_the_outer_parked_siblings_defer` at `fdc71d70`).
    ///
    /// TICKET-103 — it also counts the owner on its OWN scope (`JoinScope::owners_blocked`, from
    /// `self.fiber_scope`) and, when `joined` is `Some`, the nursery it joins on this same sched
    /// (`JoinScope::joins_blocked` on that nursery's origin scope). `flag_deadlock_leaves` reads
    /// both. `owner` is `self.mn` in both branches, so `fiber_scope` indexes the right sched.
    pub(super) fn blocked_owner_guard(
        &self,
        sched: &Arc<MnSched>,
        joined: Option<usize>,
    ) -> Option<BlockedOwnerGuard> {
        let (owner, cross_sched) = if self.owns_nested_sched(sched) {
            (Arc::clone(sched), false)
        } else {
            let own = self.mn.clone()?;
            if Arc::ptr_eq(&own, sched) {
                return None;
            }
            (own, true)
        };
        {
            let mut c = owner.lock();
            c.owner_blocked(cross_sched);
            if let Some(s) = self.fiber_scope {
                c.scopes[s].owners_blocked += 1;
            }
            if let Some(j) = joined {
                debug_assert!(
                    !cross_sched,
                    "a joined scope lives on the owner's own sched"
                );
                c.scopes[j].joins_blocked += 1;
            }
        }
        owner.notify_waiters();
        Some(BlockedOwnerGuard {
            sched: owner,
            cross_sched,
            scope: self.fiber_scope,
            joined,
        })
    }

    /// `chezzi test --max-heap` — collect and read the cap, at a point that is NOT an instruction
    /// boundary. Callers must have every live value of the pending call already on `self.stack`.
    ///
    /// W6-10s residual (a), closed: `over_cap` is assigned only in `Heap::sweep()`, `sweep()` runs
    /// only when `should_collect()` fires, and the only non-test caller of `should_collect()` is the
    /// top of [`run_until`](Vm::run_until)'s dispatch loop — whose guard is `self.frames.len() >
    /// base_level`. When this was written (W7-29) a task whose entire body was one native call
    /// pushed no frame, so it never entered that loop and its heap was never sampled. Measured then
    /// on the release binary: `spawn xs.len()` over a 300 K-element `List[str]` PASSED a 8 MB cap at
    /// 170.9 MB (21×), while the byte-identical `spawn use(xs)` (`use` = `return xs.len()`, one
    /// bytecode frame) correctly reported OVER-MEMORY at the same RSS. The worker's copy is the
    /// BIGGER one, too: `from_wire` mints a fresh `Obj::Str` per element and does not re-intern, so
    /// N aliases of one interned literal become N objects.
    ///
    /// TICKET-235: every task has an entry frame now ([`Vm::task_entry`]), so such a body does
    /// reach an instruction boundary. The sample stays: a worker heap is born over the cap in few
    /// objects and `should_collect` counts objects. Removing it needs its own measurement against
    /// `over_memory_trips_on_an_all_native_task_body`.
    ///
    /// SCOPE — [`start_task`](Vm::start_task), which every task is dispatched through: `spawn`,
    /// `parallel:` and `Executor` jobs alike. `Heap::request_collect` (set in
    /// [`spawn_worker`](Vm::spawn_worker)) is the second mechanism; read the note at that call
    /// site before deleting either.
    ///
    /// COST CEILING: this is a FULL mark-sweep of the heap the fiber runs on, once per task start.
    /// The heap is the M:N worker's own and freshly born, so it is small. (An engine where every fiber
    /// shared ONE heap would make a capped run O(live heap × tasks); the cooperative engine that did
    /// that has been removed, and `--max-heap` was refused with it at the CLI anyway. If a
    /// shared-heap execution mode is ever reintroduced, this is the line that needs a cheaper
    /// trigger.)
    ///
    /// **Collecting before the task's entry frame is pushed is sound.** The sample runs in
    /// `start_task` ahead of `invoke_value`, so `self.frames` is still empty here. Empty frames stop
    /// `run_until`'s *loop*; they do not make a direct [`collect`](Vm::collect) unsafe. `collect`
    /// roots the operand stack first (`for v in &self.stack`), plus the swapped-in child's module
    /// objects, intern cache and snapshot registry — the same root set the task's first instruction
    /// boundary would see, minus frame slots that do not exist yet. That is why the contract above
    /// is "operands on the stack".
    ///
    /// No `unwind_deferred`: at this point no frame is pushed and no `defer` is registered. The
    /// `is_over_memory` marker alone is what buckets the verdict `OverMemory` and makes it
    /// uncatchable by `recover:`.
    pub(super) fn sample_mem_cap(&mut self, span: Span) -> Result<(), RuntimeError> {
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
}
