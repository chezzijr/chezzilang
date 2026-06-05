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
}

/// One heap slot: the object (or a hole, for swept/free slots) + its GC mark bit.
#[derive(Debug)]
struct Slot {
    obj: Option<Obj>,
    mark: bool,
}

/// The object heap: a slab of slots with a free-list for reuse.
#[derive(Debug, Default)]
pub struct Heap {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

impl Heap {
    pub fn new() -> Self {
        Heap::default()
    }

    /// Allocate an object, returning its handle. Reuses a free slot when available.
    pub fn alloc(&mut self, obj: Obj) -> GcRef {
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
}
