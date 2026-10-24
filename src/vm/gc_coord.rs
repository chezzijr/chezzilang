//! The stop-the-world GC coordinator: it stops every thread attached to one heap before a collection.
//!
//! One [`GcCoord`] serves one heap core (one run). It is Go's `stopTheWorldWithSema` plus
//! `entersyscall`/`exitsyscall` (`go/src/runtime/proc.go`), cut down to what a mark-sweep needs:
//!
//! - An ATTACHED thread holds an [`Attached`] token from [`GcCoord::attach`] and may touch the heap.
//!   It must reach a safepoint ([`Attached::poll`]) or detach in bounded time. One token per OS
//!   thread: a thread that holds two would count twice, and a stop would wait for the one it does
//!   not poll with.
//! - A DETACHED thread ([`Attached::detach`], Go's syscall state) counts as stopped. It publishes its
//!   roots first, then may block on anything. [`DetachTicket::reattach`] waits for the end of any
//!   collection before the thread touches the heap again.
//! - The thread that wants a collection calls [`Attached::request_collect`]. It sets `stw`, waits
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
//! What the types enforce:
//! - Only an attached thread can poll, request a collection or detach: those are methods of
//!   [`Attached`], and `detach` consumes it. A thread is counted in `stopped` only if it is counted
//!   in `attached`.
//! - LEAVE: dropping an [`Attached`] (a return or a panic) leaves the heap, also during a stop.
//!   Dropping a [`DetachTicket`] without a reattach leaves too and withdraws its published roots.
//!   A ticket holds its own coordinator, so it can never touch another coordinator's roots.
//! - A roots closure of `poll` runs before the poll takes the lock: it may read the coordinator
//!   (`epoch`, [`GcCoord::stop_stats`]).
//!   A panic in it unwinds out of `poll` before the thread is counted as stopped. The thread leaves
//!   mid-stop when the unwind drops its token (accepted: the requester then proceeds without it).
//!
//! What the types do not enforce:
//! - LIVENESS: a thread that holds an [`Attached`] but neither polls nor detaches blocks a collection
//!   forever (the requester waits for it). Every blocking call of an attached thread must detach
//!   first; M1.5's guard test enforces that.
//! - `collect` runs with the lock released and every other attached thread stopped. It must not
//!   call into the coordinator except to read it (`epoch`, [`GcCoord::stop_stats`]): an `attach`
//!   or a reattach there waits for the end of this same stop. (The requester's own token is
//!   borrowed, so `collect` cannot poll or request with it.)

use crate::vm::value::GcRef;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Requested,
    Collecting,
}

struct State {
    phase: Phase,
    /// Threads that hold an [`Attached`] token.
    attached: usize,
    /// Attached threads parked at a safepoint in the current stop.
    stopped: usize,
    /// Bumped once per finished collection; a stopped thread waits for it to move.
    gc_epoch: u64,
    /// Roots handed in by the threads stopped in the current stop.
    stopped_roots: Vec<GcRef>,
    /// Roots published by detached threads, keyed by their ticket id.
    detached_roots: Vec<(u64, Vec<GcRef>)>,
    next_ticket: u64,
    /// Stops that reached `Collecting` (`CHEZZI_HEAP_STATS`).
    stops: u64,
    /// The longest time from a request entering `Requested` to `Collecting`.
    max_stop: Duration,
}

pub struct GcCoord {
    st: Mutex<State>,
    cv: Condvar,
    /// Set while a stop is requested; mutators read it with a Relaxed load at each safepoint.
    stw: AtomicBool,
}

/// An attached thread's right to touch the heap. Dropping it leaves the heap.
#[must_use = "dropping the token leaves the heap at once"]
pub struct Attached {
    /// `None` only after [`Attached::detach`] moved it into the ticket, so the drop does not leave
    /// a second time.
    coord: Option<Arc<GcCoord>>,
}

/// A detached thread's published roots. [`DetachTicket::reattach`] turns it back into an
/// [`Attached`]; dropping it instead leaves the heap and withdraws the roots.
#[must_use = "dropping the ticket withdraws the published roots"]
pub struct DetachTicket {
    /// `None` after a reattach took it, so the drop does not withdraw the roots a second time.
    coord: Option<Arc<GcCoord>>,
    id: u64,
    /// The collection count at the detach: a reattach that sees it moved reports it.
    epoch: u64,
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
                stops: 0,
                max_stop: Duration::ZERO,
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
    #[cfg(test)]
    pub fn epoch(&self) -> u64 {
        self.lock().gc_epoch
    }

    /// Is a stop requested? The one Relaxed load a safepoint makes ([`Attached::poll`]), for a
    /// caller that must build its roots before it polls.
    #[inline]
    pub fn stop_requested(&self) -> bool {
        self.stw.load(Ordering::Relaxed)
    }

    /// The stops so far and the longest wait of a requester for the world to stop.
    pub fn stop_stats(&self) -> (u64, Duration) {
        let st = self.lock();
        (st.stops, st.max_stop)
    }

    /// A thread joins the heap. It waits for a running stop to end first.
    pub fn attach(self: &Arc<Self>) -> Attached {
        let mut st = self.wait_idle(self.lock());
        st.attached += 1;
        Attached {
            coord: Some(self.clone()),
        }
    }

    #[cold]
    fn stop_here(&self, roots: impl FnOnce() -> Vec<GcRef>) {
        // Outside the lock: the closure may read the coordinator, and a panic in it leaves the
        // lock free (the token's drop then leaves the heap).
        let roots = roots();
        let st = self.lock();
        match st.phase {
            Phase::Idle => {} // a stale `stw` read: the stop already ended, drop the roots
            Phase::Requested | Phase::Collecting => self.park(st, roots),
        }
    }

    /// Stop counting one attached thread. Only a requester waits for the count to fall, so the
    /// wake is made only while one waits: a detach or a leave outside a stop makes no syscall.
    fn leave(&self, mut st: MutexGuard<'_, State>) {
        st.attached -= 1;
        let wake = st.phase == Phase::Requested;
        drop(st);
        if wake {
            self.cv.notify_all();
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
}

impl Attached {
    fn coord(&self) -> &GcCoord {
        self.coord
            .as_deref()
            .expect("a live token holds its coordinator")
    }

    /// A safepoint. Cheap (one Relaxed load) when no stop is requested. Under a stop it hands in
    /// `roots()` and parks until the collection ends.
    #[inline]
    pub fn poll(&mut self, roots: impl FnOnce() -> Vec<GcRef>) {
        let c = self.coord();
        if c.stw.load(Ordering::Relaxed) {
            c.stop_here(roots);
        }
    }

    /// Go `entersyscall`: publish `roots` and stop counting this thread. The thread may then block
    /// on anything; it must not touch the heap until [`DetachTicket::reattach`].
    pub fn detach(mut self, roots: Vec<GcRef>) -> DetachTicket {
        let coord = self
            .coord
            .take()
            .expect("a live token holds its coordinator");
        let mut st = coord.lock();
        let id = st.next_ticket;
        let epoch = st.gc_epoch;
        st.next_ticket += 1;
        st.detached_roots.push((id, roots));
        // The roots and the leave in one hold: a requester never sees the thread uncounted
        // without its roots.
        coord.leave(st);
        DetachTicket {
            coord: Some(coord),
            id,
            epoch,
        }
    }

    /// Stop the world and run `collect` with every root. If another thread already requested a
    /// stop, this thread stops like a poller instead and `collect` does not run: one collection
    /// serves both (the run has one heap). `collect` runs with the lock released (see the module
    /// doc).
    pub fn request_collect(&mut self, own_roots: Vec<GcRef>, collect: impl FnOnce(&[GcRef])) {
        let c = self.coord();
        let mut st = c.lock();
        match st.phase {
            Phase::Requested | Phase::Collecting => return c.park(st, own_roots),
            Phase::Idle => {}
        }
        st.phase = Phase::Requested;
        c.stw.store(true, Ordering::Relaxed);
        let asked = Instant::now();
        while st.stopped + 1 < st.attached {
            st = c.wait(st);
        }
        st.phase = Phase::Collecting;
        st.stops += 1;
        st.max_stop = st.max_stop.max(asked.elapsed());
        let mut roots = own_roots;
        roots.append(&mut st.stopped_roots);
        for (_, r) in &st.detached_roots {
            roots.extend_from_slice(r);
        }
        drop(st);
        // Release the world even if `collect` panics, so no stopped thread waits forever.
        let _release = Release(c);
        collect(&roots);
    }
}

impl Drop for Attached {
    /// Leave: the thread stops counting, also during a stop (a waiting requester proceeds).
    fn drop(&mut self) {
        if let Some(c) = &self.coord {
            c.leave(c.lock());
        }
    }
}

impl DetachTicket {
    /// Go `exitsyscall`: wait until no stop is running, withdraw the published roots, count the
    /// thread again. The flag is whether a collection ran while the thread was detached.
    pub fn reattach(mut self) -> (Attached, bool) {
        let coord = self
            .coord
            .take()
            .expect("only reattach takes the coordinator");
        let mut st = coord.wait_idle(coord.lock());
        withdraw(&mut st, self.id);
        st.attached += 1;
        let collected = st.gc_epoch != self.epoch;
        drop(st);
        (Attached { coord: Some(coord) }, collected)
    }
}

impl Drop for DetachTicket {
    /// Leave while detached: withdraw the published roots.
    fn drop(&mut self) {
        if let Some(c) = &self.coord {
            withdraw(&mut c.lock(), self.id);
        }
    }
}

/// Remove the roots that the ticket `id` published.
fn withdraw(st: &mut State, id: u64) {
    let at = st.detached_roots.iter().position(|(t, _)| *t == id);
    st.detached_roots
        .swap_remove(at.expect("a live DetachTicket has published roots"));
}

/// Ends a stop: epoch + 1, `stw` cleared, every stopped thread released.
struct Release<'a>(&'a GcCoord);

impl Drop for Release<'_> {
    fn drop(&mut self) {
        let c = self.0;
        let mut st = c.lock();
        st.gc_epoch += 1;
        st.stopped = 0;
        // The requester drained them, and no thread can park after that: every other attached
        // thread is already parked, and attach and reattach wait for Idle.
        debug_assert!(st.stopped_roots.is_empty());
        st.stopped_roots.clear();
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
            let stop = Arc::new(AtomicBool::new(false));
            let roots_calls = Arc::new(AtomicUsize::new(0));

            let detacher = {
                let (c, barrier) = (c.clone(), barrier.clone());
                thread::spawn(move || {
                    let a = c.attach();
                    barrier.wait();
                    let ticket = a.detach(vec![GcRef(1)]);
                    release_rx.recv().unwrap();
                    let _a = ticket.reattach();
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
                    let mut a = c.attach();
                    barrier.wait();
                    while !stop.load(Ordering::SeqCst) {
                        a.poll(|| {
                            roots_calls.fetch_add(1, Ordering::SeqCst);
                            vec![GcRef(2)]
                        });
                        thread::yield_now();
                    }
                })
            };

            let mut me = c.attach();
            barrier.wait();
            // The detached thread stays blocked until after this returns: the `within` bound is
            // the check that the collector did not wait for it.
            let mut runs = Vec::new();
            me.request_collect(vec![GcRef(3)], |roots| runs.push(sorted(roots)));
            stop.store(true, Ordering::SeqCst);
            release_tx.send(()).unwrap();
            detacher.join().unwrap();
            poller.join().unwrap();

            assert_eq!(runs, vec![vec![1, 2, 3]]);
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
                    let a = c.attach();
                    let ticket = a.detach(vec![GcRef(7)]);
                    detached_tx.send(()).unwrap();
                    reattach_rx.recv().unwrap();
                    let _a = ticket.reattach();
                    returned_tx.send(finished.load(Ordering::SeqCst)).unwrap();
                })
            };
            detached_rx.recv().unwrap();

            let collector = {
                let (c, finished) = (c.clone(), finished.clone());
                thread::spawn(move || {
                    let mut me = c.attach();
                    let go_rx = Mutex::new(go_rx);
                    me.request_collect(vec![], |roots| {
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
                    let mut me = c.attach();
                    for _ in 0..ROUNDS {
                        barrier.wait();
                        me.request_collect(vec![GcRef(id)], |roots| {
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

    /// A released poller is running again even before it wakes, so a stop requested at once must
    /// not count it as stopped. Each collection must see the poller parked in that same stop.
    #[test]
    fn rerequest_waits_for_released_poller() {
        const ROUNDS: u64 = 2000;
        within(|| {
            let c = Arc::new(GcCoord::new());
            let barrier = Arc::new(Barrier::new(2));
            let running = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let poller = {
                let (c, barrier, running, stop) =
                    (c.clone(), barrier.clone(), running.clone(), stop.clone());
                thread::spawn(move || {
                    let mut a = c.attach();
                    barrier.wait();
                    while !stop.load(Ordering::SeqCst) {
                        running.store(false, Ordering::SeqCst);
                        a.poll(|| vec![GcRef(2)]);
                        running.store(true, Ordering::SeqCst);
                        thread::yield_now();
                    }
                })
            };
            let mut me = c.attach();
            barrier.wait();
            let mut bad = Vec::new();
            for round in 0..ROUNDS {
                me.request_collect(vec![GcRef(1)], |roots| {
                    let (roots, live) = (sorted(roots), running.load(Ordering::SeqCst));
                    if roots != [1, 2] || live {
                        bad.push((round, roots, live));
                    }
                });
            }
            stop.store(true, Ordering::SeqCst);
            poller.join().unwrap();
            assert!(
                bad.is_empty(),
                "collections ran while the poller ran: {bad:?}"
            );
            assert_eq!(c.epoch(), ROUNDS);
        });
    }

    /// A roots closure may read the coordinator: it runs before the poll takes the lock.
    #[test]
    fn roots_closure_may_call_epoch() {
        within(|| {
            let c = Arc::new(GcCoord::new());
            let barrier = Arc::new(Barrier::new(2));
            let stop = Arc::new(AtomicBool::new(false));
            let poller = {
                let (c, barrier, stop) = (c.clone(), barrier.clone(), stop.clone());
                thread::spawn(move || {
                    let mut a = c.attach();
                    barrier.wait();
                    while !stop.load(Ordering::SeqCst) {
                        a.poll(|| vec![GcRef(c.epoch() as u32 + 2)]);
                        thread::yield_now();
                    }
                })
            };
            let mut me = c.attach();
            barrier.wait();
            let mut runs = Vec::new();
            me.request_collect(vec![GcRef(1)], |roots| runs.push(sorted(roots)));
            stop.store(true, Ordering::SeqCst);
            poller.join().unwrap();
            assert_eq!(runs, vec![vec![1, 2]]);
        });
    }

    /// A thread that drops its token while a requester waits for it leaves the count: the
    /// collection proceeds without it.
    #[test]
    fn dropped_attached_does_not_block_a_collection() {
        within(|| {
            let c = Arc::new(GcCoord::new());
            let (ready_tx, ready_rx) = mpsc::channel::<()>();
            let leaver = {
                let c = c.clone();
                thread::spawn(move || {
                    let a = c.attach();
                    ready_tx.send(()).unwrap();
                    // Leave during the stop, never at a safepoint.
                    while !c.stw.load(Ordering::SeqCst) {
                        thread::yield_now();
                    }
                    drop(a);
                })
            };
            ready_rx.recv().unwrap();
            let mut me = c.attach();
            let mut runs = Vec::new();
            me.request_collect(vec![GcRef(1)], |roots| runs.push(sorted(roots)));
            leaver.join().unwrap();
            assert_eq!(runs, vec![vec![1]]);
            assert_eq!(c.epoch(), 1);
        });
    }

    /// A ticket dropped without a reattach withdraws its roots from the next collection.
    #[test]
    fn dropped_detach_ticket_releases_its_roots() {
        within(|| {
            let c = Arc::new(GcCoord::new());
            let mut me = c.attach();
            let ticket = c.attach().detach(vec![GcRef(9)]);
            let mut runs = Vec::new();
            me.request_collect(vec![GcRef(1)], |roots| runs.push(sorted(roots)));
            drop(ticket);
            me.request_collect(vec![GcRef(1)], |roots| runs.push(sorted(roots)));
            assert_eq!(runs, vec![vec![1, 9], vec![1]]);
        });
    }

    /// The stop stats count each collection, not each request: a requester that joins a running
    /// stop as a poller adds nothing. `stop_requested` reads the flag a safepoint checks.
    #[test]
    fn stop_stats_count_each_collection() {
        within(|| {
            let c = Arc::new(GcCoord::new());
            let mut me = c.attach();
            assert_eq!(c.stop_stats().0, 0);
            assert!(!c.stop_requested());
            let mut seen = Vec::new();
            me.request_collect(vec![], |_| seen.push(c.stop_stats().0));
            me.request_collect(vec![], |_| {});
            assert_eq!(seen, vec![1], "the stop counts before its collection runs");
            assert_eq!(c.stop_stats().0, 2);
            assert!(!c.stop_requested(), "the flag clears at release");
        });
    }
}
