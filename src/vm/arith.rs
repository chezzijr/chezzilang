// vm::arith — split out of vm/mod.rs. `super::*` == the `vm` module.
// Arithmetic, comparison, ordering, hashing, equality.

use super::*;

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
            .map_err(|e| self.err(e.message, span))?;
        let h = self.heap.alloc(Obj::Cffi(std::sync::Arc::new(cffi)));
        self.push(Value::obj(h));
        Ok(())
    }

    #[inline(never)]
    pub(super) fn q_arith(
        &mut self,
        site: usize,
        kind: crate::vm::op::BinKind,
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
    /// arm — both engines run this same code, so two-engine parity holds. (It formerly replicated the
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
        let held = [l, r];
        let roots: &[Value] = if self.eq_may_reenter() { &held } else { &[] };
        let eq = self.with_roots(roots, |vm| vm.values_equal_guarded(l, r, 0, span))?;
        self.push(Value::bool(eq ^ negate));
        Ok(())
    }

    /// Does `l == r` dispatch to a user `eq(self, o: Self) -> bool`? `Some((proto, home))` only when
    /// both operands are the SAME struct/enum type AND that type declares the `Eq` HOOK; a mismatched
    /// pair is `None` WITHOUT calling user code.
    ///
    /// The hook is looked up in [`Program::eq_struct`] / [`Program::eq_enum`] — dense tables indexed by
    /// the ids the operands already carry — NOT by a `"eq"` name lookup in the method map. Two reasons,
    /// both load-bearing:
    ///
    /// * **Correctness.** A method merely NAMED `eq` is not the hook: `Opt[T].eq(self, x: T)` is an
    ///   ordinary method (the operand is a type parameter, not `Self`), and dispatching `==` to it
    ///   answered a silently wrong `true`. The compiler decides hook-vs-ordinary from the declaration
    ///   (`binds_eq_hook`), after the checker has rejected every other shape, so the operator can only
    ///   ever reach a real `(self, Self) -> bool`.
    /// * **Cost.** This runs on the MISS path of every struct/enum `==` — including every `Option` /
    ///   `Result` compare — where the answer is almost always "no". A table index beats hashing `"eq"`.
    fn user_eq_method(&self, l: Value, r: Value) -> Option<(usize, GcRef)> {
        let (hl, hr) = (l.as_obj()?, r.as_obj()?);
        match (self.heap.get(hl), self.heap.get(hr)) {
            (Obj::Struct { tid: a, .. }, Obj::Struct { tid: b, .. }) if a == b => {
                let (proto, home) = (*self.program.eq_struct.get(*a as usize)?)?;
                Some((proto, self.module_objs[home]))
            }
            // Keyed on the ENUM, not the variant: one type's `eq` also decides `Shape.Circle ==
            // Shape.Square` (Rust `PartialEq` / Python `__eq__` compare ACROSS variants), so a
            // `variant_id` equality guard would be too narrow and silently keep the structural
            // `false` for every cross-variant pair. Equal entries ⇒ same enum (a hook proto belongs to
            // exactly one enum), so this compare IS the same-type guard the struct arm gets from `tid`.
            (Obj::Enum { variant_id: a, .. }, Obj::Enum { variant_id: b, .. }) => {
                let hook = (*self.program.eq_enum.get(*a as usize)?)?;
                if Some(hook) != *self.program.eq_enum.get(*b as usize)? {
                    return None;
                }
                Some((hook.0, self.module_objs[hook.1]))
            }
            _ => None,
        }
    }

    /// `BinLocalLocal{a,b,kind}` — push `local[a] <op> local[b]`. `#[inline(never)]` keeps the body
    /// out of `step`'s frame (see the call site).
    #[inline(never)]
    pub(super) fn op_bin_local_local(
        &mut self,
        a: usize,
        b: usize,
        kind: crate::vm::op::BinKind,
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
        kind: crate::vm::op::BinKind,
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
            let v = x
                .checked_add(delta)
                .ok_or_else(|| self.err("integer overflow in Add".to_string(), span))?;
            self.stack[at] = self.make_int(v);
        } else if cur.is_float() {
            let f = self.float_of(cur);
            self.stack[at] = self.box_float(f + delta as f64);
        } else {
            // A boxed big-int local, or a non-numeric — route through the exact `arith` path.
            self.push(cur);
            let dv = self.make_int(delta); // `delta` is an unbounded fused literal → box if wide.
            self.push(dv);
            self.arith(&Op::Add, span)?;
            let v = self.pop();
            let at = self.base() + slot;
            self.stack[at] = v;
        }
        Ok(())
    }

    /// Int/Int fast path for the fused binops (`BinLocalLocal` / `BinLocalConst`). Must match
    /// `arith` (overflow / div-by-zero errors) and `compare_op` (ordering) for `Int` operands
    /// exactly. Anything non-`Int` never reaches here — the caller falls back to the slow path.
    pub(super) fn fast_int_bin(
        &mut self,
        x: i64,
        y: i64,
        kind: crate::vm::op::BinKind,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        use crate::vm::op::BinKind;
        // Arithmetic is computed in true i64 (overflow still faults past i64), then `make_int` boxes
        // any result outside ±2^62 — the inline↔box fork, invisible to the program.
        let v = match kind {
            BinKind::Add => {
                let n = x
                    .checked_add(y)
                    .ok_or_else(|| self.err("integer overflow in Add".to_string(), span))?;
                self.make_int(n)
            }
            BinKind::Sub => {
                let n = x
                    .checked_sub(y)
                    .ok_or_else(|| self.err("integer overflow in Sub".to_string(), span))?;
                self.make_int(n)
            }
            BinKind::Mul => {
                let n = x
                    .checked_mul(y)
                    .ok_or_else(|| self.err("integer overflow in Mul".to_string(), span))?;
                self.make_int(n)
            }
            BinKind::Div => {
                if y == 0 {
                    return Err(self.err("division by zero".to_string(), span));
                }
                let n = x
                    .checked_div(y)
                    .ok_or_else(|| self.err("integer overflow in Div".to_string(), span))?;
                self.make_int(n)
            }
            BinKind::Mod => {
                if y == 0 {
                    return Err(self.err("modulo by zero".to_string(), span));
                }
                // `wrapping_rem` == `checked_rem` for every input except `MIN % -1`, where the true
                // mathematical remainder IS `0` (representable) — Rust's `checked_rem` returns `None`
                // there only to dodge the x86 `IDIV` hardware trap, not because the answer overflows.
                // Unlike `Div`, `%` never overflows past `b == 0`.
                self.make_int(x.wrapping_rem(y))
            }
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
        kind: crate::vm::op::BinKind,
        span: Span,
    ) -> Result<(), RuntimeError> {
        use crate::vm::op::BinKind;
        match kind {
            BinKind::Add => self.arith(&Op::Add, span),
            BinKind::Sub => self.arith(&Op::Sub, span),
            BinKind::Mul => self.arith(&Op::Mul, span),
            BinKind::Div => self.arith(&Op::Div, span),
            BinKind::Mod => self.arith(&Op::Mod, span),
            BinKind::Lt => self.compare_op(&Op::Lt, span),
            BinKind::LtEq => self.compare_op(&Op::LtEq, span),
            BinKind::Gt => self.compare_op(&Op::Gt, span),
            BinKind::GtEq => self.compare_op(&Op::GtEq, span),
        }
    }

    pub(super) fn arith(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        let r = self.pop();
        let l = self.pop();
        let name = match op {
            Op::Add => "Add",
            Op::Sub => "Sub",
            Op::Mul => "Mul",
            Op::Div => "Div",
            Op::Mod => "Mod",
            _ => unreachable!(),
        };
        let result = if self.is_integral(l) && self.is_integral(r) {
            let (a, b) = (self.int_of(l), self.int_of(r));
            let v = match op {
                Op::Add => a.checked_add(b),
                Op::Sub => a.checked_sub(b),
                Op::Mul => a.checked_mul(b),
                Op::Div | Op::Mod if b == 0 => {
                    return Err(self.err(
                        format!(
                            "{} by zero",
                            if matches!(op, Op::Div) {
                                "division"
                            } else {
                                "modulo"
                            }
                        ),
                        span,
                    ));
                }
                Op::Div => a.checked_div(b),
                // `wrapping_rem` never overflows past the `b == 0` guard above (see `fast_int_bin`'s
                // `BinKind::Mod` arm) — wrap in `Some` so this shared overflow check never fires for Mod.
                Op::Mod => Some(a.wrapping_rem(b)),
                _ => unreachable!(),
            };
            let n = v.ok_or_else(|| self.err(format!("integer overflow in {name}"), span))?;
            self.make_int(n)
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
                _ => unreachable!(),
            };
            self.box_float(f)
        } else {
            match (l.view(), r.view()) {
                // Same-newtype arithmetic: `Meters + Meters` etc. UNWRAPS both wrappers, runs the
                // underlying's NATIVE primitive op (identical overflow/div-by-zero/float semantics — it
                // recurses through `self.binary` on the inners), then REWRAPS in the same newtype. This
                // is NOT a user `add` method — it is the underlying's own op (distinct from struct
                // overloading). The checker has rejected `Meters + float` / `Meters + Seconds`, so a
                // mismatched pair never reaches here from typechecked code. Must precede struct_arith.
                (ValueView::Obj(ha), ValueView::Obj(hb))
                    if matches!(op, Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod)
                        && self.same_newtype_keys(ha, hb) =>
                {
                    self.newtype_arith(op, ha, hb, name, span)?
                }
                // Arithmetic overloading: `+`/`-`/`*` on two structs (or two enums) dispatch to
                // `add`/`sub`/`mul` (the `Add`/`Sub`/`Mul` protocols). The checker has verified
                // conformance. Must precede the string-concat `Add` arm below (which would otherwise
                // reject struct+struct).
                (ValueView::Obj(ha), ValueView::Obj(hb))
                    if matches!(op, Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod)
                        && matches!(self.heap.get(ha), Obj::Struct { .. } | Obj::Enum { .. })
                        && matches!(self.heap.get(hb), Obj::Struct { .. } | Obj::Enum { .. }) =>
                {
                    self.struct_arith(op, l, r, span)?
                }
                (ValueView::Obj(ha), ValueView::Obj(hb)) if matches!(op, Op::Add) => {
                    match (self.heap.get(ha), self.heap.get(hb)) {
                        (Obj::Str(a), Obj::Str(b)) => {
                            let s = format!("{a}{b}");
                            let h = self.heap.alloc(Obj::Str(s.into()));
                            Value::obj(h)
                        }
                        // List concat (gap #3): `[1,2] + [3,4]` — identical to `.concat` (vm:7688).
                        (Obj::List(a), Obj::List(b)) => {
                            let mut out = a.clone();
                            out.extend(b.iter().copied());
                            Value::obj(self.heap.alloc(Obj::List(out)))
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
                // List repeat (gap #3): `[0] * 3` / `3 * [0]` (commutative, Python-style). `n <= 0` →
                // empty; guard capacity against the Vec overflow abort, like `str.repeat` (vm:7514).
                (ValueView::Obj(ha), ValueView::Int(n))
                | (ValueView::Int(n), ValueView::Obj(ha))
                    if matches!(op, Op::Mul) && matches!(self.heap.get(ha), Obj::List(_)) =>
                {
                    self.list_repeat(ha, n, span)?
                }
                // List repeat with a boxed (>2^62) count: the count boxes as `Obj::BigInt`, so it
                // arrives as `Obj` not `Int`. Route it to `list_repeat` (its capacity guard faults
                // with "list repeat capacity overflow") instead of the generic "cannot apply Mul".
                (ValueView::Obj(ha), ValueView::Obj(hb))
                    if matches!(op, Op::Mul)
                        && ((matches!(self.heap.get(ha), Obj::List(_))
                            && matches!(self.heap.get(hb), Obj::BigInt(_)))
                            || (matches!(self.heap.get(hb), Obj::List(_))
                                && matches!(self.heap.get(ha), Obj::BigInt(_)))) =>
                {
                    let (lh, cnt) = if matches!(self.heap.get(ha), Obj::List(_)) {
                        (ha, self.int_of(Value::obj(hb)))
                    } else {
                        (hb, self.int_of(Value::obj(ha)))
                    };
                    self.list_repeat(lh, cnt, span)?
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
    pub(super) fn neg_value(&mut self, v: Value, span: Span) -> Result<Value, RuntimeError> {
        if let Some(n) = self.int_val(v) {
            let neg = n
                .checked_neg()
                .ok_or_else(|| self.err("integer overflow in negation".to_string(), span))?;
            Ok(self.make_int(neg))
        } else if v.is_float() {
            let f = self.float_of(v);
            Ok(self.box_float(-f))
        } else if let Some(h) = v.as_obj()
            && matches!(self.heap.get(h), Obj::Struct { .. } | Obj::Enum { .. })
        {
            // M22: unary `-` on a struct/enum dispatches to its `neg(self) -> Self` method
            // (the `Neg` protocol). Mirrors `struct_arith`, but self-only (no `other`).
            let (proto, home) = self.resolve_overload_method(v, "neg", span)?;
            self.guarded(|vm| vm.run_proto(proto, home, None, vec![v], true, false, span))
        } else {
            Err(self.err(format!("cannot apply Neg to {}", self.type_name(v)), span))
        }
    }

    /// `[elem...] * n` — repeat the list `n` times into a fresh list (gap #3). `n <= 0` → empty.
    /// Guards the allocation against capacity overflow (a giant `n` would otherwise abort the
    /// process via Vec's panic) — raises a RECOVERABLE fault, mirroring `str.repeat`. Mirrored
    /// byte-for-byte in `interp::eval_binary`.
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
            return Ok(Value::obj(self.heap.alloc(Obj::List(Vec::new()))));
        }
        let n = n as usize;
        // Guard the allocation: a giant `n` would abort the process via `Vec`'s capacity panic.
        // Bound the BYTE size (`count * size_of::<Value>()`) by `isize::MAX`, matching `Vec`'s own
        // limit — `str.repeat` does the same on its byte length (vm:7514). Recoverable fault.
        match items
            .len()
            .checked_mul(n)
            .and_then(|t| t.checked_mul(std::mem::size_of::<Value>()))
            .filter(|&bytes| bytes <= isize::MAX as usize)
        {
            Some(_) => {
                let src = items.clone();
                let total = src.len() * n;
                // The outer guard only bounds the byte size by `isize::MAX`; a huge-but-representable
                // total (e.g. 1e17) still passes it yet cannot actually be allocated, and
                // `Vec::with_capacity` would ABORT the process. `try_reserve_exact` converts that
                // into the same recoverable fault.
                let mut out: Vec<Value> = Vec::new();
                if out.try_reserve_exact(total).is_err() {
                    return Err(self.err("list repeat capacity overflow".to_string(), span));
                }
                for _ in 0..n {
                    out.extend(src.iter().copied());
                }
                Ok(Value::obj(self.heap.alloc(Obj::List(out))))
            }
            None => Err(self.err("list repeat capacity overflow".to_string(), span)),
        }
    }

    /// Set algebra for the operator forms `| & - ^` (gap #3). Mirrors the
    /// `union`/`intersection`/`difference` set methods (vm:7918) using the cached per-element
    /// hashes (no re-hashing; membership may still dispatch a user `eq`). `^` (symmetric-difference) has no method form:
    /// it is the union of (mine ∉ other) THEN (other ∉ mine), in that canonical insertion order so
    /// the result's print order is deterministic and parity-equal with the serial-VM oracle.
    pub(super) fn set_op(
        &mut self,
        op: SetOp,
        ha: GcRef,
        hb: GcRef,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let mine = match self.heap.get(ha) {
            Obj::Set(s) => s.entries.clone(),
            _ => unreachable!(),
        };
        let other = match self.heap.get(hb) {
            Obj::Set(s) => s.entries.clone(),
            _ => unreachable!(),
        };
        // Dedup-insert: propagate a cyclic-key depth fault (`?`) instead of swallowing it — the
        // membership `==` here is the same one `s.add`/`|` are DEFINED by, so it must fault alike.
        let add = |vm: &mut Vm, set: &mut SetData, he: u64, e: Value| -> Result<(), RuntimeError> {
            if vm
                .set_slot(&set.entries, set.candidates(he), e, span)?
                .is_none()
            {
                set.push(he, e);
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
            let mut out = SetData::default();
            match op {
                SetOp::Union => {
                    for (he, e) in mine.iter().chain(other.iter()) {
                        add(vm, &mut out, *he, *e)?;
                    }
                }
                SetOp::Intersection => {
                    for (he, e) in &mine {
                        if in_set(vm, &other, *he, *e)? {
                            add(vm, &mut out, *he, *e)?;
                        }
                    }
                }
                SetOp::Difference => {
                    for (he, e) in &mine {
                        if !in_set(vm, &other, *he, *e)? {
                            add(vm, &mut out, *he, *e)?;
                        }
                    }
                }
                SetOp::SymmetricDifference => {
                    for (he, e) in &mine {
                        if !in_set(vm, &other, *he, *e)? {
                            add(vm, &mut out, *he, *e)?;
                        }
                    }
                    for (he, e) in &other {
                        if !in_set(vm, &mine, *he, *e)? {
                            add(vm, &mut out, *he, *e)?;
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
    /// passed as the call's args (rooted as the new frame's locals). Mirrors `interp::struct_arith`.
    pub(super) fn struct_arith(
        &mut self,
        op: &Op,
        l: Value,
        r: Value,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let method = match op {
            Op::Add => "add",
            Op::Sub => "sub",
            Op::Mul => "mul",
            Op::Div => "div",
            Op::Mod => "mod",
            _ => unreachable!("struct_arith only handles + - * / %"),
        };
        let (proto, home) = self.resolve_overload_method(l, method, span)?;
        self.guarded(|vm| vm.run_proto(proto, home, None, vec![l, r], true, false, span))
    }

    /// Do `ha` and `hb` both hold a newtype with the SAME runtime key? (Drives same-type operator
    /// auto-flow — `Meters + Meters`, never `Meters + Seconds`.)
    pub(super) fn same_newtype_keys(&self, ha: GcRef, hb: GcRef) -> bool {
        match (self.heap.get(ha), self.heap.get(hb)) {
            (Obj::NewType { type_key: a, .. }, Obj::NewType { type_key: b, .. }) => a == b,
            _ => false,
        }
    }

    /// Same-newtype arithmetic: unwrap both inners, run the underlying's NATIVE primitive op (via the
    /// scalar `arith_scalar` core — identical overflow/div-by-zero/float semantics as a raw int/float
    /// op), then REWRAP in the same newtype key. NOT a user method (distinct from struct overloading).
    pub(super) fn newtype_arith(
        &mut self,
        op: &Op,
        ha: GcRef,
        hb: GcRef,
        name: &str,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let (key, a) = match self.heap.get(ha) {
            Obj::NewType { type_key, inner } => (type_key.clone(), *inner),
            _ => unreachable!(),
        };
        let b = match self.heap.get(hb) {
            Obj::NewType { inner, .. } => *inner,
            _ => unreachable!(),
        };
        let inner = self.arith_scalar(op, a, b, name, span)?;
        Ok(Value::obj(self.heap.alloc(Obj::NewType {
            type_key: key,
            inner,
        })))
    }

    /// The underlying primitive `+`/`-`/`*`/`/`/`%` on two scalar values (int or float), with the
    /// SAME overflow / division-by-zero / float semantics as the inline `binary` arms. Shared by the
    /// newtype same-type operator path so it byte-matches a raw int/float op.
    pub(super) fn arith_scalar(
        &mut self,
        op: &Op,
        a: Value,
        b: Value,
        name: &str,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if self.is_integral(a) && self.is_integral(b) {
            let (a, b) = (self.int_of(a), self.int_of(b));
            let v = match op {
                Op::Add => a.checked_add(b),
                Op::Sub => a.checked_sub(b),
                Op::Mul => a.checked_mul(b),
                Op::Div | Op::Mod if b == 0 => {
                    let kind = if matches!(op, Op::Div) {
                        "division"
                    } else {
                        "modulo"
                    };
                    return Err(self.err(format!("{kind} by zero"), span));
                }
                Op::Div => a.checked_div(b),
                // `wrapping_rem` never overflows past the `b == 0` guard above (see `fast_int_bin`'s
                // `BinKind::Mod` arm) — wrap in `Some` so this shared overflow check never fires for Mod.
                Op::Mod => Some(a.wrapping_rem(b)),
                _ => unreachable!(),
            };
            let n = v.ok_or_else(|| self.err(format!("integer overflow in {name}"), span))?;
            Ok(self.make_int(n))
        } else if self.is_numeric(a) && self.is_numeric(b) {
            let (x, y) = (self.as_f64(a), self.as_f64(b));
            // Float arithmetic is total IEEE-754: division/modulo by zero yields inf/-inf/NaN,
            // never a fault. (The INT arm above still faults on /0 and overflow.)
            let f = match op {
                Op::Add => x + y,
                Op::Sub => x - y,
                Op::Mul => x * y,
                Op::Div => x / y,
                Op::Mod => x % y,
                _ => unreachable!(),
            };
            Ok(self.box_float(f))
        } else {
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

    /// Resolve `(proto, home_module_obj)` for an operator-overload method `method` on receiver `recv`
    /// — a struct (via `program.structs`) or an enum (via `program.enum_methods` + `enum_home`). The
    /// shared dispatch core for arithmetic and ordering overloads on both struct and enum values.
    pub(super) fn resolve_overload_method(
        &self,
        recv: Value,
        method: &str,
        span: Span,
    ) -> Result<(usize, GcRef), RuntimeError> {
        debug_assert!(recv.is_obj(), "resolve_overload_method on non-obj");
        let h = recv.as_gcref();
        match self.heap.get(h) {
            Obj::Struct { tid, .. } => {
                let name = self.struct_name_of_tid(*tid);
                let def = self
                    .program
                    .structs
                    .get(name)
                    .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
                let proto = *def.methods.get(method).ok_or_else(|| {
                    self.err(
                        format!("struct '{}' has no '{method}' method", def.display_name),
                        span,
                    )
                })?;
                Ok((proto, self.module_objs[def.module_idx]))
            }
            Obj::Enum { variant_id, .. } => {
                let key = self.enum_names(*variant_id).0.to_string();
                let proto = *self
                    .program
                    .enum_methods
                    .get(&key)
                    .and_then(|ms| ms.get(method))
                    .ok_or_else(|| {
                        self.err(
                            format!(
                                "enum '{}' has no '{method}' method",
                                crate::compiler::bare_display(&key)
                            ),
                            span,
                        )
                    })?;
                Ok((proto, self.module_objs[self.enum_home_module(&key)]))
            }
            // A newtype's overload/hook methods (`hash`/`str`/user methods) resolve via
            // `newtype_methods`, mirroring the enum path.
            Obj::NewType { type_key, .. } => {
                let key = type_key.to_string();
                let proto = *self
                    .program
                    .newtype_methods
                    .get(&key)
                    .and_then(|ms| ms.get(method))
                    .ok_or_else(|| {
                        self.err(
                            format!(
                                "newtype '{}' has no '{method}' method",
                                crate::compiler::bare_display(&key)
                            ),
                            span,
                        )
                    })?;
                Ok((proto, self.module_objs[self.newtype_home_module(&key)]))
            }
            _ => unreachable!("overload receiver is a struct, enum, or newtype"),
        }
    }

    /// Bitwise / shift ops — int-only (gap #13). Shift amounts outside `0..64` are a runtime error
    /// (Rust would otherwise panic), with a message identical to the serial-VM oracle's.
    pub(super) fn bitwise(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        let r = self.pop();
        let l = self.pop();
        let name = match op {
            Op::BitAnd => "BitAnd",
            Op::BitOr => "BitOr",
            Op::BitXor => "BitXor",
            Op::Shl => "Shl",
            Op::Shr => "Shr",
            _ => unreachable!(),
        };
        let result = if self.is_integral(l) && self.is_integral(r) {
            let (a, b) = (self.int_of(l), self.int_of(r));
            let v = match op {
                Op::BitAnd => a & b,
                Op::BitOr => a | b,
                Op::BitXor => a ^ b,
                Op::Shl | Op::Shr => {
                    if !(0..64).contains(&b) {
                        return Err(
                            self.err(format!("shift amount {b} out of range (0..64)"), span)
                        );
                    }
                    if matches!(op, Op::Shl) {
                        // Left shift can overflow (drop high bits) like `+ - * /`; treat
                        // it as a recoverable fault, not a silent wrap. Round-trip test:
                        // `(a << b) >> b == a` holds iff no significant bit was shifted out
                        // (correct for negative operands too — `-1 << 63` round-trips).
                        let v = a << (b as u32);
                        if (v >> (b as u32)) != a {
                            return Err(self.err(format!("integer overflow in {name}"), span));
                        }
                        v
                    } else {
                        a >> (b as u32)
                    }
                }
                _ => unreachable!(),
            };
            self.make_int(v)
        } else {
            match (l.view(), r.view()) {
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
                        _ => SetOp::SymmetricDifference,
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

    /// `x in container` — membership test. Pops `[x, container]`, pushes a `Bool`. Dispatches on the
    /// container kind, reusing the same equality / hashing the `.contains`/`.has` methods use: a
    /// list/set tests element membership, a map tests KEY membership (Python-style), a str tests
    /// substring. The checker has already type-directed this; the runtime is the fallback.
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
            let (proto, home) = self.resolve_overload_method(container, "contains", span)?;
            let res = self.guarded(|vm| {
                vm.run_proto(
                    proto,
                    home,
                    None,
                    vec![container, needle],
                    true,
                    false,
                    span,
                )
            })?;
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
                        _ => {
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
                _ => {
                    return Err(self.err(
                        format!("cannot use `in` on {}", self.type_name(container)),
                        span,
                    ));
                }
            },
            _ => {
                return Err(self.err(
                    format!("cannot use `in` on {}", self.type_name(container)),
                    span,
                ));
            }
        };
        self.push(Value::bool(found));
        Ok(())
    }

    pub(super) fn compare_op(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        let r = self.pop();
        let l = self.pop();
        // Same-newtype ordering: `Meters < Meters` UNWRAPS both and compares the underlyings with
        // their NATIVE ordering (the checker rejected `Meters < float` / `< Seconds`). Not a user
        // `compare` method — the underlying's native compare. Must precede the struct/enum overload.
        if let (Some(hl), Some(hr)) = (l.as_obj(), r.as_obj())
            && self.same_newtype_keys(hl, hr)
        {
            let a = match self.heap.get(hl) {
                Obj::NewType { inner, .. } => *inner,
                _ => unreachable!(),
            };
            let b = match self.heap.get(hr) {
                Obj::NewType { inner, .. } => *inner,
                _ => unreachable!(),
            };
            let bres = self.ordered_bool(op, a, b, span)?;
            self.push(Value::bool(bres));
            return Ok(());
        }
        // Operator overloading: ordering on two structs dispatches to `compare(self, other) -> int`
        // (the `Comparable` protocol). The checker has verified conformance. Equality stays
        // structural; only ordering is overloaded. Mirrors `interp::struct_ordering`.
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
    /// four ops true. Mirrors `interp::eval_binary`'s `Lt|LtEq|Gt|GtEq` arm.
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
                _ => unreachable!(),
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
                    _ => unreachable!(),
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
    /// method, mapping the sign of the result to a boolean. Mirrors `interp::struct_ordering`.
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
            _ => unreachable!(),
        };
        self.push(Value::bool(b));
        Ok(())
    }

    /// Call a struct's `compare(self, other) -> int` method and return the resulting `Ordering`.
    /// Shared by ordering operators (`struct_ordering`) and `list.sort()` over Comparable structs.
    /// Mirrors `interp::struct_compare`.
    pub(super) fn struct_compare(
        &mut self,
        l: Value,
        r: Value,
        span: Span,
    ) -> Result<std::cmp::Ordering, RuntimeError> {
        let (proto, home) = self.resolve_overload_method(l, "compare", span)?;
        let res =
            self.guarded(|vm| vm.run_proto(proto, home, None, vec![l, r], true, false, span))?;
        match self.int_val(res) {
            Some(n) => Ok(n.cmp(&0)),
            None => Err(self.err(
                format!("compare() must return int, got {}", self.type_name(res)),
                span,
            )),
        }
    }

    /// A `u64` hash of `v` for map/set keys, upholding the invariant `values_equal(a,b) ⇒
    /// hash(a)==hash(b)`. Numeric keys hash by their canonical f64 bits (so `Int(3)` and `Float(3.0)`
    /// collide, matching `values_equal`'s numeric unification); str by content; a struct key
    /// dispatches its user `hash(self) -> int` (re-entrant — may allocate / trigger GC). Floats are
    /// rejected as keys by the checker (NaN footgun), so only integral-valued floats reach here.
    pub(super) fn hash_value(&mut self, v: Value, span: Span) -> Result<u64, RuntimeError> {
        // A struct key dispatches its user `hash()` (re-entrant). Everything else is scalar. A boxed
        // float is Float-tagged (not `as_obj`) so it falls to the scalar arm below; a boxed `BigInt`
        // is Obj-tagged and must be treated as the integral scalar it is.
        if let Some(h) = v.as_obj() {
            match self.heap.get(h) {
                Obj::Struct { .. } => self.struct_hash(v, span),
                // An enum key dispatches its user `hash(self) -> int` via the shared enum-aware
                // resolver, mirroring the struct path (re-entrant — may allocate / trigger GC).
                Obj::Enum { .. } => self.enum_hash(v, span),
                // A newtype key dispatches its user `hash(self) -> int` (opt-in — the checker rejects
                // a newtype with no `hash` as a key, even over an intrinsically-hashable underlying).
                Obj::NewType { .. } => self.newtype_hash(v, span),
                Obj::Str(_) | Obj::Bytes(_) | Obj::BigInt(_) => Ok(self.scalar_hash(v)),
                _ => Err(self.err(
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
                _ => 0,
            },
        }
    }

    /// Dispatch a struct key's user `hash(self) -> int`, returning its `i64` as a `u64`. Mirrors
    /// [`struct_compare`] (re-entrant via `run_proto`).
    pub(super) fn struct_hash(&mut self, v: Value, span: Span) -> Result<u64, RuntimeError> {
        let Some(h) = v.as_obj() else { unreachable!() };
        let Obj::Struct { tid, .. } = self.heap.get(h) else {
            unreachable!()
        };
        let name = self.struct_name_of_tid(*tid);
        let def = self
            .program
            .structs
            .get(name)
            .cloned()
            .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        // A ZERO-FIELD struct with no `hash` method hashes to a constant (0): it has no state, so
        // there is nothing to hash. `==`'s type-tag guard keeps distinct empty-struct types unequal
        // despite the shared hash. Mirrors the checker's zero-field `Hashable` intrinsic and the
        // serial-VM oracle's identical constant (two-engine parity).
        if def.fields.is_empty() && !def.methods.contains_key("hash") {
            return Ok(0);
        }
        let proto = *def.methods.get("hash").ok_or_else(|| {
            self.err(
                format!(
                    "struct '{}' has no 'hash' method (needed to use it as a map/set key)",
                    def.display_name
                ),
                span,
            )
        })?;
        let home = self.module_objs[def.module_idx];
        let res = self.guarded(|vm| vm.run_proto(proto, home, None, vec![v], true, false, span))?;
        match self.int_val(res) {
            Some(n) => Ok(n as u64),
            None => Err(self.err(
                format!("hash() must return int, got {}", self.type_name(res)),
                span,
            )),
        }
    }

    /// Dispatch an enum key's user `hash(self) -> int` via the shared enum-aware
    /// [`resolve_overload_method`], mirroring [`struct_hash`] (re-entrant via `run_proto`).
    pub(super) fn enum_hash(&mut self, v: Value, span: Span) -> Result<u64, RuntimeError> {
        let (proto, home) = self.resolve_overload_method(v, "hash", span)?;
        let res = self.guarded(|vm| vm.run_proto(proto, home, None, vec![v], true, false, span))?;
        match self.int_val(res) {
            Some(n) => Ok(n as u64),
            None => Err(self.err(
                format!("hash() must return int, got {}", self.type_name(res)),
                span,
            )),
        }
    }

    /// Dispatch a newtype key's user `hash(self) -> int` via the shared resolver (mirrors `enum_hash`;
    /// re-entrant via `run_proto`). The checker guarantees a key-used newtype defines `hash`.
    pub(super) fn newtype_hash(&mut self, v: Value, span: Span) -> Result<u64, RuntimeError> {
        let (proto, home) = self.resolve_overload_method(v, "hash", span)?;
        let res = self.guarded(|vm| vm.run_proto(proto, home, None, vec![v], true, false, span))?;
        match self.int_val(res) {
            Some(n) => Ok(n as u64),
            None => Err(self.err(
                format!("hash() must return int, got {}", self.type_name(res)),
                span,
            )),
        }
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

    /// Snapshot (deep-copy) a struct/enum/newtype key/element on the STORE path (Go value-key
    /// model): after the key is stored, mutating the caller's original value can no longer reach the
    /// stored key, so the collection can't be silently corrupted. Only the three heap-aggregate arms
    /// `hash_value` dispatches (`Struct`/`Enum`/`NewType`) are copied — scalars and every immutable /
    /// by-reference object pass through UNCHANGED (zero-clone hot path). Infallible + pure-alloc (no
    /// VM re-entry ⇒ no GC fires mid-copy), so it needs no rooting; a caller that then re-enters the
    /// VM (e.g. `hash_value` on a *later* element) must root the returned snapshot itself.
    ///
    /// Deliberately NOT `deep_clone` (the concurrency airlock's `to_wire`/`from_wire`): that
    /// serializer (a) FAULTS on a generator / cyclic key — nonsensical for a plain
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
                    Obj::Struct { .. } | Obj::Enum { .. } | Obj::NewType { .. }
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
            _ => key,
        }
    }

    /// True iff `v`'s value graph must be stored BY REFERENCE rather than snapshotted — i.e. it
    /// contains a cycle OR is deeper than [`MAX_STRUCTURAL_DEPTH`]. Both cases are stored by handle
    /// (base behavior): a cyclic snapshot can't be `values_equal` to the original, and an over-deep
    /// snapshot's tail is aliased ([`Vm::snapshot_value`] caps there) so a held-key lookup trips the
    /// same depth guard in `values_equal` and silently misses — whereas a by-reference key resolves
    /// instantly on the `ha == hb` identity short-circuit. Read-only, allocates only two small work
    /// sets; runs on the cold struct/enum/newtype key-insert path. `on_path` holds the DFS recursion
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
            Obj::Struct { fields, .. } => fields.as_slice().to_vec(),
            Obj::Enum { payload, .. } => payload.clone(),
            Obj::NewType { inner, .. } => vec![*inner],
            Obj::List(items) | Obj::Tuple(items) => items.clone(),
            Obj::Map(m) => m
                .entries
                .iter()
                .flat_map(|(_, k, val)| [*k, *val])
                .collect(),
            Obj::Set(s) => s.entries.iter().map(|(_, e)| *e).collect(),
            _ => {
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
    /// aggregate arms (`Struct`/`Enum`/`NewType`/`List`/`Tuple`/`Map`/`Set`/`ByteArray`) and returns
    /// every other value (scalars, immutable `Str`/`Bytes`/`Ptr`/`Builtin`, and all identity-compared
    /// by-reference objects: `Closure`/`Func`/`Channel`/`Shared`/…/`Generator`/`Iter`/`Cell`) BY
    /// REFERENCE — keeping those handles is what preserves `values_equal` (identity-only for them) and
    /// avoids the airlock's non-sendable faults. `visited` maps an original aggregate handle to its
    /// copy so a cyclic key is copied ONCE (cycle preserved, not overflowed); `depth` caps genuinely
    /// deep (non-cyclic) keys at `MAX_STRUCTURAL_DEPTH` — the SAME bound `values_equal`/`to_wire` use —
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
        if depth > MAX_STRUCTURAL_DEPTH {
            return v; // absurdly deep (non-cyclic) key: stop copying, alias the tail (no overflow)
        }
        if let Some(&c) = visited.get(&h) {
            return Value::obj(c); // already copied (shared sub-value or cycle back-edge)
        }
        match self.heap.get(h) {
            Obj::List(items) => {
                let items = items.clone();
                let nh = self.heap.alloc(Obj::List(items.clone()));
                visited.insert(h, nh);
                let copied: Vec<Value> = items
                    .iter()
                    .map(|&x| self.snapshot_value(x, visited, depth + 1))
                    .collect();
                if let Obj::List(dst) = self.heap.get_mut(nh) {
                    *dst = copied;
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
                if let Obj::Tuple(dst) = self.heap.get_mut(nh) {
                    *dst = copied;
                }
                Value::obj(nh)
            }
            Obj::Struct { tid, fields } => {
                let (tid, fields) = (*tid, fields.clone());
                let nh = self.heap.alloc(Obj::Struct {
                    tid,
                    fields: fields.clone(),
                });
                visited.insert(h, nh);
                let copied: Vec<Value> = fields
                    .iter()
                    .map(|&f| self.snapshot_value(f, visited, depth + 1))
                    .collect();
                if let Obj::Struct { fields, .. } = self.heap.get_mut(nh) {
                    *fields = Fields::from_vec(copied);
                }
                Value::obj(nh)
            }
            Obj::Enum {
                variant_id,
                payload,
            } => {
                let (variant_id, payload) = (*variant_id, payload.clone());
                let nh = self.heap.alloc(Obj::Enum {
                    variant_id,
                    payload: payload.clone(),
                });
                visited.insert(h, nh);
                let copied: Vec<Value> = payload
                    .iter()
                    .map(|&p| self.snapshot_value(p, visited, depth + 1))
                    .collect();
                if let Obj::Enum { payload, .. } = self.heap.get_mut(nh) {
                    *payload = copied;
                }
                Value::obj(nh)
            }
            Obj::NewType { type_key, inner } => {
                let (type_key, inner) = (type_key.clone(), *inner);
                let nh = self.heap.alloc(Obj::NewType { type_key, inner });
                visited.insert(h, nh);
                let ci = self.snapshot_value(inner, visited, depth + 1);
                if let Obj::NewType { inner, .. } = self.heap.get_mut(nh) {
                    *inner = ci;
                }
                Value::obj(nh)
            }
            Obj::Map(m) => {
                let entries = m.entries.clone();
                let nh = self.heap.alloc(Obj::Map(m.clone()));
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
                if let Obj::Map(dst) = self.heap.get_mut(nh) {
                    dst.entries = copied;
                }
                Value::obj(nh)
            }
            Obj::Set(s) => {
                let entries = s.entries.clone();
                let nh = self.heap.alloc(Obj::Set(s.clone()));
                visited.insert(h, nh);
                let copied: Vec<(u64, Value)> = entries
                    .iter()
                    .map(|&(hh, e)| (hh, self.snapshot_value(e, visited, depth + 1)))
                    .collect();
                if let Obj::Set(dst) = self.heap.get_mut(nh) {
                    dst.entries = copied;
                }
                Value::obj(nh)
            }
            // A `bytearray` is mutable + structurally compared but a GC LEAF (raw bytes, no children):
            // copy the buffer, no recursion, no visited entry (it can't participate in a cycle).
            Obj::ByteArray(b) => Value::obj(self.heap.alloc(Obj::ByteArray(b.clone()))),
            // Everything else (immutable `Str`/`Bytes`/`Ptr`/`Builtin` + all identity-compared
            // by-reference objects) is kept BY REFERENCE — a copy would break `values_equal` for the
            // identity arms and needlessly duplicate the immutable ones.
            _ => v,
        }
    }

    /// `xs.sort()` over a list of Comparable structs, ordering via each struct's `compare`. Because
    /// `compare` re-enters the VM (and may allocate / trigger GC), this mirrors `list_sort_by`
    /// exactly: snapshot the elements into a heap list ROOTED on the operand stack, permute
    /// *indices* re-read from that rooted list per comparison (never holding unrooted `Value`s
    /// across a `compare` call), then write the result back. (Primitives use the faster
    /// `value_order`, which never re-enters the VM.) Mirrors `interp::eval_list_sort`.
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
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!("list_sort on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::obj(snap_h)); // ROOT the snapshot across the comparator calls
        let n = match self.heap.get(snap_h) {
            Obj::List(v) => v.len(),
            _ => unreachable!(),
        };
        let order = match self.msort_indices_structs(snap_h, (0..n).collect(), span) {
            Ok(o) => o,
            Err(e) => {
                self.pop(); // unroot snapshot
                self.pop(); // unroot source
                return Err(e);
            }
        };
        // No comparator calls remain, so no GC: read the rooted snapshot and write the result back.
        let reordered: Vec<Value> = match self.heap.get(snap_h) {
            Obj::List(v) => order.iter().map(|&i| v[i]).collect(),
            _ => unreachable!(),
        };
        if let Obj::List(v) = self.heap.get_mut(src_h) {
            *v = reordered;
        }
        self.pop(); // unroot snapshot
        self.pop(); // unroot source
        Ok(Value::nil())
    }

    /// Stable top-down merge sort over `idx` (positions into the rooted list `src_h`), comparing
    /// elements via each struct's `compare`. Re-reads elements from `src_h` per comparison so no
    /// unrooted `Value` is held across the GC-capable `struct_compare` call.
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
        let left = self.msort_indices_structs(src_h, idx, span)?;
        let right = self.msort_indices_structs(src_h, right, span)?;
        let mut out = Vec::with_capacity(n);
        let (mut li, mut ri) = (0, 0);
        while li < left.len() && ri < right.len() {
            let a = match self.heap.get(src_h) {
                Obj::List(v) => v[left[li]],
                _ => unreachable!(),
            };
            let b = match self.heap.get(src_h) {
                Obj::List(v) => v[right[ri]],
                _ => unreachable!(),
            };
            // `<= Equal` keeps the left element first on ties → stable.
            if self.struct_compare(a, b, span)?.is_le() {
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

    pub(super) fn compare(&self, l: Value, r: Value) -> Option<std::cmp::Ordering> {
        // Numeric-newtype (`Comparable`) unwrap: a `List[newtype=int/float]` reaches `.min()`/`.max()`
        // through here. Order by the wrapped scalar's NATIVE order — same as bare `<` (see `compare_op`),
        // never a user `compare` method. Recurse one side per call → converges to scalar operands
        // (handles both-newtype, defensive one-side, and nested `newtype B = A`). MUST precede the
        // scalar fast paths below.
        if let Some(ha) = l.as_obj()
            && let Obj::NewType { inner, .. } = self.heap.get(ha)
        {
            return self.compare(*inner, r);
        }
        if let Some(hb) = r.as_obj()
            && let Obj::NewType { inner, .. } = self.heap.get(hb)
        {
            return self.compare(l, *inner);
        }
        // Both integral (inline or boxed) → exact i64 order; else both numeric → f64 (NaN → None).
        if self.is_integral(l) && self.is_integral(r) {
            return Some(self.int_of(l).cmp(&self.int_of(r)));
        }
        if self.is_numeric(l) && self.is_numeric(r) {
            return self.as_f64(l).partial_cmp(&self.as_f64(r));
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
                    let (p, hs, stored) = match vm.heap.get(mh) {
                        Obj::Map(m) => match m.candidates(hk).get(i) {
                            Some(&p) => match m.entries.get(p) {
                                Some(&(h, k, _)) => (p, h, k),
                                None => return Ok(None),
                            },
                            None => return Ok(None),
                        },
                        _ => unreachable!("map_probe on non-map"),
                    };
                    let eq = vm.elem_equal(stored, key, 0, span)?;
                    // Position still holding the key we just compared? (The entry's VALUE may have
                    // changed — `m[k] = v` inside `eq` does not move anything — so only the cached
                    // hash and the key are checked.) If not, the map was structurally mutated and
                    // every position, `p` included, is stale: start over.
                    let live = !reenter
                        || match vm.heap.get(mh) {
                            Obj::Map(m) => m
                                .entries
                                .get(p)
                                .is_some_and(|&(h, k, _)| h == hs && k == stored),
                            _ => unreachable!("map_probe on non-map"),
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
                    let (p, hs, stored) = match vm.heap.get(sh) {
                        Obj::Set(s) => match s.candidates(he).get(i) {
                            Some(&p) => match s.entries.get(p) {
                                Some(&(h, e)) => (p, h, e),
                                None => return Ok(None),
                            },
                            None => return Ok(None),
                        },
                        _ => unreachable!("set_probe on non-set"),
                    };
                    let eq = vm.elem_equal(stored, elem, 0, span)?;
                    let live = !reenter
                        || match vm.heap.get(sh) {
                            Obj::Set(s) => s
                                .entries
                                .get(p)
                                .is_some_and(|&(h, e)| h == hs && e == stored),
                            _ => unreachable!("set_probe on non-set"),
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
    /// therefore collect? Both hook tables are left EMPTY unless the program declares at least one
    /// `fn eq(self, o: Self) -> bool` (`Compiler::build_eq_hooks`), so this is a whole-program answer,
    /// and `false` for the overwhelming majority of programs. Used to skip the probe rooting on the
    /// hot map/set/`==` paths, where it would otherwise be pure overhead.
    #[inline]
    fn eq_may_reenter(&self) -> bool {
        !self.program.eq_struct.is_empty() || !self.program.eq_enum.is_empty()
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
    /// bench back to flat (`docs/benchmarks.md`).
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
    /// `MAX_STRUCTURAL_DEPTH` is threaded through the structural recursion as before. A user `eq` that
    /// itself compares starts a FRESH depth-0 walk in a new VM frame — bounded there by the call-depth
    /// guard, not by this counter.
    pub(super) fn values_equal_guarded(
        &mut self,
        l: Value,
        r: Value,
        depth: usize,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        if depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.depth_exceeded_err(span));
        }
        // Exact i64 equality when BOTH operands are integral (inline `Int` OR boxed `BigInt`) —
        // Python parity. MUST precede the numeric arm below, which compares via `as_f64` — lossy for
        // `|i64| > 2^53` (distinct ints round to one f64). The canonical-rep invariant (inline XOR
        // boxed) makes this correct across kinds: an inline int and a boxed big-int are never equal.
        if self.is_integral(l) && self.is_integral(r) {
            return Ok(self.int_of(l) == self.int_of(r));
        }
        // Cross-type numeric (`1 == 1.0`) and float==float compare via f64.
        if self.is_numeric(l) && self.is_numeric(r) {
            return Ok(self.as_f64(l) == self.as_f64(r));
        }
        match (l.view(), r.view()) {
            (ValueView::Bool(a), ValueView::Bool(b)) => Ok(a == b),
            (ValueView::Nil, ValueView::Nil) => Ok(true),
            (ValueView::Obj(ha), ValueView::Obj(hb)) => {
                // `Eq` protocol (M23). The `user_eq_method` peek is `&self`, so the immutable
                // `self.heap` borrow is over before `run_proto` needs `&mut self` (the borrow shape
                // `op_contains`/`struct_compare` use). Both operands are already GC-reachable from
                // whatever rooted the top of this walk (`with_roots` at the entry points).
                // `eq_hook_off` is `Atomic.cas`'s window (see the field's doc): a compare under the
                // box's value lock must stay structural, whatever the checker did or did not see.
                if let Some((proto, home)) = (!self.eq_hook_off)
                    .then(|| self.user_eq_method(l, r))
                    .flatten()
                {
                    let res = self.guarded(|vm| {
                        vm.run_proto(proto, home, None, vec![l, r], true, false, span)
                    })?;
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
                    // Two newtypes are equal iff they are the SAME newtype (key) and their inners are
                    // structurally equal. A different key is a distinct type ⇒ never equal.
                    (
                        Obj::NewType {
                            type_key: ka,
                            inner: ia,
                        },
                        Obj::NewType {
                            type_key: kb,
                            inner: ib,
                        },
                    ) => {
                        if ka != kb {
                            return Ok(false);
                        }
                        let (ia, ib) = (*ia, *ib);
                        self.elem_equal(ia, ib, depth + 1, span)
                    }
                    // Two opaque `ptr` handles are equal iff they hold the same raw address (identity).
                    // Distinct heap slots can wrap the same address (e.g. a re-`from_wire`'d handle or
                    // `std.ffi.null()` twice), so the same-`GcRef` shortcut above is not enough.
                    (Obj::Ptr(a), Obj::Ptr(b)) => Ok(a == b),
                    // Two first-class builtin-fn values are equal iff they name the SAME builtin. Each
                    // value-position use emits a fresh `Op::LoadBuiltin` → a distinct handle, so the
                    // `ha == hb` identity short-circuit above never fires; compare by name to match the
                    // interp (derived `PartialEq` on `Value::Builtin`'s `Rc<str>`) — VM==interp parity.
                    (Obj::Builtin(a), Obj::Builtin(b)) => Ok(a == b),
                    // Boxed scalars compare by value, identically to the inline `Int`/`Float` arms.
                    (Obj::BigInt(a), Obj::BigInt(b)) => Ok(a == b),
                    (Obj::FloatBox(a), Obj::FloatBox(b)) => Ok(a == b),
                    _ => Ok(false),
                }
            }
            _ => Ok(false),
        }
    }

    /// Total order over scalar values for `sort()`. The checker restricts `sort` to homogeneous
    /// int/float/str lists; str elements are read through the heap. Anything else compares Equal.
    pub(super) fn value_order(&self, a: Value, b: Value) -> std::cmp::Ordering {
        use std::cmp::Ordering::Equal;
        // Numeric-newtype (`Comparable`) unwrap: a `List[newtype=int/float]` reaches `.sort()` through
        // here. Order by the wrapped scalar's NATIVE order — same as bare `<` (see `compare_op`), never
        // a user `compare` method. Recurse one side per call → converges to scalar operands. MUST
        // precede the scalar fast paths below (without it a NewType falls to `_ => Equal` → silent no-op).
        if let Some(ha) = a.as_obj()
            && let Obj::NewType { inner, .. } = self.heap.get(ha)
        {
            return self.value_order(*inner, b);
        }
        if let Some(hb) = b.as_obj()
            && let Obj::NewType { inner, .. } = self.heap.get(hb)
        {
            return self.value_order(a, *inner);
        }
        // Homogeneous lists only (checker-enforced): both int (inline/boxed) → exact i64; both float
        // → total_cmp; both str → lexical. A mixed/other pair compares Equal.
        if self.is_integral(a) && self.is_integral(b) {
            return self.int_of(a).cmp(&self.int_of(b));
        }
        if a.is_float() && b.is_float() {
            return self.float_of(a).total_cmp(&self.float_of(b));
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
