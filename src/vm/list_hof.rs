//! VM: list higher-order methods, sorting, reductions, map pairs.
//! Split out of `src/vm/call.rs`. `super::*` is the parent module.

use super::*;

impl Vm {
    /// Higher-order list methods `map` / `filter` / `fold`. `src_h` is the receiver list.
    ///
    /// SNAPSHOT semantics: iteration walks a copy of the receiver's elements taken at call time, so
    /// a callback that MUTATES the receiver (e.g. `xs.pop()`/`xs.push(..)`) does NOT perturb the
    /// iteration sequence — it always visits exactly the elements present when the HOF was invoked.
    /// This (a) matches comprehensions/for-loops (`Op::ListClone`) and `list_sort_by`/`sort_by_key`
    /// (which snapshot), (b) matches Python `map`/`filter`, and (c) is OOB-safe: indexing the
    /// original live list while a callback shrinks it would panic (regression:
    /// `map_shrinking_callback_no_panic`).
    ///
    /// GC discipline: each element is fed to a closure via `invoke_value`, which runs nested VM
    /// frames that can trigger GC at instruction boundaries. To keep the GC from collecting in-flight
    /// heap values, the source list, the snapshot list, the partially-built result list (map/filter),
    /// and the fold accumulator are all kept rooted on the operand stack across the iteration. Returns
    /// the result (caller pushes it).
    pub(super) fn list_hof(
        &mut self,
        src_h: GcRef,
        method: &str,
        mut args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // ROOT the source list on the operand stack: a method receiver is popped before dispatch, so
        // an inline temporary (`make().map(..)`) is otherwise unrooted and the callback's GC could
        // collect it before we snapshot.
        self.push(Value::obj(src_h));
        // Take a SNAPSHOT now: iterate the receiver's elements as of call
        // time so a callback that shrinks/grows the receiver mid-iteration neither perturbs the
        // sequence nor indexes past the live (now-shorter) Vec. The snapshot is heap-allocated and
        // rooted on the operand stack so its elements survive the callback's collections.
        let snap_h = {
            let elems = match self.heap.get(src_h).as_list() {
                Some(v) => v.clone(),
                None => unreachable!("list_hof on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::obj(snap_h)); // ROOT the snapshot
        let n = match self.heap.get(snap_h).as_list() {
            Some(v) => v.len(),
            None => unreachable!(),
        };
        match method {
            "map" | "filter" => {
                if args.len() != 1 {
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(self.err(
                        format!("'{method}' expects 1 argument(s), got {}", args.len()),
                        span,
                    ));
                }
                let f = args.swap_remove(0);
                let is_filter = method == "filter";
                // ROOT the result list too.
                let res_h = self.heap.alloc(Obj::List(RVec::new()));
                self.push(Value::obj(res_h));
                for i in 0..n {
                    // Read from the rooted SNAPSHOT, not the live receiver: a callback that shrinks
                    // the receiver must not affect this index (stays valid, no OOB).
                    let elem = match self.heap.get(snap_h).as_list() {
                        Some(v) => v.get(i),
                        None => unreachable!(),
                    };
                    // May GC; source, snapshot, and result lists are rooted, so elements survive.
                    let out = self.guarded(|vm| vm.invoke_value(f, vec![elem], span))?;
                    if is_filter {
                        match out.as_bool() {
                            Some(true) => {
                                let heap = self.heap.write(res_h);
                                if let Obj::List(items) = heap.get(res_h) {
                                    items.push(heap, elem);
                                }
                            }
                            Some(false) => {}
                            None => {
                                self.pop(); // unroot result
                                self.pop(); // unroot snapshot
                                self.pop(); // unroot source
                                return Err(self.err(
                                    format!(
                                        "filter predicate must return bool, got {}",
                                        self.type_name(out)
                                    ),
                                    span,
                                ));
                            }
                        }
                    } else {
                        let heap = self.heap.write(res_h);
                        if let Obj::List(items) = heap.get(res_h) {
                            items.push(heap, out);
                        }
                    }
                }
                self.pop(); // unroot result
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                Ok(Value::obj(res_h))
            }
            "fold" => {
                if args.len() != 2 {
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(self.err(
                        format!("'fold' expects 2 argument(s), got {}", args.len()),
                        span,
                    ));
                }
                let f = args.swap_remove(1);
                let init = args.swap_remove(0);
                // ROOT the accumulator: push init, remember its slot, and replace in place each step.
                // `acc_slot` sits below every nested frame's base (frames push above the current
                // stack top and pop back to it), so the index stays valid across `invoke_value`.
                self.push(init);
                let acc_slot = self.stack.len() - 1;
                for i in 0..n {
                    // Read from the rooted SNAPSHOT (see map/filter): OOB-safe under a shrinking
                    // callback.
                    let elem = match self.heap.get(snap_h).as_list() {
                        Some(v) => v.get(i),
                        None => unreachable!(),
                    };
                    let acc = self.stack[acc_slot];
                    let new = self.guarded(|vm| vm.invoke_value(f, vec![acc, elem], span))?;
                    self.stack[acc_slot] = new;
                }
                let acc = self.pop(); // unroot accumulator
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                Ok(acc)
            }
            // take_while: NEW list of the prefix while `pred` holds; stops (pred not called) at the
            // first false. Reads the rooted SNAPSHOT — a pred that shrinks the receiver can't OOB.
            "take_while" | "drop_while" => {
                if args.len() != 1 {
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(self.err(
                        format!("'{method}' expects 1 argument(s), got {}", args.len()),
                        span,
                    ));
                }
                let f = args.swap_remove(0);
                let is_drop = method == "drop_while";
                let res_h = self.heap.alloc(Obj::List(RVec::new()));
                self.push(Value::obj(res_h)); // ROOT the result list
                let mut cut = n; // first index where pred is false (n = all-true)
                for i in 0..n {
                    let elem = match self.heap.get(snap_h).as_list() {
                        Some(v) => v.get(i),
                        None => unreachable!(),
                    };
                    let out = self.guarded(|vm| vm.invoke_value(f, vec![elem], span))?;
                    match out.as_bool() {
                        Some(true) => {
                            if !is_drop {
                                let heap = self.heap.write(res_h);
                                if let Obj::List(items) = heap.get(res_h) {
                                    items.push(heap, elem);
                                }
                            }
                        }
                        Some(false) => {
                            cut = i;
                            break;
                        }
                        None => {
                            self.pop(); // unroot result
                            self.pop(); // unroot snapshot
                            self.pop(); // unroot source
                            return Err(self.err(
                                format!(
                                    "{method} predicate must return bool, got {}",
                                    self.type_name(out)
                                ),
                                span,
                            ));
                        }
                    }
                }
                if is_drop {
                    // Collect the snapshot suffix from the first non-matching index.
                    for i in cut..n {
                        let elem = match self.heap.get(snap_h).as_list() {
                            Some(v) => v.get(i),
                            None => unreachable!(),
                        };
                        let heap = self.heap.write(res_h);
                        if let Obj::List(items) = heap.get(res_h) {
                            items.push(heap, elem);
                        }
                    }
                }
                self.pop(); // unroot result
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                Ok(Value::obj(res_h))
            }
            // count: number of snapshot elements satisfying `pred`. position: index of the FIRST
            // match as Option[int] (short-circuits), else None.
            "count" | "position" => {
                if args.len() != 1 {
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(self.err(
                        format!("'{method}' expects 1 argument(s), got {}", args.len()),
                        span,
                    ));
                }
                let f = args.swap_remove(0);
                let is_pos = method == "position";
                let mut count = 0_i64;
                let mut found: Option<usize> = None;
                for i in 0..n {
                    let elem = match self.heap.get(snap_h).as_list() {
                        Some(v) => v.get(i),
                        None => unreachable!(),
                    };
                    let out = self.guarded(|vm| vm.invoke_value(f, vec![elem], span))?;
                    match out.as_bool() {
                        Some(true) => {
                            if is_pos {
                                found = Some(i);
                                break;
                            }
                            count += 1;
                        }
                        Some(false) => {}
                        None => {
                            self.pop(); // unroot snapshot
                            self.pop(); // unroot source
                            return Err(self.err(
                                format!(
                                    "{method} predicate must return bool, got {}",
                                    self.type_name(out)
                                ),
                                span,
                            ));
                        }
                    }
                }
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                if is_pos {
                    Ok(match found {
                        Some(i) => self.alloc_enum("Option", "Some", vec![Value::int(i as i64)]),
                        None => self.alloc_enum("Option", "None", vec![]),
                    })
                } else {
                    Ok(Value::int(count))
                }
            }
            _ => unreachable!("list_hof called with non-HOF method {method}"),
        }
    }

    /// A sort callback must not mutate the list being sorted (W8-4): the list is a `Result`-less
    /// snapshot-and-write-back, so a mutation the write-back would erase is a contract violation,
    /// not something to discard silently. Compares the live list at `src_h` to the rooted snapshot
    /// at `snap_h` by raw `Value` word (`Value`'s derived `PartialEq` is bit equality — NaN-safe,
    /// and it catches a length change AND a same-length element write). Call this immediately
    /// before the write-back, at all three sort call sites.
    pub(super) fn sort_mutation_check(
        &mut self,
        src_h: GcRef,
        snap_h: GcRef,
        method: &str,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let changed = match (self.heap.get(src_h), self.heap.get(snap_h)) {
            (Obj::List(cur), Obj::List(snap)) => cur.iter().ne(snap.iter()),
            _ => true,
        };
        if changed {
            return Err(self.err(
                format!(
                    "list modified during '{method}' -- a sort callback must not mutate the list being sorted"
                ),
                span,
            ));
        }
        Ok(())
    }

    /// `xs.sort_by(cmp)` — stable in-place sort driven by a Chezzi comparator `fn(T, T) -> int`
    /// (negative = a before b, positive = a after b, zero = equal). The comparator re-enters the VM
    /// and may GC, so we never hold the elements in an unrooted Rust `Vec`: the source list stays
    /// rooted on the operand stack, and the merge sort permutes plain `usize` **indices**, re-reading
    /// elements from the rooted heap object on each comparison. The final permutation is materialised
    /// only after all comparator calls finish (no GC in between). Returns `nil`.
    pub(super) fn list_sort_by(
        &mut self,
        src_h: GcRef,
        mut args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if args.len() != 1 {
            return Err(self.err(
                format!("'sort_by' expects 1 argument(s), got {}", args.len()),
                span,
            ));
        }
        let cmp = args.swap_remove(0);
        // Root the source list itself: a method receiver is popped before dispatch, so an inline
        // temporary (`make().sort_by(...)`) is otherwise unrooted and the comparator's GC could
        // collect it before the write-back.
        self.push(Value::obj(src_h));
        // Sort a SNAPSHOT taken now: a comparator that mutates the source
        // list mid-sort must not perturb the ordering, and its mutations are discarded by the final
        // write-back. The snapshot list is itself heap-allocated and rooted on the operand stack so
        // its elements survive the comparator's collections.
        let snap_h = {
            let elems = match self.heap.get(src_h).as_list() {
                Some(v) => v.clone(),
                None => unreachable!("list_sort_by on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::obj(snap_h)); // ROOT the snapshot
        let n = match self.heap.get(snap_h).as_list() {
            Some(v) => v.len(),
            None => unreachable!(),
        };
        let order = match self.msort_indices(snap_h, (0..n).collect(), cmp, span) {
            Ok(o) => o,
            Err(e) => {
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                return Err(e);
            }
        };
        if let Err(e) = self.sort_mutation_check(src_h, snap_h, "sort_by", span) {
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
    /// elements via the Chezzi comparator `cmp`.
    pub(super) fn msort_indices(
        &mut self,
        src_h: GcRef,
        idx: Vec<usize>,
        cmp: Value,
        span: Span,
    ) -> Result<Vec<usize>, RuntimeError> {
        let n = idx.len();
        if n <= 1 {
            return Ok(idx);
        }
        let mut idx = idx;
        let right = idx.split_off(n / 2);
        let left = self.msort_indices(src_h, idx, cmp, span)?;
        let right = self.msort_indices(src_h, right, cmp, span)?;
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
            // `<= 0` keeps the left element first on ties → stable.
            if self.compare_with(cmp, a, b, span)? <= 0 {
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

    /// Run the comparator on `(a, b)` and return its int result (errors if it returns non-int).
    pub(super) fn compare_with(
        &mut self,
        cmp: Value,
        a: Value,
        b: Value,
        span: Span,
    ) -> Result<i64, RuntimeError> {
        let res = self.guarded(|vm| vm.invoke_value(cmp, vec![a, b], span))?;
        match self.int_val(res) {
            Some(n) => Ok(n),
            None => Err(self.err(
                format!(
                    "sort_by comparator must return int, got {}",
                    self.type_name(res)
                ),
                span,
            )),
        }
    }

    /// `xs.sort_by_key(f)` — stable in-place sort by a derived key `f: fn(T) -> K`. Mirrors
    /// `list_sort_by`'s GC discipline: the source list, an element snapshot, AND a parallel **keys**
    /// list are all rooted on the operand stack so the re-entrant extractor (and a Comparable-struct
    /// key's `compare`) can GC freely. Keys are computed once per element; the merge sort permutes
    /// `usize` indices, re-reading keys from the rooted keys list per comparison. Returns `nil`.
    pub(super) fn list_sort_by_key(
        &mut self,
        src_h: GcRef,
        mut args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if args.len() != 1 {
            return Err(self.err(
                format!("'sort_by_key' expects 1 argument(s), got {}", args.len()),
                span,
            ));
        }
        let f = args.swap_remove(0);
        self.push(Value::obj(src_h)); // ROOT the source list
        let snap_h = {
            let elems = match self.heap.get(src_h).as_list() {
                Some(v) => v.clone(),
                None => unreachable!("list_sort_by_key on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::obj(snap_h)); // ROOT the snapshot
        let n = match self.heap.get(snap_h).as_list() {
            Some(v) => v.len(),
            None => unreachable!(),
        };
        // Compute keys once per element into a rooted list. Each `invoke_value` may GC; already-pushed
        // keys survive because `keys_h` is rooted (a `Vec::push` into it does not itself GC).
        let keys_h = self.heap.alloc(Obj::List(RVec::with_capacity(n)));
        self.push(Value::obj(keys_h)); // ROOT the keys
        for i in 0..n {
            let e = match self.heap.get(snap_h).as_list() {
                Some(v) => v.get(i),
                None => unreachable!(),
            };
            match self.guarded(|vm| vm.invoke_value(f, vec![e], span)) {
                Ok(k) => {
                    let heap = self.heap.write(keys_h);
                    if let Obj::List(v) = heap.get(keys_h) {
                        v.push(heap, k);
                    }
                }
                Err(err) => {
                    self.pop(); // unroot keys
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(err);
                }
            }
        }
        let order = match self.msort_indices_by_key(keys_h, (0..n).collect(), span) {
            Ok(o) => o,
            Err(e) => {
                self.pop(); // unroot keys
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                return Err(e);
            }
        };
        if let Err(e) = self.sort_mutation_check(src_h, snap_h, "sort_by_key", span) {
            self.pop(); // unroot keys
            self.pop(); // unroot snapshot
            self.pop(); // unroot source
            return Err(e);
        }
        // No extractor/compare calls remain, so no GC: reorder the snapshot and write back.
        let reordered: Vec<Value> = match self.heap.get(snap_h).as_list() {
            Some(v) => order.iter().map(|&i| v.get(i)).collect(),
            None => unreachable!(),
        };
        let heap = self.heap.write(src_h);
        if let Obj::List(v) = heap.get(src_h) {
            v.replace(heap, &reordered);
        }
        self.pop(); // unroot keys
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(Value::nil())
    }

    /// `xs.min()` / `xs.max()` — the extremal element by natural order. A Comparable-struct element's
    /// `compare` re-enters the VM (may GC AND may mutate the source list), so the scan runs over a
    /// GC-rooted SNAPSHOT (mirrors [`Vm::list_sort_by_key`]/[`Vm::list_min_max_by`]) — never the live
    /// source, whose length can change under a re-entrant comparator. Returns `Option[T]` — `None` on
    /// an empty list (Rust's `Iterator::min`/`max`, matching the sibling `first`/`last`/`pop`), NOT a
    /// fault. First-seen tie-break (an equal element never displaces the earlier best), matching
    /// Python's `min`/`max`.
    pub(super) fn list_reduce_extreme(
        &mut self,
        src_h: GcRef,
        is_max: bool,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let n = match self.heap.get(src_h).as_list() {
            Some(v) => v.len(),
            None => unreachable!("list_reduce_extreme on non-list"),
        };
        if n == 0 {
            return Ok(self.alloc_enum("Option", "None", vec![]));
        }
        // A Comparable-struct element's `compare` re-enters user code, which may MUTATE (shrink) the
        // source list, so scan an immutable SNAPSHOT (mirrors `list_sort_by_key`/`list_min_max_by`) —
        // indexing the live source post-user-code would panic out of bounds. GC-rooted so its elements
        // survive `order_key`'s struct-compare allocations.
        self.push(Value::obj(src_h)); // ROOT the source list
        let snap_h = {
            let elems = match self.heap.get(src_h).as_list() {
                Some(v) => v.clone(),
                None => unreachable!(),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::obj(snap_h)); // ROOT the snapshot
        let mut best = 0usize;
        let mut err = None;
        for i in 1..n {
            let (cur, best_v) = match self.heap.get(snap_h).as_list() {
                Some(v) => (v.get(i), v.get(best)),
                None => unreachable!(),
            };
            match self.order_key(cur, best_v, span) {
                // min: strict `<` displaces; max: strict `>` displaces. Equal never displaces → first-seen.
                Ok(ord) => {
                    if (is_max && ord.is_gt()) || (!is_max && ord.is_lt()) {
                        best = i;
                    }
                }
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let result = match self.heap.get(snap_h).as_list() {
            Some(v) => v.get(best),
            None => unreachable!(),
        };
        if let Some(e) = err {
            self.pop(); // unroot snapshot
            self.pop(); // unroot source
            return Err(e);
        }
        // Wrap while the snapshot is still rooted. Belt-and-braces, NOT load-bearing today:
        // `Heap::alloc` never collects, and the only collection boundary THIS PATH can reach is
        // `run_until`'s instruction boundary — the scan's last re-entry is already over, and the
        // other collection site (`Vm::sample_mem_cap`, once per task start, `sched.rs`) runs before
        // this method call, not during it. Kept because a shrinking comparator can leave `result`
        // reachable ONLY through the snapshot, so if anything between here and the caller's
        // `push` ever re-enters the VM, this is the order that is already correct.
        let some = self.alloc_enum("Option", "Some", vec![result]);
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(some)
    }

    /// `xs.min_by(f)` / `xs.max_by(f)` — the ELEMENT whose derived key `f: fn(T) -> K` is extremal.
    /// Same GC discipline as [`Vm::list_sort_by_key`]: source, an element snapshot, AND a parallel
    /// keys list are rooted so the re-entrant extractor (and a Comparable-struct key's `compare`) can
    /// GC freely. Keys are computed once per element; a linear argmin/argmax over the rooted keys picks
    /// the extremal index (first-seen tie), then the element at that index is read from the rooted
    /// snapshot. Returns `Option[T]` — `None` on an empty list (same lineage as `min`/`max`), NOT a
    /// fault.
    pub(super) fn list_min_max_by(
        &mut self,
        src_h: GcRef,
        mut args: Vec<Value>,
        is_max: bool,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let name = if is_max { "max_by" } else { "min_by" };
        if args.len() != 1 {
            return Err(self.err(
                format!("'{name}' expects 1 argument(s), got {}", args.len()),
                span,
            ));
        }
        let f = args.swap_remove(0);
        let n = match self.heap.get(src_h).as_list() {
            Some(v) => v.len(),
            None => unreachable!("list_min_max_by on non-list"),
        };
        if n == 0 {
            return Ok(self.alloc_enum("Option", "None", vec![]));
        }
        self.push(Value::obj(src_h)); // ROOT the source list
        let snap_h = {
            let elems = match self.heap.get(src_h).as_list() {
                Some(v) => v.clone(),
                None => unreachable!(),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::obj(snap_h)); // ROOT the snapshot
        let keys_h = self.heap.alloc(Obj::List(RVec::with_capacity(n)));
        self.push(Value::obj(keys_h)); // ROOT the keys
        for i in 0..n {
            let e = match self.heap.get(snap_h).as_list() {
                Some(v) => v.get(i),
                None => unreachable!(),
            };
            match self.guarded(|vm| vm.invoke_value(f, vec![e], span)) {
                Ok(k) => {
                    let heap = self.heap.write(keys_h);
                    if let Obj::List(v) = heap.get(keys_h) {
                        v.push(heap, k);
                    }
                }
                Err(err) => {
                    self.pop(); // unroot keys
                    self.pop(); // unroot snapshot
                    self.pop(); // unroot source
                    return Err(err);
                }
            }
        }
        // Linear argmin/argmax over the rooted keys (first-seen tie-break).
        let mut best = 0usize;
        let mut err = None;
        for i in 1..n {
            let (cur, best_k) = match self.heap.get(keys_h).as_list() {
                Some(v) => (v.get(i), v.get(best)),
                None => unreachable!(),
            };
            match self.order_key(cur, best_k, span) {
                Ok(ord) => {
                    if (is_max && ord.is_gt()) || (!is_max && ord.is_lt()) {
                        best = i;
                    }
                }
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let result = match self.heap.get(snap_h).as_list() {
            Some(v) => v.get(best),
            None => unreachable!(),
        };
        if let Some(e) = err {
            self.pop(); // unroot keys
            self.pop(); // unroot snapshot
            self.pop(); // unroot source
            return Err(e);
        }
        // Wrap while the snapshot is still rooted — see `list_reduce_extreme` for why that is
        // belt-and-braces rather than load-bearing.
        let some = self.alloc_enum("Option", "Some", vec![result]);
        self.pop(); // unroot keys
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(some)
    }

    /// Stable top-down merge sort over `idx` (positions into the rooted keys list `keys_h`), ordering
    /// by each key's natural order via [`order_key`].
    pub(super) fn msort_indices_by_key(
        &mut self,
        keys_h: GcRef,
        idx: Vec<usize>,
        span: Span,
    ) -> Result<Vec<usize>, RuntimeError> {
        let n = idx.len();
        if n <= 1 {
            return Ok(idx);
        }
        let mut idx = idx;
        let right = idx.split_off(n / 2);
        let left = self.msort_indices_by_key(keys_h, idx, span)?;
        let right = self.msort_indices_by_key(keys_h, right, span)?;
        let mut out = Vec::with_capacity(n);
        let (mut li, mut ri) = (0, 0);
        while li < left.len() && ri < right.len() {
            let a = match self.heap.get(keys_h).as_list() {
                Some(v) => v.get(left[li]),
                None => unreachable!(),
            };
            let b = match self.heap.get(keys_h).as_list() {
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

    /// Natural order over two `sort_by_key` keys: a Comparable struct or enum key dispatches to its
    /// `compare`; scalar keys (int/float/str) use the built-in [`Vm::compare`]. The checker has
    /// verified the key type is orderable.
    pub(super) fn order_key(
        &mut self,
        a: Value,
        b: Value,
        span: Span,
    ) -> Result<std::cmp::Ordering, RuntimeError> {
        // TICKET-146: a same-kind tuple / List / Option pair walks lexicographically under the TOTAL
        // order. MUST precede the struct/enum gate — an `Option` is an `Obj::Enum` with no `compare`.
        if self.seq_pair(a, b) {
            return Ok(self
                .seq_order(a, b, true, span)?
                .unwrap_or(std::cmp::Ordering::Equal));
        }
        if let (Some(ha), Some(hb)) = (a.as_obj(), b.as_obj())
            && matches!(self.heap.get(ha), Obj::Struct { .. } | Obj::Enum { .. })
            && matches!(self.heap.get(hb), Obj::Struct { .. } | Obj::Enum { .. })
        {
            return self.struct_compare(a, b, span);
        }
        // Float keys order by `float_order` for the WHOLE comparison (not just the NaN case), exactly
        // mirroring `sort()`'s `value_order` Float arm — so `sort_by_key` and `sort()` agree on every
        // float pair: `-0.0`/`+0.0` are Equal (stable, first wins, like CPython) and NaN is
        // deterministic, to one end. Int keys deliberately stay on the int path below (`Int.cmp`):
        // routing them through `as_f64` would lose precision past 2^53.
        if a.is_float() && b.is_float() {
            return Ok(super::arith::float_order(
                self.float_of(a),
                self.float_of(b),
            ));
        }
        match self.compare(a, b) {
            Some(ord) => Ok(ord),
            // Numeric `None` means a NaN float — handled above for the Float/Float case; this arm
            // only catches a mixed int/float key pair (not reachable for a single key type K), kept
            // deterministic via `float_order` for safety.
            None if self.is_numeric(a) && self.is_numeric(b) => {
                Ok(super::arith::float_order(self.as_f64(a), self.as_f64(b)))
            }
            // Genuinely-incomparable types: unreachable from well-typed source; kept for safety.
            None => Err(self.err(
                format!(
                    "sort_by_key keys are not comparable: {} vs {}",
                    self.type_name(a),
                    self.type_name(b)
                ),
                span,
            )),
        }
    }

    /// Built-in methods on `str` / `list` (M6). The result is returned (not pushed) so the caller
    /// owns stack discipline. Multi-allocation paths (`split`) are safe: the GC only collects at
    /// instruction boundaries, never mid-opcode, so all `alloc`s here complete uninterrupted.
    /// Clone the elements of a `list`-typed argument for `concat`/`extend`. The checker guarantees
    /// the type; a non-list here is an internal invariant break, reported for safety.
    pub(super) fn expect_list_obj(
        &self,
        method: &str,
        arg: Value,
        span: Span,
    ) -> Result<Vec<Value>, RuntimeError> {
        match arg.as_obj().map(|ah| self.heap.get(ah)) {
            Some(Obj::List(items)) => Ok(items.to_vec()),
            _ => Err(self.err(
                format!(
                    "{method}() expects a list argument, got {}",
                    self.type_name(arg)
                ),
                span,
            )),
        }
    }

    /// Insert-or-overwrite `(hk, key, val)` into the heap map at `h` (last write wins) and return the
    /// value it replaced (`None` on a new key). Used by `map.insert`, `map.update` and the RwShared
    /// `_set_key` primitive. On INSERT it snapshots a struct/enum key (Go value-key model) so the
    /// target map does NOT alias the source map's stored key object — otherwise mutating one map's
    /// key (e.g. via `keys()`) would silently corrupt the other. `snapshot_key` is pure alloc (no
    /// GC), so `h` stays valid across it.
    pub(super) fn map_upsert_in_heap(
        &mut self,
        h: GcRef,
        hk: u64,
        key: Value,
        val: Value,
        span: Span,
    ) -> Result<Option<Value>, RuntimeError> {
        // `key`/`val` are in-flight Rust locals here (the source map's entries were cloned out), so
        // root `val` across the probe — `map_probe` roots the target map and the key itself.
        let pos = self.with_roots(&[val], |vm| vm.map_probe(h, hk, key, span))?;
        match pos {
            Some(i) => {
                let Obj::Map(m) = self.heap.get_mut(h) else {
                    unreachable!()
                };
                Ok(Some(std::mem::replace(&mut m.entries[i].2, val)))
            }
            None => {
                let key = self.snapshot_key(key);
                let Obj::Map(m) = self.heap.get_mut(h) else {
                    unreachable!()
                };
                m.push(hk, key, val);
                Ok(None)
            }
        }
    }

    /// An `int` method-argument, with a uniform type error matching the interp.
    pub(super) fn int_arg(&self, method: &str, v: &Value, span: Span) -> Result<i64, RuntimeError> {
        match self.int_val(*v) {
            Some(n) => Ok(n),
            None => Err(self.err(
                format!(
                    "{method}() expects an int argument, got {}",
                    self.type_name(*v)
                ),
                span,
            )),
        }
    }

    /// W6-3 — dispatch an **intrinsic** protocol method: one the checker grants a BUILT-IN with no
    /// user method behind it (`src/checker/proto.rs::satisfies_args_d`'s `grant_intrinsic` early-outs
    /// — `Add`/`Sub`/`Mul`/`Div`/`Mod`/`Neg` + `Comparable` on int/float, `Hashable`
    /// on int/str/bytes/bool/tuple, `Index`/`IndexSet`/`Slice` on list/map/str/bytes/
    /// bytearray). An erased `[T: Add]` body may write `a.add(b)`, so the VM must answer it.
    ///
    /// EVERY arm **delegates** to the exact primitive the operator form uses — `arith` for
    /// `add`..`mod`, `neg_value` for `neg`, `hash_value` (the Map/Set key hash) for `hash`,
    /// `compare` for `compare`, `get_index`/`set_index`/`slice_range` for the indexing trio — so
    /// `a.add(b)` ≡ `a + b` and `c.index(k)` ≡ `c[k]` by CONSTRUCTION: same value, same int/float
    /// coercion, same overflow / divide-by-zero / out-of-bounds fault text. Nothing is
    /// reimplemented here.
    ///
    /// Returns `Ok(None)` when `(method, argc)` is not an intrinsic pair, so the caller re-raises its
    /// original `has no method` error unchanged. Called ONLY from a method-resolution MISS, so a
    /// struct/enum that DEFINES `add`/`hash`/`index`/… always gets ITS method (it resolves
    /// first) and this costs nothing on any successful dispatch.
    ///
    /// `compare` on a NaN operand is the one arm that delegates somewhere ELSE than the operator: it
    /// answers the total order `sort()`/`.min()`/`.max()` use (via [`Vm::order_key`]) rather than
    /// faulting, so `compare`/`sort`/`min`/`max` share ONE order while `<`/`<=`/`>`/`>=` stay IEEE
    /// (false for every NaN comparison). See the arm below.
    ///
    /// There is currently NO unpairable grant — `INTRINSIC_UNPAIRED` in `src/checker/proto.rs` is empty
    /// (W6-3b narrowed `Iterator` so a raw collection no longer claims `next`).
    /// One `(key, value)` tuple per entry of the map `h`, in `entries` order. The one builder of a
    /// Map's iteration elements (owner U9): `items()`, the `for` snapshot (`Op::ListClone`) and
    /// `drain_iterable` (`List(m)`, `Set(m)`, `Map(m)`, `.iter()`) all call it. `Heap::alloc` never
    /// collects, so the tuples need no rooting.
    pub(super) fn map_pairs(&mut self, h: GcRef) -> Vec<Value> {
        let Obj::Map(m) = self.heap.get(h) else {
            unreachable!("map_pairs on a non-map")
        };
        let pairs: Vec<(Value, Value)> = m.entries.iter().map(|(_, k, v)| (*k, *v)).collect();
        pairs
            .into_iter()
            .map(|(k, v)| Value::obj(self.heap.alloc(Obj::Tuple(vec![k, v]))))
            .collect()
    }
}
