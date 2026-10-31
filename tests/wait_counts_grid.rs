//! The counts a wait registers for the two deadlock verdicts come from one table,
//! `block::wait_steps` (party, body marks, waiter or `inflight`, blocked owner). This grid runs
//! each wait kind in each context a program reaches, as a genuine deadlock and as a live program,
//! at `CHEZZI_THREADS` 1, 2 and the default. One test per (op, context). The `Builder` context
//! is not a cell: a program reaches it only when the OS refuses the eager nursery's drainer thread.
//! `vm::tests::extern_mnsched::w758_only_a_thread_with_no_scheduler_under_it_counts_a_nursery_party`
//! runs its `Nursery` party cell on a real `Vm`.
//!
//! Expected results are Go 1.27 run output of the same shapes (`GOMAXPROCS=1` and 4): every dead
//! cell prints `fatal error: all goroutines are asleep - deadlock!`, every live cell prints its
//! value, and a `select` with a `time.After` arm takes the timer. Two kinds of cell differ from Go
//! by decision:
//! - `main_cb` (main inside a native callback) is an unjudged party (DEC-136, DEC-181): a dead
//!   channel, guard or join wait there hangs, never settles (`docs/contributing/gaps.md` W23-1). A
//!   nursery join there is a party (the `1 +` of `live`), so it reports the deadlock.
//! - `job` reports the deadlock of a job's own wait as a fault of the run, as Go does.
//! - `main_defer` (main's `defer:`, a judged party) reports the deadlock, as Go's deferred
//!   receive does.
//!
//! Each program's `time.sleep_ms` delays a feeder; a cell's outcome does not depend on the order of
//! the feed and the wait.

#[path = "support/grid_tests.rs"]
#[macro_use]
mod grid_tests;
#[path = "support/hang_deadline.rs"]
mod hang_deadline;

const PRELUDE: &str = "import std.time
import Executor, Shared from std.concurrency

fn feed(c: Channel[int]):
    time.sleep_ms(50)
    c.send(1)

fn drain(c: Channel[int]):
    time.sleep_ms(50)
    _ := c.recv()

fn started(g: Channel[int]) -> int:
    g.send(1)
    return 0

fn hold(s: Shared[int], c: Channel[int], g: Channel[int]):
    s.update(fn(x: int) -> int: x + started(g) + c.recv())

fn blockr(c: Channel[int]):
    _ := c.recv()

fn noop():
    pass

fn runner(c: Channel[int], s: Shared[int], g: Channel[int]):
    v := op(c, s, g)
    print(\"r {v}\")

fn runner_cb(c: Channel[int], s: Shared[int], g: Channel[int]):
    ys := [0].map(fn(i: int) -> int: op(c, s, g))
    print(\"r {ys[0]}\")

";

/// The body of `fn op` for each wait kind, and the feeder that ends it in a live cell.
fn op_body(op: &str) -> (&'static str, &'static str) {
    match op {
        "recv" => ("    return c.recv()\n", "feed"),
        "send" => ("    c.send(1)\n    c.send(2)\n    return 0\n", "drain"),
        "wait" => (
            "    wait:\n        v := c.recv():\n            return v\n    return 0\n",
            "feed",
        ),
        "wait_timer" => (
            "    t := time.timer(150)\n    wait:\n        v := c.recv():\n            return v\n        _ := t.recv():\n            return 9\n    return 0\n",
            "feed",
        ),
        "guard" => (
            "    _ := g.recv()\n    s.update(fn(x: int) -> int: x + 1)\n    return s.get()\n",
            "feed",
        ),
        "join" => (
            "    ex := Executor(1)\n    _ := ex.submit(fn(): blockr(c))\n    ex.shutdown()\n    return 0\n",
            "feed",
        ),
        "nursery" => (
            "    parallel:\n        spawn blockr(c)\n    return 0\n",
            "feed",
        ),
        other => panic!("no op {other}"),
    }
}

/// Where `op` runs, as the lines of `main`.
fn ctx_lines(ctx: &str) -> &'static str {
    match ctx {
        "main" => "    v := op(c, s, g)\n    print(\"r {v}\")\n",
        "body" => {
            "    parallel:\n        spawn noop()\n        v := op(c, s, g)\n        print(\"r {v}\")\n"
        }
        "task" => "    parallel:\n        spawn runner(c, s, g)\n",
        "task_cb" => "    parallel:\n        spawn runner_cb(c, s, g)\n",
        "job" => {
            "    jx := Executor(1)\n    t := jx.submit(fn() -> int: op(c, s, g))\n    match t.get():\n        ?v: print(\"r {v}\")\n        !e: print(\"e {e.message()}\")\n"
        }
        "main_cb" => {
            "    ys := [0].map(fn(i: int) -> int: op(c, s, g))\n    print(\"r {ys[0]}\")\n"
        }
        "main_defer" => "    defer:\n        v := op(c, s, g)\n        print(\"r {v}\")\n",
        other => panic!("no context {other}"),
    }
}

fn program(op: &str, ctx: &str, live: bool) -> String {
    let (body, feeder) = op_body(op);
    let mut p = format!(
        "{PRELUDE}fn op(c: Channel[int], s: Shared[int], g: Channel[int]) -> int:\n{body}\nfn main():\n    c := Channel[int](1)\n    s := Shared(0)\n    g := Channel[int](1)\n    fx := Executor(1)\n    hx := Executor(1)\n"
    );
    if op == "guard" {
        p.push_str("    _ := hx.submit(fn(): hold(s, c, g))\n");
    }
    if live {
        p.push_str(&format!("    _ := fx.submit(fn(): {feeder}(c))\n"));
    }
    p.push_str(ctx_lines(ctx));
    p.push_str("main()\nprint(\"end\")\n");
    p
}

/// What a cell must print or report.
enum Want {
    /// exit 0, one of these `r` lines, then `end`
    Settles(&'static [&'static str]),
    /// a nonzero exit whose stderr names a deadlock
    Deadlock,
    /// W23-1: a hang (killed at the deadline) or a deadlock fault, never a settle
    Declines,
}

fn want(op: &str, ctx: &str, live: bool) -> Want {
    if op == "wait_timer" {
        return Want::Settles(if live { &["r 1", "r 9"] } else { &["r 9"] });
    }
    if live {
        return Want::Settles(match op {
            "recv" | "wait" => &["r 1"],
            "guard" => &["r 2"],
            _ => &["r 0"],
        });
    }
    if ctx == "main_cb" && op != "nursery" {
        return Want::Declines;
    }
    Want::Deadlock
}

fn cell(op: &str, ctx: &str) {
    let dir =
        std::env::temp_dir().join(format!("chz-wait-counts-{}-{op}-{ctx}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut misses = Vec::new();
    for live in [false, true] {
        let path = dir.join(format!("{}.chz", if live { "live" } else { "dead" }));
        std::fs::write(&path, program(op, ctx, live)).unwrap();
        for threads in [Some("1"), Some("2"), None] {
            let got = hang_deadline::run_with_hang_deadline(&path, threads);
            let shown = match &got {
                None => "killed at the deadline".to_string(),
                Some(o) => format!(
                    "status {:?}\n--stdout--\n{}--stderr--\n{}",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                ),
            };
            let deadlock = got.as_ref().is_some_and(|o| {
                !o.status.success() && String::from_utf8_lossy(&o.stderr).contains("deadlock")
            });
            let ok = match want(op, ctx, live) {
                Want::Settles(lines) => got.as_ref().is_some_and(|o| {
                    let out = String::from_utf8_lossy(&o.stdout);
                    o.status.success() && lines.iter().any(|l| out == format!("{l}\nend\n"))
                }),
                Want::Deadlock => deadlock,
                Want::Declines => got.is_none() || deadlock,
            };
            if !ok {
                misses.push(format!(
                    "{op} x {ctx} x {} at CHEZZI_THREADS={threads:?}: {shown}",
                    if live { "live" } else { "dead" }
                ));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(misses.is_empty(), "{}", misses.join("\n\n"));
}

grid_tests!(OPS x CTXS: [recv, send, wait, wait_timer, guard, join, nursery]
    x [main, body, task, task_cb, job, main_cb, main_defer] => crate::cell);
