//! `chezzi test` — the Rust-side test runner (M20).
//!
//! Discovers `*_test.chz` files, compiles each as its own entry graph, and invokes every `test fn`
//! (free tests + suite methods) on a reusable VM. The runner is Rust-side by necessity: a Chezzi
//! `recover:` only hands back the fault *message*, not its `span`, so only Rust catching the
//! `RuntimeError` directly gets the `.span` (hence `file:line`) the headline feature needs.
//!
//! Only the `assert` primitive is dual-engine (parity discipline); this orchestration is VM-only —
//! its output is Rust-formatted `PASS/FAIL`, not Chezzi program stdout, so no golden parity applies.

use crate::vm::Vm;
use crate::vm::op::Program;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The outcome of a `chezzi test` run: the rendered report and whether everything passed.
pub struct TestReport {
    pub text: String,
    pub passed: bool,
}

/// One test's verdict. `assert` is the ONE intended failure signal of a (void) `test fn`, so an
/// `assert` fault is a `Fail`; any OTHER runtime fault (OOB, div-by-zero, missing key, native fault,
/// a crashed setup hook) is an unexpected `Error` — the pytest FAILED-vs-ERROR distinction.
///
/// Extension point for the ergonomics wave: new buckets (`TimedOut`, `OverMemory`, …) become new
/// variants here; the render loop + summary + `passed` flag fan out from a single `match`.
enum Verdict {
    Pass,
    Fail {
        line: usize,
        msg: String,
    },
    Error {
        line: usize,
        msg: String,
    },
    /// `chezzi test --max-heap` — the test's live heap exceeded the cap and it was hard-aborted. No
    /// meaningful source line (the abort fires at a GC boundary, not a statement). Counts as failure.
    OverMemory {
        msg: String,
    },
    /// `chezzi test --timeout` — the test ran longer than the wall-clock cap and was hard-aborted at a
    /// loop back-edge. No meaningful source line (the abort fires at a checkpoint, not a statement).
    /// M:N-engine-only; counts as failure.
    TimedOut {
        msg: String,
    },
}

/// One test's result (for the report + summary counts).
struct Outcome {
    /// The test's name (`fn_name` for a free test, `Suite::method` for a suite test).
    name: String,
    /// The `*_test.chz` file the test came from (the `file` half of `file:line`).
    file: String,
    /// Pass / Fail (assert) / Error (any other fault).
    verdict: Verdict,
}

/// Bucket a caught per-test `RuntimeError`. The `is_over_memory` marker takes priority: a `--max-heap`
/// hard-abort is `OverMemory` regardless of what fault emerged from the unwind (a defer may fault while
/// unwinding — the abort re-stamps the marker onto whatever propagates, and it crosses the worker→
/// parent boundary WITH the error, so a spawned task's runaway aborts on either engine). Else an
/// `assert` failure → `Fail`, anything else → `Error`.
fn verdict_from_fault(e: crate::vm::RuntimeError) -> Verdict {
    // `is_timed_out` checked FIRST. It is mutually exclusive with `is_over_memory` in practice (a
    // timeout trips at a loop back-edge, over-memory at a GC boundary — a single abort carries one
    // marker), so the order only fixes a nominal tie; either way the abort buckets, never FAIL/ERROR.
    if e.is_timed_out {
        Verdict::TimedOut { msg: e.message }
    } else if e.is_over_memory {
        Verdict::OverMemory { msg: e.message }
    } else if e.is_assert {
        Verdict::Fail {
            line: e.span.line,
            msg: e.message,
        }
    } else {
        Verdict::Error {
            line: e.span.line,
            msg: e.message,
        }
    }
}

/// Run every `test fn` discovered under `root` (a single `*_test.chz` file or a directory walked
/// recursively). Returns the rendered report + overall pass/fail. Never panics on a test fault — the
/// VM stays reusable, so one failing test does not abort the rest.
pub fn run_tests(root: &Path, parallel: bool) -> TestReport {
    run_tests_capped(root, parallel, 0)
}

/// Like [`run_tests`], plus the opt-in `--max-heap` per-test cap (`max_heap`: byte count, `0` = OFF).
/// A test whose in-VM live heap exceeds the cap is hard-aborted (bypassing `recover:`) and bucketed
/// [`Verdict::OverMemory`] (counts as failure). Deterministic-in-VM (not OS RSS), so the serial == M:N
/// gate holds. With `max_heap == 0` this is byte-identical to the pre-cap runner.
pub fn run_tests_capped(root: &Path, parallel: bool, max_heap: usize) -> TestReport {
    run_tests_timed(root, parallel, max_heap, 0)
}

/// Like [`run_tests_capped`], plus the opt-in `--timeout` per-test wall-clock cap (`timeout_ms`: ms,
/// `0` = OFF). A test running longer than `timeout_ms` is hard-aborted at a loop back-edge (bypassing
/// `recover:`) and bucketed [`Verdict::TimedOut`] (counts as failure). M:N-engine-only (a wall-clock
/// trip is non-deterministic → the CLI rejects it with `--serial`). With `timeout_ms == 0` this is
/// byte-identical to the un-timed runner.
pub fn run_tests_timed(
    root: &Path,
    parallel: bool,
    max_heap: usize,
    timeout_ms: u64,
) -> TestReport {
    let files = match collect_test_files(root) {
        Ok(f) => f,
        Err(e) => {
            return TestReport {
                text: format!("chezzi test: {e}\n"),
                passed: false,
            };
        }
    };
    if files.is_empty() {
        return TestReport {
            text: format!("no *_test.chz files found under {}\n", root.display()),
            passed: false,
        };
    }

    let mut outcomes: Vec<Outcome> = Vec::new();
    let mut report = String::new();
    // A compile/type/resolve error fails a whole file before any test runs. Tracked separately from
    // per-test outcomes so it is reported ONCE (as `ERROR`) and does not inflate the test counts with
    // a phantom `FAIL …:0`.
    let mut file_errors = 0usize;

    for file in &files {
        match run_file(file, parallel, max_heap, timeout_ms) {
            Ok(mut file_outcomes) => outcomes.append(&mut file_outcomes),
            Err(msg) => {
                report.push_str(&format!("ERROR {}\n  {msg}\n", file.display()));
                file_errors += 1;
            }
        }
    }

    // Per-test lines, then a summary.
    for o in &outcomes {
        match &o.verdict {
            Verdict::Pass => report.push_str(&format!("PASS {} ({})\n", o.name, o.file)),
            Verdict::Fail { line, msg } => {
                report.push_str(&format!("FAIL {} ({}:{}) {}\n", o.name, o.file, line, msg))
            }
            Verdict::Error { line, msg } => {
                report.push_str(&format!("ERROR {} ({}:{}) {}\n", o.name, o.file, line, msg))
            }
            Verdict::OverMemory { msg } => {
                report.push_str(&format!("OVER-MEMORY {} ({}) {}\n", o.name, o.file, msg))
            }
            Verdict::TimedOut { msg } => {
                report.push_str(&format!("TIMED-OUT {} ({}) {}\n", o.name, o.file, msg))
            }
        }
    }
    let total = outcomes.len();
    let failed = outcomes
        .iter()
        .filter(|o| matches!(o.verdict, Verdict::Fail { .. }))
        .count();
    let errored = outcomes
        .iter()
        .filter(|o| matches!(o.verdict, Verdict::Error { .. }))
        .count();
    let over_memory = outcomes
        .iter()
        .filter(|o| matches!(o.verdict, Verdict::OverMemory { .. }))
        .count();
    let timed_out = outcomes
        .iter()
        .filter(|o| matches!(o.verdict, Verdict::TimedOut { .. }))
        .count();
    let passed_count = total - failed - errored - over_memory - timed_out;
    report.push_str(&format!(
        "\n{total} test(s): {passed_count} passed, {failed} failed, {errored} errored"
    ));
    // Only surface the over-memory clause when there is one, so the common (cap-off) output stays
    // byte-identical to the pre-cap runner.
    if over_memory > 0 {
        report.push_str(&format!(", {over_memory} over-memory"));
    }
    // Same for the timeout clause — off-by-default so a timeout-OFF run is byte-identical.
    if timed_out > 0 {
        report.push_str(&format!(", {timed_out} timed out"));
    }
    if file_errors > 0 {
        report.push_str(&format!(", {file_errors} file error(s)"));
    }
    // Zero discovered tests with no file errors is a failure, mirroring the "no *_test.chz files
    // found" exit code above: an accidentally-empty test file (every `test` keyword forgotten) must
    // not pass silently and read as a green run to a CI gate.
    let no_tests_discovered = total == 0 && file_errors == 0;
    if no_tests_discovered {
        report.push_str(" — no tests discovered");
    }
    report.push('\n');

    TestReport {
        text: report,
        passed: failed == 0
            && errored == 0
            && over_memory == 0
            && timed_out == 0
            && file_errors == 0
            && !no_tests_discovered,
    }
}

/// Compile + run one `*_test.chz` file on the selected engine (`parallel`: `false` = cooperative
/// serial VM, `true` = M:N OS-thread VM), returning a per-test outcome list (or a compile-error
/// message for the whole file). Compilation is engine-independent and stays on the caller's thread;
/// BOTH engine runs then dispatch on a [`crate::vm::on_vm_stack`] thread — the M:N scheduler needs
/// the large VM stack, and the SERIAL VM needs it too for deep structural recursion (a cyclic-key
/// `==` walks to `MAX_STRUCTURAL_DEPTH` = 10000 before faulting recoverably, which overflows the
/// 8 MB main thread but not the 384 MB VM stack). This matches `chezzi run` (both engines run on
/// [`crate::vm::run_file_with_entry`]'s VM-stack thread), so a `test` verdict mirrors a `run`.
fn run_file(
    file: &Path,
    parallel: bool,
    max_heap: usize,
    timeout_ms: u64,
) -> Result<Vec<Outcome>, String> {
    let graph = crate::resolver::build_graph(file).map_err(|e| e.to_string())?;
    if let Err(errs) = crate::checker::check_graph(&graph) {
        // Surface the first type error (matches `chezzi check`'s headline).
        let first = errs
            .first()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "type error".into());
        return Err(first);
    }
    let program = crate::compiler::compile_graph(&graph).map_err(|e| e.message)?;
    let program: Arc<Program> = Arc::new(program);
    let file_label = file.display().to_string();

    crate::vm::on_vm_stack(move || invoke_all(program, file_label, parallel, max_heap, timeout_ms))
}

/// Run every `test fn` + suite in a compiled program on a fresh VM, returning per-test outcomes (or
/// an init-error message for the whole file). Engine-agnostic: `parallel` selects serial vs M:N via
/// [`Vm::set_parallel`] before the module top-levels run. Ownership (not `&Arc`) so the M:N variant
/// can move it onto its own stack thread.
fn invoke_all(
    program: Arc<Program>,
    file_label: String,
    parallel: bool,
    max_heap: usize,
    timeout_ms: u64,
) -> Result<Vec<Outcome>, String> {
    let mut vm = Vm::for_program(Arc::clone(&program));
    vm.set_parallel(parallel);
    // `--max-heap` cap (0 = off). Per-test reset of the over-memory latch is VM-side (in each invoke
    // entry point), so `run_suite` needs no cap threading — the VM is already configured.
    vm.set_max_heap(max_heap);
    // `--timeout` cap (0 = off). Like the heap cap it is VM config read at each invoke entry (which
    // arms a fresh deadline), so `run_suite` needs no threading — the VM is already configured.
    vm.set_timeout(timeout_ms);
    // Initialize the module(s): run top-levels once so globals/functions/structs exist.
    if let Err(e) = vm.init_for_tests() {
        return Err(format!(
            "error initializing test module: {} (line {})",
            e.message, e.span.line
        ));
    }

    let mut outcomes: Vec<Outcome> = Vec::new();

    // Free tests, in declaration order.
    for (name, proto) in program.tests.iter() {
        let verdict = match vm.invoke_test(*proto) {
            Ok(()) => Verdict::Pass,
            Err(e) => verdict_from_fault(e),
        };
        let _ = vm.take_out(); // discard the test's stdout (kept reusable)
        outcomes.push(Outcome {
            name: name.clone(),
            file: file_label.clone(),
            verdict,
        });
    }

    // Suites: construct once, run lifecycle hooks around each test method.
    for suite in program.suites.iter() {
        run_suite(&mut vm, suite, &file_label, &mut outcomes);
    }

    vm.reap_after_tests();
    Ok(outcomes)
}

/// Drive one suite: construct the instance once, then for each test method run
/// `before_each?` → method → `after_each?` (always, even on failure, like `defer`), with
/// `before_all?`/`after_all?` framing the whole suite.
fn run_suite(vm: &mut Vm, suite: &crate::vm::op::SuiteInfo, file: &str, out: &mut Vec<Outcome>) {
    let hook = |name: &str| suite.hooks.get(name).copied();

    // Construct the instance. A failure here fails every test in the suite (nothing can run). A
    // crashed constructor is setup failure → ERROR-class (the test never ran), whatever the fault.
    let instance = match vm.build_suite_instance(suite.new_thunk) {
        Ok(v) => v,
        Err(e) => {
            for (tname, _) in suite.tests.iter() {
                out.push(Outcome {
                    name: format!("{}::{}", suite.name, tname),
                    file: file.to_string(),
                    verdict: Verdict::Error {
                        line: e.span.line,
                        msg: format!("suite construction failed: {}", e.message),
                    },
                });
            }
            return;
        }
    };

    // before_all? — a failure fails the whole suite (no test method runs); after_all still runs.
    // A hook fault is setup failure → ERROR-class.
    if let Some(p) = hook("before_all")
        && let Err(e) = vm.invoke_suite_method(p, instance)
    {
        for (tname, _) in suite.tests.iter() {
            out.push(Outcome {
                name: format!("{}::{}", suite.name, tname),
                file: file.to_string(),
                verdict: Verdict::Error {
                    line: e.span.line,
                    msg: format!("before_all failed: {}", e.message),
                },
            });
        }
        if let Some(ap) = hook("after_all") {
            let _ = vm.invoke_suite_method(ap, instance);
        }
        let _ = vm.take_out();
        return;
    }

    for (tname, proto) in suite.tests.iter() {
        let mut verdict = Verdict::Pass;
        // before_each? — a failure is the test's failure (the method is skipped); after_each still
        // runs. A hook crash is setup failure → ERROR-class.
        if let Some(p) = hook("before_each")
            && let Err(e) = vm.invoke_suite_method(p, instance)
        {
            verdict = Verdict::Error {
                line: e.span.line,
                msg: format!("before_each failed: {}", e.message),
            };
        }
        // The test method itself (only if before_each passed). This is the ONE place an `assert`
        // fault reads as FAIL; any other fault in the body is ERROR (via `verdict_from_fault`).
        if matches!(verdict, Verdict::Pass)
            && let Err(e) = vm.invoke_suite_method(*proto, instance)
        {
            verdict = verdict_from_fault(e);
        }
        // after_each? — ALWAYS runs (even on failure, like `defer`), so the invocation must NOT be
        // short-circuited. It does not mask the original failure; only if the test passed but
        // after_each itself faults does that become the test's (ERROR-class) failure.
        if let Some(p) = hook("after_each") {
            let ae = vm.invoke_suite_method(p, instance);
            if matches!(verdict, Verdict::Pass)
                && let Err(e) = ae
            {
                verdict = Verdict::Error {
                    line: e.span.line,
                    msg: format!("after_each failed: {}", e.message),
                };
            }
        }
        let _ = vm.take_out();
        out.push(Outcome {
            name: format!("{}::{}", suite.name, tname),
            file: file.to_string(),
            verdict,
        });
    }

    // after_all? — runs after the last test method.
    if let Some(p) = hook("after_all") {
        let _ = vm.invoke_suite_method(p, instance);
        let _ = vm.take_out();
    }
}

/// Collect the `*_test.chz` files for a path: a single file (must end in `_test.chz`), or every
/// `*_test.chz` under a directory (recursive). Files are returned in a stable sorted order.
fn collect_test_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    if root.is_file() {
        if is_test_file(root) {
            return Ok(vec![root.to_path_buf()]);
        }
        return Err(format!("{} is not a *_test.chz file", root.display()));
    }
    if !root.is_dir() {
        return Err(format!("{} does not exist", root.display()));
    }
    let mut files = Vec::new();
    walk_dir(root, &mut files)?;
    files.sort();
    Ok(files)
}

/// Recursively gather `*_test.chz` files under `dir`.
fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if path.is_dir() {
            walk_dir(&path, out)?;
        } else if is_test_file(&path) {
            out.push(path);
        }
    }
    Ok(())
}

/// True if `path`'s file name ends in `_test.chz`.
fn is_test_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with("_test.chz"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir =
                std::env::temp_dir().join(format!("chezzi_test_{}_{}", std::process::id(), n));
            std::fs::create_dir_all(&dir).unwrap();
            TmpDir(dir)
        }
        fn write(&self, rel: &str, contents: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::write(&p, contents).unwrap();
            p
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn b4_happy_path_all_pass() {
        let d = TmpDir::new();
        let f = d.write(
            "math_test.chz",
            "test fn one():\n    assert 1 + 1 == 2\ntest fn two():\n    assert \"a\" + \"b\" == \"ab\"\n",
        );
        let report = run_tests(&f, false);
        assert!(
            report.passed,
            "all tests should pass; report:\n{}",
            report.text
        );
        assert!(report.text.contains("PASS one"), "report:\n{}", report.text);
        assert!(report.text.contains("PASS two"), "report:\n{}", report.text);
        assert!(
            report.text.contains("2 test(s): 2 passed, 0 failed"),
            "report:\n{}",
            report.text
        );
    }

    #[test]
    fn b5_failure_reports_file_and_line() {
        let d = TmpDir::new();
        // The failing `assert` is on line 3.
        let f = d.write(
            "fail_test.chz",
            "test fn boom():\n    x := 1\n    assert x == 2, \"x must be two\"\n",
        );
        let report = run_tests(&f, false);
        assert!(
            !report.passed,
            "the run must fail; report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("FAIL boom"),
            "report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("fail_test.chz:3"),
            "report must carry file:line; report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("x must be two"),
            "report:\n{}",
            report.text
        );
    }

    #[test]
    fn dir_walk_collects_test_files_only() {
        let d = TmpDir::new();
        d.write("a_test.chz", "test fn a():\n    assert true\n");
        d.write("b_test.chz", "test fn b():\n    assert true\n");
        d.write("not_a_test.chz", "print(\"ignored\")\n"); // no `_test.chz` suffix
        let report = run_tests(&d.0, false);
        assert!(report.passed, "report:\n{}", report.text);
        assert!(report.text.contains("PASS a"), "report:\n{}", report.text);
        assert!(report.text.contains("PASS b"), "report:\n{}", report.text);
        assert!(
            report.text.contains("2 test(s): 2 passed, 0 failed"),
            "report:\n{}",
            report.text
        );
    }

    #[test]
    fn compile_error_in_test_file_reports_once() {
        let d = TmpDir::new();
        // A type error (assert on a non-bool) fails the whole file before any test runs. It must be
        // reported ONCE as ERROR, not as a phantom `FAIL …:0`, and must not inflate the test count.
        let f = d.write("broken_test.chz", "test fn t():\n    assert 1\n");
        let report = run_tests(&f, false);
        assert!(!report.passed, "report:\n{}", report.text);
        assert!(report.text.contains("ERROR"), "report:\n{}", report.text);
        assert!(
            !report.text.contains(":0)"),
            "no phantom :0 line; report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("0 test(s)"),
            "report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("file error(s)"),
            "report:\n{}",
            report.text
        );
    }

    #[test]
    fn clean_test_file_with_zero_tests_fails() {
        // A `*_test.chz` that compiles cleanly but declares no `test fn` (e.g. the `test` keyword was
        // forgotten) must NOT pass with exit 0 — that would be indistinguishable from a green run.
        let d = TmpDir::new();
        let f = d.write(
            "empty_test.chz",
            "fn helper():\n    print(\"not a test\")\n",
        );
        let report = run_tests(&f, false);
        assert!(
            !report.passed,
            "zero discovered tests must fail; report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("no tests discovered"),
            "report:\n{}",
            report.text
        );
    }

    #[test]
    fn non_test_file_path_errors() {
        let d = TmpDir::new();
        let f = d.write("plain.chz", "print(\"hi\")\n");
        let report = run_tests(&f, false);
        assert!(!report.passed);
        assert!(
            report.text.contains("not a *_test.chz file"),
            "report:\n{}",
            report.text
        );
    }

    #[test]
    fn c3_suite_lifecycle_order_and_shared_fixture() {
        let d = TmpDir::new();
        // A suite with a shared `log` fixture (a list) mutated by hooks + tests. Each test asserts
        // the hook order it should observe, and after_each runs even when a test fails.
        let src = "\
struct Suite:
    log: List[str] = []
    fn before_all(self):
        self.log.push(\"before_all\")
    fn after_all(self):
        self.log.push(\"after_all\")
    fn before_each(self):
        self.log.push(\"before_each\")
    fn after_each(self):
        self.log.push(\"after_each\")

    test fn first(self):
        # before_all once, then before_each for this test
        assert self.log == [\"before_all\", \"before_each\"], \"first ordering\"

    test fn second(self):
        # first ran: before_all, before_each, (test), after_each, then before_each again
        assert self.log == [\"before_all\", \"before_each\", \"after_each\", \"before_each\"], \"second ordering\"
";
        let f = d.write("suite_test.chz", src);
        let report = run_tests(&f, false);
        assert!(
            report.passed,
            "suite ordering should hold; report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("PASS Suite::first"),
            "report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("PASS Suite::second"),
            "report:\n{}",
            report.text
        );
    }

    /// Collect the per-test verdicts of a report as `(name, passed?)` pairs, in report order — the
    /// engine-independent signal the dual-engine gate compares. Parses the rendered lines (the
    /// `Outcome` list isn't public) — `PASS <name> (...)` / `FAIL <name> (...) ...`.
    fn verdicts(text: &str) -> Vec<(String, &'static str)> {
        text.lines()
            .filter_map(|l| {
                let (tag, rest) = if let Some(r) = l.strip_prefix("PASS ") {
                    ("PASS", r)
                } else if let Some(r) = l.strip_prefix("FAIL ") {
                    ("FAIL", r)
                } else if let Some(r) = l.strip_prefix("OVER-MEMORY ") {
                    // A `--max-heap` trip must participate too: a test that trips on one engine but
                    // not the other is a parity bug the gate has to catch, not silently drop.
                    ("OVER-MEMORY", r)
                } else if let Some(r) = l.strip_prefix("TIMED-OUT ") {
                    ("TIMED-OUT", r)
                } else {
                    // ERROR must participate too: a test that ERRORs on one engine but FAILs/PASSes
                    // on the other is a parity bug the gate has to catch, not silently drop.
                    ("ERROR", l.strip_prefix("ERROR ")?)
                };
                let name = rest.split(" (").next().unwrap_or(rest).to_string();
                Some((name, tag))
            })
            .collect()
    }

    #[test]
    fn chz_suite_passes_both_engines() {
        // The dedicated native suite (`tests/chz/`) is the dogfood guard AND the serial==M:N parity
        // gate for ported behavioral tests: `chezzi test` itself runs a single engine, so the parity
        // dimension these tests carried in `vm/parity_tests.rs` is preserved HERE by running the whole
        // suite on BOTH the cooperative serial VM and the M:N OS-thread VM and asserting identical
        // per-test verdicts. A test that passes serial but fails M:N (or vice versa) is a parity bug
        // caught by `cargo test`, not just by `chezzi test`.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/chz");

        let serial = run_tests(&root, false);
        assert!(
            serial.passed,
            "tests/chz must pass on the serial VM; report:\n{}",
            serial.text
        );

        let parallel = run_tests(&root, true);
        assert!(
            parallel.passed,
            "tests/chz must pass on the M:N VM; report:\n{}",
            parallel.text
        );

        // Same tests, same verdicts, same order — the parity assertion.
        assert_eq!(
            verdicts(&serial.text),
            verdicts(&parallel.text),
            "serial vs M:N verdict mismatch — a parity bug.\nserial:\n{}\nM:N:\n{}",
            serial.text,
            parallel.text
        );
    }

    #[test]
    fn error_bucket_for_non_assert_fault() {
        // A test whose body faults on something OTHER than `assert` (here an out-of-bounds index) is
        // an unexpected fault → the ERROR bucket, not FAIL. Indexing is dynamic, so it faults at
        // runtime rather than being caught by the checker.
        let d = TmpDir::new();
        let f = d.write(
            "boom_test.chz",
            "test fn boom():\n    xs := [1]\n    y := xs[5]\n    print(y)\n",
        );
        let report = run_tests(&f, false);
        assert!(
            !report.passed,
            "an errored test fails the run; report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("ERROR boom"),
            "non-assert fault must render ERROR; report:\n{}",
            report.text
        );
        assert!(
            !report.text.contains("FAIL boom"),
            "must NOT be a FAIL; report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("1 errored"),
            "summary must count the ERROR bucket; report:\n{}",
            report.text
        );
    }

    #[test]
    fn fail_bucket_for_assert_false() {
        // A plain `assert false` is the intended failure signal → FAIL, never ERROR.
        let d = TmpDir::new();
        let f = d.write(
            "af_test.chz",
            "test fn boom():\n    assert false, \"nope\"\n",
        );
        let report = run_tests(&f, false);
        assert!(!report.passed, "report:\n{}", report.text);
        assert!(
            report.text.contains("FAIL boom"),
            "report:\n{}",
            report.text
        );
        assert!(
            !report.text.contains("ERROR boom"),
            "report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("1 failed, 0 errored"),
            "report:\n{}",
            report.text
        );
    }

    #[test]
    fn passing_test_is_pass_and_report_passed() {
        let d = TmpDir::new();
        let f = d.write(
            "ok_test.chz",
            "test fn one():\n    assert true\ntest fn two():\n    assert true\n",
        );
        let report = run_tests(&f, false);
        assert!(report.passed, "report:\n{}", report.text);
        assert!(
            report
                .text
                .contains("2 test(s): 2 passed, 0 failed, 0 errored"),
            "report:\n{}",
            report.text
        );
    }

    #[test]
    fn hook_fault_is_error_not_fail() {
        // A fault in a setup hook (here `before_each`) means the fixture crashed — that is ERROR-class
        // (the test itself never got to run its assertions), regardless of whether the hook used
        // `assert` or faulted some other way.
        let d = TmpDir::new();
        let src = "\
struct Suite:
    fn before_each(self):
        xs := [1]
        y := xs[5]
        print(y)

    test fn t(self):
        assert true
";
        let f = d.write("hook_test.chz", src);
        let report = run_tests(&f, false);
        assert!(!report.passed, "report:\n{}", report.text);
        assert!(
            report.text.contains("ERROR Suite::t"),
            "hook fault is ERROR-class; report:\n{}",
            report.text
        );
        assert!(
            !report.text.contains("FAIL Suite::t"),
            "report:\n{}",
            report.text
        );
    }

    #[test]
    fn over_memory_bucket_for_runaway_alloc() {
        // A test that grows an unbounded list under a LOW cap must land in the OverMemory bucket on
        // BOTH engines (serial == M:N) — the trip is deterministic-in-VM, not OS RSS.
        let d = TmpDir::new();
        // Push a fresh heap list each iteration: the cap is checked only at GC boundaries, and GC
        // fires on `Obj`-count growth — a loop pushing inline ints (no `Obj` alloc) would never
        // trigger a sweep and so never trip the cap (the documented GC-granularity limit).
        let f = d.write(
            "boom_test.chz",
            "test fn boom():\n    xs := []\n    for i in range(1000000):\n        xs.push([i])\n",
        );
        for parallel in [false, true] {
            let report = run_tests_capped(&f, parallel, 1_000_000);
            assert!(
                !report.passed,
                "over-memory must fail the run (parallel={parallel}); report:\n{}",
                report.text
            );
            assert!(
                report.text.contains("OVER-MEMORY boom"),
                "runaway alloc must render OVER-MEMORY (parallel={parallel}); report:\n{}",
                report.text
            );
            assert!(
                report.text.contains("over-memory"),
                "summary must count the bucket (parallel={parallel}); report:\n{}",
                report.text
            );
            assert!(
                !report.text.contains("FAIL boom") && !report.text.contains("ERROR boom"),
                "must be OVER-MEMORY, not FAIL/ERROR (parallel={parallel}); report:\n{}",
                report.text
            );
        }
    }

    // ---- `--timeout` wall-clock cap (M:N-engine-only; tests run parallel=true) ----
    // Robust to CI timing: a CLEARLY-infinite loop under a SHORT timeout, or a CLEARLY-fast test
    // under a GENEROUS timeout — never near-boundary.

    #[test]
    fn timed_out_bucket_for_infinite_loop() {
        // THE regression test for the prior hang bug: a top-level `while true: pass` runs OUTSIDE the
        // fiber scheduler (`invoke_test → run_proto → run_until`), so the reds checkpoint never gates
        // it — only the loop back-edge does. Under a 50ms cap it MUST terminate and bucket TimedOut.
        let d = TmpDir::new();
        let f = d.write(
            "spin_test.chz",
            "test fn spin():\n    while true:\n        pass\n",
        );
        let report = run_tests_timed(&f, true, 0, 50);
        assert!(
            !report.passed,
            "an infinite loop must fail the run; report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("TIMED-OUT spin"),
            "infinite loop must render TIMED-OUT; report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("timed out"),
            "summary must count the bucket; report:\n{}",
            report.text
        );
    }

    #[test]
    fn timeout_control_passes_under_generous_timeout() {
        // A trivially-fast test under a 60s cap passes normally — the throttled back-edge check never
        // false-trips a fast test.
        let d = TmpDir::new();
        let f = d.write("fast_test.chz", "test fn quick():\n    assert 1 + 1 == 2\n");
        let report = run_tests_timed(&f, true, 0, 60_000);
        assert!(report.passed, "report:\n{}", report.text);
        assert!(
            report.text.contains("PASS quick"),
            "report:\n{}",
            report.text
        );
        assert!(
            !report.text.contains("timed out"),
            "a fast test must not surface the timeout clause; report:\n{}",
            report.text
        );
    }

    #[test]
    fn recover_does_not_catch_timeout() {
        // The wall-clock abort unwinds PAST `recover:`: `r := recover: <infinite loop>` cannot swallow
        // it, so control never reaches the trailing `assert false` and the test lands TimedOut.
        let d = TmpDir::new();
        let f = d.write(
            "rectimeout_test.chz",
            "test fn t():\n    r := recover:\n        while true:\n            pass\n    assert false\n",
        );
        let report = run_tests_timed(&f, true, 0, 50);
        assert!(!report.passed, "report:\n{}", report.text);
        assert!(
            report.text.contains("TIMED-OUT t"),
            "recover: must NOT catch a timeout abort; report:\n{}",
            report.text
        );
    }

    #[test]
    fn timed_out_across_spawn() {
        // A hang inside a `spawn`ed task must bucket TimedOut on the M:N engine: the worker runs on its
        // own VM, so the absolute deadline must be threaded onto it, and its loop's back-edge trips the
        // cap. The `is_timed_out` marker crosses the worker→parent fault boundary.
        let d = TmpDir::new();
        let f = d.write(
            "spawnspin_test.chz",
            "fn spin() -> int:\n    while true:\n        pass\n    return 0\ntest fn t():\n    parallel:\n        spawn spin()\n",
        );
        let report = run_tests_timed(&f, true, 0, 50);
        assert!(!report.passed, "report:\n{}", report.text);
        assert!(
            report.text.contains("TIMED-OUT t"),
            "a spawned task's hang must bucket TIMED-OUT; report:\n{}",
            report.text
        );
        // The worker must inherit the parent's `timeout_ms`, not the `Vm::new` default 0 — the abort
        // message reads the raw cap, so a spawned-task timeout must render the real "(50ms)", not "(0ms)".
        assert!(
            report.text.contains("(50ms)") && !report.text.contains("(0ms)"),
            "spawned-task timeout message must show the real cap, not 0ms; report:\n{}",
            report.text
        );
    }

    #[test]
    fn over_memory_control_passes_under_generous_cap() {
        // A small alloc under a generous cap passes normally — the cap only trips on runaway growth.
        let d = TmpDir::new();
        let f = d.write(
            "ctl_test.chz",
            "test fn small():\n    xs := []\n    for i in range(100):\n        xs.push(i)\n    assert xs.len() == 100\n",
        );
        let report = run_tests_capped(&f, false, 100_000_000);
        assert!(report.passed, "report:\n{}", report.text);
        assert!(
            report.text.contains("PASS small"),
            "report:\n{}",
            report.text
        );
    }

    #[test]
    fn recover_does_not_catch_over_memory() {
        // The hard-abort unwinds PAST `recover:` — a `r := recover: <runaway>` cannot swallow it, so
        // the test still lands OverMemory (control never reaches the trailing assert).
        let d = TmpDir::new();
        let f = d.write(
            "rec_test.chz",
            "fn boom() -> int:\n    xs := []\n    for i in range(1000000):\n        xs.push([i])\n    return 0\ntest fn t():\n    r := recover: boom()\n    assert true\n",
        );
        let report = run_tests_capped(&f, false, 1_000_000);
        assert!(!report.passed, "report:\n{}", report.text);
        assert!(
            report.text.contains("OVER-MEMORY t"),
            "recover: must NOT catch the over-memory abort; report:\n{}",
            report.text
        );
    }

    #[test]
    fn recover_does_not_catch_over_memory_via_native_reentry() {
        // The hard-abort must bypass `recover:` even when the runaway alloc trips inside a NATIVE
        // RE-ENTRY (a HOF callback) — a nested `run_until` whose `Err` bubbles to the outer loop. The
        // outer Err funnel must recognise the over-memory marker and keep bypassing `recover:`, or the
        // guard is defeated by `r := recover: <list>.map(<runaway closure>)` and the test wrongly PASSES.
        let d = TmpDir::new();
        let f = d.write(
            "recnat_test.chz",
            "fn grow(x: int) -> int:\n    ys := []\n    for j in range(1000000):\n        ys.push([j])\n    return 0\ntest fn t():\n    r := recover: [1].map(grow)\n    assert true\n",
        );
        for parallel in [false, true] {
            let report = run_tests_capped(&f, parallel, 1_000_000);
            assert!(
                !report.passed,
                "(parallel={parallel}) report:\n{}",
                report.text
            );
            assert!(
                report.text.contains("OVER-MEMORY t"),
                "recover: must NOT catch an over-memory abort raised inside a HOF callback (parallel={parallel}); report:\n{}",
                report.text
            );
        }
    }

    #[test]
    fn over_memory_buckets_across_spawn_on_both_engines() {
        // A runaway alloc inside a `spawn`ed task must bucket OverMemory on BOTH engines. On M:N the
        // task runs on a worker VM with its own heap, so the cap must be threaded onto the worker and
        // the over-memory marker must cross the worker→parent fault boundary — else serial buckets
        // OverMemory while M:N passes (a parity divergence).
        let d = TmpDir::new();
        let f = d.write(
            "spawnmem_test.chz",
            "fn runaway() -> int:\n    ys := []\n    for j in range(1000000):\n        ys.push([j])\n    return 0\ntest fn t():\n    parallel:\n        spawn runaway()\n",
        );
        for parallel in [false, true] {
            let report = run_tests_capped(&f, parallel, 1_000_000);
            assert!(
                !report.passed,
                "(parallel={parallel}) report:\n{}",
                report.text
            );
            assert!(
                report.text.contains("OVER-MEMORY t"),
                "a spawned task's runaway alloc must bucket OVER-MEMORY (parallel={parallel}); report:\n{}",
                report.text
            );
        }
    }

    #[test]
    fn over_memory_concurrent_under_cap_passes_on_both_engines() {
        // The cap's guaranteed envelope on the low side: a CONCURRENT test whose tasks each stay well
        // under a generous cap must PASS on both engines — no false trip. (The near-boundary aggregate
        // case, where per-fiber allocation sums over the cap, is the documented per-heap divergence and
        // is deliberately NOT asserted here — see `docs/future.md §3b`.)
        let d = TmpDir::new();
        let f = d.write(
            "concpass_test.chz",
            "fn work() -> int:\n\
            \x20   ys := []\n\
            \x20   for j in range(2000):\n\
            \x20       ys.push([j])\n\
            \x20   return ys.len()\n\
             test fn t():\n\
            \x20   parallel:\n\
            \x20       spawn work()\n\
            \x20       spawn work()\n",
        );
        for parallel in [false, true] {
            let report = run_tests_capped(&f, parallel, 100_000_000);
            assert!(
                report.passed,
                "a concurrent test well under a generous cap must pass (parallel={parallel}); report:\n{}",
                report.text
            );
            assert!(
                report.text.contains("PASS t"),
                "(parallel={parallel}) report:\n{}",
                report.text
            );
        }
    }

    #[test]
    fn over_memory_defer_is_still_capped_during_unwind() {
        // A `defer` that itself allocates runaway must ALSO be hard-aborted — the cap stays armed
        // through the abort's own cleanup unwind. Regression: an over-broad latch disabled the guard
        // for the whole unwind, so a runaway defer ran completely UNCAPPED (could OOM the process the
        // guard exists to protect). Observable proof: the defer's post-alloc statement (a push into a
        // module-global list) must NOT execute — the defer is cut short at its first GC boundary while
        // the tripped test's data is still rooted (heap still over cap). A follow-up test reads the
        // sentinel: len stays 1 iff the defer was bounded.
        let d = TmpDir::new();
        let f = d.write(
            "defermem_test.chz",
            "sentinel := [0]\n\
             fn leak():\n\
            \x20   defer:\n\
            \x20       junk := []\n\
            \x20       for j in range(200000):\n\
            \x20           junk.push([j])\n\
            \x20       sentinel.push(1)\n\
            \x20   xs := []\n\
            \x20   for i in range(1000000):\n\
            \x20       xs.push([i])\n\
             test fn trip():\n\
            \x20   leak()\n\
             test fn defer_was_bounded():\n\
            \x20   assert sentinel.len() == 1\n",
        );
        for parallel in [false, true] {
            let report = run_tests_capped(&f, parallel, 1_000_000);
            assert!(
                report.text.contains("OVER-MEMORY trip"),
                "the tripping test must bucket OVER-MEMORY (parallel={parallel}); report:\n{}",
                report.text
            );
            assert!(
                report.text.contains("PASS defer_was_bounded"),
                "the runaway defer must be cut short by the still-armed cap, so its post-alloc push \
                 never runs (parallel={parallel}); report:\n{}",
                report.text
            );
        }
    }

    #[test]
    fn c3_after_each_runs_even_on_failure() {
        let d = TmpDir::new();
        // `first` fails; `second` then asserts that after_each STILL ran for `first` (the fixture log
        // proves it), i.e. after_each fires even when the test method faulted.
        let src = "\
struct Suite:
    log: List[str] = []
    fn after_each(self):
        self.log.push(\"ae\")

    test fn first(self):
        assert false, \"deliberate\"

    test fn second(self):
        # after_each ran once for `first` despite its failure.
        assert self.log == [\"ae\"], \"after_each must run on failure\"
";
        let f = d.write("ae_test.chz", src);
        let report = run_tests(&f, false);
        assert!(!report.passed, "first must fail; report:\n{}", report.text);
        assert!(
            report.text.contains("FAIL Suite::first"),
            "report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("PASS Suite::second"),
            "report:\n{}",
            report.text
        );
    }
}
