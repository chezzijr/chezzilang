//! Heap slot storage: a segmented slab whose slots never move once allocated.
//!
//! A [`GcRef`]'s bits are `chunk << 12 | idx`: 4,096 slots per chunk, 20 bits of chunk index.
//! Chunks fill in order, so the bits ARE the dense slot number that `Heap`'s free list and
//! `marks`/`copied`/`viewed` bitsets index. A chunk never moves once allocated, so a `&Slot` stays
//! at one address while the slab grows (a `Vec<Slot>` moves every slot when it grows).
//!
//! A chunk's storage is allocated UNINITIALIZED and only the slots below [`Slab::len`] are written,
//! so a heap that holds one object touches one page of its 256 KiB chunk. Until every task shares
//! one heap, each spawned task owns a heap, and `tests/spawn_storm_memory.rs` gates the max RSS.
//!
//! The chunk table has two levels so it never moves and an empty heap costs ~2 KiB: a boxed top
//! array of 256 pointers to lazily allocated arrays of 4,096 chunk pointers. The pointers are
//! atomics, published with `Release` and read with `Acquire`, so a later step can share the table
//! between threads without a redesign. The slab has one owner today: `push` takes `&mut self`.

use super::heap::Obj;
use super::value::GcRef;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, Ordering};

/// One heap slot: the object, or a hole for swept/free slots. Exactly 64B (`Option<Obj>` niche-packs
/// `None` free). The GC mark bit is NOT here — it lives in `Heap::marks`, a dense parallel bitset,
/// so the `mark:bool` no longer pads the slot from 64B to 72B (the memory win). (Sweep still scans
/// every slot's `obj` to find garbage — the bitset does not avoid that; it saves the per-slot byte.)
#[derive(Debug)]
pub(super) struct Slot {
    pub(super) obj: Option<Obj>,
}

const CHUNK_BITS: u32 = 12;
/// Slots per chunk.
const CHUNK: usize = 1 << CHUNK_BITS;
/// Chunk pointers per second-level array.
const L2: usize = 4096;
/// Second-level arrays in the top array: `TOP * L2` chunks of `CHUNK` slots cover every `u32`.
const TOP: usize = 256;

/// One chunk's storage: `CHUNK` slots, of which the first `Slab::chunk_len` are initialized.
type ChunkPtr = *mut MaybeUninit<Slot>;
type Level2 = [AtomicPtr<MaybeUninit<Slot>>; L2];

/// The segmented slot store behind `Heap`. See the module doc.
pub(super) struct Slab {
    top: Box<[AtomicPtr<Level2>; TOP]>,
    /// Slots ever appended. Every slot below it is initialized (a swept slot holds `obj: None`).
    len: usize,
    /// The slab owns its `Slot`s: this keeps `Slab`'s `Send`/`Sync` equal to `Slot`'s.
    _owns: PhantomData<Slot>,
}

impl Default for Slab {
    fn default() -> Self {
        Slab {
            top: Box::new([const { AtomicPtr::new(null_mut()) }; TOP]),
            len: 0,
            _owns: PhantomData,
        }
    }
}

impl std::fmt::Debug for Slab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slab").field("len", &self.len).finish()
    }
}

impl Slab {
    /// Slots ever appended (live, free and hole alike).
    #[inline]
    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// The chunk pointer for chunk number `c`, or null if that chunk is not allocated.
    #[inline]
    fn chunk(&self, c: usize) -> ChunkPtr {
        let l2 = self.top[c / L2].load(Ordering::Acquire);
        if l2.is_null() {
            return null_mut();
        }
        // SAFETY: a non-null second-level pointer came from `Box::into_raw` in `push` and is freed
        // only by `drop`, which needs `&mut self`.
        unsafe { (*l2)[c % L2].load(Ordering::Acquire) }
    }

    /// How many slots of chunk `c` are initialized.
    #[inline]
    fn chunk_len(&self, c: usize) -> usize {
        (self.len - c * CHUNK).min(CHUNK)
    }

    /// A pointer to the initialized slot `i`. Panics when `i` was never appended, like a `Vec` index.
    #[inline]
    fn slot_ptr(&self, h: GcRef) -> *mut Slot {
        let i = h.0 as usize;
        assert!(
            i < self.len,
            "GcRef {i} out of range (slab len {})",
            self.len
        );
        let c = i >> CHUNK_BITS;
        // SAFETY: `i < len`, so the second-level array and chunk `c` are allocated (no null check on
        // this hot path) and slot `i % CHUNK` of it is initialized; a chunk is `CHUNK` slots long.
        unsafe {
            let l2 = self.top[c / L2].load(Ordering::Acquire);
            let chunk = (*l2)[c % L2].load(Ordering::Acquire);
            chunk.add(i % CHUNK).cast::<Slot>()
        }
    }

    #[inline]
    pub(super) fn get(&self, h: GcRef) -> &Slot {
        // SAFETY: `slot_ptr` returns an initialized slot that lives as long as `self`; `&self`
        // excludes every `&mut` to it.
        unsafe { &*self.slot_ptr(h) }
    }

    #[inline]
    pub(super) fn get_mut(&mut self, h: GcRef) -> &mut Slot {
        // SAFETY: as in `get`; `&mut self` makes this the only reference into the slab.
        unsafe { &mut *self.slot_ptr(h) }
    }

    /// Append `slot` and return its handle. Allocates a chunk (and a second-level array) on first
    /// use; neither ever moves.
    pub(super) fn push(&mut self, slot: Slot) -> GcRef {
        let i = self.len;
        assert!(i <= u32::MAX as usize, "heap slab full ({i} slots)");
        let (c, k) = (i >> CHUNK_BITS, i % CHUNK);
        let mut chunk = self.chunk(c);
        if chunk.is_null() {
            chunk = self.alloc_chunk(c);
        }
        // SAFETY: slot `k` of chunk `c` is inside the chunk and not yet initialized (`k` = its
        // `chunk_len`), so the write neither overwrites a live `Slot` nor leaks one.
        unsafe { chunk.add(k).write(MaybeUninit::new(slot)) };
        self.len = i + 1;
        GcRef(i as u32)
    }

    #[cold]
    fn alloc_chunk(&mut self, c: usize) -> ChunkPtr {
        let top = &self.top[c / L2];
        let mut l2 = top.load(Ordering::Acquire);
        if l2.is_null() {
            let fresh: Box<Level2> = (0..L2)
                .map(|_| AtomicPtr::new(null_mut()))
                .collect::<Box<[_]>>()
                .try_into()
                .unwrap_or_else(|_| unreachable!("L2 elements"));
            l2 = Box::into_raw(fresh);
            top.store(l2, Ordering::Release);
        }
        let chunk =
            Box::into_raw(Box::<[Slot]>::new_uninit_slice(CHUNK)).cast::<MaybeUninit<Slot>>();
        // SAFETY: `l2` is non-null and owned by this slab (see `chunk`).
        unsafe { (*l2)[c % L2].store(chunk, Ordering::Release) };
        chunk
    }

    /// Chunk `c`'s initialized slots, for chunk numbers below the chunk count.
    fn chunk_slice(&self, c: usize) -> &[Slot] {
        // SAFETY: every chunk below the chunk count is allocated, and its first `chunk_len(c)`
        // slots are initialized; `MaybeUninit<Slot>` has `Slot`'s layout.
        unsafe { std::slice::from_raw_parts(self.chunk(c).cast::<Slot>(), self.chunk_len(c)) }
    }

    /// Every initialized slot, in index order.
    pub(super) fn iter(&self) -> impl Iterator<Item = &Slot> {
        (0..self.len.div_ceil(CHUNK)).flat_map(move |c| self.chunk_slice(c).iter())
    }
}

impl Drop for Slab {
    fn drop(&mut self) {
        for c in 0..self.len.div_ceil(CHUNK) {
            let (chunk, n) = (self.chunk(c), self.chunk_len(c));
            // SAFETY: the first `n` slots of this allocated chunk are initialized and dropped once
            // here; the chunk came from `Box::<[MaybeUninit<Slot>]>` of `CHUNK` slots.
            unsafe {
                std::ptr::drop_in_place(std::ptr::slice_from_raw_parts_mut(
                    chunk.cast::<Slot>(),
                    n,
                ));
                drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                    chunk, CHUNK,
                )));
            }
        }
        for top in self.top.iter() {
            let l2 = top.load(Ordering::Acquire);
            if !l2.is_null() {
                // SAFETY: from `Box::into_raw` in `alloc_chunk`; freed once here.
                drop(unsafe { Box::from_raw(l2) });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::heap::Obj;
    use crate::vm::value::GcRef;

    fn big(slab: &Slab, h: GcRef) -> i64 {
        match slab.get(h).obj {
            Some(Obj::BigInt(n)) => n,
            ref other => panic!("slot {} holds {other:?}", h.0),
        }
    }

    #[test]
    fn slot_address_is_stable_across_growth() {
        let mut slab = Slab::default();
        let first = slab.push(Slot {
            obj: Some(Obj::BigInt(-1)),
        });
        let addr = slab.get(first) as *const Slot;
        let refs: Vec<GcRef> = (0..10_000)
            .map(|i| {
                slab.push(Slot {
                    obj: Some(Obj::BigInt(i)),
                })
            })
            .collect();
        assert_eq!(slab.get(first) as *const Slot, addr);
        assert_eq!(big(&slab, first), -1);
        for (i, h) in refs.iter().enumerate() {
            assert_eq!(big(&slab, *h), i as i64);
        }
        assert_eq!(slab.len(), 10_001);
    }

    #[test]
    fn ref_bits_cross_chunk_boundary() {
        let mut slab = Slab::default();
        let refs: Vec<GcRef> = (0..4097)
            .map(|i| {
                slab.push(Slot {
                    obj: Some(Obj::BigInt(i)),
                })
            })
            .collect();
        let (a, b) = (refs[4095], refs[4096]);
        assert_eq!((a.0 >> 12, a.0 & 0xfff), (0, 4095));
        assert_eq!((b.0 >> 12, b.0 & 0xfff), (1, 0));
        assert_eq!(big(&slab, a), 4095);
        assert_eq!(big(&slab, b), 4096);
        assert_eq!(slab.iter().count(), 4097);
    }
}
