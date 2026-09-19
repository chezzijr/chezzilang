//! TICKET-141 (W14-14) — the per-`MnSched` FIFO width gate.
//!
//! A CPU loop inside a native re-entry callback cannot be parked (its caller's loop state lives on
//! the host stack), so the THREAD hands its runner slot to a replacement and takes one back in
//! arrival order. That keeps `--threads=N` at N runners (Go's `GOMAXPROCS=1`, DEC-059).
//!
//! INVARIANT: a gated `Vm` runs Chezzi code only while it holds a permit, and releases it around
//! every wait in place (a demote, a guard wait, an inline nursery join, ...). A gated waiter holds a
//! fiber it already dequeued that no other worker can steal, so a permit holder waiting in place
//! for that fiber would hang both.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

#[derive(Default)]
struct GateSt {
    free: usize,
    queue: VecDeque<u64>,
    next_ticket: u64,
}

#[derive(Default)]
pub(super) struct WidthGate {
    st: Mutex<GateSt>,
    cv: Condvar,
    waiting: AtomicUsize,
}

impl WidthGate {
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
    pub(super) fn acquire(&self) {
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        let me = st.next_ticket;
        st.next_ticket += 1;
        st.queue.push_back(me);
        self.waiting.fetch_add(1, Ordering::Relaxed);
        while !(st.free > 0 && st.queue.front() == Some(&me)) {
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        st.queue.pop_front();
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
        let g = WidthGate::default();
        g.release();
        g.acquire();
        assert_eq!(g.waiting(), 0);
    }

    #[test]
    fn acquire_blocks_until_a_release() {
        let g = Arc::new(WidthGate::default());
        let g2 = Arc::clone(&g);
        let h = std::thread::spawn(move || g2.acquire());
        spin_until_waiting(&g, 1);
        g.release();
        h.join().expect("acquirer panicked");
        assert_eq!(g.waiting(), 0);
    }

    #[test]
    fn waiters_are_served_in_arrival_order() {
        let g = Arc::new(WidthGate::default());
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
}
