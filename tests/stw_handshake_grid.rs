//! M1.5 — the stop-the-world handshake at every blocking mode: one small program per mode, run with
//! `CHEZZI_GC_STRESS=1` (every instruction boundary requests a collection, so every GC stops every
//! attached thread) at `CHEZZI_THREADS=1`, `2` and the default. A thread that blocks while it holds
//! its GC token never reaches a safepoint, so the next stop waits for it forever: such a program
//! does not finish. Each run must exit 0 with its golden stdout inside the bound, and the
//! `CHEZZI_HEAP_STATS` line must count the stops (`stw=N`, N > 0).
//!
//! A run that outlives the bound is killed and FAILS. It is never retried: a retry would hide a
//! missed detach that hangs one run in N.

#[path = "support/grid_tests.rs"]
#[macro_use]
mod grid_tests;

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The bound on one run. The slowest cell takes a few seconds in a debug build.
const LIMIT: Duration = Duration::from_secs(60);

/// A busy loop each program shares: it runs enough instructions under stress to request many
/// stops while another thread blocks.
const BUSY: &str = "fn busy(n: int) -> int:
    s := 0
    i := 0
    while i < 50:
        s = s + i * n
        i = i + 1
    return s
";

const MUST: &str = "fn must[T](r: T!) -> T:
    match r:
        ?v: return v
        !e: panic(e.message())
";

/// (mode, program body after the shared prelude, golden stdout)
fn program(mode: &str) -> (String, &'static str) {
    let (imports, body, want) = match mode {
        // A recv inside a native callback on an M:N worker: the worker blocks in place.
        "demote" => (
            "",
            "fn main():
    ch := Channel[int]()
    out := Channel[int](1)
    parallel:
        spawn:
            xs := [1, 2, 3].map(fn(n: int) -> int: n + ch.recv())
            out.send(xs[0] + xs[1] + xs[2])
        spawn:
            _ := busy(1)
            ch.send(10)
            ch.send(20)
            ch.send(30)
    print(out.recv())
main()
",
            "66\n",
        ),
        // `main` blocks on its own thread in a recv that an Executor job feeds.
        "in_place" => (
            "import Executor from std.concurrency\n",
            "fn main():
    ch := Channel[int]()
    ex := Executor()
    ex.submit(fn(): ch.send(busy(1)))
    print(ch.recv())
    ex.shutdown()
main()
",
            "1225\n",
        ),
        // `main` joins its outermost `parallel:` and may run the fibers on its own stack.
        "help" => (
            "",
            "fn work(n: int, out: Channel[int]):
    out.send(busy(n))
fn main():
    out := Channel[int](3)
    parallel:
        spawn work(1, out)
        spawn work(2, out)
        spawn work(3, out)
    print(out.recv() + out.recv() + out.recv())
main()
",
            "7350\n",
        ),
        // `main` sleeps on its own thread while an Executor job runs.
        "inline_sleep" => (
            "import Executor from std.concurrency\nimport std.time\n",
            "fn main():
    out := Channel[int](1)
    ex := Executor()
    ex.submit(fn(): out.send(busy(2)))
    time.sleep_ms(30)
    _ := time.timer(10).recv()
    print(out.recv())
    ex.shutdown()
main()
",
            "2450\n",
        ),
        // Two fibers hand values over an unbuffered channel.
        "rendezvous" => (
            "",
            "fn ping(a: Channel[int], b: Channel[int]):
    i := 0
    while i < 20:
        a.send(i)
        _ := b.recv()
        i = i + 1
fn pong(a: Channel[int], b: Channel[int], out: Channel[int]):
    s := 0
    i := 0
    while i < 20:
        s = s + a.recv()
        b.send(0)
        i = i + 1
    out.send(s)
fn main():
    a := Channel[int](0)
    b := Channel[int](0)
    out := Channel[int](1)
    parallel:
        spawn ping(a, b)
        spawn pong(a, b, out)
    print(out.recv())
main()
",
            "190\n",
        ),
        // `main` waits in `shutdown()` for two running jobs.
        "executor_join" => (
            "import Executor from std.concurrency\n",
            "fn main():
    out := Channel[int](2)
    ex := Executor()
    ex.submit(fn(): out.send(busy(1)))
    ex.submit(fn(): out.send(busy(2)))
    ex.shutdown()
    print(out.recv() + out.recv())
main()
",
            "3675\n",
        ),
        // A fiber joins a nested `parallel:` it opened.
        "nursery_join" => (
            "",
            "fn work(n: int, out: Channel[int]):
    out.send(busy(n))
fn main():
    out := Channel[int](2)
    parallel:
        spawn:
            parallel:
                spawn work(1, out)
                spawn work(2, out)
    print(out.recv() + out.recv())
main()
",
            "3675\n",
        ),
        // Two fibers talk over a loopback socket: the waits park on the netpoller.
        "socket_park" => (
            "import std.net\n",
            "fn main():
    ln := must(net.listen(\"127.0.0.1:0\"))
    addr := must(ln.addr())
    out := Channel[str](1)
    parallel:
        spawn:
            peer := must(ln.accept())
            out.send(must(peer.read(5)))
        spawn:
            _ := busy(1)
            c := must(net.connect(addr))
            _ := c.write(\"hello\")
    print(out.recv())
main()
",
            "hello\n",
        ),
        // `main` accepts and reads a loopback socket on its own thread; an Executor job writes.
        "socket_in_place" => (
            "import Executor from std.concurrency\nimport std.net\n",
            "fn writer(addr: str):
    _ := busy(1)
    c := must(net.connect(addr))
    _ := c.write(\"hello\")
fn main():
    ln := must(net.listen(\"127.0.0.1:0\"))
    addr := must(ln.addr())
    ex := Executor()
    ex.submit(fn(): writer(addr))
    peer := must(ln.accept())
    print(must(peer.read(5)))
    ex.shutdown()
main()
",
            "hello\n",
        ),
        other => panic!("unknown mode {other}"),
    };
    (format!("{imports}{MUST}{BUSY}{body}"), want)
}

/// Runs `src` with stress and stats on; `None` = killed at [`LIMIT`].
fn run_stressed(name: &str, src: &str, threads: Option<&str>) -> Option<(i32, String, String)> {
    let dir = std::env::temp_dir().join(format!("chz-stw-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(&path, src).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run")
        .arg(&path)
        .env("CHEZZI_GC_STRESS", "1")
        .env("CHEZZI_HEAP_STATS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match threads {
        Some(t) => {
            cmd.env("CHEZZI_THREADS", t);
        }
        None => {
            cmd.env_remove("CHEZZI_THREADS");
        }
    }
    let mut child = cmd.spawn().expect("spawn chezzi");
    let deadline = Instant::now() + LIMIT;
    let status = loop {
        if let Some(st) = child.try_wait().expect("try_wait") {
            break Some(st);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let status = status?;
    let (mut out, mut err) = (String::new(), String::new());
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut out);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    let _ = std::fs::remove_dir_all(&dir);
    Some((status.code().unwrap_or(-1), out, err))
}

/// The stop count from the `[heap-stats]` line, `None` when the line has no `stw=`.
fn stw_count(stderr: &str) -> Option<u64> {
    let line = stderr.lines().find(|l| l.starts_with("[heap-stats]"))?;
    let at = line.find("stw=")? + "stw=".len();
    let digits: String = line[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn assert_cell(mode: &str, threads: &str) {
    let (src, want) = program(mode);
    let t = match threads {
        "t1" => Some("1"),
        "t2" => Some("2"),
        "tdefault" => None,
        other => panic!("unknown thread count {other}"),
    };
    let name = format!("{mode}-{threads}");
    let Some((code, out, err)) = run_stressed(&name, &src, t) else {
        panic!(
            "{name}: no exit within {LIMIT:?} under CHEZZI_GC_STRESS=1 (a blocked thread kept its GC token)"
        );
    };
    assert_eq!(code, 0, "{name}: exit code\nstdout:\n{out}\nstderr:\n{err}");
    assert_eq!(out, want, "{name}: stdout\nstderr:\n{err}");
    let stw = stw_count(&err);
    assert!(
        stw.is_some_and(|n| n > 0),
        "{name}: the [heap-stats] line counts no stop (got {stw:?})\nstderr:\n{err}"
    );
}

grid_tests!(MODES x THREADS: [demote, in_place, help, inline_sleep, rendezvous, executor_join, nursery_join, socket_park, socket_in_place] x [t1, t2, tdefault] => crate::assert_cell);
