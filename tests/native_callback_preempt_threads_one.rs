//! TICKET-141 (W14-14) — at `CHEZZI_THREADS=1` a CPU loop inside a native re-entry callback
//! (`List.map`) is never preempted, so a sibling task that must run to release it starves and the
//! program hangs. At two workers the sibling runs on the other worker and the program completes.
//!
//! Spawns and polls rather than calling `output()`: a hung child never closes its pipes, so
//! `output()` would wedge this test binary instead of failing it (same pattern as
//! `nested_nursery_deadlock_threads_one.rs`).

use std::process::Command;

const SRC: &str = r#"import std.concurrency
import std.time
flag := AtomicInt(0)
fn waitflag(f: AtomicInt, x: int) -> int:
    n := 0
    while f.load() == 0:
        n += 1
    return x
parallel:
    spawn:
        ys := [1].map(fn(x): waitflag(flag, x))
        print("mapped {ys}")
    spawn:
        time.sleep_ms(20)
        flag.store(1)
print("done")
"#;

#[test]
fn cpu_loop_in_map_callback_is_preempted_at_one_worker() {
    let dir = std::env::temp_dir().join(format!("chz-ticket141-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("sh7.chz");
    std::fs::write(&path, SRC).expect("write fixture");

    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn chezzi");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(st) => break Some(st),
            None if std::time::Instant::now() >= deadline => break None,
            // Poll against a deadline: a hung child never closes its pipes.
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        panic!("callback CPU loop starved its sibling at one worker (no exit within 10s)");
    };

    let mut stdout = String::new();
    if let Some(mut o) = child.stdout.take() {
        use std::io::Read as _;
        let _ = o.read_to_string(&mut stdout);
    }
    assert!(
        status.success(),
        "expected rc=0, got {status} (stdout: {stdout})"
    );
    assert_eq!(stdout, "mapped [1]\ndone\n");
    let _ = std::fs::remove_dir_all(&dir);
}
