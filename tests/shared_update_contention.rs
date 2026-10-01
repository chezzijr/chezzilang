//! TICKET-193: contended `Shared.update` must not be pathologically slow at small worker counts.
//! 6 tasks x 2000 `s.update(inc)` measured 0.03 s at T=1 but 15.6-19.6 s at T=2 on the release
//! binary; Go (`sync.Mutex`, GOMAXPROCS=2) does the same work in 0.007 s.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SRC: &str = "import std.concurrency
fn inc(n: int) -> int:
    return n + 1
fn main():
    s := Shared[int](0)
    parallel:
        for _ in range(6):
            spawn:
                for _ in range(2000):
                    s.update(inc)
    print(s.get())
main()
";

/// Runs `SRC` at `threads` workers; returns (stdout, wall time). Kills the run after 40 s.
fn run_at(threads: &str) -> (String, Duration) {
    let dir = std::env::temp_dir().join(format!("chz-t193-{threads}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("su.chz");
    std::fs::write(&path, SRC).expect("write program");
    let start = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", threads)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn chezzi");
    while child.try_wait().expect("wait").is_none() && start.elapsed() < Duration::from_secs(40) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let out = child.wait_with_output().expect("output");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        start.elapsed(),
    )
}

#[test]
fn contended_shared_update_is_fast_at_every_worker_count() {
    for threads in ["1", "2", "4", "0"] {
        let (out, took) = run_at(threads);
        assert_eq!(
            out.trim(),
            "12000",
            "CHEZZI_THREADS={threads}: wrong final value"
        );
        assert!(
            took < Duration::from_secs(3),
            "CHEZZI_THREADS={threads}: 12000 contended Shared.update took {took:?}, ceiling 3s"
        );
    }
}
