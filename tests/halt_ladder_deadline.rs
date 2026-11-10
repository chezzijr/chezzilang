//! TICKET-286 (concurrency-facts family 3): "which halts end a blocked party, in which order".
//! `Vm::block_halts` is the one ladder with the `--timeout` rung; a wait loop with its own copy
//! misses it, and a timed-out test leaks its open nursery into the next test. Each test drives
//! `chezzi test --timeout=300` on a file and reads the report.

use std::process::Command;

/// Runs `chezzi test --timeout=300` on `src` and returns stdout + stderr.
fn run_timeout_test(name: &str, src: &str) -> String {
    run_test_file(name, src, "300")
}

/// Runs `chezzi test --show-output --timeout=<timeout_ms>` on `src` under `timeout 30` (a hung run
/// is killed and prints no summary line) and returns stdout + stderr.
fn run_test_file(name: &str, src: &str, timeout_ms: &str) -> String {
    run_test_file_at(name, src, &format!("--timeout={timeout_ms}"), None)
}

/// [`run_test_file`] with any halt `flag` (`--timeout=N`, `--max-heap=N`) and, when `threads` is
/// `Some`, that `CHEZZI_THREADS` worker count.
fn run_test_file_at(name: &str, src: &str, flag: &str, threads: Option<&str>) -> String {
    let dir = std::env::temp_dir().join(format!("chz-t286-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(format!("{name}_test.chz"));
    std::fs::write(&path, src).expect("write program");
    let mut cmd = Command::new("timeout");
    cmd.arg("30")
        .arg(env!("CARGO_BIN_EXE_chezzi"))
        .arg("test")
        .arg("--show-output")
        .arg(flag)
        .arg(&path);
    if let Some(t) = threads {
        cmd.env("CHEZZI_THREADS", t);
    }
    let out = cmd.output().expect("spawn chezzi");
    let _ = std::fs::remove_dir_all(&dir);
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

const J_LEAK: &str = r#"import std.time
test fn a_first():
    parallel:
        spawn:
            time.sleep_ms(1000)
        time.sleep_ms(1000)
test fn b_sleep_ok():
    time.sleep_ms(10)
"#;

const J_2A: &str = r#"import std.process
ch := Channel[int](0)
fn r(n: int) -> int:
    got_ch := ch.recv() else: panic("ch closed")
    return got_ch
test fn t():
    parallel:
        spawn:
            _ := [0].map(r)
        spawn:
            _ := process.run("sleep 0.5")
            panic("boom")
"#;

#[test]
fn timed_out_test_does_not_leak_its_halt_into_the_next_test() {
    let got = run_timeout_test("jleak", J_LEAK);
    assert!(
        got.contains("1 passed, 0 failed, 0 errored, 1 timed out"),
        "b_sleep_ok must pass after a_first times out; got:\n{got}"
    );
}

#[test]
fn recv_in_a_native_callback_reads_the_deadline_rung() {
    let got = run_timeout_test("j2a", J_2A);
    assert!(
        got.contains("TIMED-OUT t") && got.contains("test exceeded --timeout (300ms)"),
        "the 300 ms deadline lands before the 500 ms panic; got:\n{got}"
    );
}

/// The grid program: one shared channel and guard, one `op_<wait>` per wait kind. `hold` keeps the
/// guard for 3 s inside a callback sleep, so `op_guard` waits on it.
const GRID_HDR: &str = "import std.process\nimport std.time\nimport std.os\nimport Shared from std.concurrency\nch := Channel[int](0)\nsh := Shared(0)\nfn op_recv(n: int) -> int:\n    v := ch.recv() else: panic(\"ch closed\")\n    return v\nfn op_send(n: int) -> int:\n    ch.send(n)\n    return n\nfn op_wait(n: int) -> int:\n    wait:\n        r := ch.recv():\n            v := r else: panic(\"ch closed\")\n            return v\n    return 0\nfn op_sleep(n: int) -> int:\n    time.sleep_ms(3000)\n    return n\nfn op_timer(n: int) -> int:\n    _ := time.timer(3000).recv()\n    return n\nfn op_guard(n: int) -> int:\n    sh.update(fn(v: int) -> int: v + 1)\n    return n\nfn slow(v: int) -> int:\n    time.sleep_ms(3000)\n    return v\nfn hold(n: int) -> int:\n    sh.update(slow)\n    return n\n";
/// The task that holds the guard before `op_guard` asks for it.
const GUARD_HOLDER: &str = "        spawn:\n            _ := hold(0)\n        time.sleep_ms(50)\n";
/// The wait kinds a blocked party sits in.
const WAITS: &[&str] = &["recv", "send", "wait", "sleep", "timer", "guard"];
/// Where the wait runs: straight in a spawn (a park), or inside a native callback (a demote loop).
const CONTEXTS: &[(&str, &str)] = &[
    ("direct", "_ := op_{op}(0)"),
    ("callback", "_ := [0].map(op_{op})"),
];
/// (halt, the sibling's body, `--timeout` ms, the report fragment the cell must print).
/// `timeout`: the sibling sits in a 1 s host call, then prints `LATE`. No other halt exists, so only
/// the deadline rung ends the wait before `LATE`; a cell that prints `LATE` waited past it.
/// `order`: the deadline (300 ms) lands before the sibling's fault (500 ms); the wait wakes on the
/// fault, and its ladder must still pick the deadline.
const HALTS: &[(&str, &str, &str, &str)] = &[
    (
        "timeout",
        "            _ := process.run(\"sleep 1\")\n            print(\"LATE\")\n",
        "300",
        "TIMED-OUT t (",
    ),
    (
        "order",
        "            _ := process.run(\"sleep 0.5\")\n            panic(\"boom\")\n",
        "300",
        "TIMED-OUT t (",
    ),
    (
        "fault",
        "            time.sleep_ms(100)\n            panic(\"boom\")\n",
        "5000",
        ") boom",
    ),
    (
        "exit",
        "            time.sleep_ms(100)\n            os.exit(3)\n",
        "5000",
        ") exit",
    ),
];

#[test]
fn every_wait_kind_reports_the_halt_that_lands_first() {
    let mut bad = Vec::new();
    for (halt, sib, ms, want) in HALTS {
        for op in WAITS {
            for (ctx, call) in CONTEXTS {
                let holder = if *op == "guard" { GUARD_HOLDER } else { "" };
                let call = call.replace("{op}", op);
                let src = format!(
                    "{GRID_HDR}test fn t():\n    parallel:\n{holder}        spawn:\n            {call}\n        spawn:\n{sib}"
                );
                let got = run_test_file(&format!("grid-{halt}-{op}-{ctx}"), &src, ms);
                if !got.contains(want) || got.contains("LATE") {
                    bad.push(format!("{halt} x {op} x {ctx}: {got}"));
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} cells disagree:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// A timed-out test whose child sits in a `defer` that blocks forever on a `recv` nobody can send.
const DEFER_FOREVER: &str = r#"import std.time
ch := Channel[int](0)
test fn a_first():
    parallel:
        spawn:
            defer:
                _ := ch.recv()
            print("in body")
        time.sleep_ms(1000)
test fn b_sleep_ok():
    time.sleep_ms(10)
"#;

/// A timed-out test whose child sits in a 3 s host call.
const HOST_CALL_CHILD: &str = r#"import std.process
import std.time
test fn a_first():
    parallel:
        spawn:
            _ := process.run("sleep 3")
        time.sleep_ms(1000)
test fn b_sleep_ok():
    time.sleep_ms(10)
"#;

#[test]
fn a_child_blocked_in_a_defer_does_not_hang_the_run_after_a_timeout() {
    let got = run_timeout_test("deferforever", DEFER_FOREVER);
    assert!(
        got.contains("TIMED-OUT a_first")
            && got.contains("2 test(s): 1 passed, 0 failed, 0 errored, 1 timed out"),
        "the run must end, with a_first timed out and b_sleep_ok passing; got:\n{got}"
    );
}

#[test]
fn a_child_in_a_host_call_does_not_cut_the_next_test_after_a_timeout() {
    let got = run_timeout_test("hostchild", HOST_CALL_CHILD);
    assert!(
        got.contains("TIMED-OUT a_first")
            && got.contains("2 test(s): 1 passed, 0 failed, 0 errored, 1 timed out"),
        "the run must end, with a_first timed out and b_sleep_ok passing; got:\n{got}"
    );
}

/// TICKET-299 (wave 24 F2): a `--timeout` hard halt inside the inner of two `parallel:` levels.
/// The halt unwinds synchronously with no level abort (TICKET-135 keeps `report` false for a
/// hard halt): every body `defer` runs, innermost first, and no child is joined, so a child's
/// `defer` output is not asserted (measured: `C` at `CHEZZI_THREADS=1` only).
const J_LEVELS: &str = r#"import std.time
fn kid(tag: str, up: Channel[int], gate: Channel[int]):
    defer:
        _ := gate.recv()
        print(tag)
    up.send(1)
    while true:
        time.sleep_ms(1)
test fn a_timeout_inside_the_inner_level():
    up := Channel[int](2)
    g1 := Channel[int](1)
    g2 := Channel[int](1)
    parallel:
        spawn kid("c", up, g2)
        _ := up.recv()
        defer:
            print("b")
            g2.send(1)
        parallel:
            spawn kid("C", up, g1)
            _ := up.recv()
            defer:
                print("B")
                g1.send(1)
            while true:
                time.sleep_ms(1)
test fn b_after():
    print("after")
"#;

#[test]
fn a_hard_halt_inside_two_levels_runs_both_body_defers_innermost_first() {
    for t in ["1", "2"] {
        let out = run_test_file_at("t299_levels", J_LEVELS, "--timeout=300", Some(t));
        assert!(
            out.contains("a_timeout_inside_the_inner_level")
                && out.contains("test exceeded --timeout (300ms)\n    B\n    b\n"),
            "T={t}: the halt runs B, then b: {out}"
        );
        assert!(
            out.contains("PASS b_after"),
            "T={t}: the next test runs: {out}"
        );
    }
}

/// TICKET-299 (wave 24 F2): a hard halt in a CANCELLED task, inside two `parallel:` levels.
/// `faulty`'s fault cancels `victim`; `victim` leaves its inner level (body `defer`s `B1`, then
/// `B2`) and `B1` halts. The kids park on `hold`, so only a level abort ends them; their `defer`
/// waits on the gate the level's `B2` / `b` sends on. `@HALT@` and `@CHECK@` are whole lines.
const J_CANCELLED: &str = r#"import std.time
import Shared from std.concurrency
hold: Channel[int] = Channel[int]()
fn note(log: Shared[str], s: str):
    log.update(fn(t: str) -> str: t + s)
fn kid(tag: str, log: Shared[str], up: Channel[int], gate: Channel[int]):
    defer:
        _ := gate.recv()
        note(log, tag)
    up.send(1)
    _ := hold.recv()
fn victim(log: Shared[str], go: Channel[int]):
    up := Channel[int](2)
    g1 := Channel[int](1)
    g2 := Channel[int](1)
    parallel:
        spawn kid("c", log, up, g2)
        _ := up.recv()
        defer:
            note(log, "b")
            g2.send(1)
        parallel:
            spawn kid("C", log, up, g1)
            _ := up.recv()
            defer:
                note(log, "B2")
                g1.send(1)
            defer:
                note(log, "B1")
@HALT@
            go.send(1)
            while true:
                time.sleep_ms(1)
fn faulty(go: Channel[int]):
    _ := go.recv()
    panic("sib")
test fn a_halt_in_a_cancelled_task():
    log := Shared("")
    go := Channel[int](1)
    r := recover:
        parallel:
            spawn victim(log, go)
            spawn faulty(go)
    assert "{r}" == "!sib", "{r}"
@CHECK@
test fn b_after():
    print("after")
"#;
/// `--timeout`: the offloaded sleep ends with the halt while `victim` is cancelled, so the
/// worker loop's hard-halt arm unwinds through `unwind_deferred_escaped` with
/// `report_escaped && run_defers` and leaves both levels there. The run-wide halt also ends the
/// kids, so their letters are stripped (measured: `C` in 1 of 7 T=2 runs).
const TIMEOUT_HALT: &str = "                time.sleep_ms(60000)";
const TIMEOUT_CHECK: &str =
    "    assert log.get().replace(\"C\", \"\").replace(\"c\", \"\") == \"B1B2b\", log.get()";
/// `--max-heap`: only `victim` allocates, so the halt stays the running drain's fault and the
/// level leave goes on: `B2`, the inner abort (`C`), `b`, the outer abort (`c`).
const HEAP_HALT: &str = "                xs: List[str] = []\n                i := 0\n                while true:\n                    xs.push(\"{i} abcdefghijklmnopqrstuvwxyz0123456789abcd\")\n                    i += 1";
const HEAP_CHECK: &str = "    assert log.get() == \"B1B2Cbc\", log.get()";

#[test]
fn a_hard_halt_in_a_cancelled_task_leaves_its_levels_body_defers_first() {
    for t in ["1", "2"] {
        for (name, flag, halt, check) in [
            (
                "t299_cancel_timeout",
                "--timeout=300",
                TIMEOUT_HALT,
                TIMEOUT_CHECK,
            ),
            (
                "t299_cancel_heap",
                "--max-heap=20000000",
                HEAP_HALT,
                HEAP_CHECK,
            ),
        ] {
            let src = J_CANCELLED
                .replace("@HALT@", halt)
                .replace("@CHECK@", check);
            let out = run_test_file_at(name, &src, flag, Some(t));
            assert!(
                out.contains("PASS a_halt_in_a_cancelled_task") && out.contains("PASS b_after"),
                "T={t} {flag}: {out}"
            );
        }
    }
}
