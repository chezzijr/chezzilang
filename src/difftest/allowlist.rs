//! Known, deliberately-unfixed divergences. Each matcher returns `Some(reason)` to downgrade
//! a divergence to a non-finding.
//!
//! This list is intentionally tiny: the Python shim already absorbs every *documented*
//! intentional difference (bool/nil spelling, raw nested strings, truncating int `/`/`%`).
//! An entry here is for a corner we have consciously decided not to chase — but it must be
//! narrow, cite why, and must not mask a **value** divergence, only a **formatting** one.
//!
//! `MATCHERS` is empty (W7-31, 2026-08-07): the one prior entry, `float_scientific_crossover`,
//! excused "Rust `{}` vs CPython `repr` pick different scientific-notation crossovers" — and that
//! specific divergence does not occur. `vm::format_float` → `fmtspec::repr_float` implements
//! CPython's crossover RULE directly (scientific when the decimal exponent is `< -4` or `>= 16`),
//! and the two agree byte-for-byte at every boundary (measured table in `docs/gaps.md` §W7-31).
//! The entry was a pure masking device; deleted rather than gated.
//!
//! Scope that claim carefully — it is about the CROSSOVER, not about float formatting in general.
//! Chezzi's shortest-repr DIGITS came from Rust's formatter, which breaks an exact half-way tie
//! away from zero where CPython breaks it to even; that was a real divergence, tracked separately
//! as `docs/gaps.md` §W7-32 and **FIXED there 2026-08-07** (`fmtspec::repr_float` now re-renders an
//! odd-last-digit shortest repr half-to-even and keeps it if it round-trips). It was never
//! allow-list material — it was a bug, and this oracle SHOULD have reported it. Note also that `vm/parity_tests.rs::python_float_repr_str_parity` is a serial==M:N
//! golden against a hardcoded literal, not a CPython differential: it cannot vouch for parity with
//! CPython, and citing it as if it could is how W7-32 stayed invisible.

use super::ast::Program;
use super::run::Capture;

type Matcher = fn(Option<&Program>, &Capture, &Capture) -> Option<&'static str>;

// Extension point, deliberately empty — see the module doc above. A future entry goes here.
const MATCHERS: &[Matcher] = &[];

pub fn check(prog: Option<&Program>, chz: &Capture, py: &Capture) -> Option<&'static str> {
    // W7-31: an allow-list entry excuses a *formatting* difference between two SUCCESSFUL runs.
    // A non-zero exit on either side is never that — it's a crash or a fault, and downgrading it
    // to a non-finding is the exact bug this floor closes. This was filed as a per-MATCHER gate
    // (`float_scientific_crossover` requiring `code == Some(0)` on both sides) rather than a
    // call-site floor, reasoning that `MATCHERS` is an extension point and a future entry might
    // legitimately apply to a fault arm. With zero entries there is no matcher left to gate, so
    // the floor lives here instead. A future entry that genuinely needs a fault arm moves this
    // gate down into that matcher (and the others) — it is never simply deleted.
    if chz.code != Some(0) || py.code != Some(0) {
        return None;
    }
    for m in MATCHERS {
        if let Some(reason) = m(prog, chz, py) {
            return Some(reason);
        }
    }
    None
}
