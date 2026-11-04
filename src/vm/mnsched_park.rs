//! VM: parking and waking: park, send and recv hand-offs, `wait` parks, yield, work stealing,
//! bucket and key wakes.
//! Split out of `src/vm/mnsched.rs`. `super::*` is the parent module.

use super::*;

impl MnSched {
    /// Park the running fiber on channel `key` (it blocked on an empty `recv`), freeing the worker —
    /// BUT close the park gap first. Between `recv`'s empty-check and this call (a swap-out apart) a
    /// sibling `send` may have enqueued a message (+ run [`MnSched::send_wake`], which found no parked
    /// fiber) or a sibling may have tripped cancel; either would strand this fiber forever. So, holding
    /// the core lock (the SAME lock `send_wake`/`cancel_drain` take), re-check the channel queue and
    /// the cancel flag: if a message arrived or cancel is set, requeue the fiber as `Ready` (it will
    /// re-run `recv` and pop, or unwind on the cancel back-edge) instead of parking. Lock order is
    /// core-OUTER, channel-`q`-INNER everywhere (`send_wake` matches), so there is no ABBA cycle.
    /// No `cv` notify on the park path: parking creates no runnable work; an all-parked deadlock is
    /// detected by this worker's next `take_runnable` (`running == 0`).
    ///
    /// TICKET-185 — "a message is waiting" is [`ChanState::recv_ready_for`]: a buffered value OR a
    /// live offer of a parked sender. When nothing is waiting on a rendezvous channel, this receiver
    /// publishes its SLOT in the same `core.q` hold, so a sender that arrives next commits the slot
    /// (`ChanState::give`) instead of finding nobody. A slot is published only after this hold saw
    /// no live offer, so no parked sender needs a wake here.
    ///
    /// TICKET-129 (W13-5 residual) — takes `core` BY VALUE and moves it into the fiber's
    /// [`PendingOp`] under the core lock, rather than leaving it live on the caller's stack.
    pub(super) fn park(&self, key: usize, core: Arc<ChannelCore>, mut fiber: Fiber) {
        // Allocated BEFORE the sched lock: this critical section serializes every fiber's park.
        let p = crate::vm::core::Pending::new();
        let mut op = crate::vm::core::PendingOp::new(Arc::clone(&p), Vec::with_capacity(1));
        let mut c = self.lock();
        c.leave_running();
        // Close the park gap: re-check (under the core lock) whether a message is waiting, the channel
        // was CLOSED (a concurrent `close()` between `recv`'s empty-check and here — the fiber must
        // re-run to observe `closed` and end its `for`/fault, not park forever), or cancel was tripped.
        let (message_waiting, closed) = {
            let mut g = core.q.lock().unwrap_or_else(|e| e.into_inner());
            let ready = (g.recv_ready_for(None), g.closed);
            if !ready.0 && !ready.1 && core.cap == Some(0) {
                g.slot(&p, 0);
            }
            ready
        };
        // A concurrent `trip()` (between `recv`'s empty-check and here) sets `done_latch` then runs
        // `close_wake`, which finds no parked fiber yet — so close the gap by re-checking the latch too
        // (a tripped done() channel is "ready", like `closed`), else a `wait:`/`recv` on a manually
        // cancelled token's `done()` strands forever under the OS-thread engine.
        let latched = core.done_latch.load(Ordering::Relaxed);
        // Cross-nursery flat scheduler — read the PARKING fiber's SCOPE cancel (not the sched's global
        // `cancel`), so an inner fault that tripped only its scope re-checks the right flag here.
        let cancelled =
            c.scope_cancel_tripped(fiber.scope_id) || fiber.owned_tripped() || fiber.deadline_due();
        // The fiber carries its slot (and, with or without one, its channel) until its re-run
        // `recv` settles it; a requeued fiber settles at once on that re-run.
        op.at.push((core, 0, false));
        fiber.pending = Some(op);
        if message_waiting || closed || latched || cancelled {
            fiber.state = FiberState::Ready;
            c.global.push_back(fiber);
            self.runnable.fetch_add(1, Ordering::Relaxed); // running → ready (requeued)
            self.notify_waiters();
        } else {
            fiber.state = FiberState::Blocked; // running → parked: runnable unchanged
            c.parked
                .entry(key)
                .or_default()
                .push(ParkedEntry::Recv(fiber));
            c.parked_n += 1;
        }
    }

    /// Bounded-channel backpressure — the send-side twin of [`MnSched::park`]. The running fiber
    /// blocked on a `send` that published its OFFER (TICKET-185, `ChanState::send`); park it in
    /// `key`'s bucket, filed as [`ParkedEntry::Send`] (TICKET-028 — at cap 0 the bucket is NOT
    /// homogeneous per-instant: a parked sender and a parked receiver can share a bucket; a
    /// receiver's `WakeKind::Settled` wake requeues only the one whose offer it took). The gap re-check reads the
    /// offer's own `Pending`: requeue `Ready` once a receiver took the offer or `close()` closed it
    /// (the re-run `send` settles it), or the scope was cancelled; else park. Lock order core-OUTER /
    /// q-INNER matches `park`. A receiver that takes the offer wakes this fiber via
    /// [`MnSched::recv_wake`]/[`MnSched::handoff_wake`] (called from every pop).
    pub(super) fn park_send(&self, key: usize, mut fiber: Fiber) {
        let mut c = self.lock();
        c.leave_running();
        let cancelled =
            c.scope_cancel_tripped(fiber.scope_id) || fiber.owned_tripped() || fiber.deadline_due();
        let settled = fiber.pending.as_ref().is_none_or(|op| !op.p.is_queued());
        if settled || cancelled {
            fiber.state = FiberState::Ready;
            c.global.push_back(fiber);
            self.runnable.fetch_add(1, Ordering::Relaxed); // running → ready (requeued)
            self.notify_waiters();
        } else {
            fiber.state = FiberState::Blocked; // running → parked: runnable unchanged
            c.parked
                .entry(key)
                .or_default()
                .push(ParkedEntry::Send(fiber));
            c.parked_n += 1;
        }
    }
    /// Wake `kind`'s parked entries on `key` (TICKET-185: the receive side passes
    /// [`WakeKind::Settled`], so only the sender whose offer it took wakes).
    ///
    /// TICKET-128 (W13-25) — replaces the old always-broadcast wake. When exactly ONE fiber was
    /// woken (`n == 1`), file it in `wid`'s own `LocalQ.runnext` (Go's `runnext` handoff) instead of
    /// requeuing it to the global queue and broadcasting: the pair stays on one worker and no
    /// broadcast-to-every-idle-worker cost is paid per message. `recruit` controls whether that
    /// worker is ALSO nudged awake: a waker that keeps running passes `true`, because it may block
    /// its own thread in a `Kind::Inline` native before anyone else reaches the handed-off fiber.
    /// With `n == 0` it still notifies (DEC-128's receive-side rule). Falls back to the old
    /// broadcast path when more than one fiber woke, or the `runnext` slot was already occupied by a
    /// fresher handoff.
    pub(super) fn handoff_wake(
        &self,
        key: usize,
        core: &Arc<ChannelCore>,
        kind: WakeKind,
        wid: usize,
        recruit: bool,
    ) {
        #[cfg(test)]
        self.recv_side_wakes.fetch_add(1, Ordering::Relaxed);
        let mut c = self.lock();
        let n = self.wake_bucket(&mut c, key, kind);
        self.hand_off(c, n, wid, recruit);
        core.wake_all();
        self.wake_run_wide(key, kind);
    }

    /// TICKET-128's hand-off tail, shared by [`Self::handoff_wake`] and a cap-0 give in
    /// [`Self::send_commit`]: `n` fibers were just woken into `global` under `c`. One woken fiber
    /// goes to `wid`'s `runnext`; otherwise (or on the seeded coin flip, or an occupied `runnext`)
    /// the idle workers are broadcast to.
    fn hand_off(
        &self,
        c: std::sync::MutexGuard<'_, SchedCore>,
        n: usize,
        wid: usize,
        recruit: bool,
    ) {
        let mut c = c;
        // TICKET-167 — seeded mode takes the existing broadcast path (below) on a coin flip instead
        // of always handing off; the coin is checked BEFORE `pop_back` so a "no" leaves `global`
        // untouched for the broadcast path to find.
        if n == 1
            && wid < self.locals.len()
            && !(sched_seed::on() && self.rng.below(2) == 0)
            && let Some(f) = c.global.pop_back()
        {
            drop(c);
            let mut lq = self.lock_local(wid);
            if lq.runnext.is_none() {
                lq.runnext = Some(f);
                lq.runnext_at = Some(std::time::Instant::now());
                drop(lq);
                if recruit {
                    self.recruit();
                }
            } else {
                drop(lq);
                self.lock().global.push_back(f);
                self.notify_waiters();
            }
        } else {
            drop(c);
            self.notify_waiters();
        }
    }

    /// TICKET-185 — the M:N `send`: [`ChanState::send`]'s one decision (closed / buffer / give to a
    /// slot / offer / full) made under the sched lock, then, when the value MOVED, the wake: a
    /// cap-0 give hands the one slot owner to `wid`'s `runnext`; a buffered push wakes every
    /// receiver parked on `key`. An `Offered` result wakes nobody here: the caller wakes the
    /// receivers once, through [`MnSched::handoff_wake`] (a parked fiber, TICKET-128's `runnext`) or
    /// [`MnSched::close_wake`]'s fan-out (a party blocking in place).
    pub(super) fn send_commit(
        &self,
        key: usize,
        core: &Arc<ChannelCore>,
        v: Value,
        offer: Option<(&Arc<crate::vm::core::Pending>, u32)>,
        wid: usize,
    ) -> crate::vm::core::SendOutcome {
        let mut c = self.lock();
        let out = core
            .q
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(core.cap, v, offer);
        if !matches!(out, crate::vm::core::SendOutcome::Sent) {
            return out;
        }
        if core.cap == Some(0) {
            // A cap-0 `Sent` is a `give`: it filled exactly one slot, so only that slot's owner can
            // proceed. Hand it to this worker's `runnext` (DEC-128; `recruit` because the giver
            // keeps running) — a global push plus a broadcast migrates the pair every message.
            let n = self.wake_bucket(&mut c, key, WakeKind::Settled);
            self.hand_off(c, n, wid, true);
            core.wake_all();
            self.wake_run_wide(key, WakeKind::Settled);
            return out;
        }
        self.wake_bucket(&mut c, key, WakeKind::All);
        drop(c);
        self.notify_waiters();
        self.wake_run_wide(key, WakeKind::All);
        core.wake_all();
        out
    }
    /// A `recv` took a value on `key` (TICKET-185): requeue every parked party whose hand-off
    /// committed ([`WakeKind::Settled`]) — the sender whose offer `pop_for` took or moved into the
    /// buffer — and leave every queued party parked. The enlist route of [`Vm::wake_senders_core`].
    pub(super) fn recv_wake(&self, key: usize, core: &Arc<ChannelCore>) {
        let kind = WakeKind::Settled;
        let mut c = self.lock();
        self.wake_bucket(&mut c, key, kind);
        drop(c);
        self.notify_waiters();
        core.wake_all();
        self.wake_run_wide(key, kind);
    }

    /// §6d M:N multi-channel `wait` park — the N-key generalization of [`MnSched::park`]. The running
    /// fiber blocked on a `wait` whose every arm channel was empty/live; `arms` is `(key, core)` for
    /// each arm (captured by the worker loop while the fiber heap was live, exactly like
    /// `Disp::Park`). Holding the core lock (the SAME lock `send_wake`/`close_wake`/`cancel_drain`
    /// take), close the park gap for ALL N arms first: if ANY arm became ready, or cancel was tripped
    /// between the `WaitPoll` empty-poll and here, requeue the fiber `Ready` (it re-runs `WaitPoll`,
    /// which settles and re-polls source order) instead of parking — else strand-forever. Otherwise
    /// allocate ONE `Arc<WaitPark>` holding the fiber and file a clone of the token in every arm's
    /// bucket; `parked_n += 1` (one fiber, not N tokens). Lock order core-OUTER / channel-`q`-INNER
    /// matches `park`, so no ABBA; no `cv` notify on the park path (parking creates no runnable work —
    /// an all-parked deadlock is caught by the next `take_runnable`).
    ///
    /// TICKET-185 — the `wait:`'s ONE `Pending` rides in `fiber.pending`: `op_wait_poll` already
    /// published every SEND arm's offer under it, and this re-check publishes every rendezvous RECV
    /// arm's slot under it, in the same `core.q` hold that saw the arm not ready. Readiness ignores
    /// the `wait:`'s own entries ([`ChanState::recv_ready_for`]`(Some(p))`), so a send arm and a recv
    /// arm on one channel never see each other.
    /// Each arm is `(SOURCE arm index, key, core, is_send)`: an off arm is not in `arms`, so a slot
    /// is published under the arm's source index, never its position here.
    pub(super) fn park_wait(
        &self,
        arms: Vec<(u32, usize, Arc<ChannelCore>, bool)>,
        mut fiber: Fiber,
    ) {
        let mut c = self.lock();
        c.leave_running();
        let p = fiber.pending.as_ref().map(|op| Arc::clone(&op.p));
        // Gap re-check for EVERY arm (mirrors `park`'s 1-key re-check). Cross-nursery flat scheduler
        // — read the parking fiber's SCOPE cancel (not the sched's global `cancel`). A SEND arm is
        // ready exactly when its offer settled — taken by a receiver, or closed by `close()` — which
        // the shared `Pending`'s state says for every send arm at once.
        let mut ready_now = c.scope_cancel_tripped(fiber.scope_id)
            || fiber.owned_tripped()
            || fiber.deadline_due()
            || p.as_ref().is_some_and(|p| !p.is_queued());
        // D2 — an arm is READY (requeue: the re-run `WaitPoll` takes it) or LIVE (empty but still
        // wakeable). A closed recv arm is ready — the poll takes it with `!RecvError` — so a `close()`
        // that lands between `op_wait_poll`'s empty poll and this park (its `close_wake` found the
        // bucket still empty) is seen here, and the requeue terminates on the taken arm.
        if !ready_now {
            for (i, _, core, is_send) in &arms {
                if *is_send {
                    // A parked send arm is ready exactly when its offer settled — taken by a
                    // receiver, or closed by `close()` — which the `Pending` read above says.
                    continue;
                }
                let ready = {
                    let mut g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                    // RECV arm: ready with a value to take (a closed channel still drains its
                    // buffer first) or once closed and drained.
                    let ready = g.closed || g.recv_ready_for(p.as_ref());
                    if !ready
                        && core.cap == Some(0)
                        && let Some(p) = &p
                    {
                        g.slot(p, *i);
                    }
                    ready
                };
                // A tripped `done_latch` (a concurrent `trip()`) makes this arm ready, same as a queued
                // value or a close — re-check it in the gap or a `wait: tok.done()` strands forever.
                // (Timers/latches are recv-only channels, so this never applies to a send arm.)
                if ready || core.done_latch.load(Ordering::Relaxed) {
                    ready_now = true;
                    break;
                }
            }
        }
        if ready_now {
            fiber.state = FiberState::Ready;
            c.global.push_back(fiber);
            self.runnable.fetch_add(1, Ordering::Relaxed); // running → ready (requeued, re-polls)
            self.notify_waiters();
            return;
        }
        fiber.state = FiberState::Blocked; // running → parked: runnable unchanged
        let keys: Vec<usize> = arms.iter().map(|(_, k, _, _)| *k).collect();
        let wp = Arc::new(WaitPark {
            fiber: Mutex::new(Some(fiber)),
            keys: keys.clone(),
            p,
            claimed: AtomicBool::new(false),
        });
        for key in keys {
            c.parked
                .entry(key)
                .or_default()
                .push(ParkedEntry::Wait(Arc::clone(&wp)));
        }
        c.parked_n += 1; // ONE fiber, regardless of arm count
    }

    /// D3 — the running fiber exhausted its reduction budget; requeue it at the TAIL of the **global**
    /// queue (not a local) and free the worker. Routing a time-slice preemption to the global queue
    /// (as Go does) preserves cross-worker fairness: the worker returns to the shared pool and picks up
    /// *other* runnable work via its next batch-grab, instead of re-popping the same CPU-bound fiber
    /// from its own local forever (which would re-introduce the D3 starvation). Decrements `running`
    /// like `park`/`finish`. Unlike `park` it touches no `parked` bucket (a yield carries no channel
    /// handle) and always requeues, so there is no park-gap/cancel re-check: a cancelled fiber requeued
    /// here re-runs and observes the flag at the next back-edge.
    ///
    /// gaps.md W8-7 — deliberately **no** `cv.notify` here (was `notify_all`, 22nd-of-22 site,
    /// removed): preemption fires every `CONTEXT_REDS` dispatched ops per fiber, so with a
    /// multi-fiber CPU-bound scope this was a `notify_all` many times a second, each waking every
    /// idle worker into an O(W) `try_steal` probe that finds nothing and re-parks — O(W^2) mutex/futex
    /// churn per time slice (measured: default worker count on a 12-core box was SLOWER than
    /// `--threads=4`, 28x the sys time). No wake is needed here because the fiber this requeues almost
    /// always already has a live consumer — with ONE hole, closed at the departure rather than here
    /// (see below):
    /// 1. The yielding worker itself loops straight back into `take_runnable`
    ///    (`Vm::mn_worker_loop`, `sched.rs`). Precisely: its only exit that does NOT go through
    ///    `take_runnable` is `self.demoted` — the other one, `Take::Stop`, is *returned by*
    ///    `take_runnable`, so a worker taking it has already evaluated the queue and cannot be holding
    ///    an unconsumed yield. `mn_worker_loop`'s `self.demoted` arm carries the full exit enumeration.
    ///    `demoted` is taken when THIS worker was itself covered by a
    ///    replacement earlier (mid-fiber, at demote time) and now leaves for good instead of looping
    ///    back. That replacement was spun up while `runnable == 0` and typically parked into this same
    ///    `take_runnable`'s untimed `cv.wait` well before this yield — so on the `demoted` exit no one
    ///    is left awake, unless something notifies. Fixed at the departure, not here: `mn_worker_loop`
    ///    now does `sched.notify_waiters()` on the `self.demoted` return (`sched.rs`), which is where
    ///    the actual consumer gap is — see its doc for the enumeration. (This is the gaps.md W8-7 hang
    ///    regression fix.)
    /// 2. The owner-scope-completing case this argument used to cite separately is **vacuous**, not a
    ///    second live consumer: `take_runnable`'s owner-stop branch sits *after* the global batch-grab,
    ///    so it is reachable only with the global queue already empty — the worker would have grabbed
    ///    its own just-yielded fiber first. It cannot be the thing that leaves a yielded fiber
    ///    unconsumed.
    /// 3. `runnable` accounting is unchanged (still incremented under this same lock), so
    ///    `is_deadlocked`'s `runnable == 0` predicate cannot false-fire on a yielded fiber.
    /// 4. Ordering is unchanged (global tail), so round-robin fairness is exactly as before — pinned by
    ///    `mnsched_yield_fiber_requeues_at_tail`, unmodified.
    ///
    /// Go does NOT do the same — checked against go1.26.6's `runtime/proc.go`: `goschedImpl` (the
    /// preemption path) calls `wakep()` on EVERY preemption, which CAS-guards a single idle P awake
    /// (`sched.nmspinning`) — a damped single wake, not no wake. But note WHAT that wake is for.
    /// `goschedImpl`'s last line is `schedule()`, on the same M — Go's exact equivalent of this
    /// worker's `take_runnable` re-entry — so the preempted `g` already has a consumer and `wakep` is
    /// a **recruitment** wake, not a liveness one: it brings an idle P online now that there is one
    /// more runnable `g` than there are runners. That is why one spinner is enough to suppress it
    /// (`nmspinning`) and why it is gated on `mainStarted`, neither of which would make sense for a
    /// wake the `g`'s survival depended on. So the honest comparison is NOT that Chezzi's guarantee is
    /// stronger — both runtimes have the same always-present consumer. What Chezzi gives up by not
    /// waking is the RECRUITMENT: Chezzi wakes ZERO times per preemption where Go wakes at most one,
    /// so an idle Chezzi worker is never recruited mid-slice the way an idle Go P can be. That is
    /// sound here because a preemption does not create work — it re-queues work that already had a
    /// runner — and because the ways a fiber becomes runnable for the FIRST time all notify (`inject`,
    /// `wake_bucket` at all four callers, the park/send/wait gap requeues, `cancel_drain`,
    /// `complete_offload`, and the batch-grab surplus push above). Exactly one other site increments
    /// `runnable` without notifying — `register_scope_seeded`, which seeds a nested nursery's fibers on
    /// a LIVE sched — and it has **two callers whose safety arguments are different**, which matters
    /// because this enumeration is the whole liveness case:
    ///   - `Vm::run_mn_nursery_nested` (`sched.rs`, the seeding caller) is safe because it becomes the
    ///     consumer on the very next line (`shell.mn_worker_loop(..)`) — the `yield_fiber` argument.
    ///   - `Vm::activate_eager_nursery`'s nested-scope branch (`sched.rs`) is NOT: it returns an
    ///     `EagerScope` with no loop after it. It is safe **only because it passes `Vec::new()`**, so
    ///     the `fetch_add` is `+0` and nothing becomes runnable. **A future commit that seeds a
    ///     non-empty vec there gets no notify and no consumer** — add the notify at that call site if
    ///     you ever do.
    ///
    /// `seed` is a third no-notify site, but it runs pre-start. This is the sys-time collapse W8-7
    /// measured.
    ///
    /// TICKET-128 (W13-25) — `handoff_wake`'s one-fiber path is a FOURTH no-broadcast site, and its
    /// consumer argument is per-caller, not the shared one above:
    ///   - The send-side deposit (`Vm::send_step`, `netio.rs`) files the fiber in the SENDING
    ///     worker's own `runnext` and calls `park_send` on the very next line — its consumer is that
    ///     worker's own next `take_runnable` re-entry, exactly like a yield. Its one gap:
    ///     `take_runnable_inner`'s step 0 periodic global pull can run a global fiber ahead of that
    ///     `runnext` and may block its thread in a native, so step 0 calls `recruit()` whenever it
    ///     pulls a fiber while `runnext` is occupied.
    ///   - The receive-side handoff (`Vm::wake_senders_core`, `netio.rs`) has NO guaranteed consumer:
    ///     the receiver keeps running and may block its own thread (e.g. in a native) before ever
    ///     reaching the handed-off sender. It always passes `recruit: true`, which wakes at most one
    ///     `idle_cv` sleeper (damped: a no-op while any worker is already spinning). A woken spinner
    ///     cannot steal the fresh `runnext` until `HANDOFF_GRACE` elapses (`try_steal`), which gives
    ///     the intended consumer time to arrive on its own first.
    pub(super) fn yield_fiber(&self, mut fiber: Fiber) {
        let mut c = self.lock();
        c.leave_running();
        fiber.state = FiberState::Ready;
        c.global.push_back(fiber);
        self.runnable.fetch_add(1, Ordering::Relaxed); // running → ready (round-robin requeue)
    }

    /// D4c — try to steal runnable work for an idle worker `wid` from a sibling's local queue. Probes
    /// every other worker (rotating start via a shared counter, to avoid convoying) and steals
    /// **half** (ceil) of the first non-empty victim, taken from the ring BACK first (the victim runs
    /// its front, so thief and victim rarely contend on the same fiber) and falling back to the
    /// victim's `runnext` once the ring is exhausted. Draining `runnext` too is what keeps the
    /// deadlock predicate sound: a fiber stranded in a victim's `runnext` is counted in `runnable`
    /// (suppressing the deadlock fire) but, if a thief could not reach it, no one would run it → a
    /// silent hang. `runnext` is unused at runtime today (only `recv`-wake/spawn would populate it,
    /// neither of which routes through a local yet), but stealing it now makes the contract hold for
    /// any future commit that does. Returns the stolen fibers (the caller pushes them onto its own
    /// local). Locks one victim local at a time (class **B**, alone), never while holding the core
    /// lock → no ABBA. Net-zero on `runnable` (the fibers stay runnable, just move locals).
    pub(super) fn try_steal(&self, wid: usize) -> Vec<Fiber> {
        let n = self.locals.len();
        if n <= 1 {
            return Vec::new();
        }
        let r = if sched_seed::on() {
            self.rng.next() as usize
        } else {
            self.steal_ctr.fetch_add(1, Ordering::Relaxed)
        };
        for i in 0..n {
            let v = (wid + 1 + r.wrapping_add(i)) % n;
            if v == wid {
                continue;
            }
            let mut vq = self.lock_local(v);
            // TICKET-128 (W13-25) — a `runnext` younger than `HANDOFF_GRACE` is not yet stealable
            // (see `LocalQ::runnext_at`); older than that it counts and steals like any other fiber.
            let rn_stealable = vq.runnext_at.is_none_or(|t| t.elapsed() >= HANDOFF_GRACE);
            let len = vq.ring.len() + usize::from(vq.runnext.is_some() && rn_stealable);
            if len == 0 {
                continue;
            }
            let take = len.div_ceil(2); // ceil-half, so a victim with 1 still yields it
            let mut stolen = Vec::with_capacity(take);
            for _ in 0..take {
                // Ring BACK first, then the `runnext` slot as a last resort (if stealable).
                let next = vq.ring.pop_back().or_else(|| {
                    if rn_stealable {
                        vq.runnext.take()
                    } else {
                        None
                    }
                });
                if let Some(f) = next {
                    stolen.push(f);
                } else {
                    break;
                }
            }
            if !stolen.is_empty() {
                return stolen;
            }
        }
        Vec::new()
    }

    /// A `send` into channel `key`: enqueue the message AND move every fiber parked on it back onto
    /// the global queue (as `Ready`), atomically under the core lock — so it serializes with [`MnSched::park`]'s
    /// gap re-check and a fiber parking concurrently is never lost. `notify_all` may over-notify (a
    /// spuriously woken worker finds the queue empty and re-parks) — targeted wake + the StoreLoad
    /// barrier are D4. Lock order: core-OUTER, channel-`q`-INNER (matches `park`).
    /// Wake every fiber parked on `key`, draining the bucket. Caller holds the core lock (`c`). Two
    /// entry kinds:
    /// * `ParkedEntry::Recv(f)` — a plain `recv` park: requeue the fiber `Ready` (the pre-`wait`
    ///   behavior, byte-identical: a `send` enqueues then makes the one receiver runnable; a `close`
    ///   makes all receivers runnable to observe the closed flag).
    /// * `ParkedEntry::Wait(wp)` — a multi-channel `wait` token: CAS `wp.claimed` false→true. The
    ///   FIRST waker (this key or a concurrent waker on a sibling key) wins → `take()` the single
    ///   fiber, requeue it `Ready` exactly once (`parked_n -= 1`), and SWEEP its token out of every
    ///   OTHER `wp.keys` bucket (by `Arc::ptr_eq`) under this same lock hold, so a later `send`/`close`
    ///   to a swept channel can never re-wake the now-moved fiber. A loser sees `claimed` already set
    ///   and drops the stale token (no double-wake, no panic). All under the one core-lock hold.
    ///
    /// Returns the number of fibers actually woken (moved `parked` → `global` `Ready`). TICKET-128
    /// (W13-25) — `handoff_wake` needs this count to decide between a one-fiber handoff (file it in
    /// the waker's own `runnext`) and a broadcast (more than one, or the woken fiber already
    /// reclaimed by the general path).
    fn wake_bucket(&self, c: &mut SchedCore, key: usize, kind: WakeKind) -> usize {
        let Some(entries) = c.parked.remove(&key) else {
            return 0;
        };
        let mut woken = 0usize;
        let mut keep: Vec<ParkedEntry> = Vec::new();
        for entry in entries {
            if kind == WakeKind::Settled && !entry_settled(&entry) {
                // TICKET-185 — this party's hand-off has not committed; waking it would re-run its
                // op, find nothing, and re-park (DEC-028's livelock). Leave it parked.
                keep.push(entry);
                continue;
            }
            match entry {
                ParkedEntry::Recv(f) => {
                    c.parked_n -= 1;
                    self.runnable.fetch_add(1, Ordering::Relaxed); // parked → ready
                    woken += 1;
                    let mut f = f;
                    f.state = FiberState::Ready;
                    c.global.push_back(f);
                }
                ParkedEntry::Send(mut f) => {
                    c.parked_n -= 1;
                    self.runnable.fetch_add(1, Ordering::Relaxed); // parked → ready
                    woken += 1;
                    f.state = FiberState::Ready;
                    c.global.push_back(f);
                }
                ParkedEntry::Wait(wp) => {
                    // CAS the wake-once gate: only the winner takes the fiber + sweeps.
                    if wp
                        .claimed
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        continue; // already claimed by a concurrent waker on another key — stale token
                    }
                    let mut f = wp
                        .fiber
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                        .expect("WaitPark fiber claimed twice");
                    c.parked_n -= 1; // ONE fiber, matching park_wait's +1
                    self.runnable.fetch_add(1, Ordering::Relaxed); // parked → ready
                    woken += 1;
                    f.state = FiberState::Ready;
                    c.global.push_back(f);
                    // Sweep the stale token out of every OTHER arm bucket (this `key` is already drained
                    // via the `remove` above). Match by Arc identity so we never disturb an unrelated
                    // fiber that happens to share a bucket.
                    for &other in &wp.keys {
                        if other == key {
                            continue;
                        }
                        if let Some(bucket) = c.parked.get_mut(&other) {
                            bucket.retain(
                                |e| !matches!(e, ParkedEntry::Wait(o) if Arc::ptr_eq(o, &wp)),
                            );
                            if bucket.is_empty() {
                                c.parked.remove(&other);
                            }
                        }
                    }
                }
            }
        }
        if !keep.is_empty() {
            c.parked.insert(key, keep);
        }
        woken
    }

    /// TICKET-099 — after waking this sched's own `parked` bucket for `key`, wake the matching bucket
    /// on every OTHER live `MnSched` of this run too. A parked receiver can sit on a sibling sched or a
    /// descendant sched, not just an ancestor — the old upward-only ancestor chain never reached
    /// either, which is the whole defect this replaces. Takes `sched_registry` first, upgrades every `Weak`,
    /// prunes the dead entries, then DROPS the registry lock before touching any sched core (registry
    /// then core, R then A — the same order [`Vm::wake_on_send_key`] already uses); the caller must
    /// hold no sched core lock and no `ChannelCore::q` when this runs. `wake_key` takes each peer's OWN
    /// core lock in turn, so no two sched cores are ever held at once (no ABBA). A peer whose bucket is
    /// empty is not notified at all (TICKET-130, see `wake_key`).
    fn wake_run_wide(&self, key: usize, kind: WakeKind) {
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
            if !std::ptr::eq(Arc::as_ptr(&s), self as *const MnSched) {
                s.wake_key(key, kind);
            }
        }
    }

    /// Drain this sched's `parked` bucket for `key` and, when that requeued at least one fiber,
    /// notify its workers — one link of [`MnSched::wake_run_wide`], also used by the W7-56 registry
    /// walk in [`Vm::wake_on_send`] (an eager `Executor` job holds no sched, so it reaches a parked
    /// fiber only this way). Takes the core lock itself, so the caller must hold NO sched core lock
    /// and no `ChannelCore::q`.
    ///
    /// TICKET-130 (W13-26) — a drain that requeued NO fiber changes no `runnable`/`parked_n`/`global`
    /// a worker of this sched reads, so it notifies nobody. `wake_run_wide` calls this on every peer
    /// sched once per wake, and the old unconditional `notify_waiters` woke every idle worker of
    /// every peer on every rendezvous message: a ping-pong nested four `parallel:` levels deep went
    /// from ~1.6 s to ~7.7 s at 8 workers. Skipping it is live because every parker re-checks the
    /// channel under this same core lock (`park`, `park_send`, `park_wait`): a racing fiber is either
    /// already in the bucket (`n > 0`, notified here) or sees the new state and requeues itself.
    pub(super) fn wake_key(&self, key: usize, kind: WakeKind) {
        let mut c = self.lock();
        let n = self.wake_bucket(&mut c, key, kind);
        drop(c);
        if n > 0 {
            self.notify_waiters();
        }
    }

    pub(super) fn send_wake(&self, key: usize, core: &Arc<ChannelCore>, v: Value) {
        let mut c = self.lock();
        core.q.lock().unwrap_or_else(|e| e.into_inner()).push(v);
        self.wake_bucket(&mut c, key, WakeKind::All);
        drop(c);
        self.notify_waiters();
        // gaps.md B5 — also wake a receiver parked on this channel in an ANCESTOR (parent) nursery's
        // sched (eager nested nursery only; no-op otherwise). Value is already queued above.
        self.wake_run_wide(key, WakeKind::All);
        // D5 owe #3 (Path C) — also wake any worker thread DEMOTED on this channel (blocked in place
        // on `core.cv` after a `recv` inside a native callback). Snapshot-parked fibers are requeued
        // above + woken via `self.cv`; a demoted thread instead waits on the channel's OWN condvar, so
        // it must be notified here. Without this it would only re-check on its bounded poll timeout
        // (added latency, not a hang). No-op when no thread is demoted on this channel.
        core.wake_all();
    }

    /// A `close()` on channel `key`: wake EVERY fiber parked on it (not just one, as a `send` would —
    /// a close has no value to deliver, it just unblocks all receivers) so each re-runs its `recv` /
    /// `ChanRecvOrClosed` and observes the now-closed channel (a `for v in ch:` ends, a bare `recv`
    /// gives `!RecvError`, a `wait:` takes the closed arm). Atomic under the core lock so it
    /// serializes with [`MnSched::park`]'s gap re-check (the `closed` flag is already set by the
    /// caller before this call). Same lock discipline as
    /// `send_wake`; `core.cv.notify_all` also wakes any thread DEMOTED on this channel.
    pub(super) fn close_wake(&self, key: usize, core: &Arc<ChannelCore>) {
        let mut c = self.lock();
        self.wake_bucket(&mut c, key, WakeKind::All);
        drop(c);
        self.notify_waiters();
        core.wake_all();
        // gaps.md B5 — a close from inside an eager body must also wake a receiver ranging over this
        // channel in an ANCESTOR nursery so it observes the close and ends (no-op for ordinary scheds).
        self.wake_run_wide(key, WakeKind::All);
    }
}
