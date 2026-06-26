//! Unattended differential fuzzer: runs the CPython oracle over a seed range and reports any
//! divergence (with reproducing sources). The CI-gate variant lives in `tests/difftest.rs`;
//! both share the engine under `src/difftest/` via a `#[path]` include.
//!
//! Usage:
//!   cargo run --release --bin difffuzz -- [--seeds A..B] [--seed N] [--timeout-ms N]
//!                                          [--straight-line] [--floats] [--quiet]
//!
//! The `chezzi` binary is located as a sibling of this executable (so build both first, e.g.
//! `cargo build --release`).

#[path = "../difftest/mod.rs"]
mod difftest;

use difftest::{Features, run::Config};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut start = 0u64;
    let mut end = 10_000u64;
    let mut timeout_ms = 10_000u64;
    let mut feat = Features::full();
    let mut quiet = false;

    let val = |i: usize, flag: &str| -> String {
        args.get(i)
            .unwrap_or_else(|| {
                eprintln!("{flag} requires a value");
                std::process::exit(2);
            })
            .clone()
    };

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seeds" => {
                i += 1;
                let (a, b) = parse_range(&val(i, "--seeds"));
                start = a;
                end = b;
            }
            "--seed" => {
                i += 1;
                start = val(i, "--seed").parse().expect("--seed N");
                end = start + 1;
            }
            "--timeout-ms" => {
                i += 1;
                timeout_ms = val(i, "--timeout-ms").parse().expect("--timeout-ms N");
            }
            "--straight-line" => feat = Features::straight_line(),
            "--floats" => feat.floats = true,
            "--quiet" => quiet = true,
            "-h" | "--help" => {
                eprintln!(
                    "usage: difffuzz [--seeds A..B] [--seed N] [--timeout-ms N] [--straight-line] [--floats] [--quiet]"
                );
                return;
            }
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    if end < start {
        eprintln!("empty/inverted seed range: {start}..{end}");
        std::process::exit(2);
    }

    let chezzi = locate_chezzi();
    let mut cfg = Config::new(chezzi);
    cfg.timeout = Duration::from_millis(timeout_ms);

    let mut findings = 0usize;
    let total = end - start;
    for seed in start..end {
        let (outcome, chz, py) = difftest::run_seed(&cfg, seed, feat);
        if outcome.is_finding() {
            findings += 1;
            println!("{}", difftest::describe(seed, &outcome, &chz, &py));
        }
        if !quiet && seed != start && (seed - start).is_multiple_of(200) {
            eprintln!(
                "progress: {}/{} seeds, {} findings",
                seed - start,
                total,
                findings
            );
        }
    }

    eprintln!(
        "done: {total} seeds, {findings} finding(s) [{:?}]",
        (start, end)
    );
    if findings > 0 {
        std::process::exit(1);
    }
}

fn parse_range(s: &str) -> (u64, u64) {
    let parts: Vec<&str> = s.split("..").collect();
    assert!(parts.len() == 2, "range must be A..B");
    (
        parts[0].parse().expect("range start"),
        parts[1].parse().expect("range end"),
    )
}

fn locate_chezzi() -> std::path::PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let dir = exe.parent().expect("exe dir");
    let cand = dir.join("chezzi");
    if cand.exists() {
        return cand;
    }
    // fall back to PATH
    std::path::PathBuf::from("chezzi")
}
