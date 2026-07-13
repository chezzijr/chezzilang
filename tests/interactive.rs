//! Real-PROCESS interactive-CLI tests: `chezzi run` STREAMS stdout as the program produces it.
//!
//! These cannot be expressed as in-VM assertions: the property under test is *when* the bytes leave
//! the process (a prompt must be readable while the child is still blocked on an unanswered stdin;
//! a killed program must retain what it already printed). The lib test helpers keep the BUFFERED
//! sink (the serial-vs-M:N parity oracle) — only the CLI streams.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique temp directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("chezzi_interactive_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::write(&p, contents).unwrap();
        p
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Spawn `chezzi run [--serial] <file>` with piped stdin/stdout/stderr. Flags go BEFORE the file.
fn spawn(entry: &PathBuf, serial: bool) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run");
    if serial {
        cmd.arg("--serial");
    }
    cmd.arg(entry)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi")
}

/// Read from `r` on a helper thread until `want` bytes have arrived (or 5s elapse). Never
/// `wait_with_output()`: the child still holds an open, unanswered stdin, so waiting deadlocks.
fn read_bytes_timeout<R: Read + Send + 'static>(mut r: R, want: usize) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut acc = Vec::new();
        let mut b = [0u8; 1];
        while acc.len() < want {
            match r.read(&mut b) {
                Ok(0) | Err(_) => break,
                Ok(_) => acc.push(b[0]),
            }
        }
        let _ = tx.send(String::from_utf8_lossy(&acc).into_owned());
    });
    rx.recv_timeout(Duration::from_secs(5)).ok()
}

/// Read ONE line (bounded) from a reader on a helper thread.
fn read_line_timeout<R: Read + Send + 'static>(r: R) -> Option<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut rd = BufReader::new(r);
        let mut line = String::new();
        if rd.read_line(&mut line).is_ok() {
            let _ = tx.send(line);
        }
    });
    rx.recv_timeout(Duration::from_secs(5)).ok()
}

const PROMPT_PROG: &str = "\
import std.io
print(\"name? \", end=\"\")
io.flush()
n := io.read_line()
match n:
    Some(v): print(\"hi\", v)
    None: print(\"hi ?\")
";

fn prompt_before_stdin_answer(serial: bool) {
    let t = TmpDir::new();
    let entry = t.write("main.chz", PROMPT_PROG);
    let mut child = spawn(&entry, serial);
    let out = child.stdout.take().unwrap();
    // The child has NOT been given any stdin yet — it is blocked in read_line. The prompt must
    // already be readable. (Today: nothing is written until the VM returns → times out.)
    let got = read_bytes_timeout(out, 6).expect("prompt did not arrive before stdin was answered");
    assert_eq!(got, "name? ");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"ada\n").unwrap();
    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "exit: {status:?}");
}

#[test]
fn prompt_before_stdin_answer_mn() {
    prompt_before_stdin_answer(false);
}

#[test]
fn prompt_before_stdin_answer_serial() {
    prompt_before_stdin_answer(true);
}

fn killed_program_retains_output(serial: bool) {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "print(\"alive\")\nx := 0\nwhile true:\n    x = x + 1\n",
    );
    let mut child = spawn(&entry, serial);
    let out = child.stdout.take().unwrap();
    let line = read_line_timeout(out).expect("no output from a program that never exits");
    assert_eq!(line.trim_end(), "alive");
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn killed_program_retains_output_mn() {
    killed_program_retains_output(false);
}

#[test]
fn killed_program_retains_output_serial() {
    killed_program_retains_output(true);
}

const SPAWN_PROG: &str = "\
fn worker():
    print(\"task-live\")
    x := 0
    while true:
        x = x + 1

parallel:
    spawn: worker()
";

fn spawned_task_print_visible_before_join(serial: bool) {
    let t = TmpDir::new();
    let entry = t.write("main.chz", SPAWN_PROG);
    let mut child = spawn(&entry, serial);
    let out = child.stdout.take().unwrap();
    let line = read_line_timeout(out).expect("spawned task's print never arrived before the join");
    assert_eq!(line.trim_end(), "task-live");
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn spawned_task_print_visible_before_join_mn() {
    spawned_task_print_visible_before_join(false);
}

#[test]
fn spawned_task_print_visible_before_join_serial() {
    spawned_task_print_visible_before_join(true);
}

const CONCURRENT_PROG: &str = "\
fn task(i: int):
    print(\"t{i}\")

parallel:
    for i in range(8):
        spawn: task(i)
";

fn concurrent_prints_interleave_all_lines(serial: bool) {
    let t = TmpDir::new();
    let entry = t.write("main.chz", CONCURRENT_PROG);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run");
    if serial {
        cmd.arg("--serial");
    }
    let out = cmd.arg(&entry).output().expect("spawn chezzi");
    assert!(out.status.success());
    let mut got: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect();
    let mut want: Vec<String> = (0..8).map(|i| format!("t{i}")).collect();
    // ORDER-INSENSITIVE by design: cross-task interleaving is nondeterministic (the whole point).
    got.sort();
    want.sort();
    assert_eq!(got, want);
}

#[test]
fn concurrent_prints_interleave_all_lines_mn() {
    concurrent_prints_interleave_all_lines(false);
}

#[test]
fn concurrent_prints_interleave_all_lines_serial() {
    concurrent_prints_interleave_all_lines(true);
}

const INPUT_PROG: &str = "\
import input from std.io
n := input(\"name? \")
match n:
    Some(v): print(\"hi\", v)
    None: print(\"hi ?\")
";

fn input_prompt_roundtrip(serial: bool) {
    let t = TmpDir::new();
    let entry = t.write("main.chz", INPUT_PROG);
    let mut child = spawn(&entry, serial);
    let out = child.stdout.take().unwrap();
    let got = read_bytes_timeout(out, 6).expect("input()'s prompt did not arrive before the read");
    assert_eq!(got, "name? ");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"ada\n").unwrap();
    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "exit: {status:?}");
}

#[test]
fn input_prompt_roundtrip_mn() {
    input_prompt_roundtrip(false);
}

#[test]
fn input_prompt_roundtrip_serial() {
    input_prompt_roundtrip(true);
}

/// A closed read end (`chezzi run x.chz | head -1`) must exit cleanly, never panic.
#[test]
fn broken_pipe_no_panic() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "for i in range(200000):\n    print(\"line\", i)\n",
    );
    let mut child = spawn(&entry, false);
    drop(child.stdout.take()); // close the read end immediately
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    let status = child.wait().unwrap();
    assert!(!err.contains("panicked at"), "panicked: {err}");
    assert!(status.code().is_some(), "killed by a signal: {status:?}");
}
