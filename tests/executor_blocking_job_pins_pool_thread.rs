//! TICKET-052: an eager `Executor` job that blocks (on a `Channel`, a `Shared`, or
//! `time.sleep_ms`) hands its pool thread to a replacement worker and retires when it ends, so
//! every shape below completes at any worker count including one.
//!
//! Subprocess-only (see `executor_reentrant_shutdown.rs`'s module doc): the pool is one
//! process-wide `OnceLock`, sized once per process, so a genuine hang here must not be able to
//! wedge the `cargo test --lib` binary or starve unrelated tests sharing its pool.

use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

/// Write `src` into a per-test temp dir, run it, and return (status, stdout, wall time). `status`
/// is `None` when the child had to be killed after `secs`. `threads` sets `CHEZZI_THREADS`
/// (`None` = default worker count).
fn run(src: &str, threads: Option<&str>, secs: u64) -> (Option<ExitStatus>, String, Duration) {
    let dir = std::env::temp_dir().join(format!(
        "chz-executor-blocking-pins-thread-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(&path, src).expect("write program");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run").arg(&path);
    if let Some(t) = threads {
        cmd.env("CHEZZI_THREADS", t);
    } else {
        cmd.env_remove("CHEZZI_THREADS");
    }
    let start = Instant::now();
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn chezzi");

    let deadline = Instant::now() + Duration::from_secs(secs);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let elapsed = start.elapsed();
    let output = child
        .wait_with_output()
        .unwrap_or_else(|_| std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let _ = std::fs::remove_dir_all(&dir);
    (status, stdout, elapsed)
}

#[test]
fn executor_consumer_then_producer_completes_at_one_worker() {
    let src = "import std.concurrency\nex := Executor()\nch := Channel[int]()\nex.submit(fn(): print(ch.recv()))\nex.submit(fn(): ch.send(7))\nex.shutdown()\nprint(\"ok\")\n";
    let (status, stdout, _) = run(src, Some("1"), 15);
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
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines, vec!["7", "ok"], "stdout: {stdout:?}");
}

#[test]
fn thirteen_consumers_and_one_feeder_complete_at_the_default_worker_count() {
    let src = "import std.concurrency\nch := Channel[int]()\nfn worker():\n    print(ch.recv())\nfn feeder():\n    for i in 0..13:\n        ch.send(i)\nex := Executor()\nfor _ in 0..13:\n    ex.submit(worker)\nex.submit(feeder)\nex.shutdown()\nprint(\"ok\")\n";
    let (status, stdout, _) = run(src, None, 30);
    let status = status.unwrap_or_else(|| {
        panic!(
            "13 consumers plus one feeder must not hang the whole program at the default worker \
             count: killed after 30s timeout instead of exiting"
        )
    });
    assert!(status.success(), "must exit 0: status {:?}", status.code());
    assert_eq!(
        stdout.lines().last(),
        Some("ok"),
        "stdout's last line must be ok: {stdout:?}"
    );
}

#[test]
fn nested_executors_complete_deeper_than_the_worker_count() {
    let src = "import std.concurrency\nfn level(n: int):\n    if n == 0:\n        print(\"bottom\")\n        return\n    ex := Executor()\n    ex.submit(fn(): level(n - 1))\n    ex.shutdown()\nlevel(13)\nprint(\"ok\")\n";
    let (status, stdout, _) = run(src, Some("1"), 30);
    let status = status.unwrap_or_else(|| {
        panic!(
            "nested Executors 13 deep must not hang at CHEZZI_THREADS=1: killed after 30s \
             timeout instead of exiting"
        )
    });
    assert!(status.success(), "must exit 0: status {:?}", status.code());
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines, vec!["bottom", "ok"], "stdout: {stdout:?}");
}

#[test]
fn a_blocked_guard_and_a_blocked_recv_do_not_starve_the_job_that_frees_them() {
    let src = "import std.concurrency\nch := Channel[int]()\nsh := Shared(0)\nfn bump(v: int) -> int:\n    return v + ch.recv()\nex := Executor()\nex.submit(fn(): sh.update(bump))\nex.submit(fn(): sh.set(5))\nex.submit(fn(): ch.send(1))\nex.shutdown()\nprint(\"ok\")\n";
    let (status, stdout, _) = run(src, Some("1"), 30);
    let status = status.unwrap_or_else(|| {
        panic!(
            "a blocked guard and a blocked recv must not starve the job that frees them at \
             CHEZZI_THREADS=1: killed after 30s timeout instead of exiting"
        )
    });
    assert!(status.success(), "must exit 0: status {:?}", status.code());
    assert_eq!(stdout.lines().last(), Some("ok"), "stdout: {stdout:?}");
}

#[test]
fn six_sleeping_jobs_overlap_at_one_worker() {
    let src = "import std.concurrency\nimport std.time\nex := Executor()\nfor _ in 0..6:\n    ex.submit(fn(): time.sleep_ms(500))\nex.shutdown()\nprint(\"ok\")\n";
    let (status, stdout, elapsed) = run(src, Some("1"), 30);
    let status = status.unwrap_or_else(|| {
        panic!(
            "six sleeping jobs must not serialize at CHEZZI_THREADS=1: killed after 30s timeout \
             instead of exiting"
        )
    });
    assert!(status.success(), "must exit 0: status {:?}", status.code());
    assert_eq!(stdout.lines().last(), Some("ok"), "stdout: {stdout:?}");
    // A blocked sleeping job must not pin its pool thread — six 500ms sleeps that overlap finish
    // well under their serialized sum (~3s). This is a wall-clock bound, not a ratio
    // (`tests/no_wall_clock_ratio_gates.rs` permits it): headroom over both the ~0.52s overlap
    // floor and the ~3.05s serialized baseline measured for this shape.
    assert!(
        elapsed < Duration::from_millis(1500),
        "six sleeping jobs at CHEZZI_THREADS=1 took {elapsed:?}, expected well under 1500ms"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn a_yielded_pool_thread_retires_instead_of_growing_the_pool() {
    let src = "import std.concurrency\nimport std.io\nimport std.time\n\nfn thread_count() -> int:\n    text := io.read_file(\"/proc/self/status\") ?? \"\"\n    for ln in text.split(\"\\n\"):\n        if ln.starts_with(\"Threads:\"):\n            parts := ln.split(\"\\t\")\n            return parts[1].trim().to_int() ?? -1\n    return -1\n\nwarmup := Executor()\nwarmup.submit(fn(): time.sleep_ms(1))\nwarmup.shutdown()\ntime.sleep_ms(300)\nprint(\"mid \" + str(thread_count()))\n\nch := Channel[int]()\nex := Executor()\nfor _ in 0..40:\n    ex.submit(fn(): ch.recv())\nfn feeder():\n    for i in 0..40:\n        ch.send(i)\nex.submit(feeder)\nex.shutdown()\ntime.sleep_ms(300)\nprint(\"end \" + str(thread_count()))\n";
    let (status, stdout, _) = run(src, None, 60);
    let status = status.unwrap_or_else(|| {
        panic!(
            "40 blocked jobs must not hang the whole program at the default worker count: \
             killed after 60s timeout instead of exiting"
        )
    });
    assert!(status.success(), "must exit 0: status {:?}", status.code());
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "stdout: {stdout:?}");
    let mid: i64 = lines[0]
        .strip_prefix("mid ")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("expected 'mid <n>', got {:?}", lines[0]));
    let end: i64 = lines[1]
        .strip_prefix("end ")
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("expected 'end <n>', got {:?}", lines[1]));
    assert!(
        end - mid <= 5,
        "a yielded pool thread must retire instead of growing the pool: mid={mid} end={end}"
    );
}
