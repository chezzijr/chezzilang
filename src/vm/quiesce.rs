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
//! # Counted parties, and why the count is sound
//!
//! `live = 1 (the main thread) + Σ ExecutorCore::outstanding` over the run's [`ExecRegistry`].
//! `outstanding` is bumped at `reserve()` — at `submit`, BEFORE the job is dispatched — and dropped at
//! `finish()`, so a job still queued behind a saturated pool already counts as live. No new counter is
//! introduced, deliberately: an UNDER-count of `live` is the one error direction that produces a false
//! deadlock, and `outstanding` is maintained by the code that owns job lifetime.
//!
//! **The load-bearing invariant.** A thread that is not a counted party — an `MnSched` worker, a
//! netpoller callback, a timer callback, a blocking-pool thread — only ever runs user code while some
//! counted party is inside a nursery or a native call. Such a party is live and NOT registered as
//! blocked, so `blocked < live` and the verdict is vetoed. An uncounted sender therefore always
//! implies a veto, which is why no separate "is a scheduler alive?" global is needed. The corollary is
//! `Vm::is_counted_party`: a party registers only when it has no scheduler of any kind and is not
//! inside a native callback.
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

use std::sync::{Arc, Mutex};

use super::core::{ChannelCore, ExecRegistry};

/// What one registered party is waiting for — and, through [`PartyWait::satisfiable`], whether that
/// wait could already be over.
///
/// The channel cores are held by `Arc` rather than by `GcRef`: a party's heap is not reachable from
/// the thread evaluating the verdict, and a core outlives every handle to it.
pub(super) enum PartyWait {
    /// A single blocking `recv` on an empty channel.
    Recv(Arc<ChannelCore>),
    /// A `send` blocked on a full bounded channel.
    Send(Arc<ChannelCore>),
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
    Wait(Vec<(Arc<ChannelCore>, bool)>),
    /// A thread inside an `Executor` join (`shutdown()` or the program-exit drain), waiting on that
    /// core's `outstanding` to reach 0. This is the node whose absence made W7-12's arms unable to
    /// see `main`-inside-`shutdown()`.
    ///
    /// **It carries the core, and the reason is a measured FALSE FAULT.** A `Join` that answered a
    /// flat "never satisfiable" was wrong for an ALREADY-DRAINED join: `join_eager_jobs` registers
    /// before it can take the executor lock, so `Executor(); e.shutdown()` — and the whole window
    /// while the last job's `finish` wakes the joiner — put a permanently-unsatisfiable party in the
    /// registry for a thread that was about to return and keep running. A sibling sampling in that
    /// window faulted a live program (measured 2/20 runs on a loop of drained shutdowns beside a
    /// blocked consumer). Its wait condition is exactly `outstanding() == 0`, so that is what it
    /// answers.
    Join(Arc<super::core::ExecutorCore>),
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
    fn satisfiable(&self) -> bool {
        match self {
            // `Vm::block_recv` settles on a queued value, a `trip()` latch, or `closed` (which
            // returns `ClosedEmpty` — the `for v in ch:` ends, a bare `recv` faults; either is
            // progress).
            PartyWait::Recv(core) => {
                let g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                !g.is_empty()
                    || g.closed
                    || core.done_latch.load(std::sync::atomic::Ordering::Relaxed)
            }
            // The blocking `send` loop settles on free space, or on `closed` (it faults `CLOSED_SEND`
            // — W7-13r(c)).
            PartyWait::Send(core) => {
                let g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                core.cap.is_none_or(|c| g.len() < c) || g.closed
            }
            // `Vm::op_wait_poll`, arm kind by arm kind: a RECV arm is ready on a queued value or a
            // `trip()` latch — NOT on `closed`, which the poll SKIPS — while a SEND arm is ready on
            // free space or on `closed` (the poll faults `CLOSED_SEND` there). A timer arm delivers on
            // its own deadline with nobody sending, so it is never judged.
            PartyWait::Wait(arms) => arms.iter().any(|(core, is_send)| {
                let g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                if *is_send {
                    core.cap.is_none_or(|c| g.len() < c) || g.closed
                } else {
                    !g.is_empty()
                        || core.done_latch.load(std::sync::atomic::Ordering::Relaxed)
                        || core.timer.is_some()
                }
            }),
            // A join is over exactly when the executor owes nothing. See the variant's doc: answering
            // a flat `false` here faulted an already-drained `shutdown()`.
            PartyWait::Join(core) => {
                core.eager
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .outstanding()
                    == 0
            }
        }
    }
}

/// The run's registry of blocked parties. One per `Vm::new`, shared by `Arc` with every worker.
#[derive(Default)]
pub(super) struct QuiesceState {
    parties: Mutex<Vec<Arc<PartyWait>>>,
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

    /// Drop a latched exit — the per-test reset (`Vm::invoke_test`); see the field's doc.
    pub(super) fn clear_exit(&self) {
        *self.exit.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Register a blocked party for as long as the returned guard lives.
    pub(super) fn block(self: &Arc<Self>, wait: PartyWait) -> PartyGuard {
        let wait = Arc::new(wait);
        self.parties
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::clone(&wait));
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
    /// **Lock discipline.** Order is `parties` → (`exec_registry` → one `ExecutorCore::eager`) →
    /// `ChannelCore::q`, one at a time. Nothing anywhere acquires `parties` while holding a channel or
    /// executor lock: every blocking site in `netio.rs` registers BEFORE it locks the queue it then
    /// waits on, and `join_eager_jobs` registers before it takes `eager`. So this adds no cycle. (Same
    /// rule the deleted `eager_join_deadlocked` documented; it is tightened here, not relaxed.)
    pub(super) fn quiesced(&self, exec_registry: &ExecRegistry) -> bool {
        let parties = self.parties.lock().unwrap_or_else(|e| e.into_inner());
        // `1 +` is the main thread, which is a party for the whole run and is the only one not owned
        // by an executor slot. Read under the party lock so a `submit` cannot slip a new job past a
        // count already taken (it could only be issued by a RUNNING party, which is unregistered and
        // therefore already vetoes — but the read is free here and the invariant is worth pinning).
        let live = 1 + Self::outstanding_jobs(exec_registry);
        if parties.len() < live {
            return false; // somebody is still running — they may yet send.
        }
        !parties.iter().any(|p| p.satisfiable())
    }

    /// Σ `outstanding` over every executor created in this run. Snapshots the registry and drops its
    /// lock before taking any per-core lock (see [`Self::quiesced`]'s lock discipline).
    ///
    /// Also the nursery predicate's W7-56 veto ([`super::MnSched::is_deadlocked`]) — the same count,
    /// for the same reason: an outstanding job is an UNCOUNTED sender.
    pub(super) fn outstanding_jobs(exec_registry: &ExecRegistry) -> usize {
        let cores: Vec<_> = exec_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        cores
            .iter()
            .map(|c| {
                c.eager
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .outstanding()
            })
            .sum()
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
        if let Some(i) = g.iter().position(|p| Arc::ptr_eq(p, &self.wait)) {
            g.swap_remove(i);
        }
    }
}
