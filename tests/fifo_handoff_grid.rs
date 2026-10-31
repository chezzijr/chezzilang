//! A `Writer`/`Reader` syscall on a FIFO hands its runner slot to a replacement while it blocks, as a
//! stdin read does (Go: a blocking syscall hands off its P). Before, the thread kept its run permit
//! inside the syscall, so at one worker the task on the other end of the FIFO never ran: the run hung.
//!
//! - `write_full`: one task writes 200000 bytes (more than a pipe holds) into a FIFO, another drains it.
//! - `read_empty`: the reader opens the FIFO, signals the writer, and reads the empty FIFO; the writer
//!   writes 100 bytes only after the signal.
//! - `flush_full`: the 200000 bytes wait in an `io.buffered` writer and reach the FIFO at `flush()`.
//! - `exec_write_full`: an `Executor` job writes, `main` drains.
//!
//! Each run is its own process and must exit 0 with its golden stdout within [`LIMIT`]. A run that
//! outlives the bound is killed and FAILS. It is never retried.

#[path = "support/grid_tests.rs"]
#[macro_use]
mod grid_tests;

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The bound on one run. A fixed run ends in well under 1 s in a debug build.
const LIMIT: Duration = Duration::from_secs(60);

const PRELUDE: &str = "import std.io as io
import Executor from std.concurrency

fn must[T](r: T!) -> T:
    match r:
        ?v: return v
        !e: panic(e.message())

fn close_it(w: io.Writer):
    match w.close():
        ?_: pass
        !e: panic(e.message())

fn drain(r: io.Reader) -> int:
    total := 0
    while true:
        b := must(r.read_bytes(4096))
        if b.len() == 0:
            break
        total += b.len()
    return total

fn reader(path: str, got: Channel[int]):
    got.send(drain(must(io.open(path))))
";

const WRITE_FULL: &str = "fn writer(path: str):
    w := must(io.create(path))
    _ := must(w.write(\"x\".repeat(200000)))
    close_it(w)

got := Channel[int](1)
parallel:
    spawn writer(\"@FIFO@\")
    spawn reader(\"@FIFO@\", got)
print(\"reader got\", got.recv())
";

const READ_EMPTY: &str = "fn writer(path: str, go: Channel[bool]):
    w := must(io.create(path))
    _ := go.recv()
    _ := must(w.write(\"x\".repeat(100)))
    close_it(w)

fn first_reader(path: str, go: Channel[bool], got: Channel[int]):
    r := must(io.open(path))
    go.send(true)
    got.send(drain(r))

go := Channel[bool](1)
got := Channel[int](1)
parallel:
    spawn first_reader(\"@FIFO@\", go, got)
    spawn writer(\"@FIFO@\", go)
print(\"reader got\", got.recv())
";

const FLUSH_FULL: &str = "fn writer(path: str):
    w := must(io.create(path))
    b := io.buffered(w, 300000)
    _ := must(b.write(\"x\".repeat(200000)))
    match b.flush():
        ?_: pass
        !e: panic(e.message())
    close_it(w)

got := Channel[int](1)
parallel:
    spawn writer(\"@FIFO@\")
    spawn reader(\"@FIFO@\", got)
print(\"reader got\", got.recv())
";

const EXEC_WRITE_FULL: &str = "fn writer(path: str):
    w := must(io.create(path))
    _ := must(w.write(\"x\".repeat(200000)))
    close_it(w)

ex := Executor()
ex.submit(fn(): writer(\"@FIFO@\"))
n := drain(must(io.open(\"@FIFO@\")))
ex.shutdown()
print(\"reader got\", n)
";

/// The whole program of `kind` and its golden stdout.
fn source(kind: &str) -> (String, &'static str) {
    let body = match kind {
        "write_full" => (WRITE_FULL, "reader got 200000\n"),
        "read_empty" => (READ_EMPTY, "reader got 100\n"),
        "flush_full" => (FLUSH_FULL, "reader got 200000\n"),
        "exec_write_full" => (EXEC_WRITE_FULL, "reader got 200000\n"),
        other => panic!("unknown kind {other}"),
    };
    (format!("{PRELUDE}\n{}", body.0), body.1)
}

fn assert_cell(kind: &str, threads: &str) {
    let (src, want) = source(kind);
    let name = format!("{kind}-{threads}");
    let dir = std::env::temp_dir().join(format!("chz-fifo-handoff-{name}-{}", std::process::id()));
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
        panic!("{name}: no exit within {LIMIT:?} (a FIFO syscall kept its runner slot)");
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

grid_tests!(KINDS x THREADS: [write_full, read_empty, flush_full, exec_write_full] x [t1, t2, tdefault] => crate::assert_cell);
