//! Real-PROCESS exit-status tests for `std.os.exit(code)`. Drives the actual
//! `env!("CARGO_BIN_EXE_chezzi")` binary via `std::process::Command`, because the bug this pins —
//! `os.exit(-1)` reporting SUCCESS (status 0) to the shell/CI — is invisible to an in-VM assertion
//! on `pending_exit`: the old clamp turned a negative code into `0`, and only the process status
//! reveals that a "failure" exit was seen by the shell as a success.
//!
//! Rule under test (POSIX `exit(3)` / bash / Python / Go): the process status is the LOW 8 BITS of
//! the code — `code & 0xff`. So `-1` → 255, `300` → 44, `-256` → 0.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique temp directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_exit_{}_{}", std::process::id(), n));
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

/// Run `chezzi run [--serial] <file>` on a program that calls `os.exit(code)`; return the process
/// exit status the OS reports.
fn exit_status(code: &str, serial: bool) -> i32 {
    let t = TmpDir::new();
    let entry = t.write("main.chz", &format!("import std.os\nos.exit({code})\n"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run");
    if serial {
        cmd.arg("--serial");
    }
    let out = cmd.arg(&entry).output().expect("spawn chezzi");
    out.status.code().expect("exited with a status (no signal)")
}

#[test]
fn os_exit_status_is_the_low_8_bits_on_both_engines() {
    // (code, expected process status) — the POSIX mask, both ends.
    let cases = [
        ("0", 0),     // boundary: success stays success
        ("1", 1),     // boundary: the ordinary failure code
        ("-1", 255),  // THE BUG: used to clamp to 0 = silent SUCCESS in CI
        ("255", 255), // boundary: the top of the byte
        ("300", 44),  // >255 masks (300 & 0xff), exactly like POSIX `exit(300)`
        ("-256", 0),  // a negative multiple of 256 masks to 0 — the mask is total, not a clamp
        ("-2", 254),  // a second negative, to pin two's-complement masking
    ];
    for (code, want) in cases {
        for serial in [false, true] {
            let got = exit_status(code, serial);
            assert_eq!(
                got,
                want,
                "os.exit({code}) on {} engine: expected process status {want}, got {got}",
                if serial { "serial" } else { "M:N" }
            );
        }
    }
}

#[test]
fn a_program_that_never_exits_explicitly_succeeds() {
    // Boundary/no-regression: without `os.exit`, a clean program is still status 0.
    let t = TmpDir::new();
    let entry = t.write("main.chz", "print(\"hi\")\n");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run"])
        .arg(&entry)
        .output()
        .expect("spawn chezzi");
    assert_eq!(out.status.code(), Some(0), "a clean run exits 0");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
}

/// Run `chezzi <sub> <file>` under a watchdog: `(status, stdout)`, or a PANIC if it outlives `secs`.
/// A hang must FAIL the test, never mask as a pass — which is exactly what W7-47 was before the fix.
/// std only (no `timeout(1)` dependency). Output is tiny, so reading the pipe after the wait cannot
/// deadlock on a full buffer.
fn run_capped_sub(sub: &str, entry: &std::path::Path, secs: u64) -> (i32, String) {
    use std::io::Read;
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg(sub)
        .arg(entry)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(s) => break s,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("chezzi {sub} {} hung for >{secs}s", entry.display());
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    };
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("piped stdout")
        .read_to_string(&mut out)
        .expect("read stdout");
    (status.code().expect("exited with a status"), out)
}

fn run_capped(entry: &std::path::Path, secs: u64) -> (i32, String) {
    run_capped_sub("run", entry, secs)
}

/// W7-47 — an eager `Executor` job's `os.exit` must terminate the process while `main` is parked in
/// a socket op, like Go's `os.Exit` from a goroutine (measured: rc=3, immediate). Before the fix the
/// exit code sat on the job's isolated worker `Vm` until a join `main` could never reach, and the run
/// hung forever (rc=124 under `timeout`) — so the watchdog above IS the assertion.
///
/// M:N only: `--serial` refuses the socket op outright (W7-40's documented engine difference) and so
/// already exits 3 today by a different route. An ephemeral port (`:0`), never a fixed one — CI collides.
#[test]
fn eager_job_os_exit_terminates_a_socket_blocked_main() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        r#"import std.net
import std.concurrency
import std.os
import std.time

fn bail():
    time.sleep_ms(50)
    print("job exiting")
    os.exit(3)

ex := Executor()
ex.submit(bail)
match net.listen("127.0.0.1:0"):
    Ok(l):
        print("listening")
        match l.accept():
            Ok(c):
                print("accepted")
            Err(e):
                print("accept err")
    Err(e):
        print("listen err")
"#,
    );
    let (status, out) = run_capped(&entry, 20);
    assert_eq!(
        status, 3,
        "the job's os.exit(3) is the process status; got {status} (out: {out:?})"
    );
    assert!(out.contains("listening"), "main reached accept(): {out:?}");
    assert!(out.contains("job exiting"), "the job ran: {out:?}");
}

/// W7-47, the channel variant — the socket is only the *reachable* example, not the trigger. Here
/// `main` registers as a `PartyWait::Recv`, so pre-fix the run did not hang: the quiescence verdict
/// fired first and it reported `recv on an empty channel: deadlock` with rc=1. Wrong answer rather
/// than no answer, and Go says 3 — which is why the exit rung sits ABOVE `quiesced()`.
#[test]
fn eager_job_os_exit_terminates_a_recv_blocked_main() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        r#"import std.concurrency
import std.os
import std.time

fn bail():
    time.sleep_ms(50)
    print("job exiting")
    os.exit(3)

ex := Executor()
ex.submit(bail)
ch := Channel[int]()
print("waiting")
v := ch.recv()
print("got {v}")
"#,
    );
    let (status, out) = run_capped(&entry, 20);
    assert_eq!(
        status, 3,
        "the job's os.exit(3) beats the deadlock verdict; got {status} (out: {out:?})"
    );
    assert!(out.contains("waiting"), "main reached recv(): {out:?}");
    assert!(out.contains("job exiting"), "the job ran: {out:?}");
}

/// W7-47 review defect 1 — the run-wide exit cell must NOT leak between `test fn`s. `chezzi test`
/// builds ONE `Vm` per test FILE and reuses it (`invoke_all`), and `pending_exit` is reset per
/// invocation while the cell is not — so before the reset in `Vm::invoke_test`, a `test fn` calling
/// `os.exit` made every LATER test that blocks fail with `exit`. Order-dependent and identical on
/// both engines, so the `.chz` suite's serial==M:N gate is structurally blind to it — it has to be
/// asserted at the runner level, here.
#[test]
fn a_test_fn_that_exits_does_not_poison_later_blocking_tests() {
    let t = TmpDir::new();
    let entry = t.write(
        "poison_test.chz",
        r#"import std.os
import std.time

test fn a_exits():
    os.exit(0)

test fn b_sleeps():
    time.sleep_ms(10)
    assert 1 == 1

test fn c_plain():
    assert 2 == 2
"#,
    );
    let (_status, out) = run_capped_sub("test", &entry, 20);
    // `a_exits` still errors (an `os.exit` inside a test is not a pass) — but only IT does.
    assert!(
        out.contains("ERROR a_exits"),
        "the exiting test errors: {out:?}"
    );
    assert!(
        out.contains("PASS b_sleeps"),
        "the LATER blocking test must not inherit the exit: {out:?}"
    );
    assert!(
        out.contains("PASS c_plain"),
        "and neither does a non-blocking one: {out:?}"
    );
}

/// W7-47 review defect 2 — an `os.exit` from an eager `Executor` job while a NURSERY task is blocked
/// must be the process status, not a `deadlock` verdict about the user's program. `first_exit` in
/// `reduce_task_slots` only ever comes from a task SLOT, so a job's exit (it owns no slot) was
/// invisible there and the nursery's deadlock error won instead — a confident wrong answer, the
/// `parked-is-not-stuck` class. Go: rc=3.
///
/// The `sleeper` sibling is load-bearing, not decoration: it keeps the nursery's deadlock predicate
/// vetoed past the 50 ms exit, so the verdict is formed AFTER an exit exists. Without a live sibling
/// the predicate fires at ~11 ms — before any exit — which is a separate, pre-existing false
/// deadlock (a live eager job that WILL `send` is ignored by the same predicate), out of scope here.
#[test]
fn eager_job_os_exit_beats_a_blocked_nurserys_deadlock_verdict() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        r#"import std.concurrency
import std.os
import std.time

ch := Channel[int]()

fn bail():
    time.sleep_ms(50)
    print("job exiting")
    os.exit(3)

fn waiter():
    print("child waiting")
    v := ch.recv()
    print("child got {v}")

fn sleeper():
    time.sleep_ms(300)

ex := Executor()
ex.submit(bail)
parallel:
    spawn waiter()
    spawn sleeper()
print("after nursery")
"#,
    );
    let (status, out) = run_capped(&entry, 20);
    assert_eq!(
        status, 3,
        "the job's os.exit(3) outranks the nursery deadlock verdict; got {status} (out: {out:?})"
    );
    assert!(
        out.contains("child waiting"),
        "the nursery task blocked: {out:?}"
    );
    assert!(
        !out.contains("after nursery"),
        "the nursery never completed: {out:?}"
    );
}
