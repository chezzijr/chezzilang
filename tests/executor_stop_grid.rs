//! TICKET-232 (wave 21 family E2): the Executor stop grid. Job state x stop call x who stops x
//! Executor kind x worker count. Each cell is a generated program run through the built binary,
//! and asserts which jobs run, that the handle settles, and that no false deadlock is reported.
//!
//! The rule the grid pins: a submit that returned without a fault and is under the cap always
//! starts. `shutdown_now()` cancels held jobs only; a started job runs to its first cancellation
//! point. Ancestor: CPython `ThreadPoolExecutor` (`shutdown(wait=False, cancel_futures=True)`
//! cancels only futures no worker has taken).
//!
//! Harness copied from `tests/executor_job_owner.rs` (`run`, `Got`, `judge`): each integration
//! test is its own crate.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const OK: &str = "got 7";
const CANCELLED: &str = "got !task cancelled: shutdown_now() stopped it before it finished";
const SUBMIT_FAULT: &str = "submit on a shut-down Executor (it no longer accepts work)";

/// One finished run. `code` is `None` when the harness killed the child at ten seconds.
struct Got {
    code: Option<i32>,
    out: String,
    err: String,
}

impl Got {
    fn show(&self) -> String {
        format!(
            "code={:?}\n--stdout--\n{}--stderr--\n{}",
            self.code, self.out, self.err
        )
    }
    fn got_lines(&self) -> Vec<&str> {
        self.out.lines().filter(|l| l.starts_with("got ")).collect()
    }
    fn ran(&self) -> bool {
        self.out.lines().any(|l| l == "T ran")
    }
}

/// Runs `src` at `threads` workers.
fn run(src: &str, threads: &str) -> Got {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("chz-t232-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(&path, src).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", threads)
        .env_remove("CHEZZI_SCHED_SEED")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let start = Instant::now();
    let mut child = cmd.spawn().expect("spawn chezzi");
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out_reader = std::thread::spawn(move || {
        let mut s = String::new();
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            s.push_str(&line);
            s.push('\n');
        }
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let code = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s.code().unwrap_or(-1));
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let out = out_reader.join().expect("stdout reader");
    let err = err_reader.join().expect("stderr reader");
    let _ = std::fs::remove_dir_all(&dir);
    Got { code, out, err }
}

/// Asserts `misses` is empty, printing every red cell.
fn judge(misses: Vec<String>) {
    assert!(
        misses.is_empty(),
        "{} red cell(s):\n{}",
        misses.len(),
        misses.join("\n")
    );
}

/// The target job's state when the stop call lands.
#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    /// Held behind the cap: every slot is taken by a blocker.
    Queued,
    /// Submitted under the cap; nothing orders its first instruction before the stop.
    Dispatched,
    /// Inside a recursion with no cancellation point.
    Running,
    ParkedSleep,
    ParkedChannel,
    /// Its handle already read `7`.
    Done,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Stop {
    Shutdown,
    ShutdownNow,
}

/// Who calls the stop: main, or a job of the same Executor.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Stopper {
    Main,
    SameJob,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Bounded1,
    Bounded2,
    Unbounded,
}

impl Kind {
    fn cap(self) -> usize {
        match self {
            Kind::Bounded1 => 1,
            Kind::Bounded2 => 2,
            Kind::Unbounded => 0,
        }
    }
    fn ctor(self) -> &'static str {
        match self {
            Kind::Bounded1 => "Executor(1)",
            Kind::Bounded2 => "Executor(2)",
            Kind::Unbounded => "Executor()",
        }
    }
}

const HEAD: &str = r#"import std.time
import std.concurrency
import std.concurrency.task
started := Channel[int](1)
gate := Channel[int](1)
go := Channel[int](1)
fn burn(n: int) -> int:
    if n < 2:
        return n
    return burn(n - 1) + burn(n - 2)
fn blocker():
    _v := gate.recv()
"#;

/// The program of one cell. Ordering reads no clock: a channel orders the stop after the state.
fn prog(state: State, stop: Stop, stopper: Stopper, kind: Kind) -> String {
    let call = match stop {
        Stop::Shutdown => "shutdown()",
        Stop::ShutdownNow => "shutdown_now()",
    };
    let same = stopper == Stopper::SameJob;
    let cap = kind.cap();
    let mut s = String::from(HEAD);

    // The target job.
    s.push_str("fn target() -> int:\n");
    match state {
        State::Queued | State::Dispatched | State::Done => {}
        State::Running => s.push_str("    started.send(1)\n    _b := burn(27)\n"),
        State::ParkedSleep => s.push_str("    started.send(1)\n    time.sleep_ms(300)\n"),
        State::ParkedChannel => s.push_str("    started.send(1)\n    _g := gate.recv()\n"),
    }
    s.push_str("    print(\"T ran\")\n    return 7\n");

    // What the stopper does before the stop call. Only Done reads the handle `h`.
    let mut pre = Vec::new();
    match state {
        State::Queued => {
            if same {
                pre.push("_go := go.recv()".to_string());
            }
            if stop == Stop::Shutdown {
                // One send per blocker that waits on `gate`; a SameJob stopper is itself a blocker.
                for _ in 0..cap - usize::from(same) {
                    pre.push("gate.send(1)".to_string());
                }
            }
        }
        State::Dispatched => {}
        State::Running | State::ParkedSleep => pre.push("_s := started.recv()".to_string()),
        State::ParkedChannel => {
            pre.push("_s := started.recv()".to_string());
            if stop == Stop::Shutdown {
                pre.push("gate.send(1)".to_string());
            }
        }
        State::Done => pre.push("_r := h.get()".to_string()),
    }
    if same {
        let params = if state == State::Done {
            "e: Executor, h: task.Task[int]"
        } else {
            "e: Executor"
        };
        s.push_str(&format!("fn stopper({params}):\n"));
        for line in &pre {
            s.push_str(&format!("    {line}\n"));
        }
        s.push_str(&format!("    e.{call}\n"));
    }

    s.push_str(&format!("fn main():\n    ex := {}\n", kind.ctor()));
    const SUBMIT: &str = "    h := task.submit_task(ex, fn() -> int: target())\n";
    match (state, same) {
        (State::Queued, false) => {
            for _ in 0..cap {
                s.push_str("    ex.submit(fn(): blocker())\n");
            }
            s.push_str(SUBMIT);
        }
        (State::Queued, true) => {
            s.push_str("    ex.submit(fn(): stopper(ex))\n");
            for _ in 0..cap - 1 {
                s.push_str("    ex.submit(fn(): blocker())\n");
            }
            s.push_str(SUBMIT);
            s.push_str("    go.send(1)\n");
        }
        (State::Dispatched, true) => {
            // The ticket program: the stopper job is submitted BEFORE the target and stops at once.
            s.push_str("    ex.submit(fn(): stopper(ex))\n");
            s.push_str(SUBMIT);
        }
        (State::Done, true) => {
            s.push_str(SUBMIT);
            s.push_str("    ex.submit(fn(): stopper(ex, h))\n");
        }
        (_, true) => {
            s.push_str(SUBMIT);
            s.push_str("    ex.submit(fn(): stopper(ex))\n");
        }
        (_, false) => s.push_str(SUBMIT),
    }
    if same {
        s.push_str("    ex.shutdown()\n");
    } else {
        for line in &pre {
            s.push_str(&format!("    {line}\n"));
        }
        s.push_str(&format!("    ex.{call}\n"));
    }
    s.push_str("    print(\"got {h.get()}\")\nmain()\n");
    s
}

/// Cells the grid does not run, each with its reason.
fn skipped(state: State, stopper: Stopper, kind: Kind) -> bool {
    // An unbounded Executor has no queue: every submit is under the cap.
    if state == State::Queued && kind == Kind::Unbounded {
        return true;
    }
    // The stopper and the target cannot both hold the one slot, so the stopper cannot observe
    // the target Dispatched, Running or parked. Queued (the stopper holds the slot) and Done
    // (the target freed it) are reachable and run.
    stopper == Stopper::SameJob
        && kind == Kind::Bounded1
        && !matches!(state, State::Queued | State::Done)
}

/// `None` when the run obeys its cell's rule, else what is wrong.
fn check(state: State, stop: Stop, stopper: Stopper, kind: Kind, g: &Got) -> Option<String> {
    let silent = !g.ran() && g.got_lines().is_empty();
    // Exception 2 — a job that calls `shutdown()` on its own `Executor(1)` with a job held behind
    // it waits on itself: the held job needs the stopper's slot. Pinned to the measured outcome,
    // a deadlock fault, so a change to it is visible. CPython 3.14.7 twin (3 runs): `stopper
    // raised RuntimeError cannot join current thread`, `T ran`, `got 7`, rc 0. Both refuse the
    // self-join; CPython fails the job, Chezzi fails the run. Whether a job may join its own
    // Executor is not this ticket's decision.
    if state == State::Queued
        && stopper == Stopper::SameJob
        && stop == Stop::Shutdown
        && kind == Kind::Bounded1
    {
        return (g.code != Some(1) || !g.err.contains("deadlock") || !silent)
            .then(|| "the self-wait must end the run with a deadlock fault".to_string());
    }
    // Exception 1 — nothing orders the stopper's `shut` write after main's `shut` read, so the
    // target's submit may fault. That is a refusal the submitter sees. The silent drop (exit
    // status zero without `T ran`) stays red through the rule below.
    if state == State::Dispatched
        && stopper == Stopper::SameJob
        && g.code == Some(1)
        && g.err.contains(SUBMIT_FAULT)
        && silent
    {
        return None;
    }
    if g.code != Some(0) {
        return Some("exit status is not zero".to_string());
    }
    if g.err.contains("deadlock") {
        return Some("stderr reports a deadlock".to_string());
    }
    let cancelled = stop == Stop::ShutdownNow
        && matches!(
            state,
            State::Queued | State::ParkedSleep | State::ParkedChannel
        );
    let want = if cancelled { CANCELLED } else { OK };
    if g.got_lines() != [want] {
        return Some(format!("the handle must settle exactly once as `{want}`"));
    }
    if g.ran() == cancelled {
        return Some(if cancelled {
            "a cancelled job ran".to_string()
        } else {
            "a started job did not run".to_string()
        });
    }
    None
}

#[test]
fn executor_stop_grid() {
    let mut cells = Vec::new();
    for state in [
        State::Queued,
        State::Dispatched,
        State::Running,
        State::ParkedSleep,
        State::ParkedChannel,
        State::Done,
    ] {
        for stop in [Stop::Shutdown, Stop::ShutdownNow] {
            for stopper in [Stopper::Main, Stopper::SameJob] {
                for kind in [Kind::Bounded1, Kind::Bounded2, Kind::Unbounded] {
                    if skipped(state, stopper, kind) {
                        continue;
                    }
                    for t in ["1", "2", "0"] {
                        // Dispatched is the one state a channel cannot order: sample it.
                        let runs = if state == State::Dispatched { 20 } else { 1 };
                        for _ in 0..runs {
                            cells.push((state, stop, stopper, kind, t));
                        }
                    }
                }
            }
        }
    }
    let next = AtomicUsize::new(0);
    let misses = Mutex::new(Vec::new());
    std::thread::scope(|sc| {
        for _ in 0..6 {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(&(state, stop, stopper, kind, t)) = cells.get(i) else {
                        break;
                    };
                    let g = run(&prog(state, stop, stopper, kind), t);
                    if let Some(why) = check(state, stop, stopper, kind, &g) {
                        misses.lock().unwrap().push(format!(
                            "{state:?} x {stop:?} x {stopper:?} x {kind:?} T={t}: {why}\n{}",
                            g.show()
                        ));
                    }
                }
            });
        }
    });
    judge(misses.into_inner().unwrap());
}
