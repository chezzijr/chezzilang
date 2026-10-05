//! TICKET-211 — the process-wide cache of parked raw runner threads. `Vm::start_runners` hands
//! each non-Executor runner it claimed to [`start`] as a [`Lease`]; a parked thread takes the
//! lease if one is free, else a new `chezzi-eager-helper` thread starts. Without the cache every
//! nursery started and joined one OS thread per claimed wid: 20 000 rounds of `churn8` ran at
//! 1.55x base at 4 workers (owner note 2026-10-05). The pool is not a substitute: a pool helper
//! serving a scope whose enclosing body is parked hung 21 `vm::tests` (DEC-159), and the claim
//! fires while the body is open. Executor runners stay outside the cache (DEC-213).
//!
//! INVARIANTS:
//! 1. Under the lock `idle >= leases.len()`, so a queued lease always has a parked thread to take it.
//! 2. Only an ungated thread parks, because width state is per OS thread and permanent
//!    (`src/vm/width.rs`); a born-gated lease always gets a fresh thread.
//! 3. `idle += 1`, the slot drop and `helper_done` happen in that order under the cache lock, so a
//!    nursery whose join returns finds its helpers already parked.
//! 4. A lease holds its `NestedDrainerSlot` from `start_runners` until it parks, and a thread is
//!    spawned only when no parked thread is unpromised, so cached threads never exceed the slot
//!    budget `worker_count().max(2)`.

use super::sched::{NestedDrainerSlot, spawn_runner_thread};
use super::{MnSched, SENTINEL_SCOPE, Vm, width};
use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// One runner's work: run `shell` as worker `wid` of `sched` until `terminate`.
pub(super) struct Lease {
    pub(super) sched: Arc<MnSched>,
    pub(super) shell: Vm,
    pub(super) wid: usize,
    pub(super) slot: NestedDrainerSlot,
}

struct Cache {
    /// Parked threads, promised or not (invariant 1).
    idle: usize,
    leases: VecDeque<Lease>,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache {
    idle: 0,
    leases: VecDeque::new(),
});
static WAKE: Condvar = Condvar::new();

/// How long a parked thread waits for its next lease before it exits.
const RUNNER_LINGER: Duration = Duration::from_secs(1);

/// Run `lease` on a parked thread if one is free, else on a new thread.
pub(super) fn start(lease: Lease, born_gated: bool) -> std::io::Result<()> {
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if !born_gated && c.idle > c.leases.len() {
        c.leases.push_back(lease);
        WAKE.notify_one();
        return Ok(());
    }
    drop(c);
    #[cfg(test)]
    lease
        .sched
        .quiesce
        .runner_spawns
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let sched = Arc::clone(&lease.sched);
    spawn_runner_thread(&sched, "chezzi-eager-helper", born_gated, move || {
        serve(lease)
    })
    .map(drop)
}

/// A runner thread's life: serve one lease, park, take the next one or leave after
/// [`RUNNER_LINGER`].
fn serve(first: Lease) {
    let mut next = Some(first);
    while let Some(lease) = next.take() {
        let Lease {
            sched,
            mut shell,
            wid,
            slot,
        } = lease;
        let ok = std::panic::catch_unwind(AssertUnwindSafe(|| {
            shell.mn_worker_loop(&sched, wid, SENTINEL_SCOPE)
        }))
        .is_ok();
        drop(shell);
        let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let park = ok && !width::gated();
        if park {
            c.idle += 1;
        }
        drop(slot);
        sched.helper_done();
        drop(sched);
        if !park {
            return;
        }
        loop {
            if let Some(l) = c.leases.pop_front() {
                c.idle -= 1;
                next = Some(l);
                break;
            }
            let (g, timeout) = WAKE
                .wait_timeout(c, RUNNER_LINGER)
                .unwrap_or_else(|e| e.into_inner());
            c = g;
            if timeout.timed_out() && c.leases.is_empty() {
                c.idle -= 1;
                return;
            }
        }
    }
}
