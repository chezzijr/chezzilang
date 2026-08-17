//! CLI-level tests for the runtime stack trace naming its file (`docs/gaps.md` W8-14): the headline
//! `runtime error (…)` coordinate and every `at FUNC (called at …)` frame render `path:line:col`
//! instead of the bare `line N, col M` that used to leave a std-module fault looking like it lived in
//! the user's own file. Drives the real `env!("CARGO_BIN_EXE_chezzi")` binary (mirrors
//! `tests/check_errors_json.rs` / `tests/module_root.rs`), because the render only happens at the
//! `RunError`/`format_trace` boundary — the library's `run_program`/`run_capture` test helpers lex
//! standalone (`file == 0`) and never reach it.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique temp directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_rstp_{}_{}", std::process::id(), n));
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

/// Run `chezzi <args>` with cwd `dir` (so a path under it relativizes the same way `render_span`
/// does), returning `(stdout, stderr, exit_success)`.
fn run(dir: &std::path::Path, args: &[&str]) -> (String, String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn chezzi");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

/// Case 1 — single-file fault: the headline AND every frame name the entry file, not a bare
/// `line N, col M`.
#[test]
fn single_file_fault_names_entry_file_on_every_frame() {
    let t = TmpDir::new();
    t.write(
        "main.chz",
        "fn boom(x: int) -> int:\n    return [1][x]\n\nfn go():\n    boom(9)\n\ngo()\n",
    );
    let (_stdout, stderr, ok) = run(&t.0, &["run", "main.chz"]);
    assert!(!ok, "the program must fault");
    assert!(
        stderr.starts_with("runtime error (main.chz:2:"),
        "headline must name the entry file, got:\n{stderr}"
    );
    assert!(
        stderr.contains("at boom (called at main.chz:5:"),
        "innermost frame must name the entry file, got:\n{stderr}"
    );
    assert!(
        stderr.contains("at go (called at main.chz:7:"),
        "outer frame must name the entry file, got:\n{stderr}"
    );
}

/// Case 2 — a three-module program (entry imports `a` and `b`; `a` imports `c`) where the fault, the
/// inner frame and the outer frame are in THREE DIFFERENT files. `file` ids are assigned DFS
/// pre-order (entry=1, a=2, c=3, b=4) while `Program::modules` is deps-first post-order
/// (c, a, b, entry) — a wrong id→path mapping (e.g. indexing by `file - 1`) would misattribute a
/// frame to the wrong file here even though the single-file case above stays accidentally correct.
#[test]
fn three_module_fault_names_each_frame_distinct_file() {
    let t = TmpDir::new();
    t.write(
        "main.chz",
        "import a\nimport b\n\nfn go():\n    print(a.afunc())\n\ngo()\n",
    );
    t.write(
        "a.chz",
        "import c\n\nfn afunc() -> int:\n    return c.cfault(9)\n",
    );
    t.write("b.chz", "fn bnoop() -> int:\n    return 1\n");
    t.write("c.chz", "fn cfault(x: int) -> int:\n    return [1][x]\n");
    let (_stdout, stderr, ok) = run(&t.0, &["run", "main.chz"]);
    assert!(!ok, "the program must fault");
    // The fault itself is in c.chz.
    assert!(
        stderr.starts_with("runtime error (c.chz:2:"),
        "headline must name c.chz (where the fault is), got:\n{stderr}"
    );
    // The innermost frame (`cfault`'s call site) is inside a.chz.
    assert!(
        stderr.contains("at cfault (called at a.chz:4:"),
        "inner frame must name a.chz, got:\n{stderr}"
    );
    // The outer frame (`afunc`'s call site) is inside main.chz — a THIRD distinct file.
    assert!(
        stderr.contains("at afunc (called at main.chz:5:"),
        "outer frame must name main.chz, got:\n{stderr}"
    );
    // b.chz is never named — the mismatch-catching id assignment must not scramble in a module that
    // is imported but never on the fault's call chain.
    assert!(
        !stderr.contains("b.chz"),
        "b.chz must not appear in an unrelated fault's trace, got:\n{stderr}"
    );
}

/// Case 3 — a fault raised from `std.flag` (`std/flag.chz`, an unregistered `get_str` flag) names
/// `flag.chz`, never a bare `line 132` that would read as the user's own file.
#[test]
fn std_module_fault_names_the_std_file_not_a_bare_line() {
    let t = TmpDir::new();
    t.write(
        "main.chz",
        "import std.flag\n\nfs := flag.new()\nfs.get_str(\"zz\")\n",
    );
    let (_stdout, stderr, ok) = run(&t.0, &["run", "main.chz"]);
    assert!(!ok, "the program must fault");
    assert!(
        stderr.contains("flag.chz:"),
        "headline must name flag.chz, got:\n{stderr}"
    );
    assert!(
        !stderr.contains("(line 132,"),
        "must not regress to a bare, unattributed line number, got:\n{stderr}"
    );
}
