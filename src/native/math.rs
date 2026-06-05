//! `std.math` — native float intrinsics (M6c).
//!
//! Every function takes and returns `float` (Chezzi has no implicit int→float, so callers pass
//! floats; the checker's `native_module_sig("std.math")` enforces this). Pure Rust `std` — no
//! third-party crates.

use super::{expect_args, Host, HostError, NativeFn, NativeRet};

fn abs(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "abs", 1)?;
    Ok(NativeRet::Float(h.arg_float(0)?.abs()))
}

fn min(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "min", 2)?;
    Ok(NativeRet::Float(h.arg_float(0)?.min(h.arg_float(1)?)))
}

fn max(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "max", 2)?;
    Ok(NativeRet::Float(h.arg_float(0)?.max(h.arg_float(1)?)))
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
    ("min", min),
    ("max", max),
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
