//! TICKET-208 (W20 Family E1): an `Executor` job IS a spawned task. Every fact a new party
//! inherits from its starter — the globals view, the copy-write fault, cancel reach, fault
//! delivery, the exit rule, the runner permit — has one rule, whichever door started the party.
//! One grid enumerates it: party x fact x worker count x mode.
//!
//! Judged against the ancestors: Go for concurrency (a goroutine panic ends the program and no
//! `recover` in `main` catches it; `os.Exit` runs no deferred call), CPython for the `Executor`
//! API (`Future.result()` carries the job's exception; `ThreadPoolExecutor(max_workers=n)`).

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[path = "support/child_rusage.rs"]
mod child_rusage;

const THREADS: [&str; 3] = ["1", "2", "0"];
const BOUND: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Run,
    Test,
    /// `chezzi test --timeout=300`.
    TestTimeout,
}

/// One finished run. `code` is `None` when the harness killed the child at nine seconds.
struct Got {
    code: Option<i32>,
    out: String,
    err: String,
    wall: Duration,
    /// Each stdout line with its arrival time since the child started.
    stamps: Vec<(Duration, String)>,
}

impl Got {
    fn at(&self, line: &str) -> Option<Duration> {
        self.stamps.iter().find(|(_, l)| l == line).map(|(t, _)| *t)
    }
    fn show(&self) -> String {
        format!(
            "code={:?} wall={:.2}s\n--stdout--\n{}--stderr--\n{}",
            self.code,
            self.wall.as_secs_f64(),
            self.out,
            self.err
        )
    }
}

fn write_program(name: &str, src: &str, test: bool) -> (std::path::PathBuf, std::path::PathBuf) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("chz-t208-{name}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(if test { "cell_test.chz" } else { "main.chz" });
    std::fs::write(&path, src).expect("write program");
    (dir, path)
}

/// Runs `src` at `threads` workers. `stdin_after` pipes stdin and writes one line after that
/// delay; otherwise stdin is null.
fn run(name: &str, src: &str, mode: Mode, threads: &str, stdin_after: Option<Duration>) -> Got {
    let (dir, path) = write_program(name, src, mode != Mode::Run);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    match mode {
        Mode::Run => cmd.arg("run"),
        Mode::Test => cmd.arg("test"),
        Mode::TestTimeout => cmd.arg("test").arg("--timeout=300"),
    };
    cmd.arg(&path)
        .env("CHEZZI_THREADS", threads)
        .env_remove("CHEZZI_SCHED_SEED")
        .stdin(if stdin_after.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let start = Instant::now();
    let mut child = cmd.spawn().expect("spawn chezzi");
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out_reader = std::thread::spawn(move || {
        let mut stamps = Vec::new();
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            stamps.push((start.elapsed(), line));
        }
        stamps
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let feeder = stdin_after.map(|d| {
        let mut stdin = child.stdin.take().unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(d);
            let _ = stdin.write_all(b"line\n");
        })
    });
    let code = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s.code().unwrap_or(-1));
        }
        if start.elapsed() > Duration::from_secs(9) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let wall = start.elapsed();
    if let Some(f) = feeder {
        let _ = f.join();
    }
    let stamps = out_reader.join().expect("stdout reader");
    let err = err_reader.join().expect("stderr reader");
    let _ = std::fs::remove_dir_all(&dir);
    let out = stamps.iter().map(|(_, l)| format!("{l}\n")).collect();
    Got {
        code,
        out,
        err,
        wall,
        stamps,
    }
}

/// How a party is started. `nursery` parties start inside a `parallel:` body; the others start on
/// the Executor `ex`. `start` has `$F` where the party's zero-argument function goes.
struct Party {
    name: &'static str,
    nursery: bool,
    start: &'static str,
}

const PARTIES: [Party; 7] = [
    Party {
        name: "spawn",
        nursery: true,
        start: "spawn $F()",
    },
    Party {
        name: "nursery-child",
        nursery: true,
        start: "spawn nest($F)",
    },
    Party {
        name: "submit",
        nursery: false,
        start: "ex.submit($F)",
    },
    Party {
        name: "job-nursery-fiber",
        nursery: false,
        start: "ex.submit(fn(): nest($F))",
    },
    Party {
        name: "job-submits-job",
        nursery: false,
        start: "ex.submit(fn(): resubmit(ex, $F))",
    },
    Party {
        name: "executor-in-job",
        nursery: false,
        start: "ex.submit(fn(): own_ex($F))",
    },
    Party {
        name: "executor-in-job-nursery",
        nursery: false,
        start: "ex.submit(fn(): nest(fn(): own_ex($F)))",
    },
];

const PRELUDE: &str = "import std.time
import std.concurrency
import std.os
import std.io
import submit_task from std.concurrency.task
xs := [1]
out := Channel[str](4)
hs := Channel[int](8)
stuck := Channel[int](0)
flag := Shared[bool](false)
fn nest(f: fn() -> nil):
    parallel:
        spawn f()
fn resubmit(e: Executor, f: fn() -> nil):
    e.submit(f)
fn own_ex(f: fn() -> nil):
    e2 := Executor()
    e2.submit(f)
    e2.shutdown()
fn show1():
    out.send(\"1 {xs}\")
fn show2():
    out.send(\"2 {xs}\")
fn wr():
    hs.send(1)
    xs.push(9)
fn wr_recovered():
    r := recover: xs.push(9)
    match r:
        Ok(_): out.send(\"no fault\")
        Err(e): out.send(e.message())
fn parked():
    hs.send(1)
    _ := stuck.recv()
    flag.set(true)
    print(\"ran\")
fn sleeper():
    hs.send(1)
    time.sleep_ms(3000)
    print(\"ran\")
fn deferring():
    defer:
        print(\"d\")
    hs.send(1)
    _ := stuck.recv()
fn boomer():
    panic(\"boom\")
fn bad_int() -> int:
    panic(\"boom\")
fn slow():
    time.sleep_ms(6000)
fn burn():
    t0 := time.now_ms()
    i := 0
    while time.now_ms() - t0 < 600:
        i += 1
";

/// `PRELUDE`, `decls`, then `body` run by the party's context: inside `parallel:` for a nursery
/// party, between `ex := Executor()` and `end` for the others. `$S(f)` in a body line expands to
/// the party's start of `f`. `test` wraps the context in `test fn cell():` instead of `main`.
fn prog(p: &Party, decls: &str, body: &[&str], end: &str, test: bool) -> String {
    let mut s = String::from(PRELUDE);
    s.push_str(decls);
    s.push_str(if test {
        "test fn cell():\n"
    } else {
        "fn main():\n"
    });
    let pad = if p.nursery {
        s.push_str("    parallel:\n");
        "        "
    } else {
        s.push_str("    ex := Executor()\n");
        "    "
    };
    for line in body.iter().copied().chain((!p.nursery).then_some(end)) {
        if line.is_empty() {
            continue;
        }
        let line = match (line.find("$S("), line.rfind(')')) {
            (Some(a), Some(b)) => format!(
                "{}{}{}",
                &line[..a],
                p.start.replace("$F", &line[a + 3..b]),
                &line[b + 1..]
            ),
            _ => line.to_string(),
        };
        s.push_str(pad);
        s.push_str(&line);
        s.push('\n');
    }
    if !test {
        s.push_str("main()\n");
    }
    s
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

#[test]
fn globals_are_copied_at_start_for_every_party() {
    let mut misses = Vec::new();
    for p in &PARTIES {
        let src = prog(
            p,
            "",
            &[
                "$S(show1)",
                "print(out.recv())",
                "xs.push(2)",
                "$S(show2)",
                "print(out.recv())",
            ],
            "ex.shutdown()",
            false,
        );
        for t in THREADS {
            let g = run("globals", &src, Mode::Run, t, None);
            if g.out != "1 [1]\n2 [1, 2]\n" || g.code != Some(0) {
                misses.push(format!("{} T={t}: {}", p.name, g.show()));
            }
        }
    }
    judge(misses);
}

#[test]
fn a_write_to_the_copy_faults_for_every_party() {
    let mut misses = Vec::new();
    for p in &PARTIES {
        let src = prog(p, "", &["$S(wr)", "hs.recv()"], "ex.shutdown()", false);
        for t in THREADS {
            let g = run("copywrite", &src, Mode::Run, t, None);
            if !g.err.contains("is this task's copy") || g.code == Some(0) || g.code.is_none() {
                misses.push(format!("{} T={t}: {}", p.name, g.show()));
            }
        }
    }
    judge(misses);
}

#[test]
fn cancel_reaches_every_party() {
    let mut misses = Vec::new();
    for p in &PARTIES {
        // (cutter, body, end, mode, required stderr-or-stdout text, required exit code)
        let mut cells: Vec<(&str, Vec<&str>, &str, Mode, &str, Option<i32>)> = vec![
            (
                "sibling-fault",
                vec!["$S(parked)", "hs.recv()", "$S(boomer)"],
                "ex.shutdown()",
                Mode::Run,
                "boom",
                None,
            ),
            (
                "exit",
                vec!["$S(parked)", "hs.recv()", "os.exit(3)"],
                "ex.shutdown()",
                Mode::Run,
                "",
                Some(3),
            ),
            (
                "timeout",
                vec!["$S(sleeper)", "hs.recv()"],
                "ex.shutdown()",
                Mode::TestTimeout,
                "TIMED-OUT cell",
                None,
            ),
        ];
        if !p.nursery {
            cells.push((
                "shutdown_now",
                vec!["$S(parked)", "hs.recv()"],
                "ex.shutdown_now()",
                Mode::Run,
                "",
                Some(0),
            ));
        }
        for (cutter, body, end, mode, text, code) in cells {
            let src = prog(p, "", &body, end, mode != Mode::Run);
            for t in THREADS {
                let g = run("cancel", &src, mode, t, None);
                let all = format!("{}{}", g.out, g.err);
                if g.out.contains("ran")
                    || g.wall >= BOUND
                    || g.code.is_none()
                    || !all.contains(text)
                    || code.is_some_and(|c| g.code != Some(c))
                    || (code.is_none() && g.code == Some(0))
                {
                    misses.push(format!("{} x {cutter} T={t}: {}", p.name, g.show()));
                }
            }
        }
    }
    judge(misses);
}

const HANDLE_DECLS: &str = "fn h(e: Executor):
    t := submit_task(e, bad_int)
    match t.get():
        Ok(_): print(\"ok\")
        Err(x): print(\"err \" + x.message())
    hs.send(1)
fn h_own():
    e := Executor()
    h(e)
    e.shutdown()
fn exits() -> int:
    os.exit(3)
    return 1
";

#[test]
fn a_job_fault_is_delivered_by_its_handle_or_ends_the_run() {
    let mut misses = Vec::new();
    let submit = &PARTIES[2];
    // A handle job's fault is an `Err` on the handle; the run exits zero.
    for (name, start) in [
        ("direct", "h(ex)"),
        ("in-job", "ex.submit(fn(): h(ex))"),
        ("own-executor-in-job", "ex.submit(h_own)"),
        (
            "own-executor-in-job-nursery",
            "ex.submit(fn(): nest(h_own))",
        ),
    ] {
        let src = prog(
            submit,
            HANDLE_DECLS,
            &[start, "hs.recv()"],
            "ex.shutdown()",
            false,
        );
        for t in THREADS {
            let g = run("handle", &src, Mode::Run, t, None);
            if g.code != Some(0) || !g.out.starts_with("err ") || !g.out.contains("boom") {
                misses.push(format!("handle {name} T={t}: {}", g.show()));
            }
        }
    }
    // A fire-and-forget fault ends the run at once, while another Executor's job is unfinished.
    for p in PARTIES.iter().filter(|p| !p.nursery) {
        let src = prog(
            p,
            "",
            &[
                "ex2 := Executor()",
                "ex2.submit(slow)",
                "$S(boomer)",
                "ex2.shutdown()",
                "print(\"main done\")",
            ],
            "ex.shutdown()",
            false,
        );
        for t in THREADS {
            let g = run("forget", &src, Mode::Run, t, None);
            if g.code.is_none()
                || g.code == Some(0)
                || !g.err.contains("boom")
                || g.out.contains("main done")
                || g.wall >= BOUND
            {
                misses.push(format!("fire-and-forget {} T={t}: {}", p.name, g.show()));
            }
        }
    }
    // `os.exit` inside a handle job is not an `Err`: it ends the run with its code.
    let src = prog(
        submit,
        HANDLE_DECLS,
        &[
            "t := submit_task(ex, exits)",
            "_r := t.get()",
            "print(\"after\")",
        ],
        "ex.shutdown()",
        false,
    );
    for t in THREADS {
        let g = run("handle-exit", &src, Mode::Run, t, None);
        if g.code != Some(3) || g.out.contains("after") {
            misses.push(format!("exit in a handle job T={t}: {}", g.show()));
        }
    }
    judge(misses);
}

#[test]
fn os_exit_runs_no_defer_for_any_party() {
    let mut misses = Vec::new();
    for p in &PARTIES {
        let src = prog(
            p,
            "",
            &["$S(deferring)", "hs.recv()", "os.exit(3)"],
            "ex.shutdown()",
            false,
        );
        for t in THREADS {
            let g = run("nodefer", &src, Mode::Run, t, None);
            if g.code != Some(3) || g.out.contains('d') {
                misses.push(format!("{} T={t}: {}", p.name, g.show()));
            }
        }
    }
    judge(misses);
}

#[cfg(unix)]
#[test]
fn one_runner_at_one_worker_for_every_party() {
    let mut misses = Vec::new();
    for p in &PARTIES {
        let src = prog(p, "", &["$S(burn)", "burn()"], "ex.shutdown()", false);
        let (dir, path) = write_program("runner", &src, false);
        let (wall, user, _sys, status, _out) =
            child_rusage::run_timed(&["run", path.to_str().unwrap()], "1");
        let _ = std::fs::remove_dir_all(&dir);
        if !status.success() || user.as_secs_f64() > 1.3 * wall.as_secs_f64() {
            misses.push(format!(
                "{}: status={status:?} wall={:.2}s user={:.2}s",
                p.name,
                wall.as_secs_f64(),
                user.as_secs_f64()
            ));
        }
    }
    judge(misses);
}

#[test]
fn executor_limit_caps_running_jobs() {
    let src = "import std.time
import std.concurrency
cur := AtomicInt(0)
over := AtomicInt(0)
two := AtomicInt(0)
fn job():
    v := cur.add(1)
    if v > 2:
        over.store(1)
    if v == 2:
        two.store(1)
    time.sleep_ms(100)
    cur.sub(1)
ex := Executor(2)
ex.submit(job)
ex.submit(job)
ex.submit(job)
ex.submit(job)
ex.shutdown()
print(\"over={over.load()} two={two.load()}\")
";
    let mut misses = Vec::new();
    for t in THREADS {
        let g = run("limit", src, Mode::Run, t, None);
        if g.out != "over=0 two=1\n" || g.code != Some(0) {
            misses.push(format!("T={t}: {}", g.show()));
        }
    }
    judge(misses);
}

#[test]
fn a_test_run_reports_a_top_level_job_fault() {
    let src = "import std.time
import std.concurrency
fn slowbad():
    time.sleep_ms(200)
    xs := [1]
    print(xs[3])
ex := Executor()
ex.submit(slowbad)
test fn a():
    time.sleep_ms(500)
test fn b():
    assert true
";
    let mut misses = Vec::new();
    for t in THREADS {
        let g = run("c2", src, Mode::Test, t, None);
        let all = format!("{}{}", g.out, g.err);
        if g.code.is_none() || g.code == Some(0) || !all.contains("out of bounds") {
            misses.push(format!("T={t}: {}", g.show()));
        }
    }
    judge(misses);
}

#[test]
fn the_grid_holds_inside_a_test_fn() {
    let mut misses = Vec::new();
    for p in &PARTIES {
        let mut body = vec![
            "$S(show1)",
            "assert out.recv() == \"1 [1]\"",
            "xs.push(2)",
            "$S(show2)",
            "assert out.recv() == \"2 [1, 2]\"",
            "$S(wr_recovered)",
            "assert out.recv().contains(\"is this task's copy\")",
        ];
        if !p.nursery {
            body.extend([
                "t := submit_task(ex, bad_int)",
                "match t.get():",
                "    Ok(_): assert false",
                "    Err(e): assert e.message().contains(\"boom\")",
                "$S(parked)",
                "hs.recv()",
                "ex.shutdown_now()",
                "assert not flag.get()",
            ]);
        }
        let src = prog(p, "", &body, "", true);
        for t in THREADS {
            let g = run("testfn", &src, Mode::Test, t, None);
            if g.code != Some(0) || !g.out.contains("0 failed, 0 errored") {
                misses.push(format!("{} T={t}: {}", p.name, g.show()));
            }
        }
    }
    judge(misses);
}

/// The joiner is not the creator. A cut joiner leaves at once and the job it waited for runs on.
/// Green on the old engine; it guards the join rebuilt by step 5.
#[test]
fn a_cut_joiner_leaves_inside_the_bound_and_the_job_survives() {
    const HEAD: &str = "import std.time
import std.concurrency
ex := Executor()
started := Channel[int](1)
done := Channel[int](1)
fn job():
    started.send(1)
    time.sleep_ms(3000)
    done.send(1)
fn boomer():
    started.recv()
    panic(\"boom\")
";
    const TAIL: &str = "ex.submit(job)
r := recover: owner()
print(\"cut\")
done.recv()
print(\"job done\")
";
    let sibling = format!(
        "{HEAD}fn owner():
    parallel:
        spawn boomer()
        ex.shutdown()
{TAIL}"
    );
    let cancel = format!(
        "{HEAD}fn owner():
    parallel:
        spawn boomer()
        spawn:
            ex.shutdown()
{TAIL}"
    );
    let mut misses = Vec::new();
    for (name, src) in [("sibling-fault", &sibling), ("cancel", &cancel)] {
        for t in THREADS {
            let g = run("cutjoin", src, Mode::Run, t, None);
            let cut = g.at("cut");
            let job = g.at("job done");
            if g.code != Some(0)
                || cut.is_none_or(|c| c >= BOUND)
                || job.is_none_or(|j| j < Duration::from_millis(2500))
            {
                misses.push(format!(
                    "{name} T={t}: cut={cut:?} job={job:?} {}",
                    g.show()
                ));
            }
        }
    }
    let timeout = "import std.time
import std.concurrency
ex := Executor()
fn job():
    time.sleep_ms(3000)
    print(\"job ran\")
test fn cell():
    ex.submit(job)
    ex.shutdown()
";
    for t in THREADS {
        let g = run("cutjoin-to", timeout, Mode::TestTimeout, t, None);
        let all = format!("{}{}", g.out, g.err);
        if !all.contains("TIMED-OUT cell") || all.contains("job ran") || g.wall >= BOUND {
            misses.push(format!("timeout T={t}: {}", g.show()));
        }
    }
    judge(misses);
}

/// A flagless `main` holds no cancel flag, so every state it can be in must read the run-wide
/// halt. The 100 ms only makes a mid-state arrival the common one; either order must pass.
#[test]
fn a_fire_and_forget_fault_cuts_main_in_every_state() {
    const HEAD: &str = "import std.time
import std.concurrency
import std.io
fn bad():
    time.sleep_ms(100)
    panic(\"boom\")
fn double(x: int) -> int:
    return x * 2
fn keep(a: int, b: int) -> int:
    return b
fn slow():
    time.sleep_ms(6000)
c := Channel[int](0)
fn feeder():
    time.sleep_ms(6000)
    c.send(1)
fn waiter():
    _v := c.recv()
ex := Executor()
ex.submit(bad)
";
    let states: [(&str, &str); 7] = [
        ("loop", "n := 0\nwhile true:\n    n += 1\n"),
        (
            "native-hof",
            "n := range(0, 10000000).map(double).fold(0, keep)\n",
        ),
        ("sleep", "time.sleep_ms(6000)\n"),
        (
            "recv",
            "ex2 := Executor()\nex2.submit(feeder)\nv := c.recv()\n",
        ),
        ("shutdown", "ex.submit(slow)\nex.shutdown()\n"),
        (
            "nursery-join",
            "ex2 := Executor()\nex2.submit(feeder)\nparallel:\n    spawn waiter()\n",
        ),
        ("stdin-read", "_line := io.read_line()\n"),
    ];
    let mut misses = Vec::new();
    for (name, state) in states {
        let src = format!("{HEAD}{state}print(\"main done\")\n");
        let stdin = (name == "stdin-read").then_some(Duration::from_millis(500));
        for t in THREADS {
            let g = run("cutmain", &src, Mode::Run, t, stdin);
            if g.code.is_none()
                || g.code == Some(0)
                || !g.err.contains("boom")
                || g.out.contains("main done")
                || g.wall >= BOUND
            {
                misses.push(format!("{name} T={t}: {}", g.show()));
            }
        }
    }
    judge(misses);
}

#[test]
fn a_test_run_reports_a_job_fault_that_lands_past_a_passing_test() {
    let mut misses = Vec::new();
    for d in [0, 300] {
        let src = format!(
            "import std.time
import std.concurrency
fn bad():
    time.sleep_ms({d})
    panic(\"boom\")
ex := Executor()
test fn one():
    ex.submit(bad)
test fn two():
    assert true
"
        );
        for t in THREADS {
            let g = run("pasttest", &src, Mode::Test, t, None);
            let all = format!("{}{}", g.out, g.err);
            if g.code.is_none() || g.code == Some(0) || !all.contains("boom") {
                misses.push(format!("D={d} T={t}: {}", g.show()));
            }
        }
    }
    judge(misses);
}
