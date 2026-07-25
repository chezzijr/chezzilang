//! `std.process` — run a shell command and capture its output (M8).
//!
//! `cmd(line)` runs `line` through `sh -c`, so a full command line works (pipes, redirects,
//! arguments). The result mirrors how you read a command at a terminal: exit status 0 yields
//! `Ok(stdout)`, any other status (or a spawn failure) yields `Err(stderr)` — note that on failure
//! stdout is discarded (only stderr is surfaced). This hits the real process table directly (like
//! `std.io.read_file` hits the real filesystem); it does not route through the injected `HostConfig`.
//!
//! W6-4 — the `str`-returning forms NEVER decode lossily: an undecodable stream is a clean `Err`
//! naming the bytes twin (`cmd_bytes` for the shell form, `run_args_bytes` for the argv form), the same
//! answer B1/R1 ratified for `Socket.read`/`io.read_file`. The twins are Go `cmd.Output()`-shaped:
//! `Ok(stdout: bytes)` on a zero exit, `Err` on a non-zero exit or a spawn failure.
//!
//! SECURITY: because the line is handed to the shell (`shell=True`-style, like Python's
//! `subprocess`), interpolating untrusted input into it is a shell-injection vector. Callers must
//! sanitize or avoid building `cmd` strings from untrusted data.
//!
//! `run(line)` / `run_args(prog, args)` are the structured forms: both return `Result[ProcResult]`
//! where `ProcResult{stdout: str, stderr: str, code: int}` carries BOTH streams and the exit code.
//! A non-zero exit is a NORMAL `Ok(ProcResult)` with `code != 0` (stdout is NOT discarded) — only a
//! spawn failure (no such program, permission denied) is `Err`. A signal-killed process has no exit
//! code; it is reported as `code = -1`. `run` still goes through `sh -c` (same shell semantics as
//! `cmd`, same injection caveat); `run_args` runs `prog` directly with `args` as the argv vector —
//! NO shell — so metacharacters in `args` are passed literally and are injection-safe.

use super::{Host, HostError, NativeFn, NativeRet, expect_args};

/// Run `line` through `sh -c` and capture both streams (the shell form of `cmd`/`run`/`cmd_bytes`).
fn spawn_shell(line: &str) -> std::io::Result<std::process::Output> {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(line)
        .output()
}

/// W6-4 — decode a child stream as UTF-8, or a clean `Err` NAMING the bytes twin. A `str`-typed seam
/// must never mangle bytes into U+FFFD: that is the ratified B1/R1 answer (`Socket.read` points at
/// `Socket.read_bytes`, `io.read_file` at `io.read_bytes`), and it is what both owning ancestors do —
/// Python's `subprocess` text mode raises `UnicodeDecodeError`, Go's `cmd.Output()` hands back
/// `[]byte`. `which` is the stream name, `twin` the bytes fn that CAN carry the payload.
fn decode_stream(which: &str, twin: &str, bytes: Vec<u8>) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|e| {
        format!(
            "child {which} is not valid utf-8 ({}) — capture binary output with process.{twin}",
            e.utf8_error()
        )
    })
}

/// The `Err` message for a command that ran but exited non-zero: its stderr, falling back to a status
/// line so the error is never an empty string. This renders a DIAGNOSTIC, not user payload, so it stays
/// `from_utf8_lossy` on purpose (W6-4 covers the payload seams; an error message has nowhere to point).
fn status_err_msg(out: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if !stderr.is_empty() {
        return stderr;
    }
    match out.status.code() {
        Some(c) => format!("command exited with status {c}"),
        None => "command terminated by signal".to_string(),
    }
}

fn cmd(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "cmd", 1)?;
    let line = h.arg_str(0)?;
    match spawn_shell(&line) {
        Ok(out) => {
            if !out.status.success() {
                return Ok(NativeRet::Err(status_err_msg(&out)));
            }
            Ok(match decode_stream("stdout", "cmd_bytes", out.stdout) {
                Ok(stdout) => NativeRet::Ok(Box::new(NativeRet::Str(stdout))),
                Err(msg) => NativeRet::Err(msg),
            })
        }
        Err(e) => Ok(NativeRet::Err(format!("failed to run command: {e}"))),
    }
}

/// W6-4 — the bytes twin's result, in Go `cmd.Output()` shape: `Ok(stdout)` as raw `bytes` on a zero
/// exit, `Err` on any other status (a failed command's output can't pose as a successful capture —
/// same rule as `io.read_bytes` / `request.get_bytes`). stderr + the code are unreachable on this path
/// (a `ProcResult` cannot carry `bytes` — see the gaps.md residual); use `run` when you need them.
fn stdout_bytes_ret(out: std::process::Output) -> NativeRet {
    if out.status.success() {
        NativeRet::Ok(Box::new(NativeRet::Bytes(out.stdout)))
    } else {
        NativeRet::Err(status_err_msg(&out))
    }
}

fn cmd_bytes(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "cmd_bytes", 1)?;
    let line = h.arg_str(0)?;
    Ok(match spawn_shell(&line) {
        Ok(out) => stdout_bytes_ret(out),
        Err(e) => NativeRet::Err(format!("failed to run command: {e}")),
    })
}

fn run_args_bytes(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "run_args_bytes", 2)?;
    let prog = h.arg_str(0)?;
    let args = h.arg_str_list(1)?;
    Ok(
        match std::process::Command::new(&prog).args(&args).output() {
            Ok(out) => stdout_bytes_ret(out),
            Err(e) => NativeRet::Err(format!("failed to run command: {e}")),
        },
    )
}

/// Build a `Result[ProcResult]` from a finished command's captured output. A signal-killed process
/// (no exit code) reports `code = -1`. W6-4 — either stream failing to decode fails the WHOLE call
/// (`ProcResult`'s fields are `str`; there is nowhere to put the bytes), naming `twin` as the hatch.
fn proc_result_ret(out: std::process::Output, twin: &str) -> NativeRet {
    let code = out.status.code().unwrap_or(-1) as i64;
    let (stdout, stderr) = match (
        decode_stream("stdout", twin, out.stdout),
        decode_stream("stderr", twin, out.stderr),
    ) {
        (Ok(o), Ok(e)) => (o, e),
        // The code is the only other thing the caller would have got; carry it in the message.
        (Err(msg), _) | (Ok(_), Err(msg)) => {
            return NativeRet::Err(format!("{msg} (the child exited with code {code})"));
        }
    };
    NativeRet::Ok(Box::new(NativeRet::Struct {
        name: "ProcResult".into(),
        fields: vec![
            ("stdout".into(), NativeRet::Str(stdout)),
            ("stderr".into(), NativeRet::Str(stderr)),
            ("code".into(), NativeRet::Int(code)),
        ],
    }))
}

/// Run `line` through `sh -c` and capture the full structured result. Non-zero exit is `Ok` (code
/// carried); only a spawn failure is `Err`.
fn do_run(line: &str) -> NativeRet {
    match spawn_shell(line) {
        Ok(out) => proc_result_ret(out, "cmd_bytes"),
        Err(e) => NativeRet::Err(format!("failed to run command: {e}")),
    }
}

/// Run `prog` directly with `args` as the argv vector — NO shell, so `args` are passed literally
/// (injection-safe). Non-zero exit is `Ok` (code carried); only a spawn failure is `Err`.
fn do_run_args(prog: &str, args: &[String]) -> NativeRet {
    match std::process::Command::new(prog).args(args).output() {
        Ok(out) => proc_result_ret(out, "run_args_bytes"),
        Err(e) => NativeRet::Err(format!("failed to run command: {e}")),
    }
}

fn run(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "run", 1)?;
    let line = h.arg_str(0)?;
    Ok(do_run(&line))
}

fn run_args(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "run_args", 2)?;
    let prog = h.arg_str(0)?;
    let args = h.arg_str_list(1)?;
    Ok(do_run_args(&prog, &args))
}

/// Callable members. `(name, fn)`.
pub const MEMBERS: &[(&str, NativeFn)] = &[
    ("cmd", cmd),
    ("run", run),
    ("run_args", run_args),
    ("cmd_bytes", cmd_bytes),
    ("run_args_bytes", run_args_bytes),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Pull a field out of a `NativeRet::Struct`, asserting the wrapper/struct shape. Returns the
    /// field's `NativeRet`. Panics with a clear message if the shape is wrong.
    fn proc_field<'a>(ret: &'a NativeRet, field: &str) -> &'a NativeRet {
        let NativeRet::Ok(inner) = ret else {
            panic!("expected Ok(ProcResult), got {ret:?}");
        };
        let NativeRet::Struct { name, fields } = inner.as_ref() else {
            panic!("expected Struct ProcResult, got {inner:?}");
        };
        assert_eq!(name, "ProcResult");
        fields
            .iter()
            .find(|(k, _)| k == field)
            .map(|(_, v)| v)
            .unwrap_or_else(|| panic!("ProcResult has no field {field}"))
    }

    fn as_str(ret: &NativeRet) -> &str {
        match ret {
            NativeRet::Str(s) => s,
            other => panic!("expected Str, got {other:?}"),
        }
    }

    fn as_int(ret: &NativeRet) -> i64 {
        match ret {
            NativeRet::Int(n) => *n,
            other => panic!("expected Int, got {other:?}"),
        }
    }

    #[test]
    fn do_run_captures_both_streams_and_code() {
        // stdout + zero exit.
        let ok = do_run("echo hi");
        assert!(as_str(proc_field(&ok, "stdout")).contains("hi"));
        assert_eq!(as_str(proc_field(&ok, "stderr")), "");
        assert_eq!(as_int(proc_field(&ok, "code")), 0);

        // Non-zero exit is Ok WITH the code + stderr, stdout NOT discarded.
        let nz = do_run("echo out; echo err 1>&2; exit 3");
        assert!(as_str(proc_field(&nz, "stdout")).contains("out"));
        assert!(as_str(proc_field(&nz, "stderr")).contains("err"));
        assert_eq!(as_int(proc_field(&nz, "code")), 3);
    }

    #[test]
    fn do_run_args_no_shell_interpretation() {
        // Injection-safety proof: shell metacharacters are passed LITERALLY, never evaluated.
        let args: Vec<String> = vec!["$(echo PWNED)".into(), ";".into(), "&&".into()];
        let ret = do_run_args("echo", &args);
        let stdout = as_str(proc_field(&ret, "stdout"));
        assert!(stdout.contains("$(echo PWNED)"), "got: {stdout:?}");
        assert!(stdout.contains(';'), "got: {stdout:?}");
        assert!(!stdout.contains("PWNED\n") || stdout.contains("$(echo PWNED)"));
        // The literal substring must NOT have been substituted to its command output.
        assert!(
            !stdout.replace("$(echo PWNED)", "").contains("PWNED"),
            "shell substitution leaked: {stdout:?}"
        );
        assert_eq!(as_int(proc_field(&ret, "code")), 0);
    }

    #[test]
    fn do_run_args_exit_code_nonzero_is_ok() {
        // `false` exits non-zero — a normal Ok result carrying the code, not an Err.
        let ret = do_run_args("sh", &["-c".into(), "exit 7".into()]);
        assert_eq!(as_int(proc_field(&ret, "code")), 7);
    }

    #[test]
    fn do_run_args_spawn_failure_is_err() {
        let ret = do_run_args("definitely-no-such-prog-xyz", &[]);
        assert!(matches!(ret, NativeRet::Err(_)), "got: {ret:?}");
    }
}
