// vm::arith — split out of vm/mod.rs. `super::*` == the `vm` module.
// Arithmetic, comparison, ordering, hashing, equality.

use super::*;
use crate::ast::BinaryOp;
use crate::base::hooks::Hook;

impl Vm {
    // ===== 8-byte `Value` boxing helpers =====
    //
    // An i64 outside ±2^62 is heap-boxed as `Obj::BigInt` (an Obj-tagged `Value`); a float is
    // heap-boxed as `Obj::FloatBox` (a Float-tagged `Value`). These are the ONLY producers of
    // wide-int / float `Value`s and the canonical readers. The inline-int fast path stays first in
    // every reader so the int-heavy hot loops never touch the heap.

    /// Make an int `Value`: inline if in ±2^62, else box as `Obj::BigInt`. The single guard that
    /// keeps the canonical-representation invariant (an i64 is inline XOR boxed, never both).
    #[inline]
    pub(super) fn make_int(&mut self, n: i64) -> Value {
        if (Value::INT_MIN_INLINE..=Value::INT_MAX_INLINE).contains(&n) {
            Value::int(n)
        } else {
            Value::obj(self.heap.alloc(Obj::BigInt(n)))
        }
    }

    /// The i64 iff `v` is an int (inline `Int` OR boxed `BigInt`), else `None`.
    #[inline]
    pub(super) fn int_val(&self, v: Value) -> Option<i64> {
        if let Some(n) = v.as_int_inline() {
            Some(n)
        } else if v.is_obj() {
            if let Obj::BigInt(n) = self.heap.get(v.as_gcref()) {
                Some(*n)
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Is `v` any integer (inline or boxed big-int)?
    #[inline]
    pub(super) fn is_integral(&self, v: Value) -> bool {
        v.is_int() || (v.is_obj() && matches!(self.heap.get(v.as_gcref()), Obj::BigInt(_)))
    }

    /// Unwrap an int (inline or boxed). Panics on a non-int.
    #[inline]
    pub(super) fn int_of(&self, v: Value) -> i64 {
        self.int_val(v).expect("int_of on non-int")
    }

    /// Box a float as `Obj::FloatBox`, returning a Float-tagged `Value`.
    #[inline]
    pub(super) fn box_float(&mut self, f: f64) -> Value {
        Value::float_ref(self.heap.alloc(Obj::FloatBox(f)))
    }

    /// Unwrap a boxed float's f64. Panics on a non-float.
    #[inline]
    pub(super) fn float_of(&self, v: Value) -> f64 {
        debug_assert!(v.is_float(), "float_of on non-float");
        if let Obj::FloatBox(f) = self.heap.get(v.as_gcref()) {
            *f
        } else {
            unreachable!("float_of on non-FloatBox")
        }
    }

    /// Is `v` a number (int inline/boxed, or a boxed float)?
    #[inline]
    pub(super) fn is_numeric(&self, v: Value) -> bool {
        v.is_int() || v.is_float() || self.is_integral(v)
    }

    /// Coerce a numeric `Value` to f64 (int → f64, float → its f64). Panics on a non-numeric.
    #[inline]
    pub(super) fn as_f64(&self, v: Value) -> f64 {
        if v.is_float() {
            self.float_of(v)
        } else {
            self.int_of(v) as f64
        }
    }

    /// M19 Tier-2 — adaptive quickening for the un-fused generic arith/ordered-compare binops
    /// (`Add..GtEq`). `site` indexes [`Vm::quicken`]. Cold: observe the two stack operands' types once,
    /// then run the generic path. Int-specialized: take the `fast_int_bin` path (the exact int
    /// behaviour the superinstructions already ship), deopting to `Q_GENERIC` if a non-int operand
    /// shows up. Generic: always the unfused `arith`/`compare_op` via `run_bin_kind`. Every path
    /// produces a byte-identical result to the original `step` arm.
    /// `MakeCffi(id)` — eager `dlopen` + `dlsym` at module init from `Program.cffi_defs[id]`, then
    /// push the resolved `Obj::Cffi`. A missing library / symbol surfaces as a runtime error here
    /// (the spec's startup-failure model). `#[inline(never)]` keeps its locals (the cloned `CffiDef`'s
    /// `Vec`s) off `step`'s stack frame, preserving the deep-recursion depth-guard headroom.
    #[inline(never)]
    pub(super) fn op_make_cffi(&mut self, id: u32, span: Span) -> Result<(), RuntimeError> {
        let def = self.program.cffi_defs[id as usize].clone();
        let cffi = crate::native::cffi::Cffi::new(&def.lib, &def.name, def.params, def.ret)
            .map(|c| c.with_c_variadic(def.c_variadic))
            .map_err(|e| self.err(e.message, span))?;
        let cffi = std::sync::Arc::new(cffi);
        let key = FnKey::Cffi(std::sync::Arc::as_ptr(&cffi) as usize);
        let v = self.fn_value(key, || Obj::Cffi(cffi));
        self.push(v);
        Ok(())
    }

    #[inline(never)]
    pub(super) fn q_arith(
        &mut self,
        site: usize,
        kind: crate::base::op::BinKind,
        span: Span,
    ) -> Result<(), RuntimeError> {
        match self.quicken[site] {
            Q_INT => {
                let n = self.stack.len();
                if let (Some(x), Some(y)) = (
                    self.stack[n - 2].as_int_inline(),
                    self.stack[n - 1].as_int_inline(),
                ) {
                    let v = self.fast_int_bin(x, y, kind, span)?;
                    self.stack.truncate(n - 2);
                    self.stack.push(v);
                    Ok(())
                } else {
                    // A non-int operand reached a specialized site — deopt permanently (operands stay
                    // on the stack for the generic path to pop).
                    self.quicken[site] = Q_GENERIC;
                    self.run_bin_kind(kind, span)
                }
            }
            Q_GENERIC => self.run_bin_kind(kind, span),
            _ => {
                // Q_COLD — record whether this site is int/int, then run the generic path this once.
                let n = self.stack.len();
                let both_int = self.stack[n - 2].as_int_inline().is_some()
                    && self.stack[n - 1].as_int_inline().is_some();
                self.quicken[site] = if both_int { Q_INT } else { Q_GENERIC };
                self.run_bin_kind(kind, span)
            }
        }
    }

    /// M19 Tier-2 — adaptive quickening for `Eq`/`NotEq` (never fused, so always reached here). The
    /// int fast path uses EXACT `x == y` (i64), matching `values_equal_guarded`'s exact `(Int,Int)`
    /// arm. (It formerly replicated the
    /// lossy `as_f64(x) == as_f64(y)`, which wrongly equated distinct ints above 2^53.) `negate` flips
    /// the result for `NotEq`. The generic path is [`Self::eq_operator`], shared with the kept
    /// `Op::Eq`/`Op::NotEq` `step` arms. A struct/enum operand fails `as_int_inline`, so a site warmed
    /// to `Q_INT` DEOPTS to `Q_GENERIC` and falls through — the fast path needs no `Eq`-protocol arm.
    #[inline(never)]
    pub(super) fn q_eq(
        &mut self,
        site: usize,
        negate: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if self.quicken[site] == Q_INT {
            let n = self.stack.len();
            if let (Some(x), Some(y)) = (
                self.stack[n - 2].as_int_inline(),
                self.stack[n - 1].as_int_inline(),
            ) {
                self.stack.truncate(n - 2);
                let eq = x == y;
                self.push(Value::bool(eq ^ negate));
                return Ok(());
            }
            self.quicken[site] = Q_GENERIC; // non-int at a specialized site → deopt
        } else if self.quicken[site] == Q_COLD {
            let n = self.stack.len();
            let both_int = self.stack[n - 2].as_int_inline().is_some()
                && self.stack[n - 1].as_int_inline().is_some();
            self.quicken[site] = if both_int { Q_INT } else { Q_GENERIC };
            // fall through to the generic path this first time
        }
        self.eq_operator(negate, span)
    }

    /// The `==` / `!=` OPERATOR entry (`negate` flips it to `!=`). Pops `[l, r]`, pushes a `Bool`.
    ///
    /// `Eq` protocol (M23): two operands of the SAME struct/enum type whose type defines
    /// `eq(self, o: Self) -> bool` dispatch to that method; everything else keeps the structural
    /// worker. The dispatch itself lives in [`Self::values_equal_guarded`] — the ONE place every
    /// consumer (this operator, `in`, map/set probing, the recursive container arms) routes through.
    ///
    /// The popped operands are rooted across the call: a user `eq` allocates, and `l`/`r` are off the
    /// operand stack from here on.
    #[inline(never)]
    pub(super) fn eq_operator(&mut self, negate: bool, span: Span) -> Result<(), RuntimeError> {
        let r = self.pop();
        let l = self.pop();
        // A top-level hooked `==` runs its `eq` as a frame (the operands are its locals); a nested
        // container walk keeps the synchronous hook call in `values_equal_guarded`.
        if !self.eq_hook_off
            && self.eq_hook_applies(l, r)
            && self.push_hook_frame(Hook::Eq, l, &[r], RetAction::EqBool { negate }, span)?
        {
            return Ok(());
        }
        let held = [l, r];
        let roots: &[Value] = if self.eq_may_reenter() { &held } else { &[] };
        let eq = self.with_roots(roots, |vm| vm.values_equal_guarded(l, r, 0, span))?;
        self.push(Value::bool(eq ^ negate));
        Ok(())
    }

    /// TICKET-250 — do `l` and `r` share one declared type that has the `Eq` hook? The same-type
    /// guard is read from the operands: one `tid` for structs, one enum for enum values (a hook
    /// also decides `Shape.Circle == Shape.Square`, as Rust `PartialEq` and Python `__eq__` do).
    /// This runs on the MISS path of every struct/enum `==`, where the answer is almost always
    /// "no": one flag test, then one dense table index.
    pub(super) fn eq_hook_applies(&self, l: Value, r: Value) -> bool {
        if !self.program.any_eq_hook || !self.hook_of(l, Hook::Eq) {
            return false;
        }
        let (Some(hl), Some(hr)) = (l.as_obj(), r.as_obj()) else {
            return false;
        };
        match (self.heap.get(hl), self.heap.get(hr)) {
            (Obj::Struct { tid: a, .. }, Obj::Struct { tid: b, .. }) => a == b,
            (Obj::Enum { variant_id: a, .. }, Obj::Enum { variant_id: b, .. }) => {
                self.enum_names(*a).0 == self.enum_names(*b).0
            }
            _ => false,
        }
    }

    /// `BinLocalLocal{a,b,kind}` — push `local[a] <op> local[b]`. `#[inline(never)]` keeps the body
    /// out of `step`'s frame (see the call site).
    #[inline(never)]
    pub(super) fn op_bin_local_local(
        &mut self,
        a: usize,
        b: usize,
        kind: crate::base::op::BinKind,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let l = self.stack[self.base() + a];
        let r = self.stack[self.base() + b];
        if let (Some(x), Some(y)) = (l.as_int_inline(), r.as_int_inline()) {
            let v = self.fast_int_bin(x, y, kind, span)?;
            self.push(v);
        } else {
            self.push(l);
            self.push(r);
            self.run_bin_kind(kind, span)?;
        }
        Ok(())
    }

    /// `BinLocalConst{slot,val,kind}` — push `local[slot] <op> val`.
    #[inline(never)]
    pub(super) fn op_bin_local_const(
        &mut self,
        slot: usize,
        val: i64,
        kind: crate::base::op::BinKind,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let l = self.stack[self.base() + slot];
        if let Some(x) = l.as_int_inline() {
            let v = self.fast_int_bin(x, val, kind, span)?;
            self.push(v);
        } else {
            self.push(l);
            let cv = self.make_int(val); // `val` is an unbounded fused literal → box if wide.
            self.push(cv);
            self.run_bin_kind(kind, span)?;
        }
        Ok(())
    }

    /// `IncLocal{slot,delta}` — in-place `local[slot] += delta`. Falls back to the exact unfused
    /// `GetLocal; ConstInt; Add; SetLocal` for a non-numeric local (so `arith`'s error wins).
    #[inline(never)]
    pub(super) fn op_inc_local(
        &mut self,
        slot: usize,
        delta: i64,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let at = self.base() + slot;
        let cur = self.stack[at];
        if let Some(x) = cur.as_int_inline() {
            let v = self.int_op(BinaryOp::Add, x, delta, span)?;
            self.stack[at] = self.make_int(v);
        } else if cur.is_float() {
            let f = self.float_of(cur);
            self.stack[at] = self.box_float(f + delta as f64);
        } else {
            // A boxed big-int local, or a non-numeric — route through the exact `arith` path.
            self.push(cur);
            let dv = self.make_int(delta); // `delta` is an unbounded fused literal → box if wide.
            self.push(dv);
            self.arith(&Op::Add, false, span)?;
            let v = self.pop();
            let at = self.base() + slot;
            self.stack[at] = v;
        }
        Ok(())
    }

    /// One int arithmetic or bitwise op through its owner, `consteval::int_binop`: the value, or
    /// the op's fault as a run-time error at `span`.
    #[inline(always)]
    pub(super) fn int_op(
        &self,
        op: BinaryOp,
        a: i64,
        b: i64,
        span: Span,
    ) -> Result<i64, RuntimeError> {
        match crate::ast::consteval::int_binop(op, a, b) {
            Some(Ok(n)) => Ok(n),
            Some(Err(f)) => Err(self.err(crate::base::hooks::int_fault_message(f), span)),
            None => unreachable!("{op:?} is not int arithmetic"),
        }
    }

    /// Int/Int fast path for the fused binops (`BinLocalLocal` / `BinLocalConst`). Must match
    /// `arith` (overflow / div-by-zero errors) and `compare_op` (ordering) for `Int` operands
    /// exactly. Anything non-`Int` never reaches here — the caller falls back to the slow path.
    pub(super) fn fast_int_bin(
        &mut self,
        x: i64,
        y: i64,
        kind: crate::base::op::BinKind,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        use crate::base::op::BinKind;
        // Arithmetic is computed in true i64 (overflow still faults past i64), then `make_int` boxes
        // any result outside ±2^62 — the inline↔box fork, invisible to the program.
        let v = match kind {
            BinKind::Add => self.make_int(self.int_op(BinaryOp::Add, x, y, span)?),
            BinKind::Sub => self.make_int(self.int_op(BinaryOp::Sub, x, y, span)?),
            BinKind::Mul => self.make_int(self.int_op(BinaryOp::Mul, x, y, span)?),
            BinKind::Div => self.make_int(self.int_op(BinaryOp::Div, x, y, span)?),
            BinKind::Mod => self.make_int(self.int_op(BinaryOp::Mod, x, y, span)?),
            BinKind::Lt => Value::bool(x < y),
            BinKind::LtEq => Value::bool(x <= y),
            BinKind::Gt => Value::bool(x > y),
            BinKind::GtEq => Value::bool(x >= y),
        };
        Ok(v)
    }

    /// Slow-path dispatch for a fused binop: the two operands are already on the stack, so route to
    /// the existing `arith` / `compare_op` (preserving struct overloading, string concat, float
    /// promotion, and fiber parking — anything the unfused op sequence would do).
    pub(super) fn run_bin_kind(
        &mut self,
        kind: crate::base::op::BinKind,
        span: Span,
    ) -> Result<(), RuntimeError> {
        use crate::base::op::BinKind;
        match kind {
            BinKind::Add => self.arith(&Op::Add, true, span),
            BinKind::Sub => self.arith(&Op::Sub, true, span),
            BinKind::Mul => self.arith(&Op::Mul, true, span),
            BinKind::Div => self.arith(&Op::Div, true, span),
            BinKind::Mod => self.arith(&Op::Mod, true, span),
            BinKind::Lt => self.compare_op(&Op::Lt, span),
            BinKind::LtEq => self.compare_op(&Op::LtEq, span),
            BinKind::Gt => self.compare_op(&Op::Gt, span),
            BinKind::GtEq => self.compare_op(&Op::GtEq, span),
        }
    }

    /// `flat`: the caller is the dispatch loop's step (a hook runs as a frame, see
    /// [`Vm::hook_op`]); a native caller that needs the value now passes `false`.
    pub(super) fn arith(&mut self, op: &Op, flat: bool, span: Span) -> Result<(), RuntimeError> {
        let r = self.pop();
        let l = self.pop();
        let bop = match op {
            Op::Add => BinaryOp::Add,
            Op::Sub => BinaryOp::Sub,
            Op::Mul => BinaryOp::Mul,
            Op::Div => BinaryOp::Div,
            Op::Mod => BinaryOp::Mod,
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
            | Op::Neg
            | Op::Not
            | Op::Lt
            | Op::LtEq
            | Op::Gt
            | Op::GtEq
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
        let name = Hook::of_binary(bop)
            .expect("an arithmetic op has a hook")
            .protocol();
        let result = if self.is_integral(l) && self.is_integral(r) {
            let (a, b) = (self.int_of(l), self.int_of(r));
            self.make_int(self.int_op(bop, a, b, span)?)
        } else if self.is_numeric(l) && self.is_numeric(r) {
            let (x, y) = (self.as_f64(l), self.as_f64(r));
            // Float arithmetic is total IEEE-754: division/modulo by zero yields inf/-inf/NaN,
            // never a fault. (The INT arm above still faults on /0 and overflow.)
            let f = match op {
                Op::Add => x + y,
                Op::Sub => x - y,
                Op::Mul => x * y,
                Op::Div => x / y,
                Op::Mod => x % y,
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
                | Op::Neg
                | Op::Not
                | Op::Lt
                | Op::LtEq
                | Op::Gt
                | Op::GtEq
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
            self.box_float(f)
        } else {
            match (l.view(), r.view()) {
                // Arithmetic overloading: `+`/`-`/`*` on two structs (or two enums) dispatch to
                // `add`/`sub`/`mul` (the `Add`/`Sub`/`Mul` protocols). The checker has verified
                // conformance. Must precede the string-concat `Add` arm below (which would otherwise
                // reject struct+struct).
                (ValueView::Obj(ha), ValueView::Obj(hb))
                    if matches!(op, Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod)
                        && matches!(self.heap.get(ha), Obj::Struct { .. } | Obj::Enum { .. })
                        && matches!(self.heap.get(hb), Obj::Struct { .. } | Obj::Enum { .. }) =>
                {
                    return self.struct_arith(op, l, r, flat, span);
                }
                (ValueView::Obj(ha), ValueView::Obj(hb)) if matches!(op, Op::Add) => {
                    match (self.heap.get(ha), self.heap.get(hb)) {
                        (Obj::Str(a), Obj::Str(b)) => {
                            let s = format!("{a}{b}");
                            let h = self.heap.alloc(Obj::Str(s.into()));
                            Value::obj(h)
                        }
                        // bytes / bytearray concat: the left operand's type (CPython).
                        (Obj::Bytes(a), Obj::Bytes(b)) => {
                            let out: Box<[u8]> = a.iter().chain(b.iter()).copied().collect();
                            Value::obj(self.heap.alloc(Obj::Bytes(out)))
                        }
                        (Obj::ByteArray(a), Obj::ByteArray(b)) => {
                            let out: RVec<u8> = a.iter().chain(b.iter()).collect();
                            Value::obj(self.heap.alloc(Obj::ByteArray(out)))
                        }
                        (Obj::ByteArray(a), Obj::Bytes(b)) => {
                            let out: RVec<u8> = a.iter().chain(b.iter().copied()).collect();
                            Value::obj(self.heap.alloc(Obj::ByteArray(out)))
                        }
                        (Obj::Bytes(a), Obj::ByteArray(b)) => {
                            let out: Box<[u8]> = a.iter().copied().chain(b.iter()).collect();
                            Value::obj(self.heap.alloc(Obj::Bytes(out)))
                        }
                        // List concat (gap #3): `[1,2] + [3,4]` — identical to `.concat` (vm:7688).
                        (Obj::List(a), Obj::List(b)) => {
                            let mut out = a.to_vec();
                            out.extend(b.iter());
                            Value::obj(self.heap.alloc(Obj::List(out.into())))
                        }
                        _ => {
                            return Err(self.err(
                                format!(
                                    "cannot apply {name} to {} and {}",
                                    self.type_name(l),
                                    self.type_name(r)
                                ),
                                span,
                            ));
                        }
                    }
                }
                // Set difference (gap #3): `a - b` — identical to `.difference` (vm:7918).
                (ValueView::Obj(ha), ValueView::Obj(hb))
                    if matches!(op, Op::Sub)
                        && matches!(self.heap.get(ha), Obj::Set(_))
                        && matches!(self.heap.get(hb), Obj::Set(_)) =>
                {
                    self.set_op(SetOp::Difference, ha, hb, span)?
                }
                _ => {
                    return Err(self.err(
                        format!(
                            "cannot apply {name} to {} and {}",
                            self.type_name(l),
                            self.type_name(r)
                        ),
                        span,
                    ));
                }
            }
        };
        self.push(result);
        Ok(())
    }

    /// Unary `-v` — the single source of truth for [`Op::Neg`] AND for the intrinsic `Neg` protocol
    /// method (`v.neg()` in an erased `[T: Neg]` body, dispatched by `Vm::intrinsic_proto_method`).
    /// Extracted verbatim from the `Op::Neg` handler so the two forms are observationally identical:
    /// same `integer overflow in negation`, same `-0.0` float behavior, same `cannot apply Neg to X`,
    /// same struct/enum `neg(self)` overload dispatch.
    /// Pushes the result; `flat` as for [`Vm::arith`].
    pub(super) fn neg_value(
        &mut self,
        v: Value,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if let Some(n) = self.int_val(v) {
            let neg = crate::ast::consteval::int_neg(n)
                .map_err(|f| self.err(crate::base::hooks::int_fault_message(f), span))?;
            let r = self.make_int(neg);
            self.push(r);
            Ok(())
        } else if v.is_float() {
            let f = self.float_of(v);
            let r = self.box_float(-f);
            self.push(r);
            Ok(())
        } else if let Some(h) = v.as_obj()
            && matches!(self.heap.get(h), Obj::Struct { .. } | Obj::Enum { .. })
        {
            // M22: unary `-` on a struct/enum dispatches to its `neg(self) -> Self` method
            // (the `Neg` protocol). Mirrors `struct_arith`, but self-only (no `other`).
            if self.hook_op(flat, Hook::Neg, v, &[], RetAction::Push, span)? {
                Ok(())
            } else {
                Err(self.missing_hook(v, Hook::Neg, span))
            }
        } else {
            Err(self.err(format!("cannot apply Neg to {}", self.type_name(v)), span))
        }
    }

    /// Set algebra for the operator forms `| & - ^` (gap #3). Mirrors the
    /// `union`/`intersection`/`difference` set methods (vm:7918) using the cached per-element
    /// hashes (no re-hashing; membership may still dispatch a user `eq`). `^` (symmetric-difference) has no method form:
    /// it is the union of (mine ∉ other) THEN (other ∉ mine), in that canonical insertion order so
    /// the result's print order is deterministic.
    pub(super) fn set_op(
        &mut self,
        op: SetOp,
        ha: GcRef,
        hb: GcRef,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let mine = match self.heap.get(ha).as_set() {
            Some(s) => s.entries.to_vec(),
            None => unreachable!(),
        };
        let other = match self.heap.get(hb).as_set() {
            Some(s) => s.entries.to_vec(),
            None => unreachable!(),
        };
        // Dedup-insert: propagate a cyclic-key depth fault (`?`) instead of swallowing it — the
        // membership `==` here is the same one `s.add`/`|` are DEFINED by, so it must fault alike.
        let add = |vm: &mut Vm, set: &SetData, he: u64, e: Value| -> Result<(), RuntimeError> {
            if vm
                .set_slot(&set.entries, set.candidates(he), e, span)?
                .is_none()
            {
                set.push(&vm.heap, he, e);
            }
            Ok(())
        };
        let in_set =
            |vm: &mut Vm, set: &[(u64, Value)], he: u64, e: Value| -> Result<bool, RuntimeError> {
                for &(h2, e2) in set {
                    if h2 == he && vm.elem_equal(e2, e, 0, span)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            };
        // Root the source ELEMENTS, not just the two source sets: `mine`/`other`/`out` are Rust
        // locals, and a user `eq` that empties `ha`/`hb` mid-walk orphans every element the locals
        // still hold (rooting the containers alone would not keep them alive). `out` only ever holds
        // elements of `mine`/`other`, so it is covered.
        let mut elems: Vec<Value> = vec![Value::obj(ha), Value::obj(hb)];
        if self.eq_may_reenter() {
            elems.extend(mine.iter().chain(other.iter()).map(|&(_, e)| e));
        }
        let out = self.with_roots(&elems, |vm| {
            let out = SetData::default();
            match op {
                SetOp::Union => {
                    for (he, e) in mine.iter().chain(other.iter()) {
                        add(vm, &out, *he, *e)?;
                    }
                }
                SetOp::Intersection => {
                    for (he, e) in &mine {
                        if in_set(vm, &other, *he, *e)? {
                            add(vm, &out, *he, *e)?;
                        }
                    }
                }
                SetOp::Difference => {
                    for (he, e) in &mine {
                        if !in_set(vm, &other, *he, *e)? {
                            add(vm, &out, *he, *e)?;
                        }
                    }
                }
                SetOp::SymmetricDifference => {
                    for (he, e) in &mine {
                        if !in_set(vm, &other, *he, *e)? {
                            add(vm, &out, *he, *e)?;
                        }
                    }
                    for (he, e) in &other {
                        if !in_set(vm, &mine, *he, *e)? {
                            add(vm, &out, *he, *e)?;
                        }
                    }
                }
            }
            Ok(out)
        })?;
        Ok(Value::obj(self.heap.alloc(Obj::Set(out))))
    }

    /// Arithmetic operator overloading: dispatch `+`/`-`/`*` on two structs to the receiver's
    /// `add`/`sub`/`mul(self, other) -> Self` method (the `Add`/`Sub`/`Mul` protocols). `l`/`r` are
    /// passed as the call's args (rooted as the new frame's locals).
    /// Pushes the result; `flat` as for [`Vm::arith`].
    pub(super) fn struct_arith(
        &mut self,
        op: &Op,
        l: Value,
        r: Value,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let hook = match op {
            Op::Add => Hook::Add,
            Op::Sub => Hook::Sub,
            Op::Mul => Hook::Mul,
            Op::Div => Hook::Div,
            Op::Mod => Hook::Mod,
            Op::BitAnd => Hook::BitAnd,
            Op::BitOr => Hook::BitOr,
            Op::BitXor => Hook::BitXor,
            Op::Shl => Hook::Shl,
            Op::Shr => Hook::Shr,
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
            | Op::Neg
            | Op::Not
            | Op::Lt
            | Op::LtEq
            | Op::Gt
            | Op::GtEq
            | Op::Eq
            | Op::NotEq
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
            | Op::NewExecutorN => unreachable!("struct_arith only handles + - * / %"),
        };
        if self.hook_op(flat, hook, l, &[r], RetAction::Push, span)? {
            Ok(())
        } else {
            Err(self.missing_hook(l, hook, span))
        }
    }

    /// Bitwise / shift ops — int-only (gap #13). Shift amounts outside `0..64` are a runtime error
    /// (Rust would otherwise panic).
    /// `flat` as for [`Vm::arith`].
    pub(super) fn bitwise(&mut self, op: &Op, flat: bool, span: Span) -> Result<(), RuntimeError> {
        let r = self.pop();
        let l = self.pop();
        let bop = match op {
            Op::BitAnd => BinaryOp::BitAnd,
            Op::BitOr => BinaryOp::BitOr,
            Op::BitXor => BinaryOp::BitXor,
            Op::Shl => BinaryOp::Shl,
            Op::Shr => BinaryOp::Shr,
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
            | Op::Lt
            | Op::LtEq
            | Op::Gt
            | Op::GtEq
            | Op::Eq
            | Op::NotEq
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
        let name = Hook::of_binary(bop)
            .expect("a bitwise op has a hook")
            .protocol();
        let result = if self.is_integral(l) && self.is_integral(r) {
            let (a, b) = (self.int_of(l), self.int_of(r));
            let v = self.int_op(bop, a, b, span)?;
            self.make_int(v)
        } else {
            match (l.view(), r.view()) {
                // Bit-operator overloading: two structs (or enums) dispatch to `bitand`/`bitor`/
                // `bitxor`/`shl`/`shr` (the `BitAnd`..`Shr` protocols); the checker verified it.
                (ValueView::Obj(ha), ValueView::Obj(hb))
                    if matches!(self.heap.get(ha), Obj::Struct { .. } | Obj::Enum { .. })
                        && matches!(self.heap.get(hb), Obj::Struct { .. } | Obj::Enum { .. }) =>
                {
                    return self.struct_arith(op, l, r, flat, span);
                }
                // Set algebra (gap #3): `|`→union, `&`→intersection, `^`→symmetric-difference on two
                // sets. (`<< >>` stay int-only and fall through to the error below.) Identical to the
                // `.union`/`.intersection` methods; `^` has no method form. Mirrors interp.
                (ValueView::Obj(ha), ValueView::Obj(hb))
                    if matches!(op, Op::BitOr | Op::BitAnd | Op::BitXor)
                        && matches!(self.heap.get(ha), Obj::Set(_))
                        && matches!(self.heap.get(hb), Obj::Set(_)) =>
                {
                    let set_op = match op {
                        Op::BitOr => SetOp::Union,
                        Op::BitAnd => SetOp::Intersection,
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
                        | Op::Lt
                        | Op::LtEq
                        | Op::Gt
                        | Op::GtEq
                        | Op::Eq
                        | Op::NotEq
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
                        | Op::NewExecutorN => SetOp::SymmetricDifference,
                    };
                    self.set_op(set_op, ha, hb, span)?
                }
                _ => {
                    return Err(self.err(
                        format!(
                            "cannot apply {name} to {} and {}",
                            self.type_name(l),
                            self.type_name(r)
                        ),
                        span,
                    ));
                }
            }
        };
        self.push(result);
        Ok(())
    }
}

/// The ONE float ordering behind `sort`, `sort_by_key`, `min`, `max`, `min_by` and `max_by`:
/// `f64::total_cmp` (NaN deterministically at one end) except that `+0.0` and `-0.0` are EQUAL,
/// like `==` and like CPython/Rust `partial_cmp`. `total_cmp` alone ranks `-0.0 < +0.0`, which
/// broke sort stability and first-wins `min`/`max` (`[0.0, -0.0].min()` is `0.0` in CPython). Do not
/// put a bare `total_cmp` back at a sort site.
pub(super) fn float_order(a: f64, b: f64) -> std::cmp::Ordering {
    if a == 0.0 && b == 0.0 {
        std::cmp::Ordering::Equal
    } else {
        a.total_cmp(&b)
    }
}

/// Exact `int` vs `float` comparison, CPython's `float_richcompare` rule: no int-to-f64 coercion,
/// so no i64 above 2^53 collapses onto another. The `w >= 2^63`/`w < -2^63` tests MUST precede the
/// `floor(w) as i64` cast: a Rust float-to-int cast saturates, so `9223372036854775808.0 as i64` is
/// `i64::MAX`, and a cast-first version would wrongly answer `i64::MAX == 2^63` as equal. `-2^63`
/// stays on the exact path below: `i64::MIN` is exactly representable as an f64.
pub(super) fn cmp_int_f64(v: i64, w: f64) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    if w.is_nan() {
        return None;
    }
    if w >= 9223372036854775808.0 {
        return Some(Ordering::Less);
    }
    if w < -9223372036854775808.0 {
        return Some(Ordering::Greater);
    }
    let fl = w.floor();
    let fi = fl as i64;
    Some(match v.cmp(&fi) {
        Ordering::Equal if w > fl => Ordering::Less,
        other @ Ordering::Less | other @ Ordering::Equal | other @ Ordering::Greater => other,
    })
}
