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
    pub(super) fn take_runnable(&self, wid: usize, tick: u64, scope_id: usize) -> Take {
        let mut spun = false;
        let t = self.take_runnable_inner(wid, tick, scope_id, &mut spun);
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

    fn take_runnable_inner(&self, wid: usize, tick: u64, scope_id: usize, spun: &mut bool) -> Take {
        // gaps.md W7-58 — "this worker has already asked the process-wide verdict since its last
        // wait". Bounds the (relatively expensive, `parties`-locking) escalation below to ONE call per
        // wait, so it can never spin. It is NOT a claim that the verdict's inputs only move on a
        // notify — they do not, which is why the idle wait below is BOUNDED whenever this is set (see
        // there). Cleared at every wait site.
        let mut judged = false;
        // TICKET-213 — an idle Executor runner has already waited once for a next job.
        let mut lingered = false;
        let mut idle_since = None;
        let me = std::thread::current().id();
        struct IdleReg<'a>(&'a MnSched);
        impl Drop for IdleReg<'_> {
            fn drop(&mut self) {
                if !width::listed() {
                    return;
                }
                let me = width::my_slot();
                let mut g = self.0.gated_idle.lock().unwrap_or_else(|e| e.into_inner());
                g.retain(|s| !Arc::ptr_eq(s, &me));
                me.set_listed(false);
                drop(g);
                width::cancel(&me);
            }
        }
        let _idle = IdleReg(self);
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
                width::acquire();
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
                let mut c = self.lock();
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
            // Cross-nursery flat scheduler — SCOPE-SCOPED owner stop. A nested nursery's inline OWNER
            // (passed its own `scope_id`) returns `Stop` when ITS OWN scope is complete (`done == total`
            // && !body_open), EVEN while other scopes still have work — because its OS thread drains the
            // GLOBAL queue (it ran cross-nursery siblings while alive), the moment its scope is done it
            // must return so the nested `run_mn_nursery` unwinds back to its caller (an OUTER fiber whose
            // continuation may unblock the rest). It does NOT set global `terminate` (that would stop
            // farmed helpers other scopes still need). A FARMED helper passes the SENTINEL scope_id
            // (`usize::MAX`) and never self-stops — it keeps draining the global queue until global
            // `terminate` (set by `finish` only when ALL scopes are done, or by deadlock/fault/exit).
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
                    let (guard, _) = self
                        .cv
                        .wait_timeout(c, EXEC_RUNNER_LINGER)
                        .unwrap_or_else(|e| e.into_inner());
                    drop(guard);
                    continue;
                }
                c.release_runner(wid);
                return Take::Stop;
            }
            if scope_id != SENTINEL_SCOPE && c.owner_scope_done(scope_id) {
                self.notify_waiters();
                return Take::Stop;
            }
            // TICKET-118 (W13-8) — drain a cancelled family's parked fibers BEFORE judging a
            // deadlock, so a `cancel` (or any scope-cancel) that trips an ANCESTOR flag a
            // parked fiber's own re-check can't see (`SchedCore::scope_cancel_tripped`, only read by
            // `park`/`park_send`/`park_wait`, not by this predicate scan) still requeues that fiber
            // instead of getting faulted as deadlocked. `cancel_drain` moves the family's parks to
            // `global`; each requeued fiber re-checks the same flags at its recv/send/`wait:`
            // checkpoint and unwinds; a `defer` body demotes and never parks so it is unaffected; a
            // park landing after the trip is caught by step 8's own re-check, not this scan. ONE read
            // per lock hold: a trip landing between two separate reads could let this scan say
            // `false` while the N4 veto inside `is_deadlocked`/`local_quiesced` said `true` (or vice
            // versa), parking the worker on a stale verdict (TICKET-118 plan-validation, 2026-09-15
            // 10:59Z) — `awaiting_drain` threads this ONE read into both calls below instead.
            // TICKET-126 — gated by `CANCEL_GEN` (see `SchedCore::drain_scan_due`); the generation
            // read is this pass's one cancel read.
            if let Some(sid) = c.drain_scan_due(self) {
                drop(c);
                self.drain_family(sid);
                continue;
            }
            let awaiting_drain = Some(false);
            // D4a — deadlock predicate reads the authoritative `runnable` count rather than
            // `global.is_empty()`: under the split queues there is no single queue to test, but
            // `runnable == 0` means no fiber is queued in any local or the global. Sound because we
            // hold the core lock and `running == 0` excludes the only out-of-lock mutator (a running
            // worker's local push/steal), so no fiber can be in flight to become runnable.
            if self.is_deadlocked_given(&c, awaiting_drain) {
                // TICKET-232 — latch the verdict before any leaf is flagged; the lock is dropped
                // for it, so the verdict is re-derived under the new hold.
                // TICKET-236 — the licence is the one `flag_leaves_and_wake` cuts under. `None`
                // is a declined licence or a moved epoch: wait (timed), never spin on it.
                drop(c);
                let Some(relocked) =
                    self.latch_own_verdict(|c| c.victims_proven() || self.may_fault_unproven())
                else {
                    let (guard, _) = self
                        .cv
                        .wait_timeout(self.lock(), DEMOTE_POLL_BACKOFF)
                        .unwrap_or_else(|e| e.into_inner());
                    drop(guard);
                    judged = false;
                    continue;
                };
                c = relocked;
                // TICKET-103 — fault joined leaves first. A non-terminating flag `continue`s: a
                // SENTINEL helper treats `Stop` as exit-forever, and the leaf's inline owner reaches
                // its scope-scoped owner stop on the next pass.
                // TICKET-129 (W13-5 residual) — `None` means the verdict is unproven and no peer
                // licenses it yet: wait (timed — `judged = false` so the next pass re-derives the
                // W7-58 verdict too), never spin on the same declined judgement.
                let Some(done) = self.flag_leaves_and_wake(&mut c) else {
                    let (guard, _) = self
                        .cv
                        .wait_timeout(c, DEMOTE_POLL_BACKOFF)
                        .unwrap_or_else(|e| e.into_inner());
                    drop(guard);
                    judged = false;
                    continue;
                };
                self.notify_waiters();
                if done {
                    return Take::Stop;
                }
                continue;
            }
            // gaps.md W7-58 — THE NURSERY OWNER'S JUDGE. The gate above declined because a job is
            // outstanding (W7-56). That veto is right when the job is RUNNING, and wrong when the job
            // is itself blocked forever — which is exactly W7-58's shape. The process-wide verdict can
            // tell the two apart (a stuck job is a registered party with an unsatisfiable wait; a
            // running one is unregistered, and `parties.len() < live` then vetoes), but ONLY a
            // counted party polling `block_halt_check` ever asks it — and a nursery owner never
            // reaches that call, it sits in this loop. So an idle worker asks on its behalf.
            //
            // **`drop(c)` is the most dangerous line in this change.** `quiesced` takes `parties` (P)
            // and then, through `PartyWait::Nursery::satisfiable`, this same core lock (A). The one
            // total order is P → A, so the guard MUST be released first; holding it here is a real
            // hang, not a style nit. Everything below the gap is therefore re-derived: `continue`
            // rather than fall through, so `c.terminate` and the queue gates at the top of the loop
            // are re-evaluated rather than skipped with a stale verdict (a lost wakeup otherwise).
            if !judged
                && !self.body_held_by_fiber(&c)
                && self.local_quiesced_given(&c, awaiting_drain)
            {
                // TICKET-099 — `judged` is set BEFORE the peer veto below, not after. It is what
                // selects the timed `DEMOTE_POLL_BACKOFF` park further down over the untimed
                // `self.cv.wait(c)` — and a vetoed sched needs that timed park: nothing notifies this
                // sched's `cv` when a PEER quiesces later, so leaving `judged` false here would let a
                // vetoed sched sleep forever with nothing left to re-examine it.
                judged = true;
                if self.any_peer_can_move() {
                    continue;
                }
                drop(c);
                // TICKET-223 — through the one latch the party judge shares: the verdict is a run
                // halt BEFORE any victim below is flagged, so a party never runs past it.
                // TICKET-236 — one epoch bracket over the party walk and the re-read of this
                // sched: a waker that left inside it declines the verdict, and the timed park
                // below re-judges.
                let e0 = self.quiesce.left();
                let verdict = self
                    .quiesce
                    .decide_since(e0, crate::vm::quiesce::Judge::Sched)
                    .stands();
                c = self.lock();
                if verdict && self.is_deadlocked_ignoring_jobs(&c) && self.quiesce.left() == e0 {
                    // TICKET-103 — same leaf-first flag as above. TICKET-129 — same declined-verdict
                    // wait as above.
                    let Some(done) = self.flag_leaves_and_wake(&mut c) else {
                        let (guard, _) = self
                            .cv
                            .wait_timeout(c, DEMOTE_POLL_BACKOFF)
                            .unwrap_or_else(|e| e.into_inner());
                        drop(guard);
                        continue;
                    };
                    self.notify_waiters();
                    if done {
                        return Take::Stop;
                    }
                    continue;
                }
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
                let (guard, _) = self
                    .cv
                    .wait_timeout(c, SPIN_BACKOFF)
                    .unwrap_or_else(|e| e.into_inner());
                drop(guard);
                self.spinning.fetch_sub(1, Ordering::Relaxed);
                judged = false; // W7-58 — a real wait ended: the verdict's inputs may have moved.
                continue;
            }
            debug_assert!(
                c.global.is_empty(),
                "D4e park invariant: runnable==0 but the global queue is non-empty"
            );
            // gaps.md W7-58 — the untimed sleep is correct ONLY while the answer depends solely on
            // this sched. When `judged` is set it does not: this worker asked the PROCESS-WIDE verdict
            // and was told "not yet", and that verdict reads `parties` — a set that changes when some
            // OTHER thread registers, with no notify to this sched (nothing pokes a sched on a party
            // registration, and adding one would create a `parties → sched_registry → SchedCore` edge
            // that inverts the P → A order this change establishes). So the untimed wait made the
            // judge EDGE-TRIGGERED: measured, `ex.submit(job)` where `job` opens its own stuck
            // `parallel:`, then `main` sleeping 300 ms before `ex.shutdown()`, hung 5/5 — the job's
            // sched judged while `parties.len() == 1 < live == 2`, slept, and `main`'s later `Join`
            // registration reached nobody. (The tell: adding a third job that finishes later made it
            // fault, because `finish` pokes the sched.)
            //
            // Bounded ONLY in that state, deliberately: a sched that is not stuck-modulo-jobs still
            // sleeps untimed, so the healthy path gains no poll at all. When it IS stuck this pays
            // exactly the [`DEMOTE_POLL_BACKOFF`] cadence every other blocking-in-place site already
            // pays (`block_wait_tick`, `demote_recv_block`, `demote_block_socket`,
            // `block_until_deadline`), for the same reason: a lost wakeup then costs latency instead
            // of the whole run.
            //
            // TICKET-118 (W13-7) — only the marked joiner (`SchedCore::pool_joiner`) ever times or
            // yields here; every other idle worker keeps its untimed/timed choice above unchanged.
            match self.joiner_step(&c, me, &mut idle_since) {
                JoinerStep::Untimed => {}
                JoinerStep::Timed => {
                    let (guard, _) = self
                        .cv
                        .wait_timeout(c, DEMOTE_POLL_BACKOFF)
                        .unwrap_or_else(|e| e.into_inner());
                    drop(guard);
                    judged = false;
                    continue;
                }
                JoinerStep::Yield => {
                    drop(c);
                    crate::vm::pool::yield_slot(None);
                    judged = false;
                    continue;
                }
            }
            if judged {
                let (guard, _) = self
                    .cv
                    .wait_timeout(c, DEMOTE_POLL_BACKOFF)
                    .unwrap_or_else(|e| e.into_inner());
                drop(guard);
            } else {
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
                    self.idle_sleepers.fetch_add(1, Ordering::Relaxed);
                    let guard = self.idle_cv.wait(c).unwrap_or_else(|e| e.into_inner());
                    self.idle_sleepers.fetch_sub(1, Ordering::Relaxed);
                    drop(guard);
                } else if !width::reserved() {
                    self.idle_sleepers.fetch_add(1, Ordering::Relaxed);
                    let (guard, _) = self
                        .idle_cv
                        .wait_timeout(c, DEMOTE_POLL_BACKOFF)
                        .unwrap_or_else(|e| e.into_inner());
                    self.idle_sleepers.fetch_sub(1, Ordering::Relaxed);
                    drop(guard);
                }
            }
            judged = false; // W7-58 — see above.
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
            c.scopes[origin].joins_blocked += 1;
            c.scopes[fiber.scope_id].owners_blocked += 1;
            c.join_parked.push((origin, fiber));
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
            let (origin, mut f) = c.join_parked.remove(i);
            c.scopes[origin].joins_blocked -= 1;
            c.scopes[f.scope_id].owners_blocked -= 1;
            f.state = FiberState::Ready;
            c.global.push_back(f);
            self.runnable.fetch_add(1, Ordering::Relaxed);
            woken += 1;
        }
        woken
    }

    /// TICKET-103 — [`SchedCore::flag_deadlock_leaves`], then wake the owners of the families it
    /// completed. The sched-wide fallback (`Some(true)`) records join-parked owners itself.
    ///
    /// TICKET-129 (W13-5 residual) — `unproven_ok` is licensed either by this sched's own leaves
    /// already being provable ([`SchedCore::victims_proven`], no peer question needed) or by
    /// [`MnSched::may_fault_unproven`]. Returns `None` when the flag declined: the caller must wait,
    /// never spin (a bare `continue` here would busy-loop on the same unproven verdict).
    pub(super) fn flag_leaves_and_wake(&self, c: &mut SchedCore) -> Option<bool> {
        let ok = c.victims_proven() || self.may_fault_unproven();
        let err = self.verdict_err();
        let done = c.flag_deadlock_leaves(&err, ok)?;
        if !done {
            self.wake_completed_joins(c);
        }
        Some(done)
    }

    /// TICKET-129 (W13-5 residual) — may an UNPROVEN deadlock verdict on this sched fault? Licensed
    /// only when no live peer sched can still move or prove its own victims (Go's rule: fault only
    /// when every goroutine in the process is parked). Called with this sched's own core lock (`self`)
    /// already held — `try_lock`s the registry and each peer's core, reading contention as "a peer can
    /// move" (never blocks, matches [`MnSched::any_peer_can_move`]'s discipline).
    ///
    /// Four clauses per peer (Digest gotchas 2-4, measured on the planning spike):
    /// 1. no incomplete scope and no open body — the peer has FINISHED but its owner thread has not
    ///    yet dropped its `BlockedOwnerGuard` (`Disp::Park`'s Arc-drop-under-lock in [`MnSched::park`]
    ///    only closes the analogous window for `provable`, not this one). Faulting in that gap drops a
    ///    live feeder (`g3`, `d2a`): decline.
    /// 2. no incomplete scope but an OPEN body — a finished-looking peer whose body is still open is
    ///    NOT that window; treating it as a veto hung `j2.chz` at every run. Skip it (matches
    ///    [`MnSched::peer_can_move`]'s own first clause).
    /// 3. [`MnSched::can_still_move`] — the peer still has live work and may yet feed this sched's
    ///    candidate: decline.
    /// 4. the peer's own state is ALREADY a provable genuine deadlock
    ///    (`cross_sched_blocked_owners == 0 && !body_held_by_fiber && local_quiesced && victims_proven`):
    ///    decline, and let that peer resolve itself first rather than racing it.
    ///
    /// Only when every live peer clears every clause does this return `true`.
    pub(super) fn may_fault_unproven(&self) -> bool {
        let g = match self.sched_registry.try_lock() {
            Ok(g) => g,
            Err(std::sync::TryLockError::WouldBlock) => return false,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
        };
        let live: Vec<_> = g.iter().filter_map(|w| w.upgrade()).collect();
        drop(g);
        for s in live
            .iter()
            .filter(|s| !std::ptr::eq(Arc::as_ptr(s), self as *const MnSched))
        {
            let c = match s.core.try_lock() {
                Ok(c) => c,
                Err(std::sync::TryLockError::WouldBlock) => return false,
                Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            };
            if !c.any_scope_incomplete() {
                if !c.any_body_open() {
                    return false; // clause 1 — finished, guard not yet dropped
                }
                continue; // clause 2 — finished-looking but body open: not that window
            }
            if s.can_still_move(&c, false) {
                return false; // clause 3 — peer may still move
            }
            if c.cross_sched_blocked_owners == 0
                && !s.body_held_by_fiber(&c)
                && s.local_quiesced(&c)
                && c.victims_proven()
            {
                return false; // clause 4 — peer will resolve its own provable deadlock first
            }
        }
        true
    }
}
