//! `chezzi test` — the Rust-side test runner (M20).
//!
//! Discovers `*_test.chz` files, compiles each as its own entry graph, and invokes every `test fn`
//! (free tests + suite methods) on a reusable VM. The runner is Rust-side by necessity: a Chezzi
//! `recover:` only hands back the fault *message*, not its `span`, so only Rust catching the
//! `RuntimeError` directly gets the `.span` (hence `file:line`) the headline feature needs.
//!
//! Only the `assert` primitive is dual-engine (parity discipline); this orchestration is VM-only —
//! its output is Rust-formatted `PASS/FAIL`, not Chezzi program stdout, so no golden parity applies.

use crate::vm::op::Program;
use crate::vm::Vm;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The outcome of a `chezzi test` run: the rendered report and whether everything passed.
pub struct TestReport {
    pub text: String,
    pub passed: bool,
}

/// One test's result (for the report + summary counts).
struct Outcome {
    /// The test's name (`fn_name` for a free test, `Suite::method` for a suite test).
    name: String,
    /// The `*_test.chz` file the test came from (the `file` half of `file:line`).
    file: String,
    /// `None` ⇒ pass; `Some((line, message))` ⇒ fail at that source line.
    failure: Option<(usize, String)>,
}

/// Run every `test fn` discovered under `root` (a single `*_test.chz` file or a directory walked
/// recursively). Returns the rendered report + overall pass/fail. Never panics on a test fault — the
/// VM stays reusable, so one failing test does not abort the rest.
pub fn run_tests(root: &Path) -> TestReport {
    let files = match collect_test_files(root) {
        Ok(f) => f,
        Err(e) => {
            return TestReport { text: format!("chezzi test: {e}\n"), passed: false };
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
        match run_file(file) {
            Ok(mut file_outcomes) => outcomes.append(&mut file_outcomes),
            Err(msg) => {
                report.push_str(&format!("ERROR {}\n  {msg}\n", file.display()));
                file_errors += 1;
            }
        }
    }

    // Per-test lines, then a summary.
    for o in &outcomes {
        match &o.failure {
            None => report.push_str(&format!("PASS {} ({})\n", o.name, o.file)),
            Some((line, msg)) => {
                report.push_str(&format!("FAIL {} ({}:{}) {}\n", o.name, o.file, line, msg))
            }
        }
    }
    let total = outcomes.len();
    let failed = outcomes.iter().filter(|o| o.failure.is_some()).count();
    let passed_count = total - failed;
    report.push_str(&format!("\n{total} test(s): {passed_count} passed, {failed} failed"));
    if file_errors > 0 {
        report.push_str(&format!(", {file_errors} file error(s)"));
    }
    report.push('\n');

    TestReport { text: report, passed: failed == 0 && file_errors == 0 }
}

/// Compile + run one `*_test.chz` file, returning a per-test outcome list (or a compile-error
/// message for the whole file).
fn run_file(file: &Path) -> Result<Vec<Outcome>, String> {
    let graph = crate::resolver::build_graph(file).map_err(|e| e.to_string())?;
    if let Err(errs) = crate::checker::check_graph(&graph) {
        // Surface the first type error (matches `chezzi check`'s headline).
        let first = errs.first().map(|e| e.message.clone()).unwrap_or_else(|| "type error".into());
        return Err(first);
    }
    let program = crate::compiler::compile_graph(&graph).map_err(|e| e.message)?;
    let program: Arc<Program> = Arc::new(program);

    let mut vm = Vm::for_program(Arc::clone(&program));
    // Initialize the module(s): run top-levels once so globals/functions/structs exist.
    if let Err(e) = vm.init_for_tests() {
        return Err(format!("error initializing test module: {} (line {})", e.message, e.span.line));
    }

    let file_label = file.display().to_string();
    let mut outcomes: Vec<Outcome> = Vec::new();

    // Free tests, in declaration order.
    for (name, proto) in program.tests.iter() {
        let failure = vm.invoke_test(*proto).err().map(|e| (e.span.line, e.message));
        let _ = vm.take_out(); // discard the test's stdout (kept reusable)
        outcomes.push(Outcome {
            name: name.clone(),
            file: file_label.clone(),
            failure,
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

    // Construct the instance. A failure here fails every test in the suite (nothing can run).
    let instance = match vm.build_suite_instance(suite.new_thunk) {
        Ok(v) => v,
        Err(e) => {
            for (tname, _) in suite.tests.iter() {
                out.push(Outcome {
                    name: format!("{}::{}", suite.name, tname),
                    file: file.to_string(),
                    failure: Some((e.span.line, format!("suite construction failed: {}", e.message))),
                });
            }
            return;
        }
    };

    // before_all? — a failure fails the whole suite (no test method runs); after_all still runs.
    if let Some(p) = hook("before_all")
        && let Err(e) = vm.invoke_suite_method(p, instance)
    {
        for (tname, _) in suite.tests.iter() {
            out.push(Outcome {
                name: format!("{}::{}", suite.name, tname),
                file: file.to_string(),
                failure: Some((e.span.line, format!("before_all failed: {}", e.message))),
            });
        }
        if let Some(ap) = hook("after_all") {
            let _ = vm.invoke_suite_method(ap, instance);
        }
        let _ = vm.take_out();
        return;
    }

    for (tname, proto) in suite.tests.iter() {
        // before_each? — a failure is the test's failure (the method is skipped); after_each still runs.
        let mut failure: Option<(usize, String)> = None;
        if let Some(p) = hook("before_each")
            && let Err(e) = vm.invoke_suite_method(p, instance)
        {
            failure = Some((e.span.line, format!("before_each failed: {}", e.message)));
        }
        // The test method itself (only if before_each passed).
        if failure.is_none()
            && let Err(e) = vm.invoke_suite_method(*proto, instance)
        {
            failure = Some((e.span.line, e.message));
        }
        // after_each? — ALWAYS runs (even on failure, like `defer`), so the invocation must NOT be
        // short-circuited by `failure`. It does not mask the original failure; only if the test
        // passed but after_each itself faults does that become the test's failure.
        if let Some(p) = hook("after_each") {
            let ae = vm.invoke_suite_method(p, instance);
            if failure.is_none()
                && let Err(e) = ae
            {
                failure = Some((e.span.line, format!("after_each failed: {}", e.message)));
            }
        }
        let _ = vm.take_out();
        out.push(Outcome {
            name: format!("{}::{}", suite.name, tname),
            file: file.to_string(),
            failure,
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
    let entries = std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
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
            let dir = std::env::temp_dir().join(format!("chezzi_test_{}_{}", std::process::id(), n));
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
        let report = run_tests(&f);
        assert!(report.passed, "all tests should pass; report:\n{}", report.text);
        assert!(report.text.contains("PASS one"), "report:\n{}", report.text);
        assert!(report.text.contains("PASS two"), "report:\n{}", report.text);
        assert!(report.text.contains("2 test(s): 2 passed, 0 failed"), "report:\n{}", report.text);
    }

    #[test]
    fn b5_failure_reports_file_and_line() {
        let d = TmpDir::new();
        // The failing `assert` is on line 3.
        let f = d.write(
            "fail_test.chz",
            "test fn boom():\n    x := 1\n    assert x == 2, \"x must be two\"\n",
        );
        let report = run_tests(&f);
        assert!(!report.passed, "the run must fail; report:\n{}", report.text);
        assert!(report.text.contains("FAIL boom"), "report:\n{}", report.text);
        assert!(
            report.text.contains("fail_test.chz:3"),
            "report must carry file:line; report:\n{}",
            report.text
        );
        assert!(report.text.contains("x must be two"), "report:\n{}", report.text);
    }

    #[test]
    fn dir_walk_collects_test_files_only() {
        let d = TmpDir::new();
        d.write("a_test.chz", "test fn a():\n    assert true\n");
        d.write("b_test.chz", "test fn b():\n    assert true\n");
        d.write("not_a_test.chz", "print(\"ignored\")\n"); // no `_test.chz` suffix
        let report = run_tests(&d.0);
        assert!(report.passed, "report:\n{}", report.text);
        assert!(report.text.contains("PASS a"), "report:\n{}", report.text);
        assert!(report.text.contains("PASS b"), "report:\n{}", report.text);
        assert!(report.text.contains("2 test(s): 2 passed, 0 failed"), "report:\n{}", report.text);
    }

    #[test]
    fn compile_error_in_test_file_reports_once() {
        let d = TmpDir::new();
        // A type error (assert on a non-bool) fails the whole file before any test runs. It must be
        // reported ONCE as ERROR, not as a phantom `FAIL …:0`, and must not inflate the test count.
        let f = d.write("broken_test.chz", "test fn t():\n    assert 1\n");
        let report = run_tests(&f);
        assert!(!report.passed, "report:\n{}", report.text);
        assert!(report.text.contains("ERROR"), "report:\n{}", report.text);
        assert!(!report.text.contains(":0)"), "no phantom :0 line; report:\n{}", report.text);
        assert!(report.text.contains("0 test(s)"), "report:\n{}", report.text);
        assert!(report.text.contains("file error(s)"), "report:\n{}", report.text);
    }

    #[test]
    fn non_test_file_path_errors() {
        let d = TmpDir::new();
        let f = d.write("plain.chz", "print(\"hi\")\n");
        let report = run_tests(&f);
        assert!(!report.passed);
        assert!(report.text.contains("not a *_test.chz file"), "report:\n{}", report.text);
    }

    #[test]
    fn c3_suite_lifecycle_order_and_shared_fixture() {
        let d = TmpDir::new();
        // A suite with a shared `log` fixture (a list) mutated by hooks + tests. Each test asserts
        // the hook order it should observe, and after_each runs even when a test fails.
        let src = "\
struct Suite:
    log: list[str] = []
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
        let report = run_tests(&f);
        assert!(report.passed, "suite ordering should hold; report:\n{}", report.text);
        assert!(report.text.contains("PASS Suite::first"), "report:\n{}", report.text);
        assert!(report.text.contains("PASS Suite::second"), "report:\n{}", report.text);
    }

    #[test]
    fn d1_dogfood_example_tests_pass() {
        // The committed `examples/*_test.chz` files (membership/operators/match_or + the suite) must
        // all pass under the runner — the dogfood guard.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        for name in ["membership_test.chz", "operators_test.chz", "match_or_test.chz", "suite_test.chz"] {
            let f = root.join(name);
            let report = run_tests(&f);
            assert!(report.passed, "{name} should pass; report:\n{}", report.text);
        }
    }

    #[test]
    fn c3_after_each_runs_even_on_failure() {
        let d = TmpDir::new();
        // `first` fails; `second` then asserts that after_each STILL ran for `first` (the fixture log
        // proves it), i.e. after_each fires even when the test method faulted.
        let src = "\
struct Suite:
    log: list[str] = []
    fn after_each(self):
        self.log.push(\"ae\")

    test fn first(self):
        assert false, \"deliberate\"

    test fn second(self):
        # after_each ran once for `first` despite its failure.
        assert self.log == [\"ae\"], \"after_each must run on failure\"
";
        let f = d.write("ae_test.chz", src);
        let report = run_tests(&f);
        assert!(!report.passed, "first must fail; report:\n{}", report.text);
        assert!(report.text.contains("FAIL Suite::first"), "report:\n{}", report.text);
        assert!(report.text.contains("PASS Suite::second"), "report:\n{}", report.text);
    }
}
