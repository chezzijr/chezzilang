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
use std::time::{Duration, Instant};

/// The outcome of a `chezzi test` run: the rendered report and whether everything passed.
pub struct TestReport {
    pub text: String,
    pub passed: bool,
}

/// Report verbosity for the CLI ergonomics wave. `Normal` is the DEFAULT and its output is
/// byte-identical to the pre-wave runner (the load-bearing invariant the dual-engine gate compares).
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Verbosity {
    /// One `PASS/FAIL/ERROR name (file[:line])` line per test, then the summary (today's output).
    #[default]
    Normal,
    /// One char per test (`.`/`F`/`E`/`M`/`T`) then the summary line — no per-test lines.
    Quiet,
    /// `Normal` plus per-test wall-clock timing (`… (Nms)`) and a total-duration summary line.
    Verbose,
}

/// Opt-in knobs for a `chezzi test` run. `Default` = every feature OFF, `Verbosity::Normal`,
/// `color` off — the exact pre-wave behavior, so the render path a default run takes is unchanged
/// and the `chz_suite_passes_both_engines` byte-identity gate stays green. Each new flag is gated on
/// its field being non-default (mirroring the off-by-default `--max-heap`/`--timeout` summary clauses).
#[derive(Clone, Default)]
pub struct RunOpts {
    /// `--max-heap` per-test live-heap cap in bytes (`0` = OFF).
    pub max_heap: usize,
    /// `--timeout` per-test wall-clock cap in ms (`0` = OFF).
    pub timeout_ms: u64,
    /// `-k`/`--filter` substring: run only tests whose displayed name contains it (`None` = all).
    pub filter: Option<String>,
    /// `--fail-fast`: stop after the first non-pass verdict (in deterministic declaration order).
    pub fail_fast: bool,
    /// `--show-output`: surface a failing test's captured stdout, indented under its line.
    pub show_output: bool,
    /// `--errors=json`: emit ONLY a JSON document (per-test results + totals), no human lines.
    pub json: bool,
    /// `-q`/`-v` verbosity.
    pub verbosity: Verbosity,
    /// Colorize the verdict tag (resolved to a bool in `cmd_test` via `IsTerminal`; the runner never
    /// probes the tty itself so a captured (non-tty) test harness never sees ANSI unless forced).
    pub color: bool,
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
    /// Wall-clock time the invoke took (always measured — negligible; surfaced only under `-v`/json).
    duration: Duration,
    /// The test's captured stdout — kept ONLY when `--show-output` is on (else empty, discarded).
    captured_out: String,
}

/// The JSON status token for a verdict (mirrors `--errors=json` on `check`/`run` for CI consumers).
fn verdict_status(v: &Verdict) -> &'static str {
    match v {
        Verdict::Pass => "pass",
        Verdict::Fail { .. } => "fail",
        Verdict::Error { .. } => "error",
        Verdict::OverMemory { .. } => "over_memory",
        Verdict::TimedOut { .. } => "timed_out",
    }
}

/// The source line for a verdict, if it has a meaningful one (Fail/Error only).
fn verdict_line(v: &Verdict) -> Option<usize> {
    match v {
        Verdict::Fail { line, .. } | Verdict::Error { line, .. } => Some(*line),
        _ => None,
    }
}

/// Wrap `s` in an ANSI color when `color` is on, else return it untouched. `code`: 32 green, 31 red,
/// 33 yellow. Only ever called with `color == true` from a resolved-tty CLI, so a captured test
/// harness (color defaults false) never emits an escape.
fn paint(s: &str, code: u8, color: bool) -> String {
    if color {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Encode a string as a JSON string literal (minimal, zero-dep). Dup of `main.rs::json_string` — that
/// one lives in the bin crate, unreachable from this lib module; the escaper is ~12 lines, not worth a
/// shared crate seam.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
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
    run_tests_opts(root, parallel, RunOpts::default())
}

/// Like [`run_tests`], plus the opt-in `--max-heap` per-test cap (`max_heap`: byte count, `0` = OFF).
pub fn run_tests_capped(root: &Path, parallel: bool, max_heap: usize) -> TestReport {
    run_tests_opts(
        root,
        parallel,
        RunOpts {
            max_heap,
            ..Default::default()
        },
    )
}

/// Like [`run_tests_capped`], plus the opt-in `--timeout` per-test wall-clock cap (`timeout_ms`: ms,
/// `0` = OFF).
pub fn run_tests_timed(
    root: &Path,
    parallel: bool,
    max_heap: usize,
    timeout_ms: u64,
) -> TestReport {
    run_tests_opts(
        root,
        parallel,
        RunOpts {
            max_heap,
            timeout_ms,
            ..Default::default()
        },
    )
}

/// The core runner. `parallel` selects the engine (serial oracle vs M:N); `opts` carries every opt-in
/// ergonomics knob. **`RunOpts::default()` reproduces the pre-wave output byte-for-byte** — every new
/// clause is gated on its field being non-default, so the render path a no-flag run takes is unchanged
/// and the `chz_suite_passes_both_engines` byte-identity gate stays green.
///
/// **Determinism / ordering:** files run in sorted path order; within a file, free tests run in
/// declaration order, then each suite's methods in declaration order. `--fail-fast` stops at the first
/// non-pass in exactly that order (later tests simply don't run).
pub fn run_tests_opts(root: &Path, parallel: bool, opts: RunOpts) -> TestReport {
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
    // A compile/type/resolve error fails a whole file before any test runs. Collected (not rendered
    // inline) so the JSON path can suppress the human `ERROR <file>` lines.
    let mut file_error_msgs: Vec<(String, String)> = Vec::new();
    // Tests skipped by `--filter` (threaded up from the invoke sites — discovery of names happens
    // inside the compiled program, not before).
    let mut filtered_out = 0usize;

    for file in &files {
        match run_file(file, parallel, &opts) {
            Ok((mut file_outcomes, skipped)) => {
                filtered_out += skipped;
                let hit_non_pass = file_outcomes
                    .iter()
                    .any(|o| !matches!(o.verdict, Verdict::Pass));
                outcomes.append(&mut file_outcomes);
                // `--fail-fast`: after a file that produced a non-pass, don't start the next file.
                if opts.fail_fast && hit_non_pass {
                    break;
                }
            }
            Err(msg) => {
                file_error_msgs.push((file.display().to_string(), msg));
                if opts.fail_fast {
                    break;
                }
            }
        }
    }

    let file_errors = file_error_msgs.len();
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
    // A filter that matches nothing is NOT a silent "0 tests" — call it out and fail deterministically
    // (mirrors the zero-discovered rule). Only when the filter is what emptied the run.
    let filter_no_match = opts.filter.is_some() && total == 0 && file_errors == 0;
    let no_tests_discovered = opts.filter.is_none() && total == 0 && file_errors == 0;
    let passed = failed == 0
        && errored == 0
        && over_memory == 0
        && timed_out == 0
        && file_errors == 0
        && !no_tests_discovered
        && !filter_no_match;

    // --- JSON machine output: emit ONLY the document, no human lines (like `check --errors=json`). ---
    if opts.json {
        let text = render_json(
            &outcomes,
            &file_error_msgs,
            total,
            passed_count,
            failed,
            errored,
            over_memory,
            timed_out,
            filtered_out,
        );
        return TestReport { text, passed };
    }

    let mut report = String::new();
    // File-level errors first (unchanged position + shape).
    for (file, msg) in &file_error_msgs {
        report.push_str(&format!("ERROR {file}\n  {msg}\n"));
    }

    match opts.verbosity {
        Verbosity::Quiet => {
            // One char per test, then the summary line. No per-test lines.
            let mut dots = String::new();
            for o in &outcomes {
                let (ch, code) = match &o.verdict {
                    Verdict::Pass => ('.', 32),
                    Verdict::Fail { .. } => ('F', 31),
                    Verdict::Error { .. } => ('E', 31),
                    Verdict::OverMemory { .. } => ('M', 33),
                    Verdict::TimedOut { .. } => ('T', 33),
                };
                dots.push_str(&paint(&ch.to_string(), code, opts.color));
            }
            if !dots.is_empty() {
                report.push_str(&dots);
                report.push('\n');
            }
        }
        Verbosity::Normal | Verbosity::Verbose => {
            for o in &outcomes {
                render_line(&mut report, o, &opts);
            }
        }
    }

    report.push_str(&format!(
        "\n{total} test(s): {passed_count} passed, {failed} failed, {errored} errored"
    ));
    // Off-by-default clauses: each stays absent unless its feature fired, so the common output is
    // byte-identical to the pre-wave runner.
    if over_memory > 0 {
        report.push_str(&format!(", {over_memory} over-memory"));
    }
    if timed_out > 0 {
        report.push_str(&format!(", {timed_out} timed out"));
    }
    if file_errors > 0 {
        report.push_str(&format!(", {file_errors} file error(s)"));
    }
    if opts.filter.is_some() {
        report.push_str(&format!(" ({filtered_out} filtered out)"));
    }
    if no_tests_discovered {
        report.push_str(" — no tests discovered");
    }
    if filter_no_match {
        // Safe: filter is Some here.
        let pat = opts.filter.as_deref().unwrap_or("");
        report.push_str(&format!(" — no tests matched '{pat}'"));
    }
    // Total timing is `-v`-only (non-deterministic → never in default/quiet, which the gate compares).
    if opts.verbosity == Verbosity::Verbose {
        let total_ms: u128 = outcomes.iter().map(|o| o.duration.as_millis()).sum();
        report.push_str(&format!(" in {total_ms}ms"));
    }
    report.push('\n');

    TestReport {
        text: report,
        passed,
    }
}

/// Render one per-test line into `report` for `Normal`/`Verbose`. Colorizes the tag, appends `-v`
/// timing, and (when `--show-output`) the indented captured stdout under a non-pass line.
fn render_line(report: &mut String, o: &Outcome, opts: &RunOpts) {
    let c = opts.color;
    match &o.verdict {
        Verdict::Pass => {
            report.push_str(&format!("{} {} ({})", paint("PASS", 32, c), o.name, o.file))
        }
        Verdict::Fail { line, msg } => report.push_str(&format!(
            "{} {} ({}:{}) {}",
            paint("FAIL", 31, c),
            o.name,
            o.file,
            line,
            msg
        )),
        Verdict::Error { line, msg } => report.push_str(&format!(
            "{} {} ({}:{}) {}",
            paint("ERROR", 31, c),
            o.name,
            o.file,
            line,
            msg
        )),
        Verdict::OverMemory { msg } => report.push_str(&format!(
            "{} {} ({}) {}",
            paint("OVER-MEMORY", 33, c),
            o.name,
            o.file,
            msg
        )),
        Verdict::TimedOut { msg } => report.push_str(&format!(
            "{} {} ({}) {}",
            paint("TIMED-OUT", 33, c),
            o.name,
            o.file,
            msg
        )),
    }
    if opts.verbosity == Verbosity::Verbose {
        report.push_str(&format!(" ({}ms)", o.duration.as_millis()));
    }
    report.push('\n');
    // `--show-output`: a failing test's captured stdout, indented, for debugging (pytest show-on-fail).
    if opts.show_output && !matches!(o.verdict, Verdict::Pass) && !o.captured_out.is_empty() {
        for line in o.captured_out.lines() {
            report.push_str(&format!("    {line}\n"));
        }
    }
}

/// Build the `--errors=json` document: `{"tests":[{name,file,line?,status,duration_ms},…],"totals":{…}}`.
/// Diverges from `check`/`run`'s bare array (it needs `totals`), but reuses the flag name + the
/// suppress-all-human-output behavior for CLI consistency.
#[allow(clippy::too_many_arguments)]
fn render_json(
    outcomes: &[Outcome],
    file_error_msgs: &[(String, String)],
    total: usize,
    passed: usize,
    failed: usize,
    errored: usize,
    over_memory: usize,
    timed_out: usize,
    filtered_out: usize,
) -> String {
    let mut s = String::from("{\"tests\":[");
    for (i, o) in outcomes.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"name\":{},\"file\":{},",
            json_string(&o.name),
            json_string(&o.file)
        ));
        if let Some(line) = verdict_line(&o.verdict) {
            s.push_str(&format!("\"line\":{line},"));
        }
        s.push_str(&format!(
            "\"status\":{},\"duration_ms\":{}}}",
            json_string(verdict_status(&o.verdict)),
            o.duration.as_millis()
        ));
    }
    s.push_str("],\"totals\":{");
    s.push_str(&format!(
        "\"total\":{total},\"passed\":{passed},\"failed\":{failed},\"errored\":{errored},\
         \"over_memory\":{over_memory},\"timed_out\":{timed_out},\"filtered_out\":{filtered_out},\
         \"file_errors\":{}}}}}\n",
        file_error_msgs.len()
    ));
    s
}

/// Compile + run one `*_test.chz` file on the selected engine (`parallel`: `false` = cooperative
/// serial VM, `true` = M:N OS-thread VM), returning a per-test outcome list (or a compile-error
/// message for the whole file). Compilation is engine-independent and stays on the caller's thread;
/// BOTH engine runs then dispatch on a [`crate::vm::on_vm_stack`] thread — the M:N scheduler needs
/// the large VM stack, and the SERIAL VM needs it too for deep structural recursion (a cyclic-key
/// `==` walks to `MAX_STRUCTURAL_DEPTH` = 10000 before faulting recoverably, which overflows the
/// 8 MB main thread but not the 384 MB VM stack). This matches `chezzi run` (both engines run on
/// [`crate::vm::run_file_with_entry`]'s VM-stack thread), so a `test` verdict mirrors a `run`.
/// Returns `(per-test outcomes, count filtered out by `--filter`)` or a whole-file compile-error msg.
fn run_file(file: &Path, parallel: bool, opts: &RunOpts) -> Result<(Vec<Outcome>, usize), String> {
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
    let opts = opts.clone();

    crate::vm::on_vm_stack(move || invoke_all(program, file_label, parallel, &opts))
}

/// Run every `test fn` + suite in a compiled program on a fresh VM, returning per-test outcomes (or
/// an init-error message for the whole file). Engine-agnostic: `parallel` selects serial vs M:N via
/// [`Vm::set_parallel`] before the module top-levels run. Ownership (not `&Arc`) so the M:N variant
/// can move it onto its own stack thread.
fn invoke_all(
    program: Arc<Program>,
    file_label: String,
    parallel: bool,
    opts: &RunOpts,
) -> Result<(Vec<Outcome>, usize), String> {
    let mut vm = Vm::for_program(Arc::clone(&program));
    vm.set_parallel(parallel);
    // `--max-heap` cap (0 = off). Per-test reset of the over-memory latch is VM-side (in each invoke
    // entry point), so `run_suite` needs no cap threading — the VM is already configured.
    vm.set_max_heap(opts.max_heap);
    // `--timeout` cap (0 = off). Like the heap cap it is VM config read at each invoke entry (which
    // arms a fresh deadline), so `run_suite` needs no threading — the VM is already configured.
    vm.set_timeout(opts.timeout_ms);
    // Initialize the module(s): run top-levels once so globals/functions/structs exist.
    if let Err(e) = vm.init_for_tests() {
        return Err(format!(
            "error initializing test module: {} (line {})",
            e.message, e.span.line
        ));
    }

    let mut outcomes: Vec<Outcome> = Vec::new();
    let mut filtered_out = 0usize;

    // Free tests, in declaration order.
    for (name, proto) in program.tests.iter() {
        if filtered(opts, name) {
            filtered_out += 1;
            continue;
        }
        let start = Instant::now();
        let verdict = match vm.invoke_test(*proto) {
            Ok(()) => Verdict::Pass,
            Err(e) => verdict_from_fault(e),
        };
        let duration = start.elapsed();
        let out = vm.take_out(); // stdout: discarded unless `--show-output` (kept reusable either way)
        let non_pass = !matches!(verdict, Verdict::Pass);
        outcomes.push(Outcome {
            name: name.clone(),
            file: file_label.clone(),
            verdict,
            duration,
            captured_out: if opts.show_output { out } else { String::new() },
        });
        if opts.fail_fast && non_pass {
            vm.reap_after_tests();
            return Ok((outcomes, filtered_out));
        }
    }

    // Suites: construct once, run lifecycle hooks around each test method.
    for suite in program.suites.iter() {
        run_suite(
            &mut vm,
            suite,
            &file_label,
            &mut outcomes,
            opts,
            &mut filtered_out,
        );
        // `--fail-fast`: stop launching further suites once one produced a non-pass.
        if opts.fail_fast && outcomes.iter().any(|o| !matches!(o.verdict, Verdict::Pass)) {
            break;
        }
    }

    vm.reap_after_tests();
    Ok((outcomes, filtered_out))
}

/// True if `--filter` is active and the displayed test `name` does not contain the substring.
fn filtered(opts: &RunOpts, name: &str) -> bool {
    opts.filter
        .as_deref()
        .is_some_and(|pat| !name.contains(pat))
}

/// Drive one suite: construct the instance once, then for each test method run
/// `before_each?` → method → `after_each?` (always, even on failure, like `defer`), with
/// `before_all?`/`after_all?` framing the whole suite.
fn run_suite(
    vm: &mut Vm,
    suite: &crate::vm::op::SuiteInfo,
    file: &str,
    out: &mut Vec<Outcome>,
    opts: &RunOpts,
    filtered_out: &mut usize,
) {
    let hook = |name: &str| suite.hooks.get(name).copied();
    // A whole-suite setup failure (bad ctor / before_all) is reported as one ERROR per test method —
    // but a `--filter`ed-out method is skipped (and counted), same as if it had run.
    let push_all_error =
        |out: &mut Vec<Outcome>, filtered_out: &mut usize, line: usize, msg: &str| {
            for (tname, _) in suite.tests.iter() {
                let name = format!("{}::{}", suite.name, tname);
                if filtered(opts, &name) {
                    *filtered_out += 1;
                    continue;
                }
                out.push(Outcome {
                    name,
                    file: file.to_string(),
                    verdict: Verdict::Error {
                        line,
                        msg: msg.to_string(),
                    },
                    duration: Duration::ZERO,
                    captured_out: String::new(),
                });
            }
        };

    // Construct the instance. A failure here fails every test in the suite (nothing can run). A
    // crashed constructor is setup failure → ERROR-class (the test never ran), whatever the fault.
    let instance = match vm.build_suite_instance(suite.new_thunk) {
        Ok(v) => v,
        Err(e) => {
            push_all_error(
                out,
                filtered_out,
                e.span.line,
                &format!("suite construction failed: {}", e.message),
            );
            return;
        }
    };

    // before_all? — a failure fails the whole suite (no test method runs); after_all still runs.
    // A hook fault is setup failure → ERROR-class.
    if let Some(p) = hook("before_all")
        && let Err(e) = vm.invoke_suite_method(p, instance)
    {
        push_all_error(
            out,
            filtered_out,
            e.span.line,
            &format!("before_all failed: {}", e.message),
        );
        if let Some(ap) = hook("after_all") {
            let _ = vm.invoke_suite_method(ap, instance);
        }
        let _ = vm.take_out();
        return;
    }

    for (tname, proto) in suite.tests.iter() {
        let name = format!("{}::{}", suite.name, tname);
        if filtered(opts, &name) {
            *filtered_out += 1;
            continue;
        }
        let start = Instant::now();
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
        let duration = start.elapsed();
        let captured = vm.take_out();
        let non_pass = !matches!(verdict, Verdict::Pass);
        out.push(Outcome {
            name,
            file: file.to_string(),
            verdict,
            duration,
            captured_out: if opts.show_output {
                captured
            } else {
                String::new()
            },
        });
        // `--fail-fast`: skip the remaining methods of this suite (after_all still runs below).
        if opts.fail_fast && non_pass {
            break;
        }
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

    /// gaps.md W6-10 — a value moved across the airlock into a `Channel`/`Shared` core lives as a
    /// `WireValue` in an `Arc` OUTSIDE every `Heap`, so `live_bytes` counted it nowhere and a
    /// 195 MB channel backlog sailed straight past a 200 KB `--max-heap` cap (PASS, rc=0). The
    /// cached per-core byte summary now feeds `live_bytes`, so the natural *concurrent* runaway —
    /// an unbounded backlog, or data parked in a `Shared` — trips the cap like any other.
    #[test]
    fn over_memory_counts_offheap_wire_payload() {
        let d = TmpDir::new();
        // The cap is deliberately far above anything either program keeps in its own `Heap` — only
        // the off-heap wire storage can reach it, so the assertion isolates W6-10.
        const CAP: usize = 8_000_000;
        let d2 = TmpDir::new();
        let backlog = d.write(
            "backlog_test.chz",
            "test fn backlog():\n    ch := Channel[List[int]](200000)\n    \
             for i in range(40000):\n        ch.send([i, i, i, i, i, i, i, i])\n",
        );
        // The sibling single-value path: a big list parked in a `Shared` (a REPLACING store — the
        // summary is refreshed by `SharedCore::store`, not only at construction).
        let parked = d2.write(
            "parked_test.chz",
            "import std.concurrency\n\ntest fn parked():\n    s := Shared([0])\n    \
             xs := []\n    for i in range(150000):\n        xs.push(i)\n    s.set(xs)\n    \
             zs := []\n    for i in range(2000):\n        zs = [i]\n",
        );
        for (label, f) in [("backlog", &backlog), ("parked", &parked)] {
            for parallel in [false, true] {
                let report = run_tests_capped(f, parallel, CAP);
                assert!(
                    report.text.contains(&format!("OVER-MEMORY {label}")),
                    "off-heap wire storage must trip the cap ({label}, parallel={parallel}); \
                     report:\n{}",
                    report.text
                );
                assert!(
                    !report.text.contains(&format!("FAIL {label}"))
                        && !report.text.contains(&format!("ERROR {label}")),
                    "must be OVER-MEMORY, not FAIL/ERROR ({label}, parallel={parallel}); \
                     report:\n{}",
                    report.text
                );
            }
        }
    }

    /// W6-10 review, the SAMPLING half: counting the off-heap bytes is worthless if the cap is
    /// never sampled. `over_cap` is only evaluated inside `sweep()`, and `sweep()` only runs when
    /// `should_collect()` fires — which used to be a pure heap-OBJECT count. A program that pushes
    /// megabytes across the airlock while allocating ~2 `Obj`s per iteration therefore never swept,
    /// never sampled, and PASSED at hundreds of MB against an 8 MB cap. Both shapes below build
    /// their payload ONCE and then only re-send it, so object churn cannot be doing the work.
    #[test]
    fn over_memory_trips_without_object_churn() {
        const CAP: usize = 8_000_000;
        let d = TmpDir::new();
        let d2 = TmpDir::new();
        // ~1 MB string built once, 300 sends = ~300 MB off-heap (peak RSS 304 MB pre-fix).
        let msg = d.write(
            "msg_test.chz",
            "test fn msg():\n    parts: List[str] = []\n    \
             for i in range(100000):\n        parts.push(\"0123456789\")\n    \
             blob := \"\".join(parts)\n    ch := Channel[str](10000)\n    \
             for i in range(300):\n        ch.send(blob)\n    assert true\n",
        );
        // The sibling shape: a 200k-int list built once, sent 100 times (3369 MB RSS pre-fix).
        let ints = d2.write(
            "ints_test.chz",
            "test fn ints():\n    big: List[int] = []\n    \
             for i in range(200000):\n        big.push(i)\n    \
             ch := Channel[List[int]](1000)\n    for i in range(100):\n        ch.send(big)\n    \
             assert true\n",
        );
        for (label, f) in [("msg", &msg), ("ints", &ints)] {
            for parallel in [false, true] {
                let report = run_tests_capped(f, parallel, CAP);
                assert!(
                    report.text.contains(&format!("OVER-MEMORY {label}")),
                    "off-heap growth must PACE a sweep so the cap is sampled ({label}, \
                     parallel={parallel}); report:\n{}",
                    report.text
                );
                assert!(
                    !report.text.contains(&format!("FAIL {label}"))
                        && !report.text.contains(&format!("ERROR {label}")),
                    "must be OVER-MEMORY, not FAIL/ERROR ({label}, parallel={parallel}); \
                     report:\n{}",
                    report.text
                );
            }
        }
    }

    /// W6-10 review — the NEGATIVE direction, which matters just as much: a program comfortably
    /// UNDER the cap must still PASS while holding a shared core. A core's payload is ONE `Arc`
    /// allocation, but `from_wire` mints a FRESH `Obj::Shared` alias slot for every crossing, so 50
    /// receives of the same handle used to charge that payload 50 times and fire a spurious
    /// OVER-MEMORY at ~1/50th of the real footprint — a resource cap whose false-positive rate grows
    /// with fan-out. Bytes are now charged once per CORE per heap.
    #[test]
    fn under_cap_still_passes_with_many_handles_to_one_core() {
        let d = TmpDir::new();
        // ~1 MB parked off-heap, 50 live reconstructed handles to that ONE core, 8 MB cap.
        let f = d.write(
            "alias_test.chz",
            "import std.concurrency\n\ntest fn alias():\n    xs := []\n    \
             for i in range(20000):\n        xs.push(i)\n    s := Shared(xs)\n    \
             ch := Channel[Shared[List[int]]](100)\n    for i in range(50):\n        ch.send(s)\n    \
             hs := []\n    for i in range(50):\n        hs.push(ch.recv())\n    \
             junk := []\n    for i in range(5000):\n        junk = [i]\n    \
             assert hs.len() == 50\n",
        );
        for parallel in [false, true] {
            let report = run_tests_capped(&f, parallel, 8_000_000);
            assert!(
                report.text.contains("PASS alias"),
                "50 handles to one core must not multiply its payload (parallel={parallel}); \
                 report:\n{}",
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
    fn recover_inside_defer_does_not_catch_timeout() {
        // W7-3 boundary: the cancel-bypass carve-out is (a)-ONLY. A `recover:` installed inside a
        // `defer` body now catches a CANCEL-time fault, but a `--timeout` wall-clock abort is still
        // recover-proof there too — `is_timed_out` is not gated on `deferring`.
        //
        // The TimedOut BUCKET alone does not discriminate: the outer `--timeout` fires in the test
        // body (`deferring == 0`), takes the unconditional bypass, and the funnel re-stamps
        // `.timed_out()` onto whatever error emerges (exec.rs), so the bucket is TimedOut either way.
        // The load-bearing assertion is therefore the SWALLOWED marker: it can only appear if the
        // in-defer `recover:` caught the abort and execution continued past it.
        let d = TmpDir::new();
        let f = d.write(
            "recdefertimeout_test.chz",
            "test fn t():\n    defer:\n        r := recover:\n            while true:\n                pass\n        assert false, \"SWALLOWED-{r}\"\n    while true:\n        pass\n",
        );
        let report = run_tests_timed(&f, true, 0, 50);
        assert!(!report.passed, "report:\n{}", report.text);
        assert!(
            !report.text.contains("SWALLOWED"),
            "a recover: INSIDE a defer CAUGHT the timeout abort — execution continued past it; report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("TIMED-OUT t"),
            "a recover: INSIDE a defer must NOT catch a timeout abort; report:\n{}",
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
    fn executor_timeout_not_demoted_by_earlier_ordinary_fault() {
        // W7-5 review Fix 1: `Executor.shutdown()`'s M:N drain (`reduce_task_slots`) must select the
        // lowest-index HARD-HALT fault over an earlier ordinary one, not just the lowest index overall
        // — else a `--timeout`/`--max-heap` abort gets demoted to a plain catchable error by an
        // earlier sibling's fault. Job 0 faults immediately (ordinary); job 1 spins past the wall-clock
        // cap (hard halt, `is_timed_out`). PRE-FIX: `reduce_task_slots` picks job 0's error purely by
        // index, `recover:` catches it (it carries no hard-halt marker), and control falls through to
        // the trailing assert — the test lands FAIL, not TIMED-OUT. POST-FIX: job 1's `is_timed_out`
        // fault wins selection, bypasses `recover:` entirely (the marker-keyed bypass in `exec.rs`),
        // and the test lands TIMED-OUT with control never reaching the trailing assert.
        let d = TmpDir::new();
        let f = d.write(
            "exhalt_test.chz",
            "import std.concurrency\nfn boom():\n    panic(\"ordinary\")\nfn spin():\n    while true:\n        pass\ntest fn t():\n    ex := Executor()\n    ex.submit(boom)\n    ex.submit(spin)\n    r := recover: ex.shutdown()\n    assert false, \"SWALLOWED\"\n",
        );
        let report = run_tests_timed(&f, true, 0, 50);
        assert!(!report.passed, "report:\n{}", report.text);
        assert!(
            !report.text.contains("SWALLOWED"),
            "an earlier ordinary fault demoted the later hard-halt timeout to a catchable error; \
             report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("TIMED-OUT t"),
            "the hard-halt fault must win selection and bucket TIMED-OUT; report:\n{}",
            report.text
        );
    }

    #[test]
    fn timeout_reaches_a_job_blocked_on_a_channel_and_on_wait() {
        // Eager execution makes a blocking `Executor` job WAIT for a value instead of declaring a
        // deadlock, which is correct (its submitter is still running and may send) but means a job
        // waiting on a value that never comes hangs by design — decision D's accepted hang. That is
        // only tolerable because `--timeout` can still reach it. It can only reach it because the
        // eager blocking paths check the deadline THEMSELVES: a blocked job never reaches
        // `jump_checked`'s loop back-edge, where every other path observes it.
        //
        // Both blocking shapes are covered because they use different mechanisms — `recv` waits on the
        // one channel's condvar, while `wait:` has N arms and no single condvar to wait on, so it
        // bounded-polls. The `wait:` arm is the one that would silently spin forever if the shared
        // halt check were dropped from it.
        for (name, body) in [
            (
                "blockrecv_test.chz",
                "import std.concurrency\nch: Channel[int] = Channel[int](1)\nfn w():\n    print(ch.recv())\ntest fn t():\n    ex := Executor()\n    ex.submit(w)\n    ex.shutdown()\n    assert false, \"SWALLOWED\"\n",
            ),
            (
                "blockwait_test.chz",
                "import std.concurrency\na: Channel[int] = Channel[int](1)\nb: Channel[int] = Channel[int](1)\nfn w():\n    wait:\n        v := a.recv(): print(v)\n        v := b.recv(): print(v)\ntest fn t():\n    ex := Executor()\n    ex.submit(w)\n    ex.shutdown()\n    assert false, \"SWALLOWED\"\n",
            ),
        ] {
            let d = TmpDir::new();
            let f = d.write(name, body);
            let report = run_tests_timed(&f, true, 0, 300);
            assert!(!report.passed, "{name} report:\n{}", report.text);
            assert!(
                report.text.contains("TIMED-OUT t"),
                "{name}: --timeout must reach a job blocked in an eager Executor; report:\n{}",
                report.text
            );
            assert!(
                !report.text.contains("SWALLOWED"),
                "{name}: control must never fall through the blocked shutdown; report:\n{}",
                report.text
            );
        }
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

    // ---- CLI ergonomics wave: filter / fail-fast / show-output / json / verbosity / color ----

    /// A 3-free-test file used by several ergonomics tests: `alpha` passes, `beta` fails (assert
    /// false), `gamma` passes — in declaration order.
    fn three_test_file(d: &TmpDir) -> PathBuf {
        d.write(
            "abc_test.chz",
            "test fn alpha():\n    assert true\ntest fn beta():\n    assert false, \"boom\"\ntest fn gamma():\n    assert true\n",
        )
    }

    fn opts_with(f: impl FnOnce(&mut RunOpts)) -> RunOpts {
        let mut o = RunOpts::default();
        f(&mut o);
        o
    }

    #[test]
    fn filter_runs_only_matching_and_reports_count() {
        let d = TmpDir::new();
        let f = d.write(
            "abg_test.chz",
            "test fn alpha():\n    assert true\ntest fn beta():\n    assert true\ntest fn gamma():\n    assert true\n",
        );
        let report = run_tests_opts(&f, false, opts_with(|o| o.filter = Some("alpha".into())));
        assert!(report.passed, "report:\n{}", report.text);
        assert!(
            report.text.contains("PASS alpha"),
            "report:\n{}",
            report.text
        );
        assert!(!report.text.contains("beta"), "report:\n{}", report.text);
        assert!(!report.text.contains("gamma"), "report:\n{}", report.text);
        assert!(
            report.text.contains("(2 filtered out)"),
            "summary must note the filtered count; report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("1 test(s): 1 passed"),
            "report:\n{}",
            report.text
        );
    }

    #[test]
    fn filter_zero_match_is_clear_failure() {
        let d = TmpDir::new();
        let f = d.write(
            "abg2_test.chz",
            "test fn alpha():\n    assert true\ntest fn beta():\n    assert true\n",
        );
        let report = run_tests_opts(&f, false, opts_with(|o| o.filter = Some("zzz".into())));
        assert!(
            !report.passed,
            "a zero-match filter must fail; report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("no tests matched 'zzz'"),
            "report:\n{}",
            report.text
        );
    }

    #[test]
    fn fail_fast_stops_at_first_failure() {
        let d = TmpDir::new();
        let f = three_test_file(&d);
        let report = run_tests_opts(&f, false, opts_with(|o| o.fail_fast = true));
        assert!(!report.passed, "report:\n{}", report.text);
        assert!(
            report.text.contains("PASS alpha"),
            "report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("FAIL beta"),
            "report:\n{}",
            report.text
        );
        assert!(
            !report.text.contains("gamma"),
            "fail-fast must skip the test after the first failure; report:\n{}",
            report.text
        );
    }

    #[test]
    fn show_output_surfaces_failing_stdout() {
        let d = TmpDir::new();
        let f = d.write(
            "print_test.chz",
            "test fn boom():\n    print(\"hello-debug\")\n    assert false\n",
        );
        let shown = run_tests_opts(&f, false, opts_with(|o| o.show_output = true));
        assert!(
            shown.text.contains("hello-debug"),
            "--show-output must surface a failing test's stdout; report:\n{}",
            shown.text
        );
        // Control: default discards it.
        let hidden = run_tests(&f, false);
        assert!(
            !hidden.text.contains("hello-debug"),
            "default must discard stdout; report:\n{}",
            hidden.text
        );
    }

    #[test]
    fn show_output_not_shown_for_passing_test() {
        let d = TmpDir::new();
        let f = d.write(
            "printok_test.chz",
            "test fn ok():\n    print(\"secret\")\n    assert true\n",
        );
        let report = run_tests_opts(&f, false, opts_with(|o| o.show_output = true));
        assert!(
            !report.text.contains("secret"),
            "a PASSing test's stdout stays discarded (show-on-failure); report:\n{}",
            report.text
        );
    }

    #[test]
    fn json_emits_parseable_per_test_and_totals() {
        let d = TmpDir::new();
        let f = three_test_file(&d);
        let report = run_tests_opts(&f, false, opts_with(|o| o.json = true));
        let t = &report.text;
        assert!(
            t.trim_start().starts_with('{'),
            "json must be an object; got:\n{t}"
        );
        assert!(t.contains("\"status\":\"pass\""), "got:\n{t}");
        assert!(t.contains("\"status\":\"fail\""), "got:\n{t}");
        assert!(t.contains("\"duration_ms\":"), "got:\n{t}");
        assert!(t.contains("\"totals\""), "got:\n{t}");
        assert!(
            t.contains("\"failed\":1"),
            "totals must count the fail; got:\n{t}"
        );
        assert!(t.contains("\"passed\":2"), "got:\n{t}");
        // No human PASS/FAIL lines when json (like `check --errors=json`).
        assert!(
            !t.contains("PASS "),
            "json must suppress human lines; got:\n{t}"
        );
        assert!(!t.contains("FAIL "), "got:\n{t}");
        // A fail entry carries a line; count the status occurrences by hand.
        let pass_hits = t.matches("\"status\":\"pass\"").count();
        assert_eq!(pass_hits, 2, "got:\n{t}");
        assert!(
            t.contains("\"line\":"),
            "a fail entry must carry its line; got:\n{t}"
        );
    }

    #[test]
    fn verbose_shows_timing_default_does_not() {
        let d = TmpDir::new();
        let f = d.write("v_test.chz", "test fn quick():\n    assert true\n");
        let verbose = run_tests_opts(&f, false, opts_with(|o| o.verbosity = Verbosity::Verbose));
        assert!(
            verbose.text.contains("ms"),
            "-v output must carry timing; report:\n{}",
            verbose.text
        );
        let default = run_tests(&f, false);
        assert!(
            !default.text.contains("ms"),
            "default output must NOT carry timing (byte-identical invariant); report:\n{}",
            default.text
        );
    }

    #[test]
    fn quiet_renders_dots() {
        let d = TmpDir::new();
        let f = d.write(
            "q_test.chz",
            "test fn a():\n    assert true\ntest fn b():\n    assert false\n",
        );
        let report = run_tests_opts(&f, false, opts_with(|o| o.verbosity = Verbosity::Quiet));
        assert!(
            report.text.contains(".F") || report.text.contains(".\nF"),
            "quiet must render one char per test; report:\n{}",
            report.text
        );
        assert!(
            !report.text.contains("PASS a"),
            "quiet has no per-test lines; report:\n{}",
            report.text
        );
        assert!(
            report.text.contains("2 test(s):"),
            "quiet still prints the summary; report:\n{}",
            report.text
        );
    }

    #[test]
    fn color_absent_under_harness_present_when_forced() {
        let d = TmpDir::new();
        let f = d.write("col_test.chz", "test fn a():\n    assert true\n");
        let plain = run_tests(&f, false);
        assert!(
            !plain.text.contains("\x1b["),
            "default (color off) must emit no ANSI so the captured harness is stable; report:\n{:?}",
            plain.text
        );
        let colored = run_tests_opts(&f, false, opts_with(|o| o.color = true));
        assert!(
            colored.text.contains("\x1b[32m"),
            "color:true must green the PASS tag; report:\n{:?}",
            colored.text
        );
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
