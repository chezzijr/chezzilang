//! CLI-level module-root resolution tests. Drives the real `env!("CARGO_BIN_EXE_chezzi")` binary via
//! `std::process::Command` with a real multi-file project on disk, because the "root disagreement"
//! bug (bare `chezzi run` derived the module-graph root TWICE — once from the cwd manifest to locate
//! the entry, once by walking up from the entry FILE to resolve imports — and the two could disagree,
//! silently loading the wrong same-named module) is invisible to the library `build_graph` helpers,
//! which take the entry file directly and always walk up from it.
//!
//! Invariant under test: a single `chezzi` run computes its module-graph root exactly ONCE and uses
//! that SAME root for both (a) locating the entrypoint file and (b) resolving every `import`.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique temp directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_mroot_{}_{}", std::process::id(), n));
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

/// Build the `proj_silent` tree: an OUTER project whose manifest entrypoint lives beneath a NESTED
/// `chezzi.toml`, with a same-named `shared` module at BOTH roots. The nested marker is the trap.
/// `import shared` is root-relative, so it resolves against whichever root the run picks; the entry
/// self-calls `main()` at the top level so both the bare (manifest) run and an explicit file run
/// produce observable output.
fn proj_silent() -> TmpDir {
    let t = TmpDir::new();
    t.write(
        "chezzi.toml",
        "[project]\nentrypoint = \"services.api.main\"\n",
    );
    t.write("shared.chz", "fn util():\n    print(\"OUTER shared\")\n");
    // A nested marker — the sub-package boundary the buggy resolver stopped at for imports.
    t.write("services/chezzi.toml", "[project]\n");
    t.write(
        "services/shared.chz",
        "fn util():\n    print(\"INNER shared\")\n",
    );
    t.write(
        "services/api/main.chz",
        "import shared\n\nfn main():\n    shared.util()\n\nmain()\n",
    );
    t
}

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

// 1. Bare `chezzi run` from the OUTER root must pin the run's root to the manifest that declared the
//    entrypoint (the outer manifest), NOT the nested marker the entry file happens to sit under, so
//    `import shared` resolves to the OUTER shared.chz — never silently the inner one.
#[test]
fn bare_run_uses_manifest_root_not_nested() {
    let t = proj_silent();
    let (stdout, stderr, ok) = run(&t.0, &["run"]);
    assert!(ok, "bare run should succeed; stderr:\n{stderr}");
    assert!(
        stdout.contains("OUTER shared"),
        "bare run must resolve imports against the manifest root; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("INNER shared"),
        "bare run must NOT silently load the nested same-named module; stdout:\n{stdout}"
    );
}

// 1b. Same, on the serial (cooperative single-thread) engine — proves serial-VM == M:N-VM at the CLI
//     level (the library parity helpers cannot reproduce this bug).
#[test]
fn bare_run_serial_uses_manifest_root() {
    let t = proj_silent();
    let (stdout, stderr, ok) = run(&t.0, &["run", "--serial"]);
    assert!(ok, "bare --serial run should succeed; stderr:\n{stderr}");
    assert!(
        stdout.contains("OUTER shared"),
        "serial bare run must resolve imports against the manifest root; stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("INNER shared"),
        "serial bare run must NOT silently load the nested module; stdout:\n{stdout}"
    );
}

// 2. Explicit `chezzi run FILE` for a file beneath the nested marker uses the NEAREST marker
//    (services/chezzi.toml) as its root — the conventional Go/Cargo/npm sub-package behavior — so it
//    resolves `shared` to services/shared.chz. This is unchanged and correct; it confirms file-run
//    and manifest-run each stay internally self-consistent.
#[test]
fn file_run_uses_nearest_marker() {
    let t = proj_silent();
    let (stdout, stderr, ok) = run(&t.0, &["run", "services/api/main.chz"]);
    assert!(ok, "explicit file run should succeed; stderr:\n{stderr}");
    assert!(
        stdout.contains("INNER shared"),
        "explicit file run uses the nearest marker (sub-package root); stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("OUTER shared"),
        "explicit file run must NOT reach past the nearest marker; stdout:\n{stdout}"
    );
}

/// A normal single-root project (NO nested marker): entrypoint `src.main`, `src/main.chz` imports a
/// root-level `greet` module and self-calls `main()`. Includes a marker-free nested subdir for the
/// cwd-invariance check.
fn single_root() -> TmpDir {
    let t = TmpDir::new();
    t.write("chezzi.toml", "[project]\nentrypoint = \"src.main\"\n");
    t.write(
        "src/main.chz",
        "import greet\n\nfn main():\n    greet.hello()\n\nmain()\n",
    );
    // Root-relative import: `import greet` resolves to <root>/greet.chz.
    t.write("greet.chz", "fn hello():\n    print(\"greetings\")\n");
    // A marker-free subdir: walking up from here must find the SAME (only) manifest.
    std::fs::create_dir_all(t.0.join("sub/deeper")).unwrap();
    t
}

// 3 + 6. A single-root project behaves identically for bare-run (from root and from a marker-free
//         nested subdir — cwd-invariance) and explicit file-run; all three print the same line.
#[test]
fn single_root_bare_and_file_agree() {
    let t = single_root();

    let (a, ea, oka) = run(&t.0, &["run"]);
    assert!(oka, "bare run from root; stderr:\n{ea}");
    assert!(a.contains("greetings"), "bare run stdout:\n{a}");

    let (b, eb, okb) = run(&t.0.join("sub/deeper"), &["run"]);
    assert!(okb, "bare run from nested subdir; stderr:\n{eb}");
    assert!(
        b.contains("greetings"),
        "cwd-invariance: bare run from a nested subdir must find the same manifest; stdout:\n{b}"
    );

    let (c, ec, okc) = run(&t.0, &["run", "src/main.chz"]);
    assert!(okc, "explicit file run; stderr:\n{ec}");
    assert!(c.contains("greetings"), "explicit file run stdout:\n{c}");

    assert_eq!(a, b, "bare run must be cwd-invariant");
    assert_eq!(
        a, c,
        "bare run and explicit file run must agree on a single-root project"
    );
}

// M24 — a manifest `module:function` entrypoint is invoked BY NAME with no arguments, so it cannot
// take a hidden static-protocol type witness: there is no call site to pin `T`. Before this it
// type-checked green and then died at startup with the HIDDEN arity leaked into the message
// ("function 'main' expects 1 argument(s), got 0"), naming a parameter the source does not declare.
// Driven through the real binary because the entrypoint's name is CLI state — the library check
// helpers never see it.
#[test]
fn witness_taking_manifest_entrypoint_is_refused_with_its_reason() {
    let t = TmpDir::new();
    t.write("chezzi.toml", "[project]\nentrypoint = \"src.main:main\"\n");
    t.write(
        "src/main.chz",
        "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(5)\nfn main[T: Default]():\n    print(T.default())\n",
    );

    let (stdout, stderr, ok) = run(&t.0, &["run"]);
    assert!(!ok, "must be refused; stdout:\n{stdout}");
    assert!(
        stderr.contains("the manifest entrypoint 'main' is invoked with no arguments"),
        "the error must name the real cause, not the hidden arity; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("expects 1 argument(s), got 0"),
        "the hidden witness arity must never leak into a user-facing message; stderr:\n{stderr}"
    );

    // The same shape WITHOUT a static-carrying bound still runs: only the witness is refused.
    t.write("src/main.chz", "fn main[T]():\n    print(\"ran\")\n");
    let (stdout, stderr, ok) = run(&t.0, &["run"]);
    assert!(
        ok,
        "a plain generic entrypoint still runs; stderr:\n{stderr}"
    );
    assert!(stdout.contains("ran"), "stdout:\n{stdout}");

    // …and an explicit FILE run of the same module is untouched (it never invokes `main`).
    let (stdout, stderr, ok) = run(&t.0, &["run", "src/main.chz"]);
    assert!(ok, "explicit file run; stderr:\n{stderr}");
    assert!(
        !stdout.contains("ran"),
        "a file run executes the top level only; stdout:\n{stdout}"
    );
}

// M24 — the SAME entrypoint gate must be reachable from every consumer that statically checks the
// file, not just from bare `chezzi run`. The entrypoint is a property of the PROJECT (the manifest
// declares `main`'s required shape, exactly as Go requires `func main()` to be nullary), so
// `chezzi check` — and through the same derivation the editor/LSP — must report it too. It used to
// be CLI state threaded only through the bare-run path, so `chezzi check src/main.chz` said "ok: no
// type errors" about a project that cannot start.
#[test]
fn a_witness_taking_manifest_entrypoint_is_refused_by_check_too() {
    let t = TmpDir::new();
    t.write("chezzi.toml", "[project]\nentrypoint = \"src.main:main\"\n");
    t.write(
        "src/main.chz",
        "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(5)\nfn main[T: Default]():\n    print(T.default())\n",
    );

    let (stdout, stderr, ok) = run(&t.0, &["check", "src/main.chz"]);
    assert!(!ok, "check must refuse it too; stdout:\n{stdout}");
    let out = format!("{stdout}{stderr}");
    assert!(
        out.contains("the manifest entrypoint 'main' is invoked with no arguments"),
        "check must give the same reason as run; output:\n{out}"
    );

    // A file that is NOT the manifest entrypoint keeps every generic position it had.
    t.write(
        "src/other.chz",
        "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(5)\nfn main[T: Default]():\n    print(T.default())\n",
    );
    let (stdout, stderr, ok) = run(&t.0, &["check", "src/other.chz"]);
    assert!(
        ok,
        "only the declared entrypoint module is gated; stdout:\n{stdout}\nstderr:\n{stderr}"
    );
}
