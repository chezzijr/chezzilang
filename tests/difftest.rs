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
// P0 probes for the widened constructs (string methods, slicing, negative
// indexing, membership, tuples). Each proves shim/emitter parity by hand
// before the generator is allowed to emit it.
// ---------------------------------------------------------------------------

/// Tuple literal render, tuple-field access, and destructuring must all match the
/// Python tuple shim (`(1, two, true)` spelling, raw nested strings).
#[test]
fn p0_tuple_render() {
    let cfg = config();
    let tup = || {
        Expr::TupleLit(vec![
            Expr::IntLit(1),
            Expr::StrLit("two".into()),
            Expr::BoolLit(true),
        ])
    };
    let prog = Program {
        funcs: vec![],
        main: vec![
            Stmt::Let {
                name: "t".into(),
                ty: Ty::Tuple(vec![Ty::Int, Ty::Str, Ty::Bool]),
                init: tup(),
            },
            Stmt::Print(vec![Expr::Var("t".into())]),
            Stmt::Print(vec![Expr::TupleField {
                base: Box::new(Expr::Var("t".into())),
                idx: 0,
                ret: Ty::Int,
            }]),
            Stmt::Print(vec![Expr::TupleField {
                base: Box::new(Expr::Var("t".into())),
                idx: 1,
                ret: Ty::Str,
            }]),
            Stmt::Unpack {
                names: vec!["a".into(), "b".into(), "c".into()],
                init: Expr::Var("t".into()),
            },
            Stmt::Print(vec![Expr::Var("a".into())]),
            Stmt::Print(vec![Expr::Var("b".into())]),
        ],
    };
    let (outcome, chz, py) = run::run_program(&cfg, &prog);
    assert!(
        matches!(outcome, Outcome::Match | Outcome::AllowListed(_)),
        "{}",
        difftest::describe(0, &outcome, &chz, &py)
    );
}

/// Non-tautology guard for the tuple shim arm: a raw Python tuple print
/// (`(1, 'two', True)`) differs from the Chezzi spelling, so removing the shim
/// arm surfaces a real divergence.
#[test]
fn oracle_detects_tuple_render_divergence() {
    let cfg = config();
    let outcome = run::run_sources(
        &cfg,
        "print((1, \"two\", true))\n",
        "print((1, \"two\", True))\n",
        None,
    );
    assert!(
        outcome.is_finding(),
        "expected a tuple-render divergence finding, got {outcome:?}"
    );
}

/// Slicing lists and strings — open bounds, step, reverse, over-range clamp, and
/// negative bounds — must match natively (no shim).
#[test]
fn p0_slice() {
    let cfg = config();
    let xs = || Expr::Var("xs".into());
    let s = || Expr::Var("s".into());
    let lit = |n: i64| Some(Box::new(Expr::IntLit(n)));
    let prog = Program {
        funcs: vec![],
        main: vec![
            Stmt::Let {
                name: "xs".into(),
                ty: Ty::List(Box::new(Ty::Int)),
                init: Expr::ListLit {
                    elem: Ty::Int,
                    items: vec![
                        Expr::IntLit(10),
                        Expr::IntLit(20),
                        Expr::IntLit(30),
                        Expr::IntLit(40),
                        Expr::IntLit(50),
                    ],
                },
            },
            Stmt::Let {
                name: "s".into(),
                ty: Ty::Str,
                init: Expr::StrLit("abcdef".into()),
            },
            Stmt::Print(vec![Expr::Slice {
                base: Box::new(xs()),
                start: lit(1),
                end: lit(4),
                step: None,
                ret: Ty::List(Box::new(Ty::Int)),
            }]),
            Stmt::Print(vec![Expr::Slice {
                base: Box::new(xs()),
                start: None,
                end: None,
                step: lit(2),
                ret: Ty::List(Box::new(Ty::Int)),
            }]),
            Stmt::Print(vec![Expr::Slice {
                base: Box::new(xs()),
                start: None,
                end: None,
                step: lit(-1),
                ret: Ty::List(Box::new(Ty::Int)),
            }]),
            Stmt::Print(vec![Expr::Slice {
                base: Box::new(xs()),
                start: lit(1),
                end: lit(10),
                step: None,
                ret: Ty::List(Box::new(Ty::Int)),
            }]),
            Stmt::Print(vec![Expr::Slice {
                base: Box::new(xs()),
                start: lit(-3),
                end: lit(-1),
                step: None,
                ret: Ty::List(Box::new(Ty::Int)),
            }]),
            Stmt::Print(vec![Expr::Slice {
                base: Box::new(s()),
                start: lit(1),
                end: lit(4),
                step: None,
                ret: Ty::Str,
            }]),
            Stmt::Print(vec![Expr::Slice {
                base: Box::new(s()),
                start: None,
                end: None,
                step: lit(-1),
                ret: Ty::Str,
            }]),
            Stmt::Print(vec![Expr::Slice {
                base: Box::new(s()),
                start: lit(-3),
                end: lit(-1),
                step: None,
                ret: Ty::Str,
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

/// Negative scalar indexing of lists and strings.
#[test]
fn p0_negative_index() {
    let cfg = config();
    let idx = |b: Expr, i: i64, ret: Ty| Expr::Index {
        ret,
        base: Box::new(b),
        idx: Box::new(Expr::IntLit(i)),
    };
    let prog = Program {
        funcs: vec![],
        main: vec![
            Stmt::Let {
                name: "xs".into(),
                ty: Ty::List(Box::new(Ty::Int)),
                init: Expr::ListLit {
                    elem: Ty::Int,
                    items: vec![Expr::IntLit(10), Expr::IntLit(20), Expr::IntLit(30)],
                },
            },
            Stmt::Let {
                name: "s".into(),
                ty: Ty::Str,
                init: Expr::StrLit("abc".into()),
            },
            Stmt::Print(vec![idx(Expr::Var("xs".into()), -1, Ty::Int)]),
            Stmt::Print(vec![idx(Expr::Var("xs".into()), -3, Ty::Int)]),
            Stmt::Print(vec![idx(Expr::Var("s".into()), -1, Ty::Str)]),
        ],
    };
    let (outcome, chz, py) = run::run_program(&cfg, &prog);
    assert!(
        matches!(outcome, Outcome::Match | Outcome::AllowListed(_)),
        "{}",
        difftest::describe(0, &outcome, &chz, &py)
    );
}

/// `in` membership: list element, map key, substring (present, absent, empty).
#[test]
fn p0_membership() {
    let cfg = config();
    let in_ = |l: Expr, r: Expr| Expr::Bin {
        op: BinOp::In,
        ty: Ty::Bool,
        l: Box::new(l),
        r: Box::new(r),
    };
    let prog = Program {
        funcs: vec![],
        main: vec![
            Stmt::Let {
                name: "xs".into(),
                ty: Ty::List(Box::new(Ty::Int)),
                init: Expr::ListLit {
                    elem: Ty::Int,
                    items: vec![Expr::IntLit(1), Expr::IntLit(2), Expr::IntLit(3)],
                },
            },
            Stmt::Let {
                name: "m".into(),
                ty: Ty::Map(Box::new(Ty::Str), Box::new(Ty::Int)),
                init: Expr::MapLit {
                    k: Ty::Str,
                    v: Ty::Int,
                    entries: vec![(Expr::StrLit("a".into()), Expr::IntLit(1))],
                },
            },
            Stmt::Let {
                name: "s".into(),
                ty: Ty::Str,
                init: Expr::StrLit("abcdef".into()),
            },
            Stmt::Print(vec![in_(Expr::IntLit(2), Expr::Var("xs".into()))]),
            Stmt::Print(vec![in_(Expr::IntLit(9), Expr::Var("xs".into()))]),
            Stmt::Print(vec![in_(Expr::StrLit("a".into()), Expr::Var("m".into()))]),
            Stmt::Print(vec![in_(Expr::StrLit("z".into()), Expr::Var("m".into()))]),
            Stmt::Print(vec![in_(Expr::StrLit("cd".into()), Expr::Var("s".into()))]),
            Stmt::Print(vec![in_(Expr::StrLit("".into()), Expr::Var("s".into()))]),
        ],
    };
    let (outcome, chz, py) = run::run_program(&cfg, &prog);
    assert!(
        matches!(outcome, Outcome::Match | Outcome::AllowListed(_)),
        "{}",
        difftest::describe(0, &outcome, &chz, &py)
    );
}

/// All eight string methods on ASCII (non-empty replace `old` / split `sep`).
#[test]
fn p0_string_methods() {
    let cfg = config();
    let m = |recv: Expr, method: Method, args: Vec<Expr>, ret: Ty| Expr::Method {
        recv: Box::new(recv),
        method,
        args,
        ret,
    };
    let s = || Expr::Var("s".into());
    let prog = Program {
        funcs: vec![],
        main: vec![
            Stmt::Let {
                name: "s".into(),
                ty: Ty::Str,
                init: Expr::StrLit("Hello World".into()),
            },
            Stmt::Print(vec![m(s(), Method::Upper, vec![], Ty::Str)]),
            Stmt::Print(vec![m(s(), Method::Lower, vec![], Ty::Str)]),
            Stmt::Print(vec![m(
                s(),
                Method::Replace,
                vec![Expr::StrLit("l".into()), Expr::StrLit("L".into())],
                Ty::Str,
            )]),
            Stmt::Print(vec![m(
                s(),
                Method::Split,
                vec![Expr::StrLit(" ".into())],
                Ty::List(Box::new(Ty::Str)),
            )]),
            Stmt::Print(vec![m(
                Expr::StrLit("-".into()),
                Method::Join,
                vec![Expr::ListLit {
                    elem: Ty::Str,
                    items: vec![Expr::StrLit("a".into()), Expr::StrLit("b".into())],
                }],
                Ty::Str,
            )]),
            Stmt::Print(vec![m(
                s(),
                Method::StartsWith,
                vec![Expr::StrLit("Hell".into())],
                Ty::Bool,
            )]),
            Stmt::Print(vec![m(
                s(),
                Method::EndsWith,
                vec![Expr::StrLit("rld".into())],
                Ty::Bool,
            )]),
            Stmt::Print(vec![m(
                s(),
                Method::Contains,
                vec![Expr::StrLit("o W".into())],
                Ty::Bool,
            )]),
            Stmt::Print(vec![m(
                s(),
                Method::Contains,
                vec![Expr::StrLit("".into())],
                Ty::Bool,
            )]),
        ],
    };
    let (outcome, chz, py) = run::run_program(&cfg, &prog);
    assert!(
        matches!(outcome, Outcome::Match | Outcome::AllowListed(_)),
        "{}",
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

// ---------------------------------------------------------------------------
// Per-construct fuzz sweeps for the widened constructs. Each isolates one new
// flag (core features on so containers/loops exist) so a regression in that
// construct's generator restriction shows up as a finding here, not silently.
// ---------------------------------------------------------------------------

/// Core features on; the four widened flags individually toggled.
fn feat_only(string_methods: bool, slicing: bool, membership: bool, tuples: bool) -> Features {
    let mut f = Features::full();
    f.string_methods = string_methods;
    f.slicing = slicing;
    f.membership = membership;
    f.tuples = tuples;
    f
}

#[test]
fn fuzz_string_methods() {
    fuzz_range(feat_only(true, false, false, false), 0, 200);
}

#[test]
fn fuzz_slicing() {
    fuzz_range(feat_only(false, true, false, false), 0, 200);
}

#[test]
fn fuzz_membership() {
    fuzz_range(feat_only(false, false, true, false), 0, 200);
}

#[test]
fn fuzz_tuples() {
    fuzz_range(feat_only(false, false, false, true), 0, 200);
}

// ---------------------------------------------------------------------------
// Coverage: prove the generator actually emits each widened construct over a
// fixed seed range. Walks the abstract IR directly (unambiguous, no token
// matching) so a wiring regression turns these red.
// ---------------------------------------------------------------------------

fn expr_any(e: &Expr, f: &mut dyn FnMut(&Expr) -> bool) -> bool {
    if f(e) {
        return true;
    }
    match e {
        Expr::Unary { e, .. } => expr_any(e, f),
        Expr::Bin { l, r, .. } => expr_any(l, f) || expr_any(r, f),
        Expr::Call { args, .. } => args.iter().any(|a| expr_any(a, f)),
        Expr::ListLit { items, .. } => items.iter().any(|i| expr_any(i, f)),
        Expr::MapLit { entries, .. } => entries
            .iter()
            .any(|(k, v)| expr_any(k, f) || expr_any(v, f)),
        Expr::Index { base, idx, .. } => expr_any(base, f) || expr_any(idx, f),
        Expr::Slice {
            base,
            start,
            end,
            step,
            ..
        } => {
            expr_any(base, f)
                || start.as_deref().is_some_and(|x| expr_any(x, f))
                || end.as_deref().is_some_and(|x| expr_any(x, f))
                || step.as_deref().is_some_and(|x| expr_any(x, f))
        }
        Expr::Method { recv, args, .. } => expr_any(recv, f) || args.iter().any(|a| expr_any(a, f)),
        Expr::TupleLit(items) => items.iter().any(|i| expr_any(i, f)),
        Expr::TupleField { base, .. } => expr_any(base, f),
        Expr::Len(b) => expr_any(b, f),
        Expr::IntLit(_) | Expr::BoolLit(_) | Expr::StrLit(_) | Expr::FloatLit(_) | Expr::Var(_) => {
            false
        }
    }
}

fn stmt_any(s: &Stmt, f: &mut dyn FnMut(&Expr) -> bool) -> bool {
    match s {
        Stmt::Let { init, .. } => expr_any(init, f),
        Stmt::Assign { value, .. } => expr_any(value, f),
        Stmt::Unpack { init, .. } => expr_any(init, f),
        Stmt::If { cond, then, els } => {
            expr_any(cond, f)
                || then.iter().any(|s| stmt_any(s, f))
                || els
                    .as_ref()
                    .is_some_and(|b| b.iter().any(|s| stmt_any(s, f)))
        }
        Stmt::While { cond, body } => expr_any(cond, f) || body.iter().any(|s| stmt_any(s, f)),
        Stmt::ForRange {
            start, end, body, ..
        } => expr_any(start, f) || expr_any(end, f) || body.iter().any(|s| stmt_any(s, f)),
        Stmt::Print(args) => args.iter().any(|a| expr_any(a, f)),
        Stmt::Return(e) => e.as_ref().is_some_and(|e| expr_any(e, f)),
        Stmt::Eval(e) => expr_any(e, f),
    }
}

/// True if any seed in `0..n` (with `feat`) produces a program whose IR satisfies `pred`
/// (an Expr predicate) or `stmt_pred` (a Stmt predicate).
fn emits<F, G>(feat: Features, n: u64, mut pred: F, mut stmt_pred: G) -> bool
where
    F: FnMut(&Expr) -> bool,
    G: FnMut(&Stmt) -> bool,
{
    for seed in 0..n {
        let p = difftest::generate::gen_program(seed, feat);
        let mut hit = false;
        let scan = |b: &Block, hit: &mut bool, pred: &mut F| {
            for s in b {
                if stmt_any(s, pred) {
                    *hit = true;
                }
            }
        };
        for func in &p.funcs {
            scan(&func.body, &mut hit, &mut pred);
            for s in &func.body {
                if stmt_pred(s) {
                    hit = true;
                }
            }
        }
        scan(&p.main, &mut hit, &mut pred);
        for s in &p.main {
            if stmt_pred(s) {
                hit = true;
            }
        }
        if hit {
            return true;
        }
    }
    false
}

#[test]
fn gen_emits_string_method() {
    assert!(
        emits(
            feat_only(true, false, false, false),
            400,
            |e| matches!(e, Expr::Method { .. }),
            |_| false
        ),
        "generator never emitted a string method"
    );
}

#[test]
fn gen_emits_slice() {
    assert!(
        emits(
            feat_only(false, true, false, false),
            400,
            |e| matches!(e, Expr::Slice { .. }),
            |_| false
        ),
        "generator never emitted a slice"
    );
}

#[test]
fn gen_emits_negative_index() {
    assert!(
        emits(
            feat_only(false, true, false, false),
            400,
            |e| matches!(e, Expr::Index { idx, .. } if matches!(idx.as_ref(), Expr::IntLit(n) if *n < 0)),
            |_| false,
        ),
        "generator never emitted a negative index"
    );
}

#[test]
fn gen_emits_membership() {
    assert!(
        emits(
            feat_only(false, false, true, false),
            400,
            |e| matches!(e, Expr::Bin { op: BinOp::In, .. }),
            |_| false
        ),
        "generator never emitted an `in` membership test"
    );
}

#[test]
fn gen_emits_tuple() {
    assert!(
        emits(
            feat_only(false, false, false, true),
            400,
            |e| matches!(e, Expr::TupleLit(_) | Expr::TupleField { .. }),
            |s| matches!(s, Stmt::Unpack { .. }),
        ),
        "generator never emitted a tuple"
    );
}
