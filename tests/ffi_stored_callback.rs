//! Real-PROCESS test for gaps.md **W6-8**: a STORED FFI callback (one C keeps past the extern call
//! that received it — `signal`, `atexit`, GLib, …) used to dangle into a SIGSEGV from checker-clean
//! code, because the libffi trampoline was `ffi_closure_free`d when the extern call returned while C
//! still held its code pointer.
//!
//! Stored/cross-thread callbacks stay DEFERRED — but the deferral is now a LOUD, defined abort
//! instead of undefined behavior: the trampoline is leaked and POISONED (its VM back-pointer
//! detached), so a later invocation from C writes a named message to stderr and `abort()`s.
//!
//! Must be a subprocess test: the program dies on SIGABRT, so it can never be an `examples/*.chz`
//! stdout golden, and FFI UB is memory-layout dependent (that is exactly how the `Box<Cif>` bug
//! slipped past the goldens — see `boxed_callback_cif_address_is_stable_across_moves`).
#![cfg(target_os = "linux")]

use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique temp directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_w68_{}_{}", std::process::id(), n));
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

/// The gaps.md W6-8 repro verbatim: `signal(10, handler)` STORES the trampoline, then `raise(10)`
/// makes C invoke it after the `signal` extern call has already returned.
const REPRO: &str = r#"import std.ffi

extern "libc.so.6":
    fn signal(sig: int, h: fn(int) -> int) -> ptr
    fn raise(sig: int) -> int

fn handler(sig: int) -> int:
    print("handler", sig)
    return 0

h := signal(10, handler)
print(raise(10))
"#;

#[test]
fn stored_callback_aborts_loudly_on_both_engines() {
    for serial in [false, true] {
        let t = TmpDir::new();
        let entry = t.write("main.chz", REPRO);
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
        cmd.arg("run");
        if serial {
            cmd.arg("--serial");
        }
        let out = cmd.arg(&entry).output().expect("spawn chezzi");
        let engine = if serial { "--serial" } else { "M:N" };
        let stderr = String::from_utf8_lossy(&out.stderr);

        // SIGABRT (6), never SIGSEGV (11) and never a silent success: the poison stub must abort,
        // not unwind (unwinding out of Rust into a C frame is itself UB).
        assert_eq!(
            out.status.signal(),
            Some(libc::SIGABRT),
            "[{engine}] a stored FFI callback must abort loudly, got status {:?} signal {:?}; \
             stderr: {stderr}",
            out.status.code(),
            out.status.signal(),
        );
        assert!(
            stderr.contains("stored/cross-thread callbacks are not supported"),
            "[{engine}] the abort must name the unsupported feature; stderr: {stderr}"
        );
    }
}
