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
//!
//! Exit codes: `0` = clean (no findings), `1` = findings were found (real divergences), `2` =
//! the harness itself is broken (bad args, or a seed's `Outcome::HarnessError` — e.g. the
//! `chezzi` binary could not be spawned) and NO seed after the failure was executed. `2` is
//! deliberately distinct from `1` so a caller can tell "the oracle broke" from "the oracle
//! worked and found bugs" — printing "N seeds, 0 finding(s)" with exit 0 over a harness that
//! never ran a single program would be a false negative dressed up as a clean pass.

#[path = "../difftest/mod.rs"]
mod difftest;

use difftest::{Features, Outcome, run::Config};
use std::time::Duration;

fn main() {
    // `args_os` + lossy: `std::env::args()` panics on a non-UTF-8 argument (see src/main.rs).
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
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
        // A harness error means the oracle could not even run this seed (e.g. `chezzi` is not
        // on PATH) — fatal, not "0 findings". Abort loud instead of grinding through the rest
        // of the range reporting nothing wrong.
        if let Outcome::HarnessError(msg) = &outcome {
            eprintln!("harness error at seed {seed}: {msg}");
            std::process::exit(2);
        }
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
