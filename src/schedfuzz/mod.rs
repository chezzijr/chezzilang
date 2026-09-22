//! The seeded-scheduler oracle harness (TICKET-167, `docs/future.md` §2b). Runs the corpus under
//! `CHEZZI_SCHED_SEED` at one or more worker counts and flags a hang, a panic, a changed exit code
//! or (where the program's output is schedule-independent) a changed stdout, judged against an
//! UNSEEDED run of the same target on the fixed baseline binary — never against a mutant's own
//! seeded run, so a target that already hangs unseeded is still comparable.
//!
//! `dead_code` is allowed module-wide, same reason as `src/difftest/mod.rs`: this is a shared
//! toolkit whose two consumers (`src/bin/schedfuzz.rs` and `tests/sched_seed_cli.rs`) exercise
//! different subsets of it.
#![allow(dead_code)]

use crate::difftest::run::{Capture, RunErr, run_one};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// How to invoke a target and, for a plain program, what its schedule-independent stdout is.
#[derive(Clone, Debug)]
pub enum TargetKind {
    /// A `tests/chz/**/*_test.chz` file, run as `chezzi test <file>`.
    ChzTest,
    /// A `examples/*.chz` file, run as `chezzi run <file>`. `expected` is the sibling
    /// `.expected` file's bytes, when the program's output is schedule-independent, else `None`.
    Program { expected: Option<Vec<u8>> },
}

#[derive(Clone, Debug)]
pub struct Target {
    pub path: PathBuf,
    pub kind: TargetKind,
}

/// Classify a single path: a `_test.chz` file runs under `chezzi test`; anything else runs under
/// `chezzi run`, picking up a sibling `.expected` file if one exists.
pub fn target_for(path: &Path) -> Target {
    let is_test = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with("_test.chz"));
    if is_test {
        return Target {
            path: path.to_path_buf(),
            kind: TargetKind::ChzTest,
        };
    }
    let expected_path = path.with_extension("expected");
    let expected = std::fs::read(&expected_path).ok();
    Target {
        path: path.to_path_buf(),
        kind: TargetKind::Program { expected },
    }
}

/// Keywords that mark a program as touching concurrency, so the corpus scan (`corpus`) does not
/// waste seed budget on single-fiber programs a scheduler choice can never perturb.
const CONCURRENCY_KEYWORDS: &[&str] = &[
    "spawn",
    "parallel:",
    "Channel[",
    "Shared[",
    "Atomic",
    "Executor",
    "std.net",
];

fn touches_concurrency(src: &str) -> bool {
    CONCURRENCY_KEYWORDS.iter().any(|k| src.contains(k))
}

/// The full corpus: every `tests/chz/**/*_test.chz`, plus every `examples/*.chz` that has a
/// sibling `.expected` file and whose source mentions concurrency. Sorted by path so a run is
/// reproducible target-order to target-order.
pub fn corpus(root: &Path) -> Vec<Target> {
    let mut out = Vec::new();

    let chz_tests_dir = root.join("tests/chz");
    let mut stack = vec![chz_tests_dir];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("_test.chz"))
            {
                out.push(target_for(&path));
            }
        }
    }

    let examples_dir = root.join("examples");
    if let Ok(entries) = std::fs::read_dir(&examples_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("chz") {
                continue;
            }
            let expected_path = path.with_extension("expected");
            if !expected_path.exists() {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&path) else {
                continue;
            };
            if !touches_concurrency(&src) {
                continue;
            }
            out.push(target_for(&path));
        }
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Run one target: `chezzi test <path>` for a `ChzTest`, `chezzi run <path>` for a `Program`.
/// Sets `CHEZZI_THREADS`; sets `CHEZZI_SCHED_SEED` when `seed` is `Some`, removes it otherwise.
pub fn run_target(
    bin: &Path,
    t: &Target,
    seed: Option<u64>,
    threads: usize,
    timeout: Duration,
) -> Result<Capture, RunErr> {
    let sub = match t.kind {
        TargetKind::ChzTest => "test",
        TargetKind::Program { .. } => "run",
    };
    let mut cmd = Command::new(bin);
    cmd.arg(sub)
        .arg(&t.path)
        .env("CHEZZI_THREADS", threads.to_string());
    match seed {
        Some(s) => {
            cmd.env("CHEZZI_SCHED_SEED", s.to_string());
        }
        None => {
            cmd.env_remove("CHEZZI_SCHED_SEED");
        }
    }
    run_one(&mut cmd, timeout)
}

/// One unseeded run at T=1 and one at T=2 of the baseline binary, on a stable target. Never built
/// from a mutant's own run — a mutant is always judged against this FIXED reference.
pub struct Baseline {
    code: Option<i32>,
    check_output: bool,
}

/// Why a target's baseline could not be trusted, so it is skipped rather than judged.
#[derive(Debug)]
pub enum Unstable {
    Timeout,
    Panic,
    RcMismatch,
}

impl Unstable {
    pub fn reason(&self) -> &'static str {
        match self {
            Unstable::Timeout => "timeout",
            Unstable::Panic => "panic",
            Unstable::RcMismatch => "rc-mismatch",
        }
    }
}

/// The result of measuring a baseline. `HarnessError` (the binary could not even be spawned) is
/// NOT the same as `Unstable` (the program ran but its own behaviour can't be trusted as a
/// reference) — a caller must treat the former as fatal, the latter as a per-target skip.
pub enum BaselineOutcome {
    Stable(Baseline),
    Unstable(Unstable),
    HarnessError(String),
}

/// Measure a target's baseline on the fixed binary: one unseeded run at T=1 and one at T=2.
/// `BaselineOutcome::Unstable` when either run times out or panics, or when the two exit codes
/// differ — such a target is printed as `UNSTABLE` by the caller and never scored as a finding.
/// `BaselineOutcome::HarnessError` when a run could not even be spawned — that is fatal, not a
/// per-target skip (same class as `Verdict::HarnessError`; W7-34).
pub fn measure_baseline(bin: &Path, t: &Target, timeout: Duration) -> BaselineOutcome {
    let r1 = run_target(bin, t, None, 1, timeout);
    let r2 = run_target(bin, t, None, 2, timeout);
    if let Err(RunErr::CouldNotRun(msg)) = &r1 {
        return BaselineOutcome::HarnessError(format!("baseline T=1: {msg}"));
    }
    if let Err(RunErr::CouldNotRun(msg)) = &r2 {
        return BaselineOutcome::HarnessError(format!("baseline T=2: {msg}"));
    }
    let (c1, c2) = match (r1, r2) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(RunErr::TimedOut), _) | (_, Err(RunErr::TimedOut)) => {
            return BaselineOutcome::Unstable(Unstable::Timeout);
        }
        _ => unreachable!("CouldNotRun handled above; only TimedOut remains"),
    };
    if c1.stderr_text().contains("panicked at") || c2.stderr_text().contains("panicked at") {
        return BaselineOutcome::Unstable(Unstable::Panic);
    }
    if c1.code != c2.code {
        return BaselineOutcome::Unstable(Unstable::RcMismatch);
    }
    let check_output = match &t.kind {
        TargetKind::Program {
            expected: Some(exp),
        } => c1.stdout == *exp && c2.stdout == *exp,
        _ => false,
    };
    BaselineOutcome::Stable(Baseline {
        code: c1.code,
        check_output,
    })
}

/// What a run flagged, judged against the target's `Baseline`.
#[derive(Debug)]
pub enum Finding {
    Hang,
    Panic(String),
    Rc { base: Option<i32>, got: Option<i32> },
    Output,
}

/// The result of judging one run. `HarnessError` is NOT a finding and NOT a clean run — the
/// child never even started (`RunErr::CouldNotRun`), so nothing was compared. Callers must treat
/// it as fatal, the same rule `src/difftest/run.rs` documents for `Outcome::HarnessError`
/// (W7-34: a harness error scored as "no finding" hides that the oracle never ran).
#[derive(Debug)]
pub enum Verdict {
    Finding(Finding),
    Clean,
    HarnessError(String),
}

/// Judge one run against a target's baseline.
pub fn judge(b: &Baseline, t: &Target, r: &Result<Capture, RunErr>) -> Verdict {
    match r {
        Err(RunErr::TimedOut) => Verdict::Finding(Finding::Hang),
        Err(RunErr::CouldNotRun(msg)) => Verdict::HarnessError(msg.clone()),
        Ok(cap) => {
            if cap.stderr_text().contains("panicked at") {
                return Verdict::Finding(Finding::Panic(cap.stderr_text().into_owned()));
            }
            if let Some(sig) = cap.signal {
                return Verdict::Finding(Finding::Panic(format!("killed by signal {sig}")));
            }
            if cap.code != b.code {
                return Verdict::Finding(Finding::Rc {
                    base: b.code,
                    got: cap.code,
                });
            }
            if let TargetKind::Program {
                expected: Some(exp),
            } = &t.kind
                && b.check_output
                && cap.stdout != *exp
            {
                return Verdict::Finding(Finding::Output);
            }
            Verdict::Clean
        }
    }
}

/// One `FINDING` line, with a replay command a human can paste.
pub fn report_line(t: &Target, seed: u64, threads: usize, finding: &Finding) -> String {
    let kind = match finding {
        Finding::Hang => "hang",
        Finding::Panic(_) => "panic",
        Finding::Rc { .. } => "rc",
        Finding::Output => "output",
    };
    let sub = match t.kind {
        TargetKind::ChzTest => "test",
        TargetKind::Program { .. } => "run",
    };
    format!(
        "FINDING kind={kind} seed={seed} threads={threads} program={} replay: CHEZZI_SCHED_SEED={seed} CHEZZI_THREADS={threads} chezzi {sub} {}",
        t.path.display(),
        t.path.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_for_names_a_test_file() {
        let t = target_for(Path::new("tests/chz/spec/foo_test.chz"));
        assert!(matches!(t.kind, TargetKind::ChzTest));
    }

    #[test]
    fn target_for_names_a_plain_program() {
        let t = target_for(Path::new("examples/hello.chz"));
        assert!(matches!(t.kind, TargetKind::Program { .. }));
    }

    /// A run that could not even spawn (`RunErr::CouldNotRun`) must not be judged clean — the
    /// child never ran, so there is nothing to compare. Precedent: `src/difftest/run.rs`
    /// `a_hang_retry_harness_error_is_not_silently_a_timeout` (W7-34: a harness error must be
    /// FATAL, never scored as "no finding").
    #[test]
    fn judge_reports_a_harness_error_not_a_clean_run() {
        let b = Baseline {
            code: Some(0),
            check_output: false,
        };
        let t = Target {
            path: PathBuf::from("tests/chz/spec/foo_test.chz"),
            kind: TargetKind::ChzTest,
        };
        let r: Result<Capture, RunErr> = Err(RunErr::CouldNotRun(
            "could not run \"chezzi\": No such file or directory (os error 2)".into(),
        ));
        let verdict = judge(&b, &t, &r);
        assert!(
            matches!(verdict, Verdict::HarnessError(_)),
            "a CouldNotRun run must report Verdict::HarnessError, not a clean/finding verdict, got {verdict:?}"
        );
    }
}
