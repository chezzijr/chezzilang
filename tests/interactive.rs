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
///
/// The helper KEEPS DRAINING `r` to EOF after handing back the first `want` bytes, and only then
/// drops it. It must: `r` is the child's `ChildStdout`, so dropping it closes the pipe's READ end
/// while the child is still blocked in `read_line` owing us its final line. That child's next
/// `print` then hits EPIPE — and a dead stdout is a deliberate runtime fault (`stdout closed
/// (broken pipe)`, exit 1, see `d965d96`), which raced the VM's post-emit `stream_halt` check and
/// made every caller of this helper flake under a loaded box (~1-in-N, and 5/60 pinned to one core).
/// The test was manufacturing the very broken pipe it then asserted `success()` against.
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
        // Hold the read end open until the child is done writing — see the doc above.
        let mut sink = Vec::new();
        let _ = r.read_to_end(&mut sink);
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

const STDIN_TASKS_PROG: &str = "\
import std.io
fn t():
    match io.read_line():
        Some(v): io.print(\"got {v}\")
        None: io.print(\"eof\")

parallel:
    spawn: t()
    spawn: t()
t()
";

/// Shared stdin, on the REAL process stdin (`Stdin::Real` — the parity tests only cover the injected
/// `Lines` variant): three piped lines, two spawned readers + the entry reader ⇒ every line is read
/// exactly ONCE, by SOME reader, and no reader sees a false EOF. This pins that `std::io::stdin()`'s
/// internal lock really is line-atomic across the M:N engine's real worker threads.
fn task_reads_piped_stdin(serial: bool) {
    let t = TmpDir::new();
    let entry = t.write("main.chz", STDIN_TASKS_PROG);
    let mut child = spawn(&entry, serial);
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"a\nb\nc\n").unwrap();
    drop(stdin); // real EOF after the three lines
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "exit: {:?}", out.status);
    let text = String::from_utf8_lossy(&out.stdout);
    let mut got: Vec<&str> = text.lines().collect();
    got.sort_unstable();
    // ORDER-INSENSITIVE by design: WHICH task gets which line is nondeterministic (Go/Python).
    assert_eq!(
        got,
        vec!["got a", "got b", "got c"],
        "no false EOF, no duplicated line; got:\n{text}"
    );
}

#[test]
fn task_reads_piped_stdin_mn() {
    task_reads_piped_stdin(false);
}

#[test]
fn task_reads_piped_stdin_serial() {
    task_reads_piped_stdin(true);
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

/// A closed read end (`chezzi run x.chz | head -1`) must TERMINATE promptly, and report the run as
/// FAILED: the writer thread marks stdout dead and the VM raises an ordinary runtime fault at its
/// next `print` (Python raises `BrokenPipeError` here for the same reason). Rust installs `SIG_IGN`
/// for SIGPIPE, so an ignored EPIPE would leave this never-ending printer spinning at 100% CPU.
///
/// The status must be NON-ZERO: the program did not finish, and its output was truncated. It must
/// also not borrow the `os.exit` channel to say so — see `fault_under_broken_pipe_is_not_success`,
/// the regression this contract exists to prevent.
fn broken_pipe_terminates_with_fault(serial: bool) {
    let t = TmpDir::new();
    let entry = t.write("main.chz", "while true:\n    print(\"x\")\n");
    let mut child = spawn(&entry, serial);
    drop(child.stdout.take()); // close the read end immediately
    let mut err = String::new();
    let mut stderr = child.stderr.take().unwrap();
    let status =
        wait_timeout(&mut child, 20).expect("kept printing to a dead pipe (EPIPE ignored)");
    use std::io::Read;
    let _ = stderr.read_to_string(&mut err);
    assert!(status.code().is_some(), "killed by a signal: {status:?}");
    assert!(
        !status.success(),
        "a dead stdout truncated the output — the run must not report success: {status:?}"
    );
    assert!(
        err.contains("stdout closed (broken pipe)"),
        "the fault must name the cause; stderr was:\n{err}"
    );
}

#[test]
fn broken_pipe_terminates_with_fault_mn() {
    broken_pipe_terminates_with_fault(false);
}

#[test]
fn broken_pipe_terminates_with_fault_serial() {
    broken_pipe_terminates_with_fault(true);
}

/// REGRESSION (the bug this milestone shipped in its first cut): a dead stdout must NEVER be signalled
/// through `pending_exit` — that is the `std.os.exit` channel, and it OUTRANKS a fault everywhere
/// (`run_file_with_entry` returns `Ok(())` + the code and discards the `Err`; `classify_mn_outcome`
/// ranks `Exit` above `Fault`). With that hijack in place, `chezzi run x.chz | head -1` on a program
/// that then faulted exited **0 with no trace** — a crashing program reporting SUCCESS to CI.
///
/// The `defer:` that prints is load-bearing: it is what runs a `print` DURING the fault unwind, on the
/// now-dead pipe. The first cut's own broken-pipe test could not see the bug because its program
/// printed nothing on the unwind path.
fn fault_under_broken_pipe_is_not_success(serial: bool) {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "import std.time\n\nfn main():\n    defer:\n        print(\"cleanup\")\n    print(\"hello\")\n    time.sleep_ms(150)\n    xs := [1]\n    print(xs[5])\n\nmain()\n",
    );
    let mut child = spawn(&entry, serial);
    drop(child.stdout.take()); // the reader is gone, as under `| head -1`
    let status = wait_timeout(&mut child, 20).expect("hung on a dead pipe");
    assert!(
        !status.success(),
        "a FAULTING run reported success because the dead pipe hijacked the exit channel: {status:?}"
    );
}

#[test]
fn fault_under_broken_pipe_is_not_success_mn() {
    fault_under_broken_pipe_is_not_success(false);
}

#[test]
fn fault_under_broken_pipe_is_not_success_serial() {
    fault_under_broken_pipe_is_not_success(true);
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

// ---------------------------------------------------------------------------------------------
// The writer threads RECORD; they never decide the program's fate. (Regressions for the first cut,
// where a failed write called `std::process::exit` from a detached thread.)
// ---------------------------------------------------------------------------------------------

/// stderr is a DIAGNOSTIC channel: an unwritable stderr (`2> /dev/full`, a dead `2> >(head -1)`
/// reader) must not touch a healthy program. It used to kill the process mid-run — exit 1 for a
/// program that had no error, with its stdout results truncated.
fn stderr_failure_does_not_kill_the_run(serial: bool) {
    let full = std::path::Path::new("/dev/full");
    if !full.exists() {
        return; // not Linux
    }
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "import std.io\nimport std.time\nio.eprint(\"warn one\")\nio.eprint(\"warn two\")\n\
         time.sleep_ms(200)\nfor i in range(50):\n    print(\"line {i}\")\n",
    );
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run");
    if serial {
        cmd.arg("--serial");
    }
    let out = cmd
        .arg(&entry)
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(full).unwrap())
        .output()
        .expect("spawn chezzi");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "an unwritable stderr killed a healthy program: {:?}",
        out.status
    );
    assert_eq!(
        stdout.lines().count(),
        50,
        "stdout was truncated by a stderr failure: {stdout:?}"
    );
}

#[test]
fn stderr_failure_does_not_kill_the_run_mn() {
    stderr_failure_does_not_kill_the_run(false);
}

#[test]
fn stderr_failure_does_not_kill_the_run_serial() {
    stderr_failure_does_not_kill_the_run(true);
}

/// A closed stdout reader must not swallow the program's OUTCOME: a run that faults after the pipe
/// broke still reports FAILURE with its trace on stderr (it used to exit 0, silently, from the writer
/// thread — `main`'s exit-status handling never ran).
fn fault_after_broken_pipe_still_reports_the_fault(serial: bool) {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        // One print (the pipe is already closed → the writer takes EPIPE during the sleep), then a
        // genuine fault, with no further print to halt on: the FAULT must decide the exit status.
        "import std.time\nprint(\"a\")\ntime.sleep_ms(300)\nxs := [1]\nprint(xs[5])\n",
    );
    let mut child = spawn(&entry, serial);
    drop(child.stdout.take()); // close the read end immediately
    let err = child.stderr.take().unwrap();
    let status = wait_timeout(&mut child, 20).expect("never exited");
    let err = read_line_timeout(err).unwrap_or_default();
    assert!(
        !status.success(),
        "a faulting program reported SUCCESS after a broken pipe: {status:?}"
    );
    assert!(
        err.contains("runtime error"),
        "the fault trace was lost: {err:?}"
    );
}

#[test]
fn fault_after_broken_pipe_still_reports_the_fault_mn() {
    fault_after_broken_pipe_still_reports_the_fault(false);
}

#[test]
fn fault_after_broken_pipe_still_reports_the_fault_serial() {
    fault_after_broken_pipe_still_reports_the_fault(true);
}

/// `2>&1 | head -1` — BOTH writer threads take EPIPE in the same window. Two threads calling libc
/// `exit(3)` concurrently is undefined (a hang in the exit-handler lock, atexit run twice); no thread
/// exits the process any more, so this just terminates.
fn both_streams_broken_terminates(serial: bool) {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "import std.io\nwhile true:\n    io.print(\"o\")\n    io.eprint(\"e\")\n",
    );
    let mut child = spawn(&entry, serial);
    drop(child.stdout.take());
    drop(child.stderr.take());
    let status = wait_timeout(&mut child, 20).expect("did not terminate with both pipes closed");
    assert!(status.code().is_some(), "killed by a signal: {status:?}");
}

#[test]
fn both_streams_broken_terminates_mn() {
    both_streams_broken_terminates(false);
}

#[test]
fn both_streams_broken_terminates_serial() {
    both_streams_broken_terminates(true);
}

/// A PARTIAL line (`print(x, end="")`, no newline) must reach the terminal as it is produced — a
/// progress indicator, and the "a killed program keeps what it printed" contract. `std::io::stdout()`
/// is a `LineWriter`, so the writer thread must flush every message; otherwise nothing appears until
/// a newline (or ever, if the program is killed).
fn partial_line_print_is_visible_without_flush(serial: bool) {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "print(\".\", end=\"\")\nwhile true:\n    x := 1\n",
    );
    let mut child = spawn(&entry, serial);
    let out = child.stdout.take().unwrap();
    let got = read_bytes_timeout(out, 1);
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(
        got.as_deref(),
        Some("."),
        "a partial-line print never left the process"
    );
}

#[test]
fn partial_line_print_is_visible_without_flush_mn() {
    partial_line_print_is_visible_without_flush(false);
}

#[test]
fn partial_line_print_is_visible_without_flush_serial() {
    partial_line_print_is_visible_without_flush(true);
}

/// D5, the `io.flush()` seam: a stalled stdout consumer must not starve unrelated tasks. `flush`
/// (and `input`/`read_line`, which used to pre-flush) must never WAIT on the writer thread — that
/// thread is parked in `write(2)` on the full pipe, so waiting pins the fiber's core worker for as
/// long as the consumer stalls. Witness: a task that sleeps, then writes a file.
fn stalled_reader_flush_does_not_starve_other_tasks(serial: bool) {
    let t = TmpDir::new();
    let witness = t.0.join("witness_flush.txt");
    let src = format!(
        "import std.io\nimport std.time\n\n\
         fn spam():\n    for i in range(20000):\n        io.print(\"{pad}\")\n        io.flush()\n\n\
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
        cmd.arg("--threads=2");
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
        "io.flush() waited on a stalled stdout consumer and starved an unrelated task"
    );
}

#[test]
fn stalled_reader_flush_does_not_starve_other_tasks_mn() {
    stalled_reader_flush_does_not_starve_other_tasks(false);
}

#[test]
fn stalled_reader_flush_does_not_starve_other_tasks_serial() {
    stalled_reader_flush_does_not_starve_other_tasks(true);
}
