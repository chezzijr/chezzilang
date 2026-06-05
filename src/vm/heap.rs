//! The GC-managed heap (M5).
//!
//! M5a: `alloc` only inserts (the slot/free-list machinery is in place; the mark-sweep collector
//! lands in M5b). Objects are addressed by [`GcRef`] (a slot index), so handle copies alias one
//! object. The VM owns the heap and mutates objects through `&mut heap[h]` — no `RefCell` needed.

use super::op::ProtoId;
use super::value::{GcRef, Value};
use std::collections::HashMap;

/// A heap object — the reference half of the value space.
#[derive(Debug, Clone)]
pub enum Obj {
    Str(Box<str>),
    List(Vec<Value>),
    /// Fields in declaration order (deterministic `Display` / iteration).
    Struct {
        name: Box<str>,
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
    /// A module namespace: its name + top-level bindings (this *is* the module's globals table).
    Module {
        name: Box<str>,
        globals: HashMap<String, Value>,
    },
    /// A native (Rust) function — a member of a native std module (`std.math` etc., M6c). Holds no
    /// heap references, so it has no GC children.
    Native {
        name: Box<str>,
        func: crate::native::NativeFn,
    },
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
            Obj::Struct { fields, .. } => fields.iter().for_each(|(_, v)| push(v)),
            Obj::Enum { payload, .. } => payload.iter().for_each(&mut push),
            Obj::Func { home, .. } => out.push(*home),
            Obj::Closure { captured, home, .. } => {
                captured.values().for_each(&mut push);
                out.push(*home);
            }
            Obj::Module { globals, .. } => globals.values().for_each(&mut push),
            Obj::Native { .. } => {}
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
