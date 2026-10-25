//! The GC-managed heap (M5).
//!
//! Objects are addressed by [`GcRef`] (a slot index), so handle copies alias one object. One heap
//! serves a run, and another thread can read any object it can reach, so an op never holds a
//! `&mut Obj` over a reachable object (M3.0b): it arms the write with [`Heap::write`] and mutates
//! through `&self` methods whose stores are atomic slot stores (an `RVec` slot, an `RCell`, a
//! map entry, `vm/rvec.rs`) or go through a lock (a module's names). A race on one gives a wrong
//! value, never a torn header. [`Heap::init_mut`] is the one `&mut Obj` door, for an object this
//! call built and has not published yet.

use super::chzstr::ChzStr;
use super::core::{
    AtomicCore, AtomicIntCore, ChannelCore, ExecutorCore, ListenerCore, ReaderCore, RwSharedCore,
    SharedCore, SocketCore, WriterCore,
};
use super::heap_core::{CopyLog, HeapCore, lock};
pub use super::map_data::{MapData, MapEntry, SetData, SetEntry};
pub use super::module_data::{ModuleData, ModuleNames};
use super::rvec::{RCell, RVec, RetireList};
use super::value::{GcRef, Value};
use crate::base::op::ProtoId;
use crate::lexer::Span;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Struct field storage: ≤3 fields inline (no second heap alloc), more spill to an `RVec`.
/// Fields are FIXED at construction (positional hidden-class layout; no `.push/insert/resize`
/// growth sites), so a spill block never grows and never retires: a field write is one atomic slot
/// store, inline in an [`RCell`] or in the spill block (`vm/rvec.rs`), and takes `&self`. A
/// `Value` slot is 8B → `[RCell<Value>; 3]` is 24B, and the vast majority of structs
/// (≤3 fields) fold their fields into the `Obj` slot itself — zero second malloc. Keeps `Obj` at
/// the 64B cap (M19 lever #1).
#[derive(Debug, Clone)]
pub enum Fields {
    /// ≤3 fields folded inline; `vals[len..]` are unused padding holding `Value::nil()`.
    Inline { len: u8, vals: [RCell<Value>; 3] },
    /// >3 fields in an exact-length `RVec` block (no spare capacity).
    Spill(RVec<Value>),
}

/// The fields of one [`Fields`] read, by value. A `Spill` iterator reads one snapshot of the
/// block, so do not hold it across a safepoint (see [`RVec::iter`]).
pub enum FieldsIter<'a> {
    Inline(std::slice::Iter<'a, RCell<Value>>),
    Spill(super::rvec::Iter<'a, Value>),
}

impl Iterator for FieldsIter<'_> {
    type Item = Value;
    #[inline]
    fn next(&mut self) -> Option<Value> {
        match self {
            FieldsIter::Inline(it) => it.next().map(RCell::get),
            FieldsIter::Spill(it) => it.next(),
        }
    }
}

impl Fields {
    /// Build from a positional field vec: ≤3 → `Inline`, else `Spill`.
    #[inline]
    pub fn from_vec(v: Vec<Value>) -> Self {
        if v.len() <= 3 {
            let len = v.len() as u8;
            let vals =
                std::array::from_fn(|i| RCell::new(v.get(i).copied().unwrap_or(Value::nil())));
            Fields::Inline { len, vals }
        } else {
            Fields::Spill(RVec::from_slice(&v))
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
    pub fn iter(&self) -> FieldsIter<'_> {
        match self {
            Fields::Inline { len, vals } => FieldsIter::Inline(vals[..*len as usize].iter()),
            Fields::Spill(s) => FieldsIter::Spill(s.iter()),
        }
    }

    /// A copy of every field.
    pub fn to_vec(&self) -> Vec<Value> {
        self.iter().collect()
    }

    /// Field `i`, or `None` past the last field.
    #[inline]
    pub fn get(&self, i: usize) -> Option<Value> {
        match self {
            Fields::Inline { len, vals } => vals[..*len as usize].get(i).map(RCell::get),
            Fields::Spill(s) => s.try_get(i),
        }
    }

    /// Write field `i`; `false` (and no write) past the last field. Either way a write is one
    /// slot store: an `Inline` cell or a slot of the fixed `Spill` block.
    #[inline]
    pub fn set(&self, i: usize, v: Value) -> bool {
        match self {
            Fields::Inline { len, vals } => match vals[..*len as usize].get(i) {
                Some(slot) => {
                    slot.set(v);
                    true
                }
                None => false,
            },
            Fields::Spill(s) => {
                let ok = i < s.len();
                if ok {
                    s.set(i, v);
                }
                ok
            }
        }
    }

    /// Owned-backing byte footprint for `live_bytes`: `Inline` folds into the `Obj` slot (0 extra
    /// heap); `Spill` counts its block's `cap * size_of::<Value>()`.
    #[inline]
    pub fn heap_bytes(&self) -> usize {
        match self {
            Fields::Inline { .. } => 0,
            Fields::Spill(s) => s.capacity() * std::mem::size_of::<Value>(),
        }
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
    /// `bytearray` — the MUTABLE sibling of `bytes` (Python `bytearray` model). Storage is an
    /// `RVec<u8>` (one byte per slot, `vm/rvec.rs`) mutated IN PLACE through the `GcRef` heap slot,
    /// exactly like [`List`](Obj::List), so two bindings to the same `bytearray` observe each
    /// other's writes. Still a GC LEAF — raw `u8`s, no `GcRef`s — so `children()` traces nothing
    /// (the difference vs `Bytes` is the mutability of the slot, not GC reachability).
    ByteArray(RVec<u8>),
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
    /// or the snapshot's elements get collected out from under a live cursor. `RVec<Value>`(8) +
    /// `usize`(8) = 16B, within the 64B `Obj` cap (`MapData`/`SetData` set the cap).
    Iter {
        items: RVec<Value>,
        /// The read position, advanced in place (`&self`), so two threads can step one cursor.
        pos: RCell<usize>,
    },
    /// `[a, b, …]` — a growable list on a retiring vector (`vm/rvec.rs`): a grow retires the old
    /// block, and only the stop-the-world sweep frees it. A write reads the list with
    /// [`Heap::write`] and calls the `RVec` mutator with that handle.
    List(RVec<Value>),
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
        payload: RVec<Value>,
    },
    /// A heap-allocated mutable CELL — a single boxed `Value` behind a `GcRef`, the runtime home of a
    /// by-reference-captured local (uniform-capture feature, Task A). Two bindings holding the same
    /// cell handle observe each other's writes (`CellStore`), exactly like `List`/`bytearray` mutate
    /// through the shared slot. NON-LEAF: the inner value may be a heap object, so `children()` traces
    /// it. The value sits in an [`RCell`] (8B), so a store takes `&self`.
    Cell(RCell<Value>),
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
        captured: RVec<Value>,
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
    /// name to a string — `docs/design/roadmap.md` §3c.
    Native {
        name: Box<str>,
        func: crate::native::NativeFn,
        kind: crate::native::Kind,
    },
    /// A first-class UNIVERSE builtin FUNCTION value (`print`/`ord`/`chr`/`panic`), carried by id.
    /// Produced by `Op::LoadBuiltin` when the name is used in value position; calling it (via
    /// `invoke_value`) routes back into the same builtin logic direct calls use. A GC LEAF (no
    /// `GcRef`s), like `Native`. SENDABLE (pure code) — crosses the airlock by copying the id.
    Builtin(crate::base::builtin::Builtin),
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
    /// split as [`Shared`](Obj::Shared); the core holds one value by handle (M3.3).
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

/// How `==` decides whether two heap objects are the same value — the ONE classification of
/// identity (TICKET-177, wave 16 Family 5). `Vm::values_equal_guarded` compares `Core` keys and
/// `Vm::slot_identity_in` (the `Atomic.cas` refusal) looks for `Slot`. The match below is
/// exhaustive with no `_` arm, so a new `Obj` variant must pick an answer before the crate builds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Identity {
    /// Compared by content (`values_equal_guarded`'s structural arms). A crossing copies the
    /// content, so equality survives it.
    Content,
    /// A handle to a core outside every heap (`Arc`), or a raw C address (`ptr`). Every crossing
    /// wraps the SAME core in a fresh slot, so this key — not the slot — is the handle's identity.
    Core(CoreKey),
    /// Equal only to its own heap slot. A crossing (or an `Atomic` store) re-materializes it in a
    /// fresh slot, so equality does not survive one. The `str` names the kind for a diagnostic.
    Slot(&'static str),
}

/// A handle's identity: its variant plus the core's address. The variant keeps a raw `ptr`
/// address from ever equalling some core's `Arc` address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CoreKey(std::mem::Discriminant<Obj>, usize);

impl Obj {
    /// The elements when this object is a list. Every other kind answers `None`.
    #[inline]
    pub fn as_list(&self) -> Option<&RVec<Value>> {
        match self {
            Obj::List(v) => Some(v),
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::Tuple(_)
            | Obj::Map(_)
            | Obj::Set(_)
            | Obj::Struct { .. }
            | Obj::Enum { .. }
            | Obj::Cell(_)
            | Obj::Func { .. }
            | Obj::Closure { .. }
            | Obj::Module(_)
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
            | Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Reader(_)
            | Obj::Generator(_) => None,
        }
    }

    /// The table when this object is a map. Every other kind answers `None`.
    #[inline]
    pub fn as_map(&self) -> Option<&MapData> {
        match self {
            Obj::Map(m) => Some(m),
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::List(_)
            | Obj::Tuple(_)
            | Obj::Set(_)
            | Obj::Struct { .. }
            | Obj::Enum { .. }
            | Obj::Cell(_)
            | Obj::Func { .. }
            | Obj::Closure { .. }
            | Obj::Module(_)
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
            | Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Reader(_)
            | Obj::Generator(_) => None,
        }
    }

    /// The variant id and payload when this object is an enum value. Every other kind answers
    /// `None`.
    #[inline]
    pub fn as_enum(&self) -> Option<(u32, &RVec<Value>)> {
        match self {
            Obj::Enum {
                variant_id,
                payload,
            } => Some((*variant_id, payload)),
            Obj::Set(_)
            | Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::List(_)
            | Obj::Tuple(_)
            | Obj::Map(_)
            | Obj::Struct { .. }
            | Obj::Cell(_)
            | Obj::Func { .. }
            | Obj::Closure { .. }
            | Obj::Module(_)
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
            | Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Reader(_)
            | Obj::Generator(_) => None,
        }
    }

    /// The table when this object is a set. Every other kind answers `None`.
    #[inline]
    pub fn as_set(&self) -> Option<&SetData> {
        match self {
            Obj::Set(s) => Some(s),
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::List(_)
            | Obj::Tuple(_)
            | Obj::Map(_)
            | Obj::Struct { .. }
            | Obj::Enum { .. }
            | Obj::Cell(_)
            | Obj::Func { .. }
            | Obj::Closure { .. }
            | Obj::Module(_)
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
            | Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Reader(_)
            | Obj::Generator(_) => None,
        }
    }

    pub fn identity(&self) -> Identity {
        let core = |addr: usize| Identity::Core(CoreKey(std::mem::discriminant(self), addr));
        match self {
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::List(_)
            | Obj::Tuple(_)
            | Obj::Map(_)
            | Obj::Set(_)
            | Obj::Struct { .. }
            | Obj::Enum { .. } => Identity::Content,
            // A global builtin fn value is carried by NAME and compares by name — a value, no core.
            Obj::Builtin(_) => Identity::Content,
            Obj::Channel(c) => core(Arc::as_ptr(c) as usize),
            Obj::Shared(c) => core(Arc::as_ptr(c) as usize),
            Obj::RwShared(c) => core(Arc::as_ptr(c) as usize),
            Obj::Atomic(c) => core(Arc::as_ptr(c) as usize),
            Obj::AtomicInt(c) => core(Arc::as_ptr(c) as usize),
            Obj::Executor(c) => core(Arc::as_ptr(c) as usize),
            Obj::Socket(c) => core(Arc::as_ptr(c) as usize),
            Obj::Listener(c) => core(Arc::as_ptr(c) as usize),
            Obj::Writer(c) => core(Arc::as_ptr(c) as usize),
            Obj::Reader(c) => core(Arc::as_ptr(c) as usize),
            // Two `ptr` handles are the same iff they hold the same address; `std.ffi.null()` twice,
            // or a `ptr` that crossed, is a distinct slot around one address.
            Obj::Ptr(addr) => core(*addr),
            // A bare Func, Native or Cffi is one slot per fn per heap (Vm::fn_value, TICKET-215), so its
            // == survives a crossing; it stays Slot (DEC-177). Atomic.cas refuses every fn value, a
            // Builtin included, through Vm::callable (TICKET-264).
            Obj::Func { .. } | Obj::Closure { .. } | Obj::Native { .. } | Obj::Cffi(_) => {
                Identity::Slot("a function value")
            }
            Obj::Iter { .. } => Identity::Slot("an iterator"),
            Obj::Generator(_) => Identity::Slot("a generator"),
            Obj::Module(_) => Identity::Slot("a module"),
            Obj::Cell(_) => Identity::Slot("a captured cell"),
        }
    }
}

/// One object's OWNED BACKING in bytes (`Vec`/`Box` capacities), excluding the fixed
/// `size_of::<Obj>()` slot cost that every live slot pays alike. The single sizing table for the
/// heap: `live_bytes`, `alloc` and `write`'s settle all read it, so a new `Obj` variant is
/// sized once instead of drifting between three copies.
///
/// **Core arms deliberately score 0, and take NO lock.** What a core holds is charged by
/// `live_bytes`'s own once-per-`Arc` arms. Reaching `core.inner`/`core.q` from inside `write` would
/// re-take a lock the caller may already hold — `std::sync::Mutex` is not reentrant (a job
/// capturing its own executor hung, rc=124, under a cap only). Do not add a core arm here.
#[inline]
fn obj_bytes_shallow(obj: &Obj) -> usize {
    match obj {
        Obj::Str(s) => s.as_str().len(),
        Obj::Bytes(b) => b.len(),
        Obj::ByteArray(b) => b.capacity(),
        Obj::Iter { items, .. } => items.capacity() * std::mem::size_of::<Value>(),
        Obj::List(v) => v.capacity() * std::mem::size_of::<Value>(),
        Obj::Tuple(v) => v.capacity() * std::mem::size_of::<Value>(),
        Obj::Struct { fields, .. } => fields.heap_bytes(),
        Obj::Enum { payload, .. } => payload.capacity() * std::mem::size_of::<Value>(),
        Obj::Closure { captured, .. } => captured.capacity() * std::mem::size_of::<Value>(),
        Obj::Module(m) => m.slots.capacity() * std::mem::size_of::<Value>(),
        // Map/Set: the entry block and the index table.
        Obj::Map(m) => m.heap_bytes(),
        Obj::Set(s) => s.heap_bytes(),
        Obj::BigInt(_)
        | Obj::FloatBox(_)
        | Obj::Cell(_)
        | Obj::Func { .. }
        | Obj::Native { .. }
        | Obj::Builtin(_)
        | Obj::Cffi(_)
        | Obj::Ptr(_)
        | Obj::Channel(_)
        | Obj::Shared(_)
        | Obj::RwShared(_)
        | Obj::Atomic(_)
        | Obj::AtomicInt(_)
        | Obj::Executor(_)
        | Obj::Socket(_)
        | Obj::Listener(_)
        | Obj::Writer(_)
        | Obj::Reader(_)
        | Obj::Generator(_) => 0,
    }
}

/// The object heap, as one `Vm` sees it: a handle on a [`HeapCore`] (`vm/heap_core.rs`), which
/// holds the slots, the free list, the mark and copy bits and the shared counters. Every `Vm` of
/// one run holds a handle on the run's one core (`Vm::spawn_worker`). The handle keeps what is its
/// own: a cache of free slots, the counts it has not yet added to the core's trigger and its copy
/// log.
///
/// The collector itself (root tracing) lives on the VM, which owns the roots; the heap provides the
/// slot-level primitives: [`mark`](Heap::mark), [`children`](Heap::children),
/// [`sweep`](Heap::sweep), and the allocation-driven [`should_collect`](Heap::should_collect)
/// trigger. Collection runs only at instruction boundaries (see `Vm::run_until`), so every live
/// value is reachable from the VM roots — there are no mid-opcode temporaries to miss.
#[derive(Debug)]
pub struct Heap {
    core: Arc<HeapCore>,
    /// Free slots taken from the core in batches (`HeapCore::alloc_slot`), so `alloc` pops one
    /// without a lock. Given back to the core at [`sweep`](Heap::sweep) and on drop.
    free: Vec<u32>,
    /// Allocations not yet added to the core's run-wide count (`HeapCore::flush_allocs`): the
    /// trigger is run-wide, and a handle adds its count at most every `FLUSH_EVERY` allocations.
    since_gc: usize,
    /// The `since_gc` count below which [`should_collect`](Heap::should_collect) answers `false`
    /// without reading the core. A cache with one refresh point: `should_collect` sets it to what
    /// is left of the core's `next_gc` (at most `FLUSH_EVERY`) when it reads the core with no
    /// `--max-heap` cap live. 0 means "read the core at the next call": [`sweep`](Heap::sweep), a
    /// collection another handle ran ([`observe_collection`](Heap::observe_collection)),
    /// [`request_collect`](Heap::request_collect) and [`set_mem_cap`](Heap::set_mem_cap) set it to
    /// 0, since each can lower the answer.
    gc_check_at: usize,
    /// BYTES charged since the last collection — the `since_gc` sibling for growth that allocates
    /// (almost) no `Obj`s. A monotonic pacing **HINT**, never accounting: it only decides WHEN to
    /// sample, `live_bytes()` remains the sole measure of what is live (so a replacing store charges,
    /// and a `recv`/`pop` never decrements — under-triggering leaves the `--max-heap` guard failing
    /// open, over-triggering just costs a sweep). An atomic because two of the three charge points
    /// hold `&self` and a `RwShared` reader borrows the heap from another thread (TICKET-246); only
    /// the heap's owner writes it, so a `Relaxed` load then store is enough.
    ///
    /// Every byte a heap gains arrives one of exactly three ways, and each has exactly one funnel:
    /// a NEW object → [`alloc`](Heap::alloc); growth IN PLACE inside an existing one →
    /// [`write`](Heap::write) (see `pending_mut`); a retired `RVec` block or a nursery's charge →
    /// [`charge_bytes`](Heap::charge_bytes). All three charge here, with the cap on or off (M1.7;
    /// M3.3 deleted the cap-only wire charge with the box copies): a task's garbage now
    /// waits in the run's one heap for a collection, and a task's copy of a large global is ONE
    /// object that the decoder fills through `write`, so the object count alone let 2000 such
    /// copies (1.6 GB, `benches/sched/storm.chz`) pile up before one sweep.
    ///
    /// It has to count BYTES and not events. Instructions, allocations and wire crossings are all
    /// proxies, and each has a shape that adds unbounded bytes without moving it: an instruction tick
    /// is defeated by `big.extend(chunk)` (measured ~240 MB past an 8 MB cap in ~1200 instructions),
    /// the object count by `s = s + s` ×22 (41 MB in 22 allocations) and by
    /// `"x".repeat(20000000)` (20 MB in ONE).
    since_gc_bytes: AtomicUsize,
    /// The in-place-growth half of `since_gc_bytes`: the object handed out by the last
    /// [`write`](Heap::write) and its payload size AT THAT MOMENT. The next settle — the next
    /// `write`, or [`should_collect`](Heap::should_collect) — re-measures the same object and
    /// charges the difference. A plain field: every door that reads or writes it holds `&mut self`
    /// (M1.7 made `should_collect` take `&mut`). Armed with the cap on or off (M1.7: see
    /// `since_gc_bytes`).
    ///
    /// This is the door for byte growth that allocates NOTHING: `xs.push(i)` /
    /// `m[k] = v` / `s.insert(x)` / `ba.push(b)` / `big.extend(chunk)` all append into the `Vec` behind
    /// an EXISTING `Obj`, so neither the object count nor the wire counter moves and `sweep()` never
    /// runs — `over_cap` is assigned nowhere else, so the cap failed OPEN. Measured on the release
    /// binary against `--max-heap=8000000`: an 80 M-element `List[int]` grown by `push` PASSED at
    /// 617.8 MB (**77× the cap**), and one `big.extend(chunk)` loop PASSED at ~240 MB in ~1200
    /// instructions.
    ///
    /// Why here and not at each growth site: `push`/`insert`/`add`/`extend`/the map index-store/… is
    /// an open N-way set, and charging some arms of it is the mistake this repo has already been
    /// bitten by twice (`docs/contributing/gaps.md` W7-22). `write` is the SOLE write door (`init_mut` calls it):
    /// `Slot.obj` is `pub(super)` in `vm/heap_slab.rs`, but the slab is reachable only through the
    /// private `Heap.core` field, so no code outside this file can reach a slot (`vm/heap_core.rs`
    /// writes a slot only to allocate or free it). A charge here cannot be forgotten by a new
    /// container method.
    ///
    /// Why deferred rather than measured on both sides of the call: `write` HANDS OUT the handle the mutator runs on,
    /// so the "after" size does not exist until the caller is done with it. Settling at the next door
    /// (or at the next `should_collect`, which is every instruction) is what makes it exact without a
    /// wrapper at 65 call sites.
    ///
    /// Sound against a freed slot because `sweep()` — the only thing that frees, and so the only
    /// thing that can make `alloc` re-hand this slot to a different object — clears it.
    pending_mut: Option<(GcRef, usize)>,
    /// TICKET-246 — copy marks recorded for one box call (see `begin_copy_log`), and whether
    /// `set_copied` is recording now. The log is made and registered with the core at the first
    /// `begin_copy_log`, so `sweep` can drop a freed slot from it.
    copy_log: Option<CopyLog>,
    copy_logging: bool,
    /// The objects this handle's last [`sweep`](Heap::sweep) freed that own an `Arc` core or a
    /// file: their drop can make a syscall (a buffered writer's flush, a socket's close), so the
    /// collector drops them after it releases the world (`Vm::gc_collect`), not inside the stop.
    graveyard: Vec<Obj>,
    /// M2: the `RVec` blocks this handle's grows retired, not yet handed to the core
    /// (`adopt_retired`; `vm/rvec.rs`).
    retired: RetireList,
}

/// How many allocations a handle counts before it adds them to the core's run-wide count.
const FLUSH_EVERY: usize = 256;

/// How many charged bytes a handle holds before its next boundary adds them to the run's count
/// (`Heap::charge_own`): one allocation can carry megabytes.
const FLUSH_BYTES: usize = 256 * 1024;

/// The smallest retired-bytes goal (`Heap::retired_gc_threshold`).
const MIN_RETIRED_GOAL: usize = 1 << 20;

/// TICKET-246 / M3.3 — tasks on other threads read the run's one heap through `&Heap` at once (a
/// box's value is shared by handle). A new `Cell` / `RefCell` in `Heap` or `Obj` must fail here, never be worked around with
/// `unsafe impl Sync`.
const _: fn() = || {
    fn s<T: Sync + Send>() {}
    s::<Heap>();
};

impl Default for Heap {
    fn default() -> Self {
        Heap::on(Arc::new(HeapCore::default()))
    }
}

impl Drop for Heap {
    fn drop(&mut self) {
        // A dropped handle's counts still pace the run (a task's payload `Vm` allocates its rebuilt
        // arguments and is dropped before the task runs). The `write` record is NOT read here:
        // the thread may have left the run's heap, and a sweep may run. It was settled while the
        // thread was attached (`Vm::gc_leave`, `ReadyWorker::into_fiber`).
        self.core.flush_allocs(self.since_gc);
        self.core.flush_bytes(*self.since_gc_bytes.get_mut());
        self.core.give_back(&mut self.free);
        // A retired block is freed only by a stop, never here.
        self.core.adopt_retired(&self.retired);
        if let Some(log) = &self.copy_log {
            self.core.unregister_copy_log(log);
        }
    }
}

impl Heap {
    pub fn new() -> Self {
        Heap::default()
    }

    /// A fresh handle on `core`.
    fn on(core: Arc<HeapCore>) -> Self {
        Heap {
            core,
            free: Vec::new(),
            since_gc: 0,
            gc_check_at: 0,
            since_gc_bytes: AtomicUsize::new(0),
            pending_mut: None,
            copy_log: None,
            copy_logging: false,
            graveyard: Vec::new(),
            retired: RetireList::default(),
        }
    }

    /// A new handle on this heap's core: it allocates slots no other handle holds and reads every
    /// object of the core. Two handles can reach one slot through safe code (a `get` on one while
    /// the other's `write` grows the object reads a retired buffer, `vm/rvec.rs`), so the protocol
    /// below is the caller's.
    ///
    /// # Safety
    ///
    /// For the life of both handles:
    /// - a handle writes the objects of the fiber it runs, and the run's module objects and what
    ///   their globals reach (M3.1), which every handle shares: each such write stores through an
    ///   atomic slot or a lock ([`write`](Heap::write)), and a race between two of them is the
    ///   program's bug, as in Go;
    /// - a sweep ([`sweep`](Heap::sweep)) runs only while every other handle is stopped at a
    ///   safepoint or detached, through the run's `GcCoord` (`vm/gc_coord.rs`, `Vm::gc_collect`).
    pub(super) unsafe fn attach(&self) -> Heap {
        Heap::on(Arc::clone(&self.core))
    }

    /// Whether this handle is its core's only one: a collection with no stop of the world is
    /// sound only then (`Vm::gc_collect` with no token).
    pub(super) fn is_sole_handle(&self) -> bool {
        Arc::strong_count(&self.core) == 1
    }

    /// Mark `h` as an airlock copy (D4 layer C, TICKET-169), and record it while a copy log is on.
    pub fn set_copied(&mut self, h: GcRef) {
        self.core.set_copied(h.0 as usize);
        if self.copy_logging
            && let Some(log) = &self.copy_log
        {
            lock(log).push(h.0);
        }
    }

    /// TICKET-246 — start recording the objects [`set_copied`](Heap::set_copied) marks. A box call
    /// records the marks made while it rebuilds its closure (the closure's captures), so it can
    /// clear exactly those when the call ends.
    pub fn begin_copy_log(&mut self) {
        match &self.copy_log {
            Some(log) => lock(log).clear(),
            None => {
                let log = CopyLog::default();
                self.core.register_copy_log(Arc::clone(&log));
                self.copy_log = Some(log);
            }
        }
        self.copy_logging = true;
    }

    /// Stop recording; the recorded marks stay until [`unmark_copy_log`](Heap::unmark_copy_log).
    pub fn end_copy_log(&mut self) {
        self.copy_logging = false;
    }

    /// Clear the mark of every recorded object that is still live, and forget the record. A
    /// capture the closure stored into the box state is the box's own value from here on.
    pub fn unmark_copy_log(&mut self) {
        self.copy_logging = false;
        let Some(log) = &self.copy_log else { return };
        for i in std::mem::take(&mut *lock(log)) {
            self.core.clear_copied(i as usize);
        }
    }

    /// Test whether `h` is an airlock copy (see `HeapCore::is_copied`).
    #[inline]
    pub fn is_copied(&self, h: GcRef) -> bool {
        self.core.is_copied(h.0 as usize)
    }

    /// This heap's identity: its core's (see `HeapCore::id`), shared by every handle of the run.
    #[inline]
    pub fn id(&self) -> u64 {
        self.core.id()
    }

    /// D4 (TICKET-240): unmark one object the crossing site built, which no parent binding can
    /// reach. `Vm::unmark_fresh` decides which objects, from the checker's shape.
    #[inline]
    pub fn unset_copied(&mut self, h: GcRef) {
        self.core.clear_copied(h.0 as usize)
    }

    /// Allocate an object, returning its handle. Reuses a free slot when available.
    #[inline]
    pub fn alloc(&mut self, obj: Obj) -> GcRef {
        self.since_gc += 1;
        // Byte funnel 1 of 3 (see `since_gc_bytes`): the only constructor. `since_gc` alone paces
        // MANY-small-object growth; this paces FEW-huge-object growth, which it cannot see —
        // `"x".repeat(20000000)` is one allocation carrying 20 MB, and `s = s + s` ×22 is 22 of them
        // carrying 41 MB, both well under the 256-object threshold. M1.7: charged with the cap off
        // too (see `since_gc_bytes`).
        self.charge_own(obj_bytes_shallow(&obj));
        match self.free.pop() {
            // SAFETY: `idx` is from this handle's own free cache, which `alloc_slot` filled; the
            // gc_coord invariant (a sweep runs only with every other handle stopped or detached,
            // `vm/gc_coord.rs`) keeps the sweep off it while this handle runs.
            Some(idx) => unsafe { self.core.reuse(idx, obj) },
            None => self.core.alloc_slot(&mut self.free, obj),
        }
    }

    #[inline]
    pub fn get(&self, h: GcRef) -> &Obj {
        self.core
            .get(h)
            .obj
            .as_ref()
            .expect("dangling GcRef (object was collected while still reachable)")
    }

    /// The `&mut Obj` of an object that this call allocated and has not published: no other
    /// thread holds its `GcRef` yet (it is in no object, global, stack or channel another thread
    /// reads). The airlock and key-snapshot rebuilds use it to fill or replace a placeholder they
    /// built. Every other write goes through [`write`](Heap::write) and a `&self` mutator. No
    /// cheap check proves "unpublished" (it is a reachability fact), so none is asserted.
    #[inline]
    pub fn init_mut(&mut self, h: GcRef) -> &mut Obj {
        self.write(h);
        // SAFETY: `&mut self` excludes this handle's other references, and the caller's contract
        // above keeps every other thread off `h`: no other handle has its `GcRef`.
        unsafe { self.core.slot_mut(h) }
            .obj
            .as_mut()
            .expect("dangling GcRef (object was collected while still reachable)")
    }

    /// Arm a write of `h` and hand back this handle shared: the door for every write to an object
    /// another thread can read. Its mutators take `&self` and store through atomic slots (an
    /// `RVec`, an `RCell`, a map entry, `vm/rvec.rs`) or a lock (a module's names). A list write
    /// is `let heap = heap.write(h);` then `heap.get(h)` and `list.push(heap, v)`.
    #[inline]
    pub fn write(&mut self, h: GcRef) -> &Heap {
        // Byte funnel 2 of 3 (see `since_gc_bytes` / `pending_mut`): the sole write door, and so
        // the sole door for growth INSIDE an existing object. Settle whatever the previous write
        // armed, then arm this one. M1.7: with the cap off too (see `since_gc_bytes`).
        // One size read serves both halves when the last write armed `h` too (a push loop): it is
        // the settle's "now" and the new record's "before".
        let bytes = obj_bytes_shallow(self.get(h));
        self.settle_knowing(Some((h, bytes)));
        self.pending_mut = Some((h, bytes));
        self
    }

    /// Charge the growth of the object the last [`write`](Heap::write) handed out, and disarm.
    /// A SHRINK charges 0 — monotonic, exactly like the wire counter (`live_bytes()` is what
    /// measures; this only decides when to look). M1.7: `Vm::swap_ctx` settles at every context
    /// switch, so a handle never keeps a record of an object another thread may write.
    #[inline]
    pub(super) fn settle_pending_mut(&mut self) {
        self.settle_knowing(None);
    }

    /// The settle, given `known`: an object and its size just read. When the armed record is that
    /// object, its size is not read again.
    #[inline]
    fn settle_knowing(&mut self, known: Option<(GcRef, usize)>) {
        if let Some((h, before)) = self.pending_mut.take() {
            let now = match known {
                Some((k, bytes)) if k == h => bytes,
                Some(_) | None => obj_bytes_shallow(self.get(h)),
            };
            self.charge_own(now.saturating_sub(before));
        }
    }

    /// [`charge_bytes`](Heap::charge_bytes) for a door that holds `&mut self` (no atomic access).
    /// Once this handle holds `FLUSH_BYTES` charged bytes, it empties its cached object threshold,
    /// so its next boundary adds them to the run's count and reads the byte goal.
    #[inline(always)]
    fn charge_own(&mut self, n: usize) {
        let held = self.since_gc_bytes.get_mut();
        *held = held.saturating_add(n);
        if *held >= FLUSH_BYTES {
            self.gc_check_at = 0;
        }
    }

    /// Live object count — for GC assertions / bounded-heap tests.
    #[cfg(test)]
    pub fn live(&self) -> usize {
        self.core.live()
    }

    /// Whether slot `h` holds an object (tests: a freed slot holds none).
    #[cfg(test)]
    pub fn holds(&self, h: GcRef) -> bool {
        self.core.get(h).obj.is_some()
    }

    /// Charge bytes against the collection trigger (see `since_gc_bytes`): byte funnel 3 of 3. An
    /// `RVec` grow charges the block it retires (`vm/rvec.rs`) and a nursery its scheduler's
    /// charge, cap or no cap.
    #[inline]
    pub fn charge_bytes(&self, n: usize) {
        let charged = self.since_gc_bytes.load(Ordering::Relaxed);
        self.since_gc_bytes
            .store(charged.saturating_add(n), Ordering::Relaxed);
    }

    /// How many charged bytes force a sweep. Under a live cap: a quarter of the cap bounds the
    /// overshoot between samples, and the 64 KB floor stops a tiny cap from GC-ing per store. With
    /// the cap off (M1.7, the shape of Go's GOGC=100 goal): the live bytes of the last sweep, at
    /// least 16 MiB. Measured on `benches/sched/storm.chz` (2000 tasks, each one 800 KB copy, four
    /// threads): a 4 MiB floor took 230 stops and +4.5% wall time against base, 16 MiB 84 stops
    /// and +1.5%.
    fn bytes_gc_threshold(&self) -> usize {
        match self.core.mem_cap() {
            0 => self.core.last_live_bytes().max(16 << 20),
            cap => (cap / 4).max(64 * 1024),
        }
    }

    /// How many retired `RVec` bytes force a sweep: the live bytes of the last sweep, at least
    /// [`MIN_RETIRED_GOAL`] (Go's GOGC=100 lets the garbage grow to the live heap). A retired block
    /// is garbage that only a sweep frees, so it needs no 16 MiB floor: on `benches/chz/map.chz`
    /// (200k inserts, no other object) the floor kept every old block until the end.
    fn retired_gc_threshold(&self) -> usize {
        self.core.last_live_bytes().max(MIN_RETIRED_GOAL)
    }

    /// The retired `RVec` bytes that wait for a sweep: this handle's list and the run's.
    fn retired_bytes(&self) -> usize {
        self.retired.bytes() + self.core.retired_bytes()
    }

    /// Whether enough has been allocated since the last collection to warrant one.
    ///
    /// W6-10 (sampling half): under a live `--max-heap` cap, BYTE growth ALSO paces a sweep. Without
    /// it a program that grows megabytes while allocating ~2 `Obj`s per iteration never reaches the
    /// object-count threshold, so `sweep()` never runs, `over_cap` is never evaluated and the cap
    /// fails OPEN (counting the bytes correctly in `live_bytes` does nothing if nobody ever looks).
    ///
    /// W7-28 — the byte counter now covers ALL THREE ways a heap gains bytes, not just the off-heap
    /// wire one: see `since_gc_bytes`. The settle here is what makes `write`'s deferred charge
    /// exact at an instruction boundary, which is the only place the cap is read.
    ///
    /// M1.7 — the trigger is run-wide: this handle adds its counts to the core's
    /// (`HeapCore::flush_allocs` / `flush_bytes`) and compares the run's totals since the last
    /// collection with `next_gc` and the byte threshold (`Heap::bytes_gc_threshold`). With the cap
    /// off the bytes (`alloc` and `write` growth) are read each time the cached object threshold
    /// is reached, at most `FLUSH_EVERY` allocations apart, and at the next boundary after this
    /// handle charged `FLUSH_BYTES`.
    #[inline]
    pub fn should_collect(&mut self) -> bool {
        if self.since_gc < self.gc_check_at {
            return false;
        }
        self.should_collect_slow()
    }

    #[cold]
    #[inline(never)]
    fn should_collect_slow(&mut self) -> bool {
        let allocs = self.core.flush_allocs(std::mem::take(&mut self.since_gc));
        let next_gc = self.core.next_gc();
        self.settle_pending_mut();
        let bytes = self
            .core
            .flush_bytes(std::mem::take(self.since_gc_bytes.get_mut()));
        if allocs >= next_gc
            || bytes >= self.bytes_gc_threshold()
            || self.retired_bytes() >= self.retired_gc_threshold()
        {
            self.gc_check_at = 0;
            return true;
        }
        if self.core.mem_cap() == 0 {
            self.gc_check_at = (next_gc - allocs).min(FLUSH_EVERY);
        }
        false
    }

    /// Another handle's collection ran while this one was stopped or detached: zero the counts it
    /// already fed and the cache it read, and disarm `write`'s record, whose slot that sweep can
    /// have freed (a settle would then read a dead slot).
    pub(super) fn observe_collection(&mut self) {
        self.since_gc = 0;
        self.gc_check_at = 0;
        *self.since_gc_bytes.get_mut() = 0;
        self.pending_mut = None;
    }

    /// W6-10s — force the next [`should_collect`](Heap::should_collect), so a heap that was HANDED a
    /// large payload samples the `--max-heap` cap even though it allocated almost no `Obj`s doing so.
    /// `over_cap` is assigned only in `sweep()`, and `sweep()` only runs when `should_collect()`
    /// fires; a worker heap holding a rebuilt `List` of any size counts ONE object, so the growth
    /// trigger never moves and the guard fails OPEN. Set by `Vm::spawn_worker` under a live cap only
    /// (so cap-off pacing is untouched), and consumed at the task's first instruction boundary in
    /// `run_until` — the first point where every live value is properly rooted.
    ///
    /// Every task (`spawn`, `parallel:`, an `Executor` job) is also sampled before dispatch by
    /// `Vm::sample_mem_cap` in `Vm::start_task` — see the note in `Vm::spawn_worker`.
    pub fn request_collect(&mut self) {
        self.core.request_collect();
        self.gc_check_at = 0;
    }

    /// Mark one object reachable. Returns `true` if it was *newly* marked (caller should then
    /// trace its children), `false` if already marked (a cycle / shared reference — stop).
    pub fn mark(&mut self, h: GcRef) -> bool {
        self.core.mark(h.0 as usize)
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
            Obj::Iter { items, .. } => items.iter().for_each(|v| push(&v)),
            Obj::List(items) => items.iter().for_each(|v| push(&v)),
            Obj::Tuple(items) => items.iter().for_each(&mut push),
            Obj::Map(m) => m.entries.iter().for_each(|(_, k, v)| {
                push(&k);
                push(&v);
            }),
            Obj::Set(s) => s.entries.iter().for_each(|(_, e)| push(&e)),
            Obj::Struct { fields, .. } => fields.iter().for_each(|v| push(&v)),
            Obj::Enum { payload, .. } => payload.iter().for_each(|v| push(&v)),
            // A boxed local's cell: the inner value may be a heap object — trace it.
            Obj::Cell(v) => push(&v.get()),
            Obj::Func { home, .. } => out.push(*home),
            Obj::Closure { captured, home, .. } => {
                captured.iter().for_each(|v| push(&v));
                out.push(*home);
            }
            Obj::Module(m) => m.slots.iter().for_each(|v| push(&v)),
            Obj::Native { .. } => {}
            // A GC leaf: holds only a name, no embedded `GcRef`s (like `Native`).
            Obj::Builtin(_) => {}
            Obj::Cffi(_) => {}
            Obj::Ptr(_) => {}
            // M3.2: a channel and an `Atomic` hold `Value`s of this one heap. Read them under the
            // core lock: no thread holds it across a safepoint, a GC door or a user call, so the
            // wait here is a short copy at most.
            Obj::Channel(core) => core
                .q
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .for_each(|v| push(&v)),
            // M3.3 — a box holds one value of this heap in an atomic slot: read with no lock, so
            // a holder stopped inside its closure never blocks the mark.
            Obj::Shared(core) => push(&core.v.get()),
            Obj::RwShared(core) => push(&core.v.get()),
            Obj::Atomic(core) => push(&core.load()),
            // `AtomicInt` holds a plain i64 — no heap refs to trace.
            Obj::AtomicInt(_) => {}
            // An Executor holds no value: a job's outcome is in its Task channel, and a running
            // job's context is in the run's root registry.
            Obj::Executor(_) => {}
            // D6/R2/R2b: a socket/listener/writer/reader core holds only an fd/buffer + a key — no heap refs.
            Obj::Socket(_) | Obj::Listener(_) | Obj::Writer(_) | Obj::Reader(_) => {}
            // Experimental generators — root the suspended frames/stack/args (see `gc_roots`).
            Obj::Generator(g) => out.extend(g.gc_roots()),
        }
        out
    }

    /// Stamp `r`'s origin span (W8-22), overwriting any prior entry for its slot.
    pub fn set_err_span(&mut self, r: GcRef, span: Span) {
        self.core.set_err_span(r, span);
    }

    /// The origin span stamped on `r`, or `None` if it was never stamped (a user-constructed
    /// `Err`, a struct error, or a slot swept since the stamp). Returns `Span` by value: `Span` is
    /// `Copy`, so the core's `err_spans` lock is released here rather than held across a caller's
    /// subsequent `alloc`/`alloc_str`.
    pub fn err_span(&self, r: GcRef) -> Option<Span> {
        self.core.err_span(r)
    }

    /// Free every unmarked object and clear all marks for the next cycle. Resets the allocation
    /// counter and grows the next-collection threshold relative to the surviving set.
    pub fn sweep(&mut self) {
        // The cached free slots go back on top of the core's stack first, so the core's free list
        // is the whole free list and the next `alloc` reuses the slot it would have reused.
        self.core.give_back(&mut self.free);
        self.core.adopt_retired(&self.retired);
        // SAFETY: the collector protocol of `attach`: `Vm::gc_collect` sweeps with every other
        // handle stopped or detached, or with this handle the core's only one. A detached handle's
        // thread holds only a `Detached` handle, which reaches no heap object (`vm/gc_door.rs`).
        unsafe { self.core.sweep_slots(&mut self.graveyard) };
        self.since_gc = 0;
        self.gc_check_at = 0;
        *self.since_gc_bytes.get_mut() = 0;
        // Drop the armed `write` record: this is the ONLY thing that frees a slot, so it is also
        // the only thing that could leave the record pointing at a slot `alloc` then re-hands to a
        // different object. Clearing it here is what makes the settle sound.
        self.pending_mut = None;
        let lb = self.live_bytes();
        self.core.record_live_bytes(lb);
    }

    /// An upper estimate of the run's live bytes with no walk: the live bytes of the last sweep,
    /// plus what every handle charged since (the run's flushed count and this handle's own).
    pub(super) fn run_bytes_estimate(&self) -> usize {
        self.core
            .last_live_bytes()
            .saturating_add(self.core.charged_bytes())
            .saturating_add(self.since_gc_bytes.load(Ordering::Relaxed))
    }

    /// Hand this handle's retired `RVec` blocks to the run (`HeapCore::adopt_retired`).
    pub(super) fn adopt_retired(&self) {
        self.core.adopt_retired(&self.retired);
    }

    /// This handle's own retire list: an `RVec` grow puts the old block on it (`vm/rvec.rs`).
    pub(super) fn retire_list(&self) -> &RetireList {
        &self.retired
    }

    /// The bytes on the run's retire list (tests only).
    #[cfg(test)]
    pub(super) fn core_retired_bytes(&self) -> usize {
        self.core.retired_bytes()
    }

    /// Blocks on this handle's retire list and on the run's (tests only).
    #[cfg(test)]
    pub(super) fn retired_len(&self) -> (usize, usize) {
        (self.retired.len(), self.core.retired_len())
    }

    /// Retired index blocks (a map's or a set's old hash table, `vm/map_data.rs`) on this
    /// handle's list and the run's (tests only).
    #[cfg(test)]
    pub(super) fn retired_index_blocks(&self) -> usize {
        use super::map_data::IndexSlot;
        self.retired.count_of::<IndexSlot>() + self.core.retired_count_of::<IndexSlot>()
    }

    /// The objects the last sweep freed whose drop the collector runs after the world restarts
    /// (see `graveyard`).
    pub(super) fn take_graveyard(&mut self) -> Vec<Obj> {
        std::mem::take(&mut self.graveyard)
    }

    /// Approximate live heap footprint in bytes: each live slot's `Obj` plus the owned backing
    /// storage (Vec/Box capacities) of the container variants. For the Phase-1 8B-`Value` memory
    /// gate — not precise allocator accounting (Map/Set count entries backing only, not the index),
    /// but stable enough to compare before/after on a fixed workload. M1.7: the walk covers the
    /// run's one heap, so this is the whole run's live footprint (`--max-heap` caps it).
    pub fn live_bytes(&self) -> usize {
        // W6-10 — `from_wire` mints a FRESH `Obj::Channel`/`Executor` slot for every crossing, so the
        // heap can hold K alias slots for one core. Charge each core ONCE (by `Arc` identity): K
        // handles to one core are K views of one allocation. A HASH set: `live_bytes` runs on every
        // `sweep()`, and a linear scan would make the pass O(D²) in distinct cores. It stays
        // allocation-free until the first core slot (`HashSet::default` does not allocate).
        let mut cores: super::fxhash::FxHashSet<usize> = Default::default();
        let mut total = 0usize;
        for slot in self.core.iter() {
            let Some(obj) = &slot.obj else { continue };
            total += std::mem::size_of::<Obj>();
            total += match obj {
                // M3.2: a channel holds `Value`s; their objects are slots of this heap, which this
                // walk counts. The channel adds one word per value it holds.
                Obj::Channel(core) if cores.insert(Arc::as_ptr(core) as usize) => core
                    .q
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .held_bytes(),
                // W7-26r — what the Executor holds outside every heap: its finished jobs' retained
                // output, and the submit-time bytes of the jobs DISPATCHED BUT NOT YET FINISHED
                // (300 of them summing to 666 MB sailed past an 8 MB cap while every individual
                // heap stayed well under it; the submitter is charged until the job finishes,
                // TICKET-205). One facade, `ExecutorCore::held_bytes`.
                Obj::Executor(core) if cores.insert(Arc::as_ptr(core) as usize) => {
                    core.held_bytes()
                }
                // Every non-core variant — the plain owned backing, sized in ONE place so a new
                // `Obj` variant cannot be counted here and forgotten at the `alloc`/`write`
                // charges. A core arm whose guard above failed (already counted for this heap)
                // lands here too and correctly scores 0. M1.7 / M3.2: a `Shared` / `RwShared` box
                // and an `Atomic` score 0 here as well: what they hold is objects of this one heap,
                // which this walk counts.
                other @ Obj::Str(_)
                | other @ Obj::Bytes(_)
                | other @ Obj::ByteArray(_)
                | other @ Obj::BigInt(_)
                | other @ Obj::FloatBox(_)
                | other @ Obj::Iter { .. }
                | other @ Obj::List(_)
                | other @ Obj::Tuple(_)
                | other @ Obj::Map(_)
                | other @ Obj::Set(_)
                | other @ Obj::Struct { .. }
                | other @ Obj::Enum { .. }
                | other @ Obj::Cell(_)
                | other @ Obj::Func { .. }
                | other @ Obj::Closure { .. }
                | other @ Obj::Module(_)
                | other @ Obj::Native { .. }
                | other @ Obj::Builtin(_)
                | other @ Obj::Cffi(_)
                | other @ Obj::Ptr(_)
                | other @ Obj::Channel(_)
                | other @ Obj::Shared(_)
                | other @ Obj::RwShared(_)
                | other @ Obj::Atomic(_)
                | other @ Obj::AtomicInt(_)
                | other @ Obj::Executor(_)
                | other @ Obj::Socket(_)
                | other @ Obj::Listener(_)
                | other @ Obj::Writer(_)
                | other @ Obj::Reader(_)
                | other @ Obj::Generator(_) => obj_bytes_shallow(other),
            };
        }
        total
    }

    /// The high-water mark of [`live_bytes`](Heap::live_bytes) sampled at each `sweep()`.
    pub fn peak_live_bytes(&self) -> usize {
        self.core.peak_live_bytes()
    }

    /// `chezzi test --max-heap` — set the per-test live-heap cap in bytes (`0` = OFF). Clears any
    /// prior `over_cap` latch so a new test starts clean.
    pub fn set_mem_cap(&mut self, cap: usize) {
        self.core.set_mem_cap(cap);
        self.gc_check_at = 0;
        // Disarm `write`'s pending record along with the cap. Cap-off never arms it, so a
        // cap→0→cap toggle is the one way a record could outlive the `sweep()` that freed its slot
        // and be settled against a slot `alloc` re-handed to a different object. Production sets the
        // cap exactly once per `Vm`, so this is belt-and-braces — but it makes the settle's soundness
        // a local property of this file instead of an argument about every caller.
        self.pending_mut = None;
    }

    /// The configured `--max-heap` cap in bytes (`0` = OFF) — for the abort message.
    pub fn mem_cap(&self) -> usize {
        self.core.mem_cap()
    }

    /// True once `sweep()` saw `live_bytes()` exceed a non-zero `mem_cap` — the VM's hard-abort signal.
    pub fn over_cap(&self) -> bool {
        self.core.over_cap()
    }

    /// Reset the `over_cap` latch (per-test lifecycle), leaving `mem_cap` intact.
    pub fn clear_over_cap(&mut self) {
        self.core.clear_over_cap();
    }
}

#[cfg(test)]
mod iter_obj_tests {
    use super::*;

    /// TICKET-251 — `as_enum` answers an enum value's variant id and payload, and `None` for any
    /// other kind.
    #[test]
    fn as_enum_answers_only_enum_values() {
        let e = Obj::Enum {
            variant_id: 7,
            payload: RVec::new(),
        };
        assert_eq!(e.as_enum().map(|(id, p)| (id, p.len())), Some((7, 0)));
        assert!(Obj::List(RVec::new()).as_enum().is_none());
    }

    /// `Obj` is 40B (M2.3: no variant needs more; the cap is 64B). Pinned so a growth shows.
    #[test]
    fn obj_iter_within_size_cap() {
        assert_eq!(std::mem::size_of::<Obj>(), 40);
    }

    /// The per-slot `Vec` element must be exactly one `Obj` wide — the mark bit lives in the
    /// parallel `marks` bitset, not padding the slot. `Option<Obj>` is one `Obj` (Obj's
    /// spare-discriminant niche makes `None` free), so the mark/sweep scan never pulls a payload
    /// into cache to read a bool.
    #[test]
    fn slot_element_is_one_obj() {
        assert_eq!(
            std::mem::size_of::<crate::vm::heap_slab::Slot>(),
            std::mem::size_of::<Obj>()
        );
    }

    /// D4 layer C (TICKET-169): the copy mark is per-slot and cleared when the slot is reused.
    #[test]
    fn copied_bit_is_per_slot_and_cleared_on_reuse() {
        let mut heap = Heap::new();
        let a = heap.alloc(Obj::List(RVec::new()));
        let b = heap.alloc(Obj::List(RVec::new()));
        heap.set_copied(a);
        assert!(heap.is_copied(a));
        assert!(!heap.is_copied(b));
        // Neither GC-marked: sweep frees both. `free` is LIFO (pushed in index order 0,1), so the
        // first re-alloc reuses `b`'s slot and the second reuses `a`'s — the one that was marked.
        heap.sweep();
        let _reuse_b = heap.alloc(Obj::List(RVec::new()));
        let c = heap.alloc(Obj::List(RVec::new()));
        assert_eq!(
            c, a,
            "the freed slot must be reused for this test to prove anything"
        );
        assert!(
            !heap.is_copied(c),
            "a reused slot must not inherit the prior copy mark"
        );
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
        assert_eq!(std::mem::size_of::<Obj>(), 40, "Fields must not grow Obj");
    }

    /// `Fields` round-trips a ≤3-field vec as `Inline` and a >3-field vec as `Spill`, preserving
    /// `len`/`to_vec`; `get` and `set` bounds-check; and a `set` lands in the LIVE backing for BOTH
    /// variants (catches an aliased-temp-copy bug on the inline array or the spill block).
    #[test]
    fn fields_inline_and_spill_roundtrip() {
        // 2 fields → Inline
        let inl = Fields::from_vec(vec![Value::int(10), Value::int(20)]);
        assert!(matches!(inl, Fields::Inline { .. }));
        assert_eq!(inl.len(), 2);
        assert_eq!(inl.to_vec(), [Value::int(10), Value::int(20)]);
        assert_eq!(inl.get(1), Some(Value::int(20)));
        assert_eq!(inl.get(2), None);
        assert!(inl.set(0, Value::int(99)) && inl.set(1, Value::int(88)));
        assert!(!inl.set(2, Value::int(1)), "past the last field");
        assert_eq!(inl.to_vec(), [Value::int(99), Value::int(88)]);

        // 4 fields → Spill
        let sp = Fields::from_vec(vec![
            Value::int(1),
            Value::int(2),
            Value::int(3),
            Value::int(4),
        ]);
        assert!(matches!(sp, Fields::Spill(_)));
        assert_eq!(sp.len(), 4);
        assert_eq!(
            sp.to_vec(),
            [Value::int(1), Value::int(2), Value::int(3), Value::int(4)]
        );
        assert_eq!(sp.get(3), Some(Value::int(4)));
        assert_eq!(sp.get(4), None);
        assert!(sp.set(3, Value::int(77)) && sp.set(0, Value::int(66)));
        assert!(!sp.set(4, Value::int(1)), "past the last field");
        assert_eq!(
            sp.to_vec(),
            [Value::int(66), Value::int(2), Value::int(3), Value::int(77)]
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
            tid: crate::base::op::TID_NONE,
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
            tid: crate::base::op::TID_NONE,
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
            items: vec![Value::obj(elem), Value::int(7)].into(),
            pos: RCell::new(0),
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
        let r = h.alloc(Obj::List(
            vec![Value::int(1), Value::int(2), Value::int(3)].into(),
        ));
        let grown = h.live_bytes();
        // one slot (size_of::<Obj>) + the Vec's 3*size_of::<Value>() backing must register
        assert!(grown > empty + std::mem::size_of::<Obj>());
        let _ = r;
    }

    /// A map's live bytes count its entry block and its index table (8 B per slot).
    #[test]
    fn live_bytes_counts_map_index() {
        let mut h = Heap::new();
        let empty = h.live_bytes();
        let m = MapData::from_entries(
            (0..100)
                .map(|k| (k, Value::int(k as i64), Value::nil()))
                .collect(),
        );
        let want = std::mem::size_of::<Obj>() + 100 * 24 + 256 * 8;
        let _r = h.alloc(Obj::Map(m));
        assert_eq!(h.live_bytes() - empty, want);
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
        assert_eq!(std::mem::size_of::<Obj>(), 40); // unchanged
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

    /// `should_collect` caches the core's threshold on the handle (`gc_check_at`). Each write that
    /// can lower the answer must reach the next call: a sweep that lowers `next_gc`, a
    /// `request_collect`, and a cap set after the cache was filled.
    #[test]
    fn should_collect_sees_each_threshold_change() {
        let mut h = Heap::new();
        let live: Vec<GcRef> = (0..300).map(|_| h.alloc(Obj::List(RVec::new()))).collect();
        assert!(h.should_collect(), "300 allocations pass the 256 floor");
        for &r in &live {
            h.mark(r);
        }
        h.sweep();
        assert!(
            !h.should_collect(),
            "the threshold is now 600; the cache holds it"
        );
        h.sweep();
        for _ in 0..256 {
            h.alloc(Obj::List(RVec::new()));
        }
        assert!(
            h.should_collect(),
            "a sweep that freed everything lowered the threshold to 256"
        );
        h.sweep();
        assert!(!h.should_collect());
        h.request_collect();
        assert!(h.should_collect(), "request_collect forces the next call");
        h.sweep();
        assert!(!h.should_collect());
        h.set_mem_cap(8_000_000);
        h.charge_bytes(2_000_000);
        assert!(
            h.should_collect(),
            "a cap set after the cache was filled paces by bytes"
        );
    }

    /// W6-10 sampling half: off-heap wire bytes pace a sweep ONLY when a `--max-heap` cap is live.
    /// Cap OFF must be bit-for-bit today's object-count pacing; cap ON must sample before the
    /// off-heap growth can overshoot; `sweep()` must reset the counter like `since_gc`.
    #[test]
    fn wire_bytes_pace_a_sweep_only_under_a_cap() {
        // (a) cap OFF: a charge below the cap-off byte goal (16 MiB with nothing live) does not
        // collect.
        let mut h = Heap::new();
        h.charge_bytes(1024 * 1024);
        assert!(
            !h.should_collect(),
            "1 MiB is under the 16 MiB cap-off goal"
        );

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

    /// W7-28 — the OTHER two byte funnels (`alloc` and `write`), same contract as the wire-byte
    /// sibling above. These are the growth the object count cannot see: FEW-huge allocations, and
    /// in-place appends into an existing object that allocate nothing at all.
    #[test]
    fn alloc_and_in_place_growth_pace_a_sweep_only_under_a_cap() {
        let big = |n: usize| Obj::List(RVec::with_capacity(n));

        // (a) M1.7 — cap OFF, both funnels pace by bytes against the cap-off goal (the last
        // sweep's live bytes, at least 16 MiB): one 20 MB object collects, though it is one
        // object, and so does a 20 MB in-place growth of a small one (the wire decoder fills a
        // copy that way). Below the goal neither collects. A charge of `FLUSH_BYTES` or more
        // empties the handle's cached object threshold, so the next call reads the bytes.
        let mut h = Heap::new();
        h.alloc(big(1_000_000)); // 8 MB of backing
        assert!(!h.should_collect(), "8 MB is under the 16 MiB cap-off goal");
        h.alloc(big(2_500_000)); // +20 MB
        assert!(
            h.should_collect(),
            "a 20 MB allocation passes the cap-off goal"
        );
        let mut h = Heap::new();
        let r = h.alloc(big(1_000)); // 8 KB of backing
        let heap = h.write(r);
        if let Obj::List(v) = heap.get(r) {
            let xs: Vec<Value> = (0..2_500_000).map(Value::int).collect();
            v.extend_from_slice(heap, &xs); // in-place bulk growth, cap off
        }
        assert!(
            h.should_collect(),
            "20 MB of in-place growth passes the cap-off goal"
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

        // (c) cap ON, funnel 2 — ONE `write` that appends in bulk. This is `big.extend(chunk)`:
        // unbounded bytes in a single instruction, which no instruction tick can bound.
        let mut h = Heap::new();
        h.set_mem_cap(8_000_000);
        let r = h.alloc(Obj::List(RVec::new()));
        assert!(!h.should_collect(), "an empty list must not collect");
        let heap = h.write(r);
        if let Obj::List(v) = heap.get(r) {
            let xs: Vec<Value> = (0..400_000).map(Value::int).collect();
            v.extend_from_slice(heap, &xs); // 3.2 MB in one call
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
        let heap = h.write(r);
        if let Obj::List(v) = heap.get(r) {
            v.clear();
        }
        assert!(!h.should_collect(), "a shrink must charge 0, not underflow");

        // (e) the settle is armed by `write` and disarmed by whoever settles first, so a second
        // `write` charges the first one's growth rather than losing it.
        let mut h = Heap::new();
        h.set_mem_cap(8_000_000);
        let a = h.alloc(Obj::List(RVec::new()));
        let b = h.alloc(Obj::List(RVec::new()));
        let heap = h.write(a);
        if let Obj::List(v) = heap.get(a) {
            let xs: Vec<Value> = (0..150_000).map(Value::int).collect();
            v.extend_from_slice(heap, &xs); // 1.2 MB, never settled by should_collect
        }
        let heap = h.write(b);
        if let Obj::List(v) = heap.get(b) {
            let xs: Vec<Value> = (0..150_000).map(Value::int).collect();
            v.extend_from_slice(heap, &xs); // +1.2 MB = 2.4 MB > cap/4
        }
        assert!(
            h.should_collect(),
            "a second write must settle the first's growth, not drop it"
        );

        // (f) the 64 KB floor survives a tiny cap (shared with the wire threshold).
        let mut h = Heap::new();
        h.set_mem_cap(1000); // cap/4 = 250, floored to 65536
        h.alloc(big(4_096)); // exactly 32 KB
        assert!(!h.should_collect(), "64 KB floor must survive a tiny cap");
        h.alloc(big(4_096)); // +32 KB = exactly the floor
        assert!(h.should_collect());
    }

    /// M3.2 — a core's children are the `Value`s it holds, read under its lock: a channel's buffer,
    /// a parked sender's offer and a parked receiver's filled slot, and an `Atomic`'s value. An
    /// Executor holds none. A value whose only holder is a core survives a collection.
    #[test]
    fn a_core_traces_every_value_it_holds() {
        use crate::vm::core::{AtomicCore, Pending};
        let mut h = Heap::new();
        let queued = h.alloc(Obj::Str("queued".into()));
        let offered = h.alloc(Obj::Str("offered".into()));
        let filled = h.alloc(Obj::Str("filled".into()));
        let stored = h.alloc(Obj::Str("stored".into()));
        let garbage = h.alloc(Obj::Str("garbage".into()));

        let buf = Arc::new(ChannelCore {
            cap: Some(1),
            ..Default::default()
        });
        let p = Pending::new();
        {
            let mut q = buf.q.lock().unwrap();
            q.push(Value::obj(queued));
            q.offer(&p, 0, Value::obj(offered));
        }
        let rv = Arc::new(ChannelCore {
            cap: Some(0),
            ..Default::default()
        });
        let r = Pending::new();
        {
            let mut q = rv.q.lock().unwrap();
            q.slot(&r, 0);
            assert!(q.give(Value::obj(filled), None));
        }
        let at = Arc::new(AtomicCore::new(Value::obj(stored)));
        let bufr = h.alloc(Obj::Channel(Arc::clone(&buf)));
        let rvr = h.alloc(Obj::Channel(Arc::clone(&rv)));
        let atr = h.alloc(Obj::Atomic(Arc::clone(&at)));
        let exr = h.alloc(Obj::Executor(Arc::new(ExecutorCore::default())));
        assert_eq!(h.children(bufr), vec![queued, offered]);
        assert_eq!(h.children(rvr), vec![filled]);
        assert_eq!(h.children(atr), vec![stored]);
        assert!(h.children(exr).is_empty());

        let mut stack = vec![bufr, rvr, atr, exr];
        while let Some(x) = stack.pop() {
            if h.mark(x) {
                stack.extend(h.children(x));
            }
        }
        h.sweep();
        // `get` PANICS on a collected slot ("dangling GcRef"): each value live only through a
        // core survives, and the one no core holds is gone.
        for (g, text) in [
            (queued, "queued"),
            (offered, "offered"),
            (filled, "filled"),
            (stored, "stored"),
        ] {
            assert!(matches!(h.get(g), Obj::Str(s) if s.as_str() == text));
        }
        assert!(
            h.core.get(garbage).obj.is_none(),
            "an unheld value is swept"
        );
    }

    /// W6-10 / M3.2 — a channel charges one word per value it holds, once per CORE: `from_wire`
    /// mints a fresh alias slot per crossing, and charging per slot would multiply the backlog by
    /// K (a false OVER-MEMORY). D distinct cores all count. The held objects are slots of the
    /// heap, which the slot walk counts.
    #[test]
    fn live_bytes_counts_a_channel_once_per_core() {
        let mut h = Heap::new();
        let word = std::mem::size_of::<Value>();
        let cc = Arc::new(ChannelCore::default());
        let c1 = h.alloc(Obj::Channel(Arc::clone(&cc)));
        let empty = h.live_bytes();
        for i in 0..1000 {
            cc.q.lock().unwrap().push(Value::int(i));
        }
        let full = h.live_bytes();
        assert!(
            full >= empty + 1000 * word,
            "the backlog registers: {empty} -> {full}"
        );
        for _ in 0..9 {
            h.alloc(Obj::Channel(Arc::clone(&cc)));
        }
        assert_eq!(
            h.live_bytes(),
            full + 9 * std::mem::size_of::<Obj>(),
            "10 handles to one channel must not multiply its backlog"
        );
        let base = h.live_bytes();
        for _ in 0..64 {
            let d = Arc::new(ChannelCore::default());
            d.q.lock().unwrap().push(Value::int(1));
            h.alloc(Obj::Channel(d));
        }
        assert!(
            h.live_bytes() >= base + 64 * (word + std::mem::size_of::<Obj>()),
            "64 DISTINCT channels must each be charged"
        );
        let _ = c1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TICKET-246 — a reader that holds only `&Heap`, on another thread, charges the pacing
    /// counter. The `thread::scope` borrow is what needs `Heap: Sync`.
    #[test]
    fn heap_is_sync_and_charges_through_a_shared_ref() {
        let heap = Heap::default();
        let shared: &Heap = &heap;
        shared.charge_bytes(40);
        std::thread::scope(|sc| {
            sc.spawn(|| shared.charge_bytes(2));
        });
        assert_eq!(shared.since_gc_bytes.load(Ordering::Relaxed), 42);
    }

    /// W8-22: a swept slot's stamped span must not leak to whatever `alloc` reuses the index for.
    #[test]
    fn heap_err_span_is_dropped_when_its_slot_is_swept() {
        let mut h = Heap::new();
        let r = h.alloc(Obj::Str("boom".into()));
        let sp = Span {
            line: 7,
            col: 3,
            file: 1,
        };
        h.set_err_span(r, sp);
        assert_eq!(h.err_span(r), Some(sp));

        // Unmarked, so `sweep` frees it.
        h.sweep();
        assert_eq!(h.err_span(r), None);

        // The freed index gets reused; the new occupant must not inherit the dead span.
        let r2 = h.alloc(Obj::Str("fresh".into()));
        assert_eq!(r2, r, "test assumes the freed slot is reused");
        assert_eq!(h.err_span(r2), None);
    }
}
