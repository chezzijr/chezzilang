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
    let dir = std::env::temp_dir().join(format!("chz-t286-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(format!("{name}_test.chz"));
    std::fs::write(&path, src).expect("write program");
    let out = Command::new("timeout")
        .arg("30")
        .arg(env!("CARGO_BIN_EXE_chezzi"))
        .arg("test")
        .arg("--show-output")
        .arg(format!("--timeout={timeout_ms}"))
        .arg(&path)
        .output()
        .expect("spawn chezzi");
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
const GRID_HDR: &str = "import std.process\nimport std.time\nimport std.os\nimport Shared from std.concurrency\nch := Channel[int](0)\nsh := Shared(0)\nfn op_recv(n: int) -> int:\n    v := ch.recv() else: panic(\"ch closed\")\n    return v\nfn op_send(n: int) -> int:\n    ch.send(n)\n    return n\nfn op_wait(n: int) -> int:\n    wait:\n        v := ch.recv():\n            return v\n    return 0\nfn op_sleep(n: int) -> int:\n    time.sleep_ms(3000)\n    return n\nfn op_timer(n: int) -> int:\n    _ := time.timer(3000).recv()\n    return n\nfn op_guard(n: int) -> int:\n    sh.update(fn(v: int) -> int: v + 1)\n    return n\nfn slow(v: int) -> int:\n    time.sleep_ms(3000)\n    return v\nfn hold(n: int) -> int:\n    sh.update(slow)\n    return n\n";
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
