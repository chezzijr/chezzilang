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

/// `child.wait()` with a deadline: `None` if the child is still running after `secs` (it is killed).
fn wait_timeout(child: &mut Child, secs: u64) -> Option<std::process::ExitStatus> {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        match child.try_wait().unwrap() {
            Some(st) => return Some(st),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// A closed read end (`chezzi run x.chz | head -1`) must TERMINATE, cleanly and promptly: the writer
/// sees `BrokenPipe` and halts the process. Rust installs `SIG_IGN` for SIGPIPE, so an ignored EPIPE
/// would leave this never-ending printer spinning forever on a dead pipe at 100% CPU.
fn broken_pipe_exits_clean(serial: bool) {
    let t = TmpDir::new();
    let entry = t.write("main.chz", "while true:\n    print(\"x\")\n");
    let mut child = spawn(&entry, serial);
    drop(child.stdout.take()); // close the read end immediately
    let status =
        wait_timeout(&mut child, 20).expect("kept printing to a dead pipe (EPIPE ignored)");
    assert!(status.code().is_some(), "killed by a signal: {status:?}");
    assert!(
        status.success(),
        "a closed reader is a clean exit: {status:?}"
    );
}

#[test]
fn broken_pipe_exits_clean_mn() {
    broken_pipe_exits_clean(false);
}

#[test]
fn broken_pipe_exits_clean_serial() {
    broken_pipe_exits_clean(true);
}

/// A stdout that CANNOT be written (`> /dev/full` → ENOSPC) must not be silently dropped: the run
/// fails loudly (diagnostic + non-zero exit), never "exit 0 with no output". Same policy as
/// `chezzi docs` (main.rs `write_stdout`): BrokenPipe = clean, any other errno = FAILURE.
fn write_error_is_reported(serial: bool) {
    let full = std::path::Path::new("/dev/full");
    if !full.exists() {
        return; // not Linux — nothing to assert against
    }
    let t = TmpDir::new();
    let entry = t.write("main.chz", "for i in range(100):\n    print(\"line\", i)\n");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run");
    if serial {
        cmd.arg("--serial");
    }
    let out = cmd
        .arg(&entry)
        .stdout(std::fs::File::create(full).unwrap())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn chezzi");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked at"), "panicked: {err}");
    assert!(
        !out.status.success(),
        "an unwritable stdout reported SUCCESS: {:?} (stderr: {err})",
        out.status
    );
    assert!(
        err.contains("cannot write stdout"),
        "no diagnostic for the failed write: {err}"
    );
}

#[test]
fn write_error_is_reported_mn() {
    write_error_is_reported(false);
}

#[test]
fn write_error_is_reported_serial() {
    write_error_is_reported(true);
}

/// A STALLED reader must not stall the engine. A streamed `print` used to be an inline, blocking
/// `write(2)` on a core worker (holding the process-global stdout lock): once the 64K pipe buffer
/// filled, every printing fiber pinned a worker in the kernel and an unrelated fiber in the same
/// nursery starved for as long as the consumer stalled (the D5 invariant: no blocking syscall on a
/// core worker). Witness: a task that sleeps, then writes a file — it must make progress while
/// nothing is draining stdout.
fn stalled_reader_does_not_starve_other_tasks(serial: bool) {
    let t = TmpDir::new();
    let witness = t.0.join("witness.txt");
    let src = format!(
        "import std.io\nimport std.time\n\n\
         fn spam():\n    for i in range(20000):\n        io.print(\"{pad}\")\n\n\
         fn witness():\n    time.sleep_ms(300)\n    io.write_file(\"{w}\", \"done\")\n\n\
         parallel:\n    for i in range(8):\n        spawn: spam()\n    spawn: witness()\n",
        pad = "x".repeat(64),
        w = witness.display()
    );
    let entry = t.write("main.chz", &src);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run");
    if serial {
        cmd.arg("--serial");
    } else {
        cmd.arg("--threads=2"); // errors with --serial; 8 printers >> workers
    }
    let mut child = cmd
        .arg(&entry)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    // Deliberately do NOT read the child's stdout: the pipe fills and stays full.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut ok = false;
    while std::time::Instant::now() < deadline {
        if witness.exists() {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        ok,
        "a stalled stdout reader starved an unrelated task (blocking write on a core worker)"
    );
}

#[test]
fn stalled_reader_does_not_starve_other_tasks_mn() {
    stalled_reader_does_not_starve_other_tasks(false);
}

#[test]
fn stalled_reader_does_not_starve_other_tasks_serial() {
    stalled_reader_does_not_starve_other_tasks(true);
}
