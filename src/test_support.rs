//! Shared helpers for the unit tests of every crate that includes this file. The one owner of the
//! fixture temp directory (a single process-wide counter names every directory, so two live fixtures
//! never share a path) and of the C callback fixture library.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A fresh temp directory, removed with everything in it on drop.
pub(crate) struct TmpDir(pub(crate) PathBuf);

impl TmpDir {
    pub(crate) fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_unit_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }

    /// The path of `rel` inside the directory. Creates nothing.
    pub(crate) fn join(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    /// Write `contents` to `rel` inside the directory, creating parent directories.
    pub(crate) fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).unwrap();
        p
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) static CB_SO_COUNTER: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Compile a tiny C fixture to a `.so` in a unique temp dir and return its path. The fixture
/// exports `int apply(int x, int (*f)(int)) { return f(x) + 1; }`,
/// `double applyd(double x, double (*f)(double)) { return f(x); }`,
/// `int apply2(int (*f)(int), int x) { return f(x) + 1; }` (callback-FIRST, for the not-last
/// arg path), and `void each(int n, void (*f)(long)) { ... }` (a void-returning callback, the
/// commonest C callback shape) — enough to round-trip an int and a float callback synchronously.
/// Built with the system `cc` (the same toolchain the FFI
/// tests already require: a unix LP64 host with libc/libm); a `cc` failure `panic!`s the test
/// loudly rather than silently skipping (matching how `dlopen` failures are asserted, not skipped).
pub(crate) fn build_callback_so() -> std::path::PathBuf {
    use std::io::Write;
    // A unique dir per call keeps parallel test threads from racing on the same file name.
    let dir = std::env::temp_dir().join(format!(
        "chezzi_cb_ffi_{}_{}",
        std::process::id(),
        CB_SO_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let csrc = dir.join("apply.c");
    let mut f = std::fs::File::create(&csrc).expect("create apply.c");
    f.write_all(
        br#"
#include <stdint.h>
int apply(int x, int (*f)(int)) { return f(x) + 1; }
double applyd(double x, double (*f)(double)) { return f(x); }
int apply2(int (*f)(int), int x) { return f(x) + 1; }
/* A small record returned BY POINTER, for the ffi deref tests. The fields are laid out at known C
   offsets: a at 0 (int32), b at 8 (int64, after padding), c at 16 (double). mkrec returns a pointer
   to a static instance the Chezzi side then reads field-by-field via the load builtins. */
struct R { int32_t a; int64_t b; double c; };
void* mkrec(void) { static struct R r = { -3, 70000, 2.5 }; return &r; }
void each(int n, void (*f)(long)) { for (long i = 0; i < n; i++) f(i); }
"#,
    )
    .expect("write apply.c");
    drop(f);
    let so = dir.join("libapply.so");
    let status = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-o"])
        .arg(&so)
        .arg(&csrc)
        .status()
        .expect("spawn cc (a working C toolchain is required for the FFI callback tests)");
    assert!(status.success(), "cc failed to build the callback fixture");
    so
}
