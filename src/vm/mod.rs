//! Bytecode stack VM (M5) — the sole execution engine. Runs the [`Program`] produced by the
//! compiler on its real-thread M:N scheduler (the cooperative `--serial` scheduler and the
//! original tree-walk `interp` parity oracle have both been removed). M5a: handle-addressed
//! values, no collector yet (the mark-sweep GC lands in M5b).

mod blocking_pool;
pub mod chzstr;
pub mod core;
mod fxhash;
pub mod heap;
pub mod op;
mod poller;
mod pool;
mod quiesce;
mod timer;
pub mod value;
pub mod wire;

use core::{
    AtomicCore, AtomicIntCore, Backing, ChannelCore, ExecRegistry, ExecutorCore,
    GUARD_DEMOTE_BUDGET, GuardCycle, ListenerCore, ReaderCore, RwSharedCore, SharedCore,
    SocketCore, WriterCore, acquire_update_guard, acquire_update_guard_within,
};
use heap::{Fields, Heap, MapData, ModuleData, Obj, SetData};
use op::{CapEntry, CapSrc, NO_IC, Op, Program, ProtoId, TID_NONE, WaitMeta};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use value::{GcRef, Value, ValueView};
use wire::{WireCallFrame, WireGenState, WireValue};

use crate::ast::Span;
use crate::lexer::render_span;
#[cfg(test)]
use crate::{lexer, parser};

/// A runtime error, with the source span it occurred at.
///
/// `is_assert` distinguishes an `assert` failure (the ONE intended failure signal of a `test fn`)
/// from any other runtime fault (OOB, div-by-zero, missing key, native fault, …). It is set `true`
/// only by the `Op::Assert` arm; every other constructor leaves it `false` (the `Default`). The
/// `chezzi test` runner reads it to bucket a fault as FAIL vs ERROR. It is deliberately NOT part of
/// `Display` (which stays message+span only) and the same fault deterministically yields the same
/// flag every time.
///
/// `is_over_memory` marks a `chezzi test --max-heap` hard-abort (the runaway-allocation guard). It is
/// set at the abort site and FORCED onto whatever error emerges from the unwind, so it travels WITH
/// the error across every propagation boundary — a nested native-reentry `run_until`, and a spawned
/// worker's fault crossing back to the parent. That marker (there is no per-VM flag) is what makes the
/// abort un-catchable by `recover:` and correctly bucketed `OverMemory`: the `run_until` Err funnel
/// bypasses `recover:` whenever it is set, and `verdict_from_fault` reads it first. Like `is_assert`,
/// it is excluded from `Display`, so the error-string comparison is unaffected. Always `false` on the
/// common path (`chezzi run`, and `--max-heap` off).
///
/// `is_timed_out` marks a `chezzi test --timeout` wall-clock hard-abort. It rides the exact same
/// machinery as `is_over_memory` — set at the abort site (the loop back-edge in `jump_checked`),
/// forced back onto whatever error emerges from the unwind, and read first by `verdict_from_fault`
/// (bucket `TimedOut`). The abort site differs (a back-edge deadline check, not a GC boundary), but
/// the recover-bypass is identical: the `run_until` Err funnel bypasses `recover:` whenever it is set.
/// A wall-clock trip is non-deterministic, so this is always `false` off the `chezzi test --timeout`
/// path — which is the only place that sets a deadline at all.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RuntimeError {
    pub message: String,
    pub span: Span,
    pub is_assert: bool,
    pub is_over_memory: bool,
    pub is_timed_out: bool,
}

impl RuntimeError {
    /// Stamp the over-memory marker onto this error (see [`RuntimeError::is_over_memory`]). Used to
    /// force the marker back on after an unwind, where a `defer` that faults mid-unwind would
    /// otherwise replace the marked error with an unmarked one.
    pub(crate) fn over_memory(mut self) -> Self {
        self.is_over_memory = true;
        self
    }

    /// Stamp the timed-out marker onto this error (see [`RuntimeError::is_timed_out`]). Sibling of
    /// [`RuntimeError::over_memory`] — forces the marker back on after an unwind so a mid-unwind
    /// `defer` fault cannot strip it and let `recover:` catch the abort.
    pub(crate) fn timed_out(mut self) -> Self {
        self.is_timed_out = true;
        self
    }
}

/// W7-5 — the ONLY conditions under which an `Executor` drain still stops early. An ordinary job
/// fault no longer aborts its siblings (the drain runs every queued job and raises the lowest-index
/// fault), but a RESOURCE CAP must stay un-swallowable: a `chezzi test --max-heap` / `--timeout`
/// abort. A bound that a sibling's work can outlive is not a bound. `os.exit` is NOT here — it
/// arrives as `pending_exit`, handled by its own arm.
///
/// **W7-5d — a dead stdout is NOT here either, and must never be re-added.** It was, and it was the
/// one term that read a process-GLOBAL condition (`stream::out_dead_reason()`) inside a predicate
/// that answers "is this ERROR a hard halt": once stdout died, every fault anywhere became a hard
/// halt. What the term bought was a whole-queue kill whose shape depended on how many jobs the pool
/// had started first — measured on the `Executor` + spew + two file-writing markers shape, neither
/// marker ran at `--threads=1`, both ran at `--threads=3+`, and `--threads=2` gave
/// either answer across repeated runs. **CPython's `ThreadPoolExecutor` — the ancestor that owns
/// `Executor` semantics — runs every submitted job at `max_workers` 1/2/4**, so a broken pipe kills
/// the printer, not its siblings. (The `invoke_native` `stream_halt` gate is the same fix applied to
/// the second global read; see there.) Pinned by
/// `dead_stdout_does_not_{cancel_sibling_executor_jobs,tear_a_multi_native_sibling}_*` in
/// `tests/interactive.rs`.
///
/// **What this costs, measured, so nobody re-adds the term to buy it back.** Under a GRACEFUL
/// `shutdown()`, a job that never prints and never returns — `ex.submit(fn(): while true: j = j + 1)`
/// — used to die with the queue and now runs forever, so `chezzi run x.chz | head -1` on that program
/// hangs where it exited in 4 ms. That is run-all keeping its promise, NOT a new uncancellable job
/// class: `shutdown_now()` still kills the same job in 54 ms at `--threads=1` and at the default (a
/// loop back-edge is a cancellation point, and `shutdown_now` trips the per-core cancel flag).
/// CPython hangs on the identical `ThreadPoolExecutor` shape (measured), so this follows the owning
/// ancestor. Go exits — but by taking SIGPIPE on fd 1 and killing the process, a signal policy Chezzi
/// does not adopt ([`Vm::stream_halt`] records why: restoring SIGPIPE would break `std.net`'s
/// EPIPE-as-an-error contract — though note Go splits BY FD NUMBER, fd 1/2 signalling and every other
/// fd returning `EPIPE`, so that conflict is not actually forced). A `parallel:`/`spawn` nursery is
/// unaffected either way — structured concurrency aborts siblings on ANY fault, by design, so the
/// same program under `spawn` still terminates promptly.
///
/// Not Executor-only despite the name: it is also `reduce_task_slots`'s hard-halt-over-ordinary error
/// precedence predicate (W7-5 review Fix 1), and `reduce_task_slots` is shared by every M:N nursery
/// join (`parallel:`, `spawn`) as well as the Executor drain. Read every use of this predicate as "is
/// this fault a hard halt", not "is this an Executor".
pub(super) fn executor_hard_halt(err: &RuntimeError) -> bool {
    err.is_over_memory || err.is_timed_out
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "runtime error ({}): {}", self.span, self.message)
    }
}

/// One frame of a runtime stack trace: a function and the call site that entered it.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceFrame {
    pub function: String,
    pub span: Span,
}

/// A runtime error enriched with a stack trace, produced at the run boundary for an uncaught fault.
/// `Display` matches [`RuntimeError`] exactly (message only — the trace is printed separately).
#[derive(Debug, Clone)]
pub struct RunError {
    pub message: String,
    pub span: Span,
    pub trace: Vec<TraceFrame>,
    /// `Span::file` id → the module's source path, snapshotted from `Program::modules` at the run
    /// boundary — the last point where the compiled program is still in hand. Empty for a run whose
    /// program had no file ids (the synthetic single-module compile path) or that faulted before a
    /// `Program` existed (a resolve/compile-time error) — `render_span` falls back to the historical
    /// `line N, col M` form in either case, never a wrong or partial path.
    pub files: Vec<(u32, std::path::PathBuf)>,
}

impl RunError {
    fn from_error(
        e: RuntimeError,
        trace: Vec<TraceFrame>,
        files: Vec<(u32, std::path::PathBuf)>,
    ) -> Self {
        RunError {
            message: e.message,
            span: e.span,
            trace,
            files,
        }
    }
    fn plain(e: RuntimeError) -> Self {
        RunError {
            message: e.message,
            span: e.span,
            trace: Vec::new(),
            files: Vec::new(),
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
/// elided. Bounds deep non-recursive chains.
const TRACE_HEAD: usize = 10;
const TRACE_TAIL: usize = 10;

/// Render a runtime error plus its stack trace for the CLI: the error line, then one indented
/// `  at <function> (<call site>)` line per frame, innermost first.
///
/// Two bounding transforms keep an infinite-recursion fault from flooding ~10_001 lines (gap #8):
/// (1) runs of consecutive frames with the SAME function name collapse to the run's innermost `at`
/// line plus a `  … (× N more identical frames) …` marker when the run length N>1; (2) if the
/// collapsed line list still exceeds `TRACE_HEAD + TRACE_TAIL`, the head and tail collapsed lines are
/// kept and the middle replaced by a `  … (M frames elided) …` marker. Both transforms are no-ops on
/// small traces with distinct names, so existing exact-trace goldens are unchanged.
pub fn format_trace(e: &RunError) -> String {
    let path_for = |file: u32| {
        e.files
            .iter()
            .find(|(f, _)| *f == file)
            .map(|(_, p)| p.as_path())
    };
    let mut s = format!(
        "runtime error ({}): {}",
        render_span(e.span, path_for(e.span.file)),
        e.message
    );
    for entry in format_frames(&e.trace, &e.files) {
        s.push('\n');
        s.push_str(&entry);
    }
    s
}

/// Render a captured call trace as `at <fn> (called at <pos>)` lines: collapses consecutive
/// same-name runs into one entry with a `× N` marker, then caps to `TRACE_HEAD` + `TRACE_TAIL`
/// entries. The single producer both `format_trace` (`chezzi run`) and `chezzi test`'s fault
/// rendering call, so the collapse/cap behaviour can never drift between the two.
pub fn format_frames(trace: &[TraceFrame], files: &[(u32, std::path::PathBuf)]) -> Vec<String> {
    let path_for = |file: u32| {
        files
            .iter()
            .find(|(f, _)| *f == file)
            .map(|(_, p)| p.as_path())
    };
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
        let mut entry = format!(
            "  at {} (called at {})",
            frame.function,
            render_span(frame.span, path_for(frame.span.file))
        );
        let run = j - i;
        if run > 1 {
            entry.push_str(&format!("\n  … (× {} more identical frames) …", run - 1));
        }
        entries.push(entry);
        i = j;
    }
    // (2) Cap the collapsed entries: keep head + tail entries, elide the middle. Capping whole
    // entries (not raw lines) keeps each `× N` marker attached to its `at` line across the boundary.
    let mut out: Vec<String> = Vec::new();
    if entries.len() > TRACE_HEAD + TRACE_TAIL {
        let elided = entries.len() - TRACE_HEAD - TRACE_TAIL;
        let tail_start = entries.len() - TRACE_TAIL;
        out.extend(entries[..TRACE_HEAD].iter().cloned());
        out.push(format!("  … ({elided} frames elided) …"));
        out.extend(entries[tail_start..].iter().cloned());
    } else {
        out.extend(entries);
    }
    out
}

/// Maximum user-function call depth — infinite recursion is a clean runtime error rather than a host
/// stack overflow.
const MAX_CALL_DEPTH: usize = 10_000;

/// Maximum structural-recursion depth for value display / equality — a cyclic data structure (e.g.
/// a struct with a `List[Self]` field forming a cycle) would otherwise recurse unbounded on the
/// HOST stack and SIGABRT (uncatchable). This bound turns that into a recoverable `RuntimeError`.
///
/// `pub(crate)` so `checker::proto::EQ_BOUNDS_MAX_IN_PROGRESS` can tie itself to this value directly
/// (W7-55 Important 4) instead of repeating the literal — the checker's `Eq`-bound cap and the
/// runtime's own equality depth cap must agree BY CONSTRUCTION, or the checker can grant a compare
/// the VM then can't perform (the `checker-superset-of-compiler` soundness class).
///
/// **Every FAULTING guard on this constant tests [`Vm::walk_base`] `+ depth`, not `depth` — that is
/// the whole of W8-43.** The two guards whose over-budget branch DEGRADES instead of faulting
/// ([`Vm::cyclic_walk`] and [`Vm::snapshot_value`], the map/set key-store pair) stay on the bare
/// `depth`: charging a degrading guard converts a stack-safety measure into a silent wrong answer
/// (an ordinary shallow key inserted from inside a hook running at depth would be stored by
/// reference instead of snapshotted). Only faulting guards may share the budget. The `MAX_CALL_DEPTH` / [`VM_STACK_BYTES`] co-tuning below assumes O(1) native frames
/// per call-depth level. That holds for `run_until`, and for the `hash`/`compare` hooks (one native
/// frame per `run_proto`). It does NOT hold for a structural walk started INSIDE a protocol hook: a
/// user `eq`/`str` that compares or stringifies re-enters the VM, and before `walk_base` existed
/// each re-entry restarted this counter at 0. Neither guard then fired — call depth stayed far below
/// 10 000 while the *product* of hook-nesting depth × per-hook walk depth grew without bound — and
/// checker-clean pure Chezzi killed the process by host stack overflow (rc=134, uncatchable by
/// `recover:`, measured), which is exactly the outcome this constant's first paragraph promises to
/// prevent. `walk_base` restores the O(1)-frames-per-level assumption by making the budget ONE
/// shared allowance across the whole nest, matching CPython's single recursion budget.
pub(crate) const MAX_STRUCTURAL_DEPTH: usize = 10_000;

// M19 Tier-2 — adaptive opcode quickening (PEP 659) per-site states (see [`Vm::quicken`]).
/// Never executed yet: on first run, observe operand types and transition to `Q_INT` or `Q_GENERIC`.
const Q_COLD: u8 = 0;
/// Specialized: both operands were `Int` — take the int fast path, guarded (a non-int operand on a
/// later run deopts the site to `Q_GENERIC`).
const Q_INT: u8 = 1;
/// Deopted / polymorphic: always run the generic path. Sticky — never re-specializes, so a site that
/// sees mixed types never thrashes between fast and slow forms.
const Q_GENERIC: u8 = 2;

/// Stack size for the VM thread: the VM recurses on the host stack when a builtin/method re-enters
/// the dispatch loop (e.g. a `str`
/// method re-entering via `run_proto`), so a large dedicated stack decouples the call-depth limit from
/// the caller's thread. Co-tuned with `MAX_CALL_DEPTH` (10_000) so the depth guard fires *before* the
/// host stack overflows: the recursive frame here is `run_until` (one per call-depth level), so a new
/// dispatch arm that grows that frame eats into the margin. **That co-tuning assumes O(1) native
/// frames per call-depth level, and a structural walk nested under a protocol hook breaks the
/// assumption** — see [`MAX_STRUCTURAL_DEPTH`]'s note on `walk_base` (W8-43), which is what restores
/// it. Sized at 384 MiB (256 → 384, briefly 512,
/// back to 384 — see below) to keep headroom for per-arm growth in **debug** builds — debug frames are far larger than
/// release, and the depth-guard test (`self_referential_stringable_hits_depth_limit`) runs in debug.
///
/// **It is not only new dispatch arms that eat the margin — `sizeof` an AST/error type does too.**
/// 384 MiB stopped covering 10_000 levels while `Span` was briefly 24 bytes (`usize` line/col + the
/// `file` id, W7-49) and this was raised to 512 MiB. `Span` is now **12 bytes** (`u32` line/col/file
/// — see [`crate::lexer::Span`]), smaller than the 16 it was before W7-49, so 384 MiB is restored and
/// re-verified by `self_referential_stringable_hits_depth_limit`. Only virtual address space is
/// reserved, so a bump is free until touched — but re-measure here on any `sizeof` growth.
///
/// **It is reserved PER M:N POOL WORKER, not once** (`src/vm/pool.rs:45`, `src/vm/blocking_pool.rs:108`,
/// `src/vm/sched.rs:828`/`:1602`): 384 MiB × 12 workers is already 4.6 GiB of reservation on a
/// 12-core box. That is why raising this number to buy front-end depth margin is the LAST resort and
/// not the first — it is also the smaller of the two big stacks a front-end walk can land on
/// (`chezzi run` re-does `build_graph` + `compile_graph` here, not on the 1 GiB
/// [`crate::FRONTEND_STACK_BYTES`] thread), so it is what `parser::MAX_DEPTH` and
/// `parser::MAX_AST_DEPTH` are sized against.
const VM_STACK_BYTES: usize = 384 * 1024 * 1024;

/// Run `f` on a fresh thread with the VM's large [`VM_STACK_BYTES`] stack, returning its result.
/// The M:N engine's cooperative recursion (and any deep user recursion) needs this stack, so the
/// several `run_*_parallel` helpers spawn it inline. Exposed so the `chezzi test` runner can run
/// its M:N test pass on the same footing without duplicating the spawn boilerplate. Panics in `f`
/// propagate (the join re-panics), matching the run helpers.
pub(crate) fn on_vm_stack<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(f)
        .expect("failed to spawn VM-stack thread")
        .join()
        .expect("VM-stack thread panicked")
}

/// Configured worker count for the M:N OS-thread engine. `0` = auto (size to
/// [`std::thread::available_parallelism`]). Set once at startup from `--threads=N` /
/// `CHEZZI_THREADS` (see `main::cmd_run`), BEFORE any `parallel:` join runs — the process-wide pool
/// ([`pool`]) is a `OnceLock` created lazily on first use, so a later store would not resize it.
static WORKER_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

/// A process-wide lock serializing every test that WRITES [`WORKER_OVERRIDE`]. The override is
/// process-global and the harness runs tests on multiple threads, so an unguarded store would change
/// the worker count under every concurrent parallel test. Same shape and same reason as
/// `native::rand::TEST_RNG_LOCK`: hold it across the whole set-run-restore sequence.
#[cfg(test)]
pub(crate) static TEST_WORKER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Override the M:N engine's worker count. `0` restores auto (= `available_parallelism()`). Must be
/// called before the first parallel run; see [`WORKER_OVERRIDE`].
///
/// `#[cfg(test)]` forces [`test_baseline_worker_count`] first — see its doc for why: this is the
/// defined point that makes a test's forced count immune to the `CHEZZI_THREADS` baseline racing in
/// after it, rather than before.
pub fn set_worker_count(n: usize) {
    #[cfg(test)]
    test_baseline_worker_count();
    WORKER_OVERRIDE.store(n, Ordering::Relaxed);
}

/// Test-only: the process baseline worker count established by `CHEZZI_THREADS` for this test
/// binary — `0` (auto) if the var is unset/empty. Resolved into [`WORKER_OVERRIDE`] exactly once,
/// forced at TWO defined points rather than left to whichever thread happens to read
/// `worker_count()` first: [`worker_count`] forces it (as its own first statement, same as before)
/// for plain readers, and — this is what closes the hazard below — [`set_worker_count`] ALSO forces
/// it, before its own store, for every WRITER. That is what turns `CHEZZI_THREADS=2 cargo test` into
/// a real second-schedule differential over the whole suite (`docs/bug-discovery.md` Tier 2): every
/// test that ever reaches `worker_count()` gets the override, not just the ones that happen to run
/// late.
///
/// Returns the resolved baseline so callers that transiently override the count under
/// [`TEST_WORKER_LOCK`] can restore to it instead of hardcoding `0` — hardcoding `0` would silently
/// drop the `CHEZZI_THREADS` differential for every test that runs later in the same process, right
/// after the one that restores it.
///
/// An unparseable `CHEZZI_THREADS` panics with a clear message rather than falling back to auto: a
/// broken env var should fail the run loudly, not quietly execute the suite at `auto` while claiming
/// to gate a second schedule (deliberately stricter than `main::cmd_run`'s CLI path, which only warns
/// and keeps running the program — a differential gate that can silently do nothing is not a gate).
///
/// The init closure stores to [`WORKER_OVERRIDE`] DIRECTLY rather than through [`set_worker_count`]
/// — calling back into `set_worker_count` here would reenter this very `OnceLock::get_or_init` on the
/// thread that is still inside it, which is UB (the std docs: current implementation deadlocks).
/// That direct store is also why a test forcing a count is provably immune to this baseline, not just
/// probably immune: [`set_worker_count`] forces this `OnceLock` to finish resolving (running the
/// closure itself, or blocking on a concurrent racer who is) BEFORE it performs its own store, so by
/// the time a forced `set_worker_count(4)` returns, the baseline write — if the closure had one left
/// to do — has already happened and cannot land after it. Before this, the only synchronization was
/// [`TEST_WORKER_LOCK`], which does not exclude this `OnceLock`: the initializer runs under the same
/// lock, on the same thread, so `worker_count()` reached from inside a forced test's own body could
/// still be the FIRST call in the whole process and fire the closure there — at `CHEZZI_THREADS=1`
/// that clobbered a forced `4` down to `1` mid-test, voiding whatever the forced count existed to
/// arm.
#[cfg(test)]
static TEST_WORKER_BASELINE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn test_baseline_worker_count() -> usize {
    *TEST_WORKER_BASELINE.get_or_init(|| {
        let n = match std::env::var("CHEZZI_THREADS") {
            Ok(raw) if !raw.trim().is_empty() => {
                let s = raw.trim();
                s.parse::<usize>().unwrap_or_else(|_| {
                    panic!(
                        "CHEZZI_THREADS='{s}' is not a valid worker count for the test binary \
                         (expected a non-negative integer; 0 = all cores)"
                    )
                })
            }
            _ => 0,
        };
        if n != 0 {
            WORKER_OVERRIDE.store(n, Ordering::Relaxed);
        }
        n
    })
}

/// The effective M:N worker count: the configured override, or `available_parallelism()` when unset
/// (`0`). Always `>= 1`. Read by the pool size, the scheduler's `nworkers`, and the eager-nursery
/// gate so all three agree.
pub fn worker_count() -> usize {
    #[cfg(test)]
    test_baseline_worker_count();
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
    /// don't).
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
    /// How many argument values this frame was actually entered with — `stack.len() - base` at
    /// [`Vm::finish_frame`], i.e. BEFORE the remaining local slots are nil-reserved. Read only by
    /// [`crate::vm::op::Op::JumpIfProvided`], so the callee's own prologue can tell an OMITTED
    /// trailing argument (which it must fill from the parameter's declared default) from one the
    /// caller supplied. Computed at the single shared frame-entry point, so every door —
    /// `push_frame`, `push_frame_in_place`, generators, `spawn`, `defer`, the wire path — gets it
    /// without threading anything through the call sites.
    argc: usize,
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
/// (returns `Some(v)`) or the body ends (`None`, state → `Done`). Generators hold live VM frame
/// state, so they never cross the airlock by value (`yield` was never part of the removed
/// interpreter's surface).
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
            // `child_gcref` roots boxed floats (Float tag) too, not just true `Obj`s.
            out.extend(args.iter().filter_map(|v| v.child_gcref()));
        }
        for v in &self.ctx.stack {
            if let Some(h) = v.child_gcref() {
                out.push(h);
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
/// `^`→symmetric-difference. Selects the algebra in `Vm::set_op`.
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
        // `child_gcref` roots BOTH true `Obj`s and boxed floats (Float tag) — a deferred/spawn arg may
        // be a boxed float and must stay alive until the call runs.
        std::iter::once(head)
            .chain(args.iter())
            .filter_map(|v| v.child_gcref())
    }
}

/// A task registered by `spawn`, awaiting its nursery's join barrier (C4). The callee/receiver and
/// arguments are evaluated and deep-copied across the airlock at the `spawn` statement (Go's
/// arg-evaluation timing); the body runs at the `parallel:` dedent. A `spawn:` block is lowered to
/// a zero-arg closure, so it rides the `Call` variant.
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
        // `child_gcref` roots BOTH true `Obj`s and boxed floats (Float tag) — a deferred/spawn arg may
        // be a boxed float and must stay alive until the call runs.
        std::iter::once(head)
            .chain(args.iter())
            .filter_map(|v| v.child_gcref())
    }
}

/// A task queued on a nursery: the call itself plus the [`ModuleSnapshot`] PINNED at its `spawn`.
///
/// W6-2 — the pin is per TASK, resolved EAGERLY in [`Vm::register_task`] (never deferred to a later
/// hook), and replayed when the task is prepared (at its nursery's join, OR at a nested nursery's
/// `early_enlist_outer`). Resolving it at the `spawn` is deliberate: `spawn` is a single source
/// position every task reaches, while "the next module-slot write, else the join" is not — the M:N
/// per-connection EAGER nursery prepares a task the moment it is spawned, so a deferred pin gave a
/// different snapshot depending on worker count (diverged between `--threads=1` and higher counts).
///
/// The snapshot itself comes from [`Vm::ensure_snapshot`]'s cache, so consecutive spawns with no
/// intervening cache invalidation (a global (re)binding, or a nursery open when the view holds a
/// mutable aggregate) share ONE build — that is what keeps a spawn storm O(1) per spawn instead of
/// O(all module globals).
///
/// `Err` = the snapshot BUILD failed at the spawn (an over-deep/cyclic global, a frame-holding
/// generator). The error is CARRIED, not raised there: preparing a task is where that fault belongs,
/// so a nursery whose tasks are all cancelled without ever being prepared (`break`/`return` out of
/// `parallel:`) stays faultless, exactly as before W6-2, and the body's output still precedes it.
/// W7-4c — a crossed cell's `GcRef` paired with the wire id its BINDING travels under. Produced by
/// `deep_clone_all`, consumed by `lower_task`, so the two crossings a task makes agree on identity.
type CellIds = Vec<(GcRef, u32)>;

/// W7-4c — what one `snapshot_modules` build yields: the snapshot, the cell registry keyed by the
/// heap it was built from, and the next free id (monotonic across builds).
type SnapshotBuild = (ModuleSnapshot, Arc<fxhash::FxHashMap<GcRef, u32>>, u32);

#[derive(Clone)]
struct QueuedTask {
    call: PendingCall,
    snap: Result<Arc<ModuleSnapshot>, RuntimeError>,
    /// W7-4c — the wire ids the spawn-time `deep_clone_all` gave this task's CLONE cells, so
    /// `lower_task` serializes them under the id `snap` already uses for the same binding and the
    /// worker rebuilds ONE cell for both. Every `GcRef` here is reachable from `call` (the clone lives
    /// inside a crossed value), so `roots()` already keeps it alive and its slot can never be
    /// recycled out from under the mapping. Empty when nothing crossed, or when no snapshot was
    /// pinned yet.
    cell_ids: CellIds,
}

impl QueuedTask {
    /// The GcRefs this pending task keeps alive — see [`PendingCall::roots`].
    fn roots(&self) -> impl Iterator<Item = GcRef> + '_ {
        self.call.roots()
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
    /// Captured stdout. BYTES, not `String` (W6-9): `Writer.write_bytes` on an `io.stdout()` backing
    /// must be byte-exact like Python's `sys.stdout.buffer.write` / Go's `os.Stdout.Write`, and a
    /// `String` buffer forced a `from_utf8_lossy` hop. Decoded once, at the Rust capture boundary
    /// ([`Vm::take_out`] and the `run_*` helpers) — and NEVER where a comparison is made: a lossy
    /// decode maps `ff` and `fe` alike and would blind any byte-level comparator, so callers that need
    /// one (the CPython differential, `src/difftest/`) diff these raw bytes via [`run_file_bytes`]
    /// ([`RunOutputRaw`]) instead.
    out: Vec<u8>,
    /// Captured stderr (written by `std.io.eprint`). Separate from `out` so streams don't mix.
    stderr: Vec<u8>,
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
    /// Safe because a worker shell's `Vm` runs one OS thread with one fiber swapped in at a time — many
    /// fibers share the shell's `field_ic`, but never concurrently (no race). Each cell self-verifies,
    /// so sharing is always sound.
    field_ic: Vec<IcCell>,
    /// M19 Phase 6 / N-way poly — per-call-site method inline caches, indexed by the `ic` id baked
    /// into `CallMethod` ops (dense `0..program.method_ic_sites`). Each site is an N-way
    /// [`MethodIcSite`] holding proto ids + module indices, not `GcRef`s, so it carries no heap state:
    /// never snapshotted, never swapped in [`Vm::swap_ctx`]. Same sharing argument as `field_ic` —
    /// sequential heap-less fibers / per-worker `Vm`s, each way tid-guarded so it self-verifies.
    method_ic: Vec<MethodIcSite>,
    /// M19 Tier-2 — adaptive opcode quickening (PEP 659) state, one byte per program instruction,
    /// indexed by site `quicken_base[pid] + ip`. The un-fused generic binop arms (`Add..GtEq` reached
    /// by stack operands; `Eq`/`NotEq` always) specialize to an int/int fast path behind a deopt
    /// guard: `Q_COLD` → on first run observe operand types → `Q_INT` (int/int) or `Q_GENERIC`
    /// (sticky; never re-specializes, so a polymorphic site never thrashes). Holds only state bytes
    /// (no `GcRef`, no proto/heap handle), so it is heap-independent like `field_ic`/`method_ic`:
    /// never snapshotted, never swapped in [`Vm::swap_ctx`]. Behaviour is byte-identical to the
    /// generic (unquickened) path by construction.
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
    /// the stack (so it is deeper) and supersedes — matching Go's defer-supersedes semantics. Reset
    /// whenever a `recover:` boundary catches a fault. Read by the driver.
    fault_trace: Option<Vec<TraceFrame>>,
    fault_trace_depth: usize,
    /// Test mode: collect before *every* instruction, to surface any missing GC root.
    gc_stress: bool,
    /// Active `parallel:` nurseries (C4), innermost last. `EnterNursery` pushes; each `spawn`
    /// registers a [`QueuedTask`] on the innermost list; `JoinNursery` drains it FIFO at the
    /// dedent. Tasks are GC roots while pending. A `recover:` boundary truncates this stack back to
    /// its install-time length on catch (see [`Handler::nursery_len`]), so a fault in the nursery
    /// body or a task can't leave a stale entry.
    nurseries: Vec<Vec<QueuedTask>>,
    /// Cross-nursery flat scheduler (M:N) — parallel to [`Vm::nurseries`] (lockstep, swapped per-fiber):
    /// `Some(scope_id)` if this nursery was EARLY-ENLISTED into the one global sched (its sibling tasks
    /// already seeded as a scope so a *nested* nursery's owner can run them — the case-A cross-nursery
    /// wake), else `None` (the normal lazy path: tasks run + reduce at this nursery's own `JoinNursery`).
    mn_scopes: Vec<Option<usize>>,
    /// TASK B — parallel to [`Vm::nurseries`] (same length, pushed/popped in lockstep): the value of
    /// the current frame's `deferred.len()` captured when each nursery was entered (`EnterNursery`),
    /// i.e. the defer floor of that `parallel:` body. A recover-scoped `?` escaping a `parallel:`
    /// must run the body's defers (those at `deferred[floor..]`) BEFORE reclaiming the nursery and
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
    /// program-exit auto-drain (C5 / A2) reaps any executor never explicitly shut down.
    executors: Vec<GcRef>,
    /// Every `Executor` core created during this RUN, heap-independently — see [`ExecRegistry`].
    /// Shared (not copied) with every worker `Vm` by [`Vm::spawn_worker`], which is what lets the
    /// program-exit join reach an executor created inside a task (W7-5b). Per-run rather than
    /// process-global on purpose: the test harness runs many programs concurrently in one process, and
    /// a static would let one run's join reach into another run's executors.
    exec_registry: ExecRegistry,
    /// gaps.md W7-56 — every `MnSched` alive in this run, so a wake with NO scheduler in scope can
    /// still reach a fiber parked on one. An eager `Executor` job's `Vm` has `mn == None` AND
    /// `mn_enlist_sched == None` (`spawn_worker` sets neither — only `spawn_shell` sets `mn`), so its
    /// `send`/`close` took the no-sched branch and notified only the channel condvar; a fiber parked
    /// in `SchedCore::parked` is reachable ONLY through that sched's `wake_bucket`, and idle workers
    /// `cv.wait` untimed. The measured symptom was a job's value sitting in the queue while the
    /// nursery slept, then faulted `deadlock`. Walked by [`Vm::wake_on_send`] — the shared tail of
    /// every no-sched wake site (`send`/`close`/`trip`/bounded enqueue/bounded slot-free).
    ///
    /// `Weak`, so a finished nursery's sched drops normally and needs no deregistration; each walk
    /// prunes the dead entries. Shared (not copied) with every worker by [`Vm::spawn_worker`], and
    /// per-run for the same reason [`Vm::exec_registry`] is.
    sched_registry: SchedRegistry,
    /// Concurrency B1/B2: set by a blocking `recv` (empty channel) running inside an active nursery
    /// scheduler. It records the channel handle the running fiber is waiting on; `run_until` and the
    /// re-entrant call path break (without unwinding defers) when it is set, returning control to the
    /// scheduler so a sibling can run. Cleared by the scheduler when it resumes a fiber. It is a
    /// VM-global (not part of [`FiberCtx`]): only one fiber runs at a time per shell, so at most one
    /// suspend is pending.
    suspend: Option<GcRef>,
    /// `wait` (§6d) — the multi-channel analogue of `suspend`: the live arm-channel handles a blocking
    /// `wait:` parked the running fiber on. Set by [`Vm::op_wait_poll`]'s M:N snapshot-park, consumed
    /// by [`Vm::run_one_fiber`]'s dispatch (`Disp::WaitPark`), which files the fiber under every key
    /// so any sibling `send` re-runs the `WaitPoll` and re-polls. Mutually exclusive with `suspend`
    /// (a fiber parks via one or the other, never both). A VM-global like `suspend` (one fiber runs
    /// at a time).
    /// Each entry is `(arm-channel handle, is_send)` so the park-gap re-check applies the right
    /// readiness predicate per arm: a recv arm wakes when its channel has a value / is closed; a SEND
    /// arm wakes when its bounded channel has a free slot (a receiver popped) / is closed.
    wait_suspend: Option<Vec<(GcRef, bool)>>,
    /// Bounded-channel backpressure — the send-side analogue of `suspend`: set by a blocking `send`
    /// on a FULL `Channel[T](cap)` running inside an active scheduler. Records the channel handle the
    /// running fiber is waiting for SPACE on. Routed by the worker loop to [`MnSched::park_send`],
    /// whose gap re-check waits for the OPPOSITE condition of a `recv` park (space-available, not
    /// message-waiting). A sibling `recv` that frees a slot wakes it ([`Vm::wake_senders`]). Mutually
    /// exclusive with `suspend`/`wait_suspend` (a fiber parks via exactly one). VM-global like
    /// `suspend` (one fiber runs at once).
    send_suspend: Option<GcRef>,
    /// D5 — set when a blocking native call ([`crate::native::Kind::blocks`]) is reached under the M:N
    /// engine: instead
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
    /// W7-11 — set by [`Vm::from_wire_memo`] when a `WireValue::Backref` targets an id that is NOT in
    /// the rebuild map. That is only legitimate for a PIECEWISE drain (`RwShared`'s copy-out views take
    /// one depth-1 element of a stored wire at a time, each with its own rebuild map, so a piece whose
    /// cycle closes through the ROOT container cannot be self-contained); every other caller pairs the
    /// serialize memo's scope with the rebuild map's by construction, so a trip there is a BUG (see the
    /// `debug_assert` in [`Vm::from_wire`]). [`Vm::from_wire_piece`] clears it before each attempt and
    /// re-rebuilds the whole container when it trips. This used to be a `.expect` that ABORTED THE HOST
    /// on a legal program. Pure scratch — cleared and read inside one helper call, never across a park,
    /// so it is a `Vm` field and NOT part of [`FiberCtx`].
    wire_backref_missing: bool,
    /// D6c — live mirror of [`FiberCtx::poll_timed_out`] while the fiber is swapped in: set by the poll
    /// thread on the detached fiber's ctx when a socket op's `timeout_ms` deadline elapsed before the
    /// fd became ready, swapped in here on schedule-in. `socket_method`/`listener_method` consume it at
    /// op ENTRY (after the `run_until` loop-top cancel check, so a sibling fault still wins): if set,
    /// clear it and return `Err("timeout")` instead of retrying the syscall. Snapshot-park path only.
    poll_timed_out: bool,
    /// B1 — live mirror of [`FiberCtx::poll_deadline`]: the absolute deadline of the `Socket.read`
    /// currently in flight on this fiber. A park REWINDS `ip` and re-executes the whole op, which would
    /// recompute `now + timeout_ms` from scratch — so a read that parks more than once (the ordinary
    /// outcome when a chunk ends mid-codepoint and the rest of it has not arrived) would restart its
    /// timeout budget on every park and could block forever. Latched on the first park of a `read` and
    /// cleared when that `read` finally returns a value. `None` = no read in flight.
    poll_deadline: Option<std::time::Instant>,
    /// N3(a) — live mirror of [`FiberCtx::poll_partial`]: `Some(owed)` iff the in-flight str `read`
    /// took 1-3 bytes off the fd that completed NO codepoint (they are carried, `owed` = carry len).
    /// A subsequent timeout on that read (netpoller-park re-entry or the demote loop) then reports the
    /// poll-once `Err("incomplete utf-8: …")` classification instead of `Err("timeout")` (which is
    /// documented as "nothing arrived"). Set at the same NeedMore points as the poll-once path, twinned
    /// with [`Vm::poll_deadline`] across [`Vm::swap_ctx`] (a park re-executes the op, so the flag must
    /// survive the ip-rewind), and cleared alongside it by [`Vm::drop_poll_latch`] on completion — a
    /// stale `Some` would make the NEXT read's timeout wrongly say "incomplete". Only str `read` sets
    /// it; `read_bytes`/`write`/`accept` never do. `None` = no partial taken.
    poll_partial: Option<usize>,
    /// Depth of native (Rust) callbacks currently on the host stack that re-enter Chezzi (operator
    /// overloads, `compare`/`hash`/`str` hooks, list HOFs, sorts, `Shared.update`, the executor
    /// drain, deferred calls). Their loop / recursion state lives on the Rust stack and cannot be
    /// parked into a [`Fiber`], so a `recv` reached while this is `> 0` cannot suspend — it faults
    /// `deadlock` instead (B1 v1 limitation). Maintained by [`Vm::guarded`].
    native_reentry: usize,
    /// TICKET-016 (W8-3) — this VM's task identity in the process-global `Shared`/`RwShared` update
    /// guard registry. Sound as a TASK identity only because a fiber cannot park while
    /// `native_reentry > 0` (see that field): the fiber holding a guard cannot be swapped off this
    /// `Vm`, so no second task can ever observe or inherit this token. Assigned once at construction
    /// from a process-wide counter.
    pub guard_token: u64,
    /// Structural depth already consumed by the ENCLOSING structural walks on the host stack, when a
    /// user protocol hook (`eq` / `str`) re-entered the VM from inside one. Every FAULTING
    /// [`MAX_STRUCTURAL_DEPTH`] guard tests `walk_base + depth`, so the budget is ONE shared allowance
    /// across a chain of nested protocol-hook re-entries — CPython's model, and the only thing that
    /// bounds the PRODUCT of hook-nesting depth × per-hook walk depth. Maintained by
    /// [`Vm::guarded_walk`], which every `run_proto` dispatch with a live structural `depth` in scope
    /// goes through.
    ///
    /// Per-`Vm`, not per-fiber, for the identical reason [`Vm::native_reentry`] is: a fiber cannot
    /// park or yield while a native re-entry is on the host stack (every park/yield gate is
    /// `native_reentry == 0`), so no other fiber can ever observe a non-zero value. Deliberately
    /// absent from [`FiberCtx`], [`GenCtx`], [`Handler`], [`Vm::swap_ctx`] and the generator ctx swap
    /// ([`Vm::swap_gen_ctx`], driven by [`Vm::generator_next`]) — do not add it to any of them.
    ///
    /// The generator omission is load-bearing. The tempting cheaper alternative — charge `depth` to
    /// [`Vm::call_depth`] around each hook — FAILS because `GenCtx` carries its own `call_depth` and
    /// the generator resume swaps it, so a generator driven from inside a hook would swap the charge
    /// away. A plain `Vm` field is not swapped there, which is exactly right: the generator's frames
    /// are on the same host stack.
    ///
    /// `recover:` needs no restoration. Unlike `call_depth` (restored from the snapshot because
    /// frames are heap-allocated and skipped over), native walk frames unwind through Rust `?`, so
    /// every [`Vm::guarded_walk`] on the path has already restored by the time the handler runs.
    walk_base: usize,
    /// While `true`, [`Vm::values_equal_guarded`] does NOT dispatch a user `eq` — it compares
    /// structurally, all the way down. Set for exactly one window: `Atomic.cas`'s compare, which runs
    /// while holding the box's value mutex, where re-entering user code could block on that same
    /// `Atomic` and deadlock a non-reentrant lock with no deadlock report. The checker rejects an
    /// `Atomic[T]` payload that reaches a user `eq`, but that walk cannot see through a `Protocol`
    /// existential or an unresolved type param, so the safety property is enforced HERE and merely
    /// diagnosed there. No user code can run inside the window (that is the point), so it cannot
    /// leak across a yield — the flag is per-`Vm` and always cleared on the same statement that set it.
    eq_hook_off: bool,
    /// Monotonic count of streamed stdout writes made by this `Vm`. Read ONLY as a before/after delta
    /// around a native call, to answer "did THIS call emit to stdout" — see the `stream_halt` gate in
    /// `call.rs`'s `invoke_native`. A counter, not a flag, so a native that re-enters Chezzi
    /// (`[1].map(f)` where `f` prints) still reports the write to every frame that spans it. Never
    /// reset; wrapping is unreachable (2^64 writes).
    ///
    /// The gate is only as good as "every streamed stdout write is counted here", so the ONLY writer
    /// is [`stream::write_out`], which is also the only door to the streamed handle — one statement
    /// does both (`gaps.md` W7-5e). A native cannot emit uncounted bytes without a `&mut Vm` it would
    /// have to bump. Do NOT collapse that into a static beside `stream::OUT`: a process-global counter
    /// means a sibling thread's write during my native call fires MY halt — the same cross-job
    /// contamination W7-5d removed, one layer down. Per-`Vm` is the correct shape.
    stdout_writes: u64,
    /// B3.4: the cancel flag of the `parallel:` nursery this worker `Vm` runs under, cloned in when
    /// the worker is spawned (`sched.rs`). The first sibling to fault or `os.exit`
    /// sets it; every other worker observes it at a dispatch back-edge (loop top) or inside a
    /// blocking `recv`'s re-checking wait, and unwinds as the `cancelled` sentinel — so a faulted
    /// nursery aborts running siblings instead of join-then-report. `None` on the top-level VM
    /// (never a worker).
    cancel: Option<Arc<AtomicBool>>,
    /// The cancel flags of the ENCLOSING scopes of the scope this VM currently runs in (outermost
    /// first; empty at the top level and in the outermost nursery). Cancelling a scope must cancel its
    /// descendants, so every checkpoint reads these too ([`Vm::cancel_requested`]) — a nested nursery
    /// keeps its OWN `cancel` (an inner fault must not cancel an outer sibling) but its fibers still
    /// die when an outer scope is cancelled. Re-pointed per fiber swap-in from
    /// [`JoinScope::ancestors`], exactly like `cancel`.
    cancel_outer: Vec<Arc<AtomicBool>>,
    /// B3.4: set true only when *this* worker observed [`Vm::cancel`] and bailed, so the join can
    /// tell a swallowed cooperative abort apart from a real fault (a cancelled task is dropped, not
    /// reported). Not in [`FiberCtx`] — like `pending_exit`, cancellation is a per-VM concern.
    cancelled: bool,
    /// Set only on the worker `Vm` of an EAGERLY-dispatched `Executor` job (M:N) — to that job's own
    /// executor core. Such a worker has no nursery scheduler and no [`MnSched`], so a blocking op
    /// falls to the "no scheduler" arm of `chan_recv_step` / `send` / `wait:`, which faults
    /// `deadlock — no runnable task can send`. That verdict was true while jobs only ran at the drain
    /// (the submitter was blocked inside `shutdown()`, so nobody COULD send) and is a LIE once jobs
    /// start at `submit` — the submitter is still running and may well send next statement. When this
    /// is `Some` those arms block on the channel's own condvar instead ([`Vm::block_recv`]),
    /// matching Python.
    ///
    /// It carries the CORE (not a bare flag) because a job's outcome slot and its cooperative cancel
    /// flag both hang off it. Whether a blocked job is DEADLOCKED is no longer asked of this core —
    /// that is the process-wide verdict in [`quiesce`] (`future.md` §2d step 0), which replaced
    /// W7-12's per-executor counters.
    eager_core: Option<Arc<ExecutorCore>>,
    /// The run's registry of blocked parties — the process-wide deadlock verdict (`future.md` §2d
    /// step 0, closing `gaps.md` `W7-12r`). Shared (not copied) with every worker by
    /// [`Vm::spawn_worker`], and per-run rather than process-global for the same reason
    /// [`Vm::exec_registry`] is: the test harness runs many programs at once in one process.
    quiesce: Arc<quiesce::QuiesceState>,
    /// `chezzi test --timeout=<MS>` — the per-test wall-clock cap in ms (`0` = OFF, the default; the
    /// `chezzi run` engine never sets it). Kept only for the abort message. VM config, NOT part of
    /// [`FiberCtx`] (not swapped): armed once per invoke entry and threaded onto M:N workers.
    timeout_ms: u64,
    /// `chezzi test --timeout` — the absolute wall-clock instant this test must finish by
    /// (`now + timeout_ms`), or `None` when the cap is OFF. Observed at the loop back-edge
    /// ([`Vm::jump_checked`]); the `None` guard there short-circuits BEFORE any clock read, so a
    /// cap-off run does ZERO added `Instant::now()` calls on the hottest dispatch path. Threaded onto
    /// M:N workers as the SAME absolute instant, so a spawned task shares the parent's deadline.
    deadline: Option<std::time::Instant>,
    /// A wrapping counter throttling the SAMPLED rungs of the two CPU-side checkpoints to one in 1024,
    /// shared by both (it is a sampler, not a position — an interleaved loop and HOF simply sample more
    /// often, which is harmless):
    ///
    /// * [`Vm::jump_checked`]'s loop back-edge — the `--timeout` clock read and the W7-57 `os.exit`
    ///   flag. Ticks unconditionally there, because the exit rung needs the sample with `--timeout`
    ///   off; the clock read itself stays behind `deadline.is_some()`.
    /// * [`Vm::guarded`]'s per-element native-HOF re-entry — the `--timeout` clock read only (its exit
    ///   rung is a bare atomic load and needs no throttle). Ticks only when `deadline.is_some()`.
    ///
    /// Either way a cap-off run does ZERO added `Instant::now()` calls on either hot path.
    back_edge_tick: u16,
    /// Depth of deferred calls currently executing ([`Vm::run_one_deferred`]). A cancel is NEVER
    /// delivered while this is non-zero: a `defer` IS the cleanup a cancelled task is being unwound
    /// to run, so its body (loops, blocking ops, HOF callbacks — every cancellation checkpoint) must
    /// run to completion. Defers also drain on the NORMAL-return and own-fault paths, where
    /// `cancelled` is still false while the scope flag is already tripped by a faulted sibling —
    /// without this counter the first checkpoint inside the first deferred call would eat it.
    /// See [`Vm::cancel_requested`].
    deferring: usize,
    /// D1 — on a `--parallel` **worker** fiber, the read-only [`ModuleSnapshot`] its `module_objs` were
    /// built from and fault into its own heap lazily, one module at a time, on first global access
    /// ([`Vm::fault_module`]). `None` on the top-level VM, a fiber with no heap of its own, and a
    /// worker SHELL (which runs no code of its own — every fiber brings its own view).
    ///
    /// W6-2 — part of the [`FiberCtx`] swap group with `module_objs`/`module_faulted`: snapshots are
    /// per-TASK now, so a shell draining the global run queue can hold fibers from different scopes
    /// built from DIFFERENT snapshots. Faulting a fiber's modules from another scope's snapshot would
    /// replay the wrong values, so the snapshot travels WITH the fiber.
    module_snapshot: Option<Arc<ModuleSnapshot>>,
    /// D1 — parallel to `module_objs` on a worker VM: whether module `i` has been faulted in yet
    /// (its globals replayed from `module_snapshot`). Empty on the top-level VM and a fiber with no
    /// heap of its own.
    module_faulted: Vec<bool>,
    /// W6-2 — CACHE of the snapshot of THIS view's module graph, so consecutive `spawn`s (and repeated
    /// nurseries in a mutation-free program) build exactly one. Invalidated by exactly two rules:
    ///
    /// 1. a module-slot write — `set_global_slot`/`module_define`, the only two slot mutators;
    /// 2. `Op::EnterNursery`, iff the cached snapshot is NOT `reusable` — i.e. some global holds a
    ///    mutable aggregate, which can be mutated IN PLACE (`q.push(1)`) with no slot write for rule 1
    ///    to see. So every nursery re-snapshots such a view, while an all-immutable view keeps one
    ///    snapshot for the whole run.
    ///
    /// Swapped per fiber with `module_snapshot`: it describes the swapped-in view, not the VM. See
    /// [`Vm::ensure_snapshot`].
    snapshot_memo: Option<Arc<ModuleSnapshot>>,
    /// W7-4a — the ONE rebuild map for this view's whole snapshot replay: wire `id` → the `Obj::Cell`
    /// already built for it. `snapshot_modules` serializes every module under ONE [`WireMemo`], so a
    /// cell reached from globals in TWO DIFFERENT modules carries ONE id; the modules fault in lazily
    /// and independently ([`Vm::fault_module`]), so the map has to outlive any single fault for both
    /// to tie to one cell. Reset by `install_snapshot` (a fresh view rebuilds from scratch).
    ///
    /// Heap-keyed, like `module_objs`: its `GcRef`s index whatever heap is current while this view is
    /// (the M:N fiber's own heap), so it swaps WITH `module_snapshot`/`module_faulted` and is a GC
    /// root in `collect`.
    ///
    /// **Cells ONLY.** `from_wire_memo` registers every identity-preserved node it rebuilds
    /// (List/Map/Set/Struct/Tuple/Closure too), but only a cell can be back-referenced from a LATER
    /// module — containers live in the memo's `path`, which pops on DFS exit, so one reached again in
    /// another module is serialized fresh under a NEW id. `fault_module` therefore prunes to cells at
    /// the end of each module: retaining the rest would make the whole module-global object graph
    /// immortal for the fiber's life, since this map is `Vm`-lived AND a GC root (a task that
    /// reassigns a big global would keep the original rooted — a `--max-heap` regression).
    snapshot_rebuild: fxhash::FxHashMap<u32, GcRef>,
    /// W7-4c — the identity registry for THIS view's snapshot: every `Obj::Cell` the cached
    /// `ModuleSnapshot` gave an id to, keyed by its `GcRef` in the heap the snapshot was BUILT from.
    /// `deep_clone_all` and `lower_task` seed their memos from it, so a cell a task reaches through
    /// its OWN captures serializes under the id the module snapshot already uses for the same binding
    /// — the two crossings then rebuild ONE cell instead of two.
    ///
    /// Dropped wherever `snapshot_memo` is (it describes that exact snapshot). **Its keys are GC
    /// roots**, and that is load-bearing, not hygiene: an unrooted key could be swept and its slot
    /// recycled to a DIFFERENT cell, which would then be silently identified with the dead cell's id
    /// and merged into the wrong binding. Bounded by the module globals' cell count — clone cells go
    /// on the task (`QueuedTask::cell_ids`), never in here, so a spawn storm does not grow it.
    snapshot_cells: Arc<fxhash::FxHashMap<GcRef, u32>>,
    /// W7-4c — MONOTONIC id counter across every snapshot this VM builds; never reset, unlike the
    /// per-build `WireMemo::next_id` it seeds. A task pins the snapshot live at its own `spawn`, but a
    /// module-slot write can drop the cache and renumber before the task is prepared. With a monotonic
    /// counter a stale id simply MISSES against the new snapshot (that task degrades to two bindings,
    /// the pre-W7-4c behavior); restarting from zero would make it COLLIDE and merge two unrelated
    /// bindings — a wrong answer instead of a missed optimisation.
    snapshot_next_id: u32,
    /// W6-2 — how many snapshots this VM has BUILT (cache misses). A `usize` bump on a cold path, and the
    /// only direct probe that the cache short-circuits: a timing bench can hint, this counts. Read by
    /// `vm::tests::snapshot_cache_*`. NOT swapped per fiber — it is a per-VM statistic, not part of the
    /// module view.
    snapshot_builds: usize,
    /// D2b — set on an M:N **worker shell** to the scheduler of the `parallel:` nursery it is draining.
    /// `Some` flips the `recv`/`send` arms onto the park/wake protocol ([`MnSched`]) instead of the
    /// legacy condvar-block; `None` on the top-level VM, the inline outermost-`parallel:` builder VM
    /// (see [`Vm::mn_enlist_sched`] below — there is no scheduler loop driving the inline body), and
    /// an eager `Executor` job's `Vm` (gets neither `mn` nor `mn_enlist_sched` from `spawn_worker`).
    /// Cloned onto each shell at enlistment ([`Vm::run_mn_nursery`]).
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
    /// (`mn.is_some()`); when `mn` is `None` (the top-level VM, the inline outermost-`parallel:`
    /// builder VM, or an eager `Executor` job's `Vm`) there is no scheduler loop driving preemption, so
    /// it goes unused.
    reds: u32,
    /// D3 — transient signal: the safepoint set this when `reds` hit 0, asking the worker loop to
    /// requeue this fiber (round-robin) instead of treating its `run_until` return as a finish.
    /// Set at the safepoint, consumed in [`Vm::run_one_fiber`]; mutually exclusive with `suspend`.
    yield_now: bool,
    /// Experimental generators — transient signal: an `Op::Yield` set this, asking the generator's
    /// private `run_until` to return control to the host `.next()` call (the yielded value is on the
    /// stack top). Reset by `generator_next` before each resume. Not swapped by `swap_ctx` (it is
    /// only ever live across the single nested `run_until` that `generator_next` drives).
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
    /// again — it exits after settling its current fiber). `0` on the top-level VM and any fiber
    /// with no heap of its own (`mn_worker_loop` is the only setter).
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

/// D6b — wall-clock cap on the blocking (no-fiber-to-park) `connect` wait, so a black-hole address
/// returns a clean timeout instead of waiting out the kernel's ~2-minute connect timeout. Generous
/// (the M:N engine, which parks instead of blocking, is the real target).
///
/// W7-59 — this is the op's OWN deadline, handed to [`Vm::demote_block_socket`], and it is
/// deliberately **not** clamped by the run's `--timeout`. That loop re-reads the run deadline at the
/// top of every iteration and raises it as a HARD `Err`, while its own deadline expiring yields the
/// CATCHABLE `Err("timeout")` — so clamping the two to the same instant would not shorten anything,
/// it would only make which of the two fires a race, and half of that race is a `--timeout` a
/// `recover:` can swallow.
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
    nurseries: Vec<Vec<QueuedTask>>,
    /// Cross-nursery flat scheduler (M:N) — parallel to [`Vm::nurseries`] (lockstep): `Some(scope_id)`
    /// if this nursery was EARLY-ENLISTED into the one global sched (its sibling tasks already seeded as
    /// a scope so a *nested* nursery's owner can run them — the case-A cross-nursery wake), else `None`
    /// (the normal lazy path: tasks run + reduce at this nursery's own `JoinNursery`). When `Some`, the
    /// nursery's `tasks` vec was drained (consumed into the scope), and its `JoinNursery` reduces the
    /// recorded scope's slot sub-range instead of running the tasks — preserving the per-nursery-join
    /// flush ORDER (so non-blocking nested spawns still flush deterministically).
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
    /// `Vm::heap` when this fiber schedules in, and back out when it parks. `None` for a fiber with
    /// no heap of its own, which aliases the single `Vm::heap` instead (decision A — share-by-ref),
    /// so its swap leaves the heap untouched. No production path builds a `None` fiber today
    /// ([`Vm::into_fiber`] always passes `Some(worker.heap)`); only tests do. The `Some`/`None`
    /// discriminant also gates every D2b side-state swap below.
    heap: Option<Heap>,
    /// D2b — per-task output buffers (Decision F: each task's stdout/stderr flushes in task order at
    /// join, never interleaved live). An M:N worker shell runs many fibers in turn, so these MUST
    /// travel with the fiber rather than living on the shell `Vm`. Swapped only for M:N fibers
    /// (`heap.is_some()`); a fiber with no heap of its own keeps `Vec::new()` and aliases the shell's
    /// buffers.
    out: Vec<u8>,
    stderr: Vec<u8>,
    /// D2b / Task 1 — the fiber's module-namespace objects + lazy-fault flags (D1). Each is a `GcRef`
    /// into the fiber's OWN heap (travels via `heap` above), so a spawned task mutates its private copy
    /// of every module global. These roots swap per-fiber (`swap_ctx` swaps them UNCONDITIONALLY).
    module_objs: Vec<GcRef>,
    module_faulted: Vec<bool>,
    /// W6-2 — the snapshot this fiber's `module_objs` fault in from, and the cache of the snapshot of
    /// its CURRENT view. Both describe the module view above, so they travel with it (see
    /// [`Vm::module_snapshot`]): a shell drains fibers from several scopes, each built from its own
    /// per-task snapshot. Heap-independent ([`SnapValue`] carries no `GcRef`), so GC rooting needs
    /// nothing for them.
    module_snapshot: Option<Arc<ModuleSnapshot>>,
    snapshot_memo: Option<Arc<ModuleSnapshot>>,
    /// W7-4a — the fiber's snapshot rebuild map (see [`Vm::snapshot_rebuild`]). UNLIKE the two above
    /// it IS heap-keyed. A fiber's own heap is never traced while parked, and its map travels with the
    /// heap here.
    snapshot_rebuild: fxhash::FxHashMap<u32, GcRef>,
    /// W7-4c — the fiber's snapshot cell registry (see [`Vm::snapshot_cells`]). Heap-keyed like
    /// `snapshot_rebuild`, and travels with the heap for the same reason.
    ///
    /// `snapshot_next_id` travels WITH it, and must: the "ids are monotonic, so a stale one MISSES"
    /// guarantee is only true while the counter is at least as high as every id in the registry. Every
    /// M:N shell starts at `0` (`spawn_worker` → `Vm::new`) and one shell drains fibers from several
    /// scopes, so a registry numbered on shell A resuming on shell B would mint ids that COLLIDE with
    /// its own entries — two unrelated bindings merged into one cell, silently.
    snapshot_cells: Arc<fxhash::FxHashMap<GcRef, u32>>,
    snapshot_next_id: u32,
    /// D2b — the fiber's `Executor` handles (GC roots into its own heap; same heap-keyed argument as
    /// `module_objs`). Empty for a fiber with no heap of its own.
    executors: Vec<GcRef>,
    /// M19 Phase 3 — the fiber's `ConstStr` intern cache (GC roots into its own heap; same heap-keyed
    /// argument as `module_objs`). Travels with `heap` across [`Vm::swap_ctx`]. Empty for a fiber
    /// with no heap of its own (it aliases the shell's cache).
    str_intern: fxhash::FxHashMap<usize, GcRef>,
    /// D6b — a non-blocking `connect` parked on writability (see [`ConnectInProgress`]). Non-heap, so
    /// it carries no `GcRef` and needs no GC rooting; but it MUST travel with the fiber across the
    /// park, so it swaps in [`Vm::swap_ctx`] like the other per-fiber state. `None` unless this fiber
    /// is mid-connect (only ever set on the M:N engine).
    pending_connect: Option<ConnectInProgress>,
    /// D6c — per-socket read/accept/write timeout marker. A socket op given a `timeout_ms` parks on
    /// the netpoller with a deadline; if that deadline elapses before the fd fires, the poll thread
    /// (which owns the detached [`Fiber`]) sets this `true` and re-injects the fiber — exactly like
    /// `pending_connect`, this travels WITH the fiber across [`Vm::swap_ctx`] so the resumed op knows
    /// the wake came from a timeout, not readiness. On schedule-in it swaps into [`Vm::poll_timed_out`];
    /// the rewound socket op checks it at ENTRY (before re-running the syscall) and returns
    /// `Err("timeout")` instead of retrying. `false` whenever no timeout wake is pending. M:N-only.
    poll_timed_out: bool,
    /// B1 — the absolute deadline of the `Socket.read` in flight on this fiber (its `timeout_ms`,
    /// latched at the first park). Travels WITH the fiber across [`Vm::swap_ctx`] like `poll_timed_out`,
    /// because the whole read op re-executes on every wake: without the latch each park would recompute
    /// `now + timeout_ms` and restart the budget. Cleared when the read returns. `None` = no read in
    /// flight. M:N-only.
    poll_deadline: Option<std::time::Instant>,
    /// N3(a) — `Some(owed)` iff the in-flight str `read` took a partial codepoint off the fd (carried,
    /// `owed` bytes). Travels WITH the fiber across [`Vm::swap_ctx`] like `poll_deadline`, so the
    /// netpoller-park re-entry knows the taken-partial state after the op re-executes and reports
    /// `incomplete utf-8` rather than `timeout`. Cleared when the read returns. `None` = no partial.
    /// M:N-only.
    poll_partial: Option<usize>,
}

/// Scheduling state of a fiber on the M:N scheduler.
enum FiberState {
    /// Spawned but not yet started; holds the task to launch on first schedule.
    Pending(PendingCall),
    /// Started and runnable — resume by re-entering its `run_until`.
    Ready,
    /// Parked on an empty channel; runnable again once a sibling `send`s (the receiver handle stays
    /// rooted on the fiber's own operand stack, so this variant carries no payload).
    Blocked,
}

/// One child fiber: its saved context plus scheduling state. While the fiber is the one actively
/// running, its context lives in the live `Vm` fields and `ctx` is empty (see the scheduler).
struct Fiber {
    ctx: FiberCtx,
    state: FiberState,
    /// D2b — the fiber's stable Decision-F outcome slot. Under the cross-nursery flat scheduler this is
    /// the GLOBAL flat index into `SchedCore::slots` (= `scopes[scope_id].base_index + local_i`),
    /// assigned at nursery build / `inject`.
    task_index: usize,
    /// Cross-nursery flat scheduler (M:N) — which nursery scope this fiber belongs to (indexes
    /// `SchedCore::scopes`). Independent of `task_index` (the flat slot). Drives the scope-scoped owner
    /// stop, per-scope done accounting, and per-scope cancel. Zero for the single-nursery fast
    /// path's sole scope.
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

/// A snapshot taken at a `recover:` boundary (`Op::PushHandler`). On a caught fault the VM restores
/// the operand stack, call frames, and call-depth to these values, then jumps to `ip` in the
/// boundary's frame with the fault message pushed as the operand.
#[derive(Clone, Copy, Debug)]
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
// `value` is read only by the worker unit tests (`ReadyWorker::run`, `#[cfg(test)]`): the join
// discards each task's return value (data exits a spawn via `Shared`/`Channel`, not a return), so
// the field is dead in the bin build — hence the allow. W7-27: every bin-build producer therefore
// stores `WireValue::Nil` — a stored result is retained until the nursery join / `shutdown()`, and a
// value nothing reads is pure retention. `out`/`stderr` are live (flushed at join).
#[allow(dead_code)]
#[derive(Debug)]
struct WorkerResult {
    value: WireValue,
    out: Vec<u8>,
    stderr: Vec<u8>,
}

/// B3.2 — a spawned task lowered to a `Send` description (no parent-heap `GcRef`s), ready to rebuild
/// in a worker heap. The callee crosses as its `ProtoId` (the proto lives in the shared `Arc<Program>`)
/// plus wire'd captures/args.
/// `home`: the index of the callee's home module in the parent's `module_objs` (B3.3c), so the
/// worker can resolve the rebuilt module obj for global / sibling-fn resolution; `None` when the
/// home is a standalone module not in `module_objs` (the unit-test fixtures), which falls back to a
/// fresh empty home.
enum Lowered {
    // TICKET-016 (W8-25): NOT given a `globals` snapshot field, unlike `WireValue::Closure` and
    // `SnapValue::Closure` — this IS the direct spawn callee (a `spawn:` block or `spawn f(..)`),
    // whose home module is already comprehensively covered by `pin_snapshot`'s whole-module
    // `ModuleSnapshot` (built and shared BEFORE this crossing). Giving it its own independent
    // `gsnap` re-wires any global closure value it reads (e.g. a module-level `let` holding another
    // closure) through a SEPARATE `WireMemo`, splitting a `Cell` two crossings must share — measured
    // regression: `airlock_cross_module_shared_binding_is_one_cell` and its `w74b` siblings split a
    // shared cell / spuriously rejected a module handle when this was added. A closure reached as a
    // NESTED VALUE (a capture, a `Channel.send`, or inside the module snapshot itself) still gets its
    // own `gsnap` via the `WireValue`/`SnapValue` arms, which is what W8-25's actual repros need.
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
    /// reject `_` and could not be spawned at all.
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
/// with the worker's pre-allocated (empty) module objects. Built at a task's pin instant by
/// [`Vm::snapshot_modules`] and cached in `snapshot_memo`.
struct ModuleSnapshot {
    modules: Vec<ModuleSnap>,
    /// W6-2 — may this snapshot's cache entry SURVIVE a nursery open? True iff every module-global slot
    /// holds an immutable leaf or an `Arc`-shared core ([`Vm::slot_snapshot_reusable`]) — i.e. nothing
    /// whose CONTENTS can change without a module-slot write (the two hooked mutators `set_global_slot`
    /// / `module_define`). A module global holding a mutable aggregate (`List`/`Map`/`Set`/`Struct`/…) is
    /// mutated IN PLACE (`q.push(1)`, `m[k] = v`, `p.x = 1`) with no slot write for the hooks to see, so
    /// a snapshot holding one is dropped from the cache at every `Op::EnterNursery` and the nursery
    /// re-snapshots — conservative, never stale between nurseries. A WHITELIST, so a future `Obj`
    /// variant defaults to "rebuild" (slower, never unsound).
    reusable: bool,
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
    let mut pairs: Vec<(String, Value)> = vec![(String::new(), Value::nil()); slots.len()];
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
        /// TICKET-016 (W8-25) — the airlock's by-value snapshot of this closure's free home-module
        /// globals (`Proto::global_free`), `(slot, snapped value)` pairs.
        globals: Vec<(u32, SnapValue)>,
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
    /// A native (Rust) fn — re-allocated with the same fn pointer (`NativeFn` is `Clone`/`Send`) and
    /// the same [`crate::native::Kind`] (a `Copy` field of its registry entry, so the rebuilt value
    /// keeps running the way the entry says).
    Native {
        name: Box<str>,
        func: crate::native::NativeFn,
        kind: crate::native::Kind,
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
    /// An `Obj::Cell` (a by-reference-captured local's box) embedding a handle — its inner snapped
    /// recursively, replayed as ONE independent cell per BINDING on the worker (design §4 F1). A
    /// pure-data cell takes the `to_wire` fast path above (`SnapValue::Wire(WireValue::Cell { .. })`).
    ///
    /// W7-4b — `id` is minted from the SAME [`WireMemo`] the wire arms use, so a binding reached down
    /// both the fast (`Wire`) and slow (here) paths keeps one identity. A second reach emits
    /// [`Backref`](SnapValue::Backref); `replay_snap` dedupes first-wins by id.
    Cell {
        id: u32,
        inner: Box<SnapValue>,
    },
    /// W7-4b — a second reach of an already-emitted `Cell` id (an off-stack sibling closure, or a
    /// letrec back-edge). The wire mirror of [`WireValue::Backref`], and it degrades the same way: an
    /// id the rebuild map has never seen resolves to `nil` and flags `wire_backref_missing` (W7-11).
    ///
    /// That degradation is a LAST resort, not a supported outcome — a `nil` where a cell belongs
    /// reaches `CellLoad on a non-handle value`. A miss means the serialize memo's scope stopped
    /// matching the rebuild map's, so [`Vm::fault_module`] owns the flag around its replay and
    /// `debug_assert`s on it (and clears it, so the miss is never charged to the next unrelated
    /// `from_wire` caller).
    Backref(u32),
    /// `(cached hash, key, value)` triples — hashes are value-derived, so they carry over unchanged.
    Map(Vec<(u64, SnapValue, SnapValue)>),
    /// `(cached hash, element)` pairs.
    Set(Vec<(u64, SnapValue)>),
}

/// B3.4 — how a `--parallel` task ended, recorded in its slot. The join ([`Vm::reduce_task_slots`])
/// scans these in task order: `Done`/`Exit` flush their buffered output; the lowest-index `Exit` or
/// `Fault` propagates (an `Exit` hard-halts the parent, a `Fault` unwinds normally so an outer
/// `recover:` can catch it); `Cancelled` is swallowed (a sibling-abort, its partial output dropped).
/// The terminal (lowest-index propagating) `Fault` ALSO flushes its buffered output at its task-order
/// slot instead of dropping it, so a faulting task's already-printed partial output survives.
/// Higher-index racy faults and `Cancelled` still drop (no deterministic slot).
#[derive(Debug)]
enum TaskOutcome {
    /// Ran to completion. Its return value crossed the airlock; output flushed in task order.
    Done(WorkerResult),
    /// Observed the nursery cancel flag and unwound (a sibling faulted/exited first). Its buffered
    /// output is FLUSHED at its task-order slot, not dropped: with cancellation points a started task
    /// always runs its prologue, so those bytes really were printed, and dropping them here would
    /// silently un-print output the program genuinely produced.
    Cancelled { out: Vec<u8>, stderr: Vec<u8> },
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
    /// W7-60 — the buffered `(stdout, stderr)` this outcome carries, whatever its variant. Every
    /// variant owns a pair and `reduce_task_slots` flushes all five unconditionally (W7-5c), so a
    /// caller that wants only the OUTPUT — the bail-out path in [`Vm::join_eager_jobs`], which must
    /// not also propagate a finished job's fault over the halt that is already unwinding — needs one
    /// accessor rather than a second five-arm `match` that could drift from the first.
    fn streams(&self) -> (&[u8], &[u8]) {
        match self {
            TaskOutcome::Done(wr) => (&wr.out, &wr.stderr),
            TaskOutcome::Cancelled { out, stderr }
            | TaskOutcome::Exit { out, stderr, .. }
            | TaskOutcome::Fault { out, stderr, .. }
            | TaskOutcome::Deadlocked { out, stderr, .. } => (out, stderr),
        }
    }
}

// B3.3-threads had a `TaskSlots` alias + a `DoneSignal` completion guard here, both owned by the
// batch farm/join helper `run_workers_on_pool`. Eager `Executor` execution retired that helper (its
// last caller was the `Executor` drain — the legacy `run_parallel_nursery` was already gone, and the
// M:N nursery joins through `MnSched`), so both went with it. The panic-safety invariant they existed
// for is unchanged and now lives inline in `Vm::dispatch_eager_job`: a Rust panic in a job's worker VM
// becomes a `Fault` slot instead of leaving `EagerState::outstanding` short and hanging `shutdown`
// forever. Covered by `executor_faulting_job_does_not_hang_shutdown`.

/// The `deadlock` fault message raised by the M:N detector ([`MnSched::take_runnable`]).
const DEADLOCK_MSG: &str = "deadlock: every task in this parallel: block is blocked on a channel it \
     cannot proceed on (an empty recv() or a full send()) and no sibling can unblock it — the nursery \
     cannot progress";

/// gaps.md W7-58 residual — the fault a thread blocked in an `Executor` join takes when the
/// process-wide verdict says nothing in the run can move again. See [`Vm::join_eager_jobs`].
const JOIN_DEADLOCK_MSG: &str = "waiting for this Executor's jobs: deadlock — every task in this run \
     is blocked and none of them can make progress, so no job can ever finish";

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
    ///
    /// §2c1 — `Some` exactly when this scope OWNS `sched` (it is the OUTERMOST eager nursery on its
    /// thread, so it built the sched and the drainer, and tears both down at its join). A NESTED eager
    /// nursery on the same thread is a *scope* on the enclosing scope's sched and has no drainer of its
    /// own — the owner's one drainer serves every scope, because it drains the GLOBAL queue.
    ///
    /// It is never `None` for an owner: `activate_eager_nursery` returns `None` rather than build a
    /// drainer-less owner, so the caller falls back to the lazy queue-at-join path instead of leaving
    /// a nursery with no worker at all during its body.
    drainer: Option<std::thread::JoinHandle<()>>,
    /// §2c1 — this nursery's scope id on `sched`. `0` for an owner; a fresh appended scope for a
    /// nested eager nursery sharing the owner's sched.
    ///
    /// **Why nesting must share ONE sched.** Two sibling nurseries on two private scheds cannot wake
    /// each other: `send_wake` scans its own sched and then `wake_parent_chain`, which is strictly
    /// upward, so there is no sideways or downward path. Giving each nursery its own sched
    /// reintroduced exactly the cross-nursery deadlock the flat scheduler was built to kill —
    /// measured, `examples/parallel_cross_nursery_{circular,fanout}.chz` both faulted
    /// `deadlock: every task in this parallel: block is blocked…`. One sched with one scope per
    /// nursery restores it: the inline owner drains the GLOBAL queue, so it runs a sibling scope's
    /// fiber, and `is_deadlocked` sees the enclosing scope's still-open body and vetoes.
    scope: usize,
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
struct WaitPark {
    /// The single parked fiber, taken exactly once by the winning CAS. `None` after it is claimed.
    fiber: Mutex<Option<Fiber>>,
    /// Every bucket key this token was filed under — the sweep set the winner removes itself from.
    keys: Vec<usize>,
    /// Wake-once gate: the first waker to win the CAS owns the fiber; all later wakers see it set and
    /// drop their (stale) token. Distinct from `parked_n` (a fiber count, not a key count).
    claimed: AtomicBool,
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
enum ParkedEntry {
    Recv(Fiber),
    Wait(Arc<WaitPark>),
}

struct MnSched {
    core: Mutex<SchedCore>,
    cv: Condvar,
    // N4 — the legacy sched-level `cancel` field is GONE. It held only the OUTERMOST nursery's flag, so
    // every read of it was a latent bug for a nested/enlisted scope: `park`/`park_wait` had already moved
    // to the per-fiber `scopes[fiber.scope_id].cancel`, and its last reader (the netpoller's `register`,
    // via `poll_park_offload`) now takes that same per-scope flag. The outermost cancel lives on
    // `scopes[0].cancel` like every other scope's.
    /// The prebuilt `deadlock` fault, cloned into every still-parked fiber's slot when the predicate
    /// fires, so the join's lowest-index-fault reduce propagates it (`DEADLOCK_MSG`). The parked
    /// fiber's OWN buffered stdout travels into its slot alongside this error (see `flag_deadlock`),
    /// so a parked task's partial output flushes at its task-order slot like a real fault would.
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
    /// W7-26r — the run's `--max-heap` cap (`0` = off), copied from the creating VM's heap. Uniform
    /// per run (`spawn_worker` gives every worker heap the parent's cap), so one field is the whole
    /// story. Read by `finish` to decide a scope's retained-backlog verdict — the observation site
    /// the blocked joining fiber cannot provide.
    mem_cap: usize,
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
    /// gaps.md B5 — the ANCESTOR sched to also wake on a `send`/`close`. `None` for every ordinary
    /// sched (top-level + lazy nested nurseries all share ONE global `MnSched`). Set ONLY on an EAGER
    /// nested nursery's PRIVATE sched (`activate_eager_nursery`), where it points at the sched the
    /// activating worker fiber was running on (its parent nursery). A `send`/`close` inside an eager
    /// body pushes the value into the SHARED `ChannelCore` but `wake_bucket` only scans the eager
    /// sched's own `parked` set — so a receiver parked in the PARENT nursery on that channel is never
    /// made runnable, and the parent quiesces to a spurious `deadlock`. `send_wake`/`close_wake` walk
    /// this chain (strictly UPWARD — no cycle, no ABBA) to requeue the parent's parked receiver onto
    /// its home sched. Value already in the shared queue → the woken receiver pops it (no double
    /// consume); an over-wake (receiver finds the queue empty and re-parks) is the already-tolerated
    /// pattern. Points UP only: parent→child ("into" an eager body) is a documented residual limit.
    parent_wake: Option<Arc<MnSched>>,
    /// gaps.md W7-56 — the run's [`ExecRegistry`], so [`MnSched::is_deadlocked`] can see an eager
    /// `Executor` job as a live, UNCOUNTED feeder. The predicate's counters model fibers of THIS
    /// sched only; an `ex.submit(f)` job runs on the shared pool with no fiber, no `runnable`, no
    /// `inflight` — so a nursery whose only task parks on a channel that job is about to feed
    /// quiesces and faults a healthy program. Same veto `quiesce::QuiesceState::quiesced` already
    /// applies process-wide (`parties.len() < live`), for the same reason.
    ///
    /// Assigned AFTER construction (like `parent_wake`) rather than through `new`, so the predicate's
    /// unit fixtures keep an empty registry — i.e. today's behaviour, which is what they test.
    exec_registry: crate::vm::core::ExecRegistry,
    /// gaps.md W7-58 — the run's [`quiesce::QuiesceState`], so an idle worker of this sched can JUDGE
    /// the process-wide verdict. A nursery OWNER never reaches [`Vm::block_halt_check`] (it sits in
    /// `mn_worker_loop`, not in a blocking native), so a run whose only stuck parties are nursery
    /// owners has no polling judge at all and would hang with the party registration alone.
    ///
    /// Assigned AFTER construction (like `parent_wake`/`exec_registry`) rather than through `new`, so
    /// the predicate's unit fixtures keep an empty state — i.e. today's behaviour, which is what they
    /// test (an empty registry means `live == 1` and `parties` empty, so `quiesced` is never reached
    /// past its count gate).
    quiesce: Arc<quiesce::QuiesceState>,
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
    /// W7-26r — the bytes this scope's finished tasks have RETAINED in `SchedCore::slots` (buffered
    /// stdout/stderr, held to the join for the task-order flush). Accumulated by `finish` and
    /// compared against `MnSched::mem_cap` there, because these slots live outside every `Heap` and
    /// so are reachable by `Heap::live_bytes` NOWHERE — a `parallel:` of 300 tasks each printing
    /// ~1 MB measured PASS at 733 MB against an 8 MB cap. Per-scope, not per-sched: a nested nursery
    /// reduces (and frees) its own slots at its own join.
    bytes: usize,
    /// `true` while an EAGER nursery's body is still running (between `EnterNursery` and `JoinNursery`)
    /// and may still `inject` more tasks. While set, a transient `done == total` for this scope must NOT
    /// terminate the global sched and `is_deadlocked` is vetoed (the body is live work the sched can't
    /// see). `JoinNursery` clears it. Always `false` for a lazy (queue-at-join) nursery.
    body_open: bool,
    /// §2c1 — `true` while this eager scope's body thread is itself BLOCKED in place (parked on a
    /// channel / `wait:` / an executor join — every counted-party block funnels through
    /// [`super::Vm::block_party_guard`], which sets and clears this).
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
    body_blocked: bool,
    /// Cross-nursery flat scheduler — `true` while this scope is an EARLY-ENLISTED outer nursery still
    /// awaiting the inline builder's own `JoinNursery` (`early_enlist_outer` sets it; `join_enlisted_scope`
    /// / `abort_enlisted_scope` clear it as the builder begins draining the scope). While set, the scope's
    /// parked fibers have a live external feeder — the builder body, which may still `send`/`close`/`spawn`
    /// — so a quiesce in which EVERY incomplete scope is `awaiting_builder` is NOT a deadlock: the builder
    /// has finished all nested service and will return to the body to feed them (see
    /// `all_incomplete_awaiting_builder` + `is_deadlocked`).
    ///
    /// §2c1 — an EAGER scope now raises it too, for the span in which its body is parked in a NESTED
    /// nursery's join (`Vm::blocked_bodies_guard(true)`). The meaning is identical: a builder will
    /// return to this scope and may then `send`/`close`, so a quiesce in which the inner scope is DONE
    /// is not a deadlock. It stays `false` for a body blocked on a CHANNEL — that body resumes only if
    /// somebody feeds it, so it promises no progress.
    awaiting_builder: bool,
    /// This scope's cancel token (the SAME `Arc` cloned onto fibers running in this scope; distinct
    /// per nursery so an inner fault cancels ONLY its scope, never an outer sibling — the structured-
    /// concurrency invariant). Read by `park`/`park_wait`'s gap re-check and the running fiber's
    /// back-edge (via the shell's re-pointed `self.cancel`) and `cancel_drain(scope_id)`.
    cancel: Arc<AtomicBool>,
    /// The cancel tokens of every ENCLOSING scope, outermost first (empty for the outermost nursery).
    /// Structured concurrency: cancelling a scope cancels its DESCENDANT scopes. A nested nursery keeps
    /// its own `cancel` (so an inner fault never cancels an outer sibling — the other half of the
    /// invariant), but its fibers must still observe an outer cancel at their checkpoints, or a nested
    /// nursery entered from a task that is later cancelled becomes UNCANCELLABLE and a spinning
    /// grandchild hangs the teardown forever. Read at every checkpoint via the shell's re-pointed
    /// `Vm::cancel_outer` (`Vm::cancel_requested`).
    ancestors: Vec<Arc<AtomicBool>>,
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
    /// N4 (demoted half) — the cancel flags each DEMOTED fiber that a CANCEL can still wake is
    /// watching (its own scope's flag + `Vm::cancel_outer`), keyed by a demote token. CANCEL is a
    /// wakeup source the park/inflight/`runnable` counters do not model: [`Vm::demote_recv_block`]
    /// ranks `cancel_requested()` ABOVE its own deadlock self-detect, so a demoted fiber whose flag
    /// is set resumes within one `DEMOTE_POLL_BACKOFF`, unwinds, and runs its `defer`s (which can
    /// `send` — waking parked siblings). Declaring deadlock against it would latch `terminate` and
    /// truncate that cleanup. Only a fiber that a cancel WOULD honour is registered (`!cancelled &&
    /// deferring == 0` at demote time — neither can change while it is blocked), so the fiber that
    /// is demoted-blocked forever INSIDE its own uncancellable `defer` registers nothing and stays a
    /// genuine deadlock. Registered/un-registered under core lock A, 1:1 with `blocked_native`.
    demote_cancel_watch: std::collections::HashMap<u64, Vec<Arc<AtomicBool>>>,
    next_demote_tok: u64,
    /// §2c1 — the waits of every BLOCKED BODY on this sched's thread (`Vm::block_party_guard`).
    ///
    /// A body that is parked on a channel is very often the RENDEZVOUS PARTNER of one of this sched's
    /// own fibers, and the counter-only predicate cannot see it — so lifting the `body_open` veto for
    /// a blocked body (`JoinScope::body_blocked`) is only sound once the body's wait is visible HERE.
    /// `is_deadlocked_ignoring_jobs` vetoes while any of these is satisfiable.
    ///
    /// **It carries the wait, not the channel, because DIRECTION decides satisfiability.** The
    /// pre-existing `demoted_chans` peek asks `!q.is_empty()`, which is the RECEIVER's question; for a
    /// body blocked on a full `send` the answer is inverted — an empty queue means it can proceed.
    /// Reusing that peek killed a live consumer 12 runs in 12 on `Channel[int](1)` with the sender in
    /// the body. `PartyWait::satisfiable` already answers both directions, so it is what is stored.
    body_waits: Vec<Arc<crate::vm::quiesce::PartyWait>>,
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

    /// §2c1 — the DEADLOCK predicate's half of [`Self::any_body_open`]: any scope whose eager body can
    /// still `inject`. A body that is itself BLOCKED in place cannot reach another `spawn`, so it is
    /// not live work and must not veto the verdict (see [`JoinScope::body_blocked`]). Terminate keeps
    /// asking `any_body_open`, which ignores this flag.
    fn any_body_injecting(&self) -> bool {
        if self.scopes.len() == 1 {
            return self.scopes[0].body_open && !self.scopes[0].body_blocked;
        }
        self.scopes.iter().any(|s| s.body_open && !s.body_blocked)
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

    /// N4 — some still-incomplete scope has its `cancel` tripped, i.e. is MID-TEARDOWN: a fault/exit/
    /// abort has cancelled it and its parked fibers are about to be requeued by `cancel_drain` so they
    /// can unwind their `defer`s. Vetoes the deadlock predicate (see [`MnSched::is_deadlocked`]).
    ///
    /// A cancel trip and its `cancel_drain` are TWO SEPARATE core-lock acquisitions apart at every seam
    /// that trips one — there are exactly THREE (the only scope-cancel stores in the VM):
    /// `Vm::trip_cancel` (exec.rs, from `classify_mn_outcome`'s fault/exit AND `run_one_fiber`'s
    /// panic-fault fallback) followed by `mn_worker_loop`'s `finish` → `cancel_drain`;
    /// `abort_enlisted_scope`; and `abort_eager_nursery` (sched.rs). (The two demote self-detect loops
    /// only READ a cancel — they trip none.) An idle worker's `take_runnable` landing in such a gap sees
    /// the pre-drain quiesce (`running == 0 && runnable == 0 && parked_n > 0`) and, without this veto,
    /// calls the teardown a DEADLOCK — `flag_deadlock` then DROPS the still-parked siblings without
    /// `unwind_deferred`, silently skipping their `defer`s.
    ///
    /// The two abort seams also clear a PRE-EXISTING veto of their own (`awaiting_builder` /
    /// `any_body_open`); both trip the scope cancel FIRST (`MnSched::trip_scope_cancel`, a store under
    /// the core lock) so the veto handoff is GAPLESS — one veto is armed before the other is dropped.
    /// The store must stay under the core lock: it is what publishes the flag to any worker that later
    /// takes that lock to evaluate the predicate (a bare `Relaxed` store outside it has no
    /// synchronizes-with edge). On the fault path the edge is `finish`'s own lock release (the predicate
    /// needs `running == 0`, which only holds after it).
    ///
    /// BOUNDED TO THAT WINDOW (this is the whole liveness argument): the veto asks for an UNDRAINED
    /// PARKED fiber of the cancelled scope, not merely `done < total`. "Some incomplete cancelled scope"
    /// was too wide and could never lift: a `defer` is not itself cancellable (`cancel_requested`'s
    /// `deferring == 0` term), so a cleanup body that blocks FOREVER (`ch.recv()` nobody will ever
    /// answer) demotes and sits there — the scope is then incomplete *because of* that fiber, the veto
    /// held forever and the M:N engine hung SILENTLY (serial reports it). The harm the veto exists to
    /// prevent is only possible while parked fibers are still owed their `cancel_drain`; once drained
    /// they are in `global` (`runnable > 0`), so the predicate is false on its own terms and the veto is
    /// no longer needed. A cancelled scope cannot RE-accumulate parked fibers after its drain: every
    /// park path re-checks THIS scope's cancel (`park`/`park_wait` re-read
    /// `c.scopes[fiber.scope_id].cancel` under the core lock and requeue `Ready` instead of parking;
    /// `poll_park_offload` hands the netpoller's `register` that same per-scope flag, which rejects the
    /// park under the registry lock `drain_sched` sweeps under) — pinned by
    /// `mnsched_park_requeues_when_cancel_tripped` and `poll_park_rejects_cancelled_inner_scope`. The
    /// NETPOLLER half of the drain window needs no veto at all: a poll-parked fiber is deliberately NOT
    /// in `parked` and `poll_park_offload` accounts it running→`inflight`, and `is_deadlocked` already
    /// requires `inflight == 0` (if that accounting ever changes, this argument changes with it).
    /// A cancelled scope whose last unsettled fiber is DEMOTED-blocked forever inside its own cleanup is
    /// therefore a REAL deadlock, and `demote_recv_block`'s self-detect reports it instead of hanging
    /// (`mnsched_cancelled_scope_whose_only_fiber_is_demoted_is_deadlock`).
    fn any_cancelled_scope_awaiting_drain(&self) -> bool {
        self.scopes.iter().enumerate().any(|(sid, s)| {
            s.done < s.total
                && s.cancel.load(Ordering::Relaxed)
                && self.scope_has_undrained_park(sid)
        })
    }

    /// Is any fiber of scope `sid` still sitting in `parked`, i.e. still owed its `cancel_drain`?
    /// Scans the same structure `cancel_drain` empties, in the same way (a `Recv` entry's scope by
    /// reference; a `Wait` token's PEEKED under its fiber lock without claiming), under the same core
    /// lock — so no new lock order, and no new state to keep in sync (a flag would have to be
    /// set/cleared at every trip + drain seam; a missed clear re-creates the hang).
    fn scope_has_undrained_park(&self, sid: usize) -> bool {
        self.parked.values().flatten().any(|e| match e {
            ParkedEntry::Recv(f) => f.scope_id == sid,
            ParkedEntry::Wait(wp) => wp
                .fiber
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|f| f.scope_id == sid),
        })
    }

    /// Cross-nursery flat scheduler — some scope has unfinished tasks (the `done < total` half of the
    /// global deadlock predicate). Fast path for the common single-nursery case.
    pub(super) fn any_scope_incomplete(&self) -> bool {
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

    /// N4 (demoted half) — start watching a demoted fiber's cancel flags. `flags` is `Vm::cancel`
    /// followed by `Vm::cancel_outer` and is passed EMPTY when a cancel could not wake this fiber
    /// anyway (it is already unwinding, or it is blocked inside a `defer` — `cancel_requested()`'s
    /// `!cancelled && deferring == 0` terms, neither of which can change while it is blocked). Returns
    /// the token to hand back to `unwatch_demoted_cancel`. Caller holds core lock A.
    fn watch_demoted_cancel(&mut self, flags: Vec<Arc<AtomicBool>>) -> u64 {
        let tok = self.next_demote_tok;
        self.next_demote_tok += 1;
        if !flags.is_empty() {
            self.demote_cancel_watch.insert(tok, flags);
        }
        tok
    }

    /// Stop watching a demoted fiber (it resumed / faulted / settled). Caller holds A.
    fn unwatch_demoted_cancel(&mut self, tok: u64) {
        self.demote_cancel_watch.remove(&tok);
    }

    /// N4 (demoted half) — does some demoted fiber have a tripped cancel flag, i.e. is it about to
    /// resume and unwind? That is live progress the deadlock predicate's counters cannot see, so it
    /// VETOES the fire (see [`MnSched::is_deadlocked`]). Self-lifting: the entry disappears the moment
    /// the fiber leaves `demote_recv_block`/`demote_wait_block`, on every exit path.
    fn any_demoted_cancel_pending(&self) -> bool {
        self.demote_cancel_watch
            .values()
            .any(|flags| flags.iter().any(|f| f.load(Ordering::Relaxed)))
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
    /// for the outermost nursery's `total` tasks + its `cancel` (which lives on `scopes[0]` like every
    /// other scope's — there is no sched-level cancel; `park`/`park_wait`/`cancel_drain`/`poll_park_offload`
    /// all use the PER-FIBER scope cancel).
    fn new(
        total: usize,
        nworkers: usize,
        cancel: Arc<AtomicBool>,
        deadlock_err: RuntimeError,
        mem_cap: usize,
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
                    bytes: 0,
                    body_open: false,
                    body_blocked: false,
                    awaiting_builder: false,
                    cancel: Arc::clone(&cancel),
                    // The creator wires the enclosing scopes' flags in (`Vm::scope_ancestors`)
                    // when this sched is built INSIDE an already-running task.
                    ancestors: Vec::new(),
                }],
                terminate: false,
                demoted_chans: std::collections::HashMap::new(),
                demote_cancel_watch: std::collections::HashMap::new(),
                next_demote_tok: 0,
                body_waits: Vec::new(),
            }),
            cv: Condvar::new(),
            deadlock_err,
            runnable: AtomicUsize::new(0),
            locals: (0..nworkers.max(1))
                .map(|_| Mutex::new(LocalQ::new()))
                .collect(),
            steal_ctr: AtomicUsize::new(0),
            inflight: AtomicUsize::new(0),
            blocked_native: AtomicUsize::new(0),
            mem_cap,
            // gaps.md B5 — no parent by default; `activate_eager_nursery` sets it on an eager sched.
            parent_wake: None,
            // gaps.md W7-56 — empty by default; both `MnSched` construction sites assign the run's
            // registry. An empty one is today's behaviour (no veto).
            exec_registry: Default::default(),
            // gaps.md W7-58 — empty by default; both `MnSched` construction sites assign the run's
            // state. An empty one has no parties, so the judge below never fires.
            quiesce: Default::default(),
        }
    }

    /// Cross-nursery flat scheduler — a NESTED nursery (nested `run_mn_nursery` / eager nursery) enlists
    /// into THIS one global sched. Appends a `JoinScope` whose `base_index` is the current flat
    /// `slots.len()`, extends `slots` by `total` `None`s (its contiguous sub-range), and returns the new
    /// `scope_id`. Append-only (existing scopes' `base_index` never shifts, so live fibers' `task_index`
    /// stays valid). Holds the core lock so the grow is atomic against the deadlock predicate. `total`
    /// may be 0 for an eager nursery (it grows via `inject`).
    fn register_scope(
        &self,
        total: usize,
        cancel: Arc<AtomicBool>,
        ancestors: Vec<Arc<AtomicBool>>,
    ) -> usize {
        let mut c = self.lock();
        let base_index = c.slots.len();
        c.slots.extend((0..total).map(|_| None));
        let scope_id = c.scopes.len();
        c.scopes.push(JoinScope {
            base_index,
            total,
            done: 0,
            bytes: 0,
            body_open: false,
            body_blocked: false,
            awaiting_builder: false,
            cancel,
            ancestors,
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
    fn register_scope_seeded(
        &self,
        cancel: Arc<AtomicBool>,
        ancestors: Vec<Arc<AtomicBool>>,
        workers: Vec<ReadyWorker>,
    ) -> usize {
        let total = workers.len();
        let mut c = self.lock();
        let base_index = c.slots.len();
        c.slots.extend((0..total).map(|_| None));
        let scope_id = c.scopes.len();
        c.scopes.push(JoinScope {
            base_index,
            total,
            done: 0,
            bytes: 0,
            body_open: false,
            body_blocked: false,
            awaiting_builder: false,
            cancel,
            ancestors,
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

    /// §2c1 — a NESTED eager scope has joined and every one of its tasks is done: pop it and give its
    /// slots back, so the enclosing scope is the LAST scope again and its own later `inject`s stay
    /// contiguous (`inject`'s invariant, and `take_scope_slots`' `base..base+total` slice).
    ///
    /// Sound because eager scopes on one thread nest strictly LIFO — `EnterNursery`/`JoinNursery` are
    /// properly nested in the bytecode and an escape reclaims innermost-first — so the scope being
    /// retired owns the TAIL of `slots` and no live fiber holds an index into it (they are all done).
    /// A no-op if it is somehow not the last scope, which keeps the slot ranges valid at worst-case
    /// cost of a stale empty scope rather than corrupting a live one.
    fn retire_last_scope(&self, scope_id: usize) {
        let mut c = self.lock();
        if scope_id + 1 != c.scopes.len() {
            return;
        }
        let s = &c.scopes[scope_id];
        if s.done < s.total {
            return;
        }
        let base = s.base_index;
        c.slots.truncate(base);
        c.scopes.pop();
    }

    /// §2c1 — how many of this sched's tasks are still unfinished, across every scope. Used by
    /// `Vm::farm_outermost_eager_helpers` to skip the pool entirely for a nursery the inline joiner
    /// can finish alone.
    pub(super) fn outstanding_tasks(&self) -> usize {
        let c = self.lock();
        c.scopes
            .iter()
            .map(|s| s.total.saturating_sub(s.done))
            .sum()
    }

    /// §2c1 — register (or drop) a BLOCKED BODY's channel waits as demoted participants of this
    /// sched, so `is_deadlocked_ignoring_jobs`' demoted-queue peek can see them.
    ///
    /// This is what makes the `body_blocked` relaxation SOUND. Lifting `body_open` tells the predicate
    /// "this body cannot inject" — true — but says nothing about the body being the RENDEZVOUS PARTNER
    /// of one of the sched's own parked fibers, which it very often is. Registering the channel puts
    /// the body in the same set the demoted-worker path uses, so the peek vetoes exactly when a value
    /// is already queued for it. Measured: without it, `Channel[int](1)` + `spawn: ch.send(0);
    /// ch.send(1)` + two body `recv`s false-faulted `recv on an empty channel: deadlock` on 4 runs in
    /// 8, against Go's `0 1`.
    ///
    /// Keyed by `Arc::as_ptr(core)`, the same key space `Vm::channel_core_ptr` uses, so a body and a
    /// demoted worker blocked on the SAME channel share one refcounted entry.
    /// **Both halves move under ONE lock acquisition, and that is a correctness requirement, not
    /// tidiness.** Raising `body_blocked` lifts the deadlock veto; registering the channel is what
    /// makes the un-vetoed predicate reach the right answer. Doing them as two acquisitions leaves a
    /// window in which the veto is down and the channel is invisible, and an idle worker sampling
    /// there reaps a perfectly satisfiable parked fiber: measured on `Channel[int](1)` with the
    /// consumer in the `spawn` and the sender in the body, the consumer was killed after ONE `recv`
    /// (`scopes=[(1,1)]`) and `main` then faulted `send on a full channel: deadlock`, 5 runs in 6.
    pub(super) fn set_body_wait(
        &self,
        scope_id: usize,
        wait: Option<&Arc<crate::vm::quiesce::PartyWait>>,
        blocked: bool,
        awaiting: bool,
    ) {
        {
            let mut c = self.lock();
            // ON: publish the wait BEFORE lifting the veto. OFF: drop the veto BEFORE retracting it.
            // Either way the un-vetoed state is never observable without the wait.
            if blocked && let Some(w) = wait {
                c.body_waits.push(Arc::clone(w));
            }
            if let Some(s) = c.scopes.get_mut(scope_id) {
                s.body_blocked = blocked;
                // §2c1 — a body parked in a NESTED nursery's join is not merely unable to inject: it
                // WILL resume the moment that inner scope completes, and may then `send`/`close` to a
                // sibling. That is exactly what `awaiting_builder` already means, so say it rather
                // than invent a second flag — `all_incomplete_awaiting_builder` then vetoes when the
                // inner scope is DONE (the builder is about to resume and feed) and does NOT veto
                // while the inner scope is itself incomplete-and-stuck (a genuine nested deadlock,
                // which must fault). A body blocked on a CHANNEL leaves it false: that body resumes
                // only if somebody feeds it, so it is not a promise of progress.
                if awaiting {
                    s.awaiting_builder = blocked;
                }
            }
            if !blocked
                && let Some(w) = wait
                && let Some(i) = c.body_waits.iter().position(|x| Arc::ptr_eq(x, w))
            {
                c.body_waits.swap_remove(i);
            }
        }
        if blocked {
            self.cv.notify_all();
        }
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
        // gaps.md W7-58 — "this worker has already asked the process-wide verdict since its last
        // wait". Bounds the (relatively expensive, `parties`-locking) escalation below to ONE call per
        // wait, so it can never spin. It is NOT a claim that the verdict's inputs only move on a
        // notify — they do not, which is why the idle wait below is BOUNDED whenever this is set (see
        // there). Cleared at every wait site.
        let mut judged = false;
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
            // gaps.md W7-58 — THE NURSERY OWNER'S JUDGE. The gate above declined because a job is
            // outstanding (W7-56). That veto is right when the job is RUNNING, and wrong when the job
            // is itself blocked forever — which is exactly W7-58's shape. The process-wide verdict can
            // tell the two apart (a stuck job is a registered party with an unsatisfiable wait; a
            // running one is unregistered, and `parties.len() < live` then vetoes), but ONLY a
            // counted party polling `block_halt_check` ever asks it — and a nursery owner never
            // reaches that call, it sits in this loop. So an idle worker asks on its behalf.
            //
            // **`drop(c)` is the most dangerous line in this change.** `quiesced` takes `parties` (P)
            // and then, through `PartyWait::Nursery::satisfiable`, this same core lock (A). The one
            // total order is P → A, so the guard MUST be released first; holding it here is a real
            // hang, not a style nit. Everything below the gap is therefore re-derived: `continue`
            // rather than fall through, so `c.terminate` and the queue gates at the top of the loop
            // are re-evaluated rather than skipped with a stale verdict (a lost wakeup otherwise).
            if !judged && self.is_deadlocked_ignoring_jobs(&c) {
                judged = true;
                drop(c);
                let verdict = self.quiesce.quiesced(&self.exec_registry);
                c = self.lock();
                if verdict && self.is_deadlocked_ignoring_jobs(&c) {
                    c.flag_deadlock(&self.deadlock_err);
                    self.cv.notify_all();
                    return Take::Stop;
                }
                continue;
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
                judged = false; // W7-58 — a real wait ended: the verdict's inputs may have moved.
                continue;
            }
            debug_assert!(
                c.global.is_empty(),
                "D4e park invariant: runnable==0 but the global queue is non-empty"
            );
            // gaps.md W7-58 — the untimed sleep is correct ONLY while the answer depends solely on
            // this sched. When `judged` is set it does not: this worker asked the PROCESS-WIDE verdict
            // and was told "not yet", and that verdict reads `parties` — a set that changes when some
            // OTHER thread registers, with no notify to this sched (nothing pokes a sched on a party
            // registration, and adding one would create a `parties → sched_registry → SchedCore` edge
            // that inverts the P → A order this change establishes). So the untimed wait made the
            // judge EDGE-TRIGGERED: measured, `ex.submit(job)` where `job` opens its own stuck
            // `parallel:`, then `main` sleeping 300 ms before `ex.shutdown()`, hung 5/5 — the job's
            // sched judged while `parties.len() == 1 < live == 2`, slept, and `main`'s later `Join`
            // registration reached nobody. (The tell: adding a third job that finishes later made it
            // fault, because `finish` pokes the sched.)
            //
            // Bounded ONLY in that state, deliberately: a sched that is not stuck-modulo-jobs still
            // sleeps untimed, so the healthy path gains no poll at all. When it IS stuck this pays
            // exactly the [`DEMOTE_POLL_BACKOFF`] cadence every other blocking-in-place site already
            // pays (`block_wait_tick`, `demote_recv_block`, `demote_block_socket`,
            // `block_until_deadline`), for the same reason: a lost wakeup then costs latency instead
            // of the whole run.
            if judged {
                let (guard, _) = self
                    .cv
                    .wait_timeout(c, DEMOTE_POLL_BACKOFF)
                    .unwrap_or_else(|e| e.into_inner());
                drop(guard);
            } else {
                let guard = self.cv.wait(c).unwrap_or_else(|e| e.into_inner());
                drop(guard);
            }
            judged = false; // W7-58 — see above.
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
            (!g.is_empty(), g.closed)
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

    /// Bounded-channel backpressure — the send-side twin of [`MnSched::park`]. The running fiber
    /// blocked on a `send` into a FULL bounded channel; park it in `key`'s bucket (as an ordinary
    /// [`ParkedEntry::Recv`] — the bucket is homogeneous per-instant since a bounded channel is never
    /// simultaneously full and empty, so no new entry variant is needed). The gap re-check is the
    /// OPPOSITE of `park`'s: requeue `Ready` if a concurrent `recv` freed a SLOT (space available), the
    /// channel was `close`d (the re-run faults "send on a closed channel"), or the scope was cancelled;
    /// else park. `core.cap` is `Some` here by construction. Lock order core-OUTER / q-INNER matches
    /// `park`. A freed slot wakes this fiber via [`MnSched::recv_wake`] (called from every bounded pop).
    fn park_send(&self, key: usize, core: &Arc<ChannelCore>, mut fiber: Fiber) {
        let mut c = self.lock();
        c.running -= 1;
        let cap = core.cap.expect("park_send on an unbounded channel");
        let (space, closed) = {
            let g = core.q.lock().unwrap_or_else(|e| e.into_inner());
            (g.len() < cap, g.closed)
        };
        let cancelled = c.scopes[fiber.scope_id].cancel.load(Ordering::Relaxed);
        if space || closed || cancelled {
            fiber.state = FiberState::Ready;
            c.global.push_back(fiber);
            self.runnable.fetch_add(1, Ordering::Relaxed); // running → ready (requeued, re-checks space)
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

    /// Bounded-channel `send` when the queue may be at capacity: the space-check + enqueue + wake of
    /// any parked receivers, ALL atomic under the sched lock so two concurrent senders can't both see
    /// space and over-fill. Returns `true` if the value was enqueued (space was free), `false` if the
    /// channel was full (the value is dropped — the caller re-serializes it on its send re-run after
    /// parking). Same lock discipline / wake fan-out as [`MnSched::send_wake`] on the enqueue path.
    fn send_wake_bounded(
        &self,
        key: usize,
        core: &Arc<ChannelCore>,
        w: WireValue,
        cap: usize,
    ) -> bool {
        // W6-7/W6-10 — summarise the message BEFORE taking core lock A: `wire_summary` is
        // O(payload), and this critical section serializes every fiber's park/wake/finish.
        let sum = crate::vm::core::wire_summary(&w);
        let mut c = self.lock();
        {
            let mut q = core.q.lock().unwrap_or_else(|e| e.into_inner());
            if q.len() >= cap {
                return false; // full — caller parks (both guards drop on return)
            }
            q.push(sum, w);
        }
        self.wake_bucket(&mut c, key);
        drop(c);
        self.cv.notify_all();
        self.wake_parent_chain(key);
        core.cv.notify_all();
        true
    }

    /// Bounded-channel backpressure — a `recv` freed a slot on `key`, so wake every parked SENDER
    /// (all filed as [`ParkedEntry::Recv`]) to re-run and grab the space. Identical fan-out to
    /// [`MnSched::close_wake`] (wake the bucket + notify + walk the parent chain) — a recv freeing a
    /// slot and a close both just "make the waiters on this key runnable"; only the reason differs.
    /// The woken senders race for the one slot; losers re-park (the documented multi-sender
    /// nondeterminism). No-op fan-out cost is bounded by the (usually 0 or 1) parked senders.
    fn recv_wake(&self, key: usize, core: &Arc<ChannelCore>) {
        self.close_wake(key, core);
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
    fn park_wait(&self, arms: Vec<(usize, Arc<ChannelCore>, bool)>, mut fiber: Fiber) {
        let mut c = self.lock();
        c.running -= 1;
        // Gap re-check for EVERY arm (mirrors `park`'s 1-key re-check): a concurrent `send`/`recv`/
        // `close`/cancel to any arm must requeue, not park. Cross-nursery flat scheduler — read the
        // parking fiber's SCOPE cancel (not the sched's global `cancel`). Readiness is KIND-AWARE: a
        // recv arm is ready with a queued value / on close; a SEND arm is ready with a FREE slot (a
        // bounded channel below capacity, or unbounded — always) / on close. Using the recv predicate
        // for a full send arm would (wrongly) call it "ready" and spin requeue→re-poll→still-full→re-park.
        let mut ready_now = c.scopes[fiber.scope_id].cancel.load(Ordering::Relaxed);
        // W7-2 — arm accounting is THREE-way, mirroring `op_wait_poll` exactly: READY (take the arm
        // now), DEAD (closed+empty recv arm — the poll SKIPS it and only counts it toward
        // `all_closed`), or LIVE (empty but still wakeable). `any_live` tracks the third.
        let mut any_live = false;
        if !ready_now {
            for (_, core, is_send) in &arms {
                let (ready, dead) = {
                    let g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                    if *is_send {
                        // SEND arm: ready with a FREE slot (bounded below cap, or unbounded) OR on
                        // close (the send then FAULTS, matching op_wait_poll's ready-then-fault).
                        // A full bounded send arm is never dead — a receiver frees a slot.
                        (g.closed || core.cap.is_none_or(|cap| g.len() < cap), false)
                    } else {
                        // RECV arm: ready ONLY with a queued value (a closed channel still drains its
                        // buffered messages). A closed+EMPTY non-timer recv arm is DEAD — nothing can
                        // ever make it ready again. It is NOT "ready": op_wait_poll SKIPS a dead arm,
                        // so requeueing on ONE dead arm among live ones spins requeue→re-poll(skip)→
                        // re-park forever (the reverted parity-perf-0 live-lock).
                        (
                            !g.is_empty(),
                            g.closed && g.is_empty() && core.timer.is_none(),
                        )
                    }
                };
                // A tripped `done_latch` (a concurrent `trip()`) makes this arm ready, same as a queued
                // value or a close — re-check it in the gap or a `wait: tok.done()` strands forever.
                // (Timers/latches are recv-only channels, so this never applies to a send arm.)
                if ready || core.done_latch.load(Ordering::Relaxed) {
                    ready_now = true;
                    break;
                }
                any_live |= !dead;
            }
        }
        // W7-2 — if EVERY arm is dead, requeue instead of parking. A `close()` that lands in the
        // window between `op_wait_poll`'s empty poll and this park runs `close_wake` against a bucket
        // that is still empty, so this re-check is the last chance to observe it; parking here strands
        // the fiber on a key nothing will ever wake and the deadlock detector (correctly) reaps it —
        // a SPURIOUS `deadlock:` fault. The requeue TERMINATES: the re-run `WaitPoll` hits `all_closed`
        // and faults "wait: all channels closed", so unlike the
        // one-dead-among-live case there is no requeue→re-park spin.
        if !ready_now && !arms.is_empty() && !any_live {
            ready_now = true;
        }
        if ready_now {
            fiber.state = FiberState::Ready;
            c.global.push_back(fiber);
            self.runnable.fetch_add(1, Ordering::Relaxed); // running → ready (requeued, re-polls)
            self.cv.notify_all();
            return;
        }
        fiber.state = FiberState::Blocked; // running → parked: runnable unchanged
        let keys: Vec<usize> = arms.iter().map(|(k, _, _)| *k).collect();
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
    /// here re-runs and observes the flag at the next back-edge.
    ///
    /// gaps.md W8-7 — deliberately **no** `cv.notify` here (was `notify_all`, 22nd-of-22 site,
    /// removed): preemption fires every `CONTEXT_REDS` dispatched ops per fiber, so with a
    /// multi-fiber CPU-bound scope this was a `notify_all` many times a second, each waking every
    /// idle worker into an O(W) `try_steal` probe that finds nothing and re-parks — O(W^2) mutex/futex
    /// churn per time slice (measured: default worker count on a 12-core box was SLOWER than
    /// `--threads=4`, 28x the sys time). No wake is needed here because the fiber this requeues almost
    /// always already has a live consumer — with ONE hole, closed at the departure rather than here
    /// (see below):
    /// 1. The yielding worker itself loops straight back into `take_runnable`
    ///    (`Vm::mn_worker_loop`, `sched.rs`). Precisely: its only exit that does NOT go through
    ///    `take_runnable` is `self.demoted` — the other one, `Take::Stop`, is *returned by*
    ///    `take_runnable`, so a worker taking it has already evaluated the queue and cannot be holding
    ///    an unconsumed yield. `mn_worker_loop`'s `self.demoted` arm carries the full exit enumeration.
    ///    `demoted` is taken when THIS worker was itself covered by a
    ///    replacement earlier (mid-fiber, at demote time) and now leaves for good instead of looping
    ///    back. That replacement was spun up while `runnable == 0` and typically parked into this same
    ///    `take_runnable`'s untimed `cv.wait` well before this yield — so on the `demoted` exit no one
    ///    is left awake, unless something notifies. Fixed at the departure, not here: `mn_worker_loop`
    ///    now does `sched.cv.notify_all()` on the `self.demoted` return (`sched.rs`), which is where
    ///    the actual consumer gap is — see its doc for the enumeration. (This is the gaps.md W8-7 hang
    ///    regression fix.)
    /// 2. The owner-scope-completing case this argument used to cite separately is **vacuous**, not a
    ///    second live consumer: `take_runnable`'s owner-stop branch sits *after* the global batch-grab,
    ///    so it is reachable only with the global queue already empty — the worker would have grabbed
    ///    its own just-yielded fiber first. It cannot be the thing that leaves a yielded fiber
    ///    unconsumed.
    /// 3. `runnable` accounting is unchanged (still incremented under this same lock), so
    ///    `is_deadlocked`'s `runnable == 0` predicate cannot false-fire on a yielded fiber.
    /// 4. Ordering is unchanged (global tail), so round-robin fairness is exactly as before — pinned by
    ///    `mnsched_yield_fiber_requeues_at_tail`, unmodified.
    ///
    /// Go does NOT do the same — checked against go1.26.6's `runtime/proc.go`: `goschedImpl` (the
    /// preemption path) calls `wakep()` on EVERY preemption, which CAS-guards a single idle P awake
    /// (`sched.nmspinning`) — a damped single wake, not no wake. But note WHAT that wake is for.
    /// `goschedImpl`'s last line is `schedule()`, on the same M — Go's exact equivalent of this
    /// worker's `take_runnable` re-entry — so the preempted `g` already has a consumer and `wakep` is
    /// a **recruitment** wake, not a liveness one: it brings an idle P online now that there is one
    /// more runnable `g` than there are runners. That is why one spinner is enough to suppress it
    /// (`nmspinning`) and why it is gated on `mainStarted`, neither of which would make sense for a
    /// wake the `g`'s survival depended on. So the honest comparison is NOT that Chezzi's guarantee is
    /// stronger — both runtimes have the same always-present consumer. What Chezzi gives up by not
    /// waking is the RECRUITMENT: Chezzi wakes ZERO times per preemption where Go wakes at most one,
    /// so an idle Chezzi worker is never recruited mid-slice the way an idle Go P can be. That is
    /// sound here because a preemption does not create work — it re-queues work that already had a
    /// runner — and because the ways a fiber becomes runnable for the FIRST time all notify (`inject`,
    /// `wake_bucket` at all four callers, the park/send/wait gap requeues, `cancel_drain`,
    /// `complete_offload`, and the batch-grab surplus push above). Exactly one other site increments
    /// `runnable` without notifying — `register_scope_seeded`, which seeds a nested nursery's fibers on
    /// a LIVE sched — and it has **two callers whose safety arguments are different**, which matters
    /// because this enumeration is the whole liveness case:
    ///   - `Vm::run_mn_nursery_nested` (`sched.rs`, the seeding caller) is safe because it becomes the
    ///     consumer on the very next line (`shell.mn_worker_loop(..)`) — the `yield_fiber` argument.
    ///   - `Vm::activate_eager_nursery`'s nested-scope branch (`sched.rs`) is NOT: it returns an
    ///     `EagerScope` with no loop after it. It is safe **only because it passes `Vec::new()`**, so
    ///     the `fetch_add` is `+0` and nothing becomes runnable. **A future commit that seeds a
    ///     non-empty vec there gets no notify and no consumer** — add the notify at that call site if
    ///     you ever do.
    ///
    /// `seed` is a third no-notify site, but it runs pre-start. This is the sys-time collapse W8-7
    /// measured.
    fn yield_fiber(&self, mut fiber: Fiber) {
        let mut c = self.lock();
        c.running -= 1;
        fiber.state = FiberState::Ready;
        c.global.push_back(fiber);
        self.runnable.fetch_add(1, Ordering::Relaxed); // running → ready (round-robin requeue)
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

    /// gaps.md B5 — after waking this sched's own `parked` bucket for `key`, walk the `parent_wake`
    /// chain and wake each ANCESTOR sched's bucket too. `None` for every ordinary sched, so this is a
    /// no-op on the hot path; it fires only for an eager nested nursery's private sched, where a
    /// receiver parked in the PARENT nursery on this channel would otherwise never be made runnable
    /// (the value is already in the shared `ChannelCore`, but wake is per-sched). Each ancestor is
    /// woken under its OWN core lock — the eager core guard is already dropped by the caller before
    /// this runs, and `parent_wake` points strictly UPWARD, so no two sched cores are ever held at
    /// once (no ABBA). `wake_bucket` bumps the ancestor's own `runnable`, requeuing the parent's
    /// receiver onto its home queue; an over-wake (empty queue → re-park) is the tolerated pattern.
    fn wake_parent_chain(&self, key: usize) {
        let mut p = self.parent_wake.clone();
        while let Some(anc) = p {
            anc.wake_key(key);
            p = anc.parent_wake.clone();
        }
    }

    /// Drain this sched's `parked` bucket for `key` and notify its workers — one link of
    /// [`MnSched::wake_parent_chain`], also used by the W7-56 registry walk in [`Vm::wake_on_send`]
    /// (an eager `Executor` job holds no sched, so it reaches a parked fiber only this way). Takes
    /// the core lock itself, so the caller must hold NO sched core lock and no `ChannelCore::q`.
    fn wake_key(&self, key: usize) {
        let mut c = self.lock();
        self.wake_bucket(&mut c, key);
        drop(c);
        self.cv.notify_all();
    }

    fn send_wake(&self, key: usize, core: &Arc<ChannelCore>, w: WireValue) {
        // Summarised BEFORE core lock A — see `send_wake_bounded`.
        let sum = crate::vm::core::wire_summary(&w);
        let mut c = self.lock();
        core.q
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(sum, w);
        self.wake_bucket(&mut c, key);
        drop(c);
        self.cv.notify_all();
        // gaps.md B5 — also wake a receiver parked on this channel in an ANCESTOR (parent) nursery's
        // sched (eager nested nursery only; no-op otherwise). Value is already queued above.
        self.wake_parent_chain(key);
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
        // gaps.md B5 — a close from inside an eager body must also wake a receiver ranging over this
        // channel in an ANCESTOR nursery so it observes the close and ends (no-op for ordinary scheds).
        self.wake_parent_chain(key);
    }

    /// Record a finished fiber's outcome in its FLAT slot, bump its SCOPE's done, drop it from
    /// `running`. Sets GLOBAL `terminate` only when EVERY scope is done (and no eager body is open) —
    /// because farmed helpers (sentinel scope_id) drain until global terminate, and the scope-scoped
    /// owner stop returns each owner the instant its OWN scope completes. The per-scope `done` drives
    /// that owner stop; the global all-done drives helper/sentinel termination.
    /// Returns whether the stored outcome ABORTS its scope (a `Fault`/`Exit`), so the caller runs the
    /// `cancel_drain` its parked siblings need. It is computed here, not by the caller, because the
    /// W7-26r backlog verdict below can turn a `Done` into a hard-halt `Fault` — a caller-side
    /// `matches!` on the outcome it handed in would miss that and leave the scope's parked fibers
    /// with nobody to wake them.
    fn finish(&self, task_index: usize, scope_id: usize, outcome: TaskOutcome) -> bool {
        let mut c = self.lock();
        c.running -= 1;
        // W7-26r — this thread is the only party that can observe the cap for a nursery whose parent
        // is blocked in the join (see `core::halt_over_backlog`). These slots live outside every
        // `Heap`, so `live_bytes` never counted them either: this is the accounting AND the
        // observation. Cheap: the summary is O(1) (buffered output capacities; a task's return value
        // is `Nil`, W7-27) and the whole block is skipped when no cap is set.
        let outcome = if self.mem_cap != 0 {
            let s = &mut c.scopes[scope_id];
            s.bytes += super::vm::core::outcome_summary(&outcome).0;
            let (outcome, over) =
                super::vm::core::halt_over_backlog(outcome, s.bytes, self.mem_cap);
            if over {
                // Under the core lock, exactly as `trip_scope_cancel` requires — the release
                // publishes the flag to every worker that later evaluates `is_deadlocked`. Only THIS
                // scope: an inner nursery's backlog must not cancel an outer sibling (structured
                // concurrency); the fault propagates outward through the join instead.
                s.cancel.store(true, Ordering::Relaxed);
            }
            outcome
        } else {
            outcome
        };
        let aborts = matches!(
            outcome,
            TaskOutcome::Fault { .. } | TaskOutcome::Exit { .. }
        );
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
        aborts
    }

    /// N4 — trip scope `scope_id`'s cancel **under the core lock**. Two reasons the lock is not
    /// optional: (1) the mutex release PUBLISHES the flag to every worker that later takes the lock to
    /// evaluate `is_deadlocked` (a bare `Relaxed` store outside it has no synchronizes-with edge, so a
    /// lock-holder could legally still read `false` and reap the scope's parked fibers as `Deadlocked`);
    /// (2) it lets the abort seams arm this veto BEFORE they clear their own (`awaiting_builder` /
    /// `body_open`) — a gapless veto handoff. Idempotent (the fault path also trips via
    /// `Vm::trip_cancel`, which is program-ordered before `finish`'s own lock release).
    fn trip_scope_cancel(&self, scope_id: usize) {
        let c = self.lock();
        c.scopes[scope_id].cancel.store(true, Ordering::Relaxed);
    }

    /// gaps.md W7-57 — the run-wide `os.exit` analogue of the intra-nursery abort teardown
    /// (`mn_worker_loop`'s `if aborts { cancel_drain; drain_sched }`): trip **every** scope's cancel and
    /// drain every parked fiber, so a nursery that is not the exiting party's own still dies now instead
    /// of at its natural end. Called for each live sched by [`Vm::halt_all_scheds`].
    ///
    /// **The cancel stores go under the core lock** — the release edge [`trip_scope_cancel`] documents.
    /// A bare `Relaxed` store outside it has no synchronizes-with edge, so a worker already holding the
    /// lock to evaluate `is_deadlocked` could legally still read `false` and reap the scope's parked
    /// fibers as `Deadlocked`, dropping their `defer`s.
    ///
    /// The lock is DROPPED before `cancel_drain`/`drain_sched`, which take it themselves (`drain_sched`
    /// keeps the poller registry leaf-level and calls `complete_offload`, which locks this core).
    ///
    /// `scopes` is snapshotted by length: a scope registered after this instant is a nursery created
    /// after the exit was published, whose fibers are covered by `jump_checked`'s back-edge exit rung
    /// and by every blocking wait's `run_exit_err` — this drain is a promptness lever, not the only one.
    pub(super) fn cancel_all(self: &Arc<Self>) {
        let n = {
            let c = self.lock();
            for s in &c.scopes {
                s.cancel.store(true, Ordering::Relaxed);
            }
            c.scopes.len()
        };
        for scope_id in 0..n {
            self.cancel_drain(scope_id);
        }
        poller::drain_sched(self);
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
    ///
    /// (An `os.exit` reaches every scope by calling this in a loop — [`MnSched::cancel_all`] — rather
    /// than by relaxing the scope-scoping here, which the structured-concurrency invariant forbids.)
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
        // W7-26r — the retained bytes leave with the slots, so the backlog total returns to zero with
        // them (`EagerState::take_slots` does the same for the executor half). Today every caller
        // takes only after `wait_for_scope`, so no fiber of this scope can still be running; this
        // keeps the counter honest if that ever stops being true, instead of leaving a stale
        // watermark that would fault the NEXT program to reach this scope id.
        c.scopes[scope_id].bytes = 0;
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
        // W7-56 — an eager `Executor` job outstanding anywhere in this RUN is a live sender the
        // counters below cannot see: it runs on the shared pool with no fiber of this sched, so it
        // bumps neither `running`/`runnable` nor `inflight`, and a nursery task parked on the channel
        // that job is about to feed reads as an all-parked quiesce. This is exactly the veto
        // `quiesce::QuiesceState::quiesced` already applies process-wide (`parties.len() < live`,
        // where `live` counts the same `outstanding`), for the same reason: an UNCOUNTED sender must
        // veto. `outstanding` is bumped at `reserve()` (at `submit`, before dispatch) and dropped at
        // `finish()`, so a job still queued behind a saturated pool already counts.
        //
        // The veto EXPIRES: `dispatch_eager_job`'s completion closure pokes every live sched after
        // `finish()`, so a job that ends without ever sending lets an idle worker re-evaluate and
        // report the genuine deadlock (the idle wait is untimed — without that poke this veto would
        // be a permanent silent hang instead of a fault).
        //
        // **It stays FIRST and it stays UNCHANGED (W7-58).** W7-58 is the case where the outstanding
        // job is itself stuck; the fix is to make the *process-wide* verdict able to see a nursery
        // owner ([`quiesce::PartyWait::Nursery`]), NOT to weaken this veto — removing it re-opens
        // W7-56 (a live program declared deadlocked).
        //
        // Lock order: this holds `SchedCore` (A) and takes `exec_registry` → one `ExecutorCore::eager`
        // beneath it, matching the V4 `demoted_chans` peek's A-then-`q`. Nothing acquires a sched core
        // lock while holding either, so no cycle.
        if crate::vm::quiesce::QuiesceState::outstanding_jobs(&self.exec_registry) > 0 {
            return false;
        }
        self.is_deadlocked_ignoring_jobs(c)
    }

    /// [`MnSched::is_deadlocked`] minus its W7-56 outstanding-job veto — "can THIS sched still move on
    /// its own?", asked without reference to any executor.
    ///
    /// Two callers, and the split is what makes W7-58's verdict non-circular:
    /// * [`quiesce::PartyWait::Nursery::satisfiable`] — an owner parked in a nursery join is
    ///   satisfiable exactly when its nursery can still move. The full predicate would be useless
    ///   there: it vetoes on `outstanding > 0`, which is precisely the W7-58 shape (a stuck job).
    ///   Sound because that job is then a registered party of the process-wide verdict in its own
    ///   right — an *unregistered* job is a running one, and `parties.len() < live` already vetoes.
    /// * the W7-58 judge in [`MnSched::take_runnable`], which escalates to the process-wide verdict
    ///   when only this sched's own predicate holds.
    ///
    /// **Why it is not circular, stated precisely.** It reads no party state and no `outstanding` —
    /// so the process-wide verdict never appears on its own right-hand side. It is NOT lock-free
    /// though: besides `SchedCore` (held by the caller) and this sched's own atomics, its last gate
    /// peeks `ChannelCore::q` for every demoted fiber. Evaluated from `PartyWait::Nursery` that makes
    /// the chain **P → A → Q**, which is the established total order (`parties` → `SchedCore` →
    /// `ChannelCore::q`); `A → Q` is the order `send_wake` and the demoted peek already use.
    pub(super) fn is_deadlocked_ignoring_jobs(&self, c: &SchedCore) -> bool {
        // The `done < total` half is now explicit (the owner-stop replaced the preceding scalar
        // `done == total` terminate check). If EVERY scope is done there is no deadlock — `finish` will
        // have (or is about to) set global `terminate`; the owner-stop returns each owner already.
        if !c.any_scope_incomplete() {
            return false;
        }
        // Per-connection spawn — an eager nursery whose body is still running is live work the sched
        // can't account (the acceptor runs inline and may `inject` a handler that wakes a parked
        // sibling). Never declare deadlock while ANY body can still inject; `close_body` at
        // `JoinNursery` re-enables the predicate so a genuine post-join deadlock still fires. Always
        // `false` on the lazy path — unchanged.
        //
        // §2c1 — `any_body_injecting`, not `any_body_open`: a body BLOCKED in place (or parked in a
        // NESTED nursery's join) cannot reach another `spawn`, so it is not the live feeder this veto
        // exists for. Without the distinction an eager top-level nursery — whose body spans
        // essentially the whole program — vetoes forever, and both a genuine `main`-plus-sibling
        // deadlock and a genuine nested deadlock HANG instead of faulting.
        //
        // **This is only safe because nesting shares ONE sched** (`EagerScope::scope`). With a private
        // sched per nursery the sibling that would feed the blocked body was invisible here, and this
        // exact relaxation false-faulted `examples/parallel_cross_nursery_{circular,fanout}.chz`. Now
        // that sibling is another SCOPE on this same sched, so it shows up in `running`/`runnable` and
        // vetoes on its own merits.
        if c.any_body_injecting() {
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
        // N4 — CANCEL is a wakeup source the counters above do not model, and a cancelled scope
        // mid-teardown is not a deadlock. Two shapes, both live progress:
        //
        // * an UNDRAINED PARKED fiber of a cancelled scope (`any_cancelled_scope_awaiting_drain`): the
        //   cancel trip and its `cancel_drain` are two core-lock acquisitions apart (three seams), and
        //   an idle worker landing in that gap sees the pre-drain quiesce. `cancel_drain` is about to
        //   requeue those fibers so they unwind their `defer`s;
        // * a DEMOTED fiber whose cancel flag is tripped (`any_demoted_cancel_pending`): it is
        //   `blocked_native`, not `parked`, so the first scan cannot see it — but `demote_recv_block`
        //   ranks `cancel_requested()` above `terminate`/self-detect, so it resumes within one
        //   `DEMOTE_POLL_BACKOFF`, unwinds and runs its `defer`s (which can `send`).
        //
        // Either way `flag_deadlock` would drop every parked fiber WITHOUT `unwind_deferred` (silently
        // skipping their `defer`s — and `reduce_task_slots` ranks Fault > Deadlocked, so the spurious
        // deadlock never even surfaced; the lost `defer` was the only symptom) and LATCH `terminate`,
        // truncating the cleanup of anything demoted inside its own `defer`. BOUNDED to real progress:
        // a cancelled scope with no undrained park whose last fiber is demoted-blocked forever INSIDE
        // an uncancellable `defer` (no cancel watch — `cancel_requested()` is false while
        // `deferring > 0`) IS a genuine deadlock and is reported, not hung. Evaluated only at the
        // quiesce (after the counter gate above), so the scan is off the idle/steal hot path. A GENUINE
        // deadlock (nothing cancelled anywhere) is untouched.
        if c.any_cancelled_scope_awaiting_drain() || c.any_demoted_cancel_pending() {
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
            .any(|(core, _)| !core.q.lock().unwrap_or_else(|e| e.into_inner()).is_empty())
        {
            return false;
        }
        // §2c1 — the same question for a BLOCKED BODY of this sched's thread, asked in the direction
        // that body actually waits in (`SchedCore::body_waits`). A satisfiable body is about to
        // resume and feed one of the fibers below, so this is not a deadlock. Chain A → Q, as above.
        if c.body_waits.iter().any(|w| w.satisfiable()) {
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
            timer,
        } = req;
        if let Some(t) = timer {
            // D5 owe #2 — a `sleep_ms`: park the fiber on the timer thread (no pool thread, no work),
            // waking it at the deadline. `sleep_ms` returns nothing, so the fiber resumes with
            // `Ok(Nil)` and the native is never run (there is nothing to compute). Same
            // inflight→runnable + `notify` accounting as the pool path (`complete_offload`), so the
            // deadlock predicate stays sound: the sleeping fiber is `inflight` and WILL come back.
            arm_timer_sleep(sched, fiber, t, span);
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
                    is_assert: false,
                    is_over_memory: false,
                    is_timed_out: false,
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
        // Cross-nursery flat scheduler — clone THIS fiber's SCOPE cancel (not the sched's legacy global
        // `cancel`, which is only the OUTERMOST nursery's) while we hold the core lock anyway: it is the
        // flag `register` must gate the park on, exactly like `park`/`park_wait`'s gap re-check. Reading
        // the global one here would let a fiber of a CANCELLED INNER scope park on a poller that
        // `drain_sched` had already swept — stranding it, and (N4) holding the cancel-teardown veto
        // forever.
        let cancel = {
            let mut c = self.lock();
            c.running -= 1;
            self.inflight.fetch_add(1, Ordering::Relaxed); // running → inflight
            Arc::clone(&c.scopes[fiber.scope_id].cancel)
        };
        // `register` rejects (returns the fiber) iff that scope's cancel was tripped before it could
        // park — a sibling faulted in the park-vs-cancel gap. Re-inject so the fiber resumes and unwinds
        // on the cancel flag, rather than parking on a poller a past `drain_sched` already swept (→ a
        // hang).
        if let Some(fiber) = poller::register(
            pp.key,
            pp.fd,
            pp.interest,
            fiber,
            Arc::clone(self),
            cancel,
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

/// W7-16 — wait out an offloaded `sleep_ms` in `DEMOTE_POLL_BACKOFF` chunks, re-arming itself on the
/// timer thread, so a cancel or a `--timeout` reaches the sleeping fiber INSIDE the sleep instead of
/// after it. Pre-fix this was one `submit_at(deadline, …)`: a nursery task sleeping 3 s ran the full
/// 3 s through a sibling's fault at 50 ms and then printed its post-sleep line (measured 3005 ms; the
/// existing cancellation checkpoint only ever covered the *entry* checkpoint, where the fault precedes the call).
///
/// **Why re-arm rather than park where `cancel_drain` can reach it.** `cancel_drain` walks `c.parked`
/// only, and filing this fiber there needs a claim-once token against the timer firing plus
/// `parked_n`/`runnable`/`inflight` all kept consistent — and a parked fiber that no channel can ever
/// feed is exactly the false-deadlock shape W7-12/W7-15 came from. It also requeues with
/// `resume_native == None`, so the resumed fiber would push nothing where the suspended `Call` expects
/// a value. Re-arming keeps the fiber under a SINGLE owner (the timer heap) at all times, so there is
/// no claim race at all.
///
/// **Counters are untouched by design.** `running -= 1` / `inflight += 1` happened once in
/// [`MnSched::offload`]; `complete_offload` runs exactly once, on whichever branch below resumes the
/// fiber. A re-arm does neither, so `inflight > 0` keeps vetoing the deadlock predicate for the whole
/// sleep — the property that makes this preferable to a park.
///
/// Each re-arm targets `min(deadline, now + backoff)` computed from the ABSOLUTE deadline, so tick
/// jitter cannot accumulate and the final wake still lands on `deadline`.
///
/// ponytail: 200 re-arms/s per sleeping fiber, each a `timers` mutex + a poller notify. Thousands of
/// concurrent sleepers would load the single timer/poll thread; upgrade path is a per-scope
/// pending-sleep registry that `cancel_drain` fires directly. (`fire_due_timers` drops the timers lock
/// before running a job, so re-arming from inside a job is safe.)
fn arm_timer_sleep(sched: Arc<MnSched>, mut fiber: Fiber, t: TimerSleep, span: Span) {
    let now = std::time::Instant::now();
    // W7-57 — `run_deadline` is evaluated BEFORE `cancel`, and the order is deliberate: a run-wide
    // `os.exit` now trips every scope's cancel (`MnSched::cancel_all`), so an exit racing an already-
    // expired `--timeout` would otherwise be reported as `cancelled` and the harness would lose its
    // `timed_out` marker. `--timeout` is the outer, absolute halt and must outrank. Nothing in-tree
    // pinned the previous order.
    let halt = if t.run_deadline.is_some_and(|rd| now >= rd) {
        Some(
            RuntimeError {
                message: format!("test exceeded --timeout ({}ms)", t.timeout_ms),
                span,
                is_assert: false,
                is_over_memory: false,
                is_timed_out: false,
            }
            .timed_out(),
        )
    } else if t.cancel.iter().any(|c| c.load(Ordering::Relaxed)) {
        Some(RuntimeError {
            message: "cancelled".to_string(),
            span,
            is_assert: false,
            is_over_memory: false,
            is_timed_out: false,
        })
    } else {
        None
    };
    if let Some(err) = halt {
        fiber.resume_native = Some(Err(err));
        sched.complete_offload(fiber);
        return;
    }
    if now >= t.deadline {
        fiber.resume_native = Some(Ok(crate::native::NativeRet::Nil));
        sched.complete_offload(fiber);
        return;
    }
    let mut next = t.deadline.min(now + DEMOTE_POLL_BACKOFF);
    if let Some(rd) = t.run_deadline {
        next = next.min(rd);
    }
    timer::submit_at(
        next,
        Box::new(move || arm_timer_sleep(sched, fiber, t, span)),
    );
}

impl SchedCore {
    /// Fault every still-parked fiber (across ALL scopes) with the deadlock error and set global
    /// terminate (called under the lock). Correct because the global predicate firing means nothing can
    /// progress anywhere. A `Wait` token sits in N buckets but is ONE fiber — claim-once (the wake CAS)
    /// dedups it so the flat slot is faulted and the fiber's SCOPE's `done` bumped exactly once.
    /// Each parked fiber's OWN buffered stdout/stderr (moved into `f.ctx.out`/`stderr` by `swap_ctx`
    /// when it parked) is carried into its `Deadlocked` slot, so `reduce_task_slots` flushes EVERY
    /// parked buffer at its task-order slot — matching what a strictly sequential run would have
    /// printed live before the deadlock returned. A distinct `Deadlocked` outcome (not `Fault`) is what lets
    /// the reduce flush ALL parked buffers here without disturbing the real-fault multi-fault path.
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
                    // Carry the parked fiber's OWN buffered stdout/stderr into its Deadlocked slot
                    // (not an empty buffer). `swap_ctx` moved this fiber's live prints into
                    // `f.ctx.out` when it parked; the downstream `reduce_task_slots` flushes EVERY
                    // Deadlocked slot's buffer at its task-order slot (not just the lowest-index one,
                    // as with a real Fault), so with two-or-more parked fibers a higher-index
                    // printer's output is preserved byte-identically to a strictly sequential run.
                    // `task_index`/`scope_id` are Copy, read before the partial move of
                    // `f.ctx.out`/`f.ctx.stderr`.
                    let (ti, sid) = (f.task_index, f.scope_id);
                    self.slots[ti] = Some(TaskOutcome::Deadlocked {
                        err: err.clone(),
                        out: f.ctx.out,
                        stderr: f.ctx.stderr,
                    });
                    self.scopes[sid].done += 1;
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
        is_assert: false,
        is_over_memory: false,
        is_timed_out: false,
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
        let value = self.worker.to_wire_at(ret, span)?;
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
    /// [`TaskOutcome`]. W7-5 — an ordinary fault does NOT trip the cancel flag: the drain runs every
    /// queued job and the join raises the lowest-index fault. Only a hard halt trips it — `os.exit`
    /// (the `pending_exit` arm) or [`executor_hard_halt`] (over-memory / timeout). W7-5d: a fault
    /// raised on a dead stdout is ORDINARY and does not trip it, so a broken pipe kills the printing
    /// job and leaves its siblings alone — see [`executor_hard_halt`] for the measured ancestors.
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
            // This worker observed a sibling's cancel and unwound — its output still flushes.
            TaskOutcome::Cancelled {
                out: std::mem::take(&mut self.worker.out),
                stderr: std::mem::take(&mut self.worker.stderr),
            }
        } else {
            match res {
                Err(e) => {
                    if executor_hard_halt(&e) {
                        self.worker.trip_cancel();
                    }
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
                        // W7-27 — the crossed value is DROPPED here, not stored. Nothing can read
                        // it: `submit` returns nil (no futures) and `reduce_task_slots` reads only
                        // `out`/`stderr`, so retaining it held every job's result for the executor's
                        // whole lifetime — 300 × ~1 MB measured at 336 MB peak RSS against CPython
                        // `ThreadPoolExecutor`'s 42 MB. The M:N nursery path stores `Nil` for the
                        // same reason (`sched.rs`, `run_mn_nursery`'s outcome). The crossing above
                        // still runs, and dropping its product does not make it dead: `to_wire_at`
                        // is FALLIBLE — a return value that cannot cross (a generator closing a
                        // reference cycle, a depth/size cap) must fault at the submit site with the
                        // task's real span. (A plain non-sendable generator is not that case: it
                        // wires to an inert `Nil` and faults only when reached — B3.3's Option B.)
                        Ok(_) => TaskOutcome::Done(WorkerResult {
                            value: WireValue::Nil,
                            out: std::mem::take(&mut self.worker.out),
                            stderr: std::mem::take(&mut self.worker.stderr),
                        }),
                        Err(e) => {
                            if executor_hard_halt(&e) {
                                self.worker.trip_cancel();
                            }
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
            // W6-2 — carry the snapshot the modules above fault in from (it used to be DROPPED here,
            // which only worked while every snapshot was the same frozen `Arc`; snapshots are
            // per-nursery now, so the shell's cannot substitute for this fiber's).
            module_snapshot: worker.module_snapshot,
            snapshot_memo: worker.snapshot_memo,
            // W7-4a — the rebuild map indexes `worker.heap` (which becomes `ctx.heap`) and belongs to
            // the view above; carry it for the same heap-keyed reason as `str_intern`, so the modules
            // that fault in later all tie to one cell per binding.
            snapshot_rebuild: worker.snapshot_rebuild,
            // W7-4c — the registry + its counter travel together (see `FiberCtx::snapshot_cells`).
            snapshot_cells: worker.snapshot_cells,
            snapshot_next_id: worker.snapshot_next_id,
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
    /// Bounded-channel backpressure — the fiber blocked on a full `send` (the send-side twin of
    /// `Park`). Carries `(key, core)` captured WHILE the fiber heap was live; the worker loop hands it
    /// to [`MnSched::park_send`], whose gap re-check waits for SPACE (not a message).
    SendPark(usize, Arc<ChannelCore>),
    /// §6d — the fiber blocked on a multi-channel `wait` (every arm empty/live). Carries
    /// `(key, core, is_send)` for each live arm, captured WHILE the fiber heap was live (like `Park`);
    /// the worker loop hands it to [`MnSched::park_wait`], which files ONE shared `WaitPark` token in
    /// every arm bucket. `is_send` selects the per-arm gap-recheck readiness predicate.
    WaitPark(Vec<(usize, Arc<ChannelCore>, bool)>),
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

/// The outcome of one bounded `send` step ([`Vm::chan_send_step`]): the value was queued (or the
/// channel was unbounded — never blocks), or the fiber parked on a full channel (re-runs on wake).
/// A closed/deadlock case is returned as an `Err` from `chan_send_step`, not a variant here.
enum SendStep {
    Sent,
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
    /// W7-18 — the `net.connect` call's span, carried so the resume can RAISE a `--timeout` hard halt
    /// attributed to the connect (see the `pending_connect` arm in `run_one_fiber`). A connect park
    /// has no other checkpoint to fall through to: the call is followed by a straight-line `match`
    /// with no back-edge and no blocking op.
    span: Span,
}

/// D5 — a blocking native call extracted at its dispatch site, ready to run off the core worker on
/// the blocking pool. The args are already materialized out of the heap into `Send` primitives
/// ([`crate::native::NativeArg`]), so the pool thread runs the native ([`OffloadHost`]) without a
/// `Vm` / heap. `span` attributes any error the native raises.
struct OffloadReq {
    func: crate::native::NativeFn,
    args: Vec<crate::native::NativeArg>,
    span: Span,
    /// D5 owe #2 — `Some(..)` for a `sleep_ms`: park the fiber on the timer thread for its duration
    /// rather than run `func` on a dirty-pool thread (a sleep does no work, just waits a deadline).
    /// `None` for every other blocking native (`io`/`fs`/`request`/`process`), which runs on the pool.
    timer: Option<TimerSleep>,
}

/// W7-16 — everything the timer thread needs to keep an offloaded `sleep_ms` a CANCELLATION and
/// `--timeout` checkpoint for its whole duration. Snapshotted at the call site (in `invoke_native`),
/// because the timer job runs with no `Vm` and no scheduler lock of its own.
struct TimerSleep {
    /// When the sleep is over.
    deadline: std::time::Instant,
    /// The fiber's scope cancel flag plus its ancestors' ([`Vm::demote_cancel_flags`] — which is
    /// deliberately EMPTY inside a `defer`, so a sleeping cleanup stays uncancellable, as contracted).
    /// Read the flags snapshotted HERE, not `c.scopes[id].cancel` inside `offload`: the latter is this
    /// scope's own flag only, and would miss an enclosing nursery's cancel.
    cancel: Vec<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// The run's absolute `--timeout` deadline, if any.
    run_deadline: Option<std::time::Instant>,
    /// …and its configured value, for the fault message.
    timeout_ms: u64,
}

mod arith;
mod call;
mod exec;
mod fileio;
mod netio;
mod sched;
mod stmt;
mod stream;

pub use stream::{flush_stream, out_dead_reason, stream_error};

/// D5 — the off-heap [`crate::native::Host`] for a blocking native run on the dirty pool (no `Vm`,
/// no heap). It serves the pre-extracted primitive args ([`crate::native::NativeArg`]) and *panics*
/// on any host-I/O method: the offload classifier ([`crate::native::Kind::Blocking`]) only covers fns
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
    fn arg_bytes(&mut self, i: usize) -> Result<Vec<u8>, crate::native::HostError> {
        match self.args.get(i) {
            // R1 — pre-extracted on the worker (heap live) into an owned byte vec; served off-thread.
            Some(crate::native::NativeArg::Bytes(b)) => Ok(b.clone()),
            Some(_) => Err(crate::native::HostError::arg_type(i, "bytes", "other")),
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
    fn read_all(&mut self) -> Result<String, crate::native::HostError> {
        unreachable!("offloaded blocking native must not read stdin (off-heap host)")
    }
    fn read_char(&mut self) -> Result<Option<String>, crate::native::HostError> {
        unreachable!("offloaded blocking native must not read stdin (off-heap host)")
    }
    fn os_args(&self) -> Vec<String> {
        unreachable!("offloaded blocking native must not read os args (off-heap host)")
    }
    fn os_env(&self, _key: &str) -> Option<String> {
        unreachable!("offloaded blocking native must not read env (off-heap host)")
    }
    fn os_getcwd(&self) -> Result<Vec<u8>, crate::native::HostError> {
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
        match self.args.get(i).copied() {
            Some(v) => match self.vm.int_val(v) {
                Some(n) => Ok(n),
                None => Err(crate::native::HostError::arg_type(
                    i,
                    "int",
                    self.vm.type_name(v),
                )),
            },
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_is_int(&self, i: usize) -> bool {
        self.args.get(i).is_some_and(|v| self.vm.is_integral(*v))
    }
    fn arg_float(&mut self, i: usize) -> Result<f64, crate::native::HostError> {
        match self.args.get(i).copied() {
            Some(v) if v.is_float() => Ok(self.vm.float_of(v)),
            Some(v) if self.vm.is_integral(v) => Ok(self.vm.int_of(v) as f64),
            Some(v) => Err(crate::native::HostError::arg_type(
                i,
                "float",
                self.vm.type_name(v),
            )),
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_bool(&mut self, i: usize) -> Result<bool, crate::native::HostError> {
        match self.args.get(i).copied() {
            Some(v) => match v.as_bool() {
                Some(b) => Ok(b),
                None => Err(crate::native::HostError::arg_type(
                    i,
                    "bool",
                    self.vm.type_name(v),
                )),
            },
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_ptr(&mut self, i: usize) -> Result<usize, crate::native::HostError> {
        match self.args.get(i).copied() {
            Some(v) => {
                if let Some(h) = v.as_obj()
                    && let Obj::Ptr(a) = self.vm.heap.get(h)
                {
                    Ok(*a)
                } else {
                    Err(crate::native::HostError::arg_type(
                        i,
                        "ptr",
                        self.vm.type_name(v),
                    ))
                }
            }
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_str(&mut self, i: usize) -> Result<String, crate::native::HostError> {
        match self.args.get(i).copied() {
            Some(v) => {
                if let Some(h) = v.as_obj()
                    && let Obj::Str(s) = self.vm.heap.get(h)
                {
                    Ok(s.to_string())
                } else {
                    Err(crate::native::HostError::arg_type(
                        i,
                        "str",
                        self.vm.type_name(v),
                    ))
                }
            }
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    /// R1 — a `bytes` arg, copied out of the heap (no aliasing). `bytes` ONLY: the checker types every
    /// seam param `bytes`, and neither a `bytearray` (not assignable to a `bytes` sink — 7b29552) nor a
    /// `list[int]` (a `bytes()`-ctor convenience, not a seam contract) can reach here from typed code.
    fn arg_bytes(&mut self, i: usize) -> Result<Vec<u8>, crate::native::HostError> {
        match self.args.get(i).copied() {
            Some(v) => {
                if let Some(h) = v.as_obj()
                    && let Obj::Bytes(b) = self.vm.heap.get(h)
                {
                    Ok(b.to_vec())
                } else {
                    Err(crate::native::HostError::arg_type(
                        i,
                        "bytes",
                        self.vm.type_name(v),
                    ))
                }
            }
            None => Err(crate::native::HostError::missing_arg(i)),
        }
    }
    fn arg_struct_fields(
        &mut self,
        i: usize,
    ) -> Result<Vec<crate::native::NativeRet>, crate::native::HostError> {
        use crate::native::NativeRet as N;
        let Some(v) = self.args.get(i).copied() else {
            return Err(crate::native::HostError::missing_arg(i));
        };
        let struct_fields = match v.as_obj().map(|h| self.vm.heap.get(h)) {
            Some(Obj::Struct { fields, .. }) => fields.as_slice().to_vec(),
            _ => {
                return Err(crate::native::HostError::arg_type(
                    i,
                    "struct",
                    self.vm.type_name(v),
                ));
            }
        };
        // Positional, declaration-order fields (the same order the StructDef declares them). Map each
        // scalar field value to a NativeRet so the cffi layer casts it to its C field width. The
        // checker guarantees flat scalar fields.
        let mut out = Vec::with_capacity(struct_fields.len());
        for fv in &struct_fields {
            let n = if let Some(k) = self.vm.int_val(*fv) {
                N::Int(k)
            } else if fv.is_float() {
                N::Float(self.vm.float_of(*fv))
            } else if let Some(b) = fv.as_bool() {
                N::Bool(b)
            } else if let Some(fh) = fv.as_obj()
                && let Obj::Ptr(a) = self.vm.heap.get(fh)
            {
                N::Ptr(*a)
            } else {
                return Err(crate::native::HostError::arg_type(
                    i,
                    "struct scalar field",
                    "other",
                ));
            };
            out.push(n);
        }
        Ok(out)
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
        let span = Span::default();
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
        let Some(v) = self.args.get(i).copied() else {
            return Err(crate::native::HostError::missing_arg(i));
        };
        let entries = match v.as_obj().map(|h| self.vm.heap.get(h)) {
            Some(Obj::Map(m)) => m.entries.clone(),
            _ => {
                return Err(crate::native::HostError::arg_type(
                    i,
                    "Map[str, str]",
                    self.vm.type_name(v),
                ));
            }
        };
        // Iterate `entries` (insertion order) so header order is deterministic and matches the interp
        // + off-heap hosts. Every key/value must be a str.
        let mut pairs = Vec::with_capacity(entries.len());
        for (_, k, val) in &entries {
            let (Some(kh), Some(vh)) = (k.as_obj(), val.as_obj()) else {
                return Err(crate::native::HostError::arg_type(
                    i,
                    "Map[str, str]",
                    "other",
                ));
            };
            let (Obj::Str(ks), Obj::Str(vs)) = (self.vm.heap.get(kh), self.vm.heap.get(vh)) else {
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
    fn arg_str_list(&mut self, i: usize) -> Result<Vec<String>, crate::native::HostError> {
        let Some(av) = self.args.get(i).copied() else {
            return Err(crate::native::HostError::missing_arg(i));
        };
        let items = match av.as_obj().map(|h| self.vm.heap.get(h)) {
            Some(Obj::List(items)) => items.clone(),
            _ => {
                return Err(crate::native::HostError::arg_type(
                    i,
                    "List[str]",
                    self.vm.type_name(av),
                ));
            }
        };
        // Iterate in list order (it IS the argv). Every element must be a str.
        let mut out = Vec::with_capacity(items.len());
        for v in &items {
            let Some(eh) = v.as_obj() else {
                return Err(crate::native::HostError::arg_type(i, "List[str]", "other"));
            };
            let Obj::Str(s) = self.vm.heap.get(eh) else {
                return Err(crate::native::HostError::arg_type(i, "List[str]", "other"));
            };
            out.push(s.to_string());
        }
        Ok(out)
    }
    fn write_stdout(&mut self, s: &str) {
        self.vm.emit_out(s);
    }
    fn write_stderr(&mut self, s: &str) {
        self.vm.emit_err(s);
    }
    fn read_line(&mut self) -> Result<Option<String>, crate::native::HostError> {
        // No flush seam here BY DESIGN: the streamed handles are unbuffered (one `write_all` + `flush`
        // per `print`, on the writer thread), so a `print("name? ", end="")` prompt is already on its
        // way out — and a fiber that WAITED for the writer would block on stdout's consumer, pinning a
        // core worker (the D5 invariant) for as long as a stalled reader cares to stall. See
        // [`stream`].
        self.vm.host.stdin.read_line()
    }
    fn read_all(&mut self) -> Result<String, crate::native::HostError> {
        self.vm.host.stdin.read_all()
    }
    fn read_char(&mut self) -> Result<Option<String>, crate::native::HostError> {
        self.vm.host.stdin.read_char()
    }
    fn os_args(&self) -> Vec<String> {
        self.vm.host.args.clone()
    }
    fn os_env(&self, key: &str) -> Option<String> {
        self.vm
            .host
            .env
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
    }
    fn os_environ(&self) -> Vec<(String, String)> {
        // Sort by key: the backing store is a `HashMap` (per-instance random iteration order), but
        // every other Chezzi map is deterministic and the serial/M:N engines each build their own
        // HashMap — an unsorted lowering would diverge run-to-run and between engines.
        let mut pairs: Vec<(String, String)> = self
            .vm
            .host
            .env
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        pairs
    }
    fn os_setenv(&mut self, key: String, value: String) {
        // Writes the SHARED HostConfig env — the SAME map `os_env`/`os_environ` read, shared by Arc
        // across M:N workers — NOT `std::env::set_var` (a third, process-global-racy source). So a
        // `setenv` from any task is observed by both readers AND by the parent/siblings.
        self.vm
            .host
            .env
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, value);
    }
    fn os_getcwd(&self) -> Result<Vec<u8>, crate::native::HostError> {
        // W7-8 — RAW bytes, not `display().to_string()`: a non-UTF-8 cwd used to come back U+FFFD-
        // substituted, naming nothing.
        std::env::current_dir()
            .map(|p| crate::native::fs::path_bytes(&p))
            .map_err(|e| crate::native::HostError {
                message: e.to_string(),
            })
    }
    fn request_exit(&mut self, code: i64) {
        // The LOW 8 BITS of the code, exactly like POSIX `exit(3)` / bash / Python / Go: `-1` → 255,
        // `300` → 44, `-256` → 0. NOT a clamp — clamping a negative code UP to 0 reported a failure
        // exit as SUCCESS to the shell/CI, and `os.exit(-1)` is the canonical failure idiom.
        let code = (code & 0xff) as i32;
        self.vm.pending_exit = Some(code);
        // W7-47 — also publish run-wide, AFTER the mask so it is applied exactly once. `pending_exit`
        // is per-`Vm`: for an eager `Executor` job it is a value only the join observes, and a `main`
        // parked in `accept()`/`recv()` never reaches the join. The run-scoped cell is what lets every
        // blocking loop see the exit (`Vm::run_exit_err`) — Go's `os.Exit` is immediate.
        //
        // W7-57 — the order is: code, then teardown, then flag.
        // 1. the code first, so anything woken by step 2 already reads it;
        // 2. halt every live nursery — a party that is SPINNING, `recv`-parked or asleep reaches no
        //    polling wait, so without this teardown it outlives the exit (a spinner forever);
        // 3. the flag LAST, so `true` publishes both of the above to whoever reads it.
        //
        // Step 3 does NOT make the cancel rung win for a scoped fiber, and an earlier revision of this
        // comment claimed it did. An `Acquire` load orders only the reads AFTER it, while both CPU
        // checkpoints read cancel BEFORE exit — so `cancel == false` + `exit == true` is legal, and the
        // measured result was a sibling `defer` that ran 2/8 times and once died mid-body. That
        // guarantee lives in [`Vm::exit_halt`], which decides from the flag's PRESENCE and so needs no
        // ordering; this sequence is only about publishing the code and the teardown before the hint.
        self.vm.quiesce.request_exit(code);
        self.vm.halt_all_scheds();
        self.vm.quiesce.mark_exit_pending();
    }
}

// `is_numeric` / `as_f64` are now heap-aware methods on `Vm` (arith.rs) — a boxed `BigInt`/`FloatBox`
// is invisible to a free fn that can't reach the heap.

/// Format a float the way Chezzi prints it — CPython `repr()`/`str()` parity: fixed with an
/// always-present `.0` on integer-valued floats, scientific (`1e+16`, `1.5e+300`, `1e-05`) when the
/// decimal exponent is `< -4` or `>= 16`. Single-sourced in `fmtspec` alongside the `{:e}` spec path.
fn format_float(x: f64) -> String {
    crate::fmtspec::repr_float(x)
}

// ===== entry points =====

/// W6-9 — the CAPTURE boundary. The buffered sink is BYTES (so `Writer.write_bytes` reaches an
/// `io.stdout()` backing unchanged), but every test helper and embedder API hands stdout back as a
/// `String`. Decode lossily here — the ONLY place a `U+FFFD` can now appear. `chezzi run` STREAMS
/// (the path a program's bytes actually reach an fd) and never passes through this.
///
/// This decode is NOT a comparison boundary: it is lossy AND not injective (`ff` and `fe` both
/// become one U+FFFD), so an oracle diffing its output would pass a byte-divergent run. Any future
/// byte-exact oracle (CPython differential, golden-file diff) takes a raw-bytes path instead —
/// [`RunOutputRaw`]/[`run_file_bytes`] (W6-9), and the capture-based [`run_capture_bytes`]
/// (W6-9b). Anything comparing two runs' output must do the same: decode for a readable message,
/// then assert on the BYTES.
fn captured(buf: Vec<u8>) -> String {
    String::from_utf8_lossy(&buf).into_owned()
}

/// Run a single-file program from source on the dedicated VM thread; returns output produced so
/// far + the outcome (test entry point).
#[cfg(test)]
pub fn run_program(src: &str) -> (String, Result<(), RuntimeError>) {
    let (out, result) = run_program_bytes(src);
    (captured(out), result)
}

/// [`run_program`] without the lossy [`captured`] decode — the raw sink bytes, for a cross-engine
/// comparison (W6-9b).
#[cfg(test)]
pub fn run_program_bytes(src: &str) -> (Vec<u8>, Result<(), RuntimeError>) {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || run_program_inner(&src))
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

#[cfg(test)]
fn run_program_inner(src: &str) -> (Vec<u8>, Result<(), RuntimeError>) {
    let tokens = match lexer::tokenize(src) {
        Ok(t) => t,
        Err(e) => {
            return (
                Vec::new(),
                Err(RuntimeError {
                    message: e.to_string(),
                    span: Span::RUNTIME,
                    is_assert: false,
                    is_over_memory: false,
                    is_timed_out: false,
                }),
            );
        }
    };
    let module = match parser::parse(tokens) {
        Ok(m) => m,
        Err(e) => {
            return (
                Vec::new(),
                Err(RuntimeError {
                    message: e.message,
                    span: e.span,
                    is_assert: false,
                    is_over_memory: false,
                    is_timed_out: false,
                }),
            );
        }
    };
    let program = match crate::compiler::compile_module_standalone(&module) {
        Ok(p) => p,
        Err(e) => {
            return (
                Vec::new(),
                Err(RuntimeError {
                    message: e.message,
                    span: e.span,
                    is_assert: false,
                    is_over_memory: false,
                    is_timed_out: false,
                }),
            );
        }
    };
    let mut vm = Vm::new(Arc::new(program));
    let result = vm.run().and_then(|()| vm.drain_live_executors());
    (vm.out, result)
}

/// Assert two program outputs contain the SAME lines regardless of order — for a concurrency test
/// whose line ORDER legitimately varies between runs under the M:N scheduler (racing spawns /
/// `Executor` tasks / multi-producer channel drains) while the line SET does not.
///
/// **Both arguments are M:N runs.** Before 2026-08-16 the first was the cooperative `--serial`
/// engine, which delivered a deterministic order, and the exact order was asserted there separately;
/// that engine is gone, so no run of any program pins a cross-task order any more. Comparing two runs
/// of the same program against each other is a *stability* check and nothing more — if a test wants a
/// real expectation, give it a literal line set.
#[cfg(test)]
pub fn assert_same_lines(cooperative: &str, mn: &str) {
    let mut a: Vec<&str> = cooperative.lines().collect();
    let mut b: Vec<&str> = mn.lines().collect();
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(
        a, b,
        "line multiset differs between the two runs\n first:\n{cooperative}\n second:\n{mn}"
    );
}

/// Run a single-file program and return its full stdout, or the error (test helper).
#[cfg(test)]
pub fn run_capture(src: &str) -> Result<String, RuntimeError> {
    run_capture_bytes(src).map(captured)
}

/// [`run_capture`] without the lossy [`captured`] decode — for callers that need the raw bytes
/// (W6-9b), e.g. a golden comparison sensitive to non-UTF-8 output.
#[cfg(test)]
pub fn run_capture_bytes(src: &str) -> Result<Vec<u8>, RuntimeError> {
    let (out, result) = run_program_bytes(src);
    result.map(|()| out)
}

/// Run a single-file program, returning stdout (or error) plus the final live-object count.
/// `stress` collects before every instruction (surfaces missing GC roots); otherwise the normal
/// allocation-threshold trigger drives collection (test helper for GC assertions).
#[cfg(test)]
pub fn run_with(src: &str, stress: bool) -> (Result<String, RuntimeError>, usize) {
    run_with_cfg(src, stress)
}

/// [`run_with`] with the engine selectable, so a GC-stress test can run on the M:N engine too. The
/// eager `Executor` path needs that: its hazards (a submitted closure rebuilt into a worker heap while
/// the parent holds the executor's core lock) exist only on that engine, and the serial-only
/// [`run_capture_stress`] cannot reach them.
#[cfg(test)]
pub fn run_with_cfg(src: &str, stress: bool) -> (Result<String, RuntimeError>, usize) {
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
                            span: Span::RUNTIME,
                            is_assert: false,
                            is_over_memory: false,
                            is_timed_out: false,
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
                            is_assert: false,
                            is_over_memory: false,
                            is_timed_out: false,
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
                            is_assert: false,
                            is_over_memory: false,
                            is_timed_out: false,
                        }),
                        0,
                    );
                }
            };
            let mut vm = Vm::new(Arc::new(program));
            vm.gc_stress = stress;
            let result = vm.run().and_then(|()| vm.drain_live_executors());
            let live = vm.heap.live();
            (result.map(|()| captured(vm.out)), live)
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
                span: Span::RUNTIME,
                is_assert: false,
                is_over_memory: false,
                is_timed_out: false,
            })?;
            let module = parser::parse(tokens).map_err(|e| RuntimeError {
                message: e.message,
                span: e.span,
                is_assert: false,
                is_over_memory: false,
                is_timed_out: false,
            })?;
            let program =
                crate::compiler::compile_module_standalone(&module).map_err(|e| RuntimeError {
                    message: e.message,
                    span: e.span,
                    is_assert: false,
                    is_over_memory: false,
                    is_timed_out: false,
                })?;
            let mut vm = Vm::new(Arc::new(program));
            vm.run()
                .and_then(|()| vm.drain_live_executors())
                .map(|()| captured(vm.out))
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

/// Stdout from a stress-mode run (panics on error) — convenience for parity-under-GC tests. Runs on
/// the M:N engine (via [`run_with`]) so it exercises the eager `Executor` dispatch.
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
                            span: Span::RUNTIME,
                            is_assert: false,
                            is_over_memory: false,
                            is_timed_out: false,
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
                            is_assert: false,
                            is_over_memory: false,
                            is_timed_out: false,
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
                            is_assert: false,
                            is_over_memory: false,
                            is_timed_out: false,
                        }),
                        0,
                    );
                }
            };
            let mut vm = Vm::new(Arc::new(program));
            let result = vm.run().and_then(|()| vm.drain_live_executors());
            let nursery_depth = vm.nurseries.len();
            (result.map(|()| captured(vm.out)), nursery_depth)
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

/// Run a multi-file program from its entry path on the dedicated VM thread: resolve the graph,
/// compile it, run each module once in dependency order, then the entry's `main()`. Output produced
/// so far is preserved alongside the outcome.
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
        Some(entry_fn),
        None,
    )
}

/// A finished run: captured `(stdout, stderr, outcome, exit_code)`. Stderr holds `std.io.eprint`
/// output. `exit_code` is `Some(n)` only when the program called `std.os.exit(n)` (a clean halt,
/// so `outcome` is `Ok`); `None` for a normal end or a runtime error.
pub type RunOutput = (String, String, Result<(), RunError>, Option<i32>);

/// [`RunOutput`] with the sink's RAW BYTES — what the program actually emitted, before the lossy
/// [`captured`] decode: `from_utf8_lossy` is not injective (`ff` and `fe` both become one U+FFFD), so
/// diffing decoded captures would blind any byte-level comparator to a genuinely divergent run — and
/// `Writer.write_bytes` (W6-9) is exactly what makes a non-UTF-8 capture reachable. Callers that need
/// a byte-exact comparison (the CPython differential, `src/difftest/`) take this one; everything else
/// keeps the `String` shape.
pub type RunOutputRaw = (Vec<u8>, Vec<u8>, Result<(), RunError>, Option<i32>);

/// The [`captured`] decode applied to a whole [`RunOutputRaw`] — the one place the `Vec<u8>` sink
/// becomes the `String` every test helper and embedder consumes.
fn to_str_output((out, err, res, code): RunOutputRaw) -> RunOutput {
    (captured(out), captured(err), res, code)
}

/// Like [`run_file`], but with an explicit [`crate::native::HostConfig`] (args/env/stdin) for the
/// native std modules. Test-only convenience over [`run_file_with_entry`] (entry-fn `None`); the
/// CLI calls [`run_file_with_entry`] directly so a `module:function` entrypoint can name a function.
#[cfg(test)]
pub fn run_file_with(entry: &std::path::Path, cfg: crate::native::HostConfig) -> RunOutput {
    to_str_output(run_file_engine(entry, cfg, None, None, false, None))
}

/// Resolve, compile, and run a program from its entry path on the dedicated VM thread, then — if
/// `entry_fn` is `Some` — invoke that named top-level function of the entry module (the
/// `module:function` manifest entrypoint). `None` runs the module top-level only (scripting model).
/// This is the single entry the CLI's `chezzi run` uses.
///
/// `root` pins the module-graph root (the "one root per run" invariant): the bare-`chezzi run`
/// manifest path passes `Some(root)` — the manifest that declared the entrypoint, computed ONCE by
/// the CLI — so every `import` resolves against the SAME root that located the entry file. `None`
/// (the explicit `chezzi run FILE` path) derives the root by walking up from the entry file (nearest
/// marker, unchanged).
pub fn run_file_with_entry(
    entry: &std::path::Path,
    cfg: crate::native::HostConfig,
    entry_fn: Option<&str>,
    root: Option<std::path::PathBuf>,
) -> RunOutput {
    to_str_output(run_file_bytes(entry, cfg, entry_fn, root))
}

/// Like [`run_file_with_entry`], but the entry source is supplied in-memory (`Some`) instead of being
/// re-read from disk on the VM thread. The CLI reads a one-shot fd (a pipe, `/dev/stdin`) exactly ONCE
/// via `main.rs::read_source` and passes the bytes here — a second read of the same fd returns empty,
/// which is the read-once fix for `chezzi run` on a pipe. `None` behaves exactly like
/// [`run_file_with_entry`] (reads the entry from disk).
pub fn run_file_with_entry_source(
    entry: &std::path::Path,
    cfg: crate::native::HostConfig,
    entry_fn: Option<&str>,
    root: Option<std::path::PathBuf>,
    entry_source: Option<String>,
) -> RunOutput {
    to_str_output(run_file_engine(
        entry,
        cfg,
        entry_fn.map(str::to_string),
        root,
        false,
        entry_source,
    ))
}

/// [`run_file_with_entry`] without the lossy decode — the entry point for any caller that needs the
/// raw bytes (see [`RunOutputRaw`]), e.g. a byte-exact test assertion or the CPython differential.
pub fn run_file_bytes(
    entry: &std::path::Path,
    cfg: crate::native::HostConfig,
    entry_fn: Option<&str>,
    root: Option<std::path::PathBuf>,
) -> RunOutputRaw {
    run_file_engine(entry, cfg, entry_fn.map(str::to_string), root, false, None)
}

/// W7-4a — a MULTI-FILE run under GC stress (collect at every safepoint). The single-source
/// `run_capture_stress` cannot reach the lazy per-module fault path, which is exactly where a cell
/// now sits parked in `Vm::snapshot_rebuild` across real safepoints between two modules' faults.
#[cfg(test)]
pub fn run_file_stress(entry: &std::path::Path) -> RunOutput {
    to_str_output(run_file_engine(
        entry,
        crate::native::HostConfig::default(),
        None,
        None,
        true,
        None,
    ))
}

fn run_file_engine(
    entry: &std::path::Path,
    cfg: crate::native::HostConfig,
    entry_fn: Option<String>,
    root: Option<std::path::PathBuf>,
    stress: bool,
    entry_source: Option<String>,
) -> RunOutputRaw {
    let entry = entry.to_path_buf();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || run_file_inner(&entry, cfg, entry_fn.as_deref(), root, stress, entry_source))
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

fn run_file_inner(
    entry: &std::path::Path,
    cfg: crate::native::HostConfig,
    entry_fn: Option<&str>,
    root: Option<std::path::PathBuf>,
    stress: bool,
    entry_source: Option<String>,
) -> RunOutputRaw {
    let build = crate::resolver::build_graph_with_entry_source_and_root(entry, entry_source, root);
    let graph = match build {
        Ok(g) => g,
        Err(e) => {
            return (
                Vec::new(),
                Vec::new(),
                Err(RunError::plain(RuntimeError {
                    message: e.message,
                    span: e.span,
                    is_assert: false,
                    is_over_memory: false,
                    is_timed_out: false,
                })),
                None,
            );
        }
    };
    let program = match crate::compiler::compile_graph(&graph) {
        Ok(p) => p,
        Err(e) => {
            return (
                Vec::new(),
                Vec::new(),
                Err(RunError::plain(RuntimeError {
                    message: e.message,
                    span: e.span,
                    is_assert: false,
                    is_over_memory: false,
                    is_timed_out: false,
                })),
                None,
            );
        }
    };
    let mut vm = Vm::new(Arc::new(program));
    vm.host = cfg;
    vm.gc_stress = stress;
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
        .and_then(|()| vm.drain_live_executors());
    // Memory probe (8B-`Value` gate): report the peak live-bytes high-water mark to real stderr,
    // gated on `CHEZZI_HEAP_STATS=1`. `.max(live_bytes())` covers workloads under the GC threshold
    // that never `sweep()` (peak would otherwise be 0). Real stderr, never `vm.out`/`vm.stderr`, so
    // stdout parity is untouched.
    if std::env::var("CHEZZI_HEAP_STATS").is_ok() {
        let peak = vm.heap.peak_live_bytes().max(vm.heap.live_bytes());
        eprintln!(
            "[heap-stats] peak_live_bytes={} size_of_value={}",
            peak,
            std::mem::size_of::<Value>()
        );
    }
    // A pending exit means `result` is the `exit()` unwind sentinel, not a fault: report the
    // requested code as a clean halt.
    if let Some(code) = vm.pending_exit {
        return (vm.out, vm.stderr, Ok(()), Some(code));
    }
    // The stack trace was captured at the uncaught fault (before frames unwound); attach it.
    let trace = vm.fault_trace.take().unwrap_or_default();
    // Snapshot the file-id → path table from the compiled program while it's still in hand — this is
    // the last point before `vm` (and its `Arc<Program>`) is consumed below.
    let files: Vec<(u32, std::path::PathBuf)> = vm.program.file_table();
    let result = result.map_err(|e| RunError::from_error(e, trace, files));
    (vm.out, vm.stderr, result, None)
}

#[cfg(test)]
mod gc_tests;
#[cfg(test)]
mod golden_tests;
#[cfg(test)]
mod tests;
