//! CLI-level diagnostic-shape tests for `chezzi check` resolve errors (the two diagnostic-quality
//! bugs). Drives the real `env!("CARGO_BIN_EXE_chezzi")` binary via `std::process::Command`, so it
//! verifies the end-to-end CLI contract (`--errors=json` JSON shape + plain-text rendering), not a
//! unit helper. These are diagnostic-only: the accept/reject decision is unchanged.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique temp directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_cej_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
    fn write(&self, rel: &str, contents: &str) -> PathBuf {
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

/// Build the repro project: main.chz imports `deep`, and deep.chz has a bad `import ghost from
/// doesnotexist` on line 4. Returns the temp dir (keep alive) and the entry path.
fn missing_module_project() -> (TmpDir, PathBuf) {
    let t = TmpDir::new();
    let main = t.write("main.chz", "import deep\nfn main(): print(1)\n");
    t.write(
        "deep.chz",
        "# pad\n# pad\n# pad\nimport ghost from doesnotexist\nfn f(): print(1)\n",
    );
    (t, main)
}

#[test]
fn resolve_error_json_is_clean_and_attributed() {
    let (_t, main) = missing_module_project();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["check", main.to_str().unwrap(), "--errors=json"])
        .output()
        .expect("run chezzi check --errors=json");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stdout = stdout.trim();

    // (a) Exactly one JSON object in the array. Cheap structural check without a JSON dep.
    assert!(
        stdout.starts_with('[') && stdout.ends_with(']'),
        "expected a JSON array, got: {stdout}"
    );
    assert_eq!(
        stdout.matches("{").count(),
        1,
        "expected exactly one error object, got: {stdout}"
    );

    // (b) The message names the importing module + the missing module.
    assert!(
        stdout.contains("in module 'deep': cannot find module 'doesnotexist'"),
        "message must name importer + missing module, got: {stdout}"
    );
    // (c) No doubled Display prefix embedded inside the JSON message.
    assert!(
        !stdout.contains("resolve error ("),
        "JSON message must not embed the `resolve error (...)` Display prefix, got: {stdout}"
    );
    // (d) Carries the line of the bad import (line 4 in deep.chz).
    assert!(
        stdout.contains("\"line\":4"),
        "must carry line 4, got: {stdout}"
    );
}

#[test]
fn resolve_error_plaintext_unchanged_and_attributed() {
    let (_t, main) = missing_module_project();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["check", main.to_str().unwrap()])
        .output()
        .expect("run chezzi check");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stderr = stderr.trim_end();

    // Plain-text keeps the `resolve error (line N, col M):` Display prefix (byte-identical rendering),
    // now followed by the module attribution.
    assert!(
        stderr.starts_with("resolve error (line 4, col 1):"),
        "plain text must keep the Display prefix, got: {stderr}"
    );
    assert!(
        stderr.contains("in module 'deep': cannot find module 'doesnotexist'"),
        "plain text must name importer + missing module, got: {stderr}"
    );
}

/// Crash-safety regression: a valid but very long left-associative binary chain or postfix chain
/// used to build an AST deep enough to overflow the recursive front-end walkers → host stack
/// overflow (SIGABRT, exit code None). The `MAX_CHAIN_DEPTH` parser cap + the dedicated front-end
/// stack turn that into either a clean parse diagnostic (over the cap) or a normal run (under it) —
/// NEVER a signal kill. Drives the real binary end-to-end (a parser unit test cannot observe the
/// process abort). See docs/bug-discovery.md (post-parse walker depth axis).
#[test]
fn deep_chains_never_signal_crash_the_host() {
    let t = TmpDir::new();

    // (a) A 6000-term `1+1+…` chain (the original repro): `check` must exit with a code (a clean
    // diagnostic), never be killed by a signal (SIGABRT → code() == None).
    let big_add = format!("x := 1{}\n", "+1".repeat(6000));
    let f = t.write("big_add.chz", &big_add);
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("run chezzi check");
    assert!(
        out.status.code().is_some(),
        "deep + chain must not signal-crash the host (got signal kill, no exit code)"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("too deeply"),
        "over-cap chain should be a clean 'too deeply' diagnostic"
    );

    // (b) Same for a deep postfix field chain and via `run` (compiler + VM path).
    let big_field = format!("x := a{}\n", ".f".repeat(6000));
    let f2 = t.write("big_field.chz", &big_field);
    let out2 = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", f2.to_str().unwrap()])
        .output()
        .expect("run chezzi run");
    assert!(
        out2.status.code().is_some(),
        "deep postfix chain via `run` must not signal-crash the host"
    );

    // (c) A chain UNDER the cap runs and prints the right value (no over-rejection).
    let ok = format!("print(1{})\n", "+1".repeat(400));
    let f3 = t.write("ok_add.chz", &ok);
    let out3 = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", f3.to_str().unwrap()])
        .output()
        .expect("run chezzi run");
    assert!(out3.status.success(), "under-cap chain must run cleanly");
    assert_eq!(String::from_utf8_lossy(&out3.stdout).trim(), "401");
}
