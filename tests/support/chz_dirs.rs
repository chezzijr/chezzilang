//! The subdirectories of tests/chz: the one list each per-directory tests/chz test is checked against (TICKET-276).

#![allow(dead_code)]

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
