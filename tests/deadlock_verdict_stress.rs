//! TICKET-232 symptom 3: a sched that judges its own deadlock must latch the verdict as a run
//! halt before it cuts a victim. The spawned `update` holds the `Shared` guard and waits on a
//! channel nobody feeds; main waits on the guard. The verdict flags the spawned leaf, the leaf
//! unwinds and frees the guard, and with no halt latched main takes the guard (a ready wait
//! outranks a halt) and prints `AFTER`: it ran past the deadlock.
//!
//! The program orders the two `update` calls with a channel, not a clock. Measured on release
//! base `61a27a6b`: `AFTER` in 27 to 67 of 600 runs per worker count, 9 of 9 samples red.
//! The test makes its own load: 64 children at once.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const PROG: &str = r#"import std.concurrency
fn hold(x: int, ready: Channel[int], c: Channel[int]) -> int:
    ready.send(1)
    return x + c.recv()
fn main():
    s := Shared(0)
    c := Channel[int](0)
    ready := Channel[int](1)
    parallel:
        spawn: s.update(fn(x: int) -> int: hold(x, ready, c))
        ready.recv()
        s.update(fn(x: int) -> int: x + 1)
        print("AFTER")
main()
"#;

const RUNS: usize = 600;

/// Runs the program at `threads` workers. The flag is true when the harness killed the child at
/// ten seconds: a hang bound, not a measure.
fn run(path: &Path, threads: &str) -> (bool, Output) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(path)
        .env("CHEZZI_THREADS", threads)
        .env_remove("CHEZZI_SCHED_SEED")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    // The program prints at most one short line per stream, so the pipes cannot fill before
    // the exit.
    let start = Instant::now();
    let killed = loop {
        if child.try_wait().expect("wait").is_some() {
            break false;
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            break true;
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    (killed, child.wait_with_output().expect("output"))
}

#[test]
fn a_guard_freed_by_the_sched_verdict_never_lets_main_run_on() {
    let dir = std::env::temp_dir().join(format!("chz-t232-verdict-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(&path, PROG).expect("write program");
    let mut bad = Vec::new();
    for t in ["1", "2", "0"] {
        let next = AtomicUsize::new(0);
        let misses = Mutex::new(Vec::new());
        std::thread::scope(|sc| {
            for _ in 0..64 {
                sc.spawn(|| {
                    while next.fetch_add(1, Ordering::Relaxed) < RUNS {
                        let (killed, out) = run(&path, t);
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        if killed
                            || stdout.contains("AFTER")
                            || out.status.code() != Some(1)
                            || !stderr.contains("deadlock")
                        {
                            misses.lock().unwrap().push(format!(
                                "killed={killed} code={:?} stdout={stdout:?} stderr={stderr:?}",
                                out.status.code()
                            ));
                        }
                    }
                });
            }
        });
        let misses = misses.into_inner().unwrap();
        if let Some(first) = misses.first() {
            bad.push(format!(
                "T={t}: {} of {RUNS} runs bad; first: {first}",
                misses.len()
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        bad.is_empty(),
        "main ran past the deadlock verdict, or the run did not end as a deadlock fault:\n{}",
        bad.join("\n")
    );
}
