//! VM: scheduler data types: joiner steps, fd waits, fibers, local queues, eager and join scopes,
//! the scope table, op step and park records.
//! Split out of `src/vm/mod.rs`. `super::*` is the parent module.

use super::*;

/// TICKET-118 (W13-7) — what a marked pool-slot joiner does on its next wait/park pass:
/// `Untimed` (not the marked joiner, or its sched still has work), `Timed` (marked and idle, but
/// under one `DEMOTE_POLL_BACKOFF` tick so far), or `Yield` (marked and idle for a full tick — hand
/// the pool slot over).
pub(super) enum JoinerStep {
    Untimed,
    Timed,
    Yield,
}

/// TICKET-118 (W13-7) — while held, `sched.core.pool_joiner` names this OS thread as the one
/// eligible to hand its bounded-pool slot to a replacement while it waits idle on this sched's
/// nursery join (DEC-052: only the top-level `Vm`, `mn.is_none()`, ever installs this). Restores
/// the previous marker (usually `None`) on drop, so a nested join's guard does not clobber an outer
/// one — though only the outermost `mn.is_none()` thread ever installs one in practice.
pub(super) struct PoolJoinerGuard {
    pub(super) sched: Arc<MnSched>,
    pub(super) prev: Option<std::thread::ThreadId>,
}

impl Drop for PoolJoinerGuard {
    fn drop(&mut self) {
        self.sched.lock().pool_joiner = self.prev;
    }
}

/// D5 owe #3 Path C (#3 socket half) — block the (replacement-covered) worker on `fd` until it is
/// readable/writable per `interest` OR `timeout` elapses, then return. Used by [`Vm::demote_block_socket`]
/// so an in-callback socket op that can't snapshot-park onto the netpoller still waits in the KERNEL
/// (woken immediately on readiness, no busy-poll) instead of pinning a CPU. The `timeout` bounds how
/// fast the caller re-observes `cancel`/`terminate`. Spurious/early returns are harmless — the caller
/// simply re-attempts the non-blocking op and re-blocks here on `WouldBlock`. On non-Unix (no `poll(2)`)
/// it degrades to a plain sleep (the same bounded backoff the netpoller already targets Unix-only).
#[cfg(unix)]
pub(super) fn wait_fd_ready(
    fd: std::os::fd::RawFd,
    interest: poller::Interest,
    timeout: std::time::Duration,
) {
    let events = match interest {
        poller::Interest::Read => libc::POLLIN,
        poller::Interest::Write => libc::POLLOUT,
    };
    let mut pfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // Ignore the result: a ready fd, a timeout, or an EINTR all lead to the same next step (re-attempt
    // the non-blocking op under the caller's lock). Never blocks longer than `ms`.
    unsafe {
        libc::poll(&mut pfd as *mut libc::pollfd, 1, ms);
    }
}

#[cfg(not(unix))]
pub(super) fn wait_fd_ready(
    _fd: std::os::fd::RawFd,
    _interest: poller::Interest,
    timeout: std::time::Duration,
) {
    std::thread::sleep(timeout);
}

/// A fiber's saved execution context (B1): every `Vm` field that `run_until` reads or writes keyed by
/// per-execution indices. Swapped with the live `Vm` fields ([`Vm::swap_ctx`]) when a fiber is
/// scheduled in or out. `pending_exit` is deliberately NOT here — `std.os.exit` halts the whole
/// program, so it stays VM-global.
#[derive(Default)]
pub(super) struct FiberCtx {
    pub(super) frames: Vec<CallFrame>,
    pub(super) stack: Vec<Value>,
    pub(super) call_depth: usize,
    pub(super) cur_base: usize,
    pub(super) handlers: Vec<Handler>,
    pub(super) nurseries: Vec<Vec<QueuedTask>>,
    /// Cross-nursery flat scheduler (M:N) — parallel to [`Vm::nurseries`] (lockstep): `Some(scope_id)`
    /// if this nursery was EARLY-ENLISTED into the one global sched (its sibling tasks already seeded as
    /// a scope so a *nested* nursery's owner can run them — the case-A cross-nursery wake), else `None`
    /// (the normal lazy path: tasks run + reduce at this nursery's own `JoinNursery`). When `Some`, the
    /// nursery's `tasks` vec was drained (consumed into the scope), and its `JoinNursery` reduces the
    /// recorded scope's slot sub-range instead of running the tasks — preserving the per-nursery-join
    /// flush ORDER (so non-blocking nested spawns still flush deterministically).
    pub(super) mn_scopes: Vec<Option<usize>>,
    /// TASK B — see [`Vm::nursery_defer_floors`]; carried per-fiber so a parked fiber's per-nursery
    /// defer floors travel with its `nurseries` across `swap_ctx`.
    pub(super) nursery_defer_floors: Vec<usize>,
    /// TICKET-048 — see [`Vm::nursery_spans`]; carried per-fiber so a parked fiber's spans travel
    /// with its nurseries across `swap_ctx`.
    pub(super) nursery_spans: Vec<Span>,
    /// Per-connection spawn — parallel to [`Vm::nurseries`] (same length, pushed/popped in lockstep):
    /// `Some` for an EAGER nursery (one entered under `--parallel` inside a live fiber, `mn.is_some()`)
    /// holding the live inner [`MnSched`] a `spawn` in this body injects handlers into; `None` for a
    /// lazy/top-level nursery (queue-at-join). Carried per-fiber so the open scope travels across a
    /// park (the acceptor blocks on `accept` mid-loop).
    pub(super) eager_scheds: Vec<Option<EagerScope>>,
    pub(super) fault_trace: Option<Vec<TraceFrame>>,
    pub(super) fault_trace_depth: usize,
    /// The task's guard identity ([`Vm::guard_token`]); 0 until its first swap-in.
    pub(super) guard_token: u64,
    /// See [`Vm::held_reads`]: the read guards the task holds, carried across a park.
    pub(super) held_reads: Vec<usize>,
    /// See [`Vm::held_guards`]: the guards the task's box-closure frames hold. Dropping a context
    /// releases them.
    pub(super) held_guards: Vec<super::core::UpdateGuard>,
    /// See [`Vm::held_gens`]: the holds the task's generator body frames keep. Dropping a context
    /// puts those generators back `Done`.
    pub(super) held_gens: Vec<super::core::GenHold>,
    /// See [`Vm::drains`]: the task's live defer drains, carried across a park.
    pub(super) drains: Vec<super::Drain>,
    /// M1.6 — this context owns its own per-fiber side state (`executors`, `str_intern` and the
    /// in-flight socket-op state below), so [`Vm::swap_ctx`] swaps it. `false`
    /// for a context that aliases the shell's (tests only: every fiber and box context owns its
    /// side state).
    pub(super) owns_side: bool,
    /// D2b — per-task output buffers (Decision F: each task's stdout/stderr flushes in task order at
    /// join, never interleaved live). A task owns its output stream, so only a task switch
    /// ([`Vm::swap_task`]) moves these. A box context never owns one: a `print` inside a box call
    /// lands in its caller's stream, so a box context's buffers stay empty.
    pub(super) out: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    /// This task's place in the spawn tree ([`Vm::task`]); a task switch moves it with the
    /// output stream.
    pub(super) task: super::deadlock::TaskPath,
    /// M3.1 — the run's one module table (the same `Arc` every context holds), so the parked
    /// context's roots reach the run's modules. Never swapped.
    pub(super) module_objs: Arc<super::module_table::ModuleTable>,
    /// D2b — the fiber's `Executor` handles (GC roots). Per fiber: swapped by
    /// [`Vm::swap_ctx`] when `owns_side`. Empty for a context that does not own side state.
    pub(super) executors: Vec<GcRef>,
    /// M19 Phase 3 — the fiber's `ConstStr` intern cache (GC roots). Per fiber:
    /// swapped by [`Vm::swap_ctx`] when `owns_side`. Empty for a context that does not own side
    /// state (it aliases the shell's cache).
    pub(super) str_intern: fxhash::FxHashMap<usize, GcRef>,
    /// D6b — a non-blocking `connect` parked on writability (see [`ConnectInProgress`]). Non-heap, so
    /// it carries no `GcRef` and needs no GC rooting; but it MUST travel with the fiber across the
    /// park, so it swaps in [`Vm::swap_ctx`] like the other per-fiber state. `None` unless this fiber
    /// is mid-connect (only ever set on the M:N engine).
    pub(super) pending_connect: Option<ConnectInProgress>,
    /// D6c — per-socket read/accept/write timeout marker. A socket op given a `timeout_ms` parks on
    /// the netpoller with a deadline; if that deadline elapses before the fd fires, the poll thread
    /// (which owns the detached [`Fiber`]) sets this `true` and re-injects the fiber — exactly like
    /// `pending_connect`, this travels WITH the fiber across [`Vm::swap_ctx`] so the resumed op knows
    /// the wake came from a timeout, not readiness. On schedule-in it swaps into [`Vm::poll_timed_out`];
    /// the rewound socket op checks it at ENTRY (before re-running the syscall) and returns
    /// `Err("timeout")` instead of retrying. `false` whenever no timeout wake is pending. M:N-only.
    /// M1.4 — atomic, so the poll thread (not attached to the GC) sets it through `&FiberCtx` and
    /// never holds a `&mut` to a registered context (see `RootRegistry::roots`).
    pub(super) poll_timed_out: AtomicBool,
    /// B1 — the absolute deadline of the `Socket.read` in flight on this fiber (its `timeout_ms`,
    /// latched at the first park). Travels WITH the fiber across [`Vm::swap_ctx`] like `poll_timed_out`,
    /// because the whole read op re-executes on every wake: without the latch each park would recompute
    /// `now + timeout_ms` and restart the budget. Cleared when the read returns. `None` = no read in
    /// flight. M:N-only.
    pub(super) poll_deadline: Option<std::time::Instant>,
    /// N3(a) — `Some(owed)` iff the in-flight str `read` took a partial codepoint off the fd (carried,
    /// `owed` bytes). Travels WITH the fiber across [`Vm::swap_ctx`] like `poll_deadline`, so the
    /// netpoller-park re-entry knows the taken-partial state after the op re-executes and reports
    /// `incomplete utf-8` rather than `timeout`. Cleared when the read returns. `None` = no partial.
    /// M:N-only.
    pub(super) poll_partial: Option<usize>,
    /// W15-3 — bytes of the in-flight `write` already sent; survives a park's ip-rewind re-run,
    /// cleared by `drop_poll_latch`.
    pub(super) poll_written: Option<usize>,
    /// M1.4 — a spawned, not-yet-started fiber's task (callee + args), launched by `start_task` on
    /// its first schedule. It lives in the context, not in [`FiberState`], so the context alone
    /// roots it. `None` once started, and for every non-fiber context. NOT moved by `swap_ctx`.
    pub(super) start: Option<PendingCall>,
}

/// Scheduling state of a fiber on the M:N scheduler.
pub(super) enum FiberState {
    /// Spawned but not yet started; its task waits in [`FiberCtx::start`].
    Pending,
    /// Started and runnable — resume by re-entering its `run_until`.
    Ready,
    /// Parked on an empty channel; runnable again once a sibling `send`s (the receiver handle stays
    /// rooted on the fiber's own operand stack, so this variant carries no payload).
    Blocked,
}

/// One child fiber: its saved context plus scheduling state. While the fiber is the one actively
/// running, its context lives in the live `Vm` fields and `ctx` is empty (see the scheduler).
pub(super) struct Fiber {
    /// M1.4 — boxed, so it never moves and the run's [`RootRegistry`] can list it.
    pub(super) ctx: CtxBox,
    pub(super) state: FiberState,
    /// D2b — the fiber's stable Decision-F outcome slot. Under the cross-nursery flat scheduler this is
    /// the GLOBAL flat index into `SchedCore::slots` (= `scopes[scope_id].base_index + local_i`),
    /// assigned at nursery build / `inject`.
    pub(super) task_index: usize,
    /// Cross-nursery flat scheduler (M:N) — which nursery scope this fiber belongs to (indexes
    /// `SchedCore::scopes`). Independent of `task_index` (the flat slot). Drives the scope-scoped owner
    /// stop, per-scope done accounting, and per-scope cancel. Zero for the single-nursery fast
    /// path's sole scope.
    pub(super) scope_id: usize,
    /// D2b — the spawning task's span, for fault/panic attribution when this fiber faults under M:N.
    pub(super) span: Span,
    /// D5 — a blocking native call offloaded to the dirty pool stashes its result here; the worker
    /// that resumes this fiber lowers the `NativeRet` into the heap + pushes it (or re-raises the
    /// `RuntimeError`) before continuing past the `Call`. `None` except in the brief window between a
    /// blocking-pool completion and the fiber's next schedule-in.
    pub(super) resume_native: Option<Result<crate::native::NativeRet, RuntimeError>>,
    /// TICKET-185 — this fiber's carried [`Vm::pending`] while it is not the running fiber. A
    /// parked `recv` holds its channel here even without a slot. Dropping a parked fiber settles
    /// it, so none of its entries outlive it.
    pub(super) pending: Option<crate::vm::core::PendingOp>,
    /// TICKET-208 — the site this fiber parked at ([`Vm::park_site`]); `None` while it runs on.
    /// A deadlocked `Executor` job is reported there (see [`SchedCore::record_deadlocked`]).
    pub(super) park_site: Option<(&'static str, Span)>,
    /// TICKET-286 — the `--timeout` deadline ([`Vm::deadline`]) this fiber parked under; `None`
    /// with the cap off. Every park gap re-check reads it, and [`MnSched::deadline_drain`] requeues
    /// the parked fiber once it passes.
    pub(super) park_deadline: Option<std::time::Instant>,
    /// TICKET-287 — this fiber's carried [`Vm::fault_park`] while it is parked mid fault-unwind.
    pub(super) fault_park: Option<super::frames::FaultPark>,
    /// TICKET-289 — an Executor job's own cancel flag ([`JobSettle::flag`]); `None` for every
    /// other fiber. A running job reads it in place of its scope's shared flag.
    pub(super) job_cancel: Option<Arc<AtomicBool>>,
    /// The verdict cut this fiber's wait inside a `defer` it parked in ([`SchedCore::cut_parked`]);
    /// the resume ends the wait with the cut (`Vm::take_verdict_cut`).
    pub(super) defer_cut: Option<VerdictCut>,
    /// This fiber's [`Vm::cut`] and `cancel_unwind_faulted` while it is not the running fiber:
    /// saved at every leave of `run_one_fiber` and taken back at the resume.
    pub(super) cut: Option<super::block::Cut>,
    pub(super) cancel_unwind_faulted: bool,
}

/// How a verdict cut a fiber parked in a `defer`, as it cuts a demoted cleanup's in-place wait
/// ([`Vm::in_place_cut`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum VerdictCut {
    /// The sched-wide verdict ([`SchedCore::flag_deadlock`], a joined leaf): the wait ends with
    /// the verdict's fault, which ranks below a cause the task already holds.
    Sched,
    /// An aborting family's cut ([`SchedCore::cut_aborting_families`]): the scope's cancel, so the
    /// task unwinds as `Cancelled` unless it holds a cut already.
    Family,
}

impl Fiber {
    /// TICKET-286 — has the `--timeout` deadline this fiber parked under passed?
    pub(super) fn deadline_due(&self) -> bool {
        self.park_deadline
            .is_some_and(|dl| std::time::Instant::now() >= dl)
    }

    /// TICKET-188 — is a flag of a nursery this parked fiber OWNS tripped? Its wake set's owned half,
    /// read by every park gap re-check; `halt_of` decides on resume whether it is a halt.
    pub(super) fn owned_tripped(&self) -> bool {
        open_nurseries(&self.ctx.eager_scheds).any(|o| o.flag.load(Ordering::Relaxed))
    }

    /// Does a cancel reach this parked fiber at all? Not while it runs a `defer` on frames (its
    /// `drains`): a `defer` is never cancelled ([`Vm::in_defer`]), so a cancel would only requeue
    /// it to park again. THE gate of every park gap re-check and every cancel drain matcher.
    pub(super) fn honours_cancel(&self) -> bool {
        self.ctx.drains.is_empty()
    }

    /// The park gap re-check's cancel half: `scope_tripped` (this fiber's scope cancel, or an
    /// ancestor's) or a nursery this fiber owns, for a fiber that honours a cancel.
    pub(super) fn cancel_wakes(&self, scope_tripped: bool) -> bool {
        self.honours_cancel() && (scope_tripped || self.owned_tripped())
    }

    /// TICKET-188 — does a cancel of scope `family` wake this parked fiber? It is a member of the
    /// family, or it owns one of the family's scopes (its child faulted), and it honours a cancel.
    /// Every per-fiber drain matcher reads this.
    pub(super) fn woken_by(&self, family: &[usize]) -> bool {
        self.honours_cancel()
            && (family.contains(&self.scope_id)
                || open_nurseries(&self.ctx.eager_scheds).any(|o| {
                    family.contains(&o.scope) || o.more.iter().any(|s| family.contains(s))
                }))
    }
}

/// TICKET-188 — THE walk of a party's open nurseries, outermost first: `n` is the `nurseries`
/// index (`eager_scheds` is lockstep with `nurseries`, DEC-048), `None` entries skipped.
pub(super) fn open_nurseries(
    eager: &[Option<EagerScope>],
) -> impl DoubleEndedIterator<Item = block::OwnedRef<'_>> {
    eager.iter().enumerate().filter_map(|(n, s)| {
        let s = s.as_ref()?;
        Some(block::OwnedRef {
            n,
            flag: &s.cancel,
            sched: &s.sched,
            scope: s.scope,
            more: &s.more_scopes,
        })
    })
}

// D2b parks fibers across worker threads (parked on one worker, requeued by a `send` on another,
// resumed on a third). That requires `Fiber: Send`; the guard makes a future non-`Send` field on
// `Fiber`/`FiberCtx` a loud compile error, not a D2b surprise.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Fiber>();
};

/// A snapshot taken at a `recover:` boundary (`Op::PushHandler`). On a caught fault the VM restores
/// the operand stack, call frames, and call-depth to these values, then jumps to `ip` in the
/// boundary's frame with the fault message pushed as the operand.
#[derive(Clone, Copy, Debug)]
pub(super) struct Handler {
    pub(super) stack_len: usize,
    pub(super) frame_len: usize,
    pub(super) call_depth: usize,
    pub(super) ip: usize,
    /// `deferred.len()` of the boundary's frame when the handler was installed. A caught fault, a
    /// `?` short-circuit, or the Ok path drains the frame's defers down to this marker — the
    /// recover block's own cleanup — before binding the recovered value.
    pub(super) defer_len: usize,
    /// `defer_markers.len()` of the boundary's frame at install. A fault / `?` short-circuit jumps
    /// past the `LeaveDeferScope`s of any defer scopes opened *inside* the recover block, so their
    /// markers would leak; the catch paths truncate `defer_markers` back to this length.
    pub(super) markers_len: usize,
    /// `nurseries.len()` at install. A fault inside a `parallel:` body or a spawned task jumps past
    /// the `JoinNursery` that would pop the nursery; the catch path truncates `nurseries` back to
    /// this length so the stale nursery (and its aborted siblings) is reclaimed.
    pub(super) nursery_len: usize,
}

/// B3.2 — what a finished task hands back to its join: the worker's captured stdout/stderr
/// (decision F — buffer-per-worker, returned to the parent rather than interleaved live), flushed
/// at the join. The join discards each task's return value (data exits a spawn via
/// `Shared`/`Channel`, and an Executor job's value via its Task channel), so none is kept.
#[derive(Debug)]
pub(super) struct WorkerResult {
    pub(super) out: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

/// B3.4 — how a `--parallel` task ended, recorded in its slot. The join ([`Vm::reduce_task_slots`])
/// scans these in task order: `Done`/`Exit` flush their buffered output; the lowest-index `Exit` or
/// `Fault` propagates (an `Exit` hard-halts the parent, a `Fault` unwinds normally so an outer
/// `recover:` can catch it); `Cancelled` is swallowed (a sibling-abort, its partial output dropped).
/// The terminal (lowest-index propagating) `Fault` ALSO flushes its buffered output at its task-order
/// slot instead of dropping it, so a faulting task's already-printed partial output survives.
/// Higher-index racy faults and `Cancelled` still drop (no deterministic slot).
#[derive(Debug)]
pub(super) enum TaskOutcome {
    /// Ran to completion; output flushed in task order.
    Done(WorkerResult),
    /// Observed the nursery cancel flag and unwound (a sibling faulted/exited first). Its buffered
    /// output is FLUSHED at its task-order slot, not dropped: with cancellation points a started task
    /// always runs its prologue, so those bytes really were printed, and dropping them here would
    /// silently un-print output the program genuinely produced.
    Cancelled { out: Vec<u8>, stderr: Vec<u8> },
    /// TICKET-147 (W14-12) — a CANCELLED task whose own unwind faulted: a `defer` of the cancelled
    /// task, or a nested nursery it aborted while unwinding, raised a real fault in place of the
    /// `cancelled` sentinel. It is a fault of the program, so `reduce_task_slots` propagates it — but
    /// ranked BELOW every ordinary `Fault` (the cancel's root cause is the more useful report;
    /// asyncio `TaskGroup` lists `['boom', 'cleanup failed']`) and above a synthesized `Deadlocked`.
    /// It does not trip the scope cancel (the scope is already cancelled) and is not a scope fault.
    CancelledFault {
        err: RuntimeError,
        out: Vec<u8>,
        stderr: Vec<u8>,
    },
    /// Called `std.os.exit(code)`. Buffered output is flushed, then the parent hard-halts with `code`.
    Exit {
        code: i32,
        out: Vec<u8>,
        stderr: Vec<u8>,
    },
    /// Faulted (runtime error or caught panic). The lowest-index fault propagates out of the join; its
    /// buffered output is flushed at its task-order slot (a Rust-panic-to-fault path may carry empty
    /// buffers — the shell buffer is not safely reachable there).
    Fault {
        err: RuntimeError,
        out: Vec<u8>,
        stderr: Vec<u8>,
        /// TICKET-195 — the faulting party's own stack trace, so a party that receives this fault
        /// reports the faulting party's frames, never its own (`Vm::adopt_child_fault`).
        trace: Vec<TraceFrame>,
    },
    /// The M:N deadlock detector aborted a nursery: EVERY still-parked fiber was recorded with this
    /// synthetic `DEADLOCK_MSG` outcome (see [`SchedCore::flag_deadlock`]). It is DISTINCT from
    /// `Fault` so `reduce_task_slots` can flush ALL parked buffers in task order (matching the serial
    /// engine, which printed those lines live before the deadlock returned) — a real multi-fault
    /// reduce still flushes only the terminal fault's buffer. Only ever originates in `flag_deadlock`
    /// (M:N); the legacy-pool reduce never produces it. In practice a real fault/exit trips
    /// `terminate` before the deadlock detector fires, so a slot vector is normally all-`Deadlocked`;
    /// but the invariant is NOT relied upon — `reduce_task_slots` applies a strict `Exit` > `Fault` >
    /// `Deadlocked` precedence for the terminal outcome, so a mixed vector (were one to arise under a
    /// race) still resolves deterministically.
    Deadlocked {
        err: RuntimeError,
        out: Vec<u8>,
        stderr: Vec<u8>,
    },
}

impl TaskOutcome {
    /// TICKET-297 — does this outcome ABORT its scope (a `Fault` or an `Exit`)? THE one answer:
    /// `MnSched::finish_with` reads it to trip the fiber's flag and to report `aborts`.
    pub(super) fn aborts(&self) -> bool {
        match self {
            TaskOutcome::Fault { .. } | TaskOutcome::Exit { .. } => true,
            TaskOutcome::Done(_)
            | TaskOutcome::Cancelled { .. }
            | TaskOutcome::CancelledFault { .. }
            | TaskOutcome::Deadlocked { .. } => false,
        }
    }

    /// TICKET-284 — does this outcome hold an `os.exit`'s output? A taker that moves it out of its
    /// slot counts it ([`MnSched::hold_exit_output`]); its receiver ends the count.
    pub(super) fn holds_exit_output(&self) -> bool {
        match self {
            TaskOutcome::Exit { out, stderr, .. } => !out.is_empty() || !stderr.is_empty(),
            TaskOutcome::Done(_)
            | TaskOutcome::Cancelled { .. }
            | TaskOutcome::CancelledFault { .. }
            | TaskOutcome::Fault { .. }
            | TaskOutcome::Deadlocked { .. } => false,
        }
    }
}

// B3.3-threads had a `TaskSlots` alias + a `DoneSignal` completion guard here, both owned by the
// batch farm/join helper `run_workers_on_pool`. Eager `Executor` execution retired that helper (its
// last caller was the `Executor` drain — the legacy `run_parallel_nursery` was already gone, and the
// M:N nursery joins through `MnSched`), so both went with it. The panic-safety invariant they existed
// for is unchanged and now lives inline in `Vm::spawn_into`: a Rust panic in a job's worker VM
// becomes a `Fault` slot instead of leaving `the Executor's sched::outstanding` short and hanging `shutdown`
// forever. Covered by `executor_faulting_job_does_not_hang_shutdown`.

/// TICKET-208 — `Executor.submit` could start no runner thread for its job.
pub(super) const EXEC_NO_RUNNER_MSG: &str =
    "Executor.submit could not start a runner thread (OS thread limit reached)";

/// TICKET-219 — the `Err` a dropped or cut job's Task is sealed with (CPython
/// `shutdown(cancel_futures=True)`).
pub(super) const EXEC_CANCELLED_MSG: &str =
    "task cancelled: cancel() stopped it before it finished";

/// TICKET-289 — the `Err` of a Task whose `submit` came after `shutdown()` or `cancel()`.
pub(super) const EXEC_SHUT_MSG: &str = "submit on a shut-down Executor (it no longer accepts work)";

/// TICKET-289 — the `Err` of a Task whose `submit` came after the creating job was cancelled.
pub(super) const EXEC_CREATOR_CANCELLED_MSG: &str =
    "submit on an Executor whose creating job was cancelled (it no longer accepts work)";

/// D2b — the M:N scheduler shared by every worker enlisted on one `parallel:` nursery (the joining
/// thread + the pool shells it farms). It replaces the legacy `--parallel` "one OS thread per task,
/// block the thread on `recv`" model with **lightweight fibers parked on `recv`** multiplexed over a
/// core-sized pool: an empty `recv` files the running fiber into `parked` (keyed by `ChannelCore`
/// pointer) and the worker grabs the next runnable fiber instead of blocking the thread; a `send`
/// drains the matching bucket back onto the global queue and wakes a worker.
///
/// D4 — the single shared run queue is split into a per-worker local (`locals[wid]`) + a shared
/// `global` overflow queue; a worker drains its own local, batch-grabs from `global`, then steals
/// from a sibling. A single `Mutex<SchedCore>` still guards `global`, the park set, the per-task
/// outcome slots, and the scheduling counts together; per-worker locals have their own locks (class
/// **B**, taken alone). The core lock is NEVER held across a `swap_ctx` / `run_until`: the worker
/// loop takes a fiber out, releases the lock, runs the fiber, then re-locks to settle. `cv` is the
/// wait point — a worker with `runnable == 0` parks here (a true `cv.wait`, D4e) until a
/// `send`/finish/terminate `notify`; with `runnable > 0` (work in a local) it spins-and-resteals
/// instead of sleeping.
///
/// **Deadlock** (the M:N redefinition of B3.5): `running == 0 && runnable == 0 && parked > 0 &&
/// done < total` — every not-done fiber is parked on some channel and none is running or queued
/// anywhere (the `runnable` atomic counts all locals + `global`), so no `send` can ever come. It is
/// an exact predicate under the single coordinator, so the legacy
/// barrier-confirm epoch dance ([`DeadlockWatch`]) is unnecessary here. For D2b the run queue is one
/// shared `VecDeque` (per-worker rings + work-stealing + a targeted-wake StoreLoad barrier are D4).
/// D4b — a per-worker run queue (Go's P-local queue): a single `runnext` slot (reserved for the
/// ping-pong-locality optimization) plus a FIFO `ring` of runnable fibers. Each worker owns one
/// behind its own `Mutex` (lock class **B**), so the common case — a worker draining its own queue —
/// never touches the global core lock. In D4c a local is populated only by the owning worker's
/// batch-grab from the global queue and by work it steals, and a worker grabs again only once its
/// local is empty, so a local never accumulates across grabs: it holds at most one grab-batch
/// (`≤ LOCAL_RING_CAP/2`) or steal-haul at a time (no hard cap or spill is needed). `runnext` holds
/// a rendezvous hand-off (`hand_off`, `handoff_wake`; DEC-128). TICKET-199 — a `runnext` pick
/// inherits the current slice (Go `inheritTime`, [`Slice`]), so a hand-off pair cannot starve the
/// ring: see [`LocalQ::pop`].
pub(super) struct LocalQ {
    pub(super) runnext: Option<Fiber>,
    /// TICKET-128 (W13-25) — when `runnext` was filed by a handoff wake. `None` when `runnext` is
    /// `None` or was populated some other way. `try_steal` won't take a `runnext` younger than
    /// `HANDOFF_GRACE`, giving the waker time to park before a sibling can snatch its partner.
    pub(super) runnext_at: Option<std::time::Instant>,
    pub(super) ring: std::collections::VecDeque<Fiber>,
}

/// Caps the per-grab batch a worker pulls from the global queue into its local (`LOCAL_RING_CAP/2`),
/// so one worker can't drain the whole global queue into its local and starve stealing. Go uses 256
/// as the local ring bound; here it serves only as the grab-batch cap (a local is drained before the
/// next grab, so it is not also enforced as a hard push bound).
pub(super) const LOCAL_RING_CAP: usize = 256;

/// D4e — the brief bounded wait a worker does when `runnable > 0` but it found no fiber to run (the
/// work is in a sibling local it lost the steal race for, or in the sub-µs in-hand `Vec` window of a
/// concurrent batch-grab). NOT the idle park (that is a true timeout-less `cv.wait` at `runnable ==
/// 0`): this is a backoff that the surplus-push / wake `notify_all` cuts short, so it adds no
/// latency on the common path. It exists only to stop (W−1) idle workers from busy-spinning on the
/// core lock across the in-hand window (the D4e SRE review's thundering-herd / oversubscription
/// finding) — a sleeping waiter neither hammers lock A nor starves the worker holding the surplus of
/// CPU. Small (the window is microseconds), and a missed notify costs ≤ this, never liveness.
pub(super) const SPIN_BACKOFF: std::time::Duration = std::time::Duration::from_micros(500);

/// TICKET-128 (W13-25) — how long a fresh `runnext` handoff is protected from `try_steal`, mirroring
/// Go's `runqgrab` delay before it will take a P's `runnext`. Without this an idle sibling can steal
/// the woken partner before the waker itself parks, and the pair migrates across workers on every
/// message — exactly the cost this handoff exists to avoid.
pub(super) const HANDOFF_GRACE: std::time::Duration = std::time::Duration::from_micros(200);

/// D4d — every Nth schedule a worker checks the global queue before its own local, bounding the
/// latency of global work while a worker is continuously fed by stealing. Go uses 61 (prime, to
/// avoid resonating with common batch sizes). TICKET-199 — a schedule here is a FRESH slice, as
/// Go's `schedtick`: an inherited `runnext` pick ([`Slice::Inherit`]) does not advance the worker's
/// `tick`. Otherwise step 0 could line up with every slice end and pull a just-preempted fiber back
/// from `global` ahead of the ring.
pub(super) const GLOBAL_CHECK_INTERVAL: u64 = 61;

/// Cross-nursery flat scheduler — the `owner_scope` a FARMED helper / eager drainer passes to
/// [`Vm::mn_worker_loop`]/[`MnSched::take_runnable`]: it owns NO scope, so it never self-stops on a
/// scope completing — it drains the global queue until global `terminate`. Only the INLINE OWNER of a
/// nested nursery passes its real `scope_id` (returns the instant its OWN scope is done).
pub(super) const SENTINEL_SCOPE: usize = usize::MAX;

/// TICKET-199 — the reduction budget a picked fiber runs on (Go `inheritTime`). A `runnext` pick is
/// `Inherit`: it keeps the worker's current reds and does not advance the worker's `tick`, so a
/// rendezvous pair handing each other through `runnext` shares one `CONTEXT_REDS` slice. A ring,
/// global or stolen pick is `Fresh`: a new budget and a new `tick`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Slice {
    Fresh,
    Inherit,
}

impl LocalQ {
    pub(super) fn new() -> Self {
        LocalQ {
            runnext: None,
            runnext_at: None,
            ring: std::collections::VecDeque::new(),
        }
    }
    /// Pop the next fiber to run: `runnext` first (locality), then the ring front (FIFO).
    ///
    /// TICKET-199 — this is where fairness is decided (Go `inheritTime`). A `runnext` pick is
    /// `Slice::Inherit`: it runs on the current slice and does not advance `schedtick`, so a hand-off
    /// pair refilling `runnext` shares one slice. When the slice runs out, the preempted fiber goes to
    /// `global` (`yield_fiber`), a partner inheriting a spent budget yields at its first op, and the
    /// ring is served. A ring pick is `Slice::Fresh`.
    pub(super) fn pop(&mut self) -> Option<(Fiber, Slice)> {
        self.runnext_at = None;
        if let Some(f) = self.runnext.take() {
            return Some((f, Slice::Inherit));
        }
        self.ring.pop_front().map(|f| (f, Slice::Fresh))
    }

    /// TICKET-167 — seeded-mode pop: uniformly at random over `runnext` (if set) plus the whole
    /// ring, instead of always taking `runnext`/the ring front. `None` when both are empty. Reports
    /// the same [`Slice`] as `pop`: `Inherit` exactly when it picks `runnext`.
    pub(super) fn pop_seeded(&mut self, rng: &sched_seed::SeedRng) -> Option<(Fiber, Slice)> {
        let has_runnext = self.runnext.is_some();
        let n = self.ring.len() + usize::from(has_runnext);
        if n == 0 {
            return None;
        }
        let k = rng.below(n as u64) as usize;
        if has_runnext && k == 0 {
            self.runnext_at = None;
            self.runnext.take().map(|f| (f, Slice::Inherit))
        } else {
            self.ring
                .remove(k - usize::from(has_runnext))
                .map(|f| (f, Slice::Fresh))
        }
    }
}

/// Per-connection spawn — the open state of an EAGER `parallel:` nursery (one activated at
/// `EnterNursery` under `--parallel` inside a live fiber, rather than lazily built at `JoinNursery`).
/// A `spawn` in the body injects a handler fiber into `sched` immediately (see [`MnSched::inject`]),
/// so the acceptor keeps running while handlers execute on the sched's farmed workers. Lives on a
/// per-fiber stack ([`FiberCtx::eager_scheds`]) in lockstep with `nurseries`, so an early
/// `?`/`return`/`break`/`recover:` reclaims it alongside the matching `nurseries` level.
pub(super) struct EagerScope {
    /// The live inner sched handlers are injected into; the acceptor becomes its inline worker at join.
    pub(super) sched: Arc<MnSched>,
    /// This inner nursery's cancel token (distinct from the outer's): a handler fault trips it to
    /// abort the accept loop + sibling handlers; the fault then propagates up as the acceptor's fault.
    pub(super) cancel: Arc<AtomicBool>,
    /// The DEDICATED raw OS thread (NOT the bounded pool) that drains injected handlers DURING the
    /// body. The eager body has no inline worker between `EnterNursery` and `JoinNursery`, so a live
    /// drainer that does not depend on the bounded pool is what guarantees liveness — on a 1-core box
    /// the pool would farm zero helpers, and nested eager nurseries would exhaust the pool (an
    /// undetectable hang, since `body_open` vetoes the deadlock predicate). Joined at the end of
    /// `join_eager_nursery`/`abort_eager_nursery` (it exits once the sched terminates). `None` only if
    /// the thread failed to spawn (then the inline join worker is the sole drainer — bounded loops
    /// still complete, just without mid-body concurrency).
    ///
    /// §2c1 — `Some` exactly when this scope OWNS `sched` (it is the OUTERMOST eager nursery on its
    /// thread, so it built the sched and the drainer, and tears both down at its join). A NESTED eager
    /// nursery on the same thread is a *scope* on the enclosing scope's sched and has no drainer of its
    /// own — the owner's one drainer serves every scope, because it drains the GLOBAL queue.
    ///
    /// It is never `None` for an owner: `activate_eager_nursery` returns `None` rather than build a
    /// drainer-less owner, so the caller falls back to the lazy queue-at-join path instead of leaving
    /// a nursery with no worker at all during its body.
    pub(super) drainer: Option<std::thread::JoinHandle<()>>,
    /// TICKET-073 — the [`sched::NestedDrainerSlot`] this scope's `drainer` holds, if any. Held so the
    /// budget share lives until the drainer is joined at the end of `join_eager_nursery` /
    /// `abort_eager_nursery`, and released by `Drop` on every path out of `EagerScope` (see
    /// `NestedDrainerSlot`'s own doc for why a hand-written release is not safe here).
    pub(super) drainer_slot: Option<sched::NestedDrainerSlot>,
    /// §2c1 — this nursery's scope id on `sched`. `0` for an owner; a fresh appended scope for a
    /// nested eager nursery sharing the owner's sched.
    ///
    /// **Why nesting must share ONE sched.** TICKET-099's `wake_run_wide` now reaches a sibling's
    /// private sched too, so wake is no longer the reason two nested eager nurseries on the same
    /// thread's chain must share one sched — the reason is the FAULT predicate. Giving each nursery
    /// its own sched reintroduced exactly the cross-nursery deadlock the flat scheduler was built to
    /// kill — measured, `examples/parallel_cross_nursery_{circular,fanout}.chz` both faulted
    /// `deadlock: every task in this parallel: block is blocked…`. One sched with one scope per
    /// nursery restores it: the inline owner drains the GLOBAL queue, so it runs a sibling scope's
    /// fiber, and `is_deadlocked` sees the enclosing scope's still-open body and vetoes.
    pub(super) scope: usize,
    /// TICKET-103 — scope lives on the spawning fiber's own sched (`self.mn`) because no private
    /// eager sched was available (`worker_count() == 1` or a denied `NestedDrainerSlot`); no
    /// drainer, no `body_open`.
    pub(super) fiber_owned: bool,
    /// TICKET-103 — continuation scopes, spawn order, sharing `cancel`; see
    /// [`MnSched::inject_or_extend`].
    pub(super) more_scopes: Vec<usize>,
}

impl EagerScope {
    /// TICKET-103 — the family: `scope` then every continuation, in slot order.
    pub(super) fn sids(&self) -> Vec<usize> {
        std::iter::once(self.scope)
            .chain(self.more_scopes.iter().copied())
            .collect()
    }
}

/// §6d M:N `wait` (select) park — ONE blocked fiber shared across the N arm-channel buckets it parks
/// on. A `Fiber` owns its live `FiberCtx` and is NOT `Clone`, so it cannot cheaply be filed under N
/// keys the way a plain index could be; instead the single fiber lives here behind
/// `Mutex<Option<Fiber>>` and a refcounted `Arc<WaitPark>` token is filed in each key's bucket. The
/// first waker to ANY key CASes `claimed` false→true; the winner `take()`s the fiber and sweeps the
/// (now stale) token out of every OTHER key's bucket by `Arc::ptr_eq` — all under one hold of the core
/// lock (serialized with [`MnSched::park_wait`]'s gap re-check), so a later `send`/`close` to a swept
/// channel can never re-wake the already-moved fiber. A single-`recv` park stays the cheaper
/// [`ParkedEntry::Recv`] 1-key case and allocates no `WaitPark`.
pub(super) struct WaitPark {
    /// The single parked fiber, taken exactly once by the winning CAS. `None` after it is claimed.
    pub(super) fiber: Mutex<Option<Fiber>>,
    /// Every bucket key this token was filed under — the sweep set the winner removes itself from.
    pub(super) keys: Vec<usize>,
    /// TICKET-185 — the `wait:`'s one `Pending` (the fiber's `pending.p`), readable without taking
    /// `fiber`. A [`WakeKind::Settled`] wake claims this token only once it left QUEUED; two `wait:`
    /// fibers each holding a RECV arm on one cap-0 channel would otherwise claim each other's tokens
    /// forever (DEC-028's hot livelock).
    pub(super) p: Option<Arc<crate::vm::core::Pending>>,
    /// Wake-once gate: the first waker to win the CAS owns the fiber; all later wakers see it set and
    /// drop their (stale) token. Distinct from `parked_n` (a fiber count, not a key count).
    pub(super) claimed: AtomicBool,
}

/// Which parked entries a bucket wake may claim. `All` wakes every entry (close, trip, a buffered
/// push). `Settled` (TICKET-185) requeues only a parked party whose `Pending` left QUEUED; every
/// other entry stays parked. Requeuing an uncommitted party re-runs its op, finds nothing, and
/// re-parks — DEC-028's livelock.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum WakeKind {
    All,
    Settled,
}

/// TICKET-185 — whether a parked entry's hand-off committed (its `Pending` left QUEUED): the one
/// fact a [`WakeKind::Settled`] wake reads. A `None` pending reads as not settled.
pub(super) fn entry_settled(e: &ParkedEntry) -> bool {
    match e {
        ParkedEntry::Recv(f) | ParkedEntry::Send(f) => {
            f.pending.as_ref().is_some_and(|op| !op.p.is_queued())
        }
        ParkedEntry::Wait(wp) => wp.p.as_ref().is_some_and(|p| !p.is_queued()),
    }
}

/// gaps.md W7-56 — every `MnSched` alive in this run, by `Weak` (see [`Vm::sched_registry`] for why
/// it exists and why the handles are weak). The [`ExecRegistry`] of schedulers.
pub(super) type SchedRegistry = Arc<Mutex<Vec<std::sync::Weak<MnSched>>>>;

/// An entry in a `SchedCore.parked` bucket: either a single fiber blocked on a plain `recv` (the
/// common 1-key case, byte-identical to the pre-`wait` engine) OR a refcounted token referencing the
/// one fiber blocked on a multi-channel `wait` (shared across N buckets — see [`WaitPark`]).
// The `Recv` variant inherently carries a whole `Fiber` (the unit of parked work — it must live
// somewhere while blocked), exactly like the `Take` enum above; boxing it would add an allocation on
// the hot recv-park path for no benefit. The `Wait` variant is a cheap `Arc`. The size asymmetry is
// the cost of keeping the common recv park alloc-free.
#[allow(clippy::large_enum_variant)]
pub(super) enum ParkedEntry {
    Recv(Fiber),
    Wait(Arc<WaitPark>),
    /// TICKET-028 — a fiber parked on a blocked `send` (its offer published). Filed by
    /// `MnSched::park_send`; a [`WakeKind::Settled`] wake requeues it once its offer was taken.
    Send(Fiber),
}

impl ParkedEntry {
    /// TICKET-236 — could this parked fiber already resume? Asks the channels its `PendingOp`
    /// names through the one pair of answers every blocked thing uses
    /// ([`quiesce::recv_satisfiable`], [`quiesce::wait_satisfiable`]). A fiber with nothing to
    /// ask (no `pending`, no channel, already claimed) answers `true`, which only declines the
    /// verdict.
    pub(super) fn satisfiable(&self) -> bool {
        match self {
            ParkedEntry::Recv(f) => f.pending.as_ref().is_none_or(|op| {
                op.at
                    .first()
                    .is_none_or(|(core, _, _)| quiesce::recv_satisfiable(core, Some(&op.p)))
            }),
            ParkedEntry::Send(f) => f.pending.as_ref().is_none_or(|op| !op.p.is_queued()),
            ParkedEntry::Wait(wp) => {
                let g = wp.fiber.lock().unwrap_or_else(|e| e.into_inner());
                g.as_ref()
                    .and_then(|f| f.pending.as_ref())
                    .is_none_or(|op| {
                        op.at.is_empty()
                            || quiesce::wait_satisfiable(
                                op.at.iter().map(|(c, _, s)| (&**c, *s)),
                                Some(&op.p),
                            )
                    })
            }
        }
    }
}

pub(super) struct MnSched {
    pub(super) core: Mutex<SchedCore>,
    pub(super) cv: Condvar,
    // N4 — the legacy sched-level `cancel` field is GONE. It held only the OUTERMOST nursery's flag, so
    // every read of it was a latent bug for a nested/enlisted scope: `park`/`park_wait` had already moved
    // to the per-fiber `scopes[fiber.scope_id].cancel`, and its last reader (the netpoller's `register`,
    // via `poll_park_offload`) now takes that same per-scope flag. The outermost cancel lives on
    // `scopes[0].cancel` like every other scope's.
    /// The prebuilt `deadlock` fault, cloned into every still-parked fiber's slot when the predicate
    /// fires, so the join's lowest-index-fault reduce propagates it (`DEADLOCK_MSG`). The parked
    /// fiber's OWN buffered stdout travels into its slot alongside this error (see `flag_deadlock`),
    /// so a parked task's partial output flushes at its task-order slot like a real fault would.
    pub(super) deadlock_err: RuntimeError,
    /// D4a — the authoritative count of *runnable* fibers (queued and waiting to be picked up; not
    /// running, parked, or done). In D2b's single shared queue this exactly mirrors `runq.len()`, but
    /// it lives here as an atomic so D4b can split `runq` into per-worker local rings + a global
    /// overflow (no single queue to `.len()`) while the deadlock predicate stays a cheap O(1) read.
    /// Maintained under the core lock alongside every queue mutation; read by `take_runnable` to test
    /// "no work anywhere" — for BOTH the deadlock predicate AND (D4e) the park gate. **Every
    /// `fetch_add`/`fetch_sub` site holds the core lock** (`seed` is the sole exception — it runs
    /// before any worker is spawned); the only out-of-lock queue movements (`try_steal`, the
    /// batch-grab surplus push) are net-zero on this count, so they touch no atomic at all. D4e's
    /// runnable-gated park RELIES on this discipline: a worker reads `runnable` under the core lock
    /// immediately before `cv.wait`, so the mutex serializes every publish against that observe (it
    /// IS the StoreLoad barrier). **Do not move a `runnable` mutation out of the core lock** — doing
    /// so would reintroduce the lost-wakeup race Go's `nmspinning` fence exists to close.
    pub(super) runnable: AtomicUsize,
    /// D4b — one run queue per worker slot (lock class **B**, taken alone — never while holding the
    /// core lock — so there is no ABBA with the core/channel locks). A worker drains its own
    /// `locals[wid]` before touching the shared global queue; D4c lets it steal from a sibling's.
    pub(super) locals: Vec<Mutex<LocalQ>>,
    /// D4c — a rotating counter mixed into the work-stealing victim choice so idle workers don't all
    /// probe the same sibling first (convoy avoidance) without per-worker RNG state.
    pub(super) steal_ctr: AtomicUsize,
    /// D5 — count of fibers currently off in the dirty/blocking pool (a 4th fiber state beyond
    /// running / runnable / parked / done). A blocking native offload transitions running→inflight;
    /// the pool's completion transitions inflight→runnable. Read by the deadlock predicate
    /// ([`MnSched::is_deadlocked`]) so an in-flight blocking call vetoes a false deadlock fire — the
    /// fiber *will* come back runnable. Like `runnable`, only ever mutated under the core lock, so the
    /// predicate's read is sound.
    pub(super) inflight: AtomicUsize,
    /// W7-26r — the run's `--max-heap` cap (`0` = off), copied from the creating VM's heap. Uniform
    /// per run (the cap lives in the run's one heap core), so one field is the whole story. Read by `finish` to decide a scope's retained-backlog verdict — the observation site
    /// the blocked joining fiber cannot provide.
    pub(super) mem_cap: usize,
    /// TICKET-167 — this sched's seeded-mode RNG stream, keyed by creation order (see `## Decisions`
    /// "One RNG stream per `MnSched`"). Draws are cheap under `sched_seed::on()` and unused otherwise.
    pub(super) rng: sched_seed::SeedRng,
    /// TICKET-205 — gated worker threads with nothing to pick. A wake that leaves a fiber runnable
    /// queues each for the permit ([`MnSched::notify_waiters`]), in registration order.
    pub(super) gated_idle: Mutex<Vec<Arc<width::Slot>>>,
    /// TICKET-099 — every live `MnSched` of this run (the same registry `Vm::sched_registry` publishes
    /// to and `Vm::wake_on_send_key` already walks). A `send`/`close` on THIS sched must be able to
    /// wake a receiver parked on ANY other sched — sibling, ancestor or descendant — not just an
    /// ancestor, so `MnSched::wake_run_wide` walks this instead of the old upward-only ancestor chain.
    /// Assigned AFTER construction (like `exec_registry`/`quiesce` below), so the predicate's
    /// unit fixtures keep an empty registry — i.e. today's behaviour, which is what they test.
    pub(super) sched_registry: crate::vm::SchedRegistry,
    /// TICKET-112 — `true` for a PRIVATE NESTED eager sched (`activate_eager_nursery` with
    /// `self.mn.is_some()`): its scope-0 body is run by a fiber of ANOTHER sched, not a thread of its
    /// own. That fiber is counted on its own sched's `running`/`runnable`/`parked_n`/`inflight`/
    /// `blocked_owners`, and every sched sits in `sched_registry` and `QuiesceState::eager_bodies` — so
    /// its liveness is always visible somewhere else. `quiesced_core`'s body clause and
    /// `is_deadlocked_ignoring_jobs`'s own-verdict decline both read this flag. Always `false` for an
    /// OUTERMOST eager sched, whose body runs on its own dedicated drainer thread.
    pub(super) body_is_fiber: bool,
    /// TICKET-208 — `Some` for an `Executor`'s sched: a scope with no body thread, whose submitters
    /// are other parties. Such a sched never judges itself locally ([`MnSched::is_deadlocked_given`])
    /// and its runners leave when no task is unfinished.
    pub(super) detached: Option<std::sync::Weak<ExecutorCore>>,
    /// gaps.md W7-56 — the run's [`ExecRegistry`], so [`MnSched::is_deadlocked`] can see an eager
    /// `Executor` job as a live, UNCOUNTED feeder. The predicate's counters model fibers of THIS
    /// sched only; an `ex.submit(f)` job runs on the shared pool with no fiber, no `runnable`, no
    /// `inflight` — so a nursery whose only task parks on a channel that job is about to feed
    /// quiesces and faults a healthy program. Same veto `quiesce::QuiesceState::quiesced` already
    /// applies process-wide (`parties.len() < live`), for the same reason.
    ///
    /// Assigned AFTER construction (like `exec_registry`) rather than through `new`, so the predicate's
    /// unit fixtures keep an empty registry — i.e. today's behaviour, which is what they test.
    pub(super) exec_registry: crate::vm::core::ExecRegistry,
    /// gaps.md W7-58 — the run's [`quiesce::QuiesceState`], so an idle worker of this sched can JUDGE
    /// the process-wide verdict. A nursery OWNER never reaches [`Vm::block_halt_check`] (it sits in
    /// `mn_worker_loop`, not in a blocking native), so a run whose only stuck parties are nursery
    /// owners has no polling judge at all and would hang with the party registration alone.
    ///
    /// Assigned AFTER construction (like `sched_registry`/`exec_registry`) rather than through `new`, so
    /// the predicate's unit fixtures keep an empty state — i.e. today's behaviour, which is what they
    /// test (an empty registry means `live == 1` and `parties` empty, so `quiesced` is never reached
    /// past its count gate).
    pub(super) quiesce: Arc<quiesce::QuiesceState>,
    /// TICKET-128 (W13-25) — the idle sleep for a worker with NOTHING runnable (`runnable == 0`).
    /// Split off `cv` (which stays for every timed wait, the joiner, and terminate/deadlock/cancel
    /// broadcasts) so a rendezvous handoff can recruit exactly one idle sleeper (`recruit`) instead
    /// of broadcasting `cv` to every parked worker on every message.
    pub(super) idle_cv: Condvar,
    /// TICKET-128 (W13-25) — count of workers currently asleep on `idle_cv`. `recruit` only bothers
    /// `notify_one` when this is nonzero; `notify_waiters` only touches `idle_cv` at all when this is
    /// nonzero.
    pub(super) idle_sleepers: AtomicUsize,
    /// TICKET-128 (W13-25) — count of workers currently in the bounded `SPIN_BACKOFF` wait (D4e,
    /// `runnable > 0` but nothing grabbed yet). `recruit` skips waking an idle sleeper while any
    /// worker is already spinning — a spinner will pick up the work on its own next pass.
    pub(super) spinning: AtomicUsize,
    /// TICKET-159/211 — how many runner leases `Vm::start_runners` handed to `runner_cache` for this
    /// sched are still serving it. The count lives on the sched because the thread that starts them
    /// (a spawning fiber, or the body at its own block) is not the thread that waits for them:
    /// `join_eager_nursery` / `abort_eager_nursery` wait on `helpers_gone` until it reaches zero.
    /// A lease's thread parks in `runner_cache` for the next sched instead of being joined.
    pub(super) helpers_live: Mutex<usize>,
    pub(super) helpers_gone: Condvar,
    /// TICKET-164 — fibers this sched handed to a worker (`Take::Run` from
    /// [`Self::take_runnable`]). Test-only; published per run by
    /// [`run_capture_counting_picks`].
    #[cfg(test)]
    pub(super) picks: AtomicUsize,
    /// TICKET-185 — receiver-side sender wakes ([`Self::handoff_wake`] calls). Test-only.
    #[cfg(test)]
    pub(super) recv_side_wakes: AtomicUsize,
}

/// Cross-nursery flat scheduler (M:N) — one nursery's JOIN RECORD (Trio/Go-style: structured
/// concurrency = join bookkeeping, not a scheduler frame). The old scalar `{done,total,body_open}`
/// became a [`ScopeTable`] of these so one global `MnSched` can host every nested nursery at once.
/// Each scope owns a contiguous `base_index..base_index+total` sub-range of the FLAT `SchedCore::slots`.
pub(super) struct JoinScope {
    /// This scope's offset into the flat `SchedCore::slots`: its tasks occupy `base_index..base_index+total`.
    pub(super) base_index: usize,
    /// Task count for this scope. Grows via `inject` (per-connection eager spawn), exactly as the old
    /// scalar `total` did.
    pub(super) total: usize,
    /// Tasks in this scope that have produced a `TaskOutcome`.
    pub(super) done: usize,
    /// W7-26r — the bytes this scope's finished tasks have RETAINED in `SchedCore::slots` (buffered
    /// stdout/stderr, held to the join for the task-order flush). Accumulated by `finish` and
    /// compared against `MnSched::mem_cap` there, because these slots live outside every `Heap` and
    /// so are reachable by `Heap::live_bytes` NOWHERE — a `parallel:` of 300 tasks each printing
    /// ~1 MB measured PASS at 733 MB against an 8 MB cap. Per-scope, not per-sched: a nested nursery
    /// reduces (and frees) its own slots at its own join.
    pub(super) bytes: usize,
    /// `true` while an EAGER nursery's body is still running (between `EnterNursery` and `JoinNursery`)
    /// and may still `inject` more tasks. While set, a transient `done == total` for this scope must NOT
    /// terminate the global sched and `is_deadlocked` is vetoed (the body is live work the sched can't
    /// see). `JoinNursery` clears it. Always `false` for a lazy (queue-at-join) nursery.
    pub(super) body_open: bool,
    /// §2c1 — `true` while this eager scope's body thread is itself BLOCKED in place (parked on a
    /// channel / `wait:` / an executor join — the body marks of a wait's counts set and clear
    /// this, `block::wait_steps`).
    ///
    /// `body_open` means "the body may still `inject`". While the body is blocked that is FALSE — it
    /// cannot reach another `spawn` until something wakes it — so the DEADLOCK predicate must stop
    /// vetoing on it, or a top-level nursery (whose body is open for essentially the whole program)
    /// would turn every genuine `main`-plus-sibling deadlock into a hang. Derived from what is
    /// *impossible*, not from what looks idle — the same shape as `awaiting_builder` below.
    ///
    /// **TERMINATE still vetoes on plain `body_open`** (`all_scopes_done() && !any_body_open()`): the
    /// body is only blocked, not finished, and it may well `spawn` again after it wakes. Only
    /// [`SchedCore::any_body_injecting`] — the deadlock predicate's question — honours this flag.
    pub(super) body_blocked: bool,
    /// Cross-nursery flat scheduler — `true` while this scope is an EARLY-ENLISTED outer nursery still
    /// awaiting the inline builder's own `JoinNursery` (`early_enlist_outer` sets it; `join_enlisted_scope`
    /// / `abort_enlisted_scope` clear it as the builder begins draining the scope). While set, the scope's
    /// parked fibers have a live external feeder — the builder body, which may still `send`/`close`/`spawn`
    /// — so a quiesce in which EVERY incomplete scope is `awaiting_builder` is NOT a deadlock: the builder
    /// has finished all nested service and will return to the body to feed them (see
    /// `all_incomplete_awaiting_builder` + `is_deadlocked`).
    ///
    /// §2c1 — an EAGER scope now raises it too, for the span in which its body is parked in a NESTED
    /// nursery's join (`BodyMark::Awaiting`). The meaning is identical: a builder will
    /// return to this scope and may then `send`/`close`, so a quiesce in which the inner scope is DONE
    /// is not a deadlock. It stays `false` for a body blocked on a CHANNEL — that body resumes only if
    /// somebody feeds it, so it promises no progress.
    pub(super) awaiting_builder: bool,
    /// This scope's cancel token (the SAME `Arc` cloned onto fibers running in this scope; distinct
    /// per nursery so an inner fault cancels ONLY its scope, never an outer sibling — the structured-
    /// concurrency invariant). Read by `park`/`park_wait`'s gap re-check and the running fiber's
    /// back-edge (via the shell's re-pointed `self.cancel`) and `cancel_drain(scope_id)`.
    pub(super) cancel: Arc<AtomicBool>,
    /// The cancel tokens of every ENCLOSING scope, outermost first (empty for the outermost nursery).
    /// Structured concurrency: cancelling a scope cancels its DESCENDANT scopes. A nested nursery keeps
    /// its own `cancel` (so an inner fault never cancels an outer sibling — the other half of the
    /// invariant), but its fibers must still observe an outer cancel at their checkpoints, or a nested
    /// nursery entered from a task that is later cancelled becomes UNCANCELLABLE and a spinning
    /// grandchild hangs the teardown forever. Read at every checkpoint via the shell's re-pointed
    /// `Vm::cancel_outer` (`Vm::halt_requested`).
    pub(super) ancestors: Vec<Arc<AtomicBool>>,
    /// This scope's own deadlock fault, used instead of `MnSched::deadlock_err` when set. `flag_deadlock`
    /// clones ONE error into every parked fiber's slot (DEC-048), so a scope with a more specific span
    /// (the owner-fiber arm of `run_mn_nursery_nested`) overrides it here. `None` for every other scope,
    /// including an early-enlisted outer scope (DEC-048: it reports the BUILDER's span) and the
    /// `(None, Some(held))` late-spawn arm (no `blocked_owners` bracket, so no per-scope span either).
    pub(super) deadlock_err: Option<RuntimeError>,
    /// TICKET-103 — the fibers OF THIS SCOPE blocked at a nested join: inline ones (of
    /// `SchedCore::blocked_owners`) and join-parked ones (`SchedCore::join_parked`). A joined family
    /// holding one is interior for `flag_deadlock_leaves`: that fiber may feed it once its own join
    /// returns.
    pub(super) owners_blocked: usize,
    /// TICKET-103 — owners joining the nursery whose ORIGIN scope this is, on this same sched:
    /// inline ones (set by `join_fiber_owned_nursery`, `abort_fiber_owned_nursery` and
    /// `run_mn_nursery_nested`'s owner arm) and join-parked ones (`MnSched::park_join`). Its family
    /// is a target for `flag_deadlock_leaves`.
    pub(super) joins_blocked: usize,
    /// TICKET-125 — the scope of the fiber that owns this fiber-owned nursery; `None` otherwise.
    pub(super) parent_scope: Option<usize>,
}

/// TICKET-199 — THE issuer and resolver of scope ids on one sched. Ids are monotonic and never
/// reissued: `open` hands out the next id, and a retired id reads as absent (`get` is `None`, and
/// `SchedCore::scope_family` of it is empty). A worker may hold a scope id after dropping the core
/// lock that proved it live (`mn_worker_loop` after `finish`, the drain scan in `take_runnable`,
/// `cancel_all`); under the old `id = len()` plus pop rule that id went out of bounds or named the
/// NEXT nursery, whose fibers were then drained. Only a reader that holds a live fiber of the scope,
/// or its owner before retire, may index (`table[id]`); every other reader goes through `get`.
/// There is deliberately no `len()`: an id is not a position.
pub(super) struct ScopeTable {
    next: usize,
    /// Live scopes, ascending by id.
    live: Vec<(usize, JoinScope)>,
}

impl ScopeTable {
    /// A table holding only the sched's root scope, id 0 (never retired).
    pub(super) fn with_root(root: JoinScope) -> Self {
        Self {
            next: 1,
            live: vec![(0, root)],
        }
    }

    /// Issue a fresh id for `scope`.
    pub(super) fn open(&mut self, scope: JoinScope) -> usize {
        let id = self.next;
        self.next += 1;
        self.live.push((id, scope));
        id
    }

    fn pos(&self, id: usize) -> Option<usize> {
        self.live.binary_search_by_key(&id, |(i, _)| *i).ok()
    }

    pub(super) fn get(&self, id: usize) -> Option<&JoinScope> {
        self.pos(id).map(|p| &self.live[p].1)
    }

    fn get_mut(&mut self, id: usize) -> Option<&mut JoinScope> {
        self.pos(id).map(|p| &mut self.live[p].1)
    }

    /// Live scopes with their ids, ascending.
    pub(super) fn iter(&self) -> impl Iterator<Item = (usize, &JoinScope)> {
        self.live.iter().map(|(i, s)| (*i, s))
    }

    pub(super) fn values(&self) -> impl Iterator<Item = &JoinScope> {
        self.live.iter().map(|(_, s)| s)
    }

    pub(super) fn values_mut(&mut self) -> impl Iterator<Item = &mut JoinScope> {
        self.live.iter_mut().map(|(_, s)| s)
    }

    pub(super) fn ids(&self) -> Vec<usize> {
        self.live.iter().map(|(i, _)| *i).collect()
    }

    pub(super) fn count(&self) -> usize {
        self.live.len()
    }

    /// The newest live scope: the only one that owns the `slots` tail and may grow in place.
    pub(super) fn last_id(&self) -> Option<usize> {
        self.live.last().map(|(i, _)| *i)
    }

    pub(super) fn remove(&mut self, id: usize) -> Option<JoinScope> {
        self.pos(id).map(|p| self.live.remove(p).1)
    }

    /// The end of the newest live scope's slot range (0 when empty): `slots` is truncated to it
    /// after every removal, so a retired tail gives its slots back.
    pub(super) fn slot_end(&self) -> usize {
        self.live.last().map_or(0, |(_, s)| s.base_index + s.total)
    }
}

impl std::ops::Index<usize> for ScopeTable {
    type Output = JoinScope;
    fn index(&self, id: usize) -> &JoinScope {
        self.get(id)
            .unwrap_or_else(|| panic!("scope {id} was retired while a live holder indexed it"))
    }
}

impl std::ops::IndexMut<usize> for ScopeTable {
    fn index_mut(&mut self, id: usize) -> &mut JoinScope {
        self.get_mut(id)
            .unwrap_or_else(|| panic!("scope {id} was retired while a live holder indexed it"))
    }
}

/// TICKET-219 — what settles a job that [`SchedCore::job_event`] drops or cuts: the fresh cap-1
/// result channel of its Task and the job's own cancel flag. Registered at the submit; every job
/// has one (TICKET-289).
pub(super) struct JobSettle {
    pub(super) ch: Arc<ChannelCore>,
    /// TICKET-289 — the job's own cancel flag: `Task.cancel()` and an uncatchable end of this job
    /// trip it, and no sibling reads it. The job's fiber carries a clone ([`Fiber::job_cancel`]).
    pub(super) flag: Arc<AtomicBool>,
}

impl JobSettle {
    /// Seal the Task's channel with `Err(msg)`, as text: the transition that seals it holds no
    /// heap, and the first receive builds the value (`Vm::recv_ready`). Returns whether this call
    /// sealed it: the first writer wins, and a body that sealed its own value keeps it. Lock order
    /// `SchedCore` then `ChannelCore::q`, as `send_wake`.
    pub(super) fn seal_err(&self, msg: &str) -> bool {
        self.ch
            .q
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .seal_err(msg)
    }
}

/// TICKET-289 — what [`SchedCore::settle_job`] seals into a job's Task when the job ends without
/// sealing its own value.
pub(super) enum JobSeal {
    /// Seal nothing: the end is a hard abort, which is never a Task outcome, or the run is over.
    Nothing,
    /// Seal the cancel value.
    Cancel,
    /// Seal `Err(message)`: the job ended by a fault.
    Fault(Box<str>),
}

/// TICKET-219 — an event in an Executor job's life, the input of [`SchedCore::job_event`].
pub(super) enum JobEvent {
    /// A submitted job, its slot reserved.
    Submit(Box<Fiber>),
    /// A job's slot was filled at `finish`; `seal` is what the Task gets if the body sealed none.
    Ended {
        task_index: usize,
        scope_id: usize,
        seal: JobSeal,
    },
    /// A parked job was reaped by the deadlock verdict.
    Reaped { task_index: usize, scope_id: usize },
    /// `cancel` drops the held jobs.
    DropHeld,
    /// TICKET-289 — `Task.cancel()`: cancel the one job whose Task channel is this one.
    CancelOne(Arc<ChannelCore>),
}

/// TICKET-219 — what the caller of [`SchedCore::job_event`] does next: enqueue `start` under the
/// sched lock, then wake the readers of `sealed` after it drops the lock
/// ([`MnSched::wake_sealed`]). `sealed` holds the handle channels the transition itself sealed
/// with their cancel value ([`SchedCore::settle_job`], TICKET-232). `cut_flag` is the flag of a
/// running job `CancelOne` tripped: the caller drains the fibers that flag wakes.
#[derive(Default)]
pub(super) struct JobStep {
    pub(super) start: Vec<Fiber>,
    pub(super) sealed: Vec<Arc<ChannelCore>>,
    pub(super) cut_flag: Option<Arc<AtomicBool>>,
}

pub(super) struct SchedCore {
    /// TICKET-236 — the run's epoch, the same `Arc` as this sched's
    /// [`quiesce::QuiesceState::epoch`] ([`MnSched::join_run`] is the one place that pairs them).
    /// Held here so each way out of the counts moves it inside the lock hold that writes the
    /// count ([`SchedCore::note_left`]).
    pub(super) left: Arc<AtomicU64>,
    /// The run's deadlock listing, the same `Arc` as [`quiesce::QuiesceState::listing`]: a parked
    /// fiber the verdict cuts adds its entry here ([`SchedCore::record_deadlocked`]).
    pub(super) listing: Arc<super::deadlock::Listing>,
    /// The global overflow / seed queue. Seed + every coordinator-path requeue (deadlock flag,
    /// cancel drain) land here; per-worker requeues go to a worker's `locals[wid]` (D4c). Drained by
    /// a worker only after its own local is empty, so the global queue is the shared fallback.
    pub(super) global: std::collections::VecDeque<Fiber>,
    /// Fibers parked on an empty `recv` OR a multi-channel `wait`, keyed by `ChannelCore` pointer
    /// ([`Vm::channel_core_ptr`]). A bucket holds either a `ParkedEntry::Recv(fiber)` (1-key recv park)
    /// or a `ParkedEntry::Wait(token)` (a shared [`WaitPark`] referenced from every arm channel's
    /// bucket — see [`MnSched::park_wait`]/`send_wake`). `parked_n` counts FIBERS (a wait fiber is +1
    /// regardless of how many buckets hold its token), not bucket entries.
    pub(super) parked: std::collections::HashMap<usize, Vec<ParkedEntry>>,
    /// Decision-F per-task outcome slots, FLAT across every nursery scope, indexed by
    /// `Fiber::task_index` (= `scopes[scope_id].base_index + local_i`). `None` until that task ends.
    /// Kept flat (not partitioned into `JoinScope`) so `finish`/`flag_deadlock`/`inject`/`reduce` index
    /// by a single global index, minimizing churn; each scope owns a contiguous `base_index..base+total`
    /// sub-range (reduce/take operate per-scope on that sub-slice — see `take_scope_slots`).
    pub(super) slots: Vec<Option<TaskOutcome>>,
    pub(super) running: usize, // fibers currently swapped into a worker (executing)
    /// Of `running`, how many are fibers whose OS thread is blocked in a NESTED nursery join on THIS
    /// sched. Such a fiber cannot send, yield or spawn until its child scope moves, so it is stuck by
    /// construction — see `is_deadlocked_ignoring_jobs`'s `running == blocked_owners` clause.
    pub(super) blocked_owners: usize,
    /// TICKET-099 — of `blocked_owners`, how many are blocked on a CHILD SCHED (a private eager
    /// nursery of its own), not a scope on THIS sched. This sched has NO visibility into that child's
    /// progress (it is not one of `scopes`), so — unlike a same-sched `blocked_owners` fiber, whose
    /// child scope's own siblings would show up in THIS sched's `running`/`runnable` if they could feed
    /// it — this sched cannot tell whether the child is genuinely stuck or about to resolve. A nonzero
    /// count vetoes only THIS sched's OWN fault decision (`is_deadlocked_ignoring_jobs`), the same way
    /// `is_deadlocked`'s `outstanding_jobs` veto does for an uncounted `Executor` sender: the child's
    /// own deadlock detection (if genuine) will fire on the CHILD sched instead, and its `Fault`
    /// propagates back through this fiber's own call return (`MnSched::finish_with` trips its cancel)
    /// exactly like any other task failure — never through this sched declaring itself deadlocked.
    /// `local_quiesced` does NOT read this field: a peer sched's veto (`peer_can_move` →
    /// `local_quiesced`) must still see THIS sched as quiesced, or the child's own genuine deadlock is
    /// vetoed forever (the hang this ticket's cross-sched `blocked_owner_guard` widening fixes).
    pub(super) cross_sched_blocked_owners: usize,
    /// TICKET-118 (W13-7) — the OS thread of the top-level `Vm` (`mn.is_none()`) joining this
    /// sched's nursery, the only thread that may hand its bounded-pool slot over while it waits
    /// (DEC-052).
    pub(super) pool_joiner: Option<std::thread::ThreadId>,
    /// TICKET-126 (W13-24) — the `CANCEL_GEN` value at which this sched's last drain scan found no
    /// cancelled scope owing a drain. Written only by [`SchedCore::drain_scan_due`].
    pub(super) drain_scan_gen: u64,
    pub(super) parked_n: usize, // total fibers across every `parked` bucket
    /// Parked fibers [`SchedCore::cut_aborting_families`] moved to `global`, owed their
    /// `runnable` count by the verdict's caller ([`MnSched::take_cut_requeued`]).
    pub(super) cut_requeued: usize,
    /// TICKET-103 — owners parked at the `JoinNursery` of a fiber-owned nursery (`Disp::JoinPark`),
    /// as `(origin scope id, fiber)`. In neither `running` nor `parked_n`. Woken by
    /// `MnSched::wake_completed_joins` once every scope of the origin's family is done; recorded
    /// `Deadlocked` by `flag_deadlock`.
    pub(super) join_parked: Vec<(usize, Fiber)>,
    /// Cross-nursery flat scheduler (M:N) — the per-nursery join records, replacing the old scalar
    /// `{done,total,body_open}`. ONE global `MnSched` is shared by every nested `run_mn_nursery` /
    /// eager nursery (built only by the outermost owner); each `register_scope` appends a `JoinScope`
    /// and enlists its fibers into the SAME global run queue. A fiber carries its `scope_id`; the inline
    /// owner of a scope stops when ITS OWN scope is done (scope-scoped owner stop), while it drains the
    /// global queue (so it naturally runs cross-nursery siblings — the case-A fix). TICKET-199 — a
    /// [`ScopeTable`]: ids are never reissued, and a retired id reads as absent.
    pub(super) scopes: ScopeTable,
    pub(super) terminate: bool, // every worker loop exits once set (all scopes done, deadlock, or os.exit/fault)
    /// TICKET-296: the deadlock verdict cut every blocked-in-place waiter of this sched. Read through
    /// `SchedCore::cut_in_place`. `flag_deadlock` latches `terminate` only when no demoted fiber is
    /// left to resume: a cut fiber unwinds and can requeue itself.
    pub(super) verdict_cut: bool,
    /// TICKET-181 — THE registry of blocked waiters the counters cannot see: every demoted recv
    /// or `wait:`, every M:N guard wait, and every blocked body of this sched's thread
    /// (the body marks of a wait's counts, `block::wait_steps`), keyed by a token. Each entry answers "can you still be
    /// satisfied?" ([`crate::vm::block::Waiter::satisfiable`]: a cancel flag it would honour, or its
    /// `PartyWait`), and `quiesced_core_given` vetoes while any can — the ONE veto clause. A new
    /// blocking shape adds a `PartyWait` arm, never a new list here. `fiber` entries are the demoted
    /// victims the verdict may claim. Registered/un-registered under core lock A; a demoted wait's
    /// entry is removed in the same lock hold as the pop that ends it (DEC-176).
    pub(super) waiters: std::collections::HashMap<u64, crate::vm::block::Waiter>,
    pub(super) next_waiter_tok: u64,
    /// TICKET-211 — THE claim table of this sched's runner threads, one bit per wid. wid 0 = the
    /// body's own thread (top-level main: body, then inline joiner); wid 1 = drainer; wids 2..=n =
    /// started runners; an Executor sched never claims wid 0. Read and written only by
    /// [`MnSched::claim_runners`] and its callers, under this lock; a runner clears its bit when it
    /// leaves.
    pub(super) runner_wids: Vec<bool>,
    /// The scope a detached sched's next `submit` grows (see [`MnSched::reserve_slot`]).
    pub(super) exec_tail: usize,
    /// `Executor(n)`'s cap on the jobs running at once; zero means no cap.
    pub(super) exec_limit: usize,
    /// Job slots admitted and not yet filled. Written by [`SchedCore::job_event`] alone, except
    /// the no-runner pre-count in `spawn_into`, which starts nothing.
    pub(super) exec_active: usize,
    /// Jobs submitted over the cap, slot reserved, not yet started. Read and written by
    /// [`SchedCore::job_event`] alone.
    pub(super) exec_held: std::collections::VecDeque<Fiber>,
    /// TICKET-219 — task index of a handle job to what settles its handle when the job is dropped
    /// or cut. Read and written by [`SchedCore::job_event`] alone (inserted at the reserve).
    pub(super) job_settle: fxhash::FxHashMap<usize, JobSettle>,
    /// A deadlocked fiber of this sched is reported at its own blocking op. True for a detached
    /// sched (an `Executor`) alone: a nursery sched reports at the nursery.
    pub(super) leaf_site: bool,
    /// How much of the scopes' retained-output `bytes` [`MnSched::take_charge`] already reported.
    pub(super) charged: usize,
    /// TICKET-286 — the run deadline whose [`MnSched::deadline_drain`] job is already armed.
    pub(super) deadline_armed: Option<std::time::Instant>,
}

/// D2b — the disposition of one fiber run on a worker shell: park it on a channel (it blocked on an
/// empty `recv`; carries the channel core ptr key + the `Arc<ChannelCore>` so `park` can re-check the
/// queue under the sched lock) or finish it with a terminal outcome.
pub(super) enum Disp {
    Park(usize, Arc<ChannelCore>),
    /// Bounded-channel backpressure — the fiber blocked on a full `send` (the send-side twin of
    /// `Park`). Carries the key captured WHILE the fiber heap was live; the worker loop hands it
    /// to [`MnSched::park_send`], whose gap re-check reads the offer's `Pending` (TICKET-185).
    SendPark(usize),
    /// §6d — the fiber blocked on a multi-channel `wait` (every arm empty/live). Carries
    /// `(source arm index, key, core, is_send)` for each live arm, captured WHILE the fiber heap was live (like `Park`);
    /// the worker loop hands it to [`MnSched::park_wait`], which files ONE shared `WaitPark` token in
    /// every arm bucket. `is_send` selects the per-arm gap-recheck readiness predicate.
    WaitPark(Vec<(u32, usize, Arc<ChannelCore>, bool)>),
    /// D3 — the fiber exhausted its reduction budget; the worker requeues it at the tail of the global queue.
    Yield,
    /// D5 — the fiber hit a blocking native call; the worker hands the call (and the fiber) to the
    /// dirty/blocking pool and is freed to schedule other work. The pool re-enqueues the fiber on
    /// completion (`MnSched::complete_offload`).
    Offload(OffloadReq),
    /// D6 — the fiber's socket op returned `WouldBlock`; the worker hands the fiber + fd to the
    /// netpoller (`MnSched::poll_park_offload`) and is freed. The poller re-enqueues the fiber on OS
    /// readiness (`MnSched::complete_offload`); the rewound op then re-runs.
    PollPark(PollPark),
    /// TICKET-103 — the fiber reached the `JoinNursery` of a fiber-owned nursery whose family is
    /// incomplete; filed via `MnSched::park_join`; the op re-executes on resume.
    JoinPark(usize),
    Finish(TaskOutcome),
}

/// D6 — a socket op parked on fd readiness: the `key` (the `SocketCore`/`ListenerCore` poll key, also
/// the netpoller registry key + the `close`-side `deregister` handle), the raw `fd`, and the
/// direction of interest. Carried from the op's `WouldBlock` site up to the worker loop, which hands
/// it to [`MnSched::poll_park_offload`].
pub(super) struct PollPark {
    pub(super) key: usize,
    pub(super) fd: std::os::fd::RawFd,
    pub(super) interest: poller::Interest,
    /// D6 — the owning socket's `in_flight` flag, handed to the poller so it can clear it on inject
    /// (see [`core::SocketCore::in_flight`]).
    pub(super) in_flight: Arc<AtomicBool>,
    /// W15-1 — the owning core's `closed` flag; `register` refuses the park when set.
    pub(super) closed: Arc<AtomicBool>,
    /// D6c — when the poll thread gives up waiting for the fd: it re-injects the fiber with its
    /// `poll_timed_out` marker set, and the rewound op returns `Err("timeout")`. `None` = park
    /// forever (nothing here bounds the poll thread's own wait either — see `next_timeout`).
    ///
    /// W7-18 — this is the SOONER of TWO deadlines, not just the op's `timeout_ms`: `park_on_fd`
    /// clamps it by the run's `--timeout` deadline, and `park_on_connect` sets it from that deadline
    /// ALONE (a `connect` takes no `timeout_ms` of its own). So `Some` no longer implies the op was
    /// given a timeout, and the marker no longer implies a catchable `Err`: the consumer decides which
    /// deadline expired by re-reading the clock — [`Vm::poll_timeout_check`] for the four rewound ops,
    /// and the `pending_connect` arm of [`Vm::run_one_fiber`] for a connect, which is not a rewound op
    /// at all and raises the hard halt directly.
    pub(super) deadline: Option<std::time::Instant>,
}

/// D6c — a parsed `timeout_ms` argument to a net socket op. `poll_once` (true iff `ms <= 0`) means
/// "do NOT park: if the syscall would block, return `Err("timeout")` at once"; otherwise the op parks
/// with `deadline` and the netpoller wakes it on readiness OR at the deadline (whichever first).
#[derive(Clone, Copy)]
pub(super) struct SockTimeout {
    pub(super) poll_once: bool,
    pub(super) deadline: std::time::Instant,
}

/// D5 owe #3 Path C (#3 socket half) — the outcome of one non-blocking socket-op attempt under the
/// in-callback demote loop ([`Vm::demote_block_socket`]). `Ready` carries the op's final
/// `Result[..]`-shaped `Value` (`sock_ok`/`sock_err`); `WouldBlock` means "fd not ready, re-poll after
/// a backoff". Used only on the demote path (a callback can't snapshot-park onto the netpoller).
pub(super) enum SockPoll {
    Ready(Result<Value, RuntimeError>),
    WouldBlock,
}

/// The outcome of one blocking-`recv` step ([`Vm::chan_recv_step`] / [`Vm::demote_recv_block`]):
/// a value was dequeued, the channel is closed-and-drained, or the fiber parked (re-runs on wake).
/// Shared by bare `recv` (`Got` → `Ok(v)`, `ClosedEmpty` → `Err(RecvError)`, no fault) and
/// the `ChanRecvOrClosed` op driving `for v in ch:` (`Got` → `Some(v)`, `ClosedEmpty` → `None`).
///
/// TICKET-185 — `Filled` is a value a sender's `give` put into this receiver's own slot. That
/// `give` committed the receiver and nobody else, and the giver has already returned, so the
/// caller wakes no sender for it (the `wait:` settle path in `Vm::op_wait_poll` never did). `Got`
/// is every other value — a buffered one, or a parked sender's offer, whose sender must be woken.
pub(super) enum RecvStep {
    Got(Value),
    Filled(Value),
    ClosedEmpty,
    Parked,
}

/// The outcome of one bounded `send` step ([`Vm::chan_send_step`]): the value was queued (or the
/// channel was unbounded — never blocks), or the fiber parked on a full channel (re-runs on wake).
/// A closed/deadlock case is returned as an `Err` from `chan_send_step`, not a variant here.
pub(super) enum SendStep {
    Sent,
    Parked,
}

/// D6b — a non-blocking `connect` whose TCP handshake is still in flight (`EINPROGRESS`): the
/// connecting (non-blocking) `TcpStream` (it owns the fd being polled, so it must outlive the park),
/// its stable poll `key`, and a fresh `in_flight` guard. Lives in [`FiberCtx`] (per-fiber, non-heap)
/// so it survives the writability park and travels with the fiber via [`Vm::swap_ctx`]; the resumed
/// `net.connect` takes it back and calls [`crate::native::net::finish_connect`] to read `SO_ERROR`.
pub(super) struct ConnectInProgress {
    pub(super) stream: std::net::TcpStream,
    pub(super) key: usize,
    pub(super) in_flight: Arc<AtomicBool>,
    /// W7-18 — the `net.connect` call's span, carried so the resume can RAISE a `--timeout` hard halt
    /// attributed to the connect (see the `pending_connect` arm in `run_one_fiber`). A connect park
    /// has no other checkpoint to fall through to: the call is followed by a straight-line `match`
    /// with no back-edge and no blocking op.
    pub(super) span: Span,
}

/// D5 — a blocking native call extracted at its dispatch site, ready to run off the core worker on
/// the blocking pool. The args are already materialized out of the heap into `Send` primitives
/// ([`crate::native::NativeArg`]), so the pool thread runs the native ([`OffloadHost`]) without a
/// `Vm` / heap. `span` attributes any error the native raises.
pub(super) struct OffloadReq {
    pub(super) func: crate::native::NativeFn,
    pub(super) args: Vec<crate::native::NativeArg>,
    pub(super) span: Span,
    /// D5 owe #2 — `Some(..)` for a `sleep_ms`: park the fiber on the timer thread for its duration
    /// rather than run `func` on a dirty-pool thread (a sleep does no work, just waits a deadline).
    /// `None` for every other blocking native (`io`/`fs`/`request`/`process`), which runs on the pool.
    pub(super) timer: Option<TimerSleep>,
}

/// W7-16 — everything the timer thread needs to keep an offloaded `sleep_ms` a CANCELLATION and
/// `--timeout` checkpoint for its whole duration. Snapshotted at the call site (in `invoke_native`),
/// because the timer job runs with no `Vm` and no scheduler lock of its own.
pub(super) struct TimerSleep {
    /// When the sleep is over.
    pub(super) deadline: std::time::Instant,
    /// The fiber's wake set ([`Vm::wake_set`] — deliberately EMPTY inside a `defer`, so a sleeping
    /// cleanup stays uncancellable, as contracted): its cancel flags and ancestors', plus the flags of
    /// the nurseries it owns (TICKET-188). Snapshotted HERE, not `c.scopes[id].cancel` inside
    /// `offload`: the latter is this scope's own flag only, and would miss an enclosing nursery's
    /// cancel and every owned child's fault.
    pub(super) wake: block::WakeSet,
    /// The run's absolute `--timeout` deadline, if any.
    pub(super) run_deadline: Option<std::time::Instant>,
    /// …and its configured value, for the fault message.
    pub(super) timeout_ms: u64,
}
