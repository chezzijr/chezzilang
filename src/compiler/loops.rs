//! Compiler: if, while, for, comprehensions, loop patching.
//! Split out of `src/compiler/mod.rs`. `super::*` is the parent module.

use super::*;
use crate::ast::{
    AssignOp, Block, CompClause, CompKind, Expr, ExprKind, Pattern, Span, Stmt, StmtKind,
};
use crate::base::op::Op;

impl Compiler {
    pub(super) fn compile_if(
        &mut self,
        fc: &mut FnComp,
        branches: &[(Expr, Block)],
        else_block: Option<&[Stmt]>,
        _span: Span,
    ) -> Result<(), CompileError> {
        let mut end_jumps = Vec::new();
        for (cond, body) in branches {
            self.compile_expr(fc, cond)?;
            fc.emit(Op::AsBool, cond.span);
            let skip = fc.emit_jump(Op::JumpIfFalse(0), cond.span);
            self.compile_defer_scoped_block(fc, body)?;
            end_jumps.push(fc.emit_jump(Op::Jump(0), cond.span));
            fc.patch_jump(skip);
        }
        if let Some(body) = else_block {
            self.compile_defer_scoped_block(fc, body)?;
        }
        for j in end_jumps {
            fc.patch_jump(j);
        }
        Ok(())
    }

    pub(super) fn compile_while(
        &mut self,
        fc: &mut FnComp,
        cond: &Expr,
        body: &[Stmt],
    ) -> Result<(), CompileError> {
        let loop_start = fc.here();
        self.compile_expr(fc, cond)?;
        fc.emit(Op::AsBool, cond.span);
        let exit = fc.emit_jump(Op::JumpIfFalse(0), cond.span);
        fc.loops.push(LoopCtx {
            continue_jumps: Vec::new(),
            break_jumps: Vec::new(),
            defer_floor: fc.defer_scopes,
            nursery_floor: fc.nursery_scopes,
        });
        self.compile_defer_scoped_block(fc, body)?;
        fc.emit(Op::Jump(loop_start), cond.span);
        fc.patch_jump(exit);
        let ctx = fc.loops.pop().expect("loop ctx pushed above");
        // `break` → loop exit (here, past the back-edge); `continue` → re-test the condition.
        let exit_target = fc.here();
        for j in ctx.break_jumps {
            fc.patch_jump_to(j, exit_target);
        }
        for j in ctx.continue_jumps {
            fc.patch_jump_to(j, loop_start);
        }
        Ok(())
    }

    pub(super) fn compile_for(
        &mut self,
        fc: &mut FnComp,
        target: &Pattern,
        iter: &Expr,
        body: &[Stmt],
        span: Span,
    ) -> Result<(), CompileError> {
        fc.begin_scope();
        fc.loops.push(LoopCtx {
            continue_jumps: Vec::new(),
            break_jumps: Vec::new(),
            defer_floor: fc.defer_scopes,
            nursery_floor: fc.nursery_scopes,
        });
        // A literal range WITH a start is a lazy counting loop, an optimisation of the general path
        // below: the grid of `tests/range_value_grid.rs` runs the literal and the same range bound to a
        // name and asserts both print the same. A literal with no start takes the general path, where
        // `Range.iter` faults (the checker reports the literal earlier).
        if let Some((start, end, inclusive)) = crate::ast::counting_range(target, iter) {
            // The start runs first (U7), then the end; both land in hidden slots before the loop
            // variable exists, so `for i in a..i` reads the OUTER `i`.
            self.compile_expr(fc, start)?;
            fc.emit(Op::AsInt, start.span);
            let start_slot = fc.add_hidden();
            fc.emit_hidden_set(start_slot, span);
            let end_slot = if let Some(end) = end {
                self.compile_expr(fc, end)?;
                fc.emit(Op::AsInt, end.span);
                let slot = fc.add_hidden();
                fc.emit_hidden_set(slot, span);
                Some(slot)
            } else {
                None
            };
            let i_slot = match target.target_ident() {
                Some(name) => fc.add_local(name.to_string()),
                None => fc.add_hidden(), // `counting_range` admits `_` only
            };
            // The COUNTER is a plain int mutated in place: the user var's own slot when it isn't
            // boxed, else a hidden slot (the boxed user cell is refreshed from it each iteration).
            let counter = fc.loopvar_raw_slot(i_slot);
            fc.emit_hidden_get(start_slot, span);
            fc.emit_set_local_raw(counter, span);

            let loop_start = fc.here();
            let mut exits = Vec::new();
            if let Some(end_slot) = end_slot {
                fc.emit_get_local_raw(counter, span);
                fc.emit_hidden_get(end_slot, span);
                fc.emit(if inclusive { Op::LtEq } else { Op::Lt }, span);
                exits.push(fc.emit_jump(Op::JumpIfFalse(0), span));
            }
            // Each iteration binds the user var to a fresh cell (when boxed) from the counter's value.
            fc.emit_loopvar_refresh(i_slot, counter, span);
            self.compile_defer_scoped_block(fc, body)?;
            // `continue` must land HERE — on the increment, not the condition (re-testing without
            // advancing `i` would loop forever) and not after it (skipping the advance also hangs).
            let inc_target = fc.here();
            if let (Some(end_slot), true) = (end_slot, inclusive) {
                // The last value of `a..=b` is `b`: leave before the increment so `a..=i64::MAX` ends
                // instead of overflowing.
                fc.emit_get_local_raw(counter, span);
                fc.emit_hidden_get(end_slot, span);
                fc.emit(Op::NotEq, span);
                exits.push(fc.emit_jump(Op::JumpIfFalse(0), span));
            }
            fc.emit_get_local_raw(counter, span);
            fc.emit(Op::ConstInt(1), span);
            fc.emit(Op::Add, span);
            fc.emit_set_local_raw(counter, span);
            fc.emit(Op::Jump(loop_start), span);
            for exit in exits {
                fc.patch_jump(exit);
            }
            self.patch_loop(fc, inc_target);
        } else {
            // The iterand may be a sequence (list/map-keys/set/str), a user struct implementing the
            // iterator protocol (`next(self) -> Option[T]`), OR a `Channel[T]` (`for v in ch:` — block
            // per value, end on close). The compiler is type-erased, so we branch at RUNTIME on
            // `IsChannel`/`HasNextHook`: the channel and struct paths are both driven LAZILY by an
            // `Option`-producing step (so an infinite iterator with a `break` terminates, and a channel
            // blocks then ends on close); anything else is indexed as a snapshotted list. The channel
            // and struct steps converge on ONE shared `Option` (None ⇒ exit, Some ⇒ bind) decoder —
            // they differ only in how they produce the `Option`.
            //
            // A tuple target (`for a, (b, c) in xs`) rides the SAME machinery: each path lands one
            // element in the hidden `elem` slot, which `emit_pattern` unpacks into the loop's raw
            // slots. A Map yields `(key, value)` tuples (owner U9), so `for k, v in m` is the same
            // tuple unpack; the checker admits a tuple target only over an element that is
            // statically a tuple of that shape.
            let single = target.target_ident().is_some();
            self.compile_expr(fc, iter)?;
            let iter_slot = fc.add_hidden();
            fc.emit_hidden_set(iter_slot, span);
            // ONE-TIME conversion: a value with the `iter` hook becomes its cursor here (`iter` wins
            // over `next`, DEC-250), and the cursor then takes the lazy `next()` step (`IsCursor`);
            // every other iterand passes through unchanged, so their fast paths are byte-identical.
            fc.emit_hidden_get(iter_slot, span);
            fc.emit(Op::IterableToCursor, iter.span);
            fc.emit_hidden_set(iter_slot, span);
            fc.emit_hidden_get(iter_slot, span);
            fc.emit(Op::IsChannel, span);
            let chan_mode_slot = fc.add_hidden(); // true ⇒ channel path (ChanRecvOrClosed)
            fc.emit_hidden_set(chan_mode_slot, span);
            fc.emit_hidden_get(iter_slot, span);
            fc.emit(Op::HasNextHook, span);
            let struct_mode_slot = fc.add_hidden(); // true ⇒ struct-iterator path (next())
            fc.emit_hidden_set(struct_mode_slot, span);
            // A generator result (experimental, VM-only) answers `next()` intrinsically, so it rides
            // the exact same lazy step as a struct iterator: force `struct_mode` true when the iterand
            // is a generator. (Kept off the seq path, which would wrongly snapshot it to a list.)
            fc.emit_hidden_get(iter_slot, span);
            fc.emit(Op::IsGenerator, span);
            let not_gen = fc.emit_jump(Op::JumpIfFalse(0), span);
            fc.emit(Op::True, span);
            fc.emit_hidden_set(struct_mode_slot, span);
            fc.patch_jump(not_gen);
            // A builtin cursor (`Obj::Iter`, an `.iter()` result — possibly the one just produced by
            // `IterableToCursor`) also answers `next()` intrinsically. Force `struct_mode` true so a
            // NAMED cursor rides the lazy `next()` step and DRIVES the shared heap cursor in place
            // (advancing the original), keeping `for` consistent with `.next()`/`List()`/`Set()` and
            // with struct iterators — instead of the seq path, which would snapshot a private copy.
            fc.emit_hidden_get(iter_slot, span);
            fc.emit(Op::IsCursor, span);
            let not_cursor = fc.emit_jump(Op::JumpIfFalse(0), span);
            fc.emit(Op::True, span);
            fc.emit_hidden_set(struct_mode_slot, span);
            fc.patch_jump(not_cursor);
            // Where each step's element lands: the one name's mechanism slot, or a hidden temp the
            // N-name destructure reads.
            let var_slots: Vec<usize> = target
                .binders()
                .iter()
                .map(|(v, _)| fc.add_local(v.to_string()))
                .collect();
            let var_raws: Vec<usize> = var_slots.iter().map(|&s| fc.loopvar_raw_slot(s)).collect();
            let elem = if single { var_raws[0] } else { fc.add_hidden() };
            let lst_slot = fc.add_hidden();
            let len_slot = fc.add_hidden();
            let idx_slot = fc.add_hidden();
            let opt_slot = fc.add_hidden();

            // Seq init (skipped on BOTH lazy paths): snapshot the iterand to a list, take its length,
            // start the index at 0. Skip when channel OR struct.
            fc.emit_hidden_get(chan_mode_slot, span);
            let chan_to_check_struct = fc.emit_jump(Op::JumpIfFalse(0), span); // not chan ⇒ check struct
            let chan_skip_init = fc.emit_jump(Op::Jump(0), span); // chan ⇒ skip seq init
            fc.patch_jump(chan_to_check_struct);
            fc.emit_hidden_get(struct_mode_slot, span);
            let to_seq_init = fc.emit_jump(Op::JumpIfFalse(0), span); // not struct ⇒ run seq init
            let struct_skip_init = fc.emit_jump(Op::Jump(0), span); // struct ⇒ skip seq init
            fc.patch_jump(to_seq_init);
            fc.emit_hidden_get(iter_slot, span);
            fc.emit(Op::ListClone, iter.span);
            fc.emit_hidden_set(lst_slot, span);
            fc.emit_hidden_get(lst_slot, span);
            fc.emit(Op::ArrLen, span);
            fc.emit_hidden_set(len_slot, span);
            fc.emit(Op::ConstInt(0), span);
            fc.emit_hidden_set(idx_slot, span);
            fc.patch_jump(chan_skip_init);
            fc.patch_jump(struct_skip_init);

            let loop_head = fc.here();
            // Channel step: `ChanRecvOrClosed` → opt_slot (blocks on empty-open, None on closed+drained).
            fc.emit_hidden_get(chan_mode_slot, span);
            let chan_to_struct = fc.emit_jump(Op::JumpIfFalse(0), span); // not chan ⇒ struct/seq
            fc.emit_hidden_get(iter_slot, span);
            fc.emit(Op::ChanRecvOrClosed, iter.span);
            fc.emit_hidden_set(opt_slot, span);
            let chan_to_opt = fc.emit_jump(Op::Jump(0), span); // ⇒ shared Option decoder
            fc.patch_jump(chan_to_struct);
            // Struct vs seq split.
            fc.emit_hidden_get(struct_mode_slot, span);
            let to_seq_step = fc.emit_jump(Op::JumpIfFalse(0), span); // false ⇒ seq step
            // ----- struct step: call next() → opt_slot -----
            fc.emit_hidden_get(iter_slot, span);
            let ic = self.next_method_ic();
            fc.emit(
                Op::CallMethod {
                    name: crate::base::hooks::Hook::Next.method().to_string(),
                    argc: 0,
                    ic,
                },
                span,
            );
            fc.emit_hidden_set(opt_slot, span);
            // ----- shared Option decoder (channel + struct): None ⇒ exit, Some(v) ⇒ bind v -----
            fc.patch_jump(chan_to_opt);
            fc.emit(Op::EnsureEnum(opt_slot), iter.span);
            // Test `None` first: a match falls through to the exit jump; a mismatch goes to `to_some`.
            let none_arm = fc.emit_jump(
                Op::MatchArm {
                    scrut: opt_slot,
                    variant: "None".to_string(),
                    variant_id: crate::base::op::VID_NONE_VARIANT,
                    enum_name: None,
                    nbind: 0,
                    bind_start: 0,
                    next: 0,
                },
                iter.span,
            );
            let lazy_exit = fc.emit_jump(Op::Jump(0), span); // None matched ⇒ leave the loop
            fc.patch_jump(none_arm); // not None ⇒ try Some here
            // `Some(v)`: a match binds the payload into `elem` and falls through to the body jump; a
            // non-Some jumps to the trap below.
            let some_arm = fc.emit_jump(
                Op::MatchArm {
                    scrut: opt_slot,
                    variant: "Some".to_string(),
                    variant_id: crate::base::op::VID_SOME,
                    enum_name: None,
                    nbind: 1,
                    bind_start: elem,
                    next: 0,
                },
                iter.span,
            );
            let to_body = fc.emit_jump(Op::Jump(0), span); // Some matched ⇒ run the body
            fc.patch_jump(some_arm); // neither None nor Some ⇒ the trap
            fc.emit(Op::MatchNoArm(opt_slot), iter.span); // not Option ⇒ runtime trap
            // ----- seq step: bounds-check the index, read the element -----
            fc.patch_jump(to_seq_step);
            fc.emit_hidden_get(idx_slot, span);
            fc.emit_hidden_get(len_slot, span);
            fc.emit(Op::Lt, span);
            let seq_exit = fc.emit_jump(Op::JumpIfFalse(0), span);
            fc.emit_hidden_get(lst_slot, span);
            fc.emit_hidden_get(idx_slot, span);
            fc.emit(Op::GetIndex, span);
            fc.emit_set_local_raw(elem, span);
            fc.patch_jump(to_body);
            if !single {
                // Unpack the element into the loop's raw slots. Reached from the lazy `Some(v)` bind
                // and from the seq read.
                let mut fails = Vec::new();
                self.emit_pattern(
                    fc,
                    target,
                    elem,
                    &mut fails,
                    span,
                    &mut BindInto::Loop(var_raws.iter()),
                )?;
                debug_assert!(fails.is_empty(), "a binding target never tests");
            }
            // Fresh cell per iteration for each boxed (captured) loop var (C1); no-op when unboxed.
            for (&vs, &vr) in var_slots.iter().zip(&var_raws) {
                fc.emit_loopvar_refresh(vs, vr, span);
            }
            self.compile_defer_scoped_block(fc, body)?;
            // `continue` lands HERE — the advance step. For a channel/struct, "advance" is just
            // re-looping (the next lazy step); for a sequence, it's the index increment.
            let inc_target = fc.here();
            fc.emit_hidden_get(chan_mode_slot, span);
            let inc_check_struct = fc.emit_jump(Op::JumpIfFalse(0), span);
            fc.emit(Op::Jump(loop_head), span); // channel: re-loop, ChanRecvOrClosed advances
            fc.patch_jump(inc_check_struct);
            fc.emit_hidden_get(struct_mode_slot, span);
            let to_seq_inc = fc.emit_jump(Op::JumpIfFalse(0), span);
            fc.emit(Op::Jump(loop_head), span); // struct: re-loop, next() advances
            fc.patch_jump(to_seq_inc);
            fc.emit_hidden_get(idx_slot, span);
            fc.emit(Op::ConstInt(1), span);
            fc.emit(Op::Add, span);
            fc.emit_hidden_set(idx_slot, span);
            fc.emit(Op::Jump(loop_head), span);
            // All exit paths land here (past the back-edge).
            fc.patch_jump(lazy_exit);
            fc.patch_jump(seq_exit);
            self.patch_loop(fc, inc_target);
        }
        fc.end_scope();
        Ok(())
    }

    /// Compile a comprehension by reusing `compile_for`: seed a hidden accumulator, then run a
    /// synthesized loop body that appends the element (`push`/`add`) or inserts the key→value pair.
    /// This means comprehensions iterate every collection — and the struct-iterator protocol —
    /// exactly like a `for` loop, with no duplicated iteration logic. Multiple `for` clauses nest
    /// (first clause outermost, last innermost) by folding the clauses RIGHT-TO-LEFT: the innermost
    /// body is the accumulator append; each clause wraps it in that clause's guards (an `if`) and
    /// then in a `compile_for`. `compile_for` opens a scope and binds the clause's vars, so a later
    /// clause's `iter`/guards (which sit inside an earlier `compile_for`'s body) see the earlier
    /// bindings. The finished accumulator is left on the stack as the expression's value.
    pub(super) fn compile_comprehension(
        &mut self,
        fc: &mut FnComp,
        kind: CompKind,
        key: Option<&Expr>,
        elem: &Expr,
        clauses: &[CompClause],
        span: Span,
    ) -> Result<(), CompileError> {
        fc.begin_scope();
        // Seed the accumulator and store it in a hidden-named local. The synthesized body refers to
        // it by name; `$comp` can't collide with a user identifier (the lexer never produces `$`).
        match kind {
            CompKind::List => fc.emit(Op::NewList(0), span),
            CompKind::Set => fc.emit(Op::NewSet(0), span),
            CompKind::Map => fc.emit(Op::NewMap(0), span),
        }
        let acc_name = "$comp".to_string();
        // `$comp` is synthesized, never referenced by a user closure, so it is never boxed — the named
        // emit helpers reduce to plain `SetLocal`/`GetLocal` for it.
        let acc_slot = fc.emit_decl_named(acc_name.clone(), span);

        let acc = Expr {
            id: crate::ast::NodeId::SYNTH,
            kind: ExprKind::Ident(acc_name),
            span,
        };
        let innermost_stmt = match kind {
            CompKind::List => method_call_stmt(acc, "push", vec![elem.clone()], span),
            CompKind::Set => method_call_stmt(acc, "insert", vec![elem.clone()], span),
            CompKind::Map => {
                let key = key.expect("a map comprehension carries a key").clone();
                Stmt {
                    kind: StmtKind::Assign {
                        target: Expr {
                            id: crate::ast::NodeId::SYNTH,
                            kind: ExprKind::Index {
                                obj: Box::new(acc),
                                index: Some(Box::new(key)),
                                types: Vec::new(),
                            },
                            span,
                        },
                        op: AssignOp::Eq,
                        value: elem.clone(),
                    },
                    span,
                }
            }
        };

        // Build the synthesized nested-loop body from the inside out, then compile only the
        // outermost `for` (which contains all the inner ones). Folding right-to-left makes clause 0
        // the outermost loop, matching left-to-right source order (Python comprehension semantics).
        let mut body: Vec<Stmt> = vec![innermost_stmt];
        for clause in clauses.iter().rev() {
            // Wrap in this clause's guards (each `if` filters the body; chained guards nest).
            for g in clause.guards.iter().rev() {
                body = vec![Stmt {
                    kind: StmtKind::If {
                        branches: vec![(g.clone(), body)],
                        else_block: None,
                    },
                    span,
                }];
            }
            // Wrap in this clause's `for`. The first clause is compiled last (outermost).
            body = vec![Stmt {
                kind: StmtKind::For {
                    target: clause.target.clone(),
                    iter: (*clause.iter).clone(),
                    body,
                },
                span,
            }];
        }

        // `body` is now a single outermost `for` statement. Compile it via `compile_for`.
        let Stmt {
            kind:
                StmtKind::For {
                    target,
                    iter,
                    body: inner,
                },
            ..
        } = &body[0]
        else {
            unreachable!("a comprehension always has at least one for clause")
        };
        self.compile_for(fc, target, iter, inner, span)?;
        // The comprehension's value is the finished accumulator.
        fc.emit_get_named(acc_slot, span);
        fc.end_scope();
        Ok(())
    }

    /// Pop the innermost `LoopCtx` and patch its pending jumps: `continue` → `inc_target` (the
    /// loop's increment), `break` → the current position (the loop exit, past the back-edge).
    fn patch_loop(&self, fc: &mut FnComp, inc_target: usize) {
        let ctx = fc.loops.pop().expect("for-loop ctx pushed in compile_for");
        let exit_target = fc.here();
        for j in ctx.break_jumps {
            fc.patch_jump_to(j, exit_target);
        }
        for j in ctx.continue_jumps {
            fc.patch_jump_to(j, inc_target);
        }
    }
}
