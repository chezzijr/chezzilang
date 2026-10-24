//! VM: this thread's GC token — attach, the safepoint poll, the GC entry and the one blocking door.
//!
//! One token per OS thread that runs VM code ([`gc_coord`](super::gc_coord)'s rule), held by the
//! `Vm` that the thread drives (`Vm::attached`):
//! - the root `Vm` attaches in `Vm::run` and leaves when it drops;
//! - a worker shell attaches for the span of its worker loop (`Vm::worker_loop_held`) and leaves
//!   when the loop returns, or when the shell drops on a panic;
//! - a NESTED `Vm` on a thread that already drives one (an inline joiner shell) attaches only after
//!   the outer `Vm` detached: the outer runs the nested loop inside [`Vm::gc_blocking`]. A second
//!   token on one thread fails a debug assertion ([`ThreadToken`]).
//!
//! A `Vm` with no token (a test fixture that never ran `Vm::run`, a worker `Vm` that only rebuilds
//! a task's payload) polls nothing and blocks with no door. It collects alone only while its
//! handle is its heap core's only one: a sweep of a core that other handles use needs a stop.
//!
//! M1.7 — a task's payload `Vm` (`Vm::prepare_worker`) allocates into the run's heap on the
//! spawner's thread, under the spawner's token, and its values are in no root set until
//! `ReadyWorker::into_fiber` registers the fiber's context. It never collects (it has no token and
//! is not its core's only handle), and the spawner reaches no safepoint, collection or door in
//! between, so no stop can complete while those values are unrooted: the stop waits for the
//! spawner. [`PayloadInFlight`] checks the second half in debug builds.

use super::gc_coord::{Attached, DetachTicket};
use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

thread_local! {
    /// The GC tokens this OS thread holds: 0 or 1.
    static TOKENS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// The task payloads this thread rebuilt that are not yet in a registered context.
    static PAYLOADS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// A rebuilt task payload in flight on this thread (see the module doc): from the payload `Vm`'s
/// first allocation to `ReadyWorker::into_fiber`. While one lives, this thread must not stop,
/// collect or detach: its values are in no root set.
pub(super) struct PayloadInFlight(());

impl PayloadInFlight {
    pub(super) fn new() -> Self {
        PAYLOADS.with(|p| p.set(p.get() + 1));
        PayloadInFlight(())
    }
}

impl Drop for PayloadInFlight {
    fn drop(&mut self) {
        PAYLOADS.with(|p| p.set(p.get().saturating_sub(1)));
    }
}

/// The debug check of [`PayloadInFlight`]'s rule, at each place a thread with a token can let a
/// collection run.
#[inline]
fn debug_no_payload_in_flight() {
    debug_assert_eq!(
        PAYLOADS.with(|p| p.get()),
        0,
        "M1.7: a stop, a collection or a door while a task payload is in no root set"
    );
}

/// An [`Attached`] token of this OS thread. It counts the thread's tokens, so a second one fails a
/// debug assertion. `None` only inside [`ThreadToken::detach`].
pub(super) struct ThreadToken(Option<Attached>);

impl ThreadToken {
    fn new(a: Attached) -> Self {
        TOKENS.with(|t| {
            debug_assert_eq!(t.get(), 0, "M1.5: a second GC token on one OS thread");
            t.set(t.get() + 1);
        });
        ThreadToken(Some(a))
    }

    fn get(&mut self) -> &mut Attached {
        self.0.as_mut().expect("a live ThreadToken holds its token")
    }

    fn detach(mut self, roots: Vec<GcRef>) -> DetachTicket {
        let a = self.0.take().expect("a live ThreadToken holds its token");
        a.detach(roots)
    }
}

impl Drop for ThreadToken {
    fn drop(&mut self) {
        TOKENS.with(|t| t.set(t.get() - 1));
    }
}

/// `CHEZZI_GC_STRESS=1`: every instruction boundary requests a collection (`Vm::gc_stress`).
pub(super) fn gc_stress_env() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("CHEZZI_GC_STRESS").is_ok_and(|v| v == "1"))
}

impl Vm {
    /// Join this run's heap for the thread that drives this `Vm`. `true` iff this call attached;
    /// that caller leaves with [`Vm::gc_leave`].
    pub(super) fn gc_attach(&mut self) -> bool {
        if self.attached.is_some() {
            return false;
        }
        self.attached = Some(ThreadToken::new(self.gc_coord.attach()));
        true
    }

    /// Leave this run's heap: a stop no longer waits for this thread. The handle's `get_mut`
    /// record is settled first, while no sweep can run (`Heap`'s drop does not read it).
    pub(super) fn gc_leave(&mut self) {
        self.heap.settle_pending_mut();
        self.attached = None;
    }

    /// A safepoint: one Relaxed load of the run's stop flag when no stop is requested. Under a
    /// stop an attached thread hands in this `Vm`'s roots and parks until the collection ends.
    #[inline]
    pub(super) fn gc_poll(&mut self) {
        if self.gc_coord.stop_requested() {
            self.gc_park();
        }
    }

    /// The slow half of a safepoint. The poll parks only while a stop runs, and a stale `stw`
    /// read finds the stop just ended, so either way a collection ran: the handle zeroes its
    /// pacing counts ([`Heap::observe_collection`]).
    #[cold]
    #[inline(never)]
    fn gc_park(&mut self) {
        if let Some(mut t) = self.attached.take() {
            debug_no_payload_in_flight();
            let roots = self.gc_roots();
            t.get().poll(|| roots);
            self.attached = Some(t);
            self.heap.observe_collection();
        }
    }

    /// THE GC entry: stop the world, then collect the run's one heap from the union of its roots
    /// ([`Vm::collect_from`]). With no token, collect alone, and only on a core with no other
    /// handle (see the module doc).
    ///
    /// A request that finds another thread's stop running joins it as a poller: that collection
    /// swept the one heap, so it serves this request too (`sample_mem_cap` and the `--max-heap`
    /// check read the core's verdict of that sweep). The freed objects whose drop can make a
    /// syscall are dropped here, after the world restarts (`Heap::take_graveyard`).
    pub(super) fn gc_collect(&mut self) {
        let Some(mut t) = self.attached.take() else {
            if self.heap.is_sole_handle() {
                self.collect();
                drop(self.heap.take_graveyard());
            }
            return;
        };
        debug_no_payload_in_flight();
        let mut ran = false;
        let r = catch_unwind(AssertUnwindSafe(|| {
            let roots = self.gc_roots();
            t.get().request_collect(roots, |union| {
                self.collect_from(union.to_vec());
                ran = true;
            });
        }));
        self.attached = Some(t);
        if !ran {
            self.heap.observe_collection();
        }
        drop(self.heap.take_graveyard());
        if let Err(p) = r {
            resume_unwind(p);
        }
    }

    /// THE blocking door. Every wait this thread can make while it holds a token runs as `f`
    /// (`tests/wait_site_guard.rs` checks it): the thread publishes its roots plus `pins`, detaches
    /// (Go `entersyscall`), runs `f`, and reattaches (Go `exitsyscall`), which waits out a running
    /// collection. With no token it only runs `f`.
    ///
    /// `f` must not allocate on or write to this `Vm`'s heap (it may read the published roots).
    /// The rule does not cover another `Vm` that `f` drives: a nursery join's `f` runs a joiner
    /// shell's worker loop, and that shell attaches its own token before it runs code. `f` must
    /// release every lock it took before it returns: the reattach can wait for a stop, and a stop
    /// waits for each attached thread, also for one blocked on that lock. A panic in `f`
    /// reattaches before it unwinds on.
    pub(super) fn gc_blocking<R>(&mut self, pins: &[GcRef], f: impl FnOnce(&mut Vm) -> R) -> R {
        let Some(t) = self.attached.take() else {
            return f(self);
        };
        debug_no_payload_in_flight();
        let mut roots = self.gc_roots();
        roots.extend_from_slice(pins);
        let ticket = t.detach(roots);
        let r = catch_unwind(AssertUnwindSafe(|| f(self)));
        let (a, collected) = ticket.reattach();
        self.attached = Some(ThreadToken::new(a));
        if collected {
            self.heap.observe_collection();
        }
        match r {
            Ok(v) => v,
            Err(p) => resume_unwind(p),
        }
    }
}
