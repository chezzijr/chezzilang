//! The cancel-wake grid (plan 2026-11-21-cancel-is-a-wake): every wait kind x every cancel source
//! x `CHEZZI_THREADS` {1, 2, unset}. A cancel must end the wait it covers by waking it, never by a
//! 5 ms poll: each cell prints its golden output and reports `ticks=0`, the last field of the
//! `CHEZZI_SCHED_STATS=1` line (every 5 ms re-arm or poll wakeup a wait took to notice a halt).
//! The one exception is a demoted socket wait, which keeps its 5 ms `poll(2)` cap (owner,
//! 2026-11-21): its `ticks` is not checked.
//!
//! Each cell runs the debug binary under a watchdog. Its exit code, stdout and stderr (minus the
//! `[sched-stats]` line) must equal the literal golden in [`GOLDEN`], recorded from the base
//! binary built at `615881a3` (one engine: never compared against a second run). Every cell runs;
//! the test fails once at the end with the list of red cells.
//!
//! Wait kinds (`waiter` signals `ready`, then waits; a `hooked` kind waits inside a map-key `hash`
//! hook, so the wait demotes):
//!
//! | kind | the wait | demoted |
//! |---|---|---|
//! | `sleep` | `time.sleep_ms(3000)` | no |
//! | `recv` | `never.recv()` | no |
//! | `send` | `never.send(1)` on an unbuffered channel | no |
//! | `wait` | `wait:` on two silent channels | no |
//! | `guard` | `s.update(..)` while `holder` holds `s` | no |
//! | `exec_join` | `shutdown()` of an Executor whose job waits in `recv()` | no |
//! | `nursery_join` | the join of a `parallel:` whose task waits in `recv()` | no |
//! | `d_recv`, `d_send`, `d_wait`, `d_sleep`, `d_guard`, `d_exec_join`, `d_nursery_join` | the same wait in a `hash` hook | yes |
//! | `d_socket` | `peer.read(1)` in a `hash` hook, the peer sends nothing | yes |
//!
//! Sources (where the waiter runs):
//!
//! | source | the waiter runs | the cancel |
//! |---|---|---|
//! | `fault` | a `parallel:` task under `recover:` | a sibling's `panic("boom")` |
//! | `ex_cancel` | an Executor job | `ex.cancel()` then `ex.shutdown()` |
//! | `task_cancel` | an Executor job | `t.cancel()` then `ex.shutdown()` |
//! | `exit` | a `parallel:` task | a sibling's `os.exit(3)` |
//! | `timeout` | a `parallel:` task of a test fn | `chezzi test --timeout=200` |
//! | `job_exit` | an Executor job | main's `os.exit(3)` |
//! | `main_heap` | an Executor job of a test fn | the test fn's own allocation passes `--max-heap=8000000` |
//! | `backlog` | an Executor job of a test fn | 600 jobs' output passes `chezzi test --max-heap=8000000` |
//!
//! Every source runs with every kind (120 pairs, each at 3 worker counts). A guard kind's `holder`
//! runs where the waiter runs (a sibling task, or a job of the same Executor); under
//! `task_cancel` main closes `hold` after the cancel, so the holder ends and `shutdown()` returns.
//! The cancel lands after `ready` plus a 50 ms timer (a timer recv parks with one timer: no
//! ticks). Under `timeout` a sibling waits on a 5 s timer, so the run is not a deadlock.
//!
//! Goldens: `CANCEL_WAKE_GRID_BASE=<base chezzi> cargo test --test cancel_wake_grid` runs that
//! binary instead and prints the table [`GOLDEN`] is pasted from.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// One wait kind.
struct Kind {
    name: &'static str,
    /// The wait runs inside a map-key `hash` hook, so it demotes.
    hooked: bool,
    /// The wait, unindented. It sends on `ready` just before it waits.
    wait: &'static str,
    /// A `holder` task holds the box `s` beside the waiter.
    holder: bool,
    /// A socket pair (`peer`) is opened at the top.
    socket: bool,
}

const fn kind(name: &'static str, hooked: bool, wait: &'static str) -> Kind {
    Kind {
        name,
        hooked,
        wait,
        holder: false,
        socket: false,
    }
}

const SLEEP: &str = "ready.send(1)\ntime.sleep_ms(3000)";
const RECV: &str = "ready.send(1)\n_ := never.recv()";
const SEND: &str = "ready.send(1)\nnever.send(1)";
const WAIT: &str = "ready.send(1)\nwait:\n    _ := never.recv(): print(\"arm 1\")\n    _ := never2.recv(): print(\"arm 2\")";
const GUARD: &str = "_ := held.recv()\nready.send(1)\ns.update(fn(v: int) -> int: v + 1)";

const EXEC_JOIN: &str =
    "ex2 := Executor()\n_ := ex2.submit(fn(): park())\nready.send(1)\nex2.shutdown()";
const NURSERY_JOIN: &str = "ready.send(1)\nparallel:\n    spawn park()";

const KINDS: &[Kind] = &[
    kind("sleep", false, SLEEP),
    kind("recv", false, RECV),
    kind("send", false, SEND),
    kind("wait", false, WAIT),
    Kind {
        holder: true,
        ..kind("guard", false, GUARD)
    },
    kind("exec_join", false, EXEC_JOIN),
    kind("nursery_join", false, NURSERY_JOIN),
    kind("d_recv", true, RECV),
    kind("d_send", true, SEND),
    kind("d_wait", true, WAIT),
    kind("d_sleep", true, SLEEP),
    Kind {
        holder: true,
        ..kind("d_guard", true, GUARD)
    },
    kind("d_exec_join", true, EXEC_JOIN),
    kind("d_nursery_join", true, NURSERY_JOIN),
    Kind {
        socket: true,
        ..kind("d_socket", true, "ready.send(1)\n_ := peer.read(1)")
    },
];

const SOURCES: &[&str] = &[
    "fault",
    "ex_cancel",
    "task_cancel",
    "exit",
    "timeout",
    "job_exit",
    "main_heap",
    "backlog",
];

/// `CHEZZI_THREADS` per run; `None` leaves it unset (the default count).
const THREADS: &[Option<&str>] = &[Some("1"), Some("2"), None];

const HEAD: &str = "import std.time
import std.os
import std.net
import Executor, Shared from std.concurrency

ready := Channel[int](1)
never := Channel[int](0)
never2 := Channel[int](0)

fn park():
    _ := never.recv()
";

const HOLDER: &str = "
s := Shared[int](0)
held := Channel[int](1)
hold := Channel[int](0)

fn grab(v: int) -> int:
    held.send(1)
    _ := hold.recv()
    return v

fn holder():
    s.update(grab)
";

const SOCKET: &str = "
fn must[T](r: T!) -> T:
    match r:
        ?v: return v
        !e: panic(e.error())

ln := must(net.listen(\"127.0.0.1:0\"))
c := must(net.connect(must(ln.addr())))
peer := must(ln.accept())
";

const HOOK: &str = "
struct H:
    n: int
    fn eq(self, other: H) -> bool: return self.n == other.n
    fn hash(self) -> int:
        hooked()
        return self.n
";

/// `text` with every line indented by `by` spaces.
fn indent(text: &str, by: usize) -> String {
    let pad = " ".repeat(by);
    text.lines().map(|l| format!("{pad}{l}\n")).collect()
}

/// The program of one cell, and its file name (`timeout` runs a test file).
fn program(k: &Kind, source: &str) -> (String, &'static str) {
    let mut s = String::from(HEAD);
    if k.holder {
        s.push_str(HOLDER);
    }
    if k.socket {
        s.push_str(SOCKET);
    }
    let body = if k.hooked {
        s.push_str(HOOK);
        s.push_str(&format!("\nfn hooked():\n{}", indent(k.wait, 4)));
        "m := {H(1): 2}\nprint(m[H(1)])"
    } else {
        k.wait
    };
    s.push_str(&format!(
        "\nfn waiter():\n    defer: print(\"defer ran\")\n{}    print(\"after wait\")\n\n",
        indent(body, 4)
    ));
    // The holder beside the waiter: a sibling task (8 or 12 spaces) or a job of `ex`.
    let sib = |by: usize| {
        if k.holder {
            indent("spawn holder()", by)
        } else {
            String::new()
        }
    };
    let job = if k.holder {
        "    _ := ex.submit(fn(): holder())\n"
    } else {
        ""
    };
    let cut = "_ := ready.recv()\n_ := time.timer(50).recv()\n";
    let main = match source {
        "fault" => format!(
            "fn main():\n    r := recover:\n        parallel:\n{}            spawn waiter()\n            spawn:\n{}                panic(\"boom\")\n    print(r)\nmain()\n",
            sib(12),
            indent(cut, 16)
        ),
        "ex_cancel" => format!(
            "fn main():\n    ex := Executor()\n{job}    t := ex.submit(fn(): waiter())\n{}    ex.cancel()\n    ex.shutdown()\n    print(t.get())\nmain()\n",
            indent(cut, 4)
        ),
        "task_cancel" => format!(
            "fn main():\n    ex := Executor()\n{job}    t := ex.submit(fn(): waiter())\n{}    t.cancel()\n{}    ex.shutdown()\n    print(t.get())\nmain()\n",
            indent(cut, 4),
            if k.holder { "    hold.close()\n" } else { "" }
        ),
        "exit" => format!(
            "fn main():\n    parallel:\n{}        spawn waiter()\n        spawn:\n{}            os.exit(3)\nmain()\n",
            sib(8),
            indent(cut, 12)
        ),
        "timeout" => format!(
            "test fn waits():\n    parallel:\n{}        spawn waiter()\n        spawn:\n            _ := time.timer(5000).recv()\n",
            sib(8)
        ),
        "job_exit" => format!(
            "fn main():\n    ex := Executor()\n{job}    _ := ex.submit(fn(): waiter())\n{}    os.exit(3)\nmain()\n",
            indent(cut, 4)
        ),
        "main_heap" => format!(
            "test fn waits():\n    ex := Executor()\n{job}    _ := ex.submit(fn(): waiter())\n    _ := ready.recv()\n    xs := [\"\"]\n    for _i in 0..20000:\n        xs.push(\"x\".repeat(1000))\n    print(xs.len())\n    ex.shutdown()\n"
        ),
        "backlog" => format!(
            "line := \"x\".repeat(50000)\n\ntest fn waits():\n    ex := Executor()\n{job}    t := ex.submit(fn(): waiter())\n    _ := ready.recv()\n    for _i in 0..600:\n        _ := ex.submit(fn(): print(line))\n    ex.shutdown()\n    print(t.get())\n"
        ),
        other => panic!("no source {other}"),
    };
    s.push_str(&main);
    let file = match source {
        "timeout" => "main_test.chz",
        // A sleep the halt does not wake outlasts the watchdog, so it reads as a hang.
        "backlog" | "main_heap" => {
            s = s.replace("time.sleep_ms(3000)", "time.sleep_ms(3600000)");
            "backlog_test.chz"
        }
        _ => "main.chz",
    };
    (s, file)
}

/// One finished run: exit code, stdout, stderr without the `[sched-stats]` line, and that line.
struct Ran {
    code: i32,
    out: String,
    err: String,
    stats: Option<String>,
}

/// Runs one cell with `bin`; `None` = still running after the watchdog (killed).
fn run(bin: &str, src: &str, file: &str, threads: Option<&str>) -> Option<Ran> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("chz-cwake-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::write(dir.join(file), src).expect("write program");
    let mut cmd = Command::new(bin);
    match file {
        "main_test.chz" => cmd.args(["test", "--timeout=200"]),
        "backlog_test.chz" => cmd.args(["test", "--max-heap=8000000"]),
        _ => cmd.arg("run"),
    };
    cmd.arg(file)
        .current_dir(&dir)
        .env("CHEZZI_SCHED_STATS", "1")
        .env_remove("CHEZZI_SCHED_SEED")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match threads {
        Some(t) => cmd.env("CHEZZI_THREADS", t),
        None => cmd.env_remove("CHEZZI_THREADS"),
    };
    let mut child = cmd.spawn().expect("spawn chezzi");
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s);
        }
        if start.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let out = out.join().expect("stdout reader");
    let err = err.join().expect("stderr reader");
    let _ = std::fs::remove_dir_all(&dir);
    let status = status?;
    let mut stats = None;
    let err = err
        .lines()
        .filter(|l| match l.strip_prefix("[sched-stats] ") {
            Some(s) => {
                stats = Some(s.to_string());
                false
            }
            None => true,
        })
        .map(|l| format!("{l}\n"))
        .collect();
    Some(Ran {
        code: status.code().unwrap_or(-1),
        out,
        err,
        stats,
    })
}

/// `ticks=<n>`, the LAST field of the stats line (`None` if it is not there).
fn ticks(stats: &str) -> Option<usize> {
    stats
        .rsplit(' ')
        .next()?
        .strip_prefix("ticks=")?
        .parse()
        .ok()
}

/// `demotes=<n>` from the stats line (`None` if it is not there).
fn demotes(stats: &str) -> Option<usize> {
    stats
        .split(' ')
        .find_map(|f| f.strip_prefix("demotes="))?
        .parse()
        .ok()
}

/// What the cell `kind`/`source` must print, at every worker count.
fn golden(kind: &str, source: &str) -> (i32, &'static str, &'static str) {
    let (_, _, code, out, err) = GOLDEN
        .iter()
        .find(|g| g.0 == kind && g.1 == source)
        .unwrap_or_else(|| panic!("no golden for {kind}/{source}"));
    (*code, out, err)
}

/// One cell's misses at one worker count.
fn check(k: &Kind, source: &str, threads: Option<&str>) -> Vec<String> {
    let (src, file) = program(k, source);
    let name = format!("{}/{source}/T={}", k.name, threads.unwrap_or("unset"));
    let Some(r) = run(env!("CARGO_BIN_EXE_chezzi"), &src, file, threads) else {
        return vec![format!("{name}: hang (killed after 20 s)")];
    };
    let mut misses = Vec::new();
    let (code, out, err) = golden(k.name, source);
    if (r.code, r.out.as_str(), r.err.as_str()) != (code, out, err) {
        misses.push(format!(
            "{name}: golden mismatch: rc={} stdout={:?} stderr={:?}; expected rc={code} stdout={out:?} stderr={err:?}",
            r.code, r.out, r.err
        ));
    }
    let t = r.stats.as_deref().and_then(ticks);
    if k.name != "d_socket" && t != Some(0) {
        misses.push(format!("{name}: ticks={t:?}, expected Some(0)"));
    }
    // The cell tests the wait it names: a hooked wait demotes, a parked one does not.
    let d = r.stats.as_deref().and_then(demotes);
    if d.map(|d| d > 0) != Some(k.hooked) {
        misses.push(format!(
            "{name}: demotes={d:?}, expected hooked={}",
            k.hooked
        ));
    }
    misses
}

/// Every (kind, source, threads) cell, `jobs` at a time; `each` returns one cell's misses.
fn every_cell(
    jobs: usize,
    each: impl Fn(&'static Kind, &'static str, Option<&'static str>) -> Vec<String> + Sync,
) -> Vec<String> {
    let cells: Vec<(&'static Kind, &'static str, Option<&'static str>)> = KINDS
        .iter()
        .flat_map(|k| {
            SOURCES
                .iter()
                .flat_map(move |s| THREADS.iter().map(move |t| (k, *s, *t)))
        })
        .collect();
    let next = AtomicUsize::new(0);
    let misses = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                while let Some(&(k, s, t)) = cells.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let m = each(k, s, t);
                    misses.lock().unwrap().extend(m);
                }
            });
        }
    });
    let mut misses = misses.into_inner().unwrap();
    misses.sort();
    misses
}

#[test]
fn a_cancel_wakes_every_wait_without_a_poll() {
    if let Ok(base) = std::env::var("CANCEL_WAKE_GRID_BASE") {
        print_goldens(&base);
        return;
    }
    let misses = every_cell(4, check);
    assert!(
        misses.is_empty(),
        "{} red cell(s):\n{}",
        misses.len(),
        misses.join("\n")
    );
}

/// `deadlocks=<n>` from the stats line (`None` if it is not there).
fn deadlocks(stats: &str) -> Option<usize> {
    stats
        .split(' ')
        .find_map(|f| f.strip_prefix("deadlocks="))?
        .parse()
        .ok()
}

/// 2000 tasks each sleeping 2 s, at T=1, 2 and unset: a sleep parks with one timer, so the run
/// prints `done` with `ticks=0` (no 5 ms re-arm) and `deadlocks=0` (a sleeper counts awake).
/// Golden from the base binary (`615881a3`): stdout `done\n`, exit 0, no stderr.
#[test]
fn two_thousand_sleepers_wake_on_their_timer_without_a_poll() {
    const SRC: &str = "import std.time

fn main():
    parallel:
        for _ in 0..2000:
            spawn: time.sleep_ms(2000)
    print(\"done\")
main()
";
    let misses: Vec<String> = std::thread::scope(|scope| {
        let runs: Vec<_> = THREADS
            .iter()
            .map(|&t| {
                scope.spawn(move || {
                    let name = format!("sleepers/T={}", t.unwrap_or("unset"));
                    let Some(r) = run(env!("CARGO_BIN_EXE_chezzi"), SRC, "main.chz", t) else {
                        return vec![format!("{name}: hang (killed after 20 s)")];
                    };
                    let mut m = Vec::new();
                    if (r.code, r.out.as_str(), r.err.as_str()) != (0, "done\n", "") {
                        m.push(format!(
                            "{name}: golden mismatch: rc={} stdout={:?} stderr={:?}",
                            r.code, r.out, r.err
                        ));
                    }
                    let stats = r.stats.as_deref();
                    let t = stats.and_then(ticks);
                    if t != Some(0) {
                        m.push(format!("{name}: ticks={t:?}, expected Some(0)"));
                    }
                    let d = stats.and_then(deadlocks);
                    if d != Some(0) {
                        m.push(format!("{name}: deadlocks={d:?}, expected Some(0)"));
                    }
                    m
                })
            })
            .collect();
        runs.into_iter()
            .flat_map(|h| h.join().expect("sleepers run"))
            .collect()
    });
    assert!(misses.is_empty(), "{}", misses.join("\n"));
}

/// A job's `defer` joins a task inside a `hash` hook when a job's output passes `--max-heap`. The
/// `defer` runs whole under a job fault, so the join must end although the run's workers left.
/// The task signals `ready` from inside the nursery, so the halt always finds it there. A nursery
/// a `defer` opens AFTER the halt, on a value nobody sends, is the open gap W23-1.
#[test]
fn a_join_in_a_defer_hook_ends_at_a_job_fault() {
    const SRC: &str = "import Executor from std.concurrency

ready := Channel[int](1)
never := Channel[int](0)
line := \"x\".repeat(50000)

fn park():
    ready.send(1)
    _ := never.recv()

struct H:
    n: int
    fn eq(self, other: H) -> bool: return self.n == other.n
    fn hash(self) -> int:
        parallel:
            spawn park()
        return self.n

fn waiter():
    defer:
        m := {H(1): 2}
        print(m.len())

test fn waits():
    ex := Executor()
    _ := ex.submit(fn(): waiter())
    _ := ready.recv()
    for _i in 0..600:
        _ := ex.submit(fn(): print(line))
    ex.shutdown()
";
    let misses: Vec<String> = THREADS
        .iter()
        .filter_map(|&t| {
            let name = format!("defer_join/T={}", t.unwrap_or("unset"));
            match run(env!("CARGO_BIN_EXE_chezzi"), SRC, "backlog_test.chz", t) {
                None => Some(format!("{name}: hang (killed after 20 s)")),
                Some(r) if (r.code, r.out.as_str(), r.err.as_str()) != (1, BACKLOG_OUT, "") => {
                    Some(format!(
                        "{name}: rc={} stdout={:?} stderr={:?}",
                        r.code, r.out, r.err
                    ))
                }
                Some(_) => None,
            }
        })
        .collect();
    assert!(misses.is_empty(), "{}", misses.join("\n"));
}

/// Runs every cell with the base binary `bin` and prints the [`GOLDEN`] table, or the cells whose
/// output differs between worker counts.
fn print_goldens(bin: &str) {
    let rows = Mutex::new(Vec::new());
    let _ = every_cell(4, |k, s, t| {
        let (src, file) = program(k, s);
        let r = run(bin, &src, file, t);
        rows.lock().unwrap().push((k.name, s, t, r));
        Vec::new()
    });
    let rows = rows.into_inner().unwrap();
    for k in KINDS {
        for s in SOURCES {
            let mine: Vec<_> = rows.iter().filter(|r| r.0 == k.name && r.1 == *s).collect();
            let shapes: Vec<Option<(i32, &str, &str)>> = mine
                .iter()
                .map(|r| {
                    r.3.as_ref()
                        .map(|r| (r.code, r.out.as_str(), r.err.as_str()))
                })
                .collect();
            if shapes.iter().any(|x| *x != shapes[0]) || shapes[0].is_none() {
                println!("// DIFFERS {}/{s}: {:?}", k.name, shapes);
                continue;
            }
            let (code, out, err) = shapes[0].unwrap();
            println!("    ({:?}, {s:?}, {code}, {out:?}, {err:?}),", k.name);
        }
    }
}

/// What every `backlog` and `main_heap` cell prints. Base printed it for `sleep`, `d_recv`, `d_send`, `d_wait`,
/// `d_sleep` and `d_socket` at every worker count. In the other nine kinds base left the parked
/// waiter unwoken: a false `deadlock` or a hang at T=1 or 2, so those rows are not base.
const BACKLOG_OUT: &str = "OVER-MEMORY waits (backlog_test.chz) test exceeded --max-heap (8000000 bytes)\n\n1 test(s): 0 passed, 0 failed, 0 errored, 1 over-memory\n";

/// `(kind, source, exit code, stdout, stderr without the stats line)`, from the base binary
/// (`615881a3`) at T=1, 2 and unset, which agreed. Not base: `exec_join/fault` and
/// `d_exec_join/fault`, a base bug the owner chose to fix (2026-11-21), and the `backlog` rows
/// [`BACKLOG_OUT`] names, and `d_nursery_join/job_exit`, where base hung (Go's `os.Exit` ends the
/// run at once, with no output and no `defer`); a re-record from base must keep those rows.
const GOLDEN: &[(&str, &str, i32, &str, &str)] = &[
    ("sleep", "fault", 0, "defer ran\n!boom\n", ""),
    (
        "sleep",
        "ex_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    (
        "sleep",
        "task_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    ("sleep", "exit", 3, "", ""),
    (
        "sleep",
        "timeout",
        1,
        "TIMED-OUT waits (main_test.chz) test exceeded --timeout (200ms)\n\n1 test(s): 0 passed, 0 failed, 0 errored, 1 timed out\n",
        "",
    ),
    ("recv", "fault", 0, "defer ran\n!boom\n", ""),
    (
        "recv",
        "ex_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    (
        "recv",
        "task_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    ("recv", "exit", 3, "", ""),
    (
        "recv",
        "timeout",
        1,
        "TIMED-OUT waits (main_test.chz) test exceeded --timeout (200ms)\n\n1 test(s): 0 passed, 0 failed, 0 errored, 1 timed out\n",
        "",
    ),
    ("send", "fault", 0, "defer ran\n!boom\n", ""),
    (
        "send",
        "ex_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    (
        "send",
        "task_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    ("send", "exit", 3, "", ""),
    (
        "send",
        "timeout",
        1,
        "TIMED-OUT waits (main_test.chz) test exceeded --timeout (200ms)\n\n1 test(s): 0 passed, 0 failed, 0 errored, 1 timed out\n",
        "",
    ),
    ("wait", "fault", 0, "defer ran\n!boom\n", ""),
    (
        "wait",
        "ex_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    (
        "wait",
        "task_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    ("wait", "exit", 3, "", ""),
    (
        "wait",
        "timeout",
        1,
        "TIMED-OUT waits (main_test.chz) test exceeded --timeout (200ms)\n\n1 test(s): 0 passed, 0 failed, 0 errored, 1 timed out\n",
        "",
    ),
    ("guard", "fault", 0, "defer ran\n!boom\n", ""),
    (
        "guard",
        "ex_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    (
        "guard",
        "task_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    ("guard", "exit", 3, "", ""),
    (
        "guard",
        "timeout",
        1,
        "TIMED-OUT waits (main_test.chz) test exceeded --timeout (200ms)\n\n1 test(s): 0 passed, 0 failed, 0 errored, 1 timed out\n",
        "",
    ),
    // Not base: base left the nested Executor's job parked and ended in a deadlock (exit 1). The
    // fault's cancel now reaches it through the flag its Executor inherited (owner, 2026-11-21).
    ("exec_join", "fault", 0, "defer ran\n!boom\n", ""),
    (
        "exec_join",
        "ex_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    (
        "exec_join",
        "task_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    ("exec_join", "exit", 3, "", ""),
    (
        "exec_join",
        "timeout",
        1,
        "TIMED-OUT waits (main_test.chz) test exceeded --timeout (200ms)\n\n1 test(s): 0 passed, 0 failed, 0 errored, 1 timed out\n",
        "",
    ),
    ("nursery_join", "fault", 0, "defer ran\n!boom\n", ""),
    (
        "nursery_join",
        "ex_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    (
        "nursery_join",
        "task_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    ("nursery_join", "exit", 3, "", ""),
    (
        "nursery_join",
        "timeout",
        1,
        "TIMED-OUT waits (main_test.chz) test exceeded --timeout (200ms)\n\n1 test(s): 0 passed, 0 failed, 0 errored, 1 timed out\n",
        "",
    ),
    ("d_recv", "fault", 0, "defer ran\n!boom\n", ""),
    (
        "d_recv",
        "ex_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    (
        "d_recv",
        "task_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    ("d_recv", "exit", 3, "", ""),
    (
        "d_recv",
        "timeout",
        1,
        "TIMED-OUT waits (main_test.chz) test exceeded --timeout (200ms)\n\n1 test(s): 0 passed, 0 failed, 0 errored, 1 timed out\n",
        "",
    ),
    ("d_send", "fault", 0, "defer ran\n!boom\n", ""),
    (
        "d_send",
        "ex_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    (
        "d_send",
        "task_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    ("d_send", "exit", 3, "", ""),
    (
        "d_send",
        "timeout",
        1,
        "TIMED-OUT waits (main_test.chz) test exceeded --timeout (200ms)\n\n1 test(s): 0 passed, 0 failed, 0 errored, 1 timed out\n",
        "",
    ),
    ("d_wait", "fault", 0, "defer ran\n!boom\n", ""),
    (
        "d_wait",
        "ex_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    (
        "d_wait",
        "task_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    ("d_wait", "exit", 3, "", ""),
    (
        "d_wait",
        "timeout",
        1,
        "TIMED-OUT waits (main_test.chz) test exceeded --timeout (200ms)\n\n1 test(s): 0 passed, 0 failed, 0 errored, 1 timed out\n",
        "",
    ),
    ("d_sleep", "fault", 0, "defer ran\n!boom\n", ""),
    (
        "d_sleep",
        "ex_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    (
        "d_sleep",
        "task_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    ("d_sleep", "exit", 3, "", ""),
    (
        "d_sleep",
        "timeout",
        1,
        "TIMED-OUT waits (main_test.chz) test exceeded --timeout (200ms)\n\n1 test(s): 0 passed, 0 failed, 0 errored, 1 timed out\n",
        "",
    ),
    ("d_guard", "fault", 0, "defer ran\n!boom\n", ""),
    (
        "d_guard",
        "ex_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    (
        "d_guard",
        "task_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    ("d_guard", "exit", 3, "", ""),
    (
        "d_guard",
        "timeout",
        1,
        "TIMED-OUT waits (main_test.chz) test exceeded --timeout (200ms)\n\n1 test(s): 0 passed, 0 failed, 0 errored, 1 timed out\n",
        "",
    ),
    // Not base, as `exec_join/fault`: base ended in the same deadlock (exit 1).
    ("d_exec_join", "fault", 0, "defer ran\n!boom\n", ""),
    (
        "d_exec_join",
        "ex_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    (
        "d_exec_join",
        "task_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    ("d_exec_join", "exit", 3, "", ""),
    (
        "d_exec_join",
        "timeout",
        1,
        "TIMED-OUT waits (main_test.chz) test exceeded --timeout (200ms)\n\n1 test(s): 0 passed, 0 failed, 0 errored, 1 timed out\n",
        "",
    ),
    ("d_nursery_join", "fault", 0, "defer ran\n!boom\n", ""),
    (
        "d_nursery_join",
        "ex_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    (
        "d_nursery_join",
        "task_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    ("d_nursery_join", "exit", 3, "", ""),
    (
        "d_nursery_join",
        "timeout",
        1,
        "TIMED-OUT waits (main_test.chz) test exceeded --timeout (200ms)\n\n1 test(s): 0 passed, 0 failed, 0 errored, 1 timed out\n",
        "",
    ),
    ("d_socket", "fault", 0, "defer ran\n!boom\n", ""),
    (
        "d_socket",
        "ex_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    (
        "d_socket",
        "task_cancel",
        0,
        "defer ran\n!task cancelled: cancel() stopped it before it finished\n",
        "",
    ),
    ("d_socket", "exit", 3, "", ""),
    (
        "d_socket",
        "timeout",
        1,
        "TIMED-OUT waits (main_test.chz) test exceeded --timeout (200ms)\n\n1 test(s): 0 passed, 0 failed, 0 errored, 1 timed out\n",
        "",
    ),
    ("sleep", "backlog", 1, BACKLOG_OUT, ""),
    ("recv", "backlog", 1, BACKLOG_OUT, ""),
    ("send", "backlog", 1, BACKLOG_OUT, ""),
    ("wait", "backlog", 1, BACKLOG_OUT, ""),
    ("guard", "backlog", 1, BACKLOG_OUT, ""),
    ("exec_join", "backlog", 1, BACKLOG_OUT, ""),
    ("nursery_join", "backlog", 1, BACKLOG_OUT, ""),
    ("d_recv", "backlog", 1, BACKLOG_OUT, ""),
    ("d_send", "backlog", 1, BACKLOG_OUT, ""),
    ("d_wait", "backlog", 1, BACKLOG_OUT, ""),
    ("d_sleep", "backlog", 1, BACKLOG_OUT, ""),
    ("d_guard", "backlog", 1, BACKLOG_OUT, ""),
    ("d_exec_join", "backlog", 1, BACKLOG_OUT, ""),
    ("d_nursery_join", "backlog", 1, BACKLOG_OUT, ""),
    ("d_socket", "backlog", 1, BACKLOG_OUT, ""),
    ("sleep", "job_exit", 3, "", ""),
    ("recv", "job_exit", 3, "", ""),
    ("send", "job_exit", 3, "", ""),
    ("wait", "job_exit", 3, "", ""),
    ("guard", "job_exit", 3, "", ""),
    ("exec_join", "job_exit", 3, "", ""),
    ("nursery_join", "job_exit", 3, "", ""),
    ("d_recv", "job_exit", 3, "", ""),
    ("d_send", "job_exit", 3, "", ""),
    ("d_wait", "job_exit", 3, "", ""),
    ("d_sleep", "job_exit", 3, "", ""),
    ("d_guard", "job_exit", 3, "", ""),
    ("d_exec_join", "job_exit", 3, "", ""),
    ("d_nursery_join", "job_exit", 3, "", ""),
    ("d_socket", "job_exit", 3, "", ""),
    ("sleep", "main_heap", 1, BACKLOG_OUT, ""),
    ("recv", "main_heap", 1, BACKLOG_OUT, ""),
    ("send", "main_heap", 1, BACKLOG_OUT, ""),
    ("wait", "main_heap", 1, BACKLOG_OUT, ""),
    ("guard", "main_heap", 1, BACKLOG_OUT, ""),
    ("exec_join", "main_heap", 1, BACKLOG_OUT, ""),
    ("nursery_join", "main_heap", 1, BACKLOG_OUT, ""),
    ("d_recv", "main_heap", 1, BACKLOG_OUT, ""),
    ("d_send", "main_heap", 1, BACKLOG_OUT, ""),
    ("d_wait", "main_heap", 1, BACKLOG_OUT, ""),
    ("d_sleep", "main_heap", 1, BACKLOG_OUT, ""),
    ("d_guard", "main_heap", 1, BACKLOG_OUT, ""),
    ("d_exec_join", "main_heap", 1, BACKLOG_OUT, ""),
    ("d_nursery_join", "main_heap", 1, BACKLOG_OUT, ""),
    ("d_socket", "main_heap", 1, BACKLOG_OUT, ""),
];
