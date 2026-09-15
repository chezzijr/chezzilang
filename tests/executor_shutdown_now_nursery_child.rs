//! W13-8: `shutdown_now()` must cancel a job's nursery child parked on a blocking `recv`, since
//! `recv` is a documented cancellation point (`docs/concurrency.md`: `shutdown_now()` "ask[s]
//! running jobs to stop at their next cancellation point"). Instead the nursery's own deadlock
//! detector fires first and faults the whole run with `deadlock: ...`, before `shutdown_now()`
//! gets a chance to cancel the child. Go's `select`+`cancel()` equivalent completes.
//!
//! Subprocess-only, matching `executor_reentrant_shutdown.rs`: needs a full-size worker pool.

use std::process::Command;

#[path = "support/hang_deadline.rs"]
mod hang_deadline;

#[test]
fn shutdown_now_cancels_a_jobs_nursery_child_blocked_on_recv() {
    let dir = std::env::temp_dir().join(format!(
        "chz-executor-shutdown-now-nursery-child-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(
        &path,
        "import std.concurrency\nimport std.time\nnever := Channel[int](0)\nfn job():\n    parallel:\n        spawn:\n            never.recv()\nfn main():\n    ex := Executor()\n    ex.submit(fn(): job())\n    time.sleep_ms(300)\n    print(\"awake\")\n    ex.shutdown_now()\n    print(\"done\")\nmain()\n",
    )
    .expect("write program");

    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .output()
        .expect("run chezzi");
    let _ = std::fs::remove_dir_all(&dir);

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "shutdown_now() must cancel a job's nursery child blocked on recv, not fault the run: \
         status {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status.code(),
    );
    assert_eq!(
        stdout, "awake\ndone\n",
        "expected both prints to survive a clean shutdown_now(): stdout {stdout:?}, stderr {stderr:?}"
    );
}

/// Writes `program` to a temp file and, at each of `Some("1")`, `Some("2")`, `None` (unset —
/// default worker count), runs it [`runs`] times through [`hang_deadline::run_with_hang_deadline`],
/// asserting every run returns `Some` and passes `check`.
fn assert_at_every_worker_count(
    file: &str,
    program: &str,
    runs: usize,
    check: impl Fn(&std::process::Output),
) {
    let dir = std::env::temp_dir().join(format!("chz-w13-8-{file}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(file);
    std::fs::write(&path, program).expect("write program");

    for threads in [Some("1"), Some("2"), None] {
        for run in 0..runs {
            let out = hang_deadline::run_with_hang_deadline(&path, threads);
            let Some(out) = out else {
                let _ = std::fs::remove_dir_all(&dir);
                panic!("CHEZZI_THREADS={threads:?} run {run}: no exit within 10s");
            };
            check(&out);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

fn assert_clean_run(out: &std::process::Output, want_stdout: &str) {
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "expected rc=0, got {:?} — stdout: {stdout} stderr: {stderr}",
        out.status
    );
    assert_eq!(
        stdout, want_stdout,
        "got stdout: {stdout:?} stderr: {stderr:?}"
    );
}

/// W13-8 — a depth-two nursery child (a nursery nested inside a nursery) parked on `recv` must be
/// cancelled by `shutdown_now()` too, not just a depth-one child.
#[test]
fn shutdown_now_cancels_a_depth_two_nursery_child() {
    let program = "import std.concurrency\n\
        import std.time\n\
        never := Channel[int](0)\n\
        fn job():\n    \
            parallel:\n        \
                spawn:\n            \
                    parallel:\n                \
                        spawn:\n                    \
                            never.recv()\n\
        fn main():\n    \
            ex := Executor()\n    \
            ex.submit(fn(): job())\n    \
            time.sleep_ms(300)\n    \
            print(\"awake\")\n    \
            ex.shutdown_now()\n    \
            print(\"done\")\n\
        main()\n";
    assert_at_every_worker_count("h2b.chz", program, 5, |out| {
        assert_clean_run(out, "awake\ndone\n");
    });
}

/// W13-8 — the same cancellation, synchronised by a channel handshake instead of a sleep (DEC-050).
#[test]
fn shutdown_now_cancels_a_nursery_child_synchronised_by_a_handshake() {
    let program = "import std.concurrency\n\
        never := Channel[int](0)\n\
        ready := Channel[int](1)\n\
        fn job():\n    \
            parallel:\n        \
                spawn:\n            \
                    ready.send(1)\n            \
                    never.recv()\n\
        fn main():\n    \
            ex := Executor()\n    \
            ex.submit(fn(): job())\n    \
            r := ready.recv()\n    \
            print(\"awake {r}\")\n    \
            ex.shutdown_now()\n    \
            print(\"done\")\n\
        main()\n";
    assert_at_every_worker_count("hs.chz", program, 5, |out| {
        assert_clean_run(out, "awake 1\ndone\n");
    });
}

/// Negative control — a job nursery child parked on `recv` FOREVER with NO `shutdown_now()` (a
/// plain `shutdown()`) is a genuine deadlock, and must still fault. The W13-8 drain trigger must
/// never fire in the absence of a tripped cancel flag.
#[test]
fn a_job_nursery_deadlock_with_no_shutdown_still_faults() {
    let program = "import std.concurrency\n\
        import std.time\n\
        never := Channel[int](0)\n\
        fn job():\n    \
            parallel:\n        \
                spawn:\n            \
                    never.recv()\n\
        fn main():\n    \
            ex := Executor()\n    \
            ex.submit(fn(): job())\n    \
            time.sleep_ms(300)\n    \
            print(\"awake\")\n    \
            ex.shutdown()\n    \
            print(\"done\")\n\
        main()\n";
    assert_at_every_worker_count("h2c_noshut.chz", program, 5, |out| {
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            !out.status.success(),
            "a genuine deadlock (no shutdown_now) must still fault: rc={:?} stdout: {stdout} \
             stderr: {stderr}",
            out.status
        );
        assert!(
            stderr.contains("deadlock: every task in this parallel: block is blocked"),
            "expected a deadlock fault, got stdout: {stdout:?} stderr: {stderr:?}"
        );
    });
}

/// W13-8 — `shutdown_now()` must end a nursery child whose `defer` blocks forever, cutting the
/// cleanup short at rc=0 rather than hanging. This is the EXISTING nursery precedent (`nrc.chz`,
/// `## Digest`), not a new choice this ticket makes: a trigger that re-drains a re-parking fiber
/// (the cleanup's own `stuck.recv()`) fails the hang deadline instead. Go's twin of this shape
/// reports `fatal error: all goroutines are asleep - deadlock!` — a deliberate, pre-existing
/// divergence this test pins today's outcome for, not endorses.
#[test]
fn shutdown_now_ends_a_nursery_child_whose_defer_blocks_forever() {
    let program = "import std.concurrency\n\
        never := Channel[int](0)\n\
        stuck := Channel[int](0)\n\
        ready := Channel[int](1)\n\
        fn cleanup():\n    \
            print(\"cleanup start\")\n    \
            stuck.recv()\n\
        fn job():\n    \
            parallel:\n        \
                spawn:\n            \
                    defer cleanup()\n            \
                    ready.send(1)\n            \
                    never.recv()\n\
        fn main():\n    \
            ex := Executor()\n    \
            ex.submit(fn(): job())\n    \
            r := ready.recv()\n    \
            print(\"awake {r}\")\n    \
            ex.shutdown_now()\n    \
            print(\"done\")\n\
        main()\n";
    assert_at_every_worker_count("dfr2.chz", program, 5, |out| {
        assert_clean_run(out, "awake 1\ncleanup start\ndone\n");
    });
}

/// W13-8 — the 10:59Z lost wakeup: `shutdown_now()`'s cancel store must reach a worker that reads
/// it on the wrong side of the trip. Twenty in-process rounds of submit/handshake/`shutdown_now` at
/// `CHEZZI_THREADS=1`, run fifty times: this samples the race rather than proving its absence
/// (a rare miss would show as an occasional hang past the deadline, not a wrong answer).
#[test]
fn shutdown_now_races_a_parked_nursery_child_at_one_worker() {
    let program = "import std.concurrency\n\
        fn job(never: Channel[int], ready: Channel[int]):\n    \
            parallel:\n        \
                spawn:\n            \
                    ready.send(1)\n            \
                    never.recv()\n\
        fn main():\n    \
            for i in range(20):\n        \
                never := Channel[int](0)\n        \
                ready := Channel[int](1)\n        \
                ex := Executor()\n        \
                ex.submit(fn(): job(never, ready))\n        \
                r := ready.recv()\n        \
                ex.shutdown_now()\n    \
            print(\"awake\")\n    \
            print(\"done\")\n\
        main()\n";
    let dir = std::env::temp_dir().join(format!("chz-w13-8-race-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("race.chz");
    std::fs::write(&path, program).expect("write program");

    for run in 0..50 {
        let out = hang_deadline::run_with_hang_deadline(&path, Some("1"));
        let Some(out) = out else {
            let _ = std::fs::remove_dir_all(&dir);
            panic!("CHEZZI_THREADS=1 run {run}: no exit within 10s");
        };
        assert_clean_run(&out, "awake\ndone\n");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
