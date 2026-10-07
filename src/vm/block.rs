//! TICKET-181 (wave 16 Family 2, "Blocking contexts") — the ONE answer to "may this op block
//! here, and how?". Every blocking op names its wait as a [`WaitSpec`]; the execution context is
//! derived on every call by [`Vm::block_ctx`] (never cached — its inputs change at many sites);
//! and [`mode`] is the table that joins them. `docs/concurrency.md` "Blocking-context table" is the
//! prose copy of [`mode`], and `tests::every_cell_matches_the_table` pins every cell.
//!
//! The per-op MECHANISM (park on a channel bucket, park on the netpoller, the timer thread) stays
//! with each op; only the decision lives here.

use super::quiesce::PartyWait;
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
    Builder,
    /// A thread that owns itself: `main`, a `main` `defer:`, `main` inside a callback.
    OwnThread { judged: bool, reentered: bool },
}

impl BlockCtx {
    /// May the process-wide deadlock verdict JUDGE this party (DEC-136)?
    ///
    /// `quiesce`'s `live` count is `1 (main) + Σ outstanding` over the run's executors, so exactly
    /// two kinds of thread are counted: `main` and an eager `Executor` job — the two with no
    /// scheduler of any kind under them — and only while every native re-entry is a `defer` drain
    /// (`native_reentry == deferring`; a `defer` body is VM code on the same thread). A worker
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
    /// a stdin read (`Kind::HostWait`)
    Stdin,
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
            | WaitSpec::Stdin
            | WaitSpec::Socket
            | WaitSpec::Connect => true,
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
            Kind::HostWait => Some(WaitSpec::Stdin),
            Kind::Inline | Kind::InterceptIo | Kind::InterceptNet | Kind::InterceptAirlock => None,
        }
    }
}

/// How an op blocks in a context.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum BlockMode {
    /// snapshot-park the fiber on the op's own wait bucket
    Park,
    /// block this worker in place and hand its runner slot to a replacement
    Demote,
    /// block this thread in place (it runs nothing else)
    InPlace,
    /// sleep this thread to the op's own deadline
    InlineSleep,
    /// refuse: the op returns its documented error or fault instead of blocking
    Refuse,
}

/// THE blocking-context table. Every blocking op asks this, and nothing else, how to block.
/// Changing a cell means changing this, `tests::every_cell_matches_the_table` and the
/// `docs/concurrency.md` "Blocking-context table" in one commit.
pub(super) fn mode(ctx: BlockCtx, spec: WaitSpec) -> BlockMode {
    use BlockMode::{Demote, InPlace, InlineSleep, Park, Refuse};
    use WaitSpec as W;
    match ctx {
        BlockCtx::Park => match spec {
            // DEC-151: stdin is a host wait we do not own; DEC-016: a guard waits in place, then
            // demotes; changed cell (c), X1: an Executor join cannot snapshot-park (the join loop
            // holds its state on the host stack), so it hands its runner slot over.
            W::Stdin | W::Guard | W::Join => Demote,
            _ => Park,
        },
        BlockCtx::Demote => match spec {
            W::Connect | W::Nursery => InPlace,
            // TICKET-185: a demoted sender blocks on its published offer, as Go blocks.
            W::Send
            | W::Recv
            | W::Timer
            | W::Wait { .. }
            | W::Sleep
            | W::Offload
            | W::Join
            | W::Stdin
            | W::Socket
            | W::Guard => Demote,
        },
        BlockCtx::Builder => match spec {
            W::Recv
            | W::Send
            | W::Wait {
                deadline: false, ..
            }
            | W::Socket => Refuse,
            W::Wait { deadline: true, .. } | W::Timer | W::Sleep => InlineSleep,
            W::Stdin | W::Guard => Demote,
            W::Connect | W::Offload | W::Join | W::Nursery => InPlace,
        },
        BlockCtx::OwnThread { .. } => match spec {
            W::Timer | W::Sleep => InlineSleep,
            W::Stdin | W::Guard => Demote,
            W::Recv
            | W::Send
            | W::Wait { .. }
            | W::Offload
            | W::Socket
            | W::Connect
            | W::Join
            | W::Nursery => InPlace,
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
    /// TICKET-208 — the run-wide job fault was delivered to this party: a fire-and-forget
    /// `Executor` job faulted, which ends the run. No `recover:` catches it (Go: a goroutine's
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
    /// the nursery's cancel flag, which a faulting child trips before `finish` records the fault
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
/// tripped AND whose family recorded a `Fault`. A flag is only a wake hint: a child trips it just
/// before `finish` records the fault, so a tripped flag with no recorded fault is not a halt.
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
/// hold as the pop or settle that ends it — DEC-176) or [`Vm::block_exit`] (a `will_return` wait).
pub(super) struct DemoteReg {
    /// the `SchedCore::waiters` token, or `None` for an `inflight` wait
    tok: Option<u64>,
}

impl DemoteReg {
    /// End the bracket under core lock A: back onto `running`, and un-account the wait.
    pub(super) fn release(self, sched: &MnSched, c: &mut SchedCore) {
        c.running += 1;
        match self.tok {
            Some(tok) => c.unregister_waiter(tok),
            None => {
                sched.inflight.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

impl Vm {
    /// THE demote bracket: take this worker off `running` and account the wait by its spec — a
    /// `will_return` wait is `inflight` (it vetoes the verdict), every other one registers a
    /// [`Waiter`] on `wait` the verdict asks "can you still be satisfied?". Spawns this thread's
    /// replacement worker once. With no M:N scheduler (`mn == None`) it keeps DEC-052's
    /// `yield_pool_slot(None)` and accounts nothing.
    pub(super) fn block_enter(
        &mut self,
        spec: WaitSpec,
        wait: Option<PartyWait>,
        what: &str,
        span: Span,
    ) -> Result<DemoteReg, RuntimeError> {
        debug_assert_eq!(spec.will_return(), wait.is_none());
        let Some(sched) = self.mn.as_ref().map(Arc::clone) else {
            self.yield_pool_slot(None);
            return Ok(DemoteReg { tok: None });
        };
        let reg = {
            let mut c = sched.lock();
            c.running -= 1;
            let tok = match wait {
                Some(w) => Some(c.register_waiter(Waiter {
                    wait: Arc::new(w),
                    cancel: self.wake_set().flags(),
                    fiber: true,
                })),
                None => {
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
        if !self.demoted {
            if !self.spawn_replacement_worker(&sched, self.wid) {
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
            self.demoted = true;
        }
        Ok(reg)
    }

    /// End a `will_return` bracket: takes core lock A itself. With no M:N scheduler there is
    /// nothing to undo, exactly as before (DEC-052).
    pub(super) fn block_exit(&mut self, reg: DemoteReg) {
        debug_assert!(
            reg.tok.is_none(),
            "a waiter-registered wait releases under its own pop"
        );
        if let Some(sched) = self.mn.as_ref().map(Arc::clone) {
            reg.release(&sched, &mut sched.lock());
        }
    }

    /// The blocking context of the running code, derived from `mn`, `mn_enlist_sched`,
    /// `native_reentry` and `deferring` on every call. An `Executor` job is a fiber (TICKET-208).
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
            return BlockCtx::Builder;
        }
        BlockCtx::OwnThread { judged, reentered }
    }

    /// TICKET-194 — THE cancellation check an op makes before it waits. An op calls it only after
    /// its own ready check failed: a send with room, a recv with a value ready, a ready `wait:` arm
    /// or a free update guard completes and is never cut here (owner decision 1,
    /// `docs/root-causes-w18.md`). `native_reentry == 0` gates it, as it gates the park: inside a
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every context, in the column order of [`TABLE`].
    fn contexts() -> Vec<BlockCtx> {
        let mut v = vec![BlockCtx::Park, BlockCtx::Demote, BlockCtx::Builder];
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
            WaitSpec::Stdin,
            WaitSpec::Socket,
            WaitSpec::Connect,
            WaitSpec::Guard,
            WaitSpec::Join,
            WaitSpec::Nursery,
        ]);
        v
    }

    /// One row per spec, one letter per context: P Park, D Demote, I InPlace, S InlineSleep,
    /// R Refuse. Columns: Park, Demote, Builder, then OwnThread as (judged, reentered) =
    /// (f,f) (f,t) (t,f) (t,t). The last column is `will_return`.
    const TABLE: [(&str, &str, bool); 15] = [
        ("Recv", "PDR IIII", false),
        ("Timer", "PDS SSSS", true),
        ("Send", "PDR IIII", false),
        ("Wait", "PDR IIII", false),
        ("Wait+send", "PDR IIII", false),
        ("Wait+deadline", "PDS IIII", true),
        ("Wait+deadline+send", "PDS IIII", true),
        ("Sleep", "PDS SSSS", true),
        ("Offload", "PDI IIII", true),
        ("Stdin", "DDD DDDD", true),
        ("Socket", "PDR IIII", true),
        ("Connect", "PII IIII", true),
        ("Guard", "DDD DDDD", false),
        ("Join", "DDI IIII", false),
        ("Nursery", "PII IIII", false),
    ];

    #[test]
    fn every_cell_matches_the_table() {
        let letter = |m: BlockMode| match m {
            BlockMode::Park => 'P',
            BlockMode::Demote => 'D',
            BlockMode::InPlace => 'I',
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
        // Gotcha 2: the child trips its flag before `finish` records the fault.
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
        assert_eq!(WaitSpec::of_native(Kind::HostWait), Some(WaitSpec::Stdin));
        assert_eq!(WaitSpec::of_native(Kind::Inline), None);
        assert_eq!(WaitSpec::of_native(Kind::InterceptIo), None);
        assert_eq!(WaitSpec::of_native(Kind::InterceptNet), None);
    }
}
