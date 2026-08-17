//! `std.math` — native math intrinsics (M6c).
//!
//! Most functions take and return `float` (Chezzi has no implicit int→float, so callers pass
//! floats; the checker's `native_module_sig("std.math")` enforces this). The exception is `abs`,
//! which is numeric-polymorphic (gap #12): int args → int, float args → float. (`min`/`max` are
//! NOT here — they live in `std.cmp` as generic `[T: Comparable]` functions, M7-G3.)
//! Pure Rust `std` — no third-party crates.

use super::{Host, HostError, Kind, NativeFn, NativeRet, expect_args};

// `abs` is numeric-polymorphic (gap #12): int args yield an int result, float args a float. The
// checker (`infer_numeric_poly`) guarantees the arg is present and numeric, so `arg_is_int(0)`
// decides the whole call.

fn abs(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "abs", 1)?;
    if h.arg_is_int(0) {
        // `i64::MIN.abs()` has no representable result (panics in debug, wraps in release). Surface a
        // recoverable overflow instead — matches the engines' checked `"integer overflow in <Op>"`.
        let v = h.arg_int(0)?.checked_abs().ok_or(HostError {
            message: "integer overflow in abs".to_string(),
        })?;
        Ok(NativeRet::Int(v))
    } else {
        Ok(NativeRet::Float(h.arg_float(0)?.abs()))
    }
}

/// A plain 1-arg `float -> float` intrinsic: `expect_args` then the same-named `f64` method.
/// Out-of-domain inputs (e.g. `asin(2.0)`, `ln(-1.0)`, `sqrt(-1.0)`) return NaN, never a fault,
/// keeping the signatures plain `float`. One `f64` op, one code path.
macro_rules! unary_float {
    ($name:ident) => {
        fn $name(h: &mut dyn Host) -> Result<NativeRet, HostError> {
            expect_args(h, stringify!($name), 1)?;
            Ok(NativeRet::Float(h.arg_float(0)?.$name()))
        }
    };
}

unary_float!(floor);
unary_float!(ceil);
unary_float!(round);

fn pow(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "pow", 2)?;
    Ok(NativeRet::Float(h.arg_float(0)?.powf(h.arg_float(1)?)))
}

unary_float!(sqrt);

// Trig / exp / log intrinsics (additive, M19-safe) — plain `float -> float` via `unary_float!`.
unary_float!(sin);
unary_float!(cos);
unary_float!(tan);
unary_float!(asin);
unary_float!(acos);
unary_float!(atan);

fn atan2(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "atan2", 2)?;
    let y = h.arg_float(0)?;
    let x = h.arg_float(1)?;
    Ok(NativeRet::Float(y.atan2(x)))
}

unary_float!(exp);
unary_float!(ln);
unary_float!(log2);
unary_float!(log10);

fn log(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "log", 2)?;
    let value = h.arg_float(0)?;
    let base = h.arg_float(1)?;
    Ok(NativeRet::Float(value.log(base)))
}

// Float predicates (IEEE-754 classification): `float -> bool`. Now that float arithmetic is total
// (inf/NaN are values), these let user code inspect a result. Plain pass-throughs over f64's own
// classifiers — plain pass-throughs, no engine bookkeeping (the `NativeRet::Bool` seam is already wired).

fn is_nan(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "is_nan", 1)?;
    Ok(NativeRet::Bool(h.arg_float(0)?.is_nan()))
}

fn is_inf(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "is_inf", 1)?;
    Ok(NativeRet::Bool(h.arg_float(0)?.is_infinite()))
}

fn is_finite(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "is_finite", 1)?;
    Ok(NativeRet::Bool(h.arg_float(0)?.is_finite()))
}

// ---- Number theory / integer math (gap §5). Python `math` semantics are the anti-drift reference. ----

/// Euclid's GCD on unsigned magnitudes (callers pass `x.unsigned_abs()`). `gcd(0,0)=0`. Working in
/// `u64` avoids the `i64::MIN.abs()` panic — the sign is irrelevant to GCD (Python's `math.gcd` is
/// always non-negative).
fn gcd_u64(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// `lcm(a,b) = |a|/gcd * |b|`, computed on magnitudes to reduce overflow risk (divide before multiply).
/// `lcm(0,·)=0` (Python). Errs — never faults — when the representable result exceeds i64.
fn lcm_i64(a: i64, b: i64) -> Result<i64, String> {
    if a == 0 || b == 0 {
        return Ok(0);
    }
    let (a, b) = (a.unsigned_abs(), b.unsigned_abs());
    let g = gcd_u64(a, b);
    let l = (a / g)
        .checked_mul(b)
        .ok_or_else(|| "integer overflow in lcm".to_string())?;
    i64::try_from(l).map_err(|_| "integer overflow in lcm".to_string())
}

/// `n!` for `0 <= n <= 20` (21! > i64::MAX, so the ceiling is the i64 limit not a design choice).
/// Errs on negative or `n > 20` — never faults.
fn factorial_i64(n: i64) -> Result<i64, String> {
    if n < 0 {
        return Err(format!("factorial: n must be non-negative, got {n}"));
    }
    if n > 20 {
        return Err(format!("factorial: {n}! overflows i64 (max is 20!)"));
    }
    Ok((1..=n).product())
}

/// `C(n,k)` — number of k-combinations. `k>n` or `k<0`... Python raises on negative; we Err. `k>n`
/// yields 0 (Python). Computes multiplicatively in i128, Erring only when the true result exceeds
/// i64 — never faults, never hangs.
fn comb_i64(n: i64, k: i64) -> Result<i64, String> {
    if n < 0 || k < 0 {
        return Err(format!(
            "comb: n and k must be non-negative, got n={n} k={k}"
        ));
    }
    if k > n {
        return Ok(0);
    }
    // Symmetry: C(n,k) == C(n,n-k); use the smaller k for fewer, smaller multiplications.
    let k = k.min(n - k);
    // `acc` holds C(n,i) exactly at the top of each step, kept <= i64::MAX by the in-loop guard
    // (like perm_i64). That bounds the intermediate `acc*(n-i)` to ~i64::MAX^2 (~8.5e37) < i128::MAX
    // (~1.7e38), so the multiply can't overflow i128, and any oversized result Errs early — which
    // also bounds the iteration count (C(n,·) climbs past i64::MAX within a few steps for large n).
    let mut acc: i128 = 1;
    for i in 0..k {
        acc = acc * (n - i) as i128 / (i + 1) as i128;
        if acc > i64::MAX as i128 {
            return Err(format!("comb: C({n},{k}) exceeds i64 range"));
        }
    }
    i64::try_from(acc).map_err(|_| format!("comb: C({n},{k}) exceeds i64 range"))
}

/// `P(n,k)` — number of k-permutations. `k>n` yields 0; negative Errs. i128 intermediate, Errs on
/// i64 overflow (never faults).
fn perm_i64(n: i64, k: i64) -> Result<i64, String> {
    if n < 0 || k < 0 {
        return Err(format!(
            "perm: n and k must be non-negative, got n={n} k={k}"
        ));
    }
    if k > n {
        return Ok(0);
    }
    let mut acc: i128 = 1;
    for i in 0..k {
        acc *= (n - i) as i128;
        if acc > i64::MAX as i128 {
            return Err(format!("perm: P({n},{k}) exceeds i64 range"));
        }
    }
    i64::try_from(acc).map_err(|_| format!("perm: P({n},{k}) exceeds i64 range"))
}

/// Parse `s` in `base` (Go `strconv.ParseInt` / Python `int(s, base)`). `base` is 0 or 2..=36; base 0
/// auto-detects a `0x`/`0o`/`0b` prefix (else decimal), and bases 2/8/16 accept the matching prefix.
/// Leading `+`/`-` sign allowed; empty/malformed digits Err (never fault).
fn parse_int_base_impl(s: &str, base: i64) -> Result<i64, String> {
    if base != 0 && !(2..=36).contains(&base) {
        return Err(format!(
            "parse_int_base: base must be 0 or 2..=36, got {base}"
        ));
    }
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let lower = rest.to_ascii_lowercase();
    let (radix, digits): (u32, &str) = if base == 0 {
        if lower.starts_with("0x") {
            (16, &rest[2..])
        } else if lower.starts_with("0o") {
            (8, &rest[2..])
        } else if lower.starts_with("0b") {
            (2, &rest[2..])
        } else {
            (10, rest)
        }
    } else {
        let radix = base as u32;
        let stripped = match radix {
            16 => lower.strip_prefix("0x").map(|_| &rest[2..]),
            8 => lower.strip_prefix("0o").map(|_| &rest[2..]),
            2 => lower.strip_prefix("0b").map(|_| &rest[2..]),
            _ => None,
        };
        (radix, stripped.unwrap_or(rest))
    };
    // Reject a second/embedded sign: after the one leading sign + optional base prefix, `digits`
    // must be bare radix digits. `from_str_radix` would otherwise re-accept a leading +/- here
    // ("+-5", "0x-5", "0b+1"), which Python int()/Go ParseInt reject.
    if digits.starts_with('+') || digits.starts_with('-') {
        return Err(format!(
            "parse_int_base: cannot parse '{s}' in base {radix}"
        ));
    }
    // Re-attach the sign and let `from_str_radix` parse it directly, so i64::MIN (whose magnitude
    // is i64::MAX+1 and cannot be parsed-then-negated) round-trips like Python/Go.
    let signed = if neg {
        format!("-{digits}")
    } else {
        digits.to_string()
    };
    i64::from_str_radix(&signed, radix)
        .map_err(|_| format!("parse_int_base: cannot parse '{s}' in base {radix}"))
}

fn gcd(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "gcd", 2)?;
    let g = gcd_u64(h.arg_int(0)?.unsigned_abs(), h.arg_int(1)?.unsigned_abs());
    // Only the 2^63 corner (an input is i64::MIN, the other 0/i64::MIN) overflows i64 — fault like abs.
    let v = i64::try_from(g).map_err(|_| HostError {
        message: "integer overflow in gcd".to_string(),
    })?;
    Ok(NativeRet::Int(v))
}

fn lcm(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "lcm", 2)?;
    let a = h.arg_int(0)?;
    let b = h.arg_int(1)?;
    lcm_i64(a, b)
        .map(NativeRet::Int)
        .map_err(|message| HostError { message })
}

// `sign` is numeric-polymorphic (like `abs`): int→int, float→float. numpy/Go convention -1/0/1.
fn sign(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "sign", 1)?;
    if h.arg_is_int(0) {
        Ok(NativeRet::Int(h.arg_int(0)?.signum()))
    } else {
        let x = h.arg_float(0)?;
        // f64::signum returns ±1.0 for ±0.0 and NaN for NaN — override to 0.0 / NaN-passthrough.
        let s = if x == 0.0 || x.is_nan() {
            x
        } else {
            x.signum()
        };
        Ok(NativeRet::Float(s))
    }
}

// `trunc(x) -> int`: toward-zero truncation. Equivalent to `int(x)`; faults on non-finite / out-of-range
// with the same message shape as the `int()` builtin (no intra-language drift).
fn trunc(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "trunc", 1)?;
    let x = h.arg_float(0)?;
    if !x.is_finite() || x < i64::MIN as f64 || x >= 9_223_372_036_854_775_808.0 {
        return Err(HostError {
            message: format!("trunc(): {x} is out of integer range"),
        });
    }
    Ok(NativeRet::Int(x.trunc() as i64))
}

fn hypot(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "hypot", 2)?;
    let x = h.arg_float(0)?;
    let y = h.arg_float(1)?;
    Ok(NativeRet::Float(x.hypot(y)))
}

fn cbrt(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "cbrt", 1)?;
    Ok(NativeRet::Float(h.arg_float(0)?.cbrt()))
}

fn factorial(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "factorial", 1)?;
    Ok(match factorial_i64(h.arg_int(0)?) {
        Ok(v) => NativeRet::Ok(Box::new(NativeRet::Int(v))),
        Err(msg) => NativeRet::Err(msg),
    })
}

fn comb(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "comb", 2)?;
    let n = h.arg_int(0)?;
    let k = h.arg_int(1)?;
    Ok(match comb_i64(n, k) {
        Ok(v) => NativeRet::Ok(Box::new(NativeRet::Int(v))),
        Err(msg) => NativeRet::Err(msg),
    })
}

fn perm(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "perm", 2)?;
    let n = h.arg_int(0)?;
    let k = h.arg_int(1)?;
    Ok(match perm_i64(n, k) {
        Ok(v) => NativeRet::Ok(Box::new(NativeRet::Int(v))),
        Err(msg) => NativeRet::Err(msg),
    })
}

fn parse_int_base(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "parse_int_base", 2)?;
    let s = h.arg_str(0)?;
    let base = h.arg_int(1)?;
    Ok(match parse_int_base_impl(&s, base) {
        Ok(v) => NativeRet::Ok(Box::new(NativeRet::Int(v))),
        Err(msg) => NativeRet::Err(msg),
    })
}

/// Callable members. `(name, fn, kind)`.
pub const MEMBERS: &[(&str, NativeFn, Kind)] = &[
    ("abs", abs, Kind::Inline),
    ("floor", floor, Kind::Inline),
    ("ceil", ceil, Kind::Inline),
    ("round", round, Kind::Inline),
    ("pow", pow, Kind::Inline),
    ("sqrt", sqrt, Kind::Inline),
    ("sin", sin, Kind::Inline),
    ("cos", cos, Kind::Inline),
    ("tan", tan, Kind::Inline),
    ("asin", asin, Kind::Inline),
    ("acos", acos, Kind::Inline),
    ("atan", atan, Kind::Inline),
    ("atan2", atan2, Kind::Inline),
    ("exp", exp, Kind::Inline),
    ("ln", ln, Kind::Inline),
    ("log2", log2, Kind::Inline),
    ("log10", log10, Kind::Inline),
    ("log", log, Kind::Inline),
    ("is_nan", is_nan, Kind::Inline),
    ("is_inf", is_inf, Kind::Inline),
    ("is_finite", is_finite, Kind::Inline),
    ("gcd", gcd, Kind::Inline),
    ("lcm", lcm, Kind::Inline),
    ("sign", sign, Kind::Inline),
    ("trunc", trunc, Kind::Inline),
    ("hypot", hypot, Kind::Inline),
    ("cbrt", cbrt, Kind::Inline),
    ("factorial", factorial, Kind::Inline),
    ("comb", comb, Kind::Inline),
    ("perm", perm, Kind::Inline),
    ("parse_int_base", parse_int_base, Kind::Inline),
];

/// Constant members. `(name, value)`.
pub const CONSTS: &[(&str, f64)] = &[
    ("pi", std::f64::consts::PI),
    ("e", std::f64::consts::E),
    ("inf", f64::INFINITY),
    ("nan", f64::NAN),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal float-only `Host` for exercising the trig/exp/log natives in isolation.
    struct FloatHost {
        floats: Vec<f64>,
    }

    impl Host for FloatHost {
        fn arg_count(&self) -> usize {
            self.floats.len()
        }
        fn arg_int(&mut self, _i: usize) -> Result<i64, HostError> {
            Err(HostError {
                message: "no int args".into(),
            })
        }
        fn arg_float(&mut self, i: usize) -> Result<f64, HostError> {
            self.floats.get(i).copied().ok_or(HostError {
                message: "missing arg".into(),
            })
        }
        fn arg_is_int(&self, _i: usize) -> bool {
            false
        }
        fn arg_str(&mut self, _i: usize) -> Result<String, HostError> {
            Err(HostError {
                message: "no str args".into(),
            })
        }
        fn arg_str_map(&mut self, _i: usize) -> Result<Vec<(String, String)>, HostError> {
            Err(HostError {
                message: "no map args".into(),
            })
        }
        fn write_stdout(&mut self, _s: &str) {}
        fn write_stderr(&mut self, _s: &str) {}
        fn read_line(&mut self) -> Result<Option<String>, HostError> {
            Ok(None)
        }
        fn os_args(&self) -> Vec<String> {
            vec![]
        }
        fn os_env(&self, _key: &str) -> Option<String> {
            None
        }
        fn os_getcwd(&self) -> Result<Vec<u8>, HostError> {
            Ok(b"/".to_vec())
        }
    }

    fn call1(f: NativeFn, x: f64) -> f64 {
        let mut h = FloatHost { floats: vec![x] };
        match f(&mut h).unwrap() {
            NativeRet::Float(v) => v,
            other => panic!("expected Float, got {other:?}"),
        }
    }

    fn call2(f: NativeFn, a: f64, b: f64) -> f64 {
        let mut h = FloatHost { floats: vec![a, b] };
        match f(&mut h).unwrap() {
            NativeRet::Float(v) => v,
            other => panic!("expected Float, got {other:?}"),
        }
    }

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {a} ~ {b}");
    }

    #[test]
    fn gcd_lcm_helpers() {
        assert_eq!(gcd_u64(0, 0), 0);
        assert_eq!(gcd_u64(12, 8), 4);
        assert_eq!(gcd_u64(0, 5), 5);
        assert_eq!(gcd_u64(5, 0), 5);
        assert_eq!(gcd_u64(17, 5), 1);
        // negatives via unsigned_abs at the seam
        assert_eq!(gcd_u64((-12i64).unsigned_abs(), 8u64), 4);
        assert_eq!(gcd_u64(12u64, (-8i64).unsigned_abs()), 4);
        assert_eq!(lcm_i64(4, 6), Ok(12));
        assert_eq!(lcm_i64(0, 0), Ok(0));
        assert_eq!(lcm_i64(0, 5), Ok(0));
        assert_eq!(lcm_i64(-4, 6), Ok(12));
        assert!(lcm_i64(i64::MAX, i64::MAX - 1).is_err());
    }

    #[test]
    fn factorial_comb_perm_helpers() {
        assert_eq!(factorial_i64(0), Ok(1));
        assert_eq!(factorial_i64(1), Ok(1));
        assert_eq!(factorial_i64(20), Ok(2_432_902_008_176_640_000));
        assert!(factorial_i64(21).is_err());
        assert!(factorial_i64(-1).is_err());
        assert_eq!(comb_i64(5, 2), Ok(10));
        assert_eq!(comb_i64(5, 6), Ok(0));
        assert_eq!(comb_i64(5, 0), Ok(1));
        assert!(comb_i64(-1, 0).is_err());
        assert!(comb_i64(5, -1).is_err());
        assert_eq!(comb_i64(62, 31), Ok(465_428_353_255_261_088));
        assert!(comb_i64(68, 34).is_err());
        // Large-n / small-k: true result « i64 but the pre-fix code's unguarded `acc*(n-i)`
        // overflowed i128 (panic in debug / wrap in release). Must be a clean Err, never a fault.
        assert!(comb_i64(10_000_000_000_000, 3).is_err());
        // Huge central binomial (n>=131 => C(n,k) > i128::MAX): clean Err, no i128 overflow.
        assert!(comb_i64(200, 100).is_err());
        assert!(comb_i64(100_000, 50_000).is_err());
        assert_eq!(perm_i64(5, 2), Ok(20));
        assert_eq!(perm_i64(5, 6), Ok(0));
        assert_eq!(perm_i64(5, 0), Ok(1));
        assert!(perm_i64(-1, 0).is_err());
        assert!(perm_i64(21, 21).is_err());
    }

    #[test]
    fn parse_int_base_helper() {
        assert_eq!(parse_int_base_impl("ff", 16), Ok(255));
        assert_eq!(parse_int_base_impl("0xff", 16), Ok(255));
        assert_eq!(parse_int_base_impl("0xff", 0), Ok(255));
        assert_eq!(parse_int_base_impl("0b101", 0), Ok(5));
        assert_eq!(parse_int_base_impl("0o17", 0), Ok(15));
        assert_eq!(parse_int_base_impl("101", 2), Ok(5));
        assert_eq!(parse_int_base_impl("-2a", 16), Ok(-42));
        assert_eq!(parse_int_base_impl("+10", 10), Ok(10));
        assert_eq!(parse_int_base_impl("42", 0), Ok(42));
        assert!(parse_int_base_impl("  ", 10).is_err());
        assert!(parse_int_base_impl("g", 16).is_err());
        assert!(parse_int_base_impl("0b2", 2).is_err());
        assert!(parse_int_base_impl("10", 37).is_err());
        assert!(parse_int_base_impl("10", 1).is_err());
        // i64::MIN boundary — parse-magnitude-then-negate rejected it (magnitude is i64::MAX+1).
        assert_eq!(
            parse_int_base_impl("-9223372036854775808", 10),
            Ok(i64::MIN)
        );
        assert_eq!(parse_int_base_impl("-8000000000000000", 16), Ok(i64::MIN));
        assert_eq!(parse_int_base_impl("9223372036854775807", 10), Ok(i64::MAX));
        // Embedded/second sign is rejected (Python int()/Go ParseInt), not silently re-accepted.
        assert!(parse_int_base_impl("+-5", 10).is_err());
        assert!(parse_int_base_impl("-+5", 10).is_err());
        assert!(parse_int_base_impl("0x-5", 0).is_err());
        assert!(parse_int_base_impl("0b+1", 0).is_err());
        assert!(parse_int_base_impl("--5", 10).is_err());
    }

    #[test]
    fn trig_exp_log_values() {
        // Exact integer-valued results.
        assert_eq!(call1(sin, 0.0), 0.0);
        assert_eq!(call1(cos, 0.0), 1.0);
        assert_eq!(call1(tan, 0.0), 0.0);
        assert_eq!(call1(asin, 0.0), 0.0);
        assert_eq!(call1(exp, 0.0), 1.0);
        assert_eq!(call1(log2, 8.0), 3.0);
        assert_eq!(call1(log10, 1000.0), 3.0);
        assert_eq!(call2(log, 8.0, 2.0), 3.0);
        // Irrational / approximate results.
        approx(call1(ln, std::f64::consts::E), 1.0);
        approx(call1(acos, 1.0), 0.0);
        approx(call1(atan, 1.0), std::f64::consts::FRAC_PI_4);
        approx(call2(atan2, 1.0, 1.0), std::f64::consts::FRAC_PI_4);
        approx(call1(sin, 1.0), 0.841_470_984_807_896_5);
    }
}
