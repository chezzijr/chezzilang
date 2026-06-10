//! D5 owe #2 — a single process-wide timer thread that parks fibers for `std.time.sleep_ms`, instead
//! of tying up one dirty-pool thread per sleep.
//!
//! Under D5 a `sleep_ms(N)` rode the blocking pool ([`super::blocking_pool`]): a pool thread did
//! `thread::sleep(N)`, so N concurrent sleepers cost N OS threads for the whole duration. But a sleep
//! does no work — it only waits a deadline — so it needs no thread of its own. This service keeps a
//! min-heap of `(deadline, job)` and one thread that sleeps until the nearest deadline, then runs
//! every due job. A timer job re-enqueues the parked fiber via the scheduler's `complete_offload`
//! (exactly like the blocking pool's completion path), so **one** thread serves any number of
//! concurrent sleepers (10⁴ sleepers ≈ 1 thread, not 10⁴).

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

/// A timer job: a self-contained `'static` closure (it owns the parked fiber and the scheduler `Arc`
/// by move, like a [`super::blocking_pool::Job`]).
pub type Job = Box<dyn FnOnce() + Send + 'static>;

static SERVICE: OnceLock<TimerService> = OnceLock::new();

/// Schedule `job` to run on (or just after) `deadline`, on the process-wide timer thread (lazily
/// created on first use).
pub fn submit_at(deadline: Instant, job: Job) {
    SERVICE.get_or_init(TimerService::new).submit_at(deadline, job);
}

/// One scheduled timer. Ordered by `(deadline, seq)` only — the `seq` tie-breaker keeps the order
/// total without requiring `Job: Ord` (two timers with the same deadline still compare distinctly, so
/// `Eq`/`Ord` are consistent and the heap is well-defined). The job itself is never compared.
struct Entry {
    deadline: Instant,
    seq: u64,
    job: Job,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.seq == other.seq
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Entry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline.cmp(&other.deadline).then(self.seq.cmp(&other.seq))
    }
}

struct State {
    /// Pending timers. Stored as `Reverse<Entry>` so the [`BinaryHeap`] (a max-heap) yields the
    /// *earliest* deadline on `peek`/`pop`.
    heap: BinaryHeap<Reverse<Entry>>,
    /// Monotonic counter handing each timer a unique tie-breaker.
    next_seq: u64,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}

/// The process-wide timer: a min-heap of pending timers plus one thread draining it.
pub struct TimerService {
    shared: Arc<Shared>,
}

impl TimerService {
    fn new() -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State { heap: BinaryHeap::new(), next_seq: 0 }),
            cv: Condvar::new(),
        });
        let t = Arc::clone(&shared);
        // The timer thread only ever locks the heap + calls the job (which re-enqueues a fiber on the
        // scheduler); it never runs interpreter code, so the default stack is ample.
        std::thread::Builder::new()
            .name("chezzi-timer".into())
            .spawn(move || timer_loop(&t))
            .expect("failed to spawn chezzi timer thread");
        TimerService { shared }
    }

    fn submit_at(&self, deadline: Instant, job: Job) {
        let mut st = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let seq = st.next_seq;
        st.next_seq += 1;
        st.heap.push(Reverse(Entry { deadline, seq, job }));
        drop(st);
        // Wake the timer thread to re-evaluate the nearest deadline: the new entry may be sooner than
        // the one it is currently sleeping until. `notify_one` — there is exactly one waiter.
        self.shared.cv.notify_one();
    }
}

/// The timer thread's lifetime: run any due jobs, then sleep until the next deadline (or
/// indefinitely while empty), re-evaluating on every wake (a sooner timer arrived, or a spurious
/// wake). Holds the heap lock only to peek/pop — a firing job runs with the lock released so it can
/// re-enter the scheduler and so concurrent `submit_at`s never block behind a job.
fn timer_loop(shared: &Shared) {
    let mut st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        // Copy out the nearest deadline (an `Instant` is `Copy`), ending the borrow of `st` so the
        // arms below can mutate/unlock it.
        let next_deadline = st.heap.peek().map(|Reverse(e)| e.deadline);
        match next_deadline {
            Some(deadline) => {
                let now = Instant::now();
                if deadline <= now {
                    let Reverse(entry) = st.heap.pop().expect("peeked, so non-empty");
                    drop(st);
                    // A panicking timer job must not poison the heap lock or kill the timer thread
                    // (which would strand every other sleeper). Catch + swallow, mirroring the
                    // blocking pool's job boundary; the fiber's own panic path already faulted it.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(entry.job));
                    st = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                } else {
                    let (g, _) = shared.cv.wait_timeout(st, deadline - now).unwrap_or_else(|e| e.into_inner());
                    st = g;
                }
            }
            None => {
                // Empty heap: park until a `submit_at` notifies.
                st = shared.cv.wait(st).unwrap_or_else(|e| e.into_inner());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// A submitted job fires on (or just after) its deadline — not early, not never.
    #[test]
    fn fires_job_after_its_deadline() {
        let (tx, rx) = mpsc::channel();
        let start = Instant::now();
        submit_at(start + Duration::from_millis(80), Box::new(move || {
            let _ = tx.send(start.elapsed());
        }));
        let elapsed = rx.recv_timeout(Duration::from_secs(5)).expect("timer fired within 5s");
        // Allow modest scheduler jitter below the nominal 80 ms, but it must not fire immediately.
        assert!(elapsed >= Duration::from_millis(60), "timer fired too early: {elapsed:?}");
    }

    /// A far-future timer must not delay a nearer one: the heap orders by deadline, so the sooner
    /// deadline fires first even when submitted second.
    #[test]
    fn nearer_deadline_fires_before_a_far_one() {
        let (tx, rx) = mpsc::channel();
        let start = Instant::now();
        let tx_far = tx.clone();
        submit_at(start + Duration::from_millis(400), Box::new(move || {
            let _ = tx_far.send("far");
        }));
        submit_at(start + Duration::from_millis(40), Box::new(move || {
            let _ = tx.send("near");
        }));
        let first = rx.recv_timeout(Duration::from_secs(5)).expect("a timer fired");
        assert_eq!(first, "near", "the sooner deadline must fire first");
    }

    /// Many concurrent timers all fire — one thread serves an arbitrary number of sleepers (the whole
    /// point: N sleepers ≈ 1 thread, not N). They also fire roughly together (~max not sum).
    #[test]
    fn many_timers_all_fire_on_one_thread() {
        let (tx, rx) = mpsc::channel();
        let n = 200;
        let start = Instant::now();
        for _ in 0..n {
            let tx = tx.clone();
            submit_at(start + Duration::from_millis(50), Box::new(move || {
                let _ = tx.send(());
            }));
        }
        drop(tx);
        let mut got = 0;
        while rx.recv_timeout(Duration::from_secs(5)).is_ok() {
            got += 1;
        }
        assert_eq!(got, n, "every concurrent timer fired");
        assert!(start.elapsed() < Duration::from_secs(2), "200 timers serialized instead of sharing one thread");
    }
}
