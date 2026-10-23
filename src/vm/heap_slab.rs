//! Heap slot storage: a segmented slab whose slots never move once allocated.
//!
//! A [`GcRef`]'s bits are the dense slot number that `HeapCore`'s free list and `marks`/`copied`
//! bitsets, and `Heap`'s `viewed` bitset, index. The slab splits that number into geometric segments: segment 0 holds
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
//! The slab is shared by every handle of one `HeapCore` (`vm/heap_core.rs`), so it works through
//! `&self`: the table entries and `len` are atomics, published with `Release` and read with
//! `Acquire` (a slot lookup reads its table entry `Relaxed` after an `Acquire` load of `len`). An
//! append needs the [`SlabAppend`] token, which the core keeps under its allocation lock, so
//! appends are serialized. A slot is read and written in place through `&self`: keeping
//! two handles off one slot is the caller's duty (see [`Slab::slot_mut`]).
//!
//! [`SlotBits`] is a bitset with one bit per slot, segmented like the slab, for the core's mark and
//! copy bits. The bits of segments 0 to 4 (slots `[0, 64)`) are one inline word, so a heap of fewer
//! than 64 slots allocates nothing for its bits.

use super::heap::Obj;
use super::value::GcRef;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ops::Range;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

/// One heap slot: the object, or a hole for swept/free slots. Exactly 64B (`Option<Obj>` niche-packs
/// `None` free). The GC mark bit is NOT here — it lives in `HeapCore::marks`, a dense parallel
/// bitset, so the `mark:bool` no longer pads the slot from 64B to 72B (the memory win). (Sweep still
/// scans every slot's `obj` to find garbage — the bitset does not avoid that; it saves the per-slot
/// byte.)
#[derive(Debug)]
pub(super) struct Slot {
    pub(super) obj: Option<Obj>,
}

/// log2 of the slots in segment 0.
const FIRST_BITS: u32 = 2;
/// Segments: 0 for `[0, 4)`, then one per power of two up to `[1 << 31, 1 << 32)`.
const SEGS: usize = 33 - FIRST_BITS as usize;

/// `(segment, offset)` of dense slot `i`. `i | 3` is never 0, so `ilog2` needs no zero case:
/// segment 0 is `ilog2` 1 (slots 0 to 3), and segment `k >= 1` is `ilog2` `k + 1`.
#[inline]
fn locate(i: usize) -> (usize, usize) {
    let k = (i | ((1 << FIRST_BITS) - 1)).ilog2() as usize + 1 - FIRST_BITS as usize;
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

/// The segmented slot store behind `HeapCore`. See the module doc.
pub(super) struct Slab {
    /// Segment `k`'s storage (`seg_cap(k)` slots) moved back by `seg_start(k)` slots, so dense
    /// slot `i` of segment `k` is at `segs[k].wrapping_add(i)` (no offset to compute). Null
    /// before slot `seg_start(k)` exists. [`seg_base`](Slab::seg_base) gives the storage's address.
    segs: [AtomicPtr<MaybeUninit<Slot>>; SEGS],
    /// Slots ever appended. Every slot below it is initialized (a swept slot holds `obj: None`).
    /// Written only by [`push`](Slab::push), under the [`SlabAppend`] token.
    len: AtomicUsize,
    /// The slab owns its `Slot`s: this keeps `Slab`'s `Send`/`Sync` equal to `Slot`'s.
    _owns: PhantomData<Slot>,
}

/// The right to append to a slab. [`Slab::new`] makes exactly one per slab, and `HeapCore` keeps it
/// under its allocation lock, so `&mut SlabAppend` proves that no other append runs.
#[derive(Debug)]
pub(super) struct SlabAppend {
    _only_slab_new: (),
}

impl std::fmt::Debug for Slab {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Slab").field("len", &self.len()).finish()
    }
}

/// The panic of a [`GcRef`] past the slab's end, out of line: the hot `slot_ptr` passes `i` and
/// `len` in registers only on this path (an inline `assert!` message keeps both on the stack).
#[cold]
#[inline(never)]
fn out_of_range(i: usize, len: usize) -> ! {
    panic!("GcRef {i} out of range (slab len {len})")
}

impl Slab {
    /// An empty slab (it allocates nothing) and its one append token.
    pub(super) fn new() -> (Slab, SlabAppend) {
        let slab = Slab {
            segs: [const { AtomicPtr::new(null_mut()) }; SEGS],
            len: AtomicUsize::new(0),
            _owns: PhantomData,
        };
        (slab, SlabAppend { _only_slab_new: () })
    }

    /// Slots ever appended.
    #[inline]
    fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    /// A pointer to the initialized slot `h`. Panics when `h` was never appended, like a `Vec` index.
    #[inline]
    fn slot_ptr(&self, h: GcRef) -> *mut Slot {
        let i = h.0 as usize;
        let len = self.len();
        if i >= len {
            out_of_range(i, len);
        }
        let k = locate(i).0;
        // SAFETY: `i < len <= 1 << 32`, so `k < SEGS`, segment `k` is allocated and its slot
        // `i - seg_start(k) < seg_cap(k)` is initialized: `push_holes` publishes `len` with
        // `Release` after both. The `Acquire` load of `len` orders this load after them, so it can
        // be `Relaxed`. `wrapping_add(i)` undoes the `wrapping_sub(seg_start(k))` that stored the
        // entry, so the pointer is in the segment's allocation and keeps its provenance.
        unsafe {
            let biased = self.segs.get_unchecked(k).load(Ordering::Relaxed);
            biased.wrapping_add(i).cast::<Slot>()
        }
    }

    #[inline]
    pub(super) fn get(&self, h: GcRef) -> &Slot {
        // SAFETY: `slot_ptr` returns an initialized slot that lives as long as `self`. No `&mut` to
        // it is live: see `slot_mut`'s contract.
        unsafe { &*self.slot_ptr(h) }
    }

    /// Slot `h`, writable through `&self`: every handle of one core writes its slots in place.
    ///
    /// Contract (the caller's duty, as in Go): while the returned `&mut` lives, no other reference
    /// to slot `h` exists. Today each `Vm` has its own core, so its `Heap` handle (behind `&mut`)
    /// is the only user; `Heap::attach`'s handles allocate disjoint slots. When tasks share objects
    /// (M3), a race on one object is the user's bug, and the collector runs with the world stopped.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub(super) fn slot_mut(&self, h: GcRef) -> &mut Slot {
        // SAFETY: as in `get`; exclusive use of the slot is the contract above.
        unsafe { &mut *self.slot_ptr(h) }
    }

    /// Append `slot` and return its handle (tests; the heap appends with `push_holes`).
    #[cfg(test)]
    pub(super) fn push(&self, slot: Slot, append: &mut SlabAppend) -> GcRef {
        let h = GcRef(self.push_holes(1, append).start as u32);
        self.slot_mut(h).obj = slot.obj;
        h
    }

    /// Append empty slots (`obj: None`) up to the end of the segment the next slot falls in, at
    /// most `max`, and return their dense numbers. They fill storage the segment already holds;
    /// only a first slot of a segment allocates the segment, which never moves. Below 64 slots it
    /// appends one slot, so a small heap's caller caches nothing. The token serializes appends.
    pub(super) fn push_holes(&self, max: usize, _append: &mut SlabAppend) -> Range<usize> {
        let start = self.len.load(Ordering::Relaxed);
        assert!(start <= u32::MAX as usize, "heap slab full ({start} slots)");
        let (k, off) = locate(start);
        let n = if start < LOW_SLOTS {
            1
        } else {
            max.min(seg_cap(k) - off).max(1)
        };
        // A segment is allocated at its first slot, so `off == 0` is "not allocated yet".
        let seg = if off == 0 {
            let seg: *mut MaybeUninit<Slot> =
                Box::into_raw(Box::<[Slot]>::new_uninit_slice(seg_cap(k))).cast();
            self.segs[k].store(seg.wrapping_sub(seg_start(k)), Ordering::Release);
            seg
        } else {
            self.seg_base(k)
        };
        for j in off..off + n {
            // SAFETY: slots `off..off + n` of segment `k` are inside the segment (`n` stops at its
            // end) and not yet initialized (`off` is its `seg_len`), so the write neither
            // overwrites a live `Slot` nor leaks one. The token excludes a concurrent append.
            unsafe { seg.add(j).write(MaybeUninit::new(Slot { obj: None })) };
        }
        self.len.store(start + n, Ordering::Release);
        start..start + n
    }

    /// Segment `k`'s first `seg_len(k, len)` slots as a raw slice, for a segment below the segment
    /// count of `len`.
    #[inline]
    fn seg_raw(&self, k: usize, len: usize) -> *mut [Slot] {
        std::ptr::slice_from_raw_parts_mut(self.seg_base(k).cast::<Slot>(), seg_len(k, len))
    }

    /// The first slot of segment `k`'s storage, for an allocated segment.
    #[inline]
    fn seg_base(&self, k: usize) -> *mut MaybeUninit<Slot> {
        self.segs[k]
            .load(Ordering::Acquire)
            .wrapping_add(seg_start(k))
    }

    /// Every initialized slot, in index order.
    pub(super) fn iter(&self) -> impl Iterator<Item = &Slot> {
        let len = self.len();
        // SAFETY: every segment below the segment count of `len` is allocated and its first
        // `seg_len(k, len)` slots are initialized; see `slot_mut`'s contract for the aliasing.
        (0..seg_count(len)).flat_map(move |k| unsafe { &*self.seg_raw(k, len) }.iter())
    }

    /// Every segment's initialized slots with the dense number of its first slot, in index order,
    /// writable through `&self` for the sweep. Contract: as for [`slot_mut`](Slab::slot_mut), for
    /// every slot (the collector is the only user while it runs).
    pub(super) fn segments_mut(&self) -> impl Iterator<Item = (usize, &mut [Slot])> {
        let len = self.len();
        // SAFETY: as in `iter`; the segments are disjoint, and the contract excludes every other
        // reference to their slots.
        (0..seg_count(len)).map(move |k| (seg_start(k), unsafe { &mut *self.seg_raw(k, len) }))
    }
}

/// How many slots of segment `k` are initialized when `len` slots exist, for a segment below the
/// segment count.
#[inline]
fn seg_len(k: usize, len: usize) -> usize {
    (len - seg_start(k)).min(seg_cap(k))
}

/// The number of segments that hold at least one of `len` slots.
#[inline]
fn seg_count(len: usize) -> usize {
    if len == 0 { 0 } else { locate(len - 1).0 + 1 }
}

impl Drop for Slab {
    fn drop(&mut self) {
        let len = *self.len.get_mut();
        for k in 0..seg_count(len) {
            let live = self.seg_raw(k, len);
            let seg = self.seg_base(k);
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

/// The `u64` words of segment `k`'s bits.
#[inline]
fn seg_words(k: usize) -> usize {
    seg_cap(k).div_ceil(64)
}

/// The slots whose bits share [`SlotBits::low`]: segments 0 to 4.
const LOW_SLOTS: usize = 64;

/// One segment table of a [`SlotBits`]: segment `k`'s words (for `k >= 5`), or null before its
/// first bit is set.
type BitsTable = [AtomicPtr<AtomicU64>; SEGS];

/// A bitset with one bit per slab slot: slot `i` is bit `i & 63` of a word. Slots `[0, 64)` (slab
/// segments 0 to 4) share the inline word `low`; a later segment starts at a multiple of 64 and has
/// its own words (word `off >> 6` for offset `off`). The segment table and each segment's words
/// are allocated at the first bit set in them and installed with a compare-exchange, so any thread
/// can set a bit through `&self`. A bit that was never set reads 0. The words are `AtomicU64`:
/// [`set`](SlotBits::set) and [`clear`](SlotBits::clear) use `fetch_or` / `fetch_and` for a plane
/// that threads share a word of; [`set_owned`](SlotBits::set_owned) loads then stores, for a plane
/// that one thread writes at a time (the marks).
#[derive(Default)]
pub(super) struct SlotBits {
    /// The bits of slots `[0, 64)`.
    low: AtomicU64,
    table: AtomicPtr<BitsTable>,
}

impl std::fmt::Debug for SlotBits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlotBits").finish_non_exhaustive()
    }
}

impl SlotBits {
    /// The segment table, or `None` before the first bit is set.
    #[inline]
    fn table(&self) -> Option<&BitsTable> {
        // SAFETY: a non-null table came from `Box::into_raw` in `table_or_alloc`, is never replaced
        // and lives until `drop`.
        unsafe { self.table.load(Ordering::Acquire).as_ref() }
    }

    fn table_or_alloc(&self) -> &BitsTable {
        if let Some(t) = self.table() {
            return t;
        }
        let fresh = Box::into_raw(Box::new([const { AtomicPtr::new(null_mut()) }; SEGS]));
        match self
            .table
            .compare_exchange(null_mut(), fresh, Ordering::AcqRel, Ordering::Acquire)
        {
            // SAFETY: `fresh` is now the installed table; see `table`.
            Ok(_) => unsafe { &*fresh },
            Err(won) => {
                // SAFETY: `fresh` was never shared; `won` is the installed table.
                unsafe {
                    drop(Box::from_raw(fresh));
                    &*won
                }
            }
        }
    }

    /// The words that hold the bits of the segment that starts at dense slot `start` (a
    /// `seg_start`): slot `i`'s bit is bit `i & 63` of word `(i >> 6) - (start >> 6)`. Empty for a
    /// segment past the first 64 slots before a bit in it is set.
    #[inline]
    pub(super) fn segment_words(&self, start: usize) -> &[AtomicU64] {
        if start < LOW_SLOTS {
            return std::slice::from_ref(&self.low);
        }
        self.words(locate(start).0).unwrap_or(&[])
    }

    #[inline]
    fn words(&self, k: usize) -> Option<&[AtomicU64]> {
        let p = self.table()?[k].load(Ordering::Acquire);
        // SAFETY: a non-null entry holds `seg_words(k)` words from `alloc_words`, never replaced
        // and alive until `drop`.
        (!p.is_null()).then(|| unsafe { std::slice::from_raw_parts(p, seg_words(k)) })
    }

    /// The word that holds slot `i`'s bit and the bit's mask, or `None` before a bit of its
    /// segment is set.
    #[inline]
    fn word(&self, i: usize) -> Option<(&AtomicU64, u64)> {
        if i < LOW_SLOTS {
            return Some((&self.low, 1u64 << i));
        }
        let (k, off) = locate(i);
        let p = self.table()?[k].load(Ordering::Acquire);
        // SAFETY: as in `words`; `off < seg_cap(k)`, so `off >> 6 < seg_words(k)`.
        (!p.is_null()).then(|| (unsafe { &*p.add(off >> 6) }, 1u64 << (off & 63)))
    }

    /// As [`word`](SlotBits::word), allocating the segment's words when none is set yet.
    #[inline]
    fn word_or_alloc(&self, i: usize) -> (&AtomicU64, u64) {
        match self.word(i) {
            Some(w) => w,
            None => {
                let (k, off) = locate(i);
                (&self.alloc_words(k)[off >> 6], 1u64 << (off & 63))
            }
        }
    }

    #[cold]
    fn alloc_words(&self, k: usize) -> &[AtomicU64] {
        let entry = &self.table_or_alloc()[k];
        let fresh: Box<[AtomicU64]> = (0..seg_words(k)).map(|_| AtomicU64::new(0)).collect();
        let fresh = Box::into_raw(fresh).cast::<AtomicU64>();
        let p = match entry.compare_exchange(null_mut(), fresh, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => fresh,
            Err(won) => {
                // SAFETY: `fresh` was never shared and holds `seg_words(k)` words.
                unsafe {
                    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                        fresh,
                        seg_words(k),
                    )))
                };
                won
            }
        };
        // SAFETY: `p` is the installed entry; see `words`.
        unsafe { std::slice::from_raw_parts(p, seg_words(k)) }
    }

    /// Test the bit of slot `i`.
    #[inline]
    pub(super) fn get(&self, i: usize) -> bool {
        self.word(i)
            .is_some_and(|(w, bit)| w.load(Ordering::Relaxed) & bit != 0)
    }

    /// Set the bit of slot `i` with `fetch_or`.
    #[inline]
    pub(super) fn set(&self, i: usize) {
        let (w, bit) = self.word_or_alloc(i);
        w.fetch_or(bit, Ordering::Relaxed);
    }

    /// Clear the bit of slot `i` with `fetch_and` (no-op before its segment has a bit set).
    #[inline]
    pub(super) fn clear(&self, i: usize) {
        if let Some((w, bit)) = self.word(i) {
            w.fetch_and(!bit, Ordering::Relaxed);
        }
    }

    /// Set the bit of slot `i` with a load and a store; returns whether it was clear. For a plane
    /// that one thread writes at a time.
    #[inline]
    pub(super) fn set_owned(&self, i: usize) -> bool {
        let (w, bit) = self.word_or_alloc(i);
        let old = w.load(Ordering::Relaxed);
        w.store(old | bit, Ordering::Relaxed);
        old & bit == 0
    }
}

impl Drop for SlotBits {
    fn drop(&mut self) {
        let table = *self.table.get_mut();
        if table.is_null() {
            return;
        }
        // SAFETY: `table` and every non-null entry were made by `Box::into_raw` in this type, with
        // the lengths used here, and are freed once here.
        unsafe {
            for (k, e) in (*table).iter_mut().enumerate() {
                let p = *e.get_mut();
                if !p.is_null() {
                    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                        p,
                        seg_words(k),
                    )));
                }
            }
            drop(Box::from_raw(table));
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
        let (slab, mut append) = Slab::new();
        let first = slab.push(
            Slot {
                obj: Some(Obj::BigInt(-1)),
            },
            &mut append,
        );
        let addr = slab.get(first) as *const Slot;
        let refs: Vec<GcRef> = (0..10_000)
            .map(|i| {
                slab.push(
                    Slot {
                        obj: Some(Obj::BigInt(i)),
                    },
                    &mut append,
                )
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
        let (slab, mut append) = Slab::new();
        let refs: Vec<GcRef> = (0..4097)
            .map(|i| {
                slab.push(
                    Slot {
                        obj: Some(Obj::BigInt(i)),
                    },
                    &mut append,
                )
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
        let (slab, mut append) = Slab::new();
        let n = 1000; // segments 0 to 7 full, part of segment 8
        for i in 0..n {
            let obj = match i % 3 {
                0 => Some(Obj::AtomicInt(Arc::clone(&core))),
                1 => Some(Obj::ByteArray(vec![7; 100 + i])),
                _ => None,
            };
            slab.push(Slot { obj }, &mut append);
        }
        let held = (0..n).filter(|i| i % 3 == 0).count();
        assert_eq!(Arc::strong_count(&core), 1 + held);
        drop(slab);
        assert_eq!(Arc::strong_count(&core), 1);
    }

    /// A bit reads 0 until set, in every segment, and `set_owned` answers whether it was clear.
    #[test]
    fn slot_bits_set_and_clear_across_segments() {
        let bits = SlotBits::default();
        let probe = [0usize, 3, 4, 63, 64, 4095, 4096, 100_000];
        for &i in &probe {
            assert!(!bits.get(i), "slot {i} starts clear");
        }
        for &i in &probe {
            bits.set(i);
        }
        for &i in &probe {
            assert!(bits.get(i));
            assert!(!bits.get(i + 1) || probe.contains(&(i + 1)));
        }
        bits.clear(64);
        assert!(!bits.get(64));
        assert!(bits.get(63), "a clear touches one bit");
        assert!(bits.set_owned(5), "slot 5 was clear");
        assert!(!bits.set_owned(5), "slot 5 is set now");
        let words = bits.segment_words(4096);
        assert_eq!(words.len(), 4096 / 64);
        assert_eq!(words[0].load(Ordering::Relaxed), 1);
        assert_eq!(
            bits.segment_words(0).len(),
            1,
            "slots [0, 64) share one word"
        );
        assert!(SlotBits::default().segment_words(64).is_empty());
    }
}
