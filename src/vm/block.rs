//! TICKET-181 (wave 16 Family 2, "Blocking contexts") — the ONE answer to "may this op block
//! here, and how?". Every blocking op names its wait as a [`WaitSpec`]; the execution context is
//! derived on every call by [`Vm::block_ctx`] (never cached — its inputs change at many sites);
//! and [`mode`] is the table that joins them. `docs/reference/concurrency/` "Blocking-context table" is the
//! prose copy of [`mode`], and `tests::every_cell_matches_the_table` pins every cell.
//!
//! The per-op MECHANISM (park on a channel bucket, park on the netpoller, the timer thread) stays
//! with each op; only the decision lives here.

use super::gc_door::Detached;
use super::quiesce::{PartyGuard, PartyWait};
use super::sched::BlockedOwnerGuard;
use super::{GcRef, sysmon, width};
use super::{MnSched, RuntimeError, SchedCore, Span, TraceFrame, Vm};
use crate::native::Kind;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Where the running code sits, as far as blocking is concerned. Derived by [`Vm::block_ctx`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum BlockCtx {
    /// An M:N worker shell running a fiber with no host frame under it: the fiber can snapshot-park.
    Park,
    /// An M:N worker shell inside a native callback, `defer:` drain, generator resume or
    /// `Shared.update` closure: the host stack cannot be unwound, so the worker blocks in place and
    /// hands its runner slot to a replacement.
    Demote,
    /// The inline outermost-`parallel:` builder (`mn_enlist_sched`). It has no worker loop to
    /// drive a park.
    Builder { reentered: bool },
    /// A thread that owns itself: `main`, a `main` `defer:`, `main` inside a callback.
    OwnThread { judged: bool, reentered: bool },
}

impl BlockCtx {
    /// May the process-wide deadlock verdict JUDGE this party (DEC-136)?
    ///
    /// `quiesce`'s `live` count is `1 (main) + Σ outstanding` over the run's executors, so exactly
    /// two kinds of thread are counted: `main` and an eager `Executor` job — the two with no
    /// scheduler of any kind under them — and only while every native re-entry is a synchronous
    /// hard-halt `defer` drain (`native_reentry == deferring`: `--max-heap`, `--timeout`; its body is
    /// VM code on the same thread). Every other `defer` drain runs on frames, so it needs no
    /// exception: its wait is judged at `native_reentry == 0`, or parks on a fiber. A worker
    /// shell, the inline builder, or a party inside a real callback is not counted; each can only
    /// run user code while some counted party is inside a nursery or a native call, and such a
    /// party is live and unregistered, which vetoes the verdict. Never widen this to a callback
    /// re-entry: registering such a party deletes that veto and turns a safe hang into a false
    /// fault. See [`crate::vm::quiesce`] for the full argument.
    pub(super) fn judged(self) -> bool {
        matches!(self, BlockCtx::OwnThread { judged: true, .. })
    }
}

/// What a blocking op waits for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum WaitSpec {
    /// an empty `recv` on an ordinary channel
    Recv,
    /// a `recv` on a `time.timer(ms)` channel (its value is synthesised at the deadline)
    Timer,
    /// a full `send`
    Send,
    /// a multi-arm `wait:`; `deadline` = a timer arm, `has_send` = a send arm
    Wait { deadline: bool, has_send: bool },
    /// `time.sleep_ms` (`Kind::TimedWait`)
    Sleep,
    /// an off-heap blocking native: `std.fs`, `std.request`, `std.process` (`Kind::Blocking`)
    Offload,
    /// a host fd wait: a stdin read (`Kind::HostWait`), or a `Writer`/`Reader` syscall on a FIFO, a
    /// pipe or a device (it waits without end for the party on the other end)
    HostFd,
    /// a would-block socket `accept`/`read`/`write`
    Socket,
    /// a `net.connect` handshake in flight
    Connect,
    /// a `Shared`/`RwShared` update-guard acquire
    Guard,
    /// `Executor.shutdown()` / an Executor join
    Join,
    /// a `parallel:` nursery join
    Nursery,
    /// an `extern` C call: it keeps its permit, and the watcher hands over the permit of a long
    /// one (`sysmon`)
    Extern,
}

impl WaitSpec {
    /// Does this wait end without any Chezzi party acting — a deadline or an external completion?
    /// Such a demoted wait is accounted `inflight` (it vetoes the verdict); every other one
    /// registers a waiter the verdict asks "can you still be satisfied?". A guard is never
    /// `inflight` (DEC-063).
    pub(super) fn will_return(self) -> bool {
        match self {
            WaitSpec::Timer
            | WaitSpec::Sleep
            | WaitSpec::Offload
            | WaitSpec::HostFd
            | WaitSpec::Socket
            | WaitSpec::Connect
            | WaitSpec::Extern => true,
            WaitSpec::Wait { deadline, .. } => deadline,
            WaitSpec::Recv
            | WaitSpec::Send
            | WaitSpec::Guard
            | WaitSpec::Join
            | WaitSpec::Nursery => false,
        }
    }

    /// The row of a blocking native, by its `Kind`; `None` for a native that does not block.
    pub(super) fn of_native(kind: Kind) -> Option<WaitSpec> {
        match kind {
            Kind::TimedWait => Some(WaitSpec::Sleep),
            Kind::Blocking => Some(WaitSpec::Offload),
            Kind::HostWait => Some(WaitSpec::HostFd),
            Kind::Inline | Kind::InterceptIo | Kind::InterceptNet | Kind::InterceptTimer => None,
        }
    }

    /// The row of a syscall on a `Writer`/`Reader` fd: `HostFd` when the fd can wait without end (a
    /// FIFO, a pipe or a device), `None` for a regular file, whose syscalls answer at once.
    pub(super) fn of_fd(waits: bool) -> Option<WaitSpec> {
        waits.then_some(WaitSpec::HostFd)
    }
}

/// How a wait hands this thread's runner slot over: [`wait_steps`]'s answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Handover {
    /// the thread keeps its slot
    Keep,
    /// [`Vm::block_enter`]: a replacement worker takes the slot, and the wait is accounted
    Demote(Account),
    /// a nursery joiner that may not run fibers on its own stack: [`Vm::joiner_shell`] gives the
    /// joined sched a runner in its place
    Joiner,
}

/// What a wait does with the thread's run permit: [`wait_steps`]'s answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Permit {
    /// keep it (a wait that ends at once)
    Keep,
    /// give it back for the whole wait (Go: a blocking syscall hands off its P), and give the
    /// runner slot (bit 0) of the `parallel:` body the thread runs to the claim, which starts a
    /// runner for the tasks the body queued (TICKET-211). The slot goes in [`Vm::wait_enter`] and
    /// [`Vm::nursery_counts`], so a guard's bounded first stage (DEC-016) hands it over at its
    /// second stage.
    Release,
    /// keep it, and stamp the thread's watch slot: the watcher releases the permit of a call that
    /// lasts past a tick (Go `sysmon` retakes the P of a cgo call)
    Watched,
}

/// How a demoted wait is accounted while its worker is off `running`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Account {
    /// a [`Waiter`] the sched verdict asks "can you still be satisfied?"
    Waiter,
    /// `inflight`: a wait that ends by itself, which vetoes the sched verdict
    Inflight,
}

/// How a wait marks the eager nursery bodies this thread runs ([`Vm::mark_bodies`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum BodyMark {
    /// blocked on the wait, which each body's sched publishes as a waiter
    Wait,
    /// blocked in a nested nursery join: the body comes back to its scope after it (`awaiting_builder`)
    Awaiting,
}

/// The steps a wait on a VM host thread takes besides the GC detach that every wait takes, and the
/// counts it registers for the two deadlock verdicts: [`wait_steps`]'s answer. [`Vm::wait_begin`]
/// takes them and [`Vm::wait_end`] drops them; a nursery join takes its counts through
/// [`Vm::nursery_counts`].
///
/// A parked fiber registers none of these: its park files it with its sched (`parked_n`,
/// `join_parked`, a timer's `inflight`). A parked fiber is not a waiting thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct WaitSteps {
    pub(super) permit: Permit,
    pub(super) hand: Handover,
    /// a party of the process-wide verdict (`QuiesceState::parties`)
    pub(super) party: bool,
    pub(super) bodies: Option<BodyMark>,
    /// a blocked owner on this fiber's own sched (`SchedCore::blocked_owners`)
    pub(super) owner: bool,
}

impl WaitSteps {
    /// No counts, no demote and no body slot to hand over: only the permit step.
    fn registers_nothing(self) -> bool {
        self.hand == Handover::Keep
            && !self.party
            && self.bodies.is_none()
            && !self.owner
            && self.permit != Permit::Release
    }
}

/// THE steps of a wait, from its kind and the block context. `None` is a wait that ends at once (a
/// regular-file syscall, a stat, a permit take): it keeps the permit and counts nothing.
///
/// - An `extern` C call counts nothing: a thread in C is running, so both verdicts decline while
///   it lasts (Go: a goroutine in cgo is not asleep). Its permit is [`Permit::Watched`].
/// - A wait that ends by itself ([`WaitSpec::will_return`]) counts nothing: it is a live party, so
///   both verdicts decline while it lasts. A demoted one is `inflight`.
/// - Any other wait is a judged party ([`BlockCtx::judged`]), marks its bodies blocked on it, and a
///   demoted one is a [`Waiter`].
/// - A nursery join is a party on every thread with no sched under it (`main`, the builder, `main`
///   in a callback: the `1 +` of `live`), and a blocked owner on a shell. Its bodies wait for it.
///   Its joiner hands its slot over in `Vm::joiner_shell` where it may not help (the `Demote` and
///   `InPlace` cells of [`mode`]).
pub(super) fn wait_steps(spec: Option<WaitSpec>, ctx: BlockCtx) -> WaitSteps {
    let Some(spec) = spec else {
        return WaitSteps {
            permit: Permit::Keep,
            hand: Handover::Keep,
            party: false,
            bodies: None,
            owner: false,
        };
    };
    let shell = match ctx {
        BlockCtx::Park | BlockCtx::Demote => true,
        BlockCtx::Builder { .. } | BlockCtx::OwnThread { .. } => false,
    };
    match spec {
        WaitSpec::Extern => WaitSteps {
            permit: Permit::Watched,
            hand: Handover::Keep,
            party: false,
            bodies: None,
            owner: false,
        },
        WaitSpec::Nursery => WaitSteps {
            permit: Permit::Release,
            hand: match mode(ctx, spec) {
                BlockMode::Demote | BlockMode::InPlace => Handover::Joiner,
                BlockMode::Park | BlockMode::Help | BlockMode::InlineSleep | BlockMode::Refuse => {
                    Handover::Keep
                }
            },
            party: !shell,
            bodies: Some(BodyMark::Awaiting),
            owner: shell,
        },
        WaitSpec::Recv
        | WaitSpec::Timer
        | WaitSpec::Send
        | WaitSpec::Wait { .. }
        | WaitSpec::Sleep
        | WaitSpec::Offload
        | WaitSpec::HostFd
        | WaitSpec::Socket
        | WaitSpec::Connect
        | WaitSpec::Guard
        | WaitSpec::Join => {
            let ends = spec.will_return();
            let account = if ends {
                Account::Inflight
            } else {
                Account::Waiter
            };
            WaitSteps {
                permit: Permit::Release,
                hand: if mode(ctx, spec) == BlockMode::Demote {
                    Handover::Demote(account)
                } else {
                    Handover::Keep
                },
                party: !ends && ctx.judged(),
                bodies: (!ends).then_some(BodyMark::Wait),
                owner: false,
            }
        }
    }
}

/// What a wait is blocked on, for the deadlock verdicts: the condition each verdict asks, and the
/// report the party gives when the process-wide verdict names it (`None` for a join, whose report
/// is the victims').
/// One `Arc` is shared by every registration of the wait, so they cannot disagree about what the
/// thread waits for.
pub(super) struct Blocked {
    wait: Arc<PartyWait>,
    site: Option<(&'static str, Span)>,
    /// The wait as the deadlock listing names it ([`Vm::wait_site`]): `site`, or for a wait that
    /// is no party report (a demoted wait, an Executor join) the name [`Blocked::named`] gives.
    listed: Option<(&'static str, Span)>,
}

impl Blocked {
    pub(super) fn on(wait: PartyWait) -> Self {
        Blocked {
            wait: Arc::new(wait),
            site: None,
            listed: None,
        }
    }

    pub(super) fn at(wait: PartyWait, msg: &'static str, span: Span) -> Self {
        Blocked {
            wait: Arc::new(wait),
            site: Some((msg, span)),
            listed: Some((msg, span)),
        }
    }

    /// Name a wait that gives no party report, for the deadlock listing only.
    pub(super) fn named(mut self, wait: super::deadlock::Wait, span: Span) -> Self {
        self.listed = Some((wait.phrase(), span));
        self
    }
}

/// The counts a wait registered for the deadlock verdicts ([`WaitSteps`]), released on drop: the
/// body marks first (each body's veto comes back), then the blocked owner, then the party.
pub(super) struct Counts {
    owner: Option<BlockedOwnerGuard>,
    party: Option<PartyGuard>,
    /// raised `awaiting_builder` too ([`BodyMark::Awaiting`])
    awaiting: bool,
    bodies: Vec<(Arc<MnSched>, usize)>,
    /// the wait each sched in `bodies` publishes; `None` for a nested join
    wait: Option<Arc<PartyWait>>,
    /// the scheds whose body runner slot this wait handed over ([`Permit::Release`])
    runners: Vec<Arc<MnSched>>,
}

impl Counts {
    fn none() -> Self {
        Counts {
            owner: None,
            party: None,
            awaiting: false,
            bodies: Vec::new(),
            wait: None,
            runners: Vec::new(),
        }
    }
}

impl Drop for Counts {
    fn drop(&mut self) {
        for (s, scope) in &self.bodies {
            s.set_body_wait(*scope, self.wait.as_ref(), false, self.awaiting);
        }
        // TICKET-211 — the body runs again on its own thread: it takes bit 0 back.
        for s in &self.runners {
            s.body_runner(false);
        }
        drop(self.owner.take());
        drop(self.party.take());
    }
}

/// How an op blocks in a context.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum BlockMode {
    /// snapshot-park the fiber on the op's own wait bucket
    Park,
    /// block this worker in place and hand its runner slot to a replacement
    Demote,
    /// block this thread in place: it runs nothing else until the wait ends
    InPlace,
    /// a nursery join only: wait in place AND run the joined scheduler's fibers on this thread's
    /// stack. Legal only with no host frame under the joiner: a fiber run there may wait for
    /// something only a buried frame can give (TICKET-245).
    Help,
    /// sleep this thread to the op's own deadline
    InlineSleep,
    /// refuse: the op returns its documented error or fault instead of blocking
    Refuse,
}

/// THE blocking-context table. Every blocking op asks this, and nothing else, how to block.
/// Changing a cell means changing this, `tests::every_cell_matches_the_table` and the
/// `docs/reference/concurrency/` "Blocking-context table" in one commit.
pub(super) fn mode(ctx: BlockCtx, spec: WaitSpec) -> BlockMode {
    use BlockMode::{Demote, Help, InPlace, InlineSleep, Park, Refuse};
    use WaitSpec as W;
    let join = |reentered: bool| if reentered { InPlace } else { Help };
    match ctx {
        BlockCtx::Park => match spec {
            // DEC-151: stdin is a host wait we do not own. A guard wait parks on the box's lock
            // (Go `sync.Mutex`); an Executor join parks on the joined sched's completion.
            W::HostFd => Demote,
            W::Recv
            | W::Guard
            | W::Join
            | W::Timer
            | W::Send
            | W::Wait { .. }
            | W::Sleep
            | W::Offload
            | W::Socket
            | W::Connect
            | W::Nursery => Park,
            W::Extern => InPlace,
        },
        BlockCtx::Demote => match spec {
            W::Connect | W::Extern => InPlace,
            // TICKET-185: a demoted sender blocks on its published offer, as Go blocks.
            // TICKET-245: a nursery joiner hands its runner slot over and only waits.
            W::Send
            | W::Nursery
            | W::Recv
            | W::Timer
            | W::Wait { .. }
            | W::Sleep
            | W::Offload
            | W::Join
            | W::HostFd
            | W::Socket
            | W::Guard => Demote,
        },
        BlockCtx::Builder { reentered } => match spec {
            W::Recv
            | W::Send
            | W::Wait {
                deadline: false, ..
            }
            | W::Socket => Refuse,
            W::Wait { deadline: true, .. } | W::Timer | W::Sleep => InlineSleep,
            W::HostFd | W::Guard => Demote,
            W::Connect | W::Offload | W::Join | W::Extern => InPlace,
            W::Nursery => join(reentered),
        },
        BlockCtx::OwnThread { reentered, .. } => match spec {
            W::Timer | W::Sleep => InlineSleep,
            W::HostFd | W::Guard => Demote,
            W::Recv
            | W::Send
            | W::Wait { .. }
            | W::Offload
            | W::Socket
            | W::Connect
            | W::Join
            | W::Extern => InPlace,
            W::Nursery => join(reentered),
        },
    }
}

/// TICKET-195 (W18 Family B2) — THE record of why a party is unwinding when the fault is not its
/// own. `None` (on `Vm::cut`) means the party unwinds its OWN fault. Every rule keyed on "was this
/// party cut" reads it: the recover bypass, the defer ranking (`Vm::unwind_result`), the outcome
/// classification and the trace. `Vm::adopt_child_fault` is the only writer of `Delivered`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cut {
    /// a cancel flag this party holds tripped (a sibling faulted or exited); latches for the unwind
    Cancelled,
    /// another party's fault was delivered to this one: a child's halt (`floor = Some(n)`, the
    /// nursery below which a `recover:` must not catch, DEC-096), a join that reduced a child's or
    /// a job's fault, or an unjoined job fault reported in place of a verdict (`floor = None`)
    Delivered { floor: Option<usize> },
    /// TICKET-208 — the run-wide job abort was delivered to this party: a
    /// `Executor` job hit its `--max-heap` limit, which ends the run. No `recover:` catches it (Go: a goroutine's
    /// panic cannot be recovered from `main`); the party's `defer`s still run.
    RunFault,
}

/// TICKET-188 (W17 Family B) — why a party must stop now: [`halt_of`]'s answer.
#[derive(Debug)]
pub(super) enum Halt {
    /// a cancel flag this party holds is tripped
    Cancelled,
    /// a child of a nursery this party owns faulted; `floor` is that nursery's `nurseries` index,
    /// below which a `recover:` must not catch the fault (DEC-096)
    /// TICKET-195: `trace` is the faulting child's own stack trace
    ChildFault {
        floor: usize,
        err: RuntimeError,
        trace: Vec<TraceFrame>,
    },
}

/// One open nursery of a party, borrowed: what [`halt_of`] reads.
pub(super) struct OwnedRef<'a> {
    /// the nursery's `nurseries` index
    pub(super) n: usize,
    /// the nursery's cancel flag, which a faulting child trips in `MnSched::finish_with`, in the
    /// lock hold that stores its fault
    pub(super) flag: &'a Arc<AtomicBool>,
    pub(super) sched: &'a Arc<MnSched>,
    pub(super) scope: usize,
    /// TICKET-103 continuation scopes of the same family
    pub(super) more: &'a [usize],
}

/// One open nursery of a party, owned, for a wait that outlives the borrow of the party's `Vm`.
pub(super) struct OwnedScope {
    n: usize,
    flag: Arc<AtomicBool>,
    sched: Arc<MnSched>,
    scope: usize,
    more: Vec<usize>,
}

impl OwnedScope {
    pub(super) fn of(o: &OwnedRef<'_>) -> Self {
        OwnedScope {
            n: o.n,
            flag: Arc::clone(o.flag),
            sched: Arc::clone(o.sched),
            scope: o.scope,
            more: o.more.to_vec(),
        }
    }

    fn as_ref(&self) -> OwnedRef<'_> {
        OwnedRef {
            n: self.n,
            flag: &self.flag,
            sched: &self.sched,
            scope: self.scope,
            more: &self.more,
        }
    }
}

/// TICKET-188 — THE wake set of a blocked party: its cancel flags plus the flags of the nurseries
/// it owns. Every wait registration reads it ([`Vm::wake_set`]); empty where nothing may cut the
/// party (inside its own `defer`, or already unwinding).
#[derive(Default)]
pub(super) struct WakeSet {
    pub(super) cancel: Vec<Arc<AtomicBool>>,
    pub(super) owned: Vec<OwnedScope>,
}

impl WakeSet {
    /// Every flag whose trip may end the wait: the cancel flags, then the owned flags. A hint only.
    pub(super) fn flags(&self) -> Vec<Arc<AtomicBool>> {
        self.cancel
            .iter()
            .cloned()
            .chain(self.owned.iter().map(|o| Arc::clone(&o.flag)))
            .collect()
    }

    /// [`halt_of`] over this set. Takes the owned nurseries' sched locks: never call it under one.
    pub(super) fn halt(&self) -> Option<Halt> {
        halt_of(
            self.cancel.iter().map(|a| &**a),
            self.owned.iter().map(OwnedScope::as_ref),
            false,
        )
    }
}

/// TICKET-188 — THE decider of "must this party stop now, and why?". Cancel first; then, unless a
/// run-wide exit is pending (DEC-096: `Exit > Fault`), the innermost owned nursery whose flag is
/// tripped AND whose family recorded a `Fault`. A flag is only a wake hint. A fault trips it under
/// the core lock with its slot (`MnSched::finish_with`), but other trips record no fault. Each
/// production caller of `trip_cancel_flag`:
/// - a child's `Exit` end (`finish_with`);
/// - the run-wide exit (`MnSched::cancel_all`, from `halt_all_scheds`);
/// - the abort seams (`trip_scope_cancel` from `abort_enlisted_scope`, the eager-nursery abort and
///   `sched::cancel_fiber_owned_family`);
/// - a `--timeout` gap wake (`netio::deadline_gap_wake`);
/// - an Executor job cut (`SchedCore::cancel_one`), an Executor join that timed out
///   (`worker_vm.rs`) and `Executor.cancel()` (`shared_methods.rs`).
///
/// The `--max-heap` backlog trip in `finish_with` turns the outcome into a `Fault`. So a tripped
/// flag with no recorded fault is not a halt.
pub(super) fn halt_of<'a>(
    cancel: impl Iterator<Item = &'a AtomicBool>,
    owned: impl DoubleEndedIterator<Item = OwnedRef<'a>>,
    exit_pending: bool,
) -> Option<Halt> {
    let mut cancel = cancel;
    if cancel.any(|f| f.load(Ordering::Relaxed)) {
        return Some(Halt::Cancelled);
    }
    if exit_pending {
        return None;
    }
    owned.rev().find_map(|o| {
        if !o.flag.load(Ordering::Acquire) {
            return None;
        }
        let (err, trace) = std::iter::once(o.scope)
            .chain(o.more.iter().copied())
            .find_map(|sid| o.sched.scope_fault(sid))?;
        Some(Halt::ChildFault {
            floor: o.n,
            err,
            trace,
        })
    })
}

/// One blocked waiter in `SchedCore::waiters`, the registry the deadlock verdict asks.
pub(super) struct Waiter {
    /// what it waits for
    pub(super) wait: Arc<PartyWait>,
    /// the wake set's flags ([`WakeSet::flags`]); a hint only -- `satisfiable` runs under core
    /// lock A and must not call `scope_fault`
    pub(super) cancel: Vec<Arc<AtomicBool>>,
    /// a demoted fiber (a victim the verdict may claim), not a blocked body
    pub(super) fiber: bool,
    /// the scope of the demoted fiber; `None` for a blocked body
    pub(super) scope: Option<usize>,
    /// the verdict cut this waiter ([`SchedCore::cut_aborting_families`])
    pub(super) cut: bool,
}

impl Waiter {
    /// Could this waiter already resume? Generous by design (DEC-028): a false `true` only
    /// declines the verdict, a false `false` faults a live program.
    pub(super) fn satisfiable(&self) -> bool {
        self.cancel.iter().any(|f| f.load(Ordering::Relaxed)) || self.wait.satisfiable()
    }
}

/// A demote bracket entered by [`Vm::block_enter`]: how the wait is accounted while this worker
/// is off `running`. Ended by [`DemoteReg::release`] (a waiter-registered wait, in the same lock
/// hold as the pop or settle that ends it — DEC-176) or [`Vm::block_exit`] (an `inflight` wait).
struct DemoteReg {
    /// the `SchedCore::waiters` token, or `None` for an `inflight` wait
    tok: Option<u64>,
}

impl DemoteReg {
    /// End the bracket under core lock A: back onto `running`, and un-account the wait.
    fn release(self, sched: &MnSched, c: &mut SchedCore) {
        c.running += 1;
        match self.tok {
            Some(tok) => c.unregister_waiter(tok),
            None => {
                sched.leave_inflight();
            }
        }
    }
}

/// A wait started by [`Vm::wait_begin`]: the counts it registered, the demote bracket when its
/// steps take one, and whether it gave its run permit back. [`Vm::wait_end`] ends it.
pub(super) struct WaitReg {
    counts: Counts,
    demote: Option<DemoteReg>,
    released: bool,
    /// the thread stamped its watch slot ([`Permit::Watched`])
    watched: bool,
    /// a tick of this wait already ran ([`Vm::wait_tick`])
    ticked: bool,
}

impl WaitReg {
    /// The `SchedCore::waiters` token of this wait, if it registered a waiter.
    pub(super) fn tok(&self) -> Option<u64> {
        self.demote.as_ref().and_then(|d| d.tok)
    }

    /// Take the counts out of this wait, for an in-place channel tick: it drops them after the
    /// permit take and after its own `GatedReg`, before its next pop. Dropping them before the
    /// `GatedReg` made `benches/sched/rendezvous_pingpong` at T=1 14% slower (r=15).
    pub(super) fn take_counts(&mut self) -> Counts {
        std::mem::replace(&mut self.counts, Counts::none())
    }

    /// End a registered wait's demote under core lock A, in the same hold as the pop or settle
    /// that ends it (DEC-176). A no-op when the wait took no demote. [`Vm::wait_end`] still ends
    /// the wait.
    pub(super) fn release(&mut self, sched: &MnSched, c: &mut SchedCore) {
        if let Some(reg) = self.demote.take() {
            reg.release(sched, c);
        }
    }
}

impl Vm {
    /// THE read of a blocked-in-place wait's cut: the sched-wide one
    /// ([`SchedCore::cut_in_place`]), or this wait's own ([`SchedCore::waiter_cut`]). The verdict
    /// cuts a wait of its own as the stuck cleanup of a cancelled scope, so the party unwinds as
    /// cancelled ([`Cut::Cancelled`]): its `deadlock` stays swallowed, as a cancelled task's stuck
    /// cleanup's does, and the scope's cause is the report (DEC-147).
    /// The fault of this thread's wait when the verdict cut it ([`Vm::in_place_cut`]): `sched`'s
    /// deadlock fault, sited at the wait this thread is in ([`Vm::wait_site`], named by its
    /// [`Blocked`]).
    /// A wait no site named (a nursery join) adds no listing entry: its blocked children stand in
    /// its place.
    pub(super) fn cut_fault(&self, sched: &MnSched, span: Span) -> RuntimeError {
        self.cut_fault_on(sched, self.wait_site, span)
    }

    /// [`Vm::cut_fault`] of the wait at `site` (`None`: a nursery join).
    fn cut_fault_on(
        &self,
        sched: &MnSched,
        site: Option<(&'static str, Span)>,
        span: Span,
    ) -> RuntimeError {
        match site {
            Some((wait, at)) => self.cut_at(&sched.deadlock_err, wait, at),
            None => sched
                .deadlock_err
                .at_site(super::deadlock::Wait::Nursery.phrase(), span),
        }
    }

    pub(super) fn in_place_cut(&mut self, c: &SchedCore, tok: Option<u64>) -> bool {
        if c.cut_in_place() {
            return true;
        }
        if !c.waiter_cut(tok) {
            return false;
        }
        self.cut_as_cancel();
        true
    }

    /// An aborting family's cut of this task's stuck cleanup is its scope's cancel: the task
    /// unwinds as `Cancelled`, unless it already holds a cut (its cause ranks first, DEC-147).
    fn cut_as_cancel(&mut self) {
        if self.cut.is_none() {
            self.cut = Some(Cut::Cancelled);
        }
    }

    /// The verdict cut this fiber's wait in the `defer` it parked in (`how`,
    /// [`SchedCore::cut_parked`]): the wait ends with the verdict's fault, as a demoted cleanup's
    /// in-place wait does ([`Vm::in_place_cut`]). The resumed op does not re-run: the fault unwinds
    /// the `defer` body ([`Vm::on_step_fault`]), and the wait's pending entry is settled.
    /// A sched-wide cut names the fiber's own wait, `park_site` ([`Fiber::park_site`]), in the
    /// run's listing; an aborting family's cut is that scope's cancel and adds no entry.
    pub(super) fn take_verdict_cut(
        &mut self,
        how: super::sched_types::VerdictCut,
        park_site: Option<(&'static str, Span)>,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let Some(sched) = self.mn.clone() else {
            return Ok(());
        };
        drop(self.pending.take());
        let site = match how {
            super::sched_types::VerdictCut::Family => {
                self.cut_as_cancel();
                None
            }
            super::sched_types::VerdictCut::Sched => park_site,
        };
        let e = self.cut_fault_on(&sched, site, span);
        self.on_step_fault(0, e)
    }

    /// THE demote bracket: take this worker off `running` and account the wait — `inflight` (it
    /// vetoes the verdict), or a [`Waiter`] on `wait` the verdict asks "can you still be
    /// satisfied?". Spawns this thread's replacement worker once. With no M:N scheduler
    /// (`mn == None`) it keeps DEC-052's `yield_pool_slot(None)` and accounts nothing.
    fn block_enter(
        &mut self,
        account: Account,
        wait: Option<&Arc<PartyWait>>,
        what: &str,
        span: Span,
    ) -> Result<DemoteReg, RuntimeError> {
        let Some(sched) = self.mn.as_ref().map(Arc::clone) else {
            self.yield_pool_slot(None);
            return Ok(DemoteReg { tok: None });
        };
        let reg = {
            let mut c = sched.lock();
            c.leave_running();
            let tok = match account {
                Account::Waiter => Some(c.register_waiter(Waiter {
                    wait: Arc::clone(
                        wait.expect("a waiter-registered wait names what it waits for"),
                    ),
                    cancel: self.wake_set().flags(),
                    fiber: true,
                    scope: self.fiber_scope,
                    cut: false,
                })),
                Account::Inflight => {
                    sched.inflight.fetch_add(1, Ordering::Relaxed);
                    None
                }
            };
            drop(c);
            // An idle puller in an untimed `take_runnable` wait re-evaluates the verdict now that
            // this fiber left `running`; without it a genuine all-blocked quiesce hangs.
            sched.notify_waiters();
            DemoteReg { tok }
        };
        if !self.demote_once(&sched) {
            reg.release(&sched, &mut sched.lock());
            return Err(self.err(
                format!(
                    "{what} inside a native callback could not demote the worker (OS thread \
                     limit reached) — reduce concurrent in-callback blocking or raise the \
                     thread limit"
                ),
                span,
            ));
        }
        Ok(reg)
    }

    /// End an `inflight` bracket: takes core lock A itself. With no M:N scheduler there is
    /// nothing to undo, exactly as before (DEC-052).
    fn block_exit(&mut self, reg: DemoteReg) {
        debug_assert!(
            reg.tok.is_none(),
            "a waiter-registered wait releases under its own pop"
        );
        if let Some(sched) = self.mn.as_ref().map(Arc::clone) {
            reg.release(&sched, &mut sched.lock());
        }
    }

    /// THE counts of a wait ([`WaitSteps`]): the party first, with no sched lock held (P before
    /// A), then the body marks, then the blocked owner. `on` is what the party and the body
    /// marks wait on; `joined` is a nursery join's sched and scope.
    fn counts(
        &self,
        steps: WaitSteps,
        on: Option<&Blocked>,
        joined: Option<(&Arc<MnSched>, usize)>,
    ) -> Counts {
        let mut counts = Counts::none();
        if steps.party
            && let Some(b) = on
        {
            counts.party = Some(self.quiesce.block_shared(
                Arc::clone(&b.wait),
                self.wake_set(),
                b.site,
            ));
        }
        match steps.bodies {
            Some(BodyMark::Wait) => self.mark_bodies(&mut counts, false, on.map(|b| &b.wait)),
            Some(BodyMark::Awaiting) => self.mark_bodies(&mut counts, true, None),
            None => {}
        }
        if steps.permit == Permit::Release {
            self.hand_body_runners(&mut counts);
        }
        if steps.owner
            && let Some((sched, scope)) = joined
        {
            counts.owner =
                self.blocked_owner_guard(sched, self.owns_nested_sched(sched).then_some(scope));
        }
        counts
    }

    /// §2c1 — mark every eager nursery scope this thread runs the body of as blocked, publishing
    /// `wait` on its sched in the same `SchedCore` acquisition (`set_body_wait`). A body that is
    /// blocked cannot reach another `spawn`, so its `body_open` must stop vetoing the deadlock
    /// predicate (see [`super::JoinScope::body_blocked`]). A fiber-owned nursery's body is a
    /// counted fiber: marking its scope `awaiting_builder` would veto a genuine deadlock, so a shell
    /// marks nothing. Continuations need no entry: `set_body_wait` marks the whole family.
    fn mark_bodies(&self, counts: &mut Counts, awaiting: bool, wait: Option<&Arc<PartyWait>>) {
        counts.awaiting = awaiting;
        counts.wait = wait.cloned();
        counts.bodies = self
            .owned_eager_scopes()
            .filter(|s| !s.fiber_owned)
            .map(|s| {
                s.sched.set_body_wait(s.scope, wait, true, awaiting);
                (Arc::clone(&s.sched), s.scope)
            })
            .collect();
    }

    /// TICKET-211 — the body's thread stops running ([`Permit::Release`]): each `parallel:` body
    /// it runs gives its runner slot to the claim, which starts runners for whatever the body
    /// already queued. A fiber-owned nursery's body is a fiber; it has no slot of its own.
    fn hand_body_runners(&self, counts: &mut Counts) {
        counts.runners = self
            .owned_eager_scopes()
            .filter(|s| !s.fiber_owned && s.scope == 0)
            .map(|s| {
                let w = s.sched.body_runner(true);
                self.start_runners(&s.sched, w);
                Arc::clone(&s.sched)
            })
            .collect();
    }

    /// TICKET-095, TICKET-099 — count a fiber whose OS thread blocks in a nursery join as a
    /// `SchedCore::blocked_owners` on ITS OWN sched (`self.mn`), whether the joined `sched` is that
    /// same sched or another one: a fiber waiting on a join can feed nobody, so a peer veto
    /// (`MnSched::peer_can_move`) that asks `self.mn` "can you still feed me?" must see it blocked.
    /// TICKET-103 — it also counts the owner on its own scope (`JoinScope::owners_blocked`) and, when
    /// `joined` is `Some`, the joined nursery's origin scope on this same sched
    /// (`JoinScope::joins_blocked`); `flag_deadlock_leaves` reads both. `None` with no sched under
    /// this thread: such a joiner is a party instead.
    fn blocked_owner_guard(
        &self,
        sched: &Arc<MnSched>,
        joined: Option<usize>,
    ) -> Option<BlockedOwnerGuard> {
        let (owner, cross_sched) = if self.owns_nested_sched(sched) {
            (Arc::clone(sched), false)
        } else {
            let own = self.mn.clone()?;
            if Arc::ptr_eq(&own, sched) {
                return None;
            }
            (own, true)
        };
        {
            let mut c = owner.lock();
            c.owner_blocked(cross_sched);
            if let Some(s) = self.fiber_scope {
                c.scopes[s].owners_blocked += 1;
            }
            if let Some(j) = joined {
                debug_assert!(
                    !cross_sched,
                    "a joined scope lives on the owner's own sched"
                );
                c.scopes[j].joins_blocked += 1;
            }
        }
        owner.notify_waiters();
        Some(BlockedOwnerGuard {
            sched: owner,
            cross_sched,
            scope: self.fiber_scope,
            joined,
        })
    }

    /// The counts of a nursery join or abort of `scope` on `sched`, from the `Nursery` row of
    /// [`wait_steps`], held while the joiner waits. The join's permit step is the caller's
    /// [`Vm::wait_permit`] around the whole join.
    pub(super) fn nursery_counts(&self, sched: &Arc<MnSched>, scope: usize) -> Counts {
        let steps = wait_steps(Some(WaitSpec::Nursery), self.block_ctx());
        let on = Blocked::on(PartyWait::Nursery(Arc::clone(sched)));
        self.counts(steps, Some(&on), Some((sched, scope)))
    }

    /// The counts and the demote of a wait, steps (a) and (c) of [`wait_steps`].
    fn wait_enter(
        &mut self,
        steps: WaitSteps,
        on: Option<Blocked>,
        what: &str,
        span: Span,
    ) -> Result<(Counts, Option<DemoteReg>), RuntimeError> {
        let counts = self.counts(steps, on.as_ref(), None);
        self.wait_site = on.as_ref().and_then(|b| b.listed);
        let demote = match steps.hand {
            Handover::Demote(account) => {
                Some(self.block_enter(account, on.as_ref().map(|b| &b.wait), what, span)?)
            }
            Handover::Keep | Handover::Joiner => None,
        };
        Ok((counts, demote))
    }

    /// THE start of a wait, once for the whole wait (Go `entersyscall`): the counts [`wait_steps`]
    /// gives it on `on`, the demote when the steps say so ([`Vm::block_enter`] hands this worker's
    /// runner slot to a replacement and accounts the wait), then the run permit given back, if the
    /// thread holds one. Each tick of the wait runs through [`Vm::wait_door`]; [`Vm::wait_end`]
    /// ends it.
    pub(super) fn wait_begin(
        &mut self,
        spec: WaitSpec,
        on: Option<Blocked>,
        what: &str,
        span: Span,
    ) -> Result<WaitReg, RuntimeError> {
        let steps = wait_steps(Some(spec), self.block_ctx());
        let (counts, demote) = self.wait_enter(steps, on, what, span)?;
        let (released, watched) = self.permit_take(steps.permit);
        Ok(WaitReg {
            counts,
            demote,
            released,
            watched,
            ticked: false,
        })
    }

    /// The permit step alone: the start of a wait whose counts and demote come later
    /// ([`Vm::wait_demote`], the guard's second stage, DEC-016), or a nursery join, whose counts each
    /// join site takes ([`Vm::nursery_counts`]).
    pub(super) fn wait_permit(&mut self, spec: WaitSpec) -> WaitReg {
        let (released, watched) = self.permit_take(wait_steps(Some(spec), self.block_ctx()).permit);
        WaitReg {
            counts: Counts::none(),
            demote: None,
            released,
            watched,
            ticked: false,
        }
    }

    /// The permit step of a wait, if the thread holds a permit: `(released, watched)`.
    fn permit_take(&mut self, permit: Permit) -> (bool, bool) {
        let held = width::holds();
        let (released, watched) = match permit {
            Permit::Keep => (false, false),
            Permit::Release => (held, false),
            Permit::Watched => (false, held),
        };
        if released {
            width::release();
        }
        if watched {
            self.arm_extern_spare();
            sysmon::enter(self.extern_spare.as_ref().map_or(0, sysmon::Armed::id));
        }
        (released, watched)
    }

    /// Undo [`Vm::permit_take`]: take the permit back if the wait gave it up or the watcher retook
    /// it, a free one first.
    fn permit_back(&mut self, released: bool, watched: bool, pins: &[GcRef]) {
        let retaken = watched && !sysmon::leave();
        if retaken {
            self.extern_retaken();
        }
        if released || retaken {
            self.width_acquire_pinned(pins);
        }
    }

    /// A worker that a demote would replace arms its replacement once, for the watcher to start
    /// when it retakes a C call's permit (`sysmon`).
    fn arm_extern_spare(&mut self) {
        if self.extern_spare.is_none()
            && !self.demoted
            && self.cancel.is_some()
            && let Some(sched) = self.mn.as_ref().map(Arc::clone)
        {
            self.extern_spare = Some(sysmon::arm(self.spare(&sched, self.wid)));
        }
    }

    /// The watcher retook this thread's permit during a C call: a started spare is this worker's
    /// replacement, as [`Vm::demote_once`] would have spawned it.
    /// A spare the OS refused is dropped, so the next C call arms a new one, as a refused
    /// [`Vm::demote_once`] tries again at the next demote.
    pub(super) fn extern_retaken(&mut self) {
        let Some(state) = self.extern_spare.as_ref().map(sysmon::Armed::state) else {
            return;
        };
        match state {
            sysmon::SpareState::Waiting => {}
            sysmon::SpareState::Started => {
                self.extern_spare = None;
                self.demoted = true;
            }
            sysmon::SpareState::Refused => self.extern_spare = None,
        }
    }

    /// The counts and the demote on a wait that [`Vm::wait_permit`] started: the guard's second
    /// stage.
    pub(super) fn wait_demote(
        &mut self,
        reg: &mut WaitReg,
        spec: WaitSpec,
        on: Option<Blocked>,
        what: &str,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let steps = wait_steps(Some(spec), self.block_ctx());
        let (counts, demote) = self.wait_enter(steps, on, what, span)?;
        reg.counts = counts;
        reg.demote = demote;
        Ok(())
    }

    /// TICKET-193 — take the permit back in the middle of a wait, before the guard take: a thread
    /// that takes the guard first parks the guard's owner behind permit holders.
    pub(super) fn wait_retake(&mut self, reg: &WaitReg) {
        if reg.released {
            self.width_acquire();
        }
    }

    /// Give the permit back again after [`Vm::wait_retake`], when the take lost its race.
    pub(super) fn wait_rerelease(&mut self, reg: &WaitReg) {
        if reg.released {
            width::release();
        }
    }

    /// One tick of a wait that loops: [`Vm::wait_door`] around `f`. A later tick that finds a turn
    /// a waker queued for this thread (`width::reserved`, a gated wait) takes the turn and passes it
    /// on before it waits again, as a per-tick re-take of the permit did: a turn nobody takes blocks
    /// the head of the queue. The first tick does not, so a turn queued at the start of the wait is
    /// taken by [`Vm::wait_end`] in the waker's order.
    pub(super) fn wait_tick<R>(
        &mut self,
        reg: &mut WaitReg,
        pins: &[GcRef],
        f: impl FnOnce(&mut Detached<'_>) -> R,
    ) -> R {
        let pass = reg.released && reg.ticked;
        reg.ticked = true;
        self.wait_door(pins, |door| {
            if pass && !width::holds() && width::reserved() {
                width::acquire();
                width::release();
            }
            f(door)
        })
    }

    /// End a wait (Go `exitsyscall`): drop its counts, then take the permit back if the wait gave it
    /// up, a free one first (`pins` are the values the caller holds in Rust locals across that
    /// take), then end an `inflight` demote. The counts go first: a party still counted as blocked
    /// while it waits for a permit after its wait ended is a party that made progress.
    pub(super) fn wait_end(&mut self, reg: WaitReg, pins: &[GcRef]) {
        let WaitReg {
            counts,
            demote,
            released,
            watched,
            ticked: _,
        } = reg;
        drop(counts);
        self.wait_site = None;
        self.permit_back(released, watched, pins);
        if let Some(reg) = demote {
            self.block_exit(reg);
        }
    }

    /// A wait of one tick: [`Vm::wait_begin`], [`Vm::wait_door`] around `f`, [`Vm::wait_end`].
    /// For a `will_return` spec or `None`. A wait whose steps register nothing takes only its
    /// permit step: a short C call pays no [`Counts`].
    pub(super) fn wait_once<R>(
        &mut self,
        spec: Option<WaitSpec>,
        what: &str,
        span: Span,
        pins: &[GcRef],
        f: impl FnOnce(&mut Detached<'_>) -> R,
    ) -> Result<R, RuntimeError> {
        let Some(spec) = spec else {
            return Ok(self.wait_door(pins, f));
        };
        let steps = wait_steps(Some(spec), self.block_ctx());
        if steps.registers_nothing() {
            let (released, watched) = self.permit_take(steps.permit);
            let r = self.wait_door(pins, f);
            self.permit_back(released, watched, pins);
            return Ok(r);
        }
        let reg = self.wait_begin(spec, None, what, span)?;
        let r = self.wait_door(pins, f);
        self.wait_end(reg, pins);
        Ok(r)
    }

    /// A native or C call that holds its host thread: [`Vm::wait_once`], then the halt the call
    /// could not see while it ran, delivered at its return (TICKET-188, "Blocking calls cannot be
    /// interrupted"): the value that came back with it is dropped.
    pub(super) fn wait_call<R>(
        &mut self,
        spec: Option<WaitSpec>,
        what: &str,
        span: Span,
        pins: &[GcRef],
        f: impl FnOnce(&mut Detached<'_>) -> R,
    ) -> Result<R, RuntimeError> {
        let r = self.wait_once(spec, what, span, pins, f)?;
        self.return_halts(span)?;
        Ok(r)
    }

    /// The blocking context of the running code, derived from `mn`, `mn_enlist_sched`,
    /// `native_reentry` and `deferring` (the synchronous hard-halt drain depth) on every call. An
    /// `Executor` job is a fiber (TICKET-208).
    pub(super) fn block_ctx(&self) -> BlockCtx {
        let judged = self.native_reentry == self.deferring;
        let reentered = self.native_reentry > 0;
        if self.mn.is_some() {
            return if reentered {
                BlockCtx::Demote
            } else {
                BlockCtx::Park
            };
        }
        if self.mn_enlist_sched.is_some() {
            return BlockCtx::Builder { reentered };
        }
        BlockCtx::OwnThread { judged, reentered }
    }

    /// TICKET-194 — THE cancellation check an op makes before it waits. An op calls it only after
    /// its own ready check failed: a send with room, a recv with a value ready, a ready `wait:` arm
    /// or a free update guard completes and is never cut here (owner decision:
    /// only a wait or a loop back-edge is a cancellation point). `native_reentry == 0` gates it, as it gates the park: inside a
    /// callback the host stack cannot unwind.
    pub(super) fn wait_halt(&mut self, span: Span) -> Result<(), RuntimeError> {
        if self.native_reentry == 0
            && let Some(e) = self.take_halt(span)
        {
            return Err(e);
        }
        Ok(())
    }

    /// [`mode`] for the running code.
    pub(super) fn block_mode(&self, spec: WaitSpec) -> BlockMode {
        mode(self.block_ctx(), spec)
    }

    /// TICKET-245 — THE answer to "must this nursery joiner hand its runner slot over instead of
    /// running fibers on its own stack?", the `Nursery` row of [`wait_steps`]. Only
    /// [`Vm::joiner_shell`] asks it. A `Park` joiner that reaches an inline wait (an abort that
    /// `park_abort_origin` declines) still helps.
    pub(super) fn joiner_hands_over(&self) -> bool {
        wait_steps(Some(WaitSpec::Nursery), self.block_ctx()).hand == Handover::Joiner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every context, in the column order of [`TABLE`].
    fn contexts() -> Vec<BlockCtx> {
        let mut v = vec![
            BlockCtx::Park,
            BlockCtx::Demote,
            BlockCtx::Builder { reentered: false },
            BlockCtx::Builder { reentered: true },
        ];
        for judged in [false, true] {
            for reentered in [false, true] {
                v.push(BlockCtx::OwnThread { judged, reentered });
            }
        }
        v
    }

    /// Every wait, in the row order of [`TABLE`].
    fn specs() -> Vec<WaitSpec> {
        let mut v = vec![WaitSpec::Recv, WaitSpec::Timer, WaitSpec::Send];
        for deadline in [false, true] {
            for has_send in [false, true] {
                v.push(WaitSpec::Wait { deadline, has_send });
            }
        }
        v.extend([
            WaitSpec::Sleep,
            WaitSpec::Offload,
            WaitSpec::HostFd,
            WaitSpec::Socket,
            WaitSpec::Connect,
            WaitSpec::Guard,
            WaitSpec::Join,
            WaitSpec::Nursery,
            WaitSpec::Extern,
        ]);
        v
    }

    /// One row per spec, one letter per context: P Park, D Demote, I InPlace, H Help,
    /// S InlineSleep, R Refuse. Columns: Park, Demote, Builder as reentered = f, t, then
    /// OwnThread as (judged, reentered) = (f,f) (f,t) (t,f) (t,t). The last column is
    /// `will_return`.
    const TABLE: [(&str, &str, bool); 16] = [
        ("Recv", "PD RR IIII", false),
        ("Timer", "PD SS SSSS", true),
        ("Send", "PD RR IIII", false),
        ("Wait", "PD RR IIII", false),
        ("Wait+send", "PD RR IIII", false),
        ("Wait+deadline", "PD SS IIII", true),
        ("Wait+deadline+send", "PD SS IIII", true),
        ("Sleep", "PD SS SSSS", true),
        ("Offload", "PD II IIII", true),
        ("HostFd", "DD DD DDDD", true),
        ("Socket", "PD RR IIII", true),
        ("Connect", "PI II IIII", true),
        ("Guard", "PD DD DDDD", false),
        ("Join", "PD II IIII", false),
        ("Nursery", "PD HI HIHI", false),
        ("Extern", "II II IIII", true),
    ];

    #[test]
    fn every_cell_matches_the_table() {
        let letter = |m: BlockMode| match m {
            BlockMode::Park => 'P',
            BlockMode::Demote => 'D',
            BlockMode::InPlace => 'I',
            BlockMode::Help => 'H',
            BlockMode::InlineSleep => 'S',
            BlockMode::Refuse => 'R',
        };
        let specs = specs();
        assert_eq!(specs.len(), TABLE.len());
        for (spec, (name, row, will_return)) in specs.into_iter().zip(TABLE) {
            let got: String = contexts()
                .into_iter()
                .map(|c| letter(mode(c, spec)))
                .collect();
            let want: String = row.chars().filter(|c| *c != ' ').collect();
            assert_eq!(got, want, "row {name} ({spec:?})");
            assert_eq!(spec.will_return(), will_return, "will_return of {name}");
        }
    }

    /// [`wait_steps`]: one row per spec in the row order of [`TABLE`], one cell per context in its
    /// column order. A cell is the hand-over (K keep, W demote as a waiter, F demote as `inflight`,
    /// J the nursery joiner's hand-over), then the counts: `p` a party, `w`/`a` the body marks
    /// ([`BodyMark::Wait`], [`BodyMark::Awaiting`]), `o` a blocked owner, `-` for none. Every listed
    /// spec but `Extern` gives its permit back ([`Permit::Release`]); `Extern` is
    /// [`Permit::Watched`].
    ///
    /// The `Nursery` row's party cells are the W7-58 gate: a thread with no sched under it is the
    /// `1 +` of `live` and must count, the early-enlisted builder too (a `judged` gate would drop
    /// it); a worker shell must not, or `parties.len()` could exceed `live` and fault a live program.
    const STEPS: [(&str, &str); 16] = [
        ("Recv", "K-w- W-w- K-w- K-w- K-w- K-w- Kpw- Kpw-"),
        ("Timer", "K--- F--- K--- K--- K--- K--- K--- K---"),
        ("Send", "K-w- W-w- K-w- K-w- K-w- K-w- Kpw- Kpw-"),
        ("Wait", "K-w- W-w- K-w- K-w- K-w- K-w- Kpw- Kpw-"),
        ("Wait+send", "K-w- W-w- K-w- K-w- K-w- K-w- Kpw- Kpw-"),
        ("Wait+deadline", "K--- F--- K--- K--- K--- K--- K--- K---"),
        (
            "Wait+deadline+send",
            "K--- F--- K--- K--- K--- K--- K--- K---",
        ),
        ("Sleep", "K--- F--- K--- K--- K--- K--- K--- K---"),
        ("Offload", "K--- F--- K--- K--- K--- K--- K--- K---"),
        ("HostFd", "F--- F--- F--- F--- F--- F--- F--- F---"),
        ("Socket", "K--- F--- K--- K--- K--- K--- K--- K---"),
        ("Connect", "K--- K--- K--- K--- K--- K--- K--- K---"),
        ("Guard", "K-w- W-w- W-w- W-w- W-w- W-w- Wpw- Wpw-"),
        ("Join", "K-w- W-w- K-w- K-w- K-w- K-w- Kpw- Kpw-"),
        ("Nursery", "K-ao J-ao Kpa- Jpa- Kpa- Jpa- Kpa- Jpa-"),
        ("Extern", "K--- K--- K--- K--- K--- K--- K--- K---"),
    ];

    #[test]
    fn every_wait_steps_cell_matches_the_table() {
        let cell = |w: WaitSteps| {
            let hand = match w.hand {
                Handover::Keep => 'K',
                Handover::Demote(Account::Waiter) => 'W',
                Handover::Demote(Account::Inflight) => 'F',
                Handover::Joiner => 'J',
            };
            let bodies = match w.bodies {
                Some(BodyMark::Wait) => 'w',
                Some(BodyMark::Awaiting) => 'a',
                None => '-',
            };
            format!(
                "{hand}{}{bodies}{}",
                if w.party { 'p' } else { '-' },
                if w.owner { 'o' } else { '-' }
            )
        };
        let specs = specs();
        assert_eq!(specs.len(), STEPS.len());
        for (spec, (name, row)) in specs.into_iter().zip(STEPS) {
            let want = if spec == WaitSpec::Extern {
                Permit::Watched
            } else {
                Permit::Release
            };
            for c in contexts() {
                assert_eq!(
                    wait_steps(Some(spec), c).permit,
                    want,
                    "permit of {name} in {c:?}"
                );
            }
            let got: Vec<String> = contexts()
                .into_iter()
                .map(|c| cell(wait_steps(Some(spec), c)))
                .collect();
            assert_eq!(got.join(" "), row, "row {name} ({spec:?})");
        }
        for ctx in contexts() {
            assert_eq!(
                wait_steps(None, ctx),
                WaitSteps {
                    permit: Permit::Keep,
                    hand: Handover::Keep,
                    party: false,
                    bodies: None,
                    owner: false,
                },
                "None in {ctx:?}"
            );
        }
    }

    /// A spare whose start the OS refused does not demote the worker and does not stay armed: the
    /// next C call arms a new one.
    #[test]
    fn a_refused_spare_is_dropped_and_does_not_demote() {
        let mut vm = Vm::new(Arc::new(crate::vm::tests::empty_program()));
        vm.extern_spare = Some(sysmon::armed_refused());
        vm.extern_retaken();
        assert!(vm.extern_spare.is_none(), "a refused spare is dropped");
        assert!(!vm.demoted, "a refused spare is no replacement");
    }

    /// TICKET-188 — the owner-fault grid derives its op rows from [`TABLE`]: a new spec without a
    /// row there is a wait nobody proved a child fault reaches.
    #[test]
    fn every_wait_spec_has_an_owner_fault_grid_row() {
        const GRID: &str = include_str!("../../tests/owner_fault_grid.rs");
        for (name, _, _) in TABLE {
            assert!(
                GRID.contains(&format!("spec: \"{name}\"")),
                "WaitSpec row {name} has no owner-fault grid row in tests/owner_fault_grid.rs"
            );
        }
    }

    fn boom() -> RuntimeError {
        RuntimeError {
            message: "boom".to_string(),
            span: Span::RUNTIME,
            is_assert: false,
            is_over_memory: false,
            is_timed_out: false,
            is_deadlock: false,
            is_panic: false,
            blocked: None,
        }
    }

    /// A one-task nursery sched whose scope-0 flag is `tripped`, with a recorded `Fault` or not.
    fn owned_sched(tripped: bool, fault: bool) -> (Arc<AtomicBool>, Arc<MnSched>) {
        let flag = Arc::new(AtomicBool::new(tripped));
        let sched = Arc::new(MnSched::new(1, 1, Arc::clone(&flag), boom(), 0));
        if fault {
            sched.lock().slots[0] = Some(super::super::TaskOutcome::Fault {
                err: boom(),
                out: Vec::new(),
                stderr: Vec::new(),
                trace: Vec::new(),
            });
        }
        (flag, sched)
    }

    fn owned_ref<'a>(n: usize, flag: &'a Arc<AtomicBool>, sched: &'a Arc<MnSched>) -> OwnedRef<'a> {
        OwnedRef {
            n,
            flag,
            sched,
            scope: 0,
            more: &[],
        }
    }

    #[test]
    fn halt_of_prefers_cancel_over_a_child_fault() {
        let (flag, sched) = owned_sched(true, true);
        let cancel = AtomicBool::new(true);
        let got = halt_of(
            std::iter::once(&cancel),
            std::iter::once(owned_ref(0, &flag, &sched)),
            false,
        );
        assert!(matches!(got, Some(Halt::Cancelled)), "{got:?}");
        // With no cancel, the recorded child fault is the halt, at its nursery's floor.
        let idle = AtomicBool::new(false);
        let got = halt_of(
            std::iter::once(&idle),
            std::iter::once(owned_ref(2, &flag, &sched)),
            false,
        );
        assert!(
            matches!(&got, Some(Halt::ChildFault { floor: 2, err, .. }) if err.message == "boom"),
            "{got:?}"
        );
    }

    #[test]
    fn halt_of_declines_a_tripped_flag_with_no_recorded_fault() {
        // A trip with no fault in the slot: an `Exit` end, the run-wide exit or an abort seam.
        let (flag, sched) = owned_sched(true, false);
        let got = halt_of(
            std::iter::empty(),
            std::iter::once(owned_ref(0, &flag, &sched)),
            false,
        );
        assert!(got.is_none(), "{got:?}");
        // …and a recorded fault behind an untripped flag is not read at all (the flag is the hint).
        let (flag, sched) = owned_sched(false, true);
        let got = halt_of(
            std::iter::empty(),
            std::iter::once(owned_ref(0, &flag, &sched)),
            false,
        );
        assert!(got.is_none(), "{got:?}");
    }

    #[test]
    fn halt_of_yields_to_a_pending_exit() {
        let (flag, sched) = owned_sched(true, true);
        let got = halt_of(
            std::iter::empty(),
            std::iter::once(owned_ref(0, &flag, &sched)),
            true,
        );
        assert!(got.is_none(), "Exit outranks a child Fault: {got:?}");
        // A cancel still outranks the exit rung, as in `jump_checked`.
        let cancel = AtomicBool::new(true);
        let got = halt_of(
            std::iter::once(&cancel),
            std::iter::once(owned_ref(0, &flag, &sched)),
            true,
        );
        assert!(matches!(got, Some(Halt::Cancelled)), "{got:?}");
    }

    #[test]
    fn halt_of_reports_the_innermost_faulted_nursery() {
        let (outer_flag, outer) = owned_sched(true, true);
        let (inner_flag, inner) = owned_sched(true, true);
        let got = halt_of(
            std::iter::empty(),
            [
                owned_ref(0, &outer_flag, &outer),
                owned_ref(1, &inner_flag, &inner),
            ]
            .into_iter(),
            false,
        );
        assert!(
            matches!(got, Some(Halt::ChildFault { floor: 1, .. })),
            "{got:?}"
        );
    }

    /// TICKET-188 — the process-wide verdict reads the wake set too: an owner whose child faulted
    /// resumes at its next halt read, so it is not stuck (the `Join#15` false `deadlock` under load).
    #[test]
    fn a_party_whose_owned_nursery_faulted_vetoes_the_verdict() {
        let state: Arc<crate::vm::quiesce::QuiesceState> = Arc::default();
        let joined = owned_sched(false, false).1; // one undone task: the join is not satisfiable

        let (flag, sched) = owned_sched(true, true);
        let owner = state.block(
            PartyWait::Join(Arc::clone(&joined), 0),
            WakeSet {
                cancel: vec![],
                owned: vec![OwnedScope::of(&owned_ref(0, &flag, &sched))],
            },
        );
        assert!(
            !state.quiesced(),
            "an owner whose child faulted will be cut: the verdict must decline"
        );
        drop(owner);

        let (flag, sched) = owned_sched(true, false);
        let _owner = state.block(
            PartyWait::Join(Arc::clone(&joined), 0),
            WakeSet {
                cancel: vec![],
                owned: vec![OwnedScope::of(&owned_ref(0, &flag, &sched))],
            },
        );
        assert!(
            state.quiesced(),
            "a tripped flag with no recorded fault is not a halt: a genuine deadlock still faults"
        );
    }

    #[test]
    fn a_cancelled_party_does_not_veto_the_verdict() {
        let state: Arc<crate::vm::quiesce::QuiesceState> = Arc::default();
        let joined = owned_sched(false, false).1; // one undone task: the join is not satisfiable
        let _owner = state.block(
            PartyWait::Join(Arc::clone(&joined), 0),
            WakeSet {
                cancel: vec![Arc::new(AtomicBool::new(true))],
                owned: vec![],
            },
        );
        assert!(
            state.quiesced(),
            "a pending cancel is not a promise of progress: the verdict must still judge"
        );
    }

    #[test]
    fn of_native_maps_each_blocking_kind_to_its_row() {
        assert_eq!(WaitSpec::of_native(Kind::TimedWait), Some(WaitSpec::Sleep));
        assert_eq!(WaitSpec::of_native(Kind::Blocking), Some(WaitSpec::Offload));
        assert_eq!(WaitSpec::of_native(Kind::HostWait), Some(WaitSpec::HostFd));
        assert_eq!(WaitSpec::of_native(Kind::Inline), None);
        assert_eq!(WaitSpec::of_native(Kind::InterceptIo), None);
        assert_eq!(WaitSpec::of_native(Kind::InterceptNet), None);
    }
}
