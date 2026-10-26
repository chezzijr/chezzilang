//! VM: hashing values and keys, key snapshots, cyclic walks, struct sorting.
//! Split out of `src/vm/arith.rs`. `super::*` is the parent module.

use super::*;
use crate::base::hooks::Hook;

impl Vm {
    /// A `u64` hash of `v` for map/set keys, upholding the invariant `values_equal(a,b) ⇒
    /// hash(a)==hash(b)`. Numeric keys hash by their canonical f64 bits (so `Int(3)` and `Float(3.0)`
    /// collide, matching `values_equal`'s numeric unification); str by content; a struct key
    /// dispatches its user `hash(self) -> int` (re-entrant — may allocate / trigger GC); a tuple key
    /// combines its elements' hashes in order (TICKET-161), so it faults exactly when an element
    /// would. Floats are rejected as keys by the checker (NaN footgun), so only integral-valued
    /// floats reach here.
    pub(super) fn hash_value(&mut self, v: Value, span: Span) -> Result<u64, RuntimeError> {
        self.hash_value_d(v, 0, span)
    }

    /// [`Self::hash_value`] at structural `depth` — the tuple recursion's own level. The guard tests
    /// `walk_base + depth`, and a user `hash` reached from inside a tuple runs under
    /// [`Self::guarded_walk`] at that combined base, so nested tuples and re-entrant `hash` hooks
    /// share the ONE `MAX_STRUCTURAL_DEPTH` allowance like `values_equal_guarded`.
    fn hash_value_d(&mut self, v: Value, depth: usize, span: Span) -> Result<u64, RuntimeError> {
        if self.walk_base + depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.depth_exceeded_err(span));
        }
        // A struct key dispatches its user `hash()` (re-entrant). Everything else is scalar. A boxed
        // float is Float-tagged (not `as_obj`) so it falls to the scalar arm below; a boxed `BigInt`
        // is Obj-tagged and must be treated as the integral scalar it is.
        if let Some(h) = v.as_obj() {
            match self.heap.get(h) {
                Obj::Struct { .. } | Obj::Enum { .. } if depth > 0 => {
                    let base = self.walk_base + depth;
                    self.guarded_walk(base, |vm| vm.hash_user(v, span))
                }
                Obj::Struct { .. } | Obj::Enum { .. } => self.hash_user(v, span),
                // A tuple hashes by combining its elements' hashes, in order. Equal tuples hash
                // equal because `values_equal` compares element-wise and each element's hash
                // upholds the invariant. The tuple is rooted: an element's user `hash` can collect.
                Obj::Tuple(items) => {
                    let items = items.clone();
                    self.with_roots(&[v], |vm| {
                        let mut acc: u64 = 0x345678 ^ items.len() as u64;
                        for &e in &items {
                            acc = acc.wrapping_mul(1_000_003)
                                ^ vm.hash_value_d(e, depth + 1, span)?;
                        }
                        Ok(acc)
                    })
                }
                Obj::Str(_) | Obj::Bytes(_) | Obj::BigInt(_) => Ok(self.scalar_hash(v)),
                Obj::ByteArray(_)
                | Obj::FloatBox(_)
                | Obj::Iter { .. }
                | Obj::List(_)
                | Obj::Map(_)
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
                | Obj::Generator(_) => Err(self.err(
                    format!(
                        "{} is not hashable (cannot be a map/set key)",
                        self.type_name(v)
                    ),
                    span,
                )),
            }
        } else {
            Ok(self.scalar_hash(v))
        }
    }

    /// Dispatch the user `hash(self) -> int` of a struct / enum key. An enum resolves
    /// through the shared enum-aware resolver, mirroring the struct path. Both re-enter the VM — may allocate / trigger GC.
    fn hash_user(&mut self, v: Value, span: Span) -> Result<u64, RuntimeError> {
        let Some(h) = v.as_obj() else { unreachable!() };
        match self.heap.get(h) {
            Obj::Struct { .. } | Obj::Enum { .. } => self.declared_hash(v, span),
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
            | Obj::Generator(_) => unreachable!("hash_user receiver is a struct or enum"),
        }
    }

    /// Infallible hash for scalar keys (int/float/bool/nil/str). Numeric values hash by canonical
    /// f64 bits so `3` and `3.0` collide; str by content. Non-scalar values fall back to `0` (a
    /// correctness-safe degenerate hash — `values_equal` still confirms each probe).
    pub(super) fn scalar_hash(&self, v: Value) -> u64 {
        use std::hash::{Hash, Hasher};
        // A boxed float is Float-tagged; unwrap it heap-side. Normalise zero so `+0.0`/`-0.0` (both
        // `values_equal`) hash identically.
        if v.is_float() {
            let f = self.float_of(v);
            return (if f == 0.0 { 0.0 } else { f }).to_bits();
        }
        match v.view() {
            // Normalise zero so `Int(0)`, `+0.0`, and `-0.0` (all `values_equal`) hash identically —
            // `(-0.0).to_bits() != (0.0).to_bits()` would otherwise break the hash invariant.
            ValueView::Int(n) => (if n == 0 { 0.0 } else { n as f64 }).to_bits(),
            ValueView::Bool(b) => b as u64,
            ValueView::Nil => 0,
            ValueView::Obj(h) => match self.heap.get(h) {
                Obj::Str(s) => {
                    let mut hr = std::collections::hash_map::DefaultHasher::new();
                    s.as_bytes().hash(&mut hr);
                    hr.finish()
                }
                // `bytes` is Hashable (immutable, value-compared). Hash the raw slice — mandatory so
                // `Map[bytes, T]`/`Set[bytes]` keys distribute instead of all colliding on `0`.
                Obj::Bytes(b) => {
                    let mut hr = std::collections::hash_map::DefaultHasher::new();
                    b.as_ref().hash(&mut hr);
                    hr.finish()
                }
                // A boxed scalar hashes identically to the inline `Int`/`Float` of the same value
                // (same zero-normalised f64-bits scheme as the `Value::Int`/`Value::Float` arms
                // above) — mandatory for the "behaves like inline" invariant.
                Obj::BigInt(n) => (if *n == 0 { 0.0 } else { *n as f64 }).to_bits(),
                Obj::FloatBox(f) => (if *f == 0.0 { 0.0 } else { *f }).to_bits(),
                Obj::ByteArray(_)
                | Obj::Iter { .. }
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
                | Obj::Generator(_) => 0,
            },
        }
    }

    /// Whether `v` is a flat scalar hash key: inline `Int`/`Bool`/`Nil`, a Float-tagged box, or
    /// `Obj::Str`/`Obj::Bytes`/`Obj::BigInt`. `Struct`/`Enum` are excluded because their
    /// `hash`/`eq` re-enter the VM and `unique()` requires no `hash` at all; `ByteArray` is excluded
    /// because `bytes == bytearray` is content-equal; containers are excluded because `hash_value`
    /// faults on them.
    #[inline]
    pub(super) fn is_flat_hash_key(&self, v: Value) -> bool {
        match v.as_obj() {
            None => true,
            Some(h) => matches!(
                self.heap.get(h),
                Obj::Str(_) | Obj::Bytes(_) | Obj::BigInt(_)
            ),
        }
    }

    /// Hash a struct or enum key through its `hash` hook, returning the `i64` as a `u64`. The
    /// checker admits no struct or enum key without the hook (TICKET-280), so a miss faults.
    pub(super) fn declared_hash(&mut self, v: Value, span: Span) -> Result<u64, RuntimeError> {
        if let Some(res) = self.reentered(|vm| vm.invoke_hook(Hook::Hash, v, &[], span))? {
            return match self.int_val(res) {
                Some(n) => Ok(n as u64),
                None => Err(self.err(
                    format!("hash() must return int, got {}", self.type_name(res)),
                    span,
                )),
            };
        }
        let Obj::Struct { tid, .. } = self.heap.get(v.as_gcref()) else {
            return Err(self.missing_hook(v, Hook::Hash, span));
        };
        let name = self.struct_name_of_tid(*tid);
        let shown = self
            .program
            .structs
            .get(name)
            .map_or(name, |d| d.display_name.as_str());
        Err(self.err(
            format!("struct '{shown}' has no 'hash' method (needed to use it as a map/set key)"),
            span,
        ))
    }

    /// Hash `key`, keeping `roots` alive on the operand stack across the call. A struct key's
    /// `hash()` re-enters the VM and can trigger GC; the map/set receiver and any in-flight
    /// key/value (already popped off the stack before dispatch) must be rooted or the collector
    /// could free them mid-hash. For scalar keys this is a couple of redundant push/pops.
    pub(super) fn hash_key_rooted(
        &mut self,
        key: Value,
        roots: &[Value],
        span: Span,
    ) -> Result<u64, RuntimeError> {
        self.with_roots(roots, |vm| vm.hash_value(key, span))
    }

    /// Snapshot (deep-copy) a struct/enum key/element on the STORE path (Go value-key
    /// model): after the key is stored, mutating the caller's original value can no longer reach the
    /// stored key, so the collection can't be silently corrupted. Only the three heap-aggregate arms
    /// `hash_value` dispatches (`Struct`/`Enum`) are copied — scalars and every immutable /
    /// by-reference object pass through UNCHANGED (zero-clone hot path). Infallible + pure-alloc (no
    /// VM re-entry ⇒ no GC fires mid-copy), so it needs no rooting; a caller that then re-enters the
    /// VM (e.g. `hash_value` on a *later* element) must root the returned snapshot itself.
    ///
    /// Deliberately NOT a full deep copy (the deleted concurrency airlock's serializer): that copy
    /// (a) FAULTED on a generator / cyclic key — nonsensical for a plain
    /// single-thread insert that previously stored the key by reference and worked — and (b) rebuilds
    /// every by-reference sub-value (a `Closure`/`Channel`/`Shared`/… field) with a FRESH handle,
    /// which `values_equal` (identity-only for those arms) then never matches, so a later lookup of
    /// the same key misses. [`Vm::snapshot_value`] instead copies only the mutable, structurally-
    /// compared arms and keeps identity/by-reference sub-values by handle, so the snapshot stays
    /// `values_equal` to the original (its cached hash is therefore still valid).
    pub(super) fn snapshot_key(&mut self, key: Value) -> Value {
        match key.view() {
            ValueView::Obj(h)
                if matches!(
                    self.heap.get(h),
                    Obj::Struct { .. } | Obj::Enum { .. } | Obj::Tuple(_)
                ) =>
            {
                // A CYCLIC or OVER-DEEP key is stored BY REFERENCE (base behavior): a structural
                // snapshot of a cycle can't be `values_equal` to the original — `values_equal` itself
                // bails on a cycle / past `MAX_STRUCTURAL_DEPTH` — so a snapshotted such key would
                // never resolve on lookup (a silent true→false regression), and the over-deep walk
                // would overflow the host stack before `snapshot_value`'s own cap runs. The snapshot's
                // whole point (value-key isolation) is anyway unattainable for a self-referential
                // mutable value.
                // ponytail: cyclic / >MAX_STRUCTURAL_DEPTH struct/enum keys keep the pre-existing
                // mutate-after-store aliasing ceiling — use shallow acyclic/immutable keys if you need
                // value-key isolation.
                if self.store_key_by_reference(key) {
                    return key;
                }
                let mut visited = super::fxhash::FxHashMap::default();
                self.snapshot_value(key, &mut visited, 0)
            }
            ValueView::Int(_) | ValueView::Bool(_) | ValueView::Nil | ValueView::Obj(_) => key,
        }
    }

    /// True iff `v`'s value graph must be stored BY REFERENCE rather than snapshotted — i.e. it
    /// contains a cycle OR is deeper than [`MAX_STRUCTURAL_DEPTH`]. Both cases are stored by handle
    /// (base behavior): a cyclic snapshot can't be `values_equal` to the original, and an over-deep
    /// snapshot's tail is aliased ([`Vm::snapshot_value`] caps there) so a held-key lookup trips the
    /// same depth guard in `values_equal` and silently misses — whereas a by-reference key resolves
    /// instantly on the `ha == hb` identity short-circuit. Read-only, allocates only two small work
    /// sets; runs on the cold struct/enum key-insert path. `on_path` holds the DFS recursion
    /// stack (a back-edge into it = cycle); `done` memoizes fully-cleared nodes so a shared (DAG)
    /// sub-value isn't re-walked (no exponential blow-up on a diamond).
    fn store_key_by_reference(&self, v: Value) -> bool {
        let mut on_path = super::fxhash::FxHashMap::default();
        let mut done = super::fxhash::FxHashMap::default();
        self.cyclic_walk(v, &mut on_path, &mut done, 0)
    }

    /// DFS behind [`Vm::store_key_by_reference`]. `depth` caps host recursion at
    /// [`MAX_STRUCTURAL_DEPTH`] — the SAME bound every sibling structural walker uses — so an
    /// over-deep ACYCLIC key returns `true` (store by reference) instead of overflowing the host
    /// stack (SIGABRT), which this walk would otherwise do BEFORE the capped `snapshot_value` runs.
    fn cyclic_walk(
        &self,
        v: Value,
        on_path: &mut super::fxhash::FxHashMap<GcRef, ()>,
        done: &mut super::fxhash::FxHashMap<GcRef, ()>,
        depth: usize,
    ) -> bool {
        let Some(h) = v.as_obj() else {
            return false;
        };
        // NOT `walk_base + depth` (W8-43). This guard's over-budget branch DEGRADES (store by
        // reference) instead of faulting, so charging the enclosing hook's consumed depth to it
        // turns a stack-safety measure into a SILENT WRONG ANSWER: an ordinary shallow acyclic key
        // inserted from inside an `eq`/`str` hook running at depth would be aliased rather than
        // snapshotted, and the caller's later mutation would reach the stored key. Only the five
        // guards whose over-budget branch FAULTS may share the budget; this one and
        // `snapshot_value`'s must stay on the bare `depth`.
        if depth > MAX_STRUCTURAL_DEPTH {
            return true; // over-deep: store by reference (no snapshot, no host-stack overflow)
        }
        if on_path.contains_key(&h) {
            return true; // back-edge into the active DFS stack → cycle
        }
        if done.contains_key(&h) {
            return false; // already fully cleared (shared DAG node)
        }
        let children: Vec<Value> = match self.heap.get(h) {
            Obj::Struct { fields, .. } => fields.to_vec(),
            Obj::Enum { payload, .. } => payload.to_vec(),
            Obj::List(items) => items.to_vec(),
            Obj::Tuple(items) => items.to_vec(),
            Obj::Map(m) => m.entries.iter().flat_map(|(_, k, val)| [k, val]).collect(),
            Obj::Set(s) => s.entries.iter().map(|(_, e)| e).collect(),
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
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
            | Obj::Generator(_) => {
                done.insert(h, ()); // leaf / by-reference object: never descended when copying
                return false;
            }
        };
        on_path.insert(h, ());
        for c in children {
            if self.cyclic_walk(c, on_path, done, depth + 1) {
                return true;
            }
        }
        on_path.remove(&h);
        done.insert(h, ());
        false
    }

    /// Recursive worker for [`Vm::snapshot_key`]. Deep-copies only the MUTABLE, structurally-`==`
    /// aggregate arms (`Struct`/`Enum`/`List`/`Tuple`/`Map`/`Set`/`ByteArray`) and returns
    /// every other value (scalars, immutable `Str`/`Bytes`/`Ptr`/`Builtin`, and all identity-compared
    /// by-reference objects: `Closure`/`Func`/`Channel`/`Shared`/…/`Generator`/`Iter`/`Cell`) BY
    /// REFERENCE — keeping those handles is what preserves `values_equal` (identity-only for them) and
    /// avoids a fault on a value a deep copy cannot rebuild. `visited` maps an original aggregate handle to its
    /// copy so a cyclic key is copied ONCE (cycle preserved, not overflowed); `depth` caps genuinely
    /// deep (non-cyclic) keys at `MAX_STRUCTURAL_DEPTH` — the SAME bound `values_equal` uses —
    /// degrading to by-reference rather than a host-stack overflow. Pure alloc (no VM re-entry) so no
    /// GC can run mid-walk, hence no rooting: the intermediate handles held in Rust locals stay live.
    fn snapshot_value(
        &mut self,
        v: Value,
        visited: &mut super::fxhash::FxHashMap<GcRef, GcRef>,
        depth: usize,
    ) -> Value {
        let Some(h) = v.as_obj() else {
            return v; // scalars
        };
        // NOT `walk_base + depth` — the twin of `cyclic_walk`'s guard above, and for the same
        // reason (W8-43): this branch DEGRADES (alias the tail) rather than faults, so a charged
        // budget silently aliases a shallow key. The two stay charged IDENTICALLY (both bare
        // `depth`) so `store_key_by_reference` and `snapshot_key` agree on which keys are over-deep.
        if depth > MAX_STRUCTURAL_DEPTH {
            return v; // absurdly deep (non-cyclic) key: stop copying, alias the tail (no overflow)
        }
        if let Some(&c) = visited.get(&h) {
            return Value::obj(c); // already copied (shared sub-value or cycle back-edge)
        }
        match self.heap.get(h) {
            Obj::List(items) => {
                let items = items.to_vec();
                let nh = self.heap.alloc(Obj::List(RVec::from_slice(&items)));
                visited.insert(h, nh);
                let copied: Vec<Value> = items
                    .iter()
                    .map(|&x| self.snapshot_value(x, visited, depth + 1))
                    .collect();
                // `nh` is this snapshot's own new list: slot stores into its one block.
                if let Obj::List(dst) = self.heap.write(nh).get(nh) {
                    for (k, x) in copied.into_iter().enumerate() {
                        dst.set(k, x);
                    }
                }
                Value::obj(nh)
            }
            Obj::Tuple(items) => {
                let items = items.clone();
                let nh = self.heap.alloc(Obj::Tuple(items.clone()));
                visited.insert(h, nh);
                let copied: Vec<Value> = items
                    .iter()
                    .map(|&x| self.snapshot_value(x, visited, depth + 1))
                    .collect();
                // `nh` is this snapshot's own new tuple, unpublished: an in-place fill.
                if let Obj::Tuple(dst) = self.heap.init_mut(nh) {
                    *dst = copied;
                }
                Value::obj(nh)
            }
            Obj::Struct { tid, fields } => {
                let (tid, vals) = (*tid, fields.to_vec());
                let nh = self.heap.alloc(Obj::Struct {
                    tid,
                    fields: Fields::from_vec(vals.clone()),
                });
                visited.insert(h, nh);
                let copied: Vec<Value> = vals
                    .iter()
                    .map(|&f| self.snapshot_value(f, visited, depth + 1))
                    .collect();
                // `nh` is this snapshot's own new struct: slot stores into its one block.
                if let Obj::Struct { fields, .. } = self.heap.write(nh).get(nh) {
                    for (k, f) in copied.into_iter().enumerate() {
                        fields.set(k, f);
                    }
                }
                Value::obj(nh)
            }
            Obj::Enum {
                variant_id,
                payload,
            } => {
                let (variant_id, payload) = (*variant_id, payload.to_vec());
                let nh = self.heap.alloc(Obj::Enum {
                    variant_id,
                    payload: RVec::from_slice(&payload),
                });
                visited.insert(h, nh);
                let copied: Vec<Value> = payload
                    .iter()
                    .map(|&p| self.snapshot_value(p, visited, depth + 1))
                    .collect();
                if let Obj::Enum { payload, .. } = self.heap.write(nh).get(nh) {
                    for (k, p) in copied.into_iter().enumerate() {
                        payload.set(k, p);
                    }
                }
                Value::obj(nh)
            }
            Obj::Map(m) => {
                let entries = m.entries.to_vec();
                // An empty placeholder for a cycle back to `h`; the copy is built once below.
                let nh = self.heap.alloc(Obj::Map(MapData::default()));
                visited.insert(h, nh);
                let copied: Vec<(u64, Value, Value)> = entries
                    .iter()
                    .map(|&(hh, k, val)| {
                        (
                            hh,
                            self.snapshot_value(k, visited, depth + 1),
                            self.snapshot_value(val, visited, depth + 1),
                        )
                    })
                    .collect();
                // The placeholder holds no block, so the overwrite frees nothing.
                *self.heap.init_mut(nh) = Obj::Map(MapData::from_entries(copied));
                Value::obj(nh)
            }
            Obj::Set(s) => {
                let entries = s.entries.to_vec();
                let nh = self.heap.alloc(Obj::Set(SetData::default()));
                visited.insert(h, nh);
                let copied: Vec<(u64, Value)> = entries
                    .iter()
                    .map(|&(hh, e)| (hh, self.snapshot_value(e, visited, depth + 1)))
                    .collect();
                *self.heap.init_mut(nh) = Obj::Set(SetData::from_entries(copied));
                Value::obj(nh)
            }
            // A `bytearray` is mutable + structurally compared but a GC LEAF (raw bytes, no children):
            // copy the buffer, no recursion, no visited entry (it can't participate in a cycle).
            Obj::ByteArray(b) => Value::obj(self.heap.alloc(Obj::ByteArray(b.clone()))),
            // Everything else (immutable `Str`/`Bytes`/`Ptr`/`Builtin` + all identity-compared
            // by-reference objects) is kept BY REFERENCE — a copy would break `values_equal` for the
            // identity arms and needlessly duplicate the immutable ones.
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
            | Obj::Generator(_) => v,
        }
    }

    /// `xs.sort()` over a list of Comparable structs/enums, ordering via each one's `compare`. Because
    /// `compare` re-enters the VM (and may allocate / trigger GC), this mirrors `list_sort_by`
    /// exactly: snapshot the elements into a heap list ROOTED on the operand stack, permute
    /// *indices* re-read from that rooted list per comparison (never holding unrooted `Value`s
    /// across a `compare` call), then write the result back. (Primitives use the faster
    /// `value_order`, which never re-enters the VM.)
    pub(super) fn list_sort_structs(
        &mut self,
        src_h: GcRef,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // Root the source list itself: a method receiver is popped before dispatch, so an inline
        // temporary (`make().sort()`) is otherwise unrooted and the comparator's GC could collect it
        // before the write-back.
        self.push(Value::obj(src_h));
        let snap_h = {
            let elems = match self.heap.get(src_h).as_list() {
                Some(v) => v.clone(),
                None => unreachable!("list_sort on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::obj(snap_h)); // ROOT the snapshot across the comparator calls
        let n = match self.heap.get(snap_h).as_list() {
            Some(v) => v.len(),
            None => unreachable!(),
        };
        let order = match self.msort_indices_structs(snap_h, (0..n).collect(), span) {
            Ok(o) => o,
            Err(e) => {
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                return Err(e);
            }
        };
        if let Err(e) = self.sort_mutation_check(src_h, snap_h, "sort", span) {
            self.pop(); // unroot snapshot
            self.pop(); // unroot source
            return Err(e);
        }
        // No comparator calls remain, so no GC: read the rooted snapshot and write the result back.
        let reordered: Vec<Value> = match self.heap.get(snap_h).as_list() {
            Some(v) => order.iter().map(|&i| v.get(i)).collect(),
            None => unreachable!(),
        };
        let heap = self.heap.write(src_h);
        if let Obj::List(v) = heap.get(src_h) {
            v.replace(heap, &reordered);
        }
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(Value::nil())
    }

    /// Stable top-down merge sort over `idx` (positions into the rooted list `src_h`), comparing
    /// elements via `order_key` (a struct/enum's `compare`, or the lexicographic walk of a tuple/List/
    /// Option — TICKET-146). Re-reads elements from `src_h` per comparison so no unrooted `Value` is
    /// held across the GC-capable call.
    pub(super) fn msort_indices_structs(
        &mut self,
        src_h: GcRef,
        idx: Vec<usize>,
        span: Span,
    ) -> Result<Vec<usize>, RuntimeError> {
        let n = idx.len();
        if n <= 1 {
            return Ok(idx);
        }
        let mut idx = idx;
        let right = idx.split_off(n / 2);
        // ponytail: not charged to `walk_base`. This recurses natively AND calls `struct_compare`,
        // which can re-enter user code — the same product shape as the hook-nested structural walk.
        // Ceiling: O(log n) native frames per call-depth level (≤64 for any list that fits in
        // memory), so 10 000 call levels × 64 frames × a few hundred bytes is tens of MiB against a
        // 384 MiB stack — it cannot reach the host limit. Charge `walk_base` here if the sort ever
        // recurses on something unbounded.
        let left = self.msort_indices_structs(src_h, idx, span)?;
        let right = self.msort_indices_structs(src_h, right, span)?;
        let mut out = Vec::with_capacity(n);
        let (mut li, mut ri) = (0, 0);
        while li < left.len() && ri < right.len() {
            let a = match self.heap.get(src_h).as_list() {
                Some(v) => v.get(left[li]),
                None => unreachable!(),
            };
            let b = match self.heap.get(src_h).as_list() {
                Some(v) => v.get(right[ri]),
                None => unreachable!(),
            };
            // `<= Equal` keeps the left element first on ties → stable.
            if self.order_key(a, b, span)?.is_le() {
                out.push(left[li]);
                li += 1;
            } else {
                out.push(right[ri]);
                ri += 1;
            }
        }
        out.extend_from_slice(&left[li..]);
        out.extend_from_slice(&right[ri..]);
        Ok(out)
    }
}
