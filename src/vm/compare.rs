//! VM: membership `in`, comparison operators, struct and sequence ordering.
//! Split out of `src/vm/arith.rs`. `super::*` is the parent module.

use super::list_hof::{EXTREME_SLOTS, SORT_SLOTS};
use super::*;
use crate::base::hooks::Hook;
use std::cmp::Ordering;

/// One level of an ordering walk: `[a, b, i]`, the two sequences (tuples or lists, read fresh at
/// every step: a `compare` hook may shrink a list) and the cursor. A walk that meets no hooked
/// pair pushes no level ([`Vm::cmp_tight`]). A parked walk's header is the `==` walk's
/// ([`Vm::walk_park`]).
const CMP_LEVEL: usize = 3;

/// One ordering pair's verdict before any element is compared.
enum Order {
    /// `None`: unordered (a NaN pair under the partial order)
    Done(Option<Ordering>),
    /// two structs or enums: their `compare` hook decides
    Hook,
    /// two sequences of one kind: lexicographic, element by element
    Nest,
}

/// What an ordering walk's answer feeds once its parked hook frame returns.
#[derive(Clone, Copy, Debug)]
pub(super) enum CmpThen {
    /// an ordering operator: push its `bool` (the partial order)
    Op(Cmp),
    /// a `sort()` / `sort_by_key` merge ([`Vm::sort_resume`], the total order)
    Sort(SortKind),
    /// a `min` / `max` scan ([`Vm::extreme_resume`], the total order)
    MinMax { max: bool },
    /// the `compare` intrinsic method on a tuple / list (`x.compare(y)`): push the int (the
    /// total order)
    Push,
}

impl CmpThen {
    /// Does it read the total order (`sort` / `min` / `max`), not the operators' partial one?
    fn total(self) -> bool {
        match self {
            CmpThen::Op(_) => false,
            CmpThen::Sort(_) | CmpThen::MinMax { .. } | CmpThen::Push => true,
        }
    }

    /// The return of a hook frame pushed for a top-level pair: its caller's own, no walk slots.
    fn top(self) -> RetAction {
        match self {
            CmpThen::Op(cmp) => RetAction::CmpOp(cmp),
            CmpThen::Sort(kind) => RetAction::Sort(kind),
            CmpThen::MinMax { max } => RetAction::MinMax { max },
            CmpThen::Push => RetAction::Push,
        }
    }

    /// The slots its continuation keeps below the walk.
    fn slots(self) -> usize {
        match self {
            CmpThen::Op(_) | CmpThen::Push => 0,
            CmpThen::Sort(_) => SORT_SLOTS,
            CmpThen::MinMax { .. } => EXTREME_SLOTS,
        }
    }
}

impl Vm {
    /// The bytes of a `bytes` or `bytearray` value; `None` for any other value.
    fn byte_contents(&self, v: Value) -> Option<Vec<u8>> {
        let h = v.as_obj()?;
        if let Obj::Bytes(b) = self.heap.get(h) {
            Some(b.to_vec())
        } else if let Obj::ByteArray(b) = self.heap.get(h) {
            Some(b.to_vec())
        } else {
            None
        }
    }

    /// `x in container` — membership test. Pops `[x, container]`, pushes a `Bool`. Dispatches on the
    /// container kind, reusing the same equality / hashing the `.contains` methods use: a
    /// list/set tests element membership, a map tests KEY membership (Python-style), a str tests
    /// substring, bytes/bytearray test an int byte or a subsequence. The checker has already
    /// type-directed this; the runtime is the fallback.
    /// `#[inline(never)]` keeps `step`'s own stack frame lean (its String/Vec locals would otherwise
    /// bloat the deep-recursion path `step → run_proto → run_until → step`).
    #[inline(never)]
    /// `flat` as for [`Vm::arith`].
    pub(super) fn op_contains(&mut self, flat: bool, span: Span) -> Result<(), RuntimeError> {
        let container = self.pop();
        let needle = self.pop();
        // `Contains` protocol (L5): a struct/enum with a `contains(self, item) -> bool` method
        // dispatches `x in obj` to that method. The `matches!` peek ENDS the immutable `self.heap`
        // borrow before `resolve_overload_method`/`run_proto` need `&mut self`. Containers (list/set/map/str) skip this and take the fast path below.
        if let Some(h) = container.as_obj()
            && matches!(self.heap.get(h), Obj::Struct { .. } | Obj::Enum { .. })
        {
            if !self.hook_op(
                flat,
                Hook::Contains,
                container,
                &[needle],
                RetAction::ToBool,
                span,
            )? {
                return Err(self.missing_hook(container, Hook::Contains, span));
            }
            return Ok(());
        }
        let found = match container.view() {
            ValueView::Obj(h) => match self.heap.get(h) {
                // The `contains` scan: its slots root the list and the needle, and an element `eq`
                // hook runs as a frame when `flat`.
                Obj::List(_) => return self.scan_start(h, Scan::Has, needle, flat, span),
                Obj::Set(_) => {
                    let hx = self.hash_key_rooted(needle, &[Value::obj(h), needle], span)?;
                    self.set_probe(h, hx, needle, span)?.is_some()
                }
                Obj::Map(_) => {
                    let hk = self.hash_key_rooted(needle, &[Value::obj(h), needle], span)?;
                    self.map_probe(h, hk, needle, span)?.is_some()
                }
                Obj::Str(_) => {
                    let Some(nh) = needle.as_obj() else {
                        return Err(self.err(
                            format!(
                                "substring `in` requires a str on the left, found {}",
                                self.type_name(needle)
                            ),
                            span,
                        ));
                    };
                    let sub = match self.heap.get(nh) {
                        Obj::Str(sub) => sub.to_string(),
                        Obj::Bytes(_)
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
                            return Err(self.err(
                                format!(
                                    "substring `in` requires a str on the left, found {}",
                                    self.type_name(needle)
                                ),
                                span,
                            ));
                        }
                    };
                    let Obj::Str(hay) = self.heap.get(h) else {
                        unreachable!()
                    };
                    hay.contains(sub.as_str())
                }
                // bytes / bytearray: an int needle tests one byte, a bytes or bytearray needle tests
                // a subsequence (str's substring rule). CPython: `98 in b"abc"`, `b"bc" in b"abc"`.
                Obj::Bytes(_) | Obj::ByteArray(_) => {
                    let hay = self.byte_contents(container).unwrap_or_default();
                    if needle.is_int() {
                        let n = needle.as_int();
                        if !(0..=255).contains(&n) {
                            return Err(self.err("byte must be in range(0, 256)".to_string(), span));
                        }
                        hay.contains(&(n as u8))
                    } else if let Some(sub) = self.byte_contents(needle) {
                        sub.is_empty() || hay.windows(sub.len()).any(|w| w == sub.as_slice())
                    } else {
                        return Err(self.err(
                            format!(
                                "a bytes-like object is required, not {}",
                                self.type_name(needle)
                            ),
                            span,
                        ));
                    }
                }
                Obj::BigInt(_)
                | Obj::FloatBox(_)
                | Obj::Iter { .. }
                | Obj::Tuple(_)
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
                | Obj::Generator(_) => {
                    return Err(self.err(
                        format!("cannot use `in` on {}", self.type_name(container)),
                        span,
                    ));
                }
            },
            ValueView::Int(_) | ValueView::Bool(_) | ValueView::Nil => {
                return Err(self.missing_hook(container, Hook::Contains, span));
            }
        };
        self.push(Value::bool(found));
        Ok(())
    }

    pub(super) fn compare_op(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        let r = self.pop();
        let l = self.pop();
        // TICKET-146: a same-kind tuple / List pair orders lexicographically. MUST precede the
        // struct/enum gate below.
        if self.seq_pair(l, r) {
            let cmp = cmp_of(op);
            // A NaN element pair: every ordered compare is false.
            if let Some(ord) = self.cmp_run(l, r, false, Some(CmpThen::Op(cmp)), span)? {
                self.push(Value::bool(ord.is_some_and(|o| cmp.holds(o))));
            }
            return Ok(());
        }
        // Operator overloading: ordering on two structs dispatches to `compare(self, other) -> int`
        // (the `Comparable` protocol). The checker has verified conformance. Equality stays
        // structural; only ordering is overloaded.
        if let (Some(hl), Some(hr)) = (l.as_obj(), r.as_obj())
            && matches!(self.heap.get(hl), Obj::Struct { .. } | Obj::Enum { .. })
            && matches!(self.heap.get(hr), Obj::Struct { .. } | Obj::Enum { .. })
        {
            return self.struct_ordering(op, l, r, span);
        }
        let b = self.ordered_bool(op, l, r, span)?;
        self.push(Value::bool(b));
        Ok(())
    }

    /// Map an ordering operator (`< <= > >=`) over two values to a bool. `compare` returns `None` for
    /// two reasons we MUST distinguish: (1) both operands numeric ⇒ a NaN is involved — every ordered
    /// compare against NaN is `false` (IEEE-754 / Python / Rust parity), never a fault; (2) genuinely
    /// incomparable TYPES (the `_ => None` fallthrough, e.g. str vs int) ⇒ keep the existing fault.
    /// `Ordering` has no "unordered" value, so the NaN case is special-cased here before the
    /// is_lt/is_le/is_gt/is_ge match — encoding it as a fake `Ordering` would make exactly one of the
    /// four ops true.
    pub(super) fn ordered_bool(
        &self,
        op: &Op,
        a: Value,
        b: Value,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        match self.compare(a, b) {
            Some(ord) => Ok(match op {
                Op::Lt => ord.is_lt(),
                Op::LtEq => ord.is_le(),
                Op::Gt => ord.is_gt(),
                Op::GtEq => ord.is_ge(),
                Op::ConstInt(_)
                | Op::ConstFloat(_)
                | Op::ConstStr(_)
                | Op::ConstBytes(_)
                | Op::True
                | Op::False
                | Op::Nil
                | Op::Pop
                | Op::Assert { .. }
                | Op::LoadBuiltin(_)
                | Op::GetLocal(_)
                | Op::SetLocal(_)
                | Op::GetGlobalSlot(_)
                | Op::DefineGlobalSlot(_)
                | Op::SetGlobalSlot(_)
                | Op::GetCaptured(_)
                | Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Mod
                | Op::Neg
                | Op::Not
                | Op::Eq
                | Op::NotEq
                | Op::BitAnd
                | Op::BitOr
                | Op::BitXor
                | Op::Shl
                | Op::Shr
                | Op::Contains
                | Op::AsBool
                | Op::AsInt
                | Op::BinLocalLocal { .. }
                | Op::BinLocalConst { .. }
                | Op::IncLocal { .. }
                | Op::Jump(_)
                | Op::JumpIfFalse(_)
                | Op::JumpIfFalseKeep(_)
                | Op::JumpIfTrueKeep(_)
                | Op::PushHandler(_)
                | Op::PopHandler
                | Op::Call(_)
                | Op::CallMethod { .. }
                | Op::CallHook { .. }
                | Op::CallStatic { .. }
                | Op::CallStaticDyn { .. }
                | Op::CallBuiltin(..)
                | Op::CallNativeStatic(..)
                | Op::CallPrint(_)
                | Op::CallPrintSep { .. }
                | Op::Return
                | Op::Yield
                | Op::DeferCall(_)
                | Op::DeferMethod(..)
                | Op::EnterDeferScope
                | Op::LeaveDeferScope
                | Op::DrainHandlerDefers
                | Op::Drain(_)
                | Op::Try
                | Op::JsonDecode(_)
                | Op::JsonToValue
                | Op::NewList(_)
                | Op::NewTuple(_)
                | Op::NewMap(_)
                | Op::NewSet(_)
                | Op::NewStruct(..)
                | Op::NewCell
                | Op::CellLoad
                | Op::CellStore
                | Op::NewEnum { .. }
                | Op::MakeFunc(_)
                | Op::MakeFuncIn(_)
                | Op::MakeMethodFunc { .. }
                | Op::JumpIfProvided(..)
                | Op::MakeCffi(_)
                | Op::MakeClosure(..)
                | Op::GetField { .. }
                | Op::GetIndex
                | Op::SliceRange
                | Op::SetField { .. }
                | Op::SetIndex
                | Op::Dup
                | Op::Dup2
                | Op::ToStr
                | Op::ToStrFmt(_)
                | Op::BuildStr(_)
                | Op::ListClone
                | Op::ArrLen
                | Op::HasNextHook
                | Op::IsGenerator
                | Op::IsCursor
                | Op::IterableToCursor
                | Op::IsChannel
                | Op::ChanRecvOrClosed
                | Op::EnsureEnum(_)
                | Op::MatchArm { .. }
                | Op::MatchNoArm(_)
                | Op::EnterNursery
                | Op::JoinNursery
                | Op::ReclaimNursery
                | Op::SpawnCall(..)
                | Op::SpawnRecv(..)
                | Op::SpawnBlock(..)
                | Op::WaitPoll(_)
                | Op::NewChannel(_)
                | Op::NewShared
                | Op::NewRwShared
                | Op::NewAtomic
                | Op::NewAtomicInt
                | Op::NewExecutor
                | Op::NewExecutorN => unreachable!(),
            }),
            // Both numeric ⇒ NaN is involved ⇒ false for all four ops.
            None if self.is_numeric(a) && self.is_numeric(b) => Ok(false),
            // Genuinely-incomparable types: unreachable from well-typed source (the checker rejects
            // e.g. `str < int`); kept for internal-invariant safety.
            None => {
                let name = match op {
                    Op::Lt => "Lt",
                    Op::LtEq => "LtEq",
                    Op::Gt => "Gt",
                    Op::GtEq => "GtEq",
                    Op::ConstInt(_)
                    | Op::ConstFloat(_)
                    | Op::ConstStr(_)
                    | Op::ConstBytes(_)
                    | Op::True
                    | Op::False
                    | Op::Nil
                    | Op::Pop
                    | Op::Assert { .. }
                    | Op::LoadBuiltin(_)
                    | Op::GetLocal(_)
                    | Op::SetLocal(_)
                    | Op::GetGlobalSlot(_)
                    | Op::DefineGlobalSlot(_)
                    | Op::SetGlobalSlot(_)
                    | Op::GetCaptured(_)
                    | Op::Add
                    | Op::Sub
                    | Op::Mul
                    | Op::Div
                    | Op::Mod
                    | Op::Neg
                    | Op::Not
                    | Op::Eq
                    | Op::NotEq
                    | Op::BitAnd
                    | Op::BitOr
                    | Op::BitXor
                    | Op::Shl
                    | Op::Shr
                    | Op::Contains
                    | Op::AsBool
                    | Op::AsInt
                    | Op::BinLocalLocal { .. }
                    | Op::BinLocalConst { .. }
                    | Op::IncLocal { .. }
                    | Op::Jump(_)
                    | Op::JumpIfFalse(_)
                    | Op::JumpIfFalseKeep(_)
                    | Op::JumpIfTrueKeep(_)
                    | Op::PushHandler(_)
                    | Op::PopHandler
                    | Op::Call(_)
                    | Op::CallMethod { .. }
                    | Op::CallHook { .. }
                    | Op::CallStatic { .. }
                    | Op::CallStaticDyn { .. }
                    | Op::CallBuiltin(..)
                    | Op::CallNativeStatic(..)
                    | Op::CallPrint(_)
                    | Op::CallPrintSep { .. }
                    | Op::Return
                    | Op::Yield
                    | Op::DeferCall(_)
                    | Op::DeferMethod(..)
                    | Op::EnterDeferScope
                    | Op::LeaveDeferScope
                    | Op::DrainHandlerDefers
                    | Op::Drain(_)
                    | Op::Try
                    | Op::JsonDecode(_)
                    | Op::JsonToValue
                    | Op::NewList(_)
                    | Op::NewTuple(_)
                    | Op::NewMap(_)
                    | Op::NewSet(_)
                    | Op::NewStruct(..)
                    | Op::NewCell
                    | Op::CellLoad
                    | Op::CellStore
                    | Op::NewEnum { .. }
                    | Op::MakeFunc(_)
                    | Op::MakeFuncIn(_)
                    | Op::MakeMethodFunc { .. }
                    | Op::JumpIfProvided(..)
                    | Op::MakeCffi(_)
                    | Op::MakeClosure(..)
                    | Op::GetField { .. }
                    | Op::GetIndex
                    | Op::SliceRange
                    | Op::SetField { .. }
                    | Op::SetIndex
                    | Op::Dup
                    | Op::Dup2
                    | Op::ToStr
                    | Op::ToStrFmt(_)
                    | Op::BuildStr(_)
                    | Op::ListClone
                    | Op::ArrLen
                    | Op::HasNextHook
                    | Op::IsGenerator
                    | Op::IsCursor
                    | Op::IterableToCursor
                    | Op::IsChannel
                    | Op::ChanRecvOrClosed
                    | Op::EnsureEnum(_)
                    | Op::MatchArm { .. }
                    | Op::MatchNoArm(_)
                    | Op::EnterNursery
                    | Op::JoinNursery
                    | Op::ReclaimNursery
                    | Op::SpawnCall(..)
                    | Op::SpawnRecv(..)
                    | Op::SpawnBlock(..)
                    | Op::WaitPoll(_)
                    | Op::NewChannel(_)
                    | Op::NewShared
                    | Op::NewRwShared
                    | Op::NewAtomic
                    | Op::NewAtomicInt
                    | Op::NewExecutor
                    | Op::NewExecutorN => unreachable!(),
                };
                Err(self.err(
                    format!(
                        "cannot apply {name} to {} and {}",
                        self.type_name(a),
                        self.type_name(b)
                    ),
                    span,
                ))
            }
        }
    }

    /// Dispatch an ordering operator on two structs to the receiver's `compare(self, other) -> int`
    /// method as a frame (it runs on the dispatch loop: `compare_op`'s only callers are steps); its
    /// return maps the sign of the result to a boolean ([`RetAction::CmpOp`]).
    pub(super) fn struct_ordering(
        &mut self,
        op: &Op,
        l: Value,
        r: Value,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let cmp = cmp_of(op);
        if !self.push_hook_frame(Hook::Compare, l, &[r], RetAction::CmpOp(cmp), span)? {
            return Err(self.missing_hook(l, Hook::Compare, span));
        }
        Ok(())
    }

    /// Which lexicographic container `v` is, for the TICKET-146 ordering walk: `0` tuple, `1` list.
    /// `None` for anything else, so a user enum, an `Option` or a `Result` never enters the walk
    /// (the checker rejects every `T?` ordering: owner decision 4 of TICKET-265).
    pub(super) fn seq_kind(&self, v: Value) -> Option<u8> {
        match self.heap.get(v.as_obj()?) {
            Obj::Tuple(_) => Some(0),
            Obj::List(_) => Some(1),
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
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
            | Obj::Generator(_) => None,
        }
    }

    /// `Some(k)` when `a` and `b` are the same [`Self::seq_kind`] — the pairs every ordering gate
    /// walks ([`Self::cmp_run`]) BEFORE its struct/enum user-`compare` gate.
    pub(super) fn seq_pair(&self, a: Value, b: Value) -> bool {
        matches!((self.seq_kind(a), self.seq_kind(b)), (Some(x), Some(y)) if x == y)
    }

    /// Length of the tuple/list `v` and its `i`th element, read fresh from the heap.
    fn seq_elem(&self, v: Value, i: usize) -> (usize, Option<Value>) {
        match v.as_obj().map(|h| self.heap.get(h)) {
            Some(Obj::Tuple(xs)) => (xs.len(), xs.get(i).copied()),
            Some(Obj::List(xs)) => (xs.len(), xs.try_get(i)),
            _ => (0, None),
        }
    }

    /// The total order behind `sort()` / `sort_by_key` / `min` / `max` / `.compare()` over two
    /// values, synchronous: a hooked pair's `compare` runs nested ([`Vm::cmp_run`] with no
    /// continuation).
    pub(super) fn order_key(
        &mut self,
        a: Value,
        b: Value,
        span: Span,
    ) -> Result<Ordering, RuntimeError> {
        let ord = self.cmp_run(a, b, true, None, span)?;
        Ok(ord
            .expect("a walk with no continuation answers")
            .unwrap_or(Ordering::Equal))
    }

    /// The ordering of `l` and `r`, the walk's one entry. `total` picks the flavour: `false` is
    /// the OPERATOR order (`<`/`<=`/`>`/`>=`), partial, so a NaN element pair answers `None` (all
    /// four ops false); `true` is the TOTAL order of `sort`/`min`/`max`/`sort_by_key`/`.compare()`,
    /// whose float leaves go through `float_order` (DEC-144). `then`: `None` runs a hooked pair's
    /// `compare` nested ([`Vm::guarded_walk`]) and always answers; `Some` (the caller runs on the
    /// dispatch loop) pushes the hook's frame and answers `None`: its return resumes the walk and
    /// feeds `then` ([`Vm::cmp_resume`]).
    ///
    /// Two same-kind tuples / lists order lexicographically (TICKET-146): elements left to right,
    /// the first non-equal pair decides, a shorter prefix sorts first. An element pair is equal by
    /// identity first (CPython's `x is y or x == y`: the raw word is identity, a float is boxed
    /// per allocation), which keeps `<=` agreeing with the container `==`. `MAX_STRUCTURAL_DEPTH`
    /// is tested against `walk_base + depth` at every pair, as the `==` walk's.
    #[inline]
    pub(super) fn cmp_run(
        &mut self,
        l: Value,
        r: Value,
        total: bool,
        then: Option<CmpThen>,
        span: Span,
    ) -> Result<Option<Option<Ordering>>, RuntimeError> {
        match self.order_pair(l, r, 0, total, span)? {
            Order::Done(ord) => Ok(Some(ord)),
            pair @ (Order::Hook | Order::Nest) => self.cmp_nested(pair, l, r, total, then, span),
        }
    }

    /// The one owner of the ordering pair rules: the verdict on one pair at `depth` that needs no
    /// element compare, or what it needs. The checker has verified the operands orderable.
    #[inline(always)]
    fn order_pair(
        &self,
        a: Value,
        b: Value,
        depth: usize,
        total: bool,
        span: Span,
    ) -> Result<Order, RuntimeError> {
        if self.walk_base + depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.depth_exceeded_err(span));
        }
        // MUST precede the struct/enum gate: an `Option` is an `Obj::Enum` with no `compare`.
        if self.seq_pair(a, b) {
            return Ok(Order::Nest);
        }
        if self.compare_hook_pair(a, b) {
            return Ok(Order::Hook);
        }
        if !total {
            return Ok(Order::Done(self.compare(a, b)));
        }
        // Float keys order by `float_order` for the WHOLE comparison (not just the NaN case),
        // exactly mirroring `sort()`'s `value_order` Float arm — so `sort_by_key` and `sort()`
        // agree on every float pair: `-0.0`/`+0.0` are Equal (stable, first wins, like CPython)
        // and NaN is deterministic, to one end. Int keys deliberately stay on the int path below
        // (`Int.cmp`): routing them through `as_f64` would lose precision past 2^53.
        if a.is_float() && b.is_float() {
            return Ok(Order::Done(Some(super::arith::float_order(
                self.float_of(a),
                self.float_of(b),
            ))));
        }
        match self.compare(a, b) {
            Some(ord) => Ok(Order::Done(Some(ord))),
            // Numeric `None` means a NaN float — handled above for the Float/Float case; this arm
            // only catches a mixed int/float key pair (not reachable for a single key type K),
            // kept deterministic via `float_order` for safety.
            None if self.is_numeric(a) && self.is_numeric(b) => Ok(Order::Done(Some(
                super::arith::float_order(self.as_f64(a), self.as_f64(b)),
            ))),
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

    /// The walk past its first pair: `pair` is a hook or a sequence pair. Out of line, so the
    /// scalar answer of [`Vm::cmp_run`] stays a tight inline path (a scalar `sort_by_key`).
    #[inline(never)]
    fn cmp_nested(
        &mut self,
        pair: Order,
        l: Value,
        r: Value,
        total: bool,
        then: Option<CmpThen>,
        span: Span,
    ) -> Result<Option<Option<Ordering>>, RuntimeError> {
        let base = self.stack.len();
        let hook = match pair {
            Order::Done(ord) => return Ok(Some(ord)),
            Order::Nest => {
                if let Some(ord) = self.cmp_tight(l, r, 0, total, span)? {
                    return Ok(Some(ord));
                }
                self.stack.extend([l, r, Value::int(0)]);
                None
            }
            // A top-level pair's hook frame returns to its caller directly: no walk slots.
            Order::Hook => match then {
                Some(then) if self.push_hook_frame(Hook::Compare, l, &[r], then.top(), span)? => {
                    return Ok(None);
                }
                Some(_) | None => Some((l, r)),
            },
        };
        let out = self.cmp_levels(base, hook, None, total, then, span);
        if out.is_err() {
            self.stack.truncate(base);
        }
        out
    }

    /// The walk of a sequence pair up to its first hooked pair: no user code runs, so nothing
    /// can park, collect or mutate, and the walk recurses on the host stack over each level's
    /// elements (`MAX_STRUCTURAL_DEPTH` bounds it, as at every pair). `Ok(None)` at a hooked pair:
    /// the caller walks again on levels ([`Vm::cmp_levels`]), which meets the same pairs in the
    /// same order up to that one. The pair rules are [`Vm::order_pair`]'s.
    fn cmp_tight(
        &self,
        a: Value,
        b: Value,
        depth: usize,
        total: bool,
        span: Span,
    ) -> Result<Option<Option<Ordering>>, RuntimeError> {
        let mut i = 0;
        loop {
            let (len_a, x) = self.seq_elem(a, i);
            let (len_b, y) = self.seq_elem(b, i);
            let (Some(x), Some(y)) = (x, y) else {
                return Ok(Some(Some(len_a.cmp(&len_b))));
            };
            let ord = match x == y {
                true => Some(Ordering::Equal),
                false => match self.order_pair(x, y, depth + 1, total, span)? {
                    Order::Done(ord) => ord,
                    Order::Hook => return Ok(None),
                    Order::Nest => match self.cmp_tight(x, y, depth + 1, total, span)? {
                        Some(ord) => ord,
                        None => return Ok(None),
                    },
                },
            };
            match ord {
                Some(Ordering::Equal) => i += 1,
                other => return Ok(Some(other)),
            }
        }
    }

    /// Run the walk whose levels start at `base`: first the hooked pair `hook`, or the answer
    /// `ans` to the top level's current pair. `Ok(None)`: a hook frame was pushed (`then` is
    /// `Some`).
    fn cmp_levels(
        &mut self,
        base: usize,
        mut hook: Option<(Value, Value)>,
        mut ans: Option<Option<Ordering>>,
        total: bool,
        then: Option<CmpThen>,
        span: Span,
    ) -> Result<Option<Option<Ordering>>, RuntimeError> {
        loop {
            if let Some((x, y)) = hook.take() {
                let d = (self.stack.len() - base) / CMP_LEVEL;
                if let Some(then) = then
                    && self.walk_park(
                        base,
                        CMP_LEVEL,
                        0,
                        Hook::Compare,
                        [x, y],
                        d,
                        RetAction::CmpWalk(then),
                        span,
                    )?
                {
                    return Ok(None);
                }
                // The hook re-enters the VM and may start its own structural walk, so hand it
                // the depth this walk has already consumed — one shared budget.
                let at = self.walk_base + d;
                let res =
                    self.guarded_walk(at, |vm| vm.invoke_hook(Hook::Compare, x, &[y], span))?;
                let Some(res) = res else {
                    return Err(self.missing_hook(x, Hook::Compare, span));
                };
                ans = Some(Some(self.hook_ord(res, span)?));
            }
            let n = self.stack.len();
            if n == base {
                return Ok(Some(ans.expect("a walk with no level has its answer")));
            }
            let t = n - CMP_LEVEL;
            let (a, b) = (self.stack[t], self.stack[t + 1]);
            let mut i = self.stack[t + 2].as_int() as usize;
            match ans.take() {
                None => {}
                Some(Some(Ordering::Equal)) => i += 1,
                Some(decided) => {
                    self.stack.truncate(t);
                    ans = Some(decided);
                    continue;
                }
            }
            let (len_a, x) = self.seq_elem(a, i);
            let (len_b, y) = self.seq_elem(b, i);
            let (Some(x), Some(y)) = (x, y) else {
                self.stack.truncate(t);
                ans = Some(Some(len_a.cmp(&len_b)));
                continue;
            };
            self.stack[t + 2] = Value::int(i as i64);
            if x == y {
                ans = Some(Some(Ordering::Equal));
                continue;
            }
            match self.order_pair(x, y, (n - base) / CMP_LEVEL, total, span)? {
                Order::Done(ord) => ans = Some(ord),
                Order::Hook => hook = Some((x, y)),
                Order::Nest => self.stack.extend([x, y, Value::int(0)]),
            }
        }
    }

    /// An ordering walk's hook frame returned: its result on the stack top, over the header. Run
    /// the walk on and feed `then`. On a fault the walk's slots and `then`'s go.
    pub(super) fn cmp_resume(&mut self, then: CmpThen, span: Span) -> Result<(), RuntimeError> {
        let (res, _, base) = self.walk_unpark(CMP_LEVEL);
        let out = match self.hook_ord(res, span) {
            Ok(ord) => self.cmp_levels(base, None, Some(Some(ord)), then.total(), Some(then), span),
            Err(e) => Err(e),
        };
        match out {
            Ok(Some(ord)) => self.cmp_then(then, ord, span),
            Ok(None) => Ok(()),
            Err(e) => {
                self.stack.truncate(base - then.slots());
                Err(e)
            }
        }
    }

    /// Feed a resumed walk's answer to its continuation. A merge, a scan and `.compare()` read it
    /// as the `compare` result a top-level hook would have given (`-1` / `0` / `1`).
    fn cmp_then(
        &mut self,
        then: CmpThen,
        ord: Option<Ordering>,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let out = || Value::int(ord.unwrap_or(Ordering::Equal) as i64);
        match then {
            CmpThen::Op(cmp) => {
                self.push(Value::bool(ord.is_some_and(|o| cmp.holds(o))));
                Ok(())
            }
            CmpThen::Sort(kind) => self.sort_resume(kind, Some(out()), span),
            CmpThen::MinMax { max } => self.extreme_resume(max, Some(out()), span),
            CmpThen::Push => {
                self.push(out());
                Ok(())
            }
        }
    }
}

/// The ordering operator `op` is (an `Op::Lt` / `LtEq` / `Gt` / `GtEq`).
fn cmp_of(op: &Op) -> Cmp {
    match op {
        Op::Lt => Cmp::Lt,
        Op::LtEq => Cmp::LtEq,
        Op::Gt => Cmp::Gt,
        Op::GtEq => Cmp::GtEq,
        Op::ConstInt(_)
        | Op::ConstFloat(_)
        | Op::ConstStr(_)
        | Op::ConstBytes(_)
        | Op::True
        | Op::False
        | Op::Nil
        | Op::Pop
        | Op::Assert { .. }
        | Op::LoadBuiltin(_)
        | Op::GetLocal(_)
        | Op::SetLocal(_)
        | Op::GetGlobalSlot(_)
        | Op::DefineGlobalSlot(_)
        | Op::SetGlobalSlot(_)
        | Op::GetCaptured(_)
        | Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Mod
        | Op::Neg
        | Op::Not
        | Op::Eq
        | Op::NotEq
        | Op::BitAnd
        | Op::BitOr
        | Op::BitXor
        | Op::Shl
        | Op::Shr
        | Op::Contains
        | Op::AsBool
        | Op::AsInt
        | Op::BinLocalLocal { .. }
        | Op::BinLocalConst { .. }
        | Op::IncLocal { .. }
        | Op::Jump(_)
        | Op::JumpIfFalse(_)
        | Op::JumpIfFalseKeep(_)
        | Op::JumpIfTrueKeep(_)
        | Op::PushHandler(_)
        | Op::PopHandler
        | Op::Call(_)
        | Op::CallMethod { .. }
        | Op::CallHook { .. }
        | Op::CallStatic { .. }
        | Op::CallStaticDyn { .. }
        | Op::CallBuiltin(..)
        | Op::CallNativeStatic(..)
        | Op::CallPrint(_)
        | Op::CallPrintSep { .. }
        | Op::Return
        | Op::Yield
        | Op::DeferCall(_)
        | Op::DeferMethod(..)
        | Op::EnterDeferScope
        | Op::LeaveDeferScope
        | Op::DrainHandlerDefers
        | Op::Drain(_)
        | Op::Try
        | Op::JsonDecode(_)
        | Op::JsonToValue
        | Op::NewList(_)
        | Op::NewTuple(_)
        | Op::NewMap(_)
        | Op::NewSet(_)
        | Op::NewStruct(..)
        | Op::NewCell
        | Op::CellLoad
        | Op::CellStore
        | Op::NewEnum { .. }
        | Op::MakeFunc(_)
        | Op::MakeFuncIn(_)
        | Op::MakeMethodFunc { .. }
        | Op::JumpIfProvided(..)
        | Op::MakeCffi(_)
        | Op::MakeClosure(..)
        | Op::GetField { .. }
        | Op::GetIndex
        | Op::SliceRange
        | Op::SetField { .. }
        | Op::SetIndex
        | Op::Dup
        | Op::Dup2
        | Op::ToStr
        | Op::ToStrFmt(_)
        | Op::BuildStr(_)
        | Op::ListClone
        | Op::ArrLen
        | Op::HasNextHook
        | Op::IsGenerator
        | Op::IsCursor
        | Op::IterableToCursor
        | Op::IsChannel
        | Op::ChanRecvOrClosed
        | Op::EnsureEnum(_)
        | Op::MatchArm { .. }
        | Op::MatchNoArm(_)
        | Op::EnterNursery
        | Op::JoinNursery
        | Op::ReclaimNursery
        | Op::SpawnCall(..)
        | Op::SpawnRecv(..)
        | Op::SpawnBlock(..)
        | Op::WaitPoll(_)
        | Op::NewChannel(_)
        | Op::NewShared
        | Op::NewRwShared
        | Op::NewAtomic
        | Op::NewAtomicInt
        | Op::NewExecutor
        | Op::NewExecutorN => unreachable!(),
    }
}
