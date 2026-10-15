// vm::sched — split out of vm/mod.rs. `super::*` == the `vm` module.
// Spawn targets, runner and helper threads, and the runner/drainer/blocked-owner guards. The spawn and
// nursery code is in `nursery.rs`, the airlock in `airlock_out.rs`/`airlock_in.rs`, the M:N scheduler in `mnsched*.rs`.

use super::*;

/// Where [`Vm::spawn_into`] puts the task it starts.
pub(super) enum SpawnTarget {
    /// The innermost open `parallel:` of the calling party (`spawn`).
    Nursery,
    /// The own tail scope of `sched`, with no nursery open on the caller (`Executor.submit`).
    /// `settle` is a handle job's [`JobSettle`] (TICKET-219, `_submit_settled`).
    Scope {
        sched: Arc<MnSched>,
        settle: Option<JobSettle>,
    },
}

/// What the head of a one-line `spawn <call>` (or of `Executor.submit(f)`) is, and the entry thunk
/// its site compiled (TICKET-235). Read by [`Vm::task_entry`] only.
#[derive(Clone, Copy)]
pub(super) enum SpawnHead {
    /// A callee value. The thunk is used only when the callee pushes no frame by itself; `None` =
    /// the receiverless wrapper closure, which always does.
    Callee(Option<ProtoId>),
    /// A receiver: the thunk's `Op::CallMethod` dispatches the member on it.
    Receiver(ProtoId),
}

/// TICKET-132 — trip a fiber-owned family's cancel and drain its parked/socket-parked fibers.
/// Idempotent: a re-run after a park finds nothing left to trip or drain. Shared by
/// `Vm::park_escaped_abort` and `Vm::abort_fiber_owned_nursery`.
pub(super) fn cancel_fiber_owned_family(scope: &EagerScope) {
    scope.sched.trip_scope_cancel(scope.scope);
    scope.sched.drain_family(scope.scope);
}

/// TICKET-159/211 — wait for every runner lease `Vm::start_runners` handed to `runner_cache` for
/// `sched` (the sched's live-helper count, not `JoinHandle`s: a lease's thread parks for the next
/// sched instead of exiting). Called after `wait_for_completion`, so every scope is done and the
/// body is closed. A SENTINEL helper stops only on `terminate`, and `finish` latches that only for a
/// task that completes while no body is open — when every task finished BEFORE `close_body` nothing
/// else would, so latch it here first.
pub(super) fn join_helpers(sched: &Arc<MnSched>) {
    if !sched.has_helpers() {
        return;
    }
    sched.latch_terminate_if_done();
    sched.wait_helpers();
}

/// TICKET-073 — the one process-wide cap on EXTRA eager runner THREADS a NESTED eager nursery may
/// spend: a per-nursery `chezzi-eager` drainer (`activate_eager_nursery`) and the runner leases
/// `Vm::start_runners` hands to `runner_cache` both draw from it. A lease holds its share until its
/// raw `chezzi-eager-helper` thread parks, and the cache starts a thread only when no parked thread
/// is unpromised, so the cap also bounds the cached runner threads, parked or running. Sized
/// `worker_count().max(2)` so the bound stays linear in `--threads` and independent of nesting depth
/// and fan-out (see `src/vm/pool.rs`).
///
/// TICKET-230 — this caps drainer and helper THREADS only. How many threads RUN Chezzi code at once
/// (the CPU width, main included) is `width::RUNNERS`, alone.
static NESTED_EAGER_DRAINERS: AtomicUsize = AtomicUsize::new(0);

/// TICKET-205 — THE spawn of a raw worker thread of `sched` (the drainer, an eager helper, the
/// replacement of a demoted worker).
pub(super) fn spawn_worker_thread(
    mut shell: Vm,
    sched: &Arc<MnSched>,
    name: &str,
    wid: usize,
    owner_scope: usize,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let s = Arc::clone(sched);
    spawn_runner_thread(sched, name, move || {
        shell.mn_worker_loop(&s, wid, owner_scope)
    })
}

/// TICKET-211 — THE birth of a runner OS thread, shared by [`spawn_worker_thread`] and
/// `runner_cache::start`: width registration (adopted slot), the VM stack size, the thread name and
/// the `catch_unwind` around `body`. Every runner is gated from birth (TICKET-230), and its SPAWNER
/// lists its slot with the sched, so a wake queues it for the runner permit in the waker's order
/// instead of on the child's own start time. A refused thread withdraws the slot.
pub(super) fn spawn_runner_thread(
    sched: &Arc<MnSched>,
    name: &str,
    body: impl FnOnce() + Send + 'static,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let child_slot = width::new_slot();
    sched.idle_register_child(&child_slot);
    let adopted = Arc::clone(&child_slot);
    let r = std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .name(name.into())
        .spawn(move || {
            width::adopt(adopted);
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        });
    if r.is_err() {
        sched.idle_forget(&child_slot);
    }
    r
}

#[cfg(test)]
thread_local! {
    /// TICKET-211 — this OS thread is already counted in `QuiesceState::runner_threads`.
    static IN_RUNNER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// TICKET-211 — counts one runner thread for the span of its outermost `mn_worker_loop`. A loop
/// nested on a thread that already runs one (a fiber's inline join) is inert. Test-only.
/// TICKET-230 — the outermost loop also points this thread's permits at the run's
/// `QuiesceState::width_probe`, restoring the previous probe on drop.
#[cfg(test)]
pub(super) struct RunnerCount(
    Option<Arc<crate::vm::quiesce::QuiesceState>>,
    Option<Arc<width::WidthProbe>>,
);

#[cfg(test)]
impl RunnerCount {
    pub(super) fn enter(q: &Arc<crate::vm::quiesce::QuiesceState>) -> Self {
        if IN_RUNNER.with(|f| f.replace(true)) {
            return RunnerCount(None, None);
        }
        let now = q.runner_threads.fetch_add(1, Ordering::SeqCst) + 1;
        q.peak_runner_threads.fetch_max(now, Ordering::SeqCst);
        let prev = width::set_probe(Some(Arc::clone(&q.width_probe)));
        RunnerCount(Some(Arc::clone(q)), prev)
    }
}

#[cfg(test)]
impl Drop for RunnerCount {
    fn drop(&mut self) {
        if let Some(q) = self.0.take() {
            q.runner_threads.fetch_sub(1, Ordering::SeqCst);
            IN_RUNNER.with(|f| f.set(false));
            width::set_probe(self.1.take());
        }
    }
}

/// TICKET-205 — see [`Vm::gated_register`].
pub(super) struct GatedReg {
    pub(super) cores: Vec<Arc<ChannelCore>>,
    pub(super) me: Arc<width::Slot>,
}
impl Drop for GatedReg {
    fn drop(&mut self) {
        for c in &self.cores {
            c.gated
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|(s, _)| !Arc::ptr_eq(s, &self.me));
        }
        width::cancel(&self.me);
    }
}

/// A held share of [`NESTED_EAGER_DRAINERS`]: one drainer or helper THREAD (TICKET-230: never a
/// runner permit). Released by `Drop`, never by hand: an `EagerScope` moves across `Vm::swap_ctx`
/// and is consumed by `join_eager_nursery` OR `abort_eager_nursery`, and a hand-written release in
/// only those two consumers would leak a slot on every other drop path.
pub(super) struct NestedDrainerSlot;

impl NestedDrainerSlot {
    pub(super) fn acquire() -> Option<Self> {
        NESTED_EAGER_DRAINERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |c| {
                (c < worker_count().max(2)).then_some(c + 1)
            })
            .ok()
            .map(|_| NestedDrainerSlot)
    }
}

impl Drop for NestedDrainerSlot {
    fn drop(&mut self) {
        NESTED_EAGER_DRAINERS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// TICKET-245 — who else is guaranteed to run a joined sched's fibers while its joiner waits;
/// [`Vm::joiner_shell`]'s input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum JoinCover {
    /// nobody is guaranteed to: a lazy join and a fiber-owned join
    None,
    /// the sched's drainer and its started runners: an eager join
    Runners,
}

/// W8-8 — the inline joiner runs fibers only when the budget has a slot left after the drainer.
/// At `n == 1` the drainer already holds the only slot, so the joiner just waits for completion —
/// otherwise `--threads=1` runs two CPU runners and does not serialize.
///
/// T1-fix — a hazard this gate introduces, not fixed here: at `n == 1` the joiner no longer runs any
/// fiber loop of its own, so it is now purely a spectator waiting on the drainer thread. A panic
/// inside `take_runnable`/`park`/`finish` itself (outside `run_one_fiber`'s own inner
/// `catch_unwind`) is swallowed by the drainer thread's OUTER `catch_unwind`
/// (`activate_eager_nursery`'s `spawn(move || { catch_unwind(...) })`); the thread then exits with
/// its scope's slots unfilled, and at `n == 1` there is nothing else left to fill them, so the
/// joiner's `wait_for_completion`/`wait_for_scope` blocks forever. Pre-W8-8 the joiner's own fiber
/// loop covered that. At `n >= 2` a multi-task nursery also has the runners `Vm::start_runners`
/// started on its claim (TICKET-211, raw `chezzi-eager-helper` threads from `runner_cache`, each
/// lease holding a `NestedDrainerSlot`, never the pool), so a dead drainer still leaves runners behind whenever the budget has a
/// slot; only when the budget is empty does the joiner's own loop become the sole cover, closing the
/// window at `n >= 2` purely because the gate lets that loop run. Requires a pre-existing scheduler bug
/// to reach, so no code change here — recorded so the next reader sees the trade.
///
/// **And do not "restore" the old cover without reading what it actually did.** The pre-W8-8 joiner
/// loop did not rescue the lost fiber — nothing can; its slot is gone either way. What it did was sit
/// in `take_runnable` as an idle worker and therefore EVALUATE `is_deadlocked`, whose clause requires
/// `parked_n > 0 || any_fiber_waiter()`. So the old behaviour was: a scheduler-internal panic with at
/// least one parked sibling reported **`deadlock`** — a confidently WRONG verdict for a panicked
/// runtime — and with no parked sibling it hung anyway (the predicate declines, and the joiner's own
/// `take_runnable` then does the same untimed `cv.wait`). The change therefore narrows to: one
/// wrong-verdict case becomes a hang. Per this repo's standing rule that a heuristic verdict must
/// DECLINE rather than emit a confident wrong answer (`docs/gaps.md` W7-12, the `parked-is-not-stuck`
/// line of cases), that is the better of two bad outcomes, not a regression to undo. The real fix is
/// not a joiner loop — it is deciding what the scheduler should do when a worker thread panics at
/// all, which every `catch_unwind` in this file swallows today and which is a design question wider
/// than this gate.
pub(super) fn eager_joiner_runs_fibers(n: usize) -> bool {
    n >= 2
}

/// TICKET-118 (W13-8), extracted from `spawn_into`'s completion closure — poke every live
/// sched of `sched_registry` so an idle worker re-evaluates whatever it is waiting on. The lock is
/// taken and dropped rather than a bare `notify_all`, and that is what makes it reliable: a worker
/// that read the stale state inside its predicate did so under this same core lock, so acquiring it
/// here happens-after that read completes — the worker is either already on the condvar (and gets
/// the notify) or has not yet taken the lock (and will read the new state). A bare notify could land
/// in the gap and be lost. Also called by `shutdown_now`: a cancel store is a wake source too, and
/// without this poke a worker deciding under its own core lock could read the flag on both sides of
/// the trip and park untimed (TICKET-118 plan-validation, 2026-09-15 10:59Z).
pub(super) fn poke_live_scheds(sched_registry: &crate::vm::SchedRegistry) {
    let mut g = sched_registry.lock().unwrap_or_else(|e| e.into_inner());
    let live: Vec<_> = g.iter().filter_map(|w| w.upgrade()).collect();
    g.retain(|w| w.strong_count() > 0);
    drop(g);
    for s in live {
        drop(s.lock());
        s.notify_waiters();
    }
}

/// TICKET-095 — RAII handle for `SchedCore::blocked_owners`. Held across the span in which a fiber's
/// OS thread is blocked inline in a nested nursery join; dropped when the join returns, decrementing
/// the counter and re-notifying idle workers so a scope that just became genuinely stuck (not merely
/// counted-blocked) is re-evaluated. TICKET-099 — also decrements `cross_sched_blocked_owners` when
/// the join was into a CHILD sched (`blocked_owner_guard`'s cross-sched branch).
pub(super) struct BlockedOwnerGuard {
    pub(super) sched: Arc<MnSched>,
    pub(super) cross_sched: bool,
    /// TICKET-103 — the owner fiber's own scope, whose `owners_blocked` this guard holds.
    pub(super) scope: Option<usize>,
    /// TICKET-103 — the same-sched nursery origin scope whose `joins_blocked` this guard holds.
    pub(super) joined: Option<usize>,
}

impl Drop for BlockedOwnerGuard {
    fn drop(&mut self) {
        {
            let mut c = self.sched.lock();
            c.blocked_owners -= 1;
            if self.cross_sched {
                c.cross_sched_blocked_owners -= 1;
            }
            if let Some(s) = self.scope {
                c.scopes[s].owners_blocked -= 1;
            }
            if let Some(j) = self.joined {
                c.scopes[j].joins_blocked -= 1;
            }
        }
        self.sched.notify_waiters();
    }
}
