//! `std.math` — native math intrinsics (M6c).
//!
//! Most functions take and return `float` (Chezzi has no implicit int→float, so callers pass
//! floats; the checker's `native_module_sig("std.math")` enforces this). The exception is `abs`,
//! which is numeric-polymorphic (gap #12): int args → int, float args → float. (`min`/`max` are
//! NOT here — they live in `std.cmp` as generic `[T: Comparable]` functions, M7-G3.)
//! Pure Rust `std` — no third-party crates.

use super::{expect_args, Host, HostError, NativeFn, NativeRet};

// `abs` is numeric-polymorphic (gap #12): int args yield an int result, float args a float. The
// checker (`infer_numeric_poly`) guarantees the arg is present and numeric, so `arg_is_int(0)`
// decides the whole call.

fn abs(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "abs", 1)?;
    if h.arg_is_int(0) {
        // `i64::MIN.abs()` has no representable result (panics in debug, wraps in release). Surface a
        // recoverable overflow instead — matches the engines' checked `"integer overflow in <Op>"`.
        let v = h
            .arg_int(0)?
            .checked_abs()
            .ok_or(HostError { message: "integer overflow in abs".to_string() })?;
        Ok(NativeRet::Int(v))
    } else {
        Ok(NativeRet::Float(h.arg_float(0)?.abs()))
    }
}

fn floor(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "floor", 1)?;
    Ok(NativeRet::Float(h.arg_float(0)?.floor()))
}

fn ceil(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "ceil", 1)?;
    Ok(NativeRet::Float(h.arg_float(0)?.ceil()))
}

fn round(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "round", 1)?;
    Ok(NativeRet::Float(h.arg_float(0)?.round()))
}

fn pow(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "pow", 2)?;
    Ok(NativeRet::Float(h.arg_float(0)?.powf(h.arg_float(1)?)))
}

fn sqrt(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "sqrt", 1)?;
    let x = h.arg_float(0)?;
    if x < 0.0 {
        return Err(HostError { message: format!("sqrt() of a negative number ({x})") });
    }
    Ok(NativeRet::Float(x.sqrt()))
}

// Trig / exp / log intrinsics (additive, M19-safe). Each is a plain `float -> float` (or
// `(float, float) -> float`) pass-through mirroring `sqrt`'s shape, minus the domain check:
// out-of-domain inputs (e.g. `asin(2.0)`, `ln(-1.0)`) return NaN rather than erroring, keeping
// the signatures plain `float` and the design minimal. Same f64 op on both engines → free parity.

fn sin(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "sin", 1)?;
    Ok(NativeRet::Float(h.arg_float(0)?.sin()))
}

fn cos(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "cos", 1)?;
    Ok(NativeRet::Float(h.arg_float(0)?.cos()))
}

fn tan(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "tan", 1)?;
    Ok(NativeRet::Float(h.arg_float(0)?.tan()))
}

fn asin(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "asin", 1)?;
    Ok(NativeRet::Float(h.arg_float(0)?.asin()))
}

fn acos(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "acos", 1)?;
    Ok(NativeRet::Float(h.arg_float(0)?.acos()))
}

fn atan(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "atan", 1)?;
    Ok(NativeRet::Float(h.arg_float(0)?.atan()))
}

fn atan2(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "atan2", 2)?;
    let y = h.arg_float(0)?;
    let x = h.arg_float(1)?;
    Ok(NativeRet::Float(y.atan2(x)))
}

fn exp(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "exp", 1)?;
    Ok(NativeRet::Float(h.arg_float(0)?.exp()))
}

fn ln(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "ln", 1)?;
    Ok(NativeRet::Float(h.arg_float(0)?.ln()))
}

fn log2(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "log2", 1)?;
    Ok(NativeRet::Float(h.arg_float(0)?.log2()))
}

fn log10(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "log10", 1)?;
    Ok(NativeRet::Float(h.arg_float(0)?.log10()))
}

fn log(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "log", 2)?;
    let value = h.arg_float(0)?;
    let base = h.arg_float(1)?;
    Ok(NativeRet::Float(value.log(base)))
}

/// Callable members. `(name, fn)`.
pub const MEMBERS: &[(&str, NativeFn)] = &[
    ("abs", abs),
    ("floor", floor),
    ("ceil", ceil),
    ("round", round),
    ("pow", pow),
    ("sqrt", sqrt),
    ("sin", sin),
    ("cos", cos),
    ("tan", tan),
    ("asin", asin),
    ("acos", acos),
    ("atan", atan),
    ("atan2", atan2),
    ("exp", exp),
    ("ln", ln),
    ("log2", log2),
    ("log10", log10),
    ("log", log),
];

/// Constant members. `(name, value)`.
pub const CONSTS: &[(&str, f64)] = &[
    ("pi", std::f64::consts::PI),
    ("e", std::f64::consts::E),
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
            Err(HostError { message: "no int args".into() })
        }
        fn arg_float(&mut self, i: usize) -> Result<f64, HostError> {
            self.floats.get(i).copied().ok_or(HostError { message: "missing arg".into() })
        }
        fn arg_is_int(&self, _i: usize) -> bool {
            false
        }
        fn arg_str(&mut self, _i: usize) -> Result<String, HostError> {
            Err(HostError { message: "no str args".into() })
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
        fn os_getcwd(&self) -> Result<String, HostError> {
            Ok("/".into())
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
