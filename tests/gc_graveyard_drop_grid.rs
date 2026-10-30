//! M5 — a sweep's graveyard drops off the task runners. A freed object whose `Drop` makes a
//! syscall (`Heap::take_graveyard`) drops after the world restarts. A garbage writer with an
//! unflushed tail flushes it in its drop, and the write blocks while the FIFO is full. Run on the
//! collecting thread, the block holds every later stop (M5 before its fix: attached) or stalls the
//! task runner and the next task it takes (M5b: detached). The writer and the `Cffi` kinds go to
//! the reaper thread (`vm/reaper.rs`), which runs no task and never attaches.
//!
//! One program per buffered writer kind: an Executor job writes 60000 bytes into a FIFO, then
//! leaves a tail (7000 or 10000 bytes) in an `io.buffered` writer that becomes garbage, so the
//! drop's flush passes the 65536-byte pipe buffer. `main` reads the FIFO 300 ms after the job
//! signals. `two_jobs` runs two such jobs on two FIFOs and drains only after both signal.
//! `CHEZZI_GC_STRESS=1` collects at every instruction, so the jobs' next instructions sweep the
//! writers. `create_no_gc` (no stress) checks that a plain `create` writer keeps no bytes (M5c).
//! Each run is its own process and must exit 0 with its golden stdout within [`LIMIT`]. A run that
//! outlives the bound is killed and FAILS. It is never retried.
//!
//! M5c: a plain `create`/`append` writer is unbuffered (Go `os.File`), so it leaves no tail, and
//! the `file` kind is gone: a job that writes 67000 bytes into a 65536-byte FIFO before it signals
//! is the user's deadlock (Go's translation hangs the same way).
//!
//! Base (bd5d6403, before M5; release, 60 s cap): `file` (the M5 kind), `buffered` and `nested`
//! hang at T=2, T=4 and default, 9 of 9 runs. HEAD of M5 before the reaper (debug), `two_jobs`
//! with `create` tails: 4 of 4 runs hang at T=2, 2 of 4 at T=4 and 2 of 4 at the default count.
//!
//! No `t1` cell: at one worker the job's blocked write keeps the one run permit (`vm/width.rs`), so
//! `main` cannot run to drain the FIFO. An explicit `flush()` hangs there too, on main before M5.
//!
//! The other graveyard kinds (`heap_core::drop_site`) and why none blocks in its drop: a `Reader`,
//! `Socket` or `Listener` drop closes an fd (no linger is set); a `Channel`, `Shared`, `RwShared`,
//! `Atomic` or `AtomicInt` drop frees memory; an `Executor` drop detaches its threads (a
//! `JoinHandle` drop does not join). They drop in the GC door. A `Cffi` drop runs `dlclose`, which
//! runs the library's C destructors: those can block, so the reaper drops it, but no program here
//! builds one.

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

/// Two Executor jobs, one FIFO each: `main` drains a FIFO only after both jobs signal. A collection
/// on a runner sweeps job 1's garbage writer, and a drop there blocks that runner on the full FIFO.
/// If the runner is to run job 2 next (or runs it), job 2 never signals, and `main` never drains:
/// the run hangs. M5 before the reaper: T=2 4 of 4 runs hang, T=4 2 of 4, default 2 of 4.
const TWO_JOBS: &str = "fn leave_tail(path: str, ready: Channel[bool]):
    w := must(io.create(path))
    _ := must(w.write(\"x\".repeat(60000)))
    b := io.buffered(w, 100000)
    _ := must(b.write(\"y\".repeat(7000)))
    ready.send(true)

fn writer(path: str, ready: Channel[bool], done: Channel[str]):
    leave_tail(path, ready)
    _ := churn(200)
    done.send(\"writer done\")

fn drain(r: io.Reader) -> int:
    total := 0
    while total < 67000:
        b := must(r.read_bytes(1000))
        if b.len() == 0:
            break
        total += b.len()
    return total

fn main():
    ready := Channel[bool](2)
    done := Channel[str](2)
    ex := Executor()
    _ := ex.submit(fn(): writer(\"@FIFO@\", ready, done))
    _ := ex.submit(fn(): writer(\"@FIFO2@\", ready, done))
    r1 := must(io.open(\"@FIFO@\"))
    r2 := must(io.open(\"@FIFO2@\"))
    _ := ready.recv()
    _ := ready.recv()
    time.sleep_ms(300)
    a := drain(r1)
    b := drain(r2)
    print(\"reader got\", a, b)
    print(done.recv())
    print(done.recv())
    ex.shutdown()
main()
";

/// M5c — a job writes 100 bytes into a FIFO through a plain `create` writer and ends without a
/// `close()`. No stress and no later allocation, so no collection runs: a byte the writer kept in a
/// buffer would never reach the FIFO, and `main` would wait for it forever. `create` is unbuffered
/// (Go `os.File`), so the bytes reach the FIFO at the `write`. Base (M5b, a `BufWriter` per file
/// writer): 4 of 4 runs hang at T=2, T=4 and default; Go and main print `reader got 100`.
const CREATE_NO_GC: &str = "fn leave_bytes(path: str, ready: Channel[bool]):
    w := must(io.create(path))
    _ := must(w.write(\"z\".repeat(100)))
    ready.send(true)

fn writer(path: str, ready: Channel[bool], done: Channel[str]):
    leave_bytes(path, ready)
    done.send(\"writer done\")

fn main():
    ready := Channel[bool](1)
    done := Channel[str](1)
    ex := Executor()
    _ := ex.submit(fn(): writer(\"@FIFO@\", ready, done))
    r := must(io.open(\"@FIFO@\"))
    _ := ready.recv()
    total := 0
    while total < 100:
        b := must(r.read_bytes(100))
        if b.len() == 0:
            break
        total += b.len()
    print(\"reader got\", total)
    print(done.recv())
    ex.shutdown()
main()
";

/// The whole program of `kind`, its golden stdout, and whether it runs under `CHEZZI_GC_STRESS=1`.
fn source(kind: &str) -> (String, String, bool) {
    if kind == "two_jobs" {
        let want = "reader got 67000 67000\nwriter done\nwriter done\n".to_string();
        return (format!("{PRELUDE}\n{TWO_JOBS}"), want, true);
    }
    if kind == "create_no_gc" {
        let want = "reader got 100\nwriter done\n".to_string();
        return (format!("{PRELUDE}\n{CREATE_NO_GC}"), want, false);
    }
    let (writer, total) = program(kind);
    let src = format!("{PRELUDE}\n{writer}{MAIN}").replace("@TOTAL@", &total.to_string());
    (src, format!("reader got {total}\nwriter done\n"), true)
}

/// (writer half of the program, the bytes the FIFO carries)
fn program(kind: &str) -> (&'static str, u32) {
    match kind {
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
    let (src, want, stress) = source(kind);
    let name = format!("{kind}-{threads}");
    let dir = std::env::temp_dir().join(format!("chz-graveyard-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let (fifo, fifo2) = (dir.join("fifo"), dir.join("fifo2"));
    for f in [&fifo, &fifo2] {
        let made = Command::new("mkfifo").arg(f).status().expect("run mkfifo");
        assert!(made.success(), "mkfifo {}", f.display());
    }
    let src = src
        .replace("@FIFO@", &fifo.to_string_lossy())
        .replace("@FIFO2@", &fifo2.to_string_lossy());
    let path = dir.join("main.chz");
    std::fs::write(&path, src).expect("write program");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run")
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if stress {
        cmd.env("CHEZZI_GC_STRESS", "1");
    } else {
        cmd.env_remove("CHEZZI_GC_STRESS");
    }
    match threads {
        "t2" => {
            cmd.env("CHEZZI_THREADS", "2");
        }
        "t4" => {
            cmd.env("CHEZZI_THREADS", "4");
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
    assert_eq!(out, want, "{name}: stdout\nstderr:\n{err}");
}

grid_tests!(KINDS x THREADS: [buffered, nested, two_jobs, create_no_gc] x [t2, t4, tdefault] => crate::assert_cell);
