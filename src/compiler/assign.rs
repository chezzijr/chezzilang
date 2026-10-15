//! Compiler: assignment: `compile_assign`, element stores, `emit_store`, `emit_load`.
//! Split out of `src/compiler/mod.rs`. `super::*` is the parent module.

use super::*;
use crate::ast::{AssignOp, Expr, ExprKind, Span};
use crate::base::op::{NO_IC, Op};

impl Compiler {
    pub(super) fn compile_assign(
        &mut self,
        fc: &mut FnComp,
        target: &Expr,
        op: AssignOp,
        value: &Expr,
        span: Span,
    ) -> Result<(), CompileError> {
        match &target.kind {
            ExprKind::Ident(name) => match op.to_binop() {
                None => {
                    self.compile_expr(fc, value)?;
                    if target.is_blank() {
                        // TICKET-142 (W14-32): `_ = e` evaluates `e` and discards it; `_` has no
                        // slot to store to (the checker never declares it).
                        fc.emit(Op::Pop, span);
                    } else {
                        self.emit_store(fc, name, span);
                    }
                }
                Some(bin) => {
                    self.emit_load(fc, name, span);
                    self.compile_expr(fc, value)?;
                    fc.emit(binary_op(bin), span);
                    self.emit_store(fc, name, span);
                }
            },
            // `obj.f = v` → RHS first (Python order), then `emit_assign_value_first` rebuilds the
            // `[obj, v]` order `SetField` expects. Compound dups `obj` to read-modify-write and
            // keeps evaluating `obj` first, because `x OP= v` is `x = x OP v` (Python's order too).
            ExprKind::Field { obj, name, .. } => {
                if let Some(bin) = op.to_binop() {
                    self.compile_expr(fc, obj)?;
                    let ic = self.next_field_ic(name);
                    fc.emit(Op::Dup, span);
                    fc.emit(
                        Op::GetField {
                            name: name.clone(),
                            ic,
                        },
                        target.span,
                    );
                    self.compile_expr(fc, value)?;
                    fc.emit(binary_op(bin), span);
                    let ic = self.next_field_ic(name);
                    fc.emit(
                        Op::SetField {
                            name: name.clone(),
                            ic,
                        },
                        span,
                    );
                } else {
                    self.compile_expr(fc, value)?;
                    self.emit_assign_value_first(fc, target, span)?;
                }
            }
            // `obj[i] = v` → RHS first (Python order), then `emit_assign_value_first` rebuilds the
            // `[obj, i, v]` order `SetIndex` expects. Compound dups `[obj, i]` to read-modify-write.
            ExprKind::Index {
                obj,
                index: Some(index),
                ..
            } => {
                // No `AsInt`: the index may be a map key (str/bool). `GetIndex`/`SetIndex`
                // validate int-ness in their list/str arms at runtime.
                if let Some(bin) = op.to_binop() {
                    self.compile_expr(fc, obj)?;
                    self.compile_expr(fc, index)?;
                    fc.emit(Op::Dup2, span);
                    fc.emit(Op::GetIndex, target.span);
                    self.compile_expr(fc, value)?;
                    fc.emit(binary_op(bin), span);
                    fc.emit(Op::SetIndex, span);
                } else {
                    self.compile_expr(fc, value)?;
                    self.emit_assign_value_first(fc, target, span)?;
                }
            }
            // `a, b = b, a` — multi-target tuple assignment (op is always `Eq`; the parser enforces
            // it). Evaluate the FULL RHS tuple into a hidden local FIRST (Python semantics — so a
            // swap whose index appears on both sides is correct), then store each element into its
            // target left-to-right. Mirrors the destructuring-let lowering.
            ExprKind::Tuple(targets) => {
                // `value` is either a tuple literal (`b, a`) or any tuple-valued expression
                // (`f()` returning a tuple). Either way `compile_expr` leaves the full tuple on the
                // stack; the checker has verified arity.
                self.compile_expr(fc, value)?; // builds the full RHS tuple on the stack
                let tuple_slot = fc.add_hidden();
                fc.emit_hidden_set(tuple_slot, span);
                for (i, t) in targets.iter().enumerate() {
                    self.compile_assign_element(fc, t, tuple_slot, i, span)?;
                }
            }
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Interp(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::NoneLit
            | ExprKind::Pass
            | ExprKind::List(..)
            | ExprKind::Map(_)
            | ExprKind::Set(_)
            | ExprKind::Comprehension { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Compare { .. }
            | ExprKind::Range { .. }
            | ExprKind::Call { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => {
                // The parser admits only places (`ExprKind::is_place`).
                return Err(CompileError {
                    message: "internal: a non-place assignment target passed the parser"
                        .to_string(),
                    span,
                });
            }
        }
        Ok(())
    }

    /// Lower one element of a multi-target tuple assignment: load the stashed RHS tuple's element
    /// `i` (`GetLocal(slot)` + `GetField(i)`) onto the stack, then store it into `target` with plain
    /// `=` semantics across the ident / field / index target shapes.
    fn compile_assign_element(
        &mut self,
        fc: &mut FnComp,
        target: &Expr,
        tuple_slot: usize,
        i: usize,
        span: Span,
    ) -> Result<(), CompileError> {
        // A blank `_` element discards its tuple element: nothing to load or store.
        if target.is_blank() {
            return Ok(());
        }
        match &target.kind {
            ExprKind::Ident(name) => {
                fc.emit_hidden_get(tuple_slot, span);
                fc.emit(
                    Op::GetField {
                        name: i.to_string(),
                        ic: NO_IC,
                    },
                    span,
                );
                self.emit_store(fc, name, span);
            }
            // A nested target (`a, (b, c) = t`): stash the element, then store each inner element.
            ExprKind::Tuple(inner) => {
                fc.emit_hidden_get(tuple_slot, span);
                fc.emit(
                    Op::GetField {
                        name: i.to_string(),
                        ic: NO_IC,
                    },
                    span,
                );
                let nested = fc.add_hidden();
                fc.emit_hidden_set(nested, span);
                for (j, t) in inner.iter().enumerate() {
                    self.compile_assign_element(fc, t, nested, j, span)?;
                }
            }
            ExprKind::Field { obj, name, .. } => {
                self.compile_expr(fc, obj)?;
                fc.emit_hidden_get(tuple_slot, span);
                fc.emit(
                    Op::GetField {
                        name: i.to_string(),
                        ic: NO_IC,
                    },
                    span,
                );
                let ic = self.next_field_ic(name);
                fc.emit(
                    Op::SetField {
                        name: name.clone(),
                        ic,
                    },
                    span,
                );
            }
            ExprKind::Index {
                obj,
                index: Some(index),
                ..
            } => {
                self.compile_expr(fc, obj)?;
                self.compile_expr(fc, index)?;
                fc.emit_hidden_get(tuple_slot, span);
                fc.emit(
                    Op::GetField {
                        name: i.to_string(),
                        ic: NO_IC,
                    },
                    span,
                );
                fc.emit(Op::SetIndex, span);
            }
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Interp(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::NoneLit
            | ExprKind::Pass
            | ExprKind::List(..)
            | ExprKind::Map(_)
            | ExprKind::Set(_)
            | ExprKind::Comprehension { .. }
            | ExprKind::Unary { .. }
            | ExprKind::Binary { .. }
            | ExprKind::Compare { .. }
            | ExprKind::Range { .. }
            | ExprKind::Call { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => {
                // The parser admits only places (`ExprKind::is_place`).
                return Err(CompileError {
                    message: "internal: a non-place assignment target passed the parser"
                        .to_string(),
                    span,
                });
            }
        }
        Ok(())
    }

    /// Store the value on top of the stack into an existing binding (`=`/`+=`/`-=` semantics:
    /// never creates — a global that doesn't exist is a runtime error).
    pub(super) fn emit_store(&mut self, fc: &mut FnComp, name: &str, span: Span) {
        match fc.resolve_local(name) {
            // A boxed owner-side local writes through its cell (`emit_set_named`).
            Some(slot) => fc.emit_set_named(slot, span),
            // Uniform by-reference capture: a `defer:`/`spawn:` frame WRITING a captured name mutates
            // the shared cell. Captures are uniformly cells (spec B4), so this is `GetCaptured; <val
            // already on stack>; CellStore` (CellStore pops handle-first, then value — the Task-A
            // operand convention). This REPLACES the old fall-through to `SetGlobalSlot` (the
            // phantom-global write bug: A2 printed `0`).
            None if fc.captures(name) => {
                let slot = fc
                    .captured_names
                    .iter()
                    .position(|n| n == name)
                    .expect("captures() implies a capture slot") as u32;
                fc.emit(Op::GetCaptured(slot), span);
                fc.emit(Op::CellStore, span);
            }
            None => fc.emit(Op::SetGlobalSlot(self.global_slot(name)), span),
        }
    }

    /// Load a name's value (local → captured → global).
    pub(super) fn emit_load(&mut self, fc: &mut FnComp, name: &str, span: Span) {
        match fc.resolve_local(name) {
            // A boxed owner-side local dereferences its cell (`emit_get_named`).
            Some(slot) => fc.emit_get_named(slot, span),
            None if fc.captures(name) => {
                // Positional capture (lever #3): the slot is the name's index in this closure's
                // `captured_names` (the snapshot order `MakeClosure` populated). `captures` just
                // confirmed membership, so `position` always finds it. Captures are uniformly cells
                // (spec B4), so every captured read dereferences the handle with a trailing `CellLoad`.
                let slot = fc
                    .captured_names
                    .iter()
                    .position(|n| n == name)
                    .expect("captures() implies a capture slot") as u32;
                fc.emit(Op::GetCaptured(slot), span);
                fc.emit(Op::CellLoad, span);
            }
            None => fc.emit(Op::GetGlobalSlot(self.global_slot(name)), span),
        }
    }
}
