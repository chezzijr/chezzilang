//! W14-35c (TICKET-150): a generator fault names the faulting resume site, a bad manifest
//! entrypoint is rejected before any user code runs, and a self-referencing container prints.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_t150_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn chezzi(dir: &TmpDir, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .current_dir(&dir.0)
        .args(args)
        .output()
        .expect("failed to run chezzi")
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn generator_fault_names_the_faulting_resume_site() {
    let d = TmpDir::new();
    std::fs::write(
        d.0.join("gf3.chz"),
        "fn gen() -> Iterator[int]:\n    yield 1\n    yield 2\n    xs := [1]\n    yield xs[5]\nfn main():\n    g := gen()\n    a := g.next()\n    b := g.next()\n    print(a, b)\n    c := g.next()\n    print(c)\nmain()\n",
    )
    .unwrap();
    let out = text(&chezzi(&d, &["run", "gf3.chz"]));
    assert!(
        out.contains("called at gf3.chz:11:10"),
        "trace should name the faulting resume (line 11); got:\n{out}"
    );
}

#[test]
fn bad_manifest_entrypoint_is_rejected_before_user_code_runs() {
    let d = TmpDir::new();
    std::fs::create_dir_all(d.0.join("src")).unwrap();
    std::fs::write(
        d.0.join("chezzi.toml"),
        "[project]\nname = \"p\"\nentrypoint = \"src.e3:nope\"\n",
    )
    .unwrap();
    std::fs::write(
        d.0.join("src/e3.chz"),
        "print(\"USER CODE RAN\")\nfn main():\n    print(\"m\")\n",
    )
    .unwrap();
    let out = text(&chezzi(&d, &["run"]));
    assert!(
        !out.contains("USER CODE RAN"),
        "module top level ran before the entrypoint was validated; got:\n{out}"
    );
    assert!(
        out.contains("chezzi.toml"),
        "error should name chezzi.toml; got:\n{out}"
    );
}

#[test]
fn manifest_entrypoint_with_two_colons_is_rejected() {
    let d = TmpDir::new();
    std::fs::create_dir_all(d.0.join("src")).unwrap();
    std::fs::write(
        d.0.join("chezzi.toml"),
        "[project]\nname = \"p\"\nentrypoint = \"src.e3:main:x\"\n",
    )
    .unwrap();
    std::fs::write(d.0.join("src/e3.chz"), "fn main():\n    print(\"m\")\n").unwrap();
    let o = chezzi(&d, &["run"]);
    let out = text(&o);
    assert!(
        !o.status.success() && out.contains("chezzi.toml"),
        "run should reject a two-colon entrypoint naming chezzi.toml; got (rc={:?}):\n{out}",
        o.status.code()
    );
}

#[test]
fn self_referencing_list_prints_with_ellipsis() {
    let d = TmpDir::new();
    std::fs::write(
        d.0.join("cyc.chz"),
        "xs: List[Any] = [1]\nxs.push(xs)\nprint(xs)\n",
    )
    .unwrap();
    let out = text(&chezzi(&d, &["run", "cyc.chz"]));
    assert!(
        out.contains("[1, [...]]"),
        "expected CPython's rendering; got:\n{out}"
    );
}
