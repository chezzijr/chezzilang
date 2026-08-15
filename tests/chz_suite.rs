//! Task 7b — `tests/chz` dogfood gate (`chezzi test` on the M:N VM), moved out of the lib unit-test
//! binary into its own process. Was `test_runner::tests::chz_suite_passes`.
//!
//! **Lives in `tests/`, in its own process — same reason as `executor_results_not_retained.rs` /
//! `executor_reentrant_shutdown.rs` / `chezzi_threads_cli.rs`.** `vm::pool` is ONE process-wide
//! `OnceLock`. Under `cargo test --lib` (`RUST_TEST_THREADS=4`) this test ran concurrently with
//! ~4150 unrelated lib tests all contending for that one pool, and one `tests/chz` case,
//! `shutdown_now_interrupts_a_sleeping_job` (`tests/chz/stdlib/sleep_cancel_test.chz:45`), asserts a
//! wall-clock bound (`d < 1.0s`, 3x headroom under the job's real 3s deadline) on how fast a cancelled
//! job's pool worker gets a chance to notice the cancel. Measured on this branch: **8/8 full
//! `cargo test --lib` runs failed this exact assertion**, `d` landing 1.03s–1.44s — well under the 3s
//! deadline (the cancel genuinely lands, just delayed by pool starvation past the 1.0s bound), and
//! every failure was this one test. Filtering it alone inside the same binary
//! (`--lib chz_suite_passes`) already passed in 3.5s, which is the same fix this file makes permanent:
//! cargo runs test BINARIES one at a time, so its own process gets `vm::pool` to itself with no
//! outside contention.
//!
//! No `TEST_UUID_LOCK`/`clear_seed()` dance here (unlike the in-process version this replaces): that
//! guarded against interleaving with `vm::parity_tests::golden_uuid_via_run_file`, which lives in the
//! LIB test binary. A separate integration-test binary is a separate OS process with its own copy of
//! every `static`, sequenced strictly before/after the lib binary by cargo — interleaving is no longer
//! physically possible, so the guard has nothing left to guard against here.

#[test]
fn chz_suite_passes() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/chz");
    let report = chezzi::test_runner::run_tests(&root);
    assert!(
        report.passed,
        "tests/chz must pass on the M:N VM; report:\n{}",
        report.text
    );
}
