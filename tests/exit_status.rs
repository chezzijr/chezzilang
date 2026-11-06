//! Real-PROCESS exit-status tests for `std.os.exit(code)`. Drives the actual
//! `env!("CARGO_BIN_EXE_chezzi")` binary via `std::process::Command`, because the bug this pins —
//! `os.exit(-1)` reporting SUCCESS (status 0) to the shell/CI — is invisible to an in-VM assertion
//! on `pending_exit`: the old clamp turned a negative code into `0`, and only the process status
//! reveals that a "failure" exit was seen by the shell as a success.
//!
//! Rule under test (POSIX `exit(3)` / bash / Python / Go): the process status is the LOW 8 BITS of
//! the code — `code & 0xff`. So `-1` → 255, `300` → 44, `-256` → 0.

#[path = "support/hang_deadline.rs"]
mod hang_deadline;

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

/// Run `chezzi run <file>` on a program that calls `os.exit(code)`; return the process exit status
/// the OS reports.
fn exit_status(code: &str) -> i32 {
    let t = TmpDir::new();
    let entry = t.write("main.chz", &format!("import std.os\nos.exit({code})\n"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run");
    let out = cmd.arg(&entry).output().expect("spawn chezzi");
    out.status.code().expect("exited with a status (no signal)")
}

#[test]
fn os_exit_status_is_the_low_8_bits() {
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
        let got = exit_status(code);
        assert_eq!(
            got, want,
            "os.exit({code}): expected process status {want}, got {got}"
        );
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

#[test]
fn os_exit_in_a_match_arm_exits_with_that_status() {
    // TICKET-263 (owner, 2026-10-18; Go `os.Exit`): `os.exit` is an ordinary call to the checker, so
    // a statement arm calls it and writes `panic("unreachable")` after it. The process exits with
    // that status and the `print("after")` after the match never runs.
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "import std.os\nfn main():\n    r: int!str = !\"boom\"\n    match r:\n        ?v: print(v)\n        !e:\n            os.exit(2)\n            panic(\"unreachable\")\n    print(\"after\")\nmain()\n",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&entry)
        .output()
        .expect("spawn chezzi");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
}

/// Run `chezzi <sub> <file>` under a watchdog: `(status, stdout)`, or a PANIC if it outlives `secs`.
/// A hang must FAIL the test, never mask as a pass — which is exactly what W7-47 was before the fix.
/// std only (no `timeout(1)` dependency). Output is tiny, so reading the pipe after the wait cannot
/// deadlock on a full buffer.
fn run_capped_sub(sub: &str, entry: &std::path::Path, secs: u64) -> (i32, String) {
    let (status, out, _) = run_capped_timed(&[sub], entry, secs);
    (status, out)
}

/// [`run_capped_sub`] plus the WALL CLOCK. W7-57's three shapes are latency bugs as much as status
/// bugs — `(b)`/`(c)` already reported rc=3, just 3 s late and after printing a line Go never prints —
/// so a status-only assertion passes on the broken binary. The elapsed bound is the assertion.
fn run_capped_timed(
    args: &[&str],
    entry: &std::path::Path,
    secs: u64,
) -> (i32, String, std::time::Duration) {
    use std::io::Read;
    let started = std::time::Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(args)
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
                panic!(
                    "chezzi {} {} hung for >{secs}s",
                    args.join(" "),
                    entry.display()
                );
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    };
    let elapsed = started.elapsed();
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("piped stdout")
        .read_to_string(&mut out)
        .expect("read stdout");
    (status.code().expect("exited with a status"), out, elapsed)
}

fn run_capped(entry: &std::path::Path, secs: u64) -> (i32, String) {
    run_capped_sub("run", entry, secs)
}

/// W15-1 — closing a `Listener` from another task while a sibling is parked in `accept()` must wake
/// the parked task with an `Err`, like Go's `Close` cancelling a blocked `Accept`. Pre-fix a close
/// that races the accept's park re-arms the netpoller on the fd `close` is dropping: the run hangs,
/// or a worker panics with `netpoller add: Os { code: 9, kind: Uncategorized, message: "Bad file
/// descriptor" }` (`src/vm/poller.rs:269`). One close per run hit that window in only ~0-15% of
/// DEBUG runs, so the program below closes 200 listeners back to back, each racing its own accept.
/// Measured on the base DEBUG binary (TICKET-166 planning, 2026-09-22): 10 of 10 runs hang or panic
/// at `CHEZZI_THREADS=2` and at the default worker count. (`CHEZZI_THREADS=1` never races: the
/// closing task holds the only worker.) No sleep orders anything: an accept that runs after the
/// close returns the closed `Err` too, so every interleaving must reach `done`.
#[test]
fn closing_a_listener_wakes_a_parked_accept() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        r#"import std.net
fn main() -> None!:
    for _i in 0..200:
        ln := net.listen("127.0.0.1:0")?
        parallel:
            spawn:
                _r := ln.accept()
            spawn:
                ln.close()
    print("done")
    return
main()?
"#,
    );
    // The bounded poll lives in `hang_deadline::run_with_hang_deadline`, outside this `#[test]` body
    // (DEC-117), so both failure shapes — a hang (killed, status `None`) and the netpoller panic
    // (rc=101) — report through the SAME literal assertion text below.
    for threads in [Some("2"), None] {
        for _ in 0..3 {
            let run = hang_deadline::run_with_hang_deadline(&entry, threads);
            let status = run.as_ref().map(|o| o.status);
            let out = run
                .as_ref()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            let ok = status.is_some_and(|s| s.success()) && out.contains("done");
            assert!(
                ok,
                "W15-1: close() must wake the parked accept() with an Err, not hang or crash \
                 the netpoller; got status {status:?} at CHEZZI_THREADS={threads:?} (out: {out:?})"
            );
        }
    }
}

/// W15-1 — closing a `Socket` from another task while a sibling is parked in `read()` must wake the
/// parked task with an `Err`, same contract as [`closing_a_listener_wakes_a_parked_accept`]. Measured
/// on the base DEBUG binary (TICKET-166 planning, 2026-09-22): 10 of 10 runs hang at
/// `CHEZZI_THREADS=2` and at the default worker count. No sleep orders anything: a `read` that runs
/// after the close returns the closed `Err` too, so every interleaving must reach `done`.
#[test]
fn closing_a_socket_wakes_a_parked_read() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        r#"import std.net
fn main() -> None!:
    ln := net.listen("127.0.0.1:0")?
    addr := ln.addr()?
    for _i in 0..200:
        c := net.connect(addr)?
        _s := ln.accept()?
        parallel:
            spawn:
                _r := c.read(10)
            spawn:
                c.close()
    print("done")
    return
main()?
"#,
    );
    for threads in [Some("2"), None] {
        for _ in 0..3 {
            let run = hang_deadline::run_with_hang_deadline(&entry, threads);
            let status = run.as_ref().map(|o| o.status);
            let out = run
                .as_ref()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            let ok = status.is_some_and(|s| s.success()) && out.contains("done");
            assert!(
                ok,
                "W15-1: close() must wake the parked read() with an Err, not hang or crash \
                 the netpoller; got status {status:?} at CHEZZI_THREADS={threads:?} (out: {out:?})"
            );
        }
    }
}

/// W7-47 — an eager `Executor` job's `os.exit` must terminate the process while `main` is parked in
/// a socket op, like Go's `os.Exit` from a goroutine (measured: rc=3, immediate). Before the fix the
/// exit code sat on the job's isolated worker `Vm` until a join `main` could never reach, and the run
/// hung forever (rc=124 under `timeout`) — so the watchdog above IS the assertion.
///
/// Needs real worker threads (the since-removed cooperative engine refused the socket op outright — W7-40's documented engine difference) and so
/// already exits 3 today by a different route. An ephemeral port (`:0`), never a fixed one — CI collides.
#[test]
fn eager_job_os_exit_terminates_a_socket_blocked_main() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        r#"import std.net
import Executor from std.concurrency
import std.os
import std.time

fn bail():
    time.sleep_ms(50)
    print("job exiting")
    os.exit(3)

ex := Executor()
ex.submit(bail)
match net.listen("127.0.0.1:0"):
    ?l:
        print("listening")
        match l.accept():
            ?c:
                print("accepted")
            !e:
                print("accept err")
    !e:
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
        r#"import Executor from std.concurrency
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
/// the VM's own buffered sink, so an in-process `.chz` test is structurally blind to it — it has to be
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

fn spin_a_while():
    i := 0
    while i < 200000:
        i = i + 1

fn nap():
    time.sleep_ms(20)

test fn d_loops_and_sleeps_in_a_nursery():
    # W7-57 — the two things a leaked `os.exit` would kill that the pre-W7-57 reset never had to
    # cover: a LOOP (whose back-edge now samples the exit) and a nursery SLEEP (which `cancel_all`
    # tears down). It pins the `exit` CELL's reset, which is the load-bearing half; the paired
    # `exit_pending` atomic is only a hint (every reader confirms `pending()`), so this does NOT pin
    # that store and no longer claims to.
    parallel:
        spawn spin_a_while()
        spawn nap()
    n := 0
    while n < 200000:
        n = n + 1
    assert n == 200000
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
    assert!(
        out.contains("PASS d_loops_and_sleeps_in_a_nursery"),
        "nor a LOOPING/nursery-SLEEPING one — the W7-57 back-edge flag must be cleared too: {out:?}"
    );
}

/// W7-47 review defect 2 — an `os.exit` from an eager `Executor` job while a NURSERY task is blocked
/// must be the process status, not a `deadlock` verdict about the user's program. `first_exit` in
/// `reduce_task_slots` only ever comes from a task SLOT, so a job's exit (it owns no slot) was
/// invisible there and the nursery's deadlock error won instead — a confident wrong answer, the
/// `parked-is-not-stuck` class. Go: rc=3.
///
/// The predicate is now vetoed by TWO independent things while `bail` runs: the outstanding eager job
/// itself (W7-56 — an uncounted sender vetoes) and the `inflight` `sleeper`. Either alone keeps the
/// verdict from forming before the 50 ms exit. `sleeper` is RETAINED deliberately so this test's setup
/// does not depend on W7-56's veto — otherwise it would quietly turn into a W7-56 regression test and
/// stop covering what it is for.
///
/// (The ~11 ms fire this comment used to describe — the predicate faulting before `bail` had even run,
/// because a live eager job that WILL `send` was invisible to it — is FIXED, by W7-56. It is covered
/// by `vm::tests::executor_job_feeds_a_parked_nursery_task_instead_of_a_false_deadlock`.)
#[test]
fn eager_job_os_exit_beats_a_blocked_nurserys_deadlock_verdict() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        r#"import Executor from std.concurrency
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

// ===== gaps.md W7-57 — a run-wide `os.exit` must reach the parties that are not POLLING =====
//
// W7-47 published the exit where every *blocking wait* could see it. That leaves out every party
// whose next checkpoint is not a poll: a fiber spinning in a CPU loop reaches no wait at all, and a
// fiber parked on a `recv` or asleep is only woken by its OWN nursery's teardown — which an exit from
// OUTSIDE that nursery never ran. Measured on the pre-fix release binary, against Go 1.26.5:
//
//   shape                                   Chezzi HEAD           Go 1.26.5
//   spinning nursery task                    rc=124, hangs         rc=3, 54 ms
//   `recv`-parked task + 3 s sibling         rc=3 at 3013 ms, +"keepalive done"   rc=3, 53 ms
//   task inside `time.sleep_ms(3000)`        rc=3 at 3012 ms, +"slow child finished"  rc=3, 53 ms
//   spinning top-level `main`                rc=124, hangs         (n/a — Go has no top-level split)
//   in-callback `sleep_ms` (`native_reentry > 0`)  rc=3 at 3012 ms  rc=3, ~53 ms
//
// The fix runs the intra-nursery abort teardown (`cancel_drain` + `drain_sched`) run-WIDE from
// `request_exit`, plus an exit rung at the two CPU-side checkpoints no blocking wait covers: the loop
// back-edge (sampled 1/1024) and `guarded`'s native-HOF per-element re-entry.
//
// **M:N only, deliberately.** The removed `--serial` engine ran an eager `Executor` job at the exit
// drain rather than at `submit`, so on that engine the exiting party never existed while the nursery
// ran — these programs were outside the cross-engine parity contract by construction, and the fix
// never touched the serial engine (verified by hand before it was removed).
//
// Every one asserts ELAPSED as well as the status: on the pre-fix binary shapes (b)/(c) already
// returned 3.

/// Programs share this preamble: an eager job that exits 3 after 50 ms.
const BAIL: &str = r#"import std.concurrency
import std.time
import std.os

fn bail():
    time.sleep_ms(50)
    print("job exiting")
    os.exit(3)
"#;

/// Shared assertion: the job's exit is the process status, it arrived promptly, and the party that
/// was supposed to die did not print its completion line.
fn assert_prompt_exit(out: &str, status: i32, elapsed: std::time::Duration, ms: u64, absent: &str) {
    assert_eq!(
        status, 3,
        "the job's os.exit(3) is the process status; got {status} (out: {out:?})"
    );
    assert!(out.contains("job exiting"), "the exiting job ran: {out:?}");
    assert!(
        elapsed < std::time::Duration::from_millis(ms),
        "the exit must be prompt (Go: ~53 ms), took {elapsed:?} (out: {out:?})"
    );
    assert!(
        !out.contains(absent),
        "the doomed party must not run to completion ({absent:?}): {out:?}"
    );
}

/// W7-57 (a) — a nursery task in a tight `while` loop. Pre-fix: rc=124, hung forever, because a
/// spinner's only checkpoint is the loop back-edge and nothing there read the run-wide exit.
#[test]
fn eager_job_os_exit_kills_a_cpu_spinning_nursery_task() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        &format!(
            r#"import Executor from std.concurrency
{BAIL}
fn spinner():
    i := 0
    total := 0
    while i < 2000000000:
        total = total + i
        i = i + 1
    print("spinner finished {{total}}")

ex := Executor()
ex.submit(bail)
parallel:
    spawn spinner()
print("after nursery")
"#
        ),
    );
    let (status, out, elapsed) = run_capped_timed(&["run"], &entry, 20);
    assert_prompt_exit(&out, status, elapsed, 2000, "spinner finished");
}

/// W7-57 (b) — a nursery task parked on `recv`, kept alive by a 3 s sleeping sibling. Pre-fix the
/// status was already 3, but only after the sibling's full 3 s, and `keepalive done` printed — a line
/// Go never prints. Status-only, this test passes on the broken binary; the clock is the assertion.
#[test]
fn eager_job_os_exit_kills_a_recv_parked_nursery_task() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        &format!(
            r#"import Executor from std.concurrency
{BAIL}
ch := Channel[int]()

fn parked():
    print("child parked on recv")
    print(ch.recv())

fn keepalive():
    time.sleep_ms(3000)
    print("keepalive done")

ex := Executor()
ex.submit(bail)
parallel:
    spawn parked()
    spawn keepalive()
print("after nursery")
"#
        ),
    );
    let (status, out, elapsed) = run_capped_timed(&["run"], &entry, 20);
    assert_prompt_exit(&out, status, elapsed, 1500, "keepalive done");
    assert!(
        out.contains("child parked on recv"),
        "the task really parked: {out:?}"
    );
}

/// W7-57 (c) — a nursery task inside `time.sleep_ms(3000)`. The sleep is already CHUNKED at
/// `DEMOTE_POLL_BACKOFF` by W7-16 (`arm_timer_sleep` re-arms and re-reads `t.cancel` each tick), so no
/// new chunking is needed — the fix is only that `cancel_all` now TRIPS the flag that loop reads.
#[test]
fn eager_job_os_exit_kills_a_sleeping_nursery_task() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        &format!(
            r#"import Executor from std.concurrency
{BAIL}
fn slowchild():
    time.sleep_ms(3000)
    print("slow child finished")

ex := Executor()
ex.submit(bail)
parallel:
    spawn slowchild()
print("after nursery")
"#
        ),
    );
    let (status, out, elapsed) = run_capped_timed(&["run"], &entry, 20);
    assert_prompt_exit(&out, status, elapsed, 1500, "slow child finished");
}

/// W7-57, the shape the gap row never named — a spinning **top-level `main`**. It has NO cancel flag
/// at all (`cancel == None`, `cancel_outer` empty), so no scope teardown can ever reach it and it hung
/// forever pre-fix. This is the only test here that fails without the `jump_checked` back-edge rung.
#[test]
fn eager_job_os_exit_kills_a_cpu_spinning_main() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        &format!(
            r#"import Executor from std.concurrency
{BAIL}
ex := Executor()
ex.submit(bail)
i := 0
total := 0
while i < 2000000000:
    total = total + i
    i = i + 1
print("main finished {{total}}")
"#
        ),
    );
    let (status, out, elapsed) = run_capped_timed(&["run"], &entry, 20);
    assert_prompt_exit(&out, status, elapsed, 2000, "main finished");
}

/// W7-57, the second unnamed shape — a spinning SIBLING eager job. Its `cancel` is its executor's
/// `cancel` token, which an `os.exit` must NOT trip, so the back-edge rung is deliberately not
/// gated on "this party has no cancel flag": the job HAS one, it is just the wrong one. (This shape
/// already reported rc=3 promptly pre-fix — `main`'s join observed the exit and returned while the
/// spinner ran on — so it is a FENCE against the teardown reclassifying or delaying it.)
#[test]
fn eager_job_os_exit_kills_a_cpu_spinning_sibling_job() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        &format!(
            r#"import Executor from std.concurrency
{BAIL}
fn spinjob():
    i := 0
    total := 0
    while i < 2000000000:
        total = total + i
        i = i + 1
    print("sibling job finished {{total}}")

ex := Executor()
ex.submit(spinjob)
ex.submit(bail)
print("main waiting")
"#
        ),
    );
    let (status, out, elapsed) = run_capped_timed(&["run"], &entry, 20);
    assert_prompt_exit(&out, status, elapsed, 2000, "sibling job finished");
}

/// W7-57 — an in-callback `time.sleep_ms` (`native_reentry > 0` ⇒ `Vm::demote_block_sleep`, which
/// sleeps on the HOST stack rather than parking a fiber). Its cancel and W7-47 exit rungs sat below one
/// uninterruptible `thread::sleep(ms)`, so they fired only after the whole sleep: measured 3012 ms
/// pre-fix, 63 ms after chunking it at `DEMOTE_POLL_BACKOFF` like `Vm::block_until_deadline`.
#[test]
fn eager_job_os_exit_kills_an_in_callback_sleep() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        &format!(
            r#"import Executor from std.concurrency
{BAIL}
fn slow(x: int) -> int:
    time.sleep_ms(3000)
    return x

fn child():
    xs := [1]
    ys := xs.map(slow)
    print("callback sleep finished {{ys}}")

ex := Executor()
ex.submit(bail)
parallel:
    spawn child()
print("after nursery")
"#
        ),
    );
    let (status, out, elapsed) = run_capped_timed(&["run"], &entry, 20);
    assert_prompt_exit(&out, status, elapsed, 1500, "callback sleep finished");
}

/// W7-57, the third unnamed shape — a party inside a NATIVE HOF's element loop (`list`/iterator
/// `map`/`filter`/`fold`, a comparator, an operator overload). That Rust loop emits no `Op::Jump`, so
/// `jump_checked`'s back-edge never fires inside it; its only checkpoint is `Vm::guarded`'s per-element
/// re-entry, which observed CANCEL alone. A top-level `main` (or an eager job) has no applicable cancel
/// flag, so it ran the whole HOF to completion after the exit — measured 2154 ms on the release binary
/// for `range(0, 10_000_000).map(f).fold(0, g)`, printing its completion line, versus Go's immediate
/// `os.Exit`. The `guarded` exit rung closes it.
///
/// (`--timeout` still has this gap at the same checkpoint — the same program under
/// `chezzi test --timeout=500` ran 2208 ms and reported `assertion failed`, not `TIMED-OUT`. That is a
/// separate pre-existing defect, not fixed here.)
#[test]
fn eager_job_os_exit_kills_a_main_inside_a_native_hof() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        &format!(
            r#"import Executor from std.concurrency
{BAIL}
fn double(x: int) -> int:
    return x * 2 + 1

fn keep(a: int, b: int) -> int:
    return a

ex := Executor()
ex.submit(bail)
n := List.from_iter(0..10000000).map(double).fold(0, keep)
print("main hof finished {{n}}")
"#
        ),
    );
    let (status, out, elapsed) = run_capped_timed(&["run"], &entry, 60);
    assert_prompt_exit(&out, status, elapsed, 1500, "main hof finished");
}

/// FALSE-HALT FENCE — the direction that actually matters. With NO `os.exit` anywhere, a sleeping
/// nursery task must sleep its full 300 ms and then run on. A `cancel_all` that fired without an exit,
/// or an `exit_pending` flag latched from nothing, would truncate this.
#[test]
fn a_sleeping_nursery_task_is_untouched_without_an_exit() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        r#"import std.time

fn napper():
    time.sleep_ms(300)
    print("napper done")

parallel:
    spawn napper()
print("after nursery")
"#,
    );
    let (status, out, elapsed) = run_capped_timed(&["run"], &entry, 20);
    assert_eq!(status, 0, "a clean run exits 0 (out: {out:?})");
    assert!(out.contains("napper done"), "the sleep completed: {out:?}");
    assert!(out.contains("after nursery"), "the nursery joined: {out:?}");
    assert!(
        elapsed >= std::time::Duration::from_millis(250),
        "the sleep was NOT truncated, took {elapsed:?}"
    );
}

/// FALSE-HALT FENCE — a nursery that completes normally: a task parked on `recv` is fed by a sibling
/// and both finish. The teardown must not disturb an ordinary park/wake.
#[test]
fn a_normally_completing_nursery_is_untouched_without_an_exit() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        r#"import std.time

ch := Channel[int]()

fn taker():
    print("taker got {ch.recv()}")

fn giver():
    time.sleep_ms(50)
    ch.send(7)

parallel:
    spawn taker()
    spawn giver()
print("after nursery")
"#,
    );
    let (status, out) = run_capped(&entry, 20);
    assert_eq!(status, 0, "a clean run exits 0 (out: {out:?})");
    assert!(out.contains("taker got 7"), "the handoff happened: {out:?}");
    assert!(out.contains("after nursery"), "the nursery joined: {out:?}");
}

/// TICKET-213 (owner decision 5) — **an exit runs no `defer` of a cancelled sibling** (Go `os.Exit`,
/// Python `os._exit`). Until TICKET-213 this test asserted the opposite rule (W7-57: the sibling's
/// `defer` ran whole). The job's exit is published while `sib` sleeps; the halt wakes `sib`, it
/// unwinds, and its `defer` must not start: no `DEFER ENTER`, and the body never resumes.
#[test]
fn an_exit_runs_no_defer_of_a_cancelled_sibling() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        &format!(
            r#"import Executor from std.concurrency
{BAIL}
fn double(x: int) -> int:
    return x * 2

fn keep(a: int, b: int) -> int:
    return a

fn cleanup():
    print("DEFER ENTER")
    i := 0
    while i < 3000000:
        i = i + 1
    n := List.from_iter(0..1000000).map(double).fold(0, keep)
    print("DEFER EXIT {{i}} {{n}}")

fn sib():
    defer cleanup()
    time.sleep_ms(3000)
    print("sib body done")

ex := Executor()
ex.submit(bail)
parallel:
    spawn sib()
print("after nursery")
"#
        ),
    );
    let (status, out) = run_capped(&entry, 30);
    assert_eq!(status, 3, "the job's exit is the status (out: {out:?})");
    assert!(
        !out.contains("DEFER ENTER"),
        "an exit runs no defer of a sibling (out: {out:?})"
    );
    assert!(
        !out.contains("sib body done"),
        "the sibling's body never resumes (out: {out:?})"
    );
}

/// TICKET-213 — an exit CUTS a `defer` body that is already running. The `defer` body sends the
/// handshake, so the job's exit lands while the body sleeps.
#[test]
fn an_exit_cuts_a_running_defer_body() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        r#"import Executor from std.concurrency
import std.time
import std.os
hs := Channel[int](1)
fn cleanup():
    hs.send(1)
    time.sleep_ms(3000)
    print("DEFER EXIT")
fn work():
    defer cleanup()
fn bail():
    hs.recv()
    os.exit(3)
ex := Executor()
ex.submit(bail)
work()
print("after work")
"#,
    );
    let (status, out) = run_capped(&entry, 30);
    assert_eq!(status, 3, "the job's exit is the status (out: {out:?})");
    assert!(
        !out.contains("DEFER EXIT"),
        "the exit cut the defer body (out: {out:?})"
    );
    assert!(
        !out.contains("after work"),
        "main did not run on (out: {out:?})"
    );
}

/// TICKET-213 — the `is_cancelled()` cell of `exit_halt`: `boom` cancels `sib`, whose `defer` body
/// is spinning on the CPU when the job's exit lands. A fiber already cut as `Cancelled` runs user
/// code only inside a `defer` of its own unwind, and the exit must cut that body too. With
/// `exit_halt` keeping `self.is_cancelled() ||` the program hangs and the cap kills it.
#[test]
fn an_exit_cuts_a_spinning_defer_body_of_a_cancelled_sibling() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        r#"import Executor from std.concurrency
import std.time
import std.os
hs := Channel[int](1)
fn cleanup():
    hs.send(1)
    i := 0
    while i < 400000000:
        i = i + 1
    print("DEFER EXIT")
fn sib():
    defer cleanup()
    time.sleep_ms(3000)
    print("sib body done")
fn boom():
    panic("boom")
fn bail():
    hs.recv()
    os.exit(3)
ex := Executor()
ex.submit(bail)
parallel:
    spawn sib()
    spawn boom()
print("after nursery")
"#,
    );
    let (status, out) = run_capped(&entry, 30);
    assert_eq!(status, 3, "the job's exit is the status (out: {out:?})");
    assert!(
        !out.contains("DEFER EXIT"),
        "the exit cut the spinning defer body (out: {out:?})"
    );
    assert!(
        !out.contains("after nursery"),
        "main did not run on (out: {out:?})"
    );
}

/// W7-57 review defect 1, the other half — a spinning party with no cancel flag of its own (a
/// top-level `main`) and one whose only flag is its executor's `cancel` token (an eager job)
/// are killed promptly by an exit. Since TICKET-213 `exit_halt` has no suppression at all: an exit
/// cuts a running `defer` body and an already-cancelled fiber too.
#[test]
fn the_defer_suppression_does_not_rescue_a_spinning_flagless_party() {
    for (name, tail) in [
        (
            "spinning main",
            r#"
import Executor from std.concurrency
ex := Executor()
ex.submit(bail)
i := 0
total := 0
while i < 2000000000:
    total = total + i
    i = i + 1
print("main finished {total}")
"#,
        ),
        (
            "spinning eager job",
            r#"
import Executor from std.concurrency
fn spinjob():
    i := 0
    total := 0
    while i < 2000000000:
        total = total + i
        i = i + 1
    print("sibling job finished {total}")

ex := Executor()
ex.submit(spinjob)
ex.submit(bail)
print("main waiting")
"#,
        ),
    ] {
        let t = TmpDir::new();
        let entry = t.write("main.chz", &format!("{BAIL}{tail}"));
        let (status, out, elapsed) = run_capped_timed(&["run"], &entry, 20);
        assert_eq!(status, 3, "{name}: got {status} (out: {out:?})");
        assert!(
            elapsed < std::time::Duration::from_millis(2000),
            "{name}: must still die promptly, took {elapsed:?} (out: {out:?})"
        );
        assert!(
            !out.contains("finished"),
            "{name}: the spinner must not run to completion (out: {out:?})"
        );
    }
}

/// W7-57 review defect 2 — the `os.exit` cell must reset at EVERY per-invocation entry point, not just
/// free `test fn`s. W7-47 put `clear_exit` in `Vm::invoke_test` alone, so an `os.exit` inside a SUITE
/// method latched for the rest of the file: every later suite method and every lifecycle hook died
/// with `exit`, falsifying `test_runner`'s "after_each always runs, even on failure, like `defer`".
/// W7-57's flag escalated it from "later tests that BLOCK" to anything with a loop or a native HOF.
/// Fixed by moving the reset into `Vm::reset_for_invoke`, shared by `invoke_test`,
/// `invoke_suite_method` and `build_suite_instance`.
///
/// `after_all` asserting `self.n == 2` is what pins the HOOKS: `n` only reaches 2 if both `after_each`
/// invocations ran past their loops, and a hook fault is reported, so silent truncation cannot hide.
#[test]
fn a_suite_test_that_exits_does_not_poison_later_methods_or_hooks() {
    let t = TmpDir::new();
    let entry = t.write(
        "suite_test.chz",
        r#"import std.os

struct A:
    n: int = 0

    test fn a_exits(self):
        os.exit(3)

struct S:
    n: int = 0

    fn before_each(self):
        i := 0
        while i < 300000:
            i = i + 1

    fn after_each(self):
        i := 0
        while i < 300000:
            i = i + 1
        self.n = self.n + 1

    fn after_all(self):
        i := 0
        while i < 300000:
            i = i + 1
        assert self.n == 2

    test fn s_loops(self):
        i := 0
        while i < 300000:
            i = i + 1
        assert i == 300000

    test fn s_plain(self):
        assert 1 == 1
"#,
    );
    let (_status, out, _) = run_capped_timed(&["test"], &entry, 30);
    assert!(
        out.contains("ERROR A::a_exits"),
        "the exiting suite method errors: {out:?}"
    );
    assert!(
        out.contains("PASS S::s_loops"),
        "a LATER suite method that only LOOPS must not inherit the exit: {out:?}"
    );
    assert!(out.contains("PASS S::s_plain"), "nor a plain one: {out:?}");
    // A truncated `after_each`/`after_all` surfaces as a hook error line; `after_all`'s
    // `assert self.n == 2` is what makes truncation observable at all.
    assert!(
        !out.contains("after_all") && !out.contains("after_each"),
        "no lifecycle hook faulted — they ran whole: {out:?}"
    );
}

/// W7-57 review, prosecutor 2 charge 2 -- re-pinned by TICKET-067 (`docs/contributing/gaps.md` W10-12).
///
/// A `test fn` leaks an `Executor` job that `os.exit`s 400 ms later. Since TICKET-067 the A2 join is
/// scoped PER TEST (`Vm::exec_registry_mark` / `Vm::reap_executors_since`), so the leaking test joins
/// the executor IT created before returning: it waits out the 400 ms and takes the `exit` itself. The
/// run then CONTINUES -- `pending_exit` is reset per invocation -- and the status is the runner's
/// ordinary "1 errored", not the leaked 3. Measured: `ERROR a_leaks_a_job (leak_test.chz:1:1) exit`.
///
/// Before TICKET-067 the join ran once per FILE, so the exit landed on whichever LATER test happened
/// to be running (`ERROR b_loops`) -- an arbitrary attribution this test pinned as a documented
/// ceiling. That shape survives only where no test owns the executor, and it moved to
/// `a_leaked_job_from_a_top_level_executor_still_aborts_a_later_test`.
///
/// `b_loops` is a 2 M-iteration loop, not the 60 M it used to be (23.1 s to 1.2 s, measured): it no
/// longer has to be in flight when the exit lands, and is kept only as the "a later test runs
/// untouched" control.
#[test]
fn a_leaked_jobs_exit_is_attributed_to_the_test_that_leaked_it() {
    let t = TmpDir::new();
    let entry = t.write(
        "leak_test.chz",
        r#"import Executor from std.concurrency
import std.time
import std.os

fn bail():
    time.sleep_ms(400)
    os.exit(3)

test fn a_leaks_a_job():
    ex := Executor()
    ex.submit(bail)
    assert 1 == 1

test fn b_loops():
    i := 0
    while i < 2000000:
        i = i + 1
    assert i == 2000000

test fn c_after():
    assert 2 == 2
"#,
    );
    let (status, out) = run_capped_sub("test", &entry, 60);
    assert!(
        out.lines()
            .any(|l| l.starts_with("ERROR a_leaks_a_job (") && l.ends_with(") exit")),
        "the leaking test joins its own executor and takes the exit: {out:?}"
    );
    assert!(
        out.contains("PASS b_loops"),
        "a later test is untouched by it: {out:?}"
    );
    assert!(
        out.contains("PASS c_after"),
        "and the run CONTINUES past it -- the reset is per-invocation: {out:?}"
    );
    assert!(
        out.contains("3 test(s): 2 passed, 0 failed, 1 errored"),
        "{out:?}"
    );
    assert_eq!(
        status, 1,
        "the status is the runner's ordinary failure code, NOT the leaked 3: {out:?}"
    );
}

/// TICKET-067 (`docs/contributing/gaps.md` W10-12) -- the DOCUMENTED CEILING that survives the per-test A2 join.
///
/// `shared_ex` is built at the `_test.chz` MODULE TOP LEVEL, so it sits below every test's registry
/// mark and no test joins it. `a_leaks_a_job` returns at once, and the leaked job's `os.exit(3)`
/// 400 ms later aborts whichever LATER test is running -- here `b_loops`, whose 60 M-iteration loop
/// needs ~22 s in a debug build, so the exit always lands inside it. The run continues and the status
/// is the runner's ordinary 1. Measured: `PASS a_leaks_a_job`, then
/// `ERROR b_loops (toplevel_leak_test.chz:17:11) exit`, then `PASS c_after`, then
/// `3 test(s): 2 passed, 0 failed, 1 errored`, rc=1, whole file 0.44 s.
///
/// The attribution is arbitrary (`b_loops` did nothing wrong) and is kept rather than re-plumbed: a
/// run-level halt verdict is a `test_runner` redesign (a new verdict kind, an rc policy, a
/// `--fail-fast` interaction) that belongs in its own change.
///
/// It is also the end-to-end fence for the mark: drain from registry index 0 instead and this goes
/// red, because `a_leaks_a_job` would join `shared_ex` and take the `exit` itself while `b_loops`
/// passed. `test_runner::tests::an_executor_created_before_the_first_test_is_not_joined_by_it` pins
/// the other half -- a shared executor must still accept a `submit` from a later test.
#[test]
fn a_leaked_job_from_a_top_level_executor_still_aborts_a_later_test() {
    let t = TmpDir::new();
    let entry = t.write(
        "toplevel_leak_test.chz",
        r#"import Executor from std.concurrency
import std.time
import std.os

shared_ex := Executor()

fn bail():
    time.sleep_ms(400)
    os.exit(3)

test fn a_leaks_a_job():
    shared_ex.submit(bail)
    assert 1 == 1

test fn b_loops():
    i := 0
    while i < 60000000:
        i = i + 1
    assert i == 60000000

test fn c_after():
    assert 2 == 2
"#,
    );
    let (status, out) = run_capped_sub("test", &entry, 60);
    assert!(
        out.contains("PASS a_leaks_a_job"),
        "the test does not own the executor, so it does not join it: {out:?}"
    );
    assert!(
        out.lines()
            .any(|l| l.starts_with("ERROR b_loops (") && l.ends_with(") exit")),
        "the in-flight test absorbs the leaked exit: {out:?}"
    );
    assert!(
        out.contains("PASS c_after"),
        "and the run CONTINUES past it -- the reset is per-invocation: {out:?}"
    );
    assert_eq!(
        status, 1,
        "the status is the runner's ordinary failure code, NOT the leaked 3: {out:?}"
    );
}

// ===== `chezzi test --timeout` must reach `Vm::guarded` too, not only `jump_checked` =====
//
// Not an `os.exit` story, but the same seam and found while closing it: W7-57 added an exit rung to
// `guarded` (the per-element re-entry of `map`/`filter`/`fold`/`sort_by`) and left the DEADLINE rung
// only in `jump_checked`. Measured on the release binary before the fix:
//
//   --timeout=500, plain `while` loop (jump_checked) → TIMED-OUT at  505 ms   ok
//   --timeout=500, the same work as a native HOF     → PASS      at 1985 ms   wrong
//
// A resource cap that reports PASS on a test which blew through it by 4× is worse than one that is
// merely late: it teaches distrust of every green run. Same rung order as `jump_checked` — deadline
// above cancel above exit — and, like `jump_checked`'s deadline rung and unlike the two below it, NOT
// suppressed inside a `defer`: a cap a `defer` can outrun is the same hole one level down.

/// Both halves in ONE file so the plain-loop test is a live control: if the harness or the cap itself
/// broke, `loop_spins` would stop reporting `TIMED-OUT` too and the HOF result would mean nothing.
#[test]
fn timeout_reaches_a_native_hof_not_just_a_loop() {
    let t = TmpDir::new();
    let entry = t.write(
        "cap_test.chz",
        r#"fn double(x: int) -> int:
    return x * 2

fn add(a: int, b: int) -> int:
    return a + b

test fn loop_spins():
    i := 0
    total := 0
    while i < 2000000000:
        total = total + i
        i = i + 1
    assert total > 0

test fn hof_spins():
    total := List.from_iter(0..10000000).map(double).fold(0, add)
    assert total > 0
"#,
    );
    let (_status, out, elapsed) = run_capped_timed(&["test", "--timeout=500"], &entry, 60);
    assert!(
        out.contains("TIMED-OUT loop_spins"),
        "CONTROL — the loop back-edge rung still fires: {out:?}"
    );
    assert!(
        out.contains("TIMED-OUT hof_spins"),
        "the native-HOF re-entry must honour the cap too — it reported PASS at 1985 ms before the \
         rung was added: {out:?}"
    );
    assert!(
        !out.contains("passed") || out.contains("0 passed"),
        "neither test may PASS: {out:?}"
    );
    // Two tests, each capped at 500 ms, plus process start — 1 s of slack over the 1 s of budget.
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "both caps fired promptly, took {elapsed:?}: {out:?}"
    );
}

// ===== TICKET-096: the module top-level nursery treats `main` differently from a `parallel:`
// sibling =====

/// Defect A — a nursery OWNER (the module top level) is now a cancel participant in its own scope:
/// a `recover:` installed INSIDE the faulting nursery's body does not catch the cancel, and the
/// program aborts exactly once at rc=1 (`docs/reference/spec/`, `docs/history/designs/concurrency-b3.md:235`).
#[test]
fn module_top_level_fault_is_delivered_twice_and_truncates_after_recover() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        r#"import std.time
spawn:
    panic("boom")
print("before recover")
r := recover:
    time.sleep_ms(50)
    1
print("r={r}")
time.sleep_ms(50)
print("THIS LINE NEVER PRINTS")
"#,
    );
    let (status, out) = run_capped(&entry, 20);
    assert!(
        out.contains("before recover"),
        "main ran before the fault: {out:?}"
    );
    assert!(
        !out.contains("r=!'boom'"),
        "BUG: the recover: caught the module nursery cancel: {out:?}"
    );
    assert!(
        !out.contains("THIS LINE NEVER PRINTS"),
        "the abort must stop the program: {out:?}"
    );
    assert_eq!(status, 1, "the fault must abort exactly once: {out:?}");
}

/// Defect B — the top-level `main` fiber's `while` loop back-edge is not a cancel checkpoint, so a
/// sibling task's fault does not cancel a doomed CPU loop promptly. `docs/reference/concurrency/`
/// claims "the loop back-edge is the checkpoint" on the top-level `main` thread too, and the
/// identical loop inside a `parallel:` sibling IS cancelled promptly (measured ~33 ms in the
/// ticket). Here the loop is sized so an UNCANCELLED run takes several seconds; a correct run
/// must finish in well under a second.
#[test]
fn module_top_level_loop_back_edge_is_not_a_cancel_checkpoint() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        r#"import std.time
spawn:
    time.sleep_ms(20)
    panic("boom")
i := 0
while i < 20000000:
    i = i + 1
print("main loop finished i={i}")
"#,
    );
    let (_status, out, elapsed) = run_capped_timed(&["run"], &entry, 30);
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "BUG: the top-level loop is not cancelled at its back-edge, ran to completion after \
         {elapsed:?} instead of aborting promptly on the sibling's fault: {out:?}"
    );
    assert!(
        !out.contains("main loop finished"),
        "BUG: the top-level loop ran to completion: {out:?}"
    );
}

/// FALSE-HALT FENCE for the rung above — the direction that matters. A HOF-heavy test that finishes
/// UNDER the cap must still `PASS` with its result intact (no truncated `map`/`fold`), and the same
/// file with NO `--timeout` at all — the common case, and every `chezzi run` — must be unaffected,
/// which is also what pins that the clock is never read when the cap is off (`deadline.is_some()` is
/// checked before the tick).
#[test]
fn a_hof_under_the_cap_is_untouched_and_no_cap_is_untouched() {
    let t = TmpDir::new();
    let entry = t.write(
        "under_test.chz",
        r#"fn double(x: int) -> int:
    return x * 2

fn add(a: int, b: int) -> int:
    return a + b

test fn hof_completes():
    xs := [1, 2, 3, 4, 5]
    doubled := xs.map(double)
    assert doubled == [2, 4, 6, 8, 10]
    assert xs.fold(0, add) == 15
    assert List.from_iter(0..1000).map(double).fold(0, add) == 999000
"#,
    );
    for args in [
        vec!["test", "--timeout=30000"],
        vec!["test"], // no cap at all
    ] {
        let label = args.join(" ");
        let (status, out, _) = run_capped_timed(&args, &entry, 60);
        assert!(
            out.contains("PASS hof_completes"),
            "{label}: a HOF under the cap runs whole and passes: {out:?}"
        );
        assert!(
            !out.contains("TIMED-OUT"),
            "{label}: nothing timed out: {out:?}"
        );
        assert_eq!(status, 0, "{label}: clean run (out: {out:?})");
    }
}

/// TICKET-063 — a `Shared.update` holder blocked on a `recv` nothing feeds, and a sibling waiting
/// for its guard: two tasks blocked in place, cut by one deadlock verdict.
const GUARD_WAITER: &str = r#"import Shared from std.concurrency

fn take(ch: Channel[int]) -> int:
    v := ch.recv() else: panic("ch closed")
    return v

test fn guard_waiter_hangs():
    s := Shared(0)
    ch := Channel[int]()
    parallel:
        spawn:
            s.update(fn(x: int) -> int: x + take(ch))
        spawn:
            s.set(5)
"#;

// ===== TICKET-063: a `Shared.update` guard waiter parked behind a sibling's blocking `recv` now
// faults `deadlock` well inside `--timeout`, instead of hanging past it (W10-15) =====
//
// A guard wait used to be accounted `inflight`, which vetoes the process-wide deadlock verdict
// unconditionally, so this run used to hang past its own `--timeout=500` and rely on
// `run_capped_timed`'s external wall-clock kill (its "hung for >{secs}s" panic WAS the
// reproduction). The fix registers the wait as a `Waiter` instead, so the deadlock verdict now
// fires on its own well inside the cap: the runner self-terminates reporting `ERROR`, not
// `TIMED-OUT` at the 500ms cap.
#[test]
fn timeout_reaches_a_guard_waiter_parked_behind_a_recv() {
    let t = TmpDir::new();
    let entry = t.write("guard_waiter_test.chz", GUARD_WAITER);
    // 10s wall-clock bound on the CHILD process: well over the 500ms cap, so a correct runner
    // finishes in well under a second. If the runner cannot self-terminate, `run_capped_timed`
    // panics with "hung for >10s" instead of returning — that panic message IS the reproduction.
    let (status, out, elapsed) = run_capped_timed(&["test", "--timeout=500"], &entry, 10);
    assert_eq!(status, 1, "an errored test suite exits 1: {out:?}");
    assert!(
        out.contains("ERROR guard_waiter_hangs"),
        "expected the deadlock fault to be reported as an ERROR, not TIMED-OUT: {out:?}"
    );
    assert!(
        out.contains("deadlock:"),
        "expected a deadlock fault: {out:?}"
    );
    assert!(
        out.contains("1 test(s): 0 passed, 0 failed, 1 errored"),
        "expected the suite summary to record one errored test: {out:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "the deadlock verdict must fire well inside the 500ms cap, took {elapsed:?}: {out:?}"
    );
}

// ===== One deadlock verdict that lists every blocked task, the same every run =====
//
// Go's report: one header, then every blocked task with its own wait and its frames, in task order.
// Before, the first cut task to wake faulted, cancelled its sibling, and alone was reported, with
// the nursery's text (`deadlock: every task in this parallel: block …`) or the guard waiter's own
// (`update guard wait: deadlock …`) depending on which woke first. `held` makes the holder take the
// guard before its sibling asks for it, so the program has one schedule: without it the sibling
// may `set` before the holder's `update` and finish, which is a different, correct run (Go's
// listing changes with it too). Many concurrent runs per worker count (the load moves the wake
// order) must print byte-identical output. The seeded scheduler does not reorder these wakes (both
// waits are in place, on their own threads), so the runs are concurrent processes, not seeds.
const GUARD_WAITER_HELD: &str = r#"import Shared from std.concurrency

fn take(ch: Channel[int], held: Channel[int]) -> int:
    held.send(1)
    v := ch.recv() else: panic("ch closed")
    return v

test fn guard_waiter_hangs():
    s := Shared(0)
    ch := Channel[int]()
    held := Channel[int](1)
    parallel:
        spawn:
            s.update(fn(x: int) -> int: x + take(ch, held))
        spawn:
            _ := held.recv() else: panic("held closed")
            s.set(5)
"#;

/// `chezzi test` on `entry` at `threads` workers, `n` concurrent runs: each run's stdout, with the
/// temp dir spelled `<dir>`.
fn concurrent_test_runs(
    entry: &std::path::Path,
    args: &[&str],
    threads: &str,
    n: usize,
) -> Vec<String> {
    let dir = entry.parent().unwrap().display().to_string();
    std::thread::scope(|s| {
        let runs: Vec<_> = (0..n)
            .map(|_| {
                s.spawn(|| {
                    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
                        .arg("test")
                        .args(args)
                        .arg(entry)
                        .env("CHEZZI_THREADS", threads)
                        .output()
                        .expect("spawn chezzi");
                    String::from_utf8_lossy(&out.stdout).replace(&dir, "<dir>")
                })
            })
            .collect();
        runs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

#[test]
fn a_guard_waiter_deadlock_lists_both_tasks_the_same_every_run() {
    let t = TmpDir::new();
    let entry = t.write("guard_waiter_test.chz", GUARD_WAITER_HELD);
    let want = "ERROR guard_waiter_hangs (<dir>/guard_waiter_test.chz:5:10) deadlock: 2 tasks are \
                blocked and none can proceed\n  \
                task 1: recv() on an empty channel at <dir>/guard_waiter_test.chz:5:10\n    \
                at take (called at <dir>/guard_waiter_test.chz:14:45)\n    \
                at <closure> (called at <dir>/guard_waiter_test.chz:14:13)\n    \
                at <spawned task> (called at <dir>/guard_waiter_test.chz:13:9)\n  \
                task 2: waiting for the Shared guard at <dir>/guard_waiter_test.chz:17:13\n    \
                at <spawned task> (called at <dir>/guard_waiter_test.chz:15:9)\n\n\
                1 test(s): 0 passed, 0 failed, 1 errored\n";
    for threads in ["1", "2", "4"] {
        for (i, out) in concurrent_test_runs(&entry, &["--timeout=500"], threads, 48)
            .iter()
            .enumerate()
        {
            assert_eq!(out, want, "threads={threads}: run {i}");
        }
    }
}

// A `parallel:` body waits for the guard that its spawned task holds while that task blocks in
// `recv()` inside `update`. The body's cut unwinds past the nursery it opened, and that abort's
// blocked task joins the listing ahead of the body's own wait (a nursery's tasks in place of the
// task that opened it). Before, the body alone was listed (`1 task is blocked`): the abort's
// reduce took the latched verdict in place of its slot, the fatal unwind dropped what the abort
// returned, and a holder that read the abort's cancel before its own cut ended cancelled.
// `held` orders the two waits, so the program has one schedule. The body runs at top level
// (`chezzi run`) and in a `test fn`, concurrently at 1, 2 and 4 workers.
const GUARD_HOLDER_BODY: &str = r#"s := Shared(0)
ch := Channel[int]()
held := Channel[int](1)
parallel:
    spawn:
        s.update(fn(x: int) -> int: x + take(ch, held))
    _ := held.recv() else: panic("held closed")
    s.set(5)
"#;

#[test]
fn a_body_waiting_for_a_blocked_guard_holder_lists_the_holder_too() {
    let head = GUARD_WAITER_HELD.split("test fn").next().unwrap();
    let t = TmpDir::new();
    let dir = t.0.display().to_string();
    let entry = t.write("holder.chz", &format!("{head}{GUARD_HOLDER_BODY}"));
    let want = "runtime error (<dir>/holder.chz:5:10): deadlock: 2 tasks are blocked and none can \
                proceed\n  \
                task 1: recv() on an empty channel at <dir>/holder.chz:5:10\n    \
                at take (called at <dir>/holder.chz:13:41)\n    \
                at <closure> (called at <dir>/holder.chz:13:9)\n    \
                at <spawned task> (called at <dir>/holder.chz:12:5)\n  \
                task 2: waiting for the Shared guard at <dir>/holder.chz:15:5\n";
    for threads in ["1", "2", "4"] {
        let runs: Vec<_> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..24)
                .map(|_| s.spawn(|| hang_deadline::run_with_hang_deadline(&entry, Some(threads))))
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (i, run) in runs.into_iter().enumerate() {
            let out = run.unwrap_or_else(|| panic!("threads={threads}: run {i} hung"));
            let err = String::from_utf8_lossy(&out.stderr).replace(&dir, "<dir>");
            assert_eq!(
                (out.status.code(), err.as_str()),
                (Some(1), want),
                "run threads={threads}: run {i}"
            );
        }
    }
    let body: String = GUARD_HOLDER_BODY
        .lines()
        .map(|l| format!("    {l}\n"))
        .collect();
    let entry = t.write(
        "holder_test.chz",
        &format!("{head}test fn holder():\n{body}"),
    );
    let want = "ERROR holder (<dir>/holder_test.chz:5:10) deadlock: 2 tasks are blocked and none \
                can proceed\n  \
                task 1: recv() on an empty channel at <dir>/holder_test.chz:5:10\n    \
                at take (called at <dir>/holder_test.chz:14:45)\n    \
                at <closure> (called at <dir>/holder_test.chz:14:13)\n    \
                at <spawned task> (called at <dir>/holder_test.chz:13:9)\n  \
                task 2: waiting for the Shared guard at <dir>/holder_test.chz:16:9\n\n\
                1 test(s): 0 passed, 0 failed, 1 errored\n";
    for threads in ["1", "2", "4"] {
        for (i, out) in concurrent_test_runs(&entry, &["--timeout=2000"], threads, 24)
            .iter()
            .enumerate()
        {
            assert_eq!(out, want, "test threads={threads}: run {i}");
        }
    }
}

// The mirror of the shape above: the body holds the guard and blocks in `recv()` inside
// `update`, and its spawned task waits for the guard. The body blocks in place inside a native
// callback, so no process-wide verdict judges it; the nursery's verdict counts it blocked and
// cuts it with the waiter. Before, the waiter's cut cancelled no sibling and the body, which reads
// no verdict of its own, waited forever (a hang at every worker count).
const GUARD_MIRROR_BODY: &str = r#"s := Shared(0)
ch := Channel[int]()
held := Channel[int](1)
parallel:
    spawn:
        _ := held.recv() else: panic("held closed")
        s.set(5)
    s.update(fn(x: int) -> int: x + take(ch, held))
"#;

#[test]
fn a_guard_holder_body_blocked_in_update_lists_its_waiter_too() {
    let head = GUARD_WAITER_HELD.split("test fn").next().unwrap();
    let t = TmpDir::new();
    let dir = t.0.display().to_string();
    let entry = t.write("mirror.chz", &format!("{head}{GUARD_MIRROR_BODY}"));
    let want = "runtime error (<dir>/mirror.chz:14:9): deadlock: 2 tasks are blocked and none can \
                proceed\n  \
                task 1: waiting for the Shared guard at <dir>/mirror.chz:14:9\n    \
                at <spawned task> (called at <dir>/mirror.chz:12:5)\n  \
                task 2: recv() on an empty channel at <dir>/mirror.chz:5:10\n    \
                at take (called at <dir>/mirror.chz:15:37)\n    \
                at <closure> (called at <dir>/mirror.chz:15:5)\n";
    for threads in ["1", "2", "4"] {
        let runs: Vec<_> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..24)
                .map(|_| s.spawn(|| hang_deadline::run_with_hang_deadline(&entry, Some(threads))))
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        for (i, run) in runs.into_iter().enumerate() {
            let out = run.unwrap_or_else(|| panic!("threads={threads}: run {i} hung"));
            let err = String::from_utf8_lossy(&out.stderr).replace(&dir, "<dir>");
            assert_eq!(
                (out.status.code(), err.as_str()),
                (Some(1), want),
                "run threads={threads}: run {i}"
            );
        }
    }
    let body: String = GUARD_MIRROR_BODY
        .lines()
        .map(|l| format!("    {l}\n"))
        .collect();
    let entry = t.write(
        "mirror_test.chz",
        &format!("{head}test fn mirror():\n{body}"),
    );
    let want = "ERROR mirror (<dir>/mirror_test.chz:15:13) deadlock: 2 tasks are blocked and none \
                can proceed\n  \
                task 1: waiting for the Shared guard at <dir>/mirror_test.chz:15:13\n    \
                at <spawned task> (called at <dir>/mirror_test.chz:13:9)\n  \
                task 2: recv() on an empty channel at <dir>/mirror_test.chz:5:10\n    \
                at take (called at <dir>/mirror_test.chz:16:41)\n    \
                at <closure> (called at <dir>/mirror_test.chz:16:9)\n\n\
                1 test(s): 0 passed, 0 failed, 1 errored\n";
    for threads in ["1", "2", "4"] {
        for (i, out) in concurrent_test_runs(&entry, &["--timeout=2000"], threads, 24)
            .iter()
            .enumerate()
        {
            assert_eq!(out, want, "test threads={threads}: run {i}");
        }
    }
}

// A deadlock across two nurseries: an outer task waits on `a`, and the inner nursery of its
// sibling holds a sender on a full channel and a second receiver on `a`. Every blocked task of
// both nurseries is listed, in spawn-tree order (the outer task, then the inner nursery's tasks).
#[test]
fn a_deadlock_across_two_nurseries_lists_the_tasks_of_both() {
    let t = TmpDir::new();
    let entry = t.write(
        "cross_test.chz",
        r#"test fn cross_nursery():
    a := Channel[int]()
    b := Channel[int](1)
    parallel:
        spawn:
            _ := a.recv() else: panic("a closed")
        spawn:
            parallel:
                spawn:
                    b.send(1)
                    b.send(2)
                spawn:
                    _ := a.recv() else: panic("a closed")
"#,
    );
    let want = "ERROR cross_nursery (<dir>/cross_test.chz:6:18) deadlock: 3 tasks are blocked and \
                none can proceed\n  \
                task 1: recv() on an empty channel at <dir>/cross_test.chz:6:18\n    \
                at <spawned task> (called at <dir>/cross_test.chz:5:9)\n  \
                task 2: send() on a full channel at <dir>/cross_test.chz:11:21\n    \
                at <spawned task> (called at <dir>/cross_test.chz:9:17)\n  \
                task 3: recv() on an empty channel at <dir>/cross_test.chz:13:26\n    \
                at <spawned task> (called at <dir>/cross_test.chz:12:17)\n\n\
                1 test(s): 0 passed, 0 failed, 1 errored\n";
    for threads in ["1", "2", "4"] {
        for (i, out) in concurrent_test_runs(&entry, &["--timeout=2000"], threads, 16)
            .iter()
            .enumerate()
        {
            assert_eq!(out, want, "threads={threads}: run {i}");
        }
    }
}

// The JSON shape of the listing: the test's `message` is the header, and `tasks` lists each blocked
// task's `wait`, its site and its `frames`.
#[test]
fn a_deadlock_listing_is_a_tasks_array_in_errors_json() {
    let t = TmpDir::new();
    let entry = t.write("guard_waiter_test.chz", GUARD_WAITER_HELD);
    let out = &concurrent_test_runs(&entry, &["--errors=json", "--timeout=500"], "2", 1)[0];
    let out = mask_durations(out);
    let f = "\"file\":\"<dir>/guard_waiter_test.chz\"";
    let want = format!(
        "{{\"tests\":[{{\"name\":\"guard_waiter_hangs\",{f},\"line\":5,\"status\":\"error\",\
         \"message\":\"deadlock: 2 tasks are blocked and none can proceed\",\"tasks\":[\
         {{\"wait\":\"recv() on an empty channel\",{f},\"line\":5,\"col\":10,\"frames\":[\
         {{\"function\":\"take\",{f},\"line\":14,\"col\":45}},\
         {{\"function\":\"<closure>\",{f},\"line\":14,\"col\":13}},\
         {{\"function\":\"<spawned task>\",{f},\"line\":13,\"col\":9}}]}},\
         {{\"wait\":\"waiting for the Shared guard\",{f},\"line\":17,\"col\":13,\"frames\":[\
         {{\"function\":\"<spawned task>\",{f},\"line\":15,\"col\":9}}]}}],\
         \"duration_ms\":N}}],\"file_errors\":[],\"totals\":{{\"total\":1,\"passed\":0,\
         \"failed\":0,\"errored\":1,\"over_memory\":0,\"timed_out\":0,\"filtered_out\":0,\
         \"file_errors\":0}}}}\n"
    );
    assert_eq!(out, want);
}

/// `out` with every `"duration_ms":<digits>` spelled `"duration_ms":N`.
fn mask_durations(out: &str) -> String {
    let key = "\"duration_ms\":";
    let mut s = String::new();
    let mut rest = out;
    while let Some(i) = rest.find(key) {
        s.push_str(&rest[..i + key.len()]);
        s.push('N');
        rest = rest[i + key.len()..].trim_start_matches(|c: char| c.is_ascii_digit());
    }
    s.push_str(rest);
    s
}

// A deadlock whose blocked tasks sit in two Executors and a nursery: the reap drains every
// Executor, so each one's blocked job is listed, in Executor creation order after the test's own
// task. Before, the drain stopped at the first Executor's deadlock and the second one's job was
// reported apart, as an `(executor job)` error.
#[test]
fn a_deadlock_lists_the_blocked_jobs_of_every_executor() {
    let t = TmpDir::new();
    let entry = t.write(
        "exec_test.chz",
        r#"import std.concurrency

fn w(c: Channel[int]):
    _ := c.recv() else: panic("closed")

test fn two_executors():
    a := Channel[int]()
    ex1 := concurrency.Executor()
    _ := ex1.submit(fn(): w(a))
    ex2 := concurrency.Executor()
    _ := ex2.submit(fn(): w(a))
    parallel:
        spawn w(a)
"#,
    );
    let job = |line: u32| {
        format!(
            "  task {}: recv() on an empty channel at <dir>/exec_test.chz:4:10\n    \
             at w (called at <dir>/exec_test.chz:{line}:27)\n    \
             at <closure> (called at <native:std.concurrency>:127:19)\n    \
             at _run_outcome (called at <native:std.concurrency>:106:36)\n    \
             at <closure> (called at <native:std.concurrency>:106:9)\n",
            if line == 9 { 2 } else { 3 }
        )
    };
    let want = format!(
        "ERROR two_executors (<dir>/exec_test.chz:4:10) deadlock: 3 tasks are blocked and none \
         can proceed\n  \
         task 1: recv() on an empty channel at <dir>/exec_test.chz:4:10\n    \
         at w (called at <dir>/exec_test.chz:13:15)\n{}{}\n\
         1 test(s): 0 passed, 0 failed, 1 errored\n",
        job(9),
        job(11)
    );
    for threads in ["1", "2", "4"] {
        for (i, out) in concurrent_test_runs(&entry, &["--timeout=2000"], threads, 16)
            .iter()
            .enumerate()
        {
            assert_eq!(out, &want, "threads={threads}: run {i}");
        }
    }
}

// A deadlock that ends in two Executors, one of them joined by a nursery task the verdict cut:
// `joiner` blocks in `ex.shutdown()` on its own Executor's stuck job, its sibling and `main`'s
// Executor job wait on `b`. After the nursery's verdict, `main`'s exit drain joins its own
// Executor first. Before, its idle runner declined the standing verdict for as long as the cut
// joiner's Executor sat finished and unjoined, which the drain reaches only after: a hang in some
// runs at two or more workers. Each run must report the same four tasks.
#[test]
fn a_deadlock_with_an_unjoined_finished_executor_reports_every_run() {
    let t = TmpDir::new();
    let entry = t.write(
        "d7.chz",
        r#"import std.concurrency

fn w(c: Channel[int]):
    _ := c.recv() else: panic("closed")

fn joiner(a: Channel[int]):
    ex := concurrency.Executor()
    _ := ex.submit(fn(): w(a))
    ex.shutdown()

fn main():
    a := Channel[int]()
    b := Channel[int]()
    ex := concurrency.Executor()
    _ := ex.submit(fn(): w(b))
    parallel:
        spawn joiner(a)
        spawn w(b)
    print("done")

main()
"#,
    );
    let dir = t.0.display().to_string();
    let job = |k: u32, line: u32| {
        format!(
            "  task {k}: recv() on an empty channel at <dir>/d7.chz:4:10\n    \
             at w (called at <dir>/d7.chz:{line}:26)\n    \
             at <closure> (called at <native:std.concurrency>:127:19)\n    \
             at _run_outcome (called at <native:std.concurrency>:106:36)\n    \
             at <closure> (called at <native:std.concurrency>:106:9)\n"
        )
    };
    let want = format!(
        "runtime error (<dir>/d7.chz:9:5): deadlock: 4 tasks are blocked and none can proceed\n  \
         task 1: waiting for an Executor join at <dir>/d7.chz:9:5\n    \
         at joiner (called at <dir>/d7.chz:17:15)\n  \
         task 2: recv() on an empty channel at <dir>/d7.chz:4:10\n    \
         at w (called at <dir>/d7.chz:18:15)\n{}{}",
        job(3, 15),
        job(4, 8)
    );
    for threads in ["2", "4"] {
        for batch in 0..4 {
            let runs: Vec<_> = std::thread::scope(|s| {
                let hs: Vec<_> = (0..25)
                    .map(|_| {
                        s.spawn(|| hang_deadline::run_with_hang_deadline(&entry, Some(threads)))
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            });
            for (i, run) in runs.into_iter().enumerate() {
                let out =
                    run.unwrap_or_else(|| panic!("threads={threads}: run {} hung", batch * 25 + i));
                let err = String::from_utf8_lossy(&out.stderr).replace(&dir, "<dir>");
                assert_eq!(
                    (out.status.code(), err.as_str()),
                    (Some(1), want.as_str()),
                    "threads={threads}: run {}",
                    batch * 25 + i
                );
            }
        }
    }
}
