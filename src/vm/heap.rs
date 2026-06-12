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

/// A real hash table that *also* preserves insertion order. `entries` is the insertion-ordered
/// store (so iteration, `keys()`, set equality, and GC tracing stay deterministic); `index` maps a
/// key's cached hash to its candidate positions in `entries` for O(1)-average lookup. The cached
/// `u64` per entry makes index rebuild (after a remove) a pure, engine-free pass — no re-hashing of
/// user `hash()` methods. Probing always confirms a hash hit with the engine's `values_equal`
/// (structural), so a collision never returns the wrong key. The `index` holds plain `usize` — it
/// is **not** a GC child (only `entries`' keys/values are traced).
#[derive(Debug, Clone, Default)]
pub struct MapData {
    pub entries: Vec<(u64, Value, Value)>,
    /// `cached-hash → candidate positions`. FxHash-keyed (the `u64` is already a content hash; see
    /// [`super::fxhash`]). Plain `usize` values, **not** a GC child.
    pub index: FxHashMap<u64, Vec<usize>>,
}

impl MapData {
    /// Positions in `entries` whose key hashed to `h` (the probe candidates).
    pub fn candidates(&self, h: u64) -> &[usize] {
        self.index.get(&h).map_or(&[], |v| v.as_slice())
    }
    /// Append a fresh entry (caller has confirmed the key is absent), updating the index.
    pub fn push(&mut self, h: u64, k: Value, v: Value) {
        let pos = self.entries.len();
        self.entries.push((h, k, v));
        self.index.entry(h).or_default().push(pos);
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
            self.index.entry(*h).or_default().push(pos);
        }
    }
}

/// A hash *set* with the same insertion-order-preserving design as [`MapData`].
#[derive(Debug, Clone, Default)]
pub struct SetData {
    pub entries: Vec<(u64, Value)>,
    /// `cached-hash → candidate positions`, FxHash-keyed (see [`MapData::index`]).
    pub index: FxHashMap<u64, Vec<usize>>,
}

impl SetData {
    pub fn candidates(&self, h: u64) -> &[usize] {
        self.index.get(&h).map_or(&[], |v| v.as_slice())
    }
    pub fn push(&mut self, h: u64, e: Value) {
        let pos = self.entries.len();
        self.entries.push((h, e));
        self.index.entry(h).or_default().push(pos);
    }
    pub fn remove_at(&mut self, i: usize) -> (u64, Value) {
        let removed = self.entries.remove(i);
        self.rebuild_index();
        removed
    }
    fn rebuild_index(&mut self) {
        self.index.clear();
        for (pos, (h, _)) in self.entries.iter().enumerate() {
            self.index.entry(*h).or_default().push(pos);
        }
    }
}

/// A heap object — the reference half of the value space.
#[derive(Debug, Clone)]
pub enum Obj {
    Str(ChzStr),
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
    /// Fields in declaration order (deterministic `Display` / iteration). `tid` is the struct type's
    /// dense layout id (`StructDef::tid`), stamped at construction so the field IC can guard on a
    /// pure-int compare; `TID_NONE` for a struct whose name isn't a registered type (never IC-cached).
    Struct {
        name: Box<str>,
        tid: u32,
        fields: Vec<(Box<str>, Value)>,
    },
    Enum {
        ty: Box<str>,
        variant: Box<str>,
        payload: Vec<Value>,
    },
    /// A named function (top-level `fn` / method) + the module globals it resolves against.
    Func {
        proto: ProtoId,
        home: GcRef,
    },
    /// An anonymous function + its snapshot-captured environment (name → value) + home globals.
    Closure {
        proto: ProtoId,
        captured: HashMap<String, Value>,
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
            Obj::List(items) => items.iter().for_each(&mut push),
            Obj::Tuple(items) => items.iter().for_each(&mut push),
            Obj::Map(m) => m.entries.iter().for_each(|(_, k, v)| {
                push(k);
                push(v);
            }),
            Obj::Set(s) => s.entries.iter().for_each(|(_, e)| push(e)),
            Obj::Struct { fields, .. } => fields.iter().for_each(|(_, v)| push(v)),
            Obj::Enum { payload, .. } => payload.iter().for_each(&mut push),
            Obj::Func { home, .. } => out.push(*home),
            Obj::Closure { captured, home, .. } => {
                captured.values().for_each(&mut push);
                out.push(*home);
            }
            Obj::Module { slots, .. } => slots.iter().for_each(&mut push),
            Obj::Native { .. } => {}
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
