//! CPython differential oracle — CI gate.
//!
//! Shares the engine with `src/bin/difffuzz.rs` via a `#[path]` include (the crate has no
//! `[lib]`). `env!("CARGO_BIN_EXE_chezzi")` is provided by cargo for integration tests.

#[path = "../src/difftest/mod.rs"]
mod difftest;

use difftest::ast::*;
use difftest::{Config, Features, Outcome, run};
use std::path::PathBuf;
use std::time::Duration;

fn config() -> Config {
    let mut c = Config::new(env!("CARGO_BIN_EXE_chezzi"));
    c.timeout = Duration::from_secs(20);
    c
}

fn manifest(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

// ---------------------------------------------------------------------------
// P0 — the riskiest piece (output formatting) proven before any generation.
// ---------------------------------------------------------------------------

/// The existing hand-written bench pairs must already agree through the runner. This
/// validates the process harness (capture, exit codes, diff) end-to-end.
#[test]
fn p0_existing_bench_pairs_match() {
    let cfg = config();
    let names = [
        "fib", "loop", "str", "primes", "list", "map", "struct", "empty",
    ];
    for name in names {
        let chz = std::fs::read_to_string(manifest(&format!("benches/chz/{name}.chz")))
            .unwrap_or_else(|_| panic!("missing benches/chz/{name}.chz"));
        let py = std::fs::read_to_string(manifest(&format!("benches/py/{name}.py")))
            .unwrap_or_else(|_| panic!("missing benches/py/{name}.py"));
        let outcome = run::run_sources(&cfg, &chz, &py, None);
        assert!(
            matches!(outcome, Outcome::Match | Outcome::AllowListed(_)),
            "bench pair {name} diverged: {outcome:?}"
        );
    }
}

/// The shim's value-stringifying must reproduce Chezzi's spelling for the by-design diffs:
/// bool `true`/`false`, nested strings raw (unquoted), and a list mixing types.
#[test]
fn p0_shim_formatting_probes() {
    let cfg = config();
    // print(true); print(false); print(["a", 1, true]); print({"k": "v"})
    let prog = Program {
        funcs: vec![],
        main: vec![
            Stmt::Print(vec![Expr::BoolLit(true)]),
            Stmt::Print(vec![Expr::BoolLit(false)]),
            Stmt::Print(vec![Expr::ListLit {
                elem: Ty::Str,
                items: vec![Expr::StrLit("a".into()), Expr::StrLit("b".into())],
            }]),
            Stmt::Print(vec![Expr::MapLit {
                k: Ty::Str,
                v: Ty::Int,
                entries: vec![(Expr::StrLit("k".into()), Expr::IntLit(7))],
            }]),
        ],
    };
    let (outcome, chz, py) = run::run_program(&cfg, &prog);
    assert!(
        matches!(outcome, Outcome::Match | Outcome::AllowListed(_)),
        "{}",
        difftest::describe(0, &outcome, &chz, &py)
    );
}

/// Negative-operand integer `/` and `%` must match between Chezzi and the `_chz_div`/`_chz_mod`
/// shim (truncate toward zero / sign of dividend).
#[test]
fn p0_shim_div_mod_negatives() {
    let cfg = config();
    let cases = [(-7, 3), (7, -3), (-7, -3), (10, 3), (-10, 3)];
    for (a, b) in cases {
        let prog = Program {
            funcs: vec![],
            main: vec![
                Stmt::Print(vec![Expr::Bin {
                    op: BinOp::Div,
                    ty: Ty::Int,
                    l: Box::new(Expr::IntLit(a)),
                    r: Box::new(Expr::IntLit(b)),
                }]),
                Stmt::Print(vec![Expr::Bin {
                    op: BinOp::Mod,
                    ty: Ty::Int,
                    l: Box::new(Expr::IntLit(a)),
                    r: Box::new(Expr::IntLit(b)),
                }]),
            ],
        };
        let (outcome, chz, py) = run::run_program(&cfg, &prog);
        assert!(
            matches!(outcome, Outcome::Match | Outcome::AllowListed(_)),
            "div/mod {a},{b}: {}",
            difftest::describe(0, &outcome, &chz, &py)
        );
    }
}

// ---------------------------------------------------------------------------
// Non-tautology guard — prove the oracle actually detects a real divergence.
// If the shim were removed, raw Python `%` (sign of divisor) differs from Chezzi.
// ---------------------------------------------------------------------------

#[test]
fn oracle_detects_real_divergence() {
    let cfg = config();
    // Chezzi: -7 % 3 == -1 (sign of dividend). Raw Python: -7 % 3 == 2 (sign of divisor).
    let outcome = run::run_sources(&cfg, "print(-7 % 3)\n", "print(-7 % 3)\n", None);
    assert!(
        outcome.is_finding(),
        "expected a divergence finding, got {outcome:?}"
    );
}

/// Guards the `try_call` arg-bound fix: a function that squares its parameter, called with a
/// large argument, overflows i64 in Chezzi (fault) but not in Python (bignum) — a finding. The
/// generator must never emit such a call (int args are bounded literals); this proves that if
/// it ever regressed, the fuzz tests would go red instead of silently passing.
#[test]
fn oracle_detects_call_arg_overflow() {
    let cfg = config();
    let prog = Program {
        funcs: vec![Func {
            name: "f0".into(),
            params: vec![("p0".into(), Ty::Int)],
            ret: Ty::Int,
            body: vec![Stmt::Return(Some(Expr::Bin {
                op: BinOp::Mul,
                ty: Ty::Int,
                l: Box::new(Expr::Var("p0".into())),
                r: Box::new(Expr::Var("p0".into())),
            }))],
        }],
        main: vec![
            Stmt::Let {
                name: "v0".into(),
                ty: Ty::Int,
                init: Expr::IntLit(5_000_000_000), // 5e9; (5e9)^2 = 2.5e19 > i64::MAX
            },
            Stmt::Print(vec![Expr::Call {
                name: "f0".into(),
                ret: Ty::Int,
                args: vec![Expr::Var("v0".into())],
            }]),
        ],
    };
    let (outcome, chz, py) = run::run_program(&cfg, &prog);
    assert!(
        outcome.is_finding(),
        "overflow-via-call should be a finding: {}",
        difftest::describe(0, &outcome, &chz, &py)
    );
}

// ---------------------------------------------------------------------------
// Generated-program fuzzing over a fixed, reproducible seed range.
// ---------------------------------------------------------------------------

fn fuzz_range(feat: Features, start: u64, end: u64) {
    let cfg = config();
    let mut findings = Vec::new();
    for seed in start..end {
        let (outcome, chz, py) = difftest::run_seed(&cfg, seed, feat);
        if outcome.is_finding() {
            findings.push(difftest::describe(seed, &outcome, &chz, &py));
        }
    }
    assert!(
        findings.is_empty(),
        "differential divergences found:\n{}",
        findings.join("\n")
    );
}

#[test]
fn fuzz_straight_line() {
    fuzz_range(Features::straight_line(), 0, 120);
}

#[test]
fn fuzz_full() {
    fuzz_range(Features::full(), 0, 120);
}

/// Heavier sweep for manual runs: `cargo test --test difftest -- --ignored`.
#[test]
#[ignore]
fn fuzz_full_heavy() {
    fuzz_range(Features::full(), 0, 3000);
}
