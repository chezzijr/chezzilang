//! TICKET-095 — a deadlock nested under a spawned task's implicit nursery hangs forever at
//! `CHEZZI_THREADS=1`, where every other worker count faults `deadlock: …` in ~11 ms. The nested
//! private `MnSched`'s inline joiner never sits in `take_runnable` to evaluate `is_deadlocked` when
//! `eager_helper_wids(1)` is empty (`src/vm/sched.rs:5584`), so the program hangs with no output and
//! no diagnostic — `--timeout` is a `chezzi test` flag, so `chezzi run` has no escape hatch.
//!
//! Spawns and polls rather than calling `output()`: a hung child never closes its pipes, so
//! `output()` would wedge this test binary instead of failing it (same pattern as
//! `chezzi_gc_deadlock.rs`).

use std::process::Command;

#[test]
fn nested_nursery_deadlock_faults_at_one_worker_like_every_other_count() {
    let dir = std::env::temp_dir().join(format!("chz-ticket095-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("dd6.chz");
    std::fs::write(
        &path,
        "ch := Channel[int](0)\n\
         fn f():\n    \
             spawn:\n        \
                 print(ch.recv())\n\
         spawn f()\n",
    )
    .expect("write nested-nursery deadlock fixture");

    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn chezzi");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
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
            "CHEZZI_THREADS=1 must fault `deadlock: …` at rc=1 like every other worker count, not \
             hang forever with no output (no exit within 10s)"
        );
    };

    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        use std::io::Read as _;
        let _ = e.read_to_string(&mut stderr);
    }
    assert!(
        !status.success(),
        "expected a nonzero exit for an undetected deadlock, got {status} (stderr: {stderr})"
    );
    assert!(
        stderr.contains("deadlock:"),
        "expected a `deadlock: …` fault on stderr, got: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
