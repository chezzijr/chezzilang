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
