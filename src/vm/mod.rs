//! Bytecode stack VM (M5) — the sole execution engine. Runs the [`Program`] produced by the
//! compiler on its real-thread M:N scheduler (the cooperative `--serial` scheduler and the
//! original tree-walk `interp` parity oracle have both been removed). M5a: handle-addressed
//! values, no collector yet (the mark-sweep GC lands in M5b).

mod blocking_pool;
pub mod chzstr;
pub mod core;
pub mod crossing;
mod fxhash;
pub mod heap;
pub mod op;
mod poller;
mod pool;
mod quiesce;
mod runner_cache;
pub mod sched_seed;
mod timer;
pub mod value;
mod width;
pub mod wire;

use core::{
    AtomicCore, AtomicIntCore, Backing, ChannelCore, ExecRegistry, ExecutorCore,
    GUARD_DEMOTE_BUDGET, GuardCycle, ListenerCore, ReaderCore, RwSharedCore, SharedCore,
    SocketCore, WriterCore, acquire_update_guard_within,
};
use heap::{Fields, Heap, Identity, MapData, ModuleData, Obj, SetData};
use op::{CapEntry, CapSrc, NO_IC, Op, Program, ProtoId, TID_NONE, WaitMeta};
use quiesce::RunHalt;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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
///
/// `is_deadlock` marks a scheduler deadlock verdict (TICKET-135, D1). The `run_until` Err funnel
/// makes `recover:` transparent to it, and it is re-stamped after every unwind.
///
/// `is_panic` marks a fault raised by `panic(...)`; `run_until`'s catch never relocates it to the
/// user's call into std (TICKET-148).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RuntimeError {
    pub message: String,
    pub span: Span,
    pub is_assert: bool,
    pub is_over_memory: bool,
    pub is_timed_out: bool,
    pub is_deadlock: bool,
    pub is_panic: bool,
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

    /// Stamp the deadlock marker onto this error (see [`RuntimeError::is_deadlock`]). Sibling of
    /// [`RuntimeError::timed_out`] — set at every scheduler verdict site, and forced back on after an
    /// unwind so a mid-unwind `defer` fault cannot strip it and let `recover:` catch the verdict.
    pub(crate) fn deadlock(mut self) -> Self {
        self.is_deadlock = true;
        self
    }

    /// This verdict, reported as the blocking op of one waiter: its own text and span, every
    /// marker kept. The one way a sched's deadlock fault is re-sited.
    pub(crate) fn at_site(&self, msg: &str, span: Span) -> Self {
        let mut e = self.clone();
        e.message = msg.to_string();
        e.span = span;
        e
    }

    /// Mark this fault as raised by `panic(...)` (see [`RuntimeError::is_panic`]).
    pub(crate) fn raised_by_panic(mut self) -> Self {
        self.is_panic = true;
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
/// `pub(crate)` so `checker::eq::EQ_BOUNDS_MAX_IN_PROGRESS` can tie itself to this value directly
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

/// TICKET-126 — counts calls to [`SchedCore::cancelled_scope_awaiting_drain`], the O(scopes) idle-path
/// scan TICKET-118 added. Test-only instrumentation for the behavioural proxy that pins the cancel-
/// generation guard: with the guard, a program that never cancels must not re-run this per idle pass.
#[cfg(test)]
static CANCEL_SCAN_CALLS: AtomicUsize = AtomicUsize::new(0);

/// TICKET-126 (W13-24) — bumped once by every cancel-flag trip ([`trip_cancel_flag`]). Process-wide
/// on purpose: an ancestor flag (an `Executor`'s `shutdown_now`) is shared by scheds the storing site
/// cannot name. Starts at 1 so a fresh sched (`drain_scan_gen == 0`) always scans once.
static CANCEL_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// TICKET-126 (W13-24) — the ONLY way production code sets a scope-cancel or ancestor-cancel flag.
/// Store FIRST, then bump, and bump with `Release` against the `Acquire` load in
/// [`SchedCore::drain_scan_due`]. That release/acquire pair on `CANCEL_GEN` is the ONLY thing that
/// makes the flag store visible to a scanner which sees the new generation: `Relaxed` on both sides
/// establishes no happens-before. There is no mutex to fall back on — `Vm::trip_cancel`
/// (`src/vm/exec.rs:212`) stores an ancestor flag under NO core lock, while
/// `SchedCore::scope_cancel_tripped` reads that same shared flag under the core lock. Under
/// `Relaxed`/`Relaxed` this interleaving is reachable: the trip stores the flag and bumps the
/// generation; `drain_scan_due` loads the new generation, scans, reads the flag as still `false`,
/// returns `None` and RECORDS the new generation; only the NEXT trip re-arms the scan, so the fiber
/// parked under the cancelled ancestor is never drained and `take_runnable` faults it as a deadlock —
/// W13-8, the regression TICKET-118 fixed.
pub(crate) fn trip_cancel_flag(flag: &AtomicBool) {
    flag.store(true, Ordering::Relaxed);
    CANCEL_GEN.fetch_add(1, Ordering::Release);
}

/// TICKET-200 — THE answer to "is this scope cancelled": its own flag or any ancestor's (an
/// `Executor`'s `shutdown_now` trips a flag a job's nursery scope carries in `JoinScope::ancestors`).
/// `SchedCore::scope_cancel_tripped` reads it under the core lock; `poller::register` reads a
/// snapshot of it under the registry lock, so a socket park re-checks the same flags every other
/// park path does (DEC-118).
pub(super) struct ScopeCancel {
    pub(super) own: Arc<AtomicBool>,
    pub(super) ancestors: Vec<Arc<AtomicBool>>,
}

impl ScopeCancel {
    pub(super) fn tripped(&self) -> bool {
        self.own.load(Ordering::Relaxed) || self.ancestors.iter().any(|a| a.load(Ordering::Relaxed))
    }
}

/// A process-wide lock serializing every test that WRITES [`WORKER_OVERRIDE`]. The override is
/// process-global and the harness runs tests on multiple threads, so an unguarded store would change
/// the worker count under every concurrent parallel test. Same shape and same reason as
/// `native::rand::TEST_RNG_LOCK`: hold it across the whole set-run-restore sequence. A test that
/// forces a count must start with [`rerun_in_child`].
#[cfg(test)]
pub(crate) static TEST_WORKER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// TICKET-230: `set_worker_count` sets the cap of the process-wide `width::RUNNERS`, so a test that
/// forces a count throttles every concurrently running lib test. It re-runs itself alone in a child
/// copy of the test binary, which has its own gate. Returns `true` in the parent (the child already
/// ran the body) and `false` in the child.
#[cfg(test)]
pub(crate) fn rerun_in_child(module: &str, name: &str) -> bool {
    if std::env::var_os("CHEZZI_FORCED_COUNT_CHILD").is_some() {
        return false;
    }
    // libtest names a test by its path inside the crate: the caller's `module_path!()` without
    // the crate name, then the fn name.
    let module = module.split_once("::").map_or(module, |(_, rest)| rest);
    let test_path = format!("{module}::{name}");
    let out = std::process::Command::new(std::env::current_exe().expect("current_exe"))
        .args([
            test_path.as_str(),
            "--exact",
            "--test-threads=1",
            "--nocapture",
        ])
        .env("CHEZZI_FORCED_COUNT_CHILD", "1")
        .output()
        .expect("spawn forced-count child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains(" 1 passed;"),
        "forced-count child {test_path} failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    true
}

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
    // TICKET-213 — the auto count is read from the OS once per process: `available_parallelism()`
    // is a syscall plus cgroup file reads, and the task-start path asks several times per task
    // (measured: 2000 Executor round trips at the default count 374 ms, at `--threads=28` 117 ms).
    static AUTO: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    match WORKER_OVERRIDE.load(Ordering::Relaxed) {
        0 => *AUTO.get_or_init(|| {
            std::thread::available_parallelism()
                .map(|x| x.get())
                .unwrap_or(1)
                .max(1)
        }),
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
    /// Module toplevel frame — a `?` unhandled here is a top-level unhandled error that exits the
    /// program; a bare expression statement's value is discarded (`chezzi check` warns, W8-2).
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
    /// TICKET-137 — a module-global generator the task's snapshot could not copy (it was running at the
    /// spawn, or held a value that cannot cross tasks). Inert until driven; any `.next()` faults with
    /// this message instead of reading as `nil` or as exhausted.
    Unsendable(Box<str>),
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
    /// TICKET-190, TICKET-240: the creating call's verdict for each PARAM slot, two bits per param
    /// (layout `crossing::stamp_mask`), set only by `Op::StampGen`. A marking crossing reads it
    /// through `crossing::frame_slot`; a local slot reads the proto's own masks there.
    stamp: u64,
}

// Hand-rolled so `CallFrame`/`Handler`/`Deferred` need not derive `Debug` (keeps the hot call
// record free of an unused derive); prints lifecycle + frame count, not the whole parked stack.
impl std::fmt::Debug for GeneratorCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match &self.state {
            GenState::Pending(_) => "Pending",
            GenState::Suspended => "Suspended",
            GenState::Done => "Done",
            GenState::Unsendable(_) => "Unsendable",
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
    /// `spawn recv.m(args)` is this shape too: its callee is the site's entry thunk and the
    /// receiver is argument 0 (TICKET-235, `Vm::task_entry`).
    Call {
        callee: Value,
        args: Vec<Value>,
        span: Span,
    },
}

impl PendingCall {
    /// The GcRefs this pending task keeps alive (callee + arguments).
    fn roots(&self) -> impl Iterator<Item = GcRef> + '_ {
        let PendingCall::Call {
            callee: head, args, ..
        } = self;
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
/// heap it was built from, the TICKET-111 data-node registry (same keying), and the next free id
/// (monotonic across builds).
type SnapshotBuild = (
    ModuleSnapshot,
    Arc<fxhash::FxHashMap<GcRef, u32>>,
    Arc<fxhash::FxHashMap<GcRef, u32>>,
    u32,
);

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
    /// D4 (TICKET-179) — the spawn op's freshness bitmask, passed to [`Vm::rebuild_ready`]: bit 0 = the
    /// receiver, bit `i + 1` = arg `i`. `0` for a `spawn:` block.
    fresh: u32,
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

/// TICKET-215 — what names a bare fn value in [`Vm::fn_values`]: a Chezzi fn item by proto, a native
/// std fn by member name + fn pointer, an extern fn by its shared `Arc<Cffi>` address, a universe
/// builtin by name (TICKET-226).
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum FnKey {
    Proto(ProtoId),
    Native(Box<str>, usize),
    Cffi(usize),
    Builtin(Box<str>),
}

/// TICKET-226 — how a callable value runs: [`Vm::callable`]'s answer, read by call, `lower_task`,
/// `Executor.submit` and the entrypoint. Holds only `Copy`/cheap fields, so reading it allocates
/// nothing on the hot user-fn/closure paths.
pub(crate) enum Callee {
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
        kind: crate::native::Kind,
    },
    Builtin(Box<str>),
    Cffi(std::sync::Arc<crate::native::cffi::Cffi>),
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
    /// TICKET-215 — the one fn object per [`FnKey`] in this heap (see [`Vm::fn_value`]). GC roots,
    /// heap-keyed like `str_intern`, swapped with the heap in [`Vm::swap_ctx`].
    fn_values: fxhash::FxHashMap<FnKey, GcRef>,
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
    /// TICKET-246 — the open box calls of the task running on this `Vm`, innermost last (see
    /// `boxctx.rs`). A `Vm` field, NOT a `FiberCtx` field, and `swap_ctx` does not move it: a box
    /// closure runs under `Vm::reentered`, where no other fiber runs on this stack (DEC-245).
    box_chain: Vec<boxctx::BoxFrame>,
    /// A fault from seating the caller's view at a site that cannot return one
    /// (`ensure_module_faulted`). `read_slot` raises it; `with_box` clears it.
    box_view_err: Option<RuntimeError>,
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
    /// TICKET-048 — parallel to [`Vm::nurseries`] (same length, pushed and popped in lockstep): the
    /// source span of the construct that OPENED each nursery — the `parallel:` keyword, or the first
    /// statement of an implicit nursery's block. Read when building the `DEADLOCK_MSG` fault so its
    /// headline names a real `path:line:col` instead of `Span::RUNTIME`.
    nursery_spans: Vec<Span>,
    /// Per-connection spawn — parallel to [`Vm::nurseries`] (lockstep): `Some` for an eager nursery
    /// (entered under `--parallel` inside a fiber), `None` for a lazy/top-level one. A `spawn` whose
    /// innermost nursery is eager injects its handler fiber straight into the scope's sched (runs
    /// concurrently with the body) instead of queueing it for the join. See [`EagerScope`].
    eager_scheds: Vec<Option<EagerScope>>,
    /// TICKET-103 — the scope id of the fiber this shell is running; set on every swap-in by
    /// `run_one_fiber`.
    fiber_scope: Option<usize>,
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
    /// TICKET-103 — origin scope of the fiber-owned nursery whose `JoinNursery` parked the running
    /// fiber; consumed by `run_one_fiber` into `Disp::JoinPark`.
    join_suspend: Option<usize>,
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
    /// TICKET-208 — where and why the running fiber asked to park: the deadlock text the in-place
    /// path raises for the same op, and the op's span. Set beside `suspend` / `send_suspend` /
    /// `wait_suspend`, moved to [`Fiber::park_site`] by `run_one_fiber`.
    park_site: Option<(&'static str, Span)>,
    /// TICKET-185 — the blocked party this `Vm` is running (a parked `send`'s offer, a parked
    /// `recv`'s slot, a `wait:`'s offers and slots), carried across a park by [`Fiber::pending`].
    /// The re-run op settles it first ([`crate::vm::core::PendingOp::settle`]).
    pending: Option<crate::vm::core::PendingOp>,
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
    /// `debug_assert` in [`Vm::from_wire`]). [`Vm::from_wire_pieces`] clears it before each attempt and
    /// re-rebuilds the whole container when it trips. This used to be a `.expect` that ABORTED THE HOST
    /// on a legal program. Pure scratch — cleared and read inside one helper call, never across a park,
    /// so it is a `Vm` field and NOT part of [`FiberCtx`].
    wire_backref_missing: bool,
    /// D4 layer C (TICKET-169): true while `from_wire_memo`/`replay_snap` is running an airlock
    /// copying walk (`rebuild_ready`'s captures/args/receiver, `fault_module`'s global snapshot, or
    /// a crossing `Closure`/`Generator`'s captures). Every object allocated while this is set gets
    /// `Heap::set_copied`, so a later write to it faults. Pure scratch, toggled and restored around
    /// one call — never part of [`FiberCtx`].
    pub(super) copy_mark: bool,
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
    /// W15-3 — bytes of the in-flight `write` already sent; survives a park's ip-rewind re-run,
    /// cleared by `drop_poll_latch`.
    poll_written: Option<usize>,
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
    /// descendants, so every checkpoint reads these too ([`Vm::halt_requested`]) — a nested nursery
    /// keeps its OWN `cancel` (an inner fault must not cancel an outer sibling) but its fibers still
    /// die when an outer scope is cancelled. Re-pointed per fiber swap-in from
    /// [`JoinScope::ancestors`], exactly like `cancel`.
    cancel_outer: Vec<Arc<AtomicBool>>,
    /// TICKET-195 — why this party is unwinding when the fault is not its own ([`block::Cut`]).
    /// `Cut::Cancelled` (B3.4): this worker observed [`Vm::cancel`] and bailed, so the join can tell
    /// a swallowed cooperative abort apart from a real fault. `Cut::Delivered { floor }`
    /// (TICKET-096): a recorded child fault — or a reduced job fault — is unwinding this owner; with
    /// `floor = Some(n)`, a `recover:` installed INSIDE nursery `n`'s body must not catch it, while
    /// one installed outside it still must (`tests/chz/spec/nursery_fault_verdict_test.chz`). Not in
    /// [`FiberCtx`] — like `pending_exit`, it is reset at every fiber schedule-in.
    cut: Option<block::Cut>,
    /// TICKET-147 (W14-12): set by a cancel funnel when a `defer` (or a nested nursery it aborted)
    /// replaced the `cancelled` sentinel with a real, non-deadlock fault. Read (and cleared) by
    /// [`Vm::classify_mn_outcome`], which then reports `CancelledFault` instead of swallowing it.
    cancel_unwind_faulted: bool,
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
    /// See [`Vm::halt_requested`].
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
    /// 2. a task start ([`Vm::fresh_view`]), iff the cached snapshot is NOT `reusable` — some global
    ///    holds a mutable aggregate, which can be mutated IN PLACE (`q.push(1)`) with no slot write
    ///    for rule 1 to see — AND its `view` stamp is stale: `Heap::get_mut` handed out an object
    ///    the snapshot reaches since the build (TICKET-213). An unmutated view and an all-immutable
    ///    view keep one snapshot.
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
    /// TICKET-111 — the same registry as `snapshot_cells`, but for adoptable DATA nodes
    /// (`Vm::is_adoptable_node`): every data node a module global reached in THIS view's snapshot,
    /// keyed by its `GcRef` in the heap the snapshot was built from. Seeds `WireMemo::base_nodes` so a
    /// spawn-crossed alias of the same node mints the SAME id, and `snapshot_adopt` then ties the two
    /// crossings to one object during `fault_module`'s replay only. Reset alongside `snapshot_cells`
    /// everywhere that field is.
    snapshot_nodes: Arc<fxhash::FxHashMap<GcRef, u32>>,
    /// TICKET-111 — this task's spawn-crossed captures, keyed by their adoption id, waiting to be
    /// installed as the module global's own object the first time `fault_module` replays that
    /// global. `rebuild_ready` populates it (`prepare_worker`'s adopt ids); `fault_module`'s
    /// container/generator arms consult it ONLY while `adopt_active`, and each entry is removed once
    /// consumed. GC-rooted (its VALUES): a module the task never reads keeps its entries alive for the
    /// task's whole life, bounded by what the task's own captures held at spawn. Cleared in
    /// `install_snapshot`.
    snapshot_adopt: fxhash::FxHashMap<u32, GcRef>,
    /// TICKET-111 — true ONLY for the duration of `fault_module`'s own globals-replay loop. Adoption
    /// must be scoped this tightly: `RwShared.slice` (`src/vm/netio.rs`) relies on `from_wire_memo`'s
    /// container arms NOT deduping on any other path, and a `Channel` message's ids are a separate id
    /// space that could otherwise collide with an adopt id (see `airlock_adoption_is_scoped_to_the_snapshot_replay`).
    adopt_active: bool,
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
    /// W13-20 — the generator-side frames of a fault that propagated OUT of a `generator_next`
    /// resume, waiting to be prepended to the DRIVER-side capture. Consumed exactly once, by the
    /// dispatch loop's error arm (`exec.rs`'s uncaught arm — a fault that reaches that arm for the
    /// FIRST time takes it there). A fault a `recover:` catches instead never reaches that arm, so
    /// EVERY catch path that can run a deferred `.next()` after this slot might already be set must
    /// clear it explicitly, or it survives to decorate a later, unrelated uncaught fault: `exec.rs`'s
    /// `recover:`-catch arm (after BOTH its defer drains — `unwind_deferred` for frames above the
    /// boundary and `drain_frame_to` for the recover block's own defers, since a `defer it.next()`
    /// can sit in either), and `stmt.rs`'s recover-scoped `?` catch (`do_try`, after both of ITS
    /// drains). Grep for `gen_fault_prefix.clear()` for the full list — a THIRD catch path added
    /// later needs the same clear, derived the same way (every drain it runs after this slot could
    /// already be set), not by pattern-matching the two here. Deliberately absent from `FiberCtx`,
    /// `GenCtx`, `Vm::swap_ctx` and `Vm::swap_gen_ctx`, for the same reason `gen_host_ctx` is: a
    /// fiber cannot park mid-resume.
    gen_fault_prefix: Vec<TraceFrame>,
    /// TICKET-150 (W14-35c) — the list/map/set/struct objects whose default `print`/`str` render is
    /// currently OPEN, innermost last (CPython's `Py_ReprEnter`). An object met again while open is a
    /// back edge and renders as `[...]` / `{...}` / `Set(...)` / `...`. An in-progress STACK, not a
    /// visited set, so a shared but acyclic child (`[x, x]`) still prints in full. Every push is
    /// popped on both the `Ok` and `Err` path. Deliberately absent from `FiberCtx` and `GenCtx`:
    /// a render never parks (a `str` hook runs under `guarded_walk`), so no other fiber can observe it.
    repr_active: Vec<GcRef>,
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
    /// TICKET-205 — this `Vm` cannot leave its OS thread at a slice end (an Executor job, a gated
    /// nursery body), so it counts reductions and hands its runner slot over in place.
    slice_in_place: bool,
    /// TICKET-230 — the run's main thread holds one runner permit from `Vm::run` until the root
    /// `Vm` drops.
    run_permit: Option<width::RunPermit>,
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

/// TICKET-213 — how long an Executor's runner thread waits for a next job before it leaves. It
/// decides thread reuse only, never what a program computes: a `submit` during the wait wakes the
/// runner through the sched's condvar, and one after it starts a fresh runner as before.
const EXEC_RUNNER_LINGER: std::time::Duration = std::time::Duration::from_millis(5);

mod arith;
mod block;
mod boxctx;
mod compare;
mod equality;
mod hash;
#[cfg(test)]
use block::Waiter;
use block::{BlockMode, DemoteReg, WaitSpec};
mod call;
mod core_method;
mod json_decode;
mod list_hof;
mod method_call;
pub(crate) use call::is_mutating_native_kind;
mod airlock_in;
mod airlock_out;
mod blocking;
mod builtins;
mod channel;
mod demote;
mod display;
mod exec;
mod field_index;
mod fileio;
mod frames;
mod globals;
mod netio;
mod nursery;
mod range;
mod sched;
mod shared_methods;
mod socket;
mod step;
mod stmt;
mod worker_loop;
mod worker_vm;
pub(crate) use boxctx::CROSSING_CLOSURE_METHODS;
pub(crate) use stmt::{COPY_WRITE_TAIL, OPTION_VALUE_WORD, RESULT_VALUE_WORD, carrier_word_for};
mod sched_types;
mod stream;
use sched_types::*;
mod mnsched;
mod mnsched_park;
mod mnsched_take;
mod mnsched_verdict;
use mnsched::*;
mod sched_core;
use sched_core::*;
mod host;
pub(crate) use host::*;
mod entry;
pub use entry::*;

pub use stream::{flush_stream, out_dead_reason, stream_error};

#[cfg(test)]
mod gc_tests;
#[cfg(test)]
mod golden_tests;
#[cfg(test)]
mod tests;
