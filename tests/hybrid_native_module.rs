//! CLI-level tests for the HYBRID native+Chezzi std module: a single `std/*.chz` file mixing bodyless
//! `native fn` decls with BODIED Chezzi `fn`s (module-level `math.divmod`) and native-struct bodied
//! methods (`io.Reader.lines`). Driven through the real `env!("CARGO_BIN_EXE_chezzi")` binary because
//! the soundness half — an ill-typed BODY in a native module must be REJECTED — is exercised by
//! pointing `$CHEZZI_STD` at a deliberately-corrupted copy of the std tree, and `$CHEZZI_STD` is
//! process-global: it MUST live in a child process (an in-library test would leak the override to every
//! concurrently-running test, per the warning in `src/resolver/mod.rs`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_hybrid_{}_{}", std::process::id(), n));
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

/// Recursively copy `src` into `dst` (the std tree is shallow: files + one nested `concurrency/` dir).
fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_tree(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}

/// A fresh temp copy of the real `std/` tree — the base for a corrupted-std soundness probe.
fn copied_std() -> TmpDir {
    let t = TmpDir::new();
    let real = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("std");
    copy_tree(&real, &t.0);
    t
}

/// Run `chezzi run [extra] <file>` with an optional `$CHEZZI_STD`. Returns (stdout, stderr, success).
fn run(file: &Path, extra: &[&str], std_dir: Option<&Path>) -> (String, String, bool) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg("run").args(extra).arg(file);
    if let Some(d) = std_dir {
        cmd.env("CHEZZI_STD", d);
    }
    let out = cmd.output().expect("failed to spawn chezzi");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

#[test]
fn bodied_fn_runs_and_matches_across_engines() {
    let t = TmpDir::new();
    let prog = t.write(
        "main.chz",
        "import std.math\n\
         print(math.divmod(17, 5))\n\
         print(math.gcd(12, 18))\n\
         print(math.divmod(-7, 2))\n",
    );
    let (mn_out, mn_err, mn_ok) = run(&prog, &[], None);
    let (ser_out, ser_err, ser_ok) = run(&prog, &["--serial"], None);
    assert!(mn_ok, "M:N run failed: {mn_err}");
    assert!(ser_ok, "serial run failed: {ser_err}");
    // `divmod` = truncating integer division (like `a / b`): -7/2 = -3, -7 % 2 = -1.
    assert_eq!(
        mn_out, "(3, 2)\n6\n(-3, -1)\n",
        "unexpected output: {mn_out}"
    );
    assert_eq!(mn_out, ser_out, "serial vs M:N output differs");
}

#[test]
fn bodied_native_struct_method_runs() {
    let t = TmpDir::new();
    let data = t.write("data.txt", "alpha\nbeta\ngamma\n");
    let prog = t.write(
        "main.chz",
        &format!(
            "import std.io\n\
             r := io.open(\"{}\")\n\
             match r:\n\
            \x20   Ok(f):\n\
            \x20       for l in f.lines():\n\
            \x20           print(l)\n\
            \x20   Err(e):\n\
            \x20       print(\"err\")\n",
            data.display()
        ),
    );
    let (mn_out, mn_err, mn_ok) = run(&prog, &[], None);
    let (ser_out, _, ser_ok) = run(&prog, &["--serial"], None);
    assert!(mn_ok, "M:N run failed: {mn_err}");
    assert!(ser_ok, "serial run failed");
    assert_eq!(mn_out, "alpha\nbeta\ngamma\n");
    assert_eq!(mn_out, ser_out);
}

#[test]
fn native_file_can_import_another_module() {
    // A native `std/*.chz` is still a real `.chz` and may `import` — a bodied fn there uses the import.
    let stddir = copied_std();
    let math = stddir.0.join("math.chz");
    let mut src = std::fs::read_to_string(&math).unwrap();
    src.push_str(
        "\nimport std.string as s\n\
         fn parity_word(n: int) -> str:\n\
        \x20   if n % 2 == 0:\n\
        \x20       return s.reverse(\"even\")\n\
        \x20   return s.repeat(\"odd\", 2)\n",
    );
    std::fs::write(&math, src).unwrap();

    let t = TmpDir::new();
    let prog = t.write(
        "main.chz",
        "import std.math\n\
         print(math.parity_word(4))\n\
         print(math.parity_word(7))\n\
         print(math.divmod(9, 2))\n",
    );
    let (mn_out, mn_err, mn_ok) = run(&prog, &[], Some(&stddir.0));
    let (ser_out, _, ser_ok) = run(&prog, &["--serial"], Some(&stddir.0));
    assert!(mn_ok, "native-file-import run failed: {mn_err}");
    assert!(ser_ok, "serial run failed");
    assert_eq!(mn_out, "neve\noddodd\n(4, 1)\n");
    assert_eq!(mn_out, ser_out);
}

#[test]
fn ill_typed_module_level_bodied_fn_is_rejected() {
    let stddir = copied_std();
    // Append a bodied fn whose body violates its declared return type.
    let math = stddir.0.join("math.chz");
    let mut src = std::fs::read_to_string(&math).unwrap();
    src.push_str("\nfn spike_bad() -> int:\n    return \"not an int\"\n");
    std::fs::write(&math, src).unwrap();

    let t = TmpDir::new();
    let prog = t.write("main.chz", "import std.math\nprint(math.gcd(4, 6))\n");
    let (_, err, ok) = run(&prog, &[], Some(&stddir.0));
    assert!(
        !ok,
        "ill-typed bodied fn must be REJECTED, but the run succeeded"
    );
    assert!(
        err.contains("expected return type int, found str"),
        "expected a return-type error, got: {err}"
    );
}

#[test]
fn ill_typed_bodied_native_struct_method_is_rejected() {
    let stddir = copied_std();
    // Corrupt `Reader.lines`: yield an int under its `-> Iterator[str]` declaration.
    let io = stddir.0.join("io.chz");
    let src = std::fs::read_to_string(&io).unwrap();
    let bad = src.replace("Some(l): yield l", "Some(l): yield 42");
    assert_ne!(bad, src, "test fixture did not patch Reader.lines");
    std::fs::write(&io, bad).unwrap();

    let t = TmpDir::new();
    // Importing std.io pulls it into the graph so its bodied method gets checked.
    let prog = t.write("main.chz", "import std.io\nprint(\"hi\")\n");
    let (_, err, ok) = run(&prog, &[], Some(&stddir.0));
    assert!(
        !ok,
        "ill-typed bodied METHOD must be REJECTED, but the run succeeded"
    );
    assert!(
        err.contains("expected yield type str, found int"),
        "expected a yield-type error, got: {err}"
    );
}
