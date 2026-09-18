//! TICKET-136 (W14-11, W14-16, W14-35) — deadlock-verdict misses after TICKET-135's D1.
//! Real-PROCESS tests: the hang case needs a wall-clock bound and the deadlock verdict is fatal.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Run `src` under `chezzi run`; kill it after 10 s. Returns (stdout, stderr, code), code None on timeout.
fn run(name: &str, src: &[&str]) -> (String, String, Option<i32>) {
    run_with_threads(name, src, None)
}

/// [`run`] with `CHEZZI_THREADS` set to `threads` when `Some` (the default pool size when `None`).
fn run_with_threads(
    name: &str,
    src: &[&str],
    threads: Option<&str>,
) -> (String, String, Option<i32>) {
    let dir = std::env::temp_dir().join(format!("chz-ticket136-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join(name);
    std::fs::write(&path, src.join("\n")).expect("write fixture");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run")
        .arg(&path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match threads {
        Some(n) => cmd.env("CHEZZI_THREADS", n),
        None => cmd.env_remove("CHEZZI_THREADS"),
    };
    let mut child = cmd.spawn().expect("spawn chezzi");
    let start = Instant::now();
    let code = loop {
        if let Some(st) = child.try_wait().expect("try_wait") {
            break st.code();
        }
        if start.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let (mut o, mut e) = (String::new(), String::new());
    child.stdout.take().unwrap().read_to_string(&mut o).unwrap();
    child.stderr.take().unwrap().read_to_string(&mut e).unwrap();
    (o, e, code)
}

#[test]
fn main_defer_that_can_never_complete_faults_deadlock() {
    let (out, err, code) = run(
        "d9.chz",
        &[
            "fn main():",
            "    c := Channel[int](0)",
            "    defer: c.recv()",
            "    print(\"x\")",
            "main()",
            "",
        ],
    );
    assert_eq!(out, "x\n");
    assert!(
        code == Some(1) && err.contains("deadlock"),
        "a never-completing main-thread defer must fault `deadlock`, got code {code:?} (stderr: {err})"
    );
}

#[test]
fn for_over_channel_in_generator_driven_from_a_task_prints_the_value() {
    let (out, err, code) = run(
        "q3.chz",
        &[
            "fn gen(c: Channel[int]) -> Iterator[int]:",
            "    for v in c:",
            "        yield v",
            "fn burn(n: int) -> int:",
            "    s := 0",
            "    for i in range(n):",
            "        s += i % 7",
            "    return s",
            "fn main():",
            "    c := Channel[int](4)",
            "    parallel:",
            "        spawn:",
            "            c.send(burn(3000000) % 2)",
            "            c.close()",
            "        spawn:",
            "            for v in gen(c):",
            "                print(v)",
            "main()",
            "",
        ],
    );
    assert!(
        code == Some(0) && out.trim().len() == 1,
        "the generator must print the one sent value and exit 0, got code {code:?} (stdout: {out:?}, stderr: {err})"
    );
}

#[test]
fn rendezvous_send_deadlock_names_the_missing_receiver() {
    let (_, err, code) = run(
        "rz.chz",
        &[
            "fn main():",
            "    c := Channel[int](0)",
            "    c.send(1)",
            "main()",
            "",
        ],
    );
    assert_eq!(code, Some(1), "stderr: {err}");
    assert!(
        !err.contains("at capacity"),
        "a rendezvous channel has no slots; the message must not say it is at capacity (stderr: {err})"
    );
}

/// Every worker count the verdict must hold at: `1`, `2` and the default pool.
const THREAD_COUNTS: [Option<&str>; 3] = [Some("1"), Some("2"), None];

/// Assert `res` faulted `deadlock` (exit 1) rather than hanging (`code None`).
fn assert_deadlock(what: &str, threads: Option<&str>, res: &(String, String, Option<i32>)) {
    let (out, err, code) = res;
    assert!(
        *code == Some(1) && err.contains("deadlock"),
        "{what} must fault `deadlock` at CHEZZI_THREADS={threads:?}, got code {code:?} (stdout: {out:?}, stderr: {err})"
    );
}

#[test]
fn defer_in_a_nursery_body_that_can_never_send_faults_deadlock() {
    // d2: the body `defer` runs AFTER the join; the join's verdict fires, then the defer's `send` hangs.
    let src = [
        "fn main():",
        "    c := Channel[int](0)",
        "    parallel:",
        "        defer:",
        "            c.send(1)",
        "        spawn:",
        "            print(\"got {c.recv()}\")",
        "    print(\"end\")",
        "main()",
        "",
    ];
    for t in THREAD_COUNTS {
        let res = run_with_threads("d2.chz", &src, t);
        assert_deadlock("a nursery-body defer that can never send", t, &res);
        assert_eq!(res.0, "", "nothing may print before the deadlock (T={t:?})");
    }
}

#[test]
fn defer_in_a_nursery_body_that_can_never_recv_faults_deadlock() {
    let src = [
        "fn main():",
        "    c := Channel[int](0)",
        "    never := Channel[int](0)",
        "    parallel:",
        "        defer:",
        "            c.recv()",
        "        spawn:",
        "            never.recv()",
        "    print(\"end\")",
        "main()",
        "",
    ];
    for t in THREAD_COUNTS {
        assert_deadlock(
            "a nursery-body defer that can never recv",
            t,
            &run_with_threads("d6.chz", &src, t),
        );
    }
}

#[test]
fn module_top_level_defer_that_can_never_complete_faults_deadlock() {
    let src = [
        "c := Channel[int](0)",
        "never := Channel[int](0)",
        "defer:",
        "    c.recv()",
        "never.recv()",
        "",
    ];
    for t in THREAD_COUNTS {
        assert_deadlock(
            "a module top-level defer that can never complete",
            t,
            &run_with_threads("d8.chz", &src, t),
        );
    }
}

#[test]
fn executor_job_and_main_nested_nursery_trees_fault_deadlock() {
    // h1 (W14-10): fixed by TICKET-135's D1; pinned here at every worker count.
    let src = [
        "import std.concurrency",
        "fn job(never: Channel[int]):",
        "    parallel:",
        "        spawn:",
        "            parallel:",
        "                spawn:",
        "                    never.recv()",
        "                never.recv()",
        "        never.recv()",
        "fn main():",
        "    never := Channel[int](0)",
        "    ex := Executor()",
        "    ex.submit(fn(): job(never))",
        "    parallel:",
        "        spawn:",
        "            parallel:",
        "                spawn:",
        "                    never.recv()",
        "                never.recv()",
        "        never.recv()",
        "    ex.shutdown()",
        "main()",
        "",
    ];
    for t in THREAD_COUNTS {
        assert_deadlock(
            "two 3-level parked nursery trees",
            t,
            &run_with_threads("h1.chz", &src, t),
        );
    }
}

#[test]
fn an_exit_from_a_job_outranks_a_main_defer_that_can_never_complete() {
    // The `ready.send` inside the `defer` (a handshake, DEC-050) releases the job only once `main`
    // is in the defer drain, so `os.exit(3)` always lands while `deferring > 0`. Go twin: exit 3.
    let src = [
        "import std.concurrency",
        "import std.os",
        "fn quit(ready: Channel[int]):",
        "    ready.recv()",
        "    os.exit(3)",
        "fn main():",
        "    c := Channel[int](0)",
        "    ready := Channel[int](1)",
        "    ex := Executor()",
        "    ex.submit(fn(): quit(ready))",
        "    defer:",
        "        ready.send(1)",
        "        c.recv()",
        "    print(\"x\")",
        "main()",
        "",
    ];
    for t in THREAD_COUNTS {
        let (out, err, code) = run_with_threads("exit3.chz", &src, t);
        assert_eq!(
            (out.as_str(), code),
            ("x\n", Some(3)),
            "a pending os.exit must outrank the defer's deadlock at CHEZZI_THREADS={t:?} (stderr: {err})"
        );
    }
}

/// Samples per (program, worker count) for the live-defer controls: a false deadlock on a defer
/// that a live task WILL feed is a rate, not a one-off, so it is sampled (>= 8).
const LIVE_DEFER_SAMPLES: usize = 8;

fn assert_live_defer_completes(name: &str, src: &[&str], expect_out: &str) {
    for t in THREAD_COUNTS {
        for i in 0..LIVE_DEFER_SAMPLES {
            let (out, err, code) = run_with_threads(name, src, t);
            assert!(
                code == Some(0) && out == expect_out,
                "sample {i} at CHEZZI_THREADS={t:?}: a defer that a live task will feed must complete \
                 (rc 0, stdout {expect_out:?}); got code {code:?}, stdout {out:?}, stderr: {err}"
            );
        }
    }
}

// Negative controls (parked is not stuck): each `defer` recvs from a channel a STILL-LIVE task will
// send to, ordered by a handshake (`go`), never a sleep (DEC-050). A false `deadlock` here is worse
// than the hang the widening of `is_counted_party` fixes.

#[test]
fn main_defer_that_a_live_job_will_feed_still_completes() {
    let src = [
        "import std.concurrency",
        "fn feed(go: Channel[int], c: Channel[int]):",
        "    go.recv()",
        "    c.send(42)",
        "fn main():",
        "    c := Channel[int](0)",
        "    go := Channel[int](1)",
        "    ex := Executor()",
        "    ex.submit(fn(): feed(go, c))",
        "    defer:",
        "        go.send(1)",
        "        print(c.recv())",
        "        ex.shutdown()",
        "    print(\"x\")",
        "main()",
        "",
    ];
    assert_live_defer_completes("live_main_defer.chz", &src, "x\n42\n");
}

#[test]
fn module_top_level_defer_that_a_live_job_will_feed_still_completes() {
    let src = [
        "import std.concurrency",
        "fn feed(go: Channel[int], c: Channel[int]):",
        "    go.recv()",
        "    c.send(42)",
        "c := Channel[int](0)",
        "go := Channel[int](1)",
        "ex := Executor()",
        "ex.submit(fn(): feed(go, c))",
        "defer:",
        "    go.send(1)",
        "    print(c.recv())",
        "    ex.shutdown()",
        "print(\"x\")",
        "",
    ];
    assert_live_defer_completes("live_top_defer.chz", &src, "x\n42\n");
}

#[test]
fn nursery_body_defer_that_a_live_outer_task_will_feed_still_completes() {
    // The inner body's `defer` runs after ITS join, on the outer body's thread, while the outer
    // nursery's spawned task is still live and will send once released.
    let src = [
        "fn feed(go: Channel[int], c: Channel[int]):",
        "    go.recv()",
        "    c.send(42)",
        "fn main():",
        "    c := Channel[int](0)",
        "    go := Channel[int](1)",
        "    parallel:",
        "        spawn:",
        "            feed(go, c)",
        "        parallel:",
        "            defer:",
        "                go.send(1)",
        "                print(c.recv())",
        "            spawn:",
        "                pass",
        "    print(\"end\")",
        "main()",
        "",
    ];
    assert_live_defer_completes("live_nursery_defer.chz", &src, "42\nend\n");
}
