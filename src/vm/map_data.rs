//! The hash map and set payloads (`MapData`, `SetData`) and their hash index (`HashIndex`).
//!
//! M2.3 (Go parity, `vm/rvec.rs`): a map's entries live on an [`RVec`] and its index in an
//! [`RBox`], so a grow, a remove or a rewrite retires the old entry block or index table, and only
//! the stop-the-world sweep frees it. A racy prober can see a torn entry or a half-written table
//! (the user's race, a documented known limit), but never freed memory:
//! - An insert that fits the current table writes it in place. An insert of a new key into a full
//!   table clones the table into one twice the size, swaps it in, and retires the old one.
//! - A `Pos::Many` overflow vec pushes in place while it has spare capacity; a full one is copied
//!   into a new vec twice the size, and the old vec retires.
//! - A remove builds a new entry block and a new table, and swaps both in.
//!
//! The borrow rule (as for a list, M2.2): a mutator takes `&self` plus the `&Heap` that
//! `Heap::write(h)` hands back for a heap object, or the VM's `&self.heap` for a Rust-local
//! payload. A caller holds no `candidates` slice across a mutator call.

use super::fxhash::FxHashMap;
use super::heap::Heap;
use super::rvec::{RBox, RVec, retire_box};
use super::value::Value;

/// A map entry: `(cached hash, key, value)`.
pub type MapEntry = (u64, Value, Value);
/// A set entry: `(cached hash, element)`.
pub type SetEntry = (u64, Value);

/// A single key's position(s) in `entries`. Numeric keys hash injectively (`(n as f64).to_bits()`),
/// so the overwhelmingly common case is a single candidate — [`Pos::One`] inlines it with **zero
/// heap allocation** (the old `Vec<usize>` paid one tiny alloc per distinct key). [`Pos::Many`]
/// holds the overflow for genuine hash collisions only (string `DefaultHasher`, or a user `hash()`
/// that returns a constant); boxing the `Vec` keeps `Pos` at two words. Probing always confirms a
/// hit with `values_equal`, so a collision is still correct.
#[derive(Debug, Clone)]
pub(crate) enum Pos {
    One(usize),
    // `Box` keeps `Pos` at 2 words (`usize` + tag) instead of 4; the alloc only happens on a real
    // collision, which is off the numeric-key hot path entirely.
    #[allow(clippy::box_collection)]
    Many(Box<Vec<usize>>),
}

/// `cached-hash → candidate position(s)`, FxHash-keyed (the `u64` is already a content hash; see
/// [`super::fxhash`]). The plain table: a wire table ([`WireTable`](super::wire::WireTable)) owns
/// one outright, and a heap map or set holds one in a [`SharedIndex`]. Holds plain `usize`s,
/// **not** GC children (only `entries`' keys/values are traced).
#[derive(Debug, Clone, Default)]
pub(crate) struct HashIndex(FxHashMap<u64, Pos>);

impl HashIndex {
    /// Positions whose key hashed to `h` (the probe candidates). One → a 1-element slice in place;
    /// Many → the overflow vec; absent → empty.
    #[inline]
    pub(crate) fn candidates(&self, h: u64) -> &[usize] {
        match self.0.get(&h) {
            Some(Pos::One(p)) => std::slice::from_ref(p),
            Some(Pos::Many(v)) => v.as_slice(),
            None => &[],
        }
    }
    /// Record that `entries[pos]`'s key hashed to `h`, in a table no other thread can see.
    /// Absent → `One`; first collision → upgrade to `Many` carrying BOTH the prior and the new
    /// position; further collisions → push.
    #[inline]
    pub(crate) fn insert(&mut self, h: u64, pos: usize) {
        self.insert_with(h, pos, |v, pos| v.push(pos));
    }
    /// [`insert`](HashIndex::insert) for a table racy readers may hold: `push_many` appends to a
    /// `Many` vec.
    #[inline]
    fn insert_with(
        &mut self,
        h: u64,
        pos: usize,
        push_many: impl FnOnce(&mut Box<Vec<usize>>, usize),
    ) {
        use std::collections::hash_map::Entry;
        match self.0.entry(h) {
            Entry::Vacant(e) => {
                e.insert(Pos::One(pos));
            }
            Entry::Occupied(mut e) => match e.get_mut() {
                Pos::One(prev) => {
                    let prev = *prev;
                    e.insert(Pos::Many(Box::new(vec![prev, pos])));
                }
                Pos::Many(v) => push_many(v, pos),
            },
        }
    }
    /// Whether inserting `h` keeps the table's allocation: `h` is present, or the table has room
    /// for one more key (no remove ever leaves a tombstone, so room is `len < capacity`).
    fn fits(&self, h: u64) -> bool {
        self.0.len() < self.0.capacity() || self.0.contains_key(&h)
    }
    /// A copy of this table with room for twice its keys (at least 4).
    fn grown(&self) -> HashIndex {
        let mut t =
            FxHashMap::with_capacity_and_hasher((self.0.len() * 2).max(4), Default::default());
        t.extend(self.0.iter().map(|(&h, p)| (h, p.clone())));
        HashIndex(t)
    }
    /// The bytes the table owns (its slots and the `Many` vecs), for the retire charge.
    fn heap_bytes(&self) -> usize {
        let many: usize = self
            .0
            .values()
            .map(|p| match p {
                Pos::One(_) => 0,
                Pos::Many(v) => v.capacity() * size_of::<usize>(),
            })
            .sum();
        self.0.capacity() * (size_of::<(u64, Pos)>() + 1) + many
    }
    /// The index of a table whose `i`-th entry's key hashed to the `i`-th item of `hashes`.
    pub(crate) fn from_hashes(hashes: impl Iterator<Item = u64>) -> HashIndex {
        let mut ix = HashIndex::default();
        for (pos, h) in hashes.enumerate() {
            ix.insert(h, pos);
        }
        ix
    }
}

/// A heap map's or set's [`HashIndex`]: the table in an [`RBox`], so a table or `Many` vec that
/// must reallocate is replaced and retired, never freed under a racy prober (see the module doc).
#[derive(Debug, Clone, Default)]
struct SharedIndex(RBox<HashIndex>);

impl SharedIndex {
    fn from_hashes(hashes: impl Iterator<Item = u64>) -> Self {
        SharedIndex(RBox::new(HashIndex::from_hashes(hashes)))
    }
    #[inline]
    fn candidates(&self, h: u64) -> &[usize] {
        self.0.get().map_or(&[], |ix| ix.candidates(h))
    }
    /// Record that `entries[pos]`'s key hashed to `h`; a reallocation retires on `heap`.
    #[inline]
    fn insert(&self, heap: &Heap, h: u64, pos: usize) {
        match self.0.get() {
            Some(ix) if ix.fits(h) => {
                self.0.update_in_place(|ix| {
                    ix.insert_with(h, pos, |v, pos| {
                        if v.len() < v.capacity() {
                            v.push(pos);
                        } else {
                            let mut new = Vec::with_capacity(v.capacity() * 2);
                            new.extend_from_slice(v);
                            new.push(pos);
                            let old = std::mem::replace(v, Box::new(new));
                            let bytes = old.capacity() * size_of::<usize>();
                            retire_box(heap, old, bytes);
                        }
                    })
                });
            }
            cur => {
                let (mut new, old_bytes) = match cur {
                    Some(ix) => (ix.grown(), ix.heap_bytes()),
                    None => (
                        HashIndex(FxHashMap::with_capacity_and_hasher(3, Default::default())),
                        0,
                    ),
                };
                new.insert(h, pos);
                self.0.replace(heap, new, old_bytes);
            }
        }
    }
    /// Swap in the index of `hashes`; the old table retires on `heap`.
    fn rebuild(&self, heap: &Heap, hashes: impl Iterator<Item = u64>) {
        let old_bytes = self.0.get().map_or(0, HashIndex::heap_bytes);
        self.0
            .replace(heap, HashIndex::from_hashes(hashes), old_bytes);
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
    /// `cached-hash → candidate position(s)` (see [`HashIndex`]). **Not** a GC child.
    index: SharedIndex,
}

impl MapData {
    /// A map of `entries`, which hold distinct keys.
    pub fn from_entries(entries: Vec<MapEntry>) -> Self {
        let index = SharedIndex::from_hashes(entries.iter().map(|e| e.0));
        MapData {
            entries: entries.into(),
            index,
        }
    }
    /// Positions in `entries` whose key hashed to `h` (the probe candidates).
    #[inline]
    pub fn candidates(&self, h: u64) -> &[usize] {
        self.index.candidates(h)
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
    }
    /// Remove the entry at `i` (preserving order): a new entry block and an index rebuilt from the
    /// cached hashes (pure, no re-hashing) swap in, and the old ones retire on `heap`.
    pub fn remove_at(&self, heap: &Heap, i: usize) -> MapEntry {
        let mut es = self.entries.to_vec();
        let removed = es.remove(i);
        self.entries.replace(heap, &es);
        self.index.rebuild(heap, es.iter().map(|e| e.0));
        removed
    }
}

/// A hash *set* with the same insertion-order-preserving design as [`MapData`].
#[derive(Debug, Clone, Default)]
pub struct SetData {
    pub entries: RVec<SetEntry>,
    /// `cached-hash → candidate position(s)` (see [`HashIndex`]).
    index: SharedIndex,
}

impl SetData {
    /// A set of `entries`, which hold distinct elements.
    pub fn from_entries(entries: Vec<SetEntry>) -> Self {
        let index = SharedIndex::from_hashes(entries.iter().map(|e| e.0));
        SetData {
            entries: entries.into(),
            index,
        }
    }
    #[inline]
    pub fn candidates(&self, h: u64) -> &[usize] {
        self.index.candidates(h)
    }
    /// See [`MapData::push`].
    #[inline]
    pub fn push(&self, heap: &Heap, h: u64, e: Value) {
        let pos = self.entries.len();
        self.entries.push(heap, (h, e));
        self.index.insert(heap, h, pos);
    }
    /// See [`MapData::remove_at`].
    pub fn remove_at(&self, heap: &Heap, i: usize) -> SetEntry {
        let mut es = self.entries.to_vec();
        let removed = es.remove(i);
        self.entries.replace(heap, &es);
        self.index.rebuild(heap, es.iter().map(|e| e.0));
        removed
    }
}
