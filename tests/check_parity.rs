//! Real-PROCESS tests for `chezzi run --check-parity <file>` — the CLI flag that runs the SAME
//! program on BOTH engines (cooperative serial oracle + M:N OS-thread) and reports whether their
//! captured stdout/stderr/terminal-result are byte-identical. This exposes the test-only serial==M:N
//! parity oracle (`assert_file_parity`) as a one-command user check. Driven through the actual
//! `env!("CARGO_BIN_EXE_chezzi")` binary — the seam is the bin's arg loop + the buffered
//! `run_file_with_entry` path, not any VM internal, so it belongs at the process level.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique temp directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_parity_{}_{}", std::process::id(), n));
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

/// `--check-parity` combined with `--serial` is a contradiction (it runs BOTH engines) → clear error,
/// non-zero exit.
#[test]
fn check_parity_conflicts_with_serial() {
    let t = TmpDir::new();
    let entry = t.write("main.chz", "print(\"hi\")\n");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", "--check-parity", "--serial"])
        .arg(&entry)
        .output()
        .expect("spawn chezzi");
    assert_ne!(out.status.code(), Some(0), "conflict must exit non-zero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--check-parity") && stderr.contains("mutually exclusive"),
        "expected a clear conflict error, got: {stderr}"
    );
}

/// The deterministic-by-construction concurrency stress example runs identically on both engines →
/// exit 0, `parity OK` on stderr, its stdout printed once.
#[test]
fn check_parity_ok_on_concurrent_jobs() {
    let example = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/concurrent_jobs.chz");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", "--check-parity"])
        .arg(&example)
        .output()
        .expect("spawn chezzi");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "parity-held run exits 0; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("parity OK"),
        "expected `parity OK`, stderr:\n{stderr}"
    );
    assert!(
        stdout.contains("ALL PASS (49 checks)"),
        "captured stdout printed once, stdout:\n{stdout}"
    );
}

/// A stdin-reading program diverges under `--check-parity`: the two sequential legs share the real
/// process stdin fd, so leg 1 (serial) drains the piped line and leg 2 (M:N) sees EOF. A reliable,
/// environment-independent stdout divergence — and a live demo of the documented stdin limitation.
#[test]
fn check_parity_reports_divergence() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "import std.io\nmatch io.read_line():\n    Some(s): print(s)\n    None: print(\"EOF\")\n",
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", "--check-parity"])
        .arg(&entry)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"hi\n")
        .expect("write stdin");
    let out = child.wait_with_output().expect("wait chezzi");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "divergence must exit non-zero");
    assert!(
        stderr.contains("parity DIVERGENCE"),
        "expected a divergence report, stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("hi") && stderr.contains("EOF"),
        "report shows both sides, stderr:\n{stderr}"
    );
}

// ===== W6-9 — the oracle compares RAW BYTES, not a lossily-decoded capture =====
//
// The sink is `Vec<u8>` so `write_bytes` is byte-exact (W6-9), which means a program can now emit
// non-UTF-8 — and `String::from_utf8_lossy` is NOT injective: `ff` and `fe` both become one U+FFFD.
// Comparing DECODED captures would report `parity OK` for a run whose engines put different bytes on
// fd 1, i.e. the feature would have degraded its own detector. `--check-parity` promises
// "byte-identical stdout", so it (and the in-tree `assert_file_parity` it mirrors) diffs bytes.

/// Run `chezzi run --check-parity <file>` to completion.
fn check_parity(entry: &PathBuf) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", "--check-parity"])
        .arg(entry)
        .output()
        .expect("spawn chezzi")
}

#[test]
fn check_parity_reports_a_byte_only_divergence() {
    // The channel orders the two tasks, so each engine's byte order is deterministic: serial prints
    // live (`fe ff`), M:N flushes each task's slot in task order (`ff fe`). Both decode to "\u{FFFD}
    // \u{FFFD}" — only a byte-level diff can see it.
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "import std.io\n\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn:\n            _ := ch.recv()\n            _ := io.stdout().write_bytes(b\"\\xff\")\n        spawn:\n            _ := io.stdout().write_bytes(b\"\\xfe\")\n            ch.send(1)\nmain()\n",
    );
    let out = check_parity(&entry);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "divergence must exit non-zero");
    assert!(
        stderr.contains("parity DIVERGENCE"),
        "a divergence visible only in the RAW bytes must not decode away, stderr:\n{stderr}"
    );
}

#[test]
fn check_parity_echoes_the_captured_bytes_unchanged() {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "import std.io\n\n_ := io.stdout().write_bytes(b\"\\xff\\xfe\")\n",
    );
    let out = check_parity(&entry);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "parity holds here:\n{stderr}");
    assert!(stderr.contains("parity OK"), "stderr:\n{stderr}");
    // The tool must reproduce the output of the command it checks — `chezzi run` emits `ff fe`.
    assert_eq!(
        out.stdout,
        vec![0xff, 0xfe],
        "check-parity re-encoded the capture it echoes"
    );
}
