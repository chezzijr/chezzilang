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
//! failures/hangs — exactly the tests already annotated "needs ≥2 free pool threads", pool risk G3,
//! `docs/gaps.md` W7-12r); `RUST_TEST_THREADS=1` took >54 minutes without finishing. A subprocess
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
        "tests/chz must pass at the default worker count\nsummary: {summary_default}\nstderr: {err_default}\nstdout tail:\n{}",
        tail(&out_default)
    );
    assert!(
        summary_default.contains(" passed, 0 failed, 0 errored"),
        "default run must be all-passing: {summary_default}"
    );

    let (ok_2, summary_2, out_2, err_2) = run_chz_test(&root, Some("2"), None);
    assert!(
        ok_2,
        "tests/chz must pass with CHEZZI_THREADS=2\nsummary: {summary_2}\nstderr: {err_2}\nstdout tail:\n{}",
        tail(&out_2)
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
/// path specifically (not merely `vm::worker_count()` in the lib test binary). Uses the exact "needs
/// ≥2 free pool threads" shape documented in `docs/gaps.md` (pool risk G3): a bounded channel already
/// full, one `Executor` job blocked trying to send into it, a second job that must close the channel
/// to unblock the first. `Vm::mn_join`'s eager dispatch means the first job PERMANENTLY holds its
/// pool thread (no replacement spin — a documented v1 hazard, not a bug), so:
/// - at 1 worker, the closer can never be dispatched → genuine hang (bounded here by `--timeout`, so
///   this test cannot itself wedge the runner);
/// - at ≥2 workers, the closer runs on the second, the blocked send observes the close and faults
///   `send on a closed channel` — fast (measured: single-digit ms).
///
/// A dropped/no-op env read would make ALL THREE runs behave like the default (>=2 cores on any CI
/// box) — i.e. all three would fault fast, none would time out. Seeing the 1-worker run actually
/// time out is the proof the knob has power, not just that something passed twice.
#[test]
fn chezzi_test_cli_honors_chezzi_threads_via_a_two_worker_precondition() {
    let dir = std::env::temp_dir().join(format!("chz-threads-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("needs_two_workers_test.chz");
    std::fs::write(
        &path,
        "import std.concurrency\n\n\
         test fn needs_two_workers():\n    \
         ch := Channel[int](1)\n    \
         ch.send(0)\n    \
         ex := Executor()\n    \
         ex.submit(fn(): ch.send(1))\n    \
         ex.submit(fn(): ch.close())\n    \
         ex.shutdown()\n    \
         assert true\n",
    )
    .expect("write program");

    // Default (auto — >=2 workers on any real box): the second job gets its own pool thread and
    // closes the channel; the blocked send faults immediately. Bounded to 5s as a smoke guard, not
    // because this run is expected to need it.
    let (_, summary, out, _) = run_chz_test(&path, None, Some(5_000));
    assert!(
        summary.contains("1 errored") && out.contains("send on a closed channel"),
        "default worker count should fault fast on the closed channel, not hang: {summary}\n{out}"
    );

    // CHEZZI_THREADS=2: same shape, explicit count instead of auto.
    let (_, summary, out, _) = run_chz_test(&path, Some("2"), Some(5_000));
    assert!(
        summary.contains("1 errored") && out.contains("send on a closed channel"),
        "CHEZZI_THREADS=2 should fault fast on the closed channel, not hang: {summary}\n{out}"
    );

    // CHEZZI_THREADS=1: the closer can never be dispatched — this must TIME OUT, not fault and not
    // pass. If it instead faults fast (or passes), the env var never reached the pool.
    let (_, summary, out, _) = run_chz_test(&path, Some("1"), Some(2_000));
    assert!(
        summary.contains("1 timed out"),
        "CHEZZI_THREADS=1 must starve the two-worker precondition and TIME OUT — a fault or a pass \
         here means CHEZZI_THREADS did not reach chezzi test's pool: {summary}\n{out}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// W8-8 — `--threads=1` (via `CHEZZI_THREADS=1`) must run exactly ONE CPU runner, not two. Before the
/// fix the inline joiner ran a fiber loop ALONGSIDE the unconditional `chezzi-eager` drainer thread
/// even at a budget of 1, so an 8x CPU workload only ran ~4.4x slower than a 1x workload (two runners
/// splitting the work) instead of ~8x. Program A runs one CPU burn inside a `parallel:` nursery,
/// program B runs eight copies of the same burn in one `parallel:` nursery, both timed under
/// `CHEZZI_THREADS=1`. The two runs are sequential, not concurrent, so a load spike landing on one and
/// not the other can shift the ratio — but not by enough to explain a pass at the 5.5 threshold:
/// T1-fix's review measured, under 16 concurrent CPU spinners on a 12-core box, ratios of 7.19 / 7.32 /
/// 7.03 against an idle-box baseline of 7.71 / 7.50 / 8.04 — well clear of 5.5 either way. Post-fix the
/// ratio must approach 8; pre-fix it measured
/// ~4.0 on a debug build (docs/gaps.md W8-8 measured ~4.4 on the release binary). The burn size (150k
/// iterations) is calibrated to ~100ms on a DEBUG build — `CARGO_BIN_EXE_chezzi` under `cargo test` is
/// the debug binary, not `--release`.
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

    let path_a = dir.join("burn_one.chz");
    std::fs::write(
        &path_a,
        format!(
            "{burn}fn main():\n    \
             parallel:\n        \
             spawn: burn(150000)\n\
             main()\n"
        ),
    )
    .expect("write program A");

    let path_b = dir.join("burn_eight.chz");
    let spawns = "        spawn: burn(150000)\n".repeat(8);
    std::fs::write(
        &path_b,
        format!("{burn}fn main():\n    parallel:\n{spawns}main()\n"),
    )
    .expect("write program B");

    let time_run = |path: &Path| -> std::time::Duration {
        let start = std::time::Instant::now();
        let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .arg("run")
            .arg(path)
            .env("CHEZZI_THREADS", "1")
            .output()
            .expect("run chezzi");
        let elapsed = start.elapsed();
        assert!(
            out.status.success(),
            "chezzi run {path:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        elapsed
    };

    let t1 = time_run(&path_a);
    let t8 = time_run(&path_b);

    // Negative control: t1 must be non-trivial, or a near-zero/near-zero ratio could pass by accident.
    assert!(
        t1 > std::time::Duration::from_millis(10),
        "program A finished too fast ({t1:?}) to be a meaningful baseline — recalibrate the burn size"
    );

    let ratio = t8.as_secs_f64() / t1.as_secs_f64();
    assert!(
        ratio > 5.5,
        "--threads=1 must serialize: 8x the CPU work should take close to 8x as long (t1={t1:?}, \
         t8={t8:?}, ratio={ratio:.2}). A ratio near 4 means the inline joiner is STILL running a \
         second fiber loop alongside the drainer at a budget of 1 (W8-8)."
    );

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
/// budget of one. Same construction as `threads_one_serializes_cpu_bound_parallel_tasks`: one burn vs
/// eight, both under `CHEZZI_THREADS=1`, ratio must clear 5.5 (measured pre-fix ~3.9 on this debug
/// binary, ~1.97 cores on the release binary per the review).
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

    let path_a = dir.join("nested_burn_one.chz");
    std::fs::write(
        &path_a,
        format!(
            "{burn}fn work():\n    \
             parallel:\n        \
             spawn: burn(150000)\n\n\
             fn main():\n    \
             parallel:\n        \
             work()\n\
             main()\n"
        ),
    )
    .expect("write program A");

    let path_b = dir.join("nested_burn_eight.chz");
    let spawns = "        spawn: burn(150000)\n".repeat(8);
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

    let time_run = |path: &Path| -> std::time::Duration {
        let start = std::time::Instant::now();
        let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .arg("run")
            .arg(path)
            .env("CHEZZI_THREADS", "1")
            .output()
            .expect("run chezzi");
        let elapsed = start.elapsed();
        assert!(
            out.status.success(),
            "chezzi run {path:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        elapsed
    };

    let t1 = time_run(&path_a);
    let t8 = time_run(&path_b);

    assert!(
        t1 > std::time::Duration::from_millis(10),
        "program A finished too fast ({t1:?}) to be a meaningful baseline — recalibrate the burn size"
    );

    let ratio = t8.as_secs_f64() / t1.as_secs_f64();
    assert!(
        ratio > 5.5,
        "--threads=1 must serialize a NESTED eager parallel: too: 8x the CPU work should take close \
         to 8x as long (t1={t1:?}, t8={t8:?}, ratio={ratio:.2}). A ratio well under 8 (measured ~3.9 \
         pre-fix on a debug build) means the nested scope's inline join loop is STILL running \
         alongside the outer scope's drainer at a budget of 1 (W8-8, nested arm)."
    );

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

/// The GC's mark walk must never hold two core locks at once.
///
/// `collect_core_gcrefs` used to lock a core's payload and recurse straight into a NESTED core's lock
/// while still holding the first, and `Heap::children` held the outer core's guard across the whole
/// call. With a CYCLIC core graph two workers marking concurrently then acquired the same two locks in
/// opposite orders — a textbook ABBA deadlock. Every thread parked in `futex_do_wait` at 0% CPU, with
/// no deadlock report and no way for `--timeout` to reach it, so it presents as a silent total hang.
///
/// Measured on the release binary at `CHEZZI_THREADS=4`: **8/40 runs hung** before the fix (and 2/80
/// even before the mark walk was sped up — the speedup widened a pre-existing window rather than
/// creating it), **0/40 after**. It needs all three of: a cycle in the core graph, GC pressure, and
/// `CHEZZI_THREADS >= 2` — removing any one gives 0 hangs, which is what identified the mechanism.
///
/// This drives the built binary because the hang is a whole-process stall; an in-process test would
/// wedge the test binary. The `--timeout` flag deliberately is NOT used: it cannot interrupt a thread
/// blocked on a `Mutex`, which is the point. The bound is wall-clock on the subprocess instead.
///
/// 40 rounds. The per-run hang rate is profile-dependent — measured pre-fix at 20% release (8/40)
/// and 12.5% debug (5/40) — and `cargo test` runs DEBUG, so size the round count off the debug
/// number: 1 - 0.875^40 ~ 99.5% RED. (24 rounds would be only ~96%, and the first RED check of this
/// test passed by luck at that count.) ~2s when green.
#[test]
fn gc_mark_walk_does_not_deadlock_on_a_cyclic_core_graph() {
    let dir = std::env::temp_dir().join(format!("chz-gc-abba-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("cyclic_cores.chz");
    std::fs::write(
        &path,
        "import std.concurrency\n\
         \n\
         struct Link:\n    \
             flag: Shared[bool]\n    \
             dl: Channel[bool]\n    \
             kids: Channel[Link]\n\
         \n\
         fn mk() -> Link:\n    \
             return Link(flag=Shared(false), dl=Channel[bool](), kids=Channel[Link]())\n\
         \n\
         root := mk()\n\
         cur := root\n\
         i := 0\n\
         while i < 2:\n    \
             nxt := mk()\n    \
             cur.kids.send(nxt)\n    \
             cur = nxt\n    \
             i = i + 1\n\
         cur.kids.send(root)\n\
         root.kids.send(root)\n\
         \n\
         parallel:\n    \
             spawn:\n        \
                 k := 0\n        \
                 while k < 2000:\n            \
                     root.flag.set(k % 2 == 0)\n            \
                     junk := [k, k + 1, k + 2]\n            \
                     k = k + 1\n    \
             spawn:\n        \
                 k := 0\n        \
                 while k < 2000:\n            \
                     cur.flag.update(fn(b: bool) -> bool: not b)\n            \
                     junk := {\"a\": k, \"b\": k}\n            \
                     k = k + 1\n    \
             spawn:\n        \
                 k := 0\n        \
                 while k < 2000:\n            \
                     junk := [[k], [k + 1]]\n            \
                     k = k + 1\n\
         print(\"alive\")\n",
    )
    .expect("write cyclic-core fixture");

    // SPAWN + poll + kill, never `output()`: a deadlocked child never closes its pipes, so
    // `output()` blocks forever and the RED case wedges the whole test binary instead of failing.
    // (Measured: it did exactly that on the first draft of this test.)
    for round in 0..40 {
        let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .arg("run")
            .arg(&path)
            .env("CHEZZI_THREADS", "4")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn chezzi");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let status = loop {
            match child.try_wait().expect("try_wait") {
                Some(st) => break Some(st),
                None if std::time::Instant::now() >= deadline => break None,
                None => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        };
        let Some(status) = status else {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "round {round}: the cyclic-core program DEADLOCKED (no exit within 20s) - the GC \
                 mark walk is holding two core locks at once again"
            );
        };
        let mut stdout = String::new();
        if let Some(mut o) = child.stdout.take() {
            use std::io::Read as _;
            let _ = o.read_to_string(&mut stdout);
        }
        assert!(status.success(), "round {round}: program exited {status}");
        assert!(
            stdout.contains("alive"),
            "round {round}: program did not reach its final print"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(15);
    lines[start..].join("\n")
}
