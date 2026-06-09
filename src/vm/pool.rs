//! B3.3-threads — the bounded OS-thread work pool that runs `--parallel` task bodies (decision B).
//!
//! A `parallel:` with N spawns must **not** become N OS threads: nested `parallel:` would explode
//! N×M. Instead there is **one** process-wide pool sized to [`available_parallelism`], created
//! lazily on first use and living for the process lifetime. A `parallel:` join farms its tasks here
//! and the joining thread itself runs one inline (decision B: the parent participates), so total
//! live threads stay bounded at `N + (joining threads)` regardless of `parallel:` nesting depth.
//!
//! Each pool thread is spawned with the same 256 MiB stack as the main VM thread
//! ([`super::VM_STACK_BYTES`]) — a worker `Vm` recurses as deeply as the parent can. Idle threads
//! block on the queue condvar; the process exiting reaps them (they are never joined).
//!
//! Known v1 hazard (documented, accepted — decision B / risk G3): a bounded pool + blocking `recv`
//! can starve if every pool thread blocks on a producer that is queued-but-unscheduled. Mitigation
//! is parent-participation + the "tasks should not out-block the pool" rule; work-stealing /
//! grow-on-stall is deferred.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;

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
        let n = thread::available_parallelism().map(|x| x.get()).unwrap_or(1).max(1);
        let queue: Queue = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        for _ in 0..n {
            let q = Arc::clone(&queue);
            thread::Builder::new()
                .stack_size(super::VM_STACK_BYTES)
                .name("chezzi-pool".into())
                .spawn(move || worker_loop(&q))
                .expect("failed to spawn chezzi pool thread");
        }
        Pool { queue }
    })
}

/// A pool thread's lifetime: pull the next job (parking on the condvar while the queue is empty) and
/// run it, forever. Never returns; the process exit reaps the thread.
fn worker_loop(queue: &Queue) {
    let (lock, cv) = &**queue;
    loop {
        let job = {
            let mut q = lock.lock().unwrap();
            loop {
                if let Some(job) = q.pop_front() {
                    break job;
                }
                q = cv.wait(q).unwrap();
            }
        };
        job();
    }
}

/// Enqueue `job` and wake one idle pool thread to run it.
pub fn submit(job: Job) {
    let (lock, cv) = &*pool().queue;
    lock.lock().unwrap().push_back(job);
    cv.notify_one();
}
