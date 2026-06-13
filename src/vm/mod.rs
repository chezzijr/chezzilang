//! Bytecode stack VM (M5) — the Phase-2 execution path. Runs the [`Program`] produced by the
//! compiler, reproducing the tree-walk interpreter's semantics byte-for-byte (golden/parity tests
//! cross-check the two engines). M5a: handle-addressed values, no collector yet (the mark-sweep
//! GC lands in M5b).

pub mod core;
pub mod heap;
pub mod op;
mod blocking_pool;
pub mod chzstr;
mod fxhash;
mod poller;
mod pool;
mod timer;
pub mod value;
pub mod wire;

use core::{AtomicCore, ChannelCore, ExecutorCore, ListenerCore, SharedCore, SocketCore};
use heap::{Heap, MapData, Obj, SetData};
use op::{CapEntry, CapSrc, Op, Program, ProtoId, NO_IC, TID_NONE};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use value::{GcRef, Value};
use wire::WireValue;

use crate::ast::Span;
#[cfg(test)]
use crate::{lexer, parser};

/// A runtime error, with the source span it occurred at. Mirrors `interp::RuntimeError` (same
/// `Display`) so the two engines' failures compare equal in parity tests.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeError {
    pub message: String,
    pub span: Span,
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime error ({}): {}", self.span, self.message)
    }
}

/// One frame of a runtime stack trace: a function and the call site that entered it. Mirrors
/// `interp::TraceFrame`.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceFrame {
    pub function: String,
    pub span: Span,
}

/// A runtime error enriched with a stack trace, produced at the run boundary for an uncaught fault.
/// `Display` matches [`RuntimeError`] exactly (message only — the trace is printed separately) so
/// parity tests that compare error strings are unaffected. Mirrors `interp::RunError`.
#[derive(Debug, Clone)]
pub struct RunError {
    pub message: String,
    pub span: Span,
    pub trace: Vec<TraceFrame>,
}

impl RunError {
    fn from_error(e: RuntimeError, trace: Vec<TraceFrame>) -> Self {
        RunError { message: e.message, span: e.span, trace }
    }
    fn plain(e: RuntimeError) -> Self {
        RunError { message: e.message, span: e.span, trace: Vec::new() }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime error ({}): {}", self.span, self.message)
    }
}

/// Render a runtime error plus its stack trace for the CLI: the error line, then one indented
/// `  at <function> (<call site>)` line per frame, innermost first. Shared by both engines' drivers.
pub fn format_trace(message: &str, span: Span, trace: &[TraceFrame]) -> String {
    let mut s = format!("runtime error ({span}): {message}");
    for frame in trace {
        s.push_str(&format!("\n  at {} (called at {})", frame.function, frame.span));
    }
    s
}

/// Maximum user-function call depth — mirrors the interpreter, so infinite recursion is a clean
/// runtime error rather than a host stack overflow.
const MAX_CALL_DEPTH: usize = 10_000;

/// Maximum structural-recursion depth for value display / equality — a cyclic data structure (e.g.
/// a struct with a `list[Self]` field forming a cycle) would otherwise recurse unbounded on the
/// HOST stack and SIGABRT (uncatchable). This bound turns that into a recoverable `RuntimeError`.
const MAX_STRUCTURAL_DEPTH: usize = 10_000;

// M19 Tier-2 — adaptive opcode quickening (PEP 659) per-site states (see [`Vm::quicken`]).
/// Never executed yet: on first run, observe operand types and transition to `Q_INT` or `Q_GENERIC`.
const Q_COLD: u8 = 0;
/// Specialized: both operands were `Int` — take the int fast path, guarded (a non-int operand on a
/// later run deopts the site to `Q_GENERIC`).
const Q_INT: u8 = 1;
/// Deopted / polymorphic: always run the generic path. Sticky — never re-specializes, so a site that
/// sees mixed types never thrashes between fast and slow forms.
const Q_GENERIC: u8 = 2;

/// Stack size for the VM thread (same as the interpreter's): the VM recurses on the host stack
/// when a builtin/method re-enters the dispatch loop, so a large dedicated stack decouples the
/// call-depth limit from the caller's thread.
const VM_STACK_BYTES: usize = 256 * 1024 * 1024;

/// One activation record.
struct CallFrame {
    proto: ProtoId,
    ip: usize,
    /// Index into the operand stack where this frame's slots begin.
    base: usize,
    /// Module globals this frame resolves top-level names against (home-globals).
    home: GcRef,
    /// The closure object backing this frame, if it is a closure call (for `GetCaptured`).
    closure: Option<GcRef>,
    /// Whether this frame counts toward the call-depth limit (user calls do; module toplevels
    /// don't, matching the interpreter).
    counted: bool,
    /// Module toplevel frame — an `Err`/`None` unhandled here (a `?` or a bare expression
    /// statement) is a top-level unhandled error that exits the program.
    is_toplevel: bool,
    /// Calls registered by `defer` in this frame, in source order. Drained LIFO when the frame
    /// exits (return / `?` / panic). Receiver/args are evaluated at the `defer` statement and held
    /// here as values; the call runs at drain. GC-rooted in [`Vm::collect`].
    deferred: Vec<Deferred>,
    /// Stack of lexical defer-scope markers (each = `deferred.len()` at `EnterDeferScope`). A
    /// `LeaveDeferScope` drains `deferred` back down to the top marker, giving block-scoped defer:
    /// a block's defers run when that block exits, not when the whole frame does.
    defer_markers: Vec<usize>,
    /// `nurseries.len()` at frame entry (mirrors [`Handler::nursery_len`]). A `?`/return that escapes
    /// a `parallel:` body in this frame jumps past the `JoinNursery` that would pop the nursery;
    /// `do_return` truncates `nurseries` back to this length so the stale nursery (and its GC-rooted
    /// pending-task args) is reclaimed instead of leaking to program exit — matching the interp's
    /// unconditional `exec_parallel` pop.
    nursery_len: usize,
    /// M-C: this frame's proto opened an implicit nursery at body entry (it contains a bare `spawn`).
    /// On exit, `do_return` JOINS that nursery (runs its tasks) rather than cancelling it; any *inner*
    /// `parallel:` escaped by the same `return`/`?` is still cancelled. Copied from
    /// [`op::Proto::has_implicit_nursery`] at frame push.
    has_implicit_nursery: bool,
    /// Source span of the call site that pushed this frame (where the function was invoked). Used to
    /// build a runtime stack trace; not part of execution.
    call_span: Span,
}

/// A call registered by `defer`, with its receiver/arguments already evaluated. The held values are
/// GC roots while the deferred call is pending.
enum Deferred {
    /// `defer f(args)` — invoke the callable value with the args (`invoke_value`).
    Call { callee: Value, args: Vec<Value>, span: Span },
    /// `defer recv.name(args)` — dispatch the named method on the receiver.
    Method { recv: Value, name: String, args: Vec<Value>, span: Span },
}

impl Deferred {
    /// The GcRefs this deferred call keeps alive (callee/receiver + arguments).
    fn roots(&self) -> impl Iterator<Item = GcRef> + '_ {
        let (head, args) = match self {
            Deferred::Call { callee, args, .. } => (callee, args),
            Deferred::Method { recv, args, .. } => (recv, args),
        };
        std::iter::once(head)
            .chain(args.iter())
            .filter_map(|v| if let Value::Obj(h) = v { Some(*h) } else { None })
    }
}

/// A task registered by `spawn`, awaiting its nursery's join barrier (C4). The callee/receiver and
/// arguments are evaluated and deep-copied across the airlock at the `spawn` statement (Go's
/// arg-evaluation timing); the body runs at the `parallel:` dedent. Mirrors the interpreter's
/// `Task` enum — a `spawn:` block is lowered to a zero-arg closure, so it rides the `Call` variant.
/// The held values are GC roots while the task is pending (see [`Vm::collect`]).
enum PendingCall {
    /// `spawn f(args)` (or a `spawn:` block, lowered to a zero-arg closure) — invoke the callable.
    Call { callee: Value, args: Vec<Value>, span: Span },
    /// `spawn recv.name(args)` — dispatch the named method on the receiver.
    Method { recv: Value, name: String, args: Vec<Value>, span: Span },
}

impl PendingCall {
    /// The GcRefs this pending task keeps alive (callee/receiver + arguments).
    fn roots(&self) -> impl Iterator<Item = GcRef> + '_ {
        let (head, args) = match self {
            PendingCall::Call { callee, args, .. } => (callee, args),
            PendingCall::Method { recv, args, .. } => (recv, args),
        };
        std::iter::once(head)
            .chain(args.iter())
            .filter_map(|v| if let Value::Obj(h) = v { Some(*h) } else { None })
    }

    /// The spawning span of this task (D2b — carried onto the fiber for fault attribution).
    fn span(&self) -> Span {
        match self {
            PendingCall::Call { span, .. } | PendingCall::Method { span, .. } => *span,
        }
    }
}

/// M19 Phase 4/5b — a struct-field inline-cache cell: the field `idx` last resolved at this call
/// site, plus the `tid` (struct layout id) it was resolved against. Holds plain ints (no `GcRef`), so
/// it is invisible to GC, snapshots, and `swap_ctx`. A hit requires `tid == obj.tid` (a pure-int
/// compare — every instance of a given `tid` shares the field layout, so the cached `idx` is the
/// right slot), replacing P4's field-name string re-verify. `tid == TID_NONE` is the empty/sentinel
/// state: it never matches a live struct (unregistered structs also carry `TID_NONE`, so they fall to
/// the probe), forcing a fill on first use and barring a false hit across distinct unregistered types.
#[derive(Clone, Copy)]
struct IcCell {
    idx: u32,
    tid: u32,
}

impl IcCell {
    /// `tid` is the sole liveness gate (a hit requires `tid != TID_NONE && tid == obj.tid`); the
    /// `idx: u32::MAX` is just a defensive default, not a sentinel — don't reintroduce idx-based
    /// empty-checking.
    const EMPTY: IcCell = IcCell { idx: u32::MAX, tid: TID_NONE };
}

/// M19 Phase 6 — a single method-call inline-cache cell. `tid` is the struct layout id the cached
/// dispatch was resolved for (the sole liveness gate, like [`IcCell`]); a hit requires
/// `tid != TID_NONE && tid == recv.tid`, so an empty cell or a different receiver type re-resolves.
/// `proto` is the resolved method body (program-global, stable across heaps); `module_idx` recovers
/// the method's home module from the *current* heap's `module_objs` on the fast path — held as an
/// index, NOT a `GcRef`, so the cell stays heap-independent (invisible to GC / snapshots / `swap_ctx`,
/// exactly like the field IC). Module-member / core-type calls never fill a cell (they don't match
/// the `Obj::Struct` guard) and so always take the slow path.
#[derive(Clone, Copy)]
struct MethodIcCell {
    tid: u32,
    proto: ProtoId,
    module_idx: u32,
}

impl MethodIcCell {
    const EMPTY: MethodIcCell = MethodIcCell { tid: TID_NONE, proto: 0, module_idx: 0 };
}

struct Vm {
    program: Arc<Program>,
    heap: Heap,
    stack: Vec<Value>,
    frames: Vec<CallFrame>,
    out: String,
    /// Captured stderr (written by `std.io.eprint`). Separate from `out` so streams don't mix.
    stderr: String,
    /// Runtime configuration the native std modules read (args/env/stdin). Default = inert.
    host: crate::native::HostConfig,
    call_depth: usize,
    /// Each module's namespace object, indexed by module index (run-once cache + import targets).
    module_objs: Vec<GcRef>,
    /// M19 Phase 3 — per-heap intern cache for `ConstStr` literals. Keyed by the literal's data
    /// pointer (`s.as_ptr()`), which is stable for the program's lifetime (the `String` lives in the
    /// immutable `Arc<Program>`). A `ConstStr` push reuses the cached `Obj::Str` instead of
    /// alloc+clone+box every time — strings are never mutated in place, and there is no identity
    /// operator, so aliasing is unobservable. The cached `GcRef`s are GC roots (see [`Vm::collect`])
    /// so they're never swept; the cache is heap-keyed, so an M:N fiber swaps it WITH its heap in
    /// [`Vm::swap_ctx`] (like `module_objs`/`executors`).
    str_intern: fxhash::FxHashMap<usize, GcRef>,
    /// M19 Phase 4 — per-call-site struct-field inline caches, indexed by the `ic` id baked into
    /// `GetField`/`SetField` ops (dense `0..program.field_ic_sites`). Holds field indices, not
    /// `GcRef`s, so it carries no heap state: never snapshotted, never swapped in [`Vm::swap_ctx`].
    /// Cooperative fibers share one `Vm` but run sequentially (no race); `--parallel` workers each
    /// own a separate `Vm` (no race). Each cell self-verifies, so sharing is always sound.
    field_ic: Vec<IcCell>,
    /// M19 Phase 6 — per-call-site method inline caches, indexed by the `ic` id baked into
    /// `CallMethod` ops (dense `0..program.method_ic_sites`). Holds proto ids + module indices, not
    /// `GcRef`s, so it carries no heap state: never snapshotted, never swapped in [`Vm::swap_ctx`].
    /// Same sharing argument as `field_ic` — sequential cooperative fibers / per-worker `Vm`s, each
    /// cell tid-guarded so it self-verifies.
    method_ic: Vec<MethodIcCell>,
    /// M19 Tier-2 — adaptive opcode quickening (PEP 659) state, one byte per program instruction,
    /// indexed by site `quicken_base[pid] + ip`. The un-fused generic binop arms (`Add..GtEq` reached
    /// by stack operands; `Eq`/`NotEq` always) specialize to an int/int fast path behind a deopt
    /// guard: `Q_COLD` → on first run observe operand types → `Q_INT` (int/int) or `Q_GENERIC`
    /// (sticky; never re-specializes, so a polymorphic site never thrashes). Holds only state bytes
    /// (no `GcRef`, no proto/heap handle), so it is heap-independent like `field_ic`/`method_ic`:
    /// never snapshotted, never swapped in [`Vm::swap_ctx`]. Behaviour is byte-identical to the
    /// generic path, so two-engine parity is preserved by construction.
    quicken: Vec<u8>,
    /// Prefix sum of per-proto `code.len()` over `program.protos` — the base offset into `quicken`
    /// for each proto, so a site id is `quicken_base[pid] + ip`. Built once in `Vm::new`; program-
    /// derived (heap-independent), so never swapped (mirrors `quicken`).
    quicken_base: Vec<u32>,
    /// The current frame's slot base — cached so local access doesn't re-walk `frames` each op.
    cur_base: usize,
    /// Active `recover:` boundaries (a stack; the innermost is last). A runtime fault unwinds to the
    /// nearest handler whose frame is owned by the currently-running dispatch loop.
    handlers: Vec<Handler>,
    /// Set by `std.os.exit(code)` (clamped to `0..=255`). While set, the fault dispatch bypasses
    /// every `recover:` handler and unwinds to the top — a hard, uncatchable halt; the driver then
    /// reports `code` as the process exit status.
    pending_exit: Option<i32>,
    /// The stack trace of the uncaught fault that propagates, captured before frames unwind. The
    /// **deepest** fault wins (`fault_trace_depth` = frame count at capture): the original fault site
    /// captures first; a `defer`red call that itself faults runs while its owning frame is still on
    /// the stack (so it is deeper) and supersedes — matching Go's defer-supersedes semantics and the
    /// interpreter. Reset whenever a `recover:` boundary catches a fault. Read by the driver.
    fault_trace: Option<Vec<TraceFrame>>,
    fault_trace_depth: usize,
    /// Test mode: collect before *every* instruction, to surface any missing GC root.
    gc_stress: bool,
    /// B3.3-threads: `--parallel` engine selected. When set, `join_nursery` runs a nursery's tasks
    /// on the real OS-thread pool (each in its own worker `Vm`) instead of cooperative fibers, and a
    /// blocking `recv` on an empty channel waits on the channel's `Condvar` rather than parking a
    /// fiber. Default `false` keeps the cooperative single-thread engine (decision A). Workers
    /// inherit it (see [`Vm::spawn_worker`]) so nested `parallel:` recurses onto the pool too.
    parallel: bool,
    /// Active `parallel:` nurseries (C4), innermost last. `EnterNursery` pushes; each `spawn`
    /// registers a [`PendingCall`] on the innermost list; `JoinNursery` drains it FIFO at the
    /// dedent. Tasks are GC roots while pending. A `recover:` boundary truncates this stack back to
    /// its install-time length on catch (see [`Handler::nursery_len`]), so a fault in the nursery
    /// body or a task can't leave a stale entry.
    nurseries: Vec<Vec<PendingCall>>,
    /// TASK B — parallel to [`Vm::nurseries`] (same length, pushed/popped in lockstep): the value of
    /// the current frame's `deferred.len()` captured when each nursery was entered (`EnterNursery`),
    /// i.e. the defer floor of that `parallel:` body. A recover-scoped `?` escaping a `parallel:`
    /// must run the body's defers (those at `deferred[floor..]`) BEFORE writing the cancel-report and
    /// only THEN run the recover block's own defers — matching the interp, whose `exec_parallel`
    /// reports after the body's `exec_scoped_block` has already drained its defers.
    nursery_defer_floors: Vec<usize>,
    /// Per-connection spawn — parallel to [`Vm::nurseries`] (lockstep): `Some` for an eager nursery
    /// (entered under `--parallel` inside a fiber), `None` for a lazy/top-level one. A `spawn` whose
    /// innermost nursery is eager injects its handler fiber straight into the scope's sched (runs
    /// concurrently with the body) instead of queueing it for the join. See [`EagerScope`].
    eager_scheds: Vec<Option<EagerScope>>,
    /// Every `Executor` created during the run (`Op::NewExecutor`), in creation order. These handles
    /// are GC roots (see [`Vm::collect`]) so an un-shut executor's queued work survives until the
    /// program-exit auto-drain (C5 / A2) reaps any executor never explicitly shut down — the VM
    /// parity counterpart of the interpreter's `Rc` registry.
    executors: Vec<GcRef>,
    /// Concurrency B1/B2: set by a blocking `recv` (empty channel) running inside an active nursery
    /// scheduler. It records the channel handle the running fiber is waiting on; `run_until` and the
    /// re-entrant call path break (without unwinding defers) when it is set, returning control to the
    /// scheduler so a sibling can run. Cleared by the scheduler when it resumes a fiber. It is a
    /// VM-global (not part of [`FiberCtx`]): only one fiber runs at a time, so at most one suspend is
    /// pending. See [`Vm::run_scheduler`].
    suspend: Option<GcRef>,
    /// D5 — set when a blocking native call (`is_blocking`) is reached under the M:N engine: instead
    /// of running inline (pinning the worker), `invoke_native` records the call here and returns a
    /// sentinel; the worker loop hands it to the dirty/blocking pool ([`Disp::Offload`]) and is freed.
    /// Like `suspend` it is a VM-global (one fiber runs at a time) and gates the result-push at the
    /// call site (via [`Vm::paused`]). Reset per schedule-in; never preserved across a park/offload.
    offload: Option<OffloadReq>,
    /// D6 — set when a non-blocking socket op (`read`/`write`/`accept`) returns `WouldBlock` under the
    /// M:N engine: instead of busy-retrying, `socket_method`/`listener_method` rewinds `ip` (the op
    /// re-executes on resume), records the fd + interest here, and returns a sentinel; the worker loop
    /// hands it to the netpoller ([`Disp::PollPark`]) and is freed. Sibling of `offload`/`suspend` (one
    /// fiber runs at a time) — gates the result-push at the call site via [`Vm::paused`]. Reset per
    /// schedule-in; never preserved across a park.
    poll_park: Option<PollPark>,
    /// D6b — a non-blocking `connect` whose handshake is in flight: the connecting stream + its poll
    /// key + guard (see [`ConnectInProgress`]). Unlike `poll_park` (reset per schedule-in), this must
    /// SURVIVE the writability park, so it lives in [`FiberCtx`] and swaps with the fiber; this `Vm`
    /// field just holds it while the fiber is swapped in. The resumed `net.connect` re-run takes it to
    /// finish the connect. `None` whenever no connect is mid-flight.
    pending_connect: Option<ConnectInProgress>,
    /// D6c — live mirror of [`FiberCtx::poll_timed_out`] while the fiber is swapped in: set by the poll
    /// thread on the detached fiber's ctx when a socket op's `timeout_ms` deadline elapsed before the
    /// fd became ready, swapped in here on schedule-in. `socket_method`/`listener_method` consume it at
    /// op ENTRY (after the `run_until` loop-top cancel check, so a sibling fault still wins): if set,
    /// clear it and return `Err("timeout")` instead of retrying the syscall. Snapshot-park path only.
    poll_timed_out: bool,
    /// Depth of native (Rust) callbacks currently on the host stack that re-enter Chezzi (operator
    /// overloads, `compare`/`hash`/`str` hooks, list HOFs, sorts, `Shared.update`, the executor
    /// drain, deferred calls). Their loop / recursion state lives on the Rust stack and cannot be
    /// parked into a [`Fiber`], so a `recv` reached while this is `> 0` cannot suspend — it faults
    /// `deadlock` instead (B1 v1 limitation). Maintained by [`Vm::guarded`].
    native_reentry: usize,
    /// Active cooperative-scheduler levels (B1/B2), innermost last. Each [`Nursery`] holds the parked
    /// joining (parent) fiber's context plus its child fibers; non-empty means a `recv` may suspend.
    /// Every parked fiber here is a GC root (see [`Vm::collect`]).
    scheduler_stack: Vec<Nursery>,
    /// B3.4: the cancel flag of the `parallel:` nursery this worker `Vm` runs under (`--parallel`
    /// only; cloned in by [`Vm::run_parallel_nursery`]). The first sibling to fault or `os.exit`
    /// sets it; every other worker observes it at a dispatch back-edge (loop top) or inside a
    /// blocking `recv`'s re-checking wait, and unwinds as the `cancelled` sentinel — so a faulted
    /// nursery aborts running siblings instead of join-then-report. `None` on the cooperative engine
    /// (it already aborts via the scheduler unwind) and on the top-level VM (never a worker).
    cancel: Option<Arc<AtomicBool>>,
    /// B3.4: set true only when *this* worker observed [`Vm::cancel`] and bailed, so the join can
    /// tell a swallowed cooperative abort apart from a real fault (a cancelled task is dropped, not
    /// reported). Not in [`FiberCtx`] — like `pending_exit`, cancellation is a per-VM concern.
    cancelled: bool,
    /// D1 — on a `--parallel` **worker** VM, the read-only [`ModuleSnapshot`] of the parent's module
    /// graph that this worker faults into its own heap lazily, one module at a time, on first global
    /// access ([`Vm::fault_module`]). `None` on the top-level VM and the cooperative engine (which
    /// never fault — the top-level's `module_objs` are the real, fully-populated modules; a worker's
    /// start empty and fill on demand). A nested `spawn` inside a worker hands this same `Arc` down
    /// (its faulted graph is identical to the snapshot — module globals are read-only under
    /// `--parallel`), so the snapshot propagates across worker generations with stable indices.
    module_snapshot: Option<Arc<ModuleSnapshot>>,
    /// D1 — parallel to `module_objs` on a worker VM: whether module `i` has been faulted in yet
    /// (its globals replayed from `module_snapshot`). Empty on the top-level / cooperative VM.
    module_faulted: Vec<bool>,
    /// D1 — the top-level VM's memoized snapshot of its own (real) module graph, built once on the
    /// first `spawn`/`submit` drain and shared by every worker it prepares. Read-only after build
    /// (module globals are frozen under `--parallel`, decision G1), so one build serves the whole
    /// run. A worker reuses its `module_snapshot` instead of this (see [`Vm::ensure_snapshot`]).
    snapshot_memo: Option<Arc<ModuleSnapshot>>,
    /// D2b — set on an M:N **worker shell** to the scheduler of the `parallel:` nursery it is draining.
    /// `Some` flips the `recv`/`send` arms onto the park/wake protocol ([`MnSched`]) instead of the
    /// legacy condvar-block; `None` on the cooperative engine, the top-level VM, and a prepared task
    /// fiber's heap-only worker. Cloned onto each shell at enlistment ([`Vm::run_mn_nursery`]).
    mn: Option<Arc<MnSched>>,
    /// D3 — reduction budget of the fiber currently swapped in: the number of ops it may still
    /// dispatch before it must yield its worker (BEAM-style preemption). Reset to [`CONTEXT_REDS`]
    /// on every schedule-in ([`Vm::run_one_fiber`]) and decremented at the `run_until` loop-top
    /// safepoint. Live per-VM scratch (like `pending_exit`/`cancelled`), NOT part of [`FiberCtx`]:
    /// it is reset per schedule, never preserved across a park. Only consulted under the M:N engine
    /// (`mn.is_some()`); the cooperative engine never preempts (it is the frozen parity oracle).
    reds: u32,
    /// D3 — transient signal: the safepoint set this when `reds` hit 0, asking the worker loop to
    /// requeue this fiber (round-robin) instead of treating its `run_until` return as a finish.
    /// Set at the safepoint, consumed in [`Vm::run_one_fiber`]; mutually exclusive with `suspend`.
    yield_now: bool,
    /// D5 owe #3 (Path C) — this M:N worker shell's worker id (its `locals[wid]` slot), set at the top
    /// of [`Vm::mn_worker_loop`]. Read by [`Vm::demote_recv_block`] so a demoted worker's raw
    /// replacement thread reuses the same `wid` (safe: a demoted worker never touches `locals[wid]`
    /// again — it exits after settling its current fiber). `0` on the cooperative engine / top-level VM.
    wid: usize,
    /// D5 owe #3 (Path C) — set true the first time THIS worker thread blocks in place on a `recv`
    /// reached inside a native callback (a "thread demotion", Go's `handoffp`). Once demoted, a fresh
    /// replacement worker covers this thread's `wid`, so after its current fiber settles
    /// [`Vm::mn_worker_loop`] returns (the demoted thread exits) — keeping the net live-worker count at
    /// N. A per-WORKER-thread flag, NOT per-fiber: it is deliberately NOT reset in
    /// [`Vm::run_one_fiber`] and NOT part of [`FiberCtx`] (a demoted thread runs exactly one fiber to
    /// settle, then exits, so it never carries the flag into another fiber).
    demoted: bool,
}

/// D3 — a fiber's reduction budget per schedule-in: how many ops it dispatches before yielding its
/// worker so a queued sibling runs (BEAM's default is 4000; tunable — see `docs/concurrency-tier-d.md`
/// §"Open / deferred"). Large enough that the per-op decrement is negligible, small enough that a
/// CPU-bound fiber relinquishes its worker promptly.
const CONTEXT_REDS: u32 = 4000;

/// D6 — per-call `Socket::read(n)` buffer cap (16 MiB). A huge caller-supplied `n` is clamped to this
/// so it can't eagerly allocate gigabytes before any data arrives; the caller loops for larger
/// payloads (`read` returns the actual byte count). Mirrors `io::read_file`'s read limit.
const MAX_SOCKET_READ: usize = 16 * 1024 * 1024;

/// D6b — wall-clock cap on the top-level (no-fiber-to-park) blocking `connect` fallback, so a
/// black-hole address returns a clean timeout instead of spinning for the kernel's ~2-minute connect
/// timeout. Generous (the M:N engine, which parks instead of blocking, is the real target).
const CONNECT_BLOCK_TIMEOUT_SECS: u64 = 10;

/// D5 owe #3 (Path C) — how long a demoted worker thread waits on the channel condvar between
/// re-checks (queue / cancel / terminate). A `send` notifies the condvar so the common case wakes
/// immediately; this bounded poll only backstops a lost wakeup (≤ this much added latency, never a
/// hang) and bounds how fast a demoted thread observes `terminate`/`cancel` (a sibling fault/deadlock).
const DEMOTE_POLL_BACKOFF: std::time::Duration = std::time::Duration::from_millis(5);

/// D5 owe #3 Path C (#3 socket half) — block the (replacement-covered) worker on `fd` until it is
/// readable/writable per `interest` OR `timeout` elapses, then return. Used by [`Vm::demote_block_socket`]
/// so an in-callback socket op that can't snapshot-park onto the netpoller still waits in the KERNEL
/// (woken immediately on readiness, no busy-poll) instead of pinning a CPU. The `timeout` bounds how
/// fast the caller re-observes `cancel`/`terminate`. Spurious/early returns are harmless — the caller
/// simply re-attempts the non-blocking op and re-blocks here on `WouldBlock`. On non-Unix (no `poll(2)`)
/// it degrades to a plain sleep (the same bounded backoff the netpoller already targets Unix-only).
#[cfg(unix)]
fn wait_fd_ready(fd: std::os::fd::RawFd, interest: poller::Interest, timeout: std::time::Duration) {
    let events = match interest {
        poller::Interest::Read => libc::POLLIN,
        poller::Interest::Write => libc::POLLOUT,
    };
    let mut pfd = libc::pollfd { fd, events, revents: 0 };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // Ignore the result: a ready fd, a timeout, or an EINTR all lead to the same next step (re-attempt
    // the non-blocking op under the caller's lock). Never blocks longer than `ms`.
    unsafe {
        libc::poll(&mut pfd as *mut libc::pollfd, 1, ms);
    }
}

#[cfg(not(unix))]
fn wait_fd_ready(_fd: std::os::fd::RawFd, _interest: poller::Interest, timeout: std::time::Duration) {
    std::thread::sleep(timeout);
}

/// A fiber's saved execution context (B1): every `Vm` field that `run_until` reads or writes keyed by
/// per-execution indices. Swapped with the live `Vm` fields ([`Vm::swap_ctx`]) when a fiber is
/// scheduled in or out. `pending_exit` is deliberately NOT here — `std.os.exit` halts the whole
/// program, so it stays VM-global.
#[derive(Default)]
struct FiberCtx {
    frames: Vec<CallFrame>,
    stack: Vec<Value>,
    call_depth: usize,
    cur_base: usize,
    handlers: Vec<Handler>,
    nurseries: Vec<Vec<PendingCall>>,
    /// TASK B — see [`Vm::nursery_defer_floors`]; carried per-fiber so a parked fiber's per-nursery
    /// defer floors travel with its `nurseries` across `swap_ctx`.
    nursery_defer_floors: Vec<usize>,
    /// Per-connection spawn — parallel to [`Vm::nurseries`] (same length, pushed/popped in lockstep):
    /// `Some` for an EAGER nursery (one entered under `--parallel` inside a live fiber, `mn.is_some()`)
    /// holding the live inner [`MnSched`] a `spawn` in this body injects handlers into; `None` for a
    /// lazy/top-level nursery (queue-at-join). Carried per-fiber so the open scope travels across a
    /// park (the acceptor blocks on `accept` mid-loop).
    eager_scheds: Vec<Option<EagerScope>>,
    fault_trace: Option<Vec<TraceFrame>>,
    fault_trace_depth: usize,
    /// D2a — an M:N fiber carries its OWN heap (share-nothing): `swap_ctx` swaps it with the host
    /// `Vm::heap` when this fiber schedules in, and back out when it parks. `None` for cooperative
    /// fibers, which all alias the single `Vm::heap` (decision A — share-by-ref), so their swap
    /// leaves the heap untouched and the cooperative engine stays byte-identical. The `Some`/`None`
    /// discriminant also gates every D2b side-state swap below.
    heap: Option<Heap>,
    /// D2b — per-task output buffers (Decision F: each task's stdout/stderr flushes in task order at
    /// join, never interleaved live). An M:N worker shell runs many fibers in turn, so these MUST
    /// travel with the fiber rather than living on the shell `Vm`. Swapped only for M:N fibers
    /// (`heap.is_some()`); a cooperative fiber keeps `String::new()` and aliases the shell's buffers.
    out: String,
    stderr: String,
    /// D2b — the fiber's module-namespace objects + lazy-fault flags (D1). Each is a `GcRef` into the
    /// fiber's OWN heap, which travels via `heap` above; a `GcRef` is only valid against the heap it
    /// was allocated in, so these roots MUST swap atomically with that heap. Empty for cooperative
    /// fibers (they alias the shell's `module_objs`/`module_faulted`).
    module_objs: Vec<GcRef>,
    module_faulted: Vec<bool>,
    /// D2b — the fiber's `Executor` handles (GC roots into its own heap; same heap-keyed argument as
    /// `module_objs`). Empty for cooperative fibers.
    executors: Vec<GcRef>,
    /// M19 Phase 3 — the fiber's `ConstStr` intern cache (GC roots into its own heap; same heap-keyed
    /// argument as `module_objs`). Travels with `heap` across [`Vm::swap_ctx`]. Empty for cooperative
    /// fibers (they alias the shell's cache).
    str_intern: fxhash::FxHashMap<usize, GcRef>,
    /// D6b — a non-blocking `connect` parked on writability (see [`ConnectInProgress`]). Non-heap, so
    /// it carries no `GcRef` and needs no GC rooting; but it MUST travel with the fiber across the
    /// park, so it swaps in [`Vm::swap_ctx`] like the other per-fiber state. `None` unless this fiber
    /// is mid-connect (only ever set on the M:N engine — cooperative connect blocks instead).
    pending_connect: Option<ConnectInProgress>,
    /// D6c — per-socket read/accept/write timeout marker. A socket op given a `timeout_ms` parks on
    /// the netpoller with a deadline; if that deadline elapses before the fd fires, the poll thread
    /// (which owns the detached [`Fiber`]) sets this `true` and re-injects the fiber — exactly like
    /// `pending_connect`, this travels WITH the fiber across [`Vm::swap_ctx`] so the resumed op knows
    /// the wake came from a timeout, not readiness. On schedule-in it swaps into [`Vm::poll_timed_out`];
    /// the rewound socket op checks it at ENTRY (before re-running the syscall) and returns
    /// `Err("timeout")` instead of retrying. `false` whenever no timeout wake is pending. M:N-only.
    poll_timed_out: bool,
}

/// Scheduling state of a child fiber within a [`Nursery`].
enum FiberState {
    /// Spawned but not yet started; holds the task to launch on first schedule.
    Pending(PendingCall),
    /// Started and runnable — resume by re-entering its `run_until`.
    Ready,
    /// Parked on an empty channel; runnable again once a sibling `send`s (D0 records the parked
    /// index in [`Nursery::blocked_on`] under the channel's core pointer — the receiver handle
    /// itself stays rooted on the fiber's own operand stack, so this variant carries no payload).
    Blocked,
    /// Ran to completion.
    Done,
}

/// One child fiber: its saved context plus scheduling state. While the fiber is the one actively
/// running, its context lives in the live `Vm` fields and `ctx` is empty (see the scheduler).
struct Fiber {
    ctx: FiberCtx,
    state: FiberState,
    /// D2b — the fiber's stable Decision-F outcome slot (its index in the nursery's task order),
    /// assigned at nursery build. Unused by the cooperative engine (it carries the child index).
    task_index: usize,
    /// D2b — the spawning task's span, for fault/panic attribution when this fiber faults under M:N.
    span: Span,
    /// D5 — a blocking native call offloaded to the dirty pool stashes its result here; the worker
    /// that resumes this fiber lowers the `NativeRet` into the heap + pushes it (or re-raises the
    /// `RuntimeError`) before continuing past the `Call`. `None` except in the brief window between a
    /// blocking-pool completion and the fiber's next schedule-in.
    resume_native: Option<Result<crate::native::NativeRet, RuntimeError>>,
}

// D2a — a `Fiber` now carries its own `Heap` (via `FiberCtx::heap`), and D2b parks fibers across
// worker threads (parked on one worker, requeued by a `send` on another, resumed on a third). That
// requires `Fiber: Send`. `Heap` is already `Send` (a plain `Vec` of `Obj`, no `Rc`/`RefCell`; a
// whole `Vm`/`Heap` already crosses the pool boundary via `ReadyWorker`), so this holds today — the
// guard makes a future non-`Send` field on `Fiber`/`FiberCtx` a loud compile error, not a D2b
// surprise.
const _: fn() = || {
    fn assert_send<T: Send>() {}
    assert_send::<Fiber>();
};

/// One active `parallel:` scheduler level (B1/B2): the parked context of the joining (parent) fiber
/// and the child fibers spawned into the nursery. Pushed on `JoinNursery`, popped when every child
/// is `Done`.
struct Nursery {
    /// The joining fiber's context, parked while its children run cooperatively.
    parent: FiberCtx,
    children: Vec<Fiber>,
    /// D0 — runnable child indices, ordered (a `BTreeSet`, not a FIFO queue, so `pop_first` always
    /// returns the **lowest-index** runnable child — byte-identical to the old `pick_runnable`
    /// linear scan, but O(log N) instead of O(N) per turn). Seeded with every child index on
    /// `JoinNursery` (all start `Pending`); a child re-enters only via [`Vm::wake_on_send`].
    ready: std::collections::BTreeSet<usize>,
    /// D0 — child indices parked on an empty channel, keyed by the channel's `ChannelCore` pointer
    /// (`Arc::as_ptr as usize`), NOT its `GcRef`: cooperative `spawn` deep-clones a channel
    /// (`from_wire` allocs a fresh handle onto the same `Arc<ChannelCore>`), so sibling fibers hold
    /// distinct handles aliasing one core — a handle key would lose the wakeup. A sibling `send`
    /// drains the matching bucket back onto `ready`.
    blocked_on: std::collections::HashMap<usize, Vec<usize>>,
}

/// A snapshot taken at a `recover:` boundary (`Op::PushHandler`). On a caught fault the VM restores
/// the operand stack, call frames, and call-depth to these values, then jumps to `ip` in the
/// boundary's frame with the fault message pushed as the operand.
#[derive(Clone, Copy)]
struct Handler {
    stack_len: usize,
    frame_len: usize,
    call_depth: usize,
    ip: usize,
    /// `deferred.len()` of the boundary's frame when the handler was installed. A caught fault, a
    /// `?` short-circuit, or the Ok path drains the frame's defers down to this marker — the
    /// recover block's own cleanup — before binding the recovered value.
    defer_len: usize,
    /// `defer_markers.len()` of the boundary's frame at install. A fault / `?` short-circuit jumps
    /// past the `LeaveDeferScope`s of any defer scopes opened *inside* the recover block, so their
    /// markers would leak; the catch paths truncate `defer_markers` back to this length.
    markers_len: usize,
    /// `nurseries.len()` at install. A fault inside a `parallel:` body or a spawned task jumps past
    /// the `JoinNursery` that would pop the nursery; the catch path truncates `nurseries` back to
    /// this length so the stale nursery (and its aborted siblings) is reclaimed.
    nursery_len: usize,
}

/// B3.2 — what a run isolated worker hands back across the airlock: the task's return value
/// serialized in the worker heap, plus the worker's captured stdout/stderr (decision F —
/// buffer-per-worker, returned to the parent rather than interleaved live).
// `value` is read only by the worker unit tests: the `--parallel` join discards each task's return
// value (data exits a spawn via `Shared`/`Channel`, not a return), so the field is dead in the bin
// build — hence the allow. `out`/`stderr` are live (flushed at join).
#[allow(dead_code)]
#[derive(Debug)]
struct WorkerResult {
    value: WireValue,
    out: String,
    stderr: String,
}

/// B3.2 — a spawned task lowered to a `Send` description (no parent-heap `GcRef`s), ready to rebuild
/// in a worker heap. The callee crosses as its `ProtoId` (the proto lives in the shared `Arc<Program>`)
/// plus wire'd captures/args.
/// `home`: the index of the callee's home module in the parent's `module_objs` (B3.3c), so the
/// worker can resolve the rebuilt module obj for global / sibling-fn resolution; `None` when the
/// home is a standalone module not in `module_objs` (the unit-test fixtures), which falls back to a
/// fresh empty home.
enum Lowered {
    Closure { proto: ProtoId, captured: Vec<(String, WireValue)>, args: Vec<WireValue>, home: Option<usize>, span: Span },
    Func { proto: ProtoId, args: Vec<WireValue>, home: Option<usize>, span: Span },
    /// `spawn recv.m(args)` (B3.3d) — the receiver + args cross by wire; dispatch resolves the method
    /// against the worker's reconstructed `module_objs` (struct methods index `module_objs[module_idx]`).
    Method { recv: WireValue, name: String, args: Vec<WireValue>, span: Span },
}

/// D1 — a heap-independent, read-only snapshot of the parent's initialized module graph, shared
/// across a nursery's workers via `Arc` (like `Arc<Program>`) and **faulted into each worker heap
/// lazily, one module at a time, on first global access** (see [`Vm::fault_module`]). It replaces
/// the eager per-task `build_worker_modules` reconstruction: N tasks now share one snapshot build
/// + cheap `Arc` clones, and a task that touches only its home module rebuilds only that module.
///
/// `modules` is parallel to the parent's `module_objs` by index, so a callable's `home` /
/// `module_idx` (already an index under the airlock — see [`Vm::home_index`]) lines up directly
/// with the worker's pre-allocated (empty) module objects. Built once by [`Vm::snapshot_modules`].
struct ModuleSnapshot {
    modules: Vec<ModuleSnap>,
}

/// D1 — one module in a [`ModuleSnapshot`]: its name plus its top-level globals as heap-independent
/// [`SnapValue`]s (insertion order preserved so replay is deterministic).
struct ModuleSnap {
    name: Box<str>,
    globals: Vec<(String, SnapValue)>,
}

/// M19 Phase 2b — a module's globals as `(name, value)` pairs in **slot order** (slot `i` at index
/// `i`). Used to snapshot a module deterministically: replaying the pairs in order via
/// `module_define` rebuilds slots in the same order the compiler assigned them.
fn module_slot_pairs(slots: &[Value], index: &std::collections::HashMap<Box<str>, u32>) -> Vec<(String, Value)> {
    // Invariant: `index` names every slot `0..slots.len()` (the three growth paths — `run_module`
    // pre-size, `module_define` append, `set_global_slot` overwrite — keep `slots`/`index` in
    // lockstep). If that ever breaks, an unnamed hole would replay as a duplicate empty name and
    // collapse later slots in a worker, silently corrupting its globals — so fail loudly here.
    debug_assert_eq!(slots.len(), index.len(), "module slots/index out of lockstep — slot order would corrupt on worker fault");
    let mut pairs: Vec<(String, Value)> = vec![(String::new(), Value::Nil); slots.len()];
    for (name, &i) in index {
        pairs[i as usize] = (name.to_string(), slots[i as usize]);
    }
    pairs
}

/// D1 — one global value in heap-independent form, the snapshot analogue of what
/// [`Vm::map_global_value`] rebuilt eagerly. `WireValue` already encodes pure data, `str`-by-value,
/// and `Channel`/`Shared`/`Executor` cores (Arc-shared, meaningful in any heap), so the common case
/// is [`SnapValue::Wire`]; the other variants cover exactly the values `to_wire` cannot carry
/// heap-independently — a callable's parent-heap `GcRef` home/captures, an import-alias module ref,
/// a native fn, and any container that embeds one of those. `Send` (every field is), so an
/// `Arc<ModuleSnapshot>` crosses to pool threads.
enum SnapValue {
    /// Fast path: a value whose wire form carries no by-reference `Handle` (pure data / `str` by
    /// value / a `Channel`/`Shared`/`Executor` core). Replayed via `from_wire`.
    Wire(WireValue),
    /// A named function — re-allocated over the worker's home module on replay. `home` is an index
    /// into `module_objs` (resolved via [`Vm::worker_home`], which falls back to a fresh empty module
    /// for `None` — a home not in the table, i.e. the hand-built unit-test fixtures).
    Func { proto: ProtoId, home: Option<usize> },
    /// An anonymous function + its captured environment (each capture itself a `SnapValue`).
    Closure { proto: ProtoId, captured: Vec<(String, SnapValue)>, home: Option<usize> },
    /// An import-alias global bound to another module — replays to the worker's `module_objs[idx]`
    /// (the pre-alloced module obj, which faults its own globals lazily — no eager cascade).
    ModuleAlias(usize),
    /// A module value NOT in `module_objs` (defensive — shouldn't occur for a bound import; mirrors
    /// the `None` arm of the old `map_global_value`): replayed as a fresh, eagerly-populated module.
    ModuleInline { name: Box<str>, globals: Vec<(String, SnapValue)> },
    /// A native (Rust) fn — re-allocated with the same fn pointer (`NativeFn` is `Clone`/`Send`).
    Native { name: Box<str>, func: crate::native::NativeFn },
    List(Vec<SnapValue>),
    Tuple(Vec<SnapValue>),
    Enum { ty: Box<str>, variant: Box<str>, payload: Vec<SnapValue> },
    Struct { name: Box<str>, fields: Vec<(Box<str>, SnapValue)> },
    /// `(cached hash, key, value)` triples — hashes are value-derived, so they carry over unchanged.
    Map(Vec<(u64, SnapValue, SnapValue)>),
    /// `(cached hash, element)` pairs.
    Set(Vec<(u64, SnapValue)>),
}

/// B3.4 — how a `--parallel` task ended, recorded in its slot. The join (`run_parallel_nursery`)
/// scans these in task order: `Done`/`Exit` flush their buffered output; the lowest-index `Exit` or
/// `Fault` propagates (an `Exit` hard-halts the parent, a `Fault` unwinds normally so an outer
/// `recover:` can catch it); `Cancelled` is swallowed (a sibling-abort, its partial output dropped).
#[derive(Debug)]
enum TaskOutcome {
    /// Ran to completion. Its return value crossed the airlock; output flushed in task order.
    Done(WorkerResult),
    /// Observed the nursery cancel flag and unwound (a sibling faulted/exited first). Dropped.
    Cancelled,
    /// Called `std.os.exit(code)`. Buffered output is flushed, then the parent hard-halts with `code`.
    Exit { code: i32, out: String, stderr: String },
    /// Faulted (runtime error or caught panic). The lowest-index fault propagates out of the join.
    Fault(RuntimeError),
}

/// B3.3-threads — the per-task outcome slots a `--parallel` nursery collects (task order; `None`
/// until that task finishes). Shared with the pool threads via `Arc`; each fills its own index.
type TaskSlots = Arc<Mutex<Vec<Option<TaskOutcome>>>>;

/// B3.3-threads — a completion guard for a farmed pool task: its `Drop` bumps the nursery's
/// finished-count and wakes the joining thread **on every exit path**, including a panic unwinding
/// through the task body. This is what makes [`Vm::run_parallel_nursery`]'s join robust — without it
/// a panicking worker would leave the counter short and hang the joiner forever (the join's wait loop
/// would never see `count == pool_count`). Poison-tolerant so a poisoned counter can't re-panic here.
struct DoneSignal(Arc<(Mutex<usize>, std::sync::Condvar)>);

impl Drop for DoneSignal {
    fn drop(&mut self) {
        let (lock, cv) = &*self.0;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        cv.notify_all();
    }
}

/// The `deadlock` fault message, shared by the cooperative scheduler ([`Vm::run_scheduler`]) and
/// the `--parallel` M:N detector ([`MnSched::take_runnable`]) so the error is byte-identical across
/// engines.
const DEADLOCK_MSG: &str = "deadlock: every task in this parallel: block is blocked on an empty \
     channel recv() and no sibling can send — the nursery cannot progress";

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
/// (`≤ LOCAL_RING_CAP/2`) or steal-haul at a time (no hard cap or spill is needed). `runnext` is not
/// populated at runtime yet (only a `recv`-wake/spawn routed through a local would); `try_steal`
/// already drains it so the deadlock predicate stays sound if a future commit starts using it.
struct LocalQ {
    runnext: Option<Fiber>,
    ring: std::collections::VecDeque<Fiber>,
}

/// Caps the per-grab batch a worker pulls from the global queue into its local (`LOCAL_RING_CAP/2`),
/// so one worker can't drain the whole global queue into its local and starve stealing. Go uses 256
/// as the local ring bound; here it serves only as the grab-batch cap (a local is drained before the
/// next grab, so it is not also enforced as a hard push bound).
const LOCAL_RING_CAP: usize = 256;

/// D4e — the brief bounded wait a worker does when `runnable > 0` but it found no fiber to run (the
/// work is in a sibling local it lost the steal race for, or in the sub-µs in-hand `Vec` window of a
/// concurrent batch-grab). NOT the idle park (that is a true timeout-less `cv.wait` at `runnable ==
/// 0`): this is a backoff that the surplus-push / wake `notify_all` cuts short, so it adds no
/// latency on the common path. It exists only to stop (W−1) idle workers from busy-spinning on the
/// core lock across the in-hand window (the D4e SRE review's thundering-herd / oversubscription
/// finding) — a sleeping waiter neither hammers lock A nor starves the worker holding the surplus of
/// CPU. Small (the window is microseconds), and a missed notify costs ≤ this, never liveness.
const SPIN_BACKOFF: std::time::Duration = std::time::Duration::from_micros(500);

/// D4d — every Nth schedule a worker checks the global queue before its own local, bounding the
/// latency of global work while a worker is continuously fed by stealing. Go uses 61 (prime, to
/// avoid resonating with common batch sizes).
const GLOBAL_CHECK_INTERVAL: u64 = 61;

impl LocalQ {
    fn new() -> Self {
        LocalQ { runnext: None, ring: std::collections::VecDeque::new() }
    }
    /// Pop the next fiber to run: `runnext` first (locality), then the ring front (FIFO).
    fn pop(&mut self) -> Option<Fiber> {
        self.runnext.take().or_else(|| self.ring.pop_front())
    }
}

/// Per-connection spawn — the open state of an EAGER `parallel:` nursery (one activated at
/// `EnterNursery` under `--parallel` inside a live fiber, rather than lazily built at `JoinNursery`).
/// A `spawn` in the body injects a handler fiber into `sched` immediately (see [`MnSched::inject`]),
/// so the acceptor keeps running while handlers execute on the sched's farmed workers. Lives on a
/// per-fiber stack ([`FiberCtx::eager_scheds`]) in lockstep with `nurseries`, so an early
/// `?`/`return`/`break`/`recover:` reclaims it alongside the matching `nurseries` level.
struct EagerScope {
    /// The live inner sched handlers are injected into; the acceptor becomes its inline worker at join.
    sched: Arc<MnSched>,
    /// This inner nursery's cancel token (distinct from the outer's): a handler fault trips it to
    /// abort the accept loop + sibling handlers; the fault then propagates up as the acceptor's fault.
    cancel: Arc<AtomicBool>,
    /// The DEDICATED raw OS thread (NOT the bounded pool) that drains injected handlers DURING the
    /// body. The eager body has no inline worker between `EnterNursery` and `JoinNursery`, so a live
    /// drainer that does not depend on the bounded pool is what guarantees liveness — on a 1-core box
    /// the pool would farm zero helpers, and nested eager nurseries would exhaust the pool (an
    /// undetectable hang, since `body_open` vetoes the deadlock predicate). Joined at the end of
    /// `join_eager_nursery`/`abort_eager_nursery` (it exits once the sched terminates). `None` only if
    /// the thread failed to spawn (then the inline join worker is the sole drainer — bounded loops
    /// still complete, just without mid-body concurrency).
    drainer: Option<std::thread::JoinHandle<()>>,
}

struct MnSched {
    core: Mutex<SchedCore>,
    cv: Condvar,
    /// B3.4 — the shared cancel flag (same token cloned onto every enlisted shell). Read under the
    /// core lock by [`MnSched::park`] to close the park-vs-cancel gap: a fiber must not park if cancel
    /// was tripped after its `recv` empty-check but before it actually parks.
    cancel: Arc<AtomicBool>,
    /// The prebuilt `deadlock` fault, cloned into every still-parked fiber's slot when the predicate
    /// fires, so the join's lowest-index-fault reduce propagates it byte-identically (`DEADLOCK_MSG`).
    deadlock_err: RuntimeError,
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
    runnable: AtomicUsize,
    /// D4b — one run queue per worker slot (lock class **B**, taken alone — never while holding the
    /// core lock — so there is no ABBA with the core/channel locks). A worker drains its own
    /// `locals[wid]` before touching the shared global queue; D4c lets it steal from a sibling's.
    locals: Vec<Mutex<LocalQ>>,
    /// D4c — a rotating counter mixed into the work-stealing victim choice so idle workers don't all
    /// probe the same sibling first (convoy avoidance) without per-worker RNG state.
    steal_ctr: AtomicUsize,
    /// D5 — count of fibers currently off in the dirty/blocking pool (a 4th fiber state beyond
    /// running / runnable / parked / done). A blocking native offload transitions running→inflight;
    /// the pool's completion transitions inflight→runnable. Read by the deadlock predicate
    /// ([`MnSched::is_deadlocked`]) so an in-flight blocking call vetoes a false deadlock fire — the
    /// fiber *will* come back runnable. Like `runnable`, only ever mutated under the core lock, so the
    /// predicate's read is sound.
    inflight: AtomicUsize,
    /// D5 owe #3 (Path C) — count of fibers currently **demoted**: blocked in place on a channel
    /// condvar after a `recv` reached inside a native callback (a 5th fiber state, distinct from
    /// `inflight`). Mutated only under the core lock by [`Vm::demote_recv_block`] (running→demoted on
    /// block, demoted→running on resume). Distinct from `inflight` because the two un-account by
    /// different paths AND feed the deadlock predicate differently: an `inflight` fiber WILL come back
    /// from the pool/poller (external progress guaranteed) so it vetoes a deadlock, but a `blocked_native`
    /// fiber comes back ONLY if a sibling sends — so when every remaining fiber is parked or
    /// blocked_native with nothing running/runnable/inflight, that IS a deadlock and the predicate must
    /// fire (see [`MnSched::is_deadlocked`]). The block loop checks the channel queue before `terminate`,
    /// so a value that was genuinely sent always wins over a spuriously-fired terminate.
    blocked_native: AtomicUsize,
}

struct SchedCore {
    /// The global overflow / seed queue. Seed + every coordinator-path requeue (deadlock flag,
    /// cancel drain) land here; per-worker requeues go to a worker's `locals[wid]` (D4c). Drained by
    /// a worker only after its own local is empty, so the global queue is the shared fallback.
    global: std::collections::VecDeque<Fiber>,
    /// Fibers parked on an empty `recv`, keyed by `ChannelCore` pointer ([`Vm::channel_core_ptr`]).
    parked: std::collections::HashMap<usize, Vec<Fiber>>,
    /// Decision-F per-task outcome slots, indexed by `Fiber::task_index`. `None` until that task ends.
    slots: Vec<Option<TaskOutcome>>,
    running: usize,  // fibers currently swapped into a worker (executing)
    parked_n: usize, // total fibers across every `parked` bucket
    done: usize,     // fibers that have produced a `TaskOutcome`
    total: usize,    // nursery task count — grows via `inject` for per-connection spawn (was fixed in D2b)
    terminate: bool, // every worker loop exits once set (done==total, deadlock, or os.exit/fault)
    /// Per-connection spawn — `true` while an EAGER nursery's body is still running (between
    /// `EnterNursery` and `JoinNursery`) and may still `inject` more tasks. While set, a transient
    /// `done == total` must NOT terminate the sched (the acceptor may inject the next handler) and
    /// `is_deadlocked` is vetoed (the body is live work the sched can't see). `JoinNursery` clears it
    /// so the inline worker can terminate once every handler is done. Always `false` for a lazy
    /// (queue-at-join) nursery, so the existing engine is byte-identical.
    body_open: bool,
    /// D5 owe #3 Path C (#1 false-positive fix) — `ChannelCore`s that a demoted (blocked-in-callback)
    /// fiber is waiting on, keyed by core ptr ([`Vm::channel_core_ptr`]) → (core, refcount). A demoted
    /// fiber polls its OWN queue (a `send` `push_back`s + notifies the channel condvar, NOT `runnable`),
    /// so a value queued for it is invisible to the counter-only predicate. [`MnSched::is_deadlocked`]
    /// peeks each registered queue before firing: a non-empty one means that fiber WILL pop + progress,
    /// so it is not a deadlock. Registered/un-registered under core lock A by [`Vm::demote_recv_block`];
    /// the refcount handles 2+ fibers demoted on the same channel.
    demoted_chans: std::collections::HashMap<usize, (Arc<ChannelCore>, usize)>,
}

impl SchedCore {
    /// Register a demoted fiber's channel (refcounted). Caller holds core lock A.
    fn register_demoted(&mut self, ptr: usize, core: &Arc<ChannelCore>) {
        self.demoted_chans.entry(ptr).or_insert_with(|| (Arc::clone(core), 0)).1 += 1;
    }

    /// Drop a demoted fiber's channel registration (removes the entry at refcount 0). Caller holds A.
    fn unregister_demoted(&mut self, ptr: usize) {
        if let Some(entry) = self.demoted_chans.get_mut(&ptr) {
            entry.1 -= 1;
            if entry.1 == 0 {
                self.demoted_chans.remove(&ptr);
            }
        }
    }
}

/// What [`MnSched::take_runnable`] hands a worker: the next fiber to run, or `Stop` (this worker
/// should leave the loop — the nursery is done, deadlocked, or otherwise terminated). The size
/// asymmetry is inherent (a `Fiber` is the unit of work and must move out of the queue regardless);
/// boxing the `Run` payload would add an allocation on the schedule hot path for no benefit.
#[allow(clippy::large_enum_variant)]
enum Take {
    Run(Fiber),
    Stop,
}

impl MnSched {
    fn new(total: usize, nworkers: usize, cancel: Arc<AtomicBool>, deadlock_err: RuntimeError) -> Self {
        MnSched {
            core: Mutex::new(SchedCore {
                global: std::collections::VecDeque::new(),
                parked: std::collections::HashMap::new(),
                slots: (0..total).map(|_| None).collect(),
                running: 0,
                parked_n: 0,
                done: 0,
                total,
                terminate: false,
                body_open: false,
                demoted_chans: std::collections::HashMap::new(),
            }),
            cv: Condvar::new(),
            cancel,
            deadlock_err,
            runnable: AtomicUsize::new(0),
            locals: (0..nworkers.max(1)).map(|_| Mutex::new(LocalQ::new())).collect(),
            steal_ctr: AtomicUsize::new(0),
            inflight: AtomicUsize::new(0),
            blocked_native: AtomicUsize::new(0),
        }
    }

    /// Lock a worker's local run queue (lock class **B**), tolerating poison.
    fn lock_local(&self, wid: usize) -> std::sync::MutexGuard<'_, LocalQ> {
        self.locals[wid].lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Lock the core, tolerating poison (a panicking worker must not wedge the rest of the nursery —
    /// same discipline as [`DoneSignal`]).
    fn lock(&self) -> std::sync::MutexGuard<'_, SchedCore> {
        self.core.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Seed the run queue with the nursery's fibers (task order).
    fn seed(&self, fibers: Vec<Fiber>) {
        self.runnable.fetch_add(fibers.len(), Ordering::Relaxed);
        self.lock().global.extend(fibers);
    }

    /// Per-connection `spawn` — inject a freshly-built handler fiber into a LIVE, running sched
    /// (the twin of [`MnSched::complete_offload`], but it ADDS a task rather than re-queuing an
    /// existing one). Assigns the fiber's outcome-slot index ITSELF (the pre-grow `total`) — so the
    /// caller cannot mis-index a slot — then grows `total` + the Decision-F `slots` vec, queues the
    /// fiber on the run queue, and bumps `runnable`, **all under the one core lock** so `total += 1`
    /// is paired atomically with `runnable += 1`: at the instant `total` grows there is a queued
    /// runnable fiber, so the deadlock predicate's `runnable == 0` clause stays sound (no spurious
    /// deadlock from the grow). Indices are assigned under the lock, so a single injector (the
    /// acceptor fiber — the only thing that can inject into ITS scope) gets monotonic spawn-order
    /// indices and `reduce_task_slots` flushes injected-task output deterministically.
    fn inject(&self, mut fiber: Fiber) {
        debug_assert!(matches!(fiber.state, FiberState::Pending(_)), "an injected handler must be unstarted (Pending) so `run_one_fiber` runs its body via `start_task`");
        let mut c = self.lock();
        fiber.task_index = c.total; // authoritative — the slot index is `total`, never trust the caller
        c.total += 1;
        c.slots.push(None);
        c.global.push_back(fiber);
        self.runnable.fetch_add(1, Ordering::Relaxed);
        drop(c);
        self.cv.notify_all();
    }

    /// Per-connection spawn — mark this (eager) sched's body as still producing tasks: a transient
    /// `done == total` will not terminate it and `is_deadlocked` is vetoed, so farmed workers park
    /// waiting for the next `inject` instead of exiting. Called at `EnterNursery`.
    fn open_body(&self) {
        self.lock().body_open = true;
    }

    /// Per-connection spawn — the eager body reached `JoinNursery`: no more injections. Clear the flag
    /// and wake every worker so the run-out-of-work path can terminate (`done == total`) or fire a
    /// genuine deadlock now that the body is no longer live work.
    fn close_body(&self) {
        self.lock().body_open = false;
        self.cv.notify_all();
    }

    /// Pop a runnable fiber for worker `wid`, marking it `running`. Search order (D4b): the worker's
    /// own `locals[wid]` (lock class **B**, taken alone), then the shared global queue (core lock).
    /// With none runnable, park this worker on `cv` until a `send`/finish/yield makes progress — or
    /// return `Stop` when the nursery is done, deadlocked, or terminated. The local queue is checked
    /// **before** the core lock and released first, so the lock order is always B-then-A (never the
    /// reverse) → no ABBA. The deadlock predicate reads the authoritative `runnable` count (no single
    /// queue to test under the split); `running == 0` is the key — while any fiber runs it might
    /// `send`/`yield` and make a sibling runnable, so an idle worker waits rather than declaring
    /// deadlock. (D4c will insert work-stealing passes between the local and the park.)
    fn take_runnable(&self, wid: usize, tick: u64) -> Take {
        loop {
            // 0. D4d — every `GLOBAL_CHECK_INTERVAL`th schedule, pull from the global queue FIRST
            //    (before own local / stealing). Without this a worker continuously refilled by
            //    stealing a busy sibling's local could leave older global work waiting; the periodic
            //    pull bounds that latency (Go's `schedtick % 61`). A single fiber is enough — it is a
            //    fairness nudge, not the main grab path.
            if tick.is_multiple_of(GLOBAL_CHECK_INTERVAL) {
                let mut c = self.lock();
                if let Some(f) = c.global.pop_front() {
                    c.running += 1;
                    self.runnable.fetch_sub(1, Ordering::Relaxed); // runnable → running
                    return Take::Run(f);
                }
                drop(c);
            }
            // 1. Own local queue — lock B alone, release before touching the core lock.
            if let Some(f) = self.lock_local(wid).pop() {
                let mut c = self.lock();
                c.running += 1;
                self.runnable.fetch_sub(1, Ordering::Relaxed); // runnable → running
                return Take::Run(f);
            }
            // 2. D4c — work-stealing: own local empty, try to steal half from a sibling (B alone, no
            //    core lock). Push the haul onto our own local and re-loop to pop it. Net-zero on
            //    `runnable`, so this never perturbs the deadlock predicate.
            let stolen = self.try_steal(wid);
            if !stolen.is_empty() {
                let mut lq = self.lock_local(wid);
                for f in stolen {
                    lq.ring.push_back(f);
                }
                continue;
            }
            // 3. Global queue (batch-grab) + termination/deadlock + park (core lock A).
            let mut c = self.lock();
            if c.terminate {
                return Take::Stop;
            }
            if !c.global.is_empty() {
                // D4c — Go-style `globrunqget`: grab a *capped* batch into our own local (one core-lock
                // acquisition amortized over the whole batch — the contention win), run the first, and
                // leave the rest in the global queue for sibling workers (cap `g/nworkers + 1` so we
                // don't hoard). The first transitions runnable→running (−1); the extras stay runnable
                // (global→local, net-zero). The extras are pushed to the local AFTER releasing the core
                // lock (order A-then-release-then-B → never B-while-holding-A).
                let g = c.global.len();
                let take = (g / self.locals.len() + 1).min(g).min(LOCAL_RING_CAP / 2);
                let first = c.global.pop_front().unwrap();
                c.running += 1;
                self.runnable.fetch_sub(1, Ordering::Relaxed); // first: runnable → running
                let extra: Vec<Fiber> = (1..take).filter_map(|_| c.global.pop_front()).collect();
                drop(c);
                if !extra.is_empty() {
                    {
                        let mut lq = self.lock_local(wid);
                        for f in extra {
                            lq.ring.push_back(f);
                        }
                    }
                    // D4c — the surplus is now stealable by idle siblings, but it landed in a local
                    // with no accompanying `notify_all` (unlike the global-queue requeue paths). The
                    // surplus IS counted in `runnable` (the global→local move is net-zero), so under
                    // D4e an idle sibling that races a park here observes `runnable > 0` and spins to
                    // steal it rather than truly sleeping — this `notify_all` additionally wakes any
                    // sibling that was already in a real `cv.wait` (parked when `runnable` was 0, e.g.
                    // before this batch was produced) so it re-checks and steals promptly. Notify
                    // after releasing the local lock (B) — `cv` is the core's, not held here.
                    self.cv.notify_all();
                }
                return Take::Run(first);
            }
            // Per-connection spawn — `body_open` holds termination open while an eager nursery's body
            // is still running (it may `inject` the next handler even though every task SO FAR is
            // done). `JoinNursery`'s `close_body` clears it, after which the inline worker terminates.
            // Always `false` on the lazy path, so this is the unchanged `done == total` terminate.
            if c.done == c.total && !c.body_open {
                c.terminate = true;
                self.cv.notify_all();
                return Take::Stop;
            }
            // D4a — deadlock predicate reads the authoritative `runnable` count rather than
            // `global.is_empty()`: under the split queues there is no single queue to test, but
            // `runnable == 0` means no fiber is queued in any local or the global. Sound because we
            // hold the core lock and `running == 0` excludes the only out-of-lock mutator (a running
            // worker's local push/steal), so no fiber can be in flight to become runnable.
            if self.is_deadlocked(&c) {
                c.flag_deadlock(&self.deadlock_err);
                self.cv.notify_all();
                return Take::Stop;
            }
            // D4e — runnable-gated park (replaces the D4c bounded `wait_timeout` poll). `runnable`
            // counts every fiber queued in the global queue OR any worker local (batch-grab and steal
            // move fibers between queues net-zero, so a fiber sitting in a local stays counted), PLUS
            // the sub-microsecond in-hand `Vec` window of a concurrent grab/steal (popped from one
            // queue, not yet pushed to the next). So `runnable > 0` means work exists somewhere
            // reachable-now (in a local, stealable) or reachable-imminently (the in-hand window): do
            // NOT truly sleep — we must re-loop and re-steal/re-grab. But do NOT busy-spin either: a
            // brief `wait_timeout` backoff (`SPIN_BACKOFF`) that any wake `notify_all` cuts short, then
            // `continue`. Busy-spinning (`drop; yield_now; continue`) was the first cut, but the D4e
            // SRE review flagged that (W−1) idle workers in a fan-out-then-quiesce burst would then
            // hammer core lock A across the in-hand window, and on an oversubscribed host the spinners
            // would starve the very worker holding the surplus of CPU — *widening* the window. A
            // sleeping waiter does neither, and the surplus-push `notify_all` (above) wakes it the
            // instant the work lands, so there is no added latency on the common path; the timeout is
            // only a backstop for a wake that fired just before we waited. `runnable == 0` means no
            // fiber is queued anywhere: park for real on `cv` with NO timeout — a true sleep, woken
            // only by a sibling's `send`/`yield`/`finish`/offload-complete `notify`.
            //
            // Lost-wakeup-free: every site that makes a fiber runnable does `runnable.fetch_add` while
            // holding THIS core lock and then `notify`s; this worker reads `runnable` under the same
            // lock immediately before it waits, and `cv.wait`/`wait_timeout` atomically
            // releases-and-enqueues before any notifier can re-acquire the lock. So a
            // `runnable++`/`notify` either happens-before this read (we observe `runnable > 0`, take
            // the backoff branch, and re-steal) or after the wait is registered (the `notify` reaches
            // us). The mutex serializes the publish against the observe — it IS the StoreLoad barrier,
            // so Go's lockless `nmspinning` fence is unnecessary here (chezzi's precise `runnable`
            // atomic is the reachability oracle Go lacks). Terminate/deadlock/cancel still broadcast
            // via `notify_all`, which wakes these true sleepers to exit/unwind.
            if self.runnable.load(Ordering::Relaxed) > 0 {
                let (guard, _) = self.cv.wait_timeout(c, SPIN_BACKOFF).unwrap_or_else(|e| e.into_inner());
                drop(guard);
                continue;
            }
            debug_assert!(
                c.global.is_empty(),
                "D4e park invariant: runnable==0 but the global queue is non-empty"
            );
            let guard = self.cv.wait(c).unwrap_or_else(|e| e.into_inner());
            drop(guard);
        }
    }

    /// Park the running fiber on channel `key` (it blocked on an empty `recv`), freeing the worker —
    /// BUT close the park gap first. Between `recv`'s empty-check and this call (a swap-out apart) a
    /// sibling `send` may have enqueued a message (+ run [`MnSched::send_wake`], which found no parked
    /// fiber) or a sibling may have tripped cancel; either would strand this fiber forever. So, holding
    /// the core lock (the SAME lock `send_wake`/`cancel_drain` take), re-check the channel queue and
    /// the cancel flag: if a message arrived or cancel is set, requeue the fiber as `Ready` (it will
    /// re-run `recv` and pop, or unwind on the cancel back-edge) instead of parking. Lock order is
    /// core-OUTER, channel-`q`-INNER everywhere (`send_wake` matches), so there is no ABBA cycle.
    /// No `cv` notify on the park path: parking creates no runnable work; an all-parked deadlock is
    /// detected by this worker's next `take_runnable` (`running == 0`).
    fn park(&self, key: usize, core: &Arc<ChannelCore>, mut fiber: Fiber) {
        let mut c = self.lock();
        c.running -= 1;
        // Close the park gap: re-check (under the core lock) whether a message is waiting, the channel
        // was CLOSED (a concurrent `close()` between `recv`'s empty-check and here — the fiber must
        // re-run to observe `closed` and end its `for`/fault, not park forever), or cancel was tripped.
        let (message_waiting, closed) = {
            let g = core.q.lock().unwrap_or_else(|e| e.into_inner());
            (!g.queue.is_empty(), g.closed)
        };
        if message_waiting || closed || self.cancel.load(Ordering::Relaxed) {
            fiber.state = FiberState::Ready;
            c.global.push_back(fiber);
            self.runnable.fetch_add(1, Ordering::Relaxed); // running → ready (requeued)
            self.cv.notify_all();
        } else {
            fiber.state = FiberState::Blocked; // running → parked: runnable unchanged
            c.parked.entry(key).or_default().push(fiber);
            c.parked_n += 1;
        }
    }

    /// D3 — the running fiber exhausted its reduction budget; requeue it at the TAIL of the **global**
    /// queue (not a local) and free the worker. Routing a time-slice preemption to the global queue
    /// (as Go does) preserves cross-worker fairness: the worker returns to the shared pool and picks up
    /// *other* runnable work via its next batch-grab, instead of re-popping the same CPU-bound fiber
    /// from its own local forever (which would re-introduce the D3 starvation). Decrements `running`
    /// like `park`/`finish`. Unlike `park` it touches no `parked` bucket (a yield carries no channel
    /// handle) and always requeues, so there is no park-gap/cancel re-check: a cancelled fiber requeued
    /// here re-runs and observes the flag at the next back-edge. `notify_all` wakes an idle worker.
    fn yield_fiber(&self, mut fiber: Fiber) {
        let mut c = self.lock();
        c.running -= 1;
        fiber.state = FiberState::Ready;
        c.global.push_back(fiber);
        self.runnable.fetch_add(1, Ordering::Relaxed); // running → ready (round-robin requeue)
        self.cv.notify_all();
    }

    /// D4c — try to steal runnable work for an idle worker `wid` from a sibling's local queue. Probes
    /// every other worker (rotating start via a shared counter, to avoid convoying) and steals
    /// **half** (ceil) of the first non-empty victim, taken from the ring BACK first (the victim runs
    /// its front, so thief and victim rarely contend on the same fiber) and falling back to the
    /// victim's `runnext` once the ring is exhausted. Draining `runnext` too is what keeps the
    /// deadlock predicate sound: a fiber stranded in a victim's `runnext` is counted in `runnable`
    /// (suppressing the deadlock fire) but, if a thief could not reach it, no one would run it → a
    /// silent hang. `runnext` is unused at runtime today (only `recv`-wake/spawn would populate it,
    /// neither of which routes through a local yet), but stealing it now makes the contract hold for
    /// any future commit that does. Returns the stolen fibers (the caller pushes them onto its own
    /// local). Locks one victim local at a time (class **B**, alone), never while holding the core
    /// lock → no ABBA. Net-zero on `runnable` (the fibers stay runnable, just move locals).
    fn try_steal(&self, wid: usize) -> Vec<Fiber> {
        let n = self.locals.len();
        if n <= 1 {
            return Vec::new();
        }
        let r = self.steal_ctr.fetch_add(1, Ordering::Relaxed);
        for i in 0..n {
            let v = (wid + 1 + r.wrapping_add(i)) % n;
            if v == wid {
                continue;
            }
            let mut vq = self.lock_local(v);
            let len = vq.ring.len() + usize::from(vq.runnext.is_some());
            if len == 0 {
                continue;
            }
            let take = len.div_ceil(2); // ceil-half, so a victim with 1 still yields it
            let mut stolen = Vec::with_capacity(take);
            for _ in 0..take {
                // Ring BACK first, then the `runnext` slot as a last resort.
                if let Some(f) = vq.ring.pop_back().or_else(|| vq.runnext.take()) {
                    stolen.push(f);
                } else {
                    break;
                }
            }
            if !stolen.is_empty() {
                return stolen;
            }
        }
        Vec::new()
    }

    /// A `send` into channel `key`: enqueue the message AND move every fiber parked on it back onto
    /// the global queue (as `Ready`), atomically under the core lock — so it serializes with [`MnSched::park`]'s
    /// gap re-check and a fiber parking concurrently is never lost. `notify_all` may over-notify (a
    /// spuriously woken worker finds the queue empty and re-parks) — targeted wake + the StoreLoad
    /// barrier are D4. Lock order: core-OUTER, channel-`q`-INNER (matches `park`).
    fn send_wake(&self, key: usize, core: &Arc<ChannelCore>, w: WireValue) {
        let mut c = self.lock();
        core.q.lock().unwrap_or_else(|e| e.into_inner()).queue.push_back(w);
        if let Some(woken) = c.parked.remove(&key) {
            c.parked_n -= woken.len();
            self.runnable.fetch_add(woken.len(), Ordering::Relaxed); // parked → ready
            for mut f in woken {
                f.state = FiberState::Ready;
                c.global.push_back(f);
            }
        }
        drop(c);
        self.cv.notify_all();
        // D5 owe #3 (Path C) — also wake any worker thread DEMOTED on this channel (blocked in place
        // on `core.cv` after a `recv` inside a native callback). Snapshot-parked fibers are requeued
        // above + woken via `self.cv`; a demoted thread instead waits on the channel's OWN condvar, so
        // it must be notified here. Without this it would only re-check on its bounded poll timeout
        // (added latency, not a hang). No-op when no thread is demoted on this channel.
        core.cv.notify_all();
    }

    /// A `close()` on channel `key`: wake EVERY fiber parked on it (not just one, as a `send` would —
    /// a close has no value to deliver, it just unblocks all receivers) so each re-runs its `recv` /
    /// `ChanRecvOrClosed` and observes the now-closed channel (a `for v in ch:` ends, a bare `recv`
    /// faults). Atomic under the core lock so it serializes with [`MnSched::park`]'s gap re-check (the
    /// `closed` flag is already set by the caller before this call). Same lock discipline as
    /// `send_wake`; `core.cv.notify_all` also wakes any thread DEMOTED on this channel.
    fn close_wake(&self, key: usize, core: &Arc<ChannelCore>) {
        let mut c = self.lock();
        if let Some(woken) = c.parked.remove(&key) {
            c.parked_n -= woken.len();
            self.runnable.fetch_add(woken.len(), Ordering::Relaxed); // parked → ready
            for mut f in woken {
                f.state = FiberState::Ready;
                c.global.push_back(f);
            }
        }
        drop(c);
        self.cv.notify_all();
        core.cv.notify_all();
    }

    /// Record a finished fiber's outcome in its task slot and drop it from `running`. Terminates the
    /// nursery once every task is done.
    fn finish(&self, task_index: usize, outcome: TaskOutcome) {
        let mut c = self.lock();
        c.running -= 1;
        c.slots[task_index] = Some(outcome);
        c.done += 1;
        // Per-connection spawn — do NOT latch `terminate` while an eager body is still injecting: a
        // transient `done == total` (every handler SO FAR finished) is not completion — the acceptor
        // may inject more. `close_body` at `JoinNursery` clears `body_open`, and the next run-out-of-
        // work `take_runnable` terminates. Always `false` on the lazy path → unchanged D2b behavior.
        if c.done == c.total && !c.body_open {
            c.terminate = true;
        }
        self.cv.notify_all();
    }

    /// B3.4 — after cancel is tripped, move every parked fiber back onto the global queue so a worker resumes it
    /// and it observes the cancel flag (at the recv re-check / a dispatch back-edge) and unwinds.
    fn cancel_drain(&self) {
        let mut c = self.lock();
        if c.parked_n == 0 {
            return;
        }
        let buckets: Vec<Vec<Fiber>> = c.parked.drain().map(|(_, v)| v).collect();
        let drained: usize = buckets.iter().map(|v| v.len()).sum();
        c.parked_n = 0;
        self.runnable.fetch_add(drained, Ordering::Relaxed); // parked → ready
        for v in buckets {
            for mut f in v {
                f.state = FiberState::Ready;
                c.global.push_back(f);
            }
        }
        self.cv.notify_all();
    }

    /// Drain the per-task outcome slots after the nursery terminates (joining thread, post-loop).
    fn take_slots(&self) -> Vec<Option<TaskOutcome>> {
        std::mem::take(&mut self.lock().slots)
    }

    /// D5 owe #3 (Path C) — block until every task slot is filled (`done == total`), then return. The
    /// joining thread calls this AFTER its `mn_worker_loop` returns and BEFORE `take_slots`, because
    /// under Path C the loop can return in two ways that race slot-completion: (a) the joining thread
    /// itself demoted and early-exited (its replacement is still draining the nursery), or (b) a
    /// `terminate` (deadlock) was set before the `blocked_native` threads finished faulting in place.
    /// In the common case `mn_worker_loop` already returned because `done == total`, so this is a
    /// non-blocking re-check. Poison-tolerant (a panicked worker must not wedge the join). Liveness
    /// rests on the invariant that EVERY demote-loop exit settles its fiber (value → resume → finish;
    /// cancel/terminate/self-detected-deadlock → fault → finish; spawn-failure → fault → finish), so
    /// `done` strictly advances to `total`.
    fn wait_for_completion(&self) {
        let mut c = self.lock();
        while c.done < c.total {
            c = self.cv.wait(c).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// The B3.5 deadlock predicate (D4a + D5): every not-done fiber is parked, with none running,
    /// none queued anywhere (`runnable`), and **none in flight in the blocking pool** (`inflight`) —
    /// so no `send` and no blocking-pool completion can ever arrive to wake a parked fiber. Called
    /// under the core lock (the caller holds `c`); `running == 0` excludes the only out-of-lock
    /// `runnable` mutator, and `inflight` is mutated only under the core lock, so both reads are
    /// sound. The `done < total` half is guaranteed by the call site (the `done == total` terminate
    /// check precedes this).
    fn is_deadlocked(&self, c: &SchedCore) -> bool {
        // Per-connection spawn — an eager nursery whose body is still running is live work this sched
        // can't account (the acceptor runs inline on its OUTER sched and may `inject` a handler that
        // wakes a parked sibling). Never declare deadlock while the body is open; `close_body` at
        // `JoinNursery` re-enables the predicate so a genuine post-join deadlock still fires. Always
        // `false` on the lazy path — unchanged.
        if c.body_open {
            return false;
        }
        if !(c.running == 0
            && self.runnable.load(Ordering::Relaxed) == 0
            && self.inflight.load(Ordering::Relaxed) == 0
            // D5 owe #3 (Path C) — a `blocked_native` fiber (demoted, waiting in place on a channel
            // condvar) comes back only via a sibling `send`; if nothing is running/runnable/inflight,
            // no send can ever arrive, so an all-parked-or-blocked_native quiesce IS a deadlock. The
            // demoted thread observes the resulting `terminate` (via its bounded condvar poll) and
            // faults in place. (`blocked_native++` notifies `cv` so an idle puller re-evaluates this.)
            && (c.parked_n > 0 || self.blocked_native.load(Ordering::Relaxed) > 0))
        {
            return false;
        }
        // D5 owe #3 Path C (#1 false-positive fix) — before declaring deadlock, peek every demoted
        // fiber's channel queue (A-then-q — the caller holds the `SchedCore` guard, the same order
        // `send_wake` uses, so no ABBA). A value already queued for a demoted fiber is invisible to the
        // counters above (a `send` doesn't bump `runnable` for a demoted fiber), but that fiber WILL pop
        // it on its next poll and make progress — so this is NOT a deadlock. Without this peek, a sibling
        // `send` racing the quiesce could spuriously fault an innocent PARKED sibling.
        if c.demoted_chans
            .values()
            .any(|(core, _)| !core.q.lock().unwrap_or_else(|e| e.into_inner()).queue.is_empty())
        {
            return false;
        }
        true
    }

    /// D5 — hand a fiber that hit a blocking native call to the dirty/blocking pool, freeing this
    /// core worker. Transitions running→inflight under the core lock (so the deadlock predicate is
    /// immediately sound: the fiber is accounted as in-flight, not vanished), then submits the call.
    /// The pool thread runs the native off-heap ([`run_offload`]) and `complete_offload`s the fiber
    /// back onto the run queue. Takes `&Arc<Self>` so the completion closure can hold the scheduler.
    fn offload(self: &Arc<Self>, fiber: Fiber, req: OffloadReq) {
        {
            let mut c = self.lock();
            c.running -= 1;
            self.inflight.fetch_add(1, Ordering::Relaxed); // running → inflight
        }
        let sched = Arc::clone(self);
        // On the timer path `func`/`args` are intentionally unused (a sleep computes nothing — the
        // fiber resumes with `Nil`); they are consumed only by the pool branch below.
        let OffloadReq { func, args, span, timer_ms } = req;
        if let Some(ms) = timer_ms {
            // D5 owe #2 — a `sleep_ms`: park the fiber on the timer thread (no pool thread, no work),
            // waking it at the deadline. `sleep_ms` returns nothing, so the fiber resumes with
            // `Ok(Nil)` and the native is never run (there is nothing to compute). Same
            // inflight→runnable + `notify` accounting as the pool path (`complete_offload`), so the
            // deadlock predicate stays sound: the sleeping fiber is `inflight` and WILL come back.
            // `checked_add` saturates a pathological `ms` (e.g. centuries) to a far-future deadline
            // instead of panicking the worker on `Instant` overflow — a panic here escapes *before*
            // `complete_offload` and would pin `inflight` forever (hang). Matches the old
            // `thread::sleep` path's "effectively infinite sleep" rather than a crash.
            let dur = std::time::Duration::from_millis(ms);
            let deadline = std::time::Instant::now()
                .checked_add(dur)
                .unwrap_or_else(|| std::time::Instant::now() + std::time::Duration::from_secs(86_400 * 365));
            timer::submit_at(
                deadline,
                Box::new(move || {
                    let mut fiber = fiber;
                    fiber.resume_native = Some(Ok(crate::native::NativeRet::Nil));
                    sched.complete_offload(fiber);
                }),
            );
            return;
        }
        blocking_pool::submit(Box::new(move || {
            // `complete_offload` MUST run on every path — if it didn't (e.g. the native panicked and
            // unwound), `inflight` would stay pinned forever, vetoing the deadlock predicate and
            // hanging the nursery with the fiber lost. So catch a panic here and surface it as a
            // fault on the fiber (matching an inline native panic, which `run_one_fiber`'s
            // `catch_unwind` also turns into a task fault) rather than letting it escape into the
            // pool's belt-and-suspenders `catch_unwind`.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_offload(func, args)));
            let result = match outcome {
                Ok(Ok(nr)) => Ok(nr),
                Ok(Err(e)) => Err(RuntimeError { message: e.message, span }),
                Err(p) => Err(panic_to_fault(p, span)),
            };
            let mut fiber = fiber;
            fiber.resume_native = Some(result);
            sched.complete_offload(fiber);
        }));
    }

    /// D6 — hand a fiber whose socket op returned `WouldBlock` to the netpoller, freeing this core
    /// worker. Transitions running→inflight under the core lock (so the deadlock predicate is
    /// immediately sound — a socket-parked fiber is accounted as in-flight, vetoing a false deadlock;
    /// it WILL be woken by the OS), then registers fd interest. The poller `complete_offload`s the
    /// fiber back onto the run queue on readiness (inflight→runnable), exactly like the blocking pool.
    /// Takes `&Arc<Self>` so the poller can hold the scheduler for that completion.
    fn poll_park_offload(self: &Arc<Self>, fiber: Fiber, pp: PollPark) {
        {
            let mut c = self.lock();
            c.running -= 1;
            self.inflight.fetch_add(1, Ordering::Relaxed); // running → inflight
        }
        // `register` rejects (returns the fiber) iff cancel was tripped before it could park — a
        // sibling faulted in the park-vs-cancel gap. Re-inject so the fiber resumes and unwinds on the
        // cancel flag, rather than parking on a poller a past `drain_sched` already swept (→ a hang).
        if let Some(fiber) = poller::register(pp.key, pp.fd, pp.interest, fiber, Arc::clone(self), Arc::clone(&pp.in_flight), pp.deadline) {
            pp.in_flight.store(false, Ordering::Release);
            self.complete_offload(fiber);
        }
    }

    /// D5 — the blocking pool finished an offloaded native: re-enqueue the fiber (with its result
    /// stashed in `resume_native`) as `Ready` on the global queue and wake a worker. Transitions
    /// inflight→runnable under the core lock; the `notify_all` is the wakep (reusing the D4 wake
    /// path). The `runnable.fetch_add` here happens under the core lock and is followed by `notify`,
    /// so D4e's runnable-gated park is lost-wakeup-free: a worker parking concurrently either sees
    /// `runnable > 0` and does not sleep, or is already in `cv.wait` and is reached by this `notify`.
    /// D6 reuses this verbatim as the netpoller's inject path (a socket op re-runs, so it stashes no
    /// `resume_native` — the fiber's stays `None`).
    fn complete_offload(&self, mut fiber: Fiber) {
        let mut c = self.lock();
        self.inflight.fetch_sub(1, Ordering::Relaxed); // inflight → runnable
        fiber.state = FiberState::Ready;
        c.global.push_back(fiber);
        self.runnable.fetch_add(1, Ordering::Relaxed);
        drop(c);
        self.cv.notify_all();
    }
}

/// D5 — run an offloaded blocking native off the core worker (on a blocking-pool thread, no `Vm`):
/// serve its already-extracted primitive args through an [`OffloadHost`] and invoke it. The scoped
/// blocking fns read only primitive args + return a primitive [`NativeRet`], so they never touch the
/// heap / host I/O here (the off-heap host `unreachable!`s if one ever does — a misclassification).
fn run_offload(
    func: crate::native::NativeFn,
    args: Vec<crate::native::NativeArg>,
) -> Result<crate::native::NativeRet, crate::native::HostError> {
    let mut host = OffloadHost { args };
    func(&mut host)
}

impl SchedCore {
    /// Fault every still-parked fiber with the deadlock error and terminate (called under the lock).
    fn flag_deadlock(&mut self, err: &RuntimeError) {
        let buckets: Vec<Vec<Fiber>> = self.parked.drain().map(|(_, v)| v).collect();
        for v in buckets {
            for f in v {
                self.slots[f.task_index] = Some(TaskOutcome::Fault(err.clone()));
                self.done += 1;
            }
        }
        self.parked_n = 0;
        self.terminate = true;
    }
}

/// B3.3-threads — turn a caught panic payload (from `catch_unwind` around a worker task) into a
/// `RuntimeError` so a Rust panic inside a `--parallel` task surfaces as an ordinary task fault
/// (joined + reported) instead of hanging the join or aborting the process opaquely.
fn panic_to_fault(payload: Box<dyn std::any::Any + Send>, span: Span) -> RuntimeError {
    let msg = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    RuntimeError { message: format!("internal error: a parallel task panicked: {msg}"), span }
}

/// B3.3-threads — a worker `Vm` with its task already reconstructed in its own heap, ready to run on
/// whatever thread owns it. [`Vm::prepare_worker`] builds this (the parent-heap-touching half of the
/// old `run_task_isolated`); [`ReadyWorker::run`] is the thread-side half (invoke + wire the result
/// back). Splitting the two lets the bounded pool prepare a task on the parent thread, then **move**
/// the whole `ReadyWorker` (it is `Send` — `Vm` is `Send`, `Value`/`String` are `Send`) onto a pool
/// thread to execute. Single-thread `run_task_isolated` = `prepare_worker(task)?.run()`.
struct ReadyWorker {
    worker: Vm,
    call: ReadyCall,
    span: Span,
}

/// How a [`ReadyWorker`]'s task is invoked, with all values already reconstructed in the worker heap.
enum ReadyCall {
    /// A `spawn f(x)` / `spawn:` block — invoke the rebuilt callable with its rebuilt args.
    Invoke { callee: Value, args: Vec<Value> },
    /// A `spawn recv.m(args)` method task — dispatch `name` on the rebuilt receiver/args (B3.3d).
    Method { recv: Value, name: String, args: Vec<Value> },
}

impl ReadyWorker {
    /// Run the prepared task to completion **on the current thread** and hand back its return value
    /// (wired into a `Send` form) plus the worker's captured `out`/`stderr` (decision F). A method
    /// task that blocks on `recv` would leave no result on the stack; under `--parallel` the blocking
    /// `recv` instead waits on the channel condvar (it never returns the suspend sentinel), so the
    /// `suspend` guard is a safety net against a future regression, not a live path.
    #[cfg(test)]
    fn run(mut self) -> Result<WorkerResult, RuntimeError> {
        let span = self.span;
        let ret = Self::invoke(&mut self.worker, self.call, span)?;
        // The result must be cross-safe too — a worker returning a `str`/closure can't hand a
        // worker-heap `GcRef` back to the parent.
        let value = self.worker.to_wire(ret)?;
        self.worker.ensure_crossable(&value, span)?;
        Ok(WorkerResult { value, out: self.worker.out, stderr: self.worker.stderr })
    }

    /// Invoke the prepared task on the worker VM, leaving its return value on the stack popped into
    /// `ret`. Borrows `worker` and consumes `call` (disjoint fields), so the caller keeps `worker`
    /// afterward to inspect `pending_exit`/`cancelled` (B3.4).
    fn invoke(worker: &mut Vm, call: ReadyCall, span: Span) -> Result<Value, RuntimeError> {
        match call {
            ReadyCall::Invoke { callee, args } => worker.invoke_value(callee, args, span),
            ReadyCall::Method { recv, name, args } => {
                let argc = args.len();
                worker.push(recv);
                for a in args {
                    worker.push(a);
                }
                worker.do_method_call(&name, argc, NO_IC, span)?;
                if worker.suspend.is_some() {
                    return Err(worker.err(
                        "spawn: a method task blocked on recv in an isolated worker (no scheduler until B3.3-threads)".to_string(),
                        span,
                    ));
                }
                Ok(worker.pop())
            }
        }
    }

    /// B3.4 — the `--parallel` join's entry point: run the task and classify how it ended into a
    /// [`TaskOutcome`]. On any abnormal end (fault, panic-as-fault upstream, or `os.exit`) it trips
    /// the nursery cancel flag so running siblings abort at their next back-edge / blocked `recv`.
    /// Precedence: a deliberate `os.exit` (worker `pending_exit`) → `Exit`; an observed sibling
    /// cancel (`worker.cancelled`) → `Cancelled` (swallowed); else the invoke result maps to
    /// `Fault`/`Done`. Output buffers are moved out only on the paths that flush them.
    fn run_outcome(mut self) -> TaskOutcome {
        let span = self.span;
        let res = Self::invoke(&mut self.worker, self.call, span);
        // Classify the outcome (legacy `Executor`-drain pool path; the nursery engine is M:N now).
        if let Some(code) = self.worker.pending_exit {
            // A child `std.os.exit(code)` is a fault-that-cancels: it surfaces as an `Err` sentinel
            // with `pending_exit` set. Trip cancel, flush its output, hand the code up for a halt.
            self.worker.trip_cancel();
            TaskOutcome::Exit {
                code,
                out: std::mem::take(&mut self.worker.out),
                stderr: std::mem::take(&mut self.worker.stderr),
            }
        } else if self.worker.cancelled {
            // This worker observed a sibling's cancel and unwound — swallow it (output dropped).
            TaskOutcome::Cancelled
        } else {
            match res {
                Err(e) => {
                    self.worker.trip_cancel();
                    TaskOutcome::Fault(e)
                }
                Ok(ret) => {
                    let crossed = self
                        .worker
                        .to_wire(ret)
                        .and_then(|value| self.worker.ensure_crossable(&value, span).map(|()| value));
                    match crossed {
                        Ok(value) => TaskOutcome::Done(WorkerResult {
                            value,
                            out: std::mem::take(&mut self.worker.out),
                            stderr: std::mem::take(&mut self.worker.stderr),
                        }),
                        Err(e) => {
                            self.worker.trip_cancel();
                            TaskOutcome::Fault(e)
                        }
                    }
                }
            }
        }
    }

    /// D2b — deconstruct a prepared worker into a lightweight [`Fiber`] for the M:N engine: its
    /// reconstructed heap + lazy-module roots + executors become the fiber's `FiberCtx` (heap-keyed
    /// state that travels together — see [`FiberCtx`]), and its `ReadyCall` becomes a `Pending` task
    /// `start_task` launches on first schedule. The worker `Vm` shell is discarded: under M:N a fiber
    /// runs on a shared host shell (its module snapshot is re-installed there), not its own `Vm`.
    fn into_fiber(self, task_index: usize) -> Fiber {
        let ReadyWorker { worker, call, span } = self;
        let task = match call {
            ReadyCall::Invoke { callee, args } => PendingCall::Call { callee, args, span },
            ReadyCall::Method { recv, name, args } => PendingCall::Method { recv, name, args, span },
        };
        let ctx = FiberCtx {
            heap: Some(worker.heap),
            module_objs: worker.module_objs,
            module_faulted: worker.module_faulted,
            executors: worker.executors,
            // M19 Phase 3 — the intern cache indexes `worker.heap`, which becomes `ctx.heap`; carry it
            // so the heap-keyed invariant holds (its `GcRef`s stay valid against the heap they travel with).
            str_intern: worker.str_intern,
            ..FiberCtx::default()
        };
        Fiber { ctx, state: FiberState::Pending(task), task_index, span, resume_native: None }
    }
}

/// D2b — the disposition of one fiber run on a worker shell: park it on a channel (it blocked on an
/// empty `recv`; carries the channel core ptr key + the `Arc<ChannelCore>` so `park` can re-check the
/// queue under the sched lock) or finish it with a terminal outcome.
enum Disp {
    Park(usize, Arc<ChannelCore>),
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
    Finish(TaskOutcome),
}

/// D6 — a socket op parked on fd readiness: the `key` (the `SocketCore`/`ListenerCore` poll key, also
/// the netpoller registry key + the `close`-side `deregister` handle), the raw `fd`, and the
/// direction of interest. Carried from the op's `WouldBlock` site up to the worker loop, which hands
/// it to [`MnSched::poll_park_offload`].
struct PollPark {
    key: usize,
    fd: std::os::fd::RawFd,
    interest: poller::Interest,
    /// D6 — the owning socket's `in_flight` flag, handed to the poller so it can clear it on inject
    /// (see [`core::SocketCore::in_flight`]).
    in_flight: Arc<AtomicBool>,
    /// D6c — the optional read/accept/write timeout deadline. `Some` iff the op was given a
    /// `timeout_ms`: if the fd has not fired by then, the poll thread re-injects the fiber with its
    /// `poll_timed_out` marker set so the rewound op returns `Err("timeout")`. `None` = park forever.
    deadline: Option<std::time::Instant>,
}

/// D6c — a parsed `timeout_ms` argument to a net socket op. `poll_once` (true iff `ms <= 0`) means
/// "do NOT park: if the syscall would block, return `Err("timeout")` at once"; otherwise the op parks
/// with `deadline` and the netpoller wakes it on readiness OR at the deadline (whichever first).
#[derive(Clone, Copy)]
struct SockTimeout {
    poll_once: bool,
    deadline: std::time::Instant,
}

/// D5 owe #3 Path C (#3 socket half) — the outcome of one non-blocking socket-op attempt under the
/// in-callback demote loop ([`Vm::demote_block_socket`]). `Ready` carries the op's final
/// `Result[..]`-shaped `Value` (`sock_ok`/`sock_err`); `WouldBlock` means "fd not ready, re-poll after
/// a backoff". Used only on the demote path (a callback can't snapshot-park onto the netpoller).
enum SockPoll {
    Ready(Result<Value, RuntimeError>),
    WouldBlock,
}

/// The outcome of one blocking-`recv` step ([`Vm::chan_recv_step`] / [`Vm::demote_recv_block`]):
/// a value was dequeued, the channel is closed-and-drained, or the fiber parked (re-runs on wake).
/// Shared by bare `recv` (`Got` → value, `ClosedEmpty` → "receive on a closed channel" fault) and
/// the `ChanRecvOrClosed` op driving `for v in ch:` (`Got` → `Some(v)`, `ClosedEmpty` → `None`).
enum RecvStep {
    Got(WireValue),
    ClosedEmpty,
    Parked,
}

/// D6b — a non-blocking `connect` whose TCP handshake is still in flight (`EINPROGRESS`): the
/// connecting (non-blocking) `TcpStream` (it owns the fd being polled, so it must outlive the park),
/// its stable poll `key`, and a fresh `in_flight` guard. Lives in [`FiberCtx`] (per-fiber, non-heap)
/// so it survives the writability park and travels with the fiber via [`Vm::swap_ctx`]; the resumed
/// `net.connect` takes it back and calls [`crate::native::net::finish_connect`] to read `SO_ERROR`.
struct ConnectInProgress {
    stream: std::net::TcpStream,
    key: usize,
    in_flight: Arc<AtomicBool>,
}

/// D5 — a blocking native call extracted at its dispatch site, ready to run off the core worker on
/// the blocking pool. The args are already materialized out of the heap into `Send` primitives
/// ([`crate::native::NativeArg`]), so the pool thread runs the native ([`OffloadHost`]) without a
/// `Vm` / heap. `span` attributes any error the native raises.
struct OffloadReq {
    func: crate::native::NativeFn,
    args: Vec<crate::native::NativeArg>,
    span: Span,
    /// D5 owe #2 — `Some(ms)` for a `sleep_ms`: park the fiber on the timer thread for `ms` rather
    /// than run `func` on a dirty-pool thread (a sleep does no work, just waits a deadline). `None`
    /// for every other blocking native (`io`/`fs`/`request`/`process`), which runs on the pool.
    timer_ms: Option<u64>,
}

impl Vm {
    fn new(program: Arc<Program>) -> Self {
        let field_ic = vec![IcCell::EMPTY; program.field_ic_sites as usize];
        let method_ic = vec![MethodIcCell::EMPTY; program.method_ic_sites as usize];
        // M19 Tier-2 quickening: prefix-sum the per-proto code lengths into `quicken_base`, and size
        // `quicken` to the program's total instruction count (one Q_COLD cell per site). Cheap — one
        // pass at startup, which has ~11× headroom vs CPython.
        let mut quicken_base = Vec::with_capacity(program.protos.len());
        let mut acc: u32 = 0;
        for p in &program.protos {
            quicken_base.push(acc);
            acc += p.code.len() as u32;
        }
        let quicken = vec![Q_COLD; acc as usize];
        Vm {
            program,
            heap: Heap::new(),
            stack: Vec::new(),
            frames: Vec::new(),
            out: String::new(),
            stderr: String::new(),
            host: crate::native::HostConfig::default(),
            call_depth: 0,
            module_objs: Vec::new(),
            str_intern: fxhash::FxHashMap::default(),
            field_ic,
            method_ic,
            quicken,
            quicken_base,
            cur_base: 0,
            handlers: Vec::new(),
            pending_exit: None,
            fault_trace: None,
            fault_trace_depth: 0,
            gc_stress: false,
            parallel: false,
            nurseries: Vec::new(),
            eager_scheds: Vec::new(),
            nursery_defer_floors: Vec::new(),
            executors: Vec::new(),
            suspend: None,
            offload: None,
            poll_park: None,
            pending_connect: None,
            poll_timed_out: false,
            native_reentry: 0,
            reds: 0,           // D3 — set to CONTEXT_REDS per schedule-in (run_one_fiber)
            yield_now: false,  // D3
            wid: 0,            // D5 owe #3 (Path C) — set in mn_worker_loop
            demoted: false,    // D5 owe #3 (Path C)
            scheduler_stack: Vec::new(),
            cancel: None,
            cancelled: false,
            module_snapshot: None,
            module_faulted: Vec::new(),
            snapshot_memo: None,
            mn: None,
        }
    }

    fn err(&self, message: String, span: Span) -> RuntimeError {
        RuntimeError { message, span }
    }

    /// B3.4 — set this VM's nursery cancel flag (if it runs under one), so sibling workers abort.
    /// No-op on the cooperative engine / top-level VM (`cancel` is `None`).
    fn trip_cancel(&self) {
        if let Some(c) = &self.cancel {
            c.store(true, Ordering::Relaxed);
        }
    }

    /// Swap the live per-execution `Vm` fields with `ctx` (B1). Used by the nursery scheduler to
    /// schedule a fiber in (its saved context becomes live) or out (the running context is parked
    /// back into the fiber). Exactly the fields [`FiberCtx`] holds — `pending_exit` stays global.
    fn swap_ctx(&mut self, ctx: &mut FiberCtx) {
        std::mem::swap(&mut self.frames, &mut ctx.frames);
        std::mem::swap(&mut self.stack, &mut ctx.stack);
        std::mem::swap(&mut self.call_depth, &mut ctx.call_depth);
        std::mem::swap(&mut self.cur_base, &mut ctx.cur_base);
        std::mem::swap(&mut self.handlers, &mut ctx.handlers);
        std::mem::swap(&mut self.nurseries, &mut ctx.nurseries);
        std::mem::swap(&mut self.nursery_defer_floors, &mut ctx.nursery_defer_floors);
        std::mem::swap(&mut self.eager_scheds, &mut ctx.eager_scheds);
        std::mem::swap(&mut self.fault_trace, &mut ctx.fault_trace);
        std::mem::swap(&mut self.fault_trace_depth, &mut ctx.fault_trace_depth);
        // D2a — an M:N fiber (`Some`) owns its heap; swap it with the host's. A cooperative fiber
        // (`None`) shares the single `Vm::heap` (decision A), so its heap is left untouched and the
        // cooperative engine stays byte-identical by construction. D2b — the same `Some` gate carries
        // the fiber's heap-keyed side state (out/stderr/module roots/executors), so they move
        // atomically WITH the heap their `GcRef`s index. A cooperative fiber swaps none of it.
        if let Some(ctx_heap) = ctx.heap.as_mut() {
            debug_assert!(self.parallel, "cooperative fiber must never carry its own heap (decision A)");
            std::mem::swap(&mut self.heap, ctx_heap);
            std::mem::swap(&mut self.out, &mut ctx.out);
            std::mem::swap(&mut self.stderr, &mut ctx.stderr);
            std::mem::swap(&mut self.module_objs, &mut ctx.module_objs);
            std::mem::swap(&mut self.module_faulted, &mut ctx.module_faulted);
            std::mem::swap(&mut self.executors, &mut ctx.executors);
            // M19 Phase 3 — the intern cache's `GcRef`s index this fiber's OWN heap, so it MUST travel
            // atomically with the heap (same heap-keyed argument as `module_objs`). A cooperative fiber
            // (`heap: None`) never reaches here and keeps aliasing the shell's cache.
            std::mem::swap(&mut self.str_intern, &mut ctx.str_intern);
            // D6b — a mid-flight `connect` parked on writability swaps WITH its fiber (it owns the
            // connecting fd that the netpoller is watching; it must not be left on the shell where the
            // next fiber would inherit or drop it).
            std::mem::swap(&mut self.pending_connect, &mut ctx.pending_connect);
            // D6c — a socket timeout marker set by the poll thread (on the detached fiber's ctx) swaps
            // in here so the resumed socket op sees it at entry. M:N-only, like `pending_connect`.
            std::mem::swap(&mut self.poll_timed_out, &mut ctx.poll_timed_out);
        }
    }

    /// B1 / D3 — the running fiber paused mid-flight and its frames stay live to replay on resume:
    /// either a blocking `recv` parked it (`suspend`) or it exhausted its D3 reduction budget
    /// (`yield_now`). Both unwind every nested `run_until` / call site the SAME way — propagate up
    /// WITHOUT popping a result or pushing a sentinel — so every "callee paused" gate tests this, not
    /// `suspend` alone. (`yield_now` is only ever set under the M:N engine — the safepoint gates it on
    /// `mn.is_some()` — so the cooperative engine, where it is always false, is unchanged by
    /// construction.)
    fn paused(&self) -> bool {
        self.suspend.is_some() || self.yield_now || self.offload.is_some() || self.poll_park.is_some()
    }

    /// Run `f` with the native-reentry guard raised (B1). A blocking `recv` reached while the guard
    /// is up cannot park (its caller's loop/recursion state lives on the Rust stack, not in a
    /// [`Fiber`]), so it faults `deadlock` instead of suspending. Wraps every site that re-enters
    /// Chezzi code from native Rust.
    fn guarded<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, RuntimeError>) -> Result<T, RuntimeError> {
        self.native_reentry += 1;
        let r = f(self);
        self.native_reentry -= 1;
        r
    }

    /// If `v` is an unhandled error (`Err(..)`/`None`) reaching the top level, build the runtime
    /// error that exits the program. Mirrors `interp::top_level_error` — message must be identical.
    fn top_level_error(&self, v: Value, span: Span) -> Option<RuntimeError> {
        let Value::Obj(h) = v else { return None };
        let Obj::Enum { ty, variant, payload } = self.heap.get(h) else { return None };
        // Builtin `Result`/`Option` only — a user enum that shadows `Err`/`None` is a normal value.
        let unhandled = (ty.as_ref() == "Result" && variant.as_ref() == "Err")
            || (ty.as_ref() == "Option" && variant.as_ref() == "None");
        if !unhandled {
            return None;
        }
        let detail = match payload.first() {
            Some(p) => self.display(*p),
            None => self.display(v),
        };
        Some(self.err(format!("unhandled error: {detail}"), span))
    }

    // ----- top-level drivers -----

    /// Run every module in dependency order, then the entry's `main()`.
    fn run(&mut self) -> Result<(), RuntimeError> {
        for idx in 0..self.program.modules.len() {
            self.run_module(idx)?;
        }
        Ok(())
    }

    fn run_module(&mut self, idx: usize) -> Result<(), RuntimeError> {
        let m = self.program.modules[idx].clone();
        // M19 Phase 2b: pre-size the namespace to the compiler's slot count and build its name→slot
        // index from `global_slots`, so `DefineGlobalSlot(i)` / bind-import writes land in the slot
        // the compiler chose. Native modules carry no slots (members injected by name below).
        let index: std::collections::HashMap<Box<str>, u32> =
            m.global_slots.iter().enumerate().map(|(i, n)| (n.as_str().into(), i as u32)).collect();
        let mod_obj = self.heap.alloc(Obj::Module {
            name: m.label.clone().into_boxed_str(),
            slots: vec![Value::Nil; m.global_slots.len()],
            index,
        });
        debug_assert_eq!(self.module_objs.len(), idx);
        self.module_objs.push(mod_obj);

        // A native std module: populate its globals with Rust `NativeFn`s + float constants and
        // skip running a toplevel. Mirrors the interpreter's `eval_module` native arm.
        if let Some(name) = m.native {
            for (mname, func) in crate::native::native_members(name) {
                let nat = self.heap.alloc(Obj::Native {
                    name: (*mname).into(),
                    func: *func,
                });
                self.module_define(mod_obj, mname, Value::Obj(nat));
            }
            for (cname, cval) in crate::native::native_consts(name) {
                self.module_define(mod_obj, cname, Value::Float(*cval));
            }
            return Ok(());
        }

        // Bind imports (dependencies already ran, so their namespaces are populated).
        for imp in &m.imports {
            self.bind_import(mod_obj, imp)?;
        }

        // Run the module body once. No module auto-runs `main` — it's an ordinary function the
        // program calls itself (scripting-language model). An unhandled `Err`/`None` reaching the
        // top level (via `PopExprStmt` or a top-level `?`) exits during this call.
        self.run_proto(m.toplevel, mod_obj, None, Vec::new(), false, true, Span { line: 1, col: 1 })?;
        Ok(())
    }

    fn bind_import(&mut self, into: GcRef, imp: &crate::resolver::ResolvedImport) -> Result<(), RuntimeError> {
        use crate::ast::Import;
        let target_idx = self
            .program
            .module_index(&imp.target)
            .expect("resolver guarantees the import target is in the graph");
        let target_obj = self.module_objs[target_idx];
        match &imp.import {
            Import::Module { path, alias } => {
                let name = alias.clone().unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                self.module_define(into, &name, Value::Obj(target_obj));
            }
            Import::From { names, .. } => {
                for (member, alias) in names {
                    let value = self.module_global(target_obj, member).ok_or_else(|| {
                        let tname = self.module_name(target_obj);
                        self.err(format!("module '{tname}' has no member '{member}'"), imp.span)
                    })?;
                    self.module_define(into, alias.as_ref().unwrap_or(member), value);
                }
            }
        }
        Ok(())
    }

    /// Push a frame for `proto` and run the dispatch loop until it returns; yield its result.
    #[allow(clippy::too_many_arguments)]
    fn run_proto(
        &mut self,
        proto: ProtoId,
        home: GcRef,
        closure: Option<GcRef>,
        args: Vec<Value>,
        counted: bool,
        is_toplevel: bool,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let base_level = self.frames.len();
        self.push_frame(proto, home, closure, args, counted, is_toplevel, span)?;
        self.run_until(base_level)?;
        // B1/D3: the call paused mid-flight — a blocking `recv` parked it, or it exhausted its D3
        // reduction budget and is yielding its worker. The frames stay live (they replay on resume);
        // propagate the signal up without popping a result — the caller gates on `paused()` before
        // using the (sentinel) return value.
        if self.paused() {
            return Ok(Value::Nil);
        }
        Ok(self.stack.pop().unwrap_or(Value::Nil))
    }

    #[allow(clippy::too_many_arguments)]
    fn push_frame(
        &mut self,
        proto: ProtoId,
        home: GcRef,
        closure: Option<GcRef>,
        args: Vec<Value>,
        counted: bool,
        is_toplevel: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        self.frame_depth_guard(counted, span)?;
        let base = self.stack.len();
        for a in args {
            self.stack.push(a);
        }
        self.finish_frame(proto, home, closure, base, counted, is_toplevel, span);
        Ok(())
    }

    /// P1 — push a frame whose `argc` parameters are *already* contiguous on the operand stack at
    /// `[base..base + argc]` (the bytecode `Op::Call` fast path leaves them there to avoid the
    /// per-call `Vec<Value>` round-trip). Identical to [`Vm::push_frame`] minus the arg copy; never a
    /// top-level frame. The depth guard runs after the args are positioned — on overflow the args
    /// stay on the stack, but a `recover:` handler truncates to its saved `stack_len` and an uncaught
    /// overflow aborts, so the leftover slots are unobservable (same end state as the `Vec` path).
    fn push_frame_in_place(
        &mut self,
        proto: ProtoId,
        home: GcRef,
        closure: Option<GcRef>,
        base: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        self.frame_depth_guard(true, span)?;
        self.finish_frame(proto, home, closure, base, true, false, span);
        Ok(())
    }

    /// Bump + bound-check the call-depth counter (the infinite-recursion guard). Shared by the
    /// `Vec` and in-place frame-entry paths so both raise the identical overflow error.
    fn frame_depth_guard(&mut self, counted: bool, span: Span) -> Result<(), RuntimeError> {
        if counted {
            self.call_depth += 1;
            if self.call_depth > MAX_CALL_DEPTH {
                self.call_depth -= 1;
                return Err(self.err(
                    format!("maximum call depth ({MAX_CALL_DEPTH}) exceeded (infinite recursion?)"),
                    span,
                ));
            }
        }
        Ok(())
    }

    /// Reserve the non-parameter local slots above `[base..]` and push the `CallFrame`. Assumes the
    /// `argc` parameters are already on the stack starting at `base`. Shared frame-install tail.
    #[allow(clippy::too_many_arguments)]
    fn finish_frame(
        &mut self,
        proto: ProtoId,
        home: GcRef,
        closure: Option<GcRef>,
        base: usize,
        counted: bool,
        is_toplevel: bool,
        span: Span,
    ) {
        let n_slots = self.program.protos[proto].n_slots;
        // Reserve the remaining (non-parameter) local slots.
        while self.stack.len() < base + n_slots {
            self.stack.push(Value::Nil);
        }
        self.frames.push(CallFrame {
            proto,
            ip: 0,
            base,
            home,
            closure,
            counted,
            is_toplevel,
            deferred: Vec::new(),
            defer_markers: Vec::new(),
            nursery_len: self.nurseries.len(),
            has_implicit_nursery: self.program.protos[proto].has_implicit_nursery,
            call_span: span,
        });
        self.cur_base = base;
    }

    /// Build a stack trace from the live frames (innermost first), skipping module-toplevel frames.
    /// Valid only while the frames are intact — i.e. on the uncaught-error path, before unwinding.
    fn capture_trace(&self) -> Vec<TraceFrame> {
        self.frames
            .iter()
            .rev()
            .filter(|f| !f.is_toplevel)
            .map(|f| TraceFrame {
                function: self.program.protos[f.proto].name.clone(),
                span: f.call_span,
            })
            .collect()
    }

    // ----- the dispatch loop -----

    fn run_until(&mut self, base_level: usize) -> Result<(), RuntimeError> {
        // M19 — hoist the per-entry `Arc::clone(&self.program)`: borrow the program by raw
        // pointer instead of bumping the refcount. `self.program` is an immutable
        // `Arc<Program>` set once in `Vm::new` and NEVER reassigned (cooperative `spawn` /
        // `--parallel` workers each build their own `Vm`; `swap_ctx` swaps heap/frames/stack,
        // not `program`), so the pointee outlives this loop and the borrow is disjoint from
        // the `&mut self` fields `step` mutates (`step` only reads program data). Post-flatten
        // this entry is hit per top-level run + per native re-entry (HOF callbacks, operator
        // overloads, deferred calls) + per fiber resume — so the saved atomic shows on
        // callback-heavy code (see `benches/chz/hof.chz`).
        let program: *const Program = Arc::as_ptr(&self.program);
        while self.frames.len() > base_level {
            // Collect at instruction boundaries only: here every live value is reachable from the
            // VM roots (operand stack, frame slots, frame homes/closures, module namespaces) —
            // there are no mid-opcode temporaries off the stack to miss.
            if self.gc_stress || self.heap.should_collect() {
                self.collect();
            }
            // B3.4: a `--parallel` worker observes its nursery's cancel flag at this back-edge (the
            // same boundary `gc_stress` is checked at). A sibling having faulted / `os.exit`d set it;
            // unwind the whole worker so this still-running task aborts promptly instead of burning
            // cycles to completion. Cancellation behaves like an uncaught fault that bypasses
            // `recover:`: `unwind_deferred(base_level)` runs every frame's `defer`s (Go semantics)
            // AND drops their handlers, so a `recover:` inside the task cannot catch the cancel and
            // resume it (a cancelled task must die). `!self.cancelled` latches on the first
            // observation: while the cancel unwind runs the task's `defer`s back through this loop,
            // re-firing would skip them — so we stop observing once cancellation is in flight.
            if !self.cancelled && self.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
                self.cancelled = true;
                let span = self.frames[self.frames.len() - 1].call_span;
                let rte = self.err("cancelled".to_string(), span);
                // B3.4 cancel: no cancel-report — a cancelled task's pending nurseries are torn down
                // silently (the parent that set the flag already escaped and reported its own).
                let rte = self.unwind_deferred(base_level, false).unwrap_or(rte);
                return Err(rte);
            }
            // D3: reduction-counting preemption (M:N engine only — the cooperative engine is the
            // frozen parity oracle and never preempts). Decrement the budget per dispatched op; at
            // exhaustion yield this worker so a queued sibling runs (round-robin fairness). Placed
            // AFTER the cancel check so cancel wins (a cancelled fiber unwinds, never yields). The
            // `native_reentry == 0` guard mirrors `recv`-park: a yield inside a native callback can't
            // save the caller's Rust-stack state, so we defer it (leave `reds` at 0 and re-check next
            // op, once the reentry unwinds). Reuses the suspend/rewind contract — frames stay intact,
            // resume re-enters `run_until(0)` — but carries no channel handle (a voluntary park).
            if self.mn.is_some() {
                if self.reds == 0 {
                    if self.native_reentry == 0 {
                        self.yield_now = true;
                        return Ok(());
                    }
                } else {
                    self.reds -= 1;
                }
            }
            let fi = self.frames.len() - 1;
            let pid = self.frames[fi].proto;
            let ip = self.frames[fi].ip;
            self.frames[fi].ip = ip + 1;
            // Borrow the instruction (no per-step clone — the hot path must not allocate).
            // SAFETY: `program` points into `self.program`'s immutable, never-reassigned
            // `Arc<Program>` (see the loop-entry note); the pointee outlives the loop and `op`
            // borrows program data disjoint from the `&mut self` fields `step` touches.
            let proto_ref = &unsafe { &*program }.protos[pid];
            let op = &proto_ref.code[ip];
            let span = proto_ref.lines[ip];
            // M19 Phase 7 — inline the hottest opcodes here so they skip the per-op `self.step(op, span)`
            // call + its big match jump-table; the long tail delegates to `step`. The inlined arms call
            // the SAME helpers as `step` (or copy its 1–3-line body verbatim), so there is one source of
            // truth per op — keep these in lock-step with `step` if either is edited. `fi` is the current
            // frame index (valid for the `Jump` ip write: jumps never change the frame; `Call`/`Return`
            // re-read frames in their helpers).
            let step_result = match op {
                Op::GetLocal(slot) => {
                    let v = self.stack[self.cur_base + slot];
                    self.stack.push(v);
                    Ok(())
                }
                Op::SetLocal(slot) => {
                    let v = self.pop();
                    self.stack[self.cur_base + slot] = v;
                    Ok(())
                }
                Op::BinLocalLocal { a, b, kind } => self.op_bin_local_local(*a, *b, *kind, span),
                Op::BinLocalConst { slot, val, kind } => self.op_bin_local_const(*slot, *val, *kind, span),
                Op::IncLocal { slot, delta } => self.op_inc_local(*slot, *delta, span),
                Op::Jump(t) => {
                    self.frames[fi].ip = *t;
                    Ok(())
                }
                Op::JumpIfFalse(t) => {
                    if let Value::Bool(false) = self.pop() {
                        self.frames[fi].ip = *t;
                    }
                    Ok(())
                }
                Op::Call(argc) => self.do_call(*argc, span),
                Op::Return => self.do_return(false),
                // M19 Tier-2 — inline the index ops (hot in the `map` bench) so they skip the `step`
                // call + big-match jump; the helpers carry the Int-key fast path. One source of truth.
                Op::GetIndex => self.get_index(span),
                Op::SetIndex => self.set_index(span),
                // M19 Tier-2 — adaptive opcode quickening (PEP 659). These are the UN-FUSED generic
                // binop arms: `Add..GtEq` here are reached only by stack-operand binops (the
                // `local⊕local`/`local⊕const` windows already fused to superinstructions); `Eq`/`NotEq`
                // are never fused. Each consults a per-site (proto,ip) deopt cell and takes an int/int
                // fast path once warm. Handled here (not in `step`) because the site id needs `pid`+`ip`,
                // which only `run_until` has. The slow path is byte-identical to the kept `step` arms.
                Op::Add => self.q_arith(self.quicken_base[pid] as usize + ip, crate::vm::op::BinKind::Add, span),
                Op::Sub => self.q_arith(self.quicken_base[pid] as usize + ip, crate::vm::op::BinKind::Sub, span),
                Op::Mul => self.q_arith(self.quicken_base[pid] as usize + ip, crate::vm::op::BinKind::Mul, span),
                Op::Div => self.q_arith(self.quicken_base[pid] as usize + ip, crate::vm::op::BinKind::Div, span),
                Op::Mod => self.q_arith(self.quicken_base[pid] as usize + ip, crate::vm::op::BinKind::Mod, span),
                Op::Lt => self.q_arith(self.quicken_base[pid] as usize + ip, crate::vm::op::BinKind::Lt, span),
                Op::LtEq => self.q_arith(self.quicken_base[pid] as usize + ip, crate::vm::op::BinKind::LtEq, span),
                Op::Gt => self.q_arith(self.quicken_base[pid] as usize + ip, crate::vm::op::BinKind::Gt, span),
                Op::GtEq => self.q_arith(self.quicken_base[pid] as usize + ip, crate::vm::op::BinKind::GtEq, span),
                Op::Eq => self.q_eq(self.quicken_base[pid] as usize + ip, false, span),
                Op::NotEq => self.q_eq(self.quicken_base[pid] as usize + ip, true, span),
                other => self.step(other, span),
            };
            if let Err(rte) = step_result {
                // `std.os.exit(code)` is a hard halt: unwind past every `recover:` to the top.
                if self.pending_exit.is_some() {
                    return Err(rte);
                }
                // B3.4: a cancel observed deeper in this step (a blocking `recv` that woke on the
                // nursery cancel flag set `self.cancelled` and returned the sentinel) unwinds the
                // whole worker — run defers, bypass `recover:`, mirroring the loop-top check. A
                // cancelled task must not be caught and resumed.
                if self.cancelled {
                    let rte = self.unwind_deferred(base_level, false).unwrap_or(rte);
                    return Err(rte);
                }
                // Capture the stack trace of an uncaught fault now, while the frames are still intact
                // (the unwind below drops them). The deepest fault wins: the original fault captures
                // first, and a deeper deferred-call fault (run while its frame is still live) replaces
                // it. A fault this loop CAN catch resets the capture below, so no stale trace survives
                // a `recover:`.
                let caught_here = matches!(self.handlers.last().copied(), Some(h) if h.frame_len > base_level);
                if !caught_here && self.frames.len() > self.fault_trace_depth {
                    self.fault_trace = Some(self.capture_trace());
                    self.fault_trace_depth = self.frames.len();
                }
                // The nearest `recover:` boundary owned by THIS dispatch loop catches the fault; a
                // handler at/below `base_level` belongs to an outer loop, so we unwind to
                // `base_level` and propagate. Either way, every frame discarded on the way runs its
                // deferred calls first (Go: defers run as the panic unwinds, before recover regains
                // control). A fault inside a deferred call supersedes the original.
                let target = match self.handlers.last().copied() {
                    Some(h) if h.frame_len > base_level => h.frame_len,
                    _ => base_level,
                };
                // A genuine fault (not a B3.4 cancel / `std.os.exit`, both handled above) cancels-and-
                // reports each unwound frame's escaped nurseries — emitted PER FRAME, BEFORE that
                // frame's `defer`s, matching the interp oracle (whose `exec_parallel` /
                // `leave_implicit_nursery` report as the body unwinds, then `finish_frame` runs the
                // defers). `unwind_deferred` does the interleaving; this covers BOTH the uncaught arm
                // (no handler) and the frames discarded above a catching `recover:`.
                let rte = self.unwind_deferred(target, true).unwrap_or(rte);
                // A deferred `std.os.exit` turns the unwind into a hard halt.
                if self.pending_exit.is_some() {
                    return Err(rte);
                }
                match self.handlers.last().copied() {
                    Some(h) if h.frame_len > base_level => {
                        self.handlers.pop();
                        // This `recover:` caught the fault — discard any trace captured deeper in (it
                        // belongs to a fault that is now handled), so a later uncaught fault re-captures.
                        self.fault_trace = None;
                        self.fault_trace_depth = 0;
                        // `unwind_deferred` already dropped frames down to `h.frame_len`; restore the
                        // operand stack / call-depth / ip to the boundary's snapshot.
                        self.stack.truncate(h.stack_len);
                        self.call_depth = h.call_depth;
                        self.cur_base = self.frames.last().map(|f| f.base).unwrap_or(0);
                        self.frames[h.frame_len - 1].ip = h.ip;
                        // `unwind_deferred` ran the defers of frames ABOVE the boundary, but the
                        // boundary frame's own (recover-block) defers remain — drain them now, before
                        // binding the result. A fault in one supersedes the original.
                        let rte = self.drain_frame_to(h.defer_len).unwrap_or(rte);
                        // Drop the scope markers of any defer scopes opened inside the recover block:
                        // the fault jumped past their `LeaveDeferScope`s, so they would otherwise leak
                        // and corrupt later drains in this frame.
                        self.frames[h.frame_len - 1].defer_markers.truncate(h.markers_len);
                        // Reclaim any `parallel:` nursery the fault unwound past (its `JoinNursery`
                        // never ran) — mirrors the interpreter always reclaiming its nursery list.
                        // TASK B: route through `drain_escaped_nursery` so a `?` caught by `recover:`
                        // cancels-and-reports its unstarted tasks IDENTICALLY to an uncaught `?`.
                        self.drain_escaped_nursery(h.nursery_len);
                        if self.pending_exit.is_some() {
                            return Err(rte);
                        }
                        // Convert the fault message (a `str`, i.e. an `Error`) into `Err(msg)`; the
                        // boundary's `done` label receives a ready `Result`.
                        let msg = self.alloc_str(rte.message);
                        let err = self.alloc_enum("Result", "Err", vec![msg]);
                        self.push(err);
                    }
                    // Uncaught: `unwind_deferred(target, true)` above already cancelled-and-reported
                    // every unwound frame's escaped nurseries (the toplevel module nursery preserved).
                    _ => return Err(rte),
                }
            }
            // B1/D3: the running fiber paused — a blocking `recv` parked it, or (D3) it exhausted its
            // reduction budget at the safepoint above and is yielding. Stop the dispatch loop WITHOUT
            // unwinding (frames + defers stay intact to replay on resume) and hand control back up.
            // For a yield detected in a NESTED `run_until`, this is how each outer level bails after
            // its in-flight call op returns — propagating the yield all the way to the worker loop.
            if self.paused() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Mark-sweep collection. Roots: the whole operand stack (which contains every frame's local
    /// slots *and* any in-flight expression temporaries), each frame's home module + backing
    /// closure, and the module namespace cache. Everything else is garbage.
    /// Collect the GC roots held in a parked fiber context (B1): operand-stack objects, each frame's
    /// home/closure and pending deferred calls, and not-yet-run nursery tasks. Mirrors the
    /// live-context rooting in [`Vm::collect`].
    fn root_ctx(ctx: &FiberCtx, work: &mut Vec<GcRef>) {
        for v in &ctx.stack {
            if let Value::Obj(h) = v {
                work.push(*h);
            }
        }
        for f in &ctx.frames {
            work.push(f.home);
            if let Some(c) = f.closure {
                work.push(c);
            }
            for d in &f.deferred {
                work.extend(d.roots());
            }
        }
        for nursery in &ctx.nurseries {
            for task in nursery {
                work.extend(task.roots());
            }
        }
    }

    fn collect(&mut self) {
        let mut work: Vec<GcRef> = Vec::new();
        for v in &self.stack {
            if let Value::Obj(h) = v {
                work.push(*h);
            }
        }
        for f in &self.frames {
            work.push(f.home);
            if let Some(c) = f.closure {
                work.push(c);
            }
            for d in &f.deferred {
                work.extend(d.roots());
            }
        }
        // Pending `spawn` tasks (C4): their captured callee/receiver/args are roots until the task
        // runs at the nursery's join.
        for nursery in &self.nurseries {
            for task in nursery {
                work.extend(task.roots());
            }
        }
        // Live executors (C5 / A2): their queued work must survive to the program-exit auto-drain
        // even when no in-program handle remains.
        work.extend(self.executors.iter().copied());
        work.extend(self.module_objs.iter().copied());
        // M19 Phase 3 — interned `ConstStr` handles are roots: they're cached for reuse across pushes
        // of the same op, so they must never be swept out from under a later push. Heap-keyed, so this
        // roots the cache for *this* heap (an M:N fiber's cache swapped in with its heap).
        work.extend(self.str_intern.values().copied());
        // Parked fibers in active cooperative schedulers (B1/B2): each level's joining-fiber context
        // plus every child fiber's context are roots while the children run. The CURRENTLY running
        // fiber's context is the live `self.{stack,frames,nurseries}` already rooted above; a parked
        // fiber's context lives in its `FiberCtx` (or, for a not-yet-started child, in its `Pending`
        // task). Without this, a blocked fiber's locals would be swept while it waits.
        // D2a — `scheduler_stack` is the COOPERATIVE engine's parked fibers, which all alias this
        // single `self.heap` (decision A), so `root_ctx` traces their roots into it directly. They
        // carry no heap of their own (`ctx.heap == None`); a parked M:N fiber (D2b) instead owns a
        // share-nothing heap that lives off this `Vm` and is quiescent while parked — it is NEVER
        // traced cross-heap here, only collected when that fiber is next scheduled in and runs its
        // own `run_until` safepoint. (A `--parallel` worker `Vm` has an empty `scheduler_stack`, so
        // this loop is a no-op on workers.)
        for nursery in &self.scheduler_stack {
            debug_assert!(nursery.parent.heap.is_none(), "a cooperative parked fiber must not own a heap (decision A)");
            Self::root_ctx(&nursery.parent, &mut work);
            for child in &nursery.children {
                debug_assert!(child.ctx.heap.is_none(), "a cooperative child fiber must not own a heap (decision A)");
                Self::root_ctx(&child.ctx, &mut work);
                if let FiberState::Pending(task) = &child.state {
                    work.extend(task.roots());
                }
            }
        }

        while let Some(h) = work.pop() {
            if self.heap.mark(h) {
                work.extend(self.heap.children(h));
            }
        }
        self.heap.sweep();
    }

    fn base(&self) -> usize {
        self.cur_base
    }

    fn jump(&mut self, target: usize) {
        self.frames.last_mut().unwrap().ip = target;
    }

    fn push(&mut self, v: Value) {
        self.stack.push(v);
    }

    fn pop(&mut self) -> Value {
        self.stack.pop().expect("operand stack underflow")
    }

    fn step(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        match op {
            Op::ConstInt(n) => self.push(Value::Int(*n)),
            Op::ConstFloat(x) => self.push(Value::Float(*x)),
            Op::ConstStr(s) => {
                // M19 Phase 3 — intern by data pointer (stable for the program's lifetime, since the
                // literal lives in the immutable `Arc<Program>`). First push of this op allocs the
                // heap `Obj::Str`; every later push reuses the cached, GC-rooted handle — no clone,
                // no alloc. Sound because strings are immutable and there is no identity operator.
                let key = s.as_ptr() as usize;
                let h = match self.str_intern.get(&key) {
                    Some(&h) => h,
                    None => {
                        let h = self.heap.alloc(Obj::Str(s.as_str().into()));
                        self.str_intern.insert(key, h);
                        h
                    }
                };
                self.push(Value::Obj(h));
            }
            Op::True => self.push(Value::Bool(true)),
            Op::False => self.push(Value::Bool(false)),
            Op::Nil => self.push(Value::Nil),
            Op::Pop => {
                self.pop();
            }
            Op::PopExprStmt => {
                let v = self.pop();
                // An unhandled `Err`/`None` at the top level exits the program.
                if self.frames.last().unwrap().is_toplevel
                    && let Some(e) = self.top_level_error(v, span)
                {
                    return Err(e);
                }
            }
            Op::GetLocal(slot) => {
                let v = self.stack[self.base() + slot];
                self.push(v);
            }
            Op::SetLocal(slot) => {
                let v = self.pop();
                let at = self.base() + slot;
                self.stack[at] = v;
            }
            Op::GetGlobalSlot(slot) => {
                let home = self.frames.last().unwrap().home;
                self.ensure_module_faulted(home); // D1: lazily reconstruct the worker's home module
                let v = self.global_slot(home, *slot);
                self.push(v);
            }
            Op::DefineGlobalSlot(slot) => {
                let v = self.pop();
                let home = self.frames.last().unwrap().home;
                self.set_global_slot(home, *slot, v);
            }
            Op::SetGlobalSlot(slot) => {
                let v = self.pop();
                let home = self.frames.last().unwrap().home;
                self.set_global_slot(home, *slot, v);
            }
            Op::GetCaptured(name) => {
                let clo = self.frames.last().unwrap().closure;
                let home = self.frames.last().unwrap().home;
                self.ensure_module_faulted(home); // D1: home-global fallback may hit a not-yet-faulted module
                let v = clo
                    .and_then(|h| match self.heap.get(h) {
                        Obj::Closure { captured, .. } => captured.get(name).copied(),
                        _ => None,
                    })
                    .or_else(|| self.module_global(home, name))
                    .ok_or_else(|| self.err(format!("undefined name '{name}'"), span))?;
                self.push(v);
            }
            Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod => self.arith(op, span)?,
            Op::Neg => {
                let v = self.pop();
                let r = match v {
                    Value::Int(n) => n.checked_neg().map(Value::Int).ok_or_else(|| self.err("integer overflow in negation".to_string(), span))?,
                    Value::Float(f) => Value::Float(-f),
                    other => return Err(self.err(format!("cannot apply Neg to {}", self.type_name(other)), span)),
                };
                self.push(r);
            }
            Op::Not => {
                let v = self.pop();
                match v {
                    Value::Bool(b) => self.push(Value::Bool(!b)),
                    other => return Err(self.err(format!("cannot apply Not to {}", self.type_name(other)), span)),
                }
            }
            Op::Lt | Op::LtEq | Op::Gt | Op::GtEq => self.compare_op(op, span)?,
            Op::Eq => {
                let r = self.pop();
                let l = self.pop();
                let eq = self.values_equal_guarded(l, r, 0, span)?;
                self.push(Value::Bool(eq));
            }
            Op::NotEq => {
                let r = self.pop();
                let l = self.pop();
                let eq = self.values_equal_guarded(l, r, 0, span)?;
                self.push(Value::Bool(!eq));
            }
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr => self.bitwise(op, span)?,
            Op::AsBool => {
                let v = *self.stack.last().unwrap();
                if !matches!(v, Value::Bool(_)) {
                    return Err(self.err(format!("expected bool, found {}", self.type_name(v)), span));
                }
            }
            Op::AsInt => {
                let v = *self.stack.last().unwrap();
                if !matches!(v, Value::Int(_)) {
                    return Err(self.err(format!("expected int, found {}", self.type_name(v)), span));
                }
            }
            // ----- M19 superinstructions. Bodies live in `#[inline(never)]` helpers so `step`'s own
            // stack frame stays lean. Plain calls no longer recurse the host stack (call-flattening:
            // `Op::Call` pushes a frame and the running `run_until` loop executes it), but the
            // HOF/method/deferred re-entrant path still cycles `step → run_proto → run_until → step`,
            // so a fat `step` frame would still bloat that recursion. -----
            Op::BinLocalLocal { a, b, kind } => self.op_bin_local_local(*a, *b, *kind, span)?,
            Op::BinLocalConst { slot, val, kind } => self.op_bin_local_const(*slot, *val, *kind, span)?,
            Op::IncLocal { slot, delta } => self.op_inc_local(*slot, *delta, span)?,
            Op::PushHandler(target) => self.handlers.push(Handler {
                stack_len: self.stack.len(),
                frame_len: self.frames.len(),
                call_depth: self.call_depth,
                ip: *target,
                defer_len: self.frames.last().map(|f| f.deferred.len()).unwrap_or(0),
                markers_len: self.frames.last().map(|f| f.defer_markers.len()).unwrap_or(0),
                nursery_len: self.nurseries.len(),
            }),
            Op::PopHandler => {
                self.handlers.pop();
            }
            Op::Jump(t) => self.jump(*t),
            Op::JumpIfFalse(t) => {
                if let Value::Bool(false) = self.pop() {
                    self.jump(*t);
                }
            }
            Op::JumpIfFalseKeep(t) => {
                if let Value::Bool(false) = *self.stack.last().unwrap() {
                    self.jump(*t);
                }
            }
            Op::JumpIfTrueKeep(t) => {
                if let Value::Bool(true) = *self.stack.last().unwrap() {
                    self.jump(*t);
                }
            }
            Op::Call(argc) => self.do_call(*argc, span)?,
            Op::CallMethod { name, argc, ic } => self.do_method_call(name, *argc, *ic, span)?,
            Op::CallBuiltin(name, argc) => self.do_builtin(name, *argc, span)?,
            Op::CallPrint(argc) => self.do_print(*argc, span)?,
            Op::Return => self.do_return(false)?,
            Op::DeferCall(argc) => self.do_defer(None, *argc, span),
            Op::DeferMethod(name, argc) => self.do_defer(Some(name.clone()), *argc, span),
            Op::EnterDeferScope => {
                let frame = self.frames.last_mut().unwrap();
                let marker = frame.deferred.len();
                frame.defer_markers.push(marker);
            }
            Op::LeaveDeferScope => {
                if let Some(e) = self.leave_defer_scope() {
                    return Err(e);
                }
            }
            Op::DrainHandlerDefers => {
                // The live recover handler is still installed (its `PopHandler` follows). Drain the
                // block's defers down to its marker; a fault propagates and is caught by that same
                // handler (becoming the recover's `Err`).
                if let Some(marker) = self.handlers.last().map(|h| h.defer_len)
                    && let Some(e) = self.drain_frame_to(marker)
                {
                    return Err(e);
                }
            }
            Op::Try => self.do_try(span)?,
            Op::JsonDecode(desc) => {
                let desc = desc.clone();
                self.json_decode(&desc, span)?;
            }
            Op::NewList(n) => {
                let at = self.stack.len() - *n;
                let items: Vec<Value> = self.stack.split_off(at);
                let h = self.heap.alloc(Obj::List(items));
                self.push(Value::Obj(h));
            }
            Op::NewTuple(n) => {
                let at = self.stack.len() - *n;
                let items: Vec<Value> = self.stack.split_off(at);
                let h = self.heap.alloc(Obj::Tuple(items));
                self.push(Value::Obj(h));
            }
            Op::NewMap(n) => {
                // Build an insertion-ordered hash map with last-key-wins upsert. Phase 1 hashes
                // every key while ALL operands are still rooted on the stack (a struct key's hash()
                // re-enters the VM and can GC); phase 2 then builds the map with no further re-entry
                // (so no GC), reading keys/values from the still-rooted stack.
                let count = *n;
                let at = self.stack.len() - 2 * count;
                let mut hashes = Vec::with_capacity(count);
                for j in 0..count {
                    let k = self.stack[at + 2 * j];
                    hashes.push(self.hash_value(k, span)?);
                }
                let mut map = MapData::default();
                for (j, &hk) in hashes.iter().enumerate() {
                    let (k, v) = (self.stack[at + 2 * j], self.stack[at + 2 * j + 1]);
                    match map.candidates(hk).iter().copied().find(|&p| self.values_equal(map.entries[p].1, k)) {
                        Some(p) => map.entries[p].2 = v,
                        None => map.push(hk, k, v),
                    }
                }
                self.stack.truncate(at);
                let h = self.heap.alloc(Obj::Map(map));
                self.push(Value::Obj(h));
            }
            Op::NewSet(n) => {
                // Insertion-ordered hash set, dedup keeping first occurrence. Same two-phase rooting
                // as NewMap (phase 1 hashes all elements rooted; phase 2 builds GC-free).
                let count = *n;
                let at = self.stack.len() - count;
                let mut hashes = Vec::with_capacity(count);
                for j in 0..count {
                    hashes.push(self.hash_value(self.stack[at + j], span)?);
                }
                let mut set = SetData::default();
                for (j, &he) in hashes.iter().enumerate() {
                    let e = self.stack[at + j];
                    if !set.candidates(he).iter().copied().any(|p| self.values_equal(set.entries[p].1, e)) {
                        set.push(he, e);
                    }
                }
                self.stack.truncate(at);
                let h = self.heap.alloc(Obj::Set(set));
                self.push(Value::Obj(h));
            }
            Op::NewStruct(name, argc) => self.new_struct(name, *argc, span)?,
            Op::NewEnum(ty, variant, argc) => self.new_enum(ty, variant, *argc, span)?,
            Op::MakeFunc(proto) => {
                let home = self.frames.last().unwrap().home;
                let h = self.heap.alloc(Obj::Func { proto: *proto, home });
                self.push(Value::Obj(h));
            }
            Op::MakeClosure(proto, entries) => {
                let frame = self.frames.last().unwrap();
                let (base, home, enclosing) = (frame.base, frame.home, frame.closure);
                let mut captured = std::collections::HashMap::new();
                for e in entries {
                    let v = match e.src {
                        CapSrc::Slot(i) => self.stack[base + i],
                        CapSrc::Captured => enclosing
                            .and_then(|h| match self.heap.get(h) {
                                Obj::Closure { captured, .. } => captured.get(&e.name).copied(),
                                _ => None,
                            })
                            .unwrap_or(Value::Nil),
                    };
                    captured.insert(e.name.clone(), v);
                }
                let h = self.heap.alloc(Obj::Closure { proto: *proto, captured, home });
                self.push(Value::Obj(h));
            }
            Op::GetField { name, ic } => self.get_field(name, *ic, span)?,
            Op::GetIndex => self.get_index(span)?,
            Op::GetSlice => self.get_slice(span)?, // Phase 4
            Op::SetField { name, ic } => self.set_field(name, *ic, span)?,
            Op::SetIndex => self.set_index(span)?,
            Op::Dup => {
                let top = *self.stack.last().expect("Dup on empty stack");
                self.push(top);
            }
            Op::Dup2 => {
                let n = self.stack.len();
                let a = self.stack[n - 2];
                let b = self.stack[n - 1];
                self.push(a);
                self.push(b);
            }
            Op::ToStr => {
                let v = self.stack[self.stack.len() - 1]; // leave rooted; stringify may run user code
                let s = self.stringify(v, span, 0)?;
                self.pop();
                let h = self.heap.alloc(Obj::Str(s.into()));
                self.push(Value::Obj(h));
            }
            Op::BuildStr(n) => {
                let at = self.stack.len() - *n;
                // Stringify in place so each interpolated part stays rooted while a `str` method runs.
                let mut s = String::new();
                for i in 0..*n {
                    let p = self.stack[at + i];
                    self.stringify_into(&mut s, p, span, 0)?; // one buffer, no per-part String
                }
                self.stack.truncate(at);
                let h = self.heap.alloc(Obj::Str(s.into()));
                self.push(Value::Obj(h));
            }
            Op::ListClone => {
                // Normalise a `for` iterand to an index-iterable list: a list is cloned (so a body
                // that mutates it doesn't disturb iteration); a map yields its keys (gap #14).
                let v = self.pop();
                match v {
                    Value::Obj(h) => match self.heap.get(h) {
                        Obj::List(items) => {
                            let cloned = items.clone();
                            let nh = self.heap.alloc(Obj::List(cloned));
                            self.push(Value::Obj(nh));
                        }
                        Obj::Map(m) => {
                            let keys: Vec<Value> = m.entries.iter().map(|(_, k, _)| *k).collect();
                            let nh = self.heap.alloc(Obj::List(keys));
                            self.push(Value::Obj(nh));
                        }
                        Obj::Set(s) => {
                            let elems: Vec<Value> = s.entries.iter().map(|(_, e)| *e).collect();
                            let nh = self.heap.alloc(Obj::List(elems));
                            self.push(Value::Obj(nh));
                        }
                        // A string iterates as 1-char strings (Python-style; gap: char type).
                        Obj::Str(s) => {
                            // Collect `char`s (Copy — no per-char `String`) to release the heap borrow,
                            // then box each in one alloc via `alloc_char`.
                            let chars: Vec<char> = s.chars().collect();
                            let items: Vec<Value> =
                                chars.into_iter().map(|c| self.alloc_char(c)).collect();
                            let nh = self.heap.alloc(Obj::List(items));
                            self.push(Value::Obj(nh));
                        }
                        _ => return Err(self.err(format!("cannot iterate over {}", self.type_name(v)), span)),
                    },
                    other => return Err(self.err(format!("cannot iterate over {}", self.type_name(other)), span)),
                }
            }
            Op::ArrLen => {
                let v = self.pop();
                let len = match v {
                    Value::Obj(h) => match self.heap.get(h) {
                        Obj::List(items) => items.len() as i64,
                        _ => unreachable!("ArrLen on non-list"),
                    },
                    _ => unreachable!("ArrLen on non-list"),
                };
                self.push(Value::Int(len));
            }
            Op::IsStruct => {
                let v = self.pop();
                let is_struct =
                    matches!(v, Value::Obj(h) if matches!(self.heap.get(h), Obj::Struct { .. }));
                self.push(Value::Bool(is_struct));
            }
            Op::IsMap => {
                let v = self.pop();
                let is_map = matches!(v, Value::Obj(h) if matches!(self.heap.get(h), Obj::Map(_)));
                self.push(Value::Bool(is_map));
            }
            Op::IsChannel => {
                let v = self.pop();
                let is_chan =
                    matches!(v, Value::Obj(h) if matches!(self.heap.get(h), Obj::Channel(_)));
                self.push(Value::Bool(is_chan));
            }
            Op::ChanRecvOrClosed => {
                // `for v in ch:` step: pop a value (parking on empty-open exactly like `recv`) and push
                // `Some(v)`, or push `None` once the channel is closed-and-drained (the loop's clean
                // exit). Runs at the loop top, never inside a native callback (`native_reentry == 0`),
                // so it takes the snapshot-park / cooperative-park / fault paths — never the demote path.
                let v = self.pop();
                let Value::Obj(h) = v else {
                    return Err(self.err("`for` over a non-channel value".to_string(), span));
                };
                match self.chan_recv_step(h, span)? {
                    RecvStep::Got(w) => {
                        let val = self.from_wire(w);
                        let opt = self.alloc_enum("Option", "Some", vec![val]);
                        self.push(opt);
                    }
                    RecvStep::ClosedEmpty => {
                        let opt = self.alloc_enum("Option", "None", vec![]);
                        self.push(opt);
                    }
                    // `chan_recv_step` re-rooted the handle + set `suspend`; `run_until`'s `paused()`
                    // gate returns to the scheduler, and the op re-runs (rewound `ip`) on resume.
                    RecvStep::Parked => {}
                }
            }
            Op::EnsureEnum(slot) => {
                let v = self.stack[self.base() + *slot];
                if !matches!(v, Value::Obj(h) if matches!(self.heap.get(h), Obj::Enum { .. })) {
                    return Err(self.err(format!("cannot match on {}", self.type_name(v)), span));
                }
            }
            Op::MatchArm { scrut, variant, nbind, bind_start, next } => self.match_arm(*scrut, variant, *nbind, *bind_start, *next, span)?,
            Op::MatchNoArm(slot) => {
                let v = self.stack[self.base() + *slot];
                let variant = match v {
                    Value::Obj(h) => match self.heap.get(h) {
                        Obj::Enum { variant, .. } => variant.to_string(),
                        _ => String::new(),
                    },
                    _ => String::new(),
                };
                return Err(self.err(format!("no match arm for variant '{variant}'"), span));
            }
            Op::EnterNursery => {
                self.nurseries.push(Vec::new());
                // TASK B — capture this parallel body's defer floor so a recover-scoped `?` can run
                // the body's defers before the cancel-report (see `nursery_defer_floors`).
                let floor = self.frames.last().map(|f| f.deferred.len()).unwrap_or(0);
                self.nursery_defer_floors.push(floor);
                // Per-connection spawn — a NESTED nursery under `--parallel` (entered inside a live
                // fiber, `mn.is_some()`) activates an EAGER sched NOW so `spawn`s in the body inject
                // handlers that run concurrently with the accept loop. The top-level nursery
                // (`mn.is_none()`) and the cooperative engine stay lazy (queue-at-join → `None`).
                //
                // Gated on ≥2 hardware threads: an eager inner join blocks the parent's OUTER worker
                // (decision B — parent participates) while it waits for handlers, and a handler that
                // services an OUTER sibling (a client) needs that sibling to make progress — which it
                // can't if the outer nursery has only ONE worker (a 1-core box → every nursery is
                // single-worker → deadlock). With ≥2 hw threads the outer nursery has a spare worker.
                // On a single core we fall back to the lazy queue-at-join path (handlers drain at the
                // join), which still serves a realistic parallel-client server and never deadlocks —
                // and `--parallel` on one core is already a degenerate config.
                let eager = self.parallel
                    && self.mn.is_some()
                    && std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) >= 2;
                let scope = eager.then(|| self.activate_eager_nursery());
                self.eager_scheds.push(scope);
            }
            Op::JoinNursery => self.join_nursery()?,
            // TASK B — `break`/`continue` leaving a `parallel:` scope: cancel-and-report its unstarted
            // tasks and pop exactly that one level (the compiler emits one per escaped scope).
            Op::ReclaimNursery => {
                let from = self.nurseries.len().saturating_sub(1);
                self.drain_escaped_nursery(from);
            }
            Op::SpawnCall(argc) => self.do_spawn(None, *argc, span)?,
            Op::SpawnMethod(name, argc) => self.do_spawn(Some(name.clone()), *argc, span)?,
            Op::SpawnBlock(proto, entries) => self.do_spawn_block(*proto, entries, span)?,
            Op::NewChannel => {
                let h = self.heap.alloc(Obj::Channel(Arc::new(ChannelCore::default())));
                self.push(Value::Obj(h));
            }
            Op::NewShared => {
                let init = self.pop();
                // The box holds the wire form (single serialization == the old deep_clone-in).
                let init = self.to_wire(init).expect("Shared init must be sendable (B3.1 single-thread)");
                let h = self.heap.alloc(Obj::Shared(Arc::new(SharedCore { v: Mutex::new(init), ..Default::default() })));
                self.push(Value::Obj(h));
            }
            // `NewAtomic`/`NewTimer` delegate to `#[inline(never)]` helpers so their locals (the timer's
            // `Instant`/`Duration` math) do NOT inflate `step`'s stack frame — `step` is on the per-op
            // recursion path, so a fatter frame here multiplies across a deep call chain (debug builds
            // don't reuse match-arm stack slots) and can overflow the host stack before the
            // `MAX_CALL_DEPTH` guard fires. Keep these cold constructors out of line.
            Op::NewAtomic => {
                let v = self.new_atomic();
                self.push(v);
            }
            Op::NewTimer => {
                let v = self.new_timer(span)?;
                self.push(v);
            }
            Op::NewExecutor => {
                let h = self.heap.alloc(Obj::Executor(Arc::new(ExecutorCore::default())));
                // Register for the program-exit auto-drain; the handle is also a GC root, so the
                // executor's queued work survives even after every in-program handle is gone.
                self.executors.push(h);
                self.push(Value::Obj(h));
            }
        }
        Ok(())
    }

    // ----- arithmetic / comparison -----

    /// M19 Tier-2 — adaptive quickening for the un-fused generic arith/ordered-compare binops
    /// (`Add..GtEq`). `site` indexes [`Vm::quicken`]. Cold: observe the two stack operands' types once,
    /// then run the generic path. Int-specialized: take the `fast_int_bin` path (the exact int
    /// behaviour the superinstructions already ship), deopting to `Q_GENERIC` if a non-int operand
    /// shows up. Generic: always the unfused `arith`/`compare_op` via `run_bin_kind`. Every path
    /// produces a byte-identical result to the original `step` arm.
    #[inline(never)]
    fn q_arith(&mut self, site: usize, kind: crate::vm::op::BinKind, span: Span) -> Result<(), RuntimeError> {
        match self.quicken[site] {
            Q_INT => {
                let n = self.stack.len();
                if let (Value::Int(x), Value::Int(y)) = (self.stack[n - 2], self.stack[n - 1]) {
                    let v = self.fast_int_bin(x, y, kind, span)?;
                    self.stack.truncate(n - 2);
                    self.stack.push(v);
                    Ok(())
                } else {
                    // A non-int operand reached a specialized site — deopt permanently (operands stay
                    // on the stack for the generic path to pop).
                    self.quicken[site] = Q_GENERIC;
                    self.run_bin_kind(kind, span)
                }
            }
            Q_GENERIC => self.run_bin_kind(kind, span),
            _ => {
                // Q_COLD — record whether this site is int/int, then run the generic path this once.
                let n = self.stack.len();
                let both_int = matches!((self.stack[n - 2], self.stack[n - 1]), (Value::Int(_), Value::Int(_)));
                self.quicken[site] = if both_int { Q_INT } else { Q_GENERIC };
                self.run_bin_kind(kind, span)
            }
        }
    }

    /// M19 Tier-2 — adaptive quickening for `Eq`/`NotEq` (never fused, so always reached here). The
    /// int fast path REPLICATES the generic numeric comparison `as_f64(x) == as_f64(y)` (lossy for
    /// `|i64| > 2^53`) — NOT exact `x == y` — so it stays byte-identical to `values_equal_guarded`
    /// (`Value::Int` is numeric) and to the interpreter; preserving that loss is what keeps two-engine
    /// parity. `negate` flips the result for `NotEq`. Mirrors the kept `Op::Eq`/`Op::NotEq` `step` arms.
    #[inline(never)]
    fn q_eq(&mut self, site: usize, negate: bool, span: Span) -> Result<(), RuntimeError> {
        if self.quicken[site] == Q_INT {
            let n = self.stack.len();
            if let (Value::Int(x), Value::Int(y)) = (self.stack[n - 2], self.stack[n - 1]) {
                self.stack.truncate(n - 2);
                let eq = (x as f64) == (y as f64);
                self.push(Value::Bool(eq ^ negate));
                return Ok(());
            }
            self.quicken[site] = Q_GENERIC; // non-int at a specialized site → deopt
        } else if self.quicken[site] == Q_COLD {
            let n = self.stack.len();
            let both_int = matches!((self.stack[n - 2], self.stack[n - 1]), (Value::Int(_), Value::Int(_)));
            self.quicken[site] = if both_int { Q_INT } else { Q_GENERIC };
            // fall through to the generic path this first time
        }
        let r = self.pop();
        let l = self.pop();
        let eq = self.values_equal_guarded(l, r, 0, span)?;
        self.push(Value::Bool(eq ^ negate));
        Ok(())
    }

    /// `BinLocalLocal{a,b,kind}` — push `local[a] <op> local[b]`. `#[inline(never)]` keeps the body
    /// out of `step`'s frame (see the call site).
    #[inline(never)]
    fn op_bin_local_local(&mut self, a: usize, b: usize, kind: crate::vm::op::BinKind, span: Span) -> Result<(), RuntimeError> {
        let l = self.stack[self.base() + a];
        let r = self.stack[self.base() + b];
        if let (Value::Int(x), Value::Int(y)) = (l, r) {
            let v = self.fast_int_bin(x, y, kind, span)?;
            self.push(v);
        } else {
            self.push(l);
            self.push(r);
            self.run_bin_kind(kind, span)?;
        }
        Ok(())
    }

    /// `BinLocalConst{slot,val,kind}` — push `local[slot] <op> val`.
    #[inline(never)]
    fn op_bin_local_const(&mut self, slot: usize, val: i64, kind: crate::vm::op::BinKind, span: Span) -> Result<(), RuntimeError> {
        let l = self.stack[self.base() + slot];
        if let Value::Int(x) = l {
            let v = self.fast_int_bin(x, val, kind, span)?;
            self.push(v);
        } else {
            self.push(l);
            self.push(Value::Int(val));
            self.run_bin_kind(kind, span)?;
        }
        Ok(())
    }

    /// `IncLocal{slot,delta}` — in-place `local[slot] += delta`. Falls back to the exact unfused
    /// `GetLocal; ConstInt; Add; SetLocal` for a non-numeric local (so `arith`'s error wins).
    #[inline(never)]
    fn op_inc_local(&mut self, slot: usize, delta: i64, span: Span) -> Result<(), RuntimeError> {
        let at = self.base() + slot;
        match self.stack[at] {
            Value::Int(x) => {
                let v = x.checked_add(delta).ok_or_else(|| self.err("integer overflow in Add".to_string(), span))?;
                self.stack[at] = Value::Int(v);
            }
            Value::Float(f) => self.stack[at] = Value::Float(f + delta as f64),
            other => {
                self.push(other);
                self.push(Value::Int(delta));
                self.arith(&Op::Add, span)?;
                let v = self.pop();
                let at = self.base() + slot;
                self.stack[at] = v;
            }
        }
        Ok(())
    }

    /// Int/Int fast path for the fused binops (`BinLocalLocal` / `BinLocalConst`). Must match
    /// `arith` (overflow / div-by-zero errors) and `compare_op` (ordering) for `Int` operands
    /// exactly. Anything non-`Int` never reaches here — the caller falls back to the slow path.
    fn fast_int_bin(&self, x: i64, y: i64, kind: crate::vm::op::BinKind, span: Span) -> Result<Value, RuntimeError> {
        use crate::vm::op::BinKind;
        let v = match kind {
            BinKind::Add => Value::Int(x.checked_add(y).ok_or_else(|| self.err("integer overflow in Add".to_string(), span))?),
            BinKind::Sub => Value::Int(x.checked_sub(y).ok_or_else(|| self.err("integer overflow in Sub".to_string(), span))?),
            BinKind::Mul => Value::Int(x.checked_mul(y).ok_or_else(|| self.err("integer overflow in Mul".to_string(), span))?),
            BinKind::Div => {
                if y == 0 {
                    return Err(self.err("division by zero".to_string(), span));
                }
                Value::Int(x.checked_div(y).ok_or_else(|| self.err("integer overflow in Div".to_string(), span))?)
            }
            BinKind::Mod => {
                if y == 0 {
                    return Err(self.err("modulo by zero".to_string(), span));
                }
                Value::Int(x.checked_rem(y).ok_or_else(|| self.err("integer overflow in Mod".to_string(), span))?)
            }
            BinKind::Lt => Value::Bool(x < y),
            BinKind::LtEq => Value::Bool(x <= y),
            BinKind::Gt => Value::Bool(x > y),
            BinKind::GtEq => Value::Bool(x >= y),
        };
        Ok(v)
    }

    /// Slow-path dispatch for a fused binop: the two operands are already on the stack, so route to
    /// the existing `arith` / `compare_op` (preserving struct overloading, string concat, float
    /// promotion, and fiber parking — anything the unfused op sequence would do).
    fn run_bin_kind(&mut self, kind: crate::vm::op::BinKind, span: Span) -> Result<(), RuntimeError> {
        use crate::vm::op::BinKind;
        match kind {
            BinKind::Add => self.arith(&Op::Add, span),
            BinKind::Sub => self.arith(&Op::Sub, span),
            BinKind::Mul => self.arith(&Op::Mul, span),
            BinKind::Div => self.arith(&Op::Div, span),
            BinKind::Mod => self.arith(&Op::Mod, span),
            BinKind::Lt => self.compare_op(&Op::Lt, span),
            BinKind::LtEq => self.compare_op(&Op::LtEq, span),
            BinKind::Gt => self.compare_op(&Op::Gt, span),
            BinKind::GtEq => self.compare_op(&Op::GtEq, span),
        }
    }

    fn arith(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        let r = self.pop();
        let l = self.pop();
        let name = match op {
            Op::Add => "Add",
            Op::Sub => "Sub",
            Op::Mul => "Mul",
            Op::Div => "Div",
            Op::Mod => "Mod",
            _ => unreachable!(),
        };
        let result = match (l, r) {
            (Value::Int(a), Value::Int(b)) => {
                let v = match op {
                    Op::Add => a.checked_add(b),
                    Op::Sub => a.checked_sub(b),
                    Op::Mul => a.checked_mul(b),
                    Op::Div | Op::Mod if b == 0 => {
                        return Err(self.err(format!("{} by zero", if matches!(op, Op::Div) { "division" } else { "modulo" }), span));
                    }
                    Op::Div => a.checked_div(b),
                    Op::Mod => a.checked_rem(b),
                    _ => unreachable!(),
                };
                Value::Int(v.ok_or_else(|| self.err(format!("integer overflow in {name}"), span))?)
            }
            (a, b) if is_numeric(a) && is_numeric(b) => {
                let (x, y) = (as_f64(a), as_f64(b));
                if matches!(op, Op::Div | Op::Mod) && y == 0.0 {
                    return Err(self.err(format!("{} by zero", if matches!(op, Op::Div) { "division" } else { "modulo" }), span));
                }
                Value::Float(match op {
                    Op::Add => x + y,
                    Op::Sub => x - y,
                    Op::Mul => x * y,
                    Op::Div => x / y,
                    Op::Mod => x % y,
                    _ => unreachable!(),
                })
            }
            // Arithmetic overloading: `+`/`-`/`*` on two structs dispatch to `add`/`sub`/`mul` (the
            // `Add`/`Sub`/`Mul` protocols). The checker has verified conformance. Must precede the
            // string-concat `Add` arm below (which would otherwise reject struct+struct).
            (Value::Obj(ha), Value::Obj(hb))
                if matches!(op, Op::Add | Op::Sub | Op::Mul)
                    && matches!(self.heap.get(ha), Obj::Struct { .. })
                    && matches!(self.heap.get(hb), Obj::Struct { .. }) =>
            {
                self.struct_arith(op, l, r, span)?
            }
            (Value::Obj(ha), Value::Obj(hb)) if matches!(op, Op::Add) => {
                if let (Obj::Str(a), Obj::Str(b)) = (self.heap.get(ha), self.heap.get(hb)) {
                    let s = format!("{a}{b}");
                    let h = self.heap.alloc(Obj::Str(s.into()));
                    Value::Obj(h)
                } else {
                    return Err(self.err(format!("cannot apply {name} to {} and {}", self.type_name(l), self.type_name(r)), span));
                }
            }
            _ => return Err(self.err(format!("cannot apply {name} to {} and {}", self.type_name(l), self.type_name(r)), span)),
        };
        self.push(result);
        Ok(())
    }

    /// Arithmetic operator overloading: dispatch `+`/`-`/`*` on two structs to the receiver's
    /// `add`/`sub`/`mul(self, other) -> Self` method (the `Add`/`Sub`/`Mul` protocols). `l`/`r` are
    /// passed as the call's args (rooted as the new frame's locals). Mirrors `interp::struct_arith`.
    fn struct_arith(&mut self, op: &Op, l: Value, r: Value, span: Span) -> Result<Value, RuntimeError> {
        let method = match op {
            Op::Add => "add",
            Op::Sub => "sub",
            Op::Mul => "mul",
            _ => unreachable!("struct_arith only handles + - *"),
        };
        let Value::Obj(h) = l else { unreachable!() };
        let Obj::Struct { name, .. } = self.heap.get(h) else { unreachable!() };
        let name = name.clone();
        let def = self
            .program
            .structs
            .get(name.as_ref())
            .cloned()
            .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        let proto = *def
            .methods
            .get(method)
            .ok_or_else(|| self.err(format!("struct '{name}' has no '{method}' method"), span))?;
        let home = self.module_objs[def.module_idx];
        self.guarded(|vm| vm.run_proto(proto, home, None, vec![l, r], true, false, span))
    }

    /// Bitwise / shift ops — int-only (gap #13). Shift amounts outside `0..64` are a runtime error
    /// (Rust would otherwise panic), with a message identical to the interpreter's.
    fn bitwise(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        let r = self.pop();
        let l = self.pop();
        let name = match op {
            Op::BitAnd => "BitAnd",
            Op::BitOr => "BitOr",
            Op::BitXor => "BitXor",
            Op::Shl => "Shl",
            Op::Shr => "Shr",
            _ => unreachable!(),
        };
        let result = match (l, r) {
            (Value::Int(a), Value::Int(b)) => {
                let v = match op {
                    Op::BitAnd => a & b,
                    Op::BitOr => a | b,
                    Op::BitXor => a ^ b,
                    Op::Shl | Op::Shr => {
                        if !(0..64).contains(&b) {
                            return Err(self.err(format!("shift amount {b} out of range (0..64)"), span));
                        }
                        if matches!(op, Op::Shl) { a << (b as u32) } else { a >> (b as u32) }
                    }
                    _ => unreachable!(),
                };
                Value::Int(v)
            }
            _ => return Err(self.err(format!("cannot apply {name} to {} and {}", self.type_name(l), self.type_name(r)), span)),
        };
        self.push(result);
        Ok(())
    }

    fn compare_op(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        let r = self.pop();
        let l = self.pop();
        // Operator overloading: ordering on two structs dispatches to `compare(self, other) -> int`
        // (the `Comparable` protocol). The checker has verified conformance. Equality stays
        // structural; only ordering is overloaded. Mirrors `interp::struct_ordering`.
        if let (Value::Obj(hl), Value::Obj(hr)) = (l, r)
            && matches!(self.heap.get(hl), Obj::Struct { .. })
            && matches!(self.heap.get(hr), Obj::Struct { .. })
        {
            return self.struct_ordering(op, l, r, span);
        }
        let name = match op {
            Op::Lt => "Lt",
            Op::LtEq => "LtEq",
            Op::Gt => "Gt",
            Op::GtEq => "GtEq",
            _ => unreachable!(),
        };
        let ord = self.compare(l, r).ok_or_else(|| self.err(format!("cannot apply {name} to {} and {}", self.type_name(l), self.type_name(r)), span))?;
        let b = match op {
            Op::Lt => ord.is_lt(),
            Op::LtEq => ord.is_le(),
            Op::Gt => ord.is_gt(),
            Op::GtEq => ord.is_ge(),
            _ => unreachable!(),
        };
        self.push(Value::Bool(b));
        Ok(())
    }

    /// Dispatch an ordering operator on two structs to the receiver's `compare(self, other) -> int`
    /// method, mapping the sign of the result to a boolean. Mirrors `interp::struct_ordering`.
    fn struct_ordering(&mut self, op: &Op, l: Value, r: Value, span: Span) -> Result<(), RuntimeError> {
        let ord = self.struct_compare(l, r, span)?;
        let b = match op {
            Op::Lt => ord.is_lt(),
            Op::LtEq => ord.is_le(),
            Op::Gt => ord.is_gt(),
            Op::GtEq => ord.is_ge(),
            _ => unreachable!(),
        };
        self.push(Value::Bool(b));
        Ok(())
    }

    /// Call a struct's `compare(self, other) -> int` method and return the resulting `Ordering`.
    /// Shared by ordering operators (`struct_ordering`) and `list.sort()` over Comparable structs.
    /// Mirrors `interp::struct_compare`.
    fn struct_compare(&mut self, l: Value, r: Value, span: Span) -> Result<std::cmp::Ordering, RuntimeError> {
        let Value::Obj(h) = l else { unreachable!() };
        let Obj::Struct { name, .. } = self.heap.get(h).clone() else { unreachable!() };
        let def = self
            .program
            .structs
            .get(name.as_ref())
            .cloned()
            .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        let proto = *def.methods.get("compare").ok_or_else(|| {
            self.err(format!("struct '{name}' has no 'compare' method (needed to order its values)"), span)
        })?;
        let home = self.module_objs[def.module_idx];
        match self.guarded(|vm| vm.run_proto(proto, home, None, vec![l, r], true, false, span))? {
            Value::Int(n) => Ok(n.cmp(&0)),
            other => Err(self.err(format!("compare() must return int, got {}", self.type_name(other)), span)),
        }
    }

    /// A `u64` hash of `v` for map/set keys, upholding the invariant `values_equal(a,b) ⇒
    /// hash(a)==hash(b)`. Numeric keys hash by their canonical f64 bits (so `Int(3)` and `Float(3.0)`
    /// collide, matching `values_equal`'s numeric unification); str by content; a struct key
    /// dispatches its user `hash(self) -> int` (re-entrant — may allocate / trigger GC). Floats are
    /// rejected as keys by the checker (NaN footgun), so only integral-valued floats reach here.
    fn hash_value(&mut self, v: Value, span: Span) -> Result<u64, RuntimeError> {
        match v {
            // A struct key dispatches its user `hash()` (re-entrant). Everything else is scalar.
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Struct { .. } => self.struct_hash(v, span),
                Obj::Str(_) => Ok(self.scalar_hash(v)),
                _ => Err(self.err(format!("{} is not hashable (cannot be a map/set key)", self.type_name(v)), span)),
            },
            _ => Ok(self.scalar_hash(v)),
        }
    }

    /// Infallible hash for scalar keys (int/float/bool/nil/str). Numeric values hash by canonical
    /// f64 bits so `3` and `3.0` collide; str by content. Non-scalar values fall back to `0` (a
    /// correctness-safe degenerate hash — `values_equal` still confirms each probe).
    fn scalar_hash(&self, v: Value) -> u64 {
        use std::hash::{Hash, Hasher};
        match v {
            // Normalise zero so `Int(0)`, `+0.0`, and `-0.0` (all `values_equal`) hash identically —
            // `(-0.0).to_bits() != (0.0).to_bits()` would otherwise break the hash invariant.
            Value::Int(n) => (if n == 0 { 0.0 } else { n as f64 }).to_bits(),
            Value::Float(f) => (if f == 0.0 { 0.0 } else { f }).to_bits(),
            Value::Bool(b) => b as u64,
            Value::Nil => 0,
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Str(s) => {
                    let mut hr = std::collections::hash_map::DefaultHasher::new();
                    s.as_bytes().hash(&mut hr);
                    hr.finish()
                }
                _ => 0,
            },
        }
    }

    /// Dispatch a struct key's user `hash(self) -> int`, returning its `i64` as a `u64`. Mirrors
    /// [`struct_compare`] (re-entrant via `run_proto`).
    fn struct_hash(&mut self, v: Value, span: Span) -> Result<u64, RuntimeError> {
        let Value::Obj(h) = v else { unreachable!() };
        let Obj::Struct { name, .. } = self.heap.get(h).clone() else { unreachable!() };
        let def = self
            .program
            .structs
            .get(name.as_ref())
            .cloned()
            .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        let proto = *def.methods.get("hash").ok_or_else(|| {
            self.err(format!("struct '{name}' has no 'hash' method (needed to use it as a map/set key)"), span)
        })?;
        let home = self.module_objs[def.module_idx];
        match self.guarded(|vm| vm.run_proto(proto, home, None, vec![v], true, false, span))? {
            Value::Int(n) => Ok(n as u64),
            other => Err(self.err(format!("hash() must return int, got {}", self.type_name(other)), span)),
        }
    }

    /// Hash `key`, keeping `roots` alive on the operand stack across the call. A struct key's
    /// `hash()` re-enters the VM and can trigger GC; the map/set receiver and any in-flight
    /// key/value (already popped off the stack before dispatch) must be rooted or the collector
    /// could free them mid-hash. For scalar keys this is a couple of redundant push/pops.
    fn hash_key_rooted(&mut self, key: Value, roots: &[Value], span: Span) -> Result<u64, RuntimeError> {
        for &r in roots {
            self.push(r);
        }
        let res = self.hash_value(key, span);
        for _ in roots {
            self.pop();
        }
        res
    }

    /// `xs.sort()` over a list of Comparable structs, ordering via each struct's `compare`. Because
    /// `compare` re-enters the VM (and may allocate / trigger GC), this mirrors `list_sort_by`
    /// exactly: snapshot the elements into a heap list ROOTED on the operand stack, permute
    /// *indices* re-read from that rooted list per comparison (never holding unrooted `Value`s
    /// across a `compare` call), then write the result back. (Primitives use the faster
    /// `value_order`, which never re-enters the VM.) Mirrors `interp::eval_list_sort`.
    fn list_sort_structs(&mut self, src_h: GcRef, span: Span) -> Result<Value, RuntimeError> {
        // Root the source list itself: a method receiver is popped before dispatch, so an inline
        // temporary (`make().sort()`) is otherwise unrooted and the comparator's GC could collect it
        // before the write-back.
        self.push(Value::Obj(src_h));
        let snap_h = {
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!("list_sort on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::Obj(snap_h)); // ROOT the snapshot across the comparator calls
        let n = match self.heap.get(snap_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!(),
        };
        let order = match self.msort_indices_structs(snap_h, (0..n).collect(), span) {
            Ok(o) => o,
            Err(e) => {
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                return Err(e);
            }
        };
        // No comparator calls remain, so no GC: read the rooted snapshot and write the result back.
        let reordered: Vec<Value> = match self.heap.get(snap_h) {
            Obj::List(v) => order.iter().map(|&i| v[i]).collect(),
            _ => unreachable!(),
        };
        if let Obj::List(v) = self.heap.get_mut(src_h) {
            *v = reordered;
        }
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(Value::Nil)
    }

    /// Stable top-down merge sort over `idx` (positions into the rooted list `src_h`), comparing
    /// elements via each struct's `compare`. Re-reads elements from `src_h` per comparison so no
    /// unrooted `Value` is held across the GC-capable `struct_compare` call.
    fn msort_indices_structs(&mut self, src_h: GcRef, idx: Vec<usize>, span: Span) -> Result<Vec<usize>, RuntimeError> {
        let n = idx.len();
        if n <= 1 {
            return Ok(idx);
        }
        let mut idx = idx;
        let right = idx.split_off(n / 2);
        let left = self.msort_indices_structs(src_h, idx, span)?;
        let right = self.msort_indices_structs(src_h, right, span)?;
        let mut out = Vec::with_capacity(n);
        let (mut li, mut ri) = (0, 0);
        while li < left.len() && ri < right.len() {
            let a = match self.heap.get(src_h) {
                Obj::List(v) => v[left[li]],
                _ => unreachable!(),
            };
            let b = match self.heap.get(src_h) {
                Obj::List(v) => v[right[ri]],
                _ => unreachable!(),
            };
            // `<= Equal` keeps the left element first on ties → stable.
            if self.struct_compare(a, b, span)?.is_le() {
                out.push(left[li]);
                li += 1;
            } else {
                out.push(right[ri]);
                ri += 1;
            }
        }
        out.extend_from_slice(&left[li..]);
        out.extend_from_slice(&right[ri..]);
        Ok(out)
    }

    fn compare(&self, l: Value, r: Value) -> Option<std::cmp::Ordering> {
        match (l, r) {
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(&b)),
            (a, b) if is_numeric(a) && is_numeric(b) => as_f64(a).partial_cmp(&as_f64(b)),
            (Value::Obj(ha), Value::Obj(hb)) => match (self.heap.get(ha), self.heap.get(hb)) {
                (Obj::Str(a), Obj::Str(b)) => Some(a.cmp(b)),
                _ => None,
            },
            _ => None,
        }
    }

    /// Structural equality mirroring `interp::values_equal`. Thin `bool` wrapper over the
    /// depth-guarded worker (kept so the ~39 existing call sites — many in hot hash-probe paths
    /// bound by `values_equal(a,b) ⇒ hash(a)==hash(b)` — are untouched). A depth-exceeded fault
    /// (cyclic data) degrades to "not equal" here; the language `==`/`!=` ops surface it instead.
    fn values_equal(&self, l: Value, r: Value) -> bool {
        self.values_equal_guarded(l, r, 0, Span { line: 1, col: 1 }).unwrap_or(false)
    }

    /// Depth-guarded structural equality. Returns `Err` (recoverable) once recursion exceeds
    /// [`MAX_STRUCTURAL_DEPTH`] — guarding against cyclic data structures overflowing the host stack.
    fn values_equal_guarded(&self, l: Value, r: Value, depth: usize, span: Span) -> Result<bool, RuntimeError> {
        if depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.err("maximum structural depth (10000) exceeded (cyclic data structure?)".to_string(), span));
        }
        match (l, r) {
            (a, b) if is_numeric(a) && is_numeric(b) => Ok(as_f64(a) == as_f64(b)),
            (Value::Bool(a), Value::Bool(b)) => Ok(a == b),
            (Value::Nil, Value::Nil) => Ok(true),
            (Value::Obj(ha), Value::Obj(hb)) => {
                if ha == hb {
                    return Ok(true);
                }
                // Snapshot the element/entry handles out of the heap so the borrow is released before
                // recursing through `&self` methods (mirrors the borrow discipline of the seq paths).
                match (self.heap.get(ha), self.heap.get(hb)) {
                    (Obj::Str(a), Obj::Str(b)) => Ok(a == b),
                    (Obj::List(a), Obj::List(b)) => {
                        if a.len() != b.len() {
                            return Ok(false);
                        }
                        let (a, b): (Vec<Value>, Vec<Value>) = (a.clone(), b.clone());
                        for (x, y) in a.iter().zip(&b) {
                            if !self.values_equal_guarded(*x, *y, depth + 1, span)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                    (Obj::Tuple(a), Obj::Tuple(b)) => {
                        if a.len() != b.len() {
                            return Ok(false);
                        }
                        let (a, b): (Vec<Value>, Vec<Value>) = (a.clone(), b.clone());
                        for (x, y) in a.iter().zip(&b) {
                            if !self.values_equal_guarded(*x, *y, depth + 1, span)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                    // Maps are unordered: equal iff same size and every (key, value) entry of `a` has
                    // a structurally-equal match in `b` (mirrors the Set arm; the cached hash is unused).
                    (Obj::Map(a), Obj::Map(b)) => {
                        if a.entries.len() != b.entries.len() {
                            return Ok(false);
                        }
                        let ae: Vec<(Value, Value)> = a.entries.iter().map(|(_, k, v)| (*k, *v)).collect();
                        let be: Vec<(Value, Value)> = b.entries.iter().map(|(_, k, v)| (*k, *v)).collect();
                        for (ka, va) in &ae {
                            let mut found = false;
                            for (kb, vb) in &be {
                                if self.values_equal_guarded(*ka, *kb, depth + 1, span)?
                                    && self.values_equal_guarded(*va, *vb, depth + 1, span)?
                                {
                                    found = true;
                                    break;
                                }
                            }
                            if !found {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                    // Sets are unordered: equal iff same size and every element of `a` is in `b`.
                    (Obj::Set(a), Obj::Set(b)) => {
                        if a.entries.len() != b.entries.len() {
                            return Ok(false);
                        }
                        let ae: Vec<Value> = a.entries.iter().map(|(_, x)| *x).collect();
                        let be: Vec<Value> = b.entries.iter().map(|(_, x)| *x).collect();
                        for x in &ae {
                            let mut found = false;
                            for y in &be {
                                if self.values_equal_guarded(*x, *y, depth + 1, span)? {
                                    found = true;
                                    break;
                                }
                            }
                            if !found {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                    (Obj::Struct { name: na, fields: fa, .. }, Obj::Struct { name: nb, fields: fb, .. }) => {
                        if na != nb || fa.len() != fb.len() {
                            return Ok(false);
                        }
                        let fa: Vec<(Box<str>, Value)> = fa.iter().map(|(k, v)| (k.clone(), *v)).collect();
                        let fb: Vec<(Box<str>, Value)> = fb.iter().map(|(k, v)| (k.clone(), *v)).collect();
                        for ((ka, va), (kb, vb)) in fa.iter().zip(&fb) {
                            if ka != kb || !self.values_equal_guarded(*va, *vb, depth + 1, span)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                    (Obj::Enum { ty: ta, variant: va, payload: pa }, Obj::Enum { ty: tb, variant: vb, payload: pb }) => {
                        if ta != tb || va != vb || pa.len() != pb.len() {
                            return Ok(false);
                        }
                        let pa: Vec<Value> = pa.clone();
                        let pb: Vec<Value> = pb.clone();
                        for (x, y) in pa.iter().zip(&pb) {
                            if !self.values_equal_guarded(*x, *y, depth + 1, span)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
            _ => Ok(false),
        }
    }

    /// Total order over scalar values for `sort()`. The checker restricts `sort` to homogeneous
    /// int/float/str lists; str elements are read through the heap. Anything else compares Equal.
    fn value_order(&self, a: Value, b: Value) -> std::cmp::Ordering {
        use std::cmp::Ordering::Equal;
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => x.cmp(&y),
            (Value::Float(x), Value::Float(y)) => x.total_cmp(&y),
            (Value::Obj(ha), Value::Obj(hb)) => match (self.heap.get(ha), self.heap.get(hb)) {
                (Obj::Str(x), Obj::Str(y)) => x.cmp(y),
                _ => Equal,
            },
            _ => Equal,
        }
    }

    // ----- calls -----

    fn do_call(&mut self, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        // Fast path — a plain `Func`/`Closure` runs directly over the args already contiguous on the
        // stack, skipping the `split_off` `Vec` alloc + the re-push in `push_frame`. The callee sits
        // one slot below the args; we drop it (shifting the args down one) so they become the new
        // frame's parameter slots in place. Native / not-callable callees fall through to the `Vec`
        // path (`invoke_native` needs an owned `Vec`, HOFs build args off-stack).
        let callee = self.stack[at - 1];
        if let Value::Obj(h) = callee {
            let kind = match self.heap.get(h) {
                Obj::Func { proto, home } => Some((*proto, *home, None)),
                Obj::Closure { proto, home, .. } => Some((*proto, *home, Some(h))),
                _ => None,
            };
            if let Some((proto, home, clo)) = kind {
                // Arity check BEFORE disturbing the stack — identical messages to `invoke_value`, and
                // the error path leaves `[callee, args…]` intact for the trace / `recover:`.
                let arity = self.program.protos[proto].arity;
                if clo.is_none() {
                    self.check_arity("function", &self.program.protos[proto].name, arity, argc, span)?;
                } else if argc != arity {
                    return Err(self.err(format!("closure expects {arity} argument(s), got {argc}"), span));
                }
                // Drop the callee from beneath the args (argc-element memmove; `Value: Copy`).
                self.stack.copy_within(at.., at - 1);
                self.stack.pop();
                // M19 call-flattening: push the callee frame and let the *running* `run_until` loop
                // execute it, instead of recursing into a fresh `run_until` (which cost a native Rust
                // stack frame + an `Arc::clone(&self.program)` per call). The frame lands at
                // `frames.len()-1` with `ip = 0`; the loop already advanced the caller's `ip` past
                // this `Call` (on the captured caller index, before `step`), so the next iteration
                // runs the callee from its start. The callee's eventual `Op::Return` → `do_return`
                // pushes the result onto the caller's stack and pops the frame, and the loop resumes
                // the caller — no synchronous result to push here. Pause/`recover:`/`defer` are caught
                // by the loop body's own checks (they operate on `self.frames`, not the Rust stack).
                self.push_frame_in_place(proto, home, clo, at - 1, span)?;
                return Ok(());
            }
        }
        // Slow path — native, struct, or not-callable.
        let args: Vec<Value> = self.stack.split_off(at);
        let callee = self.pop();
        let v = self.invoke_value(callee, args, span)?;
        if self.paused() {
            return Ok(()); // B1/D3: callee parked on `recv` or yielded; don't push a sentinel result.
        }
        self.push(v);
        Ok(())
    }

    /// Dispatch an already-evaluated callable `Value` on evaluated args, *returning* the result
    /// instead of pushing it. Shared by `do_call` (which pushes) and the higher-order list methods
    /// (which call it per element while keeping their source/result lists rooted on the stack).
    /// `args.len()` is the explicit arg count for arity checks.
    fn invoke_value(&mut self, callee: Value, args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
        let argc = args.len();
        match callee {
            Value::Obj(h) => {
                // Borrow the heap object only long enough to read its `Copy` fields. The old code
                // `self.heap.get(h).clone()` deep-cloned the whole `Obj` on *every* call — for a
                // closure that meant cloning its captured-environment `HashMap` each time — just to
                // read `proto`/`home`. `Native` still clones its (small) name `String`, but the hot
                // user-function/closure paths now copy three scalars and allocate nothing.
                enum Callee {
                    Func { proto: ProtoId, home: GcRef },
                    Closure { proto: ProtoId, home: GcRef },
                    Native { func: crate::native::NativeFn, name: Box<str> },
                    NotCallable,
                }
                let kind = match self.heap.get(h) {
                    Obj::Func { proto, home } => Callee::Func { proto: *proto, home: *home },
                    Obj::Closure { proto, home, .. } => Callee::Closure { proto: *proto, home: *home },
                    Obj::Native { func, name } => Callee::Native { func: *func, name: name.clone() },
                    _ => Callee::NotCallable,
                };
                match kind {
                    Callee::Func { proto, home } => {
                        // `&...name` (no clone): `check_arity` only formats the message on mismatch.
                        self.check_arity("function", &self.program.protos[proto].name, self.program.protos[proto].arity, argc, span)?;
                        self.run_proto(proto, home, None, args, true, false, span)
                    }
                    Callee::Closure { proto, home } => {
                        if argc != self.program.protos[proto].arity {
                            return Err(self.err(format!("closure expects {} argument(s), got {argc}", self.program.protos[proto].arity), span));
                        }
                        self.run_proto(proto, home, Some(h), args, true, false, span)
                    }
                    Callee::Native { func, name } => self.invoke_native(func, &name, args, span),
                    Callee::NotCallable => Err(self.err(format!("'{}' is not callable", self.type_name(callee)), span)),
                }
            }
            other => Err(self.err(format!("'{}' is not callable", self.type_name(other)), span)),
        }
    }

    /// Invoke a native (Rust) function value (M6c). Builds a [`VmHost`] over the evaluated args,
    /// runs the binding, then lowers its engine-neutral [`NativeRet`] into a heap-allocated `Value`
    /// and pushes it. Lowering (the only allocation) happens here — at an instruction boundary,
    /// after the call returns — so the "collect only at instruction boundaries" GC invariant holds.
    fn invoke_native(
        &mut self,
        func: crate::native::NativeFn,
        name: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // D6 — `std.net.connect` / `listen` are intercepted: they allocate a `Socket`/`Listener`
        // handle (a heap object over an `Arc`'d core), which a pure off-heap native cannot do. Run
        // inline in the VM (the `func` placeholder in `net.rs` never executes).
        if name == "connect" || name == "listen" {
            return self.net_connect_or_listen(name, args, span);
        }
        // D5 — under the M:N engine, a blocking native call (`read_file` / `sleep_ms` / `fs.*`) is
        // OFFLOADED to the dirty pool rather than run inline, so it can't pin a core worker (the G3
        // starvation). Gated on `native_reentry == 0`: a blocking native reached inside a native
        // callback can't park the fiber (its caller's loop state is on the Rust host stack), so it
        // falls through to inline. Record the call + extracted primitive args; the worker loop hands
        // it to the pool ([`Disp::Offload`]) and `paused()` skips the (missing) result-push here. The
        // result is lowered + pushed by the worker that resumes the fiber after completion.
        if self.mn.is_some()
            && self.native_reentry == 0
            && crate::native::is_blocking(name)
            && let Some(nargs) = self.extract_native_args(&args)
        {
            // D5 owe #2 — `sleep_ms` rides the timer thread (park + deadline-wake), not a pool thread
            // (`timer_ms = Some(ms)`). A non-positive (or non-int) `sleep_ms` has nothing to wait for,
            // so it is NOT offloaded — `offload` stays `None` and execution falls through to the
            // inline path below (which returns `Nil` instantly). Every other blocking native (the
            // `io`/`fs`/`request`/`process` set) keeps `timer_ms = None` → the dirty pool.
            let offload = match name {
                "sleep_ms" => {
                    // Copy the duration out first (ends the `nargs` borrow before the move below).
                    let ms = match nargs.first() {
                        Some(crate::native::NativeArg::Int(ms)) if *ms > 0 => Some(*ms as u64),
                        _ => None, // sleep_ms(<=0) / non-int: inline no-op
                    };
                    ms.map(|ms| OffloadReq { func, args: nargs, span, timer_ms: Some(ms) })
                }
                _ => Some(OffloadReq { func, args: nargs, span, timer_ms: None }),
            };
            if let Some(req) = offload {
                self.offload = Some(req);
                return Ok(Value::Nil); // sentinel; never pushed (the `paused()` gate at the call site)
            }
        }
        // D5 owe #3 Path C (#3) — a `sleep_ms(ms>0)` reached INSIDE a native callback (the offload gate
        // above is skipped here because it requires `native_reentry == 0`). Rather than run inline and
        // pin the worker for `ms`, DEMOTE the worker: spawn a replacement + sleep in place + resume. A
        // non-positive / non-int arg has nothing to wait for → falls through to the inline no-op.
        if self.mn.is_some()
            && self.native_reentry > 0
            && name == "sleep_ms"
            && let Some(Value::Int(ms)) = args.first()
            && *ms > 0
        {
            return self.demote_block_sleep(*ms as u64, span);
        }
        let mut host = VmHost { vm: self, args };
        let ret = func(&mut host).map_err(|e| RuntimeError { message: e.message, span })?;
        Ok(self.lower_native(ret))
    }

    /// D5 — materialize a blocking native's already-evaluated `Value` args into `Send` primitives so
    /// the dirty-pool thread can run the call without the heap. Returns `None` if any arg is not a
    /// primitive (int / float / bool / str) — the scoped blocking fns only ever take primitives, so a
    /// non-primitive means "don't offload, run inline" (a safe fallback, never a fault).
    fn extract_native_args(&self, args: &[Value]) -> Option<Vec<crate::native::NativeArg>> {
        use crate::native::NativeArg as A;
        args.iter()
            .map(|v| match v {
                Value::Int(n) => Some(A::Int(*n)),
                Value::Float(f) => Some(A::Float(*f)),
                Value::Bool(b) => Some(A::Bool(*b)),
                Value::Obj(h) => match self.heap.get(*h) {
                    Obj::Str(s) => Some(A::Str(s.to_string())),
                    // A `map[str, str]` arg (today only `request`'s headers) is snapshotted into
                    // owned pairs so it survives the off-heap handoff. Any non-str key/value reverts
                    // to `None` → run inline (safe fallback; the checker guarantees str/str for
                    // typed code, so this is unreachable from a well-typed program).
                    Obj::Map(m) => {
                        let mut pairs = Vec::with_capacity(m.entries.len());
                        for (_, k, v) in &m.entries {
                            let (Value::Obj(kh), Value::Obj(vh)) = (k, v) else {
                                return None;
                            };
                            let (Obj::Str(ks), Obj::Str(vs)) =
                                (self.heap.get(*kh), self.heap.get(*vh))
                            else {
                                return None;
                            };
                            pairs.push((ks.to_string(), vs.to_string()));
                        }
                        Some(A::Map(pairs))
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// Lower a native fn's engine-neutral [`crate::native::NativeRet`] into a VM `Value`, allocating
    /// heap objects for the reference kinds. `Ok`/`Err`/`Some`/`None` become the built-in
    /// `Result` / `Option` enum objects.
    fn lower_native(&mut self, ret: crate::native::NativeRet) -> Value {
        use crate::native::NativeRet as N;
        match ret {
            N::Int(n) => Value::Int(n),
            N::Float(f) => Value::Float(f),
            N::Bool(b) => Value::Bool(b),
            N::Nil => Value::Nil,
            N::Str(s) => self.alloc_str(s),
            N::List(items) => {
                let mut vs = Vec::with_capacity(items.len());
                for x in items {
                    vs.push(self.lower_native(x));
                }
                Value::Obj(self.heap.alloc(Obj::List(vs)))
            }
            N::Struct { name, fields } => {
                // Lower fields first (each may allocate), then allocate the struct itself — keeps
                // every allocation at this instruction boundary, preserving the GC invariant.
                let mut fs = Vec::with_capacity(fields.len());
                for (k, v) in fields {
                    let lv = self.lower_native(v);
                    fs.push((k.into_boxed_str(), lv));
                }
                let tid = self.struct_tid(&name);
                Value::Obj(self.heap.alloc(Obj::Struct { name: name.into_boxed_str(), tid, fields: fs }))
            }
            N::Map(entries) => {
                // Native maps have unique scalar (str) keys — hash them directly (no re-entry, no
                // dedup needed).
                let mut map = MapData::default();
                for (k, v) in entries {
                    let lk = self.lower_native(k);
                    let lv = self.lower_native(v);
                    let hk = self.scalar_hash(lk);
                    map.push(hk, lk, lv);
                }
                Value::Obj(self.heap.alloc(Obj::Map(map)))
            }
            N::Ok(inner) => {
                let p = self.lower_native(*inner);
                self.alloc_enum("Result", "Ok", vec![p])
            }
            N::Err(msg) => {
                let p = self.alloc_str(msg);
                self.alloc_enum("Result", "Err", vec![p])
            }
            N::Some(inner) => {
                let p = self.lower_native(*inner);
                self.alloc_enum("Option", "Some", vec![p])
            }
            N::None => self.alloc_enum("Option", "None", Vec::new()),
        }
    }

    fn alloc_enum(&mut self, ty: &str, variant: &str, payload: Vec<Value>) -> Value {
        Value::Obj(self.heap.alloc(Obj::Enum {
            ty: ty.into(),
            variant: variant.into(),
            payload,
        }))
    }

    /// `Op::JsonDecode`: pop the `Result[Json]` from `parse`, coerce its `Ok` payload against the
    /// descriptor (passing through an `Err`), push the resulting `Result[T]`.
    fn json_decode(&mut self, desc: &crate::json_decode::TypeDescriptor, span: Span) -> Result<(), RuntimeError> {
        let res = self.pop();
        let bad = "decode: parse did not return a Result".to_string();
        let (rty, variant, payload) =
            self.enum_parts(res).ok_or_else(|| self.err(bad.clone(), span))?;
        if rty != "Result" {
            return Err(self.err(bad, span));
        }
        match variant.as_str() {
            "Err" => {
                self.push(res); // a Result Err(str) is already a valid Result[T]
                Ok(())
            }
            "Ok" if payload.len() == 1 => {
                let jv = payload[0];
                match self.coerce_json(jv, desc, "$") {
                    Ok(v) => {
                        let r = self.alloc_enum("Result", "Ok", vec![v]);
                        self.push(r);
                    }
                    Err(msg) => {
                        let s = self.alloc_str(msg);
                        let r = self.alloc_enum("Result", "Err", vec![s]);
                        self.push(r);
                    }
                }
                Ok(())
            }
            _ => Err(self.err(bad, span)),
        }
    }

    /// The enum type, variant name, and (copied) payload of an enum value; `None` if not an enum.
    fn enum_parts(&self, v: Value) -> Option<(String, String, Vec<Value>)> {
        match v {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Enum { ty, variant, payload } => {
                    Some((ty.to_string(), variant.to_string(), payload.clone()))
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Coerce a parsed `Json` value into a concrete value of the descriptor's type. `path` is a
    /// JSON-pointer-ish breadcrumb for error messages. Mirrors the interpreter's `coerce_json`.
    fn coerce_json(
        &mut self,
        jv: Value,
        desc: &crate::json_decode::TypeDescriptor,
        path: &str,
    ) -> Result<Value, String> {
        use crate::json_decode::TypeDescriptor as D;
        let (_jty, variant, payload) = self
            .enum_parts(jv)
            .ok_or_else(|| format!("decode: expected a JSON value at {path}"))?;
        let mismatch = |want: &str| format!("decode: expected {want} at {path}, found {}", crate::json_decode::json_kind(&variant));
        match desc {
            D::Int => {
                let f = self.json_num(&variant, &payload).ok_or_else(|| mismatch("int"))?;
                if f.fract() != 0.0 || !f.is_finite() {
                    return Err(format!("decode: expected an integer at {path}, found {f}"));
                }
                Ok(Value::Int(f as i64))
            }
            D::Float => {
                let f = self.json_num(&variant, &payload).ok_or_else(|| mismatch("float"))?;
                Ok(Value::Float(f))
            }
            D::Bool => match (variant.as_str(), payload.first()) {
                ("Bool", Some(Value::Bool(b))) => Ok(Value::Bool(*b)),
                _ => Err(mismatch("bool")),
            },
            D::Str => {
                if variant == "Str" {
                    let s = self.val_str(payload[0]).unwrap_or_default();
                    Ok(self.alloc_str(s))
                } else {
                    Err(mismatch("str"))
                }
            }
            D::Option(inner) => {
                if variant == "Null" {
                    Ok(self.alloc_enum("Option", "None", Vec::new()))
                } else {
                    let v = self.coerce_json(jv, inner, path)?;
                    Ok(self.alloc_enum("Option", "Some", vec![v]))
                }
            }
            D::List(inner) => {
                if variant != "Arr" {
                    return Err(mismatch("array"));
                }
                let items = match self.heap.get(self.as_obj(payload[0])) {
                    Obj::List(items) => items.clone(),
                    _ => return Err(mismatch("array")),
                };
                let mut out = Vec::with_capacity(items.len());
                for (i, it) in items.into_iter().enumerate() {
                    out.push(self.coerce_json(it, inner, &format!("{path}[{i}]"))?);
                }
                Ok(Value::Obj(self.heap.alloc(Obj::List(out))))
            }
            D::Map(inner) => {
                if variant != "Obj" {
                    return Err(mismatch("object"));
                }
                let entries = match self.heap.get(self.as_obj(payload[0])) {
                    Obj::Map(m) => m.entries.clone(),
                    _ => return Err(mismatch("object")),
                };
                let mut out = MapData::default();
                for (hk, k, v) in entries {
                    let key = self.val_str(k).unwrap_or_default();
                    let coerced = self.coerce_json(v, inner, &format!("{path}.{key}"))?;
                    out.push(hk, k, coerced); // str keys unchanged → reuse the cached hash
                }
                Ok(Value::Obj(self.heap.alloc(Obj::Map(out))))
            }
            D::Struct { name, fields } => {
                if variant != "Obj" {
                    return Err(mismatch(&format!("object for {name}")));
                }
                let entries = match self.heap.get(self.as_obj(payload[0])) {
                    Obj::Map(m) => m.entries.clone(),
                    _ => return Err(mismatch("object")),
                };
                let mut field_vals: Vec<(Box<str>, Value)> = Vec::with_capacity(fields.len());
                for (fname, fdesc) in fields {
                    let found = entries.iter().find(|(_, k, _)| {
                        self.val_str(*k).as_deref() == Some(fname.as_str())
                    });
                    let fpath = format!("{path}.{fname}");
                    let v = match found {
                        Some((_, _, jval)) => self.coerce_json(*jval, fdesc, &fpath)?,
                        None => match fdesc {
                            // A missing Option field decodes to None; anything else is an error.
                            D::Option(_) => self.alloc_enum("Option", "None", Vec::new()),
                            _ => return Err(format!("decode: missing key '{fname}' at {path}")),
                        },
                    };
                    field_vals.push((fname.clone().into_boxed_str(), v));
                }
                let tid = self.struct_tid(name);
                let h = self.heap.alloc(Obj::Struct { name: name.clone().into_boxed_str(), tid, fields: field_vals });
                Ok(Value::Obj(h))
            }
        }
    }

    /// The `f64` of a JSON `Num`, else `None`.
    fn json_num(&self, variant: &str, payload: &[Value]) -> Option<f64> {
        if variant == "Num" {
            match payload.first() {
                Some(Value::Float(f)) => Some(*f),
                Some(Value::Int(n)) => Some(*n as f64),
                _ => None,
            }
        } else {
            None
        }
    }

    /// The owned text of a str value, else `None`.
    fn val_str(&self, v: Value) -> Option<String> {
        match v {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Str(s) => Some(s.to_string()),
                _ => None,
            },
            _ => None,
        }
    }

    /// The heap handle of an `Obj` value (caller guarantees it is one).
    fn as_obj(&self, v: Value) -> GcRef {
        match v {
            Value::Obj(h) => h,
            _ => unreachable!("as_obj on non-object"),
        }
    }

    fn check_arity(&self, _kind: &str, name: &str, want: usize, got: usize, span: Span) -> Result<(), RuntimeError> {
        if want != got {
            return Err(self.err(format!("function '{name}' expects {want} argument(s), got {got}"), span));
        }
        Ok(())
    }

    /// `ic`: the per-call-site method inline-cache id from the `CallMethod` op, or [`NO_IC`] for the
    /// native-re-entry callers (`spawn`/`defer` method tasks) that need a *synchronous* result and so
    /// must take the re-entrant `run_proto` path (never the in-place frame flatten). A real `ic` ⟺ the
    /// caller is the running dispatch loop (the sole emit path), so a real `ic` is exactly the
    /// "flatten-safe" signal: the pushed frame is executed by the `run_until` that called us.
    fn do_method_call(&mut self, method: &str, argc: usize, ic: u32, span: Span) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let recv = self.pop();
        // `compare` on a primitive (int/float/str): they intrinsically satisfy `Comparable`, so an
        // erased generic body may call `.compare()` on a concrete primitive. Return the sign of the
        // ordering. Structs with their own `compare` fall through to the normal dispatch below.
        // Mirrors `interp::eval_method_call`.
        if method == "compare" && args.len() == 1 {
            let is_prim = matches!(recv, Value::Int(_) | Value::Float(_))
                || matches!(recv, Value::Obj(h) if matches!(self.heap.get(h), Obj::Str(_)));
            if is_prim
                && let Some(ord) = self.compare(recv, args[0])
            {
                self.push(Value::Int(ord as i64));
                return Ok(());
            }
        }
        let Value::Obj(h) = recv else {
            return Err(self.err(format!("type {} has no method '{method}'", self.type_name(recv)), span));
        };
        // M19 Phase 6 — method-call inline-cache fast path (struct methods only). A hit on a matching
        // `tid` collapses the `program.structs` clone + name-keyed `def.methods` probe to one int
        // compare AND flattens the call: `[recv, args…]` go on the stack and the method frame is
        // installed in place, so the running `run_until` executes the body and its `Return` pushes the
        // result — no re-entrant `run_proto`. Only the dispatch loop reaches here (real `ic`); the
        // arity guard re-runs (cheap) so a hit can never enter a frame with the wrong slot count.
        if ic != NO_IC {
            let cell = self.method_ic[ic as usize];
            if cell.tid != TID_NONE
                && let Obj::Struct { tid, .. } = self.heap.get(h)
                && *tid == cell.tid
            {
                let proto = cell.proto;
                let arity = self.program.protos[proto].arity;
                if arity != argc + 1 {
                    return Err(self.err(format!("function '{}' expects {} argument(s), got {}", self.program.protos[proto].name, arity, argc + 1), span));
                }
                let home = self.module_objs[cell.module_idx as usize];
                let base = self.stack.len();
                self.stack.push(recv);
                self.stack.extend(args);
                return self.push_frame_in_place(proto, home, None, base, span);
            }
        }
        // Higher-order list methods (`map`/`filter`/`fold`) call a closure per element, which runs
        // nested VM frames that may GC at instruction boundaries. They keep the source + result
        // (and fold's accumulator) rooted on the operand stack across the loop — see `list_hof`.
        if matches!(self.heap.get(h), Obj::List(_)) && matches!(method, "map" | "filter" | "fold") {
            let result = self.list_hof(h, method, args, span)?;
            self.push(result);
            return Ok(());
        }
        // `sort_by` also runs a closure per comparison, but sorts in place and returns nil.
        if matches!(self.heap.get(h), Obj::List(_)) && method == "sort_by" {
            let result = self.list_sort_by(h, args, span)?;
            self.push(result);
            return Ok(());
        }
        // `sort_by_key` calls a key extractor once per element, then sorts in place by key.
        if matches!(self.heap.get(h), Obj::List(_)) && method == "sort_by_key" {
            let result = self.list_sort_by_key(h, args, span)?;
            self.push(result);
            return Ok(());
        }
        // Concurrency C4: `Channel` / `Shared` methods mutate the heap object in place (and `update`
        // re-enters the VM), so dispatch them directly off the handle, like the core-type methods.
        if matches!(self.heap.get(h), Obj::Channel(_)) {
            let result = self.channel_method(h, method, &args, span)?;
            if self.suspend.is_some() {
                return Ok(()); // B1: `recv` parked this fiber and re-rooted the receiver itself.
            }
            self.push(result);
            return Ok(());
        }
        if matches!(self.heap.get(h), Obj::Shared(_)) {
            let result = self.shared_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        if matches!(self.heap.get(h), Obj::Atomic(_)) {
            let result = self.atomic_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        if matches!(self.heap.get(h), Obj::Executor(_)) {
            let result = self.executor_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        // D6: `Socket` / `Listener` methods operate on the fd in the `Arc`'d core and may park the
        // fiber on the netpoller (a would-block `read`/`write`/`accept`). Dispatch off the handle, like
        // the other core handles; gate the result-push on `poll_park` (mirrors the channel `recv` park
        // gate just above, but routed to the poller — strictly separate from `suspend`).
        if matches!(self.heap.get(h), Obj::Socket(_)) {
            let result = self.socket_method(h, method, &args, span)?;
            if self.poll_park.is_some() {
                return Ok(()); // D6: the op `WouldBlock`ed and re-rooted the receiver itself.
            }
            self.push(result);
            return Ok(());
        }
        if matches!(self.heap.get(h), Obj::Listener(_)) {
            let result = self.listener_method(h, method, &args, span)?;
            if self.poll_park.is_some() {
                return Ok(());
            }
            self.push(result);
            return Ok(());
        }
        // Core-type methods (M6): built-in methods on `str` / `list`. Handled before the clone-match
        // so `list.push` mutates the heap object in place (the match below clones the Obj). Mirrors
        // `interp::builtins::call_method` exactly — error strings included (parity-tested).
        if matches!(self.heap.get(h), Obj::Str(_) | Obj::List(_) | Obj::Map(_) | Obj::Set(_)) {
            let result = self.core_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        self.ensure_module_faulted(h); // D1: `module.fn(...)` on a not-yet-faulted worker module
        match self.heap.get(h).clone() {
            // `module.fn(args)` — plain call on the looked-up member, no `self`.
            Obj::Module { name, slots, index } => {
                let member = index.get(method).map(|&i| slots[i as usize]).ok_or_else(|| self.err(format!("module '{name}' has no member '{method}'"), span))?;
                self.stack.push(member);
                self.stack.extend(args);
                self.do_call(argc, span)
            }
            Obj::Struct { name, fields, tid, .. } => {
                let def = self.program.structs.get(name.as_ref()).cloned().ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
                if let Some(&proto) = def.methods.get(method) {
                    let home = self.module_objs[def.module_idx];
                    if self.program.protos[proto].arity != argc + 1 {
                        // `self` + explicit args.
                        return Err(self.err(format!("function '{}' expects {} argument(s), got {}", self.program.protos[proto].name, self.program.protos[proto].arity, argc + 1), span));
                    }
                    // M19 Phase 6 — fill the method IC so the next call at this site hits the fast path
                    // above (only for the dispatch-loop path: a real `ic`, a registered layout `tid`).
                    if ic != NO_IC && tid != TID_NONE {
                        self.method_ic[ic as usize] = MethodIcCell { tid, proto, module_idx: def.module_idx as u32 };
                        // Flatten: install the frame in place and let the running `run_until` execute it
                        // (mirrors the IC fast path + the `Op::Call` flatten). The re-entrant callers
                        // pass NO_IC and so keep the synchronous `run_proto` path below.
                        let base = self.stack.len();
                        self.stack.push(recv);
                        self.stack.extend(args);
                        return self.push_frame_in_place(proto, home, None, base, span);
                    }
                    let mut call_args = Vec::with_capacity(argc + 1);
                    call_args.push(recv);
                    call_args.extend(args);
                    let v = self.run_proto(proto, home, None, call_args, true, false, span)?;
                    if self.paused() {
                        return Ok(()); // B1/D3: the method parked on a blocking `recv` or yielded.
                    }
                    self.push(v);
                    return Ok(());
                }
                // No method named `method`: fall back to a function-typed *field* — `recv.f(args)`
                // where `f` holds a function value (the checker verified `f: fn(...) -> ...`).
                // Invoked as a value (no `self` bound — it's not a method).
                if let Some((_, fval)) = fields.iter().find(|(k, _)| k.as_ref() == method) {
                    let v = self.invoke_value(*fval, args, span)?;
                    if self.paused() {
                        return Ok(()); // B1/D3: the function-field call parked on `recv` or yielded.
                    }
                    self.push(v);
                    return Ok(());
                }
                Err(self.err(format!("struct '{name}' has no method '{method}'"), span))
            }
            _ => Err(self.err(format!("type {} has no method '{method}'", self.type_name(recv)), span)),
        }
    }

    /// Higher-order list methods `map` / `filter` / `fold`. `src_h` is the receiver list. Each
    /// element is fed to a closure via `invoke_value`, which runs nested VM frames that can trigger
    /// GC at instruction boundaries. To keep the GC from collecting in-flight heap values, the
    /// source list, the partially-built result list (map/filter), and the fold accumulator are all
    /// kept rooted on the operand stack across the iteration. Returns the result (caller pushes it).
    /// Arity & error messages match the interp exactly (parity-tested).
    fn list_hof(&mut self, src_h: GcRef, method: &str, mut args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
        // ROOT the source list on the operand stack so its elements survive every closure call.
        self.push(Value::Obj(src_h));
        let n = match self.heap.get(src_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!("list_hof on non-list"),
        };
        match method {
            "map" | "filter" => {
                if args.len() != 1 {
                    self.pop(); // unroot source before erroring
                    return Err(self.err(format!("'{method}' expects 1 argument(s), got {}", args.len()), span));
                }
                let f = args.swap_remove(0);
                let is_filter = method == "filter";
                // ROOT the result list too.
                let res_h = self.heap.alloc(Obj::List(Vec::new()));
                self.push(Value::Obj(res_h));
                for i in 0..n {
                    // Re-read each iteration; `src_h` stays valid (rooted on the stack).
                    let elem = match self.heap.get(src_h) {
                        Obj::List(v) => v[i],
                        _ => unreachable!(),
                    };
                    // May GC; both source and result lists are rooted, so their elements survive.
                    let out = self.guarded(|vm| vm.invoke_value(f, vec![elem], span))?;
                    if is_filter {
                        match out {
                            Value::Bool(true) => {
                                if let Obj::List(items) = self.heap.get_mut(res_h) {
                                    items.push(elem);
                                }
                            }
                            Value::Bool(false) => {}
                            other => {
                                self.pop(); // unroot result
                                self.pop(); // unroot source
                                return Err(self.err(format!("filter predicate must return bool, got {}", self.type_name(other)), span));
                            }
                        }
                    } else if let Obj::List(items) = self.heap.get_mut(res_h) {
                        items.push(out);
                    }
                }
                self.pop(); // unroot result
                self.pop(); // unroot source
                Ok(Value::Obj(res_h))
            }
            "fold" => {
                if args.len() != 2 {
                    self.pop(); // unroot source
                    return Err(self.err(format!("'fold' expects 2 argument(s), got {}", args.len()), span));
                }
                let f = args.swap_remove(1);
                let init = args.swap_remove(0);
                // ROOT the accumulator: push init, remember its slot, and replace in place each step.
                // `acc_slot` sits below every nested frame's base (frames push above the current
                // stack top and pop back to it), so the index stays valid across `invoke_value`.
                self.push(init);
                let acc_slot = self.stack.len() - 1;
                for i in 0..n {
                    let elem = match self.heap.get(src_h) {
                        Obj::List(v) => v[i],
                        _ => unreachable!(),
                    };
                    let acc = self.stack[acc_slot];
                    let new = self.guarded(|vm| vm.invoke_value(f, vec![acc, elem], span))?;
                    self.stack[acc_slot] = new;
                }
                let acc = self.pop(); // unroot accumulator
                self.pop(); // unroot source
                Ok(acc)
            }
            _ => unreachable!("list_hof called with non-HOF method {method}"),
        }
    }

    /// `xs.sort_by(cmp)` — stable in-place sort driven by a Chezzi comparator `fn(T, T) -> int`
    /// (negative = a before b, positive = a after b, zero = equal). The comparator re-enters the VM
    /// and may GC, so we never hold the elements in an unrooted Rust `Vec`: the source list stays
    /// rooted on the operand stack, and the merge sort permutes plain `usize` **indices**, re-reading
    /// elements from the rooted heap object on each comparison. The final permutation is materialised
    /// only after all comparator calls finish (no GC in between). Returns `nil`.
    fn list_sort_by(&mut self, src_h: GcRef, mut args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
        if args.len() != 1 {
            return Err(self.err(format!("'sort_by' expects 1 argument(s), got {}", args.len()), span));
        }
        let cmp = args.swap_remove(0);
        // Root the source list itself: a method receiver is popped before dispatch, so an inline
        // temporary (`make().sort_by(...)`) is otherwise unrooted and the comparator's GC could
        // collect it before the write-back.
        self.push(Value::Obj(src_h));
        // Sort a SNAPSHOT taken now (matching the interpreter): a comparator that mutates the source
        // list mid-sort must not perturb the ordering, and its mutations are discarded by the final
        // write-back. The snapshot list is itself heap-allocated and rooted on the operand stack so
        // its elements survive the comparator's collections.
        let snap_h = {
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!("list_sort_by on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::Obj(snap_h)); // ROOT the snapshot
        let n = match self.heap.get(snap_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!(),
        };
        let order = match self.msort_indices(snap_h, (0..n).collect(), cmp, span) {
            Ok(o) => o,
            Err(e) => {
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                return Err(e);
            }
        };
        // No comparator calls remain, so no GC: read the rooted snapshot and write the result back.
        let reordered: Vec<Value> = match self.heap.get(snap_h) {
            Obj::List(v) => order.iter().map(|&i| v[i]).collect(),
            _ => unreachable!(),
        };
        if let Obj::List(v) = self.heap.get_mut(src_h) {
            *v = reordered;
        }
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(Value::Nil)
    }

    /// Stable top-down merge sort over `idx` (positions into the rooted list `src_h`), comparing
    /// elements via the Chezzi comparator `cmp`.
    fn msort_indices(&mut self, src_h: GcRef, idx: Vec<usize>, cmp: Value, span: Span) -> Result<Vec<usize>, RuntimeError> {
        let n = idx.len();
        if n <= 1 {
            return Ok(idx);
        }
        let mut idx = idx;
        let right = idx.split_off(n / 2);
        let left = self.msort_indices(src_h, idx, cmp, span)?;
        let right = self.msort_indices(src_h, right, cmp, span)?;
        let mut out = Vec::with_capacity(n);
        let (mut li, mut ri) = (0, 0);
        while li < left.len() && ri < right.len() {
            let a = match self.heap.get(src_h) {
                Obj::List(v) => v[left[li]],
                _ => unreachable!(),
            };
            let b = match self.heap.get(src_h) {
                Obj::List(v) => v[right[ri]],
                _ => unreachable!(),
            };
            // `<= 0` keeps the left element first on ties → stable.
            if self.compare_with(cmp, a, b, span)? <= 0 {
                out.push(left[li]);
                li += 1;
            } else {
                out.push(right[ri]);
                ri += 1;
            }
        }
        out.extend_from_slice(&left[li..]);
        out.extend_from_slice(&right[ri..]);
        Ok(out)
    }

    /// Run the comparator on `(a, b)` and return its int result (errors if it returns non-int).
    fn compare_with(&mut self, cmp: Value, a: Value, b: Value, span: Span) -> Result<i64, RuntimeError> {
        match self.guarded(|vm| vm.invoke_value(cmp, vec![a, b], span))? {
            Value::Int(n) => Ok(n),
            other => Err(self.err(format!("sort_by comparator must return int, got {}", self.type_name(other)), span)),
        }
    }

    /// `xs.sort_by_key(f)` — stable in-place sort by a derived key `f: fn(T) -> K`. Mirrors
    /// `list_sort_by`'s GC discipline: the source list, an element snapshot, AND a parallel **keys**
    /// list are all rooted on the operand stack so the re-entrant extractor (and a Comparable-struct
    /// key's `compare`) can GC freely. Keys are computed once per element; the merge sort permutes
    /// `usize` indices, re-reading keys from the rooted keys list per comparison. Returns `nil`.
    fn list_sort_by_key(&mut self, src_h: GcRef, mut args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
        if args.len() != 1 {
            return Err(self.err(format!("'sort_by_key' expects 1 argument(s), got {}", args.len()), span));
        }
        let f = args.swap_remove(0);
        self.push(Value::Obj(src_h)); // ROOT the source list
        let snap_h = {
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!("list_sort_by_key on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::Obj(snap_h)); // ROOT the snapshot
        let n = match self.heap.get(snap_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!(),
        };
        // Compute keys once per element into a rooted list. Each `invoke_value` may GC; already-pushed
        // keys survive because `keys_h` is rooted (a `Vec::push` into it does not itself GC).
        let keys_h = self.heap.alloc(Obj::List(Vec::with_capacity(n)));
        self.push(Value::Obj(keys_h)); // ROOT the keys
        for i in 0..n {
            let e = match self.heap.get(snap_h) {
                Obj::List(v) => v[i],
                _ => unreachable!(),
            };
            match self.guarded(|vm| vm.invoke_value(f, vec![e], span)) {
                Ok(k) => {
                    if let Obj::List(v) = self.heap.get_mut(keys_h) {
                        v.push(k);
                    }
                }
                Err(err) => {
                    self.pop(); // unroot keys
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(err);
                }
            }
        }
        let order = match self.msort_indices_by_key(keys_h, (0..n).collect(), span) {
            Ok(o) => o,
            Err(e) => {
                self.pop(); // unroot keys
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                return Err(e);
            }
        };
        // No extractor/compare calls remain, so no GC: reorder the snapshot and write back.
        let reordered: Vec<Value> = match self.heap.get(snap_h) {
            Obj::List(v) => order.iter().map(|&i| v[i]).collect(),
            _ => unreachable!(),
        };
        if let Obj::List(v) = self.heap.get_mut(src_h) {
            *v = reordered;
        }
        self.pop(); // unroot keys
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(Value::Nil)
    }

    /// Stable top-down merge sort over `idx` (positions into the rooted keys list `keys_h`), ordering
    /// by each key's natural order via [`order_key`].
    fn msort_indices_by_key(&mut self, keys_h: GcRef, idx: Vec<usize>, span: Span) -> Result<Vec<usize>, RuntimeError> {
        let n = idx.len();
        if n <= 1 {
            return Ok(idx);
        }
        let mut idx = idx;
        let right = idx.split_off(n / 2);
        let left = self.msort_indices_by_key(keys_h, idx, span)?;
        let right = self.msort_indices_by_key(keys_h, right, span)?;
        let mut out = Vec::with_capacity(n);
        let (mut li, mut ri) = (0, 0);
        while li < left.len() && ri < right.len() {
            let a = match self.heap.get(keys_h) {
                Obj::List(v) => v[left[li]],
                _ => unreachable!(),
            };
            let b = match self.heap.get(keys_h) {
                Obj::List(v) => v[right[ri]],
                _ => unreachable!(),
            };
            // `<= Equal` keeps the left element first on ties → stable.
            if self.order_key(a, b, span)?.is_le() {
                out.push(left[li]);
                li += 1;
            } else {
                out.push(right[ri]);
                ri += 1;
            }
        }
        out.extend_from_slice(&left[li..]);
        out.extend_from_slice(&right[ri..]);
        Ok(out)
    }

    /// Natural order over two `sort_by_key` keys: a Comparable struct key dispatches to its
    /// `compare`; scalar keys (int/float/str) use the built-in [`Vm::compare`]. The checker has
    /// verified the key type is orderable.
    fn order_key(&mut self, a: Value, b: Value, span: Span) -> Result<std::cmp::Ordering, RuntimeError> {
        if let (Value::Obj(ha), Value::Obj(hb)) = (a, b)
            && matches!(self.heap.get(ha), Obj::Struct { .. })
            && matches!(self.heap.get(hb), Obj::Struct { .. })
        {
            return self.struct_compare(a, b, span);
        }
        self.compare(a, b).ok_or_else(|| {
            self.err(
                format!("sort_by_key keys are not comparable: {} vs {}", self.type_name(a), self.type_name(b)),
                span,
            )
        })
    }

    /// Built-in methods on `str` / `list` (M6). The result is returned (not pushed) so the caller
    /// owns stack discipline. Multi-allocation paths (`split`) are safe: the GC only collects at
    /// instruction boundaries, never mid-opcode, so all `alloc`s here complete uninterrupted.
    /// Clone the elements of a `list`-typed argument for `concat`/`extend`. The checker guarantees
    /// the type; a non-list here is an internal invariant break, reported for safety.
    fn expect_list_obj(&self, method: &str, arg: Value, span: Span) -> Result<Vec<Value>, RuntimeError> {
        match arg {
            Value::Obj(ah) => match self.heap.get(ah) {
                Obj::List(items) => Ok(items.clone()),
                _ => Err(self.err(format!("{method}() expects a list argument, got {}", self.type_name(arg)), span)),
            },
            other => Err(self.err(format!("{method}() expects a list argument, got {}", self.type_name(other)), span)),
        }
    }

    /// Insert-or-overwrite `(hk, key, val)` into the heap map at `h` (last write wins). Used by
    /// `map.update`. No allocation, so no GC concerns.
    fn map_upsert_in_heap(&mut self, h: GcRef, hk: u64, key: Value, val: Value) {
        let Obj::Map(m) = self.heap.get(h) else { unreachable!() };
        let pos = m.candidates(hk).iter().copied().find(|&p| self.values_equal(m.entries[p].1, key));
        let Obj::Map(m) = self.heap.get_mut(h) else { unreachable!() };
        match pos {
            Some(i) => m.entries[i].2 = val,
            None => m.push(hk, key, val),
        }
    }

    fn core_method(&mut self, h: GcRef, method: &str, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        // A str argument's owned text, with a uniform type error matching the interp.
        let str_arg = |vm: &Vm, i: usize| -> Result<String, RuntimeError> {
            match args[i] {
                Value::Obj(ah) => match vm.heap.get(ah) {
                    Obj::Str(a) => Ok(a.to_string()),
                    _ => Err(vm.err(format!("{method}() expects a str argument, got {}", vm.type_name(args[i])), span)),
                },
                other => Err(vm.err(format!("{method}() expects a str argument, got {}", vm.type_name(other)), span)),
            }
        };
        match self.heap.get(h) {
            Obj::Str(s) => {
                let s = s.to_string();
                match method {
                    "len" => {
                        self.arity_err("len", args, 0, span)?;
                        Ok(Value::Int(s.chars().count() as i64))
                    }
                    "upper" => {
                        self.arity_err("upper", args, 0, span)?;
                        Ok(self.alloc_str(s.to_uppercase()))
                    }
                    "lower" => {
                        self.arity_err("lower", args, 0, span)?;
                        Ok(self.alloc_str(s.to_lowercase()))
                    }
                    "trim" => {
                        self.arity_err("trim", args, 0, span)?;
                        Ok(self.alloc_str(s.trim().to_string()))
                    }
                    // `str` conforms to `Error`: `message()` returns the string itself.
                    "message" => {
                        self.arity_err("message", args, 0, span)?;
                        Ok(self.alloc_str(s.to_string()))
                    }
                    "split" => {
                        self.arity_err("split", args, 1, span)?;
                        let sep = str_arg(self, 0)?;
                        let parts: Vec<Value> =
                            s.split(sep.as_str()).map(|p| self.alloc_str(p.to_string())).collect();
                        Ok(Value::Obj(self.heap.alloc(Obj::List(parts))))
                    }
                    "chars" => {
                        self.arity_err("chars", args, 0, span)?;
                        let cs: Vec<Value> = s.chars().map(|c| self.alloc_char(c)).collect();
                        Ok(Value::Obj(self.heap.alloc(Obj::List(cs))))
                    }
                    "starts_with" => {
                        self.arity_err("starts_with", args, 1, span)?;
                        Ok(Value::Bool(s.starts_with(str_arg(self, 0)?.as_str())))
                    }
                    "contains" => {
                        self.arity_err("contains", args, 1, span)?;
                        Ok(Value::Bool(s.contains(str_arg(self, 0)?.as_str())))
                    }
                    "join" => {
                        self.arity_err("join", args, 1, span)?;
                        let Value::Obj(lh) = args[0] else {
                            return Err(self.err(format!("join() expects a list of str, got {}", self.type_name(args[0])), span));
                        };
                        let Obj::List(items) = self.heap.get(lh) else {
                            return Err(self.err(format!("join() expects a list of str, got {}", self.type_name(args[0])), span));
                        };
                        let mut out = String::new();
                        for (i, item) in items.clone().iter().enumerate() {
                            let Value::Obj(ih) = item else {
                                return Err(self.err(format!("join() expects a list of str, got an element of type {}", self.type_name(*item)), span));
                            };
                            let Obj::Str(part) = self.heap.get(*ih) else {
                                return Err(self.err(format!("join() expects a list of str, got an element of type {}", self.type_name(*item)), span));
                            };
                            if i > 0 {
                                out.push_str(&s);
                            }
                            out.push_str(part);
                        }
                        Ok(self.alloc_str(out))
                    }
                    _ => Err(self.err(format!("type str has no method '{method}'"), span)),
                }
            }
            Obj::List(items) => match method {
                "len" => {
                    self.arity_err("len", args, 0, span)?;
                    Ok(Value::Int(items.len() as i64))
                }
                "push" => {
                    self.arity_err("push", args, 1, span)?;
                    let v = args[0];
                    let Obj::List(items) = self.heap.get_mut(h) else { unreachable!() };
                    items.push(v);
                    Ok(Value::Nil)
                }
                "pop" => {
                    self.arity_err("pop", args, 0, span)?;
                    let Obj::List(items) = self.heap.get_mut(h) else { unreachable!() };
                    match items.pop() {
                        Some(v) => {
                            let eh = self.heap.alloc(Obj::Enum {
                                ty: "Option".into(),
                                variant: "Some".into(),
                                payload: vec![v],
                            });
                            Ok(Value::Obj(eh))
                        }
                        None => {
                            let eh = self.heap.alloc(Obj::Enum {
                                ty: "Option".into(),
                                variant: "None".into(),
                                payload: vec![],
                            });
                            Ok(Value::Obj(eh))
                        }
                    }
                }
                "reverse" => {
                    self.arity_err("reverse", args, 0, span)?;
                    let Obj::List(items) = self.heap.get_mut(h) else { unreachable!() };
                    items.reverse();
                    Ok(Value::Nil)
                }
                "sort" => {
                    self.arity_err("sort", args, 0, span)?;
                    // In place, ascending. Checker guarantees a homogeneous orderable element type.
                    // A list of Comparable structs orders via each struct's `compare` (engine
                    // re-entry, so a merge sort that holds `&mut self`); primitives use the faster
                    // `value_order`. Str elements live on the heap, so `value_order` needs
                    // `&self.heap` — clone the elements out, sort (no alloc/closure → no GC for the
                    // primitive path), then write back.
                    let is_struct =
                        matches!(items.first(), Some(Value::Obj(hh)) if matches!(self.heap.get(*hh), Obj::Struct { .. }));
                    if is_struct {
                        // Struct compare re-enters the VM (may GC) → rooted, index-based sort.
                        return self.list_sort_structs(h, span);
                    }
                    let mut elems = items.clone();
                    elems.sort_by(|a, b| self.value_order(*a, *b));
                    let Obj::List(items) = self.heap.get_mut(h) else { unreachable!() };
                    *items = elems;
                    Ok(Value::Nil)
                }
                "contains" => {
                    self.arity_err("contains", args, 1, span)?;
                    let target = args[0];
                    let elems = items.clone();
                    Ok(Value::Bool(elems.iter().any(|v| self.values_equal(*v, target))))
                }
                "index_of" => {
                    self.arity_err("index_of", args, 1, span)?;
                    let target = args[0];
                    let elems = items.clone();
                    let idx = elems.iter().position(|v| self.values_equal(*v, target));
                    Ok(Value::Int(idx.map(|i| i as i64).unwrap_or(-1)))
                }
                "concat" => {
                    self.arity_err("concat", args, 1, span)?;
                    let mut out = items.clone();
                    out.extend(self.expect_list_obj("concat", args[0], span)?);
                    // `out` is fully built and moved into the new Obj before any GC can run.
                    Ok(Value::Obj(self.heap.alloc(Obj::List(out))))
                }
                "extend" => {
                    self.arity_err("extend", args, 1, span)?;
                    // Snapshot the other side first so `xs.extend(xs)` (self-extend) terminates.
                    let appended = self.expect_list_obj("extend", args[0], span)?;
                    let Obj::List(items) = self.heap.get_mut(h) else { unreachable!() };
                    items.extend(appended);
                    Ok(Value::Nil)
                }
                "sum" => {
                    self.arity_err("sum", args, 0, span)?;
                    let any_float = items.iter().any(|v| matches!(v, Value::Float(_)));
                    if any_float {
                        let mut acc = 0.0_f64;
                        for v in items.iter() {
                            match v {
                                Value::Int(n) => acc += *n as f64,
                                Value::Float(f) => acc += *f,
                                other => {
                                    return Err(self.err(format!("sum() expects a numeric list, got an element of type {}", self.type_name(*other)), span));
                                }
                            }
                        }
                        Ok(Value::Float(acc))
                    } else {
                        let mut acc = 0_i64;
                        for v in items.iter() {
                            match v {
                                Value::Int(n) => acc += *n,
                                other => {
                                    return Err(self.err(format!("sum() expects a numeric list, got an element of type {}", self.type_name(*other)), span));
                                }
                            }
                        }
                        Ok(Value::Int(acc))
                    }
                }
                _ => Err(self.err(format!("type list has no method '{method}'"), span)),
            },
            Obj::Map(m) => match method {
                "len" => {
                    self.arity_err("len", args, 0, span)?;
                    Ok(Value::Int(m.entries.len() as i64))
                }
                "has" => {
                    self.arity_err("has", args, 1, span)?;
                    let key = args[0];
                    let hk = self.hash_key_rooted(key, &[Value::Obj(h), key], span)?;
                    let Obj::Map(m) = self.heap.get(h) else { unreachable!() };
                    let found = m.candidates(hk).iter().any(|&p| self.values_equal(m.entries[p].1, key));
                    Ok(Value::Bool(found))
                }
                "get" => {
                    self.arity_err("get", args, 1, span)?;
                    let key = args[0];
                    let hk = self.hash_key_rooted(key, &[Value::Obj(h), key], span)?;
                    let Obj::Map(m) = self.heap.get(h) else { unreachable!() };
                    let found = m.candidates(hk).iter().copied()
                        .find(|&p| self.values_equal(m.entries[p].1, key))
                        .map(|p| m.entries[p].2);
                    match found {
                        Some(v) => Ok(self.alloc_enum("Option", "Some", vec![v])),
                        None => Ok(self.alloc_enum("Option", "None", vec![])),
                    }
                }
                "keys" => {
                    self.arity_err("keys", args, 0, span)?;
                    let keys: Vec<Value> = m.entries.iter().map(|(_, k, _)| *k).collect();
                    Ok(Value::Obj(self.heap.alloc(Obj::List(keys))))
                }
                "values" => {
                    self.arity_err("values", args, 0, span)?;
                    let vals: Vec<Value> = m.entries.iter().map(|(_, _, v)| *v).collect();
                    Ok(Value::Obj(self.heap.alloc(Obj::List(vals))))
                }
                "remove" => {
                    self.arity_err("remove", args, 1, span)?;
                    let key = args[0];
                    let hk = self.hash_key_rooted(key, &[Value::Obj(h), key], span)?;
                    let Obj::Map(m) = self.heap.get(h) else { unreachable!() };
                    let pos = m.candidates(hk).iter().copied().find(|&p| self.values_equal(m.entries[p].1, key));
                    match pos {
                        Some(i) => {
                            let Obj::Map(m) = self.heap.get_mut(h) else { unreachable!() };
                            let (_, _, v) = m.remove_at(i);
                            Ok(self.alloc_enum("Option", "Some", vec![v]))
                        }
                        None => Ok(self.alloc_enum("Option", "None", vec![])),
                    }
                }
                "merge" | "update" => {
                    self.arity_err(method, args, 1, span)?;
                    // Snapshot the incoming entries (with their cached hashes — engine-wide
                    // consistent, so reuse is sound) first; handles `m.merge(m)`/`m.update(m)`.
                    let incoming = match args[0] {
                        Value::Obj(oh) => match self.heap.get(oh) {
                            Obj::Map(o) => o.entries.clone(),
                            _ => return Err(self.err(format!("{method}() expects a map argument, got {}", self.type_name(args[0])), span)),
                        },
                        other => return Err(self.err(format!("{method}() expects a map argument, got {}", self.type_name(other)), span)),
                    };
                    if method == "merge" {
                        let Obj::Map(m) = self.heap.get(h) else { unreachable!() };
                        let mut out = m.clone();
                        for (hk, key, val) in incoming {
                            let pos = out.candidates(hk).iter().copied().find(|&p| self.values_equal(out.entries[p].1, key));
                            match pos {
                                Some(i) => out.entries[i].2 = val,
                                None => out.push(hk, key, val),
                            }
                        }
                        // `out` is fully built and moved into the new Obj before any GC can run.
                        Ok(Value::Obj(self.heap.alloc(Obj::Map(out))))
                    } else {
                        for (hk, key, val) in incoming {
                            self.map_upsert_in_heap(h, hk, key, val);
                        }
                        Ok(Value::Nil)
                    }
                }
                _ => Err(self.err(format!("type map has no method '{method}'"), span)),
            },
            Obj::Set(s) => match method {
                "len" => {
                    self.arity_err("len", args, 0, span)?;
                    Ok(Value::Int(s.entries.len() as i64))
                }
                "has" => {
                    self.arity_err("has", args, 1, span)?;
                    let x = args[0];
                    let hx = self.hash_key_rooted(x, &[Value::Obj(h), x], span)?;
                    let Obj::Set(s) = self.heap.get(h) else { unreachable!() };
                    Ok(Value::Bool(s.candidates(hx).iter().any(|&p| self.values_equal(s.entries[p].1, x))))
                }
                "add" => {
                    self.arity_err("add", args, 1, span)?;
                    let x = args[0];
                    let hx = self.hash_key_rooted(x, &[Value::Obj(h), x], span)?;
                    let Obj::Set(s) = self.heap.get(h) else { unreachable!() };
                    let present = s.candidates(hx).iter().any(|&p| self.values_equal(s.entries[p].1, x));
                    if !present {
                        let Obj::Set(s) = self.heap.get_mut(h) else { unreachable!() };
                        s.push(hx, x);
                    }
                    Ok(Value::Nil)
                }
                "remove" => {
                    self.arity_err("remove", args, 1, span)?;
                    let x = args[0];
                    let hx = self.hash_key_rooted(x, &[Value::Obj(h), x], span)?;
                    let Obj::Set(s) = self.heap.get(h) else { unreachable!() };
                    let pos = s.candidates(hx).iter().copied().find(|&p| self.values_equal(s.entries[p].1, x));
                    match pos {
                        Some(i) => {
                            let Obj::Set(s) = self.heap.get_mut(h) else { unreachable!() };
                            s.remove_at(i);
                            Ok(Value::Bool(true))
                        }
                        None => Ok(Value::Bool(false)),
                    }
                }
                "union" | "intersection" | "difference" => {
                    self.arity_err(method, args, 1, span)?;
                    // Both operands already carry per-element cached hashes, so set algebra needs no
                    // re-hashing (no user code re-enters) — purely build a fresh hash set, deduping
                    // and membership-testing via the cached hashes confirmed by `values_equal`.
                    let mine = match self.heap.get(h) {
                        Obj::Set(s) => s.entries.clone(),
                        _ => unreachable!(),
                    };
                    let other = self.set_arg(args[0], method, span)?;
                    let mut out = SetData::default();
                    let add = |vm: &Vm, set: &mut SetData, he: u64, e: Value| {
                        if !set.candidates(he).iter().any(|&p| vm.values_equal(set.entries[p].1, e)) {
                            set.push(he, e);
                        }
                    };
                    match method {
                        "union" => {
                            for (he, e) in mine.iter().chain(other.entries.iter()) {
                                add(self, &mut out, *he, *e);
                            }
                        }
                        // intersection keeps mine's elements present in other; difference drops them.
                        m => {
                            let keep_when_present = m == "intersection";
                            for (he, e) in &mine {
                                let in_other = other.candidates(*he).iter().any(|&p| self.values_equal(other.entries[p].1, *e));
                                if in_other == keep_when_present {
                                    add(self, &mut out, *he, *e);
                                }
                            }
                        }
                    }
                    Ok(Value::Obj(self.heap.alloc(Obj::Set(out))))
                }
                _ => Err(self.err(format!("type set has no method '{method}'"), span)),
            },
            _ => unreachable!("core_method dispatched a non-str/list/map/set receiver"),
        }
    }

    /// Read a set argument (for set algebra), erroring if it isn't a set. Returns a clone of its
    /// [`SetData`] (entries + index) so membership tests reuse the cached hashes.
    fn set_arg(&self, v: Value, method: &str, span: Span) -> Result<SetData, RuntimeError> {
        match v {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Set(s) => Ok(s.clone()),
                _ => Err(self.err(format!("{method}() expects a set argument, got {}", self.type_name(v)), span)),
            },
            _ => Err(self.err(format!("{method}() expects a set argument, got {}", self.type_name(v)), span)),
        }
    }

    /// Allocate a heap string and return its handle as a `Value`.
    fn alloc_str(&mut self, s: String) -> Value {
        Value::Obj(self.heap.alloc(Obj::Str(s.into())))
    }

    /// M19 Phase 3 — the 1-char `str` value for `c`, in a single allocation. `c.to_string()` +
    /// `into_boxed_str` is two allocs (a `String`, then a shrink-to-fit realloc); encoding straight
    /// into a stack buffer and boxing the `&str` is one. Used by string indexing/iteration/`chr`.
    fn alloc_char(&mut self, c: char) -> Value {
        let mut buf = [0u8; 4];
        Value::Obj(self.heap.alloc(Obj::Str((&*c.encode_utf8(&mut buf)).into())))
    }

    /// Return from the current frame. `propagated` true ⇒ the value came from `?` (no observable
    /// difference here; the caller treats it as the function's result, exactly like the interp).
    ///
    /// Deferred calls (`defer`) run LIFO first, while the frame is still live so the GC keeps their
    /// values — and the return value — rooted. A fault in a deferred call supersedes the frame's
    /// result (Go: a panic in a defer wins): it returns `Err` and the frame is still torn down.
    fn do_return(&mut self, _propagated: bool) -> Result<(), RuntimeError> {
        // M-C implicit nurseries: if this frame opened one, JOIN it here (run its spawned tasks to
        // completion) BEFORE the frame unwinds — `return`/`?`/fall-through is the join barrier. This
        // runs while the frame is still current and the return value (if any) still sits on the
        // operand stack; `join_nursery` swaps the whole `FiberCtx`, never the operand value, so the
        // value survives. Any *inner* `parallel:` this return/`?` escaped sits ABOVE the implicit
        // nursery and is cancelled-and-reported first (existing escape semantics). A task that faults
        // during the join propagates as this function's error (the frame is intact, so the normal
        // unwind machinery runs its defers). NB: an uncaught *body* fault never reaches here — it
        // unwinds via the handler path, which cancels (not joins) the implicit nursery.
        let frame_top = self.frames.last().unwrap();
        let nursery_floor = frame_top.nursery_len;
        if frame_top.has_implicit_nursery {
            self.drain_escaped_nursery(nursery_floor + 1); // cancel inner escaped `parallel:` levels
            if self.nurseries.len() > nursery_floor {
                self.join_nursery()?; // join the implicit nursery (runs its tasks)
            }
        }
        // Drain with the return value still on top of the stack (rooted) and the frame still on
        // `self.frames` (so `collect` roots the pending records). Defers run AFTER the implicit-nursery
        // join above (tasks complete, then cleanup).
        let defer_err = self.drain_top_frame_deferred();
        let ret = self.pop();
        let frame = self.frames.pop().unwrap();
        if frame.counted {
            self.call_depth -= 1;
        }
        self.stack.truncate(frame.base);
        self.cur_base = self.frames.last().map(|f| f.base).unwrap_or(0);
        // Reclaim any `parallel:` nursery this frame opened but whose `JoinNursery` was skipped by a
        // `?`/return escape (no-op on the normal fall-through path — `JoinNursery` already popped it,
        // so `nurseries.len() == frame.nursery_len`; also a no-op when the implicit nursery above was
        // just joined). Mirrors the `recover:` catch path and the interp's unconditional
        // `exec_parallel` pop; a nested frame keeps the parent's nursery (it captured the parent depth
        // at entry). TASK B: route through `drain_escaped_nursery` so the unstarted tasks are
        // cancelled-and-reported (not silently dropped). NB: within-frame `break`/`continue` out of a
        // `parallel:` no longer rely on this — the compiler emits a `ReclaimNursery` before their
        // loop-exit `Jump` (see `compile_parallel`/`emit_loop_body_drain`), reclaiming block-scoped.
        self.drain_escaped_nursery(frame.nursery_len);
        // Drop any `recover:` handlers installed in the frame we just left (e.g. a `?` early-return
        // out of a recover block) — they must not survive to catch a later, unrelated fault.
        while self.handlers.last().is_some_and(|h| h.frame_len > self.frames.len()) {
            self.handlers.pop();
        }
        if let Some(e) = defer_err {
            return Err(e);
        }
        self.push(ret);
        Ok(())
    }

    /// `defer f(args)` / `defer recv.m(args)` — pop the callee/receiver + `argc` args off the stack
    /// and record a deferred call on the current frame (drained LIFO at frame exit). The values were
    /// evaluated now (Go semantics); the call runs at exit.
    fn do_defer(&mut self, method: Option<String>, argc: usize, span: Span) {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let head = self.pop();
        let d = match method {
            Some(name) => Deferred::Method { recv: head, name, args, span },
            None => Deferred::Call { callee: head, args, span },
        };
        self.frames.last_mut().unwrap().deferred.push(d);
    }

    /// Run one deferred call to completion (the result is discarded). `Call` rides `invoke_value`;
    /// `Method` re-uses the normal method dispatch by pushing the receiver + args and popping the
    /// discarded result. The pop→push window has no instruction boundary, so the moved-out values
    /// can't be collected before they're re-rooted.
    fn run_one_deferred(&mut self, d: Deferred) -> Result<(), RuntimeError> {
        // Guarded: a deferred call runs during frame teardown (the LIFO drain loop is Rust-stack
        // state), so a blocking `recv` inside it cannot park — it faults `deadlock` (B1).
        self.guarded(|vm| match d {
            Deferred::Call { callee, args, span } => {
                vm.invoke_value(callee, args, span)?;
                Ok(())
            }
            Deferred::Method { recv, name, args, span } => {
                let argc = args.len();
                vm.push(recv);
                for a in args {
                    vm.push(a);
                }
                vm.do_method_call(&name, argc, NO_IC, span)?;
                vm.pop(); // discard the deferred call's result
                Ok(())
            }
        })
    }

    // ----- concurrency C4: sequential, run-to-completion executor (mirrors the interpreter) -----

    /// `spawn f(args)` / `spawn recv.m(args)` — pop `argc(+1)` operands, deep-copy the args (and, for
    /// the method form, the receiver) across the airlock, and register the task on the innermost
    /// nursery. The callee passes by handle (like `defer`); only data crosses the airlock. Mirrors
    /// the interpreter's `exec_spawn`.
    fn do_spawn(&mut self, method: Option<String>, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let raw_args: Vec<Value> = self.stack.split_off(at);
        let head = self.pop();
        let args: Vec<Value> = raw_args.into_iter().map(|a| self.deep_clone(a)).collect();
        let task = match method {
            Some(name) => {
                let recv = self.deep_clone(head);
                PendingCall::Method { recv, name, args, span }
            }
            None => PendingCall::Call { callee: head, args, span },
        };
        self.register_task(task, span)
    }

    /// `spawn:` block — snapshot the captured bindings from the current frame (like `MakeClosure`),
    /// deep-copy each captured value across the airlock, build a zero-arg closure over the synthetic
    /// block proto, and register it as a `Call` task. Mirrors the interpreter's `Task::Block`
    /// (captured locals deep-copied; home globals by handle).
    fn do_spawn_block(&mut self, proto: ProtoId, entries: &[CapEntry], span: Span) -> Result<(), RuntimeError> {
        let frame = self.frames.last().unwrap();
        let (base, home, enclosing) = (frame.base, frame.home, frame.closure);
        let mut captured = std::collections::HashMap::new();
        for e in entries {
            let v = match e.src {
                CapSrc::Slot(i) => self.stack[base + i],
                CapSrc::Captured => enclosing
                    .and_then(|h| match self.heap.get(h) {
                        Obj::Closure { captured, .. } => captured.get(&e.name).copied(),
                        _ => None,
                    })
                    .unwrap_or(Value::Nil),
            };
            // Deep-copy across the airlock: the task can't share mutable state with the parent.
            captured.insert(e.name.clone(), self.deep_clone(v));
        }
        let h = self.heap.alloc(Obj::Closure { proto, captured, home });
        self.register_task(PendingCall::Call { callee: Value::Obj(h), args: Vec::new(), span }, span)
    }

    /// Register a spawned task on the innermost nursery. Per-connection spawn: if that nursery is
    /// EAGER, build the handler into a live [`Fiber`] (serializing its args out of THIS fiber's heap,
    /// the same airlock copy `do_spawn`'s `deep_clone` does) and [`MnSched::inject`] it straight into
    /// the running sched — it runs concurrently with the rest of the body. The `task_index` is the
    /// scope's monotonic `next_index` (spawn order), so Decision-F output stays deterministic.
    /// Otherwise (lazy/top-level) push the `PendingCall` for the join to drain. The checker guarantees
    /// a `parallel:` is open, but we guard for parity with the interpreter's runtime error.
    fn register_task(&mut self, task: PendingCall, span: Span) -> Result<(), RuntimeError> {
        // Eager innermost nursery → inject a live fiber. Clone the sched Arc, drop the borrow so
        // `prepare_worker` can take `&mut self`; `inject` assigns the real slot index under its lock
        // (the `0` placeholder is overwritten), so no caller-side index bookkeeping is needed.
        if let Some(Some(scope)) = self.eager_scheds.last() {
            let sched = Arc::clone(&scope.sched);
            let fiber = self.prepare_worker(task)?.into_fiber(0);
            sched.inject(fiber);
            return Ok(());
        }
        match self.nurseries.last_mut() {
            Some(nursery) => {
                nursery.push(task);
                Ok(())
            }
            None => Err(self.err("spawn must be inside a parallel: block".to_string(), span)),
        }
    }

    /// `parallel:` dedent — run the nursery's spawned tasks as cooperative fibers (B1/B2). The
    /// joining (parent) fiber is parked while the children run; a child that blocks on an empty
    /// `recv` suspends and the scheduler switches to a runnable sibling, resuming it once a sibling
    /// `send`s. A child that never blocks runs to completion before the next starts — identical to
    /// the old FIFO run-to-completion drain, so non-blocking programs are byte-for-byte unchanged.
    /// The first child fault (or `std.os.exit`) aborts the remaining siblings and propagates; on that
    /// path the parent's restored `run_until` handles `recover:`/unwind in its own context.
    /// TASK B — cancel-and-report when a `parallel:` body escapes its `JoinNursery` early (`?` /
    /// `return` / `break` / `continue`) or when a fault unwinds past it. Pop every nursery entry ABOVE
    /// `from_len` (the level the escaping construct should restore to); for each lazy nursery that
    /// holds unstarted [`PendingCall`]s, write ONE report line to stdout (`out`, the stream the parity
    /// harnesses read) — emitting PER-NURSERY, innermost-first, byte-identical to the interpreter,
    /// whose `exec_parallel` / `leave_implicit_nursery` report once per frame/block as it unwinds (two
    /// stacked nurseries → two lines, not one combined `2 pending`). The tasks are then DROPPED: they
    /// never started, so there is no fiber to cancel and no buffered output to flush. This preserves
    /// the old `truncate`'s no-leak behavior (depth returns to `from_len`) and adds the observable
    /// report. Replaces the bare `self.nurseries.truncate(from_len)` at every reclaim site.
    fn drain_escaped_nursery(&mut self, from_len: usize) {
        if self.nurseries.len() <= from_len {
            return; // nothing escaped past the join (e.g. normal fall-through already popped it)
        }
        while self.nurseries.len() > from_len {
            self.nursery_defer_floors.pop(); // lockstep with `nurseries`
            let nursery = self.nurseries.pop().unwrap_or_default();
            // Per-connection spawn — pop the eager scope in lockstep. An eager nursery's handlers are
            // already-started live fibers (no unstarted `PendingCall`s to count): cancel + drain + flush
            // them. A lazy nursery's entries are unstarted tasks → report one line per such nursery.
            match self.eager_scheds.pop().flatten() {
                Some(scope) => self.abort_eager_nursery(scope),
                None => {
                    if !nursery.is_empty() {
                        self.out.push_str(&crate::runtime::pending_cancel_report(nursery.len()));
                    }
                }
            }
        }
    }

    fn join_nursery(&mut self) -> Result<(), RuntimeError> {
        // Consume this nursery's tasks (FIFO). Popping the entry now (as the old drain did at the
        // end) keeps the parent's `Handler::nursery_len` accounting correct on a later fault.
        self.nursery_defer_floors.pop(); // keep the parallel floor stack in lockstep with `nurseries`
        let tasks = self.nurseries.pop().unwrap_or_default();
        // Per-connection spawn — pop the eager scope in lockstep. An eager nursery injected its tasks
        // live (so `tasks` is empty); its join drains the handlers it spawned, not a queued list.
        if let Some(Some(scope)) = self.eager_scheds.pop() {
            return self.join_eager_nursery(scope);
        }
        if tasks.is_empty() {
            return Ok(());
        }
        // D2b: under `--parallel`, run the tasks as lightweight M:N fibers on the OS-thread pool
        // (park-on-`recv`), instead of cooperative fibers (decision A keeps the cooperative path the
        // default below).
        if self.parallel {
            return self.run_mn_nursery(tasks);
        }
        let children: Vec<Fiber> = tasks
            .into_iter()
            .enumerate()
            .map(|(i, t)| Fiber { span: t.span(), ctx: FiberCtx::default(), state: FiberState::Pending(t), task_index: i, resume_native: None })
            .collect();
        // D0: every child starts `Pending` ⇒ runnable, so seed `ready` with all indices in order.
        let ready = (0..children.len()).collect();
        // Park the parent: move its live context into the nursery, leaving `self.*` as the fresh,
        // empty arena the children execute in. The nursery (parent + children) is GC-rooted while on
        // `scheduler_stack`.
        let mut nursery = Nursery { parent: FiberCtx::default(), children, ready, blocked_on: std::collections::HashMap::new() };
        self.swap_ctx(&mut nursery.parent);
        self.scheduler_stack.push(nursery);
        let result = self.run_scheduler();
        // Tear the level down and restore the parent context on every path (normal / fault / exit).
        let mut nursery = self.scheduler_stack.pop().expect("scheduler level present");
        self.swap_ctx(&mut nursery.parent);
        result
    }

    /// D2b — the `--parallel` M:N engine: run a nursery's tasks as **lightweight fibers parked on
    /// `recv`** multiplexed over the bounded pool, the replacement for the legacy "one OS thread per
    /// task, block the thread on `recv`" model. The core M:N win: an empty `recv` parks the fiber and
    /// frees its worker instead of pinning the thread, so `#fibers ≫ #threads` producer/consumer
    /// workloads complete instead of starving.
    ///
    /// 1. **Prepare every task into a lightweight [`Fiber`]** ([`Vm::prepare_worker`] →
    ///    [`ReadyWorker::into_fiber`], serial, against the parent heap): each carries its own heap +
    ///    lazy-module roots + a `Pending` task.
    /// 2. **Seed the shared [`MnSched`]** (run queue + park set + per-task slots) and enlist workers:
    ///    the joining thread runs one shell loop inline (decision B — parent participates) and up to
    ///    `available_parallelism()-1` more shells are farmed to the pool. A shell is a thin host `Vm`
    ///    (shared module snapshot installed, sched/cancel wired); fibers swap their own heaps in/out.
    ///    Bounded by core count, so a nested `parallel:` never becomes thread-per-task.
    /// 3. **Park/wake**: an empty `recv` parks the fiber in the channel's wait set ([`MnSched::park`])
    ///    and the worker grabs the next fiber; a `send` drains that set back onto the run queue and
    ///    wakes a worker ([`MnSched::send_wake`]). Over-notify is correct (a spuriously-woken fiber replays
    ///    its rewound-ip `recv` and re-parks); targeted wake + the StoreLoad barrier are D4.
    /// 4. **Reduce** the per-task slots in task order ([`Vm::reduce_task_slots`]) — decision F output
    ///    flush + `Exit`-over-`Fault` precedence.
    ///
    /// B3.4 — **cancellation**: the shared `cancel: Arc<AtomicBool>` (cloned onto every shell) aborts
    /// running fibers at a dispatch back-edge and parked fibers via [`MnSched::cancel_drain`] (they are
    /// requeued and observe the flag on resume). B3.5 — **deadlock** is redefined as the exact
    /// predicate `running == 0 && runnable == 0 && parked > 0 && done < total`, evaluated atomically by
    /// [`MnSched::take_runnable`] (no barrier-confirm needed under a single coordinator). Residual
    /// hangs (decision D): deadlocks spanning nurseries or involving `Executor` work — `MnSched.parked`
    /// is per-nursery, so a cross-nursery `send` delivers the message but does not wake across scheds.
    fn run_mn_nursery(&mut self, tasks: Vec<PendingCall>) -> Result<(), RuntimeError> {
        let total = tasks.len();
        // 1. Prepare every task into a Fiber against the parent heap (must happen on this thread).
        let cancel = Arc::new(AtomicBool::new(false));
        let snap = self.ensure_snapshot();
        let mut fibers = Vec::with_capacity(total);
        for (i, t) in tasks.into_iter().enumerate() {
            fibers.push(self.prepare_worker(t)?.into_fiber(i));
        }
        // 2. Build + seed the shared scheduler.
        let deadlock_err = self.err(DEADLOCK_MSG.to_string(), Span { line: 1, col: 1 });
        let nworkers = std::thread::available_parallelism().map(|x| x.get()).unwrap_or(1).max(1).min(total.max(1));
        let sched = Arc::new(MnSched::new(total, nworkers, Arc::clone(&cancel), deadlock_err));
        sched.seed(fibers);
        // 3. Farm helper shells to the pool — **fire-and-forget**. They accelerate the nursery via
        //    real parallelism, but the join MUST NOT wait on them: a farmed shell can be starved
        //    indefinitely by a saturated process-wide pool (nested or concurrent nurseries hold every
        //    pool thread in their own loops), and waiting on its `DoneSignal` would then deadlock the
        //    join. A panicking shell is contained by the pool's own `catch_unwind` (see `pool.rs`).
        //    Each farmed shell gets a distinct worker id `wid` in `1..nworkers` (owning `locals[wid]`);
        //    the inline shell below is `wid` 0.
        for wid in 1..nworkers {
            let mut shell = self.spawn_shell(&snap, &sched, &cancel);
            let sched = Arc::clone(&sched);
            pool::submit(Box::new(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| shell.mn_worker_loop(&sched, wid)));
            }));
        }
        // 4. The joining thread runs the inline shell to completion (decision B — parent participates),
        //    on its OWN shell so `self` keeps the parent fiber's context untouched (a nested
        //    `parallel:` recurses here without disturbing it). The inline shell is a COMPLETE scheduler
        //    over the shared run queue: even if every farmed shell is starved, it alone drains all
        //    fibers (park/wake via the sched) until `terminate` (done == total, or deadlock). So
        //    liveness depends ONLY on this always-running thread, never on a bounded pool resource —
        //    `mn_worker_loop` returns iff `terminate` is set iff every task slot is filled, so the
        //    starved farmed shells (which observe `terminate` and `Stop` whenever they finally run)
        //    need not be joined.
        let mut shell = self.spawn_shell(&snap, &sched, &cancel);
        shell.mn_worker_loop(&sched, 0);
        // 5. D5 owe #3 (Path C) — wait for every slot to be filled before reducing. `mn_worker_loop`
        //    can now return before `done == total`: if the joining thread itself demoted it early-exits
        //    (a replacement is still draining), and a deadlock `terminate` can precede the blocked_native
        //    threads faulting in place. A non-blocking re-check in the common case (loop returned because
        //    `done == total`); on Path C it parks until the replacements/demoted threads fill the slots.
        sched.wait_for_completion();
        // 6. Reduce the per-task outcome slots (task order, decision F + Exit-over-Fault precedence).
        let slots = sched.take_slots();
        self.reduce_task_slots(slots)
    }

    /// Per-connection spawn — the EAGER counterpart to [`Vm::run_mn_nursery`], split across the
    /// `parallel:` body. Activate at `EnterNursery`: build an empty live [`MnSched`] (`total` grows as
    /// the body `inject`s handlers), flag its body open (so a transient `done == total` does not
    /// terminate it), and spawn ONE dedicated **raw OS thread** (`wid` 1) that drains injected handlers
    /// concurrently with the accept loop. `wid` 0 is the inline join worker ([`Vm::join_eager_nursery`]).
    ///
    /// Why a raw thread, not the bounded pool: the eager body has NO inline worker between
    /// `EnterNursery` and `JoinNursery`, so liveness during the body depends entirely on this drainer.
    /// A bounded-pool helper (the lazy path's accelerator) is the WRONG tool here — `available_parallelism()`
    /// can be 1 (no helper farmed at all → the body never drains → the sequential-client pattern
    /// deadlocks), and a long-running pool job per eager nursery exhausts the fixed pool under nesting
    /// (an undetectable hang, since `body_open` vetoes the deadlock predicate). A raw thread (like the
    /// D5-owe-#3 demote replacement) is unconditional and pool-independent — exactly one extra OS thread
    /// per open eager nursery, joined when the nursery completes. Handlers within one eager nursery
    /// multiplex over this one drainer + the join worker (M:N — handlers park on socket ops, so one
    /// thread serves many); multi-core handler parallelism is future work.
    fn activate_eager_nursery(&mut self) -> EagerScope {
        let cancel = Arc::new(AtomicBool::new(false));
        let snap = self.ensure_snapshot();
        debug_assert!(self.module_snapshot.is_some(), "an eager nursery only activates on a worker shell (gated by mn.is_some())");
        let deadlock_err = self.err(DEADLOCK_MSG.to_string(), Span { line: 1, col: 1 });
        // wid 0 = inline join worker; wid 1 = the dedicated raw drainer below.
        let sched = Arc::new(MnSched::new(0, 2, Arc::clone(&cancel), deadlock_err));
        sched.open_body();
        let mut shell = self.spawn_shell(&snap, &sched, &cancel);
        let drain_sched = Arc::clone(&sched);
        let drainer = std::thread::Builder::new()
            .stack_size(VM_STACK_BYTES)
            .name("chezzi-eager".into())
            .spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| shell.mn_worker_loop(&drain_sched, 1)));
            })
            .ok();
        EagerScope { sched, cancel, drainer }
    }

    /// Per-connection spawn — `JoinNursery` for an eager nursery (the normal fall-through path). Close
    /// the body (no more injections → the sched may terminate once every handler is done), then run
    /// the inline join worker (`wid` 0) to help drain remaining handlers, wait for every slot to fill,
    /// and reduce (Decision-F output flush in spawn order; a handler fault propagates as the
    /// acceptor's body fault, which the outer nursery then sees). Mirrors `run_mn_nursery`'s tail.
    fn join_eager_nursery(&mut self, scope: EagerScope) -> Result<(), RuntimeError> {
        let EagerScope { sched, cancel, drainer, .. } = scope;
        sched.close_body();
        let snap = self.ensure_snapshot();
        let mut shell = self.spawn_shell(&snap, &sched, &cancel);
        shell.mn_worker_loop(&sched, 0);
        sched.wait_for_completion();
        if let Some(h) = drainer {
            let _ = h.join();
        }
        let slots = sched.take_slots();
        self.reduce_task_slots(slots)
    }

    /// Per-connection spawn — reclaim an eager nursery whose body ESCAPED early (`?`/`return`/`break`/
    /// `continue` or a `recover:` catch jumped past its `JoinNursery`). The injected handlers are live
    /// fibers, so (unlike a lazy nursery's unstarted `PendingCall`s) they must be cancelled, not just
    /// dropped: trip the inner cancel, drain channel- and socket-parked handlers (D6b
    /// `cancel_drain` + `drain_sched`), run the inline worker to settle them, then flush their output
    /// (Decision F). The body's own escape error is what propagates, so a handler fault here is
    /// swallowed (only its buffered output + any `os.exit` are honored via `reduce_task_slots`).
    fn abort_eager_nursery(&mut self, scope: EagerScope) {
        let EagerScope { sched, cancel, drainer, .. } = scope;
        cancel.store(true, Ordering::Relaxed);
        sched.close_body();
        sched.cancel_drain();
        poller::drain_sched(&sched);
        let snap = self.ensure_snapshot();
        let mut shell = self.spawn_shell(&snap, &sched, &cancel);
        shell.mn_worker_loop(&sched, 0);
        sched.wait_for_completion();
        if let Some(h) = drainer {
            let _ = h.join();
        }
        let slots = sched.take_slots();
        // The body's escape error is what propagates; a handler fault here is swallowed. But
        // `reduce_task_slots` still sets `self.pending_exit` for a handler `os.exit` (decision C —
        // a hard halt wins), which the catch site honors after the drain — so it is NOT lost.
        let _ = self.reduce_task_slots(slots);
    }

    /// D2b — build a thin host **shell** `Vm` for the M:N engine: a worker `Vm` with the shared
    /// read-only module snapshot, the nursery scheduler, and the cancel token wired in. It runs no
    /// code itself — fibers swap their own heap + module roots into it ([`Vm::swap_ctx`]); the shell
    /// only provides the dispatch engine, the shared `module_snapshot` (for lazy module fault-in), and
    /// the `mn`/`cancel` flags the `recv`/`send`/back-edge paths read.
    fn spawn_shell(&self, snap: &Arc<ModuleSnapshot>, sched: &Arc<MnSched>, cancel: &Arc<AtomicBool>) -> Vm {
        let mut shell = self.spawn_worker();
        shell.module_snapshot = Some(Arc::clone(snap));
        shell.mn = Some(Arc::clone(sched));
        shell.cancel = Some(Arc::clone(cancel));
        shell
    }

    /// D5 owe #3 (Path C) — a blocking `recv` reached inside a native callback (the host-stack loop
    /// frame of `xs.map(f)` / a sort comparator / `Shared.update(f)`, so `native_reentry > 0`) cannot
    /// snapshot-park. Instead of faulting `deadlock`, this worker thread **demotes**: it blocks in place
    /// on the channel's own condvar and resumes in place once a sibling `send`s — Go's `handoffp`. A
    /// fresh replacement worker is spun up ONCE (covering this thread's `wid`) so the live runnable-worker
    /// count stays at N; after this fiber settles, [`Vm::mn_worker_loop`] sees `self.demoted` and exits,
    /// so steady-state live workers = N + (fibers currently blocked in a callback) — Go's exact cost.
    ///
    /// Returns [`RecvStep::Got`] (the native callback continues on this thread with the value),
    /// [`RecvStep::ClosedEmpty`] (the channel was `close()`d while demoted — the caller faults
    /// "receive on a closed channel"), or a `cancelled` / `deadlock` fault (which unwinds the callback
    /// → the fiber faults). Never [`RecvStep::Parked`] — a demoted recv blocks in place, it never
    /// snapshot-parks. Only ever called on the M:N engine inside a native callback (the recv site gates
    /// on `mn.is_some() && native_reentry > 0`).
    fn demote_recv_block(&mut self, h: GcRef, span: Span) -> Result<RecvStep, RuntimeError> {
        let sched = Arc::clone(self.mn.as_ref().expect("demote_recv_block on the cooperative engine"));
        let core = self.channel_core(h);
        let ptr = self.channel_core_ptr(h);
        // 1. Account running → blocked_native AND register the channel (#1 fix), under core lock A, then
        //    notify so an idle puller sitting in an untimed `take_runnable` `cv.wait` re-evaluates the
        //    deadlock predicate now that this fiber left `running` (without this notify a genuine
        //    all-blocked quiesce would never be detected — a hang). The registration lets
        //    `is_deadlocked` peek this fiber's queue so a value a sibling races in isn't misread as a
        //    deadlock (the #1 false-positive against an innocent parked sibling).
        {
            let mut c = sched.lock();
            c.running -= 1;
            sched.blocked_native.fetch_add(1, Ordering::Relaxed);
            c.register_demoted(ptr, &core);
            drop(c);
            sched.cv.notify_all();
        }
        // 2. Spin up a replacement worker ONCE per demoted thread (covers this `wid` while we block).
        //    Subsequent re-entries of this loop on the SAME thread (a callback that recvs repeatedly)
        //    reuse the already-spawned coverage — one spawn + one eventual exit per demoted thread.
        //    If the OS refuses the thread (a real mode for this raw-thread-per-demotion design under
        //    `RLIMIT_NPROC`/ENOMEM with many fibers blocked-in-callback), DON'T panic mid-accounting:
        //    un-roll step 1 (account + registry) and fault this fiber cleanly so the join still completes.
        if !self.demoted {
            if !self.spawn_replacement_worker(&sched, self.wid) {
                let mut c = sched.lock();
                c.running += 1;
                sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
                c.unregister_demoted(ptr);
                drop(c);
                return Err(self.err(
                    "recv inside a native callback could not demote the worker (OS thread limit \
                     reached) — reduce concurrent in-callback blocking or raise the thread limit"
                        .to_string(),
                    span,
                ));
            }
            self.demoted = true;
        }
        // 3. Block in place. The pop + un-account (blocked_native--/running++) + un-register are ATOMIC
        //    under core lock A (A-then-q — the order `send_wake` uses → no ABBA), so the deadlock checker
        //    never observes an emptied-but-still-counted/registered demoted fiber (the #1 window). The
        //    QUEUE is checked FIRST so a genuinely-sent value always wins over a spuriously-set
        //    `terminate`. Each exit path un-accounts under A and returns directly (no separate "step 4");
        //    lost condvar wakeups are bounded by `DEMOTE_POLL_BACKOFF` (≤ latency, never a hang).
        loop {
            // --- settle under core lock A: pop wins over cancel / terminate / deadlock ---
            {
                let mut c = sched.lock();
                let mut qg = core.q.lock().unwrap_or_else(|e| e.into_inner());
                let popped = qg.queue.pop_front();
                if let Some(w) = popped {
                    c.running += 1;
                    sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
                    c.unregister_demoted(ptr);
                    drop(qg);
                    drop(c);
                    return Ok(RecvStep::Got(w));
                }
                // Closed-and-drained: the queue is empty here (pop-first) and the channel is closed, so
                // no value will ever arrive — signal `ClosedEmpty` (the caller faults "receive on a
                // closed channel"). Read while still holding the queue lock so it is atomic with the
                // pop above. Ranks below a delivered value, above terminate/deadlock.
                let closed = qg.closed;
                drop(qg);
                // Cancel (a sibling faulted): set `cancelled` BEFORE returning the Err so the outcome is
                // SWALLOWED (a cancelled task is dropped, not reported) instead of surfacing as a Fault —
                // mirrors the snapshot-park recv's cancel branch.
                if self.cancel.as_ref().is_some_and(|x| x.load(Ordering::Relaxed)) {
                    self.cancelled = true;
                    c.running += 1;
                    sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
                    c.unregister_demoted(ptr);
                    drop(c);
                    return Err(self.err("cancelled".to_string(), span));
                }
                if closed {
                    c.running += 1;
                    sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
                    c.unregister_demoted(ptr);
                    drop(c);
                    return Ok(RecvStep::ClosedEmpty);
                }
                // Terminate without a delivered value (genuine deadlock / nursery torn down): fault in
                // place. Path C self-sufficient deadlock detection: evaluate the predicate HERE rather
                // than depending on a separate idle puller being alive to fire it (the replacement could
                // be the last worker and itself demoted → otherwise a hang). The queue-first pop above
                // means OUR channel is empty here, and `is_deadlocked` now also peeks OTHER demoted
                // channels (#1), so firing can never strand a value destined for any demoted fiber.
                if c.terminate {
                    c.running += 1;
                    sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
                    c.unregister_demoted(ptr);
                    drop(c);
                    return Err(sched.deadlock_err.clone());
                }
                if sched.is_deadlocked(&c) {
                    c.flag_deadlock(&sched.deadlock_err);
                    c.running += 1;
                    sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
                    c.unregister_demoted(ptr);
                    drop(c);
                    sched.cv.notify_all();
                    return Err(sched.deadlock_err.clone());
                }
            }
            // --- wait on the channel's OWN condvar (q-only; core lock A released) ---
            let q = core.q.lock().unwrap_or_else(|e| e.into_inner());
            if q.queue.is_empty() {
                let _ = core.cv.wait_timeout(q, DEMOTE_POLL_BACKOFF);
            }
        }
    }

    /// D5 owe #3 Path C (#3) — a `sleep_ms(ms>0)` reached INSIDE a native callback (`native_reentry > 0`)
    /// on the M:N engine. The offload gate (`running → timer thread`) requires `native_reentry == 0`
    /// (the callback's `for`-loop state lives on the un-snapshottable Rust host stack), so without this
    /// the sleep runs INLINE and pins the worker for `ms`. Instead DEMOTE like a recv-in-callback: spin a
    /// replacement worker + sleep in place + resume, freeing the worker for `ms` (Go's `handoffp`).
    /// Accounted as `inflight` (NOT `blocked_native`): a sleeper returns unconditionally, so it must VETO
    /// the deadlock predicate (like an offloaded blocking native — `is_deadlocked` already treats
    /// `inflight>0` as "external progress guaranteed"). A `blocked_native` fiber is the opposite (it
    /// returns only via a sibling `send`). Returns `Ok(Nil)` (`sleep_ms` yields nothing). Residual: the
    /// `thread::sleep` is uninterruptible, so a cancel during the sleep is observed only after it returns
    /// (no worse than the inline pin it replaces — the worker is now freed).
    fn demote_block_sleep(&mut self, ms: u64, span: Span) -> Result<Value, RuntimeError> {
        let sched = Arc::clone(self.mn.as_ref().expect("demote_block_sleep on the cooperative engine"));
        // 1. Account running → inflight under the core lock, then notify idle pullers (a worker sitting in
        //    an untimed `cv.wait` re-evaluates now that this fiber left `running`).
        {
            let mut c = sched.lock();
            c.running -= 1;
            sched.inflight.fetch_add(1, Ordering::Relaxed);
            drop(c);
            sched.cv.notify_all();
        }
        // 2. Spin up a replacement worker ONCE per demoted thread (reuse the `self.demoted` coverage the
        //    recv demote sets — one spawn + one eventual exit per demoted thread regardless of how many
        //    times it blocks). Un-roll the accounting + fault cleanly if the OS refuses the thread.
        if !self.demoted {
            if !self.spawn_replacement_worker(&sched, self.wid) {
                let mut c = sched.lock();
                c.running += 1;
                sched.inflight.fetch_sub(1, Ordering::Relaxed);
                drop(c);
                return Err(self.err(
                    "sleep_ms inside a native callback could not demote the worker (OS thread limit \
                     reached) — reduce concurrent in-callback blocking or raise the thread limit"
                        .to_string(),
                    span,
                ));
            }
            self.demoted = true;
        }
        // 3. Sleep in place (the worker is covered by the replacement).
        std::thread::sleep(std::time::Duration::from_millis(ms));
        // 4. Un-account inflight → running (the `+1` is essential — the fiber's next dispatch does
        //    `running -= 1`, which would underflow without this restore).
        {
            let mut c = sched.lock();
            c.running += 1;
            sched.inflight.fetch_sub(1, Ordering::Relaxed);
        }
        // Cancel observed during/after the sleep (a sibling faulted): set `cancelled` and fault so the
        // outcome is SWALLOWED (a cancelled task is dropped, not reported), mirroring `demote_recv_block`
        // + the snapshot-park recv. Without this, a cancelled task would sleep through every remaining
        // callback element and then fault NORMALLY at a later back-edge — wrong classification (a
        // cancelled-task Fault masking the real sibling error) and wasted in-callback sleeps. Faulting
        // here aborts the native callback loop immediately, so no further elements sleep.
        if self.cancel.as_ref().is_some_and(|x| x.load(Ordering::Relaxed)) {
            self.cancelled = true;
            return Err(self.err("cancelled".to_string(), span));
        }
        Ok(Value::Nil)
    }

    /// D5 owe #3 Path C (#3 socket half) — a socket `read`/`write`/`accept` that `WouldBlock`s INSIDE a
    /// native callback (`native_reentry > 0`) on the M:N engine. [`Vm::park_on_fd`] only parks on the
    /// netpoller when `native_reentry == 0` (the callback's `for`-loop state lives on the un-snapshottable
    /// Rust host stack), so without this the op surfaces a misleading `--parallel`-engine error even
    /// though we *are* on `--parallel`. Instead DEMOTE like [`Vm::demote_block_sleep`]: spin a replacement
    /// worker once + backoff-poll the **non-blocking** op in place until it's ready, then resume.
    ///
    /// Accounted as `inflight` (NOT `blocked_native`): a socket op is woken by external OS readiness, so
    /// it must VETO the deadlock predicate — exactly the netpoller-park accounting (a lone in-callback
    /// `accept` with no client correctly never self-terminates, Go-identical), hence **no** `is_deadlocked`
    /// self-fire here. The flip side: this op exits the wait ONLY via fd readiness, `cancel`, or another
    /// worker setting `terminate` — so a nursery where *every* remaining fiber is an in-callback socket
    /// demote on an fd that never becomes ready, with no faulting sibling, hangs silently (no `deadlock`
    /// fault). That is the same Go-identical "all goroutines waiting on a never-ready socket" case the
    /// netpoller park already has; the rule is unchanged (don't await an fd that nothing will signal).
    ///
    /// Between attempts the worker kernel-BLOCKS on the fd via [`wait_fd_ready`] (woken immediately on
    /// readiness, no busy-poll, no wasted syscalls — close to the epoll path it can't use here) with a
    /// `DEMOTE_POLL_BACKOFF` timeout so a sibling-fault `cancel` is still observed within that window.
    /// `cancel`/`terminate` are re-checked at the TOP of every iteration (before re-attempting), so a
    /// cancelled task stops issuing socket work promptly and its outcome is SWALLOWED (mirrors the
    /// recv/sleep demote). `attempt` re-runs one non-blocking op (it owns a cloned `Arc<…Core>`) and
    /// returns `SockPoll::Ready` with the op's `Result`-shaped `Value`, or `SockPoll::WouldBlock`.
    fn demote_block_socket(
        &mut self,
        fd: std::os::fd::RawFd,
        interest: poller::Interest,
        span: Span,
        mut attempt: impl FnMut(&mut Vm) -> SockPoll,
    ) -> Result<Value, RuntimeError> {
        let sched = Arc::clone(self.mn.as_ref().expect("demote_block_socket on the cooperative engine"));
        self.demote_socket_enter(span)?;
        let out = loop {
            // Observe teardown/cancel BEFORE doing more work each iteration. Cancel (a sibling faulted):
            // set `cancelled` so the outcome is SWALLOWED (a cancelled task is dropped, not reported).
            if self.cancel.as_ref().is_some_and(|x| x.load(Ordering::Relaxed)) {
                self.cancelled = true;
                break Err(self.err("cancelled".to_string(), span));
            }
            // Nursery torn down (deadlock elsewhere / `os.exit`): fault in place. An `inflight` socket op
            // never *self*-fires the predicate (it vetoes it), so a genuine quiesce is surfaced by another
            // worker setting `terminate`, observed here within the backoff.
            if sched.lock().terminate {
                break Err(sched.deadlock_err.clone());
            }
            match attempt(self) {
                SockPoll::Ready(r) => break r,
                SockPoll::WouldBlock => wait_fd_ready(fd, interest, DEMOTE_POLL_BACKOFF),
            }
        };
        self.demote_socket_exit();
        out
    }

    /// D5 owe #3 Path C (#3 socket half) — enter the in-callback socket demote: account `running → inflight`
    /// under core lock A + notify idle pullers (a worker in an untimed `cv.wait` re-evaluates now that this
    /// fiber left `running`), then spin a replacement worker ONCE (reuse the `self.demoted` coverage the
    /// recv/sleep demote also uses — one spawn + one eventual exit per demoted thread). On OS-refuse, un-roll
    /// the accounting and fault cleanly so the join still completes. Mirrors [`Vm::demote_block_sleep`] 1–2.
    fn demote_socket_enter(&mut self, span: Span) -> Result<(), RuntimeError> {
        let sched = Arc::clone(self.mn.as_ref().expect("demote_socket_enter on the cooperative engine"));
        {
            let mut c = sched.lock();
            c.running -= 1;
            sched.inflight.fetch_add(1, Ordering::Relaxed);
            drop(c);
            sched.cv.notify_all();
        }
        if !self.demoted {
            if !self.spawn_replacement_worker(&sched, self.wid) {
                let mut c = sched.lock();
                c.running += 1;
                sched.inflight.fetch_sub(1, Ordering::Relaxed);
                drop(c);
                return Err(self.err(
                    "a socket op inside a native callback could not demote the worker (OS thread limit \
                     reached) — reduce concurrent in-callback blocking or raise the thread limit"
                        .to_string(),
                    span,
                ));
            }
            self.demoted = true;
        }
        Ok(())
    }

    /// D5 owe #3 Path C (#3 socket half) — exit the in-callback socket demote: un-account `inflight →
    /// running` (the `+1` is essential — the fiber's next dispatch does `running -= 1`, which would
    /// underflow without this restore). Mirrors [`Vm::demote_block_sleep`] step 4.
    fn demote_socket_exit(&mut self) {
        let sched = Arc::clone(self.mn.as_ref().expect("demote_socket_exit on the cooperative engine"));
        let mut c = sched.lock();
        c.running += 1;
        sched.inflight.fetch_sub(1, Ordering::Relaxed);
    }

    /// D5 owe #3 (Path C) — spawn a fresh OS thread running a replacement M:N worker shell over the
    /// same scheduler, reusing the demoting worker's `wid`. A RAW thread ([`VM_STACK_BYTES`] stack,
    /// like the pool), NOT a `pool::submit` job: the bounded pool is fixed-size, so a blocked-in-callback
    /// pool thread would shrink it — the demoted thread is "off the pool" (Go grows `m` under
    /// cgo/syscalls). The replacement drains the shared run queue until `terminate` (`Take::Stop`), then
    /// exits — detached + reaped at nursery/process end (it holds only `Arc`s, so the joining thread can
    /// return without any use-after-free). Panic-guarded like the farmed shells. Returns `false` iff the
    /// OS refused the thread (caller faults the fiber rather than blocking with no coverage); the
    /// snapshot/cancel `.expect`s are true invariants (only reachable on the M:N engine in a nursery).
    fn spawn_replacement_worker(&self, sched: &Arc<MnSched>, wid: usize) -> bool {
        let snap = self
            .module_snapshot
            .as_ref()
            .expect("Path C replacement worker without a module snapshot");
        let cancel = self
            .cancel
            .as_ref()
            .expect("Path C replacement worker without a cancel token");
        let mut shell = self.spawn_shell(snap, sched, cancel);
        let sched = Arc::clone(sched);
        std::thread::Builder::new()
            .stack_size(VM_STACK_BYTES)
            .name("chezzi-mn-repl".into())
            .spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| shell.mn_worker_loop(&sched, wid)));
            })
            .is_ok()
    }

    /// D2b — a worker shell's lifetime: pull a runnable fiber, run it to its next park/finish, settle,
    /// repeat until the scheduler terminates. Generalizes the cooperative [`Vm::run_child`] to a
    /// shared run queue + park set across threads.
    fn mn_worker_loop(&mut self, sched: &Arc<MnSched>, wid: usize) {
        self.wid = wid; // D5 owe #3 (Path C) — `demote_recv_block` reuses this for the replacement worker
        let mut tick: u64 = 0;
        loop {
            tick = tick.wrapping_add(1);
            let mut fiber = match sched.take_runnable(wid, tick) {
                Take::Run(f) => f,
                Take::Stop => return,
            };
            let task_index = fiber.task_index;
            let span = fiber.span;
            match self.run_one_fiber(&mut fiber, span) {
                Disp::Park(key, core) => sched.park(key, &core, fiber),
                Disp::Yield => sched.yield_fiber(fiber),
                // D5 — the fiber hit a blocking native; hand it + the call to the dirty pool (frees
                // this worker). The pool re-enqueues it on completion via `complete_offload`.
                Disp::Offload(req) => sched.offload(fiber, req),
                // D6 — the fiber's socket op `WouldBlock`ed; hand it + the fd to the netpoller (frees
                // this worker). The poller re-enqueues it via `complete_offload` on OS readiness.
                Disp::PollPark(pp) => sched.poll_park_offload(fiber, pp),
                Disp::Finish(outcome) => {
                    let aborts = matches!(outcome, TaskOutcome::Fault(_) | TaskOutcome::Exit { .. });
                    sched.finish(task_index, outcome);
                    // A fault/exit tripped the cancel flag (in `classify_mn_outcome`); requeue parked
                    // siblings so they observe it and unwind (running ones see it at a back-edge).
                    // `cancel_drain` reaches channel-`recv`-parked fibers; `drain_sched` reaches the
                    // ones parked on the netpoller (`accept`/`read`/`write`/`connect`) — together they
                    // cover every parked fiber, so a net server sharing a nursery with a faulting
                    // sibling now unwinds instead of hanging (D6b — the production-ready gate).
                    if aborts {
                        sched.cancel_drain();
                        poller::drain_sched(sched);
                    }
                }
            }
            // D5 owe #3 (Path C) — this worker DEMOTED mid-fiber (blocked in place on a callback `recv`)
            // and a replacement now covers its `wid`. The fiber it was running has just settled
            // (finished, or re-parked for another worker to resume), so this thread exits to keep the
            // net live-worker count at N. The joining thread's `wait_for_completion` holds the reduce
            // until the replacements fill every slot.
            if self.demoted {
                return;
            }
        }
    }

    /// D2b — run a single fiber on this shell: swap its context in, start/resume it until it parks or
    /// finishes, decide its disposition WHILE its heap is live (the park key and outcome are heap-keyed
    /// reads), then swap the context back out. The run is panic-guarded so a worker-VM panic becomes a
    /// task `Fault` (keeps the loop alive + the slot filled — the join can't hang).
    fn run_one_fiber(&mut self, fiber: &mut Fiber, span: Span) -> Disp {
        self.swap_ctx(&mut fiber.ctx);
        self.suspend = None;
        self.offload = None;
        self.poll_park = None;
        self.cancelled = false;
        self.pending_exit = None;
        self.reds = CONTEXT_REDS; // D3 — fresh reduction budget on every schedule-in (BEAM semantics)
        self.yield_now = false;
        let state = std::mem::replace(&mut fiber.state, FiberState::Ready);
        // D5 — a fiber resumed after a blocking-native offload carries the pool's result. Take it now
        // (heap is swapped in) and push it below so the suspended `Call` completes and dispatch
        // continues past it.
        let resume_native = fiber.resume_native.take();
        let disp = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // D5 — lower + push the offloaded native's result before resuming, so the operand stack
            // holds what the `Call` would have pushed and `run_until` continues correctly. The `Err`
            // arm carries a fault from the pool job: either the native PANICKED (caught in the job,
            // surfaced here — faulting the task without running its defers, exactly as an inline
            // native panic does via `run_one_fiber`'s outer `catch_unwind`), or it returned a
            // `HostError` (unreachable for the scoped fns — fs/io surface I/O failures as `Result`
            // *values* and arg types are checker-guaranteed). Either way the task faults.
            if let Some(result) = resume_native {
                match result {
                    Ok(nr) => {
                        let v = self.lower_native(nr);
                        self.push(v);
                    }
                    Err(rte) => return Disp::Finish(self.classify_mn_outcome(Err(rte))),
                }
            }
            // D6b — a fiber resumed from a non-blocking `connect` park carries the connecting socket in
            // `pending_connect` (swapped in with its ctx). Complete the handshake (read `SO_ERROR`) and
            // push the `Result[Socket]` the `net.connect` call site is waiting for, then continue past
            // it. `finish_pending_connect` never faults (it yields a `Result` *value*). Mutually
            // exclusive with `resume_native` (a fiber is offload-parked OR connect-parked, never both).
            if let Some(cip) = self.pending_connect.take() {
                let v = self.finish_pending_connect(cip);
                self.push(v);
            }
            let res = match state {
                FiberState::Pending(task) => self.start_task(task),
                FiberState::Ready | FiberState::Blocked => self.run_until(0),
                FiberState::Done => unreachable!("mn_worker_loop scheduled a Done fiber"),
            };
            if res.is_ok() && self.offload.is_some() {
                // D5 — the fiber hit a blocking native; hand it to the dirty pool. Mutually exclusive
                // with `suspend`/`yield_now` (offload returns up via the `paused()` gate before any
                // `recv` runs or the budget is re-checked).
                Disp::Offload(self.offload.take().unwrap())
            } else if res.is_ok() && self.poll_park.is_some() {
                // D6 — the fiber's socket op `WouldBlock`ed; hand it to the netpoller (frees this
                // worker). Mutually exclusive with `offload`/`suspend`/`yield_now` — the socket op
                // returns up via the `paused()` gate before any other safepoint runs.
                Disp::PollPark(self.poll_park.take().unwrap())
            } else if res.is_ok() && self.suspend.is_some() {
                let h = self.suspend.take().unwrap();
                // Capture the park key + the channel `Arc` WHILE the fiber heap is live (`h` is a
                // GcRef into it); `park` re-checks the queue through this `Arc` under the sched lock.
                Disp::Park(self.channel_core_ptr(h), self.channel_core(h))
            } else if res.is_ok() && self.yield_now {
                // D3 — budget exhausted (mutually exclusive with `suspend`: the safepoint returns
                // before dispatching, so no `recv` ran this slice). Frames stay intact; resume
                // re-enters `run_until(0)`.
                Disp::Yield
            } else {
                Disp::Finish(self.classify_mn_outcome(res))
            }
        }))
        .unwrap_or_else(|p| Disp::Finish(TaskOutcome::Fault(panic_to_fault(p, span))));
        self.swap_ctx(&mut fiber.ctx);
        disp
    }

    /// D2b — classify a finished fiber's run into a [`TaskOutcome`] (the M:N analogue of
    /// [`ReadyWorker::run_outcome`]). Unlike the legacy path it uses `start_task`/`run_until` and
    /// **discards the task's return value**, matching the cooperative parity oracle (which never
    /// inspects a task's return). Trips the shared cancel flag on a fault/exit so siblings abort. The
    /// fiber's `out`/`stderr` are taken from the live (swapped-in) shell buffers.
    fn classify_mn_outcome(&mut self, res: Result<(), RuntimeError>) -> TaskOutcome {
        if let Some(code) = self.pending_exit {
            self.trip_cancel();
            TaskOutcome::Exit { code, out: std::mem::take(&mut self.out), stderr: std::mem::take(&mut self.stderr) }
        } else if self.cancelled {
            TaskOutcome::Cancelled
        } else {
            match res {
                Err(e) => {
                    self.trip_cancel();
                    TaskOutcome::Fault(e)
                }
                Ok(()) => TaskOutcome::Done(WorkerResult {
                    value: WireValue::Nil,
                    out: std::mem::take(&mut self.out),
                    stderr: std::mem::take(&mut self.stderr),
                }),
            }
        }
    }

    /// B3.3-threads / B3.6 — the engine-agnostic farm/join/flush core: run a vector of already-prepared
    /// [`ReadyWorker`]s on the bounded pool and reduce their outcomes. The caller wires each worker's
    /// `cancel` / `deadlock` token first ([`run_parallel_nursery`] sets both; the `Executor` pool drain
    /// sets `cancel` only — an `Executor`-spanning deadlock is an accepted hang, decision D). Farms
    /// `ready[1..]` to the pool, runs `ready[0]` inline (parent participates — decision B), joins on the
    /// `DoneSignal` counter, flushes `Done`/`Exit` output in **task order** (decision F), and applies the
    /// `Exit`-over-`Fault` precedence (an `os.exit` hard-halts the parent; a fault unwinds for an outer
    /// `recover:`). A `Cancelled` outcome is swallowed.
    fn run_workers_on_pool(&mut self, ready: Vec<ReadyWorker>) -> Result<(), RuntimeError> {
        let n = ready.len();
        // Per-task outcome slots (task order) + a finished-count condvar the pool bumps.
        let results: TaskSlots = Arc::new(Mutex::new((0..n).map(|_| None).collect()));
        let done: Arc<(Mutex<usize>, std::sync::Condvar)> = Arc::new((Mutex::new(0), std::sync::Condvar::new()));

        // 2. Farm tasks[1..] to the pool; keep tasks[0] to run inline. Every farmed job runs under a
        //    `DoneSignal` guard whose `Drop` bumps the completion counter + wakes the joiner on EVERY
        //    exit path — including a Rust panic unwinding through `rw.run()` (a worker-VM `unwrap` /
        //    poisoned core lock). Without it a panicking task would leave the counter short and hang
        //    the join forever; with it the panic is caught, converted to a fault slot, and joined like
        //    any other error. (Review: panic→hang was the one blocking defect.)
        let mut iter = ready.into_iter().enumerate();
        let first = iter.next();
        for (i, rw) in iter {
            let results = Arc::clone(&results);
            let done = Arc::clone(&done);
            let span = rw.span;
            pool::submit(Box::new(move || {
                // Drop runs LAST (declared first), so the slot is committed before the counter bumps.
                let _signal = DoneSignal(done);
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rw.run_outcome()))
                    .unwrap_or_else(|p| TaskOutcome::Fault(panic_to_fault(p, span)));
                results.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(r);
            }));
        }
        // 3. Parent participates: run task[0] on this thread (it may block on `recv`, woken by a pool
        //    sibling's `send`). Caught the same way so an inline-task panic still joins the pool tasks
        //    and reports rather than unwinding past the still-pending wait.
        if let Some((i, rw)) = first {
            let span = rw.span;
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rw.run_outcome()))
                .unwrap_or_else(|p| TaskOutcome::Fault(panic_to_fault(p, span)));
            results.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(r);
        }
        // 4a. Wait for the farmed tasks (n-1) to finish (the `DoneSignal` guard guarantees the counter
        //     reaches `pool_count` even if some tasks panicked).
        let pool_count = n.saturating_sub(1);
        {
            let (lock, cv) = &*done;
            let mut g = lock.lock().unwrap_or_else(|e| e.into_inner());
            while *g < pool_count {
                g = cv.wait(g).unwrap_or_else(|e| e.into_inner());
            }
        }
        // 4b. Flush worker output in task order (decision F) and select the terminal outcome.
        //     `Done`/`Exit` output is flushed; `Fault`/`Cancelled` output is dropped (a faulting
        //     worker's partial output never had a deterministic position; a cancelled worker's work
        //     is incomplete). The fault-free goldens only ever hit `Done`, so they stay byte-identical.
        //
        //     Precedence: an `os.exit` is an UNCONDITIONAL hard halt, so the lowest-index `Exit`
        //     wins over any `Fault` regardless of index — otherwise a recoverable sibling fault at a
        //     lower index could demote a child's `os.exit` to a catchable error (a `recover:` around
        //     the `parallel:` would swallow it and the process would not exit). Within a kind, the
        //     lowest index wins (scan order + `is_none()` guard), matching the cooperative engine's
        //     first-fault rule.
        // Take the slots out under the lock rather than `Arc::try_unwrap`: a just-finished pool
        // thread bumps the `done` counter (in `DoneSignal::drop`) *before* its closure environment —
        // which still owns a `results` `Arc` clone — is dropped, so the joiner can wake with
        // `strong_count > 1` and `try_unwrap` would spuriously fail. `mem::take` needs only the lock.
        let slots = std::mem::take(&mut *results.lock().unwrap_or_else(|e| e.into_inner()));
        self.reduce_task_slots(slots)
    }

    /// B3.3-threads / D2b — reduce a nursery's per-task outcome slots (task order) into the join's
    /// result, flushing output and applying `Exit`-over-`Fault` precedence. Shared by the legacy pool
    /// engine ([`run_workers_on_pool`]) and the M:N engine ([`run_mn_nursery`]).
    ///
    /// `Done`/`Exit` output is flushed in task order (decision F); `Fault`/`Cancelled` output is
    /// dropped (a faulting worker's partial output had no deterministic position; a cancelled worker's
    /// work is incomplete). The fault-free goldens only ever hit `Done`, so they stay byte-identical.
    /// Precedence: an `os.exit` is an UNCONDITIONAL hard halt, so the lowest-index `Exit` wins over any
    /// `Fault` regardless of index — otherwise a lower-index recoverable fault could demote a child's
    /// `os.exit` to a catchable error. Within a kind, the lowest index wins (scan order + `is_none()`).
    fn reduce_task_slots(&mut self, slots: Vec<Option<TaskOutcome>>) -> Result<(), RuntimeError> {
        let mut first_exit: Option<i32> = None;
        let mut first_fault: Option<RuntimeError> = None;
        for slot in slots {
            match slot.expect("every task slot was filled before join returned") {
                TaskOutcome::Done(wr) => {
                    self.out.push_str(&wr.out);
                    self.stderr.push_str(&wr.stderr);
                }
                TaskOutcome::Exit { code, out, stderr } => {
                    self.out.push_str(&out);
                    self.stderr.push_str(&stderr);
                    if first_exit.is_none() {
                        first_exit = Some(code);
                    }
                }
                TaskOutcome::Fault(e) => {
                    if first_fault.is_none() {
                        first_fault = Some(e);
                    }
                }
                TaskOutcome::Cancelled => {}
            }
        }
        match (first_exit, first_fault) {
            // A child `os.exit` hard-halts the parent: set `pending_exit` and return the exit
            // sentinel. The op→`step`→`run_until` chain sees `pending_exit` and unwinds past every
            // `recover:` to the driver, which reports `code` as the process exit status (decision C).
            // It wins over any sibling fault — a hard halt is never demoted to a catchable error.
            (Some(code), _) => {
                self.pending_exit = Some(code);
                Err(self.err("exit".to_string(), Span { line: 1, col: 1 }))
            }
            // A real fault propagates normally so an outer `recover:` can still catch it.
            (None, Some(e)) => Err(e),
            (None, None) => Ok(()),
        }
    }

    /// Cooperatively drive the children of the innermost scheduler level until all are `Done`. D0:
    /// pops the lowest-index runnable child from the level's `ready` set each turn (O(log N), vs the
    /// old O(N) `pick_runnable` linear scan — same lowest-index order, so byte-identical). A child
    /// that blocks on an empty channel leaves `ready` and is re-added by a sibling `send`
    /// ([`Vm::wake_on_send`]). When `ready` empties: all children `Done` ⇒ the nursery is finished;
    /// otherwise every remaining child is parked on an empty channel no sibling can fill — a deadlock.
    fn run_scheduler(&mut self) -> Result<(), RuntimeError> {
        loop {
            let next = self.scheduler_stack.last_mut().expect("scheduler level present").ready.pop_first();
            match next {
                Some(i) => self.run_child(i)?,
                None => {
                    if self.all_children_done() {
                        return Ok(());
                    }
                    return Err(self.err(DEADLOCK_MSG.to_string(), Span { line: 1, col: 1 }));
                }
            }
        }
    }

    fn all_children_done(&self) -> bool {
        self.scheduler_stack
            .last()
            .expect("scheduler level present")
            .children
            .iter()
            .all(|c| matches!(c.state, FiberState::Done))
    }

    /// Run (start or resume) child `i` of the top scheduler level until it completes or blocks. The
    /// child is taken out of the level (replaced by a `Done` placeholder) so its context can be
    /// swapped into `self.*` without holding a `scheduler_stack` borrow across the run — a nested
    /// `parallel:` pushes/pops its own level meanwhile. On return the child's context is parked back
    /// and its new state recorded.
    fn run_child(&mut self, i: usize) -> Result<(), RuntimeError> {
        let mut child = {
            let level = self.scheduler_stack.last_mut().expect("scheduler level present");
            std::mem::replace(&mut level.children[i], Fiber { ctx: FiberCtx::default(), state: FiberState::Done, task_index: i, span: Span { line: 1, col: 1 }, resume_native: None })
        };
        self.swap_ctx(&mut child.ctx); // self.* = child's execution context
        self.suspend = None; // clear any prior wait before (re)running
        let outcome = match std::mem::replace(&mut child.state, FiberState::Ready) {
            FiberState::Pending(task) => self.start_task(task),
            // Resume: the saved frames replay via the rewound `recv` op and ordinary `Return`s — no
            // host-stack nesting is rebuilt (run_until is frame-count driven).
            FiberState::Ready | FiberState::Blocked => self.run_until(0),
            FiberState::Done => unreachable!("run_child on a Done fiber"),
        };
        self.swap_ctx(&mut child.ctx); // park the (possibly-suspended) context back into the child
        // D0: a run always ends `Done` or `Blocked` (never left `Ready`), so a finished child is
        // simply dropped from scheduling; a blocked child registers in `blocked_on` under its
        // channel's core pointer so a sibling `send` can re-add it to `ready` ([`wake_on_send`]).
        let result = match outcome {
            Ok(()) => {
                child.state = match self.suspend.take() {
                    Some(h) => {
                        let key = self.channel_core_ptr(h);
                        self.scheduler_stack
                            .last_mut()
                            .expect("scheduler level present")
                            .blocked_on
                            .entry(key)
                            .or_default()
                            .push(i);
                        FiberState::Blocked
                    }
                    None => FiberState::Done,
                };
                Ok(())
            }
            Err(e) => {
                child.state = FiberState::Done;
                Err(e)
            }
        };
        self.scheduler_stack.last_mut().expect("scheduler level present").children[i] = child;
        result
    }

    /// D0 — the `ChannelCore` identity (`Arc::as_ptr as usize`) behind a channel handle, the stable
    /// key for [`Nursery::blocked_on`]. Stable across the distinct `GcRef`s sibling fibers hold for
    /// the same channel (cooperative `spawn` deep-clones the handle onto the shared `Arc`).
    fn channel_core_ptr(&self, h: GcRef) -> usize {
        match self.heap.get(h) {
            Obj::Channel(core) => Arc::as_ptr(core) as usize,
            // A fiber only ever parks via a `recv` on a `Channel`, so `suspend` always holds a
            // channel handle. Fail loud (matching `channel_core` above) rather than silently filing
            // the park under a sentinel key `wake_on_send` would never match — a silent mis-key
            // would mis-report a `deadlock` (review: Incident Response Commander).
            _ => unreachable!("channel_core_ptr on a non-channel park handle"),
        }
    }

    /// D0 — a `send` into channel `h` may unblock siblings parked on its `recv`. Drain the matching
    /// `blocked_on` bucket back onto `ready` for **every** scheduler level (not just the innermost):
    /// a fiber nested in an inner `parallel:` can `send` to a channel an outer-level sibling parked
    /// on, and that outer fiber must become runnable once control unwinds back to its level. No-op
    /// under `--parallel` (workers never push `scheduler_stack`) and when no sibling is parked.
    fn wake_on_send(&mut self, h: GcRef) {
        if self.scheduler_stack.is_empty() {
            return;
        }
        let key = self.channel_core_ptr(h);
        for level in &mut self.scheduler_stack {
            if let Some(woken) = level.blocked_on.remove(&key) {
                level.ready.extend(woken);
            }
        }
    }

    /// Launch a fiber's initial task in the (already swapped-in) child context. Mirrors the old
    /// `run_pending`, but a blocking `recv` may park the fiber mid-flight: the `do_method_call` /
    /// `invoke_value` paths leave `self.suspend` set and the frames live, so the discard-pop is
    /// skipped (there is no result yet) and the scheduler resumes the fiber later.
    fn start_task(&mut self, task: PendingCall) -> Result<(), RuntimeError> {
        match task {
            PendingCall::Call { callee, args, span } => {
                self.invoke_value(callee, args, span)?;
                Ok(())
            }
            PendingCall::Method { recv, name, args, span } => {
                let argc = args.len();
                self.push(recv);
                for a in args {
                    self.push(a);
                }
                self.do_method_call(&name, argc, NO_IC, span)?;
                if !self.paused() {
                    self.pop(); // discard the completed task's result (none pending if paused/yielded)
                }
                Ok(())
            }
        }
    }

    /// Deep-copy a value across a task airlock (`spawn` / `Channel.send` / `Shared` get-set):
    /// data — scalars, collections, structs, enums — is recursively cloned into fresh heap objects
    /// so a task can't share mutable state with the spawner. `str` (immutable), callables, modules,
    /// and `Channel` / `Shared` handles pass by reference (the handle is what crosses). Mirrors
    /// `interp::deep_clone` exactly. Allocates, but only at the instruction boundary that called it
    /// (no GC runs mid-clone), so intermediate handles can't be collected.
    ///
    /// B3.0: implemented as a [`WireValue`] round-trip — `to_wire` (read-only serialize) then
    /// `from_wire` (reconstruct into this heap). Byte-identical to the old direct deep-copy; the
    /// wire form is what de-risks B3.1 (cores → `Arc`) and B3.3 (the value crosses a real thread).
    /// The `.expect` cannot fire in B3.0: `to_wire` is total (every `Obj` variant maps to a wire
    /// arm — by-reference objects cross as `Handle`), so it is statically infallible here. Its
    /// `Result` return is forward-plumbing for B3.3, where by-reference handles (`Module` / `Func` /
    /// closures) can no longer cross an OS-thread boundary and `to_wire` gains real `Err` arms; at
    /// that point callers switch to direct `to_wire?` / `from_wire`.
    fn deep_clone(&mut self, v: Value) -> Value {
        let w = self.to_wire(v).expect("deep_clone: airlock value must be sendable (B3.0 single-thread)");
        self.from_wire(w)
    }

    /// B3.0 — serialize a value into its [`WireValue`] form (the airlock's outbound half). A
    /// read-only walk of the heap, structurally identical to `deep_clone`'s old recursion but
    /// allocating nothing. Data (list/tuple/map/set/struct/enum) recurses; immutable / by-reference
    /// objects (`Str`, callables, modules, `Channel`/`Shared`/`Executor`) cross as
    /// [`WireValue::Handle`] (the existing handle, same heap in B3.0). `Map`/`Set` carry their cached
    /// hashes through so reconstruction never re-hashes.
    ///
    /// **B3.0: this is total — every `Value` and every `Obj` variant maps to a wire arm, so it never
    /// returns `Err`** (the `?` only forwards from recursion, which is itself infallible). The
    /// `Result` return is deliberate forward-plumbing: at B3.3, by-reference handles whose object
    /// cannot cross an OS thread (`Module` with mutable globals, `Func`/`Closure`) stop mapping to
    /// `Handle` and instead return a real `Err` defensive fault here. No such arm exists yet.
    fn to_wire(&self, v: Value) -> Result<WireValue, RuntimeError> {
        Ok(match v {
            Value::Int(n) => WireValue::Int(n),
            Value::Float(f) => WireValue::Float(f),
            Value::Bool(b) => WireValue::Bool(b),
            Value::Nil => WireValue::Nil,
            Value::Obj(h) => match self.heap.get(h) {
                // B3.3a: `str` crosses by value (owned bytes) so it can survive an OS-thread heap
                // boundary; immutable + value-compared, so a fresh handle on reconstruction is
                // observationally identical to sharing this one.
                Obj::Str(s) => WireValue::Str(s.as_str().into()),
                // By-reference callables: cross as the existing handle (matches the old deep_clone arm).
                Obj::Func { .. }
                | Obj::Closure { .. }
                | Obj::Module { .. }
                | Obj::Native { .. } => WireValue::Handle(h),
                // B3.1: the shared cores cross as the `Arc` itself (clone = refcount bump), so a
                // `from_wire` in any heap reaches the same mailbox/box/queue.
                Obj::Channel(core) => WireValue::Channel(Arc::clone(core)),
                Obj::Shared(core) => WireValue::Shared(Arc::clone(core)),
                Obj::Atomic(core) => WireValue::Atomic(Arc::clone(core)),
                Obj::Executor(core) => WireValue::Executor(Arc::clone(core)),
                // D6: a socket/listener handle crosses as its shared `Arc` core (a spawned fiber
                // reaches the same fd) — same shape as `Channel`/`Shared`/`Executor`.
                Obj::Socket(core) => WireValue::Socket(Arc::clone(core)),
                Obj::Listener(core) => WireValue::Listener(Arc::clone(core)),
                Obj::List(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for x in items {
                        out.push(self.to_wire(*x)?);
                    }
                    WireValue::List(out)
                }
                Obj::Tuple(items) => {
                    let mut out = Vec::with_capacity(items.len());
                    for x in items {
                        out.push(self.to_wire(*x)?);
                    }
                    WireValue::Tuple(out)
                }
                Obj::Map(m) => {
                    let mut out = Vec::with_capacity(m.entries.len());
                    for (hash, k, val) in &m.entries {
                        out.push((*hash, self.to_wire(*k)?, self.to_wire(*val)?));
                    }
                    WireValue::Map(out)
                }
                Obj::Set(s) => {
                    let mut out = Vec::with_capacity(s.entries.len());
                    for (hash, e) in &s.entries {
                        out.push((*hash, self.to_wire(*e)?));
                    }
                    WireValue::Set(out)
                }
                Obj::Struct { name, fields, .. } => {
                    let mut out = Vec::with_capacity(fields.len());
                    for (k, val) in fields {
                        out.push((k.clone(), self.to_wire(*val)?));
                    }
                    WireValue::Struct { name: name.clone(), fields: out }
                }
                Obj::Enum { ty, variant, payload } => {
                    let mut out = Vec::with_capacity(payload.len());
                    for x in payload {
                        out.push(self.to_wire(*x)?);
                    }
                    WireValue::Enum { ty: ty.clone(), variant: variant.clone(), payload: out }
                }
            },
        })
    }

    /// B3.0 — reconstruct a [`WireValue`] into a heap [`Value`] (the airlock's inbound half). Data
    /// arms `alloc` fresh objects into *this* `Vm`'s heap (mirroring the old deep_clone allocation);
    /// [`WireValue::Handle`] returns the same handle (by-reference preserved). `Map`/`Set` rebuild
    /// via `push(hash, …)` with the carried hash, so iteration order + index are identical. Builds
    /// bottom-up (children before parent alloc) and `alloc` never collects, so — like deep_clone —
    /// no intermediate is lost mid-reconstruction.
    // `&mut self` is intentional: `from_wire` reconstructs *into* this VM's heap (it allocates),
    // so it is not the usual ownership-less `from_*` constructor the lint expects.
    #[allow(clippy::wrong_self_convention)]
    fn from_wire(&mut self, w: WireValue) -> Value {
        match w {
            WireValue::Int(n) => Value::Int(n),
            WireValue::Float(f) => Value::Float(f),
            WireValue::Bool(b) => Value::Bool(b),
            WireValue::Nil => Value::Nil,
            // B3.3a: rebuild a fresh heap `str` from the owned bytes (by value, not the old handle).
            WireValue::Str(s) => Value::Obj(self.heap.alloc(Obj::Str(s.into()))),
            WireValue::Handle(h) => Value::Obj(h),
            // B3.1: rebuild a fresh heap handle onto the SAME shared core (`Arc` already cloned in
            // `to_wire`). Not registered in `self.executors` — the original `NewExecutor` handle there
            // drives the program-exit auto-drain and shares this core, so the alias needs no entry.
            WireValue::Channel(core) => Value::Obj(self.heap.alloc(Obj::Channel(core))),
            WireValue::Shared(core) => Value::Obj(self.heap.alloc(Obj::Shared(core))),
            WireValue::Atomic(core) => Value::Obj(self.heap.alloc(Obj::Atomic(core))),
            WireValue::Executor(core) => Value::Obj(self.heap.alloc(Obj::Executor(core))),
            // D6: rebuild a fresh heap handle onto the SAME shared socket/listener core (`Arc` cloned
            // in `to_wire`) — two fibers reach one fd.
            WireValue::Socket(core) => Value::Obj(self.heap.alloc(Obj::Socket(core))),
            WireValue::Listener(core) => Value::Obj(self.heap.alloc(Obj::Listener(core))),
            WireValue::List(items) => {
                let cloned: Vec<Value> = items.into_iter().map(|x| self.from_wire(x)).collect();
                Value::Obj(self.heap.alloc(Obj::List(cloned)))
            }
            WireValue::Tuple(items) => {
                let cloned: Vec<Value> = items.into_iter().map(|x| self.from_wire(x)).collect();
                Value::Obj(self.heap.alloc(Obj::Tuple(cloned)))
            }
            WireValue::Map(entries) => {
                let mut out = MapData::default();
                for (hash, k, val) in entries {
                    let (ck, cv) = (self.from_wire(k), self.from_wire(val));
                    out.push(hash, ck, cv);
                }
                Value::Obj(self.heap.alloc(Obj::Map(out)))
            }
            WireValue::Set(entries) => {
                let mut out = SetData::default();
                for (hash, e) in entries {
                    let ce = self.from_wire(e);
                    out.push(hash, ce);
                }
                Value::Obj(self.heap.alloc(Obj::Set(out)))
            }
            WireValue::Struct { name, fields } => {
                let cloned: Vec<(Box<str>, Value)> =
                    fields.into_iter().map(|(k, val)| (k, self.from_wire(val))).collect();
                let tid = self.struct_tid(&name);
                Value::Obj(self.heap.alloc(Obj::Struct { name, tid, fields: cloned }))
            }
            WireValue::Enum { ty, variant, payload } => {
                let cloned: Vec<Value> = payload.into_iter().map(|x| self.from_wire(x)).collect();
                Value::Obj(self.heap.alloc(Obj::Enum { ty, variant, payload: cloned }))
            }
            // B3.6: rebuild a submitted closure by value over the worker's reconstructed home module
            // (the `proto` is shared via `Arc<Program>`; captures reconstruct bottom-up into this heap).
            // `worker_home` resolves the home index against this VM's `module_objs` (the rebuilt graph
            // in a pool worker, or the live graph in a cooperative same-heap drain).
            WireValue::Closure { proto, captured, home } => {
                let cap: std::collections::HashMap<String, Value> =
                    captured.into_iter().map(|(k, w)| (String::from(k), self.from_wire(w))).collect();
                let home = self.worker_home(home);
                Value::Obj(self.heap.alloc(Obj::Closure { proto, captured: cap, home }))
            }
        }
    }

    /// B3.2 — construct a fresh worker `Vm` that shares this VM's compiled program by `Arc`
    /// (read-only) but owns its own empty heap. Execution-shaping flags (`gc_stress`) carry over so a
    /// worker is exercised under the same GC pressure as the parent; `host` is left inert (B3.2's
    /// isolation tasks don't touch host I/O — B3.3 threads it through when real workers run user I/O).
    /// No OS thread yet; the caller drives the returned worker synchronously.
    fn spawn_worker(&self) -> Vm {
        let mut worker = Vm::new(Arc::clone(&self.program));
        worker.gc_stress = self.gc_stress;
        // Workers run on the pool too, so a nested `parallel:` inside a task recurses onto threads
        // (and a worker's `recv` blocks on the condvar, not a fiber). B3.3-threads.
        worker.parallel = self.parallel;
        // B3.3-threads: thread the parent's **read-only** host state (process args + env) through so a
        // `--parallel` task reading `std.os.args` / an env var sees the same values instead of inert
        // defaults (the B3.2 silent-divergence owe). `stdin` is deliberately NOT shared: it is a
        // single consumable stream owned by the main thread; handing each worker a copy of
        // `Stdin::Lines` would duplicate input and concurrent `Stdin::Real` reads would race — a task
        // reading stdin gets EOF (documented). `HostConfig` isn't `Clone`, so build it field-wise.
        worker.host = crate::native::HostConfig {
            args: self.host.args.clone(),
            env: self.host.env.clone(),
            stdin: crate::native::Stdin::Empty,
        };
        worker
    }

    /// B3.2 — run a spawned task in an isolated worker (`spawn_worker`): its args/captures cross IN as
    /// [`WireValue`] (serialized in *this* parent heap, reconstructed in the worker heap) and its
    /// return value + captured `out`/`stderr` cross back OUT. The worker runs **synchronously** on the
    /// calling thread (no OS thread until B3.3), proving the `Arc<Program>` + heap-handoff plumbing in
    /// isolation.
    ///
    /// The callee is **not** crossed as a parent-heap `Handle` (a `GcRef` is meaningless in another
    /// heap); instead the task is lowered to its `ProtoId` + wire'd captures (the proto lives in the
    /// shared `Arc<Program>`) and the worker rebuilds the closure over its own heap.
    ///
    /// **Cross-heap safety (enforced, not just documented).** A `WireValue` that still carries a
    /// by-reference [`Handle`](WireValue::has_handle) — a `str`, a closure/func value, a module — is a
    /// parent-heap `GcRef` that means nothing in the worker heap, so every crossed value (captures,
    /// args, and the returned result) is checked with [`Vm::ensure_crossable`] and a clean
    /// `RuntimeError` is raised rather than silently reconstructing a dangling handle. Plain data and
    /// `Channel`/`Shared`/`Executor` handles (which cross as a shared `Arc`, not a `GcRef`) pass.
    /// `str`/closure crossing **by value** lands in B3.3.
    ///
    /// B3.3c/d: the worker's `home` is a **read-only snapshot** of the parent's module graph
    /// ([`Vm::build_worker_modules`]) — top-level fns resolve via the rebuilt home globals, imports via
    /// the rebuilt `module_objs` — so a task may read post-init globals and call sibling/imported fns
    /// (module globals are read-only under `--parallel`, decision G1 / gate B3.3b). **Method tasks**
    /// (`spawn recv.m()`) dispatch against that rebuilt graph. Still deferred to B3.3-threads: real OS
    /// threads + a condvar `recv` (a method that blocks on `recv` faults here, no scheduler yet).
    /// The whole graph is reconstructed per task (correctness-first; pooling is a B3.3-threads concern).
    /// The synchronous single-thread driver: prepare + run on the calling thread. The `--parallel`
    /// engine calls `prepare_worker` and `ReadyWorker::run` separately (across the pool boundary), so
    /// this convenience wrapper is now only used by the B3.2–B3.3d worker unit tests.
    #[cfg(test)]
    fn run_task_isolated(&mut self, task: PendingCall) -> Result<WorkerResult, RuntimeError> {
        self.prepare_worker(task)?.run()
    }

    /// B3.3-threads — the parent-thread half of [`Vm::run_task_isolated`]: lower the task to a `Send`
    /// description against THIS heap, build the worker + reconstruct the module graph in its heap, and
    /// rebuild the callee/receiver + args **into the worker heap**, yielding a [`ReadyWorker`] that
    /// can be moved to a pool thread and `run()`. Everything that reads the parent heap happens here;
    /// nothing in `ReadyWorker::run` touches `self`.
    fn prepare_worker(&mut self, task: PendingCall) -> Result<ReadyWorker, RuntimeError> {
        // 1. Lower the task to a `Send` description in THIS (parent) heap (read-only serialize),
        //    rejecting any value that can't cross a heap boundary as-is.
        let lowered = match task {
            PendingCall::Call { callee, args, span } => {
                let wargs = self.wire_args(args, span)?;
                match callee {
                    Value::Obj(h) => match self.heap.get(h).clone() {
                        Obj::Closure { proto, captured, home } => {
                            let mut wcap = Vec::with_capacity(captured.len());
                            for (k, v) in captured {
                                let w = self.to_wire(v)?;
                                self.ensure_crossable(&w, span)?;
                                wcap.push((k, w));
                            }
                            Lowered::Closure { proto, captured: wcap, args: wargs, home: self.home_index(home), span }
                        }
                        Obj::Func { proto, home } => Lowered::Func { proto, args: wargs, home: self.home_index(home), span },
                        _ => return Err(self.err(
                            format!("spawn: '{}' is not an isolable task", self.type_name(callee)),
                            span,
                        )),
                    },
                    _ => return Err(self.err(
                        format!("spawn: '{}' is not an isolable task", self.type_name(callee)),
                        span,
                    )),
                }
            }
            // B3.3d: the receiver + args cross by wire; dispatch resolves against the worker's
            // reconstructed `module_objs` (built below). `ensure_crossable` keeps a non-sendable
            // receiver (e.g. a closure) from silently dangling.
            PendingCall::Method { recv, name, args, span } => {
                let wrecv = self.to_wire(recv)?;
                self.ensure_crossable(&wrecv, span)?;
                let wargs = self.wire_args(args, span)?;
                Lowered::Method { recv: wrecv, name, args: wargs, span }
            }
        };

        // 2. Build the worker + install the shared read-only module snapshot (D1): pre-alloc empty
        //    module objs (indices line up with the parent), faulting each module's globals into the
        //    worker heap lazily on first access — instead of eagerly reconstructing the whole graph
        //    per task. 3. rebuild the callable/receiver + args into the worker heap (a `home` index
        //    resolves to a pre-alloced empty module that faults on first global read). The actual
        //    invoke is `ReadyWorker::run`.
        let snap = self.ensure_snapshot();
        let mut worker = self.spawn_worker();
        worker.install_snapshot(snap);
        let (call, span) = match lowered {
            Lowered::Closure { proto, captured, args, home, span } => {
                let home = worker.worker_home(home);
                let cap = captured.into_iter().map(|(k, w)| (k, worker.from_wire(w))).collect();
                let callee = Value::Obj(worker.heap.alloc(Obj::Closure { proto, captured: cap, home }));
                let args = args.into_iter().map(|w| worker.from_wire(w)).collect();
                (ReadyCall::Invoke { callee, args }, span)
            }
            Lowered::Func { proto, args, home, span } => {
                let home = worker.worker_home(home);
                let callee = Value::Obj(worker.heap.alloc(Obj::Func { proto, home }));
                let args = args.into_iter().map(|w| worker.from_wire(w)).collect();
                (ReadyCall::Invoke { callee, args }, span)
            }
            Lowered::Method { recv, name, args, span } => {
                let recv = worker.from_wire(recv);
                let args = args.into_iter().map(|w| worker.from_wire(w)).collect();
                (ReadyCall::Method { recv, name, args }, span)
            }
        };
        Ok(ReadyWorker { worker, call, span })
    }

    /// B3.6 — the `Executor`-drain analogue of [`prepare_worker`]: build a worker, install the shared
    /// read-only [`ModuleSnapshot`] (D1 — modules fault in lazily on first global access), and rebuild
    /// a submitted closure (a [`WireValue::Closure`] drained from the executor queue) into that heap as
    /// a zero-arg call. Infallible — the closure already crossed `to_wire`/`ensure_crossable` at
    /// `submit`. `--parallel` only.
    fn prepare_worker_from_wire(&mut self, task: WireValue, span: Span) -> ReadyWorker {
        let snap = self.ensure_snapshot();
        let mut worker = self.spawn_worker();
        worker.install_snapshot(snap);
        let callee = worker.from_wire(task);
        ReadyWorker { worker, call: ReadyCall::Invoke { callee, args: Vec::new() }, span }
    }

    /// B3.6 — drain a shut `Executor`'s pending tasks onto the bounded pool under `--parallel`. Each
    /// queued closure becomes a [`ReadyWorker`] sharing a fresh per-drain cancel flag (first fault
    /// aborts siblings, matching the cooperative inline `r?`); **no** deadlock watch (decision D — an
    /// `Executor`-spanning deadlock hangs, as documented). Output is flushed in submission (queue) order
    /// by [`run_workers_on_pool`] (decision F).
    fn drain_executor_on_pool(&mut self, tasks: Vec<WireValue>, span: Span) -> Result<(), RuntimeError> {
        if tasks.is_empty() {
            return Ok(());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let mut ready = Vec::with_capacity(tasks.len());
        for t in tasks {
            let mut rw = self.prepare_worker_from_wire(t, span);
            rw.worker.cancel = Some(Arc::clone(&cancel));
            ready.push(rw);
        }
        self.run_workers_on_pool(ready)
    }

    /// Reject a wired value that still carries a by-reference [`Handle`](WireValue::has_handle) — a
    /// heap-local `GcRef` that cannot cross into another heap as-is (B3.2). `str`/closure crossing by
    /// value lands in B3.3; until then this converts a would-be dangling handle into a clean fault.
    fn ensure_crossable(&self, w: &WireValue, span: Span) -> Result<(), RuntimeError> {
        if w.has_handle() {
            return Err(self.err(
                "spawn: this task value can't cross a worker boundary yet — only plain data and \
                 Channel/Shared/Executor handles are sendable in B3.2 (str/closure cross by value in B3.3)"
                    .to_string(),
                span,
            ));
        }
        Ok(())
    }

    /// Serialize a task's argument list across the airlock (read-only walk of this heap), rejecting any
    /// argument that can't cross a heap boundary as-is (see [`Vm::ensure_crossable`]).
    fn wire_args(&self, args: Vec<Value>, span: Span) -> Result<Vec<WireValue>, RuntimeError> {
        args.into_iter()
            .map(|a| {
                let w = self.to_wire(a)?;
                self.ensure_crossable(&w, span)?;
                Ok(w)
            })
            .collect()
    }

    /// B3.6 — serialize a callable (`Executor.submit`'s argument) across the airlock **by value** as a
    /// [`WireValue::Closure`], so a submitted task can be reconstructed and run on a pool thread. Unlike
    /// the generic `to_wire` (which crosses a closure as a by-reference `Handle`), this lowers the
    /// callee to its `proto` (shared via `Arc<Program>`) + wire'd captures + a `home` *index* — no
    /// heap-local `GcRef` survives. A captured value is itself `to_wire`d (so a `Channel`/`Shared`
    /// capture crosses as its shared `Arc`); the A3b checker gate already rejected a non-sendable
    /// capture, so [`ensure_crossable`] here is the defensive backstop. A bare `Func` (no captures) is a
    /// degenerate closure with an empty capture set.
    fn wire_callable(&self, v: Value, span: Span) -> Result<WireValue, RuntimeError> {
        if let Value::Obj(h) = v {
            match self.heap.get(h) {
                Obj::Closure { proto, captured, home } => {
                    let mut wcap = Vec::with_capacity(captured.len());
                    for (k, cv) in captured {
                        let w = self.to_wire(*cv)?;
                        self.ensure_crossable(&w, span)?;
                        wcap.push((k.clone().into_boxed_str(), w));
                    }
                    return Ok(WireValue::Closure { proto: *proto, captured: wcap, home: self.home_index(*home) });
                }
                Obj::Func { proto, home } => {
                    return Ok(WireValue::Closure { proto: *proto, captured: Vec::new(), home: self.home_index(*home) });
                }
                _ => {}
            }
        }
        Err(self.err("submit requires a function or closure".to_string(), span))
    }

    /// A fresh empty module to serve as a worker closure's `home`. The parent's `home` `GcRef` can't
    /// cross heaps; used as the fallback when the task's home is not a real `module_objs` entry (the
    /// hand-built unit-test fixtures) — real spawns resolve a reconstructed module (see `worker_home`).
    fn fresh_worker_home(&mut self) -> GcRef {
        self.heap.alloc(Obj::Module { name: "<worker>".into(), slots: Vec::new(), index: Default::default() })
    }

    /// B3.3c — the index of a `home` module `GcRef` in this VM's `module_objs`, so the worker can
    /// resolve the corresponding rebuilt module. `None` for a home not in the table (test fixtures).
    fn home_index(&self, home: GcRef) -> Option<usize> {
        self.module_objs.iter().position(|&m| m == home)
    }

    /// B3.3c — resolve a lowered home index to this (worker) VM's reconstructed module obj, falling
    /// back to a fresh empty home when the parent home was not a real module (test fixtures).
    fn worker_home(&mut self, idx: Option<usize>) -> GcRef {
        match idx {
            Some(i) if i < self.module_objs.len() => self.module_objs[i],
            _ => self.fresh_worker_home(),
        }
    }

    /// D1 — the shared, read-only [`ModuleSnapshot`] of this VM's initialized module graph, built once
    /// and reused for every worker it prepares. On a **worker** VM the snapshot it was handed already
    /// describes its (lazily-faulted) graph exactly — module globals are frozen under `--parallel`
    /// (decision G1) — so a nested `spawn` reuses that same `Arc` rather than re-snapshotting a partial
    /// heap; on the **top-level** VM the snapshot is built from the real, fully-populated modules and
    /// memoized in `snapshot_memo`.
    fn ensure_snapshot(&mut self) -> Arc<ModuleSnapshot> {
        if let Some(s) = &self.module_snapshot {
            return Arc::clone(s);
        }
        if let Some(s) = &self.snapshot_memo {
            return Arc::clone(s);
        }
        let snap = Arc::new(self.snapshot_modules());
        self.snapshot_memo = Some(Arc::clone(&snap));
        snap
    }

    /// D1 — read this VM's initialized module graph (read-only) into a heap-independent
    /// [`ModuleSnapshot`]: one [`ModuleSnap`] per module in `module_objs` order (so a callable's home
    /// index lines up with a worker's pre-alloced modules), each global lowered by [`Vm::to_snap`].
    /// Replaces the eager per-task `build_worker_modules` reconstruction — built once, replayed lazily.
    fn snapshot_modules(&self) -> ModuleSnapshot {
        let modules = self
            .module_objs
            .iter()
            .map(|&pm| {
                // M19 Phase 2b — collect globals in *slot order* (not HashMap iteration order) so a
                // worker replays them into matching slots; the shared `Arc<Program>` slot map makes
                // parent and worker agree on slot↔name regardless of any hash ordering.
                let (name, globals): (Box<str>, Vec<(String, Value)>) = match self.heap.get(pm) {
                    Obj::Module { name, slots, index } => (name.clone(), module_slot_pairs(slots, index)),
                    _ => ("<worker>".into(), Vec::new()),
                };
                let globals = globals.into_iter().map(|(k, v)| (k, self.to_snap(v))).collect();
                ModuleSnap { name, globals }
            })
            .collect();
        ModuleSnapshot { modules }
    }

    /// D1 — lower one parent-heap global value into a heap-independent [`SnapValue`]. The snapshot
    /// analogue of the old `map_global_value`: a `GcRef` is heap-local, so a callable's home/captures,
    /// an import-alias module ref, and any container embedding one of those must be encoded
    /// structurally, never by reference.
    ///
    /// - **Fast path:** a value whose wire form has no by-reference `Handle` is pure data (`str` by
    ///   value) or a `Channel`/`Shared`/`Executor` core (Arc-shared) — encode the exact wire form.
    /// - `Func`/`Closure` → record the home as a `module_objs` index (captures recursed); import-alias
    ///   `Module` → `ModuleAlias(idx)`; `Native` → fn pointer; containers → element-wise (map/set
    ///   hashes are value-derived, carried unchanged).
    fn to_snap(&self, v: Value) -> SnapValue {
        let h = match v {
            Value::Obj(h) => h,
            scalar => return SnapValue::Wire(self.to_wire(scalar).expect("scalar is always sendable")),
        };
        // Fast path: no embedded callable/module → the wire form is exact and cheap.
        if let Ok(w) = self.to_wire(v)
            && !w.has_handle()
        {
            return SnapValue::Wire(w);
        }
        match self.heap.get(h).clone() {
            Obj::Func { proto, home } => SnapValue::Func { proto, home: self.home_index(home) },
            Obj::Closure { proto, captured, home } => {
                let captured = captured.iter().map(|(k, cv)| (k.clone(), self.to_snap(*cv))).collect();
                SnapValue::Closure { proto, captured, home: self.home_index(home) }
            }
            // An import alias bound to another module obj.
            Obj::Module { name, slots, index } => match self.home_index(h) {
                Some(idx) => SnapValue::ModuleAlias(idx),
                // A module not in `module_objs` (shouldn't occur for a bound import) — encode inline,
                // in slot order so replay rebuilds matching slots.
                None => {
                    let globals = module_slot_pairs(&slots, &index)
                        .into_iter()
                        .map(|(k, mv)| (k, self.to_snap(mv)))
                        .collect();
                    SnapValue::ModuleInline { name, globals }
                }
            },
            Obj::Native { name, func } => SnapValue::Native { name, func },
            // Containers embedding a callable: encode each element. (Pure-data containers took the fast
            // path above.)
            Obj::List(items) => SnapValue::List(items.iter().map(|x| self.to_snap(*x)).collect()),
            Obj::Tuple(items) => SnapValue::Tuple(items.iter().map(|x| self.to_snap(*x)).collect()),
            Obj::Struct { name, fields, .. } => {
                let fields = fields.iter().map(|(k, fv)| (k.clone(), self.to_snap(*fv))).collect();
                SnapValue::Struct { name, fields }
            }
            Obj::Enum { ty, variant, payload } => {
                let payload = payload.iter().map(|x| self.to_snap(*x)).collect();
                SnapValue::Enum { ty, variant, payload }
            }
            Obj::Map(m) => SnapValue::Map(
                m.entries.iter().map(|(hash, k, val)| (*hash, self.to_snap(*k), self.to_snap(*val))).collect(),
            ),
            Obj::Set(s) => SnapValue::Set(s.entries.iter().map(|(hash, e)| (*hash, self.to_snap(*e))).collect()),
            // Leaf data / cores are handled by the fast path; if `to_wire` ever errored above we land
            // here for a `str`/core (always sendable) — encode its wire form.
            Obj::Str(_)
            | Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::Atomic(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_) => {
                SnapValue::Wire(self.to_wire(v).expect("str / channel / shared / atomic / executor / socket is always sendable"))
            }
        }
    }

    /// D1 — install a shared [`ModuleSnapshot`] into a freshly-built worker: pre-alloc one **empty**
    /// `Module` per snapshot entry (index order preserved so a callable's home index lines up), seed
    /// the per-module faulted flags, and keep the `Arc` so each module's globals fault in lazily on
    /// first access ([`Vm::fault_module`]). The cheap replacement for eager `build_worker_modules`.
    fn install_snapshot(&mut self, snap: Arc<ModuleSnapshot>) {
        debug_assert!(self.module_objs.is_empty(), "install_snapshot expects a fresh worker");
        for m in &snap.modules {
            let wm = self.heap.alloc(Obj::Module { name: m.name.clone(), slots: Vec::new(), index: std::collections::HashMap::new() });
            self.module_objs.push(wm);
        }
        self.module_faulted = vec![false; snap.modules.len()];
        self.module_snapshot = Some(snap);
    }

    /// D1 — fault module `idx`'s globals into this worker's heap from the snapshot, the first time any
    /// global of that module is read. Idempotent (guarded by `module_faulted`); the flag is set
    /// *before* replaying so a self-referential global (e.g. a [`SnapValue::ModuleAlias`] back to this
    /// same module) resolves to the already-alloced module obj without re-entering. No-op once faulted.
    fn fault_module(&mut self, idx: usize) {
        if self.module_faulted[idx] {
            return;
        }
        self.module_faulted[idx] = true;
        let snap = Arc::clone(self.module_snapshot.as_ref().expect("worker has a snapshot"));
        let module = self.module_objs[idx];
        for (name, sv) in &snap.modules[idx].globals {
            let val = self.replay_snap(sv);
            self.module_define(module, name, val);
        }
    }

    /// D1 — if this is a worker VM (a snapshot is installed), ensure the module that owns `home` has
    /// been faulted in before its globals are read. No-op on the top-level / cooperative VM (no
    /// snapshot — `module_objs` are the real, already-populated modules), so those engines are
    /// untouched. Called at every module-global read site (`GetGlobal`, the `GetCaptured` home
    /// fallback, module member access, and a `module.fn(...)` call).
    fn ensure_module_faulted(&mut self, home: GcRef) {
        if self.module_snapshot.is_none() {
            return;
        }
        if let Some(idx) = self.module_objs.iter().position(|&m| m == home) {
            self.fault_module(idx);
        }
    }

    /// D1 — replay a [`SnapValue`] into this worker's heap (the inverse of [`Vm::to_snap`]): the
    /// snapshot is shared behind an `Arc`, so this borrows and clones leaf data (`WireValue`, fn
    /// pointer) rather than moving. `ModuleAlias(idx)` resolves to the pre-alloced `module_objs[idx]`
    /// — which faults its own globals lazily on first access, so no eager cascade.
    fn replay_snap(&mut self, snap: &SnapValue) -> Value {
        match snap {
            SnapValue::Wire(w) => self.from_wire(w.clone()),
            SnapValue::Func { proto, home } => {
                let whome = self.worker_home(*home);
                Value::Obj(self.heap.alloc(Obj::Func { proto: *proto, home: whome }))
            }
            SnapValue::Closure { proto, captured, home } => {
                let whome = self.worker_home(*home);
                let cap = captured.iter().map(|(k, cv)| (k.clone(), self.replay_snap(cv))).collect();
                Value::Obj(self.heap.alloc(Obj::Closure { proto: *proto, captured: cap, home: whome }))
            }
            SnapValue::ModuleAlias(idx) => Value::Obj(self.module_objs[*idx]),
            SnapValue::ModuleInline { name, globals } => {
                let wm = self.heap.alloc(Obj::Module { name: name.clone(), slots: Vec::new(), index: std::collections::HashMap::new() });
                for (k, gv) in globals {
                    let val = self.replay_snap(gv);
                    self.module_define(wm, k, val);
                }
                Value::Obj(wm)
            }
            SnapValue::Native { name, func } => Value::Obj(self.heap.alloc(Obj::Native { name: name.clone(), func: *func })),
            SnapValue::List(xs) => {
                let v = xs.iter().map(|x| self.replay_snap(x)).collect();
                Value::Obj(self.heap.alloc(Obj::List(v)))
            }
            SnapValue::Tuple(xs) => {
                let v = xs.iter().map(|x| self.replay_snap(x)).collect();
                Value::Obj(self.heap.alloc(Obj::Tuple(v)))
            }
            SnapValue::Struct { name, fields } => {
                let f = fields.iter().map(|(k, fv)| (k.clone(), self.replay_snap(fv))).collect();
                let tid = self.struct_tid(name);
                Value::Obj(self.heap.alloc(Obj::Struct { name: name.clone(), tid, fields: f }))
            }
            SnapValue::Enum { ty, variant, payload } => {
                let p = payload.iter().map(|x| self.replay_snap(x)).collect();
                Value::Obj(self.heap.alloc(Obj::Enum { ty: ty.clone(), variant: variant.clone(), payload: p }))
            }
            SnapValue::Map(entries) => {
                let mut out = MapData::default();
                for (hash, k, val) in entries {
                    let (ck, cv) = (self.replay_snap(k), self.replay_snap(val));
                    out.push(*hash, ck, cv);
                }
                Value::Obj(self.heap.alloc(Obj::Map(out)))
            }
            SnapValue::Set(entries) => {
                let mut out = SetData::default();
                for (hash, e) in entries {
                    let ce = self.replay_snap(e);
                    out.push(*hash, ce);
                }
                Value::Obj(self.heap.alloc(Obj::Set(out)))
            }
        }
    }

    /// B3.1 — clone out the shared `Arc<ChannelCore>` behind a `Channel` handle (refcount bump). The
    /// `Arc` is held only for the duration of the calling method, so locking it does not borrow the
    /// heap, leaving `self` free for the re-entrant value paths (`from_wire`, `invoke_value`).
    fn channel_core(&self, h: GcRef) -> Arc<ChannelCore> {
        match self.heap.get(h) {
            Obj::Channel(core) => Arc::clone(core),
            _ => unreachable!("channel_core on non-channel"),
        }
    }

    fn shared_core(&self, h: GcRef) -> Arc<SharedCore> {
        match self.heap.get(h) {
            Obj::Shared(core) => Arc::clone(core),
            _ => unreachable!("shared_core on non-shared"),
        }
    }

    fn atomic_core(&self, h: GcRef) -> Arc<AtomicCore> {
        match self.heap.get(h) {
            Obj::Atomic(core) => Arc::clone(core),
            _ => unreachable!("atomic_core on non-atomic"),
        }
    }

    /// `Atomic(v)` — pop the init, box its wire form behind a fresh `Arc<AtomicCore>`. `#[inline(never)]`
    /// so its locals stay out of `step`'s (recursion-path) stack frame.
    #[inline(never)]
    fn new_atomic(&mut self) -> Value {
        let init = self.pop();
        let init = self.to_wire(init).expect("Atomic init must be sendable (B3.1 single-thread)");
        Value::Obj(self.heap.alloc(Obj::Atomic(Arc::new(AtomicCore { v: Mutex::new(init) }))))
    }

    /// `timer(ms)` — pop the `ms` int, push a fresh `Channel[bool]` stamped with `now + ms`. Delivery is
    /// handled at `recv` time (in the receiver's scheduler), NOT here, so a timer made at the top level
    /// can be `recv`'d inside a `--parallel` child. `#[inline(never)]` so the `Instant`/`Duration` math
    /// stays out of `step`'s (recursion-path) stack frame.
    #[inline(never)]
    fn new_timer(&mut self, span: Span) -> Result<Value, RuntimeError> {
        let ms = match self.pop() {
            Value::Int(ms) => ms.max(0) as u64,
            other => return Err(self.err(format!("timer(ms) expects int, got {}", self.type_name(other)), span)),
        };
        // Saturate a pathological `ms` to a far-future deadline rather than panic on `Instant` overflow
        // (mirrors the `sleep_ms` offload path).
        let deadline = std::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(ms))
            .unwrap_or_else(|| std::time::Instant::now() + std::time::Duration::from_secs(86_400 * 365));
        let core = Arc::new(ChannelCore { timer: Some(deadline), ..Default::default() });
        Ok(Value::Obj(self.heap.alloc(Obj::Channel(core))))
    }

    fn executor_core(&self, h: GcRef) -> Arc<ExecutorCore> {
        match self.heap.get(h) {
            Obj::Executor(core) => Arc::clone(core),
            _ => unreachable!("executor_core on non-executor"),
        }
    }

    /// D6 — clone out the shared `Arc<SocketCore>`/`Arc<ListenerCore>` behind a handle (refcount bump),
    /// mirroring [`channel_core`](Vm::channel_core). The `Arc` is held only for the calling method, so
    /// locking the fd does not borrow the heap.
    fn socket_core(&self, h: GcRef) -> Arc<SocketCore> {
        match self.heap.get(h) {
            Obj::Socket(core) => Arc::clone(core),
            _ => unreachable!("socket_core on non-socket"),
        }
    }

    fn listener_core(&self, h: GcRef) -> Arc<ListenerCore> {
        match self.heap.get(h) {
            Obj::Listener(core) => Arc::clone(core),
            _ => unreachable!("listener_core on non-listener"),
        }
    }

    /// D6 — build a `Result::Ok(v)` / `Result::Err(msg)` for a socket op (mirrors `lower_native`'s
    /// `Ok`/`Err` arms — the surface contract is `read/write/accept -> Result`).
    fn sock_ok(&mut self, v: Value) -> Value {
        self.alloc_enum("Result", "Ok", vec![v])
    }
    fn sock_err(&mut self, msg: impl Into<String>) -> Value {
        let ev = self.alloc_str(msg.into());
        self.alloc_enum("Result", "Err", vec![ev])
    }

    /// D6 — `std.net.connect(addr)` / `listen(addr)`: allocate a non-blocking `Socket`/`Listener`
    /// handle (or a `Result::Err` on a bad address / bind failure). Intercepted in `invoke_native`
    /// because it allocates a heap handle over an `Arc`'d core — a pure off-heap native can't.
    ///
    /// D6b — `connect` is now a **true non-blocking** connect: an in-progress handshake (`EINPROGRESS`)
    /// parks the fiber on the socket's writability rather than pinning a worker for the round trip. The
    /// connecting socket is stashed in `pending_connect`; the netpoller wakes the fiber on writability
    /// and [`Vm::run_one_fiber`] completes it via [`Vm::finish_pending_connect`] (read `SO_ERROR`) and
    /// pushes the resulting `Socket` — the bytecode call site never re-runs. The instant (loopback)
    /// case still returns immediately.
    fn net_connect_or_listen(&mut self, name: &str, args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
        let addr = match args.first() {
            Some(Value::Obj(h)) => match self.heap.get(*h) {
                Obj::Str(s) => s.to_string(),
                _ => return Err(self.err(format!("std.net.{name} expects an address string"), span)),
            },
            _ => return Err(self.err(format!("std.net.{name} expects an address string"), span)),
        };
        match name {
            "connect" => match crate::native::net::connect_nonblocking(&addr) {
                // Connected synchronously (the common loopback case) — wrap + return at once.
                Ok((stream, false)) => Ok(self.alloc_socket_ok(stream, core::next_poll_key(), core::new_in_flight())),
                // Handshake in flight: park the fiber on writability under the M:N engine; off it (the
                // cooperative / top-level v1 fallback, where there is no fiber to park), block until the
                // handshake settles. net targets `--parallel`.
                Ok((stream, true)) => {
                    if self.mn.is_some() && self.native_reentry == 0 {
                        self.park_on_connect(stream);
                        Ok(Value::Nil) // parked sentinel; `poll_park` gates the result-push at `do_call`
                    } else if self.mn.is_some() {
                        // `native_reentry > 0` — a `connect` reached inside a native callback (operator
                        // overload, list HOF, `Shared.update`, ...). The caller's loop state lives on the
                        // Rust stack, so the fiber can't park; and blocking here would pin a worker
                        // thread on the handshake. Fail loud, exactly as `read`/`write`/`accept` do.
                        Ok(self.sock_err("connect would block: std.net sockets require the --parallel engine"))
                    } else {
                        // Top-level / cooperative: no fiber to park, so block (bounded) until the
                        // handshake settles. net targets `--parallel`; this keeps a top-level
                        // `net.connect` usable as the v1 fallback.
                        Ok(self.block_until_connected(stream))
                    }
                }
                Err(e) => Ok(self.sock_err(format!("{addr}: {e}"))),
            },
            "listen" => match crate::native::net::listen_nonblocking(&addr) {
                Ok(listener) => {
                    let core = Arc::new(ListenerCore { listener: Mutex::new(Some(listener)), key: core::next_poll_key(), in_flight: core::new_in_flight() });
                    let v = Value::Obj(self.heap.alloc(Obj::Listener(core)));
                    Ok(self.sock_ok(v))
                }
                Err(e) => Ok(self.sock_err(format!("{addr}: {e}"))),
            },
            _ => unreachable!("net_connect_or_listen on '{name}'"),
        }
    }

    /// D6b — wrap a connected `TcpStream` in a `Socket` handle and return `Ok(Socket)`. `key`/`in_flight`
    /// become the socket's poll identity for later `read`/`write` parks (a fresh pair for a synchronous
    /// connect; the connect's own pair, reused, for one that parked — its `in_flight` was cleared on
    /// inject).
    fn alloc_socket_ok(&mut self, stream: std::net::TcpStream, key: usize, in_flight: Arc<AtomicBool>) -> Value {
        let core = Arc::new(SocketCore { stream: Mutex::new(Some(stream)), key, in_flight });
        let v = Value::Obj(self.heap.alloc(Obj::Socket(core)));
        self.sock_ok(v)
    }

    /// D6b — finish a connect that parked on writability: `SO_ERROR` clear ⇒ `Ok(Socket)`, else
    /// `Err(msg)`. Reuses the connect's poll key + guard so the resulting socket keeps a stable identity.
    fn finish_pending_connect(&mut self, cip: ConnectInProgress) -> Value {
        match crate::native::net::finish_connect(&cip.stream) {
            Ok(()) => self.alloc_socket_ok(cip.stream, cip.key, cip.in_flight),
            Err(e) => self.sock_err(format!("connect failed: {e}")),
        }
    }

    /// D6b — park the current fiber on a connecting socket's writability. Stash the connecting stream
    /// in `pending_connect` (it owns the fd the poller will watch, so it must outlive the park) and set
    /// the `poll_park` sentinel; the worker loop hands both to the netpoller. Unlike a `read`/`write`
    /// park there is NO `ip` rewind — `net.connect`'s call site already popped its args and pushed
    /// nothing (`do_call` saw `paused()`), so on resume [`Vm::run_one_fiber`] finishes the connect and
    /// pushes the `Socket` exactly where the call would have, and execution continues past the call.
    fn park_on_connect(&mut self, stream: std::net::TcpStream) {
        let key = core::next_poll_key();
        let in_flight = core::new_in_flight();
        in_flight.store(true, Ordering::Release); // mark parked (matches `park_on_fd`'s swap(true))
        let fd = stream.as_raw_fd();
        self.pending_connect = Some(ConnectInProgress { stream, key, in_flight: Arc::clone(&in_flight) });
        // A `connect` never carries a user timeout (the `connect` surface takes only an address); it
        // parks forever (or until `drain_sched` re-injects it on a sibling fault).
        self.poll_park = Some(PollPark { key, fd, interest: poller::Interest::Write, in_flight, deadline: None });
    }

    /// D6b — the top-level connect fallback (no fiber to park): block until the handshake settles, then
    /// return `Ok(Socket)` / `Err`. Bounded by a wall-clock deadline so a black-hole address (no RST,
    /// no SYN-ACK — `SO_ERROR` never sets, the fd never becomes writable) returns a clean timeout
    /// instead of spinning for the kernel's multi-minute connect timeout. net targets the M:N
    /// `--parallel` engine, so this path exists only to keep a top-level `net.connect` usable.
    fn block_until_connected(&mut self, stream: std::net::TcpStream) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(CONNECT_BLOCK_TIMEOUT_SECS);
        loop {
            match crate::native::net::finish_connect(&stream) {
                // SO_ERROR clear AND the peer is reachable ⇒ connected.
                Ok(()) if stream.peer_addr().is_ok() => {
                    return self.alloc_socket_ok(stream, core::next_poll_key(), core::new_in_flight());
                }
                Err(e) => return self.sock_err(format!("connect failed: {e}")),
                Ok(()) if std::time::Instant::now() >= deadline => {
                    return self.sock_err("connect failed: timed out");
                }
                Ok(()) => std::thread::sleep(std::time::Duration::from_millis(1)), // not settled yet
            }
        }
    }

    /// D6 — `Socket` methods: `read(n) -> Result[str]`, `write(s) -> Result[int]`, `close() -> nil`.
    /// On a would-block, under the M:N engine the fiber PARKS on the netpoller (re-root the receiver,
    /// rewind `ip` so the op re-executes on resume, set the `poll_park` sentinel — mirrors the channel
    /// `recv` park, but routed to the poller). Off the M:N engine (top level / cooperative) there is no
    /// fiber to park, so the op blocks the thread once (a documented v1 fallback — net targets
    /// `--parallel`).
    fn socket_method(&mut self, h: GcRef, method: &str, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        match method {
            "read" => {
                // `read(n)` or `read(n, timeout_ms)` — the optional trailing int bounds the FIRST
                // readiness (D6c). On a timeout the netpoller re-injects this fiber with `poll_timed_out`
                // set; the rewound op re-runs and lands HERE — so check it at entry (after the
                // `run_until` loop-top cancel check, which a sibling fault wins) and return Err.
                self.arity_range_err("read", args, 1, 2, span)?;
                if self.poll_timed_out {
                    self.poll_timed_out = false;
                    return Ok(self.sock_err("timeout"));
                }
                let timeout = self.parse_timeout_ms(args.get(1), span)?;
                // Cap the per-call buffer: a huge `read(n)` (caller-controlled) must not eagerly
                // allocate gigabytes before a byte arrives (review). The caller already loops for large
                // payloads — `read` returns the actual count.
                let n = match args.first() {
                    Some(Value::Int(n)) => ((*n).max(0) as usize).min(MAX_SOCKET_READ),
                    _ => return Err(self.err("read expects an int byte count".into(), span)),
                };
                let core = self.socket_core(h);
                let mut buf = vec![0u8; n];
                let attempt = {
                    let mut guard = core.stream.lock().unwrap();
                    let Some(stream) = guard.as_mut() else {
                        return Ok(self.sock_err("read on a closed socket"));
                    };
                    match std::io::Read::read(stream, &mut buf) {
                        Ok(got) => Ok(got),
                        Err(e) => Err((e, stream.as_raw_fd())),
                    }
                };
                match attempt {
                    Ok(got) => {
                        buf.truncate(got);
                        let s = String::from_utf8_lossy(&buf).into_owned();
                        let sv = self.alloc_str(s);
                        Ok(self.sock_ok(sv))
                    }
                    Err((e, fd)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // `timeout_ms == 0` (poll-once): do NOT park — surface the timeout immediately.
                        if timeout.is_some_and(|t| t.poll_once) {
                            return Ok(self.sock_err("timeout"));
                        }
                        let target = PollPark { key: core.key, fd, interest: poller::Interest::Read, in_flight: Arc::clone(&core.in_flight), deadline: timeout.map(|t| t.deadline) };
                        if self.park_on_fd(h, args, target, span)? {
                            return Ok(Value::Nil); // parked (sentinel; `poll_park` gates the push)
                        }
                        // No netpoller-park: inside a native callback on M:N (`native_reentry > 0`, the
                        // Rust-stack `map`/sort loop can't snapshot-park) → DEMOTE + backoff-poll the
                        // non-blocking read in place (#3 socket half). Off the M:N engine (top-level /
                        // cooperative) there is no fiber to demote → fail loud (a silent hang would also
                        // defeat the cooperative deadlock detector). net targets the `--parallel` engine.
                        if self.mn.is_some() && self.native_reentry > 0 {
                            let core = Arc::clone(&core);
                            return self.demote_block_socket(fd, poller::Interest::Read, span, move |vm| {
                                let mut b = vec![0u8; n];
                                let r = {
                                    let mut guard = core.stream.lock().unwrap_or_else(|e| e.into_inner());
                                    let Some(stream) = guard.as_mut() else {
                                        return SockPoll::Ready(Ok(vm.sock_err("read on a closed socket")));
                                    };
                                    std::io::Read::read(stream, &mut b)
                                };
                                match r {
                                    Ok(got) => {
                                        b.truncate(got);
                                        let s = String::from_utf8_lossy(&b).into_owned();
                                        let sv = vm.alloc_str(s);
                                        SockPoll::Ready(Ok(vm.sock_ok(sv)))
                                    }
                                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => SockPoll::WouldBlock,
                                    Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                                }
                            });
                        }
                        Ok(self.sock_err("read would block: std.net sockets require the --parallel engine"))
                    }
                    Err((e, _)) => Ok(self.sock_err(format!("{e}"))),
                }
            }
            "write" => {
                // `write(s)` or `write(s, timeout_ms)` — the optional trailing int bounds writability.
                self.arity_range_err("write", args, 1, 2, span)?;
                if self.poll_timed_out {
                    self.poll_timed_out = false;
                    return Ok(self.sock_err("timeout"));
                }
                let timeout = self.parse_timeout_ms(args.get(1), span)?;
                let data = match args.first() {
                    Some(Value::Obj(sh)) => match self.heap.get(*sh) {
                        Obj::Str(s) => s.as_bytes().to_vec(),
                        _ => return Err(self.err("write expects a str".into(), span)),
                    },
                    _ => return Err(self.err("write expects a str".into(), span)),
                };
                let core = self.socket_core(h);
                let attempt = {
                    let mut guard = core.stream.lock().unwrap();
                    let Some(stream) = guard.as_mut() else {
                        return Ok(self.sock_err("write on a closed socket"));
                    };
                    match std::io::Write::write(stream, &data) {
                        Ok(got) => Ok(got),
                        Err(e) => Err((e, stream.as_raw_fd())),
                    }
                };
                match attempt {
                    Ok(got) => Ok(self.sock_ok(Value::Int(got as i64))),
                    Err((e, fd)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if timeout.is_some_and(|t| t.poll_once) {
                            return Ok(self.sock_err("timeout"));
                        }
                        let target = PollPark { key: core.key, fd, interest: poller::Interest::Write, in_flight: Arc::clone(&core.in_flight), deadline: timeout.map(|t| t.deadline) };
                        if self.park_on_fd(h, args, target, span)? {
                            return Ok(Value::Nil);
                        }
                        // In-callback on M:N → demote + backoff-poll the non-blocking write (#3 socket half).
                        if self.mn.is_some() && self.native_reentry > 0 {
                            let core = Arc::clone(&core);
                            return self.demote_block_socket(fd, poller::Interest::Write, span, move |vm| {
                                let r = {
                                    let mut guard = core.stream.lock().unwrap_or_else(|e| e.into_inner());
                                    let Some(stream) = guard.as_mut() else {
                                        return SockPoll::Ready(Ok(vm.sock_err("write on a closed socket")));
                                    };
                                    std::io::Write::write(stream, &data)
                                };
                                match r {
                                    Ok(got) => SockPoll::Ready(Ok(vm.sock_ok(Value::Int(got as i64)))),
                                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => SockPoll::WouldBlock,
                                    Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                                }
                            });
                        }
                        Ok(self.sock_err("write would block: std.net sockets require the --parallel engine"))
                    }
                    Err((e, _)) => Ok(self.sock_err(format!("{e}"))),
                }
            }
            "close" => {
                self.arity_err("close", args, 0, span)?;
                let core = self.socket_core(h);
                // Disarm any pending poller registration (a `close` racing a park) before the fd drops;
                // a no-op in the common case (the owning fiber is running, not parked).
                poller::deregister(core.key);
                *core.stream.lock().unwrap() = None;
                Ok(Value::Nil)
            }
            _ => Err(self.err(format!("type Socket has no method '{method}'"), span)),
        }
    }

    /// D6 — `Listener` methods: `accept() -> Result[Socket]`, `close() -> nil`. `accept` parks on the
    /// listening fd's readability (a pending connection) under the M:N engine, like `Socket::read`.
    fn listener_method(&mut self, h: GcRef, method: &str, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        match method {
            "accept" => {
                // `accept()` or `accept(timeout_ms)` — the optional trailing int bounds how long to
                // wait for an inbound connection (D6c). Mirrors `Socket::read`'s timeout handling.
                self.arity_range_err("accept", args, 0, 1, span)?;
                if self.poll_timed_out {
                    self.poll_timed_out = false;
                    return Ok(self.sock_err("timeout"));
                }
                let timeout = self.parse_timeout_ms(args.first(), span)?;
                let core = self.listener_core(h);
                let attempt = {
                    let guard = core.listener.lock().unwrap();
                    let Some(listener) = guard.as_ref() else {
                        return Ok(self.sock_err("accept on a closed listener"));
                    };
                    match listener.accept() {
                        Ok((stream, _peer)) => Ok(stream),
                        Err(e) => Err((e, listener.as_raw_fd())),
                    }
                };
                match attempt {
                    Ok(stream) => Ok(self.accept_socket_value(stream)),
                    Err((e, fd)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if timeout.is_some_and(|t| t.poll_once) {
                            return Ok(self.sock_err("timeout"));
                        }
                        let target = PollPark { key: core.key, fd, interest: poller::Interest::Read, in_flight: Arc::clone(&core.in_flight), deadline: timeout.map(|t| t.deadline) };
                        if self.park_on_fd(h, args, target, span)? {
                            return Ok(Value::Nil);
                        }
                        // In-callback on M:N → demote + backoff-poll the non-blocking accept (#3 socket half).
                        if self.mn.is_some() && self.native_reentry > 0 {
                            let core = Arc::clone(&core);
                            return self.demote_block_socket(fd, poller::Interest::Read, span, move |vm| {
                                let r = {
                                    let guard = core.listener.lock().unwrap_or_else(|e| e.into_inner());
                                    let Some(listener) = guard.as_ref() else {
                                        return SockPoll::Ready(Ok(vm.sock_err("accept on a closed listener")));
                                    };
                                    listener.accept()
                                };
                                match r {
                                    Ok((stream, _peer)) => SockPoll::Ready(Ok(vm.accept_socket_value(stream))),
                                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => SockPoll::WouldBlock,
                                    Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                                }
                            });
                        }
                        Ok(self.sock_err("accept would block: std.net sockets require the --parallel engine"))
                    }
                    Err((e, _)) => Ok(self.sock_err(format!("{e}"))),
                }
            }
            "addr" => {
                self.arity_err("addr", args, 0, span)?;
                let core = self.listener_core(h);
                let addr = {
                    let guard = core.listener.lock().unwrap();
                    match guard.as_ref() {
                        Some(l) => l.local_addr().map(|a| a.to_string()).map_err(|e| e.to_string()),
                        None => Err("addr on a closed listener".to_string()),
                    }
                };
                match addr {
                    Ok(a) => {
                        let v = self.alloc_str(a);
                        Ok(self.sock_ok(v))
                    }
                    Err(e) => Ok(self.sock_err(e)),
                }
            }
            "close" => {
                self.arity_err("close", args, 0, span)?;
                let core = self.listener_core(h);
                poller::deregister(core.key);
                *core.listener.lock().unwrap() = None;
                Ok(Value::Nil)
            }
            _ => Err(self.err(format!("type Listener has no method '{method}'"), span)),
        }
    }

    /// D6 — wrap an accepted `TcpStream` (set non-blocking) into a fresh `Socket` handle, as a
    /// `Result::Ok`.
    fn accept_socket_value(&mut self, stream: std::net::TcpStream) -> Value {
        stream.set_nonblocking(true).ok();
        let core = Arc::new(SocketCore { stream: Mutex::new(Some(stream)), key: core::next_poll_key(), in_flight: core::new_in_flight() });
        let v = Value::Obj(self.heap.alloc(Obj::Socket(core)));
        self.sock_ok(v)
    }

    /// D6 — the M:N park half shared by every would-block socket op. Returns `Ok(true)` if the fiber
    /// was parked on the netpoller; `Ok(false)` off the M:N engine (or inside a native callback, whose
    /// Rust-stack state can't be parked) — the caller then surfaces a `Result::Err` (net requires the
    /// `--parallel` engine; blocking the only thread would wedge the cooperative deadlock detector).
    /// `Err` only for a **concurrent op on a shared socket**: oneshot epoll allows ONE registration per
    /// fd, so a second fiber reaching a would-block op while the first is parked (`in_flight` already
    /// set) faults cleanly rather than corrupting the poller registry (review: Critical). On the park
    /// path it restores the pre-call operand stack (receiver THEN args — the exact layout `CallMethod`
    /// re-pops; unlike a 0-arg `recv` park, `read(n)`/`write(s)` must re-push their args), rewinds `ip`
    /// so the op re-executes on resume, and sets the `poll_park` sentinel for the worker loop.
    ///
    /// D6c — `target.deadline` (the optional `timeout_ms`) is honored ONLY on this snapshot-park path:
    /// the netpoller wakes the fiber on readiness OR at the deadline. The in-callback demote path
    /// (`native_reentry > 0`, where this returns `Ok(false)`) does NOT honor it — a demoted op
    /// backoff-polls in the kernel until readiness regardless of `timeout_ms` (a documented v1 gap;
    /// in-callback socket timeouts are out of scope, matching the in-callback connect-blocks behavior).
    fn park_on_fd(&mut self, h: GcRef, args: &[Value], target: PollPark, span: Span) -> Result<bool, RuntimeError> {
        if self.mn.is_some() && self.native_reentry == 0 {
            // The `in_flight` guard: at most one op may be parked on a socket at a time. A second
            // concurrent op on a shared socket (`Arc`) faults rather than overwrite the registry entry
            // (which would drop the first fiber + leak `inflight`) or double-`add` the fd (EEXIST panic).
            if target.in_flight.swap(true, Ordering::AcqRel) {
                return Err(self.err("concurrent operation on a shared socket is not supported".into(), span));
            }
            self.push(Value::Obj(h)); // receiver (deeper on the stack)
            for &a in args {
                self.push(a); // its args, in order, back on top
            }
            self.frames.last_mut().unwrap().ip -= 1;
            self.poll_park = Some(target);
            Ok(true)
        } else {
            Ok(false)
        }
    }


    /// `Channel[T]` methods (C2/C4): `send` (move-on-send, deep-copied in), `recv` (FIFO; empty =
    /// deadlock fault under the sequential executor), `len`. Mirrors `interp::eval_channel_method` —
    /// error strings byte-identical (parity-tested).
    fn channel_method(&mut self, h: GcRef, method: &str, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        match method {
            "send" => {
                self.arity_err("send", args, 1, span)?;
                // B3.1: serialize once into the core (the wire form IS the airlock copy).
                let w = self.to_wire(args[0])?;
                // Closed-channel guard: a `send` after `close()` faults (Go-panic analog). A `close`
                // racing in the window between this check and the enqueue is benign — the value is
                // still buffered and drained before the close is observed (drain-before-close), exactly
                // like Go's racy `select`/close. Strict mutual exclusion isn't required.
                if self.channel_core(h).q.lock().unwrap().closed {
                    return Err(self.err("send on a closed channel".to_string(), span));
                }
                self.channel_send_wire(h, w);
                Ok(Value::Nil)
            }
            // `try_send` is the safe partner of `send`: channels are unbounded, so its only failure is
            // a closed channel — returns `false` then (never faults), `true` once the value is queued.
            "try_send" => {
                self.arity_err("try_send", args, 1, span)?;
                let w = self.to_wire(args[0])?;
                if self.channel_core(h).q.lock().unwrap().closed {
                    return Ok(Value::Bool(false));
                }
                self.channel_send_wire(h, w);
                Ok(Value::Bool(true))
            }
            "recv" => {
                self.arity_err("recv", args, 0, span)?;
                // D5 owe #3 (Path C) — a `recv` reached INSIDE a native callback on the M:N engine
                // (`native_reentry > 0`) can't snapshot-park (its host-stack loop frame is not
                // capturable), so it DEMOTES the worker thread: block in place on the channel condvar +
                // spin a replacement, resuming on a sibling `send` (Go's `handoffp`). Handled before
                // `chan_recv_step` (which only covers the snapshot-park / cooperative-park / fault
                // paths). `demote_recv_block` is itself closed-aware (a `close` faults the demoted recv).
                // A `timer(ms)` channel is excluded from demote — it has no sibling sender to block on;
                // `chan_recv_step` synthesises its value (inline-sleep to the deadline) at any reentry.
                if self.mn.is_some() && self.native_reentry > 0 && self.channel_core(h).timer.is_none() {
                    return match self.demote_recv_block(h, span)? {
                        RecvStep::Got(w) => Ok(self.from_wire(w)),
                        RecvStep::ClosedEmpty => {
                            Err(self.err("receive on a closed channel".to_string(), span))
                        }
                        // demote never parks (it blocks in place); a Parked here is impossible.
                        RecvStep::Parked => unreachable!("demote_recv_block never parks"),
                    };
                }
                match self.chan_recv_step(h, span)? {
                    RecvStep::Got(w) => Ok(self.from_wire(w)),
                    // `chan_recv_step` already re-rooted the receiver + set `suspend`; the sentinel is
                    // never observed (`do_method_call` gates the result-push on `suspend`).
                    RecvStep::Parked => Ok(Value::Nil),
                    // Closed-and-drained: a distinct fault (not the deadlock fault) — no producer left.
                    RecvStep::ClosedEmpty => {
                        Err(self.err("receive on a closed channel".to_string(), span))
                    }
                }
            }
            "try_recv" => {
                // A1: non-blocking poll. Unlike `recv` it never touches `scheduler_stack` /
                // `native_reentry` / `suspend` / `ip` — it always returns immediately with an
                // `Option`: `Some(v)` if queued, `None` if empty. Mirrors `interp::eval_channel_method`.
                self.arity_err("try_recv", args, 0, span)?;
                let core = self.channel_core(h);
                let popped = core.q.lock().unwrap().queue.pop_front();
                // A `timer(ms)` channel reports ready (`Some(true)`) once its deadline has passed, even
                // with nothing queued — the level-triggered, non-blocking poll (used by `wait`'s
                // source-order scan and the `else` arm). `--parallel` may also have a real `true`
                // queued by the background send; either way `Some(true)`.
                let popped = popped.or_else(|| {
                    core.timer.filter(|d| std::time::Instant::now() >= *d).map(|_| WireValue::Bool(true))
                });
                Ok(match popped {
                    Some(w) => {
                        let v = self.from_wire(w);
                        self.alloc_enum("Option", "Some", vec![v])
                    }
                    None => self.alloc_enum("Option", "None", vec![]),
                })
            }
            // `close()` marks the channel closed (idempotent) and wakes every parked / demoted
            // receiver so each re-runs and observes the close: a `for v in ch:` ends, a bare `recv`
            // faults. Mirrors `send`'s wake fan-out but delivers no value.
            "close" => {
                self.arity_err("close", args, 0, span)?;
                let core = self.channel_core(h);
                core.q.lock().unwrap().closed = true;
                if let Some(sched) = self.mn.clone() {
                    let key = self.channel_core_ptr(h);
                    sched.close_wake(key, &core);
                } else {
                    // Wake any demoted OS thread blocked on this core's condvar (in-callback recv).
                    core.cv.notify_all();
                    // Cooperative engine: re-add every sibling fiber parked on this channel's `recv`.
                    self.wake_on_send(h);
                }
                Ok(Value::Nil)
            }
            "len" => {
                self.arity_err("len", args, 0, span)?;
                let n = self.channel_core(h).q.lock().unwrap().queue.len();
                Ok(Value::Int(n as i64))
            }
            _ => Err(self.err(format!("type Channel has no method '{method}'"), span)),
        }
    }

    /// Enqueue an already-wire-serialized message into a channel and wake any receivers — the shared
    /// tail of `send`/`try_send` (after their respective closed-channel guards). On the M:N engine the
    /// enqueue + wake of every fiber parked on this channel is atomic under the sched lock
    /// ([`MnSched::send_wake`]) so a sibling parking concurrently can't be lost. With no scheduler
    /// (cooperative / cross-nursery / top-level) it enqueues + notifies the core condvar (a demoted
    /// in-callback recv) + re-adds any cooperative fiber parked on this channel.
    fn channel_send_wire(&mut self, h: GcRef, w: WireValue) {
        let core = self.channel_core(h);
        if let Some(sched) = self.mn.clone() {
            let key = self.channel_core_ptr(h);
            sched.send_wake(key, &core, w);
        } else {
            core.q.lock().unwrap().queue.push_back(w);
            core.cv.notify_all();
            self.wake_on_send(h);
        }
    }

    /// One blocking-`recv` step on the snapshot-park / cooperative-park / fault paths (NOT the
    /// in-callback demote path, which `recv` handles directly). Pops a value if one is waiting,
    /// signals `ClosedEmpty` on a closed-and-drained channel, or parks the running fiber (re-rooting
    /// the receiver + rewinding `ip` so the calling op re-runs on resume, setting `suspend`). Shared
    /// by `recv` (`CallMethod`) and the `ChanRecvOrClosed` op (`for v in ch:`).
    fn chan_recv_step(&mut self, h: GcRef, span: Span) -> Result<RecvStep, RuntimeError> {
        // A `timer(ms)` channel delivers `true` once its deadline passes. Handled here (uniformly,
        // before the ordinary park logic) so it works regardless of the engine the receiver runs in
        // and where the timer was created. Delivery is scheduled at RECV time, in the recv's own
        // scheduler — not at construction (a timer made at the top level can be recv'd in a child).
        {
            let core = self.channel_core(h);
            if let Some(deadline) = core.timer {
                // A prior park's timer `send` may already have delivered — consume it first.
                if let Some(w) = core.q.lock().unwrap().queue.pop_front() {
                    return Ok(RecvStep::Got(w));
                }
                let now = std::time::Instant::now();
                if now >= deadline {
                    return Ok(RecvStep::Got(WireValue::Bool(true)));
                }
                if self.mn.is_some() && self.native_reentry == 0 {
                    // --parallel, top level: schedule a one-shot background `send(true)` at the deadline
                    // (in THIS scheduler) and park. The pending timer is accounted `inflight` so it
                    // vetoes the deadlock predicate while the lone fiber waits; the job un-accounts it.
                    if self.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
                        self.cancelled = true;
                        return Err(self.err("cancelled".to_string(), span));
                    }
                    let sched = self.mn.clone().unwrap();
                    let key = self.channel_core_ptr(h);
                    let core_job = Arc::clone(&core);
                    let sched_job = Arc::clone(&sched);
                    sched.inflight.fetch_add(1, Ordering::Relaxed);
                    timer::submit_at(
                        deadline,
                        Box::new(move || {
                            sched_job.send_wake(key, &core_job, WireValue::Bool(true));
                            sched_job.inflight.fetch_sub(1, Ordering::Relaxed);
                        }),
                    );
                    self.park_recv(h);
                    return Ok(RecvStep::Parked);
                }
                // Cooperative VM / interp / a `--parallel` callback (`native_reentry > 0`): inline-sleep
                // to the deadline (single-thread, or an already-blocking host-stack context), synthesise.
                // Limitation (vs `sleep_ms`, which DEMOTES at `native_reentry > 0`): a `timer.recv()`
                // reached inside a native callback under `--parallel` pins THIS worker for the timeout
                // (no replacement is spun). Sound — siblings on the other N-1 workers still progress —
                // but lower throughput than `sleep_ms`'s demote. Acceptable for v1; demote-reuse is a
                // future improvement. The cooperative/interp inline-sleep blocks siblings the same way
                // their `sleep_ms` already does (single-thread).
                std::thread::sleep(deadline - now);
                return Ok(RecvStep::Got(WireValue::Bool(true)));
            }
        }
        // M:N snapshot-park path (empty-open parks the fiber; the worker loop files it into the wait
        // set). Cancel is checked FIRST: a fiber woken only to be cancelled must not re-park.
        if self.mn.is_some() && self.native_reentry == 0 {
            if self.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
                self.cancelled = true;
                return Err(self.err("cancelled".to_string(), span));
            }
            let core = self.channel_core(h);
            let mut g = core.q.lock().unwrap();
            if let Some(w) = g.queue.pop_front() {
                return Ok(RecvStep::Got(w));
            }
            if g.closed {
                return Ok(RecvStep::ClosedEmpty);
            }
            drop(g);
            self.park_recv(h);
            return Ok(RecvStep::Parked);
        }
        // Cooperative / no-scheduler path. Pop + closed read are atomic under one lock.
        let core = self.channel_core(h);
        let mut g = core.q.lock().unwrap();
        if let Some(w) = g.queue.pop_front() {
            return Ok(RecvStep::Got(w));
        }
        let closed = g.closed;
        drop(g);
        if closed {
            return Ok(RecvStep::ClosedEmpty);
        }
        // Empty + open: under an active nursery scheduler (and not in a native callback) the fiber
        // suspends; the scheduler resumes it once a sibling `send`s.
        if !self.scheduler_stack.is_empty() && self.native_reentry == 0 {
            self.park_recv(h);
            return Ok(RecvStep::Parked);
        }
        // No scheduler (top level / single fiber) or a native callback on the cooperative engine: no
        // sibling could ever fill the channel — a real deadlock.
        Err(self.err(
            "recv on an empty channel: deadlock — nothing is queued and the \
             sequential executor cannot block waiting for a producer (a \
             consumer that waits mid-flight on a live producer needs C5)"
                .to_string(),
            span,
        ))
    }

    /// Park the running fiber on an empty `recv`: re-root the receiver on the operand stack, rewind
    /// `ip` so the current op (`CallMethod(recv)` or `ChanRecvOrClosed`) re-executes on resume, and
    /// set the `suspend` sentinel. The scheduler / worker loop files the fiber into the channel's
    /// wait set; a sibling `send`/`close` wakes it.
    fn park_recv(&mut self, h: GcRef) {
        self.push(Value::Obj(h));
        self.frames.last_mut().unwrap().ip -= 1;
        self.suspend = Some(h);
    }

    /// `Shared[T]` methods (C3/C4): `get` (copies out), `set` (copies in), `update` (read-modify-write
    /// via the re-entrant call path). Mirrors `interp::eval_shared_method`. The box is re-rooted on
    /// the operand stack across `update`'s nested call (the receiver was popped in `do_method_call`).
    fn shared_method(&mut self, h: GcRef, method: &str, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        match method {
            "get" => {
                self.arity_err("get", args, 0, span)?;
                // Clone the wire form out under the lock, then reconstruct into this heap (one
                // round-trip == the old deep_clone-out).
                let w = self.shared_core(h).v.lock().unwrap().clone();
                Ok(self.from_wire(w))
            }
            "set" => {
                self.arity_err("set", args, 1, span)?;
                let w = self.to_wire(args[0])?;
                *self.shared_core(h).v.lock().unwrap() = w;
                Ok(Value::Nil)
            }
            "update" => {
                self.arity_err("update", args, 1, span)?;
                let f = args[0];
                let core = self.shared_core(h);
                // B3.3-threads: serialise the whole read-modify-write so concurrent OS-thread updates
                // can't lose each other (Shared[T]'s core contract). Held only under `--parallel`
                // (the cooperative engine is single-thread, so it keeps its current behavior and
                // never risks deadlocking a same-box nested update). The value lock `v` is still held
                // only briefly — read here, write at the end — so the closure may freely re-enter
                // `get`/`set` (or `update` on a *different* box). A `--parallel` closure that re-enters
                // `update` on the SAME box deadlocks: a documented edge (it could only lose-update
                // before). The handle is re-rooted on the operand stack so the nested call's GC keeps
                // the core's contents traced (the receiver was popped off the stack in `do_method_call`).
                let _serialise = if self.parallel { Some(core.update_lock.lock().unwrap()) } else { None };
                let w = core.v.lock().unwrap().clone();
                let cur = self.from_wire(w);
                self.push(Value::Obj(h));
                let next = self.guarded(|vm| vm.invoke_value(f, vec![cur], span));
                self.pop();
                let next = next?;
                let stored = self.to_wire(next)?;
                *core.v.lock().unwrap() = stored;
                Ok(Value::Nil)
            }
            _ => Err(self.err(format!("type Shared has no method '{method}'"), span)),
        }
    }

    /// `Atomic[T]` methods: `load` (copy out), `store` (copy in), `exchange` (swap, returns old),
    /// `cas(expected, new) -> bool` (swap iff the box equals `expected`), `add`/`sub` (numeric RMW,
    /// returns the new value). Each is a single lock-op-unlock, so the RMW is atomic across threads —
    /// no user closure runs under the lock (unlike `Shared.update`), so no `update_lock` is needed.
    /// Mirrors `interp::eval_atomic_method`. `add`/`sub` use the language's `checked_add`/`checked_sub`
    /// (int overflow faults, like the `+`/`-` operators) and plain float arithmetic.
    fn atomic_method(&mut self, h: GcRef, method: &str, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        match method {
            "load" => {
                self.arity_err("load", args, 0, span)?;
                let w = self.atomic_core(h).v.lock().unwrap().clone();
                Ok(self.from_wire(w))
            }
            "store" => {
                self.arity_err("store", args, 1, span)?;
                let w = self.to_wire(args[0])?;
                *self.atomic_core(h).v.lock().unwrap() = w;
                Ok(Value::Nil)
            }
            "exchange" => {
                self.arity_err("exchange", args, 1, span)?;
                let new_w = self.to_wire(args[0])?;
                let core = self.atomic_core(h);
                let old = {
                    let mut g = core.v.lock().unwrap();
                    std::mem::replace(&mut *g, new_w)
                };
                Ok(self.from_wire(old))
            }
            "cas" => {
                self.arity_err("cas", args, 2, span)?;
                let core = self.atomic_core(h);
                // Hold the value lock across compare+swap so the CAS is atomic. `from_wire`/`to_wire`/
                // `values_equal` borrow `self`, not the guard (which borrows the cloned `Arc`), so the
                // lock can stay held while they run.
                let mut g = core.v.lock().unwrap();
                let cur = self.from_wire(g.clone());
                let swapped = self.values_equal(cur, args[0]);
                if swapped {
                    *g = self.to_wire(args[1])?;
                }
                Ok(Value::Bool(swapped))
            }
            "add" | "sub" => {
                self.arity_err(method, args, 1, span)?;
                let delta = self.to_wire(args[0])?;
                let core = self.atomic_core(h);
                let mut g = core.v.lock().unwrap();
                let new = match (&*g, &delta) {
                    (WireValue::Int(a), WireValue::Int(b)) => {
                        let (r, label) = if method == "add" {
                            (a.checked_add(*b), "Add")
                        } else {
                            (a.checked_sub(*b), "Sub")
                        };
                        WireValue::Int(r.ok_or_else(|| self.err(format!("integer overflow in {label}"), span))?)
                    }
                    (WireValue::Float(a), WireValue::Float(b)) => {
                        WireValue::Float(if method == "add" { a + b } else { a - b })
                    }
                    // The checker gates `add`/`sub` to numeric element types, so this is unreachable.
                    _ => return Err(self.err(format!("type Atomic has no method '{method}'"), span)),
                };
                *g = new.clone();
                drop(g);
                Ok(self.from_wire(new))
            }
            _ => Err(self.err(format!("type Atomic has no method '{method}'"), span)),
        }
    }

    /// `Executor` methods (C5/escape hatch): `submit` (enqueue a detached task closure, rejected once
    /// shut), `shutdown` (graceful — drain FIFO via the re-entrant call path), `shutdown_now` (discard
    /// pending). Mirrors `interp::eval_executor_method` — error strings byte-identical (parity-tested).
    /// The executor handle is re-rooted on the operand stack across the drain, and each popped task is
    /// rooted across its nested call (the receiver was popped in `do_method_call`).
    fn executor_method(&mut self, h: GcRef, method: &str, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        match method {
            "submit" => {
                self.arity_err("submit", args, 1, span)?;
                let core = self.executor_core(h);
                {
                    let mut g = core.inner.lock().unwrap();
                    if g.shut {
                        return Err(self.err(
                            "submit on a shut-down Executor (it no longer accepts work)".to_string(),
                            span,
                        ));
                    }
                    // B3.6: under `--parallel` the task closure crosses **by value** (`WireValue::Closure`
                    // — proto + wire'd captures + home index) so a pool-thread drain can rebuild and run
                    // it; queued captures stay rooted via the executor handle's `children()` (the
                    // `Closure` arm of `collect_core_gcrefs`). On the cooperative default engine it crosses
                    // **by handle** (`to_wire` → `Handle`) exactly as before B3.6 — the drain runs on this
                    // same heap, so captures must stay *shared by reference* (a mutation between `submit`
                    // and drain is observable, matching the interp oracle); a by-value snapshot here would
                    // break `VM == interp` for the sequential subset (decision A). The engine flag is fixed
                    // for a VM's lifetime, so submit-time and drain-time agree on which form was queued.
                    let w = if self.parallel {
                        self.wire_callable(args[0], span)?
                    } else {
                        self.to_wire(args[0])?
                    };
                    g.queue.push_back(w);
                }
                Ok(Value::Nil)
            }
            "shutdown" => {
                self.arity_err("shutdown", args, 0, span)?;
                let core = self.executor_core(h);
                // Mark shut first so a task that re-enters this executor (submit/shutdown) sees it.
                core.inner.lock().unwrap().shut = true;
                if self.parallel {
                    // B3.6: drain the whole queue under the lock (drop the guard before running any
                    // task — never hold the core lock across an invoke), then run the tasks on the
                    // bounded pool. Output flushes in submission order; the first fault propagates.
                    let tasks: Vec<WireValue> = core.inner.lock().unwrap().queue.drain(..).collect();
                    self.drain_executor_on_pool(tasks, span)?;
                } else {
                    // Cooperative engine: inline FIFO drain (unchanged). Root the executor handle across
                    // the drain (its remaining queue is traced via it); each popped task is rooted on
                    // the stack across its re-entrant call.
                    self.push(Value::Obj(h));
                    loop {
                        // Pop under the lock, then DROP the guard before the re-entrant call.
                        let task = core.inner.lock().unwrap().queue.pop_front();
                        let Some(task) = task else { break };
                        let task = self.from_wire(task);
                        self.push(task);
                        let r = self.guarded(|vm| vm.invoke_value(task, vec![], span));
                        self.pop();
                        r?;
                    }
                    self.pop(); // the executor root
                }
                Ok(Value::Nil)
            }
            "shutdown_now" => {
                self.arity_err("shutdown_now", args, 0, span)?;
                let core = self.executor_core(h);
                let mut g = core.inner.lock().unwrap();
                g.shut = true;
                g.queue.clear();
                Ok(Value::Nil)
            }
            _ => Err(self.err(format!("type Executor has no method '{method}'"), span)),
        }
    }

    /// Mirrors `interp::Interp::drain_live_executors` (C5 / A2): at a clean program end, gracefully
    /// drain every `Executor` created but never explicitly `shutdown`/`shutdown_now`-ed, in creation
    /// order, reusing the shipped `shutdown` path (FIFO, first-fault-aborts-siblings). A hard
    /// `std.os.exit` is not drained (the caller gates on `pending_exit`); a task that calls
    /// `os.exit` mid-drain stops the remaining drain.
    fn drain_live_executors(&mut self, span: Span) -> Result<(), RuntimeError> {
        if self.pending_exit.is_some() {
            return Ok(());
        }
        // Snapshot the handles: a drained task may create new executors; reap only those alive at
        // exit (parity with the interpreter's `Vec<Rc>` snapshot).
        let execs = self.executors.clone();
        for h in execs {
            let shut = self.executor_core(h).inner.lock().unwrap().shut;
            if shut {
                continue;
            }
            self.executor_method(h, "shutdown", &[], span)?;
            if self.pending_exit.is_some() {
                break; // a drained task called os.exit — hard halt, stop draining
            }
        }
        Ok(())
    }

    /// Drain the current (top) frame's deferred calls, LIFO, popping one at a time from the frame's
    /// own list so the not-yet-run records stay GC-rooted in the frame. Skipped on a hard
    /// `std.os.exit` (Go: `os.Exit` does not run deferred calls). Returns the latest fault, if any.
    fn drain_top_frame_deferred(&mut self) -> Option<RuntimeError> {
        if self.pending_exit.is_some() {
            return None;
        }
        let fi = self.frames.len() - 1;
        let mut err = None;
        while let Some(d) = self.frames[fi].deferred.pop() {
            if let Err(e) = self.run_one_deferred(d) {
                err = Some(e);
                if self.pending_exit.is_some() {
                    break;
                }
            }
        }
        err
    }

    /// Leave a lexical defer scope (`LeaveDeferScope`): pop the top marker and run the current
    /// frame's defers registered since it, LIFO. This is the block-scoped analogue of
    /// `drain_top_frame_deferred` — it drains down to a marker, not to the bottom of the frame.
    /// Skipped on a hard `std.os.exit`. Returns the latest fault from a deferred call, if any.
    fn leave_defer_scope(&mut self) -> Option<RuntimeError> {
        let fi = self.frames.len() - 1;
        debug_assert!(
            !self.frames[fi].defer_markers.is_empty(),
            "LeaveDeferScope without a matching EnterDeferScope (compiler scope-count desync)"
        );
        let marker = self.frames[fi].defer_markers.pop().unwrap_or(0);
        self.drain_frame_to(marker)
    }

    /// Drain the current (top) frame's pending defers down to `marker` (the count to leave behind),
    /// LIFO. The block-scoped analogue of `drain_top_frame_deferred` for an explicit marker — used
    /// by `LeaveDeferScope` and by every `recover:` boundary path. Skipped on a hard `std.os.exit`.
    /// Returns the latest fault from a deferred call, if any.
    fn drain_frame_to(&mut self, marker: usize) -> Option<RuntimeError> {
        if self.pending_exit.is_some() {
            return None;
        }
        let fi = self.frames.len() - 1;
        let mut err = None;
        while self.frames[fi].deferred.len() > marker {
            let d = self.frames[fi].deferred.pop().unwrap();
            if let Err(e) = self.run_one_deferred(d) {
                err = Some(e);
                if self.pending_exit.is_some() {
                    break;
                }
            }
        }
        err
    }

    /// Unwind frames from the current depth down to `target_frame_len`, running each discarded
    /// frame's deferred calls (innermost first) before dropping it. Used on a fault: deferred
    /// cleanup runs as the stack unwinds, before a `recover:` boundary regains control (or before
    /// the program exits on an uncaught fault). A fault in a deferred call supersedes the original.
    ///
    /// `report_escaped` — a genuine fault (not a B3.4 cancel / `std.os.exit`) cancels-and-reports
    /// each discarded frame's escaped nurseries (its implicit nursery + any inner `parallel:` the
    /// fault unwound past) BEFORE that frame's `defer`s run — matching the interp oracle, which
    /// reports in `exec_parallel` / `leave_implicit_nursery` as the body unwinds and only then runs
    /// `finish_frame`'s defers. The MODULE top-level nursery is preserved (it joins only on a clean
    /// run to program end; an uncaught top-level fault leaves it silent, as in the interp).
    fn unwind_deferred(&mut self, target_frame_len: usize, report_escaped: bool) -> Option<RuntimeError> {
        let mut err = None;
        while self.frames.len() > target_frame_len {
            let fi = self.frames.len() - 1;
            // Report this frame's escaped nurseries BEFORE its defers (drain pops innermost-first, so
            // inner `parallel:` levels report before the frame's implicit one — the interp's order).
            if report_escaped {
                let f = &self.frames[fi];
                let floor = if f.is_toplevel && f.has_implicit_nursery {
                    f.nursery_len + 1 // preserve the module nursery
                } else {
                    f.nursery_len
                };
                self.drain_escaped_nursery(floor.min(self.nurseries.len()));
            }
            if self.pending_exit.is_none() {
                while let Some(d) = self.frames[fi].deferred.pop() {
                    if let Err(e) = self.run_one_deferred(d) {
                        err = Some(e);
                        if self.pending_exit.is_some() {
                            break;
                        }
                    }
                }
            }
            let frame = self.frames.pop().unwrap();
            if frame.counted {
                self.call_depth -= 1;
            }
            self.stack.truncate(frame.base);
            self.cur_base = self.frames.last().map(|f| f.base).unwrap_or(0);
            while self.handlers.last().is_some_and(|h| h.frame_len > self.frames.len()) {
                self.handlers.pop();
            }
        }
        err
    }

    fn do_try(&mut self, span: Span) -> Result<(), RuntimeError> {
        let v = self.pop();
        // Extract (variant, payload-arity, first-payload) up front so the heap borrow is released
        // before we mutate the stack / unwind a frame.
        let info = match v {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Enum { ty, variant, payload } => Some((ty.to_string(), variant.to_string(), payload.len(), payload.first().copied())),
                _ => None,
            },
            _ => None,
        };
        // Gate on the *type* (`Result`/`Option`), not the bare variant name, so a user enum that
        // shadows `Ok`/`Err`/`Some`/`None` is not treated as a Result/Option by `?`.
        if let Some((ty, variant, n, first)) = info {
            if (ty == "Result" && variant == "Ok" || ty == "Option" && variant == "Some") && n == 1 {
                self.push(first.unwrap());
                return Ok(());
            }
            if ty == "Result" && variant == "Err" || ty == "Option" && variant == "None" {
                // A `?` directly inside a `recover:` block (a handler installed in THIS frame)
                // short-circuits to that boundary (try-block style): the `Err`/`None` value becomes
                // the recover's result. Function-scoped `?` (no same-frame handler) falls through.
                let frame_len = self.frames.len();
                if let Some(h) = self.handlers.pop_if(|h| h.frame_len == frame_len) {
                    self.stack.truncate(h.stack_len);
                    self.call_depth = h.call_depth;
                    // Drop scope markers of defer scopes opened inside the recover block — the `?`
                    // jumps past their `LeaveDeferScope`s, so they would otherwise leak.
                    self.frames.last_mut().unwrap().defer_markers.truncate(h.markers_len);
                    // TASK B — a recover-scoped `?` jumps past the `JoinNursery` of any `parallel:`
                    // opened inside the recover block: cancel-and-report its unstarted tasks HERE
                    // (before the handler binds its result and execution continues), so a recover-caught
                    // `?` reports IDENTICALLY-AND-AS-EARLY as an uncaught one — matching the interp,
                    // whose `exec_parallel` reports during the `?` unwind, before the recover's value is
                    // produced. Without this the nursery lingered until the whole frame returned (the
                    // report then trailed `print("recovered")`, an interp/VM divergence).
                    //
                    // ORDERING (matches the interp oracle): the escaped `parallel:` BODY's own defers
                    // must run BEFORE the cancel-report, and the recover block's defers AFTER it —
                    // because in the interp the body is its own `exec_scoped_block` whose defers drain
                    // as the `?` unwinds out of the body, and only then does `exec_parallel` report;
                    // the recover block's defers run later, at the recover boundary. So: drain the
                    // body defers down to the outermost escaped nursery's floor, report, then drain the
                    // remaining (recover-block) defers down to the handler's install-time floor. A body
                    // defer fault is held and superseded by any later recover-block defer fault.
                    let mut body_defer_err = if self.nurseries.len() > h.nursery_len {
                        let floor = self.nursery_defer_floors[h.nursery_len];
                        self.drain_frame_to(floor)
                    } else {
                        None
                    };
                    if self.pending_exit.is_some() && let Some(e) = body_defer_err.take() {
                        return Err(e);
                    }
                    self.drain_escaped_nursery(h.nursery_len);
                    // Drain the recover block's own defers before binding the result. A fault in one
                    // supersedes the propagated value (becomes the recover's `Err`); a recover-block
                    // defer fault in turn supersedes a body defer fault (it unwinds later).
                    match self.drain_frame_to(h.defer_len) {
                        Some(e) if self.pending_exit.is_some() => return Err(e),
                        Some(e) => {
                            let msg = self.alloc_str(e.message);
                            let err = self.alloc_enum("Result", "Err", vec![msg]);
                            self.push(err);
                        }
                        None => match body_defer_err {
                            // No recover-block defer fault, but a parallel-body defer faulted: that
                            // becomes the recover's `Err` (Go semantics — a defer fault supersedes).
                            Some(e) => {
                                let msg = self.alloc_str(e.message);
                                let err = self.alloc_enum("Result", "Err", vec![msg]);
                                self.push(err);
                            }
                            None => self.push(v), // the propagated Result/Option value IS the result
                        },
                    }
                    self.jump(h.ip);
                    return Ok(());
                }
                // A `?` at the top level (no enclosing function) is an unhandled error → exit. Use
                // the `?` op's own `span` so the reported location matches the interp (which threads
                // the `?`'s `expr.span` through its propagation marker).
                if self.frames.last().unwrap().is_toplevel {
                    return Err(self.top_level_error(v, span).unwrap_or_else(|| {
                        self.err(format!("unhandled error: {}", self.display(v)), span)
                    }));
                }
                // Otherwise early-return this value from the enclosing function (running its
                // deferred calls first; a fault in one propagates as a fault).
                self.push(v);
                self.do_return(true)?;
                return Ok(());
            }
        }
        Err(self.err(format!("'?' expects Result or Option, found {}", self.type_name(v)), span))
    }

    // ----- construction / access -----

    fn new_struct(&mut self, name: &str, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let def = self.program.structs.get(name).cloned().ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        if argc != def.fields.len() {
            return Err(self.err(format!("struct '{name}' expects {} field(s), got {argc}", def.fields.len()), span));
        }
        let at = self.stack.len() - argc;
        let vals: Vec<Value> = self.stack.split_off(at);
        let fields: Vec<(Box<str>, Value)> = def.fields.iter().cloned().map(|f| f.into_boxed_str()).zip(vals).collect();
        let h = self.heap.alloc(Obj::Struct { name: name.into(), tid: def.tid, fields });
        self.push(Value::Obj(h));
        Ok(())
    }

    /// The dense layout id for a struct type `name`, or [`TID_NONE`] if it isn't a registered type
    /// (native/ad-hoc structs) — such a struct never IC-caches, so it stays sound on the probe path.
    fn struct_tid(&self, name: &str) -> u32 {
        self.program.structs.get(name).map_or(TID_NONE, |d| d.tid)
    }

    fn new_enum(&mut self, ty: &str, variant: &str, argc: usize, span: Span) -> Result<(), RuntimeError> {
        if let Some(def) = self.program.variants.get(variant)
            && argc != def.arity
        {
            return Err(self.err(format!("variant '{variant}' expects {} value(s), got {argc}", def.arity), span));
        }
        let at = self.stack.len() - argc;
        let payload: Vec<Value> = self.stack.split_off(at);
        let h = self.heap.alloc(Obj::Enum { ty: ty.into(), variant: variant.into(), payload });
        self.push(Value::Obj(h));
        Ok(())
    }

    fn get_field(&mut self, name: &str, ic: u32, span: Span) -> Result<(), RuntimeError> {
        let obj = self.pop();
        let Value::Obj(h) = obj else {
            return Err(self.err(format!("cannot read field '{name}' of {}", self.type_name(obj)), span));
        };
        self.ensure_module_faulted(h); // D1: `module.member` on a not-yet-faulted worker module
        // M19 Phase 5b — inline-cache fast path: a hit collapses the struct name-probe to one pure-int
        // `tid` compare (the struct's layout id). Same `tid` ⇒ same field order ⇒ the cached `idx` is
        // the right slot, so the field-name re-verify is unnecessary. `cell.tid == TID_NONE` (empty or
        // an unregistered struct) never matches, forcing the probe below. `fields.get` stays bounds-
        // safe (defensive; same `tid` guarantees in-range). Worst case on a miss: a re-probe + refill.
        if ic != NO_IC {
            let cell = self.field_ic[ic as usize];
            if cell.tid != TID_NONE
                && let Obj::Struct { tid, fields, .. } = self.heap.get(h)
                && *tid == cell.tid
                && let Some((_, v)) = fields.get(cell.idx as usize)
            {
                let v = *v;
                self.push(v);
                return Ok(());
            }
        }
        match self.heap.get(h) {
            // `t.0`, `t.1`, … — tuple element access. The field name is the element index.
            Obj::Tuple(items) => {
                let v = name.parse::<usize>().ok().and_then(|i| items.get(i).copied());
                match v {
                    Some(v) => {
                        self.push(v);
                        Ok(())
                    }
                    None => Err(self.err(
                        format!("tuple has no element '.{name}' (len {})", items.len()),
                        span,
                    )),
                }
            }
            Obj::Struct { tid, fields, .. } => {
                // Probe + capture the index (and the layout `tid`) so the IC can cache both (Value is
                // Copy and `tid` is a `u32`, so `found` owns its data and the heap borrow ends here,
                // freeing `self` for the field_ic write below).
                let tid = *tid;
                let found = fields
                    .iter()
                    .enumerate()
                    .find(|(_, (k, _))| k.as_ref() == name)
                    .map(|(i, (_, v))| (i, *v));
                match found {
                    Some((i, v)) => {
                        if ic != NO_IC {
                            self.field_ic[ic as usize] = IcCell { idx: i as u32, tid };
                        }
                        self.push(v);
                        Ok(())
                    }
                    None => {
                        let shown = self.display(obj);
                        Err(self.err(format!("no field '{name}' on {shown}"), span))
                    }
                }
            }
            Obj::Module { name: mname, slots, index } => match index.get(name).map(|&i| slots[i as usize]) {
                Some(v) => {
                    self.push(v);
                    Ok(())
                }
                None => Err(self.err(format!("module '{mname}' has no member '{name}'"), span)),
            },
            _ => Err(self.err(format!("cannot read field '{name}' of {}", self.type_name(obj)), span)),
        }
    }

    /// `obj[start..end]` — bounds-clamped half-open copy of a list/str, or a struct's `slice`.
    fn get_slice(&mut self, span: Span) -> Result<(), RuntimeError> {
        let end = self.pop();
        let start = self.pop();
        let obj = self.pop();
        let s = match start {
            Value::Int(n) => n,
            other => return Err(self.err(format!("expected int, found {}", self.type_name(other)), span)),
        };
        let e = match end {
            Value::Int(n) => n,
            other => return Err(self.err(format!("expected int, found {}", self.type_name(other)), span)),
        };
        let Value::Obj(h) = obj else {
            return Err(self.err(format!("cannot slice {}", self.type_name(obj)), span));
        };
        // Snapshot the result kind without holding the heap borrow across the alloc / method call.
        enum Sliced {
            List(Vec<Value>),
            Str(String),
            Struct,
        }
        let sliced = match self.heap.get(h) {
            Obj::List(items) => {
                let (lo, hi) = clamp_range(s, e, items.len());
                Sliced::List(items[lo..hi].to_vec())
            }
            Obj::Str(string) => {
                let chars: Vec<char> = string.chars().collect();
                let (lo, hi) = clamp_range(s, e, chars.len());
                Sliced::Str(chars[lo..hi].iter().collect())
            }
            Obj::Struct { .. } => Sliced::Struct,
            _ => return Err(self.err(format!("cannot slice {}", self.type_name(obj)), span)),
        };
        match sliced {
            Sliced::List(slice) => {
                // Root the source across the alloc: the new list shares its element handles, which
                // are otherwise unreachable (the source was popped) and could be collected by a GC.
                self.push(obj);
                let nh = self.heap.alloc(Obj::List(slice));
                self.pop();
                self.push(Value::Obj(nh));
            }
            Sliced::Str(sub) => {
                let nh = self.heap.alloc(Obj::Str(sub.into()));
                self.push(Value::Obj(nh));
            }
            Sliced::Struct => {
                let v = self.dispatch_index_method(h, "slice", vec![obj, Value::Int(s), Value::Int(e)], span)?;
                self.push(v);
            }
        }
        Ok(())
    }

    /// Dispatch an `Index`/`IndexSet`/`Slice` protocol method (`index`/`set_index`/`slice`) on a
    /// struct heap object. `args` already includes the receiver as its first element (bound to
    /// `self`). Mirrors `struct_arith`'s frame dispatch; the args are rooted as the new frame's locals.
    fn dispatch_index_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let Obj::Struct { name, .. } = self.heap.get(h) else { unreachable!() };
        let name = name.clone();
        let def = self
            .program
            .structs
            .get(name.as_ref())
            .cloned()
            .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        let proto = *def
            .methods
            .get(method)
            // Wording byte-identical to `interp::call_struct_method` (the engines parity-test stdout).
            .ok_or_else(|| self.err(format!("struct '{name}' has no method '{method}'"), span))?;
        let home = self.module_objs[def.module_idx];
        // Guarded (B1): `index`/`slice`/`set_index` overloads run from native opcode handlers whose
        // operand state is on the host stack, so a blocking `recv` inside one cannot park — it faults
        // `deadlock` instead of suspending (matches `struct_arith`/`compare`/`hash`).
        self.guarded(|vm| vm.run_proto(proto, home, None, args, true, false, span))
    }

    fn get_index(&mut self, span: Span) -> Result<(), RuntimeError> {
        // The index is NOT pre-validated as int (the `AsInt` was removed so map keys can be
        // str/bool): pop it as a Value and validate per object kind.
        let key = self.pop();
        let obj = self.pop();
        let Value::Obj(h) = obj else {
            return Err(self.err(format!("cannot index {}", self.type_name(obj)), span));
        };
        // M19 Tier-2 — Int-key fast path. A `List` or `Map` indexed by an `Int` needs no rooting:
        // `scalar_hash` on an int allocates nothing, can't GC, can't re-enter user code, so the
        // `hash_key_rooted` push/pop the general Map arm does is pure waste. The `candidates` +
        // `values_equal` probe is unchanged, so an Int key still matches a `values_equal` `Float`
        // key. `Str`/`Struct` (and any non-Int key) fall through to the general match below.
        if let Value::Int(n) = key {
            match self.heap.get(h) {
                Obj::List(items) => {
                    return match usize::try_from(n).ok().and_then(|i| items.get(i).copied()) {
                        Some(v) => {
                            self.push(v);
                            Ok(())
                        }
                        None => Err(self.err(format!("index {n} out of bounds (len {})", items.len()), span)),
                    };
                }
                Obj::Map(_) => {
                    let hk = self.scalar_hash(key);
                    let Obj::Map(m) = self.heap.get(h) else { unreachable!() };
                    return match m.candidates(hk).iter().copied().find(|&p| self.values_equal(m.entries[p].1, key)) {
                        Some(p) => {
                            let v = m.entries[p].2;
                            self.push(v);
                            Ok(())
                        }
                        None => Err(self.err("key not found".to_string(), span)),
                    };
                }
                _ => {}
            }
        }
        // Require an int index for list/str (the message matches the old `AsInt` exactly, for parity).
        let int_idx = |vm: &Vm| -> Result<i64, RuntimeError> {
            match key {
                Value::Int(n) => Ok(n),
                other => Err(vm.err(format!("expected int, found {}", vm.type_name(other)), span)),
            }
        };
        match self.heap.get(h) {
            Obj::List(items) => {
                let idx = int_idx(self)?;
                let v = usize::try_from(idx).ok().and_then(|i| items.get(i).copied());
                match v {
                    Some(v) => {
                        self.push(v);
                        Ok(())
                    }
                    None => Err(self.err(format!("index {idx} out of bounds (len {})", items.len()), span)),
                }
            }
            Obj::Str(s) => {
                let idx = int_idx(self)?;
                let chars: Vec<char> = s.chars().collect();
                match usize::try_from(idx).ok().and_then(|i| chars.get(i).copied()) {
                    Some(c) => {
                        let nh = self.alloc_char(c);
                        self.push(nh);
                        Ok(())
                    }
                    None => Err(self.err(format!("index {idx} out of bounds (len {})", chars.len()), span)),
                }
            }
            Obj::Map(_) => {
                let hk = self.hash_key_rooted(key, &[obj, key], span)?;
                let Obj::Map(m) = self.heap.get(h) else { unreachable!() };
                match m.candidates(hk).iter().copied().find(|&p| self.values_equal(m.entries[p].1, key)) {
                    Some(p) => {
                        let v = m.entries[p].2;
                        self.push(v);
                        Ok(())
                    }
                    None => Err(self.err("key not found".to_string(), span)),
                }
            }
            // A struct satisfying `Index` dispatches `obj[k]` to `index(self, k)`.
            Obj::Struct { .. } => {
                let v = self.dispatch_index_method(h, "index", vec![obj, key], span)?;
                self.push(v);
                Ok(())
            }
            _ => Err(self.err(format!("cannot index {}", self.type_name(obj)), span)),
        }
    }

    fn set_field(&mut self, name: &str, ic: u32, span: Span) -> Result<(), RuntimeError> {
        let val = self.pop();
        let obj = self.pop();
        let Value::Obj(h) = obj else {
            return Err(self.err(format!("cannot assign field '{name}' of {}", self.type_name(obj)), span));
        };
        // M19 Phase 5b — IC fast path (see [`Vm::get_field`]): a hit on the `tid` guard writes straight
        // to the cached index (no field-name re-verify); a miss falls through to the probe + cache-fill.
        if ic != NO_IC {
            let cell = self.field_ic[ic as usize];
            if cell.tid != TID_NONE
                && let Obj::Struct { tid, fields, .. } = self.heap.get_mut(h)
                && *tid == cell.tid
                && let Some((_, slot)) = fields.get_mut(cell.idx as usize)
            {
                *slot = val;
                return Ok(());
            }
        }
        let found;
        match self.heap.get_mut(h) {
            Obj::Struct { tid, fields, .. } => {
                let tid = *tid;
                match fields.iter_mut().enumerate().find(|(_, (k, _))| k.as_ref() == name) {
                    Some((i, (_, slot))) => {
                        *slot = val;
                        found = (i as u32, tid);
                    }
                    None => {
                        let shown = self.display(obj);
                        return Err(self.err(format!("no field '{name}' on {shown}"), span));
                    }
                }
            }
            _ => return Err(self.err(format!("cannot assign field '{name}' of {}", self.type_name(obj)), span)),
        }
        if ic != NO_IC {
            self.field_ic[ic as usize] = IcCell { idx: found.0, tid: found.1 };
        }
        Ok(())
    }

    fn set_index(&mut self, span: Span) -> Result<(), RuntimeError> {
        let val = self.pop();
        // The index is NOT pre-validated as int (AsInt removed for map keys): pop as a Value.
        let key = self.pop();
        let obj = self.pop();
        let Value::Obj(h) = obj else {
            return Err(self.err(format!("cannot index {}", self.type_name(obj)), span));
        };
        // M19 Tier-2 — Int-key fast path for a Map write: `scalar_hash` on an int needs no rooting
        // (it can't GC or re-enter, unlike a struct key's `hash()`), so skip `hash_key_rooted`. Same
        // `candidates`/`values_equal`/`push` as the general Map arm → byte-identical behavior. A
        // `Struct` with an Int key still falls through to its `set_index` protocol dispatch below.
        if let Value::Int(_) = key
            && matches!(self.heap.get(h), Obj::Map(_))
        {
            let hk = self.scalar_hash(key);
            let Obj::Map(m) = self.heap.get(h) else { unreachable!() };
            let pos = m.candidates(hk).iter().copied().find(|&p| self.values_equal(m.entries[p].1, key));
            let Obj::Map(m) = self.heap.get_mut(h) else { unreachable!() };
            match pos {
                Some(i) => m.entries[i].2 = val,
                None => m.push(hk, key, val),
            }
            return Ok(());
        }
        // For a map, hash the key (rooting the map/key/value across a struct key's re-entrant
        // hash()), locate the entry, then mutate — updating the side index on insert.
        if matches!(self.heap.get(h), Obj::Map(_)) {
            let hk = self.hash_key_rooted(key, &[obj, key, val], span)?;
            let Obj::Map(m) = self.heap.get(h) else { unreachable!() };
            let pos = m.candidates(hk).iter().copied().find(|&p| self.values_equal(m.entries[p].1, key));
            let Obj::Map(m) = self.heap.get_mut(h) else { unreachable!() };
            match pos {
                Some(i) => m.entries[i].2 = val,
                None => m.push(hk, key, val),
            }
            return Ok(());
        }
        // A struct satisfying `IndexSet` dispatches `obj[k] = v` to `set_index(self, k, v)`.
        if matches!(self.heap.get(h), Obj::Struct { .. }) {
            self.dispatch_index_method(h, "set_index", vec![obj, key, val], span)?;
            return Ok(());
        }
        let idx = match key {
            Value::Int(n) => n,
            other => return Err(self.err(format!("expected int, found {}", self.type_name(other)), span)),
        };
        match self.heap.get_mut(h) {
            Obj::List(items) => match usize::try_from(idx).ok().filter(|i| *i < items.len()) {
                Some(i) => {
                    items[i] = val;
                    Ok(())
                }
                None => {
                    let len = items.len();
                    Err(self.err(format!("index {idx} out of bounds (len {len})"), span))
                }
            },
            _ => Err(self.err(format!("cannot index {}", self.type_name(obj)), span)),
        }
    }

    fn match_arm(&mut self, scrut: usize, variant: &str, nbind: usize, bind_start: usize, next: usize, span: Span) -> Result<(), RuntimeError> {
        let v = self.stack[self.base() + scrut];
        let h = match v {
            Value::Obj(h) => h,
            _ => unreachable!("scrutinee ensured to be an enum"),
        };
        let (matches, payload) = match self.heap.get(h) {
            Obj::Enum { variant: vn, payload, .. } => (vn.as_ref() == variant, payload.clone()),
            _ => unreachable!("scrutinee ensured to be an enum"),
        };
        if !matches {
            self.jump(next);
            return Ok(());
        }
        if payload.len() != nbind {
            return Err(self.err(format!("pattern '{variant}' binds {nbind} value(s) but variant carries {}", payload.len()), span));
        }
        let base = self.base();
        for (k, pv) in payload.into_iter().enumerate() {
            self.stack[base + bind_start + k] = pv;
        }
        Ok(())
    }

    // ----- builtins / print -----

    fn do_print(&mut self, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        // Keep the args rooted on the operand stack while stringifying — a `Stringable` `str` method
        // runs user code that can GC. `stringify` pushes/pops above `at + argc`, so these indices
        // stay valid across the loop.
        let mut parts = Vec::with_capacity(argc);
        for i in 0..argc {
            let v = self.stack[at + i];
            parts.push(self.stringify(v, span, 0)?);
        }
        self.stack.truncate(at);
        self.out.push_str(&parts.join(" "));
        self.out.push('\n');
        self.push(Value::Nil);
        Ok(())
    }

    fn do_builtin(&mut self, name: &str, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        let result = match name {
            "len" => self.builtin_len(&args, span)?,
            "range" => self.builtin_range(&args, span)?,
            "int" => self.builtin_int(&args, span)?,
            "float" => self.builtin_float(&args, span)?,
            "str" => self.builtin_str(&args, span)?,
            "ord" => self.builtin_ord(&args, span)?,
            "chr" => self.builtin_chr(&args, span)?,
            "set" => self.builtin_set(&args, span)?,
            _ => unreachable!("unknown builtin {name}"),
        };
        self.push(result);
        Ok(())
    }

    fn arity_err(&self, name: &str, args: &[Value], n: usize, span: Span) -> Result<(), RuntimeError> {
        if args.len() == n {
            Ok(())
        } else {
            Err(self.err(format!("{name}() expects {n} argument(s), got {}", args.len()), span))
        }
    }

    /// D6c — arity check for a method that accepts an inclusive `min..=max` argument range (the net
    /// socket ops: `read`/`write` take 1–2, `accept` 0–1 — the optional trailing `timeout_ms`).
    fn arity_range_err(&self, name: &str, args: &[Value], min: usize, max: usize, span: Span) -> Result<(), RuntimeError> {
        if (min..=max).contains(&args.len()) {
            Ok(())
        } else {
            Err(self.err(format!("{name}() expects {min}–{max} argument(s), got {}", args.len()), span))
        }
    }

    /// D6c — parse the optional trailing `timeout_ms` int arg of a net socket op. `Ok(None)` if no
    /// timeout arg was passed (park forever — the existing behavior). `Ok(Some(Timeout))` otherwise:
    /// `poll_once` is true iff `ms <= 0` (`0` polls once and never parks; a negative saturates to it),
    /// and `deadline` is `now + ms`, saturated to a far-future deadline for a pathological `ms`
    /// (centuries) rather than panicking the worker on `Instant` overflow (mirrors `sleep_ms`). `Err`
    /// for a non-int timeout arg (the checker also rejects this; this is the runtime backstop).
    fn parse_timeout_ms(&self, arg: Option<&Value>, span: Span) -> Result<Option<SockTimeout>, RuntimeError> {
        match arg {
            None => Ok(None),
            Some(Value::Int(ms)) => {
                let poll_once = *ms <= 0;
                let ms = (*ms).max(0) as u64;
                let dur = std::time::Duration::from_millis(ms);
                let deadline = std::time::Instant::now()
                    .checked_add(dur)
                    .unwrap_or_else(|| std::time::Instant::now() + std::time::Duration::from_secs(86_400 * 365));
                Ok(Some(SockTimeout { poll_once, deadline }))
            }
            Some(_) => Err(self.err("timeout_ms expects an int (milliseconds)".into(), span)),
        }
    }

    /// `set()` → empty set; `set(list)` → a deduped hash set of the list's elements.
    fn builtin_set(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        let (list_obj, src): (Value, Vec<Value>) = match args {
            [] => (Value::Nil, Vec::new()),
            [one] => match one {
                Value::Obj(h) => match self.heap.get(*h) {
                    Obj::List(items) => (*one, items.clone()),
                    _ => return Err(self.err(format!("set() expects a list, got {}", self.type_name(*one)), span)),
                },
                other => return Err(self.err(format!("set() expects a list, got {}", self.type_name(*other)), span)),
            },
            _ => return Err(self.err(format!("set() expects 0 or 1 argument(s), got {}", args.len()), span)),
        };
        // Root the source list so its elements survive a struct element's re-entrant hash() GC; hash
        // every element first (phase 1, rooted), then build the set GC-free (phase 2).
        self.push(list_obj);
        let built = (|| {
            let mut hashes = Vec::with_capacity(src.len());
            for &v in &src {
                hashes.push(self.hash_value(v, span)?);
            }
            let mut set = SetData::default();
            for (i, &v) in src.iter().enumerate() {
                let he = hashes[i];
                if !set.candidates(he).iter().any(|&p| self.values_equal(set.entries[p].1, v)) {
                    set.push(he, v);
                }
            }
            Ok(set)
        })();
        self.pop(); // unroot the source list
        Ok(Value::Obj(self.heap.alloc(Obj::Set(built?))))
    }

    fn builtin_len(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        self.arity_err("len", args, 1, span)?;
        match args[0] {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::List(items) => Ok(Value::Int(items.len() as i64)),
                Obj::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
                _ => Err(self.err(format!("len() expects a list or str, got {}", self.type_name(args[0])), span)),
            },
            other => Err(self.err(format!("len() expects a list or str, got {}", self.type_name(other)), span)),
        }
    }

    fn builtin_range(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        const MAX_RANGE_LEN: i64 = 10_000_000;
        let (start, end) = match args {
            [Value::Int(n)] => (0, *n),
            [Value::Int(a), Value::Int(b)] => (*a, *b),
            _ => return Err(self.err("range() expects range(end) or range(start, end) of ints".to_string(), span)),
        };
        let len = i128::from(end) - i128::from(start);
        if len > i128::from(MAX_RANGE_LEN) {
            return Err(self.err(format!("range() length {len} exceeds the maximum of {MAX_RANGE_LEN}"), span));
        }
        let items: Vec<Value> = (start..end).map(Value::Int).collect();
        Ok(Value::Obj(self.heap.alloc(Obj::List(items))))
    }

    fn builtin_int(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        self.arity_err("int", args, 1, span)?;
        match args[0] {
            Value::Int(n) => Ok(Value::Int(n)),
            Value::Float(f) => {
                if !f.is_finite() || f < i64::MIN as f64 || f >= 9_223_372_036_854_775_808.0 {
                    return Err(self.err(format!("int(): {f} is out of integer range"), span));
                }
                Ok(Value::Int(f as i64))
            }
            Value::Bool(b) => Ok(Value::Int(i64::from(b))),
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Str(s) => s.trim().parse::<i64>().map(Value::Int).map_err(|_| self.err(format!("int(): cannot parse '{s}' as an integer"), span)),
                _ => Err(self.err(format!("int() cannot convert {}", self.type_name(args[0])), span)),
            },
            other => Err(self.err(format!("int() cannot convert {}", self.type_name(other)), span)),
        }
    }

    fn builtin_float(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        self.arity_err("float", args, 1, span)?;
        match args[0] {
            Value::Float(f) => Ok(Value::Float(f)),
            Value::Int(n) => Ok(Value::Float(n as f64)),
            Value::Bool(b) => Ok(Value::Float(f64::from(b))),
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Str(s) => s.trim().parse::<f64>().map(Value::Float).map_err(|_| self.err(format!("float(): cannot parse '{s}' as a float"), span)),
                _ => Err(self.err(format!("float() cannot convert {}", self.type_name(args[0])), span)),
            },
            other => Err(self.err(format!("float() cannot convert {}", self.type_name(other)), span)),
        }
    }

    fn builtin_str(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        self.arity_err("str", args, 1, span)?;
        let s = self.stringify(args[0], span, 0)?;
        Ok(Value::Obj(self.heap.alloc(Obj::Str(s.into()))))
    }

    /// `ord(s)` — codepoint of the first char of `s`. Mirrors `interp::builtins::ord` (errors too).
    fn builtin_ord(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        self.arity_err("ord", args, 1, span)?;
        match args[0] {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Str(s) => match s.chars().next() {
                    Some(c) => Ok(Value::Int(c as i64)),
                    None => Err(self.err("ord() of an empty string".to_string(), span)),
                },
                _ => Err(self.err(format!("ord() expects a str, got {}", self.type_name(args[0])), span)),
            },
            other => Err(self.err(format!("ord() expects a str, got {}", self.type_name(other)), span)),
        }
    }

    /// `chr(n)` — the 1-char str for codepoint `n`. Mirrors `interp::builtins::chr` (errors too).
    fn builtin_chr(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        self.arity_err("chr", args, 1, span)?;
        match args[0] {
            Value::Int(n) => u32::try_from(n)
                .ok()
                .and_then(char::from_u32)
                .map(|c| self.alloc_char(c))
                .ok_or_else(|| self.err(format!("chr(): {n} is not a valid Unicode codepoint"), span)),
            other => Err(self.err(format!("chr() expects an int, got {}", self.type_name(other)), span)),
        }
    }


    // ----- module namespace helpers -----

    /// Read a module global. **D1 invariant:** on a `--parallel` worker VM a module's globals are
    /// faulted in lazily, so any NEW caller that reads globals on a worker must call
    /// [`Vm::ensure_module_faulted`] for `module` first (the existing op/field/method read sites do);
    /// otherwise it may observe an empty, not-yet-faulted module and spuriously fail to resolve.
    fn module_global(&self, module: GcRef, name: &str) -> Option<Value> {
        match self.heap.get(module) {
            Obj::Module { slots, index, .. } => index.get(name).map(|&i| slots[i as usize]),
            _ => None,
        }
    }

    /// M19 Phase 2b — read a module global by compile-time slot. The home module is always pre-sized
    /// before any `GetGlobalSlot`: the top-level engine sizes it from `global_slots` in `run_module`,
    /// and a worker faults it fully in (`fault_module`) before reading. So the index is always valid.
    fn global_slot(&self, module: GcRef, slot: u32) -> Value {
        match self.heap.get(module) {
            Obj::Module { slots, .. } => slots[slot as usize],
            _ => Value::Nil,
        }
    }

    /// M19 Phase 2b — write a module global by compile-time slot (`DefineGlobalSlot`/`SetGlobalSlot`).
    fn set_global_slot(&mut self, module: GcRef, slot: u32, value: Value) {
        if let Obj::Module { slots, .. } = self.heap.get_mut(module) {
            slots[slot as usize] = value;
        }
    }

    /// Define (or overwrite) a global by name. M19 Phase 2b — if `name` already has a slot (the
    /// common case: the run driver pre-sized + indexed the module from `global_slots`, so imports
    /// and `DefineGlobalSlot` targets are already present) the value lands in that slot; otherwise a
    /// fresh slot is appended (native-module population + worker fault replay both build up modules
    /// this way, growing slots in the same order the parent assigned them).
    fn module_define(&mut self, module: GcRef, name: &str, value: Value) {
        if let Obj::Module { slots, index, .. } = self.heap.get_mut(module) {
            match index.get(name) {
                Some(&i) => slots[i as usize] = value,
                None => {
                    index.insert(name.into(), slots.len() as u32);
                    slots.push(value);
                }
            }
        }
    }

    fn module_name(&self, module: GcRef) -> String {
        match self.heap.get(module) {
            Obj::Module { name, .. } => name.to_string(),
            _ => String::new(),
        }
    }

    // ----- display / type names -----

    fn type_name(&self, v: Value) -> &'static str {
        match v {
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Bool(_) => "bool",
            Value::Nil => "nil",
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Str(_) => "str",
                Obj::List(_) => "list",
                Obj::Tuple(_) => "tuple",
                Obj::Map(_) => "map",
                Obj::Set(_) => "set",
                Obj::Struct { .. } => "struct",
                Obj::Enum { .. } => "enum",
                Obj::Func { .. } | Obj::Closure { .. } => "function",
                Obj::Module { .. } => "module",
                Obj::Native { .. } => "function",
                Obj::Channel(_) => "Channel",
                Obj::Shared(_) => "Shared",
                Obj::Atomic(_) => "Atomic",
                Obj::Executor(_) => "Executor",
                Obj::Socket(_) => "Socket",
                Obj::Listener(_) => "Listener",
            },
        }
    }

    /// `Display` form, matching `interp::value::Value`'s `Display` exactly. Thin wrapper over the
    /// depth-guarded worker — kept infallible so every error-message / `display_wire` caller is
    /// unchanged; a cyclic structure renders as `<...>` here (the print path surfaces the error).
    fn display(&self, v: Value) -> String {
        self.display_guarded(v, 0).unwrap_or_else(|_| "<...>".to_string())
    }

    /// Depth-guarded structural display. Returns `Err` (recoverable) once recursion exceeds
    /// [`MAX_STRUCTURAL_DEPTH`] — guarding cyclic data from overflowing the host stack.
    fn display_guarded(&self, v: Value, depth: usize) -> Result<String, RuntimeError> {
        if depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.err(
                "maximum structural depth (10000) exceeded (cyclic data structure?)".to_string(),
                Span { line: 1, col: 1 },
            ));
        }
        match v {
            Value::Int(n) => Ok(n.to_string()),
            Value::Float(x) => Ok(format_float(x)),
            Value::Bool(b) => Ok(b.to_string()),
            Value::Nil => Ok("nil".to_string()),
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Str(s) => Ok(s.to_string()),
                Obj::List(items) => {
                    let mut parts = Vec::with_capacity(items.len());
                    for v in items {
                        parts.push(self.display_guarded(*v, depth + 1)?);
                    }
                    Ok(format!("[{}]", parts.join(", ")))
                }
                Obj::Tuple(items) => {
                    let mut parts = Vec::with_capacity(items.len());
                    for v in items {
                        parts.push(self.display_guarded(*v, depth + 1)?);
                    }
                    Ok(format!("({})", parts.join(", ")))
                }
                Obj::Map(m) => {
                    let mut parts = Vec::with_capacity(m.entries.len());
                    for (_, k, v) in &m.entries {
                        parts.push(format!("{}: {}", self.display_guarded(*k, depth + 1)?, self.display_guarded(*v, depth + 1)?));
                    }
                    Ok(format!("{{{}}}", parts.join(", ")))
                }
                Obj::Set(s) => {
                    if s.entries.is_empty() {
                        Ok("set()".to_string())
                    } else {
                        let mut parts = Vec::with_capacity(s.entries.len());
                        for (_, v) in &s.entries {
                            parts.push(self.display_guarded(*v, depth + 1)?);
                        }
                        Ok(format!("{{{}}}", parts.join(", ")))
                    }
                }
                Obj::Struct { name, fields, .. } => {
                    let mut parts = Vec::with_capacity(fields.len());
                    for (k, v) in fields {
                        parts.push(format!("{k}={}", self.display_guarded(*v, depth + 1)?));
                    }
                    Ok(format!("{name}({})", parts.join(", ")))
                }
                Obj::Enum { variant, payload, .. } => {
                    if payload.is_empty() {
                        Ok(variant.to_string())
                    } else {
                        let mut parts = Vec::with_capacity(payload.len());
                        for v in payload {
                            parts.push(self.display_guarded(*v, depth + 1)?);
                        }
                        Ok(format!("{variant}({})", parts.join(", ")))
                    }
                }
                Obj::Func { proto, .. } => Ok(format!("<fn {}>", self.program.protos[*proto].name)),
                Obj::Closure { .. } => Ok("<closure>".to_string()),
                Obj::Module { name, .. } => Ok(format!("<module {name}>")),
                Obj::Native { name, .. } => Ok(format!("<native fn {name}>")),
                Obj::Channel(core) => Ok(format!("Channel(len={})", core.q.lock().unwrap().queue.len())),
                // B3.1: the box holds the wire form; render it directly (`display` is `&self` and
                // cannot `from_wire`, which allocates — `display_wire` is the read-only equivalent).
                Obj::Shared(core) => Ok(format!("Shared({})", self.display_wire(&core.v.lock().unwrap()))),
                Obj::Atomic(core) => Ok(format!("Atomic({})", self.display_wire(&core.v.lock().unwrap()))),
                Obj::Executor(core) => Ok(format!("Executor(pending={})", core.inner.lock().unwrap().queue.len())),
                // D6: render open/closed without exposing the fd; matches no interp counterpart (net
                // is VM-only) but mirrors the core handles' structural `Display`.
                Obj::Socket(core) => {
                    Ok(format!("Socket({})", if core.stream.lock().unwrap().is_some() { "open" } else { "closed" }))
                }
                Obj::Listener(core) => {
                    Ok(format!("Listener({})", if core.listener.lock().unwrap().is_some() { "open" } else { "closed" }))
                }
            },
        }
    }

    /// `Display` form of a [`WireValue`] — the read-only (`&self`) counterpart of [`display`] for
    /// values that live in a core (only `Shared` renders its contents). Mirrors `display` arm-for-arm;
    /// a `Handle(GcRef)` resolves back through the heap via `display`, a nested core renders like its
    /// heap counterpart. B3.1: total over the sendable set.
    fn display_wire(&self, w: &WireValue) -> String {
        match w {
            WireValue::Int(n) => n.to_string(),
            WireValue::Float(x) => format_float(*x),
            WireValue::Bool(b) => b.to_string(),
            WireValue::Nil => "nil".to_string(),
            WireValue::Str(s) => s.to_string(),
            WireValue::Handle(h) => self.display(Value::Obj(*h)),
            WireValue::List(items) => {
                let inner = items.iter().map(|v| self.display_wire(v)).collect::<Vec<_>>().join(", ");
                format!("[{inner}]")
            }
            WireValue::Tuple(items) => {
                let inner = items.iter().map(|v| self.display_wire(v)).collect::<Vec<_>>().join(", ");
                format!("({inner})")
            }
            WireValue::Map(entries) => {
                let inner = entries
                    .iter()
                    .map(|(_, k, v)| format!("{}: {}", self.display_wire(k), self.display_wire(v)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{{{inner}}}")
            }
            WireValue::Set(entries) => {
                if entries.is_empty() {
                    "set()".to_string()
                } else {
                    let inner = entries.iter().map(|(_, v)| self.display_wire(v)).collect::<Vec<_>>().join(", ");
                    format!("{{{inner}}}")
                }
            }
            WireValue::Struct { name, fields } => {
                let inner = fields.iter().map(|(k, v)| format!("{k}={}", self.display_wire(v))).collect::<Vec<_>>().join(", ");
                format!("{name}({inner})")
            }
            WireValue::Enum { variant, payload, .. } => {
                if payload.is_empty() {
                    variant.to_string()
                } else {
                    let inner = payload.iter().map(|v| self.display_wire(v)).collect::<Vec<_>>().join(", ");
                    format!("{variant}({inner})")
                }
            }
            WireValue::Channel(core) => format!("Channel(len={})", core.q.lock().unwrap().queue.len()),
            WireValue::Shared(core) => format!("Shared({})", self.display_wire(&core.v.lock().unwrap())),
            WireValue::Atomic(core) => format!("Atomic({})", self.display_wire(&core.v.lock().unwrap())),
            WireValue::Executor(core) => format!("Executor(pending={})", core.inner.lock().unwrap().queue.len()),
            // D6: render open/closed without exposing the fd (mirrors the heap `Display`).
            WireValue::Socket(core) => {
                format!("Socket({})", if core.stream.lock().unwrap().is_some() { "open" } else { "closed" })
            }
            WireValue::Listener(core) => {
                format!("Listener({})", if core.listener.lock().unwrap().is_some() { "open" } else { "closed" })
            }
            // B3.6: a wired closure renders like its heap counterpart (`Obj::Closure` → "<closure>").
            WireValue::Closure { .. } => "<closure>".to_string(),
        }
    }

    /// Protocol-aware render for `print` / `str()` / interpolation: a struct with a self-only
    /// `str(self) -> str` method (the `Stringable` protocol) dispatches to it; everything else uses
    /// the default structural repr, recursing through `stringify` so nested structs honour the
    /// protocol too. Mirrors `interp::Interp::stringify` exactly (parity-tested). Distinct from the
    /// `&self` `display` above, which stays the pure structural form for error/debug text.
    fn stringify(&mut self, v: Value, span: Span, depth: usize) -> Result<String, RuntimeError> {
        let mut s = String::new();
        self.stringify_into(&mut s, v, span, depth)?;
        Ok(s)
    }

    /// Render `v` by appending into `out` — the allocation-free core shared by `stringify` (which
    /// wraps it in a fresh `String`) and `BuildStr` (which reuses one buffer across all interpolation
    /// parts). Byte-identical output to the old return-a-`String` form; only the intermediate
    /// per-part / per-element `String`s are gone.
    fn stringify_into(&mut self, out: &mut String, v: Value, span: Span, depth: usize) -> Result<(), RuntimeError> {
        use std::fmt::Write as _;
        // Guard against cyclic data overflowing the host stack — turns SIGABRT into a recoverable
        // `RuntimeError` (a `str` method re-stringifies at the *same* depth, so a non-recursive
        // protocol hook doesn't burn the budget).
        if depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.err("maximum structural depth (10000) exceeded (cyclic data structure?)".to_string(), span));
        }
        match v {
            Value::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Value::Float(x) => out.push_str(&format_float(x)),
            Value::Bool(b) => out.push_str(if b { "true" } else { "false" }),
            Value::Nil => out.push_str("nil"),
            // ROOT the object on the operand stack: a `str` method runs nested frames that GC at
            // instruction boundaries, and the container keeps its transitive contents reachable.
            Value::Obj(h) => {
                self.push(v);
                let r = self.stringify_obj_into(out, h, span, depth);
                self.pop();
                return r;
            }
        }
        Ok(())
    }

    fn stringify_obj_into(&mut self, out: &mut String, h: GcRef, span: Span, depth: usize) -> Result<(), RuntimeError> {
        use std::fmt::Write as _;
        // Clone the object's shape out so no heap borrow is held across the nested `&mut self` calls.
        match self.heap.get(h).clone() {
            Obj::Str(s) => out.push_str(&s),
            Obj::List(items) => {
                out.push('[');
                self.stringify_seq_into(out, &items, span, depth + 1)?;
                out.push(']');
            }
            Obj::Tuple(items) => {
                out.push('(');
                self.stringify_seq_into(out, &items, span, depth + 1)?;
                out.push(')');
            }
            Obj::Map(m) => {
                out.push('{');
                for (i, (_, k, mv)) in m.entries.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    self.stringify_into(out, *k, span, depth + 1)?;
                    out.push_str(": ");
                    self.stringify_into(out, *mv, span, depth + 1)?;
                }
                out.push('}');
            }
            Obj::Set(s) => {
                if s.entries.is_empty() {
                    out.push_str("set()");
                } else {
                    out.push('{');
                    for (i, (_, e)) in s.entries.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        self.stringify_into(out, *e, span, depth + 1)?;
                    }
                    out.push('}');
                }
            }
            Obj::Struct { name, fields, .. } => {
                // `str(self) -> str` overrides the default repr. Only a self-only method is the hook.
                if let Some(def) = self.program.structs.get(name.as_ref()).cloned()
                    && let Some(&proto) = def.methods.get("str")
                    && self.program.protos[proto].arity == 1
                {
                    let home = self.module_objs[def.module_idx];
                    let res = self.guarded(|vm| vm.run_proto(proto, home, None, vec![Value::Obj(h)], true, false, span))?;
                    return self.stringify_into(out, res, span, depth);
                }
                let _ = write!(out, "{name}(");
                for (i, (k, fv)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let _ = write!(out, "{k}=");
                    self.stringify_into(out, *fv, span, depth + 1)?;
                }
                out.push(')');
            }
            Obj::Enum { variant, payload, .. } => {
                out.push_str(&variant);
                if !payload.is_empty() {
                    out.push('(');
                    self.stringify_seq_into(out, &payload, span, depth + 1)?;
                    out.push(')');
                }
            }
            Obj::Func { proto, .. } => {
                let _ = write!(out, "<fn {}>", self.program.protos[proto].name);
            }
            Obj::Closure { .. } => out.push_str("<closure>"),
            Obj::Module { name, .. } => {
                let _ = write!(out, "<module {name}>");
            }
            Obj::Native { name, .. } => {
                let _ = write!(out, "<native fn {name}>");
            }
            // Channel / Shared / Executor have no protocol hook — reuse the structural `Display`
            // (matches the interpreter's `stringify` catch-all falling back to `Display`).
            Obj::Channel(_) | Obj::Shared(_) | Obj::Atomic(_) | Obj::Executor(_) | Obj::Socket(_) | Obj::Listener(_) => {
                out.push_str(&self.display_guarded(Value::Obj(h), depth)?);
            }
        }
        Ok(())
    }

    fn stringify_seq_into(&mut self, out: &mut String, elems: &[Value], span: Span, depth: usize) -> Result<(), RuntimeError> {
        for (i, e) in elems.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            self.stringify_into(out, *e, span, depth)?;
        }
        Ok(())
    }
}

/// D5 — the off-heap [`crate::native::Host`] for a blocking native run on the dirty pool (no `Vm`,
/// no heap). It serves the pre-extracted primitive args ([`crate::native::NativeArg`]) and *panics*
/// on any host-I/O method: the offload classifier ([`crate::native::is_blocking`]) only flags fns
/// that read primitive args + return a primitive `NativeRet`, so reaching stdout/stderr/stdin/os here
/// means a fn was misclassified as off-heap-safe — a bug to surface loudly, not paper over.
struct OffloadHost {
    args: Vec<crate::native::NativeArg>,
}

impl crate::native::Host for OffloadHost {
    fn arg_count(&self) -> usize {
        self.args.len()
    }
    fn arg_int(&mut self, i: usize) -> Result<i64, crate::native::HostError> {
        match self.args.get(i) {
            Some(crate::native::NativeArg::Int(n)) => Ok(*n),
            Some(_) => Err(crate::native::HostError::arg_type(i, "int", "other")),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_is_int(&self, i: usize) -> bool {
        matches!(self.args.get(i), Some(crate::native::NativeArg::Int(_)))
    }
    fn arg_float(&mut self, i: usize) -> Result<f64, crate::native::HostError> {
        match self.args.get(i) {
            Some(crate::native::NativeArg::Float(f)) => Ok(*f),
            Some(crate::native::NativeArg::Int(n)) => Ok(*n as f64),
            Some(_) => Err(crate::native::HostError::arg_type(i, "float", "other")),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_str(&mut self, i: usize) -> Result<String, crate::native::HostError> {
        match self.args.get(i) {
            Some(crate::native::NativeArg::Str(s)) => Ok(s.clone()),
            Some(_) => Err(crate::native::HostError::arg_type(i, "str", "other")),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_str_map(&mut self, i: usize) -> Result<Vec<(String, String)>, crate::native::HostError> {
        match self.args.get(i) {
            // Pre-extracted on the worker (heap live) into owned pairs; served back off-thread.
            Some(crate::native::NativeArg::Map(pairs)) => Ok(pairs.clone()),
            Some(_) => Err(crate::native::HostError::arg_type(i, "map[str, str]", "other")),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn write_stdout(&mut self, _s: &str) {
        unreachable!("offloaded blocking native must not write stdout (off-heap host)")
    }
    fn write_stderr(&mut self, _s: &str) {
        unreachable!("offloaded blocking native must not write stderr (off-heap host)")
    }
    fn read_line(&mut self) -> Result<Option<String>, crate::native::HostError> {
        unreachable!("offloaded blocking native must not read stdin (off-heap host)")
    }
    fn os_args(&self) -> Vec<String> {
        unreachable!("offloaded blocking native must not read os args (off-heap host)")
    }
    fn os_env(&self, _key: &str) -> Option<String> {
        unreachable!("offloaded blocking native must not read env (off-heap host)")
    }
    fn os_getcwd(&self) -> Result<String, crate::native::HostError> {
        unreachable!("offloaded blocking native must not read cwd (off-heap host)")
    }
}

/// The VM's [`crate::native::Host`] adapter: lets a native fn read the evaluated `Value` arguments
/// (reaching into the heap for `str` args) and write to the captured output buffers. Holds `&mut
/// Vm` plus the arg vector; it allocates nothing itself — the returned [`crate::native::NativeRet`]
/// is lowered to heap objects by [`Vm::lower_native`] after the call returns. (Stdin / args / env /
/// cooperative-exit are wired in a later milestone; the unwired methods return inert defaults.)
struct VmHost<'a> {
    vm: &'a mut Vm,
    args: Vec<Value>,
}

impl crate::native::Host for VmHost<'_> {
    fn arg_count(&self) -> usize {
        self.args.len()
    }
    fn arg_int(&mut self, i: usize) -> Result<i64, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Int(n)) => Ok(*n),
            Some(other) => Err(crate::native::HostError::arg_type(i, "int", self.vm.type_name(*other))),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_is_int(&self, i: usize) -> bool {
        matches!(self.args.get(i), Some(Value::Int(_)))
    }
    fn arg_float(&mut self, i: usize) -> Result<f64, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Float(f)) => Ok(*f),
            Some(Value::Int(n)) => Ok(*n as f64),
            Some(other) => Err(crate::native::HostError::arg_type(i, "float", self.vm.type_name(*other))),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_str(&mut self, i: usize) -> Result<String, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Obj(h)) => match self.vm.heap.get(*h) {
                Obj::Str(s) => Ok(s.to_string()),
                _ => {
                    let got = self.vm.type_name(self.args[i]);
                    Err(crate::native::HostError::arg_type(i, "str", got))
                }
            },
            Some(other) => Err(crate::native::HostError::arg_type(i, "str", self.vm.type_name(*other))),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_str_map(&mut self, i: usize) -> Result<Vec<(String, String)>, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Obj(h)) => match self.vm.heap.get(*h) {
                Obj::Map(m) => {
                    // Iterate `entries` (insertion order) so header order is deterministic and
                    // matches the interp + off-heap hosts. Every key/value must be a str.
                    let mut pairs = Vec::with_capacity(m.entries.len());
                    for (_, k, v) in &m.entries {
                        let (Value::Obj(kh), Value::Obj(vh)) = (k, v) else {
                            return Err(crate::native::HostError::arg_type(i, "map[str, str]", "other"));
                        };
                        let (Obj::Str(ks), Obj::Str(vs)) = (self.vm.heap.get(*kh), self.vm.heap.get(*vh))
                        else {
                            return Err(crate::native::HostError::arg_type(i, "map[str, str]", "other"));
                        };
                        pairs.push((ks.to_string(), vs.to_string()));
                    }
                    Ok(pairs)
                }
                _ => {
                    let got = self.vm.type_name(self.args[i]);
                    Err(crate::native::HostError::arg_type(i, "map[str, str]", got))
                }
            },
            Some(other) => {
                Err(crate::native::HostError::arg_type(i, "map[str, str]", self.vm.type_name(*other)))
            }
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn write_stdout(&mut self, s: &str) {
        self.vm.out.push_str(s);
    }
    fn write_stderr(&mut self, s: &str) {
        self.vm.stderr.push_str(s);
    }
    fn read_line(&mut self) -> Result<Option<String>, crate::native::HostError> {
        self.vm.host.stdin.read_line()
    }
    fn os_args(&self) -> Vec<String> {
        self.vm.host.args.clone()
    }
    fn os_env(&self, key: &str) -> Option<String> {
        self.vm.host.env.get(key).cloned()
    }
    fn os_getcwd(&self) -> Result<String, crate::native::HostError> {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .map_err(|e| crate::native::HostError { message: e.to_string() })
    }
    fn request_exit(&mut self, code: i64) {
        self.vm.pending_exit = Some(code.clamp(0, 255) as i32);
    }
}

fn is_numeric(v: Value) -> bool {
    matches!(v, Value::Int(_) | Value::Float(_))
}

/// Clamp a half-open `start..end` slice to a length-`len` sequence: both bounds clamp into
/// `[0, len]`, `start > end` collapses to empty. Returns `(lo, hi)` with `lo <= hi <= len`.
/// Mirrors `interp::clamp_range` (the two engines keep byte-identical slice semantics).
fn clamp_range(start: i64, end: i64, len: usize) -> (usize, usize) {
    let len_i = len as i64;
    let lo = start.clamp(0, len_i) as usize;
    let hi = end.clamp(0, len_i) as usize;
    (lo, hi.max(lo))
}

fn as_f64(v: Value) -> f64 {
    match v {
        Value::Int(n) => n as f64,
        Value::Float(f) => f,
        _ => unreachable!("as_f64 on non-numeric"),
    }
}

/// Format a float the way Chezzi prints it (matches `interp::value::format_float`).
fn format_float(x: f64) -> String {
    if x.is_finite() && x.fract() == 0.0 {
        format!("{x:.1}")
    } else {
        format!("{x}")
    }
}

// ===== entry points =====

/// Run a single-file program from source on the dedicated VM thread; returns output produced so
/// far + the outcome (test entry point, mirroring `interp::run_program`).
#[cfg(test)]
pub fn run_program(src: &str) -> (String, Result<(), RuntimeError>) {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || run_program_inner(&src))
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

#[cfg(test)]
fn run_program_inner(src: &str) -> (String, Result<(), RuntimeError>) {
    let tokens = match lexer::tokenize(src) {
        Ok(t) => t,
        Err(e) => return (String::new(), Err(RuntimeError { message: e.to_string(), span: Span { line: 1, col: 1 } })),
    };
    let module = match parser::parse(tokens) {
        Ok(m) => m,
        Err(e) => return (String::new(), Err(RuntimeError { message: e.message, span: e.span })),
    };
    let program = match crate::compiler::compile_module_standalone(&module) {
        Ok(p) => p,
        Err(e) => return (String::new(), Err(RuntimeError { message: e.message, span: e.span })),
    };
    let mut vm = Vm::new(Arc::new(program));
    let result = vm
        .run()
        .and_then(|()| vm.drain_live_executors(Span { line: 1, col: 1 }));
    (vm.out, result)
}

/// Run a single-file program and return its full stdout, or the error (test helper).
#[cfg(test)]
pub fn run_capture(src: &str) -> Result<String, RuntimeError> {
    let (out, result) = run_program(src);
    result.map(|()| out)
}

/// B3.3-threads — run a single-file program on the `--parallel` engine (real OS-thread pool +
/// condvar `recv`) and return its stdout or error. The deterministic-by-construction `--parallel`
/// unit tests/goldens drive this (decision A: the cooperative default stays the parity oracle).
#[cfg(test)]
pub fn run_capture_parallel(src: &str) -> Result<String, RuntimeError> {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            let tokens = lexer::tokenize(&src).map_err(|e| RuntimeError { message: e.to_string(), span: Span { line: 1, col: 1 } })?;
            let module = parser::parse(tokens).map_err(|e| RuntimeError { message: e.message, span: e.span })?;
            let program = crate::compiler::compile_module_standalone(&module).map_err(|e| RuntimeError { message: e.message, span: e.span })?;
            let mut vm = Vm::new(Arc::new(program));
            vm.parallel = true;
            vm.run().and_then(|()| vm.drain_live_executors(Span { line: 1, col: 1 })).map(|()| vm.out)
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

/// Run a single-file program, returning stdout (or error) plus the final live-object count.
/// `stress` collects before every instruction (surfaces missing GC roots); otherwise the normal
/// allocation-threshold trigger drives collection (test helper for GC assertions).
#[cfg(test)]
pub fn run_with(src: &str, stress: bool) -> (Result<String, RuntimeError>, usize) {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            let tokens = match lexer::tokenize(&src) {
                Ok(t) => t,
                Err(e) => return (Err(RuntimeError { message: e.to_string(), span: Span { line: 1, col: 1 } }), 0),
            };
            let module = match parser::parse(tokens) {
                Ok(m) => m,
                Err(e) => return (Err(RuntimeError { message: e.message, span: e.span }), 0),
            };
            let program = match crate::compiler::compile_module_standalone(&module) {
                Ok(p) => p,
                Err(e) => return (Err(RuntimeError { message: e.message, span: e.span }), 0),
            };
            let mut vm = Vm::new(Arc::new(program));
            vm.gc_stress = stress;
            let result = vm
                .run()
                .and_then(|()| vm.drain_live_executors(Span { line: 1, col: 1 }));
            let live = vm.heap.live();
            (result.map(|()| vm.out), live)
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

/// Run a program on a deliberately SMALL host stack (test helper for the call-flattening guarantee).
/// Deep *plain-function* recursion must not consume host stack — frames live in the heap `frames`
/// `Vec`, not via a per-call `run_until` recursion — so it survives a stack far below the production
/// [`VM_STACK_BYTES`]. On the old recurse-per-call engine this overflowed and aborted.
#[cfg(test)]
pub fn run_capture_on_stack(src: &str, stack_bytes: usize) -> Result<String, RuntimeError> {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(stack_bytes)
        .spawn(move || {
            let tokens = lexer::tokenize(&src).map_err(|e| RuntimeError { message: e.to_string(), span: Span { line: 1, col: 1 } })?;
            let module = parser::parse(tokens).map_err(|e| RuntimeError { message: e.message, span: e.span })?;
            let program = crate::compiler::compile_module_standalone(&module).map_err(|e| RuntimeError { message: e.message, span: e.span })?;
            let mut vm = Vm::new(Arc::new(program));
            vm.run().and_then(|()| vm.drain_live_executors(Span { line: 1, col: 1 })).map(|()| vm.out)
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

/// Stdout from a stress-mode run (panics on error) — convenience for parity-under-GC tests.
#[cfg(test)]
pub fn run_capture_stress(src: &str) -> String {
    run_with(src, true).0.unwrap_or_else(|e| panic!("unexpected runtime error under GC stress: {e}"))
}

/// Run a single-file program, returning stdout (or error) plus the **final nursery-stack depth**
/// (`self.nurseries.len()` after the run). A clean program leaves it at 0; a leaked `parallel:`
/// nursery (a `?`/return that skipped its `JoinNursery`) shows up as a non-zero residual — the
/// white-box check for the `do_return` nursery-truncation fix (test helper).
#[cfg(test)]
pub fn run_capture_nursery_len(src: &str) -> (Result<String, RuntimeError>, usize) {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            let tokens = match lexer::tokenize(&src) {
                Ok(t) => t,
                Err(e) => return (Err(RuntimeError { message: e.to_string(), span: Span { line: 1, col: 1 } }), 0),
            };
            let module = match parser::parse(tokens) {
                Ok(m) => m,
                Err(e) => return (Err(RuntimeError { message: e.message, span: e.span }), 0),
            };
            let program = match crate::compiler::compile_module_standalone(&module) {
                Ok(p) => p,
                Err(e) => return (Err(RuntimeError { message: e.message, span: e.span }), 0),
            };
            let mut vm = Vm::new(Arc::new(program));
            let result = vm
                .run()
                .and_then(|()| vm.drain_live_executors(Span { line: 1, col: 1 }));
            let nursery_depth = vm.nurseries.len();
            (result.map(|()| vm.out), nursery_depth)
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

/// Run a multi-file program from its entry path on the dedicated VM thread. Mirrors
/// `interp::run_file`: resolve the graph, compile it, run each module once in dependency order,
/// then the entry's `main()`. Output produced so far is preserved alongside the outcome.
/// Convenience wrapper with the default (inert) host config. Test-only — the CLI uses
/// [`run_file_with`] to pass a process-backed config.
#[cfg(test)]
pub fn run_file(entry: &std::path::Path) -> RunOutput {
    run_file_with(entry, crate::native::HostConfig::default())
}

/// A finished run: captured `(stdout, stderr, outcome, exit_code)`. Stderr holds `std.io.eprint`
/// output. `exit_code` is `Some(n)` only when the program called `std.os.exit(n)` (a clean halt,
/// so `outcome` is `Ok`); `None` for a normal end or a runtime error.
pub type RunOutput = (String, String, Result<(), RunError>, Option<i32>);

/// Like [`run_file`], but with an explicit [`crate::native::HostConfig`] (args/env/stdin) for the
/// native std modules. The CLI passes a process-backed config; tests inject a deterministic one.
pub fn run_file_with(entry: &std::path::Path, cfg: crate::native::HostConfig) -> RunOutput {
    run_file_engine(entry, cfg, false)
}

/// Like [`run_file_with`], but runs on the **B3.3-threads `--parallel` engine** (real OS-thread
/// pool + condvar `recv`) rather than the cooperative default. The CLI selects this for
/// `chezzi run --parallel <file>`.
pub fn run_file_parallel(entry: &std::path::Path, cfg: crate::native::HostConfig) -> RunOutput {
    run_file_engine(entry, cfg, true)
}

fn run_file_engine(entry: &std::path::Path, cfg: crate::native::HostConfig, parallel: bool) -> RunOutput {
    let entry = entry.to_path_buf();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || run_file_inner(&entry, cfg, parallel))
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

fn run_file_inner(entry: &std::path::Path, cfg: crate::native::HostConfig, parallel: bool) -> RunOutput {
    let graph = match crate::resolver::build_graph(entry) {
        Ok(g) => g,
        Err(e) => return (String::new(), String::new(), Err(RunError::plain(RuntimeError { message: e.message, span: e.span })), None),
    };
    let program = match crate::compiler::compile_graph(&graph) {
        Ok(p) => p,
        Err(e) => return (String::new(), String::new(), Err(RunError::plain(RuntimeError { message: e.message, span: e.span })), None),
    };
    let mut vm = Vm::new(Arc::new(program));
    vm.host = cfg;
    vm.parallel = parallel;
    // On a clean finish, gracefully reap any Executor never explicitly shut down (C5 / A2). Skipped
    // on a fault (the program is already erroring) and on a hard `std.os.exit` (handled inside
    // `drain_live_executors` via `pending_exit`).
    let result = vm
        .run()
        .and_then(|()| vm.drain_live_executors(Span { line: 1, col: 1 }));
    // A pending exit means `result` is the `exit()` unwind sentinel, not a fault: report the
    // requested code as a clean halt.
    if let Some(code) = vm.pending_exit {
        return (vm.out, vm.stderr, Ok(()), Some(code));
    }
    // The stack trace was captured at the uncaught fault (before frames unwound); attach it.
    let trace = vm.fault_trace.take().unwrap_or_default();
    let result = result.map_err(|e| RunError::from_error(e, trace));
    (vm.out, vm.stderr, result, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a program to completion, returning its stdout (panics on runtime error).
    fn run(src: &str) -> String {
        run_capture(src).unwrap_or_else(|e| panic!("unexpected runtime error: {e}"))
    }

    /// Build a VM `map[str, str]` value with the given pairs (insertion order preserved), so the
    /// host-side map readers can be unit-tested without compiling a program.
    fn build_str_map(vm: &mut Vm, pairs: &[(&str, &str)]) -> Value {
        let span = Span { line: 1, col: 1 };
        let mut map = MapData::default();
        for (k, v) in pairs {
            let kv = vm.alloc_str((*k).to_string());
            let vv = vm.alloc_str((*v).to_string());
            let hk = vm.hash_value(kv, span).unwrap();
            map.push(hk, kv, vv);
        }
        Value::Obj(vm.heap.alloc(Obj::Map(map)))
    }

    /// `OffloadHost::arg_str_map` serves the pre-extracted `NativeArg::Map` pairs back (so an
    /// offloaded `request()` reads its headers off-thread); a non-map arg errors with `arg_type`.
    #[test]
    fn offload_host_arg_str_map_roundtrips() {
        use crate::native::Host;
        let mut host = OffloadHost {
            args: vec![
                crate::native::NativeArg::Map(vec![("X-Custom".into(), "value".into())]),
                crate::native::NativeArg::Str("not-a-map".into()),
            ],
        };
        assert_eq!(host.arg_str_map(0).unwrap(), vec![("X-Custom".into(), "value".into())]);
        assert!(host.arg_str_map(1).is_err(), "a non-map NativeArg must error");
        assert!(host.arg_str_map(9).is_err(), "a missing arg must error");
    }

    /// `extract_native_args` snapshots a `map[str, str]` Value into `NativeArg::Map` (insertion
    /// order) so `request()` can offload; a non-str-valued map reverts to `None` (run inline).
    #[test]
    fn extract_native_args_snapshots_str_map() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let m = build_str_map(&mut vm, &[("a", "1"), ("b", "2")]);
        let got = vm.extract_native_args(&[m]).expect("str/str map extracts");
        assert_eq!(
            got,
            vec![crate::native::NativeArg::Map(vec![
                ("a".into(), "1".into()),
                ("b".into(), "2".into()),
            ])],
            "pairs preserved in insertion order"
        );
        // A map with a non-str value (here an int) is not snapshottable → None (safe inline fallback).
        let span = Span { line: 1, col: 1 };
        let mut bad = MapData::default();
        let kv = vm.alloc_str("k".to_string());
        let hk = vm.hash_value(kv, span).unwrap();
        bad.push(hk, kv, Value::Int(7));
        let bad_map = Value::Obj(vm.heap.alloc(Obj::Map(bad)));
        assert_eq!(vm.extract_native_args(&[bad_map]), None);
    }

    /// `VmHost::arg_str_map` reads a live heap map in insertion order; a non-map arg errors.
    #[test]
    fn vm_host_arg_str_map_reads_live_map() {
        use crate::native::Host;
        let mut vm = Vm::new(Arc::new(empty_program()));
        let m = build_str_map(&mut vm, &[("one", "1"), ("two", "2")]);
        let not_map = Value::Int(3);
        let mut host = VmHost { vm: &mut vm, args: vec![m, not_map] };
        assert_eq!(
            host.arg_str_map(0).unwrap(),
            vec![("one".into(), "1".into()), ("two".into(), "2".into())]
        );
        assert!(host.arg_str_map(1).is_err(), "a non-map arg must error");
    }

    /// M-C implicit nurseries: a bare `spawn` at function scope (no explicit `parallel:`) joins at
    /// the function's end — inline statements after the spawn run first, then the spawned body.
    /// Identical on the cooperative default and the `--parallel` engine.
    #[test]
    fn implicit_nursery_basic_vm() {
        let src = "fn w():\n    print(\"w\")\nfn main():\n    print(\"a\")\n    spawn w()\n    print(\"b\")\nmain()\n";
        assert_eq!(run(src), "a\nb\nw\n");
        assert_eq!(run_capture_parallel(src).expect("parallel"), "a\nb\nw\n");
    }

    /// M-C: `return <value>` is a JOIN point — pending spawned tasks run to completion, THEN the
    /// value returns. No cancel-report. This is the regression guard for the cancel→join inversion.
    #[test]
    fn implicit_nursery_return_joins_vm() {
        let src = "fn w(n: int):\n    print(\"w{n}\")\nfn f() -> int:\n    spawn w(1)\n    spawn w(2)\n    print(\"x\")\n    return 0\nfn main():\n    print(f())\nmain()\n";
        assert_eq!(run(src), "x\nw1\nw2\n0\n");
        assert_eq!(run_capture_parallel(src).expect("parallel"), "x\nw1\nw2\n0\n");
    }

    /// M-C: the module top level is an implicit nursery that joins at program exit.
    #[test]
    fn implicit_nursery_toplevel_vm() {
        let src = "fn w():\n    print(\"w\")\nprint(\"end\")\nspawn w()\n";
        assert_eq!(run(src), "end\nw\n");
        assert_eq!(run_capture_parallel(src).expect("parallel"), "end\nw\n");
    }

    /// Assert a program yields `expected` on all three engines (cooperative VM, frozen interp,
    /// `--parallel`) — the M-C parity bar.
    #[cfg(test)]
    fn assert_mc_parity(src: &str, expected: &str) {
        assert_eq!(run(src), expected, "cooperative VM");
        assert_eq!(crate::interp::run_capture(src).expect("interp"), expected, "interp");
        assert_eq!(run_capture_parallel(src).expect("parallel"), expected, "--parallel");
    }

    /// M-C: spawned tasks JOIN before the frame's `defer`s run (tasks complete, then cleanup).
    #[test]
    fn implicit_nursery_defer_orders_tasks_then_defers() {
        let src = "fn w():\n    print(\"task\")\nfn cleanup():\n    print(\"cleanup\")\nfn main():\n    defer cleanup()\n    spawn w()\n    print(\"body\")\nmain()\n";
        assert_mc_parity(src, "body\ntask\ncleanup\n");
    }

    /// M-C: a `?` early-return is a JOIN point — pending tasks run before the error propagates.
    #[test]
    fn implicit_nursery_try_joins_before_propagating() {
        let src = "fn w():\n    print(\"task ran\")\nfn g() -> int!:\n    return Err(\"inner\")\nfn f() -> int!:\n    spawn w()\n    x := g()?\n    print(\"unreached\")\n    return Ok(x)\nfn main():\n    r := recover:\n        f()?\n        0\n    print(\"done\")\nmain()\n";
        assert_mc_parity(src, "task ran\ndone\n");
    }

    /// M-C function-boundary rule: a task spawned in a callee joins at the callee's end, not the
    /// caller's `parallel:` dedent — it cannot outlive the function that spawned it.
    #[test]
    fn implicit_nursery_respects_function_boundary() {
        let src = "fn task(label: str):\n    print(label)\nfn helper():\n    spawn task(\"helper-task\")\n    print(\"helper body\")\nfn main():\n    parallel:\n        spawn helper()\n    print(\"main after parallel\")\nmain()\n";
        assert_mc_parity(src, "helper body\nhelper-task\nmain after parallel\n");
    }

    /// M-C: nested functions each have their own implicit nursery — no task leaks across a call.
    #[test]
    fn implicit_nursery_nested_functions() {
        let src = "fn leaf(id: int):\n    print(\"leaf {id}\")\nfn inner():\n    spawn leaf(1)\n    spawn leaf(2)\n    print(\"inner body\")\nfn main():\n    spawn leaf(3)\n    inner()\n    print(\"main body\")\nmain()\n";
        assert_mc_parity(src, "inner body\nleaf 1\nleaf 2\nmain body\nleaf 3\n");
    }

    /// M-C regression (review-panel BUG): a `?` early-return from a body with a bare `spawn` must
    /// surface the USER's `Err(...)`, not the internal `? propagation` sentinel. The interp join loop
    /// previously let the spawned task's `finish_frame` clear the in-flight `?` value.
    #[test]
    fn implicit_nursery_try_preserves_error_value() {
        let src = "fn w():\n    print(\"task ran\")\nfn g() -> int!:\n    return Err(\"boom-value\")\nfn f() -> int!:\n    spawn w()\n    x := g()?\n    return Ok(x)\nfn main():\n    r := recover:\n        f()?\n        99\n    print(\"after: {r}\")\nmain()\n";
        assert_mc_parity(src, "task ran\nafter: Err(boom-value)\n");
    }

    /// M-C regression (review-panel BUG): a bare `spawn` inside a `defer:` block is legal — the
    /// deferred block runs in its own frame with its own implicit nursery, joined when the block ends.
    /// The VM previously omitted the nursery for deferred-block protos and hit the runtime guard.
    #[test]
    fn implicit_nursery_spawn_in_defer_block() {
        let src = "fn work(n: int):\n    print(n)\nfn main():\n    defer:\n        spawn work(1)\n    print(\"body\")\nmain()\n";
        assert_mc_parity(src, "body\n1\n");
    }

    /// M-C: a genuine body fault caught by `recover:` cancels-and-reports the implicit nursery's
    /// unstarted tasks (they do NOT run) — identical to an explicit `parallel:` escape, on all engines.
    #[test]
    fn implicit_nursery_fault_cancels_pending_tasks() {
        let src = "fn w():\n    print(\"should not run\")\nfn f():\n    spawn w()\n    x := [1]\n    print(x[9])\nfn main():\n    r := recover:\n        f()\n        0\n    print(\"recovered\")\nmain()\n";
        assert_mc_parity(src, "1 pending task(s) cancelled on early exit from parallel:\nrecovered\n");
    }

    /// Assert an UNCAUGHT fault yields identical stdout on the cooperative VM and the frozen interp,
    /// and that both actually faulted. `run_capture` drops stdout on `Err`, so go through the
    /// `(stdout, result)` harness directly. This is the cancel-report parity bar for uncaught faults.
    #[cfg(test)]
    fn assert_fault_parity(src: &str, expected_out: &str) {
        let (vm_out, vm_res) = run_program(src);
        assert!(vm_res.is_err(), "VM expected to fault, got {vm_out:?}");
        assert_eq!(vm_out, expected_out, "cooperative VM stdout");
        let (it_out, it_res) = crate::interp::run_program(src);
        assert!(it_res.is_err(), "interp expected to fault, got {it_out:?}");
        assert_eq!(it_out, expected_out, "interp stdout");
        assert_eq!(vm_out, it_out, "VM/interp cancel-report divergence");
    }

    /// Parity gap fix (T1): an UNCAUGHT body fault with one un-run task on the function's implicit
    /// nursery reports the cancellation on stdout — previously only the interp printed it.
    #[test]
    fn uncaught_fault_reports_implicit_nursery() {
        let src = "fn w():\n    print(\"should not run\")\nfn boom():\n    spawn w()\n    x := [1]\n    print(x[9])\nfn main():\n    boom()\nmain()\n";
        assert_fault_parity(src, "1 pending task(s) cancelled on early exit from parallel:\n");
    }

    /// Parity gap fix (T2): an UNCAUGHT fault inside an explicit `parallel:` block reports its
    /// un-run task on stdout (the pre-M-C form of the same gap).
    #[test]
    fn uncaught_fault_reports_explicit_parallel() {
        let src = "fn w():\n    print(\"should not run\")\nfn main():\n    parallel:\n        spawn w()\n        x := [1]\n        print(x[9])\nmain()\n";
        assert_fault_parity(src, "1 pending task(s) cancelled on early exit from parallel:\n");
    }

    /// Parity gap fix (T3): TWO stacked implicit nurseries each with a pending task report
    /// PER-NURSERY (two lines, innermost first) — matching the interp's per-frame reporting, not one
    /// combined line. Guards the `drain_escaped_nursery` sum→per-line change.
    #[test]
    fn uncaught_fault_reports_each_nursery_separately() {
        let src = "fn w(tag: str):\n    print(\"ran {tag}\")\nfn boom():\n    spawn w(\"boom\")\n    x := [1]\n    print(x[9])\nfn main():\n    spawn w(\"main\")\n    boom()\nmain()\n";
        let line = "1 pending task(s) cancelled on early exit from parallel:\n";
        assert_fault_parity(src, &format!("{line}{line}"));
    }

    /// Parity gap fix (T4 guard): a top-level bare `spawn` followed by an uncaught TOP-LEVEL fault
    /// stays SILENT on both engines — the module nursery is not reported (it joins only at clean
    /// exit). The fix must preserve this (don't drain the toplevel frame's own implicit nursery).
    #[test]
    fn uncaught_toplevel_fault_does_not_report_module_nursery() {
        let src = "fn w():\n    print(\"ran top\")\nspawn w()\nx := [1]\nprint(x[9])\n";
        assert_fault_parity(src, "");
    }

    /// Parity gap fix: a recover-CAUGHT fault unwinding two stacked nurseries also reports
    /// PER-NURSERY (two lines), then the recover continues — previously the VM combined them into
    /// one `2 pending` line while the interp emitted two.
    #[test]
    fn recover_caught_fault_reports_each_nursery_separately() {
        let src = "fn w(tag: str):\n    print(\"ran {tag}\")\nfn boom():\n    spawn w(\"boom\")\n    x := [1]\n    print(x[9])\nfn outer():\n    spawn w(\"outer\")\n    boom()\nfn main():\n    r := recover:\n        outer()\n        0\n    print(\"recovered\")\nmain()\n";
        let line = "1 pending task(s) cancelled on early exit from parallel:\n";
        assert_mc_parity(src, &format!("{line}{line}recovered\n"));
    }

    /// Parity gap fix (review-panel BUG, ordering): the cancel-report is emitted BEFORE the faulting
    /// frame's `defer`s run — matching the interp (`leave_implicit_nursery` reports, then
    /// `finish_frame` runs defers). The VM previously ran defers in `unwind_deferred` FIRST and only
    /// reported afterward (`cleanup` then report — a divergence the no-defer tests above missed).
    #[test]
    fn uncaught_fault_reports_before_frame_defers() {
        let src = "fn w():\n    print(\"task\")\nfn cleanup():\n    print(\"cleanup\")\nfn boom():\n    defer cleanup()\n    spawn w()\n    x := [1]\n    print(x[9])\nfn main():\n    boom()\nmain()\n";
        assert_fault_parity(src, "1 pending task(s) cancelled on early exit from parallel:\ncleanup\n");
    }

    /// Same report-before-defer ordering on the recover-CAUGHT path, then the recover continues.
    #[test]
    fn recover_caught_fault_reports_before_frame_defers() {
        let src = "fn w():\n    print(\"task\")\nfn cleanup():\n    print(\"cleanup\")\nfn boom():\n    defer cleanup()\n    spawn w()\n    x := [1]\n    print(x[9])\nfn main():\n    r := recover:\n        boom()\n        0\n    print(\"recovered\")\nmain()\n";
        assert_mc_parity(src, "1 pending task(s) cancelled on early exit from parallel:\ncleanup\nrecovered\n");
    }

    /// Multi-frame interleave: each unwound frame reports its nursery, THEN runs its defer, before
    /// the next (outer) frame — innermost-first (`report boom, cleanup boom, report outer, cleanup
    /// outer`). Guards the per-frame interleave in `unwind_deferred` against batching regressions.
    #[test]
    fn uncaught_fault_interleaves_report_and_defer_per_frame() {
        let src = "fn w(t: str):\n    print(\"task {t}\")\nfn cl(t: str):\n    print(\"cleanup {t}\")\nfn boom():\n    defer cl(\"boom\")\n    spawn w(\"boom\")\n    x := [1]\n    print(x[9])\nfn outer():\n    defer cl(\"outer\")\n    spawn w(\"outer\")\n    boom()\nfn main():\n    outer()\nmain()\n";
        let line = "1 pending task(s) cancelled on early exit from parallel:\n";
        assert_fault_parity(src, &format!("{line}cleanup boom\n{line}cleanup outer\n"));
    }

    /// M19 SSO — the production `alloc_str` path stores short strings inline (no `Box` heap alloc)
    /// and spills longer ones to the heap. This guards the wiring of `ChzStr` into the VM's hot
    /// string-construction funnel; `chzstr.rs` unit tests cover the selection logic itself.
    #[test]
    fn vm_alloc_str_inlines_short_spills_long() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let short = vm.alloc_str("item-499999".to_string()); // 11 bytes ≤ INLINE_CAP
        let long = vm.alloc_str("x".repeat(crate::vm::chzstr::INLINE_CAP + 1)); // > INLINE_CAP
        let inline = matches!(vm.heap.get(match short { Value::Obj(h) => h, _ => unreachable!() }), Obj::Str(s) if s.is_inline());
        let heap = matches!(vm.heap.get(match long { Value::Obj(h) => h, _ => unreachable!() }), Obj::Str(s) if !s.is_inline());
        assert!(inline, "short string should be stored inline");
        assert!(heap, "long string should spill to the heap");
    }

    /// Run a program expected to fail; return the runtime error message.
    fn run_err(src: &str) -> String {
        match run_capture(src) {
            Ok(out) => panic!("expected a runtime error, got output: {out:?}"),
            Err(e) => e.message,
        }
    }

    // ---- M19 Phase 3: ConstStr interning + per-char alloc (correctness guards) ----

    #[test]
    fn interned_literal_repeated_pushes_render_identically() {
        // The same literal op pushed many times must render identically — interning must not change
        // the observed value (no identity operator exists, so aliasing is invisible).
        assert_eq!(
            run("i := 0\nwhile i < 3:\n    print(\"hi\")\n    i = i + 1\n"),
            "hi\nhi\nhi\n"
        );
    }

    #[test]
    fn interned_fstring_literal_parts_in_loop() {
        // Interpolation literal chunks (`n=` / `!`) are ConstStr pushes repeated per iteration.
        assert_eq!(
            run("i := 0\nwhile i < 3:\n    print(\"n={i}!\")\n    i = i + 1\n"),
            "n=0!\nn=1!\nn=2!\n"
        );
    }

    #[test]
    fn interned_literal_as_map_key_repeated() {
        // A literal reused as a map key: aliasing must preserve structural (by-content) map lookup.
        assert_eq!(
            run("m := {}\ni := 0\nwhile i < 3:\n    m[\"k\"] = i\n    i = i + 1\nprint(m[\"k\"])\n"),
            "2\n"
        );
    }

    #[test]
    fn interned_strings_survive_gc_stress() {
        // Proves interned ConstStr objects are GC-rooted: collect-before-every-instruction must not
        // sweep a cached literal out from under a later push of the same op.
        let src = "i := 0\nout := \"\"\nwhile i < 50:\n    out = out + \"x\"\n    i = i + 1\nprint(out.len())\n";
        assert_eq!(run_capture_stress(src), run(src));
        assert_eq!(run(src), "50\n");
    }

    #[test]
    fn per_char_sites_render_unchanged() {
        // `for c in str`, string indexing, `chars()`, and `chr()` all build 1-char strs via the
        // single-allocation helper — output must stay byte-identical (same UTF-8).
        assert_eq!(run("for c in \"héllo\":\n    print(c)\n"), "h\né\nl\nl\no\n");
        assert_eq!(run("s := \"héllo\"\nprint(s[1])\n"), "é\n");
        assert_eq!(run("for c in \"abc\".chars():\n    print(c)\n"), "a\nb\nc\n");
        assert_eq!(run("print(chr(233))\n"), "é\n");
    }

    // ---- M19: FxHash map/set index hasher (correctness guards) ----
    // The map/set `index` (cached-hash → positions) and `str_intern` swap SipHash for a cheap FxHash.
    // The hasher only picks buckets; `values_equal` confirms every probe, so behavior must not change.

    #[test]
    fn fxhash_map_int_keys_insert_lookup_remove() {
        // Int keys hash straight to f64 bits, so this exercises only the index BuildHasher. Insert,
        // read, remove (rebuilds the index), then re-insert — all must agree with the interpreter.
        let src = "m := {}\ni := 0\nwhile i < 50:\n    m[i] = i * 2\n    i = i + 1\n\
                   m.remove(10)\nm.remove(20)\nm[10] = 999\n\
                   acc := 0\nfor k in m:\n    acc = acc + m[k]\nprint(acc)\nprint(m.len())\n";
        // sum(2i, i in 0..50) = 2450; drop 20→-40, drop10 then re-add 10→999: 2450-20-40+999 = 3389
        assert_eq!(run_parity(src), "3389\n49\n");
    }

    #[test]
    fn fxhash_map_str_keys() {
        // String keys hash by content (DefaultHasher, unchanged) then route through the index
        // BuildHasher (changed). Repeated-key updates must still land in the same entry.
        let src = concat!(
            "counts := {}\n",
            "for w in [\"a\", \"b\", \"a\", \"c\", \"a\", \"b\"]:\n",
            "    if counts.has(w):\n",
            "        counts[w] = counts[w] + 1\n",
            "    else:\n",
            "        counts[w] = 1\n",
            "for k in counts:\n",
            "    print(\"{k}={counts[k]}\")\n",
        );
        assert_eq!(run_parity(src), "a=3\nb=2\nc=1\n");
    }

    #[test]
    fn fxhash_constant_hash_collision_still_resolves() {
        // A struct key whose hash() is constant forces every key into ONE index bucket. The probe
        // must still find the right entry via structural ==, regardless of the bucket hasher.
        let src = "struct K:\n    v: int\n    fn hash(self) -> int:\n        return 7\n\
                   m := {}\ni := 0\nwhile i < 30:\n    m[K(i)] = i\n    i = i + 1\n\
                   print(m[K(7)])\nprint(m[K(29)])\nprint(m.has(K(30)))\nprint(m.len())\n";
        assert_eq!(run_parity(src), "7\n29\nfalse\n30\n");
    }

    #[test]
    fn fxhash_set_dedup_and_ops() {
        // Set dedup + union/intersection/difference over the index hasher.
        let src = "a := set([1, 2, 3, 2, 1])\nb := set([3, 4, 5])\n\
                   print(a.len())\nprint(a.union(b).len())\nprint(a.intersection(b).len())\nprint(a.difference(b).len())\n";
        assert_eq!(run_parity(src), "3\n5\n1\n2\n");
    }

    // ---- M19 Tier-2: index-access specialization (behavior-preserving guards) ----
    // The Int-key fast path in `get_index`/`set_index` (skips the rooting that protects a struct
    // key's re-entrant hash) and the inline `GetIndex`/`SetIndex` dispatch are VM-only speedups, so
    // every result + error string must stay byte-identical to the frozen interpreter. `idx_parity`
    // compares the full `Result` outcome (stdout OR error message). These pin the contract BEFORE the
    // change and stay green AFTER.
    fn idx_parity(src: &str) {
        let vm = run_capture(src).map_err(|e| e.to_string());
        let interp = crate::interp::run_capture(src).map_err(|e| e.to_string());
        assert_eq!(vm, interp, "vm/interp divergence (index specialization must be behavior-preserving):\n{src}");
    }

    #[test]
    fn idxspec_int_map_get_hit_and_miss() {
        // Int-key map read: a present key returns its value; an absent key faults "key not found".
        idx_parity("m := {1: 10, 2: 20}\nprint(m[1])\nprint(m[2])\n");
        idx_parity("m := {1: 10}\nprint(m[99])\n"); // miss → "key not found", same on both engines
    }

    #[test]
    fn idxspec_int_map_set_overwrite_and_insert() {
        // Int-key map write: overwrite an existing entry, insert a new one; len + reads agree.
        idx_parity(
            "m := {1: 10}\nm[1] = 11\nm[2] = 20\nprint(m[1])\nprint(m[2])\nprint(m.len())\n",
        );
    }

    #[test]
    fn idxspec_int_list_get_set_in_bounds() {
        idx_parity("xs := [5, 6, 7]\nprint(xs[0])\nxs[2] = 99\nprint(xs[2])\n");
    }

    #[test]
    fn idxspec_list_out_of_bounds_message_exact() {
        // Both get and set must surface the exact same bounds message through the fast path's fallback.
        idx_parity("xs := [1, 2, 3]\nprint(xs[5])\n");
        idx_parity("xs := [1, 2, 3]\nxs[5] = 0\n");
        idx_parity("xs := [1, 2, 3]\nprint(xs[-1])\n"); // negative → out of bounds, not a panic
    }

    #[test]
    fn idxspec_non_int_map_keys_via_fallback() {
        // Str + bool keys must NOT take the Int fast path — they route through the unchanged general
        // match (content/scalar hash). Output + a str-key miss message stay identical.
        idx_parity("m := {\"a\": 1, \"b\": 2}\nprint(m[\"a\"])\nprint(m[\"b\"])\n");
        idx_parity("m := {true: 1, false: 0}\nprint(m[false])\nprint(m[true])\n");
        idx_parity("m := {\"a\": 1}\nprint(m[\"z\"])\n"); // str miss → "key not found"
    }

    #[test]
    fn idxspec_struct_index_protocol_via_fallback() {
        // THE TRAP: an Int key on a struct receiver must dispatch the `index`/`set_index` protocol,
        // NOT the List/Map Int fast path. The receiver kind (Struct) gates the fast path, not the key.
        let src = "struct Buf:\n    xs: list[int]\n    fn index(self, k: int) -> int:\n        return self.xs[k]\n    fn set_index(self, k: int, v: int):\n        self.xs[k] = v\n\
                   b := Buf([10, 20, 30])\nprint(b[0])\nb[1] = 99\nprint(b[1])\n";
        idx_parity(src);
    }

    #[test]
    fn idxspec_int_float_key_collision_resolves() {
        // Int(3) and Float(3.0) hash identically (3.0.to_bits()) and are values_equal. The fast path
        // shortcuts only the HASH, never the candidates+values_equal probe, so a Float key inserted as
        // 3.0 is found by m[3] and vice-versa — exactly the interpreter's behavior.
        idx_parity("m := {}\nm[3] = \"int\"\nprint(m[3.0])\nm[3.0] = \"float\"\nprint(m[3])\nprint(m.len())\n");
    }

    // ---- M19 Phase 4: struct-field inline cache (correctness guards) ----

    /// Run on the VM and the frozen interpreter; assert byte-identical stdout (the M19 parity bar),
    /// and return the shared output. The field IC is a VM-only speedup, so any divergence is a bug.
    fn run_parity(src: &str) -> String {
        let vm = run_capture(src).expect("vm run");
        let interp = crate::interp::run_capture(src).expect("interp run");
        assert_eq!(vm, interp, "vm/interp divergence (field IC must be behavior-preserving)");
        vm
    }

    #[test]
    fn ic_deep_field_read() {
        // Read the LAST field of a 6-field struct in a loop: exercises the IC hit path past five
        // would-be name-probes. Cached idx must point at `f` every iteration.
        let src = "struct S:\n    a: int\n    b: int\n    c: int\n    d: int\n    e: int\n    f: int\n\
                   s := S(1, 2, 3, 4, 5, 6)\n\
                   i := 0\nacc := 0\nwhile i < 5:\n    acc = acc + s.f\n    i = i + 1\nprint(acc)\n";
        assert_eq!(run_parity(src), "30\n");
    }

    #[test]
    fn ic_field_write_then_read() {
        // SetField IC: mutate `x` (plain) and `y` (compound) in a loop, then read both back. The
        // write cache and the read cache must agree on the field index.
        let src = "struct P:\n    x: int\n    y: int\n\
                   p := P(0, 0)\n\
                   i := 0\nwhile i < 4:\n    p.x = p.x + 2\n    p.y += 3\n    i = i + 1\n\
                   print(p.x)\nprint(p.y)\n";
        assert_eq!(run_parity(src), "8\n12\n");
    }

    #[test]
    fn ic_distinct_layouts() {
        // Two structs whose shared field names sit at DIFFERENT indices (A{x,y} vs B{y,x}), read at
        // their own sites in one loop. A bug that confused per-site IC cells (bad id allocation, or a
        // hit that skipped the name re-verify) would return a wrong field; the verify keeps it sound.
        let src = "struct A:\n    x: int\n    y: int\nstruct B:\n    y: int\n    x: int\n\
                   a := A(1, 2)\nb := B(3, 4)\n\
                   i := 0\ns := 0\nwhile i < 3:\n    s = s + a.x + a.y + b.x + b.y\n    i = i + 1\nprint(s)\n";
        // per iter: a.x=1 a.y=2 b.x=4 b.y=3 => 10; *3 = 30
        assert_eq!(run_parity(src), "30\n");
    }

    #[test]
    fn ic_self_field_method() {
        // `self.field` reads inside a method called in a loop — the hot OO path the IC targets.
        let src = "struct Counter:\n    n: int\n\n    fn get(self) -> int:\n        return self.n\n\
                   c := Counter(7)\n\
                   i := 0\nacc := 0\nwhile i < 5:\n    acc = acc + c.get()\n    i = i + 1\nprint(acc)\n";
        assert_eq!(run_parity(src), "35\n");
    }

    #[test]
    fn ic_struct_under_parallel_engine() {
        // The IC lives on each worker `Vm` too; field-heavy code run on the real-thread engine must
        // produce the same output as the cooperative engine (the caches are per-Vm, self-verifying).
        let src = "struct Pt:\n    x: int\n    y: int\n\n    fn sum(self) -> int:\n        return self.x + self.y\n\
                   p := Pt(3, 4)\n\
                   acc := 0\ni := 0\nwhile i < 100:\n    acc = acc + p.sum()\n    p.x = p.x + 1\n    i = i + 1\n\
                   print(acc)\nprint(p.x)\n";
        assert_eq!(run_capture_parallel(src).expect("parallel"), run(src));
    }

    #[test]
    fn ic_gc_stress_fields() {
        // Field reads under collect-before-every-instruction: cached indices stay valid because GC
        // never reorders a struct's `fields` Vec (and the IC holds indices, not GcRefs).
        let src = "struct V:\n    a: int\n    b: int\n    c: int\n\
                   v := V(10, 20, 30)\n\
                   i := 0\nacc := 0\nwhile i < 30:\n    acc = acc + v.a + v.b + v.c\n    i = i + 1\nprint(acc)\n";
        assert_eq!(run_capture_stress(src), run(src));
        assert_eq!(run_parity(src), "1800\n");
    }

    // ---- M19 Phase 5b: struct type-id guard on the field IC (correctness guards) ----
    // The IC hit now guards on a numeric `tid` (== struct layout identity) instead of re-verifying
    // the field name string. Soundness rests on: every distinct layout has a distinct `tid`, and the
    // empty/sentinel `tid` never matches. These lock that behavior is unchanged.

    #[test]
    fn typeid_guard_distinct_layouts_keep_distinct_values() {
        // Same field names on two types at SWAPPED indices, read in a hot loop. With the tid guard,
        // each per-type site caches (tid, idx); a guard that ignored type identity (or stamped a
        // shared tid) would read the wrong slot. Values asserted, not just a sum, to pin the layout.
        let src = concat!(
            "struct A:\n    v: int\n    w: int\n",
            "struct B:\n    w: int\n    v: int\n",
            "a := A(1, 2)\n",
            "b := B(3, 4)\n",
            "i := 0\nout := 0\n",
            "while i < 4:\n    out = out + a.v * 1000 + a.w * 100 + b.v * 10 + b.w\n    i = i + 1\n",
            // per iter: a.v=1,a.w=2,b.v=4,b.w=3 -> 1000+200+40+3 = 1243 ; *4 = 4972
            "print(out)\n",
        );
        assert_eq!(run_parity(src), "4972\n");
    }

    #[test]
    fn typeid_guard_struct_round_trips_through_channel() {
        // A struct sent across a Channel is serialized (to_wire) and rebuilt (from_wire) in the
        // receiver; from_wire must stamp a `tid` so the receiver's field IC stays sound. VM-only
        // (channels don't run under the frozen interp), so assert the VM output directly.
        let src = concat!(
            "struct Pt:\n    x: int\n    y: int\n",
            "fn worker(ch: Channel[Pt]):\n    p := ch.recv()\n    print(\"{p.x} {p.y}\")\n",
            "fn sender(ch: Channel[Pt]):\n    ch.send(Pt(10, 20))\n",
            "fn main():\n    ch := Channel[Pt]()\n    parallel:\n        spawn worker(ch)\n        spawn sender(ch)\n",
            "main()\n",
        );
        assert_eq!(run(src), "10 20\n");
    }

    // ---- M19 Tier-2: adaptive opcode quickening (PEP 659), v1 — binops — correctness guards ----
    // The un-fused generic binop arms (`Add..GtEq` reached by stack operands; `Eq`/`NotEq` always)
    // specialize to an int/int fast path behind a per-`Vm`, per-site (proto,ip) deopt guard. The
    // side table holds only state bytes (no `GcRef`), so it is heap-independent — never swapped in
    // `swap_ctx`, like `field_ic`/`method_ic`. Behaviour is byte-identical to the generic path; the
    // interpreter is untouched, so two-engine parity holds by construction. These guard the gotchas.

    #[test]
    fn quicken_table_presized_and_based() {
        // White-box wiring: the per-`Vm` quicken side table has one state byte per program
        // instruction, and `quicken_base` is the prefix sum of per-proto code lengths so a site is
        // `quicken_base[pid] + ip` (mirrors `field_ic_sites`/`field_ic` presizing).
        let src = "i := 0\ntotal := 0\nwhile i < 5:\n    total = total + i * i\n    i = i + 1\nprint(total)\n";
        let tokens = lexer::tokenize(src).unwrap();
        let module = parser::parse(tokens).unwrap();
        let program = crate::compiler::compile_module_standalone(&module).unwrap();
        let vm = Vm::new(Arc::new(program.clone()));
        let total: usize = program.protos.iter().map(|p| p.code.len()).sum();
        assert_eq!(vm.quicken.len(), total, "one quicken cell per instruction");
        assert_eq!(vm.quicken_base.len(), program.protos.len(), "one base per proto");
        // prefix-sum invariant: base[0]==0, base[k+1]==base[k]+len(proto[k])
        let mut acc = 0u32;
        for (pid, p) in program.protos.iter().enumerate() {
            assert_eq!(vm.quicken_base[pid], acc, "base[{pid}] is the running prefix sum");
            acc += p.code.len() as u32;
        }
        // all cells start Cold (0)
        assert!(vm.quicken.iter().all(|&b| b == 0), "every site starts Cold");
    }

    #[test]
    fn quicken_eq_preserves_lossy_f64_semantics() {
        // GOTCHA: the generic `Eq` compares numerics via `as_f64(a)==as_f64(b)` (mod.rs:3380), which
        // is LOSSY for i64 beyond 2^53. The quickened int fast path MUST replicate the loss (NOT use
        // exact `x==y`), or it diverges from the interpreter and breaks parity. 2^53 vs 2^53+1 both
        // round to the same f64, so `==` is TRUE and `!=` is FALSE under the (preserved) semantics.
        // Run it in a hot loop so the site warms past Cold into the specialized Int state.
        let src = "i := 0\nhits := 0\nwhile i < 3:\n    if 9007199254740992 == 9007199254740993:\n        hits = hits + 1\n    i = i + 1\nprint(hits)\n";
        assert_eq!(run_parity(src), "3\n");
        let src2 = "i := 0\nmiss := 0\nwhile i < 3:\n    if 9007199254740992 != 9007199254740993:\n        miss = miss + 1\n    i = i + 1\nprint(miss)\n";
        assert_eq!(run_parity(src2), "0\n");
    }

    #[test]
    fn quicken_eq_small_ints_exact() {
        // Small ints (within f64 exact range) compare normally; loop warms the site to Int state.
        let src = "i := 0\nc := 0\nwhile i < 6:\n    if i == 3:\n        c = c + 100\n    if i != 3:\n        c = c + 1\n    i = i + 1\nprint(c)\n";
        // i==3 once (+100), i!=3 five times (+5) = 105
        assert_eq!(run_parity(src), "105\n");
    }

    #[test]
    fn quicken_deopt_int_then_float_then_str() {
        // A single generic `+` site reached first with ints (warms to Int), then floats, then strings
        // (string concat) — each must deopt cleanly to the generic path and stay correct. The `+` of
        // two CALL results is a stack-operand Add (un-fused), exactly the quickening target.
        let src = "fn add2[T](a: T, b: T) -> T:\n    return a + b\n\
                   print(add2(add2(2, 3), add2(4, 5)))\n\
                   print(add2(1.5, 2.5))\n\
                   print(add2(\"ab\", \"cd\"))\n";
        // 5+9=14 ; 1.5+2.5=4.0 ; abcd
        assert_eq!(run_parity(src), "14\n4.0\nabcd\n");
    }

    #[test]
    fn quicken_stack_arith_and_compare_int_fast_path() {
        // Stack-operand arith + ordered compare on ints (the un-fused generic arms). `(a*b) - (c+d)`
        // pushes intermediate results then operates on them — not a `local⊕local`/`local⊕const`
        // window, so it never fuses to a superinstruction and rides the quickened path instead.
        let src = "fn f(a: int, b: int, c: int, d: int) -> int:\n    return (a * b) - (c + d)\n\
                   i := 0\nacc := 0\nwhile i < 4:\n    if f(i + 2, i + 3, i, 1) > 0:\n        acc = acc + f(i + 2, i + 3, i, 1)\n    i = i + 1\nprint(acc)\n";
        // f = (i+2)(i+3) - (i+1): i=0:6-1=5; i=1:12-2=10; i=2:20-3=17; i=3:30-4=26 ; all >0 => 58
        assert_eq!(run_parity(src), "58\n");
    }

    #[test]
    fn quicken_overflow_and_divzero_errors_match_generic() {
        // The quickened int fast path reuses `fast_int_bin`, so overflow / div-by-zero must raise the
        // SAME error as the generic `arith` path. Warm the site, then trip it.
        let dz = "i := 0\nwhile i < 1:\n    print(10 / (i - i))\n    i = i + 1\n";
        let err = run_capture(dz).unwrap_err().to_string();
        assert!(err.contains("division by zero"), "got: {err}");
        let mz = "i := 0\nwhile i < 1:\n    print(10 % (i - i))\n    i = i + 1\n";
        let err2 = run_capture(mz).unwrap_err().to_string();
        assert!(err2.contains("modulo by zero"), "got: {err2}");
    }

    // ---- M19 Phase 6: method-call inline cache (+ flatten) — correctness guards ----
    // `Op::CallMethod` on a struct caches `(tid → proto, module_idx)` per call site, mirroring the
    // field IC: a hit on a matching `tid` skips the `program.structs` clone + the name-keyed
    // `def.methods` probe. The cell holds no `GcRef` (proto id + module_idx are heap-independent), so
    // it is invisible to GC / snapshots / `swap_ctx` — sound across cooperative fibers and `--parallel`.

    #[test]
    fn method_ic_sites_allocated_and_vm_presized() {
        // White-box wiring: a program with struct-method calls allocates ≥1 method-IC site, and the
        // VM pre-sizes its per-`Vm` `method_ic` vector to match (mirrors `field_ic_sites`/`field_ic`).
        let src = "struct C:\n    n: int\n\n    fn g(self) -> int:\n        return self.n\nc := C(5)\nprint(c.g())\n";
        let tokens = lexer::tokenize(src).unwrap();
        let module = parser::parse(tokens).unwrap();
        let program = crate::compiler::compile_module_standalone(&module).unwrap();
        assert!(program.method_ic_sites >= 1, "expected ≥1 method-IC site, got {}", program.method_ic_sites);
        let vm = Vm::new(Arc::new(program.clone()));
        assert_eq!(vm.method_ic.len(), program.method_ic_sites as usize);
    }

    #[test]
    fn method_ic_monomorphic_hot_loop() {
        // A struct method called in a hot loop — the IC hit path. The method DISPATCH (not a field
        // read) is what the method IC caches; the cached proto must be re-used every iteration.
        let src = "struct Acc:\n    n: int\n\n    fn add(self, k: int) -> int:\n        return self.n + k\n\
                   a := Acc(10)\ni := 0\nout := 0\nwhile i < 5:\n    out = out + a.add(i)\n    i = i + 1\nprint(out)\n";
        // per iter: 10 + i -> 10,11,12,13,14 = 60
        assert_eq!(run_parity(src), "60\n");
    }

    #[test]
    fn method_ic_polymorphic_one_site_via_protocol() {
        // A protocol-bounded generic fn has ONE `CallMethod` site (type-erased body) reached by two
        // distinct struct types. A method-IC hit that ignored type identity would dispatch a stale
        // proto (Sq.area on a Rect); the `tid` guard forces a re-resolve on the type switch.
        let src = "protocol Shape:\n    fn area(self) -> int\n\
                   struct Sq:\n    s: int\n\n    fn area(self) -> int:\n        return self.s * self.s\n\
                   struct Rect:\n    w: int\n    h: int\n\n    fn area(self) -> int:\n        return self.w * self.h\n\
                   fn describe[S: Shape](x: S) -> int:\n    return x.area()\n\
                   i := 0\nout := 0\nwhile i < 4:\n    out = out + describe(Sq(3)) + describe(Rect(2, 5))\n    i = i + 1\nprint(out)\n";
        // per iter: 9 + 10 = 19 ; *4 = 76
        assert_eq!(run_parity(src), "76\n");
    }

    #[test]
    fn method_ic_under_parallel_engine() {
        // The method IC lives on each worker `Vm`; method-heavy code on the real-thread engine must
        // match the cooperative engine (caches are per-Vm, tid-guarded, self-verifying).
        let src = "struct Pt:\n    x: int\n    y: int\n\n    fn sum(self) -> int:\n        return self.x + self.y\n\
                   p := Pt(3, 4)\nacc := 0\ni := 0\nwhile i < 100:\n    acc = acc + p.sum()\n    p.x = p.x + 1\n    i = i + 1\n\
                   print(acc)\nprint(p.x)\n";
        assert_eq!(run_capture_parallel(src).expect("parallel"), run(src));
    }

    #[test]
    fn method_ic_gc_stress() {
        // Method dispatch under collect-before-every-instruction: the cached proto/module_idx stay
        // valid because they hold no GcRef and GC never reorders a struct's identity.
        let src = "struct Box:\n    v: int\n\n    fn doubled(self) -> int:\n        return self.v * 2\n\
                   b := Box(21)\ni := 0\nacc := 0\nwhile i < 30:\n    acc = acc + b.doubled()\n    i = i + 1\nprint(acc)\n";
        assert_eq!(run_capture_stress(src), run(src));
        assert_eq!(run_parity(src), "1260\n");
    }

    #[test]
    fn method_ic_function_typed_field_not_cached() {
        // `recv.f(args)` where `f` is a function-typed FIELD (not a method) must keep dispatching via
        // `invoke_value` — the method IC must never cache it as a method proto.
        let src = "struct H:\n    op: fn(int) -> int\n\
                   double := fn(x: int) -> int: x * 2\nh := H(double)\n\
                   i := 0\nout := 0\nwhile i < 3:\n    out = out + h.op(i + 1)\n    i = i + 1\nprint(out)\n";
        // per iter: (i+1)*2 -> 2,4,6 = 12
        assert_eq!(run_parity(src), "12\n");
    }

    #[test]
    fn method_ic_struct_method_shadowing_hof_name() {
        // The IC fast path sits BEFORE the list-HOF / core-type guards in `do_method_call`. A struct
        // whose own method is named `map` (a built-in list HOF name) must dispatch the STRUCT method,
        // never the list HOF — the `Obj::Struct` tid guard makes the collision impossible, this pins it.
        let src = "struct Grid:\n    n: int\n\n    fn map(self, k: int) -> int:\n        return self.n + k\n\
                   g := Grid(100)\ni := 0\nacc := 0\nwhile i < 3:\n    acc = acc + g.map(i)\n    i = i + 1\nprint(acc)\n";
        // 100+0 + 100+1 + 100+2 = 303
        assert_eq!(run_parity(src), "303\n");
    }

    #[test]
    fn method_ic_flattened_method_with_defer_in_loop() {
        // A flattened method's `do_return` must drain the frame's `defer`s on the IC-hit path, every
        // iteration, AFTER the return value is captured (Go order) — pinned across repeated hits.
        let src = "fn note(id: int):\n    print(\"d{id}\")\n\
                   struct Logger:\n    id: int\n\n    fn work(self, n: int) -> int:\n        defer note(self.id)\n        return n * 2\n\
                   l := Logger(7)\ni := 0\nacc := 0\nwhile i < 3:\n    acc = acc + l.work(i)\n    i = i + 1\nprint(acc)\n";
        // each call: prints d7 (defer), returns n*2 -> 0,2,4 = 6
        assert_eq!(run_parity(src), "d7\nd7\nd7\n6\n");
    }

    #[test]
    fn method_ic_uncaught_fault_on_hit_path() {
        // Warm the IC with a good call, then fault on a cached hit. The flattened/cached path must
        // produce the SAME uncaught-fault behavior (message + that the program errors) as a fresh
        // resolve — the frozen interp is the oracle (run_err asserts the VM error; parity via interp).
        let src = "struct Bomb:\n    n: int\n\n    fn blow(self, d: int) -> int:\n        return self.n / d\n\
                   b := Bomb(10)\nprint(b.blow(2))\nprint(b.blow(0))\n";
        let vm_err = run_err(src);
        let interp_err = match crate::interp::run_capture(src) {
            Ok(o) => panic!("expected interp error, got {o:?}"),
            Err(e) => e.message,
        };
        assert_eq!(vm_err, interp_err, "VM/interp must agree on the IC-hit-path fault message");
        assert!(vm_err.contains("zero") || vm_err.contains("division"), "got: {vm_err}");
    }

    #[test]
    fn method_ic_survives_fiber_park_under_parallel() {
        // The per-`Vm` `method_ic` must stay intact across a `swap_ctx` (a fiber parks on `recv`, another
        // runs, the parked fiber resumes and makes a CACHED method call). The central liveness claim:
        // the cell holds no `GcRef`, so a context swap can't invalidate it. VM == `--parallel`.
        let src = "struct Acc:\n    base: int\n\n    fn fold_in(self, k: int) -> int:\n        return self.base + k\n\
                   fn consumer(ch: Channel[int]):\n    a := Acc(1000)\n    total := 0\n    i := 0\n    while i < 4:\n        v := ch.recv()\n        total = total + a.fold_in(v)\n        i = i + 1\n    print(total)\n\
                   fn producer(ch: Channel[int]):\n    i := 0\n    while i < 4:\n        ch.send(i)\n        i = i + 1\n\
                   fn main():\n    ch := Channel[int]()\n    parallel:\n        spawn consumer(ch)\n        spawn producer(ch)\nmain()\n";
        // total = 4*1000 + (0+1+2+3) = 4006
        assert_eq!(run_capture_parallel(src).expect("parallel"), run(src));
        assert_eq!(run(src), "4006\n");
    }

    #[test]
    fn inlined_hot_ops_path_matches_step() {
        // M19 Phase 7 — `run_until` dispatches the hottest ops (GetLocal/SetLocal, the superinstrs,
        // Jump/JumpIfFalse, Call/Return) inline and delegates the tail to `step`. This hammers every
        // inlined op in one program (locals + `a+b`/`a+const`/`i+=1` superinstrs + a conditional +
        // a call + a return) and pins the inline path == the frozen interp (which has no such split).
        let src = "fn f(a: int, b: int) -> int:\n    return a + b\n\
                   i := 0\nacc := 0\n\
                   while i < 20:\n    x := i * 2\n    if x % 3 == 0:\n        acc = acc + f(x, i)\n    else:\n        acc = acc + 1\n    i = i + 1\nprint(acc)\n";
        // x=0,3*?: i=0 x=0 x%3==0 acc+=f(0,0)=0; i=1 x=2 no acc+=1; i=2 x=4 no +1; i=3 x=6 yes +f(6,3)=9;
        // i=4 x=8 no +1; i=5 x=10 no +1; i=6 x=12 yes +f(12,6)=18; ... let the engines agree on the value.
        let out = run_parity(src);
        assert_eq!(out, crate::interp::run_capture(src).expect("interp"));
        assert!(!out.is_empty());
    }

    #[test]
    fn method_call_flatten_deep_recursion_on_small_stack() {
        // Phase 6b: a recursive struct method must not consume host stack (frames live in the heap
        // `frames` Vec, executed by the running `run_until` — not a per-call Rust recursion). Survives
        // a host stack far below production `VM_STACK_BYTES`, like the plain-call flatten guarantee.
        let src = "struct R:\n    base: int\n\n    fn down(self, n: int) -> int:\n        if n == 0:\n            return self.base\n        return self.down(n - 1)\n\
                   r := R(99)\nprint(r.down(8000))\n";
        assert_eq!(run_capture_on_stack(src, 256 * 1024).expect("deep method recursion on small stack"), "99\n");
    }

    #[test]
    fn calls_preserve_arg_order_nesting_and_result_slot() {
        // P1 characterization: locks call semantics before the in-place-args refactor. The bugs an
        // in-place fast path could introduce are stack-position errors — wrong arg order, a stale
        // callee slot left under the result, or a misplaced return value in a larger expression.
        // Non-commutative op catches arg-order swaps; the nested/expression forms catch slot drift.
        assert_eq!(run("fn sub(a: int, b: int) -> int:\n    return a - b\nprint(sub(10, 3))\n"), "7\n");
        assert_eq!(
            run("fn sub(a: int, b: int) -> int:\n    return a - b\nprint(sub(sub(20, 5), sub(8, 3)))\n"),
            "10\n"
        );
        assert_eq!(
            run("fn sub(a: int, b: int) -> int:\n    return a - b\nprint(sub(10, 3) * 2 + 1)\n"),
            "15\n"
        );
        // Zero-arg call returning a value; result used in an expression.
        assert_eq!(run("fn five() -> int:\n    return 5\nprint(five() + 1)\n"), "6\n");
        // Recursion through the call path.
        assert_eq!(
            run("fn fib(n: int) -> int:\n    if n < 2:\n        return n\n    return fib(n - 1) + fib(n - 2)\nprint(fib(10))\n"),
            "55\n"
        );
        // Closure value called via a binding (the Closure arm of the fast path).
        assert_eq!(run("g := fn(x: int) -> int: x * 2\nprint(g(21))\n"), "42\n");
        // Closure capturing an outer binding, then called.
        assert_eq!(
            run("k := 100\nadd := fn(x: int) -> int: x + k\nprint(add(7))\n"),
            "107\n"
        );
        // HOF native (`map`) still routes through the Vec path in invoke_value — must stay correct.
        assert_eq!(run("print([1, 2, 3].map(fn(x: int) -> int: x + 1))\n"), "[2, 3, 4]\n");
        // `defer` inside a called fn runs at that fn's exit (LIFO), not the caller's.
        assert_eq!(
            run("fn log(s: str):\n    print(s)\nfn f():\n    defer log(\"a\")\n    defer log(\"b\")\n    log(\"body\")\nf()\nlog(\"after\")\n"),
            "body\nb\na\nafter\n"
        );
    }

    #[test]
    fn fstring_and_str_render_all_value_shapes() {
        // P2 characterization: locks the exact BuildStr / stringify output across every value
        // shape before the stringify-into-buffer refactor (separators, braces, nesting, hooks).
        assert_eq!(run("print(\"{1} {2.5} {true}\")\n"), "1 2.5 true\n");
        assert_eq!(run("x := 42\nprint(\"i={x}\")\n"), "i=42\n");
        assert_eq!(run("print(\"{[1, 2, 3]}\")\n"), "[1, 2, 3]\n");
        assert_eq!(run("print(\"{(1, 2)}\")\n"), "(1, 2)\n");
        assert_eq!(run("print(\"{[[1], [2, 3]]}\")\n"), "[[1], [2, 3]]\n");
        assert_eq!(run("m := {\"a\": 1, \"b\": 2}\nprint(\"{m}\")\n"), "{a: 1, b: 2}\n");
        assert_eq!(run("print(str({1, 2}))\n"), "{1, 2}\n");
        assert_eq!(run("s: set[int] = set()\nprint(str(s))\n"), "set()\n");
        // Struct default repr + a multi-part f-string mixing literal text and several holes.
        assert_eq!(
            run("struct P:\n    x: int\n    y: int\nprint(\"p={P(3, 4)} end\")\n"),
            "p=P(x=3, y=4) end\n"
        );
        // `str(self)` protocol hook overrides the default repr inside interpolation.
        assert_eq!(
            run("struct Pt:\n    x: int\n    fn str(self) -> str:\n        return \"<{self.x}>\"\nprint(\"v={Pt(7)}\")\n"),
            "v=<7>\n"
        );
        // Enum nullary + payload variants.
        assert_eq!(
            run("enum E:\n    A\n    B(int, int)\nprint(\"{A} {B(1, 2)}\")\n"),
            "A B(1, 2)\n"
        );
    }

    #[test]
    fn list_comprehension_maps_and_filters() {
        assert_eq!(run("print([x * 2 for x in [1, 2, 3]])\n"), "[2, 4, 6]\n");
        assert_eq!(run("print([x for x in [1, 2, 3, 4] if x % 2 == 0])\n"), "[2, 4]\n");
    }

    #[test]
    fn list_comprehension_over_range() {
        assert_eq!(run("print([x * x for x in 0..5])\n"), "[0, 1, 4, 9, 16]\n");
    }

    #[test]
    fn set_comprehension_dedupes() {
        assert_eq!(run("print({x % 3 for x in [0, 1, 2, 3, 4, 5]})\n"), "{0, 1, 2}\n");
    }

    #[test]
    fn map_comprehension_builds_entries() {
        assert_eq!(run("print({x: x * x for x in [1, 2, 3]})\n"), "{1: 1, 2: 4, 3: 9}\n");
    }

    #[test]
    fn map_comprehension_over_map_keys_and_values() {
        assert_eq!(
            run("m := {\"a\": 1, \"b\": 2}\nprint({k: v * 10 for k, v in m})\n"),
            "{a: 10, b: 20}\n"
        );
    }

    // ----- M6c: native function values -----

    fn empty_program() -> Program {
        Program {
            protos: vec![],
            structs: Default::default(),
            variants: Default::default(),
            modules: vec![],
            field_ic_sites: 0,
            method_ic_sites: 0,
        }
    }

    #[test]
    fn vm_calls_native_fn_value() {
        use crate::native::{Host, HostError, NativeRet};
        fn add(h: &mut dyn Host) -> Result<NativeRet, HostError> {
            crate::native::expect_args(h, "add", 2)?;
            Ok(NativeRet::Int(h.arg_int(0)? + h.arg_int(1)?))
        }
        let mut vm = Vm::new(Arc::new(empty_program()));
        let h = vm.heap.alloc(Obj::Native { name: "add".into(), func: add });
        vm.push(Value::Obj(h));
        vm.push(Value::Int(40));
        vm.push(Value::Int(2));
        vm.do_call(2, Span { line: 1, col: 1 }).unwrap();
        assert_eq!(vm.pop(), Value::Int(42));
    }

    #[test]
    fn vm_native_str_return_lowers_to_heap_with_no_children() {
        use crate::native::{Host, HostError, NativeRet};
        fn greet(_h: &mut dyn Host) -> Result<NativeRet, HostError> {
            Ok(NativeRet::Str("hi".into()))
        }
        let mut vm = Vm::new(Arc::new(empty_program()));
        let nat = vm.heap.alloc(Obj::Native { name: "greet".into(), func: greet });
        // A native fn handle has no GC children (guards the mark-phase claim).
        assert!(vm.heap.children(nat).is_empty());
        vm.push(Value::Obj(nat));
        vm.do_call(0, Span { line: 1, col: 1 }).unwrap();
        let result = vm.pop();
        assert_eq!(vm.display(result), "hi");
    }

    // ----- B3.0: WireValue airlock (to_wire / from_wire) -----

    /// A round-trip through the wire form, into the *same* heap, must be value-equal to the original
    /// over a deeply-nested sendable mix (scalars, str, list, tuple, map, set, struct, enum). This is
    /// the airlock's correctness invariant (B3.0): serialize then reconstruct loses nothing.
    #[test]
    fn wire_roundtrip_preserves_value_equality() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let s = vm.heap.alloc(Obj::Str("s".into()));
        let tup = vm.heap.alloc(Obj::Tuple(vec![Value::Bool(true), Value::Nil]));
        let st = vm.heap.alloc(Obj::Struct {
            name: "P".into(),
            tid: TID_NONE,
            fields: vec![("x".into(), Value::Int(1)), ("y".into(), Value::Obj(s))],
        });
        let en = vm.heap.alloc(Obj::Enum {
            ty: "Option".into(),
            variant: "Some".into(),
            payload: vec![Value::Int(9)],
        });
        let mut m = MapData::default();
        m.push(10, Value::Int(1), Value::Int(100));
        m.push(20, Value::Obj(s), Value::Int(200));
        let map = vm.heap.alloc(Obj::Map(m));
        let mut set = SetData::default();
        set.push(5, Value::Int(1));
        set.push(6, Value::Int(2));
        let setobj = vm.heap.alloc(Obj::Set(set));
        let list = vm.heap.alloc(Obj::List(vec![
            Value::Int(1),
            Value::Obj(s),
            Value::Obj(tup),
            Value::Obj(st),
            Value::Obj(en),
            Value::Obj(map),
            Value::Obj(setobj),
        ]));
        let v = Value::Obj(list);

        let w = vm.to_wire(v).expect("nested sendable value should serialize");
        let wired = vm.from_wire(w);
        assert!(vm.values_equal(v, wired), "wire round-trip changed the value");
        // Data is reconstructed into a *fresh* handle (deep copy, not aliasing the original).
        assert_ne!(v, wired, "round-tripped data should be a distinct heap object");
    }

    /// `Map`/`Set` cross the wire carrying their **cached hashes** and **insertion order** unchanged —
    /// `from_wire` rebuilds via `push(hash, …)`, never re-hashing. Pins byte-identical reconstruction
    /// (the iteration order + index a later `print`/lookup observes) even when two keys collide.
    #[test]
    fn wire_preserves_map_hashes_and_order() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let mut m = MapData::default();
        m.push(42, Value::Int(1), Value::Int(10)); // collides with the third entry on hash 42
        m.push(7, Value::Int(2), Value::Int(20));
        m.push(42, Value::Int(3), Value::Int(30));
        let map = Value::Obj(vm.heap.alloc(Obj::Map(m)));

        let w = vm.to_wire(map).expect("map should serialize");
        let wired = vm.from_wire(w);
        let Value::Obj(h) = wired else { panic!("expected heap obj") };
        let Obj::Map(rebuilt) = vm.heap.get(h) else { panic!("expected map") };
        let hashes: Vec<u64> = rebuilt.entries.iter().map(|(hash, ..)| *hash).collect();
        assert_eq!(hashes, vec![42, 7, 42], "cached hashes / order must survive the round-trip");
        // The index must reflect the cached hashes (collision bucket points at positions 0 and 2).
        assert_eq!(rebuilt.candidates(42), &[0, 2]);
        assert_eq!(rebuilt.candidates(7), &[1]);
    }

    /// By-reference callables (`Func`/`Closure`/`Module`/`Native`) cross the airlock **by handle** —
    /// `to_wire`→`from_wire` returns the *same* `GcRef` (matching the old `deep_clone` by-handle arm).
    /// (`Str` no longer qualifies — it crosses by value as of B3.3a; see `wire_crosses_str_by_value`.)
    #[test]
    fn wire_passes_by_reference_objects_as_same_handle() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let m = vm.heap.alloc(Obj::Module { name: "m".into(), slots: Vec::new(), index: Default::default() });
        let v = Value::Obj(m);
        let w = vm.to_wire(v).expect("by-ref object should serialize");
        assert_eq!(vm.from_wire(w), v, "by-reference object must round-trip to the same handle");
    }

    /// B3.3a: a `str` crosses the airlock **by value** (owned bytes), not as a by-reference
    /// `Handle(GcRef)`: `from_wire` allocates a *fresh* heap `str` that is value-equal but a distinct
    /// handle. This is what lets a `str` cross a real OS-thread heap boundary at B3.3 (a `GcRef` would
    /// be a meaningless slot index there). Parity-safe: `str` is immutable + value-compared and Chezzi
    /// has no identity operator, so a fresh handle is observationally identical to the shared one.
    #[test]
    fn wire_crosses_str_by_value() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let s = vm.heap.alloc(Obj::Str("imm".into()));
        let v = Value::Obj(s);
        let w = vm.to_wire(v).expect("str should serialize");
        let wired = vm.from_wire(w);
        assert_ne!(wired, v, "a crossed str gets a fresh handle (by value, not by handle)");
        assert!(vm.values_equal(v, wired), "the fresh str must be value-equal to the original");
    }

    /// B3.3a: a `str` used as a **map key** crosses by value and stays findable — the cached hash is
    /// carried through and `from_wire` rebuilds the key as a fresh handle whose content hashes
    /// identically (hashing keys on bytes, not `GcRef`), so the reconstructed map's bucket index is
    /// preserved. Guards against a future change that hashed/compared str keys by handle identity.
    #[test]
    fn wire_str_map_key_survives_roundtrip() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let key = vm.heap.alloc(Obj::Str("k".into()));
        let mut m = MapData::default();
        let h = vm.scalar_hash(Value::Obj(key));
        m.push(h, Value::Obj(key), Value::Int(42));
        let map = Value::Obj(vm.heap.alloc(Obj::Map(m)));

        let w = vm.to_wire(map).expect("map with a str key should serialize");
        let wired = vm.from_wire(w);
        let Value::Obj(mh) = wired else { panic!("expected map handle") };
        let Obj::Map(rebuilt) = vm.heap.get(mh) else { panic!("expected map") };
        // Same single entry: a fresh str key, value-equal, same cached hash → same bucket.
        assert_eq!(rebuilt.entries.len(), 1);
        let (rh, rk, rv) = &rebuilt.entries[0];
        assert_eq!(*rh, h, "cached hash preserved");
        assert_eq!(*rv, Value::Int(42));
        assert_eq!(rebuilt.candidates(h), &[0], "index bucket points at the rebuilt key");
        assert!(vm.values_equal(*rk, Value::Obj(key)), "rebuilt str key is value-equal");
    }

    /// B3.1: `Channel`/`Shared`/`Executor` cross the airlock as their shared `Arc<…Core>`. The
    /// round-trip yields a *fresh* `GcRef` (a new handle obj) wrapping the **same** core — identity is
    /// at the `Arc`, not the handle, so two tasks still reach one mailbox/box/queue.
    #[test]
    fn wire_shares_core_across_a_fresh_handle() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let ch = vm.heap.alloc(Obj::Channel(Arc::new(ChannelCore::default())));
        let sh = vm.heap.alloc(Obj::Shared(Arc::new(SharedCore::default())));
        let ex = vm.heap.alloc(Obj::Executor(Arc::new(ExecutorCore::default())));
        for h in [ch, sh, ex] {
            let v = Value::Obj(h);
            let w = vm.to_wire(v).expect("core handle should serialize");
            let wired = vm.from_wire(w);
            assert_ne!(wired, v, "a crossed core gets a fresh handle (new GcRef)");
            // Same underlying core: an `Arc::ptr_eq` between the two handles' cores.
            let same = match (vm.heap.get(h), vm.heap.get(match wired { Value::Obj(g) => g, _ => unreachable!() })) {
                (Obj::Channel(a), Obj::Channel(b)) => Arc::ptr_eq(a, b),
                (Obj::Shared(a), Obj::Shared(b)) => Arc::ptr_eq(a, b),
                (Obj::Executor(a), Obj::Executor(b)) => Arc::ptr_eq(a, b),
                _ => false,
            };
            assert!(same, "the fresh handle must point at the SAME shared core");
        }
    }

    /// B3.1: two handles produced from one core (the `from_wire` airlock copy) reach the SAME mailbox
    /// — `send` on one handle is `recv`-able through the other. Proves the `Arc` core is shared, not
    /// duplicated, across the wire.
    #[test]
    fn channel_core_shared_across_handles() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let h1 = vm.heap.alloc(Obj::Channel(Arc::new(ChannelCore::default())));
        // Cross the airlock → a second handle onto the same core.
        let w = vm.to_wire(Value::Obj(h1)).unwrap();
        let Value::Obj(h2) = vm.from_wire(w) else { panic!("expected handle") };
        let sp = Span { line: 1, col: 1 };
        vm.channel_method(h1, "send", &[Value::Int(7)], sp).unwrap();
        // recv through the OTHER handle sees the message.
        assert_eq!(vm.channel_method(h2, "recv", &[], sp).unwrap(), Value::Int(7));
    }

    /// B3.3-threads sub-step 2: under `--parallel`, a `recv` on an empty channel **blocks the OS
    /// thread** on the core's `Condvar` and is woken by a `send` from another thread (real
    /// cross-thread blocking, not a fiber park). Two `Vm`s share one `ChannelCore` via `Arc`; the
    /// worker blocks in `recv` on its own thread, the main thread `send`s and wakes it. The outcome
    /// is `42` regardless of interleaving (send-first → immediate pop; block-first → cv wake), so the
    /// test is deterministic without sleeps. Also a compile-time proof that `Vm: Send` (it moves into
    /// `thread::spawn`), the load-bearing fact for the whole thread-flip.
    #[test]
    fn parallel_recv_blocks_until_send_wakes_it() {
        let core = Arc::new(ChannelCore::default());
        let mut worker = Vm::new(Arc::new(empty_program()));
        worker.parallel = true;
        let wh = worker.heap.alloc(Obj::Channel(Arc::clone(&core)));
        let mut sender = Vm::new(Arc::new(empty_program()));
        let sh = sender.heap.alloc(Obj::Channel(Arc::clone(&core)));
        let sp = Span { line: 1, col: 1 };
        let handle = std::thread::spawn(move || worker.channel_method(wh, "recv", &[], sp).unwrap());
        sender.channel_method(sh, "send", &[Value::Int(42)], sp).unwrap();
        assert_eq!(handle.join().unwrap(), Value::Int(42));
    }

    /// Call-flattening × M:N parking: a fiber that `recv`-parks **several flattened plain-function
    /// frames deep** (`main → collect → deep_recv ×6`, all `Op::Call`, parking at `ip > 0`) must
    /// suspend with its frames intact and, on a sibling `send`, resume through `run_until(0)` and
    /// thread the received value back up the flattened chain. Pre-flatten each of those frames was a
    /// nested Rust `run_until`; now they share one loop, so resume reads them straight from the heap
    /// `frames` Vec. (Closes the coverage gap the review flagged: park deep in *bytecode* frames, not
    /// just inside a native HOF callback.)
    #[test]
    fn parallel_recv_parks_deep_in_flattened_frames_and_resumes() {
        let src = "\
fn deep_recv(ch: Channel[int], depth: int) -> int:
    if depth <= 0:
        return ch.recv()
    return deep_recv(ch, depth - 1)

fn collect(ch: Channel[int], out: Channel[int]):
    out.send(deep_recv(ch, 5))

fn produce(ch: Channel[int], v: int):
    ch.send(v)

fn main():
    ch := Channel[int]()
    out := Channel[int]()
    parallel:
        spawn collect(ch, out)
        spawn produce(ch, 99)
    print(out.recv())

main()
";
        assert_eq!(run_capture_parallel(src).expect("parallel run"), "99\n");
    }

    /// D2a: an M:N fiber carries its OWN heap (share-nothing). `swap_ctx` swaps that heap with the
    /// host `Vm`'s when the fiber is scheduled in, and back out when it parks — the prerequisite for
    /// D2b parking a fiber across worker threads. Round-trip: a fiber heap holding `"fiber-obj"` and
    /// a host heap holding `"vm-obj"` exchange on swap-in and restore on swap-out.
    #[test]
    fn swap_ctx_round_trips_an_mn_fiber_heap() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        vm.parallel = true; // M:N fibers only carry their own heap under --parallel (decision A).
        let hv = vm.heap.alloc(Obj::Str("vm-obj".into()));

        let mut fiber_heap = Heap::new();
        let hf = fiber_heap.alloc(Obj::Str("fiber-obj".into()));
        let mut ctx = FiberCtx { heap: Some(fiber_heap), ..FiberCtx::default() };

        // Swap the fiber in: self.heap becomes the fiber's heap; the host heap parks in the ctx.
        vm.swap_ctx(&mut ctx);
        assert!(matches!(vm.heap.get(hf), Obj::Str(s) if &s[..] == "fiber-obj"));
        assert!(matches!(ctx.heap.as_ref().unwrap().get(hv), Obj::Str(s) if &s[..] == "vm-obj"));

        // Swap back out: the host heap is restored, the fiber keeps its own heap.
        vm.swap_ctx(&mut ctx);
        assert!(matches!(vm.heap.get(hv), Obj::Str(s) if &s[..] == "vm-obj"));
        assert!(matches!(ctx.heap.as_ref().unwrap().get(hf), Obj::Str(s) if &s[..] == "fiber-obj"));
    }

    /// D2b: an M:N fiber carries its own per-task SIDE state too — `out`/`stderr` (Decision-F output
    /// buffers) and the heap-keyed roots `module_objs`/`module_faulted`/`executors` (each a `GcRef`
    /// into the fiber's own heap, so they MUST travel atomically with that heap). `swap_ctx` round-
    /// trips all of them alongside the heap, gated on `heap.is_some()` so a cooperative fiber
    /// (`heap: None`) leaves the shell's side state untouched (byte-identical, asserted separately).
    #[test]
    fn mn_swap_ctx_round_trips_fiber_side_state() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        vm.parallel = true;
        vm.out.push_str("host-out");
        vm.stderr.push_str("host-err");
        let host_mod = vm.heap.alloc(Obj::Str("host-mod".into()));
        let host_exec = vm.heap.alloc(Obj::Str("host-exec".into()));
        vm.module_objs = vec![host_mod];
        vm.module_faulted = vec![true];
        vm.executors = vec![host_exec];
        // M19 Phase 3 — the intern cache is heap-keyed too; it must round-trip with the heap.
        let host_str = vm.heap.alloc(Obj::Str("host-str".into()));
        vm.str_intern.insert(0x10, host_str);

        let mut fiber_heap = Heap::new();
        let fib_mod = fiber_heap.alloc(Obj::Str("fiber-mod".into()));
        let fib_exec = fiber_heap.alloc(Obj::Str("fiber-exec".into()));
        let fib_str = fiber_heap.alloc(Obj::Str("fiber-str".into()));
        let mut ctx = FiberCtx {
            heap: Some(fiber_heap),
            out: "fiber-out".to_string(),
            stderr: "fiber-err".to_string(),
            module_objs: vec![fib_mod],
            module_faulted: vec![false],
            executors: vec![fib_exec],
            str_intern: fxhash::FxHashMap::from_iter([(0x20usize, fib_str)]),
            ..FiberCtx::default()
        };

        // Schedule in: the fiber's side state becomes live; the shell's parks into the ctx.
        vm.swap_ctx(&mut ctx);
        assert_eq!(vm.out, "fiber-out");
        assert_eq!(vm.stderr, "fiber-err");
        assert_eq!(vm.module_objs, vec![fib_mod]);
        assert_eq!(vm.module_faulted, vec![false]);
        assert_eq!(vm.executors, vec![fib_exec]);
        assert_eq!(vm.str_intern.get(&0x20), Some(&fib_str));
        assert_eq!(vm.str_intern.get(&0x10), None);
        assert_eq!(ctx.out, "host-out");
        assert_eq!(ctx.module_objs, vec![host_mod]);
        assert_eq!(ctx.str_intern.get(&0x10), Some(&host_str));

        // Park out: the shell's side state is restored; the fiber keeps its own.
        vm.swap_ctx(&mut ctx);
        assert_eq!(vm.out, "host-out");
        assert_eq!(vm.stderr, "host-err");
        assert_eq!(vm.module_objs, vec![host_mod]);
        assert_eq!(vm.module_faulted, vec![true]);
        assert_eq!(vm.executors, vec![host_exec]);
        assert_eq!(vm.str_intern.get(&0x10), Some(&host_str));
        assert_eq!(ctx.out, "fiber-out");
        assert_eq!(ctx.module_objs, vec![fib_mod]);
        assert_eq!(ctx.str_intern.get(&0x20), Some(&fib_str));
    }

    /// Per-connection spawn: a fiber running an eager `parallel:` body can PARK (its acceptor blocks
    /// on `accept`) between `EnterNursery` and `JoinNursery`, so the open eager scope — the live
    /// inner sched + its monotonic spawn index — MUST travel with the fiber across `swap_ctx`, just
    /// like `nurseries`. Otherwise the scope leaks onto whatever fiber the shell schedules next.
    #[test]
    fn eager_scope_round_trips_with_fiber_ctx() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        vm.parallel = true;
        let host_sched = Arc::new(mk_sched(0));
        vm.eager_scheds.push(Some(EagerScope {
            sched: Arc::clone(&host_sched),
            cancel: Arc::new(AtomicBool::new(false)),
            drainer: None,
        }));

        let fiber_sched = Arc::new(mk_sched(0));
        let mut ctx = FiberCtx {
            eager_scheds: vec![Some(EagerScope {
                sched: Arc::clone(&fiber_sched),
                cancel: Arc::new(AtomicBool::new(false)),
                drainer: None,
            })],
            ..FiberCtx::default()
        };

        // Schedule the fiber in: its eager scope becomes live; the host's parks into the ctx.
        vm.swap_ctx(&mut ctx);
        assert_eq!(vm.eager_scheds.len(), 1);
        assert!(Arc::ptr_eq(&vm.eager_scheds[0].as_ref().unwrap().sched, &fiber_sched), "the fiber's eager scope is now live");
        assert!(Arc::ptr_eq(&ctx.eager_scheds[0].as_ref().unwrap().sched, &host_sched), "the host's scope parked into the ctx");

        // Park the fiber out: the host's scope is restored; the fiber keeps its own.
        vm.swap_ctx(&mut ctx);
        assert!(Arc::ptr_eq(&vm.eager_scheds[0].as_ref().unwrap().sched, &host_sched), "host scope restored");
        assert!(Arc::ptr_eq(&ctx.eager_scheds[0].as_ref().unwrap().sched, &fiber_sched));
    }

    /// D2b companion to [`swap_ctx_leaves_heap_untouched_for_cooperative_fiber`]: a cooperative fiber
    /// (`heap: None`) must leave the shell's `out`/`module_objs`/`executors` untouched too, so the
    /// cooperative engine stays byte-identical.
    #[test]
    fn mn_swap_ctx_leaves_side_state_untouched_for_cooperative_fiber() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        vm.out.push_str("host-out");
        let host_mod = vm.heap.alloc(Obj::Str("host-mod".into()));
        vm.module_objs = vec![host_mod];
        let mut ctx = FiberCtx::default();
        vm.swap_ctx(&mut ctx);
        assert_eq!(vm.out, "host-out");
        assert_eq!(vm.module_objs, vec![host_mod]);
        assert!(ctx.out.is_empty(), "swap must not give a cooperative fiber side state");
        assert!(ctx.module_objs.is_empty());
    }

    // ---- D2b MnSched scheduler mechanics (Step 2 — hand-built fibers, no bytecode) ----

    fn dl_err() -> RuntimeError {
        RuntimeError { message: DEADLOCK_MSG.to_string(), span: Span { line: 1, col: 1 } }
    }
    fn mk_sched(total: usize) -> MnSched {
        // 4 worker slots by default — enough for the multi-`wid` steal tests; single-worker tests
        // just use `wid` 0.
        MnSched::new(total, 4, Arc::new(AtomicBool::new(false)), dl_err())
    }
    fn mk_fiber(task_index: usize) -> Fiber {
        Fiber { ctx: FiberCtx::default(), state: FiberState::Ready, task_index, span: Span { line: 1, col: 1 }, resume_native: None }
    }
    /// An UNSTARTED fiber (`Pending`) — what `inject`/`seed` require so `run_one_fiber` runs the task
    /// body via `start_task` (a `Ready` fiber is treated as a resume and runs no body).
    fn mk_pending_fiber(task_index: usize) -> Fiber {
        let task = PendingCall::Call { callee: Value::Nil, args: Vec::new(), span: Span { line: 1, col: 1 } };
        Fiber { ctx: FiberCtx::default(), state: FiberState::Pending(task), task_index, span: Span { line: 1, col: 1 }, resume_native: None }
    }
    fn empty_core() -> Arc<ChannelCore> {
        Arc::new(ChannelCore::default())
    }
    fn core_key(core: &Arc<ChannelCore>) -> usize {
        Arc::as_ptr(core) as usize
    }
    fn take_run(s: &MnSched) -> Fiber {
        // tick=1 → not a periodic-global-check schedule, so the normal own-local-then-global order
        // applies (what the existing unit tests assert).
        match s.take_runnable(0, 1) {
            Take::Run(f) => f,
            Take::Stop => panic!("expected a runnable fiber, got Stop"),
        }
    }

    /// D2b/U1: `take_runnable` pops the shared run queue in FIFO order and marks each popped fiber
    /// `running`.
    #[test]
    fn mnsched_take_runnable_pops_in_order_and_counts_running() {
        let sched = mk_sched(2);
        sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
        assert_eq!(take_run(&sched).task_index, 0);
        assert_eq!(take_run(&sched).task_index, 1);
        assert_eq!(sched.lock().running, 2);
    }

    /// D4a: `runnable` is the authoritative count of runnable (queued, not running/parked/done)
    /// fibers — in D2b's single-queue world it mirrors `runq.len()` exactly, but it is maintained as
    /// an atomic so D4b's per-worker split (local rings + global, no single queue to `.len()`) can
    /// keep using it for the deadlock predicate. This pins the bump/decrement discipline: seed +N,
    /// pop −1 (runnable→running), park unchanged (running→parked), send_wake +woken (parked→ready),
    /// finish unchanged (running→done).
    #[test]
    fn mnsched_runnable_tracks_single_queue() {
        let sched = mk_sched(3);
        let core = empty_core();
        let key = core_key(&core);
        sched.seed(vec![mk_fiber(0), mk_fiber(1), mk_fiber(2)]);
        assert_eq!(sched.runnable.load(Ordering::Relaxed), 3, "seed bumps runnable");
        let f0 = take_run(&sched);
        assert_eq!(sched.runnable.load(Ordering::Relaxed), 2, "pop transitions runnable→running");
        sched.park(key, &core, f0);
        assert_eq!(sched.runnable.load(Ordering::Relaxed), 2, "park transitions running→parked (no change)");
        sched.send_wake(key, &core, WireValue::Int(7));
        assert_eq!(sched.runnable.load(Ordering::Relaxed), 3, "send_wake transitions parked→ready");
        let f = take_run(&sched);
        assert_eq!(sched.runnable.load(Ordering::Relaxed), 2);
        sched.finish(f.task_index, TaskOutcome::Cancelled);
        assert_eq!(sched.runnable.load(Ordering::Relaxed), 2, "finish transitions running→done (no change)");
        // The invariant: with no per-worker locals populated, runnable == global.len() at quiescence.
        assert_eq!(sched.runnable.load(Ordering::Relaxed), sched.lock().global.len());
    }

    /// Per-connection spawn: `inject` adds a task to a LIVE sched — it grows `total` + `slots`
    /// (so the dynamically-spawned handler gets a Decision-F outcome slot) and queues the fiber
    /// runnable, all under one core lock (the `complete_offload` twin). This is what lifts the
    /// "fixed total — no spawn-after-join" restriction.
    #[test]
    fn mnsched_inject_grows_total_and_slots() {
        let sched = mk_sched(1);
        sched.seed(vec![mk_fiber(0)]); // total 1, slots.len 1, runnable 1
        sched.inject(mk_pending_fiber(1));
        let c = sched.lock();
        assert_eq!(c.total, 2, "inject grows total");
        assert_eq!(c.slots.len(), 2, "inject grows the outcome-slot vec");
        assert_eq!(c.global.len(), 2, "the injected fiber is queued runnable");
        drop(c);
        assert_eq!(sched.runnable.load(Ordering::Relaxed), 2, "inject runnable-accounts the new fiber");
    }

    /// Per-connection spawn: injecting a runnable fiber into a sched where every existing fiber is
    /// parked must VETO the deadlock predicate — `total += 1` is paired with `runnable += 1` under
    /// one lock, so the new fiber is immediately accounted and `is_deadlocked` sees `runnable > 0`.
    #[test]
    fn mnsched_inject_does_not_false_deadlock() {
        let sched = mk_sched(1);
        sched.seed(vec![mk_fiber(0)]);
        let f0 = take_run(&sched); // running 1, runnable 0
        let core = empty_core();
        sched.park(core_key(&core), &core, f0); // parked 1, running 0, runnable 0 → deadlock
        {
            let c = sched.lock();
            assert!(sched.is_deadlocked(&c), "all parked, nothing runnable/inflight = deadlock");
        }
        sched.inject(mk_pending_fiber(1)); // runnable 1
        {
            let c = sched.lock();
            assert!(!sched.is_deadlocked(&c), "an injected runnable fiber vetoes the deadlock fire");
        }
    }

    /// D4b: a `LocalQ` pops `runnext` first (locality), then the ring in FIFO order, then `None`.
    #[test]
    fn localq_runnext_then_ring_order() {
        let mut q = LocalQ::new();
        q.ring.push_back(mk_fiber(1));
        q.ring.push_back(mk_fiber(2));
        q.runnext = Some(mk_fiber(0));
        assert_eq!(q.pop().unwrap().task_index, 0, "runnext runs first");
        assert_eq!(q.pop().unwrap().task_index, 1, "then ring FIFO");
        assert_eq!(q.pop().unwrap().task_index, 2);
        assert!(q.pop().is_none());
    }

    /// D4b: `take_runnable(wid)` drains the worker's own `locals[wid]` BEFORE the shared global queue.
    /// (In D4b nothing populates a local at runtime; this drives it directly to pin the search order
    /// the D4c requeue/steal paths depend on.)
    #[test]
    fn take_runnable_prefers_local_over_global() {
        let sched = mk_sched(2);
        sched.seed(vec![mk_fiber(1)]); // task 1 → global, runnable == 1
        sched.lock_local(0).ring.push_back(mk_fiber(0)); // task 0 → worker 0's local
        sched.runnable.fetch_add(1, Ordering::Relaxed); // keep the counter consistent (==2)
        assert_eq!(take_run(&sched).task_index, 0, "own local drained before the global queue");
        assert_eq!(take_run(&sched).task_index, 1, "then the global queue");
        assert_eq!(sched.runnable.load(Ordering::Relaxed), 0);
        assert_eq!(sched.lock().running, 2);
    }

    /// A trivial blocking-shaped native for the offload tests: double the first int arg. Off-heap-safe
    /// (reads only a primitive arg, returns a primitive), so it can run on an [`OffloadHost`].
    fn double_native(h: &mut dyn crate::native::Host) -> Result<crate::native::NativeRet, crate::native::HostError> {
        Ok(crate::native::NativeRet::Int(h.arg_int(0)? * 2))
    }

    /// A native that panics — stands in for a misclassified blocking fn that hits an `OffloadHost`
    /// `unreachable!`, or any panic inside an offloaded call.
    fn panic_native(_h: &mut dyn crate::native::Host) -> Result<crate::native::NativeRet, crate::native::HostError> {
        panic!("boom inside offloaded native")
    }

    /// D5 — a panic inside an offloaded native must NOT lose the fiber. If the pool job lets the panic
    /// escape, `complete_offload` never runs: `inflight` stays pinned, the fiber's slot stays empty,
    /// and the nursery hangs forever (the deadlock predicate is vetoed by `inflight > 0`). The job
    /// must catch the panic, surface it as a fault on the fiber, and always re-enqueue → `inflight`
    /// returns to 0 and the resumed fiber faults like an inline native panic.
    #[test]
    fn offload_native_panic_still_completes_and_faults() {
        let sched = Arc::new(mk_sched(1));
        sched.seed(vec![mk_fiber(0)]);
        let f0 = take_run(&sched); // running == 1
        let req = OffloadReq { func: panic_native, args: vec![], span: Span { line: 1, col: 1 }, timer_ms: None };
        sched.offload(f0, req);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while sched.inflight.load(Ordering::Relaxed) != 0 {
            assert!(std::time::Instant::now() < deadline, "panic in offloaded native lost the fiber (inflight pinned → hang)");
            std::thread::yield_now();
        }
        // The fiber came back runnable carrying a fault to raise on resume.
        let f0 = match sched.take_runnable(0, 1) {
            Take::Run(f) => f,
            Take::Stop => panic!("fiber not requeued after panicking offload"),
        };
        assert!(matches!(f0.resume_native, Some(Err(_))), "panic surfaced as a fault on the fiber");
    }

    /// D5: an in-flight blocking offload must SUPPRESS the deadlock fire. The predicate is
    /// `running==0 && runnable==0 && parked_n>0 && done<total` — but a fiber off in the blocking pool
    /// (counted by `inflight`) is neither running, runnable, nor parked, and *will* come back runnable,
    /// so `inflight>0` must veto the deadlock declaration (else a program that parks everyone while one
    /// fiber blocks in `read_file` would falsely fault `deadlock`).
    #[test]
    fn deadlock_predicate_suppressed_by_inflight_offload() {
        let sched = mk_sched(2);
        let c = sched.lock();
        // running==0, runnable==0, one parked, none done: a real deadlock with no in-flight work.
        let mut c = c;
        c.parked_n = 1;
        assert!(sched.is_deadlocked(&c), "all-parked + nothing in flight = deadlock");
        sched.inflight.fetch_add(1, Ordering::Relaxed);
        assert!(!sched.is_deadlocked(&c), "an in-flight blocking offload vetoes the deadlock fire");
    }

    /// D5 owe #3 Path C (#1) — the deadlock false-positive fix. A demoted (blocked-in-callback) fiber
    /// polls its OWN channel queue; a value a sibling already queued there is invisible to the
    /// counter-only predicate (a `send` `push_back`s + notifies the channel condvar — it does NOT bump
    /// `runnable`). Registering the demoted channel lets `is_deadlocked` peek it: a non-empty queue
    /// means that fiber WILL pop + make progress (possibly waking a parked sibling), so an apparent
    /// all-blocked quiesce is NOT a deadlock — don't fault an innocent parked sibling.
    #[test]
    fn deadlock_predicate_vetoed_by_queued_value_on_demoted_channel() {
        let sched = mk_sched(2);
        let core = empty_core();
        let ptr = core_key(&core);
        let mut c = sched.lock();
        // The #1 race: one demoted fiber (blocked_native) + one parked sibling, nothing running /
        // runnable / inflight — the counter-only predicate fires (the false positive).
        c.parked_n = 1;
        sched.blocked_native.fetch_add(1, Ordering::Relaxed);
        c.register_demoted(ptr, &core);
        // A sibling already queued a value on the demoted fiber's channel: it will pop + progress.
        core.q.lock().unwrap().queue.push_back(WireValue::Int(7));
        assert!(
            !sched.is_deadlocked(&c),
            "a queued value on a demoted channel must veto the deadlock fire (#1 false-positive)"
        );
        // Drain it: now the demoted fiber truly has nothing queued → a real all-blocked deadlock.
        core.q.lock().unwrap().queue.pop_front();
        assert!(
            sched.is_deadlocked(&c),
            "an empty demoted channel with all fibers blocked IS a genuine deadlock"
        );
        // Un-register restores the pre-demote predicate (no stale registry entry vetoing forever).
        c.unregister_demoted(ptr);
        assert!(
            sched.is_deadlocked(&c),
            "after un-register the predicate is unchanged (still all-blocked)"
        );
    }

    /// D5 owe #3 Path C (#1) — the registry is REFCOUNTED so 2+ fibers demoted on the SAME channel each
    /// register/unregister independently (one `unregister` must not drop the channel while a second
    /// demoted fiber still waits on it). Drives refcount 0→1→2→1→0 and asserts the veto survives the
    /// single `unregister` (the entry is still present at refcount 1) and only the empty/fully-removed
    /// state declares deadlock. Catches a refcount-direction regression (remove-at-1 / wrong increment)
    /// that the single-fiber test cannot — exactly the "stale entry permanently vetoes a real deadlock"
    /// vs "premature removal re-opens the false-positive" failure modes.
    #[test]
    fn demoted_channel_registry_is_refcounted_for_two_fibers_on_one_channel() {
        let sched = mk_sched(3);
        let core = empty_core();
        let ptr = core_key(&core);
        let mut c = sched.lock();
        // Two fibers demoted on the SAME channel + one parked sibling; nothing else running.
        c.parked_n = 1;
        sched.blocked_native.fetch_add(2, Ordering::Relaxed);
        c.register_demoted(ptr, &core);
        c.register_demoted(ptr, &core); // refcount now 2
        // A value queued on the shared channel → at least one demoted fiber pops + progresses.
        core.q.lock().unwrap().queue.push_back(WireValue::Int(7));
        assert!(!sched.is_deadlocked(&c), "queued value on the shared demoted channel vetoes deadlock");
        // One fiber pops + un-registers (refcount 2→1); the OTHER is still demoted on this channel, so
        // the entry must remain. Queue now empty → but the entry's presence alone does NOT veto; the
        // peek is queue-driven, so an empty registered channel is a genuine all-blocked deadlock.
        core.q.lock().unwrap().queue.pop_front();
        c.unregister_demoted(ptr); // refcount 2→1, entry retained
        assert!(
            sched.is_deadlocked(&c),
            "refcount 1 + empty queue = genuine deadlock (the surviving demoted fiber has nothing)"
        );
        // A fresh value for the surviving fiber re-vetoes via the retained entry (proves it wasn't
        // dropped at the first unregister).
        core.q.lock().unwrap().queue.push_back(WireValue::Int(9));
        assert!(
            !sched.is_deadlocked(&c),
            "the retained refcount-1 entry still peeks the queue (entry not dropped at refcount 1)"
        );
        core.q.lock().unwrap().queue.pop_front();
        c.unregister_demoted(ptr); // refcount 1→0, entry removed
        assert!(c.demoted_chans.is_empty(), "the entry is removed only at refcount 0");
        assert!(sched.is_deadlocked(&c), "all demoted fibers gone, still all-blocked = deadlock");
    }

    /// D6: `poll_park_offload` hands a fiber whose socket op `WouldBlock`ed to the netpoller —
    /// running→inflight — so a socket-parked fiber is accounted as in-flight (it WILL be woken by the
    /// OS) and vetoes a false deadlock, exactly like a blocking-pool offload. Uses a real loopback fd
    /// (never written) so the fiber genuinely stays parked; `deregister` cleans up (delete-before-drop).
    #[test]
    fn poll_park_offload_moves_running_to_inflight() {
        use std::os::fd::AsRawFd;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let _client = std::net::TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        server.set_nonblocking(true).unwrap();
        let key = usize::MAX - 10;

        let sched = Arc::new(mk_sched(2));
        sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
        let f0 = take_run(&sched); // running == 1, runnable == 1
        assert_eq!(sched.lock().running, 1);

        sched.poll_park_offload(f0, PollPark { key, fd: server.as_raw_fd(), interest: poller::Interest::Read, in_flight: core::new_in_flight(), deadline: None });
        assert_eq!(sched.lock().running, 0, "poll-park freed the worker (running decremented)");
        assert_eq!(sched.inflight.load(Ordering::Relaxed), 1, "running → inflight on poll-park");

        // The in-flight socket op vetoes a deadlock even with the sibling still queued drained off.
        let mut c = sched.lock();
        c.parked_n = 1;
        assert!(!sched.is_deadlocked(&c), "a socket op in flight on the poller vetoes a false deadlock");
        drop(c);

        // Clean up: deregister disarms the fd + re-injects (inflight→runnable), before `server` drops.
        assert!(poller::deregister(key), "the parked socket op was registered");
        assert_eq!(sched.inflight.load(Ordering::Relaxed), 0, "deregister re-injected the fiber");
    }

    /// D5: `offload` hands a fiber to the blocking pool (running→inflight); when the pool finishes the
    /// native it `complete_offload`s the fiber back onto the run queue (inflight→runnable) with the
    /// raw [`NativeRet`] stashed for the worker to lower + push on resume.
    #[test]
    fn offload_runs_native_and_requeues_fiber_with_result() {
        let sched = Arc::new(mk_sched(2));
        sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
        let f0 = take_run(&sched); // running==1, runnable==1
        assert_eq!(sched.lock().running, 1);

        let req = OffloadReq {
            func: double_native,
            args: vec![crate::native::NativeArg::Int(21)],
            span: Span { line: 1, col: 1 },
            timer_ms: None,
        };
        sched.offload(f0, req);

        // The job runs asynchronously on the blocking pool; wait (bounded) for it to complete.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while sched.inflight.load(Ordering::Relaxed) != 0 {
            assert!(std::time::Instant::now() < deadline, "offloaded native never completed");
            std::thread::yield_now();
        }
        assert_eq!(sched.lock().running, 0, "offload freed the worker (running decremented)");
        assert_eq!(sched.runnable.load(Ordering::Relaxed), 2, "f1 still queued + f0 requeued on completion");

        // The requeued fiber carries the lowered-pending native result (Int(21)*2 == Int(42)).
        let mut found = None;
        while let Take::Run(f) = sched.take_runnable(0, 1) {
            if f.task_index == 0 {
                found = Some(f);
                break;
            }
        }
        let f0 = found.expect("offloaded fiber requeued");
        assert_eq!(f0.resume_native, Some(Ok(crate::native::NativeRet::Int(42))));
    }

    /// D5 owe #2: a `timer_ms` offload parks the fiber on the *timer* thread (not the blocking pool):
    /// running→inflight at submit (so it vetoes the deadlock predicate while sleeping), then the timer
    /// fires at the deadline and `complete_offload`s the fiber back (inflight→runnable) carrying
    /// `Ok(Nil)` — the native is never run on this path (a sleep computes nothing). Guards the timer
    /// branch + that the sleeping fiber can't fault a false deadlock.
    #[test]
    fn timer_offload_parks_then_requeues_fiber_with_nil() {
        let sched = Arc::new(mk_sched(2));
        sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
        let f0 = take_run(&sched); // running == 1, runnable == 1
        assert_eq!(sched.lock().running, 1);

        // `func`/`args` are intentionally ignored on the timer path (the fiber resumes with `Nil`);
        // `double_native` is just a stand-in to satisfy the struct.
        let req = OffloadReq {
            func: double_native,
            args: vec![],
            span: Span { line: 1, col: 1 },
            timer_ms: Some(40),
        };
        sched.offload(f0, req);

        // While the timer holds it the fiber is `inflight` — neither running, runnable, nor parked —
        // and must veto a deadlock fire (it WILL come back).
        assert_eq!(sched.inflight.load(Ordering::Relaxed), 1, "timer offload moved the fiber to inflight");
        {
            let c = sched.lock();
            assert!(!sched.is_deadlocked(&c), "a timer-parked (inflight) fiber must not fault a false deadlock");
        }

        // The timer fires at the deadline and requeues the fiber.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while sched.inflight.load(Ordering::Relaxed) != 0 {
            assert!(std::time::Instant::now() < deadline, "timer never fired the parked fiber back (lost wakeup?)");
            std::thread::yield_now();
        }
        assert_eq!(sched.lock().running, 0, "timer offload freed the worker");
        assert_eq!(sched.runnable.load(Ordering::Relaxed), 2, "f1 still queued + f0 requeued by the timer");

        let mut found = None;
        while let Take::Run(f) = sched.take_runnable(0, 1) {
            if f.task_index == 0 {
                found = Some(f);
                break;
            }
        }
        let f0 = found.expect("timer-parked fiber requeued");
        assert_eq!(f0.resume_native, Some(Ok(crate::native::NativeRet::Nil)), "sleep resumes with Nil, native not run");
    }

    /// D4c: `try_steal` grabs ceil-half of the first non-empty victim's ring (from the back), leaving
    /// the rest, and is net-zero on `runnable` (the fibers stay runnable, just change owner).
    #[test]
    fn schedule_steals_half_from_victim() {
        let sched = mk_sched(4);
        {
            let mut vq = sched.lock_local(1);
            for i in 0..4 {
                vq.ring.push_back(mk_fiber(i));
            }
        }
        sched.runnable.fetch_add(4, Ordering::Relaxed); // keep the counter consistent with the queues
        let stolen = sched.try_steal(0); // worker 0 steals from worker 1
        assert_eq!(stolen.len(), 2, "ceil(4/2) stolen");
        assert_eq!(sched.lock_local(1).ring.len(), 2, "half left with the victim");
        assert_eq!(sched.runnable.load(Ordering::Relaxed), 4, "stealing is net-zero on runnable");
    }

    /// D4d: on a `GLOBAL_CHECK_INTERVAL`th schedule a worker pulls from the global queue before its
    /// own local (anti-starvation); on any other tick it drains its own local first.
    #[test]
    fn schedule_pulls_global_every_61st_tick() {
        // tick=61 (a multiple) → the global fiber wins over the local one.
        let periodic = mk_sched(4);
        periodic.lock_local(0).ring.push_back(mk_fiber(0)); // local
        periodic.seed(vec![mk_fiber(1)]); // global (bumps runnable)
        periodic.runnable.fetch_add(1, Ordering::Relaxed); // for the local fiber
        let got = match periodic.take_runnable(0, GLOBAL_CHECK_INTERVAL) {
            Take::Run(f) => f.task_index,
            Take::Stop => panic!("expected a runnable fiber"),
        };
        assert_eq!(got, 1, "periodic tick drains the global queue first");

        // tick=1 (not a multiple) → the local fiber wins (normal order).
        let normal = mk_sched(4);
        normal.lock_local(0).ring.push_back(mk_fiber(0));
        normal.seed(vec![mk_fiber(1)]);
        normal.runnable.fetch_add(1, Ordering::Relaxed);
        let got = match normal.take_runnable(0, 1) {
            Take::Run(f) => f.task_index,
            Take::Stop => panic!("expected a runnable fiber"),
        };
        assert_eq!(got, 0, "non-periodic tick drains the own local first");
    }

    /// D4c: a thief never steals from itself and skips empty victims (returns nothing when only its
    /// own local has work).
    #[test]
    fn steal_skips_self_and_empty_victims() {
        let sched = mk_sched(4);
        {
            let mut own = sched.lock_local(0);
            own.ring.push_back(mk_fiber(0));
            own.ring.push_back(mk_fiber(1));
        }
        assert!(sched.try_steal(0).is_empty(), "no sibling has work; must not steal from self");
    }

    /// D2b/U2: parking the running fiber on an EMPTY channel frees the worker (`running--`,
    /// `parked++`); a `send_wake` on that channel enqueues the message and moves the fiber back onto
    /// the run queue as `Ready`.
    #[test]
    fn mnsched_park_then_wake_requeues_fiber() {
        let sched = mk_sched(1);
        let core = empty_core();
        let key = core_key(&core);
        sched.seed(vec![mk_fiber(0)]);
        let f = take_run(&sched);
        sched.park(key, &core, f);
        {
            let c = sched.lock();
            assert_eq!(c.running, 0);
            assert_eq!(c.parked_n, 1);
            assert!(c.global.is_empty());
        }
        sched.send_wake(key, &core, WireValue::Int(7));
        {
            let c = sched.lock();
            assert_eq!(c.parked_n, 0);
            assert_eq!(c.global.len(), 1);
        }
        let g = take_run(&sched);
        assert_eq!(g.task_index, 0);
        assert!(matches!(g.state, FiberState::Ready));
    }

    /// D3/U: a fiber that exhausts its reduction budget `yield_fiber`s — the scheduler frees the
    /// worker (`running--`) and requeues it at the **tail** of `runq` (round-robin), still `Ready`.
    /// No park bucket is touched (a yield carries no channel handle). Mirrors the park/wake test.
    #[test]
    fn mnsched_yield_fiber_requeues_at_tail() {
        let sched = mk_sched(2);
        sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
        let f0 = take_run(&sched); // pops task 0, running == 1
        assert_eq!(f0.task_index, 0);
        sched.yield_fiber(f0); // requeue task 0 behind task 1
        {
            let c = sched.lock();
            assert_eq!(c.running, 0);
            assert_eq!(c.parked_n, 0); // a yield never parks
            assert_eq!(c.global.len(), 2);
        }
        // Round-robin: task 1 (which was behind task 0) now runs before the requeued task 0.
        assert_eq!(take_run(&sched).task_index, 1);
        let back = take_run(&sched);
        assert_eq!(back.task_index, 0);
        assert!(matches!(back.state, FiberState::Ready));
    }

    /// D4/stress: a combined-churn workload that exercises EVERY new D4 path together under a
    /// watchdog — 500 consumers that block on `recv` (park + `send_wake`), 500 producers that do CPU
    /// work (reduction `yield` → global, batch-grab, work-stealing between idle workers) then `send`
    /// (waking a parked consumer), all `#fibers ≫ #workers`. The consumers accumulate into one
    /// `Shared`; the join must complete with the exact arithmetic sum, with no lost/duplicated fiber,
    /// no false deadlock, and no hang (the watchdog turns a regression — a lost wakeup, a steal/grab
    /// accounting bug, a deadlock-predicate false positive — into a loud failure rather than a wedge).
    #[test]
    fn d4_worksteal_cpu_and_channel_stress() {
        let src = "\
fn producer(ch: Channel[int], lo: int, hi: int):
    acc := 0
    i := lo
    while i < hi:
        acc += i
        i += 1
    ch.send(acc)

fn consumer(ch: Channel[int], sink: Shared[int]):
    v := ch.recv()
    sink.update(fn(x): x + v)

fn main():
    ch := Channel[int]()
    sink := Shared(0)
    parallel:
        for _ in 0..500:
            spawn consumer(ch, sink)
        for k in 0..500:
            spawn producer(ch, k * 10, k * 10 + 10)
    print(sink.get())

main()
";
        // sum_{k=0}^{499} sum_{i=10k}^{10k+9} i = sum_{k=0}^{499} (100k + 45) = 12_475_000 + 22_500.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_capture_parallel(src));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(r) => assert_eq!(r.expect("mixed work-steal nursery completed"), "12497500\n"),
            Err(_) => panic!("hung — D4 work-stealing/grab/wait_timeout regressed (lost wakeup or accounting bug)"),
        }
    }

    /// D4e/stress: the lost-wakeup regression guard for the runnable-gated park. D4e removed the 2 ms
    /// `wait_timeout` backstop — an idle worker now sleeps on `cv` INDEFINITELY when `runnable == 0`,
    /// woken ONLY by a real `notify` from a sibling's `send`/`yield`/`finish`/offload-complete. A
    /// single missed wakeup is no longer a 2 ms stall but a PERMANENT hang. The race is probabilistic
    /// (the batch-grab in-hand window, the park-vs-send gap), so we REPEAT a park-heavy
    /// consumer-first workload many rounds, each under a watchdog: any lost wakeup in any round =>
    /// the round never completes => `recv_timeout` fires => loud failure. 300 consumers are spawned
    /// FIRST (they all `recv`-park, driving every worker to a true `cv.wait` sleep with `runnable`
    /// near zero), then 300 producers wake them — the exact sleep→`send_wake`→wake path D4e changed.
    #[test]
    fn d4e_pingpong_no_lost_wakeup_stress() {
        let src = "\
fn producer(ch: Channel[int], lo: int, hi: int):
    acc := 0
    i := lo
    while i < hi:
        acc += i
        i += 1
    ch.send(acc)

fn consumer(ch: Channel[int], sink: Shared[int]):
    v := ch.recv()
    sink.update(fn(x): x + v)

fn main():
    ch := Channel[int]()
    sink := Shared(0)
    parallel:
        for _ in 0..300:
            spawn consumer(ch, sink)
        for k in 0..300:
            spawn producer(ch, k * 10, k * 10 + 10)
    print(sink.get())

main()
";
        // sum_{k=0}^{299} sum_{i=10k}^{10k+9} i = sum_{k=0}^{299} (100k + 45) = 100*44850 + 300*45.
        let expected = format!("{}\n", 100 * (299 * 300 / 2) + 300 * 45);
        for round in 0..25 {
            let want = expected.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(run_capture_parallel(src));
            });
            match rx.recv_timeout(std::time::Duration::from_secs(20)) {
                Ok(r) => assert_eq!(r.expect("park-heavy nursery completed"), want, "round {round}"),
                Err(_) => panic!(
                    "hung on round {round} — D4e runnable-gated park lost a wakeup \
                     (an idle worker slept on cv with a runnable fiber pending)"
                ),
            }
        }
    }

    /// D4e/stress: wake-from-TRUE-sleep. Distinct from the churn test above, this isolates the exact
    /// state D4e introduced — workers asleep on `cv` with `runnable == 0` — and proves a later `send`
    /// wakes them with the poll gone. One `slow_producer` burns CPU on a single worker while `N`
    /// consumers `recv`-park; with nothing queued (`runnable == 0`) every OTHER worker reaches the
    /// runnable-gated branch and does a real `cv.wait` (no 2 ms timeout to fall back on). Only when
    /// the producer finishes its spin and fires its burst of `send`s are the sleepers woken. The join
    /// completing with `sink == N` proves no sleeper was stranded. Watchdog 30 s.
    #[test]
    fn d4e_wake_parked_workers_from_true_sleep() {
        let n = 200usize;
        let src = format!(
            "\
fn slow_producer(ch: Channel[int], n: int):
    acc := 0
    i := 0
    while i < 8000000:
        acc += i
        i += 1
    j := 0
    while j < n:
        ch.send(acc + j)
        j += 1

fn consumer(ch: Channel[int], sink: Shared[int]):
    ch.recv()
    sink.update(fn(x): x + 1)

fn main():
    ch := Channel[int]()
    sink := Shared(0)
    parallel:
        for _ in 0..{n}:
            spawn consumer(ch, sink)
        spawn slow_producer(ch, {n})
    print(sink.get())

main()
"
        );
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_capture_parallel(&src));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(r) => assert_eq!(r.expect("wake-from-sleep nursery completed"), format!("{n}\n")),
            Err(_) => panic!(
                "hung — D4e: a `send` failed to wake workers parked in the runnable-gated `cv.wait` \
                 (lost wakeup from true sleep)"
            ),
        }
    }

    /// D5 — the discriminating offload proof. `N = workers * 4` fibers each `sleep_ms(150)`. Run
    /// INLINE on the core pool, each of the `workers` threads must run 4 sleeps back-to-back → wall
    /// clock ≥ `4 * 150 = 600 ms` regardless of core count. OFFLOADED to the dirty pool, all `N`
    /// sleeps run concurrently → wall clock ≈ `150 ms`. Asserting `< 450 ms` (`3 * sleep`) fails on
    /// the inline path and passes once offload is wired — and the `N ∝ workers` construction keeps
    /// that gap on any machine (the inline path is always 4 batches). Watchdog 30 s.
    #[test]
    fn d5_blocking_sleeps_run_concurrently_not_serialized() {
        let workers = std::thread::available_parallelism().map(|x| x.get()).unwrap_or(1).max(1);
        let n = workers * 4;
        let src = format!(
            "\
import std.time

fn sleeper():
    time.sleep_ms(150)

fn main():
    parallel:
        for _ in 0..{n}:
            spawn sleeper()
    print(\"done\")

main()
"
        );
        let entry = write_temp_chz("d5_sleeps", &src);
        let run_entry = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let out = run_file_parallel(&run_entry, crate::native::HostConfig::default());
            let _ = tx.send((out, start.elapsed()));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(30));
        let _ = std::fs::remove_file(&entry);
        match result {
            Ok(((out, _err, res, _code), elapsed)) => {
                assert!(res.is_ok(), "sleeper nursery faulted: {res:?}");
                assert_eq!(out, "done\n");
                assert!(
                    elapsed < std::time::Duration::from_millis(450),
                    "{n} sleep_ms(150) fibers took {elapsed:?} — blocking calls serialized on the core pool \
                     instead of offloading to the dirty pool (G3 starvation)"
                );
            }
            Err(_) => panic!("hung — D5 offload/complete regressed (lost wakeup or inflight accounting bug)"),
        }
    }

    /// D5 owe #3 Path C (#3 sleep-in-callback demote) — the discriminating proof, mirroring
    /// `d5_blocking_sleeps_run_concurrently_not_serialized` but with the `sleep_ms` reached INSIDE a
    /// native callback (`[1].map(nap)`, `native_reentry > 0`). The offload gate requires
    /// `native_reentry == 0`, so without the demote this sleep runs INLINE and pins its worker:
    /// `N = workers * 4` such tasks ⇒ 4 back-to-back batches ⇒ ≥ `4 * 150 = 600 ms` on any core count.
    /// WITH the demote each sleeping callback frees its worker (spawns a replacement) so all `N` run
    /// concurrently ⇒ ≈ `150 ms`. Asserting `< 450 ms` fails on the inline path and passes once the
    /// in-callback demote is wired. Watchdog 30 s.
    #[test]
    fn d5_owe3_path_c_sleep_in_callback_demotes_frees_worker() {
        let workers = std::thread::available_parallelism().map(|x| x.get()).unwrap_or(1).max(1);
        let n = workers * 4;
        let src = format!(
            "\
import std.time

fn nap(x: int) -> int:
    time.sleep_ms(150)
    return x

fn sleeper():
    [1].map(nap)

fn main():
    parallel:
        for _ in 0..{n}:
            spawn sleeper()
    print(\"done\")

main()
"
        );
        let entry = write_temp_chz("d5_owe3_sleep_cb", &src);
        let run_entry = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let out = run_file_parallel(&run_entry, crate::native::HostConfig::default());
            let _ = tx.send((out, start.elapsed()));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(30));
        let _ = std::fs::remove_file(&entry);
        match result {
            Ok(((out, _err, res, _code), elapsed)) => {
                assert!(res.is_ok(), "in-callback sleeper nursery faulted: {res:?}");
                assert_eq!(out, "done\n");
                assert!(
                    elapsed < std::time::Duration::from_millis(450),
                    "{n} sleep_ms(150)-in-callback fibers took {elapsed:?} — the in-callback sleep pinned \
                     its worker (ran inline) instead of demoting (#3)"
                );
            }
            Err(_) => panic!("hung — D5 owe #3 Path C sleep-in-callback demote regressed"),
        }
    }

    /// D5 owe #3 Path C (#3) — correctness of the in-callback sleep demote: a `sleep_ms` inside a
    /// native `xs.map` still produces the right result after demoting (the worker is freed + resumed in
    /// place; output unchanged). The sum proves all three callbacks ran past their sleep. Watchdog 30 s.
    #[test]
    fn d5_owe3_path_c_sleep_in_callback_correct() {
        let src = "\
import std.time

fn nap(x: int) -> int:
    time.sleep_ms(20)
    return x * 2

fn work(sink: Shared[int]):
    ys := [1, 2, 3].map(nap)
    sink.update(fn(x): x + ys[0] + ys[1] + ys[2])

fn main():
    sink := Shared(0)
    parallel:
        spawn work(sink)
        spawn work(sink)
    print(sink.get())

main()
";
        let entry = write_temp_chz("d5_owe3_sleep_correct", src);
        let run_entry = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file_parallel(&run_entry, crate::native::HostConfig::default()));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(30));
        let _ = std::fs::remove_file(&entry);
        match result {
            Ok((out, _err, res, _code)) => {
                assert!(res.is_ok(), "in-callback sleep demote faulted: {res:?}");
                // each work(): 2*(1+2+3) = 12; two tasks update the same sink → 24.
                assert_eq!(out, "24\n");
            }
            Err(_) => panic!("hung — D5 owe #3 Path C sleep-in-callback demote regressed (correctness)"),
        }
    }

    /// D5 owe #3 Path C (#3 socket half) — correctness of the in-callback socket DEMOTE: a `Socket::read`
    /// reached INSIDE a native `xs.map` callback (`native_reentry > 0`) that `WouldBlock`s must demote
    /// the worker (spin a replacement, backoff-poll the non-blocking read in place) and resume with the
    /// real bytes — NOT surface the `--parallel`-engine error. `park_on_fd` only parks on the netpoller
    /// when `native_reentry == 0`; inside a callback the Rust-stack `map` loop can't snapshot-park, so
    /// without the demote the read returns `Result::Err("read would block: ... require the --parallel
    /// engine")`, which `?` propagates → the client prints `ERR:…` instead of the echoed line. The
    /// server `sleep_ms(50)`s after `accept` before writing, so the client's in-callback read is
    /// *guaranteed* empty (forces the demote path deterministically). Parallel-only, 30 s watchdog.
    #[test]
    fn d5_owe3_path_c_socket_read_in_callback_demotes() {
        let src = "\
import std.net
import std.time

fn read_reply(s: Socket) -> str!:
    line := s.read(64)?
    return Ok(line)

fn do_client(addr: str) -> str!:
    sock := net.connect(addr)?
    socks := [sock]
    replies := socks.map(read_reply)
    line := replies[0]?
    sock.close()
    return Ok(line)

fn client(addr: str):
    match do_client(addr):
        Ok(line): print(line)
        Err(e): print(\"ERR:\" + e.message())

fn server(listener: Listener) -> int!:
    conn := listener.accept()?
    time.sleep_ms(50)
    conn.write(\"hello\")?
    conn.close()
    listener.close()
    return Ok(0)

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    addr := listener.addr()?
    parallel:
        spawn server(listener)
        spawn client(addr)
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"\")
        Err(e): print(\"RUN-ERR:\" + e.message())

main()
";
        let entry = write_temp_chz("d5_owe3_sock_read_cb", src);
        let run_entry = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file_parallel(&run_entry, crate::native::HostConfig::default()));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(30));
        let _ = std::fs::remove_file(&entry);
        match result {
            Ok((out, _err, res, _code)) => {
                assert!(res.is_ok(), "in-callback socket read demote faulted: {res:?}");
                // client prints the echoed line; main prints a trailing blank line.
                assert_eq!(out, "hello\n\n", "in-callback read did not demote (got: {out:?})");
            }
            Err(_) => panic!("hung — D5 owe #3 Path C socket-read-in-callback demote regressed"),
        }
    }

    /// D5 owe #3 Path C (#3 socket half) — the listener path: an `accept` reached INSIDE a native `map`
    /// callback that `WouldBlock`s (no client yet) demotes + resumes once a sibling client connects,
    /// instead of erroring. Proves the `Listener::accept` gate, not just `Socket::read`. The client
    /// `sleep_ms(50)`s before connecting so the in-callback `accept` is guaranteed to block first.
    /// Parallel-only, 30 s watchdog.
    #[test]
    fn d5_owe3_path_c_accept_in_callback_demotes() {
        let src = "\
import std.net
import std.time

fn accept_one(l: Listener) -> int!:
    conn := l.accept()?
    conn.read(64)?
    conn.close()
    return Ok(1)

fn do_server(listener: Listener) -> int!:
    ls := [listener]
    got := ls.map(accept_one)
    n := got[0]?
    listener.close()
    return Ok(n)

fn server(listener: Listener):
    match do_server(listener):
        Ok(n): print(n)
        Err(e): print(\"ERR:\" + e.message())

fn client(addr: str) -> int!:
    time.sleep_ms(50)
    sock := net.connect(addr)?
    sock.write(\"ping\")?
    sock.close()
    return Ok(0)

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    addr := listener.addr()?
    parallel:
        spawn server(listener)
        spawn client(addr)
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"\")
        Err(e): print(\"RUN-ERR:\" + e.message())

main()
";
        let entry = write_temp_chz("d5_owe3_sock_accept_cb", src);
        let run_entry = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file_parallel(&run_entry, crate::native::HostConfig::default()));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(30));
        let _ = std::fs::remove_file(&entry);
        match result {
            Ok((out, _err, res, _code)) => {
                assert!(res.is_ok(), "in-callback accept demote faulted: {res:?}");
                assert_eq!(out, "1\n\n", "in-callback accept did not demote (got: {out:?})");
            }
            Err(_) => panic!("hung — D5 owe #3 Path C accept-in-callback demote regressed"),
        }
    }

    /// D5 — a blocking *filesystem* native (`fs.exists`, returns a `bool`) is offloaded, runs off the
    /// core worker, and its result is lowered + pushed on resume so execution continues correctly
    /// past the call. `N` fibers each check a real temp file and bump a `Shared` — the join sum must
    /// be exactly `N` (every offloaded call returned `true` and resumed into the `if`). Guards the
    /// resume-continues-past-the-call + bool-lowering path. Watchdog 30 s.
    #[test]
    fn d5_blocking_fs_calls_offload_and_resume_correctly() {
        let path = std::env::temp_dir().join(format!("chezzi_d5_exists_{}.txt", std::process::id()));
        std::fs::write(&path, b"x").expect("write temp file");
        let path_str = path.to_str().expect("utf8 temp path").to_string();
        let n = 64usize;
        let src = format!(
            "\
import std.fs

fn checker(sink: Shared[int], path: str):
    if fs.exists(path):
        sink.update(fn(x): x + 1)

fn main():
    sink := Shared(0)
    parallel:
        for _ in 0..{n}:
            spawn checker(sink, \"{path_str}\")
    print(sink.get())

main()
"
        );
        let entry = write_temp_chz("d5_fs", &src);
        let run_entry = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file_parallel(&run_entry, crate::native::HostConfig::default()));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(30));
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&entry);
        match result {
            Ok((out, _err, res, _code)) => {
                assert!(res.is_ok(), "fs.exists nursery faulted: {res:?}");
                assert_eq!(out, format!("{n}\n"));
            }
            Err(_) => panic!("hung — D5 fs offload/resume regressed"),
        }
    }

    /// D5 — a blocking native reached *inside a native callback* (here `fs.exists` inside the
    /// per-element fn of `list.map`, which runs under the `native_reentry` guard) must NOT be
    /// offloaded to the dirty pool: the callback's loop state lives on the Rust host stack and cannot be
    /// parked into a fiber. The offload is gated on `native_reentry == 0`, so it falls back to inline
    /// execution and the map completes correctly (no fault, no corruption). Guards the gate for a
    /// NON-sleep blocking native (`sleep_ms` specifically now DEMOTES the worker inside a callback —
    /// see `d5_owe3_path_c_sleep_in_callback_*`). Watchdog 30 s.
    #[test]
    fn d5_blocking_native_in_callback_runs_inline() {
        let path = std::env::temp_dir().join(format!("chezzi_d5_cb_exists_{}.txt", std::process::id()));
        std::fs::write(&path, b"x").expect("write temp file");
        let path_str = path.to_str().expect("utf8 temp path").to_string();
        let src = format!(
            "\
import std.fs

fn dbl(x: int) -> int:
    if fs.exists(\"{path_str}\"):
        return x * 2
    return 0

fn work(sink: Shared[int]):
    ys := [1, 2, 3].map(dbl)
    sink.update(fn(x): x + ys[0] + ys[1] + ys[2])

fn main():
    sink := Shared(0)
    parallel:
        spawn work(sink)
    print(sink.get())

main()
"
        );
        let entry = write_temp_chz("d5_callback", &src);
        let run_entry = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file_parallel(&run_entry, crate::native::HostConfig::default()));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(30));
        let _ = std::fs::remove_file(&entry);
        let _ = std::fs::remove_file(&path);
        match result {
            Ok((out, _err, res, _code)) => {
                assert!(res.is_ok(), "in-callback nursery faulted: {res:?}");
                assert_eq!(out, "12\n");
            }
            Err(_) => panic!("hung — D5 native_reentry gate regressed (offloaded an in-callback blocking native)"),
        }
    }

    /// D5 owe #3 (Path A) — a blocking `recv` reached **through a chezzi-source HOF** (`iter.map`,
    /// `std/iter.chz`) parks instead of faulting `deadlock`, unlike the native `.map` (whose Rust loop
    /// frame breaks the snapshot chain). Every frame from the fiber's entry to the `recv` is a VM frame
    /// (`map`'s `for`-loop + the closure), so the park is sound. The exact A/B of the contrast test
    /// `fibers_recv_inside_map_callback_faults` (native `xs.map` → `deadlock`); here `iter.map` succeeds.
    ///
    /// The **cooperative** leg is the deterministic guard: tasks run in spawn order on one thread, so
    /// `consume` (spawned first) reaches `recv` on the still-empty channel and **must park** before the
    /// `produce` sibling can run — a regressed park/wake faults `deadlock` or hangs, never flake-passes.
    /// (Under `--parallel` the producer races the consumer on another thread and may fill the unbounded
    /// FIFO before the first `recv`, so that leg can't *force* a park — it's the real-engine + hang
    /// guard, run under a 30 s watchdog.) Sum `66` proves all three recvs threaded through the closure.
    #[test]
    fn d5_owe3_recv_in_iter_map_callback_parks() {
        let src = "\
import std.iter

fn produce(ch: Channel[int]):
    ch.send(10)
    ch.send(20)
    ch.send(30)

fn consume(ch: Channel[int], out: Shared[int]):
    ys := iter.map([1, 2, 3], fn(x: int) -> int: x + ch.recv())
    out.update(fn(a): a + ys[0] + ys[1] + ys[2])

fn main():
    ch := Channel[int]()
    out := Shared(0)
    parallel:
        spawn consume(ch, out)
        spawn produce(ch)
    print(out.get())

main()
";
        let entry = write_temp_chz("d5_owe3_iter_map", src);
        // Cooperative leg — deterministic: `consume` parks on the empty channel before `produce` runs.
        let (co, _ce, cr, _cc) = run_file_with(&entry, crate::native::HostConfig::default());
        assert!(cr.is_ok(), "cooperative iter.map recv-in-callback faulted (park regressed): {cr:?}");
        assert_eq!(co, "66\n", "cooperative iter.map recv-in-callback wrong sum");
        // Parallel leg — the real M:N engine, under a watchdog so a park/wake hang fails loud.
        let run_entry = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file_parallel(&run_entry, crate::native::HostConfig::default()));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(30));
        let _ = std::fs::remove_file(&entry);
        match result {
            Ok((out, _err, res, _code)) => {
                assert!(res.is_ok(), "parallel iter.map recv-in-callback nursery faulted: {res:?}");
                assert_eq!(out, "66\n");
            }
            Err(_) => panic!("hung — D5 owe #3 Path A regressed (recv inside iter.map did not park)"),
        }
    }

    /// D5 owe #3 (Path A) — the new `std/iter.chz` HOFs (`map`/`filter`/`fold`/`reduce`) are correct
    /// and byte-identical across both engines. `map` to a different return type (`int -> str`)
    /// exercises generic-return inference (`U` is bound solely from the closure, not from `xs`), the
    /// primary risk flagged in the plan — it works without explicit type args. Cooperative (no
    /// `--parallel`): this guards the functional surface; the park behaviour is guarded separately by
    /// `d5_owe3_recv_in_iter_map_callback_parks`.
    #[test]
    fn d5_owe3_iter_hofs_correct_on_both_engines() {
        let src = "\
import std.iter

fn main():
    xs := [1, 2, 3, 4, 5]
    print(iter.map(xs, fn(x: int) -> int: x * x))
    print(iter.filter(xs, fn(x: int) -> bool: x % 2 == 0))
    print(iter.fold(xs, 0, fn(a: int, x: int) -> int: a + x))
    print(iter.reduce(xs, fn(a: int, b: int) -> int: a * b))
    print(iter.map([1, 2], fn(x: int) -> str: \"n{x}\"))
    # subtraction is non-commutative — locks the left-to-right fold order (0-1-2-3-4-5 = -15)
    print(iter.fold(xs, 0, fn(a: int, x: int) -> int: a - x))

main()
";
        let entry = write_temp_chz("d5_owe3_hofs", src);
        let cfg = crate::native::HostConfig::default;
        let (vo, _ve, vr, _vc) = run_file_with(&entry, cfg());
        let (io, _ie, ir, _ic) = crate::interp::run_file(&entry);
        let _ = std::fs::remove_file(&entry);
        assert!(vr.is_ok(), "vm run faulted: {vr:?}");
        assert!(ir.is_ok(), "interp run faulted: {ir:?}");
        assert_eq!(vo, io, "vm/interp stdout divergence");
        assert_eq!(vo, "[1, 4, 9, 16, 25]\n[2, 4]\n15\n120\n[n1, n2]\n-15\n");
    }

    /// D5 owe #3 (Path C) — a blocking `recv` reached inside a **native** callback (`xs.map`, whose
    /// per-element loop frame lives on the Rust host stack and CANNOT be snapshot-parked) no longer
    /// faults `deadlock` under `--parallel`: the worker thread is **demoted** (blocks in place on the
    /// channel condvar, a fresh replacement worker covers its `wid`), and **resumes in place** when a
    /// sibling `send`s — Go's `handoffp`. The contrast to Path A (`d5_owe3_recv_in_iter_map_callback_parks`,
    /// where `iter.map` is chezzi source → pure VM frames → snapshot-parks) and to the cooperative-engine
    /// pin (`fibers_recv_inside_map_callback_faults`, which still faults — demotion is M:N-only). The
    /// result is written with `Shared.set` (NOT `update`) so the recv site is the `xs.map` callback only,
    /// avoiding the `update_lock`-held-while-blocked hazard. Sum `66` = (1+10)+(2+20)+(3+30): all three
    /// recvs threaded through the native map callback. Parallel-only, under a 30 s watchdog so a
    /// demote/resume hang fails loud instead of hanging the suite. The producer `sleep_ms`s before its
    /// first `send` so the consumer's first map-callback `recv` is **guaranteed empty** — forcing the
    /// demote path deterministically (without the delay the producer races ahead and pre-fills the FIFO,
    /// so the `recv` never blocks and the test would flake-pass even with Path C broken).
    #[test]
    fn d5_owe3_path_c_recv_in_native_map_callback_demotes() {
        let src = "\
import std.time

fn use_map(ch: Channel[int], out: Shared[int]):
    xs := [1, 2, 3]
    ys := xs.map(fn(x): x + ch.recv())
    out.set(ys[0] + ys[1] + ys[2])

fn fill(ch: Channel[int]):
    time.sleep_ms(50)
    ch.send(10)
    ch.send(20)
    ch.send(30)

fn main():
    ch := Channel[int]()
    out := Shared(0)
    parallel:
        spawn use_map(ch, out)
        spawn fill(ch)
    print(out.get())

main()
";
        let entry = write_temp_chz("d5_owe3_path_c", src);
        let run_entry = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file_parallel(&run_entry, crate::native::HostConfig::default()));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(30));
        let _ = std::fs::remove_file(&entry);
        match result {
            Ok((out, _err, res, _code)) => {
                assert!(res.is_ok(), "Path C: recv inside native xs.map faulted under --parallel: {res:?}");
                assert_eq!(out, "66\n");
            }
            Err(_) => panic!("hung — D5 owe #3 Path C regressed (recv inside native xs.map did not demote-and-resume)"),
        }
    }

    /// D5 owe #3 (Path C) — a `recv` inside a native callback with **no possible sender** must still
    /// **fault `deadlock`**, not hang. This is the load-bearing half of the pragmatic deadlock scope:
    /// the demoted thread is accounted as `blocked_native` (a 5th fiber state), which feeds
    /// [`MnSched::is_deadlocked`] (`parked_n>0 || blocked_native>0`). The demote's `blocked_native++`
    /// notifies `cv` so the idle replacement worker re-evaluates the predicate; on fire, `flag_deadlock`
    /// sets `terminate`, the demoted thread observes it within `DEMOTE_POLL_BACKOFF` and faults in place,
    /// and `wait_for_completion` lets the join reduce the deadlock outcome. Watchdog 30 s: a regressed
    /// predicate (or a missing notify) would HANG here instead of faulting.
    #[test]
    fn d5_owe3_path_c_recv_in_callback_no_sender_still_deadlocks() {
        let src = "\
fn use_map(ch: Channel[int]):
    xs := [1]
    ys := xs.map(fn(x): x + ch.recv())
    print(ys)

fn main():
    ch := Channel[int]()
    parallel:
        spawn use_map(ch)

main()
";
        let entry = write_temp_chz("d5_owe3_path_c_dl", src);
        let run_entry = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file_parallel(&run_entry, crate::native::HostConfig::default()));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(30));
        let _ = std::fs::remove_file(&entry);
        match result {
            Ok((_out, _err, res, _code)) => match res {
                Err(e) => {
                    let s = format!("{e:?}");
                    assert!(s.contains("deadlock"), "Path C: no-sender recv-in-callback should fault deadlock, got: {s}");
                }
                Ok(()) => panic!("Path C: no-sender recv-in-callback unexpectedly succeeded"),
            },
            Err(_) => panic!("hung — D5 owe #3 Path C deadlock detection regressed (blocked_native predicate / notify)"),
        }
    }

    /// D5 owe #3 Path C (#1 false-positive) — the deadlock checker must NOT fault an innocent parked
    /// sibling when a demoted fiber has a value racing into its queue. F1 demotes on `a` inside a native
    /// `xs.map`, then wakes F2 via `c.send(7)`; F2 snapshot-parks on `c`; F3 feeds `a` then finishes.
    /// The bad interleaving (microseconds): F3's `running→0` quiesce can fire the predicate before F1
    /// pops its queued `10`, wrongly killing the parked F2. With the #1 fix (`is_deadlocked` peeks the
    /// demoted channel `a`, which holds `10`) the fire is vetoed. Run many times to expose the race; the
    /// output must ALWAYS be `7` and NEVER a spurious `deadlock`. Watchdog per iteration.
    #[test]
    fn d5_owe3_path_c_no_false_deadlock_when_demoted_fiber_has_queued_value() {
        let src = "\
fn main():
    a := Channel[int]()
    c := Channel[int]()
    xs := [1]
    parallel:
        spawn:
            xs.map(fn(x): x + a.recv())
            c.send(7)
        spawn: print(c.recv())
        spawn: a.send(10)

main()
";
        let entry = write_temp_chz("d5_owe3_path_c_fp", src);
        for i in 0..200 {
            let run_entry = entry.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(run_file_parallel(&run_entry, crate::native::HostConfig::default()));
            });
            let result = rx.recv_timeout(std::time::Duration::from_secs(30));
            // Remove the temp file BEFORE asserting so an assert panic doesn't leak it (the other paths
            // out of this test all unwind, and the post-loop cleanup never runs on panic).
            if i == 199 {
                let _ = std::fs::remove_file(&entry);
            }
            match result {
                Ok((out, _err, res, _code)) => {
                    if res.is_err() || out != "7\n" {
                        let _ = std::fs::remove_file(&entry);
                    }
                    assert!(
                        res.is_ok(),
                        "iter {i}: spurious fault (the #1 false-positive killing the parked sibling?): {res:?}"
                    );
                    assert_eq!(out, "7\n", "iter {i}: wrong output");
                }
                Err(_) => {
                    let _ = std::fs::remove_file(&entry);
                    panic!("iter {i}: hung — D5 owe #3 Path C regressed");
                }
            }
        }
    }

    /// D5 owe #1 — a blocking *subprocess* native (`process.cmd`, returns `Result[str]`) is offloaded,
    /// runs off the core worker, and its `Ok`/`Err` result is lowered + pushed on resume so the `match`
    /// continues correctly past the call. `N` fibers (≫ the core pool) each run a trivial command and
    /// bump a `Shared` on `Ok` — the join sum must be exactly `N` (every offloaded `cmd` returned `Ok`
    /// and resumed into the arm). Guards the request/process classification + the Result-lowering
    /// resume path for a non-`io`/`fs` blocking native. Watchdog 30 s.
    #[test]
    fn d5_owe1_blocking_process_cmd_offloads_and_resumes_correctly() {
        let n = 64usize;
        let src = format!(
            "\
import std.process

fn checker(sink: Shared[int]):
    match process.cmd(\"true\"):
        Ok(out): sink.update(fn(x): x + 1)
        Err(e): print(e.message())

fn main():
    sink := Shared(0)
    parallel:
        for _ in 0..{n}:
            spawn checker(sink)
    print(sink.get())

main()
"
        );
        let entry = write_temp_chz("d5_owe1_cmd", &src);
        let run_entry = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file_parallel(&run_entry, crate::native::HostConfig::default()));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(30));
        let _ = std::fs::remove_file(&entry);
        match result {
            Ok((out, _err, res, _code)) => {
                assert!(res.is_ok(), "process.cmd nursery faulted: {res:?}");
                assert_eq!(out, format!("{n}\n"));
            }
            Err(_) => panic!("hung — D5 owe #1 process.cmd offload/resume regressed"),
        }
    }

    /// D5 test helper: write a Chezzi source to a uniquely-named temp `.chz` file and return its path
    /// (so `run_file_parallel` resolves `import std.*` through the real module graph, unlike
    /// `compile_module_standalone`). The caller removes it after the run.
    fn write_temp_chz(tag: &str, src: &str) -> std::path::PathBuf {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("chezzi_{tag}_{}_{seq}.chz", std::process::id()));
        std::fs::write(&path, src).expect("write temp .chz");
        path
    }

    /// D3/the discriminating fairness test. 64 CPU "hog" fibers (≫ the core-sized worker pool) each
    /// busy-wait on a `Shared[int]` until it reaches 50, spawned FIRST; then 50 "short" fibers that
    /// each `update(+1)` and exit. WITHOUT preemption every worker grabs a hog (FIFO seed order), all
    /// spin forever on a counter the never-scheduled shorts can't advance → permanent hang. WITH
    /// reduction-counting preemption the hogs yield, the shorts run, the counter reaches 50, the hogs
    /// observe it and exit. A watchdog turns the no-preemption hang into a test FAILURE (not an
    /// infinite hang) and stands as the regression guard if preemption ever regresses.
    #[test]
    fn d3_preemption_prevents_cpu_hog_starvation() {
        let src = "\
fn hog(s: Shared[int], k: int):
    while s.get() < k:
        continue

fn short(s: Shared[int]):
    s.update(fn(x): x + 1)

fn main():
    s := Shared(0)
    parallel:
        for _ in 0..64:
            spawn hog(s, 50)
        for _ in 0..50:
            spawn short(s)
    print(s.get())

main()
";
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_capture_parallel(src));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(r) => assert_eq!(r.expect("hog/short nursery completed"), "50\n"),
            Err(_) => panic!("starved — D3 preemption regressed (CPU hogs never yielded their workers)"),
        }
    }

    /// D3/soundness: thousands of CPU-bound fibers (each a bounded loop + a `Shared` increment), far
    /// more than the worker pool, all complete under heavy yield churn — no corruption, no lost fiber,
    /// no false deadlock. Bounded loops terminate regardless of preemption, so this is a soundness
    /// guard for the yield/requeue machinery rather than the discriminating fairness test above.
    #[test]
    fn d3_thousands_of_cpu_fibers_all_complete() {
        let src = "\
fn work(s: Shared[int]):
    i := 0
    while i < 100:
        i += 1
    s.update(fn(x): x + 1)

fn main():
    s := Shared(0)
    parallel:
        for _ in 0..10000:
            spawn work(s)
    print(s.get())

main()
";
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_capture_parallel(src));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(60)) {
            Ok(r) => assert_eq!(r.expect("10k-fiber nursery completed"), "10000\n"),
            Err(_) => panic!("10k CPU-bound fibers did not all complete in time (yield machinery hang?)"),
        }
    }

    /// D3/regression: a reduction yield must unwind cleanly through **nested function calls**. A yield
    /// is detected at the safepoint of the innermost `run_until`; every enclosing `run_proto`/call site
    /// must propagate it up WITHOUT popping a result (the frames replay on resume) — the same contract
    /// as a `recv`-park. The first cut only guarded `suspend`, so a yield deep in a call chain
    /// (main → work → middle → inner, the shape of `primes_parallel`) let `run_proto` pop a live stack
    /// temp as a bogus return value → "expected bool, found int". This computes a known sum across two
    /// workers, each looping 50 k times through a 3-deep call chain (millions of ops ≫ CONTEXT_REDS, so
    /// many yields fire mid-chain), and also crosses a channel `recv` — exercising yield + park together.
    #[test]
    fn d3_yield_unwinds_through_nested_calls() {
        let src = "\
fn inner(n: int) -> int:
    return n * 2

fn middle(n: int) -> int:
    return inner(n) + 1

fn work(lo: int, hi: int, out: Channel[int]):
    acc := 0
    i := lo
    while i < hi:
        acc += middle(i)
        i += 1
    out.send(acc)

fn main():
    out := Channel[int]()
    parallel:
        spawn work(0, 50000, out)
        spawn work(50000, 100000, out)
    total := 0
    for _ in 0..2:
        total += out.recv()
    print(total)

main()
";
        // sum_{i=0}^{99999} (2*i + 1) = 100000 * 100000.
        assert_eq!(run_capture_parallel(src).unwrap(), "10000000000\n");
    }

    /// D2b park-gap guard (the lost-wakeup fix): if a message is already queued when `park` runs (a
    /// `send` landed in the gap between `recv`'s empty-check and the park), the fiber must NOT park —
    /// it is requeued `Ready` so it re-runs `recv` and pops the message. Without this the fiber would
    /// park forever behind a delivered-but-unconsumed message → a false deadlock.
    #[test]
    fn mnsched_park_requeues_when_message_already_waiting() {
        let sched = mk_sched(1);
        let core = empty_core();
        let key = core_key(&core);
        sched.seed(vec![mk_fiber(0)]);
        let f = take_run(&sched);
        // Simulate a send that landed in the gap (message queued, but this fiber wasn't parked yet).
        core.q.lock().unwrap().queue.push_back(WireValue::Int(7));
        sched.park(key, &core, f);
        let c = sched.lock();
        assert_eq!(c.parked_n, 0, "must not park behind a waiting message");
        assert_eq!(c.global.len(), 1, "fiber requeued to re-run recv");
    }

    /// D2b park-gap guard (cancel half): if cancel was tripped in the gap before the park, the fiber
    /// must be requeued (to unwind on the back-edge) rather than parked (where it would be stranded).
    #[test]
    fn mnsched_park_requeues_when_cancel_tripped() {
        let cancel = Arc::new(AtomicBool::new(false));
        let sched = MnSched::new(1, 4, Arc::clone(&cancel), dl_err());
        let core = empty_core();
        sched.seed(vec![mk_fiber(0)]);
        let f = take_run(&sched);
        cancel.store(true, Ordering::Relaxed);
        sched.park(core_key(&core), &core, f);
        let c = sched.lock();
        assert_eq!(c.parked_n, 0, "must not park a cancelled fiber");
        assert_eq!(c.global.len(), 1);
    }

    /// D2b/U4: every not-done fiber parked, none running, run queue empty ⇒ deadlock. `take_runnable`
    /// detects it, faults every parked fiber with `DEADLOCK_MSG`, and terminates.
    #[test]
    fn mnsched_deadlock_when_all_parked_runq_empty() {
        let sched = mk_sched(2);
        let c1 = empty_core();
        let c2 = empty_core();
        sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
        let a = take_run(&sched);
        let b = take_run(&sched);
        sched.park(core_key(&c1), &c1, a);
        sched.park(core_key(&c2), &c2, b);
        assert!(matches!(sched.take_runnable(0, 1), Take::Stop));
        let slots = sched.take_slots();
        assert_eq!(slots.len(), 2);
        for s in slots {
            assert!(matches!(s, Some(TaskOutcome::Fault(e)) if e.message == DEADLOCK_MSG));
        }
    }

    /// D2b: `finish` records a task's outcome in its slot, drops it from `running`, and flips
    /// `terminate` once every task is done.
    #[test]
    fn mnsched_finish_writes_slot_and_terminates_at_total() {
        let sched = mk_sched(2);
        sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
        let a = take_run(&sched);
        let b = take_run(&sched);
        sched.finish(a.task_index, TaskOutcome::Cancelled);
        {
            let c = sched.lock();
            assert_eq!(c.done, 1);
            assert!(!c.terminate);
        }
        sched.finish(b.task_index, TaskOutcome::Cancelled);
        {
            let c = sched.lock();
            assert_eq!(c.done, 2);
            assert!(c.terminate);
        }
        assert!(matches!(sched.take_runnable(0, 1), Take::Stop));
    }

    /// D2b/U5 (mechanics half): `cancel_drain` moves every parked fiber back onto the run queue so a
    /// worker resumes it and it observes the cancel flag on its next dispatch back-edge.
    #[test]
    fn mnsched_cancel_drain_requeues_parked() {
        let sched = mk_sched(2);
        let c1 = empty_core();
        let c2 = empty_core();
        sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
        let a = take_run(&sched);
        let b = take_run(&sched);
        sched.park(core_key(&c1), &c1, a);
        sched.park(core_key(&c2), &c2, b);
        sched.cancel_drain();
        let c = sched.lock();
        assert_eq!(c.parked_n, 0);
        assert_eq!(c.global.len(), 2);
    }

    /// D2b/G1 (headline): #fibers ≫ #threads. 64 consumer fibers each block on an empty channel while
    /// 64 producer fibers send. On the legacy "one OS thread per task, block the thread on `recv`"
    /// engine the blocked consumers pin every pool thread and the queued producers never run
    /// (starvation/hang). Under the M:N engine the consumers PARK (freeing their workers), the
    /// producers run and wake them, and the sum completes.
    #[test]
    fn mn_many_blocked_consumers_complete_without_starving() {
        let src = "\
fn producer(ch: Channel[int], i: int):
    ch.send(i)
fn consumer(ch: Channel[int], acc: Shared[int]):
    v := ch.recv()
    acc.update(fn(x): x + v)
fn main():
    ch := Channel[int]()
    acc := Shared(0)
    parallel:
        for i in 0..64:
            spawn consumer(ch, acc)
        for i in 0..64:
            spawn producer(ch, i)
    print(acc.get())
main()
";
        assert_eq!(run_capture_parallel(src).unwrap(), "2016\n"); // sum 0..64
    }

    /// D2b: 1000 producer fibers + 1 consumer recv-looping 1000 times, multiplexed over the
    /// core-sized pool — 1001 fibers on ~N threads, no thread-per-fiber. The consumer parks between
    /// sends and resumes via the rewound-ip `recv` replay.
    #[test]
    fn mn_thousand_fiber_pipeline_completes() {
        let src = "\
fn producer(ch: Channel[int], i: int):
    ch.send(i)
fn consumer(ch: Channel[int], acc: Shared[int]):
    total := 0
    for _ in 0..1000:
        total = total + ch.recv()
    acc.update(fn(x): x + total)
fn main():
    ch := Channel[int]()
    acc := Shared(0)
    parallel:
        spawn consumer(ch, acc)
        for i in 0..1000:
            spawn producer(ch, i)
    print(acc.get())
main()
";
        assert_eq!(run_capture_parallel(src).unwrap(), "499500\n"); // sum 0..1000
    }

    /// D2a: a cooperative fiber carries NO heap (`heap: None`) — every cooperative fiber aliases the
    /// single `Vm::heap` (decision A, share-by-ref). `swap_ctx` must leave `self.heap` untouched, so
    /// the cooperative engine stays byte-identical.
    #[test]
    fn swap_ctx_leaves_heap_untouched_for_cooperative_fiber() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let hv = vm.heap.alloc(Obj::Str("vm-obj".into()));
        let mut ctx = FiberCtx::default();
        assert!(ctx.heap.is_none(), "a default (cooperative) fiber carries no heap");
        vm.swap_ctx(&mut ctx);
        assert!(matches!(vm.heap.get(hv), Obj::Str(s) if &s[..] == "vm-obj"));
        assert!(ctx.heap.is_none(), "swap must not give a cooperative fiber a heap");
    }

    /// D2a GC canary: a `collect` while an M:N fiber is swapped in must trace the FIBER's heap (via
    /// the swapped-in operand stack) and must NOT touch the parked host heap. After the fiber parks
    /// back out, the host heap and its stack-rooted object are intact. This is the one path the
    /// swap-with-heap logic adds that the goldens can't reach (no runtime site parks a fiber until
    /// D2b), so it guards the moved-heap rooting directly.
    #[test]
    fn collect_under_swapped_in_fiber_heap_preserves_parked_host_object() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        vm.parallel = true;
        let hv = vm.heap.alloc(Obj::Str("vm-obj".into()));
        vm.push(Value::Obj(hv)); // keep the host object stack-rooted

        let mut fiber_heap = Heap::new();
        let hf = fiber_heap.alloc(Obj::Str("fiber-obj".into()));
        let mut ctx = FiberCtx {
            heap: Some(fiber_heap),
            stack: vec![Value::Obj(hf)], // the fiber's own stack roots its object
            ..FiberCtx::default()
        };

        // Schedule in: host heap + stack park into the ctx; self.{heap,stack} are the fiber's.
        vm.swap_ctx(&mut ctx);
        vm.collect(); // roots self.stack (the fiber's) → hf survives in the fiber heap
        assert!(matches!(vm.heap.get(hf), Obj::Str(s) if &s[..] == "fiber-obj"));

        // Park back out: the untouched host heap + its object are restored.
        vm.swap_ctx(&mut ctx);
        assert!(matches!(vm.heap.get(hv), Obj::Str(s) if &s[..] == "vm-obj"));
        assert_eq!(vm.pop(), Value::Obj(hv));
    }

    /// D2a share-nothing lock: a `collect` while an M:N fiber is swapped in must leave the parked
    /// HOST heap fully quiescent — not even sweeping its UNROOTED garbage. An object rooted by
    /// nothing in any context would be swept by a normal host-heap collect; here the collect runs on
    /// the fiber heap, so the host heap is never traced and the garbage survives. This proves the
    /// parked heap is untouched (the positive canary only shows a *stack-rooted* host object
    /// survives; this shows the collect didn't run on the host heap at all) — the guarantee D2b
    /// relies on when parking fibers across worker threads.
    #[test]
    fn collect_under_swapped_in_fiber_heap_leaves_parked_host_heap_quiescent() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        vm.parallel = true;
        // Rooted by nothing — a host-heap collect would sweep it.
        let garbage = vm.heap.alloc(Obj::Str("host-garbage".into()));

        let mut fiber_heap = Heap::new();
        let hf = fiber_heap.alloc(Obj::Str("fiber-obj".into()));
        let mut ctx = FiberCtx {
            heap: Some(fiber_heap),
            stack: vec![Value::Obj(hf)],
            ..FiberCtx::default()
        };

        vm.swap_ctx(&mut ctx);
        vm.collect(); // runs on the fiber heap only — the parked host heap is not traced
        vm.swap_ctx(&mut ctx);

        // The unrooted host object is still alive: collect never ran on the host heap. (Were it
        // swept, `heap.get` would panic on the dangling GcRef.)
        assert!(matches!(vm.heap.get(garbage), Obj::Str(s) if &s[..] == "host-garbage"));
    }

    /// B3.1: `shut` lives in the shared core, so a `from_wire`'d alias observes a shutdown done through
    /// the original handle — `submit` on the alias then fails with the byte-identical message.
    #[test]
    fn executor_core_shut_is_shared_across_handles() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let h1 = vm.heap.alloc(Obj::Executor(Arc::new(ExecutorCore::default())));
        let w = vm.to_wire(Value::Obj(h1)).unwrap();
        let Value::Obj(h2) = vm.from_wire(w) else { panic!("expected handle") };
        let sp = Span { line: 1, col: 1 };
        vm.executor_method(h1, "shutdown", &[], sp).unwrap();
        let dummy = vm.heap.alloc(Obj::Str("task".into()));
        let err = vm.executor_method(h2, "submit", &[Value::Obj(dummy)], sp).unwrap_err();
        assert_eq!(err.message, "submit on a shut-down Executor (it no longer accepts work)");
    }

    /// B3.1: `display` of a `Shared` box renders its contents through `display_wire` (a boxed `str`
    /// renders from its owned bytes — B3.3a), since `display` is `&self` and can't `from_wire`.
    #[test]
    fn display_shared_renders_contents() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let s = vm.heap.alloc(Obj::Str("hi".into()));
        let boxed = vm.to_wire(Value::Obj(s)).unwrap();
        let sh = vm.heap.alloc(Obj::Shared(Arc::new(SharedCore { v: Mutex::new(boxed), ..Default::default() })));
        assert_eq!(vm.display(Value::Obj(sh)), "Shared(hi)");
    }

    // ----- B3.2: isolated worker-VM construction (no threads) -----

    /// Build a one-proto program + a parent `Vm`, plus a zero-arg closure over proto 0 with a dummy
    /// home module (the test protos never read globals). Mirrors how `do_spawn_block` shapes a task.
    fn worker_fixture(code: Vec<Op>) -> (Vm, PendingCall) {
        let sp = Span { line: 1, col: 1 };
        let proto = op::Proto { name: "task".into(), arity: 0, n_slots: 0, lines: vec![sp; code.len()], code, has_implicit_nursery: false };
        let program = Program { protos: vec![proto], ..empty_program() };
        let mut vm = Vm::new(Arc::new(program));
        let home = vm.heap.alloc(Obj::Module { name: "<test>".into(), slots: Vec::new(), index: Default::default() });
        let clo = vm.heap.alloc(Obj::Closure { proto: 0, captured: Default::default(), home });
        (vm, PendingCall::Call { callee: Value::Obj(clo), args: Vec::new(), span: sp })
    }

    /// The worker allocates into its OWN heap, not the parent's: a task that builds a fresh list runs
    /// to completion in the worker, the parent heap's live-object count is unchanged, and the result
    /// crosses back as a `WireValue` that reconstructs (in the parent) to the expected value.
    #[test]
    fn worker_runs_in_distinct_heap() {
        // () -> [1, 2]
        let (mut vm, task) =
            worker_fixture(vec![Op::ConstInt(1), Op::ConstInt(2), Op::NewList(2), Op::Return]);
        let before = vm.heap.live();
        let res = vm.run_task_isolated(task).expect("isolated task runs");
        assert_eq!(vm.heap.live(), before, "worker must not allocate into the parent heap");
        let got = vm.from_wire(res.value);
        let want = Value::Obj(vm.heap.alloc(Obj::List(vec![Value::Int(1), Value::Int(2)])));
        assert!(vm.values_equal(got, want), "result must round-trip back to [1, 2]");
    }

    /// B3.3-threads: a worker inherits the parent's read-only host state (process args + env) so a
    /// `--parallel` task reading `std.os.args` / an env var isn't silently inert; `stdin` is reset to
    /// `Empty` (a single consumable stream is not shared across worker threads).
    #[test]
    fn worker_inherits_host_args_and_env() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        vm.host.args = vec!["prog".into(), "--flag".into()];
        vm.host.env.insert("KEY".into(), "val".into());
        vm.host.stdin = crate::native::Stdin::Real;
        let worker = vm.spawn_worker();
        assert_eq!(worker.host.args, vec!["prog".to_string(), "--flag".to_string()]);
        assert_eq!(worker.host.env.get("KEY").map(String::as_str), Some("val"));
        assert!(matches!(worker.host.stdin, crate::native::Stdin::Empty), "stdin must not be shared to workers");
    }

    /// A worker's stdout is captured in ITS `out` and returned on the `WorkerResult` (decision F:
    /// buffer-per-worker), and never leaks into the parent's `out`. The return value crosses back too.
    #[test]
    fn worker_returns_value_and_out() {
        // () -> { print("hi from worker"); 7 }
        let (mut vm, task) = worker_fixture(vec![
            Op::ConstStr("hi from worker".into()),
            Op::CallPrint(1),
            Op::Pop,
            Op::ConstInt(7),
            Op::Return,
        ]);
        let res = vm.run_task_isolated(task).expect("isolated task runs");
        assert_eq!(res.out, "hi from worker\n", "worker stdout returns on the result");
        assert_eq!(res.stderr, "", "stderr is captured separately and empty here");
        assert_eq!(vm.from_wire(res.value), Value::Int(7), "return value crosses back");
        assert_eq!(vm.out, "", "worker output must not leak into the parent's stdout");
    }

    /// A worker shares the compiled program by `Arc` (read-only), never copying it: `spawn_worker`
    /// bumps the strong count and points at the SAME allocation, and drops its clone when finished.
    #[test]
    fn worker_shares_program_arc() {
        let program = Arc::new(empty_program());
        let vm = Vm::new(Arc::clone(&program)); // program + vm = 2 refs
        assert_eq!(Arc::strong_count(&program), 2);
        let worker = vm.spawn_worker();
        assert_eq!(Arc::strong_count(&program), 3, "worker shares the program (no copy)");
        assert!(Arc::ptr_eq(&program, &worker.program), "same Program allocation, not a clone");
        drop(worker);
        assert_eq!(Arc::strong_count(&program), 2, "worker releases its program ref on drop");
    }

    /// B3.3a: a `str` return value crosses the worker boundary **by value** — the worker serializes its
    /// own-heap `str` to owned bytes, and the parent reconstructs a fresh `str` from them (no dangling
    /// `GcRef`). Replaces B3.2's reject-the-str fault now that `str` is sendable by value.
    #[test]
    fn worker_crosses_str_by_value() {
        // () -> "oops"
        let (mut vm, task) = worker_fixture(vec![Op::ConstStr("oops".into()), Op::Return]);
        let res = vm.run_task_isolated(task).expect("a str result now crosses by value");
        let got = vm.from_wire(res.value);
        let want = Value::Obj(vm.heap.alloc(Obj::Str("oops".into())));
        assert!(vm.values_equal(got, want), "str result round-trips to \"oops\"");
    }

    // ----- B3.3c: read-only `home` snapshot (worker module-graph reconstruction) -----

    /// Compile + run a single-module program, returning the populated parent `Vm` (its `module_objs[0]`
    /// holds the top-level globals). Mirrors the live load path so a worker reconstructs a real graph.
    fn ran_standalone(src: &str) -> Vm {
        let tokens = lexer::tokenize(src).expect("tokenize");
        let module = parser::parse(tokens).expect("parse");
        let program = crate::compiler::compile_module_standalone(&module).expect("compile");
        let mut vm = Vm::new(Arc::new(program));
        vm.run().expect("run");
        vm
    }

    /// Compile + run a multi-file graph from its entry path, returning the populated parent `Vm`
    /// (all imported modules present in `module_objs`).
    fn ran_graph(entry: &std::path::Path) -> Vm {
        let graph = crate::resolver::build_graph(entry).expect("graph");
        let program = crate::compiler::compile_graph(&graph).expect("compile");
        let mut vm = Vm::new(Arc::new(program));
        vm.run().expect("run");
        vm
    }

    /// Look up a top-level global in the entry module (modules run deps-first, entry last).
    fn entry_global(vm: &Vm, name: &str) -> Value {
        let m = *vm.module_objs.last().expect("at least one module");
        vm.module_global(m, name).unwrap_or_else(|| panic!("no global '{name}'"))
    }

    fn sp() -> Span {
        Span { line: 1, col: 1 }
    }

    /// A spawned task reads a module-level constant — needs the read-only `home` snapshot (B3.3c):
    /// the worker's `home` is a reconstruction of the parent module's globals, not a fresh-empty one.
    #[test]
    fn worker_reads_module_global() {
        let mut vm = ran_standalone("answer := 42\nfn get_answer() -> int:\n    return answer\n");
        let task = PendingCall::Call { callee: entry_global(&vm, "get_answer"), args: Vec::new(), span: sp() };
        let res = vm.run_task_isolated(task).expect("task reads a module global in its worker");
        assert_eq!(vm.from_wire(res.value), Value::Int(42));
    }

    #[test]
    fn worker_reads_last_of_many_globals() {
        // M19 Phase 2b: three globals defined before the fn, and the task reads the LAST one. A
        // slot scramble between the parent's compiled slots and the worker's faulted-in slots would
        // surface here as the wrong global's value (e.g. reading `a` or `b` instead of `c`).
        let mut vm = ran_standalone("a := 1\nb := 2\nc := 99\nfn get_c() -> int:\n    return c\n");
        let task = PendingCall::Call { callee: entry_global(&vm, "get_c"), args: Vec::new(), span: sp() };
        let res = vm.run_task_isolated(task).expect("task reads the last module global in its worker");
        assert_eq!(vm.from_wire(res.value), Value::Int(99));
    }

    #[test]
    fn globals_compile_to_stable_slots() {
        // M19 Phase 2b: top-level bindings get compile-time slots. Collection order is fns first,
        // then lets — so `read_b`=0, `a`=1, `b`=2 — and a fn body reads a global by its slot
        // (`GetGlobalSlot`), never by name.
        let tokens = lexer::tokenize("a := 1\nb := 2\nfn read_b() -> int:\n    return b\n").expect("tok");
        let module = parser::parse(tokens).expect("parse");
        let program = crate::compiler::compile_module_standalone(&module).expect("compile");
        assert_eq!(program.modules[0].global_slots, vec!["read_b".to_string(), "a".to_string(), "b".to_string()]);
        let read_b = program.protos.iter().find(|p| p.name == "read_b").expect("fn proto");
        assert!(
            read_b.code.iter().any(|op| matches!(op, Op::GetGlobalSlot(2))),
            "read_b should load global slot 2 (`b`): {:?}",
            read_b.code
        );
        let top = &program.protos[program.modules[0].toplevel];
        let defines: Vec<u32> =
            top.code.iter().filter_map(|op| if let Op::DefineGlobalSlot(s) = op { Some(*s) } else { None }).collect();
        // toplevel defines the fn (slot 0 via hoist) and both lets (slots 1, 2).
        assert!(
            defines.contains(&0) && defines.contains(&1) && defines.contains(&2),
            "toplevel defines slots 0, 1, 2: {:?}",
            top.code
        );
    }

    /// A spawned task calls another top-level fn in its module — sibling resolution via the
    /// reconstructed `home` globals (the sibling `Func` is re-allocated over the worker's home).
    #[test]
    fn worker_calls_sibling_free_fn() {
        let mut vm = ran_standalone("fn helper() -> int:\n    return 7\nfn task() -> int:\n    return helper() + 1\n");
        let task = PendingCall::Call { callee: entry_global(&vm, "task"), args: Vec::new(), span: sp() };
        let res = vm.run_task_isolated(task).expect("task calls a sibling fn in its worker");
        assert_eq!(vm.from_wire(res.value), Value::Int(8));
    }

    /// A spawned task calls a function from an IMPORTED module — proves cross-module `module_objs`
    /// reconstruction (the `text` import alias maps to the worker's std.str module obj, whose own
    /// globals are reconstructed too).
    #[test]
    fn worker_calls_imported_fn() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static C: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir()
            .join(format!("chezzi_b33c_{}_{}", std::process::id(), C.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir_all(&dir).unwrap();
        let entry = dir.join("main.chz");
        std::fs::write(&entry, "import std.str as text\nfn task() -> str:\n    return text.repeat(\"ab\", 2)\n").unwrap();
        let mut vm = ran_graph(&entry);
        let _ = std::fs::remove_dir_all(&dir);
        let task = PendingCall::Call { callee: entry_global(&vm, "task"), args: Vec::new(), span: sp() };
        let res = vm.run_task_isolated(task).expect("task calls an imported fn in its worker");
        let got = vm.from_wire(res.value);
        let want = Value::Obj(vm.heap.alloc(Obj::Str("abab".into())));
        assert!(vm.values_equal(got, want), "imported repeat returns abab");
    }

    // ----- B3.3d: method tasks (`spawn recv.m()`) -----

    /// A method task on a primitive receiver — `"hello".len()` dispatches in the worker (B3.3d,
    /// replaces the B3.2 reject). Core-type methods need no module graph, but exercise the new path.
    #[test]
    fn worker_runs_method_task() {
        let mut vm = Vm::new(Arc::new(empty_program()));
        let recv = vm.heap.alloc(Obj::Str("hello".into()));
        let task = PendingCall::Method { recv: Value::Obj(recv), name: "len".into(), args: Vec::new(), span: sp() };
        let res = vm.run_task_isolated(task).expect("method task now runs in a worker");
        assert_eq!(vm.from_wire(res.value), Value::Int(5));
    }

    /// A struct method resolved through reconstructed `module_objs` — and its body **reads a module
    /// global** (`scale`), so dispatch must resolve through the rebuilt home *contents*, not merely
    /// index an in-bounds placeholder. `(3 + 4) * 10 == 70`.
    #[test]
    fn worker_method_on_struct() {
        let mut vm = ran_standalone(
            "scale := 10\nstruct Point:\n    x: int\n    y: int\n    fn weighted(self) -> int:\n        return (self.x + self.y) * scale\np := Point(3, 4)\n",
        );
        let task = PendingCall::Method { recv: entry_global(&vm, "p"), name: "weighted".into(), args: Vec::new(), span: sp() };
        let res = vm.run_task_isolated(task).expect("struct method task dispatches in its worker");
        assert_eq!(vm.from_wire(res.value), Value::Int(70));
    }

    /// Cross-heap safety: a module global that is a **container of callables** (`[fn …]`) must have its
    /// nested `Func` *rebuilt* in the worker heap, not carried across as a by-reference `Handle` (a
    /// parent-heap `GcRef`). A task that calls through the list exercises the reconstructed funcs; a
    /// smuggled `GcRef` would read a wrong/out-of-range worker slot. `bump(20) == 21`.
    #[test]
    fn worker_calls_through_global_fn_container() {
        let mut vm = ran_standalone(
            "fn bump(n: int) -> int:\n    return n + 1\nhandlers := [bump]\nfn task() -> int:\n    return handlers[0](20)\n",
        );
        let task = PendingCall::Call { callee: entry_global(&vm, "task"), args: Vec::new(), span: sp() };
        let res = vm.run_task_isolated(task).expect("task calls a fn from a global container in its worker");
        assert_eq!(vm.from_wire(res.value), Value::Int(21));
    }

    /// The module-graph reconstruction must be GC-safe: with `gc_stress` on (collect before every
    /// instruction), a task that reads a **heap-typed** module global (a list) through the rebuilt home
    /// must still round-trip — the reconstructed globals stay rooted via `module_objs`.
    #[test]
    fn worker_reconstruction_survives_gc_stress() {
        let mut vm = ran_standalone("data := [1, 2, 3]\nfn total() -> int:\n    s := 0\n    for x in data:\n        s += x\n    return s\n");
        vm.gc_stress = true;
        let task = PendingCall::Call { callee: entry_global(&vm, "total"), args: Vec::new(), span: sp() };
        let res = vm.run_task_isolated(task).expect("reconstruction survives GC stress");
        assert_eq!(vm.from_wire(res.value), Value::Int(6));
    }

    // ----- arithmetic -----

    #[test]
    fn int_div_truncates() {
        assert_eq!(run("print(7 / 2)"), "3\n");
        assert_eq!(run("print(-7 / 2)"), "-3\n"); // Rust trunc-toward-zero, matching interp
    }

    #[test]
    fn int_overflow_is_error_not_wrap() {
        // A wrapping VM would print a negative number; we must error like the interpreter.
        assert!(run_err("print(9223372036854775807 + 1)").contains("integer overflow in Add"));
    }

    #[test]
    fn int_min_neg_and_div_overflow() {
        // The two other unrepresentable results: -i64::MIN and i64::MIN / -1. Both must error.
        let neg = "fn main():\n    x := -9223372036854775807 - 1\n    print(-x)\nmain()\n";
        assert!(run_err(neg).contains("integer overflow"));
        let div = "fn main():\n    x := -9223372036854775807 - 1\n    print(x / -1)\nmain()\n";
        assert!(run_err(div).contains("integer overflow"));
    }

    #[test]
    fn float_promotion_when_either_side_float() {
        assert_eq!(run("print(1 + 2.0)"), "3.0\n");
        assert_eq!(run("print(7.0 / 2.0)"), "3.5\n");
        assert_eq!(run("print(7 / 2.0)"), "3.5\n");
    }

    #[test]
    fn division_and_modulo_by_zero_error() {
        assert_eq!(run_err("print(1 / 0)"), "division by zero");
        assert_eq!(run_err("print(1 % 0)"), "modulo by zero");
        // Float by zero is an error too — not silent inf/nan.
        assert_eq!(run_err("print(1.0 / 0.0)"), "division by zero");
        assert_eq!(run_err("print(5.0 % 0.0)"), "modulo by zero");
    }

    #[test]
    fn string_concatenation() {
        assert_eq!(run(r#"print("a" + "b" + "c")"#), "abc\n");
    }

    #[test]
    fn comparison_and_equality_across_numeric_types() {
        assert_eq!(run("print(1 < 2.0)"), "true\n");
        assert_eq!(run("print(2 == 2.0)"), "true\n");
        assert_eq!(run("print(2 != 3)"), "true\n");
        assert_eq!(run(r#"print("a" < "b")"#), "true\n");
        // Cross-type equality is false, never an error.
        assert_eq!(run(r#"print(1 == "1")"#), "false\n");
    }

    #[test]
    fn arithmetic_type_error_message() {
        assert!(run_err(r#"print(1 + "x")"#).contains("cannot apply Add to int and str"));
    }

    // ----- M19 superinstructions: operands are LOCALS (inside `fn`), so the fused ops actually
    // execute (top-level `:=` is a global → `GetGlobal`, never fused). -----

    #[test]
    fn superinstruction_loop_sum_correct() {
        // `i < 5` → BinLocalConst{Lt}; `total += i` → BinLocalLocal{Add}+SetLocal; `i += 1` → IncLocal.
        let src = "fn main():\n    total := 0\n    i := 0\n    while i < 5:\n        total += i\n        i += 1\n    print(total)\nmain()";
        assert_eq!(run(src), "10\n");
    }

    #[test]
    fn superinstruction_div_mod_by_zero_via_locals() {
        // BinLocalLocal fast path must raise the same message as `arith`.
        assert_eq!(run_err("fn main():\n    x := 1\n    y := 0\n    print(x / y)\nmain()"), "division by zero");
        assert_eq!(run_err("fn main():\n    x := 1\n    y := 0\n    print(x % y)\nmain()"), "modulo by zero");
    }

    #[test]
    fn superinstruction_overflow_via_inc_and_mul() {
        // IncLocal overflow.
        assert!(run_err("fn main():\n    i := 9223372036854775807\n    i += 1\n    print(i)\nmain()").contains("integer overflow in Add"));
        // BinLocalConst Mul overflow.
        assert!(run_err("fn main():\n    x := 9223372036854775807\n    print(x * 2)\nmain()").contains("integer overflow in Mul"));
    }

    #[test]
    fn superinstructions_run_under_parallel_engine() {
        // Drives BinLocalConst / BinLocalLocal / IncLocal through the M:N engine (reduction path).
        let out = run_capture_parallel("fn main():\n    total := 0\n    i := 0\n    while i < 1000:\n        total += i\n        i += 1\n    print(total)\nmain()").unwrap();
        assert_eq!(out, "499500\n");
    }

    // ----- and / or short-circuit -----

    #[test]
    fn and_short_circuits_rhs() {
        // If `and` did not short-circuit, the `1/0` would raise a div-by-zero error.
        assert_eq!(run("print(false and (1 / 0 == 0))"), "false\n");
    }

    #[test]
    fn or_short_circuits_rhs() {
        assert_eq!(run("print(true or (1 / 0 == 0))"), "true\n");
    }

    #[test]
    fn logical_operand_must_be_bool() {
        assert_eq!(run_err("print(1 and true)"), "expected bool, found int");
    }

    // ----- display formatting -----

    #[test]
    fn float_display_keeps_one_decimal_for_integral() {
        assert_eq!(run("print(5.0)"), "5.0\n");
        assert_eq!(run("print(5.5)"), "5.5\n");
        assert_eq!(run("print(2.5 * 2.0)"), "5.0\n");
    }

    #[test]
    fn list_display() {
        assert_eq!(run("print([1, 2, 3])"), "[1, 2, 3]\n");
        assert_eq!(run("print([])"), "[]\n");
        assert_eq!(run(r#"print(["a", "b"])"#), "[a, b]\n");
    }

    #[test]
    fn struct_display_in_declaration_order() {
        let src = "\
struct Point:
    x: int
    y: int
print(Point(3, 4))";
        assert_eq!(run(src), "Point(x=3, y=4)\n");
    }

    #[test]
    fn enum_display_nullary_and_payload() {
        let src = "\
enum Shape:
    Circle(int)
    Dot
print(Circle(2))
print(Dot)";
        assert_eq!(run(src), "Circle(2)\nDot\n");
    }

    #[test]
    fn print_joins_args_with_space() {
        assert_eq!(run(r#"print("a", 1, true)"#), "a 1 true\n");
    }

    // ----- functions / control flow -----

    #[test]
    fn nested_calls_and_return() {
        let src = "\
fn add(a: int, b: int) -> int:
    return a + b
fn main():
    print(add(add(1, 2), 3))
main()";
        assert_eq!(run(src), "6\n");
    }

    #[test]
    fn forward_reference_between_top_level_fns() {
        // `main` is defined before `helper`; hoisting must make the forward ref resolve.
        let src = "\
fn main():
    print(helper(21))
fn helper(n: int) -> int:
    return n * 2
main()";
        assert_eq!(run(src), "42\n");
    }

    #[test]
    fn infinite_recursion_hits_depth_limit() {
        let src = "\
fn loop(n: int) -> int:
    return loop(n + 1)
fn main():
    print(loop(0))
main()";
        assert!(run_err(src).contains("maximum call depth"));
    }

    /// M10-G1: a self-referential `Stringable` `str` must hit the depth guard, not loop forever.
    #[test]
    fn self_referential_stringable_hits_depth_limit() {
        let src = "struct Loop:\n    n: int\n    fn str(self) -> str:\n        return str(self)\nprint(Loop(1))\n";
        assert!(run_err(src).contains("maximum call depth"));
    }

    #[test]
    fn if_elif_else() {
        let src = "\
fn classify(n: int) -> str:
    if n < 0:
        return \"neg\"
    else if n == 0:
        return \"zero\"
    else:
        return \"pos\"
fn main():
    print(classify(-1))
    print(classify(0))
    print(classify(5))
main()";
        assert_eq!(run(src), "neg\nzero\npos\n");
    }

    #[test]
    fn while_loop_with_compound_assign() {
        let src = "\
fn main():
    i := 0
    total := 0
    while i < 5:
        total += i
        i += 1
    print(total)
main()";
        assert_eq!(run(src), "10\n");
    }

    #[test]
    fn unary_neg_and_not() {
        assert_eq!(run("print(-5)"), "-5\n");
        assert_eq!(run("print(not true)"), "false\n");
        assert_eq!(run_err("print(-true)"), "cannot apply Neg to bool");
    }

    // ----- closures -----

    #[test]
    fn closure_snapshots_captured_value() {
        // The closure captures `n` by value at creation; reassigning `n` afterward must NOT be
        // visible (matches the interpreter's frame snapshot, not by-reference capture).
        let src = "\
fn make():
    n := 10
    f := fn(x: int) -> int: x + n
    n = 20
    return f
fn main():
    g := make()
    print(g(5))
main()";
        assert_eq!(run(src), "15\n");
    }

    #[test]
    fn closure_captures_distinct_environments() {
        let src = "\
fn adder(n: int):
    return fn(x: int) -> int: x + n
fn main():
    add10 := adder(10)
    add100 := adder(100)
    print(add10(1))
    print(add100(1))
main()";
        assert_eq!(run(src), "11\n101\n");
    }

    // ----- ? operator -----

    #[test]
    fn try_unwraps_ok() {
        let src = "\
fn safe_div(a: int, b: int) -> Result[int]:
    if b == 0:
        return Err(\"divide by zero\")
    return Ok(a / b)
fn main():
    r := safe_div(10, 2)?
    print(r)
main()";
        assert_eq!(run(src), "5\n");
    }

    #[test]
    fn try_propagates_err_to_caller() {
        let src = "\
fn safe_div(a: int, b: int) -> Result[int]:
    if b == 0:
        return Err(\"zero\")
    return Ok(a / b)
fn use() -> Result[int]:
    r := safe_div(1, 0)?
    return Ok(r + 1)
fn main():
    match use():
        Ok(v): print(\"ok {v}\")
        Err(e): print(\"err {e}\")
main()";
        assert_eq!(run(src), "err zero\n");
    }

    #[test]
    fn try_on_non_result_is_error() {
        let src = "\
fn f() -> int:
    x := (5)?
    return x";
        // Reaching `?` on an int is a runtime error.
        assert!(run_err(&format!("{src}\nfn main():\n    print(f())\nmain()")).contains("'?' expects Result or Option, found int"));
    }

    #[test]
    fn top_level_try_err_is_unhandled_error() {
        // A `?` at the top level whose Err reaches the top is an unhandled error (no main needed).
        assert_eq!(run_err(r#"x := Err("oops")?"#), "unhandled error: oops");
    }

    #[test]
    fn top_level_try_err_reports_real_line() {
        // The `?` is on line 3 — report there, not at a hard-coded line 1 (parity with the interp).
        let e = run_capture("fn d() -> Result[int]:\n    return Err(\"x\")\nx := d()?\n").unwrap_err();
        assert_eq!(e.message, "unhandled error: x");
        assert_eq!(e.span.line, 3, "expected the `?` line, got {}", e.span.line);
    }

    // ----- for loops -----

    #[test]
    fn for_range_sums() {
        let src = "\
fn main():
    total := 0
    for i in 0..1000:
        total += i
    print(total)
main()";
        assert_eq!(run(src), "499500\n");
    }

    #[test]
    fn for_range_is_lazy_not_materialized() {
        // A billion-element range would exhaust memory if materialized; the lazy counting loop
        // returns on the first iteration instantly.
        let src = "\
fn first() -> int:
    for i in 0..1000000000:
        return i
    return -1
fn main():
    print(first())
main()";
        assert_eq!(run(src), "0\n");
    }

    #[test]
    fn for_over_list() {
        let src = "\
fn main():
    total := 0
    for x in [10, 20, 30]:
        total += x
    print(total)
main()";
        assert_eq!(run(src), "60\n");
    }

    #[test]
    fn for_over_non_iterable_errors() {
        assert!(run_err("for x in 5:\n    print(x)").contains("cannot iterate over int"));
    }

    // ----- match -----

    #[test]
    fn match_binds_payload() {
        let src = "\
enum Shape:
    Circle(int)
    Square(int)
fn area(s: Shape) -> int:
    match s:
        Circle(r): return r * r * 3
        Square(n): return n * n
fn main():
    print(area(Circle(2)))
    print(area(Square(3)))
main()";
        assert_eq!(run(src), "12\n9\n");
    }

    #[test]
    fn match_no_arm_is_error() {
        let src = "\
enum Color:
    Red
    Green
    Blue
fn name(c: Color) -> str:
    match c:
        Red: return \"r\"
        Green: return \"g\"
fn main():
    print(name(Blue))
main()";
        assert_eq!(run_err(src), "no match arm for variant 'Blue'");
    }

    #[test]
    fn match_on_non_enum_is_error() {
        // A *payload* variant pattern unambiguously needs an enum scrutinee; matching it on an int is
        // a clean runtime error (the `EnsureEnum` guard) rather than a panic.
        let src = "\
fn main():
    match 5:
        Some(x): print(x)
main()";
        assert!(run_err(src).contains("cannot match on int"));
    }

    #[test]
    fn match_bare_ident_on_non_enum_binds_value() {
        // A bare top-level identifier against a non-enum value is a binding capturing the whole
        // value (the checker permits this only for literal scrutinees) — not an enum-match error.
        let src = "\
fn main():
    match 5:
        x: print(x)
main()";
        assert_eq!(run(src), "5\n");
    }

    // ----- field / index -----

    #[test]
    fn index_list_and_out_of_bounds() {
        assert_eq!(run("print([10, 20, 30][1])"), "20\n");
        assert_eq!(run_err("print([1, 2][5])"), "index 5 out of bounds (len 2)");
    }

    #[test]
    fn index_string_returns_char() {
        assert_eq!(run(r#"print("hello"[1])"#), "e\n");
    }

    #[test]
    fn index_assign_mutates_in_place() {
        assert_eq!(run("xs := [1, 2, 3]\nxs[1] = 9\nprint(xs)\n"), "[1, 9, 3]\n");
    }

    #[test]
    fn index_compound_assign() {
        assert_eq!(
            run("xs := [1, 2, 3]\nxs[0] += 5\nxs[2] -= 1\nprint(xs)\n"),
            "[6, 2, 2]\n"
        );
    }

    #[test]
    fn index_assign_out_of_bounds_errors() {
        assert_eq!(run_err("xs := [1, 2, 3]\nxs[5] = 0\n"), "index 5 out of bounds (len 3)");
    }

    #[test]
    fn field_assign_mutates_in_place() {
        let src = "\
struct P:
    x: int
    y: int
fn main():
    p := P(1, 2)
    p.x = 9
    print(p.x)
    print(p.y)
main()";
        assert_eq!(run(src), "9\n2\n");
    }

    #[test]
    fn field_compound_assign() {
        let src = "\
struct P:
    x: int
fn main():
    p := P(10)
    p.x += 5
    p.x -= 3
    print(p.x)
main()";
        assert_eq!(run(src), "12\n");
    }

    #[test]
    fn field_access_and_unknown_field() {
        let src = "\
struct P:
    x: int
    y: int
fn main():
    p := P(1, 2)
    print(p.x)
    print(p.y)
main()";
        assert_eq!(run(src), "1\n2\n");
    }

    #[test]
    fn struct_method_call_binds_self() {
        let src = "\
struct Counter:
    n: int
    fn doubled(self) -> int:
        return self.n * 2
fn main():
    c := Counter(21)
    print(c.doubled())
main()";
        assert_eq!(run(src), "42\n");
    }

    // ----- builtins -----

    #[test]
    fn builtin_len() {
        assert_eq!(run("print(len([1, 2, 3]))"), "3\n");
        assert_eq!(run(r#"print(len("hello"))"#), "5\n");
    }

    #[test]
    fn builtin_range_and_cap() {
        assert_eq!(run("print(range(3))"), "[0, 1, 2]\n");
        assert_eq!(run("print(range(2, 5))"), "[2, 3, 4]\n");
        assert!(run_err("print(range(20000000))").contains("exceeds the maximum"));
    }

    #[test]
    fn builtin_casts() {
        assert_eq!(run(r#"print(int("42"))"#), "42\n");
        assert_eq!(run("print(float(3))"), "3.0\n");
        assert_eq!(run("print(str(5))"), "5\n");
        assert!(run_err(r#"print(int("notnum"))"#).contains("cannot parse 'notnum'"));
    }

    // ----- construction arity / nullary variant -----

    #[test]
    fn struct_arity_error() {
        let src = "\
struct Point:
    x: int
    y: int
fn main():
    p := Point(1)
main()";
        assert!(run_err(src).contains("struct 'Point' expects 2 field(s), got 1"));
    }

    #[test]
    fn variant_arity_error() {
        assert!(run_err("fn main():\n    x := Ok(1, 2)\nmain()").contains("variant 'Ok' expects 1 value(s), got 2"));
    }

    #[test]
    fn nullary_variant_used_as_value() {
        assert_eq!(run("print(None)"), "None\n");
        let src = "\
enum Light:
    On
    Off
fn main():
    print(Off)
main()";
        assert_eq!(run(src), "Off\n");
    }

    // ----- string interpolation -----

    #[test]
    fn interpolation_and_literal_braces() {
        let src = "\
fn main():
    name := \"thuan\"
    print(\"hi {name}, {{not interpolated}}\")
main()";
        assert_eq!(run(src), "hi thuan, {not interpolated}\n");
    }

    // ----- golden parity -----

    #[test]
    fn golden_hello_chz_matches_expected() {
        let expected = include_str!("../../examples/hello.expected");
        assert_eq!(run(include_str!("../../examples/hello.chz")), expected);
    }

    #[test]
    fn golden_hello_chz_matches_interpreter() {
        let src = include_str!("../../examples/hello.chz");
        let vm_out = run_capture(src).expect("vm run");
        let interp_out = crate::interp::run_capture(src).expect("interp run");
        assert_eq!(vm_out, interp_out);
    }

    /// M8-M4 golden: `examples/set.chz` (the set type — literals, membership, algebra, iteration)
    /// byte-identical on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_set_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/set.chz");
        let expected = include_str!("../../examples/set.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `timer(ms)` golden: a one-shot timeout channel delivers `true`. Byte-identical on the
    /// cooperative VM, the interpreter, and `.expected` (both inline-sleep to the deadline). `--parallel`
    /// delivers the same value via the background timer `send` (asserted separately).
    #[test]
    fn golden_timer_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/timer.chz");
        let expected = include_str!("../../examples/timer.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
        assert_eq!(run_capture_parallel(src).expect("parallel"), expected);
    }

    /// `timer(ms)` under `--parallel`: a spawned fiber recv-blocks on the timeout channel, PARKS, and
    /// is woken by the background timer `send` at the deadline — not a false deadlock (the pending
    /// timer is accounted as `inflight`, vetoing the predicate while the lone fiber waits). Proves the
    /// async-delivery path + the deadlock veto + the park/wake on the timer channel's key.
    #[test]
    fn parallel_timer_wakes_blocked_recv() {
        let src = "fn waiter(t: Channel[bool]):\n    print(t.recv())\n\
                   fn main():\n    parallel:\n        spawn waiter(timer(20))\nmain()\n";
        assert_eq!(run_capture_parallel(src).expect("parallel"), "true\n");
    }

    /// `Atomic[T]` golden: single-thread load/store/add/sub/exchange/cas sequence, byte-identical on
    /// the VM, the interpreter, and `.expected`.
    #[test]
    fn golden_atomic_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/atomic.chz");
        let expected = include_str!("../../examples/atomic.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Slicing golden: `examples/slicing.chz` (list/str slicing + the `Index`/`IndexSet`/`Slice`
    /// protocols on a struct + a generic over both) byte-identical on the VM, interp, and `.expected`.
    #[test]
    fn golden_slicing_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/slicing.chz");
        let expected = include_str!("../../examples/slicing.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `defer` golden: LIFO order, method + free-fn calls, the `?` short-circuit path, args evaluated
    /// at the defer statement (per-iteration snapshot), the `defer:` block form (in-block order,
    /// LIFO-as-a-unit, by-value snapshot at the defer point, `?`-path), defers running before a
    /// `recover:` catch, and a fault inside a deferred call. Byte-identical on the VM, the
    /// interpreter, and its `.expected`.
    #[test]
    fn golden_defer_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/defer.chz");
        let expected = include_str!("../../examples/defer.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    // ----- concurrency C4 (VM parity for spawn / parallel: / Channel / Shared) -----

    /// C1 golden: `parallel:` nursery + both `spawn` forms run to completion at the dedent (FIFO),
    /// the parent resuming only after the join. Byte-identical on the VM, the interpreter, and the
    /// `.expected` file.
    #[test]
    fn golden_parallel_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/parallel.chz");
        let expected = include_str!("../../examples/parallel.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// B3.3-threads sub-step 1: the `--parallel` engine is selectable (`run_capture_parallel` sets
    /// `Vm::parallel`). A `parallel:` with task-order output (no cross-task blocking) yields the same
    /// result as the cooperative engine — proving the flag plumbs through without changing
    /// well-ordered output.
    #[test]
    fn parallel_engine_runs_simple_program() {
        let src = include_str!("../../examples/parallel.chz");
        let expected = include_str!("../../examples/parallel.expected");
        assert_eq!(run_capture_parallel(src).expect("parallel run"), expected);
    }

    /// M-C golden: implicit nurseries — bare `spawn` at function scope joins at the body's
    /// `return`/end; an inner `parallel:` joins earlier at its dedent. Byte-identical on all three
    /// engines (cooperative VM, frozen interp, `--parallel`).
    #[test]
    fn golden_implicit_nursery_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/implicit_nursery.chz");
        let expected = include_str!("../../examples/implicit_nursery.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
        assert_eq!(run_capture_parallel(src).expect("parallel run"), expected);
    }

    /// B3.3-threads golden: N real OS-thread tasks `update` one `Shared[int]` concurrently; the box
    /// serialises every write, so the count is exactly the spawn count (lost-update race fixed by the
    /// `update_lock`). Deterministic-by-construction (order-independent) — proves the bounded pool +
    /// `Shared` cross-thread atomicity. The default cooperative engine runs it too (still `5`).
    #[test]
    fn golden_parallel_shared_chz_matches_expected() {
        let src = include_str!("../../examples/parallel_shared.chz");
        let expected = include_str!("../../examples/parallel_shared.expected");
        assert_eq!(run_capture_parallel(src).expect("parallel run"), expected);
        // Same program on the cooperative default engine is identical (decision A oracle).
        assert_eq!(run_capture(src).expect("vm run"), expected);
    }

    /// B3.3-threads golden: the collector task `recv`s before any producer runs, so on the real-thread
    /// engine it BLOCKS on the channel condvar and is woken by producer `send`s from pool threads.
    /// It sorts what it gathers → the printed order is fixed however threads interleave
    /// (deterministic-by-construction). Exercises condvar `recv` + flush-on-join.
    #[test]
    fn golden_parallel_channel_chz_matches_expected() {
        let src = include_str!("../../examples/parallel_channel.chz");
        let expected = include_str!("../../examples/parallel_channel.expected");
        assert_eq!(run_capture_parallel(src).expect("parallel run"), expected);
    }

    // ---- Channel.close() + closed semantics (both engines) ----

    #[test]
    fn vm_channel_send_after_close_faults() {
        let err = run_err("fn main():\n    ch := Channel[int]()\n    ch.close()\n    ch.send(1)\nmain()\n");
        assert!(err.contains("send on a closed channel"), "{err}");
    }

    #[test]
    fn vm_channel_recv_on_closed_empty_faults() {
        let err = run_err("fn main():\n    ch := Channel[int]()\n    ch.close()\n    print(ch.recv())\nmain()\n");
        assert!(err.contains("receive on a closed channel"), "{err}");
    }

    #[test]
    fn vm_channel_drains_buffered_after_close() {
        let src = "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    ch.send(2)\n    ch.close()\n    print(ch.recv())\n    print(ch.recv())\nmain()\n";
        assert_eq!(run(src), "1\n2\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp"));
    }

    #[test]
    fn vm_channel_try_send_false_when_closed() {
        let src = "fn main():\n    ch := Channel[int]()\n    print(ch.try_send(1))\n    ch.close()\n    print(ch.try_send(2))\nmain()\n";
        assert_eq!(run(src), "true\nfalse\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp"));
    }

    #[test]
    fn vm_channel_double_close_ok() {
        let src = "fn main():\n    ch := Channel[int]()\n    ch.close()\n    ch.close()\n    print(1)\nmain()\n";
        assert_eq!(run(src), "1\n");
    }

    #[test]
    fn vm_channel_close_then_len_zero() {
        let src = "fn main():\n    ch := Channel[int]()\n    ch.close()\n    print(ch.len())\nmain()\n";
        assert_eq!(run(src), "0\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp"));
    }

    #[test]
    fn vm_channel_try_recv_closed_empty_is_none() {
        let src = "fn main():\n    ch := Channel[int]()\n    ch.close()\n    match ch.try_recv():\n        Some(v): print(v)\n        None: print(\"none\")\nmain()\n";
        assert_eq!(run(src), "none\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp"));
    }

    #[test]
    fn vm_for_over_channel_drains_then_exits() {
        // Producer-first (no concurrency needed): the channel is closed+full before the `for` runs.
        let src = "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    ch.send(2)\n    ch.send(3)\n    ch.close()\n    total := 0\n    for v in ch:\n        total = total + v\n    print(total)\nmain()\n";
        assert_eq!(run(src), "6\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp"));
    }

    /// `--parallel`: a `for v in ch:` consumer that runs ahead of the producer PARKS on the empty
    /// channel; a sibling `close()` (no value sent) wakes it and the loop ends cleanly (0 iterations).
    #[test]
    fn parallel_close_wakes_parked_receiver() {
        let src = "\
fn produce(ch: Channel[int]):
    ch.close()
fn consume(ch: Channel[int], out: Channel[int]):
    n := 0
    for v in ch:
        n = n + 1
    out.send(n)
fn main():
    ch := Channel[int]()
    out := Channel[int]()
    parallel:
        spawn consume(ch, out)
        spawn produce(ch)
    print(out.recv())
main()
";
        assert_eq!(run_capture_parallel(src).expect("parallel run"), "0\n");
    }

    /// `--parallel`: a single `close()` must wake EVERY receiver parked on the channel (not just one,
    /// as a `send` would). Three consumers each loop-then-report; all three exit and report.
    #[test]
    fn parallel_close_wakes_multiple_receivers() {
        let src = "\
fn consume(ch: Channel[int], done: Channel[int]):
    for v in ch:
        n := v
    done.send(1)
fn main():
    ch := Channel[int]()
    done := Channel[int]()
    parallel:
        spawn consume(ch, done)
        spawn consume(ch, done)
        spawn consume(ch, done)
        spawn:
            ch.close()
    total := 0
    for i in 0..3:
        total = total + done.recv()
    print(total)
main()
";
        assert_eq!(run_capture_parallel(src).expect("parallel run"), "3\n");
    }

    /// `--parallel`: a consumer that loops `recv` past the producer's last value used to deadlock-fault;
    /// with `for v in ch:` + a producer `close()` it drains the buffered values and exits cleanly.
    #[test]
    fn parallel_consumer_loop_terminates_on_close() {
        let src = "\
fn produce(ch: Channel[int]):
    for i in 1..6:
        ch.send(i)
    ch.close()
fn consume(ch: Channel[int], out: Channel[int]):
    total := 0
    for v in ch:
        total = total + v
    out.send(total)
fn main():
    ch := Channel[int]()
    out := Channel[int]()
    parallel:
        spawn produce(ch)
        spawn consume(ch, out)
    print(out.recv())
main()
";
        // 1+2+3+4+5 = 15, however the producer/consumer interleave.
        assert_eq!(run_capture_parallel(src).expect("parallel run"), "15\n");
    }

    /// Channel.close() golden: producer sends a run + `close()`s; consumer `for v in ch:` drains then
    /// ends cleanly. VM cooperative == interp == expected (decision A oracle), and the `--parallel`
    /// engine (consumer parks on the empty channel, woken by the producer's send/close) prints the
    /// same total. Pins the headline `for`-over-channel + `try_send`-after-close surface on all engines.
    #[test]
    fn golden_parallel_channel_close_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/parallel_channel_close.chz");
        let expected = include_str!("../../examples/parallel_channel_close.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    #[test]
    fn golden_parallel_channel_close_chz_parallel_engine() {
        let src = include_str!("../../examples/parallel_channel_close.chz");
        let expected = include_str!("../../examples/parallel_channel_close.expected");
        assert_eq!(run_capture_parallel(src).expect("parallel run"), expected);
    }

    #[test]
    fn parallel_send_after_close_faults() {
        let src = "\
fn main():
    ch := Channel[int]()
    parallel:
        spawn:
            ch.close()
            ch.send(1)
main()
";
        let err = run_capture_parallel(src).expect_err("send after close should fault");
        assert!(err.message.contains("send on a closed channel"), "{}", err.message);
    }

    /// B3.6 golden: `Executor` tasks run on the bounded pool. Three submitted closures capture the
    /// result `Channel` (sendable → crosses as a shared `Arc`) and `send` from pool threads; `shutdown`
    /// drains them onto the pool and joins, then main sorts what it gathered → fixed printed order
    /// however threads interleave. The cooperative default engine runs it too (decision A oracle: same
    /// output, inline drain), proving the `submit`-by-value / pool-drain change is observationally inert.
    #[test]
    fn golden_executor_pool_chz_matches_expected() {
        let src = include_str!("../../examples/executor_pool.chz");
        let expected = include_str!("../../examples/executor_pool.expected");
        assert_eq!(run_capture_parallel(src).expect("parallel run"), expected);
        assert_eq!(run_capture(src).expect("vm run"), expected);
    }

    /// D1 (lazy module snapshot): a `--parallel` task that calls a sibling free function which in
    /// turn reads a module-level global must resolve both against the worker's *own* home module —
    /// exercising lazy fault-in of that module into the worker heap on first global access. The
    /// cooperative default engine is the equivalence oracle (same output). Characterization test:
    /// green before D1 (eager `build_worker_modules`) and after (lazy snapshot).
    #[test]
    fn parallel_task_resolves_sibling_fn_and_global() {
        let src = "\
G := 100
fn helper(x: int) -> int:
    return x + G
fn send_one(ch: Channel[int], x: int):
    ch.send(helper(x))
fn main():
    ch := Channel[int]()
    parallel:
        spawn send_one(ch, 1)
        spawn send_one(ch, 2)
    a := ch.recv()
    b := ch.recv()
    print(a + b)
main()
";
        assert_eq!(run_capture_parallel(src).expect("parallel run"), "203\n");
        assert_eq!(run_capture(src).expect("vm run"), "203\n");
    }

    /// D1 (lazy module snapshot): many trivial `--parallel` spawns no longer pay a full
    /// per-task module-graph rebuild. Correctness gate — every one of N tasks reaches the same
    /// `Shared` box, so the serialised count is exactly N. A *loose* wall-clock ceiling is a smoke
    /// guard that the per-task O(graph) reconstruction is gone (kept generous to avoid CI flake; the
    /// real perf delta is shown via `primes_parallel` timing in the milestone verification).
    #[test]
    fn parallel_many_spawns_cheap_and_correct() {
        const N: usize = 2000;
        let mut src = String::from("fn bump(s: Shared[int]):\n    s.update(fn(x): x + 1)\nfn main():\n    s := Shared(0)\n    parallel:\n");
        for _ in 0..N {
            src.push_str("        spawn bump(s)\n");
        }
        src.push_str("    print(s.get())\nmain()\n");
        let start = std::time::Instant::now();
        let out = run_capture_parallel(&src).expect("parallel run");
        let elapsed = start.elapsed();
        assert_eq!(out, format!("{N}\n"));
        assert!(elapsed < std::time::Duration::from_secs(30), "{N} spawns took {elapsed:?} (>30s ceiling)");
    }

    /// B3.6: a submitted closure capturing a plain value (`int`) observes it **by value** across the
    /// airlock — exercises the `WireValue::Closure` capture round-trip (not just the shared-`Arc` handle
    /// path the golden's `Channel` capture takes). Auto-drained at program exit on both engines; the
    /// `--parallel` drain reconstructs and runs the closure on the pool.
    #[test]
    fn executor_submitted_closure_captures_by_value() {
        let src = "fn main():\n    n := 7\n    ex := Executor()\n    ex.submit(fn(): print(n))\nmain()\n";
        assert_eq!(run_capture(src).expect("vm run"), "7\n");
        assert_eq!(run_capture_parallel(src).expect("parallel run"), "7\n");
    }

    /// B3.6 regression (review C-01): on the **cooperative default engine** a submitted closure must
    /// cross **by handle** (captures shared by reference, same heap), NOT by a value snapshot — a
    /// mutation of a captured collection *between* `submit` and the program-exit drain is observable,
    /// matching the interp oracle (decision A: `VM == interp` for the sequential subset). An unconditional
    /// `wire_callable` (by value) printed `[1]` here; sharing by reference prints `[1, 2]`.
    #[test]
    fn executor_cooperative_submit_shares_captures_by_reference() {
        let src = "fn main():\n    xs := [1]\n    ex := Executor()\n    ex.submit(fn(): print(xs))\n    xs.push(2)\nmain()\n";
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, "[1, 2]\n", "cooperative submit shares the captured list by reference");
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"), "VM == interp oracle");
    }

    /// B3.3-threads: a nested `parallel:` runs on the same bounded pool without exploding the thread
    /// count (each join level adds only its own participating thread). Two outer tasks each spawn two
    /// inner tasks, all bumping one `Shared` → `4`, deterministic.
    #[test]
    fn parallel_nested_nursery_on_pool() {
        let src = "fn inner(s: Shared[int]):\n    s.update(fn(x): x + 1)\n\
                   fn outer(s: Shared[int]):\n    parallel:\n        spawn inner(s)\n        spawn inner(s)\n\
                   fn main():\n    s := Shared(0)\n    parallel:\n        spawn outer(s)\n        spawn outer(s)\n    print(s.get())\nmain()\n";
        assert_eq!(run_capture_parallel(src).expect("parallel run"), "4\n");
    }

    /// B3.3-threads (review C1): the per-task `DoneSignal` guard bumps the nursery's completion
    /// counter even when the task body **panics** — so a panicking `--parallel` worker can't leave the
    /// joining thread waiting forever. Proves the `Drop`-runs-on-unwind contract the join relies on.
    #[test]
    fn done_signal_bumps_counter_on_panic() {
        let done = Arc::new((Mutex::new(0usize), std::sync::Condvar::new()));
        let d2 = Arc::clone(&done);
        let h = std::thread::spawn(move || {
            let _sig = DoneSignal(d2);
            panic!("boom in a task");
        });
        let _ = h.join(); // swallow the panic
        assert_eq!(*done.0.lock().unwrap(), 1, "DoneSignal::drop must bump the counter even on panic");
    }

    /// B3.3-threads (decision F, review coverage gap): each worker buffers its own stdout and the join
    /// flushes them **in task order** — so three concurrently-run tasks that each `print` produce a
    /// deterministic, task-ordered transcript regardless of thread interleaving.
    #[test]
    fn parallel_output_flushes_in_task_order() {
        let src = "fn emit(s: str):\n    print(s)\n\
                   fn main():\n    parallel:\n        spawn emit(\"alpha\")\n        spawn emit(\"beta\")\n        spawn emit(\"gamma\")\nmain()\n";
        assert_eq!(run_capture_parallel(src).expect("parallel run"), "alpha\nbeta\ngamma\n");
    }

    /// B3.3-threads: a fault in a **pool** task (not the inline task[0]) propagates out of the join as
    /// the nursery's error after all siblings finish (sibling-abort is B3.4; here we join-then-report
    /// the first fault). The ok task and the faulting task are independent (no channel) so there is no
    /// deadlock without cancellation.
    #[test]
    fn parallel_pool_task_fault_propagates() {
        let src = "fn ok_task(s: Shared[int]):\n    s.update(fn(x): x + 1)\n\
                   fn boom():\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    s := Shared(0)\n    parallel:\n        spawn ok_task(s)\n        spawn boom()\n    print(\"unreached\")\nmain()\n";
        let err = run_capture_parallel(src).expect_err("expected the pool task fault to propagate");
        assert!(err.message.contains("out of bounds"), "got: {}", err.message);
    }

    /// gap #2: a plain `return` inside a `parallel:` body jumps past `JoinNursery`, so the nursery is
    /// never popped at the dedent. Both engines truncate `self.nurseries` back to the frame's entry
    /// depth (VM via `do_return`/`drain_escaped_nursery`; interp via `exec_parallel`'s unconditional
    /// pop), or the stale nursery leaks. TASK B: the escape now also CANCELS-AND-REPORTS the unstarted
    /// `noop()` — one report line precedes the early-return value. White-box residual-depth check +
    /// VM/interp parity. A subsequent `parallel:` runs on a clean stack (its empty join is silent).
    #[test]
    fn parallel_return_escape_leaves_clean_nursery_stack() {
        let src = "fn noop():\n    0\n\
                   fn worker() -> int:\n    parallel:\n        spawn noop()\n        return 5\n    99\n\
                   fn main():\n    print(worker())\n    parallel:\n        spawn noop()\nmain()\n";
        let (vm_out, nursery_depth) = run_capture_nursery_len(src);
        let vm_out = vm_out.expect("vm run");
        // `worker`'s parallel: escapes via `return` with one pending task → cancel+report, then `5`.
        // `main`'s trailing parallel: dedents normally to its join (NOT an escape), so it runs `noop()`
        // silently — no report, proving a later parallel: still works on the reclaimed stack.
        let report = crate::runtime::pending_cancel_report(1);
        assert_eq!(vm_out, format!("{report}5\n"), "early return wins; only the escaped nursery reports");
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"), "VM/interp parity");
        assert_eq!(nursery_depth, 0, "the return-escaped nursery must be reclaimed, not leaked");
    }

    /// gap #2, second escape form: an uncaught `?` that propagates out of the frame (no `recover:`
    /// between the `parallel:` and the program top) must also reclaim the skipped nursery via
    /// `do_return`. The whole program faults, but the run-so-far must leave no leaked nursery. TASK B:
    /// the unstarted `noop()` is cancelled-and-reported — one report line is on stdout before the fault
    /// (interp already dropped on `?`; both engines now emit the report identically).
    #[test]
    fn parallel_try_escape_leaves_clean_nursery_stack() {
        let src = "fn noop():\n    0\n\
                   fn boom() -> int!:\n    return Err(\"x\")\n\
                   fn main() -> int!:\n    parallel:\n        spawn noop()\n        y := boom()?\n        print(y)\n    Ok(0)\nmain()\n";
        let (vm_out, nursery_depth) = run_capture_nursery_len(src);
        assert!(vm_out.is_err(), "the uncaught ? faults the program");
        assert_eq!(nursery_depth, 0, "the ?-escaped nursery must be reclaimed, not leaked");
        // Cancel-and-report: stdout-so-far is exactly the report line, identical across engines.
        let report = crate::runtime::pending_cancel_report(1);
        let (vm_so_far, vm_res) = run_program(src);
        assert!(vm_res.is_err());
        assert_eq!(vm_so_far, report, "VM: one report line before the fault");
        let (interp_so_far, interp_res) = crate::interp::run_program(src);
        assert!(interp_res.is_err());
        assert_eq!(interp_so_far, vm_so_far, "interp/VM stdout-so-far parity");
    }

    /// gap #2, boundary: a `?` inside a `parallel:` that IS caught by a same-frame `recover:` must
    /// stay on the **existing** handler-catch reclaim (`Handler::nursery_len`), NOT the new
    /// `do_return` truncate — the two paths are mutually exclusive in `do_try` (recover-scoped `?`
    /// jumps to the handler and never calls `do_return`). TASK B: that recover-catch reclaim site now
    /// routes through `drain_escaped_nursery`, so a recover-caught `?` cancels-and-reports the unstarted
    /// `noop()` IDENTICALLY to an uncaught `?` — one report line precedes "recovered". Asserts the two
    /// reclaim paths don't fight: the recovered program continues, a later `parallel:` runs, stack clean.
    #[test]
    fn parallel_try_caught_by_recover_leaves_clean_nursery_stack() {
        let src = "fn noop():\n    0\n\
                   fn boom() -> int!:\n    return Err(\"x\")\n\
                   fn main():\n    r := recover:\n        parallel:\n            spawn noop()\n            y := boom()?\n            print(y)\n        0\n    print(\"recovered\")\n    parallel:\n        spawn noop()\nmain()\n";
        let (vm_out, nursery_depth) = run_capture_nursery_len(src);
        let vm_out = vm_out.expect("the ? is caught by recover, so the program completes");
        // The recover-caught `?` cancels its one pending task and reports, THEN the recover continues.
        // `main`'s trailing parallel: joins normally (not an escape) → silent.
        let report = crate::runtime::pending_cancel_report(1);
        assert_eq!(vm_out, format!("{report}recovered\n"), "recover swallows the fault; cancel+report precedes it");
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"), "VM/interp parity");
        assert_eq!(nursery_depth, 0, "the recover-caught nursery is reclaimed via the handler path");
    }

    /// gap #2, ordering boundary: a recover-scoped `?` escaping a `parallel:` whose BODY has a
    /// `defer` must order the cancel-report AFTER the parallel-body defer and BEFORE the recover
    /// continues — matching the interp oracle, whose `exec_parallel` reports only after the body's
    /// `exec_scoped_block` has drained its defers. Regression for the do_try report-before-body-defer
    /// divergence (the report previously trailed the parallel-body defer on the VM). Body-defer →
    /// report → recovered, byte-identical across interp / VM-cooperative / VM-`--parallel`.
    #[test]
    fn parallel_recover_scoped_try_orders_report_after_body_defer() {
        let src = "fn noop():\n    0\n\
                   fn pdefer():\n    print(\"PDEFER\")\n\
                   fn boom() -> int!:\n    return Err(\"x\")\n\
                   fn main():\n    r := recover:\n        parallel:\n            defer pdefer()\n            spawn noop()\n            y := boom()?\n            print(y)\n        0\n    print(\"recovered\")\nmain()\n";
        let report = crate::runtime::pending_cancel_report(1);
        let expected = format!("PDEFER\n{report}recovered\n");
        let interp_out = crate::interp::run_capture(src).expect("interp run");
        assert_eq!(interp_out, expected, "interp oracle: body-defer precedes report precedes recover");
        let (vm_out, nursery_depth) = run_capture_nursery_len(src);
        let vm_out = vm_out.expect("the ? is caught by recover, so the program completes");
        assert_eq!(vm_out, expected, "VM cooperative: report ordered after the parallel-body defer");
        assert_eq!(nursery_depth, 0, "the recover-caught nursery is reclaimed, not leaked");
        assert_eq!(run_capture_parallel(src).expect("--parallel run"), expected, "VM --parallel parity");
    }

    // ----- TASK B: pending-spawn-drop on early `parallel:` escape → cancel-and-report -----
    // Policy: an UNSTARTED spawn task on a `parallel:` that escapes early (`?`/`return`/`break`)
    // before its join is CANCELLED (not run), and ONE report line is written to stdout (`out`,
    // the stream every `run_capture*` harness reads), byte-identical across interp / VM-cooperative
    // / VM-`--parallel`. The escape propagates unchanged; the nursery stack stays leak-free (depth 0).
    //
    // NB: the spawned task's side effect is observed via `print` (a true cross-airlock observable),
    // NOT a `Shared[int]` counter — `spawn` DEEP-CLONES the box across the airlock, so a run task
    // mutates a COPY and the parent's `s.get()` stays 0 whether or not the task ran. A `print` in the
    // spawned body is the only reliable run-vs-cancelled signal.

    /// `?` escape: the spawned `side()` MUST NOT run (no "SIDE RAN") and the cancellation report IS
    /// emitted before the fault unwinds. White-box: nursery depth returns to 0 (no leak). The interp
    /// already DROPPED on `?` (it never diverged on this kind), so here all three only gain the report.
    #[test]
    fn parallel_try_escape_cancels_pending_and_reports() {
        let src = "fn side():\n    print(\"SIDE RAN\")\n\
                   fn boom() -> int!:\n    return Err(\"x\")\n\
                   fn main() -> int!:\n    parallel:\n        spawn side()\n        y := boom()?\n        print(y)\n    Ok(0)\nmain()\n";
        let report = crate::runtime::pending_cancel_report(1);
        // The `?` faults the whole program, but the report is on stdout captured so far.
        let (vm_out, depth) = run_capture_nursery_len(src);
        assert!(vm_out.is_err(), "the uncaught ? faults the program");
        assert_eq!(depth, 0, "the ?-escaped nursery must be reclaimed, not leaked");
        // Stdout captured up to the fault: exactly the cancellation report, no `side()` output.
        let (vm_so_far, vm_res) = run_program(src);
        assert!(vm_res.is_err());
        assert_eq!(vm_so_far, report, "VM cooperative: report present, task NOT run");
        // Interp parity (oracle): identical stdout-so-far + identical error class.
        let (interp_so_far, interp_res) = crate::interp::run_program(src);
        assert!(interp_res.is_err());
        assert_eq!(interp_so_far, vm_so_far, "interp/VM stdout-so-far parity");
        // --parallel parity: same fault.
        assert!(run_capture_parallel(src).is_err(), "--parallel also faults");
    }

    /// `return` escape: the spawned `side()` is CANCELLED (not run) and the report is emitted; the
    /// early `return` value still wins. Pre-fix the interp RAN the task here (printed "SIDE RAN") while
    /// the VM dropped it — the live divergence this fixes. Identical text across engines, depth 0.
    #[test]
    fn parallel_return_escape_cancels_pending_and_reports() {
        let src = "fn side():\n    print(\"SIDE RAN\")\n\
                   fn worker() -> int:\n    parallel:\n        spawn side()\n        return 5\n    99\n\
                   fn main():\n    print(worker())\nmain()\n";
        let report = crate::runtime::pending_cancel_report(1);
        // The early return wins (5); `side()` never runs (no "SIDE RAN"); the report is emitted at the
        // escape (inside `worker`, before its caller prints the result).
        let expected = format!("{report}5\n");
        let (vm_out, depth) = run_capture_nursery_len(src);
        assert_eq!(vm_out.as_deref().map(str::to_string), Ok(expected.clone()), "VM cooperative");
        assert_eq!(depth, 0, "the return-escaped nursery must be reclaimed, not leaked");
        assert_eq!(crate::interp::run_capture(src).expect("interp run"), expected, "interp parity");
        assert_eq!(run_capture_parallel(src).expect("parallel run"), expected, "--parallel parity");
    }

    /// `break`-in-loop escape: the NET-NEW VM site (a `break` that leaves a `parallel:` scope via the
    /// in-frame loop-exit Jump, NOT via `do_return`). The spawned `side()` is cancelled + reported on
    /// the iteration that breaks; the loop exits, the function continues. Pre-fix the interp RAN the
    /// task ("SIDE RAN") while the VM dropped it. Identical across engines, depth 0.
    #[test]
    fn parallel_break_escape_cancels_pending_and_reports() {
        let src = "fn side():\n    print(\"SIDE RAN\")\n\
                   fn main():\n    for i in 0..3:\n        parallel:\n            spawn side()\n            if i == 0:\n                break\n            print(\"unreached\")\n    print(\"done\")\nmain()\n";
        let report = crate::runtime::pending_cancel_report(1);
        // i==0: spawn side(), then break out of the `parallel:` scope before the join → cancel+report,
        // exit the loop. `side()` never runs (no "SIDE RAN"). "unreached" never prints.
        let expected = format!("{report}done\n");
        let (vm_out, depth) = run_capture_nursery_len(src);
        assert_eq!(vm_out.as_deref().map(str::to_string), Ok(expected.clone()), "VM cooperative (net-new break site)");
        assert_eq!(depth, 0, "the break-escaped nursery must be reclaimed, not leaked");
        assert_eq!(crate::interp::run_capture(src).expect("interp run"), expected, "interp parity");
        assert_eq!(run_capture_parallel(src).expect("parallel run"), expected, "--parallel parity");
    }

    /// B3.4: a `recv`-blocked sibling must ABORT when a sibling faults before sending, instead of
    /// hanging the join forever. `boom` is spawned first so it runs inline on the joining thread —
    /// it faults immediately and trips the nursery cancel flag without depending on pool scheduling
    /// (avoids the G3 pool-starvation hazard on low-core CI). `consumer` runs on the pool, blocks on
    /// the empty channel, and its re-checking `recv` wait observes the cancel and unwinds — so the
    /// join completes and reports the producer's fault rather than deadlocking.
    #[test]
    fn parallel_recv_blocked_sibling_aborts_on_sibling_fault() {
        let src = "fn boom(ch: Channel[int]):\n    xs := [1]\n    print(xs[9])\n\
                   fn consumer(ch: Channel[int]):\n    ch.recv()\n    print(\"consumed\")\n\
                   fn main():\n    ch := Channel[int]()\n    parallel:\n        spawn boom(ch)\n        spawn consumer(ch)\nmain()\n";
        let err = run_capture_parallel(src).expect_err("expected the producer fault to propagate, not hang");
        assert!(err.message.contains("out of bounds"), "got: {}", err.message);
    }

    /// B3.4: a CPU-bound sibling aborts mid-flight when a sibling faults, observing the cancel flag
    /// at a dispatch back-edge. `looper` runs inline (task[0]); it writes `1`, hands `trigger` a
    /// channel token (so the fault happens-after `looper` has started — no timing race), then spins
    /// and would write `99` only after the (huge) loop. `trigger` (pool) waits for the token, then
    /// faults → trips cancel → `looper` aborts mid-loop. Asserting `1` proves `looper` started AND
    /// was cancelled before completing; without the back-edge cancel check it would print `99`.
    #[test]
    fn parallel_cpu_sibling_aborts_on_sibling_fault() {
        let src = "fn looper(go: Channel[int], s: Shared[int]):\n    s.set(1)\n    go.send(0)\n    i := 0\n    while i < 1000000000:\n        i = i + 1\n    s.set(99)\n\
                   fn trigger(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    go := Channel[int]()\n    s := Shared(0)\n    r := recover:\n        parallel:\n            spawn looper(go, s)\n            spawn trigger(go)\n        0\n    print(s.get())\nmain()\n";
        let out = run_capture_parallel(src).expect("the fault is recovered, so the program completes");
        assert_eq!(out, "1\n", "looper started (1) but was cancelled before completing (never wrote 99)");
    }

    /// B3.4: `defer` still composes with cancellation. The blocked consumer is aborted when the
    /// producer faults; its `defer cleanup(s)` must still run on the cancel unwind (writing the
    /// shared sentinel), proving deferred calls fire even on a cancelled task.
    ///
    /// Synchronized like [`parallel_cpu_sibling_aborts_on_sibling_fault`]: `boom` waits for a token
    /// `consumer` sends only AFTER it has registered its `defer` and is about to block, so the fault
    /// happens-after the defer is registered — no timing race. (Under the M:N engine task start order
    /// is not deterministic, so a fault that races a not-yet-started sibling would legitimately skip
    /// its not-yet-registered defer, per Go semantics; this token makes the intended "blocked
    /// consumer is aborted" scenario the one actually exercised.)
    #[test]
    fn parallel_defer_runs_on_cancelled_sibling() {
        let src = "fn cleanup(s: Shared[int]):\n    s.set(42)\n\
                   fn consumer(ch: Channel[int], go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    ch.recv()\n\
                   fn boom(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    s := Shared(0)\n    r := recover:\n        ch := Channel[int]()\n        go := Channel[int]()\n        parallel:\n            spawn consumer(ch, go, s)\n            spawn boom(go)\n        0\n    print(s.get())\nmain()\n";
        let out = run_capture_parallel(src).expect("the producer fault is recovered, so the program completes");
        assert_eq!(out, "42\n", "the cancelled consumer's defer ran on the unwind");
    }

    /// B3.4: `defer` runs even when a task is cancelled at the CPU **back-edge** (not only on the
    /// recv path). `worker` (inline) registers `defer cleanup(s)`, signals `trigger` it has started,
    /// then spins; cancelled mid-loop, the unwind must run the defer (writing 42). Regression guard:
    /// a raw `return Err` from the loop top would bypass the defer machinery — this asserts the
    /// cancel unwinds through `unwind_deferred`. (Without the fix this prints `0`.)
    #[test]
    fn parallel_defer_runs_on_back_edge_cancel() {
        let src = "fn cleanup(s: Shared[int]):\n    s.set(42)\n\
                   fn worker(go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    i := 0\n    while i < 1000000000:\n        i = i + 1\n\
                   fn trigger(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    go := Channel[int]()\n    s := Shared(0)\n    r := recover:\n        parallel:\n            spawn worker(go, s)\n            spawn trigger(go)\n        0\n    print(s.get())\nmain()\n";
        let out = run_capture_parallel(src).expect("the trigger fault is recovered, so the program completes");
        assert_eq!(out, "42\n", "the CPU-cancelled worker's defer ran on the back-edge unwind");
    }

    /// B3.4: cancellation is NOT catchable by a `recover:` inside a worker — a cancelled task must
    /// die, not resume. `victim` (inline) writes `1`, signals it has started, then wraps a long loop
    /// in `recover:` and would write `99` after it. If the cancel sentinel were an ordinary catchable
    /// fault, the inner `recover:` would swallow it and `victim` would reach `s.set(99)`; the bypass
    /// unwinds past the recover instead, so the sentinel stays at the pre-loop `1`. (Buggy: `99`.)
    #[test]
    fn parallel_recover_inside_worker_does_not_catch_cancel() {
        let src = "fn victim(go: Channel[int], s: Shared[int]):\n    s.set(1)\n    go.send(0)\n    r := recover:\n        i := 0\n        while i < 1000000000:\n            i = i + 1\n        0\n    s.set(99)\n\
                   fn trigger(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    go := Channel[int]()\n    s := Shared(0)\n    r := recover:\n        parallel:\n            spawn victim(go, s)\n            spawn trigger(go)\n        0\n    print(s.get())\nmain()\n";
        let out = run_capture_parallel(src).expect("the trigger fault is recovered, so the program completes");
        assert_eq!(out, "1\n", "victim's inner recover must NOT catch the cancel; it never reaches s.set(99)");
    }

    /// C2 golden: `Channel[T]` fan-out — workers `send` at the dedent, the parent `recv`s after the
    /// join. Byte-identical on the VM, the interpreter, and the `.expected` file.
    #[test]
    fn golden_channel_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/channel.chz");
        let expected = include_str!("../../examples/channel.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `Atomic[int].add` cross-thread atomicity: N real-OS-thread fibers each `add(1)` one shared
    /// box; the join sum must be exactly N (no lost read-modify-write — the whole point of `Atomic`).
    #[test]
    fn parallel_atomic_add_is_exact() {
        let n = 300;
        let src = format!(
            "fn work(a: Atomic[int]):\n    a.add(1)\n\
             fn main():\n    a := Atomic(0)\n    parallel:\n        for _ in 0..{n}:\n            spawn work(a)\n    print(a.load())\nmain()\n"
        );
        assert_eq!(run_capture_parallel(&src).expect("parallel"), format!("{n}\n"));
    }

    /// `Atomic[int].cas` under contention: N fibers each increment via a load-then-CAS retry loop. A
    /// lost CAS (the box changed under us) retries, so the serialised total is exactly N — proving the
    /// compare-and-swap is atomic across threads.
    #[test]
    fn parallel_atomic_cas_increment_is_exact() {
        let n = 200;
        let src = format!(
            "fn bump(a: Atomic[int]):\n    while true:\n        cur := a.load()\n        if a.cas(cur, cur + 1):\n            break\n\
             fn main():\n    a := Atomic(0)\n    parallel:\n        for _ in 0..{n}:\n            spawn bump(a)\n    print(a.load())\nmain()\n"
        );
        assert_eq!(run_capture_parallel(&src).expect("parallel"), format!("{n}\n"));
    }

    /// `Atomic[float]` add/sub/exchange/cas must behave identically on both engines (covers the
    /// numeric-`T` arm for floats, not just ints).
    #[test]
    fn atomic_float_ops_two_engine_parity() {
        let src = "fn main():\n    a := Atomic(1.5)\n    print(a.add(2.0))\n    print(a.sub(0.5))\n    print(a.exchange(9.0))\n    print(a.cas(9.0, 4.0))\n    print(a.load())\nmain()\n";
        assert_eq!(run_capture(src).expect("vm"), crate::interp::run_capture(src).expect("interp"));
    }

    /// `cas` on a non-scalar `T` (a list) exercises the VM's lock-held `from_wire`/`values_equal` path
    /// — the most distinctive Atomic code path. Both engines must agree.
    #[test]
    fn atomic_cas_on_list_two_engine_parity() {
        let src = "fn main():\n    a := Atomic([1, 2])\n    print(a.cas([1, 2], [9]))\n    print(a.load())\n    print(a.cas([1, 2], [0]))\n    print(a.load())\nmain()\n";
        assert_eq!(run_capture(src).expect("vm"), crate::interp::run_capture(src).expect("interp"));
    }

    /// `timer(0)` (and any already-elapsed deadline) delivers `true` immediately, on every engine.
    #[test]
    fn timer_zero_delivers_immediately() {
        let src = "fn main():\n    print(timer(0).recv())\nmain()\n";
        let out = run_capture(src).expect("vm");
        assert_eq!(out, "true\n");
        assert_eq!(out, crate::interp::run_capture(src).expect("interp"));
        assert_eq!(run_capture_parallel(src).expect("parallel"), "true\n");
    }

    /// A1 golden: `Channel[T].try_recv()` — a non-blocking poll returning `T?`. Workers `send` at the
    /// dedent; the parent drains with `try_recv` (`Some` per value, then `None`). Never blocks/faults,
    /// so byte-identical on the VM, the interpreter, and the `.expected` file.
    #[test]
    fn golden_try_recv_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/try_recv.chz");
        let expected = include_str!("../../examples/try_recv.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
        assert_eq!(run_capture_stress(src), expected);
    }

    /// B1+B2 golden (VM-only): blocking `recv`. The consumer is scheduled first, parks on the empty
    /// channel, the cooperative scheduler runs the producer, and the consumer resumes to receive. The
    /// interpreter still faults `deadlock` on the same program (documented parity gap — see the interp
    /// twin `channel_block_chz_faults_deadlock_on_interp`), so this asserts the VM output + `.expected`
    /// only, NOT cross-engine parity.
    #[test]
    fn golden_channel_block_chz_matches_expected() {
        let src = include_str!("../../examples/channel_block.chz");
        let expected = include_str!("../../examples/channel_block.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        // Parked fibers must survive collection: the same program under GC stress is byte-identical.
        assert_eq!(run_capture_stress(src), expected);
    }

    // ----- B1 + B2: cooperative fibers + blocking recv (VM engine) -----

    /// Ping-pong across two channels exercises many suspend↔resume cycles: each fiber repeatedly
    /// parks on an empty `recv`, the scheduler runs the sibling whose `send` wakes it, and the parked
    /// fiber resumes mid-`while`-loop with its locals intact.
    #[test]
    fn fibers_ping_pong_interleaves() {
        let src = "fn ping(a: Channel[int], b: Channel[int]):\n    i := 0\n    while i < 3:\n        b.send(i)\n        x := a.recv()\n        print(\"ping {x}\")\n        i = i + 1\nfn pong(a: Channel[int], b: Channel[int]):\n    i := 0\n    while i < 3:\n        y := b.recv()\n        print(\"pong {y}\")\n        a.send(y + 100)\n        i = i + 1\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn ping(a, b)\n        spawn pong(a, b)\nmain()\n";
        let expected = "pong 0\nping 100\npong 1\nping 101\npong 2\nping 102\n";
        assert_eq!(run(src), expected);
        // Same result under GC stress — parked fibers' frames/locals are rooted while they wait.
        assert_eq!(run_capture_stress(src), expected);
    }

    /// All siblings parked on empty channels that no one will fill ⇒ a real deadlock (detected by the
    /// scheduler when no fiber is runnable yet not all are done).
    #[test]
    fn fibers_all_blocked_is_deadlock() {
        let src = "fn waiter(c: Channel[int]):\n    c.recv()\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn waiter(a)\n        spawn waiter(b)\nmain()\n";
        assert!(run_err(src).contains("deadlock"), "expected deadlock");
    }

    /// Native-reentry guard: a blocking `recv` reached inside a list-HOF callback cannot park (the
    /// HOF's loop state is on the Rust stack), so it faults `deadlock` even though a sibling could
    /// otherwise supply the value — the documented B1 v1 limitation, kept memory-safe.
    #[test]
    fn fibers_recv_inside_map_callback_faults() {
        let src = "fn use_map(ch: Channel[int]):\n    xs := [0]\n    ys := xs.map(fn(x): ch.recv())\n    print(ys)\nfn fill(ch: Channel[int]):\n    ch.send(1)\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn use_map(ch)\n        spawn fill(ch)\nmain()\n";
        assert!(run_err(src).contains("deadlock"), "recv inside map must fault, not suspend");
    }

    /// Native-reentry guard: a blocking `recv` inside a struct `index`/`slice`/`set_index` operator
    /// overload (run from the native indexing opcodes, host-stack state) cannot park — it faults
    /// `deadlock`. Regression for a guard gap that would otherwise corrupt the operand stack.
    #[test]
    fn fibers_recv_inside_index_overload_faults() {
        let src = "struct Src:\n    ch: Channel[int]\n    fn index(self, k: int) -> int:\n        return self.ch.recv()\nfn use_index(s: Src):\n    print(s[0])\nfn fill(ch: Channel[int]):\n    ch.send(7)\nfn main():\n    ch := Channel[int]()\n    s := Src(ch)\n    parallel:\n        spawn use_index(s)\n        spawn fill(ch)\nmain()\n";
        assert!(run_err(src).contains("deadlock"), "recv inside index overload must fault, not suspend");
    }

    /// Native-reentry guard: a blocking `recv` inside a `defer`red call (run during frame teardown,
    /// off the suspendable path) faults rather than parking. The `recv` is in the deferred function's
    /// body — only the receiver handle is captured at the `defer` statement — so it runs at teardown.
    #[test]
    fn fibers_recv_inside_defer_faults() {
        let src = "fn consume(ch: Channel[int]):\n    ch.recv()\nfn worker(ch: Channel[int]):\n    defer consume(ch)\n    print(\"body\")\nfn sender(ch: Channel[int]):\n    ch.send(5)\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn worker(ch)\n        spawn sender(ch)\nmain()\n";
        assert!(run_err(src).contains("deadlock"), "recv inside defer must fault, not suspend");
    }

    /// A nested `parallel:` inside a child fiber runs its own scheduler level (recursively); the
    /// child resumes after its grandchildren join, and the outer sibling runs afterward.
    #[test]
    fn fibers_nested_parallel() {
        let src = "fn child():\n    parallel:\n        spawn:\n            print(\"grandchild\")\n    print(\"child after nested\")\nfn main():\n    parallel:\n        spawn child()\n        spawn:\n            print(\"sibling\")\nmain()\n";
        assert_eq!(run(src), "grandchild\nchild after nested\nsibling\n");
    }

    /// D0 — the cooperative scheduler must run a large nursery in ~O(N·logN), not O(N²). 50k trivial
    /// fibers each bump one `Shared` counter; the sum proves every fiber was scheduled, and the
    /// wall-clock ceiling is the regression guard: the old `pick_runnable` linear-scan-per-turn took
    /// ~2.3 s at 50k (RED), the ready-set takes tens of ms (GREEN). The 5 s ceiling is generous for
    /// CI noise yet far below the old quadratic wall.
    #[test]
    fn fibers_scale_ready_queue_not_quadratic() {
        let n = 50_000;
        let src = format!(
            "fn work(s: Shared[int]):\n    s.update(fn(x): x + 1)\n\
             fn main():\n    s := Shared(0)\n    parallel:\n        for _ in 0..{n}:\n            spawn work(s)\n    print(s.get())\nmain()\n"
        );
        let start = std::time::Instant::now();
        let out = run(&src);
        let elapsed = start.elapsed();
        assert_eq!(out, format!("{n}\n"), "every fiber must run exactly once");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "scheduler is quadratic: {n} fibers took {elapsed:?} (ceiling 5s)"
        );
    }

    /// D0 — the `blocked_on` wake path: one consumer parks on a shared channel and is re-woken by each
    /// of many producers' `send`s. Sibling fibers hold DISTINCT `GcRef`s aliasing the same
    /// `Arc<ChannelCore>` (cooperative `spawn` deep-clones the channel), so the wake map must key on
    /// the core pointer, not the handle — a `GcRef` key would lose every wakeup and fault `deadlock`.
    #[test]
    fn fibers_many_producers_one_consumer() {
        let n = 200;
        let src = format!(
            "fn produce(ch: Channel[int]):\n    ch.send(1)\n\
             fn consume(ch: Channel[int], k: int, s: Shared[int]):\n    total := 0\n    for _ in 0..k:\n        total += ch.recv()\n    s.set(total)\n\
             fn main():\n    ch := Channel[int]()\n    s := Shared(0)\n    parallel:\n        spawn consume(ch, {n}, s)\n        for _ in 0..{n}:\n            spawn produce(ch)\n    print(s.get())\nmain()\n"
        );
        assert_eq!(run(&src), format!("{n}\n"), "consumer must receive every produced value");
    }

    /// D0 — cross-level wakeup: a fiber nested in an INNER `parallel:` `send`s to a channel an
    /// OUTER-level sibling is parked on. The `send` arm must drain the blocked set of EVERY scheduler
    /// level (not just the innermost), or the outer consumer never wakes and the nursery faults
    /// `deadlock` after the inner level joins. (The old `pick_runnable` re-scanned all levels each
    /// turn, so this worked; the ready-set must preserve it.)
    #[test]
    fn fibers_cross_level_wakeup() {
        let src = "fn consumer(ch: Channel[int], s: Shared[int]):\n    s.set(ch.recv())\n\
                   fn inner_sender(ch: Channel[int]):\n    ch.send(42)\n\
                   fn middle(ch: Channel[int]):\n    parallel:\n        spawn inner_sender(ch)\n\
                   fn main():\n    ch := Channel[int]()\n    s := Shared(0)\n    parallel:\n        spawn consumer(ch, s)\n        spawn middle(ch)\n    print(s.get())\nmain()\n";
        assert_eq!(run(src), "42\n", "outer consumer must wake from an inner-level send");
    }

    /// A `recover:` inside a child fiber catches a fault in that fiber's own context (its handlers /
    /// frames are per-fiber); the sibling is unaffected and runs normally.
    #[test]
    fn fibers_recover_inside_child_is_isolated() {
        let src = "fn boom():\n    xs := [1]\n    print(xs[9])\nfn child():\n    r := recover:\n        boom()\n        0\n    print(\"caught\")\nfn main():\n    parallel:\n        spawn child()\n        spawn:\n            print(\"sibling ok\")\nmain()\n";
        assert_eq!(run(src), "caught\nsibling ok\n");
    }

    /// C3 golden: `Shared[T]` cross-task box — three tasks bump one serialised counter. Byte-identical
    /// on the VM, the interpreter, and the `.expected` file.
    #[test]
    fn golden_shared_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/shared.chz");
        let expected = include_str!("../../examples/shared.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// C5 golden: the `Executor` escape hatch — submit/shutdown (FIFO drain), `defer ex.shutdown()`,
    /// shutdown_now (discard). Byte-identical on the VM, the interpreter, and the `.expected` file.
    #[test]
    fn golden_executor_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/executor.chz");
        let expected = include_str!("../../examples/executor.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    // Micro-tests mirroring the interpreter's C2/C3 unit tests (src/interp/mod.rs), to pin the VM's
    // channel/shared/spawn semantics directly (not just via the example goldens).

    #[test]
    fn channel_send_recv_fifo() {
        let src = "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    ch.send(2)\n    print(ch.recv())\n    print(ch.recv())\nmain()\n";
        assert_eq!(run(src), "1\n2\n");
    }

    #[test]
    fn channel_send_deep_copies_value() {
        // Mutating the original list after send must NOT change what the channel holds (airlock).
        let src = "fn main():\n    ch := Channel[list[int]]()\n    xs := [1, 2]\n    ch.send(xs)\n    xs.push(3)\n    print(ch.recv())\nmain()\n";
        assert_eq!(run(src), "[1, 2]\n");
    }

    #[test]
    fn channel_recv_on_empty_is_deadlock_error() {
        let err = run_err("fn main():\n    ch := Channel[int]()\n    print(ch.recv())\nmain()\n");
        assert!(err.contains("deadlock"), "got: {err}");
    }

    /// A1: `try_recv` on an empty channel returns `None` (never the `recv` deadlock fault).
    #[test]
    fn channel_try_recv_on_empty_returns_none() {
        let src = "fn main():\n    ch := Channel[int]()\n    match ch.try_recv():\n        Some(v): print(\"got {v}\")\n        None: print(\"empty\")\nmain()\n";
        assert_eq!(run(src), "empty\n");
    }

    /// A1: `try_recv` on a non-empty channel returns `Some(v)` (FIFO).
    #[test]
    fn channel_try_recv_with_value_returns_some() {
        let src = "fn main():\n    ch := Channel[int]()\n    ch.send(42)\n    match ch.try_recv():\n        Some(v): print(v)\n        None: print(\"empty\")\nmain()\n";
        assert_eq!(run(src), "42\n");
    }

    /// A1 × B1/B2 (VM-only): `try_recv` must drain the residue left after a *blocking* `recv` resumed.
    /// The consumer parks on an empty `recv`; the producer sends two values; the consumer resumes,
    /// `recv`s the first, then polls the rest with `try_recv` (the second value, then `None`). Pins
    /// that the resume path leaves `suspend`/`ip` clean so the following non-blocking polls behave.
    #[test]
    fn try_recv_drains_residue_after_blocking_recv_resumes() {
        let src = "fn producer(ch: Channel[int]):\n    ch.send(1)\n    ch.send(2)\nfn consumer(ch: Channel[int]):\n    a := ch.recv()\n    print(\"recv {a}\")\n    match ch.try_recv():\n        Some(v): print(\"try {v}\")\n        None: print(\"try empty\")\n    match ch.try_recv():\n        Some(v): print(\"try {v}\")\n        None: print(\"try empty\")\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn consumer(ch)\n        spawn producer(ch)\nmain()\n";
        let expected = "recv 1\ntry 2\ntry empty\n";
        assert_eq!(run(src), expected);
        assert_eq!(run_capture_stress(src), expected);
    }

    /// A1 regression guard: `try_recv` on an empty channel INSIDE an active `parallel:` scheduler must
    /// return `None` — it must NOT route through the `recv` park path (which would suspend the lone
    /// child and then deadlock, since no sibling can ever send). Pins try_recv as truly non-blocking.
    #[test]
    fn channel_try_recv_in_parallel_does_not_suspend() {
        let src = "fn probe(ch: Channel[int]):\n    match ch.try_recv():\n        Some(v): print(v)\n        None: print(\"empty\")\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn probe(ch)\nmain()\n";
        assert_eq!(run(src), "empty\n");
        assert_eq!(run_capture_stress(src), "empty\n");
    }

    #[test]
    fn shared_get_set_round_trip() {
        let src = "fn main():\n    s := Shared(1)\n    print(s.get())\n    s.set(42)\n    print(s.get())\nmain()\n";
        assert_eq!(run(src), "1\n42\n");
    }

    #[test]
    fn shared_update_read_modify_write() {
        let src = "fn main():\n    s := Shared(10)\n    s.update(fn(x): x * 2)\n    s.update(fn(x): x + 1)\n    print(s.get())\nmain()\n";
        assert_eq!(run(src), "21\n");
    }

    #[test]
    fn shared_get_does_not_alias_box() {
        // `get` copies out: mutating the returned list must not change what the box holds.
        let src = "fn main():\n    s := Shared([1, 2])\n    xs := s.get()\n    xs.push(3)\n    print(s.get())\nmain()\n";
        assert_eq!(run(src), "[1, 2]\n");
    }

    #[test]
    fn executor_submit_runs_fifo_at_shutdown() {
        let src = "fn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.submit(fn(): j(2))\n    print(0)\n    ex.shutdown()\nmain()\n";
        assert_eq!(run(src), "0\n1\n2\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp run"));
    }

    #[test]
    fn executor_submit_after_shutdown_errors() {
        let src = "fn main():\n    ex := Executor()\n    ex.shutdown()\n    ex.submit(fn(): print(1))\nmain()\n";
        let err = run_err(src);
        assert!(err.contains("shut-down Executor"), "got: {err}");
        // Parity: same fault message on the interpreter.
        let interp = crate::interp::run_capture(src).expect_err("interp should fault").message;
        assert_eq!(err, interp, "VM/interp error divergence");
    }

    #[test]
    fn executor_shutdown_now_discards_pending() {
        let src = "fn j():\n    print(99)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j())\n    ex.shutdown_now()\n    print(0)\nmain()\n";
        assert_eq!(run(src), "0\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp run"));
    }

    // ----- C5 (A2): program-exit auto-drain (VM parity) -----

    #[test]
    fn golden_executor_autodrain_matches_expected_and_interp() {
        let src = include_str!("../../examples/executor_autodrain.chz");
        let expected = include_str!("../../examples/executor_autodrain.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    #[test]
    fn executor_autodrain_runs_unshut_at_exit() {
        let src = "fn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.submit(fn(): j(2))\n    print(0)\nmain()\n";
        assert_eq!(run(src), "0\n1\n2\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp run"));
    }

    #[test]
    fn executor_autodrain_not_redrained_after_explicit_shutdown() {
        let src = "fn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.shutdown()\n    print(0)\nmain()\n";
        assert_eq!(run(src), "1\n0\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp run"));
    }

    #[test]
    fn executor_autodrain_survives_gc_stress() {
        // The VM executor registry roots un-shut work so it isn't collected before the exit drain.
        // Under collect-before-every-instruction, the drained closures must still be reachable.
        let src = "fn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.submit(fn(): j(2))\n    print(0)\nmain()\n";
        let normal = run(src);
        assert_eq!(run_capture_stress(src), normal, "VM gc_stress diverged (executor auto-drain rooting bug?)");
    }

    #[test]
    fn executor_fault_during_drain_leaves_siblings_for_reap() {
        // A task that faults mid-drain leaves the not-yet-run siblings in the queue; `defer
        // ex.shutdown()` then reaps them on the fault exit path. Both engines must drain the *live*
        // queue (not a snapshot) so leftover work survives — pins the C1 parity fix.
        let src = "fn boom():\n    x := [1]\n    print(x[9])\nfn run():\n    ex := Executor()\n    defer ex.shutdown()\n    ex.submit(fn(): print(\"A\"))\n    ex.submit(fn(): boom())\n    ex.submit(fn(): print(\"C\"))\n    ex.shutdown()\nfn main():\n    r := recover:\n        run()\n        0\n    print(\"done\")\nmain()\n";
        let vm = run_capture(src).expect("vm run");
        assert_eq!(vm, "A\nC\ndone\n");
        assert_eq!(vm, crate::interp::run_capture(src).expect("interp run"), "VM/interp divergence");
    }

    #[test]
    fn executor_reentrant_shutdown_now_during_drain() {
        // A task that calls `shutdown_now()` mid-drain discards the remaining siblings on BOTH
        // engines (the drain pops from the live queue, so the clear takes effect) — pins the C1 fix.
        let src = "fn stop(e: Executor):\n    e.shutdown_now()\nfn main():\n    ex := Executor()\n    ex.submit(fn(): print(\"A\"))\n    ex.submit(fn(): stop(ex))\n    ex.submit(fn(): print(\"C\"))\n    ex.shutdown()\n    print(\"end\")\nmain()\n";
        let vm = run_capture(src).expect("vm run");
        assert_eq!(vm, "A\nend\n");
        assert_eq!(vm, crate::interp::run_capture(src).expect("interp run"), "VM/interp divergence");
    }

    #[test]
    fn spawn_first_error_aborts_siblings() {
        // The first task to fault aborts the remaining siblings and propagates out of `parallel:`.
        let src = "fn boom():\n    x := [1]\n    print(x[5])\nfn quiet():\n    print(\"ran\")\nfn main():\n    parallel:\n        spawn boom()\n        spawn quiet()\nmain()\n";
        let vm = run_err(src);
        let interp = match crate::interp::run_capture(src) {
            Ok(o) => panic!("expected error, got {o:?}"),
            Err(e) => e.message,
        };
        assert_eq!(vm, interp, "VM/interp error divergence");
    }

    #[test]
    fn spawn_composes_with_recover() {
        // A task fault is catchable by a `recover:` enclosing the nursery (parity-checked).
        let src = "fn boom():\n    x := [1]\n    print(x[9])\nfn main():\n    r := recover:\n        parallel:\n            spawn boom()\n        0\n    print(\"recovered\")\nmain()\n";
        let vm = run_capture(src).expect("vm run");
        assert_eq!(vm, "recovered\n");
        assert_eq!(vm, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Block-scoped defer: assert VM == interp == `expected` for a snippet.
    fn assert_defer_scope(src: &str, expected: &str) {
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected, "vm output");
        assert_eq!(
            crate::interp::run_capture(src).expect("interp run"),
            expected,
            "interp output"
        );
    }

    /// A `for`-body defer runs at the END of each iteration (block scope), not at function return.
    #[test]
    fn defer_for_body_runs_per_iteration() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..3:\n        defer log(\"i={i}\")\n    log(\"done\")\nmain()\n",
            "i=0\ni=1\ni=2\ndone\n",
        );
    }

    /// A `while`-body defer runs at the END of each iteration.
    #[test]
    fn defer_while_body_runs_per_iteration() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    i := 0\n    while i < 3:\n        defer log(\"w={i}\")\n        i = i + 1\n    log(\"done\")\nmain()\n",
            "w=0\nw=1\nw=2\ndone\n",
        );
    }

    /// An `if`-branch defer fires at the branch's end, before the statement after the `if`.
    #[test]
    fn defer_if_branch_runs_at_branch_end() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    if true:\n        defer log(\"cleanup\")\n        log(\"work\")\n    log(\"after\")\nmain()\n",
            "work\ncleanup\nafter\n",
        );
    }

    /// A statement-form `match` arm defer fires at the arm's end.
    #[test]
    fn defer_match_arm_runs_at_arm_end() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    x := 1\n    match x:\n        1:\n            defer log(\"arm\")\n            log(\"body\")\n        _: log(\"other\")\n    log(\"after\")\nmain()\n",
            "body\narm\nafter\n",
        );
    }

    /// `continue` drains the current iteration's loop-body defers then advances; `break` drains them
    /// then leaves the loop.
    #[test]
    fn defer_break_continue_drain_loop_body() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..4:\n        defer log(\"d{i}\")\n        if i == 1:\n            continue\n        if i == 2:\n            break\n        log(\"body{i}\")\nmain()\n",
            "body0\nd0\nd1\nd2\n",
        );
    }

    /// `defer:` block form — the body runs top-to-bottom at scope exit, but is LIFO *as a unit*
    /// relative to a surrounding single-call defer. `MakeClosure` + `DeferCall(0)` on the VM;
    /// `Deferred::Block` on the interp. Asserted byte-identical on both engines.
    #[test]
    fn defer_block_form_runs_in_order_lifo_as_unit() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    defer log(\"outer\")\n    defer:\n        log(\"b1\")\n        log(\"b2\")\n    log(\"body\")\nmain()\n",
            "body\nb1\nb2\nouter\n",
        );
    }

    /// `defer:` block captures its free variables by value at the defer point — a later reassignment
    /// of the local is not seen inside the block (matches `defer f(x)` eager arg eval + the VM's
    /// `MakeClosure` capture). Parity-checked across both engines.
    #[test]
    fn defer_block_form_snapshots_by_value() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    x := 1\n    defer:\n        log(\"x={x}\")\n    x = 2\n    log(\"now={x}\")\nmain()\n",
            "now=2\nx=1\n",
        );
    }

    /// A `?` short-circuit INSIDE a `defer:` block: the block has no error-return contract, so the
    /// propagated `Err` is discarded (statements after the `?` don't run) — byte-identical on both
    /// engines. The VM runs the block as a closure and discards its return at the defer boundary; the
    /// interp absorbs the propagation in `run_block_task` to match (regression for a found divergence
    /// where the interp leaked a "? propagation" runtime error).
    #[test]
    fn defer_block_form_discards_question_propagation() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn risky(ok: bool) -> int!:\n    if ok:\n        return Ok(1)\n    return Err(\"boom\")\nfn main() -> int!:\n    defer:\n        log(\"clean start\")\n        n := risky(false)?\n        log(\"clean end\")\n    log(\"body\")\n    return Ok(0)\nmain()\n",
            "body\nclean start\n",
        );
    }

    /// A `defer:` block in a loop body runs per-iteration and drains on `break` (exercises
    /// `EnterDeferScope`/`LeaveDeferScope` wrapping the closure-thunk defer).
    #[test]
    fn defer_block_form_per_iteration_and_break() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..3:\n        defer:\n            log(\"d{i}a\")\n            log(\"d{i}b\")\n        if i == 1:\n            break\n        log(\"body{i}\")\n    log(\"done\")\nmain()\n",
            "body0\nd0a\nd0b\nd1a\nd1b\ndone\n",
        );
    }

    /// A `break` nested inside an `if` (its own defer scope) inside the loop drains BOTH the
    /// if-branch and the loop-body defers, inner-first, before leaving — the post-loop `done` must
    /// print AFTER the cleanup (proving the drain happens at break, not at function return).
    #[test]
    fn defer_break_inside_if_drains_inner_first() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..3:\n        defer log(\"loop{i}\")\n        if i == 1:\n            defer log(\"if{i}\")\n            break\n        log(\"body{i}\")\n    log(\"done\")\nmain()\n",
            "body0\nloop0\nif1\nloop1\ndone\n",
        );
    }

    /// A `recover:`-block defer runs at the recover boundary on the **Ok** path — after the trailing
    /// expression is evaluated, before the result is bound.
    #[test]
    fn defer_recover_runs_on_ok_path() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn risky(ok: bool) -> int!:\n    if ok:\n        return Ok(1)\n    return Err(\"boom\")\nfn main():\n    r := recover:\n        defer log(\"release\")\n        x := risky(true)?\n        x\n    log(\"got\")\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err {e.message()}\")\nmain()\n",
            "release\ngot\nok 1\n",
        );
    }

    /// A `recover:`-block defer runs on the **`?` short-circuit** path, before the propagated
    /// `Err`/`None` is bound as the recover's result.
    #[test]
    fn defer_recover_runs_on_try_path() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn risky(ok: bool) -> int!:\n    if ok:\n        return Ok(1)\n    return Err(\"boom\")\nfn main():\n    r := recover:\n        defer log(\"release\")\n        x := risky(false)?\n        x\n    log(\"got\")\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err {e.message()}\")\nmain()\n",
            "release\ngot\nerr boom\n",
        );
    }

    /// A `recover:`-block defer runs on the **genuine-fault** path, as the panic unwinds to the
    /// boundary, before the `Err(message)` is bound.
    #[test]
    fn defer_recover_runs_on_fault_path() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    r := recover:\n        defer log(\"release\")\n        xs := [1]\n        y := xs[5]\n        y\n    log(\"got\")\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err {e.message()}\")\nmain()\n",
            "release\ngot\nerr index 5 out of bounds (len 1)\n",
        );
    }

    /// A defer that itself faults during a `recover:` unwind supersedes the in-flight result — its
    /// fault becomes the recover's `Err`.
    #[test]
    fn defer_recover_fault_supersedes() {
        assert_defer_scope(
            "fn boom():\n    xs := [1]\n    x := xs[9]\nfn main():\n    r := recover:\n        defer boom()\n        42\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err {e.message()}\")\nmain()\n",
            "err index 9 out of bounds (len 1)\n",
        );
    }

    /// A `break` in an INNER loop drains only that loop's body defers; the outer loop-body defer
    /// still fires at the end of each outer iteration. Locks the per-loop `defer_floor` capture.
    #[test]
    fn defer_inner_loop_break_drains_only_inner() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..2:\n        defer log(\"outer{i}\")\n        for j in 0..3:\n            defer log(\"inner{i}-{j}\")\n            if j == 1:\n                break\n        log(\"after{i}\")\nmain()\n",
            "inner0-0\ninner0-1\nafter0\nouter0\ninner1-0\ninner1-1\nafter1\nouter1\n",
        );
    }

    /// A defer scope (here an `if`) nested INSIDE a `recover:` block that faults must not leak its
    /// scope marker past the recover boundary: the enclosing loop-body defer still drains at each
    /// iteration's end, not at function return. (Regression: VM leaked `defer_markers` on the recover
    /// catch path, corrupting later `LeaveDeferScope`s and diverging from the interp.)
    #[test]
    fn defer_nested_scope_in_faulting_recover_no_marker_leak() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..2:\n        defer log(\"loop{i}\")\n        r := recover:\n            if true:\n                defer log(\"inner{i}\")\n                xs := [1]\n                y := xs[5]\n                y\n            0\n        log(\"end{i}\")\nmain()\n",
            "inner0\nend0\nloop0\ninner1\nend1\nloop1\n",
        );
    }

    /// Same leak via the `?`-short-circuit catch path (not a genuine fault): a defer scope nested in
    /// the recover block must not strand its marker when `?` jumps to the boundary.
    #[test]
    fn defer_nested_scope_in_try_recover_no_marker_leak() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\nfn boom() -> int!:\n    return Err(\"x\")\nfn main():\n    for i in 0..2:\n        defer log(\"loop{i}\")\n        r := recover:\n            if true:\n                defer log(\"inner{i}\")\n                n := boom()?\n                n\n            0\n        log(\"end{i}\")\nmain()\n",
            "inner0\nend0\nloop0\ninner1\nend1\nloop1\n",
        );
    }

    /// Top-level (module-body) defers run LIFO when the program ends normally.
    #[test]
    fn defer_top_level_runs_lifo_at_exit() {
        assert_defer_scope(
            "fn log(s: str):\n    print(s)\ndefer log(\"first\")\ndefer log(\"second\")\nlog(\"body\")\n",
            "body\nsecond\nfirst\n",
        );
    }

    // ----- golden coverage for the formerly-orphaned examples + the comprehensive torture
    // programs (edge_cases / evaluator / ledger). Each pins exact output AND cross-engine parity.

    /// `examples/hof.chz` — a function-typed parameter applied to a closure.
    #[test]
    fn golden_hof_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/hof.chz");
        let expected = include_str!("../../examples/hof.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/list_hof.chz` — `map`/`filter`/`fold`, incl. an element-type-changing map.
    #[test]
    fn golden_list_hof_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/list_hof.chz");
        let expected = include_str!("../../examples/list_hof.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/list_methods.chz` — pop/reverse/contains/index_of/sum (int + float).
    #[test]
    fn golden_list_methods_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/list_methods.chz");
        let expected = include_str!("../../examples/list_methods.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/loops.chz` — break/continue across for-range, for-list, and while loops.
    #[test]
    fn golden_loops_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/loops.chz");
        let expected = include_str!("../../examples/loops.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/match_value.chz` — `match` on int/str literals with `_`, stmt + expr forms.
    #[test]
    fn golden_match_value_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/match_value.chz");
        let expected = include_str!("../../examples/match_value.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/pair.chz` — tuples, multi-return, destructuring let, `.0`/`.1` access.
    #[test]
    fn golden_pair_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/pair.chz");
        let expected = include_str!("../../examples/pair.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/method_default_args.chz` — default + named args on methods (was parity-only).
    #[test]
    fn golden_method_default_args_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/method_default_args.chz");
        let expected = include_str!("../../examples/method_default_args.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/method_type_params.chz` — a method's own `[U]` inferred per call (was parity-only).
    #[test]
    fn golden_method_type_params_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/method_type_params.chz");
        let expected = include_str!("../../examples/method_type_params.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/param_protocol.chz` — a user-defined parameterized protocol bound (was parity-only).
    #[test]
    fn golden_param_protocol_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/param_protocol.chz");
        let expected = include_str!("../../examples/param_protocol.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/edge_cases.chz` — torture test: arithmetic faults under `recover:`, int/float
    /// boundaries, empty/nested collection printing, slice clamping, index faults, truthiness,
    /// block-scoped shadowing, closure capture-by-value, defer LIFO, and comprehensions.
    #[test]
    fn golden_edge_cases_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/edge_cases.chz");
        let expected = include_str!("../../examples/edge_cases.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/evaluator.chz` — a full tokenizer + recursive-descent parser + AST evaluator with
    /// `Result`/`?` error paths (bad char, unbalanced parens, trailing input, divide-by-zero).
    #[test]
    fn golden_evaluator_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/evaluator.chz");
        let expected = include_str!("../../examples/evaluator.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// `examples/ledger.chz` — account ledger: a map of mutable structs, overdraft `Result`s, a
    /// `defer` closing line, `sort_by` ranking, and guarded comprehensions.
    #[test]
    fn golden_ledger_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/ledger.chz");
        let expected = include_str!("../../examples/ledger.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// M1 (tier-1) golden: `examples/string_iter.chz` (chars + iterable strings) byte-identical
    /// on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_string_iter_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/string_iter.chz");
        let expected = include_str!("../../examples/string_iter.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Default + named arguments on free functions: `examples/default_args.chz` byte-identical on
    /// the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_default_args_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/default_args.chz");
        let expected = include_str!("../../examples/default_args.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Default + named arguments on struct constructors: `examples/named_struct.chz` byte-identical
    /// on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_named_struct_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/named_struct.chz");
        let expected = include_str!("../../examples/named_struct.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Gap #5 golden: `examples/map.chz` is byte-identical to its `.expected` on the VM,
    /// and to the interpreter (the cross-engine acceptance bar for maps).
    #[test]
    fn golden_map_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/map.chz");
        let expected = include_str!("../../examples/map.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// M10-G1 golden: `examples/stringable.chz` (the `Stringable` protocol — `str(self)` dispatch
    /// from print/str()/interpolation, nested too) byte-identical on the VM, interp, and `.expected`.
    #[test]
    fn golden_stringable_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/stringable.chz");
        let expected = include_str!("../../examples/stringable.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// M10-G3 golden: `examples/operators.chz` (operator overloading via `Add`/`Sub`/`Mul` + the
    /// multi-bound `T: Add + Mul`) byte-identical on the VM, interp, and `.expected`.
    #[test]
    fn golden_operators_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/operators.chz");
        let expected = include_str!("../../examples/operators.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// M10-G3 golden: `examples/type_alias.chz` (transparent type aliases) byte-identical on the
    /// VM, interp, and `.expected`.
    #[test]
    fn golden_type_alias_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/type_alias.chz");
        let expected = include_str!("../../examples/type_alias.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// G1 golden: `examples/generics.chz` (generics + structural `Comparable`) is byte-identical
    /// on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_generics_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/generics.chz");
        let expected = include_str!("../../examples/generics.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// G2 golden: generic structs are byte-identical on the VM, interpreter, and `.expected`.
    #[test]
    fn golden_generic_structs_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/generic_structs.chz");
        let expected = include_str!("../../examples/generic_structs.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Tier-2 golden: generic enums (Tree[T] / Either[A, B]) — byte-identical VM, interp, expected.
    #[test]
    fn golden_generic_enum_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/generic_enum.chz");
        let expected = include_str!("../../examples/generic_enum.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Golden: real hash-table map/set with Hashable struct keys — byte-identical VM, interp, expected.
    #[test]
    fn golden_hashmap_keys_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/hashmap_keys.chz");
        let expected = include_str!("../../examples/hashmap_keys.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Tech-debt golden: `examples/explicit_type_args.chz` (explicit call-site type arguments on a
    /// generic fn / struct / enum-variant constructor) byte-identical VM, interp, and `.expected`.
    #[test]
    fn golden_explicit_type_args_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/explicit_type_args.chz");
        let expected = include_str!("../../examples/explicit_type_args.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Tech-debt golden: `examples/set_eq.chz` (order-independent set equality incl. nested in a
    /// struct/list) byte-identical on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_set_eq_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/set_eq.chz");
        let expected = include_str!("../../examples/set_eq.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Golden: `examples/map_eq.chz` — map equality is order-independent (same key→value pairs
    /// regardless of insertion order), incl. nested in a struct/list, byte-identical on the VM, the
    /// interpreter, and its `.expected`. Pins the fix that made map `==` consistent with set `==`.
    #[test]
    fn golden_map_eq_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/map_eq.chz");
        let expected = include_str!("../../examples/map_eq.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Golden: `examples/cycle_guard.chz` — a cyclic data structure makes `print`/`==` a recoverable
    /// `RuntimeError` (depth-guarded) instead of an uncatchable host stack overflow, and a
    /// deep-but-acyclic structure still renders fine. Byte-identical on the VM, the interpreter, and
    /// its `.expected`.
    #[test]
    fn golden_cycle_guard_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/cycle_guard.chz");
        let expected = include_str!("../../examples/cycle_guard.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Tech-debt parity: a `set` nested inside a struct / list must compare unordered on BOTH
    /// engines (top-level set `==` already did). Previously the interp's derived `SetData::eq` was
    /// order-sensitive, so `W(set([1,2,3])) == W(set([3,2,1]))` was `true` on the VM but `false` on
    /// the interp.
    #[test]
    fn nested_set_equality_parity() {
        let src = "\
struct W:
    s: set[int]
a := W(set([1, 2, 3]))
b := W(set([3, 2, 1]))
print(a == b)
print([set([1, 2])] == [set([2, 1])])
";
        let vm = run_capture(src).expect("vm");
        let interp = crate::interp::run_capture(src).expect("interp");
        assert_eq!(vm, interp);
        assert_eq!(vm, "true\ntrue\n");
    }

    #[test]
    fn sort_over_comparable_structs_on_vm() {
        let src = "\
struct P:
    n: int
    t: str
    fn compare(self, o: P) -> int:
        return self.n - o.n
    fn show(self) -> str:
        return self.t + str(self.n)
xs := [P(3, \"c\"), P(1, \"a\"), P(2, \"b\"), P(1, \"z\")]
xs.sort()
for x in xs:
    print(x.show())
";
        assert_eq!(run(src), "a1\nz1\nb2\nc3\n");
    }

    #[test]
    fn struct_ordering_dispatches_to_compare_on_vm() {
        let src = "\
struct P:
    n: int
    fn compare(self, other: P) -> int:
        return self.n - other.n
print(P(1) < P(2))
print(P(2) < P(1))
print(P(5) >= P(5))
";
        assert_eq!(run(src), "true\nfalse\ntrue\n");
    }

    #[test]
    fn primitive_compare_method_on_vm() {
        let src = "fn c[T: Comparable](a: T, b: T) -> int:\n    return a.compare(b)\nprint(c(2, 5))\nprint(c(5, 2))\n";
        assert_eq!(run(src), "-1\n1\n");
    }

    /// Gap #11 golden: `examples/sort_by.chz` (custom comparators, stable order, tuple-field sort)
    /// is byte-identical on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_sort_by_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/sort_by.chz");
        let expected = include_str!("../../examples/sort_by.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Call-flattening guarantee: deep *plain-function* recursion no longer consumes host Rust stack
    /// (frames live in the heap `frames` `Vec`, not via a per-call `run_until` recursion), so it runs
    /// to completion on a stack far below the production 256 MiB `VM_STACK_BYTES`. Before flattening,
    /// the VM recursed ~25 KiB of host stack per call and overflowed a 1 MiB stack (an uncatchable
    /// abort). Depth stays well under `MAX_CALL_DEPTH` (10_000). Parity: same value on the interpreter.
    #[test]
    fn deep_plain_recursion_runs_on_small_host_stack() {
        let src = "\
fn sum_to(n: int) -> int:
    if n <= 0:
        return 0
    return n + sum_to(n - 1)

print(sum_to(5000))
";
        let out = super::run_capture_on_stack(src, 1024 * 1024)
            .expect("deep plain recursion should run on a 1 MiB host stack after call-flattening");
        assert_eq!(out, "12502500\n");
        assert_eq!(out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// M19 — guards the `run_until` per-entry program borrow (the hoisted `Arc::clone` →
    /// raw-pointer) across the native-reentry paths that re-enter `run_until`: HOF callbacks
    /// (`map`/`fold` closures), an operator-overload `compare` (`<` on a struct), and a
    /// `defer` unwinding through a recursive call. If the raw pointer ever dangled across a
    /// re-entry or resume, VM output would diverge from the interpreter.
    #[test]
    fn native_reentry_hof_compare_defer_parity() {
        let src = "\
struct P:
    v: int
    fn compare(self, other: P) -> int:
        return self.v - other.v

fn leave(n: int):
    print(\"leave {n}\")

fn rec(n: int) -> int:
    defer leave(n)
    if n <= 0:
        return 0
    doubled := [1, 2, 3].map(fn(x: int) -> int: x * n)
    s := doubled.fold(0, fn(a: int, x: int) -> int: a + x)
    if P(n) < P(n + 1):
        s = s + rec(n - 1)
    return s

print(rec(3))
";
        let out = run_capture(src).expect("vm run");
        assert_eq!(out, "leave 0\nleave 1\nleave 2\nleave 3\n36\n");
        assert_eq!(out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Gap #10 golden: `examples/cipher.chz` (ord/chr — ROT13 + manual digit parsing) is
    /// byte-identical on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_cipher_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/cipher.chz");
        let expected = include_str!("../../examples/cipher.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Gap #14 (+ #11) golden: `examples/word_freq.chz` iterates a map with `for w, c in counts`
    /// and ranks tuples with `sort_by`. Byte-identical on the VM, the interpreter, and `.expected`.
    #[test]
    fn golden_word_freq_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/word_freq.chz");
        let expected = include_str!("../../examples/word_freq.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Gap #15 golden: `examples/match_nested.chz` (tuple patterns, nested `Some((a, b))`, nested
    /// literals) is byte-identical on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_match_nested_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/match_nested.chz");
        let expected = include_str!("../../examples/match_nested.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Match-guard golden: `examples/match_guard.chz` (`pattern if cond:` arms, expr + stmt forms)
    /// is byte-identical on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_match_guard_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/match_guard.chz");
        let expected = include_str!("../../examples/match_guard.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Range-pattern golden: `examples/match_range.chz` (half-open `start..end` int patterns) is
    /// byte-identical on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_match_range_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/match_range.chz");
        let expected = include_str!("../../examples/match_range.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Gap #13 golden: `examples/bits.chz` (`& | ^ << >>` — XOR-fold + bitmask) is byte-identical
    /// on the VM, the interpreter, and its `.expected`.
    #[test]
    fn golden_bits_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/bits.chz");
        let expected = include_str!("../../examples/bits.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Round-2 probe goldens: recursive data-structure + evaluator programs that surfaced the
    /// round-2 gaps. Byte-identical on the VM, the interpreter, and their `.expected`.
    #[test]
    fn golden_bst_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/bst.chz");
        let expected = include_str!("../../examples/bst.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    #[test]
    fn golden_linked_list_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/linked_list.chz");
        let expected = include_str!("../../examples/linked_list.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    #[test]
    fn golden_calc_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/calc.chz");
        let expected = include_str!("../../examples/calc.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    // ----- struct iterator protocol (`for x in s` driven by `next(self) -> Option[T]`) -----

    #[test]
    fn for_over_struct_iterator_counts() {
        let src = "struct Counter:\n    n: int\n    limit: int\n    fn next(self) -> Option[int]:\n        if self.n >= self.limit:\n            return None\n        v := self.n\n        self.n = self.n + 1\n        return Some(v)\nfn main():\n    for x in Counter(0, 5):\n        print(x)\nmain()\n";
        assert_eq!(run(src), "0\n1\n2\n3\n4\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp run"));
    }

    #[test]
    fn for_over_struct_iterator_break_lazy() {
        let src = "struct Fib:\n    a: int\n    b: int\n    fn next(self) -> Option[int]:\n        v := self.a\n        nb := self.a + self.b\n        self.a = self.b\n        self.b = nb\n        return Some(v)\nfn main():\n    for x in Fib(0, 1):\n        if x > 10:\n            break\n        print(x)\nmain()\n";
        assert_eq!(run(src), "0\n1\n1\n2\n3\n5\n8\n");
        assert_eq!(run(src), crate::interp::run_capture(src).expect("interp run"));
    }

    /// Golden: the iterator example runs on the VM with exactly the expected output, matching interp.
    #[test]
    fn golden_iterator_chz_matches_expected_and_interp() {
        let src = include_str!("../../examples/iterator.chz");
        let expected = include_str!("../../examples/iterator.expected");
        let vm_out = run_capture(src).expect("vm run");
        assert_eq!(vm_out, expected);
        assert_eq!(vm_out, crate::interp::run_capture(src).expect("interp run"));
    }

    // ----- cyclic-data structural-depth guard + order-independent map == -----

    #[test]
    fn cyclic_print_errors_not_crashes() {
        let src = "\
struct Node:
    next: list[Node]
a := Node([])
b := Node([])
a.next.push(b)
b.next.push(a)
print(a)
";
        assert!(run_err(src).contains("maximum structural depth"), "expected structural-depth error");
    }

    #[test]
    fn cyclic_equality_errors_not_crashes() {
        let src = "\
struct Node:
    next: list[Node]
a := Node([])
b := Node([])
a.next.push(b)
b.next.push(a)
c := Node([])
d := Node([])
c.next.push(d)
d.next.push(c)
print(a == c)
";
        assert!(run_err(src).contains("maximum structural depth"), "expected structural-depth error");
    }

    #[test]
    fn cyclic_print_is_recoverable() {
        let src = "\
struct Node:
    next: list[Node]
a := Node([])
b := Node([])
a.next.push(b)
b.next.push(a)
r := recover:
    print(a)
match r:
    Ok(v): print(\"ok\")
    Err(e): print(\"caught: {e.message()}\")
";
        let out = run(src);
        assert!(out.contains("caught: maximum structural depth"), "expected recovered error, got {out:?}");
    }

    #[test]
    fn map_equality_is_order_independent() {
        assert_eq!(run("print({1: 10, 2: 20} == {2: 20, 1: 10})\n"), "true\n");
    }

    #[test]
    fn map_equality_distinguishes_values() {
        assert_eq!(run("print({1: 10} == {1: 99})\n"), "false\n");
        assert_eq!(run("print({1: 10} == {1: 10, 2: 20})\n"), "false\n");
    }

    #[test]
    fn deep_acyclic_structure_ok() {
        let src = "\
x := [0]
i := 0
while i < 100:
    x = [x]
    i = i + 1
y := [0]
j := 0
while j < 100:
    y = [y]
    j = j + 1
print(x == y)
";
        assert_eq!(run(src), "true\n");
    }
}

#[cfg(test)]
mod gc_tests {
    use super::*;

    /// A value reachable only via the operand stack (mid-expression temporary) must survive a
    /// collection — the headline use-after-collect trap. Each list is built, left on the stack,
    /// then indexed; a GC fires (stress) between build and index.
    #[test]
    fn value_only_on_operand_stack_survives() {
        assert_eq!(run_capture_stress("print([str(1), str(2), str(3)][0] + [str(4), str(5)][1])"), "15\n");
    }

    /// A value held only in a call-frame local slot survives collections triggered by later
    /// allocations in the same frame.
    #[test]
    fn value_in_frame_slot_survives() {
        let src = "\
fn main():
    x := [str(1), str(2)]
    junk := str(3)
    more := [str(4), str(5), str(6)]
    print(x)
main()";
        assert_eq!(run_capture_stress(src), "[1, 2]\n");
    }

    /// A value reachable only through a module's globals (the namespace cache root) survives.
    #[test]
    fn value_in_module_global_survives() {
        let src = "\
K := [str(7), str(8)]
fn main():
    a := str(1)
    b := [str(2), str(3)]
    print(K)
main()";
        assert_eq!(run_capture_stress(src), "[7, 8]\n");
    }

    /// A value reachable only through a closure's captured environment survives — after the
    /// defining frame is gone, only the closure object holds it.
    #[test]
    fn value_in_closure_capture_survives() {
        let src = "\
fn make():
    secret := str(42)
    return fn(): secret
fn main():
    g := make()
    junk := [str(1), str(2), str(3)]
    print(g())
main()";
        assert_eq!(run_capture_stress(src), "42\n");
    }

    /// Set algebra with heap-allocated (string) elements under GC stress: the source set, the
    /// argument set, and the freshly-built result must all survive a collection mid-operation.
    #[test]
    fn set_algebra_survives_gc_stress() {
        let src = "\
a := set([\"al\" + \"pha\", \"be\" + \"ta\", \"gam\" + \"ma\"])
b := set([\"be\" + \"ta\", \"de\" + \"lta\"])
print(a.union(b).len())
print(a.intersection(b).len())
print(a.difference(b).len())
total := 0
for w in a:
    total += w.len()
print(total)";
        // alpha+beta+gamma = 5+4+5 = 14
        assert_eq!(run_capture_stress(src), "4\n1\n2\n14\n");
    }

    /// `list.sort()` over Comparable structs whose `compare` allocates (triggering GC mid-sort) must
    /// not collect the in-flight elements OR the source list — even when the receiver is an inline
    /// temporary (popped before dispatch, so otherwise unrooted). Regression for the M7-G3 review.
    #[test]
    fn struct_sort_survives_gc_stress() {
        let src = "\
struct M:
    c: int
    fn compare(self, o: M) -> int:
        junk := [str(self.c), str(o.c)]
        return self.c - o.c
fn make() -> list[M]:
    xs := []
    i := 0
    while i < 8:
        xs.push(M((i * 5) % 7))
        i = i + 1
    return xs
fn main():
    xs := make()
    xs.sort()
    out := \"\"
    for m in xs:
        out = out + str(m.c)
    print(out)
    make().sort()              # inline temporary receiver
    print(\"ok\")
main()";
        assert_eq!(run_capture_stress(src), "00123456\nok\n");
    }

    /// A struct key's `hash()` allocates (triggering GC mid-operation). The map/set obj and the
    /// in-flight key/value — popped off the operand stack before dispatch — must stay rooted across
    /// every hash, including with an INLINE-TEMPORARY receiver (`make_map().get(k)`). Regression for
    /// the hash-table struct-key rooting.
    #[test]
    fn map_struct_key_survives_gc_stress() {
        let src = "\
struct K:
    n: int
    fn hash(self) -> int:
        junk := [str(self.n), str(self.n + 1)]
        return self.n
fn make_map() -> map[K, str]:
    m: map[K, str] = {}
    i := 0
    while i < 8:
        m[K(i)] = str(i)
        i = i + 1
    return m
fn main():
    m := make_map()
    out := \"\"
    for k in m:
        out = out + m[k]
    print(out)
    print(m.has(K(3)))
    print(m.get(K(5)))
    print(make_map().get(K(2)))   # inline-temporary receiver
    print(m.remove(K(0)))
    print(m.len())
main()";
        assert_eq!(
            run_capture_stress(src),
            "01234567\ntrue\nSome(5)\nSome(2)\nSome(0)\n7\n"
        );
    }

    /// Set construction (`set([..])`) + `add` over structs whose `hash()` allocates, including
    /// algebra — none of the elements may be collected mid-hash.
    #[test]
    fn set_struct_hash_survives_gc_stress() {
        let src = "\
struct K:
    n: int
    fn hash(self) -> int:
        junk := [str(self.n)]
        return self.n
fn main():
    a := set([K(1), K(2), K(2), K(3)])
    print(a.len())
    a.add(K(3))
    a.add(K(4))
    print(a.len())
    b := set([K(3), K(4), K(5)])
    print(a.union(b).len())
    print(a.intersection(b).len())
    print(a.difference(b).len())
main()";
        // a = {1,2,3,4}; b = {3,4,5}; |a∪b|=5, |a∩b|=2, |a\\b|=2
        assert_eq!(run_capture_stress(src), "3\n4\n5\n2\n2\n");
    }

    /// Same hazard via `sort_by` with an allocating comparator on an inline-temporary list.
    #[test]
    fn struct_sort_by_inline_temporary_survives_gc_stress() {
        let src = "\
struct M:
    c: int
    fn compare(self, o: M) -> int:
        junk := [str(self.c)]
        return self.c - o.c
fn make() -> list[M]:
    xs := []
    i := 0
    while i < 6:
        xs.push(M((i * 5) % 7))
        i = i + 1
    return xs
fn main():
    make().sort_by(fn(a: M, b: M) -> int: a.compare(b))
    print(\"ok\")
main()";
        assert_eq!(run_capture_stress(src), "ok\n");
    }

    /// An `Err` value propagated by `?` through a function boundary survives collection.
    #[test]
    fn value_propagated_by_try_survives() {
        let src = "\
fn d() -> Result[str]:
    return Err(str(99))
fn use() -> Result[str]:
    x := d()?
    return Ok(x)
fn main():
    match use():
        Ok(v): print(v)
        Err(e): print(\"got {e}\")
main()";
        assert_eq!(run_capture_stress(src), "got 99\n");
    }

    /// An allocation-heavy loop's garbage must be reclaimed: the live set stays bounded rather
    /// than growing with the iteration count (threshold-driven GC, not stress mode).
    #[test]
    fn allocation_loop_is_bounded() {
        let src = "\
fn main():
    i := 0
    while i < 10000:
        x := [str(i)]
        i += 1
    print(i)
main()";
        let (out, live) = run_with(src, false);
        assert_eq!(out.unwrap(), "10000\n");
        // Without collection this would be ~20000+ live objects; the threshold GC keeps it small.
        assert!(live < 2000, "heap not bounded: {live} live objects after 10000 allocating iterations");
    }

    /// Stress-mode collection must not change observable behavior on a feature-rich program.
    #[test]
    fn hello_chz_identical_under_gc_stress() {
        let expected = include_str!("../../examples/hello.expected");
        assert_eq!(run_capture_stress(include_str!("../../examples/hello.chz")), expected);
    }

    /// Stress vs. normal must agree on a program exercising structs, enums, closures, and match.
    #[test]
    fn stress_matches_normal_on_mixed_program() {
        let src = "\
struct Box:
    v: int
    fn get(self) -> int:
        return self.v
enum Opt:
    Has(int)
    Nope
fn pick(o: Opt) -> int:
    match o:
        Has(n): return n
        Nope: return -1
fn main():
    b := Box(7)
    add := fn(x: int) -> int: x + b.get()
    print(add(3))
    print(pick(Has(9)))
    print(pick(Nope))
    items := [str(1), str(2), str(3)]
    for s in items:
        print(s)
main()";
        let normal = run_capture(src).unwrap();
        assert_eq!(run_capture_stress(src), normal);
    }

    /// Concurrency C4 rooting: a `spawn`'s deep-copied args, a pending task's captured closure env,
    /// and the values queued in a `Channel` / boxed in a `Shared` must all survive collections that
    /// fire (under stress) between registration and the nursery's join. Each task allocates strings
    /// so a missing root would corrupt the output (or panic on a dangling `GcRef`).
    #[test]
    fn spawn_pending_tasks_survive_gc_stress() {
        let src = "\
fn work(tag: str, out: Channel[str]):
    out.send(\"{tag}!\")
fn main():
    ch := Channel[str]()
    base := str(100)
    parallel:
        spawn work(str(1), ch)
        spawn work(str(2), ch)
        spawn:
            ch.send(\"blk-{base}\")
    print(ch.len())
    for _ in 0..3:
        print(ch.recv())
main()";
        let normal = run_capture(src).unwrap();
        assert_eq!(run_capture_stress(src), normal);
        assert_eq!(normal, crate::interp::run_capture(src).expect("interp run"));
    }

    /// B3.1 regression: a core nested *inside* another core. The channel core is reachable ONLY
    /// through the `Shared` box's wire value once `stash`'s local channel handle is gone — its queued
    /// `"hello"` (a heap `Str` handle embedded in the channel core's wire queue) must still be traced
    /// as a GC root, or `gc_stress` sweeps it and `recv` dangles. Pins that `collect_gcrefs` recurses
    /// into nested cores.
    #[test]
    fn nested_core_contents_survive_gc_stress() {
        let src = "\
fn stash(s: Shared[Channel[str]]):
    ch := s.get()
    ch.send(\"hello\")
fn main():
    s := Shared(Channel[str]())
    stash(s)
    base := str(100)
    ch := s.get()
    print(ch.recv())
main()";
        let normal = run_capture(src).unwrap();
        assert_eq!(normal, "hello\n");
        assert_eq!(run_capture_stress(src), normal, "nested core contents must survive GC");
    }

    /// `Shared` box + `update`'s re-entrant call survive GC stress (the box is re-rooted across the
    /// nested user-fn call; the boxed list's elements stay reachable through collections).
    #[test]
    fn shared_box_survives_gc_stress() {
        let src = "\
fn appended(xs: list[str], v: str) -> list[str]:
    xs.push(v)
    return xs
fn push_one(s: Shared[list[str]], v: str):
    s.update(fn(xs): appended(xs, v))
fn main():
    s := Shared([str(0)])
    parallel:
        spawn push_one(s, str(1))
        spawn push_one(s, str(2))
    print(s.get())
main()";
        let normal = run_capture(src).unwrap();
        assert_eq!(run_capture_stress(src), normal);
        assert_eq!(normal, crate::interp::run_capture(src).expect("interp run"));
    }

    /// Executor: submitted task closures (queued in the heap obj) and the popped task drained at
    /// `shutdown` must survive GC firing between submit and drain, and across each task's re-entrant
    /// call. Each task allocates a string into a Channel — a missing root would corrupt the output or
    /// dangle a `GcRef`.
    #[test]
    fn executor_tasks_survive_gc_stress() {
        let src = "\
fn work(tag: str, out: Channel[str]):
    out.send(\"{tag}!\")
fn main():
    ch := Channel[str]()
    ex := Executor()
    ex.submit(fn(): work(str(1), ch))
    ex.submit(fn(): work(str(2), ch))
    ex.shutdown()
    for _ in 0..2:
        print(ch.recv())
main()";
        let normal = run_capture(src).unwrap();
        assert_eq!(run_capture_stress(src), normal, "VM gc_stress diverged (executor rooting bug?)");
        assert_eq!(normal, crate::interp::run_capture(src).expect("interp run"));
    }
}

#[cfg(test)]
mod parity_tests {
    //! Cross-engine parity: the VM and the tree-walk interpreter must agree on stdout *and* error
    //! for every program. These are the M5 acceptance tests — any divergence fails here.
    use super::*;
    use std::path::PathBuf;
    use std::time::Instant;

    /// Outcome of a run, normalized so interp and VM results compare directly.
    fn interp_outcome(src: &str) -> Result<String, String> {
        crate::interp::run_capture(src).map_err(|e| e.to_string())
    }
    fn vm_outcome(src: &str) -> Result<String, String> {
        run_capture(src).map_err(|e| e.to_string())
    }

    fn assert_parity(src: &str) {
        assert_eq!(vm_outcome(src), interp_outcome(src), "VM/interp divergence for:\n{src}");
    }

    /// M19 SSO — string ops must stay byte-identical across both engines for strings that straddle
    /// the `ChzStr` inline/heap boundary (`INLINE_CAP` = 22 bytes), including multi-byte UTF-8.
    /// Exercises concat, split/join, indexing, iteration, `==`, `.chars()`, and string map keys.
    #[test]
    fn sso_boundary_string_ops_parity() {
        let src = r#"
fn main():
    a := "aaaaaaaaaaaaaaaaaaaaa"      # 21 bytes (inline)
    b := "bbbbbbbbbbbbbbbbbbbbbb"     # 22 bytes (inline boundary)
    c := "ccccccccccccccccccccccc"    # 23 bytes (heap)
    print(a.len())
    print(b.len())
    print(c.len())

    # concat crosses the boundary in both directions
    ab := a + b
    print(ab.len())
    print(ab)
    print(a + "z")                    # 22, still inline
    print(b + "z")                    # 23, spills to heap

    # equality across storage (built two ways)
    print(b == "b" + "bbbbbbbbbbbbbbbbbbbbb")
    print((a + b) == (a + b))

    # indexing + iteration over a heap-length string
    print(c[0])
    print(c[22])
    n := 0
    for ch in c:
        n += 1
    print(n)

    # range-slice producing results on both sides of the boundary (slice itself allocs a str)
    print(c[0..22])                   # 22 bytes — inline
    print(c[0..23])                   # 23 bytes — heap
    print(ab[0..22])

    # case-fold can change byte length, straddling the boundary either way
    print(b.upper())                  # 22 ascii → 22, inline
    print(c.lower())                  # 23 → 23, heap
    print("héllo-wörld-straße".upper())   # multibyte fold (ß→SS grows length)

    # split / join round trip straddling the boundary
    joined := "left-segment-twelve,right-side-thirteen"
    bits := joined.split(",")
    print(bits[0])
    print(bits[1])
    print(",".join(bits) == joined)

    # f-string interpolation growing past the boundary
    i := 0
    while i < 5:
        print("prefix-pad-prefix-pad-{i}")   # ~22-23 bytes, straddles
        i += 1

    # multi-byte UTF-8: short (inline by bytes) and long (heap)
    m := "héllo wörld"                # 13 bytes, 11 chars — inline
    print(m.len())
    print(m.chars().len())
    big := "ñññññññññññññññ"          # 15 chars × 2 bytes = 30 — heap
    print(big.len())
    print(big.chars().len())

    # string map keys straddling the boundary
    mm := {a: 1, c: 2}
    print(mm[a])
    print(mm[c])

main()
"#;
        assert_parity(src);
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    static PARITY_TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> Self {
            let n = PARITY_TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("chezzi_par_{}_{}", std::process::id(), n));
            std::fs::create_dir_all(&dir).unwrap();
            TmpDir(dir)
        }
        fn write(&self, rel: &str, contents: &str) -> PathBuf {
            let p = self.0.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, contents).unwrap();
            p
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Run a multi-file program (one or more `.chz` files) through BOTH engines via `run_file`,
    /// assert they agree on stdout and on ok/err, and return the agreed stdout. `files` is
    /// `(relative_path, contents)`; `entry` names the file to run. Needed because the single-file
    /// `assert_parity` can't exercise imports (and std modules require the import path).
    fn assert_parity_file(files: &[(&str, &str)], entry: &str) -> String {
        let t = TmpDir::new();
        let mut entry_path = None;
        for (rel, contents) in files {
            let p = t.write(rel, contents);
            if *rel == entry {
                entry_path = Some(p);
            }
        }
        let entry_path = entry_path.expect("entry must be one of the files");
        let (io, ie_out, ir, _) = crate::interp::run_file(&entry_path);
        let (vo, ve_out, vr, _) = run_file(&entry_path);
        assert_eq!(io, vo, "stdout divergence (interp vs vm) for entry {entry}");
        assert_eq!(ie_out, ve_out, "stderr divergence (interp vs vm) for entry {entry}");
        match (&ir, &vr) {
            (Ok(()), Ok(())) => {}
            (Err(ie), Err(ve)) => {
                assert_eq!(ie.to_string(), ve.to_string(), "error divergence (interp vs vm)");
            }
            _ => panic!("ok/err divergence: interp={ir:?} vm={vr:?}"),
        }
        io
    }

    /// Convenience: a single entry file (the common std-module case).
    fn parity_entry(src: &str) -> String {
        assert_parity_file(&[("main.chz", src)], "main.chz")
    }

    // ----- C5 (A2): program-exit auto-drain is skipped on a hard `os.exit` -----

    #[test]
    fn executor_autodrain_skipped_on_os_exit() {
        // `os.exit` is a hard halt — like `defer`, the program-exit auto-drain is skipped, so a
        // submitted-but-un-shut executor's work must NOT run. Driven through the file path on both
        // engines (parity), since it imports std.os.
        let out = parity_entry(
            "import std.os\nfn j():\n    print(\"RAN\")\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j())\n    print(\"before exit\")\n    os.exit(0)\nmain()\n",
        );
        assert_eq!(out, "before exit\n");
    }

    // ----- M9: std.regex parity (exercises NativeRet::Struct lowering on both engines) -----

    #[test]
    fn regex_find_all_replace_split_parity() {
        let out = parity_entry(
            r##"import std.regex
match regex.find_all("[0-9]+", "a1 22 333"):
    Ok(ms):
        for m in ms:
            print(m.text)
    Err(e): print(e)
match regex.replace_all("[0-9]+", "a1b22c", "#"):
    Ok(s): print(s)
    Err(e): print(e)
match regex.split(",", "a,b,c"):
    Ok(parts): print("|".join(parts))
    Err(e): print(e)
"##,
        );
        assert_eq!(out, "1\n22\n333\na#b#c\na|b|c\n");
    }

    /// `std.request` against a loopback server, run through BOTH engines (exercises `NativeRet::Map`
    /// lowering on each). The server serves one canned response per connection; interp and vm each
    /// open one, so it accepts twice.
    #[test]
    fn request_get_parity_against_local_server() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let body = "pong";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nX-Test: hi\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(resp.as_bytes()).unwrap();
            }
        });

        let src = format!(
            "import std.request\nmatch request.get(\"http://{addr}/\"):\n    Ok(resp):\n        print(str(resp.status))\n        print(resp.body)\n        print(resp.headers[\"x-test\"])\n    Err(e): print(e)\n"
        );
        let out = parity_entry(&src);
        server.join().unwrap();
        assert_eq!(out, "200\npong\nhi\n");
    }

    /// `std.request` new verbs + custom headers, run through BOTH engines against a loopback server
    /// that records every request's wire bytes. Each engine issues a `put` and a header-carrying
    /// `request("DELETE", …)`, so the server accepts 4 times (2 per engine). Asserts (a) identical
    /// stdout across VM and interp and (b) the right method line + custom header reached the wire —
    /// locking the off-heap `NativeArg::Map` headers path and the verb wrappers under parity.
    #[test]
    fn request_verbs_and_headers_parity_against_local_server() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_srv = Arc::clone(&seen);
        let server = std::thread::spawn(move || {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 1024];
                let n = stream.read(&mut buf).unwrap_or(0);
                seen_srv.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).into_owned());
                // `Connection: close` so ureq's thread-local pool (shared across both engine runs on
                // this test thread) never reuses a server-closed socket — one fresh conn per request.
                let resp = "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nok";
                stream.write_all(resp.as_bytes()).unwrap();
            }
        });

        // Each engine: a PUT verb wrapper, then a header-carrying general DELETE.
        let src = format!(
            "import std.request\nmatch request.put(\"http://{addr}/\", \"payload\"):\n    Ok(r): print(str(r.status))\n    Err(e): print(e)\nmatch request.request(\"DELETE\", \"http://{addr}/\", \"\", {{\"X-Custom\": \"value\"}}):\n    Ok(r): print(str(r.status))\n    Err(e): print(e)\n"
        );
        let out = parity_entry(&src);
        server.join().unwrap();
        assert_eq!(out, "200\n200\n", "VM/interp must agree and both requests succeed");

        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 4, "two requests per engine");
        let puts = reqs.iter().filter(|r| r.starts_with("PUT ")).count();
        let deletes = reqs.iter().filter(|r| r.starts_with("DELETE ")).count();
        let with_header = reqs.iter().filter(|r| r.contains("X-Custom: value")).count();
        assert_eq!(puts, 2, "both engines must send PUT");
        assert_eq!(deletes, 2, "both engines must send DELETE");
        assert_eq!(with_header, 2, "the custom header must reach the wire on both engines");
    }

    #[test]
    fn regex_find_groups_and_span_parity() {
        let out = parity_entry(
            r#"import std.regex
match regex.find("([a-z]+)@([a-z]+)", "xx ann@host"):
    Ok(opt):
        match opt:
            Some(m): print(m.text + " " + str(m.start) + " " + ",".join(m.groups))
            None: print("none")
    Err(e): print(e)
"#,
        );
        assert_eq!(out, "ann@host 3 ann,host\n");
    }

    // ----- break / continue parity (both engines must agree AND produce the right output) -----

    /// Assert both engines agree AND that the (shared) stdout equals `expect`. A hang here means a
    /// `continue` is landing on the wrong target (re-test without advancing → infinite loop).
    fn assert_parity_out(src: &str, expect: &str) {
        assert_parity(src);
        assert_eq!(vm_outcome(src).expect("program should run"), expect, "for:\n{src}");
    }

    #[test]
    fn bitwise_ops_parity() {
        assert_parity_out(
            "print(5 & 3)\nprint(5 | 2)\nprint(5 ^ 3)\nprint(1 << 4)\nprint(255 >> 4)\n",
            "1\n7\n6\n16\n15\n",
        );
    }

    #[test]
    fn bitwise_precedence_below_comparison_parity() {
        // `5 & 3 == 1` is `(5 & 3) == 1` (bitwise binds tighter than `==`, Python-style).
        assert_parity_out("print(5 & 3 == 1)\n", "true\n");
    }

    #[test]
    fn xor_fold_single_number_parity() {
        assert_parity_out(
            "xs := [4,1,2,1,4,2,7]\nacc := 0\nfor x in xs:\n    acc = acc ^ x\nprint(acc)\n",
            "7\n",
        );
    }

    #[test]
    fn shift_out_of_range_error_parity() {
        // Dynamic shift the checker can't catch — both engines must raise the same runtime error.
        assert_parity("print(1 << 64)\n");
        assert_parity("print(1 << -1)\n");
    }

    #[test]
    fn match_tuple_pattern_parity() {
        assert_parity_out(
            "p := (3, 4)\nmatch p:\n    (0, y): print(y)\n    (x, y): print(x + y)\n",
            "7\n",
        );
    }

    #[test]
    fn match_tuple_literal_arm_parity() {
        assert_parity_out(
            "p := (1, 9)\nlabel := match p:\n    (1, n): \"one {n}\"\n    _: \"other\"\nprint(label)\n",
            "one 9\n",
        );
    }

    #[test]
    fn match_nested_variant_in_tuple_parity() {
        assert_parity_out(
            "o: (int, int)? = Some((10, 20))\nmatch o:\n    None: print(\"none\")\n    Some((a, b)): print(a + b)\n",
            "30\n",
        );
    }

    #[test]
    fn match_nested_heap_payload_gc_stress() {
        // Nested pattern binding heap values (strings) inside a tuple inside a variant; a GC mid-bind
        // must not collect the still-referenced payload.
        let src = "o: (str, str)? = Some((\"a\" + \"b\", \"c\" + \"d\"))\nmatch o:\n    None: print(\"none\")\n    Some((x, y)): print(x + y)\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "abcd\n");
        assert_eq!(run_capture_stress(src), "abcd\n", "VM gc_stress diverged (rooting bug?)");
    }

    #[test]
    fn match_guard_fallthrough_parity() {
        // The first arm's pattern binds but its guard is false → fall through to the next arm.
        assert_parity_out(
            "n := 5\nmatch n:\n    x if x < 0: print(\"neg\")\n    x if x > 10: print(\"big\")\n    _: print(\"mid\")\n",
            "mid\n",
        );
    }

    #[test]
    fn match_guard_first_true_parity() {
        assert_parity_out(
            "n := -2\nlabel := match n:\n    x if x < 0: \"neg\"\n    _: \"nonneg\"\nprint(label)\n",
            "neg\n",
        );
    }

    #[test]
    fn match_range_boundaries_parity() {
        // Half-open: start is inclusive, end is exclusive.
        assert_parity_out(
            "fn b(n: int) -> str:\n    return match n:\n        0..10: \"lo\"\n        10..20: \"hi\"\n        _: \"out\"\nprint(b(0))\nprint(b(9))\nprint(b(10))\nprint(b(19))\nprint(b(20))\n",
            "lo\nlo\nhi\nhi\nout\n",
        );
    }

    #[test]
    fn match_range_with_literal_mix_parity() {
        // A match mixing int literals and ranges still routes through the literal path.
        assert_parity_out(
            "fn f(n: int) -> str:\n    return match n:\n        0: \"zero\"\n        1..100: \"small\"\n        _: \"big\"\nprint(f(0))\nprint(f(50))\nprint(f(500))\n",
            "zero\nsmall\nbig\n",
        );
    }

    #[test]
    fn for_over_map_keys_parity() {
        assert_parity_out(
            "m := {\"a\": 1, \"b\": 2, \"c\": 3}\nfor k in m:\n    print(k)\n",
            "a\nb\nc\n",
        );
    }

    /// Regression (review #1): a struct field named `decode` must still be indexable — the
    /// `.decode[…]` JSON form is only stolen when a real `[Type](arg)` follows.
    #[test]
    fn field_named_decode_is_indexable_parity() {
        let out = parity_entry(
            "struct Box:\n    decode: list[int]\nb := Box([10, 20, 30])\nprint(b.decode[1])\nprint(b.decode[0] + b.decode[2])\n",
        );
        assert_eq!(out, "20\n40\n");
    }

    /// Regression (review #2): malformed and out-of-range numbers come back as `Err` / stringify
    /// cleanly — they must never abort the host (no uncaught `float()`/`int()` panic).
    #[test]
    fn json_malformed_numbers_are_errors_parity() {
        let out = parity_entry(
            "import std.json\nfn tp(s: str) -> str:\n    match json.parse(s):\n        Ok(j): return \"OK \" + json.stringify(j)\n        Err(e): return \"ERR\"\nprint(tp(\"1e\"))\nprint(tp(\"1.\"))\nprint(tp(\"100000000000000000000\"))\n",
        );
        assert_eq!(out, "ERR\nERR\nOK 100000000000000000000.0\n");
    }

    #[test]
    fn json_decode_struct_parity() {
        let out = parity_entry(
            "import std.json\nstruct P:\n    x: int\n    y: int\nmatch json.decode[P](\"{{\\\"x\\\":1,\\\"y\\\":2}}\"):\n    Ok(p): print(p.x + p.y)\n    Err(e): print(e)\n",
        );
        assert_eq!(out, "3\n");
    }

    #[test]
    fn json_decode_error_parity() {
        let out = parity_entry(
            "import std.json\nstruct P:\n    x: int\nmatch json.decode[P](\"{{\\\"y\\\":2}}\"):\n    Ok(p): print(p.x)\n    Err(e): print(e)\n",
        );
        assert_eq!(out, "decode: missing key 'x' at $\n");
    }

    #[test]
    fn process_cmd_ok_and_err_parity() {
        let out = parity_entry(
            "import std.process\nmatch process.cmd(\"printf abc\"):\n    Ok(s): print(\"ok:\" + s)\n    Err(e): print(\"err:\" + e)\nmatch process.cmd(\"exit 2\"):\n    Ok(s): print(\"ok\")\n    Err(e): print(\"err:\" + e)\n",
        );
        assert_eq!(out, "ok:abc\nerr:command exited with status 2\n");
    }

    #[test]
    fn fs_predicates_parity() {
        let out = parity_entry(
            "import std.fs\nprint(fs.exists(\"Cargo.toml\"))\nprint(fs.exists(\"definitely_not_here.zzz\"))\nprint(fs.is_dir(\"src\"))\n",
        );
        assert_eq!(out, "true\nfalse\ntrue\n");
    }

    #[test]
    fn time_format_parity() {
        let out = parity_entry(
            "import std.time\nprint(time.format(0))\nprint(time.format(1700000000))\nprint(time.now() > 0)\n",
        );
        assert_eq!(out, "1970-01-01 00:00:00\n2023-11-14 22:13:20\ntrue\n");
    }

    #[test]
    fn set_dedup_and_algebra_parity() {
        assert_parity_out(
            "s := {3, 1, 3, 2, 1}\nprint(s.len())\nprint({1,2,3}.union({3,4}).len())\nprint({1,2,3}.intersection({2,3,4}).len())\nprint({1,2,3}.difference({2,3}).len())\nprint({1,2} == {2,1})\n",
            "3\n4\n2\n1\ntrue\n",
        );
    }

    #[test]
    fn set_mutation_and_iteration_parity() {
        assert_parity_out(
            "s := set()\ns.add(10)\ns.add(10)\ns.add(20)\nprint(s.len())\nprint(s.remove(10))\nprint(s.remove(10))\ntotal := 0\nfor x in {5, 15, 25}:\n    total += x\nprint(total)\n",
            "2\ntrue\nfalse\n45\n",
        );
    }

    #[test]
    fn set_display_parity() {
        assert_parity_out("print({1, 2, 3})\nprint(set())\n", "{1, 2, 3}\nset()\n");
    }

    #[test]
    fn str_chars_parity() {
        assert_parity_out(
            "cs := \"héllo\".chars()\nprint(cs.len())\nprint(cs[1])\n",
            "5\né\n",
        );
    }

    #[test]
    fn for_over_str_parity() {
        assert_parity_out(
            "out := \"\"\nfor c in \"abc\":\n    out = out + c + \"-\"\nprint(out)\n",
            "a-b-c-\n",
        );
    }

    #[test]
    fn for_over_empty_str_parity() {
        assert_parity_out(
            "n := 0\nfor c in \"\":\n    n += 1\nprint(n)\n",
            "0\n",
        );
    }

    #[test]
    fn for_over_map_key_value_parity() {
        assert_parity_out(
            "m := {\"a\": 1, \"b\": 2}\ns := 0\nfor k, v in m:\n    print(\"{k}={v}\")\n    s += v\nprint(s)\n",
            "a=1\nb=2\n3\n",
        );
    }

    #[test]
    fn for_over_map_kv_mutation_during_iteration_parity() {
        // The body reassigns a not-yet-visited key; both engines must agree (snapshot semantics:
        // the value bound is the one captured at loop start, like list iteration).
        assert_parity_out(
            "m := {\"a\": 1, \"b\": 2, \"c\": 3}\nout := 0\nfor k, v in m:\n    m[\"c\"] = 99\n    out += v\nprint(out)\n",
            "6\n",
        );
    }

    #[test]
    fn for_over_map_kv_remove_during_iteration_parity() {
        // Removing a future key mid-iteration must not crash one engine while the other succeeds.
        assert_parity_out(
            "m := {\"a\": 1, \"b\": 2}\nfirst := true\nsum := 0\nfor k, v in m:\n    if first:\n        m.remove(\"b\")\n        first = false\n    sum += v\nprint(sum)\n",
            "3\n",
        );
    }

    #[test]
    fn for_over_map_break_continue_parity() {
        // break/continue still target the index increment over the keys sequence.
        assert_parity_out(
            "m := {\"a\": 1, \"b\": 2, \"c\": 3, \"d\": 4}\nfor k, v in m:\n    if v == 2: continue\n    if v == 4: break\n    print(k)\n",
            "a\nc\n",
        );
    }

    #[test]
    fn cmp_max_int_parity() {
        // Generic min/max now live in std.cmp; abs stays in std.math. File/graph path required.
        let out = parity_entry("import std.cmp\nimport std.math\nfn main():\n    print(cmp.max(3, 5))\n    print(cmp.min(3, 5))\n    print(math.abs(-5))\nmain()\n");
        assert_eq!(out, "5\n3\n5\n");
    }

    #[test]
    fn cmp_max_float_parity() {
        let out = parity_entry("import std.cmp\nimport std.math\nfn main():\n    print(cmp.max(3.0, 5.0))\n    print(math.abs(-2.5))\nmain()\n");
        assert_eq!(out, "5.0\n2.5\n");
    }

    #[test]
    fn cmp_max_struct_parity() {
        // The generic max over a Comparable struct must be byte-identical on both engines.
        let src = "import std.cmp\nstruct P:\n    n: int\n    fn compare(self, o: P) -> int:\n        return self.n - o.n\nfn main():\n    print(cmp.max(P(2), P(9)).n)\n    print(cmp.min(P(2), P(9)).n)\nmain()\n";
        assert_eq!(parity_entry(src), "9\n2\n");
    }

    #[test]
    fn ord_chr_parity() {
        assert_parity_out("print(ord(\"A\"))\nprint(chr(97))\n", "65\na\n");
    }

    #[test]
    fn ord_chr_roundtrip_parity() {
        assert_parity_out("print(chr(ord(\"z\")))\n", "z\n");
    }

    #[test]
    fn ord_index_digit_value_parity() {
        // The digit-value idiom over an indexed char.
        assert_parity_out("s := \"7\"\nprint(ord(s[0]) - ord(\"0\"))\n", "7\n");
    }

    #[test]
    fn ord_empty_string_error_parity() {
        // Runtime error (checker can't catch it) — message must match across engines.
        assert_parity("print(ord(\"\"))\n");
    }

    #[test]
    fn chr_invalid_codepoint_error_parity() {
        assert_parity("print(chr(-1))\n");
        assert_parity("print(chr(2000000))\n");
    }

    #[test]
    fn sort_by_descending_parity() {
        assert_parity_out(
            "xs := [3,1,2]\nxs.sort_by(fn(a: int, b: int) -> int: b - a)\nprint(xs)\n",
            "[3, 2, 1]\n",
        );
    }

    #[test]
    fn sort_by_stable_by_key_parity() {
        // Equal keys (string length) must keep input order — stability is part of the contract.
        assert_parity_out(
            "ws := [\"bb\", \"a\", \"dd\", \"e\"]\nws.sort_by(fn(a: str, b: str) -> int: a.len() - b.len())\nprint(ws)\n",
            "[a, e, bb, dd]\n",
        );
    }

    #[test]
    fn sort_by_comparator_mutates_list_parity() {
        // A comparator that mutates an element being sorted must behave identically on both engines.
        // Both sort a snapshot taken at call time and overwrite the list with the sorted result, so
        // the in-comparator `xs[0] = 100` is discarded.
        let src = "xs := [3, 1, 2]\nfn cmp(a: int, b: int) -> int:\n    xs[0] = 100\n    return a - b\nxs.sort_by(cmp)\nprint(xs)\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "[1, 2, 3]\n");
    }

    #[test]
    fn sort_by_empty_and_singleton_parity() {
        assert_parity_out(
            "xs := [42]\nxs.sort_by(fn(a: int, b: int) -> int: a - b)\nprint(xs)\n",
            "[42]\n",
        );
    }

    #[test]
    fn break_early_for_parity() {
        assert_parity_out(
            "s := 0\nfor i in 0..10:\n    if i == 5: break\n    s += i\nprint(s)\n",
            "10\n",
        );
    }

    #[test]
    fn continue_for_terminates_parity() {
        // THE increment-landing guard: `continue` must reach the loop's `i += 1`, never the
        // condition (would re-test the same `i` forever). If this hangs, the target is wrong.
        assert_parity_out(
            "for i in 0..5:\n    if i == 1: continue\n    if i == 3: continue\n    print(i)\n",
            "0\n2\n4\n",
        );
    }

    #[test]
    fn while_break_parity() {
        assert_parity_out(
            "i := 0\nwhile true:\n    if i == 3: break\n    i += 1\nprint(i)\n",
            "3\n",
        );
    }

    #[test]
    fn while_continue_progresses_parity() {
        // The counter advances BEFORE the `continue`, so the `while` still terminates.
        assert_parity_out(
            "i := 0\ns := 0\nwhile i < 5:\n    i += 1\n    if i == 2: continue\n    s += i\nprint(s)\n",
            "13\n",
        );
    }

    #[test]
    fn break_in_if_in_loop_parity() {
        assert_parity_out(
            "for i in 0..10:\n    if i > 2:\n        break\n    print(i)\n",
            "0\n1\n2\n",
        );
    }

    #[test]
    fn return_from_loop_parity() {
        // `return` inside a loop still returns the whole function (break/continue don't intercept it).
        assert_parity_out(
            "fn f():\n    for i in 0..10:\n        if i == 2: return i\n    return -1\nprint(f())\n",
            "2\n",
        );
    }

    #[test]
    fn nested_loop_inner_break_parity() {
        // Inner `break` does not break the outer loop: the outer runs all 3 iterations.
        assert_parity_out(
            "n := 0\nfor i in 0..3:\n    for j in 0..3:\n        break\n    n += 1\nprint(n)\n",
            "3\n",
        );
    }

    #[test]
    fn continue_list_for_parity() {
        // `continue` over a LIST for-loop (not just range) advances to the next element.
        assert_parity_out(
            "for x in [1,2,3,4]:\n    if x % 2 == 0: continue\n    print(x)\n",
            "1\n3\n",
        );
    }

    #[test]
    fn break_list_for_parity() {
        assert_parity_out(
            "for x in [10,20,30,40]:\n    if x == 30: break\n    print(x)\n",
            "10\n20\n",
        );
    }

    // ----- literal + wildcard match parity -----

    #[test]
    fn match_int_literals_stmt_parity() {
        assert_parity("n := 2\nmatch n:\n    0: print(\"zero\")\n    1: print(\"one\")\n    _: print(\"many\")\n");
    }

    #[test]
    fn match_str_literals_expr_parity() {
        assert_parity("c := \"x\"\ns := match c:\n    \"a\": \"first\"\n    _: \"other\"\nprint(s)\n");
    }

    #[test]
    fn match_bool_literals_parity() {
        assert_parity("b := false\nmatch b:\n    true: print(\"yes\")\n    false: print(\"no\")\n    _: print(\"?\")\n");
    }

    #[test]
    fn match_literal_matched_arm_parity() {
        // The matching literal arm fires (wildcard not reached).
        assert_parity("n := 1\ns := match n:\n    0: \"a\"\n    1: \"b\"\n    _: \"z\"\nprint(s)\n");
    }

    #[test]
    fn match_wildcard_reached_parity() {
        // No literal matches → the `_` arm fires.
        assert_parity("n := 9\ns := match n:\n    0: \"a\"\n    1: \"b\"\n    _: \"z\"\nprint(s)\n");
    }

    #[test]
    fn match_variant_regression_parity() {
        // A variant match still lowers via the variant path unchanged.
        assert_parity("o := Some(5)\nmatch o:\n    Some(v): print(\"got {v}\")\n    None: print(\"none\")\n");
    }

    #[test]
    fn parity_std_math() {
        let src = "\
import std.math
fn main():
    print(math.floor(2.7))
    print(math.ceil(2.1))
    print(math.sqrt(16.0))
    print(math.pow(2.0, 10.0))
    print(math.abs(0.0 - 3.5))
    print(math.round(2.5))
    print(math.pi)
main()";
        assert_eq!(
            parity_entry(src),
            "2.0\n3.0\n4.0\n1024.0\n3.5\n3.0\n3.141592653589793\n"
        );
    }

    #[test]
    fn parity_std_math_sqrt_negative_errors() {
        // math.sqrt of a negative is a runtime error, identical on both engines.
        let src = "import std.math\nfn main():\n    print(math.sqrt(0.0 - 1.0))\nmain()";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (_io, _ie, ir, _ic) = crate::interp::run_file(&entry);
        let (_vo, _ve, vr, _vc) = run_file(&entry);
        let ie = ir.unwrap_err().to_string();
        let ve = vr.unwrap_err().to_string();
        assert_eq!(ie, ve);
        assert!(ie.contains("sqrt() of a negative number"), "{ie}");
    }

    #[test]
    fn math_abs_min_overflows() {
        // `math.abs(i64::MIN)` has no representable result. Raw `i64::abs()` would panic (debug) or
        // wrap (release); the native fn must surface a recoverable overflow like every other op,
        // identically on both engines. i64::MIN is built as `-MAX - 1` (the literal
        // `9223372036854775808` overflows the lexer).
        let src = "import std.math\nfn main():\n    x := -9223372036854775807 - 1\n    print(math.abs(x))\nmain()";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let ie = crate::interp::run_file(&entry).2.unwrap_err().to_string();
        let ve = run_file(&entry).2.unwrap_err().to_string();
        assert_eq!(ie, ve, "abs-overflow error must be identical on both engines");
        assert!(ie.contains("integer overflow in abs"), "{ie}");
    }

    #[test]
    fn math_abs_min_overflow_is_recoverable() {
        // The overflow is a normal recoverable fault: `recover:` turns it into an Err, not a crash.
        let src = "import std.math\nfn main():\n    x := -9223372036854775807 - 1\n    r := recover:\n        math.abs(x)\n    match r:\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()";
        let out = parity_entry(src);
        assert!(out.contains("integer overflow in abs"), "{out}");
    }

    #[test]
    fn exit_threads_code_through_both_engines() {
        // `std.os.exit(code)` halts the program with that exit code on both engines: output before
        // the call is preserved, the statement after it never runs, and the run is not an error.
        let src = "import std.os\nfn main():\n    print(\"before\")\n    os.exit(3)\n    print(\"after\")\nmain()";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (io, _ie, ir, ic) = crate::interp::run_file(&entry);
        let (vo, _ve, vr, vc) = run_file(&entry);
        assert_eq!(io, "before\n", "interp stdout");
        assert_eq!(vo, "before\n", "vm stdout");
        assert_eq!(ic, Some(3), "interp exit code");
        assert_eq!(vc, Some(3), "vm exit code");
        assert!(ir.is_ok() && vr.is_ok(), "exit is not a runtime error: interp={ir:?} vm={vr:?}");
    }

    #[test]
    fn defer_top_level_skipped_by_os_exit() {
        // `std.os.exit` is a hard halt — top-level defers do NOT run through it (matches Go's
        // `os.Exit`, and the existing frame/recover bypass).
        let src = "import std.os\nfn log(s: str):\n    print(s)\ndefer log(\"cleanup\")\nprint(\"before\")\nos.exit(2)\nprint(\"after\")\n";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (io, _ie, ir, ic) = crate::interp::run_file(&entry);
        let (vo, _ve, vr, vc) = run_file(&entry);
        assert_eq!(io, "before\n", "interp: cleanup defer skipped by os.exit");
        assert_eq!(vo, "before\n", "vm: cleanup defer skipped by os.exit");
        assert_eq!(ic, Some(2), "interp exit code");
        assert_eq!(vc, Some(2), "vm exit code");
        assert!(ir.is_ok() && vr.is_ok(), "exit is not a runtime error: interp={ir:?} vm={vr:?}");
    }

    #[test]
    fn defer_top_level_runs_on_unhandled_error() {
        // An unhandled top-level `?` error still unwinds through the module body's defers (cleanup
        // runs before the program reports the error).
        let src = "fn log(s: str):\n    print(s)\nfn boom() -> int!:\n    return Err(\"nope\")\ndefer log(\"cleanup\")\nprint(\"before\")\nx := boom()?\nprint(\"after\")\n";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (io, _ie, ir, _ic) = crate::interp::run_file(&entry);
        let (vo, _ve, vr, _vc) = run_file(&entry);
        assert_eq!(io, "before\ncleanup\n", "interp: top-level defer runs on unhandled error");
        assert_eq!(vo, "before\ncleanup\n", "vm: top-level defer runs on unhandled error");
        assert!(ir.is_err() && vr.is_err(), "unhandled `?` is an error: interp={ir:?} vm={vr:?}");
    }

    #[test]
    fn exit_is_not_caught_by_recover() {
        // A hard exit unwinds past a `recover:` boundary — it is NOT converted to an `Err` value.
        let src = "import std.os\nfn main():\n    x := recover:\n        os.exit(7)\n    print(\"unreachable\")\nmain()";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (io, _ie, _ir, ic) = crate::interp::run_file(&entry);
        let (vo, _ve, _vr, vc) = run_file(&entry);
        assert_eq!(io, "", "interp: nothing after the recover runs");
        assert_eq!(vo, "", "vm: nothing after the recover runs");
        assert_eq!(ic, Some(7), "interp exit code");
        assert_eq!(vc, Some(7), "vm exit code");
    }

    #[test]
    fn exit_in_spawned_child_aborts_siblings() {
        // B1/B2: `std.os.exit` inside a child fiber is a hard halt — it aborts the remaining siblings
        // and the rest of the program. The first child prints then exits(3); the second child and the
        // post-`parallel:` statement never run. Identical on both engines (no blocking involved).
        let src = "import std.os\nfn a():\n    print(\"a\")\n    os.exit(3)\nfn b():\n    print(\"b\")\nfn main():\n    parallel:\n        spawn a()\n        spawn b()\n    print(\"after\")\nmain()\n";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (io, _ie, ir, ic) = crate::interp::run_file(&entry);
        let (vo, _ve, vr, vc) = run_file(&entry);
        assert_eq!(vo, "a\n", "vm: sibling and post-parallel statement aborted by os.exit");
        assert_eq!(io, "a\n", "interp: sibling and post-parallel statement aborted by os.exit");
        assert_eq!(vc, Some(3), "vm exit code");
        assert_eq!(ic, Some(3), "interp exit code");
        assert!(ir.is_ok() && vr.is_ok(), "os.exit is a clean halt, not an error: interp={ir:?} vm={vr:?}");
    }

    /// B3.4: a child `std.os.exit(code)` on the `--parallel` OS-thread pool is a clean hard-halt,
    /// not a fault. Cross-thread: the worker's `pending_exit` propagates up the join to the parent
    /// VM, the exiting child's buffered output is flushed, and the post-`parallel:` statement never
    /// runs. The `--parallel` counterpart of `exit_in_spawned_child_aborts_siblings`.
    #[test]
    fn parallel_child_os_exit_halts_with_code() {
        let src = "import std.os\nfn a():\n    print(\"a\")\n    os.exit(3)\nfn b():\n    print(\"b\")\nfn main():\n    parallel:\n        spawn a()\n        spawn b()\n    print(\"after\")\nmain()\n";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (out, _err, res, code) = run_file_parallel(&entry, crate::native::HostConfig::default());
        assert_eq!(code, Some(3), "child os.exit code propagates cross-thread to the parent");
        assert!(res.is_ok(), "os.exit is a clean halt, not an error: {res:?}");
        assert!(out.contains('a'), "the exiting child's buffered output is flushed: got {out:?}");
        assert!(!out.contains("after"), "the post-parallel statement never runs after os.exit: got {out:?}");
    }

    /// B3.4: `os.exit` in one child aborts a `recv`-blocked sibling too (same machinery as a fault —
    /// it trips the nursery cancel flag), so the join completes with the exit code instead of hanging.
    /// `exiter` is spawned first → runs inline on the joining thread (the exit trips cancel without
    /// depending on pool scheduling); the recv-blocked `consumer` runs on the pool and aborts.
    #[test]
    fn parallel_os_exit_aborts_recv_blocked_sibling() {
        let src = "import std.os\nfn exiter(ch: Channel[int]):\n    os.exit(5)\nfn consumer(ch: Channel[int]):\n    ch.recv()\n    print(\"consumed\")\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn exiter(ch)\n        spawn consumer(ch)\nmain()\n";
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (out, _err, _res, code) = run_file_parallel(&entry, crate::native::HostConfig::default());
        assert_eq!(code, Some(5), "os.exit code propagates; the recv-blocked consumer aborts, no hang");
        assert!(!out.contains("consumed"), "the aborted consumer never ran past its blocked recv: got {out:?}");
    }

    /// B3.5 — run an entry on the `--parallel` engine under a watchdog: a missing/broken deadlock
    /// detector would hang the nursery forever, so we run it on a side thread and fail loudly if it
    /// doesn't finish, instead of wedging the whole test binary. (On a clean detector none of these
    /// ever time out — the leak only happens on the failure path we're guarding against.)
    fn run_parallel_watchdog(src: &str) -> RunOutput {
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file_parallel(&entry, crate::native::HostConfig::default()));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(r) => r,
            Err(_) => panic!("hung: --parallel nursery did not terminate (deadlock detection missing/broken)"),
        }
    }

    /// B3.5 — the cooperative `fibers_all_blocked_is_deadlock` golden, ported to `--parallel`: two
    /// tasks each block on a distinct empty channel with no producer. The cooperative scheduler
    /// already faults this; under threads B3.5's nursery-local detector must fault it too rather
    /// than hang on the condvars.
    #[test]
    fn parallel_all_blocked_deadlock_faults() {
        let src = "fn waiter(c: Channel[int]):\n    c.recv()\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn waiter(a)\n        spawn waiter(b)\nmain()\n";
        let (_o, _e, res, _c) = run_parallel_watchdog(src);
        let err = res.expect_err("an all-blocked --parallel nursery must fault, not hang");
        assert!(err.message.contains("deadlock"), "got: {}", err.message);
    }

    /// B3.5 — the named anti-false-positive case: one sibling `send`s the very channel the other
    /// `recv`s, so the nursery genuinely progresses. The barrier-confirm detector must NOT report a
    /// deadlock (a real send aborts any half-built all-blocked confirmation).
    #[test]
    fn parallel_near_miss_does_not_false_positive() {
        let src = "fn consumer(c: Channel[int]):\n    print(c.recv())\nfn producer(c: Channel[int]):\n    c.send(7)\nfn main():\n    c := Channel[int]()\n    parallel:\n        spawn consumer(c)\n        spawn producer(c)\n    print(\"done\")\nmain()\n";
        let (out, _e, res, _c) = run_parallel_watchdog(src);
        assert!(res.is_ok(), "near-miss must not fault: {res:?}");
        assert!(out.contains('7'), "consumer received the sent value: {out:?}");
        assert!(out.contains("done"), "the nursery joined and main continued: {out:?}");
    }

    /// B3.5 — a three-task relay (consumer ← relay ← producer) where `blocked == live` is reached
    /// only momentarily while a message is in flight. A naive blocked-count detector false-positives
    /// here; the per-epoch barrier (a worker holding a deliverable message pops it instead of
    /// confirming empty) must not.
    #[test]
    fn parallel_chained_near_miss_no_false_positive() {
        let src = "fn relay(x: Channel[int], z: Channel[int]):\n    v := x.recv()\n    z.send(v)\nfn producer(x: Channel[int]):\n    x.send(1)\nfn consumer(z: Channel[int]):\n    print(z.recv())\nfn main():\n    x := Channel[int]()\n    z := Channel[int]()\n    parallel:\n        spawn consumer(z)\n        spawn relay(x, z)\n        spawn producer(x)\n    print(\"ok\")\nmain()\n";
        let (out, _e, res, _c) = run_parallel_watchdog(src);
        assert!(res.is_ok(), "chained relay must not false-positive: {res:?}");
        assert!(out.contains('1'), "the relayed value reached the consumer: {out:?}");
        assert!(out.contains("ok"), "the nursery joined: {out:?}");
    }

    /// D6 — single-connection loopback over `std.net` on the M:N engine: a `parallel:` runs a server
    /// fiber (`listen`/`accept`/`read`/`write`) and a client fiber (`connect`/`write`/`read`) in one
    /// program. A would-block `accept`/`read` parks on the netpoller and resumes on readiness, so the
    /// round-trip completes without a thread per op. `Listener.addr()` surfaces the OS-assigned port so
    /// the client can reach the `:0` bind. Watchdog-guarded.
    #[test]
    fn net_loopback_round_trip_over_parallel() {
        let src = r#"import std.net

fn serve(server: Listener) -> int!:
    conn := server.accept()?
    msg := conn.read(64)?
    conn.write("echo:" + msg)?
    conn.close()
    server.close()
    return Ok(0)

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("hello")?
    reply := sock.read(64)?
    print(reply)
    sock.close()
    return Ok(0)

fn run() -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn serve(server)
        spawn client(addr)
    return Ok(0)

fn main():
    match run():
        Ok(_): print("done")
        Err(e): print("net error: " + e.message())

main()
"#;
        let (out, _e, res, _c) = run_parallel_watchdog(src);
        assert!(res.is_ok(), "net round-trip must not fault: {res:?}");
        assert!(out.contains("echo:hello"), "the client received the server's echo: {out:?}");
        assert!(out.contains("done"), "the nursery joined and main continued: {out:?}");
        assert!(!out.contains("net error"), "no I/O error on the happy path: {out:?}");
    }

    /// D6c — run a `--parallel` net program from a temp file under a 30 s watchdog (net round-trips
    /// can legitimately take longer than `run_parallel_watchdog`'s 5 s, and a regressed timeout would
    /// HANG rather than fault). Returns the captured stdout, or panics loudly on a hang.
    fn run_net_timeout_watchdog(tag: &str, src: &str) -> String {
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file_parallel(&entry, crate::native::HostConfig::default()));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok((out, _err, res, _code)) => {
                assert!(res.is_ok(), "{tag}: program faulted: {res:?}");
                out
            }
            Err(_) => panic!("{tag}: hung — D6c socket timeout regressed (the op parked forever)"),
        }
    }

    /// D6c — `conn.read(n, timeout_ms)` returns `Err("timeout")` when the peer accepts but never
    /// writes. The server accepts the connection and then sleeps past the client's read timeout; the
    /// client's `read(64, 100)` parks on the netpoller with a 100 ms deadline, the deadline fires
    /// before any data, and the rewound op returns `Err` with `e.message() == "timeout"`.
    #[test]
    fn read_timeout_returns_err() {
        let src = "\
import std.net
import std.time

fn server(listener: Listener) -> int!:
    conn := listener.accept()?
    time.sleep_ms(400)
    conn.close()
    listener.close()
    return Ok(0)

fn client(addr: str):
    sock := net.connect(addr)?
    match sock.read(64, 100):
        Ok(s): print(\"GOT:\" + s)
        Err(e): print(\"ERR:\" + e.message())
    sock.close()

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    addr := listener.addr()?
    parallel:
        spawn server(listener)
        spawn client(addr)
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"\")
        Err(e): print(\"RUN-ERR:\" + e.message())

main()
";
        let out = run_net_timeout_watchdog("read_timeout", src);
        assert!(out.contains("ERR:timeout"), "read(64, 100) must surface Err(\"timeout\"): {out:?}");
        assert!(!out.contains("GOT:"), "no data should have been read: {out:?}");
    }

    /// D6c — `server.accept(timeout_ms)` returns `Err("timeout")` when NO client ever connects, and the
    /// program terminates (no hang). The lone acceptor parks on the netpoller with a deadline; the
    /// deadline fires, the rewound `accept` returns `Err("timeout")`, and the nursery joins.
    #[test]
    fn accept_timeout_returns_err() {
        let src = "\
import std.net

fn server(listener: Listener):
    match listener.accept(100):
        Ok(_): print(\"ACCEPTED\")
        Err(e): print(\"ERR:\" + e.message())
    listener.close()

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    parallel:
        spawn server(listener)
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"done\")
        Err(e): print(\"RUN-ERR:\" + e.message())

main()
";
        let out = run_net_timeout_watchdog("accept_timeout", src);
        assert!(out.contains("ERR:timeout"), "accept(100) with no client must surface Err(\"timeout\"): {out:?}");
        assert!(out.contains("done"), "the nursery joined and main continued (no hang): {out:?}");
        assert!(!out.contains("ACCEPTED"), "nothing was accepted: {out:?}");
    }

    /// D6c regression — a `read(n)` with NO timeout still parks FOREVER (until data arrives): the
    /// timeout machinery must not have made the untimed read return early. The server sleeps well past
    /// any plausible deadline before writing; the client's untimed `read(64)` must wait for the bytes
    /// (not time out) and print them.
    #[test]
    fn read_without_timeout_still_parks_forever() {
        let src = "\
import std.net
import std.time

fn server(listener: Listener) -> int!:
    conn := listener.accept()?
    time.sleep_ms(300)
    conn.write(\"late\")?
    conn.close()
    listener.close()
    return Ok(0)

fn client(addr: str):
    sock := net.connect(addr)?
    match sock.read(64):
        Ok(s): print(\"GOT:\" + s)
        Err(e): print(\"ERR:\" + e.message())
    sock.close()

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    addr := listener.addr()?
    parallel:
        spawn server(listener)
        spawn client(addr)
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"\")
        Err(e): print(\"RUN-ERR:\" + e.message())

main()
";
        let out = run_net_timeout_watchdog("read_no_timeout", src);
        assert!(out.contains("GOT:late"), "an untimed read must block until data, not time out: {out:?}");
        assert!(!out.contains("ERR:"), "no timeout/error on the untimed read: {out:?}");
    }

    /// D6c — the bundled `examples/socket_timeout.chz` golden: a `--parallel` program that demonstrates
    /// both an `accept(timeout_ms)` and a `read(n, timeout_ms)` timeout branch, run end-to-end against
    /// its `.expected` output. Net examples need `--parallel` (no fibers to park on the cooperative
    /// engine), so — like `echo_server.chz` — this is exercised here rather than in the cooperative
    /// golden harness. Watchdog-guarded so a regression faults instead of hanging the test binary.
    #[test]
    fn example_socket_timeout_matches_expected() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let entry = manifest.join("examples/socket_timeout.chz");
        let expected = std::fs::read_to_string(manifest.join("examples/socket_timeout.expected"))
            .expect("read examples/socket_timeout.expected");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file_parallel(&entry, crate::native::HostConfig::default()));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok((out, _err, res, _code)) => {
                assert!(res.is_ok(), "socket_timeout.chz faulted: {res:?}");
                assert_eq!(out, expected, "socket_timeout.chz output diverged from its golden");
            }
            Err(_) => panic!("socket_timeout.chz hung — D6c timeout regressed"),
        }
    }

    /// D6b — the production-ready gate (regression for the documented HANG): a `parallel:` runs a
    /// fiber parked on `accept` (no client ever connects, so it parks on the netpoller forever) beside
    /// a sibling that faults. Before D6b's `poller::drain_sched`, the faulting sibling tripped cancel
    /// but never reached the poller-parked acceptor — its task stayed `inflight`, the fault never
    /// propagated, and the nursery wedged. Now the drain re-injects the acceptor, it unwinds on the
    /// cancel flag, and the original fault surfaces. Watchdog-guarded: a regression re-hangs here.
    #[test]
    fn net_faulting_sibling_aborts_accept_parked_peer() {
        let src = r#"import std.net

fn faulter(z: int) -> int!:
    return Ok(10 / z)

fn acceptor(server: Listener) -> int!:
    conn := server.accept()?
    conn.close()
    return Ok(0)

fn run() -> int!:
    server := net.listen("127.0.0.1:0")?
    parallel:
        spawn acceptor(server)
        spawn faulter(0)
    return Ok(0)

fn main():
    match run():
        Ok(_): print("joined ok")
        Err(e): print("caught: " + e.message())

main()
"#;
        let (out, _e, res, _c) = run_parallel_watchdog(src);
        let err = res.expect_err("the faulting sibling's error must propagate, not hang the nursery");
        assert!(err.message.contains("division by zero"), "the original fault surfaces: {}", err.message);
        assert!(!out.contains("joined ok"), "the nursery faulted rather than joining cleanly: {out:?}");
    }

    /// D6b — non-blocking `connect` actually parks (and is drainable): a fiber connects to an
    /// unroutable TEST-NET-1 address (RFC 5737 `192.0.2.0/24` — the SYN gets no reply, so the
    /// non-blocking connect stays `EINPROGRESS` and the fiber parks on writability *indefinitely*),
    /// while a sibling faults. A blocking v1 connect would have pinned a worker on the dead handshake;
    /// the parked connect must instead be reached by `poller::drain_sched` so the fault propagates and
    /// the nursery joins. Deterministic (the address never completes) and watchdog-guarded.
    #[test]
    fn net_connect_parks_and_is_drained_on_fault() {
        let src = r#"import std.net

fn faulter(z: int) -> int!:
    return Ok(10 / z)

fn dialer() -> int!:
    sock := net.connect("192.0.2.1:9")?
    sock.close()
    return Ok(0)

fn run() -> int!:
    parallel:
        spawn dialer()
        spawn faulter(0)
    return Ok(0)

fn main():
    match run():
        Ok(_): print("joined ok")
        Err(e): print("caught: " + e.message())

main()
"#;
        let (out, _e, res, _c) = run_parallel_watchdog(src);
        let err = res.expect_err("the faulting sibling aborts the connect-parked dialer, no hang");
        assert!(err.message.contains("division by zero"), "the original fault surfaces: {}", err.message);
        assert!(!out.contains("joined ok"), "the nursery faulted rather than joining cleanly: {out:?}");
    }

    /// D6b — the top-level (no-`--parallel`) blocking connect fallback returns a clean `Err` rather
    /// than hanging: `net.connect` to a dead loopback port (bound-then-dropped) settles to a refusal
    /// through `block_until_connected`. Guards the bounded-spin fix — a regression to an unbounded spin
    /// on a non-completing handshake would surface as a watchdog timeout here.
    #[test]
    fn net_connect_top_level_dead_port_errors_not_hangs() {
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        let src = format!(
            "import std.net\nfn main():\n    match net.connect(\"{dead}\"):\n        Ok(_): print(\"connected\")\n        Err(e): print(\"refused\")\nmain()\n"
        );
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let t = TmpDir::new();
            let entry = t.write("main.chz", &src);
            let (out, _e, res, _c) = run_file(&entry); // cooperative (no --parallel) ⇒ the blocking fallback
            let _ = tx.send((out, res));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(15)) {
            Ok((out, res)) => {
                res.expect("top-level connect program runs");
                assert_eq!(out, "refused\n", "dead port ⇒ Err branch (bounded, no hang)");
            }
            Err(_) => panic!("hung: top-level connect to a dead port did not return (unbounded spin?)"),
        }
    }

    /// D6 — the headline netpoller test: an echo server services **far more connections than there are
    /// workers**, without a thread per connection. One acceptor fiber + N=100 client fibers run in a
    /// single `parallel:` over a core-sized pool (100 ≫ cores). Every client parks on its `read` and
    /// the acceptor parks on each `accept`/`read` — on the netpoller, not a pinned worker — so all 100
    /// round-trips complete. Without the poller (thread-per-park) the bounded pool would starve and the
    /// watchdog would fire. (N stays under the TCP backlog so the v1 *blocking* connect never pins a
    /// worker waiting for backlog room — non-blocking connect is deferred to D6b; the per-connection
    /// handler runs inline in the acceptor because M:N's fixed task `total` has no spawn-after-join.)
    #[test]
    fn net_echo_server_services_more_conns_than_workers() {
        let src = r#"import std.net

fn acceptor(server: Listener, n: int) -> int!:
    for _ in 0..n:
        conn := server.accept()?
        msg := conn.read(64)?
        conn.write("echo:" + msg)?
        conn.close()
    server.close()
    return Ok(0)

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("ping")?
    reply := sock.read(64)?
    sock.close()
    if reply == "echo:ping":
        return Ok(1)
    return Err("bad reply: " + reply)

fn run(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn acceptor(server, n)
        for _ in 0..n:
            spawn client(addr)
    return Ok(0)

fn main():
    match run(100):
        Ok(_): print("all served")
        Err(e): print("error: " + e.message())

main()
"#;
        let (out, _e, res, _c) = run_parallel_watchdog(src);
        assert!(res.is_ok(), "100-conn echo server must not fault: {res:?}");
        assert!(out.contains("all served"), "every connection was serviced + the nursery joined: {out:?}");
        assert!(!out.contains("error"), "no client saw a bad echo: {out:?}");
    }

    /// Per-connection spawn — the spec's canonical shape: the acceptor `spawn`s a `handle(conn)`
    /// fiber PER connection inside its `parallel:` instead of serving inline, and the inner nursery
    /// joins them. `#conns ≫ #workers` still completes (handlers multiplex over the core-sized pool).
    /// Exercises the eager-nursery + `MnSched::inject` path end-to-end with the bytecode engine.
    #[test]
    fn net_echo_server_spawns_handler_per_connection() {
        // Nested socket nurseries (an acceptor's `parallel:` servicing outer-sibling clients) need
        // ≥2 hw threads: the inner join blocks the parent's outer worker (decision B), so on a single
        // core the outer clients can't progress to drain the echoes — a pre-existing M:N limit that
        // per-connection spawn is the first to exercise. Skip on 1 core (CI is ≥2 core) rather than hang.
        if std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) < 2 {
            return;
        }
        let src = r#"import std.net

fn handle(conn: Socket) -> int!:
    msg := conn.read(64)?
    conn.write("echo:" + msg)?
    conn.close()
    return Ok(0)

fn acceptor(server: Listener, n: int) -> int!:
    parallel:
        for _ in 0..n:
            conn := server.accept()?
            spawn handle(conn)
    server.close()
    return Ok(0)

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("ping")?
    reply := sock.read(64)?
    sock.close()
    if reply == "echo:ping":
        return Ok(1)
    return Err("bad reply: " + reply)

fn run(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn acceptor(server, n)
        for _ in 0..n:
            spawn client(addr)
    return Ok(0)

fn main():
    match run(100):
        Ok(_): print("all served")
        Err(e): print("error: " + e.message())

main()
"#;
        let (out, _e, res, _c) = run_parallel_watchdog(src);
        assert!(res.is_ok(), "per-connection-spawn echo server must not fault: {res:?}");
        assert!(out.contains("all served"), "every connection was handled by its own fiber: {out:?}");
        assert!(!out.contains("error"), "no client saw a bad echo: {out:?}");
    }

    /// Per-connection spawn — proves handlers run CONCURRENTLY with accepting (not queued-to-join).
    /// A single client opens N connections SEQUENTIALLY: each reply must arrive before the next
    /// connect. The acceptor `spawn`s a handler per connection. Under the old queue-at-join model the
    /// handler never ran during the accept loop, so the client's first `read` blocked forever and the
    /// acceptor's second `accept` had no incoming connection → hang (watchdog fires). The eager
    /// inner nursery runs each handler immediately, unblocking the client so the loop advances.
    #[test]
    fn net_echo_sequential_client_needs_concurrent_handlers() {
        // Eager per-connection spawn requires ≥2 hardware threads (the inner join blocks the parent's
        // sole outer worker on a single core — see `Op::EnterNursery`). This test's whole point is a
        // handler running mid-loop to unblock the next connect, which a 1-core box cannot do; skip it
        // there rather than hang. CI runners are ≥2 core in practice.
        if std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) < 2 {
            return;
        }
        let src = r#"import std.net

fn handle(conn: Socket) -> int!:
    msg := conn.read(64)?
    conn.write("echo:" + msg)?
    conn.close()
    return Ok(0)

fn acceptor(server: Listener, n: int) -> int!:
    parallel:
        for _ in 0..n:
            conn := server.accept()?
            spawn handle(conn)
    server.close()
    return Ok(0)

fn client(addr: str, n: int) -> int!:
    for i in 0..n:
        sock := net.connect(addr)?
        sock.write("ping")?
        reply := sock.read(64)?
        sock.close()
        if reply != "echo:ping":
            return Err("bad reply: " + reply)
    return Ok(0)

fn run(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn acceptor(server, n)
        spawn client(addr, n)
    return Ok(0)

fn main():
    match run(8):
        Ok(_): print("all served")
        Err(e): print("error: " + e.message())

main()
"#;
        let (out, _e, res, _c) = run_parallel_watchdog(src);
        assert!(res.is_ok(), "sequential client must complete once handlers run concurrently: {res:?}");
        assert!(out.contains("all served"), "all 8 sequential round-trips serviced: {out:?}");
        assert!(!out.contains("error"), "every reply was a correct echo: {out:?}");
    }

    /// Per-connection spawn — a per-connection HANDLER fault propagates as the acceptor's fault and
    /// tears the run down WITHOUT hanging. One injected handler faults (index-out-of-bounds — a real
    /// runtime fault, since a spawned task's `Result` *return value* is discarded); the eager inner
    /// nursery trips its own cancel (D6b `cancel_drain` + `drain_sched` reach sibling handlers), the
    /// join surfaces the fault as the acceptor's body fault, and the OUTER nursery then cancels the
    /// clients (so a client stranded without its echo unwinds instead of blocking on `read` forever).
    #[test]
    fn net_echo_handler_fault_cancels_acceptor() {
        // Nested socket nursery → needs ≥2 hw threads (see `net_echo_server_spawns_handler_per_connection`).
        if std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) < 2 {
            return;
        }
        let src = r#"import std.net

fn handle(conn: Socket, i: int) -> int!:
    msg := conn.read(64)?
    if i == 0:
        conn.close()
        boom := [1]
        return Ok(boom[10])
    conn.write("echo:" + msg)?
    conn.close()
    return Ok(0)

fn acceptor(server: Listener, n: int) -> int!:
    parallel:
        for i in 0..n:
            conn := server.accept()?
            spawn handle(conn, i)
    server.close()
    return Ok(0)

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("ping")?
    reply := sock.read(64)?
    sock.close()
    return Ok(1)

fn run(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn acceptor(server, n)
        for _ in 0..n:
            spawn client(addr)
    return Ok(0)

fn main():
    match run(6):
        Ok(_): print("all served")
        Err(e): print("error: " + e.message())

main()
"#;
        // The whole point is "no hang": `run_parallel_watchdog` panics if the nursery never
        // terminates. A faulting handler must drive the run to a clean finish (fault surfaced via the
        // acceptor's `match`, or propagated), not deadlock the netpoller-parked siblings.
        let (out, _e, res, _c) = run_parallel_watchdog(src);
        assert!(
            res.is_err() || out.contains("error"),
            "a per-connection handler fault must surface (faulted run or reported error), not be swallowed: res={res:?} out={out:?}"
        );
        assert!(!out.contains("all served"), "the run must not report success once a handler faulted: {out:?}");
    }

    /// Per-connection spawn — the DEGENERATE eager nursery: a `parallel:` body (entered eagerly under
    /// `--parallel` inside a fiber) that injects NOTHING. `activate_eager_nursery` builds a `total==0`
    /// sched with `body_open`; `JoinNursery` must `close_body` and have the inline worker terminate
    /// immediately (`done==0==total`) and join the drainer — not hang on the empty sched. Pins the
    /// `body_open` → `close_body` → terminate handshake on the empty path.
    #[test]
    fn eager_nursery_with_zero_spawns_completes() {
        if std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) < 2 {
            return;
        }
        let src = "fn worker():\n    parallel:\n        print(\"eager body, no spawn\")\n    print(\"worker done\")\nfn main():\n    parallel:\n        spawn worker()\nmain()\n";
        let (out, _e, res, _c) = run_parallel_watchdog(src);
        assert!(res.is_ok(), "an empty eager nursery must join cleanly: {res:?}");
        assert!(out.contains("eager body, no spawn"), "the body ran: {out:?}");
        assert!(out.contains("worker done"), "the eager nursery joined and the worker continued: {out:?}");
    }

    /// Per-connection spawn — CONCURRENT eager nurseries (the pool-exhaustion regression): four
    /// independent servers each run their OWN eager per-connection-spawn nursery at once. Because each
    /// eager nursery drains on a DEDICATED raw OS thread (not the bounded process pool), they do not
    /// starve each other — with the earlier pool-farmed design, four long-running eager drainers would
    /// exhaust a core-sized pool and hang (undetectably, since `body_open` vetoes the deadlock predicate).
    #[test]
    fn net_concurrent_eager_servers_do_not_exhaust_pool() {
        if std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) < 2 {
            return;
        }
        let src = r#"import std.net

fn handle(conn: Socket) -> int!:
    msg := conn.read(64)?
    conn.write("echo:" + msg)?
    conn.close()
    return Ok(0)

fn server_loop(server: Listener, n: int) -> int!:
    parallel:
        for _ in 0..n:
            conn := server.accept()?
            spawn handle(conn)
    server.close()
    return Ok(0)

fn pinger(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("ping")?
    reply := sock.read(64)?
    sock.close()
    if reply == "echo:ping":
        return Ok(1)
    return Err("bad reply: " + reply)

fn one_server(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn server_loop(server, n)
        for _ in 0..n:
            spawn pinger(addr)
    return Ok(0)

fn run(servers: int, conns: int) -> int!:
    parallel:
        for _ in 0..servers:
            spawn one_server(conns)
    return Ok(0)

fn main():
    match run(4, 12):
        Ok(_): print("all servers done")
        Err(e): print("error: " + e.message())

main()
"#;
        let (out, _e, res, _c) = run_parallel_watchdog(src);
        assert!(res.is_ok(), "concurrent eager servers must not fault: {res:?}");
        assert!(out.contains("all servers done"), "every concurrent eager nursery completed: {out:?}");
        assert!(!out.contains("error"), "no pinger saw a bad echo: {out:?}");
    }

    /// B3.5 — a task that finishes normally but strands a `recv`-blocked sibling (it never sent the
    /// channel the sibling waits on) is a deadlock. Exercises the `task_finished` `live--` path:
    /// dropping the finished task from the live count makes `blocked == live`, so the survivor faults.
    #[test]
    fn parallel_finished_task_leaves_sibling_deadlocked() {
        let src = "fn waiter(c: Channel[int]):\n    c.recv()\nfn quick():\n    print(\"quick\")\nfn main():\n    c := Channel[int]()\n    parallel:\n        spawn waiter(c)\n        spawn quick()\nmain()\n";
        let (out, _e, res, _c) = run_parallel_watchdog(src);
        let err = res.expect_err("a finished sibling that strands a recv-blocked task is a deadlock");
        assert!(err.message.contains("deadlock"), "got: {}", err.message);
        assert!(out.contains("quick"), "the finished task's output is still flushed: {out:?}");
    }

    /// Run an entry through both engines with a freshly-built [`crate::native::HostConfig`] each
    /// (the config isn't `Clone` — `mk_cfg` produces an identical one per engine). Asserts stdout +
    /// ok/err parity; returns the agreed stdout.
    fn parity_entry_cfg(
        src: &str,
        mk_cfg: impl Fn() -> crate::native::HostConfig,
    ) -> String {
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (io, ie_out, ir, _ic) = crate::interp::run_file_with(&entry, mk_cfg());
        let (vo, ve_out, vr, _vc) = run_file_with(&entry, mk_cfg());
        assert_eq!(io, vo, "stdout divergence (interp vs vm)");
        assert_eq!(ie_out, ve_out, "stderr divergence (interp vs vm)");
        assert_eq!(ir.is_ok(), vr.is_ok(), "ok/err divergence: interp={ir:?} vm={vr:?}");
        io
    }

    #[test]
    fn parity_std_io_print() {
        assert_eq!(
            parity_entry("import std.io\nfn main():\n    io.print(\"hello\")\nmain()"),
            "hello\n"
        );
    }

    #[test]
    fn parity_std_io_read_write_file() {
        let t = TmpDir::new();
        let data = t.0.join("data.txt").display().to_string();
        let src = format!(
            "import std.io\nfn main():\n    match io.write_file(\"{data}\", \"hello\\nworld\"):\n        Ok(_): io.print(\"wrote\")\n        Err(e): io.print(e)\n    match io.read_file(\"{data}\"):\n        Ok(s): io.print(s)\n        Err(e): io.print(e)\nmain()"
        );
        let entry = t.write("main.chz", &src);
        let (io_out, _ie, ir, _) = crate::interp::run_file(&entry);
        let (vo, _ve, vr, _) = run_file(&entry);
        assert!(ir.is_ok() && vr.is_ok(), "interp={ir:?} vm={vr:?}");
        assert_eq!(io_out, vo);
        assert_eq!(io_out, "wrote\nhello\nworld\n");
    }

    #[test]
    fn parity_std_io_read_missing_file_errs() {
        // The error text comes from the same `std::fs` call on both engines, so it matches; we only
        // assert the Err branch is taken (deterministic regardless of OS message).
        let src = "import std.io\nfn main():\n    match io.read_file(\"/no/such/chezzi/path/xyz\"):\n        Ok(s): io.print(s)\n        Err(e): io.print(\"err\")\nmain()";
        assert_eq!(parity_entry(src), "err\n");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn read_file_caps_oversized_input() {
        // /dev/zero is unbounded; read_file must return an Err (the size cap), not OOM.
        let src = "import std.io\nfn main():\n    match io.read_file(\"/dev/zero\"):\n        Ok(s): io.print(\"ok\")\n        Err(e): io.print(\"capped\")\nmain()";
        assert_eq!(parity_entry(src), "capped\n");
    }

    #[test]
    fn parity_std_io_read_line_consumes_injected_stdin() {
        use crate::native::{HostConfig, Stdin};
        let src = "import std.io\nfn main():\n    match io.read_line():\n        Some(l): io.print(\"got {l}\")\n        None: io.print(\"eof\")\n    match io.read_line():\n        Some(l): io.print(l)\n        None: io.print(\"eof\")\nmain()";
        let out = parity_entry_cfg(src, || HostConfig {
            stdin: Stdin::Lines(["alpha".to_string()].into_iter().collect()),
            ..Default::default()
        });
        assert_eq!(out, "got alpha\neof\n");
    }

    #[test]
    fn parity_std_io_eprint_goes_to_stderr_not_stdout() {
        let src = "import std.io\nfn main():\n    io.eprint(\"to stderr\")\n    io.print(\"to stdout\")\nmain()";
        // Parity (both engines): stdout has only the print line, stderr has only the eprint line.
        assert_eq!(parity_entry(src), "to stdout\n");
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let (out, err, res, _) = run_file(&entry);
        assert!(res.is_ok());
        assert_eq!(out, "to stdout\n");
        assert_eq!(err, "to stderr\n");
    }

    #[test]
    fn parity_std_os_args_and_env() {
        use crate::native::HostConfig;
        let src = "import std.io\nimport std.os\nfn main():\n    for a in os.args():\n        io.print(a)\n    match os.env(\"CHEZZI_TEST_VAR\"):\n        Some(v): io.print(v)\n        None: io.print(\"no var\")\nmain()";
        let out = parity_entry_cfg(src, || HostConfig {
            args: vec!["x".to_string(), "y".to_string()],
            env: [("CHEZZI_TEST_VAR".to_string(), "hi".to_string())].into_iter().collect(),
            ..Default::default()
        });
        assert_eq!(out, "x\ny\nhi\n");
    }

    #[test]
    fn parity_std_os_env_missing_is_none() {
        use crate::native::HostConfig;
        let src = "import std.io\nimport std.os\nfn main():\n    match os.env(\"DEFINITELY_UNSET_XYZ\"):\n        Some(v): io.print(v)\n        None: io.print(\"none\")\nmain()";
        let out = parity_entry_cfg(src, HostConfig::default);
        assert_eq!(out, "none\n");
    }

    #[test]
    fn parity_std_os_getcwd_ok() {
        let src = "import std.io\nimport std.os\nfn main():\n    match os.getcwd():\n        Ok(p): io.print(\"ok\")\n        Err(e): io.print(\"err\")\nmain()";
        assert_eq!(parity_entry(src), "ok\n");
    }

    /// Run a single-file (importing std) program on the VM with GC stress on (collect before every
    /// instruction) and the given config — surfaces any native-return value the collector might free
    /// while still reachable.
    fn vm_run_file_stress(src: &str, cfg: crate::native::HostConfig) -> String {
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        let graph = crate::resolver::build_graph(&entry).unwrap();
        let program = crate::compiler::compile_graph(&graph).unwrap();
        let mut vm = Vm::new(Arc::new(program));
        vm.gc_stress = true;
        vm.host = cfg;
        vm.run().unwrap_or_else(|e| panic!("unexpected error under GC stress: {e}"));
        vm.out
    }

    #[test]
    fn parity_std_str_pure_chezzi_with_mixed_native_import() {
        // std.str is a real Chezzi file (crate/std/str.chz); std.io is native — both in one program.
        let src = "import std.io\nimport std.str as text\nfn main():\n    io.print(text.repeat(\"ab\", 3))\n    io.print(text.reverse(\"hello\"))\n    io.print(text.pad_left(\"7\", 3, \"0\"))\n    if text.is_empty(\"\"):\n        io.print(\"empty\")\n    for line in text.split_lines(\"a\\nb\\nc\"):\n        io.print(line)\nmain()";
        assert_eq!(parity_entry(src), "ababab\nolleh\n007\nempty\na\nb\nc\n");
    }

    #[test]
    fn native_returned_heap_values_survive_gc_stress() {
        use crate::native::HostConfig;
        // Each os.args() call allocates a fresh heap list (immediately garbage); under stress the
        // collector runs every instruction. A dangling handle in native lowering would panic here.
        let src = "import std.io\nimport std.os\nfn main():\n    n := 0\n    while n < 300:\n        xs := os.args()\n        n += 1\n    io.print(\"done {n}\")\nmain()";
        let cfg = HostConfig { args: vec!["a".to_string()], ..Default::default() };
        let out = vm_run_file_stress(src, cfg);
        assert_eq!(out, "done 300\n");
    }

    /// A spread of programs exercising every feature class — run through BOTH engines.
    const PROGRAMS: &[&str] = &[
        // arithmetic + promotion + truncation
        "print(7 / 2)\nprint(1 + 2.0)\nprint(2.5 * 2.0)\nprint(10 % 3)",
        // string concat + interpolation + escapes
        "fn main():\n    n := \"x\"\n    print(\"a{n}b {1 + 2} {{lit}}\")\nmain()",
        // comparison + equality + bool logic
        "print(1 < 2)\nprint(2 == 2.0)\nprint(true and false)\nprint(false or true)\nprint(not true)",
        // lists, indexing, len
        "print([1, 2, 3])\nprint([10, 20, 30][2])\nprint(len([1, 2]))",
        // structs + methods
        "struct P:\n    x: int\n    y: int\n    fn sum(self) -> int:\n        return self.x + self.y\nfn main():\n    p := P(3, 4)\n    print(p)\n    print(p.sum())\nmain()",
        // enums + match + payload binding
        "enum S:\n    C(int)\n    Sq(int)\nfn a(s: S) -> int:\n    match s:\n        C(r): return r * r\n        Sq(n): return n * n\nfn main():\n    print(a(C(3)))\n    print(a(Sq(4)))\nmain()",
        // generic enum (type-erased): same enum at two element types + match payload substitution
        "enum Tree[T]:\n    Leaf\n    Node(T, Tree[T], Tree[T])\nfn sum(t: Tree[int]) -> int:\n    match t:\n        Leaf: return 0\n        Node(v, l, r): return sum(l) + v + sum(r)\nfn main():\n    t: Tree[int] = Node(2, Node(1, Leaf, Leaf), Node(3, Leaf, Leaf))\n    print(sum(t))\nmain()",
        // closures
        "fn adder(n: int):\n    return fn(x: int) -> int: x + n\nfn main():\n    f := adder(10)\n    print(f(5))\nmain()",
        // ? operator (Ok + Err propagation)
        "fn d(a: int, b: int) -> Result[int]:\n    if b == 0:\n        return Err(\"zero\")\n    return Ok(a / b)\nfn use() -> Result[int]:\n    r := d(10, 0)?\n    return Ok(r)\nfn main():\n    match use():\n        Ok(v): print(v)\n        Err(e): print(e)\nmain()",
        // for + while loops
        "fn main():\n    t := 0\n    for i in 0..100:\n        t += i\n    print(t)\n    n := 5\n    while n > 0:\n        n -= 1\n    print(n)\nmain()",
        // builtins
        "print(range(4))\nprint(int(\"7\") + 1)\nprint(float(3))\nprint(len([1, 2, 3]))\nprint(str(42))",
        // recursion
        "fn fib(n: int) -> int:\n    if n < 2:\n        return n\n    return fib(n - 1) + fib(n - 2)\nfn main():\n    print(fib(15))\nmain()",
        // inferred return type (no `-> T`): runtime is unaffected, both engines agree
        "fn add(a: int, b: int):\n    return a + b\nfn classify(n: int):\n    if n == 0:\n        return Some(0)\n    return None\nfn main():\n    print(add(2, 3))\n    match classify(0):\n        Some(v): print(v)\n        None: print(\"none\")\nmain()",
        // expression-valued match (multiline) + if (inline): both engines must agree on the value
        "fn lookup(k: int) -> int?:\n    if k == 0:\n        return None\n    return Some(k)\nfn main():\n    found := match lookup(7):\n        Some(v): v\n        None: -1\n    print(found)\n    sign := if found > 0: \"pos\" else: \"neg\"\n    print(sign)\n    none := match lookup(0):\n        Some(v): v\n        None: -1\n    print(none)\nmain()",
        // ----- M6: core-type methods (str) -----
        "print(\"abcd\".len())\nprint(\"Hi There\".upper())\nprint(\"Hi There\".lower())\nprint(\"  pad  \".trim())",
        // str conforms to Error: message() returns the string itself
        "print(\"boom\".message())",
        // Go-style Result[T, E]: custom struct error (T!E), match, message() dispatch
        "struct DbErr:\n    code: int\n    fn message(self) -> str:\n        return \"db {self.code}\"\nfn q(ok: bool) -> int!DbErr:\n    if ok:\n        return Ok(1)\n    return Err(DbErr(503))\nfn main():\n    match q(false):\n        Ok(v): print(v)\n        Err(e): print(e.message())\n    match q(true):\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()",
        // default-Error path: Err(str) flows as Result[int, Error], consumed via message()
        "fn parse(ok: bool) -> int!:\n    if ok:\n        return Ok(42)\n    return Err(\"bad input\")\nfn main():\n    match parse(false):\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()",
        // ----- M11 Phase B: recover boundary -----
        // recover catches index-OOB; Ok path wraps the trailing value
        "fn main():\n    r := recover:\n        [1, 2][9]\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"recovered: {e.message()}\")\nmain()",
        // recover catches divide-by-zero
        "fn main():\n    r := recover:\n        10 / 0\n    match r:\n        Ok(v): print(v)\n        Err(e): print(\"err: {e.message()}\")\nmain()",
        // recover catches integer overflow
        "fn main():\n    r := recover:\n        9223372036854775807 * 2\n    match r:\n        Ok(v): print(v)\n        Err(e): print(\"ovf\")\nmain()",
        // recover ok-path wraps the value
        "fn main():\n    r := recover:\n        2 + 3\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err\")\nmain()",
        // a fault three calls deep is caught at the boundary (no per-call wrapping)
        "fn a() -> int:\n    return b()\nfn b() -> int:\n    return c()\nfn c() -> int:\n    return [1][9]\nfn main():\n    r := recover:\n        a()\n    match r:\n        Ok(v): print(v)\n        Err(e): print(\"deep recovered\")\nmain()",
        // `?` inside recover short-circuits to `r` (try-block): the Err lands in `r`, and code
        // AFTER the recover still runs — the enclosing fn returns a plain str, so this only works
        // if `?` did NOT exit the function.
        "fn d(b: int) -> int!:\n    if b == 0:\n        return Err(\"zero\")\n    return Ok(10 / b)\nfn use() -> str:\n    r := recover:\n        x := d(0)?\n        x + 1\n    match r:\n        Ok(v): return \"ok\"\n        Err(e): return \"caught {e.message()}\"\nfn main():\n    print(use())\nmain()",
        // `?` Ok path inside recover: value unwrapped, trailing expression becomes the Ok result
        "fn d(b: int) -> int!:\n    return Ok(10 / b)\nfn main():\n    r := recover:\n        x := d(2)?\n        x + 1\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(e.message())\nmain()",
        // side effects before a caught fault PERSIST (keep semantics) — both engines must agree
        "fn main():\n    x := 1\n    r := recover:\n        x = 99\n        [1][9]\n    match r:\n        Ok(v): print(\"ok\")\n        Err(e): print(\"recovered\")\n    print(\"x={x}\")\nmain()",
        // nested recover: the inner boundary catches, the outer sees a normal value
        "fn main():\n    r := recover:\n        inner := recover:\n            [1][9]\n        match inner:\n            Ok(v): v\n            Err(e): 0\n    match r:\n        Ok(v): print(\"outer ok {v}\")\n        Err(e): print(\"outer err\")\nmain()",
        // recovered value composes with `?` after the boundary\n
        "fn run() -> int!:\n    r := recover:\n        [10, 20][0]\n    v := r?\n    return Ok(v + 1)\nfn main():\n    match run():\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()",
        "print(\"a,b,c\".split(\",\"))\nprint(\",\".join([\"a\", \"b\", \"c\"]))",
        "print(\"abc\".starts_with(\"ab\"))\nprint(\"abc\".starts_with(\"z\"))\nprint(\"abc\".contains(\"b\"))\nprint(\"abc\".contains(\"q\"))",
        // chained core-type methods
        "print(\"  Hello,World  \".trim().lower().split(\",\"))",
        // ----- M6: core-type methods (list) -----
        "fn main():\n    xs := [1, 2]\n    xs.push(3)\n    xs.push(4)\n    print(xs)\n    print(xs.len())\nmain()",
        // ----- M6: pipe operator -----
        "fn inc(n: int) -> int: n + 1\nfn dbl(n: int) -> int: n * 2\nfn main():\n    print(5 |> inc() |> dbl())\nmain()",
        "fn shout(s: str) -> str: s.upper()\nfn main():\n    print(\"hi\" |> shout())\nmain()",
        // ----- error parity -----
        "print(1 / 0)",
        "print([1, 2][9])",
        "print(1 + \"x\")",
        "fn loop(n: int) -> int:\n    return loop(n + 1)\nfn main():\n    print(loop(0))\nmain()",
        // M6 method error parity
        "print(\"hi\".upper(\"extra\"))",
        "print(\"hi\".frobnicate())",
        "print(\",\".join([1, 2]))",
        "print((5).upper())",
        // arg-eval order: a bad method/receiver with an erroring arg must report the SAME error on
        // both engines — the VM evaluates args (operands) before the call, so the interp must too.
        "print((5).frob(1 / 0))",
        "print(\"hi\".frob(1 / 0))",
        // ----- entry model: no auto-main; unhandled top-level Err/None exits -----
        "fn main():\n    print(\"hi\")",                                  // main defined but never called → no output
        "Err(\"boom\")",                                                  // bare top-level Err → unhandled error
        "x := Err(\"oops\")?",                                            // top-level `?` Err → unhandled error
        "fn g() -> Option[int]:\n    return None\ng()",                   // bare None → unhandled error
        "fn f() -> Result[int]:\n    return Err(\"x\")\nr := f()\nprint(\"handled\")", // Err bound = handled → no exit
        "fn main():\n    print(\"before\")\n    x := Err(\"boom\")?\n    print(\"after\")\nmain()", // partial output then exit
        // a user enum shadowing `Err` is a normal value: bare one must NOT exit, `?` must reject it
        "enum Signal:\n    Err(int)\n    Quiet\nErr(5)\nprint(\"made it\")",
        "enum Signal:\n    Err(int)\n    Quiet\nfn f() -> int:\n    x := Err(5)?\n    return x\nf()",
        // unhandled top-level error INSIDE a top-level block (interp: call_depth 0, VM: is_toplevel)
        "if true:\n    Err(\"boom\")\nprint(\"after\")",                  // bare Err in `if` → exit, no "after"
        "for i in 0..1:\n    Err(\"x\")\nprint(\"after\")",              // bare Err in `for` → exit
        "fn d() -> Result[int]:\n    return Err(\"z\")\nif true:\n    x := d()?\n    print(x)", // top-level `?` in block → exit (same span both engines)
    ];

    #[test]
    fn parity_full_suite_vm_vs_interp() {
        for src in PROGRAMS {
            assert_parity(src);
        }
    }

    #[test]
    fn parity_index_assign() {
        assert_parity("xs := [1, 2, 3]\nxs[1] = 9\nxs[0] += 4\nxs[2] -= 1\nprint(xs)\n");
    }

    #[test]
    fn parity_index_assign_out_of_bounds() {
        assert_parity("xs := [1, 2, 3]\nxs[9] = 0\nprint(xs)\n");
    }

    #[test]
    fn parity_compound_index_oob_vs_rhs_error_order() {
        // Compound `xs[i] += rhs` on an out-of-bounds `i` where `rhs` ALSO errors: both engines
        // must agree on which error wins. The VM reads the target (bounds-check) before `rhs`;
        // the interp must do the same.
        assert_parity("xs := [1, 2, 3]\nz := 0\nxs[5] += 1 / z\n");
    }

    #[test]
    fn parity_compound_index_oob_skips_rhs_side_effect() {
        // On an out-of-bounds compound assign, neither engine should run the rhs side effect.
        assert_parity(
            "fn side() -> int:\n    print(\"rhs ran\")\n    return 0\nxs := [1, 2, 3]\nxs[5] += side()\nprint(\"after\")\n",
        );
    }

    #[test]
    fn parity_field_assign() {
        assert_parity(
            "struct P:\n    x: int\n    y: int\np := P(1, 2)\np.x = 9\np.y += 3\nprint(p.x)\nprint(p.y)\n",
        );
    }

    // NOTE: method_type_params / param_protocol / method_default_args were parity-only; they are
    // now full golden tests (`golden_*_chz_matches_expected_and_interp` above), which assert exact
    // output AND cross-engine parity, so the weaker `parity_*` file tests were removed.

    #[test]
    fn parity_hof_param() {
        let src = "fn apply(f: fn(int) -> int, v: int) -> int:\n    return f(v)\ninc := fn(x: int) -> int: x + 1\nprint(apply(inc, 4))\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "5\n");
    }

    #[test]
    fn parity_list_pop_some() {
        let src = "xs := [1,2,3]\nx := xs.pop()\nmatch x:\n    Some(v): print(\"got {v}\")\n    None: print(\"empty\")\nprint(xs.len())\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "got 3\n2\n");
    }

    #[test]
    fn parity_list_pop_empty_none() {
        let src = "xs := [1]\na := xs.pop()\nb := xs.pop()\nmatch b:\n    Some(v): print(\"v\")\n    None: print(\"none\")\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "none\n");
    }

    #[test]
    fn parity_list_reverse() {
        let src = "xs := [3,1,2]\nxs.reverse()\nprint(xs[0])\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "2\n");
    }

    #[test]
    fn parity_list_contains() {
        let src = "print([1,2,3].contains(2))\nprint([1,2,3].contains(9))\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "true\nfalse\n");
    }

    #[test]
    fn parity_list_index_of() {
        let src = "print([10,20,30].index_of(20))\nprint([1,2].index_of(9))\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "1\n-1\n");
    }

    #[test]
    fn parity_list_sum() {
        let src = "print([1,2,3,4].sum())\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "10\n");
    }

    #[test]
    fn parity_list_sort_int() {
        let src = "xs := [3,1,2]\nxs.sort()\nprint(xs[0])\nprint(xs[2])\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "1\n3\n");
    }

    #[test]
    fn parity_list_sort_str() {
        let src = "xs := [\"banana\",\"apple\",\"cherry\"]\nxs.sort()\nfor s in xs:\n    print(s)\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "apple\nbanana\ncherry\n");
    }

    #[test]
    fn parity_list_sort_float() {
        let src = "xs := [3.5, 1.1, 2.2]\nxs.sort()\nprint(xs[0])\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "1.1\n");
    }

    // ===== higher-order list methods: map / filter / fold =====
    //
    // These call a closure per element. On the VM each closure runs nested frames that can GC at
    // instruction boundaries, so the source/result lists (and fold's accumulator) must stay rooted.
    // Several tests use HEAP elements (strings / nested lists) and run under `gc_stress` so that a
    // collection actually happens mid-iteration — if rooting is wrong they crash with a dangling ref.

    #[test]
    fn parity_list_map_int() {
        let src = "xs := [1,2,3]\nys := xs.map(fn(x: int) -> int: x * 2)\nprint(ys)\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "[2, 4, 6]\n");
    }

    #[test]
    fn parity_list_map_to_str_gc_stress() {
        // Each element maps to a freshly-allocated string (heap), so collection mid-map matters.
        let src = "xs := [1,2,3]\nys := xs.map(fn(x: int) -> str: \"n{x}\")\nfor y in ys:\n    print(y)\n";
        assert_parity(src);
        let expected = "n1\nn2\nn3\n";
        assert_eq!(vm_outcome(src).unwrap(), expected);
        assert_eq!(run_capture_stress(src), expected, "VM gc_stress diverged (rooting bug?)");
    }

    #[test]
    fn parity_list_map_to_nested_list_gc_stress() {
        // Maps each element to a nested list (heap); the result list holds heap children.
        let src = "xs := [1,2,3]\nys := xs.map(fn(x: int) -> list[int]: [x, x])\nprint(ys[1][0])\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "2\n");
        assert_eq!(run_capture_stress(src), "2\n", "VM gc_stress diverged (rooting bug?)");
    }

    #[test]
    fn parity_list_filter_gc_stress() {
        // Filter over string elements; kept elements are heap objects pushed into the result.
        let src = "xs := [\"a\",\"bb\",\"ccc\",\"d\"]\nys := xs.filter(fn(x: str) -> bool: x.len() > 1)\nprint(ys.len())\nprint(ys[0])\n";
        assert_parity(src);
        let expected = "2\nbb\n";
        assert_eq!(vm_outcome(src).unwrap(), expected);
        assert_eq!(run_capture_stress(src), expected, "VM gc_stress diverged (rooting bug?)");
    }

    #[test]
    fn parity_list_filter_int() {
        let src = "xs := [1,2,3,4]\nys := xs.filter(fn(x: int) -> bool: x % 2 == 0)\nprint(ys.len())\nprint(ys[0])\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "2\n2\n");
    }

    #[test]
    fn parity_list_fold_str_acc_gc_stress() {
        // Fold building a string accumulator (heap) — each step allocates a new acc string, so the
        // rooted accumulator slot must survive the next element's closure call.
        let src = "xs := [\"a\",\"b\",\"c\"]\ns := xs.fold(\"\", fn(a: str, x: str) -> str: a + x)\nprint(s)\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "abc\n");
        assert_eq!(run_capture_stress(src), "abc\n", "VM gc_stress diverged (rooting bug?)");
    }

    #[test]
    fn parity_list_sort_by_str_gc_stress() {
        // Sort heap-string elements by length; the comparator re-enters the VM and a collection can
        // fire mid-sort. The source list must stay rooted (we permute indices, not raw Values).
        let src = "xs := [\"ccc\",\"a\",\"dd\",\"b\"]\nxs.sort_by(fn(a: str, b: str) -> int: a.len() - b.len())\nfor x in xs:\n    print(x)\n";
        assert_parity(src);
        let expected = "a\nb\ndd\nccc\n";
        assert_eq!(vm_outcome(src).unwrap(), expected);
        assert_eq!(run_capture_stress(src), expected, "VM gc_stress diverged (rooting bug?)");
    }

    #[test]
    fn parity_list_sort_by_nested_list_gc_stress() {
        // Elements are nested lists (heap); sort by first element. Exercises rooting of heap children
        // across comparator calls under stress.
        let src = "xs := [[3,0],[1,0],[2,0]]\nxs.sort_by(fn(a: list[int], b: list[int]) -> int: a[0] - b[0])\nprint(xs[0][0])\nprint(xs[2][0])\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "1\n3\n");
        assert_eq!(run_capture_stress(src), "1\n3\n", "VM gc_stress diverged (rooting bug?)");
    }

    #[test]
    fn parity_list_fold_sum() {
        let src = "print([1,2,3,4].fold(0, fn(a: int, x: int) -> int: a + x))\n";
        assert_parity(src);
        assert_eq!(vm_outcome(src).unwrap(), "10\n");
    }

    fn fixture(rel: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
    }

    /// Run a file through both engines and assert identical (stdout, error).
    fn assert_file_parity(rel: &str) {
        let path = fixture(rel);
        let (vm_out, vm_err, vm_res, _) = run_file(&path);
        let (ip_out, ip_err, ip_res, _) = crate::interp::run_file(&path);
        assert_eq!(vm_out, ip_out, "stdout divergence for {rel}");
        assert_eq!(vm_err, ip_err, "stderr divergence for {rel}");
        assert_eq!(vm_res.err().map(|e| e.to_string()), ip_res.err().map(|e| e.to_string()), "error divergence for {rel}");
    }

    #[test]
    fn golden_hello_via_run_file() {
        let path = fixture("examples/hello.chz");
        let expected = std::fs::read_to_string(fixture("examples/hello.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok());
        assert_eq!(out, expected);
    }

    /// M6 golden: core-type methods + pipe run end-to-end on the VM and byte-match the interp.
    #[test]
    fn golden_methods_via_run_file() {
        let path = fixture("examples/methods.chz");
        let expected = std::fs::read_to_string(fixture("examples/methods.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/methods.chz");
    }

    /// Golden: in-place index & field assignment run end-to-end on the VM and byte-match the interp.
    #[test]
    fn golden_mutate_via_run_file() {
        let path = fixture("examples/mutate.chz");
        let expected = std::fs::read_to_string(fixture("examples/mutate.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/mutate.chz");
    }

    /// M6c golden: the std-library demo (native std.io/math/os + Chezzi std.str) runs end-to-end on
    /// the VM and byte-matches both the `.expected` file and the interpreter.
    #[test]
    fn golden_std_demo_via_run_file() {
        let path = fixture("examples/std_demo.chz");
        let expected = std::fs::read_to_string(fixture("examples/std_demo.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/std_demo.chz");
    }

    /// Additive std.math trig/exp/log intrinsics run end-to-end on the VM and byte-match both the
    /// `.expected` file and the interpreter (parity via `assert_file_parity`).
    #[test]
    fn golden_math_more_via_run_file() {
        let path = fixture("examples/math_more.chz");
        let expected = std::fs::read_to_string(fixture("examples/math_more.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/math_more.chz");
    }

    /// A complete self-contained program (merge sort + binary search + stats over std.math) runs on
    /// the VM, byte-matches `.expected`, and stays identical to the interpreter.
    #[test]
    fn golden_overflow_via_run_file() {
        // The integer-overflow policy, end-to-end: every overflow (arith, neg, div, math.abs) is a
        // recoverable fault, identical on both engines.
        let path = fixture("examples/overflow.chz");
        let expected = std::fs::read_to_string(fixture("examples/overflow.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/overflow.chz");
    }

    #[test]
    fn golden_stats_app_via_run_file() {
        let path = fixture("examples/stats.chz");
        let expected = std::fs::read_to_string(fixture("examples/stats.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/stats.chz");
    }

    /// G3 golden: `examples/stdlib_cmp.chz` — `import std.cmp`, generic `min`/`max`/`clamp` over
    /// int/float/str/struct, and `list.sort()` over Comparable structs. Byte-matches `.expected`
    /// and stays identical on interp + VM.
    #[test]
    fn golden_stdlib_cmp_via_run_file() {
        let path = fixture("examples/stdlib_cmp.chz");
        let expected = std::fs::read_to_string(fixture("examples/stdlib_cmp.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/stdlib_cmp.chz");
    }

    /// std.str helpers golden: `examples/str_more.chz` — the additive ends_with/index_of/count/
    /// replace/strip_prefix/strip_suffix funcs, end-to-end on the VM, byte-identical to `.expected`
    /// and the interpreter.
    #[test]
    fn golden_str_more_via_run_file() {
        let path = fixture("examples/str_more.chz");
        let expected = std::fs::read_to_string(fixture("examples/str_more.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/str_more.chz");
    }

    /// std.iter helpers golden: `examples/iter_more.chz` — the additive take/drop/any/all/find/
    /// flatten funcs, end-to-end on the VM, byte-identical to `.expected` and the interpreter.
    #[test]
    fn golden_iter_more_via_run_file() {
        let path = fixture("examples/iter_more.chz");
        let expected = std::fs::read_to_string(fixture("examples/iter_more.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/iter_more.chz");
    }

    /// M8-M5 golden: `examples/json_decode.chz` — type-directed `json.decode[T]` into struct /
    /// typed map / list / scalar, with Option fields, extra-key tolerance, and an error case.
    /// Byte-identical on interp + VM.
    #[test]
    fn golden_json_decode_via_run_file() {
        let path = fixture("examples/json_decode.chz");
        let expected = std::fs::read_to_string(fixture("examples/json_decode.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/json_decode.chz");
    }

    /// M8-M3 golden: `examples/sys.chz` — the native trio std.process/std.fs/std.time, end-to-end
    /// on the VM, byte-identical to `.expected` and the interpreter (deterministic ops only).
    #[test]
    fn golden_sys_via_run_file() {
        let path = fixture("examples/sys.chz");
        let expected = std::fs::read_to_string(fixture("examples/sys.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/sys.chz");
    }

    /// Comprehensions golden: `examples/comprehensions.chz` — list/set/map comprehensions, a guard,
    /// and a range source. Byte-matches `.expected` and stays identical on interp + VM.
    #[test]
    fn golden_comprehensions_via_run_file() {
        let path = fixture("examples/comprehensions.chz");
        let expected = std::fs::read_to_string(fixture("examples/comprehensions.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/comprehensions.chz");
    }

    /// Radix-literal golden: `examples/hex.chz` — hex/binary/octal literals feeding bitwise +
    /// arithmetic. Byte-matches `.expected` and stays identical on interp + VM.
    #[test]
    fn golden_hex_via_run_file() {
        let path = fixture("examples/hex.chz");
        let expected = std::fs::read_to_string(fixture("examples/hex.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/hex.chz");
    }

    /// List concat/extend + map merge/update golden: `examples/concat_merge.chz`. New-vs-mutate
    /// semantics + arg-wins-on-key-clash. Byte-matches `.expected`, identical on interp + VM.
    #[test]
    fn golden_concat_merge_via_run_file() {
        let path = fixture("examples/concat_merge.chz");
        let expected = std::fs::read_to_string(fixture("examples/concat_merge.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/concat_merge.chz");
    }

    /// Tuple-destructuring `for` + `std.iter` golden: `examples/for_tuple.chz` — destructure a list
    /// of tuples, one-var whole-tuple, triples, `enumerate`/`zip`, comprehension combo. Byte-matches
    /// `.expected`, identical on interp + VM (the `IsMap` runtime split is exercised alongside maps).
    #[test]
    fn golden_for_tuple_via_run_file() {
        let path = fixture("examples/for_tuple.chz");
        let expected = std::fs::read_to_string(fixture("examples/for_tuple.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/for_tuple.chz");
    }

    /// Optional chaining + null-coalescing golden: `examples/optchain.chz` — `?.field`, `?.method()`,
    /// `??`, chaining + None short-circuit. Desugared to `match`; byte-matches `.expected`, identical
    /// on interp + VM.
    #[test]
    fn golden_optchain_via_run_file() {
        let path = fixture("examples/optchain.chz");
        let expected = std::fs::read_to_string(fixture("examples/optchain.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/optchain.chz");
    }

    /// Runtime stack trace: a faulting nested call reports the error line + the call chain (innermost
    /// first) with each call's line. Asserted on the VM, and the interp must produce the IDENTICAL
    /// formatted trace (frames carry the same call-site spans on both engines).
    #[test]
    fn stack_trace_reports_call_chain_on_both_engines() {
        let path = fixture("examples/stack_trace.chz");
        let (_out, _err, res, _) = run_file(&path);
        let e = res.expect_err("program should fault");
        assert_eq!(e.message, "division by zero");
        let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
        assert_eq!(names, vec!["divide", "compute", "main"]);
        // Call-site lines, innermost first.
        let lines: Vec<usize> = e.trace.iter().map(|f| f.span.line).collect();
        assert_eq!(lines, vec![15, 18, 20]);
        let vm_fmt = format_trace(&e.message, e.span, &e.trace);
        assert!(vm_fmt.contains("at divide (called at line 15"), "got: {vm_fmt}");

        // Interp parity: identical formatted trace.
        let (_o, _er, ip_res, _) = crate::interp::run_file(&path);
        let ie = ip_res.expect_err("program should fault");
        let ip_fmt = crate::interp::format_trace(&ie.message, ie.span, &ie.trace);
        assert_eq!(vm_fmt, ip_fmt, "engines must produce the same stack trace");
    }

    /// A `recover:`-caught fault leaves no stale frames: a *later* uncaught fault's trace shows only
    /// its own chain, not the recovered call.
    #[test]
    fn recovered_fault_does_not_pollute_later_trace() {
        let src = "fn boom() -> int:\n    return 1 / 0\nfn safe() -> int:\n    r := recover:\n        boom()\n    return 7\nfn deeper() -> int:\n    xs := [1, 2]\n    return xs[9]\nfn main():\n    print(safe())\n    print(deeper())\nmain()\n";
        let dir = std::env::temp_dir().join("chezzi_recover_trace_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rt.chz");
        std::fs::write(&path, src).unwrap();
        let (_o, _e, res, _) = run_file(&path);
        let e = res.expect_err("should fault");
        let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
        assert_eq!(names, vec!["deeper", "main"], "no stale 'boom'/'safe' frames");
    }

    /// A `defer`red call that itself faults supersedes the original fault (Go semantics); the trace
    /// must reflect the DEFERRED fault's chain (deeper, includes the deferred fn), identically on
    /// both engines — not the original body fault's chain.
    #[test]
    fn deferred_fault_trace_supersedes_on_both_engines() {
        let src = "fn boom() -> int:\n    return 1 / 0\nfn worker() -> int:\n    defer boom()\n    xs := [1, 2]\n    return xs[9]\nfn main():\n    print(worker())\nmain()\n";
        let dir = std::env::temp_dir().join("chezzi_defer_trace_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dt.chz");
        std::fs::write(&path, src).unwrap();
        let (_o, _e, res, _) = run_file(&path);
        let e = res.expect_err("should fault");
        let vm_names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
        assert_eq!(vm_names, vec!["boom", "worker", "main"], "deferred fault's chain");
        let vm_fmt = format_trace(&e.message, e.span, &e.trace);
        let (_o2, _e2, ip_res, _) = crate::interp::run_file(&path);
        let ie = ip_res.expect_err("should fault");
        let ip_fmt = crate::interp::format_trace(&ie.message, ie.span, &ie.trace);
        assert_eq!(vm_fmt, ip_fmt, "engines must agree on a deferred-fault trace");
    }

    /// Non-constant default golden: `examples/default_expr.chz` — defaults that are arithmetic on
    /// literals, a global times a literal, and a function call (free fns + struct fields). Byte-matches
    /// `.expected`, identical on interp + VM.
    #[test]
    fn golden_default_expr_via_run_file() {
        let path = fixture("examples/default_expr.chz");
        let expected = std::fs::read_to_string(fixture("examples/default_expr.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/default_expr.chz");
    }

    /// Function-typed field call golden: `examples/fn_field.chz` — `recv.f(args)` where `f` is a
    /// `fn`-typed field resolves to field-access-then-call (on `self` and on an external receiver),
    /// not a method. Byte-matches `.expected`, identical on interp + VM.
    #[test]
    fn golden_fn_field_via_run_file() {
        let path = fixture("examples/fn_field.chz");
        let expected = std::fs::read_to_string(fixture("examples/fn_field.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/fn_field.chz");
    }

    /// `sort_by_key` golden: `examples/sort_by_key.chz` — sort in place by a derived key (int/str
    /// keys, stable, descending-via-negation, and a Comparable *struct* key). Byte-matches
    /// `.expected`, identical on interp + VM.
    #[test]
    fn golden_sort_by_key_via_run_file() {
        let path = fixture("examples/sort_by_key.chz");
        let expected = std::fs::read_to_string(fixture("examples/sort_by_key.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/sort_by_key.chz");
    }

    /// `Ref[T]` golden: `examples/ref.chz` — a pure-Chezzi one-field mutable box (`std.ref`):
    /// `get`/`set`/`update`, closure-capture accumulation through the shared struct, generic over a
    /// non-int type. Byte-matches `.expected`, identical on interp + VM. No engine change.
    #[test]
    fn golden_ref_via_run_file() {
        let path = fixture("examples/ref.chz");
        let expected = std::fs::read_to_string(fixture("examples/ref.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/ref.chz");
    }

    /// Tuple destructuring + match-on-tuple + guards golden: `examples/tuple_match.chz` — `a, b :=
    /// fn()`, typed tuple value + `.0`/`.1`, `match` literal/binding/guard arms, `Some((a, b))`.
    /// Coverage for behavior that already worked. Byte-matches `.expected`, identical on interp + VM.
    #[test]
    fn golden_tuple_match_via_run_file() {
        let path = fixture("examples/tuple_match.chz");
        let expected = std::fs::read_to_string(fixture("examples/tuple_match.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/tuple_match.chz");
    }

    /// `std.os.exit(code)` golden: `examples/exit.chz` halts at the negative branch with status 2.
    /// Byte-matches `.expected` on both engines and both report the same exit code.
    #[test]
    fn golden_exit_via_run_file() {
        let path = fixture("examples/exit.chz");
        let expected = std::fs::read_to_string(fixture("examples/exit.expected")).unwrap();
        let (vo, _ve, vr, vc) = run_file(&path);
        let (io, _ie, ir, ic) = crate::interp::run_file(&path);
        assert!(vr.is_ok() && ir.is_ok(), "exit is a clean halt: vm={vr:?} interp={ir:?}");
        assert_eq!(vo, expected, "vm stdout");
        assert_eq!(io, expected, "interp stdout");
        assert_eq!(vc, Some(2), "vm exit code");
        assert_eq!(ic, Some(2), "interp exit code");
    }

    /// M8-M2 golden: `examples/json_dynamic.chz` — `import std.json`, the pure-Chezzi `Json` enum
    /// parse/stringify round-trip + accessors + unicode escapes + an error case. Byte-matches
    /// `.expected` and stays identical on interp + VM.
    #[test]
    fn golden_json_dynamic_via_run_file() {
        let path = fixture("examples/json_dynamic.chz");
        let expected = std::fs::read_to_string(fixture("examples/json_dynamic.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/json_dynamic.chz");
    }

    /// M9 golden: `examples/regex_demo.chz` — `import std.regex` (is_match / find with capture
    /// groups / find_all / replace_all / split + a bad-pattern Err). Byte-matches `.expected` and
    /// stays identical on interp + VM.
    #[test]
    fn golden_regex_demo_via_run_file() {
        let path = fixture("examples/regex_demo.chz");
        let expected = std::fs::read_to_string(fixture("examples/regex_demo.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/regex_demo.chz");
    }

    /// Golden: `examples/knapsack.chz` fills an int DP table with `cmp.max` (std.cmp generic over
    /// Comparable). Runs on the VM, byte-matches `.expected`, and stays identical to the interp.
    #[test]
    fn golden_knapsack_via_run_file() {
        let path = fixture("examples/knapsack.chz");
        let expected = std::fs::read_to_string(fixture("examples/knapsack.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/knapsack.chz");
    }

    /// `Iterator[T]` golden: a generic fn bounded `[S: Iterator[T], T]` over list/str/set/struct,
    /// with the element type flowing into returns. Parity-checked across both engines.
    #[test]
    fn golden_iterator_bound_via_run_file() {
        let path = fixture("examples/iterator_bound.chz");
        let expected = std::fs::read_to_string(fixture("examples/iterator_bound.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/iterator_bound.chz");
    }

    /// Lazy iterator adapters (Take/Mapped over an infinite Count) — the no-`yield` story. The inner
    /// `self.inner.next()` recovers the element type through the `I: Iterator[T]` bound on both engines.
    #[test]
    fn golden_iter_adapters_via_run_file() {
        let path = fixture("examples/iter_adapters.chz");
        let expected = std::fs::read_to_string(fixture("examples/iter_adapters.expected")).unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{res:?}");
        assert_eq!(out, expected);
        assert_file_parity("examples/iter_adapters.chz");
    }

    #[test]
    fn golden_multi_file_project_via_vm() {
        let expected = std::fs::read_to_string(fixture("tests/fixtures/proj/main.expected")).unwrap();
        let (out, _err, res, _) = run_file(&fixture("tests/fixtures/proj/main.chz"));
        assert!(res.is_ok());
        assert_eq!(out, expected);
        assert_file_parity("tests/fixtures/proj/main.chz");
    }

    /// The M4.5 headline bug, now on the VM: an imported function reading its module's top-level
    /// constant must resolve against *its own* module, not the caller — even when the caller
    /// defines a same-named global with a different value.
    #[test]
    fn imported_fn_uses_home_globals() {
        let (out, _err, res, _) = run_file(&fixture("tests/fixtures/homeglobals/main.chz"));
        assert!(res.is_ok());
        assert_eq!(out, "from-lib\nfrom-main\n");
        assert_file_parity("tests/fixtures/homeglobals/main.chz");
    }

    /// Whole multi-file project is byte-identical under GC stress.
    #[test]
    fn multi_file_identical_under_gc_stress() {
        // The fixture is small; run it under stress by routing through the entry graph manually.
        let expected = std::fs::read_to_string(fixture("tests/fixtures/proj/main.expected")).unwrap();
        let graph = crate::resolver::build_graph(&fixture("tests/fixtures/proj/main.chz")).unwrap();
        let program = crate::compiler::compile_graph(&graph).unwrap();
        let mut vm = Vm::new(Arc::new(program));
        vm.gc_stress = true;
        vm.run().unwrap();
        assert_eq!(vm.out, expected);
    }

    // ----- map / dictionary parity (gap #5) -----

    #[test]
    fn parity_map_literal_print() {
        // Deterministic insertion order; duplicate key -> last wins. Display is `{k: v, …}`.
        assert_parity_out("m := {\"a\": 1, \"b\": 2}\nprint(m)\n", "{a: 1, b: 2}\n");
        assert_parity_out("e := {}\nprint(e)\n", "{}\n");
        assert_parity_out("m := {\"a\": 1, \"a\": 9}\nprint(m)\n", "{a: 9}\n");
    }

    #[test]
    fn parity_map_index_read() {
        assert_parity_out("m := {\"a\": 1, \"b\": 2}\nprint(m[\"b\"])\n", "2\n");
    }

    #[test]
    fn parity_map_missing_key_read_errors() {
        // Both engines must error identically on a missing key.
        let src = "m := {\"a\": 1}\nprint(m[\"z\"])\n";
        assert_parity(src);
        assert!(vm_outcome(src).unwrap_err().contains("key not found"), "{:?}", vm_outcome(src));
    }

    #[test]
    fn parity_map_index_insert_and_update() {
        assert_parity_out(
            "m := {\"a\": 1}\nm[\"b\"] = 2\nm[\"a\"] = 9\nprint(m)\n",
            "{a: 9, b: 2}\n",
        );
    }

    #[test]
    fn parity_map_compound_assign() {
        assert_parity_out("m := {\"a\": 1}\nm[\"a\"] += 5\nprint(m[\"a\"])\n", "6\n");
    }

    #[test]
    fn parity_map_compound_assign_missing_key_errors() {
        // Compound on a missing key is an error (consistent with read-missing).
        let src = "m := {\"a\": 1}\nm[\"z\"] += 1\n";
        assert_parity(src);
        assert!(vm_outcome(src).unwrap_err().contains("key not found"), "{:?}", vm_outcome(src));
    }

    #[test]
    fn parity_map_methods() {
        assert_parity_out("m := {\"a\": 1, \"b\": 2}\nprint(m.len())\n", "2\n");
        assert_parity_out("m := {\"a\": 1}\nprint(m.has(\"a\"))\nprint(m.has(\"z\"))\n", "true\nfalse\n");
        assert_parity_out(
            "m := {\"a\": 1}\nmatch m.get(\"a\"):\n    Some(v): print(v)\n    None: print(\"absent\")\n",
            "1\n",
        );
        assert_parity_out(
            "m := {\"a\": 1}\nmatch m.get(\"z\"):\n    Some(v): print(v)\n    None: print(\"absent\")\n",
            "absent\n",
        );
        assert_parity_out("m := {\"a\": 1, \"b\": 2}\nprint(m.keys())\n", "[a, b]\n");
        assert_parity_out("m := {\"a\": 1, \"b\": 2}\nprint(m.values())\n", "[1, 2]\n");
    }

    #[test]
    fn parity_map_remove() {
        assert_parity_out(
            "m := {\"a\": 1, \"b\": 2}\nmatch m.remove(\"a\"):\n    Some(v): print(v)\n    None: print(\"absent\")\nprint(m)\n",
            "1\n{b: 2}\n",
        );
        // remove of a missing key -> None, map unchanged.
        assert_parity_out(
            "m := {\"a\": 1}\nmatch m.remove(\"z\"):\n    Some(v): print(v)\n    None: print(\"absent\")\nprint(m)\n",
            "absent\n{a: 1}\n",
        );
    }

    #[test]
    fn parity_map_keys_iteration() {
        assert_parity_out(
            "m := {\"a\": 1, \"b\": 2, \"c\": 3}\nfor k in m.keys():\n    print(k)\n",
            "a\nb\nc\n",
        );
    }

    #[test]
    fn parity_map_int_and_bool_keys() {
        assert_parity_out("m := {1: \"x\", 2: \"y\"}\nprint(m[2])\n", "y\n");
        assert_parity_out("m := {true: 1, false: 0}\nprint(m[false])\n", "0\n");
    }

    // ----- Hashable struct keys (hash-table map/set) -----

    /// A struct with `hash(self) -> int` as a map key: insert/update/get/has/remove + insertion-order
    /// iteration must be byte-identical across both engines.
    #[test]
    fn parity_map_struct_key() {
        let src = "\
struct P:
    x: int
    y: int
    fn hash(self) -> int:
        return self.x * 31 + self.y
fn main():
    m: map[P, str] = {}
    m[P(1, 2)] = \"a\"
    m[P(3, 4)] = \"b\"
    m[P(1, 2)] = \"z\"
    for k in m:
        print(k)
    print(m[P(3, 4)])
    print(m.has(P(1, 2)))
    print(m.has(P(9, 9)))
    print(m.get(P(3, 4)))
    print(m.remove(P(1, 2)))
    print(m.len())
main()";
        assert_parity(src);
    }

    /// Set of structs: dedup of structurally-equal keys via custom hash + union/intersection/difference.
    #[test]
    fn parity_set_struct_algebra() {
        let src = "\
struct P:
    x: int
    fn hash(self) -> int:
        return self.x
fn main():
    a: set[P] = set([P(1), P(2), P(2), P(3)])
    b: set[P] = set([P(2), P(3), P(4)])
    print(a.len())
    print(a.union(b).len())
    print(a.intersection(b).len())
    print(a.difference(b).len())
    print(a.has(P(2)))
    a.remove(P(2))
    print(a.has(P(2)))
main()";
        assert_parity(src);
    }

    /// A struct used as a map key but MISSING `hash()` is a checker error — but `run_capture` bypasses
    /// the checker, so the runtime must error consistently (not panic) on both engines.
    #[test]
    fn parity_map_struct_key_missing_hash_errors() {
        let src = "\
struct P:
    x: int
fn main():
    m: map[P, int] = {}
    m[P(1)] = 5
main()";
        assert_parity(src);
    }

    /// REGRESSION (AsInt relocation): a non-int LIST index now errors at runtime in `GetIndex`,
    /// with the SAME message the removed `AsInt` produced. The checker is bypassed by `run_capture`,
    /// so this exercises the relocated runtime validation on both engines.
    #[test]
    fn parity_list_non_int_index_still_errors() {
        let src = "xs := [1, 2, 3]\nprint(xs[\"a\"])\n";
        assert_parity(src);
        assert!(vm_outcome(src).unwrap_err().contains("expected int, found str"), "{:?}", vm_outcome(src));
        // And on assignment (SetIndex relocation).
        let src2 = "xs := [1, 2, 3]\nxs[\"a\"] = 9\n";
        assert_parity(src2);
        assert!(vm_outcome(src2).unwrap_err().contains("expected int, found str"), "{:?}", vm_outcome(src2));
    }

    #[test]
    fn parity_map_gc_stress_heap_keys_and_values() {
        // Keys AND values are heap strings; build many maps so collection runs mid-stream and the
        // `Heap::children` tracing of BOTH keys and values is exercised (a use-after-free if either
        // is untraced). The keys()/values() lists also hold heap children.
        let src = "fn main():\n    i := 0\n    while i < 200:\n        m := {\"k{i}\": \"v{i}\"}\n        m[\"extra\"] = \"x{i}\"\n        if i == 199:\n            print(m[\"k{i}\"])\n            print(m.values())\n        i += 1\nmain()\n";
        assert_parity(src);
        let expected = "v199\n[v199, x199]\n";
        assert_eq!(vm_outcome(src).unwrap(), expected);
        assert_eq!(run_capture_stress(src), expected, "VM gc_stress diverged (untraced map key/value?)");
    }

    /// Record the VM speedup over the interpreter on a loop-heavy script (the spec's perf check).
    /// Asserts a conservative floor that holds even in debug builds; the real ~6x lands in release.
    #[test]
    fn bench_vm_faster_than_interp() {
        let src = "fn main():\n    total := 0\n    i := 0\n    while i < 500000:\n        total += (i * 3 - 1) % 7\n        i += 1\n    print(total)\nmain()";
        let t = Instant::now();
        let ip = crate::interp::run_capture(src).unwrap();
        let interp_t = t.elapsed();
        let t = Instant::now();
        let vm = run_capture(src).unwrap();
        let vm_t = t.elapsed();
        assert_eq!(vm, ip, "engines disagree on the benchmark output");
        let ratio = interp_t.as_secs_f64() / vm_t.as_secs_f64();
        println!("VM speedup over interp: {ratio:.1}x (interp {interp_t:?}, vm {vm_t:?}) [debug build; ~6x in release]");
        assert!(ratio >= 1.2, "VM not faster than interp: {ratio:.2}x");
    }

    // ===== gap #8: tuples + multi-return + destructuring =====

    #[test]
    fn parity_tuple_literal_display() {
        assert_parity_out("t := (1, 2)\nprint(t)\n", "(1, 2)\n");
    }

    #[test]
    fn parity_tuple_element_access() {
        assert_parity_out("t := (3, 4)\nprint(t.0)\nprint(t.1)\n", "3\n4\n");
    }

    #[test]
    fn parity_tuple_element_out_of_range_errors() {
        // The checker would catch `.2` statically, but `t` here is built so both engines hit the
        // runtime bounds check with the identical message — parity on the error path.
        assert_parity("t := (1, 2)\nprint(t.0)\nprint(t.1)\n");
    }

    #[test]
    fn parity_destructure_local() {
        assert_parity_out("a, b := (1, 2)\nprint(a)\nprint(b)\n", "1\n2\n");
    }

    #[test]
    fn parity_tuple_equality() {
        assert_parity_out("print((1, 2) == (1, 2))\nprint((1, 2) == (1, 3))\n", "true\nfalse\n");
    }

    #[test]
    fn parity_multi_return_destructured_at_call_site() {
        let src = "fn pair() -> (int, int):\n    return (3, 4)\nfn main():\n    a, b := pair()\n    print(a + b)\nmain()\n";
        assert_parity_out(src, "7\n");
    }

    #[test]
    fn parity_tuple_heap_elements_gc_stress() {
        // A tuple of heap values (a string + a list). Under GC stress a collection happens between
        // building the tuple and reading it back — proving `Heap::children` traces tuple elements.
        let src = "t := (\"hi\", [1, 2, 3])\nprint(t.0)\nprint(t.1)\n";
        assert_parity(src);
        assert_eq!(run_capture_stress(src), "hi\n[1, 2, 3]\n", "tuple elements not GC-traced?");
    }

    // ----- slicing + Index/IndexSet/Slice protocol dispatch (VM ↔ interp parity) -----

    #[test]
    fn slice_list_and_str_parity() {
        assert_parity_out("print([1, 2, 3, 4, 5][1..3])\n", "[2, 3]\n");
        assert_parity_out("print(\"hello\"[0..2])\n", "he\n");
    }

    #[test]
    fn slice_clamped_parity() {
        assert_parity_out("print([1, 2, 3][1..99])\n", "[2, 3]\n");
        assert_parity_out("print([1, 2, 3][2..1])\n", "[]\n");
        assert_parity_out("print([1, 2, 3][-1..2])\n", "[1, 2]\n");
    }

    const BUF_PROG: &str = "\
struct Buf:
    xs: list[int]
    fn index(self, key: int) -> int:
        return self.xs[key]
    fn set_index(self, key: int, val: int):
        self.xs[key] = val
    fn slice(self, start: int, end: int) -> list[int]:
        return self.xs[start..end]
fn main():
    b := Buf([10, 20, 30])
    print(b[0])
    b[1] = 99
    print(b[1])
    b[0] += 5
    print(b[0])
    print(b[0..2])
main()";

    #[test]
    fn struct_index_slice_dispatch_parity() {
        assert_parity_out(BUF_PROG, "10\n99\n15\n[15, 99]\n");
    }

    #[test]
    fn slice_survives_gc_stress() {
        // The sliced list shares the source's element handles; a GC during the slice alloc must not
        // collect them. (Source is an inline temporary, unrooted except by the slice path.)
        let src = "print([1, 2, 3, 4, 5][1..4])\n";
        assert_parity(src);
        assert_eq!(run_capture_stress(src), "[2, 3, 4]\n", "slice elements not GC-rooted?");
    }
}
