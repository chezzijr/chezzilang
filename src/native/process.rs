//! `std.process` — run a shell command and capture its output (M8).
//!
//! `cmd(line)` runs `line` through `sh -c`, so a full command line works (pipes, redirects,
//! arguments). The result mirrors how you read a command at a terminal: exit status 0 yields
//! `Ok(stdout)`, any other status (or a spawn failure) yields `Err(stderr)` — note that on failure
//! stdout is discarded (only stderr is surfaced). Output is decoded lossily as UTF-8. This hits the
//! real process table directly (like `std.io.read_file` hits the real filesystem); it does not
//! route through the injected `HostConfig`.
//!
//! SECURITY: because the line is handed to the shell (`shell=True`-style, like Python's
//! `subprocess`), interpolating untrusted input into it is a shell-injection vector. Callers must
//! sanitize or avoid building `cmd` strings from untrusted data.

use super::{expect_args, Host, HostError, NativeFn, NativeRet};

fn cmd(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "cmd", 1)?;
    let line = h.arg_str(0)?;
    match std::process::Command::new("sh").arg("-c").arg(&line).output() {
        Ok(out) => {
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
                Ok(NativeRet::Ok(Box::new(NativeRet::Str(stdout))))
            } else {
                // Non-zero exit: report stderr. Fall back to a status line if it wrote nothing
                // there, so the error is never an empty string.
                let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
                let msg = if stderr.is_empty() {
                    match out.status.code() {
                        Some(c) => format!("command exited with status {c}"),
                        None => "command terminated by signal".to_string(),
                    }
                } else {
                    stderr
                };
                Ok(NativeRet::Err(msg))
            }
        }
        Err(e) => Ok(NativeRet::Err(format!("failed to run command: {e}"))),
    }
}

/// Callable members. `(name, fn)`.
pub const MEMBERS: &[(&str, NativeFn)] = &[("cmd", cmd)];
