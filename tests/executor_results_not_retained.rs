//! W7-27 — an `Executor` job's RETURN VALUE is not retained (`submit` returns nil; Chezzi has no
//! futures, and `reduce_task_slots` reads only `out`/`stderr`). Guarded with `--max-heap`: a retained
//! result backlog is what the cap sees, so 300 discarded ~1 MB job results must stay a `PASS` under an
//! 8 MB cap.
//!
//! **Lives in `tests/` — its own process — on purpose, not in `test_runner.rs`'s `mod tests`.**
//! `vm::pool` is ONE process-wide `OnceLock`, shared by every test in whatever binary it runs in. At
//! an 8 MB cap and ~1 MB per job, as few as 8 of this test's OWN 300 submissions sitting
//! queued-but-not-yet-dispatched trips the cap — and that is genuine, correctly-accounted memory
//! (`ExecutorCore::pending`), not a measurement bug (see the `EXEC_MEM_CAP_LOCK` doc in
//! `test_runner.rs` for the sibling family this pairs with). Measured: run alone, this test passes
//! 100% (13/13, `--test-threads=1` and solo `--exact`); run inside the lib unit-test binary
//! alongside its ~65 `executor`-named siblings (`RUST_TEST_THREADS=4`), it failed **10/10**, and
//! widening the cap only masked it — even 4× (32 MB) still failed 10/10, and it did not clear until
//! the cap was within shouting distance of the FULL backlog (350 MB), at which point a genuine
//! retention regression could no longer trip it either. A queued-but-undispatched backlog and a
//! retained-result backlog are BOTH `job_count × payload_size`, so no cap value or payload size run
//! in that shared process can tell them apart while they race — the cap is fundamentally
//! contention-dependent there. `cargo test`'s default is to run test BINARIES one at a time (never
//! two targets' bodies executing concurrently, only the `#[test]` fns WITHIN one binary run
//! concurrently), so a dedicated integration-test binary gives this program the exclusive access to
//! `vm::pool` its 8 MB margin assumes, without weakening the cap or lock-stepping it against every
//! other `Executor` test in the tree.

use std::path::Path;

fn write_test_file(name: &str, contents: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("chz-execret-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(name);
    std::fs::write(&path, contents).expect("write test file");
    path
}

#[test]
fn executor_results_are_not_retained() {
    const CAP: usize = 8_000_000;
    // ~1 MB blob, captured (so each submit wires it and paces the parent's sweeps) and RETURNED by
    // 300 jobs. Nothing reads those 300 MB, so nothing may hold them.
    let path = write_test_file(
        "ret_test.chz",
        "import std.concurrency\n\ntest fn execret():\n    parts: List[str] = []\n    \
         for i in range(100000):\n        parts.push(\"0123456789\")\n    \
         blob := \"\".join(parts)\n    ex := Executor()\n    for i in range(300):\n        \
         ex.submit(fn() -> str: blob)\n    ex.shutdown()\n    assert true\n",
    );
    let report = chezzi::test_runner::run_tests_capped(&path, CAP);
    assert!(
        report.text.contains("PASS execret"),
        "300 discarded ~1 MB job results must not be retained; report:\n{}",
        report.text
    );
    let _ = std::fs::remove_dir_all(Path::new(&path).parent().unwrap());
}
