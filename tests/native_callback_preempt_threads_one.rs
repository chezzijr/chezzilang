//! TICKET-141 (W14-14) — at `CHEZZI_THREADS=1` a CPU loop inside a native re-entry callback
//! (`List.map`, `Shared.update`) is never preempted, so a sibling task that must run to release it
//! starves and the program hangs. At two workers the sibling runs on the other worker and the
//! program completes.
//!
//! Spawns and polls rather than calling `output()`: a hung child never closes its pipes, so
//! `output()` would wedge this test binary instead of failing it (same pattern as
//! `nested_nursery_deadlock_threads_one.rs`).

use std::process::{Command, ExitStatus};

/// `sh7.chz`: a `List.map` callback spins on a flag that a sleeping sibling sets.
const SH7: &str = r#"import std.concurrency
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

/// `sh8.chz`: the same wait inside a `Shared.update` closure.
const SH8: &str = r#"import std.concurrency
import std.time
flag := AtomicInt(0)
s := Shared(0)
fn waitflag(f: AtomicInt, x: int) -> int:
    while f.load() == 0:
        x += 0
    return x + 1
parallel:
    spawn s.update(fn(x): waitflag(flag, x))
    spawn:
        time.sleep_ms(20)
        flag.store(1)
print("done {s.get()}")
"#;

/// `sh4.chz`: a sibling faults, so the spinning `update` closure must be cancelled, not commit.
const C4: &str = r#"import std.concurrency
import std.time
flag := AtomicInt(0)
s := Shared(0)
fn spinflag(f: AtomicInt, x: int) -> int:
    while f.load() == 0:
        x += 0
    return x + 1000
r := recover:
    parallel:
        spawn s.update(fn(x): spinflag(flag, x))
        spawn:
            time.sleep_ms(20)
            panic("sib")
print("{r} {s.get()}")
"#;

/// `sh6.chz`: a sleeping sibling must wake while a callback spins, and stop the spin early.
const C6: &str = r#"import std.concurrency
import std.time
flag := AtomicInt(0)
fn spin(f: AtomicInt, n: int) -> int:
    i := 0
    while i < n and f.load() == 0:
        i += 1
    return i
parallel:
    spawn:
        ys := [3000000].map(fn(n): spin(flag, n))
        if ys[0] < 3000000:
            print("stopped early")
        else:
            print("ran to the end")
    spawn:
        time.sleep_ms(20)
        print("sleeper woke")
        flag.store(1)
"#;

/// Shape `d`: a nursery opened and joined inside a preempted callback.
const D: &str = r#"import std.concurrency
flag := AtomicInt(0)
ch := Channel[int](0)
fn wait_then_join(f: AtomicInt, c: Channel[int], x: int) -> int:
    while f.load() == 0:
        x += 0
    got := AtomicInt(0)
    parallel:
        spawn:
            got.store(c.recv())
    return x + got.load()
parallel:
    spawn:
        ys := [1].map(fn(x): wait_then_join(flag, ch, x))
        print("mapped {ys}")
    spawn:
        flag.store(1)
    spawn:
        ch.send(41)
print("done")
"#;

/// Runs `src` at `CHEZZI_THREADS=1`. Returns `None` after killing a child that has not exited in
/// `secs`; otherwise the exit status and stdout.
fn run_at_one_worker(tag: &str, src: &str, secs: u64) -> Option<(ExitStatus, String)> {
    let dir = std::env::temp_dir().join(format!("chz-ticket141-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join(format!("{tag}.chz"));
    std::fs::write(&path, src).expect("write fixture");

    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn chezzi");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
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
        return None;
    };

    let mut stdout = String::new();
    if let Some(mut o) = child.stdout.take() {
        use std::io::Read as _;
        let _ = o.read_to_string(&mut stdout);
    }
    let _ = std::fs::remove_dir_all(&dir);
    Some((status, stdout))
}

#[test]
fn cpu_loop_in_map_callback_is_preempted_at_one_worker() {
    let Some((status, stdout)) = run_at_one_worker("sh7", SH7, 10) else {
        panic!("callback CPU loop starved its sibling at one worker (no exit within 10s)");
    };
    assert!(
        status.success(),
        "expected rc=0, got {status} (stdout: {stdout})"
    );
    assert_eq!(stdout, "mapped [1]\ndone\n");
}

#[test]
fn cpu_loop_in_update_callback_is_preempted_at_one_worker() {
    let Some((status, stdout)) = run_at_one_worker("sh8", SH8, 10) else {
        panic!("update callback CPU loop starved its sibling at one worker (no exit within 10s)");
    };
    assert!(
        status.success(),
        "expected rc=0, got {status} (stdout: {stdout})"
    );
    assert_eq!(stdout, "done 1\n");
}

#[test]
fn sibling_fault_cancels_a_spinning_update_at_one_worker() {
    let Some((status, stdout)) = run_at_one_worker("c4", C4, 10) else {
        panic!(
            "a sibling's fault never reached the spinning update at one worker (no exit within 10s)"
        );
    };
    assert!(
        status.success(),
        "expected rc=0, got {status} (stdout: {stdout})"
    );
    assert_eq!(stdout, "Err('sib') 0\n");
}

#[test]
fn a_sleeping_sibling_runs_during_a_callback_spin_at_one_worker() {
    let Some((status, stdout)) = run_at_one_worker("c6", C6, 30) else {
        panic!(
            "a sleeping sibling never woke during a callback spin at one worker (no exit within 30s)"
        );
    };
    assert!(
        status.success(),
        "expected rc=0, got {status} (stdout: {stdout})"
    );
    assert_eq!(stdout, "sleeper woke\nstopped early\n");
}

#[test]
fn a_nursery_joined_inside_a_preempted_callback_completes_at_one_worker() {
    let Some((status, stdout)) = run_at_one_worker("d", D, 10) else {
        panic!(
            "a nursery joined inside a preempted callback hung at one worker (no exit within 10s)"
        );
    };
    assert!(
        status.success(),
        "expected rc=0, got {status} (stdout: {stdout})"
    );
    assert_eq!(stdout, "mapped [42]\ndone\n");
}
