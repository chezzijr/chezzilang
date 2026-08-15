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
// **M:N only, deliberately.** The serial engine does not dispatch an eager `Executor` job at `submit`
// — it runs jobs at the exit drain — so on that engine the exiting party does not exist while the
// nursery runs. These programs are outside the two-engine parity contract by construction; the serial
// engine is unchanged by this fix (verified by hand on the release binary).
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
            r#"{BAIL}
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
            r#"{BAIL}
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
            r#"{BAIL}
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
            r#"{BAIL}
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
/// `shutdown_now` token, which an `os.exit` must NOT trip, so the back-edge rung is deliberately not
/// gated on "this party has no cancel flag": the job HAS one, it is just the wrong one. (This shape
/// already reported rc=3 promptly pre-fix — `main`'s join observed the exit and returned while the
/// spinner ran on — so it is a FENCE against the teardown reclassifying or delaying it.)
#[test]
fn eager_job_os_exit_kills_a_cpu_spinning_sibling_job() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        &format!(
            r#"{BAIL}
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
            r#"{BAIL}
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
            r#"{BAIL}
fn double(x: int) -> int:
    return x * 2 + 1

fn keep(a: int, b: int) -> int:
    return a

ex := Executor()
ex.submit(bail)
n := range(0, 10000000).map(double).fold(0, keep)
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

/// W7-57 review defect 1 — **a cancelled sibling's `defer` runs to COMPLETION**, and the two engines
/// agree. This is the test `quiesce.rs` used to cite `exit_in_spawned_child_aborts_siblings` for; that
/// program contains no `defer` at all, so the citation pinned nothing.
///
/// The first cut of the W7-57 rungs broke this two ways at once: M:N stopped running the defer while
/// the serial engine still did (an engine divergence, and the code asserted the opposite of what it did), and
/// worse, a defer that had already STARTED was killed part-way — measured at 2/8 and 6/6 depending on
/// timing. A half-executed cleanup is worse than either running or skipping it: inconsistent state,
/// nondeterministically. Fixed by suppressing the exit rung inside a `defer` (`run_exit_err`) and by
/// routing a fiber that HOLDS a cancel flag down the `Cancelled` path (`Vm::exit_halt`).
///
/// The defer body here is deliberately long AND crosses a native HOF — the two checkpoints W7-57 added
/// — so a partial run would show up as `DEFER ENTER` without `DEFER EXIT`.
#[test]
fn a_cancelled_siblings_defer_runs_whole_on_both_engines() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        &format!(
            r#"{BAIL}
fn double(x: int) -> int:
    return x * 2

fn keep(a: int, b: int) -> int:
    return a

fn cleanup():
    print("DEFER ENTER")
    i := 0
    while i < 3000000:
        i = i + 1
    n := range(0, 1000000).map(double).fold(0, keep)
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
        out.contains("DEFER ENTER"),
        "the sibling's defer ran (out: {out:?})"
    );
    assert!(
        out.contains("DEFER EXIT 3000000 0"),
        "…and ran to COMPLETION, never truncated mid-body (out: {out:?})"
    );
}

/// W7-57 review defect 1, the other half — the shapes the `defer` suppression must NOT rescue. A
/// party with no cancel flag of its own (a top-level `main`) and one whose only flag is its executor's
/// `shutdown_now` token (an eager job) are still killed promptly, because `exit_halt`'s suppression is
/// `deferring`/`cancelled`, never "this party is flagless".
#[test]
fn the_defer_suppression_does_not_rescue_a_spinning_flagless_party() {
    for (name, tail) in [
        (
            "spinning main",
            r#"
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

/// W7-57 review, prosecutor 2 charge 2 — DOCUMENTED CEILING, pinned so it cannot drift silently.
///
/// A `test fn` leaks an `Executor` job that `os.exit`s 400 ms later, i.e. during a LATER test. What
/// happens today: the test that happens to be running when the exit lands is aborted with `exit`, the
/// run then CONTINUES, and the process status is the ordinary "1 errored" — not 3.
///
/// That attribution is admittedly arbitrary (`b_loops` did nothing wrong). It is kept rather than
/// re-plumbed because the alternative — a run-level halt verdict — is a `test_runner` redesign (a new
/// verdict kind, an rc policy, `--fail-fast` interaction) that belongs in its own change, and because
/// what is here is strictly better than what it replaced: pre-W7-57 the leaked exit was invisible and
/// the file reported `2 passed`, rc=0. This test pins the real behaviour by name so that "one
/// arbitrary test errors" is a decision on record, not an accident.
#[test]
fn a_leaked_jobs_exit_aborts_whichever_test_is_running_and_the_run_continues() {
    let t = TmpDir::new();
    let entry = t.write(
        "leak_test.chz",
        r#"import std.concurrency
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
    while i < 60000000:
        i = i + 1
    assert i == 60000000

test fn c_after():
    assert 2 == 2
"#,
    );
    let (status, out) = run_capped_sub("test", &entry, 60);
    assert!(out.contains("PASS a_leaks_a_job"), "{out:?}");
    assert!(
        out.contains("ERROR b_loops"),
        "the in-flight test absorbs the leaked exit: {out:?}"
    );
    assert!(
        out.contains("PASS c_after"),
        "and the run CONTINUES past it — the reset is per-invocation: {out:?}"
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
    total := range(0, 10000000).map(double).fold(0, add)
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
    assert range(0, 1000).map(double).fold(0, add) == 999000
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
