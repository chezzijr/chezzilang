//! TICKET-135 (W14 D1) — a deadlock is FATAL (Go's `all goroutines are asleep`), not
//! `recover:`-able. Boundary: value/operation faults recover; scheduler / whole-program faults
//! (deadlock, `os.exit`, resource caps) do not. Before the fix `recover:` turns the verdict into an
//! `Err` and the program keeps running.

use std::process::Command;

#[test]
fn recover_does_not_catch_a_deadlock() {
    let dir = std::env::temp_dir().join(format!("chz-ticket135-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("dd135.chz");
    let src = [
        "ch := Channel[int](0)",
        "r := recover: ch.recv()",
        "print(\"recovered\")",
        "",
    ];
    std::fs::write(&path, src.join("\n")).expect("write deadlock fixture");

    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .output()
        .expect("run chezzi");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("recovered"),
        "a deadlock verdict must abort the program, but `recover:` caught it (stdout: {stdout:?})"
    );
    assert!(
        !out.status.success() && stderr.contains("deadlock"),
        "expected a fatal `deadlock` at nonzero rc, got {} (stderr: {stderr})",
        out.status
    );
}
