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

    /// TICKET-118 (W13-8) — is scope `sid`'s cancel flag itself set, or any ancestor's (an
    /// `Executor`'s `cancel` trips a flag a job's nursery scope carries in
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

    /// Cross-nursery flat scheduler — some scope has unfinished tasks (the `done < total` half of the
    /// global deadlock predicate).
    pub(super) fn any_scope_incomplete(&self) -> bool {
        self.scopes.values().any(|s| s.done < s.total)
    }

    /// TICKET-236 — THE way off `running` (a park, a yield, a finish, a demote, an offload).
    pub(super) fn leave_running(&mut self) {
        self.running -= 1;
    }

    /// TICKET-236 — THE way a task of scope `sid` becomes done. A fiber parked in an `Executor`
    /// join of this sched whose join is now over is woken on its own sched (this core is locked,
    /// so [`MnSched::wake_cross`]).
    pub(super) fn task_done(&mut self, sid: usize) {
        self.scopes[sid].done += 1;
        if !self.exec_joiners.is_empty() {
            let undone = self.undone_tasks();
            self.exec_joiners.retain(|(s, slack, key)| {
                if undone > *slack {
                    return true;
                }
                if let Some(s) = s.upgrade() {
                    s.wake_cross(*key);
                }
                false
            });
        }
        if self.root && sid == 0 {
            // `main`'s end as fiber 1 (scope 0 holds it alone): the driving thread takes main's
            // count back ([`Vm::run_root`]).
            self.awake.task_started(1);
        }
        // After the joiners' wakes: a wake counts before the waker's own end.
        self.awake.task_ended();
    }

    /// TICKET-236 — THE way a scope leaves the table ([`MnSched::retire_scope`]).
    pub(super) fn scope_retired(&mut self, sid: usize) {
        self.scopes.remove(sid);
    }

    /// TICKET-236 — THE way scope `sid`'s eager body stops injecting for good.
    fn body_closed(&mut self, sid: usize) {
        self.scopes[sid].body_open = false;
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
    /// other scope's — there is no sched-level cancel; `park`/`park_wait`/`drain_family`/`poll_park_offload`
    /// all use the PER-FIBER scope cancel).
    pub(super) fn new(
        total: usize,
        nworkers: usize,
        cancel: Arc<AtomicBool>,
        deadlock_err: RuntimeError,
        mem_cap: usize,
    ) -> Self {
        // Its own state by default; a sched that belongs to a run takes the run's state through
        // [`MnSched::join_run`].
        let quiesce: Arc<quiesce::QuiesceState> = Default::default();
        MnSched {
            core: Mutex::new(SchedCore {
                listing: Arc::clone(&quiesce.listing),
                awake: Arc::clone(&quiesce.awake),
                root: false,
                global: std::collections::VecDeque::new(),
                parked: std::collections::HashMap::new(),
                slots: (0..total).map(|_| None).collect(),
                running: 0,
                parked_n: 0,
                join_parked: Vec::new(),
                exec_joiners: Vec::new(),
                scopes: ScopeTable::with_root(JoinScope {
                    base_index: 0,
                    total,
                    done: 0,
                    bytes: 0,
                    body_open: false,
                    cancel: Arc::clone(&cancel),
                    // The creator wires the enclosing scopes' flags in (`Vm::scope_ancestors`)
                    // when this sched is built INSIDE an already-running task.
                    ancestors: Vec::new(),
                    deadlock_err: None,
                    parent_scope: None,
                }),
                terminate: false,
                verdict_cut: false,
                waiters: std::collections::HashMap::new(),
                next_waiter_tok: 0,
                runner_wids: vec![false; nworkers.max(1)],
                exec_tail: 0,
                exec_limit: 0,
                exec_active: 0,
                exec_held: std::collections::VecDeque::new(),
                job_settle: Default::default(),
                leaf_site: false,
                charged: 0,
                deadline_armed: None,
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
            // TICKET-112 — `None` by default; an Executor's sched sets it (`Vm::new_detached_sched`).
            detached: None,
            // gaps.md W7-56 — empty by default; both `MnSched` construction sites assign the run's
            // registry. An empty one is today's behaviour (no veto).
            exec_registry: Default::default(),
            quiesce,
            idle_cv: Condvar::new(),
            idle_sleepers: AtomicUsize::new(0),
            spinning: AtomicUsize::new(0),
            visitors: AtomicUsize::new(0),
            #[cfg(test)]
            picks: AtomicUsize::new(0),
            #[cfg(test)]
            recv_side_wakes: AtomicUsize::new(0),
        }
    }

    /// TICKET-236 — make this sched part of the run whose state is `q`: its core counts into
    /// `q`'s awake count and lists into `q`'s listing. The ONE place that pairs them; never assign
    /// `MnSched::quiesce` directly.
    pub(super) fn join_run(&mut self, q: &Arc<quiesce::QuiesceState>) {
        self.quiesce = Arc::clone(q);
        let c = self.core.get_mut().unwrap_or_else(|e| e.into_inner());
        c.listing = Arc::clone(&q.listing);
        c.awake = Arc::clone(&q.awake);
    }

    /// TICKET-236 — THE way off `inflight`.
    pub(super) fn leave_inflight(&self) {
        self.inflight.fetch_sub(1, Ordering::Relaxed);
    }

    /// A scope of `total` slots for a test fixture (the engine registers its scopes seeded,
    /// [`MnSched::register_scope_seeded`]). Opens a `JoinScope` whose `base_index` is the current flat
    /// `slots.len()`, extends `slots` by `total` `None`s (its contiguous sub-range), and returns the new
    /// `scope_id`, issued by [`ScopeTable::open`] (never a reissued id). Existing scopes' `base_index`
    /// never shifts, so live fibers' `task_index` stays valid. Holds the core lock so the grow is
    /// atomic against the deadlock predicate. `total` may be 0 for an eager nursery (it grows via
    /// `inject`).
    #[cfg(test)]
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
            cancel,
            ancestors,
            deadlock_err: None,
            parent_scope: None,
        });
        // Cross-nursery flat scheduler — a late `spawn:` into a non-outermost nursery registers a
        // fresh TRAILING scope on the fiber's own sched (`Vm::activate_fiber_owned_nursery`) AFTER
        // every prior scope finished, which may already have latched global `terminate` (`finish`'s
        // `all_scopes_done`). A newly-registered scope has unfinished work (`done=0 < total`), so
        // the sched is by definition no longer "all done": un-latch `terminate` so the inline owner
        // that will run this scope is not stopped on the stale flag (`take_runnable` checks
        // `terminate` first → `Stop` → the scope would never run and `wait_for_scope` would hang).
        // Harmless on the never-terminated nested path.
        c.terminate = false;
        c.verdict_cut = false;
        scope_id
    }

    /// Register a fresh trailing scope AND seed its fibers **atomically under one core lock**. Unlike
    /// `register_scope` followed by a separate `seed` (two locks with a wide gap — `prepare_worker` runs
    /// in between), this guarantees there is never an instant where the scope is visible (`done < total`)
    /// with its fibers not yet queued and counted awake (`QuiesceState::awake`). Mirrors [`MnSched::inject`]'s grow+runnable atomicity contract. `workers` are already
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
            cancel,
            ancestors,
            deadlock_err: None,
            parent_scope: None,
        });
        // A freshly-registered scope has unfinished work — un-latch any stale global `terminate` (see
        // `register_scope`) so the inline owner that drains it is not stopped on the stale flag.
        c.terminate = false;
        c.verdict_cut = false;
        // Seed in the SAME lock: bump `runnable` and queue every fiber before the lock is dropped, so the
        // scope and its runnable fibers become visible together (no `runnable == 0` gap).
        self.runnable.fetch_add(total, Ordering::Relaxed);
        c.awake.task_started(total);
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

    /// TICKET-205 — list a worker's slot as idle (once).
    pub(super) fn idle_register(&self, slot: &Arc<width::Slot>) {
        if slot.is_listed() {
            return;
        }
        let mut g = self.gated_idle.lock().unwrap_or_else(|e| e.into_inner());
        g.push(Arc::clone(slot));
        slot.set_listed(true);
    }

    /// TICKET-205 — the spawner of a gated worker lists the child's slot and queues it for the
    /// permit when a fiber is already runnable. A child born with nothing runnable is any idle
    /// worker: it lists itself when the gate is tight, else it sleeps untimed (a listed sleeper
    /// polls, so every idle worker of a run born listed polled while one task ran).
    pub(super) fn idle_register_child(&self, slot: &Arc<width::Slot>) {
        if self.runnable.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut g = self.gated_idle.lock().unwrap_or_else(|e| e.into_inner());
        g.push(Arc::clone(slot));
        slot.set_listed(true);
        width::reserve(slot);
    }

    /// TICKET-205 — drop a slot whose thread the OS refused, with any ticket reserved for it.
    pub(super) fn idle_forget(&self, slot: &Arc<width::Slot>) {
        let mut g = self.gated_idle.lock().unwrap_or_else(|e| e.into_inner());
        g.retain(|s| !Arc::ptr_eq(s, slot));
        slot.set_listed(false);
        drop(g);
        width::cancel(slot);
    }

    /// The wake for ONE fiber made runnable (a `spawn`, [`MnSched::inject_or_extend`]): the
    /// gated idle workers queue for the permit as in [`Self::notify_waiters`], the `cv` waiters
    /// re-check, and one idle sleeper is recruited (Go's `newproc` → `wakep`). A broadcast here
    /// woke every idle runner of the run ([`Vm::run_root`] starts `worker_count() - 1`) per
    /// spawn, and the herd took the core lock and the CPUs from the fibers just spawned.
    pub(super) fn notify_new_work(&self) {
        self.reserve_gated_idle();
        self.cv.notify_all();
        self.recruit();
        // A new fiber recruits a host worker even while one visits ([`Self::recruit_host`]):
        // the visitor keeps running the spawner, not the fiber it spawned.
        if self.visitors.load(Ordering::Relaxed) > 0 {
            self.wake_host(false);
        }
    }

    /// An Executor's sched has no thread of its own while the run has a host
    /// ([`quiesce::QuiesceState::host`]): a wake of it with a fiber runnable wakes the host's idle
    /// workers, one per runnable fiber (`all`) or one. The host's core lock is taken first: an
    /// idle host worker reads this sched's `runnable` under it
    /// ([`quiesce::QuiesceState::runnable_guest`]), so it has read the new count or is already
    /// waiting when the notify lands.
    pub(super) fn wake_host(&self, all: bool) {
        if self.detached.is_none() || self.runnable.load(Ordering::Relaxed) == 0 {
            return;
        }
        let Some(host) = self.quiesce.host() else {
            return;
        };
        drop(host.lock());
        host.cv.notify_all();
        if all {
            let n = self.runnable.load(Ordering::Relaxed);
            for _ in 0..n.min(host.idle_sleepers.load(Ordering::Relaxed)) {
                host.idle_cv.notify_one();
            }
        } else {
            host.recruit();
        }
    }

    /// [`Self::wake_host`] for one handed-off fiber (Go's `wakep`): no host worker is recruited
    /// while one visits this sched, as [`Self::recruit`] skips while a worker spins.
    fn recruit_host(&self) {
        if self.visitors.load(Ordering::Relaxed) == 0 {
            self.wake_host(false);
        }
    }

    /// TICKET-230 — the waker queues gated idle workers only when no permit is free beyond the
    /// queued tickets ([`width::full`]): with a free permit the woken worker takes it itself,
    /// and a reserved ticket would make it wait behind a thread the OS has not run yet
    /// (head-of-line wait). The `gated_idle` walk is skipped with it.
    pub(super) fn reserve_gated_idle(&self) {
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
    }

    /// Kick every demoted wait filed on this sched (each [`crate::vm::block::Waiter`]'s
    /// [`crate::vm::block::Wake`]): each re-reads its halts and its cut. THE wake of a demoted
    /// wait for a change its predicate does not read: the one [`super::mnsched_verdict::cancel`]
    /// (every source) and the run's verdict cut. The caller holds no sched core lock.
    pub(super) fn wake_demoted(&self) {
        let wakes: Vec<Arc<crate::vm::block::Wake>> = self
            .lock()
            .waiters
            .values()
            .filter_map(|w| w.wake.clone())
            .collect();
        for w in wakes {
            w.kick();
        }
    }

    pub(super) fn notify_waiters(&self) {
        self.reserve_gated_idle();
        self.cv.notify_all();
        if self.idle_sleepers.load(Ordering::Relaxed) > 0 {
            self.idle_cv.notify_all();
        }
        self.wake_host(true);
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
        self.recruit_host();
    }

    /// Seed the run queue with the nursery's fibers (task order).
    pub(super) fn seed(&self, fibers: Vec<Fiber>) {
        self.runnable.fetch_add(fibers.len(), Ordering::Relaxed);
        let mut c = self.lock();
        c.awake.task_started(fibers.len());
        c.global.extend(fibers);
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
    /// `base_index..base_index+total` sub-range contiguous (the contract `reduce`/`drain_scopes`
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
        let opened = self.reserve_slot(&mut c, &mut fiber, scope_id);
        self.enqueue_locked(&mut c, fiber);
        let wids = self.claim_runners(&mut c);
        drop(c);
        self.notify_new_work();
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

    /// TICKET-219 — `cancel` drops the held jobs
    /// ([`JobEvent::DropHeld`]).
    pub(super) fn drop_held(&self) {
        let mut c = self.lock();
        let step = c.job_event(JobEvent::DropHeld, self.quiesce.run_halt());
        drop(c);
        self.wake_sealed(step.sealed);
    }

    /// TICKET-289 — `Task.cancel()`: cancel the one job whose Task channel is `ch`
    /// ([`JobEvent::CancelOne`]). A held job is dropped and its Task settles; a running job's own
    /// flag is returned: the caller's cancel (`mnsched_verdict::cancel`) trips it and drains the
    /// fibers it wakes on every sched of the run, with no lock held. A job that already ended is a
    /// no-op.
    pub(super) fn cancel_job(&self, ch: &Arc<ChannelCore>) -> Option<Arc<AtomicBool>> {
        let mut c = self.lock();
        let step = c.job_event(JobEvent::CancelOne(Arc::clone(ch)), self.quiesce.run_halt());
        drop(c);
        if !step.sealed.is_empty() {
            self.notify_waiters();
        }
        self.wake_sealed(step.sealed);
        step.cut_flag
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
    pub(super) fn reserve_slot(
        &self,
        c: &mut SchedCore,
        fiber: &mut Fiber,
        scope_id: usize,
    ) -> Option<usize> {
        debug_assert!(
            matches!(fiber.state, FiberState::Pending) && fiber.ctx.start.is_some(),
            "an injected handler must be unstarted (Pending, task in `ctx.start`) so `run_one_fiber` runs its body via `start_task`"
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
                cancel: Arc::clone(&origin.cancel),
                ancestors: origin.ancestors.clone(),
                deadlock_err: origin.deadlock_err.clone(),
                parent_scope: None,
            };
            let id = c.scopes.open(cont);
            fiber.scope_id = id;
            Some(id)
        };
        fiber.task_index = base_index;
        c.slots.push(None);
        // The task is alive from its slot on: a held job too, which ends through `task_done`.
        c.awake.task_started(1);
        opened
    }

    /// [`MnSched::reserve_slot`] on the Executor's own tail scope, for `Executor.submit` alone: its
    /// submitters are many parties, so the sched keeps the tail. A `spawn` in a job's nursery
    /// reserves on that nursery's scope instead, so a nursery child is never a job.
    /// `settle` is the job's [`JobSettle`] (TICKET-219), registered under its task index; the
    /// fiber takes a clone of its cancel flag (TICKET-289).
    pub(super) fn reserve_job_slot(
        &self,
        c: &mut SchedCore,
        fiber: &mut Fiber,
        settle: JobSettle,
    ) -> Option<usize> {
        let tail = c.exec_tail;
        let opened = self.reserve_slot(c, fiber, tail);
        if let Some(id) = opened {
            c.exec_tail = id;
        }
        fiber.job_cancel = Some(Arc::clone(&settle.flag));
        c.job_settle.insert(fiber.task_index, settle);
        opened
    }

    /// TICKET-211 — THE runner-count decision: the wids the caller must start now (`Vm::start_runners`).
    /// Claims free wids while fewer than `n` (the worker count) are set and fewer runners than
    /// undone tasks run: for an Executor this is `min(n, undone)`. Called in the lock hold that made
    /// the work runnable, so a runner leaving (it clears its bit under this lock) is never counted.
    pub(super) fn claim_runners(&self, c: &mut SchedCore) -> Vec<usize> {
        let budget = c.runner_wids.len() - 1;
        let undone = c.undone_tasks();
        let mut claimed = c.runner_wids.iter().filter(|b| **b).count();
        let mut out = Vec::new();
        for wid in 1..c.runner_wids.len() {
            if claimed >= budget || claimed >= undone {
                break;
            }
            if !c.runner_wids[wid] {
                c.runner_wids[wid] = true;
                claimed += 1;
                out.push(wid);
            }
        }
        out
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

    /// What this sched holds outside every heap: its finished tasks' retained output (kept under a
    /// live cap only). M1.7: an unfinished task's payload is objects of the run's one heap, which
    /// `live_bytes` counts, so it is not charged here. Takes the core lock.
    pub(super) fn held_bytes(&self) -> usize {
        let c = self.lock();
        c.scopes.values().map(|s| s.bytes).sum::<usize>()
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
    /// caller holds no sched lock. TICKET-284 — the count is read again under the sched lock, and
    /// the exit output this call drains is counted under it; the third value is that count, which
    /// the caller hands to `Vm::adopt_exit_output`.
    #[must_use]
    pub(super) fn take_finished_streams(&self) -> (Vec<u8>, Vec<u8>, usize) {
        // TICKET-284 — a take counts its exit output under the sched lock, so the count is read
        // again under it: a joiner that took an exit's output after the wait is still counted here.
        let mut c = loop {
            self.quiesce.await_exit_origins();
            // TICKET-284 — test-only: the reader reaches the slots after a joined job took them, the
            // order a loaded host produces (see the hold in `Disp::Finish`).
            #[cfg(test)]
            if crate::vm::HOLD_ADOPTED_EXIT.load(Ordering::Relaxed)
                && self.quiesce.pending() == Some(3)
            {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let c = self.lock();
            if !self.quiesce.exit_origins_open() {
                break c;
            }
        };
        let exits = self.hold_exit_output(c.slots.iter().flatten());
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
        (out, err, exits)
    }

    /// One sleep of a demoted `Executor` join: wait on the sched's condvar while more than `slack`
    /// tasks are undone, until `until` (the run's deadline) or a kick of `wake` since `seen`.
    pub(super) fn join_tick(
        &self,
        slack: usize,
        until: Option<std::time::Instant>,
        wake: &crate::vm::block::Wake,
        seen: u64,
    ) {
        let c = self.lock();
        drop(crate::vm::block::sleep_until(&self.cv, c, until, |c| {
            !c.join_over(slack) && !wake.kicked(seen)
        }));
    }

    /// Per-connection spawn — `scope_id`'s eager body reached `JoinNursery`: no more injections. Clear
    /// the flag and wake every worker so the run-out-of-work path can terminate (all scopes done) or
    /// fire a genuine deadlock now that the body is no longer live work.
    /// TICKET-211 — closing scope 0 also gives the body's runner bit back: the thread is no longer
    /// the body.
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
    /// place again when it is once more the last scope (`drain_scopes`' `base..base+total` slice
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

/// W7-16 — a fiber's offloaded `sleep_ms` parks with ONE timer, so a cancel or a `--timeout`
/// reaches it INSIDE the sleep instead of after it, and nothing polls. Pre-W7-16 a nursery task
/// sleeping 3 s ran the full 3 s through a sibling's fault at 50 ms and then printed its post-sleep
/// line (measured 3005 ms); W7-16 then re-armed the timer every `DEMOTE_POLL_BACKOFF` to read the
/// halts (2000 sleepers: ~2 s of CPU).
///
/// It files the fiber as a [`ParkedEntry::Sleep`] where a cancel or `--timeout` drain finds it
/// (the [`MnSched::drain_parked`] matchers select it like any parked fiber), re-reads the halts,
/// then arms its timer at `min(deadline, run deadline)`. Filing before the read closes the gap: a
/// trip whose drain ran before the filing is seen by the read, any later drain finds the entry.
/// Whichever party claims it ([`SleepPark`]) requeues it through [`settle_sleep`] with the sleep's
/// result in `resume_native`, so the suspended `Call` gets its value.
///
/// **Counters.** `running -= 1` / `inflight += 1` happened once in [`MnSched::offload`];
/// `complete_offload` runs once, in `settle_sleep`. While filed it is in `parked_n`, which the
/// drains' early-outs read, but never asleep in the awake count: its timer will wake it, so a
/// sleep cannot read as a deadlock (the W7-12/W7-15 false-deadlock shape).
pub(super) fn arm_timer_sleep(sched: Arc<MnSched>, fiber: Fiber, t: Arc<TimerSleep>, span: Span) {
    let honours = fiber.honours_cancel();
    let sp = Arc::new(SleepPark {
        fiber: Mutex::new(Some(fiber)),
        sleep: t,
        span,
        sched: Arc::downgrade(&sched),
        claimed: AtomicBool::new(false),
        timer: Arc::default(),
    });
    {
        let mut c = sched.lock();
        c.parked
            .entry(sp.key())
            .or_default()
            .push(ParkedEntry::Sleep(Arc::clone(&sp)));
        c.parked_n += 1;
    }
    if let Some(err) = sleep_halt(honours, &sp.sleep, span) {
        // Already halted: end it now, unless a drain claimed it first (then that drain settles it).
        if let Some(fiber) = sched.claim_sleep(&sp) {
            fiber_resume(&sched, fiber, Err(err));
        }
        return;
    }
    let t = &sp.sleep;
    let due = t.run_deadline.map_or(t.deadline, |rd| t.deadline.min(rd));
    let stop = Arc::clone(&sp.timer);
    timer::submit_stoppable_at(
        due,
        // The job holds the sched weakly: a stopped entry waits in the heap for its removal
        // ([`timer::stop`]), and it must not keep a finished sched alive until then.
        Box::new(move || {
            if let Some(sched) = sp.sched.upgrade()
                && let Some(fiber) = sched.claim_sleep(&sp)
            {
                settle_sleep(&sched, fiber, &sp);
            }
        }),
        stop,
    );
}

/// The halt that ends a sleep now, in this order. W7-57 — `run_deadline` BEFORE `cancel`: a
/// run-wide `os.exit` trips every scope's cancel (`cancel(CancelTarget::All)`), so an exit racing
/// an already-expired `--timeout` would otherwise be reported as `cancelled` and the harness would
/// lose its `timed_out` marker. `--timeout` is the outer, absolute halt and must outrank.
/// TICKET-188 — a cancel, or a child fault of a nursery this fiber owns; not inside a `defer`
/// (`honours`, [`Fiber::honours_cancel`]): a cleanup's sleep runs its full length. `cancelled` is
/// only the resume marker: the resume re-derives the halt through `Vm::take_halt`. A tripped flag
/// with no recorded fault yet is no halt.
fn sleep_halt(honours: bool, t: &TimerSleep, span: Span) -> Option<RuntimeError> {
    if t.run_deadline
        .is_some_and(|rd| std::time::Instant::now() >= rd)
    {
        return Some(super::exec::timed_out_err(t.timeout_ms, span));
    }
    (honours && t.wake.halt().is_some()).then(|| RuntimeError {
        message: "cancelled".to_string(),
        span,
        is_assert: false,
        is_over_memory: false,
        is_timed_out: false,
        is_panic: false,
    })
}

/// The claim's winner ends the sleep `sp` it took `fiber` from: with its halt, at its deadline
/// with `Nil`, or, woken by a drain with neither due (a flag of a nursery it owns tripped with no
/// fault), files it again with a new token and timer. It stops the old timer first, so a drain's
/// win leaves no live entry in the timer heap. Holds no lock.
pub(super) fn settle_sleep(sched: &Arc<MnSched>, fiber: Fiber, sp: &SleepPark) {
    // A drain won the claim: the timer is dead. A no-op when the timer won.
    timer::stop(&sp.timer);
    if let Some(err) = sleep_halt(fiber.honours_cancel(), &sp.sleep, sp.span) {
        fiber_resume(sched, fiber, Err(err));
    } else if std::time::Instant::now() >= sp.sleep.deadline {
        fiber_resume(sched, fiber, Ok(crate::native::NativeRet::Nil));
    } else {
        arm_timer_sleep(Arc::clone(sched), fiber, Arc::clone(&sp.sleep), sp.span);
    }
}

/// Requeue a fiber whose offloaded native ended with `result`.
fn fiber_resume(
    sched: &MnSched,
    mut fiber: Fiber,
    result: Result<crate::native::NativeRet, RuntimeError>,
) {
    fiber.resume_native = Some(result);
    sched.complete_offload(fiber);
}

impl MnSched {
    /// Claim the filed sleep `sp` for the caller (its timer, or its own halt re-read): in one core
    /// lock hold, win the CAS, take the entry out of `parked` and the fiber out of `sp`. `None`
    /// when a drain claimed it first.
    fn claim_sleep(&self, sp: &Arc<SleepPark>) -> Option<Fiber> {
        let mut c = self.lock();
        if sp
            .claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            debug_assert!(
                !c.parked.contains_key(&sp.key()),
                "a claimed sleep is still filed"
            );
            return None;
        }
        let filed = c.parked.remove(&sp.key());
        debug_assert!(
            matches!(filed.as_deref(), Some([ParkedEntry::Sleep(e)]) if Arc::ptr_eq(e, sp)),
            "an unclaimed sleep is filed alone under its key"
        );
        c.parked_n -= 1;
        let fiber = sp.fiber.lock().unwrap_or_else(|e| e.into_inner()).take();
        Some(fiber.expect("SleepPark fiber claimed twice"))
    }
}
