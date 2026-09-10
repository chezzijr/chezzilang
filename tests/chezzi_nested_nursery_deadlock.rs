//! TICKET-103 (W12-1 / W12-4) — at `CHEZZI_THREADS=1`, a nested `parallel:` nursery whose OWNER
//! body blocks on a channel op is falsely `deadlock`-faulted even though a live sibling can still
//! unblock it, and a task that RECOVERS a genuine inner-nursery `deadlock` poisons the enclosing
//! nursery's bookkeeping so a later live rendezvous is also falsely reported as `deadlock`.
//! T=2/4/default: both programs complete cleanly (`docs/gaps.md` W12-1 / W12-4).

use std::process::Command;

fn run_at_threads_1(path: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(path)
        .env("CHEZZI_THREADS", "1")
        .output()
        .expect("spawn chezzi")
}

/// W12-1: the owner of a nested nursery blocks on `out.send(inner.recv() + 1)` while its own
/// child is still alive on `inner`. The join must complete (`got 2`), not fault `deadlock`.
#[test]
fn nested_nursery_owner_blocked_on_channel_op_completes_at_threads_1() {
    let dir = std::env::temp_dir().join(format!("chz-w12-1-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("owner_blocked.chz");
    std::fs::write(
        &path,
        "fn main():\n    \
             out := Channel[int](0)\n    \
             parallel:\n        \
                 spawn:\n            \
                     inner := Channel[int](0)\n            \
                     parallel:\n                \
                         spawn:\n                    \
                             inner.send(1)\n                \
                         out.send(inner.recv() + 1)\n        \
                 print(\"got {out.recv()}\")\n\
         main()\n",
    )
    .expect("write fixture");

    for round in 0..5 {
        let out = run_at_threads_1(&path);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "round {round}: expected rc=0, got {} — stderr: {stderr}",
            out.status
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("got 2"),
            "round {round}: expected \"got 2\", got stdout: {stdout} stderr: {stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// W12-4: a task recovers a genuine inner-nursery `deadlock`, then rendezvous with a sibling over
/// `out`. The exchange succeeds (`task got 1`) but the outer join then falsely faults `deadlock`
/// too — the recovered fault leaves the enclosing sched's bookkeeping stale.
#[test]
fn recovered_inner_deadlock_does_not_poison_the_enclosing_nursery_at_threads_1() {
    let dir = std::env::temp_dir().join(format!("chz-w12-4-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("recovered_poisons.chz");
    std::fs::write(
        &path,
        "fn main():\n    \
             ch := Channel[int](0)\n    \
             out := Channel[int](0)\n    \
             parallel:\n        \
                 spawn:\n            \
                     r := recover:\n                \
                         parallel:\n                    \
                             spawn:\n                        \
                                 ch.recv()\n            \
                     match r:\n                \
                         Ok(_): print(\"inner ok\")\n                \
                         Err(e): print(\"inner err\")\n            \
                     print(\"task got {out.recv()}\")\n        \
                 spawn:\n            \
                     out.send(1)\n    \
             print(\"done\")\n\
         main()\n",
    )
    .expect("write fixture");

    for round in 0..5 {
        let out = run_at_threads_1(&path);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "round {round}: expected rc=0, got {} — stderr: {stderr}",
            out.status
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("inner err")
                && stdout.contains("task got 1")
                && stdout.contains("done"),
            "round {round}: expected full completion, got stdout: {stdout} stderr: {stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
