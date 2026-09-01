//! Panic-fuzz harness: feed adversarial / malformed inputs to `chezzi check` (the full front-end —
//! `lexer` → `parser` → `checker`) and assert the crash-safety invariant: malformed input yields a
//! clean diagnostic, never a Rust **panic** or a **signal** crash (SIGSEGV / SIGABRT /
//! stack-overflow). Bug-discovery lever #1 (`docs/bug-discovery.md`).
//!
//! Self-contained, no `crate::` references, so the same sources compile into both the
//! `tests/panicfuzz.rs` CI gate and the `src/bin/panicfuzz.rs` long-runner via `#[path]`. Chezzi now
//! has a library crate (`src/lib.rs`), so an in-process target is technically possible; this remains
//! a SUBPROCESS harness because shelling out catches more crash classes — notably **stack overflow**,
//! the most likely deep-parser crash — which `catch_unwind` cannot. It is a stable, dependency-free
//! stand-in for `cargo-fuzz` (no nightly / rustup / cargo-fuzz in this environment).
//!
//! `dead_code` is allowed module-wide: the two consumers (the test gate and the fuzz bin) exercise
//! different subsets of this shared toolkit.
#![allow(dead_code)]

pub mod generate;
pub mod rng;
pub mod run;

pub use run::{Config, Outcome};

/// Generate the input for `seed`, feed it to `chezzi check`, and classify. Returns the outcome plus
/// the raw triggering input (so a finding can be reported / reproduced via `--seed N`).
pub fn run_seed(cfg: &Config, seed: u64, corpus: &[Vec<u8>]) -> (Outcome, Vec<u8>) {
    let input = generate::gen_input(seed, corpus);
    let outcome = run::run_input(cfg, &input);
    (outcome, input)
}

/// Render a paste-ready description of a finding: the seed, the crash kind, the `chezzi` stderr
/// (carrying the Rust panic `file:line`), and the raw triggering input (lossy UTF-8 with non-UTF-8
/// bytes marked). Reproduce with `panicfuzz --seed <seed>`.
pub fn describe(seed: u64, outcome: &Outcome, input: &[u8]) -> String {
    let mut s = format!("\n=== seed {seed}: {} ===\n", kind_label(outcome));
    match outcome {
        Outcome::HostPanic { cap } => {
            s.push_str(&format!(
                "--- chezzi stderr (RUST PANIC) ---\n{}\n",
                cap.stderr
            ));
        }
        Outcome::Crash { cap } => {
            // NAME the signal — SIGSEGV, SIGABRT and SIGFPE want three different first moves, and
            // "killed by signal" tells the reader none of that (W7-38).
            let which = match cap.signal {
                Some(sig) => format!("{} (signal {sig})", run::signal_name(sig)),
                None => "an unknown signal (code: None)".to_string(),
            };
            s.push_str(&format!(
                "--- chezzi killed by {which} ---\nstderr:\n{}\n",
                cap.stderr
            ));
        }
        Outcome::HarnessError(msg) => s.push_str(&format!("harness error: {msg}\n")),
        _ => {}
    }
    s.push_str(&format!(
        "reproduce: panicfuzz --seed {seed}\n--- triggering input ({} bytes) ---\n{}\n",
        input.len(),
        escape_bytes(input)
    ));
    s
}

/// One stable word per `Outcome` variant. Also the histogram key the fuzz binary tallies by, so a
/// sweep's `done:` line says WHAT the seeds did, not just how many findings came out (W7-34's
/// residual: "0 findings" over 0 executed checks looks identical to a clean sweep).
pub fn kind_label(o: &Outcome) -> &'static str {
    match o {
        Outcome::Clean { .. } => "Clean",
        Outcome::HostPanic { .. } => "HostPanic",
        Outcome::Crash { .. } => "Crash",
        Outcome::Timeout => "Timeout",
        Outcome::HarnessError(_) => "HarnessError",
    }
}

/// Lossy-render bytes for a report: printable UTF-8 passes through; other bytes become `\xNN`.
fn escape_bytes(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len());
    for ch in String::from_utf8_lossy(b).chars() {
        if ch == '\u{FFFD}' {
            // Replacement char — the original byte was invalid UTF-8; the exact byte is lost. Mark it.
            out.push_str("\\x??");
        } else if ch == '\n' || ch == '\t' || !ch.is_control() {
            out.push(ch);
        } else {
            out.push_str(&format!("\\x{:02x}", ch as u32));
        }
    }
    out
}
