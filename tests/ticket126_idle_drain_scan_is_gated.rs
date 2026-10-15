//! TICKET-126 (W13-24) — source-text rule: `MnSched::take_runnable`'s idle path must gate its
//! per-pass cancel-drain scan behind a cancel-generation check, not call
//! `cancelled_scope_awaiting_drain` on every idle pass regardless of whether any cancel was ever
//! tripped (TICKET-118's measured 17.9% ping-pong regression at the default worker count).
//!
//! Modelled on `tests/d3_test_lives_in_the_cli_target.rs` (TICKET-114): a source-text check rather
//! than a wall-clock bound, because a wall-clock ratio cannot be the in-suite test
//! (`tests/no_wall_clock_ratio_gates.rs`) and this needs to be RED on `main`'s production code
//! unchanged, with no test-only instrumentation.
//!
//! Red on base: `take_runnable` calls `c.cancelled_scope_awaiting_drain(self)` directly. Green once the
//! fix routes that call through a generation gate (e.g. `SchedCore::drain_scan_due`) that skips the
//! scan while no cancel flag has tripped since the sched's last scan found nothing.

use std::fs;

#[test]
fn take_runnable_gates_the_idle_cancel_drain_scan() {
    // Every production VM source: the scheduler may live in any file of `src/vm/`.
    let mut read = 0;
    for entry in fs::read_dir("src/vm").expect("read src/vm") {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".rs") || name.ends_with("tests.rs") {
            continue;
        }
        let src = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
        read += 1;
        // The raw O(scopes) scan is private to `SchedCore`: only its own methods (receiver `self`,
        // that is `drain_scan_due` and `any_cancelled_scope_awaiting_drain`) may call it. Any
        // other receiver (`c.` in `take_runnable`) is a direct call on the idle path.
        const SCAN: &str = "cancelled_scope_awaiting_drain(";
        for (at, _) in src.match_indices(SCAN) {
            let before = &src[..at];
            let allowed =
                before.ends_with("fn ") || before.ends_with("any_") || before.ends_with("self.");
            let line = before.lines().count() + usize::from(before.ends_with('\n'));
            assert!(
                allowed,
                "src/vm/{name}:{line}: a call of `{SCAN}..)` outside `SchedCore`'s own methods scans \
                 every idle pass; it must go through the cancel-generation gate `drain_scan_due` \
                 (TICKET-126, W13-24)"
            );
        }
    }
    assert!(read > 0, "found no source under src/vm");
}
