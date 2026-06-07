//! `std.os` — native process/environment access (M6c).
//!
//! Reads its data from the engine's injected [`super::HostConfig`] (args/env), never directly from
//! the real process — so runs are deterministic and testable. `getcwd` queries the real working
//! directory (an inherent process property, identical across both engines).
//!
//! `exit(code)` is a cooperative hard exit: it records the code on the host and returns an error
//! sentinel that unwinds past any `recover:` to the top level, where the driver reports it as the
//! process exit status (clamped to `0..=255`). It is *not* catchable.

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

/// `exit(code)` — record a cooperative hard exit and unwind. The returned error is a sentinel: the
/// engine recognizes the pending exit and reports `code` as the process exit status rather than a
/// runtime error. The message is never surfaced (the unwind is intercepted before it prints).
fn exit(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "exit", 1)?;
    let code = h.arg_int(0)?;
    h.request_exit(code);
    Err(HostError { message: "exit".into() })
}

/// Callable members. `(name, fn)`.
pub const MEMBERS: &[(&str, NativeFn)] =
    &[("args", args), ("env", env), ("getcwd", getcwd), ("exit", exit)];
