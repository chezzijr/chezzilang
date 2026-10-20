//! VM: the M:N scheduler core: `SchedCore` scope bookkeeping and `MnSched` setup (scope
//! registration, injection, slots, runners, bodies, helpers, scope cancel). Taking, parking and the
//! verdicts are in `mnsched_take.rs`, `mnsched_park.rs` and `mnsched_verdict.rs`.
//! Split out of `src/vm/mod.rs`. `super::*` is the parent module.

use super::*;

impl SchedCore {
    /// Cross-nursery flat scheduler — every scope's tasks are done (the global terminate condition,
    /// together with `!any_body_open`).
    pub(super) fn all_scopes_done(&self) -> bool {
        self.scopes.values().all(|s| s.done == s.total)
    }

    /// Cross-nursery flat scheduler — any scope's eager body is still injecting (vetoes terminate +
    /// the global deadlock predicate, exactly as the old scalar `body_open` did).
    pub(super) fn any_body_open(&self) -> bool {
        self.scopes.values().any(|s| s.body_open)
    }

    /// §2c1 — the DEADLOCK predicate's half of [`Self::any_body_open`]: any scope whose eager body can
    /// still `inject`. A body that is itself BLOCKED in place cannot reach another `spawn`, so it is
    /// not live work and must not veto the verdict (see [`JoinScope::body_blocked`]). Terminate keeps
    /// asking `any_body_open`, which ignores this flag.
    pub(super) fn any_body_injecting(&self) -> bool {
        self.scopes.values().any(|s| s.body_open && !s.body_blocked)
    }

    /// Cross-nursery flat scheduler — true when EVERY still-incomplete scope is one merely awaiting the
    /// inline builder's own `JoinNursery` (early-enlisted). The builder, having finished all nested
    /// service (else a non-`awaiting_builder` scope would still be incomplete), WILL return to the body
    /// and can `send`/`close`/`spawn` to feed these parked siblings — so this quiesce is NOT a deadlock.
    /// A single-scope sched never holds an enlisted scope, so the fast path is always `false` (zero cost
    /// on the common path). (Cross-nursery flat scheduler — charges #1/#2.)
    pub(super) fn all_incomplete_awaiting_builder(&self) -> bool {
        if self.scopes.count() == 1 {
            return false;
        }
        let mut any_incomplete = false;
        for s in self.scopes.values() {
            if s.done < s.total {
                any_incomplete = true;
                if !s.awaiting_builder {
                    return false;
                }
            }
        }
        any_incomplete
    }

    /// N4 — some still-incomplete scope has its `cancel` tripped, i.e. is MID-TEARDOWN: a fault/exit/
    /// abort has cancelled it and its parked fibers are about to be requeued by `cancel_drain` so they
    /// can unwind their `defer`s. Vetoes the deadlock predicate (see [`MnSched::is_deadlocked`]).
    ///
    /// A cancel trip and its `cancel_drain` are TWO SEPARATE core-lock acquisitions apart at every seam
    /// that trips one — there are exactly THREE (the only scope-cancel stores in the VM):
    /// `Vm::trip_cancel` (exec.rs, from `classify_mn_outcome`'s fault/exit AND `run_one_fiber`'s
    /// panic-fault fallback) followed by `mn_worker_loop`'s `finish` → `cancel_drain`;
    /// `abort_enlisted_scope`; and `abort_eager_nursery` (sched.rs). (The two demote self-detect loops
    /// only READ a cancel — they trip none.) An idle worker's `take_runnable` landing in such a gap sees
    /// the pre-drain quiesce (`running == 0 && runnable == 0 && parked_n > 0`) and, without this veto,
    /// calls the teardown a DEADLOCK — `flag_deadlock` then DROPS the still-parked siblings without
    /// `unwind_deferred`, silently skipping their `defer`s.
    ///
    /// The two abort seams also clear a PRE-EXISTING veto of their own (`awaiting_builder` /
    /// `any_body_open`); both trip the scope cancel FIRST (`MnSched::trip_scope_cancel`, a store under
    /// the core lock) so the veto handoff is GAPLESS — one veto is armed before the other is dropped.
    /// The store must stay under the core lock: it is what publishes the flag to any worker that later
    /// takes that lock to evaluate the predicate (a bare `Relaxed` store outside it has no
    /// synchronizes-with edge). On the fault path the edge is `finish`'s own lock release (the predicate
    /// needs `running == 0`, which only holds after it).
    ///
    /// BOUNDED TO THAT WINDOW (this is the whole liveness argument): the veto asks for an UNDRAINED
    /// PARKED fiber of the cancelled scope, not merely `done < total`. "Some incomplete cancelled scope"
    /// was too wide and could never lift: a `defer` is not itself cancellable (`halt_requested`'s
    /// `deferring == 0` term), so a cleanup body that blocks FOREVER (`ch.recv()` nobody will ever
    /// answer) demotes and sits there — the scope is then incomplete *because of* that fiber, the veto
    /// held forever and the M:N engine hung SILENTLY (serial reports it). The harm the veto exists to
    /// prevent is only possible while parked fibers are still owed their `cancel_drain`; once drained
    /// they are in `global` (`runnable > 0`), so the predicate is false on its own terms and the veto is
    /// no longer needed. A cancelled scope cannot RE-accumulate parked fibers after its drain: every
    /// park path re-checks this scope's cancel OR an ancestor's (`park`/`park_send`/`park_wait` re-read
    /// `c.scope_cancel_tripped(fiber.scope_id)` under the core lock and requeue `Ready` instead of
    /// parking; `poll_park_offload` hands the netpoller's `register` the same `ScopeCancel`, which
    /// rejects the park under the registry lock `drain_family` sweeps under) — pinned by
    /// `mnsched_park_requeues_when_cancel_tripped` and `poll_park_rejects_cancelled_inner_scope`. The
    /// NETPOLLER half counts too (TICKET-200): a poll-parked fiber is deliberately NOT in `parked` and
    /// `poll_park_offload` accounts it running→`inflight`, so `is_deadlocked` (which requires
    /// `inflight == 0`) never judges it — but nothing wakes it either, so the drain scan must still
    /// find it. `scope_has_undrained_park` reads the poller registry for it, under the registry lock
    /// taken while this core lock A is held; the registry is a leaf (nothing under it takes a core
    /// lock or calls `complete_offload`), so the order A → registry cannot invert.
    /// A cancelled scope whose last unsettled fiber is DEMOTED-blocked forever inside its own cleanup is
    /// therefore a REAL deadlock, and `demote_recv_block`'s self-detect reports it instead of hanging
    /// (`mnsched_cancelled_scope_whose_only_fiber_is_demoted_is_deadlock`).
    pub(super) fn any_cancelled_scope_awaiting_drain(&self, me: &MnSched) -> bool {
        self.cancelled_scope_awaiting_drain(me).is_some()
    }

    /// TICKET-118 (W13-8) — is scope `sid`'s cancel flag itself set, or any ancestor's (an
    /// `Executor`'s `shutdown_now` trips a flag a job's nursery scope carries in
    /// `JoinScope::ancestors`, installed by `Vm::run_one_fiber` as this fiber's `cancel_outer`)?
    pub(super) fn scope_cancel_tripped(&self, sid: usize) -> bool {
        self.scope_cancel(sid).tripped()
    }

    /// TICKET-200 — scope `sid`'s own cancel flag and its ancestors' flags, for
    /// [`ScopeCancel::tripped`].
    pub(super) fn scope_cancel(&self, sid: usize) -> ScopeCancel {
        let s = &self.scopes[sid];
        ScopeCancel {
            own: Arc::clone(&s.cancel),
            ancestors: s.ancestors.clone(),
        }
    }

    /// TICKET-118 (W13-8) — the first scope (if any) whose own flag or an ancestor's is tripped,
    /// still incomplete, and still owing a `cancel_drain` to some parked fiber. `MnSched::take_runnable`
    /// drains it before judging a deadlock; every other caller reads the own-flag-only
    /// `any_cancelled_scope_awaiting_drain` above.
    fn cancelled_scope_awaiting_drain(&self, me: &MnSched) -> Option<usize> {
        #[cfg(test)]
        CANCEL_SCAN_CALLS.fetch_add(1, Ordering::Relaxed);
        self.scopes
            .iter()
            .find(|&(sid, s)| {
                s.done < s.total
                    && self.scope_cancel_tripped(sid)
                    && self.scope_has_undrained_park(me, sid)
            })
            .map(|(sid, _)| sid)
    }

    /// TICKET-126 (W13-24) — the drain scan, skipped while no cancel flag was tripped since this sched's
    /// last scan found nothing. Skipping is sound because a cancelled scope cannot gain a parked fiber
    /// after its trip (`park`/`park_send`/`park_wait` re-check the flags under this lock and requeue,
    /// DEC-118). The generation is recorded only on `None`: a found scope is re-scanned until its drain
    /// empties it. Before this gate the scan ran on every idle pass of every worker (W13-24).
    ///
    /// The load is `Acquire`, against the `Release` bump in [`trip_cancel_flag`]. That pair is what
    /// orders a flag store before this scan, so seeing a newer generation means seeing the flag. The
    /// flag loads in `scope_cancel_tripped` stay `Relaxed`: this one acquire orders them all.
    pub(super) fn drain_scan_due(&mut self, me: &MnSched) -> Option<usize> {
        let cancel_gen = CANCEL_GEN.load(Ordering::Acquire);
        if self.drain_scan_gen == cancel_gen {
            return None;
        }
        let found = self.cancelled_scope_awaiting_drain(me);
        if found.is_none() {
            self.drain_scan_gen = cancel_gen;
        }
        found
    }

    /// Is any fiber of scope `sid` still sitting in `parked`, or parked on `me`'s sockets in the
    /// poller registry, i.e. still owed its `drain_family`? Scans the same structures `drain_family`
    /// empties, in the same way (a `Recv` entry's scope by reference; a `Wait` token's PEEKED under its
    /// fiber lock without claiming; a poll-parked fiber by `poller::any_parked`, TICKET-200) — so no
    /// new state to keep in sync (a counter would have to be set/cleared at every trip + drain seam;
    /// a missed clear re-creates the hang). The registry read takes the registry lock under this core
    /// lock A; the registry is a leaf (see `any_cancelled_scope_awaiting_drain`).
    fn scope_has_undrained_park(&self, me: &MnSched, sid: usize) -> bool {
        if poller::any_parked(me, &self.scope_family(sid)) {
            return true;
        }
        self.parked.values().flatten().any(|e| match e {
            ParkedEntry::Recv(f) => f.woken_by(&[sid]),
            ParkedEntry::Send(f) => f.woken_by(&[sid]),
            ParkedEntry::Wait(wp) => wp
                .fiber
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|f| f.woken_by(&[sid])),
        })
    }

    /// Cross-nursery flat scheduler — some scope has unfinished tasks (the `done < total` half of the
    /// global deadlock predicate).
    pub(super) fn any_scope_incomplete(&self) -> bool {
        self.scopes.values().any(|s| s.done < s.total)
    }

    /// TICKET-125 (exec_join, DEC-112 bullet 3) — every counted fiber on this sched is an owner
    /// blocked at a nested join: it can feed nobody (DEC-099) and has no parked fiber of its own
    /// (`parked_n == 0`), so a caller demanding a visible parked victim before counting this sched
    /// live must not — that demand is exactly what hung `exec_join`/`b4a`/`e10`/`e11` at T>=2.
    pub(super) fn only_blocked_owners(&self) -> bool {
        self.running > 0 && self.running == self.blocked_owners && self.parked_n == 0
    }

    /// TICKET-236 — a waker left the counts a verdict reads: move the run's epoch
    /// ([`quiesce::QuiesceState::note_left`]). Called after the leaver's last channel effect and
    /// inside the lock hold that publishes the leave, so a judge that saw the waker gone also
    /// sees the moved epoch and declines. The methods below are the only ways out of those
    /// counts; a transfer between two counts inside one lock hold needs no bump.
    pub(super) fn note_left(&self) {
        self.left.fetch_add(1, Ordering::SeqCst);
    }

    /// TICKET-236 — THE way off `running` (a park, a yield, a finish, a demote, an offload).
    pub(super) fn leave_running(&mut self) {
        self.running -= 1;
        self.note_left();
    }

    /// TICKET-236 — THE way a task of scope `sid` becomes done.
    pub(super) fn task_done(&mut self, sid: usize) {
        self.scopes[sid].done += 1;
        self.note_left();
    }

    /// TICKET-236 — THE way a scope leaves the table ([`MnSched::retire_scope`]).
    pub(super) fn scope_retired(&mut self, sid: usize) {
        self.note_left();
        self.scopes.remove(sid);
    }

    /// TICKET-236 — THE way an owner blocks at a nested join (DEC-095); `cross_sched` when the
    /// joined sched is not this one.
    pub(super) fn owner_blocked(&mut self, cross_sched: bool) {
        self.blocked_owners += 1;
        if cross_sched {
            self.cross_sched_blocked_owners += 1;
        }
        self.note_left();
    }

    /// TICKET-236 — THE way scope `sid`'s eager body stops injecting for good.
    fn body_closed(&mut self, sid: usize) {
        self.scopes[sid].body_open = false;
        self.note_left();
    }

    /// TICKET-236 — THE writer of scope `sid`'s `body_blocked`, and of its `awaiting_builder`
    /// when `awaiting` is set ([`MnSched::set_body_wait`]). Moves the epoch on block AND on
    /// unblock, so "every write of these two flags moves the epoch" holds with no condition.
    ///
    /// §2c1 — a body parked in a NESTED nursery's join is not merely unable to inject: it WILL
    /// resume the moment that inner scope completes, and may then `send`/`close` to a sibling.
    /// That is exactly what `awaiting_builder` already means, so say it rather than invent a
    /// second flag — `all_incomplete_awaiting_builder` then vetoes when the inner scope is DONE
    /// (the builder is about to resume and feed) and does NOT veto while the inner scope is
    /// itself incomplete-and-stuck (a genuine nested deadlock, which must fault). A body blocked
    /// on a CHANNEL leaves it false: that body resumes only if somebody feeds it, so it is not a
    /// promise of progress.
    fn body_wait(&mut self, sid: usize, blocked: bool, awaiting: bool) {
        self.scopes[sid].body_blocked = blocked;
        if awaiting {
            self.scopes[sid].awaiting_builder = blocked;
        }
        self.note_left();
    }

    /// TICKET-236 — THE way scope `sid` stops awaiting its builder: the builder reached the
    /// scope's join.
    pub(super) fn builder_joined(&mut self, sid: usize) {
        self.scopes[sid].awaiting_builder = false;
        self.note_left();
    }

    /// TICKET-181 — register a blocked waiter; returns its token for [`Self::unregister_waiter`].
    /// Caller holds core lock A.
    pub(super) fn register_waiter(&mut self, w: crate::vm::block::Waiter) -> u64 {
        let tok = self.next_waiter_tok;
        self.next_waiter_tok += 1;
        self.waiters.insert(tok, w);
        tok
    }

    /// TICKET-181 — drop the waiter `tok` names. Caller holds core lock A.
    pub(super) fn unregister_waiter(&mut self, tok: u64) {
        self.waiters.remove(&tok);
    }

    /// TICKET-181, TICKET-236 — the ONE veto of every blocked thing: could any registered waiter
    /// or any parked fiber ([`ParkedEntry::satisfiable`]) already resume? Runs under A and takes only Q and the guard registry G (a leaf), the order `send_wake` uses.
    /// `me` is the sched this core belongs to. A `Join` waiter is answered here, never through
    /// [`quiesce::PartyWait::satisfiable`], which would lock a sched core under this one: a join of
    /// `me` reads `self`, and a join of another sched uses `try_lock` (a busy core answers `true`,
    /// which only declines the verdict).
    pub(super) fn any_blocked_satisfiable(&self, me: &MnSched) -> bool {
        self.waiters.values().any(|w| match &*w.wait {
            quiesce::PartyWait::Join(s, slack) => {
                if w.cancel.iter().any(|f| f.load(Ordering::Relaxed)) {
                    return true;
                }
                if std::ptr::eq(Arc::as_ptr(s), me) {
                    self.join_over(*slack)
                } else {
                    match s.core.try_lock() {
                        Ok(c) => c.join_over(*slack),
                        Err(std::sync::TryLockError::Poisoned(e)) => {
                            e.into_inner().join_over(*slack)
                        }
                        Err(std::sync::TryLockError::WouldBlock) => true,
                    }
                }
            }
            quiesce::PartyWait::Recv(..)
            | quiesce::PartyWait::Send(_)
            | quiesce::PartyWait::Wait(..)
            | quiesce::PartyWait::Nursery(_)
            | quiesce::PartyWait::Guard(..) => w.satisfiable(),
        }) || self.parked.values().flatten().any(ParkedEntry::satisfiable)
    }

    /// TICKET-236 — THE Join predicate: a join of this sched is over when it owes nothing but
    /// the joiner's own `slack` tasks. The join loops and every verdict's Join question read it.
    pub(super) fn join_over(&self, slack: usize) -> bool {
        self.undone_tasks() <= slack
    }

    /// Tasks registered and not yet finished, over every scope.
    pub(super) fn undone_tasks(&self) -> usize {
        self.scopes
            .values()
            .map(|s| s.total.saturating_sub(s.done))
            .sum()
    }

    pub(super) fn release_runner(&mut self, wid: usize) {
        if let Some(b) = self.runner_wids.get_mut(wid) {
            *b = false;
        }
    }

    /// TICKET-181 — is any demoted fiber blocked here (a victim the verdict may claim)?
    pub(super) fn any_fiber_waiter(&self) -> bool {
        self.waiters.values().any(|w| w.fiber)
    }
}

/// What [`MnSched::take_runnable`] hands a worker: the next fiber to run, or `Stop` (this worker
/// should leave the loop — the nursery is done, deadlocked, or otherwise terminated). The size
/// asymmetry is inherent (a `Fiber` is the unit of work and must move out of the queue regardless);
/// boxing the `Run` payload would add an allocation on the schedule hot path for no benefit.
#[allow(clippy::large_enum_variant)]
pub(super) enum Take {
    Run(Fiber, Slice),
    Stop,
}

impl MnSched {
    /// Build the ONE global sched the outermost owner shares with every nested nursery. Seeds scope 0
    /// for the outermost nursery's `total` tasks + its `cancel` (which lives on `scopes[0]` like every
    /// other scope's — there is no sched-level cancel; `park`/`park_wait`/`cancel_drain`/`poll_park_offload`
    /// all use the PER-FIBER scope cancel).
    pub(super) fn new(
        total: usize,
        nworkers: usize,
        cancel: Arc<AtomicBool>,
        deadlock_err: RuntimeError,
        mem_cap: usize,
    ) -> Self {
        // gaps.md W7-58 — its own state by default: no parties, so the judge below never fires.
        // TICKET-236 — a sched that belongs to a run takes the run's state through
        // [`MnSched::join_run`], which also re-pairs the core's epoch handle.
        let quiesce: Arc<quiesce::QuiesceState> = Default::default();
        MnSched {
            core: Mutex::new(SchedCore {
                left: quiesce.epoch(),
                global: std::collections::VecDeque::new(),
                parked: std::collections::HashMap::new(),
                slots: (0..total).map(|_| None).collect(),
                running: 0,
                blocked_owners: 0,
                cross_sched_blocked_owners: 0,
                pool_joiner: None,
                drain_scan_gen: 0,
                parked_n: 0,
                join_parked: Vec::new(),
                scopes: ScopeTable::with_root(JoinScope {
                    base_index: 0,
                    total,
                    done: 0,
                    bytes: 0,
                    body_open: false,
                    body_blocked: false,
                    awaiting_builder: false,
                    cancel: Arc::clone(&cancel),
                    // The creator wires the enclosing scopes' flags in (`Vm::scope_ancestors`)
                    // when this sched is built INSIDE an already-running task.
                    ancestors: Vec::new(),
                    deadlock_err: None,
                    owners_blocked: 0,
                    joins_blocked: 0,
                    parent_scope: None,
                }),
                terminate: false,
                waiters: std::collections::HashMap::new(),
                next_waiter_tok: 0,
                runner_wids: vec![false; nworkers.max(1)],
                exec_tail: 0,
                exec_limit: 0,
                exec_active: 0,
                exec_held: std::collections::VecDeque::new(),
                job_settle: Default::default(),
                leaf_site: false,
                slot_charge: Default::default(),
                unfinished_bytes: 0,
                charged: 0,
            }),
            cv: Condvar::new(),
            deadlock_err,
            runnable: AtomicUsize::new(0),
            locals: (0..nworkers.max(1))
                .map(|_| Mutex::new(LocalQ::new()))
                .collect(),
            steal_ctr: AtomicUsize::new(0),
            inflight: AtomicUsize::new(0),
            mem_cap,
            rng: sched_seed::SeedRng::new(),
            gated_idle: Mutex::new(Vec::new()),
            // TICKET-099 — empty by default; both `MnSched` construction sites assign the run's
            // registry. An empty one is today's behaviour (no peers to wake or veto against).
            sched_registry: Default::default(),
            // TICKET-112 — false by default; `activate_eager_nursery` sets it for a nested sched.
            body_is_fiber: false,
            detached: None,
            // gaps.md W7-56 — empty by default; both `MnSched` construction sites assign the run's
            // registry. An empty one is today's behaviour (no veto).
            exec_registry: Default::default(),
            quiesce,
            idle_cv: Condvar::new(),
            idle_sleepers: AtomicUsize::new(0),
            spinning: AtomicUsize::new(0),
            helpers_live: Mutex::new(0),
            helpers_gone: Condvar::new(),
            #[cfg(test)]
            picks: AtomicUsize::new(0),
            #[cfg(test)]
            recv_side_wakes: AtomicUsize::new(0),
        }
    }

    /// TICKET-236 — make this sched part of the run whose state is `q`: it judges through `q`
    /// and its core moves `q`'s epoch. The ONE place that pairs the two; never assign
    /// `MnSched::quiesce` directly, or the core keeps moving an epoch no judge of the run reads.
    pub(super) fn join_run(&mut self, q: &Arc<quiesce::QuiesceState>) {
        self.quiesce = Arc::clone(q);
        self.core.get_mut().unwrap_or_else(|e| e.into_inner()).left = q.epoch();
    }

    /// TICKET-236 — THE way off `inflight`. Moves the epoch FIRST: `inflight` is read under no
    /// lock, so a bump after the subtraction could land after a judge's closing read.
    pub(super) fn leave_inflight(&self) {
        self.quiesce.note_left();
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }

    /// TICKET-118 (W13-7) — mark this OS thread as the one thread of THIS sched allowed to hand its
    /// bounded-pool slot over while it waits idle on a nursery join. Restores the previous marker
    /// (DEC-052: only ever installed by the top-level `Vm`, `mn.is_none()`) when the guard drops.
    pub(super) fn pool_joiner_guard(self: &Arc<Self>) -> PoolJoinerGuard {
        let prev = self.lock().pool_joiner.replace(std::thread::current().id());
        PoolJoinerGuard {
            sched: Arc::clone(self),
            prev,
        }
    }

    /// TICKET-118 (W13-7) — what the marked joiner (if this thread is one) should do on its next
    /// wait/park pass. `idle_since` tracks how long `running`/`runnable` have both read zero;
    /// resets to `None` the moment either is nonzero.
    ///
    /// TICKET-126 (W13-24) — the id compare goes first and `me` is computed once per `take_runnable`
    /// call outside the lock, because every helper worker is a pool job, so the old order paid two
    /// TLS reads and a `thread::current()` under the core lock on every idle pass (measured +15% at
    /// 28 workers, W13-24). The conjunction is logically identical: `c.pool_joiner != Some(me)` is
    /// true whenever `c.pool_joiner.is_none()` was.
    pub(super) fn joiner_step(
        &self,
        c: &SchedCore,
        me: std::thread::ThreadId,
        idle_since: &mut Option<std::time::Instant>,
    ) -> JoinerStep {
        if c.pool_joiner != Some(me) || !crate::vm::pool::may_yield_slot() {
            return JoinerStep::Untimed;
        }
        if crate::vm::pool::job_waits_for_my_slot() {
            return JoinerStep::Yield;
        }
        if c.running != 0 || self.runnable.load(Ordering::Relaxed) != 0 {
            *idle_since = None;
            return JoinerStep::Timed;
        }
        let now = std::time::Instant::now();
        match *idle_since {
            Some(t) if now.duration_since(t) >= DEMOTE_POLL_BACKOFF => JoinerStep::Yield,
            Some(_) => JoinerStep::Timed,
            None => {
                *idle_since = Some(now);
                JoinerStep::Timed
            }
        }
    }

    /// TICKET-118 (W13-7) — wait according to `step`: `Untimed` parks on `cv` with no deadline,
    /// `Timed` parks for one `DEMOTE_POLL_BACKOFF` tick, `Yield` hands the pool slot to a
    /// replacement (dropping the core lock first — `pool::yield_slot` must never run while holding
    /// it) and re-locks.
    pub(super) fn joiner_wait<'a>(
        &'a self,
        c: std::sync::MutexGuard<'a, SchedCore>,
        idle_since: &mut Option<std::time::Instant>,
    ) -> std::sync::MutexGuard<'a, SchedCore> {
        match self.joiner_step(&c, std::thread::current().id(), idle_since) {
            JoinerStep::Untimed => self.cv.wait(c).unwrap_or_else(|e| e.into_inner()),
            JoinerStep::Timed => {
                self.cv
                    .wait_timeout(c, DEMOTE_POLL_BACKOFF)
                    .unwrap_or_else(|e| e.into_inner())
                    .0
            }
            JoinerStep::Yield => {
                drop(c);
                crate::vm::pool::yield_slot(None);
                self.lock()
            }
        }
    }

    /// Cross-nursery flat scheduler — a NESTED nursery (nested `run_mn_nursery` / eager nursery) enlists
    /// into THIS one global sched. Opens a `JoinScope` whose `base_index` is the current flat
    /// `slots.len()`, extends `slots` by `total` `None`s (its contiguous sub-range), and returns the new
    /// `scope_id`, issued by [`ScopeTable::open`] (never a reissued id). Existing scopes' `base_index`
    /// never shifts, so live fibers' `task_index` stays valid. Holds the core lock so the grow is
    /// atomic against the deadlock predicate. `total` may be 0 for an eager nursery (it grows via
    /// `inject`).
    pub(super) fn register_scope(
        &self,
        total: usize,
        cancel: Arc<AtomicBool>,
        ancestors: Vec<Arc<AtomicBool>>,
    ) -> usize {
        let mut c = self.lock();
        let base_index = c.slots.len();
        c.slots.extend((0..total).map(|_| None));
        let scope_id = c.scopes.open(JoinScope {
            base_index,
            total,
            done: 0,
            bytes: 0,
            body_open: false,
            body_blocked: false,
            awaiting_builder: false,
            cancel,
            ancestors,
            deadlock_err: None,
            owners_blocked: 0,
            joins_blocked: 0,
            parent_scope: None,
        });
        // Cross-nursery flat scheduler — a late `spawn:` into a non-outermost nursery registers a fresh
        // TRAILING scope on the HELD sched (`run_mn_nursery` held-nested branch) AFTER every prior scope
        // finished, which may already have latched global `terminate` (`finish`'s `all_scopes_done`). A
        // newly-registered scope has unfinished work (`done=0 < total`), so the sched is by definition no
        // longer "all done": un-latch `terminate` so the inline owner that will run this scope is not
        // stopped on the stale flag (`take_runnable` checks `terminate` first → `Stop` → the scope would
        // never run and `wait_for_scope` would hang). Harmless on the never-terminated nested path.
        c.terminate = false;
        scope_id
    }

    /// Register a fresh trailing scope AND seed its fibers **atomically under one core lock**. Unlike
    /// `register_scope` followed by a separate `seed` (two locks with a wide gap — `prepare_worker` runs
    /// in between), this guarantees there is never an instant where the scope is visible (`done < total`,
    /// `awaiting_builder == false`) with `runnable == 0`. That window is a false-quiesce a SENTINEL helper
    /// could read via `is_deadlocked` and fault an innocent parked outer sibling — reachable on the
    /// late-spawn-into-middle path where the inline builder driving the registration is NOT counted in
    /// `running`. Mirrors [`MnSched::inject`]'s grow+runnable atomicity contract. `workers` are already
    /// prepared (no fallible/heap work happens inside the lock); each fiber's flat `task_index` is
    /// `base_index + i`, assigned here under the lock.
    pub(super) fn register_scope_seeded(
        &self,
        cancel: Arc<AtomicBool>,
        ancestors: Vec<Arc<AtomicBool>>,
        workers: Vec<ReadyWorker>,
    ) -> usize {
        let total = workers.len();
        let mut c = self.lock();
        let base_index = c.slots.len();
        c.slots.extend((0..total).map(|_| None));
        let scope_id = c.scopes.open(JoinScope {
            base_index,
            total,
            done: 0,
            bytes: 0,
            body_open: false,
            body_blocked: false,
            awaiting_builder: false,
            cancel,
            ancestors,
            deadlock_err: None,
            owners_blocked: 0,
            joins_blocked: 0,
            parent_scope: None,
        });
        // A freshly-registered scope has unfinished work — un-latch any stale global `terminate` (see
        // `register_scope`) so the inline owner that drains it is not stopped on the stale flag.
        c.terminate = false;
        // Seed in the SAME lock: bump `runnable` and queue every fiber before the lock is dropped, so the
        // scope and its runnable fibers become visible together (no `runnable == 0` gap).
        self.runnable.fetch_add(total, Ordering::Relaxed);
        for (i, w) in workers.into_iter().enumerate() {
            c.global.push_back(w.into_fiber(base_index + i, scope_id));
        }
        scope_id
    }

    /// Lock a worker's local run queue (lock class **B**), tolerating poison.
    pub(super) fn lock_local(&self, wid: usize) -> std::sync::MutexGuard<'_, LocalQ> {
        self.locals[wid].lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Lock the core, tolerating poison (a panicking worker must not wedge the rest of the nursery —
    /// same discipline as [`DoneSignal`]).
    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, SchedCore> {
        self.core.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// TICKET-128 (W13-25) — a run-wide wake. Broadcasts `cv` (the joiner and every timed waiter
    /// still listen there) and, only when a worker is actually asleep on `idle_cv`, broadcasts that
    /// too. Every site that used to broadcast `cv` alone for a wake reachable by an idle worker now
    /// calls this instead, so narrowing the idle sleep to its own condvar never strands a sleeper
    /// that the old broadcast would have reached.
    /// TICKET-205 — list a worker's slot as idle (once).
    pub(super) fn idle_register(&self, slot: &Arc<width::Slot>) {
        if slot.is_listed() {
            return;
        }
        let mut g = self.gated_idle.lock().unwrap_or_else(|e| e.into_inner());
        g.push(Arc::clone(slot));
        slot.set_listed(true);
    }

    /// TICKET-205 — the spawner of a gated worker lists the child's slot, and queues it for the
    /// permit when a fiber is already runnable.
    pub(super) fn idle_register_child(&self, slot: &Arc<width::Slot>) {
        let mut g = self.gated_idle.lock().unwrap_or_else(|e| e.into_inner());
        g.push(Arc::clone(slot));
        slot.set_listed(true);
        if self.runnable.load(Ordering::Relaxed) > 0 {
            width::reserve(slot);
        }
    }

    /// TICKET-205 — drop a slot whose thread the OS refused, with any ticket reserved for it.
    pub(super) fn idle_forget(&self, slot: &Arc<width::Slot>) {
        let mut g = self.gated_idle.lock().unwrap_or_else(|e| e.into_inner());
        g.retain(|s| !Arc::ptr_eq(s, slot));
        slot.set_listed(false);
        drop(g);
        width::cancel(slot);
    }

    pub(super) fn notify_waiters(&self) {
        // TICKET-230 — the waker queues gated idle workers only when no permit is free beyond the
        // queued tickets ([`width::full`]): with a free permit the woken worker takes it itself,
        // and a reserved ticket would make it wait behind a thread the OS has not run yet
        // (head-of-line wait). The `gated_idle` walk is skipped with it.
        if self.runnable.load(Ordering::Relaxed) > 0 && width::full() {
            for s in self
                .gated_idle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
            {
                width::reserve(s);
            }
        }
        self.cv.notify_all();
        if self.idle_sleepers.load(Ordering::Relaxed) > 0 {
            self.idle_cv.notify_all();
        }
    }

    /// TICKET-128 (W13-25) — wake exactly one idle worker, damped: a no-op while any worker is
    /// already spinning (D4e's `SPIN_BACKOFF` loop) or none is asleep. This is Go's `wakep`: a
    /// handoff wake's consumer is the worker that keeps running after the handoff (send-side
    /// `park_send`, or nothing at all on the receive side), so no BROADCAST is needed — only a
    /// recruit for the rarer case where that consumer blocks its own thread before reaching the
    /// handed-off fiber.
    pub(super) fn recruit(&self) {
        if self.spinning.load(Ordering::Relaxed) == 0
            && self.idle_sleepers.load(Ordering::Relaxed) > 0
        {
            self.idle_cv.notify_one();
        }
    }

    /// Seed the run queue with the nursery's fibers (task order).
    pub(super) fn seed(&self, fibers: Vec<Fiber>) {
        self.runnable.fetch_add(fibers.len(), Ordering::Relaxed);
        self.lock().global.extend(fibers);
    }

    /// Per-connection `spawn` — inject a freshly-built handler fiber into a LIVE, running sched
    /// (the twin of [`MnSched::complete_offload`], but it ADDS a task rather than re-queuing an
    /// existing one). Assigns the fiber's outcome-slot index ITSELF (the pre-grow `total`) — so the
    /// caller cannot mis-index a slot — then grows `total` + the Decision-F `slots` vec, queues the
    /// fiber on the run queue, and bumps `runnable`, **all under the one core lock** so `total += 1`
    /// is paired atomically with `runnable += 1`: at the instant `total` grows there is a queued
    /// runnable fiber, so the deadlock predicate's `runnable == 0` clause stays sound (no spurious
    /// deadlock from the grow). Indices are assigned under the lock, so a single injector (the
    /// acceptor fiber — the only thing that can inject into ITS scope) gets monotonic spawn-order
    /// indices and `reduce_task_slots` flushes injected-task output deterministically.
    /// Inject into `scope_id` (an eager nursery's scope). The eager nursery owns its OWN sched (a
    /// single scope, the LAST scope), so the new slot is the flat `slots` END — keeping the scope's
    /// `base_index..base_index+total` sub-range contiguous (the contract `reduce`/`take_scope_slots`
    /// rely on). The `debug_assert` pins that invariant: inject only ever targets the last scope, so
    /// growing it never overruns a later scope's range.
    #[cfg(test)]
    pub(super) fn inject(&self, fiber: Fiber, scope_id: usize) {
        let (opened, wids) = self.inject_or_extend(fiber, scope_id);
        for wid in wids {
            self.unclaim_runner(wid);
        }
        debug_assert!(
            opened.is_none(),
            "inject only grows the LAST scope (keeps flat slots contiguous)"
        );
    }

    /// TICKET-103 — [`MnSched::inject`] for a sched whose scope order is NOT LIFO. A fiber may
    /// register its own nursery's scope on this sched (`Vm::activate_fiber_owned_nursery`), after
    /// which `main` or an outer fiber can spawn into its own, now non-last, scope. Growing that scope
    /// would overrun the later scope's slot range, so this opens a *continuation scope* instead: a
    /// new trailing `JoinScope` sharing the target's cancel token (the nursery's *family*, see
    /// `SchedCore::scope_family`), its ancestors and its `deadlock_err`. Returns the continuation's
    /// id when it opened one, `None` when it grew `scope_id` in place (it was the last scope). The
    /// caller must pass its nursery's TAIL scope, so the last scope still owns the slot tail
    /// (`retire_scope`). Same one-lock grow+runnable atomicity as `inject`; like `inject`, it
    /// does not un-latch `terminate`.
    ///
    /// TICKET-211 — also returns the runner wids the new work claims ([`MnSched::claim_runners`],
    /// same lock hold); the caller starts them with `Vm::start_runners`.
    pub(super) fn inject_or_extend(
        &self,
        mut fiber: Fiber,
        scope_id: usize,
    ) -> (Option<usize>, Vec<usize>) {
        let mut c = self.lock();
        let opened = self.reserve_slot(&mut c, &mut fiber, scope_id, 0);
        self.enqueue_locked(&mut c, fiber);
        let wids = self.claim_runners(&mut c);
        drop(c);
        self.notify_waiters();
        (opened, wids)
    }

    /// Make a reserved fiber runnable (the second half of [`MnSched::inject_or_extend`]).
    pub(super) fn enqueue_locked(&self, c: &mut SchedCore, fiber: Fiber) {
        c.global.push_back(fiber);
        self.runnable.fetch_add(1, Ordering::Relaxed);
    }

    /// TICKET-219 — a submitted job, for `Executor.submit`, which starts its runners between the
    /// reserve and this call: [`SchedCore::job_event`] starts it when it is under the cap, and
    /// drops it only under a run halt. Over the cap it is held, and dropped under a run halt or a
    /// cancel (TICKET-232).
    pub(super) fn submit_job(&self, fiber: Fiber) {
        let mut c = self.lock();
        let step = c.job_event(JobEvent::Submit(Box::new(fiber)), self.quiesce.run_halt());
        let started = !step.start.is_empty();
        for f in step.start {
            self.enqueue_locked(&mut c, f);
        }
        drop(c);
        if started || !step.sealed.is_empty() {
            self.notify_waiters();
        }
        self.wake_sealed(step.sealed);
    }

    /// TICKET-219 — `shutdown_now` drops the held jobs ([`JobEvent::DropHeld`]).
    pub(super) fn drop_held(&self) {
        let mut c = self.lock();
        let step = c.job_event(JobEvent::DropHeld, self.quiesce.run_halt());
        drop(c);
        self.wake_sealed(step.sealed);
    }

    /// TICKET-232 — wake the readers of each handle channel [`SchedCore::settle_job`] sealed,
    /// with no sched lock held: [`MnSched::close_wake`] takes it. The seal itself is already
    /// written, inside the transition.
    pub(super) fn wake_sealed(&self, sealed: Vec<Arc<ChannelCore>>) {
        for ch in sealed {
            self.close_wake(Arc::as_ptr(&ch) as usize, &ch);
        }
    }

    /// Claim `fiber`'s outcome slot (the first half of [`MnSched::inject_or_extend`]): it grows the
    /// tail scope, or opens a continuation scope when `scope_id` is no longer the sched's last.
    /// A nonzero `charge` is the task's submit-time bytes, held until [`MnSched::finish`].
    pub(super) fn reserve_slot(
        &self,
        c: &mut SchedCore,
        fiber: &mut Fiber,
        scope_id: usize,
        charge: usize,
    ) -> Option<usize> {
        debug_assert!(
            matches!(fiber.state, FiberState::Pending(_)),
            "an injected handler must be unstarted (Pending) so `run_one_fiber` runs its body via `start_task`"
        );
        let base_index = c.slots.len(); // authoritative flat slot index — the slots END
        let opened = if c.scopes.last_id() == Some(scope_id) {
            c.scopes[scope_id].total += 1;
            fiber.scope_id = scope_id;
            None
        } else {
            let origin = &c.scopes[scope_id];
            let cont = JoinScope {
                base_index,
                total: 1,
                done: 0,
                bytes: 0,
                body_open: false,
                body_blocked: false,
                awaiting_builder: false,
                cancel: Arc::clone(&origin.cancel),
                ancestors: origin.ancestors.clone(),
                deadlock_err: origin.deadlock_err.clone(),
                owners_blocked: 0,
                joins_blocked: 0,
                parent_scope: None,
            };
            let id = c.scopes.open(cont);
            fiber.scope_id = id;
            Some(id)
        };
        fiber.task_index = base_index;
        c.slots.push(None);
        if charge != 0 {
            c.slot_charge.insert(base_index, charge);
            c.unfinished_bytes += charge;
        }
        opened
    }

    /// [`MnSched::reserve_slot`] on the Executor's own tail scope, for `Executor.submit` alone: its
    /// submitters are many parties, so the sched keeps the tail. A `spawn` in a job's nursery
    /// reserves on that nursery's scope instead, so a nursery child is never a job.
    /// `settle` is a handle job's [`JobSettle`] (TICKET-219), registered under its task index.
    pub(super) fn reserve_job_slot(
        &self,
        c: &mut SchedCore,
        fiber: &mut Fiber,
        charge: usize,
        settle: Option<JobSettle>,
    ) -> Option<usize> {
        let tail = c.exec_tail;
        let opened = self.reserve_slot(c, fiber, tail, charge);
        if let Some(id) = opened {
            c.exec_tail = id;
        }
        if let Some(s) = settle {
            c.job_settle.insert(fiber.task_index, s);
        }
        opened
    }

    /// TICKET-211 — THE runner-count decision: the wids the caller must start now (`Vm::start_runners`).
    /// Claims free wids while fewer than `n` (the worker count) are set and fewer fiber runners than
    /// undone tasks run. The body's own thread (bit 0 while scope 0's body is open) counts toward `n`
    /// but runs no fiber. For an Executor (bit 0 never set) this is `min(n, undone)`. Called in the
    /// lock hold that made the work runnable, so a runner leaving (it clears its bit under this
    /// lock) is never counted.
    pub(super) fn claim_runners(&self, c: &mut SchedCore) -> Vec<usize> {
        let budget = c.runner_wids.len() - 1;
        let undone = c.undone_tasks();
        let mut claimed = c.runner_wids.iter().filter(|b| **b).count();
        let body = c.runner_wids[0] && c.scopes.get(0).is_some_and(|s| s.body_open);
        let mut fiber_runners = claimed - usize::from(body);
        let mut out = Vec::new();
        for wid in 1..c.runner_wids.len() {
            if claimed >= budget || fiber_runners >= undone {
                break;
            }
            if !c.runner_wids[wid] {
                c.runner_wids[wid] = true;
                claimed += 1;
                fiber_runners += 1;
                out.push(wid);
            }
        }
        out
    }

    /// TICKET-211 — the body's own thread stops (`blocked`) or resumes running: it gives bit 0 back
    /// or takes it again, in one lock hold. A blocked body returns the runners its freed slot
    /// claims; the caller starts them. A no-op for an Executor, which has no body thread.
    pub(super) fn body_runner(&self, blocked: bool) -> Vec<usize> {
        if self.detached.is_some() {
            return Vec::new();
        }
        let mut c = self.lock();
        c.runner_wids[0] = !blocked;
        if blocked {
            self.claim_runners(&mut c)
        } else {
            Vec::new()
        }
    }

    /// TICKET-211 — may the closed body's thread run fibers as the inline joiner? Only under the
    /// cap: it claims bit 0 when fewer than `n` bits are set.
    pub(super) fn claim_joiner(&self) -> bool {
        let mut c = self.lock();
        let claimed = c.runner_wids.iter().filter(|b| **b).count();
        if claimed < c.runner_wids.len() - 1 {
            c.runner_wids[0] = true;
            true
        } else {
            false
        }
    }

    /// TICKET-211 — the lazy sched's runners are fixed at build -- the owner on wid 0 and pool
    /// helpers on wids 1..n -- so the claim must never start another.
    pub(super) fn mark_all_runners_claimed(&self) {
        self.lock().runner_wids.fill(true);
    }

    /// A runner thread could not start: give its claim back. Returns whether any runner is left.
    pub(super) fn unclaim_runner(&self, wid: usize) -> bool {
        let mut c = self.lock();
        c.runner_wids[wid] = false;
        c.runner_wids.iter().any(|b| *b)
    }

    /// What this sched holds outside every heap: the submit-time bytes of its unfinished tasks plus
    /// its finished tasks' retained output (kept under a live cap only). Takes the core lock.
    pub(super) fn held_bytes(&self) -> usize {
        let c = self.lock();
        c.unfinished_bytes + c.scopes.values().map(|s| s.bytes).sum::<usize>()
    }

    /// W7-26, the sampling half — the growth of the retained-output bytes since the last call, for
    /// the submitting heap's GC pacing counter. A delta: the counter is monotonic between sweeps.
    pub(super) fn take_charge(&self) -> usize {
        let mut c = self.lock();
        let sum: usize = c.scopes.values().map(|s| s.bytes).sum();
        let old = std::mem::replace(&mut c.charged, sum);
        sum.saturating_sub(old)
    }

    /// W7-60 — the buffered output of every task that has ALREADY finished, in slot order, for a
    /// joiner that leaves while other tasks are unfinished. Length-preserving (a running task keeps
    /// its index) and idempotent (a second call returns nothing). TICKET-270 — first waits until
    /// every fiber that called `os.exit` stored its slot ([`quiesce::QuiesceState::await_exit_origins`]); the
    /// caller holds no sched lock.
    pub(super) fn take_finished_streams(&self) -> (Vec<u8>, Vec<u8>) {
        self.quiesce.await_exit_origins();
        let mut c = self.lock();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        for o in c.slots.iter_mut().flatten() {
            let (o_out, o_err) = match o {
                TaskOutcome::Done(wr) => (&mut wr.out, &mut wr.stderr),
                TaskOutcome::Cancelled { out, stderr }
                | TaskOutcome::CancelledFault { out, stderr, .. }
                | TaskOutcome::Exit { out, stderr, .. }
                | TaskOutcome::Fault { out, stderr, .. }
                | TaskOutcome::Deadlocked { out, stderr, .. } => (out, stderr),
            };
            out.append(o_out);
            err.append(o_err);
        }
        for s in c.scopes.values_mut() {
            s.bytes = 0;
        }
        c.charged = 0;
        (out, err)
    }

    /// One tick of an `Executor` join: wait on the sched's condvar while more than `slack` tasks
    /// are undone, for at most `tick`.
    pub(super) fn join_tick(&self, slack: usize, tick: std::time::Duration) {
        let c = self.lock();
        if !c.join_over(slack) {
            drop(
                self.cv
                    .wait_timeout(c, tick)
                    .unwrap_or_else(|e| e.into_inner()),
            );
        }
    }

    /// Per-connection spawn — mark `scope_id`'s (eager) body as still producing tasks: a transient
    /// `done == total` will not terminate it and `is_deadlocked` is vetoed, so farmed workers park
    /// waiting for the next `inject` instead of exiting. Called at `EnterNursery`.
    pub(super) fn open_body(&self, scope_id: usize) {
        self.lock().scopes[scope_id].body_open = true;
    }

    /// Per-connection spawn — `scope_id`'s eager body reached `JoinNursery`: no more injections. Clear
    /// the flag and wake every worker so the run-out-of-work path can terminate (all scopes done) or
    /// fire a genuine deadlock now that the body is no longer live work.
    /// TICKET-211 — closing scope 0 also gives the body's runner bit back: the thread is no longer
    /// the body, and runs fibers only if [`MnSched::claim_joiner`] lets it.
    pub(super) fn close_body(&self, scope_id: usize) {
        {
            let mut c = self.lock();
            c.body_closed(scope_id);
            if scope_id == 0 {
                c.runner_wids[0] = false;
            }
        }
        self.notify_waiters();
    }

    /// §2c1 — a NESTED eager scope has joined and every one of its tasks is done: remove it from the
    /// table and give the slot tail back, so a later `inject` into the enclosing scope can grow it in
    /// place again when it is once more the last scope (`take_scope_slots`' `base..base+total` slice
    /// stays contiguous).
    ///
    /// TICKET-199 — any scope whose `done == total` may be retired, not only the last one: scope
    /// order on one sched is NOT LIFO (DEC-103). A retired middle scope leaves a hole in `slots`
    /// (nothing indexes it — its tasks are all done); `slots` is truncated to the newest live scope's
    /// end after every removal. The retired id is never reissued, so a worker still holding it after
    /// dropping the lock reads an absent scope instead of a foreign one. A no-op for a scope with
    /// unfinished tasks or an id already retired.
    pub(super) fn retire_scope(&self, scope_id: usize) {
        let mut c = self.lock();
        match c.scopes.get(scope_id) {
            Some(s) if s.done == s.total => {}
            _ => return,
        }
        c.scope_retired(scope_id);
        let end = c.scopes.slot_end();
        c.slots.truncate(end);
    }

    /// TICKET-211 — one more runner lease serves this sched.
    pub(super) fn helper_started(&self) {
        *self.helpers_live.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    }

    /// TICKET-211 — a runner lease stopped serving this sched.
    pub(super) fn helper_done(&self) {
        *self.helpers_live.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        self.helpers_gone.notify_all();
    }

    pub(super) fn has_helpers(&self) -> bool {
        *self.helpers_live.lock().unwrap_or_else(|e| e.into_inner()) > 0
    }

    /// TICKET-211 — wait until every runner lease of this sched is done.
    pub(super) fn wait_helpers(&self) {
        let mut n = self.helpers_live.lock().unwrap_or_else(|e| e.into_inner());
        while *n > 0 {
            n = self.helpers_gone.wait(n).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// TICKET-159 — `finish`'s completion latch, for a caller that observed the completion itself:
    /// every scope done and no body open sets `terminate` and wakes every worker, so farmed SENTINEL
    /// helpers exit. A no-op otherwise.
    pub(super) fn latch_terminate_if_done(&self) {
        {
            let mut c = self.lock();
            if c.all_scopes_done() && !c.any_body_open() {
                c.terminate = true;
            }
        }
        self.notify_waiters();
    }

    /// TICKET-159 — scope `sid`'s cancel flag, for a shell that serves it.
    pub(super) fn scope_cancel(&self, sid: usize) -> Option<Arc<AtomicBool>> {
        self.lock().scopes.get(sid).map(|s| Arc::clone(&s.cancel))
    }

    /// §2c1 — register (or drop) a BLOCKED BODY's channel waits as demoted participants of this
    /// sched, so `is_deadlocked_ignoring_jobs`' demoted-queue peek can see them.
    ///
    /// This is what makes the `body_blocked` relaxation SOUND. Lifting `body_open` tells the predicate
    /// "this body cannot inject" — true — but says nothing about the body being the RENDEZVOUS PARTNER
    /// of one of the sched's own parked fibers, which it very often is. Registering the channel puts
    /// the body in the same set the demoted-worker path uses, so the peek vetoes exactly when a value
    /// is already queued for it. Measured: without it, `Channel[int](1)` + `spawn: ch.send(0);
    /// ch.send(1)` + two body `recv`s false-faulted `recv on an empty channel: deadlock` on 4 runs in
    /// 8, against Go's `0 1`.
    ///
    /// Keyed by `Arc::as_ptr(core)`, the same key space `Vm::channel_core_ptr` uses, so a body and a
    /// demoted worker blocked on the SAME channel share one refcounted entry.
    /// **Both halves move under ONE lock acquisition, and that is a correctness requirement, not
    /// tidiness.** Raising `body_blocked` lifts the deadlock veto; registering the channel is what
    /// makes the un-vetoed predicate reach the right answer. Doing them as two acquisitions leaves a
    /// window in which the veto is down and the channel is invisible, and an idle worker sampling
    /// there reaps a perfectly satisfiable parked fiber: measured on `Channel[int](1)` with the
    /// consumer in the `spawn` and the sender in the body, the consumer was killed after ONE `recv`
    /// (`scopes=[(1,1)]`) and `main` then faulted `send on a full channel: deadlock`, 5 runs in 6.
    pub(super) fn set_body_wait(
        &self,
        scope_id: usize,
        wait: Option<&Arc<crate::vm::quiesce::PartyWait>>,
        blocked: bool,
        awaiting: bool,
    ) {
        {
            let mut c = self.lock();
            // ON: publish the wait BEFORE lifting the veto. OFF: drop the veto BEFORE retracting it.
            // Either way the un-vetoed state is never observable without the wait.
            if blocked && let Some(w) = wait {
                c.register_waiter(crate::vm::block::Waiter {
                    wait: Arc::clone(w),
                    cancel: Vec::new(),
                    fiber: false,
                });
            }
            // TICKET-103 — the whole family, in this same acquisition: an unmarked continuation keeps
            // `all_incomplete_awaiting_builder` false and lets the predicate fault a sibling the body
            // feeds after its nested join.
            // TICKET-199 — a retired `scope_id` has an empty family, so this is a no-op for it.
            let family = c.scope_family(scope_id);
            for i in family {
                c.body_wait(i, blocked, awaiting);
            }
            if !blocked
                && let Some(w) = wait
                && let Some(tok) = c
                    .waiters
                    .iter()
                    .find(|(_, x)| Arc::ptr_eq(&x.wait, w))
                    .map(|(t, _)| *t)
            {
                c.unregister_waiter(tok);
            }
        }
        if blocked {
            self.notify_waiters();
        }
    }
}

/// D5 — run an offloaded blocking native off the core worker (on a blocking-pool thread, no `Vm`):
/// serve its already-extracted primitive args through an [`OffloadHost`] and invoke it. The scoped
/// blocking fns read only primitive args + return a primitive [`NativeRet`], so they never touch the
/// heap / host I/O here (the off-heap host `unreachable!`s if one ever does — a misclassification).
pub(super) fn run_offload(
    func: crate::native::NativeFn,
    args: Vec<crate::native::NativeArg>,
) -> Result<crate::native::NativeRet, crate::native::HostError> {
    let mut host = OffloadHost { args };
    func(&mut host)
}

/// W7-16 — wait out an offloaded `sleep_ms` in `DEMOTE_POLL_BACKOFF` chunks, re-arming itself on the
/// timer thread, so a cancel or a `--timeout` reaches the sleeping fiber INSIDE the sleep instead of
/// after it. Pre-fix this was one `submit_at(deadline, …)`: a nursery task sleeping 3 s ran the full
/// 3 s through a sibling's fault at 50 ms and then printed its post-sleep line (measured 3005 ms; the
/// existing cancellation checkpoint only ever covered the *entry* checkpoint, where the fault precedes the call).
///
/// **Why re-arm rather than park where `cancel_drain` can reach it.** `cancel_drain` walks `c.parked`
/// only, and filing this fiber there needs a claim-once token against the timer firing plus
/// `parked_n`/`runnable`/`inflight` all kept consistent — and a parked fiber that no channel can ever
/// feed is exactly the false-deadlock shape W7-12/W7-15 came from. It also requeues with
/// `resume_native == None`, so the resumed fiber would push nothing where the suspended `Call` expects
/// a value. Re-arming keeps the fiber under a SINGLE owner (the timer heap) at all times, so there is
/// no claim race at all.
///
/// **Counters are untouched by design.** `running -= 1` / `inflight += 1` happened once in
/// [`MnSched::offload`]; `complete_offload` runs exactly once, on whichever branch below resumes the
/// fiber. A re-arm does neither, so `inflight > 0` keeps vetoing the deadlock predicate for the whole
/// sleep — the property that makes this preferable to a park.
///
/// Each re-arm targets `min(deadline, now + backoff)` computed from the ABSOLUTE deadline, so tick
/// jitter cannot accumulate and the final wake still lands on `deadline`.
///
/// ponytail: 200 re-arms/s per sleeping fiber, each a `timers` mutex + a poller notify. Thousands of
/// concurrent sleepers would load the single timer/poll thread; upgrade path is a per-scope
/// pending-sleep registry that `cancel_drain` fires directly. (`fire_due_timers` drops the timers lock
/// before running a job, so re-arming from inside a job is safe.)
pub(super) fn arm_timer_sleep(sched: Arc<MnSched>, mut fiber: Fiber, t: TimerSleep, span: Span) {
    let now = std::time::Instant::now();
    // W7-57 — `run_deadline` is evaluated BEFORE `cancel`, and the order is deliberate: a run-wide
    // `os.exit` now trips every scope's cancel (`MnSched::cancel_all`), so an exit racing an already-
    // expired `--timeout` would otherwise be reported as `cancelled` and the harness would lose its
    // `timed_out` marker. `--timeout` is the outer, absolute halt and must outrank. Nothing in-tree
    // pinned the previous order.
    let halt = if t.run_deadline.is_some_and(|rd| now >= rd) {
        Some(
            RuntimeError {
                message: format!("test exceeded --timeout ({}ms)", t.timeout_ms),
                span,
                is_assert: false,
                is_over_memory: false,
                is_timed_out: false,
                is_deadlock: false,
                is_panic: false,
            }
            .timed_out(),
        )
    } else if t.wake.halt().is_some() {
        // TICKET-188 — a cancel, or a child fault of a nursery this fiber owns. `cancelled` is only
        // the resume marker: the resume re-derives the halt through `Vm::take_halt`. A tripped flag
        // with no recorded fault yet (the child is between its trip and `finish`) re-arms below.
        Some(RuntimeError {
            message: "cancelled".to_string(),
            span,
            is_assert: false,
            is_over_memory: false,
            is_timed_out: false,
            is_deadlock: false,
            is_panic: false,
        })
    } else {
        None
    };
    if let Some(err) = halt {
        fiber.resume_native = Some(Err(err));
        sched.complete_offload(fiber);
        return;
    }
    if now >= t.deadline {
        fiber.resume_native = Some(Ok(crate::native::NativeRet::Nil));
        sched.complete_offload(fiber);
        return;
    }
    let mut next = t.deadline.min(now + DEMOTE_POLL_BACKOFF);
    if let Some(rd) = t.run_deadline {
        next = next.min(rd);
    }
    timer::submit_at(
        next,
        Box::new(move || arm_timer_sleep(sched, fiber, t, span)),
    );
}
