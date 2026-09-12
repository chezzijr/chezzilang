//! W13-8: `shutdown_now()` must cancel a job's nursery child parked on a blocking `recv`, since
//! `recv` is a documented cancellation point (`docs/concurrency.md`: `shutdown_now()` "ask[s]
//! running jobs to stop at their next cancellation point"). Instead the nursery's own deadlock
//! detector fires first and faults the whole run with `deadlock: ...`, before `shutdown_now()`
//! gets a chance to cancel the child. Go's `select`+`cancel()` equivalent completes.
//!
//! Subprocess-only, matching `executor_reentrant_shutdown.rs`: needs a full-size worker pool.

use std::process::Command;

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
