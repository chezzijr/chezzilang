//! TICKET-151 (W14-40) — at `CHEZZI_THREADS=1` a blocking stdin read inside a native callback
//! (`io.input` under `List.map`) keeps the worker's width permit, so a sibling task that became
//! runnable before the read waits until the read returns. Go's `sysmon` hands the P off, so the
//! sibling runs at once.
//!
//! The test withholds stdin until the sibling has printed (or 4 s after `map done`), so the
//! failure is a bounded assertion, not a hang.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// `w40.chz`: a CPU loop in a callback gates the thread, then a callback reads stdin.
const W40: &str = r#"import std.concurrency
import std.io
fn spin(n: int) -> int:
    t := 0
    for i in 0..n:
        t += i
    return t
fn main():
    ch := Channel[int](1)
    parallel:
        spawn:
            ys := [40000000].map(fn(n: int) -> int: spin(n))
            print("map done")
            ch.send(1)
            zs := [1].map(fn(k: int) -> Option[str]: io.input(""))
            print("read done")
        spawn:
            v := ch.recv()
            print("sibling woke", v)
main()
"#;

#[test]
fn sibling_runs_while_a_callback_blocks_on_stdin() {
    let dir = std::env::temp_dir().join(format!("chz_t151_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("w40.chz");
    std::fs::write(&file, W40).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .env("CHEZZI_THREADS", "1")
        .arg("run")
        .arg(&file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn chezzi");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut seen: Vec<String> = Vec::new();
    let mut map_done_at: Option<Instant> = None;
    let hard_stop = Instant::now() + Duration::from_secs(120);
    let mut woke = false;
    // Stdin stays empty until the sibling prints, or 4 s after `map done`.
    while Instant::now() < hard_stop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(l) => {
                if l == "map done" {
                    map_done_at = Some(Instant::now());
                }
                woke = l.starts_with("sibling woke");
                seen.push(l);
                if woke {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if map_done_at.is_some_and(|t| t.elapsed() >= Duration::from_secs(4)) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(b"hi\n");
    drop(stdin);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        woke,
        "sibling did not run while the callback blocked on stdin; stdout before stdin was released: {seen:?}"
    );
}
