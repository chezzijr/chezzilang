//! TICKET-236 (wave 22 family B): a task parked on an Executor job's result channel is declared
//! deadlocked while `shutdown_now()` cuts that job. The handle settles every round, so the
//! verdict is false. Ancestor: CPython `ThreadPoolExecutor` completes all 300 rounds.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn nursery_reader_of_cut_job_is_not_a_false_deadlock() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/executor_false_deadlock.chz"
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(path)
        .env("CHEZZI_THREADS", "2")
        .env_remove("CHEZZI_SCHED_SEED")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    let mut stderr = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s);
        }
        if start.elapsed() > Duration::from_secs(60) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let err = reader.join().expect("stderr reader");
    assert!(status.is_some_and(|s| s.success()), "run failed: {err}");
}
