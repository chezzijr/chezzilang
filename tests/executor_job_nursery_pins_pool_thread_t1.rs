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

#[path = "support/hang_deadline.rs"]
mod hang_deadline;

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

/// W13-7 — two jobs, each opening its own nested nursery, feeding each other across a job boundary.
/// Five runs at `CHEZZI_THREADS=1`: the joiner-yield bracket must let the queued sibling job start.
#[test]
fn two_jobs_with_nested_nurseries_feeding_each_other_complete_at_t1() {
    let program = "import std.concurrency\n\
        a := Channel[int](0)\n\
        b := Channel[int](0)\n\
        fn j1():\n    \
            parallel:\n        \
                spawn:\n            \
                    parallel:\n                \
                        spawn:\n                    \
                            b.send(a.recv() + 1)\n\
        fn j2():\n    \
            parallel:\n        \
                spawn:\n            \
                    parallel:\n                \
                        spawn:\n                    \
                            a.send(1)\n                    \
                            print(\"j2 got {b.recv()}\")\n\
        fn main():\n    \
            ex := Executor()\n    \
            ex.submit(fn(): j1())\n    \
            ex.submit(fn(): j2())\n    \
            ex.shutdown()\n    \
            print(\"done\")\n\
        main()\n";
    let dir = std::env::temp_dir().join(format!("chz-w13-7-h1-t1-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("h1.chz");
    std::fs::write(&path, program).expect("write fixture");

    for run in 0..5 {
        let out = hang_deadline::run_with_hang_deadline(&path, Some("1"));
        let Some(out) = out else {
            let _ = std::fs::remove_dir_all(&dir);
            panic!("CHEZZI_THREADS=1 run {run}: no exit within 10s");
        };
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "CHEZZI_THREADS=1 run {run}: expected rc=0, got {:?} — stdout: {stdout} stderr: {stderr}",
            out.status
        );
        assert_eq!(
            stdout, "j2 got 2\ndone\n",
            "CHEZZI_THREADS=1 run {run}: got stdout: {stdout:?} stderr: {stderr:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// W13-7 — a job that submits a nested job back from nursery depth three. Five runs at
/// `CHEZZI_THREADS=1`.
#[test]
fn a_job_that_submits_back_from_depth_three_completes_at_t1() {
    let program = "import std.concurrency\n\
        fn main():\n    \
            ex := Executor()\n    \
            out := Channel[int](0)\n    \
            ex.submit(fn(): deep(ex, out))\n    \
            print(\"got {out.recv()}\")\n    \
            ex.shutdown()\n    \
            print(\"done\")\n\
        fn deep(ex: Executor, out: Channel[int]):\n    \
            parallel:\n        \
                spawn:\n            \
                    parallel:\n                \
                        spawn:\n                    \
                            parallel:\n                        \
                                spawn:\n                            \
                                    r := Channel[int](0)\n                            \
                                    ex.submit(fn(): r.send(7))\n                            \
                                    out.send(r.recv())\n\
        main()\n";
    let dir = std::env::temp_dir().join(format!("chz-w13-7-h3-t1-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("h3.chz");
    std::fs::write(&path, program).expect("write fixture");

    for run in 0..5 {
        let out = hang_deadline::run_with_hang_deadline(&path, Some("1"));
        let Some(out) = out else {
            let _ = std::fs::remove_dir_all(&dir);
            panic!("CHEZZI_THREADS=1 run {run}: no exit within 10s");
        };
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "CHEZZI_THREADS=1 run {run}: expected rc=0, got {:?} — stdout: {stdout} stderr: {stderr}",
            out.status
        );
        assert_eq!(
            stdout, "got 7\ndone\n",
            "CHEZZI_THREADS=1 run {run}: got stdout: {stdout:?} stderr: {stderr:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// W13-7 — two nursery jobs and their feeders, at `CHEZZI_THREADS=2`: the two-worker idle-park
/// joiner branch in `MnSched::take_runnable` must let the queued feeder jobs start too.
#[test]
fn two_nursery_jobs_and_their_feeders_complete_at_two_workers() {
    let program = "import std.concurrency\n\
        a := Channel[int](0)\n\
        b := Channel[int](0)\n\
        fn j1():\n    \
            parallel:\n        \
                spawn:\n            \
                    print(\"j1 got {a.recv()}\")\n\
        fn j2():\n    \
            parallel:\n        \
                spawn:\n            \
                    print(\"j2 got {b.recv()}\")\n\
        fn main():\n    \
            ex := Executor()\n    \
            ex.submit(fn(): j1())\n    \
            ex.submit(fn(): j2())\n    \
            ex.submit(fn(): a.send(1))\n    \
            ex.submit(fn(): b.send(2))\n    \
            ex.shutdown()\n    \
            print(\"done\")\n\
        main()\n";
    let dir = std::env::temp_dir().join(format!("chz-w13-7-two-t2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("two.chz");
    std::fs::write(&path, program).expect("write fixture");

    for run in 0..5 {
        let out = hang_deadline::run_with_hang_deadline(&path, Some("2"));
        let Some(out) = out else {
            let _ = std::fs::remove_dir_all(&dir);
            panic!("CHEZZI_THREADS=2 run {run}: no exit within 10s");
        };
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "CHEZZI_THREADS=2 run {run}: expected rc=0, got {:?} — stdout: {stdout} stderr: {stderr}",
            out.status
        );
        let mut lines: Vec<&str> = stdout.lines().collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            vec!["done", "j1 got 1", "j2 got 2"],
            "CHEZZI_THREADS=2 run {run}: got stdout: {stdout:?} stderr: {stderr:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
