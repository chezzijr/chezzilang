//! The run-wide awake count (`quiesce::Awake`, Go's `checkdead` count): every wait kind in every
//! context, as a live program (a sibling ends the wait) and as a `nothing` program (no satisfier).
//! Each run prints `CHEZZI_SCHED_STATS=1`'s `[sched-stats] ... deadlocks=<k>`, where `k` counts the
//! deadlock verdicts: the times the count of awake tasks dropped to 0 while tasks were alive.
//!
//! What a cell expects:
//! - `live`: its golden stdout, exit 0, `deadlocks=0`. One `deadlocks=1` even once is a missed
//!   wake (a parked task made runnable without its +1), never a flake to retry.
//! - `nothing`: the report (`deadlock` on stderr, nonzero exit) and `deadlocks=1`. Main runs as
//!   fiber 1 (plan 2026-11-17 Task 3), so its waits park and count asleep like any task's: at the
//!   wait itself (`main`), at its `parallel:` join (`task`, `callback`, `defer`) or at `t.get()`
//!   (`job`). The count's debug asserts (`awake <= alive` at every write, both 0 at the run's
//!   end) run on every cell of this debug binary.
//! - `nothing` cells of a wait that is awake as in Go (a pending `timer`/`sleep`, a socket
//!   `accept`): they never end by themselves, so they run as a `chezzi test` under `--timeout` and
//!   expect `TIMED-OUT` with `deadlocks=0`.
//! - `stdin` `nothing`: `chezzi test` gives a test no stdin and `chezzi run` has no `--timeout`, so
//!   it runs under `chezzi run` with stdin held open and empty for 400 ms, then closed: the read
//!   ends at EOF, as a Go read in a syscall does, and the cell expects its live stdout with
//!   `deadlocks=0` (a stdin read is awake).
//!
//! Skipped cells (kind x context): none. Every kind runs in every context.
//!
//! Each variant runs at `CHEZZI_THREADS` 1, 2 and 4, unseeded and at `CHEZZI_SCHED_SEED` 1..3.

#[path = "support/grid_tests.rs"]
#[macro_use]
mod grid_tests;

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const PRELUDE: &str = "import std.time
import std.io
import std.net
import Executor, Shared, RwShared from std.concurrency

c := Channel[int](0)
full := Channel[int](1)
gate := Channel[int](1)
held := Channel[int](1)
s := Shared(0)
rw := RwShared[int](0)
ln := lis()

fn lis() -> net.Listener:
    match net.listen(\"127.0.0.1:0\"):
        ?l: return l
        !e: panic(\"{e}\")

fn dial():
    match ln.addr():
        ?a:
            _ := net.connect(a)
        !e: panic(\"{e}\")

fn take() -> int:
    v := c.recv() else: panic(\"c closed\")
    return v

fn blockr():
    _ := c.recv()

fn signal() -> int:
    held.send(1)
    return 0

fn opened() -> int:
    v := gate.recv() else: panic(\"gate closed\")
    return v

fn hold():
    s.update(fn(x: int) -> int: x + signal() + opened())

fn hold_rw():
    rw.write(fn(x: int) -> int: x + signal() + opened())

fn runner():
    v := op()
    print(\"r {v}\")

fn runner_cb():
    ys := [0].map(fn(i: int) -> int: op())
    print(\"r {ys[0]}\")

fn runner_defer():
    defer:
        v := op()
        print(\"r {v}\")
    pass

";

/// The body of `fn op() -> int` for each wait kind, the live feeder's body (run as an Executor
/// job, after 50 ms), and the `r` value a live cell prints.
fn kind(k: &str) -> (&'static str, &'static str, &'static str) {
    match k {
        "recv" => ("    return take()\n", "c.send(1)", "1"),
        "send_full" => (
            "    full.send(1)\n    full.send(2)\n    return 0\n",
            "_ := full.recv()",
            "0",
        ),
        "send_rdv" => ("    c.send(1)\n    return 0\n", "_ := c.recv()", "0"),
        "wait" => (
            "    wait:\n        r := c.recv():\n            v := r else: panic(\"c closed\")\n            return v\n    return 0\n",
            "c.send(1)",
            "1",
        ),
        "timer" => ("    _ := time.timer(T).recv()\n    return 3\n", "pass", "3"),
        "sleep" => ("    time.sleep_ms(T)\n    return 4\n", "pass", "4"),
        "guard" => (
            "    _ := held.recv()\n    s.update(fn(x: int) -> int: x + 1)\n    return s.get()\n",
            "gate.send(1)",
            "2",
        ),
        "rwguard" => (
            "    _ := held.recv()\n    rw.write(fn(x: int) -> int: x + 1)\n    return rw.get()\n",
            "gate.send(1)",
            "2",
        ),
        "nursery" => (
            "    parallel:\n        spawn blockr()\n    return 0\n",
            "c.send(1)",
            "0",
        ),
        "exec_join" => (
            "    ex := Executor(1)\n    _ := ex.submit(fn(): blockr())\n    ex.shutdown()\n    return 0\n",
            "c.send(1)",
            "0",
        ),
        "task_get" => (
            "    ex := Executor(1)\n    t := ex.submit(fn() -> int: take())\n    match t.get():\n        ?v: return v\n        !e: return -1\n",
            "c.send(1)",
            "1",
        ),
        "accept" => (
            "    match ln.accept():\n        ?p: return 5\n        !e: return -1\n",
            "dial()",
            "5",
        ),
        "stdin" => ("    _ := io.read_line()\n    return 6\n", "pass", "6"),
        other => panic!("no kind {other}"),
    }
}

/// Where `op` runs, as the lines of `main`.
fn context(ctx: &str) -> &'static str {
    match ctx {
        "main" => "    v := op()\n    print(\"r {v}\")\n",
        "task" => "    parallel:\n        spawn runner()\n",
        "job" => {
            "    jx := Executor(1)\n    t := jx.submit(fn() -> int: op())\n    match t.get():\n        ?v: print(\"r {v}\")\n        !e: print(\"e {e.error()}\")\n"
        }
        "callback" => "    parallel:\n        spawn runner_cb()\n",
        "defer" => "    parallel:\n        spawn runner_defer()\n",
        other => panic!("no context {other}"),
    }
}

/// A wait that is awake as in Go and that nothing in the run ends: its `nothing` cell runs under
/// `chezzi test --timeout`.
fn awake_wait(k: &str) -> bool {
    matches!(k, "timer" | "sleep" | "accept")
}

/// The program of one cell. `test` wraps `main` in a `test fn` (the `--timeout` cells).
fn program(k: &str, ctx: &str, live: bool, test: bool) -> String {
    let (body, feeder, _) = kind(k);
    // A pending timer or sleep of the `nothing` variant outlives every `--timeout`.
    let body = body.replace("(T)", if live { "(50)" } else { "(600000)" });
    let mut p = format!(
        "{PRELUDE}fn op() -> int:\n{body}\nfn feed():\n    time.sleep_ms(50)\n    {feeder}\n\nfn main():\n"
    );
    match k {
        "guard" => p.push_str("    hx := Executor(1)\n    _ := hx.submit(fn(): hold())\n"),
        "rwguard" => p.push_str("    hx := Executor(1)\n    _ := hx.submit(fn(): hold_rw())\n"),
        _ => {}
    }
    if live {
        p.push_str("    fx := Executor(1)\n    _ := fx.submit(fn(): feed())\n");
    }
    p.push_str(context(ctx));
    if test {
        p.push_str("\ntest fn cell():\n    main()\n    print(\"end\")\n");
    } else {
        p.push_str("\nmain()\nprint(\"end\")\n");
    }
    p
}

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

/// Run one program at `threads` workers and `seed`. `test` runs it as `chezzi test --timeout=400`;
/// `stdin`: `Some(true)` feeds the line `x` 100 ms after the start, `Some(false)` holds stdin open
/// and empty for 400 ms; then it closes. `None` closes it at once. `None` back: still running
/// after 20 s (killed).
fn run(
    src: &str,
    test: bool,
    stdin: Option<bool>,
    threads: &str,
    seed: Option<u32>,
) -> Option<Ran> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("chz-awake-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(if test { "cell_test.chz" } else { "cell.chz" });
    std::fs::write(&path, src).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    if test {
        cmd.arg("test").arg("--timeout=400");
    } else {
        cmd.arg("run");
    }
    cmd.arg(&path)
        .env("CHEZZI_THREADS", threads)
        .env("CHEZZI_SCHED_STATS", "1")
        .env_remove("CHEZZI_SCHED_SEED")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(s) = seed {
        cmd.env("CHEZZI_SCHED_SEED", s.to_string());
    }
    let mut child = cmd.spawn().expect("spawn chezzi");
    let mut w = child.stdin.take().expect("stdin pipe");
    let feeder = std::thread::spawn(move || {
        match stdin {
            Some(true) => {
                std::thread::sleep(Duration::from_millis(100));
                let _ = w.write_all(b"x\n");
            }
            Some(false) => std::thread::sleep(Duration::from_millis(400)),
            None => {}
        }
        drop(w);
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
    let _ = feeder.join();
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

fn cell(k: &str, ctx: &str) {
    let mut misses = Vec::new();
    for live in [true, false] {
        let timed = !live && awake_wait(k);
        let src = program(k, ctx, live, timed);
        let want_r = format!("r {}\nend\n", kind(k).2);
        for threads in ["1", "2", "4"] {
            for seed in [None, Some(1), Some(2), Some(3)] {
                let stdin = (k == "stdin").then_some(live);
                let got = run(&src, timed, stdin, threads, seed);
                let at = format!(
                    "{k} x {ctx} x {} at T={threads} seed={seed:?}",
                    if live { "live" } else { "nothing" }
                );
                let Some(r) = got else {
                    misses.push(format!("{at}: killed after 20 s"));
                    continue;
                };
                let ok = if live || k == "stdin" {
                    r.code == 0 && r.out == want_r
                } else if timed {
                    r.code != 0 && r.out.contains("TIMED-OUT")
                } else {
                    r.code != 0 && r.err.contains("deadlock")
                };
                if !ok {
                    misses.push(format!(
                        "{at}: rc={} deadlocks={:?}\n--stdout--\n{}--stderr--\n{}",
                        r.code,
                        r.deadlocks(),
                        r.out,
                        r.err
                    ));
                } else {
                    // A `nothing` cell's wait is the verdict, except an awake wait's; see the file
                    // comment.
                    let want = usize::from(!live && !timed && k != "stdin");
                    if r.deadlocks() != Some(want) {
                        misses.push(format!(
                            "{at}: deadlocks={:?}, expected Some({want})",
                            r.deadlocks()
                        ));
                    }
                }
            }
        }
    }
    assert!(misses.is_empty(), "{}", misses.join("\n\n"));
}

grid_tests!(KINDS x CTXS: [recv, send_full, send_rdv, wait, timer, sleep, guard, rwguard, nursery,
    exec_join, task_get, accept, stdin] x [main, task, job, callback, defer] => crate::cell);
