//! TICKET-205 (was TICKET-141, W14-14) — the ONE process-wide FIFO runner gate, [`RUNNERS`].
//!
//! A party that cannot leave its OS thread at a slice end — a CPU loop inside a native re-entry
//! callback (its caller's loop state lives on the host stack), an `Executor` job, a gated nursery
//! body — cannot be parked, so the THREAD hands its runner slot over and takes one back in arrival
//! order. That keeps `--threads=N` at N runners (Go's `GOMAXPROCS=1`, DEC-059).
//!
//! The gate state is per OS thread, not per `Vm`: `GATED` (permanent once set), `HOLDS` (returns
//! the permit when the thread exits) and `SLOT` (the queue ticket a WAKER reserved for the thread).
//!
//! INVARIANTS:
//! - A gated thread runs Chezzi code, picks a fiber and draws from the seeded RNG only while it
//!   holds a permit, and releases it around every wait in place (a demote, a guard wait, an inline
//!   nursery join, an idle sleep, ...). A gated waiter holds a fiber it already dequeued that no
//!   other worker can steal, so a permit holder waiting in place for that fiber would hang both.
//! - The waker queues the woken thread ([`reserve`]), so wake order is the waker's order and not
//!   the order the OS runs the woken threads in.
//! - A reserved ticket lives only while its thread is listed as a waiter. Whoever unlists the
//!   slot withdraws the ticket ([`cancel`]): a ticket nobody takes sits at the queue head and hangs
//!   every gated thread of the process.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

/// TICKET-205 — the ONE runner gate of the process. Every thread that cannot leave its OS thread
/// (a native callback, an Executor job, a gated nursery body) hands its runner slot over here.
pub(super) static RUNNERS: WidthGate = WidthGate::new();

thread_local! {
    /// This OS thread runs Chezzi code only while it holds a permit of [`RUNNERS`].
    static GATED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static HOLDS: HoldGuard = const { HoldGuard(std::cell::Cell::new(false)) };
}

/// Returns the permit when a gated thread exits while holding one.
struct HoldGuard(std::cell::Cell<bool>);
impl Drop for HoldGuard {
    fn drop(&mut self) {
        if self.0.get() {
            RUNNERS.release();
        }
    }
}

pub(super) fn gated() -> bool {
    GATED.with(|g| g.get())
}
pub(super) fn holds() -> bool {
    HOLDS.with(|h| h.0.get())
}
/// Turn this thread's implicit runner slot into an explicit permit that it holds. Permanent.
pub(super) fn convert() {
    if !gated() {
        GATED.with(|g| g.set(true));
        HOLDS.with(|h| h.0.set(true));
    }
}
/// A thread spawned to take over a gated thread's slot starts gated, with no permit.
pub(super) fn born_gated(on: bool) {
    if on {
        GATED.with(|g| g.set(true));
    }
}
pub(super) fn release() {
    if holds() {
        HOLDS.with(|h| h.0.set(false));
        RUNNERS.release();
    }
}
/// One in-place wait with the permit released.
pub(super) fn released<R>(wait: impl FnOnce() -> R) -> R {
    release();
    let r = wait();
    acquire();
    r
}
pub(super) fn acquire() {
    if gated() && !holds() {
        with_slot(|s| RUNNERS.acquire_slot(s));
        HOLDS.with(|h| h.0.set(true));
    }
}

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
/// A slot made by the thread that SPAWNS a gated thread, so it can queue the child for the permit
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
/// The waker queues the woken thread for the permit, in the waker's own order.
pub(super) fn reserve(slot: &Slot) {
    RUNNERS.reserve(slot);
}

struct GateSt {
    free: usize,
    queue: VecDeque<u64>,
    next_ticket: u64,
}

pub(super) struct WidthGate {
    st: Mutex<GateSt>,
    cv: Condvar,
    waiting: AtomicUsize,
}

impl WidthGate {
    pub(super) const fn new() -> Self {
        WidthGate {
            st: Mutex::new(GateSt {
                free: 0,
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

    /// Return a permit. Wakes waiters only when the queue is non-empty (DEC-028: no wake per
    /// preemption).
    pub(super) fn release(&self) {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        st.free += 1;
        let wake = !st.queue.is_empty();
        drop(st);
        if wake {
            self.cv.notify_all();
        }
    }

    /// Take a permit, waiting in FIFO order. `notify_all` is required on the release side: a
    /// `Condvar` cannot target the queue head, and only gate waiters sleep on this `cv`.
    #[cfg(test)]
    pub(super) fn acquire(&self) {
        self.acquire_slot(&Slot::new());
    }

    fn reserve(&self, slot: &Slot) {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        if slot.ticket.load(Ordering::Relaxed) == 0 {
            let t = st.next_ticket;
            st.next_ticket += 1;
            st.queue.push_back(t);
            slot.ticket.store(t + 1, Ordering::Relaxed);
            self.waiting.fetch_add(1, Ordering::Relaxed);
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
            self.waiting.fetch_sub(1, Ordering::Relaxed);
            drop(st);
            self.cv.notify_all();
        }
    }

    fn acquire_slot(&self, slot: &Slot) {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        let me = match slot.ticket.load(Ordering::Relaxed) {
            0 => {
                let t = st.next_ticket;
                st.next_ticket += 1;
                st.queue.push_back(t);
                slot.ticket.store(t + 1, Ordering::Relaxed);
                self.waiting.fetch_add(1, Ordering::Relaxed);
                t
            }
            t => t - 1,
        };
        while !(st.free > 0 && st.queue.front() == Some(&me)) {
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        st.queue.pop_front();
        slot.ticket.store(0, Ordering::Relaxed);
        st.free -= 1;
        self.waiting.fetch_sub(1, Ordering::Relaxed);
        let more = st.free > 0 && !st.queue.is_empty();
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
    fn release_then_acquire_does_not_block() {
        let g = WidthGate::new();
        g.release();
        g.acquire();
        assert_eq!(g.waiting(), 0);
    }

    #[test]
    fn acquire_blocks_until_a_release() {
        let g = Arc::new(WidthGate::new());
        let g2 = Arc::clone(&g);
        let h = std::thread::spawn(move || g2.acquire());
        spin_until_waiting(&g, 1);
        g.release();
        h.join().expect("acquirer panicked");
        assert_eq!(g.waiting(), 0);
    }

    #[test]
    fn waiters_are_served_in_arrival_order() {
        let g = Arc::new(WidthGate::new());
        let (tx, rx) = mpsc::channel();
        let mut hs = Vec::new();
        for id in 0..2 {
            let g2 = Arc::clone(&g);
            let tx2 = tx.clone();
            hs.push(std::thread::spawn(move || {
                g2.acquire();
                tx2.send(id).unwrap();
            }));
            spin_until_waiting(&g, id + 1);
        }
        g.release();
        assert_eq!(
            rx.recv().unwrap(),
            0,
            "the first arrival must be served first"
        );
        g.release();
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
        let g = Arc::new(WidthGate::new());
        let slot_a = Arc::new(Slot::new());
        g.reserve(&slot_a);
        let (tx, rx) = mpsc::channel();
        let (g2, tx2) = (Arc::clone(&g), tx.clone());
        let hb = std::thread::spawn(move || {
            g2.acquire_slot(&Slot::new());
            tx2.send(B).unwrap();
        });
        spin_until_waiting(&g, 2);
        let (g2, slot2) = (Arc::clone(&g), Arc::clone(&slot_a));
        let ha = std::thread::spawn(move || {
            g2.acquire_slot(&slot2);
            tx.send(A).unwrap();
        });
        // One permit at a time: two at once would let both acquire and race their sends.
        g.release();
        assert_eq!(
            rx.recv().unwrap(),
            A,
            "the reserved ticket must be served before the later arrival"
        );
        g.release();
        assert_eq!(rx.recv().unwrap(), B);
        ha.join().expect("acquirer A panicked");
        hb.join().expect("acquirer B panicked");
        assert_eq!(g.waiting(), 0);
    }
}
