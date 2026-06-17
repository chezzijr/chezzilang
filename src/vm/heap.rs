//! The GC-managed heap (M5).
//!
//! M5a: `alloc` only inserts (the slot/free-list machinery is in place; the mark-sweep collector
//! lands in M5b). Objects are addressed by [`GcRef`] (a slot index), so handle copies alias one
//! object. The VM owns the heap and mutates objects through `&mut heap[h]` — no `RefCell` needed.

use super::chzstr::ChzStr;
use super::core::{AtomicCore, ChannelCore, ExecutorCore, ListenerCore, SharedCore, SocketCore};
use super::fxhash::FxHashMap;
use super::op::ProtoId;
use super::value::{GcRef, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// A single key's position(s) in `entries`. Numeric keys hash injectively (`(n as f64).to_bits()`),
/// so the overwhelmingly common case is a single candidate — [`Pos::One`] inlines it with **zero
/// heap allocation** (the old `Vec<usize>` paid one tiny alloc per distinct key). [`Pos::Many`]
/// holds the overflow for genuine hash collisions only (string `DefaultHasher`, or a user `hash()`
/// that returns a constant); boxing the `Vec` keeps `Pos` at two words so `MapData`/`SetData` size
/// is unchanged. Probing always confirms a hit with `values_equal`, so a collision is still correct.
#[derive(Debug, Clone)]
enum Pos {
    One(usize),
    // `Box` keeps `Pos` at 2 words (`usize` + tag) instead of 4; the alloc only happens on a real
    // collision, which is off the numeric-key hot path entirely.
    #[allow(clippy::box_collection)]
    Many(Box<Vec<usize>>),
}

/// `cached-hash → candidate position(s)`, FxHash-keyed (the `u64` is already a content hash; see
/// [`super::fxhash`]). Shared by [`MapData`] and [`SetData`] — the index logic is identical for both.
/// Holds plain `usize`s, **not** GC children (only `entries`' keys/values are traced).
#[derive(Debug, Clone, Default)]
struct HashIndex(FxHashMap<u64, Pos>);

impl HashIndex {
    /// Positions whose key hashed to `h` (the probe candidates). One → a 1-element slice in place;
    /// Many → the overflow vec; absent → empty.
    #[inline]
    fn candidates(&self, h: u64) -> &[usize] {
        match self.0.get(&h) {
            Some(Pos::One(p)) => std::slice::from_ref(p),
            Some(Pos::Many(v)) => v.as_slice(),
            None => &[],
        }
    }
    /// Record that `entries[pos]`'s key hashed to `h`. Absent → `One`; first collision → upgrade to
    /// `Many` carrying BOTH the prior and the new position; further collisions → push.
    #[inline]
    fn insert(&mut self, h: u64, pos: usize) {
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
                Pos::Many(v) => v.push(pos),
            },
        }
    }
    #[inline]
    fn clear(&mut self) {
        self.0.clear();
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
    pub entries: Vec<(u64, Value, Value)>,
    /// `cached-hash → candidate position(s)` (see [`HashIndex`]). **Not** a GC child.
    index: HashIndex,
}

impl MapData {
    /// Positions in `entries` whose key hashed to `h` (the probe candidates).
    #[inline]
    pub fn candidates(&self, h: u64) -> &[usize] {
        self.index.candidates(h)
    }
    /// Append a fresh entry (caller has confirmed the key is absent), updating the index.
    #[inline]
    pub fn push(&mut self, h: u64, k: Value, v: Value) {
        let pos = self.entries.len();
        self.entries.push((h, k, v));
        self.index.insert(h, pos);
    }
    /// Remove the entry at `i` (shifting the tail, preserving order) and rebuild the index from the
    /// cached hashes — pure, no re-hashing.
    pub fn remove_at(&mut self, i: usize) -> (u64, Value, Value) {
        let removed = self.entries.remove(i);
        self.rebuild_index();
        removed
    }
    fn rebuild_index(&mut self) {
        self.index.clear();
        for (pos, (h, _, _)) in self.entries.iter().enumerate() {
            self.index.insert(*h, pos);
        }
    }
}

/// A hash *set* with the same insertion-order-preserving design as [`MapData`].
#[derive(Debug, Clone, Default)]
pub struct SetData {
    pub entries: Vec<(u64, Value)>,
    /// `cached-hash → candidate position(s)` (see [`HashIndex`]).
    index: HashIndex,
}

impl SetData {
    #[inline]
    pub fn candidates(&self, h: u64) -> &[usize] {
        self.index.candidates(h)
    }
    #[inline]
    pub fn push(&mut self, h: u64, e: Value) {
        let pos = self.entries.len();
        self.entries.push((h, e));
        self.index.insert(h, pos);
    }
    pub fn remove_at(&mut self, i: usize) -> (u64, Value) {
        let removed = self.entries.remove(i);
        self.rebuild_index();
        removed
    }
    fn rebuild_index(&mut self) {
        self.index.clear();
        for (pos, (h, _)) in self.entries.iter().enumerate() {
            self.index.insert(*h, pos);
        }
    }
}

/// A heap object — the reference half of the value space.
#[derive(Debug, Clone)]
pub enum Obj {
    Str(ChzStr),
    /// `bytes` — an immutable heap byte sequence (Python `bytes` model). A GC LEAF: it holds only
    /// raw `u8`s (no `GcRef`s), so `children()` returns nothing — it is marked reachable but traces
    /// no children, exactly like `Str`/`Native`. `Box<[u8]>` is 16B, well within the 88B `Obj` cap.
    Bytes(Box<[u8]>),
    /// `bytearray` — the MUTABLE sibling of `bytes` (Python `bytearray` model). Storage is a `Vec<u8>`
    /// mutated IN PLACE through the `GcRef` heap slot (`heap.get_mut`), exactly like [`List`](Obj::List),
    /// so two bindings to the same `bytearray` observe each other's writes. Still a GC LEAF — raw `u8`s,
    /// no `GcRef`s — so `children()` traces nothing (the difference vs `Bytes` is the mutability of the
    /// slot, not GC reachability). `Vec<u8>` is 24B (= `List`'s `Vec<Value>`), within the 88B `Obj` cap.
    ByteArray(Vec<u8>),
    /// `Iter` — a composable cursor (the `Iterable[T]` `.iter()` result), the heap payload behind the
    /// existential `Iterator[T]` type. A frozen SNAPSHOT (`items`) of a collection's contents at the
    /// instant `.iter()` was called, plus a read `pos`. `.next()` returns `Some(items[pos])` and
    /// advances; `None` (idempotent) past the end. NON-LEAF (unlike `Bytes`/`ByteArray`): `items` may
    /// hold heap `GcRef`s (a list of structs, a set of strings, …), so `children()` MUST trace them
    /// or the snapshot's elements get collected out from under a live cursor. `Vec<Value>`(24) +
    /// `usize`(8) = 32B, within the 88B `Obj` cap (`Module` still dominates).
    Iter { items: Vec<Value>, pos: usize },
    List(Vec<Value>),
    /// `(a, b, …)` — a fixed-arity, immutable tuple. Elements may be heap objects, so they are
    /// traced as GC children (same as `List`).
    Tuple(Vec<Value>),
    /// `{k: v, …}` — an insertion-ordered hash map (see [`MapData`]). Keys AND values may be heap
    /// objects, so BOTH are traced as GC children (the cached hashes / index are not).
    Map(MapData),
    /// `{a, b, …}` — an insertion-ordered hash set (see [`SetData`]). Elements may be heap objects,
    /// so they are traced as GC children.
    Set(SetData),
    /// Fields stored POSITIONALLY in declaration order (hidden-class / `__slots__` layout — M19
    /// memory-layout lever #1). No per-instance field-name strings: names are resolved on the cold
    /// path (Display / error / probe miss) from `StructDef::fields` via `tid`/`name`, which is the
    /// JIT groundwork — a flat `Vec<Value>` with constant, declaration-order field offsets. `tid` is
    /// the struct type's dense layout id (`StructDef::tid`), stamped at construction so the field IC
    /// can guard on a pure-int compare; `TID_NONE` for a struct whose name isn't a registered type
    /// (never IC-cached). `name` is kept (consumed by method-dispatch / Display / arith / hash).
    Struct {
        name: Box<str>,
        tid: u32,
        fields: Vec<Value>,
    },
    /// An enum variant instance. M19 memory-layout lever #2 — identified by a dense numeric
    /// `variant_id` (`VariantDef::variant_id`), NOT by per-instance type/variant-name `Box<str>`s:
    /// match dispatch / equality / `?` are pure-int compares on this id (the JIT jump-table
    /// groundwork), and the type + variant names resolve from `Program::variants_by_id` on the cold
    /// path only (Display / stringify / error / wire / snap). The enum analogue of `Struct.tid`. Saves
    /// two string allocs per instantiation. `VID_NONE` for an unregistered variant (defensive only —
    /// not constructible from source). `payload` holds the variant's values (GC-traced as children).
    Enum {
        variant_id: u32,
        payload: Vec<Value>,
    },
    /// A named function (top-level `fn` / method) + the module globals it resolves against.
    Func {
        proto: ProtoId,
        home: GcRef,
    },
    /// An anonymous function + its snapshot-captured environment + home globals. M19 lever #3 —
    /// captures are *positional*: `captured[slot]` for the compile-time slot in `Op::GetCaptured`,
    /// populated in the snapshot order recorded by the proto's `capture_names`. No per-instance
    /// names and no per-read string hash (mirrors positional struct fields, lever #1).
    Closure {
        proto: ProtoId,
        captured: Vec<Value>,
        home: GcRef,
    },
    /// A module namespace: its name + top-level bindings. M19 Phase 2b — globals are stored
    /// slot-indexed (`slots[i]` for compile-time slot `i`) for hash-free `GetGlobalSlot` reads; the
    /// `index` (name → slot) backs `module.member` field reads, imports, native population, and errors.
    Module {
        name: Box<str>,
        slots: Vec<Value>,
        index: HashMap<Box<str>, u32>,
    },
    /// A native (Rust) function — a member of a native std module (`std.math` etc., M6c). Holds no
    /// heap references, so it has no GC children.
    Native {
        name: Box<str>,
        func: crate::native::NativeFn,
    },
    /// A dynamic C-ABI FFI function (`extern "lib":`). Wraps the `dlopen`'d library + resolved
    /// symbol + marshalling signature behind an `Arc` (so it is `Send` for `--parallel` and shared
    /// by the M:N snapshot path without re-`dlopen`). Holds no `GcRef`s, so it has no GC children.
    Cffi(Arc<crate::native::cffi::Cffi>),
    /// `Channel[T]` (C2) — a *handle* to the shared mailbox [`ChannelCore`]. B3.1: the queue itself
    /// moved OUT of the heap into the `Arc` (it holds wire-form messages, not `GcRef`s); the heap
    /// keeps only this handle, and two handles can alias one core. `children()` still traces any
    /// `Handle`s embedded in the core's queued messages (e.g. queued closures; B3.3a: `str` messages
    /// queue as owned bytes and root nothing).
    Channel(Arc<ChannelCore>),
    /// `Shared[T]` (C3) — a *handle* to the cross-task mutable box [`SharedCore`] (B3.1). See
    /// [`Channel`](Obj::Channel) for the handle/core split.
    Shared(Arc<SharedCore>),
    /// `Atomic[T]` — a *handle* to the cross-task atomic box [`AtomicCore`]. Same handle/core split as
    /// [`Shared`](Obj::Shared); the core holds one boxed wire value behind a `Mutex`.
    Atomic(Arc<AtomicCore>),
    /// `Executor` (C5 escape hatch) — a *handle* to the shared work queue [`ExecutorCore`] (B3.1).
    /// The queued task closures live in the core as wire values (`Handle(closure)` at B3.1); `shut`
    /// lives in the shared core so aliasing handles agree on shutdown state. See
    /// [`Channel`](Obj::Channel).
    Executor(Arc<ExecutorCore>),
    /// `Socket` (D6) — a *handle* to a non-blocking connected TCP stream [`SocketCore`]. Same
    /// handle/core split as [`Channel`](Obj::Channel); the core holds an OS fd (no `WireValue`s, no
    /// `GcRef`s), so it traces no GC children.
    Socket(Arc<SocketCore>),
    /// `Listener` (D6) — a *handle* to a non-blocking accepting socket [`ListenerCore`]. See
    /// [`Socket`](Obj::Socket).
    Listener(Arc<ListenerCore>),
    /// Experimental generators (VM-only) — a suspendable coroutine produced by calling a `yield`-ing
    /// function. Holds its bytecode + home/closure + lifecycle state + parked execution context; its
    /// `.next()` is intrinsic (see `Vm::generator_next`). Boxed to keep `Obj` small (the core carries
    /// `Vec`s of frames/stack). GC children come from [`super::GeneratorCore::gc_roots`].
    Generator(Box<super::GeneratorCore>),
}

/// One heap slot: the object (or a hole, for swept/free slots) + its GC mark bit.
#[derive(Debug)]
struct Slot {
    obj: Option<Obj>,
    mark: bool,
}

/// Smallest GC threshold — don't collect until at least this many objects have been allocated
/// since the last collection (avoids thrashing on tiny programs).
const MIN_GC_THRESHOLD: usize = 256;

/// The object heap: a slab of slots with a free-list for reuse + mark-sweep bookkeeping.
///
/// The collector itself (root tracing) lives on the VM, which owns the roots; the heap provides
/// the slot-level primitives: [`mark`](Heap::mark), [`children`](Heap::children),
/// [`sweep`](Heap::sweep), and the allocation-driven [`should_collect`](Heap::should_collect)
/// trigger. Collection runs only at instruction boundaries (see `Vm::run_until`), so every live
/// value is reachable from the VM roots — there are no mid-opcode temporaries to miss.
#[derive(Debug)]
pub struct Heap {
    slots: Vec<Slot>,
    free: Vec<u32>,
    /// Live (allocated, not freed) object count.
    live: usize,
    /// Allocations since the last collection — drives the growth-threshold trigger.
    since_gc: usize,
    /// Collect once `since_gc` reaches this; grows with the live set after each collection.
    next_gc: usize,
}

impl Default for Heap {
    fn default() -> Self {
        Heap {
            slots: Vec::new(),
            free: Vec::new(),
            live: 0,
            since_gc: 0,
            next_gc: MIN_GC_THRESHOLD,
        }
    }
}

impl Heap {
    pub fn new() -> Self {
        Heap::default()
    }

    /// Allocate an object, returning its handle. Reuses a free slot when available.
    pub fn alloc(&mut self, obj: Obj) -> GcRef {
        self.live += 1;
        self.since_gc += 1;
        if let Some(idx) = self.free.pop() {
            let slot = &mut self.slots[idx as usize];
            slot.obj = Some(obj);
            slot.mark = false;
            GcRef(idx)
        } else {
            let idx = self.slots.len() as u32;
            self.slots.push(Slot {
                obj: Some(obj),
                mark: false,
            });
            GcRef(idx)
        }
    }

    pub fn get(&self, h: GcRef) -> &Obj {
        self.slots[h.0 as usize]
            .obj
            .as_ref()
            .expect("dangling GcRef (object was collected while still reachable)")
    }

    pub fn get_mut(&mut self, h: GcRef) -> &mut Obj {
        self.slots[h.0 as usize]
            .obj
            .as_mut()
            .expect("dangling GcRef (object was collected while still reachable)")
    }

    /// Live object count — for GC assertions / bounded-heap tests.
    #[cfg(test)]
    pub fn live(&self) -> usize {
        self.live
    }

    /// Whether enough has been allocated since the last collection to warrant one.
    pub fn should_collect(&self) -> bool {
        self.since_gc >= self.next_gc
    }

    /// Mark one object reachable. Returns `true` if it was *newly* marked (caller should then
    /// trace its children), `false` if already marked (a cycle / shared reference — stop).
    pub fn mark(&mut self, h: GcRef) -> bool {
        let slot = &mut self.slots[h.0 as usize];
        if slot.mark {
            false
        } else {
            slot.mark = true;
            true
        }
    }

    /// The heap handles directly referenced by an object (for the mark worklist).
    pub fn children(&self, h: GcRef) -> Vec<GcRef> {
        let mut out = Vec::new();
        let mut push = |v: &Value| {
            if let Value::Obj(c) = v {
                out.push(*c);
            }
        };
        match self.get(h) {
            Obj::Str(_) => {}
            // A GC leaf: raw bytes, no embedded `GcRef`s, so nothing to trace (like `Str`).
            Obj::Bytes(_) => {}
            // Also a GC leaf — the mutable `bytearray` still holds only raw `u8`s, never a `GcRef`.
            Obj::ByteArray(_) => {}
            // NON-LEAF: the cursor's snapshot may hold heap `GcRef`s, which must stay alive while the
            // cursor is reachable (a not-yet-consumed element is still owned by the cursor).
            Obj::Iter { items, .. } => items.iter().for_each(&mut push),
            Obj::List(items) => items.iter().for_each(&mut push),
            Obj::Tuple(items) => items.iter().for_each(&mut push),
            Obj::Map(m) => m.entries.iter().for_each(|(_, k, v)| {
                push(k);
                push(v);
            }),
            Obj::Set(s) => s.entries.iter().for_each(|(_, e)| push(e)),
            Obj::Struct { fields, .. } => fields.iter().for_each(&mut push),
            Obj::Enum { payload, .. } => payload.iter().for_each(&mut push),
            Obj::Func { home, .. } => out.push(*home),
            Obj::Closure { captured, home, .. } => {
                captured.iter().for_each(&mut push);
                out.push(*home);
            }
            Obj::Module { slots, .. } => slots.iter().for_each(&mut push),
            Obj::Native { .. } => {}
            Obj::Cffi(_) => {}
            // B3.1: the core lives in an `Arc` outside this heap and holds `WireValue`s, but those
            // can still carry `Handle(GcRef)`s into *this* heap (an `Executor`'s queued closures and
            // any core nested inside another core; B3.3a: `str` messages queue as owned bytes, rooting
            // nothing). Trace every
            // reachable embedded handle so those objects stay rooted while the core holds them; `seen`
            // breaks `Arc` cycles. (The doc's "drop these arms" is wrong at B3.1 — closures can't
            // cross by value until B3.3/G1, so cores still hold heap refs.)
            Obj::Channel(core) => {
                let mut seen = vec![Arc::as_ptr(core) as usize];
                for w in core.q.lock().unwrap().queue.iter() {
                    crate::vm::core::collect_core_gcrefs(w, &mut out, &mut seen);
                }
            }
            Obj::Shared(core) => {
                let mut seen = vec![Arc::as_ptr(core) as usize];
                crate::vm::core::collect_core_gcrefs(&core.v.lock().unwrap(), &mut out, &mut seen);
            }
            Obj::Atomic(core) => {
                let mut seen = vec![Arc::as_ptr(core) as usize];
                crate::vm::core::collect_core_gcrefs(&core.v.lock().unwrap(), &mut out, &mut seen);
            }
            Obj::Executor(core) => {
                let mut seen = vec![Arc::as_ptr(core) as usize];
                for w in core.inner.lock().unwrap().queue.iter() {
                    crate::vm::core::collect_core_gcrefs(w, &mut out, &mut seen);
                }
            }
            // D6: a socket/listener core holds only an OS fd + a poll key — no heap refs to trace.
            Obj::Socket(_) | Obj::Listener(_) => {}
            // Experimental generators — root the suspended frames/stack/args (see `gc_roots`).
            Obj::Generator(g) => out.extend(g.gc_roots()),
        }
        out
    }

    /// Free every unmarked object and clear all marks for the next cycle. Resets the allocation
    /// counter and grows the next-collection threshold relative to the surviving set.
    pub fn sweep(&mut self) {
        for (idx, slot) in self.slots.iter_mut().enumerate() {
            if slot.obj.is_some() {
                if slot.mark {
                    slot.mark = false;
                } else {
                    slot.obj = None;
                    self.free.push(idx as u32);
                    self.live -= 1;
                }
            }
        }
        self.since_gc = 0;
        self.next_gc = (self.live * 2).max(MIN_GC_THRESHOLD);
    }
}

#[cfg(test)]
mod iter_obj_tests {
    use super::*;

    /// `Obj::Iter` must stay within the 88B cap (Vec 24B + usize 8B = 32B, `Module` still dominates).
    #[test]
    fn obj_iter_within_size_cap() {
        assert_eq!(std::mem::size_of::<Obj>(), 88);
    }

    /// A cursor is NON-LEAF: `children()` must yield its snapshot's heap refs so a not-yet-consumed
    /// element survives a GC pass (contrast `Bytes`/`ByteArray`, which trace nothing).
    #[test]
    fn obj_iter_traces_items_as_gc_children() {
        let mut heap = Heap::new();
        // A heap element the cursor must keep alive.
        let elem = heap.alloc(Obj::Str("kept".into()));
        let cursor = heap.alloc(Obj::Iter { items: vec![Value::Obj(elem), Value::Int(7)], pos: 0 });
        let kids = heap.children(cursor);
        assert!(kids.contains(&elem), "cursor must trace its heap-ref items, got {kids:?}");
        // Mark only the cursor as a root, trace, sweep — the element must survive.
        heap.mark(cursor);
        for c in heap.children(cursor) {
            heap.mark(c);
        }
        heap.sweep();
        assert!(matches!(heap.get(elem), Obj::Str(s) if &**s == "kept"));
    }
}
