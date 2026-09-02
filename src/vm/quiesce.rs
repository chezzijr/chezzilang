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
//! TICKET-028 — a rendezvous `Channel[T](0)` is simultaneously empty AND full (`queue.len() < cap`
//! is `0 < 0`, always false), so its send side is judged against `ChanState::recv_waiting`, never
//! against `cap` — see [`ChanState::has_send_slot`](super::core::ChanState::has_send_slot).
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

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
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
    /// A `send` blocked on a full bounded channel. The second field is `Some(handle)` for a
    /// block-in-place rendezvous sender that has DEPOSITED its value (TICKET-042a) — its wait is
    /// also over once that deposit is taken/withdrawn, even with no free slot (a cap-0 channel's
    /// `has_send_slot` does not track a deposit already claimed).
    Send(Arc<ChannelCore>, Option<Arc<AtomicU8>>),
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
    ///
    /// **…minus the joiner's OWN slot** — the `usize` is 1 when the joining thread is itself an
    /// eager job of this very core, else 0. A job that calls `ex.shutdown()`/`ex.shutdown_now()` on
    /// the executor it is running under stays counted in that core's `outstanding` for the whole
    /// wait, so a flat `outstanding() == 0` made its party self-referentially unsatisfiable: nothing
    /// the run could do would ever satisfy it, which is exactly the shape the verdict reads as
    /// "unfeedable". Measured on a healthy program whose every job ran to completion (`A` and `C`
    /// both printed), `main`'s join faulted `deadlock` in 8/60 debug runs with `shutdown_now` and
    /// 8/8 with `shutdown`. [`super::Vm::join_eager_jobs`] computes the identical slack for its own
    /// wait loop — the two must agree, or a joiner would return while its party still claimed to be
    /// stuck.
    Join(Arc<super::core::ExecutorCore>, usize),
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
            PartyWait::Recv(core) => {
                let g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                !g.is_empty()
                    || g.closed
                    || core.done_latch.load(std::sync::atomic::Ordering::Relaxed)
            }
            // The blocking `send` loop settles on free space, or on `closed` (it faults `CLOSED_SEND`
            // — W7-13r(c)).
            PartyWait::Send(core, deposit) => {
                let g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                deposit
                    .as_ref()
                    .is_some_and(|d| d.load(Ordering::Relaxed) != super::core::DEPOSIT_QUEUED)
                    || g.has_send_slot(core.cap)
                    || g.closed
            }
            // `Vm::op_wait_poll`, arm kind by arm kind: a RECV arm is ready on a queued value or a
            // `trip()` latch — NOT on `closed`, which the poll SKIPS — while a SEND arm is ready on
            // free space or on `closed` (the poll faults `CLOSED_SEND` there). A timer arm delivers on
            // its own deadline with nobody sending, so it is never judged.
            PartyWait::Wait(arms) => arms.iter().any(|(core, is_send)| {
                let g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                if *is_send {
                    g.has_send_slot(core.cap) || g.closed
                } else {
                    !g.is_empty()
                        || core.done_latch.load(std::sync::atomic::Ordering::Relaxed)
                        || core.timer.is_some()
                }
            }),
            // A join is over exactly when the executor owes nothing BUT this joiner's own job. See
            // the variant's doc: answering a flat `false` here faulted an already-drained
            // `shutdown()`, and ignoring `slack` faulted a job that shut down its own executor.
            PartyWait::Join(core, slack) => {
                core.eager
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .outstanding()
                    <= *slack
            }
            // W7-58 — a nursery join is over exactly when the nursery can still move: the sched's OWN
            // deadlock predicate, minus its W7-56 outstanding-job veto.
            //
            // **Not circular.** `is_deadlocked_ignoring_jobs` reads only `SchedCore` + this sched's own
            // atomics — never `parties`, never `outstanding` — so the process-wide verdict never
            // appears on its own right-hand side. The `_ignoring_jobs` half is the load-bearing part:
            // the full `is_deadlocked` vetoes on `outstanding > 0`, and the whole point of this arm is
            // the case where the outstanding job is itself a registered, stuck party. Dropping the
            // veto HERE is sound precisely because the job is then visible as its own party (an
            // unregistered job is a live one, and `parties.len() < live` already vetoes).
            //
            // Lock order: `parties` (P) → `SchedCore` (A) → `ChannelCore::q` (Q) — the predicate's
            // last gate peeks Q for every demoted fiber, which is the order `send_wake` already uses.
            // The judge in `MnSched::take_runnable` DROPS its core guard before calling `quiesced`,
            // and no other site takes `parties` while holding a core lock.
            PartyWait::Nursery(sched) => {
                let c = sched.lock();
                !sched.is_deadlocked_ignoring_jobs(&c)
            }
        }
    }
}

/// The run's registry of blocked parties. One per `Vm::new`, shared by `Arc` with every worker.
#[derive(Default)]
pub(super) struct QuiesceState {
    parties: Mutex<Vec<Arc<PartyWait>>>,
    /// §2c1 — every OUTERMOST eager nursery alive in this run, by `Weak` (like [`super::SchedRegistry`]).
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
    exit_pending: AtomicBool,
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
    pub(super) fn mark_exit_pending(&self) {
        self.exit_pending.store(true, Ordering::Release);
    }

    /// W7-57 — the lock-free "is there a run-wide exit?" the CPU-side checkpoints ask, so they need no
    /// mutex on the hot path. A `true` is only a HINT: `Vm::exit_halt` and `Vm::run_exit_err` both
    /// confirm `pending()` before acting, so a spurious `true` is a self-healing no-op.
    pub(super) fn exit_pending(&self) -> bool {
        self.exit_pending.load(Ordering::Acquire)
    }

    /// Drop a latched exit — the per-invocation reset (`Vm::reset_for_invoke`, shared by every
    /// `chezzi test` entry point); see the field's doc. The `exit` cell is the load-bearing half; the
    /// atomic is cleared with it to keep the mirror honest and to save the confirming lock.
    pub(super) fn clear_exit(&self) {
        *self.exit.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.exit_pending.store(false, Ordering::Release);
    }

    /// Register a blocked party for as long as the returned guard lives.
    pub(super) fn block(self: &Arc<Self>, wait: PartyWait) -> PartyGuard {
        self.block_shared(Arc::new(wait))
    }

    /// §2c1 — [`Self::block`] over an `Arc` the caller already holds, so ONE `PartyWait` can be both
    /// the registered party AND the sched-side `SchedCore::body_waits` entry. Two separately-built
    /// waits for the same block could disagree about what the thread waits for; one cannot.
    pub(super) fn block_shared(self: &Arc<Self>, wait: Arc<PartyWait>) -> PartyGuard {
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
    /// **Lock discipline.** Order is `parties` (P) → `SchedCore` (A) → (`exec_registry` → one
    /// `ExecutorCore::eager`) → `ChannelCore::q`, one at a time. Nothing anywhere acquires `parties`
    /// while holding a channel, executor OR sched-core lock: every blocking site in `netio.rs`
    /// registers BEFORE it locks the queue it then waits on, `join_eager_jobs` registers before it
    /// takes `eager`, W7-58's nursery owner registers with no core lock held, and the W7-58 judge
    /// inside `MnSched::take_runnable` DROPS its `SchedCore` guard before calling this. So this adds no
    /// cycle. `parties` is globally exclusive, so at most one thread is ever inside this function and
    /// it takes each `SchedCore` singly — there is no A→A' edge either. (Same rule the deleted
    /// `eager_join_deadlocked` documented; it is tightened here, not relaxed.)
    pub(super) fn quiesced(&self, exec_registry: &ExecRegistry) -> bool {
        self.verdict(exec_registry).is_some()
    }

    /// The verdict, PLUS "and every registered party is an [`PartyWait::Join`]" — the narrow question
    /// [`super::Vm::join_eager_jobs`] asks (gaps.md W7-58 residual).
    ///
    /// A joiner must not fault while any OTHER kind of party is registered, and the reason is the
    /// quality of the diagnostic, not caution. Every non-`Join` party has a judge of its own — a
    /// channel/`wait:` party polls [`super::Vm::block_halt_check`] at the same 5 ms cadence, and a
    /// `Nursery` party is judged by its own sched's idle worker — and those faults name the actual
    /// blocking SITE (`recv on an empty channel: deadlock` at the offending line) instead of the join
    /// that is merely downstream of it. Letting the joiner race them would make WHICH message a user
    /// sees a coin flip (measured: `two_executors_deadlocking_each_other_fault`'s shape flipped
    /// between the two). When every party IS a `Join`, there is no other judge at all — that is
    /// exactly the residual, and then the joiner must speak.
    pub(super) fn quiesced_only_joins(&self, exec_registry: &ExecRegistry) -> bool {
        self.verdict(exec_registry) == Some(true)
    }

    /// `None` = not stuck. `Some(only_joins)` = stuck, and `only_joins` says whether every registered
    /// party is a [`PartyWait::Join`]. One evaluation under ONE hold of the party lock, so the two
    /// public questions can never disagree about the party set (see this function's `quiesced` doc).
    fn verdict(&self, exec_registry: &ExecRegistry) -> Option<bool> {
        let parties = self.parties.lock().unwrap_or_else(|e| e.into_inner());
        // `1 +` is the main thread, which is a party for the whole run and is the only one not owned
        // by an executor slot. Read under the party lock so a `submit` cannot slip a new job past a
        // count already taken (it could only be issued by a RUNNING party, which is unregistered and
        // therefore already vetoes — but the read is free here and the invariant is worth pinning).
        // §2c1 — plus one per OUTERMOST eager nursery that still holds an undone task: those fibers
        // are uncounted senders, and this is the term that stops a healthy `spawn: ch.send(1)` beside
        // a blocking `ch.recv()` on `main` from reading as a deadlock. See `eager_bodies`.
        let live = 1 + Self::outstanding_jobs(exec_registry) + self.live_eager_bodies();
        if parties.len() < live {
            return None; // somebody is still running — they may yet send.
        }
        if parties.iter().any(|p| p.satisfiable()) {
            return None;
        }
        Some(parties.iter().all(|p| matches!(**p, PartyWait::Join(..))))
    }

    /// §2c1 — publish an OUTERMOST eager nursery's sched, so [`Self::live_eager_bodies`] can count its
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
        live.iter()
            .filter(|s| {
                let c = s.lock();
                c.any_scope_incomplete() && !s.is_deadlocked_ignoring_jobs(&c)
            })
            .count()
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
