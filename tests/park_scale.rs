//! A `Shared` guard wait and an `Executor.shutdown()` join in a fiber park their task (plan
//! 2026-11-08-park-guard-and-join, Tasks 2 and 3). 200 spawned tasks wait for a guard another task holds for 300 ms, at `--threads=2`: the run must
//! print `200` and report `demotes=0` under `CHEZZI_SCHED_STATS=1` (Go 1.27, `GOMAXPROCS=2`: 1000
//! `sync.Mutex` waiters start no thread). Before the plan each waiter demoted its worker
//! (`demotes=199`). 100 spawned tasks each `shutdown()` an Executor whose job waits for a sibling's
//! send: the same, `demotes=0` (Go 1.27: 1000 result waiters start no thread).
//!
//! The guard deadlocks end with the one global report (plan 2026-11-17) at T=1, 2 and 4: an AB-BA
//! lock cycle just blocks, and a guard wait in a `defer` of a cancelled task that nothing releases
//! blocks too; once every task is asleep the run lists each one at its wait, `main` first at its
//! `parallel:` join (Go twin of each: fatal `all goroutines are asleep - deadlock!`).

use std::process::{Command, Stdio};

/// Runs `src` at `--threads=<threads>` under `CHEZZI_SCHED_STATS=1`: (exit code, stdout, stderr
/// without the stats line, demotes).
fn run(name: &str, src: &str, threads: &str) -> (i32, String, String, Option<usize>) {
    let dir =
        std::env::temp_dir().join(format!("chz-park-{name}-{threads}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::write(dir.join("main.chz"), src).expect("write program");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", &format!("--threads={threads}"), "main.chz"])
        .current_dir(&dir)
        .env("CHEZZI_SCHED_STATS", "1")
        .stdin(Stdio::null())
        .output()
        .expect("run chezzi");
    let _ = std::fs::remove_dir_all(&dir);
    let mut demotes = None;
    let err: String = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| match l.strip_prefix("[sched-stats] ") {
            Some(stats) => {
                demotes = stats
                    .split(' ')
                    .find_map(|f| f.strip_prefix("demotes="))
                    .and_then(|n| n.parse().ok());
                false
            }
            None => true,
        })
        .map(|l| format!("{l}\n"))
        .collect();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        err,
        demotes,
    )
}

const WAITERS: &str = "import Shared from std.concurrency
import std.time
s := Shared[int](0)
fn hold(v: int) -> int:
    time.sleep_ms(300)
    return v
fn inc(v: int) -> int:
    return v + 1
fn main():
    parallel:
        spawn:
            s.update(hold)
        for _ in 0..200:
            spawn:
                time.sleep_ms(20)
                s.update(inc)
    print(s.get())
main()
";

#[test]
fn two_hundred_guard_waiters_park() {
    let (code, out, err, demotes) = run("waiters", WAITERS, "2");
    assert_eq!((code, out.as_str()), (0, "200\n"), "stderr: {err}");
    assert_eq!(
        demotes,
        Some(0),
        "every guard waiter must park; stderr: {err}"
    );
}

const JOINERS: &str = "import Executor from std.concurrency
fn main():
    ch := Channel[int](0)
    total := Channel[int](100)
    parallel:
        for _ in 0..100:
            spawn:
                ex := Executor()
                _ := ex.submit(fn() -> None:
                    match ch.recv():
                        ?v: total.send(v)
                        !e: print(e.error())
                )
                ex.shutdown()
        for _ in 0..100:
            spawn:
                ch.send(1)
    n := 0
    for _ in 0..100:
        match total.recv():
            ?v: n += v
            !e: print(e.error())
    print(n)
main()
";

#[test]
fn hundred_executor_joiners_park() {
    let (code, out, err, demotes) = run("joiners", JOINERS, "2");
    assert_eq!((code, out.as_str()), (0, "100\n"), "stderr: {err}");
    assert_eq!(
        demotes,
        Some(0),
        "every Executor joiner must park; stderr: {err}"
    );
}

const ABBA: &str = "import Shared from std.concurrency
import std.time
a := Shared[int](0)
b := Shared[int](0)
fn inc(x: int) -> int:
    return x + 1
fn ab(v: int) -> int:
    time.sleep_ms(50)
    b.update(inc)
    return v
fn ba(v: int) -> int:
    time.sleep_ms(150)
    a.update(inc)
    return v
fn main():
    parallel:
        spawn:
            a.update(ab)
        spawn:
            b.update(ba)
main()
";

/// Both holders block at their second `update` (Go twin: AB-BA `sync.Mutex` is fatal, all asleep).
const ABBA_ERR: &str = "runtime error (main.chz:16:5): deadlock: every task is blocked
  task 1: waiting for a nursery join at main.chz:16:5
    at main (called at main.chz:21:1)
  task 2: waiting for the Shared guard at main.chz:9:5
    at ab (called at main.chz:18:13)
    at <spawned task> (called at main.chz:17:9)
  task 3: waiting for the Shared guard at main.chz:13:5
    at ba (called at main.chz:20:13)
    at <spawned task> (called at main.chz:19:9)
";

#[test]
fn ab_ba_guard_cycle_blocks_and_gets_the_global_report() {
    for t in ["1", "2", "4"] {
        let (code, out, err, _) = run("abba", ABBA, t);
        assert_eq!(
            (code, out.as_str(), err.as_str()),
            (1, "", ABBA_ERR),
            "T={t}"
        );
    }
}

const DEFER_CUT: &str = "import Shared from std.concurrency
import std.time
s := Shared[int](0)
ch := Channel[int](0)
fn hold(v: int) -> int:
    _ := ch.recv()
    return v
fn inc(v: int) -> int:
    return v + 1
fn main():
    parallel:
        spawn:
            defer s.update(hold)
            time.sleep_ms(300)
        spawn:
            defer s.update(inc)
            time.sleep_ms(300)
        spawn:
            time.sleep_ms(50)
            panic(\"boom\")
main()
";

/// TICKET-298 (owner answer 2(b)): the stuck `defer` holds the guard; once the panic cancels both
/// sleepers, every task is asleep and the run lists each at its wait: `main` at its join, the
/// holder at its `recv()`, and the other `defer` at the guard unless it took the guard first and
/// ended (Go twin: fatal, all asleep; the panic is not reported).
const DEFER_CUT_ERR: &str = "runtime error (main.chz:11:5): deadlock: every task is blocked
  task 1: waiting for a nursery join at main.chz:11:5
    at main (called at main.chz:21:1)
  task 2: recv() on an empty channel at main.chz:6:10
    at hold (called at main.chz:13:19)
    at <spawned task> (called at main.chz:12:9)
  task 3: waiting for the Shared guard at main.chz:16:19
    at <spawned task> (called at main.chz:15:9)
";

/// The order of `DEFER_CUT` where the `inc` `defer` takes the guard first and ends.
const DEFER_CUT_INC_FIRST_ERR: &str =
    "runtime error (main.chz:11:5): deadlock: every task is blocked
  task 1: waiting for a nursery join at main.chz:11:5
    at main (called at main.chz:21:1)
  task 2: recv() on an empty channel at main.chz:6:10
    at hold (called at main.chz:13:19)
    at <spawned task> (called at main.chz:12:9)
";

#[test]
fn a_guard_holder_stuck_in_a_cancelled_defer_is_a_fatal_deadlock() {
    for t in ["1", "2", "4"] {
        let (code, out, err, _) = run("dcut", DEFER_CUT, t);
        assert!(
            code == 1 && out.is_empty() && (err == DEFER_CUT_ERR || err == DEFER_CUT_INC_FIRST_ERR),
            "T={t}: rc={code} stdout={out:?}\n{err}"
        );
    }
}

/// A nested nursery deadlocks inside `hold`, which holds the guard; its sibling task waits for
/// that guard (`NESTED_GUARD`) or on a channel `hold` never sends to (`NESTED_RECV`). The listing
/// holds every task at T=1, 2 and 4: `main` at its join, `hold`'s task at its inner join, the inner
/// child at the guard, and the sibling (Go twin: fatal, all asleep).
const NESTED_GUARD: &str = "import Shared from std.concurrency
import std.time
fn inc(v: int) -> int:
    return v + 1
fn sib(v: int) -> int:
    print(\"sibling took\", v)
    return v + 100
fn main():
    s := Shared[int](0)
    fn hold(v: int) -> int:
        print(\"hold took\", v)
        time.sleep_ms(50)
        parallel:
            spawn:
                s.update(inc)
        return v + 1
    parallel:
        spawn:
            s.update(hold)
        spawn:
            s.update(sib)
    print(s.get())
main()
";

const NESTED_GUARD_ERR: &str = "runtime error (main.chz:17:5): deadlock: every task is blocked
  task 1: waiting for a nursery join at main.chz:17:5
    at main (called at main.chz:23:1)
  task 2: waiting for a nursery join at main.chz:13:9
    at hold (called at main.chz:19:13)
    at <spawned task> (called at main.chz:18:9)
  task 3: waiting for the Shared guard at main.chz:15:17
    at <spawned task> (called at main.chz:14:13)
  task 4: waiting for the Shared guard at main.chz:21:13
    at <spawned task> (called at main.chz:20:9)
";

/// The other order of `NESTED_GUARD` (the sibling takes the guard first; at T=4 on base too): the
/// sibling ends, and only `hold`'s nested child is blocked.
const NESTED_GUARD_SIB_FIRST_ERR: &str = "runtime error (main.chz:17:5): deadlock: every task is \
blocked
  task 1: waiting for a nursery join at main.chz:17:5
    at main (called at main.chz:23:1)
  task 2: waiting for a nursery join at main.chz:13:9
    at hold (called at main.chz:19:13)
    at <spawned task> (called at main.chz:18:9)
  task 3: waiting for the Shared guard at main.chz:15:17
    at <spawned task> (called at main.chz:14:13)
";

const NESTED_RECV: &str = "import Shared from std.concurrency
fn inc(v: int) -> int:
    return v + 1
fn main():
    s := Shared[int](0)
    ch := Channel[int](0)
    fn hold(v: int) -> int:
        print(\"hold took\", v)
        parallel:
            spawn:
                s.update(inc)
        ch.send(1)
        return v + 1
    parallel:
        spawn:
            s.update(hold)
        spawn:
            match ch.recv():
                ?x: print(\"sib got\", x)
                !e: print(e.error())
    print(s.get())
main()
";

const NESTED_RECV_ERR: &str = "runtime error (main.chz:14:5): deadlock: every task is blocked
  task 1: waiting for a nursery join at main.chz:14:5
    at main (called at main.chz:22:1)
  task 2: waiting for a nursery join at main.chz:9:9
    at hold (called at main.chz:16:13)
    at <spawned task> (called at main.chz:15:9)
  task 3: waiting for the Shared guard at main.chz:11:17
    at <spawned task> (called at main.chz:10:13)
  task 4: recv() on an empty channel at main.chz:18:19
    at <spawned task> (called at main.chz:17:9)
";

#[test]
fn nested_verdict_lists_a_parked_guard_waiter() {
    for (name, src, want) in [
        ("nguard", NESTED_GUARD, NESTED_GUARD_ERR),
        ("nrecv", NESTED_RECV, NESTED_RECV_ERR),
    ] {
        // 20 runs per worker count: a lost waiter shows in a minority of runs.
        for t in ["1", "2", "4"] {
            for i in 0..20 {
                let (code, out, err, _) = run(name, src, t);
                // Which task takes the guard first is a race; each order has one right listing.
                let want = if out == "sibling took 0\nhold took 100\n" {
                    NESTED_GUARD_SIB_FIRST_ERR
                } else {
                    assert_eq!(out, "hold took 0\n", "{name} T={t} run {i}");
                    want
                };
                assert_eq!((code, err.as_str()), (1, want), "{name} T={t} run {i}");
            }
        }
    }
}

/// A test that deadlocks while it holds a module-global `Shared` leaves the box cut for the rest of
/// the run: a later test fn that touches it fails with a deadlock at its own `update` (owner answer
/// 1(a), TICKET-298; Go blocks every `Lock` of a `Mutex` nobody unlocked: fatal, all asleep). Each
/// test's verdict is its own. The text at T=1, 2 and 4.
const CUT_THEN_TEST: &str = "import Shared from std.concurrency
s := Shared[int](0)
fn inc(v: int) -> int:
    return v + 1
fn stuck(v: int) -> int:
    ch := Channel[int](0)
    match ch.recv():
        ?x: return x
        !e: return v
test fn holds_and_deadlocks():
    s.update(stuck)
test fn updates_after():
    s.update(inc)
    assert s.get() == 1
";

const CUT_THEN_TEST_OUT: &str =
    "ERROR holds_and_deadlocks (main_test.chz:7:11) deadlock: every task is blocked
  task 1: recv() on an empty channel at main_test.chz:7:11
    at stuck (called at main_test.chz:11:5)
ERROR updates_after (main_test.chz:13:5) deadlock: every task is blocked
  task 1: waiting for the Shared guard at main_test.chz:13:5

2 test(s): 0 passed, 0 failed, 2 errored
";

#[test]
fn a_test_after_a_cut_holder_cannot_take_the_guard() {
    for t in ["1", "2", "4"] {
        let dir = std::env::temp_dir().join(format!("chz-park-cuttest-{t}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("main_test.chz"), CUT_THEN_TEST).expect("write program");
        let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .args(["test", "main_test.chz"])
            .current_dir(&dir)
            .env("CHEZZI_THREADS", t)
            .stdin(Stdio::null())
            .output()
            .expect("run chezzi");
        let _ = std::fs::remove_dir_all(&dir);
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            (out.status.code(), all.as_str()),
            (Some(1), CUT_THEN_TEST_OUT),
            "T={t}"
        );
    }
}

/// A holder parked in a `defer` of its `update` closure, inside a nested nursery: the holder keeps
/// the guard while it waits, so the sibling that waits for the guard never takes it, and once
/// every task is asleep the run ends with the one report (review S1b; Go twin: fatal, all asleep).
const DEFER_HOLDER: &str = "import Shared from std.concurrency
import std.time
fn sib(v: int) -> int:
    print(\"sibling took\", v)
    return v + 100
fn main():
    s := Shared[int](0)
    ch := Channel[int](0)
    fn f(v: int) -> int:
        defer ch.recv()
        print(\"f took\", v)
        return v + 1
    parallel:
        spawn:
            parallel:
                spawn:
                    s.update(f)
        spawn:
            time.sleep_ms(100)
            s.update(sib)
    print(s.get())
main()
";

#[test]
fn a_cut_holder_in_a_defer_gives_its_guard_to_no_waiter() {
    for t in ["1", "2", "4"] {
        for i in 0..5 {
            let (code, out, err, _) = run(&format!("dholder{i}"), DEFER_HOLDER, t);
            assert!(
                code == 1
                    && out == "f took 0\n"
                    && err.contains("deadlock: every task is blocked")
                    && err.contains("waiting for the Shared guard at main.chz:20:13"),
                "T={t} run {i}: rc={code} stdout={out:?} stderr={err:?}"
            );
        }
    }
}
