//! M1.8 — a value that a blocking door holds in a Rust local survives a collection on another
//! thread. `tests/chz/spec/gc_blocking_pins_test.chz` has one test per door kind; this runs it with
//! `CHEZZI_GC_STRESS=1` (every instruction boundary requests a collection) at 2 workers, unseeded
//! and at 5 scheduler seeds. A value the door did not pin is freed by another thread's sweep, and
//! its next read panics `dangling GcRef` or reads a reused slot.
//!
//! A run that outlives the bound is killed and FAILS. It is never retried.

#[path = "support/grid_tests.rs"]
#[macro_use]
mod grid_tests;

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The bound on one run. A stressed run takes about 6 s in a debug build.
const LIMIT: Duration = Duration::from_secs(120);

const FILE: &str = "tests/chz/spec/gc_blocking_pins_test.chz";

fn assert_seed(seed: &str) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["test", FILE])
        .env("CHEZZI_GC_STRESS", "1")
        .env("CHEZZI_THREADS", "2")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match seed.strip_prefix('s') {
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
            panic!("{seed}: no exit within {LIMIT:?} under CHEZZI_GC_STRESS=1");
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
        "{seed}: exit {status}\nstdout:\n{out}\nstderr:\n{err}"
    );
}

grid_tests!(SEEDS in seed: [unseeded, s1, s2, s3, s4, s5] => crate::assert_seed);
