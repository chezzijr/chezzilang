//! VM: the structural `==` walk on an explicit stack: its levels sit in operand-stack slots, so an
//! element `eq` hook runs as a frame that can park ([`RetAction::EqWalk`]), and the list scans
//! (`in`, `contains`, `index_of`, `unique`, `dedup`) that compare through it.
//! `super::*` is the parent module.

use super::arith::cmp_int_f64;
use super::*;
use crate::base::builtin_method::ListMethod;
use crate::base::hooks::Hook;

/// One container level of a walk: `[tag, a, b, i, j]`. `tag` packs the [`Shape`] (low 2 bits) and
/// the child count; `a` / `b` are snapshot lists of the two containers' children; `i` / `j` the
/// cursors. A walk that meets no hooked pair pushes no level ([`Vm::eq_tight`]).
const LEVEL: usize = 5;

/// What a parked walk keeps above its levels, under the hook frame: `[depth, walk_base, levels]`,
/// the walk's start depth, the `walk_base` to restore and the level count.
const HEADER: usize = 3;

/// A list scan's slots: `[hay, x, i, j]`, the list (or its snapshot), the needle or the result
/// list, and the cursors.
const SCAN: usize = 4;

/// How a level pairs the children of its two containers.
#[derive(Clone, Copy)]
enum Shape {
    /// child `i` of `a` with child `i` of `b` (list, tuple, struct fields, enum payload)
    Seq = 0,
    /// every entry `i` of `a` with some entry `j` of `b`: key, then value (child `2e` is entry
    /// `e`'s key, `2e + 1` its value); `j`'s low bit is the value phase
    Map = 1,
    /// every element `i` of `a` with some element `j` of `b`
    Set = 2,
}

/// One pair's verdict before any child is compared.
enum Pair {
    Done(bool),
    /// one declared type with an `eq` hook: the hook decides
    Hook,
    /// two containers of one shape and `n` children each
    Nest(Shape, usize),
}

/// What a level's cursors ask for next.
enum Next {
    /// the level is decided: it is popped and this is its answer
    Pop(bool),
    /// compare this pair (an element compare: identity first)
    Pair(Value, Value),
}

/// What a walk's answer feeds once its parked hook frame returns.
#[derive(Clone, Copy, Debug)]
pub(super) enum EqThen {
    /// the `==` / `!=` operator: push the `bool`, flipped for `!=`
    Op { negate: bool },
    /// the `eq` intrinsic method (`x.eq(y)`): push the `bool`
    Push,
    /// a list scan's element compare: the scan runs on from its slots ([`Vm::scan_step`])
    Scan(Scan),
}

/// A list scan that compares elements by `==`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Scan {
    /// `x in xs`, `xs.contains(x)`
    Has,
    IndexOf,
    Unique,
    Dedup,
}

impl Scan {
    /// The scan a list method runs, if it is one. The one owner of which list methods are scans
    /// (`do_method_call` and `core_method` both read it).
    pub(super) fn of(m: ListMethod) -> Option<Scan> {
        match m {
            ListMethod::Contains => Some(Scan::Has),
            ListMethod::IndexOf => Some(Scan::IndexOf),
            ListMethod::Unique => Some(Scan::Unique),
            ListMethod::Dedup => Some(Scan::Dedup),
            ListMethod::Len
            | ListMethod::Push
            | ListMethod::Pop
            | ListMethod::Reverse
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

    fn name(self) -> &'static str {
        match self {
            Scan::Has => "contains",
            Scan::IndexOf => "index_of",
            Scan::Unique => "unique",
            Scan::Dedup => "dedup",
        }
    }
}

impl EqThen {
    /// The slots its continuation keeps below the walk.
    fn slots(self) -> usize {
        match self {
            EqThen::Op { .. } | EqThen::Push => 0,
            EqThen::Scan(_) => SCAN,
        }
    }
}

impl Vm {
    /// Depth-guarded structural `==` of `l` and `r` at `depth`, the walk's one entry. `then`:
    /// `None` runs a hooked pair's `eq` nested ([`Vm::guarded_walk`]) and always answers; `Some`
    /// (the handler runs on the dispatch loop) pushes the hook's frame and answers `None`: its
    /// return resumes the walk and feeds `then` ([`Vm::eq_resume`]).
    ///
    /// The `Eq` protocol (M23): two values of one declared type with an `eq` hook dispatch to it,
    /// BEFORE the `ha == hb` identity short-circuit (CPython's `do_richcompare` has none either).
    /// A container's elements get the identity shortcut ([`Vm::elem_equal`], CPython's
    /// `PyObject_RichCompareBool`). `MAX_STRUCTURAL_DEPTH` is tested against `walk_base + depth`
    /// at every pair, so a hook that compares continues on the SAME budget ([`Vm::walk_base`],
    /// W8-43).
    pub(super) fn eq_run(
        &mut self,
        l: Value,
        r: Value,
        depth: usize,
        then: Option<EqThen>,
        span: Span,
    ) -> Result<Option<bool>, RuntimeError> {
        match self.eq_pair(l, r, depth, span)? {
            Pair::Done(eq) => Ok(Some(eq)),
            pair @ (Pair::Hook | Pair::Nest(..)) => self.eq_nested(pair, l, r, depth, then, span),
        }
    }

    /// [`Vm::eq_run`] for an element pair: identity first ([`Vm::elem_equal`]).
    #[inline]
    pub(super) fn elem_run(
        &mut self,
        l: Value,
        r: Value,
        depth: usize,
        then: Option<EqThen>,
        span: Span,
    ) -> Result<Option<bool>, RuntimeError> {
        if same_elem(l, r) {
            return Ok(Some(true));
        }
        self.eq_run(l, r, depth, then, span)
    }

    /// The verdict on one pair that needs no child compare, or what it needs.
    #[inline(always)]
    fn eq_pair(&self, l: Value, r: Value, depth: usize, span: Span) -> Result<Pair, RuntimeError> {
        if self.walk_base + depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.depth_exceeded_err(span));
        }
        // Exact i64 equality when BOTH operands are integral (inline `Int` OR boxed `BigInt`) —
        // Python parity. MUST precede the mixed arm below: the both-integral arm has already
        // returned there, so at most one side of a mixed numeric pair is integral, and the other
        // side is always a boxed float. The canonical-rep invariant (inline XOR boxed) makes this
        // correct across kinds: an inline int and a boxed big-int are never equal.
        if self.is_integral(l) && self.is_integral(r) {
            return Ok(Pair::Done(self.int_of(l) == self.int_of(r)));
        }
        // Mixed int/float (`1 == 1.0` above 2^53) compares exactly via `cmp_int_f64`, never `as_f64`.
        if self.is_integral(l) && self.is_numeric(r) {
            return Ok(Pair::Done(
                cmp_int_f64(self.int_of(l), self.float_of(r)) == Some(std::cmp::Ordering::Equal),
            ));
        }
        if self.is_integral(r) && self.is_numeric(l) {
            return Ok(Pair::Done(
                cmp_int_f64(self.int_of(r), self.float_of(l)) == Some(std::cmp::Ordering::Equal),
            ));
        }
        // float==float compares via f64 (so `nan == nan` stays `false`).
        if self.is_numeric(l) && self.is_numeric(r) {
            return Ok(Pair::Done(self.float_of(l) == self.float_of(r)));
        }
        let (ha, hb) = match (l.view(), r.view()) {
            (ValueView::Bool(a), ValueView::Bool(b)) => return Ok(Pair::Done(a == b)),
            (ValueView::Nil, ValueView::Nil) => return Ok(Pair::Done(true)),
            (ValueView::Obj(ha), ValueView::Obj(hb)) => (ha, hb),
            _ => return Ok(Pair::Done(false)),
        };
        // `eq_hook_off` is `Atomic.cas`'s window (see the field's doc): a compare under the box's
        // value lock must stay structural, whatever the checker did or did not see.
        if !self.eq_hook_off && self.eq_hook_applies(l, r) {
            return Ok(Pair::Hook);
        }
        if ha == hb {
            return Ok(Pair::Done(true));
        }
        let nest = |shape, n: usize, m: usize| match (n == m, n) {
            (false, _) => Pair::Done(false),
            (true, 0) => Pair::Done(true),
            (true, n) => Pair::Nest(shape, n),
        };
        Ok(match (self.heap.get(ha), self.heap.get(hb)) {
            (Obj::Str(a), Obj::Str(b)) => Pair::Done(a == b),
            (Obj::Bytes(a), Obj::Bytes(b)) => Pair::Done(a == b),
            // `bytearray` equality is structural byte-equality. Cross-type `bytes ==
            // bytearray` is content-equal (Python parity: `b"a" == bytearray(b"a")` is true).
            (Obj::ByteArray(a), Obj::ByteArray(b)) => Pair::Done(a.iter().eq(b.iter())),
            (Obj::Bytes(a), Obj::ByteArray(b)) => Pair::Done(a.iter().copied().eq(b.iter())),
            (Obj::ByteArray(a), Obj::Bytes(b)) => Pair::Done(a.iter().eq(b.iter().copied())),
            (Obj::List(a), Obj::List(b)) => nest(Shape::Seq, a.len(), b.len()),
            (Obj::Tuple(a), Obj::Tuple(b)) => nest(Shape::Seq, a.len(), b.len()),
            // Maps are unordered: equal iff same size and every (key, value) entry of `a` has a
            // structurally-equal match in `b` (the cached hash is unused).
            (Obj::Map(a), Obj::Map(b)) => nest(Shape::Map, a.entries.len(), b.entries.len()),
            // Sets are unordered: equal iff same size and every element of `a` is in `b`.
            (Obj::Set(a), Obj::Set(b)) => nest(Shape::Set, a.entries.len(), b.entries.len()),
            // Positional structural compare: the `ta != tb` guard preserves type distinction (same
            // tid ⇒ same StructDef ⇒ identical field order), so a by-position value compare
            // suffices. Equal tid ⟹ same struct type (tids are dense per-type ids).
            (
                Obj::Struct {
                    tid: ta,
                    fields: fa,
                },
                Obj::Struct {
                    tid: tb,
                    fields: fb,
                },
            ) => match ta == tb {
                true => nest(Shape::Seq, fa.len(), fb.len()),
                false => Pair::Done(false),
            },
            // M19 lever #2 — equal `variant_id` ⟹ same enum type AND variant (ids are globally
            // unique per (enum, variant) pair).
            (
                Obj::Enum {
                    variant_id: va,
                    payload: pa,
                },
                Obj::Enum {
                    variant_id: vb,
                    payload: pb,
                },
            ) => match va == vb {
                true => nest(Shape::Seq, pa.len(), pb.len()),
                false => Pair::Done(false),
            },
            // Two first-class builtin-fn values are equal iff they name the SAME builtin. Each
            // value-position use emits a fresh `Op::LoadBuiltin` → a distinct handle, so the
            // `ha == hb` identity short-circuit above never fires; compare by name instead.
            (Obj::Builtin(a), Obj::Builtin(b)) => Pair::Done(a == b),
            // Boxed scalars compare by value, identically to the inline `Int`/`Float` arms.
            (Obj::BigInt(a), Obj::BigInt(b)) => Pair::Done(a == b),
            (Obj::FloatBox(a), Obj::FloatBox(b)) => Pair::Done(a == b),
            // TICKET-177: a handle (or a `ptr`) is equal iff it names the same core, its
            // `Obj::identity` key. Two slots can wrap one core, so the `ha == hb` shortcut above
            // is only the fast path. Anything else reaching here is a kind mismatch or a `Slot`
            // object in another slot: unequal.
            (a, b) => Pair::Done(matches!(
                (a.identity(), b.identity()),
                (Identity::Core(x), Identity::Core(y)) if x == y
            )),
        })
    }

    /// The walk past its first pair: `pair` is a hook or a container pair. Out of line, so the
    /// scalar / str answer of [`Vm::eq_run`] stays a tight inline path (a map probe's compare).
    #[inline(never)]
    fn eq_nested(
        &mut self,
        pair: Pair,
        l: Value,
        r: Value,
        depth: usize,
        then: Option<EqThen>,
        span: Span,
    ) -> Result<Option<bool>, RuntimeError> {
        if let Pair::Nest(shape, _) = pair
            && let Some(eq) = self.eq_tight(shape, l.as_gcref(), r.as_gcref(), depth, span)?
        {
            return Ok(Some(eq));
        }
        let base = self.stack.len();
        let hook = match pair {
            Pair::Done(eq) => return Ok(Some(eq)),
            Pair::Hook => Some((l, r)),
            Pair::Nest(shape, n) => {
                self.push_level(shape, n, l, r);
                None
            }
        };
        let out = self.eq_levels(base, depth, hook, None, then, span);
        if out.is_err() {
            self.stack.truncate(base);
        }
        out
    }

    /// Run the walk whose levels start at `base`: first the hooked pair `hook`, or the answer
    /// `ans` to the top level's current pair. `Ok(None)`: a hook frame was pushed (`then` is
    /// `Some`).
    fn eq_levels(
        &mut self,
        base: usize,
        depth: usize,
        mut hook: Option<(Value, Value)>,
        mut ans: Option<bool>,
        then: Option<EqThen>,
        span: Span,
    ) -> Result<Option<bool>, RuntimeError> {
        loop {
            if let Some((x, y)) = hook.take() {
                let d = depth + (self.stack.len() - base) / LEVEL;
                ans = Some(match then {
                    Some(then) => match self.walk_park(
                        base,
                        LEVEL,
                        depth,
                        Hook::Eq,
                        [x, y],
                        d,
                        RetAction::EqWalk(then),
                        span,
                    )? {
                        true => return Ok(None),
                        // No hook body after all: the nested call's answer, `None` → its fault.
                        false => self.hook_bool(Value::nil(), Hook::Eq, span)?,
                    },
                    None => {
                        // The hook re-enters the VM and may start its own structural walk, so
                        // hand it the depth this walk has already consumed — one shared budget.
                        let at = self.walk_base + d;
                        let res = self
                            .guarded_walk(at, |vm| vm.invoke_hook(Hook::Eq, x, &[y], span))?
                            .unwrap_or(Value::nil());
                        self.hook_bool(res, Hook::Eq, span)?
                    }
                });
            }
            if self.stack.len() == base {
                return Ok(Some(ans.expect("a walk with no level has its answer")));
            }
            match self.level_step(ans.take()) {
                Next::Pop(eq) => {
                    self.stack.truncate(self.stack.len() - LEVEL);
                    ans = Some(eq);
                }
                Next::Pair(x, y) => {
                    if same_elem(x, y) {
                        ans = Some(true);
                        continue;
                    }
                    let d = depth + (self.stack.len() - base) / LEVEL;
                    match self.eq_pair(x, y, d, span)? {
                        Pair::Done(eq) => ans = Some(eq),
                        Pair::Hook => hook = Some((x, y)),
                        Pair::Nest(shape, n) => self.push_level(shape, n, x, y),
                    }
                }
            }
        }
    }

    /// Push a level for the containers `a` and `b` (`n` children each, `n` entries for a map).
    /// Each side becomes a snapshot list of its children: an `eq` hook can mutate the containers,
    /// and the snapshot keeps the children it has still to compare alive (a hook that empties a
    /// source would otherwise orphan them). The containers sit in their slots while the
    /// snapshots allocate.
    fn push_level(&mut self, shape: Shape, n: usize, a: Value, b: Value) {
        let at = self.stack.len();
        self.stack
            .extend([Value::int(((n << 2) | shape as usize) as i64), a, b]);
        self.stack.extend([Value::int(0), Value::int(0)]);
        for k in [1, 2] {
            let kids = self.walk_children(self.stack[at + k].as_gcref());
            let snap = self.heap.alloc(Obj::List(kids.into()));
            self.stack[at + k] = Value::obj(snap);
        }
    }

    /// The walk of a container pair up to its first hooked pair: no user code runs, so nothing
    /// can park, collect or mutate, and the walk recurses on the host stack over each level's
    /// children (`MAX_STRUCTURAL_DEPTH` bounds it, as at every pair). `Ok(None)` at a hooked pair:
    /// the caller walks again on levels ([`Vm::eq_levels`]), which meets the same pairs in the
    /// same order up to that one. The pair rules are [`Vm::eq_pair`]'s.
    fn eq_tight(
        &self,
        shape: Shape,
        a: GcRef,
        b: GcRef,
        depth: usize,
        span: Span,
    ) -> Result<Option<bool>, RuntimeError> {
        let (ka, kb) = (self.walk_children(a), self.walk_children(b));
        let pair = |x: Value, y: Value| self.elem_tight(x, y, depth + 1, span);
        // A map entry is two children (its key, then its value), a set's or a sequence's one.
        let k = match shape {
            Shape::Map => 2,
            Shape::Seq | Shape::Set => 1,
        };
        for (n, ea) in ka.chunks(k).enumerate() {
            // A sequence pairs child `n` with child `n`; a map or set searches every entry of `b`.
            let cands = match shape {
                Shape::Seq => &kb[n..(n + 1).min(kb.len())],
                Shape::Map | Shape::Set => &kb[..],
            };
            let mut found = false;
            for eb in cands.chunks(k) {
                let mut all = true;
                for (&x, &y) in ea.iter().zip(eb) {
                    match pair(x, y)? {
                        Some(true) => {}
                        Some(false) => {
                            all = false;
                            break;
                        }
                        None => return Ok(None),
                    }
                }
                if all {
                    found = true;
                    break;
                }
            }
            if !found {
                return Ok(Some(false));
            }
        }
        Ok(Some(true))
    }

    /// [`Vm::eq_tight`] for one element pair at `depth`: identity first; `Ok(None)` at a hooked
    /// pair.
    fn elem_tight(
        &self,
        x: Value,
        y: Value,
        depth: usize,
        span: Span,
    ) -> Result<Option<bool>, RuntimeError> {
        if same_elem(x, y) {
            return Ok(Some(true));
        }
        match self.eq_pair(x, y, depth, span)? {
            Pair::Done(eq) => Ok(Some(eq)),
            Pair::Nest(shape, _) => self.eq_tight(shape, x.as_gcref(), y.as_gcref(), depth, span),
            Pair::Hook => Ok(None),
        }
    }

    /// Apply `ans`, the answer to the top level's current pair, and pick its next pair.
    fn level_step(&mut self, ans: Option<bool>) -> Next {
        let t = self.stack.len() - LEVEL;
        let tag = self.stack[t].as_int() as usize;
        let (a, b) = (self.stack[t + 1].as_gcref(), self.stack[t + 2].as_gcref());
        let (mut i, mut j) = (
            self.stack[t + 3].as_int() as usize,
            self.stack[t + 4].as_int() as usize,
        );
        let n = tag >> 2;
        let (ka, kb) = match tag & 3 {
            0 => {
                if let Some(eq) = ans {
                    if !eq {
                        return Next::Pop(false);
                    }
                    i += 1;
                }
                if i == n {
                    return Next::Pop(true);
                }
                (i, i)
            }
            1 => {
                match (ans, j & 1) {
                    (None, _) => {}
                    // the keys match: compare the values
                    (Some(true), 0) => j |= 1,
                    (Some(false), 0) => j += 2,
                    // the entry matches: on to the next entry of `a`
                    (Some(true), _) => (i, j) = (i + 1, 0),
                    (Some(false), _) => j += 1,
                }
                if i == n {
                    return Next::Pop(true);
                }
                if j >> 1 == n {
                    return Next::Pop(false);
                }
                // `j` is child `2e + phase` of `b`; `a`'s child of the same phase
                (2 * i + (j & 1), j)
            }
            _ => {
                match ans {
                    None => {}
                    Some(true) => (i, j) = (i + 1, 0),
                    Some(false) => j += 1,
                }
                if i == n {
                    return Next::Pop(true);
                }
                if j == n {
                    return Next::Pop(false);
                }
                (i, j)
            }
        };
        self.stack[t + 3] = Value::int(i as i64);
        self.stack[t + 4] = Value::int(j as i64);
        // A child another task removed meanwhile (a race on a shared container): unequal.
        match (self.walk_child(a, ka), self.walk_child(b, kb)) {
            (Some(x), Some(y)) => Next::Pair(x, y),
            _ => Next::Pop(false),
        }
    }

    /// Element `k` of a level's snapshot list, a scan's list or its result list; `None` past
    /// its end. A render's levels read their snapshots by it too (`display.rs`).
    pub(super) fn walk_child(&self, h: GcRef, k: usize) -> Option<Value> {
        match self.heap.get(h).as_list() {
            Some(v) => v.try_get(k),
            None => unreachable!("a walk reads a list"),
        }
    }

    /// The children of a container a pair nests into, in the order a level pairs them: a map's
    /// child `2e` is entry `e`'s key, `2e + 1` its value. A render's level snapshots them in the
    /// order it writes them (`display.rs`).
    pub(super) fn walk_children(&self, h: GcRef) -> Vec<Value> {
        match self.heap.get(h) {
            Obj::List(v) => v.to_vec(),
            Obj::Tuple(v) => v.clone(),
            Obj::Struct { fields, .. } => fields.to_vec(),
            Obj::Enum { payload, .. } => payload.to_vec(),
            Obj::Map(m) => m.entries.iter().flat_map(|(_, k, v)| [k, v]).collect(),
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
            | Obj::Generator(_) => unreachable!("a walk level holds a container"),
        }
    }

    /// Park a walk whose levels (`level` slots each) start at `base`: push its header and the
    /// `hook` frame of the pair `x`, `y` at depth `d`, its return `ret`. The hook runs with
    /// `walk_base` raised by `d` (the shared budget), restored by its return ([`Vm::walk_unpark`])
    /// or its unwind ([`Vm::walk_drop`]). The one header layout of the `==` walk and the ordering
    /// walk (`compare.rs`). `Ok(false)`: `x` has no hook body.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn walk_park(
        &mut self,
        base: usize,
        level: usize,
        depth: usize,
        hook: Hook,
        [x, y]: [Value; 2],
        d: usize,
        ret: RetAction,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        let saved = self.walk_base;
        let levels = (self.stack.len() - base) / level;
        self.stack
            .extend([depth, saved, levels].map(|k| Value::int(k as i64)));
        self.walk_base = saved + d;
        let pushed = self.push_hook_frame(hook, x, &[y], ret, span);
        if !matches!(pushed, Ok(true)) {
            self.walk_base = saved;
            self.stack.truncate(base + levels * level);
        }
        pushed
    }

    /// A parked walk's hook frame returned: pop its result and the header below it and restore
    /// `walk_base`. Returns the result, the walk's start depth and the base of its levels
    /// (`level` slots each).
    pub(super) fn walk_unpark(&mut self, level: usize) -> (Value, usize, usize) {
        let res = self.pop();
        let n = self.stack.len();
        let [depth, saved, levels] = [3, 2, 1].map(|k| self.stack[n - k].as_int() as usize);
        self.stack.truncate(n - HEADER);
        self.walk_base = saved;
        (res, depth, self.stack.len() - levels * level)
    }

    /// A walk's hook frame returned: its result on the stack top, over the header. Run the walk
    /// on and feed `then`. On a fault the walk's slots and `then`'s go.
    pub(super) fn eq_resume(&mut self, then: EqThen, span: Span) -> Result<(), RuntimeError> {
        let (res, depth, base) = self.walk_unpark(LEVEL);
        let out = match self.hook_bool(res, Hook::Eq, span) {
            Ok(eq) => self.eq_levels(base, depth, None, Some(eq), Some(then), span),
            Err(e) => Err(e),
        };
        match out {
            Ok(Some(eq)) => self.eq_then(then, eq, span),
            Ok(None) => Ok(()),
            Err(e) => {
                self.stack.truncate(base - then.slots());
                Err(e)
            }
        }
    }

    /// A parked walk's hook frame unwound (`base` its frame base): restore `walk_base` from the
    /// header ([`Vm::walk_park`]). The slots go with the caller's stack.
    pub(super) fn walk_drop(&mut self, base: usize) {
        self.walk_base = self.stack[base - 2].as_int() as usize;
    }

    /// Feed a resumed walk's answer to its continuation.
    fn eq_then(&mut self, then: EqThen, eq: bool, span: Span) -> Result<(), RuntimeError> {
        match then {
            EqThen::Op { negate } => {
                self.push(Value::bool(eq ^ negate));
                Ok(())
            }
            EqThen::Push => {
                self.push(Value::bool(eq));
                Ok(())
            }
            EqThen::Scan(kind) => self.scan_step(kind, Some(eq), true, span),
        }
    }

    /// A list method that scans by `==` (`contains`, `index_of`, `unique`, `dedup`) on `h`, its
    /// result left on the stack, or (`flat`) an element `eq` hook's frame that resumes it.
    pub(super) fn list_scan(
        &mut self,
        h: GcRef,
        kind: Scan,
        args: &[Value],
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let x = match kind {
            Scan::Has | Scan::IndexOf => {
                self.arity_err(kind.name(), args, 1, span)?;
                args[0]
            }
            Scan::Unique | Scan::Dedup => {
                self.arity_err(kind.name(), args, 0, span)?;
                if kind == Scan::Unique
                    && let Some(out) = self.unique_by_hash(h, span)?
                {
                    self.push(out);
                    return Ok(());
                }
                Value::nil()
            }
        };
        self.scan_start(h, kind, x, flat, span)
    }

    /// Start the scan `kind` over the list `h` (`x` the needle of a `Has` / `IndexOf`): push its
    /// slots and run it. The list and the needle sit in the slots, rooted; a `unique` / `dedup`
    /// result list is allocated into its slot once they do.
    pub(super) fn scan_start(
        &mut self,
        h: GcRef,
        kind: Scan,
        x: Value,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        // A `Has` / `IndexOf` compares on the host stack up to the first hooked element: the
        // elements before it answered with no user code, so the scan on slots starts at it.
        let mut start = 0;
        if matches!(kind, Scan::Has | Scan::IndexOf) {
            loop {
                let Some(v) = self.walk_child(h, start) else {
                    self.push(scan_answer(kind, None));
                    return Ok(());
                };
                match self.elem_tight(v, x, 0, span)? {
                    Some(true) => {
                        self.push(scan_answer(kind, Some(start)));
                        return Ok(());
                    }
                    Some(false) => start += 1,
                    None => break,
                }
            }
        }
        let at = self.stack.len();
        self.stack
            .extend([Value::obj(h), x, Value::int(start as i64), Value::int(0)]);
        if matches!(kind, Scan::Unique | Scan::Dedup) {
            let out = self.heap.alloc(Obj::List(RVec::new()));
            self.stack[at + 1] = Value::obj(out);
        }
        // A hook can mutate the list: scan a snapshot, as a `for` does.
        if self.eq_may_reenter() {
            let snap = match self.heap.get(h).as_list() {
                Some(v) => v.clone(),
                None => unreachable!("a scan over a non-list"),
            };
            let snap = self.heap.alloc(Obj::List(snap));
            self.stack[at] = Value::obj(snap);
        }
        self.scan_step(kind, None, flat, span)
    }

    /// Run a scan on from its slots: first apply `ans`, the current element compare's answer.
    /// On a fault the slots are dropped.
    fn scan_step(
        &mut self,
        kind: Scan,
        ans: Option<bool>,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let base = self.stack.len() - SCAN;
        let r = self.scan_run(kind, base, ans, flat, span);
        if r.is_err() {
            self.stack.truncate(base);
        }
        r
    }

    fn scan_run(
        &mut self,
        kind: Scan,
        base: usize,
        mut ans: Option<bool>,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let hay = self.stack[base].as_gcref();
        let x = self.stack[base + 1];
        let mut i = self.stack[base + 2].as_int() as usize;
        let mut j = self.stack[base + 3].as_int() as usize;
        loop {
            let (l, r) = match kind {
                // `x in xs`: element `eq` needle, in order.
                Scan::Has | Scan::IndexOf => {
                    match ans.take() {
                        Some(true) => return self.scan_finish(kind, base, Some(i)),
                        Some(false) => i += 1,
                        None => {}
                    }
                    let Some(v) = self.walk_child(hay, i) else {
                        return self.scan_finish(kind, base, None);
                    };
                    (v, x)
                }
                // Collapse only CONSECUTIVE runs (Rust `Vec::dedup`): keep an element iff it
                // differs from the previously-kept one.
                Scan::Dedup => {
                    if let Some(eq) = ans.take() {
                        if !eq {
                            self.scan_keep(x, hay, i);
                        }
                        i += 1;
                    }
                    let Some(v) = self.walk_child(hay, i) else {
                        return self.scan_finish(kind, base, None);
                    };
                    let last = self.walk_last(x);
                    match last {
                        Some(p) => (p, v),
                        None => {
                            self.scan_keep(x, hay, i);
                            i += 1;
                            continue;
                        }
                    }
                }
                // Remove ALL duplicates, first-occurrence order (Python `dict.fromkeys`): each
                // element against every kept one. The flat-scalar case took the hash index.
                Scan::Unique => {
                    match ans.take() {
                        Some(true) => (i, j) = (i + 1, 0),
                        Some(false) => j += 1,
                        None => {}
                    }
                    let Some(v) = self.walk_child(hay, i) else {
                        return self.scan_finish(kind, base, None);
                    };
                    match self.walk_child(x.as_gcref(), j) {
                        Some(o) => (o, v),
                        None => {
                            self.scan_keep(x, hay, i);
                            (i, j) = (i + 1, 0);
                            continue;
                        }
                    }
                }
            };
            self.stack[base + 2] = Value::int(i as i64);
            self.stack[base + 3] = Value::int(j as i64);
            match self.elem_run(l, r, 0, flat.then_some(EqThen::Scan(kind)), span)? {
                Some(eq) => ans = Some(eq),
                None => return Ok(()),
            }
        }
    }

    /// The last element of the result list `out`.
    fn walk_last(&self, out: Value) -> Option<Value> {
        match self.heap.get(out.as_gcref()).as_list() {
            Some(v) => v.len().checked_sub(1).and_then(|k| v.try_get(k)),
            None => unreachable!("a scan's result list"),
        }
    }

    /// Append element `i` of `hay` to the result list `out`.
    fn scan_keep(&mut self, out: Value, hay: GcRef, i: usize) {
        let Some(v) = self.walk_child(hay, i) else {
            return;
        };
        let out = out.as_gcref();
        let heap = self.heap.write(out);
        if let Obj::List(o) = heap.get(out) {
            o.push(heap, v);
        }
    }

    /// The scan's answer replaces its slots: `found` for a `Has` / `IndexOf`, the result list
    /// for a `Unique` / `Dedup`.
    fn scan_finish(
        &mut self,
        kind: Scan,
        base: usize,
        found: Option<usize>,
    ) -> Result<(), RuntimeError> {
        let out = match kind {
            Scan::Has | Scan::IndexOf => scan_answer(kind, found),
            Scan::Unique | Scan::Dedup => self.stack[base + 1],
        };
        self.stack.truncate(base);
        self.push(out);
        Ok(())
    }
}

/// A `Has` / `IndexOf` scan's answer for the element it `found`.
fn scan_answer(kind: Scan, found: Option<usize>) -> Value {
    match kind {
        Scan::IndexOf => Value::int(found.map_or(-1, |i| i as i64)),
        Scan::Has | Scan::Unique | Scan::Dedup => Value::bool(found.is_some()),
    }
}

/// An element compare's identity shortcut: CPython's `x is y or x == y`. The raw-word compare IS
/// identity here — a float is heap-boxed per alloc, so ONE `nan` stored into two containers
/// compares equal while two computed NaNs stay unequal. Counted in tests (`ELEM_COMPARES`).
#[inline(always)]
fn same_elem(l: Value, r: Value) -> bool {
    #[cfg(test)]
    super::ELEM_COMPARES.with(|c| c.set(c.get() + 1));
    l == r
}
