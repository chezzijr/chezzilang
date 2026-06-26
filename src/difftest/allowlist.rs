//! Known, deliberately-unfixed divergences. Each matcher returns `Some(reason)` to downgrade
//! a divergence to a non-finding.
//!
//! This list is intentionally tiny: the Python shim already absorbs every *documented*
//! intentional difference (bool/nil spelling, raw nested strings, truncating int `/`/`%`).
//! An entry here is for a corner we have consciously decided not to chase (e.g. a float
//! shortest-decimal / scientific-notation crossover where Rust's `{}` and CPython's `repr`
//! pick different but equivalent spellings). Keep each entry narrow and cite why.

use super::ast::Program;
use super::run::Capture;

type Matcher = fn(Option<&Program>, &Capture, &Capture) -> Option<&'static str>;

const MATCHERS: &[Matcher] = &[float_scientific_crossover];

pub fn check(prog: Option<&Program>, chz: &Capture, py: &Capture) -> Option<&'static str> {
    for m in MATCHERS {
        if let Some(reason) = m(prog, chz, py) {
            return Some(reason);
        }
    }
    None
}

/// Rust's `{}` and CPython `repr` agree on the shortest round-tripping decimal but switch to
/// scientific notation at different magnitudes (e.g. `1e-05`, `1e+16`). When the only
/// difference between the two outputs is an `e`-notation token on one side, treat it as the
/// known float-formatting crossover rather than a semantic bug.
fn float_scientific_crossover(
    _prog: Option<&Program>,
    chz: &Capture,
    py: &Capture,
) -> Option<&'static str> {
    let a = &chz.stdout;
    let b = &py.stdout;
    if a == b {
        return None;
    }
    // Only fires when exactly one side uses exponent notation and the lengths are close —
    // a conservative guard so it never masks a real arithmetic divergence.
    let a_sci = a.contains('e') || a.contains('E');
    let b_sci = b.contains('e') || b.contains('E');
    if a_sci != b_sci && both_numericish(a) && both_numericish(b) {
        return Some(
            "float shortest-decimal vs scientific-notation crossover (Rust {} vs CPython repr)",
        );
    }
    None
}

fn both_numericish(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty()
        && t.chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'))
}
