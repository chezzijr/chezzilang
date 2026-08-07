//! Unattended panic-fuzzer for the Chezzi front-end (bug-discovery lever #1): feeds adversarial /
//! malformed inputs to `chezzi check` over a seed range and reports any input that makes the
//! front-end **crash** — a Rust panic (`panicked at` on stderr) or a signal kill (SIGSEGV /
//! SIGABRT / stack-overflow, exit code `None`). The crash-safety invariant is: malformed input
//! yields a clean diagnostic, never a panic or a signal. The CI-gate variant lives in
//! `tests/panicfuzz.rs`; both share the engine under `src/panicfuzz/` via a `#[path]` include (the
//! fuzz engine is not exposed by the `chezzi` library, so it is pulled in by path rather than `use`).
//!
//! Usage:
//!   cargo run --release --bin panicfuzz -- [--seeds A..B] [--seed N] [--timeout-ms N] [--quiet]
//!
//! The `chezzi` binary is located as a sibling of this executable (so build both first, e.g.
//! `cargo build --release`).
//!
//! Exit codes: `0` = clean (no findings), `1` = findings were found (real panics/crashes), `2` =
//! the harness itself is broken (bad args, or a seed's `Outcome::HarnessError` — e.g. the
//! `chezzi` binary could not be spawned) and NO seed after the failure was executed. `2` is
//! deliberately distinct from `1` so a caller can tell "the oracle broke" from "the oracle
//! worked and found bugs" — printing "N seeds, 0 finding(s)" with exit 0 over a harness that
//! never ran a single program would be a false negative dressed up as a clean pass
//! (`docs/gaps.md` W7-35, mirroring `difffuzz`'s W7-34 contract).
//! **Findings win over the abort**: if the harness breaks mid-range AFTER a real crash was already
//! found, the exit code is `1`, not `2` — a real finding must never be masked by a later
//! environment failure (the abort is still printed, and the `done:` line says how many seeds
//! actually ran). An EMPTY range (`--seeds 5..5`) is a usage error (`2`), not a clean pass: zero
//! inputs checked is exactly the "0 findings" false negative this contract exists to prevent
//! (`docs/gaps.md` W7-38).
//!
//! NOTE: a *release* sibling `chezzi` is built with overflow-checks OFF, so arithmetic-overflow
//! wraps silently and is invisible here. For a full overflow sweep, build with
//! `RUSTFLAGS="-C overflow-checks=on"`. Signal / segfault / explicit-panic crashes are still caught
//! regardless of the build profile.

#[path = "../panicfuzz/mod.rs"]
mod panicfuzz;

use panicfuzz::Outcome;
use panicfuzz::run::Config;
use std::time::Duration;

fn main() {
    // `args_os` + lossy: `std::env::args()` panics on a non-UTF-8 argument (see src/main.rs).
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let mut start = 0u64;
    let mut end = 100_000u64;
    let mut timeout_ms = 10_000u64;
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
            "--quiet" => quiet = true,
            "-h" | "--help" => {
                eprintln!("usage: panicfuzz [--seeds A..B] [--seed N] [--timeout-ms N] [--quiet]");
                return;
            }
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    // `<=`, not `<`: an EMPTY range checks zero inputs and prints
    // `done: 0 seeds 5..5, 0 finding(s) []` with exit 0 — a clean pass over zero evidence, the
    // exact false negative W7-34/W7-35 exist to close, one layer up in the arg parser (W7-38).
    if end <= start {
        eprintln!("empty or inverted seed range: {start}..{end} (need end > start)");
        std::process::exit(2);
    }

    let chezzi = locate_chezzi();
    let mut cfg = Config::new(chezzi);
    cfg.timeout = Duration::from_millis(timeout_ms);

    let corpus = load_corpus();
    if !quiet {
        eprintln!(
            "panicfuzz: chezzi={}, corpus={} examples, seeds {start}..{end}",
            cfg.chezzi_bin.display(),
            corpus.len()
        );
        eprintln!(
            "NOTE: a release-built chezzi has overflow-checks OFF — arithmetic-overflow wraps \
             silently and is invisible here. For a full overflow sweep rebuild with \
             RUSTFLAGS=\"-C overflow-checks=on\". Signal/segfault/panic crashes are still caught."
        );
    }

    let mut findings = 0usize;
    // Outcome histogram, keyed by `panicfuzz::kind_label`. Without it `done: 20 seeds, 0
    // finding(s)` is byte-identical whether every input was actually checked or every input timed
    // out and NOTHING was ever checked — `HarnessError` closes "the child never started", not "the
    // children started and nothing was checked" (`docs/gaps.md` W7-34). `BTreeMap` so the order is
    // stable run to run. Deliberately NO abort threshold / "too many timeouts" heuristic: a verdict
    // that cannot be certain must DECLINE rather than emit a confident wrong one (this project's
    // standing rule — `docs/gaps.md` W7-12, the `parked-is-not-stuck` family), and a wrong "your
    // oracle is broken" abort would teach distrust of every future one. Make the run LEGIBLE and
    // let the human read the histogram.
    let mut hist: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    let total = end - start;
    let mut ran = 0u64;
    let mut aborted = false;
    for seed in start..end {
        let (outcome, input) = panicfuzz::run_seed(&cfg, seed, &corpus);
        ran += 1;
        *hist.entry(panicfuzz::kind_label(&outcome)).or_default() += 1;
        // A harness error means the oracle could not even run this seed (e.g. `chezzi` is not
        // on PATH) — fatal, not "0 findings". Abort loud instead of grinding through the rest of
        // the range reporting nothing wrong. BREAK, never `exit()` from inside the loop: an
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
            println!("{}", panicfuzz::describe(seed, &outcome, &input));
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

/// Locate `examples/*.chz` by walking up from the executable for a dir containing both `examples/`
/// and `Cargo.toml` (the project root), falling back to the compile-time `CARGO_MANIFEST_DIR` (so
/// the corpus is still found when `target/` is redirected outside the project). Returns an empty
/// corpus if not found — strategy-3 (mutation) then degrades to the token sampler.
fn load_corpus() -> Vec<Vec<u8>> {
    if let Some(dir) = find_examples_dir() {
        return read_chz(&dir);
    }
    Vec::new()
}

fn find_examples_dir() -> Option<std::path::PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let mut cur = exe.parent();
        while let Some(dir) = cur {
            let examples = dir.join("examples");
            if examples.is_dir() && dir.join("Cargo.toml").is_file() {
                return Some(examples);
            }
            cur = dir.parent();
        }
    }
    // Fallback: the project root baked in at compile time.
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
    if manifest.is_dir() {
        return Some(manifest);
    }
    None
}

fn read_chz(dir: &std::path::Path) -> Vec<Vec<u8>> {
    // Sort paths by filename: `read_dir` order is filesystem-defined, so without this the
    // seed->base-example mapping (and thus strategy-3 reproducibility + the gate's input set)
    // would be machine-dependent.
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("chz") {
                paths.push(p);
            }
        }
    }
    paths.sort();
    paths
        .into_iter()
        .filter_map(|p| std::fs::read(&p).ok())
        .collect()
}
