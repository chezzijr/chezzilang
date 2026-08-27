//! `chezzi run`/`check` on a one-shot fd (a pipe, `/dev/stdin`) — real-PROCESS tests.
//!
//! The entry source is read TWICE: once by the CLI to validate readability
//! (`main.rs::read_source`), and again by `resolver::build_graph` on the way into
//! `type_check`/the VM. A pipe/`/dev/stdin` can only be read once, so the second read sees EOF and
//! the program that actually runs is the EMPTY program — which succeeds. `chezzi tokens` reads the
//! fd only once and is unaffected, which is what rules out the read itself as broken.

use std::io::Write;
use std::process::{Command, Stdio};

/// Pipe `source` into `chezzi run /dev/stdin` and return (stdout, stderr, exit code).
fn run_piped(source: &str) -> (String, String, i32) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg("/dev/stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    cmd.stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    let out = cmd.wait_with_output().expect("wait chezzi");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().expect("exited with a status (no signal)"),
    )
}

#[test]
fn run_on_a_pipe_executes_the_piped_program() {
    let (stdout, stderr, code) = run_piped("print(\"HELLO\")\n");
    assert_eq!(
        stdout, "HELLO\n",
        "expected the piped program's own output, got stdout={stdout:?} stderr={stderr:?} code={code}"
    );
    assert_eq!(code, 0);
}
