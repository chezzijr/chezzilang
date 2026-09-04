//! CLI-level tests for stripping a single leading UTF-8 BOM (`U+FEFF`) across `run`, `check`,
//! `test`, `tokens`, `ast` and imported modules (TICKET-058). Drives the real
//! `env!("CARGO_BIN_EXE_chezzi")` binary via `std::process::Command`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique temp directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_bom_{}_{}", std::process::id(), n));
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

#[test]
fn run_strips_a_leading_bom() {
    let t = TmpDir::new();
    let bom = t.write("bom.chz", "\u{feff}print(1)\n");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", bom.to_str().unwrap()])
        .output()
        .expect("run chezzi run");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1");
}

#[test]
fn tokens_strips_a_leading_bom() {
    let t = TmpDir::new();
    let bom = t.write("bom.chz", "\u{feff}print(1)\n");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["tokens", bom.to_str().unwrap()])
        .output()
        .expect("run chezzi tokens");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let first_line = stdout.lines().next().unwrap_or("");
    assert!(
        first_line.contains("Ident(\"print\")"),
        "got {first_line:?}"
    );
}

#[test]
fn ast_strips_a_leading_bom() {
    let t = TmpDir::new();
    let bom = t.write("bom.chz", "\u{feff}print(1)\n");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["ast", bom.to_str().unwrap()])
        .output()
        .expect("run chezzi ast");
    assert!(out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("lex error"));
}

#[test]
fn check_json_columns_are_counted_from_the_first_visible_char() {
    let t = TmpDir::new();
    let bom = t.write("bom_name.chz", "\u{feff}print(nope)\n");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["check", bom.to_str().unwrap(), "--errors=json"])
        .output()
        .expect("run chezzi check --errors=json");
    assert!(!out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"line\":1,\"col\":7,\"end_line\":1,\"end_col\":11"),
        "got {stdout:?}"
    );
    assert!(
        stdout.contains("\"message\":\"unknown name 'nope'\""),
        "got {stdout:?}"
    );
}

#[test]
fn check_text_caret_is_aligned_on_a_bom_prefixed_file() {
    let t = TmpDir::new();
    let bom = t.write("bom_name.chz", "\u{feff}print(nope)\n");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["check", bom.to_str().unwrap()])
        .output()
        .expect("run chezzi check");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("1 | print(nope)"), "got {stderr:?}");
    assert!(stderr.contains("  |       ^^^^"), "got {stderr:?}");
}

#[test]
fn an_imported_module_with_a_bom_resolves() {
    let t = TmpDir::new();
    let main = t.write("main.chz", "import helper\nprint(helper.answer())\n");
    t.write("helper.chz", "\u{feff}fn answer() -> int:\n    return 7\n");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", main.to_str().unwrap()])
        .output()
        .expect("run chezzi run");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "7");
}

#[test]
fn chezzi_test_runs_a_bom_prefixed_test_file() {
    let t = TmpDir::new();
    t.write(
        "x_test.chz",
        "\u{feff}test fn passes():\n    assert 1 == 1\n",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["test", t.0.to_str().unwrap()])
        .output()
        .expect("run chezzi test");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("1 passed, 0 failed"),
        "got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn a_mid_file_bom_is_still_an_error() {
    let t = TmpDir::new();
    let mid = t.write("mid.chz", "\u{feff}print(\u{feff}1)\n");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", mid.to_str().unwrap()])
        .output()
        .expect("run chezzi run");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("mid.chz:1:7): lex error: unexpected character"),
        "got {stderr:?}"
    );
}

#[test]
fn a_bom_only_file_is_an_empty_program_at_rc_0() {
    let t = TmpDir::new();
    let bomonly = t.write("bomonly.chz", "\u{feff}");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", bomonly.to_str().unwrap()])
        .output()
        .expect("run chezzi run");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).is_empty());
}
