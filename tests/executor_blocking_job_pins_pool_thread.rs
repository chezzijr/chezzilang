//! TICKET-052: an eager `Executor` job that blocks (on a `Channel`, a `Shared`, or
//! `time.sleep_ms`) does not release its pool thread the way a `spawn` task does. With
//! `CHEZZI_THREADS=1` and one consumer job queued ahead of the job that would unblock it, the
//! consumer parks holding the only pool thread, the unblocker never gets dispatched, and the
//! program hangs forever.
//!
//! Subprocess-only (see `executor_reentrant_shutdown.rs`'s module doc): the pool is one
//! process-wide `OnceLock`, sized once per process, so a genuine hang here must not be able to
//! wedge the `cargo test --lib` binary or starve unrelated tests sharing its pool.

use std::process::Command;
use std::time::Duration;

#[test]
fn executor_consumer_then_producer_completes_at_one_worker() {
    let dir = std::env::temp_dir().join(format!(
        "chz-executor-blocking-pins-thread-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(
        &path,
        "import std.concurrency\nex := Executor()\nch := Channel[int]()\nex.submit(fn(): print(ch.recv()))\nex.submit(fn(): ch.send(7))\nex.shutdown()\nprint(\"ok\")\n",
    )
    .expect("write program");

    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn chezzi");

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break Some(status);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = std::fs::remove_dir_all(&dir);

    let status = status.unwrap_or_else(|| {
        panic!(
            "an Executor consumer submitted before its producer must not hang the whole program \
             at CHEZZI_THREADS=1: killed after 15s timeout instead of exiting"
        )
    });

    assert!(
        status.success(),
        "consumer-then-producer at CHEZZI_THREADS=1 must exit 0: status {:?}",
        status.code(),
    );
}
