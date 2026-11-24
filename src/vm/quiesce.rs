//! The run's deadlock count and run-wide halts.
//!
//! Go's detector (`fatal error: all goroutines are asleep - deadlock!`): [`Awake`] counts the
//! tasks of the run that are not asleep. It changes only at a task's start, park, wake and end;
//! the -1 that takes it to 0 with tasks still alive calls [`super::MnSched::on_deadlock`], the one
//! verdict. [`QuiesceState`] holds the count, the run's listing and the run-wide halts (`os.exit`,
//! a job's hard abort, the latched verdict) every party reads through [`QuiesceState::run_halt`].
//!
//! # Not a `static`
//!
//! Per-run state, held behind an `Arc` on the `Vm` and shared with every worker by
//! `Vm::spawn_worker`, exactly like [`ExecRegistry`] and for exactly the same reason: `cargo test`
//! runs many programs concurrently in ONE process, and a process-global registry would let one run's
//! state be counted against another run's.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// TICKET-223 — the latched deadlock verdict ([`QuiesceState::latch_deadlock`]).
#[derive(Default)]
struct DeadlockCell {
    decided: bool,
    /// The latched verdict's id: each latch takes the next, so a `chezzi test` test's verdict
    /// and a later test's are told apart ([`QuiesceState::verdict`]).
    id: u64,
}

/// One task in `Awake`'s `alive` half (the high 32 bits); `awake` is the low 32 bits.
const ONE_ALIVE: u64 = 1 << 32;

/// The run's count of tasks that are not asleep, Go's `checkdead` count: `alive` tasks (started,
/// not ended) and `awake` tasks among them (not parked in a bucket or at a join). It changes only
/// at a task's start, park, wake and end, through the four methods below, which are its only
/// writers. Both halves live in one atomic word, so every change is one RMW and every reader sees
/// both at one instant. A task waiting in place (main at a party wait, a demoted waiter) stays
/// awake: its wakers only notify a condvar. A wake adds its +1 inside the lock hold that
/// publishes it, before the waker can park or end, as Go's `goready` runs before the M goes idle.
///
/// `awake` reaching 0 while `alive > 0` means every task is asleep: the caller whose write took
/// it there runs the run's verdict ([`super::MnSched::on_deadlock`]).
///
/// A pending timer is a task of the count while a task is parked on it
/// ([`super::core::ChannelCore::timer_wait`]), as Go 1.23's `checkdead` counts a timer only while
/// a goroutine waits on its channel: a timer a `wait:` left behind keeps nothing awake.
#[derive(Default)]
pub(super) struct Awake(AtomicU64);

impl Awake {
    /// `n` new tasks, awake: before their parent continues.
    pub(super) fn task_started(&self, n: usize) {
        if n > 0 {
            let d = n as u64 * (ONE_ALIVE + 1);
            Self::check(self.0.fetch_add(d, Ordering::SeqCst) + d);
        }
    }

    /// An awake task ended. True when that took `awake` to 0 with tasks still alive.
    pub(super) fn task_ended(&self) -> bool {
        let prev = self.0.fetch_sub(ONE_ALIVE + 1, Ordering::SeqCst);
        debug_assert!(prev & u64::from(u32::MAX) > 0, "an ended task was awake");
        Self::verdict(prev.wrapping_sub(ONE_ALIVE + 1))
    }

    /// An awake task parked: after its wait is registered, under the bucket's lock. True when
    /// that took `awake` to 0 with tasks still alive.
    pub(super) fn asleep(&self) -> bool {
        let prev = self.0.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(prev & u64::from(u32::MAX) > 0, "a parking task was awake");
        Self::verdict(prev.wrapping_sub(1))
    }

    /// `n` parked tasks made runnable, inside the lock hold that requeues them.
    pub(super) fn woken(&self, n: usize) {
        if n > 0 {
            Self::check(self.0.fetch_add(n as u64, Ordering::SeqCst) + n as u64);
        }
    }

    /// Whether every alive task is asleep now: the verdict's own condition, read again by
    /// [`super::MnSched::on_deadlock`] after its deadline drains.
    pub(super) fn all_asleep(&self) -> bool {
        Self::verdict(self.0.load(Ordering::SeqCst))
    }

    /// Whether the count drains to no task alive within 2 s: the run's end, for a debug assert.
    /// It waits because a token task (a deferred wake, a task's end with its wakes) is released
    /// just after the wake it covers, so the woken main can reach its end first.
    pub(super) fn drains(&self) -> bool {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while self.0.load(Ordering::SeqCst) != 0 {
            if std::time::Instant::now() > until {
                return false;
            }
            std::thread::yield_now();
        }
        true
    }

    /// `awake <= alive` after a write.
    fn check(now: u64) {
        debug_assert!(
            now & u64::from(u32::MAX) <= now >> 32,
            "awake {} > alive {}",
            now & u64::from(u32::MAX),
            now >> 32
        );
    }

    fn verdict(now: u64) -> bool {
        Self::check(now);
        now & u64::from(u32::MAX) == 0 && now >> 32 > 0
    }
}

/// The run's registry of blocked parties. One per `Vm::new`, shared by `Arc` with every worker.
#[derive(Default)]
pub(super) struct QuiesceState {
    /// The run's awake count. Each `SchedCore` of the run holds the same `Arc`
    /// ([`super::MnSched::join_run`]).
    pub(super) awake: Arc<Awake>,
    /// The run's deadlock listing: every task the verdict cut, added at its cut. Each `SchedCore` of
    /// the run holds the same `Arc` ([`MnSched::join_run`]).
    pub(super) listing: Arc<super::deadlock::Listing>,
    /// The one latch between the deadlock verdict and the `--timeout` deadline timer, a waker
    /// the count does not hold ([`super::MnSched::arm_deadline_wake`]). The verdict holds it from
    /// its deadline drains to its latch; the timer holds it while it requeues, and requeues nothing
    /// once the verdict latched. So whichever takes it first wins: the verdict never latches over a
    /// task the timer requeued, and the timer never requeues a task the verdict lists. Taken with no
    /// lock held; a `SchedCore` lock is taken under it.
    verdict_gate: Mutex<()>,
    /// The run's root sched while its root fiber runs ([`super::Vm::run_root`]): its workers are
    /// the run's one set of runner threads, and they run the Executors' jobs too (Go: one pool of
    /// Ms), so an Executor starts no thread of its own while a host is set.
    host: Mutex<std::sync::Weak<super::MnSched>>,
    /// Every Executor sched of the run, by `Weak`: the scheds whose runnable fibers an idle host
    /// worker takes ([`Self::runnable_guest`]). A leaf lock: nothing is locked under it.
    guests: Mutex<Vec<std::sync::Weak<super::MnSched>>>,
    /// Where the next [`Self::runnable_guest`] scan starts: the guest a scan last found, so a
    /// visit that gives its thread back to another guest hands it to that one.
    guest_turn: std::sync::atomic::AtomicUsize,
    /// The run-wide `os.exit` request (W7-47). `os.exit` writes `pending_exit` on whichever `Vm` ran
    /// the native, which for an eager `Executor` job is that job's isolated worker — a value nobody
    /// observes until the join. A party blocked in a socket/channel wait never reaches the join, so
    /// the request is published HERE too, where every blocking loop can see it (`Vm::run_exit_err`).
    ///
    /// **`Mutex<Option<i32>>`, not an `AtomicI32`**: this struct derives `Default`, and an atomic
    /// would default to `0` — i.e. "exit code 0 is pending" on every fresh run.
    ///
    /// **The cell is per-RUN, and `chezzi test` treats each `test fn` as its own run** — one `Vm` is
    /// built per test FILE and reused across every test in it (`invoke_all`), so without a reset a
    /// `test fn` that calls `os.exit` would latch the cell and halt every LATER test that blocks.
    /// [`Vm::invoke_test`] clears it, beside the other per-test resets.
    exit: Mutex<Option<i32>>,
    /// TICKET-270, TICKET-284 — the exit output in flight ([`ExitOrigins`]). A flush of finished
    /// output ([`super::MnSched::take_finished_streams`]) waits on `exit_origins_cv` until `open` is
    /// 0, so it never reads a slot before the exit output that belongs there. [`Self::clear_exit`]
    /// does not reset it: each party ends its own count when it stores its slot.
    exit_origins: Mutex<ExitOrigins>,
    exit_origins_cv: std::sync::Condvar,
    /// gaps.md W7-57 — the lock-free mirror of `exit.is_some()`, for the two CPU-side checkpoints that
    /// cannot afford a mutex: `jump_checked`'s loop back-edge (sampled 1/1024) and `guarded`'s
    /// per-element native-HOF re-entry. A party that is spinning in a loop or grinding through a
    /// `map`/`fold` reaches no blocking wait at all, so the `Mutex<Option>` above — read only by the
    /// demote/poll loops — never reaches it and the run hangs forever, or finishes work Go would not.
    ///
    /// Stored **`Release` AFTER** the code, at the start of [`super::Vm::halt_run`] (before the
    /// scheds are halted), and loaded `Acquire` — which publishes the code to whoever reads `true`,
    /// and nothing more. The window left is the gap between the cell store and this store: a
    /// function entry, a loop back-edge and the return of a call that held its thread
    /// (`Vm::return_halts`) read the hint and miss a halt published in it, and see it at their next
    /// read; a `defer` pop reads the cell itself for that reason (`stmt.rs` `has_next_deferred`,
    /// TICKET-213). **It does NOT order the two rungs against each other**: an acquire orders only
    /// the reads that FOLLOW it, and both CPU checkpoints read cancel BEFORE exit, so a stale
    /// `cancel == false` beside `exit == true` is a legal interleaving. An earlier revision leaned on
    /// that ordering to keep a scoped fiber on the `Cancelled` path and it was simply wrong — measured
    /// as a sibling `defer` running 2/8 times, once truncated mid-body. [`super::Vm::exit_halt`]
    /// decides from the flag's PRESENCE instead, which needs no ordering at all.
    ///
    /// **Only ever a HINT.** The `exit` `Mutex` above is the authority: every reader confirms
    /// `pending()` before acting, so a stale `true` costs one uncontended lock per 1024 back-edges and
    /// nothing else. [`clear_exit`](Self::clear_exit) still clears it (paired with the cell it
    /// mirrors, and it saves that lock), but correctness does not rest on that store — it rests on the
    /// cell, which every `chezzi test` entry point resets via `Vm::reset_for_invoke`.
    run_halt_hint: AtomicBool,
    /// TICKET-208 — the `--max-heap` abort of an `Executor` job: the second cause of a run-wide
    /// halt, beside `exit`. Read through the one funnel `Vm::run_exit_err`, taken by `Vm::rank_end`.
    job_fault: Mutex<JobFaultCell>,
    /// Wakes [`Self::take_job_fault`] when the publisher's teardown is over.
    job_fault_cv: std::sync::Condvar,
    /// TICKET-223 — the deadlock verdict, the third cause of a run-wide halt. Latched once by
    /// [`Self::decide`] under the party lock; read through `Vm::run_exit_err`; its report is taken
    /// by `Vm::rank_end`. Reset only by [`Self::clear_exit`].
    deadlock: Mutex<DeadlockCell>,
    /// TICKET-211 — runner threads of this run inside `Vm::mn_worker_loop` right now, one per OS
    /// thread however deep its loops nest (`sched::RunnerCount`). Test-only.
    #[cfg(test)]
    pub(super) runner_threads: std::sync::atomic::AtomicUsize,
    /// TICKET-211 — the high-water mark of `runner_threads`. Test-only.
    #[cfg(test)]
    pub(super) peak_runner_threads: std::sync::atomic::AtomicUsize,
    /// TICKET-211 — runner OS threads this run started (`sched::spawn_runner_thread`). Test-only.
    #[cfg(test)]
    pub(super) runner_spawns: std::sync::atomic::AtomicUsize,
    /// TICKET-230 — this run's runner permit holders, now and at peak (`width::WidthProbe`).
    /// Test-only.
    #[cfg(test)]
    pub(super) width_probe: Arc<super::width::WidthProbe>,
}

/// One job fault's state. One abort is one publish, one teardown and one report, however many
/// jobs end carrying a copy of it.
#[derive(Default)]
struct JobFaultCell {
    /// The fault, until `Vm::rank_end` takes it for the report.
    fault: Option<(super::RuntimeError, Vec<super::TraceFrame>)>,
    /// The run is halted by a job fault. Outlives the report: tasks still unwind after it, and a
    /// join reads this to stop waiting ([`RunHalt::ends_join`]). Reset by `clear_exit`.
    raised: bool,
    /// The publisher has not finished `Vm::halt_run`. The report waits for it: a teardown that
    /// lands later would cancel the reap that follows the report.
    halting: bool,
}

/// TICKET-219 — the kind of run-wide halt in force ([`QuiesceState::run_halt`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum RunHalt {
    Running,
    /// A job's `--max-heap` abort: every `defer` still runs whole.
    Fault,
    /// An `os.exit`: no `defer` runs in any party.
    Exit,
    /// TICKET-223 — a latched deadlock verdict: no `defer` runs (TICKET-152), no handle settles.
    Deadlock,
}

impl RunHalt {
    /// Whether this halt settles the held job handles. An exit and a deadlock settle nothing.
    pub(super) fn settles(self) -> bool {
        !matches!(self, RunHalt::Exit | RunHalt::Deadlock)
    }

    /// THE answer to which halts cut a `defer`: an exit (TICKET-213) and a latched verdict
    /// (TICKET-152) stop a running `defer` and every queued one; a job fault waits for the cleanup.
    /// `Vm::run_halt_due` and `Vm::has_next_deferred` read it.
    pub(super) fn cuts_cleanup(self) -> bool {
        matches!(self, RunHalt::Exit | RunHalt::Deadlock)
    }

    /// Does this halt end a nursery join's wait for its children? As in Go, nothing waits for a
    /// task once the run is over: its workers leave, so a queued child may never run. A deadlock
    /// verdict is not one: it ends the join's children itself, and they settle.
    pub(super) fn ends_join(self) -> bool {
        match self {
            RunHalt::Fault | RunHalt::Exit => true,
            RunHalt::Running | RunHalt::Deadlock => false,
        }
    }
}

impl QuiesceState {
    /// Publish an `os.exit`. First writer wins, exactly like Go: whichever `os.Exit` runs first sets
    /// the status, and a later one cannot rewrite it.
    pub(super) fn request_exit(&self, code: i32) {
        let mut g = self.exit.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(test)]
        let hold = code == RUN_END_WINDOW_EXIT && g.is_none();
        g.get_or_insert(code);
        drop(g);
        #[cfg(test)]
        if hold {
            hold_run_end_window();
        }
    }

    /// The pending run-wide exit code, if any.
    pub(super) fn pending(&self) -> Option<i32> {
        *self.exit.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// TICKET-270 — a fiber called `os.exit`. The host `request_exit` calls this BEFORE it publishes
    /// the code, so a joiner that reads the code also sees the count.
    pub(super) fn begin_exit_origin(&self) {
        let mut g = self.exit_origins.lock().unwrap_or_else(|e| e.into_inner());
        g.open += 1;
        g.begun += 1;
    }

    /// TICKET-270 — an exiting fiber's `MnSched::finish` returned: its slot holds its output.
    pub(super) fn end_exit_origin(&self) {
        let mut g = self.exit_origins.lock().unwrap_or_else(|e| e.into_inner());
        g.open = g.open.saturating_sub(1);
        drop(g);
        self.exit_origins_cv.notify_all();
    }

    /// TICKET-284 — is an exit's output still in flight? `MnSched::take_finished_streams` asks it
    /// under the sched lock, the lock every taker counts under.
    pub(super) fn exit_origins_open(&self) -> bool {
        self.exit_origins
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .open
            > 0
    }

    /// TICKET-284 — how many exit counts ever began. The run-end pass reads it before and after;
    /// a change means exit output moved while the pass read, so the pass repeats.
    pub(super) fn exit_origins_begun(&self) -> u64 {
        self.exit_origins
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .begun
    }

    /// TICKET-270 — wait until every fiber that called `os.exit` stored its slot. Bounded: an
    /// origin runs no `defer` and joins nothing after the call (DEC-213), so it reaches `finish`
    /// at once. The caller must hold no sched lock.
    pub(super) fn await_exit_origins(&self) {
        let mut g = self.exit_origins.lock().unwrap_or_else(|e| e.into_inner());
        while g.open > 0 {
            g = self
                .exit_origins_cv
                .wait(g)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// W7-57 — latch the back-edge flag. Called by `Vm::halt_run` AFTER the code is published and
    /// before the scheds are halted; see the field's doc for the window that order leaves.
    pub(super) fn mark_run_halt(&self) {
        self.run_halt_hint.store(true, Ordering::Release);
    }

    /// W7-57 — the lock-free "is there a run-wide exit?" the CPU-side checkpoints ask, so they need no
    /// mutex on the hot path. A `true` is only a HINT: `Vm::exit_halt` and `Vm::run_exit_err` both
    /// confirm `pending()` before acting, so a spurious `true` is a self-healing no-op.
    pub(super) fn run_halt_hint(&self) -> bool {
        self.run_halt_hint.load(Ordering::Acquire)
    }

    /// Drop a latched exit — the per-invocation reset (`Vm::reset_for_invoke`, shared by every
    /// `chezzi test` entry point); see the field's doc. The `exit` cell is the load-bearing half; the
    /// atomic is cleared with it to keep the mirror honest and to save the confirming lock.
    pub(super) fn clear_exit(&self) {
        *self.exit.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.deadlock
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .decided = false;
        // The hint covers a pending job fault too, and this reset never clears that.
        let mut g = self.job_fault.lock().unwrap_or_else(|e| e.into_inner());
        g.raised = g.fault.is_some();
        self.run_halt_hint.store(g.raised, Ordering::Release);
    }

    /// TICKET-208 — publish an `Executor` job's `--max-heap` abort: it ends the whole run. First
    /// writer wins (Go: the first panic is the report). `true` for that writer only: it then runs
    /// `Vm::halt_run`, which latches the hint ([`Self::mark_run_halt`]) and halts every sched, the
    /// order `request_exit` uses, and calls [`Self::end_job_halt`]. A later job that ends with a
    /// copy of the fault is the same abort, so it publishes nothing and halts nothing.
    pub(super) fn request_job_fault(
        &self,
        err: super::RuntimeError,
        trace: Vec<super::TraceFrame>,
    ) -> bool {
        #[cfg(test)]
        let hold = err.message.contains(RUN_END_WINDOW_MARK);
        let mut g = self.job_fault.lock().unwrap_or_else(|e| e.into_inner());
        if g.raised {
            return false;
        }
        g.raised = true;
        g.halting = true;
        g.fault = Some((err, trace));
        drop(g);
        #[cfg(test)]
        if hold {
            hold_run_end_window();
        }
        true
    }

    /// The publisher of a job fault finished its teardown: the report may be taken.
    pub(super) fn end_job_halt(&self) {
        self.job_fault
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .halting = false;
        self.job_fault_cv.notify_all();
    }

    /// The pending job fault, if any.
    pub(super) fn job_fault(&self) -> Option<(super::RuntimeError, Vec<super::TraceFrame>)> {
        self.job_fault
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fault
            .clone()
    }

    /// TICKET-219 — the run halt's kind, the one source `Vm::run_halt_due` and
    /// `SchedCore::job_event` read. An exit outranks a job fault: `os.exit` runs no `defer`.
    /// A latched verdict outranks a job fault, because a verdict cuts a `defer` and a job fault does
    /// not (TICKET-223). The report order is separate: `run_exit_err` and `rank_end` still put a
    /// job fault first (DEC-200).
    pub(super) fn run_halt(&self) -> RunHalt {
        if self.pending().is_some() {
            RunHalt::Exit
        } else if self
            .deadlock
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .decided
        {
            RunHalt::Deadlock
        } else if self
            .job_fault
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .raised
        {
            RunHalt::Fault
        } else {
            RunHalt::Running
        }
    }

    /// [`Self::verdict_gate`], held for the caller's whole check-then-act.
    pub(super) fn verdict_gate(&self) -> std::sync::MutexGuard<'_, ()> {
        self.verdict_gate.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The deadlock verdict's latch ([`super::MnSched::on_deadlock`]): the run halts as
    /// `RunHalt::Deadlock`; returns the verdict's id. `None` when a verdict is latched already. It
    /// never un-latches but at [`Self::clear_exit`] (a `chezzi test` test's own run).
    pub(super) fn latch_deadlock(&self) -> Option<u64> {
        let mut cell = self.deadlock.lock().unwrap_or_else(|e| e.into_inner());
        if cell.decided {
            return None;
        }
        cell.decided = true;
        cell.id += 1;
        let id = cell.id;
        drop(cell);
        self.run_halt_hint.store(true, Ordering::Release);
        Some(id)
    }

    /// The latched verdict's id, while one is latched.
    pub(super) fn verdict(&self) -> Option<u64> {
        let cell = self.deadlock.lock().unwrap_or_else(|e| e.into_inner());
        cell.decided.then_some(cell.id)
    }

    pub(super) fn has_job_fault(&self) -> bool {
        self.job_fault
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fault
            .is_some()
    }

    /// Wait until a job fault's publisher finished its teardown. Bounded: `Vm::halt_run` waits for
    /// no task. `Vm::rank_end` calls it in the GC door, before it takes the report.
    pub(super) fn await_job_halt(&self) {
        let mut g = self.job_fault.lock().unwrap_or_else(|e| e.into_inner());
        while g.halting {
            g = self.job_fault_cv.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Take the job fault out of its cell. ONE caller, `Vm::rank_end`, which reports what it
    /// takes; nothing else clears the cell, so a fault is never dropped unreported.
    pub(super) fn take_job_fault(&self) -> Option<(super::RuntimeError, Vec<super::TraceFrame>)> {
        self.job_fault
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fault
            .take()
    }

    /// Make `host` the run's root sched; returns the one it replaces, which the caller restores.
    pub(super) fn set_host(
        &self,
        host: std::sync::Weak<super::MnSched>,
    ) -> std::sync::Weak<super::MnSched> {
        std::mem::replace(
            &mut *self.host.lock().unwrap_or_else(|e| e.into_inner()),
            host,
        )
    }

    /// The run's root sched, while its root fiber runs.
    pub(super) fn host(&self) -> Option<Arc<super::MnSched>> {
        self.host
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upgrade()
    }

    /// List an Executor's sched as one whose jobs the host's workers run.
    pub(super) fn add_guest(&self, sched: &Arc<super::MnSched>) {
        let mut g = self.guests.lock().unwrap_or_else(|e| e.into_inner());
        if g.len() >= 64 {
            g.retain(|w| w.strong_count() > 0);
        }
        g.push(Arc::downgrade(sched));
    }

    /// An Executor sched of the run other than `except` with a runnable fiber, if any, scanning
    /// from the one the last scan found (round robin). Every sched it upgrades goes into `keep`,
    /// so a caller under a core lock drops the last handle of none of them there.
    pub(super) fn runnable_guest(
        &self,
        keep: &mut Vec<Arc<super::MnSched>>,
        except: Option<&Arc<super::MnSched>>,
    ) -> Option<Arc<super::MnSched>> {
        let g = self.guests.lock().unwrap_or_else(|e| e.into_inner());
        let n = g.len();
        let start = self.guest_turn.load(Ordering::Relaxed);
        for k in 0..n {
            let i = (start + k) % n;
            let Some(s) = g[i].upgrade() else {
                continue;
            };
            let found = s.runnable.load(Ordering::Relaxed) > 0
                && !except.is_some_and(|e| Arc::ptr_eq(e, &s));
            keep.push(Arc::clone(&s));
            if found {
                self.guest_turn.store(i, Ordering::Relaxed);
                return Some(s);
            }
        }
        None
    }
}

/// TICKET-270 — test-only: a job fault whose message contains this text holds its publisher open.
#[cfg(test)]
const RUN_END_WINDOW_MARK: &str = "TICKET-270 job fault window";
/// TICKET-270 — test-only: an `os.exit` with this code holds its publisher open.
#[cfg(test)]
const RUN_END_WINDOW_EXIT: i32 = 3;

/// TICKET-270 — test-only: the first publisher of a marked run end sleeps 300 ms after the cell
/// is stored and before its slot is stored, so a joiner that reads the halt inside that window is
/// reachable with no host load (`a_job_that_ends_the_run_keeps_its_output_at_every_drain_position`).
#[cfg(test)]
fn hold_run_end_window() {
    std::thread::sleep(std::time::Duration::from_millis(300));
}

/// TICKET-284 — the exit output in flight. `open` counts the parties that hold an exit's output
/// outside a slot the flush reads (a fiber that called `os.exit`, or one that took exit output out
/// of a slot) and have not yet stored their own slot. `begun` counts every count ever begun, so the
/// run-end pass (`Vm::flush_finished_jobs_from`) can tell that output moved while it read.
#[derive(Default)]
struct ExitOrigins {
    open: usize,
    begun: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TICKET-219 — the halt kind: `Running` with no cell, `Fault` with a job fault, and `Exit`
    /// whenever an exit is pending, even beside a job fault (an exit settles nothing).
    #[test]
    fn run_halt_reads_exit_before_job_fault() {
        let q = QuiesceState::default();
        assert_eq!(q.run_halt(), RunHalt::Running);
        q.request_job_fault(super::super::RuntimeError::default(), Vec::new());
        assert_eq!(q.run_halt(), RunHalt::Fault);
        q.request_exit(17);
        assert_eq!(q.run_halt(), RunHalt::Exit);
    }

    // TICKET-223's `decide_latches_once_and_reports_the_party_site` and
    // `decide_without_a_party_site_latches_nothing` are deleted with the party judge (plan
    // 2026-11-17: `main` is fiber 1, no party term; the awake count latches through
    // `latch_deadlock`). `tests/awake_count_grid.rs` (`nothing` cells) and
    // `tests/deadlock_is_fatal.rs` cover the latch and its report.

    /// TICKET-223 — a latched verdict outranks a waiting job fault (it cuts a `defer`, a job fault
    /// does not); an exit still outranks both.
    #[test]
    fn run_halt_ranks_a_verdict_above_a_job_fault() {
        let q = Arc::new(QuiesceState::default());
        assert!(q.latch_deadlock().is_some());
        q.request_job_fault(super::super::RuntimeError::default(), Vec::new());
        assert_eq!(q.run_halt(), RunHalt::Deadlock);
        q.request_exit(17);
        assert_eq!(q.run_halt(), RunHalt::Exit);
    }
}
