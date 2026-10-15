//! VM: membership `in`, comparison operators, struct and sequence ordering.
//! Split out of `src/vm/arith.rs`. `super::*` is the parent module.

use super::*;
use crate::base::hooks::Hook;

impl Vm {
    /// The bytes of a `bytes` or `bytearray` value; `None` for any other value.
    fn byte_contents(&self, v: Value) -> Option<Vec<u8>> {
        let h = v.as_obj()?;
        if let Obj::Bytes(b) = self.heap.get(h) {
            Some(b.to_vec())
        } else if let Obj::ByteArray(b) = self.heap.get(h) {
            Some(b.clone())
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
    pub(super) fn op_contains(&mut self, span: Span) -> Result<(), RuntimeError> {
        let container = self.pop();
        let needle = self.pop();
        // `Contains` protocol (L5): a struct/enum with a `contains(self, item) -> bool` method
        // dispatches `x in obj` to that method. The `matches!` peek ENDS the immutable `self.heap`
        // borrow before `resolve_overload_method`/`run_proto` need `&mut self` (mirrors
        // `struct_compare`). Containers (list/set/map/str) skip this and take the fast path below.
        if let Some(h) = container.as_obj()
            && matches!(self.heap.get(h), Obj::Struct { .. } | Obj::Enum { .. })
        {
            let Some(res) =
                self.reentered(|vm| vm.invoke_hook(Hook::Contains, container, &[needle], span))?
            else {
                return Err(self.missing_hook(container, Hook::Contains, span));
            };
            return match res.as_bool() {
                Some(b) => {
                    self.push(Value::bool(b));
                    Ok(())
                }
                None => Err(self.err(
                    format!("contains() must return bool, got {}", self.type_name(res)),
                    span,
                )),
            };
        }
        let found = match container.view() {
            ValueView::Obj(h) => match self.heap.get(h) {
                Obj::List(items) => {
                    // Root the list and the needle: `elems` is a clone held only in a Rust local, and
                    // a user `eq` on an element re-enters the VM (and can collect).
                    let elems = items.clone();
                    self.with_roots(&[Value::obj(h), needle], |vm| {
                        vm.seq_slot(&elems, needle, span)
                    })?
                    .is_some()
                }
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
        // TICKET-146: a same-kind tuple / List / Option pair orders lexicographically. MUST precede
        // the struct/enum gate below — an `Option` is an `Obj::Enum` with no `compare` method.
        if self.seq_pair(l, r) {
            let b = match self.seq_order(l, r, false, span)? {
                Some(ord) => match op {
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
                    | Op::StampGen(_)
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
                    | Op::CallPrint(_)
                    | Op::CallPrintSep { .. }
                    | Op::Return
                    | Op::Yield
                    | Op::DeferCall(_)
                    | Op::DeferMethod(..)
                    | Op::EnterDeferScope
                    | Op::LeaveDeferScope
                    | Op::DrainHandlerDefers
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
                    | Op::NewTimer
                    | Op::NewExecutor
                    | Op::NewExecutorN => unreachable!(),
                },
                None => false, // a NaN element pair: every ordered compare is false
            };
            self.push(Value::bool(b));
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
                | Op::StampGen(_)
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
                | Op::CallPrint(_)
                | Op::CallPrintSep { .. }
                | Op::Return
                | Op::Yield
                | Op::DeferCall(_)
                | Op::DeferMethod(..)
                | Op::EnterDeferScope
                | Op::LeaveDeferScope
                | Op::DrainHandlerDefers
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
                | Op::NewTimer
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
                    | Op::StampGen(_)
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
                    | Op::CallPrint(_)
                    | Op::CallPrintSep { .. }
                    | Op::Return
                    | Op::Yield
                    | Op::DeferCall(_)
                    | Op::DeferMethod(..)
                    | Op::EnterDeferScope
                    | Op::LeaveDeferScope
                    | Op::DrainHandlerDefers
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
                    | Op::NewTimer
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
    /// method, mapping the sign of the result to a boolean.
    pub(super) fn struct_ordering(
        &mut self,
        op: &Op,
        l: Value,
        r: Value,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let ord = self.struct_compare(l, r, span)?;
        let b = match op {
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
            | Op::StampGen(_)
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
            | Op::CallPrint(_)
            | Op::CallPrintSep { .. }
            | Op::Return
            | Op::Yield
            | Op::DeferCall(_)
            | Op::DeferMethod(..)
            | Op::EnterDeferScope
            | Op::LeaveDeferScope
            | Op::DrainHandlerDefers
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
            | Op::NewTimer
            | Op::NewExecutor
            | Op::NewExecutorN => unreachable!(),
        };
        self.push(Value::bool(b));
        Ok(())
    }

    /// Call a struct's `compare(self, other) -> int` method and return the resulting `Ordering`.
    /// Shared by ordering operators (`struct_ordering`) and `list.sort()` over Comparable structs.
    pub(super) fn struct_compare(
        &mut self,
        l: Value,
        r: Value,
        span: Span,
    ) -> Result<std::cmp::Ordering, RuntimeError> {
        let Some(res) = self.reentered(|vm| vm.invoke_hook(Hook::Compare, l, &[r], span))? else {
            return Err(self.missing_hook(l, Hook::Compare, span));
        };
        match self.int_val(res) {
            Some(n) => Ok(n.cmp(&0)),
            None => Err(self.err(
                format!("compare() must return int, got {}", self.type_name(res)),
                span,
            )),
        }
    }

    /// Which lexicographic container `v` is, for the TICKET-146 ordering walk: `0` tuple, `1` list,
    /// `2` `Option` (an `Obj::Enum` stamped with the reserved `VID_SOME`/`VID_NONE_VARIANT` id,
    /// disjoint from every user variant id). `None` for anything else, so a user enum or a
    /// `Result` never enters the walk.
    pub(super) fn seq_kind(&self, v: Value) -> Option<u8> {
        use crate::base::op::{VID_NONE_VARIANT, VID_SOME};
        match self.heap.get(v.as_obj()?) {
            Obj::Tuple(_) => Some(0),
            Obj::List(_) => Some(1),
            Obj::Enum { variant_id, .. }
                if *variant_id == VID_SOME || *variant_id == VID_NONE_VARIANT =>
            {
                Some(2)
            }
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
    /// hands to [`Self::seq_order`] BEFORE its struct/enum user-`compare` gate (an `Option` is an
    /// `Obj::Enum` with no `compare` method, so reaching that gate would fault).
    pub(super) fn seq_pair(&self, a: Value, b: Value) -> bool {
        matches!((self.seq_kind(a), self.seq_kind(b)), (Some(x), Some(y)) if x == y)
    }

    /// Length of the tuple/list `v` and its `i`th element, read fresh from the heap.
    fn seq_elem(&self, v: Value, i: usize) -> (usize, Option<Value>) {
        match v.as_obj().map(|h| self.heap.get(h)) {
            Some(Obj::Tuple(xs) | Obj::List(xs)) => (xs.len(), xs.get(i).copied()),
            _ => (0, None),
        }
    }

    /// Lexicographic order of two same-kind tuple/List/Option values (TICKET-146): elements left to
    /// right, the first non-equal pair decides, a shorter prefix sorts first, `None < Some(_)`.
    ///
    /// `total` picks the flavour. `false` is the OPERATOR order (`<`/`<=`/`>`/`>=`): partial, so a NaN
    /// element pair yields `None` (all four ops false). `true` is the TOTAL order behind
    /// `sort`/`min`/`max`/`sort_by_key`/`.compare()`, whose float leaves go through `order_key`'s
    /// `float_order` (DEC-144) — never a bare `total_cmp`.
    ///
    /// A user `compare` inside an element re-enters the VM (may GC, may shrink a list), so both
    /// containers are rooted and every step re-reads lengths and elements from the heap.
    pub(super) fn seq_order(
        &mut self,
        a: Value,
        b: Value,
        total: bool,
        span: Span,
    ) -> Result<Option<std::cmp::Ordering>, RuntimeError> {
        use crate::base::op::VID_SOME;
        use std::cmp::Ordering;
        self.with_roots(&[a, b], |vm| {
            if vm.seq_kind(a) == Some(2) {
                let payload = |vm: &Self, v: Value| match v.as_obj().map(|h| vm.heap.get(h)) {
                    Some(Obj::Enum {
                        variant_id,
                        payload,
                    }) if *variant_id == VID_SOME => payload.first().copied(),
                    _ => None,
                };
                return match (payload(vm, a), payload(vm, b)) {
                    (None, None) => Ok(Some(Ordering::Equal)),
                    (None, Some(_)) => Ok(Some(Ordering::Less)),
                    (Some(_), None) => Ok(Some(Ordering::Greater)),
                    (Some(x), Some(y)) => vm.elem_order(x, y, total, span),
                };
            }
            let mut i = 0;
            loop {
                let (len_a, x) = vm.seq_elem(a, i);
                let (len_b, y) = vm.seq_elem(b, i);
                let (Some(x), Some(y)) = (x, y) else {
                    return Ok(Some(len_a.cmp(&len_b)));
                };
                match vm.elem_order(x, y, total, span)? {
                    Some(Ordering::Equal) => i += 1,
                    other => return Ok(other),
                }
            }
        })
    }

    /// One element pair of a [`Self::seq_order`] walk. CPython's `x is y or x == y` first: the raw
    /// word is identity (a float is boxed per allocation), which keeps `<=` agreeing with the
    /// container `==` (`elem_equal`). Then the total order via `order_key`, or the partial one:
    /// nested containers recurse, a struct/enum orders by its `compare`, a scalar by `compare`
    /// (`None` on NaN).
    fn elem_order(
        &mut self,
        x: Value,
        y: Value,
        total: bool,
        span: Span,
    ) -> Result<Option<std::cmp::Ordering>, RuntimeError> {
        if x == y {
            return Ok(Some(std::cmp::Ordering::Equal));
        }
        // The pair may reach user code (`compare` re-enters the VM and can GC), and a list element
        // may since have left its container — root both across the call.
        self.with_roots(&[x, y], |vm| {
            if total {
                return Ok(Some(vm.order_key(x, y, span)?));
            }
            if vm.seq_pair(x, y) {
                return vm.seq_order(x, y, false, span);
            }
            if let (Some(hx), Some(hy)) = (x.as_obj(), y.as_obj())
                && matches!(vm.heap.get(hx), Obj::Struct { .. } | Obj::Enum { .. })
                && matches!(vm.heap.get(hy), Obj::Struct { .. } | Obj::Enum { .. })
            {
                return Ok(Some(vm.struct_compare(x, y, span)?));
            }
            Ok(vm.compare(x, y))
        })
    }
}
