//! The hash map and set payloads (`MapData`, `SetData`) and their hash index (`SharedIndex`).
//!
//! M2.3 (Go parity, `vm/rvec.rs`): a map's entries and its index each live on an [`RVec`], so a
//! grow, a remove or a rewrite retires the old entry block or index table, and only the
//! stop-the-world sweep frees it. A racy prober can see a torn entry (a wrong or missing hit: the
//! user's race, a documented known limit), but every read is an atomic load inside a live block:
//! - The index is an open-addressing table of [`IndexSlot`]s with linear probing. A slot is one
//!   `u64` word: a hash tag in the high [`TAG_BITS`] bits, `position + 1` in the low
//!   [`POS_BITS`] bits; a zero word is an empty slot. One store writes a slot, so a racy prober
//!   never sees a tag and a position of two writes. A collision takes the next free slot.
//! - A tag hit is only a candidate: the probe then compares the full hash cached in the entry
//!   (`entries[pos].0`), so a user `eq` never runs on a key whose hash differs.
//! - A map or set holds at most `2^40 - 1` entries (the position field); one more panics with the
//!   `RVec` capacity fault ([`rvec::capacity_overflow`]). The entry block of that many entries
//!   (24 TiB for a map) fails to allocate first.
//! - The load factor stays at most 1/2: an insert whose entry count would pass half the capacity
//!   builds a table twice the size from the entries' cached hashes, swaps it in, and retires the
//!   old one. Any other insert writes one empty slot in place.
//! - A probe reads at most `capacity` slots, so a racy table cannot make it loop, and it yields
//!   only positions below the entry count of the entry snapshot it read first.
//! - A remove builds a new entry block and a new table, and swaps both in.
//!
//! The borrow rule (as for a list, M2.2): a mutator takes `&self` plus the `&Heap` that
//! `Heap::write(h)` hands back for a heap object, or the VM's `&self.heap` for a Rust-local
//! payload.

use super::heap::Heap;
use super::rvec::{self, CachedHash, RVec, View};
use super::value::Value;
use std::sync::atomic::{AtomicU64, Ordering};

/// A map entry: `(cached hash, key, value)`.
pub type MapEntry = (u64, Value, Value);
/// A set entry: `(cached hash, element)`.
pub type SetEntry = (u64, Value);
/// One index slot: `tag << POS_BITS | (position + 1)`; a zero word is an empty slot.
pub(super) type IndexSlot = u64;

/// The low bits of a slot that hold `position + 1`.
const POS_BITS: u32 = 40;
/// The high bits of a slot that hold the hash tag.
const TAG_BITS: u32 = 64 - POS_BITS;
const POS_MASK: u64 = (1 << POS_BITS) - 1;

/// The Fibonacci-hashing multiplier: it spreads the cached hash (an int key's hash is its `f64`
/// bits, whose low bits are mostly zero) over the top bits that pick the first slot.
const SPREAD: u64 = 0x9e37_79b9_7f4a_7c15;

/// Where hash `h` goes in a table of `cap` slots (a power of two, at least 8): its first slot
/// (the top `log2(cap)` bits of the spread hash) and its tag (the next [`TAG_BITS`] bits, so a
/// tag tells apart keys whose first slots match).
#[inline(always)]
fn place(h: u64, cap: usize) -> (usize, u64) {
    let x = h.wrapping_mul(SPREAD);
    let b = cap.trailing_zeros();
    ((x >> (64 - b)) as usize, (x << b) >> (64 - TAG_BITS))
}

/// The slot word for `tag` and entry position `pos`. A position past the field faults as an
/// `RVec` that cannot grow does.
#[inline(always)]
fn slot_word(tag: u64, pos: usize) -> IndexSlot {
    let p = pos as u64 + 1;
    if p > POS_MASK {
        rvec::capacity_overflow();
    }
    tag << POS_BITS | p
}

/// The capacity of a table for `n` entries: a power of two, at least 8, at most half full.
fn table_cap(n: usize) -> usize {
    n.saturating_mul(2).next_power_of_two().max(8)
}

/// A table of `n` entries, from their `(hash, position)` pairs; no other thread sees it yet.
fn build(n: usize, keys: impl Iterator<Item = (u64, usize)>) -> RVec<IndexSlot> {
    let cap = table_cap(n);
    let t: RVec<IndexSlot> = RVec::zeroed(cap);
    let v = t.view();
    for (h, pos) in keys {
        let (mut i, tag) = place(h, cap);
        while v.get(i) != 0 {
            i = (i + 1) & (cap - 1);
        }
        v.set(i, slot_word(tag, pos));
    }
    t
}

/// The count of a table's structural changes: each push (a new position) and each remove (later
/// positions shift down) adds one; a value write moves nothing and adds none. A probe whose user
/// `eq` can re-enter reads it before the compare and restarts when it moved (`Vm::map_probe`).
#[derive(Debug, Default)]
struct Version(AtomicU64);

impl Version {
    fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
    fn bump(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

impl Clone for Version {
    fn clone(&self) -> Self {
        Version(AtomicU64::new(self.get()))
    }
}

/// A heap map's or set's hash index: the open-addressing table on an [`RVec`] (see the module
/// doc). Holds positions, **not** GC children (only `entries`' keys/values are traced).
#[derive(Debug, Clone, Default)]
struct SharedIndex(RVec<IndexSlot>);

impl SharedIndex {
    /// The index of a table whose `i`-th entry's key hashed to `hashes[i]`.
    fn from_hashes(hashes: &[u64]) -> Self {
        if hashes.is_empty() {
            return SharedIndex::default();
        }
        SharedIndex(build(hashes.len(), hashes.iter().copied().zip(0..)))
    }
    /// The positions in the snapshot `entries` whose cached hash is `h` (the probe candidates).
    #[inline]
    fn candidates<'a, E: CachedHash>(&'a self, h: u64, entries: View<'a, E>) -> Candidates<'a, E> {
        let v = self.0.view();
        let cap = v.len();
        let (i, tag) = if cap == 0 { (0, 0) } else { place(h, cap) };
        Candidates {
            i,
            left: cap,
            v,
            entries,
            h,
            tag,
        }
    }
    /// Record that `entries[pos]`'s key hashed to `h`, where `pos` is the entry count before the
    /// push; a grow rebuilds the table from the cached hashes of `entries` and retires the old
    /// table on `heap`.
    #[inline]
    fn insert<E: CachedHash>(&self, heap: &Heap, entries: &RVec<E>, h: u64, pos: usize) {
        let v = self.0.view();
        let cap = v.len();
        if (pos + 1) * 2 <= cap {
            let (mut i, tag) = place(h, cap);
            for _ in 0..cap {
                if v.get(i) == 0 {
                    v.set(i, slot_word(tag, pos));
                    return;
                }
                i = (i + 1) & (cap - 1);
            }
        }
        // Past half full (or full under a racing writer): a table twice the size. The slots keep
        // no full hash, so the entries give them; the new entry is `(h, pos)` in any case.
        let es = entries.view();
        let old = (0..pos.min(es.len())).map(|p| (es.hash(p), p));
        let t = build(cap.max(pos + 1), old.chain([(h, pos)]));
        self.0.install(heap, t);
    }
    /// The bytes the table owns.
    fn heap_bytes(&self) -> usize {
        self.0.capacity() * size_of::<IndexSlot>()
    }
    /// Swap in the index of `hashes` (no table when it is empty); the old table retires on
    /// `heap`.
    fn rebuild(&self, heap: &Heap, hashes: &[u64]) {
        if hashes.is_empty() {
            self.0.install(heap, RVec::new());
            return;
        }
        self.0
            .install(heap, build(hashes.len(), hashes.iter().copied().zip(0..)));
    }
}

/// See [`SharedIndex::candidates`]. Do not hold it across a safepoint after the table could
/// retire (a mutator of the same map or set).
pub struct Candidates<'a, E: CachedHash> {
    v: View<'a, IndexSlot>,
    entries: View<'a, E>,
    h: u64,
    tag: u64,
    i: usize,
    left: usize,
}

#[cfg(test)]
thread_local! {
    static SLOT_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Index slots this thread's probes read so far (tests only).
#[cfg(test)]
pub(super) fn slot_reads() -> usize {
    SLOT_READS.with(|c| c.get())
}

impl<E: CachedHash> Iterator for Candidates<'_, E> {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<usize> {
        let mask = self.v.len().wrapping_sub(1);
        while self.left > 0 {
            let s = self.v.get(self.i);
            #[cfg(test)]
            SLOT_READS.with(|c| c.set(c.get() + 1));
            self.i = (self.i + 1) & mask;
            self.left -= 1;
            if s == 0 {
                self.left = 0;
                return None;
            }
            let pos = ((s & POS_MASK) - 1) as usize;
            if s >> POS_BITS == self.tag
                && pos < self.entries.len()
                && self.entries.hash(pos) == self.h
            {
                return Some(pos);
            }
        }
        None
    }
}

/// A real hash table that *also* preserves insertion order. `entries` is the insertion-ordered
/// store (so iteration, `keys()`, set equality, and GC tracing stay deterministic); `index` maps a
/// key's cached hash to its candidate position(s) in `entries` for O(1)-average lookup. The cached
/// `u64` per entry makes index rebuild (after a remove) a pure, engine-free pass — no re-hashing of
/// user `hash()` methods. Probing always confirms a hash hit with the engine's `values_equal`
/// (structural), so a collision never returns the wrong key. The `index` holds plain `usize` — it
/// is **not** a GC child (only `entries`' keys/values are traced).
#[derive(Debug, Clone, Default)]
pub struct MapData {
    pub entries: RVec<MapEntry>,
    /// `cached-hash → candidate position(s)` (see [`SharedIndex`]). **Not** a GC child.
    index: SharedIndex,
    version: Version,
}

impl MapData {
    /// A map of `entries`, which hold distinct keys.
    pub fn from_entries(entries: Vec<MapEntry>) -> Self {
        let hashes: Vec<u64> = entries.iter().map(|e| e.0).collect();
        let index = SharedIndex::from_hashes(&hashes);
        MapData {
            entries: entries.into(),
            index,
            version: Version::default(),
        }
    }
    /// Positions in `entries` whose key hashed to `h` (the probe candidates).
    #[inline]
    pub fn candidates(&self, h: u64) -> Candidates<'_, MapEntry> {
        self.index.candidates(h, self.entries.view())
    }
    /// The count of structural changes (see `Version`).
    #[inline]
    pub fn version(&self) -> u64 {
        self.version.get()
    }
    /// The bytes the entry block and the index table own (`live_bytes`).
    pub fn heap_bytes(&self) -> usize {
        self.entries.capacity() * size_of::<MapEntry>() + self.index.heap_bytes()
    }
    /// The value of entry `i`, or `None` when the entry block holds no entry `i`. M3.0a — a
    /// probe's position names the entry snapshot the probe read, and another thread can remove
    /// entries after it: the caller reads `None` as a miss.
    #[inline]
    pub fn value_at(&self, i: usize) -> Option<Value> {
        self.entries.try_get(i).map(|e| e.2)
    }
    /// Replace the value of entry `i` (its key and position stay); returns the old value, or
    /// `None` (and writes nothing) when the entry block holds no entry `i` (see `value_at`).
    #[inline]
    #[must_use = "`None` means no entry `i`: the caller inserts the key"]
    pub fn set_value(&self, i: usize, v: Value) -> Option<Value> {
        let es = self.entries.view();
        if i >= es.len() {
            return None;
        }
        let (h, k, old) = es.get(i);
        es.set(i, (h, k, v));
        Some(old)
    }
    /// Append a fresh entry (caller has confirmed the key is absent), updating the index. A grow
    /// retires on `heap`.
    #[inline]
    pub fn push(&self, heap: &Heap, h: u64, k: Value, v: Value) {
        let pos = self.entries.len();
        self.entries.push(heap, (h, k, v));
        self.index.insert(heap, &self.entries, h, pos);
        self.version.bump();
    }
    /// Remove the entry at `i` (preserving order): a new entry block and an index rebuilt from the
    /// cached hashes (pure, no re-hashing) swap in, and the old ones retire on `heap`. `None`
    /// (and no change) when the entry block holds no entry `i` (see `value_at`).
    pub fn remove_at(&self, heap: &Heap, i: usize) -> Option<MapEntry> {
        let mut es = self.entries.to_vec();
        if i >= es.len() {
            return None;
        }
        let removed = es.remove(i);
        self.entries.replace(heap, &es);
        let hashes: Vec<u64> = es.iter().map(|e| e.0).collect();
        self.index.rebuild(heap, &hashes);
        self.version.bump();
        Some(removed)
    }
}

/// A hash *set* with the same insertion-order-preserving design as [`MapData`].
#[derive(Debug, Clone, Default)]
pub struct SetData {
    pub entries: RVec<SetEntry>,
    /// `cached-hash → candidate position(s)` (see [`SharedIndex`]).
    index: SharedIndex,
    version: Version,
}

impl SetData {
    /// A set of `entries`, which hold distinct elements.
    pub fn from_entries(entries: Vec<SetEntry>) -> Self {
        let hashes: Vec<u64> = entries.iter().map(|e| e.0).collect();
        let index = SharedIndex::from_hashes(&hashes);
        SetData {
            entries: entries.into(),
            index,
            version: Version::default(),
        }
    }
    #[inline]
    pub fn candidates(&self, h: u64) -> Candidates<'_, SetEntry> {
        self.index.candidates(h, self.entries.view())
    }
    /// The count of structural changes (see `Version`).
    #[inline]
    pub fn version(&self) -> u64 {
        self.version.get()
    }
    /// See [`MapData::heap_bytes`].
    pub fn heap_bytes(&self) -> usize {
        self.entries.capacity() * size_of::<SetEntry>() + self.index.heap_bytes()
    }
    /// See [`MapData::push`].
    #[inline]
    pub fn push(&self, heap: &Heap, h: u64, e: Value) {
        let pos = self.entries.len();
        self.entries.push(heap, (h, e));
        self.index.insert(heap, &self.entries, h, pos);
        self.version.bump();
    }
    /// See [`MapData::remove_at`].
    pub fn remove_at(&self, heap: &Heap, i: usize) -> Option<SetEntry> {
        let mut es = self.entries.to_vec();
        if i >= es.len() {
            return None;
        }
        let removed = es.remove(i);
        self.entries.replace(heap, &es);
        let hashes: Vec<u64> = es.iter().map(|e| e.0).collect();
        self.index.rebuild(heap, &hashes);
        self.version.bump();
        Some(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::MapData;
    use crate::vm::heap::{Fields, Heap, Obj};
    use crate::vm::value::Value;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// An index slot is one 8-byte word, and a map's bytes count the table at 8 bytes a slot.
    #[test]
    fn index_slot_is_one_word() {
        let _g = crate::vm::rvec::test_serial();
        let mut heap = Heap::new();
        assert_eq!(size_of::<super::IndexSlot>(), 8);
        let m = MapData::default();
        m.push(&heap, 1, Value::int(1), Value::int(2));
        let entries = m.entries.capacity() * size_of::<super::MapEntry>();
        assert_eq!(m.heap_bytes(), entries + 8 * 8, "8 slots of 8 bytes");
        heap.sweep();
    }

    /// The inverse of `SPREAD` mod 2^64 (Newton's iteration; `SPREAD` is odd).
    fn spread_inverse() -> u64 {
        let mut x = super::SPREAD;
        for _ in 0..6 {
            x = x.wrapping_mul(2u64.wrapping_sub(super::SPREAD.wrapping_mul(x)));
        }
        assert_eq!(x.wrapping_mul(super::SPREAD), 1);
        x
    }

    /// Two hashes whose spread values differ only in bit 0 pick one first slot and one tag at
    /// every table size: a probe for one must not yield the other's entry (its full hash
    /// differs, so a user `eq` must not run on it).
    #[test]
    fn a_tag_hit_with_another_hash_is_no_candidate() {
        let _g = crate::vm::rvec::test_serial();
        let mut heap = Heap::new();
        let inv = spread_inverse();
        let mut seen = 0;
        for x in [
            0x8000_0000_0000_0000u64,
            0x1234_5678_9abc_def0,
            u64::MAX - 1,
        ] {
            let (h1, h2) = (x.wrapping_mul(inv), (x ^ 1).wrapping_mul(inv));
            let m = MapData::default();
            // Grow the table through 8 .. 4096 slots; probe at each size.
            for k in 0..2000u64 {
                let h = if k == 0 {
                    h1
                } else {
                    k.wrapping_mul(0x51_7cc1_b727_220a)
                };
                m.push(&heap, h, Value::int(k as i64), Value::nil());
                assert_eq!(m.candidates(h1).collect::<Vec<_>>(), [0], "h1 at {k}");
                assert_eq!(m.candidates(h2).count(), 0, "h2 at {k}");
                seen += 1;
            }
            let s = super::SetData::default();
            s.push(&heap, h1, Value::int(1));
            assert_eq!(s.candidates(h2).count(), 0, "set");
        }
        assert_eq!(seen, 6000);
        heap.sweep();
    }

    /// The last position the slot holds encodes and decodes whole; one more is the `RVec`
    /// capacity fault.
    #[test]
    fn a_position_past_the_slot_field_faults() {
        let last = super::POS_MASK as usize - 1;
        let w = super::slot_word(super::POS_MASK >> 16, last);
        assert_eq!(w >> super::POS_BITS, super::POS_MASK >> 16, "the tag");
        assert_eq!((w & super::POS_MASK) - 1, last as u64, "the position");
        let r = std::panic::catch_unwind(|| super::slot_word(0, last + 1));
        let msg = r
            .expect_err("past the field")
            .downcast::<&str>()
            .ok()
            .map(|m| *m);
        assert_eq!(msg, Some("an RVec capacity overflows"));
    }

    /// A remove of the last entry leaves no table, like a map that never held one.
    #[test]
    fn removing_the_last_entry_drops_the_index_table() {
        let _g = crate::vm::rvec::test_serial();
        let mut heap = Heap::new();
        let m = MapData::default();
        m.push(&heap, 1, Value::int(1), Value::int(2));
        assert_eq!(m.index.0.len(), 8);
        m.remove_at(&heap, 0);
        assert_eq!(m.index.0.len(), 0, "an empty map holds no table");
        heap.sweep();
    }

    /// A snapshot of a struct key that holds a map builds the copy's blocks once: it retires
    /// nothing.
    #[test]
    fn snapshot_of_a_map_retires_nothing() {
        let _g = crate::vm::rvec::test_serial();
        let mut vm = crate::vm::Vm::new(Arc::new(crate::vm::tests::empty_program()));
        let m = MapData::from_entries(
            (0..10)
                .map(|k| (k, Value::int(k as i64), Value::nil()))
                .collect(),
        );
        let mh = vm.heap.alloc(Obj::Map(m));
        let key = vm.heap.alloc(Obj::Struct {
            tid: crate::base::op::TID_NONE,
            fields: Fields::from_vec(vec![Value::obj(mh)]),
        });
        let before = vm.heap.retired_len();
        let snap = vm.snapshot_key(Value::obj(key));
        assert_ne!(snap, Value::obj(key), "the key is copied");
        assert_eq!(vm.heap.retired_len(), before, "the copy retired no block");
    }

    /// One thread inserts 50k keys (growing the entry block and the index many times) while
    /// another probes. Every probe reads only live blocks; a hit names the key it was asked for.
    /// After both join, every key is found once.
    #[test]
    fn racing_insert_and_probe() {
        const N: u64 = 50_000;
        let _g = crate::vm::rvec::test_serial();
        let mut heap = Heap::new();
        let m = MapData::default();
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                for k in 0..N {
                    m.push(&heap, k, Value::int(k as i64), Value::int(-(k as i64)));
                }
                done.store(true, Ordering::Release);
            });
            s.spawn(|| {
                let mut k = 0u64;
                while !done.load(Ordering::Acquire) {
                    for p in m.candidates(k) {
                        if let Some((h, key, _)) = m.entries.try_get(p) {
                            assert!(h != k || key == Value::int(k as i64));
                        }
                    }
                    k = (k + 7919) % N;
                }
            });
        });
        for k in 0..N {
            let hits: Vec<usize> = m.candidates(k).collect();
            assert_eq!(hits, [k as usize], "key {k}");
            assert_eq!(m.value_at(hits[0]), Some(Value::int(-(k as i64))));
        }
        heap.sweep();
    }
}
