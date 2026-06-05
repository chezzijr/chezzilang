//! `std.os` — native process/environment access (M6c).
//!
//! Reads its data from the engine's injected [`super::HostConfig`] (args/env), never directly from
//! the real process — so runs are deterministic and testable. `getcwd` queries the real working
//! directory (an inherent process property, identical across both engines).
//!
//! `exit(code)` is intentionally **not** here yet: a correct cooperative exit needs an exit-code
//! channel threaded through both run drivers and the CLI; that is a focused follow-up.

use super::{expect_args, Host, HostError, NativeFn, NativeRet};

fn args(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "args", 0)?;
    let items = h.os_args().into_iter().map(NativeRet::Str).collect();
    Ok(NativeRet::List(items))
}

fn env(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "env", 1)?;
    let key = h.arg_str(0)?;
    match h.os_env(&key) {
        Some(v) => Ok(NativeRet::Some(Box::new(NativeRet::Str(v)))),
        None => Ok(NativeRet::None),
    }
}

fn getcwd(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "getcwd", 0)?;
    match h.os_getcwd() {
        Ok(p) => Ok(NativeRet::Ok(Box::new(NativeRet::Str(p)))),
        Err(e) => Ok(NativeRet::Err(e.message)),
    }
}

/// Callable members. `(name, fn)`.
pub const MEMBERS: &[(&str, NativeFn)] = &[("args", args), ("env", env), ("getcwd", getcwd)];
