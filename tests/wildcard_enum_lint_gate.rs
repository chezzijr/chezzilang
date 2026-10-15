//! TICKET-251: a `_ =>` arm on a language enum gives a new variant the old behaviour silently.
//! The lint `clippy::wildcard_enum_match_arm` must be denied in ONE place, `Cargo.toml`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The number of `allow(clippy::wildcard_enum_match_arm)` attributes in non-test code under `src`.
/// Raise it only with a one-line comment at the new site that says why a future variant cannot be
/// wrong there. Any line under `src` that names the lint counts, a comment included.
const PINNED_ALLOWS: usize = 0;

/// The crate roots whose TEST build is out of scope for the deny. Each one carries the line
/// `#![cfg_attr(test, allow(clippy::wildcard_enum_match_arm))]`.
const TEST_SCOPE_ROOTS: [&str; 5] = [
    "crates/chezzi-syntax/src/lib.rs",
    "src/bin/difffuzz.rs",
    "src/bin/panicfuzz.rs",
    "src/bin/schedfuzz.rs",
    "src/lib.rs",
];

const ROOT_LINE: &str = "#![cfg_attr(test, allow(clippy::wildcard_enum_match_arm))]";

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn wildcard_enum_match_arm_is_denied_in_cargo_toml() {
    let toml = fs::read_to_string("Cargo.toml").expect("read Cargo.toml");
    let denied = toml.lines().any(|l| {
        let l = l.trim();
        l.starts_with("wildcard_enum_match_arm")
            && (l.contains("\"deny\"") || l.contains("\"forbid\""))
    });
    assert!(
        denied,
        "Cargo.toml [lints.clippy] does not deny wildcard_enum_match_arm"
    );
}

#[test]
fn allows_of_the_lint_in_src_are_pinned() {
    let mut files = Vec::new();
    rust_files(Path::new("src"), &mut files);
    rust_files(Path::new("crates"), &mut files);
    let mut allows = Vec::new();
    let mut roots = Vec::new();
    for file in files {
        let name = file.to_string_lossy().replace('\\', "/");
        let text = fs::read_to_string(&file).expect("read source");
        for (i, line) in text.lines().enumerate() {
            if !line.contains("clippy::wildcard_enum_match_arm") {
                continue;
            }
            if line.trim() == ROOT_LINE {
                roots.push(name.clone());
            } else {
                allows.push(format!("{name}:{}", i + 1));
            }
        }
    }
    roots.sort();
    assert_eq!(
        allows.len(),
        PINNED_ALLOWS,
        "the lint is named at {allows:?}; PINNED_ALLOWS is {PINNED_ALLOWS}"
    );
    assert_eq!(roots, TEST_SCOPE_ROOTS, "test-scope allow lines moved");
}

/// The suite gate runs tests only, so this test is where the gate runs clippy. The deny in
/// `Cargo.toml` makes a wildcard arm on an enum in the library or a binary a clippy error.
/// `--features lsp` puts the `chezzi-lsp` binary in the run: its `required-features` skips it
/// otherwise.
#[test]
fn clippy_passes_for_the_library_and_the_binaries() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let out = Command::new(cargo)
        .args(["clippy", "--lib", "--bins", "--features", "lsp", "--quiet"])
        .output()
        .expect("run cargo clippy");
    assert!(
        out.status.success(),
        "`cargo clippy --lib --bins --features lsp` failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
