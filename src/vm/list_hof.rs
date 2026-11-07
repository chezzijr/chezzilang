//! VM: list higher-order methods, sorting, reductions, map pairs.
//! Split out of `src/vm/call.rs`. `super::*` is the parent module.

use super::rvec::View;
use super::*;
use crate::base::builtin_method::ListMethod;
use crate::base::hooks::Hook;

/// A list method that calls its callback once per element, in order: the loop
/// [`Vm::list_hof`] runs. `sort_by_key` / `min_by` / `max_by` are its key pass.
#[derive(Clone, Copy, Debug)]
pub(super) enum HofKind {
    Map,
    Filter,
    Fold,
    TakeWhile,
    DropWhile,
    Count,
    Position,
    SortByKey,
    MinBy,
    MaxBy,
}

impl HofKind {
    /// The loop a list method runs, if it is one. The one owner of "which list methods are HOF
    /// loops" (`do_method_call` and `core_method` both read it).
    pub(super) fn of(m: ListMethod) -> Option<HofKind> {
        match m {
            ListMethod::Map => Some(HofKind::Map),
            ListMethod::Filter => Some(HofKind::Filter),
            ListMethod::Fold => Some(HofKind::Fold),
            ListMethod::TakeWhile => Some(HofKind::TakeWhile),
            ListMethod::DropWhile => Some(HofKind::DropWhile),
            ListMethod::Count => Some(HofKind::Count),
            ListMethod::Position => Some(HofKind::Position),
            ListMethod::SortByKey => Some(HofKind::SortByKey),
            ListMethod::MinBy => Some(HofKind::MinBy),
            ListMethod::MaxBy => Some(HofKind::MaxBy),
            ListMethod::Len
            | ListMethod::Push
            | ListMethod::Pop
            | ListMethod::Reverse
            | ListMethod::Contains
            | ListMethod::IndexOf
            | ListMethod::Concat
            | ListMethod::Extend
            | ListMethod::Sort
            | ListMethod::Sum
            | ListMethod::SortBy
            | ListMethod::Min
            | ListMethod::Max
            | ListMethod::First
            | ListMethod::Last
            | ListMethod::Reversed
            | ListMethod::Repeat
            | ListMethod::Insert
            | ListMethod::Remove
            | ListMethod::Unique
            | ListMethod::Dedup
            | ListMethod::Chunk
            | ListMethod::Windows
            | ListMethod::Copy => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            HofKind::Map => "map",
            HofKind::Filter => "filter",
            HofKind::Fold => "fold",
            HofKind::TakeWhile => "take_while",
            HofKind::DropWhile => "drop_while",
            HofKind::Count => "count",
            HofKind::Position => "position",
            HofKind::SortByKey => "sort_by_key",
            HofKind::MinBy => "min_by",
            HofKind::MaxBy => "max_by",
        }
    }
}

/// The slots a HOF loop keeps on the caller's operand stack, below each callback frame:
/// `[src, snap, acc, f, i]` — the receiver, its snapshot, the accumulator (the result list, the
/// fold value, the count, the found index or the keys list), the callback and the current index.
/// The stack roots them for the GC, and a `recover:` catch truncates them with its saved length.
const HOF_SLOTS: usize = 5;

/// A list method that orders by a comparator that may be user code (`sort_by`, `sort` over
/// structs / enums / tuples / lists, `min`, `max`): the loop [`Vm::list_compare_method`] runs.
#[derive(Clone, Copy, Debug)]
pub(super) enum CmpMethod {
    SortBy,
    Sort,
    Min,
    Max,
}

impl CmpMethod {
    /// The comparator loop a list method runs, if it is one. The one owner of which list methods
    /// do (`do_method_call` and `core_method` both read it).
    #[inline]
    pub(super) fn of(m: ListMethod) -> Option<CmpMethod> {
        match m {
            ListMethod::SortBy => Some(CmpMethod::SortBy),
            ListMethod::Sort => Some(CmpMethod::Sort),
            ListMethod::Min => Some(CmpMethod::Min),
            ListMethod::Max => Some(CmpMethod::Max),
            ListMethod::Len
            | ListMethod::Push
            | ListMethod::Pop
            | ListMethod::Reverse
            | ListMethod::Contains
            | ListMethod::IndexOf
            | ListMethod::Concat
            | ListMethod::Extend
            | ListMethod::Sum
            | ListMethod::First
            | ListMethod::Last
            | ListMethod::Reversed
            | ListMethod::Repeat
            | ListMethod::Insert
            | ListMethod::Remove
            | ListMethod::Unique
            | ListMethod::Dedup
            | ListMethod::Chunk
            | ListMethod::Windows
            | ListMethod::Copy
            | ListMethod::Map
            | ListMethod::Filter
            | ListMethod::Fold
            | ListMethod::TakeWhile
            | ListMethod::DropWhile
            | ListMethod::Count
            | ListMethod::Position
            | ListMethod::SortByKey
            | ListMethod::MinBy
            | ListMethod::MaxBy => None,
        }
    }
}

/// Which sort a merge runs: where its comparator comes from.
#[derive(Clone, Copy, Debug)]
pub(super) enum SortKind {
    /// `xs.sort_by(cmp)`: the user comparator on two elements.
    By,
    /// `xs.sort_by_key(f)` after its key pass: the keys' natural order.
    ByKey,
    /// `xs.sort()` on structs / enums / tuples / lists: the elements' natural order.
    Natural,
}

impl SortKind {
    fn method(self) -> &'static str {
        match self {
            SortKind::By => "sort_by",
            SortKind::ByKey => "sort_by_key",
            SortKind::Natural => "sort",
        }
    }
}

/// The slots a merge sort keeps on the caller's operand stack, below each comparator frame:
/// `[src, snap, vals, cmp, buf, lo, hi, depth, li, ri]` — the receiver, its snapshot, the values
/// compared (the snapshot, or `sort_by_key`'s keys), the comparator (`sort_by`), the index buffer
/// (two halves of `n` indices), the merge node `lo..hi`, its depth in the tree and its two
/// cursors.
const SORT_SLOTS: usize = 10;

/// The slots a min / max scan keeps on the caller's operand stack: `[src, snap, vals, best, i]`.
const EXTREME_SLOTS: usize = 5;

/// A merge node: `lo..hi` at `depth` in the tree of `0..n`.
type Node = (usize, usize, usize);

/// The offsets of the index buffer's halves a node at `depth` merges from and into: a node writes
/// the half its parent reads (the root, depth 0, writes half 0), so no run is copied back.
fn halves(depth: usize, n: usize) -> (usize, usize) {
    if depth.is_multiple_of(2) {
        (n, 0)
    } else {
        (0, n)
    }
}

/// The first merge, in the top-down merge sort's post-order, of the node `lo..hi` at `depth`
/// (`hi - lo >= 2`): its deepest-leftmost node whose two runs need no merge of their own. A node
/// splits at `lo + len / 2`, as the recursive form split at `n / 2`.
fn first_merge((mut lo, mut hi, mut depth): Node) -> Node {
    loop {
        let mid = lo + (hi - lo) / 2;
        if mid - lo > 1 {
            hi = mid;
        } else if hi - mid > 1 {
            lo = mid;
        } else {
            return (lo, hi, depth);
        }
        depth += 1;
    }
}

/// The merge after node `lo..hi` in the post-order over the tree of `0..n`; `None` after the
/// root. A left child is followed by its right sibling's first merge (or the parent, when that
/// run is one element), a right child by its parent.
fn next_merge(n: usize, lo: usize, hi: usize) -> Option<Node> {
    if (lo, hi) == (0, n) {
        return None;
    }
    let (mut plo, mut phi, mut depth) = (0, n, 0);
    loop {
        let mid = plo + (phi - plo) / 2;
        if (plo, mid) == (lo, hi) {
            return Some(if phi - mid > 1 {
                first_merge((mid, phi, depth + 1))
            } else {
                (plo, phi, depth)
            });
        }
        if (mid, phi) == (lo, hi) {
            return Some((plo, phi, depth));
        }
        if hi <= mid {
            phi = mid;
        } else {
            plo = mid;
        }
        depth += 1;
    }
}

/// Move the left (`left`) or the right run's head into the output of node `lo..hi`, whose runs
/// are read at `src` and written at `dst` in the index buffer `ids`.
#[inline]
#[allow(clippy::too_many_arguments)]
fn merge_take(
    ids: &View<'_, Value>,
    (src, dst): (usize, usize),
    lo: usize,
    mid: usize,
    li: &mut usize,
    ri: &mut usize,
    left: bool,
) {
    let k = lo + (*li - lo) + (*ri - mid);
    let cur = if left { li } else { ri };
    ids.set(dst + k, ids.get(src + *cur));
    *cur += 1;
}

impl Vm {
    /// A list HOF (`map` / `filter` / `fold` / `take_while` / `drop_while` / `count` /
    /// `position`, and the key pass of `sort_by_key` / `min_by` / `max_by`) on the list `src_h`.
    /// Leaves the result on the stack, or (`flat`) the next callback's frame, whose return runs
    /// the loop on ([`RetAction::Hof`]), so a wait in the callback parks like any call. `flat` is
    /// true only on the dispatch loop (`do_method_call` with a real `ic`); a synchronous caller
    /// runs every callback nested.
    ///
    /// SNAPSHOT semantics: iteration walks a copy of the receiver's elements taken at call time, so
    /// a callback that MUTATES the receiver (e.g. `xs.pop()`/`xs.push(..)`) does NOT perturb the
    /// iteration sequence — it always visits exactly the elements present when the HOF was invoked.
    /// This (a) matches comprehensions/for-loops (`Op::ListClone`) and the sorts (which
    /// snapshot), (b) matches Python `map`/`filter`, and (c) is OOB-safe: indexing the original
    /// live list while a callback shrinks it would panic (regression:
    /// `map_shrinking_callback_no_panic`).
    pub(super) fn list_hof(
        &mut self,
        src_h: GcRef,
        kind: HofKind,
        mut args: Vec<Value>,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let want = if matches!(kind, HofKind::Fold) { 2 } else { 1 };
        if args.len() != want {
            return Err(self.err(
                format!(
                    "'{}' expects {want} argument(s), got {}",
                    kind.name(),
                    args.len()
                ),
                span,
            ));
        }
        let f = args.pop().expect("the callback is the last argument");
        // ROOT the source list: a method receiver is popped before dispatch, so an inline
        // temporary (`make().map(..)`) is otherwise unrooted.
        self.push(Value::obj(src_h));
        let snap = match self.heap.get(src_h).as_list() {
            Some(v) => v.clone(),
            None => unreachable!("list_hof on non-list"),
        };
        let n = snap.len();
        let snap_h = self.heap.alloc(Obj::List(snap));
        self.push(Value::obj(snap_h));
        let acc = match kind {
            HofKind::Map | HofKind::Filter | HofKind::TakeWhile | HofKind::DropWhile => {
                Value::obj(self.heap.alloc(Obj::List(RVec::new())))
            }
            HofKind::Fold => args[0],
            HofKind::Count => Value::int(0),
            HofKind::Position => Value::nil(),
            HofKind::SortByKey | HofKind::MinBy | HofKind::MaxBy => {
                Value::obj(self.heap.alloc(Obj::List(RVec::with_capacity(n))))
            }
        };
        self.push(acc);
        self.push(f);
        self.push(Value::int(0));
        self.hof_resume(kind, None, flat, span)
    }

    /// Run a HOF loop on from its stack slots: first apply `out`, the result of the callback at
    /// the current index, if there is one. On a fault the slots are dropped.
    pub(super) fn hof_resume(
        &mut self,
        kind: HofKind,
        out: Option<Value>,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let base = self.stack.len() - HOF_SLOTS;
        let r = self.hof_step(kind, base, out, flat, span);
        if r.is_err() {
            self.stack.truncate(base);
        }
        r
    }

    fn hof_step(
        &mut self,
        kind: HofKind,
        base: usize,
        out: Option<Value>,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let snap_h = self.stack[base + 1].as_obj().expect("the snapshot");
        let f = self.stack[base + 3];
        let n = match self.heap.get(snap_h).as_list() {
            Some(v) => v.len(),
            None => unreachable!(),
        };
        let mut i = self.stack[base + 4].as_int() as usize;
        if let Some(out) = out {
            if !self.hof_post(kind, base, i, out, span)? {
                return self.hof_finish(kind, base, flat, span);
            }
            i += 1;
        }
        while i < n {
            // The per-element cancel point: this loop's back-edge.
            self.guarded_checkpoint()?;
            self.stack[base + 4] = Value::int(i as i64);
            // Read from the rooted SNAPSHOT, not the live receiver.
            let elem = match self.heap.get(snap_h).as_list() {
                Some(v) => v.get(i),
                None => unreachable!(),
            };
            let pair = [self.stack[base + 2], elem];
            let call_args = if matches!(kind, HofKind::Fold) {
                &pair[..]
            } else {
                &pair[1..]
            };
            if flat && self.push_callback_frame(f, call_args, RetAction::Hof(kind), span)? {
                return Ok(());
            }
            let out = self.reentered(|vm| vm.invoke_value(f, call_args.to_vec(), span))?;
            if !self.hof_post(kind, base, i, out, span)? {
                break;
            }
            i += 1;
        }
        self.hof_finish(kind, base, flat, span)
    }

    /// Apply the callback's result `out` for element `i`. `false` stops the loop.
    fn hof_post(
        &mut self,
        kind: HofKind,
        base: usize,
        i: usize,
        out: Value,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        let acc = self.stack[base + 2];
        let snap_h = self.stack[base + 1].as_obj().expect("the snapshot");
        let elem = |vm: &Vm, i: usize| match vm.heap.get(snap_h).as_list() {
            Some(v) => v.get(i),
            None => unreachable!(),
        };
        let append = |vm: &mut Vm, v: Value| {
            let h = acc.as_obj().expect("a list accumulator");
            let heap = vm.heap.write(h);
            if let Obj::List(items) = heap.get(h) {
                items.push(heap, v);
            }
        };
        let pred = |vm: &Vm| {
            out.as_bool().ok_or_else(|| {
                vm.err(
                    format!(
                        "{} predicate must return bool, got {}",
                        kind.name(),
                        vm.type_name(out)
                    ),
                    span,
                )
            })
        };
        match kind {
            HofKind::Map | HofKind::SortByKey | HofKind::MinBy | HofKind::MaxBy => {
                append(self, out)
            }
            HofKind::Fold => self.stack[base + 2] = out,
            HofKind::Filter => {
                if pred(self)? {
                    append(self, elem(self, i));
                }
            }
            HofKind::TakeWhile => {
                if !pred(self)? {
                    return Ok(false);
                }
                append(self, elem(self, i));
            }
            HofKind::DropWhile => {
                if !pred(self)? {
                    // Collect the snapshot suffix from the first non-matching index.
                    let n = match self.heap.get(snap_h).as_list() {
                        Some(v) => v.len(),
                        None => unreachable!(),
                    };
                    for j in i..n {
                        append(self, elem(self, j));
                    }
                    return Ok(false);
                }
            }
            HofKind::Count => {
                if pred(self)? {
                    self.stack[base + 2] = Value::int(acc.as_int() + 1);
                }
            }
            HofKind::Position => {
                if pred(self)? {
                    self.stack[base + 2] = Value::int(i as i64);
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// The loop is over: replace its slots with the result (a sort or a min / max scan by the
    /// keys runs on from them first).
    fn hof_finish(
        &mut self,
        kind: HofKind,
        base: usize,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let acc = self.stack[base + 2];
        let result = match kind {
            HofKind::Map
            | HofKind::Filter
            | HofKind::TakeWhile
            | HofKind::DropWhile
            | HofKind::Fold
            | HofKind::Count => acc,
            HofKind::Position => {
                if acc.is_nil() {
                    self.alloc_enum("Option", "None", &[])
                } else {
                    self.alloc_enum("Option", "Some", &[acc])
                }
            }
            HofKind::SortByKey => return self.sort_by_keys(base, flat, span),
            HofKind::MinBy | HofKind::MaxBy => {
                return self.min_max_by_keys(base, matches!(kind, HofKind::MaxBy), flat, span);
            }
        };
        self.stack.truncate(base);
        self.push(result);
        Ok(())
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

    /// Run the comparator method `m` on the list `src_h`, leaving its result on the stack, or
    /// (`flat`) the comparator's frame. `Ok(false)` for a `sort` whose order never calls user code
    /// (primitives use `core_method`'s `value_order` sort).
    pub(super) fn list_compare_method(
        &mut self,
        src_h: GcRef,
        m: CmpMethod,
        args: &[Value],
        flat: bool,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        match m {
            CmpMethod::SortBy => {
                if args.len() != 1 {
                    return Err(self.err(
                        format!("'sort_by' expects 1 argument(s), got {}", args.len()),
                        span,
                    ));
                }
                self.sort_list(src_h, SortKind::By, args[0], flat, span)?;
            }
            CmpMethod::Sort => {
                self.arity_err("sort", args, 0, span)?;
                if !self.compare_reenters(src_h) {
                    return Ok(false);
                }
                self.sort_list(src_h, SortKind::Natural, Value::nil(), flat, span)?;
            }
            CmpMethod::Min | CmpMethod::Max => {
                let is_max = matches!(m, CmpMethod::Max);
                self.arity_err(if is_max { "max" } else { "min" }, args, 0, span)?;
                self.list_reduce_extreme(src_h, is_max, flat, span)?;
            }
        }
        Ok(true)
    }

    /// Does ordering the list `h` call user code? Its elements are structs or enums (a `compare`
    /// hook) or tuples / lists (the TICKET-146 walk, which may reach one; an `Option` is an
    /// `Obj::Enum`). The list is homogeneous, so the first element answers.
    fn compare_reenters(&self, h: GcRef) -> bool {
        let first = match self.heap.get(h).as_list() {
            Some(v) => v.try_get(0),
            None => unreachable!("compare_reenters on a non-list"),
        };
        matches!(first.and_then(|v| v.as_obj()), Some(hh) if matches!(self.heap.get(hh), Obj::Struct { .. } | Obj::Enum { .. } | Obj::Tuple(_) | Obj::List(_)))
    }

    /// Sort the list `src_h` in place by `kind`'s comparator (`cmp` for `sort_by`), stable. The
    /// merge reads a SNAPSHOT taken now, so a comparator that mutates the list cannot perturb it
    /// (and [`Vm::sort_mutation_check`] reports the mutation). Leaves `nil` on the stack, or
    /// (`flat`) the comparator's frame, whose return runs the merge on ([`RetAction::Sort`]).
    fn sort_list(
        &mut self,
        src_h: GcRef,
        kind: SortKind,
        cmp: Value,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        // ROOT the source list: a method receiver is popped before dispatch, so an inline
        // temporary (`make().sort_by(..)`) is otherwise unrooted.
        self.push(Value::obj(src_h));
        let snap = match self.heap.get(src_h).as_list() {
            Some(v) => v.clone(),
            None => unreachable!("sort on a non-list"),
        };
        let snap_h = self.heap.alloc(Obj::List(snap));
        self.push(Value::obj(snap_h));
        // The values compared are the elements themselves.
        self.push(Value::obj(snap_h));
        self.push(cmp);
        self.sort_start(kind, flat, span)
    }

    /// Start the merge over the four slots `[src, snap, vals, cmp]` on the stack top: push the
    /// index buffer and the first merge node, then run it.
    fn sort_start(&mut self, kind: SortKind, flat: bool, span: Span) -> Result<(), RuntimeError> {
        let snap_h = self.stack[self.stack.len() - 3]
            .as_obj()
            .expect("the snapshot");
        let n = match self.heap.get(snap_h).as_list() {
            Some(v) => v.len(),
            None => unreachable!(),
        };
        // Both halves start as the identity: every one-element run is already in place.
        let ids: Vec<Value> = (0..n).chain(0..n).map(|i| Value::int(i as i64)).collect();
        let buf_h = self.heap.alloc(Obj::List(ids.into()));
        self.push(Value::obj(buf_h));
        let (lo, hi, depth) = if n > 1 {
            first_merge((0, n, 0))
        } else {
            (0, 0, 0)
        };
        for k in [lo, hi, depth, lo, lo + (hi - lo) / 2] {
            self.push(Value::int(k as i64));
        }
        self.sort_resume(kind, None, flat, span)
    }

    /// Run a merge on from its stack slots: first apply `out`, the comparator's result for the
    /// current pair, if there is one. On a fault the slots are dropped.
    pub(super) fn sort_resume(
        &mut self,
        kind: SortKind,
        out: Option<Value>,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let base = self.stack.len() - SORT_SLOTS;
        let r = self.sort_step(kind, base, out, flat, span);
        if r.is_err() {
            self.stack.truncate(base);
        }
        r
    }

    /// The top-down merge sort's merges, in its post-order (the comparator sees the same pairs in
    /// the same order as the recursive form), each node `lo..hi` merging its runs `lo..mid` and
    /// `mid..hi` from one half of the index buffer into the other ([`halves`]).
    fn sort_step(
        &mut self,
        kind: SortKind,
        base: usize,
        out: Option<Value>,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let slot = |vm: &Vm, k: usize| vm.stack[base + k];
        let vals_h = slot(self, 2).as_obj().expect("a rooted list");
        let buf_h = slot(self, 4).as_obj().expect("a rooted list");
        let cmp = slot(self, 3);
        let mut lo = slot(self, 5).as_int() as usize;
        let mut hi = slot(self, 6).as_int() as usize;
        let mut depth = slot(self, 7).as_int() as usize;
        let mut li = slot(self, 8).as_int() as usize;
        let mut ri = slot(self, 9).as_int() as usize;
        // The verdict on the current pair, once its comparator has answered.
        let mut left = match out {
            Some(out) => Some(self.sort_verdict(kind, out, span)?),
            None => None,
        };
        loop {
            // Up to the next pair to compare, with one view of the buffer: no user code runs in
            // between (a view lives until the next safepoint).
            let pair = {
                let ids = self.list_view(buf_h);
                let n = ids.len() / 2;
                if let Some(left) = left.take() {
                    let mid = lo + (hi - lo) / 2;
                    merge_take(&ids, halves(depth, n), lo, mid, &mut li, &mut ri, left);
                }
                loop {
                    if hi - lo < 2 {
                        break None;
                    }
                    let mid = lo + (hi - lo) / 2;
                    let (src, dst) = halves(depth, n);
                    if li < mid && ri < hi {
                        let vals = self.list_view(vals_h);
                        let at = |i: usize| vals.get(ids.get(src + i).as_int() as usize);
                        break Some((at(li), at(ri)));
                    }
                    // One run is spent: append the other's tail, then move on.
                    let k = lo + (li - lo) + (ri - mid);
                    for j in li..mid {
                        ids.set(dst + k + (j - li), ids.get(src + j));
                    }
                    let k = k + (mid - li);
                    for j in ri..hi {
                        ids.set(dst + k + (j - ri), ids.get(src + j));
                    }
                    (lo, hi, depth) = next_merge(n, lo, hi).unwrap_or((0, 0, 0));
                    li = lo;
                    ri = lo + (hi - lo) / 2;
                }
            };
            let Some((a, b)) = pair else {
                return self.sort_finish(kind, base, span);
            };
            left = Some(match kind {
                SortKind::By => {
                    // The per-comparison cancel point: this loop's back-edge.
                    self.guarded_checkpoint()?;
                    if flat {
                        self.sort_save(base, [lo, hi, depth, li, ri]);
                        if self.push_callback_frame(cmp, &[a, b], RetAction::Sort(kind), span)? {
                            return Ok(());
                        }
                    }
                    let out = self.reentered(|vm| vm.invoke_value(cmp, vec![a, b], span))?;
                    self.sort_verdict(kind, out, span)?
                }
                SortKind::ByKey | SortKind::Natural => {
                    if flat && self.compare_hook_pair(a, b) {
                        self.sort_save(base, [lo, hi, depth, li, ri]);
                        if self.push_hook_frame(
                            Hook::Compare,
                            a,
                            &[b],
                            RetAction::Sort(kind),
                            span,
                        )? {
                            return Ok(());
                        }
                    }
                    // `<= Equal` keeps the left element first on ties → stable.
                    self.order_key(a, b, span)?.is_le()
                }
            });
        }
    }

    /// One snapshot of the rooted list `h`; drop it before the next safepoint.
    #[inline]
    fn list_view(&self, h: GcRef) -> View<'_, Value> {
        match self.heap.get(h).as_list() {
            Some(v) => v.view(),
            None => unreachable!("a rooted list"),
        }
    }

    /// The comparator's result as "the left element goes first". `<= 0` keeps the left element
    /// first on ties → stable.
    #[inline]
    fn sort_verdict(&self, kind: SortKind, out: Value, span: Span) -> Result<bool, RuntimeError> {
        match kind {
            SortKind::By => match self.int_val(out) {
                Some(n) => Ok(n <= 0),
                None => Err(self.err(
                    format!(
                        "sort_by comparator must return int, got {}",
                        self.type_name(out)
                    ),
                    span,
                )),
            },
            SortKind::ByKey | SortKind::Natural => Ok(self.hook_ord(out, span)?.is_le()),
        }
    }

    /// Store the merge node and its cursors in their slots before a comparator frame.
    #[inline]
    fn sort_save(&mut self, base: usize, at: [usize; 5]) {
        for (k, v) in at.into_iter().enumerate() {
            self.stack[base + 5 + k] = Value::int(v as i64);
        }
    }

    /// The merge is over: write the permutation (the buffer's half 0, the root's output) back
    /// and replace the slots with `nil`.
    fn sort_finish(&mut self, kind: SortKind, base: usize, span: Span) -> Result<(), RuntimeError> {
        let [src_h, snap_h, buf_h] =
            [0, 1, 4].map(|k| self.stack[base + k].as_obj().expect("a rooted list"));
        let order: Vec<usize> = match self.heap.get(buf_h).as_list() {
            Some(ids) => (0..ids.len() / 2)
                .map(|i| ids.get(i).as_int() as usize)
                .collect(),
            None => unreachable!(),
        };
        self.sort_commit(src_h, snap_h, &order, kind.method(), span)?;
        self.stack.truncate(base);
        self.push(Value::nil());
        Ok(())
    }

    /// Write the snapshot `snap_h`, permuted by `order`, back into `src_h`, unless a callback
    /// mutated the list (W8-4). No comparator call remains, so nothing collects in between.
    fn sort_commit(
        &mut self,
        src_h: GcRef,
        snap_h: GcRef,
        order: &[usize],
        method: &str,
        span: Span,
    ) -> Result<(), RuntimeError> {
        self.sort_mutation_check(src_h, snap_h, method, span)?;
        let reordered: Vec<Value> = match self.heap.get(snap_h).as_list() {
            Some(v) => order.iter().map(|&i| v.get(i)).collect(),
            None => unreachable!(),
        };
        let heap = self.heap.write(src_h);
        if let Obj::List(v) = heap.get(src_h) {
            v.replace(heap, &reordered);
        }
        Ok(())
    }

    /// `xs.sort_by_key(f)` once its key pass ([`Vm::list_hof`]) is done: a stable in-place sort
    /// by the keys, from the loop's slots `[src, snap, keys, f, i]` at `base`. Struct / enum /
    /// tuple / list keys run the merge ([`Vm::sort_start`]), each struct key's `compare` a frame
    /// when `flat`; scalar keys never re-enter, so Rust's stable sort orders them.
    fn sort_by_keys(&mut self, base: usize, flat: bool, span: Span) -> Result<(), RuntimeError> {
        let [src_h, snap_h, keys_h] =
            [0, 1, 2].map(|k| self.stack[base + k].as_obj().expect("a rooted list"));
        if self.compare_reenters(keys_h) {
            self.stack.truncate(base + 4);
            return self.sort_start(SortKind::ByKey, flat, span);
        }
        let keys = match self.heap.get(keys_h).as_list() {
            Some(v) => v.to_vec(),
            None => unreachable!(),
        };
        let mut order: Vec<usize> = (0..keys.len()).collect();
        let mut err = None;
        order.sort_by(|&a, &b| match self.order_key(keys[a], keys[b], span) {
            Ok(ord) => ord,
            Err(e) => {
                err.get_or_insert(e);
                std::cmp::Ordering::Equal
            }
        });
        if let Some(e) = err {
            return Err(e);
        }
        self.sort_commit(src_h, snap_h, &order, "sort_by_key", span)?;
        self.stack.truncate(base);
        self.push(Value::nil());
        Ok(())
    }

    /// `xs.min()` / `xs.max()` — the extremal element by natural order, over a rooted SNAPSHOT (a
    /// struct element's `compare` may mutate, even shrink, the source list). Leaves `Option[T]` —
    /// `None` on an empty list (Rust's `Iterator::min`/`max`, matching the sibling
    /// `first`/`last`/`pop`), NOT a fault — or (`flat`) a `compare` frame ([`Vm::extreme_resume`]).
    /// First-seen tie-break (an equal element never displaces the earlier best), matching
    /// Python's `min`/`max`.
    fn list_reduce_extreme(
        &mut self,
        src_h: GcRef,
        is_max: bool,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        self.push(Value::obj(src_h)); // ROOT the source list
        let snap = match self.heap.get(src_h).as_list() {
            Some(v) => v.clone(),
            None => unreachable!("list_reduce_extreme on non-list"),
        };
        let snap_h = self.heap.alloc(Obj::List(snap));
        self.push(Value::obj(snap_h));
        // The values compared are the elements themselves.
        self.push(Value::obj(snap_h));
        self.push(Value::int(0));
        self.push(Value::int(1));
        self.extreme_resume(is_max, None, flat, span)
    }

    /// `xs.min_by(f)` / `xs.max_by(f)` once its key pass ([`Vm::list_hof`]) is done: the ELEMENT
    /// whose key is extremal (first-seen tie), scanned from the loop's slots at `base`, whose `f`
    /// and `i` become the scan's `best` and `i`. `None` on an empty list.
    fn min_max_by_keys(
        &mut self,
        base: usize,
        is_max: bool,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        self.stack[base + 3] = Value::int(0);
        self.stack[base + 4] = Value::int(1);
        self.extreme_resume(is_max, None, flat, span)
    }

    /// Run a min / max scan on from its slots `[src, snap, vals, best, i]`: first apply `out`, the
    /// `compare` result of `vals[i]` against `vals[best]`, if there is one. On a fault the slots
    /// are dropped.
    pub(super) fn extreme_resume(
        &mut self,
        is_max: bool,
        out: Option<Value>,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let base = self.stack.len() - EXTREME_SLOTS;
        let r = self.extreme_step(is_max, base, out, flat, span);
        if r.is_err() {
            self.stack.truncate(base);
        }
        r
    }

    fn extreme_step(
        &mut self,
        is_max: bool,
        base: usize,
        out: Option<Value>,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let [snap_h, vals_h] =
            [1, 2].map(|k| self.stack[base + k].as_obj().expect("a rooted list"));
        let n = match self.heap.get(snap_h).as_list() {
            Some(v) => v.len(),
            None => unreachable!(),
        };
        if n == 0 {
            let none = self.alloc_enum("Option", "None", &[]);
            self.stack.truncate(base);
            self.push(none);
            return Ok(());
        }
        // min: strict `<` displaces; max: strict `>` displaces. Equal never displaces → first-seen.
        let displaces = |ord: std::cmp::Ordering| if is_max { ord.is_gt() } else { ord.is_lt() };
        let [mut best, mut i] = [3, 4].map(|k| self.stack[base + k].as_int() as usize);
        if let Some(out) = out {
            if displaces(self.hook_ord(out, span)?) {
                best = i;
            }
            i += 1;
        }
        while i < n {
            let (cur, best_v) = match self.heap.get(vals_h).as_list() {
                Some(v) => (v.get(i), v.get(best)),
                None => unreachable!(),
            };
            if flat && self.compare_hook_pair(cur, best_v) {
                self.stack[base + 3] = Value::int(best as i64);
                self.stack[base + 4] = Value::int(i as i64);
                let ret = RetAction::MinMax { max: is_max };
                if self.push_hook_frame(Hook::Compare, cur, &[best_v], ret, span)? {
                    return Ok(());
                }
            }
            if displaces(self.order_key(cur, best_v, span)?) {
                best = i;
            }
            i += 1;
        }
        let result = match self.heap.get(snap_h).as_list() {
            Some(v) => v.get(best),
            None => unreachable!(),
        };
        // Wrap while the snapshot is still rooted: a shrinking comparator can leave `result`
        // reachable only through it.
        let some = self.alloc_enum("Option", "Some", &[result]);
        self.stack.truncate(base);
        self.push(some);
        Ok(())
    }

    /// Do two values order through a `compare` hook (both structs or enums)? Such a pair is never
    /// a TICKET-146 sequence pair. [`Vm::order_key`] reads it, and a flat sort or scan pushes the
    /// hook's frame for it.
    fn compare_hook_pair(&self, a: Value, b: Value) -> bool {
        let hooked = |v: Value| {
            v.as_obj()
                .is_some_and(|h| matches!(self.heap.get(h), Obj::Struct { .. } | Obj::Enum { .. }))
        };
        hooked(a) && hooked(b)
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
        if self.compare_hook_pair(a, b) {
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
        if let Some(i) = pos {
            let Obj::Map(m) = self.heap.write(h).get(h) else {
                unreachable!()
            };
            // M3.0a — `None`: another thread removed the entry after the probe, an insert.
            if let Some(old) = m.set_value(i, val) {
                return Ok(Some(old));
            }
        }
        let key = self.snapshot_key(key);
        let heap = self.heap.write(h);
        let Obj::Map(m) = heap.get(h) else {
            unreachable!()
        };
        m.push(heap, hk, key, val);
        Ok(None)
    }

    /// `xs.repeat(n)` — repeat the list `n` times into a fresh list. `n <= 0` → empty.
    /// Sizes the whole block once with `RVec::try_with_capacity`: a total that overflows or a
    /// block that cannot be allocated raises a RECOVERABLE fault (as `str.repeat` does), not a
    /// panic or an abort.
    pub(super) fn list_repeat(
        &mut self,
        h: GcRef,
        n: i64,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let Obj::List(items) = self.heap.get(h) else {
            unreachable!("list_repeat receiver is a list")
        };
        if n <= 0 {
            return Ok(Value::obj(self.heap.alloc(Obj::List(RVec::new()))));
        }
        let n = n as usize;
        // An empty source repeats to `[]` in constant time for any `n`: `total` below is 0
        // regardless of `n`, so without this the loop at the bottom of this fn would still
        // iterate `n` times extending nothing (W13-10). CPython's `len([] * (2**63-1))` is `0`
        // instantly.
        if items.is_empty() {
            return Ok(Value::obj(self.heap.alloc(Obj::List(RVec::new()))));
        }
        // Guard the allocation: a total that overflows, or a block that cannot be allocated
        // (`RVec::try_with_capacity`: past `isize::MAX` bytes, or a null from the allocator), is
        // a recoverable fault, not an abort.
        let src = items.to_vec();
        let out = match src
            .len()
            .checked_mul(n)
            .map(RVec::<Value>::try_with_capacity)
        {
            Some(Ok(out)) => out,
            Some(Err(AllocFailed)) | None => {
                return Err(self.err("list repeat capacity overflow".to_string(), span));
            }
        };
        // The block holds every copy, so no extend below grows it.
        for _ in 0..n {
            out.extend_from_slice(&self.heap, &src);
        }
        Ok(Value::obj(self.heap.alloc(Obj::List(out))))
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
        let pairs: Vec<(Value, Value)> = m.entries.iter().map(|(_, k, v)| (k, v)).collect();
        pairs
            .into_iter()
            .map(|(k, v)| Value::obj(self.heap.alloc(Obj::Tuple(vec![k, v]))))
            .collect()
    }
}
