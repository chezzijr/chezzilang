//! TICKET-167 -- the seeded-scheduler oracle (`docs/future.md` §2b) does not exist yet. There is no
//! `CHEZZI_SCHED_SEED` env var anywhere in `src/` (confirmed by grep), so it is silently ignored: a
//! failing run gives no way to know, let alone replay, the schedule that produced the failure. Per
//! the ticket's part 1 requirement ("A failing run prints its seed, and rerunning with that seed
//! reproduces the failure"), the seed must appear in the failing run's own diagnostics.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("chezzi_sched_seed_{}_{}", std::process::id(), n));
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

/// A failing run under `CHEZZI_SCHED_SEED` must report the seed it used, so the failure can be
/// replayed. Today the env var is read nowhere in the engine, so nothing prints it.
#[test]
fn a_failing_run_under_sched_seed_reports_its_seed() {
    let t = TmpDir::new();
    let entry = t.write("main.chz", "panic(\"boom\")\n");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .env("CHEZZI_SCHED_SEED", "12345")
        .arg("run")
        .arg(&entry)
        .output()
        .expect("spawn chezzi");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("12345"),
        "a failing run under CHEZZI_SCHED_SEED=12345 must report the seed \
         somewhere in its output, so the failure can be replayed; got {combined:?}"
    );
}

/// One plain helper: run `chezzi run <path>` with an optional seed and worker count, returning
/// `(stdout, stderr, exit code)`. Not a clock read, not a sleep — `tests/no_wall_clock_ratio_gates.rs`
/// scans this file's `#[test]` bodies for both.
fn run_chezzi(path: &std::path::Path, seed: Option<u64>, threads: usize) -> (String, String, i32) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run")
        .arg(path)
        .env("CHEZZI_THREADS", threads.to_string());
    match seed {
        Some(s) => {
            cmd.env("CHEZZI_SCHED_SEED", s.to_string());
        }
        None => {
            cmd.env_remove("CHEZZI_SCHED_SEED");
        }
    }
    let out = cmd.output().expect("spawn chezzi");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/sched_seed")
        .join(name)
}

/// A passing run's stdout/stderr are byte-identical whether or not `CHEZZI_SCHED_SEED` is set: the
/// seeded picks reorder *which* runnable fiber goes next, they never change what a single-fiber
/// program prints.
#[test]
fn a_passing_run_under_sched_seed_changes_no_output() {
    let t = TmpDir::new();
    let entry = t.write("main.chz", "print(\"hi\")\n");
    let (out1, err1, rc1) = run_chezzi(&entry, None, 1);
    let (out2, err2, rc2) = run_chezzi(&entry, Some(7), 1);
    assert_eq!(rc1, 0, "unseeded run should pass: {err1}");
    assert_eq!(rc2, 0, "seeded run should pass: {err2}");
    assert_eq!(out1, out2);
    assert_eq!(err1, err2);
}

/// Part 1: replay at `CHEZZI_THREADS=1`, held to a MEASURED rate, not byte-for-byte.
///
/// Byte-for-byte replay is not reachable on today's engine. A top-level `parallel:` body runs on the
/// main thread while its `chezzi-eager` drainer runs the spawned fibers, and nothing gates the two:
/// at `CHEZZI_THREADS=1` a body and a task that both burn CPU measured 195% CPU on the base binary
/// (TICKET-167 `## Thread`, `docs/gaps.md`). So the fixture nests its fan-out inside ONE spawned task:
/// the inner nursery is fiber-owned and runs on the drainer alone. A residual race near the start
/// remains. Measured on the debug binary, 8 seeds x 20 runs: the modal output per seed appeared in
/// 144 of 160 runs idle and 143 of 160 under load (per-seed minimum 16 of 20). The flat
/// `interleave.chz` measured 86 of 160, so this test goes red if a draw reads OS time or a racing
/// thread's stream, or if the fixture loses its nesting. Raise the bar to byte-for-byte when the
/// `docs/gaps.md` two-runner row is fixed.
#[test]
fn the_same_seed_replays_at_one_worker_at_the_measured_rate() {
    const RUNS: usize = 10;
    const MIN_PER_SEED: usize = 4;
    const MIN_TOTAL: usize = 60;
    let prog = fixture("nested_interleave.chz");
    let mut total = 0;
    let mut report = Vec::new();
    for seed in 1..=8u64 {
        let mut counts: std::collections::HashMap<String, usize> = Default::default();
        for _ in 0..RUNS {
            let (out, err, rc) = run_chezzi(&prog, Some(seed), 1);
            assert_eq!(rc, 0, "seed {seed} should pass: {err}");
            *counts.entry(out).or_default() += 1;
        }
        let modal = counts.values().copied().max().unwrap_or(0);
        assert!(
            modal >= MIN_PER_SEED,
            "seed {seed} replayed its modal output in only {modal} of {RUNS} runs at T=1: {counts:?}"
        );
        total += modal;
        report.push((seed, modal));
    }
    assert!(
        total >= MIN_TOTAL,
        "seeded T=1 replay rate fell to {total} of {} runs (need {MIN_TOTAL}); per seed: {report:?}",
        8 * RUNS
    );
}

/// Part 1: the seed must actually drive the schedule, not be ignored. Unseeded T=1 is FIFO (measured
/// in `## Digest`); with the seed wired in, different seeds must produce at least two distinct
/// schedules, and at least one must differ from the FIFO order.
#[test]
fn different_seeds_drive_different_schedules_at_one_worker() {
    const FIFO: &str = "t0.0 t0.1 t0.2 t1.0 t1.1 t1.2 t2.0 t2.1 t2.2 t3.0 t3.1 t3.2\n";
    let prog = fixture("interleave.chz");
    let mut outs = std::collections::HashSet::new();
    for seed in 1..=16u64 {
        let (out, err, rc) = run_chezzi(&prog, Some(seed), 1);
        assert_eq!(rc, 0, "seed {seed} should pass: {err}");
        outs.insert(out);
    }
    assert!(
        outs.len() >= 2,
        "16 seeds at T=1 produced only one distinct schedule: {outs:?}"
    );
    assert!(
        outs.iter().any(|o| o != FIFO),
        "every seeded schedule matched the unseeded FIFO order: {outs:?}"
    );
}

/// An invalid seed warns on stderr and the run still executes, unseeded.
#[test]
fn an_invalid_sched_seed_warns_and_runs_unseeded() {
    let t = TmpDir::new();
    let entry = t.write("main.chz", "print(\"hi\")\n");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run").arg(&entry).env("CHEZZI_SCHED_SEED", "abc");
    let out = cmd.output().expect("spawn chezzi");
    assert_eq!(out.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ignoring invalid CHEZZI_SCHED_SEED='abc'"),
        "got stderr {stderr:?}"
    );
}
