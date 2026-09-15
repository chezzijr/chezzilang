//! TICKET-118 — shared poll-until-exit helper for a subprocess `chezzi run` that may hang.
//!
//! `output()`/`wait_with_output()` alone can't be used: a genuinely hung child never closes its
//! stdout/stderr pipes, so those calls block forever. This spawns, polls `try_wait` on a bounded
//! deadline, and kills + returns `None` on expiry, giving a bounded "did it exit" answer.
//!
//! Lives in a plain fn outside any `#[test]` body (DEC-117, mirroring the `child_rusage.rs` /
//! `chezzi_threads_cli.rs::run_with_hang_deadline` precedent), so `tests/no_wall_clock_ratio_gates.rs`
//! lists no new test name for its poll loop.

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// Run `chezzi run <program>`, optionally forcing `CHEZZI_THREADS=<threads>` (env removed when
/// `None`). Polls every 20ms for up to 10s; `None` means the child was killed for outliving the
/// deadline.
pub fn run_with_hang_deadline(program: &Path, threads: Option<&str>) -> Option<Output> {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.args(["run", program.to_str().unwrap()]);
    match threads {
        Some(t) => {
            cmd.env("CHEZZI_THREADS", t);
        }
        None => {
            cmd.env_remove("CHEZZI_THREADS");
        }
    }
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().expect("try_wait chezzi").is_some() {
            return Some(child.wait_with_output().expect("collect chezzi output"));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
