//! The subdirectories of tests/chz: the one list each per-directory tests/chz test is checked against (TICKET-276).

#![allow(dead_code)]

/// Set to `1` for a `chezzi test` of tests/chz when the binary is a debug build. A tests/chz file
/// whose wall-clock ceiling only a release build meets reads it with `os.env` and returns early.
pub const DEBUG_BINARY_ENV: &str = "CHEZZI_TEST_DEBUG_BINARY";

/// The one place that tells a tests/chz run its binary's build mode. `CARGO_BIN_EXE_chezzi` is built
/// in this test crate's profile, so `cfg!(debug_assertions)` here is the binary's build mode. Unset
/// (a manual `chezzi test tests/chz`, or a release test build), the files assert their ceilings.
pub fn set_build_mode(cmd: &mut std::process::Command) -> &mut std::process::Command {
    if cfg!(debug_assertions) {
        cmd.env(DEBUG_BINARY_ENV, "1")
    } else {
        cmd.env_remove(DEBUG_BINARY_ENV)
    }
}

/// Fails when a subdirectory of `tests/chz` is missing from `listed`, when `listed` names a
/// directory that does not exist, or when a `.chz` file sits in `tests/chz` itself.
pub fn assert_every_subdirectory_listed(listed: &[&str]) {
    let root = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/chz"));
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(root).expect("read tests/chz") {
        let entry = entry.expect("read a tests/chz entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.path().is_dir() {
            dirs.push(name);
        } else {
            assert!(
                !name.ends_with(".chz"),
                "tests/chz/{name} sits outside every subdirectory, so no per-directory test runs it"
            );
        }
    }
    let mut listed: Vec<String> = listed.iter().map(|name| name.to_string()).collect();
    dirs.sort();
    listed.sort();
    assert_eq!(
        dirs, listed,
        "every subdirectory of tests/chz needs a row in the per-directory test list"
    );
}
