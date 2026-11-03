//! The steps a wait on a VM host thread takes (detach from the GC, release the run permit, demote)
//! come from the wait's kind and the block context. One cell is one wait kind in one context at one
//! worker count. Each cell checks liveness: a sibling task makes progress while the wait is held.
//!
//! - `print`: task A writes 200000 bytes into a FIFO and blocks with the Writer's lock held; the
//!   printer renders the same Writer after 50 ms; the reader drains the FIFO after 200 ms. Go (a
//!   `sync.Mutex` held across the write, `GOMAXPROCS=1`) prints both lines: a goroutine blocked
//!   on a mutex holds no P.
//!
//! Contexts: `fiber` (a `spawn` task), `fiber_cb` (the same inside a `[0].map` callback), `main`
//! (the `parallel:` body or top-level `main`), `job` (an `Executor` job).
//!
//! Each run is its own process and must exit 0 with its golden stdout within [`LIMIT`]. A run that
//! outlives the bound is killed and FAILS. It is never retried.

#[path = "support/grid_tests.rs"]
#[macro_use]
mod grid_tests;

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The bound on one run. A live run ends in about 0.3 s.
const LIMIT: Duration = Duration::from_secs(30);

const PRINT_PRELUDE: &str = "import std.io as io
import std.time as time
import Executor from std.concurrency

fn must[T](r: T!) -> T:
    match r:
        ?v: return v
        !e: panic(e.message())

fn writer(path: str, share: Channel[io.Writer]):
    w := must(io.create(path))
    share.send(w)
    _ := must(w.write(\"x\".repeat(200000)))
    match w.close():
        ?_: pass
        !e: panic(e.message())

fn show(w: io.Writer):
    time.sleep_ms(50)
    s := \"{w}\"
    print(\"printer done\", s.len() > 0)

fn show_cb(w: io.Writer) -> int:
    show(w)
    return 0

fn reader(path: str, got: Channel[int]):
    r := must(io.open(path))
    time.sleep_ms(200)
    total := 0
    while true:
        b := must(r.read_bytes(4096))
        if b.len() == 0:
            break
        total += b.len()
    got.send(total)

share := Channel[io.Writer](1)
got := Channel[int](1)
";

/// The program body of `op` in `ctx`.
fn body(op: &str, ctx: &str) -> &'static str {
    match (op, ctx) {
        ("print", "fiber") => {
            "parallel:\n    spawn writer(\"@FIFO@\", share)\n    spawn:\n        show(share.recv())\n    spawn reader(\"@FIFO@\", got)\nprint(\"reader got\", got.recv())\n"
        }
        ("print", "fiber_cb") => {
            "parallel:\n    spawn writer(\"@FIFO@\", share)\n    spawn:\n        _ := [share.recv()].map(show_cb)\n    spawn reader(\"@FIFO@\", got)\nprint(\"reader got\", got.recv())\n"
        }
        ("print", "main") => {
            "ex := Executor()\n_ := ex.submit(fn(): writer(\"@FIFO@\", share))\n_ := ex.submit(fn(): reader(\"@FIFO@\", got))\nshow(share.recv())\nprint(\"reader got\", got.recv())\nex.shutdown()\n"
        }
        ("print", "job") => {
            "ex := Executor()\n_ := ex.submit(fn(): show(share.recv()))\nparallel:\n    spawn writer(\"@FIFO@\", share)\n    spawn reader(\"@FIFO@\", got)\nex.shutdown()\nprint(\"reader got\", got.recv())\n"
        }
        (op, ctx) => panic!("unknown cell {op}/{ctx}"),
    }
}

/// The whole program of `op` in `ctx` and its golden stdout (the Go output).
fn source(op: &str, ctx: &str) -> (String, &'static str) {
    let (prelude, want) = match op {
        "print" => (PRINT_PRELUDE, "printer done true\nreader got 200000\n"),
        other => panic!("unknown op {other}"),
    };
    (format!("{prelude}\n{}", body(op, ctx)), want)
}

fn assert_cell(op: &str, ctx: &str, threads: &str) {
    let (src, want) = source(op, ctx);
    let name = format!("{op}-{ctx}-{threads}");
    let dir = std::env::temp_dir().join(format!("chz-wait-steps-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let fifo = dir.join("fifo");
    let made = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "mkfifo {}", fifo.display());
    let path = dir.join("main.chz");
    std::fs::write(&path, src.replace("@FIFO@", &fifo.to_string_lossy())).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("CHEZZI_GC_STRESS");
    match threads {
        "t1" => {
            cmd.env("CHEZZI_THREADS", "1");
        }
        "t2" => {
            cmd.env("CHEZZI_THREADS", "2");
        }
        "tdefault" => {
            cmd.env_remove("CHEZZI_THREADS");
        }
        other => panic!("unknown thread count {other}"),
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
    let Some(status) = status else {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("{name}: no exit within {LIMIT:?} (the wait kept its run permit)");
    };
    let (mut out, mut err) = (String::new(), String::new());
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut out);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        status.code(),
        Some(0),
        "{name}: exit code\nstdout:\n{out}\nstderr:\n{err}"
    );
    assert_eq!(out, want, "{name}: stdout\nstderr:\n{err}");
}

grid_tests!(OPS x CTXS x THREADS: [print] x [fiber, fiber_cb, main, job] x [t1, t2, tdefault] => crate::assert_cell);
