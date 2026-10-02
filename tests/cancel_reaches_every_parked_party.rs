//! TICKET-200 (Family B3): a cancel must reach a socket-parked fiber of a job's nursery, and an
//! unjoined job fault that precedes a `--timeout` must be reported, not dropped.

use std::process::Command;

#[path = "support/hang_deadline.rs"]
mod hang_deadline;

const NET_PRELUDE: &str = "import std.time\nimport std.net\nimport std.concurrency\nfn lis() -> Listener:\n    match net.listen(\"127.0.0.1:0\"):\n        Ok(l): return l\n        Err(e): panic(\"{e}\")\n";

#[test]
fn shutdown_now_reaches_a_job_nursery_fiber_parked_in_accept() {
    let dir = std::env::temp_dir().join(format!("chz-t200-accept-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("n5.chz");
    let body = "ln := lis()\nfn srv():\n    defer:\n        print(\"srv defer\")\n    parallel:\n        spawn:\n            defer:\n                print(\"fiber defer\")\n            r := ln.accept()\n            print(\"accept returned {r}\")\nex := Executor()\nex.submit(srv)\ntime.sleep_ms(100)\nex.shutdown_now()\nprint(\"after shutdown_now\")\n";
    std::fs::write(&path, format!("{NET_PRELUDE}{body}")).expect("write program");
    let out = hang_deadline::run_with_hang_deadline(&path, Some("1"));
    let _ = std::fs::remove_dir_all(&dir);
    let out = out.expect("shutdown_now hung: no exit within 10s");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        stdout.contains("fiber defer") && stdout.contains("after shutdown_now"),
        "shutdown_now must cut the accept-parked fiber and run its defers: {stdout:?}"
    );
}

#[test]
fn unjoined_job_fault_before_timeout_is_reported() {
    let dir = std::env::temp_dir().join(format!("chz-t200-rank-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("y2_test.chz");
    std::fs::write(
        &path,
        "import std.time\nimport std.concurrency\nfn bad():\n    xs := [1]\n    print(xs[3])\ntest fn unjoined_then_times_out():\n    ex := Executor()\n    ex.submit(bad)\n    time.sleep_ms(3000)\n",
    )
    .expect("write program");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["test", "--timeout=500"])
        .arg(&path)
        .output()
        .expect("run chezzi test");
    let _ = std::fs::remove_dir_all(&dir);
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        all.contains("index"),
        "the earlier unjoined job fault must be reported, not only TIMED-OUT: {all:?}"
    );
}
