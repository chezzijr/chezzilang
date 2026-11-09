//! TICKET-298 (wave 24 F1) — a family cut re-labelled as a cancel must release a Shared guard
//! `CUT`, so no waiter reads the half-done value, and the run ends in a fatal deadlock.

use std::process::Command;

const SRC: &str = r#"import Shared from std.concurrency
import sleep_ms from std.time
s := Shared([0])
never := Channel[int](0)
fn stuck(xs: List[int]) -> List[int]:
    xs.push(1)
    _ := never.recv()
    xs.push(2)
    return xs
parallel:
    spawn:
        r := recover:
            parallel:
                spawn:
                    defer s.update(stuck)
                    sleep_ms(1000)
                spawn:
                    sleep_ms(20)
                    panic("boom")
        print("inner", r)
    spawn:
        sleep_ms(100)
        print("C sees", s.get())
print("end")
"#;

#[test]
fn family_cut_with_stuck_update_in_defer_does_not_release_the_guard_normally() {
    let dir = std::env::temp_dir().join(format!("chz-ticket298-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create fixture dir");
    let path = dir.join("cut_shared.chz");
    std::fs::write(&path, SRC).expect("write fixture");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .env("CHEZZI_THREADS", "1")
        .output()
        .expect("run chezzi");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("C sees [0, 1]"),
        "a waiter read the half-done value (stdout: {stdout:?})"
    );
    assert!(
        !out.status.success() && stderr.contains("deadlock:"),
        "expected a fatal `deadlock:` at nonzero rc, got {} (stderr: {stderr})",
        out.status
    );
}
