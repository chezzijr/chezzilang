//! TICKET-114: `vm::worker_count()` sizes the M:N pool from
//! `std::thread::available_parallelism()` (`src/vm/mod.rs`), which reflects CPU **affinity**, not a
//! cgroup CPU-bandwidth quota (`cpu.max`). Confined to a quota narrower than the host's core count
//! *while something else runs in the same cgroup* (here: the `cargo test` harness process itself),
//! the pool still spawns full-box-sized, oversubscribing the quota — and D3's own
//! `vm::tests::d3_thousands_of_cpu_fibers_all_complete` (10 000 tiny CPU fibers, 60 s bound) then
//! doesn't just run slower, it HANGS to the full 60 s bound, a real stall rather than proportional
//! slowdown (a bare, cargo-free invocation of the same binary/test under the same quota finishes in
//! well under 1s — sampled 2/2; the same command run through `cargo test --lib` hangs to 60.00s
//! sampled 3/3, nested one CPUQuota scope inside another so it reproduces however tight the OUTER
//! scope already is, e.g. under the pipeline's own `run-test.sh` wrapper).
//!
//! Reproduced with a nested `systemd-run --user --scope -p CPUQuota=400%` around `cargo test --lib`
//! targeting that one test, one thread, with a 65s outer bound (5s slack over the panic's own 60s).
//! `--include-ignored` because main `#[ignore]`s that lib test until TICKET-114 moves it (7253a982):
//! without it libtest skips the test, exits 0, and this repro reads green before any fix.
//! The test pre-builds the lib test target before it opens the quota scope, so the bound times the run and never a compile.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn d3_thousands_of_fibers_does_not_hang_under_a_narrow_cpu_quota() {
    // Build the lib test target HERE, outside the quota scope and before any hog starts, in this
    // process's own environment (the one the inner `cargo test --lib` inherits), so the bound below
    // times the test run and never a compile. Measured 2026-09-11 (TICKET-114 planning): with a
    // stale lib test target the inner cargo recompiled `ring`/`rustls`/`rustls-webpki` inside the
    // hog window and hit the 65 s `timeout` (`Some(124)`) without ever starting the test.
    let build = Command::new("cargo")
        .args(["test", "--lib", "--no-run"])
        .output()
        .expect("spawn cargo test --lib --no-run");
    assert!(
        build.status.success(),
        "cargo test --lib --no-run failed before the quota run\nstderr: {}",
        String::from_utf8_lossy(&build.stderr)
    );

    let unit = format!(
        "ticket114-d3-quota-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    // Saturate the 4-core quota with unrelated CPU work FIRST (28 busy loops, matching this
    // host's core count) — "anything else runs" alongside the VM's full-box-sized pool, without
    // relying on incidental load from other processes sharing the box.
    let hogs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let cmd = format!(
        "for i in $(seq 1 {hogs}); do yes > /dev/null & done; \
         trap 'kill $(jobs -p) 2>/dev/null' EXIT; \
         timeout 65 cargo test --lib vm::tests::d3_thousands_of_cpu_fibers_all_complete -- --include-ignored --nocapture --test-threads=1"
    );
    let mut child = Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "--quiet",
            "-u",
            &unit,
            "-p",
            "CPUQuota=400%",
            "--",
            "bash",
            "-c",
            &cmd,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect(
            "spawn systemd-run --user --scope -p CPUQuota=400% -- cargo test --lib \
             (is systemd-run on PATH and is a user session available?)",
        );

    let start = Instant::now();
    // 75s outer bound: 65s inner `timeout` + headroom for `systemd-run`/`cargo` startup.
    let bound = Duration::from_secs(75);
    loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            let out = child.wait_with_output().expect("collect output");
            assert!(
                status.success(),
                "cargo test --lib vm::tests::d3_thousands_of_cpu_fibers_all_complete exited {:?} \
                 under a 4-core CPUQuota\nstdout: {}\nstderr: {}",
                status.code(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        if start.elapsed() > bound {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "10k CPU-bound fibers did not complete within {:?} under a 4-core CPUQuota \
                 (M:N pool sized to available_parallelism(), ignoring the cgroup quota)",
                bound
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
