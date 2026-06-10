//! D5 — the growable "dirty" / blocking pool for opaque blocking native calls (`std.io.read_file` /
//! `write_file`, `std.fs.*`, `std.time.sleep_ms`).
//!
//! The core `--parallel` pool ([`super::pool`]) is bounded to `available_parallelism()`: a blocking
//! native run inline on a core worker pins it for the whole call, so enough concurrent blocking calls
//! starve the scheduler (the live G3 hazard). D5 offloads a blocking native here instead: the fiber
//! parks, the core worker is freed, and a *blocking-pool* thread runs the call and re-enqueues the
//! fiber on completion.
//!
//! Unlike the core pool this one is **growable** (à la Go `spawn_blocking` / BEAM async pool): it
//! spawns a fresh thread when a job arrives and no thread is idle (up to a generous cap), and reaps a
//! thread that sits idle past a timeout. So N concurrent `sleep_ms` calls run on ~N threads in
//! parallel rather than serializing over the core pool, and the threads drain away when the burst is
//! over.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

/// Cap on live blocking-pool threads. Generous (a blocking native is expected to be I/O-bound, not
/// CPU-bound, so many can sleep in parallel cheaply) but finite, so a pathological program can't
/// spawn unbounded OS threads. Beyond the cap, blocking jobs queue.
const BLOCKING_POOL_CAP: usize = 512;
/// A blocking-pool thread idle this long reaps itself — the pool shrinks back after a burst.
const REAP_AFTER: Duration = Duration::from_secs(10);

static POOL: OnceLock<BlockingPool> = OnceLock::new();

/// Submit `job` to the process-wide blocking pool (lazily created on first use).
pub fn submit(job: Job) {
    POOL.get_or_init(|| BlockingPool::new(BLOCKING_POOL_CAP, REAP_AFTER)).submit(job);
}

/// A unit of blocking work: a self-contained `'static` closure (it owns the offloaded fiber, the
/// `NativeFn` + extracted args, and the scheduler `Arc` by move).
pub type Job = Box<dyn FnOnce() + Send + 'static>;

/// The mutable pool state, all guarded by the one mutex so the grow/reap accounting is consistent.
struct State {
    queue: VecDeque<Job>,
    /// Threads currently parked in `wait_timeout` waiting for a job.
    idle: usize,
    /// Live threads (spawned, not yet reaped). Incremented in [`BlockingPool::submit`] under the
    /// lock *before* the thread spawns (so a back-to-back submit sees the right count), decremented
    /// by a thread that reaps itself.
    total: usize,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}

/// A growable blocking-work pool. Construct one with [`BlockingPool::new`]; the process-wide instance
/// lives behind a `OnceLock` ([`global`]).
pub struct BlockingPool {
    shared: Arc<Shared>,
    /// Max live threads. A job submitted while all `cap` threads are busy waits in the queue until
    /// one frees, rather than spawning unbounded threads.
    cap: usize,
    /// A thread idle longer than this reaps itself (shrinks the pool back down after a burst).
    reap_after: Duration,
}

impl BlockingPool {
    fn new(cap: usize, reap_after: Duration) -> Self {
        BlockingPool {
            shared: Arc::new(Shared { state: Mutex::new(State { queue: VecDeque::new(), idle: 0, total: 0 }), cv: Condvar::new() }),
            cap: cap.max(1),
            reap_after,
        }
    }

    /// Enqueue `job`. If every live thread is busy (fewer parked threads than queued jobs) and we are
    /// under the cap, spawn a fresh thread; otherwise wake a parked one (or, at the cap, leave it for
    /// a busy thread to pick up when it loops).
    pub fn submit(&self, job: Job) {
        let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        st.queue.push_back(job);
        if st.idle < st.queue.len() && st.total < self.cap {
            // Fewer parked threads than queued jobs, and we're under the cap → grow. The new thread
            // checks the queue itself on startup, so no notify is needed here.
            st.total += 1;
            drop(st);
            self.spawn_thread();
        } else {
            // Either a parked thread per queued job already exists, or we're at the cap (a busy
            // thread pops the job when it loops). `notify_all`, not `notify_one`: a single
            // notification can be consumed by a thread that is timing out to reap, leaving a queued
            // job unwoken until the reap timeout; waking all idle threads (they re-check the queue
            // and re-park if empty) closes that race. Matches the scheduler core's wake discipline.
            self.shared.cv.notify_all();
        }
    }

    fn spawn_thread(&self) {
        let shared = Arc::clone(&self.shared);
        let reap_after = self.reap_after;
        std::thread::Builder::new()
            .stack_size(super::VM_STACK_BYTES)
            .name("chezzi-blocking".into())
            .spawn(move || worker_loop(&shared, reap_after))
            .expect("failed to spawn chezzi blocking-pool thread");
    }

    #[cfg(test)]
    fn total_threads(&self) -> usize {
        self.shared.state.lock().unwrap_or_else(|e| e.into_inner()).total
    }
}

/// A blocking-pool thread's lifetime: pull and run jobs, parking (with a reap timeout) while the queue
/// is empty. Reaps itself — returns, decrementing `total` — once it has sat idle past `reap_after`
/// with nothing queued.
fn worker_loop(shared: &Shared, reap_after: Duration) {
    loop {
        let job = {
            let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(job) = st.queue.pop_front() {
                    break Some(job);
                }
                st.idle += 1;
                let (g, res) = shared.cv.wait_timeout(st, reap_after).unwrap_or_else(|e| e.into_inner());
                st = g;
                st.idle -= 1;
                if res.timed_out() && st.queue.is_empty() {
                    st.total -= 1;
                    break None; // reap
                }
                // Woken (or spurious): re-check the queue.
            }
        };
        match job {
            Some(job) => {
                // A job is panic-guarded at its own boundary, but catch here too so one panicking
                // job never silently shrinks the pool below what the accounting believes.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            }
            None => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A submitted job actually runs on a pool thread.
    #[test]
    fn runs_submitted_job() {
        let pool = BlockingPool::new(8, Duration::from_secs(10));
        let (tx, rx) = mpsc::channel();
        pool.submit(Box::new(move || tx.send(42).unwrap()));
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).expect("job ran"), 42);
    }

    /// When a job arrives and the only thread is busy, the pool spawns a second thread (grow-on-stall)
    /// rather than queueing behind the busy one.
    #[test]
    fn grows_when_no_thread_is_idle() {
        let pool = BlockingPool::new(8, Duration::from_secs(10));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let started = Arc::new((Mutex::new(0usize), Condvar::new()));

        // Two jobs that each block until released. Each bumps `started` so we can wait for both to be
        // actually running (not merely submitted) before asserting the thread count.
        for _ in 0..2 {
            let gate = Arc::clone(&gate);
            let started = Arc::clone(&started);
            pool.submit(Box::new(move || {
                {
                    let (l, c) = &*started;
                    *l.lock().unwrap() += 1;
                    c.notify_all();
                }
                let (l, c) = &*gate;
                let mut open = l.lock().unwrap();
                while !*open {
                    open = c.wait(open).unwrap();
                }
            }));
        }

        // Wait (bounded) for both jobs to be running concurrently — a regression that fails to grow
        // leaves the second job queued, so this times out and fails loudly rather than hanging.
        let (l, c) = &*started;
        let mut n = l.lock().unwrap();
        let deadline = Duration::from_secs(5);
        while *n < 2 {
            let (g, res) = c.wait_timeout(n, deadline).unwrap();
            n = g;
            assert!(!res.timed_out() || *n >= 2, "second concurrent job never started — pool did not grow");
        }
        drop(n);
        assert_eq!(pool.total_threads(), 2, "pool grew a second thread for the second concurrent job");

        // Release both.
        let (l, c) = &*gate;
        *l.lock().unwrap() = true;
        c.notify_all();
    }

    /// A thread that finishes its job and sits idle past `reap_after` reaps itself.
    #[test]
    fn reaps_idle_thread() {
        let pool = BlockingPool::new(8, Duration::from_millis(80));
        let (tx, rx) = mpsc::channel();
        pool.submit(Box::new(move || tx.send(()).unwrap()));
        rx.recv_timeout(Duration::from_secs(5)).expect("job ran");
        assert_eq!(pool.total_threads(), 1, "one thread spawned for the job");

        // Past the reap timeout the idle thread exits.
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(pool.total_threads(), 0, "idle thread reaped after the timeout");
    }

    /// The pool never exceeds its cap, even with more concurrent blocking jobs than the cap.
    #[test]
    fn respects_cap() {
        let pool = BlockingPool::new(1, Duration::from_secs(10));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let ran = Arc::new(Mutex::new(0usize));

        for _ in 0..3 {
            let gate = Arc::clone(&gate);
            let ran = Arc::clone(&ran);
            pool.submit(Box::new(move || {
                let (l, c) = &*gate;
                let mut open = l.lock().unwrap();
                while !*open {
                    open = c.wait(open).unwrap();
                }
                *ran.lock().unwrap() += 1;
            }));
        }
        // Give any erroneous extra threads a chance to spawn.
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(pool.total_threads(), 1, "cap of 1 honored despite 3 concurrent jobs");

        // Release; all three jobs eventually run on the single capped thread.
        let (l, c) = &*gate;
        *l.lock().unwrap() = true;
        c.notify_all();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if *ran.lock().unwrap() == 3 {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "all jobs ran on the capped pool");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
