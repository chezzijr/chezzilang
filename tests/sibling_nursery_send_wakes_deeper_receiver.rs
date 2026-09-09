//! TICKET-099 — a `send` from a sibling eager nursery never wakes a receiver parked in a DEEPER
//! nursery. `MnSched::parent_wake` only walks UPWARD (child sched to parent sched), so a sibling's
//! `send_wake` can't reach a receiver parked on another sibling's private sched. The receiving task
//! CAN proceed (a sibling is ready to send), but the nursery spuriously faults `deadlock` instead.
//! Measured on the release binary at `2be17751`: 30/30 fault at `CHEZZI_THREADS=8`, 0/30 at `=1`.

use std::process::Command;

#[test]
fn sibling_send_wakes_receiver_in_a_deeper_nursery_at_eight_workers() {
    for threads in ["1", "2", "4", "8"] {
        let dir =
            std::env::temp_dir().join(format!("chz-ticket099-{}-{}", std::process::id(), threads));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let path = dir.join("h3.chz");
        std::fs::write(
            &path,
            "ch := Channel[int](0)\n\
             fn f():\n    \
                 spawn:\n        \
                     print(\"got {ch.recv()}\")\n\
             parallel:\n    \
                 spawn f()\n    \
                 spawn:\n        \
                     ch.send(42)\n",
        )
        .expect("write sibling-nursery repro fixture");

        // This is a RACE, not a deterministic fault: whether the sibling's `send` beats the deeper
        // receiver's `park` depends on scheduling, so a single run can pass even on a buggy binary
        // under load (measured: exits 0 on base `main` under load average ~1.4, 40/40 fails on an idle
        // box). Sample the race N times and fail on the FIRST loss rather than asking one run to lose
        // it — this reports a RATE, matching the ticket's own worker-count sampling methodology.
        const ITERATIONS: u32 = 20;
        for i in 0..ITERATIONS {
            // Spawn-and-poll under a wall-clock deadline rather than `.output()`: a hung child (the
            // pre-fix peer-veto direction) never closes its pipes, so `.output()` would wedge this
            // test binary instead of failing it.
            let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
                .arg("run")
                .arg(&path)
                .env("CHEZZI_THREADS", threads)
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
                    "CHEZZI_THREADS={threads} iteration {}/{ITERATIONS}: chezzi hung for 10s \
                     instead of returning",
                    i + 1
                );
            };
            let mut stdout = String::new();
            let mut stderr = String::new();
            use std::io::Read as _;
            if let Some(mut o) = child.stdout.take() {
                let _ = o.read_to_string(&mut stdout);
            }
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut stderr);
            }
            assert!(
                status.success(),
                "CHEZZI_THREADS={threads} iteration {}/{ITERATIONS}: expected rc=0 and `got 42` (a \
                 sibling sender can unblock the deeper receiver), got status {status} (stdout: \
                 {stdout}, stderr: {stderr})",
                i + 1,
            );
            assert!(
                stdout.contains("got 42"),
                "CHEZZI_THREADS={threads} iteration {}/{ITERATIONS}: expected `got 42` on stdout, \
                 got: {stdout}",
                i + 1
            );
            assert!(
                !stderr.contains("deadlock:"),
                "CHEZZI_THREADS={threads} iteration {}/{ITERATIONS}: expected no `deadlock:` fault \
                 (the receiver CAN proceed, a sibling CAN unblock it), got: {stderr}",
                i + 1
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Shape B — an ANCESTOR sends after a delay, to a receiver already parked in a DEEPER nursery.
/// `MnSched::is_deadlocked_ignoring_jobs` reads only its own `SchedCore`, so the deeper sched
/// quiesces and faults `deadlock` immediately, long before the sender's 300ms sleep elapses and the
/// send exists. This is deterministic (not a race): the receiver is parked well before the send is
/// even reachable. Measured on the release binary at `2be17751`: faults `deadlock:` at every worker
/// count above one.
#[test]
fn an_ancestor_send_wakes_a_receiver_parked_in_a_deeper_nursery() {
    for threads in ["1", "2", "4", "8"] {
        let dir =
            std::env::temp_dir().join(format!("chz-ticket099b-{}-{}", std::process::id(), threads));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let path = dir.join("h3b.chz");
        std::fs::write(
            &path,
            "import std.time\n\
             ch := Channel[int](0)\n\
             fn feed(c: Channel[int]):\n    \
                 time.sleep_ms(300)\n    \
                 c.send(1)\n\
             parallel:\n    \
                 spawn feed(ch)\n    \
                 spawn:\n        \
                     parallel:\n            \
                         spawn:\n                \
                             print(\"inner got \" + str(ch.recv()))\n",
        )
        .expect("write ancestor-send repro fixture");

        let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .arg("run")
            .arg(&path)
            .env("CHEZZI_THREADS", threads)
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
            panic!("CHEZZI_THREADS={threads}: chezzi hung for 10s instead of returning");
        };
        let mut stdout = String::new();
        let mut stderr = String::new();
        use std::io::Read as _;
        if let Some(mut o) = child.stdout.take() {
            let _ = o.read_to_string(&mut stdout);
        }
        if let Some(mut e) = child.stderr.take() {
            let _ = e.read_to_string(&mut stderr);
        }
        assert!(
            status.success(),
            "CHEZZI_THREADS={threads}: expected rc=0 and `inner got 1` (an ancestor sender can \
             unblock a deeper receiver), got status {status} (stdout: {stdout}, stderr: {stderr})",
        );
        assert!(
            stdout.contains("inner got 1"),
            "CHEZZI_THREADS={threads}: expected `inner got 1` on stdout, got: {stdout}"
        );
        assert!(
            !stderr.contains("deadlock:"),
            "CHEZZI_THREADS={threads}: expected no `deadlock:` fault (the receiver CAN proceed, the \
             ancestor's delayed send CAN unblock it), got: {stderr}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
