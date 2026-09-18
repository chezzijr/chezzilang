//! TICKET-103 (W12-1 / W12-4) — at `CHEZZI_THREADS=1`, a nested `parallel:` nursery whose OWNER
//! body blocks on a channel op is falsely `deadlock`-faulted even though a live sibling can still
//! unblock it, and a task that RECOVERS a genuine inner-nursery `deadlock` poisons the enclosing
//! nursery's bookkeeping so a later live rendezvous is also falsely reported as `deadlock`.
//! T=2/4/default: both programs complete cleanly (`docs/gaps.md` W12-1 / W12-4).
//! TICKET-135 (D1): a deadlock verdict is now FATAL, so no program can RECOVER an inner deadlock and
//! keep running. The recovered shapes below abort with `deadlock` at every worker count.

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

/// W12-4 (TICKET-135, D1): a task `recover:`s a genuine inner-nursery `deadlock`. The verdict is
/// FATAL, so `recover:` is transparent to it: the program aborts, `inner err` never prints, and the
/// later `out` rendezvous and the outer join never run.
#[test]
fn a_recovered_inner_deadlock_is_fatal_at_threads_1() {
    let dir = std::env::temp_dir().join(format!("chz-w12-4-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("recovered_fatal.chz");
    std::fs::write(&path, RECOVERED).expect("write fixture");

    for round in 0..5 {
        let out = run_at_threads_1(&path);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && stderr.contains("deadlock"),
            "round {round}: expected a fatal `deadlock`, got {} — stderr: {stderr}",
            out.status
        );
        assert!(
            !stdout.contains("inner err") && !stdout.contains("done"),
            "round {round}: `recover:` must not catch the verdict, got stdout: {stdout}"
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
        spawn level(DEPTH, out)
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

/// TICKET-112 — a task joining a nested nursery whose only child is genuinely deadlocked, while two
/// cousin nurseries feed each other only after that join returns: at `CHEZZI_THREADS>=2` the private
/// nested sched's open body used to veto the genuine deadlock's own fault, hanging the whole run.
const COUSIN_FED: &str = r#"fn main():
    never := Channel[int](0)
    x := Channel[int](0)
    y := Channel[int](0)
    parallel:
        spawn:
            r := recover:
                parallel:
                    spawn:
                        never.recv()
            match r:
                Ok(_): print("inner ok")
                Err(e): print("inner err")
            x.send(1)
        spawn:
            parallel:
                spawn:
                    y.send(x.recv() + 1)
                print("F got {y.recv()}")
    print("done")
main()
"#;

/// TICKET-135 (W14-39) — task A's nested nursery child panics while task B's nested nursery child
/// is parked on a channel that only A's tail would feed.
const PANIC_COUSIN: &str = r#"fn main():
    x := Channel[int](0)
    y := Channel[int](0)
    parallel:
        spawn:
            parallel:
                spawn:
                    panic("boom")
            x.send(1)
        spawn:
            parallel:
                spawn:
                    y.send(x.recv() + 1)
                print("F got {y.recv()}")
    print("done")
main()
"#;

/// TICKET-135 (W14-39) — as `PANIC_COUSIN`, but B's nested nursery body parks on its own channel.
const PANIC_BODY_PARKED: &str = r#"fn main():
    x := Channel[int](0)
    y := Channel[int](0)
    parallel:
        spawn:
            parallel:
                spawn:
                    panic("boom")
        spawn:
            parallel:
                spawn:
                    x.recv()
                y.recv()
    print("done")
main()
"#;

/// TICKET-112 — a genuine nested deadlock whose outer body sits at its own join (not a channel
/// recv), a third shape of the same open-body-veto defect.
const NOFEED_JOIN: &str = r#"fn main():
    out := Channel[int](0)
    never := Channel[int](0)
    parallel:
        spawn:
            inner := Channel[int](0)
            parallel:
                spawn:
                    never.recv()
                out.send(inner.recv() + 1)
        spawn:
            print("got {out.recv()}")
    print("done")
main()
"#;

/// TICKET-112 — an Executor job's nursery spawns a task that opens its own nested nursery, whose
/// only child is genuinely deadlocked. Traces the same defect on the Executor path.
const EXEC_NESTED: &str = r#"import std.concurrency

fn job():
    never := Channel[int](0)
    x := Channel[int](0)
    r := recover:
        parallel:
            spawn:
                parallel:
                    spawn:
                        parallel:
                            spawn:
                                never.recv()
                    x.recv()
    match r:
        Ok(_): print("job ok")
        Err(e): print("job err")

fn main():
    ex := Executor()
    ex.submit(fn(): job())
    ex.shutdown()
    print("done")
main()
"#;

const DD6: &str = r#"ch := Channel[int](0)
fn f():
    spawn:
        print(ch.recv())
spawn f()
"#;

const DD6_FED: &str = r#"fn f(ch: Channel[int]):
    spawn:
        print("f got {ch.recv()}")

fn main():
    ch := Channel[int](0)
    parallel:
        spawn f(ch)
        ch.send(7)
    print("done")
main()
"#;

const RET_FED: &str = r#"fn f(ch: Channel[int]) -> int:
    spawn:
        print("f got {ch.recv()}")
    return 3

fn main():
    ch := Channel[int](0)
    parallel:
        spawn:
            print("f returned {f(ch)}")
        ch.send(7)
    print("done")
main()
"#;

const TRY_FED: &str = r#"fn g(ch: Channel[int]) -> Result[int, str]:
    spawn:
        print("g got {ch.recv()}")
    r: Result[int, str] = Err("bail")
    v := r?
    return Ok(v)

fn main():
    ch := Channel[int](0)
    parallel:
        spawn:
            print("g -> {g(ch)}")
        ch.send(7)
    print("done")
main()
"#;

/// What a completing fixture must print. `Exact` lines are causally ordered. `ThenLast` lines may
/// print in any order, followed by one causally-last line.
enum Expect<'a> {
    Exact(&'a [&'a str]),
    ThenLast(&'a [&'a str], &'a str),
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

/// The depth `recursive` runs at, per worker count.
///
/// TICKET-112 lifted the T>=2 clamp: a nested eager sched now counts as live work in the
/// process-wide verdict (`QuiesceState::live_eager_bodies`), so `recursive` no longer false-faults
/// past the granted-slot path at any worker count. Full depth everywhere.
fn recursive_depth(_threads: Option<&str>) -> usize {
    30
}

/// TICKET-103 (W12-1, W12-4) — every live nested-nursery shape completes with Go's output at every
/// worker count: an owner blocked on its own child's channel, a recursion of that shape (30 deep at
/// T=1), a fn whose implicit nursery joins at fall-through, `return` or `?` while a caller feeds
/// its child, and a sibling spawned after the inner nursery opened. (The recovered-inner-deadlock
/// shapes moved to the fatal test below: TICKET-135, D1.)
///
/// Spawns and polls rather than calling `output()`: a hung child never closes its pipes, so
/// `output()` would wedge this test binary instead of failing it.
#[test]
fn fixed_nested_nursery_shapes_complete_at_every_worker_count() {
    for threads in WORKER_COUNTS {
        let depth = recursive_depth(threads);
        let recursive_src = RECURSIVE.replace("DEPTH", &depth.to_string());
        let recursive_want = format!("depth {}", depth + 1);
        let recursive_want = [recursive_want.as_str()];
        let fixtures: [(&str, &str, Expect); 6] = [
            ("owner_blocked", OWNER_BLOCKED, Expect::Exact(&["got 2"])),
            ("recursive", &recursive_src, Expect::Exact(&recursive_want)),
            ("dd6_fed", DD6_FED, Expect::Exact(&["f got 7", "done"])),
            (
                "ret_fed",
                RET_FED,
                Expect::Exact(&["f got 7", "f returned 3", "done"]),
            ),
            (
                "try_fed",
                TRY_FED,
                Expect::Exact(&["g got 7", "g -> Err('bail')", "done"]),
            ),
            (
                "late_feed",
                LATE_FEED,
                Expect::ThenLast(&["inner got 5", "late sibling done"], "done"),
            ),
        ];
        for (name, src, expect) in &fixtures {
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
/// waits on a channel nobody sends, both still fault `deadlock` at every worker count. TICKET-135
/// (D1): the shapes that `recover:` an inner deadlock (`recovered`, `recovered_swapped`,
/// `late_recover`, `cousin_fed`, `exec_nested`) abort too, before any post-recover line prints.
#[test]
fn nested_nursery_genuine_deadlocks_still_fault_at_every_worker_count() {
    for (name, src) in [
        ("nofeed", NOFEED),
        ("recstuck", RECSTUCK),
        ("dd6", DD6),
        ("nofeed_join", NOFEED_JOIN),
        ("recovered", RECOVERED),
        ("recovered_swapped", RECOVERED_SWAPPED),
        ("late_recover", LATE_RECOVER),
        ("cousin_fed", COUSIN_FED),
        ("exec_nested", EXEC_NESTED),
    ] {
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
                    !status.success()
                        && stderr.contains("deadlock")
                        && !stdout.contains("inner err")
                        && !stdout.contains("job err"),
                    "{name}, CHEZZI_THREADS={threads:?}, round {round}: expected a fatal \
                     `deadlock` with no post-recover output, got status {status}, \
                     stdout {stdout:?}, stderr {stderr:?}"
                );
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
}

/// TICKET-135 (W14-39) — a sibling task's fault cancels a task parked in its nested nursery's BODY,
/// and the run faults `boom` at every worker count instead of hanging. The cancel unwind used to
/// skip aborting the nested nursery, orphaning its parked child; at `CHEZZI_THREADS=1` no worker
/// was left to drain it. Go 1.27 prints `panic: boom`.
#[test]
fn a_sibling_fault_cancels_a_task_parked_in_its_nested_nursery_body_at_every_worker_count() {
    for (name, src) in [
        ("panic_cousin", PANIC_COUSIN),
        ("panic_body_parked", PANIC_BODY_PARKED),
    ] {
        for threads in WORKER_COUNTS {
            for round in 0..8 {
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
                        "{name}, CHEZZI_THREADS={threads:?}, round {round}: must fault `boom`, \
                         not hang (no exit within 10s)"
                    );
                };
                let (stdout, stderr) = read_pipes(&mut child);
                assert!(
                    !status.success()
                        && stderr.contains("boom")
                        && !stdout.contains("F got")
                        && !stdout.contains("done"),
                    "{name}, CHEZZI_THREADS={threads:?}, round {round}: expected a fatal `boom`, \
                     got status {status}, stdout {stdout:?}, stderr {stderr:?}"
                );
                let _ = std::fs::remove_dir_all(&dir);
            }
        }
    }
}
