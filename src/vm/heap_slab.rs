//! Heap slot storage: a segmented slab whose slots never move once allocated.
//!
//! A [`GcRef`]'s bits are the dense slot number that `Heap`'s free list and `marks`/`copied`/
//! `viewed` bitsets index. The slab splits that number into geometric segments: segment 0 holds
//! slots `[0, 4)` and segment `k >= 1` holds `[4 << (k-1), 4 << k)`, so 31 segments cover every
//! `u32`. A segment never moves once allocated, so a `&Slot` stays at one address while the slab
//! grows (a `Vec<Slot>` moves every slot when it grows).
//!
//! The per-heap cost is small on purpose: until every task shares one heap, each spawned task
//! owns a heap, and `tests/spawn_storm_memory.rs` gates the max RSS. An empty slab allocates
//! nothing (the segment table is an inline array). A heap with one object allocates one 256 B
//! segment, the first capacity a `Vec<Slot>` takes. A segment is allocated UNINITIALIZED, and only
//! the slots below `Slab.len` are written. Each new segment doubles the capacity, as `Vec` growth
//! does: a large segment is one large allocation (huge pages apply to it), and the slab reserves at
//! most 2x `len` slots once `len` is past the first segment.
//!
//! The table entries are atomics, published with `Release` and read with `Acquire`, so a later
//! step can share the slab between threads without a redesign. The slab has one owner today:
//! `push` takes `&mut self`.

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

/// log2 of the slots in segment 0.
const FIRST_BITS: u32 = 2;
/// Segments: 0 for `[0, 4)`, then one per power of two up to `[1 << 31, 1 << 32)`.
const SEGS: usize = 33 - FIRST_BITS as usize;

/// `(segment, offset)` of dense slot `i`.
#[inline]
fn locate(i: usize) -> (usize, usize) {
    let k = (usize::BITS - (i >> FIRST_BITS).leading_zeros()) as usize;
    (k, i - seg_start(k))
}

/// The first dense slot of segment `k`: 0, 4, 8, 16, ...
#[inline]
fn seg_start(k: usize) -> usize {
    ((1usize << FIRST_BITS) >> 1 << k) & !((1usize << FIRST_BITS) - 1)
}

/// The slots in segment `k`: 4 for segment 0 and 1, then doubling.
#[inline]
fn seg_cap(k: usize) -> usize {
    seg_start(k).max(1 << FIRST_BITS)
}

/// The segmented slot store behind `Heap`. See the module doc.
pub(super) struct Slab {
    /// Segment `k`'s storage (`seg_cap(k)` slots), or null before slot `seg_start(k)` exists.
    segs: [AtomicPtr<MaybeUninit<Slot>>; SEGS],
    /// Slots ever appended. Every slot below it is initialized (a swept slot holds `obj: None`).
    len: usize,
    /// The slab owns its `Slot`s: this keeps `Slab`'s `Send`/`Sync` equal to `Slot`'s.
    _owns: PhantomData<Slot>,
}

impl Default for Slab {
    fn default() -> Self {
        Slab {
            segs: [const { AtomicPtr::new(null_mut()) }; SEGS],
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
    /// How many slots of segment `k` are initialized, for a segment below the segment count.
    #[inline]
    fn seg_len(&self, k: usize) -> usize {
        (self.len - seg_start(k)).min(seg_cap(k))
    }

    /// The number of segments that hold at least one slot.
    #[inline]
    fn seg_count(&self) -> usize {
        if self.len == 0 {
            0
        } else {
            locate(self.len - 1).0 + 1
        }
    }

    /// A pointer to the initialized slot `h`. Panics when `h` was never appended, like a `Vec` index.
    #[inline]
    fn slot_ptr(&self, h: GcRef) -> *mut Slot {
        let i = h.0 as usize;
        assert!(
            i < self.len,
            "GcRef {i} out of range (slab len {})",
            self.len
        );
        let (k, off) = locate(i);
        // SAFETY: `i < len <= 1 << 32`, so `k < SEGS`, segment `k` is allocated and its slot
        // `off < seg_cap(k)` is initialized.
        unsafe {
            let seg = self.segs.get_unchecked(k).load(Ordering::Acquire);
            seg.add(off).cast::<Slot>()
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

    /// Append `slot` and return its handle. Allocates the next segment when `slot` is the first
    /// slot of it; a segment never moves.
    pub(super) fn push(&mut self, slot: Slot) -> GcRef {
        let i = self.len;
        assert!(i <= u32::MAX as usize, "heap slab full ({i} slots)");
        let (k, off) = locate(i);
        let mut seg = self.segs[k].load(Ordering::Acquire);
        if seg.is_null() {
            seg = Box::into_raw(Box::<[Slot]>::new_uninit_slice(seg_cap(k))).cast();
            self.segs[k].store(seg, Ordering::Release);
        }
        // SAFETY: slot `off` of segment `k` is inside the segment and not yet initialized (`off` is
        // its `seg_len`), so the write neither overwrites a live `Slot` nor leaks one.
        unsafe { seg.add(off).write(MaybeUninit::new(slot)) };
        self.len = i + 1;
        GcRef(i as u32)
    }

    /// Segment `k`'s initialized slots as a raw slice, for a segment below the segment count.
    #[inline]
    fn seg_raw(&self, k: usize) -> *mut [Slot] {
        let seg = self.segs[k].load(Ordering::Acquire).cast::<Slot>();
        std::ptr::slice_from_raw_parts_mut(seg, self.seg_len(k))
    }

    /// Every initialized slot, in index order.
    pub(super) fn iter(&self) -> impl Iterator<Item = &Slot> {
        // SAFETY: every segment below the segment count is allocated and its first `seg_len(k)`
        // slots are initialized; `&self` excludes every `&mut` to them.
        (0..self.seg_count()).flat_map(move |k| unsafe { &*self.seg_raw(k) }.iter())
    }

    /// Every segment's initialized slots with the dense number of its first slot, in index order.
    pub(super) fn segments_mut(&mut self) -> impl Iterator<Item = (usize, &mut [Slot])> {
        let this = &*self;
        // SAFETY: as in `iter`; the segments are disjoint and `&mut self` is held for the
        // iterator's life, so each `&mut [Slot]` is the only reference to its slots.
        (0..this.seg_count()).map(move |k| (seg_start(k), unsafe { &mut *this.seg_raw(k) }))
    }
}

impl Drop for Slab {
    fn drop(&mut self) {
        for k in 0..self.seg_count() {
            let live = self.seg_raw(k);
            let seg = self.segs[k].load(Ordering::Acquire);
            // SAFETY: `live` is this segment's initialized slots, dropped once here; the segment
            // came from `Box::<[MaybeUninit<Slot>]>` of `seg_cap(k)` slots.
            unsafe {
                std::ptr::drop_in_place(live);
                drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                    seg,
                    seg_cap(k),
                )));
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
        assert_eq!(slab.iter().count(), 10_001);
    }

    #[test]
    fn segment_layout_is_geometric() {
        // Segment 0 holds 4 slots; segment k >= 1 holds [4 << (k-1), 4 << k).
        assert_eq!(locate(0), (0, 0));
        assert_eq!(locate(3), (0, 3));
        assert_eq!(locate(4), (1, 0));
        assert_eq!(locate(7), (1, 3));
        assert_eq!(locate(8), (2, 0));
        assert_eq!(locate(4095), (10, 2047));
        assert_eq!(locate(4096), (11, 0));
        assert_eq!(locate(u32::MAX as usize), (SEGS - 1, (1 << 31) - 1));
        for k in 1..SEGS {
            assert_eq!(
                seg_start(k),
                seg_start(k - 1) + seg_cap(k - 1),
                "segment {k} follows {}",
                k - 1
            );
        }
    }

    #[test]
    fn ref_bits_cross_segment_boundary() {
        let mut slab = Slab::default();
        let refs: Vec<GcRef> = (0..4097)
            .map(|i| {
                slab.push(Slot {
                    obj: Some(Obj::BigInt(i)),
                })
            })
            .collect();
        for i in [3usize, 4, 7, 8, 63, 64, 4095, 4096] {
            assert_eq!(refs[i].0 as usize, i, "the bits are the dense index");
            assert_eq!(big(&slab, refs[i]), i as i64);
        }
        assert_eq!(slab.iter().count(), 4097);
        let seen: Vec<i64> = slab
            .segments_mut()
            .flat_map(|(start, seg)| {
                seg.iter()
                    .enumerate()
                    .map(move |(k, s)| match s.obj {
                        Some(Obj::BigInt(n)) => {
                            assert_eq!(n as usize, start + k);
                            n
                        }
                        ref other => panic!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(seen, (0..4097).collect::<Vec<i64>>());
    }

    /// Drop must drop every initialized slot exactly once, and no slot past `len`. An
    /// `Obj::AtomicInt` holds an `Arc`, so its strong count counts the drops; `ByteArray`s own heap
    /// memory and swept holes (`obj: None`) sit between them. (Miri needs nightly; this box has none.)
    #[test]
    fn drop_frees_each_slot_once() {
        use crate::vm::core::AtomicIntCore;
        use std::sync::Arc;
        let core = Arc::new(AtomicIntCore {
            v: std::sync::atomic::AtomicI64::new(0),
        });
        let mut slab = Slab::default();
        let n = 1000; // segments 0 to 7 full, part of segment 8
        for i in 0..n {
            let obj = match i % 3 {
                0 => Some(Obj::AtomicInt(Arc::clone(&core))),
                1 => Some(Obj::ByteArray(vec![7; 100 + i])),
                _ => None,
            };
            slab.push(Slot { obj });
        }
        let held = (0..n).filter(|i| i % 3 == 0).count();
        assert_eq!(Arc::strong_count(&core), 1 + held);
        drop(slab);
        assert_eq!(Arc::strong_count(&core), 1);
    }
}
