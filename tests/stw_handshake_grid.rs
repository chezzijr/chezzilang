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
        // One fiber opens a FIFO and reads it, one opens another FIFO and writes it; an outside
        // `sh` process is the other end of both, 2 s late. A sibling allocates meanwhile, so each
        // collection stops the world while an open or a read blocks in the kernel. A thread that
        // keeps its GC token through a blocking syscall holds that stop for about 2 s
        // (Go `entersyscall` detaches around each syscall).
        "fifo_open_rendezvous" => (
            "import std.io as io\n",
            "fn reader(out: Channel[str], fin: Channel[int]):
    r := must(io.open(\"@FIFO_IN@\"))
    out.send(\"{r.read_line()}\")
    fin.send(1)
fn writer(fin: Channel[int]):
    w := must(io.create(\"@FIFO_OUT@\"))
    _ := w.write(\"bye\\n\")
    _ := w.flush()
    _ := w.close()
    fin.send(1)
fn allocer(fin: Channel[int], done: Channel[str]):
    while fin.len() < 2:
        _ := busy(1)
    done.send(\"allocer done\")
fn main():
    out := Channel[str](1)
    fin := Channel[int](2)
    done := Channel[str](1)
    parallel:
        spawn reader(out, fin)
        spawn writer(fin)
        spawn allocer(fin, done)
    print(out.recv())
    print(done.recv())
main()
",
            "hi\nallocer done\n",
        ),
        // A fiber sleeps 1.5 s inside an extern C call while a sibling allocates. A C call that
        // keeps its GC token holds each stop for the whole call (`stw_max_us` >= 1.5 s).
        "extern_sleep_does_not_stall_stop" => (
            "",
            "extern \"libc\":
    fn usleep(us: int) -> int
fn sleeper(out: Channel[str]):
    _ := usleep(1500000)
    out.send(\"sleeper done\")
fn allocer(out: Channel[str]):
    i := 0
    xs: List[str] = []
    while i < 200:
        xs.push(str(i))
        i += 1
    out.send(\"allocer done\")
fn main():
    a := Channel[str](1)
    b := Channel[str](1)
    parallel:
        spawn sleeper(a)
        spawn allocer(b)
    print(b.recv())
    print(a.recv())
main()
",
            "allocer done\nsleeper done\n",
        ),
        // M1.8 — Go `cgocallback`. Two fibers call `pthread_once` on one control word: the first
        // runs `slow_init`, a callback that sleeps 1.5 s; the second blocks inside C until that
        // init returns. A sibling allocates meanwhile. A C call with a callback param that keeps
        // its GC token stalls every stop until the init returns, and the callback's reattach
        // after its sleep waits for that stop: the run hangs.
        "extern_callback_blocks" => (
            "import std.ffi\nimport std.time\n",
            "extern \"libc\":
    fn pthread_once(once: ffi.ptr, init: fn() -> None) -> int
fn slow_init():
    time.sleep_ms(1500)
fn fast_init():
    pass
fn first(once: ffi.ptr, out: Channel[str]):
    _ := pthread_once(once, slow_init)
    out.send(\"first done\")
fn second(once: ffi.ptr, out: Channel[str]):
    time.sleep_ms(200)
    _ := pthread_once(once, fast_init)
    out.send(\"second done\")
fn allocer(fin: Channel[str], out: Channel[str]):
    while fin.len() < 1:
        _ := busy(1)
    out.send(\"allocer done\")
fn main():
    once := ffi.alloc(8)
    ffi.store_int64_at(once, 0, 0)
    a := Channel[str](1)
    b := Channel[str](1)
    c := Channel[str](1)
    parallel:
        spawn first(once, a)
        spawn second(once, b)
        spawn allocer(b, c)
    print(c.recv())
    print(a.recv())
    print(b.recv())
main()
",
            "allocer done\nfirst done\nsecond done\n",
        ),
        // M1.8 — a fiber reads a line from a FIFO whose writer is silent for 2 s, so the read
        // holds the Reader's lock while it waits detached. A sibling prints the Reader; another
        // allocates until the print lands. A print that waits for the lock attached holds each
        // stop for about 1.7 s. With 2 workers the allocer gets no runner while both others
        // wait, so only the default thread count measures the stall.
        "print_busy_reader" => (
            "import std.io as io\nimport std.time\n",
            "fn reader(r: io.Reader, out: Channel[str]):
    out.send(\"{r.read_line()}\")
fn printer(r: io.Reader, out: Channel[str]):
    time.sleep_ms(300)
    out.send(str(r))
fn allocer(fin: Channel[str], out: Channel[str]):
    while fin.len() < 1:
        _ := busy(1)
    out.send(\"allocer done\")
fn main():
    r := must(io.open(\"@FIFO_SLOW@\"))
    a := Channel[str](1)
    b := Channel[str](1)
    c := Channel[str](1)
    parallel:
        spawn reader(r, a)
        spawn printer(r, b)
        spawn allocer(b, c)
    print(c.recv())
    print(b.recv())
    print(a.recv())
main()
",
            "allocer done\nReader(open)\nhi\n",
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
    let (fifo_in, fifo_out) = (dir.join("fifo_in"), dir.join("fifo_out"));
    // The far end of both FIFOs: an outside process that opens them 2 s after it starts.
    let far_end = src.contains("@FIFO_IN@").then(|| {
        for f in [&fifo_in, &fifo_out] {
            let made = Command::new("mkfifo").arg(f).status().expect("run mkfifo");
            assert!(made.success(), "mkfifo {}", f.display());
        }
        let script = format!(
            "sleep 2; printf 'hi\\n' > '{}'; cat '{}' > /dev/null",
            fifo_in.display(),
            fifo_out.display()
        );
        Command::new("sh")
            .args(["-c", &script])
            .spawn()
            .expect("spawn the far end")
    });
    // A FIFO whose far end opens at once and writes one line 2 s later.
    let fifo_slow = dir.join("fifo_slow");
    let slow_end = src.contains("@FIFO_SLOW@").then(|| {
        let made = Command::new("mkfifo")
            .arg(&fifo_slow)
            .status()
            .expect("run mkfifo");
        assert!(made.success(), "mkfifo {}", fifo_slow.display());
        let script = format!(
            "exec 3> '{}'; sleep 2; printf 'hi\\n' >&3",
            fifo_slow.display()
        );
        Command::new("sh")
            .args(["-c", &script])
            .spawn()
            .expect("spawn the slow far end")
    });
    let far_end = far_end.or(slow_end);
    let src = src
        .replace("@FIFO_IN@", &fifo_in.to_string_lossy())
        .replace("@FIFO_OUT@", &fifo_out.to_string_lossy())
        .replace("@FIFO_SLOW@", &fifo_slow.to_string_lossy());
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
    let Some(status) = status else {
        if let Some(mut far) = far_end {
            let _ = far.kill();
            let _ = far.wait();
        }
        return None;
    };
    let (mut out, mut err) = (String::new(), String::new());
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut out);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    if let Some(mut far) = far_end {
        let _ = far.kill();
        let _ = far.wait();
    }
    let _ = std::fs::remove_dir_all(&dir);
    Some((status.code().unwrap_or(-1), out, err))
}

/// The stop count from the `[heap-stats]` line, `None` when the line has no `stw=`.
fn stw_count(stderr: &str) -> Option<u64> {
    stats_field(stderr, " stw=")
}

/// The number after `key` on the `[heap-stats]` line.
fn stats_field(stderr: &str, key: &str) -> Option<u64> {
    let line = stderr.lines().find(|l| l.starts_with("[heap-stats]"))?;
    let at = line.find(key)? + key.len();
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
    // A blocking syscall that keeps its token holds a stop for the whole block (>= 1 s here).
    if matches!(
        mode,
        "fifo_open_rendezvous"
            | "extern_sleep_does_not_stall_stop"
            | "extern_callback_blocks"
            | "print_busy_reader"
    ) {
        let max = stats_field(&err, "stw_max_us=");
        assert!(
            max.is_some_and(|us| us < 1_000_000),
            "{name}: a stop waited for a blocking syscall (stw_max_us={max:?})\nstderr:\n{err}"
        );
    }
    let stw = stw_count(&err);
    assert!(
        stw.is_some_and(|n| n > 0),
        "{name}: the [heap-stats] line counts no stop (got {stw:?})\nstderr:\n{err}"
    );
}

grid_tests!(MODES x THREADS: [demote, in_place, help, inline_sleep, rendezvous, executor_join, nursery_join, socket_park, socket_in_place, fifo_open_rendezvous, extern_sleep_does_not_stall_stop] x [t1, t2, tdefault] => crate::assert_cell);
// M1.8 — no `t1` cell: at one worker the first fiber's demoted sleep waits for the runner permit
// that the second fiber keeps inside C, which waits for the first (an extern call keeps its
// permit), and the print has no third runner to stall.
grid_tests!(WAIT_MODES x WAIT_THREADS: [extern_callback_blocks, print_busy_reader] x [t2, tdefault] => crate::assert_cell);

/// The longest stop on `benches/chz/many_list.chz` is under 100 ms, and the run counts stops.
/// M2.9: the bound was 10 ms (M1.9), but the stage-2 collector marks and sweeps the whole heap
/// with the world stopped (about 45 ms release, 0.5 s at opt-level 0, for 128 MB live). Concurrent
/// mark (design stage 5, `docs/design/go-shared-heap.md`) is the fix that lets it go back to 10 ms.
#[test]
fn heap_stats_report_stw_pause() {
    let bench = concat!(env!("CARGO_MANIFEST_DIR"), "/benches/chz/many_list.chz");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(bench)
        .env("CHEZZI_HEAP_STATS", "1")
        .env_remove("CHEZZI_GC_STRESS")
        .stdin(Stdio::null())
        .output()
        .expect("run many_list");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "many_list failed\nstderr:\n{err}");
    let stw = stw_count(&err);
    assert!(
        stw.is_some_and(|n| n > 0),
        "no stop counted (got {stw:?})\nstderr:\n{err}"
    );
    let max = stats_field(&err, "stw_max_us=");
    assert!(
        max.is_some_and(|us| us < 100_000),
        "max stop pause is 100 ms or more (stw_max_us={max:?})\nstderr:\n{err}"
    );
}
