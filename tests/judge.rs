//! Gate over the `judge/` known-answer oracle: `judge/run.chz` must type-check against the
//! current `std.fs` surface, or the oracle silently bit-rots with nothing to notice (TICKET-047).

use std::process::Command;

#[test]
fn judge_run_type_checks() {
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["check", "judge/run.chz"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run chezzi check judge/run.chz");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "judge/run.chz failed to type-check:\n{stderr}"
    );
}

use std::path::PathBuf;

/// Every committed `judge/problems/<slug>/samples/*.in`, counted at check time. A literal
/// here would break the moment a problem is added, and would not notice a harness that
/// silently globbed nothing.
fn committed_sample_count() -> usize {
    let probs = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("judge/problems");
    let mut n = 0;
    for entry in std::fs::read_dir(&probs).expect("read judge/problems") {
        let samples = entry.expect("judge/problems entry").path().join("samples");
        if let Ok(rd) = std::fs::read_dir(&samples) {
            n += rd
                .filter_map(|f| f.ok())
                .filter(|f| f.path().extension().map(|x| x == "in").unwrap_or(false))
                .count();
        }
    }
    n
}

/// Run the just-built binary against the harness, pointing the harness at that same binary.
fn judge(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(args)
        .env("CHEZZI_BIN", env!("CARGO_BIN_EXE_chezzi"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap_or_else(|e| panic!("run chezzi {args:?}: {e}"));
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `chezzi check` writes diagnostics to stderr and `ok: no type errors` to stdout, so an
/// empty stderr is the whole warning channel. This goes red on its own if the discarded
/// `Option` at `judge/run.chz:50` is left unbound: the check then exits 0 and still warns.
#[test]
fn judge_run_check_is_warning_free() {
    let (_, _, stderr) = judge(&["check", "judge/run.chz"]);
    assert!(
        stderr.is_empty(),
        "chezzi check judge/run.chz is not silent:\n{stderr}"
    );
}

/// The committed samples must actually RUN, not merely type-check: this exercises the glob,
/// the `list_dir` slug scan, the subprocess and the verdict logic. `--samples-only` keeps it
/// bounded and deterministic on a machine that has run `python3 judge/generate.py`.
#[test]
fn judge_samples_pass() {
    let n = committed_sample_count();
    assert!(
        n > 0,
        "no committed samples under judge/problems/*/samples/"
    );
    let (ok, stdout, stderr) = judge(&["run", "judge/run.chz", "--samples-only"]);
    assert!(ok, "judge harness exited non-zero:\n{stdout}\n{stderr}");
    assert!(
        stdout.contains(&format!(
            "done: {n} case(s), 0 failure(s), 0 problem(s) skipped"
        )),
        "expected {n} committed sample case(s), all passing:\n{stdout}\n{stderr}"
    );
}

/// The gate must test the binary cargo just built. `.project/run-test.sh` builds into
/// `~/.cache/chezzi-target-pipeline/<key>`, so the harness's default
/// `./target/release/chezzi` is a stale, unrelated binary there - and in a fresh ticket
/// worktree it is absent entirely. A non-zero exit and the string `exit 127` are therefore
/// facts BOTH branches produce, so neither can falsify this. The falsifying evidence is the
/// PATH: `judge_case` builds its FAULT detail as `exit {r.code}: {first_line(r.stderr)}`
/// and `main` prints that detail to stdout, so a harness that read CHEZZI_BIN names
/// `/nonexistent/chezzi` there, while one that ignored it names `./target/release/chezzi`
/// or passes at exit 0.
#[test]
fn judge_harness_honours_chezzi_bin() {
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", "judge/run.chz", "--samples-only", "weird_algorithm"])
        .env("CHEZZI_BIN", "/nonexistent/chezzi")
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run chezzi run judge/run.chz");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success() && stdout.contains("/nonexistent/chezzi"),
        "CHEZZI_BIN was ignored - the harness ran some other binary:\n{stdout}"
    );
}

/// The full sweep, off the default `cargo test` path (40.9 s for 318 cases on a release
/// binary, and far slower on a debug one). Run `python3 judge/generate.py` first, then
/// `cargo test --release --test judge -- --ignored`. Mirrors
/// `tests/difftest.rs::fuzz_full_heavy`.
#[test]
#[ignore]
fn judge_full_sweep_heavy() {
    let data = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("judge/data");
    assert!(
        data.is_dir(),
        "judge/data/ is missing - run: python3 judge/generate.py"
    );
    let (ok, stdout, stderr) = judge(&["run", "judge/run.chz"]);
    assert!(
        ok && stdout.contains("0 failure(s), 0 problem(s) skipped"),
        "full judge sweep did not pass clean:\n{stdout}\n{stderr}"
    );
}
