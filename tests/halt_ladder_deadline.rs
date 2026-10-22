//! TICKET-286 (concurrency-facts family 3): "which halts end a blocked party, in which order".
//! `Vm::block_halts` is the one ladder with the `--timeout` rung; a wait loop with its own copy
//! misses it, and a timed-out test leaks its open nursery into the next test. Each test drives
//! `chezzi test --timeout=300` on a file and reads the report.

use std::process::Command;

/// Runs `chezzi test --timeout=300` on `src` and returns stdout + stderr.
fn run_timeout_test(name: &str, src: &str) -> String {
    let dir = std::env::temp_dir().join(format!("chz-t286-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(format!("{name}_test.chz"));
    std::fs::write(&path, src).expect("write program");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["test", "--timeout=300"])
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
    return ch.recv()
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
