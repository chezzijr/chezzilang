//! TICKET-224 cell 2 — a loop-free recursion has no cut point, so `os.exit` waits for it to finish.
//! Real-PROCESS test: the exit fires at 100 ms; the recursion alone runs far longer.

use std::process::Command;
use std::time::{Duration, Instant};

const SRC: &str = "import std.time
import std.os
import std.concurrency
fn fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)
fn quit():
    time.sleep_ms(100)
    os.exit(7)
ex := Executor()
ex.submit(quit)
print(fib(36))
";

#[test]
fn exit_lands_inside_a_loop_free_recursion() {
    let path = std::env::temp_dir().join(format!("t224_rec_{}.chz", std::process::id()));
    std::fs::write(&path, SRC).unwrap();
    let t0 = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", "--threads=2"])
        .arg(&path)
        .output()
        .expect("spawn chezzi");
    let wall = t0.elapsed();
    let _ = std::fs::remove_file(&path);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert_eq!(stdout, "", "nothing prints after os.exit");
    assert!(
        wall < Duration::from_millis(1500),
        "os.exit at 100 ms must land inside the recursion, took {wall:?}"
    );
}
