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
//! Red on base: `take_runnable` calls `c.cancelled_scope_awaiting_drain()` directly. Green once the
//! fix routes that call through a generation gate (e.g. `SchedCore::drain_scan_due`) that skips the
//! scan while no cancel flag has tripped since the sched's last scan found nothing.

use std::fs;

#[test]
fn take_runnable_gates_the_idle_cancel_drain_scan() {
    let src = fs::read_to_string("src/vm/mod.rs").expect("read src/vm/mod.rs");
    assert!(
        !src.contains("c.cancelled_scope_awaiting_drain()"),
        "src/vm/mod.rs: take_runnable still calls `c.cancelled_scope_awaiting_drain()` directly on \
         every idle pass; it must go through a cancel-generation gate instead (TICKET-126, W13-24)"
    );
}
