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
//! **Findings win over the abort**: if the harness breaks mid-range AFTER a real divergence was
//! already confirmed, the exit code is `1`, not `2` — a real finding must never be masked by a
//! later environment failure (the abort is still printed, and the `done:` line says how many
//! seeds actually ran). An EMPTY range (`--seeds 5..5`) is a usage error (`2`), not a clean pass:
//! zero comparisons is exactly the "0 findings" false negative this whole contract exists to
//! prevent (`docs/gaps.md` W7-38).

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

    // `<=`, not `<`: an EMPTY range runs zero seeds, compares nothing, and prints
    // `done: 0 seeds 5..5, 0 finding(s) []` with exit 0 — a clean pass over zero evidence, the
    // exact false negative W7-34 and its residual exist to close, one layer up in the arg parser
    // (W7-38).
    if end <= start {
        eprintln!("empty or inverted seed range: {start}..{end} (need end > start)");
        std::process::exit(2);
    }

    let chezzi = locate_chezzi();
    let mut cfg = Config::new(chezzi);
    cfg.timeout = Duration::from_millis(timeout_ms);

    let mut findings = 0usize;
    // Outcome histogram, keyed by `difftest::kind_label`. Without it `done: 20 seeds, 0 finding(s)`
    // is byte-identical whether every seed was actually compared or every seed timed out and
    // NOTHING was ever compared — `HarnessError` closes "the child never started", not "the
    // children started and nothing was compared" (`docs/gaps.md` W7-34). `BTreeMap` so the order
    // is stable run to run. Deliberately NO abort threshold / "too many timeouts" heuristic: a
    // verdict that cannot be certain must DECLINE rather than emit a confident wrong one (this
    // project's standing rule — `docs/gaps.md` W7-12, the `parked-is-not-stuck` family), and a
    // wrong "your oracle is broken" abort would teach distrust of every future one. Make the run
    // LEGIBLE and let the human read the histogram.
    let mut hist: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    let total = end - start;
    let mut ran = 0u64;
    let mut aborted = false;
    for seed in start..end {
        let (outcome, chz, py) = difftest::run_seed(&cfg, seed, feat);
        ran += 1;
        *hist.entry(difftest::kind_label(&outcome)).or_default() += 1;
        // A harness error means the oracle could not even run this seed (e.g. `chezzi` is not
        // on PATH) — fatal, not "0 findings". Abort loud instead of grinding through the rest
        // of the range reporting nothing wrong. BREAK, never `exit()` from inside the loop: an
        // in-loop exit skips the `done:` line and the histogram — losing the count of seeds that
        // did run on the one run where that count matters most — and hard-codes exit 2 over any
        // real finding already confirmed earlier in the range (W7-38).
        if let Outcome::HarnessError(msg) = &outcome {
            eprintln!("harness error at seed {seed}: {msg}");
            aborted = true;
            break;
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

    let hist: Vec<String> = hist.iter().map(|(k, n)| format!("{k} {n}")).collect();
    eprintln!(
        "done: {ran} of {total} seeds {start}..{end}, {findings} finding(s) [{}]",
        hist.join(", ")
    );
    if aborted {
        eprintln!(
            "ABORTED: the harness broke (see above) — {} seed(s) never ran",
            total - ran
        );
    }
    // A real finding outranks the abort: exit 1 so it is never masked, and say both happened.
    if findings > 0 {
        if aborted {
            eprintln!("exit 1 (real findings) even though the harness also broke — both above");
        }
        std::process::exit(1);
    }
    if aborted {
        std::process::exit(2);
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
