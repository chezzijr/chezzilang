//! TICKET-114 (W12-23): D3's soundness guard for the yield/requeue machinery -- 10 000 CPU-bound
//! fibers (each a bounded loop + one `Shared.update`), far more than the worker pool, all complete
//! under heavy yield churn: no corruption, no lost fiber, no false deadlock. Bounded loops terminate
//! regardless of preemption, so this is a soundness guard, not a fairness test.
//!
//! **Lives in `tests/`, driving the built binary at a FIXED `CHEZZI_THREADS=8`.** It used to be
//! `vm::tests::d3_thousands_of_cpu_fibers_all_complete`, run in-process through `run_capture` at
//! the lib target's default pool (every core). The debug build then hung to its 60 s bound whenever
//! other load shared the box (debug CLI, filer's table: 0.3 s at `CHEZZI_THREADS=8`, > 120 s at
//! 27/28 = nproc). `vm::pool` is one process-wide `OnceLock`, so a lib test cannot size its own
//! pool (DEC-095); a subprocess gets its own. Measured at 8 on the debug CLI under a 4-core
//! `CPUQuota` with 28 `yes` hogs: 9.96-12.80 s, 5 of 5 green.
//!
//! Spawns and polls rather than calling `output()`: a hung child never closes its pipes, so
//! `output()` would wedge this test binary instead of failing it.

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn d3_thousands_of_cpu_fibers_all_complete() {
    let program = "\
import std.concurrency

fn work(s: Shared[int]):
    i := 0
    while i < 100:
        i += 1
    s.update(fn(x): x + 1)

fn main():
    s := Shared(0)
    parallel:
        for _ in 0..10000:
            spawn work(s)
    print(s.get())

main()
";
    let dir = std::env::temp_dir().join(format!("chz-ticket114-d3-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("d3_cpu_fibers.chz");
    std::fs::write(&path, program).expect("write d3 fixture");

    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", "8")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");

    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(st) => break Some(st),
            None if Instant::now() >= deadline => break None,
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        panic!(
            "10k CPU-bound fibers did not all complete in time at CHEZZI_THREADS=8 \
             (yield machinery hang?)"
        );
    };

    let mut stdout = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut stdout);
    }
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr);
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        status.success(),
        "expected rc=0, got {status} -- stdout: {stdout} stderr: {stderr}"
    );
    assert_eq!(
        stdout, "10000\n",
        "every one of the 10 000 fibers must bump the counter exactly once; stderr: {stderr}"
    );
}
