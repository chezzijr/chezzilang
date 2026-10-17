//! VM: `==` and `compare`, map and set slots and probes, reentrancy guards, `values_equal_guarded`.
//! Split out of `src/vm/arith.rs`. `super::*` is the parent module.

use super::arith::cmp_int_f64;
use super::arith::float_order;
use super::*;
use crate::base::hooks::Hook;

impl Vm {
    pub(super) fn compare(&self, l: Value, r: Value) -> Option<std::cmp::Ordering> {
        // Both integral (inline or boxed) → exact i64 order. The both-integral arm above has
        // already returned, so at most one side of a mixed numeric pair is integral here — the
        // other side is always a boxed float (`is_numeric` is int | float | integral), so
        // `cmp_int_f64` compares the int exactly instead of coercing it through f64 (NaN → None).
        if self.is_integral(l) && self.is_integral(r) {
            return Some(self.int_of(l).cmp(&self.int_of(r)));
        }
        if self.is_integral(l) && self.is_numeric(r) {
            return cmp_int_f64(self.int_of(l), self.float_of(r));
        }
        if self.is_integral(r) && self.is_numeric(l) {
            return cmp_int_f64(self.int_of(r), self.float_of(l)).map(|o| o.reverse());
        }
        if self.is_numeric(l) && self.is_numeric(r) {
            return self.float_of(l).partial_cmp(&self.float_of(r));
        }
        match (l.view(), r.view()) {
            (ValueView::Obj(ha), ValueView::Obj(hb)) => {
                match (self.heap.get(ha), self.heap.get(hb)) {
                    (Obj::Str(a), Obj::Str(b)) => Some(a.cmp(b)),
                    // `bytes`/`bytearray` order lexicographically by byte (Python parity), including
                    // cross-type (Python `b"a" < bytearray(b"b")` compares by content).
                    (Obj::Bytes(a), Obj::Bytes(b)) => Some(a.cmp(b)),
                    (Obj::ByteArray(a), Obj::ByteArray(b)) => Some(a.cmp(b)),
                    (Obj::Bytes(a), Obj::ByteArray(b)) => Some(a.as_ref().cmp(b.as_slice())),
                    (Obj::ByteArray(a), Obj::Bytes(b)) => Some(a.as_slice().cmp(b.as_ref())),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Test-only `bool` convenience over the depth-guarded worker. A depth-exceeded fault (cyclic
    /// data) degrades to "not equal" — acceptable in unit tests, but NOT for production container
    /// membership / key-equality, which must SURFACE the fault the same way `==`/`!=` do (see the
    /// `*_slot` helpers below). Kept `#[cfg(test)]` so no production caller swallows the fault.
    #[cfg(test)]
    pub(super) fn values_equal(&mut self, l: Value, r: Value) -> bool {
        self.values_equal_guarded(l, r, 0, Span::RUNTIME)
            .unwrap_or(false)
    }

    /// Per-ELEMENT equality inside a container: CPython's `x is y or x == y`. The raw-word compare
    /// IS identity here — a float is heap-boxed per alloc (`Obj::FloatBox` behind its own Float tag),
    /// so ONE `nan` value stored into two containers carries the same word and compares equal, while
    /// two independently-computed NaNs keep distinct boxes and stay unequal. NaN is the only behavior
    /// change: for every other value raw-word equality already implied `==`.
    ///
    /// NOT for the `==` OPERATOR — bare `nan == nan` must stay false (Python parity). Use this only
    /// where an element/field/entry is being matched (`in`, `index_of`, `has`, and the recursive arms
    /// of [`Self::values_equal_guarded`]); the operator's own entry point calls the worker directly.
    ///
    /// `&mut self` since M23: the worker can dispatch a user `eq(self, o: Self) -> bool`, which
    /// re-enters the VM. The identity short-circuit below still runs FIRST, so an element compared
    /// against itself never reaches user code (CPython's `PyObject_RichCompareBool`, which the
    /// `==` OPERATOR deliberately does not share — see [`Self::values_equal_guarded`]).
    #[inline]
    pub(super) fn elem_equal(
        &mut self,
        l: Value,
        r: Value,
        depth: usize,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        #[cfg(test)]
        super::ELEM_COMPARES.with(|c| c.set(c.get() + 1));
        if l == r {
            return Ok(true);
        }
        self.values_equal_guarded(l, r, depth, span)
    }

    /// First index in `hay` structurally-equal to `needle`, or `None`. Depth-fault propagating
    /// (`?`), so a cyclic operand raises the same recoverable "maximum structural depth" fault as
    /// `==` instead of silently comparing unequal. Allocation-free flat scan.
    ///
    /// `hay` must NOT be borrowed out of `self.heap` (M23: `elem_equal` takes `&mut self`) — every
    /// caller already passes a cloned/local element vec. For a heap Map/Set use
    /// [`Self::map_probe`] / [`Self::set_probe`], which re-read the container per candidate.
    ///
    /// Roots `hay` + `needle` HERE rather than at each of the four call sites: the callers root the
    /// source list and the needle, which does NOT keep the cloned elements alive once a re-entrant
    /// `eq` empties that list. One guard in the shared scan covers `in`, `contains`, `index_of` and
    /// `unique` alike.
    #[inline]
    pub(super) fn seq_slot(
        &mut self,
        hay: &[Value],
        needle: Value,
        span: Span,
    ) -> Result<Option<usize>, RuntimeError> {
        self.with_elem_roots(hay, &[needle], |vm| {
            for (i, v) in hay.iter().enumerate() {
                if vm.elem_equal(*v, needle, 0, span)? {
                    return Ok(Some(i));
                }
            }
            Ok(None)
        })
    }

    /// First candidate position in a LOCAL (not-yet-heap) Set's `entries` whose element structurally
    /// equals `key`, or `None`. `cands` are `candidates(hash(key))` positions; `entries[p].1` is the
    /// element. Depth-fault propagating (see [`Self::seq_slot`]).
    ///
    /// Only for a `SetData` the caller owns (a half-built literal / set-algebra result). A Set that
    /// already lives on the heap must go through [`Self::set_probe`].
    #[inline]
    pub(super) fn set_slot(
        &mut self,
        entries: &[(u64, Value)],
        cands: &[usize],
        key: Value,
        span: Span,
    ) -> Result<Option<usize>, RuntimeError> {
        // The local's elements live only in the caller's Rust-local `SetData` — root them across the
        // (re-entrant) compare, exactly as [`Self::seq_slot`] does for a cloned element vec.
        let elems: Vec<Value> = if self.eq_may_reenter() {
            entries.iter().map(|&(_, e)| e).collect()
        } else {
            Vec::new()
        };
        self.with_elem_roots(&elems, &[key], |vm| {
            for &p in cands {
                if vm.elem_equal(entries[p].1, key, 0, span)? {
                    return Ok(Some(p));
                }
            }
            Ok(None)
        })
    }

    /// First candidate position in a LOCAL (not-yet-heap) Map's `entries` whose key structurally
    /// equals `key`, or `None`. `cands` are `candidates(hash(key))` positions; `entries[p].1` is the
    /// stored key. Depth-fault propagating (see [`Self::seq_slot`]).
    ///
    /// Only for a `MapData` the caller owns; a heap Map must go through [`Self::map_probe`].
    #[inline]
    pub(super) fn map_slot(
        &mut self,
        entries: &[(u64, Value, Value)],
        cands: &[usize],
        key: Value,
        span: Span,
    ) -> Result<Option<usize>, RuntimeError> {
        // Keys AND values of the caller's Rust-local `MapData` are in flight — root both (see
        // [`Self::set_slot`]).
        let elems: Vec<Value> = if self.eq_may_reenter() {
            entries.iter().flat_map(|&(_, k, v)| [k, v]).collect()
        } else {
            Vec::new()
        };
        self.with_elem_roots(&elems, &[key], |vm| {
            for &p in cands {
                if vm.elem_equal(entries[p].1, key, 0, span)? {
                    return Ok(Some(p));
                }
            }
            Ok(None)
        })
    }

    /// Probe the HEAP map at `mh` for `key` (pre-hashed to `hk`), returning the matching entry
    /// position. The heap-resident twin of [`Self::map_slot`]: `elem_equal` needs `&mut self` (a user
    /// `eq` re-enters the VM), so the entry slice can no longer be held borrowed across the compare —
    /// the candidate positions are read out ONCE and each stored key is re-read from the heap per
    /// probe step.
    ///
    /// Roots `mh` and `key` on the operand stack for the duration (both are typically off-stack Rust
    /// locals at a method-dispatch site, and a user `eq` can allocate → collect). A caller holding
    /// FURTHER in-flight values (an insert's `val`) wraps the call in [`Self::with_roots`].
    /// Nothing is copied out of the heap: the candidate list is re-indexed per probe step (a distinct
    /// key almost always has exactly one candidate, so that is one extra index lookup on the
    /// terminating step and NO allocation).
    ///
    /// **The returned position is re-validated after EVERY compare** and the whole probe restarts if
    /// it moved (CPython `lookdict`'s `DKIX_KEY_CHANGED` → restart). Re-reading the candidate list
    /// alone is NOT enough: an `eq` that mutates the map on the compare that returns TRUE hands the
    /// caller a position that has already shifted, and every caller indexes with it — `m.get(k)`
    /// answered a neighbouring entry's value, or panicked `index out of bounds` on the shrunken
    /// `entries` (an uncatchable process abort from pure Chezzi source). Validation is one extra
    /// index read of `(hash, key)` on the terminating step, gated to nothing when no `eq` can
    /// re-enter. A restart re-runs the user `eq`, so an `eq` that mutates on EVERY call spins — the
    /// same way an `eq` containing `while true` does, and interruptible for the same reason (it is
    /// running bytecode); "mutate the map you are being probed against" stays a bad idea, it is just
    /// no longer memory-unsafe.
    #[inline]
    pub(super) fn map_probe(
        &mut self,
        mh: GcRef,
        hk: u64,
        key: Value,
        span: Span,
    ) -> Result<Option<usize>, RuntimeError> {
        // One whole-program answer, hoisted: with no `eq` hook anywhere, NOTHING inside this loop can
        // run user code, so both the rooting and the position re-validation are pure overhead.
        let reenter = self.eq_may_reenter();
        let held = [Value::obj(mh), key];
        let roots: &[Value] = if reenter { &held } else { &[] };
        self.with_roots(roots, |vm| {
            'restart: loop {
                let mut i = 0;
                loop {
                    let (p, hs, stored) = match vm.heap.get(mh).as_map() {
                        Some(m) => match m.candidates(hk).get(i) {
                            Some(&p) => match m.entries.get(p) {
                                Some(&(h, k, _)) => (p, h, k),
                                None => return Ok(None),
                            },
                            None => return Ok(None),
                        },
                        None => unreachable!("map_probe on non-map"),
                    };
                    let eq = vm.elem_equal(stored, key, 0, span)?;
                    // Position still holding the key we just compared? (The entry's VALUE may have
                    // changed — `m[k] = v` inside `eq` does not move anything — so only the cached
                    // hash and the key are checked.) If not, the map was structurally mutated and
                    // every position, `p` included, is stale: start over.
                    let live = !reenter
                        || match vm.heap.get(mh).as_map() {
                            Some(m) => m
                                .entries
                                .get(p)
                                .is_some_and(|&(h, k, _)| h == hs && k == stored),
                            None => unreachable!("map_probe on non-map"),
                        };
                    if !live {
                        continue 'restart;
                    }
                    if eq {
                        return Ok(Some(p));
                    }
                    i += 1;
                }
            }
        })
    }

    /// The Set twin of [`Self::map_probe`] — probe the HEAP set at `sh` for `elem` (pre-hashed to
    /// `he`), rooting `sh`/`elem` across the (re-entrant) equality and re-validating the position
    /// after every compare (see [`Self::map_probe`] for why the re-read alone is not enough).
    #[inline]
    pub(super) fn set_probe(
        &mut self,
        sh: GcRef,
        he: u64,
        elem: Value,
        span: Span,
    ) -> Result<Option<usize>, RuntimeError> {
        let reenter = self.eq_may_reenter();
        let held = [Value::obj(sh), elem];
        let roots: &[Value] = if reenter { &held } else { &[] };
        self.with_roots(roots, |vm| {
            'restart: loop {
                let mut i = 0;
                loop {
                    let (p, hs, stored) = match vm.heap.get(sh).as_set() {
                        Some(s) => match s.candidates(he).get(i) {
                            Some(&p) => match s.entries.get(p) {
                                Some(&(h, e)) => (p, h, e),
                                None => return Ok(None),
                            },
                            None => return Ok(None),
                        },
                        None => unreachable!("set_probe on non-set"),
                    };
                    let eq = vm.elem_equal(stored, elem, 0, span)?;
                    let live = !reenter
                        || match vm.heap.get(sh).as_set() {
                            Some(s) => s
                                .entries
                                .get(p)
                                .is_some_and(|&(h, e)| h == hs && e == stored),
                            None => unreachable!("set_probe on non-set"),
                        };
                    if !live {
                        continue 'restart;
                    }
                    if eq {
                        return Ok(Some(p));
                    }
                    i += 1;
                }
            }
        })
    }

    /// Can ANY equality in this program dispatch a user `eq` — i.e. can a compare re-enter the VM and
    /// therefore collect? `Program::any_eq_hook` is a whole-program answer, and `false` for the
    /// overwhelming majority of programs. Used to skip the probe rooting on the
    /// hot map/set/`==` paths, where it would otherwise be pure overhead.
    #[inline]
    pub(super) fn eq_may_reenter(&self) -> bool {
        self.program.any_eq_hook
    }

    /// Root the ELEMENTS of two cloned element/field/entry vecs across `f` — the container-equality
    /// shape, and a no-op when no `eq` can re-enter (see [`Self::eq_may_reenter`]).
    ///
    /// Rooting the two source CONTAINERS is not enough. A container-equality arm snapshots the child
    /// handles into a Rust local and then walks it; a re-entrant `eq` that empties the sources
    /// orphans every child still to be compared, and the next `heap.get` on one of them is a
    /// `dangling GcRef` abort (or, before the collection catches up, a compare against a recycled
    /// object — equality that depends on GC timing). The children are what the walk actually holds,
    /// so the children are what gets pinned; the containers are pinned transitively by whatever
    /// rooted the top of the walk. Inductive down the recursion: each level roots the children it
    /// hands to the next.
    /// `#[inline(never)]` is MEASURED, not decoration: inlining the six container-arm closures back
    /// into `values_equal_guarded` bloats it enough to cost the neighbouring codegen **+3.3% on the
    /// `struct` bench** — a bench with no `==` in it at all. Keeping the walks out of line puts every
    /// bench back to flat (`docs/contributing/benchmarks.md`).
    #[inline(never)]
    pub(super) fn with_elem_roots<T>(
        &mut self,
        a: &[Value],
        b: &[Value],
        f: impl FnOnce(&mut Self) -> Result<T, RuntimeError>,
    ) -> Result<T, RuntimeError> {
        if !self.eq_may_reenter() {
            return f(self);
        }
        let roots: Vec<Value> = a.iter().chain(b).copied().collect();
        self.with_roots(&roots, f)
    }

    /// Run `f` with `roots` pinned on the operand stack, unrooting on BOTH the `Ok` and `Err` path.
    /// The generalisation of [`Self::hash_key_rooted`]: a user `hash`/`eq` hook re-enters the VM and
    /// can trigger a collection, so every in-flight value the caller holds only in a Rust local (a
    /// popped receiver/argument, a cloned element vec's source, a wire-reconstructed value) must be
    /// reachable from a GC root for the duration. The operand stack is truncated back to its entry
    /// height afterwards, so `f` may also push roots of its own (a freshly-built snapshot key) and
    /// need not unwind them itself.
    pub(super) fn with_roots<T>(
        &mut self,
        roots: &[Value],
        f: impl FnOnce(&mut Self) -> Result<T, RuntimeError>,
    ) -> Result<T, RuntimeError> {
        let base = self.stack.len();
        for &r in roots {
            self.push(r);
        }
        let out = f(self);
        self.stack.truncate(base);
        out
    }

    /// Depth-guarded structural equality — and, since M23, the single place a user
    /// `eq(self, o: Self) -> bool` is dispatched from, so EVERY consumer (the `==`/`!=` operator,
    /// `in`, `index_of`, `unique`/`dedup`, map/set key probing, the recursive `List`/`Tuple`/`Map`/
    /// `Set`/`Struct`/`Enum` arms) inherits it. Returns `Err` (recoverable) once recursion exceeds
    /// [`MAX_STRUCTURAL_DEPTH`] — guarding against cyclic data structures overflowing the host stack.
    ///
    /// The hook is dispatched BEFORE the `ha == hb` identity short-circuit: CPython's `do_richcompare`
    /// has no identity fast path either, so `x == x` really does call the user's `eq`. Containers get
    /// the identity shortcut from [`Self::elem_equal`] instead (CPython's
    /// `PyObject_RichCompareBool`), which never reaches this function for an identical pair.
    ///
    /// `MAX_STRUCTURAL_DEPTH` is threaded through the structural recursion, and the guard tests
    /// `walk_base + depth` — so a user `eq` that itself compares does NOT restart at 0: it continues
    /// on the SAME budget (see [`Vm::walk_base`]), CPython's model. Call depth alone never rescues
    /// this: the repro that motivated the fix (W8-43) dies at hook-nesting depth ~5 000 of a 10 000
    /// `MAX_CALL_DEPTH` cap, because each of those 5 000 levels stacks a fresh 100-frame native walk
    /// on the host stack — the call-depth guard counts the levels, not their product.
    pub(super) fn values_equal_guarded(
        &mut self,
        l: Value,
        r: Value,
        depth: usize,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        if self.walk_base + depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.depth_exceeded_err(span));
        }
        // Exact i64 equality when BOTH operands are integral (inline `Int` OR boxed `BigInt`) —
        // Python parity. MUST precede the mixed arm below: the both-integral arm has already
        // returned there, so at most one side of a mixed numeric pair is integral, and the other
        // side is always a boxed float. The canonical-rep invariant (inline XOR boxed) makes this
        // correct across kinds: an inline int and a boxed big-int are never equal.
        if self.is_integral(l) && self.is_integral(r) {
            return Ok(self.int_of(l) == self.int_of(r));
        }
        // Mixed int/float (`1 == 1.0` above 2^53) compares exactly via `cmp_int_f64`, never `as_f64`.
        if self.is_integral(l) && self.is_numeric(r) {
            return Ok(
                cmp_int_f64(self.int_of(l), self.float_of(r)) == Some(std::cmp::Ordering::Equal)
            );
        }
        if self.is_integral(r) && self.is_numeric(l) {
            return Ok(
                cmp_int_f64(self.int_of(r), self.float_of(l)) == Some(std::cmp::Ordering::Equal)
            );
        }
        // float==float compares via f64 (so `nan == nan` stays `false`).
        if self.is_numeric(l) && self.is_numeric(r) {
            return Ok(self.float_of(l) == self.float_of(r));
        }
        match (l.view(), r.view()) {
            (ValueView::Bool(a), ValueView::Bool(b)) => Ok(a == b),
            (ValueView::Nil, ValueView::Nil) => Ok(true),
            (ValueView::Obj(ha), ValueView::Obj(hb)) => {
                // `Eq` protocol (M23): two values of one declared type that has the `eq` hook. Both operands are already GC-reachable from
                // whatever rooted the top of this walk (`with_roots` at the entry points).
                // `eq_hook_off` is `Atomic.cas`'s window (see the field's doc): a compare under the
                // box's value lock must stay structural, whatever the checker did or did not see.
                if !self.eq_hook_off && self.eq_hook_applies(l, r) {
                    // The hook re-enters the VM and may start its own structural walk, so hand it
                    // the depth this walk has already consumed — one shared budget (see
                    // [`Vm::walk_base`]), not a fresh 10 000 per nesting level.
                    let base = self.walk_base + depth;
                    let res = self
                        .guarded_walk(base, |vm| vm.invoke_hook(Hook::Eq, l, &[r], span))?
                        .unwrap_or(Value::nil());
                    return match res.as_bool() {
                        Some(b) => Ok(b),
                        None => Err(self.err(
                            format!("eq() must return bool, got {}", self.type_name(res)),
                            span,
                        )),
                    };
                }
                if ha == hb {
                    return Ok(true);
                }
                // Snapshot the element/entry handles out of the heap so the borrow is released before
                // recursing through `&self` methods (mirrors the borrow discipline of the seq paths).
                match (self.heap.get(ha), self.heap.get(hb)) {
                    (Obj::Str(a), Obj::Str(b)) => Ok(a == b),
                    (Obj::Bytes(a), Obj::Bytes(b)) => Ok(a == b),
                    // `bytearray` equality is structural byte-equality. Cross-type `bytes ==
                    // bytearray` is content-equal (Python parity: `b"a" == bytearray(b"a")` is true).
                    (Obj::ByteArray(a), Obj::ByteArray(b)) => Ok(a == b),
                    (Obj::Bytes(a), Obj::ByteArray(b)) => Ok(a.as_ref() == b.as_slice()),
                    (Obj::ByteArray(a), Obj::Bytes(b)) => Ok(a.as_slice() == b.as_ref()),
                    (Obj::List(a), Obj::List(b)) => {
                        if a.len() != b.len() {
                            return Ok(false);
                        }
                        let (a, b): (Vec<Value>, Vec<Value>) = (a.clone(), b.clone());
                        self.with_elem_roots(&a, &b, |vm| {
                            for (x, y) in a.iter().zip(&b) {
                                if !vm.elem_equal(*x, *y, depth + 1, span)? {
                                    return Ok(false);
                                }
                            }
                            Ok(true)
                        })
                    }
                    (Obj::Tuple(a), Obj::Tuple(b)) => {
                        if a.len() != b.len() {
                            return Ok(false);
                        }
                        let (a, b): (Vec<Value>, Vec<Value>) = (a.clone(), b.clone());
                        self.with_elem_roots(&a, &b, |vm| {
                            for (x, y) in a.iter().zip(&b) {
                                if !vm.elem_equal(*x, *y, depth + 1, span)? {
                                    return Ok(false);
                                }
                            }
                            Ok(true)
                        })
                    }
                    // Maps are unordered: equal iff same size and every (key, value) entry of `a` has
                    // a structurally-equal match in `b` (mirrors the Set arm; the cached hash is unused).
                    (Obj::Map(a), Obj::Map(b)) => {
                        if a.entries.len() != b.entries.len() {
                            return Ok(false);
                        }
                        let ae: Vec<(Value, Value)> =
                            a.entries.iter().map(|(_, k, v)| (*k, *v)).collect();
                        let be: Vec<(Value, Value)> =
                            b.entries.iter().map(|(_, k, v)| (*k, *v)).collect();
                        // Keys AND values are in flight here — flatten both sides for the roots.
                        let flat = |e: &[(Value, Value)]| -> Vec<Value> {
                            e.iter().flat_map(|&(k, v)| [k, v]).collect()
                        };
                        let (af, bf) = if self.eq_may_reenter() {
                            (flat(&ae), flat(&be))
                        } else {
                            (Vec::new(), Vec::new())
                        };
                        self.with_elem_roots(&af, &bf, |vm| {
                            for (ka, va) in &ae {
                                let mut found = false;
                                for (kb, vb) in &be {
                                    if vm.elem_equal(*ka, *kb, depth + 1, span)?
                                        && vm.elem_equal(*va, *vb, depth + 1, span)?
                                    {
                                        found = true;
                                        break;
                                    }
                                }
                                if !found {
                                    return Ok(false);
                                }
                            }
                            Ok(true)
                        })
                    }
                    // Sets are unordered: equal iff same size and every element of `a` is in `b`.
                    (Obj::Set(a), Obj::Set(b)) => {
                        if a.entries.len() != b.entries.len() {
                            return Ok(false);
                        }
                        let ae: Vec<Value> = a.entries.iter().map(|(_, x)| *x).collect();
                        let be: Vec<Value> = b.entries.iter().map(|(_, x)| *x).collect();
                        self.with_elem_roots(&ae, &be, |vm| {
                            for x in &ae {
                                let mut found = false;
                                for y in &be {
                                    if vm.elem_equal(*x, *y, depth + 1, span)? {
                                        found = true;
                                        break;
                                    }
                                }
                                if !found {
                                    return Ok(false);
                                }
                            }
                            Ok(true)
                        })
                    }
                    (
                        Obj::Struct {
                            tid: ta,
                            fields: fa,
                        },
                        Obj::Struct {
                            tid: tb,
                            fields: fb,
                        },
                    ) => {
                        // Positional structural compare: the `ta != tb` guard preserves type
                        // distinction (same tid ⇒ same StructDef ⇒ identical field order), so a
                        // by-position value compare suffices — no per-field name clone needed. Equal
                        // tid ⟹ same struct type (tids are dense per-type ids), one int compare.
                        if ta != tb || fa.len() != fb.len() {
                            return Ok(false);
                        }
                        let fa: Vec<Value> = fa.as_slice().to_vec();
                        let fb: Vec<Value> = fb.as_slice().to_vec();
                        self.with_elem_roots(&fa, &fb, |vm| {
                            for (va, vb) in fa.iter().zip(&fb) {
                                if !vm.elem_equal(*va, *vb, depth + 1, span)? {
                                    return Ok(false);
                                }
                            }
                            Ok(true)
                        })
                    }
                    (
                        Obj::Enum {
                            variant_id: va,
                            payload: pa,
                        },
                        Obj::Enum {
                            variant_id: vb,
                            payload: pb,
                        },
                    ) => {
                        // M19 lever #2 — equal `variant_id` ⟹ same enum type AND variant (ids are
                        // globally unique per (enum, variant) pair), so this one int compare subsumes the
                        // old `ty == ty && variant == variant`.
                        if va != vb || pa.len() != pb.len() {
                            return Ok(false);
                        }
                        let pa: Vec<Value> = pa.clone();
                        let pb: Vec<Value> = pb.clone();
                        self.with_elem_roots(&pa, &pb, |vm| {
                            for (x, y) in pa.iter().zip(&pb) {
                                if !vm.elem_equal(*x, *y, depth + 1, span)? {
                                    return Ok(false);
                                }
                            }
                            Ok(true)
                        })
                    }
                    // Two first-class builtin-fn values are equal iff they name the SAME builtin. Each
                    // value-position use emits a fresh `Op::LoadBuiltin` → a distinct handle, so the
                    // `ha == hb` identity short-circuit above never fires; compare by name instead
                    // (derived `PartialEq` on the `Rc<str>` name).
                    (Obj::Builtin(a), Obj::Builtin(b)) => Ok(a == b),
                    // Boxed scalars compare by value, identically to the inline `Int`/`Float` arms.
                    (Obj::BigInt(a), Obj::BigInt(b)) => Ok(a == b),
                    (Obj::FloatBox(a), Obj::FloatBox(b)) => Ok(a == b),
                    // TICKET-177: a handle (or a `ptr`) is equal iff it names the same core, its
                    // `Obj::identity` key. A crossing wraps the same core in a fresh slot, so the
                    // `ha == hb` shortcut above is only the fast path. Anything else reaching here is
                    // a kind mismatch or a `Slot` object in another slot: unequal.
                    (a, b) => Ok(matches!(
                        (a.identity(), b.identity()),
                        (Identity::Core(x), Identity::Core(y)) if x == y
                    )),
                }
            }
            _ => Ok(false),
        }
    }

    /// The kind of the first heap object reachable from `v` that is a fn value or whose `==` is
    /// slot identity ([`Identity::Slot`]), or `None`. `Atomic.cas` refuses such a payload: every
    /// `load()` rebuilds a slot-identity object in a fresh slot, so the compare could never succeed
    /// and a CAS retry loop would spin forever (TICKET-144, DEC-144). Every fn value is refused,
    /// a builtin (`ord`) included, whose `==` is by name: one rule for every fn, as Go's func is
    /// uncomparable (TICKET-264). [`Vm::callable`] decides fn-ness; [`Obj::identity`], read by the
    /// equality arm too, decides the rest. A `Core` handle is not descended: it compares by core,
    /// never by what the core holds. `seen` makes a cyclic payload terminate.
    pub(super) fn slot_identity_in(&self, v: Value) -> Option<&'static str> {
        let mut stack: Vec<GcRef> = v.child_gcref().into_iter().collect();
        let mut seen = super::fxhash::FxHashSet::default();
        while let Some(h) = stack.pop() {
            if !seen.insert(h) {
                continue;
            }
            if self.callable(Value::obj(h)).is_some() {
                return Some("a function value");
            }
            match self.heap.get(h).identity() {
                Identity::Slot(what) => return Some(what),
                Identity::Core(_) => {}
                Identity::Content => stack.extend(self.heap.children(h)),
            }
        }
        None
    }

    /// Total order over scalar values for `sort()`. The checker restricts `sort` to homogeneous
    /// int/float/str lists; str elements are read through the heap. Anything else compares Equal.
    pub(super) fn value_order(&self, a: Value, b: Value) -> std::cmp::Ordering {
        use std::cmp::Ordering::Equal;
        // Homogeneous lists only (checker-enforced): both int (inline/boxed) → exact i64; both float
        // → `float_order`; both str → lexical. A mixed/other pair compares Equal.
        if self.is_integral(a) && self.is_integral(b) {
            return self.int_of(a).cmp(&self.int_of(b));
        }
        if a.is_float() && b.is_float() {
            return float_order(self.float_of(a), self.float_of(b));
        }
        match (a.as_obj(), b.as_obj()) {
            (Some(ha), Some(hb)) => match (self.heap.get(ha), self.heap.get(hb)) {
                (Obj::Str(x), Obj::Str(y)) => x.cmp(y),
                _ => Equal,
            },
            _ => Equal,
        }
    }

    // ----- calls -----
}
