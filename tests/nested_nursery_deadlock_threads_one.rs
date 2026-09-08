//! TICKET-095 — a deadlock nested under a spawned task's implicit nursery hangs forever at
//! `CHEZZI_THREADS=1`, where every other worker count faults `deadlock: …` in ~11 ms.
//! `Vm::op_enter_nursery` makes a nursery entered inside a spawned task LAZY at
//! `worker_count() == 1`, so it runs as a SCOPE on the ENCLOSING sched instead of building its own —
//! the enclosing fiber's own OS thread becomes that scope's only worker, and that fiber stays
//! counted in `SchedCore::running` while it sits blocked in the join, so `running == 0` could never
//! hold. The program hangs with no output and no diagnostic — `--timeout` is a `chezzi test` flag,
//! so `chezzi run` has no escape hatch.
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
        stderr.contains("dd6.chz:3:5): deadlock:"),
        "expected a `deadlock: …` fault naming the INNER nursery's span (dd6.chz:3:5), like every \
         other worker count, got: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recovered_nested_nursery_deadlock_lets_the_program_continue_at_one_worker() {
    let dir = std::env::temp_dir().join(format!("chz-ticket095-rec-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("rec.chz");
    std::fs::write(
        &path,
        "ch := Channel[int](0)\n\
         fn f():\n    \
             spawn:\n        \
                 v := ch.recv()\n\
         r := recover:\n    \
             parallel:\n        \
                 spawn f()\n\
         print(r)\n\
         print(\"still running\")\n",
    )
    .expect("write recovered nested-nursery deadlock fixture");

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
            // Poll interval against a deadline, not a happens-before edge: a hung child never closes
            // its pipes, so `output()` would wedge this test binary instead of failing it, and a
            // subprocess offers no channel/latch/counter to wait on instead.
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        panic!(
            "CHEZZI_THREADS=1 must let `recover:` catch the nested-nursery deadlock and continue, \
             like every other worker count, not hang forever (no exit within 10s)"
        );
    };

    let mut stdout = String::new();
    if let Some(mut o) = child.stdout.take() {
        use std::io::Read as _;
        let _ = o.read_to_string(&mut stdout);
    }
    assert!(
        status.success(),
        "expected rc=0 (the deadlock was caught by `recover:`), got {status} (stdout: {stdout})"
    );
    assert!(
        stdout.contains("Err('deadlock:"),
        "expected the recovered `Err('deadlock: …')` on stdout, got: {stdout}"
    );
    assert!(
        stdout.contains("still running"),
        "expected the program to continue past the recovered deadlock, got: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
