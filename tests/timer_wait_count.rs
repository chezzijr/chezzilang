//! A `timer(ms)` is a task of the run's awake count (`quiesce::Awake`) only while a task is parked
//! on its channel and it has not fired, Go 1.23's rule: a timer nobody waits on keeps nothing
//! awake. A `wait:` that another arm wins leaves its timer pending with no waiter, so a later
//! deadlock is reported at once (Go: `fatal error: all goroutines are asleep - deadlock!`), and a
//! task parked on a pending timer is never part of a deadlock.
//!
//! Each cell runs at `CHEZZI_THREADS` 1, 2 and 4, unseeded and at `CHEZZI_SCHED_SEED` 1..3, and
//! must end within 2 s with its golden stdout, exit code and `deadlocks=<k>` count.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// One finished run: exit code, stdout, stderr.
struct Ran {
    code: i32,
    out: String,
    err: String,
}

impl Ran {
    /// `deadlocks=<k>` from the `[sched-stats]` line (`None` if missing).
    fn deadlocks(&self) -> Option<usize> {
        self.err
            .lines()
            .filter_map(|l| l.strip_prefix("[sched-stats] "))
            .flat_map(|l| l.split(' '))
            .find_map(|f| f.strip_prefix("deadlocks="))
            .and_then(|n| n.parse().ok())
    }
}

/// Run `src` (`chezzi test` when `test`, with `args` before the path) at `threads` and `seed`.
/// `None`: still running after 2 s (killed).
fn run(src: &str, test: bool, args: &[&str], threads: &str, seed: Option<u32>) -> Option<Ran> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("chz-timer-wait-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(if test { "cell_test.chz" } else { "cell.chz" });
    std::fs::write(&path, src).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg(if test { "test" } else { "run" })
        .args(args)
        .arg(&path)
        .env("CHEZZI_THREADS", threads)
        .env("CHEZZI_SCHED_STATS", "1")
        .env_remove("CHEZZI_SCHED_SEED")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(s) = seed {
        cmd.env("CHEZZI_SCHED_SEED", s.to_string());
    }
    let mut child = cmd.spawn().expect("spawn chezzi");
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s);
        }
        if start.elapsed() > Duration::from_secs(2) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut out = String::new();
    let mut err = String::new();
    let _ = child.stdout.take().unwrap().read_to_string(&mut out);
    let _ = child.stderr.take().unwrap().read_to_string(&mut err);
    let _ = std::fs::remove_dir_all(&dir);
    status.map(|s| Ran {
        code: s.code().unwrap_or(-1),
        out,
        err,
    })
}

/// Run one cell everywhere: `check` judges each run, `deadlocks` is the verdict count it expects.
fn cell(src: &str, test: bool, args: &[&str], deadlocks: usize, check: impl Fn(&Ran) -> bool) {
    let mut misses = Vec::new();
    for threads in ["1", "2", "4"] {
        for seed in [None, Some(1), Some(2), Some(3)] {
            let at = format!("T={threads} seed={seed:?}");
            let Some(r) = run(src, test, args, threads, seed) else {
                misses.push(format!("{at}: killed after 2 s"));
                continue;
            };
            if !check(&r) || r.deadlocks() != Some(deadlocks) {
                misses.push(format!(
                    "{at}: rc={} deadlocks={:?} (expected {deadlocks})\n--stdout--\n{}--stderr--\n{}",
                    r.code,
                    r.deadlocks(),
                    r.out,
                    r.err
                ));
            }
        }
    }
    assert!(misses.is_empty(), "{}", misses.join("\n\n"));
}

/// A `wait:` arm wins over a 10-minute timer; then main blocks on a channel nobody sends to.
const LOST_TIMER: &str = "import std.time

c := Channel[int](0)
parallel:
    spawn:
        time.sleep_ms(50)
        c.send(1)
    spawn:
        wait:
            r := c.recv(): print(\"got\")
            _ := time.timer(600000).recv(): assert false
d := Channel[int](0)
_ := d.recv()
";

#[test]
fn a_timer_that_lost_its_wait_does_not_hide_a_deadlock() {
    cell(LOST_TIMER, false, &[], 1, |r| {
        r.code == 1 && r.out == "got\n" && r.err.contains("deadlock: every task is blocked")
    });
}

#[test]
fn a_timer_left_by_one_test_does_not_hide_the_next_tests_deadlock() {
    let src = "import std.time

test fn a_timer_loses():
    c := Channel[int](0)
    parallel:
        spawn:
            time.sleep_ms(50)
            c.send(1)
        spawn:
            wait:
                r := c.recv(): print(\"got\")
                _ := time.timer(600000).recv(): assert false

test fn b_deadlock():
    c := Channel[int](0)
    _ := c.recv()
";
    cell(src, true, &[], 1, |r| {
        r.code == 1
            && r.out.contains("PASS a_timer_loses")
            && r.out.contains("ERROR b_deadlock")
            && r.out.contains("deadlock: every task is blocked")
            && r.out.contains("1 passed, 0 failed, 1 errored")
    });
}

#[test]
fn a_reused_timer_counts_again_when_waited_on_alone() {
    let src = "import std.time

c := Channel[int](0)
t := time.timer(200)
parallel:
    spawn:
        c.send(1)
    spawn:
        wait:
            r := c.recv(): print(\"got\")
            _ := t.recv(): assert false
_ := t.recv()
print(\"fired\")
";
    cell(src, false, &[], 0, |r| {
        r.code == 0 && r.out == "got\nfired\n"
    });
}

#[test]
fn a_timer_arm_wins_a_wait_on_an_empty_channel() {
    let src = "import std.time

c := Channel[int](0)
wait:
    r := c.recv(): print(\"c\")
    _ := time.timer(200).recv(): print(\"timeout\")
";
    cell(src, false, &[], 0, |r| r.code == 0 && r.out == "timeout\n");
}

#[test]
fn a_lone_timer_recv_ends() {
    let src = "import std.time

_ := time.timer(200).recv()
print(\"done\")
";
    cell(src, false, &[], 0, |r| r.code == 0 && r.out == "done\n");
}

/// `--timeout` fires a module timer's job early (at the test's deadline, before the timer's own):
/// that early wake is not the timer firing, so a later test that waits on the same timer still
/// counts it (base binary and Go: the wait ends when the timer fires, no deadlock).
#[test]
fn an_early_timeout_wake_does_not_mark_the_timer_fired() {
    let src = "import std.time

t := time.timer(300)

test fn a_times_out():
    parallel:
        spawn:
            _ := t.recv()

test fn b_waits():
    parallel:
        spawn:
            _ := t.recv()
";
    cell(src, true, &["--timeout=200"], 0, |r| {
        r.code == 1
            && r.out.contains("TIMED-OUT a_times_out")
            && r.out.contains("PASS b_waits")
            && r.out.contains("0 errored, 1 timed out")
    });
}
