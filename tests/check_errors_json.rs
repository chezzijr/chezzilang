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
