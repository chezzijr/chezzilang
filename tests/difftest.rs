//! CPython differential oracle — CI gate.
//!
//! Shares the engine with `src/bin/difffuzz.rs` via a `#[path]` include (the diff engine is not
//! exposed by the `chezzi` library, so it is pulled in by path rather than `use`).
//! `env!("CARGO_BIN_EXE_chezzi")` is provided by cargo for integration tests.

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

/// Float `/` is total IEEE-754 in Chezzi (`docs/spec.md:472`): a zero divisor is `inf`/`-inf`/`NaN`,
/// never a fault — a DELIBERATE divergence from CPython, which raises `ZeroDivisionError` on the
/// same expression (Chezzi follows Rust/Go here, confirmed by the project owner 2026-08-07). The
/// shim's `_chz_fdiv` absorbs this, exactly like `_chz_div`/`_chz_mod` absorb truncating int
/// division. All four sign combinations of a zero divisor are covered because signed zero is the
/// part a naive `if b == 0: inf if a > 0 else -inf` gets wrong (two of the four cases flip): pins
/// `_chz_fdiv`'s `copysign` pair against ever being "simplified" back to that.
#[test]
fn p0_shim_float_div_signed_zero() {
    let cfg = config();
    let cases: [(f64, f64); 5] = [
        (1.0, 0.0),
        (1.0, -0.0),
        (-1.0, 0.0),
        (-1.0, -0.0),
        (0.0, 0.0),
    ];
    for (a, b) in cases {
        let prog = Program {
            funcs: vec![],
            main: vec![Stmt::Print(vec![Expr::Bin {
                op: BinOp::Div,
                ty: Ty::Float,
                l: Box::new(Expr::FloatLit(a)),
                r: Box::new(Expr::FloatLit(b)),
            }])],
        };
        let (outcome, chz, py) = run::run_program(&cfg, &prog);
        assert!(
            matches!(outcome, Outcome::Match | Outcome::AllowListed(_)),
            "float div {a}/{b}: {}",
            difftest::describe(0, &outcome, &chz, &py)
        );
    }
}

/// A mixed `Div` (`ty: Float`, dividend a `FloatLit`, divisor an INT `0`) exercises the exact
/// same `_chz_fdiv` chain as `p0_shim_float_div_signed_zero`, but through the mixed-arithmetic
/// path this task adds: `emit_python.rs`'s `bin()` routes on `ty` alone, so a `Float`-typed `Div`
/// node reaches `_chz_fdiv` regardless of whether the divisor operand is textually an int or a
/// float literal, and `_chz_fdiv`'s guard is `b == 0.0` — Python's `0 == 0.0` is `True`, so an
/// integer `0` divisor takes the same `inf`/`-inf` branch a float `0.0` divisor would. Pins that
/// chain for the mixed case specifically: if a future change ever narrows the guard to something
/// that stops matching an integer `0` (e.g. `b is 0.0`, or an `isinstance` check), this goes red
/// where the all-float `p0_shim_float_div_signed_zero` would not.
#[test]
fn p0_shim_mixed_float_div_zero_divisor() {
    let cfg = config();
    for dividend in [1.0, -1.0] {
        let prog = Program {
            funcs: vec![],
            main: vec![Stmt::Print(vec![Expr::Bin {
                op: BinOp::Div,
                ty: Ty::Float,
                l: Box::new(Expr::FloatLit(dividend)),
                r: Box::new(Expr::IntLit(0)),
            }])],
        };
        let (outcome, chz, py) = run::run_program(&cfg, &prog);
        assert!(
            matches!(outcome, Outcome::Match | Outcome::AllowListed(_)),
            "mixed float div {dividend}/0: {}",
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
    fuzz_range_cfg(&config(), feat, start, end);
}

/// `Config`-parametrized so a test can point it at a broken harness (e.g. a `chezzi_bin` that
/// does not exist) and pin the abort behavior below, without spawning the real 20s-timeout `chezzi`.
fn fuzz_range_cfg(cfg: &Config, feat: Features, start: u64, end: u64) {
    let mut findings = Vec::new();
    // Same outcome histogram the `difffuzz` binary prints, for the same reason (W7-34's
    // residual): a sweep where every seed timed out compared NOTHING, and a bare green tick is
    // byte-identical to a sweep that really did compare 3000 programs. `--nocapture` shows it;
    // a failing run prints it unconditionally. Deliberately NOT an assertion threshold — a
    // heuristic that cannot be certain must stay legible rather than emit a confident wrong
    // verdict about the machine it happens to be running on.
    let mut hist: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for seed in start..end {
        let (outcome, chz, py) = difftest::run_seed(cfg, seed, feat);
        *hist.entry(difftest::kind_label(&outcome)).or_default() += 1;
        // A harness error means the oracle never ran this seed at all — not a divergence, and
        // not something to accumulate: 3000 identical ENOENT messages would help nobody, so
        // fail on the first one instead of burying it in a findings list it isn't a member of.
        if let Outcome::HarnessError(msg) = &outcome {
            // Don't let the abort silently swallow real divergences already confirmed earlier
            // in this same range — the harness broke, but those findings are still real.
            if findings.is_empty() {
                panic!("harness error at seed {seed}: {msg}");
            }
            panic!(
                "harness error at seed {seed}: {msg}\n\n{} earlier finding(s) already confirmed before the harness broke:\n{}",
                findings.len(),
                findings.join("\n")
            );
        }
        if outcome.is_finding() {
            findings.push(difftest::describe(seed, &outcome, &chz, &py));
        }
    }
    // Outcomes that represent a REAL comparison of the two engines. `Timeout` and `HarnessError`
    // are precisely the two that mean "nothing was compared".
    let compared: usize = hist
        .iter()
        .filter(|(k, _)| {
            [
                "Match",
                "AllowListed",
                "Divergence",
                "HostPanic",
                "BothError",
            ]
            .contains(k)
        })
        .map(|(_, n)| n)
        .sum();
    let hist: Vec<String> = hist.iter().map(|(k, n)| format!("{k} {n}")).collect();
    eprintln!(
        "fuzz sweep {start}..{end}: {} finding(s), {compared} compared [{}]",
        findings.len(),
        hist.join(", ")
    );
    assert!(
        findings.is_empty(),
        "differential divergences found (sweep {start}..{end} [{}]):\n{}",
        hist.join(", "),
        findings.join("\n")
    );
    // The histogram above is `eprintln!`, and libtest CAPTURES stderr on a PASSING test — so a
    // sweep where every seed timed out (nothing compared at all) is a green tick indistinguishable
    // from one that really compared 120 programs. Acknowledging the capture in a comment is not a
    // guard; this is. A hard "zero comparisons" floor is CERTAIN — deliberately not a "too many
    // timeouts" threshold, which is a heuristic, and an uncertain heuristic must decline rather
    // than guess (`docs/gaps.md` W7-12, W7-38). It also closes the empty-range hole the sibling
    // arg parsers had: `fuzz_range(feat, 5, 5)` now fails instead of passing over nothing.
    assert!(
        compared > 0,
        "vacuous sweep {start}..{end}: compared 0 of {} seeds — nothing was ever run against \
         CPython, so a green result here proves NOTHING [{}]",
        end - start,
        hist.join(", ")
    );
}

/// This is `fuzz_range`'s own consumer of `Outcome::HarnessError` — the CI gate's abort path —
/// pinned directly: a `chezzi_bin` that does not exist must panic with a message naming the
/// problem, not silently score the range as "0 findings". `#[should_panic]` is appropriate
/// because panicking IS `fuzz_range_cfg`'s contract for this input; `expected` is specific
/// enough ("harness error at seed") that a panic from some other cause (e.g. an actual
/// divergence, which panics via `assert!` with a different message) would not satisfy it.
#[test]
#[should_panic(expected = "harness error at seed")]
fn fuzz_range_aborts_on_harness_error() {
    let cfg = Config::new("/nonexistent/chezzi-does-not-exist");
    fuzz_range_cfg(&cfg, Features::straight_line(), 0, 5);
}

/// The gate must refuse to pass over ZERO comparisons (W7-38). Forced with a 1 ms timeout, which
/// no real `chezzi run` + `python3` pair can beat, so every seed comes back `Timeout` — the shape
/// that used to be a silent green because libtest captures the histogram on a passing test.
#[test]
#[should_panic(expected = "compared 0 of")]
fn fuzz_range_refuses_to_pass_over_zero_comparisons() {
    let mut cfg = config();
    cfg.timeout = Duration::from_millis(1);
    fuzz_range_cfg(&cfg, Features::straight_line(), 0, 2);
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

/// Core features plus floats. Kept as its own gate even once `Features::full()` turns floats on,
/// so a future `full()` edit cannot silently take float coverage back out.
fn feat_floats() -> Features {
    let mut f = Features::full();
    f.floats = true;
    f
}

#[test]
fn fuzz_floats() {
    // `feat_floats()` is `Features::full()` with `floats` forced true, and `full().floats` is
    // already `true` (§W7-37) — so today `feat_floats() == full()`, and seeds 0..120 here would
    // regenerate byte-identical programs to `fuzz_full`'s 0..120. Disjoint range so the ~15s this
    // sweep costs buys new coverage instead of repeating `fuzz_full`'s work; the fence itself is
    // still real (it survives someone flipping `full().floats` back to `false`).
    fuzz_range(feat_floats(), 5000, 5200);
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

/// Floats must reach the generator as *arithmetic*, not just as a literal both emitters render
/// from the same `float_lit` — a literal-only float is byte-identical by construction, so a
/// green float sweep over it proves nothing. This is the probe that would have gone red for the
/// pre-`W7-32` oracle's blind spot.
#[test]
fn gen_emits_float_binop() {
    assert!(
        emits(
            feat_floats(),
            400,
            |e| matches!(e, Expr::Bin { ty: Ty::Float, .. }),
            |_| false
        ),
        "generator never emitted a float binary operation"
    );
}

/// Honest float-operand detector for the comparison probe below. `Expr::Bin`'s `ty` is the
/// RESULT type (`Ty::Bool` for every comparison), so matching on it cannot tell an int
/// comparison from a float one — this walks an operand's own shape instead. It recognizes a
/// float through `FloatLit`, a float arithmetic `Bin` (`ty: Ty::Float`), a float-returning
/// `Call`, or a float-returning `Index` — but NOT a float `Var`, since `Expr::Var` carries no
/// type in this IR at all (int and float vars are indistinguishable from the bare node). That
/// blind spot is acceptable here: it only means some float-comparison hits go unseen by this
/// probe, not that the probe can pass vacuously — a `FloatLit`-only predicate would also miss
/// the interesting shapes, which is exactly the failure mode this is guarding against.
///
/// The blind spot is an UNDER-count, measured, not assumed: dumping the emitted source for seeds
/// 0..400 shows float-`Var` comparisons really are generated (`(6.125 > v2)` at seed 2,
/// `(v6 != -0.00000000000000000008131516293641283)` at seed 3) alongside the shapes this
/// predicate does see (`(-2146246697418752.0 > (3.375 - -7.75))` at seed 1). So the probe
/// fires on strictly fewer programs than actually contain a float comparison — the safe
/// direction: it can go red spuriously, never green vacuously.
fn is_float_operand(e: &Expr) -> bool {
    matches!(
        e,
        Expr::FloatLit(_)
            | Expr::Bin { ty: Ty::Float, .. }
            | Expr::Call { ret: Ty::Float, .. }
            | Expr::Index { ret: Ty::Float, .. }
    )
}

/// Comparison in `gen_bool` used to call `gen_int` for BOTH operands unconditionally, so
/// `< <= > >= == !=` never ran on floats — everything else float-related exercises how a float
/// *renders*, never a VALUE comparison. See `is_float_operand` for why this can't just match on
/// `Expr::Bin { ty: Ty::Bool, .. }` — that's true of every comparison, int or float alike.
#[test]
fn gen_emits_float_comparison() {
    assert!(
        emits(
            feat_floats(),
            400,
            |e| matches!(
                e,
                Expr::Bin {
                    op: BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne,
                    l,
                    ..
                } if is_float_operand(l)
            ),
            |_| false
        ),
        "generator never emitted a float comparison"
    );
}

/// `gen_float` used to choose only from `[Add, Sub, Mul]` — `Div` never appeared, so
/// `emit_python.rs`'s float-`Div` shim routing (`_chz_fdiv`) was dead code by construction, exactly
/// the `W7-37` failure mode (coverage both engines "agree" on because neither runs it). Mutation-
/// proven: reverting `gen_float`'s op list back to `[Add, Sub, Mul]` turns this red.
#[test]
fn gen_emits_float_div() {
    assert!(
        emits(
            feat_floats(),
            400,
            |e| matches!(
                e,
                Expr::Bin {
                    op: BinOp::Div,
                    ty: Ty::Float,
                    ..
                }
            ),
            |_| false
        ),
        "generator never emitted a float division"
    );
}

/// Honest int-operand detector, mirror of `is_float_operand` above (same blind spot: `Expr::Var`
/// carries no type in this IR, so an int var is indistinguishable from a float var at the bare
/// node — not reproduced as a second float-detection scheme, just the same shapes typed for Int).
fn is_int_operand(e: &Expr) -> bool {
    matches!(
        e,
        Expr::IntLit(_)
            | Expr::Bin { ty: Ty::Int, .. }
            | Expr::Call { ret: Ty::Int, .. }
            | Expr::Index { ret: Ty::Int, .. }
    )
}

/// `gen_float`'s composite arm used to draw both operands from `gen_float` only — int↔float
/// mixed arithmetic (`1 + 2.0`) was never generated, the last item `docs/gaps.md` W7-37 deferred.
/// `is_int_operand`/`is_float_operand` are each an UNDER-count (the `Var` blind spot), so this
/// probe can go red spuriously but never pass vacuously: it only fires on a `Bin { ty: Float }`
/// where it can positively identify one int-shaped operand AND one float-shaped operand, which
/// is a strict subset of all mixed nodes actually generated.
#[test]
fn gen_emits_mixed_int_float_arith() {
    assert!(
        emits(
            feat_floats(),
            400,
            |e| matches!(
                e,
                Expr::Bin { ty: Ty::Float, l, r, .. }
                    if (is_int_operand(l) && is_float_operand(r))
                        || (is_float_operand(l) && is_int_operand(r))
            ),
            |_| false
        ),
        "generator never emitted a mixed int/float binary operation"
    );
}

/// `try_call` used to be asked only for `Ty::Int`, so ~2/3 of generated functions were emitted
/// and never invoked — code both engines "agreed" on because neither ran it.
#[test]
fn gen_emits_non_int_call() {
    assert!(
        emits(
            feat_floats(),
            400,
            |e| matches!(e, Expr::Call { ret, .. } if *ret != Ty::Int),
            |_| false
        ),
        "generator never emitted a call to a non-int-returning function"
    );
}

/// Same for `try_index`: element reads on `List[str]` / `List[bool]` / `Map[_, float]` were
/// never generated.
#[test]
fn gen_emits_non_int_index() {
    assert!(
        emits(
            feat_floats(),
            400,
            |e| matches!(e, Expr::Index { ret, .. } if *ret != Ty::Int),
            |_| false
        ),
        "generator never emitted a non-int index read"
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
