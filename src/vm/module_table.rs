//! The run's one module table: each module's namespace object, by module index (M3.1).
//!
//! Every `Vm` and every parked context of a run holds this one table (`Vm::spawn_worker`), so a
//! task started before a later module initialized still reads that module once it has. The size
//! is the program's module count, fixed before any module runs; `Vm::run_module` fills slot `i`
//! before module `i`'s body runs, and a slot is written once. A slot is an atomic word: the fill
//! stores with Release and a read loads with Acquire, so a thread that reads a module's `GcRef`
//! sees the module object it names (the M3.0b publication rule).

use super::value::GcRef;
use std::sync::atomic::{AtomicU32, Ordering};

/// The marker of a slot whose module has not started its init.
const EMPTY: u32 = u32::MAX;

/// See the module doc.
#[derive(Debug, Default)]
pub(super) struct ModuleTable {
    slots: Box<[AtomicU32]>,
}

impl ModuleTable {
    /// A table for `n` modules, every slot empty.
    pub(super) fn new(n: usize) -> Self {
        ModuleTable {
            slots: (0..n).map(|_| AtomicU32::new(EMPTY)).collect(),
        }
    }

    /// Module `i`'s object, or `None` when module `i` has not started its init (or is out of
    /// range).
    #[inline]
    pub(super) fn get(&self, i: usize) -> Option<GcRef> {
        let h = self.slots.get(i)?.load(Ordering::Acquire);
        (h != EMPTY).then_some(GcRef(h))
    }

    /// Fill slot `i` with module `i`'s object, before the module's body runs.
    pub(super) fn set(&self, i: usize, h: GcRef) {
        debug_assert_eq!(
            self.slots[i].load(Ordering::Relaxed),
            EMPTY,
            "a module slot is written once"
        );
        self.slots[i].store(h.0, Ordering::Release);
    }

    /// Every module object that has started its init, in module order (GC roots).
    pub(super) fn iter(&self) -> impl Iterator<Item = GcRef> + '_ {
        (0..self.slots.len()).filter_map(|i| self.get(i))
    }

    /// The index of the module whose object is `h`.
    pub(super) fn index_of(&self, h: GcRef) -> Option<usize> {
        (0..self.slots.len()).find(|&i| self.get(i) == Some(h))
    }

    /// A table already holding `refs` (unit tests that build a context by hand).
    #[cfg(test)]
    pub(super) fn of(refs: &[GcRef]) -> Self {
        let t = ModuleTable::new(refs.len());
        for (i, &h) in refs.iter().enumerate() {
            t.set(i, h);
        }
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_reads_empty_until_it_is_filled() {
        let t = ModuleTable::new(3);
        assert_eq!(t.get(1), None);
        assert_eq!(t.get(9), None, "out of range reads empty");
        t.set(1, GcRef(7));
        assert_eq!(t.get(1), Some(GcRef(7)));
        assert_eq!(t.iter().collect::<Vec<_>>(), vec![GcRef(7)]);
        assert_eq!(t.index_of(GcRef(7)), Some(1));
        assert_eq!(t.index_of(GcRef(8)), None);
    }
}
