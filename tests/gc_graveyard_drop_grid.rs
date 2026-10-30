//! M5 — a sweep's graveyard drops through the GC door. A freed object whose `Drop` makes a syscall
//! (`Heap::take_graveyard`) drops after the world restarts, on the collecting thread. A garbage
//! writer with an unflushed tail flushes it in that drop, and the write blocks while the FIFO is
//! full. If the thread is still attached, the next stop waits for it. The FIFO's reader is the same
//! run's `main`, and its next instruction requests that stop: the run hangs.
//!
//! One program per graveyard kind whose drop can block. An Executor job writes 60000 bytes into a
//! FIFO, then leaves a tail (7000 or 10000 bytes) in a writer that becomes garbage, so the drop's
//! flush passes the 65536-byte pipe buffer. `main` reads the FIFO 300 ms after the job signals.
//! `CHEZZI_GC_STRESS=1` collects at every instruction, so the job's next instructions sweep the
//! writer. Each run is its own process and must exit 0 with its golden stdout within [`LIMIT`].
//! A run that outlives the bound is killed and FAILS. It is never retried.
//!
//! No `t1` cell: at one worker the job's blocked write keeps the one run permit (`vm/width.rs`), so
//! `main` cannot run to drain the FIFO. An explicit `flush()` hangs there too, on main before M5.
//!
//! The other graveyard kinds (`is_arc_core` in `vm/heap_core.rs`) and why none blocks in its drop:
//! a `Reader`, `Socket` or `Listener` drop closes an fd (no linger is set); a `Channel`, `Shared`,
//! `RwShared`, `Atomic` or `AtomicInt` drop frees memory; an `Executor` drop detaches its threads
//! (a `JoinHandle` drop does not join). A `Cffi` drop runs `dlclose`, which runs the library's C
//! destructors: those can block, and the same door covers them, but no program here builds one.

#[path = "support/grid_tests.rs"]
#[macro_use]
mod grid_tests;

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The bound on one run. A fixed run ends in about 1 s in a debug build.
const LIMIT: Duration = Duration::from_secs(60);

const PRELUDE: &str = "import std.io as io
import std.time as time
import Executor from std.concurrency

fn must[T](r: T!) -> T:
    match r:
        ?v: return v
        !e: panic(e.message())

fn churn(n: int) -> int:
    s := 0
    for i in 0..n:
        xs := [i, i + 1, i + 2]
        s += xs.len()
    return s
";

/// `main`: start the writer job, read the FIFO 300 ms after the job signals, print both results.
const MAIN: &str = "
fn main():
    path := \"@FIFO@\"
    ready := Channel[bool](1)
    done := Channel[str](1)
    ex := Executor()
    _ := ex.submit(fn(): writer(path, ready, done))
    r := must(io.open(path))
    _ := ready.recv()
    time.sleep_ms(300)
    total := 0
    while total < @TOTAL@:
        b := must(r.read_bytes(1000))
        if b.len() == 0:
            break
        total += b.len()
    print(\"reader got\", total)
    print(done.recv())
    ex.shutdown()
main()
";

/// (writer half of the program, the bytes the FIFO carries)
fn program(kind: &str) -> (&'static str, u32) {
    match kind {
        // A plain `create` writer: its `BufWriter` holds a 7000-byte tail.
        "file" => (
            "fn leave_tail(path: str, ready: Channel[bool]):
    w := must(io.create(path))
    _ := must(w.write(\"x\".repeat(60000)))
    _ := must(w.write(\"y\".repeat(7000)))
    ready.send(true)

fn writer(path: str, ready: Channel[bool], done: Channel[str]):
    leave_tail(path, ready)
    _ := churn(200)
    done.send(\"writer done\")
",
            67000,
        ),
        // A `buffered` writer over a live file writer: `WriterCore::drop` writes its tail.
        "buffered" => (
            "fn leave_tail(w: io.Writer, ready: Channel[bool]):
    b := io.buffered(w, 100000)
    _ := must(b.write(\"y\".repeat(10000)))
    ready.send(true)

fn writer(path: str, ready: Channel[bool], done: Channel[str]):
    w := must(io.create(path))
    _ := must(w.write(\"x\".repeat(60000)))
    leave_tail(w, ready)
    _ := churn(200)
    done.send(\"writer done\")
",
            70000,
        ),
        // `buffered(buffered(w))`: the outer drop moves the tail into the inner, whose drop writes it.
        "nested" => (
            "fn leave_tail(w: io.Writer, ready: Channel[bool]):
    b := io.buffered(io.buffered(w, 100000), 100000)
    _ := must(b.write(\"y\".repeat(10000)))
    ready.send(true)

fn writer(path: str, ready: Channel[bool], done: Channel[str]):
    w := must(io.create(path))
    _ := must(w.write(\"x\".repeat(60000)))
    leave_tail(w, ready)
    _ := churn(200)
    done.send(\"writer done\")
",
            70000,
        ),
        other => panic!("unknown kind {other}"),
    }
}

fn assert_cell(kind: &str, threads: &str) {
    let (writer, total) = program(kind);
    let name = format!("{kind}-{threads}");
    let dir = std::env::temp_dir().join(format!("chz-graveyard-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let fifo = dir.join("fifo");
    let made = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "mkfifo {}", fifo.display());
    let src = format!("{PRELUDE}\n{writer}{MAIN}")
        .replace("@FIFO@", &fifo.to_string_lossy())
        .replace("@TOTAL@", &total.to_string());
    let path = dir.join("main.chz");
    std::fs::write(&path, src).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run")
        .arg(&path)
        .env("CHEZZI_GC_STRESS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match threads {
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
        panic!(
            "{name}: no exit within {LIMIT:?} (a garbage writer's drop blocked while its thread held its GC token)"
        );
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
    assert_eq!(
        out,
        format!("reader got {total}\nwriter done\n"),
        "{name}: stdout\nstderr:\n{err}"
    );
}

grid_tests!(KINDS x THREADS: [file, buffered, nested] x [t2, tdefault] => crate::assert_cell);
