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

/// `chezzi check` on a named FIFO must render its DIAGNOSTIC — caret and all — and terminate.
///
/// `run_on_a_named_fifo_executes_the_program` above only covers a program that SUCCEEDS, so it
/// never reaches the diagnostic renderer. That is the gap this test closes: `render_diag`'s source
/// cache is pre-seeded with the entry bytes precisely so the caret echo never re-opens a one-shot
/// fd, but the seed is only reachable if it is keyed the way the LOOKUP keys it —
/// `resolver::canonical_or_abs`, the module id `path_for` reports. Keyed on the raw CLI path
/// instead, this hangs: `canonicalize` SUCCEEDS for a named FIFO (unlike `/dev/stdin`, where it
/// fails and the two spellings coincidentally agree), the keys diverge, the cache misses, and
/// `read_to_string` opens the FIFO a second time with no live writer.
///
/// Same load-bearing order as above: spawn the child FIRST, then open the writer.
#[cfg(unix)]
#[test]
fn check_on_a_named_fifo_renders_the_caret() {
    let t = TmpDir::new();
    let fifo = t.path("bad.chz");
    let status = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(status.success(), "mkfifo failed");

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        if rx.recv_timeout(std::time::Duration::from_secs(30)).is_err() {
            eprintln!("check_on_a_named_fifo_renders_the_caret: timed out");
            std::process::abort();
        }
    });

    // RELATIVE path, run from the temp dir. That is the whole point: for an ABSOLUTE, already
    // canonical path the raw spelling and `canonical_or_abs` agree, the buggy key works by
    // accident, and this test would pass against the defect it exists to catch.
    let child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .current_dir(&t.0)
        .arg("check")
        .arg("bad.chz")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");

    let mut writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&fifo)
        .expect("open fifo for writing");
    writer
        .write_all(b"xs := [1, 2, 3]\nprint(xs.lenght())\n")
        .expect("write program to fifo");
    drop(writer);

    let out = child.wait_with_output().expect("wait chezzi");
    let _ = tx.send(());
    watchdog.join().expect("watchdog thread panicked");

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let all = format!("{stdout}{stderr}");
    let code = out.status.code().expect("exited with a status (no signal)");
    assert!(
        all.contains("has no method 'lenght'"),
        "expected the type error, got stdout={stdout:?} stderr={stderr:?} code={code}"
    );
    assert!(
        all.contains("print(xs.lenght())"),
        "expected the caret snippet to echo the source line from the SEEDED entry bytes (a cache \
         miss re-reads the FIFO and drops it), got stdout={stdout:?} stderr={stderr:?}"
    );
    assert_eq!(code, 1, "a type error must exit 1");
}
