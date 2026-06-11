//! D5 owe #2 / D6b — `std.time.sleep_ms` deadlines. Originally a dedicated timer thread; **D6b folded
//! that onto the netpoller's single poll thread** (one OS thread now serves both socket readiness and
//! sleep deadlines — see [`super::poller`]). This module is the thin compatibility shim that keeps the
//! `timer::submit_at` call site in [`super::MnSched::offload`] stable: it delegates straight to
//! [`super::poller::submit_timer`]. The behavioral tests live in `poller.rs` alongside the merged loop.

use super::poller;
use std::time::Instant;

/// A timer job: a self-contained `'static` closure (it owns the parked fiber + scheduler `Arc` by
/// move). Re-exported as the poller's `TimerJob` under a stable name for the offload call site.
pub type Job = poller::TimerJob;

/// Schedule `job` to run on (or just after) `deadline`, on the process-wide netpoller poll thread.
pub fn submit_at(deadline: Instant, job: Job) {
    poller::submit_timer(deadline, job);
}
