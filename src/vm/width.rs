//! TICKET-205 (was TICKET-141, W14-14) — the ONE process-wide FIFO runner gate, [`RUNNERS`].
//!
//! TICKET-230 — the gate is the single owner of CPU width: `--threads=N` caps the threads that run
//! Chezzi code at N process-wide, main included (Go's `GOMAXPROCS`, DEC-059). A party that cannot
//! leave its OS thread at a slice end — a CPU loop inside a native re-entry callback, an `Executor`
//! job, a nursery body — cannot be parked, so the THREAD hands its permit over and takes one back in
//! arrival order.
//!
//! The gate state is per OS thread, not per `Vm`: `HOLDS` (returns the permit when the thread
//! exits) and `SLOT` (the queue ticket a WAKER reserved for the thread).
//!
//! INVARIANTS:
//! - Every thread that runs Chezzi code is gated from birth. It runs Chezzi code, picks a fiber and
//!   draws from the seeded RNG only while it holds a permit, and releases it around every wait in
//!   place (a demote, a guard wait, an inline nursery join, an idle sleep, ...). A waiter holds a
//!   fiber it already dequeued that no other worker can steal, so a permit holder waiting in place
//!   for that fiber would hang both.
//! - The gate's capacity is `worker_count()`, read at every acquire. There is no other width budget.
//! - The waker queues the woken thread ([`reserve`]) only when no permit is free beyond the queued
//!   tickets, so while the budget is full wake order is the waker's order and not the order the OS
//!   runs the woken threads in. A ticket at queue position i is granted while `held + i < cap`.
//! - A reserved ticket lives only while its thread is listed as a waiter. Whoever unlists the
//!   slot withdraws the ticket ([`cancel`]): a ticket nobody takes sits at the queue head and hangs
//!   every thread of the process.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

/// TICKET-205 / TICKET-230 — the ONE runner gate of the process: `worker_count()` permits.
pub(super) static RUNNERS: WidthGate = WidthGate::new(super::worker_count);

thread_local! {
    static HOLDS: HoldGuard = const { HoldGuard(std::cell::Cell::new(false)) };
}

/// Returns the permit when a thread exits while holding one.
struct HoldGuard(std::cell::Cell<bool>);
impl Drop for HoldGuard {
    fn drop(&mut self) {
        if self.0.get() {
            probe_leave();
            RUNNERS.release();
        }
    }
}

pub(super) fn holds() -> bool {
    HOLDS.with(|h| h.0.get())
}
pub(super) fn release() {
    if holds() {
        HOLDS.with(|h| h.0.set(false));
        probe_leave();
        RUNNERS.release();
    }
}
/// TICKET-230 — take a FREE permit without waiting: `false` when none is free beyond the queued
/// tickets, or when a waker already queued this thread (its ticket keeps its place).
pub(super) fn try_acquire() -> bool {
    if holds() {
        return true;
    }
    let free = with_slot(|s| {
        s.ticket.load(Ordering::Relaxed) == 0
            && RUNNERS.try_take(RUNNERS.waiting.load(Ordering::SeqCst))
    });
    if free {
        HOLDS.with(|h| h.0.set(true));
        probe_enter();
    }
    free
}
/// One in-place wait with the permit released.
pub(super) fn released<R>(wait: impl FnOnce() -> R) -> R {
    release();
    let r = wait();
    acquire();
    r
}
pub(super) fn acquire() {
    if !holds() {
        with_slot(|s| RUNNERS.acquire_slot(s));
        HOLDS.with(|h| h.0.set(true));
        probe_enter();
    }
}

/// TICKET-230 — the permit a run's main thread takes in `Vm::run`. Its drop returns the permit
/// only if this guard took it: a thread that already held one keeps it.
pub(super) struct RunPermit(bool);
impl RunPermit {
    pub(super) fn take() -> Self {
        let fresh = !holds();
        acquire();
        RunPermit(fresh)
    }
}
impl Drop for RunPermit {
    fn drop(&mut self) {
        if self.0 {
            release();
        }
    }
}

/// TICKET-230 — test-only count of one run's permit holders: `now` at this moment, `peak` the most
/// at once. Per run, so concurrently running lib tests do not pollute each other's counts.
#[cfg(test)]
#[derive(Default, Debug)]
pub(super) struct WidthProbe {
    pub(super) now: AtomicUsize,
    pub(super) peak: AtomicUsize,
}
#[cfg(test)]
thread_local! {
    /// The probe of the run this thread works for; the next acquire counts in it.
    static PROBE: std::cell::RefCell<Option<std::sync::Arc<WidthProbe>>> =
        const { std::cell::RefCell::new(None) };
    /// The probe the held permit was counted in; the release uncounts it there.
    static HELD_PROBE: std::cell::RefCell<Option<std::sync::Arc<WidthProbe>>> =
        const { std::cell::RefCell::new(None) };
}
/// Point this thread's next permit at `p`. Returns the previous probe.
#[cfg(test)]
pub(super) fn set_probe(
    p: Option<std::sync::Arc<WidthProbe>>,
) -> Option<std::sync::Arc<WidthProbe>> {
    PROBE.with(|c| c.replace(p))
}
#[cfg(test)]
fn probe_enter() {
    let p = PROBE.try_with(|c| c.borrow().clone()).ok().flatten();
    if let Some(p) = p {
        let now = p.now.fetch_add(1, Ordering::SeqCst) + 1;
        p.peak.fetch_max(now, Ordering::SeqCst);
        let _ = HELD_PROBE.try_with(|c| *c.borrow_mut() = Some(p));
    }
}
#[cfg(test)]
fn probe_leave() {
    if let Ok(Some(p)) = HELD_PROBE.try_with(|c| c.borrow_mut().take()) {
        p.now.fetch_sub(1, Ordering::SeqCst);
    }
}
#[cfg(not(test))]
fn probe_enter() {}
#[cfg(not(test))]
fn probe_leave() {}

/// One per OS thread: the queue ticket a WAKER reserved for it (`0` = none, else ticket + 1).
/// Written only under the gate lock.
#[derive(Debug)]
pub(super) struct Slot {
    ticket: std::sync::atomic::AtomicU64,
}
impl Slot {
    const fn new() -> Self {
        Slot {
            ticket: std::sync::atomic::AtomicU64::new(0),
        }
    }
}
/// Withdraws a reserved ticket nobody will take when its thread exits.
struct OwnSlot(std::sync::Arc<Slot>);
impl Drop for OwnSlot {
    fn drop(&mut self) {
        RUNNERS.cancel(&self.0);
    }
}
thread_local! {
    static SLOT: std::cell::OnceCell<OwnSlot> = const { std::cell::OnceCell::new() };
}
fn with_slot<R>(f: impl FnOnce(&std::sync::Arc<Slot>) -> R) -> R {
    SLOT.with(|s| {
        f(&s.get_or_init(|| OwnSlot(std::sync::Arc::new(Slot::new())))
            .0)
    })
}
pub(super) fn my_slot() -> std::sync::Arc<Slot> {
    with_slot(std::sync::Arc::clone)
}
/// A slot made by the thread that SPAWNS a runner thread, so it can queue the child for the permit
/// before the child first runs. The child takes it with [`adopt`].
pub(super) fn new_slot() -> std::sync::Arc<Slot> {
    std::sync::Arc::new(Slot::new())
}
pub(super) fn adopt(slot: std::sync::Arc<Slot>) {
    SLOT.with(|s| {
        let _ = s.set(OwnSlot(slot));
    });
}

/// Withdraw a ticket reserved for a slot whose thread will not take it: the thread was refused by
/// the OS, or it stopped being listed as a waiter (a waker reserves only for a LISTED slot, under
/// the list's lock, so no ticket can appear after the unlisting).
pub(super) fn cancel(slot: &Slot) {
    RUNNERS.cancel(slot);
}
/// Has a waker queued this thread for the permit?
pub(super) fn reserved() -> bool {
    with_slot(|s| s.ticket.load(Ordering::Relaxed) != 0)
}
/// TICKET-230 — no permit is free beyond the queued tickets.
pub(super) fn full() -> bool {
    RUNNERS.held.load(Ordering::SeqCst) + RUNNERS.waiting.load(Ordering::SeqCst) >= (RUNNERS.cap)()
}
/// The waker queues the woken thread for the permit, in the waker's own order, when the gate is
/// [`full`].
pub(super) fn reserve(slot: &Slot) {
    RUNNERS.reserve(slot);
}

struct GateSt {
    queue: VecDeque<u64>,
    next_ticket: u64,
}

pub(super) struct WidthGate {
    /// The capacity, read at every acquire: a changed worker count needs no resize.
    cap: fn() -> usize,
    /// TICKET-230 — permits held now: raised by CAS (never past `cap`), lowered by `release`.
    /// Atomic so the free-permit path takes no lock.
    held: AtomicUsize,
    st: Mutex<GateSt>,
    cv: Condvar,
    /// Queued tickets, equal to `st.queue.len()`; written under `st`.
    waiting: AtomicUsize,
}

impl WidthGate {
    pub(super) const fn new(cap: fn() -> usize) -> Self {
        WidthGate {
            cap,
            held: AtomicUsize::new(0),
            st: Mutex::new(GateSt {
                queue: VecDeque::new(),
                next_ticket: 0,
            }),
            cv: Condvar::new(),
            waiting: AtomicUsize::new(0),
        }
    }
    /// How many threads are queued for a permit (lock-free; a hint for the preemption safepoint).
    pub(super) fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Relaxed)
    }

    /// Take a permit if one is free beyond the first `ahead` queued tickets.
    fn try_take(&self, ahead: usize) -> bool {
        let cap = (self.cap)();
        let mut h = self.held.load(Ordering::SeqCst);
        while h + ahead < cap {
            match self
                .held
                .compare_exchange_weak(h, h + 1, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return true,
                Err(x) => h = x,
            }
        }
        false
    }

    /// Return a permit. Wakes waiters only when the queue is non-empty (DEC-028: no wake per
    /// preemption). The lock orders the wake after a waiter's last check; the free-permit path
    /// takes no lock.
    pub(super) fn release(&self) {
        let prev = self.held.fetch_sub(1, Ordering::SeqCst);
        debug_assert!(prev > 0, "TICKET-230: release without a held permit");
        if self.waiting.load(Ordering::SeqCst) > 0 {
            drop(self.st.lock().unwrap_or_else(|e| e.into_inner()));
            self.cv.notify_all();
        }
    }

    /// Take a permit, waiting in FIFO order. `notify_all` is required on the release side: a
    /// `Condvar` cannot target the queue head, and only gate waiters sleep on this `cv`.
    #[cfg(test)]
    pub(super) fn acquire(&self) {
        self.acquire_slot(&Slot::new());
    }

    /// TICKET-230 — queues a ticket only when no permit is free beyond the queued tickets: wake
    /// order matters only when the budget is full. At cap 1 a waker that holds the permit always
    /// queues, so T=1 order is unchanged.
    fn reserve(&self, slot: &Slot) {
        let cap = (self.cap)();
        if self.held.load(Ordering::SeqCst) + self.waiting.load(Ordering::SeqCst) < cap {
            return;
        }
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        if slot.ticket.load(Ordering::Relaxed) == 0
            && self.held.load(Ordering::SeqCst) + st.queue.len() >= cap
        {
            let t = st.next_ticket;
            st.next_ticket += 1;
            st.queue.push_back(t);
            slot.ticket.store(t + 1, Ordering::Relaxed);
            self.waiting.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn cancel(&self, slot: &Slot) {
        if slot.ticket.load(Ordering::Relaxed) == 0 {
            return;
        }
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        let t = slot.ticket.swap(0, Ordering::Relaxed);
        if t != 0 {
            st.queue.retain(|q| *q != t - 1);
            self.waiting.fetch_sub(1, Ordering::SeqCst);
            drop(st);
            self.cv.notify_all();
        }
    }

    /// TICKET-230 — a ticket at queue position i is granted while `held + i < cap`, not only at the
    /// queue head: a head-only grant made every thread wait behind a ticket reserved for a thread
    /// the OS had not run yet (`send_one_channel` T=0 ran 29.2x base). At cap 1 the two rules agree.
    fn acquire_slot(&self, slot: &Slot) {
        if slot.ticket.load(Ordering::Relaxed) == 0
            && self.try_take(self.waiting.load(Ordering::SeqCst))
        {
            return;
        }
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        let me = match slot.ticket.load(Ordering::Relaxed) {
            0 => {
                let t = st.next_ticket;
                st.next_ticket += 1;
                st.queue.push_back(t);
                slot.ticket.store(t + 1, Ordering::Relaxed);
                self.waiting.fetch_add(1, Ordering::SeqCst);
                t
            }
            t => t - 1,
        };
        let pos = loop {
            let pos = st
                .queue
                .iter()
                .position(|q| *q == me)
                .expect("a waiting thread's ticket is queued");
            if self.try_take(pos) {
                break pos;
            }
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        };
        st.queue.remove(pos);
        slot.ticket.store(0, Ordering::Relaxed);
        self.waiting.fetch_sub(1, Ordering::SeqCst);
        let more = !st.queue.is_empty() && self.held.load(Ordering::SeqCst) < (self.cap)();
        drop(st);
        if more {
            self.cv.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, mpsc};

    fn spin_until_waiting(g: &WidthGate, n: usize) {
        while g.waiting() != n {
            std::thread::yield_now();
        }
    }

    #[test]
    fn acquire_on_a_free_gate_does_not_block() {
        let g = WidthGate::new(|| 1);
        g.acquire();
        assert_eq!(g.waiting(), 0);
        g.release();
    }

    #[test]
    fn acquire_blocks_until_a_release() {
        let g = Arc::new(WidthGate::new(|| 1));
        g.acquire();
        let g2 = Arc::clone(&g);
        let h = std::thread::spawn(move || {
            g2.acquire();
            g2.release();
        });
        spin_until_waiting(&g, 1);
        g.release();
        h.join().expect("acquirer panicked");
        assert_eq!(g.waiting(), 0);
    }

    #[test]
    fn waiters_are_served_in_arrival_order() {
        let g = Arc::new(WidthGate::new(|| 1));
        g.acquire();
        let (tx, rx) = mpsc::channel();
        let mut hs = Vec::new();
        for id in 0..2 {
            let g2 = Arc::clone(&g);
            let tx2 = tx.clone();
            hs.push(std::thread::spawn(move || {
                g2.acquire();
                tx2.send(id).unwrap();
                g2.release();
            }));
            spin_until_waiting(&g, id + 1);
        }
        g.release();
        assert_eq!(
            rx.recv().unwrap(),
            0,
            "the first arrival must be served first"
        );
        assert_eq!(rx.recv().unwrap(), 1);
        for h in hs {
            h.join().expect("acquirer panicked");
        }
    }

    /// TICKET-205: the WAKER fixes a woken thread's place. A reserves (as its waker would), B
    /// arrives and queues, and only then does A's own thread reach the gate: A is still first.
    #[test]
    fn a_reserved_ticket_keeps_its_place_ahead_of_a_later_acquirer() {
        const A: usize = 0;
        const B: usize = 1;
        let g = Arc::new(WidthGate::new(|| 1));
        g.acquire();
        let slot_a = Arc::new(Slot::new());
        g.reserve(&slot_a);
        let (tx, rx) = mpsc::channel();
        let (g2, tx2) = (Arc::clone(&g), tx.clone());
        let hb = std::thread::spawn(move || {
            g2.acquire_slot(&Slot::new());
            tx2.send(B).unwrap();
            g2.release();
        });
        spin_until_waiting(&g, 2);
        let (g2, slot2) = (Arc::clone(&g), Arc::clone(&slot_a));
        let ha = std::thread::spawn(move || {
            g2.acquire_slot(&slot2);
            tx.send(A).unwrap();
            g2.release();
        });
        // One permit: each holder releases after its own send, so the sends cannot race.
        g.release();
        assert_eq!(
            rx.recv().unwrap(),
            A,
            "the reserved ticket must be served before the later arrival"
        );
        assert_eq!(rx.recv().unwrap(), B);
        ha.join().expect("acquirer A panicked");
        hb.join().expect("acquirer B panicked");
        assert_eq!(g.waiting(), 0);
    }

    /// TICKET-230: the gate grants `cap` permits at once, and a third holder waits for a release.
    #[test]
    fn a_gate_grants_up_to_its_cap_and_no_more() {
        let g = Arc::new(WidthGate::new(|| 2));
        g.acquire();
        g.acquire();
        assert_eq!(g.waiting(), 0);
        let (tx, rx) = mpsc::channel();
        let g2 = Arc::clone(&g);
        let h = std::thread::spawn(move || {
            g2.acquire();
            tx.send(()).unwrap();
            g2.release();
        });
        spin_until_waiting(&g, 1);
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "a third acquire must wait while two permits are held"
        );
        g.release();
        rx.recv()
            .expect("the third acquire must proceed after a release");
        h.join().expect("acquirer panicked");
        g.release();
        assert_eq!(g.waiting(), 0);
    }

    /// TICKET-230: a waker queues the wakee only when no permit is free beyond the queued tickets.
    #[test]
    fn a_reserve_with_a_free_permit_queues_nothing() {
        let g = WidthGate::new(|| 2);
        g.reserve(&Slot::new());
        assert_eq!(
            g.waiting(),
            0,
            "a reserve must not queue a ticket while a permit is free"
        );
    }

    /// TICKET-230: a ticket at queue position i is granted while `held + i < cap`. A head-only grant
    /// made a thread wait behind a ticket reserved for a thread the OS had not run yet.
    #[test]
    fn a_ticket_behind_an_untaken_ticket_proceeds_while_permits_are_free() {
        let g = Arc::new(WidthGate::new(|| 2));
        g.acquire();
        g.acquire();
        let a = Slot::new();
        g.reserve(&a);
        let (tx, rx) = mpsc::channel();
        let g2 = Arc::clone(&g);
        let h = std::thread::spawn(move || {
            g2.acquire();
            tx.send(()).unwrap();
            g2.release();
        });
        spin_until_waiting(&g, 2);
        g.release();
        g.release();
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(2)).is_ok(),
            "a ticket behind an untaken ticket must proceed while permits are free"
        );
        assert_eq!(g.waiting(), 1, "the untaken ticket stays queued");
        g.cancel(&a);
        h.join().expect("acquirer panicked");
        assert_eq!(g.waiting(), 0);
    }
}
