//! TICKET-103 (W12-1 / W12-4) — at `CHEZZI_THREADS=1`, a nested `parallel:` nursery whose OWNER
//! body blocks on a channel op is falsely `deadlock`-faulted even though a live sibling can still
//! unblock it, and a task that RECOVERS a genuine inner-nursery `deadlock` poisons the enclosing
//! nursery's bookkeeping so a later live rendezvous is also falsely reported as `deadlock`.
//! T=2/4/default: both programs complete cleanly (`docs/gaps.md` W12-1 / W12-4).

use std::process::Command;

fn run_at_threads_1(path: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(path)
        .env("CHEZZI_THREADS", "1")
        .output()
        .expect("spawn chezzi")
}

/// W12-1: the owner of a nested nursery blocks on `out.send(inner.recv() + 1)` while its own
/// child is still alive on `inner`. The join must complete (`got 2`), not fault `deadlock`.
#[test]
fn nested_nursery_owner_blocked_on_channel_op_completes_at_threads_1() {
    let dir = std::env::temp_dir().join(format!("chz-w12-1-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("owner_blocked.chz");
    std::fs::write(
        &path,
        "fn main():\n    \
             out := Channel[int](0)\n    \
             parallel:\n        \
                 spawn:\n            \
                     inner := Channel[int](0)\n            \
                     parallel:\n                \
                         spawn:\n                    \
                             inner.send(1)\n                \
                         out.send(inner.recv() + 1)\n        \
                 print(\"got {out.recv()}\")\n\
         main()\n",
    )
    .expect("write fixture");

    for round in 0..5 {
        let out = run_at_threads_1(&path);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "round {round}: expected rc=0, got {} — stderr: {stderr}",
            out.status
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("got 2"),
            "round {round}: expected \"got 2\", got stdout: {stdout} stderr: {stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// W12-4: a task recovers a genuine inner-nursery `deadlock`, then rendezvous with a sibling over
/// `out`. The exchange succeeds (`task got 1`) but the outer join then falsely faults `deadlock`
/// too — the recovered fault leaves the enclosing sched's bookkeeping stale.
#[test]
fn recovered_inner_deadlock_does_not_poison_the_enclosing_nursery_at_threads_1() {
    let dir = std::env::temp_dir().join(format!("chz-w12-4-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("recovered_poisons.chz");
    std::fs::write(
        &path,
        "fn main():\n    \
             ch := Channel[int](0)\n    \
             out := Channel[int](0)\n    \
             parallel:\n        \
                 spawn:\n            \
                     r := recover:\n                \
                         parallel:\n                    \
                             spawn:\n                        \
                                 ch.recv()\n            \
                     match r:\n                \
                         Ok(_): print(\"inner ok\")\n                \
                         Err(e): print(\"inner err\")\n            \
                     print(\"task got {out.recv()}\")\n        \
                 spawn:\n            \
                     out.send(1)\n    \
             print(\"done\")\n\
         main()\n",
    )
    .expect("write fixture");

    for round in 0..5 {
        let out = run_at_threads_1(&path);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "round {round}: expected rc=0, got {} — stderr: {stderr}",
            out.status
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("inner err")
                && stdout.contains("task got 1")
                && stdout.contains("done"),
            "round {round}: expected full completion, got stdout: {stdout} stderr: {stderr}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The worker counts Tests A and B sweep; `None` removes `CHEZZI_THREADS` so the default runs.
const WORKER_COUNTS: [Option<&str>; 5] = [Some("1"), Some("2"), Some("4"), Some("8"), None];

const OWNER_BLOCKED: &str = r#"fn main():
    out := Channel[int](0)
    parallel:
        spawn:
            inner := Channel[int](0)
            parallel:
                spawn:
                    inner.send(1)
                out.send(inner.recv() + 1)
        print("got {out.recv()}")
main()
"#;

const RECURSIVE: &str = r#"fn level(n: int, out: Channel[int]):
    if n == 0:
        out.send(1)
        return
    inner := Channel[int](0)
    parallel:
        spawn level(n - 1, inner)
        out.send(inner.recv() + 1)

fn main():
    out := Channel[int](0)
    parallel:
        spawn level(30, out)
        print("depth {out.recv()}")
main()
"#;

const RECOVERED: &str = r#"fn main():
    ch := Channel[int](0)
    out := Channel[int](0)
    parallel:
        spawn:
            r := recover:
                parallel:
                    spawn:
                        ch.recv()
            match r:
                Ok(_): print("inner ok")
                Err(e): print("inner err")
            print("task got {out.recv()}")
        spawn:
            out.send(1)
    print("done")
main()
"#;

const RECOVERED_SWAPPED: &str = r#"fn main():
    ch := Channel[int](0)
    out := Channel[int](0)
    parallel:
        spawn:
            r := recover:
                parallel:
                    spawn:
                        ch.recv()
            match r:
                Ok(_): print("inner ok")
                Err(e): print("inner err")
            out.send(1)
        spawn:
            print("task got {out.recv()}")
    print("done")
main()
"#;

const LATE_FEED: &str = r#"fn main():
    started := Channel[int](0)
    gate := Channel[int](0)
    parallel:
        spawn:
            parallel:
                spawn:
                    print("inner got {gate.recv()}")
                started.send(1)
        started.recv()
        spawn:
            gate.send(5)
            print("late sibling done")
    print("done")
main()
"#;

const LATE_RECOVER: &str = r#"fn main():
    ch := Channel[int](0)
    out := Channel[int](0)
    started := Channel[int](0)
    parallel:
        spawn:
            r := recover:
                parallel:
                    spawn:
                        ch.recv()
                    started.send(1)
            match r:
                Ok(_): print("inner ok")
                Err(e): print("inner err")
            print("task got {out.recv()}")
        started.recv()
        spawn:
            out.send(1)
    print("done")
main()
"#;

const NOFEED: &str = r#"fn main():
    out := Channel[int](0)
    never := Channel[int](0)
    parallel:
        spawn:
            inner := Channel[int](0)
            parallel:
                spawn:
                    never.recv()
                out.send(inner.recv() + 1)
        print("got {out.recv()}")
main()
"#;

const RECSTUCK: &str = r#"fn main():
    ch := Channel[int](0)
    out := Channel[int](0)
    parallel:
        spawn:
            r := recover:
                parallel:
                    spawn:
                        ch.recv()
            match r:
                Ok(_): print("inner ok")
                Err(e): print("inner err")
            print("after recover")
            print("task got {out.recv()}")
    print("done")
main()
"#;

/// What a completing fixture must print. `Exact` lines are causally ordered. `ThenLast` lines may
/// print in any order, followed by one causally-last line.
enum Expect {
    Exact(&'static [&'static str]),
    ThenLast(&'static [&'static str], &'static str),
}

/// Write `src` to a fresh temp dir and start `chezzi run` on it, piped, at `threads`.
fn spawn_fixture(
    name: &str,
    src: &str,
    threads: Option<&str>,
    round: usize,
) -> (std::path::PathBuf, std::process::Child) {
    let dir = std::env::temp_dir().join(format!(
        "chz-t103-{}-{name}-{}-{round}",
        std::process::id(),
        threads.unwrap_or("default")
    ));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join(format!("{name}.chz"));
    std::fs::write(&path, src).expect("write fixture");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run")
        .arg(&path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    match threads {
        Some(t) => cmd.env("CHEZZI_THREADS", t),
        None => cmd.env_remove("CHEZZI_THREADS"),
    };
    (dir, cmd.spawn().expect("spawn chezzi"))
}

/// Read a finished child's stdout and stderr.
fn read_pipes(child: &mut std::process::Child) -> (String, String) {
    use std::io::Read as _;
    let (mut out, mut err) = (String::new(), String::new());
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut out);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    (out, err)
}

/// TICKET-103 (W12-1, W12-4) — every live nested-nursery shape completes with Go's output at every
/// worker count: an owner blocked on its own child's channel, a 30-deep recursion of that shape, a
/// recovered inner deadlock (both roles), and a sibling spawned after the inner nursery opened.
///
/// Spawns and polls rather than calling `output()`: a hung child never closes its pipes, so
/// `output()` would wedge this test binary instead of failing it.
#[test]
#[ignore = "TICKET-103: red until the parking join lands; plan step 1 removes this attribute"]
fn fixed_nested_nursery_shapes_complete_at_every_worker_count() {
    let fixtures: [(&str, &str, Expect); 6] = [
        ("owner_blocked", OWNER_BLOCKED, Expect::Exact(&["got 2"])),
        ("recursive", RECURSIVE, Expect::Exact(&["depth 31"])),
        (
            "recovered",
            RECOVERED,
            Expect::Exact(&["inner err", "task got 1", "done"]),
        ),
        (
            "recovered_swapped",
            RECOVERED_SWAPPED,
            Expect::Exact(&["inner err", "task got 1", "done"]),
        ),
        (
            "late_feed",
            LATE_FEED,
            Expect::ThenLast(&["inner got 5", "late sibling done"], "done"),
        ),
        (
            "late_recover",
            LATE_RECOVER,
            Expect::Exact(&["inner err", "task got 1", "done"]),
        ),
    ];
    for (name, src, expect) in &fixtures {
        for threads in WORKER_COUNTS {
            for round in 0..10 {
                let (dir, mut child) = spawn_fixture(name, src, threads, round);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                let status = loop {
                    match child.try_wait().expect("try_wait") {
                        Some(st) => break Some(st),
                        None if std::time::Instant::now() >= deadline => break None,
                        None => std::thread::sleep(std::time::Duration::from_millis(20)),
                    }
                };
                let Some(status) = status else {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{name}, CHEZZI_THREADS={threads:?}, round {round}: no exit within 20s");
                };
                let (stdout, stderr) = read_pipes(&mut child);
                let at = format!(
                    "{name}, CHEZZI_THREADS={threads:?}, round {round}: status {status}, \
                     stdout {stdout:?}, stderr {stderr:?}"
                );
                assert!(status.success(), "expected rc 0 — {at}");
                let lines: Vec<&str> = stdout.lines().collect();
                match expect {
                    Expect::Exact(want) => assert_eq!(&lines, want, "{at}"),
                    Expect::ThenLast(any, last) => {
                        let (tail, head) = lines.split_last().expect("some output");
                        let mut head = head.to_vec();
                        head.sort_unstable();
                        let mut any = any.to_vec();
                        any.sort_unstable();
                        assert_eq!((head, *tail), (any, *last), "{at}");
                    }
                }
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
}

/// TICKET-103 — the fix must not turn a genuine nested deadlock into a hang: an owner blocked on a
/// child that waits on a channel nobody sends, and a recovered inner deadlock whose task then
/// waits on a channel nobody sends, both still fault `deadlock` at every worker count.
#[test]
fn nested_nursery_genuine_deadlocks_still_fault_at_every_worker_count() {
    for (name, src) in [("nofeed", NOFEED), ("recstuck", RECSTUCK)] {
        for threads in WORKER_COUNTS {
            for round in 0..5 {
                let (dir, mut child) = spawn_fixture(name, src, threads, round);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                let status = loop {
                    match child.try_wait().expect("try_wait") {
                        Some(st) => break Some(st),
                        None if std::time::Instant::now() >= deadline => break None,
                        None => std::thread::sleep(std::time::Duration::from_millis(20)),
                    }
                };
                let Some(status) = status else {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!(
                        "{name}, CHEZZI_THREADS={threads:?}, round {round}: a genuine deadlock must \
                         fault, not hang (no exit within 10s)"
                    );
                };
                let (stdout, stderr) = read_pipes(&mut child);
                assert!(
                    !status.success() && stderr.contains("deadlock"),
                    "{name}, CHEZZI_THREADS={threads:?}, round {round}: expected a `deadlock` \
                     fault, got status {status}, stdout {stdout:?}, stderr {stderr:?}"
                );
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
}
