//! CLI defects found by the 2026-09-07 external dogfood pass (TICKET-091).

use std::io::Write;
use std::process::{Command, Stdio};

fn run(args: &[&str]) -> (String, String, i32) {
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(args)
        .output()
        .expect("spawn chezzi");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().expect("exited with a status (no signal)"),
    )
}

/// (a) neither `--version` nor `version` exists yet — both fall through to `unknown command`.
#[test]
fn version_flag_prints_a_version_and_exits_0() {
    let (stdout, stderr, code) = run(&["--version"]);
    assert_eq!(
        code, 0,
        "expected --version to succeed, got stdout={stdout:?} stderr={stderr:?} code={code}"
    );
    assert!(!stderr.contains("unknown command"), "stderr={stderr:?}");
}

/// `version`, `--version` and `-V` all print the crate version and exit 0.
#[test]
fn version_command_and_flags_print_the_crate_version() {
    let expected = format!("chezzi {}\n", env!("CARGO_PKG_VERSION"));
    for flag in ["version", "--version", "-V"] {
        let (stdout, stderr, code) = run(&[flag]);
        assert_eq!(
            code, 0,
            "chezzi {flag}: got stdout={stdout:?} stderr={stderr:?} code={code}"
        );
        assert_eq!(stdout, expected, "chezzi {flag}: got stdout={stdout:?}");
        assert_eq!(stderr, "", "chezzi {flag}: got stderr={stderr:?}");
    }
}

/// Run `chezzi <cmd> /dev/stdin`, feed it a program that prints far more than a pipe buffer holds,
/// read only a little of stdout then drop it (what `| head` does), and return the child's stderr
/// and exit code.
fn closed_pipe(cmd: &str) -> (String, Option<i32>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg(cmd)
        .arg("/dev/stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");

    let mut prog = String::from("fn main():\n");
    for i in 0..2000 {
        prog.push_str(&format!("    print({i})\n"));
    }
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(prog.as_bytes());
    drop(stdin);

    let mut stdout = child.stdout.take().unwrap();
    let mut buf = [0u8; 64];
    let _ = std::io::Read::read(&mut stdout, &mut buf);
    drop(stdout);

    let out = child.wait_with_output().expect("wait chezzi");
    (
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code(),
    )
}

/// `tokens` on a closed pipe reports `run`'s own broken-pipe wording, not a raw Rust panic.
#[test]
fn tokens_on_a_closed_pipe_reports_the_run_wording() {
    let (stderr, code) = closed_pipe("tokens");
    assert_eq!(
        stderr.trim_end(),
        "chezzi tokens: stdout closed (broken pipe)",
        "got stderr={stderr:?}"
    );
    assert!(!stderr.contains("panicked at"), "got stderr={stderr:?}");
    assert_eq!(code, Some(1), "got stderr={stderr:?}");
}

/// `ast` on a closed pipe reports `run`'s own broken-pipe wording, not a raw Rust panic.
#[test]
fn ast_on_a_closed_pipe_reports_the_run_wording() {
    let (stderr, code) = closed_pipe("ast");
    assert_eq!(
        stderr.trim_end(),
        "chezzi ast: stdout closed (broken pipe)",
        "got stderr={stderr:?}"
    );
    assert!(!stderr.contains("panicked at"), "got stderr={stderr:?}");
    assert_eq!(code, Some(1), "got stderr={stderr:?}");
}

/// (b) `chezzi ast` piped into a closed reader (`head -1`) panics with a raw Rust broken-pipe
/// message instead of `run`'s own wording.
#[test]
fn ast_on_a_closed_pipe_does_not_panic() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("ast")
        .arg("/dev/stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");

    // Enough AST output that the pipe fills before the process can drain it.
    let mut prog = String::from("fn main():\n");
    for i in 0..2000 {
        prog.push_str(&format!("    print({i})\n"));
    }
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(prog.as_bytes());
    drop(stdin);

    // Only read a little of stdout, then drop it — this is what `| head -3` does to the pipe.
    let mut stdout = child.stdout.take().unwrap();
    let mut buf = [0u8; 64];
    let _ = std::io::Read::read(&mut stdout, &mut buf);
    drop(stdout);

    let out = child.wait_with_output().expect("wait chezzi");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !stderr.contains("panicked at library/std/src/io/stdio.rs"),
        "expected no raw Rust panic on a closed pipe, got stderr={stderr:?}"
    );
}

/// (c) `chezzi help` never documents the `--` terminator, and its closing NOTE only describes the
/// file-argument form — the only way to pass args to a manifest-`entrypoint` run.
#[test]
fn help_note_documents_the_double_dash_terminator() {
    let (stdout, _stderr, _code) = run(&["help"]);
    let note = stdout
        .split("NOTE:")
        .nth(1)
        .expect("expected a NOTE: block in chezzi help output");
    assert!(
        note.contains("--") && note.to_lowercase().contains("entrypoint"),
        "expected the NOTE block to document `--` as the way to pass args to a manifest entrypoint \
         (it currently only describes the file-argument form), got NOTE block:\n{note}"
    );
}
