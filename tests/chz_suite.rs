//! The `tests/chz` dogfood gate: `chezzi test tests/chz/<dir>` on the M:N VM, one test per subdirectory.
//!
//! `vm::pool` is one process-wide `OnceLock`, and `sleep_cancel_test.chz` asserts `d < 1.0s` on how
//! fast a cancelled job's worker runs, which failed 8 of 8 runs when other tests shared the pool.
//! Since TICKET-276 each directory runs the `chezzi` binary as its own process, because `cargo test`
//! runs a binary's tests on parallel threads that would share one pool.

#[path = "support/chz_dirs.rs"]
mod chz_dirs;
#[path = "support/grid_tests.rs"]
#[macro_use]
mod grid_tests;

grid_tests!(CHZ_DIRS in chz_suite_passes: [spec, stdlib, suites] => crate::assert_chz_dir);

/// One `tests/chz` directory through `chezzi test`, in its own process and its own pool.
fn assert_chz_dir(dir: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/chz")
        .join(dir);
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("test").arg(&path).env_remove("CHEZZI_THREADS");
    let out = chz_dirs::set_build_mode(&mut cmd)
        .output()
        .expect("run chezzi test");
    assert!(
        out.status.success(),
        "tests/chz/{dir} must pass on the M:N VM; report:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every subdirectory of `tests/chz` has a row in [`CHZ_DIRS`], and no `.chz` file sits outside one.
#[test]
fn every_tests_chz_subdirectory_has_a_test() {
    chz_dirs::assert_every_subdirectory_listed(CHZ_DIRS);
}
