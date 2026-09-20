//! TICKET-152 (W14-37) — a FATAL deadlock reports directly and runs no `defer`, like Go's
//! `fatal error: all goroutines are asleep - deadlock!`. Before the fix the uncaught path still
//! unwound the frames, so `main`'s `defer` printed on the way out.

use std::process::Command;

/// Write `src` (joined by newlines) to a fixture named `name` and run it through the built
/// `chezzi` binary at `threads` workers (`None` = the default). Returns (stdout, stderr, code).
fn run_at(name: &str, src: &[&str], threads: Option<&str>) -> (String, String, Option<i32>) {
    let dir = std::env::temp_dir().join(format!("chz-ticket152-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join(name);
    std::fs::write(&path, src.join("\n")).expect("write fixture");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run").arg(&path);
    match threads {
        Some(n) => cmd.env("CHEZZI_THREADS", n),
        None => cmd.env_remove("CHEZZI_THREADS"),
    };
    let out = cmd.output().expect("run chezzi");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

const WORKER_COUNTS: [Option<&str>; 3] = [Some("1"), Some("2"), None];

#[test]
fn fatal_deadlock_runs_no_defer() {
    let src = [
        "fn main():",
        "    defer:",
        "        print(\"main defer ran\")",
        "    ch := Channel[int]()",
        "    print(ch.recv())",
        "main()",
        "",
    ];
    for threads in WORKER_COUNTS {
        let (stdout, stderr, code) = run_at("w37.chz", &src, threads);
        assert_eq!(code, Some(1), "threads={threads:?} stderr: {stderr}");
        assert!(
            stderr.contains("deadlock"),
            "threads={threads:?} stderr: {stderr}"
        );
        assert!(
            !stdout.contains("main defer ran"),
            "a fatal deadlock must run no defer (threads={threads:?}), stdout: {stdout:?}"
        );
    }
}

#[test]
fn nursery_verdict_deadlock_runs_no_defer() {
    let src = [
        "fn worker(ch: Channel[int]):",
        "    defer:",
        "        print(\"worker defer ran\")",
        "    print(ch.recv())",
        "fn main():",
        "    defer:",
        "        print(\"main defer ran\")",
        "    ch := Channel[int]()",
        "    parallel:",
        "        spawn: worker(ch)",
        "        spawn: worker(ch)",
        "    print(\"after\")",
        "main()",
        "",
    ];
    for threads in WORKER_COUNTS {
        let (stdout, stderr, code) = run_at("nursery_verdict.chz", &src, threads);
        assert_eq!(code, Some(1), "threads={threads:?} stderr: {stderr}");
        assert!(
            stderr.contains("deadlock: every task in this parallel: block is blocked"),
            "threads={threads:?} stderr: {stderr}"
        );
        assert_eq!(
            stdout, "",
            "no task may run a defer on a fatal deadlock (threads={threads:?})"
        );
    }
}

#[test]
fn an_ordinary_uncaught_fault_still_runs_its_defer() {
    let src = [
        "fn main():",
        "    defer:",
        "        print(\"main defer ran\")",
        "    xs := [1]",
        "    print(xs[9])",
        "main()",
        "",
    ];
    for threads in WORKER_COUNTS {
        let (stdout, stderr, code) = run_at("ordinary_fault.chz", &src, threads);
        assert_eq!(code, Some(1), "threads={threads:?} stderr: {stderr}");
        assert!(
            stderr.contains("index 9 out of bounds"),
            "threads={threads:?} stderr: {stderr}"
        );
        assert_eq!(
            stdout, "main defer ran\n",
            "an ordinary fault keeps its defer (threads={threads:?})"
        );
    }
}
