//! §2c1 — a bounded-channel RENDEZVOUS across the nursery boundary must complete.
//!
//! The `parallel:` body blocks on a channel while a `spawn`ed sibling works the other end. Each side
//! is the other's only way forward, so each must be visible to the verdict that judges the other:
//! the body is a counted party in `quiesce`, the sibling is a fiber in an `MnSched`, and neither
//! bookkeeping saw the other until eager start put them in the same program at the same time.
//!
//! **This lives in `tests/`, driving the real binary, because the in-process harness cannot see the
//! bug at all.** `run_capture` runs with the BUFFERED stdout sink, which serialises the two
//! sides differently: with the fix reverted, both programs below pass 12 runs in 12 there. Through
//! the CLI they fail.
//!
//! **Stated plainly: at `cargo test` this is a SMOKE CHECK, not a strong guard.** `CARGO_BIN_EXE_chezzi`
//! is the DEBUG binary, and the window is release-speed-only — with `SchedCore::body_waits` reverted,
//! measured: RELEASE 7/10 and 3/10 runs faulted, DEBUG 0/20 and 1/20, and raising the exchange count
//! to 200 did not move debug off 0/10. So a green run here does not prove the fix present; what it
//! does prove is that the shipping semantics are right on every run it makes, and it fails loudly if
//! the shape regresses to the PRE-§2c1 behaviour (which faulted 6 runs in 6, on both engines, in
//! debug and release alike). The red evidence for the fix itself is the release measurement above,
//! reproduced by reverting the one `if` in `MnSched::is_deadlocked_ignoring_jobs`.
//!
//! **The rendezvous needs real worker threads.** On the since-removed cooperative engine a spawned
//! task did not start until the nursery's join,
//! so a blocking body can never reach it and the program faults by design (`EMPTY_RECV_DEADLOCK`'s
//! own hint says so). Pre-§2c1 BOTH engines faulted these, 6 runs in 6.
//!
//! **Looped, because both failures were RACES**, single-sample-green in either direction:
//! - the verdict must see that the blocked body is the parked fiber's rendezvous partner — missing
//!   that faulted `recv on an empty channel: deadlock` 4 runs in 8;
//! - it must see which DIRECTION the body waits in. The pre-existing demoted-queue peek asks the
//!   receiver's question (`!q.is_empty()`), which is inverted for a body blocked on a full `send`;
//!   missing that killed the live consumer after one `recv` and faulted `send on a full channel`
//!   12 runs in 12.

use std::process::Command;

/// Run `src` through the real binary on the default (M:N) engine and return stdout, asserting the
/// run succeeded. `label` names the shape in the failure message; `i` is the loop iteration, because
/// which iteration first fails is the only useful thing to know about a race.
fn run_ok(dir: &std::path::Path, name: &str, src: &str, label: &str, i: usize) -> String {
    let path = dir.join(name);
    std::fs::write(&path, src).expect("write program");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .output()
        .expect("run chezzi");
    assert!(
        out.status.success(),
        "{label} failed on iteration {i}: status {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn bounded_channel_rendezvous_across_the_nursery_boundary_completes() {
    let dir = std::env::temp_dir().join(format!("chz-rendezvous-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");

    // Body RECEIVES, sibling sends.
    let recv_in_body = "fn main():\n    ch := Channel[int](1)\n    parallel:\n        spawn:\n            ch.send(0)\n            ch.send(1)\n        print(ch.recv())\n        print(ch.recv())\nmain()\n";
    // Body SENDS, sibling receives — the direction-inverted case.
    let send_in_body = "fn main():\n    ch := Channel[int](1)\n    parallel:\n        spawn:\n            for _ in 0..5:\n                print(\"got {ch.recv()}\")\n        for i in 0..5:\n            ch.send(i)\nmain()\n";

    for i in 0..10 {
        assert_eq!(
            run_ok(&dir, "recv_in_body.chz", recv_in_body, "recv-in-body", i),
            "0\n1\n"
        );
        // The sibling's lines are the whole transcript here; `chezzi run` streams, so ORDER between
        // the two sides is not asserted — only that every value crossed.
        let out = run_ok(&dir, "send_in_body.chz", send_in_body, "send-in-body", i);
        let mut lines: Vec<&str> = out.lines().collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            vec!["got 0", "got 1", "got 2", "got 3", "got 4"],
            "send-in-body lost a value on iteration {i}: {out:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// The other half of the contract: a GENUINE deadlock across the same boundary must still FAULT, not
/// hang. Both sides `recv`, nobody sends. Guards the direction the rendezvous fix could over-correct
/// in — a veto generous enough to save the healthy program must not also save this one.
#[test]
fn a_genuine_rendezvous_deadlock_still_faults() {
    let dir = std::env::temp_dir().join(format!("chz-rendezvous-dl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("both_recv.chz");
    std::fs::write(
        &path,
        "fn main():\n    ch := Channel[int]()\n    parallel:\n        spawn:\n            _ := ch.recv()\n        _ := ch.recv()\nmain()\n",
    )
    .expect("write program");

    for i in 0..5 {
        let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .arg("run")
            .arg(&path)
            .output()
            .expect("run chezzi");
        assert!(
            !out.status.success(),
            "a genuine both-recv deadlock must fault, not succeed (iteration {i})"
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("deadlock"),
            "expected a deadlock fault on iteration {i}, got: {err}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
