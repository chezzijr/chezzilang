//! VM: the airlock inbound: `from_wire`, rebuild, unmark.
//! Split out of `src/vm/sched.rs`. `super::*` is the parent module.

use super::airlock_out::WireMemo;
use super::*;
use crate::base::crossing::{self, Fresh, Kids, Route};

impl Vm {
    /// B3.0 — reconstruct a [`WireValue`] into a heap [`Value`] (the airlock's inbound half). Data
    /// arms `alloc` fresh objects into *this* `Vm`'s heap (mirroring the old deep_clone allocation);
    /// [`WireValue::Handle`] returns the same handle (by-reference preserved). `Map`/`Set` rebuild
    /// via `push(hash, …)` with the carried hash, so iteration order + index are identical. Every
    /// identity-preserved arm (`Cell`/`Closure` AND every container) ties the knot (placeholder-alloc →
    /// register id → recurse → patch), so a self-referential value round-trips; `alloc` never collects,
    /// so no intermediate is lost mid-reconstruction.
    // `&mut self` is intentional: `from_wire` reconstructs *into* this VM's heap (it allocates),
    // so it is not the usual ownership-less `from_*` constructor the lint expects.
    #[allow(clippy::wrong_self_convention)]
    pub(super) fn from_wire(&mut self, w: WireValue) -> Value {
        self.from_wire_on(Route::Handoff, w)
    }

    /// [`from_wire`](Vm::from_wire) on `route`: the one place a whole-value rebuild sets the mark.
    #[allow(clippy::wrong_self_convention)]
    pub(super) fn from_wire_on(&mut self, route: Route, w: WireValue) -> Value {
        // Fresh rebuild memo per root: it maps each identity-preserved wire `id` (a `Cell`/`Closure` or
        // a container) to the heap `GcRef` of its placeholder, so a `WireValue::Backref(id)` nested
        // inside resolves to the already-alloc'd (and about-to-be-patched) node — tying a serialized
        // value cycle back together.
        let mut rebuild = super::fxhash::FxHashMap::<u32, GcRef>::default();
        // TICKET-189: the caller names the route and `crossing::marks` decides the mark. Every
        // `from_wire` caller is a hand-off read (Channel/Shared/RwShared/Atomic, an Executor job
        // root), which it never marks; `airlock_native` rebuilds under `Route::CopyRead` (TICKET-220).
        let saved_copy_mark = self.copy_mark;
        self.copy_mark = crossing::marks(route, saved_copy_mark);
        let v = self.from_wire_memo(w, &mut rebuild);
        self.copy_mark = saved_copy_mark;
        // W7-11 — every caller of `from_wire` rebuilds a WHOLE crossing, so its rebuild map spans the
        // same scope the serialize memo did and a `Backref` can never dangle. A piecewise drain goes
        // through [`from_wire_pieces`](Vm::from_wire_pieces) instead. Keep the invariant loud where it is
        // genuinely a bug; in release a dangling ref degrades to `nil` rather than aborting the host.
        debug_assert!(
            !self.wire_backref_missing,
            "from_wire: a whole-value rebuild hit a dangling Backref — memo scope != rebuild scope \
             (a piecewise drain must call from_wire_pieces)"
        );
        v
    }

    /// TICKET-220 — the engine half of `std.concurrency.task_copy_of` (`Kind::InterceptAirlock`):
    /// serialize the value and rebuild it under `Route::CopyRead`, so a task copy's read takes every
    /// rule the rebuild has (the generator frame mask, the module stop) with no second walker.
    pub(super) fn airlock_native(
        &mut self,
        name: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match name {
            "task_copy_of" => {
                let v = args.first().copied().unwrap_or_else(Value::nil);
                let w = self.to_wire_memo_at(v, span, &mut WireMemo::default())?;
                Ok(self.from_wire_on(Route::CopyRead, w))
            }
            _ => unreachable!("airlock_native on '{name}'"),
        }
    }

    /// Rebuild a wire item list into heap `Value`s **at exact capacity**. `pick` pulls the
    /// [`WireValue`] out of each element, so a keyed list (`Vec<(Box<str>, WireValue)>` — struct
    /// fields, closure captures) shares the one path with a plain `Vec<WireValue>`.
    ///
    /// NOT `items.into_iter().map(…).collect()`, which is the shape this replaced. Rust specializes
    /// that into an **in-place** collect when the destination element is no larger than the source's
    /// AND their alignments are equal (`align_of::<SRC>() != align_of::<DEST>()` is what makes std
    /// bail): the source `Vec`'s allocation is reused and the result inherits its capacity, scaled by
    /// the size ratio. Both conditions hold at every airlock rebuild — everything here maps into
    /// `Value` (8 B, align 8) from an align-8 source — so the inflation was **22×** from a plain
    /// `Vec<WireValue>` (176 B) and **24×** from a keyed `Vec<(Box<str>, WireValue)>` (192 B).
    ///
    /// Every container crossing the airlock (`spawn` args, `Channel.recv`, `Shared.get`, closure
    /// captures) therefore kept the whole wire buffer alive for the rebuilt object's entire lifetime:
    /// a 200 000-int `List` measured `capacity = 4 400 000` — a **35.2 MB** `Obj::List` holding
    /// 1.6 MB of data — and 50 such spawns peaked at 3.45 GB (203 MB after). It is invisible to
    /// `len()` and to every value-level test; only `Vec::capacity` shows it.
    ///
    /// **Use this at EVERY wire→`Value` list rebuild, not just the ones in `from_wire_memo`.** The
    /// first cut of this fix converted that function's eight container arms and left the identical
    /// shape in `deep_clone_all` and in `rebuild_ready`'s five `Lowered` arms — two of which
    /// (`deep_clone_all`'s result and the `Lowered::Closure` captures) land in a durable
    /// `Obj::Closure { captured }`, so `spawn` with a capturing closure still leaked 22–24×, under a
    /// doc comment claiming captures were fixed. Adversarial review caught it; the suite did not.
    ///
    /// Pre-sizing and pushing keeps the exact-capacity guarantee at the cost of one extra live
    /// buffer while the source drains, which the wire copy was already paying.
    pub(super) fn rebuild_items<T>(
        &mut self,
        items: Vec<T>,
        rebuild: &mut super::fxhash::FxHashMap<u32, GcRef>,
        pick: impl Fn(T) -> WireValue,
    ) -> Vec<Value> {
        let mut out = Vec::with_capacity(items.len());
        for it in items {
            let v = self.from_wire_memo(pick(it), rebuild);
            out.push(v);
        }
        out
    }

    /// TICKET-240 Rule W: unmark exactly the objects `shape` says the crossing site built, in the
    /// just-rebuilt value `v`. A positional shape applies only when the object has that many
    /// children; on any mismatch nothing below is unmarked (a false fault, never a missed mark).
    /// A `Node` never enters a map key or a set item: only [`Vm::unmark_graph`] does.
    pub(super) fn unmark_fresh(&mut self, v: Value, shape: &Fresh) {
        let kids = match shape {
            Fresh::Marked => return,
            Fresh::All => return self.unmark_graph(v),
            Fresh::Node(kids) => kids,
        };
        let Some(h) = v.as_obj() else {
            return;
        };
        self.heap.unset_copied(h);
        let (len, positional) = match self.heap.get(h) {
            Obj::List(xs) | Obj::Tuple(xs) => (xs.len(), true),
            Obj::Enum { payload, .. } => (payload.len(), true),
            Obj::Struct { fields, .. } => (fields.as_slice().len(), true),
            Obj::Map(m) => (m.entries.len(), false),
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::Set(_)
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
            | Obj::Generator(_) => return,
        };
        for i in 0..len {
            let kid = match kids {
                Kids::Marked => return,
                Kids::Each(s) => &**s,
                Kids::At(shapes) if positional && shapes.len() == len => &shapes[i],
                Kids::At(_) => return,
            };
            let child = match self.heap.get(h) {
                Obj::List(xs) | Obj::Tuple(xs) => xs.get(i).copied(),
                Obj::Enum { payload, .. } => payload.get(i).copied(),
                Obj::Struct { fields, .. } => fields.get(i).copied(),
                Obj::Map(m) => m.entries.get(i).map(|e| e.2),
                Obj::Str(_)
                | Obj::Bytes(_)
                | Obj::ByteArray(_)
                | Obj::BigInt(_)
                | Obj::FloatBox(_)
                | Obj::Iter { .. }
                | Obj::Set(_)
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
            };
            if let Some(child) = child {
                self.unmark_fresh(child, kid);
            }
        }
    }

    /// Unmark every data object reachable from `v`: a graph the crossing site built whole. A
    /// closure, fn, generator, cell, iterator, module or handle stays marked and is not entered.
    fn unmark_graph(&mut self, v: Value) {
        let mut stack: Vec<GcRef> = v.as_obj().into_iter().collect();
        while let Some(h) = stack.pop() {
            // A tuple and an enum carry no mark and cannot form a cycle on their own, so they
            // are always entered. Every other data kind is entered once: its mark is the guard.
            let unmarked_kind = matches!(self.heap.get(h), Obj::Tuple(_) | Obj::Enum { .. });
            if !unmarked_kind && !self.heap.is_copied(h) {
                continue;
            }
            let objs = |vs: &[Value]| vs.iter().filter_map(|v| v.as_obj()).collect::<Vec<_>>();
            let kids = match self.heap.get(h) {
                Obj::List(xs) | Obj::Tuple(xs) => objs(xs),
                Obj::Enum { payload, .. } => objs(payload),
                Obj::Struct { fields, .. } => objs(fields.as_slice()),
                Obj::Set(s) => s.entries.iter().filter_map(|e| e.1.as_obj()).collect(),
                Obj::Map(m) => m
                    .entries
                    .iter()
                    .flat_map(|e| [e.1.as_obj(), e.2.as_obj()])
                    .flatten()
                    .collect(),
                Obj::ByteArray(_) => Vec::new(),
                Obj::Str(_)
                | Obj::Bytes(_)
                | Obj::BigInt(_)
                | Obj::FloatBox(_)
                | Obj::Iter { .. }
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
                | Obj::Generator(_) => continue,
            };
            self.heap.unset_copied(h);
            stack.extend(kids);
        }
    }

    /// Rebuild a crossing generator's frame slots (TICKET-190, TICKET-240). Every slot takes the
    /// route's ambient mark; then, on a marking route, each slot is unmarked as far as
    /// [`crossing::frame_slot`] says for generator `proto` created with `stamp`: its root only, or
    /// its whole graph. A frame never takes a positional shape (it lives on between build and
    /// crossing). Unmarking runs after the whole frame is rebuilt, so a root also reached from
    /// another slot is unmarked once.
    fn rebuild_frame_slots(
        &mut self,
        slots: Vec<WireValue>,
        rebuild: &mut super::fxhash::FxHashMap<u32, GcRef>,
        proto: ProtoId,
        stamp: u64,
    ) -> Vec<Value> {
        let out = self.rebuild_items(slots, rebuild, |w| w);
        if self.copy_mark {
            let p = &self.program.protos[proto];
            let (private, deep, arity) = (p.private_slots, p.deep_slots, p.arity);
            for (k, &v) in out.iter().enumerate() {
                match crossing::frame_slot(private, deep, arity, stamp, k) {
                    crossing::SlotFresh::Marked => {}
                    crossing::SlotFresh::Root => self.unmark_fresh(v, &Fresh::root()),
                    crossing::SlotFresh::All => self.unmark_fresh(v, &Fresh::All),
                }
            }
        }
        out
    }

    /// Worker behind [`Vm::from_wire`] — reconstructs into this heap, threading `rebuild` (wire `id` →
    /// placeholder `GcRef`) so any value cycle (a recursive local `fn`, a self-referential struct/list/
    /// map, a mixed struct+closure cycle) round-trips. Every identity-preserved arm (`Cell`/`Closure`
    /// AND every container) TIES THE KNOT: alloc a placeholder object FIRST, register its `id` in
    /// `rebuild`, recurse the children (a nested `Backref(id)` resolves to the placeholder), then
    /// `heap.get_mut`-patch the placeholder with the reconstructed children. Memory-safe: `Heap::alloc`
    /// never collects (heap.rs), so no GC fires between placeholder-alloc and the patch, and `GcRef` is
    /// a GC-traced index (never a raw pointer) — the placeholder can never dangle or alias.
    #[allow(clippy::wrong_self_convention)]
    pub(super) fn from_wire_memo(
        &mut self,
        w: WireValue,
        rebuild: &mut super::fxhash::FxHashMap<u32, GcRef>,
    ) -> Value {
        #[cfg(test)]
        super::WIRE_DECODE_NODES.with(|c| c.set(c.get() + 1));
        match w {
            // Re-create on the DESTINATION heap: `make_int` re-inlines or re-boxes (wide) and
            // `box_float` re-boxes, so the airlock round-trip is representation-stable.
            WireValue::Int(n) => self.make_int(n),
            WireValue::Float(f) => self.box_float(f),
            WireValue::Bool(b) => Value::bool(b),
            WireValue::Nil => Value::nil(),
            // B3.3a: rebuild a fresh heap `str` from the owned bytes (by value, not the old handle).
            WireValue::Str(s) => Value::obj(self.heap.alloc(Obj::Str(s.into()))),
            // Rebuild a fresh heap `bytes` from the owned raw bytes (by value, like `str`).
            WireValue::Bytes(b) => Value::obj(self.heap.alloc(Obj::Bytes(b))),
            // Rebuild a FRESH, independent heap `bytearray` from the owned raw bytes (deep copy across
            // the airlock, like `list`) — the other side never shares this VM's buffer.
            WireValue::ByteArray(b) => {
                let h = self.heap.alloc(Obj::ByteArray(b.into_vec()));
                if self.copy_mark {
                    self.heap.set_copied(h);
                }
                Value::obj(h)
            }
            WireValue::Handle(h) => Value::obj(h),
            // B3.1: rebuild a fresh heap handle onto the SAME shared core (`Arc` already cloned in
            // `to_wire`). Not registered in `self.executors` — the original `NewExecutor` handle there
            // drives the program-exit auto-drain and shares this core, so the alias needs no entry.
            WireValue::Channel(core) => Value::obj(self.heap.alloc(Obj::Channel(core))),
            WireValue::Shared(core) => Value::obj(self.heap.alloc(Obj::Shared(core))),
            WireValue::RwShared(core) => Value::obj(self.heap.alloc(Obj::RwShared(core))),
            WireValue::Atomic(core) => Value::obj(self.heap.alloc(Obj::Atomic(core))),
            WireValue::AtomicInt(core) => Value::obj(self.heap.alloc(Obj::AtomicInt(core))),
            WireValue::Executor(core) => Value::obj(self.heap.alloc(Obj::Executor(core))),
            // D6: rebuild a fresh heap handle onto the SAME shared socket/listener core (`Arc` cloned
            // in `to_wire`) — two fibers reach one fd.
            WireValue::Socket(core) => Value::obj(self.heap.alloc(Obj::Socket(core))),
            WireValue::Listener(core) => Value::obj(self.heap.alloc(Obj::Listener(core))),
            // R2: rebuild a fresh heap handle onto the SAME shared writer core (`Arc` cloned in
            // `to_wire`) — two fibers reach one output.
            WireValue::Writer(core) => Value::obj(self.heap.alloc(Obj::Writer(core))),
            // R2b: rebuild a fresh heap handle onto the SAME shared reader core (`Arc` cloned in
            // `to_wire`) — two fibers reach one fd.
            WireValue::Reader(core) => Value::obj(self.heap.alloc(Obj::Reader(core))),
            // Rebuild a fresh `Obj::Ptr` from the raw address carried by value (heap-independent).
            WireValue::Ptr(a) => Value::obj(self.heap.alloc(Obj::Ptr(a))),
            // The heap's one `Obj::Builtin` for this id ([`Vm::fn_value`], TICKET-226).
            WireValue::Builtin(b) => self.fn_value(FnKey::Builtin(b), || Obj::Builtin(b)),
            // The heap's one `Obj::Native` for this fn ([`Vm::fn_value`], TICKET-215), from the name +
            // fn pointer carried by value (pure code) — same as the `SnapValue::Native` rebuild.
            WireValue::Native { name, func, kind } => {
                let key = FnKey::Native(name.clone(), func as usize);
                self.fn_value(key, || Obj::Native { name, func, kind })
            }
            // The heap's one `Obj::Cffi` for this fn ([`Vm::fn_value`], TICKET-215), sharing the SAME
            // `Arc<Cffi>` (no re-dlopen) — same as the `SnapValue::Cffi` rebuild.
            WireValue::Cffi(c) => {
                self.fn_value(FnKey::Cffi(Arc::as_ptr(&c) as usize), || Obj::Cffi(c))
            }
            // TIE THE KNOT (see the `Cell`/`Closure` arms): alloc an empty placeholder container
            // FIRST, register its `id` in `rebuild` BEFORE recursing children, so a nested
            // `Backref(id)` (a self-referential list/struct/map) resolves to this exact handle; then
            // `heap.get_mut`-patch the placeholder with the reconstructed children. `Heap::alloc` never
            // collects, so no GC runs between the placeholder and the patch. (Nothing READS the
            // placeholder's contents mid-reconstruction — only its handle identity is observed.)
            WireValue::List { id, items } => {
                // TICKET-111 — adopted: this capture IS the module global's object. Still discard-
                // rebuild the children so every id they define registers (gotcha 2), then return the
                // adopted handle instead of a fresh one.
                if let Some(h) = self.adopt_node(id) {
                    rebuild.insert(id, h);
                    self.rebuild_items(items, rebuild, |x| x);
                    if self.copy_mark {
                        self.heap.set_copied(h);
                    }
                    return Value::obj(h);
                }
                let h = self.heap.alloc(Obj::List(Vec::new()));
                if self.copy_mark {
                    self.heap.set_copied(h);
                }
                rebuild.insert(id, h);
                let cloned = self.rebuild_items(items, rebuild, |x| x);
                *self.heap.get_mut(h) = Obj::List(cloned);
                Value::obj(h)
            }
            WireValue::Tuple { id, items } => {
                if let Some(h) = self.adopt_node(id) {
                    rebuild.insert(id, h);
                    self.rebuild_items(items, rebuild, |x| x);
                    return Value::obj(h);
                }
                let h = self.heap.alloc(Obj::Tuple(Vec::new()));
                rebuild.insert(id, h);
                let cloned = self.rebuild_items(items, rebuild, |x| x);
                *self.heap.get_mut(h) = Obj::Tuple(cloned);
                Value::obj(h)
            }
            WireValue::Iter { id, items, pos } => {
                if let Some(h) = self.adopt_node(id) {
                    rebuild.insert(id, h);
                    self.rebuild_items(items, rebuild, |x| x);
                    return Value::obj(h);
                }
                let h = self.heap.alloc(Obj::Iter {
                    items: Vec::new(),
                    pos,
                });
                rebuild.insert(id, h);
                let cloned = self.rebuild_items(items, rebuild, |x| x);
                match self.heap.get_mut(h) {
                    Obj::Iter { items, .. } => *items = cloned,
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
                    | Obj::Generator(_) => unreachable!("placeholder was alloc'd as Obj::Iter"),
                }
                Value::obj(h)
            }
            WireValue::Map { id, entries } => {
                if let Some(h) = self.adopt_node(id) {
                    rebuild.insert(id, h);
                    for (_, k, val) in entries.into_entries() {
                        self.from_wire_memo(k, rebuild);
                        self.from_wire_memo(val, rebuild);
                    }
                    if self.copy_mark {
                        self.heap.set_copied(h);
                    }
                    return Value::obj(h);
                }
                let h = self.heap.alloc(Obj::Map(MapData::default()));
                if self.copy_mark {
                    self.heap.set_copied(h);
                }
                rebuild.insert(id, h);
                // Reconstruction reuses the CARRIED hash (`push(hash, …)`) — never re-hashes a
                // (possibly cyclic) key, and keeps iteration order + index byte-identical.
                let mut out = MapData::default();
                for (hash, k, val) in entries.into_entries() {
                    let ck = self.from_wire_memo(k, rebuild);
                    let cv = self.from_wire_memo(val, rebuild);
                    out.push(hash, ck, cv);
                }
                *self.heap.get_mut(h) = Obj::Map(out);
                Value::obj(h)
            }
            WireValue::Set { id, entries } => {
                if let Some(h) = self.adopt_node(id) {
                    rebuild.insert(id, h);
                    for (_, e) in entries.into_entries() {
                        self.from_wire_memo(e, rebuild);
                    }
                    if self.copy_mark {
                        self.heap.set_copied(h);
                    }
                    return Value::obj(h);
                }
                let h = self.heap.alloc(Obj::Set(SetData::default()));
                if self.copy_mark {
                    self.heap.set_copied(h);
                }
                rebuild.insert(id, h);
                let mut out = SetData::default();
                for (hash, e) in entries.into_entries() {
                    let ce = self.from_wire_memo(e, rebuild);
                    out.push(hash, ce);
                }
                *self.heap.get_mut(h) = Obj::Set(out);
                Value::obj(h)
            }
            WireValue::Struct { id, name, fields } => {
                if let Some(h) = self.adopt_node(id) {
                    rebuild.insert(id, h);
                    self.rebuild_items(fields, rebuild, |(_, val)| val);
                    if self.copy_mark {
                        self.heap.set_copied(h);
                    }
                    return Value::obj(h);
                }
                // Positional layout: the wire fields arrive in declaration order (to_wire emits
                // them so), so rebuild positionally — the carried names are discarded.
                let tid = self.struct_tid(&name);
                let h = self.heap.alloc(Obj::Struct {
                    tid,
                    fields: Fields::from_vec(Vec::new()),
                });
                if self.copy_mark {
                    self.heap.set_copied(h);
                }
                rebuild.insert(id, h);
                let cloned = self.rebuild_items(fields, rebuild, |(_, val)| val);
                match self.heap.get_mut(h) {
                    Obj::Struct { fields, .. } => *fields = Fields::from_vec(cloned),
                    Obj::Str(_)
                    | Obj::Bytes(_)
                    | Obj::ByteArray(_)
                    | Obj::BigInt(_)
                    | Obj::FloatBox(_)
                    | Obj::Iter { .. }
                    | Obj::List(_)
                    | Obj::Tuple(_)
                    | Obj::Map(_)
                    | Obj::Set(_)
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
                    | Obj::Generator(_) => unreachable!("placeholder was alloc'd as Obj::Struct"),
                }
                Value::obj(h)
            }
            WireValue::Enum {
                id,
                variant_id,
                payload,
            } => {
                if let Some(h) = self.adopt_node(id) {
                    rebuild.insert(id, h);
                    self.rebuild_items(payload, rebuild, |x| x);
                    return Value::obj(h);
                }
                let h = self.heap.alloc(Obj::Enum {
                    variant_id,
                    payload: Vec::new(),
                });
                rebuild.insert(id, h);
                let cloned = self.rebuild_items(payload, rebuild, |x| x);
                // M19 lever #2 — the dense `variant_id` crossed the airlock directly (shared
                // `Arc<Program>`), so it is replayed as-is — no lossy name re-resolution.
                match self.heap.get_mut(h) {
                    Obj::Enum { payload, .. } => *payload = cloned,
                    Obj::Str(_)
                    | Obj::Bytes(_)
                    | Obj::ByteArray(_)
                    | Obj::BigInt(_)
                    | Obj::FloatBox(_)
                    | Obj::Iter { .. }
                    | Obj::List(_)
                    | Obj::Tuple(_)
                    | Obj::Map(_)
                    | Obj::Set(_)
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
                    | Obj::Generator(_) => unreachable!("placeholder was alloc'd as Obj::Enum"),
                }
                Value::obj(h)
            }
            // A back-reference closes a serialized value cycle (or, for a `Cell`, ties a second
            // reference to the one binding — W7-4): resolve it to the placeholder registered under this
            // `id`. Present whenever the rebuild map spans the SAME scope the serialize memo did —
            // `to_wire` emits the defining node before any `Backref` to it, and `from_wire_memo`
            // registers the placeholder BEFORE recursing children. Keeping those two scopes equal is
            // the caller's job (see [`deep_clone_all`](Vm::deep_clone_all)); a wire drained PIECEWISE
            // (`RwShared`'s read views) may hold a piece that back-references a sibling piece or the
            // ROOT container, so those views rebuild the whole wire into ONE map first
            // ([`Vm::from_wire_pieces`], `Vm::box_copy_pieces`, TICKET-154).
            //
            // W7-11 — that last case used to `.expect` here and ABORT THE HOST on a legal program
            // (`a.next = xs; RwShared(xs).at(0)`). It now flags the miss and hands back an inert
            // placeholder; [`from_wire_pieces`](Vm::from_wire_pieces) sees the flag and re-does the
            // rebuild over the WHOLE container, which defines the id. The placeholder is never
            // observable: the only caller that can legitimately trip the flag discards this whole
            // result, and every other caller is `debug_assert`ed in [`from_wire`](Vm::from_wire).
            WireValue::Backref(id) => match rebuild.get(&id) {
                Some(&h) => Value::obj(h),
                None => {
                    self.wire_backref_missing = true;
                    Value::nil()
                }
            },
            // Rebuild a FRESH, independent `Obj::Cell` on this side (deep copy, never a shared box) —
            // the receiving task owns its own cell (design §4 F1). TIE THE KNOT: alloc a placeholder
            // `Cell(Nil)` and register its `id` BEFORE recursing `inner`, so a nested `Backref(id)`
            // (the self-cell a recursive local `fn` closes) resolves to this exact handle; then patch
            // the placeholder with the reconstructed inner. `Heap::alloc` never collects, so no GC runs
            // between the placeholder and the patch.
            // W7-4: a wire may carry the SAME cell definition more than once (`snapshot_modules`
            // re-emits it per module so each is self-contained) — the first rebuild wins and every later
            // definition of that id resolves to it, exactly like a `Backref`. That is what keeps
            // `Channel.recv`/`Shared.get`/`RwShared.get` on ONE cell per binding.
            WireValue::Cell { id, inner } => {
                if let Some(&prev) = rebuild.get(&id) {
                    return Value::obj(prev);
                }
                let h = self.heap.alloc(Obj::Cell(Value::nil()));
                if self.copy_mark {
                    self.heap.set_copied(h);
                }
                rebuild.insert(id, h);
                let inner = self.from_wire_memo(*inner, rebuild);
                *self.heap.get_mut(h) = Obj::Cell(inner);
                Value::obj(h)
            }
            // B3.6: rebuild a submitted closure by value over the worker's reconstructed home module
            // (the `proto` is shared via `Arc<Program>`; captures reconstruct bottom-up into this heap).
            // `worker_home` resolves the home index against this VM's `module_objs` (the rebuilt graph
            // in a pool worker, or the live graph directly when running with no heap of its own).
            // TIE THE KNOT like
            // the `Cell` arm: resolve `home` and alloc a placeholder `Closure` with `Nil` captures,
            // register its `id`, THEN recurse the captures (a nested `Backref(id)` resolves to this
            // handle), and patch the captured slots. `worker_home`/`from_wire_memo` may `alloc` but never
            // collect, so the placeholder cannot be lost mid-reconstruction.
            WireValue::Closure {
                id,
                proto,
                captured,
                home,
                origin_heap,
            } => {
                let home = self.worker_home(home);
                let n = captured.len();
                let h = self.heap.alloc(Obj::Closure {
                    proto,
                    captured: vec![Value::nil(); n],
                    home,
                });
                rebuild.insert(id, h);
                self.heap.note_stored_from(h, origin_heap);
                // Lever #3: rebuild positionally — push values in wire (slot) order, discard the
                // carried names (they live in `proto.capture_names`). `to_wire` emits in slot order.
                // D4 layer C (owner ruling 2026-09-23): the captures follow
                // `Route::ClosureCaptures` (TICKET-189). Back in the exact heap they serialized out
                // of — a same-task `Channel`/`Shared` round-trip the value never left (W7-4c) —
                // they keep the enclosing mark. In any other heap they are marked, even when the
                // closure crosses outside an ambient `copy_mark` walk (a Channel to another task).
                let saved_copy_mark = self.copy_mark;
                self.copy_mark = crossing::marks(
                    Route::ClosureCaptures {
                        same_heap: origin_heap == self.heap.id(),
                    },
                    saved_copy_mark,
                );
                let cap = self.rebuild_items(captured, rebuild, |(_k, w)| w);
                self.copy_mark = saved_copy_mark;
                // Owner decision D2 (TICKET-137): a crossing carries captures only. The closure's
                // globals are whatever module copy the RUNNING task owns (`home` resolved above).
                match self.heap.get_mut(h) {
                    Obj::Closure { captured, .. } => {
                        *captured = cap;
                    }
                    Obj::Str(_)
                    | Obj::Bytes(_)
                    | Obj::ByteArray(_)
                    | Obj::BigInt(_)
                    | Obj::FloatBox(_)
                    | Obj::Iter { .. }
                    | Obj::List(_)
                    | Obj::Tuple(_)
                    | Obj::Map(_)
                    | Obj::Set(_)
                    | Obj::Struct { .. }
                    | Obj::Enum { .. }
                    | Obj::Cell(_)
                    | Obj::Func { .. }
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
                    | Obj::Generator(_) => unreachable!("placeholder was alloc'd as Obj::Closure"),
                }
                Value::obj(h)
            }
            // B3.3: rebuild a bare fn by value over the worker's reconstructed home module (the `proto`
            // is shared via `Arc<Program>`; `worker_home` resolves the home index like the Closure arm).
            WireValue::Func { proto, home } => {
                let home = self.worker_home(home);
                self.fn_value(FnKey::Proto(proto), || Obj::Func { proto, home })
            }
            // F3 path C: rebuild a FRESH, independent `GeneratorCore` on this heap (deep-copy
            // independence, like `Cell`/`Iter`). `worker_home` resolves the home index; the backing
            // closure + parked slots reconstruct bottom-up into this heap. `Pending` reuses
            // `alloc_generator`; `Done`/`Suspended` build the core directly (this method is in-module,
            // so it can construct the private `CallFrame`/`GenCtx`/`GeneratorCore`). The rebuilt frame's
            // `home`/`closure` reuse the core's rebuilt `GcRef`s (they were equal at serialize time).
            WireValue::Generator {
                id,
                proto,
                home,
                closure,
                state,
            } => {
                // TICKET-111 — adopted: this capture IS the module global's object. Still rebuild the
                // closure/parked children so every id their subtree defines registers (gotcha 2 — a
                // later global's Backref into this discarded subtree must still resolve), then discard
                // the result and return the adopted handle.
                if let Some(h) = self.adopt_node(id) {
                    rebuild.insert(id, h);
                    if let Some(c) = closure {
                        self.from_wire_memo(*c, rebuild);
                    }
                    match state {
                        WireGenState::Pending(wargs, _) => {
                            self.rebuild_items(wargs, rebuild, |w| w);
                        }
                        WireGenState::Suspended { stack, .. } => {
                            self.rebuild_items(stack, rebuild, |w| w);
                        }
                        WireGenState::Done | WireGenState::Unsendable(_) => {}
                    }
                    return Value::obj(h);
                }
                let home = self.worker_home(home);
                let closure = closure.map(|c| {
                    self.from_wire_memo(*c, rebuild)
                        .as_obj()
                        .expect("a generator's backing closure wire rebuilds to a heap object")
                });
                let g = match state {
                    WireGenState::Pending(wargs, stamp) => {
                        let args = self.rebuild_frame_slots(wargs, rebuild, proto, stamp);
                        let g = self.alloc_generator(proto, home, closure, args);
                        if let Some(h) = g.as_obj()
                            && let Obj::Generator(core) = self.heap.get_mut(h)
                        {
                            core.stamp = stamp;
                        }
                        g
                    }
                    WireGenState::Done | WireGenState::Unsendable(_) => {
                        let state = match state {
                            WireGenState::Unsendable(m) => GenState::Unsendable(m),
                            WireGenState::Pending(..)
                            | WireGenState::Suspended { .. }
                            | WireGenState::Done => GenState::Done,
                        };
                        let core = GeneratorCore {
                            proto,
                            home,
                            closure,
                            state,
                            ctx: GenCtx::default(),
                            stamp: 0,
                        };
                        Value::obj(self.heap.alloc(Obj::Generator(Box::new(core))))
                    }
                    WireGenState::Suspended {
                        frame,
                        stack,
                        call_depth,
                        cur_base,
                        handlers,
                        stamp,
                    } => {
                        let stack = self.rebuild_frame_slots(stack, rebuild, proto, stamp);
                        let rebuilt = CallFrame {
                            proto: frame.proto,
                            ip: frame.ip,
                            base: frame.base,
                            home,
                            closure,
                            counted: frame.counted,
                            is_toplevel: frame.is_toplevel,
                            deferred: Vec::new(),
                            defer_markers: frame.defer_markers,
                            nursery_len: frame.nursery_len,
                            has_implicit_nursery: frame.has_implicit_nursery,
                            call_span: frame.call_span,
                            argc: frame.argc,
                        };
                        // Backlog arm (b): the live `recover:` handlers cross as plain-data (`Copy`,
                        // `GcRef`-free), so they need no reconstruction — their `usize` indices
                        // address the frame/stack rebuilt just above and stay valid on this heap.
                        // `generator_next` rebases each handler's `nursery_len` to the resuming
                        // driver's floor, so the (now stale, cross-heap) sender value is inert.
                        let ctx = GenCtx {
                            frames: vec![rebuilt],
                            stack,
                            call_depth,
                            cur_base,
                            handlers,
                        };
                        let core = GeneratorCore {
                            proto,
                            home,
                            closure,
                            state: GenState::Suspended,
                            ctx,
                            stamp,
                        };
                        Value::obj(self.heap.alloc(Obj::Generator(Box::new(core))))
                    }
                };
                // TICKET-111 — a generator's id is adoption-only (never a `Backref` target, DEC-100),
                // but `rebuild_ready` still needs its rebuilt handle to seed `snapshot_adopt`.
                rebuild.insert(id, g.as_obj().expect("generator rebuilds to a heap object"));
                g
            }
        }
    }
}

impl Vm {
    /// TICKET-111 — the pending adoption capture for `id`, if this is `fault_module`'s own replay
    /// (`adopt_active`) and this task's `snapshot_adopt` still holds one. Consuming (`remove`), so a
    /// second reach of the same id within the same replay mints/rebuilds fresh instead of re-adopting
    /// an already-claimed handle. `None` on EVERY other path that reaches `from_wire_memo` — a Channel
    /// message's ids are a separate id space that must never be checked against this map (gotcha 1,
    /// `airlock_adoption_is_scoped_to_the_snapshot_replay`).
    fn adopt_node(&mut self, id: u32) -> Option<GcRef> {
        if !self.adopt_active {
            return None;
        }
        self.snapshot_adopt.remove(&id)
    }
}
