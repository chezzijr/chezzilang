//! TICKET-152 (W14-37) — a FATAL deadlock reports directly and runs no `defer`, like Go's
//! `fatal error: all goroutines are asleep - deadlock!`. Before the fix the uncaught path still
//! unwound the frames, so `main`'s `defer` printed on the way out.

use std::process::Command;

#[test]
fn fatal_deadlock_runs_no_defer() {
    let dir = std::env::temp_dir().join(format!("chz-ticket152-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("w37.chz");
    let src = [
        "fn main():",
        "    defer:",
        "        print(\"main defer ran\")",
        "    ch := Channel[int]()",
        "    print(ch.recv())",
        "main()",
        "",
    ];
    std::fs::write(&path, src.join("\n")).expect("write fixture");
    for threads in [Some("1"), Some("2"), None] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
        cmd.arg("run").arg(&path);
        match threads {
            Some(n) => cmd.env("CHEZZI_THREADS", n),
            None => cmd.env_remove("CHEZZI_THREADS"),
        };
        let out = cmd.output().expect("run chezzi");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(1),
            "threads={threads:?} stderr: {stderr}"
        );
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
