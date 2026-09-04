//! B3.3-threads — the bounded OS-thread work pool that runs `--parallel` task bodies (decision B).
//!
//! A `parallel:` with N spawns must **not** become N OS threads: nested `parallel:` would explode
//! N×M. Instead there is **one** process-wide pool sized to [`super::worker_count`] (the configured
//! `--threads=N` / `CHEZZI_THREADS`, or [`available_parallelism`] when unset), created lazily on
//! first use and living for the process lifetime. A `parallel:` join farms its tasks here
//! and the joining thread itself runs one inline (decision B: the parent participates), so total
//! live threads stay bounded at `N + (joining threads)` regardless of `parallel:` nesting depth.
//!
//! Each pool thread is spawned with the same 256 MiB stack as the main VM thread
//! ([`super::VM_STACK_BYTES`]) — a worker `Vm` recurses as deeply as the parent can. Idle threads
//! block on the queue condvar; the process exiting reaps them (they are never joined).
//!
//! TICKET-052 — a pool thread about to block in place (an eager `Executor` job with `mn.is_none()`)
//! hands its slot to a freshly spawned replacement worker ([`yield_slot`]) and retires instead of
//! looping when its job ends, so the live pool count stays at [`super::worker_count`] plus the
//! number of jobs blocked RIGHT NOW rather than growing without bound or starving the job that would
//! unblock them. At most one replacement per job (`SLOT` leaves `Held` on the first yield), and a
//! block shorter than one [`super::DEMOTE_POLL_BACKOFF`] tick spawns nothing (see the call sites in
//! `sched.rs`/`netio.rs`). An OS-refused replacement blocks in place instead of faulting the job —
//! the pre-TICKET-052 behaviour, so a thread-starved host loses nothing it had.

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// A unit of pool work: a fully self-contained closure (it owns its [`super::ReadyWorker`] and the
/// result/Done channels by move), so it is `'static` and needs no borrowed state.
type Job = Box<dyn FnOnce() + Send + 'static>;

/// The shared job queue: a FIFO behind a `Mutex`, with a `Condvar` pool threads park on when idle.
type Queue = Arc<(Mutex<VecDeque<Job>>, Condvar)>;

struct Pool {
    queue: Queue,
}

static POOL: OnceLock<Pool> = OnceLock::new();

/// Get (or lazily create) the process-wide pool. First call spawns `available_parallelism()` worker
/// threads (min 1); subsequent calls return the same pool.
fn pool() -> &'static Pool {
    POOL.get_or_init(|| {
        let n = super::worker_count();
        let queue: Queue = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        for _ in 0..n {
            assert!(spawn_worker(&queue), "failed to spawn chezzi pool thread");
        }
        Pool { queue }
    })
}

/// Spawn one pool worker thread over `queue`. `false` iff the OS refused the thread.
fn spawn_worker(queue: &Queue) -> bool {
    let q = Arc::clone(queue);
    thread::Builder::new()
        .stack_size(super::VM_STACK_BYTES)
        .name("chezzi-pool".into())
        .spawn(move || worker_loop(&q))
        .is_ok()
}

/// This pool thread's current job slot: whether it still holds the OS thread it was spawned with, or
/// has handed it to a replacement (and so must retire when its job ends).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Held,
    Yielded,
    Refused,
}

thread_local! {
    /// Whether this pool thread is currently running a job (vs. idle on the queue condvar).
    /// [`yield_slot`] is a no-op off a job — only a job's own OS thread may yield its slot.
    static ON_JOB: Cell<bool> = const { Cell::new(false) };
    /// This thread's slot state for the CURRENT job, reset at the top of every `job()` call.
    static SLOT: Cell<Slot> = const { Cell::new(Slot::Held) };
    /// When the current job first asked to yield, `None` until the first call. Reset per job, not per
    /// blocking op, so a job that blocks repeatedly still spawns at most one replacement.
    static BLOCK_SINCE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Pure decision: has a block that started at `first_seen` (relative to `now`) crossed `budget`?
/// `None` budget yields immediately (the guard path, whose only caller has already paid its own
/// budget and is about to block with no deadline). `None` `first_seen` means this is the first call
/// for this block, so nothing has been observed to elapse yet.
pub(super) fn should_yield_slot(
    first_seen: Option<Instant>,
    now: Instant,
    budget: Option<Duration>,
) -> bool {
    let Some(b) = budget else {
        return true;
    };
    let Some(t) = first_seen else {
        return false;
    };
    now.duration_since(t) >= b
}

/// Called from inside a blocking-in-place wait on an eager job's OS thread. Returns `true` iff this
/// call yielded the slot (spawned a replacement worker and marked `SLOT` `Yielded`); the caller does
/// not otherwise change behaviour on the result — the thread still returns to its caller and blocks
/// exactly as before, it just no longer holds the pool at `worker_count()` while doing so. A no-op
/// (returns `false`) off a job (`ON_JOB` false) or once a job's slot has already left `Held`.
pub(super) fn yield_slot(budget: Option<Duration>) -> bool {
    if !ON_JOB.with(|c| c.get()) {
        return false;
    }
    if SLOT.with(|c| c.get()) != Slot::Held {
        return false;
    }
    let now = Instant::now();
    let first_seen = BLOCK_SINCE.with(|c| {
        if c.get().is_none() {
            c.set(Some(now));
        }
        c.get()
    });
    if !should_yield_slot(first_seen, now, budget) {
        return false;
    }
    if spawn_worker(&pool().queue) {
        SLOT.with(|c| c.set(Slot::Yielded));
        true
    } else {
        SLOT.with(|c| c.set(Slot::Refused));
        false
    }
}

/// A pool thread's lifetime: pull the next job (parking on the condvar while the queue is empty) and
/// run it. Returns (retiring the thread) once a job it ran yielded its slot to a replacement;
/// otherwise loops forever — the process exit reaps a thread that never yields.
fn worker_loop(queue: &Queue) {
    let (lock, cv) = &**queue;
    loop {
        let job = {
            let mut q = lock.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(job) = q.pop_front() {
                    break job;
                }
                q = cv.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        ON_JOB.with(|c| c.set(true));
        SLOT.with(|c| c.set(Slot::Held));
        BLOCK_SINCE.with(|c| c.set(None));
        // Defense in depth: a job already converts its task's panic into a fault slot + signals
        // completion via its `DoneSignal` guard, so `job()` should not unwind — but if it ever does
        // (e.g. a panic in the slot write itself), catching it here keeps this pool thread alive for
        // the next job instead of silently shrinking the pool.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
        ON_JOB.with(|c| c.set(false));
        if SLOT.with(|c| c.get()) == Slot::Yielded {
            return;
        }
    }
}

/// Enqueue `job` and wake one idle pool thread to run it.
pub fn submit(job: Job) {
    let (lock, cv) = &*pool().queue;
    lock.lock().unwrap().push_back(job);
    cv.notify_one();
}

#[cfg(test)]
mod tests {
    use super::should_yield_slot;
    use std::time::{Duration, Instant};

    // Pure-function tests only — `pool()` is a process-wide `OnceLock`, so no lib unit test may
    // `submit` to it (CLAUDE.md).

    #[test]
    fn no_budget_yields_immediately() {
        let t0 = Instant::now();
        assert!(should_yield_slot(Some(t0), t0, None));
    }

    #[test]
    fn first_observation_never_yields() {
        let t0 = Instant::now();
        let budget = Duration::from_millis(5);
        assert!(!should_yield_slot(None, t0, Some(budget)));
    }

    #[test]
    fn budget_elapsed_yields() {
        let t0 = Instant::now();
        let budget = Duration::from_millis(5);
        assert!(should_yield_slot(Some(t0), t0 + budget, Some(budget)));
    }

    #[test]
    fn budget_not_yet_elapsed_does_not_yield() {
        let t0 = Instant::now();
        let budget = Duration::from_millis(5);
        assert!(!should_yield_slot(Some(t0), t0 + budget / 2, Some(budget)));
    }
}
