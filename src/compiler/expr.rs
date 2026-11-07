//! Compiler: expressions: `compile_expr`, else guards, `recover`, `if`/`match` expressions,
//! identifiers, defer.
//! Split out of `src/compiler/mod.rs`. `super::*` is the parent module.

use super::*;
use crate::ast::{
    BinaryOp, DeferTarget, Expr, ExprKind, MatchExprArm, Span, Stmt, StmtKind, UnaryOp,
};
use crate::base::op::Op;
use crate::checker::Resolution;

impl Compiler {
    /// Compile one value-producing expression, then apply the checker's implicit wrap for its node
    /// (TICKET-227). The ONLY code that applies a [`crate::checker::WrapTable`] entry.
    pub(super) fn compile_expr(
        &mut self,
        fc: &mut FnComp,
        expr: &Expr,
    ) -> Result<(), CompileError> {
        self.compile_expr_node(fc, expr)?;
        if expr.id.0 != crate::ast::NodeId::SYNTH.0 {
            self.emit_wrap_at(fc, expr.id, expr.span)?;
        }
        Ok(())
    }

    /// Wrap the stack top in `Some`/`Ok` when the checker recorded a wrap for node `id`.
    fn emit_wrap_at(
        &mut self,
        fc: &mut FnComp,
        id: crate::ast::NodeId,
        span: Span,
    ) -> Result<(), CompileError> {
        let (enum_key, variant) = match self.wraps.get(&(self.current_module_idx, id.0)) {
            None => return Ok(()),
            Some(crate::checker::Wrap::Some) => ("Option", "Some"),
            Some(crate::checker::Wrap::Ok) => ("Result", "Ok"),
        };
        self.emit_carrier_wrap(fc, enum_key, variant, span)
    }

    /// The dispatch over `ExprKind`; call [`Self::compile_expr`] instead, except to re-enter a
    /// node whose wrap `compile_expr` already applies (a `??`/`?.` lowering keeps its NodeId).
    fn compile_expr_node(&mut self, fc: &mut FnComp, expr: &Expr) -> Result<(), CompileError> {
        match &expr.kind {
            ExprKind::Int(n) => fc.emit(Op::ConstInt(*n), expr.span),
            ExprKind::Float(x) => fc.emit(Op::ConstFloat(*x), expr.span),
            ExprKind::Bool(b) => fc.emit(if *b { Op::True } else { Op::False }, expr.span),
            ExprKind::Pass => fc.emit(Op::Nil, expr.span),
            ExprKind::NoneLit => {
                if let Resolution::Variant { enum_key, variant } = self.resolution(expr)?.clone() {
                    self.emit_new_enum(fc, &enum_key, &variant, 0, expr.span);
                }
            }
            // Final text: the lexer resolved the escapes and folded `{{` / `}}`.
            ExprKind::Str(s) => fc.emit(Op::ConstStr(s.clone()), expr.span),
            // Fragments are parsed, already-normalized children.
            ExprKind::Interp(chunks) => self.compile_interp(fc, chunks, expr.span)?,
            // Raw string: emit the literal directly, so braces stay literal and backslashes are
            // verbatim.
            ExprKind::RawStr(s) => fc.emit(Op::ConstStr(s.clone()), expr.span),
            ExprKind::Bytes(b) => fc.emit(Op::ConstBytes(b.clone().into_boxed_slice()), expr.span),
            ExprKind::Ident(_) => self.compile_ident(fc, expr)?,
            ExprKind::List(items, _) => {
                for it in items {
                    self.compile_expr(fc, it)?;
                }
                fc.emit(Op::NewList(items.len()), expr.span);
            }
            ExprKind::Tuple(items) => {
                for it in items {
                    self.compile_expr(fc, it)?;
                }
                fc.emit(Op::NewTuple(items.len()), expr.span);
            }
            ExprKind::Map(entries) => {
                // Push `[k0, v0, k1, v1, …]`, then build the map (last duplicate key wins at runtime).
                for (k, v) in entries {
                    self.compile_expr(fc, k)?;
                    self.compile_expr(fc, v)?;
                }
                fc.emit(Op::NewMap(entries.len()), expr.span);
            }
            ExprKind::Set(elems) => {
                // No widening: a `float` is not Hashable, so a set element is never float.
                for e in elems {
                    self.compile_expr(fc, e)?;
                }
                fc.emit(Op::NewSet(elems.len()), expr.span);
            }
            ExprKind::Comprehension {
                kind,
                key,
                elem,
                clauses,
            } => self.compile_comprehension(fc, *kind, key.as_deref(), elem, clauses, expr.span)?,
            ExprKind::Unary { op, expr: inner } => {
                self.compile_expr(fc, inner)?;
                match op {
                    UnaryOp::Neg => fc.emit(Op::Neg, expr.span),
                    UnaryOp::Not => {
                        fc.emit(Op::AsBool, inner.span);
                        fc.emit(Op::Not, expr.span);
                    }
                    // `!e` is `Err(e)`; `?x` compiles its operand, and the checker's wrap
                    // record on this node (applied by `compile_expr`) builds the carrier.
                    UnaryOp::ErrVal => self.emit_carrier_wrap(fc, "Result", "Err", expr.span)?,
                    UnaryOp::Wrap => {}
                }
            }
            ExprKind::Binary {
                op: op @ (BinaryOp::And | BinaryOp::Or),
                lhs,
                rhs,
            } => {
                // Short-circuit: lhs is always bool-checked; rhs only when needed; result is bool.
                self.compile_expr(fc, lhs)?;
                fc.emit(Op::AsBool, lhs.span);
                let jump = match op {
                    BinaryOp::And => fc.emit_jump(Op::JumpIfFalseKeep(0), expr.span),
                    BinaryOp::Add
                    | BinaryOp::Sub
                    | BinaryOp::Mul
                    | BinaryOp::Div
                    | BinaryOp::Mod
                    | BinaryOp::Lt
                    | BinaryOp::LtEq
                    | BinaryOp::Gt
                    | BinaryOp::GtEq
                    | BinaryOp::Eq
                    | BinaryOp::NotEq
                    | BinaryOp::Or
                    | BinaryOp::BitAnd
                    | BinaryOp::BitOr
                    | BinaryOp::BitXor
                    | BinaryOp::Shl
                    | BinaryOp::Shr
                    | BinaryOp::In => fc.emit_jump(Op::JumpIfTrueKeep(0), expr.span),
                };
                fc.emit(Op::Pop, expr.span);
                self.compile_expr(fc, rhs)?;
                fc.emit(Op::AsBool, rhs.span);
                fc.patch_jump(jump);
            }
            ExprKind::Binary { op, lhs, rhs } => {
                self.compile_expr(fc, lhs)?;
                self.compile_expr(fc, rhs)?;
                fc.emit(binary_op(*op), expr.span);
            }
            // TICKET-077: a Python-style chained comparison (`a < b <= c`) lowers like `and`, one
            // link per adjacent pair, but the middle operand must be evaluated exactly once — so
            // each interior operand is stashed in a hidden temp and read back for the next link,
            // instead of being recompiled.
            ExprKind::Compare { operands, ops } => {
                self.compile_expr(fc, &operands[0])?;
                let mut end_jumps = Vec::new();
                for (i, op) in ops.iter().enumerate() {
                    self.compile_expr(fc, &operands[i + 1])?;
                    let interior_slot = if i + 1 < ops.len() {
                        let slot = fc.add_hidden();
                        fc.emit_hidden_set(slot, operands[i + 1].span);
                        fc.emit_hidden_get(slot, operands[i + 1].span);
                        Some(slot)
                    } else {
                        None
                    };
                    fc.emit(binary_op(*op), expr.span);
                    if let Some(slot) = interior_slot {
                        end_jumps.push(fc.emit_jump(Op::JumpIfFalseKeep(0), expr.span));
                        fc.emit(Op::Pop, expr.span);
                        fc.emit_hidden_get(slot, operands[i + 1].span);
                    }
                }
                for jump in end_jumps {
                    fc.patch_jump(jump);
                }
            }
            // TICKET-253: a range literal builds the prelude struct `Range(start, end, 1, inclusive)`;
            // an omitted bound is `None`, a present one `Some(bound)`. Module 0 is the prelude.
            ExprKind::Range {
                start,
                end,
                inclusive,
            } => {
                for bound in [start, end] {
                    match bound {
                        Some(b) => {
                            self.compile_expr(fc, b)?;
                            self.emit_new_enum(fc, "Option", "Some", 1, b.span);
                        }
                        None => self.emit_new_enum(fc, "Option", "None", 0, expr.span),
                    }
                }
                fc.emit(Op::ConstInt(1), expr.span);
                fc.emit(if *inclusive { Op::True } else { Op::False }, expr.span);
                self.emit_new_struct(
                    fc,
                    self.type_key(0, crate::resolver::RANGE_STRUCT),
                    4,
                    expr.span,
                )?;
            }
            // `type_args` are type-erased — the compiler never sees them (checker already used them).
            ExprKind::Call {
                callee,
                args,
                named,
                bracket,
                ..
            } => {
                let bracket = bracket.as_deref();
                self.compile_call(fc, callee, args, named, bracket, expr.span, expr.id)?;
            }
            ExprKind::Field {
                obj,
                name,
                name_span,
            } => {
                // A tuple slot (`t.0`) is a value, never a name; everything else reads the
                // checker's record on the `Field`.
                let res = if crate::ast::is_tuple_index(name) {
                    Resolution::Member
                } else {
                    self.resolution(expr)?.clone()
                };
                match res {
                    Resolution::Variant { enum_key, variant } => {
                        self.emit_new_enum(fc, &enum_key, &variant, 0, expr.span);
                    }
                    // A type path's method read as a value (`Bx[int].make`, `Pt.getx`): its proto
                    // resolves at run time, as `CallStatic`'s does. The type head is never lowered.
                    // A `from` overload: the checker's pick for this read, as `CallStatic`'s.
                    Resolution::MethodFn { type_key, method } => {
                        let method = self.overload_pick(*name_span).unwrap_or(method);
                        fc.emit(Op::MakeMethodFunc { type_key, method }, expr.span);
                    }
                    Resolution::NativeStatic(which) => {
                        let p = self.synth_fn_proto(SynthFn::NativeStatic(which), |c| {
                            Ok(c.build_native_static_fn(which))
                        })?;
                        fc.emit(Op::MakeFunc(p), expr.span);
                    }
                    // A bound's instance method through a type parameter (`T.get`).
                    Resolution::ParamMethodFn { method, arity } => {
                        let p = self.synth_fn_proto(
                            SynthFn::ParamMethod {
                                method: method.clone(),
                                arity,
                            },
                            |c| Ok(c.build_param_method_fn(&method, arity)),
                        )?;
                        fc.emit(Op::MakeFunc(p), expr.span);
                    }
                    // std.json's decode read as a value (TICKET-214).
                    Resolution::Decode(desc) => {
                        self.compile_decode_fn_value(fc, obj, &desc, expr.span)?
                    }
                    // `Fn`: the head of a value turbofish `m.f[int]`, a member of the module.
                    Resolution::Member
                    | Resolution::ModuleMember { .. }
                    | Resolution::Fn { .. } => {
                        self.compile_expr(fc, obj)?;
                        let ic = self.next_field_ic(name);
                        fc.emit(
                            Op::GetField {
                                name: name.clone(),
                                ic,
                            },
                            expr.span,
                        );
                    }
                    other @ Resolution::Local
                    | other @ Resolution::Global { .. }
                    | other @ Resolution::Builtin(_)
                    | other @ Resolution::StructCtor(_)
                    | other @ Resolution::Provider
                    | other @ Resolution::IndexCall
                    | other @ Resolution::WitnessStatic(_)
                    | other @ Resolution::Module(_)
                    | other @ Resolution::PatBinding
                    | other @ Resolution::PatStruct(_)
                    // U10: the checker rejects a payload variant read as a value.
                    | other @ Resolution::VariantFn { .. } => {
                        return Err(CompileError {
                            message: format!("internal: a value field resolved to {other:?}"),
                            span: expr.span,
                        });
                    }
                }
            }
            // One bracket, two readings (TICKET-222): a type-applied fn value (`pair[str, int]`,
            // `idt[(int, str)]`) when the checker resolved the head to a fn-like item; as the
            // receiver of a member access / call it resolved into a variant ctor, static-method
            // call or nullary variant, which never lowers the head. Otherwise the index reading.
            ExprKind::Index { obj, index, .. } => {
                if self.compile_type_applied_fn_value(fc, expr)? {
                    return Ok(());
                }
                let Some(index) = index else {
                    return Err(CompileError {
                        message: "internal: a type-only bracket reached the index lowering"
                            .to_string(),
                        span: expr.span,
                    });
                };
                self.compile_expr(fc, obj)?;
                self.compile_expr(fc, index)?;
                // TICKET-253: the checker decided once whether this subscript reads as a slice (a
                // `Range` index on a sliceable receiver); the opcode follows its record.
                let slice_key = crate::checker::carrier_key(
                    self.current_module_idx,
                    self.kw_frag_ctx,
                    self.kw_frag_ord,
                    index.span,
                );
                if self.hook_calls.get(&slice_key) == Some(&crate::base::hooks::Hook::Slice) {
                    fc.emit(Op::SliceRange, expr.span);
                } else {
                    // No `AsInt`: the index may be a map key (str/bool), not just a list/str int.
                    // `GetIndex` validates int-ness in its list/str arm at runtime.
                    fc.emit(Op::GetIndex, expr.span);
                }
            }
            ExprKind::Try(inner) => {
                self.compile_expr(fc, inner)?;
                fc.emit(Op::Try, expr.span);
            }
            // W7-43 — `?.`/`??` carriers reach the backend intact; clone-and-lower here, then
            // recurse, so the synthesized nodes route through the ordinary `Field`/`compile_call`
            // arms and pick up method inline caches, keyword permutations and witness args exactly
            // as the equivalent hand-written spelling does. `lower_carrier_*` are the SAME
            // functions the checker lowered with, on the same input, so spans (and therefore every
            // table key derived from them) cannot drift.
            ExprKind::NullCoalesce { op_span, .. } => {
                let key = crate::checker::carrier_key(
                    self.current_module_idx,
                    self.kw_frag_ctx,
                    self.kw_frag_ord,
                    *op_span,
                );
                let mut c = expr.clone();
                // Same discipline as the `?.` arm below: a missing entry is a hard error, never a
                // default. Silently picking the `Option` lowering for a `Result` operand would emit
                // `Some`/`None` arms against an `Ok`/`Err` value — a wrong runtime VALUE SHAPE under
                // a green `chezzi check`.
                match self.carriers.get(&key) {
                    Some(crate::checker::CarrierMode::ResultCoalesce) => {
                        crate::desugar::lower_carrier_result_coalesce(&mut c, self.next_opt_tmp())
                    }
                    Some(
                        crate::checker::CarrierMode::Option | crate::checker::CarrierMode::Unknown,
                    ) => crate::desugar::lower_carrier_option(&mut c, self.next_opt_tmp()),
                    Some(
                        crate::checker::CarrierMode::Try | crate::checker::CarrierMode::OptionVoid,
                    ) => {
                        return Err(CompileError {
                            message: "internal: a '?.' lowering was recorded for a '??' — the \
                                      type-checker and the backend disagree about this expression"
                                .to_string(),
                            span: expr.span,
                        });
                    }
                    None => {
                        return Err(CompileError {
                            message: "internal: no lowering recorded for this '??' — the \
                                      type-checker and the backend disagree about this expression"
                                .to_string(),
                            span: expr.span,
                        });
                    }
                }
                self.compile_expr_node(fc, &c)?;
            }
            ExprKind::OptChain { name_span, .. } => {
                let key = crate::checker::carrier_key(
                    self.current_module_idx,
                    self.kw_frag_ctx,
                    self.kw_frag_ord,
                    *name_span,
                );
                let mut c = expr.clone();
                // A MISSING entry is a hard error, never a default: it means the checker's
                // traversal and this one disagree about the program, and silently picking the
                // Option lowering would emit an `Option[T]` where the checker promised `T` — a
                // wrong runtime VALUE SHAPE under a green `chezzi check`. Same discipline as the
                // missing-witness guard in `witness_srcs`.
                match self.carriers.get(&key) {
                    Some(crate::checker::CarrierMode::Try) => {
                        crate::desugar::lower_carrier_try(&mut c)
                    }
                    Some(crate::checker::CarrierMode::OptionVoid) => {
                        crate::desugar::lower_carrier_option_void(&mut c, self.next_opt_tmp())
                    }
                    Some(
                        crate::checker::CarrierMode::Option | crate::checker::CarrierMode::Unknown,
                    ) => crate::desugar::lower_carrier_option(&mut c, self.next_opt_tmp()),
                    Some(crate::checker::CarrierMode::ResultCoalesce) => {
                        return Err(CompileError {
                            message: "internal: a '??' lowering was recorded for a '?.' — the \
                                      type-checker and the backend disagree about this expression"
                                .to_string(),
                            span: expr.span,
                        });
                    }
                    None => {
                        return Err(CompileError {
                            message: "internal: no lowering recorded for this '?.' — the \
                                      type-checker and the backend disagree about this expression"
                                .to_string(),
                            span: expr.span,
                        });
                    }
                }
                self.compile_expr_node(fc, &c)?;
            }
            ExprKind::Closure { params, body, .. } => {
                self.compile_closure(fc, params, body, expr.span)?
            }
            ExprKind::Match { scrutinee, arms } => {
                self.compile_match_expr(fc, scrutinee, arms, expr.span)?
            }
            ExprKind::IfElse { cond, then, els } => self.compile_if_expr(fc, cond, then, els)?,
            ExprKind::Recover(block, form) => self.compile_recover(fc, block, *form, expr.span)?,
            ExprKind::ElseGuard { value, err, body } => {
                self.compile_else_guard(fc, expr, value, err, body)?
            }
        }
        Ok(())
    }

    /// `recover: <block>` — install a handler over the block; on the happy path wrap the block's
    /// trailing-expression value in `Ok`, on a caught fault the VM has pushed the message `str` and
    /// we wrap it in `Err`. Both paths leave exactly one `Result` value on the stack.
    /// `value else e: <block>`: test `value` for the success variant the checker recorded on the
    /// guard, leaving its payload; otherwise bind the error and run the block. The checker proved
    /// the block leaves, so the code after it is a trap, never a fall-through.
    fn compile_else_guard(
        &mut self,
        fc: &mut FnComp,
        expr: &Expr,
        value: &Expr,
        err: &Option<(String, Span)>,
        body: &[Stmt],
    ) -> Result<(), CompileError> {
        let span = expr.span;
        let Some(Resolution::Variant { enum_key, variant }) = self
            .resolutions
            .get(&(self.current_module_idx, expr.id.0))
            .cloned()
        else {
            return Err(CompileError {
                message: "internal: no carrier variant recorded for this else guard -- the \
                          type-checker and the backend disagree"
                    .to_string(),
                span,
            });
        };
        self.compile_expr(fc, value)?;
        let guarded = fc.add_hidden();
        fc.emit_hidden_set(guarded, span);
        let payload = fc.add_hidden();
        let variant_id = self.variant_id_of_key(&enum_key, &variant);
        let success = fc.emit_jump(
            Op::MatchArm {
                scrut: guarded,
                variant,
                variant_id,
                enum_name: None,
                nbind: 1,
                bind_start: payload,
                next: 0,
            },
            span,
        );
        let to_done = fc.emit_jump(Op::Jump(0), span);
        fc.patch_jump(success);
        fc.begin_scope();
        let mut not_err = None;
        if let Some((name, _)) = err {
            // `MatchArm` writes a raw value; the named binding (a cell when a closure captures
            // it) is declared from that slot.
            let raw = fc.add_hidden();
            not_err = Some(fc.emit_jump(
                Op::MatchArm {
                    scrut: guarded,
                    variant: "Err".to_string(),
                    variant_id: crate::base::op::VID_ERR,
                    enum_name: None,
                    nbind: 1,
                    bind_start: raw,
                    next: 0,
                },
                span,
            ));
            fc.emit_hidden_get(raw, span);
            fc.emit_decl_named(name.clone(), span);
        }
        self.compile_block_flat(fc, body)?;
        fc.end_scope();
        if let Some(j) = not_err {
            fc.patch_jump(j);
        }
        fc.emit(Op::MatchNoArm(guarded), span);
        fc.patch_jump(to_done);
        fc.emit_hidden_get(payload, span);
        Ok(())
    }

    fn compile_recover(
        &mut self,
        fc: &mut FnComp,
        block: &[Stmt],
        form: crate::ast::RecoverForm,
        span: Span,
    ) -> Result<(), CompileError> {
        // The handler target is `done`. Three paths converge there, each leaving one `Result`:
        //   • normal: wrap the value in `Ok`, drop the handler, fall through;
        //   • panic: the VM unwinds, pushes `Err(message)`, and jumps to `done`;
        //   • `?` Err/None in the block: `do_try` pushes the propagated value and jumps to `done`.
        let push = fc.emit_jump(Op::PushHandler(0), span);
        fc.begin_scope();
        match form {
            // The block form has no tail value: every statement is ordinary, then `Ok(nil)`.
            crate::ast::RecoverForm::Block => {
                for stmt in block {
                    self.compile_stmt(fc, stmt)?;
                }
                fc.emit(Op::Nil, span);
            }
            // The inline form is one expression whose value is the `Ok` payload.
            crate::ast::RecoverForm::Inline => {
                let [stmt] = block else {
                    unreachable!("an inline recover holds one expression statement")
                };
                let StmtKind::Expr(e) = &stmt.kind else {
                    unreachable!("an inline recover holds one expression statement")
                };
                self.compile_expr(fc, e)?;
            }
        }
        fc.end_scope();
        // The recover block is a defer scope: drain its own defers at the boundary (Ok path), before
        // wrapping the value. The fault and `?` paths drain via the handler in the VM. Only emit when
        // the block directly holds a `defer`, keeping defer-free recovers byte-identical.
        if block_has_defer(block) {
            fc.emit(Op::DrainHandlerDefers, span);
        }
        fc.emit(
            Op::NewEnum {
                variant: "Ok".to_string(),
                variant_id: crate::base::op::VID_OK,
                argc: 1,
            },
            span,
        );
        fc.emit(Op::PopHandler, span);
        let done = fc.here();
        fc.patch_jump_to(push, done);
        Ok(())
    }

    /// Expression-position `match`: like `compile_match`, but each arm body is compiled as an
    /// expression that leaves its value on the stack, so the whole `match` yields one value.
    fn compile_match_expr(
        &mut self,
        fc: &mut FnComp,
        scrutinee: &Expr,
        arms: &[MatchExprArm],
        span: Span,
    ) -> Result<(), CompileError> {
        // Leaves the arm's value on the stack; `compile_expr` applies the arm's own wrap.
        let widen = |s: &mut Self, fc: &mut FnComp, body: &Expr| -> Result<(), CompileError> {
            s.compile_expr(fc, body)
        };
        if self.arms_are_literal(arms.iter().map(|a| &a.pattern)) {
            return self.compile_match_lit(fc, scrutinee, arms, span, widen);
        }
        self.compile_match_general(fc, scrutinee, arms, span, widen)
    }

    /// Expression-position `if c: a else: b`: condition, then jump to whichever branch; both
    /// branches leave exactly one value, so one value remains at the join.
    fn compile_if_expr(
        &mut self,
        fc: &mut FnComp,
        cond: &Expr,
        then: &Expr,
        els: &Expr,
    ) -> Result<(), CompileError> {
        self.compile_if_expr_chain(fc, cond, then, els)
    }

    /// Body of `compile_if_expr`. Every branch, an `elif` tail included, compiles through
    /// `compile_expr`, the one place a wrap is applied (TICKET-227).
    fn compile_if_expr_chain(
        &mut self,
        fc: &mut FnComp,
        cond: &Expr,
        then: &Expr,
        els: &Expr,
    ) -> Result<(), CompileError> {
        self.compile_expr(fc, cond)?;
        fc.emit(Op::AsBool, cond.span);
        let skip = fc.emit_jump(Op::JumpIfFalse(0), cond.span);
        self.compile_expr(fc, then)?;
        let end = fc.emit_jump(Op::Jump(0), cond.span);
        fc.patch_jump(skip);
        self.compile_expr(fc, els)?;
        fc.patch_jump(end);
        Ok(())
    }

    fn compile_ident(&mut self, fc: &mut FnComp, e: &Expr) -> Result<(), CompileError> {
        let ExprKind::Ident(name) = &e.kind else {
            unreachable!("compile_ident on a non-Ident");
        };
        match self.resolution(e)?.clone() {
            Resolution::Variant { enum_key, variant } => {
                self.emit_new_enum(fc, &enum_key, &variant, 0, e.span);
            }
            Resolution::Builtin(b) => fc.emit(Op::LoadBuiltin(b), e.span),
            Resolution::Local
            | Resolution::Global { .. }
            | Resolution::Fn { .. }
            | Resolution::Module(_) => self.emit_load(fc, name, e.span),
            other @ Resolution::StructCtor(_)
            | other @ Resolution::Provider
            | other @ Resolution::ParamMethodFn { .. }
            | other @ Resolution::IndexCall
            | other @ Resolution::MethodFn { .. }
            | other @ Resolution::NativeStatic(_)
            | other @ Resolution::WitnessStatic(_)
            | other @ Resolution::ModuleMember { .. }
            | other @ Resolution::Member
            | other @ Resolution::PatBinding
            | other @ Resolution::PatStruct(_)
            | other @ Resolution::Decode(_)
            // U10: the checker rejects a payload variant read as a value.
            | other @ Resolution::VariantFn { .. } => {
                return Err(CompileError {
                    message: format!("internal: a value name resolved to {other:?}"),
                    span: e.span,
                });
            }
        }
        Ok(())
    }

    pub(super) fn compile_defer(
        &mut self,
        fc: &mut FnComp,
        target: &DeferTarget,
        span: Span,
    ) -> Result<(), CompileError> {
        let call = match target {
            DeferTarget::Call(call) => call,
            DeferTarget::Block(body) => {
                // `defer:` block → a synthetic zero-arg closure capturing the referenced bindings by
                // reference at the defer point (exactly `compile_spawn`'s Block arm), then
                // defer-invoke it with 0 args. Reuses `MakeClosure` + `DeferCall` — no
                // new op. Free-variable capture (Finding D): only names the block references.
                let snap = fc.snapshot_entries();
                let (kept, free) = filter_entries_free_block(&snap, body, &[]);
                let entries = self.with_witness_captures(&snap, kept, Some(&free));
                let captured_names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
                let mut child = FnComp::new("<deferred block>".to_string(), 0, false);
                // Uniform by-reference capture (Task A): the block's own boxed-name set (unwired).
                child.boxed_names = captured_names_of_body(body, &[]);
                child.captured_names = captured_names;
                // M-C: a `defer:` block is its own function body (it runs as a closure in its own
                // frame at frame exit) — a bare `spawn` inside it binds to the block's own implicit
                // nursery, joined when the deferred block returns. Mirrors `compile_spawn`'s block arm
                // (and the interp's `run_block_task`, which gates deferred blocks the same way).
                let implicit = block_has_bare_spawn(body);
                if implicit {
                    child.has_implicit_nursery = true;
                    child.emit(Op::EnterNursery, span);
                    child.nursery_scopes += 1;
                }
                // M24 Task 4: the block reaches the enclosing frame's witnesses through
                // `with_witness_captures` above.
                self.compile_block_scoped(&mut child, body)?;
                if implicit {
                    child.nursery_scopes -= 1;
                }
                child.emit(Op::Nil, span);
                child.emit(Op::Return, span);
                let pid = self.finish(child);
                fc.emit(Op::MakeClosure(pid, entries), span);
                fc.emit(Op::DeferCall(0), span);
                return Ok(());
            }
        };
        let ExprKind::Call {
            callee,
            args,
            named,
            bracket,
            ..
        } = &call.kind
        else {
            return Err(CompileError {
                message: "defer requires a function or method call".to_string(),
                span,
            });
        };
        // `defer fs[k](1)`: the indexed element is the deferred value call.
        if self.compile_index_call_head(fc, call.id, callee, bracket.as_deref(), call.span)? {
            let argc = self.compile_call_args(fc, call.id, callee, args, named, call.span)?;
            fc.emit(Op::DeferCall(argc), call.span);
            return Ok(());
        }
        // M24-5b — `defer Type.m(..)`: no receiver value to hold, so it rides the eager-args wrapper
        // instead of `Op::DeferMethod`.
        if self.receiverless_call_head(callee)? {
            let n =
                self.compile_receiverless_target(fc, (call.id, callee, args, named), call.span)?;
            fc.emit(Op::DeferCall(n), call.span);
            return Ok(());
        }
        if let ExprKind::Field {
            obj,
            name,
            name_span,
        } = &callee.kind
            && !crate::ast::is_tuple_index(name)
        {
            // `defer xs.sum()` over a `List[float]` needs the same hidden `0.0` seed the eager
            // `Op::CallMethod` emit pushes — `Op::DeferMethod` lands in the very same
            // `Vm::do_method_call`, so without it an empty list sums to `0`, not `0.0`. Method
            // dispatch has exactly three emit sites (`CallMethod`, `DeferMethod`, and the spawn
            // member arm, whose entry thunk runs a `CallMethod`); all three consult the seed.
            if let Some(seed) = self.sum_seed(name, args, *name_span) {
                self.compile_expr(fc, obj)?;
                self.emit_sum_seed(fc, &seed, call.span);
                let ic = self.next_method_ic();
                fc.emit(Op::DeferMethod(name.as_str().into(), 1, ic), call.span);
                return Ok(());
            }
            self.compile_expr(fc, obj)?;
            let argc = self.compile_call_args(fc, call.id, callee, args, named, call.span)?;
            // M24-5: the hidden witness arguments ride LAST, exactly as on the eager `Op::CallMethod`.
            let w = self.emit_member_witness_args(fc, callee, name, *name_span, call.span)?;
            let ic = self.next_method_ic();
            fc.emit(
                Op::DeferMethod(name.as_str().into(), argc + w, ic),
                call.span,
            );
            return Ok(());
        }
        // A deferred value call: its arguments -- default fills included -- are evaluated here, at
        // the statement, in the checker's slot plan when it bound the call.
        self.compile_expr(fc, callee)?;
        let argc = self.compile_call_args(fc, call.id, callee, args, named, call.span)?;
        let w = self.emit_indirect_witness_args(fc, callee, call.span)?;
        fc.emit(Op::DeferCall(argc + w), call.span);
        Ok(())
    }
}
