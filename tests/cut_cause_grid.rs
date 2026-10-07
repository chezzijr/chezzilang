//! TICKET-195 (W18 Family B2): a party is either unwinding its OWN fault or it is CUT by another
//! party's (a cancel, a child's fault, a job's fault). Every rule keyed on that answer — which
//! fault is reported, whose trace prints, which work is stopped — reads one record, `Vm::cut`.
//! One grid enumerates it: halt cause × party × cleanup × worker count.
//!
//! The boundary axis is "own fault vs delivered fault": `own` and `timeout` are the own-fault
//! controls for `child`, `join` and `cancel`, and `join` vs `child` is "delivered at the join vs
//! delivered mid-wait". Judged against the ancestors: Go reports a child's `panic: boom` whatever
//! its owner's `defer` does, and CPython lets an outside `ThreadPoolExecutor`'s job finish.
//! A stuck cleanup on a cut owner meets the deadlock verdict and ends the run, as Go's does; one in
//! a cancelled sibling stays swallowed (TICKET-223).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// One finished run: exit code, stdout, stderr, and whether the cell's marker file existed 500 ms
/// after the process exited (`false` when the cell has no marker).
struct Got {
    code: i32,
    out: String,
    err: String,
    marker: bool,
}

/// Runs the program `src(marker_path)` at `threads` workers; `None` = still running after the
/// 10 s kill limit. `test_mode` names the file `cell_test.chz` and runs `chezzi test
/// --timeout=300`; otherwise `main.chz` under `chezzi run`.
fn run_with(
    name: &str,
    src: &dyn Fn(&Path) -> String,
    test_mode: bool,
    threads: &str,
    seed: Option<u32>,
) -> Option<Got> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "chz-t195-{name}-{threads}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let marker: PathBuf = dir.join("marker");
    let text = src(&marker);
    let path = dir.join(if test_mode {
        "cell_test.chz"
    } else {
        "main.chz"
    });
    std::fs::write(&path, &text).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    if test_mode {
        cmd.arg("test").arg("--timeout=300");
    } else {
        cmd.arg("run");
    }
    cmd.arg(&path)
        .env("CHEZZI_THREADS", threads)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(s) = seed {
        cmd.env("CHEZZI_SCHED_SEED", s.to_string());
    } else {
        cmd.env_remove("CHEZZI_SCHED_SEED");
    }
    let mut child = cmd.spawn().expect("spawn chezzi");
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s);
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut out = String::new();
    let mut err = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    let uses_marker = text.contains(marker.to_str().unwrap());
    if uses_marker {
        // A job the bail failed to stop would create it after the process exited, too.
        std::thread::sleep(Duration::from_millis(500));
    }
    let marker = uses_marker && marker.exists();
    let _ = std::fs::remove_dir_all(&dir);
    status.map(|s| Got {
        code: s.code().unwrap_or(-1),
        out,
        err,
        marker,
    })
}

const PRELUDE: &str = "import std.time
import std.concurrency
import std.os
import std.process
stuck := Channel[int](0)
fn boomer():
    time.sleep_ms(50)
    panic(\"boom\")
fn waiter():
    time.sleep_ms(1000)
";

/// The `job` cause's extra declarations: a job that faults with an index error before it sends.
const JOB_PRELUDE: &str = "jch := Channel[int](0)
fn work(xs: List[int]):
    jch.send(xs[5])
";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Cause {
    Child,
    Join,
    Cancel,
    Exit,
    Own,
    Job,
    Deadlock,
    Timeout,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Party {
    Main,
    Fn,
    Spawned,
    Job,
    Nested,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Cleanup {
    None,
    Stuck,
    Faulting,
    Slow,
}

fn cleanup_lines(c: Cleanup) -> Vec<String> {
    let v: &[&str] = match c {
        Cleanup::None => &[],
        Cleanup::Stuck => &["defer:", "    print(stuck.recv())"],
        Cleanup::Faulting => &["defer:", "    panic(\"cleanup failed\")"],
        Cleanup::Slow => &["defer:", "    time.sleep_ms(1000)"],
    };
    v.iter().map(|s| s.to_string()).collect()
}

fn indent(lines: &[String], n: usize) -> Vec<String> {
    let pad = "    ".repeat(n);
    lines.iter().map(|l| format!("{pad}{l}")).collect()
}

/// Axis 1: the cause body B, with the cleanup C where the cause places it.
fn body(cause: Cause, c: Cleanup) -> Vec<String> {
    let c = cleanup_lines(c);
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let mut b = Vec::new();
    match cause {
        Cause::Child => {
            b.extend(s(&["parallel:", "    spawn boomer()"]));
            b.extend(indent(&c, 1));
            b.push("    waiter()".into());
        }
        Cause::Join => {
            b.extend(s(&["parallel:", "    spawn boomer()"]));
            b.extend(indent(&c, 1));
            b.push("    pass".into());
        }
        Cause::Cancel => {
            b.extend(s(&["parallel:", "    spawn boomer()", "    spawn:"]));
            b.extend(indent(&c, 2));
            b.push("        waiter()".into());
        }
        Cause::Exit => {
            b.extend(s(&[
                "parallel:",
                "    spawn:",
                "        time.sleep_ms(50)",
                "        os.exit(3)",
            ]));
            b.extend(indent(&c, 1));
            b.push("    waiter()".into());
        }
        Cause::Own => {
            b.extend(c);
            b.push("panic(\"own\")".into());
        }
        Cause::Job => {
            b.extend(c);
            b.push("print(jch.recv())".into());
        }
        Cause::Deadlock => {
            b.extend(c);
            b.push("print(stuck.recv())".into());
        }
        Cause::Timeout => {
            b.extend(c);
            b.push("waiter()".into());
        }
    }
    b
}

/// Axis 2: the party wrapper W(B), as (declarations, module-level statements). Test mode moves
/// the statements into `test fn cell():`.
fn wrap(party: Party, b: Vec<String>) -> (Vec<String>, Vec<String>) {
    let owner = |b: &[String]| {
        let mut d = vec!["fn owner():".to_string()];
        d.extend(indent(b, 1));
        d
    };
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    match party {
        Party::Main => (Vec::new(), b),
        Party::Fn => (owner(&b), s(&["owner()"])),
        Party::Spawned => (owner(&b), s(&["parallel:", "    spawn owner()"])),
        Party::Job => (
            owner(&b),
            s(&["ex0 := Executor()", "ex0.submit(owner)", "ex0.shutdown()"]),
        ),
        Party::Nested => {
            let mut d = s(&[
                "fn owner():",
                "    parallel:",
                "        spawn:",
                "            time.sleep_ms(2000)",
            ]);
            d.extend(indent(&b, 2));
            (d, s(&["owner()"]))
        }
    }
}

fn join(lines: &[String]) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

/// A cause × party × cleanup program (every party but `shutdown`).
fn cell_src(cause: Cause, party: Party, c: Cleanup, test_mode: bool) -> String {
    let (decls, stmts) = wrap(party, body(cause, c));
    let mut src = PRELUDE.to_string();
    let mut stmts_all = Vec::new();
    if cause == Cause::Job {
        src.push_str(JOB_PRELUDE);
        stmts_all.push("jx := Executor()".to_string());
        stmts_all.push("jx.submit(fn(): work([1, 2]))".to_string());
    }
    stmts_all.extend(stmts);
    src.push_str(&join(&decls));
    if test_mode {
        src.push_str("test fn cell():\n");
        src.push_str(&join(&indent(&stmts_all, 1)));
    } else {
        src.push_str(&join(&stmts_all));
    }
    src
}

/// C1: the owner waits in `ex.shutdown()` of an Executor created OUTSIDE its nursery.
fn shutdown_src(cause: Cause, c: Cleanup) -> String {
    let c = cleanup_lines(c);
    let mut owner = vec!["fn owner():".to_string(), "    parallel:".to_string()];
    owner.push("        spawn boomer()".into());
    match cause {
        Cause::Child => {
            owner.extend(indent(&c, 2));
            owner.push("        ex.shutdown()".into());
        }
        Cause::Cancel => {
            owner.push("        spawn:".into());
            owner.extend(indent(&c, 3));
            owner.push("            ex.shutdown()".into());
        }
        _ => unreachable!(),
    }
    let mut src = PRELUDE.to_string();
    src.push_str(
        "done := Channel[int](1)
fn job():
    time.sleep_ms(300)
    done.send(1)
ex := Executor()
ex.submit(job)
",
    );
    src.push_str(&join(&owner));
    src.push_str(
        "r := recover: owner()
print(r)
time.sleep_ms(500)
print(\"job done? {done.try_recv()}\")
",
    );
    src
}

/// `shutdown` × `timeout` (test mode): a module-level executor whose job has no checkpoint. With
/// `joined`, the test joins it and the `--timeout` bail must stop the job's second call; without,
/// the non-vacuity control, the job must reach it.
fn shutdown_timeout_src(c: Cleanup, joined: bool, marker: &Path) -> String {
    let mut src = PRELUDE.to_string();
    src.push_str(&format!(
        "ex := Executor()
fn job():
    _s := recover: process.run(\"sleep 1\")
    _r := recover: process.run(\"touch {}\")
test fn cell():
",
        marker.display()
    ));
    let mut body = cleanup_lines(c);
    body.push("ex.submit(job)".into());
    body.push(
        if joined {
            "ex.shutdown()"
        } else {
            "time.sleep_ms(1000)"
        }
        .into(),
    );
    src.push_str(&join(&indent(&body, 1)));
    for i in 0..6 {
        src.push_str(&format!("test fn later_{i}():\n    time.sleep_ms(250)\n"));
    }
    src
}

/// The verdict-drain cell (`Vm::finish_run`): a job parked forever under a main deadlock.
fn verdict_drain_src() -> String {
    let mut src = PRELUDE.to_string();
    src.push_str(
        "jstuck := Channel[int](0)
fn parked():
    print(\"job started\")
    print(jstuck.recv())
ex := Executor()
ex.submit(parked)
time.sleep_ms(200)
print(stuck.recv())
",
    );
    src
}

#[derive(Clone, Copy, Debug)]
enum Expect {
    /// `child`, `join`, `cancel`: the child's `boom`, its frames only, no cleanup report
    Delivered,
    Exit,
    Own(Cleanup),
    Job,
    Deadlock,
    /// C1: the outside Executor's job finished
    ShutdownSurvives,
    /// C1, a `child` cause with a stuck cleanup on the owner: the cleanup meets the deadlock
    /// verdict, which is fatal (Go: `all goroutines are asleep`); the cause `boom` is the one report
    /// (DEC-147) and no `recover:` catches it (TICKET-223)
    ShutdownStuckFatal,
    /// test mode: the cell timed out
    TimedOut,
    /// `shutdown` × `timeout`: timed out, and the job's second call never ran
    TimedOutStopped,
    /// the non-vacuity control: the unjoined job DID make its second call
    Control,
    /// `finish_run`: a verdict, the parked job's line, no hang
    VerdictDrain,
}

struct Cell {
    name: String,
    src: Box<dyn Fn(&Path) -> String + Send + Sync>,
    test_mode: bool,
    runs: Vec<(&'static str, Option<u32>)>,
    expect: Expect,
}

fn ok(e: Expect, g: &Got) -> bool {
    let (out, err) = (&g.out, &g.err);
    match e {
        Expect::Delivered => {
            g.code != 0
                && err.contains("boom")
                && !err.contains("cleanup failed")
                && !err.contains("deadlock")
                && !err.contains("at waiter")
                && !err.contains("at owner")
        }
        Expect::Exit => g.code == 3 && !err.contains("boom"),
        Expect::Own(c) => {
            g.code != 0
                && match c {
                    Cleanup::Stuck => err.contains("deadlock"),
                    Cleanup::Faulting => err.contains("cleanup failed"),
                    _ => err.contains("own"),
                }
        }
        Expect::Job => {
            g.code != 0 && err.contains("index 5 out of bounds") && !err.contains("deadlock")
        }
        Expect::Deadlock => g.code != 0 && err.contains("deadlock"),
        Expect::ShutdownSurvives => {
            g.code == 0 && out.contains("Err('boom')") && out.contains("job done? Some(1)")
        }
        Expect::ShutdownStuckFatal => {
            g.code != 0
                && err.contains("boom")
                && !err.contains("deadlock")
                && !out.contains("Err('boom')")
                && !out.contains("job done?")
        }
        Expect::TimedOut => out.contains("TIMED-OUT cell") && out.contains("1 timed out"),
        Expect::TimedOutStopped => {
            out.contains("TIMED-OUT cell") && out.contains("1 timed out") && !g.marker
        }
        Expect::Control => g.marker,
        Expect::VerdictDrain => {
            g.code != 0 && err.contains("deadlock") && out.contains("job started")
        }
    }
}

const T3: [(&str, Option<u32>); 3] = [("0", None), ("1", None), ("2", None)];
const SEEDED: [(&str, Option<u32>); 4] = [
    ("1", Some(1)),
    ("1", Some(2)),
    ("2", Some(1)),
    ("2", Some(2)),
];

fn cells() -> Vec<Cell> {
    use Cause as K;
    use Cleanup as C;
    use Party as P;
    let parties = [P::Main, P::Fn, P::Spawned, P::Job, P::Nested];
    let mut v: Vec<Cell> = Vec::new();
    // Run mode: 5 parties × 7 causes × 3 cleanups.
    for cause in [
        K::Child,
        K::Join,
        K::Cancel,
        K::Exit,
        K::Own,
        K::Job,
        K::Deadlock,
    ] {
        for party in parties {
            for c in [C::None, C::Stuck, C::Faulting] {
                let mut runs = T3.to_vec();
                if c == C::Stuck {
                    runs.extend(SEEDED);
                }
                let expect = match cause {
                    K::Child | K::Join | K::Cancel => Expect::Delivered,
                    K::Exit => Expect::Exit,
                    K::Own => Expect::Own(c),
                    K::Job => Expect::Job,
                    K::Deadlock => Expect::Deadlock,
                    K::Timeout => unreachable!(),
                };
                v.push(Cell {
                    name: format!("run {cause:?} x {party:?} x {c:?}"),
                    src: Box::new(move |_| cell_src(cause, party, c, false)),
                    test_mode: false,
                    runs,
                    expect,
                });
            }
        }
    }
    // Run mode: `shutdown` × {child, cancel} × 3 cleanups (C1).
    for cause in [K::Child, K::Cancel] {
        for c in [C::None, C::Stuck, C::Faulting] {
            let mut runs = T3.to_vec();
            if c == C::Stuck {
                runs.extend(SEEDED);
            }
            v.push(Cell {
                name: format!("run {cause:?} x Shutdown x {c:?}"),
                src: Box::new(move |_| shutdown_src(cause, c)),
                test_mode: false,
                runs,
                expect: if cause == K::Child && c == C::Stuck {
                    Expect::ShutdownStuckFatal
                } else {
                    Expect::ShutdownSurvives
                },
            });
        }
    }
    // Test mode: 5 parties × `timeout` × 4 cleanups.
    for party in parties {
        for c in [C::None, C::Stuck, C::Faulting, C::Slow] {
            v.push(Cell {
                name: format!("test Timeout x {party:?} x {c:?}"),
                src: Box::new(move |_| cell_src(K::Timeout, party, c, true)),
                test_mode: true,
                runs: T3.to_vec(),
                expect: Expect::TimedOut,
            });
        }
    }
    // Test mode: `shutdown` × `timeout` × 4 cleanups — the bail stops the job.
    for c in [C::None, C::Stuck, C::Faulting, C::Slow] {
        v.push(Cell {
            name: format!("test Timeout x Shutdown x {c:?}"),
            src: Box::new(move |m| shutdown_timeout_src(c, true, m)),
            test_mode: true,
            runs: T3.to_vec(),
            expect: Expect::TimedOutStopped,
        });
    }
    // Test mode: `slow` × {child, join, own} × {main, fn, job, nested}: a hard halt inside a cut
    // party's cleanup stays a hard halt.
    for cause in [K::Child, K::Join, K::Own] {
        for party in [P::Main, P::Fn, P::Job, P::Nested] {
            v.push(Cell {
                name: format!("test {cause:?} x {party:?} x Slow"),
                src: Box::new(move |_| cell_src(cause, party, C::Slow, true)),
                test_mode: true,
                runs: T3.to_vec(),
                expect: Expect::TimedOut,
            });
        }
    }
    // Non-vacuity control: the same `shutdown` × `timeout` program, unjoined.
    v.push(Cell {
        name: "control Timeout x Shutdown unjoined".to_string(),
        src: Box::new(|m| shutdown_timeout_src(C::None, false, m)),
        test_mode: true,
        runs: T3.to_vec(),
        expect: Expect::Control,
    });
    // Verdict drain: a job parked forever under a main deadlock must not hang the drain.
    let mut runs = T3.to_vec();
    runs.extend(SEEDED);
    v.push(Cell {
        name: "run verdict drain".to_string(),
        src: Box::new(|_| verdict_drain_src()),
        test_mode: false,
        runs,
        expect: Expect::VerdictDrain,
    });
    v
}

fn check_cell(c: &Cell) -> Vec<String> {
    let tag: String = c
        .name
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    let mut misses = Vec::new();
    for &(t, seed) in &c.runs {
        let start = Instant::now();
        let got = run_with(&tag, &*c.src, c.test_mode, t, seed);
        let ms = start.elapsed().as_millis();
        let pass = got.as_ref().is_some_and(|g| ok(c.expect, g));
        if !pass {
            let what = match got {
                None => "hang (killed after 10s)".to_string(),
                Some(g) => format!(
                    "rc={} marker={} stdout={:?} stderr={:?} after {ms} ms",
                    g.code,
                    g.marker,
                    g.out.chars().take(200).collect::<String>(),
                    g.err.chars().take(240).collect::<String>()
                ),
            };
            misses.push(format!(
                "{} T={t} seed={seed:?}: {what}; expected {:?}",
                c.name, c.expect
            ));
        }
    }
    misses
}

/// Every halt cause × party × cleanup, at T=0/1/2 (stuck cleanups also at seeds 1 and 2), plus
/// the non-vacuity control and the verdict-drain cells. See the module doc.
#[test]
fn cut_cause_grid_every_cause_party_and_cleanup() {
    let cells = cells();
    let next = AtomicUsize::new(0);
    let misses = Mutex::new(Vec::new());
    std::thread::scope(|sc| {
        for _ in 0..4 {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(c) = cells.get(i) else { break };
                    let m = check_cell(c);
                    misses.lock().unwrap().extend(m);
                }
            });
        }
    });
    let mut misses = misses.into_inner().unwrap();
    misses.sort();
    assert!(
        misses.is_empty(),
        "{} misses over {} cells:\n{}",
        misses.len(),
        cells.len(),
        misses.join("\n")
    );
}
