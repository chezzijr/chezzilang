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

use super::gc_coord::{Attached, DetachTicket};
use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

thread_local! {
    /// The GC tokens this OS thread holds: 0 or 1.
    static TOKENS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
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

    /// Leave this run's heap: a stop no longer waits for this thread.
    pub(super) fn gc_leave(&mut self) {
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

    #[cold]
    #[inline(never)]
    fn gc_park(&mut self) {
        if let Some(mut t) = self.attached.take() {
            let roots = self.gc_roots();
            t.get().poll(|| roots);
            self.attached = Some(t);
        }
    }

    /// THE GC entry: stop the world, then collect. With no token, collect alone, and only on a
    /// core with no other handle (see the module doc).
    ///
    /// A request that finds another thread's stop running joins it as a poller, and that
    /// collection does not sweep this heap: until M1.7 each fiber's heap is its own. So the entry
    /// requests again after the joined stop ends, until a stop runs this `Vm`'s own collection
    /// (`sample_mem_cap` and the `--max-heap` check read that sweep). A fresh request, and not a
    /// sweep with the world running, keeps the M1.7 shape: every collection is a stop.
    pub(super) fn gc_collect(&mut self) {
        let Some(mut t) = self.attached.take() else {
            if self.heap.is_sole_handle() {
                self.collect();
            }
            return;
        };
        let r = catch_unwind(AssertUnwindSafe(|| {
            let mut swept = false;
            while !swept {
                let roots = self.gc_roots();
                // M1.7: collect the run's one heap from the union of the roots. Until then each
                // fiber has its own heap, the union holds refs into other heaps, and this collects
                // only its own.
                t.get().request_collect(roots, |_union| {
                    self.collect();
                    swept = true;
                });
            }
        }));
        self.attached = Some(t);
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
        let mut roots = self.gc_roots();
        roots.extend_from_slice(pins);
        let ticket = t.detach(roots);
        let r = catch_unwind(AssertUnwindSafe(|| f(self)));
        self.attached = Some(ThreadToken::new(ticket.reattach()));
        match r {
            Ok(v) => v,
            Err(p) => resume_unwind(p),
        }
    }
}
