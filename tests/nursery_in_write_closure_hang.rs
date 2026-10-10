//! TICKET-245 — a `parallel:` nursery inside a `RwShared.write` closure, called from 3 tasks,
//! hangs silently at `CHEZZI_THREADS=1` and `2` (expected `240`, as at the default worker count).

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SRC: &str = "import std.concurrency
fn one(c: AtomicInt):
    c.add(1)
fn body(x: int) -> int:
    c := AtomicInt(0)
    parallel:
        for _ in 0..4:
            spawn one(c)
    return x + c.load()
fn caller(r: RwShared[int]):
    for _ in 0..20:
        r.write(body)
r := RwShared(0)
parallel:
    for _ in 0..3:
        spawn caller(r)
print(r.get())
";

/// Run the program at `threads` workers; kill it after 10 s and return stdout or "TIMEOUT".
fn run(threads: &str) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg("/dev/stdin")
        .env("CHEZZI_THREADS", threads)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(SRC.as_bytes())
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let start = Instant::now();
    loop {
        if child.try_wait().expect("try_wait").is_some() {
            let mut s = String::new();
            stdout.read_to_string(&mut s).unwrap();
            return s.trim().to_string();
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            return "TIMEOUT".to_string();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn nursery_in_write_closure_finishes_at_one_worker() {
    assert_eq!(run("1"), "240");
}

#[test]
fn nursery_in_write_closure_finishes_at_two_workers() {
    assert_eq!(run("2"), "240");
}
