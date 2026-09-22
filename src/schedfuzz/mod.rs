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

/// Measure a target's baseline on the fixed binary: one unseeded run at T=1 and one at T=2.
/// `Err(Unstable)` when either run times out or panics, or when the two exit codes differ — such
/// a target is printed as `UNSTABLE` by the caller and never scored as a finding.
pub fn measure_baseline(bin: &Path, t: &Target, timeout: Duration) -> Result<Baseline, Unstable> {
    let r1 = run_target(bin, t, None, 1, timeout);
    let r2 = run_target(bin, t, None, 2, timeout);
    let (c1, c2) = match (r1, r2) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(RunErr::TimedOut), _) | (_, Err(RunErr::TimedOut)) => return Err(Unstable::Timeout),
        _ => return Err(Unstable::Timeout),
    };
    if c1.stderr_text().contains("panicked at") || c2.stderr_text().contains("panicked at") {
        return Err(Unstable::Panic);
    }
    if c1.code != c2.code {
        return Err(Unstable::RcMismatch);
    }
    let check_output = match &t.kind {
        TargetKind::Program {
            expected: Some(exp),
        } => c1.stdout == *exp && c2.stdout == *exp,
        _ => false,
    };
    Ok(Baseline {
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

/// Judge one run against a target's baseline. `Err(RunErr::CouldNotRun(_))` is a harness error,
/// never a finding — the child never even ran.
pub fn judge(b: &Baseline, t: &Target, r: &Result<Capture, RunErr>) -> Option<Finding> {
    match r {
        Err(RunErr::TimedOut) => Some(Finding::Hang),
        Err(RunErr::CouldNotRun(_)) => None,
        Ok(cap) => {
            if cap.stderr_text().contains("panicked at") {
                return Some(Finding::Panic(cap.stderr_text().into_owned()));
            }
            if let Some(sig) = cap.signal {
                return Some(Finding::Panic(format!("killed by signal {sig}")));
            }
            if cap.code != b.code {
                return Some(Finding::Rc {
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
                return Some(Finding::Output);
            }
            None
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
}
