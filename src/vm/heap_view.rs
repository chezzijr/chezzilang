//! TICKET-213 view state: which objects one context's cached module snapshot reaches, and the stamp
//! that tells the snapshot is still current.
//!
//! A [`ViewState`] belongs to ONE execution context (a fiber, a box context, a shell), not to a heap:
//! every handle of a run shares one heap core (M1.7), so the core's id cannot name a view. The live
//! context's view sits on its `Vm`'s heap handle (`Heap::view`), because `Heap::write`, the sole
//! write door, moves the epoch. `Vm::swap_ctx` swaps it with the parked context's
//! (`FiberCtx::view`), like the module table it describes.
//!
//! A snapshot is stamped with `(id, epoch)` at its build and is current exactly while both still
//! match the live view: `id` is fresh per view, never reused, so a snapshot built in another
//! context never matches; `epoch` moves at a new view and at each `&mut` to a viewed object.

use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide source for [`ViewState::id`]: one fresh id per view, never reused.
static NEXT_VIEW_ID: AtomicU64 = AtomicU64::new(1);

/// One context's view: see the module doc.
#[derive(Debug)]
pub(super) struct ViewState {
    id: u64,
    epoch: u64,
    /// One bit per heap slot: the object is reached by the snapshot of this view.
    viewed: Vec<u64>,
    /// Some bit of `viewed` is set: a view that never marked an aggregate stays at one predictable
    /// branch in `write`.
    any: bool,
}

impl Default for ViewState {
    fn default() -> Self {
        ViewState {
            id: NEXT_VIEW_ID.fetch_add(1, Ordering::Relaxed),
            epoch: 0,
            viewed: Vec::new(),
            any: false,
        }
    }
}

impl ViewState {
    /// The stamp a snapshot of this view carries (see the module doc).
    #[inline]
    pub(super) fn stamp(&self) -> (u64, u64) {
        (self.id, self.epoch)
    }

    #[inline]
    pub(super) fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Start marking a new view: forget the previous view's objects and move the epoch.
    pub(super) fn begin(&mut self) {
        self.viewed.fill(0);
        self.any = false;
        self.epoch += 1;
    }

    /// Mark slot `i`; `false` when it was already marked in this view.
    pub(super) fn set(&mut self, i: usize) -> bool {
        if self.viewed.len() <= i >> 6 {
            self.viewed.resize((i >> 6) + 1, 0);
        }
        let bit = 1u64 << (i & 63);
        let fresh = self.viewed[i >> 6] & bit == 0;
        self.viewed[i >> 6] |= bit;
        self.any = true;
        fresh
    }

    /// The `write` hook: a write to a viewed slot `i` moves the epoch.
    #[inline]
    pub(super) fn touch(&mut self, i: usize) {
        if self.any && self.is_set(i) {
            self.epoch += 1;
        }
    }

    #[inline]
    fn is_set(&self, i: usize) -> bool {
        self.viewed
            .get(i >> 6)
            .is_some_and(|w| (w >> (i & 63)) & 1 == 1)
    }

    /// Clear slot `i`'s bit: a reused slot must not inherit a stale mark. A slot another context
    /// reuses can keep this view's stale bit; it costs that view at most a spurious rebuild.
    #[inline]
    pub(super) fn clear(&mut self, i: usize) {
        if self.any
            && let Some(w) = self.viewed.get_mut(i >> 6)
        {
            *w &= !(1u64 << (i & 63));
        }
    }
}
