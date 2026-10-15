//! Compiler: blocks, defer scopes, `compile_stmt`, `wait`, `parallel:`, `spawn`.
//! Split out of `src/compiler/mod.rs`. `super::*` is the parent module.

use super::*;
use crate::ast::{
    Expr, ExprKind, Pattern, Span, SpawnTarget, Stmt, StmtKind, WaitArm, WaitArmKind,
};
use crate::vm::op::{Op, ProtoId, WaitMeta};

impl Compiler {
    /// Compile statements into the current scope without opening a new one (used for the toplevel
    /// and for blocks whose scope is managed by the caller).
    pub(super) fn compile_block_flat(
        &mut self,
        fc: &mut FnComp,
        stmts: &[Stmt],
    ) -> Result<(), CompileError> {
        for stmt in stmts {
            self.compile_stmt(fc, stmt)?;
        }
        Ok(())
    }

    /// Compile a block in a fresh lexical scope (locals don't leak past the block).
    pub(super) fn compile_block_scoped(
        &mut self,
        fc: &mut FnComp,
        stmts: &[Stmt],
    ) -> Result<(), CompileError> {
        fc.begin_scope();
        self.compile_block_flat(fc, stmts)?;
        fc.end_scope();
        Ok(())
    }

    /// Compile a lexical block that is also a **defer scope**: any `defer` directly inside it runs
    /// when this block exits, not when the whole frame does. When the block statically holds a
    /// `defer` we bracket it with `EnterDeferScope`/`LeaveDeferScope`; otherwise this is exactly
    /// `compile_block_scoped` and emits nothing extra (defer-free code is byte-identical).
    pub(super) fn compile_defer_scoped_block(
        &mut self,
        fc: &mut FnComp,
        stmts: &[Stmt],
    ) -> Result<(), CompileError> {
        let has_defer = block_has_defer(stmts);
        if has_defer {
            fc.emit(Op::EnterDeferScope, stmts[0].span);
            fc.defer_scopes += 1;
        }
        self.compile_block_scoped(fc, stmts)?;
        if has_defer {
            fc.emit(Op::LeaveDeferScope, stmts[stmts.len() - 1].span);
            fc.defer_scopes -= 1;
        }
        Ok(())
    }

    /// A statement-form `match` arm body as a defer scope. The arm's lexical scope is already
    /// opened/closed by `compile_match_general`, so this brackets the *flat* body with defer-scope
    /// ops when (and only when) it directly contains a `defer`.
    pub(super) fn compile_defer_scoped_arm(
        &mut self,
        fc: &mut FnComp,
        stmts: &[Stmt],
    ) -> Result<(), CompileError> {
        let has_defer = block_has_defer(stmts);
        if has_defer {
            fc.emit(Op::EnterDeferScope, stmts[0].span);
            fc.defer_scopes += 1;
        }
        self.compile_block_flat(fc, stmts)?;
        if has_defer {
            fc.emit(Op::LeaveDeferScope, stmts[stmts.len() - 1].span);
            fc.defer_scopes -= 1;
        }
        Ok(())
    }

    /// Drain everything a `break`/`continue` leaves, innermost-first, on the jump path only (the
    /// blocks' own `LeaveDeferScope`/`JoinNursery` are skipped by the jump, so each is emitted
    /// exactly once). W14-4: a `parallel:` level the escape leaves runs its OWN body defers, then a
    /// `ReclaimNursery` (cancel + wait, so its children run their defers), and only then the defers
    /// of the enclosing blocks down to the loop body (inclusive) -- the order
    /// `Vm::unwind_escaped_levels` gives `return`/`?`. Every level between a loop and its `break` is
    /// a `parallel:` level: implicit nurseries open at `FnComp` entry, below every loop's
    /// `nursery_floor`. The compiler's `defer_scopes`/`nursery_scopes` counts are unchanged: the
    /// scopes stay lexically open and emit their natural ops on the fall-through path.
    fn emit_loop_escape_drain(&mut self, fc: &mut FnComp, span: Span) {
        let Some((defer_floor, nursery_floor)) =
            fc.loops.last().map(|c| (c.defer_floor, c.nursery_floor))
        else {
            return; // no enclosing loop — the checker rejects this `break`/`continue`
        };
        let escaped = fc.nursery_scopes - nursery_floor;
        let marks: Vec<usize> =
            fc.nursery_defer_marks[fc.nursery_defer_marks.len() - escaped..].to_vec();
        let mut open = fc.defer_scopes;
        for &mark in marks.iter().rev() {
            for _ in 0..(open - mark) {
                fc.emit(Op::LeaveDeferScope, span);
            }
            open = mark;
            fc.emit(Op::ReclaimNursery, span);
        }
        for _ in 0..(open - defer_floor) {
            fc.emit(Op::LeaveDeferScope, span);
        }
    }

    pub(super) fn compile_stmt(
        &mut self,
        fc: &mut FnComp,
        stmt: &Stmt,
    ) -> Result<(), CompileError> {
        match &stmt.kind {
            StmtKind::Let { target, value, .. } => {
                self.compile_expr(fc, value)?;
                match target {
                    // destructuring let `a, (b, c) := value`: stash the tuple in a hidden local; the
                    // one pattern lowering reads each element (`GetField`) and declares the names.
                    Pattern::Tuple(_) => {
                        let tuple_slot = fc.add_hidden();
                        fc.emit_hidden_set(tuple_slot, stmt.span);
                        let mut fails = Vec::new();
                        self.emit_pattern(fc, target, tuple_slot, &mut fails, stmt.span, &mut BindInto::Decl)?;
                        debug_assert!(fails.is_empty(), "a binding target never tests");
                    }
                    // TICKET-142 (W14-32): `_ := e` evaluates `e` and discards it — `_` is never
                    // declared, so it has no local slot and no module global slot (the checker
                    // agrees: it never declares `_`).
                    Pattern::Wildcard => fc.emit(Op::Pop, stmt.span),
                    Pattern::Ident(name, _, _) => {
                        if fc.is_global_scope() {
                            fc.emit(Op::DefineGlobalSlot(self.global_slot(name)), stmt.span);
                        } else {
                            fc.emit_decl_frame(name.clone(), stmt.span);
                        }
                    }
                    Pattern::Variant { .. }
                    | Pattern::Literal(_)
                    | Pattern::Range { .. }
                    | Pattern::Or(_)
                    | Pattern::Carrier { .. } => {
                        unreachable!("a binding target holds Ident, Wildcard or Tuple only")
                    }
                }
                Ok(())
            }
            StmtKind::Assign { target, op, value } => self.compile_assign(fc, target, *op, value, stmt.span),
            StmtKind::Expr(expr) => {
                self.compile_expr(fc, expr)?;
                // An expression statement's value is discarded like any other pop; the checker's
                // W8-2 warning is the only check on a dropped carrier now. Use `expr.span` (not
                // `stmt.span`) so a fault inside the expression still points at the expression.
                fc.emit(Op::Pop, expr.span);
                Ok(())
            }
            // Top-level fns are hoisted; struct/enum/import are no-ops at execution time. A `fn`
            // statement nested in a block is a FIRST-CLASS LOCAL function — a closure-with-a-name:
            // it captures outer bindings by reference (uniform cell model, like a closure expression)
            // and may RECURSE (letrec: its name is bound into its own cell BEFORE the body captures
            // it). Routed through `MakeClosure`, NOT the non-capturing `MakeFunc` path.
            StmtKind::Fn(decl) => {
                if fc.is_global_scope() {
                    Ok(()) // already hoisted
                } else if fc.is_boxed_name(&decl.name) {
                    // The name is captured (recursive self-reference and/or captured by a deeper
                    // sibling closure), so it lives in a CELL. LETREC: create the empty cell in the
                    // name's slot FIRST, snapshot it into the child's capture env, build the closure,
                    // then store the finished closure back INTO that same cell. Ordering is
                    // load-bearing — the cell handle captured by the body must be the same handle the
                    // closure is stored into, so a self-call resolves to this very closure.
                    let slot = fc.add_local(decl.name.clone());
                    fc.emit(Op::Nil, stmt.span);
                    fc.emit(Op::NewCell, stmt.span);
                    fc.emit_set_local_raw(slot, stmt.span);
                    // Free-variable capture (Finding D): keep only the names the body references
                    // (relative to its own params). The recursive self-name is free in a recursive
                    // body → stays captured → the self-call resolves through the cell above.
                    let snap = fc.snapshot_entries();
                    let (kept, free) =
                        filter_entries_free_block(&snap, &decl.body, &decl.params);
                    let entries = self.with_witness_captures(&snap, kept, Some(&free));
                    let captured_names: Vec<String> =
                        entries.iter().map(|e| e.name.clone()).collect();
                    let pid = self.compile_fn_captured(decl, captured_names)?;
                    fc.emit(Op::MakeClosure(pid, entries), stmt.span);
                    // Stack: [closure]; push the cell handle → [closure, handle]; `CellStore` pops
                    // handle-first then value → cell := closure. Stack drained.
                    fc.emit_get_local_raw(slot, stmt.span);
                    fc.emit(Op::CellStore, stmt.span);
                    Ok(())
                } else {
                    // Not captured: snapshot the enclosing bindings BEFORE declaring the name, build
                    // the closure, and bind it plainly. (Still a closure so it can capture outer
                    // locals — it just needs no self-cell because nothing captures it.)
                    // Free-variable capture (Finding D): keep only referenced names.
                    let snap = fc.snapshot_entries();
                    let (kept, free) =
                        filter_entries_free_block(&snap, &decl.body, &decl.params);
                    let entries = self.with_witness_captures(&snap, kept, Some(&free));
                    let captured_names: Vec<String> =
                        entries.iter().map(|e| e.name.clone()).collect();
                    let pid = self.compile_fn_captured(decl, captured_names)?;
                    fc.emit(Op::MakeClosure(pid, entries), stmt.span);
                    fc.emit_decl_named(decl.name.clone(), stmt.span);
                    Ok(())
                }
            }
            StmtKind::Struct { .. }
            | StmtKind::Enum { .. }
            | StmtKind::Protocol { .. }
            | StmtKind::Extern { .. } // bound at module init (see compile_module), like top-level fn
            // A `native fn`/`native ctor` decl is a compile-time SIGNATURE source only — it gets no
            // bytecode, no binding, and is never a callable user fn (dispatch stays name-keyed).
            | StmtKind::Native(_)
            // A `native struct` decl is likewise a compile-time SIGNATURE source only (checker-harvested
            // from a companion stub); its runtime layout stays native (name-keyed), so no bytecode.
            | StmtKind::NativeStruct { .. }
            // A `native enum` decl (Option/Result shape mirror) is a compile-time SIGNATURE source only;
            // construction/match stay native + Rust-wired, so it emits no bytecode.
            | StmtKind::NativeEnum { .. }
            | StmtKind::NativeType { .. }
            | StmtKind::TypeAlias { .. }
            | StmtKind::Import(_) => Ok(()),
            StmtKind::Return(value) => {
                match value {
                    Some(e) => self.compile_expr(fc, e)?,
                    None => {
                        fc.emit(Op::Nil, stmt.span);
                        self.emit_ret_coerce(fc, stmt.span)?;
                    }
                }
                fc.emit(Op::Return, stmt.span);
                Ok(())
            }
            // `yield <expr>` — evaluate the operand and suspend (experimental generators). The value
            // is left on the stack for `Op::Yield`'s runtime handler to hand back to `.next()`.
            StmtKind::Yield(e) => {
                self.compile_expr(fc, e)?;
                fc.emit(Op::Yield, stmt.span);
                Ok(())
            }
            StmtKind::Break => {
                // Drain the current iteration's defers and cancel any `parallel:` nursery this `break`
                // leaves (TASK B, silent cancel §2c1), innermost-first (W14-4): a nursery's body
                // defers, then its reclaim (its children unwind), then the enclosing blocks' defers.
                // They run at the `break`, not at function return.
                self.emit_loop_escape_drain(fc, stmt.span);
                let j = fc.emit_jump(Op::Jump(0), stmt.span);
                match fc.current_loop() {
                    Some(ctx) => ctx.break_jumps.push(j),
                    None => return Err(CompileError {
                        message: "break outside loop".to_string(),
                        span: stmt.span,
                    }),
                }
                Ok(())
            }
            StmtKind::Continue => {
                self.emit_loop_escape_drain(fc, stmt.span);
                let j = fc.emit_jump(Op::Jump(0), stmt.span);
                match fc.current_loop() {
                    Some(ctx) => ctx.continue_jumps.push(j),
                    None => return Err(CompileError {
                        message: "continue outside loop".to_string(),
                        span: stmt.span,
                    }),
                }
                Ok(())
            }
            // `pass` — a no-op statement; emits no bytecode.
            StmtKind::Pass => Ok(()),
            StmtKind::Assert { cond, msg } => {
                // A comparison condition is split so the operand VALUES survive the comparison
                // opcode: duplicate them (`Op::Dup2`) before comparing, compare the copies, and on
                // the failing path the originals are still on the stack (under `msg`) for
                // `Op::Assert` to render. `In` is excluded (see `assert_cmp`).
                if let ExprKind::Binary { op, lhs, rhs } = &cond.kind
                    && let Some(c) = assert_cmp(*op)
                {
                    self.compile_expr(fc, lhs)?;
                    self.compile_expr(fc, rhs)?;
                    fc.emit(Op::Dup2, cond.span);
                    fc.emit(binary_op(*op), cond.span);
                    let to_fail = fc.emit_jump(Op::JumpIfFalse(0), stmt.span);
                    // Passing path: pop the two duplicated operands so a passing comparison assert
                    // leaves no residue on the stack (see ## Decisions — unbounded growth in a loop).
                    fc.emit(Op::Pop, stmt.span);
                    fc.emit(Op::Pop, stmt.span);
                    let to_end = fc.emit_jump(Op::Jump(0), stmt.span);
                    fc.patch_jump(to_fail);
                    if let Some(m) = msg {
                        self.compile_expr(fc, m)?;
                    }
                    fc.emit(
                        Op::Assert {
                            has_msg: msg.is_some(),
                            cmp: Some(c),
                        },
                        stmt.span,
                    );
                    fc.patch_jump(to_end);
                    return Ok(());
                }
                // Lazy message evaluation — `msg` is only evaluated on failure: compile `cond`, and
                // only on the false path compile `msg` then
                // `Op::Assert` (which always faults). A passing assert never touches `msg`, so a
                // side-effecting/faulting message expression never runs on the passing path.
                // `Op::Assert` carries `stmt.span` so the fault location points at the `assert` statement itself.
                self.compile_expr(fc, cond)?;
                let to_fail = fc.emit_jump(Op::JumpIfFalse(0), stmt.span);
                let to_end = fc.emit_jump(Op::Jump(0), stmt.span);
                fc.patch_jump(to_fail);
                if let Some(m) = msg {
                    self.compile_expr(fc, m)?;
                }
                fc.emit(
                    Op::Assert {
                        has_msg: msg.is_some(),
                        cmp: None,
                    },
                    stmt.span,
                );
                fc.patch_jump(to_end);
                Ok(())
            }
            StmtKind::Defer(target) => self.compile_defer(fc, target, stmt.span),
            StmtKind::If { branches, else_block } => self.compile_if(fc, branches, else_block.as_deref(), stmt.span),
            StmtKind::While { cond, body } => self.compile_while(fc, cond, body),
            StmtKind::For { target, iter, body } => {
                self.compile_for(fc, target, iter, body, stmt.span)
            }
            StmtKind::Match { scrutinee, arms } => self.compile_match(fc, scrutinee, arms, stmt.span),
            // Concurrency C4 — sequential, run-to-completion executor.
            StmtKind::Parallel { body } => self.compile_parallel(fc, body, stmt.span),
            StmtKind::Spawn(target) => self.compile_spawn(fc, target, stmt.span),
            StmtKind::Wait { arms, else_block } => {
                self.compile_wait(fc, arms, else_block.as_deref(), stmt.span)
            }
        }
    }

    /// `wait:` — Chezzi's `select` (§6d). Evaluate each arm's channel once (source order) → N handles
    /// on the stack, then a single `Op::WaitPoll` that polls them and jumps to the chosen arm's body
    /// (or `else`), or parks. Each arm body is a lexical sub-scope (like a `match` arm): the selected
    /// value arrives on the stack top and the prologue binds (`:=`) / assigns (`=`) / drops (`_`) it.
    /// A SEND arm (`ch.send(v):`) instead leaves `[chan, value]` and binds nothing.
    fn compile_wait(
        &mut self,
        fc: &mut FnComp,
        arms: &[WaitArm],
        else_block: Option<&[Stmt]>,
        span: Span,
    ) -> Result<(), CompileError> {
        // Evaluate each arm's operands once, source order (left on the stack for the poll to re-read
        // on every re-park). A recv arm pushes ONE handle (the channel); a send arm pushes TWO (the
        // channel THEN the value) — the exact top-to-bottom eval order Go's `select` requires.
        let mut is_send = Vec::with_capacity(arms.len());
        for arm in arms {
            match &arm.kind {
                WaitArmKind::Recv { chan, .. } => {
                    self.compile_expr(fc, chan)?;
                    is_send.push(false);
                }
                WaitArmKind::Send { call } => {
                    let (obj, value) = send_arm_parts(call);
                    self.compile_expr(fc, obj)?;
                    self.compile_expr(fc, value)?;
                    is_send.push(true);
                }
            }
        }
        // Placeholder; back-patched with the arm/else targets once the bodies are laid out.
        let poll_at = fc.emit_jump(
            Op::WaitPoll(Box::new(WaitMeta {
                n: arms.len(),
                arm_targets: Vec::new(),
                else_target: None,
                is_send: is_send.clone(),
            })),
            span,
        );
        let mut arm_targets = Vec::with_capacity(arms.len());
        let mut end_jumps = Vec::new();
        for arm in arms {
            arm_targets.push(fc.here());
            fc.begin_scope();
            match &arm.kind {
                // The selected value is on the stack top — deliver it per the arm's target.
                WaitArmKind::Recv { target, .. } => match target {
                    Pattern::Ident(name, _, _) => {
                        fc.emit_decl_named(name.clone(), arm.span);
                    }
                    Pattern::Wildcard => fc.emit(Op::Pop, arm.span),
                    Pattern::Tuple(_) => {
                        let slot = fc.add_hidden();
                        fc.emit_hidden_set(slot, arm.span);
                        let mut fails = Vec::new();
                        self.emit_pattern(
                            fc,
                            target,
                            slot,
                            &mut fails,
                            arm.span,
                            &mut BindInto::Decl,
                        )?;
                        debug_assert!(fails.is_empty(), "a binding target never tests");
                    }
                    Pattern::Variant { .. }
                    | Pattern::Literal(_)
                    | Pattern::Range { .. }
                    | Pattern::Or(_)
                    | Pattern::Carrier { .. } => {
                        unreachable!("a binding target holds Ident, Wildcard or Tuple only")
                    }
                },
                // A send arm binds nothing — `take_wait_send_arm` pushes no value, so no prologue.
                WaitArmKind::Send { .. } => {}
            }
            self.compile_defer_scoped_arm(fc, &arm.body)?;
            fc.end_scope();
            end_jumps.push(fc.emit_jump(Op::Jump(0), arm.span));
        }
        let else_target = if let Some(b) = else_block {
            let t = fc.here();
            self.compile_defer_scoped_block(fc, b)?;
            end_jumps.push(fc.emit_jump(Op::Jump(0), span));
            Some(t)
        } else {
            None
        };
        let end = fc.here();
        for j in end_jumps {
            fc.patch_jump_to(j, end);
        }
        fc.set_code(
            poll_at,
            Op::WaitPoll(Box::new(WaitMeta {
                n: arms.len(),
                arm_targets,
                else_target,
                is_send,
            })),
        );
        Ok(())
    }

    /// Store the value on the stack top into an existing lvalue, for a plain `=` and for a `wait`
    /// `=` arm. `Ident` pops straight into the binding; `Field`/`Index` stash the value in a
    /// hidden temp, evaluate the object (and index), then reload it — so the `[obj, (index,)
    /// value]` order `SetField`/`SetIndex` expect is reconstructed even though the value was
    /// produced FIRST.
    pub(super) fn emit_assign_value_first(
        &mut self,
        fc: &mut FnComp,
        target: &Expr,
        span: Span,
    ) -> Result<(), CompileError> {
        match &target.kind {
            ExprKind::Ident(name) => self.emit_store(fc, name, span),
            ExprKind::Field { obj, name, .. } => {
                let tmp = fc.add_hidden();
                fc.emit_hidden_set(tmp, span);
                self.compile_expr(fc, obj)?;
                fc.emit_hidden_get(tmp, span);
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
                let tmp = fc.add_hidden();
                fc.emit_hidden_set(tmp, span);
                self.compile_expr(fc, obj)?;
                self.compile_expr(fc, index)?;
                fc.emit_hidden_get(tmp, span);
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
            | ExprKind::Tuple(_)
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
                return Err(CompileError {
                    message: "invalid assignment target".to_string(),
                    span,
                });
            }
        }
        Ok(())
    }

    /// `parallel:` — open a nursery, run the body (spawns register tasks; inline statements run
    /// immediately), join at the dedent, THEN flush the block's defers. The defer scope brackets the
    /// body but its closing `LeaveDeferScope` lands AFTER `JoinNursery`, so a `defer` inside the block
    /// runs after its spawned children join — same order as the implicit function-body nursery (whose
    /// `do_return` joins before running body defers). Emit sequence: EnterNursery, [EnterDeferScope],
    /// body, JoinNursery, [LeaveDeferScope]. We inline the bracketing (rather than call
    /// `compile_defer_scoped_block`) because there is no seam to slip `JoinNursery` between the
    /// helper's paired defer-scope ops. Defer-free blocks are byte-identical (the `has_defer` gate
    /// skips both defer-scope ops, exactly as the helper does).
    fn compile_parallel(
        &mut self,
        fc: &mut FnComp,
        body: &[Stmt],
        span: Span,
    ) -> Result<(), CompileError> {
        fc.emit(Op::EnterNursery, span);
        // TASK B — track the open nursery scope so a `break`/`continue` inside `body` knows to emit a
        // `ReclaimNursery` (silent cancel) before its loop-exit jump. Mirrors `defer_scopes`.
        fc.nursery_scopes += 1;
        fc.nursery_defer_marks.push(fc.defer_scopes);
        let has_defer = block_has_defer(body);
        if has_defer {
            fc.emit(Op::EnterDeferScope, body[0].span);
            fc.defer_scopes += 1;
        }
        // The counter bracketing must wrap this body compile exactly as before (so
        // `emit_loop_escape_drain` emits the right count on a break/continue out of the block); only
        // the fall-through JoinNursery/LeaveDeferScope order changes.
        self.compile_block_scoped(fc, body)?;
        fc.nursery_defer_marks.pop();
        fc.nursery_scopes -= 1;
        fc.emit(Op::JoinNursery, span);
        if has_defer {
            fc.emit(Op::LeaveDeferScope, body[body.len() - 1].span);
            fc.defer_scopes -= 1;
        }
        Ok(())
    }

    /// TICKET-235 — the entry thunk of one `spawn <call>` site: a proto that takes the head and its
    /// `k` operands as parameters and replays the call (`Op::CallMethod` for `Some(method)`,
    /// `Op::Call` otherwise). The operands are evaluated in the parent, by the spawn site; the thunk
    /// only gives the call a frame to run and park in. `Vm::task_entry` decides whether a task
    /// enters through it.
    pub(super) fn spawn_entry_proto(
        &mut self,
        name: &str,
        method: Option<&str>,
        k: usize,
        span: Span,
    ) -> ProtoId {
        let mut child = FnComp::new(name.to_string(), k + 1, false);
        for i in 0..=k {
            child.add_local(format!("$t{i}"));
        }
        for i in 0..=k {
            child.emit(Op::GetLocal(i), span);
        }
        match method {
            Some(name) => {
                let ic = self.next_method_ic();
                child.emit(
                    Op::CallMethod {
                        name: name.to_string(),
                        argc: k,
                        ic,
                    },
                    span,
                );
            }
            None => child.emit(Op::Call(k), span),
        }
        child.emit(Op::Return, span);
        self.finish(child)
    }

    /// `spawn` — register a task on the innermost nursery. Form 1 (`spawn f(args)` / `spawn
    /// recv.m(args)`) evaluates the callee/receiver + args here, in the parent, and emits
    /// `SpawnCall`/`SpawnRecv` carrying the site's entry thunk ([`Self::spawn_entry_proto`]): a
    /// head that pushes no frame by itself (a receiver, a native fn value) runs as argument 0 of
    /// that thunk, so every task starts in a frame (TICKET-235). Form 2 (`spawn:` block) compiles
    /// the block as a synthetic zero-arg proto and emits `SpawnBlock`, capturing the enclosing
    /// bindings (like a closure).
    fn compile_spawn(
        &mut self,
        fc: &mut FnComp,
        target: &SpawnTarget,
        span: Span,
    ) -> Result<(), CompileError> {
        match target {
            SpawnTarget::Call(call) => {
                let ExprKind::Call {
                    callee,
                    args,
                    named,
                    bracket,
                    ..
                } = &call.kind
                else {
                    return Err(CompileError {
                        message: "spawn requires a function or method call".to_string(),
                        span,
                    });
                };
                // `spawn fs[k](10)`: the indexed element is the spawned value call.
                if self.compile_index_call_head(
                    fc,
                    call.id,
                    callee,
                    bracket.as_deref(),
                    call.span,
                )? {
                    let n = self.compile_call_args(fc, call.id, callee, args, named, call.span)?;
                    let entry = self.spawn_entry_proto("<spawned task>", None, n, call.span);
                    fc.emit(
                        Op::SpawnCall(n, self.crossing_mask(call.id), Some(entry)),
                        call.span,
                    );
                    return Ok(());
                }
                // M24-5b — `spawn Type.m(..)`: no receiver value to hold, so it rides the eager-args
                // wrapper instead of `Op::SpawnRecv`.
                if self.receiverless_call_head(callee)? {
                    let n = self.compile_receiverless_target(
                        fc,
                        (call.id, callee, args, named),
                        call.span,
                    )?;
                    fc.emit(
                        Op::SpawnCall(n, self.crossing_mask(call.id), None),
                        call.span,
                    );
                    return Ok(());
                }
                if let ExprKind::Field {
                    obj,
                    name,
                    name_span,
                } = &callee.kind
                    && !crate::ast::is_tuple_index(name)
                {
                    // Same hidden `0.0` seed the eager `Op::CallMethod` emit pushes: the entry
                    // thunk's `Op::CallMethod` consumes it, so a missing seed changes an empty
                    // `List[float].sum()`'s answer. The seed is a plain scalar, so it crosses
                    // `do_spawn`'s `deep_clone_all` airlock exactly like any other spawned
                    // argument.
                    if let Some(seed) = self.sum_seed(name, args, *name_span) {
                        self.compile_expr(fc, obj)?;
                        self.emit_sum_seed(fc, &seed, call.span);
                        let fresh = self.crossing_mask(call.id);
                        let entry =
                            self.spawn_entry_proto("<spawned task>", Some(name), 1, call.span);
                        fc.emit(Op::SpawnRecv(1, fresh, entry), call.span);
                        return Ok(());
                    }
                    self.compile_expr(fc, obj)?;
                    let n = self.compile_call_args(fc, call.id, callee, args, named, call.span)?;
                    // M24-5: the hidden witness arguments ride LAST, exactly as they do on the eager
                    // `Op::CallMethod`, so the widened `argc` reaches the same proto.
                    let w =
                        self.emit_member_witness_args(fc, callee, name, *name_span, call.span)?;
                    let fresh = self.crossing_mask(call.id);
                    let entry =
                        self.spawn_entry_proto("<spawned task>", Some(name), n + w, call.span);
                    fc.emit(Op::SpawnRecv(n + w, fresh, entry), call.span);
                } else {
                    // A spawned value call: its arguments -- default fills included -- are
                    // evaluated here, at the statement, in the checker's slot plan when it bound
                    // the call.
                    self.compile_expr(fc, callee)?;
                    let n = self.compile_call_args(fc, call.id, callee, args, named, call.span)?;
                    // M24-5: TRAILING — after the arguments, never in source order.
                    let w = self.emit_indirect_witness_args(fc, callee, call.span)?;
                    let fresh = self.crossing_mask(call.id);
                    let entry = self.spawn_entry_proto("<spawned task>", None, n + w, call.span);
                    fc.emit(Op::SpawnCall(n + w, fresh, Some(entry)), call.span);
                }
                Ok(())
            }
            SpawnTarget::Block(body) => {
                // Capture only the names this block references (its free-variable set); the values
                // are deep-copied across the airlock at `SpawnBlock`. The block becomes a synthetic
                // zero-arg proto whose free names resolve via `GetCaptured`. Free-variable capture
                // (Finding D) avoids dragging unused non-sendable siblings across the airlock.
                let snap = fc.snapshot_entries();
                let (kept, free) = filter_entries_free_block(&snap, body, &[]);
                let entries = self.with_witness_captures(&snap, kept, Some(&free));
                let captured_names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
                let mut child = FnComp::new("<spawned task>".to_string(), 0, false);
                // Uniform by-reference capture (Task A): the block's own boxed-name set (unwired).
                child.boxed_names = captured_names_of_body(body, &[]);
                child.captured_names = captured_names;
                // M-C: a `spawn:` block is its own function body — a nested bare `spawn` inside it
                // binds to the block's *own* implicit nursery, joined when the task returns.
                let implicit = block_has_bare_spawn(body);
                if implicit {
                    child.has_implicit_nursery = true;
                    child.emit(Op::EnterNursery, span);
                    child.nursery_scopes += 1;
                }
                // M24 Task 4: the block reaches the enclosing frame's witnesses through
                // `with_witness_captures` above — a `str` per witness, deep-copied across the
                // airlock like any captured string.
                self.compile_block_scoped(&mut child, body)?;
                if implicit {
                    child.nursery_scopes -= 1;
                }
                child.emit(Op::Nil, span);
                child.emit(Op::Return, span);
                let pid = self.finish(child);
                fc.emit(Op::SpawnBlock(pid, entries), span);
                Ok(())
            }
        }
    }
}
