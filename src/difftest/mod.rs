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
            // `ChezziHang`'s `chz` is a SYNTHESIZED capture (empty stdout, a stderr note,
            // `code: None`) — say so explicitly, or an empty chezzi-stdout block next to a
            // Python stdout reads as an ordinary (and wrong) stdout-mismatch report.
            if *kind == run::DivKind::ChezziHang {
                s.push_str(
                    "--- chezzi produced NO CAPTURE: it did not exit within the timeout (hang) ---\n--- python exited 0 ---\n",
                );
            }
            // Decoded for the human reading the report. When the two sides are byte-different but
            // decode alike (non-UTF-8 output), the text below looks identical — so spell the raw
            // bytes out too, or the report would contradict the verdict.
            s.push_str(&format!(
                "--- chezzi stdout ---\n{}\n--- python stdout ---\n{}\n",
                chz.stdout_text(),
                py.stdout_text()
            ));
            // W7-30's byte-only-divergence hex fallback applies to BOTH stdout-comparing kinds:
            // `Stdout` (both exit 0) and `BothErrorStdout` (both failed, prefix differs) can each
            // have two byte-different stdouts that happen to DECODE alike — without the raw hex
            // line that would render as an unreadable contradiction (a `Divergence` verdict over
            // two identical-looking blocks), the exact defect W7-30 fixed.
            if (*kind == run::DivKind::Stdout || *kind == run::DivKind::BothErrorStdout)
                && chz.stdout_text() == py.stdout_text()
            {
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
            // `chz.code: None` means a SIGNAL kill (see `Capture::code`), which may carry no
            // `panicked at` marker at all (a bare SIGSEGV writes nothing). Say so explicitly, or
            // an empty-stderr signal death renders as an empty block that contradicts the
            // HostPanic verdict above it — the same defect W7-30 had to fix in this function.
            // NAME the signal: "killed by a SIGNAL" alone does not tell a SIGSEGV (memory bug)
            // from a SIGABRT (assert/double-panic) from a SIGFPE, and those want three different
            // first moves. An unactionable report teaches the same distrust as a wrong one (W7-30,
            // W7-38).
            if let Some(sig) = chz.signal {
                s.push_str(&format!(
                    "--- chezzi killed by {} (signal {sig}) ---\n",
                    run::signal_name(sig)
                ));
            } else if chz.code.is_none() {
                s.push_str("--- chezzi killed by a SIGNAL (code: None, number unavailable) ---\n");
            }
            if !chz.stderr_text().trim().is_empty() {
                s.push_str(&format!(
                    "--- chezzi stderr (RUST PANIC) ---\n{}\n",
                    chz.stderr_text()
                ));
            }
        }
        Outcome::Timeout { which } => s.push_str(&format!("timed out: {which}\n")),
        Outcome::AllowListed(reason) => s.push_str(&format!("allow-listed: {reason}\n")),
        Outcome::HarnessError(msg) => s.push_str(&format!("harness error: {msg}\n")),
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

/// One stable word per `Outcome` variant. Also the histogram key the fuzz binaries tally by, so a
/// sweep's `done:` line says WHAT the seeds did, not just how many findings came out (W7-34's
/// residual: "0 findings" over 0 executed comparisons looks identical to a clean sweep).
pub fn kind_label(o: &Outcome) -> &'static str {
    match o {
        Outcome::Match => "Match",
        Outcome::AllowListed(_) => "AllowListed",
        Outcome::Divergence { .. } => "Divergence",
        Outcome::HostPanic { .. } => "HostPanic",
        Outcome::Timeout { .. } => "Timeout",
        Outcome::BothError => "BothError",
        Outcome::HarnessError(_) => "HarnessError",
    }
}
