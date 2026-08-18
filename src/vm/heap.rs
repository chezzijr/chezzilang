//! The GC-managed heap (M5).
//!
//! M5a: `alloc` only inserts (the slot/free-list machinery is in place; the mark-sweep collector
//! lands in M5b). Objects are addressed by [`GcRef`] (a slot index), so handle copies alias one
//! object. The VM owns the heap and mutates objects through `&mut heap[h]` — no `RefCell` needed.

use super::chzstr::ChzStr;
use super::core::{
    AtomicCore, AtomicIntCore, ChannelCore, ExecutorCore, ListenerCore, ReaderCore, RwSharedCore,
    SharedCore, SocketCore, WriterCore,
};
use super::fxhash::FxHashMap;
use super::op::ProtoId;
use super::value::{GcRef, Value};
use std::cell::Cell;
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

/// A module namespace's payload — boxed behind [`Obj::Module`] so the rare, cold `Module` variant
/// doesn't set the `Obj` size cap (mirrors [`Obj::Generator`]'s `Box<GeneratorCore>`). Its name +
/// slot-indexed top-level bindings (`slots[i]` for compile-time slot `i`) + the `index` (name → slot)
/// that backs `module.member` reads, imports, native population, and errors. See [`Obj::Module`].
#[derive(Debug, Clone)]
pub struct ModuleData {
    pub name: Box<str>,
    pub slots: Vec<Value>,
    pub index: HashMap<Box<str>, u32>,
}

/// Struct field storage: ≤3 fields inline (no second heap alloc), more spill to a boxed slice.
/// Fields are FIXED at construction (positional hidden-class layout; no `.push/insert/resize`
/// growth sites), so this never needs `Vec`'s capacity slot. `Value` is 8B → `[Value; 3]` is 24B,
/// and the vast majority of structs (≤3 fields) fold their fields into the `Obj` slot itself — zero
/// second malloc. `>3` fields spill to a boxed slice. Keeps `Obj` at the 64B cap (M19 lever #1).
#[derive(Debug, Clone)]
pub enum Fields {
    /// ≤3 fields folded inline; `vals[len..]` are unused padding holding `Value::nil()`.
    Inline { len: u8, vals: [Value; 3] },
    /// >3 fields in a heap-allocated exact-length boxed slice (no spare capacity).
    Spill(Box<[Value]>),
}

impl Fields {
    /// Build from a positional field vec: ≤3 → `Inline`, else `Spill`.
    #[inline]
    pub fn from_vec(v: Vec<Value>) -> Self {
        if v.len() <= 3 {
            let len = v.len() as u8;
            let mut vals = [Value::nil(); 3];
            for (i, x) in v.into_iter().enumerate() {
                vals[i] = x;
            }
            Fields::Inline { len, vals }
        } else {
            Fields::Spill(v.into_boxed_slice())
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        match self {
            Fields::Inline { len, .. } => *len as usize,
            Fields::Spill(s) => s.len(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn as_slice(&self) -> &[Value] {
        match self {
            Fields::Inline { len, vals } => &vals[..*len as usize],
            Fields::Spill(s) => s,
        }
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [Value] {
        match self {
            Fields::Inline { len, vals } => &mut vals[..*len as usize],
            Fields::Spill(s) => s,
        }
    }

    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, Value> {
        self.as_slice().iter()
    }

    #[inline]
    pub fn get(&self, i: usize) -> Option<&Value> {
        self.as_slice().get(i)
    }

    #[inline]
    pub fn get_mut(&mut self, i: usize) -> Option<&mut Value> {
        self.as_mut_slice().get_mut(i)
    }

    /// Owned-backing byte footprint for `live_bytes`: `Inline` folds into the `Obj` slot (0 extra
    /// heap); `Spill` counts its boxed slice's `len * size_of::<Value>()`.
    #[inline]
    pub fn heap_bytes(&self) -> usize {
        match self {
            Fields::Inline { .. } => 0,
            Fields::Spill(s) => s.len() * std::mem::size_of::<Value>(),
        }
    }
}

impl std::ops::Index<usize> for Fields {
    type Output = Value;
    #[inline]
    fn index(&self, i: usize) -> &Value {
        &self.as_slice()[i]
    }
}

impl std::ops::IndexMut<usize> for Fields {
    #[inline]
    fn index_mut(&mut self, i: usize) -> &mut Value {
        &mut self.as_mut_slice()[i]
    }
}

/// A heap object — the reference half of the value space.
#[derive(Debug, Clone)]
pub enum Obj {
    Str(ChzStr),
    /// `bytes` — an immutable heap byte sequence (Python `bytes` model). A GC LEAF: it holds only
    /// raw `u8`s (no `GcRef`s), so `children()` returns nothing — it is marked reachable but traces
    /// no children, exactly like `Str`/`Native`. `Box<[u8]>` is 16B, well within the 64B `Obj` cap.
    Bytes(Box<[u8]>),
    /// `bytearray` — the MUTABLE sibling of `bytes` (Python `bytearray` model). Storage is a `Vec<u8>`
    /// mutated IN PLACE through the `GcRef` heap slot (`heap.get_mut`), exactly like [`List`](Obj::List),
    /// so two bindings to the same `bytearray` observe each other's writes. Still a GC LEAF — raw `u8`s,
    /// no `GcRef`s — so `children()` traces nothing (the difference vs `Bytes` is the mutability of the
    /// slot, not GC reachability). `Vec<u8>` is 24B (= `List`'s `Vec<Value>`), within the 64B `Obj` cap.
    ByteArray(Vec<u8>),
    /// A heap-boxed `i64` outside the ±2^62 inline-`Value` range (8B-`Value` milestone). A GC LEAF:
    /// holds one raw `i64`, no `GcRef`s, so `children()` traces nothing (like `Bytes`). Immutable, so
    /// aliasing two `Value`s to one box is invisible. UNUSED until the 8B-`Value` swap (Phase 1); the
    /// behavior arms exist so a `BigInt(n)` behaves identically to the inline `Int(n)`.
    BigInt(i64),
    /// A heap-boxed `f64` (8B-`Value` milestone: floats do not fit inline). A GC LEAF: one raw `f64`,
    /// no `GcRef`s. Immutable, so aliasing two `Value`s to one box is invisible. UNUSED until the
    /// 8B-`Value` swap (Phase 1); the behavior arms exist so a `FloatBox(f)` behaves identically to
    /// the inline `Float(f)`.
    FloatBox(f64),
    /// `Iter` — a composable cursor (the `Iterable[T]` `.iter()` result), the heap payload behind the
    /// existential `Iterator[T]` type. A frozen SNAPSHOT (`items`) of a collection's contents at the
    /// instant `.iter()` was called, plus a read `pos`. `.next()` returns `Some(items[pos])` and
    /// advances; `None` (idempotent) past the end. NON-LEAF (unlike `Bytes`/`ByteArray`): `items` may
    /// hold heap `GcRef`s (a list of structs, a set of strings, …), so `children()` MUST trace them
    /// or the snapshot's elements get collected out from under a live cursor. `Vec<Value>`(24) +
    /// `usize`(8) = 32B, within the 64B `Obj` cap (`MapData`/`SetData` set the cap).
    Iter {
        items: Vec<Value>,
        pos: usize,
    },
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
    /// memory-layout lever #1). No per-instance field-name strings AND no per-instance type-name
    /// string: names are resolved on the cold path (method-dispatch / Display / arith / hash / error /
    /// wire / snap) from `tid` — the type name via `Program::struct_names` (`struct_name_of_tid`), the
    /// field names from `StructDef::fields`. This is the JIT groundwork — a flat `Vec<Value>` with
    /// constant, declaration-order field offsets. `tid` is the struct type's dense layout id
    /// (`StructDef::tid`), stamped at construction so the field IC can guard on a pure-int compare;
    /// `TID_NONE` for a struct whose name isn't a registered type (never IC-cached, resolves to `"?"`).
    /// The struct analogue of `Enum.variant_id`. Saves one string alloc per instantiation.
    Struct {
        tid: u32,
        fields: Fields,
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
    /// A `newtype` instance — a DISTINCT nominal wrapper around a single `inner` value (the heap
    /// analogue of a 1-field `Struct`). `type_key` is the newtype's runtime key. `inner` may be a heap
    /// object, so it is GC-traced as a child. Method/protocol/hash/str dispatch reuses the struct/enum
    /// paths via `Program::newtype_methods`; scalar operators unwrap→primitive-op→rewrap.
    NewType {
        type_key: Box<str>,
        inner: Value,
    },
    /// A heap-allocated mutable CELL — a single boxed `Value` behind a `GcRef`, the runtime home of a
    /// by-reference-captured local (uniform-capture feature, Task A). Two bindings holding the same
    /// cell handle observe each other's writes (`CellStore`), exactly like `List`/`bytearray` mutate
    /// through the shared slot. NON-LEAF: the inner value may be a heap object, so `children()` traces
    /// it (like a 1-field `NewType`). `Value` is 16B, well within the 64B `Obj` cap.
    Cell(Value),
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
    /// Boxed ([`ModuleData`]) so this rare/cold variant doesn't set the `Obj` size cap (like
    /// [`Generator`](Obj::Generator)) — one cold pointer hop off the module-member path.
    Module(Box<ModuleData>),
    /// A native (Rust) function — a member of a native std module (`std.math` etc., M6c). Holds no
    /// heap references, so it has no GC children.
    ///
    /// `kind` is the member's [`crate::native::Kind`], copied off its registry entry when the module
    /// binds (`vm/exec.rs`). It travels with the value so `Vm::invoke_native` knows how to RUN this
    /// native (inline / dirty-pool offload / timed wait / engine-intercepted) without comparing its
    /// name to a string — `docs/future.md` §3c.
    Native {
        name: Box<str>,
        func: crate::native::NativeFn,
        kind: crate::native::Kind,
    },
    /// A first-class UNIVERSE builtin FUNCTION value (`print`/`ord`/`chr`/`panic`), carried by name.
    /// Produced by `Op::LoadBuiltin` when the name is used in value position; calling it (via
    /// `invoke_value`) routes back into the same builtin logic direct calls use. A GC LEAF (holds
    /// only a name `Box<str>`, no `GcRef`s), like `Native`. SENDABLE (pure code) — crosses the airlock
    /// by cloning the name. The interp twin is `Value::Builtin`.
    Builtin(Box<str>),
    /// A dynamic C-ABI FFI function (`extern "lib":`). Wraps the `dlopen`'d library + resolved
    /// symbol + marshalling signature behind an `Arc` (so it is `Send` for `--parallel` and shared
    /// by the M:N snapshot path without re-`dlopen`). Holds no `GcRef`s, so it has no GC children.
    Cffi(Arc<crate::native::cffi::Cffi>),
    /// An opaque C-ABI pointer handle (`ptr`) — a raw `void*` address carried as a `usize`. Produced
    /// by an `extern "lib":` fn returning `ptr` and by `std.ffi.null()`. Untyped, never auto-freed
    /// (the program calls the library's own destroy), and holds no `GcRef`s, so it has no GC children.
    /// `Send` (a plain address), so it crosses the spawn/channel airlock by value.
    Ptr(usize),
    /// `Channel[T]` (C2) — a *handle* to the shared mailbox [`ChannelCore`]. B3.1: the queue itself
    /// moved OUT of the heap into the `Arc` (it holds wire-form messages, not `GcRef`s); the heap
    /// keeps only this handle, and two handles can alias one core. `children()` still traces any
    /// `Handle`s embedded in the core's queued messages (e.g. queued closures; B3.3a: `str` messages
    /// queue as owned bytes and root nothing).
    Channel(Arc<ChannelCore>),
    /// `Shared[T]` (C3) — a *handle* to the cross-task mutable box [`SharedCore`] (B3.1). See
    /// [`Channel`](Obj::Channel) for the handle/core split.
    Shared(Arc<SharedCore>),
    /// `RwShared[T]` — a *handle* to the cross-task read-write box [`RwSharedCore`]. Same handle/core
    /// split as [`Shared`](Obj::Shared); the core holds one boxed wire value behind a `RwLock`.
    RwShared(Arc<RwSharedCore>),
    /// `Atomic[T]` — a *handle* to the cross-task atomic box [`AtomicCore`]. Same handle/core split as
    /// [`Shared`](Obj::Shared); the core holds one boxed wire value behind a `Mutex`.
    Atomic(Arc<AtomicCore>),
    /// `AtomicInt` — a *handle* to the monomorphic LOCK-FREE int atomic [`AtomicIntCore`]. Same
    /// handle/core split as [`Atomic`](Obj::Atomic), but the core is a raw `AtomicI64` (no lock).
    AtomicInt(Arc<AtomicIntCore>),
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
    /// `Writer` (R2) — a *handle* to a write-only file/stream [`WriterCore`]. Same handle/core split as
    /// [`Socket`](Obj::Socket); the core holds an fd/buffer (no `WireValue`s, no `GcRef`s), so it traces
    /// no GC children.
    Writer(Arc<WriterCore>),
    /// `Reader` (R2b) — a *handle* to a read-only file [`ReaderCore`]. Same handle/core split as
    /// [`Writer`](Obj::Writer)/[`Socket`](Obj::Socket); the core holds a `BufReader<File>` (no
    /// `WireValue`s, no `GcRef`s), so it traces no GC children.
    Reader(Arc<ReaderCore>),
    /// Experimental generators (VM-only) — a suspendable coroutine produced by calling a `yield`-ing
    /// function. Holds its bytecode + home/closure + lifecycle state + parked execution context; its
    /// `.next()` is intrinsic (see `Vm::generator_next`). Boxed to keep `Obj` small (the core carries
    /// `Vec`s of frames/stack). GC children come from [`super::GeneratorCore::gc_roots`].
    Generator(Box<super::GeneratorCore>),
}

/// One heap slot: the object, or a hole for swept/free slots. Exactly 64B (`Option<Obj>` niche-packs
/// `None` free). The GC mark bit is NOT here — it lives in [`Heap::marks`], a dense parallel bitset,
/// so the `mark:bool` no longer pads the slot from 64B to 72B (the memory win). (Sweep still scans
/// every slot's `obj` to find garbage — the bitset does not avoid that; it saves the per-slot byte.)
#[derive(Debug)]
struct Slot {
    obj: Option<Obj>,
}

/// Smallest GC threshold — don't collect until at least this many objects have been allocated
/// since the last collection (avoids thrashing on tiny programs).
const MIN_GC_THRESHOLD: usize = 256;

/// One object's OWNED BACKING in bytes (`Vec`/`Box` capacities), excluding the fixed
/// `size_of::<Obj>()` slot cost that every live slot pays alike. The single sizing table for the
/// heap: `bytes_in` (the `live_bytes`/`own_bytes` walk), `alloc` and `get_mut`'s settle all read it,
/// so a new `Obj` variant is sized once instead of drifting between three copies.
///
/// **Core arms deliberately score 0, and take NO lock.** A `Channel`/`Shared`/`RwShared`/`Atomic`/
/// `Executor` payload lives in an `Arc` outside every heap and is already charged by
/// `Vm::to_wire_crossable`; `bytes_in` adds its own once-per-`Arc` walk on top for the reachability
/// question. Reaching `core.inner`/`core.q` from inside `get_mut` would re-take a lock the caller may
/// already hold — `std::sync::Mutex` is not reentrant, and that exact self-deadlock (a job capturing
/// its own executor: hang, rc=124, under a cap only) is why `Heap::own_bytes` exists. Do not add a
/// core arm here.
fn obj_bytes_shallow(obj: &Obj) -> usize {
    match obj {
        Obj::Str(s) => s.as_str().len(),
        Obj::Bytes(b) => b.len(),
        Obj::ByteArray(b) => b.capacity(),
        Obj::Iter { items, .. } => items.capacity() * std::mem::size_of::<Value>(),
        Obj::List(v) | Obj::Tuple(v) => v.capacity() * std::mem::size_of::<Value>(),
        Obj::Struct { fields, .. } => fields.heap_bytes(),
        Obj::Enum { payload, .. } => payload.capacity() * std::mem::size_of::<Value>(),
        Obj::Closure { captured, .. } => captured.capacity() * std::mem::size_of::<Value>(),
        Obj::Module(m) => m.slots.capacity() * std::mem::size_of::<Value>(),
        // Map/Set: entries + the index cost; approximate by entries backing only.
        Obj::Map(m) => m.entries.capacity() * std::mem::size_of::<(u64, Value, Value)>(),
        Obj::Set(s) => s.entries.capacity() * std::mem::size_of::<(u64, Value)>(),
        _ => 0,
    }
}

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
    /// GC mark bits, one per slot index (bit `i & 63` of word `i >> 6`). A dense parallel bitset
    /// grown in lockstep with `slots` — pulling the bit out of `Slot` drops it 72B→64B (the mark
    /// test-and-set also touches a compact word rather than a scattered slot byte). Post-sweep
    /// invariant: all bits 0 (survivors cleared, holes never marked).
    marks: Vec<u64>,
    free: Vec<u32>,
    /// Live (allocated, not freed) object count.
    live: usize,
    /// Allocations since the last collection — drives the growth-threshold trigger.
    since_gc: usize,
    /// BYTES charged since the last collection — the `since_gc` sibling for growth that allocates
    /// (almost) no `Obj`s. A monotonic pacing **HINT**, never accounting: it only decides WHEN to
    /// sample, `live_bytes()` remains the sole measure of what is live (so a replacing store charges,
    /// and a `recv`/`pop` never decrements — under-triggering leaves the `--max-heap` guard failing
    /// open, over-triggering just costs a sweep). Read by
    /// [`should_collect`](Heap::should_collect) ONLY when `mem_cap != 0`, so cap-off pacing is
    /// bit-for-bit the object-count trigger it has always been. `Cell` because two of the three
    /// charge points hold `&self`.
    ///
    /// Every byte a heap gains arrives one of exactly three ways, and each has exactly one funnel:
    /// a NEW object → [`alloc`](Heap::alloc); growth IN PLACE inside an existing one →
    /// [`get_mut`](Heap::get_mut) (see `pending_mut`); an off-heap wire payload →
    /// `Vm::to_wire_crossable`. All three charge here.
    ///
    /// It has to count BYTES and not events. Instructions, allocations and wire crossings are all
    /// proxies, and each has a shape that adds unbounded bytes without moving it: an instruction tick
    /// is defeated by `big.extend(chunk)` (measured ~240 MB past an 8 MB cap in ~1200 instructions),
    /// the object count by `s = s + s` ×22 (41 MB in 22 allocations) and by
    /// `"x".repeat(20000000)` (20 MB in ONE).
    since_gc_bytes: Cell<usize>,
    /// The in-place-growth half of `since_gc_bytes`: the object handed out by the last
    /// [`get_mut`](Heap::get_mut) and its payload size AT THAT MOMENT. The next settle — the next
    /// `get_mut`, or [`should_collect`](Heap::should_collect) — re-measures the same object and
    /// charges the difference. `Cell` because `should_collect` holds `&self`.
    ///
    /// This is the door for byte growth that allocates NOTHING: `xs.push(i)` /
    /// `m[k] = v` / `s.add(x)` / `ba.push(b)` / `big.extend(chunk)` all append into the `Vec` behind
    /// an EXISTING `Obj`, so neither the object count nor the wire counter moves and `sweep()` never
    /// runs — `over_cap` is assigned nowhere else, so the cap failed OPEN. Measured on the release
    /// binary against `--max-heap=8000000`: an 80 M-element `List[int]` grown by `push` PASSED at
    /// 617.8 MB (**77× the cap**), and one `big.extend(chunk)` loop PASSED at ~240 MB in ~1200
    /// instructions.
    ///
    /// Why here and not at each growth site: `push`/`insert`/`add`/`extend`/the map index-store/… is
    /// an open N-way set, and charging some arms of it is the mistake this repo has already been
    /// bitten by twice (`docs/gaps.md` W7-22). `get_mut` is the SOLE `&mut Obj` door (verified: no
    /// `.obj.as_mut()` or `slots[..]` access exists outside this file), so a charge here cannot be
    /// forgotten by a new container method — the same forget-proof argument that put the wire charge
    /// in `Vm::to_wire_crossable`.
    ///
    /// Why deferred rather than measured on both sides of the call: `get_mut` HANDS OUT the `&mut`,
    /// so the "after" size does not exist until the caller is done with it. Settling at the next door
    /// (or at the next `should_collect`, which is every instruction) is what makes it exact without a
    /// wrapper at 65 call sites.
    ///
    /// Sound against a freed slot because `sweep()` — the only thing that frees, and so the only
    /// thing that can make `alloc` re-hand this slot to a different object — clears it.
    pending_mut: Cell<Option<(GcRef, usize)>>,
    /// Collect once `since_gc` reaches this; grows with the live set after each collection.
    next_gc: usize,
    /// Peak of `live_bytes()` sampled at each `sweep()` — the memory probe's high-water mark
    /// (reported behind `CHEZZI_HEAP_STATS=1`). The Phase-1 8B-`Value` gate compares this.
    peak_live_bytes: usize,
    /// `chezzi test --max-heap` — the per-test live-heap cap in bytes (`0` = OFF, the default; the
    /// `chezzi run` engine never sets it). Checked once per `sweep()` against the `lb` already
    /// computed for `peak_live_bytes`, so it costs one `!= 0 &&` on the common (cap-off) path.
    mem_cap: usize,
    /// Re-evaluated by every `sweep()` to `mem_cap != 0 && live_bytes() > mem_cap` — the VM reads it at
    /// its GC boundary to hard-abort the running test, re-observed each sweep like a cancel checkpoint
    /// (so a runaway `defer` during the abort unwind re-trips it); cleared per test by `clear_over_cap`.
    over_cap: bool,
    /// W6-10s — force the next [`should_collect`](Heap::should_collect), regardless of object count.
    /// Set only under a live cap, by [`request_collect`](Heap::request_collect), for a heap that is
    /// handed a large payload in very few `Obj`s (a worker's rebuilt task args/captures) — the
    /// object-count trigger cannot see those bytes, so without this nobody ever samples the cap.
    /// Cleared by `sweep()`.
    force_collect: bool,
}

impl Default for Heap {
    fn default() -> Self {
        Heap {
            slots: Vec::new(),
            marks: Vec::new(),
            free: Vec::new(),
            live: 0,
            since_gc: 0,
            since_gc_bytes: Cell::new(0),
            pending_mut: Cell::new(None),
            next_gc: MIN_GC_THRESHOLD,
            peak_live_bytes: 0,
            mem_cap: 0,
            over_cap: false,
            force_collect: false,
        }
    }
}

impl Heap {
    pub fn new() -> Self {
        Heap::default()
    }

    /// Test the mark bit for slot `i` (absent word → unmarked).
    #[inline]
    fn is_marked(&self, i: usize) -> bool {
        self.marks
            .get(i >> 6)
            .is_some_and(|w| (w >> (i & 63)) & 1 == 1)
    }

    /// Set the mark bit for slot `i` (word `i >> 6` must already exist — grown at alloc).
    #[inline]
    fn set_mark(&mut self, i: usize) {
        self.marks[i >> 6] |= 1u64 << (i & 63);
    }

    /// Clear the mark bit for slot `i` (no-op if the word is absent).
    #[inline]
    fn clear_mark(&mut self, i: usize) {
        if let Some(w) = self.marks.get_mut(i >> 6) {
            *w &= !(1u64 << (i & 63));
        }
    }

    /// Allocate an object, returning its handle. Reuses a free slot when available.
    pub fn alloc(&mut self, obj: Obj) -> GcRef {
        self.live += 1;
        self.since_gc += 1;
        // Byte funnel 1 of 3 (see `since_gc_bytes`): the only constructor. `since_gc` alone paces
        // MANY-small-object growth; this paces FEW-huge-object growth, which it cannot see —
        // `"x".repeat(20000000)` is one allocation carrying 20 MB, and `s = s + s` ×22 is 22 of them
        // carrying 41 MB, both well under the 256-object threshold.
        if self.mem_cap != 0 {
            self.charge_bytes(obj_bytes_shallow(&obj));
        }
        if let Some(idx) = self.free.pop() {
            self.slots[idx as usize].obj = Some(obj);
            self.clear_mark(idx as usize); // defensive: already 0 post-sweep (matches old mark=false)
            GcRef(idx)
        } else {
            let idx = self.slots.len() as u32;
            self.slots.push(Slot { obj: Some(obj) });
            // Grow `marks` in lockstep so word `idx>>6` exists for a later `set_mark`.
            while self.marks.len() <= (idx as usize) >> 6 {
                self.marks.push(0);
            }
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
        // Byte funnel 2 of 3 (see `since_gc_bytes` / `pending_mut`): the sole `&mut Obj` door, and so
        // the sole door for growth INSIDE an existing object. Settle whatever the previous `get_mut`
        // handed out, then arm this one.
        if self.mem_cap != 0 {
            self.settle_pending_mut();
            self.pending_mut
                .set(Some((h, obj_bytes_shallow(self.get(h)))));
        }
        self.slots[h.0 as usize]
            .obj
            .as_mut()
            .expect("dangling GcRef (object was collected while still reachable)")
    }

    /// Charge the growth of the object the last [`get_mut`](Heap::get_mut) handed out, and disarm.
    /// A SHRINK charges 0 — monotonic, exactly like the wire counter (`live_bytes()` is what
    /// measures; this only decides when to look).
    #[inline]
    fn settle_pending_mut(&self) {
        if let Some((h, before)) = self.pending_mut.take() {
            let now = obj_bytes_shallow(self.get(h));
            self.charge_bytes(now.saturating_sub(before));
        }
    }

    /// Live object count — for GC assertions / bounded-heap tests.
    #[cfg(test)]
    pub fn live(&self) -> usize {
        self.live
    }

    /// Charge bytes against the collection trigger (see `since_gc_bytes`). Byte funnel 3 of 3 is
    /// `Vm::to_wire_crossable`, the one helper every cross-heap value store routes through; the other
    /// two callers are [`alloc`](Heap::alloc) and [`get_mut`](Heap::get_mut)'s settle. Only ever
    /// reached when a `--max-heap` cap is live.
    #[inline]
    pub fn charge_bytes(&self, n: usize) {
        self.since_gc_bytes
            .set(self.since_gc_bytes.get().saturating_add(n));
    }

    /// How many charged bytes force a sweep under a live cap: a quarter of the cap bounds the
    /// overshoot between samples, and the 64 KB floor stops a tiny cap from GC-ing per store.
    fn bytes_gc_threshold(&self) -> usize {
        (self.mem_cap / 4).max(64 * 1024)
    }

    /// Whether enough has been allocated since the last collection to warrant one.
    ///
    /// W6-10 (sampling half): under a live `--max-heap` cap, BYTE growth ALSO paces a sweep. Without
    /// it a program that grows megabytes while allocating ~2 `Obj`s per iteration never reaches the
    /// object-count threshold, so `sweep()` never runs, `over_cap` is never evaluated and the cap
    /// fails OPEN (counting the bytes correctly in `live_bytes` does nothing if nobody ever looks).
    ///
    /// W7-28 — the byte counter now covers ALL THREE ways a heap gains bytes, not just the off-heap
    /// wire one: see `since_gc_bytes`. The settle here is what makes `get_mut`'s deferred charge
    /// exact at an instruction boundary, which is the only place the cap is read.
    ///
    /// Gated on `mem_cap != 0`: with no cap `over_cap` is meaningless anyway, and pacing stays
    /// bit-for-bit the object-count trigger it has always been.
    pub fn should_collect(&self) -> bool {
        if self.force_collect || self.since_gc >= self.next_gc {
            return true;
        }
        if self.mem_cap == 0 {
            return false;
        }
        self.settle_pending_mut();
        self.since_gc_bytes.get() >= self.bytes_gc_threshold()
    }

    /// W6-10s — force the next [`should_collect`](Heap::should_collect), so a heap that was HANDED a
    /// large payload samples the `--max-heap` cap even though it allocated almost no `Obj`s doing so.
    /// `over_cap` is assigned only in `sweep()`, and `sweep()` only runs when `should_collect()`
    /// fires; a worker heap holding a rebuilt `List` of any size counts ONE object, so the growth
    /// trigger never moves and the guard fails OPEN. Set by `Vm::spawn_worker` under a live cap only
    /// (so cap-off pacing is untouched), and consumed at the task's first instruction boundary in
    /// `run_until` — the first point where every live value is properly rooted.
    ///
    /// This is the guard for the [`ReadyWorker::invoke`] door (eager `Executor` jobs); the `spawn` /
    /// `parallel:` fiber door is sampled before dispatch by `Vm::sample_mem_cap` instead. Neither
    /// subsumes the other — see the two-door note in `Vm::spawn_worker`.
    pub fn request_collect(&mut self) {
        self.force_collect = true;
    }

    /// Mark one object reachable. Returns `true` if it was *newly* marked (caller should then
    /// trace its children), `false` if already marked (a cycle / shared reference — stop).
    pub fn mark(&mut self, h: GcRef) -> bool {
        let i = h.0 as usize;
        if self.is_marked(i) {
            false
        } else {
            self.set_mark(i);
            true
        }
    }

    /// The heap handles directly referenced by an object (for the mark worklist).
    pub fn children(&self, h: GcRef) -> Vec<GcRef> {
        let mut out = Vec::new();
        // `child_gcref` returns the handle for BOTH a true `Obj` (tag 000, incl. boxed `BigInt`) AND a
        // boxed float (Float tag 010) — a boxed float inside a container must be traced or it is swept.
        let mut push = |v: &Value| {
            if let Some(c) = v.child_gcref() {
                out.push(c);
            }
        };
        match self.get(h) {
            Obj::Str(_) => {}
            // A GC leaf: raw bytes, no embedded `GcRef`s, so nothing to trace (like `Str`).
            Obj::Bytes(_) => {}
            // Also a GC leaf — the mutable `bytearray` still holds only raw `u8`s, never a `GcRef`.
            Obj::ByteArray(_) => {}
            // GC leaves: a boxed scalar holds one raw `i64`/`f64`, no `GcRef`s (like `Bytes`).
            Obj::BigInt(_) => {}
            Obj::FloatBox(_) => {}
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
            // The wrapped inner value may be a heap object — trace it (like a 1-field struct).
            Obj::NewType { inner, .. } => push(inner),
            // A boxed local's cell: the inner value may be a heap object — trace it (like `NewType`).
            Obj::Cell(v) => push(v),
            Obj::Func { home, .. } => out.push(*home),
            Obj::Closure { captured, home, .. } => {
                captured.iter().for_each(&mut push);
                out.push(*home);
            }
            Obj::Module(m) => m.slots.iter().for_each(&mut push),
            Obj::Native { .. } => {}
            // A GC leaf: holds only a name, no embedded `GcRef`s (like `Native`).
            Obj::Builtin(_) => {}
            Obj::Cffi(_) => {}
            Obj::Ptr(_) => {}
            // B3.1: the core lives in an `Arc` outside this heap and holds `WireValue`s, but those
            // can still carry `Handle(GcRef)`s into *this* heap (an `Executor`'s queued closures and
            // any core nested inside another core; B3.3a: `str` messages queue as owned bytes, rooting
            // nothing). Trace every
            // reachable embedded handle so those objects stay rooted while the core holds them; `seen`
            // breaks `Arc` cycles. (The doc's "drop these arms" is wrong at B3.1 — closures can't
            // cross by value until B3.3/G1, so cores still hold heap refs.)
            //
            // W6-7: the walk is O(payload) and the GC threshold is object-COUNT based, so a big wire
            // container — ONE heap slot — used to be re-walked on every one of the constant GCs a
            // `for_each`/`from_wire` loop provokes: O(allocations × payload) = quadratic. Both queue
            // cores maintain their `(bytes, dirty)` summary at push/pop (their queue field is
            // private, so no site can forget); the single-value cores cache it at store time. A
            // payload with no `Handle` and no nested core is skipped outright — O(1) per pass.
            Obj::Channel(core) => {
                let g = core.q.lock().unwrap();
                if g.summary().1 {
                    let mut seen =
                        super::fxhash::FxHashSet::from_iter([Arc::as_ptr(core) as usize]);
                    for w in g.iter() {
                        crate::vm::core::collect_core_gcrefs(w, &mut out, &mut seen);
                    }
                }
            }
            Obj::Shared(core) => {
                let g = core.v.lock().unwrap();
                Self::mark_core_payload(&g, &core.summary, Arc::as_ptr(core) as usize, &mut out);
            }
            Obj::RwShared(core) => {
                let g = core.v.read().unwrap();
                Self::mark_core_payload(&g, &core.summary, Arc::as_ptr(core) as usize, &mut out);
            }
            Obj::Atomic(core) => {
                let g = core.v.lock().unwrap();
                Self::mark_core_payload(&g, &core.summary, Arc::as_ptr(core) as usize, &mut out);
            }
            // `AtomicInt` holds a plain i64 — no heap refs to trace.
            Obj::AtomicInt(_) => {}
            Obj::Executor(core) => {
                let g = core.inner.lock().unwrap();
                if g.summary().1 {
                    let mut seen =
                        super::fxhash::FxHashSet::from_iter([Arc::as_ptr(core) as usize]);
                    for w in g.iter() {
                        crate::vm::core::collect_core_gcrefs(w, &mut out, &mut seen);
                    }
                }
            }
            // D6/R2/R2b: a socket/listener/writer/reader core holds only an fd/buffer + a key — no heap refs.
            Obj::Socket(_) | Obj::Listener(_) | Obj::Writer(_) | Obj::Reader(_) => {}
            // Experimental generators — root the suspended frames/stack/args (see `gc_roots`).
            Obj::Generator(g) => out.extend(g.gc_roots()),
        }
        out
    }

    /// W6-7 — trace a single-value core's payload (`Shared`/`RwShared`/`Atomic`) through its cached
    /// summary. `WS_UNKNOWN` (the `Default`, and what a core built outside a store path starts at)
    /// walks once and memoizes; `WS_CLEAN` short-circuits; `WS_DIRTY` walks every pass and is never
    /// memoized. Called with the payload lock held, so a concurrent store cannot interleave between
    /// reading the state and reading the payload.
    ///
    /// The `debug_assert` is the net for THE trap in this design: the payload of these cores is
    /// *replaced* by `set`/`update`/`store`/`exchange`/`cas`/`add`/`sub`, and a store that forgot to
    /// refresh the summary would leave a stale `CLEAN` — the GC would stop tracing a live handle
    /// (use-after-free). Every debug-build GC pass re-derives the verdict and compares.
    fn mark_core_payload(
        w: &crate::vm::wire::WireValue,
        summary: &crate::vm::core::WireSummary,
        core_id: usize,
        out: &mut Vec<GcRef>,
    ) {
        use crate::vm::core::{WS_CLEAN, WS_DIRTY};
        let state = summary.state();
        debug_assert!(
            state != WS_CLEAN || !crate::vm::core::wire_summary(w).1,
            "stale CLEAN core summary — a store path failed to refresh it (would under-root the GC)"
        );
        if state == WS_CLEAN {
            return;
        }
        if state != WS_DIRTY {
            // UNKNOWN: one walk fills the cache (and the `--max-heap` byte count).
            summary.set(w);
            if summary.state() == WS_CLEAN {
                return;
            }
        }
        let mut seen = super::fxhash::FxHashSet::from_iter([core_id]);
        crate::vm::core::collect_core_gcrefs(w, out, &mut seen);
    }

    /// Free every unmarked object and clear all marks for the next cycle. Resets the allocation
    /// counter and grows the next-collection threshold relative to the surviving set.
    pub fn sweep(&mut self) {
        for idx in 0..self.slots.len() {
            if self.slots[idx].obj.is_some() {
                if self.is_marked(idx) {
                    self.clear_mark(idx);
                } else {
                    self.slots[idx].obj = None;
                    self.free.push(idx as u32);
                    self.live -= 1;
                }
            }
        }
        self.since_gc = 0;
        self.since_gc_bytes.set(0);
        // Drop the armed `get_mut` record: this is the ONLY thing that frees a slot, so it is also
        // the only thing that could leave the record pointing at a slot `alloc` then re-hands to a
        // different object. Clearing it here is what makes the settle sound.
        self.pending_mut.set(None);
        self.force_collect = false;
        self.next_gc = (self.live * 2).max(MIN_GC_THRESHOLD);
        let lb = self.live_bytes();
        if lb > self.peak_live_bytes {
            self.peak_live_bytes = lb;
        }
        // `--max-heap` runaway-alloc guard: reuse the `lb` just computed (no second scan). Off when
        // `mem_cap == 0`, so `over_cap` stays false forever on the common path.
        self.over_cap = self.mem_cap != 0 && lb > self.mem_cap;
    }

    /// Approximate live heap footprint in bytes: each live slot's `Obj` plus the owned backing
    /// storage (Vec/Box capacities) of the container variants. For the Phase-1 8B-`Value` memory
    /// gate — not precise allocator accounting (Map/Set count entries backing only, not the index),
    /// but stable enough to compare before/after on a fixed workload.
    pub fn live_bytes(&self) -> usize {
        self.bytes_in(true)
    }

    /// W7-26r sibling — this heap's OWN allocation: [`live_bytes`](Heap::live_bytes) minus every
    /// `Arc`-shared core payload. Used to charge a queued job's freshly built worker heap to its
    /// submitter, where the full walk would be wrong twice over.
    ///
    /// **Aliasing.** A `Channel`/`Shared`/`RwShared`/`Atomic`/`Executor` crosses the airlock as a
    /// SHARED `Arc`, not a copy — so a captured `Shared` holding 1 MB is one allocation that the
    /// submitter's own `live_bytes` already counts. Charging it again per queued job made 60 jobs
    /// report 60 MB against a true 3.8 MB (a false OVER-MEMORY at a 20 MB cap; found by adversarial
    /// review). What a submit really ADDS is the deep-copied plain data, which is exactly what this
    /// counts.
    ///
    /// **Locks.** Taking none is also what makes this callable from the submit path at all: the core
    /// arms of the full walk take `core.inner` / `core.q`, and `Vm::executor_method` reaches this
    /// while holding `core.inner` — `std::sync::Mutex` is not reentrant, so a job capturing its own
    /// executor self-deadlocked (measured: hang, rc=124, under a cap only). The call site is off the
    /// lock now as well; this is the belt to that braces.
    pub fn own_bytes(&self) -> usize {
        self.bytes_in(false)
    }

    /// The shared body of [`live_bytes`](Heap::live_bytes) / [`own_bytes`](Heap::own_bytes):
    /// `include_cores` decides whether an `Arc`-shared core's payload is charged to this heap (the
    /// reachability question) or skipped entirely (the "what does this heap itself own" question).
    fn bytes_in(&self, include_cores: bool) -> usize {
        let mut total = 0usize;
        // W6-10 — an off-heap wire payload belongs to ONE `Arc` core, but `from_wire` mints a FRESH
        // `Obj::Channel`/`Shared`/… slot for every crossing, so a heap can hold K alias slots for one
        // core. Charge each core's bytes ONCE per heap (by `Arc` pointer identity), or K handles to a
        // 100 MB payload report 1 GB and the cap fires OVER-MEMORY on a program using 1/K of it.
        //
        // A HASH set, not a linear scan: `live_bytes` runs on EVERY `sweep()` (the `peak_live_bytes`
        // probe, cap or no cap), so an O(distinct cores) membership scan per core slot would make the
        // GC pass O(D²) — the exact quadratic W6-7 exists to remove, just on a different axis
        // (Go-idiomatic mailbox-per-connection code holds thousands of distinct cores). It stays
        // allocation-free until the first core slot shows up (`HashSet::default` does not allocate).
        let mut cores: super::fxhash::FxHashSet<usize> = Default::default();
        // W6-10r — whether to also charge the cores NESTED inside a core's payload (a `Channel`
        // parked in a `Shared`, whose own alias slot has been swept: reachable from this heap, but
        // owning no `Obj` slot of its own, so it used to be counted nowhere and backlogged 304 MB
        // past an 8 MB cap). Gated on a live cap for the same reason the byte-aware GC pacing is
        // (`should_collect`): with `mem_cap == 0` `over_cap` is meaningless, so a `chezzi run`, a
        // bench and the two-worker-count `tests/chz` gate pay one `!= 0` load and ZERO extra walks — the walk is
        // O(payload) per DIRTY core per sweep, on top of the mark pass's. A CLEAN core stays O(1)
        // either way. (`CHEZZI_HEAP_STATS`'s cap-off peak therefore still omits nested-core bytes,
        // exactly as it did before this fix.)
        let deep = self.mem_cap != 0;
        for slot in &self.slots {
            let Some(obj) = &slot.obj else { continue };
            total += std::mem::size_of::<Obj>();
            total += match obj {
                // W6-10 — an airlocked value lives as a `WireValue` in an `Arc` OUTSIDE every
                // `Heap`, so it used to be counted nowhere and a 195 MB channel backlog sailed past
                // a 200 KB `--max-heap` cap. Each core's cached byte count is charged ONCE per heap
                // (`once`, by `Arc` identity — K handles to one core are K views of one allocation,
                // not K allocations). What this number means: **bytes reachable from THIS heap**. A
                // core shared by N M:N worker heaps therefore appears in each of them — correct for
                // a per-heap reachability cap (each worker really can reach it), but it is not an
                // ownership split, so the N heaps' totals do not add up to process RSS.
                //
                // W6-10r — under a cap the charge also RECURSES into the cores nested in this one's
                // payload, sharing the same `cores` set so a nested core that *also* has an alias
                // slot here is still charged exactly once, whichever way it is met first.
                Obj::Channel(core) if include_cores && cores.insert(Arc::as_ptr(core) as usize) => {
                    let g = core.q.lock().unwrap();
                    if deep {
                        crate::vm::core::queue_bytes_deep(g.summary(), g.iter(), &mut cores)
                    } else {
                        g.summary().0
                    }
                }
                // W7-26 — BOTH payload halves. `inner` is the lazy QUEUE half (filled only by the
                // since-removed cooperative engine); under eager `submit` it stays empty forever and
                // every finished job's result lands in
                // `eager` instead (300 × ~1 MB of results measured **PASS at 313 MB against an
                // 8 MB cap** while only `inner` was read). W7-27 — what a finished job leaves in
                // `eager` is now its buffered `out`/`stderr` only: the return value is dropped, since
                // nothing can read it. Those buffers are unbounded on their own (held to the W7-5c
                // task-order flush), so this arm is needed either way. Exactly one half is ever non-zero for a
                // given executor, but summing is what keeps this honest — the same argument the
                // `Executor(pending=…)` display already makes. The locks are taken SEQUENTIALLY,
                // and even nested they would keep the `inner → eager` order the submit arm
                // establishes (`Vm::executor_method` holds `inner` across `dispatch_eager_job`,
                // which takes `eager` alone). `core::nested_core_bytes`'s `Executor` arm reads both
                // halves too and MUST stay in lockstep: the `cores` set is shared between the two
                // walks, so a half missing from either arm is dropped whenever that walk gets there
                // first.
                //
                // Rooting is deliberately NOT mirrored: a job's return value crossed by value with
                // no parent-heap `GcRef` (B3.2, enforced by `ensure_crossable` and fenced by a
                // `debug_assert` in `outcome_summary`), which is why `children` has no `eager` arm.
                Obj::Executor(core)
                    if include_cores && cores.insert(Arc::as_ptr(core) as usize) =>
                {
                    let queued = {
                        let g = core.inner.lock().unwrap();
                        if deep {
                            crate::vm::core::queue_bytes_deep(g.summary(), g.iter(), &mut cores)
                        } else {
                            g.summary().0
                        }
                    };
                    let g = core.eager.lock().unwrap_or_else(|e| e.into_inner());
                    // W7-26r sibling — plus the jobs this executor has DISPATCHED BUT NOT STARTED.
                    // Each is a fully built worker heap parked in the process-global pool queue,
                    // owned by no heap and so counted nowhere: 300 of them summing to 666 MB sailed
                    // past an 8 MB cap while every individual heap stayed well under it. The
                    // submitter owns them until the pool picks them up (`ExecutorCore::pending`),
                    // and `Relaxed` is enough — this is a size estimate sampled at a sweep, not a
                    // synchronization edge.
                    queued
                        + core.pending.load(std::sync::atomic::Ordering::Relaxed)
                        + if deep {
                            crate::vm::core::queue_bytes_deep(g.summary(), g.values(), &mut cores)
                        } else {
                            g.summary().0
                        }
                }
                Obj::Shared(core) if include_cores && cores.insert(Arc::as_ptr(core) as usize) => {
                    if deep {
                        crate::vm::core::value_core_bytes_deep(
                            &core.summary,
                            &core.v.lock().unwrap(),
                            &mut cores,
                        )
                    } else {
                        core.summary.bytes()
                    }
                }
                Obj::RwShared(core)
                    if include_cores && cores.insert(Arc::as_ptr(core) as usize) =>
                {
                    if deep {
                        crate::vm::core::value_core_bytes_deep(
                            &core.summary,
                            &core.v.read().unwrap(),
                            &mut cores,
                        )
                    } else {
                        core.summary.bytes()
                    }
                }
                Obj::Atomic(core) if include_cores && cores.insert(Arc::as_ptr(core) as usize) => {
                    if deep {
                        crate::vm::core::value_core_bytes_deep(
                            &core.summary,
                            &core.v.lock().unwrap(),
                            &mut cores,
                        )
                    } else {
                        core.summary.bytes()
                    }
                }
                // Every non-core variant — the plain owned backing, sized in ONE place so a new
                // `Obj` variant cannot be counted here and forgotten at the `alloc`/`get_mut`
                // charges. A core arm whose guard above failed (already counted for this heap, or
                // `include_cores == false`) lands here too and correctly scores 0.
                other => obj_bytes_shallow(other),
            };
        }
        total
    }

    /// The high-water mark of [`live_bytes`](Heap::live_bytes) sampled at each `sweep()`.
    pub fn peak_live_bytes(&self) -> usize {
        self.peak_live_bytes
    }

    /// `chezzi test --max-heap` — set the per-test live-heap cap in bytes (`0` = OFF). Clears any
    /// prior `over_cap` latch so a new test starts clean.
    pub fn set_mem_cap(&mut self, cap: usize) {
        self.mem_cap = cap;
        self.over_cap = false;
        // Disarm `get_mut`'s pending record along with the cap. Cap-off never arms it, so a
        // cap→0→cap toggle is the one way a record could outlive the `sweep()` that freed its slot
        // and be settled against a slot `alloc` re-handed to a different object. Production sets the
        // cap exactly once per `Vm`, so this is belt-and-braces — but it makes the settle's soundness
        // a local property of this file instead of an argument about every caller.
        self.pending_mut.set(None);
    }

    /// The configured `--max-heap` cap in bytes (`0` = OFF) — for the abort message.
    pub fn mem_cap(&self) -> usize {
        self.mem_cap
    }

    /// True once `sweep()` saw `live_bytes()` exceed a non-zero `mem_cap` — the VM's hard-abort signal.
    pub fn over_cap(&self) -> bool {
        self.over_cap
    }

    /// Reset the `over_cap` latch (per-test lifecycle), leaving `mem_cap` intact.
    pub fn clear_over_cap(&mut self) {
        self.over_cap = false;
    }
}

#[cfg(test)]
mod iter_obj_tests {
    use super::*;

    /// `Obj::Iter` must stay within the 64B cap (Vec 24B + usize 8B = 32B; `MapData`/`SetData` at
    /// 56B of payload + the 8B enum discriminant set the cap now that `Module`'s payload is boxed).
    #[test]
    fn obj_iter_within_size_cap() {
        assert_eq!(std::mem::size_of::<Obj>(), 64);
    }

    /// The per-slot `Vec` element must be exactly 64B — the mark bit lives in the parallel `marks`
    /// bitset, not padding the slot. `Option<Obj>` is 64B (Obj's spare-discriminant niche makes
    /// `None` free), so with `mark` gone the slot is exactly one 64B `Obj` wide, and the mark/sweep
    /// scan never pulls a payload into cache to read a bool.
    #[test]
    fn slot_element_is_64b() {
        assert_eq!(std::mem::size_of::<Slot>(), 64);
    }

    /// M19 lever #1: inline struct fields. `Fields` must fit in ≤32B (`[Value;3]`=24B + len:u8 + the
    /// discriminant, packed into alignment padding) so `Obj::Struct`'s payload (tid 4 + Fields) stays
    /// within the 56B cap → `Obj` stays 64. If this fails, the inline width is too big; STOP rather
    /// than widen past `[Value;3]`.
    #[test]
    fn fields_inline_width_fits() {
        assert!(
            std::mem::size_of::<Fields>() <= 32,
            "Fields is {}B, must be <= 32B to keep Obj at 64",
            std::mem::size_of::<Fields>()
        );
        assert_eq!(
            std::mem::size_of::<Obj>(),
            64,
            "Fields must not push Obj past 64"
        );
    }

    /// `Fields` round-trips a ≤3-field vec as `Inline` and a >3-field vec as `Spill`, preserving
    /// `len`/`as_slice`; `get` bounds-checks; and a `get_mut`/`IndexMut` write lands in the LIVE
    /// backing for BOTH variants (catches an aliased-temp-copy bug on the inline array or spill box).
    #[test]
    fn fields_inline_and_spill_roundtrip() {
        // 2 fields → Inline
        let mut inl = Fields::from_vec(vec![Value::int(10), Value::int(20)]);
        assert!(matches!(inl, Fields::Inline { .. }));
        assert_eq!(inl.len(), 2);
        assert_eq!(inl.as_slice(), &[Value::int(10), Value::int(20)]);
        assert_eq!(inl.get(1), Some(&Value::int(20)));
        assert_eq!(inl.get(2), None);
        *inl.get_mut(0).unwrap() = Value::int(99);
        inl[1] = Value::int(88);
        assert_eq!(inl.as_slice(), &[Value::int(99), Value::int(88)]);

        // 4 fields → Spill
        let mut sp = Fields::from_vec(vec![
            Value::int(1),
            Value::int(2),
            Value::int(3),
            Value::int(4),
        ]);
        assert!(matches!(sp, Fields::Spill(_)));
        assert_eq!(sp.len(), 4);
        assert_eq!(
            sp.as_slice(),
            &[Value::int(1), Value::int(2), Value::int(3), Value::int(4)]
        );
        assert_eq!(sp.get(3), Some(&Value::int(4)));
        assert_eq!(sp.get(4), None);
        *sp.get_mut(3).unwrap() = Value::int(77);
        sp[0] = Value::int(66);
        assert_eq!(
            sp.as_slice(),
            &[Value::int(66), Value::int(2), Value::int(3), Value::int(77)]
        );
    }

    /// A `Spill` struct (>3 fields) holding a heap ref must trace that ref in `children()` and count
    /// its boxed backing in `live_bytes`; an `Inline` struct (≤3 fields) adds 0 extra heap beyond the
    /// `Obj` slot. Mirrors `live_bytes_counts_list_backing`.
    #[test]
    fn struct_gc_and_live_bytes() {
        let mut h = Heap::new();
        let elem = h.alloc(Obj::Str("kept".into()));
        let before = h.live_bytes();
        // 4-field Spill struct holding the heap Str among its fields.
        let sp = h.alloc(Obj::Struct {
            tid: crate::vm::op::TID_NONE,
            fields: Fields::from_vec(vec![
                Value::int(1),
                Value::obj(elem),
                Value::int(3),
                Value::int(4),
            ]),
        });
        assert!(
            h.children(sp).contains(&elem),
            "Spill struct must trace its heap-ref field"
        );
        let after = h.live_bytes();
        assert!(
            after >= before + std::mem::size_of::<Obj>() + 4 * std::mem::size_of::<Value>(),
            "Spill backing must register in live_bytes"
        );
        // trace survives a mark/sweep
        h.mark(sp);
        for c in h.children(sp) {
            h.mark(c);
        }
        h.sweep();
        assert!(matches!(h.get(elem), Obj::Str(s) if &**s == "kept"));

        // Inline struct (2 fields) adds only the Obj slot — 0 extra heap.
        let base = h.live_bytes();
        let _inl = h.alloc(Obj::Struct {
            tid: crate::vm::op::TID_NONE,
            fields: Fields::from_vec(vec![Value::int(1), Value::int(2)]),
        });
        assert_eq!(
            h.live_bytes(),
            base + std::mem::size_of::<Obj>(),
            "Inline struct must add exactly one Obj slot, 0 extra heap"
        );
    }

    /// A cursor is NON-LEAF: `children()` must yield its snapshot's heap refs so a not-yet-consumed
    /// element survives a GC pass (contrast `Bytes`/`ByteArray`, which trace nothing).
    #[test]
    fn obj_iter_traces_items_as_gc_children() {
        let mut heap = Heap::new();
        // A heap element the cursor must keep alive.
        let elem = heap.alloc(Obj::Str("kept".into()));
        let cursor = heap.alloc(Obj::Iter {
            items: vec![Value::obj(elem), Value::int(7)],
            pos: 0,
        });
        let kids = heap.children(cursor);
        assert!(
            kids.contains(&elem),
            "cursor must trace its heap-ref items, got {kids:?}"
        );
        // Mark only the cursor as a root, trace, sweep — the element must survive.
        heap.mark(cursor);
        for c in heap.children(cursor) {
            heap.mark(c);
        }
        heap.sweep();
        assert!(matches!(heap.get(elem), Obj::Str(s) if &**s == "kept"));
    }

    /// `live_bytes()` must register a container's owned Vec backing, not just the `Obj` slot itself.
    #[test]
    fn live_bytes_counts_list_backing() {
        let mut h = Heap::new();
        let empty = h.live_bytes();
        let r = h.alloc(Obj::List(vec![Value::int(1), Value::int(2), Value::int(3)]));
        let grown = h.live_bytes();
        // one slot (size_of::<Obj>) + the Vec's 3*size_of::<Value>() backing must register
        assert!(grown > empty + std::mem::size_of::<Obj>());
        let _ = r;
    }

    /// `BigInt`/`FloatBox` are GC LEAVES (`children()` traces nothing, like `Bytes`) and must not
    /// grow `Obj` past the 64B cap.
    #[test]
    fn bigint_and_floatbox_are_leaves() {
        let mut h = Heap::new();
        let a = h.alloc(Obj::BigInt(i64::MAX));
        let b = h.alloc(Obj::FloatBox(3.5));
        assert!(h.children(a).is_empty());
        assert!(h.children(b).is_empty());
        assert_eq!(std::mem::size_of::<Obj>(), 64); // cap unchanged
    }

    /// With no cap set (`mem_cap == 0`), `sweep()` never trips `over_cap` regardless of live size.
    #[test]
    fn mem_cap_off_never_trips() {
        let mut h = Heap::new();
        let r = h.alloc(Obj::Str("x".into()));
        h.mark(r); // survive the sweep so live_bytes > 0
        h.sweep();
        assert!(!h.over_cap());
    }

    /// A tiny cap trips `over_cap` once a marked (surviving) object exceeds it at sweep — a single
    /// `Obj` slot is 64B >> 1, so one live string is over a 1-byte cap.
    #[test]
    fn mem_cap_trips_when_live_exceeds() {
        let mut h = Heap::new();
        h.set_mem_cap(1);
        let r = h.alloc(Obj::Str("x".into()));
        h.mark(r);
        h.sweep();
        assert!(h.over_cap());
        // clear_over_cap resets the latch for the next per-test lifecycle.
        h.clear_over_cap();
        assert!(!h.over_cap());
    }

    /// W6-10 sampling half: off-heap wire bytes pace a sweep ONLY when a `--max-heap` cap is live.
    /// Cap OFF must be bit-for-bit today's object-count pacing; cap ON must sample before the
    /// off-heap growth can overshoot; `sweep()` must reset the counter like `since_gc`.
    #[test]
    fn wire_bytes_pace_a_sweep_only_under_a_cap() {
        // (a) cap OFF: any amount of charged off-heap growth must NOT force a collection.
        let h = Heap::new();
        h.charge_bytes(64 * 1024 * 1024);
        assert!(!h.should_collect(), "cap-off pacing must ignore wire bytes");

        // (b) cap ON: sub-threshold does not collect, threshold does. cap/4 = 2 MB here.
        let mut h = Heap::new();
        h.set_mem_cap(8_000_000);
        h.charge_bytes(1024);
        assert!(!h.should_collect(), "sub-threshold charge must not collect");
        h.charge_bytes(2_000_000);
        assert!(
            h.should_collect(),
            "cap/4 of charged wire bytes must collect"
        );

        // (c) the counter resets at sweep, like `since_gc`.
        h.sweep();
        assert!(
            !h.should_collect(),
            "sweep must reset the wire-byte counter"
        );

        // (d) the 64 KB floor: a tiny cap must not force a GC on every small store.
        let mut h = Heap::new();
        h.set_mem_cap(1000);
        h.charge_bytes(32 * 1024);
        assert!(!h.should_collect(), "64 KB floor must survive a tiny cap");
        h.charge_bytes(32 * 1024);
        assert!(h.should_collect());
    }

    /// W7-28 — the OTHER two byte funnels (`alloc` and `get_mut`), same contract as the wire-byte
    /// sibling above. These are the growth the object count cannot see: FEW-huge allocations, and
    /// in-place appends into an existing object that allocate nothing at all.
    #[test]
    fn alloc_and_in_place_growth_pace_a_sweep_only_under_a_cap() {
        let big = |n: usize| Obj::List(Vec::with_capacity(n));

        // (a) cap OFF must charge NOTHING — and this phase can actually FAIL: it does the growth
        // cap-off, THEN turns the cap on. A charge that leaked past the `mem_cap` gate would have
        // banked > cap/4 already and the very next `should_collect` would fire.
        let mut h = Heap::new();
        let r = h.alloc(big(1_000_000)); // 8 MB of backing, cap off
        if let Obj::List(v) = h.get_mut(r) {
            v.extend((0..1_000_000).map(Value::int)); // in-place bulk growth, cap off
        }
        assert!(!h.should_collect(), "cap-off pacing must ignore bytes");
        h.set_mem_cap(8_000_000); // cap/4 = 2 MB
        assert!(
            !h.should_collect(),
            "cap-off calls must have accumulated nothing"
        );

        // (b) cap ON, funnel 1 — `alloc` charges the new object's own backing. Sub-threshold does
        // not collect; crossing cap/4 does. This is `\"x\".repeat(20000000)`: 20 MB in ONE
        // allocation, which `since_gc`'s 256-object threshold can never see.
        let mut h = Heap::new();
        h.set_mem_cap(8_000_000);
        h.alloc(big(100_000)); // 800 KB
        assert!(!h.should_collect(), "sub-threshold alloc must not collect");
        h.alloc(big(200_000)); // +1.6 MB = 2.4 MB
        assert!(h.should_collect(), "cap/4 of allocated bytes must collect");

        // (c) cap ON, funnel 2 — ONE `get_mut` that appends in bulk. This is `big.extend(chunk)`:
        // unbounded bytes in a single instruction, which no instruction tick can bound.
        let mut h = Heap::new();
        h.set_mem_cap(8_000_000);
        let r = h.alloc(Obj::List(Vec::new()));
        assert!(!h.should_collect(), "an empty list must not collect");
        if let Obj::List(v) = h.get_mut(r) {
            v.extend((0..400_000).map(Value::int)); // 3.2 MB in one call
        }
        assert!(
            h.should_collect(),
            "a bulk in-place append must be charged at the next settle"
        );

        // (d) a SHRINK charges 0 — monotonic, like the wire counter. `mark` first: this heap has no
        // VM roots, so an unmarked `r` would be freed and the settle would read a dead slot (which
        // is exactly why `sweep()` clears `pending_mut`).
        h.mark(r);
        h.sweep();
        assert!(!h.should_collect(), "sweep must reset the byte counter");
        if let Obj::List(v) = h.get_mut(r) {
            v.clear();
            v.shrink_to_fit();
        }
        assert!(!h.should_collect(), "a shrink must charge 0, not underflow");

        // (e) the settle is armed by `get_mut` and disarmed by whoever settles first, so a second
        // `get_mut` charges the first one's growth rather than losing it.
        let mut h = Heap::new();
        h.set_mem_cap(8_000_000);
        let a = h.alloc(Obj::List(Vec::new()));
        let b = h.alloc(Obj::List(Vec::new()));
        if let Obj::List(v) = h.get_mut(a) {
            v.extend((0..150_000).map(Value::int)); // 1.2 MB, never settled by should_collect
        }
        if let Obj::List(v) = h.get_mut(b) {
            v.extend((0..150_000).map(Value::int)); // +1.2 MB = 2.4 MB > cap/4
        }
        assert!(
            h.should_collect(),
            "a second get_mut must settle the first's growth, not drop it"
        );

        // (f) the 64 KB floor survives a tiny cap (shared with the wire threshold).
        let mut h = Heap::new();
        h.set_mem_cap(1000); // cap/4 = 250, floored to 65536
        h.alloc(big(4_096)); // exactly 32 KB
        assert!(!h.should_collect(), "64 KB floor must survive a tiny cap");
        h.alloc(big(4_096)); // +32 KB = exactly the floor
        assert!(h.should_collect());
    }

    use std::sync::Mutex;

    fn wlist(n: i64) -> crate::vm::wire::WireValue {
        crate::vm::wire::WireValue::List {
            id: 0,
            items: (0..n).map(crate::vm::wire::WireValue::Int).collect(),
        }
    }

    /// W7-26 — record a finished eager job returning `value`, summarising it exactly as
    /// `dispatch_eager_job` does (the summary is computed OFF the lock, so the test must too).
    fn finish_done(
        g: &mut crate::vm::core::EagerState,
        idx: usize,
        value: crate::vm::wire::WireValue,
    ) {
        let outcome = crate::vm::TaskOutcome::Done(crate::vm::WorkerResult {
            value,
            out: Vec::new(),
            stderr: Vec::new(),
        });
        let sum = crate::vm::core::outcome_summary(&outcome);
        g.finish(idx, sum, outcome);
    }

    /// W6-7 — a core's pure-data wire payload is walked ONCE (lazily, on the first mark) and then
    /// short-circuited: `children()` must go O(payload) → O(1) per GC pass. A bigger payload swapped
    /// in BEHIND the memo (not via a store path) moves neither the walk nor the cached byte count —
    /// that is what proves the memo is a real short-circuit and not just a re-walk.
    #[test]
    fn core_payload_walk_is_memoized() {
        use crate::vm::core::{WS_CLEAN, WS_UNKNOWN};
        let mut h = Heap::new();
        let core = Arc::new(SharedCore {
            v: Mutex::new(wlist(1000)),
            ..Default::default()
        });
        let r = h.alloc(Obj::Shared(Arc::clone(&core)));
        assert_eq!(core.summary.state(), WS_UNKNOWN);
        assert!(h.children(r).is_empty());
        assert_eq!(core.summary.state(), WS_CLEAN);
        let bytes = core.summary.bytes();
        assert!(bytes > 1000 * std::mem::size_of::<crate::vm::wire::WireValue>());

        // Swap in a 5x bigger payload WITHOUT going through a store path: a memoized CLEAN core is
        // never re-walked, so neither `children()` nor the cached byte count moves. (A real store
        // goes through `SharedCore::store`, which refreshes both — and a debug-build `debug_assert`
        // in `mark_core_payload` re-derives the verdict on every pass to catch a store that didn't.)
        *core.v.lock().unwrap() = wlist(5000);
        assert!(
            h.children(r).is_empty(),
            "a CLEAN memo must short-circuit the walk entirely"
        );
        assert_eq!(
            core.summary.bytes(),
            bytes,
            "memoized bytes must not re-walk"
        );

        // …and a real store DOES refresh it.
        core.store(wlist(5000));
        assert!(core.summary.bytes() > bytes);
        assert_eq!(core.summary.state(), WS_CLEAN);
    }

    /// W6-7 boundary — a payload that CAN root a heap object stays DIRTY and is re-walked on every
    /// pass (never memoized): a direct `Handle`, a handle queued in a `Channel`, and a handle that
    /// appears in a NESTED core only after the outer core was first marked.
    #[test]
    fn dirty_core_payload_is_still_traced() {
        use crate::vm::core::WS_DIRTY;
        let mut h = Heap::new();
        let kept = h.alloc(Obj::Str("kept".into()));

        let sc = Arc::new(SharedCore {
            v: Mutex::new(crate::vm::wire::WireValue::List {
                id: 0,
                items: vec![crate::vm::wire::WireValue::Handle(kept)],
            }),
            ..Default::default()
        });
        let sr = h.alloc(Obj::Shared(Arc::clone(&sc)));
        assert!(h.children(sr).contains(&kept));
        assert_eq!(sc.summary.state(), WS_DIRTY);
        assert!(h.children(sr).contains(&kept), "DIRTY must re-walk");

        let cc = Arc::new(ChannelCore::default());
        let hw = crate::vm::wire::WireValue::Handle(kept);
        cc.q.lock()
            .unwrap()
            .push(crate::vm::core::wire_summary(&hw), hw);
        let cr = h.alloc(Obj::Channel(Arc::clone(&cc)));
        assert!(h.children(cr).contains(&kept));
        assert!(h.children(cr).contains(&kept));

        // NESTED: the outer core's payload is a `Shared` holding only ints when first marked; the
        // inner core then gains a handle. `wire_summary` calls any nested core dirty, so the outer
        // keeps walking and the late handle is still collected.
        let inner = Arc::new(SharedCore {
            v: Mutex::new(wlist(4)),
            ..Default::default()
        });
        let outer = Arc::new(SharedCore {
            v: Mutex::new(crate::vm::wire::WireValue::List {
                id: 0,
                items: vec![crate::vm::wire::WireValue::Shared(Arc::clone(&inner))],
            }),
            ..Default::default()
        });
        let or = h.alloc(Obj::Shared(Arc::clone(&outer)));
        assert!(h.children(or).is_empty());
        assert_eq!(
            outer.summary.state(),
            WS_DIRTY,
            "a nested core is never CLEAN"
        );
        *inner.v.lock().unwrap() = crate::vm::wire::WireValue::Handle(kept);
        assert!(
            h.children(or).contains(&kept),
            "a handle stored into a NESTED core must still be traced"
        );
    }

    /// W6-10 — an airlocked wire payload lives in an `Arc` outside every `Heap`, so `live_bytes` used
    /// to count it nowhere (195 MB RSS passed a 200 KB cap). It now contributes via the cached summary.
    #[test]
    fn live_bytes_counts_offheap_wire_payload() {
        let mut h = Heap::new();
        let empty = Arc::new(SharedCore::default());
        let er = h.alloc(Obj::Shared(empty));
        h.children(er); // fill the lazy summary
        let base = h.live_bytes();

        let core = Arc::new(SharedCore {
            v: Mutex::new(wlist(10_000)),
            ..Default::default()
        });
        let r = h.alloc(Obj::Shared(Arc::clone(&core)));
        h.children(r); // fill the lazy summary
        let with_payload = h.live_bytes();
        assert!(
            with_payload > base + 10_000 * std::mem::size_of::<crate::vm::wire::WireValue>(),
            "the off-heap payload must register: {base} -> {with_payload}"
        );

        // A channel's backlog grows and shrinks live_bytes as messages are pushed/popped.
        let cc = Arc::new(ChannelCore::default());
        let cr = h.alloc(Obj::Channel(Arc::clone(&cc)));
        let before = h.live_bytes();
        for _ in 0..100 {
            let w = wlist(50);
            cc.q.lock()
                .unwrap()
                .push(crate::vm::core::wire_summary(&w), w);
        }
        let queued = h.live_bytes();
        assert!(queued > before + 100 * 50 * std::mem::size_of::<crate::vm::wire::WireValue>());
        while cc.q.lock().unwrap().pop().is_some() {}
        assert_eq!(
            h.live_bytes(),
            before,
            "a drained queue must return to zero"
        );
        let _ = cr;
    }

    /// W6-10r — a core reachable ONLY through another core's payload (its own alias slot swept) owns
    /// no `Obj` slot, so `live_bytes` used to reach its bytes nowhere: measured on the release
    /// binary, a `Channel` parked in a `Shared` backlogged to **304 MB peak RSS and PASSED** an 8 MB
    /// `--max-heap`, while the identical program holding the channel in a live local tripped.
    ///
    /// Also pins the two properties the fix must not break: the walk is GATED on a live cap (cap-off
    /// pacing/peak behaviour is bit-for-bit what it was), and a nested core that ALSO has an alias
    /// slot here is still charged exactly once.
    #[test]
    fn live_bytes_counts_a_nested_core_with_no_alias_slot() {
        let mut h = Heap::new();
        // The inner core has a real backlog and NO alias slot of its own in this heap.
        let inner = Arc::new(ChannelCore::default());
        for _ in 0..100 {
            let w = wlist(500);
            inner
                .q
                .lock()
                .unwrap()
                .push(crate::vm::core::wire_summary(&w), w);
        }
        let inner_bytes = inner.q.lock().unwrap().summary().0;
        assert!(inner_bytes > 100 * 500 * std::mem::size_of::<crate::vm::wire::WireValue>());

        let outer = Arc::new(SharedCore::default());
        outer.store(crate::vm::wire::WireValue::Channel(Arc::clone(&inner)));
        let or = h.alloc(Obj::Shared(Arc::clone(&outer)));
        h.children(or); // fill the lazy summary, exactly as a real GC pass would

        // Cap OFF: unchanged behaviour — the nested backlog is not walked.
        let shallow = h.live_bytes();
        assert!(
            shallow < inner_bytes,
            "with no cap the nested walk must not run: {shallow} vs {inner_bytes}"
        );

        // Cap ON: the nested backlog is charged.
        h.set_mem_cap(1);
        let deep = h.live_bytes();
        assert_eq!(
            deep,
            shallow + inner_bytes,
            "a nested core's bytes must be charged under a cap"
        );

        // The inner core gains an alias slot of its own: still charged ONCE (per-`Arc` de-dup shared
        // between the slot scan and the nested walk), not twice.
        let ir = h.alloc(Obj::Channel(Arc::clone(&inner)));
        assert_eq!(
            h.live_bytes(),
            deep + std::mem::size_of::<Obj>(),
            "a nested core with its own alias slot is charged once, not twice"
        );
        let _ = ir;
    }

    /// W7-26 — the `Obj::Executor` arm used to read only `inner`, the lazy QUEUE half. `submit` runs
    /// EAGERLY, so `inner` stays empty forever and every finished
    /// job's result sits in `eager` instead: measured on the release binary, 300 × ~1 MB of results
    /// **PASSED an 8 MB `--max-heap` at 313 MB peak RSS**.
    ///
    /// Also pins the two decisions the fix rests on: the charge is UNCONDITIONAL (unlike the nested
    /// walk above, the eager half's own bytes are correct cap-off too — accounting and enforcement
    /// stay separate, as in Go's `MemStats` vs `GOMEMLIMIT`), and `take_slots` returns the state to
    /// baseline so a second `shutdown` cannot leave phantom bytes charged.
    #[test]
    fn live_bytes_counts_an_executors_eager_results() {
        let mut h = Heap::new();
        let core = Arc::new(crate::vm::core::ExecutorCore::default());
        let r = h.alloc(Obj::Executor(Arc::clone(&core)));
        let base = h.live_bytes();

        let payload = 500 * std::mem::size_of::<crate::vm::wire::WireValue>();
        {
            let mut g = core.eager.lock().unwrap();
            for i in 0..10 {
                assert_eq!(g.reserve(), i);
                finish_done(&mut g, i, wlist(500));
            }
        }

        // Cap OFF: the results are counted anyway — this half needs no gate.
        let with_results = h.live_bytes();
        assert!(
            with_results >= base + 10 * payload,
            "a finished job's result must register with no cap: {base} -> {with_results}"
        );
        // Cap ON: same number (the `deep` gate only adds the NESTED-core recursion, and these
        // results hold pure data).
        h.set_mem_cap(1);
        assert_eq!(
            h.live_bytes(),
            with_results,
            "the eager charge must not depend on the cap"
        );

        // A core nested in a result IS charged under a cap, through the shared `cores` de-dup.
        let nested = Arc::new(SharedCore::default());
        nested.store(wlist(500));
        {
            let mut g = core.eager.lock().unwrap();
            let i = g.reserve();
            let w = crate::vm::wire::WireValue::Shared(Arc::clone(&nested));
            finish_done(&mut g, i, w);
        }
        assert!(
            h.live_bytes() >= with_results + nested.summary.bytes(),
            "a core nested inside an eager result must be charged under a cap"
        );

        // The join drains the slots — the charge goes with them.
        core.eager.lock().unwrap().take_slots();
        assert_eq!(
            h.live_bytes(),
            base,
            "take_slots must return the eager half to zero"
        );
        let _ = r;
    }

    /// W7-26, filed by adversarial review of the fix and MEASURED before the second arm landed: an
    /// executor holding 880 400 bytes of eager results, reachable only through an `Obj::Shared`
    /// payload, was counted as **240**. `Heap::live_bytes` gained the eager half but
    /// `core::nested_core_bytes` — the walk that reaches a core with no alias slot of its own — kept
    /// reading `inner` alone. And because the two walks SHARE the `cores`/`seen` set, the miss is
    /// not confined to slot-less cores: whichever walk meets a core first is the only one that
    /// charges it, so a live `Obj::Executor` slot ALSO loses its eager half whenever the enclosing
    /// container is visited first. Both orders are asserted here.
    ///
    /// This is the wave-6 meta-finding again (a fix applied to SOME arms of an N-way set), which is
    /// why the arms now carry lockstep comments pointing at each other.
    #[test]
    fn nested_executor_charges_its_eager_half_in_either_visit_order() {
        let ex = Arc::new(crate::vm::core::ExecutorCore::default());
        {
            let mut g = ex.eager.lock().unwrap();
            for i in 0..10 {
                assert_eq!(g.reserve(), i);
                finish_done(&mut g, i, wlist(500));
            }
        }
        let eager_bytes = ex.eager.lock().unwrap().summary().0;
        assert!(eager_bytes > 10 * 500 * std::mem::size_of::<crate::vm::wire::WireValue>());

        // (a) reachable ONLY through a `Shared`'s payload — no `Obj::Executor` slot at all.
        let mut h = Heap::new();
        h.set_mem_cap(1);
        let outer = Arc::new(SharedCore::default());
        outer.store(crate::vm::wire::WireValue::Executor(Arc::clone(&ex)));
        let base = h.live_bytes();
        let sr = h.alloc(Obj::Shared(Arc::clone(&outer)));
        assert!(
            h.live_bytes() >= base + eager_bytes,
            "a nested executor's eager results must be charged (was 240 of 880 400)"
        );
        let _ = sr;

        // (b) the executor DOES have its own slot, but the enclosing `Shared` is met first — the
        // shared `cores` set means the nested walk is the one that has to charge both halves.
        let mut h = Heap::new();
        h.set_mem_cap(1);
        let base = h.live_bytes();
        let sr = h.alloc(Obj::Shared(Arc::clone(&outer)));
        let er = h.alloc(Obj::Executor(Arc::clone(&ex)));
        assert!(
            h.live_bytes() >= base + eager_bytes,
            "visit order must not decide whether the eager half is counted"
        );
        let (_, _) = (sr, er);
    }

    /// W6-10r — the arms the headline test does NOT reach: a core nested inside a QUEUED MESSAGE
    /// (so `queue_bytes_deep`'s recursive branch and a container arm, not just the single-value
    /// path), and a core CYCLE. Filed by review of the fix: the headline test's nested core sits
    /// directly in a `Shared`'s payload and its queue is pure data, so it short-circuits before
    /// reaching either — the exact "fixed on SOME arms of an N-way set" shape.
    #[test]
    fn nested_core_bytes_walks_queued_messages_and_terminates_on_a_cycle() {
        let mut h = Heap::new();
        h.set_mem_cap(1);

        // A core nested inside a queued LIST message: queue → message → container → core.
        let deep = Arc::new(SharedCore::default());
        deep.store(wlist(500));
        let deep_bytes = deep.summary.bytes();
        assert!(deep_bytes > 500 * std::mem::size_of::<crate::vm::wire::WireValue>());

        let mid = Arc::new(ChannelCore::default());
        let msg = crate::vm::wire::WireValue::List {
            id: 0,
            items: vec![crate::vm::wire::WireValue::Shared(Arc::clone(&deep))],
        };
        mid.q
            .lock()
            .unwrap()
            .push(crate::vm::core::wire_summary(&msg), msg);
        let base = h.live_bytes();
        let cr = h.alloc(Obj::Channel(Arc::clone(&mid)));
        let with_deep = h.live_bytes();
        assert!(
            with_deep >= base + deep_bytes,
            "a core nested inside a QUEUED message must be charged: \
             {base} -> {with_deep} (nested payload {deep_bytes})"
        );
        let _ = cr;

        // A → B → A. The walk must terminate (and charge each core once).
        let a = Arc::new(SharedCore::default());
        let b = Arc::new(SharedCore::default());
        b.store(crate::vm::wire::WireValue::Shared(Arc::clone(&a)));
        a.store(crate::vm::wire::WireValue::Shared(Arc::clone(&b)));
        let ar = h.alloc(Obj::Shared(Arc::clone(&a)));
        let cyclic = h.live_bytes();
        assert!(cyclic > with_deep, "the cycle's cores must still register");
        assert_eq!(
            cyclic,
            h.live_bytes(),
            "the walk must be stable, not growing"
        );
        let _ = ar;
    }

    /// W6-10 review — a core's payload is ONE `Arc` allocation, but `from_wire` mints a FRESH
    /// `Obj::Shared`/`Obj::Channel` alias slot for every crossing, so a heap can hold K slots for
    /// one core. Charging the payload once per SLOT multiplies it by K and makes `--max-heap` report
    /// memory the process does not hold — a false-positive OVER-MEMORY at ~footprint/K, and the
    /// false-positive rate grows with fan-out. Bytes are counted once per CORE per heap.
    #[test]
    fn live_bytes_counts_a_shared_core_once_per_heap() {
        let mut h = Heap::new();
        let core = Arc::new(SharedCore {
            v: Mutex::new(wlist(10_000)),
            ..Default::default()
        });
        let first = h.alloc(Obj::Shared(Arc::clone(&core)));
        h.children(first); // fill the lazy summary
        let one_alias = h.live_bytes();

        // Nine more handles to the SAME core (what `hs.push(ch.recv())` × K produces).
        for _ in 0..9 {
            h.alloc(Obj::Shared(Arc::clone(&core)));
        }
        let ten_aliases = h.live_bytes();
        assert!(
            ten_aliases - one_alias < 10 * std::mem::size_of::<Obj>() + 4096,
            "10 handles to one core must not multiply its payload: {one_alias} -> {ten_aliases}"
        );

        // Same for a queue core.
        let cc = Arc::new(ChannelCore::default());
        for _ in 0..100 {
            let w = wlist(50);
            cc.q.lock()
                .unwrap()
                .push(crate::vm::core::wire_summary(&w), w);
        }
        let c1 = h.alloc(Obj::Channel(Arc::clone(&cc)));
        let one_chan = h.live_bytes();
        for _ in 0..9 {
            h.alloc(Obj::Channel(Arc::clone(&cc)));
        }
        assert!(
            h.live_bytes() - one_chan < 10 * std::mem::size_of::<Obj>() + 4096,
            "10 handles to one channel must not multiply its backlog"
        );
        let _ = c1;
    }

    /// W6-7/W6-10 round-2 review — the de-dup must key on the CORE, so K aliases of one core count
    /// once (the test above) but D *distinct* cores all count. It is a hash set, not a linear scan:
    /// `live_bytes` runs on every `sweep()`, so an O(D) membership scan per core slot would be an
    /// O(D²) GC pass (measured: 40 000 channels + 500 k allocations went 0.11 s → 1.24 s).
    #[test]
    fn live_bytes_sums_every_distinct_core() {
        let mut h = Heap::new();
        let base = h.live_bytes();
        let mut one = 0usize;
        for i in 0..64 {
            let core = Arc::new(SharedCore {
                v: Mutex::new(wlist(100)),
                ..Default::default()
            });
            let r = h.alloc(Obj::Shared(Arc::clone(&core)));
            h.children(r); // fill the lazy summary
            if i == 0 {
                one = h.live_bytes() - base;
            }
        }
        let all = h.live_bytes() - base;
        assert!(
            all > 60 * one,
            "64 DISTINCT cores must each be charged: one={one}, all={all}"
        );
    }

    /// W6-7 review — THE trap. `Shared`/`RwShared`/`Atomic` payloads are REPLACED (`set`/`update`/
    /// `write`/`store`/`exchange`/`cas`/`add`/`sub`), so a store that forgets to refresh the memo
    /// leaves a stale `WS_CLEAN` next to a handle-bearing payload → the GC stops tracing it →
    /// use-after-free. Every replacing store path must survive a memoized-CLEAN core. Deleting any
    /// `summary.set` in `vm::core`'s `store`/`store_guarded` turns this RED.
    #[test]
    fn replacing_store_refreshes_the_gc_summary() {
        use crate::vm::core::{AtomicCore, RwSharedCore, WS_CLEAN};
        use crate::vm::wire::WireValue;
        let mut h = Heap::new();
        let kept = h.alloc(Obj::Str("kept".into()));
        let handle = || WireValue::List {
            id: 0,
            items: vec![WireValue::Handle(kept)],
        };

        // Shared::set / Shared::update write-back.
        let sc = Arc::new(SharedCore {
            v: Mutex::new(wlist(4)),
            ..Default::default()
        });
        let sr = h.alloc(Obj::Shared(Arc::clone(&sc)));
        assert!(h.children(sr).is_empty());
        assert_eq!(sc.summary.state(), WS_CLEAN, "memoized CLEAN");
        sc.store(handle());
        assert!(
            h.children(sr).contains(&kept),
            "SharedCore::store must refresh the memo"
        );

        // RwShared::set / RwShared::write write-back.
        let rc = Arc::new(RwSharedCore {
            v: std::sync::RwLock::new(wlist(4)),
            ..Default::default()
        });
        let rr = h.alloc(Obj::RwShared(Arc::clone(&rc)));
        assert!(h.children(rr).is_empty());
        assert_eq!(rc.summary.state(), WS_CLEAN);
        rc.store(handle());
        assert!(
            h.children(rr).contains(&kept),
            "RwSharedCore::store must refresh the memo"
        );

        // Atomic::store, and the guarded RMW paths (exchange / cas / add|sub).
        let ac = Arc::new(AtomicCore {
            v: Mutex::new(wlist(4)),
            ..Default::default()
        });
        let ar = h.alloc(Obj::Atomic(Arc::clone(&ac)));
        assert!(h.children(ar).is_empty());
        assert_eq!(ac.summary.state(), WS_CLEAN);
        ac.store(handle());
        assert!(
            h.children(ar).contains(&kept),
            "AtomicCore::store must refresh the memo"
        );
        ac.store(wlist(4)); // back to CLEAN
        assert!(h.children(ar).is_empty());
        {
            let w = handle();
            let sum = crate::vm::core::wire_summary(&w);
            let mut g = ac.v.lock().unwrap();
            ac.store_guarded(&mut g, w, sum);
        }
        assert!(
            h.children(ar).contains(&kept),
            "AtomicCore::store_guarded must refresh the memo"
        );

        // …and the payload actually SURVIVES a collection rooted only through the core.
        for r in [sr, rr, ar] {
            let mut stack = vec![r];
            while let Some(x) = stack.pop() {
                if h.mark(x) {
                    stack.extend(h.children(x));
                }
            }
        }
        h.sweep();
        // `get` PANICS on a collected slot ("dangling GcRef"), so this line is the assertion: a
        // value live ONLY through a core payload must survive the sweep.
        assert!(matches!(h.get(kept), Obj::Str(s) if s.as_str() == "kept"));
    }
}
