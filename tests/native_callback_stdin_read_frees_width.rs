//! TICKET-151 (W14-40) — at `CHEZZI_THREADS=1` a blocking stdin read (`io.input`) keeps the worker,
//! so a sibling task that became runnable before the read waits until the read returns. Go's
//! `sysmon` hands the P off, so the sibling runs at once.
//!
//! The defect is not confined to a callback: a DIRECT `io.input` starves a runnable sibling the same
//! way, with no width permit involved (releasing the permit alone fixes neither shape).
//!
//! Each test withholds stdin until the sibling has printed (or 4 s after the fixture's last line
//! before the read), so the failure is a bounded assertion, not a hang.

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

/// No callback and no spin: the fiber never preempts, so it holds no width permit.
const DIRECT: &str = r#"import std.concurrency
import std.io
fn main():
    ch := Channel[int](1)
    parallel:
        spawn:
            ch.send(1)
            z := io.input("")
            print("read done")
        spawn:
            v := ch.recv()
            print("sibling woke", v)
main()
"#;

/// The only runnable work is a task blocked on stdin; the other task waits on it. A read returns on
/// the user's input, so it must veto the deadlock verdict.
const ONLY_READER: &str = r#"import std.concurrency
import std.io
fn main():
    ch := Channel[str](1)
    parallel:
        spawn:
            print("reading")
            line := io.input("")
            ch.send(line ?? "none")
        spawn:
            print("got", ch.recv())
main()
"#;

fn spawn_chezzi(dir: &std::path::Path, src: &str, stderr: Stdio) -> std::process::Child {
    std::fs::create_dir_all(dir).unwrap();
    let file = dir.join("prog.chz");
    std::fs::write(&file, src).unwrap();
    Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .env("CHEZZI_THREADS", "1")
        .arg("run")
        .arg(&file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .expect("spawn chezzi")
}

/// Forwards the child's stdout lines over a channel so the test can wait on them with a timeout.
fn stdout_lines(child: &mut std::process::Child) -> mpsc::Receiver<String> {
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}

/// Releases the withheld stdin, waits for the child, removes its temp dir.
fn release_stdin_and_reap(mut child: std::process::Child, dir: &std::path::Path) {
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(b"hi\n");
    drop(stdin);
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sibling_runs_while_a_callback_blocks_on_stdin() {
    let dir = std::env::temp_dir().join(format!("chz_t151_{}", std::process::id()));
    let mut child = spawn_chezzi(&dir, W40, Stdio::null());
    let rx = stdout_lines(&mut child);
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
    release_stdin_and_reap(child, &dir);
    assert!(
        woke,
        "sibling did not run while the callback blocked on stdin; stdout before stdin was released: {seen:?}"
    );
}

#[test]
fn sibling_runs_while_a_direct_stdin_read_blocks() {
    let dir = std::env::temp_dir().join(format!("chz_t151_direct_{}", std::process::id()));
    let mut child = spawn_chezzi(&dir, DIRECT, Stdio::null());
    let rx = stdout_lines(&mut child);
    let mut seen: Vec<String> = Vec::new();
    // The fixture prints nothing before the read, so the 4 s fallback starts at spawn.
    let spawned_at = Instant::now();
    let mut woke = false;
    while spawned_at.elapsed() < Duration::from_secs(120) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(l) => {
                woke = l.starts_with("sibling woke");
                seen.push(l);
                if woke {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if spawned_at.elapsed() >= Duration::from_secs(4) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    release_stdin_and_reap(child, &dir);
    assert!(
        woke,
        "sibling did not run while a direct io.input blocked; stdout before stdin was released: {seen:?}"
    );
}

/// A read must veto the deadlock verdict (DEC-063: `inflight`, not `blocked_native`). While the
/// reader waits, the other task is parked on a channel only the reader can feed; the run must
/// neither fault nor exit before stdin arrives, then finish with the line the reader got.
#[test]
fn a_task_blocked_on_stdin_is_not_a_deadlock() {
    let dir = std::env::temp_dir().join(format!("chz_t151_veto_{}", std::process::id()));
    let mut child = spawn_chezzi(&dir, ONLY_READER, Stdio::piped());
    let rx = stdout_lines(&mut child);
    let first = rx.recv_timeout(Duration::from_secs(60));
    // A deadlock verdict prints a line or ends the run; silence is the pass.
    let quiet = rx.recv_timeout(Duration::from_millis(1500));
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin.write_all(b"hi\n");
    drop(stdin);
    let out = child.wait_with_output().expect("wait chezzi");
    let rest: Vec<String> = rx.try_iter().collect();
    let _ = std::fs::remove_dir_all(&dir);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(first, Ok("reading".to_string()), "stderr: {stderr}");
    assert_eq!(
        quiet,
        Err(mpsc::RecvTimeoutError::Timeout),
        "the run spoke or exited while the reader waited on stdin; stderr: {stderr}"
    );
    assert!(
        out.status.success(),
        "run faulted after stdin arrived; stderr: {stderr}"
    );
    assert!(
        !stderr.to_lowercase().contains("deadlock"),
        "deadlock verdict while a task read stdin: {stderr}"
    );
    assert_eq!(rest, vec!["got hi".to_string()], "stderr: {stderr}");
}
