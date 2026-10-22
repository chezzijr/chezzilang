//! `chezzi test` — the Rust-side test runner (M20).
//!
//! Discovers `*_test.chz` files, compiles each as its own entry graph, and invokes every `test fn`
//! (free tests + suite methods) on a reusable VM. The runner is Rust-side by necessity: a Chezzi
//! `recover:` only hands back the fault *message*, not its `span`, so only Rust catching the
//! `RuntimeError` directly gets the `.span` (hence `file:line`) the headline feature needs.
//!
//! This orchestration's output is Rust-formatted `PASS/FAIL`, not Chezzi program stdout, so no
//! golden-output gate applies.

use crate::base::op::Program;
use crate::vm::Vm;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The outcome of a `chezzi test` run: the rendered report and whether everything passed.
pub struct TestReport {
    /// The lossy render — what every string-matching consumer reads. Byte-identical to the
    /// pre-change report for any UTF-8 capture, EXCEPT a captured `"\r\n"`: the render loop moved
    /// from `str::lines()` (which strips a trailing `\r`) to `split_inclusive(|&b| b == b'\n')`, so
    /// `"a\r\n"` now renders `    a\r\n` where it used to render `    a\n` — the `\r` is the
    /// program's real byte.
    pub text: String,
    // ponytail: a second copy of `bytes`; make it a `Cow` accessor if a report ever gets large.
    /// The byte-exact report the CLI writes to fd 1 (`--show-output` can carry non-UTF-8).
    pub bytes: Vec<u8>,
    /// A failing test's captured stderr (`io.eprint`), under `--show-output` — never mixed into
    /// `bytes`/`text`, since that would change every string-matching consumer of the stdout report.
    pub stderr_bytes: Vec<u8>,
    pub passed: bool,
}

/// Report verbosity for the CLI ergonomics wave. `Normal` is the DEFAULT and its output is
/// byte-identical to the pre-wave runner (the load-bearing invariant the golden-output gate compares).
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
/// and the `chz_suite_passes` regression gate stays green. Each new flag is gated on
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

/// A fault's rendered position + call trace, shared by the `Fail`/`Error` verdicts.
///
/// `pos` is the `path:line:col` string [`crate::lexer::render_span`] produces — identical to what
/// `chezzi run` prints. `line` is kept ONLY so `--errors=json`'s `line` key stays byte-unchanged
/// (that document deliberately carries no `col`). `frames` holds the [`crate::vm::format_frames`]
/// entries whose call span carries a real module coordinate; it is EMPTY when the fault was in the
/// test body itself — the sole frame there is the invoke frame `Vm::invoke_test` pushes with
/// `Span::RUNTIME`, which `fault_site` drops (`## Digest` gotcha 9).
#[derive(Clone)]
struct FaultSite {
    pos: String,
    line: usize,
    frames: Vec<String>,
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
        site: FaultSite,
        msg: String,
    },
    Error {
        site: FaultSite,
        msg: String,
    },
    /// `chezzi test --max-heap` — the test's live heap exceeded the cap and it was hard-aborted. No
    /// meaningful source line (the abort fires at a GC boundary, not a statement). Counts as failure.
    OverMemory {
        msg: String,
    },
    /// `chezzi test --timeout` — the test ran longer than the wall-clock cap and was hard-aborted at a
    /// loop back-edge. No meaningful source line (the abort fires at a checkpoint, not a statement).
    /// Counts as failure.
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
    captured_out: Vec<u8>,
    /// The test's captured stderr (`io.eprint`) — kept ONLY when `--show-output` is on, same policy
    /// as `captured_out`.
    captured_err: Vec<u8>,
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
        Verdict::Fail { site, .. } | Verdict::Error { site, .. } => Some(site.line),
        Verdict::Pass | Verdict::OverMemory { .. } | Verdict::TimedOut { .. } => None,
    }
}

/// The failure text for a verdict, if it has one (every non-`Pass` variant carries a `msg`).
fn verdict_msg(v: &Verdict) -> Option<&str> {
    match v {
        Verdict::Pass => None,
        Verdict::Fail { msg, .. }
        | Verdict::Error { msg, .. }
        | Verdict::OverMemory { msg }
        | Verdict::TimedOut { msg } => Some(msg),
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

/// Resolve a caught fault's position + call trace. Called IMMEDIATELY after the failing invoke and
/// before any further invoke — `Vm::take_fault_trace` drains the deepest-wins latch, so a later
/// invoke would otherwise steal or clear it.
///
/// `fallback` preserves the pre-fix behaviour of naming the test file when the fault's own span
/// carries no module coordinate (`Span::file == 0`, e.g. a synthesized span) — see `## Decisions`.
///
/// The captured trace always carries the invoke frame `Vm::invoke_test` (or
/// `invoke_suite_method`/`build_suite_instance`) pushed with `Span::RUNTIME`, `file: 0`. That frame
/// is dropped here (filtering on `span.file != 0`), NOT in `vm::format_frames`, because
/// `chezzi run`'s trace never carries one and moving the rule into the shared renderer would change
/// `chezzi run`'s output too.
fn fault_site(
    vm: &mut Vm,
    e: &crate::vm::RuntimeError,
    files: &[(u32, PathBuf)],
    fallback: &Path,
) -> FaultSite {
    let path = files
        .iter()
        .find(|(f, _)| *f == e.span.file)
        .map(|(_, p)| p.as_path())
        .unwrap_or(fallback);
    let trace: Vec<crate::vm::TraceFrame> = vm
        .take_fault_trace()
        .into_iter()
        .filter(|f| f.span.file != 0)
        .collect();
    FaultSite {
        pos: crate::lexer::render_span(e.span, Some(path)),
        line: e.span.line as usize,
        frames: crate::vm::format_frames(&trace, files),
    }
}

/// Bucket a caught per-test `RuntimeError`. The `is_over_memory` marker takes priority: a `--max-heap`
/// hard-abort is `OverMemory` regardless of what fault emerged from the unwind (a defer may fault while
/// unwinding — the abort re-stamps the marker onto whatever propagates, and it crosses the worker→
/// parent boundary WITH the error, so a spawned task's runaway aborts on either engine). Else an
/// `assert` failure → `Fail`, anything else → `Error`.
fn verdict_from_fault(e: crate::vm::RuntimeError, site: FaultSite) -> Verdict {
    // `is_timed_out` checked FIRST. It is mutually exclusive with `is_over_memory` in practice (a
    // timeout trips at a loop back-edge, over-memory at a GC boundary — a single abort carries one
    // marker), so the order only fixes a nominal tie; either way the abort buckets, never FAIL/ERROR.
    if e.is_timed_out {
        Verdict::TimedOut { msg: e.message }
    } else if e.is_over_memory {
        Verdict::OverMemory { msg: e.message }
    } else if e.is_assert {
        Verdict::Fail {
            site,
            msg: e.message,
        }
    } else {
        Verdict::Error {
            site,
            msg: e.message,
        }
    }
}

/// Post-hoc `--timeout` honesty check, applied to the measured wall clock AFTER the invoke returns.
/// The deadline abort fires at a VM checkpoint, and a `Kind::Blocking` native (every `std.fs` /
/// `std.io` / `std.process` / `std.request` syscall) is offloaded to the dirty pool with no checkpoint
/// until it returns — so a test that spends its life inside one runs to completion and lands `Pass`
/// having blown the cap by any margin. We cannot interrupt it, but reporting PASS on it teaches
/// distrust of every green run, so it buckets `TimedOut` here instead. The message deliberately
/// differs from the abort path's `test exceeded --timeout (Nms)` so a report distinguishes "cut
/// short" from "overran, uninterruptible".
///
/// **Only `Pass` is upgraded.** A `Fail`/`Error`/`OverMemory`/already-`TimedOut` verdict keeps its own
/// bucket: an assertion failure says more about the test than "also slow" does.
fn apply_timeout_overrun(verdict: Verdict, duration: Duration, opts: &RunOpts) -> Verdict {
    if opts.timeout_ms > 0
        && matches!(verdict, Verdict::Pass)
        && duration.as_millis() >= u128::from(opts.timeout_ms)
    {
        return Verdict::TimedOut {
            msg: format!(
                "test exceeded --timeout ({}ms): ran {}ms and could not be aborted (a blocking \
                 std.fs/std.io/std.process/std.request call has no checkpoint until it returns)",
                opts.timeout_ms,
                duration.as_millis()
            ),
        };
    }
    verdict
}

/// Run every `test fn` discovered under `root` (a single `*_test.chz` file or a directory walked
/// recursively). Returns the rendered report + overall pass/fail. Never panics on a test fault — the
/// VM stays reusable, so one failing test does not abort the rest.
pub fn run_tests(root: &Path) -> TestReport {
    run_tests_opts(root, RunOpts::default())
}

/// Like [`run_tests`], plus the opt-in `--max-heap` per-test cap (`max_heap`: byte count, `0` = OFF).
pub fn run_tests_capped(root: &Path, max_heap: usize) -> TestReport {
    run_tests_opts(
        root,
        RunOpts {
            max_heap,
            ..Default::default()
        },
    )
}

/// Like [`run_tests_capped`], plus the opt-in `--timeout` per-test wall-clock cap (`timeout_ms`: ms,
/// `0` = OFF).
pub fn run_tests_timed(root: &Path, max_heap: usize, timeout_ms: u64) -> TestReport {
    run_tests_opts(
        root,
        RunOpts {
            max_heap,
            timeout_ms,
            ..Default::default()
        },
    )
}

/// The core runner. `opts` carries every opt-in
/// ergonomics knob. **`RunOpts::default()` reproduces the pre-wave output byte-for-byte** — every new
/// clause is gated on its field being non-default, so the render path a no-flag run takes is unchanged
/// and the `chz_suite_passes` regression gate stays green.
///
/// **Determinism / ordering:** files run in sorted path order; within a file, free tests run in
/// declaration order, then each suite's methods in declaration order. `--fail-fast` stops at the first
/// non-pass in exactly that order (later tests simply don't run).
pub fn run_tests_opts(root: &Path, opts: RunOpts) -> TestReport {
    let files = match collect_test_files(root) {
        Ok(f) => f,
        Err(e) => {
            let text = format!("chezzi test: {e}\n");
            return TestReport {
                bytes: text.clone().into_bytes(),
                text,
                stderr_bytes: Vec::new(),
                passed: false,
            };
        }
    };
    if files.is_empty() {
        let text = format!("no *_test.chz files found under {}\n", root.display());
        return TestReport {
            bytes: text.clone().into_bytes(),
            text,
            stderr_bytes: Vec::new(),
            passed: false,
        };
    }

    let mut outcomes: Vec<Outcome> = Vec::new();
    // A compile/type/resolve error fails a whole file before any test runs. Collected (not rendered
    // inline) so the JSON path can suppress the human `ERROR <file>` lines.
    let mut file_error_msgs: Vec<(String, FileError)> = Vec::new();
    // Tests skipped by `--filter` (threaded up from the invoke sites — discovery of names happens
    // inside the compiled program, not before).
    let mut filtered_out = 0usize;
    // One `chezzi test` invocation prints each distinct checker warning ONCE — see `run_file`.
    let mut seen_warnings: std::collections::HashSet<String> = std::collections::HashSet::new();

    for file in &files {
        match run_file(file, &opts, &mut seen_warnings) {
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
            Err(fe) => {
                file_error_msgs.push((file.display().to_string(), fe));
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
        return TestReport {
            bytes: text.clone().into_bytes(),
            text,
            stderr_bytes: Vec::new(),
            passed,
        };
    }

    let mut report = Vec::new();
    // File-level errors first (unchanged position + shape).
    for (file, fe) in &file_error_msgs {
        report.extend_from_slice(format!("ERROR {file}\n  {}\n", fe.rendered).as_bytes());
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
                report.extend_from_slice(dots.as_bytes());
                report.push(b'\n');
            }
        }
        Verbosity::Normal | Verbosity::Verbose => {
            for o in &outcomes {
                render_line(&mut report, o, &opts);
            }
        }
    }

    report.extend_from_slice(
        format!("\n{total} test(s): {passed_count} passed, {failed} failed, {errored} errored")
            .as_bytes(),
    );
    // Off-by-default clauses: each stays absent unless its feature fired, so the common output is
    // byte-identical to the pre-wave runner.
    if over_memory > 0 {
        report.extend_from_slice(format!(", {over_memory} over-memory").as_bytes());
    }
    if timed_out > 0 {
        report.extend_from_slice(format!(", {timed_out} timed out").as_bytes());
    }
    if file_errors > 0 {
        report.extend_from_slice(format!(", {file_errors} file error(s)").as_bytes());
    }
    if opts.filter.is_some() {
        report.extend_from_slice(format!(" ({filtered_out} filtered out)").as_bytes());
    }
    if no_tests_discovered {
        report.extend_from_slice(" — no tests discovered".as_bytes());
    }
    if filter_no_match {
        // Safe: filter is Some here.
        let pat = opts.filter.as_deref().unwrap_or("");
        report.extend_from_slice(format!(" — no tests matched '{pat}'").as_bytes());
    }
    // Total timing is `-v`-only (non-deterministic → never in default/quiet, which the gate compares).
    if opts.verbosity == Verbosity::Verbose {
        let total_ms: u128 = outcomes.iter().map(|o| o.duration.as_millis()).sum();
        report.extend_from_slice(format!(" in {total_ms}ms").as_bytes());
    }
    report.push(b'\n');

    // `--show-output`'s stderr half: one block per non-pass test with captured stderr, appended to
    // fd 2 (never mixed into `report`/`bytes`, which stay the stdout document).
    let mut stderr_bytes = Vec::new();
    if opts.show_output {
        for o in &outcomes {
            if matches!(o.verdict, Verdict::Pass) || o.captured_err.is_empty() {
                continue;
            }
            stderr_bytes.extend_from_slice(
                format!("--- captured stderr: {} ({}) ---\n", o.name, o.file).as_bytes(),
            );
            stderr_bytes.extend_from_slice(&o.captured_err);
            if !o.captured_err.ends_with(b"\n") {
                stderr_bytes.push(b'\n');
            }
        }
    }

    TestReport {
        text: String::from_utf8_lossy(&report).into_owned(),
        bytes: report,
        stderr_bytes,
        passed,
    }
}

/// Render one per-test line into `report` for `Normal`/`Verbose`. Colorizes the tag, appends `-v`
/// timing, and (when `--show-output`) the indented captured stdout under a non-pass line.
fn render_line(report: &mut Vec<u8>, o: &Outcome, opts: &RunOpts) {
    let c = opts.color;
    match &o.verdict {
        Verdict::Pass => report.extend_from_slice(
            format!("{} {} ({})", paint("PASS", 32, c), o.name, o.file).as_bytes(),
        ),
        Verdict::Fail { site, msg } => report.extend_from_slice(
            format!("{} {} ({}) {}", paint("FAIL", 31, c), o.name, site.pos, msg).as_bytes(),
        ),
        Verdict::Error { site, msg } => report.extend_from_slice(
            format!(
                "{} {} ({}) {}",
                paint("ERROR", 31, c),
                o.name,
                site.pos,
                msg
            )
            .as_bytes(),
        ),
        Verdict::OverMemory { msg } => report.extend_from_slice(
            format!(
                "{} {} ({}) {}",
                paint("OVER-MEMORY", 33, c),
                o.name,
                o.file,
                msg
            )
            .as_bytes(),
        ),
        Verdict::TimedOut { msg } => report.extend_from_slice(
            format!(
                "{} {} ({}) {}",
                paint("TIMED-OUT", 33, c),
                o.name,
                o.file,
                msg
            )
            .as_bytes(),
        ),
    }
    if opts.verbosity == Verbosity::Verbose {
        report.extend_from_slice(format!(" ({}ms)", o.duration.as_millis()).as_bytes());
    }
    report.push(b'\n');
    let frames: &[String] = match &o.verdict {
        Verdict::Fail { site, .. } | Verdict::Error { site, .. } => &site.frames,
        Verdict::Pass | Verdict::OverMemory { .. } | Verdict::TimedOut { .. } => &[],
    };
    for frame in frames {
        report.extend_from_slice(frame.as_bytes());
        report.push(b'\n');
    }
    // `--show-output`: a failing test's captured stdout, indented, for debugging (pytest show-on-fail).
    if opts.show_output && !matches!(o.verdict, Verdict::Pass) && !o.captured_out.is_empty() {
        for line in o.captured_out.split_inclusive(|&b| b == b'\n') {
            report.extend_from_slice(b"    ");
            report.extend_from_slice(line);
            if line.last() != Some(&b'\n') {
                report.push(b'\n');
            }
        }
    }
}

/// Build the `--errors=json` document: `{"tests":[{name,file,line?,status,message?,duration_ms},…],"totals":{…}}`.
/// Diverges from `check`/`run`'s bare array (it needs `totals`), but reuses the flag name + the
/// suppress-all-human-output behavior for CLI consistency.
#[allow(clippy::too_many_arguments)]
/// One whole-file diagnostic (resolve/check/compile/init failure) that stopped a `*_test.chz` file
/// before any test ran. Carries both renderings: `rendered` for the human `ERROR <file>` line,
/// `message`/`file`/`line`/`col`/`help` for `--errors=json`'s `file_errors` array. `file`/`line`/`col`
/// are `None` when the failure has no module coordinate (never a claimed-wrong one — DEC-048).
struct FileError {
    rendered: String,
    message: String,
    file: Option<String>,
    line: Option<u32>,
    col: Option<u32>,
    help: Option<String>,
}

/// Build a [`FileError`] from the checker's first error. `path` is the module the error is anchored
/// in, when known.
fn file_error_from_check(e: &crate::checker::CheckError, path: Option<&Path>) -> FileError {
    FileError {
        rendered: e.render(path),
        message: e.message.clone(),
        file: path.map(|p| crate::lexer::display_path(p).to_string_lossy().into_owned()),
        line: Some(e.span.line),
        col: Some(e.span.col),
        help: e.help.clone(),
    }
}

#[allow(clippy::too_many_arguments)]
fn render_json(
    outcomes: &[Outcome],
    file_error_msgs: &[(String, FileError)],
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
            "\"status\":{}",
            json_string(verdict_status(&o.verdict))
        ));
        if let Some(msg) = verdict_msg(&o.verdict) {
            s.push_str(&format!(",\"message\":{}", json_string(msg)));
        }
        s.push_str(&format!(",\"duration_ms\":{}}}", o.duration.as_millis()));
    }
    s.push_str("],\"file_errors\":[");
    for (i, (_, fe)) in file_error_msgs.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('{');
        if let Some(file) = &fe.file {
            s.push_str(&format!("\"file\":{},", json_string(file)));
        }
        if let (Some(line), Some(col)) = (fe.line, fe.col) {
            s.push_str(&format!("\"line\":{line},\"col\":{col},"));
        }
        s.push_str(&format!(
            "\"severity\":\"error\",\"message\":{}",
            json_string(&fe.message)
        ));
        if let Some(help) = &fe.help {
            s.push_str(&format!(",\"help\":{}", json_string(help)));
        }
        s.push('}');
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

/// Compile + run one `*_test.chz` file, returning a per-test outcome list (or a compile-error
/// message for the whole file). Compilation stays on the caller's thread; the run then dispatches on
/// a [`crate::vm::on_vm_stack`] thread — a cyclic-key `==` walks to `MAX_STRUCTURAL_DEPTH` = 10000
/// before faulting recoverably, which overflows the 8 MB main thread but not the 384 MB VM stack.
/// This matches `chezzi run` (which also runs on [`crate::run::run_file_with_entry`]'s VM-stack
/// thread), so a `test` verdict mirrors a `run`.
/// Returns `(per-test outcomes, count filtered out by `--filter`)` or a whole-file [`FileError`].
fn run_file(
    file: &Path,
    opts: &RunOpts,
    seen_warnings: &mut std::collections::HashSet<String>,
) -> Result<(Vec<Outcome>, usize), FileError> {
    let graph = crate::resolver::build_graph(file).map_err(|e| FileError {
        rendered: e.to_string(),
        message: e.message.clone(),
        file: e
            .path
            .as_deref()
            .map(|p| crate::lexer::display_path(p).to_string_lossy().into_owned()),
        line: Some(e.span.line),
        col: Some(e.span.col),
        help: None,
    })?;
    let (res, warns) = crate::checker::check_graph_diags(&graph, None);
    // Advisory only — they go to stderr and change no test outcome, so a `--errors=json` run's
    // stdout stays the machine-readable result document.
    //
    // Deduplicated across the whole `chezzi test` invocation: every test file is its own entry
    // graph, so a warning in a SHARED imported module is re-checked (and would be re-printed) once
    // per importer. The key is the declaring module's path + span + message, not `Span::file` —
    // that id is a per-graph sequence and so differs between two files' graphs — and not the
    // rendered line alone, since two entry modules can collide on line/col/message.
    for w in &warns {
        let module = graph_path(&graph, w.span.file)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        if seen_warnings.insert(format!("{module}|{w}")) {
            crate::errln!("{}", w.render(graph_path(&graph, w.span.file)));
        }
    }
    if let Err(errs) = res {
        // Surface the first type error (matches `chezzi check`'s headline).
        let first = errs
            .first()
            .map(|e| file_error_from_check(e, graph_path(&graph, e.span.file)));
        return Err(first.unwrap_or(FileError {
            rendered: "type error".into(),
            message: "type error".into(),
            file: None,
            line: None,
            col: None,
            help: None,
        }));
    }
    let program = crate::compiler::compile_graph(&graph).map_err(|e| {
        let path = graph_path(&graph, e.span.file);
        FileError {
            rendered: e.message.clone(),
            message: e.message.clone(),
            file: path.map(|p| crate::lexer::display_path(p).to_string_lossy().into_owned()),
            line: Some(e.span.line),
            col: Some(e.span.col),
            help: None,
        }
    })?;
    let program: Arc<Program> = Arc::new(program);
    let file_label = file.display().to_string();
    let opts = opts.clone();

    crate::vm::on_vm_stack(move || invoke_all(program, file_label, &opts))
}

/// The source path a `Span::file` id resolves to within `graph`'s module set — the same lookup the
/// warning dedupe key already performs, shared so `CheckError::render`'s two callers here don't
/// repeat the `find`.
fn graph_path(graph: &crate::resolver::ModuleGraph, file: u32) -> Option<&Path> {
    graph
        .modules
        .iter()
        .find(|m| m.file == file)
        .map(|m| m.id.0.as_path())
}

/// Run every `test fn` + suite in a compiled program on a fresh VM, returning per-test outcomes (or
/// an init-error message for the whole file). Ownership (not `&Arc`) so it can be moved onto its own
/// stack thread.
fn invoke_all(
    program: Arc<Program>,
    file_label: String,
    opts: &RunOpts,
) -> Result<(Vec<Outcome>, usize), FileError> {
    let mut vm = Vm::for_program(Arc::clone(&program));
    let files = program.file_table();
    // `--max-heap` cap (0 = off). Per-test reset of the over-memory latch is VM-side (in each invoke
    // entry point), so `run_suite` needs no cap threading — the VM is already configured.
    vm.set_max_heap(opts.max_heap);
    // `--timeout` cap (0 = off). Like the heap cap it is VM config read at each invoke entry (which
    // arms a fresh deadline), so `run_suite` needs no threading — the VM is already configured.
    vm.set_timeout(opts.timeout_ms);
    // Initialize the module(s): run top-levels once so globals/functions/structs exist.
    if let Err(e) = vm.init_for_tests() {
        let rendered = format!(
            "error initializing test module: {} (line {})",
            e.message, e.span.line
        );
        let path = files
            .iter()
            .find(|(f, _)| *f == e.span.file)
            .map(|(_, p)| p);
        return Err(FileError {
            message: format!("error initializing test module: {}", e.message),
            rendered,
            file: path.map(|p| crate::lexer::display_path(p).to_string_lossy().into_owned()),
            line: path.map(|_| e.span.line),
            col: path.map(|_| e.span.col),
            help: None,
        });
    }

    let mut outcomes: Vec<Outcome> = Vec::new();
    let mut filtered_out = 0usize;
    let fallback = PathBuf::from(&file_label);

    // Free tests, in declaration order.
    for (name, proto) in program.tests.iter() {
        if filtered(opts, name) {
            filtered_out += 1;
            continue;
        }
        let start = Instant::now();
        let mark = vm.exec_registry_mark();
        // TICKET-200 — `rank_end` before the reap: the reap takes the slots `rank_end` reads.
        let ran = vm.invoke_test(*proto);
        let mut verdict = match vm.rank_end(ran) {
            Ok(()) => Verdict::Pass,
            Err(e) => {
                let site = fault_site(&mut vm, &e, &files, &fallback);
                verdict_from_fault(e, site)
            }
        };
        // Join this test's own detached executors (A2). A fault here upgrades a `Pass` only — a
        // test that already failed keeps its own bucket, and its jobs are still joined either way.
        if let Err(e) = vm.reap_executors_since(mark)
            && matches!(verdict, Verdict::Pass)
        {
            let site = fault_site(&mut vm, &e, &files, &fallback);
            verdict = verdict_from_fault(e, site);
        }
        let duration = start.elapsed();
        let verdict = apply_timeout_overrun(verdict, duration, opts);
        let out = vm.take_out_bytes(); // stdout: discarded unless `--show-output` (kept reusable either way)
        let err = vm.take_err_bytes();
        let non_pass = !matches!(verdict, Verdict::Pass);
        outcomes.push(Outcome {
            name: name.clone(),
            file: file_label.clone(),
            verdict,
            duration,
            captured_out: if opts.show_output { out } else { Vec::new() },
            captured_err: if opts.show_output { err } else { Vec::new() },
        });
        if opts.fail_fast && non_pass {
            reap_after_tests_row(&mut vm, &mut outcomes, &file_label, &files, &fallback);
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
            &files,
            &fallback,
        );
        // `--fail-fast`: stop launching further suites once one produced a non-pass.
        if opts.fail_fast && outcomes.iter().any(|o| !matches!(o.verdict, Verdict::Pass)) {
            break;
        }
    }

    reap_after_tests_row(&mut vm, &mut outcomes, &file_label, &files, &fallback);
    Ok((outcomes, filtered_out))
}

/// The end-of-file reap. A fault the reap returns is reported as its own errored row,
/// `(executor job)`: it is never dropped (TICKET-208, C2). A job fault is its Task outcome (TICKET-289).
fn reap_after_tests_row(
    vm: &mut Vm,
    outcomes: &mut Vec<Outcome>,
    file_label: &str,
    files: &[(u32, PathBuf)],
    fallback: &Path,
) {
    if let Err(e) = vm.reap_after_tests() {
        let site = fault_site(vm, &e, files, fallback);
        outcomes.push(Outcome {
            name: "(executor job)".to_string(),
            file: file_label.to_string(),
            verdict: verdict_from_fault(e, site),
            duration: Duration::ZERO,
            captured_out: Vec::new(),
            captured_err: Vec::new(),
        });
    }
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
#[allow(clippy::too_many_arguments)]
fn run_suite(
    vm: &mut Vm,
    suite: &crate::base::op::SuiteInfo,
    file: &str,
    out: &mut Vec<Outcome>,
    opts: &RunOpts,
    filtered_out: &mut usize,
    files: &[(u32, PathBuf)],
    fallback: &Path,
) {
    let hook = |name: &str| suite.hooks.get(name).copied();
    // A whole-suite setup failure (bad ctor / before_all) is reported as one ERROR per test method —
    // but a `--filter`ed-out method is skipped (and counted), same as if it had run.
    let push_all_error =
        |out: &mut Vec<Outcome>, filtered_out: &mut usize, site: &FaultSite, msg: &str| {
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
                        site: site.clone(),
                        msg: msg.to_string(),
                    },
                    duration: Duration::ZERO,
                    captured_out: Vec::new(),
                    captured_err: Vec::new(),
                });
            }
        };

    // Construct the instance. A failure here fails every test in the suite (nothing can run). A
    // crashed constructor is setup failure → ERROR-class (the test never ran), whatever the fault.
    let instance = match vm.build_suite_instance(suite.new_thunk) {
        Ok(v) => v,
        Err(e) => {
            let site = fault_site(vm, &e, files, fallback);
            push_all_error(
                out,
                filtered_out,
                &site,
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
        let site = fault_site(vm, &e, files, fallback);
        push_all_error(
            out,
            filtered_out,
            &site,
            &format!("before_all failed: {}", e.message),
        );
        run_after_all(vm, suite, instance, file, out, opts, files, fallback);
        let _ = vm.take_out_bytes();
        return;
    }

    for (tname, proto) in suite.tests.iter() {
        let name = format!("{}::{}", suite.name, tname);
        if filtered(opts, &name) {
            *filtered_out += 1;
            continue;
        }
        let start = Instant::now();
        let mark = vm.exec_registry_mark();
        let mut verdict = Verdict::Pass;
        // before_each? — a failure is the test's failure (the method is skipped); after_each still
        // runs. A hook crash is setup failure → ERROR-class.
        if let Some(p) = hook("before_each")
            && let Err(e) = vm.invoke_suite_method(p, instance)
        {
            let site = fault_site(vm, &e, files, fallback);
            verdict = Verdict::Error {
                site,
                msg: format!("before_each failed: {}", e.message),
            };
        }
        // The test method itself (only if before_each passed). This is the ONE place an `assert`
        // fault reads as FAIL; any other fault in the body is ERROR (via `verdict_from_fault`).
        // TICKET-200 — ranked by `rank_end` before the reap, like a free test.
        if matches!(verdict, Verdict::Pass)
            && let Err(e) = {
                let ran = vm.invoke_suite_method(*proto, instance);
                vm.rank_end(ran.map(|_| ()))
            }
        {
            let site = fault_site(vm, &e, files, fallback);
            verdict = verdict_from_fault(e, site);
        }
        // after_each? — ALWAYS runs (even on failure, like `defer`), so the invocation must NOT be
        // short-circuited. It does not mask the original failure; only if the test passed but
        // after_each itself faults does that become the test's (ERROR-class) failure.
        if let Some(p) = hook("after_each") {
            let ae = vm.invoke_suite_method(p, instance);
            if matches!(verdict, Verdict::Pass)
                && let Err(e) = ae
            {
                let site = fault_site(vm, &e, files, fallback);
                verdict = Verdict::Error {
                    site,
                    msg: format!("after_each failed: {}", e.message),
                };
            }
        }
        // Join this test's own detached executors (A2), same policy as the free-test loop: a fault
        // here upgrades a `Pass` only.
        if let Err(e) = vm.reap_executors_since(mark)
            && matches!(verdict, Verdict::Pass)
        {
            let site = fault_site(vm, &e, files, fallback);
            verdict = verdict_from_fault(e, site);
        }
        let duration = start.elapsed();
        let verdict = apply_timeout_overrun(verdict, duration, opts);
        let captured = vm.take_out_bytes();
        let captured_stderr = vm.take_err_bytes();
        let non_pass = !matches!(verdict, Verdict::Pass);
        out.push(Outcome {
            name,
            file: file.to_string(),
            verdict,
            duration,
            captured_out: if opts.show_output {
                captured
            } else {
                Vec::new()
            },
            captured_err: if opts.show_output {
                captured_stderr
            } else {
                Vec::new()
            },
        });
        // `--fail-fast`: skip the remaining methods of this suite (after_all still runs below).
        if opts.fail_fast && non_pass {
            break;
        }
    }

    // after_all? — runs after the last test method.
    run_after_all(vm, suite, instance, file, out, opts, files, fallback);
}

/// Run a suite's `after_all` hook, if it has one, and push its fault as its own `<Suite>::after_all`
/// error row when it faults (never an upgrade of the last test's verdict — CPython's `tearDownClass`
/// reports the same way: a separate error entry, the suite's own methods still counted as run).
/// A no-op when the suite has no `after_all`.
#[allow(clippy::too_many_arguments)]
fn run_after_all(
    vm: &mut Vm,
    suite: &crate::base::op::SuiteInfo,
    instance: crate::vm::value::Value,
    file: &str,
    out: &mut Vec<Outcome>,
    opts: &RunOpts,
    files: &[(u32, PathBuf)],
    fallback: &Path,
) {
    let Some(p) = suite.hooks.get("after_all").copied() else {
        return;
    };
    let start = Instant::now();
    let mark = vm.exec_registry_mark();
    let result = vm.invoke_suite_method(p, instance);
    // A drain fault folds into this row only when the hook itself did not already fault — the hook's
    // own fault takes priority, same as the per-test policy: whichever failure says more wins.
    let result = match result {
        Ok(_) => vm.reap_executors_since(mark),
        Err(e) => vm.rank_end(Err(e)),
    };
    let duration = start.elapsed();
    let captured = vm.take_out_bytes();
    let captured_stderr = vm.take_err_bytes();
    if let Err(e) = result {
        let site = fault_site(vm, &e, files, fallback);
        out.push(Outcome {
            name: format!("{}::after_all", suite.name),
            file: file.to_string(),
            verdict: Verdict::Error {
                site,
                msg: format!("after_all failed: {}", e.message),
            },
            duration,
            captured_out: if opts.show_output {
                captured
            } else {
                Vec::new()
            },
            captured_err: if opts.show_output {
                captured_stderr
            } else {
                Vec::new()
            },
        });
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
mod tests;
