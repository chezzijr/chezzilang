//! TICKET-181 (wave 16 Family 2, "Blocking contexts"): each blocking op decides for itself whether
//! it may block in the current execution context. These are the named red cells before the fix;
//! the plan extends this file into the full op × context grid. Each cell runs a fresh process
//! under a hard timeout at T=1, T=2 and T=0 (default), and compares against the RUN Go/CPython
//! result recorded per cell.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Runs `src` at `threads` workers; `None` = still running after `limit` (killed).
fn run(name: &str, src: &str, threads: &str, limit: Duration) -> Option<(i32, String, String)> {
    let dir =
        std::env::temp_dir().join(format!("chz-t181-{name}-{threads}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(&path, src).expect("write program");
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", threads)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
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
    let src = "import std.concurrency\nfn main():\n    ch := Channel[int](0)\n    parallel:\n        spawn:\n            ex := Executor()\n            ex.submit(fn() -> nil:\n                print(ch.recv())\n            )\n            ex.shutdown()\n            print(\"done\")\n        spawn:\n            ch.send(7)\nmain()\n";
    assert_cell("x1", src, "7\ndone\n");
}

/// X2: `peer.read(2)` inside a `.map()` callback on the main thread, data arriving 50 ms later
/// from a sibling. CPython and Go block and read; no Executor is involved.
#[test]
fn x2_socket_read_in_main_callback_blocks_and_reads() {
    let src = "import std.net\nimport std.time\nfn show[T](r: Result[T]) -> str:\n    match r:\n        Ok(_): return \"ok\"\n        Err(e): return \"err \" + e.message()\nfn main() -> Result[int]:\n    ln := net.listen(\"127.0.0.1:0\")?\n    c := net.connect(ln.addr()?)?\n    peer := ln.accept()?\n    parallel:\n        spawn:\n            time.sleep_ms(50)\n            _ := c.write(\"hi\")\n        print([2].map(fn(n: int) -> str: show(peer.read(n))))\n    return Ok(0)\n_ := main()\n";
    assert_cell("x2", src, "['ok']\n");
}
