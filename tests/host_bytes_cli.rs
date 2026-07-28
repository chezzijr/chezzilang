//! Real-PROCESS tests that HOSTILE BYTES at the host boundary never crash the CLI (W7-6 / W7-7).
//!
//! `std::env::args()` and `std::env::vars()` PANIC on a non-UTF-8 item, so a non-UTF-8 program
//! argument, script path, or environment variable aborted `chezzi` with rc=101 and a
//! `panicked at library/std/src/env.rs` before the program ever started. It is a HOST panic, so
//! `recover:` cannot see it and no in-VM assertion can pin it — hence a spawned-process test.
//! `args_os()` / `vars_os()` are the non-panicking forms; the CLI now decodes them lossily.
//!
//! Unix-only: constructing a non-UTF-8 `OsString` needs `OsStringExt::from_vec`.
#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique temp directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("chezzi_hostbytes_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
    fn hello(&self) -> PathBuf {
        let p = self.0.join("hello.chz");
        std::fs::write(&p, "print(\"hi\")\n").unwrap();
        p
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The three raw bytes `A \xff B` — valid as an OS string, never valid UTF-8.
fn bad_bytes() -> OsString {
    OsString::from_vec(b"A\xffB".to_vec())
}

/// Assert the run did not die inside the Rust host (the only bar these tests set).
fn assert_no_host_panic(out: &Output, what: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked at"),
        "{what}: the CLI host-panicked: {stderr}"
    );
    assert_ne!(
        out.status.code(),
        Some(101),
        "{what}: rc=101 (a Rust panic abort); stderr: {stderr}"
    );
}

#[test]
fn non_utf8_program_arg_does_not_host_panic() {
    let t = TmpDir::new();
    let entry = t.hello();
    for serial in [true, false] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
        cmd.arg("run");
        if serial {
            cmd.arg("--serial");
        }
        let out = cmd
            .arg(&entry)
            .arg(bad_bytes())
            .output()
            .expect("spawn chezzi");
        assert_no_host_panic(&out, "non-UTF-8 program arg");
        assert!(out.status.success(), "the program itself must still run");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
    }
}

#[test]
fn non_utf8_script_path_does_not_host_panic() {
    // A script whose PATH is not valid UTF-8 cannot be RUN (the path plumbing is `String`), but it
    // must fail CLEANLY — a refusal, never a host panic.
    let t = TmpDir::new();
    let mut raw = t.0.clone().into_os_string().into_vec();
    raw.extend_from_slice(b"/sc\xffipt.chz");
    let bad_path = OsString::from_vec(raw);
    std::fs::write(&bad_path, "print(\"hi\")\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&bad_path)
        .output()
        .expect("spawn chezzi");
    assert_no_host_panic(&out, "non-UTF-8 script path");
    assert_eq!(out.status.code(), Some(1), "must fail cleanly, not succeed");
}

/// The lossy decode must never SELECT A DIFFERENT FILE (W7-6, adversarial-review fix).
///
/// `to_string_lossy` is not injective, so the raw path `sc\xffipt.chz` and a real file literally
/// named `sc\u{FFFD}ipt.chz` decode to the same string. Opening the alias would run the *other*
/// program and exit 0 — strictly worse than the rc=101 host panic `args_os()` replaced. Pre-fix this
/// test printed "WRONG FILE RAN" with rc=0.
#[test]
fn non_utf8_script_path_never_runs_the_utf8_decoy() {
    let t = TmpDir::new();

    // The file actually asked for: raw byte 0xFF in its name.
    let mut raw = t.0.clone().into_os_string().into_vec();
    raw.extend_from_slice(b"/sc\xffipt.chz");
    let intended = OsString::from_vec(raw);
    std::fs::write(&intended, "print(\"INTENDED\")\n").unwrap();

    // The decoy: a *valid UTF-8* file whose name is what the lossy decode produces.
    let decoy = t.0.join("sc\u{FFFD}ipt.chz");
    std::fs::write(&decoy, "print(\"WRONG FILE RAN\")\n").unwrap();
    assert_ne!(
        std::fs::read(&intended).unwrap(),
        std::fs::read(&decoy).unwrap(),
        "the two files must be distinct for this test to mean anything"
    );

    for serial in [true, false] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
        cmd.arg("run");
        if serial {
            cmd.arg("--serial");
        }
        let out = cmd.arg(&intended).output().expect("spawn chezzi");
        assert_no_host_panic(&out, "non-UTF-8 script path with a U+FFFD decoy");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.contains("WRONG FILE RAN"),
            "the CLI ran the U+FFFD-named DECOY instead of the path it was given: {stdout}"
        );
        assert_eq!(
            out.status.code(),
            Some(1),
            "a lossy path must be refused, not silently resolved: {stdout}"
        );
    }
}

#[test]
fn non_utf8_env_var_does_not_host_panic() {
    // A trivial program that never touches `std.os`: the environment is snapshotted at startup.
    let t = TmpDir::new();
    let entry = t.hello();
    for serial in [true, false] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
        cmd.arg("run");
        if serial {
            cmd.arg("--serial");
        }
        let out = cmd
            .arg(&entry)
            .env("BAD", bad_bytes())
            .output()
            .expect("spawn chezzi");
        assert_no_host_panic(&out, "non-UTF-8 env var");
        assert!(out.status.success(), "the program itself must still run");
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
    }
}
