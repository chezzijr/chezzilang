//! Checker: expression inference core: interpolation chunks, `infer`, slots, hover records, else
//! guards, `recover`.
//! Split out of `src/checker/pattern.rs`. `super::*` is the parent module.

use super::*;
use crate::ast::consteval;
use crate::ast::{CarrierTag, RecoverForm};

/// What the VALUE children of an expression receive as their expected type, from the node's own
/// hint ([`child_hint`]). A child that is not a value child (a condition, a scrutinee, a callee, a
/// subscript, a statement of a block) receives none. The node itself may still read its hint (a
/// generic fn value, an overload pick, a constant's width).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ChildHint {
    /// No child sees the hint: a leaf, a field/`?.` receiver, an interpolation fragment, a range
    /// bound, an index object.
    None,
    /// Each value child sees the node's hint unchanged, as a seed: an operator's operands, an
    /// `if`/`match` arm (an arm owns the slot when the node owns it, `branch.rs`).
    Same,
    /// The node's own arm takes the hint and installs each child's part of it: a list/set/map
    /// item, a tuple element, a comprehension element, a closure body, a call argument's
    /// parameter, the payload of a prefix `?x`, the error of a prefix `!e`.
    Element,
    /// The child is the carrier the node unwraps (`x?`, `x ?? d`'s `x`, `x else: ...`): it gets a
    /// payload hint (`Hint::payload`), "a `T!E` or `T?` whose `T` is the hint's payload".
    /// A generic call binds its carrier result's payload against it (`Checker::hint_shape`). A
    /// seed, never a wrap (TICKET-227). `d` of `x ?? d` is the node's value: it keeps the hint.
    Unwrap,
    /// The child is the payload the node wraps (`recover: x` builds `T!`): it gets the hint's `T`.
    Payload,
}

/// THE table of [`ChildHint`]s: one rule per expression kind, read by every child site that does
/// not forward through its own arm (`Checker::with_child_hint`). Exhaustive: a new kind decides
/// what its children see.
pub(super) fn child_hint(kind: &ExprKind) -> ChildHint {
    match kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Interp(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::NoneLit
        | ExprKind::Pass
        | ExprKind::Ident(_)
        | ExprKind::Range { .. }
        | ExprKind::Field { .. }
        | ExprKind::Index { .. }
        | ExprKind::OptChain { .. } => ChildHint::None,
        ExprKind::Unary { op, .. } => match op {
            UnaryOp::Neg | UnaryOp::Not => ChildHint::Same,
            UnaryOp::ErrVal | UnaryOp::Wrap => ChildHint::Element,
        },
        ExprKind::Binary { .. }
        | ExprKind::Compare { .. }
        | ExprKind::Match { .. }
        | ExprKind::IfElse { .. } => ChildHint::Same,
        ExprKind::List(..)
        | ExprKind::Tuple(_)
        | ExprKind::Map(_)
        | ExprKind::Set(_)
        | ExprKind::Comprehension { .. }
        | ExprKind::Closure { .. }
        | ExprKind::Call { .. } => ChildHint::Element,
        ExprKind::Try(_) | ExprKind::NullCoalesce { .. } | ExprKind::ElseGuard { .. } => {
            ChildHint::Unwrap
        }
        ExprKind::Recover(..) => ChildHint::Payload,
    }
}

impl Checker {
    /// Type-check an interpolated string literal's chunks ([`ExprKind::Interp`], built by the parser
    /// from the holes the lexer found). Every fragment `Expr` is run through the normal
    /// `infer_value` path: undefined names, type/method/arity mismatches and void-call fragments all
    /// surface here as compile errors instead of slipping past `check` to panic the compiler
    /// (`global_slot`) or fault at runtime. A fragment's span is the real physical position of its
    /// first char (the lexer lexed it in place), so a fragment error points at the EXPRESSION, where
    /// CPython carets inside an f-string, and two fragments never share a witness/keyword/carrier
    /// table key. Format-spec *validation* against the value's type stays below. Always returns
    /// `Ty::Str`.
    pub(super) fn check_interp_chunks(&mut self, chunks: &[crate::ast::Chunk], span: Span) -> Ty {
        // A value+keyword call inside a `{…}` fragment is keyed by (string span, fragment ordinal).
        // That pair used to be what kept two fragments whose first named-arg value shared a
        // fragment-relative column off one table slot; since M24-6 a fragment's spans are real
        // physical positions, so the pair is belt-and-braces (see `WitnessKey`'s doc).
        // Save/restore for nested interpolations. The compiler keeps the identical pair.
        let saved_ctx = self.kw_frag_ctx;
        let saved_ord = self.kw_frag_ord;
        let mut ord = 0usize;
        for chunk in chunks {
            if let crate::ast::Chunk::Expr(e, spec, fields) = chunk {
                self.kw_frag_ctx = span;
                self.kw_frag_ord = ord;
                // No re-anchoring: a fragment is re-lexed with the literal's absolute line AND
                // column, so its own span is a real source position and a fragment error points at
                // the EXPRESSION, exactly where CPython points inside an f-string (measured on
                // 3.14.6: `print(f"hello {f'inner {nope} x'} world")` carets `nope` itself, not the
                // literal). Three anchors lived here across this milestone — one on a cloned root,
                // one beside the AST — and each was a workaround for the column being fake; with a
                // real column there is nothing left to anchor, and the checker finally agrees with
                // the compiler, which never re-anchored.
                let ty = self.infer_value(e);
                // The spec's nested width/precision fields evaluate after the value, width first
                // (the order `compile_interp` emits them). Each must be an `int`; `Unknown` keeps
                // the runtime backstop (`fmtspec::field_from_int`).
                if let Some(fs) = spec {
                    let mut slots = [(fs.dyn_width, "width"), (fs.dyn_precision, "precision")]
                        .into_iter()
                        .filter(|(dynamic, _)| *dynamic)
                        .map(|(_, what)| what);
                    for field in fields {
                        let fty = self.infer_value(field);
                        if let Some(what) = slots.next()
                            && !matches!(fty, Ty::Int | Ty::Unknown)
                        {
                            self.error(
                                field.span,
                                format!(
                                    "format spec: a nested {what} field must be an int, found {fty}"
                                ),
                            );
                        }
                    }
                }
                // Static format-spec/value-type check: a CONCRETE static type is checked at COMPILE
                // time (same wording the runtime backstop would emit — single-sourced in
                // `fmtspec`); only `Unknown`, a generic `Param(T)` and a protocol existential keep
                // the runtime backstop (see `format_spec_kind`).
                if let Some(fs) = spec
                    && let Some((kind, text_form)) = {
                        let ty = self.pinning_value_ty(&ty);
                        self.format_spec_kind(&ty)
                    }
                    && let Err(msg) = crate::fmtspec::spec_valid_for_scalar(fs, kind)
                {
                    // TICKET-124 (W13-18) / TICKET-142 (W14-19): a value that renders as its text
                    // form goes through the runtime's `FmtArg::Other` → `render_str` path — the same
                    // string-format rules a scalar `Str` follows — so a spec that fails those rules
                    // is provably wrong here too.
                    if text_form {
                        self.error(span, format!("{msg} ({ty} is formatted as its text form)"));
                    } else {
                        self.error(span, msg);
                    }
                }
                ord += 1;
            }
        }
        self.kw_frag_ctx = saved_ctx;
        self.kw_frag_ord = saved_ord;
        Ty::Str
    }

    /// The [`crate::fmtspec::ScalarKind`] a format spec on a value of static type `ty` is checked
    /// against, plus whether the value renders as its text form (for the diagnostic). `None` keeps
    /// the runtime backstop: `Unknown`, a generic `Param(T)`, a protocol existential, a module.
    fn format_spec_kind(&self, ty: &Ty) -> Option<(crate::fmtspec::ScalarKind, bool)> {
        use crate::fmtspec::ScalarKind;
        match scalar_kind_of(ty) {
            Some(kind) => Some((kind, false)),
            None if renders_as_text(ty) => Some((ScalarKind::Str, true)),
            None => None,
        }
    }

    /// Infer an expression that is used in **value position** (assignment RHS, a call/collection
    /// argument, a binary/unary operand, an index/range bound, …). `nil` is a return-only / void
    /// type, never a writable value: a void call's result must not silently propagate into a binding
    /// or another expression. So if the expr is exactly `Ty::Nil`, report it and degrade to `Unknown`
    /// (suppressing the cascade). A bare void call AS A STATEMENT keeps using plain `infer` (legal),
    /// as does a fn/closure RETURN expr (returning nil just makes a void fn — not "using nil").
    pub(super) fn infer_value(&mut self, expr: &Expr) -> Ty {
        let ty = self.infer(expr);
        if let Ty::Module(m) = &ty {
            let m = m.clone();
            self.error(
                expr.span,
                format!("module '{m}' is not a value — a module is only a qualifier, write '{m}.<member>'"),
            );
            return Ty::Unknown;
        }
        if ty == Ty::Nil {
            self.error(
                expr.span,
                "expression returns no value (None) and cannot be used as a value".to_string(),
            );
            return Ty::Unknown;
        }
        ty
    }

    /// TICKET-227 — install `hint` as the expected type of `e`. With `slot`, `e` OWNS it: only
    /// `infer` of `e` may wrap `e` into the hint's carrier. Without, the hint is a seed (it guides
    /// inference and never wraps). A synthesized node never owns a hint.
    pub(super) fn install_hint(&mut self, e: &Expr, hint: Ty, slot: bool) {
        self.install_whole_hint(e, Hint::plain(hint), slot);
    }

    /// [`Self::install_hint`] with the hint's payload marker kept: a branch arm forwards its
    /// node's whole hint ([`ChildHint::Same`]), a payload hint under `?`, `??` or `else:` included.
    pub(super) fn install_whole_hint(&mut self, e: &Expr, hint: Hint, slot: bool) {
        self.expected_hint = Some(hint);
        self.hint_owner = (slot && e.id.0 != crate::ast::NodeId::SYNTH.0).then_some(e.id);
    }

    /// TICKET-227 — infer `e` in value position as the value of a typed slot `slot`: `e` owns the
    /// slot's type and wraps into it where [`Self::meet_slot`] says so.
    pub(super) fn infer_value_in(&mut self, e: &Expr, slot: &Ty) -> Ty {
        self.install_hint(e, slot.clone(), true);
        let t = self.infer_value(e);
        self.expected_hint = None;
        self.hint_owner = None;
        t
    }

    pub(super) fn infer(&mut self, expr: &Expr) -> Ty {
        // TICKET-269: the receiver `bracket_is_index` already inferred, with its diagnostics.
        if let Some((_, ty)) = self.handed_recv.take_if(|r| r.0.0 == expr.id.0) {
            return ty;
        }
        // TICKET-225: a var bound since a type was stored reads as its binding.
        if self.tyvars.borrow().any()
            && let Some(h) = &self.expected_hint
        {
            let ty = self.zonk(&h.ty);
            self.expected_hint = Some(Hint { ty, ..h.clone() });
        }
        // TICKET-227: the slot this node owns, if the hint was installed FOR it. Taken here, so
        // no child ever sees the owner.
        let owner = self.hint_owner.take();
        let slot = match owner {
            Some(o) if o.0 == expr.id.0 => self.expected_hint.as_ref().map(|h| h.ty.clone()),
            _ => None,
        };
        self.hint_owned = slot.is_some();
        let ty = self.infer_kind(expr);
        let ty = if self.tyvars.borrow().any() {
            self.zonk(&ty)
        } else {
            ty
        };
        if matches!(self.untyped_call, Some(None))
            && !self.generic_arg_prepass
            && matches!(expr.kind, ExprKind::Call { .. })
            && ty.is_unknown()
            && !self.call_diverges(expr)
        {
            self.untyped_call = Some(Some(expr.span));
        }
        // EDITOR HOVER probe: record this expr's type if its leaf/field anchor is the cursor token.
        // No-op (one `Option` check) unless a probe is armed. Children infer before parents and only
        // LEAF kinds record, so a parent expression never overwrites the smaller symbol's type.
        if self.hover_probe.is_some() {
            self.hover_record_expr(expr, &ty);
        }
        match &slot {
            Some(s) => self.meet_slot(s, expr, ty),
            None => ty,
        }
    }

    /// TICKET-227 (D3) — THE one place an implicit wrap is decided: the value `value` of type `ty`
    /// meets the typed slot `slot` it owns. A plain `T` at a `T?`/`T!E` slot records a wrap and
    /// takes the slot's type; anything else keeps its own type, and the slot's `assignable` compare
    /// reports a misfit. Called only by [`Self::infer`].
    fn meet_slot(&mut self, slot: &Ty, value: &Expr, ty: Ty) -> Ty {
        match self.wrap_mode(slot, &ty) {
            Some(w) => {
                self.record_wrap(value.id, w, value.span);
                slot.clone()
            }
            None => ty,
        }
    }

    /// TICKET-142 (W14-33): the dispatch every expression inference passes through. Wraps
    /// [`Self::infer_kind_inner`] with the constant-overflow check: at the root of each maximal
    /// arithmetic (`Binary`/`Unary`) tree, run ONE `consteval::eval` over the whole tree and report
    /// each overflow once. A child of a `Binary`/`Unary` sees `arith_parent` and skips (its parent's
    /// scan already entered it); a child of any other node (a call argument under a `+`) starts its
    /// own tree. Each node is scanned at most once, so the check is linear even on a
    /// `MAX_AST_DEPTH` chain. Must not touch `hint_owned` (the inner fn takes it first).
    /// A walk with `inferring_ret` set rolls back its diagnostics and `const_overflow_seen`, so it
    /// skips the scan and the real walk reports each overflow once (TICKET-183).
    /// A prefix `?x` / `!e` is not arithmetic: its operand starts its own tree and meets its own slot.
    pub(super) fn infer_kind(&mut self, expr: &Expr) -> Ty {
        let covered = self.arith_parent;
        let is_arith = match &expr.kind {
            ExprKind::Unary { op, .. } => matches!(op, UnaryOp::Neg | UnaryOp::Not),
            ExprKind::Binary { .. } => true,
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Interp(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::NoneLit
            | ExprKind::Pass
            | ExprKind::Ident(_)
            | ExprKind::List(..)
            | ExprKind::Tuple(_)
            | ExprKind::Map(_)
            | ExprKind::Set(_)
            | ExprKind::Comprehension { .. }
            | ExprKind::Compare { .. }
            | ExprKind::Range { .. }
            | ExprKind::Call { .. }
            | ExprKind::Field { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => false,
        };
        if is_arith && !covered {
            self.scan_const_overflow(expr);
        }
        if !covered {
            self.const_meets_slot(expr, None);
        }
        self.arith_parent = is_arith;
        let ty = self.infer_kind_inner(expr);
        self.arith_parent = covered;
        // TICKET-218: a C width is a SLOT tag; a value read through any expression is its scalar
        // (owner rules 1 and 3: an `int8` value is an `int`, arithmetic on it yields `int`).
        match ty {
            Ty::Width(_) => ty.scalar().clone(),
            t @ Ty::Int
            | t @ Ty::Float
            | t @ Ty::Bool
            | t @ Ty::Str
            | t @ Ty::Bytes
            | t @ Ty::ByteArray
            | t @ Ty::Nil
            | t @ Ty::List(_)
            | t @ Ty::Map(..)
            | t @ Ty::Set(_)
            | t @ Ty::Func { .. }
            | t @ Ty::Tuple(_)
            | t @ Ty::Struct(..)
            | t @ Ty::Enum(..)
            | t @ Ty::Param(_)
            | t @ Ty::Result(..)
            | t @ Ty::Option(_)
            | t @ Ty::Channel(_)
            | t @ Ty::Shared(_)
            | t @ Ty::Atomic(_)
            | t @ Ty::AtomicInt
            | t @ Ty::RwShared(_)
            | t @ Ty::Executor
            | t @ Ty::Socket
            | t @ Ty::Listener
            | t @ Ty::Writer
            | t @ Ty::Reader
            | t @ Ty::Ptr
            | t @ Ty::Protocol(..)
            | t @ Ty::Module(_)
            | t @ Ty::Unknown
            | t @ Ty::Var(_) => t,
        }
    }

    /// Run `f`, the inference of a child of a node whose kind has the rule `rule`
    /// ([`child_hint`]), under the hint that rule gives the child; then put the node's own hint back
    /// for the node itself and its other children. `Same` and `Element` leave the hint in place
    /// (the node's own arm forwards or takes it).
    pub(super) fn with_child_hint<R>(
        &mut self,
        rule: ChildHint,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        match rule {
            ChildHint::Same | ChildHint::Element => return f(self),
            ChildHint::None | ChildHint::Unwrap | ChildHint::Payload => {}
        }
        let hint = self.expected_hint.take();
        let owner = self.hint_owner.take();
        if let Some(h) = &hint {
            let payload = h.ty.slot_payload().clone();
            self.expected_hint = match rule {
                // A payload hint names a carrier's `T`, which is not itself a carrier the child
                // unwraps: an `x?` under a payload hint gets none.
                ChildHint::Unwrap if h.payload => None,
                ChildHint::Unwrap => Some(Hint {
                    ty: payload,
                    payload: true,
                }),
                // `recover: x` builds the node's carrier: `x` is its payload (under a payload
                // hint the node's value is that carrier, and `h.ty` is already its `T`).
                ChildHint::Payload if h.payload => Some(Hint::plain(h.ty.clone())),
                ChildHint::Payload => Some(Hint::plain(payload)),
                ChildHint::None | ChildHint::Same | ChildHint::Element => None,
            };
        }
        let r = f(self);
        self.expected_hint = hint;
        self.hint_owner = owner;
        r
    }

    /// TICKET-225 (R5, Go's untyped constants) — the one place a constant meets its slot. A literal
    /// or a constant expression (`1 << 8`, `3e38 + 3e38`) is checked against the C width of the
    /// expected type it is inferred under: `constant 256 does not fit int8 (-128..127)`. `slot` is
    /// `None` from `infer_kind`, which reads `expected_hint`; an assignment passes its target type,
    /// which no hint carries (`infer` reads an lvalue as its scalar). Skipped while inferring a
    /// return (DEC-183) or in the generic-arg prepass; one report per span.
    /// A constant that wraps into a carrier slot (`o: int8? = 300`) meets the payload's width; one
    /// layer, as `wrap_mode` wraps one layer.
    pub(super) fn const_meets_slot(&mut self, expr: &Expr, slot: Option<&Ty>) {
        use crate::ast::consteval::{Const, Fold};
        if self.inferring_ret
            || self.generic_arg_prepass
            || !matches!(
                expr.kind,
                ExprKind::Int(_)
                    | ExprKind::Float(_)
                    | ExprKind::Unary { .. }
                    | ExprKind::Binary { .. }
            )
        {
            return;
        }
        let Some(w) = slot
            .or(self.expected_hint.as_ref().map(|h| &h.ty))
            .and_then(|h| h.width().or_else(|| h.slot_payload().width()))
            .cloned()
        else {
            return;
        };
        let float = w == crate::native::cffi::CType::Float32;
        let shown = match consteval::eval(expr, &mut self.const_scan_visits) {
            Fold::Value(Const::Float(f)) if float && !w.fits_f64(f) => format!("{f:e}"),
            Fold::Value(Const::Int(k)) if !float && !w.fits_int(k, true) => k.to_string(),
            Fold::Value(_) | Fold::Fault(..) | Fold::NotConst => return,
        };
        if self.const_overflow_seen.insert(expr.span) {
            let name = w.width_name().unwrap_or("?");
            let range = w.range_text();
            self.error(
                expr.span,
                format!("constant {shown} does not fit {name} {range}"),
            );
        }
    }

    /// TICKET-225 / TICKET-227: infer `e` as the value of the typed slot `slot` (a constant meets
    /// the slot's width; a plain value wraps into a carrier slot); `None` infers with no hint. For
    /// value sites with a statement-tail slot (yield, inline body, closure body).
    pub(super) fn infer_in_slot(&mut self, e: &Expr, slot: Option<Ty>) -> Ty {
        match slot {
            Some(s) => {
                self.install_hint(e, s, true);
                let t = self.infer(e);
                self.expected_hint = None;
                self.hint_owner = None;
                t
            }
            None => self.infer(e),
        }
    }

    fn infer_kind_inner(&mut self, expr: &Expr) -> Ty {
        let owned = std::mem::take(&mut self.hint_owned);
        match &expr.kind {
            ExprKind::Int(_) => Ty::Int,
            ExprKind::Float(_) => Ty::Float,
            ExprKind::Str(_) => Ty::Str,
            // Fragments are real children, normalized (named/default/variadic args) by `desugar`.
            ExprKind::Interp(chunks) => self.with_child_hint(child_hint(&expr.kind), |c| {
                c.check_interp_chunks(chunks, expr.span)
            }),
            ExprKind::RawStr(_) => Ty::Str, // verbatim `str`, no interpolation to check
            ExprKind::Bytes(_) => Ty::Bytes,
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::NoneLit => self.infer_ident(expr, crate::lexer::NONE, expr.span),
            ExprKind::Pass => Ty::Nil,
            ExprKind::Ident(name) => self.infer_ident(expr, name, expr.span),
            ExprKind::List(items, _) => {
                // Consume any expected-type hint (a `List[E]` slot: an annotated `let`, a call
                // arg, a return position — or the synthesized variadic list). `take()` so the
                // hint drives THIS literal's element type and never leaks into a nested element
                // call. `None` keeps the ordinary bottom-up inference.
                let hint = self
                    .expected_hint
                    .take()
                    .map(|h| h.ty.slot_payload().clone());
                self.infer_list(items, hint.as_ref())
            }
            ExprKind::Tuple(items) => {
                // TICKET-124 (W13-13): consume any expected-type hint (a `(Box[Named], int)` slot),
                // same `take()`-then-project contract as `List`/`Map` above, so a nested ctor
                // literal inside the tuple sees its own element's hint instead of the bare
                // bottom-up type.
                let hint = self.expected_hint.take().map(|h| h.ty);
                let tys = match hint.map(|h| h.slot_payload().clone()) {
                    Some(Ty::Tuple(hs)) if hs.len() == items.len() => items
                        .iter()
                        .zip(&hs)
                        .map(|(e, h)| {
                            if ty_concrete_but(h, &|n| self.rigid_param(n, &[])) {
                                self.infer_arg(e, Some(h))
                            } else {
                                self.infer_value(e)
                            }
                        })
                        .collect(),
                    _ => items.iter().map(|e| self.infer_value(e)).collect(),
                };
                Ty::Tuple(tys)
            }
            // Same `take()`-then-project contract as the `List` arm above. Without the `take()` the
            // OUTER `Map[str, List[int]]` stayed in the slot and was consumed — wasted — by the first
            // entry's own inference.
            ExprKind::Map(entries) => {
                let hint = self
                    .expected_hint
                    .take()
                    .map(|h| h.ty.slot_payload().clone());
                self.infer_map(entries, hint.as_ref())
            }
            ExprKind::Set(elems) => {
                let hint = self
                    .expected_hint
                    .take()
                    .map(|h| h.ty.slot_payload().clone());
                self.infer_set(elems, hint.as_ref())
            }
            ExprKind::Comprehension {
                kind,
                key,
                elem,
                clauses,
            } => self.infer_comprehension(*kind, key.as_deref(), elem, clauses),
            ExprKind::Unary { op, expr: inner } => self.infer_unary(expr, *op, inner),
            ExprKind::Binary { op, lhs, rhs } => self.infer_binary(*op, lhs, rhs, expr.span),
            ExprKind::Compare { operands, ops } => self.infer_compare_chain(operands, ops),
            // TICKET-253: a range is a VALUE of the prelude struct `Range`; its bounds are `int`s.
            ExprKind::Range { start, end, .. } => {
                self.with_child_hint(child_hint(&expr.kind), |c| {
                    for bound in [start, end].into_iter().flatten() {
                        c.expect_int(bound, "range bound");
                    }
                });
                self.range_ty()
            }
            ExprKind::Call {
                callee,
                args,
                named,
                type_args,
                bracket,
            } => self.infer_call(
                callee,
                args,
                named,
                type_args,
                bracket.as_deref(),
                expr.span,
                expr.id,
            ),
            ExprKind::Field {
                obj,
                name,
                name_span,
            } => self.infer_field(expr, obj, name, *name_span),
            ExprKind::Index { obj, index, types } => {
                self.infer_index(expr, obj, index.as_deref(), types)
            }
            ExprKind::Try(inner) => self.infer_try(inner, expr.span, child_hint(&expr.kind)),
            // W7-43 — optional-chaining `?.` / null-coalescing `??` are CARRIER nodes: the checker
            // types the operand, picks the lowering, then clone-lowers and infers the clone. The
            // choice needs the operand's TYPE, which is why desugar no longer lowers them; the
            // picked mode is recorded in the `CarrierTable` so the type-blind compiler agrees.
            ExprKind::OptChain { obj, name_span, .. } => {
                self.infer_opt_chain(expr, obj, *name_span, expr.span)
            }
            ExprKind::NullCoalesce { lhs, op_span, .. } => {
                self.infer_null_coalesce(expr, lhs, *op_span)
            }
            ExprKind::Closure { params, ret, body } => {
                // The closure reads its expected `fn` type from the hint like a list literal
                // does, through one carrier layer; `meet_slot` wraps it. With no `fn` slot the
                // free-closure inference (sources #2/#3) and the ambiguity check happen inside
                // `infer_closure`.
                let hint = self.expected_hint.take().map(|h| h.ty);
                let exp = hint
                    .as_ref()
                    .map(|h| h.slot_payload())
                    .filter(|h| matches!(h, Ty::Func { .. }));
                self.infer_closure(params, ret.as_ref(), body, exp)
            }
            ExprKind::Match { scrutinee, arms } => self.infer_match(scrutinee, arms, owned),
            ExprKind::IfElse { cond, then, els } => self.infer_if_else(cond, then, els, owned),
            ExprKind::Recover(block, form) => {
                self.infer_recover(block, *form, child_hint(&expr.kind))
            }
            ExprKind::ElseGuard { value, err, body } => {
                self.infer_else_guard(expr, value, err, body)
            }
        }
    }

    /// Record `(ty, kind)` as the hover result if `span` is the armed probe position (entry module,
    /// first hit wins). The single place a probe hit is committed; both the expr-leaf path and the
    /// let-binding path funnel through here. A no-op when no probe is armed.
    pub(super) fn hover_record_at(
        &mut self,
        span: Span,
        ty: &Ty,
        kind: HoverKind,
        doc: Option<String>,
    ) {
        let Some((pl, pc)) = self.hover_probe else {
            return;
        };
        if self.hover_result.is_some() || self.current_module_id != self.hover_entry {
            return;
        }
        if span.line == pl && span.col == pc {
            self.hover_result = Some((ty.clone(), kind, doc));
        }
    }

    /// Editor hover for a `from M import T` user type (struct/enum). Computes the effective
    /// doc — the type's own decl docstring carried across the module boundary, else a `kind (from
    /// module)` fallback — then (1) seeds `name_docs[bind]` so a later bare (`x: T`) / generic-head
    /// (`x: T[..]`) annotation use surfaces the same doc (those arms read `name_docs`), and (2) records
    /// the import-line token hover at `name_span`. Both halves are probe-gated no-ops off the hover
    /// probe (`name_docs` is editor-tooling-only and entry-module-scoped), so this is behavior-neutral.
    pub(super) fn record_imported_type_hover(
        &mut self,
        bind: &str,
        name_span: Span,
        ty: &Ty,
        own_doc: Option<&str>,
        kind_word: &str,
        path: &[String],
    ) {
        if self.hover_probe.is_none() {
            return;
        }
        let doc = own_doc
            .map(str::to_string)
            .unwrap_or_else(|| format!("{kind_word} (from {})", path.join(".")));
        self.name_docs.insert(bind.to_string(), doc.clone());
        self.hover_record_at(name_span, ty, HoverKind::Type, Some(doc));
    }

    /// Editor hover for a per-name import of a native/reserved TYPE (`import Shared from
    /// std.concurrency`, `import Socket from std.net`, `import ptr from std.ffi`, …). These branches
    /// license the name via the per-module sets and short-circuit BEFORE the user-struct import arm
    /// that records a hover, so the import-line token would otherwise show nothing. Records that
    /// token hover with the type's `builtin_type_doc` blurb (else a `(from <module>)` fallback) and
    /// its resolved native `Ty` for display. Probe-gated no-op off the hover probe; unlike a user
    /// `.chz` type NO `name_docs` seeding is needed — the bare/annotation use already resolves its
    /// doc through `builtin_type_doc` in the `Type::Named`/`Type::Generic` hover arms.
    pub(super) fn record_native_type_import_hover(
        &mut self,
        member: &str,
        name_span: Span,
        path: &[String],
    ) {
        if self.hover_probe.is_none() {
            return;
        }
        let ty = self
            .qualified_builtin_ty(member, &[])
            .unwrap_or(Ty::Unknown);
        let doc = builtin_type_doc(member)
            .unwrap_or_else(|| format!("{member} (from {})", path.join(".")));
        self.hover_record_at(name_span, &ty, HoverKind::Type, Some(doc));
    }

    /// The DISPLAY-only signature for a reserved callable builtin, for editor hover + value-position
    /// typing. The eight MIGRATED universe builtins (`ord`/`chr`/`panic`/`int`/`float`/`str`/`bytes`/
    /// `bytearray`) source their sig from `std/prelude.chz` via [`Checker::native_prelude_sigs`]; the
    /// still-synthetic `print` + container/runtime ctors fall through to [`builtin_container_sig`].
    /// Covers exactly the builtin callables (`builtin_callables`, drift-guarded). Not used for direct-call typing (the
    /// `infer_named_call` arms handle that) — only hover + the first-class value form.
    pub(super) fn builtin_sig(&self, name: &str) -> Option<FnSig> {
        if let Some(sig) = self.native_prelude_sigs.get(name) {
            return Some(sig.clone());
        }
        builtin_container_sig(name)
    }

    /// Hover-record a LEAF expression (identifier / literal) or a field-name access. Non-leaf kinds
    /// (Binary/Index/Call/…) are skipped so hovering `a` in `a[0]` reports `a`'s type, not the element
    /// type — the parent never overwrites the child. The field-name access anchors on `name_span` (the
    /// field-name token), not the receiver-start `expr.span`, so `a.b.c` resolves the hovered segment.
    pub(super) fn hover_record_expr(&mut self, expr: &Expr, ty: &Ty) {
        match &expr.kind {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            // An interpolated literal hovers as one `str` literal, like its un-desugared `Str` form
            // (its fragments record their own hovers when inferred).
            | ExprKind::Interp(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bytes(_)
            | ExprKind::Bool(_) | ExprKind::NoneLit => self.hover_record_at(expr.span, ty, HoverKind::Literal, None),
            ExprKind::Ident(name) => {
                // doc source mirrors the resolution: a `let`-bound local/global → `name_docs`; a free
                // fn → its `FnSig::doc`; a bare type/ctor name used as a value → `name_docs`. All keyed
                // by simple name, entry-module-scoped (safe — hover only fires in the entry module).
                if self.lookup(name).is_some() {
                    // Only a TRUE module-top-level binding (resolves at scope 0) owns its `name_docs`
                    // entry; a shadowing param/local of the same name has no doc of its own and must
                    // NOT borrow the global's (`name_docs` is keyed by bare name).
                    let at_top_level = self.owning_scope(name) == Some(0);
                    let doc = if at_top_level {
                        self.name_docs.get(name).cloned()
                    } else {
                        None
                    };
                    self.hover_record_at(expr.span, ty, HoverKind::Local, doc);
                } else if let Some(sig) = self.functions.get(name) {
                    self.hover_record_at(expr.span, ty, HoverKind::Func, sig.doc.clone());
                } else {
                    let doc = self.name_docs.get(name).cloned();
                    self.hover_record_at(expr.span, ty, HoverKind::Other, doc);
                }
            }
            ExprKind::Field { name_span, .. } => {
                self.hover_record_at(*name_span, ty, HoverKind::Field, None);
            }
            ExprKind::Pass | ExprKind::List(..) | ExprKind::Tuple(_) | ExprKind::Map(_) | ExprKind::Set(_) | ExprKind::Comprehension { .. } | ExprKind::Unary { .. } | ExprKind::Binary { .. } | ExprKind::Compare { .. } | ExprKind::Range { .. } | ExprKind::Call { .. } | ExprKind::Index { .. } | ExprKind::Try(_) | ExprKind::OptChain { .. } | ExprKind::NullCoalesce { .. } | ExprKind::Closure { .. } | ExprKind::Match { .. } | ExprKind::IfElse { .. } | ExprKind::Recover(..) | ExprKind::ElseGuard { .. } => {}
        }
    }

    /// Build a DISPLAY-only `Ty::Func` for a by-name call callee (free fn, struct constructor, or a
    /// reserved builtin via [`builtin_sig`]), for editor hover. `None` only for bare enum variants —
    /// they carry no recordable signature, so hover stays `None`. Pure read of the fn/struct/builtin
    /// tables: emits no error and changes no checking decision (it is only ever called under the hover
    /// probe). The free-fn branch displays a generic fn's declared signature verbatim (`FnSig`
    /// params/ret stay `Ty::Param(T)` → "fn(T, T) -> T"); the struct branch mirrors `name_is_generic`'s
    /// module-keyed `bare_key` lookup and renders fields → `Struct` ("fn(int, int) -> Vec2"); the
    /// builtin branch returns a canonical display sig ("fn(int) -> List[int]" for `range`).
    pub(super) fn callee_display_ty(&self, name: &str) -> Option<Ty> {
        if let Some(sig) = self.functions.get(name) {
            return Some(crate::checker::fn_value_ty(sig));
        }
        // A RESERVED builtin container/handle (`List`/`Map`/`Set`/`Channel`/`Shared`/…) now ALSO has a
        // `self.structs` entry — for its harvested METHOD table — but it is NOT a nominal struct: its
        // ctor callee must display the FLAT `builtin_container_sig` shape (`fn(?) -> List[?]`), NOT a
        // struct-ctor sig synthesized from the (empty) field list. Skip the struct branch for these so
        // they fall through to `builtin_sig` below (mirrors `resolve_type`'s reserved-type guard).
        if builtin_container_sig(name).is_none()
            && let Some(info) = self.structs.get(&self.bare_key(name))
        {
            let params: Vec<Ty> = info.fields.iter().map(|(_, t)| t.clone()).collect();
            let targs: Vec<Ty> = info
                .type_params
                .iter()
                .map(|tp| Ty::Param(tp.name.clone()))
                .collect();
            return Some(Ty::Func {
                params,
                ret: Box::new(Ty::Struct(name.to_string(), targs)),
                labels: FnLabels::default(),
                variadic: crate::ast::Variadic::No,
            });
        }
        // A free / constructor builtin (`print`/`range`/`List`/`Channel`/…): a DISPLAY-only signature
        // from `builtin_sig` (the inference arms aren't a single queryable sig). Reserved names can't
        // be user-shadowed, so this never collides with the fn/struct tables above.
        if let Some(sig) = self.builtin_sig(name) {
            return Some(crate::checker::fn_value_ty(&sig));
        }
        None
    }

    /// `recover: <block>` yields `Result[T, Error]` where `T` is the type of the block's trailing
    /// expression (or `nil`). Non-final statements are checked for their effects.
    /// `value else e: <block>` (design D4): the payload of a present / successful `value`. On
    /// failure the block runs with the error bound to `err`, and must leave.
    fn infer_else_guard(
        &mut self,
        e: &Expr,
        value: &Expr,
        err: &Option<(String, Span)>,
        body: &Block,
    ) -> Ty {
        let m = self.tyvar_mark();
        let ty = self.with_child_hint(child_hint(&e.kind), |c| c.infer(value));
        let ty = self.close_operand(m, ty);
        let (payload, err_ty) = match &ty {
            Ty::Option(t) => ((**t).clone(), None),
            Ty::Result(t, er) => ((**t).clone(), Some((**er).clone())),
            // Already reported: stay silent.
            Ty::Unknown => (Ty::Unknown, Some(Ty::Unknown)),
            other @ Ty::Int
            | other @ Ty::Float
            | other @ Ty::Bool
            | other @ Ty::Str
            | other @ Ty::Bytes
            | other @ Ty::ByteArray
            | other @ Ty::Nil
            | other @ Ty::List(_)
            | other @ Ty::Map(..)
            | other @ Ty::Set(_)
            | other @ Ty::Func { .. }
            | other @ Ty::Tuple(_)
            | other @ Ty::Struct(..)
            | other @ Ty::Enum(..)
            | other @ Ty::Param(_)
            | other @ Ty::Channel(_)
            | other @ Ty::Shared(_)
            | other @ Ty::Atomic(_)
            | other @ Ty::AtomicInt
            | other @ Ty::RwShared(_)
            | other @ Ty::Executor
            | other @ Ty::Socket
            | other @ Ty::Listener
            | other @ Ty::Writer
            | other @ Ty::Reader
            | other @ Ty::Ptr
            | other @ Ty::Width(_)
            | other @ Ty::Protocol(..)
            | other @ Ty::Module(_)
            | other @ Ty::Var(_) => {
                self.error(
                    e.span,
                    format!("`else` needs a `T?` or `T!E` value, found {other}"),
                );
                (Ty::Unknown, Some(Ty::Unknown))
            }
        };
        if let Ok((key, variant)) = Self::carrier_variant(CarrierTag::Present, &ty) {
            self.record_variant(e.id, key, variant, e.span);
        }
        self.push_scope();
        if let Some((name, name_span)) = err {
            let bound = err_ty.unwrap_or_else(|| {
                self.error(
                    *name_span,
                    format!(
                        "`else {name}:` needs an error to bind, and {ty} has none; write `else:`"
                    ),
                );
                Ty::Unknown
            });
            self.hover_record_at(*name_span, &bound, HoverKind::Local, None);
            self.declare(name, bound);
        }
        // The guard's block is a statement child: it sees no hint.
        self.with_child_hint(ChildHint::None, |c| {
            for stmt in body {
                c.check_stmt(stmt);
            }
        });
        self.pop_scope();
        // Read after the body is checked: `call_diverges` knows a `panic` call only
        // once that call has been inferred.
        if self.block_flow(body).falls_through {
            self.error(
                e.span,
                "else block must leave (return, break, continue, panic)".to_string(),
            );
        }
        payload
    }

    /// `recover:` types as `Result[T]`. The inline form `recover: expr` carries the expression's type.
    /// The block form has no tail value (U8): every statement is an ordinary statement and the block
    /// is `!` (`Result[nil]`), the error or nothing.
    /// `rule` is the node's [`child_hint`]: the inline expression's; a block's statements see none.
    pub(super) fn infer_recover(
        &mut self,
        block: &Block,
        form: RecoverForm,
        rule: ChildHint,
    ) -> Ty {
        // `boundary::RULES` says what may cross the block: it is the target of a `?` on a `T!E`.
        self.push_scope();
        let value_ty = self.in_block(boundary::Block::Recover, |c| match form {
            RecoverForm::Block => {
                c.with_child_hint(ChildHint::None, |c| {
                    for stmt in block {
                        c.check_stmt(stmt);
                    }
                });
                Ty::Nil
            }
            RecoverForm::Inline => {
                let [stmt] = block.as_slice() else {
                    unreachable!("an inline recover holds one expression statement")
                };
                let StmtKind::Expr(e) = &stmt.kind else {
                    unreachable!("an inline recover holds one expression statement")
                };
                let ty = c.with_child_hint(rule, |c| c.infer(e));
                // A body that provably diverges (`recover: panic(...)`) yields no normal value, so its
                // `Ok` payload is bottom (`Unknown`), not `nil`. Reuses the sound, conservative
                // `flow::stmt` summary (divergence read from the resolved callee).
                if ty == Ty::Nil && !flow::stmt(stmt, &|e| c.call_diverges(e)).falls_through {
                    Ty::Unknown
                } else {
                    ty
                }
            }
        });
        self.pop_scope();
        Ty::result(value_ty)
    }
}

/// Map a type to the [`crate::fmtspec::ScalarKind`] it renders as for a static format-spec check —
/// but ONLY for CONCRETE scalars. `bool` folds into `Str` (it renders via the runtime `FmtArg::Other`
/// → `render_str` path). Everything else returns `None`: a type that renders as its text form is
/// classified by [`renders_as_text`] (and checked against `ScalarKind::Str` by
/// `Checker::format_spec_kind`), and `Unknown`/`Param(T)`/protocols keep the runtime backstop — the
/// soundness boundary that lets a generic body `"{v:.2f}"` (v: T could be float) pass check.
fn scalar_kind_of(ty: &Ty) -> Option<crate::fmtspec::ScalarKind> {
    use crate::fmtspec::ScalarKind;
    match ty {
        Ty::Int => Some(ScalarKind::Int),
        Ty::Float => Some(ScalarKind::Float),
        Ty::Str | Ty::Bool => Some(ScalarKind::Str),
        Ty::Bytes
        | Ty::ByteArray
        | Ty::Nil
        | Ty::List(_)
        | Ty::Map(..)
        | Ty::Set(_)
        | Ty::Func { .. }
        | Ty::Tuple(_)
        | Ty::Struct(..)
        | Ty::Enum(..)
        | Ty::Param(_)
        | Ty::Result(..)
        | Ty::Option(_)
        | Ty::Channel(_)
        | Ty::Shared(_)
        | Ty::Atomic(_)
        | Ty::AtomicInt
        | Ty::RwShared(_)
        | Ty::Executor
        | Ty::Socket
        | Ty::Listener
        | Ty::Writer
        | Ty::Reader
        | Ty::Ptr
        | Ty::Width(_)
        | Ty::Protocol(..)
        | Ty::Module(_)
        | Ty::Unknown
        | Ty::Var(_) => None,
    }
}

/// TICKET-142 (W14-19): does a value of this CONCRETE type render through the runtime's text form
/// (`render_str`) when a format spec is present? Measured for every native struct (`FileInfo`,
/// `AtomicInt`, `Atomic`, `Executor`, `RwShared`, `Channel`, `Writer`, `ptr`): none renders as a
/// scalar. An exhaustive `match` with no `_` arm, so a new `Ty` variant does not compile until it is
/// classified — a type whose runtime value IS a scalar must return `false` here.
fn renders_as_text(ty: &Ty) -> bool {
    match ty {
        Ty::Bytes
        | Ty::ByteArray
        | Ty::List(_)
        | Ty::Map(..)
        | Ty::Set(_)
        | Ty::Tuple(_)
        | Ty::Option(_)
        | Ty::Result(..)
        | Ty::Func { .. }
        | Ty::Struct(..)
        | Ty::Enum(..)
        | Ty::Channel(_)
        | Ty::Shared(_)
        | Ty::Atomic(_)
        | Ty::AtomicInt
        | Ty::RwShared(_)
        | Ty::Executor
        | Ty::Socket
        | Ty::Listener
        | Ty::Writer
        | Ty::Reader
        | Ty::Ptr => true,
        Ty::Int
        | Ty::Width(_)
        | Ty::Float
        | Ty::Bool
        | Ty::Str
        | Ty::Nil
        | Ty::Param(_)
        | Ty::Protocol(..)
        | Ty::Module(_)
        | Ty::Var(_)
        | Ty::Unknown => false,
    }
}
