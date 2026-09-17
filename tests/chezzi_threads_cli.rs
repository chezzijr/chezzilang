//! Task 5 (continued) — the second-worker-count differential gate for `tests/chz`
//! (`docs/bug-discovery.md` Tier 2; `--serial` is gone, so "run the suite at two worker counts" is
//! the standing differential in its place, over the M:N engine's only remaining knob:
//! `CHEZZI_THREADS`).
//!
//! **Lives in `tests/`, driving the built binary — not `test_runner::run_tests` in-process.**
//! `vm::pool` is ONE process-wide `OnceLock`, sized to `vm::worker_count()` exactly once, lazily, on
//! first use, for the life of the process (`src/vm/pool.rs`) — nothing can resize it afterward. Under
//! `cargo test --lib`, many tests run concurrently against that ONE shared pool; forcing a count from
//! inside a single test either does nothing (another test already created the pool at a different
//! size) or, if it happens to run first, permanently pins the WHOLE test binary's pool to that count
//! for the rest of the run. Measured (task-5b brief): whole-process `CHEZZI_THREADS=2 cargo test`
//! under `RUST_TEST_THREADS=4` starved 4 concurrently-running tests contending for 2 pool workers (8
//! failures/hangs — exactly the tests then annotated "needs ≥2 free pool threads", pool risk G3,
//! `docs/gaps.md` W7-12r; TICKET-052 closed that residual, and those annotations are retired).
//! `RUST_TEST_THREADS=1` took >54 minutes without finishing. A subprocess
//! gets its own process, so its own freshly-sized pool — same reason `executor_reentrant_shutdown.rs`
//! / `executor_results_not_retained.rs` already run the built binary instead of calling in-process.
//!
//! **This differential is over the ~550 Chezzi behavioural tests in `tests/chz`, not the ~4150 Rust
//! lib tests** — the lib suite has no such gate (measured above: starves, or is impractically slow at
//! `RUST_TEST_THREADS=1`). It is NOT `docs/future.md` §2b's Go-paired-programs differential and NOT a
//! seeded/interleaving M:N mode; both remain unbuilt and separately planned.
//!
//! **`chezzi test` did not honor `CHEZZI_THREADS` at all before this task** — only `cmd_run` read it;
//! `cmd_test` never called `vm::set_worker_count`, so a `CHEZZI_THREADS=2 chezzi test` differential
//! was a silent no-op (both runs used the same auto-sized pool). `test_runner.rs`'s
//! `over_memory_trips_on_an_all_native_task_body` test already documented this exact gap ("the env
//! var is read by `main::cmd_run`, not by `run_tests_capped`"). `main::apply_env_worker_count` closes
//! it for `test` too — `chezzi_test_cli_honors_chezzi_threads_via_a_two_worker_precondition` below is
//! the black-box proof that it actually reaches the pool through the CLI `test` path, not merely
//! `worker_count()` in the lib test binary (which `vm::tests::chezzi_threads_env_reaches_worker_count`
//! already covers separately).

use std::path::Path;
use std::process::Command;

#[cfg(unix)]
#[path = "support/child_rusage.rs"]
mod child_rusage;

/// W8-8's serialization gates (TICKET-059) bound child CPU against child WALL on ONE run, rather than
/// dividing two wall-clock samples from two separate runs: at `CHEZZI_THREADS=1` a single runner
/// cannot exceed 1.00 cores whatever else the machine is doing, so contention can only lower this
/// measure, never raise it past the ceiling. The headroom above 1.00 is deliberate — see the tests'
/// own doc for the measured bands that set it.
#[cfg(unix)]
const MAX_CORES_AT_ONE_WORKER: f64 = 1.20;
/// Three runs per fixture, failing if ANY run exceeds [`MAX_CORES_AT_ONE_WORKER`] — raises detection
/// without averaging away a transient second runner.
#[cfg(unix)]
const SERIALIZATION_RUNS: usize = 3;

/// Run `chezzi test <path>`, optionally forcing `CHEZZI_THREADS`, with an optional `--timeout=N`ms
/// bound (so a genuine "needs more workers than we gave it" hang can't wedge the test binary).
/// Returns `(exit_success, summary_line, full_stdout, stderr)`.
fn run_chz_test(
    path: &Path,
    threads: Option<&str>,
    timeout_ms: Option<u64>,
) -> (bool, String, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("test");
    if let Some(ms) = timeout_ms {
        cmd.arg(format!("--timeout={ms}"));
    }
    cmd.arg(path);
    match threads {
        Some(n) => {
            cmd.env("CHEZZI_THREADS", n);
        }
        None => {
            cmd.env_remove("CHEZZI_THREADS");
        }
    }
    let out = cmd.output().expect("run chezzi test");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let summary = stdout
        .lines()
        .find(|l| l.contains("test(s):"))
        .unwrap_or("<no summary line found>")
        .trim()
        .to_string();
    (out.status.success(), summary, stdout, stderr)
}

/// The differential itself: `tests/chz` must pass identically at the default (auto-sized) worker
/// count and at `CHEZZI_THREADS=2`. ~550 real behavioural assertions, run twice, each in its own
/// process/pool — the standing second-schedule gate now that `--serial` is gone.
#[test]
fn chz_suite_passes_at_a_second_worker_count() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/chz");

    let (ok_default, summary_default, out_default, err_default) = run_chz_test(&root, None, None);
    assert!(
        ok_default,
        "tests/chz must pass at the default worker count\nsummary: {summary_default}\nstderr: {err_default}\nFAIL/ERROR line(s):\n{}",
        fail_lines(&out_default)
    );
    assert!(
        summary_default.contains(" passed, 0 failed, 0 errored"),
        "default run must be all-passing: {summary_default}"
    );

    let (ok_2, summary_2, out_2, err_2) = run_chz_test(&root, Some("2"), None);
    assert!(
        ok_2,
        "tests/chz must pass with CHEZZI_THREADS=2\nsummary: {summary_2}\nstderr: {err_2}\nFAIL/ERROR line(s):\n{}",
        fail_lines(&out_2)
    );
    assert!(
        summary_2.contains(" passed, 0 failed, 0 errored"),
        "CHEZZI_THREADS=2 run must be all-passing: {summary_2}"
    );

    // Same suite discovered both times (apples-to-apples): the total test count in the summary must
    // match, so a broken/short-circuited second run can't silently "pass" by running fewer tests.
    assert_eq!(
        summary_default, summary_2,
        "the two worker counts must produce the identical pass tally over the same discovered suite"
    );
    // A sanity floor: catches a `path`/discovery regression that quietly ran zero tests and "passed"
    // vacuously (`summary_default == summary_2` alone can't tell "0 == 0" from "550 == 550").
    assert!(
        !summary_default.starts_with("0 test(s)"),
        "the suite must not be empty: {summary_default}"
    );
}

/// The causal proof that `CHEZZI_THREADS` actually reaches the pool through the `chezzi test` CLI
/// path specifically (not merely `vm::worker_count()` in the lib test binary). TICKET-052 made a
/// BLOCKED pool thread yield its slot to a replacement, so a shape that starves on a blocking wait
/// (the channel-close precondition this test used before TICKET-052) no longer starves — it now
/// dispatches the closer through the replacement worker at every count, including 1. A CPU SPIN never
/// blocks, so it never yields a slot: one job spins on a flag it can only see change from a second
/// job, and only a second POOL THREAD — not a replacement, since nothing here ever blocks — can run
/// that second job. So:
/// - at 1 worker, the setter can never be dispatched → genuine hang (bounded here by `--timeout`, so
///   this test cannot itself wedge the runner);
/// - at ≥2 workers, the setter runs on the second thread, flips the flag, and the spinning job's loop
///   exits — fast (measured: single-digit ms).
///
/// A dropped/no-op env read would make ALL THREE runs behave like the default (>=2 cores on any CI
/// box) — i.e. all three would pass fast, none would time out. Seeing the 1-worker run actually time
/// out is the proof the knob has power, not just that something passed twice.
#[test]
fn chezzi_test_cli_honors_chezzi_threads_via_a_two_worker_precondition() {
    let dir = std::env::temp_dir().join(format!("chz-threads-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("needs_two_workers_test.chz");
    std::fs::write(
        &path,
        "import std.concurrency\n\n\
         test fn needs_two_workers():\n    \
         flag := AtomicInt(0)\n    \
         fn waiter():\n        \
         while flag.load() == 0:\n            \
         pass\n    \
         fn setter():\n        \
         flag.store(1)\n    \
         ex := Executor()\n    \
         ex.submit(waiter)\n    \
         ex.submit(setter)\n    \
         ex.shutdown()\n    \
         assert true\n",
    )
    .expect("write program");

    // Default (auto — >=2 workers on any real box): the setter gets its own pool thread and flips
    // the flag; the spinning waiter observes it and returns. Bounded to 5s as a smoke guard, not
    // because this run is expected to need it.
    let (_, summary, out, _) = run_chz_test(&path, None, Some(5_000));
    assert!(
        summary.contains(" passed, 0 failed, 0 errored"),
        "default worker count should pass fast on the CPU-spin precondition, not hang: {summary}\n{out}"
    );

    // CHEZZI_THREADS=2: same shape, explicit count instead of auto.
    let (_, summary, out, _) = run_chz_test(&path, Some("2"), Some(5_000));
    assert!(
        summary.contains(" passed, 0 failed, 0 errored"),
        "CHEZZI_THREADS=2 should pass fast on the CPU-spin precondition, not hang: {summary}\n{out}"
    );

    // CHEZZI_THREADS=1: the setter can never be dispatched — this must TIME OUT, not pass. If it
    // instead passes fast, the env var never reached the pool.
    let (_, summary, out, _) = run_chz_test(&path, Some("1"), Some(2_000));
    assert!(
        summary.contains("1 timed out"),
        "CHEZZI_THREADS=1 must starve the two-worker CPU-spin precondition and TIME OUT — a pass \
         here means CHEZZI_THREADS did not reach chezzi test's pool: {summary}\n{out}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// W8-8 — `--threads=1` (via `CHEZZI_THREADS=1`) must run exactly ONE CPU runner, not two. Before the
/// fix the inline joiner ran a fiber loop ALONGSIDE the unconditional `chezzi-eager` drainer thread
/// even at a budget of 1, so an 8x CPU workload only ran ~4.4x slower than a 1x workload (two runners
/// splitting the work) instead of ~8x. Program B runs eight copies of `burn(75000)` in one `parallel:`
/// nursery under `CHEZZI_THREADS=1`; each run measures child CPU time (`user + sys` via `wait4`
/// rusage) against child wall time, asserting `cpu <= wall * MAX_CORES_AT_ONE_WORKER` — a single
/// `--threads=1` runner cannot exceed 1.00 cores, so a healthy binary can never trip this, while a
/// second CPU runner (the pre-fix defect) pushes `cpu` well past `wall`. Measured under load average
/// 28.9 on a 12-core box, four runs each: fixed 0.776-0.946 cores, pre-fix-reverted 1.267-1.771 cores.
/// The burn size (75k iterations) is what every measured band and the negative control below were
/// taken at — `CARGO_BIN_EXE_chezzi` under `cargo test` is the debug binary, not `--release`.
#[cfg(unix)]
#[test]
fn threads_one_serializes_cpu_bound_parallel_tasks() {
    let dir = std::env::temp_dir().join(format!("chz-threads-w8-8-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");

    let burn = "fn burn(n: int) -> int:\n    \
                 x := 0\n    \
                 i := 0\n    \
                 while i < n:\n        \
                 x = x + i * i - i\n        \
                 i += 1\n    \
                 return x\n\n";

    let path_b = dir.join("burn_eight.chz");
    let spawns = "        spawn: burn(75000)\n".repeat(8);
    std::fs::write(
        &path_b,
        format!("{burn}fn main():\n    parallel:\n{spawns}main()\n"),
    )
    .expect("write program B");

    for run in 0..SERIALIZATION_RUNS {
        let (wall, user, sys, status, stdout) =
            child_rusage::run_timed(&["run", path_b.to_str().unwrap()], "1");
        assert!(
            status.success(),
            "chezzi run {path_b:?} failed (run {run}): {stdout}"
        );
        let cpu = user + sys;
        // Negative control: cpu must be non-trivial, or a near-zero/near-zero ratio could pass by
        // accident. Measured 1.34-1.48 s.
        assert!(
            cpu > std::time::Duration::from_millis(500),
            "program B finished too fast (cpu={cpu:?}, run {run}) to be a meaningful measurement — \
             recalibrate the burn size"
        );
        assert!(
            cpu <= wall.mul_f64(MAX_CORES_AT_ONE_WORKER),
            "--threads=1 must run at most one CPU runner (run {run}): cpu={cpu:?} wall={wall:?} \
             (cpu must be <= wall * {MAX_CORES_AT_ONE_WORKER}). A second runner means the inline \
             joiner is running a second fiber loop alongside the drainer at a budget of 1 (W8-8)."
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// T1-fix — the same W8-8 defect on the NESTED eager-nursery arm: `fn work(): parallel: ...` **called
/// (not spawned)** from a top-level `parallel:` body, so `work()`'s `parallel:` runs synchronously on
/// `main` while the outer scope's body is still open — `activate_eager_nursery`'s
/// `self.mn.is_none() && an outer eager scope is open` branch (`src/vm/sched.rs:764`), which returns
/// `EagerScope { drainer: None, .. }` and relies entirely on the OUTER scope's `chezzi-eager` drainer.
/// (A `spawn: work()` does NOT reach this branch: the spawned fiber runs on a worker shell whose
/// `self.mn` is already `Some`, which takes the private-sched general path — already correctly gated.)
/// `join_eager_nursery`'s `drainer.is_none()` arm ran an unconditional
/// `shell.mn_worker_loop(&sched, 0, sid)` alongside the outer drainer — a second CPU runner at a
/// budget of one. Same construction and the same `cpu <= wall * MAX_CORES_AT_ONE_WORKER` check as
/// `threads_one_serializes_cpu_bound_parallel_tasks`, but nested: 16 spawns of `burn(75000)`. Measured
/// under load average 28.9 on a 12-core box, four runs each: fixed 0.687-0.855 cores, pre-fix-reverted
/// 1.131-1.697 cores. 16 spawns, not 8: at 8 one reverted run in eleven read 0.755 cores, inside the
/// healthy band, because the nested arm's second runner is the inline joiner, which covers less of the
/// run than the flat arm's pool helpers do.
#[cfg(unix)]
#[test]
fn threads_one_serializes_nested_eager_parallel_tasks() {
    let dir = std::env::temp_dir().join(format!("chz-threads-w8-8-nested-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");

    let burn = "fn burn(n: int) -> int:\n    \
                 x := 0\n    \
                 i := 0\n    \
                 while i < n:\n        \
                 x = x + i * i - i\n        \
                 i += 1\n    \
                 return x\n\n";

    // 16 spawns, not 8: measured, at 8 one reverted run in eleven read 0.755 cores -- inside the
    // healthy band, because the nested arm's second runner is the inline joiner, which covers less
    // of the run than the flat arm's pool helpers do. At 16 the reverted band is 1.131-1.697 with no
    // overlap.
    let path_b = dir.join("nested_burn_eight.chz");
    let spawns = "        spawn: burn(75000)\n".repeat(16);
    std::fs::write(
        &path_b,
        format!(
            "{burn}fn work():\n    parallel:\n{spawns}\n\
             fn main():\n    \
             parallel:\n        \
             work()\n\
             main()\n"
        ),
    )
    .expect("write program B");

    for run in 0..SERIALIZATION_RUNS {
        let (wall, user, sys, status, stdout) =
            child_rusage::run_timed(&["run", path_b.to_str().unwrap()], "1");
        assert!(
            status.success(),
            "chezzi run {path_b:?} failed (run {run}): {stdout}"
        );
        let cpu = user + sys;
        // Negative control. Measured 1.30-1.44 s at burn(75000)x16 on this box (re-measured
        // 2026-09-05; the plan's "2.81-2.94 s" figure was taken at the pre-review burn(150000)).
        assert!(
            cpu > std::time::Duration::from_millis(900),
            "program B finished too fast (cpu={cpu:?}, run {run}) to be a meaningful measurement — \
             recalibrate the burn size"
        );
        assert!(
            cpu <= wall.mul_f64(MAX_CORES_AT_ONE_WORKER),
            "--threads=1 must serialize a NESTED eager parallel: too (run {run}): cpu={cpu:?} \
             wall={wall:?} (cpu must be <= wall * {MAX_CORES_AT_ONE_WORKER}). A second runner means \
             the nested scope's inline join loop is STILL running alongside the outer scope's drainer \
             at a budget of 1 (W8-8, nested arm)."
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// W8-8 residual (TICKET-118, W13-7) — a CPU-bound nursery INSIDE an `Executor` job's `parallel:`
/// must not gain the joiner's pool slot as a second runner. `MnSched::joiner_step` gates the yield
/// on `running == 0 && runnable == 0`; if that term were dropped, the marked joiner would yield
/// while the job's own nursery is still burning CPU on the raw `chezzi-eager` drainer, handing the
/// replacement worker the queued sibling `burn` job to run beside it. Measured on the debug binary
/// (prototype, 2026-09-16): gated `real=2.902-2.951 user=2.897-2.951` (1.00 cores); with the
/// running/runnable term replaced by `false`, `real=1.509-1.615 user=2.945-3.016` (1.86-1.95 cores).
#[cfg(unix)]
#[test]
fn threads_one_serializes_a_cpu_bound_nursery_inside_an_executor_job() {
    let dir = std::env::temp_dir().join(format!("chz-w13-7-burn-job-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");

    let program = "import std.concurrency\n\
        fn burn(n: int) -> int:\n    \
            x := 0\n    \
            for i in range(n):\n        \
                x = (x + i * 7) % 1000003\n    \
            return x\n\
        fn job():\n    \
            parallel:\n        \
                spawn:\n            burn(75000)\n        \
                spawn:\n            burn(75000)\n        \
                spawn:\n            burn(75000)\n        \
                spawn:\n            burn(75000)\n        \
                spawn:\n            burn(75000)\n        \
                spawn:\n            burn(75000)\n        \
                spawn:\n            burn(75000)\n        \
                spawn:\n            burn(75000)\n\
        fn main():\n    \
            ex := Executor()\n    \
            ex.submit(fn(): job())\n    \
            ex.submit(fn(): burn(600000))\n    \
            ex.shutdown()\n    \
            print(\"done\")\n\
        main()\n";
    let path = dir.join("burn.chz");
    std::fs::write(&path, program).expect("write program");

    for run in 0..SERIALIZATION_RUNS {
        let (wall, user, sys, status, stdout) =
            child_rusage::run_timed(&["run", path.to_str().unwrap()], "1");
        assert!(
            status.success(),
            "chezzi run {path:?} failed (run {run}): {stdout}"
        );
        let cpu = user + sys;
        assert!(
            cpu > std::time::Duration::from_millis(900),
            "program finished too fast (cpu={cpu:?}, run {run}) to be a meaningful measurement — \
             recalibrate the burn size"
        );
        assert!(
            cpu <= wall.mul_f64(MAX_CORES_AT_ONE_WORKER),
            "--threads=1 must serialize a CPU-bound nursery inside an ex.submit job too (run {run}): \
             cpu={cpu:?} wall={wall:?} (cpu must be <= wall * {MAX_CORES_AT_ONE_WORKER}). A second \
             runner means the job's joiner handed its pool slot to a replacement that ran the queued \
             `burn` job beside the `chezzi-eager` drainer (TICKET-118, the idle term in \
             `MnSched::joiner_step`)."
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// C5's teeth: `std.cancel`'s "`done()` never fires before `cancelled()` flips" invariant is only
/// OBSERVABLE at a worker count the two standing `tests/chz` runs don't cover. `_mark` used to
/// `trip()` the done-channel before setting the cancel bit, so a task woken by a cascaded
/// descendant's `done()` could read `cancelled() == false`. Measured on the pre-fix release binary
/// with `cascaded_done_implies_cancelled`'s 100 rounds (root->mid->leaf, one task parked in
/// `wait: leaf.done().recv()`): FAILS 5/5 at `CHEZZI_THREADS=8`, 5/5 at `=4`, 5/5 at this host's
/// 12-core default — but **0/5 at `CHEZZI_THREADS=2`**. `chz_suite_passes` runs the default and
/// `chz_suite_passes_at_a_second_worker_count` runs `=2`, so on a 1-2 core CI box the default IS 2
/// and the gate would silently vanish. Pinning `=8` here makes it host-independent (oversubscription
/// only increases the preemption that exposes the race).
#[test]
fn cancel_c5_gate_at_eight_workers() {
    let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/chz/stdlib/cancel_test.chz");
    let (ok, summary, out, err) = run_chz_test(&file, Some("8"), Some(120_000));
    assert!(
        ok,
        "std.cancel's suite must pass at CHEZZI_THREADS=8 ({summary})\n--- stdout ---\n{}\n--- stderr ---\n{}",
        tail(&out),
        tail(&err)
    );
    // The gate is "the C5 test RAN at =8", not merely "the file passed". Without this, deleting or
    // renaming `cascaded_done_implies_cancelled` leaves this test green and meaningless — the whole
    // reason it exists is that `chz_suite_passes` only runs the default and =2, and =2 is
    // structurally blind to the race (measured 0/400 violations at =2 vs 146/400 at =8).
    assert!(
        out.contains("PASS cascaded_done_implies_cancelled"),
        "the C5 cascade test must actually run at CHEZZI_THREADS=8\n--- stdout ---\n{}",
        tail(&out)
    );
}

/// TICKET-073(a) — a `parallel:` reached from inside another `parallel:` **body** (called, not
/// spawned — same shape as `threads_one_serializes_nested_eager_parallel_tasks` above) must SCALE
/// with `--threads` the way the identical call does at top level. `activate_eager_nursery`'s nested
/// arm (`src/vm/sched.rs`, `self.mn.is_none() && an outer eager scope is open`) returns
/// `EagerScope { drainer: None, .. }`, and `join_eager_nursery`'s `drainer.is_none()` arm never farms
/// the bounded pool (only the outermost arm calls `farm_outermost_eager_helpers`) — so the nested
/// scope is served by the outer scope's ONE `chezzi-eager` drainer plus (at N>=2) the inline joiner:
/// two runners, always, regardless of `--threads`. 16 spawns of `burn(75000)` at
/// `CHEZZI_THREADS=8`: a flat (non-nested) version of this exact workload measures close to 8 cores;
/// this asserts the nested version reaches at least `MIN_CORES_AT_EIGHT_WORKERS_NESTED`, which is
/// comfortably below 8 but above the ~2.0 the bug caps it at.
#[cfg(unix)]
const MIN_CORES_AT_EIGHT_WORKERS_NESTED: f64 = 3.5;

#[cfg(unix)]
#[test]
fn threads_eight_scales_nested_eager_parallel_tasks_in_body() {
    let dir = std::env::temp_dir().join(format!(
        "chz-threads-073-nested-scale-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");

    let burn = "fn burn(n: int) -> int:\n    \
                 x := 0\n    \
                 i := 0\n    \
                 while i < n:\n        \
                 x = x + i * i - i\n        \
                 i += 1\n    \
                 return x\n\n";

    let path = dir.join("nested_burn_scale.chz");
    let spawns = "        spawn: burn(75000)\n".repeat(16);
    std::fs::write(
        &path,
        format!(
            "{burn}fn work():\n    parallel:\n{spawns}\n\
             fn main():\n    \
             parallel:\n        \
             work()\n\
             main()\n"
        ),
    )
    .expect("write program");

    for run in 0..SERIALIZATION_RUNS {
        let (wall, user, sys, status, stdout) =
            child_rusage::run_timed(&["run", path.to_str().unwrap()], "8");
        assert!(
            status.success(),
            "chezzi run {path:?} failed (run {run}): {stdout}"
        );
        let cpu = user + sys;
        assert!(
            cpu > std::time::Duration::from_millis(900),
            "program finished too fast (cpu={cpu:?}, run {run}) to be a meaningful measurement — \
             recalibrate the burn size"
        );
        // A multiplication, never a quotient of the CPU sample and the wall sample: dividing two
        // differently-sourced duration samples amplifies scheduler noise without bound, which
        // `no_rust_test_divides_two_wall_clock_samples` bans repo-wide (TICKET-049). This is the
        // same bound in the form the two W8-8 gates above already use, mirrored from an upper
        // bound to a lower one: `cpu <= wall * MAX_CORES_AT_ONE_WORKER` there, `cpu >= wall *
        // MIN_CORES_AT_EIGHT_WORKERS_NESTED` here.
        assert!(
            cpu >= wall.mul_f64(MIN_CORES_AT_EIGHT_WORKERS_NESTED),
            "a parallel: nested in a nursery BODY must scale with --threads (run {run}): \
             cpu={cpu:?} wall={wall:?}, expected cpu >= wall * \
             {MIN_CORES_AT_EIGHT_WORKERS_NESTED} at CHEZZI_THREADS=8. A nested eager join never \
             farms the bounded pool (only the outermost arm does), so it is pinned to the outer \
             drainer + inline joiner regardless of --threads (TICKET-073a)."
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// TICKET-073(b) — a `parallel:` nested inside a SPAWNED task (not called from a body) must keep live
/// OS threads bounded at `N + (joining threads)` per `src/vm/pool.rs:8`'s invariant, at every
/// `--threads` setting, not just `=1`. A spawned task runs on a worker shell whose `self.mn` is
/// already `Some`, so `activate_eager_nursery` takes the general (non-shared) path and builds a
/// brand-new private `MnSched` PLUS a dedicated `chezzi-eager` drainer thread for every nested
/// nursery — unbounded under a binary-tree fan-out. A depth-7 tree (128 sleeping leaves) is spawned
/// under `CHEZZI_THREADS=2`, and `/proc/<pid>/task` is polled while the process is alive; a healthy
/// binary stays near `N + few`, the bug reaches ~130.
#[cfg(target_os = "linux")]
#[test]
fn threads_stay_bounded_for_a_nursery_nested_in_a_spawned_task() {
    let dir = std::env::temp_dir().join(format!("chz-threads-073-leak-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");

    let program = "import std.time\n\n\
                    fn tree(d: int):\n    \
                    if d == 0:\n        \
                    time.sleep_ms(300)\n    \
                    else:\n        \
                    parallel:\n            \
                    spawn: tree(d - 1)\n            \
                    spawn: tree(d - 1)\n\n\
                    fn main():\n    \
                    tree(7)\n\
                    main()\n";
    let path = dir.join("thread_tree.chz");
    std::fs::write(&path, program).expect("write program");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", path.to_str().unwrap()])
        .env("CHEZZI_THREADS", "2")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn chezzi");
    let pid = child.id();

    let mut max_threads: usize = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/task")) {
            let n = entries.count();
            if n > max_threads {
                max_threads = n;
            }
        } else {
            break; // process exited
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let status = child.wait().expect("wait chezzi");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(status.success(), "chezzi run {path:?} failed");
    // Healthy bound: N (2) + a handful of joining threads, generously slack-ed to 20.
    const MAX_HEALTHY_THREADS: usize = 20;
    assert!(
        max_threads <= MAX_HEALTHY_THREADS,
        "live OS threads must stay bounded at N + (joining threads) per src/vm/pool.rs:8, \
         regardless of parallel: nesting depth (TICKET-073b): observed max_threads={max_threads} \
         at CHEZZI_THREADS=2, expected <= {MAX_HEALTHY_THREADS}"
    );
}

/// W13-3 (TICKET-117): the main body AND a nested owner body both channel-parked, leaf at depth 2.
const NESTED_BODY_BLOCKED: &str = "fn main():
    never := Channel[int](0)
    parallel:
        spawn:
            parallel:
                spawn:
                    never.recv()
                never.recv()
        never.recv()
    print(\"unreachable\")
main()
";

/// Control: the same nesting with the main body NOT blocked; faults in milliseconds on base.
const NESTED_OWNER_BLOCKED_ONLY: &str = "fn main():
    never := Channel[int](0)
    parallel:
        spawn:
            parallel:
                spawn:
                    never.recv()
                never.recv()
    print(\"unreachable\")
main()
";

/// Control: depth 3, the owner parked on a second `send` after one rendezvous; faults on base.
const NESTED_OWNER_SECOND_SEND: &str = "fn main():
    never := Channel[int](0)
    parallel:
        spawn:
            parallel:
                spawn:
                    parallel:
                        spawn:
                            never.recv()
                        never.send(1)
                        never.send(2)
    print(\"unreachable\")
main()
";

/// Control (W12-1): a LIVE nested rendezvous chain; it must complete, never hang or false-fault.
const NESTED_LIVE_RENDEZVOUS: &str = "fn main():
    out := Channel[int](0)
    parallel:
        spawn:
            inner := Channel[int](0)
            parallel:
                spawn:
                    inner.send(1)
                out.send(inner.recv() + 1)
        print(\"got {out.recv()}\")
main()
";

/// Control (TICKET-125 step 1, `cousin_fed.chz`): a recoverer feeds a cousin BY CHANNEL after its own
/// inner deadlock recovers. Must complete at every worker count.
const COUSIN_FED: &str = "fn main():
    never := Channel[int](0)
    x := Channel[int](0)
    y := Channel[int](0)
    parallel:
        spawn:
            r := recover:
                parallel:
                    spawn:
                        never.recv()
            match r:
                Ok(_): print(\"inner ok\")
                Err(e): print(\"inner err\")
            x.send(1)
        spawn:
            parallel:
                spawn:
                    y.send(x.recv() + 1)
                print(\"F got {y.recv()}\")
    print(\"done\")
main()
";

/// Control (TICKET-125 step 1, `i6b.chz`): TWO siblings each recover an inner deadlock, no fan-in.
/// Must complete at every worker count.
const TWO_RECOVERERS_NO_FAN_IN: &str = "fn main():
    parallel:
        for i in range(2):
            spawn:
                r := recover:
                    parallel:
                        spawn:
                            never := Channel[int](0)
                            never.recv()
                print(\"recovered {i}\")
    print(\"done\")
main()
";

const NESTED_DEADLOCK_RUNS: usize = 5;
/// TICKET-125 step 1 — `""` is the default worker count (`CHEZZI_THREADS` unset), added so every
/// caller of `assert_at_every_worker_count` also runs at the count most programs actually use.
const NESTED_DEADLOCK_THREADS: [&str; 4] = ["1", "2", "4", ""];

/// Runs `chezzi run <path>` at `CHEZZI_THREADS=<threads>`; `None` when it outlives a 20 s hang
/// deadline (the child is killed). The poll loop lives in this plain fn, not in a `#[test]` body, so
/// `tests/no_wall_clock_ratio_gates.rs`'s body scans list no new name (the `child_rusage` precedent).
/// TICKET-125 — an empty `threads` removes `CHEZZI_THREADS` instead of setting it, so the child runs
/// at the default worker count.
fn run_with_hang_deadline(path: &Path, threads: &str) -> Option<std::process::Output> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.args(["run", path.to_str().unwrap()]);
    if threads.is_empty() {
        cmd.env_remove("CHEZZI_THREADS");
    } else {
        cmd.env("CHEZZI_THREADS", threads);
    }
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if child.try_wait().expect("try_wait chezzi").is_some() {
            return Some(child.wait_with_output().expect("collect chezzi output"));
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn faulted_deadlock(out: &std::process::Output) -> bool {
    !out.status.success() && String::from_utf8_lossy(&out.stderr).contains("deadlock")
}

/// Writes `program` to a temp file and runs it [`NESTED_DEADLOCK_RUNS`] times at each of
/// [`NESTED_DEADLOCK_THREADS`], panicking on the first hang or the first run `check` rejects. Five
/// runs per count, because a ~5% flake reads as 0/1 on one run.
fn assert_at_every_worker_count(
    file: &str,
    program: &str,
    check: fn(&std::process::Output) -> bool,
    want: &str,
) {
    let dir = std::env::temp_dir().join(format!("chz-threads-117-{}-{file}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(file);
    std::fs::write(&path, program).expect("write program");
    for threads in NESTED_DEADLOCK_THREADS {
        for run in 1..=NESTED_DEADLOCK_RUNS {
            let Some(out) = run_with_hang_deadline(&path, threads) else {
                let _ = std::fs::remove_dir_all(&dir);
                panic!(
                    "{file} hung past its 20 s deadline at CHEZZI_THREADS={threads}, run {run}; want {want}"
                );
            };
            if !check(&out) {
                let _ = std::fs::remove_dir_all(&dir);
                panic!(
                    "{file} at CHEZZI_THREADS={threads}, run {run}: want {want}, got {:?}\nstdout: {}\nstderr: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// W13-3 (TICKET-117): a genuine nested deadlock whose main body is itself channel-parked must fault
/// `deadlock` at every worker count. Before the fix main's 5 ms `recv` poll woke the nested sched's
/// parked receiver with `WakeKind::All` on every tick, so no sched quiesced and T>=2 hung.
#[test]
fn nested_body_blocked_deadlock_faults_at_every_worker_count() {
    assert_at_every_worker_count(
        "nested_body_blocked.chz",
        NESTED_BODY_BLOCKED,
        faulted_deadlock,
        "a `deadlock` fault",
    );
}

/// TICKET-117's controls: the cap-0 wake-kind narrowing must not lose a verdict that already held
/// (the `b1`/`j2` hunt shapes) nor turn a live nested rendezvous into a hang (W12-1's `a1b` shape).
#[test]
fn nested_deadlock_controls_hold_at_every_worker_count() {
    assert_at_every_worker_count(
        "nested_owner_blocked_only.chz",
        NESTED_OWNER_BLOCKED_ONLY,
        faulted_deadlock,
        "a `deadlock` fault",
    );
    assert_at_every_worker_count(
        "nested_owner_second_send.chz",
        NESTED_OWNER_SECOND_SEND,
        faulted_deadlock,
        "a `deadlock` fault",
    );
    assert_at_every_worker_count(
        "nested_live_rendezvous.chz",
        NESTED_LIVE_RENDEZVOUS,
        |out| out.status.success() && String::from_utf8_lossy(&out.stdout) == "got 2\n",
        "exit 0 with stdout `got 2`",
    );
}

/// TICKET-125 step 1: a recoverer feeds a cousin by channel after its own inner deadlock recovers.
/// Must stay clean at every worker count, including the default this ticket adds.
#[test]
fn cousin_fed_completes_at_every_worker_count() {
    assert_at_every_worker_count(
        "cousin_fed.chz",
        COUSIN_FED,
        |out| {
            out.status.success()
                && String::from_utf8_lossy(&out.stdout) == "inner err\nF got 2\ndone\n"
        },
        "exit 0 with stdout `inner err`, `F got 2`, `done`",
    );
}

/// TICKET-125 step 1: two siblings each recover an inner deadlock, no fan-in. Must stay clean at
/// every worker count.
#[test]
fn i6b_two_recoverers_without_fan_in_complete_at_every_worker_count() {
    assert_at_every_worker_count(
        "i6b.chz",
        TWO_RECOVERERS_NO_FAN_IN,
        |out| {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let lines: Vec<&str> = stdout.lines().collect();
            out.status.success()
                && lines.len() == 3
                && lines[2] == "done"
                && std::collections::BTreeSet::from_iter(lines[..2].iter().copied())
                    == std::collections::BTreeSet::from_iter(["recovered 0", "recovered 1"])
        },
        "exit 0 with `recovered 0`/`recovered 1` (either order) then `done`",
    );
}

/// W13-4 (TICKET-125, split from TICKET-117): the ONLY channel-parked owner sits at depth 3. Must
/// fault `deadlock` at every worker count; hangs instead at `CHEZZI_THREADS=1`.
const CHANNEL_PARKED_OWNER_DEPTH_3: &str = "fn main():
    never := Channel[int](0)
    parallel:
        spawn:
            parallel:
                spawn:
                    parallel:
                        spawn:
                            never.recv()
                        never.recv()
    print(\"unreachable\")
main()
";

/// W13-5 (TICKET-125): a task recovers a genuine inner-nursery deadlock, then sends to a cousin whose
/// owner sits at its join with a child doing `recv()`. Must print `err`, `cousin got 5`, `done` and
/// exit 0; false-faults `deadlock` instead.
const RECOVERED_DEADLOCK_THEN_COUSIN_JOIN: &str = "fn main():
    out := Channel[int](0)
    parallel:
        spawn:
            r := recover:
                parallel:
                    spawn:
                        never := Channel[int](0)
                        never.recv()
            match r:
                Ok(_): print(\"ok\")
                Err(e): print(\"err\")
            out.send(5)
        spawn:
            parallel:
                spawn:
                    print(\"cousin got {out.recv()}\")
    print(\"done\")
main()
";

/// W13-5 (TICKET-125, `d2d.chz`): the SAME shape as [`RECOVERED_DEADLOCK_THEN_COUSIN_JOIN`] with the
/// two roles swapped (the recoverer, not the cousin, sits at a nested join). Must print `err`,
/// `task got 5`, `done` and exit 0.
const RECOVERED_DEADLOCK_ROLES_SWAPPED: &str = "fn main():
    out := Channel[int](0)
    parallel:
        spawn:
            parallel:
                spawn:
                    r := recover:
                        parallel:
                            spawn:
                                never := Channel[int](0)
                                never.recv()
                    match r:
                        Ok(_): print(\"ok\")
                        Err(e): print(\"err\")
                    print(\"task got {out.recv()}\")
        spawn:
            parallel:
                spawn:
                    out.send(5)
    print(\"done\")
main()
";

/// W13-5 (TICKET-125, `g3.chz`): the recoverer catches a `panic(\"boom\")` instead of an inner
/// deadlock, then still feeds the cousin. Must print `err`, `cousin got 5`, `done` and exit 0.
const RECOVERED_PANIC_THEN_COUSIN_JOIN: &str = "fn main():
    out := Channel[int](0)
    parallel:
        spawn:
            r := recover:
                parallel:
                    spawn:
                        panic(\"boom\")
            match r:
                Ok(_): print(\"ok\")
                Err(e): print(\"err\")
            out.send(5)
        spawn:
            parallel:
                spawn:
                    print(\"cousin got {out.recv()}\")
    print(\"done\")
main()
";

/// Runs `program` [`NESTED_DEADLOCK_RUNS`] times at `CHEZZI_THREADS=1`, asserting exit 0 with the
/// exact `want_stdout`.
fn assert_clean_sampled_at_thread_one(file: &str, program: &str, want_stdout: &str) {
    let dir = std::env::temp_dir().join(format!(
        "chz-threads-125-w135s-{}-{file}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(file);
    std::fs::write(&path, program).expect("write program");
    for run in 1..=NESTED_DEADLOCK_RUNS {
        let Some(out) = run_with_hang_deadline(&path, "1") else {
            let _ = std::fs::remove_dir_all(&dir);
            panic!("{file} hung past its 20 s deadline at CHEZZI_THREADS=1, run {run}");
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        if !(out.status.success() && stdout == want_stdout) {
            let _ = std::fs::remove_dir_all(&dir);
            panic!(
                "{file} at CHEZZI_THREADS=1, run {run}: want exit 0 with stdout `{want_stdout}`, got {:?}\nstdout: {stdout}\nstderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// W13-5 (TICKET-125): the cousin-join shape must stay clean, sampled, at `CHEZZI_THREADS=1`.
#[test]
fn w13_5_d2a_cousin_join_completes_at_thread_one_sampled() {
    assert_clean_sampled_at_thread_one(
        "d2a_sampled.chz",
        RECOVERED_DEADLOCK_THEN_COUSIN_JOIN,
        "err\ncousin got 5\ndone\n",
    );
}

/// W13-5 (TICKET-125): the roles-swapped shape must stay clean, sampled, at `CHEZZI_THREADS=1`.
#[test]
fn w13_5_d2d_roles_swapped_completes_at_thread_one_sampled() {
    assert_clean_sampled_at_thread_one(
        "d2d_sampled.chz",
        RECOVERED_DEADLOCK_ROLES_SWAPPED,
        "err\ntask got 5\ndone\n",
    );
}

/// W13-5 (TICKET-125): the recovered-`panic` shape must stay clean, sampled, at `CHEZZI_THREADS=1`.
#[test]
fn w13_5_g3_recovered_panic_then_cousin_join_completes_at_thread_one_sampled() {
    assert_clean_sampled_at_thread_one(
        "g3_sampled.chz",
        RECOVERED_PANIC_THEN_COUSIN_JOIN,
        "err\ncousin got 5\ndone\n",
    );
}

/// W13-5 (TICKET-125) residual, T>=2 — CLOSED by TICKET-129. `SchedCore::flag_deadlock_leaves` now
/// DECLINES an unproven verdict (`unproven_ok: bool`, `Option<bool>` return) unless
/// `MnSched::may_fault_unproven` licenses it — no live peer sched can still move or prove its own
/// victims, matching Go's all-goroutines-parked rule. TICKET-125's own cross-sched deferral
/// (`defer_to_provable_peer`) had cut the false-fault rate (~3/5 -> ~1/20 at T=4 on `d2a`) without
/// reaching 0 and was reverted; TICKET-129 measured 0 false faults of 60 runs per worker count on
/// `d2a`/`d2d`/`g3` (debug binary) before closing `docs/gaps.md`'s W13-5 row.
#[test]
fn w13_5_d2a_cousin_join_completes_at_every_worker_count() {
    assert_at_every_worker_count(
        "d2a_every.chz",
        RECOVERED_DEADLOCK_THEN_COUSIN_JOIN,
        |out| {
            out.status.success()
                && String::from_utf8_lossy(&out.stdout) == "err\ncousin got 5\ndone\n"
        },
        "exit 0 with stdout `err`, `cousin got 5`, `done`",
    );
}

/// W13-5 (TICKET-125) residual, T>=2 — see [`w13_5_d2a_cousin_join_completes_at_every_worker_count`].
#[test]
fn w13_5_d2d_roles_swapped_completes_at_every_worker_count() {
    assert_at_every_worker_count(
        "d2d_every.chz",
        RECOVERED_DEADLOCK_ROLES_SWAPPED,
        |out| {
            out.status.success()
                && String::from_utf8_lossy(&out.stdout) == "err\ntask got 5\ndone\n"
        },
        "exit 0 with stdout `err`, `task got 5`, `done`",
    );
}

/// W13-5 (TICKET-125) residual, T>=2 — the third instance (a recovered PANIC rather than a recovered
/// inner deadlock), see [`w13_5_d2a_cousin_join_completes_at_every_worker_count`].
#[test]
fn w13_5_g3_recovered_panic_then_cousin_join_completes_at_every_worker_count() {
    assert_at_every_worker_count(
        "g3_every.chz",
        RECOVERED_PANIC_THEN_COUSIN_JOIN,
        |out| {
            out.status.success()
                && String::from_utf8_lossy(&out.stdout) == "err\ncousin got 5\ndone\n"
        },
        "exit 0 with stdout `err`, `cousin got 5`, `done`",
    );
}

/// TICKET-125 — a GENUINE two-leaf deadlock on a channel created in `main` must still fault at every
/// worker count: the non-provable fallback (fault the lowest-index leaf, then re-judge) must retire
/// BOTH leaves rather than decline forever.
const TWO_LEAF_DEADLOCK_ON_A_MAIN_CHANNEL: &str = "fn main():
    ch := Channel[int](0)
    parallel:
        spawn:
            parallel:
                spawn:
                    ch.recv()
        spawn:
            parallel:
                spawn:
                    ch.recv()
    print(\"unreachable\")
main()
";

#[test]
fn two_leaf_deadlock_on_a_main_channel_still_faults_at_every_worker_count() {
    assert_at_every_worker_count(
        "two_leaf_deadlock_main_channel.chz",
        TWO_LEAF_DEADLOCK_ON_A_MAIN_CHANNEL,
        faulted_deadlock,
        "a `deadlock` fault",
    );
}

/// W13-25 (TICKET-128) — a rendezvous wake must not wait for the woken receiver's blocking
/// native. The sender's `for` loop finishes its three sends the moment the third receive
/// happens; the receiver then burns CPU three times before finally blocking on `io.input`.
/// A broadcast-only wake that files the woken sender behind the receiver's blocking native
/// stalls "sender resumed" until stdin is provided.
const RENDEZVOUS_WAKE_STALL: &str = "import std.io
fn main():
    ch := Channel[int](0)
    parallel:
        spawn:
            for i in range(3):
                ch.send(i)
            print(\"sender resumed\")
        spawn:
            for i in range(3):
                t := 0
                for j in range(200000):
                    t += j
                v := ch.recv()
            s := io.input(\"\")
            print(\"got stdin\")
main()
";

#[test]
fn a_rendezvous_wake_does_not_wait_for_the_receivers_blocking_native() {
    let dir = std::env::temp_dir().join(format!(
        "chz-threads-128-runnext-stall-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("rendezvous_wake_stall.chz");
    std::fs::write(&path, RENDEZVOUS_WAKE_STALL).expect("write program");

    for threads in ["2", ""] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
        cmd.args(["run", path.to_str().unwrap()]);
        if threads.is_empty() {
            cmd.env_remove("CHEZZI_THREADS");
        } else {
            cmd.env("CHEZZI_THREADS", threads);
        }
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn chezzi");

        let mut stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });

        loop {
            match rx.recv_timeout(std::time::Duration::from_secs(20)) {
                Ok(line) if line == "sender resumed" => break,
                Ok(_) => continue,
                Err(_) => panic!(
                    "the woken sender waited for stdin (W13-25 runnext stall) at \
                     CHEZZI_THREADS={threads}"
                ),
            }
        }

        use std::io::Write;
        let _ = writeln!(stdin, "x");
        drop(stdin);
        let _ = reader.join();
        let status = child.wait().expect("wait chezzi");
        assert!(
            status.success(),
            "chezzi run {path:?} failed at CHEZZI_THREADS={threads}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// W13-6 (TICKET-125): two siblings each recover an inner deadlock, then fan in to the main body's
/// `recv()` loop. Must print `t 2` and exit 0; hangs instead at `CHEZZI_THREADS=2`/default.
const TWO_RECOVERERS_FAN_IN: &str = "fn main():
    out := Channel[int](0)
    parallel:
        for i in range(2):
            spawn:
                r := recover:
                    parallel:
                        spawn:
                            never := Channel[int](0)
                            never.recv()
                match r:
                    Ok(_): out.send(0)
                    Err(e): out.send(1)
        t := 0
        for i in range(2):
            t = t + out.recv()
        print(\"t {t}\")
main()
";

/// exec_join (TICKET-125, filed by TICKET-112): an Executor job whose OUTERMOST nursery's only undone
/// fiber is an owner blocked at a nested join (the nested `spawn` has no statement after its own
/// `parallel:`, so it waits at ITS join rather than on a channel op). Must complete and exit 0; hangs
/// instead at `CHEZZI_THREADS=2`/default.
const EXECUTOR_JOB_OWNER_BLOCKED_AT_NESTED_JOIN: &str = "import std.concurrency

fn job():
    never := Channel[int](0)
    r := recover:
        parallel:
            spawn:
                parallel:
                    spawn:
                        parallel:
                            spawn:
                                never.recv()
    match r:
        Ok(_): print(\"job ok\")
        Err(e): print(\"job err\")

fn main():
    ex := Executor()
    ex.submit(fn(): job())
    ex.shutdown()
    print(\"done\")
main()
";

/// W13-4 (TICKET-125): the depth-3 channel-parked-owner shape must fault `deadlock` at EVERY worker
/// count, not just default/T=2 as the pre-fix binary already does. Red on base: hangs at T=1.
#[test]
fn w13_4_channel_parked_owner_depth_3_faults_at_every_worker_count() {
    assert_at_every_worker_count(
        "channel_parked_owner_depth3_every.chz",
        CHANNEL_PARKED_OWNER_DEPTH_3,
        faulted_deadlock,
        "a `deadlock` fault",
    );
}

/// W13-4 (TICKET-125): reproduces the depth-3 channel-parked-owner hang at `CHEZZI_THREADS=1`.
#[test]
fn w13_4_channel_parked_owner_at_depth_3_faults_at_thread_one() {
    let dir = std::env::temp_dir().join(format!("chz-threads-125-w134-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("channel_parked_owner_depth3.chz");
    std::fs::write(&path, CHANNEL_PARKED_OWNER_DEPTH_3).expect("write program");
    let out = run_with_hang_deadline(&path, "1");
    let _ = std::fs::remove_dir_all(&dir);
    let out = out.unwrap_or_else(|| {
        panic!(
            "channel_parked_owner_depth3.chz hung past its 20 s deadline at CHEZZI_THREADS=1; want a `deadlock` fault"
        )
    });
    assert!(
        faulted_deadlock(&out),
        "want a `deadlock` fault at CHEZZI_THREADS=1, got {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// W13-5 (TICKET-125): reproduces the false `deadlock` fault at `CHEZZI_THREADS=1`, where the bug
/// measured 5/5.
#[test]
fn w13_5_recovered_deadlock_then_cousin_join_completes_at_thread_one() {
    let dir = std::env::temp_dir().join(format!("chz-threads-125-w135-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("recovered_deadlock_cousin_join.chz");
    std::fs::write(&path, RECOVERED_DEADLOCK_THEN_COUSIN_JOIN).expect("write program");
    let out = run_with_hang_deadline(&path, "1");
    let _ = std::fs::remove_dir_all(&dir);
    let out = out.unwrap_or_else(|| {
        panic!(
            "recovered_deadlock_cousin_join.chz hung past its 20 s deadline at CHEZZI_THREADS=1; want exit 0 with stdout `err\\ncousin got 5\\ndone\\n`"
        )
    });
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout == "err\ncousin got 5\ndone\n",
        "want exit 0 with stdout `err\\ncousin got 5\\ndone\\n`, got {:?}\nstdout: {}\nstderr: {}",
        out.status,
        stdout,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// W13-6 (TICKET-125) residual: reproduces the fan-in hang at `CHEZZI_THREADS=2`. Step 7's
/// replacement-worker fix closed this at T=2 but false-faulted `i6n2` at T=4 (2/8); the
/// `outer.runnable>0` gate fixed T=4 but re-broke T=2, so per the plan's own rollback the fix is
/// reverted rather than shipped partially wrong. Tracked OPEN in `docs/gaps.md` (W13-6).
#[ignore = "TICKET-125 residual: W13-6 at T>=2 — see docs/gaps.md"]
#[test]
fn w13_6_two_recoverers_fan_in_completes_at_thread_two() {
    let dir = std::env::temp_dir().join(format!("chz-threads-125-w136-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("two_recoverers_fan_in.chz");
    std::fs::write(&path, TWO_RECOVERERS_FAN_IN).expect("write program");
    let out = run_with_hang_deadline(&path, "2");
    let _ = std::fs::remove_dir_all(&dir);
    let out = out.unwrap_or_else(|| {
        panic!(
            "two_recoverers_fan_in.chz hung past its 20 s deadline at CHEZZI_THREADS=2; want exit 0 with stdout `t 2\\n`"
        )
    });
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout == "t 2\n",
        "want exit 0 with stdout `t 2\\n`, got {:?}\nstdout: {}\nstderr: {}",
        out.status,
        stdout,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// exec_join (TICKET-125): the same shape must complete at EVERY worker count, not just T=2.
#[test]
fn exec_join_owner_blocked_at_nested_join_completes_at_every_worker_count() {
    assert_at_every_worker_count(
        "executor_job_owner_blocked_at_nested_join_every.chz",
        EXECUTOR_JOB_OWNER_BLOCKED_AT_NESTED_JOIN,
        |out| {
            let stdout = String::from_utf8_lossy(&out.stdout);
            out.status.success() && stdout.contains("job err") && stdout.contains("done")
        },
        "exit 0 with stdout containing `job err` and `done`",
    );
}

/// b4a (TICKET-125): an Executor job recovers a nested deadlock and completes, at every worker count.
const B4A_EXECUTOR_JOB_RECOVERS_NESTED_DEADLOCK: &str = "import std.concurrency\nfn job():
    never := Channel[int](0)
    r := recover:
        parallel:
            spawn:
                parallel:
                    spawn:
                        never.recv()
            never.recv()
    match r:
        Ok(_): print(\"job ok\")
        Err(e): print(\"job err\")
fn main():
    ex := Executor()
    ex.submit(fn(): job())
    ex.shutdown()
    print(\"done\")
main()
";

#[test]
fn b4a_executor_job_recovers_nested_deadlock_at_every_worker_count() {
    assert_at_every_worker_count(
        "b4a_executor_job_recovers_nested_deadlock.chz",
        B4A_EXECUTOR_JOB_RECOVERS_NESTED_DEADLOCK,
        |out| String::from_utf8_lossy(&out.stdout) == "job err\ndone\n" && out.status.success(),
        "exit 0 with stdout `job err\\ndone\\n`",
    );
}

/// e10 (TICKET-125): an Executor job's genuine nested deadlock still faults, at every worker count.
const E10_EXECUTOR_JOB_NESTED_DEADLOCK_FAULTS: &str =
    "import std.concurrency\nnever := Channel[int](0)
fn job():
    parallel:
        spawn:
            parallel:
                spawn:
                    never.recv()
            never.recv()
fn main():
    ex := Executor()
    ex.submit(fn(): job())
    ex.shutdown()
    print(\"unreachable\")
main()
";

#[test]
fn e10_executor_job_nested_deadlock_faults_at_every_worker_count() {
    assert_at_every_worker_count(
        "e10_executor_job_nested_deadlock.chz",
        E10_EXECUTOR_JOB_NESTED_DEADLOCK_FAULTS,
        faulted_deadlock,
        "a `deadlock` fault",
    );
}

/// e11 (TICKET-125): an Executor job's LIVE nested rendezvous still reaches `main`, at every worker count.
const E11_EXECUTOR_JOB_NESTED_DEADLOCK_REACHES_MAIN: &str =
    "import std.concurrency\nnever := Channel[int](0)
out := Channel[int](0)
fn job():
    parallel:
        spawn:
            parallel:
                spawn:
                    never.recv()
    out.send(1)
fn main():
    ex := Executor()
    ex.submit(fn(): job())
    print(\"main {out.recv()}\")
    ex.shutdown()
    print(\"unreachable\")
main()
";

#[test]
fn e11_executor_job_nested_deadlock_reaches_main_at_every_worker_count() {
    assert_at_every_worker_count(
        "e11_executor_job_nested_deadlock.chz",
        E11_EXECUTOR_JOB_NESTED_DEADLOCK_REACHES_MAIN,
        faulted_deadlock,
        "a `deadlock` fault",
    );
}

/// exec_join (TICKET-125, filed by TICKET-112): reproduces the Executor-job outermost-nursery hang at
/// `CHEZZI_THREADS=2`.
#[test]
fn exec_join_owner_blocked_at_nested_join_completes_at_thread_two() {
    let dir = std::env::temp_dir().join(format!("chz-threads-125-execjoin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("executor_job_owner_blocked_at_nested_join.chz");
    std::fs::write(&path, EXECUTOR_JOB_OWNER_BLOCKED_AT_NESTED_JOIN).expect("write program");
    let out = run_with_hang_deadline(&path, "2");
    let _ = std::fs::remove_dir_all(&dir);
    let out = out.unwrap_or_else(|| {
        panic!(
            "executor_job_owner_blocked_at_nested_join.chz hung past its 20 s deadline at CHEZZI_THREADS=2; want exit 0 with stdout containing `job err` and `done`"
        )
    });
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("job err") && stdout.contains("done"),
        "want exit 0 with stdout containing `job err` and `done`, got {:?}\nstdout: {}\nstderr: {}",
        out.status,
        stdout,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// TICKET-125 step 16 — builds one edge-table cell: `depth` nested `parallel:`/`spawn:` levels ending
/// in an innermost `spawn: never.recv()`. `channel_owner` additionally runs `never.recv()` in the
/// DEEPEST nursery's BODY (right after its own `spawn:`, still inside that same `parallel:` block) —
/// `b2d.chz`'s shape; when `false` every body is empty (a pure join-blocked owner chain). `recovered`
/// wraps the whole nested structure in `r := recover:` and prints `err`/`ok` then `done`.
fn edge_table_cell(depth: usize, channel_owner: bool, recovered: bool) -> String {
    fn nested(level: usize, depth: usize, indent: usize, channel_owner: bool) -> String {
        let pad = |n: usize| " ".repeat(n);
        let mut s = format!("{}parallel:\n{}spawn:\n", pad(indent), pad(indent + 4));
        if level == depth {
            s.push_str(&format!("{}never.recv()\n", pad(indent + 8)));
            if channel_owner {
                s.push_str(&format!("{}never.recv()\n", pad(indent + 4)));
            }
        } else {
            s.push_str(&nested(level + 1, depth, indent + 8, channel_owner));
        }
        s
    }
    let body = nested(1, depth, 4, channel_owner);
    if recovered {
        format!(
            "fn main():\n    never := Channel[int](0)\n    r := recover:\n{}    match r:\n        Ok(_): print(\"ok\")\n        Err(e): print(\"err\")\n    print(\"done\")\nmain()\n",
            body.lines()
                .map(|l| format!("    {l}\n"))
                .collect::<String>()
        )
    } else {
        format!(
            "fn main():\n    never := Channel[int](0)\n{body}    print(\"unreachable\")\nmain()\n"
        )
    }
}

/// TICKET-125 step 16 — the full owner (join/channel) × depth (1..4) × recovered (no/yes) edge table:
/// every non-recovered cell must fault `deadlock` at every worker count, every recovered cell must
/// exit 0 with stdout exactly `err\ndone\n` (DEC-092) at every worker count.
#[test]
fn nested_verdict_edge_table_matches_go_at_every_worker_count() {
    for &channel_owner in &[false, true] {
        let owner = if channel_owner { "channel" } else { "join" };
        for depth in 1..=4 {
            for &recovered in &[false, true] {
                let program = edge_table_cell(depth, channel_owner, recovered);
                let file = format!(
                    "edge_{owner}_depth{depth}_{}.chz",
                    if recovered { "recovered" } else { "plain" }
                );
                if recovered {
                    assert_at_every_worker_count(
                        &file,
                        &program,
                        |out| {
                            out.status.success()
                                && String::from_utf8_lossy(&out.stdout) == "err\ndone\n"
                        },
                        "exit 0 with stdout `err`, `done`",
                    );
                } else {
                    assert_at_every_worker_count(
                        &file,
                        &program,
                        faulted_deadlock,
                        "a `deadlock` fault",
                    );
                }
            }
        }
    }
}

/// TICKET-132 (`nat_anc.chz`, `docs/gaps.md` W13-6 residual): a nursery join taken under
/// `native_reentry > 0` (a nursery inside `[1].map(leaf)`) runs the scheduler loop INLINE on the
/// owner's own OS worker instead of parking it, so the inline waiter can pop its own ancestor fiber
/// off the global queue and then wait on a fiber beneath it. At `CHEZZI_THREADS=1` there is only one
/// worker, so this cycle always forms: 12/12 runs hang past a 15 s deadline (measured 2026-09-17).
const NAT_ANC: &str = "fn burn(n: int) -> int:
    x := 0
    i := 0
    while i < n:
        x = x + i * i - i
        i += 1
    return x

fn leaf(x: int) -> int:
    parallel:
        spawn: burn(2000000)
    return x

fn inner() -> int:
    r := [1].map(leaf)
    return r[0]

fn outer() -> int:
    parallel:
        spawn: inner()
        burn(300000)
        return 2
    return 0

fn main():
    parallel:
        spawn: outer()
        spawn: outer()
    print(\"done\")
main()
";

#[test]
fn nat_anc_nursery_join_under_native_reentry_hangs_at_thread_one() {
    let dir = std::env::temp_dir().join(format!("chz-threads-132-nat-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("nat_anc.chz");
    std::fs::write(&path, NAT_ANC).expect("write program");
    let out = run_with_hang_deadline(&path, "1");
    let _ = std::fs::remove_dir_all(&dir);
    let out = out.expect(
        "nat_anc.chz hung past its 15 s deadline at CHEZZI_THREADS=1 (TICKET-132): the nursery \
         join inside [1].map(leaf) waits INLINE on the owner's own OS worker, which can pop its \
         own ancestor fiber off the global queue",
    );
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("done"),
        "nat_anc.chz must print `done` and exit 0 at CHEZZI_THREADS=1, like its Go twin; got {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// TICKET-132 (`esc_anc.chz`, `docs/gaps.md` W13-6 residual): a body that escapes its nursery (a
/// `return` out of a nested `parallel:`) runs `abort_fiber_owned_nursery`, which also runs the
/// scheduler loop INLINE on the owner's own OS worker. Same cycle as `nat_anc.chz`: 12/12 runs hang
/// past a 15 s deadline at `CHEZZI_THREADS=1` (measured 2026-09-17).
const ESC_ANC: &str = "fn burn(n: int) -> int:
    x := 0
    i := 0
    while i < n:
        x = x + i * i - i
        i += 1
    return x

fn inner() -> int:
    parallel:
        spawn: burn(2000000)
        return 1
    return 0

fn outer() -> int:
    parallel:
        spawn: inner()
        burn(300000)
        return 2
    return 0

fn main():
    parallel:
        spawn: outer()
        spawn: outer()
    print(\"done\")
main()
";

#[test]
fn esc_anc_nursery_escape_abort_hangs_at_thread_one() {
    let dir = std::env::temp_dir().join(format!("chz-threads-132-esc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("esc_anc.chz");
    std::fs::write(&path, ESC_ANC).expect("write program");
    let out = run_with_hang_deadline(&path, "1");
    let _ = std::fs::remove_dir_all(&dir);
    let out = out.expect(
        "esc_anc.chz hung past its 15 s deadline at CHEZZI_THREADS=1 (TICKET-132): \
         abort_fiber_owned_nursery waits INLINE on the owner's own OS worker, which can pop its \
         own ancestor fiber off the global queue",
    );
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("done"),
        "esc_anc.chz must print `done` and exit 0 at CHEZZI_THREADS=1, like its Go twin; got {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(15);
    lines[start..].join("\n")
}

/// Extracts every full `FAIL`/`ERROR` record (name, position, message, and any stack frame lines)
/// from a `chezzi test` report, verbatim. `tail()` keeps only the last 15 lines of stdout, which cut
/// the actual `FAIL <name> (<file:line>) …` line on every prior red run of
/// `chz_suite_passes_at_a_second_worker_count` (TICKET-110/111/113/115) — the summary line survived,
/// but the one thing that names the failing `.chz` test did not.
fn fail_lines(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let is_record_start = |l: &str| {
        l.starts_with("PASS ")
            || l.starts_with("FAIL ")
            || l.starts_with("ERROR ")
            || l.starts_with("OVER-MEMORY ")
            || l.starts_with("TIMED-OUT ")
    };
    let is_summary = |l: &str| l.contains(" test(s):");
    let mut out: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].starts_with("FAIL ") || lines[i].starts_with("ERROR ") {
            out.push(lines[i]);
            i += 1;
            while i < lines.len() && !is_record_start(lines[i]) && !is_summary(lines[i]) {
                out.push(lines[i]);
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    if out.is_empty() {
        "<no FAIL/ERROR line found>".to_string()
    } else {
        out.join("\n")
    }
}

#[cfg(test)]
mod fail_lines_tests {
    use super::fail_lines;

    #[test]
    fn extracts_full_fail_record_including_wrapped_message() {
        let report = "PASS a (f.chz)\n\
                       FAIL parse_large_string_is_not_quadratic (tests/chz/stdlib/json_test.chz:32:5) assertion failed:\n\
                       json.parse(150000-char string) took 2.1s, expected < 2.0s\n\
                       (2.1 < 2.0)\n\
                       PASS b (f.chz)\n\
                       \n\
                       915 test(s): 914 passed, 1 failed, 0 errored\n";
        let got = fail_lines(report);
        assert!(
            got.contains("FAIL parse_large_string_is_not_quadratic (tests/chz/stdlib/json_test.chz:32:5) assertion failed:"),
            "got:\n{got}"
        );
        assert!(
            got.contains("json.parse(150000-char string) took 2.1s, expected < 2.0s"),
            "wrapped message line must be kept, not truncated: got:\n{got}"
        );
        assert!(
            !got.contains("PASS a"),
            "must not include unrelated PASS lines: got:\n{got}"
        );
        assert!(
            !got.contains("test(s):"),
            "must not include the summary line: got:\n{got}"
        );
    }
}
