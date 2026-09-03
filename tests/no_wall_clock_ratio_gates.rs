//! A `tests/chz` test must not divide two wall-clock samples.
//!
//! `tests/chz/spec/gc_core_graph_test.chz` asserted `deep / shallow < 2.8` on two ~10 ms
//! `time.monotonic()` samples. The noise in the two samples is uncorrelated and the smaller one is
//! the denominator, so CPU load amplifies the quotient without bound: 3 red runs in 25 under 32-way
//! oversubscription, worst `got 8.157102984533584 from 10.457949ms -> 85.306567ms`, and it reddened
//! the whole `cargo test` gate twice per run because `tests/chz` runs at two worker counts
//! (TICKET-049). A cost that must be pinned gets counted, not timed; a wall-clock bound with real
//! headroom is still fine, which is why the rule keys on the division, not on the clock.

use std::fs;
use std::path::{Path, PathBuf};

/// Every `.chz` file under `tests/chz`, sorted, so a failure names the same file every run.
fn chz_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            chz_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "chz") {
            out.push(p);
        }
    }
}

#[test]
fn no_chz_test_divides_two_wall_clock_samples() {
    let mut files = Vec::new();
    chz_files(Path::new("tests/chz"), &mut files);
    assert!(
        !files.is_empty(),
        "tests/chz holds no .chz files -- the scan is looking in the wrong place"
    );
    let mut bad = Vec::new();
    for path in &files {
        let text =
            fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        if !text.contains("time.monotonic()") {
            continue;
        }
        for (i, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if !code.starts_with('#') && code.contains('/') {
                bad.push(format!("{}:{}: {}", path.display(), i + 1, code.trim_end()));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "a tests/chz test that samples time.monotonic() must not divide -- a ratio of two \
         wall-clock samples amplifies scheduler noise without bound (TICKET-049). Count the work \
         in a Rust test, or assert a bound with headroom:\n{}",
        bad.join("\n")
    );
}
