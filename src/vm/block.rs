//! TICKET-181 (wave 16 Family 2, "Blocking contexts") — the ONE answer to "may this op block
//! here, and how?". Every blocking op names its wait as a [`WaitSpec`]; the execution context is
//! derived on every call by [`Vm::block_ctx`] (never cached — its inputs change at many sites);
//! and [`mode`] is the table that joins them. `docs/concurrency.md` "Blocking-context table" is the
//! prose copy of [`mode`], and `tests::every_cell_matches_the_table` pins every cell.
//!
//! The per-op MECHANISM (park on a channel bucket, park on the netpoller, the timer thread) stays
//! with each op; only the decision lives here.

use super::quiesce::PartyWait;
use super::{MnSched, RuntimeError, SchedCore, Span, Vm};
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
    /// The inline outermost-`parallel:` builder (`mn_enlist_sched`), with an eager `Executor` core
    /// (`job`) or without one. It has no worker loop to drive a park.
    Builder { job: bool },
    /// An eager `Executor` job on a `vm::pool` thread.
    PoolJob { judged: bool, reentered: bool },
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
        matches!(
            self,
            BlockCtx::OwnThread { judged: true, .. } | BlockCtx::PoolJob { judged: true, .. }
        )
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
            | WaitSpec::Connect
            | WaitSpec::Join => true,
            WaitSpec::Wait { deadline, .. } => deadline,
            WaitSpec::Recv | WaitSpec::Send | WaitSpec::Guard | WaitSpec::Nursery => false,
        }
    }

    /// The row of a blocking native, by its `Kind`; `None` for a native that does not block.
    pub(super) fn of_native(kind: Kind) -> Option<WaitSpec> {
        match kind {
            Kind::TimedWait => Some(WaitSpec::Sleep),
            Kind::Blocking => Some(WaitSpec::Offload),
            Kind::HostWait => Some(WaitSpec::Stdin),
            Kind::Inline | Kind::InterceptIo | Kind::InterceptNet => None,
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
        BlockCtx::Builder { job } => match spec {
            W::Recv
            | W::Send
            | W::Wait {
                deadline: false, ..
            } => {
                if job {
                    InPlace
                } else {
                    Refuse
                }
            }
            W::Wait { deadline: true, .. } => {
                if job {
                    InPlace
                } else {
                    InlineSleep
                }
            }
            W::Timer | W::Sleep => InlineSleep,
            W::Stdin | W::Guard => Demote,
            W::Socket => Refuse,
            W::Connect => {
                if job {
                    Refuse
                } else {
                    InPlace
                }
            }
            W::Offload | W::Join | W::Nursery => InPlace,
        },
        BlockCtx::PoolJob { .. } => match spec {
            W::Timer | W::Sleep => InlineSleep,
            W::Stdin | W::Guard => Demote,
            // An Executor job does not own its thread: a socket op returns its `Err`.
            W::Socket | W::Connect => Refuse,
            W::Recv | W::Send | W::Wait { .. } | W::Offload | W::Join | W::Nursery => InPlace,
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

/// One blocked waiter in `SchedCore::waiters`, the registry the deadlock verdict asks.
pub(super) struct Waiter {
    /// what it waits for
    pub(super) wait: Arc<PartyWait>,
    /// the cancel flags it would honour (empty where a cancel cannot wake it, e.g. inside its own
    /// uncancellable `defer`)
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
                    cancel: self.demote_cancel_flags(),
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
    /// `eager_core`, `native_reentry` and `deferring` on every call.
    pub(super) fn block_ctx(&self) -> BlockCtx {
        let judged = self.native_reentry == self.deferring;
        let reentered = self.native_reentry > 0;
        if self.mn.is_some() {
            // A worker shell never carries an eager core: `exec.rs` inits it `None` and only a job
            // worker sets it.
            debug_assert!(self.eager_core.is_none());
            return if reentered {
                BlockCtx::Demote
            } else {
                BlockCtx::Park
            };
        }
        if self.mn_enlist_sched.is_some() {
            return BlockCtx::Builder {
                job: self.eager_core.is_some(),
            };
        }
        if self.eager_core.is_some() {
            return BlockCtx::PoolJob { judged, reentered };
        }
        BlockCtx::OwnThread { judged, reentered }
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
        let mut v = vec![
            BlockCtx::Park,
            BlockCtx::Demote,
            BlockCtx::Builder { job: false },
            BlockCtx::Builder { job: true },
        ];
        for judged in [false, true] {
            for reentered in [false, true] {
                v.push(BlockCtx::PoolJob { judged, reentered });
            }
        }
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
    /// R Refuse. Columns: Park, Demote, Builder{job: false}, Builder{job: true}, then PoolJob and
    /// OwnThread each as (judged, reentered) = (f,f) (f,t) (t,f) (t,t). The last column is
    /// `will_return`.
    const TABLE: [(&str, &str, bool); 15] = [
        ("Recv", "PDRI IIII IIII", false),
        ("Timer", "PDSS SSSS SSSS", true),
        ("Send", "PDRI IIII IIII", false),
        ("Wait", "PDRI IIII IIII", false),
        ("Wait+send", "PDRI IIII IIII", false),
        ("Wait+deadline", "PDSI IIII IIII", true),
        ("Wait+deadline+send", "PDSI IIII IIII", true),
        ("Sleep", "PDSS SSSS SSSS", true),
        ("Offload", "PDII IIII IIII", true),
        ("Stdin", "DDDD DDDD DDDD", true),
        ("Socket", "PDRR RRRR IIII", true),
        ("Connect", "PIIR RRRR IIII", true),
        ("Guard", "DDDD DDDD DDDD", false),
        ("Join", "DDII IIII IIII", true),
        ("Nursery", "PIII IIII IIII", false),
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
