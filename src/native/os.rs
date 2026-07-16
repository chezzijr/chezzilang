//! `std.os` — native process/environment access (M6c).
//!
//! Reads its data from the engine's injected [`super::HostConfig`] (args/env), never directly from
//! the real process — so runs are deterministic and testable. `getcwd` queries the real working
//! directory (an inherent process property, identical across both engines).
//!
//! `exit(code)` is a cooperative hard exit: it records the code on the host and returns an error
//! sentinel that unwinds past any `recover:` to the top level, where the driver reports it as the
//! process exit status (clamped to `0..=255`). It is *not* catchable.

use super::{Host, HostError, NativeFn, NativeRet, expect_args};

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

/// `getpid()` — the current process id (a real process property, identical across both engines).
fn getpid(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "getpid", 0)?;
    Ok(NativeRet::Int(std::process::id() as i64))
}

/// `platform()` — the compile-time OS name (`"linux"`/`"macos"`/`"windows"`/…), from
/// `std::env::consts::OS`. Engine-agnostic.
fn platform(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "platform", 0)?;
    Ok(NativeRet::Str(std::env::consts::OS.to_string()))
}

/// `hostname()` — the system hostname via `libc::gethostname` (libc is already a dep — no new one).
/// Falls back to `""` on the rare syscall failure rather than promoting the signature to `Result`.
fn hostname(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "hostname", 0)?;
    // ponytail: small unsafe libc::gethostname (same shape as cffi/net's existing libc use); a fixed
    // 256-byte buffer covers HOST_NAME_MAX (64 on Linux), lossy-decoded, "" on nonzero return.
    // `libc::c_char` is i8 on x86_64 but u8 on aarch64/arm — use the platform alias so the buffer
    // element type matches `gethostname`'s `*mut c_char` on every target (not just the x86_64 dev box).
    let mut buf = [0 as libc::c_char; 256];
    let name = unsafe {
        if libc::gethostname(buf.as_mut_ptr(), buf.len() - 1) != 0 {
            String::new()
        } else {
            let cstr = std::ffi::CStr::from_ptr(buf.as_ptr());
            cstr.to_string_lossy().into_owned()
        }
    };
    Ok(NativeRet::Str(name))
}

/// `home_dir()` — the user home from the HostConfig `HOME` (the SAME source `env` reads, not
/// `std::env::var`), so it stays deterministic/testable. `None` when unset. Unix-focused (Windows
/// `USERPROFILE` fallback is a follow-up).
fn home_dir(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "home_dir", 0)?;
    match h.os_env("HOME") {
        Some(v) => Ok(NativeRet::Some(Box::new(NativeRet::Str(v)))),
        None => Ok(NativeRet::None),
    }
}

/// `temp_dir()` — the system temp directory (`std::env::temp_dir()`). Engine-agnostic.
fn temp_dir(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "temp_dir", 0)?;
    Ok(NativeRet::Str(std::env::temp_dir().display().to_string()))
}

/// `environ()` — ALL environment variables from the SAME HostConfig env map `env` reads (shared by
/// `Arc` across M:N workers), sorted by key (see `VmHost::os_environ`). Paired with `setenv` (which
/// writes that map), so a `setenv` is observed here too — one consistent source.
fn environ(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "environ", 0)?;
    let pairs = h
        .os_environ()
        .into_iter()
        .map(|(k, v)| (NativeRet::Str(k), NativeRet::Str(v)))
        .collect();
    Ok(NativeRet::Map(pairs))
}

/// `setenv(key, value)` — set an env var in the HostConfig env map (the source `env`/`environ` read),
/// shared by `Arc` across M:N workers so the write is visible to the parent + sibling tasks. Does NOT
/// touch `std::env::set_var` (a third disagreeing, process-global-racy source).
fn setenv(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "setenv", 2)?;
    let key = h.arg_str(0)?;
    let value = h.arg_str(1)?;
    h.os_setenv(key, value);
    Ok(NativeRet::Nil)
}

/// `chdir(path) -> Result[nil]` — change the REAL process cwd (`getcwd` reads the real cwd, so this
/// mutates the same). `Err` on failure.
///
/// ponytail: process-global cwd — shared by all M:N workers; a task's chdir shifts sibling tasks'
/// relative paths. Python/Go have the same ceiling; per-task virtual cwd would need a whole path-resolution
/// layer, not worth it.
fn chdir(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "chdir", 1)?;
    let p = h.arg_str(0)?;
    match std::env::set_current_dir(&p) {
        Ok(()) => Ok(NativeRet::Ok(Box::new(NativeRet::Nil))),
        Err(e) => Ok(NativeRet::Err(e.to_string())),
    }
}

/// `exit(code)` — record a cooperative hard exit and unwind. The returned error is a sentinel: the
/// engine recognizes the pending exit and reports `code` as the process exit status rather than a
/// runtime error. The message is never surfaced (the unwind is intercepted before it prints).
fn exit(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "exit", 1)?;
    let code = h.arg_int(0)?;
    h.request_exit(code);
    Err(HostError {
        message: "exit".into(),
    })
}

/// Callable members. `(name, fn)`.
pub const MEMBERS: &[(&str, NativeFn)] = &[
    ("args", args),
    ("env", env),
    ("getcwd", getcwd),
    ("exit", exit),
    ("getpid", getpid),
    ("platform", platform),
    ("hostname", hostname),
    ("home_dir", home_dir),
    ("temp_dir", temp_dir),
    ("environ", environ),
    ("setenv", setenv),
    ("chdir", chdir),
];
