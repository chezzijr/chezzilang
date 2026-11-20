//! VM: taking runnable fibers, join parks and completed-join wakes, unproven-fault leaves.
//! Split out of `src/vm/mnsched.rs`. `super::*` is the parent module.

use super::*;

impl MnSched {
    /// Pop a runnable fiber for worker `wid`, marking it `running`. Search order (D4b): the worker's
    /// own `locals[wid]` (lock class **B**, taken alone), then the shared global queue (core lock).
    /// With none runnable, park this worker on `cv` until a `send`/finish/yield makes progress — or
    /// return `Stop` when the nursery is done, deadlocked, or terminated. The local queue is checked
    /// **before** the core lock and released first, so the lock order is always B-then-A (never the
    /// reverse) → no ABBA. The deadlock predicate reads the authoritative `runnable` count (no single
    /// queue to test under the split); `running == 0` is the key — while any fiber runs it might
    /// `send`/`yield` and make a sibling runnable, so an idle worker waits rather than declaring
    /// deadlock. (D4c will insert work-stealing passes between the local and the park.)
    ///
    /// M1.5 — `vm` is the worker shell that calls this, with no fiber swapped in: each wait here
    /// runs through its GC door ([`Vm::wait_door`] with no spec), so a stop never waits for an idle
    /// worker. Each pass releases the permit at its top and takes one only for work (TICKET-230).
    pub(super) fn take_runnable(
        &self,
        vm: &mut Vm,
        wid: usize,
        tick: u64,
        scope_id: usize,
    ) -> Take {
        let mut spun = false;
        let t = self.take_runnable_inner(vm, wid, tick, scope_id, &mut spun);
        #[cfg(test)]
        if matches!(t, Take::Run(..)) {
            self.picks.fetch_add(1, Ordering::Relaxed);
        }
        // TICKET-128 (W13-25) — a worker that had to spin before it found this fiber may itself be
        // the one that just consumed a periodic global pull ahead of its own `runnext` (step 0
        // below), or may simply mean other idle siblings should be nudged awake now that there was
        // contention. Recruiting here is a cheap top-up on top of `handoff_wake`'s own recruit.
        if spun && matches!(t, Take::Run(..)) && self.runnable.load(Ordering::Relaxed) > 0 {
            self.recruit();
        }
        t
    }

    /// Take this thread's slot off this sched's idle list, with any ticket reserved for it. A
    /// visit to a guest ([`Vm::visit_guest`]) does this first: the visit lists the thread on the
    /// guest, and a host that still listed it would reserve turns it never takes.
    fn idle_unlist(&self) {
        if !width::listed() {
            return;
        }
        let me = width::my_slot();
        let mut g = self.gated_idle.lock().unwrap_or_else(|e| e.into_inner());
        g.retain(|s| !Arc::ptr_eq(s, &me));
        me.set_listed(false);
        drop(g);
        width::cancel(&me);
    }

    /// TICKET-128 (W13-25) — the body of [`Self::take_runnable`], split out so the wrapper can track
    /// whether this call spun (`spun`) before returning. See `take_runnable` for the recruit this
    /// enables.
    /// TICKET-167 — seeded-mode pop off the global queue: uniformly at random instead of FIFO front.
    /// Every requeue onto `global` uses `push_back`, so randomizing the pop side also covers timer
    /// and netpoll completion order (see `## Digest` "Global pick"). `None` on an empty queue.
    fn pop_global(&self, c: &mut SchedCore) -> Option<Fiber> {
        if sched_seed::on() && !c.global.is_empty() {
            let k = self.rng.below(c.global.len() as u64) as usize;
            c.global.remove(k)
        } else {
            c.global.pop_front()
        }
    }

    fn take_runnable_inner(
        &self,
        vm: &mut Vm,
        wid: usize,
        tick: u64,
        scope_id: usize,
        spun: &mut bool,
    ) -> Take {
        // TICKET-213 — an idle Executor runner has already waited once for a next job.
        let mut lingered = false;
        struct IdleReg<'a>(&'a MnSched);
        impl Drop for IdleReg<'_> {
            fn drop(&mut self) {
                self.0.idle_unlist();
            }
        }
        let _idle = IdleReg(self);
        // A root worker visits the run's Executor scheds at most once per take on the fairness
        // tick below (an idle one visits whenever one has a fiber runnable).
        let mut visited = false;
        loop {
            // 0. D4d — every `GLOBAL_CHECK_INTERVAL`th schedule, pull from the global queue FIRST
            //    (before own local / stealing). Without this a worker continuously refilled by
            //    stealing a busy sibling's local could leave older global work waiting; the periodic
            //    pull bounds that latency (Go's `schedtick % 61`). A single fiber is enough — it is a
            //    fairness nudge, not the main grab path.
            //    TICKET-128 (W13-25) — this pulled fiber runs AHEAD of this worker's own `runnext`
            //    and may block its thread in a `Kind::Inline` native, so if `runnext` is occupied an
            //    idle worker is recruited to steal it (nobody else is left to run it otherwise).
            // TICKET-230 — an idle unseeded worker takes a FREE permit to look for work, and picks
            // whenever it holds one: an idle worker whose relaxed `runnable` read was 0 skipped work
            // it could take (`send_one_channel` T=0 1.37 s vs base 0.76 s). Seeded mode keeps the
            // old pick rule: an idle holder must not draw from the seeded RNG (`try_steal`, the
            // global-first coin) with nothing runnable (DEC-205, DEC-209).
            if !width::holds() && (self.runnable.load(Ordering::Relaxed) > 0 || width::reserved()) {
                vm.width_acquire();
            } else if !sched_seed::on() {
                width::try_acquire();
            }
            let may_pick =
                width::holds() && (self.runnable.load(Ordering::Relaxed) > 0 || !sched_seed::on());
            let global_first = may_pick
                && if sched_seed::on() {
                    self.rng.below(8) == 0
                } else {
                    tick.is_multiple_of(GLOBAL_CHECK_INTERVAL)
                };
            if global_first {
                let mut seen = Vec::new();
                let mut c = self.lock();
                // The run's Executor jobs share this root's workers: on the fairness tick a busy
                // root worker runs them too, as an idle one does below.
                if !visited
                    && c.root
                    && let Some(guest) = self.quiesce.runnable_guest(&mut seen, None)
                {
                    drop(c);
                    visited = true;
                    self.idle_unlist();
                    if vm.visit_guest(&guest, wid) {
                        width::release();
                        return Take::Stop;
                    }
                    continue;
                }
                if let Some(f) = self.pop_global(&mut c) {
                    c.running += 1;
                    self.runnable.fetch_sub(1, Ordering::Relaxed); // runnable → running
                    drop(c);
                    if self.lock_local(wid).runnext.is_some() {
                        self.recruit();
                    }
                    return Take::Run(f, Slice::Fresh);
                }
                drop(c);
            }
            // 1. Own local queue — lock B alone, release before touching the core lock.
            let popped = if !may_pick {
                None
            } else if sched_seed::on() {
                self.lock_local(wid).pop_seeded(&self.rng)
            } else {
                self.lock_local(wid).pop()
            };
            if let Some((f, slice)) = popped {
                let mut c = self.lock();
                c.running += 1;
                self.runnable.fetch_sub(1, Ordering::Relaxed); // runnable → running
                return Take::Run(f, slice);
            }
            // 2. D4c — work-stealing: own local empty, try to steal half from a sibling (B alone, no
            //    core lock). Push the haul onto our own local and re-loop to pop it. Net-zero on
            //    `runnable`, so this never perturbs the deadlock predicate.
            let stolen = if may_pick {
                self.try_steal(wid)
            } else {
                Vec::new()
            };
            if !stolen.is_empty() {
                let mut lq = self.lock_local(wid);
                for f in stolen {
                    lq.ring.push_back(f);
                }
                continue;
            }
            // 3. Global queue (batch-grab) + termination/deadlock + park (core lock A).
            let mut seen = Vec::new();
            let mut c = self.lock();
            if c.terminate {
                c.release_runner(wid);
                width::release();
                return Take::Stop;
            }
            if !may_pick && self.runnable.load(Ordering::Relaxed) > 0 {
                drop(c);
                continue;
            }
            if !c.global.is_empty() {
                // D4c — Go-style `globrunqget`: grab a *capped* batch into our own local (one core-lock
                // acquisition amortized over the whole batch — the contention win), run the first, and
                // leave the rest in the global queue for sibling workers (cap `g/nworkers + 1` so we
                // don't hoard). The first transitions runnable→running (−1); the extras stay runnable
                // (global→local, net-zero). The extras are pushed to the local AFTER releasing the core
                // lock (order A-then-release-then-B → never B-while-holding-A).
                let g = c.global.len();
                let take = (g / self.locals.len() + 1).min(g).min(LOCAL_RING_CAP / 2);
                let first = self.pop_global(&mut c).unwrap();
                c.running += 1;
                self.runnable.fetch_sub(1, Ordering::Relaxed); // first: runnable → running
                let extra: Vec<Fiber> = (1..take).filter_map(|_| self.pop_global(&mut c)).collect();
                drop(c);
                if !extra.is_empty() {
                    {
                        let mut lq = self.lock_local(wid);
                        for f in extra {
                            lq.ring.push_back(f);
                        }
                    }
                    // D4c — the surplus is now stealable by idle siblings, but it landed in a local
                    // with no accompanying `notify_all` (unlike the global-queue requeue paths). The
                    // surplus IS counted in `runnable` (the global→local move is net-zero), so under
                    // D4e an idle sibling that races a park here observes `runnable > 0` and spins to
                    // steal it rather than truly sleeping — this `notify_all` additionally wakes any
                    // sibling that was already in a real `cv.wait` (parked when `runnable` was 0, e.g.
                    // before this batch was produced) so it re-checks and steals promptly. Notify
                    // after releasing the local lock (B) — `cv` is the core's, not held here.
                    self.notify_waiters();
                }
                return Take::Run(first, Slice::Fresh);
            }
            // A visit to an Executor's sched ends when it has nothing runnable: the visitor goes
            // back to its host ([`Vm::visit_guest`]), whose idle wait covers this sched's wakes.
            if vm.guest_of.is_some() {
                width::release();
                return Take::Stop;
            }
            // The run's Executors have no threads of their own: an idle root worker runs their
            // runnable jobs. The read is under this lock, which a guest's wake takes before it
            // notifies (`MnSched::wake_host`), so a job made runnable after it is not missed.
            if c.root
                && let Some(guest) = self.quiesce.runnable_guest(&mut seen, None)
            {
                drop(c);
                width::release();
                self.idle_unlist();
                if vm.visit_guest(&guest, wid) {
                    width::release();
                    return Take::Stop;
                }
                continue;
            }
            // Cross-nursery flat scheduler — SCOPE-SCOPED owner stop. A nested nursery's inline OWNER
            // (passed its own `scope_id`) returns `Stop` when ITS OWN scope is complete (`done == total`
            // && !body_open), EVEN while other scopes still have work — because its OS thread drains the
            // GLOBAL queue (it ran cross-nursery siblings while alive), the moment its scope is done it
            // must return so its join (`Vm::join_fiber_owned_nursery`) goes back to its caller (an
            // OUTER fiber whose continuation may unblock the rest). It does NOT set global
            // `terminate` (that would stop farmed helpers other scopes still need). A FARMED helper
            // passes the SENTINEL scope_id (`usize::MAX`) and never self-stops — it keeps draining
            // the global queue until global `terminate` (set by `finish` only when ALL scopes are done, or by deadlock/fault/exit).
            // `body_open` (eager) holds the scope open against a transient `done == total`. Single-scope
            // fast path: an outermost owner with one scope behaves exactly like the old `done == total`.
            // The owner stops only when its whole FAMILY — the origin scope plus any TICKET-103
            // continuation scopes sharing its cancel token — is done, not the origin alone
            // (`SchedCore::owner_scope_done`, TICKET-128/W13-25).
            // TICKET-230 - a waker reserves a ticket only for a listed slot and only when the gate
            // is full. With two or more permits free the woken worker takes one itself, and
            // listing every idle worker cost `send_one_channel` T=0 about 3% (one `gated_idle`
            // lock pair and one timer per idle wait). The read comes before the release so a
            // cap-1 worker always lists (seeded T=1 replay unchanged).
            if width::tight() {
                self.idle_register(&width::my_slot());
            }
            width::release();
            // TICKET-208 — an Executor holds no thread while it is idle: its runner leaves here and
            // gives its wid back under this lock, so `submit` starts a new one.
            // TICKET-213 — but not at once: it waits `EXEC_RUNNER_LINGER` for the next job first,
            // with its wid still claimed, so a submit-then-wait loop reuses one runner instead of
            // starting a thread per job. `admit_or_hold` notifies this `cv`; the leave decision is
            // re-made under the lock on the next pass, so a job enqueued during the wait is never
            // left without a runner. `terminate` is read at the top of step 3 and ends the wait.
            if self.detached.is_some() && c.all_scopes_done() {
                if !lingered {
                    lingered = true;
                    vm.wait_door(&[], |_| {
                        let (guard, _) = self
                            .cv
                            .wait_timeout(c, EXEC_RUNNER_LINGER)
                            .unwrap_or_else(|e| e.into_inner());
                        drop(guard);
                    });
                    continue;
                }
                c.release_runner(wid);
                return Take::Stop;
            }
            if scope_id != SENTINEL_SCOPE && c.owner_scope_done(scope_id) {
                self.notify_waiters();
                return Take::Stop;
            }
            // TICKET-118 (W13-8) — drain a cancelled family's parked fibers before this worker
            // sleeps, so a `cancel` that trips an ANCESTOR flag a parked fiber's own re-check can't
            // see (`SchedCore::scope_cancel_tripped`, only read by `park`/`park_send`/`park_wait`)
            // still requeues that fiber. `cancel_drain` moves the family's parks to `global`; each
            // requeued fiber re-checks the same flags at its recv/send/`wait:` checkpoint and
            // unwinds; a fiber parked in a `defer` does not honour the cancel
            // ([`Fiber::honours_cancel`]), so it is not requeued for it; a park landing after the
            // trip is caught by step 8's own re-check, not this scan.
            // TICKET-126 — gated by `CANCEL_GEN` (see `SchedCore::drain_scan_due`); the generation
            // read is this pass's one cancel read.
            if let Some(sid) = c.drain_scan_due(self) {
                drop(c);
                self.drain_family(sid);
                continue;
            }
            // D4e — runnable-gated park (replaces the D4c bounded `wait_timeout` poll). `runnable`
            // counts every fiber queued in the global queue OR any worker local (batch-grab and steal
            // move fibers between queues net-zero, so a fiber sitting in a local stays counted), PLUS
            // the sub-microsecond in-hand `Vec` window of a concurrent grab/steal (popped from one
            // queue, not yet pushed to the next). So `runnable > 0` means work exists somewhere
            // reachable-now (in a local, stealable) or reachable-imminently (the in-hand window): do
            // NOT truly sleep — we must re-loop and re-steal/re-grab. But do NOT busy-spin either: a
            // brief `wait_timeout` backoff (`SPIN_BACKOFF`) that any wake `notify_all` cuts short, then
            // `continue`. Busy-spinning (`drop; yield_now; continue`) was the first cut, but the D4e
            // SRE review flagged that (W−1) idle workers in a fan-out-then-quiesce burst would then
            // hammer core lock A across the in-hand window, and on an oversubscribed host the spinners
            // would starve the very worker holding the surplus of CPU — *widening* the window. A
            // sleeping waiter does neither, and the surplus-push `notify_all` (above) wakes it the
            // instant the work lands, so there is no added latency on the common path; the timeout is
            // only a backstop for a wake that fired just before we waited. `runnable == 0` means no
            // fiber is queued anywhere: park for real on `cv` with NO timeout — a true sleep, woken
            // only by a sibling's `send`/`yield`/`finish`/offload-complete `notify`.
            //
            // Lost-wakeup-free: every site that makes a fiber runnable does `runnable.fetch_add` while
            // holding THIS core lock and then `notify`s; this worker reads `runnable` under the same
            // lock immediately before it waits, and `cv.wait`/`wait_timeout` atomically
            // releases-and-enqueues before any notifier can re-acquire the lock. So a
            // `runnable++`/`notify` either happens-before this read (we observe `runnable > 0`, take
            // the backoff branch, and re-steal) or after the wait is registered (the `notify` reaches
            // us). The mutex serializes the publish against the observe — it IS the StoreLoad barrier,
            // so Go's lockless `nmspinning` fence is unnecessary here (chezzi's precise `runnable`
            // atomic is the reachability oracle Go lacks). Terminate/deadlock/cancel still broadcast
            // via `notify_all`, which wakes these true sleepers to exit/unwind.
            if self.runnable.load(Ordering::Relaxed) > 0 {
                self.spinning.fetch_add(1, Ordering::Relaxed);
                *spun = true;
                vm.wait_door(&[], |_| {
                    let (guard, _) = self
                        .cv
                        .wait_timeout(c, SPIN_BACKOFF)
                        .unwrap_or_else(|e| e.into_inner());
                    drop(guard);
                });
                self.spinning.fetch_sub(1, Ordering::Relaxed);
                continue;
            }
            debug_assert!(
                c.global.is_empty(),
                "D4e park invariant: runnable==0 but the global queue is non-empty"
            );
            // TICKET-128 (W13-25) — a worker with NOTHING runnable sleeps on `idle_cv`, not `cv`,
            // so a rendezvous handoff can recruit exactly this one sleeper (`recruit`) instead of
            // broadcasting to every parked worker. `notify_waiters` still reaches this wait; only
            // a bare `cv.notify_all()`/`notify_one()` would miss it.
            // TICKET-205 — a waker reserves this thread's permit ticket WITHOUT `c`, so a ticket
            // can land between the checks above and this wait, and the waker's `idle_sleepers`
            // read can miss it. A LISTED sleeper therefore never sleeps untimed: a ticket can land
            // without `c`. TICKET-230 - an unlisted sleeper can receive no ticket, so it sleeps
            // untimed on `idle_cv` as before TICKET-230, and every wake reaches it through
            // `notify_waiters` or `recruit`.
            if !width::listed() {
                vm.wait_door(&[], |_| {
                    self.idle_sleepers.fetch_add(1, Ordering::Relaxed);
                    let guard = self.idle_cv.wait(c).unwrap_or_else(|e| e.into_inner());
                    self.idle_sleepers.fetch_sub(1, Ordering::Relaxed);
                    drop(guard);
                });
            } else if !width::reserved() {
                vm.wait_door(&[], |_| {
                    self.idle_sleepers.fetch_add(1, Ordering::Relaxed);
                    let (guard, _) = self
                        .idle_cv
                        .wait_timeout(c, DEMOTE_POLL_BACKOFF)
                        .unwrap_or_else(|e| e.into_inner());
                    self.idle_sleepers.fetch_sub(1, Ordering::Relaxed);
                    drop(guard);
                });
            }
        }
    }

    /// TICKET-103 — file the running fiber as join-parked on the fiber-owned nursery whose origin
    /// scope is `origin` (`Disp::JoinPark`), freeing the worker. Closes the park gap the way `park`
    /// does: a family that completed between `join_nursery`'s check and this call requeues the
    /// fiber at once, under the same lock `finish` bumps `done` under.
    pub(super) fn park_join(&self, mut fiber: Fiber, origin: usize) {
        let mut c = self.lock();
        c.leave_running();
        if c.family_done(origin) {
            fiber.state = FiberState::Ready;
            c.global.push_back(fiber);
            self.runnable.fetch_add(1, Ordering::Relaxed); // running → ready (requeued)
        } else {
            fiber.state = FiberState::Blocked; // running → join-parked: runnable unchanged
            c.join_parked.push((origin, fiber));
            if c.awake.asleep() {
                drop(c);
                self.on_deadlock();
                return;
            }
            // Nothing became runnable: only the `cv` waiters re-check (an idle worker has no
            // work to find, and a broadcast to every idle runner of the run is a herd).
            drop(c);
            self.cv.notify_all();
            return;
        }
        drop(c);
        self.notify_waiters();
    }

    /// TICKET-103 — requeue every join-parked owner whose joined family is now complete. Returns how
    /// many it requeued; a caller that does not already notify must notify `cv` when nonzero.
    pub(super) fn wake_completed_joins(&self, c: &mut SchedCore) -> usize {
        let mut woken = 0;
        let mut i = 0;
        while i < c.join_parked.len() {
            if !c.family_done(c.join_parked[i].0) {
                i += 1;
                continue;
            }
            let (_, mut f) = c.join_parked.remove(i);
            f.state = FiberState::Ready;
            c.global.push_back(f);
            self.runnable.fetch_add(1, Ordering::Relaxed);
            woken += 1;
        }
        c.awake.woken(woken);
        woken
    }
}
