//! TICKET-167 -- the seeded-scheduler oracle (`docs/future.md` §2b) does not exist yet. There is no
//! `CHEZZI_SCHED_SEED` env var anywhere in `src/` (confirmed by grep), so it is silently ignored: a
//! failing run gives no way to know, let alone replay, the schedule that produced the failure. Per
//! the ticket's part 1 requirement ("A failing run prints its seed, and rerunning with that seed
//! reproduces the failure"), the seed must appear in the failing run's own diagnostics.

#[path = "../src/difftest/mod.rs"]
mod difftest;
#[path = "../src/schedfuzz/mod.rs"]
mod schedfuzz;

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
    let bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_chezzi"));
    let target = schedfuzz::target_for(path);
    let cap = schedfuzz::run_target(
        &bin,
        &target,
        seed,
        threads,
        std::time::Duration::from_secs(20),
    )
    .expect("spawn chezzi");
    (
        cap.stdout_text().into_owned(),
        cap.stderr_text().into_owned(),
        cap.code.unwrap_or(-1),
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

/// Part 1: replay at `CHEZZI_THREADS=1` is byte-for-byte for every fixture below (TICKET-205).
///
/// One fixture per wait site that holds 80 of 80 on an oversubscribed box (40 CPU hogs on 28
/// cores, debug and release): a fiber-owned nested fan-out, a flat fan-out, a body `recv` on a
/// rendezvous and on a bounded channel, a body `wait:`, and a `send` inside a native callback. This
/// test goes red when a gated thread picks or draws from the seeded RNG without the runner permit,
/// when an in-place channel wait stops listing its slot (`Vm::gated_register`), or when a channel
/// wake bypasses `ChannelCore::wake_all`. `docs/gaps.md` **W15-10** lists the waits that are NOT
/// byte-for-byte yet; their fixtures are in `tests/sched_seed/open/`, which no test reads. A `recv`
/// and a `wait:` inside a native callback are there since TICKET-206: 80 of 80 on an idle box, a
/// measured rate under 40 hogs. Both Executor fixtures (`executor_join.chz`,
/// `executor_interleave.chz`) stay in `open/` with a measured rate after TICKET-208 and TICKET-213
/// (join 72 of 80, job start one differing run in 480, under 40 hogs). The nursery join and the
/// `Shared` guard wait inside a native callback (`nursery_join_in_callback.chz`,
/// `guard_wait_in_callback.chz`) stay there too: by owner decision 2026-10-05 a wait joins
/// `FIXTURES` only when a fix makes it 80 of 80 under 40 hogs AND costs nothing unseeded
/// (TICKET-209 measured neither fix free).
#[test]
fn the_same_seed_replays_byte_for_byte_at_one_worker() {
    const RUNS: usize = 10;
    const FIXTURES: [&str; 6] = [
        "nested_interleave.chz",
        "interleave.chz",
        "body_recv_interleave.chz",
        "body_recv_bounded_interleave.chz",
        "body_wait_interleave.chz",
        "callback_send_interleave.chz",
    ];
    let mut red = Vec::new();
    for name in FIXTURES {
        let prog = fixture(name);
        for seed in 1..=8u64 {
            let mut counts: std::collections::HashMap<String, usize> = Default::default();
            for _ in 0..RUNS {
                let (out, err, rc) = run_chezzi(&prog, Some(seed), 1);
                assert_eq!(rc, 0, "seed {seed} on {name} should pass: {err}");
                *counts.entry(out).or_default() += 1;
            }
            if counts.len() != 1 {
                red.push(format!(
                    "seed {seed} on {name} gave {} distinct outputs at T=1: {counts:?}",
                    counts.len()
                ));
            }
        }
    }
    assert!(
        red.is_empty(),
        "seeded T=1 replay is not byte-for-byte:\n{}",
        red.join("\n")
    );
}

/// TICKET-206: a `send` inside a native callback replays byte-for-byte at one worker even when the
/// whole process is pinned to one core (`taskset -c 3`). A callback thread must queue its own next
/// turn before it releases the permit (`Vm::slice_end_in_place`), and a gated worker likewise
/// (`Vm::mn_worker_loop`); otherwise the order depends on how long the bookkeeping between the
/// release and the re-queue takes, which on one core is a scheduling accident.
#[test]
fn callback_send_replays_byte_for_byte_pinned_to_one_core() {
    const RUNS: usize = 10;
    let prog = fixture("callback_send_interleave.chz");
    let mut red = Vec::new();
    for seed in 1..=8u64 {
        let mut counts: std::collections::HashMap<String, usize> = Default::default();
        for _ in 0..RUNS {
            let out = Command::new("taskset")
                .args(["-c", "3"])
                .arg(env!("CARGO_BIN_EXE_chezzi"))
                .arg("run")
                .arg(&prog)
                .env("CHEZZI_SCHED_SEED", seed.to_string())
                .env("CHEZZI_THREADS", "1")
                .output()
                .expect("spawn taskset chezzi");
            assert!(
                out.status.success(),
                "seed {seed} pinned to one core should pass: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            *counts
                .entry(String::from_utf8_lossy(&out.stdout).into_owned())
                .or_default() += 1;
        }
        if counts.len() != 1 {
            red.push(format!(
                "seed {seed} pinned to one core gave {} distinct outputs at T=1: {counts:?}",
                counts.len()
            ));
        }
    }
    assert!(
        red.is_empty(),
        "seeded T=1 replay pinned to one core is not byte-for-byte:\n{}",
        red.join("\n")
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

/// Seeds this smoke test tries at `CHEZZI_THREADS=1`. Widen this range (never narrow it) if a
/// mutant that should hang stops reproducing within it -- see `## Rollback` fallback step 1.
const SMOKE_SEEDS: std::ops::RangeInclusive<u64> = 1..=64;

/// The two-leaf nested-nursery deadlock always faults at `CHEZZI_THREADS=1`, for every smoke
/// seed. This asserts a FAULT, not an output order, so the `docs/gaps.md` W15-10 residual replay
/// races do not affect it (there is no top-level `parallel:` racing a drainer here -- the whole
/// program is one fiber tree under the eager nursery). Used by step 4 to prove the gate goes red on
/// a reverted fix: the mutant must hang instead of faulting, so the seed's assertion catches it.
#[test]
fn sched_seed_smoke_two_leaf_always_faults_at_one_worker() {
    let bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_chezzi"));
    let prog = fixture("two_leaf_deadlock.chz");
    let target = schedfuzz::target_for(&prog);
    for seed in SMOKE_SEEDS {
        let cap = schedfuzz::run_target(
            &bin,
            &target,
            Some(seed),
            1,
            std::time::Duration::from_secs(20),
        )
        .unwrap_or_else(|e| panic!("seed {seed} did not run: {e:?}"));
        assert_ne!(
            cap.code,
            Some(0),
            "seed {seed} should fault, got rc={:?} stdout={:?}",
            cap.code,
            cap.stdout_text()
        );
        assert!(
            cap.stderr_text().contains("deadlock"),
            "seed {seed} should report a deadlock on stderr; got {:?}",
            cap.stderr_text()
        );
    }
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

/// TICKET-176 / W15-3 -- task B `close()`s a socket while task A's 16 MiB `write` is parked for
/// buffer room. The parked write must return `Err("write on a closed socket")`, never `Ok`. At
/// T=1 the seeded scheduler reaches the losing order on most seeds, so run seeds 1..16 and demand
/// zero failures.
#[test]
fn closing_a_socket_fails_a_parked_write_at_every_seed_one_worker() {
    let bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_chezzi"));
    let target = schedfuzz::target_for(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/chz/stdlib/net_close_test.chz"),
    );
    let mut failing = Vec::new();
    for seed in 1..=16u64 {
        match schedfuzz::run_target(
            &bin,
            &target,
            Some(seed),
            1,
            std::time::Duration::from_secs(60),
        ) {
            Ok(c) if c.code == Some(0) => {}
            Ok(c) => {
                let text = format!("{}{}", c.stdout_text(), c.stderr_text());
                let line = text
                    .lines()
                    .find(|l| l.contains("returned Ok"))
                    .unwrap_or("")
                    .to_string();
                failing.push(format!("seed {seed}: rc={:?} {line}", c.code));
            }
            Err(e) => failing.push(format!("seed {seed}: {e:?}")),
        }
    }
    assert!(
        failing.is_empty(),
        "parked write survived close() at T=1: {} of 16 seeds failed: {}",
        failing.len(),
        failing.join(" | ")
    );
}

/// TICKET-176 / W15-6 -- a generator driven from a spawned task, iterating a channel, must complete
/// at the default worker count. Seeds 5 and 7 hung (rc=124 at 30 s) on 2026-09-28.
/// Root cause: the deadlock veto ignored a closed channel under a demoted recv; the deterministic
/// pins are the `demoted_` veto unit tests in `src/vm/tests.rs`.
#[test]
fn a_generator_over_a_channel_from_a_task_completes_at_seeds_5_and_7_default_workers() {
    let bin = std::path::PathBuf::from(env!("CARGO_BIN_EXE_chezzi"));
    let target = schedfuzz::target_for(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/chz/spec/generator_channel_test.chz"),
    );
    let mut failing = Vec::new();
    for seed in [5u64, 7] {
        match schedfuzz::run_target(
            &bin,
            &target,
            Some(seed),
            0,
            std::time::Duration::from_secs(30),
        ) {
            Ok(c) if c.code == Some(0) => {}
            Ok(c) => failing.push(format!("seed {seed}: rc={:?}", c.code)),
            Err(e) => failing.push(format!("seed {seed}: {e:?}")),
        }
    }
    assert!(
        failing.is_empty(),
        "generator-over-channel did not complete at T=0: {}",
        failing.join("; ")
    );
}
