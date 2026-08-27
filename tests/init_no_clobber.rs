//! CLI-level and doc-contract tests for `chezzi init`'s no-clobber guarantee (W8-24): an existing
//! `src/main.chz` or `src/main_test.chz` is kept, not overwritten, and the docs must say so.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_initcli_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn init_cli_keeps_an_existing_main_chz() {
    let d = TmpDir::new();
    std::fs::create_dir_all(d.0.join("src")).unwrap();
    std::fs::write(
        d.0.join("src/main.chz"),
        "fn main(): print(\"MY REAL PROGRAM\")",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("init")
        .arg(&d.0)
        .output()
        .expect("failed to run chezzi init");
    assert!(
        output.status.success(),
        "chezzi init should exit 0; stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let contents = std::fs::read_to_string(d.0.join("src/main.chz")).unwrap();
    assert!(
        contents.contains("MY REAL PROGRAM"),
        "chezzi init overwrote an existing src/main.chz; contents:\n{contents}"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("src/main.chz         kept"),
        "stdout should report src/main.chz as kept; got:\n{stdout}"
    );

    assert!(d.0.join("chezzi.toml").is_file(), "chezzi.toml missing");
    assert!(
        d.0.join("src/main_test.chz").is_file(),
        "src/main_test.chz missing"
    );
}

#[test]
fn docs_state_that_init_never_overwrites() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

    let syntax = std::fs::read_to_string(root.join("docs/syntax.md")).unwrap();
    assert!(
        !syntax.contains("refuses to overwrite an existing"),
        "docs/syntax.md still claims init merely refuses to overwrite chezzi.toml"
    );
    assert!(
        syntax.contains("It never overwrites a file that is already there."),
        "docs/syntax.md should state that init never overwrites a file that is already there"
    );

    let gaps = std::fs::read_to_string(root.join("docs/gaps.md")).unwrap();
    assert!(
        gaps.contains("| ~~**W8-24**~~ |"),
        "docs/gaps.md should mark W8-24 as closed"
    );
}
