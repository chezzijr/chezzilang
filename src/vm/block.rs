//! TICKET-181 (wave 16 Family 2, "Blocking contexts") — the ONE answer to "may this op block
//! here, and how?". Every blocking op names its wait as a [`WaitSpec`]; the execution context is
//! derived on every call by [`Vm::block_ctx`] (never cached — its inputs change at many sites);
//! and [`mode`] is the table that joins them. `docs/reference/concurrency/` "Blocking-context table" is the
//! prose copy of [`mode`], and `tests::every_cell_matches_the_table` pins every cell.
//!
//! The per-op MECHANISM (park on a channel bucket, park on the netpoller, the timer thread) stays
//! with each op; only the decision lives here.

use super::gc_door::Detached;
use super::{GcRef, sysmon, width};
use super::{MnSched, RuntimeError, SchedCore, Span, TraceFrame, Vm};
use crate::native::Kind;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Instant;

/// Where the running code sits, as far as blocking is concerned. Derived by [`Vm::block_ctx`].
/// Every party that runs code is a fiber on a worker shell (`main` is fiber 1, an `Executor` job a
/// fiber), so the one question is whether a host frame is under it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum BlockCtx {
    /// A fiber with no host frame under it: it can snapshot-park.
    Park,
    /// A fiber with a host (Rust) frame under it: an FFI callback, a map/set probe hook, the
    /// Rust-stack `defer` drain, or an `Inline` native's callee. The host stack cannot be unwound,
    /// so the worker blocks in place and hands its runner slot to a replacement.
    Reentered,
}

impl BlockCtx {
    /// May this fiber snapshot-park here: no host frame under it?
    /// (TICKET-298, SD4: THE reader; do not re-derive it from `native_reentry`.)
    pub(super) fn parks(self) -> bool {
        match self {
            BlockCtx::Park => true,
            BlockCtx::Reentered => false,
        }
    }

    /// Is a host (Rust) frame under the running code? (TICKET-298, SD4: THE reader of
    /// `native_reentry > 0`.)
    pub(super) fn reentered(self) -> bool {
        match self {
            BlockCtx::Park => false,
            BlockCtx::Reentered => true,
        }
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
            Kind::Inline | Kind::InterceptIo(_) | Kind::InterceptNet(_) | Kind::InterceptTimer => {
                None
            }
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
    /// give it back for the whole wait (Go: a blocking syscall hands off its P)
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

/// The steps a wait on a VM host thread takes besides the GC detach that every wait takes, and the
/// counts it registers for the two deadlock verdicts: [`wait_steps`]'s answer. [`Vm::wait_begin`]
/// takes them and [`Vm::wait_end`] drops them.
///
/// A parked fiber registers none of these: its park files it with its sched (`parked_n`,
/// `join_parked`, a timer's `inflight`). A parked fiber is not a waiting thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct WaitSteps {
    pub(super) permit: Permit,
    pub(super) hand: Handover,
}

impl WaitSteps {
    /// No counts and no demote: only the permit step.
    fn registers_nothing(self) -> bool {
        self.hand == Handover::Keep && self.permit != Permit::Release
    }
}

/// THE steps of a wait, from its kind and the block context. `None` is a wait that ends at once (a
/// regular-file syscall, a stat, a permit take): it keeps the permit and counts nothing.
///
/// - An `extern` C call keeps its permit, [`Permit::Watched`] (Go: a goroutine in cgo is not
///   asleep).
/// - A wait that ends by itself ([`WaitSpec::will_return`]), demoted, is `inflight`.
/// - Any other wait, demoted, is a [`Waiter`].
/// - A nursery joiner hands its slot over in `Vm::joiner_shell` where it may not help (the
///   `Demote` cell of [`mode`]).
pub(super) fn wait_steps(spec: Option<WaitSpec>, ctx: BlockCtx) -> WaitSteps {
    let Some(spec) = spec else {
        return WaitSteps {
            permit: Permit::Keep,
            hand: Handover::Keep,
        };
    };
    match spec {
        WaitSpec::Extern => WaitSteps {
            permit: Permit::Watched,
            hand: Handover::Keep,
        },
        WaitSpec::Nursery => WaitSteps {
            permit: Permit::Release,
            hand: match mode(ctx, spec) {
                BlockMode::Demote | BlockMode::InPlace => Handover::Joiner,
                BlockMode::Park => Handover::Keep,
            },
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
            }
        }
    }
}

/// Where a wait is blocked, for the deadlock listing ([`Vm::wait_site`]).
pub(super) struct Blocked {
    listed: Option<(&'static str, Span)>,
}

impl Blocked {
    /// A wait the listing names at `msg`/`span`.
    pub(super) fn at(msg: &'static str, span: Span) -> Self {
        Blocked {
            listed: Some((msg, span)),
        }
    }

    /// A wait the listing names as `wait` at `span`.
    pub(super) fn named(wait: super::deadlock::Wait, span: Span) -> Self {
        Blocked::at(wait.phrase(), span)
    }
}

/// How an op blocks in a context.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum BlockMode {
    /// snapshot-park the fiber on the op's own wait bucket
    Park,
    /// block this worker in place and hand its runner slot to a replacement
    Demote,
    /// block this thread in place, keeping its runner slot: a C call, or a `connect` handshake
    /// under a host frame
    InPlace,
}

/// THE blocking-context table. Every blocking op asks this, and nothing else, how to block.
/// Changing a cell means changing this, `tests::every_cell_matches_the_table` and the
/// `docs/reference/concurrency/` "Blocking-context table" in one commit.
pub(super) fn mode(ctx: BlockCtx, spec: WaitSpec) -> BlockMode {
    use BlockMode::{Demote, InPlace, Park};
    use WaitSpec as W;
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
        BlockCtx::Reentered => match spec {
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
    /// a child of a nursery this party owns faulted; `floor` is that nursery's `eager_scheds` index,
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
    /// the nursery's `eager_scheds` index
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
/// production caller of `trip_cancel_flag` (all in `mnsched_verdict.rs`):
/// - a child's `Exit` end (`finish_with`);
/// - the one `cancel`, for every other source: the run-wide exit (`All`, from `halt_all_scheds`),
///   an owner's abort of its nursery, an Executor join that timed out (`worker_vm.rs`),
///   `Executor.cancel()` and `Task.cancel()` (`shared_methods.rs`) and a `--timeout` gap wake
///   (`netio::deadline_gap_wake`).
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

/// One blocked-in-place waiter in `SchedCore::waiters`, filed by [`Vm::block_enter`] for the
/// length of a demoted wait: a waiter-accounted fiber, or an `inflight` one that files a [`Wake`].
pub(super) struct Waiter {
    /// a waiter-accounted demoted fiber: while one is filed, a verdict that ends the sched keeps
    /// its workers until it settles ([`SchedCore::end_asleep`]); an `inflight` wait is not one
    pub(super) fiber: bool,
    /// the wait's wake, kicked by every cancel and the run's verdict cut
    /// ([`MnSched::wake_demoted`])
    pub(super) wake: Option<Arc<Wake>>,
}

/// A demoted wait's wake handle, filed with its [`Waiter`] for the length of the wait
/// ([`Vm::wait_begin_woken`]). A waker whose change the wait's own predicate does not read (a
/// [`super::mnsched_verdict::cancel`], the run's verdict cut, a value on a `wait:` arm the wait
/// does not sleep on) calls [`Wake::kick`]: it bumps `kicks`, then notifies the condvar the wait
/// sleeps on while holding that condvar's mutex. The wait reads [`Wake::seen`] before it reads its
/// halts and sleeps only while [`Wake::kicked`] is false, so a kick cannot land between its last
/// check and its sleep: no demoted wait polls to notice a halt. A kick is only a wake; the wait
/// re-reads its own halts, so one that covers nothing costs a spurious wake.
pub(super) struct Wake {
    kicks: AtomicU64,
    on: WakeOn,
}

/// The condvar a demoted wait sleeps on, by the mutex it waits under.
pub(super) enum WakeOn {
    /// a channel's `q` and `cv`: a recv, a send, the first arm of a `wait:`
    Chan(Arc<super::ChannelCore>),
    /// the guard registry's: an update-guard wait ([`super::core::notify_guard_waiters`])
    Guard,
    /// a sched's core lock and `cv`: an Executor join ([`MnSched::join_tick`])
    Sched(Arc<MnSched>),
    /// the wait's own: a sleep
    Own(Arc<(Mutex<()>, Condvar)>),
}

impl std::fmt::Debug for Wake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wake")
            .field("kicks", &self.kicks)
            .finish_non_exhaustive()
    }
}

impl Wake {
    /// A wake for a wait that sleeps on `on`'s condvar.
    pub(super) fn new(on: WakeOn) -> Arc<Wake> {
        Arc::new(Wake {
            kicks: AtomicU64::new(0),
            on,
        })
    }

    /// Every kick so far; read before the wait's halts.
    pub(super) fn seen(&self) -> u64 {
        self.kicks.load(Ordering::Acquire)
    }

    /// Was the wait kicked since it read `seen`?
    pub(super) fn kicked(&self, seen: u64) -> bool {
        self.seen() != seen
    }

    /// Wake the wait: bump the count, then notify under the condvar's mutex. The caller holds no
    /// sched core lock.
    pub(super) fn kick(&self) {
        self.kicks.fetch_add(1, Ordering::AcqRel);
        match &self.on {
            WakeOn::Chan(c) => {
                drop(c.q.lock().unwrap_or_else(|e| e.into_inner()));
                c.cv.notify_all();
            }
            WakeOn::Guard => super::core::notify_guard_waiters(),
            WakeOn::Sched(s) => {
                drop(s.lock());
                s.cv.notify_all();
            }
            WakeOn::Own(own) => {
                drop(own.0.lock().unwrap_or_else(|e| e.into_inner()));
                own.1.notify_all();
            }
        }
    }
}

/// The sooner of two deadlines (`None`: no deadline).
pub(super) fn sooner(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// THE sleep of a demoted wait: on `cv` while `pending` holds, until `until` (`None`: no end). A
/// wait ends at its own deadline or the run's, or at a wake; never at a poll.
pub(super) fn sleep_until<'a, T>(
    cv: &Condvar,
    g: MutexGuard<'a, T>,
    until: Option<Instant>,
    pending: impl FnMut(&mut T) -> bool,
) -> MutexGuard<'a, T> {
    match until {
        None => cv.wait_while(g, pending).unwrap_or_else(|e| e.into_inner()),
        Some(d) => {
            cv.wait_timeout_while(g, d.saturating_duration_since(Instant::now()), pending)
                .unwrap_or_else(|e| e.into_inner())
                .0
        }
    }
}

/// A demote bracket entered by [`Vm::block_enter`]: how the wait is accounted while this worker
/// is off `running`. Ended by [`DemoteReg::release`] (a waiter-registered wait, in the same lock
/// hold as the pop or settle that ends it — DEC-176) or [`Vm::block_exit`] (an `inflight` wait).
struct DemoteReg {
    /// the `SchedCore::waiters` token: a waiter-accounted wait, or one that filed a [`Wake`]
    tok: Option<u64>,
    /// the wait is accounted `inflight`
    inflight: bool,
}

impl DemoteReg {
    /// End the bracket under core lock A: back onto `running`, and un-account the wait.
    fn release(self, sched: &MnSched, c: &mut SchedCore) {
        c.running += 1;
        if let Some(tok) = self.tok {
            c.unregister_waiter(tok);
        }
        if self.inflight {
            sched.leave_inflight();
        }
    }
}

/// A wait started by [`Vm::wait_begin`]: the demote bracket (and the counts it registered) when
/// its steps take one, and whether it gave its run permit back. [`Vm::wait_end`] ends it.
pub(super) struct WaitReg {
    demote: Option<DemoteReg>,
    released: bool,
    /// the thread stamped its watch slot ([`Permit::Watched`])
    watched: bool,
    /// a tick of this wait already ran ([`Vm::wait_tick`])
    ticked: bool,
}

impl WaitReg {
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
    /// The fault of this thread's wait when its sched was torn down under it
    /// ([`Vm::in_place_cut`]): `sched`'s deadlock fault, sited at the wait this thread is in
    /// ([`Vm::wait_site`], named by its [`Blocked`]); a wait no site named is a nursery join.
    pub(super) fn cut_fault(&self, sched: &MnSched, span: Span) -> RuntimeError {
        match self.wait_site {
            Some((wait, at)) => self.cut_at(&sched.deadlock_err, wait, at),
            None => sched.deadlock_err.at_site(span),
        }
    }

    /// THE read of a blocked-in-place wait's cut: the sched was torn down, or the run's verdict
    /// ended its tasks ([`SchedCore::cut_in_place`]). The wait's held guards release `CUT`.
    pub(super) fn in_place_cut(&mut self, c: &SchedCore) -> bool {
        if !c.cut_in_place() {
            return false;
        }
        self.cut_held_guards();
        true
    }

    /// THE demote bracket: take this worker off `running` and account the wait — `inflight`, or
    /// a [`Waiter`]. A `wake` is filed with the wait's [`Waiter`] (an `inflight` wait files one
    /// for it alone). Spawns this thread's replacement worker once. With no M:N scheduler
    /// (`mn == None`) it accounts and files nothing: the wait ends at its own deadline or the
    /// run's.
    fn block_enter(
        &mut self,
        account: Account,
        wake: Option<Arc<Wake>>,
        what: &str,
        span: Span,
    ) -> Result<DemoteReg, RuntimeError> {
        let inflight = account == Account::Inflight;
        let Some(sched) = self.mn.as_ref().map(Arc::clone) else {
            return Ok(DemoteReg {
                tok: None,
                inflight,
            });
        };
        let reg = {
            let mut c = sched.lock();
            c.leave_running();
            if inflight {
                sched.inflight.fetch_add(1, Ordering::Relaxed);
            }
            let tok = (!inflight || wake.is_some()).then(|| {
                c.register_waiter(Waiter {
                    fiber: !inflight,
                    wake,
                })
            });
            drop(c);
            // An idle puller in an untimed `take_runnable` wait re-evaluates the verdict now that
            // this fiber left `running`; without it a genuine all-blocked quiesce hangs.
            sched.notify_waiters();
            DemoteReg { tok, inflight }
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
            reg.inflight,
            "a waiter-registered wait releases under its own pop"
        );
        if let Some(sched) = self.mn.as_ref().map(Arc::clone) {
            reg.release(&sched, &mut sched.lock());
        }
    }

    /// The demote of a wait and the counts it registers, step (c) of [`wait_steps`].
    fn wait_enter(
        &mut self,
        steps: WaitSteps,
        on: Option<Blocked>,
        wake: Option<Arc<Wake>>,
        what: &str,
        span: Span,
    ) -> Result<Option<DemoteReg>, RuntimeError> {
        self.wait_site = on.as_ref().and_then(|b| b.listed);
        let demote = match steps.hand {
            Handover::Demote(account) => Some(self.block_enter(account, wake, what, span)?),
            Handover::Keep | Handover::Joiner => None,
        };
        Ok(demote)
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
        self.wait_begin_woken(spec, on, None, what, span)
    }

    /// [`Vm::wait_begin`] for a demoted wait that sleeps until a [`Wake`]: its demote files `wake`
    /// with the wait's [`Waiter`].
    pub(super) fn wait_begin_woken(
        &mut self,
        spec: WaitSpec,
        on: Option<Blocked>,
        wake: Option<Arc<Wake>>,
        what: &str,
        span: Span,
    ) -> Result<WaitReg, RuntimeError> {
        let steps = wait_steps(Some(spec), self.block_ctx());
        let demote = self.wait_enter(steps, on, wake, what, span)?;
        let (released, watched) = self.permit_take(steps.permit);
        Ok(WaitReg {
            demote,
            released,
            watched,
            ticked: false,
        })
    }

    /// The permit step alone: the start of a wait whose counts and demote come later
    /// ([`Vm::wait_demote`], the guard's second stage, DEC-016), or a nursery join, which counts
    /// nothing besides.
    pub(super) fn wait_permit(&mut self, spec: WaitSpec) -> WaitReg {
        let (released, watched) = self.permit_take(wait_steps(Some(spec), self.block_ctx()).permit);
        WaitReg {
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
        wake: Arc<Wake>,
        what: &str,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let steps = wait_steps(Some(spec), self.block_ctx());
        reg.demote = self.wait_enter(steps, on, Some(wake), what, span)?;
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

    /// End a wait (Go `exitsyscall`): take the permit back if the wait gave it up, a free one first
    /// (`pins` are the values the caller holds in Rust locals across that take), then end an
    /// `inflight` demote.
    pub(super) fn wait_end(&mut self, reg: WaitReg, pins: &[GcRef]) {
        let WaitReg {
            demote,
            released,
            watched,
            ticked: _,
        } = reg;
        self.wait_site = None;
        self.permit_back(released, watched, pins);
        if let Some(reg) = demote {
            self.block_exit(reg);
        }
    }

    /// A wait of one tick: [`Vm::wait_begin`], [`Vm::wait_door`] around `f`, [`Vm::wait_end`].
    /// For a `will_return` spec or `None`. A wait whose steps register nothing takes only its
    /// permit step: a short C call registers no count.
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

    /// The blocking context of the running code, derived from `native_reentry` on every call.
    /// Every party that waits is a fiber on a worker shell: `main` is fiber 1 (`Vm::run_root`), an
    /// `Executor` job a fiber (TICKET-208).
    pub(super) fn block_ctx(&self) -> BlockCtx {
        if self.native_reentry > 0 {
            BlockCtx::Reentered
        } else {
            BlockCtx::Park
        }
    }

    /// TICKET-194 — THE cancellation check an op makes before it waits. An op calls it only after
    /// its own ready check failed: a send with room, a recv with a value ready, a ready `wait:` arm
    /// or a free update guard completes and is never cut here (owner decision:
    /// only a wait or a loop back-edge is a cancellation point). `!BlockCtx::reentered` gates it, as
    /// it gates the park: inside a callback the host stack cannot unwind.
    pub(super) fn wait_halt(&mut self, span: Span) -> Result<(), RuntimeError> {
        if !self.block_ctx().reentered()
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
    /// running fibers on its own stack?", the `Nursery` row of [`wait_steps`]: a `Reentered`
    /// joiner does, as a fiber run on its stack may wait for something only a buried frame can
    /// give. Only [`Vm::joiner_shell`] asks it. A `Park` joiner that reaches an inline wait (an
    /// abort that `park_abort_origin` declines) still helps.
    pub(super) fn joiner_hands_over(&self) -> bool {
        wait_steps(Some(WaitSpec::Nursery), self.block_ctx()).hand == Handover::Joiner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every context, in the column order of [`TABLE`].
    fn contexts() -> Vec<BlockCtx> {
        vec![BlockCtx::Park, BlockCtx::Reentered]
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

    /// One row per spec, one letter per context: P Park, D Demote, I InPlace. Columns: Park,
    /// Reentered. The last column is `will_return`.
    const TABLE: [(&str, &str, bool); 16] = [
        ("Recv", "PD", false),
        ("Timer", "PD", true),
        ("Send", "PD", false),
        ("Wait", "PD", false),
        ("Wait+send", "PD", false),
        ("Wait+deadline", "PD", true),
        ("Wait+deadline+send", "PD", true),
        ("Sleep", "PD", true),
        ("Offload", "PD", true),
        ("HostFd", "DD", true),
        ("Socket", "PD", true),
        ("Connect", "PI", true),
        ("Guard", "PD", false),
        ("Join", "PD", false),
        ("Nursery", "PD", false),
        ("Extern", "II", true),
    ];

    /// SD4 (TICKET-298): `parks` is true for `Park` alone, and `reentered` for `Reentered` alone.
    #[test]
    fn parks_and_reentered_read_every_context() {
        for c in contexts() {
            let (parks, reentered) = match c {
                BlockCtx::Park => (true, false),
                BlockCtx::Reentered => (false, true),
            };
            assert_eq!(c.parks(), parks, "parks of {c:?}");
            assert_eq!(c.reentered(), reentered, "reentered of {c:?}");
        }
    }

    #[test]
    fn every_cell_matches_the_table() {
        let letter = |m: BlockMode| match m {
            BlockMode::Park => 'P',
            BlockMode::Demote => 'D',
            BlockMode::InPlace => 'I',
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
    /// column order. A cell is the hand-over: K keep, W demote as a waiter, F demote as `inflight`,
    /// J the nursery joiner's hand-over. Every listed spec but `Extern` gives its permit back
    /// ([`Permit::Release`]); `Extern` is [`Permit::Watched`].
    const STEPS: [(&str, &str); 16] = [
        ("Recv", "K W"),
        ("Timer", "K F"),
        ("Send", "K W"),
        ("Wait", "K W"),
        ("Wait+send", "K W"),
        ("Wait+deadline", "K F"),
        ("Wait+deadline+send", "K F"),
        ("Sleep", "K F"),
        ("Offload", "K F"),
        ("HostFd", "F F"),
        ("Socket", "K F"),
        ("Connect", "K K"),
        ("Guard", "K W"),
        ("Join", "K W"),
        ("Nursery", "K J"),
        ("Extern", "K K"),
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
            hand.to_string()
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
            is_panic: false,
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

    #[test]
    fn of_native_maps_each_blocking_kind_to_its_row() {
        assert_eq!(WaitSpec::of_native(Kind::TimedWait), Some(WaitSpec::Sleep));
        assert_eq!(WaitSpec::of_native(Kind::Blocking), Some(WaitSpec::Offload));
        assert_eq!(WaitSpec::of_native(Kind::HostWait), Some(WaitSpec::HostFd));
        assert_eq!(WaitSpec::of_native(Kind::Inline), None);
        assert_eq!(
            WaitSpec::of_native(Kind::InterceptIo(crate::native::IoOp::Create)),
            None
        );
        assert_eq!(
            WaitSpec::of_native(Kind::InterceptNet(crate::native::NetOp::Connect)),
            None
        );
    }
}
