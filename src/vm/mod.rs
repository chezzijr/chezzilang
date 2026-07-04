//! Bytecode stack VM (M5) — the Phase-2 execution path. Runs the [`Program`] produced by the
//! compiler, reproducing the tree-walk interpreter's semantics byte-for-byte (golden/parity tests
//! cross-check the two engines). M5a: handle-addressed values, no collector yet (the mark-sweep
//! GC lands in M5b).

mod blocking_pool;
pub mod chzstr;
pub mod core;
mod fxhash;
pub mod heap;
pub mod op;
mod poller;
mod pool;
mod timer;
pub mod value;
pub mod wire;

use core::{
    AtomicCore, ChannelCore, ExecutorCore, ListenerCore, RwSharedCore, SharedCore, SocketCore,
};
use heap::{Heap, MapData, Obj, SetData};
use op::{CapEntry, CapSrc, NO_IC, Op, Program, ProtoId, TID_NONE, WaitMeta};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
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
        RunError {
            message: e.message,
            span: e.span,
            trace,
        }
    }
    fn plain(e: RuntimeError) -> Self {
        RunError {
            message: e.message,
            span: e.span,
            trace: Vec::new(),
        }
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime error ({}): {}", self.span, self.message)
    }
}

/// When rendering a stack trace, after run-collapsing show at most this many collapsed lines from
/// the head (innermost — closest to the fault) and tail (outermost — includes `main`); the middle is
/// elided. Bounds deep non-recursive chains. Mirrored byte-identically in `interp::format_trace`.
const TRACE_HEAD: usize = 10;
const TRACE_TAIL: usize = 10;

/// Render a runtime error plus its stack trace for the CLI: the error line, then one indented
/// `  at <function> (<call site>)` line per frame, innermost first. Shared by both engines' drivers.
///
/// Two bounding transforms keep an infinite-recursion fault from flooding ~10_001 lines (gap #8):
/// (1) runs of consecutive frames with the SAME function name collapse to the run's innermost `at`
/// line plus a `  … (× N more identical frames) …` marker when the run length N>1; (2) if the
/// collapsed line list still exceeds `TRACE_HEAD + TRACE_TAIL`, the head and tail collapsed lines are
/// kept and the middle replaced by a `  … (M frames elided) …` marker. Both transforms are no-ops on
/// small traces with distinct names, so existing exact-trace goldens are unchanged.
pub fn format_trace(message: &str, span: Span, trace: &[TraceFrame]) -> String {
    let mut s = format!("runtime error ({span}): {message}");
    // (1) Collapse consecutive same-name runs into one entry: the run's innermost `at` line plus an
    // optional `× N` marker (kept in the SAME entry so the cap below can never orphan the marker).
    let mut entries: Vec<String> = Vec::new();
    let mut i = 0;
    while i < trace.len() {
        let frame = &trace[i];
        let mut j = i + 1;
        while j < trace.len() && trace[j].function == frame.function {
            j += 1;
        }
        let mut entry = format!("  at {} (called at {})", frame.function, frame.span);
        let run = j - i;
        if run > 1 {
            entry.push_str(&format!("\n  … (× {} more identical frames) …", run - 1));
        }
        entries.push(entry);
        i = j;
    }
    // (2) Cap the collapsed entries: keep head + tail entries, elide the middle. Capping whole
    // entries (not raw lines) keeps each `× N` marker attached to its `at` line across the boundary.
    if entries.len() > TRACE_HEAD + TRACE_TAIL {
        let elided = entries.len() - TRACE_HEAD - TRACE_TAIL;
        let tail_start = entries.len() - TRACE_TAIL;
        for entry in &entries[..TRACE_HEAD] {
            s.push('\n');
            s.push_str(entry);
        }
        s.push_str(&format!("\n  … ({elided} frames elided) …"));
        for entry in &entries[tail_start..] {
            s.push('\n');
            s.push_str(entry);
        }
    } else {
        for entry in &entries {
            s.push('\n');
            s.push_str(entry);
        }
    }
    s
}

/// Maximum user-function call depth — mirrors the interpreter, so infinite recursion is a clean
/// runtime error rather than a host stack overflow.
const MAX_CALL_DEPTH: usize = 10_000;

/// Maximum structural-recursion depth for value display / equality — a cyclic data structure (e.g.
/// a struct with a `List[Self]` field forming a cycle) would otherwise recurse unbounded on the
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

/// Stack size for the VM thread (matched to the interpreter's [`crate::interp::INTERP_STACK_BYTES`]):
/// the VM recurses on the host stack when a builtin/method re-enters the dispatch loop (e.g. a `str`
/// method re-entering via `run_proto`), so a large dedicated stack decouples the call-depth limit from
/// the caller's thread. Co-tuned with `MAX_CALL_DEPTH` (10_000) so the depth guard fires *before* the
/// host stack overflows: the recursive frame here is `run_until` (one per call-depth level), so a new
/// dispatch arm that grows that frame eats into the margin. Sized at 384 MiB (up from 256 MiB) to keep
/// comfortable headroom for per-arm growth in **debug** builds — debug frames are far larger than
/// release, and the depth-guard test (`self_referential_stringable_hits_depth_limit`) runs in debug.
const VM_STACK_BYTES: usize = 384 * 1024 * 1024;

/// Configured worker count for the M:N OS-thread engine. `0` = auto (size to
/// [`std::thread::available_parallelism`]). Set once at startup from `--threads=N` /
/// `CHEZZI_THREADS` (see `main::cmd_run`), BEFORE any `parallel:` join runs — the process-wide pool
/// ([`pool`]) is a `OnceLock` created lazily on first use, so a later store would not resize it.
static WORKER_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

/// Override the M:N engine's worker count. `0` restores auto (= `available_parallelism()`). Must be
/// called before the first parallel run; see [`WORKER_OVERRIDE`].
pub fn set_worker_count(n: usize) {
    WORKER_OVERRIDE.store(n, Ordering::Relaxed);
}

/// The effective M:N worker count: the configured override, or `available_parallelism()` when unset
/// (`0`). Always `>= 1`. Read by the pool size, the scheduler's `nworkers`, and the eager-nursery
/// gate so all three agree.
pub fn worker_count() -> usize {
    match WORKER_OVERRIDE.load(Ordering::Relaxed) {
        0 => std::thread::available_parallelism()
            .map(|x| x.get())
            .unwrap_or(1)
            .max(1),
        n => n.max(1),
    }
}

/// One activation record.
// `Clone` is only used to snapshot a generator's suspended frames (experimental); the hot call
// paths never clone a frame.
#[derive(Clone)]
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

/// Experimental generators — a generator's private execution context. While the generator runs its
/// frames/stack are live in the `Vm`; between `.next()` calls they are parked here so it resumes
/// exactly where it suspended. Base/`cur_base` indices are relative to this private `stack` (the
/// generator always runs from a base-0 stack), so resuming needs no rebasing.
#[derive(Clone, Default)]
struct GenCtx {
    frames: Vec<CallFrame>,
    stack: Vec<Value>,
    call_depth: usize,
    cur_base: usize,
    handlers: Vec<Handler>,
}

/// Experimental generators — lifecycle of a generator object.
#[derive(Clone)]
enum GenState {
    /// Created but not yet started: holds the call args until the first `.next()` builds the frame.
    Pending(Vec<Value>),
    /// Started and suspended at a `yield`; its frames/stack live in [`GeneratorCore::ctx`].
    Suspended,
    /// Body returned / fell off the end (or faulted). Every further `.next()` yields `None`.
    Done,
}

/// Experimental generators — the heap payload of an `Obj::Generator`. A one-shot coroutine driven
/// synchronously by `.next()`: each call resumes [`Vm::generator_next`] until the next `Op::Yield`
/// (returns `Some(v)`) or the body ends (`None`, state → `Done`). VM-only (the interpreter rejects
/// `yield`).
#[derive(Clone)]
pub(crate) struct GeneratorCore {
    proto: ProtoId,
    home: GcRef,
    closure: Option<GcRef>,
    state: GenState,
    ctx: GenCtx,
}

// Hand-rolled so `CallFrame`/`Handler`/`Deferred` need not derive `Debug` (keeps the hot call
// record free of an unused derive); prints lifecycle + frame count, not the whole parked stack.
impl std::fmt::Debug for GeneratorCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match &self.state {
            GenState::Pending(_) => "Pending",
            GenState::Suspended => "Suspended",
            GenState::Done => "Done",
        };
        f.debug_struct("GeneratorCore")
            .field("proto", &self.proto)
            .field("state", &state)
            .field("parked_frames", &self.ctx.frames.len())
            .finish()
    }
}

impl GeneratorCore {
    /// GC roots held while the generator is suspended: its home + backing closure, every object on
    /// its parked stack, each parked frame's home/closure/deferred roots, and any not-yet-consumed
    /// `Pending` call args. Reachable only through the generator object, so missing one is a
    /// use-after-free. (When the generator is running, `ctx` is empty — its frames/stack are the
    /// live `Vm` roots — so this returns just home/closure + nothing from the empty ctx.)
    fn gc_roots(&self) -> Vec<GcRef> {
        let mut out = vec![self.home];
        if let Some(c) = self.closure {
            out.push(c);
        }
        if let GenState::Pending(args) = &self.state {
            out.extend(args.iter().filter_map(|v| {
                if let Value::Obj(h) = v {
                    Some(*h)
                } else {
                    None
                }
            }));
        }
        for v in &self.ctx.stack {
            if let Value::Obj(h) = v {
                out.push(*h);
            }
        }
        for f in &self.ctx.frames {
            out.push(f.home);
            if let Some(c) = f.closure {
                out.push(c);
            }
            for d in &f.deferred {
                out.extend(d.roots());
            }
        }
        out
    }
}

/// The four set operators (gap #3): `|`→union, `&`→intersection, `-`→difference,
/// `^`→symmetric-difference. Selects the algebra in `Vm::set_op` / `interp::set_op`.
#[derive(Clone, Copy)]
enum SetOp {
    Union,
    Intersection,
    Difference,
    SymmetricDifference,
}

/// A call registered by `defer`, with its receiver/arguments already evaluated. The held values are
/// GC roots while the deferred call is pending.
#[derive(Clone)]
enum Deferred {
    /// `defer f(args)` — invoke the callable value with the args (`invoke_value`).
    Call {
        callee: Value,
        args: Vec<Value>,
        span: Span,
    },
    /// `defer recv.name(args)` — dispatch the named method on the receiver.
    Method {
        recv: Value,
        name: String,
        args: Vec<Value>,
        span: Span,
    },
}

impl Deferred {
    /// The GcRefs this deferred call keeps alive (callee/receiver + arguments).
    fn roots(&self) -> impl Iterator<Item = GcRef> + '_ {
        let (head, args) = match self {
            Deferred::Call { callee, args, .. } => (callee, args),
            Deferred::Method { recv, args, .. } => (recv, args),
        };
        std::iter::once(head).chain(args.iter()).filter_map(|v| {
            if let Value::Obj(h) = v {
                Some(*h)
            } else {
                None
            }
        })
    }
}

/// A task registered by `spawn`, awaiting its nursery's join barrier (C4). The callee/receiver and
/// arguments are evaluated and deep-copied across the airlock at the `spawn` statement (Go's
/// arg-evaluation timing); the body runs at the `parallel:` dedent. Mirrors the interpreter's
/// `Task` enum — a `spawn:` block is lowered to a zero-arg closure, so it rides the `Call` variant.
/// The held values are GC roots while the task is pending (see [`Vm::collect`]).
/// `Clone` is shallow (a `Value`/`GcRef` copy — both originals and clones stay rooted), used by
/// `early_enlist_outer` to prepare workers from a copy so a non-crossable task faults BEFORE the
/// nursery is consumed (atomic enlist — charge #4).
#[derive(Clone)]
enum PendingCall {
    /// `spawn f(args)` (or a `spawn:` block, lowered to a zero-arg closure) — invoke the callable.
    Call {
        callee: Value,
        args: Vec<Value>,
        span: Span,
    },
    /// `spawn recv.name(args)` — dispatch the named method on the receiver.
    Method {
        recv: Value,
        name: String,
        args: Vec<Value>,
        span: Span,
    },
}

impl PendingCall {
    /// The GcRefs this pending task keeps alive (callee/receiver + arguments).
    fn roots(&self) -> impl Iterator<Item = GcRef> + '_ {
        let (head, args) = match self {
            PendingCall::Call { callee, args, .. } => (callee, args),
            PendingCall::Method { recv, args, .. } => (recv, args),
        };
        std::iter::once(head).chain(args.iter()).filter_map(|v| {
            if let Value::Obj(h) = v {
                Some(*h)
            } else {
                None
            }
        })
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
    const EMPTY: IcCell = IcCell {
        idx: u32::MAX,
        tid: TID_NONE,
    };
}

/// M19 Phase 6 — one *way* of a method-call inline cache. `tid` is the struct layout id the cached
/// dispatch was resolved for (the sole liveness gate, like [`IcCell`]); a hit requires
/// `tid != TID_NONE && tid == recv.tid`, so an empty way or a different receiver type re-resolves.
/// `proto` is the resolved method body (program-global, stable across heaps); `module_idx` recovers
/// the method's home module from the *current* heap's `module_objs` on the fast path — held as an
/// index, NOT a `GcRef`, so the way stays heap-independent (invisible to GC / snapshots / `swap_ctx`,
/// exactly like the field IC). Module-member / core-type calls never fill a way (they don't match
/// the `Obj::Struct` guard) and so always take the slow path.
#[derive(Clone, Copy)]
struct MethodIcCell {
    tid: u32,
    proto: ProtoId,
    module_idx: u32,
}

impl MethodIcCell {
    const EMPTY: MethodIcCell = MethodIcCell {
        tid: TID_NONE,
        proto: 0,
        module_idx: 0,
    };
}

/// Number of ways in the polymorphic method-call IC. A bounded-megamorphic site (e.g. a
/// `List[Shape]` walked at one `.area()` call across N≤4 distinct struct types) keeps every receiver
/// type cached so each call HITS a way and flattens — no monomorphic-refill thrash. Four covers the
/// common protocol fan-out; a 5th+ distinct type tips the site sticky-generic (see [`MethodIcSite`]).
const METHOD_IC_WAYS: usize = 4;

/// M19 — an N-way *polymorphic* method-call inline cache, one per `CallMethod` call site (indexed by
/// the baked `ic` id). Generalises the old single [`MethodIcCell`] to [`METHOD_IC_WAYS`] tid-keyed
/// ways with the binop quickening's sticky-deopt discipline: fill the next free way on a miss; once
/// all ways are occupied AND a further distinct `tid` arrives, latch `sticky` so the site stops
/// probing the ways and goes straight to the (clone-free) slow path — mirroring `Q_GENERIC` (one-way,
/// never clears, so a polymorphic site never thrashes). Holds only ints (tids / proto ids / u32
/// module indices), no `GcRef`, so it stays heap-independent like `field_ic`/`method_ic`/`quicken`:
/// never snapshotted, never swapped in [`Vm::swap_ctx`]. Each way is tid+arity re-guarded on every
/// hit, so a wrong body can never dispatch.
#[derive(Clone, Copy)]
struct MethodIcSite {
    ways: [MethodIcCell; METHOD_IC_WAYS],
    /// One-way latch: set when a 5th distinct `tid` overflows a full set of ways. Once sticky, the
    /// fast-path probe is skipped entirely (every receiver type beyond the 4 cached goes slow).
    sticky: bool,
}

impl MethodIcSite {
    const EMPTY: MethodIcSite = MethodIcSite {
        ways: [MethodIcCell::EMPTY; METHOD_IC_WAYS],
        sticky: false,
    };
}

pub struct Vm {
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
    /// M19 Phase 6 / N-way poly — per-call-site method inline caches, indexed by the `ic` id baked
    /// into `CallMethod` ops (dense `0..program.method_ic_sites`). Each site is an N-way
    /// [`MethodIcSite`] holding proto ids + module indices, not `GcRef`s, so it carries no heap state:
    /// never snapshotted, never swapped in [`Vm::swap_ctx`]. Same sharing argument as `field_ic` —
    /// sequential cooperative fibers / per-worker `Vm`s, each way tid-guarded so it self-verifies.
    method_ic: Vec<MethodIcSite>,
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
    /// Cross-nursery flat scheduler (M:N) — parallel to [`Vm::nurseries`] (lockstep, swapped per-fiber):
    /// `Some(scope_id)` if this nursery was EARLY-ENLISTED into the one global sched (its sibling tasks
    /// already seeded as a scope so a *nested* nursery's owner can run them — the case-A cross-nursery
    /// wake), else `None` (the normal lazy path: tasks run + reduce at this nursery's own `JoinNursery`).
    mn_scopes: Vec<Option<usize>>,
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
    /// `wait` (§6d) — the multi-channel analogue of `suspend`: the live arm-channel handles a blocking
    /// `wait:` parked the running fiber on. Set by [`Vm::op_wait_poll`] (cooperative engine only; the
    /// M:N engine faults a blocking `wait` for now), consumed by [`Vm::run_child`], which files the
    /// fiber under every key so any sibling `send` re-runs the `WaitPoll` and re-polls. Mutually
    /// exclusive with `suspend` (a fiber parks via one or the other, never both). A VM-global like
    /// `suspend` (one fiber runs at a time).
    wait_suspend: Option<Vec<GcRef>>,
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
    /// Cross-nursery flat scheduler (M:N) — count of OUTER nurseries early-enlisted into the global
    /// sched by a nested builder but not yet reduced at their own `JoinNursery`. While > 0 the
    /// [`Vm::mn_enlist_sched`] is held alive (the enlisted scopes' slots live in it); the last
    /// enlisted-scope join clears it. Lives on the inline builder's VM (stable across the body).
    mn_enlisted: usize,
    /// Cross-nursery flat scheduler (M:N) — the global sched the INLINE builder early-enlisted OUTER
    /// nurseries into. Held SEPARATELY from [`Vm::mn`] because the inline VM's body must run with
    /// `self.mn == None` (so its `run_until` does NOT take the worker-only D3 budget-yield / recv-park
    /// paths — there is no scheduler loop driving the inline body, so a yield/park would just stall it).
    /// The owner loops run on worker SHELLS (whose `mn` is set); only the deferred enlisted-scope joins
    /// (`join_enlisted_scope`/`abort_enlisted_scope`) read this. Cleared when `mn_enlisted` hits 0.
    mn_enlist_sched: Option<Arc<MnSched>>,
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
    /// Experimental generators — transient signal: an `Op::Yield` set this, asking the generator's
    /// private `run_until` to return control to the host `.next()` call (the yielded value is on the
    /// stack top). Reset by `generator_next` before each resume; never true on the cooperative host
    /// stack, so it leaves the frozen engine byte-identical. Not swapped by `swap_ctx` (it is only
    /// ever live across the single nested `run_until` that `generator_next` drives).
    gen_yielding: bool,
    /// Experimental generators — saved HOST contexts, one per generator currently executing (LIFO for
    /// nested generators). A running generator's frames/stack live in the live `Vm` fields; the host
    /// it suspended is parked here. `collect` roots every entry so the host's object graph survives a
    /// GC triggered inside a generator body. Empty whenever no generator runs — and at every
    /// fiber-switch point, since generators run `guarded` and so never park mid-body.
    gen_host_ctx: Vec<GenCtx>,
    /// Experimental generators — handles of generators whose bodies are currently executing (LIFO).
    /// `collect` marks each so the generator object survives to have its state written back.
    active_generators: Vec<GcRef>,
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
fn wait_fd_ready(
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
struct FiberCtx {
    frames: Vec<CallFrame>,
    stack: Vec<Value>,
    call_depth: usize,
    cur_base: usize,
    handlers: Vec<Handler>,
    nurseries: Vec<Vec<PendingCall>>,
    /// Cross-nursery flat scheduler (M:N) — parallel to [`Vm::nurseries`] (lockstep): `Some(scope_id)`
    /// if this nursery was EARLY-ENLISTED into the one global sched (its sibling tasks already seeded as
    /// a scope so a *nested* nursery's owner can run them — the case-A cross-nursery wake), else `None`
    /// (the normal lazy path: tasks run + reduce at this nursery's own `JoinNursery`). When `Some`, the
    /// nursery's `tasks` vec was drained (consumed into the scope), and its `JoinNursery` reduces the
    /// recorded scope's slot sub-range instead of running the tasks — preserving the per-nursery-join
    /// flush ORDER (so three-engine parity holds for non-blocking nested spawns).
    mn_scopes: Vec<Option<usize>>,
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
    /// D2b — the fiber's stable Decision-F outcome slot. Under the cross-nursery flat scheduler this is
    /// the GLOBAL flat index into `SchedCore::slots` (= `scopes[scope_id].base_index + local_i`),
    /// assigned at nursery build / `inject`. Unused by the cooperative engine (it carries the child index).
    task_index: usize,
    /// Cross-nursery flat scheduler (M:N) — which nursery scope this fiber belongs to (indexes
    /// `SchedCore::scopes`). Independent of `task_index` (the flat slot). Drives the scope-scoped owner
    /// stop, per-scope done accounting, and per-scope cancel. Zero for the cooperative engine / the
    /// single-nursery fast path's sole scope.
    scope_id: usize,
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
    Closure {
        proto: ProtoId,
        captured: Vec<(String, WireValue)>,
        args: Vec<WireValue>,
        home: Option<usize>,
        span: Span,
    },
    Func {
        proto: ProtoId,
        args: Vec<WireValue>,
        home: Option<usize>,
        span: Span,
    },
    /// `spawn f(args)` where `f` is a first-class builtin fn value (`Obj::Builtin`) — the callee is
    /// pure code (no captures, no home), so it crosses by name and the worker re-allocs a fresh
    /// `Obj::Builtin`, mirroring `Func`. Without this arm a builtin callee hit `prepare_worker`'s
    /// reject `_` on the M:N engine only (serial/interp dispatch it directly) — a parity divergence.
    Builtin {
        name: Box<str>,
        args: Vec<WireValue>,
        span: Span,
    },
    /// `spawn recv.m(args)` (B3.3d) — the receiver + args cross by wire; dispatch resolves the method
    /// against the worker's reconstructed `module_objs` (struct methods index `module_objs[module_idx]`).
    Method {
        recv: WireValue,
        name: String,
        args: Vec<WireValue>,
        span: Span,
    },
}

impl Lowered {
    /// The spawn-site span carried on every variant — used to re-stamp a module-global snapshot fault.
    fn span(&self) -> Span {
        match self {
            Lowered::Closure { span, .. }
            | Lowered::Func { span, .. }
            | Lowered::Builtin { span, .. }
            | Lowered::Method { span, .. } => *span,
        }
    }
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
fn module_slot_pairs(
    slots: &[Value],
    index: &std::collections::HashMap<Box<str>, u32>,
) -> Vec<(String, Value)> {
    // Invariant: `index` names every slot `0..slots.len()` (the three growth paths — `run_module`
    // pre-size, `module_define` append, `set_global_slot` overwrite — keep `slots`/`index` in
    // lockstep). If that ever breaks, an unnamed hole would replay as a duplicate empty name and
    // collapse later slots in a worker, silently corrupting its globals — so fail loudly here.
    debug_assert_eq!(
        slots.len(),
        index.len(),
        "module slots/index out of lockstep — slot order would corrupt on worker fault"
    );
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
    Func {
        proto: ProtoId,
        home: Option<usize>,
    },
    /// An anonymous function + its captured environment (each capture itself a `SnapValue`).
    Closure {
        proto: ProtoId,
        captured: Vec<(String, SnapValue)>,
        home: Option<usize>,
    },
    /// An import-alias global bound to another module — replays to the worker's `module_objs[idx]`
    /// (the pre-alloced module obj, which faults its own globals lazily — no eager cascade).
    ModuleAlias(usize),
    /// A module value NOT in `module_objs` (defensive — shouldn't occur for a bound import; mirrors
    /// the `None` arm of the old `map_global_value`): replayed as a fresh, eagerly-populated module.
    ModuleInline {
        name: Box<str>,
        globals: Vec<(String, SnapValue)>,
    },
    /// A native (Rust) fn — re-allocated with the same fn pointer (`NativeFn` is `Clone`/`Send`).
    Native {
        name: Box<str>,
        func: crate::native::NativeFn,
    },
    /// A first-class universe builtin fn (`print`/`ord`/`chr`/`panic`) — SENDABLE (pure code). Carries
    /// only the name; replayed as a fresh `Obj::Builtin`.
    Builtin(Box<str>),
    /// A dynamic C-ABI FFI fn — shares its `Arc<Cffi>` to the worker (same address space; no
    /// re-dlopen). `Cffi` is `Send + Sync`, so the Arc crosses the OS-thread boundary safely.
    Cffi(Arc<crate::native::cffi::Cffi>),
    List(Vec<SnapValue>),
    Tuple(Vec<SnapValue>),
    /// An `Iterable.iter()` cursor snapshot — its items (each a `SnapValue`) plus the cursor `pos`.
    /// Rebuilt as a fresh independent cursor on replay. Reached only for a handle-bearing cursor (a
    /// cursor over closures/modules); a pure-data cursor takes the `to_wire` fast path above.
    Iter {
        items: Vec<SnapValue>,
        pos: usize,
    },
    /// M19 lever #2 — the dense `variant_id` is carried directly (the same shared `Arc<Program>` makes
    /// it meaningful on replay; carrying the id, not the name, preserves identity under name shadowing).
    Enum {
        variant_id: u32,
        payload: Vec<SnapValue>,
    },
    Struct {
        name: Box<str>,
        fields: Vec<(Box<str>, SnapValue)>,
    },
    /// A newtype wrapper — its runtime key + its (handle-bearing) inner snap. Reached only when the
    /// inner embeds a handle (a pure-data newtype takes the `to_wire` fast path above).
    NewType {
        type_key: Box<str>,
        inner: Box<SnapValue>,
    },
    /// `(cached hash, key, value)` triples — hashes are value-derived, so they carry over unchanged.
    Map(Vec<(u64, SnapValue, SnapValue)>),
    /// `(cached hash, element)` pairs.
    Set(Vec<(u64, SnapValue)>),
}

/// B3.4 — how a `--parallel` task ended, recorded in its slot. The join (`run_parallel_nursery`)
/// scans these in task order: `Done`/`Exit` flush their buffered output; the lowest-index `Exit` or
/// `Fault` propagates (an `Exit` hard-halts the parent, a `Fault` unwinds normally so an outer
/// `recover:` can catch it); `Cancelled` is swallowed (a sibling-abort, its partial output dropped).
/// The terminal (lowest-index propagating) `Fault` ALSO flushes its buffered output at its task-order
/// slot — matching the cooperative/interp oracle, which writes a faulting task's partial output before
/// the fault propagates. Higher-index racy faults and `Cancelled` still drop (no deterministic slot).
#[derive(Debug)]
enum TaskOutcome {
    /// Ran to completion. Its return value crossed the airlock; output flushed in task order.
    Done(WorkerResult),
    /// Observed the nursery cancel flag and unwound (a sibling faulted/exited first). Dropped.
    Cancelled,
    /// Called `std.os.exit(code)`. Buffered output is flushed, then the parent hard-halts with `code`.
    Exit {
        code: i32,
        out: String,
        stderr: String,
    },
    /// Faulted (runtime error or caught panic). The lowest-index fault propagates out of the join; its
    /// buffered output is flushed at its task-order slot (a Rust-panic-to-fault path may carry empty
    /// buffers — the shell buffer is not safely reachable there).
    Fault {
        err: RuntimeError,
        out: String,
        stderr: String,
    },
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

/// Cross-nursery flat scheduler — the `owner_scope` a FARMED helper / eager drainer passes to
/// [`Vm::mn_worker_loop`]/[`MnSched::take_runnable`]: it owns NO scope, so it never self-stops on a
/// scope completing — it drains the global queue until global `terminate`. Only the INLINE OWNER of a
/// nested nursery passes its real `scope_id` (returns the instant its OWN scope is done).
const SENTINEL_SCOPE: usize = usize::MAX;

impl LocalQ {
    fn new() -> Self {
        LocalQ {
            runnext: None,
            ring: std::collections::VecDeque::new(),
        }
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

/// §6d M:N `wait` (select) park — ONE blocked fiber shared across the N arm-channel buckets it parks
/// on. A `Fiber` owns its live `FiberCtx` and is NOT `Clone`, so it cannot be filed under N keys the
/// way the cooperative engine files a cheap `usize` index; instead the single fiber lives here behind
/// `Mutex<Option<Fiber>>` and a refcounted `Arc<WaitPark>` token is filed in each key's bucket. The
/// first waker to ANY key CASes `claimed` false→true; the winner `take()`s the fiber and sweeps the
/// (now stale) token out of every OTHER key's bucket by `Arc::ptr_eq` — all under one hold of the core
/// lock (serialized with [`MnSched::park_wait`]'s gap re-check), so a later `send`/`close` to a swept
/// channel can never re-wake the already-moved fiber. A single-`recv` park stays the cheaper
/// [`ParkedEntry::Recv`] 1-key case and allocates no `WaitPark`.
struct WaitPark {
    /// The single parked fiber, taken exactly once by the winning CAS. `None` after it is claimed.
    fiber: Mutex<Option<Fiber>>,
    /// Every bucket key this token was filed under — the sweep set the winner removes itself from.
    keys: Vec<usize>,
    /// Wake-once gate: the first waker to win the CAS owns the fiber; all later wakers see it set and
    /// drop their (stale) token. Distinct from `parked_n` (a fiber count, not a key count).
    claimed: AtomicBool,
}

/// An entry in a `SchedCore.parked` bucket: either a single fiber blocked on a plain `recv` (the
/// common 1-key case, byte-identical to the pre-`wait` engine) OR a refcounted token referencing the
/// one fiber blocked on a multi-channel `wait` (shared across N buckets — see [`WaitPark`]).
// The `Recv` variant inherently carries a whole `Fiber` (the unit of parked work — it must live
// somewhere while blocked), exactly like the `Take` enum above; boxing it would add an allocation on
// the hot recv-park path for no benefit. The `Wait` variant is a cheap `Arc`. The size asymmetry is
// the cost of keeping the common recv park alloc-free.
#[allow(clippy::large_enum_variant)]
enum ParkedEntry {
    Recv(Fiber),
    Wait(Arc<WaitPark>),
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

/// Cross-nursery flat scheduler (M:N) — one nursery's JOIN RECORD (Trio/Go-style: structured
/// concurrency = join bookkeeping, not a scheduler frame). The old scalar `{done,total,body_open}`
/// became a `Vec<JoinScope>` so one global `MnSched` can host every nested nursery at once. Each scope
/// owns a contiguous `base_index..base_index+total` sub-range of the FLAT `SchedCore::slots`.
struct JoinScope {
    /// This scope's offset into the flat `SchedCore::slots`: its tasks occupy `base_index..base_index+total`.
    base_index: usize,
    /// Task count for this scope. Grows via `inject` (per-connection eager spawn), exactly as the old
    /// scalar `total` did.
    total: usize,
    /// Tasks in this scope that have produced a `TaskOutcome`.
    done: usize,
    /// `true` while an EAGER nursery's body is still running (between `EnterNursery` and `JoinNursery`)
    /// and may still `inject` more tasks. While set, a transient `done == total` for this scope must NOT
    /// terminate the global sched and `is_deadlocked` is vetoed (the body is live work the sched can't
    /// see). `JoinNursery` clears it. Always `false` for a lazy (queue-at-join) nursery.
    body_open: bool,
    /// Cross-nursery flat scheduler — `true` while this scope is an EARLY-ENLISTED outer nursery still
    /// awaiting the inline builder's own `JoinNursery` (`early_enlist_outer` sets it; `join_enlisted_scope`
    /// / `abort_enlisted_scope` clear it as the builder begins draining the scope). While set, the scope's
    /// parked fibers have a live external feeder — the builder body, which may still `send`/`close`/`spawn`
    /// — so a quiesce in which EVERY incomplete scope is `awaiting_builder` is NOT a deadlock: the builder
    /// has finished all nested service and will return to the body to feed them (see
    /// `all_incomplete_awaiting_builder` + `is_deadlocked`). Always `false` for an owned/eager scope.
    awaiting_builder: bool,
    /// This scope's cancel token (the SAME `Arc` cloned onto fibers running in this scope; distinct
    /// per nursery so an inner fault cancels ONLY its scope, never an outer sibling — the structured-
    /// concurrency invariant). Read by `park`/`park_wait`'s gap re-check and the running fiber's
    /// back-edge (via the shell's re-pointed `self.cancel`) and `cancel_drain(scope_id)`.
    cancel: Arc<AtomicBool>,
}

struct SchedCore {
    /// The global overflow / seed queue. Seed + every coordinator-path requeue (deadlock flag,
    /// cancel drain) land here; per-worker requeues go to a worker's `locals[wid]` (D4c). Drained by
    /// a worker only after its own local is empty, so the global queue is the shared fallback.
    global: std::collections::VecDeque<Fiber>,
    /// Fibers parked on an empty `recv` OR a multi-channel `wait`, keyed by `ChannelCore` pointer
    /// ([`Vm::channel_core_ptr`]). A bucket holds either a `ParkedEntry::Recv(fiber)` (1-key recv park)
    /// or a `ParkedEntry::Wait(token)` (a shared [`WaitPark`] referenced from every arm channel's
    /// bucket — see [`MnSched::park_wait`]/`send_wake`). `parked_n` counts FIBERS (a wait fiber is +1
    /// regardless of how many buckets hold its token), not bucket entries.
    parked: std::collections::HashMap<usize, Vec<ParkedEntry>>,
    /// Decision-F per-task outcome slots, FLAT across every nursery scope, indexed by
    /// `Fiber::task_index` (= `scopes[scope_id].base_index + local_i`). `None` until that task ends.
    /// Kept flat (not partitioned into `JoinScope`) so `finish`/`flag_deadlock`/`inject`/`reduce` index
    /// by a single global index, minimizing churn; each scope owns a contiguous `base_index..base+total`
    /// sub-range (reduce/take operate per-scope on that sub-slice — see `take_scope_slots`).
    slots: Vec<Option<TaskOutcome>>,
    running: usize,  // fibers currently swapped into a worker (executing)
    parked_n: usize, // total fibers across every `parked` bucket
    /// Cross-nursery flat scheduler (M:N) — the per-nursery join records, replacing the old scalar
    /// `{done,total,body_open}`. ONE global `MnSched` is shared by every nested `run_mn_nursery` /
    /// eager nursery (built only by the outermost owner); each `register_scope` appends a `JoinScope`
    /// and enlists its fibers into the SAME global run queue. A fiber carries its `scope_id`; the inline
    /// owner of a scope stops when ITS OWN scope is done (scope-scoped owner stop), while it drains the
    /// global queue (so it naturally runs cross-nursery siblings — the case-A fix). `scopes.len() == 1`
    /// is the single-nursery FAST PATH (the common case + `benches/run.chz`).
    scopes: Vec<JoinScope>,
    terminate: bool, // every worker loop exits once set (all scopes done, deadlock, or os.exit/fault)
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
    /// Cross-nursery flat scheduler — every scope's tasks are done (the global terminate condition,
    /// together with `!any_body_open`). `scopes.len() == 1` is the single-nursery fast path.
    fn all_scopes_done(&self) -> bool {
        if self.scopes.len() == 1 {
            let s = &self.scopes[0];
            return s.done == s.total;
        }
        self.scopes.iter().all(|s| s.done == s.total)
    }

    /// Cross-nursery flat scheduler — any scope's eager body is still injecting (vetoes terminate +
    /// the global deadlock predicate, exactly as the old scalar `body_open` did).
    fn any_body_open(&self) -> bool {
        if self.scopes.len() == 1 {
            return self.scopes[0].body_open;
        }
        self.scopes.iter().any(|s| s.body_open)
    }

    /// Cross-nursery flat scheduler — true when EVERY still-incomplete scope is one merely awaiting the
    /// inline builder's own `JoinNursery` (early-enlisted). The builder, having finished all nested
    /// service (else a non-`awaiting_builder` scope would still be incomplete), WILL return to the body
    /// and can `send`/`close`/`spawn` to feed these parked siblings — so this quiesce is NOT a deadlock.
    /// A single-scope sched never holds an enlisted scope, so the fast path is always `false` (zero cost
    /// on the common path). (Cross-nursery flat scheduler — charges #1/#2.)
    fn all_incomplete_awaiting_builder(&self) -> bool {
        if self.scopes.len() == 1 {
            return false;
        }
        let mut any_incomplete = false;
        for s in &self.scopes {
            if s.done < s.total {
                any_incomplete = true;
                if !s.awaiting_builder {
                    return false;
                }
            }
        }
        any_incomplete
    }

    /// Cross-nursery flat scheduler — some scope has unfinished tasks (the `done < total` half of the
    /// global deadlock predicate). Fast path for the common single-nursery case.
    fn any_scope_incomplete(&self) -> bool {
        if self.scopes.len() == 1 {
            let s = &self.scopes[0];
            return s.done < s.total;
        }
        self.scopes.iter().any(|s| s.done < s.total)
    }

    /// Register a demoted fiber's channel (refcounted). Caller holds core lock A.
    fn register_demoted(&mut self, ptr: usize, core: &Arc<ChannelCore>) {
        self.demoted_chans
            .entry(ptr)
            .or_insert_with(|| (Arc::clone(core), 0))
            .1 += 1;
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
    /// Build the ONE global sched the outermost owner shares with every nested nursery. Seeds scope 0
    /// for the outermost nursery's `total` tasks + its `cancel`; nested nurseries `register_scope` more.
    /// `MnSched::cancel` keeps the OUTERMOST cancel (back-compat for the gap re-check default + the
    /// existing unit tests), but `park`/`park_wait`/`cancel_drain` use the PER-FIBER scope cancel.
    fn new(
        total: usize,
        nworkers: usize,
        cancel: Arc<AtomicBool>,
        deadlock_err: RuntimeError,
    ) -> Self {
        MnSched {
            core: Mutex::new(SchedCore {
                global: std::collections::VecDeque::new(),
                parked: std::collections::HashMap::new(),
                slots: (0..total).map(|_| None).collect(),
                running: 0,
                parked_n: 0,
                scopes: vec![JoinScope {
                    base_index: 0,
                    total,
                    done: 0,
                    body_open: false,
                    awaiting_builder: false,
                    cancel: Arc::clone(&cancel),
                }],
                terminate: false,
                demoted_chans: std::collections::HashMap::new(),
            }),
            cv: Condvar::new(),
            cancel,
            deadlock_err,
            runnable: AtomicUsize::new(0),
            locals: (0..nworkers.max(1))
                .map(|_| Mutex::new(LocalQ::new()))
                .collect(),
            steal_ctr: AtomicUsize::new(0),
            inflight: AtomicUsize::new(0),
            blocked_native: AtomicUsize::new(0),
        }
    }

    /// Cross-nursery flat scheduler — a NESTED nursery (nested `run_mn_nursery` / eager nursery) enlists
    /// into THIS one global sched. Appends a `JoinScope` whose `base_index` is the current flat
    /// `slots.len()`, extends `slots` by `total` `None`s (its contiguous sub-range), and returns the new
    /// `scope_id`. Append-only (existing scopes' `base_index` never shifts, so live fibers' `task_index`
    /// stays valid). Holds the core lock so the grow is atomic against the deadlock predicate. `total`
    /// may be 0 for an eager nursery (it grows via `inject`).
    fn register_scope(&self, total: usize, cancel: Arc<AtomicBool>) -> usize {
        let mut c = self.lock();
        let base_index = c.slots.len();
        c.slots.extend((0..total).map(|_| None));
        let scope_id = c.scopes.len();
        c.scopes.push(JoinScope {
            base_index,
            total,
            done: 0,
            body_open: false,
            awaiting_builder: false,
            cancel,
        });
        // Cross-nursery flat scheduler — a late `spawn:` into a non-outermost nursery registers a fresh
        // TRAILING scope on the HELD sched (`run_mn_nursery` held-nested branch) AFTER every prior scope
        // finished, which may already have latched global `terminate` (`finish`'s `all_scopes_done`). A
        // newly-registered scope has unfinished work (`done=0 < total`), so the sched is by definition no
        // longer "all done": un-latch `terminate` so the inline owner that will run this scope is not
        // stopped on the stale flag (`take_runnable` checks `terminate` first → `Stop` → the scope would
        // never run and `wait_for_scope` would hang). Harmless on the never-terminated nested path.
        c.terminate = false;
        scope_id
    }

    /// Register a fresh trailing scope AND seed its fibers **atomically under one core lock**. Unlike
    /// `register_scope` followed by a separate `seed` (two locks with a wide gap — `prepare_worker` runs
    /// in between), this guarantees there is never an instant where the scope is visible (`done < total`,
    /// `awaiting_builder == false`) with `runnable == 0`. That window is a false-quiesce a SENTINEL helper
    /// could read via `is_deadlocked` and fault an innocent parked outer sibling — reachable on the
    /// late-spawn-into-middle path where the inline builder driving the registration is NOT counted in
    /// `running`. Mirrors [`MnSched::inject`]'s grow+runnable atomicity contract. `workers` are already
    /// prepared (no fallible/heap work happens inside the lock); each fiber's flat `task_index` is
    /// `base_index + i`, assigned here under the lock.
    fn register_scope_seeded(&self, cancel: Arc<AtomicBool>, workers: Vec<ReadyWorker>) -> usize {
        let total = workers.len();
        let mut c = self.lock();
        let base_index = c.slots.len();
        c.slots.extend((0..total).map(|_| None));
        let scope_id = c.scopes.len();
        c.scopes.push(JoinScope {
            base_index,
            total,
            done: 0,
            body_open: false,
            awaiting_builder: false,
            cancel,
        });
        // A freshly-registered scope has unfinished work — un-latch any stale global `terminate` (see
        // `register_scope`) so the inline owner that drains it is not stopped on the stale flag.
        c.terminate = false;
        // Seed in the SAME lock: bump `runnable` and queue every fiber before the lock is dropped, so the
        // scope and its runnable fibers become visible together (no `runnable == 0` gap).
        self.runnable.fetch_add(total, Ordering::Relaxed);
        for (i, w) in workers.into_iter().enumerate() {
            c.global.push_back(w.into_fiber(base_index + i, scope_id));
        }
        scope_id
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
    /// Inject into `scope_id` (an eager nursery's scope). The eager nursery owns its OWN sched (a
    /// single scope, the LAST scope), so the new slot is the flat `slots` END — keeping the scope's
    /// `base_index..base_index+total` sub-range contiguous (the contract `reduce`/`take_scope_slots`
    /// rely on). The `debug_assert` pins that invariant: inject only ever targets the last scope, so
    /// growing it never overruns a later scope's range.
    fn inject(&self, mut fiber: Fiber, scope_id: usize) {
        debug_assert!(
            matches!(fiber.state, FiberState::Pending(_)),
            "an injected handler must be unstarted (Pending) so `run_one_fiber` runs its body via `start_task`"
        );
        let mut c = self.lock();
        debug_assert_eq!(
            scope_id,
            c.scopes.len() - 1,
            "inject only grows the LAST scope (keeps flat slots contiguous)"
        );
        fiber.task_index = c.slots.len(); // authoritative flat slot index — the slots END
        fiber.scope_id = scope_id;
        c.scopes[scope_id].total += 1;
        c.slots.push(None);
        c.global.push_back(fiber);
        self.runnable.fetch_add(1, Ordering::Relaxed);
        drop(c);
        self.cv.notify_all();
    }

    /// Per-connection spawn — mark `scope_id`'s (eager) body as still producing tasks: a transient
    /// `done == total` will not terminate it and `is_deadlocked` is vetoed, so farmed workers park
    /// waiting for the next `inject` instead of exiting. Called at `EnterNursery`.
    fn open_body(&self, scope_id: usize) {
        self.lock().scopes[scope_id].body_open = true;
    }

    /// Per-connection spawn — `scope_id`'s eager body reached `JoinNursery`: no more injections. Clear
    /// the flag and wake every worker so the run-out-of-work path can terminate (all scopes done) or
    /// fire a genuine deadlock now that the body is no longer live work.
    fn close_body(&self, scope_id: usize) {
        self.lock().scopes[scope_id].body_open = false;
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
    fn take_runnable(&self, wid: usize, tick: u64, scope_id: usize) -> Take {
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
            // Cross-nursery flat scheduler — SCOPE-SCOPED owner stop. A nested nursery's inline OWNER
            // (passed its own `scope_id`) returns `Stop` when ITS OWN scope is complete (`done == total`
            // && !body_open), EVEN while other scopes still have work — because its OS thread drains the
            // GLOBAL queue (it ran cross-nursery siblings while alive), the moment its scope is done it
            // must return so the nested `run_mn_nursery` unwinds back to its caller (an OUTER fiber whose
            // continuation may unblock the rest). It does NOT set global `terminate` (that would stop
            // farmed helpers other scopes still need). A FARMED helper passes the SENTINEL scope_id
            // (`usize::MAX`) and never self-stops — it keeps draining the global queue until global
            // `terminate` (set by `finish` only when ALL scopes are done, or by deadlock/fault/exit).
            // `body_open` (eager) holds the scope open against a transient `done == total`. Single-scope
            // fast path: an outermost owner with one scope behaves exactly like the old `done == total`.
            if scope_id != SENTINEL_SCOPE {
                let s = &c.scopes[scope_id];
                if s.done == s.total && !s.body_open {
                    self.cv.notify_all();
                    return Take::Stop;
                }
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
                let (guard, _) = self
                    .cv
                    .wait_timeout(c, SPIN_BACKOFF)
                    .unwrap_or_else(|e| e.into_inner());
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
        // A concurrent `trip()` (between `recv`'s empty-check and here) sets `done_latch` then runs
        // `close_wake`, which finds no parked fiber yet — so close the gap by re-checking the latch too
        // (a tripped done() channel is "ready", like `closed`), else a `wait:`/`recv` on a manually
        // cancelled token's `done()` strands forever under the OS-thread engine.
        let latched = core.done_latch.load(Ordering::Relaxed);
        // Cross-nursery flat scheduler — read the PARKING fiber's SCOPE cancel (not the sched's global
        // `cancel`), so an inner fault that tripped only its scope re-checks the right flag here.
        let cancelled = c.scopes[fiber.scope_id].cancel.load(Ordering::Relaxed);
        if message_waiting || closed || latched || cancelled {
            fiber.state = FiberState::Ready;
            c.global.push_back(fiber);
            self.runnable.fetch_add(1, Ordering::Relaxed); // running → ready (requeued)
            self.cv.notify_all();
        } else {
            fiber.state = FiberState::Blocked; // running → parked: runnable unchanged
            c.parked
                .entry(key)
                .or_default()
                .push(ParkedEntry::Recv(fiber));
            c.parked_n += 1;
        }
    }

    /// §6d M:N multi-channel `wait` park — the N-key generalization of [`MnSched::park`]. The running
    /// fiber blocked on a `wait` whose every arm channel was empty/live; `arms` is `(key, core)` for
    /// each live arm (captured by the worker loop while the fiber heap was live, exactly like
    /// `Disp::Park`). Holding the core lock (the SAME lock `send_wake`/`close_wake`/`cancel_drain`
    /// take), close the park gap for ALL N arms first: if ANY arm has a queued message, was closed, or
    /// cancel was tripped between the `WaitPoll` empty-poll and here, requeue the fiber `Ready` (it
    /// re-runs `WaitPoll`, re-polls source order, and takes/skips the now-ready arm) instead of
    /// parking — else strand-forever. Otherwise allocate ONE `Arc<WaitPark>` holding the fiber and file
    /// a clone of the token in every arm's bucket; `parked_n += 1` (one fiber, not N tokens). Lock
    /// order core-OUTER / channel-`q`-INNER matches `park`, so no ABBA; no `cv` notify on the park path
    /// (parking creates no runnable work — an all-parked deadlock is caught by the next `take_runnable`).
    fn park_wait(&self, arms: Vec<(usize, Arc<ChannelCore>)>, mut fiber: Fiber) {
        let mut c = self.lock();
        c.running -= 1;
        // Gap re-check for EVERY arm (mirrors `park`'s 1-key re-check): a concurrent `send`/`close`/
        // cancel to any arm must requeue, not park. Cross-nursery flat scheduler — read the parking
        // fiber's SCOPE cancel (not the sched's global `cancel`).
        let mut ready_now = c.scopes[fiber.scope_id].cancel.load(Ordering::Relaxed);
        if !ready_now {
            for (_, core) in &arms {
                let ready = {
                    let g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                    !g.queue.is_empty() || g.closed
                };
                // A tripped `done_latch` (a concurrent `trip()`) makes this arm ready, same as a queued
                // value or a close — re-check it in the gap or a `wait: tok.done()` strands forever.
                if ready || core.done_latch.load(Ordering::Relaxed) {
                    ready_now = true;
                    break;
                }
            }
        }
        if ready_now {
            fiber.state = FiberState::Ready;
            c.global.push_back(fiber);
            self.runnable.fetch_add(1, Ordering::Relaxed); // running → ready (requeued, re-polls)
            self.cv.notify_all();
            return;
        }
        fiber.state = FiberState::Blocked; // running → parked: runnable unchanged
        let keys: Vec<usize> = arms.iter().map(|(k, _)| *k).collect();
        let wp = Arc::new(WaitPark {
            fiber: Mutex::new(Some(fiber)),
            keys: keys.clone(),
            claimed: AtomicBool::new(false),
        });
        for key in keys {
            c.parked
                .entry(key)
                .or_default()
                .push(ParkedEntry::Wait(Arc::clone(&wp)));
        }
        c.parked_n += 1; // ONE fiber, regardless of arm count
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
    /// Wake every fiber parked on `key`, draining the bucket. Caller holds the core lock (`c`). Two
    /// entry kinds:
    /// * `ParkedEntry::Recv(f)` — a plain `recv` park: requeue the fiber `Ready` (the pre-`wait`
    ///   behavior, byte-identical: a `send` enqueues then makes the one receiver runnable; a `close`
    ///   makes all receivers runnable to observe the closed flag).
    /// * `ParkedEntry::Wait(wp)` — a multi-channel `wait` token: CAS `wp.claimed` false→true. The
    ///   FIRST waker (this key or a concurrent waker on a sibling key) wins → `take()` the single
    ///   fiber, requeue it `Ready` exactly once (`parked_n -= 1`), and SWEEP its token out of every
    ///   OTHER `wp.keys` bucket (by `Arc::ptr_eq`) under this same lock hold, so a later `send`/`close`
    ///   to a swept channel can never re-wake the now-moved fiber. A loser sees `claimed` already set
    ///   and drops the stale token (no double-wake, no panic). All under the one core-lock hold.
    fn wake_bucket(&self, c: &mut SchedCore, key: usize) {
        let Some(entries) = c.parked.remove(&key) else {
            return;
        };
        for entry in entries {
            match entry {
                ParkedEntry::Recv(mut f) => {
                    c.parked_n -= 1;
                    self.runnable.fetch_add(1, Ordering::Relaxed); // parked → ready
                    f.state = FiberState::Ready;
                    c.global.push_back(f);
                }
                ParkedEntry::Wait(wp) => {
                    // CAS the wake-once gate: only the winner takes the fiber + sweeps.
                    if wp
                        .claimed
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        continue; // already claimed by a concurrent waker on another key — stale token
                    }
                    let mut f = wp
                        .fiber
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                        .expect("WaitPark fiber claimed twice");
                    c.parked_n -= 1; // ONE fiber, matching park_wait's +1
                    self.runnable.fetch_add(1, Ordering::Relaxed); // parked → ready
                    f.state = FiberState::Ready;
                    c.global.push_back(f);
                    // Sweep the stale token out of every OTHER arm bucket (this `key` is already drained
                    // via the `remove` above). Match by Arc identity so we never disturb an unrelated
                    // fiber that happens to share a bucket.
                    for &other in &wp.keys {
                        if other == key {
                            continue;
                        }
                        if let Some(bucket) = c.parked.get_mut(&other) {
                            bucket.retain(
                                |e| !matches!(e, ParkedEntry::Wait(o) if Arc::ptr_eq(o, &wp)),
                            );
                            if bucket.is_empty() {
                                c.parked.remove(&other);
                            }
                        }
                    }
                }
            }
        }
    }

    fn send_wake(&self, key: usize, core: &Arc<ChannelCore>, w: WireValue) {
        let mut c = self.lock();
        core.q
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queue
            .push_back(w);
        self.wake_bucket(&mut c, key);
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
        self.wake_bucket(&mut c, key);
        drop(c);
        self.cv.notify_all();
        core.cv.notify_all();
    }

    /// Record a finished fiber's outcome in its FLAT slot, bump its SCOPE's done, drop it from
    /// `running`. Sets GLOBAL `terminate` only when EVERY scope is done (and no eager body is open) —
    /// because farmed helpers (sentinel scope_id) drain until global terminate, and the scope-scoped
    /// owner stop returns each owner the instant its OWN scope completes. The per-scope `done` drives
    /// that owner stop; the global all-done drives helper/sentinel termination.
    fn finish(&self, task_index: usize, scope_id: usize, outcome: TaskOutcome) {
        let mut c = self.lock();
        c.running -= 1;
        c.slots[task_index] = Some(outcome);
        c.scopes[scope_id].done += 1;
        // Per-connection spawn — do NOT latch `terminate` while ANY eager body is still injecting: a
        // transient all-done (every handler SO FAR finished) is not completion — the acceptor may
        // inject more. `close_body` at `JoinNursery` clears `body_open`, and the next run-out-of-work
        // `take_runnable` terminates. Always `false` on the lazy path → unchanged D2b behavior.
        if c.all_scopes_done() && !c.any_body_open() {
            c.terminate = true;
        }
        self.cv.notify_all();
    }

    /// B3.4 — after a scope's cancel is tripped, move every parked fiber **belonging to that scope**
    /// back onto the global queue so a worker resumes it and it observes the cancel flag (at the recv
    /// re-check / a dispatch back-edge) and unwinds. Cross-nursery flat scheduler: with one global
    /// parked set shared across scopes, this MUST be scope-scoped — an inner fault must drain ONLY its
    /// own scope's parked fibers, never drag an OUTER sibling out of its legitimate park (that would
    /// break structured concurrency). Parked entries whose fiber is in a different scope are kept parked
    /// (re-filed into their buckets). A `Recv` entry's scope is read by reference; a `Wait` token's is
    /// PEEKED under its fiber lock before claiming (so a non-matching wait fiber is left intact).
    fn cancel_drain(&self, scope_id: usize) {
        let mut c = self.lock();
        if c.parked_n == 0 {
            return;
        }
        let buckets: Vec<(usize, Vec<ParkedEntry>)> = c.parked.drain().collect();
        let mut drained = 0usize;
        for (key, v) in buckets {
            let mut keep: Vec<ParkedEntry> = Vec::new();
            for entry in v {
                match entry {
                    ParkedEntry::Recv(mut f) => {
                        if f.scope_id == scope_id {
                            drained += 1;
                            f.state = FiberState::Ready;
                            c.global.push_back(f);
                        } else {
                            keep.push(ParkedEntry::Recv(f)); // a sibling scope's park — leave it.
                        }
                    }
                    ParkedEntry::Wait(wp) => {
                        // Peek the fiber's scope WITHOUT claiming (so a non-matching wait fiber is left
                        // intact in every bucket). If already claimed (a prior bucket of THIS token in
                        // this same drain, or a concurrent waker), the fiber is `None` → drop the stale
                        // token copy. Only a matching-scope, still-present fiber is claimed + requeued.
                        let in_scope = {
                            let g = wp.fiber.lock().unwrap_or_else(|e| e.into_inner());
                            g.as_ref().is_some_and(|f| f.scope_id == scope_id)
                        };
                        if !in_scope {
                            // Either a different scope (keep parked) OR already claimed (drop). Keep the
                            // token only if the fiber is still present (i.e. a different scope), else the
                            // stale token must be dropped so it can't double-wake.
                            let still_present =
                                wp.fiber.lock().unwrap_or_else(|e| e.into_inner()).is_some();
                            if still_present {
                                keep.push(ParkedEntry::Wait(wp));
                            }
                            continue;
                        }
                        if wp
                            .claimed
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                            .is_err()
                        {
                            continue; // raced to claimed by another bucket of this token — skip.
                        }
                        let mut f = wp
                            .fiber
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .take()
                            .expect("WaitPark fiber claimed twice");
                        drained += 1;
                        f.state = FiberState::Ready;
                        c.global.push_back(f);
                    }
                }
            }
            if !keep.is_empty() {
                c.parked.insert(key, keep);
            }
        }
        c.parked_n -= drained;
        self.runnable.fetch_add(drained, Ordering::Relaxed); // parked → ready
        self.cv.notify_all();
    }

    /// Drain the per-task outcome slots after the nursery terminates (joining thread, post-loop).
    fn take_slots(&self) -> Vec<Option<TaskOutcome>> {
        std::mem::take(&mut self.lock().slots)
    }

    /// Cross-nursery flat scheduler — drain ONE scope's contiguous slot sub-range
    /// (`base_index..base_index+total`) in task order, leaving other scopes' slots intact. The reducer
    /// flushes this scope's `Done`/`Exit` output in order (Decision F) — the slots are contiguous
    /// because `register_scope`/`inject` only ever grow the flat vec / the LAST scope. Replaces each
    /// taken slot with `None` so a re-take is a no-op (and the flat vec keeps its length, so other
    /// scopes' `base_index` stays valid).
    fn take_scope_slots(&self, scope_id: usize) -> Vec<Option<TaskOutcome>> {
        let mut c = self.lock();
        let (base, total) = {
            let s = &c.scopes[scope_id];
            (s.base_index, s.total)
        };
        (base..base + total).map(|i| c.slots[i].take()).collect()
    }

    /// Cross-nursery flat scheduler — block until ONE scope's slots are all filled (`done == total` for
    /// that scope). The inline owner returns from `mn_worker_loop` the instant its scope completes, so
    /// this is a non-blocking re-check in the common case; it parks only if a demoted replacement is
    /// still settling this scope's last fiber. Poison-tolerant.
    fn wait_for_scope(&self, scope_id: usize) {
        let mut c = self.lock();
        while c.scopes[scope_id].done < c.scopes[scope_id].total {
            c = self.cv.wait(c).unwrap_or_else(|e| e.into_inner());
        }
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
        while c.any_scope_incomplete() {
            c = self.cv.wait(c).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// The B3.5 deadlock predicate, now GLOBAL across every nursery scope (cross-nursery flat
    /// scheduler): SOME scope still has unfinished tasks (`any_scope_incomplete`), every not-done fiber
    /// is parked, with none running, none queued anywhere (`runnable`), and **none in flight in the
    /// blocking pool** (`inflight`) — so no `send` and no blocking-pool completion can ever arrive to
    /// wake a parked fiber, ANYWHERE. Because the park/wake set is one global set, a quiesce here means
    /// nothing can progress in ANY scope, so faulting every parked fiber is correct. Called under the
    /// core lock (the caller holds `c`); `running == 0` excludes the only out-of-lock `runnable`
    /// mutator, and `inflight` is mutated only under the core lock, so both reads are sound.
    fn is_deadlocked(&self, c: &SchedCore) -> bool {
        // The `done < total` half is now explicit (the owner-stop replaced the preceding scalar
        // `done == total` terminate check). If EVERY scope is done there is no deadlock — `finish` will
        // have (or is about to) set global `terminate`; the owner-stop returns each owner already.
        if !c.any_scope_incomplete() {
            return false;
        }
        // Per-connection spawn — an eager nursery whose body is still running is live work the sched
        // can't account (the acceptor runs inline and may `inject` a handler that wakes a parked
        // sibling). Never declare deadlock while ANY body is open; `close_body` at `JoinNursery`
        // re-enables the predicate so a genuine post-join deadlock still fires. Always `false` on the
        // lazy path — unchanged.
        if c.any_body_open() {
            return false;
        }
        // Cross-nursery flat scheduler — if every still-incomplete scope is an early-enlisted outer
        // nursery awaiting the inline builder's join, the builder has finished all nested service and
        // will return to the body to feed those parked siblings (`send`/`close`/`spawn`). That is a live
        // external feeder the counter-only predicate can't see, so it is NOT a deadlock. The flag clears
        // as the builder begins draining each enlisted scope, so a genuine post-body deadlock still fires
        // (and a stuck NESTED scope keeps a non-`awaiting_builder` scope incomplete → fires). (#1/#2.)
        if c.all_incomplete_awaiting_builder() {
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
        if c.demoted_chans.values().any(|(core, _)| {
            !core
                .q
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .queue
                .is_empty()
        }) {
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
        let OffloadReq {
            func,
            args,
            span,
            timer_ms,
        } = req;
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
                .unwrap_or_else(|| {
                    std::time::Instant::now() + std::time::Duration::from_secs(86_400 * 365)
                });
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
            let outcome =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_offload(func, args)));
            let result = match outcome {
                Ok(Ok(nr)) => Ok(nr),
                Ok(Err(e)) => Err(RuntimeError {
                    message: e.message,
                    span,
                }),
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
        if let Some(fiber) = poller::register(
            pp.key,
            pp.fd,
            pp.interest,
            fiber,
            Arc::clone(self),
            Arc::clone(&pp.in_flight),
            pp.deadline,
        ) {
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
    /// Fault every still-parked fiber (across ALL scopes) with the deadlock error and set global
    /// terminate (called under the lock). Correct because the global predicate firing means nothing can
    /// progress anywhere. A `Wait` token sits in N buckets but is ONE fiber — claim-once (the wake CAS)
    /// dedups it so the flat slot is faulted and the fiber's SCOPE's `done` bumped exactly once.
    fn flag_deadlock(&mut self, err: &RuntimeError) {
        let buckets: Vec<Vec<ParkedEntry>> = self.parked.drain().map(|(_, v)| v).collect();
        for v in buckets {
            for entry in v {
                let fiber = match entry {
                    ParkedEntry::Recv(f) => Some(f),
                    ParkedEntry::Wait(wp) => {
                        if wp
                            .claimed
                            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                            .is_err()
                        {
                            None
                        } else {
                            Some(
                                wp.fiber
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .take()
                                    .expect("WaitPark fiber claimed twice"),
                            )
                        }
                    }
                };
                if let Some(f) = fiber {
                    self.slots[f.task_index] = Some(TaskOutcome::Fault {
                        err: err.clone(),
                        out: String::new(),
                        stderr: String::new(),
                    });
                    self.scopes[f.scope_id].done += 1;
                }
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
    RuntimeError {
        message: format!("internal error: a parallel task panicked: {msg}"),
        span,
    }
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
    Method {
        recv: Value,
        name: String,
        args: Vec<Value>,
    },
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
        Ok(WorkerResult {
            value,
            out: self.worker.out,
            stderr: self.worker.stderr,
        })
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
                    TaskOutcome::Fault {
                        err: e,
                        out: std::mem::take(&mut self.worker.out),
                        stderr: std::mem::take(&mut self.worker.stderr),
                    }
                }
                Ok(ret) => {
                    // Re-stamp with the task's real span: a returned non-sendable value (a
                    // frame-holding generator) faults gracefully at the submit/spawn site, not line 0.
                    let crossed = self.worker.to_wire_at(ret, span).and_then(|value| {
                        self.worker.ensure_crossable(&value, span).map(|()| value)
                    });
                    match crossed {
                        Ok(value) => TaskOutcome::Done(WorkerResult {
                            value,
                            out: std::mem::take(&mut self.worker.out),
                            stderr: std::mem::take(&mut self.worker.stderr),
                        }),
                        Err(e) => {
                            self.worker.trip_cancel();
                            TaskOutcome::Fault {
                                err: e,
                                out: std::mem::take(&mut self.worker.out),
                                stderr: std::mem::take(&mut self.worker.stderr),
                            }
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
    fn into_fiber(self, task_index: usize, scope_id: usize) -> Fiber {
        let ReadyWorker { worker, call, span } = self;
        let task = match call {
            ReadyCall::Invoke { callee, args } => PendingCall::Call { callee, args, span },
            ReadyCall::Method { recv, name, args } => PendingCall::Method {
                recv,
                name,
                args,
                span,
            },
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
        Fiber {
            ctx,
            state: FiberState::Pending(task),
            task_index,
            scope_id,
            span,
            resume_native: None,
        }
    }
}

/// D2b — the disposition of one fiber run on a worker shell: park it on a channel (it blocked on an
/// empty `recv`; carries the channel core ptr key + the `Arc<ChannelCore>` so `park` can re-check the
/// queue under the sched lock) or finish it with a terminal outcome.
enum Disp {
    Park(usize, Arc<ChannelCore>),
    /// §6d — the fiber blocked on a multi-channel `wait` (every arm empty/live). Carries `(key, core)`
    /// for each live arm, captured WHILE the fiber heap was live (like `Park`); the worker loop hands
    /// it to [`MnSched::park_wait`], which files ONE shared `WaitPark` token in every arm bucket.
    WaitPark(Vec<(usize, Arc<ChannelCore>)>),
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
        let method_ic = vec![MethodIcSite::EMPTY; program.method_ic_sites as usize];
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
            mn_scopes: Vec::new(),
            mn_enlisted: 0,
            mn_enlist_sched: None,
            eager_scheds: Vec::new(),
            nursery_defer_floors: Vec::new(),
            executors: Vec::new(),
            suspend: None,
            wait_suspend: None,
            offload: None,
            poll_park: None,
            pending_connect: None,
            poll_timed_out: false,
            native_reentry: 0,
            reds: 0,             // D3 — set to CONTEXT_REDS per schedule-in (run_one_fiber)
            yield_now: false,    // D3
            gen_yielding: false, // experimental generators
            gen_host_ctx: Vec::new(),
            active_generators: Vec::new(),
            wid: 0,         // D5 owe #3 (Path C) — set in mn_worker_loop
            demoted: false, // D5 owe #3 (Path C)
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
        std::mem::swap(&mut self.mn_scopes, &mut ctx.mn_scopes);
        std::mem::swap(
            &mut self.nursery_defer_floors,
            &mut ctx.nursery_defer_floors,
        );
        std::mem::swap(&mut self.eager_scheds, &mut ctx.eager_scheds);
        std::mem::swap(&mut self.fault_trace, &mut ctx.fault_trace);
        std::mem::swap(&mut self.fault_trace_depth, &mut ctx.fault_trace_depth);
        // D2a — an M:N fiber (`Some`) owns its heap; swap it with the host's. A cooperative fiber
        // (`None`) shares the single `Vm::heap` (decision A), so its heap is left untouched and the
        // cooperative engine stays byte-identical by construction. D2b — the same `Some` gate carries
        // the fiber's heap-keyed side state (out/stderr/module roots/executors), so they move
        // atomically WITH the heap their `GcRef`s index. A cooperative fiber swaps none of it.
        if let Some(ctx_heap) = ctx.heap.as_mut() {
            debug_assert!(
                self.parallel,
                "cooperative fiber must never carry its own heap (decision A)"
            );
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
        self.suspend.is_some()
            || self.wait_suspend.is_some()
            || self.yield_now
            || self.offload.is_some()
            || self.poll_park.is_some()
    }

    /// Run `f` with the native-reentry guard raised (B1). A blocking `recv` reached while the guard
    /// is up cannot park (its caller's loop/recursion state lives on the Rust stack, not in a
    /// [`Fiber`]), so it faults `deadlock` instead of suspending. Wraps every site that re-enters
    /// Chezzi code from native Rust.
    fn guarded<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, RuntimeError>,
    ) -> Result<T, RuntimeError> {
        self.native_reentry += 1;
        // The guard counter MUST return to its entry value on every exit path, including an unwind:
        // it gates park-vs-demote for all blocking concurrency ops, and a re-entered FFI callback's
        // Rust panic is caught one frame up (`callback_trampoline`'s `catch_unwind`) and re-raised as
        // a recoverable error — so a plain `-= 1` after `f(self)` would be skipped on panic and leak
        // the counter at +1 for the VM's lifetime. A `Drop`-based guard can't be used here (it would
        // alias `self` across `f(self)`), so catch the unwind, decrement, then resume it unchanged.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self)));
        self.native_reentry -= 1;
        match r {
            Ok(v) => v,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    // ----- experimental generators (VM-only) -----

    /// Swap the live execution context (frames/stack/depth/base/handlers) with a parked [`GenCtx`].
    /// Smaller sibling of [`Vm::swap_ctx`]: a generator shares the host heap (like a cooperative
    /// fiber, decision A) and cannot open nurseries/spawn (checker-forbidden), so none of the
    /// heap-keyed or nursery state moves.
    fn swap_gen_ctx(&mut self, ctx: &mut GenCtx) {
        std::mem::swap(&mut self.frames, &mut ctx.frames);
        std::mem::swap(&mut self.stack, &mut ctx.stack);
        std::mem::swap(&mut self.call_depth, &mut ctx.call_depth);
        std::mem::swap(&mut self.cur_base, &mut ctx.cur_base);
        std::mem::swap(&mut self.handlers, &mut ctx.handlers);
    }

    /// Allocate a not-yet-started generator object over a generator proto + its call args. Calling a
    /// `yield`-ing function lands here instead of running the body (see `do_call`/`invoke_value`).
    fn alloc_generator(
        &mut self,
        proto: ProtoId,
        home: GcRef,
        closure: Option<GcRef>,
        args: Vec<Value>,
    ) -> Value {
        let core = GeneratorCore {
            proto,
            home,
            closure,
            state: GenState::Pending(args),
            ctx: GenCtx::default(),
        };
        Value::Obj(self.heap.alloc(Obj::Generator(Box::new(core))))
    }

    /// Resume a generator until its next `yield` (→ `Some(v)`) or until its body ends (→ `None`,
    /// state `Done`). Driven intrinsically by `.next()` (see `do_method_call`). The generator runs in
    /// its own private base-0 context swapped into the live `Vm`; the host context is parked in
    /// `gen_host_ctx` (GC-rooted) for the duration. Runs `guarded`, so a would-be blocking op inside a
    /// generator faults `deadlock` rather than parking the host.
    fn generator_next(&mut self, h: GcRef, span: Span) -> Result<Value, RuntimeError> {
        // Take the generator's lifecycle state + parked context out of the heap object. `state` is
        // left as `Done` and `ctx` as empty; the real state is written back after the run. An
        // already-`Done` generator short-circuits to `None`.
        let (proto, home, closure, mut gen_ctx, state) = {
            let Obj::Generator(g) = self.heap.get_mut(h) else {
                return Err(self.err("`.next()` on a non-generator value".to_string(), span));
            };
            if matches!(g.state, GenState::Done) {
                return Ok(self.alloc_enum("Option", "None", Vec::new()));
            }
            let state = std::mem::replace(&mut g.state, GenState::Done);
            let ctx = std::mem::take(&mut g.ctx);
            (g.proto, g.home, g.closure, ctx, state)
        };

        // Park the host context (rooted via `gen_host_ctx`) and install the generator's private
        // context into the live `Vm`. Two swaps: host out to a temp, then the generator in.
        let mut host = GenCtx::default();
        self.swap_gen_ctx(&mut host); // self.* now default-empty; `host` holds the real host context
        self.swap_gen_ctx(&mut gen_ctx); // self.* now the generator's (suspended) context; gen_ctx empty
        self.gen_host_ctx.push(host);
        self.active_generators.push(h);

        // First `.next()` builds the initial frame over the pending args (private stack starts empty,
        // so the frame lands at base 0). A resumed generator's frames are already in `self`.
        let first_call = matches!(state, GenState::Pending(_));
        let push_res = if let GenState::Pending(args) = state {
            self.push_frame(proto, home, closure, args, true, false, span)
        } else {
            Ok(())
        };

        // Run to the next suspension / end (guarded: no parking inside a generator).
        self.gen_yielding = false;
        let run = push_res.and_then(|()| self.guarded(|s| s.run_until(0)));
        let yielded = self.gen_yielding;
        self.gen_yielding = false;

        // Pull the generator's now-updated context back out and restore the host.
        let mut new_ctx = GenCtx::default();
        self.swap_gen_ctx(&mut new_ctx); // new_ctx = generator's current context; self.* now empty
        let mut host = self.gen_host_ctx.pop().expect("generator host context");
        self.swap_gen_ctx(&mut host); // self.* = host restored
        self.active_generators.pop();
        let _ = first_call;

        // A fault inside the generator: leave it `Done` (already set) with an empty ctx, propagate.
        run?;

        if yielded {
            // The yielded value sits on the generator's private stack top. Park the rest to resume.
            let v = new_ctx
                .stack
                .pop()
                .expect("yielded value on the generator stack");
            if let Obj::Generator(g) = self.heap.get_mut(h) {
                g.ctx = new_ctx;
                g.state = GenState::Suspended;
            }
            Ok(self.alloc_enum("Option", "Some", vec![v]))
        } else {
            // Body returned / fell off → exhausted. Drop the (drained) context.
            if let Obj::Generator(g) = self.heap.get_mut(h) {
                g.ctx = GenCtx::default();
                g.state = GenState::Done;
            }
            Ok(self.alloc_enum("Option", "None", Vec::new()))
        }
    }

    /// If `v` is an unhandled error (`Err(..)`/`None`) reaching the top level, build the runtime
    /// error that exits the program. Mirrors `interp::top_level_error` — message must be identical.
    fn top_level_error(&self, v: Value, span: Span) -> Option<RuntimeError> {
        let Value::Obj(h) = v else { return None };
        let Obj::Enum {
            variant_id,
            payload,
        } = self.heap.get(h)
        else {
            return None;
        };
        // Builtin `Result`/`Option` only — a user enum that shadows `Err`/`None` gets a DISTINCT id
        // (natives hold the fixed `VID_ERR`/`VID_NONE_VARIANT`), so the int compare is exactly the
        // "is this the builtin unhandled-error variant" gate (more precise than the old name compare).
        let unhandled =
            *variant_id == crate::vm::op::VID_ERR || *variant_id == crate::vm::op::VID_NONE_VARIANT;
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

    /// The entry module's runtime object (its globals/home), valid after `run()` has initialized the
    /// modules. The entry is the last module in dependency order. The `chezzi test` runner uses it as
    /// the home for free `test fn`s and suite construction thunks.
    fn entry_home(&self) -> GcRef {
        *self
            .module_objs
            .last()
            .expect("modules initialized before invoking tests")
    }

    /// `chezzi test` — invoke one zero-arg test proto (a free `test fn` or a suite construction
    /// thunk) on this already-initialized VM, returning its result. The VM stays reusable after a
    /// fault, so the runner keeps going after a failing test. `Err` carries the fault's `span` (the
    /// `assert`'s line) for `file:line` reporting.
    pub fn invoke_test(&mut self, proto: ProtoId) -> Result<(), RuntimeError> {
        debug_assert!(
            self.program.protos[proto].is_test,
            "invoke_test called on a non-test proto"
        );
        let home = self.entry_home();
        self.run_proto(
            proto,
            home,
            None,
            Vec::new(),
            true,
            false,
            Span { line: 1, col: 1 },
        )?;
        Ok(())
    }

    /// Bare `chezzi run` with a `module:function` manifest entrypoint — invoke a named top-level
    /// function of the entry module after `run()` has initialized all modules. Looks the name up in
    /// the entry module's namespace (so a re-exported import works too) and calls it with no args.
    /// A missing name (or a non-callable binding) is a clear runtime error rather than a silent no-op.
    pub fn invoke_entrypoint(&mut self, fn_name: &str) -> Result<(), RuntimeError> {
        let span = Span { line: 1, col: 1 };
        let home = self.entry_home();
        // Read the binding by name from the entry module's slot table (mirrors `module_define`).
        let callee = match self.heap.get(home) {
            Obj::Module { slots, index, .. } => index.get(fn_name).map(|&i| slots[i as usize]),
            _ => None,
        };
        let callee = callee.ok_or_else(|| {
            self.err(
                format!(
                    "entrypoint function `{fn_name}` not found in module `{}`",
                    self.module_name(home)
                ),
                span,
            )
        })?;
        // Guard with a clear message before `invoke_value`'s generic "not callable" fault.
        let callable = matches!(
            callee,
            Value::Obj(h) if matches!(
                self.heap.get(h),
                Obj::Func { .. } | Obj::Closure { .. } | Obj::Native { .. } | Obj::Cffi(_)
            )
        );
        if !callable {
            return Err(self.err(
                format!(
                    "entrypoint `{fn_name}` in module `{}` is not a function (it is a {})",
                    self.module_name(home),
                    self.type_name(callee)
                ),
                span,
            ));
        }
        self.invoke_value(callee, Vec::new(), span)?;
        Ok(())
    }

    /// `chezzi test` — invoke a suite method/lifecycle hook proto with `self` bound to `recv` (a
    /// suite instance). Returns the method's value (ignored by the runner) or its fault.
    pub fn invoke_suite_method(
        &mut self,
        proto: ProtoId,
        recv: Value,
    ) -> Result<Value, RuntimeError> {
        let home = self.entry_home();
        self.run_proto(
            proto,
            home,
            None,
            vec![recv],
            true,
            false,
            Span { line: 1, col: 1 },
        )
    }

    /// `chezzi test` — construct a suite instance via its synthetic zero-arg `__new_<Suite>` thunk.
    pub fn build_suite_instance(&mut self, new_thunk: ProtoId) -> Result<Value, RuntimeError> {
        let home = self.entry_home();
        self.run_proto(
            new_thunk,
            home,
            None,
            Vec::new(),
            true,
            false,
            Span { line: 1, col: 1 },
        )
    }

    /// `chezzi test` — initialize all modules (run top-levels once) so globals/functions exist before
    /// tests are invoked. A thin public wrapper over `run` for the runner.
    pub fn init_for_tests(&mut self) -> Result<(), RuntimeError> {
        self.run()
    }

    /// `chezzi test` — construct a fresh VM over a compiled program (the runner owns the lifecycle).
    pub fn for_program(program: Arc<Program>) -> Self {
        Vm::new(program)
    }

    /// `chezzi test` — take + clear whatever a test printed to stdout, resetting the buffer so the
    /// next test starts clean (the runner currently discards it; the report is Rust-formatted).
    pub fn take_out(&mut self) -> String {
        std::mem::take(&mut self.out)
    }

    /// `chezzi test` — drain anything the program left running (e.g. an Executor a test forgot to
    /// shut down), mirroring the ordinary run's graceful reap. Best-effort: ignore drain faults so a
    /// stray resource doesn't mask the test verdict.
    pub fn reap_after_tests(&mut self) {
        let _ = self.drain_live_executors(Span { line: 1, col: 1 });
    }

    fn run_module(&mut self, idx: usize) -> Result<(), RuntimeError> {
        let m = self.program.modules[idx].clone();
        // M19 Phase 2b: pre-size the namespace to the compiler's slot count and build its name→slot
        // index from `global_slots`, so `DefineGlobalSlot(i)` / bind-import writes land in the slot
        // the compiler chose. Native modules carry no slots (members injected by name below).
        let index: std::collections::HashMap<Box<str>, u32> = m
            .global_slots
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str().into(), i as u32))
            .collect();
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
        self.run_proto(
            m.toplevel,
            mod_obj,
            None,
            Vec::new(),
            false,
            true,
            Span { line: 1, col: 1 },
        )?;
        Ok(())
    }

    fn bind_import(
        &mut self,
        into: GcRef,
        imp: &crate::resolver::ResolvedImport,
    ) -> Result<(), RuntimeError> {
        use crate::ast::Import;
        let target_idx = self
            .program
            .module_index(&imp.target)
            .expect("resolver guarantees the import target is in the graph");
        let target_obj = self.module_objs[target_idx];
        match &imp.import {
            Import::Module { path, alias, .. } => {
                let name = alias
                    .clone()
                    .unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                self.module_define(into, &name, Value::Obj(target_obj));
            }
            Import::From { names, .. } => {
                for (member, alias) in names {
                    // `std.ffi`'s exported FFI marshalling TYPE names — the fixed-width integers
                    // (`import int32 from std.ffi`) and the opaque `ptr` handle — carry NO runtime
                    // value: they are compile-time type imports the checker resolves. Skip them here
                    // (the module has no such global by design); any other missing member is a genuine
                    // error.
                    if self.module_name(target_obj) == "std.ffi"
                        && (crate::native::ffi::TYPE_NAMES.contains(&member.as_str())
                            || member == "ptr")
                    {
                        continue;
                    }
                    // `std.concurrency`'s four exported ctor/TYPE names (`Shared`/`RwShared`/`Atomic`/
                    // `Executor`) likewise carry NO runtime value: they are checker-resolved type
                    // imports, and the ctor is resolved by the compiler's name→opcode dispatch (not a
                    // bound module member). Skip them here — the file-less native module has no such
                    // global by design. (Without this, `import Shared from std.concurrency` faults.)
                    if self.module_name(target_obj) == "std.concurrency"
                        && matches!(
                            member.as_str(),
                            "Shared" | "RwShared" | "Atomic" | "Executor"
                        )
                    {
                        continue;
                    }
                    // `std.time`'s `timer` is an opcode-backed builtin with NO runtime module-member
                    // value (the call lowers via the compiler's name→opcode dispatch). Skip it — std.time
                    // is a REAL native module, so this MUST be `timer`-specific, not a blanket std.time
                    // skip (now/monotonic/sleep_ms/format DO bind normally). Without this, `import timer
                    // from std.time` faults `module 'std.time' has no member 'timer'`.
                    if self.module_name(target_obj) == "std.time" && member == "timer" {
                        continue;
                    }
                    // `std.net`'s `Socket`/`Listener` are TYPE-only imports with NO runtime module-member
                    // value: a `Socket` value comes from `connect`/`listen` and the type resolves directly
                    // to `Ty::Socket`. Skip them — the native module has no such global by design. Mirrors
                    // the interp `bind_import` skip (parity); without it `import Socket from std.net` faults.
                    if self.module_name(target_obj) == "std.net"
                        && matches!(member.as_str(), "Socket" | "Listener")
                    {
                        continue;
                    }
                    // Bind the member's runtime value if the target module exports one (a fn/value).
                    // A `from`-imported USER type (struct/enum/alias) carries NO runtime value — it
                    // resolves through the program-global type tables by name — so a member with no
                    // global that IS a known type name is skipped (not an error). A member that is
                    // neither a value nor a type is a genuine "no member". The value bind is tried
                    // FIRST so a fn named like a type IN ANOTHER MODULE is still bound here.
                    match self.module_global(target_obj, member) {
                        Some(value) => {
                            self.module_define(into, alias.as_ref().unwrap_or(member), value);
                        }
                        None if self.program.type_names.contains(member) => {}
                        None => {
                            let tname = self.module_name(target_obj);
                            return Err(self.err(
                                format!("module '{tname}' has no member '{member}'"),
                                imp.span,
                            ));
                        }
                    }
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
            if !self.cancelled
                && self
                    .cancel
                    .as_ref()
                    .is_some_and(|c| c.load(Ordering::Relaxed))
            {
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
            // Experimental generators — an `Op::Yield` (handled last iteration) asked us to suspend:
            // hand control back to the host `.next()` with the generator's frames/stack intact. The
            // yielded value sits on the stack top for `generator_next` to take. Only ever true inside
            // the private `run_until` that `generator_next` drives, so the host loop is unaffected.
            if self.gen_yielding {
                return Ok(());
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
                Op::BinLocalConst { slot, val, kind } => {
                    self.op_bin_local_const(*slot, *val, *kind, span)
                }
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
                Op::Add => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::Add,
                    span,
                ),
                Op::Sub => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::Sub,
                    span,
                ),
                Op::Mul => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::Mul,
                    span,
                ),
                Op::Div => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::Div,
                    span,
                ),
                Op::Mod => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::Mod,
                    span,
                ),
                Op::Lt => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::Lt,
                    span,
                ),
                Op::LtEq => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::LtEq,
                    span,
                ),
                Op::Gt => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::Gt,
                    span,
                ),
                Op::GtEq => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::GtEq,
                    span,
                ),
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
                let caught_here =
                    matches!(self.handlers.last().copied(), Some(h) if h.frame_len > base_level);
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
                        self.frames[h.frame_len - 1]
                            .defer_markers
                            .truncate(h.markers_len);
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
        // Experimental generators — while a generator body runs in the live `Vm` fields above, the
        // host(s) it suspended are parked in `gen_host_ctx` (their frames/stack are not in `self`), so
        // root them here exactly like a parked fiber. The running generators' own handles are roots so
        // their objects survive to be written back (each generator's PARKED ctx, if any, is empty
        // while it runs, so `children` adds nothing extra). Both are empty outside a `generator_next`.
        for host in &self.gen_host_ctx {
            for v in &host.stack {
                if let Value::Obj(h) = v {
                    work.push(*h);
                }
            }
            for f in &host.frames {
                work.push(f.home);
                if let Some(c) = f.closure {
                    work.push(c);
                }
                for d in &f.deferred {
                    work.extend(d.roots());
                }
            }
        }
        work.extend(self.active_generators.iter().copied());
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
            debug_assert!(
                nursery.parent.heap.is_none(),
                "a cooperative parked fiber must not own a heap (decision A)"
            );
            Self::root_ctx(&nursery.parent, &mut work);
            for child in &nursery.children {
                debug_assert!(
                    child.ctx.heap.is_none(),
                    "a cooperative child fiber must not own a heap (decision A)"
                );
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
            Op::ConstBytes(b) => {
                // Not interned in v1 (unlike `ConstStr`): allocate a fresh `bytes` heap object per
                // push, like a list literal. Bytes literals are not a hot path.
                let h = self.heap.alloc(Obj::Bytes(b.clone()));
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
            Op::Assert { has_msg } => {
                // Reached only on the failing path: the compiler emits `Op::Assert` after a
                // `JumpIfFalse` that already consumed (and tested) `cond`, so this op always faults.
                // `msg` (if present) was evaluated lazily just before us — matching the interpreter,
                // which only evaluates `msg` when the assertion fails.
                let message = if *has_msg {
                    let m = self.pop();
                    match self.val_str(m) {
                        Some(s) => format!("assertion failed: {s}"),
                        None => "assertion failed".to_string(),
                    }
                } else {
                    "assertion failed".to_string()
                };
                return Err(self.err(message, span));
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
            Op::GetCaptured(slot) => {
                // Lever #3: hot path is a pure `captured[slot]` index — no string hash. The slot is
                // always in range (one capture per snapshot entry, populated at MakeClosure), and a
                // nested missing parent capture is stored as `Value::Nil` (byte-identical to the old
                // `get(name) -> Some(Nil)`).
                let frame = self.frames.last().unwrap();
                let (clo, home) = (frame.closure, frame.home);
                let v = clo.and_then(|h| match self.heap.get(h) {
                    Obj::Closure { captured, .. } => captured.get(*slot as usize).copied(),
                    _ => None,
                });
                match v {
                    Some(v) => self.push(v),
                    None => {
                        // Cold path: not a closure frame, or slot out of range. Recover the name from
                        // the proto's capture_names and fall back to a home global (D1 lazy fault).
                        self.ensure_module_faulted(home);
                        let proto = self.frames.last().unwrap().proto;
                        let name = self.program.protos[proto]
                            .capture_names
                            .get(*slot as usize)
                            .cloned();
                        let v = name
                            .as_deref()
                            .and_then(|n| self.module_global(home, n))
                            .ok_or_else(|| {
                                let label = name.unwrap_or_else(|| format!("capture#{slot}"));
                                self.err(format!("undefined name '{label}'"), span)
                            })?;
                        self.push(v);
                    }
                }
            }
            Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod => self.arith(op, span)?,
            Op::Neg => {
                let v = self.pop();
                let r = match v {
                    Value::Int(n) => n.checked_neg().map(Value::Int).ok_or_else(|| {
                        self.err("integer overflow in negation".to_string(), span)
                    })?,
                    Value::Float(f) => Value::Float(-f),
                    // M22: unary `-` on a struct/enum dispatches to its `neg(self) -> Self` method
                    // (the `Neg` protocol). Mirrors `struct_arith`, but self-only (no `other`).
                    Value::Obj(h)
                        if matches!(self.heap.get(h), Obj::Struct { .. } | Obj::Enum { .. }) =>
                    {
                        let (proto, home) = self.resolve_overload_method(v, "neg", span)?;
                        self.guarded(|vm| {
                            vm.run_proto(proto, home, None, vec![v], true, false, span)
                        })?
                    }
                    other => {
                        return Err(self.err(
                            format!("cannot apply Neg to {}", self.type_name(other)),
                            span,
                        ));
                    }
                };
                self.push(r);
            }
            Op::Not => {
                let v = self.pop();
                match v {
                    Value::Bool(b) => self.push(Value::Bool(!b)),
                    other => {
                        return Err(self.err(
                            format!("cannot apply Not to {}", self.type_name(other)),
                            span,
                        ));
                    }
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
            // `return` (not `?`): keeps `step`'s frame from materializing an extra `RuntimeError`
            // temporary, which would bloat the deep re-entrant recursion path (`str(self)`-style
            // infinite recursion must hit the 10_000 call-depth limit before exhausting the host
            // stack — `self_referential_stringable_hits_depth_limit` guards exactly this).
            Op::Contains => return self.op_contains(span),
            Op::AsBool => {
                let v = *self.stack.last().unwrap();
                if !matches!(v, Value::Bool(_)) {
                    return Err(
                        self.err(format!("expected bool, found {}", self.type_name(v)), span)
                    );
                }
            }
            Op::AsInt => {
                let v = *self.stack.last().unwrap();
                if !matches!(v, Value::Int(_)) {
                    return Err(
                        self.err(format!("expected int, found {}", self.type_name(v)), span)
                    );
                }
            }
            Op::CoerceFloat => {
                // One-way int→float widening (idempotent on Float). Reuses `builtin_float`'s
                // `n as f64`; any non-numeric top is a runtime error (the checker guarantees numeric).
                let top = self.stack.last_mut().unwrap();
                match *top {
                    Value::Int(n) => *top = Value::Float(n as f64),
                    Value::Float(_) => {}
                    other => {
                        return Err(self.err(
                            format!("expected number, found {}", self.type_name(other)),
                            span,
                        ));
                    }
                }
            }
            // ----- M19 superinstructions. Bodies live in `#[inline(never)]` helpers so `step`'s own
            // stack frame stays lean. Plain calls no longer recurse the host stack (call-flattening:
            // `Op::Call` pushes a frame and the running `run_until` loop executes it), but the
            // HOF/method/deferred re-entrant path still cycles `step → run_proto → run_until → step`,
            // so a fat `step` frame would still bloat that recursion. -----
            Op::BinLocalLocal { a, b, kind } => self.op_bin_local_local(*a, *b, *kind, span)?,
            Op::BinLocalConst { slot, val, kind } => {
                self.op_bin_local_const(*slot, *val, *kind, span)?
            }
            Op::IncLocal { slot, delta } => self.op_inc_local(*slot, *delta, span)?,
            Op::PushHandler(target) => self.handlers.push(Handler {
                stack_len: self.stack.len(),
                frame_len: self.frames.len(),
                call_depth: self.call_depth,
                ip: *target,
                defer_len: self.frames.last().map(|f| f.deferred.len()).unwrap_or(0),
                markers_len: self
                    .frames
                    .last()
                    .map(|f| f.defer_markers.len())
                    .unwrap_or(0),
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
            Op::CallStatic {
                type_key,
                method,
                argc,
            } => self.do_static_call(type_key, method, *argc, span)?,
            Op::CallBuiltin(name, argc) => self.do_builtin(name, *argc, span)?,
            Op::LoadBuiltin(name) => {
                let h = self.heap.alloc(Obj::Builtin(name.as_str().into()));
                self.push(Value::Obj(h));
            }
            Op::CallPrint(argc) => self.do_print(*argc, span)?,
            Op::CallPrintSep { argc } => self.do_print_sep(*argc, span)?,
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
                    match map
                        .candidates(hk)
                        .iter()
                        .copied()
                        .find(|&p| self.values_equal(map.entries[p].1, k))
                    {
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
                    if !set
                        .candidates(he)
                        .iter()
                        .copied()
                        .any(|p| self.values_equal(set.entries[p].1, e))
                    {
                        set.push(he, e);
                    }
                }
                self.stack.truncate(at);
                let h = self.heap.alloc(Obj::Set(set));
                self.push(Value::Obj(h));
            }
            Op::NewStruct(name, argc) => self.new_struct(name, *argc, span)?,
            Op::NewType(type_key) => {
                let inner = self.pop();
                let h = self.heap.alloc(Obj::NewType {
                    type_key: type_key.as_str().into(),
                    inner,
                });
                self.push(Value::Obj(h));
            }
            Op::NewEnum {
                variant,
                variant_id,
                argc,
            } => self.new_enum(variant, *variant_id, *argc, span)?,
            Op::MakeFunc(proto) => {
                let home = self.frames.last().unwrap().home;
                let h = self.heap.alloc(Obj::Func {
                    proto: *proto,
                    home,
                });
                self.push(Value::Obj(h));
            }
            // Body in an `#[inline(never)]` helper so `step`'s frame stays small (the deep-recursion
            // depth-guard test overflows in debug if `step` grows — same discipline as `ToStrFmt`).
            Op::MakeCffi(id) => self.op_make_cffi(*id, span)?,
            Op::MakeClosure(proto, entries) => {
                // Lever #3: build the captured env *positionally* — slot i is the i-th entry (the
                // snapshot order the child proto's `capture_names` mirrors). A nested capture reads
                // the enclosing closure's value by its positional `parent_slot`.
                let frame = self.frames.last().unwrap();
                let (base, home, enclosing) = (frame.base, frame.home, frame.closure);
                let mut captured = Vec::with_capacity(entries.len());
                for e in entries {
                    let v = match e.src {
                        CapSrc::Slot(i) => self.stack[base + i],
                        CapSrc::Captured(parent_slot) => enclosing
                            .and_then(|h| match self.heap.get(h) {
                                Obj::Closure { captured, .. } => {
                                    captured.get(parent_slot as usize).copied()
                                }
                                _ => None,
                            })
                            .unwrap_or(Value::Nil),
                    };
                    captured.push(v);
                }
                let h = self.heap.alloc(Obj::Closure {
                    proto: *proto,
                    captured,
                    home,
                });
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
            // Body in an `#[inline(never)]` helper so `step`'s frame stays small (the deep-recursion
            // depth-guard test overflows in debug if `step` grows — see commit 1450077).
            Op::ToStrFmt(spec) => self.op_to_str_fmt(spec, span)?,
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
                        // `bytes`/`bytearray` iterate as `int`s (0–255). Snapshots to a list of ints —
                        // mutating the `bytearray` during iteration does not change the loop sequence.
                        Obj::Bytes(b) => {
                            let items: Vec<Value> =
                                b.iter().map(|&x| Value::Int(x as i64)).collect();
                            let nh = self.heap.alloc(Obj::List(items));
                            self.push(Value::Obj(nh));
                        }
                        Obj::ByteArray(b) => {
                            let items: Vec<Value> =
                                b.iter().map(|&x| Value::Int(x as i64)).collect();
                            let nh = self.heap.alloc(Obj::List(items));
                            self.push(Value::Obj(nh));
                        }
                        // A cursor (`for x in pure_iterable_struct` lowered via `IterableToCursor`)
                        // snapshots its REMAINING items to the index-iterable list.
                        Obj::Iter { items, pos } => {
                            let cloned = items[(*pos).min(items.len())..].to_vec();
                            let nh = self.heap.alloc(Obj::List(cloned));
                            self.push(Value::Obj(nh));
                        }
                        _ => {
                            return Err(self
                                .err(format!("cannot iterate over {}", self.type_name(v)), span));
                        }
                    },
                    other => {
                        return Err(self.err(
                            format!("cannot iterate over {}", self.type_name(other)),
                            span,
                        ));
                    }
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
            Op::IsGenerator => {
                let v = self.pop();
                let is_gen =
                    matches!(v, Value::Obj(h) if matches!(self.heap.get(h), Obj::Generator(_)));
                self.push(Value::Bool(is_gen));
            }
            Op::IterableToCursor => {
                // One-time `for`-entry conversion: a PURE-`Iterable` struct (has `iter`, lacks `next`)
                // becomes its cursor (so the seq path drains it); everything else passes through.
                let v = self.pop();
                let convert = if let Value::Obj(h) = v
                    && let Obj::Struct { name, .. } = self.heap.get(h)
                {
                    let name = name.clone();
                    self.program.structs.get(name.as_ref()).map(|d| {
                        (
                            !d.methods.contains_key("next") && d.methods.contains_key("iter"),
                            d.methods.get("iter").copied(),
                            d.module_idx,
                        )
                    })
                } else {
                    None
                };
                match convert {
                    Some((true, Some(proto), module_idx)) => {
                        let home = self.module_objs[module_idx];
                        // Re-enter the VM to run `iter(self)`; it returns the cursor (the body calls
                        // `self.xs.iter()`). Root the receiver across the call (guarded GC).
                        self.push(v);
                        let cursor = self.guarded(|vm| {
                            vm.run_proto(proto, home, None, vec![v], true, false, span)
                        })?;
                        self.pop(); // unroot receiver
                        self.push(cursor);
                    }
                    // Not a pure-Iterable struct (a struct with `next`, a generator, a collection, …):
                    // unchanged. (A pure-Iterable struct whose `iter` is somehow missing is impossible
                    // — the checker bound it via `struct_iterable_elem`, which requires `iter`.)
                    _ => self.push(v),
                }
            }
            Op::Yield => {
                // Experimental generator suspend. The yielded value is already on the stack top; flag
                // the request and let `run_until` return to the host `.next()` after this op (the
                // frame `ip` has already advanced past the `Yield`, so resume continues after it).
                self.gen_yielding = true;
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
            Op::MatchArm {
                scrut,
                variant,
                variant_id,
                enum_name,
                nbind,
                bind_start,
                next,
            } => self.match_arm(
                *scrut,
                variant,
                *variant_id,
                enum_name.as_deref(),
                *nbind,
                *bind_start,
                *next,
                span,
            )?,
            Op::MatchNoArm(slot) => {
                let v = self.stack[self.base() + *slot];
                let variant = match v {
                    Value::Obj(h) => match self.heap.get(h) {
                        Obj::Enum { variant_id, .. } => self.enum_names(*variant_id).1.to_string(),
                        _ => String::new(),
                    },
                    _ => String::new(),
                };
                return Err(self.err(format!("no match arm for variant '{variant}'"), span));
            }
            Op::EnterNursery => {
                self.nurseries.push(Vec::new());
                self.mn_scopes.push(None); // lockstep — set Some(scope_id) only if early-enlisted
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
                let eager = self.parallel && self.mn.is_some() && worker_count() >= 2;
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
            Op::WaitPoll(meta) => self.op_wait_poll(meta, span)?,
            Op::NewChannel => {
                let h = self
                    .heap
                    .alloc(Obj::Channel(Arc::new(ChannelCore::default())));
                self.push(Value::Obj(h));
            }
            Op::NewShared => {
                let init = self.pop();
                // The box holds the wire form (single serialization == the old deep_clone-in). A
                // non-sendable init (a frame-holding generator) faults gracefully with this Op's span.
                let init = self.to_wire_at(init, span)?;
                let h = self.heap.alloc(Obj::Shared(Arc::new(SharedCore {
                    v: Mutex::new(init),
                    ..Default::default()
                })));
                self.push(Value::Obj(h));
            }
            Op::NewRwShared => {
                let init = self.pop();
                // The box holds the wire form (single serialization == the old deep_clone-in). A
                // non-sendable init (a frame-holding generator) faults gracefully with this Op's span.
                let init = self.to_wire_at(init, span)?;
                let h = self.heap.alloc(Obj::RwShared(Arc::new(RwSharedCore {
                    v: RwLock::new(init),
                    ..Default::default()
                })));
                self.push(Value::Obj(h));
            }
            // `NewAtomic`/`NewTimer` delegate to `#[inline(never)]` helpers so their locals (the timer's
            // `Instant`/`Duration` math) do NOT inflate `step`'s stack frame — `step` is on the per-op
            // recursion path, so a fatter frame here multiplies across a deep call chain (debug builds
            // don't reuse match-arm stack slots) and can overflow the host stack before the
            // `MAX_CALL_DEPTH` guard fires. Keep these cold constructors out of line.
            Op::NewAtomic => {
                let v = self.new_atomic(span)?;
                self.push(v);
            }
            Op::NewTimer => {
                let v = self.new_timer(span)?;
                self.push(v);
            }
            Op::NewExecutor => {
                let h = self
                    .heap
                    .alloc(Obj::Executor(Arc::new(ExecutorCore::default())));
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
    /// `MakeCffi(id)` — eager `dlopen` + `dlsym` at module init from `Program.cffi_defs[id]`, then
    /// push the resolved `Obj::Cffi`. A missing library / symbol surfaces as a runtime error here
    /// (the spec's startup-failure model). `#[inline(never)]` keeps its locals (the cloned `CffiDef`'s
    /// `Vec`s) off `step`'s stack frame, preserving the deep-recursion depth-guard headroom.
    #[inline(never)]
    fn op_make_cffi(&mut self, id: u32, span: Span) -> Result<(), RuntimeError> {
        let def = self.program.cffi_defs[id as usize].clone();
        let cffi = crate::native::cffi::Cffi::new(&def.lib, &def.name, def.params, def.ret)
            .map_err(|e| self.err(e.message, span))?;
        let h = self.heap.alloc(Obj::Cffi(std::sync::Arc::new(cffi)));
        self.push(Value::Obj(h));
        Ok(())
    }

    #[inline(never)]
    fn q_arith(
        &mut self,
        site: usize,
        kind: crate::vm::op::BinKind,
        span: Span,
    ) -> Result<(), RuntimeError> {
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
                let both_int = matches!(
                    (self.stack[n - 2], self.stack[n - 1]),
                    (Value::Int(_), Value::Int(_))
                );
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
            let both_int = matches!(
                (self.stack[n - 2], self.stack[n - 1]),
                (Value::Int(_), Value::Int(_))
            );
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
    fn op_bin_local_local(
        &mut self,
        a: usize,
        b: usize,
        kind: crate::vm::op::BinKind,
        span: Span,
    ) -> Result<(), RuntimeError> {
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
    fn op_bin_local_const(
        &mut self,
        slot: usize,
        val: i64,
        kind: crate::vm::op::BinKind,
        span: Span,
    ) -> Result<(), RuntimeError> {
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
                let v = x
                    .checked_add(delta)
                    .ok_or_else(|| self.err("integer overflow in Add".to_string(), span))?;
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
    fn fast_int_bin(
        &self,
        x: i64,
        y: i64,
        kind: crate::vm::op::BinKind,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        use crate::vm::op::BinKind;
        let v = match kind {
            BinKind::Add => Value::Int(
                x.checked_add(y)
                    .ok_or_else(|| self.err("integer overflow in Add".to_string(), span))?,
            ),
            BinKind::Sub => Value::Int(
                x.checked_sub(y)
                    .ok_or_else(|| self.err("integer overflow in Sub".to_string(), span))?,
            ),
            BinKind::Mul => Value::Int(
                x.checked_mul(y)
                    .ok_or_else(|| self.err("integer overflow in Mul".to_string(), span))?,
            ),
            BinKind::Div => {
                if y == 0 {
                    return Err(self.err("division by zero".to_string(), span));
                }
                Value::Int(
                    x.checked_div(y)
                        .ok_or_else(|| self.err("integer overflow in Div".to_string(), span))?,
                )
            }
            BinKind::Mod => {
                if y == 0 {
                    return Err(self.err("modulo by zero".to_string(), span));
                }
                Value::Int(
                    x.checked_rem(y)
                        .ok_or_else(|| self.err("integer overflow in Mod".to_string(), span))?,
                )
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
    fn run_bin_kind(
        &mut self,
        kind: crate::vm::op::BinKind,
        span: Span,
    ) -> Result<(), RuntimeError> {
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
                        return Err(self.err(
                            format!(
                                "{} by zero",
                                if matches!(op, Op::Div) {
                                    "division"
                                } else {
                                    "modulo"
                                }
                            ),
                            span,
                        ));
                    }
                    Op::Div => a.checked_div(b),
                    Op::Mod => a.checked_rem(b),
                    _ => unreachable!(),
                };
                Value::Int(v.ok_or_else(|| self.err(format!("integer overflow in {name}"), span))?)
            }
            (a, b) if is_numeric(a) && is_numeric(b) => {
                let (x, y) = (as_f64(a), as_f64(b));
                // Float arithmetic is total IEEE-754: division/modulo by zero yields inf/-inf/NaN,
                // never a fault. (The INT arm above still faults on /0 and overflow.)
                Value::Float(match op {
                    Op::Add => x + y,
                    Op::Sub => x - y,
                    Op::Mul => x * y,
                    Op::Div => x / y,
                    Op::Mod => x % y,
                    _ => unreachable!(),
                })
            }
            // Same-newtype arithmetic: `Meters + Meters` etc. UNWRAPS both wrappers, runs the
            // underlying's NATIVE primitive op (identical overflow/div-by-zero/float semantics — it
            // recurses through `self.binary` on the inners), then REWRAPS in the same newtype. This is
            // NOT a user `add` method — it is the underlying's own op (distinct from struct
            // overloading). The checker has rejected `Meters + float` / `Meters + Seconds`, so a
            // mismatched pair never reaches here from typechecked code. Must precede struct_arith.
            (Value::Obj(ha), Value::Obj(hb))
                if matches!(op, Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod)
                    && self.same_newtype_keys(ha, hb) =>
            {
                self.newtype_arith(op, ha, hb, name, span)?
            }
            // Arithmetic overloading: `+`/`-`/`*` on two structs (or two enums) dispatch to
            // `add`/`sub`/`mul` (the `Add`/`Sub`/`Mul` protocols). The checker has verified
            // conformance. Must precede the string-concat `Add` arm below (which would otherwise
            // reject struct+struct).
            (Value::Obj(ha), Value::Obj(hb))
                if matches!(op, Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod)
                    && matches!(self.heap.get(ha), Obj::Struct { .. } | Obj::Enum { .. })
                    && matches!(self.heap.get(hb), Obj::Struct { .. } | Obj::Enum { .. }) =>
            {
                self.struct_arith(op, l, r, span)?
            }
            (Value::Obj(ha), Value::Obj(hb)) if matches!(op, Op::Add) => {
                match (self.heap.get(ha), self.heap.get(hb)) {
                    (Obj::Str(a), Obj::Str(b)) => {
                        let s = format!("{a}{b}");
                        let h = self.heap.alloc(Obj::Str(s.into()));
                        Value::Obj(h)
                    }
                    // List concat (gap #3): `[1,2] + [3,4]` — identical to `.concat` (vm:7688).
                    (Obj::List(a), Obj::List(b)) => {
                        let mut out = a.clone();
                        out.extend(b.iter().copied());
                        Value::Obj(self.heap.alloc(Obj::List(out)))
                    }
                    _ => {
                        return Err(self.err(
                            format!(
                                "cannot apply {name} to {} and {}",
                                self.type_name(l),
                                self.type_name(r)
                            ),
                            span,
                        ));
                    }
                }
            }
            // Set difference (gap #3): `a - b` — identical to `.difference` (vm:7918).
            (Value::Obj(ha), Value::Obj(hb))
                if matches!(op, Op::Sub)
                    && matches!(self.heap.get(ha), Obj::Set(_))
                    && matches!(self.heap.get(hb), Obj::Set(_)) =>
            {
                self.set_op(SetOp::Difference, ha, hb)
            }
            // List repeat (gap #3): `[0] * 3` / `3 * [0]` (commutative, Python-style). `n <= 0` →
            // empty; guard capacity against the Vec overflow abort, like `str.repeat` (vm:7514).
            (Value::Obj(ha), Value::Int(n)) | (Value::Int(n), Value::Obj(ha))
                if matches!(op, Op::Mul) && matches!(self.heap.get(ha), Obj::List(_)) =>
            {
                self.list_repeat(ha, n, span)?
            }
            _ => {
                return Err(self.err(
                    format!(
                        "cannot apply {name} to {} and {}",
                        self.type_name(l),
                        self.type_name(r)
                    ),
                    span,
                ));
            }
        };
        self.push(result);
        Ok(())
    }

    /// `[elem...] * n` — repeat the list `n` times into a fresh list (gap #3). `n <= 0` → empty.
    /// Guards the allocation against capacity overflow (a giant `n` would otherwise abort the
    /// process via Vec's panic) — raises a RECOVERABLE fault, mirroring `str.repeat`. Mirrored
    /// byte-for-byte in `interp::eval_binary`.
    fn list_repeat(&mut self, h: GcRef, n: i64, span: Span) -> Result<Value, RuntimeError> {
        let Obj::List(items) = self.heap.get(h) else {
            unreachable!("list_repeat receiver is a list")
        };
        if n <= 0 {
            return Ok(Value::Obj(self.heap.alloc(Obj::List(Vec::new()))));
        }
        let n = n as usize;
        // Guard the allocation: a giant `n` would abort the process via `Vec`'s capacity panic.
        // Bound the BYTE size (`count * size_of::<Value>()`) by `isize::MAX`, matching `Vec`'s own
        // limit — `str.repeat` does the same on its byte length (vm:7514). Recoverable fault.
        match items
            .len()
            .checked_mul(n)
            .and_then(|t| t.checked_mul(std::mem::size_of::<Value>()))
            .filter(|&bytes| bytes <= isize::MAX as usize)
        {
            Some(_) => {
                let src = items.clone();
                let total = src.len() * n;
                // The outer guard only bounds the byte size by `isize::MAX`; a huge-but-representable
                // total (e.g. 1e17) still passes it yet cannot actually be allocated, and
                // `Vec::with_capacity` would ABORT the process. `try_reserve_exact` converts that
                // into the same recoverable fault.
                let mut out: Vec<Value> = Vec::new();
                if out.try_reserve_exact(total).is_err() {
                    return Err(self.err("list repeat capacity overflow".to_string(), span));
                }
                for _ in 0..n {
                    out.extend(src.iter().copied());
                }
                Ok(Value::Obj(self.heap.alloc(Obj::List(out))))
            }
            None => Err(self.err("list repeat capacity overflow".to_string(), span)),
        }
    }

    /// Set algebra for the operator forms `| & - ^` (gap #3). Mirrors the
    /// `union`/`intersection`/`difference` set methods (vm:7918) using the cached per-element
    /// hashes (no re-hashing, no user re-entry). `^` (symmetric-difference) has no method form:
    /// it is the union of (mine ∉ other) THEN (other ∉ mine), in that canonical insertion order so
    /// the result's print order is deterministic and parity-equal with the interpreter.
    fn set_op(&mut self, op: SetOp, ha: GcRef, hb: GcRef) -> Value {
        let mine = match self.heap.get(ha) {
            Obj::Set(s) => s.entries.clone(),
            _ => unreachable!(),
        };
        let other = match self.heap.get(hb) {
            Obj::Set(s) => s.entries.clone(),
            _ => unreachable!(),
        };
        let mut out = SetData::default();
        let add = |vm: &Vm, set: &mut SetData, he: u64, e: Value| {
            if !set
                .candidates(he)
                .iter()
                .any(|&p| vm.values_equal(set.entries[p].1, e))
            {
                set.push(he, e);
            }
        };
        let in_set = |vm: &Vm, set: &[(u64, Value)], he: u64, e: Value| {
            set.iter()
                .any(|&(h2, e2)| h2 == he && vm.values_equal(e2, e))
        };
        match op {
            SetOp::Union => {
                for (he, e) in mine.iter().chain(other.iter()) {
                    add(self, &mut out, *he, *e);
                }
            }
            SetOp::Intersection => {
                for (he, e) in &mine {
                    if in_set(self, &other, *he, *e) {
                        add(self, &mut out, *he, *e);
                    }
                }
            }
            SetOp::Difference => {
                for (he, e) in &mine {
                    if !in_set(self, &other, *he, *e) {
                        add(self, &mut out, *he, *e);
                    }
                }
            }
            SetOp::SymmetricDifference => {
                for (he, e) in &mine {
                    if !in_set(self, &other, *he, *e) {
                        add(self, &mut out, *he, *e);
                    }
                }
                for (he, e) in &other {
                    if !in_set(self, &mine, *he, *e) {
                        add(self, &mut out, *he, *e);
                    }
                }
            }
        }
        Value::Obj(self.heap.alloc(Obj::Set(out)))
    }

    /// Arithmetic operator overloading: dispatch `+`/`-`/`*` on two structs to the receiver's
    /// `add`/`sub`/`mul(self, other) -> Self` method (the `Add`/`Sub`/`Mul` protocols). `l`/`r` are
    /// passed as the call's args (rooted as the new frame's locals). Mirrors `interp::struct_arith`.
    fn struct_arith(
        &mut self,
        op: &Op,
        l: Value,
        r: Value,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let method = match op {
            Op::Add => "add",
            Op::Sub => "sub",
            Op::Mul => "mul",
            Op::Div => "div",
            Op::Mod => "mod",
            _ => unreachable!("struct_arith only handles + - * / %"),
        };
        let (proto, home) = self.resolve_overload_method(l, method, span)?;
        self.guarded(|vm| vm.run_proto(proto, home, None, vec![l, r], true, false, span))
    }

    /// Do `ha` and `hb` both hold a newtype with the SAME runtime key? (Drives same-type operator
    /// auto-flow — `Meters + Meters`, never `Meters + Seconds`.)
    fn same_newtype_keys(&self, ha: GcRef, hb: GcRef) -> bool {
        match (self.heap.get(ha), self.heap.get(hb)) {
            (Obj::NewType { type_key: a, .. }, Obj::NewType { type_key: b, .. }) => a == b,
            _ => false,
        }
    }

    /// Same-newtype arithmetic: unwrap both inners, run the underlying's NATIVE primitive op (via the
    /// scalar `arith_scalar` core — identical overflow/div-by-zero/float semantics as a raw int/float
    /// op), then REWRAP in the same newtype key. NOT a user method (distinct from struct overloading).
    fn newtype_arith(
        &mut self,
        op: &Op,
        ha: GcRef,
        hb: GcRef,
        name: &str,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let (key, a) = match self.heap.get(ha) {
            Obj::NewType { type_key, inner } => (type_key.clone(), *inner),
            _ => unreachable!(),
        };
        let b = match self.heap.get(hb) {
            Obj::NewType { inner, .. } => *inner,
            _ => unreachable!(),
        };
        let inner = self.arith_scalar(op, a, b, name, span)?;
        Ok(Value::Obj(self.heap.alloc(Obj::NewType {
            type_key: key,
            inner,
        })))
    }

    /// The underlying primitive `+`/`-`/`*`/`/`/`%` on two scalar values (int or float), with the
    /// SAME overflow / division-by-zero / float semantics as the inline `binary` arms. Shared by the
    /// newtype same-type operator path so it byte-matches a raw int/float op.
    fn arith_scalar(
        &self,
        op: &Op,
        a: Value,
        b: Value,
        name: &str,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match (a, b) {
            (Value::Int(a), Value::Int(b)) => {
                let v = match op {
                    Op::Add => a.checked_add(b),
                    Op::Sub => a.checked_sub(b),
                    Op::Mul => a.checked_mul(b),
                    Op::Div | Op::Mod if b == 0 => {
                        let kind = if matches!(op, Op::Div) {
                            "division"
                        } else {
                            "modulo"
                        };
                        return Err(self.err(format!("{kind} by zero"), span));
                    }
                    Op::Div => a.checked_div(b),
                    Op::Mod => a.checked_rem(b),
                    _ => unreachable!(),
                };
                Ok(Value::Int(v.ok_or_else(|| {
                    self.err(format!("integer overflow in {name}"), span)
                })?))
            }
            (a, b) if is_numeric(a) && is_numeric(b) => {
                let (x, y) = (as_f64(a), as_f64(b));
                // Float arithmetic is total IEEE-754: division/modulo by zero yields inf/-inf/NaN,
                // never a fault. (The INT arm above still faults on /0 and overflow.)
                Ok(Value::Float(match op {
                    Op::Add => x + y,
                    Op::Sub => x - y,
                    Op::Mul => x * y,
                    Op::Div => x / y,
                    Op::Mod => x % y,
                    _ => unreachable!(),
                }))
            }
            _ => Err(self.err(
                format!(
                    "cannot apply {name} to {} and {}",
                    self.type_name(a),
                    self.type_name(b)
                ),
                span,
            )),
        }
    }

    /// Resolve `(proto, home_module_obj)` for an operator-overload method `method` on receiver `recv`
    /// — a struct (via `program.structs`) or an enum (via `program.enum_methods` + `enum_home`). The
    /// shared dispatch core for arithmetic and ordering overloads on both struct and enum values.
    fn resolve_overload_method(
        &self,
        recv: Value,
        method: &str,
        span: Span,
    ) -> Result<(usize, GcRef), RuntimeError> {
        let Value::Obj(h) = recv else { unreachable!() };
        match self.heap.get(h) {
            Obj::Struct { name, .. } => {
                let name = name.clone();
                let def = self
                    .program
                    .structs
                    .get(name.as_ref())
                    .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
                let proto = *def.methods.get(method).ok_or_else(|| {
                    self.err(
                        format!("struct '{}' has no '{method}' method", def.display_name),
                        span,
                    )
                })?;
                Ok((proto, self.module_objs[def.module_idx]))
            }
            Obj::Enum { variant_id, .. } => {
                let key = self.enum_names(*variant_id).0.to_string();
                let proto = *self
                    .program
                    .enum_methods
                    .get(&key)
                    .and_then(|ms| ms.get(method))
                    .ok_or_else(|| {
                        self.err(
                            format!(
                                "enum '{}' has no '{method}' method",
                                crate::compiler::bare_display(&key)
                            ),
                            span,
                        )
                    })?;
                Ok((proto, self.module_objs[self.enum_home_module(&key)]))
            }
            // A newtype's overload/hook methods (`hash`/`str`/user methods) resolve via
            // `newtype_methods`, mirroring the enum path.
            Obj::NewType { type_key, .. } => {
                let key = type_key.to_string();
                let proto = *self
                    .program
                    .newtype_methods
                    .get(&key)
                    .and_then(|ms| ms.get(method))
                    .ok_or_else(|| {
                        self.err(
                            format!(
                                "newtype '{}' has no '{method}' method",
                                crate::compiler::bare_display(&key)
                            ),
                            span,
                        )
                    })?;
                Ok((proto, self.module_objs[self.newtype_home_module(&key)]))
            }
            _ => unreachable!("overload receiver is a struct, enum, or newtype"),
        }
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
                            return Err(
                                self.err(format!("shift amount {b} out of range (0..64)"), span)
                            );
                        }
                        if matches!(op, Op::Shl) {
                            // Left shift can overflow (drop high bits) like `+ - * / %`; treat
                            // it as a recoverable fault, not a silent wrap. Round-trip test:
                            // `(a << b) >> b == a` holds iff no significant bit was shifted out
                            // (correct for negative operands too — `-1 << 63` round-trips).
                            let v = a << (b as u32);
                            if (v >> (b as u32)) != a {
                                return Err(self.err(format!("integer overflow in {name}"), span));
                            }
                            v
                        } else {
                            a >> (b as u32)
                        }
                    }
                    _ => unreachable!(),
                };
                Value::Int(v)
            }
            // Set algebra (gap #3): `|`→union, `&`→intersection, `^`→symmetric-difference on two
            // sets. (`<< >>` stay int-only and fall through to the error below.) Identical to the
            // `.union`/`.intersection` methods; `^` has no method form. Mirrors interp.
            (Value::Obj(ha), Value::Obj(hb))
                if matches!(op, Op::BitOr | Op::BitAnd | Op::BitXor)
                    && matches!(self.heap.get(ha), Obj::Set(_))
                    && matches!(self.heap.get(hb), Obj::Set(_)) =>
            {
                let set_op = match op {
                    Op::BitOr => SetOp::Union,
                    Op::BitAnd => SetOp::Intersection,
                    _ => SetOp::SymmetricDifference,
                };
                self.set_op(set_op, ha, hb)
            }
            _ => {
                return Err(self.err(
                    format!(
                        "cannot apply {name} to {} and {}",
                        self.type_name(l),
                        self.type_name(r)
                    ),
                    span,
                ));
            }
        };
        self.push(result);
        Ok(())
    }

    /// `x in container` — membership test. Pops `[x, container]`, pushes a `Bool`. Dispatches on the
    /// container kind, reusing the same equality / hashing the `.contains`/`.has` methods use: a
    /// list/set tests element membership, a map tests KEY membership (Python-style), a str tests
    /// substring. The checker has already type-directed this; the runtime is the fallback.
    /// `#[inline(never)]` keeps `step`'s own stack frame lean (its String/Vec locals would otherwise
    /// bloat the deep-recursion path `step → run_proto → run_until → step`).
    #[inline(never)]
    fn op_contains(&mut self, span: Span) -> Result<(), RuntimeError> {
        let container = self.pop();
        let needle = self.pop();
        let found = match container {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::List(items) => {
                    let elems = items.clone();
                    elems.iter().any(|v| self.values_equal(*v, needle))
                }
                Obj::Set(_) => {
                    let hx = self.hash_key_rooted(needle, &[Value::Obj(h), needle], span)?;
                    let Obj::Set(s) = self.heap.get(h) else {
                        unreachable!()
                    };
                    s.candidates(hx)
                        .iter()
                        .any(|&p| self.values_equal(s.entries[p].1, needle))
                }
                Obj::Map(_) => {
                    let hk = self.hash_key_rooted(needle, &[Value::Obj(h), needle], span)?;
                    let Obj::Map(m) = self.heap.get(h) else {
                        unreachable!()
                    };
                    m.candidates(hk)
                        .iter()
                        .any(|&p| self.values_equal(m.entries[p].1, needle))
                }
                Obj::Str(_) => {
                    let Value::Obj(nh) = needle else {
                        return Err(self.err(
                            format!(
                                "substring `in` requires a str on the left, found {}",
                                self.type_name(needle)
                            ),
                            span,
                        ));
                    };
                    let sub = match self.heap.get(nh) {
                        Obj::Str(sub) => sub.to_string(),
                        _ => {
                            return Err(self.err(
                                format!(
                                    "substring `in` requires a str on the left, found {}",
                                    self.type_name(needle)
                                ),
                                span,
                            ));
                        }
                    };
                    let Obj::Str(hay) = self.heap.get(h) else {
                        unreachable!()
                    };
                    hay.contains(sub.as_str())
                }
                _ => {
                    return Err(self.err(
                        format!("cannot use `in` on {}", self.type_name(container)),
                        span,
                    ));
                }
            },
            _ => {
                return Err(self.err(
                    format!("cannot use `in` on {}", self.type_name(container)),
                    span,
                ));
            }
        };
        self.push(Value::Bool(found));
        Ok(())
    }

    fn compare_op(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        let r = self.pop();
        let l = self.pop();
        // Same-newtype ordering: `Meters < Meters` UNWRAPS both and compares the underlyings with
        // their NATIVE ordering (the checker rejected `Meters < float` / `< Seconds`). Not a user
        // `compare` method — the underlying's native compare. Must precede the struct/enum overload.
        if let (Value::Obj(hl), Value::Obj(hr)) = (l, r)
            && self.same_newtype_keys(hl, hr)
        {
            let a = match self.heap.get(hl) {
                Obj::NewType { inner, .. } => *inner,
                _ => unreachable!(),
            };
            let b = match self.heap.get(hr) {
                Obj::NewType { inner, .. } => *inner,
                _ => unreachable!(),
            };
            let bres = self.ordered_bool(op, a, b, span)?;
            self.push(Value::Bool(bres));
            return Ok(());
        }
        // Operator overloading: ordering on two structs dispatches to `compare(self, other) -> int`
        // (the `Comparable` protocol). The checker has verified conformance. Equality stays
        // structural; only ordering is overloaded. Mirrors `interp::struct_ordering`.
        if let (Value::Obj(hl), Value::Obj(hr)) = (l, r)
            && matches!(self.heap.get(hl), Obj::Struct { .. } | Obj::Enum { .. })
            && matches!(self.heap.get(hr), Obj::Struct { .. } | Obj::Enum { .. })
        {
            return self.struct_ordering(op, l, r, span);
        }
        let b = self.ordered_bool(op, l, r, span)?;
        self.push(Value::Bool(b));
        Ok(())
    }

    /// Map an ordering operator (`< <= > >=`) over two values to a bool. `compare` returns `None` for
    /// two reasons we MUST distinguish: (1) both operands numeric ⇒ a NaN is involved — every ordered
    /// compare against NaN is `false` (IEEE-754 / Python / Rust parity), never a fault; (2) genuinely
    /// incomparable TYPES (the `_ => None` fallthrough, e.g. str vs int) ⇒ keep the existing fault.
    /// `Ordering` has no "unordered" value, so the NaN case is special-cased here before the
    /// is_lt/is_le/is_gt/is_ge match — encoding it as a fake `Ordering` would make exactly one of the
    /// four ops true. Mirrors `interp::eval_binary`'s `Lt|LtEq|Gt|GtEq` arm.
    fn ordered_bool(&self, op: &Op, a: Value, b: Value, span: Span) -> Result<bool, RuntimeError> {
        match self.compare(a, b) {
            Some(ord) => Ok(match op {
                Op::Lt => ord.is_lt(),
                Op::LtEq => ord.is_le(),
                Op::Gt => ord.is_gt(),
                Op::GtEq => ord.is_ge(),
                _ => unreachable!(),
            }),
            // Both numeric ⇒ NaN is involved ⇒ false for all four ops.
            None if is_numeric(a) && is_numeric(b) => Ok(false),
            // Genuinely-incomparable types: unreachable from well-typed source (the checker rejects
            // e.g. `str < int`); kept for internal-invariant safety.
            None => {
                let name = match op {
                    Op::Lt => "Lt",
                    Op::LtEq => "LtEq",
                    Op::Gt => "Gt",
                    Op::GtEq => "GtEq",
                    _ => unreachable!(),
                };
                Err(self.err(
                    format!(
                        "cannot apply {name} to {} and {}",
                        self.type_name(a),
                        self.type_name(b)
                    ),
                    span,
                ))
            }
        }
    }

    /// Dispatch an ordering operator on two structs to the receiver's `compare(self, other) -> int`
    /// method, mapping the sign of the result to a boolean. Mirrors `interp::struct_ordering`.
    fn struct_ordering(
        &mut self,
        op: &Op,
        l: Value,
        r: Value,
        span: Span,
    ) -> Result<(), RuntimeError> {
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
    fn struct_compare(
        &mut self,
        l: Value,
        r: Value,
        span: Span,
    ) -> Result<std::cmp::Ordering, RuntimeError> {
        let (proto, home) = self.resolve_overload_method(l, "compare", span)?;
        match self.guarded(|vm| vm.run_proto(proto, home, None, vec![l, r], true, false, span))? {
            Value::Int(n) => Ok(n.cmp(&0)),
            other => Err(self.err(
                format!("compare() must return int, got {}", self.type_name(other)),
                span,
            )),
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
                // An enum key dispatches its user `hash(self) -> int` via the shared enum-aware
                // resolver, mirroring the struct path (re-entrant — may allocate / trigger GC).
                Obj::Enum { .. } => self.enum_hash(v, span),
                // A newtype key dispatches its user `hash(self) -> int` (opt-in — the checker rejects
                // a newtype with no `hash` as a key, even over an intrinsically-hashable underlying).
                Obj::NewType { .. } => self.newtype_hash(v, span),
                Obj::Str(_) | Obj::Bytes(_) => Ok(self.scalar_hash(v)),
                _ => Err(self.err(
                    format!(
                        "{} is not hashable (cannot be a map/set key)",
                        self.type_name(v)
                    ),
                    span,
                )),
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
                // `bytes` is Hashable (immutable, value-compared). Hash the raw slice — mandatory so
                // `Map[bytes, T]`/`Set[bytes]` keys distribute instead of all colliding on `0`.
                Obj::Bytes(b) => {
                    let mut hr = std::collections::hash_map::DefaultHasher::new();
                    b.as_ref().hash(&mut hr);
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
        let Obj::Struct { name, .. } = self.heap.get(h).clone() else {
            unreachable!()
        };
        let def = self
            .program
            .structs
            .get(name.as_ref())
            .cloned()
            .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        // A ZERO-FIELD struct with no `hash` method hashes to a constant (0): it has no state, so
        // there is nothing to hash. `==`'s type-tag guard keeps distinct empty-struct types unequal
        // despite the shared hash. Mirrors the checker's zero-field `Hashable` intrinsic and the
        // interpreter's identical constant (two-engine parity).
        if def.fields.is_empty() && !def.methods.contains_key("hash") {
            return Ok(0);
        }
        let proto = *def.methods.get("hash").ok_or_else(|| {
            self.err(
                format!(
                    "struct '{}' has no 'hash' method (needed to use it as a map/set key)",
                    def.display_name
                ),
                span,
            )
        })?;
        let home = self.module_objs[def.module_idx];
        match self.guarded(|vm| vm.run_proto(proto, home, None, vec![v], true, false, span))? {
            Value::Int(n) => Ok(n as u64),
            other => Err(self.err(
                format!("hash() must return int, got {}", self.type_name(other)),
                span,
            )),
        }
    }

    /// Dispatch an enum key's user `hash(self) -> int` via the shared enum-aware
    /// [`resolve_overload_method`], mirroring [`struct_hash`] (re-entrant via `run_proto`).
    fn enum_hash(&mut self, v: Value, span: Span) -> Result<u64, RuntimeError> {
        let (proto, home) = self.resolve_overload_method(v, "hash", span)?;
        match self.guarded(|vm| vm.run_proto(proto, home, None, vec![v], true, false, span))? {
            Value::Int(n) => Ok(n as u64),
            other => Err(self.err(
                format!("hash() must return int, got {}", self.type_name(other)),
                span,
            )),
        }
    }

    /// Dispatch a newtype key's user `hash(self) -> int` via the shared resolver (mirrors `enum_hash`;
    /// re-entrant via `run_proto`). The checker guarantees a key-used newtype defines `hash`.
    fn newtype_hash(&mut self, v: Value, span: Span) -> Result<u64, RuntimeError> {
        let (proto, home) = self.resolve_overload_method(v, "hash", span)?;
        match self.guarded(|vm| vm.run_proto(proto, home, None, vec![v], true, false, span))? {
            Value::Int(n) => Ok(n as u64),
            other => Err(self.err(
                format!("hash() must return int, got {}", self.type_name(other)),
                span,
            )),
        }
    }

    /// Hash `key`, keeping `roots` alive on the operand stack across the call. A struct key's
    /// `hash()` re-enters the VM and can trigger GC; the map/set receiver and any in-flight
    /// key/value (already popped off the stack before dispatch) must be rooted or the collector
    /// could free them mid-hash. For scalar keys this is a couple of redundant push/pops.
    fn hash_key_rooted(
        &mut self,
        key: Value,
        roots: &[Value],
        span: Span,
    ) -> Result<u64, RuntimeError> {
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
    fn msort_indices_structs(
        &mut self,
        src_h: GcRef,
        idx: Vec<usize>,
        span: Span,
    ) -> Result<Vec<usize>, RuntimeError> {
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
                // `bytes`/`bytearray` order lexicographically by byte (Python parity), including
                // cross-type (Python `b"a" < bytearray(b"b")` compares by content).
                (Obj::Bytes(a), Obj::Bytes(b)) => Some(a.cmp(b)),
                (Obj::ByteArray(a), Obj::ByteArray(b)) => Some(a.cmp(b)),
                (Obj::Bytes(a), Obj::ByteArray(b)) => Some(a.as_ref().cmp(b.as_slice())),
                (Obj::ByteArray(a), Obj::Bytes(b)) => Some(a.as_slice().cmp(b.as_ref())),
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
        self.values_equal_guarded(l, r, 0, Span { line: 1, col: 1 })
            .unwrap_or(false)
    }

    /// Depth-guarded structural equality. Returns `Err` (recoverable) once recursion exceeds
    /// [`MAX_STRUCTURAL_DEPTH`] — guarding against cyclic data structures overflowing the host stack.
    fn values_equal_guarded(
        &self,
        l: Value,
        r: Value,
        depth: usize,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        if depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.err(
                "maximum structural depth (10000) exceeded (cyclic data structure?)".to_string(),
                span,
            ));
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
                    (Obj::Bytes(a), Obj::Bytes(b)) => Ok(a == b),
                    // `bytearray` equality is structural byte-equality. Cross-type `bytes ==
                    // bytearray` is content-equal (Python parity: `b"a" == bytearray(b"a")` is true).
                    (Obj::ByteArray(a), Obj::ByteArray(b)) => Ok(a == b),
                    (Obj::Bytes(a), Obj::ByteArray(b)) => Ok(a.as_ref() == b.as_slice()),
                    (Obj::ByteArray(a), Obj::Bytes(b)) => Ok(a.as_slice() == b.as_ref()),
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
                        let ae: Vec<(Value, Value)> =
                            a.entries.iter().map(|(_, k, v)| (*k, *v)).collect();
                        let be: Vec<(Value, Value)> =
                            b.entries.iter().map(|(_, k, v)| (*k, *v)).collect();
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
                    (
                        Obj::Struct {
                            name: na,
                            fields: fa,
                            ..
                        },
                        Obj::Struct {
                            name: nb,
                            fields: fb,
                            ..
                        },
                    ) => {
                        // Positional structural compare: the `na != nb` guard preserves type
                        // distinction (same name ⇒ same StructDef ⇒ identical field order), so a
                        // by-position value compare suffices — no per-field name clone needed.
                        if na != nb || fa.len() != fb.len() {
                            return Ok(false);
                        }
                        let fa: Vec<Value> = fa.clone();
                        let fb: Vec<Value> = fb.clone();
                        for (va, vb) in fa.iter().zip(&fb) {
                            if !self.values_equal_guarded(*va, *vb, depth + 1, span)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                    (
                        Obj::Enum {
                            variant_id: va,
                            payload: pa,
                        },
                        Obj::Enum {
                            variant_id: vb,
                            payload: pb,
                        },
                    ) => {
                        // M19 lever #2 — equal `variant_id` ⟹ same enum type AND variant (ids are
                        // globally unique per (enum, variant) pair), so this one int compare subsumes the
                        // old `ty == ty && variant == variant`.
                        if va != vb || pa.len() != pb.len() {
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
                    // Two newtypes are equal iff they are the SAME newtype (key) and their inners are
                    // structurally equal. A different key is a distinct type ⇒ never equal.
                    (
                        Obj::NewType {
                            type_key: ka,
                            inner: ia,
                        },
                        Obj::NewType {
                            type_key: kb,
                            inner: ib,
                        },
                    ) => {
                        if ka != kb {
                            return Ok(false);
                        }
                        let (ia, ib) = (*ia, *ib);
                        self.values_equal_guarded(ia, ib, depth + 1, span)
                    }
                    // Two opaque `ptr` handles are equal iff they hold the same raw address (identity).
                    // Distinct heap slots can wrap the same address (e.g. a re-`from_wire`'d handle or
                    // `std.ffi.null()` twice), so the same-`GcRef` shortcut above is not enough.
                    (Obj::Ptr(a), Obj::Ptr(b)) => Ok(a == b),
                    // Two first-class builtin-fn values are equal iff they name the SAME builtin. Each
                    // value-position use emits a fresh `Op::LoadBuiltin` → a distinct handle, so the
                    // `ha == hb` identity short-circuit above never fires; compare by name to match the
                    // interp (derived `PartialEq` on `Value::Builtin`'s `Rc<str>`) — VM==interp parity.
                    (Obj::Builtin(a), Obj::Builtin(b)) => Ok(a == b),
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
                    self.check_arity(
                        "function",
                        &self.program.protos[proto].name,
                        arity,
                        argc,
                        span,
                    )?;
                } else if argc != arity {
                    return Err(self.err(
                        format!("closure expects {arity} argument(s), got {argc}"),
                        span,
                    ));
                }
                // Experimental generators — calling a generator function does NOT run its body; it
                // allocates a suspendable generator object over the args. (Arity was just checked.)
                if self.program.protos[proto].is_generator {
                    let args: Vec<Value> = self.stack.split_off(at); // the argc args
                    self.stack.pop(); // drop the callee left beneath them
                    let g = self.alloc_generator(proto, home, clo, args);
                    self.push(g);
                    return Ok(());
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
    fn invoke_value(
        &mut self,
        callee: Value,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let argc = args.len();
        match callee {
            Value::Obj(h) => {
                // Borrow the heap object only long enough to read its `Copy` fields. The old code
                // `self.heap.get(h).clone()` deep-cloned the whole `Obj` on *every* call — for a
                // closure that meant cloning its captured-environment `HashMap` each time — just to
                // read `proto`/`home`. `Native` still clones its (small) name `String`, but the hot
                // user-function/closure paths now copy three scalars and allocate nothing.
                enum Callee {
                    Func {
                        proto: ProtoId,
                        home: GcRef,
                    },
                    Closure {
                        proto: ProtoId,
                        home: GcRef,
                    },
                    Native {
                        func: crate::native::NativeFn,
                        name: Box<str>,
                    },
                    Builtin(Box<str>),
                    Cffi(std::sync::Arc<crate::native::cffi::Cffi>),
                    NotCallable,
                }
                let kind = match self.heap.get(h) {
                    Obj::Func { proto, home } => Callee::Func {
                        proto: *proto,
                        home: *home,
                    },
                    Obj::Closure { proto, home, .. } => Callee::Closure {
                        proto: *proto,
                        home: *home,
                    },
                    Obj::Native { func, name } => Callee::Native {
                        func: *func,
                        name: name.clone(),
                    },
                    Obj::Builtin(name) => Callee::Builtin(name.clone()),
                    Obj::Cffi(c) => Callee::Cffi(std::sync::Arc::clone(c)),
                    _ => Callee::NotCallable,
                };
                match kind {
                    Callee::Func { proto, home } => {
                        // `&...name` (no clone): `check_arity` only formats the message on mismatch.
                        self.check_arity(
                            "function",
                            &self.program.protos[proto].name,
                            self.program.protos[proto].arity,
                            argc,
                            span,
                        )?;
                        // Experimental generators — allocate, don't run (see `do_call`'s fast path).
                        if self.program.protos[proto].is_generator {
                            return Ok(self.alloc_generator(proto, home, None, args));
                        }
                        self.run_proto(proto, home, None, args, true, false, span)
                    }
                    Callee::Closure { proto, home } => {
                        if argc != self.program.protos[proto].arity {
                            return Err(self.err(
                                format!(
                                    "closure expects {} argument(s), got {argc}",
                                    self.program.protos[proto].arity
                                ),
                                span,
                            ));
                        }
                        if self.program.protos[proto].is_generator {
                            return Ok(self.alloc_generator(proto, home, Some(h), args));
                        }
                        self.run_proto(proto, home, Some(h), args, true, false, span)
                    }
                    Callee::Native { func, name } => self.invoke_native(func, &name, args, span),
                    // A first-class universe builtin fn value (`print`/`ord`/`chr`/`panic`) — route
                    // back into the SAME logic direct calls use. `print` replicates `do_print`'s
                    // value-form defaults (space-join + trailing '\n'; sep=/end= are direct-call-only
                    // via `CallPrintSep`). `panic` returns `Err` (mirrors `do_builtin`'s panic arm) so
                    // defers still unwind. `ord`/`chr` reuse `builtin_ord`/`builtin_chr` directly.
                    Callee::Builtin(name) => match name.as_ref() {
                        "print" => {
                            // ROOT the args on the operand stack while stringifying. `args` was
                            // `split_off` the stack (do_call slow path), so it is NOT a GC root; a
                            // `Stringable` `str` method runs user code that can `collect()` at a
                            // safepoint and would sweep the LATER (still-unrendered) args — a
                            // use-after-free. `do_print` guards this exact hazard by keeping the args
                            // on the operand stack across the whole stringify loop; mirror it here:
                            // push them back, render from the rooted slots, then truncate.
                            let at = self.stack.len();
                            for v in &args {
                                self.push(*v);
                            }
                            let mut parts = Vec::with_capacity(args.len());
                            for i in 0..args.len() {
                                let v = self.stack[at + i];
                                parts.push(self.stringify(v, span, 0)?);
                            }
                            self.stack.truncate(at);
                            self.out.push_str(&parts.join(" "));
                            self.out.push('\n');
                            Ok(Value::Nil)
                        }
                        "ord" => self.builtin_ord(&args, span),
                        "chr" => self.builtin_chr(&args, span),
                        "panic" => {
                            let message = match args.first() {
                                Some(Value::Obj(h)) => match self.heap.get(*h) {
                                    Obj::Str(s) => s.to_string(),
                                    _ => self.type_name(args[0]).to_string(),
                                },
                                Some(other) => self.type_name(*other).to_string(),
                                None => String::new(),
                            };
                            Err(self.err(message, span))
                        }
                        _ => unreachable!("non-first-class builtin {name} reached invoke_value"),
                    },
                    Callee::Cffi(cffi) => {
                        // Arity is checker-guaranteed, but guard defensively (a hand-built program
                        // could bypass the checker) so a wrong arg count never indexes out of bounds.
                        self.check_arity("function", cffi.name(), cffi.param_count(), argc, span)?;
                        let mut host = VmHost { vm: self, args };
                        let ret = cffi.call(&mut host).map_err(|e| RuntimeError {
                            message: e.message,
                            span,
                        })?;
                        Ok(self.lower_native(ret))
                    }
                    Callee::NotCallable => Err(self.err(
                        format!("'{}' is not callable", self.type_name(callee)),
                        span,
                    )),
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
                    ms.map(|ms| OffloadReq {
                        func,
                        args: nargs,
                        span,
                        timer_ms: Some(ms),
                    })
                }
                _ => Some(OffloadReq {
                    func,
                    args: nargs,
                    span,
                    timer_ms: None,
                }),
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
        let ret = func(&mut host).map_err(|e| RuntimeError {
            message: e.message,
            span,
        })?;
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
                    // A `Map[str, str]` arg (today only `request`'s headers) is snapshotted into
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
                    // A `List[str]` arg (today only `run_args`'s argv) is snapshotted into owned
                    // strings so it survives the off-heap handoff. Any non-str element reverts to
                    // `None` → run inline (the checker guarantees str for typed code).
                    Obj::List(items) => {
                        let mut out = Vec::with_capacity(items.len());
                        for v in items {
                            let Value::Obj(eh) = v else {
                                return None;
                            };
                            let Obj::Str(s) = self.heap.get(*eh) else {
                                return None;
                            };
                            out.push(s.to_string());
                        }
                        Some(A::List(out))
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
    /// Map a SCALAR callback result [`Value`] back to an engine-neutral [`crate::native::NativeRet`]
    /// for the FFI trampoline to write into C's return slot (the reverse of `lower_native`, scalar-
    /// only: a callback return is checker-restricted to int/float/bool/ptr). A non-scalar is a
    /// checker-prevented case; default to `Int(0)` defensively (the trampoline then writes a zeroed
    /// register, never UB).
    fn value_to_native_ret(&self, v: Value) -> crate::native::NativeRet {
        use crate::native::NativeRet as N;
        match v {
            Value::Int(n) => N::Int(n),
            Value::Float(f) => N::Float(f),
            Value::Bool(b) => N::Bool(b),
            Value::Nil => N::Nil,
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Ptr(a) => N::Ptr(*a),
                _ => N::Int(0),
            },
        }
    }

    fn lower_native(&mut self, ret: crate::native::NativeRet) -> Value {
        use crate::native::NativeRet as N;
        match ret {
            N::Int(n) => Value::Int(n),
            N::Float(f) => Value::Float(f),
            N::Bool(b) => Value::Bool(b),
            N::Nil => Value::Nil,
            N::Ptr(a) => Value::Obj(self.heap.alloc(Obj::Ptr(a))),
            N::Str(s) => self.alloc_str(s),
            N::List(items) => {
                let mut vs = Vec::with_capacity(items.len());
                for x in items {
                    vs.push(self.lower_native(x));
                }
                Value::Obj(self.heap.alloc(Obj::List(vs)))
            }
            N::Struct { name, fields } => {
                // Positional layout: lower the named native fields, then place them into a flat Vec
                // at the StructDef's declaration-order index (native emit order already matches, but
                // resolving by name keeps it robust to drift). Lower first (each may allocate), then
                // allocate the struct — keeps every allocation at this boundary (GC invariant).
                let tid = self.struct_tid(&name);
                let order: Option<Vec<String>> =
                    self.program.structs.get(&name).map(|d| d.fields.clone());
                let mut lowered: Vec<(Box<str>, Value)> = Vec::with_capacity(fields.len());
                for (k, v) in fields {
                    let lv = self.lower_native(v);
                    lowered.push((k.into_boxed_str(), lv));
                }
                let fs: Vec<Value> = match order {
                    // Registered type: place each lowered value at its declaration-order slot.
                    Some(order) => order
                        .iter()
                        .map(|fname| {
                            lowered
                                .iter()
                                .find(|(k, _)| k.as_ref() == fname.as_str())
                                .map(|(_, v)| *v)
                                .unwrap_or(Value::Nil)
                        })
                        .collect(),
                    // Ad-hoc / unregistered (TID_NONE): keep native emit order positionally.
                    None => lowered.into_iter().map(|(_, v)| v).collect(),
                };
                Value::Obj(self.heap.alloc(Obj::Struct {
                    name: name.into_boxed_str(),
                    tid,
                    fields: fs,
                }))
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

    /// Build a NATIVE `Result`/`Option` enum instance (the native / std construction path: `Ok`/`Err`/
    /// `Some`/`None`, list `pop`, regex/request/json/fs returns). M19 lever #2 — stamps the FIXED native
    /// `VID_OK`/`VID_ERR`/`VID_SOME`/`VID_NONE_VARIANT` constant DIRECTLY, never a name lookup through
    /// `Program::variants`: a user enum declaring a variant named `Ok`/`Err`/`Some`/`None` SHADOWS that
    /// name in the `variants` map (its own dense id at `4..`), so resolving by name here would stamp the
    /// user's id onto a genuine native value — collapsing native-vs-user identity (broken `==`) and
    /// missing `?`'s `variant_id == VID_SOME`/`VID_OK` gate. The reserved 0..=3 ids are disjoint from
    /// every user id, so stamping the constant keeps native and user variants distinguishable. `ty` is
    /// retained in the signature so call sites read self-documentingly, but it is not stored.
    fn alloc_enum(&mut self, ty: &str, variant: &str, payload: Vec<Value>) -> Value {
        let _ = ty;
        use crate::vm::op::{VID_ERR, VID_NONE, VID_NONE_VARIANT, VID_OK, VID_SOME};
        let variant_id = match variant {
            "Ok" => VID_OK,
            "Err" => VID_ERR,
            "Some" => VID_SOME,
            "None" => VID_NONE_VARIANT,
            // `alloc_enum` is the NATIVE construction path; it is only ever called with the four
            // reserved names above. The fallback is defensive only.
            _ => VID_NONE,
        };
        Value::Obj(self.heap.alloc(Obj::Enum {
            variant_id,
            payload,
        }))
    }

    /// `Op::JsonDecode`: pop the `Result[Json]` from `parse`, coerce its `Ok` payload against the
    /// descriptor (passing through an `Err`), push the resulting `Result[T]`.
    fn json_decode(
        &mut self,
        desc: &crate::json_decode::TypeDescriptor,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let res = self.pop();
        let bad = "decode: parse did not return a Result".to_string();
        let (rty, variant, payload) = self
            .enum_parts(res)
            .ok_or_else(|| self.err(bad.clone(), span))?;
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
                Obj::Enum {
                    variant_id,
                    payload,
                } => {
                    // M19 lever #2 — cold path: resolve the type + variant names from the id.
                    let (ty, variant) = self.enum_names(*variant_id);
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
        let mismatch = |want: &str| {
            format!(
                "decode: expected {want} at {path}, found {}",
                crate::json_decode::json_kind(&variant)
            )
        };
        match desc {
            D::Int => {
                let f = self
                    .json_num(&variant, &payload)
                    .ok_or_else(|| mismatch("int"))?;
                if f.fract() != 0.0 || !f.is_finite() {
                    return Err(format!("decode: expected an integer at {path}, found {f}"));
                }
                Ok(Value::Int(f as i64))
            }
            D::Float => {
                let f = self
                    .json_num(&variant, &payload)
                    .ok_or_else(|| mismatch("float"))?;
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
            D::Struct {
                key,
                display,
                fields,
            } => {
                if variant != "Obj" {
                    // ROOT REDESIGN — error text shows the BARE display name, never the identity key.
                    return Err(mismatch(&format!("object for {display}")));
                }
                let entries = match self.heap.get(self.as_obj(payload[0])) {
                    Obj::Map(m) => m.entries.clone(),
                    _ => return Err(mismatch("object")),
                };
                // Positional layout: `fields` (the type descriptor) is already in the struct's
                // declaration order (see `json_decode::struct_descriptor`), so push values in order.
                let mut field_vals: Vec<Value> = Vec::with_capacity(fields.len());
                for (fname, fdesc) in fields {
                    let found = entries
                        .iter()
                        .find(|(_, k, _)| self.val_str(*k).as_deref() == Some(fname.as_str()));
                    let fpath = format!("{path}.{fname}");
                    let v = match found {
                        Some((_, _, jval)) => self.coerce_json(*jval, fdesc, &fpath)?,
                        None => match fdesc {
                            // A missing Option field decodes to None; anything else is an error.
                            D::Option(_) => self.alloc_enum("Option", "None", Vec::new()),
                            _ => return Err(format!("decode: missing key '{fname}' at {path}")),
                        },
                    };
                    field_vals.push(v);
                }
                // ROOT REDESIGN — tag the value with the qualified IDENTITY KEY (so downstream
                // field/method lookups + `struct_tid` hit the right layout); display renders bare.
                let tid = self.struct_tid(key);
                let h = self.heap.alloc(Obj::Struct {
                    name: key.clone().into_boxed_str(),
                    tid,
                    fields: field_vals,
                });
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

    fn check_arity(
        &self,
        _kind: &str,
        name: &str,
        want: usize,
        got: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if want != got {
            return Err(self.err(
                format!("function '{name}' expects {want} argument(s), got {got}"),
                span,
            ));
        }
        Ok(())
    }

    /// `ic`: the per-call-site method inline-cache id from the `CallMethod` op, or [`NO_IC`] for the
    /// native-re-entry callers (`spawn`/`defer` method tasks) that need a *synchronous* result and so
    /// must take the re-entrant `run_proto` path (never the in-place frame flatten). A real `ic` ⟺ the
    /// caller is the running dispatch loop (the sole emit path), so a real `ic` is exactly the
    /// "flatten-safe" signal: the pushed frame is executed by the `run_until` that called us.
    fn do_method_call(
        &mut self,
        method: &str,
        argc: usize,
        ic: u32,
        span: Span,
    ) -> Result<(), RuntimeError> {
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
            if is_prim && let Some(ord) = self.compare(recv, args[0]) {
                self.push(Value::Int(ord as i64));
                return Ok(());
            }
        }
        let Value::Obj(h) = recv else {
            return Err(self.err(
                format!("type {} has no method '{method}'", self.type_name(recv)),
                span,
            ));
        };
        // M19 Phase 6 / N-way poly — method-call inline-cache fast path (struct methods only). Scan
        // the site's ways for a way whose cached `tid` matches the receiver layout: a hit collapses the
        // `program.structs` clone + name-keyed `def.methods` probe to a short int-compare scan AND
        // flattens the call: `[recv, args…]` go on the stack and the method frame is installed in place,
        // so the running `run_until` executes the body and its `Return` pushes the result — no re-entrant
        // `run_proto`. A megamorphic-but-bounded site (≤4 distinct receiver types) hits a way for each,
        // so it never thrashes the monomorphic refill. Only the dispatch loop reaches here (real `ic`);
        // the arity guard re-runs per hit (cheap) so a hit can never enter a frame with the wrong slot
        // count, and the tid re-compare on every probe bars a wrong body. A `sticky` site (overflowed
        // past 4 types) skips the probe and falls straight through to the slow path.
        if ic != NO_IC {
            let site = self.method_ic[ic as usize];
            if !site.sticky
                && let Obj::Struct { tid, .. } = self.heap.get(h)
            {
                let recv_tid = *tid;
                let mut hit: Option<MethodIcCell> = None;
                for way in &site.ways {
                    if way.tid != TID_NONE && way.tid == recv_tid {
                        hit = Some(*way);
                        break;
                    }
                }
                if let Some(cell) = hit {
                    let proto = cell.proto;
                    let arity = self.program.protos[proto].arity;
                    if arity != argc + 1 {
                        return Err(self.err(
                            format!(
                                "function '{}' expects {} argument(s), got {}",
                                self.program.protos[proto].name,
                                arity,
                                argc + 1
                            ),
                            span,
                        ));
                    }
                    let home = self.module_objs[cell.module_idx as usize];
                    // Experimental generators — a generator method allocates rather than running (else its
                    // `Op::Yield` would poison the host run with no `generator_next` to drive it).
                    if self.program.protos[proto].is_generator {
                        let mut gen_args = Vec::with_capacity(argc + 1);
                        gen_args.push(recv);
                        gen_args.extend(args);
                        let g = self.alloc_generator(proto, home, None, gen_args);
                        self.push(g);
                        return Ok(());
                    }
                    let base = self.stack.len();
                    self.stack.push(recv);
                    self.stack.extend(args);
                    return self.push_frame_in_place(proto, home, None, base, span);
                }
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
        // A cursor (`Obj::Iter`, the `Iterable` `.iter()` result) exposes `.next()` (advance the
        // snapshot, idempotent `None` past the end) and `.iter()` (returns self — every Iterator IS
        // Iterable, idempotently). Intrinsic, like the generator arm just below.
        if matches!(self.heap.get(h), Obj::Iter { .. }) {
            if !args.is_empty() {
                return Err(self.err(format!("a cursor's '{method}' takes no arguments"), span));
            }
            match method {
                "iter" => self.push(recv), // idempotent: iter() on a cursor returns self
                "next" => {
                    let Obj::Iter { items, pos } = self.heap.get_mut(h) else {
                        unreachable!()
                    };
                    let item = if *pos < items.len() {
                        let item = items[*pos];
                        *pos += 1;
                        Some(item)
                    } else {
                        None
                    };
                    let result = match item {
                        Some(v) => self.alloc_enum("Option", "Some", vec![v]),
                        None => self.alloc_enum("Option", "None", Vec::new()),
                    };
                    self.push(result);
                }
                _ => {
                    return Err(self.err(
                        format!("a cursor has no method '{method}' (only `next()`/`iter()`)"),
                        span,
                    ));
                }
            }
            return Ok(());
        }
        // Experimental generators — `.next()` is intrinsic (resumes the coroutine), so a generator
        // result drives `for x in g():` through the same lazy `next()` step as a struct iterator.
        // `.iter()` on a generator returns self (a generator IS an Iterator, hence Iterable).
        if matches!(self.heap.get(h), Obj::Generator(_)) {
            if method == "iter" && args.is_empty() {
                self.push(recv); // idempotent: a generator's iter() is itself
                return Ok(());
            }
            if method != "next" || !args.is_empty() {
                return Err(self.err(
                    format!("a generator has no method '{method}' (only `next()`)"),
                    span,
                ));
            }
            let result = self.generator_next(h, span)?;
            self.push(result);
            return Ok(());
        }
        if matches!(self.heap.get(h), Obj::Shared(_)) {
            let result = self.shared_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        if matches!(self.heap.get(h), Obj::RwShared(_)) {
            let result = self.rwshared_method(h, method, &args, span)?;
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
        // `.iter()` on a built-in collection (str/list/map/set/bytes/bytearray) → a FRESH cursor that
        // SNAPSHOTS the current contents in the SAME order/elements as `for x in X` (list/set elems,
        // map → keys, str → per-char str, bytes/bytearray → per-byte int). Reuses `drain_iterable`
        // (the for-loop's single source of truth), then wraps the snapshot in an `Obj::Iter`. Placed
        // BEFORE the per-type dispatch so it intercepts `iter` for every collection in one spot; a
        // collection has no user-defined `iter`, so there is no precedence concern.
        if method == "iter"
            && args.is_empty()
            && matches!(
                self.heap.get(h),
                Obj::Str(_)
                    | Obj::List(_)
                    | Obj::Map(_)
                    | Obj::Set(_)
                    | Obj::Bytes(_)
                    | Obj::ByteArray(_)
            )
        {
            // `drain_iterable` may alloc (str per-char); root the receiver across the call.
            self.push(recv);
            let items = self.drain_iterable(recv, span)?;
            self.pop(); // unroot receiver
            let cursor = self.heap.alloc(Obj::Iter { items, pos: 0 });
            self.push(Value::Obj(cursor));
            return Ok(());
        }
        // Core-type methods (M6): built-in methods on `str` / `list`. Handled before the clone-match
        // so `list.push` mutates the heap object in place (the match below clones the Obj). Mirrors
        // `interp::builtins::call_method` exactly — error strings included (parity-tested).
        if matches!(
            self.heap.get(h),
            Obj::Str(_) | Obj::List(_) | Obj::Map(_) | Obj::Set(_)
        ) {
            let result = self.core_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        // `bytes` methods (immutable byte sequence): only `decode() -> str` (UTF-8). Routed off the
        // handle like the other core-type methods.
        if matches!(self.heap.get(h), Obj::Bytes(_)) {
            let result = self.bytes_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        // `bytearray` methods (mutable buffer): `len`/`push`/`pop`/`extend`/`decode`. Routed separately
        // (not `core_method`) but with the same in-place-via-`get_mut` discipline as `list`.
        if matches!(self.heap.get(h), Obj::ByteArray(_)) {
            let result = self.bytearray_method(h, method, &args, span)?;
            self.push(result);
            return Ok(());
        }
        self.ensure_module_faulted(h); // D1: `module.fn(...)` on a not-yet-faulted worker module
        match self.heap.get(h).clone() {
            // `module.fn(args)` — plain call on the looked-up member, no `self`.
            Obj::Module { name, slots, index } => {
                let member = index
                    .get(method)
                    .map(|&i| slots[i as usize])
                    .ok_or_else(|| {
                        self.err(format!("module '{name}' has no member '{method}'"), span)
                    })?;
                self.stack.push(member);
                self.stack.extend(args);
                self.do_call(argc, span)
            }
            Obj::Struct {
                name, tid, fields, ..
            } => {
                // Fix A — resolve `(proto, module_idx)` WITHOUT cloning the whole StructDef (its
                // `fields` Vec + `methods` HashMap). On a megamorphic / sticky-generic site this slow
                // path runs per call, so the per-miss StructDef clone dwarfed the dispatch itself. We
                // bump the cheap `Arc<Program>` refcount (read-only, never alias-mutated) so the
                // immutable `structs` borrow is released before the later `&mut self` calls.
                let prog = Arc::clone(&self.program);
                let def = prog
                    .structs
                    .get(name.as_ref())
                    .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
                let resolved = def.methods.get(method).copied();
                let def_module_idx = def.module_idx;
                if let Some(proto) = resolved {
                    let home = self.module_objs[def_module_idx];
                    if self.program.protos[proto].arity != argc + 1 {
                        // `self` + explicit args.
                        return Err(self.err(
                            format!(
                                "function '{}' expects {} argument(s), got {}",
                                self.program.protos[proto].name,
                                self.program.protos[proto].arity,
                                argc + 1
                            ),
                            span,
                        ));
                    }
                    // Experimental generators — a generator method allocates rather than running (else
                    // its `Op::Yield` would poison the host run). Covers both the IC-flatten and the
                    // re-entrant `run_proto` paths below; never IC-cached (it returns, not push-frame).
                    if self.program.protos[proto].is_generator {
                        let mut gen_args = Vec::with_capacity(argc + 1);
                        gen_args.push(recv);
                        gen_args.extend(args);
                        let g = self.alloc_generator(proto, home, None, gen_args);
                        self.push(g);
                        return Ok(());
                    }
                    // M19 N-way poly — fill the next free way so the next call at this site hits the fast
                    // path above (only for the dispatch-loop path: a real `ic`, a registered layout
                    // `tid`). When all ways are occupied AND this `tid` is new, latch `sticky` so the
                    // site stops probing the (full) ways and goes straight here, mirroring the binop
                    // quickening's one-way `Q_GENERIC` deopt — a megamorphic site never thrashes.
                    if ic != NO_IC && tid != TID_NONE {
                        let site = &mut self.method_ic[ic as usize];
                        if !site.sticky {
                            let cell = MethodIcCell {
                                tid,
                                proto,
                                module_idx: def_module_idx as u32,
                            };
                            if let Some(free) = site.ways.iter_mut().find(|w| w.tid == TID_NONE) {
                                *free = cell;
                            } else {
                                // All ways occupied and `tid` is distinct from every one of them (else
                                // the fast path would have hit) — the site is megamorphic; go sticky.
                                site.sticky = true;
                            }
                        }
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
                // Invoked as a value (no `self` bound — it's not a method). Positional layout:
                // resolve the field name->index from the StructDef, then index the flat `fields`.
                let fidx = self
                    .program
                    .structs
                    .get(name.as_ref())
                    .and_then(|d| d.fields.iter().position(|f| f == method));
                if let Some(fval) = fidx.and_then(|i| fields.get(i).copied()) {
                    let v = self.invoke_value(fval, args, span)?;
                    if self.paused() {
                        return Ok(()); // B1/D3: the function-field call parked on `recv` or yielded.
                    }
                    self.push(v);
                    return Ok(());
                }
                // A user iterator struct (`next`, no explicit `iter`) IS Iterable — `.iter()` returns
                // self (idempotent), letting it flow into an `[S: Iterable[T]]` body. Mirrors interp.
                if method == "iter"
                    && args.is_empty()
                    && self
                        .program
                        .structs
                        .get(name.as_ref())
                        .is_some_and(|d| d.methods.contains_key("next"))
                {
                    self.push(recv);
                    return Ok(());
                }
                // ROOT REDESIGN — render the BARE display name (not the identity key) in the error.
                let display = self
                    .program
                    .structs
                    .get(name.as_ref())
                    .map(|d| d.display_name.clone())
                    .unwrap_or_else(|| crate::compiler::bare_display(name.as_ref()));
                Err(self.err(format!("struct '{display}' has no method '{method}'"), span))
            }
            // Enum method dispatch (name-resolved, like structs). Enums are type-erased — no `tid`,
            // so the method IC is skipped (follow-up lever); we resolve `enum_methods[key][method]`
            // off the variant's `enum_name` and dispatch with the same `self`-binding path structs use.
            Obj::Enum { variant_id, .. } => {
                let prog = Arc::clone(&self.program);
                let enum_key = self.enum_names(variant_id).0.to_string();
                let resolved = prog
                    .enum_methods
                    .get(&enum_key)
                    .and_then(|ms| ms.get(method).copied());
                if let Some(proto) = resolved {
                    // An enum method's home module is the enum's declaring module (recorded in
                    // `enum_home`), so its body resolves top-level names against the right globals.
                    let home = self.module_objs[self.enum_home_module(&enum_key)];
                    if self.program.protos[proto].arity != argc + 1 {
                        return Err(self.err(
                            format!(
                                "function '{}' expects {} argument(s), got {}",
                                self.program.protos[proto].name,
                                self.program.protos[proto].arity,
                                argc + 1
                            ),
                            span,
                        ));
                    }
                    if self.program.protos[proto].is_generator {
                        let mut gen_args = Vec::with_capacity(argc + 1);
                        gen_args.push(recv);
                        gen_args.extend(args);
                        let g = self.alloc_generator(proto, home, None, gen_args);
                        self.push(g);
                        return Ok(());
                    }
                    // Flatten only on the real dispatch-loop path (`ic != NO_IC`); re-entrant callers
                    // pass `NO_IC` and use the synchronous `run_proto` path (mirrors the struct arm).
                    if ic != NO_IC {
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
                        return Ok(());
                    }
                    self.push(v);
                    return Ok(());
                }
                let display = crate::compiler::bare_display(&enum_key);
                Err(self.err(format!("type {display} has no method '{method}'"), span))
            }
            // Newtype method dispatch (name-resolved, like enums). Resolves `newtype_methods[key]
            // [method]` off the wrapper's `type_key`. The underlying's methods are NOT inherited.
            Obj::NewType { type_key, .. } => {
                let prog = Arc::clone(&self.program);
                let nt_key = type_key.to_string();
                let resolved = prog
                    .newtype_methods
                    .get(&nt_key)
                    .and_then(|ms| ms.get(method).copied());
                if let Some(proto) = resolved {
                    let home = self.module_objs[self.newtype_home_module(&nt_key)];
                    if self.program.protos[proto].arity != argc + 1 {
                        return Err(self.err(
                            format!(
                                "function '{}' expects {} argument(s), got {}",
                                self.program.protos[proto].name,
                                self.program.protos[proto].arity,
                                argc + 1
                            ),
                            span,
                        ));
                    }
                    if self.program.protos[proto].is_generator {
                        let mut gen_args = Vec::with_capacity(argc + 1);
                        gen_args.push(recv);
                        gen_args.extend(args);
                        let g = self.alloc_generator(proto, home, None, gen_args);
                        self.push(g);
                        return Ok(());
                    }
                    if ic != NO_IC {
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
                        return Ok(());
                    }
                    self.push(v);
                    return Ok(());
                }
                let display = crate::compiler::bare_display(&nt_key);
                Err(self.err(format!("type {display} has no method '{method}'"), span))
            }
            _ => Err(self.err(
                format!("type {} has no method '{method}'", self.type_name(recv)),
                span,
            )),
        }
    }

    /// `Type.method(args)` — STATIC (associated) method dispatch (the "no self ⇒ static" rule).
    /// Stack: `[arg0, …]` — exactly `argc` values, NO receiver. Resolves `method` in the named
    /// struct's (`program.structs[key].methods`) or enum's (`program.enum_methods[key]`) method
    /// table by `type_key`; the body's home module is the type's declaring module. Pushes a frame
    /// holding just the args (arity == argc, no `self` slot) and runs it via `push_frame_in_place`
    /// (the dispatch-loop path) — structurally identical to enum-method dispatch minus the receiver.
    /// A static generator allocates rather than running, mirroring the instance arms.
    fn do_static_call(
        &mut self,
        type_key: &str,
        method: &str,
        argc: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let prog = Arc::clone(&self.program);
        // Resolve the proto + home module from the struct table first, then the enum table. The
        // compiler only emits `CallStatic` for a static method that exists on a known struct/enum,
        // so a miss here is an internal invariant break — surface it as a clear runtime error.
        let (proto, home_idx) = if let Some(def) = prog.structs.get(type_key) {
            match def.methods.get(method).copied() {
                Some(p) => (p, def.module_idx),
                None => {
                    return Err(self.err(
                        format!(
                            "type '{}' has no static method '{method}'",
                            def.display_name
                        ),
                        span,
                    ));
                }
            }
        } else if let Some(ms) = prog.enum_methods.get(type_key) {
            match ms.get(method).copied() {
                Some(p) => (p, self.enum_home_module(type_key)),
                None => {
                    let display = crate::compiler::bare_display(type_key);
                    return Err(self.err(
                        format!("type {display} has no static method '{method}'"),
                        span,
                    ));
                }
            }
        } else {
            let display = crate::compiler::bare_display(type_key);
            return Err(self.err(
                format!("type {display} has no static method '{method}'"),
                span,
            ));
        };
        // A static method has NO receiver, so its arity equals `argc` exactly (no `+ 1`).
        if self.program.protos[proto].arity != argc {
            return Err(self.err(
                format!(
                    "function '{}' expects {} argument(s), got {}",
                    self.program.protos[proto].name, self.program.protos[proto].arity, argc
                ),
                span,
            ));
        }
        let home = self.module_objs[home_idx];
        if self.program.protos[proto].is_generator {
            let at = self.stack.len() - argc;
            let gen_args: Vec<Value> = self.stack.split_off(at);
            let g = self.alloc_generator(proto, home, None, gen_args);
            self.push(g);
            return Ok(());
        }
        // The `argc` args are already contiguous on the operand stack (pushed by the compiler in
        // order, no receiver). Install the frame in place over them — the running `run_until`
        // executes the body and its `Return` pushes the result.
        let base = self.stack.len() - argc;
        self.push_frame_in_place(proto, home, None, base, span)
    }

    /// Higher-order list methods `map` / `filter` / `fold`. `src_h` is the receiver list.
    ///
    /// SNAPSHOT semantics: iteration walks a copy of the receiver's elements taken at call time, so
    /// a callback that MUTATES the receiver (e.g. `xs.pop()`/`xs.push(..)`) does NOT perturb the
    /// iteration sequence — it always visits exactly the elements present when the HOF was invoked.
    /// This (a) matches the interpreter (the parity oracle clones `elems` before dispatch — see
    /// `src/interp/mod.rs` `eval_method_call`, the `map`/`filter`/`fold`/`sort_by` arm), (b) matches
    /// comprehensions/for-loops (`Op::ListClone`) and `list_sort_by`/`sort_by_key` (which snapshot),
    /// (c) matches Python `map`/`filter`, and (d) is OOB-safe: indexing the original live list while
    /// a callback shrinks it would panic (regression: `map_shrinking_callback_no_panic`).
    ///
    /// GC discipline: each element is fed to a closure via `invoke_value`, which runs nested VM
    /// frames that can trigger GC at instruction boundaries. To keep the GC from collecting in-flight
    /// heap values, the source list, the snapshot list, the partially-built result list (map/filter),
    /// and the fold accumulator are all kept rooted on the operand stack across the iteration. Returns
    /// the result (caller pushes it). Arity & error messages match the interp exactly (parity-tested).
    fn list_hof(
        &mut self,
        src_h: GcRef,
        method: &str,
        mut args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // ROOT the source list on the operand stack: a method receiver is popped before dispatch, so
        // an inline temporary (`make().map(..)`) is otherwise unrooted and the callback's GC could
        // collect it before we snapshot.
        self.push(Value::Obj(src_h));
        // Take a SNAPSHOT now (matching the interpreter): iterate the receiver's elements as of call
        // time so a callback that shrinks/grows the receiver mid-iteration neither perturbs the
        // sequence nor indexes past the live (now-shorter) Vec. The snapshot is heap-allocated and
        // rooted on the operand stack so its elements survive the callback's collections.
        let snap_h = {
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!("list_hof on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::Obj(snap_h)); // ROOT the snapshot
        let n = match self.heap.get(snap_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!(),
        };
        match method {
            "map" | "filter" => {
                if args.len() != 1 {
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(self.err(
                        format!("'{method}' expects 1 argument(s), got {}", args.len()),
                        span,
                    ));
                }
                let f = args.swap_remove(0);
                let is_filter = method == "filter";
                // ROOT the result list too.
                let res_h = self.heap.alloc(Obj::List(Vec::new()));
                self.push(Value::Obj(res_h));
                for i in 0..n {
                    // Read from the rooted SNAPSHOT, not the live receiver: a callback that shrinks
                    // the receiver must not affect this index (stays valid, no OOB).
                    let elem = match self.heap.get(snap_h) {
                        Obj::List(v) => v[i],
                        _ => unreachable!(),
                    };
                    // May GC; source, snapshot, and result lists are rooted, so elements survive.
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
                                self.pop(); // unroot snapshot
                                self.pop(); // unroot source
                                return Err(self.err(
                                    format!(
                                        "filter predicate must return bool, got {}",
                                        self.type_name(other)
                                    ),
                                    span,
                                ));
                            }
                        }
                    } else if let Obj::List(items) = self.heap.get_mut(res_h) {
                        items.push(out);
                    }
                }
                self.pop(); // unroot result
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                Ok(Value::Obj(res_h))
            }
            "fold" => {
                if args.len() != 2 {
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(self.err(
                        format!("'fold' expects 2 argument(s), got {}", args.len()),
                        span,
                    ));
                }
                let f = args.swap_remove(1);
                let init = args.swap_remove(0);
                // ROOT the accumulator: push init, remember its slot, and replace in place each step.
                // `acc_slot` sits below every nested frame's base (frames push above the current
                // stack top and pop back to it), so the index stays valid across `invoke_value`.
                self.push(init);
                let acc_slot = self.stack.len() - 1;
                for i in 0..n {
                    // Read from the rooted SNAPSHOT (see map/filter): OOB-safe under a shrinking
                    // callback.
                    let elem = match self.heap.get(snap_h) {
                        Obj::List(v) => v[i],
                        _ => unreachable!(),
                    };
                    let acc = self.stack[acc_slot];
                    let new = self.guarded(|vm| vm.invoke_value(f, vec![acc, elem], span))?;
                    self.stack[acc_slot] = new;
                }
                let acc = self.pop(); // unroot accumulator
                self.pop(); // unroot snapshot
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
    fn list_sort_by(
        &mut self,
        src_h: GcRef,
        mut args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if args.len() != 1 {
            return Err(self.err(
                format!("'sort_by' expects 1 argument(s), got {}", args.len()),
                span,
            ));
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
    fn msort_indices(
        &mut self,
        src_h: GcRef,
        idx: Vec<usize>,
        cmp: Value,
        span: Span,
    ) -> Result<Vec<usize>, RuntimeError> {
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
    fn compare_with(
        &mut self,
        cmp: Value,
        a: Value,
        b: Value,
        span: Span,
    ) -> Result<i64, RuntimeError> {
        match self.guarded(|vm| vm.invoke_value(cmp, vec![a, b], span))? {
            Value::Int(n) => Ok(n),
            other => Err(self.err(
                format!(
                    "sort_by comparator must return int, got {}",
                    self.type_name(other)
                ),
                span,
            )),
        }
    }

    /// `xs.sort_by_key(f)` — stable in-place sort by a derived key `f: fn(T) -> K`. Mirrors
    /// `list_sort_by`'s GC discipline: the source list, an element snapshot, AND a parallel **keys**
    /// list are all rooted on the operand stack so the re-entrant extractor (and a Comparable-struct
    /// key's `compare`) can GC freely. Keys are computed once per element; the merge sort permutes
    /// `usize` indices, re-reading keys from the rooted keys list per comparison. Returns `nil`.
    fn list_sort_by_key(
        &mut self,
        src_h: GcRef,
        mut args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if args.len() != 1 {
            return Err(self.err(
                format!("'sort_by_key' expects 1 argument(s), got {}", args.len()),
                span,
            ));
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
    fn msort_indices_by_key(
        &mut self,
        keys_h: GcRef,
        idx: Vec<usize>,
        span: Span,
    ) -> Result<Vec<usize>, RuntimeError> {
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
    fn order_key(
        &mut self,
        a: Value,
        b: Value,
        span: Span,
    ) -> Result<std::cmp::Ordering, RuntimeError> {
        if let (Value::Obj(ha), Value::Obj(hb)) = (a, b)
            && matches!(self.heap.get(ha), Obj::Struct { .. })
            && matches!(self.heap.get(hb), Obj::Struct { .. })
        {
            return self.struct_compare(a, b, span);
        }
        // Float keys order by `total_cmp` for the WHOLE comparison (not just the NaN case), exactly
        // mirroring `sort()`'s `value_order` Float arm — so `sort_by_key` and `sort()` agree on every
        // float pair, including `-0.0`/`+0.0` (which `partial_cmp` ranks Equal but `total_cmp` orders
        // `-0.0 < +0.0`) and NaN (deterministic, to one end). Int keys deliberately stay on the int
        // path below (`Int.cmp`): routing them through `as_f64` would lose precision past 2^53.
        if let (Value::Float(x), Value::Float(y)) = (a, b) {
            return Ok(x.total_cmp(&y));
        }
        match self.compare(a, b) {
            Some(ord) => Ok(ord),
            // Numeric `None` means a NaN float — handled above for the Float/Float case; this arm
            // only catches a mixed int/float key pair (not reachable for a single key type K), kept
            // deterministic via `total_cmp` for safety.
            None if is_numeric(a) && is_numeric(b) => Ok(as_f64(a).total_cmp(&as_f64(b))),
            // Genuinely-incomparable types: unreachable from well-typed source; kept for safety.
            None => Err(self.err(
                format!(
                    "sort_by_key keys are not comparable: {} vs {}",
                    self.type_name(a),
                    self.type_name(b)
                ),
                span,
            )),
        }
    }

    /// Built-in methods on `str` / `list` (M6). The result is returned (not pushed) so the caller
    /// owns stack discipline. Multi-allocation paths (`split`) are safe: the GC only collects at
    /// instruction boundaries, never mid-opcode, so all `alloc`s here complete uninterrupted.
    /// Clone the elements of a `list`-typed argument for `concat`/`extend`. The checker guarantees
    /// the type; a non-list here is an internal invariant break, reported for safety.
    fn expect_list_obj(
        &self,
        method: &str,
        arg: Value,
        span: Span,
    ) -> Result<Vec<Value>, RuntimeError> {
        match arg {
            Value::Obj(ah) => match self.heap.get(ah) {
                Obj::List(items) => Ok(items.clone()),
                _ => Err(self.err(
                    format!(
                        "{method}() expects a list argument, got {}",
                        self.type_name(arg)
                    ),
                    span,
                )),
            },
            other => Err(self.err(
                format!(
                    "{method}() expects a list argument, got {}",
                    self.type_name(other)
                ),
                span,
            )),
        }
    }

    /// Insert-or-overwrite `(hk, key, val)` into the heap map at `h` (last write wins). Used by
    /// `map.update`. No allocation, so no GC concerns.
    fn map_upsert_in_heap(&mut self, h: GcRef, hk: u64, key: Value, val: Value) {
        let Obj::Map(m) = self.heap.get(h) else {
            unreachable!()
        };
        let pos = m
            .candidates(hk)
            .iter()
            .copied()
            .find(|&p| self.values_equal(m.entries[p].1, key));
        let Obj::Map(m) = self.heap.get_mut(h) else {
            unreachable!()
        };
        match pos {
            Some(i) => m.entries[i].2 = val,
            None => m.push(hk, key, val),
        }
    }

    /// An `int` method-argument, with a uniform type error matching the interp.
    fn int_arg(&self, method: &str, v: &Value, span: Span) -> Result<i64, RuntimeError> {
        match v {
            Value::Int(n) => Ok(*n),
            other => Err(self.err(
                format!(
                    "{method}() expects an int argument, got {}",
                    self.type_name(*other)
                ),
                span,
            )),
        }
    }

    fn core_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // A str argument's owned text, with a uniform type error matching the interp.
        let str_arg = |vm: &Vm, i: usize| -> Result<String, RuntimeError> {
            match args[i] {
                Value::Obj(ah) => match vm.heap.get(ah) {
                    Obj::Str(a) => Ok(a.to_string()),
                    _ => Err(vm.err(
                        format!(
                            "{method}() expects a str argument, got {}",
                            vm.type_name(args[i])
                        ),
                        span,
                    )),
                },
                other => Err(vm.err(
                    format!(
                        "{method}() expects a str argument, got {}",
                        vm.type_name(other)
                    ),
                    span,
                )),
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
                        let parts: Vec<Value> = s
                            .split(sep.as_str())
                            .map(|p| self.alloc_str(p.to_string()))
                            .collect();
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
                    // `encode() -> bytes`: UTF-8 encode (str is UTF-8 internally; copy the bytes out
                    // into a new immutable `bytes`). Always succeeds — no fault path. UTF-8 only.
                    "encode" => {
                        self.arity_err("encode", args, 0, span)?;
                        let bytes = s.as_bytes().to_vec().into_boxed_slice();
                        Ok(Value::Obj(self.heap.alloc(Obj::Bytes(bytes))))
                    }
                    "join" => {
                        self.arity_err("join", args, 1, span)?;
                        let Value::Obj(lh) = args[0] else {
                            return Err(self.err(
                                format!(
                                    "join() expects a list of str, got {}",
                                    self.type_name(args[0])
                                ),
                                span,
                            ));
                        };
                        let Obj::List(items) = self.heap.get(lh) else {
                            return Err(self.err(
                                format!(
                                    "join() expects a list of str, got {}",
                                    self.type_name(args[0])
                                ),
                                span,
                            ));
                        };
                        let mut out = String::new();
                        for (i, item) in items.clone().iter().enumerate() {
                            let Value::Obj(ih) = item else {
                                return Err(self.err(
                                    format!(
                                        "join() expects a list of str, got an element of type {}",
                                        self.type_name(*item)
                                    ),
                                    span,
                                ));
                            };
                            let Obj::Str(part) = self.heap.get(*ih) else {
                                return Err(self.err(
                                    format!(
                                        "join() expects a list of str, got an element of type {}",
                                        self.type_name(*item)
                                    ),
                                    span,
                                ));
                            };
                            if i > 0 {
                                out.push_str(&s);
                            }
                            out.push_str(part);
                        }
                        Ok(self.alloc_str(out))
                    }
                    // gap #1 (minimal subset): receiver methods forwarding to the `std.str` free
                    // fns. Pure native Rust, byte-identical to the std.str codepoint-loop oracle
                    // (see std/str.chz) and to the interp arms.
                    "ends_with" => {
                        self.arity_err("ends_with", args, 1, span)?;
                        Ok(Value::Bool(s.ends_with(str_arg(self, 0)?.as_str())))
                    }
                    "replace" => {
                        self.arity_err("replace", args, 2, span)?;
                        let old = str_arg(self, 0)?;
                        let new = str_arg(self, 1)?;
                        // std.str returns `s` unchanged for an empty `old`.
                        if old.is_empty() {
                            Ok(self.alloc_str(s))
                        } else {
                            Ok(self.alloc_str(s.replace(old.as_str(), new.as_str())))
                        }
                    }
                    "repeat" => {
                        self.arity_err("repeat", args, 1, span)?;
                        let n = self.int_arg("repeat", &args[0], span)?;
                        // std.str: n <= 0 yields "".
                        if n <= 0 {
                            Ok(self.alloc_str(String::new()))
                        } else {
                            // Guard the allocation: `str::repeat` hard-panics on capacity overflow.
                            // Raise a recoverable fault instead (repo convention for overflow).
                            match s
                                .len()
                                .checked_mul(n as usize)
                                .filter(|&t| t <= isize::MAX as usize)
                            {
                                Some(total) => {
                                    // The byte-size guard passes huge-but-representable totals that
                                    // `str::repeat` would still abort on. Probe allocation
                                    // feasibility with `try_reserve_exact` (uninitialized capacity,
                                    // freed immediately) so an infeasible request is a recoverable
                                    // fault, then fall through to the optimized `str::repeat` — which
                                    // also short-circuits to "" for an empty receiver (`total == 0`)
                                    // instead of looping `n` times.
                                    let mut probe = String::new();
                                    if probe.try_reserve_exact(total).is_err() {
                                        return Err(self.err(
                                            "string repeat capacity overflow".to_string(),
                                            span,
                                        ));
                                    }
                                    drop(probe);
                                    Ok(self.alloc_str(s.repeat(n as usize)))
                                }
                                None => {
                                    Err(self
                                        .err("string repeat capacity overflow".to_string(), span))
                                }
                            }
                        }
                    }
                    "reverse" => {
                        self.arity_err("reverse", args, 0, span)?;
                        Ok(self.alloc_str(s.chars().rev().collect::<String>()))
                    }
                    "pad_left" => {
                        self.arity_err("pad_left", args, 2, span)?;
                        let width = self.int_arg("pad_left", &args[0], span)?;
                        let fill = str_arg(self, 1)?;
                        // std.str: prepend `fill` until the codepoint length reaches `width`.
                        let mut out = s.clone();
                        while (out.chars().count() as i64) < width {
                            out = format!("{fill}{out}");
                        }
                        Ok(self.alloc_str(out))
                    }
                    "index_of" => {
                        self.arity_err("index_of", args, 1, span)?;
                        let sub = str_arg(self, 0)?;
                        // std.str: empty -> 0; otherwise the CODEPOINT index (not byte offset).
                        if sub.is_empty() {
                            Ok(Value::Int(0))
                        } else {
                            match s.find(sub.as_str()) {
                                Some(byte) => Ok(Value::Int(s[..byte].chars().count() as i64)),
                                None => Ok(Value::Int(-1)),
                            }
                        }
                    }
                    "count" => {
                        self.arity_err("count", args, 1, span)?;
                        let sub = str_arg(self, 0)?;
                        // std.str: empty -> 0; otherwise non-overlapping count.
                        if sub.is_empty() {
                            Ok(Value::Int(0))
                        } else {
                            Ok(Value::Int(s.matches(sub.as_str()).count() as i64))
                        }
                    }
                    "strip_prefix" => {
                        self.arity_err("strip_prefix", args, 1, span)?;
                        let p = str_arg(self, 0)?;
                        let out = s.strip_prefix(p.as_str()).unwrap_or(&s).to_string();
                        Ok(self.alloc_str(out))
                    }
                    "strip_suffix" => {
                        self.arity_err("strip_suffix", args, 1, span)?;
                        let p = str_arg(self, 0)?;
                        let out = s.strip_suffix(p.as_str()).unwrap_or(&s).to_string();
                        Ok(self.alloc_str(out))
                    }
                    "split_lines" => {
                        self.arity_err("split_lines", args, 0, span)?;
                        let parts: Vec<Value> = s
                            .split('\n')
                            .map(|p| self.alloc_str(p.to_string()))
                            .collect();
                        Ok(Value::Obj(self.heap.alloc(Obj::List(parts))))
                    }
                    // `strip` is a trim alias.
                    "strip" => {
                        self.arity_err("strip", args, 0, span)?;
                        Ok(self.alloc_str(s.trim().to_string()))
                    }
                    // gap #7: safe numeric parse — None on bad input (trims like int()/float()).
                    "to_int" => {
                        self.arity_err("to_int", args, 0, span)?;
                        match s.trim().parse::<i64>() {
                            Ok(n) => Ok(self.alloc_enum("Option", "Some", vec![Value::Int(n)])),
                            Err(_) => Ok(self.alloc_enum("Option", "None", vec![])),
                        }
                    }
                    "to_float" => {
                        self.arity_err("to_float", args, 0, span)?;
                        match s.trim().parse::<f64>() {
                            Ok(f) => Ok(self.alloc_enum("Option", "Some", vec![Value::Float(f)])),
                            Err(_) => Ok(self.alloc_enum("Option", "None", vec![])),
                        }
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
                    let Obj::List(items) = self.heap.get_mut(h) else {
                        unreachable!()
                    };
                    items.push(v);
                    Ok(Value::Nil)
                }
                "pop" => {
                    self.arity_err("pop", args, 0, span)?;
                    let Obj::List(items) = self.heap.get_mut(h) else {
                        unreachable!()
                    };
                    let popped = items.pop();
                    // M19 lever #2 — route through `alloc_enum` so the dense `variant_id` is stamped
                    // (replacing the two ad-hoc per-instance `Box<str>` builds).
                    Ok(match popped {
                        Some(v) => self.alloc_enum("Option", "Some", vec![v]),
                        None => self.alloc_enum("Option", "None", vec![]),
                    })
                }
                "reverse" => {
                    self.arity_err("reverse", args, 0, span)?;
                    let Obj::List(items) = self.heap.get_mut(h) else {
                        unreachable!()
                    };
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
                    let is_struct = matches!(items.first(), Some(Value::Obj(hh)) if matches!(self.heap.get(*hh), Obj::Struct { .. }));
                    if is_struct {
                        // Struct compare re-enters the VM (may GC) → rooted, index-based sort.
                        return self.list_sort_structs(h, span);
                    }
                    let mut elems = items.clone();
                    elems.sort_by(|a, b| self.value_order(*a, *b));
                    let Obj::List(items) = self.heap.get_mut(h) else {
                        unreachable!()
                    };
                    *items = elems;
                    Ok(Value::Nil)
                }
                "contains" => {
                    self.arity_err("contains", args, 1, span)?;
                    let target = args[0];
                    let elems = items.clone();
                    Ok(Value::Bool(
                        elems.iter().any(|v| self.values_equal(*v, target)),
                    ))
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
                    let Obj::List(items) = self.heap.get_mut(h) else {
                        unreachable!()
                    };
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
                                Value::Int(n) => {
                                    acc = acc.checked_add(*n).ok_or_else(|| {
                                        self.err("integer overflow in Add".to_string(), span)
                                    })?;
                                }
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
                    let Obj::Map(m) = self.heap.get(h) else {
                        unreachable!()
                    };
                    let found = m
                        .candidates(hk)
                        .iter()
                        .any(|&p| self.values_equal(m.entries[p].1, key));
                    Ok(Value::Bool(found))
                }
                "get" => {
                    self.arity_err("get", args, 1, span)?;
                    let key = args[0];
                    let hk = self.hash_key_rooted(key, &[Value::Obj(h), key], span)?;
                    let Obj::Map(m) = self.heap.get(h) else {
                        unreachable!()
                    };
                    let found = m
                        .candidates(hk)
                        .iter()
                        .copied()
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
                    let Obj::Map(m) = self.heap.get(h) else {
                        unreachable!()
                    };
                    let pos = m
                        .candidates(hk)
                        .iter()
                        .copied()
                        .find(|&p| self.values_equal(m.entries[p].1, key));
                    match pos {
                        Some(i) => {
                            let Obj::Map(m) = self.heap.get_mut(h) else {
                                unreachable!()
                            };
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
                            _ => {
                                return Err(self.err(
                                    format!(
                                        "{method}() expects a map argument, got {}",
                                        self.type_name(args[0])
                                    ),
                                    span,
                                ));
                            }
                        },
                        other => {
                            return Err(self.err(
                                format!(
                                    "{method}() expects a map argument, got {}",
                                    self.type_name(other)
                                ),
                                span,
                            ));
                        }
                    };
                    if method == "merge" {
                        let Obj::Map(m) = self.heap.get(h) else {
                            unreachable!()
                        };
                        let mut out = m.clone();
                        for (hk, key, val) in incoming {
                            let pos = out
                                .candidates(hk)
                                .iter()
                                .copied()
                                .find(|&p| self.values_equal(out.entries[p].1, key));
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
                    let Obj::Set(s) = self.heap.get(h) else {
                        unreachable!()
                    };
                    Ok(Value::Bool(
                        s.candidates(hx)
                            .iter()
                            .any(|&p| self.values_equal(s.entries[p].1, x)),
                    ))
                }
                "add" => {
                    self.arity_err("add", args, 1, span)?;
                    let x = args[0];
                    let hx = self.hash_key_rooted(x, &[Value::Obj(h), x], span)?;
                    let Obj::Set(s) = self.heap.get(h) else {
                        unreachable!()
                    };
                    let present = s
                        .candidates(hx)
                        .iter()
                        .any(|&p| self.values_equal(s.entries[p].1, x));
                    if !present {
                        let Obj::Set(s) = self.heap.get_mut(h) else {
                            unreachable!()
                        };
                        s.push(hx, x);
                    }
                    Ok(Value::Nil)
                }
                "remove" => {
                    self.arity_err("remove", args, 1, span)?;
                    let x = args[0];
                    let hx = self.hash_key_rooted(x, &[Value::Obj(h), x], span)?;
                    let Obj::Set(s) = self.heap.get(h) else {
                        unreachable!()
                    };
                    let pos = s
                        .candidates(hx)
                        .iter()
                        .copied()
                        .find(|&p| self.values_equal(s.entries[p].1, x));
                    match pos {
                        Some(i) => {
                            let Obj::Set(s) = self.heap.get_mut(h) else {
                                unreachable!()
                            };
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
                        if !set
                            .candidates(he)
                            .iter()
                            .any(|&p| vm.values_equal(set.entries[p].1, e))
                        {
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
                                let in_other = other
                                    .candidates(*he)
                                    .iter()
                                    .any(|&p| self.values_equal(other.entries[p].1, *e));
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

    /// Built-in methods on a `bytearray` receiver (`h` is the heap handle): `len`, `push(int 0..=255)`,
    /// `pop() -> Option[int]`, `extend(bytes|bytearray|List[int])`. Mirrors the interp's
    /// `eval_bytearray_method` and the checker's `bytearray_method_sig` — keep all three in lockstep.
    /// Mutators write IN PLACE through the heap slot (`get_mut`), exactly like the `list` methods, so a
    /// second binding to the same `bytearray` observes the change.
    fn bytearray_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "len" => {
                self.arity_err("len", args, 0, span)?;
                let Obj::ByteArray(b) = self.heap.get(h) else {
                    unreachable!()
                };
                Ok(Value::Int(b.len() as i64))
            }
            "push" => {
                self.arity_err("push", args, 1, span)?;
                let byte = match args[0] {
                    Value::Int(n) if (0..=255).contains(&n) => n as u8,
                    Value::Int(n) => {
                        return Err(self.err(
                            format!("byte value {n} out of range (must be 0..=255)"),
                            span,
                        ));
                    }
                    other => {
                        return Err(self.err(
                            format!("push() expects an int, got {}", self.type_name(other)),
                            span,
                        ));
                    }
                };
                let Obj::ByteArray(b) = self.heap.get_mut(h) else {
                    unreachable!()
                };
                b.push(byte);
                Ok(Value::Nil)
            }
            "pop" => {
                self.arity_err("pop", args, 0, span)?;
                let Obj::ByteArray(b) = self.heap.get_mut(h) else {
                    unreachable!()
                };
                let popped = b.pop();
                Ok(match popped {
                    Some(x) => self.alloc_enum("Option", "Some", vec![Value::Int(x as i64)]),
                    None => self.alloc_enum("Option", "None", vec![]),
                })
            }
            "extend" => {
                self.arity_err("extend", args, 1, span)?;
                // Snapshot the other side first (so `ba.extend(ba)` terminates) — also validates
                // ints 0..=255 / element types up front, mirroring the constructor.
                let appended = self.collect_bytes_arg("extend", args[0], span)?;
                let Obj::ByteArray(b) = self.heap.get_mut(h) else {
                    unreachable!()
                };
                b.extend_from_slice(&appended);
                Ok(Value::Nil)
            }
            // `decode() -> str`: UTF-8 decode the current buffer. Invalid UTF-8 is a RECOVERABLE
            // fault (catchable by `recover:`), never a panic — mirrors the bytes path + the interp.
            "decode" => {
                self.arity_err("decode", args, 0, span)?;
                let Obj::ByteArray(b) = self.heap.get(h) else {
                    unreachable!()
                };
                let bytes = b.clone();
                self.decode_utf8(&bytes, span)
            }
            _ => Err(self.err(format!("type bytearray has no method '{method}'"), span)),
        }
    }

    /// `bytes` methods (immutable byte sequence): only `decode() -> str` (UTF-8). Mirrors the interp's
    /// bytes-method arm and the checker's `bytes_method_sig` — keep all three in lockstep.
    fn bytes_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "decode" => {
                self.arity_err("decode", args, 0, span)?;
                let Obj::Bytes(b) = self.heap.get(h) else {
                    unreachable!()
                };
                let bytes = b.clone();
                self.decode_utf8(&bytes, span)
            }
            "len" => {
                self.arity_err("len", args, 0, span)?;
                let Obj::Bytes(b) = self.heap.get(h) else {
                    unreachable!()
                };
                Ok(Value::Int(b.len() as i64))
            }
            _ => Err(self.err(format!("type bytes has no method '{method}'"), span)),
        }
    }

    /// UTF-8 decode a byte slice into a new heap `str`. Invalid UTF-8 maps to a RECOVERABLE
    /// RuntimeError (catchable by `recover:`), not a panic — the error message is byte-identical to
    /// the interp's so the two engines stay parity-equal.
    fn decode_utf8(&mut self, bytes: &[u8], span: Span) -> Result<Value, RuntimeError> {
        match std::str::from_utf8(bytes) {
            Ok(s) => Ok(self.alloc_str(s.to_string())),
            Err(_) => Err(self.err("invalid UTF-8 in decode()".to_string(), span)),
        }
    }

    /// Read a set argument (for set algebra), erroring if it isn't a set. Returns a clone of its
    /// [`SetData`] (entries + index) so membership tests reuse the cached hashes.
    fn set_arg(&self, v: Value, method: &str, span: Span) -> Result<SetData, RuntimeError> {
        match v {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Set(s) => Ok(s.clone()),
                _ => Err(self.err(
                    format!(
                        "{method}() expects a set argument, got {}",
                        self.type_name(v)
                    ),
                    span,
                )),
            },
            _ => Err(self.err(
                format!(
                    "{method}() expects a set argument, got {}",
                    self.type_name(v)
                ),
                span,
            )),
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
        Value::Obj(
            self.heap
                .alloc(Obj::Str((&*c.encode_utf8(&mut buf)).into())),
        )
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
        while self
            .handlers
            .last()
            .is_some_and(|h| h.frame_len > self.frames.len())
        {
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
            Some(name) => Deferred::Method {
                recv: head,
                name,
                args,
                span,
            },
            None => Deferred::Call {
                callee: head,
                args,
                span,
            },
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
            Deferred::Method {
                recv,
                name,
                args,
                span,
            } => {
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
    fn do_spawn(
        &mut self,
        method: Option<String>,
        argc: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let raw_args: Vec<Value> = self.stack.split_off(at);
        let head = self.pop();
        let mut args: Vec<Value> = Vec::with_capacity(raw_args.len());
        for a in raw_args {
            args.push(self.deep_clone(a, span)?);
        }
        let task = match method {
            Some(name) => {
                let recv = self.deep_clone(head, span)?;
                PendingCall::Method {
                    recv,
                    name,
                    args,
                    span,
                }
            }
            None => PendingCall::Call {
                callee: head,
                args,
                span,
            },
        };
        self.register_task(task, span)
    }

    /// `spawn:` block — snapshot the captured bindings from the current frame (like `MakeClosure`),
    /// deep-copy each captured value across the airlock, build a zero-arg closure over the synthetic
    /// block proto, and register it as a `Call` task. Mirrors the interpreter's `Task::Block`
    /// (captured locals deep-copied; home globals by handle).
    fn do_spawn_block(
        &mut self,
        proto: ProtoId,
        entries: &[CapEntry],
        span: Span,
    ) -> Result<(), RuntimeError> {
        let frame = self.frames.last().unwrap();
        let (base, home, enclosing) = (frame.base, frame.home, frame.closure);
        let mut captured = Vec::with_capacity(entries.len());
        for e in entries {
            let v = match e.src {
                CapSrc::Slot(i) => self.stack[base + i],
                CapSrc::Captured(parent_slot) => enclosing
                    .and_then(|h| match self.heap.get(h) {
                        Obj::Closure { captured, .. } => {
                            captured.get(parent_slot as usize).copied()
                        }
                        _ => None,
                    })
                    .unwrap_or(Value::Nil),
            };
            // Deep-copy across the airlock: the task can't share mutable state with the parent.
            // Positional (lever #3): slot order matches the synthetic block proto's `capture_names`.
            captured.push(self.deep_clone(v, span)?);
        }
        let h = self.heap.alloc(Obj::Closure {
            proto,
            captured,
            home,
        });
        self.register_task(
            PendingCall::Call {
                callee: Value::Obj(h),
                args: Vec::new(),
                span,
            },
            span,
        )
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
            // The eager nursery owns its OWN sched (a single scope 0 — see `activate_eager_nursery`);
            // `inject` overwrites the `0` placeholder `task_index` under its lock.
            let fiber = self.prepare_worker(task)?.into_fiber(0, 0);
            sched.inject(fiber, 0);
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
            let mn_scope = self.mn_scopes.pop().flatten(); // lockstep — Some if early-enlisted
            let nursery = self.nurseries.pop().unwrap_or_default();
            // Cross-nursery flat scheduler — an EARLY-ENLISTED nursery's tasks are LIVE fibers already
            // seeded into the global sched (its `tasks` vec was drained), so an escape past its join must
            // CANCEL + drain them (trip the scope cancel, requeue parked, settle), exactly like an eager
            // nursery's `abort_eager_nursery`. No pending-task report (the tasks DID start).
            if let Some(scope_id) = mn_scope {
                self.abort_enlisted_scope(scope_id);
                // A `spawn:` issued after the enlist refilled `nursery` with unstarted late tasks; on an
                // escape they never started → report them cancelled (parity with the lazy arm below and
                // with coop), rather than silently dropping them. (Cross-nursery flat scheduler — #3.)
                if !nursery.is_empty() {
                    self.out
                        .push_str(&crate::runtime::pending_cancel_report(nursery.len()));
                }
                continue;
            }
            // Per-connection spawn — pop the eager scope in lockstep. An eager nursery's handlers are
            // already-started live fibers (no unstarted `PendingCall`s to count): cancel + drain + flush
            // them. A lazy nursery's entries are unstarted tasks → report one line per such nursery.
            match self.eager_scheds.pop().flatten() {
                Some(scope) => self.abort_eager_nursery(scope),
                None => {
                    if !nursery.is_empty() {
                        self.out
                            .push_str(&crate::runtime::pending_cancel_report(nursery.len()));
                    }
                }
            }
        }
    }

    fn join_nursery(&mut self) -> Result<(), RuntimeError> {
        // Consume this nursery's tasks (FIFO). Popping the entry now (as the old drain did at the
        // end) keeps the parent's `Handler::nursery_len` accounting correct on a later fault.
        self.nursery_defer_floors.pop(); // keep the parallel floor stack in lockstep with `nurseries`
        let mn_scope = self.mn_scopes.pop().flatten(); // lockstep — Some if early-enlisted
        let tasks = self.nurseries.pop().unwrap_or_default();
        // Per-connection spawn — pop the eager scope in lockstep. An eager nursery injected its tasks
        // live (so `tasks` is empty); its join drains the handlers it spawned, not a queued list.
        if let Some(Some(scope)) = self.eager_scheds.pop() {
            return self.join_eager_nursery(scope);
        }
        // Cross-nursery flat scheduler — this nursery was EARLY-ENLISTED into the global sched (its
        // sibling tasks already seeded as a scope so a nested nursery's owner could run them). Its
        // `tasks` were drained, so join = run the inline owner of that scope (drain any still-parked
        // siblings), wait, and reduce THAT scope's slot sub-range (preserving per-nursery-join flush
        // order → parity). See `run_mn_nursery_nested`.
        if let Some(scope_id) = mn_scope {
            self.join_enlisted_scope(scope_id)?;
            // A `spawn:` issued AFTER this nursery was enlisted refilled the drained `tasks` vec (the
            // enlist `take()` emptied it, but `mn_scopes[i]` stayed `Some`). Those late tasks were NOT
            // part of the enlisted scope — run them now, at the join, exactly as the lazy path below
            // (coop runs nursery tasks at the join too; late spawns post-date the nested `inner()` join,
            // so they have no live inner peer → parity holds). Falls through to the normal task path:
            // `run_mn_nursery` routes them to the HELD sched (if an outer scope is still enlisted) as a
            // fresh TRAILING scope — `register_scope_seeded` is append-only so the flat slots stay contiguous,
            // and it un-latches a stale `terminate` so the inline owner runs the late task instead of
            // stopping on the prior-scopes-all-done flag — else to a fresh outermost sched once no sched
            // is held. No clobber of the held sched, no panic, no drop. (Cross-nursery flat scheduler — #3.)
            if tasks.is_empty() {
                return Ok(());
            }
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
            .map(|(i, t)| Fiber {
                span: t.span(),
                ctx: FiberCtx::default(),
                state: FiberState::Pending(t),
                task_index: i,
                scope_id: 0,
                resume_native: None,
            })
            .collect();
        // D0: every child starts `Pending` ⇒ runnable, so seed `ready` with all indices in order.
        let ready = (0..children.len()).collect();
        // Park the parent: move its live context into the nursery, leaving `self.*` as the fresh,
        // empty arena the children execute in. The nursery (parent + children) is GC-rooted while on
        // `scheduler_stack`.
        let mut nursery = Nursery {
            parent: FiberCtx::default(),
            children,
            ready,
            blocked_on: std::collections::HashMap::new(),
        };
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
        // Cross-nursery flat scheduler — the OUTERMOST nursery (`self.mn.is_none()`) builds the ONE
        // global sched + farms helpers; a NESTED nursery (`self.mn.is_some()`) REUSES it (register a
        // scope, enlist into the same global run queue, run its inline owner scope-scoped). Because the
        // inline owner drains the GLOBAL queue, it naturally runs cross-nursery siblings — the case-A
        // fix (`docs/cross-nursery-flat-scheduler.md`). Nested owners farm NO helpers (they run inline
        // on the worker thread that called them).
        //
        // A THIRD case: `self.mn.is_none()` (no nested sched installed — the body runs `mn == None`) yet a
        // flat sched is HELD in `mn_enlist_sched` because an OUTER nursery was early-enlisted and has not
        // joined yet. This is a late `spawn:` into a non-outermost nursery (charge #3): the leftover late
        // tasks reach `JoinNursery` after the enlist drained the original vec. They must run on the HELD
        // sched as a fresh TRAILING scope (`register_scope_seeded` is append-only, so the flat slot ranges stay
        // contiguous) — NOT via `run_mn_nursery_outermost`, which would build a fresh sched and CLOBBER
        // the held one, leaving a later `join_enlisted_scope(scope_id)` to index a stale scope_id into the
        // fresh (len-1) sched → `index out of bounds` panic. Reusing the nested path is correct: the late
        // trailing scope reduces inline at its own join (it is NOT counted in `mn_enlisted`), exactly like
        // a nested nursery. Only when no sched is held do we build a fresh outermost sched.
        match (self.mn.clone(), self.mn_enlist_sched.clone()) {
            (Some(sched), _) => self.run_mn_nursery_nested(&sched, tasks),
            (None, Some(held)) => self.run_mn_nursery_nested(&held, tasks),
            (None, None) => self.run_mn_nursery_outermost(tasks),
        }
    }

    /// Cross-nursery flat scheduler — the OUTERMOST `parallel:` nursery (`self.mn.is_none()`): build the
    /// one global `MnSched`, farm helper shells, run the inline owner of scope 0, reduce, tear down.
    ///
    /// Case-A FIX: a `parallel:` body runs INLINE before its `JoinNursery`, so when *this* nursery is
    /// reached via a NESTED nursery's join (e.g. `inner()`'s implicit nursery joins while `main`'s outer
    /// `parallel:` still has an un-run sibling `O` queued), the outer sibling `O` is not yet in any
    /// scheduler. So the builder EARLY-ENLISTS every still-pending OUTER nursery (from `self.nurseries`)
    /// as its own scope — seeding `O` so the nested owner, draining the GLOBAL queue, can RUN it (the
    /// cross-nursery wake). But each enlisted scope's OUTPUT is reduced at ITS OWN `JoinNursery` (NOT
    /// here), so the per-nursery-join flush ORDER — and three-engine parity for non-blocking nested
    /// spawns — is preserved. `self.mn_enlisted` counts those deferred scopes; `self.mn` stays installed
    /// until the LAST of them joins (`join_enlisted_scope` tears it down).
    fn run_mn_nursery_outermost(&mut self, tasks: Vec<PendingCall>) -> Result<(), RuntimeError> {
        let total = tasks.len();
        let cancel = Arc::new(AtomicBool::new(false));
        // Peek a real nursery-site span BEFORE consuming `tasks` — a module-global generator faults
        // here (the first nursery snapshots all globals) and must report a real location.
        let nursery_span = tasks
            .first()
            .map(|t| t.span())
            .unwrap_or(Span { line: 1, col: 1 });
        let snap = self.ensure_snapshot(nursery_span)?;
        let mut fibers = Vec::with_capacity(total);
        for (i, t) in tasks.into_iter().enumerate() {
            fibers.push(self.prepare_worker(t)?.into_fiber(i, 0)); // scope 0 — the outermost nursery
        }
        let deadlock_err = self.err(DEADLOCK_MSG.to_string(), Span { line: 1, col: 1 });
        // Worker count must account for the early-enlisted OUTER scopes' tasks too (case-A: `main`'s `O`),
        // so a multi-task inner nursery + outer siblings still gets real parallelism. We don't yet know
        // the outer totals here, so size to a reasonable upper bound (core count) capped by total work
        // after enlisting is impossible to know pre-register; use core count (the inline owner alone
        // still guarantees completion, helpers only accelerate).
        let nworkers = worker_count();
        let sched = Arc::new(MnSched::new(
            total,
            nworkers,
            Arc::clone(&cancel),
            deadlock_err,
        ));
        sched.seed(fibers);
        // Early-enlist OUTER still-pending nurseries (case-A: `main`'s sibling `O` when the builder is a
        // nested join) — BEFORE farming any helper or starting the owner, so EVERY scope's fibers are
        // seeded (runnable-accounted) before any worker can run scope 0 to a park: else a helper could
        // run scope 0's fiber to a park and trip the global deadlock predicate before `O` is enlisted (a
        // multi-task inner nursery race). The sched is held in `mn_enlist_sched` (NOT `self.mn` — the
        // inline body must run with `mn == None` so it does not take the worker-only yield/park paths).
        // Each enlisted scope reduces at its OWN `JoinNursery` (deferred — preserves per-nursery order).
        // Install `mn_enlist_sched` only AFTER a SUCCESSFUL enlist that actually deferred a scope: an
        // early `?` (a `prepare_worker` backstop on a non-crossable spawn) must leave NO stale sched
        // behind for a later `join_enlisted_scope`/inline-send to pick up; and if nothing was enlisted
        // there are no deferred joins, so it stays `None` (clean teardown at this owner's reduce).
        self.early_enlist_outer(&sched)?;
        if self.mn_enlisted > 0 {
            self.mn_enlist_sched = Some(Arc::clone(&sched));
        }
        // NOW farm helper shells — SENTINEL (drain the global queue across all scopes until global
        // terminate). Farming AFTER the enlist closes the deadlock-predicate race above.
        for wid in 1..nworkers {
            let mut shell = self.spawn_shell(&snap, &sched, &cancel);
            let sched = Arc::clone(&sched);
            pool::submit(Box::new(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    shell.mn_worker_loop(&sched, wid, SENTINEL_SCOPE)
                }));
            }));
        }
        let mut shell = self.spawn_shell(&snap, &sched, &cancel);
        shell.mn_worker_loop(&sched, 0, 0); // owner of scope 0
        // The owner returned on scope 0; reduce scope 0's sub-range. The sched is released only when no
        // early-enlisted outer scope is still pending (else those scopes' slots must survive until their
        // own joins reduce them — `join_enlisted_scope` releases it at the last).
        sched.wait_for_scope(0);
        let slots = sched.take_scope_slots(0);
        if self.mn_enlisted == 0 {
            self.mn_enlist_sched = None;
        }
        self.reduce_task_slots(slots)
    }

    /// Cross-nursery flat scheduler — a NESTED `parallel:` nursery reusing the ONE global sched. EARLY-
    /// ENLIST every still-pending OUTER nursery (so a cross-nursery sibling like case-A's `O` is seeded
    /// and the inline owner — draining the GLOBAL queue — can run it), recording each enlisted scope on
    /// `self.mn_scopes` so its OWN `JoinNursery` reduces it (deferred — preserves per-nursery flush
    /// order). Then register + seed this nursery's OWN scope, run its inline owner SCOPE-SCOPED (returns
    /// the instant ITS scope is done, having drained the global queue meanwhile), wait, reduce its
    /// sub-range. Farms NO helpers (runs inline on the worker thread that called it, reusing `self.wid`).
    fn run_mn_nursery_nested(
        &mut self,
        sched: &Arc<MnSched>,
        tasks: Vec<PendingCall>,
    ) -> Result<(), RuntimeError> {
        // Runs on a worker shell (`module_snapshot.is_some()`), so `ensure_snapshot` early-returns and
        // never re-lowers globals — but pass a real span anyway (peeked before consuming `tasks`).
        let nursery_span = tasks
            .first()
            .map(|t| t.span())
            .unwrap_or(Span { line: 1, col: 1 });
        let snap = self.ensure_snapshot(nursery_span)?;
        // This nursery's OWN scope. Prepare every worker FIRST (the fallible/heap-heavy step — touches no
        // scheduler state), THEN register the scope and seed its fibers atomically. Doing the prepare
        // BEFORE registration is what makes `register_scope_seeded` race-free: there is no window where the
        // scope exists with `runnable == 0` (the old `register_scope` → prepare_worker → `seed` ordering
        // left exactly that gap, which on the late-spawn-into-middle path — inline builder not counted in
        // `running` — let a SENTINEL helper fault an innocent parked outer sibling). On a `prepare_worker`
        // Err no scope is registered (clean unwind).
        let cancel = Arc::new(AtomicBool::new(false));
        let mut workers = Vec::with_capacity(tasks.len());
        for t in tasks {
            workers.push(self.prepare_worker(t)?);
        }
        let scope_id = sched.register_scope_seeded(Arc::clone(&cancel), workers);
        let wid = self.wid;
        let mut shell = self.spawn_shell(&snap, sched, &cancel);
        shell.mn_worker_loop(sched, wid, scope_id);
        sched.wait_for_scope(scope_id);
        let slots = sched.take_scope_slots(scope_id);
        self.reduce_task_slots(slots)
    }

    /// Cross-nursery flat scheduler — EARLY-ENLIST every OUTER still-pending nursery (those above the
    /// current one on `self.nurseries`) into `sched` as its own scope: seed its sibling tasks as live
    /// fibers (so a nested owner draining the GLOBAL queue can run them — the cross-nursery wake), drain
    /// its `tasks` vec, and record the scope on `self.mn_scopes` + bump `self.mn_enlisted` so its OWN
    /// `JoinNursery` reduces it (deferred — preserving per-nursery flush order and three-engine parity).
    /// Idempotent per nursery (skips any already-enlisted `Some(_)` and any empty one).
    fn early_enlist_outer(&mut self, sched: &Arc<MnSched>) -> Result<(), RuntimeError> {
        // Independent/normal multi-level nesting is fully supported: every still-pending OUTER nursery is
        // enlisted as its own scope here, and the genuinely-CONTENDED case (2+ live receivers racing ONE
        // channel across nested nurseries) is NOT gated — it is concurrent-divergent by design (delivery
        // order may differ from the cooperative engine, or it may deadlock-fault; suspendable concurrency
        // is VM-only / divergent under `--parallel`, see PROGRESS.md). It must only never PANIC.
        for i in 0..self.nurseries.len() {
            if self.mn_scopes[i].is_some() || self.nurseries[i].is_empty() {
                continue;
            }
            let total = self.nurseries[i].len();
            // VALIDATE THEN COMMIT (atomic enlist — charge #4). Prepare every task from a CLONE first,
            // BEFORE any irreversible mutation. `prepare_worker` is the only fallible step (the checker
            // gates non-crossable spawns, so this backstop normally never fires — but if it did, an early
            // `?` here must not leave a half-state). On `Err` the nursery is untouched (originals still in
            // `self.nurseries[i]`), no scope is registered (so no unseeded scope can hang `wait_for_scope`)
            // and `mn_scopes`/`mn_enlisted` are unbumped — the fault propagates cleanly, matching coop.
            let clones: Vec<PendingCall> = self.nurseries[i].clone();
            let mut prepared = Vec::with_capacity(total);
            for t in clones {
                prepared.push(self.prepare_worker(t)?);
            }
            // COMMIT — nothing fallible remains. Discard the originals (the clones became the fibers),
            // register + seed the scope, and record it for its OWN `JoinNursery` to reduce.
            let _ = std::mem::take(&mut self.nurseries[i]);
            let cancel = Arc::new(AtomicBool::new(false));
            let scope_id = sched.register_scope(total, Arc::clone(&cancel));
            // Mark the scope as awaiting the builder's own join: its parked fibers have the live builder
            // body as a feeder, so the deadlock predicate must not fault them until the builder reaches
            // this scope's `JoinNursery` (which clears the flag). (Cross-nursery flat scheduler — #1/#2.)
            let base = {
                let mut c = sched.lock();
                c.scopes[scope_id].awaiting_builder = true;
                c.scopes[scope_id].base_index
            };
            let fibers: Vec<Fiber> = prepared
                .into_iter()
                .enumerate()
                .map(|(j, p)| p.into_fiber(base + j, scope_id))
                .collect();
            sched.seed(fibers);
            self.mn_scopes[i] = Some(scope_id);
            self.mn_enlisted += 1;
        }
        Ok(())
    }

    /// Cross-nursery flat scheduler — `JoinNursery` for a nursery that was EARLY-ENLISTED: its tasks are
    /// already live fibers in `sched` (seeded by `early_enlist_outer`). Run the inline owner of that
    /// scope to drain any still-parked siblings, wait for the scope, reduce its slot sub-range (deferred
    /// flush — preserves per-nursery order), and release the held sched once the last enlisted scope
    /// joins. Runs on the INLINE builder VM (`self.mn == None`), so the owner loop is on a SHELL.
    fn join_enlisted_scope(&mut self, scope_id: usize) -> Result<(), RuntimeError> {
        let sched = self
            .mn_enlist_sched
            .clone()
            .expect("join_enlisted_scope without a held sched");
        // The builder has reached THIS scope's join — it is no longer feeding it from body code, it is now
        // blocked draining it. Clear `awaiting_builder` so a genuine post-body deadlock (this scope parked
        // with no live sender) faults instead of being vetoed. (Cross-nursery flat scheduler — #1/#2.)
        sched.lock().scopes[scope_id].awaiting_builder = false;
        // The snapshot was already built (and any module-global generator already faulted) at the
        // outermost nursery, so this is a memo/worker-shell early-return — the span is unused.
        let snap = self.ensure_snapshot(Span { line: 1, col: 1 })?;
        let cancel = Arc::clone(&sched.lock().scopes[scope_id].cancel);
        let wid = self.wid;
        let mut shell = self.spawn_shell(&snap, &sched, &cancel);
        shell.mn_worker_loop(&sched, wid, scope_id);
        sched.wait_for_scope(scope_id);
        let slots = sched.take_scope_slots(scope_id);
        self.mn_enlisted -= 1;
        if self.mn_enlisted == 0 {
            self.mn_enlist_sched = None;
        }
        self.reduce_task_slots(slots)
    }

    /// Cross-nursery flat scheduler — reclaim an EARLY-ENLISTED scope whose nursery ESCAPED past its
    /// join (`?`/`return`/`break`/`continue`/caught fault). Its tasks are live fibers, so cancel them
    /// (trip the scope cancel, drain, settle — like `abort_eager_nursery`), reduce (only `os.exit`
    /// honored — the escape error is what propagates), and release the held sched at the last scope.
    fn abort_enlisted_scope(&mut self, scope_id: usize) {
        let Some(sched) = self.mn_enlist_sched.clone() else {
            return;
        };
        // Draining (cancelling) this scope, not feeding it — clear `awaiting_builder` so the cancel
        // quiesce is observed promptly rather than vetoed. (Cross-nursery flat scheduler — #1/#2.)
        sched.lock().scopes[scope_id].awaiting_builder = false;
        let cancel = Arc::clone(&sched.lock().scopes[scope_id].cancel);
        cancel.store(true, Ordering::Relaxed);
        sched.cancel_drain(scope_id);
        poller::drain_sched(&sched);
        // Worker-shell / memo early-return (the snapshot was already built at the outermost nursery,
        // so this cannot fault); `.expect` the Ok with that justification (this fn returns `()`).
        let snap = self
            .ensure_snapshot(Span { line: 1, col: 1 })
            .expect("abort_enlisted_scope: snapshot already built (no fault possible)");
        let wid = self.wid;
        let mut shell = self.spawn_shell(&snap, &sched, &cancel);
        shell.mn_worker_loop(&sched, wid, scope_id);
        sched.wait_for_scope(scope_id);
        let slots = sched.take_scope_slots(scope_id);
        self.mn_enlisted -= 1;
        if self.mn_enlisted == 0 {
            self.mn_enlist_sched = None;
        }
        let _ = self.reduce_task_slots(slots); // escape error propagates; only os.exit honored here
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
        // An eager nursery only activates on a worker shell where `module_snapshot.is_some()` (the
        // debug_assert below), so `ensure_snapshot` always early-returns the installed Arc and cannot
        // fault — `.expect` the Ok (this fn returns `EagerScope`, not Result). The span is unused.
        let snap = self.ensure_snapshot(Span { line: 1, col: 1 }).expect(
            "activate_eager_nursery: worker shell has an installed snapshot (no fault possible)",
        );
        debug_assert!(
            self.module_snapshot.is_some(),
            "an eager nursery only activates on a worker shell (gated by mn.is_some())"
        );
        let deadlock_err = self.err(DEADLOCK_MSG.to_string(), Span { line: 1, col: 1 });
        // wid 0 = inline join worker; wid 1 = the dedicated raw drainer below.
        let sched = Arc::new(MnSched::new(0, 2, Arc::clone(&cancel), deadlock_err));
        sched.open_body(0);
        let mut shell = self.spawn_shell(&snap, &sched, &cancel);
        let drain_sched = Arc::clone(&sched);
        let drainer = std::thread::Builder::new()
            .stack_size(VM_STACK_BYTES)
            .name("chezzi-eager".into())
            .spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    shell.mn_worker_loop(&drain_sched, 1, 0)
                }));
            })
            .ok();
        EagerScope {
            sched,
            cancel,
            drainer,
        }
    }

    /// Per-connection spawn — `JoinNursery` for an eager nursery (the normal fall-through path). Close
    /// the body (no more injections → the sched may terminate once every handler is done), then run
    /// the inline join worker (`wid` 0) to help drain remaining handlers, wait for every slot to fill,
    /// and reduce (Decision-F output flush in spawn order; a handler fault propagates as the
    /// acceptor's body fault, which the outer nursery then sees). Mirrors `run_mn_nursery`'s tail.
    fn join_eager_nursery(&mut self, scope: EagerScope) -> Result<(), RuntimeError> {
        let EagerScope {
            sched,
            cancel,
            drainer,
            ..
        } = scope;
        sched.close_body(0);
        // Worker-shell early-return (an eager nursery runs only on a shell with an installed
        // snapshot) — cannot fault; the span is unused.
        let snap = self.ensure_snapshot(Span { line: 1, col: 1 })?;
        let mut shell = self.spawn_shell(&snap, &sched, &cancel);
        shell.mn_worker_loop(&sched, 0, 0);
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
        let EagerScope {
            sched,
            cancel,
            drainer,
            ..
        } = scope;
        cancel.store(true, Ordering::Relaxed);
        sched.close_body(0);
        sched.cancel_drain(0);
        poller::drain_sched(&sched);
        // Worker-shell early-return (an eager nursery runs only on a shell with an installed
        // snapshot) — cannot fault; `.expect` the Ok (this fn returns `()`).
        let snap = self.ensure_snapshot(Span { line: 1, col: 1 }).expect(
            "abort_eager_nursery: worker shell has an installed snapshot (no fault possible)",
        );
        let mut shell = self.spawn_shell(&snap, &sched, &cancel);
        shell.mn_worker_loop(&sched, 0, 0);
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
    fn spawn_shell(
        &self,
        snap: &Arc<ModuleSnapshot>,
        sched: &Arc<MnSched>,
        cancel: &Arc<AtomicBool>,
    ) -> Vm {
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
        let sched = Arc::clone(
            self.mn
                .as_ref()
                .expect("demote_recv_block on the cooperative engine"),
        );
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
                // A tripped latch (`trip()`) delivers `true` like a passed timer — ranks below a real
                // queued value, above closed/terminate/deadlock (a `done().recv()` on a cancelled token
                // reached inside a native callback must not false-deadlock).
                if core.done_latch.load(Ordering::Relaxed) {
                    c.running += 1;
                    sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
                    c.unregister_demoted(ptr);
                    drop(qg);
                    drop(c);
                    return Ok(RecvStep::Got(WireValue::Bool(true)));
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
                if self
                    .cancel
                    .as_ref()
                    .is_some_and(|x| x.load(Ordering::Relaxed))
                {
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

    /// §6d M:N — a blocking multi-channel `wait` reached INSIDE a native callback (`native_reentry > 0`).
    /// A host-stack loop frame sits between the worker loop and the `wait`, so the fiber CANNOT
    /// snapshot-park (`park_wait`); it demotes — blocks this worker in place, polling all N arm queues
    /// in **source order** on a bounded `DEMOTE_POLL_BACKOFF` backoff. The N-arm analogue of
    /// [`Vm::demote_recv_block`]: account `running → blocked_native`, register EVERY arm channel in
    /// `demoted_chans` (so `is_deadlocked` peeks them all and a value racing onto any arm vetoes a false
    /// fire), spin a replacement worker once, then loop. Because there are N channel condvars (no single
    /// one to block on), the wait is a bounded poll rather than a targeted condvar wait — lower
    /// throughput but sound (the documented v1 limitation, same shape as the timer-in-callback note).
    /// Returns `(arm_index, value)` for the first source-order arm to deliver. A per-arm `close`+empty
    /// is SKIPPED; once EVERY arm is closed+empty it returns "all channels closed". Cancel/terminate/
    /// self-detected-deadlock fault in place. Never parks. Only called on the M:N engine inside a
    /// callback (gated `mn.is_some() && native_reentry > 0`).
    fn demote_wait_block(
        &mut self,
        arms: Vec<(usize, Arc<ChannelCore>)>,
        timer: Option<(usize, std::time::Instant)>,
        span: Span,
    ) -> Result<(usize, WireValue), RuntimeError> {
        let sched = Arc::clone(
            self.mn
                .as_ref()
                .expect("demote_wait_block on the cooperative engine"),
        );
        // 1. Account running → blocked_native AND register EVERY arm channel, under core lock A, then
        //    notify so an idle puller re-evaluates the deadlock predicate now this fiber left `running`.
        {
            let mut c = sched.lock();
            c.running -= 1;
            sched.blocked_native.fetch_add(1, Ordering::Relaxed);
            for (ptr, core) in &arms {
                c.register_demoted(*ptr, core);
            }
            drop(c);
            sched.cv.notify_all();
        }
        // Un-account helper: reverse step 1 (called on every exit path), caller holds core lock A.
        let un_account = |c: &mut SchedCore| {
            c.running += 1;
            sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
            for (ptr, _) in &arms {
                c.unregister_demoted(*ptr);
            }
        };
        // 2. Spin a replacement worker ONCE per demoted thread (covers this `wid` while we block). If the
        //    OS refuses the thread, un-roll step 1 and fault cleanly so the join still completes.
        if !self.demoted {
            if !self.spawn_replacement_worker(&sched, self.wid) {
                let mut c = sched.lock();
                un_account(&mut c);
                drop(c);
                return Err(self.err(
                    "wait inside a native callback could not demote the worker (OS thread limit \
                     reached) — reduce concurrent in-callback blocking or raise the thread limit"
                        .to_string(),
                    span,
                ));
            }
            self.demoted = true;
        }
        // 3. Block in place. Each poll: under core lock A, scan all N arms in source order — the first
        //    with a queued value wins (un-account + return). Then rank cancel > all-closed > terminate
        //    > self-detected-deadlock, exactly like `demote_recv_block`, but generalized over N arms.
        loop {
            {
                let mut c = sched.lock();
                // Source-order poll: pop the first arm with a queued value (atomic with the A hold).
                let mut all_closed = true;
                for (idx, (_, core)) in arms.iter().enumerate() {
                    let mut qg = core.q.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(w) = qg.queue.pop_front() {
                        drop(qg);
                        un_account(&mut c);
                        drop(c);
                        return Ok((idx, w));
                    }
                    // A tripped latch (`trip()`) makes this arm ready with `true` (after the value scan).
                    if core.done_latch.load(Ordering::Relaxed) {
                        drop(qg);
                        un_account(&mut c);
                        drop(c);
                        return Ok((idx, WireValue::Bool(true)));
                    }
                    if !qg.closed {
                        all_closed = false;
                    }
                }
                // Cancel (a sibling faulted): swallow the outcome (mirror the snapshot-park cancel arm).
                if self
                    .cancel
                    .as_ref()
                    .is_some_and(|x| x.load(Ordering::Relaxed))
                {
                    self.cancelled = true;
                    un_account(&mut c);
                    drop(c);
                    return Err(self.err("cancelled".to_string(), span));
                }
                // WAIT-1 (demote path) — a live timer arm fires only AFTER the source-order channel scan
                // failed (so a real `send` to any arm beats the timer on a tie). Once `now >= deadline`,
                // take the timer arm with `true`. A still-pending timer vetoes the deadlock fault below
                // (a value WILL arrive at the deadline — like an `inflight` job on the snapshot path).
                if let Some((idx, deadline)) = timer
                    && std::time::Instant::now() >= deadline
                {
                    un_account(&mut c);
                    drop(c);
                    return Ok((idx, WireValue::Bool(true)));
                }
                // Every arm closed+empty: no value can ever arrive — the all-closed `wait` fault. (A live
                // timer arm keeps `all_closed` false, so this fires only with no timer pending.)
                if all_closed {
                    un_account(&mut c);
                    drop(c);
                    return Err(self.err("wait: all channels closed".to_string(), span));
                }
                if c.terminate {
                    un_account(&mut c);
                    drop(c);
                    return Err(sched.deadlock_err.clone());
                }
                // A pending timer guarantees future progress (its deadline send), so it vetoes the
                // self-detected deadlock just like an `inflight` job does on the snapshot-park path.
                if timer.is_none() && sched.is_deadlocked(&c) {
                    c.flag_deadlock(&sched.deadlock_err);
                    un_account(&mut c);
                    drop(c);
                    sched.cv.notify_all();
                    return Err(sched.deadlock_err.clone());
                }
            }
            // No single condvar to wait on (N arms, N condvars) → bounded backoff poll. Sleep on the
            // FIRST arm's condvar with a timeout so a `send`/`close` to arm 0 wakes promptly, and any
            // other arm is observed within `DEMOTE_POLL_BACKOFF` (the documented lower-throughput path).
            let first = &arms[0].1;
            let q = first.q.lock().unwrap_or_else(|e| e.into_inner());
            if q.queue.is_empty() {
                // Clamp the backoff to the timer deadline so the loop re-polls and fires the timer arm
                // by its deadline (saturating, so a deadline that already passed yields ~zero wait).
                let backoff = match timer {
                    Some((_, d)) => DEMOTE_POLL_BACKOFF
                        .min(d.saturating_duration_since(std::time::Instant::now())),
                    None => DEMOTE_POLL_BACKOFF,
                };
                let _ = first.cv.wait_timeout(q, backoff);
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
        let sched = Arc::clone(
            self.mn
                .as_ref()
                .expect("demote_block_sleep on the cooperative engine"),
        );
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
        if self
            .cancel
            .as_ref()
            .is_some_and(|x| x.load(Ordering::Relaxed))
        {
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
        let sched = Arc::clone(
            self.mn
                .as_ref()
                .expect("demote_block_socket on the cooperative engine"),
        );
        self.demote_socket_enter(span)?;
        let out = loop {
            // Observe teardown/cancel BEFORE doing more work each iteration. Cancel (a sibling faulted):
            // set `cancelled` so the outcome is SWALLOWED (a cancelled task is dropped, not reported).
            if self
                .cancel
                .as_ref()
                .is_some_and(|x| x.load(Ordering::Relaxed))
            {
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
        let sched = Arc::clone(
            self.mn
                .as_ref()
                .expect("demote_socket_enter on the cooperative engine"),
        );
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
        let sched = Arc::clone(
            self.mn
                .as_ref()
                .expect("demote_socket_exit on the cooperative engine"),
        );
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
                // SENTINEL — the replacement covers the demoted thread's `wid` and drains the GLOBAL
                // queue (across all scopes) until global terminate; the demoted owner returns on its own
                // (its fiber settles → `self.demoted` exits its loop), so the replacement must not stop
                // early on any single scope.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    shell.mn_worker_loop(&sched, wid, SENTINEL_SCOPE)
                }));
            })
            .is_ok()
    }

    /// D2b — a worker shell's lifetime: pull a runnable fiber, run it to its next park/finish, settle,
    /// repeat until the scheduler terminates. Generalizes the cooperative [`Vm::run_child`] to a
    /// shared run queue + park set across threads.
    /// Cross-nursery flat scheduler — `owner_scope` is the scope this worker is the INLINE OWNER of
    /// (it returns when that scope completes — scope-scoped owner stop), or [`SENTINEL_SCOPE`] for a
    /// FARMED helper / drainer (which never self-stops, only on global terminate). The fiber it runs may
    /// belong to ANY scope (the queue is global): `finish`/`cancel_drain` use the FIBER's `scope_id`,
    /// while `take_runnable`'s stop check uses `owner_scope`.
    fn mn_worker_loop(&mut self, sched: &Arc<MnSched>, wid: usize, owner_scope: usize) {
        self.wid = wid; // D5 owe #3 (Path C) — `demote_recv_block` reuses this for the replacement worker
        let mut tick: u64 = 0;
        loop {
            tick = tick.wrapping_add(1);
            let mut fiber = match sched.take_runnable(wid, tick, owner_scope) {
                Take::Run(f) => f,
                Take::Stop => return,
            };
            let task_index = fiber.task_index;
            let scope_id = fiber.scope_id;
            let span = fiber.span;
            match self.run_one_fiber(&mut fiber, span) {
                Disp::Park(key, core) => sched.park(key, &core, fiber),
                // §6d — multi-channel `wait` park: file ONE shared token in every arm's bucket.
                Disp::WaitPark(arms) => sched.park_wait(arms, fiber),
                Disp::Yield => sched.yield_fiber(fiber),
                // D5 — the fiber hit a blocking native; hand it + the call to the dirty pool (frees
                // this worker). The pool re-enqueues it on completion via `complete_offload`.
                Disp::Offload(req) => sched.offload(fiber, req),
                // D6 — the fiber's socket op `WouldBlock`ed; hand it + the fd to the netpoller (frees
                // this worker). The poller re-enqueues it via `complete_offload` on OS readiness.
                Disp::PollPark(pp) => sched.poll_park_offload(fiber, pp),
                Disp::Finish(outcome) => {
                    let aborts = matches!(
                        outcome,
                        TaskOutcome::Fault { .. } | TaskOutcome::Exit { .. }
                    );
                    sched.finish(task_index, scope_id, outcome);
                    // A fault/exit tripped the FIBER's SCOPE cancel (in `classify_mn_outcome`, via the
                    // re-pointed `self.cancel`); requeue THAT scope's parked siblings so they observe it
                    // and unwind (running ones see it at a back-edge). `cancel_drain(scope_id)` reaches
                    // channel-`recv`-parked fibers in this scope ONLY (never outer siblings — structured
                    // concurrency); `drain_sched` reaches the netpoller-parked ones. Together they cover
                    // every parked fiber of the faulting scope, so a net server sharing a nursery with a
                    // faulting sibling now unwinds instead of hanging (D6b — the production-ready gate).
                    if aborts {
                        sched.cancel_drain(scope_id);
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
        // Cross-nursery flat scheduler — RE-POINT the shell's `self.cancel` to THIS fiber's SCOPE cancel
        // on every swap-in. One shell runs fibers from MULTIPLE scopes off the global queue; the
        // back-edge cancel check (`run_until`), `trip_cancel` (on this fiber's fault/exit), the demote
        // loops, and the netpoller `register` all read `self.cancel`, so it MUST track the running
        // fiber's scope — else an inner fault would trip the wrong scope and cancel an outer sibling.
        // No-op for the cooperative engine / a non-fiber run (`mn` is `None` there).
        if let Some(sched) = self.mn.clone() {
            let scope_cancel = Arc::clone(&sched.lock().scopes[fiber.scope_id].cancel);
            self.cancel = Some(scope_cancel);
        }
        self.suspend = None;
        self.wait_suspend = None; // set by `op_wait_poll`'s M:N snapshot-park (→ `Disp::WaitPark`)
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
            } else if res.is_ok() && self.wait_suspend.is_some() {
                // §6d — the fiber blocked on a multi-channel `wait`. Capture each arm's (key, core)
                // WHILE the fiber heap is live (the `GcRef`s index into it), exactly as `Disp::Park`
                // captures the single recv key; `park_wait` re-checks every arm under the sched lock.
                let handles = self.wait_suspend.take().unwrap();
                let arms: Vec<(usize, Arc<ChannelCore>)> = handles
                    .iter()
                    .map(|&h| (self.channel_core_ptr(h), self.channel_core(h)))
                    .collect();
                Disp::WaitPark(arms)
            } else if res.is_ok() && self.yield_now {
                // D3 — budget exhausted (mutually exclusive with `suspend`: the safepoint returns
                // before dispatching, so no `recv` ran this slice). Frames stay intact; resume
                // re-enters `run_until(0)`.
                Disp::Yield
            } else {
                Disp::Finish(self.classify_mn_outcome(res))
            }
        }))
        .unwrap_or_else(|p| {
            Disp::Finish(TaskOutcome::Fault {
                err: panic_to_fault(p, span),
                out: String::new(),
                stderr: String::new(),
            })
        });
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
            TaskOutcome::Exit {
                code,
                out: std::mem::take(&mut self.out),
                stderr: std::mem::take(&mut self.stderr),
            }
        } else if self.cancelled {
            TaskOutcome::Cancelled
        } else {
            match res {
                Err(e) => {
                    self.trip_cancel();
                    TaskOutcome::Fault {
                        err: e,
                        out: std::mem::take(&mut self.out),
                        stderr: std::mem::take(&mut self.stderr),
                    }
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
        let done: Arc<(Mutex<usize>, std::sync::Condvar)> =
            Arc::new((Mutex::new(0), std::sync::Condvar::new()));

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
                    .unwrap_or_else(|p| TaskOutcome::Fault {
                        err: panic_to_fault(p, span),
                        out: String::new(),
                        stderr: String::new(),
                    });
                results.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(r);
            }));
        }
        // 3. Parent participates: run task[0] on this thread (it may block on `recv`, woken by a pool
        //    sibling's `send`). Caught the same way so an inline-task panic still joins the pool tasks
        //    and reports rather than unwinding past the still-pending wait.
        if let Some((i, rw)) = first {
            let span = rw.span;
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rw.run_outcome()))
                .unwrap_or_else(|p| TaskOutcome::Fault {
                    err: panic_to_fault(p, span),
                    out: String::new(),
                    stderr: String::new(),
                });
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
        //     `Done`/`Exit` output is flushed; the terminal (lowest-index propagating) `Fault` flushes
        //     its buffered output at its slot too (oracle parity — a faulting task's partial output is
        //     emitted before the fault unwinds); higher-index racy `Fault`s + `Cancelled` still drop
        //     (no deterministic slot). The fault-free goldens only ever hit `Done`, so byte-identical.
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
    /// `Done`/`Exit` output is flushed in task order (decision F). The terminal (lowest-index
    /// propagating) `Fault` ALSO flushes its buffered output at its slot — matching the cooperative/
    /// interp oracle, which writes a faulting task's partial output before the fault unwinds. Higher-
    /// index racy `Fault`s and `Cancelled` still drop (no deterministic slot — the work is incomplete /
    /// ran past the terminal fault's cancel). The fault-free goldens only ever hit `Done`, so they stay
    /// byte-identical. Precedence: an `os.exit` is an UNCONDITIONAL hard halt, so the lowest-index
    /// `Exit` wins over any `Fault` regardless of index — otherwise a lower-index recoverable fault
    /// could demote a child's `os.exit` to a catchable error. Within a kind, the lowest index wins
    /// (scan order + `is_none()`).
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
                TaskOutcome::Fault { err, out, stderr } => {
                    // The terminal (lowest-index propagating) fault flushes its buffered output at its
                    // task-order slot — after lower-index Done/Exit, before the fault propagates —
                    // so a faulting task's partial output is no longer silently dropped. Higher-index
                    // racy faults still drop (they ran concurrently past the terminal fault's cancel;
                    // no deterministic slot position).
                    //
                    // RESIDUAL RACE (intentionally not chased here): this matches the cooperative/interp
                    // oracle byte-for-byte only when the faulting task is the nursery's SOLE
                    // output-producer. With additional output-producing siblings the M:N result can
                    // still diverge from serial's strict stop-at-first-fault order — a sibling that
                    // reaches `Done` before the faulter's cancel-trip keeps its output (serial would
                    // never have run it), and whether a lower-index sibling ends `Fault` vs `Cancelled`
                    // (which selects the propagating fault) is itself a scheduler race. The
                    // buffer-and-flush-per-task model cannot reconcile concurrency with serial's
                    // sequential stop-at-fault, so multi-task-with-fault output ordering is a separate,
                    // pre-existing nondeterminism, not asserted as parity (see the single-task test
                    // `parallel_faulting_task_flushes_partial_output_3engine`).
                    if first_fault.is_none() {
                        self.out.push_str(&out);
                        self.stderr.push_str(&stderr);
                        first_fault = Some(err);
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
            let next = self
                .scheduler_stack
                .last_mut()
                .expect("scheduler level present")
                .ready
                .pop_first();
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
            let level = self
                .scheduler_stack
                .last_mut()
                .expect("scheduler level present");
            std::mem::replace(
                &mut level.children[i],
                Fiber {
                    ctx: FiberCtx::default(),
                    state: FiberState::Done,
                    task_index: i,
                    scope_id: 0,
                    span: Span { line: 1, col: 1 },
                    resume_native: None,
                },
            )
        };
        self.swap_ctx(&mut child.ctx); // self.* = child's execution context
        self.suspend = None; // clear any prior wait before (re)running
        self.wait_suspend = None;
        // `wait` (§6d) multi-channel park: this fiber may be filed under SEVERAL `blocked_on` keys.
        // A sibling `send` to ONE of them woke it (draining only that bucket); sweep the index out of
        // every other bucket here, before it re-runs, so a later `send` to one of those channels can
        // never re-wake a fiber that already moved on (the doc's "swept out of the other buckets").
        // A no-op for an ordinary single-`recv` park (already removed from its one bucket by the wake).
        if let Some(level) = self.scheduler_stack.last_mut() {
            for bucket in level.blocked_on.values_mut() {
                bucket.retain(|&x| x != i);
            }
            level.blocked_on.retain(|_, v| !v.is_empty());
        }
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
                child.state = if let Some(handles) = self.wait_suspend.take() {
                    // `wait` blocking park: file this child under EVERY live arm-channel key, so a
                    // `send` to any of them re-runs the `WaitPoll` (which re-polls source order).
                    for h in handles {
                        let key = self.channel_core_ptr(h);
                        self.scheduler_stack
                            .last_mut()
                            .expect("scheduler level present")
                            .blocked_on
                            .entry(key)
                            .or_default()
                            .push(i);
                    }
                    FiberState::Blocked
                } else {
                    match self.suspend.take() {
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
                    }
                };
                Ok(())
            }
            Err(e) => {
                child.state = FiberState::Done;
                Err(e)
            }
        };
        self.scheduler_stack
            .last_mut()
            .expect("scheduler level present")
            .children[i] = child;
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
            PendingCall::Method {
                recv,
                name,
                args,
                span,
            } => {
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
    /// Implemented as a [`WireValue`] round-trip — `to_wire` (read-only serialize) then `from_wire`
    /// (reconstruct into this heap). Byte-identical to the old direct deep-copy; the wire form is what
    /// de-risks the cores-as-`Arc` and real-OS-thread-boundary crossings. By-reference objects cross
    /// as `Handle`; the ONE non-sendable value is a frame-holding generator (its parked frames
    /// reference this heap), so this is **fallible**: a generator (or one nested in a container) faults
    /// gracefully here, re-stamped with the real spawn-site `span` (the caller has it) via `to_wire_at`
    /// — a catchable error instead of a panic.
    fn deep_clone(&mut self, v: Value, span: Span) -> Result<Value, RuntimeError> {
        let w = self.to_wire_at(v, span)?;
        Ok(self.from_wire(w))
    }

    /// `to_wire` re-stamped with a real call-site `span`. `to_wire`'s only `Err` (a frame-holding
    /// generator crossing the airlock) carries a placeholder `Span{0,0}`; every method-level airlock
    /// site (`Channel.send`/`Shared.set`/`Atomic.store`/…) has a real span, so route through this so
    /// the catchable error reports the operation's location rather than line 0.
    fn to_wire_at(&self, v: Value, span: Span) -> Result<WireValue, RuntimeError> {
        self.to_wire(v).map_err(|e| self.err(e.message, span))
    }

    /// B3.0 — serialize a value into its [`WireValue`] form (the airlock's outbound half). A
    /// read-only walk of the heap, structurally identical to `deep_clone`'s old recursion but
    /// allocating nothing. Data (list/tuple/map/set/struct/enum) recurses; immutable / by-reference
    /// objects (`Str`, callables, modules, `Channel`/`Shared`/`Executor`) cross as
    /// [`WireValue::Handle`] (the existing handle, same heap in B3.0). `Map`/`Set` carry their cached
    /// hashes through so reconstruction never re-hashes.
    ///
    /// Every `Value` and every `Obj` variant maps to a wire arm — by-reference objects (callables,
    /// modules, `Channel`/`Shared`/`Executor`) cross as `Handle`. The ONE fallible arm is a
    /// frame-holding **generator** (`Obj::Generator`): its parked frames reference this heap, so it
    /// is not sendable and returns a graceful `a generator cannot be sent across tasks` error here
    /// (carrying a placeholder `Span{0,0}` that airlock callers re-stamp with the real site via
    /// `to_wire_at`/`deep_clone`/`ensure_snapshot`). Every other arm is infallible (the `?` only
    /// forwards the generator error up through container recursion).
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
                // `bytes` crosses by value (owned raw bytes), exactly like `str` — immutable +
                // value-compared, so a fresh handle on reconstruction is observationally identical.
                Obj::Bytes(b) => WireValue::Bytes(b.clone()),
                // `bytearray` crosses by value as a DEEP COPY (a fresh independent buffer on the other
                // side, like `list`) — never a shared mutable view. `from_wire` rebuilds a new heap
                // `bytearray`, so cross-thread mutation never aliases.
                Obj::ByteArray(b) => WireValue::ByteArray(b.clone().into_boxed_slice()),
                // A first-class builtin fn crosses the airlock BY VALUE (its name) — pure code, no
                // `GcRef` and no captured heap state, so it genuinely crosses an OS-thread boundary
                // (unlike a `Func`/`Closure` handle). `from_wire` re-allocs a fresh `Obj::Builtin`;
                // builtins are name-compared, so that is observationally identical. Works on the M:N
                // engine (this path) and the serial engine (`SnapValue::Builtin`) alike.
                Obj::Builtin(name) => WireValue::Builtin(name.clone()),
                // By-reference callables: cross as the existing handle (matches the old deep_clone arm).
                Obj::Func { .. }
                | Obj::Closure { .. }
                | Obj::Module { .. }
                | Obj::Native { .. }
                // A Cffi handle crosses as the existing handle — same shared heap (B3.0); under the
                // M:N engine the worker shares the parent address space, so the symbol stays valid.
                | Obj::Cffi(_) => WireValue::Handle(h),
                // B3.1: the shared cores cross as the `Arc` itself (clone = refcount bump), so a
                // `from_wire` in any heap reaches the same mailbox/box/queue.
                Obj::Channel(core) => WireValue::Channel(Arc::clone(core)),
                Obj::Shared(core) => WireValue::Shared(Arc::clone(core)),
                Obj::RwShared(core) => WireValue::RwShared(Arc::clone(core)),
                Obj::Atomic(core) => WireValue::Atomic(Arc::clone(core)),
                Obj::Executor(core) => WireValue::Executor(Arc::clone(core)),
                // D6: a socket/listener handle crosses as its shared `Arc` core (a spawned fiber
                // reaches the same fd) — same shape as `Channel`/`Shared`/`Executor`.
                Obj::Socket(core) => WireValue::Socket(Arc::clone(core)),
                Obj::Listener(core) => WireValue::Listener(Arc::clone(core)),
                // An opaque `ptr` handle crosses by value — its raw address is heap-independent, so a
                // fresh `Obj::Ptr` on the other side is observationally identical (immutable +
                // value-compared). Cross-safe for both the serial and M:N engines.
                Obj::Ptr(a) => WireValue::Ptr(*a),
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
                    // Positional layout: recover the declaration-order field names from the
                    // StructDef (cold cross-task path) so the WireValue encoding is unchanged.
                    let name = name.clone();
                    let names: Vec<Box<str>> = self
                        .program
                        .structs
                        .get(name.as_ref())
                        .map(|d| d.fields.iter().map(|f| f.clone().into_boxed_str()).collect())
                        .unwrap_or_default();
                    let vals: Vec<Value> = fields.clone();
                    let mut out = Vec::with_capacity(vals.len());
                    for (i, val) in vals.iter().enumerate() {
                        let k = names.get(i).cloned().unwrap_or_else(|| i.to_string().into_boxed_str());
                        out.push((k, self.to_wire(*val)?));
                    }
                    WireValue::Struct { name, fields: out }
                }
                Obj::Enum { variant_id, payload } => {
                    let mut out = Vec::with_capacity(payload.len());
                    for x in payload {
                        out.push(self.to_wire(*x)?);
                    }
                    // M19 lever #2 — carry the dense `variant_id` directly on the COLD wire path. All
                    // workers share one `Arc<Program>`, so the id is meaningful on both sides; carrying
                    // it (not the name) preserves native-vs-user identity under variant-name shadowing.
                    WireValue::Enum { variant_id: *variant_id, payload: out }
                }
                // A newtype crosses by value (deep copy), like a 1-field struct: carry its key + the
                // wired inner. Sendable iff its inner is (the checker's `sendable_rec` agrees).
                Obj::NewType { type_key, inner } => WireValue::NewType {
                    type_key: type_key.clone(),
                    inner: Box::new(self.to_wire(*inner)?),
                },
                // Experimental generators cannot cross an OS-thread heap boundary — their parked
                // frames reference this heap. The checker marks them non-sendable, so this is a
                // defensive fault (an erased path that smuggled one into a `spawn`/channel).
                Obj::Generator(_) => {
                    return Err(self.err("a generator cannot be sent across tasks".to_string(), Span { line: 0, col: 0 }));
                }
                // A cursor crosses by value as a DEEP COPY (like `List`): wire each snapshot item and
                // carry `pos`. It is plain data (a `Vec` + index), so — unlike a generator — it is
                // genuinely sendable, and `from_wire` rebuilds an independent cursor on the other side.
                // This matches the interpreter, whose `deep_clone` already deep-copies a cursor across
                // the airlock; gating it here (the old behavior) diverged VM from interp. Recursing
                // through items means a cursor over a non-sendable element faults recoverably, like a
                // `list` of that element would.
                Obj::Iter { items, pos } => {
                    let pos = *pos;
                    let items = items.clone();
                    let mut out = Vec::with_capacity(items.len());
                    for x in items {
                        out.push(self.to_wire(x)?);
                    }
                    WireValue::Iter { items: out, pos }
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
            // Rebuild a fresh heap `bytes` from the owned raw bytes (by value, like `str`).
            WireValue::Bytes(b) => Value::Obj(self.heap.alloc(Obj::Bytes(b))),
            // Rebuild a FRESH, independent heap `bytearray` from the owned raw bytes (deep copy across
            // the airlock, like `list`) — the other side never shares this VM's buffer.
            WireValue::ByteArray(b) => Value::Obj(self.heap.alloc(Obj::ByteArray(b.into_vec()))),
            WireValue::Handle(h) => Value::Obj(h),
            // B3.1: rebuild a fresh heap handle onto the SAME shared core (`Arc` already cloned in
            // `to_wire`). Not registered in `self.executors` — the original `NewExecutor` handle there
            // drives the program-exit auto-drain and shares this core, so the alias needs no entry.
            WireValue::Channel(core) => Value::Obj(self.heap.alloc(Obj::Channel(core))),
            WireValue::Shared(core) => Value::Obj(self.heap.alloc(Obj::Shared(core))),
            WireValue::RwShared(core) => Value::Obj(self.heap.alloc(Obj::RwShared(core))),
            WireValue::Atomic(core) => Value::Obj(self.heap.alloc(Obj::Atomic(core))),
            WireValue::Executor(core) => Value::Obj(self.heap.alloc(Obj::Executor(core))),
            // D6: rebuild a fresh heap handle onto the SAME shared socket/listener core (`Arc` cloned
            // in `to_wire`) — two fibers reach one fd.
            WireValue::Socket(core) => Value::Obj(self.heap.alloc(Obj::Socket(core))),
            WireValue::Listener(core) => Value::Obj(self.heap.alloc(Obj::Listener(core))),
            // Rebuild a fresh `Obj::Ptr` from the raw address carried by value (heap-independent).
            WireValue::Ptr(a) => Value::Obj(self.heap.alloc(Obj::Ptr(a))),
            // Re-alloc a fresh `Obj::Builtin` from the name carried by value (pure code, no state).
            WireValue::Builtin(name) => Value::Obj(self.heap.alloc(Obj::Builtin(name))),
            WireValue::List(items) => {
                let cloned: Vec<Value> = items.into_iter().map(|x| self.from_wire(x)).collect();
                Value::Obj(self.heap.alloc(Obj::List(cloned)))
            }
            WireValue::Tuple(items) => {
                let cloned: Vec<Value> = items.into_iter().map(|x| self.from_wire(x)).collect();
                Value::Obj(self.heap.alloc(Obj::Tuple(cloned)))
            }
            WireValue::Iter { items, pos } => {
                let cloned: Vec<Value> = items.into_iter().map(|x| self.from_wire(x)).collect();
                Value::Obj(self.heap.alloc(Obj::Iter { items: cloned, pos }))
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
                // Positional layout: the wire fields arrive in declaration order (to_wire emits
                // them so), so rebuild positionally — the carried names are discarded.
                let cloned: Vec<Value> = fields
                    .into_iter()
                    .map(|(_, val)| self.from_wire(val))
                    .collect();
                let tid = self.struct_tid(&name);
                Value::Obj(self.heap.alloc(Obj::Struct {
                    name,
                    tid,
                    fields: cloned,
                }))
            }
            WireValue::Enum {
                variant_id,
                payload,
            } => {
                let cloned: Vec<Value> = payload.into_iter().map(|x| self.from_wire(x)).collect();
                // M19 lever #2 — the dense `variant_id` crossed the airlock directly (shared
                // `Arc<Program>`), so it is replayed as-is — no lossy name re-resolution.
                Value::Obj(self.heap.alloc(Obj::Enum {
                    variant_id,
                    payload: cloned,
                }))
            }
            WireValue::NewType { type_key, inner } => {
                let inner = self.from_wire(*inner);
                Value::Obj(self.heap.alloc(Obj::NewType { type_key, inner }))
            }
            // B3.6: rebuild a submitted closure by value over the worker's reconstructed home module
            // (the `proto` is shared via `Arc<Program>`; captures reconstruct bottom-up into this heap).
            // `worker_home` resolves the home index against this VM's `module_objs` (the rebuilt graph
            // in a pool worker, or the live graph in a cooperative same-heap drain).
            WireValue::Closure {
                proto,
                captured,
                home,
            } => {
                // Lever #3: rebuild positionally — push values in wire (slot) order, discard the
                // carried names (they live in `proto.capture_names`). `to_wire` emits in slot order.
                let cap: Vec<Value> = captured
                    .into_iter()
                    .map(|(_k, w)| self.from_wire(w))
                    .collect();
                let home = self.worker_home(home);
                Value::Obj(self.heap.alloc(Obj::Closure {
                    proto,
                    captured: cap,
                    home,
                }))
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
                        Obj::Closure {
                            proto,
                            captured,
                            home,
                        } => {
                            // Lever #3: captures are positional; carry names from the proto in slot
                            // order so the wire format (Vec<(name, value)>) is unchanged.
                            let names = self.program.protos[proto].capture_names.clone();
                            let mut wcap = Vec::with_capacity(captured.len());
                            for (i, v) in captured.into_iter().enumerate() {
                                let w = self.to_wire_at(v, span)?;
                                self.ensure_crossable(&w, span)?;
                                let name = names.get(i).cloned().unwrap_or_default();
                                wcap.push((name, w));
                            }
                            Lowered::Closure {
                                proto,
                                captured: wcap,
                                args: wargs,
                                home: self.home_index(home),
                                span,
                            }
                        }
                        Obj::Func { proto, home } => Lowered::Func {
                            proto,
                            args: wargs,
                            home: self.home_index(home),
                            span,
                        },
                        // A first-class builtin fn value (`f := ord; spawn f(x)`) is pure code — cross
                        // it by name; the worker re-allocs a fresh `Obj::Builtin`. Mirrors `Func`.
                        Obj::Builtin(name) => Lowered::Builtin {
                            name,
                            args: wargs,
                            span,
                        },
                        _ => {
                            return Err(self.err(
                                format!(
                                    "spawn: '{}' is not an isolable task",
                                    self.type_name(callee)
                                ),
                                span,
                            ));
                        }
                    },
                    _ => {
                        return Err(self.err(
                            format!(
                                "spawn: '{}' is not an isolable task",
                                self.type_name(callee)
                            ),
                            span,
                        ));
                    }
                }
            }
            // B3.3d: the receiver + args cross by wire; dispatch resolves against the worker's
            // reconstructed `module_objs` (built below). `ensure_crossable` keeps a non-sendable
            // receiver (e.g. a closure) from silently dangling.
            PendingCall::Method {
                recv,
                name,
                args,
                span,
            } => {
                let wrecv = self.to_wire_at(recv, span)?;
                self.ensure_crossable(&wrecv, span)?;
                let wargs = self.wire_args(args, span)?;
                Lowered::Method {
                    recv: wrecv,
                    name,
                    args: wargs,
                    span,
                }
            }
        };

        // 2. Build the worker + install the shared read-only module snapshot (D1): pre-alloc empty
        //    module objs (indices line up with the parent), faulting each module's globals into the
        //    worker heap lazily on first access — instead of eagerly reconstructing the whole graph
        //    per task. 3. rebuild the callable/receiver + args into the worker heap (a `home` index
        //    resolves to a pre-alloced empty module that faults on first global read). The actual
        //    invoke is `ReadyWorker::run`.
        let snap = self.ensure_snapshot(lowered.span())?;
        let mut worker = self.spawn_worker();
        worker.install_snapshot(snap);
        let (call, span) = match lowered {
            Lowered::Closure {
                proto,
                captured,
                args,
                home,
                span,
            } => {
                let home = worker.worker_home(home);
                // Lever #3: rebuild positionally (slot order), discarding the carried names.
                let cap: Vec<Value> = captured
                    .into_iter()
                    .map(|(_k, w)| worker.from_wire(w))
                    .collect();
                let callee = Value::Obj(worker.heap.alloc(Obj::Closure {
                    proto,
                    captured: cap,
                    home,
                }));
                let args = args.into_iter().map(|w| worker.from_wire(w)).collect();
                (ReadyCall::Invoke { callee, args }, span)
            }
            Lowered::Func {
                proto,
                args,
                home,
                span,
            } => {
                let home = worker.worker_home(home);
                let callee = Value::Obj(worker.heap.alloc(Obj::Func { proto, home }));
                let args = args.into_iter().map(|w| worker.from_wire(w)).collect();
                (ReadyCall::Invoke { callee, args }, span)
            }
            Lowered::Builtin { name, args, span } => {
                let callee = Value::Obj(worker.heap.alloc(Obj::Builtin(name)));
                let args = args.into_iter().map(|w| worker.from_wire(w)).collect();
                (ReadyCall::Invoke { callee, args }, span)
            }
            Lowered::Method {
                recv,
                name,
                args,
                span,
            } => {
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
    /// a zero-arg call. The submitted closure already crossed `to_wire`/`ensure_crossable` at `submit`,
    /// but `ensure_snapshot` can fault if a module global is a frame-holding generator — so this
    /// forwards that snapshot fault (re-stamped with `span`) rather than panicking. `--parallel` only.
    fn prepare_worker_from_wire(
        &mut self,
        task: WireValue,
        span: Span,
    ) -> Result<ReadyWorker, RuntimeError> {
        let snap = self.ensure_snapshot(span)?;
        let mut worker = self.spawn_worker();
        worker.install_snapshot(snap);
        let callee = worker.from_wire(task);
        Ok(ReadyWorker {
            worker,
            call: ReadyCall::Invoke {
                callee,
                args: Vec::new(),
            },
            span,
        })
    }

    /// B3.6 — drain a shut `Executor`'s pending tasks onto the bounded pool under `--parallel`. Each
    /// queued closure becomes a [`ReadyWorker`] sharing a fresh per-drain cancel flag (first fault
    /// aborts siblings, matching the cooperative inline `r?`); **no** deadlock watch (decision D — an
    /// `Executor`-spanning deadlock hangs, as documented). Output is flushed in submission (queue) order
    /// by [`run_workers_on_pool`] (decision F).
    fn drain_executor_on_pool(
        &mut self,
        tasks: Vec<WireValue>,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if tasks.is_empty() {
            return Ok(());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let mut ready = Vec::with_capacity(tasks.len());
        for t in tasks {
            let mut rw = self.prepare_worker_from_wire(t, span)?;
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
                // `to_wire_at` re-stamps a generator's placeholder span with this call site's `span`.
                let w = self.to_wire_at(a, span)?;
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
                Obj::Closure {
                    proto,
                    captured,
                    home,
                } => {
                    // Lever #3: positional captures — carry names from the proto in slot order.
                    let names = &self.program.protos[*proto].capture_names;
                    let mut wcap = Vec::with_capacity(captured.len());
                    for (i, cv) in captured.iter().enumerate() {
                        // `to_wire_at` re-stamps a generator capture's placeholder span with `span`.
                        let w = self.to_wire_at(*cv, span)?;
                        self.ensure_crossable(&w, span)?;
                        let name = names.get(i).cloned().unwrap_or_default();
                        wcap.push((name.into_boxed_str(), w));
                    }
                    return Ok(WireValue::Closure {
                        proto: *proto,
                        captured: wcap,
                        home: self.home_index(*home),
                    });
                }
                Obj::Func { proto, home } => {
                    return Ok(WireValue::Closure {
                        proto: *proto,
                        captured: Vec::new(),
                        home: self.home_index(*home),
                    });
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
        self.heap.alloc(Obj::Module {
            name: "<worker>".into(),
            slots: Vec::new(),
            index: Default::default(),
        })
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
    ///
    /// Fallible: a module global that is a frame-holding generator cannot be snapshotted (its parked
    /// frames reference the parent heap). `to_wire`/`to_snap` stamp the airlock fault with a
    /// placeholder span, so this choke point RE-STAMPS it with the real nursery/spawn-site `span`
    /// (the caller has it) — a graceful, catchable error instead of a panic. The build path memoizes
    /// ONLY on success: a module-global generator fails deterministically every call (the program is
    /// rejected at its first nursery, so it never loops), which sidesteps caching a stale error.
    fn ensure_snapshot(&mut self, span: Span) -> Result<Arc<ModuleSnapshot>, RuntimeError> {
        if let Some(s) = &self.module_snapshot {
            return Ok(Arc::clone(s));
        }
        if let Some(s) = &self.snapshot_memo {
            return Ok(Arc::clone(s));
        }
        let snap = Arc::new(
            self.snapshot_modules()
                .map_err(|e| self.err(e.message, span))?,
        );
        self.snapshot_memo = Some(Arc::clone(&snap));
        Ok(snap)
    }

    /// D1 — read this VM's initialized module graph (read-only) into a heap-independent
    /// [`ModuleSnapshot`]: one [`ModuleSnap`] per module in `module_objs` order (so a callable's home
    /// index lines up with a worker's pre-alloced modules), each global lowered by [`Vm::to_snap`].
    /// Replaces the eager per-task `build_worker_modules` reconstruction — built once, replayed lazily.
    fn snapshot_modules(&self) -> Result<ModuleSnapshot, RuntimeError> {
        let mut modules = Vec::with_capacity(self.module_objs.len());
        for &pm in &self.module_objs {
            // M19 Phase 2b — collect globals in *slot order* (not HashMap iteration order) so a
            // worker replays them into matching slots; the shared `Arc<Program>` slot map makes
            // parent and worker agree on slot↔name regardless of any hash ordering.
            let (name, globals): (Box<str>, Vec<(String, Value)>) = match self.heap.get(pm) {
                Obj::Module { name, slots, index } => {
                    (name.clone(), module_slot_pairs(slots, index))
                }
                _ => ("<worker>".into(), Vec::new()),
            };
            // Fallible: a module global that is a frame-holding generator faults here (graceful,
            // re-stamped with the nursery span by `ensure_snapshot`) instead of panicking in `to_snap`.
            let mut snapped = Vec::with_capacity(globals.len());
            for (k, v) in globals {
                snapped.push((k, self.to_snap(v)?));
            }
            modules.push(ModuleSnap {
                name,
                globals: snapped,
            });
        }
        Ok(ModuleSnapshot { modules })
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
    ///
    /// Fallible: a frame-holding generator (`Obj::Generator`) is NOT sendable — its parked frames
    /// reference this heap. Reaching that arm (smuggled past the permissive `Iterator[T]` checker
    /// branch — a cursor and a generator share that existential type) returns a graceful airlock
    /// error (re-stamped with the real nursery span by `ensure_snapshot`) instead of a panic. The
    /// recursion propagates that error with `?`, so a generator nested in any container faults too.
    fn to_snap(&self, v: Value) -> Result<SnapValue, RuntimeError> {
        let h = match v {
            Value::Obj(h) => h,
            // A scalar (Int/Float/Bool/Nil) is always sendable — never `Obj::Generator`, so this
            // `.expect` is unreachable for a generator.
            scalar => {
                return Ok(SnapValue::Wire(
                    self.to_wire(scalar).expect("scalar is always sendable"),
                ));
            }
        };
        // Fast path: no embedded callable/module → the wire form is exact and cheap. A generator's
        // `to_wire` errors (so the `if let Ok` fails) — we fall through to the `Obj::Generator` arm,
        // which re-produces the same graceful error below.
        if let Ok(w) = self.to_wire(v)
            && !w.has_handle()
        {
            return Ok(SnapValue::Wire(w));
        }
        Ok(match self.heap.get(h).clone() {
            Obj::Func { proto, home } => SnapValue::Func { proto, home: self.home_index(home) },
            Obj::Closure { proto, captured, home } => {
                // Lever #3: positional captures — carry names from the proto in slot order.
                let names = &self.program.protos[proto].capture_names;
                let mut snapped = Vec::with_capacity(captured.len());
                for (i, cv) in captured.iter().enumerate() {
                    snapped.push((names.get(i).cloned().unwrap_or_default(), self.to_snap(*cv)?));
                }
                SnapValue::Closure { proto, captured: snapped, home: self.home_index(home) }
            }
            // An import alias bound to another module obj.
            Obj::Module { name, slots, index } => match self.home_index(h) {
                Some(idx) => SnapValue::ModuleAlias(idx),
                // A module not in `module_objs` (shouldn't occur for a bound import) — encode inline,
                // in slot order so replay rebuilds matching slots.
                None => {
                    let mut globals = Vec::new();
                    for (k, mv) in module_slot_pairs(&slots, &index) {
                        globals.push((k, self.to_snap(mv)?));
                    }
                    SnapValue::ModuleInline { name, globals }
                }
            },
            Obj::Native { name, func } => SnapValue::Native { name, func },
            // A first-class builtin fn is pure code — SENDABLE. Carry the name; the worker re-allocs
            // a fresh `Obj::Builtin` on replay (like `Native`, but with no fn pointer to share).
            Obj::Builtin(name) => SnapValue::Builtin(name),
            // A Cffi shares its `Arc` to the worker (which shares the parent address space): the
            // worker re-allocs `Obj::Cffi` from the SAME Arc — no re-dlopen, no symbol re-resolution.
            Obj::Cffi(c) => SnapValue::Cffi(Arc::clone(&c)),
            // Containers embedding a callable: encode each element. (Pure-data containers took the fast
            // path above.) A generator embedded in any of these faults via the recursive `?`.
            Obj::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for x in &items {
                    out.push(self.to_snap(*x)?);
                }
                SnapValue::List(out)
            }
            Obj::Tuple(items) => {
                let mut out = Vec::with_capacity(items.len());
                for x in &items {
                    out.push(self.to_snap(*x)?);
                }
                SnapValue::Tuple(out)
            }
            Obj::Struct { name, fields, .. } => {
                // Positional layout: recover declaration-order field names from the StructDef so
                // the SnapValue encoding (which carries names) is unchanged (cold cross-task path).
                let names: Vec<Box<str>> = self
                    .program
                    .structs
                    .get(name.as_ref())
                    .map(|d| d.fields.iter().map(|f| f.clone().into_boxed_str()).collect())
                    .unwrap_or_default();
                let mut out = Vec::with_capacity(fields.len());
                for (i, fv) in fields.iter().enumerate() {
                    let k = names.get(i).cloned().unwrap_or_else(|| i.to_string().into_boxed_str());
                    out.push((k, self.to_snap(*fv)?));
                }
                SnapValue::Struct { name, fields: out }
            }
            Obj::Enum { variant_id, payload } => {
                let mut out = Vec::with_capacity(payload.len());
                for x in &payload {
                    out.push(self.to_snap(*x)?);
                }
                // M19 lever #2 — carry the dense `variant_id` directly on the cold snap path (mirrors
                // `to_wire`); replay reuses it as-is against the shared program.
                SnapValue::Enum { variant_id, payload: out }
            }
            Obj::NewType { type_key, inner } => SnapValue::NewType {
                type_key,
                inner: Box::new(self.to_snap(inner)?),
            },
            Obj::Map(m) => {
                let mut out = Vec::with_capacity(m.entries.len());
                for (hash, k, val) in &m.entries {
                    out.push((*hash, self.to_snap(*k)?, self.to_snap(*val)?));
                }
                SnapValue::Map(out)
            }
            Obj::Set(s) => {
                let mut out = Vec::with_capacity(s.entries.len());
                for (hash, e) in &s.entries {
                    out.push((*hash, self.to_snap(*e)?));
                }
                SnapValue::Set(out)
            }
            // Leaf data / cores are handled by the fast path; if `to_wire` ever errored above we land
            // here for a `str`/core (always sendable) — encode its wire form. A generator is
            // `Obj::Generator` (handled below), never one of these, so this `.expect` is unreachable
            // for a generator.
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            // An opaque `ptr` is always sendable (crosses by value as `WireValue::Ptr`); normally the
            // fast path catches it, but it is a valid leaf here too.
            | Obj::Ptr(_) => {
                SnapValue::Wire(self.to_wire(v).expect("str / bytes / bytearray / channel / shared / atomic / executor / socket / ptr is always sendable"))
            }
            // A frame-holding generator is not sendable and is never legal as a module global crossing
            // to an M:N worker. Reaching here means one was smuggled past the permissive `Iterator[T]`
            // checker branch (a cursor and a generator share that existential type). Fault gracefully
            // with the same message as `to_wire`'s Generator arm — `ensure_snapshot` re-stamps the
            // placeholder span with the real nursery/spawn site.
            Obj::Generator(_) => {
                return Err(self.err("a generator cannot be sent across tasks".to_string(), Span { line: 0, col: 0 }));
            }
            // A cursor snapshots like a `List`: its items (recursively snapped) + `pos`. Only a
            // handle-bearing cursor reaches here; a pure-data cursor took the `to_wire` fast path.
            Obj::Iter { items, pos } => {
                let mut out = Vec::with_capacity(items.len());
                for x in &items {
                    out.push(self.to_snap(*x)?);
                }
                SnapValue::Iter { items: out, pos }
            }
        })
    }

    /// D1 — install a shared [`ModuleSnapshot`] into a freshly-built worker: pre-alloc one **empty**
    /// `Module` per snapshot entry (index order preserved so a callable's home index lines up), seed
    /// the per-module faulted flags, and keep the `Arc` so each module's globals fault in lazily on
    /// first access ([`Vm::fault_module`]). The cheap replacement for eager `build_worker_modules`.
    fn install_snapshot(&mut self, snap: Arc<ModuleSnapshot>) {
        debug_assert!(
            self.module_objs.is_empty(),
            "install_snapshot expects a fresh worker"
        );
        for m in &snap.modules {
            let wm = self.heap.alloc(Obj::Module {
                name: m.name.clone(),
                slots: Vec::new(),
                index: std::collections::HashMap::new(),
            });
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
        let snap = Arc::clone(
            self.module_snapshot
                .as_ref()
                .expect("worker has a snapshot"),
        );
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
                Value::Obj(self.heap.alloc(Obj::Func {
                    proto: *proto,
                    home: whome,
                }))
            }
            SnapValue::Closure {
                proto,
                captured,
                home,
            } => {
                let whome = self.worker_home(*home);
                // Lever #3: rebuild positionally (slot order), discarding the carried names.
                let cap: Vec<Value> = captured
                    .iter()
                    .map(|(_k, cv)| self.replay_snap(cv))
                    .collect();
                Value::Obj(self.heap.alloc(Obj::Closure {
                    proto: *proto,
                    captured: cap,
                    home: whome,
                }))
            }
            SnapValue::ModuleAlias(idx) => Value::Obj(self.module_objs[*idx]),
            SnapValue::ModuleInline { name, globals } => {
                let wm = self.heap.alloc(Obj::Module {
                    name: name.clone(),
                    slots: Vec::new(),
                    index: std::collections::HashMap::new(),
                });
                for (k, gv) in globals {
                    let val = self.replay_snap(gv);
                    self.module_define(wm, k, val);
                }
                Value::Obj(wm)
            }
            SnapValue::Native { name, func } => Value::Obj(self.heap.alloc(Obj::Native {
                name: name.clone(),
                func: *func,
            })),
            // Re-alloc a fresh `Obj::Builtin` from the carried name (pure code, no state to share).
            SnapValue::Builtin(name) => Value::Obj(self.heap.alloc(Obj::Builtin(name.clone()))),
            // Re-alloc from the SAME shared `Arc<Cffi>` — no re-dlopen (shared address space).
            SnapValue::Cffi(c) => Value::Obj(self.heap.alloc(Obj::Cffi(Arc::clone(c)))),
            SnapValue::List(xs) => {
                let v = xs.iter().map(|x| self.replay_snap(x)).collect();
                Value::Obj(self.heap.alloc(Obj::List(v)))
            }
            SnapValue::Iter { items, pos } => {
                let v = items.iter().map(|x| self.replay_snap(x)).collect();
                Value::Obj(self.heap.alloc(Obj::Iter {
                    items: v,
                    pos: *pos,
                }))
            }
            SnapValue::Tuple(xs) => {
                let v = xs.iter().map(|x| self.replay_snap(x)).collect();
                Value::Obj(self.heap.alloc(Obj::Tuple(v)))
            }
            SnapValue::Struct { name, fields } => {
                // Positional layout: the snap fields are in declaration order (to_snap emits them
                // so), so rebuild positionally — the carried names are discarded.
                let f = fields.iter().map(|(_, fv)| self.replay_snap(fv)).collect();
                let tid = self.struct_tid(name);
                Value::Obj(self.heap.alloc(Obj::Struct {
                    name: name.clone(),
                    tid,
                    fields: f,
                }))
            }
            SnapValue::Enum {
                variant_id,
                payload,
            } => {
                let p = payload.iter().map(|x| self.replay_snap(x)).collect();
                // M19 lever #2 — the dense `variant_id` was carried directly (mirrors `from_wire`);
                // replay it as-is against the shared program — no lossy name re-resolution.
                Value::Obj(self.heap.alloc(Obj::Enum {
                    variant_id: *variant_id,
                    payload: p,
                }))
            }
            SnapValue::NewType { type_key, inner } => {
                let inner = self.replay_snap(inner);
                Value::Obj(self.heap.alloc(Obj::NewType {
                    type_key: type_key.clone(),
                    inner,
                }))
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

    fn rwshared_core(&self, h: GcRef) -> Arc<RwSharedCore> {
        match self.heap.get(h) {
            Obj::RwShared(core) => Arc::clone(core),
            _ => unreachable!("rwshared_core on non-rwshared"),
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
    fn new_atomic(&mut self, span: Span) -> Result<Value, RuntimeError> {
        let init = self.pop();
        // A non-sendable init (a frame-holding generator) faults gracefully with the `NewAtomic` span.
        let init = self.to_wire_at(init, span)?;
        Ok(Value::Obj(self.heap.alloc(Obj::Atomic(Arc::new(
            AtomicCore {
                v: Mutex::new(init),
            },
        )))))
    }

    /// `timer(ms)` — pop the `ms` int, push a fresh `Channel[bool]` stamped with `now + ms`. Delivery is
    /// handled at `recv` time (in the receiver's scheduler), NOT here, so a timer made at the top level
    /// can be `recv`'d inside a `--parallel` child. `#[inline(never)]` so the `Instant`/`Duration` math
    /// stays out of `step`'s (recursion-path) stack frame.
    #[inline(never)]
    fn new_timer(&mut self, span: Span) -> Result<Value, RuntimeError> {
        let ms = match self.pop() {
            Value::Int(ms) => ms.max(0) as u64,
            other => {
                return Err(self.err(
                    format!("timer(ms) expects int, got {}", self.type_name(other)),
                    span,
                ));
            }
        };
        // Saturate a pathological `ms` to a far-future deadline rather than panic on `Instant` overflow
        // (mirrors the `sleep_ms` offload path).
        let deadline = std::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(ms))
            .unwrap_or_else(|| {
                std::time::Instant::now() + std::time::Duration::from_secs(86_400 * 365)
            });
        let core = Arc::new(ChannelCore {
            timer: Some(deadline),
            ..Default::default()
        });
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
    fn net_connect_or_listen(
        &mut self,
        name: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let addr = match args.first() {
            Some(Value::Obj(h)) => match self.heap.get(*h) {
                Obj::Str(s) => s.to_string(),
                _ => {
                    return Err(self.err(format!("std.net.{name} expects an address string"), span));
                }
            },
            _ => return Err(self.err(format!("std.net.{name} expects an address string"), span)),
        };
        match name {
            "connect" => match crate::native::net::connect_nonblocking(&addr) {
                // Connected synchronously (the common loopback case) — wrap + return at once.
                Ok((stream, false)) => {
                    Ok(self.alloc_socket_ok(stream, core::next_poll_key(), core::new_in_flight()))
                }
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
                        Ok(self.sock_err(
                            "connect would block: std.net sockets require the --parallel engine",
                        ))
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
                    let core = Arc::new(ListenerCore {
                        listener: Mutex::new(Some(listener)),
                        key: core::next_poll_key(),
                        in_flight: core::new_in_flight(),
                    });
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
    fn alloc_socket_ok(
        &mut self,
        stream: std::net::TcpStream,
        key: usize,
        in_flight: Arc<AtomicBool>,
    ) -> Value {
        let core = Arc::new(SocketCore {
            stream: Mutex::new(Some(stream)),
            key,
            in_flight,
        });
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
        self.pending_connect = Some(ConnectInProgress {
            stream,
            key,
            in_flight: Arc::clone(&in_flight),
        });
        // A `connect` never carries a user timeout (the `connect` surface takes only an address); it
        // parks forever (or until `drain_sched` re-injects it on a sibling fault).
        self.poll_park = Some(PollPark {
            key,
            fd,
            interest: poller::Interest::Write,
            in_flight,
            deadline: None,
        });
    }

    /// D6b — the top-level connect fallback (no fiber to park): block until the handshake settles, then
    /// return `Ok(Socket)` / `Err`. Bounded by a wall-clock deadline so a black-hole address (no RST,
    /// no SYN-ACK — `SO_ERROR` never sets, the fd never becomes writable) returns a clean timeout
    /// instead of spinning for the kernel's multi-minute connect timeout. net targets the M:N
    /// `--parallel` engine, so this path exists only to keep a top-level `net.connect` usable.
    fn block_until_connected(&mut self, stream: std::net::TcpStream) -> Value {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(CONNECT_BLOCK_TIMEOUT_SECS);
        loop {
            match crate::native::net::finish_connect(&stream) {
                // SO_ERROR clear AND the peer is reachable ⇒ connected.
                Ok(()) if stream.peer_addr().is_ok() => {
                    return self.alloc_socket_ok(
                        stream,
                        core::next_poll_key(),
                        core::new_in_flight(),
                    );
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
    fn socket_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
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
                        let target = PollPark {
                            key: core.key,
                            fd,
                            interest: poller::Interest::Read,
                            in_flight: Arc::clone(&core.in_flight),
                            deadline: timeout.map(|t| t.deadline),
                        };
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
                            return self.demote_block_socket(
                                fd,
                                poller::Interest::Read,
                                span,
                                move |vm| {
                                    let mut b = vec![0u8; n];
                                    let r = {
                                        let mut guard =
                                            core.stream.lock().unwrap_or_else(|e| e.into_inner());
                                        let Some(stream) = guard.as_mut() else {
                                            return SockPoll::Ready(Ok(
                                                vm.sock_err("read on a closed socket")
                                            ));
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
                                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                            SockPoll::WouldBlock
                                        }
                                        Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                                    }
                                },
                            );
                        }
                        Ok(self.sock_err(
                            "read would block: std.net sockets require the --parallel engine",
                        ))
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
                        let target = PollPark {
                            key: core.key,
                            fd,
                            interest: poller::Interest::Write,
                            in_flight: Arc::clone(&core.in_flight),
                            deadline: timeout.map(|t| t.deadline),
                        };
                        if self.park_on_fd(h, args, target, span)? {
                            return Ok(Value::Nil);
                        }
                        // In-callback on M:N → demote + backoff-poll the non-blocking write (#3 socket half).
                        if self.mn.is_some() && self.native_reentry > 0 {
                            let core = Arc::clone(&core);
                            return self.demote_block_socket(
                                fd,
                                poller::Interest::Write,
                                span,
                                move |vm| {
                                    let r = {
                                        let mut guard =
                                            core.stream.lock().unwrap_or_else(|e| e.into_inner());
                                        let Some(stream) = guard.as_mut() else {
                                            return SockPoll::Ready(Ok(
                                                vm.sock_err("write on a closed socket")
                                            ));
                                        };
                                        std::io::Write::write(stream, &data)
                                    };
                                    match r {
                                        Ok(got) => {
                                            SockPoll::Ready(Ok(vm.sock_ok(Value::Int(got as i64))))
                                        }
                                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                            SockPoll::WouldBlock
                                        }
                                        Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                                    }
                                },
                            );
                        }
                        Ok(self.sock_err(
                            "write would block: std.net sockets require the --parallel engine",
                        ))
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
    fn listener_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
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
                        let target = PollPark {
                            key: core.key,
                            fd,
                            interest: poller::Interest::Read,
                            in_flight: Arc::clone(&core.in_flight),
                            deadline: timeout.map(|t| t.deadline),
                        };
                        if self.park_on_fd(h, args, target, span)? {
                            return Ok(Value::Nil);
                        }
                        // In-callback on M:N → demote + backoff-poll the non-blocking accept (#3 socket half).
                        if self.mn.is_some() && self.native_reentry > 0 {
                            let core = Arc::clone(&core);
                            return self.demote_block_socket(
                                fd,
                                poller::Interest::Read,
                                span,
                                move |vm| {
                                    let r = {
                                        let guard =
                                            core.listener.lock().unwrap_or_else(|e| e.into_inner());
                                        let Some(listener) = guard.as_ref() else {
                                            return SockPoll::Ready(Ok(
                                                vm.sock_err("accept on a closed listener")
                                            ));
                                        };
                                        listener.accept()
                                    };
                                    match r {
                                        Ok((stream, _peer)) => {
                                            SockPoll::Ready(Ok(vm.accept_socket_value(stream)))
                                        }
                                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                            SockPoll::WouldBlock
                                        }
                                        Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                                    }
                                },
                            );
                        }
                        Ok(self.sock_err(
                            "accept would block: std.net sockets require the --parallel engine",
                        ))
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
                        Some(l) => l
                            .local_addr()
                            .map(|a| a.to_string())
                            .map_err(|e| e.to_string()),
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
        let core = Arc::new(SocketCore {
            stream: Mutex::new(Some(stream)),
            key: core::next_poll_key(),
            in_flight: core::new_in_flight(),
        });
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
    fn park_on_fd(
        &mut self,
        h: GcRef,
        args: &[Value],
        target: PollPark,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        if self.mn.is_some() && self.native_reentry == 0 {
            // The `in_flight` guard: at most one op may be parked on a socket at a time. A second
            // concurrent op on a shared socket (`Arc`) faults rather than overwrite the registry entry
            // (which would drop the first fiber + leak `inflight`) or double-`add` the fd (EEXIST panic).
            if target.in_flight.swap(true, Ordering::AcqRel) {
                return Err(self.err(
                    "concurrent operation on a shared socket is not supported".into(),
                    span,
                ));
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
    fn channel_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "send" => {
                self.arity_err("send", args, 1, span)?;
                // B3.1: serialize once into the core (the wire form IS the airlock copy). A
                // non-sendable value (a frame-holding generator) faults gracefully with `send`'s span.
                let w = self.to_wire_at(args[0], span)?;
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
                let w = self.to_wire_at(args[0], span)?;
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
                if self.mn.is_some()
                    && self.native_reentry > 0
                    && self.channel_core(h).timer.is_none()
                {
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
                // A tripped latch (`trip()`) reports ready forever, like a passed timer deadline.
                let popped = popped.or_else(|| {
                    if core.done_latch.load(Ordering::Relaxed) {
                        return Some(WireValue::Bool(true));
                    }
                    core.timer
                        .filter(|d| std::time::Instant::now() >= *d)
                        .map(|_| WireValue::Bool(true))
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
                // Same routing as `channel_send_wire`: an inline outermost-`parallel:` builder VM
                // (`self.mn == None`) closing a channel must wake enlisted, parked receivers via the
                // held `mn_enlist_sched`, not just the local condvar. (Cross-nursery flat scheduler #2.)
                if let Some(sched) = self.mn.clone().or_else(|| self.mn_enlist_sched.clone()) {
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
            // `trip()` flips the manual level-trigger latch (the primitive behind `std.cancel`'s
            // `done()`): the channel is then permanently ready (`recv`/`try_recv`/`wait` yield `true`).
            // Idempotent. Reuses `close()`'s exact wake fan-out so a parked `recv`/`wait` re-runs and
            // observes the latch — but does NOT set `closed` (a closed+empty `wait` arm is *skipped*;
            // we need it *ready*).
            "trip" => {
                self.arity_err("trip", args, 0, span)?;
                let core = self.channel_core(h);
                core.done_latch.store(true, Ordering::Relaxed);
                if let Some(sched) = self.mn.clone().or_else(|| self.mn_enlist_sched.clone()) {
                    let key = self.channel_core_ptr(h);
                    sched.close_wake(key, &core);
                } else {
                    core.cv.notify_all();
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
        // Route the enqueue+wake through whatever sched is in scope. A worker shell holds it in
        // `self.mn`; the INLINE outermost-`parallel:` builder VM runs with `self.mn == None` but holds
        // the global sched in `self.mn_enlist_sched` while early-enlisted outer scopes are still pending.
        // An inline-body send must still wake an enlisted, parked receiver (the cross-nursery wake), so
        // fall back to the held sched. The sender never parks, so this does not pull the inline owner
        // onto a worker yield/park path. (Cross-nursery flat scheduler — charges #1/#2.)
        if let Some(sched) = self.mn.clone().or_else(|| self.mn_enlist_sched.clone()) {
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
        // A tripped latch (`trip()`) delivers `true` immediately and forever, on every engine — a
        // pending queued value (if any) still wins first. Checked before the timer/park logic so a
        // `done().recv()` on a manually-cancelled token never parks.
        {
            let core = self.channel_core(h);
            if core.done_latch.load(Ordering::Relaxed) {
                if let Some(w) = core.q.lock().unwrap().queue.pop_front() {
                    return Ok(RecvStep::Got(w));
                }
                return Ok(RecvStep::Got(WireValue::Bool(true)));
            }
        }
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
                    if self
                        .cancel
                        .as_ref()
                        .is_some_and(|c| c.load(Ordering::Relaxed))
                    {
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
            if self
                .cancel
                .as_ref()
                .is_some_and(|c| c.load(Ordering::Relaxed))
            {
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

    /// `wait:` runtime (§6d) — execute [`Op::WaitPoll`]. The `n` arm channel handles are on the
    /// operand stack (`stack[base..base+n]`, source order). Poll source order: the first channel with
    /// a queued value (or a fired timer) wins → drop the handles, push the value, jump to that arm's
    /// body. A closed+empty arm is skipped. Nothing ready → run `else` (jump), else inline-sleep to
    /// the soonest live timer and take it, else fault (all-closed) or block (cooperative multi-channel
    /// park; the M:N park is a follow-up — a blocking `wait` faults under `--parallel` for now).
    fn op_wait_poll(&mut self, meta: &WaitMeta, span: Span) -> Result<(), RuntimeError> {
        let n = meta.n;
        let base = self.stack.len() - n;
        let mut soonest: Option<(usize, std::time::Instant)> = None;
        let mut all_closed = true;
        for i in 0..n {
            let Value::Obj(h) = self.stack[base + i] else {
                unreachable!("wait arm operand is not a channel handle");
            };
            let core = self.channel_core(h);
            let (popped, closed) = {
                let mut g = core.q.lock().unwrap();
                (g.queue.pop_front(), g.closed)
            };
            if let Some(w) = popped {
                let v = self.from_wire(w);
                self.take_wait_arm(base, v, meta.arm_targets[i]);
                return Ok(());
            }
            // A tripped latch (`trip()`) is ready like a fired timer — take the arm with `true`.
            if core.done_latch.load(Ordering::Relaxed) {
                self.take_wait_arm(base, Value::Bool(true), meta.arm_targets[i]);
                return Ok(());
            }
            if let Some(deadline) = core.timer {
                // A timer channel is never closed and always eventually ready: fired now → take it;
                // otherwise a live waiter whose deadline we may sleep to below.
                if std::time::Instant::now() >= deadline {
                    self.take_wait_arm(base, Value::Bool(true), meta.arm_targets[i]);
                    return Ok(());
                }
                all_closed = false;
                if soonest.is_none_or(|(_, d)| deadline < d) {
                    soonest = Some((i, deadline));
                }
            } else if !closed {
                all_closed = false;
            }
        }
        // Nothing ready → the non-blocking `else` fallback.
        if let Some(t) = meta.else_target {
            self.stack.truncate(base);
            self.frames.last_mut().unwrap().ip = t;
            return Ok(());
        }
        // Every arm closed+empty, no `else`, no timer → distinct fault. (A live timer arm set
        // `all_closed = false` above, so this fires only when there is genuinely nothing to wait on.)
        if all_closed {
            return Err(self.err("wait: all channels closed".to_string(), span));
        }
        // Block on all live arms. The N arm handles are on the stack (they root the channels + re-supply
        // the poll on resume). A live timer arm (`soonest`) is just another arm bucket on the M:N paths.
        let keys: Vec<GcRef> = (0..n)
            .map(|i| match self.stack[base + i] {
                Value::Obj(h) => h,
                _ => unreachable!("wait arm operand is not a channel handle"),
            })
            .collect();
        // M:N (`--parallel`) snapshot-park, top level: rewind to re-run `WaitPoll` on wake and set
        // `wait_suspend`; the worker loop captures each arm's (key, core) WHILE the fiber heap is live
        // (`Disp::WaitPark`) and `MnSched::park_wait` files ONE shared token in every arm bucket. A
        // `send`/`close` to any arm claims the fiber once and sweeps the rest (lost-wakeup-safe via the
        // park-gap re-check). Mirrors the single-`recv` `park_recv`/`Disp::Park` path, generalized to N.
        if self.mn.is_some() && self.native_reentry == 0 {
            // WAIT-1 fix — a live timer arm is NOT taken by an inline-sleep (which would pin the worker
            // and strand a sibling `send` that lands mid-window). Instead, for the soonest timer arm
            // submit ONE background `send_wake(true)` at its deadline (in THIS scheduler) and fall
            // through to the snapshot-park, so the timer channel parks as an ordinary arm bucket. On
            // wake the re-poll pops a sibling's value (timer NOT taken) OR finds `now >= deadline` and
            // takes the timer arm. The existing `WaitPark` claimed-CAS sweep guarantees exactly one of
            // {a sibling send/close, the timer's own deadline send} wins (WAIT-2 late-alarm = CAS
            // already claimed = no-op; WAIT-3 same-instant = single claimed CAS = one winner).
            if let Some((i, deadline)) = soonest {
                // Cancel checked first: a fiber about to be cancelled must not arm a stray timer (mirror
                // the single-`recv` timer-park at `chan_recv_step`).
                if self
                    .cancel
                    .as_ref()
                    .is_some_and(|c| c.load(Ordering::Relaxed))
                {
                    self.cancelled = true;
                    return Err(self.err("cancelled".to_string(), span));
                }
                let sched = self.mn.clone().unwrap();
                let key = self.channel_core_ptr(keys[i]);
                let core_job = self.channel_core(keys[i]);
                let sched_job = Arc::clone(&sched);
                // Arm ONCE per timer channel: a re-park of this same wait (woken with no consumable
                // value — e.g. a sibling `close` on another arm) re-runs WaitPoll and re-enters this
                // block, but the CAS fails the second time so we do NOT submit a redundant job. The
                // first job survives the re-park (it captures the stable `key`+`core` and wakes
                // whatever token is in this bucket at the deadline). Fresh `timer(ms)` ⇒ fresh core
                // ⇒ `armed=false`, so no reset is needed.
                if core_job
                    .timer_armed
                    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    // Account the pending timer `inflight` (gated STRICTLY on `soonest.is_some()`) so it
                    // vetoes the deadlock predicate while a lone fiber waits; the job un-accounts it.
                    sched.inflight.fetch_add(1, Ordering::Relaxed);
                    timer::submit_at(
                        deadline,
                        Box::new(move || {
                            sched_job.send_wake(key, &core_job, WireValue::Bool(true));
                            sched_job.inflight.fetch_sub(1, Ordering::Relaxed);
                        }),
                    );
                }
            }
            self.frames.last_mut().unwrap().ip -= 1; // re-run this WaitPoll on resume
            self.wait_suspend = Some(keys);
            return Ok(());
        }
        // M:N inside a native callback (`native_reentry > 0`): a host-stack loop frame sits between the
        // worker loop and here, so we cannot snapshot-park. Demote: block this worker in place, polling
        // all N arm queues in source order on a bounded backoff (mirrors `demote_recv_block`). A live
        // timer arm (`soonest`) is threaded in: after the source-order channel scan fails, the demote
        // loop takes the timer arm once `now >= deadline` (so a real send still beats the timer), and
        // clamps its backoff to the deadline. Lower throughput but sound — the documented v1 limit (§6d).
        if self.mn.is_some() {
            let arms: Vec<(usize, Arc<ChannelCore>)> = keys
                .iter()
                .map(|&h| (self.channel_core_ptr(h), self.channel_core(h)))
                .collect();
            let (arm_index, w) = self.demote_wait_block(arms, soonest, span)?;
            let v = self.from_wire(w);
            self.take_wait_arm(base, v, meta.arm_targets[arm_index]);
            return Ok(());
        }
        // Cooperative VM / interp (single-threaded) — a live timer arm inline-sleeps to the soonest
        // deadline and takes it (the frozen parity oracle; reached only when `mn.is_none()`).
        if let Some((i, deadline)) = soonest {
            let now = std::time::Instant::now();
            if now < deadline {
                std::thread::sleep(deadline - now);
            }
            self.take_wait_arm(base, Value::Bool(true), meta.arm_targets[i]);
            return Ok(());
        }
        if !self.scheduler_stack.is_empty() && self.native_reentry == 0 {
            // Cooperative multi-channel park: keep the N handles on the stack (they root the channels
            // + re-supply the poll), rewind to re-run `WaitPoll` on wake, and register the fiber on
            // every live arm channel via `wait_suspend` (consumed by `run_child`).
            self.frames.last_mut().unwrap().ip -= 1; // re-run this WaitPoll on resume
            self.wait_suspend = Some(keys);
            return Ok(());
        }
        // No scheduler (top level / single fiber) or inside a native callback: no sibling could ever
        // fill the channels — a real deadlock (mirrors `chan_recv_step`'s sequential `recv` fault).
        Err(self.err(
            "wait on channels that are all empty: deadlock — nothing is queued and the sequential \
             executor cannot block waiting for a producer"
                .to_string(),
            span,
        ))
    }

    /// Commit a chosen `wait` arm: drop the `n` channel handles (`stack[base..]`), push the received
    /// value, and jump to the arm body's target ip (the bind/assign/discard prologue).
    fn take_wait_arm(&mut self, base: usize, value: Value, target: usize) {
        self.stack.truncate(base);
        self.push(value);
        self.frames.last_mut().unwrap().ip = target;
    }

    /// `Shared[T]` methods (C3/C4): `get` (copies out), `set` (copies in), `update` (read-modify-write
    /// via the re-entrant call path). Mirrors `interp::eval_shared_method`. The box is re-rooted on
    /// the operand stack across `update`'s nested call (the receiver was popped in `do_method_call`).
    fn shared_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
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
                let w = self.to_wire_at(args[0], span)?;
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
                let _serialise = if self.parallel {
                    Some(core.update_lock.lock().unwrap())
                } else {
                    None
                };
                let w = core.v.lock().unwrap().clone();
                let cur = self.from_wire(w);
                self.push(Value::Obj(h));
                let next = self.guarded(|vm| vm.invoke_value(f, vec![cur], span));
                self.pop();
                let next = next?;
                let stored = self.to_wire_at(next, span)?;
                *core.v.lock().unwrap() = stored;
                Ok(Value::Nil)
            }
            _ => Err(self.err(format!("type Shared has no method '{method}'"), span)),
        }
    }

    /// `RwShared[T]` methods: `get`/`set` (read/write-guarded copy out/in), `read(f)` (SHARED read
    /// guard: clone out, drop guard, run `f`, return its result — NO write-back), `write(f)`
    /// (EXCLUSIVE write guard: a write-locked read-modify-write, the `Shared.update` shape under a
    /// `RwLock`). Mirrors `interp::eval_rwshared_method`. As with `Shared.update`, the lock guard is
    /// dropped across the user closure (a `RwLock` guard is not reentrant) and the receiver is
    /// re-rooted on the operand stack so the nested call's GC keeps the core's contents traced (the
    /// receiver was popped off the stack in `do_method_call`). `write`'s whole RMW is serialised
    /// across threads by a separate `update_lock` (held only under `--parallel`) — the `RwLock` write
    /// guard alone is NOT enough because it is dropped across the closure, so two writers could clone
    /// the same base and lose an update (same discipline as `Shared.update`). A closure that
    /// re-acquires the SAME box's write lock (or a write inside a read) deadlocks — a documented edge,
    /// mirroring `Shared.update`'s same-box re-entry limit.
    fn rwshared_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "get" => {
                self.arity_err("get", args, 0, span)?;
                // Clone the wire form out under the SHARED read guard, reconstruct into this heap.
                let w = self.rwshared_core(h).v.read().unwrap().clone();
                Ok(self.from_wire(w))
            }
            "set" => {
                self.arity_err("set", args, 1, span)?;
                let w = self.to_wire_at(args[0], span)?;
                *self.rwshared_core(h).v.write().unwrap() = w;
                Ok(Value::Nil)
            }
            "read" => {
                self.arity_err("read", args, 1, span)?;
                let f = args[0];
                let core = self.rwshared_core(h);
                // SHARED read guard: clone the value out, then DROP the guard before invoking `f`
                // (the guard is not reentrant; dropping it also lets other readers/a writer proceed).
                // No write-back — `read` returns `f`'s result.
                let w = core.v.read().unwrap().clone();
                let cur = self.from_wire(w);
                self.push(Value::Obj(h));
                let result = self.guarded(|vm| vm.invoke_value(f, vec![cur], span));
                self.pop();
                result
            }
            "write" => {
                self.arity_err("write", args, 1, span)?;
                let f = args[0];
                let core = self.rwshared_core(h);
                // Serialise the whole read-modify-write so concurrent OS-thread writes can't lose each
                // other (the box's contract, exactly like `Shared.update`). The `RwLock` write guard
                // alone is NOT enough: it must be DROPPED across the user closure (not reentrant), so
                // two `write`s could clone the same base and lose an update — hence a separate
                // `update_lock` held for the entire RMW *only under `--parallel`*. The cooperative
                // engine is single-thread, so it never takes `update_lock` (taking it would needlessly
                // deadlock a same-box nested write). The value lock `v` is taken only briefly (read
                // here, write at the end), so the closure may freely re-enter `get`/`set`/`read` (or
                // `write` on a *different* box). The handle is re-rooted on the operand stack so the
                // nested call's GC keeps the core's contents traced.
                let _serialise = if self.parallel {
                    Some(core.update_lock.lock().unwrap())
                } else {
                    None
                };
                let w = core.v.write().unwrap().clone();
                let cur = self.from_wire(w);
                self.push(Value::Obj(h));
                let next = self.guarded(|vm| vm.invoke_value(f, vec![cur], span));
                self.pop();
                let next = next?;
                let stored = self.to_wire_at(next, span)?;
                *core.v.write().unwrap() = stored;
                Ok(Value::Nil)
            }
            _ => Err(self.err(format!("type RwShared has no method '{method}'"), span)),
        }
    }

    /// `Atomic[T]` methods: `load` (copy out), `store` (copy in), `exchange` (swap, returns old),
    /// `cas(expected, new) -> bool` (swap iff the box equals `expected`), `add`/`sub` (numeric RMW,
    /// returns the new value). Each is a single lock-op-unlock, so the RMW is atomic across threads —
    /// no user closure runs under the lock (unlike `Shared.update`), so no `update_lock` is needed.
    /// Mirrors `interp::eval_atomic_method`. `add`/`sub` use the language's `checked_add`/`checked_sub`
    /// (int overflow faults, like the `+`/`-` operators) and plain float arithmetic.
    fn atomic_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "load" => {
                self.arity_err("load", args, 0, span)?;
                let w = self.atomic_core(h).v.lock().unwrap().clone();
                Ok(self.from_wire(w))
            }
            "store" => {
                self.arity_err("store", args, 1, span)?;
                let w = self.to_wire_at(args[0], span)?;
                *self.atomic_core(h).v.lock().unwrap() = w;
                Ok(Value::Nil)
            }
            "exchange" => {
                self.arity_err("exchange", args, 1, span)?;
                let new_w = self.to_wire_at(args[0], span)?;
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
                    *g = self.to_wire_at(args[1], span)?;
                }
                Ok(Value::Bool(swapped))
            }
            "add" | "sub" => {
                self.arity_err(method, args, 1, span)?;
                let delta = self.to_wire_at(args[0], span)?;
                let core = self.atomic_core(h);
                let mut g = core.v.lock().unwrap();
                let new = match (&*g, &delta) {
                    (WireValue::Int(a), WireValue::Int(b)) => {
                        let (r, label) = if method == "add" {
                            (a.checked_add(*b), "Add")
                        } else {
                            (a.checked_sub(*b), "Sub")
                        };
                        WireValue::Int(r.ok_or_else(|| {
                            self.err(format!("integer overflow in {label}"), span)
                        })?)
                    }
                    (WireValue::Float(a), WireValue::Float(b)) => {
                        WireValue::Float(if method == "add" { a + b } else { a - b })
                    }
                    // The checker gates `add`/`sub` to numeric element types, so this is unreachable.
                    _ => {
                        return Err(self.err(format!("type Atomic has no method '{method}'"), span));
                    }
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
    fn executor_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "submit" => {
                self.arity_err("submit", args, 1, span)?;
                let core = self.executor_core(h);
                {
                    let mut g = core.inner.lock().unwrap();
                    if g.shut {
                        return Err(self.err(
                            "submit on a shut-down Executor (it no longer accepts work)"
                                .to_string(),
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
                    let tasks: Vec<WireValue> =
                        core.inner.lock().unwrap().queue.drain(..).collect();
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
    fn unwind_deferred(
        &mut self,
        target_frame_len: usize,
        report_escaped: bool,
    ) -> Option<RuntimeError> {
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
            while self
                .handlers
                .last()
                .is_some_and(|h| h.frame_len > self.frames.len())
            {
                self.handlers.pop();
            }
        }
        err
    }

    fn do_try(&mut self, span: Span) -> Result<(), RuntimeError> {
        let v = self.pop();
        // Extract (variant_id, payload-arity, first-payload) up front so the heap borrow is released
        // before we mutate the stack / unwind a frame.
        let info = match v {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Enum {
                    variant_id,
                    payload,
                } => Some((*variant_id, payload.len(), payload.first().copied())),
                _ => None,
            },
            _ => None,
        };
        // M19 lever #2 — gate on the fixed native variant ids (`VID_OK`/`VID_SOME` unwrap, `VID_ERR`/
        // `VID_NONE_VARIANT` propagate), NOT a name compare. A user enum shadowing `Ok`/`Err`/`Some`/
        // `None` gets distinct ids, so it is correctly NOT treated as a Result/Option by `?`.
        use crate::vm::op::{VID_ERR, VID_NONE_VARIANT, VID_OK, VID_SOME};
        if let Some((variant_id, n, first)) = info {
            if (variant_id == VID_OK || variant_id == VID_SOME) && n == 1 {
                self.push(first.unwrap());
                return Ok(());
            }
            if variant_id == VID_ERR || variant_id == VID_NONE_VARIANT {
                // A `?` directly inside a `recover:` block (a handler installed in THIS frame)
                // short-circuits to that boundary (try-block style): the `Err`/`None` value becomes
                // the recover's result. Function-scoped `?` (no same-frame handler) falls through.
                let frame_len = self.frames.len();
                if let Some(h) = self.handlers.pop_if(|h| h.frame_len == frame_len) {
                    self.stack.truncate(h.stack_len);
                    self.call_depth = h.call_depth;
                    // Drop scope markers of defer scopes opened inside the recover block — the `?`
                    // jumps past their `LeaveDeferScope`s, so they would otherwise leak.
                    self.frames
                        .last_mut()
                        .unwrap()
                        .defer_markers
                        .truncate(h.markers_len);
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
                    if self.pending_exit.is_some()
                        && let Some(e) = body_defer_err.take()
                    {
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
        Err(self.err(
            format!("'?' expects Result or Option, found {}", self.type_name(v)),
            span,
        ))
    }

    // ----- construction / access -----

    fn new_struct(&mut self, name: &str, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let def = self
            .program
            .structs
            .get(name)
            .cloned()
            .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        if argc != def.fields.len() {
            return Err(self.err(
                format!(
                    "struct '{}' expects {} field(s), got {argc}",
                    def.display_name,
                    def.fields.len()
                ),
                span,
            ));
        }
        let at = self.stack.len() - argc;
        // Positional layout: the args already arrive in declaration order (desugar reorders any
        // named-field constructor before codegen), so split them straight in — no per-field name
        // strings, no zip with `def.fields`. `argc == def.fields.len()` is checked above.
        let fields: Vec<Value> = self.stack.split_off(at);
        let h = self.heap.alloc(Obj::Struct {
            name: name.into(),
            tid: def.tid,
            fields,
        });
        self.push(Value::Obj(h));
        Ok(())
    }

    /// The dense layout id for a struct type `name`, or [`TID_NONE`] if it isn't a registered type
    /// (native/ad-hoc structs) — such a struct never IC-caches, so it stays sound on the probe path.
    fn struct_tid(&self, name: &str) -> u32 {
        self.program.structs.get(name).map_or(TID_NONE, |d| d.tid)
    }

    /// M19 lever #2 — resolve a `variant_id` back to its `(enum-type, variant)` names on the COLD path
    /// (Display / stringify / error / wire / snap), where the instance no longer carries the strings.
    /// O(1) via `Program::variants_by_id`. Returns `("?", "?")` for [`crate::vm::op::VID_NONE`] / an
    /// out-of-range id (defensive — a registered enum always resolves).
    fn enum_names(&self, variant_id: u32) -> (&str, &str) {
        self.program
            .variants_by_id
            .get(variant_id as usize)
            .map_or(("?", "?"), |d| (d.enum_name.as_str(), d.name.as_str()))
    }

    /// The index of the module that declared the enum keyed by `enum_key` (its method bodies resolve
    /// top-level names against that module's globals). Defaults to module 0 if unrecorded.
    fn enum_home_module(&self, enum_key: &str) -> usize {
        self.program.enum_home.get(enum_key).copied().unwrap_or(0)
    }

    /// The index of the module that declared the newtype keyed by `key` (home-globals for its
    /// methods). Mirrors [`enum_home_module`]. Defaults to module 0 if unrecorded.
    fn newtype_home_module(&self, key: &str) -> usize {
        self.program.newtype_home.get(key).copied().unwrap_or(0)
    }

    /// Construct an enum from `Op::NewEnum`. M19 lever #2 — the dense `variant_id` is baked into the op
    /// at compile time (no runtime hash lookup); it is stamped onto the instance instead of the two
    /// per-instance type/variant `Box<str>`s. `variant` is used only for the arity-mismatch message.
    fn new_enum(
        &mut self,
        variant: &str,
        variant_id: u32,
        argc: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if let Some(def) = self.program.variants_by_id.get(variant_id as usize)
            && argc != def.arity
        {
            return Err(self.err(
                format!(
                    "variant '{variant}' expects {} value(s), got {argc}",
                    def.arity
                ),
                span,
            ));
        }
        let at = self.stack.len() - argc;
        let payload: Vec<Value> = self.stack.split_off(at);
        let h = self.heap.alloc(Obj::Enum {
            variant_id,
            payload,
        });
        self.push(Value::Obj(h));
        Ok(())
    }

    fn get_field(&mut self, name: &str, ic: u32, span: Span) -> Result<(), RuntimeError> {
        let obj = self.pop();
        let Value::Obj(h) = obj else {
            return Err(self.err(
                format!("cannot read field '{name}' of {}", self.type_name(obj)),
                span,
            ));
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
                && let Some(v) = fields.get(cell.idx as usize)
            {
                let v = *v;
                self.push(v);
                return Ok(());
            }
        }
        match self.heap.get(h) {
            // `t.0`, `t.1`, … — tuple element access. The field name is the element index.
            Obj::Tuple(items) => {
                let v = name
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| items.get(i).copied());
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
            Obj::Struct {
                name: sname,
                tid,
                fields,
                ..
            } => {
                // Positional layout: the field name->index map lives in the StructDef (declaration
                // order), not the instance. Resolve the slot there, then index the flat `fields`
                // Vec. Capture both index + layout `tid` so the IC can cache them (Value is Copy,
                // `tid` is a `u32`, so the heap borrow ends here, freeing `self` for the write).
                let tid = *tid;
                let idx = self
                    .program
                    .structs
                    .get(sname.as_ref())
                    .and_then(|d| d.fields.iter().position(|f| f == name));
                let found = idx.and_then(|i| fields.get(i).map(|v| (i, *v)));
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
            Obj::Module {
                name: mname,
                slots,
                index,
            } => match index.get(name).map(|&i| slots[i as usize]) {
                Some(v) => {
                    self.push(v);
                    Ok(())
                }
                None => Err(self.err(format!("module '{mname}' has no member '{name}'"), span)),
            },
            _ => Err(self.err(
                format!("cannot read field '{name}' of {}", self.type_name(obj)),
                span,
            )),
        }
    }

    /// `obj[start:end:step]` — Python-style slice copy of a list/str, or a struct's `slice`. Each
    /// component arrives as `Nil` (omitted → `None`) or `Int`; the shared `slice::slice_indices`
    /// resolver owns all the clamp/step/reverse math (byte-identical with the interpreter).
    fn get_slice(&mut self, span: Span) -> Result<(), RuntimeError> {
        let step = self.pop();
        let end = self.pop();
        let start = self.pop();
        let obj = self.pop();
        // Each component is `Nil` (omitted) → `None`, or an `Int` → `Some`. Anything else faults.
        let comp = |vm: &Vm, v: Value| -> Result<Option<i64>, RuntimeError> {
            match v {
                Value::Nil => Ok(None),
                Value::Int(n) => Ok(Some(n)),
                other => Err(vm.err(format!("expected int, found {}", vm.type_name(other)), span)),
            }
        };
        let s = comp(self, start)?;
        let e = comp(self, end)?;
        let st = comp(self, step)?;
        let Value::Obj(h) = obj else {
            return Err(self.err(format!("cannot slice {}", self.type_name(obj)), span));
        };
        // Snapshot the result kind without holding the heap borrow across the alloc / method call.
        enum Sliced {
            List(Vec<Value>),
            Str(String),
            Bytes(Vec<u8>),
            ByteArray(Vec<u8>),
            Struct,
        }
        let sliced = match self.heap.get(h) {
            Obj::List(items) => {
                let idxs = crate::slice::slice_indices(s, e, st, items.len())
                    .map_err(|m| self.err(m.to_string(), span))?;
                Sliced::List(idxs.iter().map(|&i| items[i]).collect())
            }
            Obj::Str(string) => {
                let chars: Vec<char> = string.chars().collect();
                let idxs = crate::slice::slice_indices(s, e, st, chars.len())
                    .map_err(|m| self.err(m.to_string(), span))?;
                Sliced::Str(idxs.iter().map(|&i| chars[i]).collect())
            }
            // `bytes[a:b:c]` slices over BYTE offsets and yields a new `bytes` (open bounds / step /
            // reverse / negative all via the shared `slice_indices`, exactly like list/str).
            Obj::Bytes(b) => {
                let idxs = crate::slice::slice_indices(s, e, st, b.len())
                    .map_err(|m| self.err(m.to_string(), span))?;
                Sliced::Bytes(idxs.iter().map(|&i| b[i]).collect())
            }
            // `bytearray[a:b:c]` slices over BYTE offsets and yields a NEW `bytearray` (mutable copy).
            Obj::ByteArray(b) => {
                let idxs = crate::slice::slice_indices(s, e, st, b.len())
                    .map_err(|m| self.err(m.to_string(), span))?;
                Sliced::ByteArray(idxs.iter().map(|&i| b[i]).collect())
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
            Sliced::Bytes(sub) => {
                let nh = self.heap.alloc(Obj::Bytes(sub.into_boxed_slice()));
                self.push(Value::Obj(nh));
            }
            Sliced::ByteArray(sub) => {
                let nh = self.heap.alloc(Obj::ByteArray(sub));
                self.push(Value::Obj(nh));
            }
            Sliced::Struct => {
                // The `slice` protocol takes three `Option[int]` components — pass real `Option`
                // values (`None`/`Some(n)`) so the user body can `match`/`??` them. Root `obj`
                // across the enum allocs (it's the only reference keeping the receiver alive).
                self.push(obj);
                let opt = |vm: &mut Vm, c: Option<i64>| match c {
                    None => vm.alloc_enum("Option", "None", Vec::new()),
                    Some(n) => vm.alloc_enum("Option", "Some", vec![Value::Int(n)]),
                };
                let s_v = opt(self, s);
                let e_v = opt(self, e);
                let st_v = opt(self, st);
                self.pop();
                let v = self.dispatch_index_method(h, "slice", vec![obj, s_v, e_v, st_v], span)?;
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
        let Obj::Struct { name, .. } = self.heap.get(h) else {
            unreachable!()
        };
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
                    return match crate::slice::norm_index(n, items.len()).map(|i| items[i]) {
                        Some(v) => {
                            self.push(v);
                            Ok(())
                        }
                        None => Err(self.err(
                            format!("index {n} out of bounds (len {})", items.len()),
                            span,
                        )),
                    };
                }
                // `bytes[i]`/`bytearray[i]` → `int` (0–255); out-of-range faults recoverably.
                Obj::Bytes(b) => {
                    return match crate::slice::norm_index(n, b.len()).map(|i| b[i] as i64) {
                        Some(v) => {
                            self.push(Value::Int(v));
                            Ok(())
                        }
                        None => {
                            Err(self
                                .err(format!("index {n} out of bounds (len {})", b.len()), span))
                        }
                    };
                }
                Obj::ByteArray(b) => {
                    return match crate::slice::norm_index(n, b.len()).map(|i| b[i] as i64) {
                        Some(v) => {
                            self.push(Value::Int(v));
                            Ok(())
                        }
                        None => {
                            Err(self
                                .err(format!("index {n} out of bounds (len {})", b.len()), span))
                        }
                    };
                }
                Obj::Map(_) => {
                    let hk = self.scalar_hash(key);
                    let Obj::Map(m) = self.heap.get(h) else {
                        unreachable!()
                    };
                    return match m
                        .candidates(hk)
                        .iter()
                        .copied()
                        .find(|&p| self.values_equal(m.entries[p].1, key))
                    {
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
                let v = crate::slice::norm_index(idx, items.len()).map(|i| items[i]);
                match v {
                    Some(v) => {
                        self.push(v);
                        Ok(())
                    }
                    None => Err(self.err(
                        format!("index {idx} out of bounds (len {})", items.len()),
                        span,
                    )),
                }
            }
            Obj::Str(s) => {
                let idx = int_idx(self)?;
                let chars: Vec<char> = s.chars().collect();
                match crate::slice::norm_index(idx, chars.len()).map(|i| chars[i]) {
                    Some(c) => {
                        let nh = self.alloc_char(c);
                        self.push(nh);
                        Ok(())
                    }
                    None => Err(self.err(
                        format!("index {idx} out of bounds (len {})", chars.len()),
                        span,
                    )),
                }
            }
            Obj::Bytes(b) => {
                let idx = int_idx(self)?;
                match crate::slice::norm_index(idx, b.len()).map(|i| b[i] as i64) {
                    Some(v) => {
                        self.push(Value::Int(v));
                        Ok(())
                    }
                    None => {
                        Err(self.err(format!("index {idx} out of bounds (len {})", b.len()), span))
                    }
                }
            }
            Obj::ByteArray(b) => {
                let idx = int_idx(self)?;
                match crate::slice::norm_index(idx, b.len()).map(|i| b[i] as i64) {
                    Some(v) => {
                        self.push(Value::Int(v));
                        Ok(())
                    }
                    None => {
                        Err(self.err(format!("index {idx} out of bounds (len {})", b.len()), span))
                    }
                }
            }
            Obj::Map(_) => {
                let hk = self.hash_key_rooted(key, &[obj, key], span)?;
                let Obj::Map(m) = self.heap.get(h) else {
                    unreachable!()
                };
                match m
                    .candidates(hk)
                    .iter()
                    .copied()
                    .find(|&p| self.values_equal(m.entries[p].1, key))
                {
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
            return Err(self.err(
                format!("cannot assign field '{name}' of {}", self.type_name(obj)),
                span,
            ));
        };
        // M19 Phase 5b — IC fast path (see [`Vm::get_field`]): a hit on the `tid` guard writes straight
        // to the cached index (no field-name re-verify); a miss falls through to the probe + cache-fill.
        if ic != NO_IC {
            let cell = self.field_ic[ic as usize];
            if cell.tid != TID_NONE
                && let Obj::Struct { tid, fields, .. } = self.heap.get_mut(h)
                && *tid == cell.tid
                && let Some(slot) = fields.get_mut(cell.idx as usize)
            {
                *slot = val;
                return Ok(());
            }
        }
        // Positional layout: resolve the field name->index from the StructDef (declaration order)
        // BEFORE the mutable heap borrow, then write the flat `fields` slot by index.
        let sname = match self.heap.get(h) {
            Obj::Struct { name, .. } => Some(name.clone()),
            _ => None,
        };
        let idx = sname
            .as_ref()
            .and_then(|n| self.program.structs.get(n.as_ref()))
            .and_then(|d| d.fields.iter().position(|f| f == name));
        let found;
        match self.heap.get_mut(h) {
            Obj::Struct { tid, fields, .. } => {
                let tid = *tid;
                match idx.and_then(|i| fields.get_mut(i).map(|slot| (i, slot))) {
                    Some((i, slot)) => {
                        *slot = val;
                        found = (i as u32, tid);
                    }
                    None => {
                        let shown = self.display(obj);
                        return Err(self.err(format!("no field '{name}' on {shown}"), span));
                    }
                }
            }
            _ => {
                return Err(self.err(
                    format!("cannot assign field '{name}' of {}", self.type_name(obj)),
                    span,
                ));
            }
        }
        if ic != NO_IC {
            self.field_ic[ic as usize] = IcCell {
                idx: found.0,
                tid: found.1,
            };
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
            let Obj::Map(m) = self.heap.get(h) else {
                unreachable!()
            };
            let pos = m
                .candidates(hk)
                .iter()
                .copied()
                .find(|&p| self.values_equal(m.entries[p].1, key));
            let Obj::Map(m) = self.heap.get_mut(h) else {
                unreachable!()
            };
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
            let Obj::Map(m) = self.heap.get(h) else {
                unreachable!()
            };
            let pos = m
                .candidates(hk)
                .iter()
                .copied()
                .find(|&p| self.values_equal(m.entries[p].1, key));
            let Obj::Map(m) = self.heap.get_mut(h) else {
                unreachable!()
            };
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
            other => {
                return Err(self.err(
                    format!("expected int, found {}", self.type_name(other)),
                    span,
                ));
            }
        };
        // `bytearray[i] = x` — the NEW mutable capability `bytes` lacks. The value must be an `int`
        // in 0..=255 (validated BEFORE the in-place write); the index must be in range. Both are
        // distinct recoverable faults. Mutation flows through the heap slot, so two bindings to the
        // same `bytearray` observe it (like `list`). Validate the value up front (`&self` borrow)
        // before the `&mut self` `get_mut` below.
        if matches!(self.heap.get(h), Obj::ByteArray(_)) {
            let byte = match val {
                Value::Int(n) if (0..=255).contains(&n) => n as u8,
                Value::Int(n) => {
                    return Err(self.err(
                        format!("byte value {n} out of range (must be 0..=255)"),
                        span,
                    ));
                }
                other => {
                    return Err(self.err(
                        format!("expected int, found {}", self.type_name(other)),
                        span,
                    ));
                }
            };
            let Obj::ByteArray(b) = self.heap.get_mut(h) else {
                unreachable!()
            };
            return match crate::slice::norm_index(idx, b.len()) {
                Some(i) => {
                    b[i] = byte;
                    Ok(())
                }
                None => {
                    let len = b.len();
                    Err(self.err(format!("index {idx} out of bounds (len {len})"), span))
                }
            };
        }
        match self.heap.get_mut(h) {
            Obj::List(items) => match crate::slice::norm_index(idx, items.len()) {
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

    #[allow(clippy::too_many_arguments)]
    fn match_arm(
        &mut self,
        scrut: usize,
        variant: &str,
        variant_id: u32,
        enum_name: Option<&str>,
        nbind: usize,
        bind_start: usize,
        next: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let v = self.stack[self.base() + scrut];
        let h = match v {
            Value::Obj(h) => h,
            _ => unreachable!("scrutinee ensured to be an enum"),
        };
        // M19 lever #2 — dispatch is a pure-int compare of the instance's stamped `variant_id` against
        // the arm's compile-time id (no variant-name string compare). `variant` is only the cold error.
        let (mut matches, vid, payload) = match self.heap.get(h) {
            Obj::Enum {
                variant_id: vid,
                payload,
            } => (*vid == variant_id, *vid, payload.clone()),
            _ => unreachable!("scrutinee ensured to be an enum"),
        };
        // SCRUTINEE-DRIVEN fallback on an id MISS. The compile-time `variant_id` can be the WRONG
        // module's id when a bare match-pattern enum qualifier (`Color.Red`) is reachable only via a
        // whole-module import and TWO whole-imported modules declare the same-named enum — the
        // construction side baked the scrutinee's correct id, but the pattern side may have guessed
        // the other module's id. Resolve from the SCRUTINEE's own `(enum_key, variant)` identity:
        // it matches iff the arm names that variant in the same (bare) enum. Built-in arms carry
        // `enum_name: None` and never enter this branch (pure-int dispatch — zero behavior change).
        if !matches && let Some(en) = enum_name {
            let (ekey, vname) = self.enum_names(vid);
            // Compare the bare display name without allocating (hot match path): strip the
            // `<module-key>::` prefix in place rather than via `bare_display`'s owned String.
            matches = vname == variant && ekey.rsplit("::").next().unwrap_or(ekey) == en;
        }
        if !matches {
            self.jump(next);
            return Ok(());
        }
        if payload.len() != nbind {
            return Err(self.err(
                format!(
                    "pattern '{variant}' binds {nbind} value(s) but variant carries {}",
                    payload.len()
                ),
                span,
            ));
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

    /// `print(args…, sep=, end=)`. Stack layout on entry: `[args… , sep, end]`. Pops `end` then
    /// `sep` (both `str`, copied out so they're no longer GC roots), stringifies the `argc` user
    /// args (kept rooted on the stack across `stringify`, which can run user code + GC), joins with
    /// `sep` and appends `end`. Byte-identical join/append semantics to the interpreter.
    fn do_print_sep(&mut self, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let end = self.pop();
        let sep = self.pop();
        let end = self.val_str(end).unwrap_or_default();
        let sep = self.val_str(sep).unwrap_or_default();
        let at = self.stack.len() - argc;
        let mut parts = Vec::with_capacity(argc);
        for i in 0..argc {
            let v = self.stack[at + i];
            parts.push(self.stringify(v, span, 0)?);
        }
        self.stack.truncate(at);
        self.out.push_str(&parts.join(&sep));
        self.out.push_str(&end);
        self.push(Value::Nil);
        Ok(())
    }

    fn do_builtin(&mut self, name: &str, argc: usize, span: Span) -> Result<(), RuntimeError> {
        let at = self.stack.len() - argc;
        let args: Vec<Value> = self.stack.split_off(at);
        // `panic(msg)` raises the SAME recoverable `RuntimeError` (`self.err`) the runtime uses for
        // overflow/OOB/decode, instead of pushing a value — it unwinds (running `defer`s) to the
        // nearest `recover:` as `Err(e)` with `e.message() == msg`, else aborts. Early-return before
        // the value-returning match so nothing is pushed on the (unwinding) path. The checker
        // guarantees a single `str` arg; fall back to the value's type name for a non-str (matches
        // the interp's defensive guard, keeping messages byte-identical across engines).
        if name == "panic" {
            let message = match args.first() {
                Some(Value::Obj(h)) => match self.heap.get(*h) {
                    Obj::Str(s) => s.to_string(),
                    _ => self.type_name(args[0]).to_string(),
                },
                Some(other) => self.type_name(*other).to_string(),
                None => String::new(),
            };
            return Err(self.err(message, span));
        }
        let result = match name {
            "range" => self.builtin_range(&args, span)?,
            "int" => self.builtin_int(&args, span)?,
            "float" => self.builtin_float(&args, span)?,
            "str" => self.builtin_str(&args, span)?,
            "ord" => self.builtin_ord(&args, span)?,
            "chr" => self.builtin_chr(&args, span)?,
            "Set" => self.builtin_set(&args, span)?,
            "List" => self.builtin_list(&args, span)?,
            "Map" => self.builtin_map(&args, span)?,
            "bytearray" => self.builtin_bytearray(&args, span)?,
            "bytes" => self.builtin_bytes(&args, span)?,
            _ => unreachable!("unknown builtin {name}"),
        };
        self.push(result);
        Ok(())
    }

    fn arity_err(
        &self,
        name: &str,
        args: &[Value],
        n: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if args.len() == n {
            Ok(())
        } else {
            Err(self.err(
                format!("{name}() expects {n} argument(s), got {}", args.len()),
                span,
            ))
        }
    }

    /// D6c — arity check for a method that accepts an inclusive `min..=max` argument range (the net
    /// socket ops: `read`/`write` take 1–2, `accept` 0–1 — the optional trailing `timeout_ms`).
    fn arity_range_err(
        &self,
        name: &str,
        args: &[Value],
        min: usize,
        max: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if (min..=max).contains(&args.len()) {
            Ok(())
        } else {
            Err(self.err(
                format!(
                    "{name}() expects {min}–{max} argument(s), got {}",
                    args.len()
                ),
                span,
            ))
        }
    }

    /// D6c — parse the optional trailing `timeout_ms` int arg of a net socket op. `Ok(None)` if no
    /// timeout arg was passed (park forever — the existing behavior). `Ok(Some(Timeout))` otherwise:
    /// `poll_once` is true iff `ms <= 0` (`0` polls once and never parks; a negative saturates to it),
    /// and `deadline` is `now + ms`, saturated to a far-future deadline for a pathological `ms`
    /// (centuries) rather than panicking the worker on `Instant` overflow (mirrors `sleep_ms`). `Err`
    /// for a non-int timeout arg (the checker also rejects this; this is the runtime backstop).
    fn parse_timeout_ms(
        &self,
        arg: Option<&Value>,
        span: Span,
    ) -> Result<Option<SockTimeout>, RuntimeError> {
        match arg {
            None => Ok(None),
            Some(Value::Int(ms)) => {
                let poll_once = *ms <= 0;
                let ms = (*ms).max(0) as u64;
                let dur = std::time::Duration::from_millis(ms);
                let deadline = std::time::Instant::now()
                    .checked_add(dur)
                    .unwrap_or_else(|| {
                        std::time::Instant::now() + std::time::Duration::from_secs(86_400 * 365)
                    });
                Ok(Some(SockTimeout {
                    poll_once,
                    deadline,
                }))
            }
            Some(_) => Err(self.err("timeout_ms expects an int (milliseconds)".into(), span)),
        }
    }

    /// Drain ANY for-iterable into a `Vec<Value>` of its elements — the runtime peer of the checker's
    /// `iter_elem` (the single source of truth for "what `for x in X` accepts"). Built-in collections
    /// copy their elements directly (list/set elems, str→per-char str, bytes/bytearray→per-byte int,
    /// map→keys, range is already materialized to a list); a `Generator` is driven via `generator_next`
    /// until `None`; a user struct with `next(self) -> Option[T]` re-enters the VM (`run_proto`) per
    /// step until `None`. Both re-entrant paths run user code that can GC, so the growing accumulator
    /// is built into a heap `Obj::List` ROOTED on the operand stack across every `.next()` (mirrors
    /// `builtin_set`/`list_hof`/`struct_hash`). The source handle is rooted too. Returns the collected
    /// elements (cloned out of the rooted list after the loop, GC-safe).
    fn drain_iterable(&mut self, v: Value, span: Span) -> Result<Vec<Value>, RuntimeError> {
        // Built-in collections: copy directly, no re-entry.
        if let Value::Obj(h) = v {
            match self.heap.get(h) {
                Obj::List(items) => return Ok(items.clone()),
                Obj::Set(s) => return Ok(s.entries.iter().map(|(_, e)| *e).collect()),
                Obj::Map(m) => return Ok(m.entries.iter().map(|(_, k, _)| *k).collect()),
                Obj::Bytes(b) => {
                    let bytes = b.clone();
                    return Ok(bytes.iter().map(|&x| Value::Int(x as i64)).collect());
                }
                Obj::ByteArray(b) => {
                    let bytes = b.clone();
                    return Ok(bytes.iter().map(|&x| Value::Int(x as i64)).collect());
                }
                Obj::Str(s) => {
                    let chars: Vec<char> = s.chars().collect();
                    return Ok(chars.into_iter().map(|c| self.alloc_char(c)).collect());
                }
                // A cursor drains its REMAINING snapshot (`items[pos..]`) directly — it IS an
                // `Iterator[T]`, so `List(xs.iter())`/`Set(...)` round-trip for free.
                Obj::Iter { items, pos } => {
                    return Ok(items[(*pos).min(items.len())..].to_vec());
                }
                _ => {}
            }
        }
        // Re-entrant paths (generator / user struct iterator): run user `.next()` until `None`, rooting
        // the source + a heap accumulator list on the operand stack across each call.
        let Value::Obj(h) = v else {
            return Err(self.err(format!("cannot iterate over {}", self.type_name(v)), span));
        };
        // Resolve the iteration step: a generator resumes; a user struct dispatches its `next` proto.
        enum Step {
            Generator,
            StructNext { proto: ProtoId, home: GcRef },
        }
        let step = match self.heap.get(h) {
            Obj::Generator(_) => Step::Generator,
            Obj::Struct { name, .. } => {
                let name = name.clone();
                let def = self
                    .program
                    .structs
                    .get(name.as_ref())
                    .cloned()
                    .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
                let proto = *def.methods.get("next").ok_or_else(|| {
                    self.err(
                        format!(
                            "cannot iterate over {} (no `next` method)",
                            self.type_name(v)
                        ),
                        span,
                    )
                })?;
                Step::StructNext {
                    proto,
                    home: self.module_objs[def.module_idx],
                }
            }
            _ => return Err(self.err(format!("cannot iterate over {}", self.type_name(v)), span)),
        };
        // Root the source + the growing accumulator list across the re-entrant calls.
        let acc = self.heap.alloc(Obj::List(Vec::new()));
        self.push(v);
        self.push(Value::Obj(acc));
        let result = (|| {
            loop {
                let res = match step {
                    Step::Generator => self.generator_next(h, span)?,
                    Step::StructNext { proto, home } => self.guarded(|vm| {
                        vm.run_proto(proto, home, None, vec![v], true, false, span)
                    })?,
                };
                let Value::Obj(rh) = res else {
                    return Err(self.err(
                        format!(
                            "iterator next() must return Option, found {}",
                            self.type_name(res)
                        ),
                        span,
                    ));
                };
                let Obj::Enum {
                    variant_id,
                    payload,
                } = self.heap.get(rh)
                else {
                    return Err(self.err(
                        format!(
                            "iterator next() must return Option, found {}",
                            self.type_name(res)
                        ),
                        span,
                    ));
                };
                match *variant_id {
                    crate::vm::op::VID_SOME => {
                        let item = *payload.first().ok_or_else(|| {
                            self.err(
                                "iterator next() returned Some with no payload".to_string(),
                                span,
                            )
                        })?;
                        let Obj::List(buf) = self.heap.get_mut(acc) else {
                            unreachable!()
                        };
                        buf.push(item);
                    }
                    crate::vm::op::VID_NONE_VARIANT => break,
                    _ => {
                        return Err(self.err(
                            format!(
                                "iterator next() must return Option, found {}",
                                self.type_name(res)
                            ),
                            span,
                        ));
                    }
                }
            }
            Ok(())
        })();
        result?;
        // Clone the collected elements out of the rooted list before unrooting.
        let Obj::List(buf) = self.heap.get(acc) else {
            unreachable!()
        };
        let out = buf.clone();
        self.pop(); // unroot accumulator
        self.pop(); // unroot source
        Ok(out)
    }

    /// `Set()` → empty set; `Set(it)` → a deduped hash set drained from ANY for-iterable.
    fn builtin_set(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        let src: Vec<Value> = match args {
            [] => Vec::new(),
            [one] => {
                let it = self.unwrap_newtype_value(*one);
                self.drain_iterable(it, span)?
            }
            _ => {
                return Err(self.err(
                    format!("Set() expects 0 or 1 argument(s), got {}", args.len()),
                    span,
                ));
            }
        };
        // Root the source elements (as a fresh heap list) so they survive a struct element's
        // re-entrant hash() GC; hash every element first (phase 1, rooted), then build GC-free.
        let list_obj = Value::Obj(self.heap.alloc(Obj::List(src.clone())));
        self.push(list_obj);
        let built = (|| {
            let mut hashes = Vec::with_capacity(src.len());
            for &v in &src {
                hashes.push(self.hash_value(v, span)?);
            }
            let mut set = SetData::default();
            for (i, &v) in src.iter().enumerate() {
                let he = hashes[i];
                if !set
                    .candidates(he)
                    .iter()
                    .any(|&p| self.values_equal(set.entries[p].1, v))
                {
                    set.push(he, v);
                }
            }
            Ok(set)
        })();
        self.pop(); // unroot the source list
        Ok(Value::Obj(self.heap.alloc(Obj::Set(built?))))
    }

    /// Cast-unwrap a generic aggregate newtype to its inner value for `List(s)`/`Set(s)`/`Map(s)`: a
    /// `Obj::NewType` (e.g. a `Stack[T] = List[T]`) peels to the wrapped collection. A non-newtype
    /// value passes through. Type args are erased at runtime; the checker verified the underlying.
    fn unwrap_newtype_value(&self, v: Value) -> Value {
        if let Value::Obj(h) = v
            && let Obj::NewType { inner, .. } = self.heap.get(h)
        {
            return *inner;
        }
        v
    }

    /// `List()` → a fresh empty list (the `List[T]()` turbofish form; mirrors `Set()`); `List(it)` →
    /// a list drained from ANY for-iterable. Mirrors `interp::Interp::builtin_list`.
    fn builtin_list(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        let items = match args {
            [] => Vec::new(),
            [one] => {
                let it = self.unwrap_newtype_value(*one);
                self.drain_iterable(it, span)?
            }
            _ => {
                return Err(self.err(
                    format!("List() expects 0 or 1 argument(s), got {}", args.len()),
                    span,
                ));
            }
        };
        Ok(Value::Obj(self.heap.alloc(Obj::List(items))))
    }

    /// `Map(it)` → a map from an iterable of 2-tuples `(k, v)` (last-wins on dup keys, like the
    /// `{k: v}` literal). Mirrors `interp::Interp::builtin_map`. A struct key's `hash()` re-enters the
    /// VM, so the in-flight key/value are rooted via `hash_key_rooted` while the building map is rooted.
    fn builtin_map(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        let one = match args {
            // `Map()` → a fresh empty map (the `Map[K, V]()` turbofish form; mirrors `Set()`).
            [] => return Ok(Value::Obj(self.heap.alloc(Obj::Map(MapData::default())))),
            [one] => one,
            _ => {
                return Err(self.err(
                    format!("Map() expects 0 or 1 argument(s), got {}", args.len()),
                    span,
                ));
            }
        };
        let it = self.unwrap_newtype_value(*one);
        // Cast-unwrapping a generic newtype over `Map[K, V]` (`Tally[T] = Map[T, int]`) yields the
        // inner map DIRECTLY — a copy, not a re-iteration as 2-tuples (iterating a map gives keys).
        if let Value::Obj(h) = it
            && let Obj::Map(inner) = self.heap.get(h)
        {
            let copy = inner.clone();
            return Ok(Value::Obj(self.heap.alloc(Obj::Map(copy))));
        }
        let drained = self.drain_iterable(it, span)?;
        // Root the drained elements (as a fresh heap list) across the re-entrant hash() calls.
        let src_obj = Value::Obj(self.heap.alloc(Obj::List(drained.clone())));
        self.push(src_obj);
        let built = (|| {
            let mut map = MapData::default();
            for elem in &drained {
                let (k, v) =
                    match elem {
                        Value::Obj(eh) => match self.heap.get(*eh) {
                            Obj::Tuple(parts) if parts.len() == 2 => (parts[0], parts[1]),
                            _ => return Err(self.err(
                                format!(
                                    "Map() expects an iterable of (key, value) 2-tuples, got {}",
                                    self.type_name(*elem)
                                ),
                                span,
                            )),
                        },
                        other => {
                            return Err(self.err(
                                format!(
                                    "Map() expects an iterable of (key, value) 2-tuples, got {}",
                                    self.type_name(*other)
                                ),
                                span,
                            ));
                        }
                    };
                let hk = self.hash_key_rooted(k, &[v], span)?;
                // last-wins upsert (mirrors the map literal + interp `map_upsert`).
                let pos = map
                    .candidates(hk)
                    .iter()
                    .copied()
                    .find(|&p| self.values_equal(map.entries[p].1, k));
                match pos {
                    Some(p) => map.entries[p].2 = v,
                    None => map.push(hk, k, v),
                }
            }
            Ok(map)
        })();
        self.pop(); // unroot the source list
        Ok(Value::Obj(self.heap.alloc(Obj::Map(built?))))
    }

    /// Collect raw bytes from a byte-sequence-shaped argument for the `bytes`/`bytearray`
    /// constructors: a `bytes`, a `bytearray` (copy), or a `List[int]` (each element 0..=255, else a
    /// recoverable fault). The `what` label names the constructor in error messages.
    fn collect_bytes_arg(&self, what: &str, v: Value, span: Span) -> Result<Vec<u8>, RuntimeError> {
        match v {
            Value::Obj(h) => {
                match self.heap.get(h) {
                    Obj::Bytes(b) => Ok(b.to_vec()),
                    Obj::ByteArray(b) => Ok(b.clone()),
                    Obj::List(items) => {
                        let mut out = Vec::with_capacity(items.len());
                        for e in items {
                            match e {
                                Value::Int(n) if (0..=255).contains(n) => out.push(*n as u8),
                                Value::Int(n) => return Err(self.err(
                                    format!(
                                        "{what}() list element {n} out of range (must be 0..=255)"
                                    ),
                                    span,
                                )),
                                other => return Err(self.err(
                                    format!(
                                        "{what}() expects a list of int, got an element of type {}",
                                        self.type_name(*other)
                                    ),
                                    span,
                                )),
                            }
                        }
                        Ok(out)
                    }
                    _ => Err(self.err(
                        format!(
                            "{what}() expects a bytes, a bytearray, or a List[int], got {}",
                            self.type_name(v)
                        ),
                        span,
                    )),
                }
            }
            other => Err(self.err(
                format!(
                    "{what}() expects a bytes, a bytearray, or a List[int], got {}",
                    self.type_name(other)
                ),
                span,
            )),
        }
    }

    /// `bytearray()` → empty; `bytearray(N)` → N zero bytes (Python); `bytearray(b)` → mutable copy of
    /// a `bytes`; `bytearray(ba)` → copy of another `bytearray`; `bytearray([ints])` → from a list of
    /// ints (each 0..=255). The MUTABLE buffer (`Obj::ByteArray`, in-place-mutated via the heap slot).
    fn builtin_bytearray(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        let bytes: Vec<u8> = match args {
            [] => Vec::new(),
            [Value::Int(n)] => {
                if *n < 0 {
                    return Err(
                        self.err(format!("bytearray() size {n} must be non-negative"), span)
                    );
                }
                // Bound the eager zero-fill: an unguarded `vec![0u8; n]` for a huge n aborts the
                // process (SIGABRT), uncatchable by `recover:`. `try_reserve` turns OOM into a
                // recoverable fault, matching range()/format-width's "never a giant abort" rule —
                // without a hard cap, so legitimately large buffers still work.
                let n = *n as usize;
                let mut buf: Vec<u8> = Vec::new();
                if buf.try_reserve_exact(n).is_err() {
                    return Err(self.err(
                        format!("bytearray() size {n} is too large to allocate"),
                        span,
                    ));
                }
                buf.resize(n, 0u8);
                buf
            }
            [one] => self.collect_bytes_arg("bytearray", *one, span)?,
            _ => {
                return Err(self.err(
                    format!("bytearray() expects 0 or 1 argument(s), got {}", args.len()),
                    span,
                ));
            }
        };
        Ok(Value::Obj(self.heap.alloc(Obj::ByteArray(bytes))))
    }

    /// `bytes(b)` → copy; `bytes(ba)` → immutable SNAPSHOT of a `bytearray`; `bytes([ints])` → from a
    /// list of ints. The conversion bridge to the IMMUTABLE form (the other being the `b"..."` literal).
    fn builtin_bytes(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        let bytes: Vec<u8> = match args {
            [one] => self.collect_bytes_arg("bytes", *one, span)?,
            _ => {
                return Err(self.err(
                    format!("bytes() expects 1 argument, got {}", args.len()),
                    span,
                ));
            }
        };
        Ok(Value::Obj(
            self.heap.alloc(Obj::Bytes(bytes.into_boxed_slice())),
        ))
    }

    fn builtin_range(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        let (start, end, step) = match args {
            [Value::Int(n)] => (0, *n, 1),
            [Value::Int(a), Value::Int(b)] => (*a, *b, 1),
            [Value::Int(a), Value::Int(b), Value::Int(s)] => (*a, *b, *s),
            _ => {
                return Err(self.err(
                    "range() expects range(end), range(start, end), or range(start, end, step) of ints"
                        .to_string(),
                    span,
                ));
            }
        };
        let items: Vec<Value> = crate::slice::range_values(start, end, step)
            .map_err(|message| self.err(message, span))?
            .into_iter()
            .map(Value::Int)
            .collect();
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
                Obj::Str(s) => s.trim().parse::<i64>().map(Value::Int).map_err(|_| {
                    self.err(format!("int(): cannot parse '{s}' as an integer"), span)
                }),
                // `int(newtype)` unwraps the inner value (the cast-unwrap path). The checker has
                // already verified the underlying is `int`, so recursing yields the inner `Int`.
                Obj::NewType { inner, .. } => {
                    let inner = *inner;
                    self.builtin_int(&[inner], span)
                }
                _ => Err(self.err(
                    format!("int() cannot convert {}", self.type_name(args[0])),
                    span,
                )),
            },
            other => Err(self.err(
                format!("int() cannot convert {}", self.type_name(other)),
                span,
            )),
        }
    }

    fn builtin_float(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        self.arity_err("float", args, 1, span)?;
        match args[0] {
            Value::Float(f) => Ok(Value::Float(f)),
            Value::Int(n) => Ok(Value::Float(n as f64)),
            Value::Bool(b) => Ok(Value::Float(f64::from(b))),
            Value::Obj(h) => {
                match self.heap.get(h) {
                    Obj::Str(s) => s.trim().parse::<f64>().map(Value::Float).map_err(|_| {
                        self.err(format!("float(): cannot parse '{s}' as a float"), span)
                    }),
                    // `float(newtype)` unwraps the inner (checker verified the underlying is float).
                    Obj::NewType { inner, .. } => {
                        let inner = *inner;
                        self.builtin_float(&[inner], span)
                    }
                    _ => Err(self.err(
                        format!("float() cannot convert {}", self.type_name(args[0])),
                        span,
                    )),
                }
            }
            other => Err(self.err(
                format!("float() cannot convert {}", self.type_name(other)),
                span,
            )),
        }
    }

    fn builtin_str(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        self.arity_err("str", args, 1, span)?;
        // `str` is dual: a `newtype N = str` with NO `str(self)` override UNWRAPS to its inner str
        // (the cast-unwrap). A `str(self)` override OR any other underlying goes through `stringify`
        // (the display cast — which itself honors the override). Mirrors the interp.
        if let Value::Obj(h) = args[0]
            && let Obj::NewType { type_key, inner } = self.heap.get(h)
            && let Value::Obj(ih) = *inner
            && matches!(self.heap.get(ih), Obj::Str(_))
            && !self
                .program
                .newtype_methods
                .get(type_key.as_ref())
                .is_some_and(|m| m.contains_key("str"))
        {
            return Ok(Value::Obj(ih));
        }
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
                _ => Err(self.err(
                    format!("ord() expects a str, got {}", self.type_name(args[0])),
                    span,
                )),
            },
            other => Err(self.err(
                format!("ord() expects a str, got {}", self.type_name(other)),
                span,
            )),
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
                .ok_or_else(|| {
                    self.err(format!("chr(): {n} is not a valid Unicode codepoint"), span)
                }),
            other => Err(self.err(
                format!("chr() expects an int, got {}", self.type_name(other)),
                span,
            )),
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
                Obj::Bytes(_) => "bytes",
                Obj::ByteArray(_) => "bytearray",
                Obj::List(_) => "List",
                Obj::Tuple(_) => "tuple",
                Obj::Map(_) => "Map",
                Obj::Set(_) => "Set",
                Obj::Struct { .. } => "struct",
                Obj::Enum { .. } => "enum",
                Obj::NewType { .. } => "newtype",
                Obj::Func { .. } | Obj::Closure { .. } => "function",
                Obj::Module { .. } => "module",
                Obj::Native { .. } => "function",
                Obj::Builtin(_) => "function",
                Obj::Cffi(_) => "function",
                Obj::Ptr(_) => "ptr",
                Obj::Channel(_) => "Channel",
                Obj::Shared(_) => "Shared",
                Obj::RwShared(_) => "RwShared",
                Obj::Atomic(_) => "Atomic",
                Obj::Executor(_) => "Executor",
                Obj::Socket(_) => "Socket",
                Obj::Listener(_) => "Listener",
                Obj::Generator(_) => "generator",
                Obj::Iter { .. } => "iterator",
            },
        }
    }

    /// `Display` form, matching `interp::value::Value`'s `Display` exactly. Thin wrapper over the
    /// depth-guarded worker — kept infallible so every error-message / `display_wire` caller is
    /// unchanged; a cyclic structure renders as `<...>` here (the print path surfaces the error).
    fn display(&self, v: Value) -> String {
        self.display_guarded(v, 0)
            .unwrap_or_else(|_| "<...>".to_string())
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
                // Python `bytes` repr `b'...'` — shared with the interp via `slice::bytes_repr`.
                Obj::Bytes(b) => Ok(crate::slice::bytes_repr(b)),
                // Python `bytearray` repr `bytearray(b'...')` — shared via `slice::bytearray_repr`.
                Obj::ByteArray(b) => Ok(crate::slice::bytearray_repr(b)),
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
                        parts.push(format!(
                            "{}: {}",
                            self.display_guarded(*k, depth + 1)?,
                            self.display_guarded(*v, depth + 1)?
                        ));
                    }
                    Ok(format!("{{{}}}", parts.join(", ")))
                }
                Obj::Set(s) => {
                    if s.entries.is_empty() {
                        Ok("Set()".to_string())
                    } else {
                        let mut parts = Vec::with_capacity(s.entries.len());
                        for (_, v) in &s.entries {
                            parts.push(self.display_guarded(*v, depth + 1)?);
                        }
                        Ok(format!("{{{}}}", parts.join(", ")))
                    }
                }
                Obj::Struct { name, fields, .. } => {
                    // Positional layout: recover declaration-order field names from the StructDef
                    // (cold display path). Snapshot name + values first to drop the heap borrow.
                    let name = name.clone();
                    let vals: Vec<Value> = fields.clone();
                    // ROOT REDESIGN — render the BARE display name (not the qualified identity key);
                    // `name` is the key the StructDef is stored under. Fall back to stripping the key.
                    let (display, names): (String, Vec<String>) = self
                        .program
                        .structs
                        .get(name.as_ref())
                        .map(|d| (d.display_name.clone(), d.fields.clone()))
                        .unwrap_or_else(|| {
                            (crate::compiler::bare_display(name.as_ref()), Vec::new())
                        });
                    let mut parts = Vec::with_capacity(vals.len());
                    for (i, v) in vals.iter().enumerate() {
                        let k = names.get(i).cloned().unwrap_or_else(|| i.to_string());
                        parts.push(format!("{k}={}", self.display_guarded(*v, depth + 1)?));
                    }
                    Ok(format!("{display}({})", parts.join(", ")))
                }
                Obj::Enum {
                    variant_id,
                    payload,
                } => {
                    // M19 lever #2 — recover the variant name from the id (cold display path).
                    let variant = self.enum_names(*variant_id).1.to_string();
                    let payload: Vec<Value> = payload.clone();
                    if payload.is_empty() {
                        Ok(variant)
                    } else {
                        let mut parts = Vec::with_capacity(payload.len());
                        for v in &payload {
                            parts.push(self.display_guarded(*v, depth + 1)?);
                        }
                        Ok(format!("{variant}({})", parts.join(", ")))
                    }
                }
                // Raw display fallback (no method dispatch here): `Name(inner)`. The `str(self)`
                // override is honored by `stringify` (the path print/`str()` actually use).
                Obj::NewType { type_key, inner } => {
                    let display = crate::compiler::bare_display(type_key.as_ref());
                    let inner = *inner;
                    Ok(format!(
                        "{display}({})",
                        self.display_guarded(inner, depth + 1)?
                    ))
                }
                Obj::Func { proto, .. } => Ok(format!("<fn {}>", self.program.protos[*proto].name)),
                Obj::Closure { .. } => Ok("<closure>".to_string()),
                Obj::Module { name, .. } => Ok(format!("<module {name}>")),
                Obj::Native { name, .. } => Ok(format!("<native fn {name}>")),
                Obj::Builtin(name) => Ok(format!("<builtin fn {name}>")),
                Obj::Cffi(c) => Ok(format!("<extern fn {}>", c.name())),
                // A raw address is non-deterministic (differs per run/engine), so never render it —
                // that would break two-engine parity if a `ptr` is printed. Only null vs live (a
                // deterministic distinction) is observable.
                Obj::Ptr(a) => Ok(if *a == 0 {
                    "<ptr null>".to_string()
                } else {
                    "<ptr>".to_string()
                }),
                Obj::Channel(core) => Ok(format!(
                    "Channel(len={})",
                    core.q.lock().unwrap().queue.len()
                )),
                // B3.1: the box holds the wire form; render it directly (`display` is `&self` and
                // cannot `from_wire`, which allocates — `display_wire` is the read-only equivalent).
                Obj::Shared(core) => Ok(format!(
                    "Shared({})",
                    self.display_wire(&core.v.lock().unwrap())
                )),
                Obj::RwShared(core) => Ok(format!(
                    "RwShared({})",
                    self.display_wire(&core.v.read().unwrap())
                )),
                Obj::Atomic(core) => Ok(format!(
                    "Atomic({})",
                    self.display_wire(&core.v.lock().unwrap())
                )),
                Obj::Executor(core) => Ok(format!(
                    "Executor(pending={})",
                    core.inner.lock().unwrap().queue.len()
                )),
                // D6: render open/closed without exposing the fd; matches no interp counterpart (net
                // is VM-only) but mirrors the core handles' structural `Display`.
                Obj::Socket(core) => Ok(format!(
                    "Socket({})",
                    if core.stream.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )),
                Obj::Listener(core) => Ok(format!(
                    "Listener({})",
                    if core.listener.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )),
                Obj::Generator(_) => Ok("<generator>".to_string()),
                Obj::Iter { .. } => Ok("<iterator>".to_string()),
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
            WireValue::Bytes(b) => crate::slice::bytes_repr(b),
            WireValue::ByteArray(b) => crate::slice::bytearray_repr(b),
            WireValue::Handle(h) => self.display(Value::Obj(*h)),
            WireValue::List(items) => {
                let inner = items
                    .iter()
                    .map(|v| self.display_wire(v))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("[{inner}]")
            }
            WireValue::Tuple(items) => {
                let inner = items
                    .iter()
                    .map(|v| self.display_wire(v))
                    .collect::<Vec<_>>()
                    .join(", ");
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
                    "Set()".to_string()
                } else {
                    let inner = entries
                        .iter()
                        .map(|(_, v)| self.display_wire(v))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{{{inner}}}")
                }
            }
            WireValue::Struct { name, fields } => {
                let inner = fields
                    .iter()
                    .map(|(k, v)| format!("{k}={}", self.display_wire(v)))
                    .collect::<Vec<_>>()
                    .join(", ");
                // ROOT REDESIGN — `name` is the qualified identity key; render the bare display name.
                let display = self
                    .program
                    .structs
                    .get(name.as_ref())
                    .map(|d| d.display_name.clone())
                    .unwrap_or_else(|| crate::compiler::bare_display(name.as_ref()));
                format!("{display}({inner})")
            }
            WireValue::Enum {
                variant_id,
                payload,
            } => {
                // M19 lever #2 — the wire form carries the id; resolve the variant name on this cold
                // display path via the shared program's `variants_by_id`.
                let variant = self.enum_names(*variant_id).1;
                if payload.is_empty() {
                    variant.to_string()
                } else {
                    let inner = payload
                        .iter()
                        .map(|v| self.display_wire(v))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{variant}({inner})")
                }
            }
            WireValue::NewType { type_key, inner } => {
                let display = crate::compiler::bare_display(type_key.as_ref());
                format!("{display}({})", self.display_wire(inner))
            }
            WireValue::Channel(core) => {
                format!("Channel(len={})", core.q.lock().unwrap().queue.len())
            }
            WireValue::Shared(core) => {
                format!("Shared({})", self.display_wire(&core.v.lock().unwrap()))
            }
            WireValue::RwShared(core) => {
                format!("RwShared({})", self.display_wire(&core.v.read().unwrap()))
            }
            WireValue::Atomic(core) => {
                format!("Atomic({})", self.display_wire(&core.v.lock().unwrap()))
            }
            WireValue::Executor(core) => format!(
                "Executor(pending={})",
                core.inner.lock().unwrap().queue.len()
            ),
            // D6: render open/closed without exposing the fd (mirrors the heap `Display`).
            WireValue::Socket(core) => {
                format!(
                    "Socket({})",
                    if core.stream.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )
            }
            WireValue::Listener(core) => {
                format!(
                    "Listener({})",
                    if core.listener.lock().unwrap().is_some() {
                        "open"
                    } else {
                        "closed"
                    }
                )
            }
            // An opaque `ptr` renders like its heap counterpart (`Obj::Ptr` → "<ptr null>"/"<ptr>");
            // never the raw address (non-deterministic across engines).
            WireValue::Ptr(a) => {
                if *a == 0 {
                    "<ptr null>".to_string()
                } else {
                    "<ptr>".to_string()
                }
            }
            // A wired first-class builtin fn renders like its heap counterpart (`<builtin fn name>`).
            WireValue::Builtin(name) => format!("<builtin fn {name}>"),
            // B3.6: a wired closure renders like its heap counterpart (`Obj::Closure` → "<closure>").
            WireValue::Closure { .. } => "<closure>".to_string(),
            // A wired cursor renders like its heap counterpart (`Obj::Iter` → "<iterator>").
            WireValue::Iter { .. } => "<iterator>".to_string(),
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
    /// `Op::ToStrFmt` — render the top-of-stack value per the parsed format spec. Scalars map
    /// straight to a [`crate::fmtspec::FmtArg`]; non-scalars are rendered via the normal
    /// `stringify_into` first (rooted on the operand stack, so a `str` method's nested frames see a
    /// live object), then formatted as a plain string. The spec's width is already capped at compile
    /// time, so no pathological allocation is possible here. Lives in its own `#[inline(never)]`
    /// helper to keep `step`'s frame small (commit 1450077).
    #[inline(never)]
    fn op_to_str_fmt(
        &mut self,
        spec: &crate::fmtspec::FormatSpec,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let v = self.stack[self.stack.len() - 1]; // leave rooted; rendering may run user code
        let mut out = String::new();
        match v {
            Value::Int(n) => crate::fmtspec::apply(spec, crate::fmtspec::FmtArg::Int(n), &mut out)
                .map_err(|m| self.err(m, span))?,
            Value::Float(x) => {
                crate::fmtspec::apply(spec, crate::fmtspec::FmtArg::Float(x), &mut out)
                    .map_err(|m| self.err(m, span))?
            }
            Value::Obj(h) if matches!(self.heap.get(h), Obj::Str(_)) => {
                let s = match self.heap.get(h) {
                    Obj::Str(s) => s.clone(),
                    _ => unreachable!(),
                };
                crate::fmtspec::apply(spec, crate::fmtspec::FmtArg::Str(&s), &mut out)
                    .map_err(|m| self.err(m, span))?;
            }
            other => {
                // Bool/Nil/containers/structs: render with the normal stringify, then treat as a
                // string for fill/align/width (type chars/precision error via `apply`).
                let mut rendered = String::new();
                self.stringify_into(&mut rendered, other, span, 0)?;
                crate::fmtspec::apply(spec, crate::fmtspec::FmtArg::Other(&rendered), &mut out)
                    .map_err(|m| self.err(m, span))?;
            }
        }
        self.pop();
        let h = self.heap.alloc(Obj::Str(out.into()));
        self.push(Value::Obj(h));
        Ok(())
    }

    fn stringify_into(
        &mut self,
        out: &mut String,
        v: Value,
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        use std::fmt::Write as _;
        // Guard against cyclic data overflowing the host stack — turns SIGABRT into a recoverable
        // `RuntimeError` (a `str` method re-stringifies at the *same* depth, so a non-recursive
        // protocol hook doesn't burn the budget).
        if depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.err(
                "maximum structural depth (10000) exceeded (cyclic data structure?)".to_string(),
                span,
            ));
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

    fn stringify_obj_into(
        &mut self,
        out: &mut String,
        h: GcRef,
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        use std::fmt::Write as _;
        // Clone the object's shape out so no heap borrow is held across the nested `&mut self` calls.
        match self.heap.get(h).clone() {
            Obj::Str(s) => out.push_str(&s),
            // `bytes` interpolates/prints as its Python `b'...'` repr (shared helper, engine-parity).
            Obj::Bytes(b) => out.push_str(&crate::slice::bytes_repr(&b)),
            // `bytearray` interpolates/prints as `bytearray(b'...')` (shared helper, engine-parity).
            Obj::ByteArray(b) => out.push_str(&crate::slice::bytearray_repr(&b)),
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
                    out.push_str("Set()");
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
                let def = self.program.structs.get(name.as_ref()).cloned();
                if let Some(def) = &def
                    && let Some(&proto) = def.methods.get("str")
                    && self.program.protos[proto].arity == 1
                {
                    let home = self.module_objs[def.module_idx];
                    let res = self.guarded(|vm| {
                        vm.run_proto(proto, home, None, vec![Value::Obj(h)], true, false, span)
                    })?;
                    return self.stringify_into(out, res, span, depth);
                }
                // Positional layout: recover declaration-order field names from the StructDef (the
                // same `def` already cloned for the `str` hook) — no per-instance name strings.
                // ROOT REDESIGN — render the BARE display name, not the qualified identity key.
                let display = def
                    .as_ref()
                    .map(|d| d.display_name.clone())
                    .unwrap_or_else(|| crate::compiler::bare_display(name.as_ref()));
                let _ = write!(out, "{display}(");
                for (i, fv) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    match def.as_ref().and_then(|d| d.fields.get(i)) {
                        Some(k) => {
                            let _ = write!(out, "{k}=");
                        }
                        None => {
                            let _ = write!(out, "{i}=");
                        }
                    }
                    self.stringify_into(out, *fv, span, depth + 1)?;
                }
                out.push(')');
            }
            Obj::Enum {
                variant_id,
                payload,
            } => {
                // `str(self) -> str` overrides the default `Variant(payload)` repr (Stringable).
                // Only a self-only method is the hook (mirrors the struct arm above).
                let key = self.enum_names(variant_id).0.to_string();
                if let Some(&proto) = self
                    .program
                    .enum_methods
                    .get(&key)
                    .and_then(|m| m.get("str"))
                    && self.program.protos[proto].arity == 1
                {
                    let home = self.module_objs[self.enum_home_module(&key)];
                    let res = self.guarded(|vm| {
                        vm.run_proto(proto, home, None, vec![Value::Obj(h)], true, false, span)
                    })?;
                    return self.stringify_into(out, res, span, depth);
                }
                // M19 lever #2 — recover the variant name from the id (cold stringify path).
                out.push_str(self.enum_names(variant_id).1);
                if !payload.is_empty() {
                    out.push('(');
                    self.stringify_seq_into(out, &payload, span, depth + 1)?;
                    out.push(')');
                }
            }
            // A newtype honors a `str(self) -> str` override (Stringable) exactly like enum/struct;
            // else it renders `Name(inner)` (its raw `Display`).
            Obj::NewType { type_key, inner } => {
                if let Some(&proto) = self
                    .program
                    .newtype_methods
                    .get(type_key.as_ref())
                    .and_then(|m| m.get("str"))
                    && self.program.protos[proto].arity == 1
                {
                    let home = self.module_objs[self.newtype_home_module(&type_key)];
                    let res = self.guarded(|vm| {
                        vm.run_proto(proto, home, None, vec![Value::Obj(h)], true, false, span)
                    })?;
                    return self.stringify_into(out, res, span, depth);
                }
                let display = crate::compiler::bare_display(type_key.as_ref());
                let _ = write!(out, "{display}(");
                self.stringify_into(out, inner, span, depth + 1)?;
                out.push(')');
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
            Obj::Builtin(name) => {
                let _ = write!(out, "<builtin fn {name}>");
            }
            Obj::Cffi(c) => {
                let _ = write!(out, "<extern fn {}>", c.name());
            }
            // Channel / Shared / Executor have no protocol hook — reuse the structural `Display`
            // (matches the interpreter's `stringify` catch-all falling back to `Display`).
            Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Ptr(_) => {
                out.push_str(&self.display_guarded(Value::Obj(h), depth)?);
            }
            // Experimental generators stringify opaquely (no protocol hook).
            Obj::Generator(_) => out.push_str("<generator>"),
            Obj::Iter { .. } => out.push_str("<iterator>"),
        }
        Ok(())
    }

    fn stringify_seq_into(
        &mut self,
        out: &mut String,
        elems: &[Value],
        span: Span,
        depth: usize,
    ) -> Result<(), RuntimeError> {
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
            Some(_) => Err(crate::native::HostError::arg_type(
                i,
                "Map[str, str]",
                "other",
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_str_list(&mut self, i: usize) -> Result<Vec<String>, crate::native::HostError> {
        match self.args.get(i) {
            // Pre-extracted on the worker (heap live) into owned strings; served back off-thread.
            Some(crate::native::NativeArg::List(items)) => Ok(items.clone()),
            Some(_) => Err(crate::native::HostError::arg_type(i, "List[str]", "other")),
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
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "int",
                self.vm.type_name(*other),
            )),
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
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "float",
                self.vm.type_name(*other),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_bool(&mut self, i: usize) -> Result<bool, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Bool(b)) => Ok(*b),
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "bool",
                self.vm.type_name(*other),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_ptr(&mut self, i: usize) -> Result<usize, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Obj(h)) => match self.vm.heap.get(*h) {
                Obj::Ptr(a) => Ok(*a),
                _ => {
                    let got = self.vm.type_name(self.args[i]);
                    Err(crate::native::HostError::arg_type(i, "ptr", got))
                }
            },
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "ptr",
                self.vm.type_name(*other),
            )),
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
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "str",
                self.vm.type_name(*other),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_struct_fields(
        &mut self,
        i: usize,
    ) -> Result<Vec<crate::native::NativeRet>, crate::native::HostError> {
        use crate::native::NativeRet as N;
        match self.args.get(i) {
            Some(Value::Obj(h)) => match self.vm.heap.get(*h) {
                // Positional, declaration-order fields (the same order the StructDef declares them).
                // Map each scalar field value to a NativeRet so the cffi layer casts it to its C
                // field width. The checker guarantees flat scalar fields.
                Obj::Struct { fields, .. } => {
                    let mut out = Vec::with_capacity(fields.len());
                    for v in fields {
                        let n = match v {
                            Value::Int(n) => N::Int(*n),
                            Value::Float(f) => N::Float(*f),
                            Value::Bool(b) => N::Bool(*b),
                            Value::Obj(fh) => match self.vm.heap.get(*fh) {
                                Obj::Ptr(a) => N::Ptr(*a),
                                _ => {
                                    return Err(crate::native::HostError::arg_type(
                                        i,
                                        "struct scalar field",
                                        "other",
                                    ));
                                }
                            },
                            _ => {
                                return Err(crate::native::HostError::arg_type(
                                    i,
                                    "struct scalar field",
                                    "other",
                                ));
                            }
                        };
                        out.push(n);
                    }
                    Ok(out)
                }
                _ => {
                    let got = self.vm.type_name(self.args[i]);
                    Err(crate::native::HostError::arg_type(i, "struct", got))
                }
            },
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "struct",
                self.vm.type_name(*other),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn invoke_callback(
        &mut self,
        arg_index: usize,
        args: &[crate::native::NativeRet],
    ) -> Result<crate::native::NativeRet, crate::native::HostError> {
        // The Chezzi closure passed as extern arg `arg_index` (a function pointer to C). Re-enter the
        // engine to run it on the C scalars, then lower its result back to a NativeRet. Same-thread,
        // synchronous: this fires inside the extern `ffi_call` on the calling (worker) thread, so it
        // is plain Rust-stack recursion through the proven `guarded`+`invoke_value` re-entry path
        // (the one map/filter/sort use). No span is available at the FFI boundary; use a zero span.
        let callee = *self
            .args
            .get(arg_index)
            .ok_or_else(|| crate::native::HostError {
                message: format!("callback argument {arg_index} is missing"),
            })?;
        let span = Span { line: 0, col: 0 };
        // Lower the C scalar args to engine `Value`s (allocations happen here, at the boundary).
        let vals: Vec<Value> = args
            .iter()
            .map(|a| self.vm.lower_native(a.clone()))
            .collect();
        let result = self
            .vm
            .guarded(|vm| vm.invoke_value(callee, vals, span))
            .map_err(|e| crate::native::HostError { message: e.message })?;
        Ok(self.vm.value_to_native_ret(result))
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
                            return Err(crate::native::HostError::arg_type(
                                i,
                                "Map[str, str]",
                                "other",
                            ));
                        };
                        let (Obj::Str(ks), Obj::Str(vs)) =
                            (self.vm.heap.get(*kh), self.vm.heap.get(*vh))
                        else {
                            return Err(crate::native::HostError::arg_type(
                                i,
                                "Map[str, str]",
                                "other",
                            ));
                        };
                        pairs.push((ks.to_string(), vs.to_string()));
                    }
                    Ok(pairs)
                }
                _ => {
                    let got = self.vm.type_name(self.args[i]);
                    Err(crate::native::HostError::arg_type(i, "Map[str, str]", got))
                }
            },
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "Map[str, str]",
                self.vm.type_name(*other),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_str_list(&mut self, i: usize) -> Result<Vec<String>, crate::native::HostError> {
        match self.args.get(i) {
            Some(Value::Obj(h)) => match self.vm.heap.get(*h) {
                Obj::List(items) => {
                    // Iterate in list order (it IS the argv). Every element must be a str.
                    let mut out = Vec::with_capacity(items.len());
                    for v in items {
                        let Value::Obj(eh) = v else {
                            return Err(crate::native::HostError::arg_type(
                                i,
                                "List[str]",
                                "other",
                            ));
                        };
                        let Obj::Str(s) = self.vm.heap.get(*eh) else {
                            return Err(crate::native::HostError::arg_type(
                                i,
                                "List[str]",
                                "other",
                            ));
                        };
                        out.push(s.to_string());
                    }
                    Ok(out)
                }
                _ => {
                    let got = self.vm.type_name(self.args[i]);
                    Err(crate::native::HostError::arg_type(i, "List[str]", got))
                }
            },
            Some(other) => Err(crate::native::HostError::arg_type(
                i,
                "List[str]",
                self.vm.type_name(*other),
            )),
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
            .map_err(|e| crate::native::HostError {
                message: e.to_string(),
            })
    }
    fn request_exit(&mut self, code: i64) {
        self.vm.pending_exit = Some(code.clamp(0, 255) as i32);
    }
}

fn is_numeric(v: Value) -> bool {
    matches!(v, Value::Int(_) | Value::Float(_))
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
        // Shortest round-trip decimal (`{}`) + an always-present `.0`: large integral floats
        // (`1.5e23`) print the shortest form that round-trips, not the exact fixed-point
        // expansion of the binary value. Rust's f64 Display never uses scientific notation.
        format!("{x}.0")
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
        Err(e) => {
            return (
                String::new(),
                Err(RuntimeError {
                    message: e.to_string(),
                    span: Span { line: 1, col: 1 },
                }),
            );
        }
    };
    let module = match parser::parse(tokens) {
        Ok(m) => m,
        Err(e) => {
            return (
                String::new(),
                Err(RuntimeError {
                    message: e.message,
                    span: e.span,
                }),
            );
        }
    };
    let program = match crate::compiler::compile_module_standalone(&module) {
        Ok(p) => p,
        Err(e) => {
            return (
                String::new(),
                Err(RuntimeError {
                    message: e.message,
                    span: e.span,
                }),
            );
        }
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
            let tokens = lexer::tokenize(&src).map_err(|e| RuntimeError {
                message: e.to_string(),
                span: Span { line: 1, col: 1 },
            })?;
            let module = parser::parse(tokens).map_err(|e| RuntimeError {
                message: e.message,
                span: e.span,
            })?;
            let program =
                crate::compiler::compile_module_standalone(&module).map_err(|e| RuntimeError {
                    message: e.message,
                    span: e.span,
                })?;
            let mut vm = Vm::new(Arc::new(program));
            vm.parallel = true;
            vm.run()
                .and_then(|()| vm.drain_live_executors(Span { line: 1, col: 1 }))
                .map(|()| vm.out)
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
                Err(e) => {
                    return (
                        Err(RuntimeError {
                            message: e.to_string(),
                            span: Span { line: 1, col: 1 },
                        }),
                        0,
                    );
                }
            };
            let module = match parser::parse(tokens) {
                Ok(m) => m,
                Err(e) => {
                    return (
                        Err(RuntimeError {
                            message: e.message,
                            span: e.span,
                        }),
                        0,
                    );
                }
            };
            let program = match crate::compiler::compile_module_standalone(&module) {
                Ok(p) => p,
                Err(e) => {
                    return (
                        Err(RuntimeError {
                            message: e.message,
                            span: e.span,
                        }),
                        0,
                    );
                }
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
            let tokens = lexer::tokenize(&src).map_err(|e| RuntimeError {
                message: e.to_string(),
                span: Span { line: 1, col: 1 },
            })?;
            let module = parser::parse(tokens).map_err(|e| RuntimeError {
                message: e.message,
                span: e.span,
            })?;
            let program =
                crate::compiler::compile_module_standalone(&module).map_err(|e| RuntimeError {
                    message: e.message,
                    span: e.span,
                })?;
            let mut vm = Vm::new(Arc::new(program));
            vm.run()
                .and_then(|()| vm.drain_live_executors(Span { line: 1, col: 1 }))
                .map(|()| vm.out)
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

/// Stdout from a stress-mode run (panics on error) — convenience for parity-under-GC tests.
#[cfg(test)]
pub fn run_capture_stress(src: &str) -> String {
    run_with(src, true)
        .0
        .unwrap_or_else(|e| panic!("unexpected runtime error under GC stress: {e}"))
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
                Err(e) => {
                    return (
                        Err(RuntimeError {
                            message: e.to_string(),
                            span: Span { line: 1, col: 1 },
                        }),
                        0,
                    );
                }
            };
            let module = match parser::parse(tokens) {
                Ok(m) => m,
                Err(e) => {
                    return (
                        Err(RuntimeError {
                            message: e.message,
                            span: e.span,
                        }),
                        0,
                    );
                }
            };
            let program = match crate::compiler::compile_module_standalone(&module) {
                Ok(p) => p,
                Err(e) => {
                    return (
                        Err(RuntimeError {
                            message: e.message,
                            span: e.span,
                        }),
                        0,
                    );
                }
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

/// Like [`run_file`], but invokes a named top-level entry function after the module loads (the
/// `module:function` manifest path). Test-only convenience over [`run_file_with_entry`].
#[cfg(test)]
pub fn run_file_entry(entry: &std::path::Path, entry_fn: &str) -> RunOutput {
    run_file_with_entry(
        entry,
        crate::native::HostConfig::default(),
        false,
        Some(entry_fn),
    )
}

/// A finished run: captured `(stdout, stderr, outcome, exit_code)`. Stderr holds `std.io.eprint`
/// output. `exit_code` is `Some(n)` only when the program called `std.os.exit(n)` (a clean halt,
/// so `outcome` is `Ok`); `None` for a normal end or a runtime error.
pub type RunOutput = (String, String, Result<(), RunError>, Option<i32>);

/// Like [`run_file`], but with an explicit [`crate::native::HostConfig`] (args/env/stdin) for the
/// native std modules. Test-only convenience over [`run_file_with_entry`] (entry-fn `None`); the
/// CLI calls [`run_file_with_entry`] directly so a `module:function` entrypoint can name a function.
#[cfg(test)]
pub fn run_file_with(entry: &std::path::Path, cfg: crate::native::HostConfig) -> RunOutput {
    run_file_engine(entry, cfg, false, None)
}

/// Like [`run_file_with`], but runs on the **B3.3-threads `--parallel` engine** (real OS-thread
/// pool + condvar `recv`). Test-only convenience over [`run_file_with_entry`]; the parity tests use
/// it to exercise the OS-thread engine.
#[cfg(test)]
pub fn run_file_parallel(entry: &std::path::Path, cfg: crate::native::HostConfig) -> RunOutput {
    run_file_engine(entry, cfg, true, None)
}

/// Resolve, compile, and run a program from its entry path on the dedicated VM thread, then — if
/// `entry_fn` is `Some` — invoke that named top-level function of the entry module (the
/// `module:function` manifest entrypoint). `None` runs the module top-level only (scripting model).
/// `parallel` selects the OS-thread engine. This is the single entry the CLI's `chezzi run` uses.
pub fn run_file_with_entry(
    entry: &std::path::Path,
    cfg: crate::native::HostConfig,
    parallel: bool,
    entry_fn: Option<&str>,
) -> RunOutput {
    run_file_engine(entry, cfg, parallel, entry_fn.map(str::to_string))
}

fn run_file_engine(
    entry: &std::path::Path,
    cfg: crate::native::HostConfig,
    parallel: bool,
    entry_fn: Option<String>,
) -> RunOutput {
    let entry = entry.to_path_buf();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || run_file_inner(&entry, cfg, parallel, entry_fn.as_deref()))
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

fn run_file_inner(
    entry: &std::path::Path,
    cfg: crate::native::HostConfig,
    parallel: bool,
    entry_fn: Option<&str>,
) -> RunOutput {
    let graph = match crate::resolver::build_graph(entry) {
        Ok(g) => g,
        Err(e) => {
            return (
                String::new(),
                String::new(),
                Err(RunError::plain(RuntimeError {
                    message: e.message,
                    span: e.span,
                })),
                None,
            );
        }
    };
    let program = match crate::compiler::compile_graph(&graph) {
        Ok(p) => p,
        Err(e) => {
            return (
                String::new(),
                String::new(),
                Err(RunError::plain(RuntimeError {
                    message: e.message,
                    span: e.span,
                })),
                None,
            );
        }
    };
    let mut vm = Vm::new(Arc::new(program));
    vm.host = cfg;
    vm.parallel = parallel;
    // On a clean finish, gracefully reap any Executor never explicitly shut down (C5 / A2). Skipped
    // on a fault (the program is already erroring) and on a hard `std.os.exit` (handled inside
    // `drain_live_executors` via `pending_exit`).
    let result = vm
        .run()
        // A `module:function` entrypoint calls the named function once the modules are initialized.
        .and_then(|()| match entry_fn {
            Some(name) => vm.invoke_entrypoint(name),
            None => Ok(()),
        })
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
mod gc_tests;
#[cfg(test)]
mod parity_tests;
#[cfg(test)]
mod tests;
