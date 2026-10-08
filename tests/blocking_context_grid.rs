//! TICKET-181 (wave 16 Family 2, "Blocking contexts"): each blocking op decides for itself whether
//! it may block in the current execution context. These are the named red cells before the fix;
//! the plan extends this file into the full op × context grid. Each cell runs a fresh process
//! under a hard timeout at T=1, T=2 and T=0 (default), and compares against the RUN Go/CPython
//! result recorded per cell.

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Runs `src` at `threads` workers; `None` = still running after `limit` (killed).
fn run(name: &str, src: &str, threads: &str, limit: Duration) -> Option<(i32, String, String)> {
    run_with(name, src, threads, None, false, limit)
}

/// `run`, plus an optional `CHEZZI_SCHED_SEED` and, with `stdin`, the line `x` fed to the
/// program 100 ms after it starts.
fn run_with(
    name: &str,
    src: &str,
    threads: &str,
    seed: Option<u32>,
    stdin: bool,
    limit: Duration,
) -> Option<(i32, String, String)> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "chz-t181-{name}-{threads}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(&path, src).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", threads)
        .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(s) = seed {
        cmd.env("CHEZZI_SCHED_SEED", s.to_string());
    }
    let mut child = cmd.spawn().expect("spawn chezzi");
    let feeder = child.stdin.take().map(|mut w| {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            let _ = w.write_all(b"x\n");
        })
    });
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s);
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
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
    if let Some(f) = feeder {
        let _ = f.join();
    }
    let _ = std::fs::remove_dir_all(&dir);
    status.map(|s| (s.code().unwrap_or(-1), out, err))
}

fn assert_cell(name: &str, src: &str, want_stdout: &str) {
    for t in ["1", "2", "0"] {
        let got = run(name, src, t, Duration::from_secs(10));
        let Some((code, out, err)) = got else {
            panic!(
                "cell {name} at CHEZZI_THREADS={t}: hang (killed after 10s); expected {want_stdout:?}"
            );
        };
        assert!(
            code == 0 && out == want_stdout,
            "cell {name} at CHEZZI_THREADS={t}: rc={code} stdout={out:?} stderr={err:?}; expected {want_stdout:?}"
        );
    }
}

/// C1: a timed `wait:` in a native callback (`.map`) inside a spawned fiber. Go (`select` with
/// `time.After` in a helper called from a goroutine) prints `[10 20]`-shaped output and exits 0.
#[test]
fn c1_timed_wait_in_fiber_callback_completes() {
    let src = "import std.time\nfn pick(i: int) -> int:\n    wait:\n        _ := time.timer(20).recv(): return i\n    return -1\nfn main():\n    parallel:\n        spawn:\n            print([1, 2].map(pick))\nmain()\n";
    assert_cell("c1", src, "[1, 2]\n");
}

/// X1: `Executor.shutdown()` inside a spawned task; the job needs a sibling fiber to send.
/// Go at GOMAXPROCS=1 and CPython complete.
#[test]
fn x1_executor_shutdown_in_fiber_releases_the_runner() {
    let src = "import std.concurrency\nfn main():\n    ch := Channel[int](0)\n    parallel:\n        spawn:\n            ex := Executor()\n            ex.submit(fn() -> None:\n                print(ch.recv())\n            )\n            ex.shutdown()\n            print(\"done\")\n        spawn:\n            ch.send(7)\nmain()\n";
    assert_cell("x1", src, "7\ndone\n");
}

/// X2: `peer.read(2)` inside a `.map()` callback on the main thread, data arriving 50 ms later
/// from a sibling. CPython and Go block and read; no Executor is involved.
#[test]
fn x2_socket_read_in_main_callback_blocks_and_reads() {
    let src = "import std.net\nimport std.time\nfn show[T](r: Result[T]) -> str:\n    match r:\n        Ok(_): return \"ok\"\n        Err(e): return \"err \" + e.message()\nfn main() -> Result[int]:\n    ln := net.listen(\"127.0.0.1:0\")?\n    c := net.connect(ln.addr()?)?\n    peer := ln.accept()?\n    parallel:\n        spawn:\n            time.sleep_ms(50)\n            _ := c.write(\"hi\")\n        print([2].map(fn(n: int) -> str: show(peer.read(n))))\n    return Ok(0)\n_ := main()\n";
    assert_cell("x2", src, "['ok']\n");
}

// ---- The op × context × satisfier grid (`grid_every_op_in_every_context`) ----

/// Who satisfies the blocked op: a sibling task (or the harness, for stdin), the op's own
/// deadline, or nothing at all.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Sat {
    Sibling,
    Deadline,
    Nothing,
}

/// What the cell must do.
#[derive(Clone, Copy, Debug)]
enum Expect {
    /// exit 0 with exactly this stdout
    Out(&'static str),
    /// exit non-zero with this text on stderr
    Fault(&'static str),
    /// still running at the 2 s probe (a declined verdict: nothing can satisfy it)
    Hang,
}

/// One execution context: `call` is the parallel-body text that reaches `op`.
struct Ctx {
    name: &'static str,
    call: &'static str,
    /// an Executor job
    /// the outcome when nothing can satisfy the op
    nothing: Expect,
}

/// One blocking op: `body` is the text of `fn op`, `sib` the sibling task's body.
struct Op {
    name: &'static str,
    body: &'static str,
    sib: &'static str,
    prelude: &'static str,
    stdin: bool,
    sats: &'static [(Sat, &'static str)],
}

struct Cell {
    name: String,
    src: String,
    stdin: bool,
    seeds: bool,
    expect: Expect,
}

const DEADLOCK: &str = "deadlock";

const PROGRAM: &str = "import std.concurrency
import std.time
import std.io
import std.net
import std.request
import submit_task from std.concurrency.task
ch := Channel[int](0)
fc := Channel[int](1)
s := Shared[int](0)
g := Shared[int](0)
{PRELUDE}
fn must[T](r: Result[T]) -> T:
    match r:
        Ok(v): return v
        Err(e): panic(e.message())
fn show[T](r: Result[T]) -> str:
    match r:
        Ok(_): return \"ok\"
        Err(e): return \"err \" + e.message()
fn op(n: int) -> int:
{OP}
    return 0
fn gen() -> Iterator[int]:
    yield op(0)
fn viadefer():
    defer:
        _ := op(0)
    pass
fn bump(v: int) -> int:
    return v + op(0)
fn job() -> None:
    _ := op(0)
fn job_cb() -> None:
    _ := [0].map(op)
fn inc(v: int) -> int:
    return v + 1
fn hold(v: int) -> int:
    time.sleep_ms(50)
    return v
fn recv_job() -> None:
    print(ch.recv())
fn recv7() -> int:
    return ch.recv()
fn main():
    parallel:
{BODY}main()
";

fn contexts() -> Vec<Ctx> {
    let ctx = |name, call, _demote: bool, _job: bool, nothing| Ctx {
        name,
        call,
        nothing,
    };
    let dead = Expect::Fault(DEADLOCK);
    vec![
        ctx("main", "        _ := op(0)\n", false, false, dead),
        ctx(
            "main_cb",
            "        _ := [0].map(op)\n",
            false,
            false,
            Expect::Hang,
        ),
        ctx("main_defer", "        viadefer()\n", false, false, dead),
        ctx(
            "fiber",
            "        spawn:\n            _ := op(0)\n",
            false,
            false,
            dead,
        ),
        ctx(
            "fiber_cb",
            "        spawn:\n            _ := [0].map(op)\n",
            true,
            false,
            dead,
        ),
        ctx(
            "fiber_defer",
            "        spawn:\n            viadefer()\n",
            true,
            false,
            dead,
        ),
        ctx(
            "fiber_gen",
            "        spawn:\n            for _ in gen():\n                pass\n",
            true,
            false,
            dead,
        ),
        ctx(
            "fiber_update",
            "        spawn:\n            s.update(bump)\n",
            true,
            false,
            dead,
        ),
        ctx(
            "job",
            "        ex := Executor()\n        ex.submit(job)\n        ex.shutdown()\n",
            false,
            true,
            dead,
        ),
        ctx(
            "job_cb",
            "        ex := Executor()\n        ex.submit(job_cb)\n        ex.shutdown()\n",
            false,
            true,
            dead,
        ),
    ]
}

const SOCK_PRELUDE: &str = "ln := must(net.listen(\"127.0.0.1:0\"))
c := must(net.connect(must(ln.addr())))
peer := must(ln.accept())";
const HTTP_PRELUDE: &str = "hl := must(net.listen(\"127.0.0.1:0\"))
url := \"http://\" + must(hl.addr()) + \"/\"";
const HTTP_SIB: &str = "            conn := must(hl.accept())
            _ := conn.read(1024)
            _ := conn.write(\"HTTP/1.1 200 OK\\r\\nContent-Length: 0\\r\\nConnection: close\\r\\n\\r\\n\")
            conn.close()
";

fn ops() -> Vec<Op> {
    use Sat::*;
    let op = |name, body, sib, prelude, stdin, sats| Op {
        name,
        body,
        sib,
        prelude,
        stdin,
        sats,
    };
    let send7 = "            ch.send(7)\n";
    let drain = "            _ := fc.recv()\n            _ := fc.recv()\n";
    vec![
        op(
            "recv",
            "    print(ch.recv())",
            send7,
            "",
            false,
            &[(Sibling, "7\n"), (Nothing, "")],
        ),
        op(
            "send",
            "    fc.send(1)\n    fc.send(2)\n    print(\"sent\")",
            drain,
            "",
            false,
            &[(Sibling, "sent\n"), (Nothing, "")],
        ),
        op(
            "wait_recv",
            "    wait:\n        v := ch.recv(): print(v)",
            send7,
            "",
            false,
            &[(Sibling, "7\n"), (Nothing, "")],
        ),
        op(
            "wait_timer",
            "    wait:\n        _ := time.timer(20).recv(): print(\"t\")",
            "",
            "",
            false,
            &[(Deadline, "t\n")],
        ),
        op(
            "wait_recv_timer",
            "    wait:\n        v := ch.recv(): print(v)\n        _ := time.timer(20).recv(): print(\"t\")",
            "",
            "",
            false,
            &[(Deadline, "t\n")],
        ),
        op(
            "wait_closed_timer",
            "    cc := Channel[int](1)\n    cc.close()\n    wait:\n        v := cc.recv(): print(v)\n        _ := time.timer(20).recv(): print(\"t\")",
            "",
            "",
            false,
            &[(Deadline, "t\n")],
        ),
        op(
            "wait_send",
            "    fc.send(1)\n    wait:\n        fc.send(2): print(\"sent\")",
            drain,
            "",
            false,
            &[(Sibling, "sent\n"), (Nothing, "")],
        ),
        op(
            "sleep",
            "    time.sleep_ms(20)\n    print(\"slept\")",
            "",
            "",
            false,
            &[(Deadline, "slept\n")],
        ),
        op(
            "stdin",
            "    match io.input(\"\"):\n        Some(l): print(l)\n        None: print(\"eof\")",
            "",
            "",
            true,
            &[(Sibling, "x\n")],
        ),
        op(
            "socket",
            "    print(show(peer.read(2)))",
            "            time.sleep_ms(50)\n            _ := c.write(\"hi\")\n",
            SOCK_PRELUDE,
            false,
            &[(Sibling, "ok\n")],
        ),
        op(
            "shutdown",
            "    ex2 := Executor()\n    ex2.submit(recv_job)\n    ex2.shutdown()\n    print(\"done\")",
            send7,
            "",
            false,
            &[(Sibling, "7\ndone\n")],
        ),
        op(
            "guard",
            "    g.update(inc)\n    print(\"upd\")",
            "            g.update(hold)\n",
            "",
            false,
            &[(Sibling, "upd\n")],
        ),
        op(
            "bare_timer",
            "    _ := time.timer(20).recv()\n    ch.send(3)",
            "            print(ch.recv())\n",
            "",
            false,
            &[(Deadline, "3\n")],
        ),
        op(
            "nested",
            "    parallel:\n        spawn:\n            print(ch.recv())",
            send7,
            "",
            false,
            &[(Sibling, "7\n"), (Nothing, "")],
        ),
        op(
            "task_get",
            "    ex3 := Executor()\n    t := submit_task(ex3, recv7)\n    match t.get():\n        Ok(v): print(v)\n        Err(e): print(e.message())\n    ex3.shutdown()",
            send7,
            "",
            false,
            &[(Sibling, "7\n")],
        ),
        op(
            "http",
            "    match request.get(url, 3000):\n        Ok(r): print(r.status)\n        Err(e): print(\"err \" + e.message())",
            HTTP_SIB,
            HTTP_PRELUDE,
            false,
            &[(Sibling, "200\n")],
        ),
    ]
}

/// The expected outcome of `op` in `ctx` with `sat`: `out`, unless a documented table cell
/// (`docs/concurrency.md` "Blocking-context table") says otherwise.
fn expect(op: &Op, ctx: &Ctx, sat: Sat, out: &'static str) -> Expect {
    // TICKET-185: a full send (or a `wait:` send arm) in a Demote context blocks on its offer, as
    // Go blocks — the old v1 `Refuse` cell is gone, so it takes the generic rule below.
    // A nested nursery's own verdict judges its children: Go faults, and so does every context.
    if op.name == "nested" && sat == Sat::Nothing {
        return Expect::Fault(DEADLOCK);
    }
    match sat {
        Sat::Nothing => ctx.nothing,
        _ => Expect::Out(out),
    }
}

fn cells() -> Vec<Cell> {
    let mut v = Vec::new();
    for ctx in contexts() {
        for op in ops() {
            for &(sat, out) in op.sats {
                // The bare timer's sibling is the parked receiver its fiber sends to.
                let with_sib =
                    (sat == Sat::Sibling || op.name == "bare_timer") && !op.sib.is_empty();
                let sib = if with_sib {
                    format!("        spawn:\n{}", op.sib)
                } else {
                    String::new()
                };
                // A main-thread call blocks the parallel body, so its sibling is spawned first; a
                // fiber's sibling is spawned after it, so at T=1 the op runs before its satisfier.
                let body = if ctx.call.starts_with("        spawn:") {
                    format!("{}{sib}", ctx.call)
                } else {
                    format!("{sib}{}", ctx.call)
                };
                let src = PROGRAM
                    .replace("{OP}", op.body)
                    .replace("{PRELUDE}", op.prelude)
                    .replace("{BODY}", &body);
                v.push(Cell {
                    name: format!("{}/{}/{:?}", op.name, ctx.name, sat),
                    src,
                    stdin: op.stdin,
                    seeds: sat == Sat::Sibling,
                    expect: expect(&op, &ctx, sat, out),
                });
            }
        }
    }
    v
}

/// Runs one cell at every worker count (and seeds 1..3 at T=1/T=2 for a sibling cell); returns
/// one line per miss.
fn check_cell(c: &Cell) -> Vec<String> {
    let mut runs: Vec<(&str, Option<u32>)> = vec![("1", None), ("2", None), ("0", None)];
    if c.seeds {
        for t in ["1", "2"] {
            for s in 1..=3 {
                runs.push((t, Some(s)));
            }
        }
    }
    let limit = match c.expect {
        Expect::Hang => Duration::from_secs(2),
        _ => Duration::from_secs(10),
    };
    let tag = c.name.replace('/', "-");
    let mut misses = Vec::new();
    for (t, seed) in runs {
        let got = run_with(&tag, &c.src, t, seed, c.stdin, limit);
        let ok = match (&got, c.expect) {
            (None, Expect::Hang) => true,
            (Some((0, out, _)), Expect::Out(w)) => out == w,
            (Some((code, _, err)), Expect::Fault(w)) => *code != 0 && err.contains(w),
            _ => false,
        };
        if !ok {
            let what = match got {
                None => "hang".to_string(),
                Some((code, out, err)) => format!(
                    "rc={code} stdout={out:?} stderr={:?}",
                    err.chars().take(160).collect::<String>()
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

/// Every blocking op × every execution context × {sibling, deadline, nothing}, each at T=1, T=2
/// and T=0 (plus seeds 1..3 at T=1/T=2 for a sibling cell), against the Go answer for the cell
/// or a documented table cell. Red on base: C1, E1-E4, X1, X1-cb, X2, F1 and the (d)
/// wait-send-arm cells in main/job callbacks.
#[test]
fn grid_every_op_in_every_context() {
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
