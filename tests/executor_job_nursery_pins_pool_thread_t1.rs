//! W13-7: an Executor job parked at a nursery join whose only child is channel-blocked pins its
//! pool thread at `CHEZZI_THREADS=1`, so a sibling job that would feed that channel never starts.
//! `docs/concurrency.md:1805` (TICKET-052) promises "a blocked job no longer pins its pool thread
//! … fixed for every shape above", but the yield bracket (`src/vm/pool.rs` `yield_slot`) only fires
//! for a job blocked directly ON a channel, not for a job parked at a nursery join whose CHILD is.
//! Go's `GOMAXPROCS=1` twin (`sched/go/h1.go`) completes.
//!
//! Spawns and polls rather than calling `output()`: a hung child never closes its pipes, so
//! `output()` would wedge this test binary instead of failing it.

use std::process::Command;

#[test]
fn job_nursery_child_blocked_on_recv_does_not_pin_the_pool_thread_at_t1() {
    let program = "import std.concurrency\n\
        a := Channel[int](0)\n\
        fn j1():\n    \
            parallel:\n        \
                spawn:\n            \
                    print(\"j1 got {a.recv()}\")\n\
        fn main():\n    \
            ex := Executor()\n    \
            ex.submit(fn(): j1())\n    \
            ex.submit(fn(): a.send(1))\n    \
            ex.shutdown()\n    \
            print(\"done\")\n\
        main()\n";

    let dir = std::env::temp_dir().join(format!(
        "chz-w13-7-job-nursery-pins-t1-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("h1c.chz");
    std::fs::write(&path, program).expect("write fixture");

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
        let _ = std::fs::remove_dir_all(&dir);
        panic!(
            "CHEZZI_THREADS=1: a job parked at a nursery join whose child is channel-blocked must \
             not pin the pool thread; expected \"j1 got 1\"/\"done\" (no exit within 10s)"
        );
    };

    let mut stdout = String::new();
    if let Some(mut o) = child.stdout.take() {
        use std::io::Read as _;
        let _ = o.read_to_string(&mut stdout);
    }
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        use std::io::Read as _;
        let _ = e.read_to_string(&mut stderr);
    }
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        status.success(),
        "CHEZZI_THREADS=1: expected rc=0, got {status} — stdout: {stdout} stderr: {stderr}"
    );
    assert_eq!(
        stdout, "j1 got 1\ndone\n",
        "CHEZZI_THREADS=1: expected \"j1 got 1\\ndone\\n\", got stdout: {stdout:?} stderr: {stderr:?}"
    );
}
