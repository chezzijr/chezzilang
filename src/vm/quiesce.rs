//! Process-wide quiescence detection — `docs/future.md` §2d **step 0**, the sound successor to
//! W7-12's per-executor predicate (`gaps.md` `W7-12r`).
//!
//! # The rule
//!
//! This is Go's own detector (`fatal error: all goroutines are asleep - deadlock!`) with ONE
//! adaptation. Go counts: nothing runnable, nothing in a syscall/timer/netpoll ⇒ everything is stuck.
//! Chezzi counts the same way — but because a blocked party here is a POLLING waiter rather than a
//! runtime-scheduled G, a snapshot of "who is parked" is not enough on its own. So the verdict is:
//!
//! > every counted party is registered as blocked **AND** no registered party's wait condition is
//! > already satisfiable.
//!
//! The second clause is what makes it safe, and it is the whole reason W7-12's counter-only attempts
//! failed. A bounded cap-1 pipeline is permanently "all parties parked" while being perfectly healthy
//! — but with `cap == 1` the channel is either non-empty (so the parked RECEIVER is satisfiable) or
//! has a free slot (so the parked SENDER is), and it can never be neither. Two jobs on a genuinely
//! empty channel, by contrast, are both unsatisfiable. Satisfiability separates "parked" from
//! "unfeedable", which no progress counter or debounce window could (see `gaps.md` W7-12's rejected
//! experiment, and the `parked-is-not-stuck` lesson).
//!
//! TICKET-185 — a rendezvous `Channel[T](0)` has no buffer: a blocked sender waits on its OFFER
//! and a blocked receiver on its SLOT, each committed by CAS on the party's own `Pending`, so both
//! sides are judged by that `Pending` and by the OTHER side's entries (`ChanState::recv_ready_for`),
//! never against `cap`.
//!
//! # Counted parties, and why the count is sound
//!
//! `live = 1 (the main thread) + live_eager_bodies()`: one per registered sched that still holds an
//! undone task that can move. Since TICKET-208 a job is a fiber of its Executor's detached sched,
//! so a job that can still send counts through its sched, never as a party of its own. An
//! UNDER-count of `live` is the one error direction that produces a false deadlock.
//!
//! **The load-bearing invariant.** A thread that is not a counted party — an `MnSched` worker, a
//! netpoller callback, a timer callback, a blocking-pool thread — only ever runs user code while some
//! counted party is inside a nursery or a native call. Such a party is live and NOT registered as
//! blocked, so `blocked < live` and the verdict is vetoed. An uncounted sender therefore always
//! implies a veto, which is why no separate "is a scheduler alive?" global is needed. The corollary is
//! `BlockCtx::judged` (`vm/block.rs`): a party registers only when it has no scheduler of any kind and is not
//! inside a native callback other than a `defer` drain.
//!
//! Both error directions are asymmetric and both fall the safe way:
//!
//! | mistake | effect | severity |
//! |---|---|---|
//! | a blocking site forgets to REGISTER | `blocked < live` → veto → hang | recoverable (missing answer) |
//! | a satisfiability check is too generous | veto → hang | recoverable |
//! | `live` under-counts a party that could send | **false deadlock on a live program** | the one unacceptable outcome |
//!
//! # Not a `static`
//!
//! Per-run state, held behind an `Arc` on the `Vm` and shared with every worker by
//! `Vm::spawn_worker`, exactly like [`ExecRegistry`] and for exactly the same reason: `cargo test`
//! runs many programs concurrently in ONE process, and a process-global registry would let one run's
//! blocked parties be counted against another run's.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::block::{Halt, WakeSet};
use super::core::{ChannelCore, Pending};
use crate::ast::Span;

/// What one registered party is waiting for — and, through [`PartyWait::satisfiable`], whether that
/// wait could already be over.
///
/// The channel cores are held by `Arc` rather than by `GcRef`: a party's heap is not reachable from
/// the thread evaluating the verdict, and a core outlives every handle to it.
pub(super) enum PartyWait {
    /// A single blocking `recv` on an empty channel, with the receiver's own `Pending` when it
    /// published a rendezvous slot (TICKET-185): its wait is over once a sender fills that slot.
    Recv(Arc<ChannelCore>, Option<Arc<Pending>>),
    /// A `send` blocked on its published OFFER (TICKET-185 — a full bounded or a rendezvous
    /// channel): its wait is over exactly when the offer's `Pending` settles (taken, or closed).
    Send(Arc<Pending>),
    /// A `wait:` over N arms — an OR-edge (§2d's table): ready on ANY arm. `is_send` marks a SEND
    /// arm, which is ready on free space rather than on a value.
    ///
    /// **Kept separate from [`PartyWait::Recv`] because `closed` means opposite things at the two
    /// sites**, and conflating them was a measured HANG regression: a single `recv` on a closed
    /// channel makes progress (it returns `ClosedEmpty` and the `for` loop ends), but the `wait:`
    /// poll *SKIPS* a closed+empty recv arm (W7-13r(a)) — so treating that arm as satisfiable vetoes
    /// the verdict forever. `c1.close(); wait: c1.recv() / c2.recv()` with nobody able to feed `c2`
    /// faulted `wait on channels that are all empty: deadlock` before this detector and hung with the
    /// two folded into one variant. Fenced by `a_wait_with_a_closed_arm_still_reports_the_deadlock`.
    ///
    /// TICKET-185 — the second field is the `wait:`'s ONE `Pending` (its offers and slots).
    Wait(Vec<(Arc<ChannelCore>, bool)>, Option<Arc<Pending>>),
    /// A thread inside an `Executor` join (`shutdown()` or the program-exit drain), waiting on that
    /// core's `outstanding` to reach 0. This is the node whose absence made W7-12's arms unable to
    /// see `main`-inside-`shutdown()`.
    ///
    /// **It carries the core, and the reason is a measured FALSE FAULT.** A `Join` that answered a
    /// flat "never satisfiable" was wrong for an ALREADY-DRAINED join: `join_executor` registers
    /// before it can take the executor lock, so `Executor(); e.shutdown()` — and the whole window
    /// while the last job's `finish` wakes the joiner — put a permanently-unsatisfiable party in the
    /// registry for a thread that was about to return and keep running. A sibling sampling in that
    /// window faulted a live program (measured 2/20 runs on a loop of drained shutdowns beside a
    /// blocked consumer). Its wait condition is exactly `outstanding() == 0`, so that is what it
    /// answers.
    ///
    /// **…minus the joiner's OWN slot** — the `usize` is 1 when the joining thread is itself an
    /// eager job of this very core, else 0. A job that calls `ex.shutdown()`/`ex.shutdown_now()` on
    /// the executor it is running under stays counted in that core's `outstanding` for the whole
    /// wait, so a flat `outstanding() == 0` made its party self-referentially unsatisfiable: nothing
    /// the run could do would ever satisfy it, which is exactly the shape the verdict reads as
    /// "unfeedable". Measured on a healthy program whose every job ran to completion (`A` and `C`
    /// both printed), `main`'s join faulted `deadlock` in 8/60 debug runs with `shutdown_now` and
    /// 8/8 with `shutdown`. [`super::Vm::join_executor`] computes the identical slack for its own
    /// wait loop — the two must agree, or a joiner would return while its party still claimed to be
    /// stuck.
    Join(Arc<super::MnSched>, usize),
    /// gaps.md W7-58 — a thread blocked inside a `parallel:` nursery join, waiting on that nursery's
    /// tasks to finish. This is the node whose absence hung the W7-58 repro: `live` counts the
    /// top-level `main` thread unconditionally (`1 +`), but a `main` sitting in `mn_worker_loop` never
    /// registered, so `parties.len() < live` vetoed forever whenever the *other* counted party (an
    /// eager `Executor` job) was the one genuinely stuck.
    ///
    /// **A LIVE QUERY, never a snapshot.** Its wait ends exactly when the nursery can move again, so
    /// satisfiability re-asks the nursery's own predicate every time the verdict is evaluated. A
    /// boolean captured at registration is `quiesce.rs`'s build-bug #1 all over again (a stale party
    /// state read against fresh channel state), which measured 6/10 false faults.
    Nursery(Arc<super::MnSched>),
    /// TICKET-063 — a thread blocked waiting for a `Shared`/`RwShared` update guard (the box's core
    /// identity, the waiting task's token). Covers a guard wait reached by the top-level/eager body
    /// itself, e.g. `main` running `s.update(f)` where `f` runs `parallel: spawn: s.update(...)`; the
    /// M:N-worker-side guard wait is registered separately, on `SchedCore::waiters`.
    ///
    /// The `usize` is the box core's `Arc` address — aliasing-safe because the party holding this wait
    /// also holds a live handle to that box (a `GcRef`/task token pinning it) for the whole lifetime of
    /// the wait, so the address cannot be reused underneath it.
    ///
    /// Lock order: P (this registry) then G (the guard registry, `core::guard_wait_satisfiable`'s own
    /// lock) — below `SchedCore` (A) exactly like every other arm here, and G is a leaf so no ABBA.
    Guard(usize, u64),
}

impl PartyWait {
    /// Could this wait already be over? Evaluated by taking each core's queue lock, so the caller must
    /// hold NO channel lock (see [`QuiesceState::quiesced`]).
    ///
    /// Deliberately generous — every "maybe" must answer `true`, because a false `true` only vetoes
    /// the verdict (a hang) while a false `false` faults a live program.
    ///
    /// **Each arm mirrors, condition for condition, what its own blocking site in `netio.rs` actually
    /// SETTLES on** — not what merely "changed". That is the same rule W7-13r(a) had to learn for the
    /// `wait:` wake predicate, and getting it wrong here is not a spin but a permanent wrong answer in
    /// one direction or the other: too generous hangs forever, too strict faults a live program. The
    /// one place the two sites genuinely disagree is `closed` on a recv, hence the separate
    /// [`PartyWait::Wait`] variant.
    pub(super) fn satisfiable(&self) -> bool {
        match self {
            // `Vm::block_recv` settles on a queued value, a `trip()` latch, or `closed` (which
            // returns `ClosedEmpty` — the `for v in ch:` ends, a bare `recv` faults; either is
            // progress).
            PartyWait::Recv(core, me) => {
                let g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                g.recv_ready_for(me.as_ref())
                    || g.closed
                    || core.done_latch.load(Ordering::Relaxed)
                    || me.as_ref().is_some_and(|p| !p.is_queued())
            }
            // The blocking `send` settles once its offer does — taken by a receiver, or closed by
            // `close()` (it faults `CLOSED_SEND`).
            PartyWait::Send(p) => !p.is_queued(),
            // `Vm::op_wait_poll`, arm kind by arm kind: the `wait:` settles once its `Pending` does
            // (an offer taken or closed, a slot filled); a RECV arm is also ready on a value it can
            // take (a buffered value or another party's live offer) or a `trip()` latch — NOT on
            // `closed`, which the poll SKIPS. A timer arm delivers on its own deadline with nobody
            // sending, so it is never judged.
            //
            // DEC-176 — the poll skips ONE closed recv arm, but a `wait:` whose EVERY arm is a closed
            // recv arm settles (`wait: all channels closed`), so the group is judged as a whole.
            PartyWait::Wait(arms, me) => {
                me.as_ref().is_some_and(|p| !p.is_queued())
                    || arms.iter().any(|(core, is_send)| {
                        let g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                        if *is_send {
                            me.is_none() && (g.send_ready_for(core.cap, None) || g.closed)
                        } else {
                            g.recv_ready_for(me.as_ref())
                                || core.done_latch.load(Ordering::Relaxed)
                                || core.timer.is_some()
                        }
                    })
                    || (!arms.is_empty()
                        && arms.iter().all(|(core, is_send)| {
                            !is_send && core.q.lock().unwrap_or_else(|e| e.into_inner()).closed
                        }))
            }
            // A join is over exactly when the executor owes nothing BUT this joiner's own job. See
            // the variant's doc: answering a flat `false` here faulted an already-drained
            // `shutdown()`, and ignoring `slack` faulted a job that shut down its own executor.
            PartyWait::Join(sched, slack) => sched.lock().undone_tasks() <= *slack,
            // W7-58 — a nursery join is over exactly when the nursery can still move: the sched's OWN
            // deadlock predicate, minus its W7-56 outstanding-job veto.
            //
            // **Not circular.** `local_quiesced` reads only `SchedCore` + this sched's own
            // atomics — never `parties`, never `outstanding` — so the process-wide verdict never
            // appears on its own right-hand side. TICKET-099 — this calls `local_quiesced` on purpose,
            // NOT the sibling predicate that also carries a peer-sched veto: this arm feeds the
            // process-wide verdict, which already does its own cross-sched accounting. A second veto
            // here would over-count `live` and hang a genuinely deadlocked run (measured on three
            // `*_still_fault` tests — see `live_eager_bodies` below). Dropping the W7-56 veto
            // HERE (the reason for the `local_` half in the first place) is sound precisely because the
            // job is then visible as its own party (an unregistered job is a live one, and
            // `parties.len() < live` already vetoes).
            //
            // Lock order: `parties` (P) → `SchedCore` (A) → `ChannelCore::q` (Q) — the predicate's
            // last gate peeks Q for every demoted fiber, which is the order `send_wake` already uses.
            // The judge in `MnSched::take_runnable` DROPS its core guard before calling `quiesced`,
            // and no other site takes `parties` while holding a core lock.
            PartyWait::Nursery(sched) => {
                let c = sched.lock();
                // TICKET-125 — a sched whose every counted fiber is an owner blocked at a nested
                // join has no parked victim to demand either (same DEC-112 bullet-3 relaxation as
                // `live_eager_bodies` above).
                !sched.quiesced_core(&c, !c.only_blocked_owners())
            }
            // TICKET-063 — mirrors `SchedCore::waiters`' veto in `local_quiesced`.
            PartyWait::Guard(key, me) => super::core::guard_wait_satisfiable(*key, *me),
        }
    }
}

/// TICKET-188 — one registered blocked party: what it waits for, and the wake set that may cut it.
struct Party {
    wait: Arc<PartyWait>,
    wake: WakeSet,
    /// TICKET-223 — the deadlock report this party gives when the verdict names it: its own text
    /// and blocking site. `None` for a Join or Nursery party, whose report is the victims' slots
    /// (DEC-208).
    site: Option<(&'static str, Span)>,
}

/// TICKET-223 — the latched deadlock verdict ([`QuiesceState::decide`]).
#[derive(Default)]
struct DeadlockCell {
    decided: bool,
    report: Option<(&'static str, Span)>,
}

/// TICKET-223 — which judge asks [`QuiesceState::decide`]: a party at its own poll, or a sched
/// worker with nothing runnable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Judge {
    Party,
    Sched,
}

/// TICKET-223 — names the judge allowed to decide first (`party` or `sched`), so a test can force
/// either order. Compiled only under `debug_assertions`: the release binary reads no test env.
#[cfg(debug_assertions)]
pub(crate) const VERDICT_JUDGE_ENV: &str = "CHEZZI_TEST_VERDICT_JUDGE";

/// TICKET-223 — `true` when [`VERDICT_JUDGE_ENV`] pins the OTHER judge, so `judge` must not decide
/// first. A `party` pin holds back the sched judge only while a party with a site is registered:
/// otherwise no party judge would ever decide. Always `false` in a release build.
fn pinned_away(judge: Judge, has_site: bool) -> bool {
    #[cfg(debug_assertions)]
    {
        static PIN: std::sync::OnceLock<Option<Judge>> = std::sync::OnceLock::new();
        let pin = *PIN.get_or_init(|| match std::env::var(VERDICT_JUDGE_ENV).as_deref() {
            Ok("party") => Some(Judge::Party),
            Ok("sched") => Some(Judge::Sched),
            _ => None,
        });
        match (pin, judge) {
            (Some(Judge::Sched), Judge::Party) => true,
            (Some(Judge::Party), Judge::Sched) => has_site,
            _ => false,
        }
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = (judge, has_site);
        false
    }
}

/// The run's registry of blocked parties. One per `Vm::new`, shared by `Arc` with every worker.
#[derive(Default)]
pub(super) struct QuiesceState {
    parties: Mutex<Vec<Party>>,
    /// §2c1 — every eager nursery alive in this run, by `Weak` (like [`super::SchedRegistry`]) —
    /// every eager nursery (nested ones since TICKET-112, which is sound only with
    /// `MnSched::body_is_fiber`).
    ///
    /// It exists because eager start broke the invariant `live`'s soundness rests on — *a nursery
    /// fiber never coexists with a counted party*. It can now: top-level `main` runs the `parallel:`
    /// body itself, so `main` blocked on `ch.recv()` while a live sibling is about to `send` registers
    /// an (correctly) unsatisfiable `Recv` party while `live == 1` counts no nursery fibers at all —
    /// `parties.len() >= live` with nothing satisfiable, i.e. a **false deadlock on a live program**,
    /// the one unacceptable direction ([`Self::verdict`]'s table above).
    ///
    /// [`Self::live_eager_bodies`] adds one to `live` per nursery that still has an undone task, which
    /// is the *safe* direction (an over-count only vetoes). It is deliberately NOT a `PartyWait`: a
    /// party is a BLOCKED THREAD, and registering a non-thread inflates `parties.len()` toward `live`
    /// — measured, that false-faulted `spawn: print(…)` beside `time.sleep_ms(300)` on `main`, a
    /// program with no channel in it at all.
    ///
    /// Pruned lazily on read; a nursery that has JOINED contributes nothing anyway (its scope is
    /// complete), so nothing has to deregister.
    eager_bodies: Mutex<Vec<std::sync::Weak<super::MnSched>>>,
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
    /// gaps.md W7-57 — the lock-free mirror of `exit.is_some()`, for the two CPU-side checkpoints that
    /// cannot afford a mutex: `jump_checked`'s loop back-edge (sampled 1/1024) and `guarded`'s
    /// per-element native-HOF re-entry. A party that is spinning in a loop or grinding through a
    /// `map`/`fold` reaches no blocking wait at all, so the `Mutex<Option>` above — read only by the
    /// demote/poll loops — never reaches it and the run hangs forever, or finishes work Go would not.
    ///
    /// Stored **`Release` AFTER** the code and after [`super::Vm::halt_all_scheds`], and loaded
    /// `Acquire` — which publishes the code and the scope-cancel stores to whoever reads `true`, and
    /// nothing more. **It does NOT order the two rungs against each other**: an acquire orders only
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
    /// TICKET-208 — the fault of a fire-and-forget `Executor` job: the second cause of a run-wide
    /// halt, beside `exit`. Read through the one funnel `Vm::run_exit_err`, taken by `Vm::rank_end`.
    job_fault: Mutex<Option<(super::RuntimeError, Vec<super::TraceFrame>)>>,
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
    /// TICKET-211 — runner starts refused by the `NestedDrainerSlot` budget. Test-only.
    #[cfg(test)]
    pub(super) runner_slot_denials: std::sync::atomic::AtomicUsize,
    /// TICKET-211 — OS threads started for this run's non-Executor runner leases. Test-only.
    #[cfg(test)]
    pub(super) runner_spawns: std::sync::atomic::AtomicUsize,
}

/// TICKET-219 — the kind of run-wide halt in force ([`QuiesceState::run_halt`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum RunHalt {
    Running,
    /// A fire-and-forget job fault: every `defer` still runs whole.
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
    /// `Vm::run_halt_due` and `Vm::next_deferred` read it.
    pub(super) fn cuts_cleanup(self) -> bool {
        matches!(self, RunHalt::Exit | RunHalt::Deadlock)
    }
}

impl QuiesceState {
    /// Publish an `os.exit`. First writer wins, exactly like Go: whichever `os.Exit` runs first sets
    /// the status, and a later one cannot rewrite it.
    pub(super) fn request_exit(&self, code: i32) {
        let mut g = self.exit.lock().unwrap_or_else(|e| e.into_inner());
        g.get_or_insert(code);
    }

    /// The pending run-wide exit code, if any.
    pub(super) fn pending(&self) -> Option<i32> {
        *self.exit.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// W7-57 — latch the back-edge flag. Called by the host `request_exit` AFTER the code is published
    /// and after every live sched has been halted; see the field's doc for why the order is required.
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
        *self.deadlock.lock().unwrap_or_else(|e| e.into_inner()) = DeadlockCell::default();
        // The hint covers a pending job fault too, and this reset never clears that.
        self.run_halt_hint
            .store(self.has_job_fault(), Ordering::Release);
    }

    /// TICKET-208 — publish a fire-and-forget `Executor` job's fault: it ends the whole run. First
    /// writer wins (Go: the first panic is the report). The caller halts every sched and then
    /// latches the hint ([`Self::mark_run_halt`]), the order `request_exit` uses.
    pub(super) fn request_job_fault(
        &self,
        err: super::RuntimeError,
        trace: Vec<super::TraceFrame>,
    ) {
        let mut g = self.job_fault.lock().unwrap_or_else(|e| e.into_inner());
        g.get_or_insert((err, trace));
    }

    /// The pending job fault, if any.
    pub(super) fn job_fault(&self) -> Option<(super::RuntimeError, Vec<super::TraceFrame>)> {
        self.job_fault
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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
        } else if self.has_job_fault() {
            RunHalt::Fault
        } else {
            RunHalt::Running
        }
    }

    pub(super) fn has_job_fault(&self) -> bool {
        self.job_fault
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Take the job fault out of its cell. ONE caller, `Vm::rank_end`, which reports what it
    /// takes; nothing else clears the cell, so a fault is never dropped unreported.
    pub(super) fn take_job_fault(&self) -> Option<(super::RuntimeError, Vec<super::TraceFrame>)> {
        self.job_fault
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// TICKET-223 — the latched report's text and site, while a verdict is decided.
    pub(super) fn deadlock_report(&self) -> Option<(&'static str, Span)> {
        let g = self.deadlock.lock().unwrap_or_else(|e| e.into_inner());
        if g.decided { g.report } else { None }
    }

    /// TICKET-223 — take the latched report; the verdict stays decided. ONE caller, `Vm::rank_end`.
    pub(super) fn take_deadlock_report(&self) -> Option<(&'static str, Span)> {
        self.deadlock
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .report
            .take()
    }

    /// Register a blocked party for as long as the returned guard lives. It has no deadlock site.
    pub(super) fn block(self: &Arc<Self>, wait: PartyWait, wake: WakeSet) -> PartyGuard {
        self.block_shared(Arc::new(wait), wake, None)
    }

    /// §2c1 — [`Self::block`] over an `Arc` the caller already holds, so ONE `PartyWait` can be both
    /// the registered party AND the sched-side `SchedCore::waiters` entry. Two separately-built
    /// waits for the same block could disagree about what the thread waits for; one cannot.
    /// `site` is the party's deadlock report (TICKET-223).
    pub(super) fn block_shared(
        self: &Arc<Self>,
        wait: Arc<PartyWait>,
        wake: WakeSet,
        site: Option<(&'static str, Span)>,
    ) -> PartyGuard {
        self.parties
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Party {
                wait: Arc::clone(&wait),
                wake,
                site,
            });
        PartyGuard {
            state: Arc::clone(self),
            wait,
        }
    }

    /// The verdict: is the whole run stuck?
    ///
    /// **The party lock is held across the WHOLE verdict, and that is a correctness requirement, not
    /// a convenience.** An earlier revision snapshotted the list and released the lock before reading
    /// any channel — and that races: a party can register, be fed, un-register and run on while the
    /// stale snapshot still names it, so the verdict then reads channel states against a party set
    /// that never existed at any single instant. Measured on the 300-handoff gate/data pipeline
    /// (`an_eager_wait_block_is_woken_by_its_arm_not_by_the_poll_timeout`): the producer parked on
    /// `gate` and the consumer parked on `data` were reported together with both channels empty — a
    /// state that is unreachable in that program, because whichever party parked second must have fed
    /// the other first. Holding the lock makes the party set and the channel reads ONE observation,
    /// and the false positive is gone.
    ///
    /// **Lock discipline.** Order is `parties` (P) → `SchedCore` (A) → (`exec_registry` → one
    /// `ExecutorCore::eager`) → `ChannelCore::q`, one at a time. Nothing anywhere acquires `parties`
    /// while holding a channel, executor OR sched-core lock: every blocking site in `netio.rs`
    /// registers BEFORE it locks the queue it then waits on, `join_executor` registers before it
    /// takes `eager`, W7-58's nursery owner registers with no core lock held, and the W7-58 judge
    /// inside `MnSched::take_runnable` DROPS its `SchedCore` guard before calling this. So this adds no
    /// cycle. `parties` is globally exclusive, so at most one thread is ever inside this function and
    /// it takes each `SchedCore` singly — there is no A→A' edge either. (Same rule the deleted
    /// `eager_join_deadlocked` documented; it is tightened here, not relaxed.)
    ///
    /// TICKET-223 — test-only: both production judges call [`Self::decide`], which evaluates the
    /// same predicate under the same lock discipline and latches it.
    #[cfg(test)]
    pub(super) fn quiesced(&self) -> bool {
        self.verdict()
    }

    /// TICKET-223 — the ONE place a judge acts on the verdict. The first judge that sees it latches
    /// it, with the report taken from the first registered party that has a site, and publishes it
    /// as a run halt; a later call answers `true` once latched. A verdict that names no party site
    /// (main at a join) latches nothing: its report is the victims' slots (DEC-208), and the run
    /// may go on (DEC-147). The latch is taken under the party lock, so
    /// it is set before any judge cuts a victim. [`Self::quiesced`] stays the pure predicate.
    ///
    /// Lock order: `parties` (P), then the verdict's own walk, then the `deadlock` cell alone. The
    /// cell is never held across the walk, because `run_halt` reads it under a `SchedCore` lock.
    pub(super) fn decide(&self, judge: Judge) -> bool {
        let parties = self.parties.lock().unwrap_or_else(|e| e.into_inner());
        if self
            .deadlock
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .decided
        {
            return true;
        }
        let site = parties.iter().find_map(|p| p.site);
        if !self.verdict_on(&parties) || pinned_away(judge, site.is_some()) {
            return false;
        }
        let mut cell = self.deadlock.lock().unwrap_or_else(|e| e.into_inner());
        cell.decided = site.is_some();
        cell.report = site;
        if site.is_some() {
            self.run_halt_hint.store(true, Ordering::Release);
        }
        true
    }

    /// One evaluation under ONE hold of the party lock (see [`Self::quiesced`]).
    #[cfg(test)]
    fn verdict(&self) -> bool {
        let parties = self.parties.lock().unwrap_or_else(|e| e.into_inner());
        self.verdict_on(&parties)
    }

    /// The verdict over `parties`, which the caller holds locked.
    fn verdict_on(&self, parties: &[Party]) -> bool {
        // `1 +` is the main thread, which is a party for the whole run. Plus one per registered
        // sched (an eager nursery, nested ones since TICKET-112, or an `Executor`'s detached
        // sched since TICKET-208) that still holds an undone task that can move: those fibers are
        // uncounted senders, and this is the term that stops a healthy `spawn: ch.send(1)` beside
        // a blocking `ch.recv()` on `main` from reading as a deadlock. See `eager_bodies`.
        let live = 1 + self.live_eager_bodies();
        if parties.len() < live {
            return false; // somebody is still running — they may yet send.
        }
        // TICKET-188 — a party whose wake set holds a recorded child fault resumes at its next halt
        // read, so its cut is progress; `WakeSet::halt` takes owned scheds' core locks (A), legal
        // under P. A tripped flag with no recorded fault is not a halt, so a genuine deadlock still
        // faults. The veto reads only `Halt::ChildFault`: a pending cancel is not a promise of
        // progress (a cancelled owner at its join waits on a child in an uncancellable `defer`).
        !parties
            .iter()
            .any(|p| p.wait.satisfiable() || matches!(p.wake.halt(), Some(Halt::ChildFault { .. })))
    }

    /// §2c1 — publish an eager nursery's sched (every eager nursery since TICKET-112), so
    /// [`Self::live_eager_bodies`] can count its
    /// fibers as uncounted senders for as long as they are undone. Takes only this lock.
    pub(super) fn register_eager_body(&self, sched: &Arc<super::MnSched>) {
        let mut g = self.eager_bodies.lock().unwrap_or_else(|e| e.into_inner());
        // A `parallel:` inside a loop registers once per iteration, and a run that never blocks never
        // calls `live_eager_bodies` to prune — so compact here too, or 20 000 iterations leave 20 000
        // dead `Weak`s behind. Amortised: the scan runs only when the vec has actually grown.
        if g.len() >= 64 {
            g.retain(|w| w.strong_count() > 0);
        }
        g.push(Arc::downgrade(sched));
    }

    /// §2c1 — how many live eager nurseries still hold an undone task. ONE per nursery, not one per
    /// task: a single un-blocked sender is all it takes to veto the verdict, and `live` only has to
    /// EXCEED `parties.len()`.
    ///
    /// Snapshots the registry and DROPS its lock before taking any `SchedCore` — the same walk
    /// `Vm::halt_all_scheds` and `outstanding_jobs` use, so the established `parties` (P) →
    /// `SchedCore` (A) order is unchanged and this lock never nests under one.
    ///
    /// A nursery that has JOINED reports every scope complete, so it contributes nothing and needs no
    /// deregistration; dead `Weak`s are pruned here.
    fn live_eager_bodies(&self) -> usize {
        let live: Vec<_> = {
            let mut g = self.eager_bodies.lock().unwrap_or_else(|e| e.into_inner());
            if g.is_empty() {
                return 0;
            }
            let live: Vec<_> = g.iter().filter_map(|w| w.upgrade()).collect();
            g.retain(|w| w.strong_count() > 0);
            live
        };
        // "Can this nursery still send?" — it must have UNDONE work AND be able to move. Counting
        // merely-incomplete nurseries over-counts `live` forever once their fibers are stuck, which
        // vetoes the verdict and HANGS a genuinely deadlocked run (measured on three `*_still_fault`
        // tests). The blocked-body-aware variant is the right question HERE and only here.
        //
        // TICKET-099 — `local_quiesced` on purpose, not the sibling predicate that also carries a
        // peer-sched veto: this fn feeds the process-wide verdict, so it must not see that veto
        // either. A vetoed body would count a quiesced-but-peer-vetoed nursery as `live`, inflate
        // `live` past the party count, and reproduce exactly the hang the paragraph above measured.
        // TICKET-112 — a NESTED sched (`body_is_fiber`) is judged with `require_parked = false`, the
        // peer question. When every undone fiber is parked or blocked at a deeper join, it can send
        // nothing until another registered sched moves, and that sched counts on its own. With
        // `local_quiesced` here such a sched counted live forever and hung `exec_nested` at
        // `CHEZZI_THREADS>=2`. Outermost scheds keep `local_quiesced` (DEC-101), EXCEPT when every
        // counted fiber is an owner blocked at a nested join (`only_blocked_owners`, TICKET-125,
        // DEC-112 bullet 3) — such a sched has no parked victim of its own to demand and hung
        // `exec_join`/`b4a`/`e10`/`e11` at T>=2 the same way.
        live.iter()
            .filter(|s| {
                let c = s.lock();
                c.any_scope_incomplete()
                    && !s.quiesced_core(&c, !(s.body_is_fiber || c.only_blocked_owners()))
            })
            .count()
    }
}

/// RAII registration of one blocked party. Dropped the moment the block ends — successfully or with a
/// fault — so a party is never counted as stuck while it is running.
pub(super) struct PartyGuard {
    state: Arc<QuiesceState>,
    wait: Arc<PartyWait>,
}

impl Drop for PartyGuard {
    fn drop(&mut self) {
        let mut g = self.state.parties.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = g.iter().position(|p| Arc::ptr_eq(&p.wait, &self.wait)) {
            g.swap_remove(i);
        }
    }
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

    /// TICKET-223 — the first judge latches the verdict with the registered party's site; a later
    /// judge reads the latch even after the pure predicate has gone false, and `clear_exit` resets it.
    #[test]
    fn decide_latches_once_and_reports_the_party_site() {
        let q = Arc::new(QuiesceState::default());
        let g = q.block_shared(
            Arc::new(PartyWait::Send(Pending::new())),
            WakeSet::default(),
            Some(("x", crate::ast::Span::default())),
        );
        assert!(q.decide(Judge::Sched));
        drop(g);
        assert!(!q.quiesced());
        assert!(q.decide(Judge::Party));
        assert_eq!(q.run_halt(), RunHalt::Deadlock);
        assert!(q.take_deadlock_report().is_some());
        q.clear_exit();
        assert_eq!(q.run_halt(), RunHalt::Running);
    }

    /// TICKET-223 — a verdict that names no party site (main at a join) is answered but not
    /// latched: its report is the victims' slots (DEC-208), and a later call re-evaluates it.
    #[test]
    fn decide_without_a_party_site_latches_nothing() {
        let q = Arc::new(QuiesceState::default());
        let g = q.block(PartyWait::Send(Pending::new()), WakeSet::default());
        assert!(q.decide(Judge::Sched));
        assert_eq!(q.run_halt(), RunHalt::Running);
        drop(g);
        assert!(!q.decide(Judge::Sched));
    }

    /// TICKET-223 — a latched verdict outranks a waiting job fault (it cuts a `defer`, a job fault
    /// does not); an exit still outranks both.
    #[test]
    fn run_halt_ranks_a_verdict_above_a_job_fault() {
        let q = Arc::new(QuiesceState::default());
        let _g = q.block_shared(
            Arc::new(PartyWait::Send(Pending::new())),
            WakeSet::default(),
            Some(("x", crate::ast::Span::default())),
        );
        assert!(q.decide(Judge::Party));
        q.request_job_fault(super::super::RuntimeError::default(), Vec::new());
        assert_eq!(q.run_halt(), RunHalt::Deadlock);
        q.request_exit(17);
        assert_eq!(q.run_halt(), RunHalt::Exit);
    }
}
