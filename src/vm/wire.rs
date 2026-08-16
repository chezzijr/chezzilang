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
use super::op::ProtoId;
use super::value::GcRef;
use crate::ast::Span;
use std::sync::Arc;

/// A `Send`-able serialization of a sendable [`Value`](super::value::Value).
///
/// Data arms (`List`/`Tuple`/`Map`/`Set`/`Struct`/`Enum`/`NewType`/`Iter`) own their contents
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
        entries: Vec<(u64, WireValue, WireValue)>,
    },
    /// `(cached hash, element)` pairs in insertion order. `id` as [`List`](WireValue::List).
    Set {
        id: u32,
        entries: Vec<(u64, WireValue)>,
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
    /// A `newtype` crossing the airlock by value (deep copy) like a 1-field struct: its runtime
    /// `type_key` (meaningful in any worker, shared `Arc<Program>`) plus its wired inner value. `id` as
    /// [`List`](WireValue::List).
    NewType {
        id: u32,
        type_key: Box<str>,
        inner: Box<WireValue>,
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
    /// container arm — `List`/`Tuple`/`Map`/`Set`/`Struct`/`Enum`/`NewType`/`Iter`) currently on the
    /// serialize DFS stack — the encoding that lets ANY value cycle cross the airlock: a recursive local
    /// `fn`'s letrec self-cell, a mutually-recursive closure pair, a self-referential struct/list/map,
    /// or a mixed struct+closure cycle. Serialize assigns each such node an `id` on first visit and, on a
    /// REVISIT of a node still on the stack (a true back-edge), emits `Backref(id)` and stops the descent;
    /// a node revisited OFF the stack (an acyclic DAG alias) is re-serialized as an independent deep copy
    /// — preserving the deep-copy-independence contract for closures and data alike.
    ///
    /// **W7-4 — [`Cell`](WireValue::Cell) is the ONE exception**: a cell is a BINDING's identity, not a
    /// value, so its id is memoized for the whole serialization scope and an OFF-stack revisit ALSO
    /// emits `Backref(id)` (two sibling closures over one captured local must land on one cell — the
    /// language's own "visible across sibling closures" rule). Cross-heap STORES additionally re-emit a
    /// cell's full definition once per depth-1 subtree (`WireMemo::elem_split`) so `RwShared`'s
    /// piecewise read views never see a `Backref` into a sibling piece; a repeated definition dedupes
    /// on rebuild, so a whole-value rebuild still ties every reference to one cell.
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
    /// — `has_handle` leaves it `false` via the `_` arm.
    Socket(Arc<SocketCore>),
    /// A `Listener` handle crossing the airlock as its shared [`ListenerCore`] (D6). See [`Socket`].
    Listener(Arc<ListenerCore>),
    /// R2 — a `Writer` handle crossing the airlock as its shared [`WriterCore`] — an `Arc`'d file/stream
    /// handle, so a spawned fiber reaches the same output. Cross-safe (an `Arc`, not a `GcRef`), like
    /// [`Socket`] — `has_handle` leaves it `false` via the `_` arm. Cross-task write ORDERING to one
    /// shared Writer is unspecified (Go's `bufio`-not-goroutine-safe rule), but each single write is one
    /// Mutex critical section.
    Writer(Arc<WriterCore>),
    /// R2b — a `Reader` handle crossing the airlock as its shared [`ReaderCore`] — an `Arc`'d file
    /// handle, so a spawned fiber reaches the same fd. Cross-safe (an `Arc`, not a `GcRef`), like
    /// [`Writer`]/[`Socket`] — `has_handle` leaves it `false` via the `_` arm. Cross-task read ORDERING
    /// against one shared Reader is unspecified (two tasks race the file offset), but each read is one
    /// Mutex critical section.
    Reader(Arc<ReaderCore>),
    /// An opaque C-ABI `ptr` handle crossing the airlock **by value** (the raw `usize` address). A C
    /// `void*` is heap-independent — the same address is meaningful in any worker's heap — so
    /// `from_wire` allocates a fresh `Obj::Ptr` wrapping the identical address. Holds no `GcRef`, so it
    /// is cross-safe (`has_handle` leaves it `false` via the `_` arm) and works on the serial engine
    /// and the M:N snapshot fast path alike.
    Ptr(usize),
    /// A first-class UNIVERSE builtin fn (`print`/`ord`/`chr`/`panic`) crossing the airlock **by
    /// value** — it is pure code identified only by name (no `GcRef`, no captured heap state), so
    /// `from_wire` allocates a fresh `Obj::Builtin` with the same name on the other side, meaningful
    /// in any worker. Unlike a `Func`/`Closure` (a `Handle` into this heap), it genuinely crosses an
    /// OS-thread boundary — cross-safe (`has_handle` leaves it `false` via the `_` arm), so a builtin
    /// captured into a spawned task works on the serial AND the M:N engine alike.
    Builtin(Box<str>),
    /// A native fn (`Obj::Native`, e.g. `math.sqrt`) carried across the airlock **by value** — it is
    /// pure code: a `fn` pointer (`Copy`, so heap-independent — the same code address is valid in any
    /// worker of this process) plus its name, no `GcRef` and no captured heap state. Like
    /// [`Builtin`](WireValue::Builtin), it genuinely crosses an OS-thread boundary; `from_wire` re-allocs
    /// a fresh `Obj::Native` with the same name/pointer/[`kind`](crate::native::Kind). Cross-safe
    /// (`has_handle` leaves it `false` via the `_` arm) — the same value the SNAPSHOT path already ships
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
    },
    /// B3.3 — a BARE function (`Obj::Func`) carried across the airlock **by value**: its `proto`
    /// (shared via `Arc<Program>`) + its `home` index (as [`Closure`](WireValue::Closure)), no captures.
    /// Kept DISTINCT from `Closure` on purpose — a bare fn and a closure are observationally different
    /// (`str(func)` renders `<fn NAME>`, `str(closure)` renders `<closure>`), so collapsing a func into
    /// an empty-capture `Closure` would render `<closure>` on the M:N snapshot rebuild path while the
    /// serial engine's live `Obj::Func` renders `<fn NAME>` — a parity divergence. Holds no `GcRef`
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
    Pending(Vec<WireValue>),
    /// Driven at least once, suspended at a `yield`: the single parked body frame plus its private
    /// operand stack (base-0), with the private `call_depth`/`cur_base`, and the live `recover:`
    /// handlers stacked over that frame (backlog arm b). A [`Handler`](super::Handler) is pure
    /// plain-data (all `usize`, `Copy`, no `GcRef`/`Value`), so it serializes as-is with no value
    /// recursion — the frame/stack it indexes are serialized coherently alongside, so the indices
    /// stay valid after `from_wire` rebuilds them on the receiver heap. The frame carries no
    /// `deferred` (a pending `defer` is a checker-unreachable HARD ARM still rejected in `to_wire`),
    /// so that is not serialized.
    Suspended {
        frame: WireCallFrame,
        stack: Vec<WireValue>,
        call_depth: usize,
        cur_base: usize,
        handlers: Vec<super::Handler>,
    },
    /// Body returned / fell off the end: no parked context at all.
    Done,
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

impl WireValue {
    /// B3.2 — does this value graph carry a by-reference [`Handle`](WireValue::Handle), i.e. a
    /// heap-local `GcRef`? Such a value cannot cross into another heap as-is — the slot index is
    /// meaningless there — so the worker airlock ([`Vm::run_task_isolated`](super::Vm)) rejects it
    /// with a clean fault. As of B3.3 `Str`, closures ([`Closure`](WireValue::Closure)), bare funcs
    /// ([`Func`](WireValue::Func)) and native/FFI fns ([`Native`](WireValue::Native)/[`Cffi`](WireValue::Cffi))
    /// all cross by value or as a shared `Arc` (none is a `Handle`), so the only flagged leaf is
    /// [`Handle`](WireValue::Handle) itself — which now wraps only `Module` (a module's mutable globals
    /// genuinely can't cross). `Func`/`Native` carry no `GcRef`, so they are always `false` (via the
    /// `_` arm); a `Cffi` is a shared `Arc`, also `false`; a `Closure` recurses through its captures
    /// (one could embed a residual `Module` handle). The shared-core arms
    /// (`Channel`/`Shared`/`Executor`) are cross-safe — they carry an `Arc`, not a `GcRef` — so they
    /// are *not* flagged.
    pub fn has_handle(&self) -> bool {
        match self {
            WireValue::Handle(_) => true,
            WireValue::List { items: xs, .. }
            | WireValue::Tuple { items: xs, .. }
            | WireValue::Enum { payload: xs, .. } => xs.iter().any(WireValue::has_handle),
            WireValue::Map { entries, .. } => entries
                .iter()
                .any(|(_, k, v)| k.has_handle() || v.has_handle()),
            WireValue::Set { entries, .. } => entries.iter().any(|(_, e)| e.has_handle()),
            WireValue::Struct { fields, .. } => fields.iter().any(|(_, v)| v.has_handle()),
            WireValue::NewType { inner, .. } => inner.has_handle(),
            // A cursor's snapshot items could themselves embed a `Handle` (e.g. a cursor over a list
            // of closures) — recurse so the snapshot fast-path stays honest.
            WireValue::Iter { items, .. } => items.iter().any(WireValue::has_handle),
            // B3.6: a closure crosses by value, but a *captured* value could itself embed a `Handle`
            // (e.g. a captured closure crossing as a nested `Closure` whose own captures aren't
            // cross-safe) — recurse so the invariant stays honest.
            WireValue::Closure { captured, .. } => captured.iter().any(|(_, v)| v.has_handle()),
            // A cell follows its inner value: a cell over pure data stays on the snapshot fast path;
            // a cell embedding a handle takes the slow path so its handle is deep-copied.
            WireValue::Cell { inner, .. } => inner.has_handle(),
            // A back-reference TERMINATES the walk: its target (an already-visited identity-preserved
            // node — a Cell/Closure or a container — on the serialize stack) is reachable elsewhere in
            // the graph, where its handle-status is already folded in. Treating it as `false` here is
            // load-bearing — it keeps `has_handle` from infinite-looping on the now-cyclic wire graph
            // (the `to_snap` fast-path `!has_handle()` hazard), and is correct because the back-edge
            // itself carries no `Handle`.
            WireValue::Backref(_) => false,
            // F3 path C: a generator crosses by value, but a parked slot (a `Pending` arg or a
            // `Suspended` operand-stack slot) or its backing closure could itself embed a `Handle` —
            // recurse so a `Module` handle parked in a slot is still flagged (the M:N worker airlock
            // rejects it) and the invariant stays honest. (A parked native/FFI fn is no longer flagged
            // — it crosses by value now — which is correct/desired.)
            WireValue::Generator { closure, state, .. } => {
                closure.as_ref().is_some_and(|c| c.has_handle())
                    || match state {
                        WireGenState::Pending(args) => args.iter().any(WireValue::has_handle),
                        WireGenState::Suspended { stack, .. } => {
                            stack.iter().any(WireValue::has_handle)
                        }
                        WireGenState::Done => false,
                    }
            }
            _ => false,
        }
    }

    /// W7-11 — this node's per-serialization identity, if it has one: every identity-preserved arm
    /// (the containers, `Cell`, `Closure`) plus [`Backref`](WireValue::Backref), whose id names the
    /// node it points at. `None` for a leaf (scalar/`Str`/`bytes`/handle/generator), which by
    /// construction cannot carry a `Backref` either.
    ///
    /// Used by [`from_wire_piece`](crate::vm::Vm::from_wire_piece) to find the just-rebuilt piece
    /// inside a whole-container rebuild: the ids are the same wire's, so the piece's own id is the
    /// key into that rebuild's map. **Not every arm has one** — a `Generator` carries no id (its
    /// parked frame can never be a `Backref` target), yet it CAN contain a `Backref` through its
    /// backing closure or a parked slot, so "no id" must never be read as "cannot dangle".
    pub fn node_id(&self) -> Option<u32> {
        match self {
            WireValue::List { id, .. }
            | WireValue::Tuple { id, .. }
            | WireValue::Map { id, .. }
            | WireValue::Set { id, .. }
            | WireValue::Iter { id, .. }
            | WireValue::Struct { id, .. }
            | WireValue::Enum { id, .. }
            | WireValue::NewType { id, .. }
            | WireValue::Cell { id, .. }
            | WireValue::Closure { id, .. }
            | WireValue::Backref(id) => Some(*id),
            _ => None,
        }
    }

    /// W7-11 — can this wire be rebuilt **on its own**, with only `known` already in the rebuild map?
    /// True iff every `Backref` it contains names an id that is either defined earlier within this
    /// wire or already in `known`.
    ///
    /// This is a **pre-check, not a post-mortem**, and that is the whole point: an attempt that
    /// discovers the miss halfway has already written partial nodes (including a `Cell` holding the
    /// inert placeholder) into the caller's map, and a caller that shares one map across several
    /// pieces — `RwShared.slice` — then serves the NEXT piece out of those poisoned entries via the
    /// `Cell` first-wins dedupe. So the miss must be known before a single node is allocated.
    ///
    /// **Mirrors [`Vm::from_wire_memo`]'s arms exactly**, and has to stay mirrored: a node registers
    /// its id BEFORE recursing children (so a self-cycle resolves), and the `Cell` arm short-circuits
    /// on an id already present WITHOUT descending (so a re-emitted cell whose first definition is
    /// already built is resolvable regardless of its contents).
    pub fn backrefs_resolvable(&self, known: &super::fxhash::FxHashMap<u32, GcRef>) -> bool {
        fn walk(
            w: &WireValue,
            defined: &mut super::fxhash::FxHashSet<u32>,
            known: &super::fxhash::FxHashMap<u32, GcRef>,
        ) -> bool {
            let all = |xs: &[WireValue], d: &mut super::fxhash::FxHashSet<u32>| {
                xs.iter().all(|x| walk(x, d, known))
            };
            match w {
                WireValue::Backref(id) => defined.contains(id) || known.contains_key(id),
                WireValue::List { id, items }
                | WireValue::Tuple { id, items }
                | WireValue::Iter { id, items, .. }
                | WireValue::Enum {
                    id, payload: items, ..
                } => {
                    defined.insert(*id);
                    all(items, defined)
                }
                WireValue::Map { id, entries } => {
                    defined.insert(*id);
                    entries
                        .iter()
                        .all(|(_, k, v)| walk(k, defined, known) && walk(v, defined, known))
                }
                WireValue::Set { id, entries } => {
                    defined.insert(*id);
                    entries.iter().all(|(_, e)| walk(e, defined, known))
                }
                WireValue::Struct { id, fields, .. } => {
                    defined.insert(*id);
                    fields.iter().all(|(_, v)| walk(v, defined, known))
                }
                WireValue::NewType { id, inner, .. } => {
                    defined.insert(*id);
                    walk(inner, defined, known)
                }
                // The rebuild short-circuits on a known id without descending — so must this.
                WireValue::Cell { id, inner } => {
                    if known.contains_key(id) || !defined.insert(*id) {
                        return true;
                    }
                    walk(inner, defined, known)
                }
                WireValue::Closure { id, captured, .. } => {
                    defined.insert(*id);
                    captured.iter().all(|(_, v)| walk(v, defined, known))
                }
                // No id of its own, but its backing closure and parked slots can carry a `Backref`.
                WireValue::Generator { closure, state, .. } => {
                    closure.as_ref().is_none_or(|c| walk(c, defined, known))
                        && match state {
                            WireGenState::Pending(args) => all(args, defined),
                            WireGenState::Suspended { stack, .. } => all(stack, defined),
                            WireGenState::Done => true,
                        }
                }
                _ => true, // leaves: scalars, Str/bytes, Func/Native/Cffi/Builtin, shared-core arms
            }
        }
        walk(self, &mut super::fxhash::FxHashSet::default(), known)
    }
}
