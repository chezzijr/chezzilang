// vm::arith — split out of vm/mod.rs. `super::*` == the `vm` module.
// Arithmetic, comparison, ordering, hashing, equality.

use super::*;

impl Vm {
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
        self.push(Value::Obj(h));
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
                if let (Value::Int(x), Value::Int(y)) = (self.stack[n - 2], self.stack[n - 1]) {
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
                let both_int = matches!(
                    (self.stack[n - 2], self.stack[n - 1]),
                    (Value::Int(_), Value::Int(_))
                );
                self.quicken[site] = if both_int { Q_INT } else { Q_GENERIC };
                self.run_bin_kind(kind, span)
            }
        }
    }

    /// M19 Tier-2 — adaptive quickening for `Eq`/`NotEq` (never fused, so always reached here). The
    /// int fast path REPLICATES the generic numeric comparison `as_f64(x) == as_f64(y)` (lossy for
    /// `|i64| > 2^53`) — NOT exact `x == y` — so it stays byte-identical to `values_equal_guarded`
    /// (`Value::Int` is numeric) and to the interpreter; preserving that loss is what keeps two-engine
    /// parity. `negate` flips the result for `NotEq`. Mirrors the kept `Op::Eq`/`Op::NotEq` `step` arms.
    #[inline(never)]
    pub(super) fn q_eq(
        &mut self,
        site: usize,
        negate: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if self.quicken[site] == Q_INT {
            let n = self.stack.len();
            if let (Value::Int(x), Value::Int(y)) = (self.stack[n - 2], self.stack[n - 1]) {
                self.stack.truncate(n - 2);
                let eq = (x as f64) == (y as f64);
                self.push(Value::Bool(eq ^ negate));
                return Ok(());
            }
            self.quicken[site] = Q_GENERIC; // non-int at a specialized site → deopt
        } else if self.quicken[site] == Q_COLD {
            let n = self.stack.len();
            let both_int = matches!(
                (self.stack[n - 2], self.stack[n - 1]),
                (Value::Int(_), Value::Int(_))
            );
            self.quicken[site] = if both_int { Q_INT } else { Q_GENERIC };
            // fall through to the generic path this first time
        }
        let r = self.pop();
        let l = self.pop();
        let eq = self.values_equal_guarded(l, r, 0, span)?;
        self.push(Value::Bool(eq ^ negate));
        Ok(())
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
        if let (Value::Int(x), Value::Int(y)) = (l, r) {
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
        if let Value::Int(x) = l {
            let v = self.fast_int_bin(x, val, kind, span)?;
            self.push(v);
        } else {
            self.push(l);
            self.push(Value::Int(val));
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
        match self.stack[at] {
            Value::Int(x) => {
                let v = x
                    .checked_add(delta)
                    .ok_or_else(|| self.err("integer overflow in Add".to_string(), span))?;
                self.stack[at] = Value::Int(v);
            }
            Value::Float(f) => self.stack[at] = Value::Float(f + delta as f64),
            other => {
                self.push(other);
                self.push(Value::Int(delta));
                self.arith(&Op::Add, span)?;
                let v = self.pop();
                let at = self.base() + slot;
                self.stack[at] = v;
            }
        }
        Ok(())
    }

    /// Int/Int fast path for the fused binops (`BinLocalLocal` / `BinLocalConst`). Must match
    /// `arith` (overflow / div-by-zero errors) and `compare_op` (ordering) for `Int` operands
    /// exactly. Anything non-`Int` never reaches here — the caller falls back to the slow path.
    pub(super) fn fast_int_bin(
        &self,
        x: i64,
        y: i64,
        kind: crate::vm::op::BinKind,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        use crate::vm::op::BinKind;
        let v = match kind {
            BinKind::Add => Value::Int(
                x.checked_add(y)
                    .ok_or_else(|| self.err("integer overflow in Add".to_string(), span))?,
            ),
            BinKind::Sub => Value::Int(
                x.checked_sub(y)
                    .ok_or_else(|| self.err("integer overflow in Sub".to_string(), span))?,
            ),
            BinKind::Mul => Value::Int(
                x.checked_mul(y)
                    .ok_or_else(|| self.err("integer overflow in Mul".to_string(), span))?,
            ),
            BinKind::Div => {
                if y == 0 {
                    return Err(self.err("division by zero".to_string(), span));
                }
                Value::Int(
                    x.checked_div(y)
                        .ok_or_else(|| self.err("integer overflow in Div".to_string(), span))?,
                )
            }
            BinKind::Mod => {
                if y == 0 {
                    return Err(self.err("modulo by zero".to_string(), span));
                }
                Value::Int(
                    x.checked_rem(y)
                        .ok_or_else(|| self.err("integer overflow in Mod".to_string(), span))?,
                )
            }
            BinKind::Lt => Value::Bool(x < y),
            BinKind::LtEq => Value::Bool(x <= y),
            BinKind::Gt => Value::Bool(x > y),
            BinKind::GtEq => Value::Bool(x >= y),
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
        let result = match (l, r) {
            (Value::Int(a), Value::Int(b)) => {
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
                    Op::Mod => a.checked_rem(b),
                    _ => unreachable!(),
                };
                Value::Int(v.ok_or_else(|| self.err(format!("integer overflow in {name}"), span))?)
            }
            (a, b) if is_numeric(a) && is_numeric(b) => {
                let (x, y) = (as_f64(a), as_f64(b));
                // Float arithmetic is total IEEE-754: division/modulo by zero yields inf/-inf/NaN,
                // never a fault. (The INT arm above still faults on /0 and overflow.)
                Value::Float(match op {
                    Op::Add => x + y,
                    Op::Sub => x - y,
                    Op::Mul => x * y,
                    Op::Div => x / y,
                    Op::Mod => x % y,
                    _ => unreachable!(),
                })
            }
            // Same-newtype arithmetic: `Meters + Meters` etc. UNWRAPS both wrappers, runs the
            // underlying's NATIVE primitive op (identical overflow/div-by-zero/float semantics — it
            // recurses through `self.binary` on the inners), then REWRAPS in the same newtype. This is
            // NOT a user `add` method — it is the underlying's own op (distinct from struct
            // overloading). The checker has rejected `Meters + float` / `Meters + Seconds`, so a
            // mismatched pair never reaches here from typechecked code. Must precede struct_arith.
            (Value::Obj(ha), Value::Obj(hb))
                if matches!(op, Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod)
                    && self.same_newtype_keys(ha, hb) =>
            {
                self.newtype_arith(op, ha, hb, name, span)?
            }
            // Arithmetic overloading: `+`/`-`/`*` on two structs (or two enums) dispatch to
            // `add`/`sub`/`mul` (the `Add`/`Sub`/`Mul` protocols). The checker has verified
            // conformance. Must precede the string-concat `Add` arm below (which would otherwise
            // reject struct+struct).
            (Value::Obj(ha), Value::Obj(hb))
                if matches!(op, Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod)
                    && matches!(self.heap.get(ha), Obj::Struct { .. } | Obj::Enum { .. })
                    && matches!(self.heap.get(hb), Obj::Struct { .. } | Obj::Enum { .. }) =>
            {
                self.struct_arith(op, l, r, span)?
            }
            (Value::Obj(ha), Value::Obj(hb)) if matches!(op, Op::Add) => {
                match (self.heap.get(ha), self.heap.get(hb)) {
                    (Obj::Str(a), Obj::Str(b)) => {
                        let s = format!("{a}{b}");
                        let h = self.heap.alloc(Obj::Str(s.into()));
                        Value::Obj(h)
                    }
                    // List concat (gap #3): `[1,2] + [3,4]` — identical to `.concat` (vm:7688).
                    (Obj::List(a), Obj::List(b)) => {
                        let mut out = a.clone();
                        out.extend(b.iter().copied());
                        Value::Obj(self.heap.alloc(Obj::List(out)))
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
            (Value::Obj(ha), Value::Obj(hb))
                if matches!(op, Op::Sub)
                    && matches!(self.heap.get(ha), Obj::Set(_))
                    && matches!(self.heap.get(hb), Obj::Set(_)) =>
            {
                self.set_op(SetOp::Difference, ha, hb)
            }
            // List repeat (gap #3): `[0] * 3` / `3 * [0]` (commutative, Python-style). `n <= 0` →
            // empty; guard capacity against the Vec overflow abort, like `str.repeat` (vm:7514).
            (Value::Obj(ha), Value::Int(n)) | (Value::Int(n), Value::Obj(ha))
                if matches!(op, Op::Mul) && matches!(self.heap.get(ha), Obj::List(_)) =>
            {
                self.list_repeat(ha, n, span)?
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
        };
        self.push(result);
        Ok(())
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
            return Ok(Value::Obj(self.heap.alloc(Obj::List(Vec::new()))));
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
                Ok(Value::Obj(self.heap.alloc(Obj::List(out))))
            }
            None => Err(self.err("list repeat capacity overflow".to_string(), span)),
        }
    }

    /// Set algebra for the operator forms `| & - ^` (gap #3). Mirrors the
    /// `union`/`intersection`/`difference` set methods (vm:7918) using the cached per-element
    /// hashes (no re-hashing, no user re-entry). `^` (symmetric-difference) has no method form:
    /// it is the union of (mine ∉ other) THEN (other ∉ mine), in that canonical insertion order so
    /// the result's print order is deterministic and parity-equal with the interpreter.
    pub(super) fn set_op(&mut self, op: SetOp, ha: GcRef, hb: GcRef) -> Value {
        let mine = match self.heap.get(ha) {
            Obj::Set(s) => s.entries.clone(),
            _ => unreachable!(),
        };
        let other = match self.heap.get(hb) {
            Obj::Set(s) => s.entries.clone(),
            _ => unreachable!(),
        };
        let mut out = SetData::default();
        let add = |vm: &Vm, set: &mut SetData, he: u64, e: Value| {
            if !set
                .candidates(he)
                .iter()
                .any(|&p| vm.values_equal(set.entries[p].1, e))
            {
                set.push(he, e);
            }
        };
        let in_set = |vm: &Vm, set: &[(u64, Value)], he: u64, e: Value| {
            set.iter()
                .any(|&(h2, e2)| h2 == he && vm.values_equal(e2, e))
        };
        match op {
            SetOp::Union => {
                for (he, e) in mine.iter().chain(other.iter()) {
                    add(self, &mut out, *he, *e);
                }
            }
            SetOp::Intersection => {
                for (he, e) in &mine {
                    if in_set(self, &other, *he, *e) {
                        add(self, &mut out, *he, *e);
                    }
                }
            }
            SetOp::Difference => {
                for (he, e) in &mine {
                    if !in_set(self, &other, *he, *e) {
                        add(self, &mut out, *he, *e);
                    }
                }
            }
            SetOp::SymmetricDifference => {
                for (he, e) in &mine {
                    if !in_set(self, &other, *he, *e) {
                        add(self, &mut out, *he, *e);
                    }
                }
                for (he, e) in &other {
                    if !in_set(self, &mine, *he, *e) {
                        add(self, &mut out, *he, *e);
                    }
                }
            }
        }
        Value::Obj(self.heap.alloc(Obj::Set(out)))
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
        Ok(Value::Obj(self.heap.alloc(Obj::NewType {
            type_key: key,
            inner,
        })))
    }

    /// The underlying primitive `+`/`-`/`*`/`/`/`%` on two scalar values (int or float), with the
    /// SAME overflow / division-by-zero / float semantics as the inline `binary` arms. Shared by the
    /// newtype same-type operator path so it byte-matches a raw int/float op.
    pub(super) fn arith_scalar(
        &self,
        op: &Op,
        a: Value,
        b: Value,
        name: &str,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match (a, b) {
            (Value::Int(a), Value::Int(b)) => {
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
                    Op::Mod => a.checked_rem(b),
                    _ => unreachable!(),
                };
                Ok(Value::Int(v.ok_or_else(|| {
                    self.err(format!("integer overflow in {name}"), span)
                })?))
            }
            (a, b) if is_numeric(a) && is_numeric(b) => {
                let (x, y) = (as_f64(a), as_f64(b));
                // Float arithmetic is total IEEE-754: division/modulo by zero yields inf/-inf/NaN,
                // never a fault. (The INT arm above still faults on /0 and overflow.)
                Ok(Value::Float(match op {
                    Op::Add => x + y,
                    Op::Sub => x - y,
                    Op::Mul => x * y,
                    Op::Div => x / y,
                    Op::Mod => x % y,
                    _ => unreachable!(),
                }))
            }
            _ => Err(self.err(
                format!(
                    "cannot apply {name} to {} and {}",
                    self.type_name(a),
                    self.type_name(b)
                ),
                span,
            )),
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
        let Value::Obj(h) = recv else { unreachable!() };
        match self.heap.get(h) {
            Obj::Struct { name, .. } => {
                let name = name.clone();
                let def = self
                    .program
                    .structs
                    .get(name.as_ref())
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
    /// (Rust would otherwise panic), with a message identical to the interpreter's.
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
        let result = match (l, r) {
            (Value::Int(a), Value::Int(b)) => {
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
                            // Left shift can overflow (drop high bits) like `+ - * / %`; treat
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
                Value::Int(v)
            }
            // Set algebra (gap #3): `|`→union, `&`→intersection, `^`→symmetric-difference on two
            // sets. (`<< >>` stay int-only and fall through to the error below.) Identical to the
            // `.union`/`.intersection` methods; `^` has no method form. Mirrors interp.
            (Value::Obj(ha), Value::Obj(hb))
                if matches!(op, Op::BitOr | Op::BitAnd | Op::BitXor)
                    && matches!(self.heap.get(ha), Obj::Set(_))
                    && matches!(self.heap.get(hb), Obj::Set(_)) =>
            {
                let set_op = match op {
                    Op::BitOr => SetOp::Union,
                    Op::BitAnd => SetOp::Intersection,
                    _ => SetOp::SymmetricDifference,
                };
                self.set_op(set_op, ha, hb)
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
        let found = match container {
            Value::Obj(h) => match self.heap.get(h) {
                Obj::List(items) => {
                    let elems = items.clone();
                    elems.iter().any(|v| self.values_equal(*v, needle))
                }
                Obj::Set(_) => {
                    let hx = self.hash_key_rooted(needle, &[Value::Obj(h), needle], span)?;
                    let Obj::Set(s) = self.heap.get(h) else {
                        unreachable!()
                    };
                    s.candidates(hx)
                        .iter()
                        .any(|&p| self.values_equal(s.entries[p].1, needle))
                }
                Obj::Map(_) => {
                    let hk = self.hash_key_rooted(needle, &[Value::Obj(h), needle], span)?;
                    let Obj::Map(m) = self.heap.get(h) else {
                        unreachable!()
                    };
                    m.candidates(hk)
                        .iter()
                        .any(|&p| self.values_equal(m.entries[p].1, needle))
                }
                Obj::Str(_) => {
                    let Value::Obj(nh) = needle else {
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
        self.push(Value::Bool(found));
        Ok(())
    }

    pub(super) fn compare_op(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        let r = self.pop();
        let l = self.pop();
        // Same-newtype ordering: `Meters < Meters` UNWRAPS both and compares the underlyings with
        // their NATIVE ordering (the checker rejected `Meters < float` / `< Seconds`). Not a user
        // `compare` method — the underlying's native compare. Must precede the struct/enum overload.
        if let (Value::Obj(hl), Value::Obj(hr)) = (l, r)
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
            self.push(Value::Bool(bres));
            return Ok(());
        }
        // Operator overloading: ordering on two structs dispatches to `compare(self, other) -> int`
        // (the `Comparable` protocol). The checker has verified conformance. Equality stays
        // structural; only ordering is overloaded. Mirrors `interp::struct_ordering`.
        if let (Value::Obj(hl), Value::Obj(hr)) = (l, r)
            && matches!(self.heap.get(hl), Obj::Struct { .. } | Obj::Enum { .. })
            && matches!(self.heap.get(hr), Obj::Struct { .. } | Obj::Enum { .. })
        {
            return self.struct_ordering(op, l, r, span);
        }
        let b = self.ordered_bool(op, l, r, span)?;
        self.push(Value::Bool(b));
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
            None if is_numeric(a) && is_numeric(b) => Ok(false),
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
        self.push(Value::Bool(b));
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
        match self.guarded(|vm| vm.run_proto(proto, home, None, vec![l, r], true, false, span))? {
            Value::Int(n) => Ok(n.cmp(&0)),
            other => Err(self.err(
                format!("compare() must return int, got {}", self.type_name(other)),
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
        match v {
            // A struct key dispatches its user `hash()` (re-entrant). Everything else is scalar.
            Value::Obj(h) => match self.heap.get(h) {
                Obj::Struct { .. } => self.struct_hash(v, span),
                // An enum key dispatches its user `hash(self) -> int` via the shared enum-aware
                // resolver, mirroring the struct path (re-entrant — may allocate / trigger GC).
                Obj::Enum { .. } => self.enum_hash(v, span),
                // A newtype key dispatches its user `hash(self) -> int` (opt-in — the checker rejects
                // a newtype with no `hash` as a key, even over an intrinsically-hashable underlying).
                Obj::NewType { .. } => self.newtype_hash(v, span),
                Obj::Str(_) | Obj::Bytes(_) => Ok(self.scalar_hash(v)),
                _ => Err(self.err(
                    format!(
                        "{} is not hashable (cannot be a map/set key)",
                        self.type_name(v)
                    ),
                    span,
                )),
            },
            _ => Ok(self.scalar_hash(v)),
        }
    }

    /// Infallible hash for scalar keys (int/float/bool/nil/str). Numeric values hash by canonical
    /// f64 bits so `3` and `3.0` collide; str by content. Non-scalar values fall back to `0` (a
    /// correctness-safe degenerate hash — `values_equal` still confirms each probe).
    pub(super) fn scalar_hash(&self, v: Value) -> u64 {
        use std::hash::{Hash, Hasher};
        match v {
            // Normalise zero so `Int(0)`, `+0.0`, and `-0.0` (all `values_equal`) hash identically —
            // `(-0.0).to_bits() != (0.0).to_bits()` would otherwise break the hash invariant.
            Value::Int(n) => (if n == 0 { 0.0 } else { n as f64 }).to_bits(),
            Value::Float(f) => (if f == 0.0 { 0.0 } else { f }).to_bits(),
            Value::Bool(b) => b as u64,
            Value::Nil => 0,
            Value::Obj(h) => match self.heap.get(h) {
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
                _ => 0,
            },
        }
    }

    /// Dispatch a struct key's user `hash(self) -> int`, returning its `i64` as a `u64`. Mirrors
    /// [`struct_compare`] (re-entrant via `run_proto`).
    pub(super) fn struct_hash(&mut self, v: Value, span: Span) -> Result<u64, RuntimeError> {
        let Value::Obj(h) = v else { unreachable!() };
        let Obj::Struct { name, .. } = self.heap.get(h).clone() else {
            unreachable!()
        };
        let def = self
            .program
            .structs
            .get(name.as_ref())
            .cloned()
            .ok_or_else(|| self.err(format!("unknown struct type '{name}'"), span))?;
        // A ZERO-FIELD struct with no `hash` method hashes to a constant (0): it has no state, so
        // there is nothing to hash. `==`'s type-tag guard keeps distinct empty-struct types unequal
        // despite the shared hash. Mirrors the checker's zero-field `Hashable` intrinsic and the
        // interpreter's identical constant (two-engine parity).
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
        match self.guarded(|vm| vm.run_proto(proto, home, None, vec![v], true, false, span))? {
            Value::Int(n) => Ok(n as u64),
            other => Err(self.err(
                format!("hash() must return int, got {}", self.type_name(other)),
                span,
            )),
        }
    }

    /// Dispatch an enum key's user `hash(self) -> int` via the shared enum-aware
    /// [`resolve_overload_method`], mirroring [`struct_hash`] (re-entrant via `run_proto`).
    pub(super) fn enum_hash(&mut self, v: Value, span: Span) -> Result<u64, RuntimeError> {
        let (proto, home) = self.resolve_overload_method(v, "hash", span)?;
        match self.guarded(|vm| vm.run_proto(proto, home, None, vec![v], true, false, span))? {
            Value::Int(n) => Ok(n as u64),
            other => Err(self.err(
                format!("hash() must return int, got {}", self.type_name(other)),
                span,
            )),
        }
    }

    /// Dispatch a newtype key's user `hash(self) -> int` via the shared resolver (mirrors `enum_hash`;
    /// re-entrant via `run_proto`). The checker guarantees a key-used newtype defines `hash`.
    pub(super) fn newtype_hash(&mut self, v: Value, span: Span) -> Result<u64, RuntimeError> {
        let (proto, home) = self.resolve_overload_method(v, "hash", span)?;
        match self.guarded(|vm| vm.run_proto(proto, home, None, vec![v], true, false, span))? {
            Value::Int(n) => Ok(n as u64),
            other => Err(self.err(
                format!("hash() must return int, got {}", self.type_name(other)),
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
        for &r in roots {
            self.push(r);
        }
        let res = self.hash_value(key, span);
        for _ in roots {
            self.pop();
        }
        res
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
    /// serializer (a) FAULTS on a generator / captured `ref` / cyclic key — nonsensical for a plain
    /// single-thread insert that previously stored the key by reference and worked — and (b) rebuilds
    /// every by-reference sub-value (a `Closure`/`Channel`/`Shared`/… field) with a FRESH handle,
    /// which `values_equal` (identity-only for those arms) then never matches, so a later lookup of
    /// the same key misses. [`Vm::snapshot_value`] instead copies only the mutable, structurally-
    /// compared arms and keeps identity/by-reference sub-values by handle, so the snapshot stays
    /// `values_equal` to the original (its cached hash is therefore still valid).
    pub(super) fn snapshot_key(&mut self, key: Value) -> Value {
        match key {
            Value::Obj(h)
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
        let Value::Obj(h) = v else {
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
            Obj::Struct { fields, .. } => fields.clone(),
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
        let Value::Obj(h) = v else {
            return v; // scalars
        };
        if depth > MAX_STRUCTURAL_DEPTH {
            return v; // absurdly deep (non-cyclic) key: stop copying, alias the tail (no overflow)
        }
        if let Some(&c) = visited.get(&h) {
            return Value::Obj(c); // already copied (shared sub-value or cycle back-edge)
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
                Value::Obj(nh)
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
                Value::Obj(nh)
            }
            Obj::Struct { name, tid, fields } => {
                let (name, tid, fields) = (name.clone(), *tid, fields.clone());
                let nh = self.heap.alloc(Obj::Struct {
                    name,
                    tid,
                    fields: fields.clone(),
                });
                visited.insert(h, nh);
                let copied: Vec<Value> = fields
                    .iter()
                    .map(|&f| self.snapshot_value(f, visited, depth + 1))
                    .collect();
                if let Obj::Struct { fields, .. } = self.heap.get_mut(nh) {
                    *fields = copied;
                }
                Value::Obj(nh)
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
                Value::Obj(nh)
            }
            Obj::NewType { type_key, inner } => {
                let (type_key, inner) = (type_key.clone(), *inner);
                let nh = self.heap.alloc(Obj::NewType { type_key, inner });
                visited.insert(h, nh);
                let ci = self.snapshot_value(inner, visited, depth + 1);
                if let Obj::NewType { inner, .. } = self.heap.get_mut(nh) {
                    *inner = ci;
                }
                Value::Obj(nh)
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
                Value::Obj(nh)
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
                Value::Obj(nh)
            }
            // A `bytearray` is mutable + structurally compared but a GC LEAF (raw bytes, no children):
            // copy the buffer, no recursion, no visited entry (it can't participate in a cycle).
            Obj::ByteArray(b) => Value::Obj(self.heap.alloc(Obj::ByteArray(b.clone()))),
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
        self.push(Value::Obj(src_h));
        let snap_h = {
            let elems = match self.heap.get(src_h) {
                Obj::List(v) => v.clone(),
                _ => unreachable!("list_sort on non-list"),
            };
            self.heap.alloc(Obj::List(elems))
        };
        self.push(Value::Obj(snap_h)); // ROOT the snapshot across the comparator calls
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
        Ok(Value::Nil)
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
        match (l, r) {
            (Value::Int(a), Value::Int(b)) => Some(a.cmp(&b)),
            (a, b) if is_numeric(a) && is_numeric(b) => as_f64(a).partial_cmp(&as_f64(b)),
            (Value::Obj(ha), Value::Obj(hb)) => match (self.heap.get(ha), self.heap.get(hb)) {
                (Obj::Str(a), Obj::Str(b)) => Some(a.cmp(b)),
                // `bytes`/`bytearray` order lexicographically by byte (Python parity), including
                // cross-type (Python `b"a" < bytearray(b"b")` compares by content).
                (Obj::Bytes(a), Obj::Bytes(b)) => Some(a.cmp(b)),
                (Obj::ByteArray(a), Obj::ByteArray(b)) => Some(a.cmp(b)),
                (Obj::Bytes(a), Obj::ByteArray(b)) => Some(a.as_ref().cmp(b.as_slice())),
                (Obj::ByteArray(a), Obj::Bytes(b)) => Some(a.as_slice().cmp(b.as_ref())),
                _ => None,
            },
            _ => None,
        }
    }

    /// Structural equality mirroring `interp::values_equal`. Thin `bool` wrapper over the
    /// depth-guarded worker (kept so the ~39 existing call sites — many in hot hash-probe paths
    /// bound by `values_equal(a,b) ⇒ hash(a)==hash(b)` — are untouched). A depth-exceeded fault
    /// (cyclic data) degrades to "not equal" here; the language `==`/`!=` ops surface it instead.
    pub(super) fn values_equal(&self, l: Value, r: Value) -> bool {
        self.values_equal_guarded(l, r, 0, Span { line: 1, col: 1 })
            .unwrap_or(false)
    }

    /// Depth-guarded structural equality. Returns `Err` (recoverable) once recursion exceeds
    /// [`MAX_STRUCTURAL_DEPTH`] — guarding against cyclic data structures overflowing the host stack.
    pub(super) fn values_equal_guarded(
        &self,
        l: Value,
        r: Value,
        depth: usize,
        span: Span,
    ) -> Result<bool, RuntimeError> {
        if depth > MAX_STRUCTURAL_DEPTH {
            return Err(self.err(
                "maximum structural depth (10000) exceeded (cyclic data structure?)".to_string(),
                span,
            ));
        }
        match (l, r) {
            (a, b) if is_numeric(a) && is_numeric(b) => Ok(as_f64(a) == as_f64(b)),
            (Value::Bool(a), Value::Bool(b)) => Ok(a == b),
            (Value::Nil, Value::Nil) => Ok(true),
            (Value::Obj(ha), Value::Obj(hb)) => {
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
                        for (x, y) in a.iter().zip(&b) {
                            if !self.values_equal_guarded(*x, *y, depth + 1, span)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                    (Obj::Tuple(a), Obj::Tuple(b)) => {
                        if a.len() != b.len() {
                            return Ok(false);
                        }
                        let (a, b): (Vec<Value>, Vec<Value>) = (a.clone(), b.clone());
                        for (x, y) in a.iter().zip(&b) {
                            if !self.values_equal_guarded(*x, *y, depth + 1, span)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
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
                        for (ka, va) in &ae {
                            let mut found = false;
                            for (kb, vb) in &be {
                                if self.values_equal_guarded(*ka, *kb, depth + 1, span)?
                                    && self.values_equal_guarded(*va, *vb, depth + 1, span)?
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
                    }
                    // Sets are unordered: equal iff same size and every element of `a` is in `b`.
                    (Obj::Set(a), Obj::Set(b)) => {
                        if a.entries.len() != b.entries.len() {
                            return Ok(false);
                        }
                        let ae: Vec<Value> = a.entries.iter().map(|(_, x)| *x).collect();
                        let be: Vec<Value> = b.entries.iter().map(|(_, x)| *x).collect();
                        for x in &ae {
                            let mut found = false;
                            for y in &be {
                                if self.values_equal_guarded(*x, *y, depth + 1, span)? {
                                    found = true;
                                    break;
                                }
                            }
                            if !found {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                    (
                        Obj::Struct {
                            name: na,
                            fields: fa,
                            ..
                        },
                        Obj::Struct {
                            name: nb,
                            fields: fb,
                            ..
                        },
                    ) => {
                        // Positional structural compare: the `na != nb` guard preserves type
                        // distinction (same name ⇒ same StructDef ⇒ identical field order), so a
                        // by-position value compare suffices — no per-field name clone needed.
                        if na != nb || fa.len() != fb.len() {
                            return Ok(false);
                        }
                        let fa: Vec<Value> = fa.clone();
                        let fb: Vec<Value> = fb.clone();
                        for (va, vb) in fa.iter().zip(&fb) {
                            if !self.values_equal_guarded(*va, *vb, depth + 1, span)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
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
                        for (x, y) in pa.iter().zip(&pb) {
                            if !self.values_equal_guarded(*x, *y, depth + 1, span)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
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
                        self.values_equal_guarded(ia, ib, depth + 1, span)
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
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => x.cmp(&y),
            (Value::Float(x), Value::Float(y)) => x.total_cmp(&y),
            (Value::Obj(ha), Value::Obj(hb)) => match (self.heap.get(ha), self.heap.get(hb)) {
                (Obj::Str(x), Obj::Str(y)) => x.cmp(y),
                _ => Equal,
            },
            _ => Equal,
        }
    }

    // ----- calls -----
}
