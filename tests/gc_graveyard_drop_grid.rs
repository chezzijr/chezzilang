//! M5d — a writer that becomes garbage writes nothing in its drop (Go: no finalizer flushes a
//! `bufio.Writer`, and `os.File`'s finalizer only closes the fd). A garbage `io.buffered` writer's
//! unflushed tail is lost; a plain `create`/`append` writer is unbuffered (M5c), so it holds no
//! bytes. No drop of a freed object makes a write syscall, so none can block a task runner, a stop
//! of the world, or the order of the writes to a file.
//!
//! - `rev`: two Executor jobs each write 60000 bytes into a FIFO, leave a 7000-byte tail in a
//!   garbage `io.buffered` writer, close the file and signal. `main` drains the second FIFO first.
//!   M5b (one reaper thread that flushed the tails in order): the first job's flush blocked the
//!   second's, and the run hung.
//! - `order` and `nested_order`: `b := io.buffered(f)` (or a nested chain), `b.write("T")`, `b`
//!   becomes garbage, a collection, `f.write("A")`, `f.close()`. The file must hold `A`: a tail
//!   flushed later could land after `A` or be lost at the close, at random.
//! - `create_no_gc` (no stress): a job writes 100 bytes into a FIFO through `io.create` and ends
//!   without `close()`; no collection runs, so a byte kept in a buffer would never arrive (M5c;
//!   M5b base: 4 of 4 runs hang at T=2, T=4 and default; Go prints `reader got 100`).
//!
//! `CHEZZI_GC_STRESS=1` (every kind but `create_no_gc`) collects at every instruction, so the
//! garbage writer is swept before the next write. Each run is its own process and must exit 0 with
//! its golden stdout within [`LIMIT`]. A run that outlives the bound is killed and FAILS. It is
//! never retried.

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

fn close_it(w: io.Writer):
    match w.close():
        ?_: pass
        !e: panic(e.message())

fn churn(n: int) -> int:
    s := 0
    for i in 0..n:
        xs := [i, i + 1, i + 2]
        s += xs.len()
    return s
";

const REV: &str = "fn leave_tail(w: io.Writer):
    _ := must(w.write(\"x\".repeat(60000)))
    b := io.buffered(w, 100000)
    _ := must(b.write(\"y\".repeat(7000)))

fn writer(path: str, go: Channel[bool], ready: Channel[bool], done: Channel[str]):
    w := must(io.create(path))
    _ := go.recv()
    leave_tail(w)
    _ := churn(200)
    close_it(w)
    ready.send(true)
    done.send(\"writer done\")

fn drain(r: io.Reader) -> int:
    total := 0
    while true:
        b := must(r.read_bytes(1000))
        if b.len() == 0:
            break
        total += b.len()
    return total

fn main():
    goa := Channel[bool](1)
    gob := Channel[bool](1)
    ready := Channel[bool](2)
    done := Channel[str](2)
    ex := Executor()
    _ := ex.submit(fn(): writer(\"@FIFO@\", goa, ready, done))
    _ := ex.submit(fn(): writer(\"@FIFO2@\", gob, ready, done))
    ra := must(io.open(\"@FIFO@\"))
    rb := must(io.open(\"@FIFO2@\"))
    goa.send(true)
    _ := ready.recv()
    time.sleep_ms(300)
    gob.send(true)
    _ := ready.recv()
    time.sleep_ms(300)
    b := drain(rb)
    a := drain(ra)
    print(\"reader got\", a, b)
    print(done.recv())
    print(done.recv())
    ex.shutdown()
main()
";

/// `@WRAP@` builds the buffered writer over `f`.
const ORDER: &str = "fn leave(f: io.Writer):
    b := @WRAP@
    _ := must(b.write(\"T\"))

fn main():
    f := must(io.create(\"@FILE@\"))
    leave(f)
    _ := churn(200)
    _ := must(f.write(\"A\"))
    close_it(f)
    print(must(io.read_file(\"@FILE@\")))
main()
";

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
fn source(kind: &str) -> (String, &'static str, bool) {
    match kind {
        "rev" => (
            format!("{PRELUDE}\n{REV}"),
            "reader got 60000 60000\nwriter done\nwriter done\n",
            true,
        ),
        "order" => (
            format!("{PRELUDE}\n{ORDER}").replace("@WRAP@", "io.buffered(f, 100)"),
            "A\n",
            true,
        ),
        "nested_order" => (
            format!("{PRELUDE}\n{ORDER}")
                .replace("@WRAP@", "io.buffered(io.buffered(f, 100), 100)"),
            "A\n",
            true,
        ),
        "create_no_gc" => (
            format!("{PRELUDE}\n{CREATE_NO_GC}"),
            "reader got 100\nwriter done\n",
            false,
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
        .replace("@FIFO2@", &fifo2.to_string_lossy())
        .replace("@FILE@", &dir.join("out.txt").to_string_lossy());
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
        "t1" => {
            cmd.env("CHEZZI_THREADS", "1");
        }
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

grid_tests!(KINDS x THREADS: [rev, order, nested_order, create_no_gc] x [t1, t2, t4, tdefault] => crate::assert_cell);
