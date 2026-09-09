//! TICKET-099 — a `send` from a sibling eager nursery never wakes a receiver parked in a DEEPER
//! nursery. `MnSched::parent_wake` only walks UPWARD (child sched to parent sched), so a sibling's
//! `send_wake` can't reach a receiver parked on another sibling's private sched. The receiving task
//! CAN proceed (a sibling is ready to send), but the nursery spuriously faults `deadlock` instead.
//! Measured on the release binary at `2be17751`: 30/30 fault at `CHEZZI_THREADS=8`, 0/30 at `=1`.

use std::process::Command;

#[test]
fn sibling_send_wakes_receiver_in_a_deeper_nursery_at_eight_workers() {
    let dir = std::env::temp_dir().join(format!("chz-ticket099-{}", std::process::id()));
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
        let output = Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .arg("run")
            .arg(&path)
            .env("CHEZZI_THREADS", "8")
            .output()
            .expect("run chezzi");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "iteration {}/{ITERATIONS}: expected rc=0 and `got 42` (a sibling sender can unblock \
             the deeper receiver), got status {} (stdout: {stdout}, stderr: {stderr})",
            i + 1,
            output.status
        );
        assert!(
            stdout.contains("got 42"),
            "iteration {}/{ITERATIONS}: expected `got 42` on stdout, got: {stdout}",
            i + 1
        );
        assert!(
            !stderr.contains("deadlock:"),
            "iteration {}/{ITERATIONS}: expected no `deadlock:` fault (the receiver CAN proceed, a \
             sibling CAN unblock it), got: {stderr}",
            i + 1
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// Shape B — an ANCESTOR sends after a delay, to a receiver already parked in a DEEPER nursery.
/// `MnSched::is_deadlocked_ignoring_jobs` reads only its own `SchedCore`, so the deeper sched
/// quiesces and faults `deadlock` immediately, long before the sender's 300ms sleep elapses and the
/// send exists. This is deterministic (not a race): the receiver is parked well before the send is
/// even reachable. Measured on the release binary at `2be17751`: faults `deadlock:` at every worker
/// count above one.
#[test]
fn an_ancestor_send_wakes_a_receiver_parked_in_a_deeper_nursery() {
    let dir = std::env::temp_dir().join(format!("chz-ticket099b-{}", std::process::id()));
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

    let output = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", "4")
        .output()
        .expect("run chezzi");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "expected rc=0 and `inner got 1` (an ancestor sender can unblock a deeper receiver), got \
         status {} (stdout: {stdout}, stderr: {stderr})",
        output.status
    );
    assert!(
        stdout.contains("inner got 1"),
        "expected `inner got 1` on stdout, got: {stdout}"
    );
    assert!(
        !stderr.contains("deadlock:"),
        "expected no `deadlock:` fault (the receiver CAN proceed, the ancestor's delayed send CAN \
         unblock it), got: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
