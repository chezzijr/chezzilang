//! TICKET-205 — at `CHEZZI_THREADS=1` an Executor job in a CPU loop starves a job of a second
//! Executor until the loop ends. `os.exit(3)` from the second job must land within a bounded time
//! (Go at `GOMAXPROCS=1` exits at ~100 ms).
//!
//! Spawns and polls: a starved child still runs, so the test kills it at the deadline.

use std::process::Command;

const PROGRAM: &str = r#"import std.time
import std.os
import std.concurrency
fn spin():
    i := 0
    while i < 400000000:
        i = i + 1
fn exiter():
    time.sleep_ms(100)
    os.exit(3)
ex := Executor()
ex.submit(spin)
ex2 := Executor()
ex2.submit(exiter)
time.sleep_ms(5000)
print("after join")
"#;

#[test]
fn executor_job_spin_does_not_starve_second_executor_job_at_t1() {
    let dir = std::env::temp_dir().join(format!("chz-ticket205-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("j3n.chz");
    std::fs::write(&path, PROGRAM).expect("write fixture");

    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn chezzi");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(st) => break Some(st),
            None if std::time::Instant::now() >= deadline => break None,
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    };
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = std::fs::remove_dir_all(&dir);
    let Some(status) = status else {
        panic!("executor spin starved the second executor's os.exit at one worker");
    };
    assert_eq!(status.code(), Some(3));
}
