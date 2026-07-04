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

mod arith;
mod call;
mod exec;
mod netio;
mod sched;
mod stmt;

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

/// Assert two program outputs contain the SAME lines regardless of order. For a concurrency test
/// where the cooperative and M:N engines deliver the same *set* of results but the interleaving —
/// and hence line order — legitimately differs under the M:N scheduler (racing spawns / Executor
/// tasks / multi-producer channel drains). The deterministic exact order stays asserted on the
/// cooperative engine separately; this only cross-checks that M:N produced the same multiset.
#[cfg(test)]
pub fn assert_same_lines(cooperative: &str, mn: &str) {
    let mut a: Vec<&str> = cooperative.lines().collect();
    let mut b: Vec<&str> = mn.lines().collect();
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(
        a, b,
        "serial vs M:N line multiset differs\n serial:\n{cooperative}\n M:N:\n{mn}"
    );
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

/// Parallel (M:N) counterpart of [`run_program`]: parse + compile + run with `parallel = true`,
/// returning `(stdout, result)` so buffered stdout is observable even when the program faults. The
/// M:N engine is the post-interp parity oracle for the cooperative VM (both live in [`Vm`]; only the
/// scheduler differs), replacing the removed tree-walk interpreter.
#[cfg(test)]
pub fn run_program_parallel(src: &str) -> (String, Result<(), RuntimeError>) {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            let tokens = match lexer::tokenize(&src) {
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
            vm.parallel = true;
            let result = vm
                .run()
                .and_then(|()| vm.drain_live_executors(Span { line: 1, col: 1 }));
            (vm.out, result)
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

/// Parallel (M:N) counterpart of [`run_file`] with default host config — the file-based parity
/// oracle for the cooperative VM after the tree-walk interpreter's removal.
#[cfg(test)]
pub fn run_file_p(entry: &std::path::Path) -> RunOutput {
    run_file_parallel(entry, crate::native::HostConfig::default())
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
