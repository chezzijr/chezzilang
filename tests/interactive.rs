//! Real-PROCESS interactive-CLI tests: `chezzi run` STREAMS stdout as the program produces it.
//!
//! These cannot be expressed as in-VM assertions: the property under test is *when* the bytes leave
//! the process (a prompt must be readable while the child is still blocked on an unanswered stdin;
//! a killed program must retain what it already printed). The lib test helpers keep the BUFFERED
//! sink (every in-process test helper) — only the CLI streams.

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

/// Spawn `chezzi run <file>` with piped stdin/stdout/stderr. Flags go BEFORE the file.
fn spawn(entry: &PathBuf) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run");
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

fn prompt_before_stdin_answer() {
    let t = TmpDir::new();
    let entry = t.write("main.chz", PROMPT_PROG);
    let mut child = spawn(&entry);
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
    prompt_before_stdin_answer();
}

fn killed_program_retains_output() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "print(\"alive\")\nx := 0\nwhile true:\n    x = x + 1\n",
    );
    let mut child = spawn(&entry);
    let out = child.stdout.take().unwrap();
    let line = read_line_timeout(out).expect("no output from a program that never exits");
    assert_eq!(line.trim_end(), "alive");
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn killed_program_retains_output_mn() {
    killed_program_retains_output();
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

fn spawned_task_print_visible_before_join() {
    let t = TmpDir::new();
    let entry = t.write("main.chz", SPAWN_PROG);
    let mut child = spawn(&entry);
    let out = child.stdout.take().unwrap();
    let line = read_line_timeout(out).expect("spawned task's print never arrived before the join");
    assert_eq!(line.trim_end(), "task-live");
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn spawned_task_print_visible_before_join_mn() {
    spawned_task_print_visible_before_join();
}

const CONCURRENT_PROG: &str = "\
fn task(i: int):
    print(\"t{i}\")

parallel:
    for i in range(8):
        spawn: task(i)
";

fn concurrent_prints_interleave_all_lines() {
    let t = TmpDir::new();
    let entry = t.write("main.chz", CONCURRENT_PROG);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run");
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
    concurrent_prints_interleave_all_lines();
}

const INPUT_PROG: &str = "\
import input from std.io
n := input(\"name? \")
match n:
    Some(v): print(\"hi\", v)
    None: print(\"hi ?\")
";

fn input_prompt_roundtrip() {
    let t = TmpDir::new();
    let entry = t.write("main.chz", INPUT_PROG);
    let mut child = spawn(&entry);
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
    input_prompt_roundtrip();
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

/// Shared stdin, on the REAL process stdin (`Stdin::Real` — the golden tests only cover the injected
/// `Lines` variant): three piped lines, two spawned readers + the entry reader ⇒ every line is read
/// exactly ONCE, by SOME reader, and no reader sees a false EOF. This pins that `std::io::stdin()`'s
/// internal lock really is line-atomic across the M:N engine's real worker threads.
fn task_reads_piped_stdin() {
    let t = TmpDir::new();
    let entry = t.write("main.chz", STDIN_TASKS_PROG);
    let mut child = spawn(&entry);
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
    task_reads_piped_stdin();
}

const READ_ALL_PROG: &str = "\
import std.io
io.print(io.read_all())
";

/// `io.read_all()` on the REAL process stdin (`Stdin::Real`, not the injected `Lines` model): pipe a
/// multi-line, multibyte-UTF-8 payload with NO trailing newline and assert the WHOLE stream comes
/// back byte-exact (the injected-`Lines` golden test cannot observe this — it reconstructs a trailing
/// `\n` the real stream lacks). `print` adds exactly one `\n`.
fn read_all_reads_whole_stdin() {
    let t = TmpDir::new();
    let entry = t.write("main.chz", READ_ALL_PROG);
    let mut child = spawn(&entry);
    let mut stdin = child.stdin.take().unwrap();
    // "héllo\nwörld" (é, ö multibyte), no trailing newline.
    stdin.write_all(b"h\xc3\xa9llo\nw\xc3\xb6rld").unwrap();
    drop(stdin); // EOF
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "exit: {:?}", out.status);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "héllo\nwörld\n");
}

#[test]
fn read_all_reads_whole_stdin_mn() {
    read_all_reads_whole_stdin();
}

const READ_CHAR_PROG: &str = "\
import std.io
while true:
    match io.read_char():
        Some(c): io.print(\"[{c}]\")
        None:
            io.print(\"done\")
            break
";

/// `io.read_char()` on the REAL process stdin: pipe a multibyte payload and assert each Unicode
/// scalar comes back WHOLE (the 2-byte `é` is one char, never split into bytes), then `None` at EOF.
fn read_char_yields_whole_scalars() {
    let t = TmpDir::new();
    let entry = t.write("main.chz", READ_CHAR_PROG);
    let mut child = spawn(&entry);
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"a\xc3\xa9").unwrap(); // "aé", no trailing newline
    drop(stdin); // EOF
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "exit: {:?}", out.status);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[a]\n[é]\ndone\n");
}

#[test]
fn read_char_yields_whole_scalars_mn() {
    read_char_yields_whole_scalars();
}

const READ_CHAR_CONCURRENT_PROG: &str = "\
import std.io
fn drain():
    while true:
        match io.read_char():
            Some(c): io.print(c)
            None: break

parallel:
    spawn: drain()
    spawn: drain()
drain()
";

/// CONCURRENT `io.read_char()` on the REAL process stdin: three tasks each loop `read_char` over one
/// shared stdin whose bytes are ALL multibyte scalars (`é` = 0xC3 0xA9). A read of one scalar must be
/// atomic — the lead byte and its continuation go to ONE reader, never split across two. If `read_char`
/// released the stdin lock between the lead and continuation byte, a second reader would grab the
/// continuation (0xA9, a bare continuation byte) → a spurious `stdin: stream is not valid UTF-8` fault
/// and/or a torn scalar. Assert: exit 0, and the multiset of emitted scalars is exactly N × `é`.
/// The race needs real worker threads.
fn read_char_concurrent_atomic() {
    const N: usize = 400;
    let t = TmpDir::new();
    let entry = t.write("main.chz", READ_CHAR_CONCURRENT_PROG);
    let mut child = spawn(&entry);
    let mut stdin = child.stdin.take().unwrap();
    let payload = "é".repeat(N).into_bytes(); // 0xC3 0xA9 × N, no ASCII, no newline
    stdin.write_all(&payload).unwrap();
    drop(stdin); // EOF
    let out = child.wait_with_output().expect("wait");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "a torn concurrent read faulted; exit: {:?}\nstderr:\n{stderr}",
        out.status
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines.len(),
        N,
        "lost/duplicated scalars; got {} lines",
        lines.len()
    );
    assert!(
        lines.iter().all(|l| *l == "é"),
        "a scalar was torn into non-`é` output:\n{text}"
    );
}

#[test]
fn read_char_concurrent_atomic_mn() {
    read_char_concurrent_atomic();
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
fn broken_pipe_terminates_with_fault() {
    let t = TmpDir::new();
    let entry = t.write("main.chz", "while true:\n    print(\"x\")\n");
    let mut child = spawn(&entry);
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
    broken_pipe_terminates_with_fault();
}

/// R2 (N1 for the `Writer` path): a `stdout()`-backed `Writer` routes `write()` through the same
/// streaming sink `print` uses, so a write into a just-closed reader (`chezzi run x.chz | head -1`)
/// must raise the SAME deterministic broken-pipe halt `print` does — spec claims `stdout().write(s)`
/// is byte-identical to `io.print(s, end="")`. Without the `stream_halt` check at the `do_method_call`
/// Writer arm, the writes silently no-op, the loop spins forever, and the unbounded stream queue grows
/// without bound. `write`'s `Result` is deliberately IGNORED here: the halt is a VM fault raised at the
/// call site (like `print`, which returns `Nil`), independent of the `Result` value.
fn writer_stdout_broken_pipe_terminates_with_fault() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "import stdout from std.io\n\nw := stdout()\nwhile true:\n    w.write(\"x\\n\")\n",
    );
    let mut child = spawn(&entry);
    drop(child.stdout.take()); // close the read end immediately
    let mut err = String::new();
    let mut stderr = child.stderr.take().unwrap();
    let status = wait_timeout(&mut child, 20)
        .expect("a stdout() Writer kept writing to a dead pipe (no stream_halt at the Writer arm)");
    use std::io::Read;
    let _ = stderr.read_to_string(&mut err);
    assert!(status.code().is_some(), "killed by a signal: {status:?}");
    assert!(
        !status.success(),
        "a dead stdout truncated the Writer output — the run must not report success: {status:?}"
    );
    assert!(
        err.contains("stdout closed (broken pipe)"),
        "the fault must name the cause; stderr was:\n{err}"
    );
}

#[test]
fn writer_stdout_broken_pipe_terminates_with_fault_mn() {
    writer_stdout_broken_pipe_terminates_with_fault();
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
fn fault_under_broken_pipe_is_not_success() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "import std.time\n\nfn main():\n    defer:\n        print(\"cleanup\")\n    print(\"hello\")\n    time.sleep_ms(150)\n    xs := [1]\n    print(xs[5])\n\nmain()\n",
    );
    let mut child = spawn(&entry);
    drop(child.stdout.take()); // the reader is gone, as under `| head -1`
    let status = wait_timeout(&mut child, 20).expect("hung on a dead pipe");
    assert!(
        !status.success(),
        "a FAULTING run reported success because the dead pipe hijacked the exit channel: {status:?}"
    );
}

#[test]
fn fault_under_broken_pipe_is_not_success_mn() {
    fault_under_broken_pipe_is_not_success();
}

/// W7-5d — a dead stdout kills the job that touched it, NOT its siblings, and NOT the rest of a
/// sibling that never touched stdout at all. Two process-GLOBAL reads made it otherwise, both now
/// gated:
///
/// 1. `executor_hard_halt` folded in `stream::out_dead_reason()`, so once stdout died ANY fault
///    became a hard halt and tripped the executor's cancel flag, killing queued/eager siblings.
/// 2. `invoke_native` (and the `Writer` arm) called `stream_halt` after EVERY native, which reads the
///    same global — so a sibling doing three `fs.atomic_write`s faulted after the FIRST one, having
///    never printed. Now gated on `Vm::stdout_writes` moving during that call.
///
/// Both were NONDETERMINISTIC before the gate: how much of each sibling survived depended on how far
/// the pool had got when the pipe broke — for (1), neither marker at `--threads=1`, both
/// at `--threads=3+`, either answer at `--threads=2` across runs; for (2), 1 of 3 writes usually and
/// 3 of 3 sometimes at `--threads=2`. `--threads=1` is the load-bearing configuration: it is the one
/// that fails again the moment either gate becomes reachable.
///
/// A broken pipe is an ORDINARY fault (`Vm::stream_halt` sets neither `is_over_memory` nor
/// `is_timed_out`), so the W7-5 run-all contract applies to it unchanged. `writes` picks which half
/// this run fences: 1 marker write per job exercises (1) alone, 3 exercise (2) as well.
///
/// **Ancestors, measured on the 3-job shape under `| head -1`:** CPython `ThreadPoolExecutor` runs
/// every submitted job and completes all three writes at `max_workers` 1/2/4 — the ancestor that owns
/// `Executor` semantics, and what this asserts. Go has no executor; its goroutines take SIGPIPE on
/// fd 1 and the whole process dies, which is a signal policy Chezzi deliberately does not adopt
/// (`Vm::stream_halt` explains why: restoring SIGPIPE would break `std.net`'s EPIPE contract).
///
/// The `spew` job is what breaks the pipe. The markers write FILES, never stdout, so they are
/// observable after the pipe is gone — and the run must STILL fault non-zero naming the pipe, or this
/// would pass with `stream_halt` deleted outright.
fn dead_stdout_does_not_cancel_sibling_executor_jobs(threads: Option<usize>, writes: usize) {
    let t = TmpDir::new();
    let dir = t.0.display().to_string();
    let markers: Vec<String> = (1..=writes).map(|i| format!("m{i}.txt")).collect();
    let body: String = markers
        .iter()
        .enumerate()
        .map(|(i, m)| format!("    r{i} := fs.atomic_write(\"{dir}/{m}\", \"{i}\")\n"))
        .collect();
    let entry = t.write(
        "main.chz",
        &format!(
            "import std.concurrency\nimport std.fs\n\n\
             fn spew():\n    i := 0\n    while i < 500000:\n        print(\"x{{i}}\")\n        i = i + 1\n\n\
             fn markers():\n{body}\n\
             ex := Executor()\nex.submit(spew)\nex.submit(markers)\nex.shutdown()\n"
        ),
    );
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run");
    if let Some(n) = threads {
        cmd.arg(format!("--threads={n}"));
    }
    let mut child = cmd
        .arg(&entry)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    drop(child.stdout.take()); // the reader is gone, as under `| head -1`
    let mut stderr = child.stderr.take().unwrap();
    let status =
        wait_timeout(&mut child, 20).expect("kept printing to a dead pipe (EPIPE ignored)");
    let mut err = String::new();
    use std::io::Read;
    let _ = stderr.read_to_string(&mut err);
    let cfg = format!("threads={threads:?}, writes={writes}");
    for m in &markers {
        assert!(
            t.0.join(m).exists(),
            "a dead stdout stopped sibling Executor work at {m} ({cfg}) — a broken pipe is an \
             ordinary fault in the job that printed, and must not reach a job that never did"
        );
    }
    // The halt itself must still fire — otherwise deleting `stream_halt` outright would pass the
    // assertions above, and the printing job would spin the unbounded stream queue instead.
    assert!(
        !status.success(),
        "a dead stdout truncated the output — the run must not report success ({cfg}): {status:?}"
    );
    assert!(
        err.contains("stdout closed (broken pipe)"),
        "the printing job's fault must still name the pipe ({cfg}); stderr was:\n{err}"
    );
}

#[test]
fn dead_stdout_does_not_cancel_sibling_executor_jobs_mn() {
    dead_stdout_does_not_cancel_sibling_executor_jobs(None, 1);
}

#[test]
fn dead_stdout_does_not_cancel_sibling_executor_jobs_mn_one_thread() {
    dead_stdout_does_not_cancel_sibling_executor_jobs(Some(1), 1);
}

#[test]
fn dead_stdout_does_not_tear_a_multi_native_sibling_mn() {
    dead_stdout_does_not_cancel_sibling_executor_jobs(None, 3);
}

#[test]
fn dead_stdout_does_not_tear_a_multi_native_sibling_mn_one_thread() {
    dead_stdout_does_not_cancel_sibling_executor_jobs(Some(1), 3);
}

/// A stdout that CANNOT be written (`> /dev/full` → ENOSPC) must not be silently dropped: the run
/// fails loudly (diagnostic + non-zero exit), never "exit 0 with no output". Same policy as
/// `chezzi docs` (main.rs `write_stdout`): BrokenPipe = clean, any other errno = FAILURE.
fn write_error_is_reported() {
    let full = std::path::Path::new("/dev/full");
    if !full.exists() {
        return; // not Linux — nothing to assert against
    }
    let t = TmpDir::new();
    let entry = t.write("main.chz", "for i in range(100):\n    print(\"line\", i)\n");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run");
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
    write_error_is_reported();
}

/// A STALLED reader must not stall the engine. A streamed `print` used to be an inline, blocking
/// `write(2)` on a core worker (holding the process-global stdout lock): once the 64K pipe buffer
/// filled, every printing fiber pinned a worker in the kernel and an unrelated fiber in the same
/// nursery starved for as long as the consumer stalled (the D5 invariant: no blocking syscall on a
/// core worker). Witness: a task that sleeps, then writes a file — it must make progress while
/// nothing is draining stdout.
fn stalled_reader_does_not_starve_other_tasks() {
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
    cmd.arg("--threads=2"); // 8 printers >> workers
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
    stalled_reader_does_not_starve_other_tasks();
}

// ---------------------------------------------------------------------------------------------
// The writer threads RECORD; they never decide the program's fate. (Regressions for the first cut,
// where a failed write called `std::process::exit` from a detached thread.)
// ---------------------------------------------------------------------------------------------

/// stderr is a DIAGNOSTIC channel: an unwritable stderr (`2> /dev/full`, a dead `2> >(head -1)`
/// reader) must not touch a healthy program. It used to kill the process mid-run — exit 1 for a
/// program that had no error, with its stdout results truncated.
fn stderr_failure_does_not_kill_the_run() {
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
    stderr_failure_does_not_kill_the_run();
}

/// A closed stdout reader must not swallow the program's OUTCOME: a run that faults after the pipe
/// broke still reports FAILURE with its trace on stderr (it used to exit 0, silently, from the writer
/// thread — `main`'s exit-status handling never ran).
fn fault_after_broken_pipe_still_reports_the_fault() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        // One print (the pipe is already closed → the writer takes EPIPE during the sleep), then a
        // genuine fault, with no further print to halt on: the FAULT must decide the exit status.
        "import std.time\nprint(\"a\")\ntime.sleep_ms(300)\nxs := [1]\nprint(xs[5])\n",
    );
    let mut child = spawn(&entry);
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
    fault_after_broken_pipe_still_reports_the_fault();
}

/// `2>&1 | head -1` — BOTH writer threads take EPIPE in the same window. Two threads calling libc
/// `exit(3)` concurrently is undefined (a hang in the exit-handler lock, atexit run twice); no thread
/// exits the process any more, so this just terminates.
fn both_streams_broken_terminates() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "import std.io\nwhile true:\n    io.print(\"o\")\n    io.eprint(\"e\")\n",
    );
    let mut child = spawn(&entry);
    drop(child.stdout.take());
    drop(child.stderr.take());
    let status = wait_timeout(&mut child, 20).expect("did not terminate with both pipes closed");
    assert!(status.code().is_some(), "killed by a signal: {status:?}");
}

#[test]
fn both_streams_broken_terminates_mn() {
    both_streams_broken_terminates();
}

/// A PARTIAL line (`print(x, end="")`, no newline) must reach the terminal as it is produced — a
/// progress indicator, and the "a killed program keeps what it printed" contract. `std::io::stdout()`
/// is a `LineWriter`, so the writer thread must flush every message; otherwise nothing appears until
/// a newline (or ever, if the program is killed).
fn partial_line_print_is_visible_without_flush() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "print(\".\", end=\"\")\nwhile true:\n    x := 1\n",
    );
    let mut child = spawn(&entry);
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
    partial_line_print_is_visible_without_flush();
}

/// D5, the `io.flush()` seam: a stalled stdout consumer must not starve unrelated tasks. `flush`
/// (and `input`/`read_line`, which used to pre-flush) must never WAIT on the writer thread — that
/// thread is parked in `write(2)` on the full pipe, so waiting pins the fiber's core worker for as
/// long as the consumer stalls. Witness: a task that sleeps, then writes a file.
fn stalled_reader_flush_does_not_starve_other_tasks() {
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
    cmd.arg("--threads=2");
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
    stalled_reader_flush_does_not_starve_other_tasks();
}

/// gap N1: a FINITE program whose print(s) land in a pipe the reader already closed drops bytes but
/// has no next `print` site to fault at (the VM finished queueing before the writer's EPIPE). On the
/// old code the exit status was a race — 0 (VM outran the EPIPE, bytes silently dropped, SUCCESS) or
/// 1 (writer won). It must now be DETERMINISTIC non-zero, matching Python's `BrokenPipeError`: the
/// post-`flush_stream()` `out_dead_reason()` check in `main`. The reader is dropped at time 0, so the
/// write is GUARANTEED to EPIPE (no race in the test) and the run must fail every time.
fn last_print_into_closed_pipe_is_deterministically_nonzero() {
    let t = TmpDir::new();
    // A single print → exactly one write, no next print site: purest exercise of the post-flush path.
    let entry = t.write("main.chz", "print(\"bye\")\n");
    for _ in 0..30 {
        let mut child = spawn(&entry);
        drop(child.stdout.take()); // reader gone before any byte is written → the write must EPIPE
        let mut err = String::new();
        let mut stderr = child.stderr.take().unwrap();
        let status =
            wait_timeout(&mut child, 20).expect("hung after a last print into a dead pipe");
        let _ = stderr.read_to_string(&mut err);
        assert!(status.code().is_some(), "killed by a signal: {status:?}");
        assert!(
            !status.success(),
            "a last print into a closed pipe dropped bytes but reported SUCCESS: {status:?}"
        );
        assert!(
            err.contains("stdout closed (broken pipe)"),
            "the failure must name the cause (same phrase as a mid-program break); stderr:\n{err}"
        );
    }
}

#[test]
fn last_print_into_closed_pipe_is_deterministically_nonzero_mn() {
    last_print_into_closed_pipe_is_deterministically_nonzero();
}

/// N1 no-regression (risk a): when the reader DRAINS every byte to EOF before closing (the reader
/// read everything — nothing was dropped), NO write fails, OUT_DEAD stays unset, and the run must
/// still exit 0. Proves the N1 fix does not over-fire on a clean finite run.
fn fully_drained_output_stays_success() {
    let t = TmpDir::new();
    let entry = t.write("main.chz", "for i in range(5):\n    print(i)\n");
    for _ in 0..30 {
        let mut child = spawn(&entry);
        // Drain stdout fully to EOF on a helper thread — the reader reads EVERYTHING, so no write
        // ever fails. (Not via read_line-then-drop: that would manufacture a broken pipe.)
        let out = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut s = String::new();
            let mut rd = out;
            let _ = rd.read_to_string(&mut s);
            let _ = tx.send(s);
        });
        let status = wait_timeout(&mut child, 20).expect("clean finite run hung");
        let got = rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
        assert!(
            status.success(),
            "a fully-drained clean run reported failure: {status:?} (stdout: {got:?})"
        );
        assert_eq!(got.lines().count(), 5, "output truncated: {got:?}");
    }
}

#[test]
fn fully_drained_output_stays_success_mn() {
    fully_drained_output_stays_success();
}

// ===== W6-9 — `Writer.write_bytes` on `io.stdout()`/`io.stderr()` must be BYTE-EXACT =====
//
// `write_bytes(b"\xff\xfe")` on a FILE writer was already byte-exact; on the console backings it
// round-tripped through `String::from_utf8_lossy` and emitted `ef bf bd ef bf bd` (two U+FFFD).
// Python (`sys.stdout.buffer.write`) and Go (`os.Stdout.Write`) both emit the raw bytes.
//
// These live here rather than in `tests/chz/` because the in-VM test runner hands stdout back as a
// Rust `String`: only a real child process can witness the bytes that actually reach fd 1/2.

/// Run `chezzi run <file>` to completion, returning its raw stdout/stderr bytes.
fn run_bytes(entry: &PathBuf) -> (Vec<u8>, Vec<u8>, std::process::ExitStatus) {
    // `wait_with_output` closes our end of the child's stdin first, so a program that never reads
    // stdin cannot deadlock here.
    let out = spawn(entry).wait_with_output().expect("wait_with_output");
    (out.stdout, out.stderr, out.status)
}

fn stdout_write_bytes_is_byte_exact() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "import std.io\n\nw := io.stdout()\nw.write_bytes(b\"\\xff\\xfe\")\n",
    );
    let (out, err, status) = run_bytes(&entry);
    assert!(
        status.success(),
        "exit: {status:?}, stderr: {}",
        String::from_utf8_lossy(&err)
    );
    assert_eq!(
        out,
        vec![0xff, 0xfe],
        "stdout().write_bytes must be byte-exact (lossy UTF-8 round-trip?)"
    );
}

#[test]
fn stdout_write_bytes_is_byte_exact_mn() {
    stdout_write_bytes_is_byte_exact();
}

/// The sibling arm — a fix applied to only SOME arms of an N-way set is the recurring meta-finding.
fn stderr_write_bytes_is_byte_exact() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "import std.io\n\nw := io.stderr()\nw.write_bytes(b\"\\xff\\xfe\")\n",
    );
    let (out, err, status) = run_bytes(&entry);
    assert!(status.success(), "exit: {status:?}");
    assert!(out.is_empty(), "nothing was written to stdout: {out:?}");
    assert_eq!(
        err,
        vec![0xff, 0xfe],
        "stderr().write_bytes must be byte-exact"
    );
}

#[test]
fn stderr_write_bytes_is_byte_exact_mn() {
    stderr_write_bytes_is_byte_exact();
}

/// `io.buffered(io.stdout(), n)` reaches the `Stdout` backing through `write_to_core`'s drain
/// recursion (buffer-full) AND `flush_core`'s (explicit flush) — both must stay byte-exact. 6 bytes
/// through a cap-4 buffer, then 2 more + `flush()`, exercises each path in order.
fn buffered_stdout_write_bytes_is_byte_exact() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "import std.io\n\nw := io.buffered(io.stdout(), 4)\n\
         w.write_bytes(b\"\\xff\\xfe\\x00\\x01\\x80\\x81\")\n\
         w.write_bytes(b\"\\xfd\\xfc\")\nw.flush()\n",
    );
    let (out, err, status) = run_bytes(&entry);
    assert!(
        status.success(),
        "exit: {status:?}, stderr: {}",
        String::from_utf8_lossy(&err)
    );
    assert_eq!(
        out,
        vec![0xff, 0xfe, 0x00, 0x01, 0x80, 0x81, 0xfd, 0xfc],
        "buffered(stdout()) must drain byte-exactly, in order"
    );
}

#[test]
fn buffered_stdout_write_bytes_is_byte_exact_mn() {
    buffered_stdout_write_bytes_is_byte_exact();
}

// ===== W6-9r item 4 — `chezzi test --show-output` must be byte-exact too =====
//
// `chezzi run` is byte-exact since W6-9; `chezzi test --show-output` still decoded a test's captured
// stdout through `Vm::take_out` (`String::from_utf8_lossy`), so `\xff\xfe` rendered as two U+FFFD.
// Only a real child process (this file's rationale, above) can witness the bytes on fd 1, and this
// is the ONLY test that covers the CLI write site (`src/main.rs`) — the in-process `test_runner`
// tests can pass even if that site still does `print!("{}", report.text)`.

/// `test --show-output` on a failing test that writes non-UTF-8 bytes to stdout must put the raw
/// bytes on fd 1, indented under the failing test's report line, with no lossy replacement chars.
#[test]
fn show_output_is_byte_exact() {
    let t = TmpDir::new();
    let entry = t.write(
        "boom_test.chz",
        "import std.io\ntest fn boom():\n    print(\"before\")\n    \
         io.stdout().write_bytes(b\"\\xff\\xfe\")\n    assert false\n",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["test", "--show-output"])
        .arg(&entry)
        .output()
        .expect("run chezzi test --show-output");
    assert!(
        out.stdout.windows(2).any(|w| w == [0xff, 0xfe]),
        "--show-output must put the raw bytes on fd 1; stdout: {:?}",
        out.stdout
    );
    assert!(
        !out.stdout.windows(3).any(|w| w == [0xef, 0xbf, 0xbd]),
        "--show-output must NOT lossily replace non-UTF-8 bytes; stdout: {:?}",
        out.stdout
    );
}

/// M2 — a closed reader (`chezzi test --show-output | head -1`) truncates the report, same as any
/// other stdout write failure: the single end-of-run `report.bytes` write hits the already-closed pipe
/// (deterministic here since we drop our read end before the child ever writes, same technique as
/// `broken_pipe_terminates_with_fault` above). Measured against the reference runners with a PASSING
/// run piped into `head -1`: `go test -v` exits 141 (SIGPIPE), `pytest -s` exits 1 — neither treats a
/// closed reader as a clean pass, so neither does `chezzi test`. Matches `chezzi run`'s own
/// broken-pipe handling (`src/main.rs`, `out_dead_reason`).
#[test]
fn show_output_reports_a_closed_reader() {
    let t = TmpDir::new();
    let entry = t.write("ok_test.chz", "test fn t():\n    assert true\n");
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["test", "--show-output"])
        .arg(&entry)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi test --show-output");
    drop(child.stdout.take()); // the reader is gone, as under `| head -1`
    let out = child
        .wait_with_output()
        .expect("wait on chezzi test --show-output");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked at"), "panicked: {err}");
    assert!(
        !out.status.success(),
        "a closed reader truncates the report and must not report success: {:?} (stderr: {err})",
        out.status
    );
    assert!(
        err.contains("stdout closed (broken pipe)"),
        "no diagnostic for the closed reader: {err}"
    );
}

/// M2, the regression itself: a write failure that is NOT a closed reader must NOT be silently
/// swallowed into a SUCCESS exit — the report is genuinely truncated. Same technique and guard as
/// `write_error_is_reported` above (`chezzi run`'s equivalent contract): `/dev/full` (ENOSPC) is
/// world-writable, no root needed, so this is skipped only where the device node itself is absent.
/// (A stdout fd opened READ-ONLY was tried first to dodge `/dev/full` per the original brief, but
/// measured false-green on this toolchain: `std::io::Stdout::write_all` returns `Ok(())` on an EBADF
/// fd — verified with a raw `write(2)` syscall probe showing the byte never lands while `write_all`
/// still reports success — so that path can't distinguish pre/post-fix and was dropped.)
/// Before the fix (`let _ = write_all(...)`) this exits 0 despite the write failing; after the fix it
/// must exit non-zero.
#[test]
fn show_output_reports_failure_on_unwritable_stdout() {
    let full = std::path::Path::new("/dev/full");
    if !full.exists() {
        return; // not Linux — nothing to assert against
    }
    let t = TmpDir::new();
    let entry = t.write("ok_test.chz", "test fn t():\n    assert true\n");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["test", "--show-output"])
        .arg(&entry)
        .stdout(std::fs::File::create(full).unwrap())
        .stderr(Stdio::piped())
        .output()
        .expect("run chezzi test --show-output");
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

/// `io.read_all()` on non-UTF-8 stdin faults with the exact message `docs/stdlib.md`'s fault-contract
/// table quotes, and the fault is RECOVERABLE (`recover:` catches it, rc stays 0).
///
/// This lives here, not in `tests/chz/stdlib/fault_contracts_test.chz` where the rest of that table's
/// rows are pinned, for one reason: the `chezzi test` harness has no way to feed a test its own stdin,
/// so the contract is unassertable from inside the language. It is assertable from a real process, and
/// this file already owns the "spawn `chezzi` and drive its pipes" machinery — so the row gets a gate
/// rather than a note explaining why it has none.
#[test]
fn read_all_faults_recoverably_on_non_utf8_stdin() {
    let t = TmpDir::new();
    let entry = t.write(
        "readall.chz",
        "import std.io\n\nr := recover: io.read_all()\nmatch r:\n    Ok(v): print(\"ok len \" + str(v.len()))\n    Err(e): print(\"err: \" + e.message())\n",
    );

    // Invalid UTF-8: 0xff/0xfe can never appear in a well-formed UTF-8 stream.
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&entry)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi run");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"abc\xff\xfedef")
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(
        stdout.trim(),
        "err: stdin: stream is not valid UTF-8",
        "message must match docs/stdlib.md's fault-contract row byte-for-byte; got: {stdout}"
    );
    assert!(
        out.status.success(),
        "the fault is recoverable, so a program that recovers it must exit 0; got {:?}",
        out.status
    );

    // Negative control: valid UTF-8 on the same program takes the `Ok` arm, so the assertion above
    // is not passing merely because `read_all` always faults.
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&entry)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi run");
    child.stdin.take().unwrap().write_all(b"hello").unwrap();
    let out = child.wait_with_output().expect("wait");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "ok len 5",
        "valid UTF-8 stdin must reach the Ok arm"
    );
}
