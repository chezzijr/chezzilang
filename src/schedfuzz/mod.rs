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
/// `chezzi run`, picking up a sibling `.expected` file if one exists. Whether that `.expected` is
/// actually trusted for the output check is decided later, per-target, by `measure_baseline`'s
/// sampled-disagreement check — never here by a keyword. A prior version skipped `.expected`
/// whenever the source contained `spawn`/`Executor`, reading `docs/concurrency.md`'s streaming-CLI
/// contract off a substring; the owner dropped it (review 2026-09-22) because it silenced the
/// output check on 42 of 46 examples — including ones with exactly one printing context — and
/// `contains("spawn")` also matched comments.
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
    let expected = std::fs::read(path.with_extension("expected")).ok();
    Target {
        path: path.to_path_buf(),
        kind: TargetKind::Program { expected },
    }
}

/// A corpus target whose schedfuzz divergence is already understood and tracked in `docs/gaps.md`,
/// so a routine clean-tree sweep does not keep re-reporting the same explained behaviour as a NEW
/// finding every run. Matched BY FILE NAME, never by a source keyword (the keyword approach is what
/// the owner dropped from `target_for`, review 2026-09-22) — each entry is hand-picked and cites the
/// row that explains it. Two classes so far:
/// - a program that prints across a `spawn`/`Executor` task boundary: `chezzi run`'s cross-task
///   print order is nondeterministic BY CONTRACT (`docs/concurrency.md` "Output ordering: streaming
///   CLI vs buffered sink"), so an `output` finding here is that CONTRACT surfacing, not a new race
///   — W15-4.
/// - a Chezzi test file gated on a wall-clock ratio (an `optimized_build()`-style
///   `if not optimized_build(): return` guard before an `assert d < N`): under schedfuzz's own
///   concurrent job load the ratio flips and the assert genuinely fails UNSEEDED too (measured, not
///   assumed — `regex_test.chz` failed 1/10 unseeded under sustained sibling load on an otherwise
///   idle box) — W15-5.
pub struct KnownTarget {
    pub file_name: &'static str,
    pub row: &'static str,
    pub reason: &'static str,
}

pub const KNOWN_TARGETS: &[KnownTarget] = &[
    KnownTarget {
        file_name: "try_recv.chz",
        row: "W15-4",
        reason: "cross-task print order is nondeterministic by contract (docs/concurrency.md)",
    },
    KnownTarget {
        file_name: "parallel.chz",
        row: "W15-4",
        reason: "cross-task print order is nondeterministic by contract (docs/concurrency.md)",
    },
    KnownTarget {
        file_name: "parallel_cross_nursery_ok.chz",
        row: "W15-4",
        reason: "cross-task print order is nondeterministic by contract (docs/concurrency.md)",
    },
    KnownTarget {
        file_name: "channel.chz",
        row: "W15-4",
        reason: "cross-task print order is nondeterministic by contract (docs/concurrency.md)",
    },
    KnownTarget {
        file_name: "channel_block.chz",
        row: "W15-4",
        reason: "cross-task print order is nondeterministic by contract (docs/concurrency.md)",
    },
    KnownTarget {
        file_name: "executor_autodrain.chz",
        row: "W15-4",
        reason: "cross-task print order is nondeterministic by contract (docs/concurrency.md)",
    },
    KnownTarget {
        file_name: "regex_test.chz",
        row: "W15-5",
        reason: "find_all_text_extraction_over_a_million_tokens_is_not_too_slow's wall-clock ratio gate flakes under sweep CPU contention (measured 1/10 unseeded under load)",
    },
    KnownTarget {
        file_name: "nested_nursery_open_outer_body_test.chz",
        row: "W15-5",
        reason: "fan_open/fan_flat's wall-clock ratio gate flakes under sweep CPU contention",
    },
    KnownTarget {
        file_name: "net_close_test.chz",
        row: "W15-3",
        reason: "a write parked on a closed Socket can return Ok instead of an error, at CHEZZI_THREADS=1",
    },
    KnownTarget {
        file_name: "generator_channel_test.chz",
        row: "W15-6",
        reason: "a generator resumed from `for v in ch:` inside a spawned task can hang at CHEZZI_THREADS=0",
    },
    KnownTarget {
        file_name: "cancel_test.chz",
        row: "W15-7",
        reason: "a cancel-propagation program can hang at CHEZZI_THREADS=0",
    },
];

/// Look up a target's `KnownTarget` entry by file name, if any.
pub fn known_target(t: &Target) -> Option<&'static KnownTarget> {
    let name = t.path.file_name().and_then(|n| n.to_str())?;
    KNOWN_TARGETS.iter().find(|k| k.file_name == name)
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
#[derive(Debug)]
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
#[derive(Debug)]
pub enum BaselineOutcome {
    Stable(Baseline),
    Unstable(Unstable),
    HarnessError(String),
}

/// Unseeded reps at the swept worker count used to establish a target's baseline. One rep
/// undercounts: an exit code that only occasionally flakes (a load-sensitive wall-clock margin,
/// `docs/gaps-archive.md` class) can agree on a single sample and read as stable. This governs the
/// rc/panic/hang baseline only — output byte-exactness is decided from a sampled-disagreement check
/// over the same reps (`check_output`), because that class of divergence can be arbitrarily rare
/// unseeded (measured 0/40 for `parallel_cross_nursery_ok.chz`) yet still real.
const BASELINE_REPS: usize = 5;

/// Measure a target's baseline on the fixed binary: `BASELINE_REPS` unseeded runs, ALL at
/// `threads` — the same worker count as the job this baseline will judge. Judging a `threads=0` job
/// against a baseline measured at T=1/T=2 is comparing it to a different program run (review
/// blocking finding, 2026-09-22: `regex_test.chz`'s `find_all_text_extraction_over_a_million_tokens_is_not_too_slow`
/// is a wall-clock ratio gate that only flakes under contention, and a baseline taken at the wrong
/// count could hide or invent that flake). `BaselineOutcome::Unstable` when any run times out or
/// panics, or when any exit code differs from the first — such a target is printed as `UNSTABLE` by
/// the caller and never scored as a finding. A stable exit code with DISAGREEING stdout across the
/// reps does not make the whole target unstable: it only turns off the output check
/// (`check_output = false`), keeping the hang/panic/rc checks live. `BaselineOutcome::HarnessError`
/// when a run could not even be spawned — that is fatal, not a per-target skip (same class as
/// `Verdict::HarnessError`; W7-34).
pub fn measure_baseline(
    bin: &Path,
    t: &Target,
    threads: usize,
    timeout: Duration,
) -> BaselineOutcome {
    let mut runs = Vec::with_capacity(BASELINE_REPS);
    for _ in 0..BASELINE_REPS {
        match run_target(bin, t, None, threads, timeout) {
            Ok(cap) => runs.push(cap),
            Err(RunErr::CouldNotRun(msg)) => {
                return BaselineOutcome::HarnessError(format!("baseline T={threads}: {msg}"));
            }
            Err(RunErr::TimedOut) => return BaselineOutcome::Unstable(Unstable::Timeout),
        }
    }
    if runs.iter().any(|c| c.stderr_text().contains("panicked at")) {
        return BaselineOutcome::Unstable(Unstable::Panic);
    }
    let code0 = runs[0].code;
    if runs.iter().any(|c| c.code != code0) {
        return BaselineOutcome::Unstable(Unstable::RcMismatch);
    }
    let check_output = match &t.kind {
        TargetKind::Program {
            expected: Some(exp),
        } => runs.iter().all(|c| c.stdout == *exp),
        _ => false,
    };
    BaselineOutcome::Stable(Baseline {
        code: code0,
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

    /// The baseline must be measured at the SAME worker count as the job it will judge — a job run
    /// at `CHEZZI_THREADS=3` must never be judged against a baseline that ran at 1 or 2 instead.
    /// Fake `chezzi` binary records the `CHEZZI_THREADS` value of every invocation it sees; asking
    /// `measure_baseline` for `threads=3` must log only `3`, never `1` or `2` (the old hardcoded
    /// pair).
    #[test]
    fn measure_baseline_runs_only_at_the_requested_worker_count() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "schedfuzz-baseline-threads-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("threads.log");
        std::fs::write(&log, b"").unwrap();
        let bin = dir.join("fake_chezzi.sh");
        let script = format!(
            "#!/bin/sh\necho \"$CHEZZI_THREADS\" >> '{l}'\nprintf 'A'\nexit 0\n",
            l = log.display()
        );
        std::fs::write(&bin, script).unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();

        let target_path = dir.join("prog.chz");
        std::fs::write(&target_path, b"# fake").unwrap();
        let t = target_for(&target_path);

        let outcome = measure_baseline(&bin, &t, 3, Duration::from_secs(5));
        let seen = std::fs::read_to_string(&log).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            matches!(outcome, BaselineOutcome::Stable(_)),
            "expected a stable baseline, got {outcome:?}"
        );
        let lines: Vec<&str> = seen.lines().collect();
        assert!(!lines.is_empty(), "expected at least one recorded run");
        assert!(
            lines.iter().all(|l| *l == "3"),
            "every baseline run must use the requested threads=3, saw: {lines:?}"
        );
    }

    /// The owner dropped the `spawn`/`Executor` keyword skip (review 2026-09-22): it silenced the
    /// output check on 42 of 46 examples, including ones with exactly one printing context, and a
    /// bare `contains("spawn")` matched comments too. `target_for` now always loads a sibling
    /// `.expected` when one exists; whether the output check actually runs is decided later, by
    /// `measure_baseline`'s sampled-disagreement check, not by a keyword.
    #[test]
    fn target_for_loads_expected_even_for_a_spawning_program() {
        let dir = std::env::temp_dir().join(format!(
            "schedfuzz-target-for-spawn-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let target_path = dir.join("prog.chz");
        std::fs::write(&target_path, b"parallel:\n    spawn:\n        print(1)\n").unwrap();
        std::fs::write(target_path.with_extension("expected"), b"1\n").unwrap();
        let t = target_for(&target_path);
        let _ = std::fs::remove_dir_all(&dir);
        match t.kind {
            TargetKind::Program { expected } => assert_eq!(
                expected.as_deref(),
                Some(&b"1\n"[..]),
                "a spawning program's .expected must still load; the sampled baseline decides whether to trust it"
            ),
            other => panic!("expected Program, got {other:?}"),
        }
    }

    /// A target whose unseeded output disagrees across SEVERAL runs (not just the two the old
    /// baseline sampled) must skip the output check rather than trust two lucky matches. Measured
    /// on `main`: `try_recv.chz`/`parallel.chz`/`parallel_cross_nursery_ok.chz` all document their
    /// own printed order as scheduling-dependent, yet a 2-run baseline happened to hit the same
    /// order both times and scored `check_output = true`, turning documented nondeterminism into
    /// a false `output` finding once seeding was on. Fake `chezzi` binary here outputs `A` on 2 of
    /// every 3 calls and `B` on the third — a 2-run sample can land on `A, A`.
    #[test]
    fn baseline_established_from_several_reps_skips_output_check_on_disagreement() {
        use std::os::unix::fs::PermissionsExt;

        let dir =
            std::env::temp_dir().join(format!("schedfuzz-baseline-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let counter = dir.join("counter");
        std::fs::write(&counter, b"0").unwrap();
        let bin = dir.join("fake_chezzi.sh");
        let script = format!(
            "#!/bin/sh\nn=$(($(cat '{c}') + 1))\necho $n > '{c}'\nif [ $((n % 3)) -eq 0 ]; then printf B; else printf A; fi\nexit 0\n",
            c = counter.display()
        );
        std::fs::write(&bin, script).unwrap();
        let mut perms = std::fs::metadata(&bin).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).unwrap();

        let target_path = dir.join("prog.chz");
        std::fs::write(&target_path, b"# fake").unwrap();
        std::fs::write(target_path.with_extension("expected"), b"A").unwrap();
        let t = target_for(&target_path);

        let outcome = measure_baseline(&bin, &t, 1, Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&dir);
        match outcome {
            BaselineOutcome::Stable(b) => assert!(
                !b.check_output,
                "a target whose unseeded runs disagree must skip the output check, not trust two lucky matches"
            ),
            other => panic!("expected Stable with check_output=false, got {other:?}"),
        }
    }

    #[test]
    fn known_target_matches_by_file_name_not_full_path() {
        let t = target_for(Path::new("/some/other/root/examples/try_recv.chz"));
        let k = known_target(&t).expect("try_recv.chz must be a known target");
        assert_eq!(k.row, "W15-4");
    }

    #[test]
    fn known_target_is_none_for_an_untracked_program() {
        let t = target_for(Path::new("examples/hello.chz"));
        assert!(
            known_target(&t).is_none(),
            "hello.chz must not match any known-target entry"
        );
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
