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
    Native {
        name: Box<str>,
        func: crate::native::NativeFn,
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
}

impl Default for Heap {
    fn default() -> Self {
        Heap {
            slots: Vec::new(),
            marks: Vec::new(),
            free: Vec::new(),
            live: 0,
            since_gc: 0,
            next_gc: MIN_GC_THRESHOLD,
            peak_live_bytes: 0,
            mem_cap: 0,
            over_cap: false,
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
                    let mut seen = vec![Arc::as_ptr(core) as usize];
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
                    let mut seen = vec![Arc::as_ptr(core) as usize];
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
        let mut seen = vec![core_id];
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
        let mut total = 0usize;
        for slot in &self.slots {
            let Some(obj) = &slot.obj else { continue };
            total += std::mem::size_of::<Obj>();
            total += match obj {
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
                // W6-10 — an airlocked value lives as a `WireValue` in an `Arc` OUTSIDE every
                // `Heap`, so it used to be counted nowhere and a 195 MB channel backlog sailed past
                // a 200 KB `--max-heap` cap. Two accepted approximations: (1) the byte walk stops at
                // a NESTED core boundary (that core's own summary owns those bytes), and (2) one
                // `Arc` core aliased by N worker heaps under M:N is counted in all N — an OVER-count,
                // so the cap only ever trips EARLIER, never later.
                Obj::Channel(core) => core.q.lock().unwrap().summary().0,
                Obj::Executor(core) => core.inner.lock().unwrap().summary().0,
                Obj::Shared(core) => core.summary.bytes(),
                Obj::RwShared(core) => core.summary.bytes(),
                Obj::Atomic(core) => core.summary.bytes(),
                _ => 0,
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

    use std::sync::Mutex;

    fn wlist(n: i64) -> crate::vm::wire::WireValue {
        crate::vm::wire::WireValue::List {
            id: 0,
            items: (0..n).map(crate::vm::wire::WireValue::Int).collect(),
        }
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
        cc.q.lock()
            .unwrap()
            .push(crate::vm::wire::WireValue::Handle(kept));
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
            cc.q.lock().unwrap().push(wlist(50));
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
}
