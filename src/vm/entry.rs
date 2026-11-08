//! VM: run entry points (`run_program`, `run_file`, captures).
//! Split out of `src/vm/mod.rs`. `super::*` is the parent module.

use super::*;

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
pub(super) fn captured(buf: Vec<u8>) -> String {
    String::from_utf8_lossy(&buf).into_owned()
}

/// Run a single-file program from source on the dedicated VM thread; returns output produced so
/// far + the outcome (test entry point).
#[cfg(test)]
pub fn run_program(src: &str) -> (String, Result<(), RuntimeError>) {
    let (out, result) = run_program_bytes(src);
    (captured(out), result)
}

/// [`run_program`] without the lossy [`captured`] decode — the raw sink bytes.
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

/// Compile a test program through the module graph, as `chezzi run` does; a front-end error is
/// the run's error.
#[cfg(test)]
pub(super) fn test_program(src: &str) -> Result<Program, RuntimeError> {
    crate::compiler::compile_source(src).map_err(|e| RuntimeError {
        message: e.message,
        span: e.span,
        is_assert: false,
        is_over_memory: false,
        is_timed_out: false,
        is_deadlock: false,
        is_panic: false,
        blocked: None,
    })
}

#[cfg(test)]
pub(super) fn run_program_inner(src: &str) -> (Vec<u8>, Result<(), RuntimeError>) {
    let program = match test_program(src) {
        Ok(p) => p,
        Err(e) => return (Vec::new(), Err(e)),
    };
    let mut vm = Vm::new(Arc::new(program));
    let result = vm.run();
    let result = vm.finish_run(result);
    (vm.out, result)
}

/// Assert two program outputs contain the SAME lines regardless of order — for a concurrency test
/// whose line ORDER legitimately varies between runs (racing spawns / `Executor` tasks /
/// multi-producer channel drains) while the line SET does not.
///
/// No run pins a cross-task order. Comparing two runs of the same program against each other is a
/// *stability* check and nothing more — if a test wants a real expectation, give it a literal line set.
#[cfg(test)]
pub fn assert_same_lines(first: &str, second: &str) {
    let mut a: Vec<&str> = first.lines().collect();
    let mut b: Vec<&str> = second.lines().collect();
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(
        a, b,
        "line multiset differs between the two runs\n first:\n{first}\n second:\n{second}"
    );
}

#[cfg(test)]
thread_local! {
    /// TICKET-164 — scheduler picks (`Take::Run` returns) made by the current test's own VM run.
    /// Thread-local, same reason as `core::CORE_PROBES`: the lib suite runs tests concurrently, so a
    /// process-global counter would be polluted by another test's run. Published by the outermost
    /// nursery join (`sched::run_mn_nursery_outermost` / `sched::join_eager_nursery`) on the VM's own
    /// thread, then read here by [`run_capture_counting_picks`].
    pub(crate) static RUN_PICKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// TICKET-233 — costs of the current test's own run, counted instead of timed so CPU load
    /// cannot move them. Each is read by [`run_capture_counting`] and counts only work done on the
    /// thread that runs the program's top level.
    ///
    /// `Vm::elem_equal` calls (every element comparison a collection method makes).
    pub(crate) static ELEM_COMPARES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Characters a `str` subscript read to find its one character.
    pub(crate) static STR_INDEX_CHARS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Receiver bytes `Vm::core_method` cloned before dispatching a `str` method.
    pub(crate) static STR_RECV_CLONED_BYTES: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    /// TICKET-211 — `(peak runner threads, runner slot denials, runner thread spawns, peak runner
    /// permit holders)` of the current test's own run, set by `run_file_inner` from the run's
    /// `QuiesceState`; read by [`run_file_counting_runners`]. The fourth is TICKET-230's.
    pub(crate) static RUN_PEAK_RUNNERS: std::cell::Cell<(usize, usize, usize, usize)> =
        const { std::cell::Cell::new((0, 0, 0, 0)) };
}

/// [`run_capture`], plus the number of scheduler picks the run made (TICKET-164). Runs on its own
/// thread like [`run_program_bytes`], since [`RUN_PICKS`] is thread-local to the VM's own thread.
#[cfg(test)]
pub fn run_capture_counting_picks(src: &str) -> (Result<String, RuntimeError>, usize) {
    run_capture_counting(src, &RUN_PICKS)
}

/// [`run_capture`], plus the value `counter` reached during the run (TICKET-233). Runs on its own
/// thread like [`run_program_bytes`], since every counter is thread-local to the VM's own thread.
#[cfg(test)]
pub fn run_capture_counting(
    src: &str,
    counter: &'static std::thread::LocalKey<std::cell::Cell<usize>>,
) -> (Result<String, RuntimeError>, usize) {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            counter.with(|c| c.set(0));
            let (out, result) = run_program_inner(&src);
            (result.map(|()| captured(out)), counter.with(|c| c.get()))
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

/// Run the file at `entry` and return its output, the run's peak runner-thread count, its runner
/// slot denials, the OS threads it started for non-Executor runners (TICKET-211) and its peak
/// runner permit holders (TICKET-230). Runs on its own thread, since [`RUN_PEAK_RUNNERS`] is
/// thread-local to the VM's own thread.
#[cfg(test)]
pub fn run_file_counting_runners(
    entry: &std::path::Path,
) -> (RunOutput, usize, usize, usize, usize) {
    let entry = entry.to_path_buf();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            RUN_PEAK_RUNNERS.with(|p| p.set((0, 0, 0, 0)));
            let out = to_str_output(run_built(
                || crate::compiler::compile_entry_file(&entry, None),
                crate::native::HostConfig::default(),
                None,
                false,
            ));
            let (peak, denials, spawns, permits) = RUN_PEAK_RUNNERS.with(|p| p.get());
            (out, peak, denials, spawns, permits)
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
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

/// The body of [`run_with`]: compile, run on a fresh VM thread, report output and live objects.
#[cfg(test)]
pub fn run_with_cfg(src: &str, stress: bool) -> (Result<String, RuntimeError>, usize) {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            let program = match test_program(&src) {
                Ok(p) => p,
                Err(e) => return (Err(e), 0),
            };
            let mut vm = Vm::new(Arc::new(program));
            vm.gc_stress |= stress;
            let result = vm.run();
            let result = vm.finish_run(result);
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
/// [`VM_STACK_BYTES`].
#[cfg(test)]
pub fn run_capture_on_stack(src: &str, stack_bytes: usize) -> Result<String, RuntimeError> {
    let src = src.to_string();
    std::thread::Builder::new()
        .stack_size(stack_bytes)
        .spawn(move || {
            let program = test_program(&src)?;
            let mut vm = Vm::new(Arc::new(program));
            let result = vm.run();
            vm.finish_run(result).map(|()| captured(vm.out))
        })
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

/// Stdout from a stress-mode run (panics on error) — convenience for GC-stress tests (via
/// [`run_with`]).
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
            let program = match test_program(&src) {
                Ok(p) => p,
                Err(e) => return (Err(e), 0),
            };
            let mut vm = Vm::new(Arc::new(program));
            let result = vm.run();
            let result = vm.finish_run(result);
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
/// `module:function` manifest path; test helper).
#[cfg(test)]
pub fn run_file_entry(entry: &std::path::Path, entry_fn: &str) -> RunOutput {
    to_str_output(run_file_engine(
        entry,
        crate::native::HostConfig::default(),
        Some(entry_fn),
        None,
        false,
    ))
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
pub fn to_str_output((out, err, res, code): RunOutputRaw) -> RunOutput {
    (captured(out), captured(err), res, code)
}

/// Like [`run_file`], but with an explicit [`crate::native::HostConfig`] (args/env/stdin) for the
/// native std modules (test helper).
#[cfg(test)]
pub fn run_file_with(entry: &std::path::Path, cfg: crate::native::HostConfig) -> RunOutput {
    to_str_output(run_file_engine(entry, cfg, None, None, false))
}

/// W7-4a — a MULTI-FILE run under GC stress (collect at every safepoint), for what the
/// single-source `run_capture_stress` cannot reach: globals of several modules.
#[cfg(test)]
pub fn run_file_stress(entry: &std::path::Path) -> RunOutput {
    to_str_output(run_file_engine(
        entry,
        crate::native::HostConfig::default(),
        None,
        None,
        true,
    ))
}

/// Resolve, compile and run the file at `entry` on the dedicated VM thread (test helper). The CLI's
/// own driver is `crate::run::run_file_bytes`, the same two calls.
#[cfg(test)]
pub fn run_file_engine(
    entry: &std::path::Path,
    cfg: crate::native::HostConfig,
    entry_fn: Option<&str>,
    root: Option<std::path::PathBuf>,
    stress: bool,
) -> RunOutputRaw {
    let entry = entry.to_path_buf();
    let entry_fn = entry_fn.map(str::to_string);
    on_vm_stack(move || {
        run_built(
            || crate::compiler::compile_entry_file(&entry, root),
            cfg,
            entry_fn.as_deref(),
            stress,
        )
    })
}

/// Build the program with `make` and run it on the current stack, then — if `entry_fn` is `Some` —
/// invoke that named top-level function of the entry module (the `module:function` manifest
/// entrypoint). The caller supplies the front end, so the VM does not depend on it. Call it inside
/// [`on_vm_stack`]: `make` resolves and compiles on the VM's stack, as the run does.
pub fn run_built(
    make: impl FnOnce() -> Result<Program, crate::base::op::CompileError>,
    cfg: crate::native::HostConfig,
    entry_fn: Option<&str>,
    stress: bool,
) -> RunOutputRaw {
    let program = match make() {
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
                    is_deadlock: false,
                    is_panic: false,
                    blocked: None,
                })),
                None,
            );
        }
    };
    let mut vm = Vm::new(Arc::new(program));
    vm.host = cfg;
    // `Vm::new` already set it from `CHEZZI_GC_STRESS`.
    vm.gc_stress |= stress;
    // On a clean finish, gracefully reap any Executor never explicitly shut down (C5 / A2). A
    // deadlock verdict still drains, so finished jobs' buffered output reaches the sink (TICKET-195);
    // skipped on any other fault (the program is already erroring) and on a hard `std.os.exit`
    // (`Vm::finish_run`).
    let result = vm
        .run()
        // A `module:function` entrypoint calls the named function once the modules are initialized.
        .and_then(|()| match entry_fn {
            Some(name) => vm.invoke_entrypoint(name),
            None => Ok(()),
        });
    let result = vm.finish_run(result);
    #[cfg(test)]
    RUN_PEAK_RUNNERS.with(|p| {
        p.set((
            vm.quiesce.peak_runner_threads.load(Ordering::SeqCst),
            vm.quiesce.runner_slot_denials.load(Ordering::SeqCst),
            vm.quiesce.runner_spawns.load(Ordering::SeqCst),
            vm.quiesce.width_probe.peak.load(Ordering::SeqCst),
        ))
    });
    // Memory probe (8B-`Value` gate): report the peak live-bytes high-water mark to real stderr,
    // gated on `CHEZZI_HEAP_STATS=1`. `.max(live_bytes())` covers workloads under the GC threshold
    // that never `sweep()` (peak would otherwise be 0). Real stderr, never `vm.out`/`vm.stderr`, so
    // stdout parity is untouched.
    // M1.5 — and the run's stops of the world: how many, and the longest wait for one.
    if std::env::var("CHEZZI_HEAP_STATS").is_ok() {
        let peak = vm.heap.peak_live_bytes().max(vm.heap.live_bytes());
        let (stw, stw_max) = vm.gc_coord.stop_stats();
        eprintln!(
            "[heap-stats] peak_live_bytes={} size_of_value={} stw={} stw_max_us={}",
            peak,
            std::mem::size_of::<Value>(),
            stw,
            stw_max.as_micros()
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
