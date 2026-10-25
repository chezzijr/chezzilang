//! A `chezzi test <file>` run under `CHEZZI_GC_STRESS=1` (every instruction boundary requests a
//! collection), bounded by a deadline. A value that a collection should keep but frees reads back
//! as a `dangling GcRef` panic or a reused slot, so the run fails. A run that outlives the bound is
//! killed and FAILS. It is never retried. Include it with
//! `#[path = "support/stressed_chz.rs"] mod stressed_chz;`.

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The bound on one run. A stressed run of a spec file takes seconds in a debug build.
const LIMIT: Duration = Duration::from_secs(120);

/// Run `file` (relative to the crate root) at `threads` workers, at scheduler seed `seed` (`None`:
/// unseeded), and assert that every test in it passes.
pub fn assert_stressed_pass(file: &str, threads: &str, seed: Option<&str>) {
    let label = format!("{file} T={threads} seed={seed:?}");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["test", file])
        .env("CHEZZI_GC_STRESS", "1")
        .env("CHEZZI_THREADS", threads)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match seed {
        Some(n) => {
            cmd.env("CHEZZI_SCHED_SEED", n);
        }
        None => {
            cmd.env_remove("CHEZZI_SCHED_SEED");
        }
    }
    let mut child = cmd.spawn().expect("spawn chezzi");
    let deadline = Instant::now() + LIMIT;
    let status = loop {
        if let Some(st) = child.try_wait().expect("try_wait") {
            break st;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{label}: no exit within {LIMIT:?} under CHEZZI_GC_STRESS=1");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (mut out, mut err) = (String::new(), String::new());
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut out);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    assert!(
        status.success() && out.contains(" 0 failed, 0 errored") && !out.contains("file error"),
        "{label}: exit {status}\nstdout:\n{out}\nstderr:\n{err}"
    );
}
