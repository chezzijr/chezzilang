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
//!
//! `run(line)` / `run_args(prog, args)` are the structured forms: both return `Result[ProcResult]`
//! where `ProcResult{stdout: str, stderr: str, code: int}` carries BOTH streams and the exit code.
//! A non-zero exit is a NORMAL `Ok(ProcResult)` with `code != 0` (stdout is NOT discarded) — only a
//! spawn failure (no such program, permission denied) is `Err`. A signal-killed process has no exit
//! code; it is reported as `code = -1`. `run` still goes through `sh -c` (same shell semantics as
//! `cmd`, same injection caveat); `run_args` runs `prog` directly with `args` as the argv vector —
//! NO shell — so metacharacters in `args` are passed literally and are injection-safe.

use super::{Host, HostError, NativeFn, NativeRet, expect_args};

fn cmd(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "cmd", 1)?;
    let line = h.arg_str(0)?;
    match std::process::Command::new("sh")
        .arg("-c")
        .arg(&line)
        .output()
    {
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

/// Build a `Result[ProcResult]` from a finished command's captured output. A signal-killed process
/// (no exit code) reports `code = -1`. Output is decoded lossily as UTF-8.
fn proc_result_ret(out: std::process::Output) -> NativeRet {
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let code = out.status.code().unwrap_or(-1) as i64;
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
    match std::process::Command::new("sh")
        .arg("-c")
        .arg(line)
        .output()
    {
        Ok(out) => proc_result_ret(out),
        Err(e) => NativeRet::Err(format!("failed to run command: {e}")),
    }
}

/// Run `prog` directly with `args` as the argv vector — NO shell, so `args` are passed literally
/// (injection-safe). Non-zero exit is `Ok` (code carried); only a spawn failure is `Err`.
fn do_run_args(prog: &str, args: &[String]) -> NativeRet {
    match std::process::Command::new(prog).args(args).output() {
        Ok(out) => proc_result_ret(out),
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
pub const MEMBERS: &[(&str, NativeFn)] = &[("cmd", cmd), ("run", run), ("run_args", run_args)];

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
