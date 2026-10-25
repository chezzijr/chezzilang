//! VM: GC roots: the one root walker (`RootView`) and the run's registry of boxed fiber contexts.
//!
//! Every place that roots an execution context (the live `Vm` fields, a generator's parked host,
//! a suspended generator, a parked fiber) builds a [`RootView`] and calls [`RootView::walk`], so a
//! root field is added in one place for all of them.
//!
//! The registry lists the M:N fiber contexts of one run, so the run's collector
//! (`Vm::collect_from`) roots the PARKED fibers too, not only the one live in a `Vm`.
//! `ReadyWorker::into_fiber` builds each fiber context through [`RootRegistry::register`]; it
//! leaves the registry when its [`CtxBox`] drops or [`CtxBox::into_inner`] takes it out.
//!
//! M1.7: a `Shared`/`RwShared` box context is NOT listed, by one rule: the collector reaches it
//! through the box object that holds it (`core::SharedState::roots_into`), as Go reaches a value
//! through its struct, so a dropped box's state dies in the same collection as the box. The caller
//! a box call parks (`BoxFrame::caller`) is rooted by the `Vm` that runs the call
//! (`Vm::gc_roots`).

use super::fxhash::FxHashMap;
use super::*;
use std::ptr::NonNull;

/// The root-holding fields of one execution context, borrowed. A context that lacks a field
/// leaves it empty (`RootView::default()`).
#[derive(Default)]
pub(super) struct RootView<'a> {
    /// The whole operand stack: every frame's local slots and any in-flight temporaries.
    pub(super) stack: &'a [Value],
    /// Each frame's home module, backing closure and pending `defer` calls.
    pub(super) frames: &'a [CallFrame],
    /// Pending `spawn` tasks (C4): their callee/receiver/args are roots until the task runs.
    pub(super) nurseries: &'a [Vec<QueuedTask>],
    /// A not-yet-started fiber's task, launched on its first schedule.
    pub(super) start: Option<&'a PendingCall>,
    /// The module namespace objects (the run's, M3.1): every context roots them.
    pub(super) module_objs: Option<&'a super::module_table::ModuleTable>,
    /// Live executors (C5 / A2): their queued work must survive to the program-exit auto-drain
    /// even when no in-program handle remains.
    pub(super) executors: &'a [GcRef],
    /// The heap-keyed caches, `None` for a context that has none (a generator's).
    pub(super) caches: Option<HeapCaches<'a>>,
}

/// The heap-keyed caches of a context whose `GcRef`s are roots.
pub(super) struct HeapCaches<'a> {
    pub(super) str_intern: &'a FxHashMap<usize, GcRef>,
}

impl RootView<'_> {
    /// THE root walk of one execution context: append every root it holds to `out`.
    pub(super) fn walk(&self, out: &mut Vec<GcRef>) {
        // `child_gcref` roots boxed floats (Float tag) too, not just true `Obj`s.
        out.extend(self.stack.iter().filter_map(|v| v.child_gcref()));
        for f in self.frames {
            out.push(f.home);
            if let Some(c) = f.closure {
                out.push(c);
            }
            for d in &f.deferred {
                out.extend(d.roots());
            }
        }
        for nursery in self.nurseries {
            for task in nursery {
                out.extend(task.roots());
            }
        }
        if let Some(task) = self.start {
            out.extend(task.roots());
        }
        if let Some(modules) = self.module_objs {
            out.extend(modules.iter());
        }
        out.extend(self.executors.iter().copied());
        let Some(c) = &self.caches else {
            return;
        };
        // M19 Phase 3 — interned `ConstStr` handles are roots: they're cached for reuse across pushes
        // of the same op, so they must never be swept out from under a later push. Heap-keyed, so this
        // roots the cache for the heap this context indexes.
        out.extend(c.str_intern.values().copied());
    }
}

impl Vm {
    /// The live per-execution fields as one [`RootView`].
    pub(super) fn root_view(&self) -> RootView<'_> {
        RootView {
            stack: &self.stack,
            frames: &self.frames,
            nurseries: &self.nurseries,
            start: None,
            module_objs: Some(&self.module_objs),
            executors: &self.executors,
            caches: Some(HeapCaches {
                str_intern: &self.str_intern,
            }),
        }
    }
}

impl GenCtx {
    /// A parked generator context (a suspended generator, or the host a running one parked) holds
    /// frames and a stack only.
    pub(super) fn root_view(&self) -> RootView<'_> {
        RootView {
            stack: &self.stack,
            frames: &self.frames,
            ..RootView::default()
        }
    }
}

impl FiberCtx {
    /// A fiber context as one [`RootView`]: the fields `Vm::swap_ctx` moves, plus `start`.
    pub(super) fn root_view(&self) -> RootView<'_> {
        RootView {
            stack: &self.stack,
            frames: &self.frames,
            nurseries: &self.nurseries,
            start: self.start.as_ref(),
            module_objs: Some(&self.module_objs),
            executors: &self.executors,
            caches: Some(HeapCaches {
                str_intern: &self.str_intern,
            }),
        }
    }

    /// The GC roots this parked fiber context holds (tests read them).
    #[cfg(test)]
    pub(super) fn roots(&self) -> Vec<GcRef> {
        let mut out = Vec::new();
        self.root_view().walk(&mut out);
        out
    }
}

/// A registered context's address. Only [`RootRegistry::roots`] reads through it.
struct CtxPtr(NonNull<FiberCtx>);

// SAFETY: sending the address to another thread is sound because the only read through it,
// `RootRegistry::roots`, holds the exclusivity argument written there.
unsafe impl Send for CtxPtr {}

// A walk and an unattached thread (the deadlock reporter, the netpoller's atomic store) may each
// hold `&FiberCtx` at once, so shared access must be sound too.
const _: fn() = || {
    fn s<T: Send + Sync>() {}
    s::<FiberCtx>();
};

#[derive(Default)]
struct Slots {
    ctxs: Vec<Option<CtxPtr>>,
    free: Vec<usize>,
}

/// The run's registry of fiber contexts. Shared (not copied) with every worker `Vm` by
/// [`Vm::spawn_worker`], and per-run for the same reason [`Vm::exec_registry`] is.
#[derive(Clone, Default)]
pub(super) struct RootRegistry(Arc<Mutex<Slots>>);

impl RootRegistry {
    /// Box `ctx` and list it here until the returned [`CtxBox`] drops.
    pub(super) fn register(&self, ctx: FiberCtx) -> CtxBox {
        let ptr = NonNull::from(Box::leak(Box::new(ctx)));
        let mut s = super::heap_core::lock(&self.0);
        let slot = match s.free.pop() {
            Some(i) => {
                s.ctxs[i] = Some(CtxPtr(ptr));
                i
            }
            None => {
                s.ctxs.push(Some(CtxPtr(ptr)));
                s.ctxs.len() - 1
            }
        };
        drop(s);
        CtxBox {
            ptr,
            reg: Some((self.clone(), slot)),
        }
    }

    /// Every root of every registered context.
    ///
    /// Contract: no `&mut FiberCtx` to a registered context exists during the walk. It holds
    /// because:
    /// 1. `CtxBox` owns its context through a raw pointer, so a `&mut Fiber` is no `&mut FiberCtx`.
    ///    One exists only for the length of a `CtxBox::deref_mut` borrow.
    /// 2. Only a fiber's runner thread (attached to the GC) calls `deref_mut` on a registered
    ///    context: `run_one_fiber`'s `swap_task` and `start` take. A thread that is not attached
    ///    writes no registered context: the netpoller sets `poll_timed_out` (an atomic) through
    ///    `&`, and `record_deadlocked` takes the context out with [`CtxBox::into_inner`] first.
    /// 3. The run's collector (`Vm::collect_from`) calls this only inside a stop of the world
    ///    (`GcCoord`, `vm/gc_coord.rs`), where every attached thread is parked at a safepoint,
    ///    between two such borrows, or from a `Vm` whose heap handle is its core's only one (no
    ///    other thread runs the run's code).
    ///
    /// A new mutable use of a parked context from a thread that is not attached breaks point 2.
    pub(super) fn roots(&self) -> Vec<GcRef> {
        let s = super::heap_core::lock(&self.0);
        let mut out = Vec::new();
        for p in s.ctxs.iter().flatten() {
            // SAFETY: a registered pointer came from `Box::leak` in `register` and stays valid until
            // `CtxBox::leave` removes it under this same lock (before the box is freed or taken), so
            // it is live here. The contract above rules out a concurrent `&mut`.
            unsafe { p.0.as_ref() }.root_view().walk(&mut out);
        }
        out
    }

    /// How many slots the registry holds, listed or free.
    #[cfg(test)]
    fn slot_count(&self) -> usize {
        super::heap_core::lock(&self.0).ctxs.len()
    }
}

/// An owned, boxed `FiberCtx` that never moves, listed in a [`RootRegistry`] while it lives. It
/// holds the box as a raw pointer rather than a `Box`: moving a `Box` asserts unique access and
/// would invalidate the registry's alias, while a raw pointer keeps both valid.
pub(super) struct CtxBox {
    ptr: NonNull<FiberCtx>,
    /// The registry and slot this context is listed at; `None` for an unlisted test fixture.
    reg: Option<(RootRegistry, usize)>,
}

// SAFETY: `CtxBox` owns its `FiberCtx` exactly as a `Box<FiberCtx>` does, and `FiberCtx: Send`
// (asserted above). The registry's alias is covered by `RootRegistry::roots`.
unsafe impl Send for CtxBox {}

impl CtxBox {
    /// A boxed context listed in no registry.
    #[cfg(test)]
    pub(super) fn unlisted(ctx: FiberCtx) -> Self {
        CtxBox {
            ptr: NonNull::from(Box::leak(Box::new(ctx))),
            reg: None,
        }
    }

    /// Leave the registry, if listed. Runs BEFORE the box is freed or taken, so a walk never reads
    /// a context it no longer owns.
    fn leave(&mut self) {
        if let Some((reg, slot)) = self.reg.take() {
            let mut s = super::heap_core::lock(&reg.0);
            s.ctxs[slot] = None;
            s.free.push(slot);
        }
    }

    /// Take the context out of its box. It leaves the registry first, so any thread may then
    /// change it.
    pub(super) fn into_inner(mut self) -> FiberCtx {
        self.leave();
        let ptr = self.ptr;
        // `reg` is `None` now, so forgetting `self` leaks nothing; the box is freed below.
        std::mem::forget(self);
        // SAFETY: `ptr` came from `Box::leak`, is no longer listed, and `forget` above skips the
        // `drop` that would free it a second time.
        *unsafe { Box::from_raw(ptr.as_ptr()) }
    }
}

#[cfg(test)]
impl Default for CtxBox {
    fn default() -> Self {
        CtxBox::unlisted(FiberCtx::default())
    }
}

impl std::ops::Deref for CtxBox {
    type Target = FiberCtx;
    fn deref(&self) -> &FiberCtx {
        // SAFETY: `ptr` came from `Box::leak` and is freed only in `drop`; `&self` gives shared
        // access, and the registry reads only under its contract.
        unsafe { self.ptr.as_ref() }
    }
}

impl std::ops::DerefMut for CtxBox {
    fn deref_mut(&mut self) -> &mut FiberCtx {
        // SAFETY: as in `deref`; `&mut self` is the one owner's exclusive access.
        unsafe { self.ptr.as_mut() }
    }
}

impl Drop for CtxBox {
    fn drop(&mut self) {
        self.leave();
        // SAFETY: `ptr` came from `Box::leak`, is no longer listed, and is freed exactly once here.
        drop(unsafe { Box::from_raw(self.ptr.as_ptr()) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(roots: Vec<GcRef>) -> Vec<u32> {
        let mut v: Vec<u32> = roots.into_iter().map(|h| h.0).collect();
        v.sort_unstable();
        v
    }

    /// Three registered contexts with distinct roots: the walker sees all three sets; drop one and
    /// it sees the other two; drop the rest and it sees nothing.
    #[test]
    fn registry_lists_live_ctxs_and_forgets_dropped() {
        let reg = RootRegistry::default();
        let ctx = |n: u32| FiberCtx {
            module_objs: Arc::new(super::super::module_table::ModuleTable::of(&[
                GcRef(n),
                GcRef(n + 1),
            ])),
            ..FiberCtx::default()
        };
        let a = reg.register(ctx(10));
        let b = reg.register(ctx(20));
        let c = reg.register(ctx(30));
        assert_eq!(sorted(reg.roots()), [10, 11, 20, 21, 30, 31]);
        drop(b);
        assert_eq!(sorted(reg.roots()), [10, 11, 30, 31]);
        // The slot `b` freed is reused (the slot count stays 3), and it lists only the new context.
        let d = reg.register(ctx(40));
        assert_eq!(
            reg.slot_count(),
            3,
            "a register after a drop reuses the freed slot"
        );
        assert_eq!(sorted(reg.roots()), [10, 11, 30, 31, 40, 41]);
        drop((a, c, d));
        assert!(reg.roots().is_empty());
    }

    /// A context taken out of its box leaves the registry first, so a thread that is not attached to
    /// the GC can then change it while a walk runs.
    #[test]
    fn into_inner_leaves_the_registry() {
        let reg = RootRegistry::default();
        let b = reg.register(FiberCtx {
            module_objs: Arc::new(super::super::module_table::ModuleTable::of(&[GcRef(7)])),
            ..FiberCtx::default()
        });
        let ctx = b.into_inner();
        assert!(
            reg.roots().is_empty(),
            "the taken context is no longer listed"
        );
        assert_eq!(ctx.module_objs.iter().collect::<Vec<_>>(), vec![GcRef(7)]);
    }
}
