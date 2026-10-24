//! B3.0 — the wire-format airlock value (`WireValue`).
//!
//! A `WireValue` is a serialized, owned mirror of the **sendable** value set (`concurrency.md` §7).
//! It is the form a value takes while crossing a task airlock: `Vm::to_wire` serializes a heap
//! `Value` into it (read-only, no allocation), and `Vm::from_wire` reconstructs a `Value` from it
//! into the destination heap. In B3.0 the destination is the *same* heap and behavior is
//! byte-identical to the old `deep_clone`; under B3.1+ the cores move to `Arc<…Core>` and under
//! B3.3 a `WireValue` is what actually crosses an OS-thread boundary (hence the owned, `GcRef`-free
//! data arms — the only `GcRef` is the by-reference `Handle` arm, which B3.1 replaces with the
//! shared `Arc` core).

use super::core::{
    AtomicCore, AtomicIntCore, ChannelCore, ExecutorCore, ListenerCore, ReaderCore, RwSharedCore,
    SharedCore, SocketCore, WriterCore,
};
use super::map_data::HashIndex;
use super::value::GcRef;
use crate::ast::Span;
use crate::base::op::ProtoId;
use std::sync::Arc;

/// An entry that carries its key's cached hash (the `u64` the heap map computed at insert).
pub trait CachedHash {
    fn cached_hash(&self) -> u64;
}

impl CachedHash for (u64, WireValue, WireValue) {
    #[inline]
    fn cached_hash(&self) -> u64 {
        self.0
    }
}

impl CachedHash for (u64, WireValue) {
    #[inline]
    fn cached_hash(&self) -> u64 {
        self.0
    }
}

/// TICKET-192 — the entries of a wire `Map`/`Set` plus the same plain [`HashIndex`] the heap
/// `MapData`/`SetData` wrap (`vm/map_data.rs`), so a stored `RwShared` map answers
/// `get_key`/`has` in O(1) expected instead of scanning. Insertion order stays the iteration order.
///
/// Read through `Deref` to a slice; there is deliberately no `DerefMut`. Every mutation goes
/// through a method that keeps `index` in step with `entries`.
///
/// TICKET-246: a box no longer stores a wire value, so the table has no mutation method at all:
/// it is built once by the serializer and consumed by the rebuild.
#[derive(Debug, Clone)]
pub struct WireTable<E> {
    entries: Vec<E>,
    index: HashIndex,
}

pub type WireMap = WireTable<(u64, WireValue, WireValue)>;
pub type WireSet = WireTable<(u64, WireValue)>;

impl<E: CachedHash> WireTable<E> {
    pub fn new(entries: Vec<E>) -> Self {
        let index = HashIndex::from_hashes(entries.iter().map(CachedHash::cached_hash));
        WireTable { entries, index }
    }
    /// Positions whose key hashed to `h` (the probe candidates).
    #[inline]
    pub fn candidates(&self, h: u64) -> &[usize] {
        self.index.candidates(h)
    }
    pub fn into_entries(self) -> Vec<E> {
        self.entries
    }
}

impl<E> std::ops::Deref for WireTable<E> {
    type Target = [E];
    #[inline]
    fn deref(&self) -> &[E] {
        &self.entries
    }
}

/// A `Send`-able serialization of a sendable [`Value`](super::value::Value).
///
/// Data arms (`List`/`Tuple`/`Map`/`Set`/`Struct`/`Enum`/`Iter`) own their contents
/// recursively; `Str` crosses **by value** (owned bytes — [`WireValue::Str`]). Callables cross **by
/// value** too — a closure as [`WireValue::Closure`] (proto + wired captures + home index), a bare fn as
/// [`WireValue::Func`], a native fn as [`WireValue::Native`] (name + fn ptr), an FFI fn as
/// [`WireValue::Cffi`] (shared `Arc<Cffi>`). Only a `Module` (mutable globals) stays by-reference as
/// [`WireValue::Handle`]. The
/// `Channel`/`Shared`/`Executor` handles cross as their shared `Arc<…Core>` (B3.1). `Map`/`Set` carry
/// their cached `u64` hashes so reconstruction never re-hashes (byte-identical iteration order +
/// index — see [`super::heap::MapData`]).
///
/// Every container/callable arm carries a per-serialization `id` (see [`WireValue::Backref`]): assigned
/// on first visit and registered on the serialize DFS stack BEFORE recursing children, so a self-
/// referential value (`a.next = b; b.next = a`, a list holding itself, a mixed struct+closure cycle)
/// round-trips instead of overflowing the depth cap — a back-edge to a node still on the stack emits
/// `Backref(id)` and `from_wire` ties the knot. (The depth cap stays as the backstop for genuinely-
/// unbounded ACYCLIC nesting.)
#[derive(Debug, Clone, Default)]
pub enum WireValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    /// The default wire value (an empty `Shared` box starts here before its first `set`).
    #[default]
    Nil,
    /// A `list` crossing by value (deep copy). The `id` is a per-serialization identity (see
    /// [`WireValue::Backref`]) so a self-referential list (`xs.push(xs)`) or any cycle passing through
    /// it round-trips: the id is registered on the DFS stack BEFORE recursing `items`, so a nested
    /// back-edge resolves to the already-alloc'd placeholder list.
    List {
        id: u32,
        items: Vec<WireValue>,
    },
    Tuple {
        id: u32,
        items: Vec<WireValue>,
    },
    /// `(cached hash, key, value)` triples in insertion order. `id` as [`List`](WireValue::List) — a
    /// self-referential map round-trips; reconstruction reuses the carried hash (never re-hashes a
    /// cyclic key).
    Map {
        id: u32,
        entries: WireMap,
    },
    /// `(cached hash, element)` pairs in insertion order. `id` as [`List`](WireValue::List).
    Set {
        id: u32,
        entries: WireSet,
    },
    /// B3.3a — a `str` carried across the airlock **by value** (owned bytes). `str` is immutable and
    /// value-compared (Chezzi has no identity operator), so `from_wire` allocating a fresh heap `str`
    /// is observationally identical to sharing the original handle — and unlike a `Handle(GcRef)`,
    /// owned bytes are meaningful across an OS-thread heap boundary (B3.3).
    Str(Box<str>),
    /// `bytes` carried across the airlock **by value** (owned raw bytes). Like [`Str`](WireValue::Str):
    /// immutable + value-compared, holds no `GcRef`, so a fresh heap `bytes` on `from_wire` is
    /// observationally identical to the original — cross-safe (`has_handle` leaves it `false`).
    Bytes(Box<[u8]>),
    /// `bytearray` carried across the airlock **by value** as a DEEP COPY (owned raw bytes), like
    /// [`List`](WireValue::List): `from_wire` rebuilds a FRESH independent `bytearray` on the other
    /// side — never a shared mutable view (a shared mutable buffer across OS threads is a data race).
    /// Holds no `GcRef`, so it is cross-safe (`has_handle` leaves it `false`).
    ByteArray(Box<[u8]>),
    /// An `Iterable.iter()` cursor carried across the airlock **by value** as a DEEP COPY, like
    /// [`List`](WireValue::List): `from_wire` rebuilds a FRESH independent cursor (its own snapshot
    /// `items` + `pos`) on the other side. A cursor is just a snapshot `Vec` + index — plain data,
    /// unlike a generator (which holds parked frames into this heap and stays non-sendable). Its
    /// items are recursively wired, so a cursor over a non-sendable element (e.g. a `Channel`) faults
    /// recoverably exactly as a `list` of that element would.
    Iter {
        id: u32,
        items: Vec<WireValue>,
        pos: usize,
    },
    Struct {
        id: u32,
        name: Box<str>,
        fields: Vec<(Box<str>, WireValue)>,
    },
    Enum {
        /// M19 lever #2 — the dense `variant_id` crosses the airlock DIRECTLY (not the name). All
        /// workers share one `Arc<Program>`, so the id is identical on both sides and needs no
        /// re-resolution. Carrying the id (not the name) is also correct under variant-name shadowing:
        /// a user enum declaring `Some`/`Ok`/… reuses the name but has its own id, so a name round-trip
        /// (`enum_names` → `variant_id`) would collapse a native `Some` (`VID_SOME`) into the user's id.
        id: u32,
        variant_id: u32,
        payload: Vec<WireValue>,
    },
    /// An `Obj::Cell` (a by-reference-captured local's heap box) crossing the airlock by value as a
    /// DEEP COPY: `from_wire` rebuilds a FRESH independent cell wrapping the wired inner value, so a
    /// plain captured local sent into a `spawn` task is an isolated per-task copy — never a shared
    /// mutable box across an OS-thread boundary (the memory-safety line, design §4 F1). `has_handle`
    /// follows the inner value, so a cell over pure data rides the cheap snapshot fast path.
    ///
    /// The `id` is a per-serialization identity assigned on first visit (see [`WireValue::Backref`]),
    /// so a `Cell`/`Closure` cycle — the letrec self-cell a recursive local `fn` produces — round-trips
    /// instead of overflowing the depth cap. On reconstruction the `id` is registered BEFORE recursing
    /// `inner`, so a `Backref(id)` nested inside resolves to the already-alloc'd placeholder cell.
    Cell {
        id: u32,
        inner: Box<WireValue>,
    },
    /// A BACK-REFERENCE to an already-serialized identity-preserved node (`Cell`/`Closure` OR any
    /// container arm — `List`/`Tuple`/`Map`/`Set`/`Struct`/`Enum`/`Iter`) — the encoding that
    /// lets ANY value cycle cross the airlock: a recursive local `fn`'s letrec self-cell, a
    /// mutually-recursive closure pair, a self-referential struct/list/map, or a mixed struct+closure
    /// cycle. Serialize assigns each such node an `id` on first visit and, on a REVISIT of a node still
    /// on the DFS stack (a true back-edge), emits `Backref(id)` and stops the descent.
    ///
    /// TICKET-100: a node revisited OFF the stack is ALSO `Backref(id)` now, for containers and closures
    /// as well as cells — one source object produces one copy per crossing, matching same-task
    /// reference semantics (`b := a` means the same thing inside a task as outside) and CPython's
    /// `copy.deepcopy` (memoizes by source identity).
    ///
    /// TICKET-154: the `RwShared` stores follow this rule too, so a stored wire may carry a `Backref`
    /// into a SIBLING depth-1 piece. The piecewise read views resolve it on the REBUILD side
    /// (`Vm::box_copy_pieces` for the looping views, `Vm::from_wire_pieces`'s whole-root
    /// fallback for a single piece), never by re-emitting definitions in the store.
    ///
    /// `from_wire` resolves a `Backref` to the placeholder registered under that `id`, tying the knot.
    /// Holds no `GcRef` and terminates the walk, so `has_handle` leaves it `false`.
    Backref(u32),
    /// A by-reference object carried across the airlock as its existing heap handle (single-thread /
    /// same heap). As of B3.3 this wraps only the object that genuinely CANNOT cross an OS-thread heap
    /// boundary: a `Module` (mutable globals — a `GcRef` meaningless on another heap). Closures/funcs
    /// now cross by value ([`Closure`](WireValue::Closure)/[`Func`](WireValue::Func)), native/FFI fns
    /// cross by value/`Arc` ([`Native`](WireValue::Native)/[`Cffi`](WireValue::Cffi)), and
    /// `Str`/`bytes`/… have always crossed by value. `has_handle` flags this arm, so the worker
    /// airlock rejects a `Module` handle that would try to cross into another heap. (Source-unreachable:
    /// `module` is not a nameable type, so this is a defensive-only guard.)
    Handle(GcRef),
    /// A `Channel` handle crossing the airlock as its shared [`ChannelCore`] (B3.1). `from_wire`
    /// allocates a *fresh* heap handle wrapping this same `Arc`, so two tasks reach one mailbox — the
    /// identity the language observes lives in the `Arc`, not the `GcRef`.
    Channel(Arc<ChannelCore>),
    /// A `Shared` handle crossing the airlock as its shared [`SharedCore`] (B3.1). See [`Channel`].
    Shared(Arc<SharedCore>),
    /// A `RwShared` handle crossing the airlock as its shared [`RwSharedCore`]. See [`Channel`]/[`Shared`].
    RwShared(Arc<RwSharedCore>),
    /// An `Atomic` handle crossing the airlock as its shared [`AtomicCore`]. See [`Channel`]/[`Shared`].
    Atomic(Arc<AtomicCore>),
    /// An `AtomicInt` handle crossing the airlock as its shared [`AtomicIntCore`]. See [`Atomic`].
    AtomicInt(Arc<AtomicIntCore>),
    /// An `Executor` handle crossing the airlock as its shared [`ExecutorCore`] (B3.1). See [`Channel`].
    Executor(Arc<ExecutorCore>),
    /// A `Socket` handle crossing the airlock as its shared [`SocketCore`] (D6) — an `Arc`'d fd, so a
    /// spawned fiber reaches the same connection. Cross-safe (an `Arc`, not a `GcRef`), like [`Channel`]
    /// — `has_handle` leaves it `false`.
    Socket(Arc<SocketCore>),
    /// A `Listener` handle crossing the airlock as its shared [`ListenerCore`] (D6). See [`Socket`].
    Listener(Arc<ListenerCore>),
    /// R2 — a `Writer` handle crossing the airlock as its shared [`WriterCore`] — an `Arc`'d file/stream
    /// handle, so a spawned fiber reaches the same output. Cross-safe (an `Arc`, not a `GcRef`), like
    /// [`Socket`] — `has_handle` leaves it `false`. Cross-task write ORDERING to one
    /// shared Writer is unspecified (Go's `bufio`-not-goroutine-safe rule), but each single write is one
    /// Mutex critical section.
    Writer(Arc<WriterCore>),
    /// R2b — a `Reader` handle crossing the airlock as its shared [`ReaderCore`] — an `Arc`'d file
    /// handle, so a spawned fiber reaches the same fd. Cross-safe (an `Arc`, not a `GcRef`), like
    /// [`Writer`]/[`Socket`] — `has_handle` leaves it `false`. Cross-task read ORDERING
    /// against one shared Reader is unspecified (two tasks race the file offset), but each read is one
    /// Mutex critical section.
    Reader(Arc<ReaderCore>),
    /// An opaque C-ABI `ptr` handle crossing the airlock **by value** (the raw `usize` address). A C
    /// `void*` is heap-independent — the same address is meaningful in any worker's heap — so
    /// `from_wire` allocates a fresh `Obj::Ptr` wrapping the identical address. Holds no `GcRef`, so it
    /// is cross-safe (`has_handle` leaves it `false`) and works on the M:N snapshot
    /// fast path too.
    Ptr(usize),
    /// A first-class UNIVERSE builtin fn (`print`/`ord`/`chr`/`panic`) crossing the airlock **by
    /// value** — it is pure code identified only by its id (no `GcRef`, no captured heap state), so
    /// `from_wire` allocates a fresh `Obj::Builtin` with the same id on the other side, meaningful
    /// in any worker. Unlike a `Func`/`Closure` (a `Handle` into this heap), it genuinely crosses an
    /// OS-thread boundary — cross-safe (`has_handle` leaves it `false`), so a builtin
    /// captured into a spawned task works correctly on the M:N engine.
    Builtin(crate::base::builtin::Builtin),
    /// A native fn (`Obj::Native`, e.g. `math.sqrt`) carried across the airlock **by value** — it is
    /// pure code: a `fn` pointer (`Copy`, so heap-independent — the same code address is valid in any
    /// worker of this process) plus its name, no `GcRef` and no captured heap state. Like
    /// [`Builtin`](WireValue::Builtin), it genuinely crosses an OS-thread boundary; `from_wire` re-allocs
    /// a fresh `Obj::Native` with the same name/pointer/[`kind`](crate::native::Kind). Cross-safe
    /// (`has_handle` leaves it `false`) — the same value the SNAPSHOT path already ships
    /// across M:N workers ([`SnapValue::Native`](super::sched)).
    Native {
        name: Box<str>,
        func: crate::native::NativeFn,
        kind: crate::native::Kind,
    },
    /// An FFI fn (`Obj::Cffi`, `extern "lib":`) carried across the airlock as its shared
    /// `Arc<Cffi>` — the `dlopen`'d library + resolved symbol behind an `Arc`, so a worker sharing this
    /// process's address space reaches the same code with no re-`dlopen`. Like [`Native`](WireValue::Native)
    /// it holds no `GcRef`, so it is cross-safe (`has_handle` leaves it `false`) — the same `Arc` the
    /// SNAPSHOT path already shares across M:N workers ([`SnapValue::Cffi`](super::sched)).
    Cffi(std::sync::Arc<crate::native::cffi::Cffi>),
    /// B3.6 / B3.3 — a closure carried across the airlock **by value**: its `proto` (which lives in the
    /// shared `Arc<Program>`, so it is meaningful in any worker), its captures wired recursively, and
    /// its `home` as an index into the parent's `module_objs` (resolved via `Vm::home_index` at wire
    /// time, `None` for a home not in the table) — never a heap-local `GcRef`. As of B3.3 the GENERIC
    /// `to_wire` also produces this arm (not just `Executor.submit`), so a closure crosses as data the
    /// same way on every airlock (spawn args/callees, Channel/Shared, module snapshot).
    /// `from_wire` rebuilds the closure over the worker's reconstructed home module.
    Closure {
        /// Per-serialization identity (see [`WireValue::Backref`]) — a self-capturing recursive closure
        /// back-references this `id` from a nested `Cell`, so the cycle round-trips.
        id: u32,
        proto: ProtoId,
        captured: Vec<(Box<str>, WireValue)>,
        home: Option<usize>,
        /// D4 layer C (TICKET-291): bit `k` set = capture `k` is the only live copy of its binding
        /// (`WireMemo::sole_captures`), so its rebuild keeps the enclosing mark; every other
        /// capture is marked (`crossing::Route::ClosureCaptures`). A capture at slot 64 or above
        /// is never sole (a false fault, never a lost write).
        sole: u64,
    },
    /// B3.3 — a BARE function (`Obj::Func`) carried across the airlock **by value**: its `proto`
    /// (shared via `Arc<Program>`) + its `home` index (as [`Closure`](WireValue::Closure)), no captures.
    /// Kept DISTINCT from `Closure` on purpose — a bare fn and a closure are observationally different
    /// (`str(func)` renders `<fn NAME>`, `str(closure)` renders `<closure>`), so collapsing a func into
    /// an empty-capture `Closure` would wrongly render `<closure>` on the M:N snapshot rebuild path,
    /// where an uncrossed `Obj::Func` renders `<fn NAME>`. Holds no `GcRef`
    /// (`has_handle` leaves it `false`), so a bare fn crosses an OS-thread boundary cleanly.
    Func {
        proto: ProtoId,
        home: Option<usize>,
    },
    /// A frame-holding generator carried across the airlock **by value** as a DEEP COPY — whether held
    /// in a frame LOCAL (F3 path C) or a MODULE GLOBAL (backlog item B, via `to_snap`). Unlike a cursor
    /// (plain snapshot data), a generator is frozen VM execution state; this arm serializes exactly that:
    /// its `proto` (shared via `Arc<Program>`), its `home` index (like
    /// [`Closure`](WireValue::Closure)), its backing closure (if any, wired recursively), and its
    /// lifecycle `state`. Every parked `Value` (a `Pending` call arg or a `Suspended` operand-stack
    /// slot) is wired recursively, so a non-sendable parked slot rejects AT SERIALIZE TIME exactly as a
    /// list of that value would. `from_wire`
    /// rebuilds a FRESH independent `GeneratorCore` on the receiving heap (deep-copy independence, like
    /// `Cell`/`Iter`). A generator suspended mid-`recover:` also crosses — its live handler stack is
    /// pure plain-data (carried on `WireGenState::Suspended`). A generator with a pending `defer` or
    /// more than one parked frame is a HARD ARM (`to_wire` rejects cleanly) — but both are
    /// checker-unreachable, so the reject is only a defensive guard against the type-blind compiler.
    Generator {
        /// TICKET-111 — adoption id only: a second reach inside one crossing is still rejected
        /// (DEC-100), so no Backref ever targets it.
        id: u32,
        proto: ProtoId,
        home: Option<usize>,
        closure: Option<Box<WireValue>>,
        state: WireGenState,
    },
}

/// F3 path C — the serialized lifecycle of a [`WireValue::Generator`]. Mirrors the runtime `GenState`
/// but with parked `Value`s replaced by owned `WireValue`s (and the parked `CallFrame` reduced to its
/// plain-data [`WireCallFrame`]).
#[derive(Debug, Clone)]
pub enum WireGenState {
    /// Created but not yet driven: the not-yet-consumed call args (each wired recursively).
    /// The `u64` is the generator's creation stamp (TICKET-190, TICKET-240, layout
    /// `crossing::stamp_mask`). It keeps `WireValue` its old size: the VM's debug-build
    /// `step` frame holds several `WireValue` locals on the per-op recursion path, and 8 bytes more
    /// each overflowed `self_referential_stringable_hits_depth_limit`.
    Pending(Vec<WireValue>, u64),
    /// Driven at least once, suspended at a `yield`: the single parked body frame plus its private
    /// operand stack (base-0), with the private `call_depth`/`cur_base`, and the live `recover:`
    /// handlers stacked over that frame (backlog arm b). A [`Handler`](super::Handler) is pure
    /// plain-data (all `usize`, `Copy`, no `GcRef`/`Value`), so it serializes as-is with no value
    /// recursion — the frame/stack it indexes are serialized coherently alongside, so the indices
    /// stay valid after `from_wire` rebuilds them on the receiver heap. The frame carries no
    /// `deferred` (a pending `defer` is a checker-unreachable HARD ARM still rejected in `to_wire`),
    /// so that is not serialized.
    Suspended {
        frame: Box<WireCallFrame>,
        stack: Vec<WireValue>,
        call_depth: usize,
        cur_base: usize,
        handlers: Vec<super::Handler>,
        /// The generator's creation stamp (layout `crossing::stamp_mask`).
        stamp: u64,
    },
    /// Body returned / fell off the end: no parked context at all.
    Done,
    /// TICKET-137 — a module-global generator the snapshot could not copy; rebuilt as
    /// `GenState::Unsendable`, which faults with this message when driven.
    Unsendable(Box<str>),
}

/// F3 path C — the plain-data (Send, `GcRef`-free) fields of a suspended generator's single parked
/// [`CallFrame`](super::CallFrame). The frame's `home`/`closure` are NOT carried here — they equal the
/// generator core's own `home`/`closure` (asserted at serialize time), so `from_wire` reuses the
/// rebuilt core's `GcRef`s. `deferred` is not carried (a pending defer is a rejected HARD ARM).
#[derive(Debug, Clone)]
pub struct WireCallFrame {
    pub proto: ProtoId,
    pub ip: usize,
    pub base: usize,
    pub counted: bool,
    pub is_toplevel: bool,
    pub defer_markers: Vec<usize>,
    pub nursery_len: usize,
    pub has_implicit_nursery: bool,
    pub call_span: Span,
    /// The frame's entry argument count (`CallFrame::argc`). Carried rather than recomputed: the
    /// rebuilt stack no longer distinguishes supplied arguments from nil-reserved locals, and a
    /// resumed frame must keep answering `Op::JumpIfProvided` the same way it did before it parked.
    pub argc: usize,
}

/// What one [`WireValue`] node is to a walk over its children, apart from the children themselves.
/// [`WireValue::visit`] returns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireNode {
    /// A heap-local `GcRef` ([`WireValue::Handle`]). It has no children.
    Handle(GcRef),
    /// A shared core whose payload sits behind the core's own lock: `Channel`, `Shared`, `RwShared`,
    /// `Atomic` or `Executor`. The `usize` is the `Arc` identity. It has no children: a walk queues
    /// the core and reads its payload later, with no other core lock held (`core::drain_pending_cores`).
    Core(usize),
    /// Any other node. `bytes` is what it owns beyond `size_of::<WireValue>()` and its children:
    /// string and byte payloads, struct, field and capture names, and one `u64` hash per map or
    /// set entry.
    Data { bytes: usize },
}

impl WireValue {
    /// The one owner of "which child `WireValue`s does this variant hold, and is it a handle or a
    /// nested core" (TICKET-267). Calls `f` on each direct child in order and returns the node's
    /// [`WireNode`]. [`has_handle`](Self::has_handle), `core::wire_summary`,
    /// `core::collect_gcrefs_structural` and `core::nested_core_bytes_structural` read this; a new
    /// variant gets its children here and nowhere else.
    ///
    /// A [`Backref`](WireValue::Backref) has no children: it ends the walk on a cyclic wire graph,
    /// and its target is reached elsewhere in the same graph.
    #[inline]
    pub fn visit(&self, mut f: impl FnMut(&WireValue)) -> WireNode {
        match self {
            WireValue::Handle(g) => WireNode::Handle(*g),
            WireValue::Channel(c) => WireNode::Core(Arc::as_ptr(c) as usize),
            WireValue::Shared(c) => WireNode::Core(Arc::as_ptr(c) as usize),
            WireValue::RwShared(c) => WireNode::Core(Arc::as_ptr(c) as usize),
            WireValue::Atomic(c) => WireNode::Core(Arc::as_ptr(c) as usize),
            WireValue::Executor(c) => WireNode::Core(Arc::as_ptr(c) as usize),
            WireValue::List { items, .. }
            | WireValue::Tuple { items, .. }
            | WireValue::Iter { items, .. }
            | WireValue::Enum { payload: items, .. } => {
                items.iter().for_each(f);
                WireNode::Data { bytes: 0 }
            }
            WireValue::Map { entries, .. } => {
                entries.iter().for_each(|(_, k, v)| {
                    f(k);
                    f(v);
                });
                WireNode::Data {
                    bytes: entries.len() * std::mem::size_of::<u64>(),
                }
            }
            WireValue::Set { entries, .. } => {
                entries.iter().for_each(|(_, e)| f(e));
                WireNode::Data {
                    bytes: entries.len() * std::mem::size_of::<u64>(),
                }
            }
            WireValue::Struct { name, fields, .. } => {
                let mut bytes = name.len();
                fields.iter().for_each(|(n, v)| {
                    bytes += n.len();
                    f(v);
                });
                WireNode::Data { bytes }
            }
            WireValue::Closure { captured, .. } => {
                let mut bytes = 0;
                captured.iter().for_each(|(n, v)| {
                    bytes += n.len();
                    f(v);
                });
                WireNode::Data { bytes }
            }
            WireValue::Cell { inner, .. } => {
                f(inner);
                WireNode::Data { bytes: 0 }
            }
            WireValue::Generator { closure, state, .. } => {
                if let Some(c) = closure {
                    f(c);
                }
                match state {
                    WireGenState::Pending(args, _) => args.iter().for_each(f),
                    WireGenState::Suspended { stack, .. } => stack.iter().for_each(f),
                    WireGenState::Done | WireGenState::Unsendable(_) => {}
                }
                WireNode::Data { bytes: 0 }
            }
            WireValue::Str(s) => WireNode::Data { bytes: s.len() },
            WireValue::Bytes(b) | WireValue::ByteArray(b) => WireNode::Data { bytes: b.len() },
            WireValue::Int(_)
            | WireValue::Float(_)
            | WireValue::Bool(_)
            | WireValue::Nil
            | WireValue::Backref(_)
            | WireValue::AtomicInt(_)
            | WireValue::Socket(_)
            | WireValue::Listener(_)
            | WireValue::Writer(_)
            | WireValue::Reader(_)
            | WireValue::Ptr(_)
            | WireValue::Builtin(_)
            | WireValue::Native { .. }
            | WireValue::Cffi(_)
            | WireValue::Func { .. } => WireNode::Data { bytes: 0 },
        }
    }

    /// B3.2 — does this value graph carry a by-reference [`Handle`](WireValue::Handle), i.e. a
    /// heap-local `GcRef`? Such a value cannot cross into another heap as-is — the slot index is
    /// meaningless there — so the worker airlock ([`Vm::run_task_isolated`](super::Vm)) rejects it
    /// with a clean fault. The only flagged leaf is [`Handle`](WireValue::Handle), which wraps only a
    /// `Module`. A nested core (`Channel`/`Shared`/`RwShared`/`Atomic`/`Executor`) is not flagged: it
    /// crosses as a shared `Arc`, not a `GcRef`. Every other node follows its children
    /// ([`visit`](Self::visit)); a `Backref` has none, which keeps this walk finite on a cyclic graph.
    pub fn has_handle(&self) -> bool {
        let mut found = false;
        match self.visit(|x| found = found || x.has_handle()) {
            WireNode::Handle(_) => true,
            WireNode::Core(_) => false,
            WireNode::Data { .. } => found,
        }
    }
}

/// TICKET-267 — the walks that read [`WireValue::visit`] agree on every variant and every child slot.
/// [`expect`] is an exhaustive `match`, so a new variant does not compile here until it has a class.
/// [`declared_variants`] reads the variant names from the enum's own source, so the coverage
/// assertion then fails until [`cells`] builds the variant or [`NO_CELL`] names it.
#[cfg(test)]
mod walk_grid {
    use super::*;
    use crate::vm::core::{
        AtomicCore, AtomicIntCore, ChannelCore, ExecutorCore, RwSharedCore, SeenCores, SharedCore,
        collect_core_gcrefs, nested_core_bytes, wire_summary,
    };
    use std::sync::Mutex;

    /// What every walk must answer for one cell.
    #[derive(Debug, PartialEq)]
    enum Expect {
        /// The child sits in one slot of this node: every walk reaches it.
        Slot,
        /// A nested core whose payload this heap's walks read; the payload holds the child.
        CoreWalked,
        /// A nested core whose payload this heap does not read: `Shared`/`RwShared` keep their value
        /// in the box heap (TICKET-246), and an `Executor` queue is always empty (decision D3).
        CoreNotWalked,
        Handle,
        Leaf,
    }

    fn expect(w: &WireValue) -> Expect {
        match w {
            WireValue::List { .. }
            | WireValue::Tuple { .. }
            | WireValue::Map { .. }
            | WireValue::Set { .. }
            | WireValue::Iter { .. }
            | WireValue::Struct { .. }
            | WireValue::Enum { .. }
            | WireValue::Cell { .. }
            | WireValue::Closure { .. }
            | WireValue::Generator { .. } => Expect::Slot,
            WireValue::Channel(_) | WireValue::Atomic(_) => Expect::CoreWalked,
            WireValue::Shared(_) | WireValue::RwShared(_) | WireValue::Executor(_) => {
                Expect::CoreNotWalked
            }
            WireValue::Handle(_) => Expect::Handle,
            WireValue::Int(_)
            | WireValue::Float(_)
            | WireValue::Bool(_)
            | WireValue::Nil
            | WireValue::Str(_)
            | WireValue::Bytes(_)
            | WireValue::ByteArray(_)
            | WireValue::Backref(_)
            | WireValue::AtomicInt(_)
            | WireValue::Socket(_)
            | WireValue::Listener(_)
            | WireValue::Writer(_)
            | WireValue::Reader(_)
            | WireValue::Ptr(_)
            | WireValue::Builtin(_)
            | WireValue::Native { .. }
            | WireValue::Cffi(_)
            | WireValue::Func { .. } => Expect::Leaf,
        }
    }

    /// The variants with no cell: each needs a live fd or a `dlopen`ed library, and none holds a child.
    const NO_CELL: [&str; 5] = ["Socket", "Listener", "Writer", "Reader", "Cffi"];

    /// The variant names of `pub enum WireValue`, read from this file: the enum is the one list.
    fn declared_variants() -> Vec<String> {
        let src = include_str!("wire.rs");
        let body = src
            .split_once("pub enum WireValue {\n")
            .expect("the WireValue enum")
            .1;
        let body = &body[..body.find("\n}\n").expect("the end of the WireValue enum")];
        body.lines()
            .filter_map(|l| l.strip_prefix("    "))
            .filter(|l| l.starts_with(|c: char| c.is_ascii_uppercase()))
            .map(|l| {
                l.split(|c: char| !c.is_ascii_alphanumeric())
                    .next()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    /// A cell's variant name, from the derived `Debug`.
    fn variant(w: &WireValue) -> String {
        format!("{w:?}")
            .split(|c: char| !c.is_ascii_alphanumeric())
            .next()
            .unwrap()
            .to_string()
    }

    fn nop(
        _: &mut dyn crate::native::Host,
    ) -> Result<crate::native::NativeRet, crate::native::HostError> {
        unreachable!("the grid never calls a native fn")
    }

    /// One cell per child SLOT of each variant: `child` sits in that one slot, and every other slot
    /// holds the leaf `Int(0)`. A queue core holds `child` as its one message.
    fn cells(child: &WireValue) -> Vec<WireValue> {
        let c = || child.clone();
        let leaf = || WireValue::Int(0);
        let chan: Arc<ChannelCore> = Arc::new(ChannelCore::default());
        chan.q.lock().unwrap().push(wire_summary(child), c());
        let generator =
            |closure: Option<Box<WireValue>>, state: WireGenState| WireValue::Generator {
                id: 1,
                proto: 0,
                home: None,
                closure,
                state,
            };
        let frame = WireCallFrame {
            proto: 0,
            ip: 0,
            base: 0,
            counted: false,
            is_toplevel: false,
            defer_markers: Vec::new(),
            nursery_len: 0,
            has_implicit_nursery: false,
            call_span: Span::default(),
            argc: 0,
        };
        vec![
            WireValue::Int(1),
            WireValue::Float(1.0),
            WireValue::Bool(true),
            WireValue::Nil,
            WireValue::List {
                id: 1,
                items: vec![c()],
            },
            WireValue::Tuple {
                id: 1,
                items: vec![c()],
            },
            WireValue::Map {
                id: 1,
                entries: WireMap::new(vec![(0, c(), leaf())]),
            },
            WireValue::Map {
                id: 1,
                entries: WireMap::new(vec![(0, leaf(), c())]),
            },
            WireValue::Set {
                id: 1,
                entries: WireSet::new(vec![(0, c())]),
            },
            WireValue::Str("s".into()),
            WireValue::Bytes(vec![1u8].into()),
            WireValue::ByteArray(vec![1u8].into()),
            WireValue::Iter {
                id: 1,
                items: vec![c()],
                pos: 0,
            },
            WireValue::Struct {
                id: 1,
                name: "S".into(),
                fields: vec![("f".into(), c())],
            },
            WireValue::Enum {
                id: 1,
                variant_id: 0,
                payload: vec![c()],
            },
            WireValue::Cell {
                id: 1,
                inner: Box::new(c()),
            },
            WireValue::Backref(1),
            WireValue::Handle(GcRef(7)),
            WireValue::Channel(chan),
            WireValue::Shared(Arc::new(SharedCore::default())),
            WireValue::RwShared(Arc::new(RwSharedCore::default())),
            WireValue::Atomic(Arc::new(AtomicCore {
                v: Mutex::new(c()),
                ..Default::default()
            })),
            WireValue::AtomicInt(Arc::new(AtomicIntCore::default())),
            WireValue::Executor(Arc::new(ExecutorCore::default())),
            WireValue::Ptr(0),
            WireValue::Builtin(crate::base::builtin::Builtin::Print),
            WireValue::Native {
                name: "nop".into(),
                func: nop,
                kind: crate::native::Kind::Inline,
            },
            WireValue::Closure {
                id: 1,
                proto: 0,
                captured: vec![("x".into(), c())],
                home: None,
                sole: 0,
            },
            WireValue::Func {
                proto: 0,
                home: None,
            },
            generator(Some(Box::new(c())), WireGenState::Done),
            generator(None, WireGenState::Pending(vec![c()], 0)),
            generator(
                None,
                WireGenState::Suspended {
                    frame: Box::new(frame),
                    stack: vec![c()],
                    call_depth: 0,
                    cur_base: 0,
                    handlers: Vec::new(),
                    stamp: 0,
                },
            ),
        ]
    }

    #[test]
    fn every_wire_variant_agrees_across_the_walks() {
        // The child: a handle (GcRef 7) and a nested Atomic core whose payload holds GcRef 9 and
        // 1000 bytes of string. `id: 3` tells it apart from every leaf.
        let inner = Arc::new(AtomicCore {
            v: Mutex::new(WireValue::List {
                id: 2,
                items: vec![
                    WireValue::Handle(GcRef(9)),
                    WireValue::Str("x".repeat(1000).into()),
                ],
            }),
            ..Default::default()
        });
        let child = WireValue::List {
            id: 3,
            items: vec![WireValue::Handle(GcRef(7)), WireValue::Atomic(inner)],
        };
        let is_child = |x: &WireValue| matches!(x, WireValue::List { id: 3, .. });
        let child_bytes = wire_summary(&child).0;
        let node = std::mem::size_of::<WireValue>();

        let all = cells(&child);
        let mut covered: Vec<String> = all
            .iter()
            .map(variant)
            .chain(NO_CELL.map(String::from))
            .collect();
        covered.sort();
        covered.dedup();
        let mut declared = declared_variants();
        declared.sort();
        assert_eq!(
            covered, declared,
            "each WireValue variant has a cell or a NO_CELL entry"
        );

        for w in &all {
            let mut roots = Vec::new();
            collect_core_gcrefs(w, &mut roots, &mut SeenCores::default());
            roots.sort_unstable_by_key(|g| g.0);
            let (bytes, dirty) = wire_summary(w);
            let nested = nested_core_bytes(w, &mut Default::default());
            let (mut children, mut reached) = (0, 0);
            w.visit(|x| {
                children += 1;
                reached += usize::from(is_child(x));
            });
            match expect(w) {
                Expect::Slot => {
                    assert_eq!(reached, 1, "{w:?}: visit reaches the child");
                    assert_eq!(
                        roots,
                        vec![GcRef(7), GcRef(9)],
                        "{w:?}: the GC walk roots the child's handles"
                    );
                    assert!(dirty, "{w:?}: wire_summary is dirty");
                    assert!(
                        bytes >= node + child_bytes,
                        "{w:?}: wire_summary counts the child ({bytes})"
                    );
                    assert!(
                        nested >= 1000,
                        "{w:?}: the nested-core byte walk counts the child's core ({nested})"
                    );
                    assert!(w.has_handle(), "{w:?}: has_handle follows the child");
                }
                Expect::CoreWalked => {
                    assert_eq!(children, 0, "{w:?}: a core has no direct child");
                    assert_eq!(
                        roots,
                        vec![GcRef(7), GcRef(9)],
                        "{w:?}: the GC walk roots the core's payload"
                    );
                    assert!(dirty, "{w:?}: a nested core is dirty");
                    assert!(
                        nested >= 1000,
                        "{w:?}: the byte walk counts the core's payload ({nested})"
                    );
                    assert!(!w.has_handle(), "{w:?}: a core crosses as an Arc");
                }
                Expect::CoreNotWalked => {
                    assert_eq!(children, 0, "{w:?}: a core has no direct child");
                    assert!(
                        roots.is_empty(),
                        "{w:?}: this heap roots nothing through it"
                    );
                    assert!(dirty, "{w:?}: a nested core is dirty");
                    assert!(!w.has_handle(), "{w:?}: a core crosses as an Arc");
                }
                Expect::Handle => {
                    assert_eq!(children, 0, "{w:?}: a handle has no child");
                    assert_eq!(roots, vec![GcRef(7)], "{w:?}");
                    assert!(dirty && w.has_handle() && nested == 0, "{w:?}");
                }
                Expect::Leaf => {
                    assert_eq!(children, 0, "{w:?}: a leaf has no child");
                    assert!(
                        roots.is_empty() && !dirty && nested == 0 && !w.has_handle(),
                        "{w:?}: a leaf roots nothing"
                    );
                }
            }
        }
    }
}
