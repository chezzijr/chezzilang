//! VM: run entry points (`run_program`, `run_file`, captures).
//! Split out of `src/vm/mod.rs`. `super::*` is the parent module.

use super::*;

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
pub(super) fn run_program_inner(src: &str) -> (Vec<u8>, Result<(), RuntimeError>) {
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
                    is_deadlock: false,
                    is_panic: false,
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
                    is_deadlock: false,
                    is_panic: false,
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
                    is_deadlock: false,
                    is_panic: false,
                }),
            );
        }
    };
    let mut vm = Vm::new(Arc::new(program));
    let result = vm.run();
    let result = vm.finish_run(result);
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
    /// thread that runs the program's top level, except [`MODULE_REPLAYS`].
    ///
    /// Wire nodes `Vm::to_wire_depth` visited (every airlock serialize, speculative ones included).
    pub(crate) static WIRE_ENCODE_NODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Wire nodes `Vm::from_wire_memo` rebuilt (every airlock deserialize).
    pub(crate) static WIRE_DECODE_NODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// `Vm::elem_equal` calls (every element comparison a collection method makes).
    pub(crate) static ELEM_COMPARES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Characters a `str` subscript read to find its one character.
    pub(crate) static STR_INDEX_CHARS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Receiver bytes `Vm::core_method` cloned before dispatching a `str` method.
    pub(crate) static STR_RECV_CLONED_BYTES: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
    /// Module replays (`Vm::fault_module` past its guard) by every task of the run, summed from
    /// `ModuleSnapshot::replays` at the outermost nursery join.
    pub(crate) static MODULE_REPLAYS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
            let out = to_str_output(run_file_inner(
                &entry,
                crate::native::HostConfig::default(),
                None,
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

/// [`run_file`], plus the value `counter` reached during the run (TICKET-233). The graph-path twin
/// of [`run_capture_counting`], for a program that needs its `std` imports resolved.
#[cfg(test)]
pub fn run_file_counting(
    entry: &std::path::Path,
    counter: &'static std::thread::LocalKey<std::cell::Cell<usize>>,
) -> (RunOutput, usize) {
    let entry = entry.to_path_buf();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || {
            counter.with(|c| c.set(0));
            let out = to_str_output(run_file_inner(
                &entry,
                crate::native::HostConfig::default(),
                None,
                None,
                false,
            ));
            (out, counter.with(|c| c.get()))
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
                            is_deadlock: false,
                            is_panic: false,
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
                            is_deadlock: false,
                            is_panic: false,
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
                            is_deadlock: false,
                            is_panic: false,
                        }),
                        0,
                    );
                }
            };
            let mut vm = Vm::new(Arc::new(program));
            vm.gc_stress = stress;
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
                is_deadlock: false,
                is_panic: false,
            })?;
            let module = parser::parse(tokens).map_err(|e| RuntimeError {
                message: e.message,
                span: e.span,
                is_assert: false,
                is_over_memory: false,
                is_timed_out: false,
                is_deadlock: false,
                is_panic: false,
            })?;
            let program =
                crate::compiler::compile_module_standalone(&module).map_err(|e| RuntimeError {
                    message: e.message,
                    span: e.span,
                    is_assert: false,
                    is_over_memory: false,
                    is_timed_out: false,
                    is_deadlock: false,
                    is_panic: false,
                })?;
            let mut vm = Vm::new(Arc::new(program));
            let result = vm.run();
            vm.finish_run(result).map(|()| captured(vm.out))
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
                            is_deadlock: false,
                            is_panic: false,
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
                            is_deadlock: false,
                            is_panic: false,
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
                            is_deadlock: false,
                            is_panic: false,
                        }),
                        0,
                    );
                }
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
pub(super) fn to_str_output((out, err, res, code): RunOutputRaw) -> RunOutput {
    (captured(out), captured(err), res, code)
}

/// Like [`run_file`], but with an explicit [`crate::native::HostConfig`] (args/env/stdin) for the
/// native std modules. Test-only convenience over [`run_file_with_entry`] (entry-fn `None`); the
/// CLI calls [`run_file_with_entry`] directly so a `module:function` entrypoint can name a function.
#[cfg(test)]
pub fn run_file_with(entry: &std::path::Path, cfg: crate::native::HostConfig) -> RunOutput {
    to_str_output(run_file_engine(entry, cfg, None, None, false))
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

/// TICKET-255 — run a graph the CLI already built and checked. `chezzi run` builds the graph one time
/// for its pre-run check and hands it here, so the prelude and every import parse once. The entry
/// source the CLI read from a one-shot fd (a pipe, `/dev/stdin`) is already inside the graph.
pub fn run_graph_with_entry(
    graph: crate::resolver::ModuleGraph,
    cfg: crate::native::HostConfig,
    entry_fn: Option<&str>,
) -> RunOutput {
    let entry_fn = entry_fn.map(str::to_string);
    to_str_output(
        std::thread::Builder::new()
            .stack_size(VM_STACK_BYTES)
            .spawn(move || run_graph_inner(&graph, cfg, entry_fn.as_deref(), false))
            .expect("failed to spawn VM thread")
            .join()
            .expect("VM thread panicked"),
    )
}

/// [`run_file_with_entry`] without the lossy decode — the entry point for any caller that needs the
/// raw bytes (see [`RunOutputRaw`]), e.g. a byte-exact test assertion or the CPython differential.
pub fn run_file_bytes(
    entry: &std::path::Path,
    cfg: crate::native::HostConfig,
    entry_fn: Option<&str>,
    root: Option<std::path::PathBuf>,
) -> RunOutputRaw {
    run_file_engine(entry, cfg, entry_fn.map(str::to_string), root, false)
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
    ))
}

pub(super) fn run_file_engine(
    entry: &std::path::Path,
    cfg: crate::native::HostConfig,
    entry_fn: Option<String>,
    root: Option<std::path::PathBuf>,
    stress: bool,
) -> RunOutputRaw {
    let entry = entry.to_path_buf();
    std::thread::Builder::new()
        .stack_size(VM_STACK_BYTES)
        .spawn(move || run_file_inner(&entry, cfg, entry_fn.as_deref(), root, stress))
        .expect("failed to spawn VM thread")
        .join()
        .expect("VM thread panicked")
}

pub(super) fn run_file_inner(
    entry: &std::path::Path,
    cfg: crate::native::HostConfig,
    entry_fn: Option<&str>,
    root: Option<std::path::PathBuf>,
    stress: bool,
) -> RunOutputRaw {
    let build = crate::resolver::build_graph_with_entry_source_and_root(entry, None, root);
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
                    is_deadlock: false,
                    is_panic: false,
                })),
                None,
            );
        }
    };
    run_graph_inner(&graph, cfg, entry_fn, stress)
}

/// Compile `graph` and run it on the current (VM-sized) stack. The one body behind [`run_file_inner`]
/// and [`run_graph_with_entry`].
pub(super) fn run_graph_inner(
    graph: &crate::resolver::ModuleGraph,
    cfg: crate::native::HostConfig,
    entry_fn: Option<&str>,
    stress: bool,
) -> RunOutputRaw {
    let program = match crate::compiler::compile_graph(graph) {
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
                })),
                None,
            );
        }
    };
    let mut vm = Vm::new(Arc::new(program));
    vm.host = cfg;
    vm.gc_stress = stress;
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
