//! TICKET-219 (wave 21 family E2): an Executor job has one owner. After a job calls `os.exit`,
//! a held job (Executor(1), second job queued) must never start. Ancestors: CPython
//! `ThreadPoolExecutor(1)` + `os._exit`, Go semaphore + `os.Exit` -- nothing runs after the exit.

use std::process::Command;

const PROG: &str = r#"import std.os
import std.time
import std.concurrency
import std.concurrency.task
fn job(i: int) -> int:
    if i == 1:
        time.sleep_ms(100)
        os.exit(17)
    print("job {i} ran")
    return i
fn main():
    ex := Executor(1)
    h1 := task.submit_task(ex, fn() -> int: job(1))
    h2 := task.submit_task(ex, fn() -> int: job(2))
    print("h2 {h2.get()}")
main()
"#;

#[test]
fn held_job_never_starts_after_exit() {
    let dir = std::env::temp_dir().join(format!("chezzi_t219_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("main.chz");
    std::fs::write(&f, PROG).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&f)
        .output()
        .expect("spawn chezzi");
    let _ = std::fs::remove_dir_all(&dir);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(17));
    assert_eq!(stdout, "", "nothing may run or print after os.exit");
}

// ---------------------------------------------------------------------------------------------
// The job owner grid (TICKET-219 step 1). Harness copied from `tests/executor_task_grid.rs`
// (`run`, `Got`, `judge`): each integration test is its own crate.

use std::io::{BufRead, BufReader, Read};
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const CANCELLED: &str = "!task cancelled: shutdown_now() stopped it before it finished";

/// One finished run. `code` is `None` when the harness killed the child at ten seconds.
struct Got {
    code: Option<i32>,
    out: String,
    err: String,
    started: bool,
}

impl Got {
    fn show(&self) -> String {
        format!(
            "code={:?} started={}\n--stdout--\n{}--stderr--\n{}",
            self.code, self.started, self.out, self.err
        )
    }
    fn got_lines(&self) -> Vec<&str> {
        self.out.lines().filter(|l| l.starts_with("got ")).collect()
    }
}

/// Runs `src` at `threads` workers. `started` reports whether the target job printed `T start`
/// to stderr: `io.eprint` is `Kind::Inline`, so no checkpoint stands between a job's start and
/// the marker (`fs.mkdir` is `Kind::Blocking`, a checkpoint that cuts the job before the write).
fn run(src: &str, threads: &str, seed: Option<&str>) -> Got {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("chz-t219-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(&path, src).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", threads)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match seed {
        Some(s) => cmd.env("CHEZZI_SCHED_SEED", s),
        None => cmd.env_remove("CHEZZI_SCHED_SEED"),
    };
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
    let started = err.lines().any(|l| l == "T start");
    let _ = std::fs::remove_dir_all(&dir);
    Got {
        code,
        out,
        err,
        started,
    }
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

#[derive(Clone, Copy, PartialEq, Debug)]
enum Party {
    Spawn,
    Exec,
    Exec1,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Handle {
    Submit,
    SubmitTask,
    SubmitResult,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Held,
    Running,
    ParkedRecv,
    ParkedGuard,
    ParkedSleep,
    Done,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Event {
    OwnExit,
    ForeignExit,
    FireAndForgetFault,
    HandleFault,
    Shutdown,
    ShutdownNow,
    CreatorCancel,
}

const HEADER: &str = r#"import std.os
import std.io
import std.time
import std.concurrency
import std.concurrency.task
gate := Channel[int](0)
g := Shared[int](0)
fn hold(x: int) -> int:
    time.sleep_ms(300)
    return x
fn holder():
    g.update(fn(x: int) -> int: hold(x))
fn boom():
    time.sleep_ms(20)
    panic("boom")
fn drop(x: int):
    pass
fn tgt():
    drop(target())
"#;

/// The target's state code, indented for a fn body.
fn state_body(s: State) -> &'static str {
    match s {
        State::Held | State::Done => "",
        State::Running => "    x := 0\n    for i in 0..300000:\n        x += i\n",
        State::ParkedRecv => "    _ := gate.recv()\n",
        State::ParkedGuard => "    g.update(fn(x: int) -> int: x + 1)\n",
        State::ParkedSleep => "    time.sleep_ms(300)\n",
    }
}

/// The own event's action (events 1 and 4), run by the job that carries it.
fn own_action(e: Event) -> &'static str {
    match e {
        Event::OwnExit => "    os.exit(17)\n",
        Event::HandleFault => "    panic(\"bad\")\n",
        _ => "",
    }
}

/// The submit expression for `f`, a zero-arg `-> int` call, on executor `ex`.
fn submit_expr(h: Handle, ex: &str, f: &str) -> String {
    match h {
        Handle::Submit => format!("{ex}.submit(fn(): drop({f}))"),
        Handle::SubmitTask => format!("task.submit_task({ex}, fn() -> int: {f})"),
        Handle::SubmitResult => format!("{ex}.submit_result(fn() -> int: {f})"),
    }
}

/// `v := expr`, or the bare `expr` for `submit`, which returns nothing.
fn bind(h: Handle, v: &str, expr: String) -> String {
    match h {
        Handle::Submit => expr,
        _ => format!("{v} := {expr}"),
    }
}

/// Unindented lines that read handle `v`.
fn read_lines(h: Handle, v: &str) -> Vec<String> {
    match h {
        Handle::Submit => vec![],
        Handle::SubmitTask => vec![
            format!("print(\"got {{{v}.get()}}\")"),
            format!("print(\"done {{{v}.done()}}\")"),
        ],
        Handle::SubmitResult => vec![format!("print(\"got {{{v}.recv()}}\")")],
    }
}

fn prog(p: Party, h: Handle, s: State, e: Event) -> String {
    let mut src = String::from(HEADER);
    // The blocker carries the own event when the target is held.
    let blocker_act = if s == State::Held { own_action(e) } else { "" };
    src.push_str(&format!(
        "fn blocker() -> int:\n    time.sleep_ms(300)\n{blocker_act}    return 1\n"
    ));
    let target_act = if s == State::Held { "" } else { own_action(e) };
    src.push_str(&format!(
        "fn target() -> int:\n    io.eprint(\"T start\")\n{}{target_act}    print(\"T end\")\n    return 7\n",
        state_body(s)
    ));
    // The handle reader `rd` for a `defer:` read (event 3).
    match h {
        Handle::Submit => {}
        Handle::SubmitTask => src.push_str(
            "fn rd(h: task.Task[int]):\n    print(\"got {h.get()}\")\n    print(\"done {h.done()}\")\n",
        ),
        Handle::SubmitResult => {
            src.push_str("fn rd(h: Channel[int!]):\n    print(\"got {h.recv()}\")\n")
        }
    }

    // The body that submits and reads: main's, or the outer job's for creator cancel.
    let mut body: Vec<String> = Vec::new();
    if p == Party::Spawn {
        body.push("parallel:".into());
        let mut inner = vec!["spawn tgt()".to_string(), "time.sleep_ms(50)".into()];
        match e {
            Event::OwnExit => {
                if s == State::ParkedRecv {
                    inner.push("gate.send(1)".into());
                }
                inner.push("time.sleep_ms(1000)".into());
            }
            Event::ForeignExit => inner.push("os.exit(17)".into()),
            _ => {
                inner.push("ex3 := Executor()".into());
                inner.push("ex3.submit(fn(): boom())".into());
                inner.push("time.sleep_ms(300)".into());
            }
        }
        body.extend(inner.into_iter().map(|l| format!("    {l}")));
        body.push("print(\"M end\")".into());
    } else {
        if s == State::ParkedGuard {
            body.push("exh := Executor()".into());
            body.push("exh.submit(fn(): holder())".into());
            body.push("time.sleep_ms(20)".into());
        }
        let cap = if p == Party::Exec1 { "1" } else { "" };
        body.push(format!("ex := Executor({cap})"));
        if s == State::Held {
            body.push(bind(h, "b", submit_expr(h, "ex", "blocker()")));
        }
        body.push(bind(h, "h", submit_expr(h, "ex", "target()")));
        // A fire-and-forget fault cuts main in a sleep; only a `defer:` reads the handle.
        let ff = e == Event::FireAndForgetFault || (e == Event::HandleFault && h == Handle::Submit);
        if ff {
            if h != Handle::Submit {
                body.push("defer: rd(h)".into());
            }
            if s == State::ParkedRecv && e == Event::HandleFault {
                body.push("gate.send(1)".into());
            }
            if e == Event::FireAndForgetFault {
                body.push("ex3 := Executor()".into());
                body.push("ex3.submit(fn(): boom())".into());
                body.push("time.sleep_ms(300)".into());
            } else {
                body.push("time.sleep_ms(1000)".into());
            }
        } else {
            body.push("time.sleep_ms(50)".into());
            match e {
                Event::OwnExit | Event::HandleFault if s == State::ParkedRecv => {
                    body.push("gate.send(1)".into())
                }
                Event::ForeignExit => body.push("os.exit(17)".into()),
                Event::Shutdown => {
                    if s == State::ParkedRecv {
                        body.push("gate.send(1)".into());
                    }
                    body.push("ex.shutdown()".into());
                }
                Event::ShutdownNow => body.push("ex.shutdown_now()".into()),
                _ => {}
            }
            if e == Event::HandleFault && s == State::Held {
                body.extend(read_lines(h, "b"));
            }
            body.extend(read_lines(h, "h"));
            if h == Handle::Submit && e == Event::OwnExit {
                body.push("time.sleep_ms(1000)".into());
            }
        }
        if e != Event::CreatorCancel {
            body.push("print(\"M end\")".into());
        }
    }
    let creator = p != Party::Spawn && e == Event::CreatorCancel;
    src.push_str(if creator {
        "fn inner():\n"
    } else {
        "fn main():\n"
    });
    for l in &body {
        src.push_str(&format!("    {l}\n"));
    }
    if creator {
        src.push_str(
            "fn main():\n    outer := Executor()\n    outer.submit(fn(): inner())\n    time.sleep_ms(50)\n    outer.shutdown_now()\n    print(\"M end\")\n",
        );
    }
    src.push_str("main()\n");
    src
}

/// Returns why the cell is red, or `None` when it is green.
fn check(p: Party, h: Handle, s: State, e: Event, g: &Got) -> Option<String> {
    if g.code.is_none() {
        return Some("the harness killed the child".into());
    }
    let m_end = g.out.lines().any(|l| l == "M end");
    let gots = g.got_lines();
    let handle = h != Handle::Submit && p != Party::Spawn;
    let dropped_held = s == State::Held
        && matches!(
            e,
            Event::OwnExit
                | Event::ForeignExit
                | Event::FireAndForgetFault
                | Event::ShutdownNow
                | Event::CreatorCancel
        );
    if dropped_held && g.started {
        return Some("a held job started".into());
    }
    let ok_or_cancel = |l: &str| l == "got 7" || l == format!("got {CANCELLED}");
    match e {
        Event::OwnExit | Event::ForeignExit => {
            if g.code != Some(17) {
                return Some("exit code is not 17".into());
            }
            if !gots.is_empty() || m_end {
                return Some("something printed after os.exit".into());
            }
        }
        Event::FireAndForgetFault => {
            if g.code != Some(1) || !g.err.contains("boom") || m_end {
                return Some("the fault did not end the run".into());
            }
            if handle {
                if gots.len() != 1 {
                    return Some(format!("{} got lines, want 1", gots.len()));
                }
                if s == State::Held && gots[0] != format!("got {CANCELLED}") {
                    return Some("the held handle did not settle cancelled".into());
                }
                if !ok_or_cancel(gots[0]) {
                    return Some("the got line is neither 7 nor cancelled".into());
                }
                if h == Handle::SubmitTask && !g.out.contains("done true") {
                    return Some("no `done true`".into());
                }
            }
        }
        Event::HandleFault => {
            if handle {
                if !gots
                    .iter()
                    .any(|l| l.starts_with("got !") && l.contains("bad"))
                {
                    return Some("no `got !...bad...` line".into());
                }
                if !m_end {
                    return Some("no `M end`".into());
                }
            } else if g.code != Some(1) || !g.err.contains("bad") || m_end {
                return Some("the fire-and-forget fault did not end the run".into());
            }
        }
        Event::Shutdown => {
            if g.code != Some(0) || !g.out.contains("T end") || !m_end {
                return Some("shutdown did not run the job to its end".into());
            }
            if handle && !gots.iter().all(|l| *l == "got 7") {
                return Some("a handle did not read 7".into());
            }
        }
        Event::ShutdownNow => {
            if g.code != Some(0) || !m_end {
                return Some("shutdown_now did not end cleanly".into());
            }
            if handle {
                if gots.len() != 1 || !ok_or_cancel(gots[0]) {
                    return Some("want exactly one 7 or cancelled got line".into());
                }
                if h == Handle::SubmitTask && !g.out.contains("done true") {
                    return Some("no `done true`".into());
                }
            }
        }
        Event::CreatorCancel => {
            if g.code != Some(0) || !m_end {
                return Some("creator cancel did not end cleanly".into());
            }
        }
    }
    None
}

const STATES: [State; 6] = [
    State::Held,
    State::Running,
    State::ParkedRecv,
    State::ParkedGuard,
    State::ParkedSleep,
    State::Done,
];
const EVENTS: [Event; 7] = [
    Event::OwnExit,
    Event::ForeignExit,
    Event::FireAndForgetFault,
    Event::HandleFault,
    Event::Shutdown,
    Event::ShutdownNow,
    Event::CreatorCancel,
];
const HANDLES: [Handle; 3] = [Handle::Submit, Handle::SubmitTask, Handle::SubmitResult];

type Cell = (
    Party,
    Handle,
    State,
    Event,
    &'static str,
    Option<&'static str>,
);

#[test]
fn job_owner_grid() {
    let mut cells: Vec<Cell> = Vec::new();
    for p in [Party::Exec, Party::Exec1] {
        for s in STATES {
            if s == State::Held && p != Party::Exec1 {
                continue;
            }
            for e in EVENTS {
                if s == State::Done && e == Event::HandleFault {
                    continue;
                }
                for h in HANDLES {
                    for t in ["1", "2", "0"] {
                        cells.push((p, h, s, e, t, None));
                    }
                    if p == Party::Exec1 {
                        for seed in ["7", "11"] {
                            cells.push((p, h, s, e, "1", Some(seed)));
                        }
                    }
                }
            }
        }
    }
    for s in [
        State::Running,
        State::ParkedRecv,
        State::ParkedSleep,
        State::Done,
    ] {
        for e in [
            Event::OwnExit,
            Event::ForeignExit,
            Event::FireAndForgetFault,
        ] {
            for t in ["1", "2", "0"] {
                cells.push((Party::Spawn, Handle::Submit, s, e, t, None));
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
                    let Some(&(p, h, s, e, t, seed)) = cells.get(i) else {
                        break;
                    };
                    let g = run(&prog(p, h, s, e), t, seed);
                    if let Some(why) = check(p, h, s, e, &g) {
                        misses.lock().unwrap().push(format!(
                            "{p:?} x {h:?} x {s:?} x {e:?} T={t} seed={seed:?}: {why}\n{}",
                            g.show()
                        ));
                    }
                }
            });
        }
    });
    judge(misses.into_inner().unwrap());
}

const DHELD: &str = r#"import std.time
import std.concurrency
import std.concurrency.task
fn blocker() -> int:
    time.sleep_ms(300)
    return 1
fn target() -> int:
    print("target ran")
    return 7
fn boom():
    time.sleep_ms(20)
    panic("boom")
fn rd(h: task.Task[int]):
    print("defer got {h.get()}")
fn main():
    ex := Executor(1)
    _b := task.submit_task(ex, fn() -> int: blocker())
    h := task.submit_task(ex, fn() -> int: target())
    defer: rd(h)
    ex2 := Executor()
    ex2.submit(fn(): boom())
    time.sleep_ms(100)
    print("M end")
main()
"#;

const DRUN: &str = r#"import std.time
import std.concurrency
import std.concurrency.task
fn target() -> int:
    time.sleep_ms(300)
    print("target ran")
    return 7
fn boom():
    time.sleep_ms(20)
    panic("boom")
fn rd(h: task.Task[int]):
    print("defer got {h.get()}")
fn main():
    ex := Executor()
    h := task.submit_task(ex, fn() -> int: target())
    defer: rd(h)
    ex2 := Executor()
    ex2.submit(fn(): boom())
    time.sleep_ms(100)
    print("M end")
main()
"#;

#[test]
fn defer_reader_settles_after_a_job_fault() {
    let want = format!("defer got {CANCELLED}\n");
    let mut misses = Vec::new();
    for (name, src) in [("dheld", DHELD), ("drun", DRUN)] {
        for t in ["1", "2", "0"] {
            let g = run(src, t, None);
            if g.code != Some(1) || !g.err.contains("boom") || g.out != want {
                misses.push(format!("{name} T={t}: {}", g.show()));
            }
        }
    }
    judge(misses);
}

/// `~/.cache/hunt6/chan/p/td2.chz` cut to 50 rounds (CHAN3).
const TD2: &str = r#"import std.concurrency
import std.concurrency.task
fn main():
    flips := AtomicInt(0)
    for r in 0..50:
        ex := Executor()
        t := task.submit_task(ex, fn() -> int: r)
        ex.shutdown()
        parallel:
            spawn:
                for _ in 0..20000:
                    if not t.done():
                        flips.add(1)
            spawn:
                _ := t.get()
    print("flips {flips.load()}")
main()
"#;

#[test]
fn done_is_monotone_while_another_task_reads() {
    let mut misses = Vec::new();
    for t in ["2", "0"] {
        let g = run(TD2, t, None);
        if g.code != Some(0) || g.out != "flips 0\n" {
            misses.push(format!("T={t}: {}", g.show()));
        }
    }
    judge(misses);
}
