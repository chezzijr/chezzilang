//! Real-PROCESS exit-status tests for `std.os.exit(code)`. Drives the actual
//! `env!("CARGO_BIN_EXE_chezzi")` binary via `std::process::Command`, because the bug this pins —
//! `os.exit(-1)` reporting SUCCESS (status 0) to the shell/CI — is invisible to an in-VM assertion
//! on `pending_exit`: the old clamp turned a negative code into `0`, and only the process status
//! reveals that a "failure" exit was seen by the shell as a success.
//!
//! Rule under test (POSIX `exit(3)` / bash / Python / Go): the process status is the LOW 8 BITS of
//! the code — `code & 0xff`. So `-1` → 255, `300` → 44, `-256` → 0.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique temp directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_exit_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::write(&p, contents).unwrap();
        p
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run `chezzi run [--serial] <file>` on a program that calls `os.exit(code)`; return the process
/// exit status the OS reports.
fn exit_status(code: &str, serial: bool) -> i32 {
    let t = TmpDir::new();
    let entry = t.write("main.chz", &format!("import std.os\nos.exit({code})\n"));
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run");
    if serial {
        cmd.arg("--serial");
    }
    let out = cmd.arg(&entry).output().expect("spawn chezzi");
    out.status.code().expect("exited with a status (no signal)")
}

#[test]
fn os_exit_status_is_the_low_8_bits_on_both_engines() {
    // (code, expected process status) — the POSIX mask, both ends.
    let cases = [
        ("0", 0),     // boundary: success stays success
        ("1", 1),     // boundary: the ordinary failure code
        ("-1", 255),  // THE BUG: used to clamp to 0 = silent SUCCESS in CI
        ("255", 255), // boundary: the top of the byte
        ("300", 44),  // >255 masks (300 & 0xff), exactly like POSIX `exit(300)`
        ("-256", 0),  // a negative multiple of 256 masks to 0 — the mask is total, not a clamp
        ("-2", 254),  // a second negative, to pin two's-complement masking
    ];
    for (code, want) in cases {
        for serial in [false, true] {
            let got = exit_status(code, serial);
            assert_eq!(
                got,
                want,
                "os.exit({code}) on {} engine: expected process status {want}, got {got}",
                if serial { "serial" } else { "M:N" }
            );
        }
    }
}

#[test]
fn a_program_that_never_exits_explicitly_succeeds() {
    // Boundary/no-regression: without `os.exit`, a clean program is still status 0.
    let t = TmpDir::new();
    let entry = t.write("main.chz", "print(\"hi\")\n");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run"])
        .arg(&entry)
        .output()
        .expect("spawn chezzi");
    assert_eq!(out.status.code(), Some(0), "a clean run exits 0");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
}
