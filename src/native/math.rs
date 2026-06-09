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

/// Callable members. `(name, fn)`.
pub const MEMBERS: &[(&str, NativeFn)] = &[
    ("abs", abs),
    ("floor", floor),
    ("ceil", ceil),
    ("round", round),
    ("pow", pow),
    ("sqrt", sqrt),
];

/// Constant members. `(name, value)`.
pub const CONSTS: &[(&str, f64)] = &[
    ("pi", std::f64::consts::PI),
    ("e", std::f64::consts::E),
];
