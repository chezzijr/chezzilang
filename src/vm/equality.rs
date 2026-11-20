//! VM: `==` and `compare`, map and set slots and probes, reentrancy guards, `values_equal_guarded`.
//! Split out of `src/vm/arith.rs`. `super::*` is the parent module.

use super::arith::cmp_int_f64;
use super::arith::float_order;
use super::*;

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
            // `false` before `true` (Python `False < True`, Rust `bool: Ord`).
            (ValueView::Bool(a), ValueView::Bool(b)) => Some(a.cmp(&b)),
            (ValueView::Obj(ha), ValueView::Obj(hb)) => {
                match (self.heap.get(ha), self.heap.get(hb)) {
                    (Obj::Str(a), Obj::Str(b)) => Some(a.cmp(b)),
                    // `bytes`/`bytearray` order lexicographically by byte (Python parity), including
                    // cross-type (Python `b"a" < bytearray(b"b")` compares by content).
                    (Obj::Bytes(a), Obj::Bytes(b)) => Some(a.cmp(b)),
                    (Obj::ByteArray(a), Obj::ByteArray(b)) => Some(a.iter().cmp(b.iter())),
                    (Obj::Bytes(a), Obj::ByteArray(b)) => Some(a.iter().copied().cmp(b.iter())),
                    (Obj::ByteArray(a), Obj::Bytes(b)) => Some(a.iter().cmp(b.iter().copied())),
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
    /// where an element/field/entry is being matched (a map/set probe, the set algebra); the
    /// walk's own element compares and the list scans take [`Self::elem_run`].
    ///
    /// `&mut self` since M23: the worker can dispatch a user `eq(self, o: Self) -> bool`, which
    /// re-enters the VM (synchronously here). The identity short-circuit still runs FIRST, so an
    /// element compared against itself never reaches user code (CPython's
    /// `PyObject_RichCompareBool`, which the `==` OPERATOR deliberately does not share).
    #[inline]
    pub(super) fn elem_equal(
        &mut self,
        l: Value,
        r: Value,
        depth: usize,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        let eq = self.elem_run(l, r, depth, None, span)?;
        Ok(eq.expect("a walk with no continuation answers"))
    }

    /// First candidate position in a LOCAL (not-yet-heap) Set's `entries` whose element structurally
    /// equals `key`, or `None`. `cands` are `candidates(hash(key))` positions; `entries[p].1` is the
    /// element. Depth-fault propagating (`?`): a cyclic operand raises the same recoverable
    /// "maximum structural depth" fault as `==`.
    ///
    /// Only for a `SetData` the caller owns (a half-built literal / set-algebra result). A Set that
    /// already lives on the heap must go through [`Self::set_probe`].
    #[inline]
    pub(super) fn set_slot(
        &mut self,
        entries: &RVec<SetEntry>,
        cands: impl Iterator<Item = usize>,
        key: Value,
        span: Span,
    ) -> Result<Option<usize>, RuntimeError> {
        // The local's elements live only in the caller's Rust-local `SetData` — root them across the
        // (re-entrant) compare.
        let elems: Vec<Value> = if self.eq_may_reenter() {
            entries.iter().map(|(_, e)| e).collect()
        } else {
            Vec::new()
        };
        self.with_elem_roots(&elems, &[key], |vm| {
            for p in cands {
                if vm.elem_equal(entries.get(p).1, key, 0, span)? {
                    return Ok(Some(p));
                }
            }
            Ok(None)
        })
    }

    /// First candidate position in a LOCAL (not-yet-heap) Map's `entries` whose key structurally
    /// equals `key`, or `None`. `cands` are `candidates(hash(key))` positions; `entries[p].1` is the
    /// stored key. Depth-fault propagating (see [`Self::set_slot`]).
    ///
    /// Only for a `MapData` the caller owns; a heap Map must go through [`Self::map_probe`].
    #[inline]
    pub(super) fn map_slot(
        &mut self,
        entries: &RVec<MapEntry>,
        cands: impl Iterator<Item = usize>,
        key: Value,
        span: Span,
    ) -> Result<Option<usize>, RuntimeError> {
        // Keys AND values of the caller's Rust-local `MapData` are in flight — root both (see
        // [`Self::set_slot`]).
        let elems: Vec<Value> = if self.eq_may_reenter() {
            entries.iter().flat_map(|(_, k, v)| [k, v]).collect()
        } else {
            Vec::new()
        };
        self.with_elem_roots(&elems, &[key], |vm| {
            for p in cands {
                if vm.elem_equal(entries.get(p).1, key, 0, span)? {
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
    /// The first candidate is read alone (a distinct key almost always has exactly one candidate:
    /// NO allocation); the second step copies the remaining candidates once (`next_candidate`),
    /// so a constant-hash probe stays linear.
    ///
    /// **The map's structural version is re-checked after EVERY compare** and the whole probe
    /// restarts with fresh candidates if it moved (CPython `lookdict`'s `DKIX_KEY_CHANGED` →
    /// restart). Checking only the compared position is NOT enough: an `eq` that removes ANOTHER
    /// key shifts every later position, so the copied candidates go stale and the next step misses
    /// a key that is still present; an `eq` that mutates on the compare that returns TRUE hands the
    /// caller a shifted position, and `m.get(k)` answered a neighbour's value or panicked `index out
    /// of bounds`. The check is one counter load per compare (`MapData::version`), gated to nothing
    /// when no `eq` can re-enter. A restart re-runs the user `eq`, so an `eq` that mutates on EVERY call spins — the
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
        // run user code, so both the rooting and the version check are pure overhead.
        let reenter = self.eq_may_reenter();
        let held = [Value::obj(mh), key];
        let roots: &[Value] = if reenter { &held } else { &[] };
        self.with_roots(roots, |vm| {
            'restart: loop {
                // The candidates past the first, read once per restart (see `next_candidate`).
                let mut rest = None;
                let mut i = 0;
                loop {
                    let (p, ver, stored) = match vm.heap.get(mh).as_map() {
                        Some(m) => match next_candidate(i, &mut rest, || m.candidates(hk)) {
                            Some(p) => match m.entries.try_get(p) {
                                Some((_, k, _)) => (p, m.version(), k),
                                None => return Ok(None),
                            },
                            None => return Ok(None),
                        },
                        None => unreachable!("map_probe on non-map"),
                    };
                    let eq = vm.elem_equal(stored, key, 0, span)?;
                    // No push or remove during the compare? (A value write — `m[k] = v` inside
                    // `eq` — moves nothing and does not count.) If one ran, every position, `p`
                    // and the copied candidates included, is stale: start over.
                    let live = !reenter
                        || match vm.heap.get(mh).as_map() {
                            Some(m) => m.version() == ver,
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

    /// The value of `key` (pre-hashed to `hk`) in the HEAP map at `mh`, or `None`: [`Self::map_probe`],
    /// then the entry it found. M3.0a — another thread can remove entries between the two reads, so
    /// a position the entry block no longer holds is a miss ([`MapData::value_at`]).
    #[inline]
    pub(super) fn map_lookup(
        &mut self,
        mh: GcRef,
        hk: u64,
        key: Value,
        span: Span,
    ) -> Result<Option<Value>, RuntimeError> {
        let Some(p) = self.map_probe(mh, hk, key, span)? else {
            return Ok(None);
        };
        match self.heap.get(mh).as_map() {
            Some(m) => Ok(m.value_at(p)),
            None => unreachable!("map_lookup on non-map"),
        }
    }

    /// The Set twin of [`Self::map_probe`] — probe the HEAP set at `sh` for `elem` (pre-hashed to
    /// `he`), rooting `sh`/`elem` across the (re-entrant) equality and re-checking the set's
    /// structural version after every compare (see [`Self::map_probe`] for why).
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
                let mut rest = None;
                let mut i = 0;
                loop {
                    let (p, ver, stored) = match vm.heap.get(sh).as_set() {
                        Some(s) => match next_candidate(i, &mut rest, || s.candidates(he)) {
                            Some(p) => match s.entries.try_get(p) {
                                Some((_, e)) => (p, s.version(), e),
                                None => return Ok(None),
                            },
                            None => return Ok(None),
                        },
                        None => unreachable!("set_probe on non-set"),
                    };
                    let eq = vm.elem_equal(stored, elem, 0, span)?;
                    let live = !reenter
                        || match vm.heap.get(sh).as_set() {
                            Some(s) => s.version() == ver,
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

    /// Root the ELEMENTS of two cloned element/entry vecs across `f` — a local set/map probe's
    /// shape, and a no-op when no `eq` can re-enter (see [`Self::eq_may_reenter`]).
    ///
    /// Rooting the source CONTAINER is not enough: the probe holds the child handles in a Rust
    /// local, and a re-entrant `eq` that empties the source orphans every child still to be
    /// compared (a `dangling GcRef` abort, or a compare against a recycled object). The `==` walk
    /// roots its children in its level slots instead (`walk.rs`).
    /// `#[inline(never)]` is MEASURED, not decoration: inlining the closures back into their
    /// callers once cost the neighbouring codegen **+3.3% on the `struct` bench** — a bench with no
    /// `==` in it at all (`docs/contributing/benchmarks.md`).
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
    /// popped receiver/argument, a cloned element vec's source) must be
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

    /// Depth-guarded structural equality, synchronous: [`Vm::eq_run`] with no continuation, so a
    /// hooked pair's `eq` runs nested. The `==` operator, the list scans and the `eq` intrinsic
    /// take the walk that parks; the map/set probes and the set algebra keep this one. Returns
    /// `Err` (recoverable) once the walk exceeds [`MAX_STRUCTURAL_DEPTH`].
    #[inline]
    pub(super) fn values_equal_guarded(
        &mut self,
        l: Value,
        r: Value,
        depth: usize,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        let eq = self.eq_run(l, r, depth, None, span)?;
        Ok(eq.expect("a walk with no continuation answers"))
    }

    /// Total order over scalar values for `sort()`. The checker restricts `sort` to a homogeneous
    /// Comparable element type. A float pair orders by `float_order` (NaN to one end); every other
    /// pair reads the one scalar order, [`Self::compare`]. An unordered pair compares Equal.
    pub(super) fn value_order(&self, a: Value, b: Value) -> std::cmp::Ordering {
        if a.is_float() && b.is_float() {
            return float_order(self.float_of(a), self.float_of(b));
        }
        self.compare(a, b).unwrap_or(std::cmp::Ordering::Equal)
    }

    // ----- calls -----
}

/// Candidate `i` of a heap probe. A probe cannot hold the table across a compare (a user `eq` can
/// reach a safepoint), so step 0 reads the first candidate alone (no allocation: the common hit),
/// and step 1 copies the rest into `rest` once; later steps index it. A push or remove during a
/// compare (the table's version moved) makes the caller restart with a fresh `rest`.
fn next_candidate<I: Iterator<Item = usize>>(
    i: usize,
    rest: &mut Option<Vec<usize>>,
    fresh: impl FnOnce() -> I,
) -> Option<usize> {
    if i == 0 {
        return fresh().next();
    }
    rest.get_or_insert_with(|| fresh().skip(1).collect())
        .get(i - 1)
        .copied()
}
