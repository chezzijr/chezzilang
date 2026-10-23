//! The stop-the-world GC coordinator: it stops every thread attached to one heap before a collection.
//!
//! One [`GcCoord`] serves one heap core (one run). It is Go's `stopTheWorldWithSema` plus
//! `entersyscall`/`exitsyscall` (`go/src/runtime/proc.go`), cut down to what a mark-sweep needs:
//!
//! - An ATTACHED thread may touch the heap. It must reach a safepoint ([`GcCoord::poll`]) or
//!   detach in bounded time.
//! - A DETACHED thread ([`GcCoord::detach`], Go's syscall state) counts as stopped. It publishes its
//!   roots first, then may block on anything. [`GcCoord::reattach`] waits for the end of any
//!   collection before the thread touches the heap again.
//! - The thread that wants a collection calls [`GcCoord::request_collect`]. It sets `stw`, waits
//!   until every other attached thread is stopped at a safepoint, runs the collector with the union
//!   of the roots (its own, the stopped threads', the detached threads'), then releases everyone.
//!   A second requester during that time stops like a poller: one collection serves both.
//!
//! Phases: `Idle` -> `Requested` (stw set, the requester waits for `stopped == attached - 1`) ->
//! `Collecting` (the collector runs with the lock released) -> `Idle` (epoch + 1, everyone woken).
//! The requester resets `stopped` to 0 when it releases the world: a released poller does not
//! decrement it, so a fast second request can never count a thread that is already running again.
//!
//! The coordinator knows nothing about `Vm`: roots are plain [`GcRef`] lists.
//!
//! LIVENESS RULE: a thread that is attached but neither polls nor detaches blocks a collection
//! forever (the requester waits for it). Every blocking call of an attached thread must detach
//! first; M1.5's guard test enforces that.

use crate::vm::value::GcRef;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Requested,
    Collecting,
}

struct State {
    phase: Phase,
    /// Threads that may touch the heap (detached threads excluded).
    attached: usize,
    /// Attached threads parked at a safepoint in the current stop.
    stopped: usize,
    /// Bumped once per finished collection; a stopped thread waits for it to move.
    gc_epoch: u64,
    /// Roots handed in by the threads stopped in the current stop.
    stopped_roots: Vec<GcRef>,
    /// Roots published by detached threads, keyed by their ticket.
    detached_roots: Vec<(u64, Vec<GcRef>)>,
    next_ticket: u64,
}

/// Proof of a [`GcCoord::detach`]; [`GcCoord::reattach`] consumes it to drop that thread's roots.
#[must_use = "a detached thread must reattach before it touches the heap"]
#[derive(Debug)]
pub struct DetachTicket(u64);

pub struct GcCoord {
    st: Mutex<State>,
    cv: Condvar,
    /// Set while a stop is requested; mutators read it with a Relaxed load at each safepoint.
    stw: AtomicBool,
}

impl GcCoord {
    pub fn new() -> Self {
        GcCoord {
            st: Mutex::new(State {
                phase: Phase::Idle,
                attached: 0,
                stopped: 0,
                gc_epoch: 0,
                stopped_roots: Vec::new(),
                detached_roots: Vec::new(),
                next_ticket: 0,
            }),
            cv: Condvar::new(),
            stw: AtomicBool::new(false),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait<'a>(&self, g: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.cv.wait(g).unwrap_or_else(|e| e.into_inner())
    }

    fn wait_idle<'a>(&self, mut st: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        while st.phase != Phase::Idle {
            st = self.wait(st);
        }
        st
    }

    /// The number of finished collections.
    pub fn epoch(&self) -> u64 {
        self.lock().gc_epoch
    }

    /// A thread joins the heap. It waits for a running stop to end first.
    pub fn attach(&self) {
        let mut st = self.wait_idle(self.lock());
        st.attached += 1;
    }

    /// Go `entersyscall`: publish `roots` and stop counting this thread. The thread may then block
    /// on anything; it must not touch the heap until [`GcCoord::reattach`].
    pub fn detach(&self, roots: Vec<GcRef>) -> DetachTicket {
        let mut st = self.lock();
        let ticket = st.next_ticket;
        st.next_ticket += 1;
        st.detached_roots.push((ticket, roots));
        st.attached -= 1;
        drop(st);
        // A requester may be waiting for this thread.
        self.cv.notify_all();
        DetachTicket(ticket)
    }

    /// Go `exitsyscall`: wait until no stop is running, drop the published roots, count the thread
    /// again.
    pub fn reattach(&self, ticket: DetachTicket) {
        let mut st = self.wait_idle(self.lock());
        let at = st.detached_roots.iter().position(|(t, _)| *t == ticket.0);
        st.detached_roots
            .swap_remove(at.expect("a DetachTicket names a live detach"));
        st.attached += 1;
    }

    /// A safepoint. Cheap (one Relaxed load) when no stop is requested. Under a stop it hands in
    /// `roots()` and parks until the collection ends.
    #[inline]
    pub fn poll(&self, roots: impl FnOnce() -> Vec<GcRef>) {
        if self.stw.load(Ordering::Relaxed) {
            self.stop_here(roots);
        }
    }

    #[cold]
    fn stop_here(&self, roots: impl FnOnce() -> Vec<GcRef>) {
        let st = self.lock();
        match st.phase {
            Phase::Idle => {} // a stale `stw` read: the stop already ended
            Phase::Requested | Phase::Collecting => self.park(st, roots()),
        }
    }

    /// Hand in `roots`, count this thread as stopped, and wait for the epoch to move.
    fn park(&self, mut st: MutexGuard<'_, State>, roots: Vec<GcRef>) {
        st.stopped_roots.extend(roots);
        st.stopped += 1;
        let epoch = st.gc_epoch;
        self.cv.notify_all();
        while st.gc_epoch == epoch {
            st = self.wait(st);
        }
    }

    /// Stop the world and run `collect` with every root. If another thread already requested a
    /// stop, this thread stops like a poller instead and `collect` does not run: one collection
    /// serves both. The caller must be attached.
    pub fn request_collect(&self, own_roots: Vec<GcRef>, collect: impl FnOnce(&[GcRef])) {
        let mut st = self.lock();
        match st.phase {
            Phase::Requested | Phase::Collecting => return self.park(st, own_roots),
            Phase::Idle => {}
        }
        debug_assert!(
            st.attached >= 1,
            "request_collect from a thread that is not attached"
        );
        st.phase = Phase::Requested;
        self.stw.store(true, Ordering::Relaxed);
        while st.stopped + 1 < st.attached {
            st = self.wait(st);
        }
        st.phase = Phase::Collecting;
        let mut roots = own_roots;
        roots.append(&mut st.stopped_roots);
        for (_, r) in &st.detached_roots {
            roots.extend_from_slice(r);
        }
        drop(st);
        // Release the world even if `collect` panics, so no stopped thread waits forever.
        let _release = Release(self);
        collect(&roots);
    }
}

/// Ends a stop: epoch + 1, `stw` cleared, every stopped thread released.
struct Release<'a>(&'a GcCoord);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        let c = self.0;
        let mut st = c.lock();
        st.gc_epoch += 1;
        st.stopped = 0;
        st.phase = Phase::Idle;
        c.stw.store(false, Ordering::Relaxed);
        drop(st);
        c.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::value::GcRef;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;
    use std::time::Duration;

    /// Runs `f` on its own thread and fails the test if it does not finish in 10 s: a hang in the
    /// coordinator becomes a test failure, never a stuck test binary.
    fn within<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = mpsc::channel();
        let h = thread::spawn(move || {
            let _ = tx.send(f());
        });
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(v) => {
                h.join().unwrap();
                v
            }
            Err(RecvTimeoutError::Timeout) => panic!("the coordinator test hung"),
            Err(RecvTimeoutError::Disconnected) => match h.join() {
                Err(p) => std::panic::resume_unwind(p),
                Ok(()) => unreachable!("the test thread exited without a result"),
            },
        }
    }

    fn sorted(roots: &[GcRef]) -> Vec<u32> {
        let mut v: Vec<u32> = roots.iter().map(|r| r.0).collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn stw_waits_for_attached_skips_detached() {
        within(|| {
            let c = Arc::new(GcCoord::new());
            let barrier = Arc::new(Barrier::new(3));
            let (release_tx, release_rx) = mpsc::channel::<()>();
            let released = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let roots_calls = Arc::new(AtomicUsize::new(0));

            let detacher = {
                let (c, barrier, released) = (c.clone(), barrier.clone(), released.clone());
                thread::spawn(move || {
                    c.attach();
                    barrier.wait();
                    let ticket = c.detach(vec![GcRef(1)]);
                    release_rx.recv().unwrap();
                    released.store(true, Ordering::SeqCst);
                    c.reattach(ticket);
                })
            };
            let poller = {
                let (c, barrier, stop, roots_calls) = (
                    c.clone(),
                    barrier.clone(),
                    stop.clone(),
                    roots_calls.clone(),
                );
                thread::spawn(move || {
                    c.attach();
                    barrier.wait();
                    while !stop.load(Ordering::SeqCst) {
                        c.poll(|| {
                            roots_calls.fetch_add(1, Ordering::SeqCst);
                            vec![GcRef(2)]
                        });
                        thread::yield_now();
                    }
                })
            };

            c.attach();
            barrier.wait();
            let mut runs = Vec::new();
            c.request_collect(vec![GcRef(3)], |roots| {
                runs.push((sorted(roots), released.load(Ordering::SeqCst)));
            });
            stop.store(true, Ordering::SeqCst);
            release_tx.send(()).unwrap();
            detacher.join().unwrap();
            poller.join().unwrap();

            assert_eq!(runs, vec![(vec![1, 2, 3], false)]);
            assert_eq!(
                roots_calls.load(Ordering::SeqCst),
                1,
                "poll asks for roots only under a stop"
            );
            assert_eq!(c.epoch(), 1);
        });
    }

    #[test]
    fn reattach_blocks_until_idle() {
        within(|| {
            let c = Arc::new(GcCoord::new());
            let finished = Arc::new(AtomicBool::new(false));
            let (detached_tx, detached_rx) = mpsc::channel::<()>();
            let (reattach_tx, reattach_rx) = mpsc::channel::<()>();
            let (returned_tx, returned_rx) = mpsc::channel::<bool>();
            let (started_tx, started_rx) = mpsc::channel::<()>();
            let (go_tx, go_rx) = mpsc::channel::<()>();

            let worker = {
                let (c, finished) = (c.clone(), finished.clone());
                thread::spawn(move || {
                    c.attach();
                    let ticket = c.detach(vec![GcRef(7)]);
                    detached_tx.send(()).unwrap();
                    reattach_rx.recv().unwrap();
                    c.reattach(ticket);
                    returned_tx.send(finished.load(Ordering::SeqCst)).unwrap();
                })
            };
            detached_rx.recv().unwrap();

            let collector = {
                let (c, finished) = (c.clone(), finished.clone());
                thread::spawn(move || {
                    c.attach();
                    let go_rx = Mutex::new(go_rx);
                    c.request_collect(vec![], |roots| {
                        assert_eq!(sorted(roots), vec![7]);
                        started_tx.send(()).unwrap();
                        go_rx.lock().unwrap().recv().unwrap();
                        finished.store(true, Ordering::SeqCst);
                    });
                })
            };
            started_rx.recv().unwrap();
            reattach_tx.send(()).unwrap();
            // A correct reattach stays blocked here; a broken one returns at once with `false`.
            assert!(
                returned_rx
                    .recv_timeout(Duration::from_millis(200))
                    .is_err(),
                "reattach returned while the collection was still running"
            );
            go_tx.send(()).unwrap();
            assert!(
                returned_rx.recv().unwrap(),
                "reattach returned before the collection ended"
            );
            worker.join().unwrap();
            collector.join().unwrap();
            assert_eq!(c.epoch(), 1);
        });
    }

    #[test]
    fn two_requesters_one_collection() {
        const ROUNDS: u64 = 200;
        within(|| {
            let c = Arc::new(GcCoord::new());
            let barrier = Arc::new(Barrier::new(2));
            let runs = Arc::new(AtomicUsize::new(0));
            let spawn = |id: u32| {
                let (c, barrier, runs) = (c.clone(), barrier.clone(), runs.clone());
                thread::spawn(move || {
                    c.attach();
                    for _ in 0..ROUNDS {
                        barrier.wait();
                        c.request_collect(vec![GcRef(id)], |roots| {
                            assert_eq!(sorted(roots), vec![1, 2]);
                            runs.fetch_add(1, Ordering::SeqCst);
                        });
                    }
                })
            };
            let (a, b) = (spawn(1), spawn(2));
            a.join().unwrap();
            b.join().unwrap();
            assert_eq!(
                runs.load(Ordering::SeqCst) as u64,
                ROUNDS,
                "one collection per round"
            );
            assert_eq!(c.epoch(), ROUNDS);
        });
    }
}
