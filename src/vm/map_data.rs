//! The hash map and set payloads (`MapData`, `SetData`) and their hash index (`SharedIndex`).
//!
//! M2.3 (Go parity, `vm/rvec.rs`): a map's entries and its index each live on an [`RVec`], so a
//! grow, a remove or a rewrite retires the old entry block or index table, and only the
//! stop-the-world sweep frees it. A racy prober can see a torn entry or a half-written slot (a
//! wrong or missing hit: the user's race, a documented known limit), but every read is an atomic
//! load inside a live block:
//! - The index is an open-addressing table of [`IndexSlot`]s, `(cached hash, position + 1)`, with
//!   linear probing; position 0 is an empty slot. A collision takes the next free slot.
//! - The load factor stays at most 1/2: an insert whose entry count would pass half the capacity
//!   builds a table twice the size, swaps it in, and retires the old one. Any other insert writes
//!   one empty slot in place.
//! - A probe reads at most `capacity` slots, so a racy table cannot make it loop, and it yields
//!   only positions below the entry count it read first.
//! - A remove builds a new entry block and a new table, and swaps both in.
//!
//! The borrow rule (as for a list, M2.2): a mutator takes `&self` plus the `&Heap` that
//! `Heap::write(h)` hands back for a heap object, or the VM's `&self.heap` for a Rust-local
//! payload.

use super::heap::Heap;
use super::rvec::{RVec, View};
use super::value::Value;
use std::sync::atomic::{AtomicU64, Ordering};

/// A map entry: `(cached hash, key, value)`.
pub type MapEntry = (u64, Value, Value);
/// A set entry: `(cached hash, element)`.
pub type SetEntry = (u64, Value);
/// One index slot: `(cached hash, position + 1)`; a zero position is an empty slot.
pub(super) type IndexSlot = (u64, u64);

/// The Fibonacci-hashing multiplier: it spreads the cached hash (an int key's hash is its `f64`
/// bits, whose low bits are mostly zero) over the top bits that pick the first slot.
const SPREAD: u64 = 0x9e37_79b9_7f4a_7c15;

/// The first slot for hash `h` in a table of `cap` slots (a power of two, at least 2).
fn first_slot(h: u64, cap: usize) -> usize {
    (h.wrapping_mul(SPREAD) >> (64 - cap.trailing_zeros())) as usize
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
        let mut i = first_slot(h, cap);
        while v.get(i).1 != 0 {
            i = (i + 1) & (cap - 1);
        }
        v.set(i, (h, pos as u64 + 1));
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
    /// The positions below `limit` whose cached hash is `h` (the probe candidates).
    #[inline]
    fn candidates(&self, h: u64, limit: usize) -> Candidates<'_> {
        let v = self.0.view();
        let cap = v.len();
        Candidates {
            i: if cap == 0 { 0 } else { first_slot(h, cap) },
            left: cap,
            v,
            h,
            limit,
        }
    }
    /// Record that `entries[pos]`'s key hashed to `h`, where `pos` is the entry count before the
    /// push; a grow retires the old table on `heap`.
    #[inline]
    fn insert(&self, heap: &Heap, h: u64, pos: usize) {
        let v = self.0.view();
        let cap = v.len();
        if (pos + 1) * 2 <= cap {
            let mut i = first_slot(h, cap);
            for _ in 0..cap {
                if v.get(i).1 == 0 {
                    v.set(i, (h, pos as u64 + 1));
                    return;
                }
                i = (i + 1) & (cap - 1);
            }
        }
        // Past half full (or full under a racing writer): a table twice the size.
        let old = (0..cap)
            .map(|i| v.get(i))
            .filter(|s| s.1 != 0)
            .map(|(sh, sp)| (sh, sp as usize - 1));
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
pub struct Candidates<'a> {
    v: View<'a, IndexSlot>,
    h: u64,
    i: usize,
    left: usize,
    limit: usize,
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

impl Iterator for Candidates<'_> {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<usize> {
        let mask = self.v.len().wrapping_sub(1);
        while self.left > 0 {
            let (sh, sp) = self.v.get(self.i);
            #[cfg(test)]
            SLOT_READS.with(|c| c.set(c.get() + 1));
            self.i = (self.i + 1) & mask;
            self.left -= 1;
            if sp == 0 {
                self.left = 0;
                return None;
            }
            let pos = (sp - 1) as usize;
            if sh == self.h && pos < self.limit {
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
    pub fn candidates(&self, h: u64) -> Candidates<'_> {
        self.index.candidates(h, self.entries.len())
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
    /// The value of entry `i`.
    #[inline]
    pub fn value_at(&self, i: usize) -> Value {
        self.entries.get(i).2
    }
    /// Replace the value of entry `i` (its key and position stay); returns the old value.
    #[inline]
    pub fn set_value(&self, i: usize, v: Value) -> Value {
        let (h, k, old) = self.entries.get(i);
        self.entries.set(i, (h, k, v));
        old
    }
    /// Append a fresh entry (caller has confirmed the key is absent), updating the index. A grow
    /// retires on `heap`.
    #[inline]
    pub fn push(&self, heap: &Heap, h: u64, k: Value, v: Value) {
        let pos = self.entries.len();
        self.entries.push(heap, (h, k, v));
        self.index.insert(heap, h, pos);
        self.version.bump();
    }
    /// Remove the entry at `i` (preserving order): a new entry block and an index rebuilt from the
    /// cached hashes (pure, no re-hashing) swap in, and the old ones retire on `heap`.
    pub fn remove_at(&self, heap: &Heap, i: usize) -> MapEntry {
        let mut es = self.entries.to_vec();
        let removed = es.remove(i);
        self.entries.replace(heap, &es);
        let hashes: Vec<u64> = es.iter().map(|e| e.0).collect();
        self.index.rebuild(heap, &hashes);
        self.version.bump();
        removed
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
    pub fn candidates(&self, h: u64) -> Candidates<'_> {
        self.index.candidates(h, self.entries.len())
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
        self.index.insert(heap, h, pos);
        self.version.bump();
    }
    /// See [`MapData::remove_at`].
    pub fn remove_at(&self, heap: &Heap, i: usize) -> SetEntry {
        let mut es = self.entries.to_vec();
        let removed = es.remove(i);
        self.entries.replace(heap, &es);
        let hashes: Vec<u64> = es.iter().map(|e| e.0).collect();
        self.index.rebuild(heap, &hashes);
        self.version.bump();
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::MapData;
    use crate::vm::heap::{Fields, Heap, Obj};
    use crate::vm::value::Value;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

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
            assert_eq!(m.value_at(hits[0]), Value::int(-(k as i64)));
        }
        heap.sweep();
    }
}
