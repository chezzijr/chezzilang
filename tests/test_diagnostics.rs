//! CLI-level pins for W8-37: `chezzi test` must render the same fault position, the same
//! type-error `line:col` and the same warning path that `chezzi run`/`chezzi check` render, instead
//! of its own message-only rendering. Drives the real `env!("CARGO_BIN_EXE_chezzi")` binary, because
//! the pre-fix drop happens in `test_runner.rs`'s CLI-facing print sites, not in an in-VM structure.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_diag_{}_{}", std::process::id(), n));
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

fn run_test(args: &[&str]) -> (String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("test");
    cmd.args(args);
    let out = cmd.output().expect("spawn chezzi");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn test_fault_carries_frames_and_col() {
    let t = TmpDir::new();
    let entry = t.write(
        "f37_test.chz",
        "fn boom(xs: List[int]):\n    return xs[9]\n\ntest fn t():\n    xs := [1]\n    boom(xs)\n",
    );
    let (stdout, _stderr) = run_test(&[entry.to_str().unwrap()]);
    assert!(
        stdout.contains("f37_test.chz:2:12) index 9 out of bounds (len 1)"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("  at boom (called at "), "stdout: {stdout}");
    assert!(stdout.contains("f37_test.chz:6:5)"), "stdout: {stdout}");
}

#[test]
fn test_type_error_carries_line_and_col() {
    let t = TmpDir::new();
    let entry = t.write(
        "h37_test.chz",
        "test fn t():\n    x: int = \"s\"\n    assert true\n",
    );
    let (stdout, _stderr) = run_test(&[entry.to_str().unwrap()]);
    assert!(stdout.contains("type error ("), "stdout: {stdout}");
    assert!(
        stdout.contains("h37_test.chz:2:14): cannot assign str to variable of type int"),
        "stdout: {stdout}"
    );
}

#[test]
fn test_warning_names_its_file() {
    let t = TmpDir::new();
    let entry = t.write(
        "w_test.chz",
        "fn g() -> Result[int, str]:\n    return Ok(1)\n\ntest fn t():\n    g()\n    assert true\n",
    );
    let (_stdout, stderr) = run_test(&[entry.to_str().unwrap()]);
    assert!(
        stderr.contains("w_test.chz:5:5): the Result returned by 'g' is discarded"),
        "stderr: {stderr}"
    );
    assert!(
        !stderr.contains("warning (line 5, col 5)"),
        "stderr: {stderr}"
    );
}

#[test]
fn imported_module_fault_names_the_library_file() {
    let t = TmpDir::new();
    t.write("lib.chz", "fn boom(xs: List[int]):\n    return xs[9]\n");
    let entry = t.write(
        "imp_test.chz",
        "import lib\n\ntest fn t():\n    lib.boom([1])\n",
    );
    let (stdout, _stderr) = run_test(&[entry.to_str().unwrap()]);
    assert!(stdout.contains("lib.chz:2:12)"), "stdout: {stdout}");
    assert!(!stdout.contains("imp_test.chz:2"), "stdout: {stdout}");
}

#[test]
fn json_document_is_unchanged_by_the_coordinate_fix() {
    let t = TmpDir::new();
    let entry = t.write(
        "f37_test.chz",
        "fn boom(xs: List[int]):\n    return xs[9]\n\ntest fn t():\n    xs := [1]\n    boom(xs)\n",
    );
    let (stdout, _stderr) = run_test(&["--errors=json", entry.to_str().unwrap()]);
    assert!(stdout.contains("\"line\":2"), "stdout: {stdout}");
    assert!(!stdout.contains("\"col\""), "stdout: {stdout}");
}
