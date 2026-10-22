//! VM: blocking waits: wait ticks, party guards, deadlines and halts, `block_recv`, `wait` polling
//! and arms.
//! Split out of `src/vm/netio.rs`. `super::*` is the parent module.

use super::core::{Pending, PendingOp, SendOutcome, Settled};
use super::netio::BlockGuard;
use super::netio::CLOSED_SEND;
use super::netio::EMPTY_RECV_DEADLOCK;
use super::netio::EMPTY_WAIT_DEADLOCK;
use super::netio::FULL_SEND_DEADLOCK;
use super::netio::deadline_gap_wake;
use super::quiesce::RunHalt;
use super::*;

impl Vm {
    /// One tick of a blocking wait: honour the halts, then wait up to [`DEMOTE_POLL_BACKOFF`]. Shared
    /// by [`Vm::block_recv`] and the blocking full-`send`.
    ///
    /// A job dispatched by an eager `submit` has no nursery scheduler and no [`MnSched`], and neither
    /// does the top-level `main` thread, so their blocking ops used to fall to the "no scheduler" arms
    /// and declare a deadlock on the spot. That verdict was TRUE while jobs only ran at the drain — the
    /// submitter was blocked inside `shutdown()`, so no runnable task could send — and became a LIE the
    /// moment jobs start at `submit`, because the submitter is still running and may send on the very
    /// next statement. Both kinds of party BLOCK here instead, which is also what Python's
    /// `ThreadPoolExecutor` does, and the *process-wide* verdict in [`crate::vm::quiesce`] decides when
    /// there is really nobody left to feed them.
    ///
    /// The wait is a BOUNDED poll, not an untimed `cv.wait`, for the same reason
    /// [`Vm::demote_recv_block`]'s is: a lost wakeup then costs latency instead of the whole run, and
    /// the two halts that must stay un-swallowable get re-checked every tick. `--timeout` is checked
    /// here explicitly because a blocked job never reaches `jump_checked`'s loop back-edge, which is
    /// where every other path observes the deadline — without this, `chezzi test --timeout` could not
    /// kill a job blocked forever on a channel, which is exactly the hang eager execution makes easier
    /// to write.
    ///
    /// **W7-13 — `ready` is re-checked under the SAME lock hold that the wait consumes, and that is
    /// the whole point of the parameter.** The caller has already tried its operation and failed, but
    /// it dropped `core.q` to do so and then ran [`Vm::eager_halt_check`] (which takes `exec_registry`
    /// and per-core `eager` locks — a wide window). A `notify_all` from a consumer landing anywhere in
    /// that gap reaches a condvar NOBODY IS ON YET and is simply lost, so the caller then slept the
    /// full [`DEMOTE_POLL_BACKOFF`] with its value already waiting. Measured on the 50-handoff cap-1
    /// pipeline: 7 of 15 runs paid a whole extra 5 ms tick, in exact 5 ms quanta.
    ///
    /// `Condvar::wait_timeout_while` closes it — it evaluates the predicate under the guard BEFORE
    /// sleeping, so a wakeup that arrived while the lock was free is observed instead of missed (and
    /// it re-checks on spurious wakeups for free). It also re-checks the predicate AFTER each inner
    /// wait, so `timed_out()` implies "still not ready" — which is what
    /// [`BLOCK_WAITS_SLEPT_WHILE_READY`] (test builds only) turns into a load-independent regression
    /// detector: revert this call to a bare `wait_timeout` and that counter goes nonzero.
    ///
    /// The wake it is waiting for was never missing:
    /// [`Vm::wake_senders`] already fires on all six pop paths, and for an eager job it lands on
    /// `core.cv`. `block_halt_check` MUST stay before the lock — the no-lock-cycle argument on
    /// the process-wide verdict depends on the registry never being taken under `ChannelCore::q`.
    ///
    /// This only makes the CHANNEL conditions instant. The halts `block_halt_check` acts on — the
    /// `--timeout` deadline, a cancel, the deadlock verdict — are not in any predicate and are still
    /// observed once per tick, so cancellation is now the SLOWEST thing in this loop rather than the
    /// fastest. That bound is unchanged by this fix, not introduced by it.
    ///
    /// TICKET-141 — releases this thread's width permit for the tick and re-takes it after.
    pub(super) fn block_wait_tick(
        &mut self,
        core: &Arc<ChannelCore>,
        span: Span,
        ready: impl FnMut(&mut crate::vm::core::ChanState) -> bool,
    ) -> Result<(), RuntimeError> {
        self.wait_released_on(&[core], |vm| vm.block_wait_tick_in_place(core, span, ready))
    }

    fn block_wait_tick_in_place(
        &mut self,
        core: &Arc<ChannelCore>,
        span: Span,
        mut ready: impl FnMut(&mut crate::vm::core::ChanState) -> bool,
    ) -> Result<(), RuntimeError> {
        self.block_halt_check(span)?;
        let q = core.q.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg_attr(not(test), allow(unused_mut))]
        let (mut guard, waited) = core
            .cv
            .wait_timeout_while(q, DEMOTE_POLL_BACKOFF, |g| {
                !ready(g) && !super::width::reserved()
            })
            .unwrap_or_else(|e| e.into_inner());
        #[cfg(test)]
        {
            BLOCK_WAITS.fetch_add(1, Ordering::Relaxed);
            if waited.timed_out() && ready(&mut guard) {
                BLOCK_WAITS_SLEPT_WHILE_READY.fetch_add(1, Ordering::Relaxed);
            }
        }
        drop((guard, waited));
        Ok(())
    }

    /// TICKET-134 — test-only: hold the window between the owner-fault rung and the verdict open
    /// until the owner's nursery has recorded a fault, so the check-then-check race
    /// [`Vm::block_halt_check`] closes is deterministically reachable. No-op unless armed via
    /// [`OWNER_FAULT_WINDOW_ENV`] in this run's own `HostConfig.env`.
    #[cfg(test)]
    fn owner_fault_window_hook(&self) {
        if !self.owns_nursery() {
            return;
        }
        let armed = self
            .host
            .env
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(OWNER_FAULT_WINDOW_ENV)
            .is_some();
        if !armed {
            return;
        }
        let t0 = std::time::Instant::now();
        while self.halt_requested().is_none() && t0.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Register this thread as a blocked party for as long as the returned guard lives, so the
    /// process-wide verdict can see it parked. The party half is `None` when this thread is not a
    /// counted party.
    ///
    /// §2c1 — it ALSO marks every eager nursery this thread owns as `body_blocked` for the same span.
    /// A body that is parked here cannot reach another `spawn`, so its `body_open` flag must stop
    /// vetoing the deadlock predicate (see [`super::JoinScope::body_blocked`]). This is the one funnel
    /// every counted-party block goes through, which is why the mark lives here rather than at each
    /// blocking site. Unmarked on drop, in `BlockGuard`'s `Drop`.
    ///
    /// TICKET-223 — `site` is the deadlock report this wait gives when the verdict names it: the
    /// one place a wait names its deadlock text. `None` for a join, whose report is the victims'.
    pub(super) fn block_party_guard(
        &self,
        wait: quiesce::PartyWait,
        site: Option<(&'static str, Span)>,
    ) -> BlockGuard {
        // The wait is published WITH the `body_blocked` mark, in one `SchedCore` acquisition per
        // sched (`set_body_wait`) — see its doc for the race that two acquisitions leave open. The
        // party (P) is taken FIRST (TICKET-232), with no `SchedCore` held, so the documented P → A
        // order holds: the mark lifts the body veto, and a judge that saw a blocked body and no
        // party had no site to latch. ONE `Arc<PartyWait>` is shared by the sched registration and
        // the party, so the two can never disagree about what this thread is waiting for.
        let wait = Arc::new(wait);
        let party = self.block_ctx().judged().then(|| {
            self.quiesce
                .block_shared(Arc::clone(&wait), self.wake_set(), site)
        });
        let mut g = self.blocked_bodies_guard_with(false, Some(wait));
        g._party = party;
        g
    }

    /// §2c1 — the `body_blocked` half of [`Vm::block_party_guard`] on its own: mark every eager
    /// nursery scope open on THIS thread as unable to inject, for as long as the guard lives.
    ///
    /// Used directly wherever the body stops running without registering a party — a NESTED eager
    /// nursery's join, where the enclosing body sits in `mn_worker_loop` rather than in a channel
    /// wait. Without it the enclosing scope's `body_open` vetoes the deadlock predicate for the whole
    /// duration of that join, and a genuine nested deadlock hangs
    /// (`parallel_cross_nursery_genuine_nested_deadlock_still_faults`).
    pub(super) fn blocked_bodies_guard(&self, awaiting_builder: bool) -> BlockGuard {
        self.blocked_bodies_guard_with(awaiting_builder, None)
    }

    /// [`Vm::blocked_bodies_guard`] plus the wait this block is on — published on every eager sched of
    /// this thread, atomically with the `body_blocked` mark.
    fn blocked_bodies_guard_with(
        &self,
        awaiting_builder: bool,
        wait: Option<Arc<quiesce::PartyWait>>,
    ) -> BlockGuard {
        let g = BlockGuard {
            _party: None,
            awaiting: awaiting_builder,
            bodies: self
                .owned_eager_scopes()
                // TICKET-103 — a fiber-owned nursery's body is a counted fiber, and marking its scope
                // `awaiting_builder` would veto a genuine deadlock. Continuations need no entry here:
                // `set_body_wait` marks the whole family.
                .filter(|s| !s.fiber_owned)
                .map(|s| {
                    s.sched
                        .set_body_wait(s.scope, wait.as_ref(), true, awaiting_builder);
                    (Arc::clone(&s.sched), s.scope)
                })
                .collect(),
            wait,
        };
        // TICKET-211 — the body's thread just stopped running: its runner slot goes to the claim,
        // which starts runners for whatever the body already queued.
        for (sched, scope) in &g.bodies {
            if *scope == 0 {
                let w = sched.body_runner(true);
                self.start_runners(sched, w);
            }
        }
        g
    }

    /// The `chezzi test --timeout` wall-clock halt, on its own so the ops that PARK a fiber can observe
    /// it too ([`Vm::chan_recv_step`], [`Vm::op_wait_poll`]) — a parked fiber reaches neither
    /// `jump_checked`'s loop back-edge nor [`Vm::block_halt_check`], so without a checkpoint at the op
    /// itself the deadline has no path to it at all (W7-17).
    ///
    /// Deliberately NOT gated on `native_reentry` (unlike the cancellation checkpoint it sits beside at
    /// those two call sites): a `--timeout` is a HARD abort that must always win, this only ever returns
    /// `Err` — it never unwinds VM state — and `block_until_deadline` already returns this same error
    /// from inside a native callback. Free when the cap is off: `Instant::now()` is read only when
    /// `self.deadline` is `Some`.
    pub(super) fn deadline_halt(&self, span: Span) -> Result<(), RuntimeError> {
        if let Some(dl) = self.deadline
            && std::time::Instant::now() >= dl
        {
            return Err(super::exec::timed_out_err(self.timeout_ms, span));
        }
        Ok(())
    }

    /// W7-47 — a run-wide `os.exit` issued by SOMEBODY ELSE (typically an eager `Executor` job), for
    /// the blocking loops whose party would otherwise never learn of it. Produces verbatim the shape
    /// `reduce_task_slots` already produces for a joined child's exit (`sched.rs`): `pending_exit` set
    /// plus the `"exit"` sentinel `Err`, which unwinds past every `recover:` to the driver.
    ///
    /// The one funnel for all three causes of a run halt, in this order: an exit, a job fault
    /// (TICKET-208), then the deadlock verdict that `QuiesceState::decide` latched (TICKET-223).
    ///
    /// Deliberately does NOT set `Cut::Cancelled` — that would SWALLOW the outcome (`run_outcome`),
    /// which is the opposite of what an exit needs.
    ///
    /// Returns the error rather than a `Result` because most call sites are demote loops that must run
    /// their un-accounting (`running += 1`, `unregister_waiter`, …) BETWEEN learning
    /// of the exit and returning it, exactly as their cancel arms do.
    pub(super) fn run_exit_err(&mut self, span: Span) -> Option<RuntimeError> {
        // TICKET-213 (owner decision 5) — an exit cuts a `defer` body; a job fault does not, a
        // cleanup still runs whole for a fault. `run_halt_due` holds that split (it keeps
        // `deferring == 0` on the job-fault arm alone), so this funnel returns the exit whenever the
        // exit cell is set, inside a `defer` too. (W7-57 suppressed the exit here while a `defer`
        // ran; Go `os.Exit` and Python `os._exit` run no cleanup.)
        if !self.run_halt_due() {
            return None;
        }
        if let Some(code) = self.quiesce.pending() {
            self.pending_exit = Some(code);
            return Some(self.err("exit".to_string(), span));
        }
        // TICKET-208 — an `Executor` job hit its `--max-heap` limit: the run ends with THAT abort, and
        // no `recover:` of this party catches it (`Cut::RunFault`).
        if let Some((err, trace)) = self.quiesce.job_fault() {
            self.adopt_run_fault(trace);
            return Some(err);
        }
        // TICKET-223 — the latched deadlock verdict: every party ends with the report of the
        // first party site the verdict named. `None` while main is at a join: the report there is
        // the victims' slots (DEC-208), and the join must stay until they are recorded.
        let (msg, site) = self.quiesce.deadlock_report()?;
        Some(self.err(msg, site).deadlock())
    }

    /// TICKET-208 — THE predicate "a run-wide halt is pending for this party": an `os.exit` from
    /// another party, an `Executor` job's `--max-heap` abort, or a latched deadlock verdict
    /// (TICKET-223). A CPU-side or in-place checkpoint reads this (behind the lock-free
    /// `run_halt_hint`); it never reads the exit, job-fault or deadlock cell by itself. An exit and
    /// a deadlock are due inside a `defer` too (TICKET-213, TICKET-152); a job fault waits for the
    /// `defer` to end. Which kinds cut a `defer` is `RunHalt::cuts_cleanup`; a latched verdict is
    /// due inside a `defer` even while a job fault waits.
    pub(super) fn run_halt_due(&self) -> bool {
        let halt = self.quiesce.run_halt();
        halt.cuts_cleanup() || (halt == RunHalt::Fault && self.deferring == 0)
    }

    /// The halts of a party that comes back from a wait it could not poll (a nursery join, an
    /// in-place blocking native): a cancel or a child fault, then the run-wide halt. No
    /// `--timeout` rung: at a return that would turn a post-hoc timeout into an abort.
    pub(super) fn resume_halts(&mut self, span: Span) -> Result<(), RuntimeError> {
        let read = self.halt_requested();
        self.read_halts(read, span).map_or(Ok(()), Err)
    }

    /// TICKET-200 — the one list of halts a party blocked in place observes, read with no lock held.
    /// [`Vm::block_halt_check`] adds the deadlock verdict and the pool-slot yield;
    /// [`Vm::guard_free_then_take`] reads it when the guard comes free; the in-place socket and
    /// sleep waits in `sched.rs` read it directly. A new wait loop calls one of the two; it never
    /// copies a rung.
    pub(super) fn block_halts(&mut self, span: Span) -> Result<(), RuntimeError> {
        // Checked HERE because a blocked job never reaches `jump_checked`'s loop back-edge, which is
        // where every other path observes the deadline. Without it `chezzi test --timeout` could not
        // kill a job blocked forever on a channel — exactly the hang eager execution makes easier to
        // write. No `back_edge_tick` throttle: this runs once per `DEMOTE_POLL_BACKOFF`, not per op.
        // `cancel`'s cooperative stop (D4), an enclosing scope's cancel and (TICKET-188) a child
        // fault of a nursery this party owns arrive first; then W7-47, the run-wide halt (an
        // `os.exit` or a job fault from another party). BELOW cancel, so a party that already holds
        // a cancel flag keeps unwinding as `Cancelled` (only a party with no cancel flag at all —
        // precisely top-level `main` — reaches that rung). ABOVE the deadlock verdict, because an
        // `Exit` outranks a synthesized `Deadlocked` — the precedence `reduce_task_slots` encodes.
        let read = self.halt_requested();
        if let Some(e) = self.blocked_halt(read, span) {
            return Err(e);
        }
        // TICKET-062 (W10-16) / TICKET-096 — a child fault recorded while the exit rung ran (DEC-134:
        // read again below the exit, and again after a positive verdict).
        if let Some(e) = self.take_halt(span) {
            return Err(e);
        }
        Ok(())
    }

    /// The halts a party blocked in place must observe. Split out of [`Vm::block_wait_tick`] so the
    /// multi-channel `wait:` path — which polls N arms instead of waiting on one condvar, and so
    /// cannot share the tick — honours exactly the same three, rather than being the one blocking op a
    /// `--timeout` cannot reach.
    ///
    /// TICKET-223 — it has no deadlock text of its own: a positive verdict is latched by
    /// `QuiesceState::decide` and delivered by [`Vm::run_exit_err`] as one of its three causes (an
    /// exit, a job fault, the verdict), with the report of the party site the latch recorded.
    pub(super) fn block_halt_check(&mut self, span: Span) -> Result<(), RuntimeError> {
        self.block_halts(span)?;
        // TICKET-134 — test-only seam: widen the check-then-check window between the rung above and
        // the verdict below so a racing fault is deterministically reachable in a test. No-op outside
        // `#[cfg(test)]` and unless armed.
        #[cfg(test)]
        self.owner_fault_window_hook();
        // The process-wide deadlock verdict (`future.md` §2d step 0), checked LAST so the two real
        // halts still outrank it. Every counted party is registered as blocked and none of their wait
        // conditions is satisfiable ⇒ nothing in this run can ever move again. No debounce: W7-12
        // needed two consecutive observations to rule out "a value landed a microsecond before I
        // looked", and the satisfiability re-check ([`quiesce::PartyWait::satisfiable`]) answers that
        // question directly instead of waiting a tick to guess at it — a value that landed IS a
        // satisfiable wait, so the verdict declines.
        //
        // TICKET-223 — this judge acts only through the one latch, `QuiesceState::decide`, which a
        // sched worker's judge shares. The latched verdict is a run halt, so this party ends through
        // `run_exit_err` with the report of the first registered party's site, whichever judge
        // decided first.
        if self.block_ctx().judged() && self.quiesce.decide(quiesce::Judge::Party).stands() {
            // TICKET-134 — the child can record its fault and complete between the rung above and
            // this verdict. A verdict that saw the nursery complete took the SchedCore lock after the
            // child's fault-slot write, so this re-read sees the fault.
            self.resume_halts(span)?;
        }
        // TICKET-052 — an eager `Executor` job (`mn.is_none()`) about to wait another tick hands its
        // pool thread to a replacement, so the job that would unblock it can still get a thread.
        self.yield_pool_slot(Some(DEMOTE_POLL_BACKOFF));
        Ok(())
    }

    /// Sleep until `deadline`, observing the same halts every other blocking-in-place path does
    /// ([`Vm::block_halt_check`]) once per [`DEMOTE_POLL_BACKOFF`] — the CONTINUOUS-checkpoint
    /// contract for a wait whose deadline WE own (`time.sleep_ms`, a `timer(ms)` channel's `recv`).
    ///
    /// **Why chunked `thread::sleep` and not a condvar.** A plain sleep has no channel to wait on, so
    /// a waker would have to be notified from every cancel-trip site AND still carry a timeout for the
    /// wall-clock deadline (which nobody notifies at all). `DEMOTE_POLL_BACKOFF` is the same bound
    /// every other blocking path here already pays ([`Vm::block_wait_tick`], `demote_recv_block`,
    /// `demote_block_socket`): ≤5 ms of cancel latency, 200 wakes/s per SLEEPING thread.
    ///
    /// **The sleeper is deliberately NOT registered** as a blocked party ([`Vm::block_party_guard`]).
    /// It is a live, unregistered party, so `blocked < live` and the process-wide verdict always
    /// declines — the safe direction (it can only delay someone else's fault, never fabricate one),
    /// and exactly what `inflight` does for the M:N side of the same sleep. A `PartyWait::Sleep` would
    /// be a false-deadlock generator: a sleeper's wait is never unsatisfiable, it always ends.
    ///
    /// **`--max-heap` reaches this loop only through the CANCEL arm, and only when the over-allocating
    /// task shares a cancel scope with the sleeper** — a nursery sibling or an `Executor` job, whose
    /// over-memory hard halt (`executor_hard_halt`) trips a flag this loop reads (measured: 365 ms).
    /// It does NOT reach a sleeping **top-level `main`**, which has no cancel flag and whose own heap
    /// is not the one growing — `--max-heap` is a per-`Vm` live-heap cap, so there is nothing here for
    /// a sleeper in a different heap to observe (measured: the sleep runs in full, 3005 ms, then the
    /// OVER-MEMORY verdict lands). `--timeout` has no such gap: it is an absolute wall-clock deadline
    /// this loop reads directly.
    ///
    /// TICKET-141 — releases this thread's width permit for the whole wait and re-takes it after.
    pub(super) fn block_until_deadline(
        &mut self,
        deadline: std::time::Instant,
        span: Span,
    ) -> Result<(), RuntimeError> {
        self.width_release();
        let r = self.block_until_deadline_in_place(deadline, span);
        self.width_acquire();
        r
    }

    fn block_until_deadline_in_place(
        &mut self,
        deadline: std::time::Instant,
        span: Span,
    ) -> Result<(), RuntimeError> {
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(());
            }
            self.block_halt_check(span)?;
            std::thread::sleep(DEMOTE_POLL_BACKOFF.min(deadline - now));
        }
    }

    /// Block on an empty `recv` until a value arrives, instead of declaring a deadlock (see
    /// [`Vm::block_wait_tick`]). Used by an eagerly-dispatched `Executor` job and by the top-level
    /// `main` thread — both own their OS thread and have no scheduler to park a fiber into. The settle
    /// order matches [`Vm::demote_recv_block`]'s exactly — a queued value beats a `trip()` latch,
    /// which beats closed-and-drained, which beats a cancel — so the two blocking-in-place paths
    /// cannot disagree about what a channel is saying.
    pub(super) fn block_recv(
        &mut self,
        core: &Arc<ChannelCore>,
        span: Span,
    ) -> Result<RecvStep, RuntimeError> {
        loop {
            // TICKET-185 — a rendezvous receiver publishes its SLOT in the same hold as the empty
            // re-check, so a sender that arrives during the wait commits the slot instead of
            // finding nobody. A fresh `Pending` per iteration; the previous one is settled below.
            let p = Pending::new();
            {
                let mut q = core.q.lock().unwrap_or_else(|e| e.into_inner());
                match core.recv_ready(&mut q) {
                    core::RecvReady::Value(w) => return Ok(RecvStep::Got(w)),
                    core::RecvReady::Fired => return Ok(RecvStep::Got(WireValue::Bool(true))),
                    core::RecvReady::Closed => return Ok(RecvStep::ClosedEmpty),
                    core::RecvReady::Wait => {}
                }
                if core.cap == Some(0) {
                    q.slot(&p, 0);
                }
            }
            let op = PendingOp::new(Arc::clone(&p), vec![(Arc::clone(core), 0, false)]);
            // Registered ONLY for the wait below — a per-iteration guard, so it is dropped before the
            // next `pop` attempt at the loop head. **That scoping is load-bearing, and holding the
            // registration across the attempt is a false-deadlock bug**: `pop()` and un-registering
            // are not one atomic step, so a party still registered while it consumes a value is
            // counted as parked at the very instant it made progress. Measured (a 300-handoff
            // gate/data pipeline, `an_eager_wait_block_is_woken_by_its_arm_not_by_the_poll_timeout`):
            // the consumer waits on `data`, the producer pops `gate` and is momentarily "blocked on an
            // empty gate" between the pop and the return — all parties registered, none satisfiable —
            // and the run faulted 6/10. The inverse costs nothing: an unregistered party makes
            // `blocked < live`, which only DECLINES a verdict (a delayed fault, never a wrong one).
            let party = self.block_party_guard(
                quiesce::PartyWait::Recv(Arc::clone(core), Some(Arc::clone(&p))),
                Some((EMPTY_RECV_DEADLOCK, span)),
            );
            // Ready == the settle conditions the loop head consumes (a value to take, `closed`, a
            // `trip()` latch), plus this receiver's own slot being filled, so the wait cannot sleep
            // through a state the next iteration would immediately take (W7-13). All are written
            // under `core.q` (`done_latch` since W7-13r(b)), so re-checking them under the guard the
            // wait consumes closes the window.
            let r = self.block_wait_tick(core, span, |g| {
                g.recv_ready_for(Some(&p))
                    || g.closed
                    || core.done_latch.load(Ordering::Relaxed)
                    || !p.is_queued()
            });
            drop(party);
            // Settle before acting on a fault: a filled slot is a delivered value.
            if let Settled::Got(_, w) = op.settle() {
                return Ok(RecvStep::Filled(w));
            }
            r?;
        }
    }

    /// `wait:` runtime (§6d) — execute [`Op::WaitPoll`]. The `n` arm channel handles are on the
    /// operand stack (`stack[base..base+n]`, source order). Poll source order: the first channel with
    /// a queued value (or a fired timer) wins → drop the handles, push the value, jump to that arm's
    /// body. A closed+empty arm is skipped. Nothing ready → run `else` (jump), else fault (all-closed)
    /// or block: an M:N snapshot-park, an M:N in-callback demote, or an in-place condvar wait for a
    /// party that owns its OS thread (an eager `Executor` job / top-level `main`, plus — for a TIMED
    /// wait only — either of those inside a native callback). A live timer arm is just another arm on
    /// every one of those; only the inline outermost-`parallel:` builder mid-body with no eager core
    /// (`BlockCtx::Builder { job: false }`) still inline-sleeps to the soonest deadline (`gaps.md` N10, and
    /// W7-14 for why the remaining inline-sleep is exactly that narrow).
    ///
    /// TICKET-185 — a `wait:` that blocks publishes ONE [`Pending`] for all its arms: an OFFER per
    /// send arm and (rendezvous only) a SLOT per recv arm. The CAS that takes an offer or fills a
    /// slot also decides the arm, so no `wait:` ever fires two arms. Every re-run of this op settles
    /// that `Pending` FIRST — before the deadline and cancel checkpoints — and only then polls.
    pub(super) fn op_wait_poll(&mut self, meta: &WaitMeta, span: Span) -> Result<(), RuntimeError> {
        let n = meta.n;
        // Per-arm stack width: a recv arm holds ONE slot (the channel handle), a SEND arm holds TWO
        // (channel THEN value). Walk a running `off` cursor from `base` so send arms are read correctly.
        let slot_width = |is_send: bool| if is_send { 2 } else { 1 };
        let total: usize = meta.is_send.iter().map(|&s| slot_width(s)).sum();
        let base = self.stack.len() - total;
        if let Some(op) = self.pending.take() {
            let settled = op.settle();
            if let Some(r) = self.wait_settled(settled, base, meta, span) {
                return r;
            }
        }
        // W7-17 — `--timeout` above the cancellation checkpoint and suppressed inside a `defer`, for
        // exactly the reasons `chan_recv_step` documents at the same seam: the deadline outranks a
        // cancel (and ending a timer arm early trips this fiber's cancel to close the park gap), while
        // a cleanup body's already-satisfiable `wait:` must still complete.
        if self.deferring == 0 {
            self.deadline_halt(span)?;
        }
        let mut soonest: Option<(usize, std::time::Instant)> = None;
        let mut all_closed = true;
        let mut off = 0usize;
        for i in 0..n {
            let slot = base + off;
            let Some(h) = self.stack[slot].as_obj() else {
                unreachable!("wait arm operand is not a channel handle");
            };
            off += slot_width(meta.is_send[i]);
            let core = self.channel_core(h);
            if meta.is_send[i] {
                // SEND arm — `send`'s own decision (`Vm::send_commit`, no offer): the value moves
                // (buffer room, a parked receiver's slot, unbounded) → take the arm; a closed channel
                // is READY-and-FAULTS (Go's panic-on-send-to-closed; the exact bare-`send` message),
                // SELECTED not skipped — first-ready wins in source order; otherwise not ready.
                let val = self.stack[slot + 1];
                let w = self.to_wire_crossable(val, span)?;
                match self.send_commit(h, &core, w, None) {
                    SendOutcome::Sent => {
                        self.take_wait_send_arm(base, meta.arm_targets[i]);
                        return Ok(());
                    }
                    SendOutcome::Closed => return Err(self.err(CLOSED_SEND.to_string(), span)),
                    SendOutcome::Full | SendOutcome::Offered => {
                        all_closed = false; // live — a receiver will take its offer
                        continue;
                    }
                }
            }
            let ready = core.recv_ready(&mut core.q.lock().unwrap());
            match ready {
                core::RecvReady::Value(w) => {
                    self.wake_senders(h); // a `wait:` arm freed a slot — wake a parked bounded sender
                    let v = self.from_wire(w);
                    self.take_wait_arm(base, v, meta.arm_targets[i]);
                    return Ok(());
                }
                core::RecvReady::Fired => {
                    self.take_wait_arm(base, Value::bool(true), meta.arm_targets[i]);
                    return Ok(());
                }
                core::RecvReady::Closed => {}
                core::RecvReady::Wait => {
                    all_closed = false;
                    if let Some(deadline) = core.timer
                        && soonest.is_none_or(|(_, d)| deadline < d)
                    {
                        soonest = Some((i, deadline));
                    }
                }
            }
        }
        // Nothing ready → the non-blocking `else` fallback.
        if let Some(t) = meta.else_target {
            self.stack.truncate(base);
            self.frames.last_mut().unwrap().ip = t;
            return Ok(());
        }
        // TICKET-194 — a ready arm or an `else` never waits, so neither is a cancellation point.
        self.wait_halt(span)?;
        // Every arm closed+empty, no `else`, no timer → distinct fault. (A live timer arm set
        // `all_closed = false` above, so this fires only when there is genuinely nothing to wait on.)
        if all_closed {
            return Err(self.err("wait: all channels closed".to_string(), span));
        }
        // Block on all live arms. The arm operands are on the stack (they root the channels + re-supply
        // the poll on resume). `keys[i]` = (channel handle, is_send), in arm order.
        let keys: Vec<(GcRef, bool)> = {
            let mut v = Vec::with_capacity(n);
            let mut off = 0usize;
            for &is_send in &meta.is_send {
                let h = self.stack[base + off]
                    .as_obj()
                    .expect("wait arm operand is not a channel handle");
                v.push((h, is_send));
                off += slot_width(is_send);
            }
            v
        };
        let has_send = keys.iter().any(|&(_, is_send)| is_send);
        let wait_spec = WaitSpec::Wait {
            deadline: soonest.is_some(),
            has_send,
        };
        let wait_mode = self.block_mode(wait_spec);
        // The `Refuse` cells (the inline outermost-`parallel:` builder with no worker loop and no
        // deadline to sleep to): nothing may block, so nothing is published.
        if wait_mode == BlockMode::Refuse {
            let msg = if has_send {
                FULL_SEND_DEADLOCK
            } else {
                EMPTY_WAIT_DEADLOCK
            };
            return Err(self.err(msg.to_string(), span));
        }
        if wait_mode == BlockMode::Park {
            // W7-17 — the ungated park checkpoint (see `chan_recv_step`'s): everything above settled
            // without blocking, so a hard abort had no business preempting it, but a PARK past the
            // deadline reaches no back-edge and no `block_halt_check` and would hang — including inside
            // a `defer`, where the top-of-fn check is suppressed.
            self.deadline_halt(span)?;
        }
        // Publish, under ONE `Pending`: an offer per send arm, and — for every mode but `Park`,
        // whose slots `MnSched::park_wait` publishes under the sched lock — a slot per rendezvous recv
        // arm. An arm that became ready since the poll stops the publishing: settle and poll again.
        let p = Pending::new();
        let mut op = PendingOp::new(Arc::clone(&p), Vec::with_capacity(n));
        let mut offered: Vec<GcRef> = Vec::new();
        let mut ready = false;
        let mut off = 0usize;
        for (i, &(h, is_send)) in keys.iter().enumerate() {
            let slot = base + off;
            off += slot_width(is_send);
            let core = self.channel_core(h);
            if is_send {
                let val = self.stack[slot + 1];
                let w = self.to_wire_crossable(val, span)?;
                let sum = crate::vm::core::wire_summary(&w);
                let mut g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                if g.closed || g.send_ready_for(core.cap, Some(&p)) {
                    ready = true;
                    break;
                }
                g.offer(&p, i as u32, sum, w);
                drop(g);
                offered.push(h);
            } else if wait_mode != BlockMode::Park {
                let mut g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                if g.recv_ready_for(Some(&p)) || core.done_latch.load(Ordering::Relaxed) {
                    ready = true;
                    break;
                }
                if !g.closed && core.cap == Some(0) {
                    g.slot(&p, i as u32);
                }
            }
            op.at.push((core, i as u32, is_send));
        }
        if ready {
            let settled = op.settle();
            return self
                .wait_settled(settled, base, meta, span)
                .unwrap_or_else(|| {
                    self.rewind_op(); // re-run this WaitPoll: it polls again
                    Ok(())
                });
        }
        // A published offer wakes no parked fiber (see `chan_send_step`): a parked cap-0 receiver
        // holds a live slot, so the failed `give` proves none can take it. Only a party blocking in
        // place re-checks a predicate, and the channel's condvar reaches it.
        for h in offered {
            self.channel_core(h).wake_all();
        }
        // M:N (`--parallel`) snapshot-park, top level: rewind to re-run `WaitPoll` on wake and set
        // `wait_suspend`; the worker loop captures each arm's (key, core) WHILE the fiber heap is live
        // (`Disp::WaitPark`) and `MnSched::park_wait` files ONE shared token in every arm bucket. A
        // `send`/`close` to any arm claims the fiber once and sweeps the rest (lost-wakeup-safe via the
        // park-gap re-check). Mirrors the single-`recv` `park_recv`/`Disp::Park` path, generalized to N.
        if wait_mode == BlockMode::Park {
            // WAIT-1 fix — a live timer arm is NOT taken by an inline-sleep (which would pin the worker
            // and strand a sibling `send` that lands mid-window). Instead, for the soonest timer arm
            // submit ONE background `send_wake(true)` at its deadline (in THIS scheduler) and fall
            // through to the snapshot-park, so the timer channel parks as an ordinary arm bucket. On
            // wake the re-poll pops a sibling's value (timer NOT taken) OR finds `now >= deadline` and
            // takes the timer arm. The existing `WaitPark` claimed-CAS sweep guarantees exactly one of
            // {a sibling send/close, the timer's own deadline send} wins (WAIT-2 late-alarm = CAS
            // already claimed = no-op; WAIT-3 same-instant = single claimed CAS = one winner).
            if let Some((i, deadline)) = soonest {
                // A fiber about to be cancelled must not arm a stray timer — the top-of-fn
                // cancellation checkpoint already returned in that case (on BOTH engines).
                let sched = self.mn.clone().unwrap();
                let key = self.channel_core_ptr(keys[i].0);
                let core_job = self.channel_core(keys[i].0);
                let sched_job = Arc::clone(&sched);
                // Arm ONCE per timer channel: a re-park of this same wait (woken with no consumable
                // value — e.g. a sibling `close` on another arm) re-runs WaitPoll and re-enters this
                // block, but the CAS fails the second time so we do NOT submit a redundant job. The
                // first job survives the re-park (it captures the stable `key`+`core` and wakes
                // whatever token is in this bucket at the deadline). Fresh `timer(ms)` ⇒ fresh core
                // ⇒ `armed=false`, so no reset is needed.
                if core_job
                    .timer_armed
                    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    // Account the pending timer `inflight` (gated STRICTLY on `soonest.is_some()`) so it
                    // vetoes the deadlock predicate while a lone fiber waits; the job un-accounts it.
                    sched.inflight.fetch_add(1, Ordering::Relaxed);
                    // W7-17 — same clamp as `chan_recv_step`'s timer park: fire at the SOONER of this
                    // arm's deadline and the run's `--timeout`, and deliver `true` ONLY if the arm's own
                    // deadline really passed. An early fire requeues the `WaitPark` token with nothing
                    // consumable — exactly the "woken with no consumable value" case the arm-once CAS
                    // above already tolerates — and the re-poll faults at the top-of-fn checkpoint. It
                    // is `deadline_gap_wake`, not a bare wake, because this job can fire BEFORE the
                    // fiber is parked and the CAS then forbids a second arm; see its doc. With
                    // `--timeout` off this is the plain `submit_at(deadline, …)`.
                    let fire_at = self.deadline.map_or(deadline, |rd| deadline.min(rd));
                    let gap_cancel = self.cancel.clone();
                    timer::submit_at(
                        fire_at,
                        Box::new(move || {
                            if std::time::Instant::now() >= deadline {
                                sched_job.send_wake(key, &core_job, WireValue::Bool(true));
                            } else {
                                deadline_gap_wake(&sched_job, key, &core_job, &gap_cancel);
                            }
                            sched_job.leave_inflight();
                        }),
                    );
                }
            }
            self.pending = Some(op);
            self.rewind_op(); // re-run this WaitPoll on resume
            let msg = if has_send {
                FULL_SEND_DEADLOCK
            } else {
                EMPTY_WAIT_DEADLOCK
            };
            self.park_site = Some((msg, span));
            self.wait_suspend = Some(keys);
            return Ok(());
        }
        let arms: Vec<(Arc<ChannelCore>, bool)> = keys
            .iter()
            .map(|&(h, is_send)| (self.channel_core(h), is_send))
            .collect();
        // M:N inside a native callback (`native_reentry > 0`): a host-stack loop frame sits between the
        // worker loop and here, so we cannot snapshot-park. Demote: block this worker in place until
        // an arm may be ready (its offers and slots stay published), then REWIND so this op settles
        // and re-polls. A live timer arm (`soonest`) bounds the wait (so a real send still beats the
        // timer on the re-poll). Lower throughput but sound — the documented v1 limit (§6d).
        if wait_mode == BlockMode::Demote {
            if let Err(e) = self.demote_wait_block(arms, &p, soonest, span) {
                let settled = op.settle();
                return self
                    .wait_settled(settled, base, meta, span)
                    .unwrap_or(Err(e));
            }
            self.pending = Some(op);
            self.rewind_op();
            return Ok(());
        }
        // **W7-14 — a live timer arm must not swallow the siblings, and this pair of gates is the
        // whole fix.** A party that owns its OS thread (an eager `Executor` job, the top-level `main`
        // thread — with or without a native callback frame under it) blocks in place with the timer
        // as one more arm and the wait merely CLAMPED to its deadline (below). The one `InlineSleep`
        // cell left after `--serial`'s removal: the INLINE outermost-`parallel:` builder mid-body
        // (`BlockCtx::Builder { job: false }`: `mn == None`, `mn_enlist_sched == Some`) with no eager
        // `Executor` core. It has no worker loop to drive a park, so a live timer arm inline-sleeps to
        // the soonest deadline — the alternative is the all-parties-blocked fault, which would be
        // wrong here. Its published offers and slots stay live for the sleep, so a receiver or sender
        // that arrives mid-window still wins, as in Go.
        if let Some((i, deadline)) = soonest
            && wait_mode == BlockMode::InlineSleep
        {
            // W7-17 — CHUNKED, not a bare `thread::sleep`: the deadline is OURS, so it is a checkpoint
            // for its whole duration.
            let slept = self.block_until_deadline(deadline, span);
            let settled = op.settle();
            if let Some(r) = self.wait_settled(settled, base, meta, span) {
                return r;
            }
            slept?;
            self.take_wait_arm(base, Value::bool(true), meta.arm_targets[i]);
            return Ok(());
        }
        // A party that owns its OS thread — an eager `Executor` job, or the top-level `main` thread,
        // now including one inside a native callback (TICKET-062, W10-1) — blocks instead of declaring
        // a deadlock, like the empty-`recv` and full-`send` cases, then REWINDs so the dispatch loop
        // re-runs this `WaitPoll`, which settles and re-polls every arm. Rewinding rather than looping
        // in place also means the halts land on the ordinary back-edge checkpoint.
        if wait_mode == BlockMode::InPlace {
            // Registered FIRST, before the halt check, so this party counts ITSELF as blocked; after
            // it, a lone `wait:`-blocked party would forever see `blocked < live` and never fault.
            // The registration is an OR-set over every arm (§2d's OR-edge: ready on ANY arm is
            // progress), so the verdict declines while any one of them is feedable.
            let (first, is_send0) = arms[0].clone(); // non-empty: an all-closed arm set returned above
            let arm_cores: Vec<Arc<ChannelCore>> =
                arms.iter().map(|(c, _)| Arc::clone(c)).collect();
            let party = self.block_party_guard(
                quiesce::PartyWait::Wait(arms, Some(Arc::clone(&p))),
                Some((EMPTY_WAIT_DEADLOCK, span)),
            );
            if let Err(e) = self.block_halt_check(span) {
                drop(party);
                let settled = op.settle();
                return self
                    .wait_settled(settled, base, meta, span)
                    .unwrap_or(Err(e));
            }
            // W7-13r(a) — wait on the FIRST arm's condvar with the tick as the timeout (every other
            // arm is observed within a tick). **The predicate must mirror what the re-run SETTLES on,
            // arm kind by arm kind — not what merely "changed"**: a wrong predicate is a live-lock
            // (a recv arm that read `|| g.closed` spun at 99% CPU on Go's ordinary
            // `select { case <-done: ; case v := <-work: }` with `done` closed). So:
            //   * any arm is ready once the `wait:`'s `Pending` settled (an offer taken, a slot
            //     filled, an offer closed);
            //   * a RECV arm is also ready on a value it can take (a buffered value or another
            //     party's live offer) or a `trip()` latch. NOT `closed` (the poll skips a dead arm).
            //
            // **The timer clamp — W7-14.** A live timer arm shortens the tick to its deadline, so the
            // re-poll takes the timer arm AT its deadline; before it, a sibling's value wins.
            let tick = soonest.map_or(DEMOTE_POLL_BACKOFF, |(_, d)| {
                DEMOTE_POLL_BACKOFF.min(d.saturating_duration_since(std::time::Instant::now()))
            });
            let arm0_ready = |g: &mut crate::vm::core::ChanState| {
                !p.is_queued()
                    || (!is_send0
                        && (g.recv_ready_for(Some(&p)) || first.done_latch.load(Ordering::Relaxed)))
            };
            // TICKET-168 — DEC-141's bracket list missed this wait; a body wait: hangs at T=1 without it.
            let refs: Vec<&Arc<ChannelCore>> = arm_cores.iter().collect();
            #[cfg(test)]
            WAIT_ARM0_ENTRIES.fetch_add(1, Ordering::Relaxed);
            self.wait_released_on(&refs, |_| {
                let q = first.q.lock().unwrap_or_else(|e| e.into_inner());
                #[cfg_attr(not(test), allow(unused_mut))]
                let (mut guard, waited) = first
                    .cv
                    .wait_timeout_while(q, tick, |g| !arm0_ready(g) && !super::width::reserved())
                    .unwrap_or_else(|e| e.into_inner());
                #[cfg(test)]
                {
                    WAIT_ARM0_BLOCKS.fetch_add(1, Ordering::Relaxed);
                    if waited.timed_out() && arm0_ready(&mut guard) {
                        WAIT_ARM0_SLEPT_WHILE_READY.fetch_add(1, Ordering::Relaxed);
                    }
                }
                drop((guard, waited));
            });
            drop(party);
            self.pending = Some(op);
            self.rewind_op();
            return Ok(());
        }
        // Inside a native callback: the host stack cannot be unwound to park and there is no thread
        // of our own to block on (mirrors `chan_recv_step`'s callback fault).
        Err(self.err(EMPTY_WAIT_DEADLOCK.to_string(), span))
    }

    /// TICKET-185 — act on a `wait:`'s settled [`Pending`]: `Some` when it decided the `wait:` (an
    /// offer was taken, a slot was filled, or a closed send arm faults), `None` when it was
    /// cancelled and the arms must be polled again.
    fn wait_settled(
        &mut self,
        settled: Settled,
        base: usize,
        meta: &WaitMeta,
        span: Span,
    ) -> Option<Result<(), RuntimeError>> {
        match settled {
            Settled::Sent(i) => {
                self.take_wait_send_arm(base, meta.arm_targets[i as usize]);
                Some(Ok(()))
            }
            Settled::Got(i, w) => {
                let v = self.from_wire(w);
                self.take_wait_arm(base, v, meta.arm_targets[i as usize]);
                Some(Ok(()))
            }
            Settled::Closed => Some(Err(self.err(CLOSED_SEND.to_string(), span))),
            Settled::Cancelled => None,
        }
    }

    /// Commit a chosen `wait` arm: drop the `n` channel handles (`stack[base..]`), push the received
    /// value, and jump to the arm body's target ip (the bind/assign/discard prologue).
    pub(super) fn take_wait_arm(&mut self, base: usize, value: Value, target: usize) {
        self.stack.truncate(base);
        self.push(value);
        self.frames.last_mut().unwrap().ip = target;
    }

    /// Commit a chosen SEND `wait` arm: drop all arm operands (`stack[base..]`, including this arm's
    /// value, already enqueued by the poll) and jump to the arm body — which binds NOTHING, so unlike
    /// [`Vm::take_wait_arm`] this pushes no value.
    pub(super) fn take_wait_send_arm(&mut self, base: usize, target: usize) {
        self.stack.truncate(base);
        self.frames.last_mut().unwrap().ip = target;
    }
}

/// Test-only instrumentation: how many waits [`Vm::block_wait_tick`] has performed, process-wide.
/// A COVERAGE floor for [`BLOCK_WAITS_SLEPT_WHILE_READY`] — "this program really did block on a
/// channel" — never a measurement: libtest runs the whole lib suite in ONE process, so a concurrent
/// test's blocking also lands here. Only ever read as a delta, and only ever compared with `>=`.
#[cfg(test)]
pub(crate) static BLOCK_WAITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// TICKET-134 — test-only: hold the window between the owner-fault rung and the verdict open until
/// the owner's nursery has recorded a fault. Fires once per run, only when the run's own
/// `HostConfig.env` carries this key, so no other lib test can trip it (libtest runs the whole lib
/// suite in one process, and `run_file_with` runs the VM on its own thread, so neither a global flag
/// nor a thread-local can be scoped to one test — the per-run `HostConfig.env` can).
#[cfg(test)]
pub(crate) const OWNER_FAULT_WINDOW_ENV: &str = "CHEZZI_TEST_OWNER_FAULT_WINDOW";

/// Test-only instrumentation: **W7-13's defect signature** — a [`Vm::block_wait_tick`] wait that
/// slept its whole [`DEMOTE_POLL_BACKOFF`] tick and yet found the channel READY when it woke, i.e. a
/// wakeup that was lost because it landed while nobody was on the condvar.
///
/// `wait_timeout_while` makes this UNREACHABLE by construction: it re-evaluates the predicate under
/// the guard before returning, so `timed_out()` can only be `true` while the predicate still says
/// NOT ready. The bare `wait_timeout` this replaced had no such guarantee, which is exactly the bug.
/// So a healthy build increments this ZERO times no matter how loaded the machine is — which is what
/// lets `eager_handshake_is_driven_by_wakeups_not_by_the_poll_timeout` assert on it directly, with no
/// wall-clock threshold to flake, and process-globally, with no neighbour able to pollute it (a
/// neighbour would have to hit the same defect, and then failing is right).
///
/// **Zero false positives depends on every readiness term being written under `core.q`**, because the
/// re-check runs while [`Vm::block_wait_tick`] still holds the guard the wait returned: a term some
/// other thread could flip in that window would be counted as a stall that never happened. All of
/// them are — a queued value and `closed` under `ChanState`, and `done_latch` since W7-13r(b) stores
/// it under `core.q` too. Move one back outside that lock and this counter turns flaky.
///
/// ONE EXCEPTION to "unreachable", found by review rather than reasoned away: on a POISONED `core.q`,
/// `wait_timeout_while` propagates the inner wait's `Err` WITHOUT running its post-wait re-check, so
/// the `into_inner` below can hand back `timed_out() == true` on an already-ready channel. Reaching it
/// needs another lib test to panic while holding some `ChannelCore::q` — a run that is already failing
/// (the bare `unwrap()`s elsewhere in this file panic on that same poison), so the cost is a
/// misleading second failure, not a false green.
#[cfg(test)]
pub(crate) static BLOCK_WAITS_SLEPT_WHILE_READY: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The eager `wait:` twin of [`BLOCK_WAITS`] — test-only instrumentation of how many times
/// `op_wait_poll`'s blocking arm-0 wait ran, process-wide. A COVERAGE floor for
/// [`WAIT_ARM0_SLEPT_WHILE_READY`], never a measurement, for the same reason `BLOCK_WAITS` isn't:
/// libtest runs the whole lib suite in one process, so a concurrent test's waits also land here.
/// Only ever read as a delta, and only ever compared with `>=` (TICKET-059).
#[cfg(test)]
pub(crate) static WAIT_ARM0_BLOCKS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The eager `wait:` twin of [`BLOCK_WAITS_SLEPT_WHILE_READY`] — a `wait_timeout_while` that slept its
/// whole tick and yet found arm 0 already ready when it woke, i.e. a lost wakeup. Zero by
/// construction on a healthy build for the same reason as its sibling: `wait_timeout_while`
/// re-evaluates the predicate under the guard before returning. Only ever compared with `== 0`
/// (TICKET-059).
#[cfg(test)]
pub(crate) static WAIT_ARM0_SLEPT_WHILE_READY: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// TICKET-270 — how many times `op_wait_poll` reached its blocking arm-0 wait, counted BEFORE
/// the wait, process-wide. The gate for [`WAIT_ARM0_BLOCKS`]: a run whose `wait:`s all found
/// their value queued never enters the block branch, so its blocks floor proves nothing. Only
/// ever read as a delta: a zero delta skips the floor, a nonzero one requires the blocks delta
/// to be `>= 1` (a blind-sleep revert keeps this counter and drops that one to 0).
#[cfg(test)]
pub(crate) static WAIT_ARM0_ENTRIES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
