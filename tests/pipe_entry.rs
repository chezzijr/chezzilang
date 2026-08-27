//! `chezzi run`/`check` on a one-shot fd (a pipe, `/dev/stdin`) — real-PROCESS tests.
//!
//! The entry source is read TWICE: once by the CLI to validate readability
//! (`main.rs::read_source`), and again by `resolver::build_graph` on the way into
//! `type_check`/the VM. A pipe/`/dev/stdin` can only be read once, so the second read sees EOF and
//! the program that actually runs is the EMPTY program — which succeeds. `chezzi tokens` reads the
//! fd only once and is unaffected, which is what rules out the read itself as broken.

use std::io::Write;
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(unix)]
use std::sync::mpsc;
#[cfg(unix)]
use std::time::Duration;

/// Pipe `source` into `chezzi <subcmd> /dev/stdin` and return (stdout, stderr, exit code).
fn piped(subcmd: &str, source: &str) -> (String, String, i32) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg(subcmd)
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

/// Pipe `source` into `chezzi run /dev/stdin` and return (stdout, stderr, exit code).
fn run_piped(source: &str) -> (String, String, i32) {
    piped("run", source)
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

#[test]
fn check_on_a_pipe_reports_the_type_error() {
    let (stdout, stderr, code) = piped("check", "x: int = \"bad\"\n");
    assert_eq!(
        code, 1,
        "expected a type error on a piped bad program, got stdout={stdout:?} stderr={stderr:?} code={code}"
    );
    assert!(stderr.contains("chezzi: 1 type error"), "stderr={stderr:?}");
}

#[test]
fn run_on_an_empty_pipe_is_an_error() {
    let (stdout, stderr, code) = run_piped("");
    assert_eq!(
        code, 1,
        "expected an empty entry to be an error, got stdout={stdout:?} stderr={stderr:?} code={code}"
    );
    assert!(stderr.contains("is empty"), "stderr={stderr:?}");
}

#[test]
fn check_on_an_empty_pipe_is_an_error() {
    let (stdout, stderr, code) = piped("check", "");
    assert_eq!(
        code, 1,
        "expected an empty entry to be an error, got stdout={stdout:?} stderr={stderr:?} code={code}"
    );
    assert!(stderr.contains("is empty"), "stderr={stderr:?}");
}

#[cfg(unix)]
static FIFO_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique temp directory, removed on drop — mirrors `tests/exit_status.rs::TmpDir`.
#[cfg(unix)]
struct TmpDir(std::path::PathBuf);
#[cfg(unix)]
impl TmpDir {
    fn new() -> Self {
        let n = FIFO_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("chezzi_pipe_entry_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
    fn path(&self, rel: &str) -> std::path::PathBuf {
        self.0.join(rel)
    }
}
#[cfg(unix)]
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `chezzi run` on a named FIFO must execute the piped program and terminate — a named FIFO hangs
/// today because the entry is read twice (once to check readability, once by the resolver), and the
/// second open of a FIFO with no live writer blocks forever.
///
/// Order is load-bearing (see TICKET-001 `## Decisions`): spawn the child FIRST, then open the writer.
/// The writer open only returns once the child has opened the FIFO for reading, which is the sync
/// point. Opening the writer BEFORE spawning the child hangs the child in its own open forever, and a
/// held `O_RDONLY|O_NONBLOCK` reader does not help (Linux `fifo_open` waits for a NEW writer).
#[cfg(unix)]
#[test]
fn run_on_a_named_fifo_executes_the_program() {
    let t = TmpDir::new();
    let fifo = t.path("prog.chz");
    let status = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("spawn mkfifo");
    assert!(status.success(), "mkfifo failed: {status:?}");

    let (tx, rx) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if rx.recv_timeout(Duration::from_secs(30)).is_err() {
            eprintln!("run_on_a_named_fifo_executes_the_program: timed out");
            std::process::abort();
        }
    });

    let child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&fifo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");

    let mut writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&fifo)
        .expect("open fifo for writing");
    writer
        .write_all(b"print(\"HELLO\")\n")
        .expect("write program to fifo");
    drop(writer);

    let out = child.wait_with_output().expect("wait chezzi");
    let _ = tx.send(());
    watchdog.join().expect("watchdog thread panicked");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let code = out.status.code().expect("exited with a status (no signal)");
    assert_eq!(
        stdout, "HELLO\n",
        "expected the FIFO-piped program's own output, got stdout={stdout:?} stderr={stderr:?} code={code}"
    );
    assert_eq!(code, 0);
}
