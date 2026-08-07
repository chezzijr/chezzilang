//! Differential-testing oracle: generate semantically-equivalent programs, render them as
//! Chezzi and Python, run both, and diff stdout. Catches *shared* semantic bugs that the
//! VM↔interp parity oracle structurally cannot (both engines were co-developed). See
//! `docs/bug-discovery.md` lever #2.
//!
//! Self-contained: no `crate::` references, so the same sources compile into both the
//! `tests/difftest.rs` CI gate and the `src/bin/difffuzz.rs` long-runner via `#[path]`.
//!
//! `dead_code` is allowed module-wide: this is a shared toolkit whose two consumers (the
//! test gate and the fuzz bin) exercise different subsets, and the IR deliberately carries
//! type fields for completeness even where the emitters use `:=` inference.
#![allow(dead_code)]

pub mod allowlist;
pub mod ast;
pub mod emit_chezzi;
pub mod emit_python;
pub mod generate;
pub mod rng;
pub mod run;

pub use generate::Features;
pub use run::{Config, Outcome};

/// Generate the program for `seed`, run it on both engines, and classify. Returns the
/// outcome plus the rendered Chezzi and Python sources (for reporting / reproduction).
pub fn run_seed(cfg: &Config, seed: u64, feat: Features) -> (Outcome, String, String) {
    let p = generate::gen_program(seed, feat);
    run::run_program(cfg, &p)
}

/// Render a one-line description of a finding suitable for a test failure or CLI report.
pub fn describe(seed: u64, outcome: &Outcome, chz_src: &str, py_src: &str) -> String {
    let mut s = format!("\n=== seed {seed}: {:?} ===\n", kind_label(outcome));
    match outcome {
        Outcome::Divergence { kind, chz, py } => {
            s.push_str(&format!("kind: {kind:?}\n"));
            // Decoded for the human reading the report. When the two sides are byte-different but
            // decode alike (non-UTF-8 output), the text below looks identical — so spell the raw
            // bytes out too, or the report would contradict the verdict.
            s.push_str(&format!(
                "--- chezzi stdout ---\n{}\n--- python stdout ---\n{}\n",
                chz.stdout_text(),
                py.stdout_text()
            ));
            if *kind == run::DivKind::Stdout && chz.stdout_text() == py.stdout_text() {
                s.push_str(&format!(
                    "--- stdout differs in RAW BYTES ONLY (the decode above is lossy) ---\nchezzi: {}\npython: {}\n",
                    hex(&chz.stdout),
                    hex(&py.stdout)
                ));
            }
            if !chz.stderr_text().trim().is_empty() {
                s.push_str(&format!("--- chezzi stderr ---\n{}\n", chz.stderr_text()));
            }
            if !py.stderr_text().trim().is_empty() {
                s.push_str(&format!("--- python stderr ---\n{}\n", py.stderr_text()));
            }
        }
        Outcome::HostPanic { chz } => {
            s.push_str(&format!(
                "--- chezzi stderr (RUST PANIC) ---\n{}\n",
                chz.stderr_text()
            ));
        }
        Outcome::Timeout { which } => s.push_str(&format!("timed out: {which}\n")),
        Outcome::AllowListed(reason) => s.push_str(&format!("allow-listed: {reason}\n")),
        _ => {}
    }
    s.push_str(&format!("--- chezzi source ---\n{chz_src}\n"));
    s.push_str(&format!("--- python source ---\n{py_src}\n"));
    s
}

/// Space-separated lowercase hex — the only rendering that survives a byte-only divergence.
fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn kind_label(o: &Outcome) -> &'static str {
    match o {
        Outcome::Match => "Match",
        Outcome::AllowListed(_) => "AllowListed",
        Outcome::Divergence { .. } => "Divergence",
        Outcome::HostPanic { .. } => "HostPanic",
        Outcome::Timeout { .. } => "Timeout",
        Outcome::BothError => "BothError",
    }
}
