//! TICKET-135 (W14 D1) — a deadlock is FATAL (Go's `all goroutines are asleep`), not
//! `recover:`-able. Boundary: value/operation faults recover; scheduler / whole-program faults
//! (deadlock, `os.exit`, resource caps) do not. Before the fix `recover:` turns the verdict into an
//! `Err` and the program keeps running.

use std::process::{Command, Output};

/// The one deadlock header (plan 2026-11-17): every task of the run is blocked.
const HEADER: &str = "deadlock: every task is blocked";

/// Write `src` to a fixture file and `chezzi run` it, optionally pinning `CHEZZI_THREADS`.
fn run_fixture(name: &str, src: &[&str], threads: Option<&str>) -> Output {
    let dir = std::env::temp_dir().join(format!("chz-ticket135-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join(name);
    std::fs::write(&path, src.join("\n")).expect("write deadlock fixture");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run").arg(&path);
    match threads {
        Some(n) => cmd.env("CHEZZI_THREADS", n),
        None => cmd.env_remove("CHEZZI_THREADS"),
    };
    cmd.output().expect("run chezzi")
}

fn assert_fatal_deadlock(out: &Output, forbidden_stdout: &str, what: &str) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains(forbidden_stdout),
        "{what}: a deadlock verdict must abort the program, but `recover:` caught it (stdout: {stdout:?})"
    );
    assert!(
        out.status.code() == Some(1) && stderr.contains(HEADER),
        "{what}: expected a fatal `{HEADER}` at rc 1, got {} (stderr: {stderr})",
        out.status
    );
}

// Go twin of each fixture: fatal `all goroutines are asleep - deadlock!`; `recover()` cannot catch
// a fatal error.
#[test]
fn recover_does_not_catch_a_deadlock() {
    let out = run_fixture(
        "dd135.chz",
        &[
            "ch := Channel[int](0)",
            "r := recover: ch.recv()",
            "print(\"recovered\")",
            "",
        ],
        None,
    );
    assert_fatal_deadlock(&out, "recovered", "top-level");
}

#[test]
fn a_recovered_nursery_deadlock_aborts() {
    let src = [
        "fn main():",
        "    c := Channel[int](0)",
        "    d := Channel[int](0)",
        "    r := recover:",
        "        parallel:",
        "            spawn:",
        "                c.send(1)",
        "            d.recv()",
        "    print(c.try_recv())",
        "main()",
        "",
    ];
    for threads in [Some("1"), Some("2"), None] {
        let out = run_fixture("nursery135.chz", &src, threads);
        assert_fatal_deadlock(&out, "Some(1)", &format!("nursery at threads={threads:?}"));
    }
}

#[test]
fn a_deadlocked_executor_job_aborts_shutdown() {
    let src = [
        "import Executor from std.concurrency",
        "fn main():",
        "    ch := Channel[int](0)",
        "    ex := Executor()",
        "    _ := ex.submit(fn(): ch.recv())",
        "    r := recover: ex.shutdown()",
        "    print(\"after shutdown\")",
        "main()",
        "",
    ];
    let out = run_fixture("exec135.chz", &src, None);
    assert_fatal_deadlock(&out, "after shutdown", "executor shutdown");
    // Main is task 1, listed at its `shutdown()` join; the job follows at its `recv()`.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("task 1: waiting for an Executor join at ")
            && stderr.contains("exec135.chz:6:19")
            && stderr.contains("task 2: recv() on an empty channel at "),
        "executor shutdown: main and its job must both be listed (stderr: {stderr})"
    );
}
