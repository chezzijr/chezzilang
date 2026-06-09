//! Bytecode compiler (M5): lowers a resolved module graph (or a single `Module`) to a [`Program`]
//! of function prototypes for the stack VM. The compiler is the *only* place that knows about slots
//! — locals resolve to operand-stack slots here; everything else (globals, struct/variant names,
//! builtins) is resolved by name, matching the tree-walk interpreter's resolution order so the VM
//! reproduces its semantics exactly.
//!
//! Two passes:
//!   1. **Hoist** — register every module's struct / enum declarations into the program-global
//!      type tables (with the interpreter's "type already defined" collision), plus the built-in
//!      `Ok`/`Err`/`Some`/`None` variants.
//!   2. **Compile** — for each module emit a `<toplevel>` proto (top-level `fn`s hoisted first so
//!      forward references resolve) and one proto per `fn` / method / closure.

use crate::ast::{
    AssignOp, BinaryOp, Block, CompKind, Expr, ExprKind, FnDecl, LitPattern, MatchArm, MatchExprArm,
    Module, Pattern, Span, SpawnTarget, Stmt, StmtKind, UnaryOp,
};
use crate::resolver::ModuleGraph;
use crate::vm::op::{
    CapEntry, CapSrc, ModuleProto, Op, Program, Proto, ProtoId, StructDef, VariantDef,
};
use crate::{lexer, parser};
use std::collections::HashMap;

/// A compile-time failure (e.g. a malformed string interpolation). Carries a span so the CLI can
/// report it like any other error. Most user errors are caught earlier by the type checker; this
/// covers what only the compiler can see.
#[derive(Debug, Clone, PartialEq)]
pub struct CompileError {
    pub message: String,
    pub span: Span,
}

/// The name a builtin resolves to (mirrors `interp::builtins::is_builtin` + the special `print`).
fn is_builtin(name: &str) -> bool {
    matches!(name, "len" | "range" | "int" | "float" | "str" | "ord" | "chr" | "set")
}

/// Compile a whole resolved module graph in dependency order.
pub fn compile_graph(graph: &ModuleGraph) -> Result<Program, CompileError> {
    let mut c = Compiler::new();
    // Pass 1: hoist all type declarations across every module.
    for lm in &graph.modules {
        c.hoist_types(&lm.ast.stmts)?;
    }
    // Pass 2: compile each module's toplevel + functions.
    for (idx, lm) in graph.modules.iter().enumerate() {
        let toplevel = c.compile_module(idx, &lm.ast)?;
        c.program.modules.push(ModuleProto {
            id: lm.id.clone(),
            label: lm.label(),
            toplevel,
            imports: lm.imports.clone(),
            native: lm.native,
        });
    }
    Ok(c.program)
}

/// Compile a single in-memory module (test helper — no imports, treated as the entry).
#[cfg(test)]
pub fn compile_module_standalone(module: &Module) -> Result<Program, CompileError> {
    // Mirror the file-backed path: normalize named/default call arguments before compiling.
    let mut module = module.clone();
    crate::desugar::run_standalone(&mut module)
        .map_err(|e| CompileError { message: e.message, span: e.span })?;
    let module = &module;
    let mut c = Compiler::new();
    c.hoist_types(&module.stmts)?;
    let toplevel = c.compile_module(0, module)?;
    // A synthetic module id so the run driver has something to key the namespace cache on.
    let id = crate::resolver::ModuleId(std::path::PathBuf::from("<main>"));
    c.program.modules.push(ModuleProto {
        id,
        label: "<main>".to_string(),
        toplevel,
        imports: Vec::new(),
        native: None,
    });
    Ok(c.program)
}

struct Compiler {
    program: Program,
    /// Struct name → declared fields (with types), kept for building `json.decode` descriptors.
    struct_fields: HashMap<String, Vec<crate::ast::Field>>,
}

impl Compiler {
    fn new() -> Self {
        let mut program = Program {
            protos: Vec::new(),
            structs: HashMap::new(),
            variants: HashMap::new(),
            modules: Vec::new(),
        };
        // Built-in Result / Option variants, available without declaration.
        for (v, e, arity) in [("Ok", "Result", 1), ("Err", "Result", 1), ("Some", "Option", 1), ("None", "Option", 0)] {
            program.variants.insert(
                v.to_string(),
                VariantDef { enum_name: e.to_string(), arity },
            );
        }
        Compiler { program, struct_fields: HashMap::new() }
    }

    /// Pass 1: register struct / enum declarations into the program-global tables.
    fn hoist_types(&mut self, stmts: &[Stmt]) -> Result<(), CompileError> {
        for stmt in stmts {
            match &stmt.kind {
                StmtKind::Struct { name, fields, .. } => {
                    if self.program.structs.contains_key(name) {
                        return Err(CompileError {
                            message: format!("type '{name}' is already defined"),
                            span: stmt.span,
                        });
                    }
                    self.program.structs.insert(
                        name.clone(),
                        StructDef {
                            fields: fields.iter().map(|f| f.name.clone()).collect(),
                            methods: HashMap::new(),
                            module_idx: 0, // filled in pass 2
                        },
                    );
                    self.struct_fields.insert(name.clone(), fields.clone());
                }
                // Type-erased: type parameters are checker-only, the runtime is identical for
                // `Tree[int]` and `Tree[str]`.
                StmtKind::Enum { name, variants, .. } => {
                    for v in variants {
                        self.program.variants.insert(
                            v.name.clone(),
                            VariantDef { enum_name: name.clone(), arity: v.payload.len() },
                        );
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Pass 2: compile one module to a toplevel proto; record method protos into the type table.
    fn compile_module(&mut self, module_idx: usize, module: &Module) -> Result<ProtoId, CompileError> {
        // Compile struct methods first, recording their proto ids + this module as their home.
        for stmt in &module.stmts {
            if let StmtKind::Struct { name, methods, .. } = &stmt.kind {
                for m in methods {
                    let pid = self.compile_fn(m, false)?;
                    let def = self.program.structs.get_mut(name).expect("hoisted");
                    def.module_idx = module_idx;
                    def.methods.insert(m.name.clone(), pid);
                }
            }
        }
        // The synthetic toplevel function: top-level `fn`s are hoisted as globals before the body.
        let mut fc = FnComp::new("<toplevel>".to_string(), 0, true);
        for stmt in &module.stmts {
            if let StmtKind::Fn(decl) = &stmt.kind {
                let pid = self.compile_fn(decl, false)?;
                fc.emit(Op::MakeFunc(pid), stmt.span);
                fc.emit(Op::DefineGlobal(decl.name.clone()), stmt.span);
            }
        }
        self.compile_block_flat(&mut fc, &module.stmts)?;
        fc.emit(Op::Nil, Span { line: 1, col: 1 });
        fc.emit(Op::Return, Span { line: 1, col: 1 });
        Ok(self.finish(fc))
    }

    /// Compile a named function / method to its own proto. `params` occupy slots `0..arity`.
    fn compile_fn(&mut self, decl: &FnDecl, _is_method: bool) -> Result<ProtoId, CompileError> {
        let mut fc = FnComp::new(decl.name.clone(), decl.params.len(), false);
        for p in &decl.params {
            fc.add_local(p.name.clone());
        }
        self.compile_block_scoped(&mut fc, &decl.body)?;
        // Fall off the end → return Nil.
        fc.emit(Op::Nil, Span { line: 1, col: 1 });
        fc.emit(Op::Return, Span { line: 1, col: 1 });
        Ok(self.finish(fc))
    }

    fn finish(&mut self, fc: FnComp) -> ProtoId {
        let pid = self.program.protos.len();
        self.program.protos.push(Proto {
            name: fc.name,
            arity: fc.arity,
            n_slots: fc.max_slots,
            code: fc.code,
            lines: fc.lines,
        });
        pid
    }

    // ----- statements -----

    /// Compile statements into the current scope without opening a new one (used for the toplevel
    /// and for blocks whose scope is managed by the caller).
    fn compile_block_flat(&mut self, fc: &mut FnComp, stmts: &[Stmt]) -> Result<(), CompileError> {
        for stmt in stmts {
            self.compile_stmt(fc, stmt)?;
        }
        Ok(())
    }

    /// Compile a block in a fresh lexical scope (locals don't leak past the block).
    fn compile_block_scoped(&mut self, fc: &mut FnComp, stmts: &[Stmt]) -> Result<(), CompileError> {
        fc.begin_scope();
        self.compile_block_flat(fc, stmts)?;
        fc.end_scope();
        Ok(())
    }

    /// Compile a lexical block that is also a **defer scope**: any `defer` directly inside it runs
    /// when this block exits, not when the whole frame does. When the block statically holds a
    /// `defer` we bracket it with `EnterDeferScope`/`LeaveDeferScope`; otherwise this is exactly
    /// `compile_block_scoped` and emits nothing extra (defer-free code is byte-identical).
    fn compile_defer_scoped_block(&mut self, fc: &mut FnComp, stmts: &[Stmt]) -> Result<(), CompileError> {
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
    fn compile_defer_scoped_arm(&mut self, fc: &mut FnComp, stmts: &[Stmt]) -> Result<(), CompileError> {
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

    /// Emit one `LeaveDeferScope` per defer scope between the current point and the enclosing loop
    /// body (inclusive), draining them LIFO before a `break`/`continue` jumps away. These ops run on
    /// the jump path only; the blocks' own end-of-scope `LeaveDeferScope`s are skipped by the jump,
    /// so each marker is popped exactly once. The compiler's `defer_scopes` count is unchanged — the
    /// scopes remain lexically open and emit their natural leaves on the fall-through path.
    fn emit_loop_body_drain(&mut self, fc: &mut FnComp, span: Span) {
        let Some(floor) = fc.loops.last().map(|c| c.defer_floor) else {
            return; // no enclosing loop — the checker rejects this `break`/`continue`
        };
        for _ in 0..(fc.defer_scopes - floor) {
            fc.emit(Op::LeaveDeferScope, span);
        }
    }

    fn compile_stmt(&mut self, fc: &mut FnComp, stmt: &Stmt) -> Result<(), CompileError> {
        match &stmt.kind {
            StmtKind::Let { names, value, .. } => {
                self.compile_expr(fc, value)?;
                if names.len() > 1 {
                    // destructuring let `a, b := value`: stash the tuple in a hidden local, then for
                    // each binding load it and read element `.i` (the tuple-aware `GetField`). No new
                    // index op — `GetField("i")` on a tuple is the element access.
                    let tuple_slot = fc.add_hidden();
                    fc.emit(Op::SetLocal(tuple_slot), stmt.span);
                    for (i, name) in names.iter().enumerate() {
                        fc.emit(Op::GetLocal(tuple_slot), stmt.span);
                        fc.emit(Op::GetField(i.to_string()), stmt.span);
                        if fc.is_global_scope() {
                            fc.emit(Op::DefineGlobal(name.clone()), stmt.span);
                        } else {
                            let slot = fc.add_local(name.clone());
                            fc.emit(Op::SetLocal(slot), stmt.span);
                        }
                    }
                } else if fc.is_global_scope() {
                    fc.emit(Op::DefineGlobal(names[0].clone()), stmt.span);
                } else {
                    let slot = fc.add_local(names[0].clone());
                    fc.emit(Op::SetLocal(slot), stmt.span);
                }
                Ok(())
            }
            StmtKind::Assign { target, op, value } => self.compile_assign(fc, target, *op, value, stmt.span),
            StmtKind::Expr(expr) => {
                self.compile_expr(fc, expr)?;
                // `PopExprStmt` (not `Pop`): an unhandled `Err`/`None` from a top-level expression
                // statement exits the program (the runtime checks the frame). Use `expr.span` (not
                // `stmt.span`) so the error location matches the interpreter exactly.
                fc.emit(Op::PopExprStmt, expr.span);
                Ok(())
            }
            // Top-level fns are hoisted; struct/enum/import are no-ops at execution time. A `fn`
            // statement nested in a block defines a local function value.
            StmtKind::Fn(decl) => {
                if fc.is_global_scope() {
                    Ok(()) // already hoisted
                } else {
                    let pid = self.compile_fn(decl, false)?;
                    fc.emit(Op::MakeFunc(pid), stmt.span);
                    let slot = fc.add_local(decl.name.clone());
                    fc.emit(Op::SetLocal(slot), stmt.span);
                    Ok(())
                }
            }
            StmtKind::Struct { .. }
            | StmtKind::Enum { .. }
            | StmtKind::Protocol { .. }
            | StmtKind::TypeAlias { .. }
            | StmtKind::Import(_) => Ok(()),
            StmtKind::Return(value) => {
                match value {
                    Some(e) => self.compile_expr(fc, e)?,
                    None => fc.emit(Op::Nil, stmt.span),
                }
                fc.emit(Op::Return, stmt.span);
                Ok(())
            }
            StmtKind::Break => {
                // Drain the current iteration's loop-body defers (and any nested block defers) before
                // jumping out, so they run at the `break`, not at function return.
                self.emit_loop_body_drain(fc, stmt.span);
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
                self.emit_loop_body_drain(fc, stmt.span);
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
            StmtKind::Defer(call) => self.compile_defer(fc, call, stmt.span),
            StmtKind::If { branches, else_block } => self.compile_if(fc, branches, else_block.as_deref(), stmt.span),
            StmtKind::While { cond, body } => self.compile_while(fc, cond, body),
            StmtKind::For { vars, iter, body } => self.compile_for(fc, vars, iter, body, stmt.span),
            StmtKind::Match { scrutinee, arms } => self.compile_match(fc, scrutinee, arms, stmt.span),
            // Concurrency C4 — sequential, run-to-completion executor (mirrors the interpreter).
            StmtKind::Parallel { body } => self.compile_parallel(fc, body, stmt.span),
            StmtKind::Spawn(target) => self.compile_spawn(fc, target, stmt.span),
        }
    }

    /// `parallel:` — open a nursery, run the body (spawns register tasks; inline statements run
    /// immediately), then join at the dedent. The body is also a defer scope (mirrors the
    /// interpreter's `exec_scoped_block`), so a `defer` directly inside the block runs at the dedent.
    fn compile_parallel(&mut self, fc: &mut FnComp, body: &[Stmt], span: Span) -> Result<(), CompileError> {
        fc.emit(Op::EnterNursery, span);
        self.compile_defer_scoped_block(fc, body)?;
        fc.emit(Op::JoinNursery, span);
        Ok(())
    }

    /// `spawn` — register a task on the innermost nursery. Form 1 (`spawn f(args)` / `spawn
    /// recv.m(args)`) evaluates the callee/receiver + args here and emits `SpawnCall`/`SpawnMethod`
    /// (mirrors `compile_defer`). Form 2 (`spawn:` block) compiles the block as a synthetic zero-arg
    /// proto and emits `SpawnBlock`, capturing the enclosing bindings (like a closure).
    fn compile_spawn(&mut self, fc: &mut FnComp, target: &SpawnTarget, span: Span) -> Result<(), CompileError> {
        match target {
            SpawnTarget::Call(call) => {
                let ExprKind::Call { callee, args, .. } = &call.kind else {
                    return Err(CompileError {
                        message: "spawn requires a function or method call".to_string(),
                        span,
                    });
                };
                if let ExprKind::Field { obj, name } = &callee.kind {
                    self.compile_expr(fc, obj)?;
                    for a in args {
                        self.compile_expr(fc, a)?;
                    }
                    fc.emit(Op::SpawnMethod(name.clone(), args.len()), call.span);
                } else {
                    self.compile_expr(fc, callee)?;
                    for a in args {
                        self.compile_expr(fc, a)?;
                    }
                    fc.emit(Op::SpawnCall(args.len()), call.span);
                }
                Ok(())
            }
            SpawnTarget::Block(body) => {
                // Capture every visible enclosing binding (like a closure); the values are
                // deep-copied across the airlock at `SpawnBlock`. The block becomes a synthetic
                // zero-arg proto whose free names resolve via `GetCaptured`.
                let entries = fc.snapshot_entries();
                let captured_names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
                let mut child = FnComp::new("<spawned task>".to_string(), 0, false);
                child.captured_names = captured_names;
                self.compile_block_scoped(&mut child, body)?;
                child.emit(Op::Nil, span);
                child.emit(Op::Return, span);
                let pid = self.finish(child);
                fc.emit(Op::SpawnBlock(pid, entries), span);
                Ok(())
            }
        }
    }

    fn compile_assign(&mut self, fc: &mut FnComp, target: &Expr, op: AssignOp, value: &Expr, span: Span) -> Result<(), CompileError> {
        match &target.kind {
            ExprKind::Ident(name) => match op {
                AssignOp::Eq => {
                    self.compile_expr(fc, value)?;
                    self.emit_store(fc, name, span);
                }
                AssignOp::PlusEq | AssignOp::MinusEq => {
                    self.emit_load(fc, name, span);
                    self.compile_expr(fc, value)?;
                    fc.emit(if op == AssignOp::PlusEq { Op::Add } else { Op::Sub }, span);
                    self.emit_store(fc, name, span);
                }
            },
            // `obj.f = v` → [obj, v] SetField; compound dups `obj` to read-modify-write.
            ExprKind::Field { obj, name } => {
                self.compile_expr(fc, obj)?;
                if op != AssignOp::Eq {
                    fc.emit(Op::Dup, span);
                    fc.emit(Op::GetField(name.clone()), target.span);
                    self.compile_expr(fc, value)?;
                    fc.emit(if op == AssignOp::PlusEq { Op::Add } else { Op::Sub }, span);
                } else {
                    self.compile_expr(fc, value)?;
                }
                fc.emit(Op::SetField(name.clone()), span);
            }
            // `obj[i] = v` → [obj, i, v] SetIndex; compound dups `[obj, i]` to read-modify-write.
            ExprKind::Index { obj, index } => {
                self.compile_expr(fc, obj)?;
                self.compile_expr(fc, index)?;
                // No `AsInt`: the index may be a map key (str/bool). `GetIndex`/`SetIndex`
                // validate int-ness in their list/str arms at runtime.
                if op != AssignOp::Eq {
                    fc.emit(Op::Dup2, span);
                    fc.emit(Op::GetIndex, target.span);
                    self.compile_expr(fc, value)?;
                    fc.emit(if op == AssignOp::PlusEq { Op::Add } else { Op::Sub }, span);
                } else {
                    self.compile_expr(fc, value)?;
                }
                fc.emit(Op::SetIndex, span);
            }
            _ => return Err(CompileError { message: "invalid assignment target".to_string(), span }),
        }
        Ok(())
    }

    /// Store the value on top of the stack into an existing binding (`=`/`+=`/`-=` semantics:
    /// never creates — a global that doesn't exist is a runtime error).
    fn emit_store(&mut self, fc: &mut FnComp, name: &str, span: Span) {
        match fc.resolve_local(name) {
            Some(slot) => fc.emit(Op::SetLocal(slot), span),
            None => fc.emit(Op::SetGlobal(name.to_string()), span),
        }
    }

    /// Load a name's value (local → captured → global), mirroring the interpreter's lookup order.
    fn emit_load(&mut self, fc: &mut FnComp, name: &str, span: Span) {
        match fc.resolve_local(name) {
            Some(slot) => fc.emit(Op::GetLocal(slot), span),
            None if fc.captures(name) => fc.emit(Op::GetCaptured(name.to_string()), span),
            None => fc.emit(Op::GetGlobal(name.to_string()), span),
        }
    }

    fn compile_if(&mut self, fc: &mut FnComp, branches: &[(Expr, Block)], else_block: Option<&[Stmt]>, _span: Span) -> Result<(), CompileError> {
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

    fn compile_while(&mut self, fc: &mut FnComp, cond: &Expr, body: &[Stmt]) -> Result<(), CompileError> {
        let loop_start = fc.here();
        self.compile_expr(fc, cond)?;
        fc.emit(Op::AsBool, cond.span);
        let exit = fc.emit_jump(Op::JumpIfFalse(0), cond.span);
        fc.loops.push(LoopCtx { continue_jumps: Vec::new(), break_jumps: Vec::new(), defer_floor: fc.defer_scopes });
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

    fn compile_for(&mut self, fc: &mut FnComp, vars: &[String], iter: &Expr, body: &[Stmt], span: Span) -> Result<(), CompileError> {
        fc.begin_scope();
        fc.loops.push(LoopCtx { continue_jumps: Vec::new(), break_jumps: Vec::new(), defer_floor: fc.defer_scopes });
        if let ExprKind::Range { start, end } = &iter.kind {
            // Lazy counting loop — the range is never materialized. The checker guarantees a single
            // loop variable for a range.
            self.compile_expr(fc, end)?;
            fc.emit(Op::AsInt, end.span);
            let end_slot = fc.add_hidden();
            fc.emit(Op::SetLocal(end_slot), span);
            self.compile_expr(fc, start)?;
            fc.emit(Op::AsInt, start.span);
            let i_slot = fc.add_local(vars[0].clone());
            fc.emit(Op::SetLocal(i_slot), span);

            let loop_start = fc.here();
            fc.emit(Op::GetLocal(i_slot), span);
            fc.emit(Op::GetLocal(end_slot), span);
            fc.emit(Op::Lt, span);
            let exit = fc.emit_jump(Op::JumpIfFalse(0), span);
            self.compile_defer_scoped_block(fc, body)?;
            // `continue` must land HERE — on the increment, not the condition (re-testing without
            // advancing `i` would loop forever) and not after it (skipping the advance also hangs).
            let inc_target = fc.here();
            fc.emit(Op::GetLocal(i_slot), span);
            fc.emit(Op::ConstInt(1), span);
            fc.emit(Op::Add, span);
            fc.emit(Op::SetLocal(i_slot), span);
            fc.emit(Op::Jump(loop_start), span);
            fc.patch_jump(exit);
            self.patch_loop(fc, inc_target);
        } else if vars.len() == 1 {
            // Single loop variable. The iterand may be a sequence (list/map-keys/set/str) OR a user
            // struct implementing the iterator protocol (`next(self) -> Option[T]`). The compiler is
            // type-erased, so we branch at RUNTIME on `IsStruct`: a struct is driven LAZILY by
            // `next()` (so an infinite iterator with a `break` terminates); anything else is indexed
            // as a snapshotted list (existing behaviour).
            self.compile_expr(fc, iter)?;
            let iter_slot = fc.add_hidden();
            fc.emit(Op::SetLocal(iter_slot), span);
            fc.emit(Op::GetLocal(iter_slot), span);
            fc.emit(Op::IsStruct, span);
            let mode_slot = fc.add_hidden(); // true ⇒ struct-iterator path
            fc.emit(Op::SetLocal(mode_slot), span);
            // The loop variable, plus the seq-path bookkeeping slots (allocated unconditionally; the
            // struct path simply never touches them) and the struct-path's `next()` result slot.
            let item_slot = fc.add_local(vars[0].clone());
            let lst_slot = fc.add_hidden();
            let len_slot = fc.add_hidden();
            let idx_slot = fc.add_hidden();
            let opt_slot = fc.add_hidden();

            // Seq init (skipped on the struct path): snapshot the iterand to a list, take its length,
            // start the index at 0.
            fc.emit(Op::GetLocal(mode_slot), span);
            let skip_seq_init = fc.emit_jump(Op::JumpIfFalse(0), span); // false ⇒ run seq init
            let jump_to_head = fc.emit_jump(Op::Jump(0), span); // struct: no init, go to head
            fc.patch_jump(skip_seq_init);
            fc.emit(Op::GetLocal(iter_slot), span);
            fc.emit(Op::ListClone, iter.span);
            fc.emit(Op::SetLocal(lst_slot), span);
            fc.emit(Op::GetLocal(lst_slot), span);
            fc.emit(Op::ArrLen, span);
            fc.emit(Op::SetLocal(len_slot), span);
            fc.emit(Op::ConstInt(0), span);
            fc.emit(Op::SetLocal(idx_slot), span);
            fc.patch_jump(jump_to_head);

            let loop_head = fc.here();
            fc.emit(Op::GetLocal(mode_slot), span);
            let to_seq_step = fc.emit_jump(Op::JumpIfFalse(0), span); // false ⇒ seq step
            // ----- struct step: call next(); None ⇒ exit, Some(v) ⇒ bind v -----
            fc.emit(Op::GetLocal(iter_slot), span);
            fc.emit(Op::CallMethod("next".to_string(), 0), span);
            fc.emit(Op::SetLocal(opt_slot), span);
            fc.emit(Op::EnsureEnum(opt_slot), iter.span);
            // Test `None` first: a match falls through to the exit jump; a mismatch goes to `to_some`.
            let none_arm = fc.emit_jump(
                Op::MatchArm { scrut: opt_slot, variant: "None".to_string(), nbind: 0, bind_start: 0, next: 0 },
                iter.span,
            );
            let struct_exit = fc.emit_jump(Op::Jump(0), span); // None matched ⇒ leave the loop
            fc.patch_jump(none_arm); // not None ⇒ try Some here
            // `Some(v)`: a match binds the payload into the loop variable's slot and falls through to
            // the body jump; a non-Some jumps to the trap below.
            let some_arm = fc.emit_jump(
                Op::MatchArm { scrut: opt_slot, variant: "Some".to_string(), nbind: 1, bind_start: item_slot, next: 0 },
                iter.span,
            );
            let to_body = fc.emit_jump(Op::Jump(0), span); // Some matched ⇒ run the body
            fc.patch_jump(some_arm); // neither None nor Some ⇒ the trap
            fc.emit(Op::MatchNoArm(opt_slot), iter.span); // not Option ⇒ runtime trap
            // ----- seq step: bounds-check the index, read the element -----
            fc.patch_jump(to_seq_step);
            fc.emit(Op::GetLocal(idx_slot), span);
            fc.emit(Op::GetLocal(len_slot), span);
            fc.emit(Op::Lt, span);
            let seq_exit = fc.emit_jump(Op::JumpIfFalse(0), span);
            fc.emit(Op::GetLocal(lst_slot), span);
            fc.emit(Op::GetLocal(idx_slot), span);
            fc.emit(Op::GetIndex, span);
            fc.emit(Op::SetLocal(item_slot), span);

            fc.patch_jump(to_body);
            self.compile_defer_scoped_block(fc, body)?;
            // `continue` lands HERE — the advance step. For a struct, "advance" is just re-looping
            // (the next `next()` call); for a sequence, it's the index increment.
            let inc_target = fc.here();
            fc.emit(Op::GetLocal(mode_slot), span);
            let to_seq_inc = fc.emit_jump(Op::JumpIfFalse(0), span);
            fc.emit(Op::Jump(loop_head), span); // struct: re-loop, next() advances
            fc.patch_jump(to_seq_inc);
            fc.emit(Op::GetLocal(idx_slot), span);
            fc.emit(Op::ConstInt(1), span);
            fc.emit(Op::Add, span);
            fc.emit(Op::SetLocal(idx_slot), span);
            fc.emit(Op::Jump(loop_head), span);
            // Both exit paths land here (past the back-edge).
            fc.patch_jump(struct_exit);
            fc.patch_jump(seq_exit);
            self.patch_loop(fc, inc_target);
        } else {
            // Multi-name `for`: either `for k, v in m` over a MAP (key, value) or tuple-destructuring
            // `for a, b, … in xs` over a `list[(A, B, …)]`. The compiler is type-erased, so we branch
            // at RUNTIME on `IsMap` (mirroring the single-var `IsStruct` split):
            //   - map: snapshot keys + values up front and index them in lockstep (so a body that
            //     mutates the map mid-loop can't perturb the bindings; matches the interpreter);
            //   - list of tuples: index the list, then destructure each element tuple into the N
            //     loop vars via `GetField(j)` (the destructure-`:=` pattern, generalized to N).
            self.compile_expr(fc, iter)?;
            let src_slot = fc.add_hidden();
            fc.emit(Op::SetLocal(src_slot), span);
            fc.emit(Op::GetLocal(src_slot), span);
            fc.emit(Op::IsMap, span);
            let mode_slot = fc.add_hidden(); // true ⇒ map path
            fc.emit(Op::SetLocal(mode_slot), span);

            let lst = fc.add_hidden(); // the list we index (map keys, or the list of tuples)
            let vals = fc.add_hidden(); // map values snapshot (map path only)
            let len = fc.add_hidden();
            let idx = fc.add_hidden();
            let elem = fc.add_hidden(); // the element read at lst[idx]
            let var_slots: Vec<usize> = vars.iter().map(|v| fc.add_local(v.clone())).collect();

            // ----- init: branch map vs list -----
            fc.emit(Op::GetLocal(mode_slot), span);
            let to_list_init = fc.emit_jump(Op::JumpIfFalse(0), span); // false ⇒ list init
            // map init: keys snapshot into `lst`, values snapshot into `vals` (same instant/order)
            fc.emit(Op::GetLocal(src_slot), span);
            fc.emit(Op::ListClone, iter.span);
            fc.emit(Op::SetLocal(lst), span);
            fc.emit(Op::GetLocal(src_slot), span);
            fc.emit(Op::CallMethod("values".to_string(), 0), span);
            fc.emit(Op::SetLocal(vals), span);
            let after_init = fc.emit_jump(Op::Jump(0), span);
            // list init: clone the list of tuples into `lst`
            fc.patch_jump(to_list_init);
            fc.emit(Op::GetLocal(src_slot), span);
            fc.emit(Op::ListClone, iter.span);
            fc.emit(Op::SetLocal(lst), span);
            fc.patch_jump(after_init);
            // common: len = lst.len(), idx = 0
            fc.emit(Op::GetLocal(lst), span);
            fc.emit(Op::ArrLen, span);
            fc.emit(Op::SetLocal(len), span);
            fc.emit(Op::ConstInt(0), span);
            fc.emit(Op::SetLocal(idx), span);

            let loop_start = fc.here();
            fc.emit(Op::GetLocal(idx), span);
            fc.emit(Op::GetLocal(len), span);
            fc.emit(Op::Lt, span);
            let exit = fc.emit_jump(Op::JumpIfFalse(0), span);
            // elem = lst[idx]
            fc.emit(Op::GetLocal(lst), span);
            fc.emit(Op::GetLocal(idx), span);
            fc.emit(Op::GetIndex, span);
            fc.emit(Op::SetLocal(elem), span);
            // ----- bind: branch map vs list -----
            fc.emit(Op::GetLocal(mode_slot), span);
            let to_list_bind = fc.emit_jump(Op::JumpIfFalse(0), span);
            // map bind: var[0] = key (elem), var[1] = vals[idx]
            fc.emit(Op::GetLocal(elem), span);
            fc.emit(Op::SetLocal(var_slots[0]), span);
            fc.emit(Op::GetLocal(vals), span);
            fc.emit(Op::GetLocal(idx), span);
            fc.emit(Op::GetIndex, span);
            fc.emit(Op::SetLocal(var_slots[1]), span);
            let after_bind = fc.emit_jump(Op::Jump(0), span);
            // list bind: destructure the tuple element into each loop var (var[j] = elem.j)
            fc.patch_jump(to_list_bind);
            for (j, &vs) in var_slots.iter().enumerate() {
                fc.emit(Op::GetLocal(elem), span);
                fc.emit(Op::GetField(j.to_string()), span);
                fc.emit(Op::SetLocal(vs), span);
            }
            fc.patch_jump(after_bind);

            self.compile_defer_scoped_block(fc, body)?;
            // `continue` lands HERE — the index increment, so the loop advances instead of looping.
            let inc_target = fc.here();
            fc.emit(Op::GetLocal(idx), span);
            fc.emit(Op::ConstInt(1), span);
            fc.emit(Op::Add, span);
            fc.emit(Op::SetLocal(idx), span);
            fc.emit(Op::Jump(loop_start), span);
            fc.patch_jump(exit);
            self.patch_loop(fc, inc_target);
        }
        fc.end_scope();
        Ok(())
    }

    /// Compile a comprehension by reusing `compile_for`: seed a hidden accumulator, then run a
    /// synthesized loop body that appends the element (`push`/`add`) or inserts the key→value pair.
    /// This means comprehensions iterate every collection — and the struct-iterator protocol —
    /// exactly like a `for` loop, with no duplicated iteration logic. The finished accumulator is
    /// left on the stack as the expression's value.
    #[allow(clippy::too_many_arguments)]
    fn compile_comprehension(
        &mut self,
        fc: &mut FnComp,
        kind: CompKind,
        key: Option<&Expr>,
        elem: &Expr,
        vars: &[String],
        iter: &Expr,
        guard: Option<&Expr>,
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
        let acc_slot = fc.add_local(acc_name.clone());
        fc.emit(Op::SetLocal(acc_slot), span);

        let acc = Expr { kind: ExprKind::Ident(acc_name), span };
        let body_stmt = match kind {
            CompKind::List => method_call_stmt(acc, "push", vec![elem.clone()], span),
            CompKind::Set => method_call_stmt(acc, "add", vec![elem.clone()], span),
            CompKind::Map => {
                let key = key.expect("a map comprehension carries a key").clone();
                Stmt {
                    kind: StmtKind::Assign {
                        target: Expr {
                            kind: ExprKind::Index { obj: Box::new(acc), index: Box::new(key) },
                            span,
                        },
                        op: AssignOp::Eq,
                        value: elem.clone(),
                    },
                    span,
                }
            }
        };
        let body = match guard {
            Some(g) => vec![Stmt {
                kind: StmtKind::If {
                    branches: vec![(g.clone(), vec![body_stmt])],
                    else_block: None,
                },
                span,
            }],
            None => vec![body_stmt],
        };

        self.compile_for(fc, vars, iter, &body, span)?;
        // The comprehension's value is the finished accumulator.
        fc.emit(Op::GetLocal(acc_slot), span);
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

    /// True if these arms form a literal/range/wildcard/binding match (no enum or tuple patterns), so
    /// it takes the lighter `compile_match_lit` path (no `EnsureEnum`, no `MatchNoArm`). A bare
    /// top-level identifier is a *binding* unless it names a known enum variant — that distinction is
    /// type-free thanks to the program-global variant registry, mirroring the runtime.
    fn arms_are_literal<'a>(&self, patterns: impl Iterator<Item = &'a Pattern>) -> bool {
        patterns.into_iter().all(|p| match p {
            Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard => true,
            // empty-binding `Name`: a binding unless it's a real variant.
            Pattern::Variant { name, bindings } if bindings.is_empty() => {
                !self.program.variants.contains_key(name)
            }
            _ => false,
        })
    }

    fn compile_match(&mut self, fc: &mut FnComp, scrutinee: &Expr, arms: &[MatchArm], span: Span) -> Result<(), CompileError> {
        if self.arms_are_literal(arms.iter().map(|a| &a.pattern)) {
            return self.compile_match_lit(fc, scrutinee, arms, span, |s, fc, body| {
                s.compile_defer_scoped_arm(fc, body)
            });
        }
        self.compile_match_general(fc, scrutinee, arms, span, |s, fc, body| {
            s.compile_defer_scoped_arm(fc, body)
        })
    }

    /// Lower a `match` whose arms are variant and/or tuple patterns, with arbitrary nesting (gap
    /// #15). Each arm tests its pattern against the scrutinee (via `emit_pattern`); on a mismatch it
    /// jumps to the next arm, on a match it binds and runs the body. Serves both the statement and
    /// expression forms via the `run_body` closure.
    fn compile_match_general<A, F>(
        &mut self,
        fc: &mut FnComp,
        scrutinee: &Expr,
        arms: &[A],
        span: Span,
        mut run_body: F,
    ) -> Result<(), CompileError>
    where
        A: MatchArmLike,
        F: FnMut(&mut Self, &mut FnComp, &A::Body) -> Result<(), CompileError>,
    {
        fc.begin_scope();
        self.compile_expr(fc, scrutinee)?;
        let scrut = fc.add_hidden();
        fc.emit(Op::SetLocal(scrut), span);
        // Variant matches keep the `EnsureEnum` guard so a non-enum scrutinee (possible only when
        // the checker couldn't infer the type) is a clean runtime error, not a panic. Tuple matches
        // need no such guard.
        if arms.iter().any(|a| matches!(a.pattern(), Pattern::Variant { .. })) {
            fc.emit(Op::EnsureEnum(scrut), scrutinee.span);
        }
        let mut end_jumps = Vec::new();
        for arm in arms {
            fc.begin_scope();
            let mut fails = Vec::new();
            self.emit_pattern(fc, arm.pattern(), scrut, &mut fails, scrutinee.span)?;
            // The guard runs with the pattern's bindings in scope; a false guard joins `fails` and
            // falls through to the next arm.
            self.emit_guard(fc, arm.guard(), &mut fails)?;
            run_body(self, fc, arm.body())?;
            end_jumps.push(fc.emit_jump(Op::Jump(0), span));
            // Any failed test in this arm jumps here — the start of the next arm.
            let next = fc.here();
            for j in fails {
                fc.patch_jump_to(j, next);
            }
            fc.end_scope();
        }
        fc.emit(Op::MatchNoArm(scrut), scrutinee.span); // exhaustive (checker) → a trap
        for j in end_jumps {
            fc.patch_jump(j);
        }
        fc.end_scope();
        Ok(())
    }

    /// Emit an optional arm guard `if <expr>`: compile the bool expr and, on `false`, jump to the
    /// next arm (the jump is pushed onto `fails`, which the caller patches). A `None` guard emits
    /// nothing.
    fn emit_guard(&mut self, fc: &mut FnComp, guard: Option<&Expr>, fails: &mut Vec<usize>) -> Result<(), CompileError> {
        if let Some(g) = guard {
            self.compile_expr(fc, g)?;
            fails.push(fc.emit_jump(Op::JumpIfFalse(0), g.span));
        }
        Ok(())
    }

    /// Emit code testing the value in local `scrut` against `pattern`. Failed tests push their jump
    /// onto `fails` (the caller patches them to the next arm); successful matches bind every name in
    /// the pattern to a fresh local in the current scope. Recurses for nested tuple/variant
    /// patterns. No new opcodes — reuses `MatchArm` (variant), `GetField` (tuple element), and
    /// `Eq`+`JumpIfFalse` (literal).
    fn emit_pattern(&mut self, fc: &mut FnComp, pattern: &Pattern, scrut: usize, fails: &mut Vec<usize>, span: Span) -> Result<(), CompileError> {
        match pattern {
            Pattern::Wildcard => {}
            Pattern::Ident(name) => {
                let s = fc.add_local(name.clone());
                fc.emit(Op::GetLocal(scrut), span);
                fc.emit(Op::SetLocal(s), span);
            }
            Pattern::Literal(lit) => {
                fc.emit(Op::GetLocal(scrut), span);
                emit_lit_const(fc, lit, span);
                fc.emit(Op::Eq, span);
                fails.push(fc.emit_jump(Op::JumpIfFalse(0), span));
            }
            Pattern::Range { start, end } => {
                emit_range_test(fc, scrut, *start, *end, fails, span);
            }
            Pattern::Tuple(subs) => {
                for (i, sub) in subs.iter().enumerate() {
                    fc.emit(Op::GetLocal(scrut), span);
                    fc.emit(Op::GetField(i.to_string()), span); // tuple element `.i`
                    let elem = fc.add_hidden();
                    fc.emit(Op::SetLocal(elem), span);
                    self.emit_pattern(fc, sub, elem, fails, span)?;
                }
            }
            Pattern::Variant { name, bindings } => {
                // One slot per payload element. A plain `Ident` binding names its slot directly (so
                // `Some(c)` binds `c` with no copy); other sub-patterns get a hidden slot to
                // destructure afterwards.
                let bind_start = fc.next_slot();
                for b in bindings {
                    match b {
                        Pattern::Ident(n) => {
                            fc.add_local(n.clone());
                        }
                        _ => {
                            fc.add_hidden();
                        }
                    }
                }
                let arm_op = fc.emit_jump(
                    Op::MatchArm { scrut, variant: name.clone(), nbind: bindings.len(), bind_start, next: 0 },
                    span,
                );
                fails.push(arm_op);
                for (i, b) in bindings.iter().enumerate() {
                    if !matches!(b, Pattern::Ident(_)) {
                        self.emit_pattern(fc, b, bind_start + i, fails, span)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Lower a literal/wildcard `match` (no `EnsureEnum` — a literal `match` on an int must not
    /// raise "cannot match on int"). Each literal arm does `scrut == literal`, jumping to the next
    /// arm on a miss; the (checker-guaranteed) `_` wildcard is the unconditional fallback. The
    /// `run_body` closure compiles an arm body — a statement block or a value-expression — so this
    /// serves both the statement and expression forms.
    fn compile_match_lit<A, F>(
        &mut self,
        fc: &mut FnComp,
        scrutinee: &Expr,
        arms: &[A],
        span: Span,
        mut run_body: F,
    ) -> Result<(), CompileError>
    where
        A: MatchArmLike,
        F: FnMut(&mut Self, &mut FnComp, &A::Body) -> Result<(), CompileError>,
    {
        fc.begin_scope();
        self.compile_expr(fc, scrutinee)?;
        let scrut = fc.add_hidden();
        fc.emit(Op::SetLocal(scrut), span);
        let mut end_jumps = Vec::new();
        for arm in arms {
            match arm.pattern() {
                Pattern::Literal(lit) => {
                    fc.begin_scope();
                    fc.emit(Op::GetLocal(scrut), scrutinee.span);
                    emit_lit_const(fc, lit, span);
                    fc.emit(Op::Eq, span);
                    let mut fails = vec![fc.emit_jump(Op::JumpIfFalse(0), span)];
                    self.emit_guard(fc, arm.guard(), &mut fails)?;
                    run_body(self, fc, arm.body())?;
                    end_jumps.push(fc.emit_jump(Op::Jump(0), span));
                    let next = fc.here();
                    for j in fails {
                        fc.patch_jump_to(j, next);
                    }
                    fc.end_scope();
                }
                Pattern::Range { start, end } => {
                    fc.begin_scope();
                    let mut fails = Vec::new();
                    emit_range_test(fc, scrut, *start, *end, &mut fails, span);
                    self.emit_guard(fc, arm.guard(), &mut fails)?;
                    run_body(self, fc, arm.body())?;
                    end_jumps.push(fc.emit_jump(Op::Jump(0), span));
                    let next = fc.here();
                    for j in fails {
                        fc.patch_jump_to(j, next);
                    }
                    fc.end_scope();
                }
                Pattern::Wildcard => {
                    // A bare `_` is the unconditional fallback (the checker guarantees exactly one
                    // covers every reachable path). A *guarded* `_ if g:` is refutable: its guard may
                    // fail, so it tests the guard and falls through to the next arm on a miss.
                    fc.begin_scope();
                    let mut fails = Vec::new();
                    self.emit_guard(fc, arm.guard(), &mut fails)?;
                    run_body(self, fc, arm.body())?;
                    end_jumps.push(fc.emit_jump(Op::Jump(0), span));
                    let next = fc.here();
                    for j in fails {
                        fc.patch_jump_to(j, next);
                    }
                    fc.end_scope();
                }
                // A bare top-level identifier that isn't a known variant is a binding catch-all
                // capturing the whole scrutinee (irrefutable, like `_` but named).
                Pattern::Variant { name, bindings } if bindings.is_empty() => {
                    fc.begin_scope();
                    let s = fc.add_local(name.clone());
                    fc.emit(Op::GetLocal(scrut), span);
                    fc.emit(Op::SetLocal(s), span);
                    let mut fails = Vec::new();
                    self.emit_guard(fc, arm.guard(), &mut fails)?;
                    run_body(self, fc, arm.body())?;
                    end_jumps.push(fc.emit_jump(Op::Jump(0), span));
                    let next = fc.here();
                    for j in fails {
                        fc.patch_jump_to(j, next);
                    }
                    fc.end_scope();
                }
                Pattern::Variant { .. } | Pattern::Tuple(_) | Pattern::Ident(_) => {
                    unreachable!("literal match has only literal/range/wildcard/binding arms (arms_are_literal)")
                }
            }
        }
        for j in end_jumps {
            fc.patch_jump(j);
        }
        fc.end_scope();
        Ok(())
    }

    // ----- expressions -----

    fn compile_expr(&mut self, fc: &mut FnComp, expr: &Expr) -> Result<(), CompileError> {
        match &expr.kind {
            ExprKind::Int(n) => fc.emit(Op::ConstInt(*n), expr.span),
            ExprKind::Float(x) => fc.emit(Op::ConstFloat(*x), expr.span),
            ExprKind::Bool(b) => fc.emit(if *b { Op::True } else { Op::False }, expr.span),
            ExprKind::Str(raw) => self.compile_str(fc, raw, expr.span)?,
            ExprKind::Ident(name) => self.compile_ident(fc, name, expr.span),
            ExprKind::List(items) => {
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
                for e in elems {
                    self.compile_expr(fc, e)?;
                }
                fc.emit(Op::NewSet(elems.len()), expr.span);
            }
            ExprKind::Comprehension { kind, key, elem, vars, iter, guard } => self
                .compile_comprehension(
                    fc,
                    *kind,
                    key.as_deref(),
                    elem,
                    vars,
                    iter,
                    guard.as_deref(),
                    expr.span,
                )?,
            ExprKind::Unary { op, expr: inner } => {
                self.compile_expr(fc, inner)?;
                match op {
                    UnaryOp::Neg => fc.emit(Op::Neg, expr.span),
                    UnaryOp::Not => {
                        fc.emit(Op::AsBool, inner.span);
                        fc.emit(Op::Not, expr.span);
                    }
                }
            }
            ExprKind::Binary { op: op @ (BinaryOp::And | BinaryOp::Or), lhs, rhs } => {
                // Short-circuit: lhs is always bool-checked; rhs only when needed; result is bool.
                self.compile_expr(fc, lhs)?;
                fc.emit(Op::AsBool, lhs.span);
                let jump = match op {
                    BinaryOp::And => fc.emit_jump(Op::JumpIfFalseKeep(0), expr.span),
                    _ => fc.emit_jump(Op::JumpIfTrueKeep(0), expr.span),
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
            ExprKind::Range { .. } => {
                // The interpreter only evaluates ranges inside `for`; a bare range has no value.
                return Err(CompileError {
                    message: "a range can only be used as the iterable of a `for` loop".to_string(),
                    span: expr.span,
                });
            }
            // `type_args` are type-erased — the compiler never sees them (checker already used them).
            ExprKind::Call { callee, args, .. } => self.compile_call(fc, callee, args, expr.span)?,
            ExprKind::Field { obj, name } => {
                self.compile_expr(fc, obj)?;
                fc.emit(Op::GetField(name.clone()), expr.span);
            }
            ExprKind::Index { obj, index } => {
                self.compile_expr(fc, obj)?;
                self.compile_expr(fc, index)?;
                // No `AsInt`: the index may be a map key (str/bool), not just a list/str int.
                // `GetIndex` validates int-ness in its list/str arm at runtime.
                fc.emit(Op::GetIndex, expr.span);
            }
            ExprKind::Slice { obj, start, end } => {
                self.compile_expr(fc, obj)?;
                self.compile_expr(fc, start)?;
                self.compile_expr(fc, end)?;
                fc.emit(Op::GetSlice, expr.span);
            }
            ExprKind::Try(inner) => {
                self.compile_expr(fc, inner)?;
                fc.emit(Op::Try, expr.span);
            }
            // `?.`/`??` carriers are lowered to `match` by the desugar pass before compilation.
            ExprKind::OptChain { .. } | ExprKind::NullCoalesce { .. } => {
                unreachable!("`?.`/`??` must be lowered by the desugar pass before compiling")
            }
            ExprKind::DecodeCall { obj, ty, arg } => {
                // Reuse the module's own `parse` (`obj.parse(arg)` → Result[Json]), then coerce the
                // parsed value into the target type with a descriptor built from `ty`.
                let parse_call = Expr {
                    kind: ExprKind::Call {
                        callee: Box::new(Expr {
                            kind: ExprKind::Field { obj: obj.clone(), name: "parse".to_string() },
                            span: expr.span,
                        }),
                        args: vec![(**arg).clone()],
                        named: Vec::new(),
                        type_args: Vec::new(),
                    },
                    span: expr.span,
                };
                self.compile_expr(fc, &parse_call)?;
                let desc = crate::json_decode::from_type(ty, &self.struct_fields, &mut Vec::new())
                    .map_err(|message| CompileError { message, span: expr.span })?;
                fc.emit(Op::JsonDecode(desc), expr.span);
            }
            ExprKind::Closure { params, body, .. } => self.compile_closure(fc, params, body, expr.span)?,
            ExprKind::Match { scrutinee, arms } => self.compile_match_expr(fc, scrutinee, arms, expr.span)?,
            ExprKind::IfElse { cond, then, els } => self.compile_if_expr(fc, cond, then, els)?,
            ExprKind::Recover(block) => self.compile_recover(fc, block, expr.span)?,
        }
        Ok(())
    }

    /// `recover: <block>` — install a handler over the block; on the happy path wrap the block's
    /// trailing-expression value in `Ok`, on a caught fault the VM has pushed the message `str` and
    /// we wrap it in `Err`. Both paths leave exactly one `Result` value on the stack.
    fn compile_recover(&mut self, fc: &mut FnComp, block: &[Stmt], span: Span) -> Result<(), CompileError> {
        // The handler target is `done`. Three paths converge there, each leaving one `Result`:
        //   • normal: wrap the trailing value in `Ok`, drop the handler, fall through;
        //   • panic: the VM unwinds, pushes `Err(message)`, and jumps to `done`;
        //   • `?` Err/None in the block: `do_try` pushes the propagated value and jumps to `done`.
        let push = fc.emit_jump(Op::PushHandler(0), span);
        fc.begin_scope();
        if let Some((last, init)) = block.split_last() {
            for stmt in init {
                self.compile_stmt(fc, stmt)?;
            }
            match &last.kind {
                StmtKind::Expr(e) => self.compile_expr(fc, e)?, // trailing value
                _ => {
                    self.compile_stmt(fc, last)?;
                    fc.emit(Op::Nil, span);
                }
            }
        } else {
            fc.emit(Op::Nil, span);
        }
        fc.end_scope();
        // The recover block is a defer scope: drain its own defers at the boundary (Ok path), before
        // wrapping the value. The fault and `?` paths drain via the handler in the VM. Only emit when
        // the block directly holds a `defer`, keeping defer-free recovers byte-identical.
        if block_has_defer(block) {
            fc.emit(Op::DrainHandlerDefers, span);
        }
        fc.emit(Op::NewEnum("Result".to_string(), "Ok".to_string(), 1), span);
        fc.emit(Op::PopHandler, span);
        let done = fc.here();
        fc.patch_jump_to(push, done);
        Ok(())
    }

    /// Expression-position `match`: like `compile_match`, but each arm body is compiled as an
    /// expression that leaves its value on the stack, so the whole `match` yields one value.
    fn compile_match_expr(&mut self, fc: &mut FnComp, scrutinee: &Expr, arms: &[MatchExprArm], span: Span) -> Result<(), CompileError> {
        if self.arms_are_literal(arms.iter().map(|a| &a.pattern)) {
            return self.compile_match_lit(fc, scrutinee, arms, span, |s, fc, body| {
                s.compile_expr(fc, body) // leaves the arm's value on the stack
            });
        }
        self.compile_match_general(fc, scrutinee, arms, span, |s, fc, body| {
            s.compile_expr(fc, body) // leaves the arm's value on the stack
        })
    }

    /// Expression-position `if c: a else: b`: condition, then jump to whichever branch; both
    /// branches leave exactly one value, so one value remains at the join.
    fn compile_if_expr(&mut self, fc: &mut FnComp, cond: &Expr, then: &Expr, els: &Expr) -> Result<(), CompileError> {
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

    fn compile_ident(&mut self, fc: &mut FnComp, name: &str, span: Span) {
        // A nullary enum variant used as a value (e.g. `None`, `Red`) — resolved before any
        // env lookup, exactly like the interpreter.
        if let Some(def) = self.program.variants.get(name)
            && def.arity == 0
        {
            let ty = def.enum_name.clone();
            fc.emit(Op::NewEnum(ty, name.to_string(), 0), span);
            return;
        }
        self.emit_load(fc, name, span);
    }

    /// `defer <call>` — evaluate the receiver/args now (Go semantics) and register a deferred call
    /// on the frame; the call runs LIFO when the frame exits. Mirrors `compile_call`'s method-vs-value
    /// split: `DeferMethod` for `obj.m(a)`, `DeferCall` for a value callee.
    fn compile_defer(&mut self, fc: &mut FnComp, call: &Expr, span: Span) -> Result<(), CompileError> {
        let ExprKind::Call { callee, args, .. } = &call.kind else {
            return Err(CompileError {
                message: "defer requires a function or method call".to_string(),
                span,
            });
        };
        if let ExprKind::Field { obj, name } = &callee.kind {
            self.compile_expr(fc, obj)?;
            for a in args {
                self.compile_expr(fc, a)?;
            }
            fc.emit(Op::DeferMethod(name.clone(), args.len()), call.span);
            return Ok(());
        }
        self.compile_expr(fc, callee)?;
        for a in args {
            self.compile_expr(fc, a)?;
        }
        fc.emit(Op::DeferCall(args.len()), call.span);
        Ok(())
    }

    fn compile_call(&mut self, fc: &mut FnComp, callee: &Expr, args: &[Expr], span: Span) -> Result<(), CompileError> {
        // Method / module-member call: `obj.name(args)`.
        if let ExprKind::Field { obj, name } = &callee.kind {
            self.compile_expr(fc, obj)?;
            for a in args {
                self.compile_expr(fc, a)?;
            }
            fc.emit(Op::CallMethod(name.clone(), args.len()), span);
            return Ok(());
        }
        // Bare-ident callees resolve by name in the interpreter's order:
        // print → builtin → struct ctor → variant ctor → value.
        if let ExprKind::Ident(name) = &callee.kind {
            // Concurrency C4: `Channel[T]()` → a fresh mailbox; `Shared(v)` → a fresh box over the
            // deep-copied init value. The checker validated arity (Channel: 0 args, Shared: 1).
            if name == "Channel" {
                fc.emit(Op::NewChannel, span);
                return Ok(());
            }
            if name == "Shared" {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(Op::NewShared, span);
                return Ok(());
            }
            // C5: `Executor()` → a fresh work queue (checker validated 0 args).
            if name == "Executor" {
                fc.emit(Op::NewExecutor, span);
                return Ok(());
            }
            if name == "print" {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(Op::CallPrint(args.len()), span);
                return Ok(());
            }
            if is_builtin(name) {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(Op::CallBuiltin(name.clone(), args.len()), span);
                return Ok(());
            }
            if self.program.structs.contains_key(name) {
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(Op::NewStruct(name.clone(), args.len()), span);
                return Ok(());
            }
            if let Some(def) = self.program.variants.get(name) {
                let ty = def.enum_name.clone();
                for a in args {
                    self.compile_expr(fc, a)?;
                }
                fc.emit(Op::NewEnum(ty, name.clone(), args.len()), span);
                return Ok(());
            }
        }
        // General callable value.
        self.compile_expr(fc, callee)?;
        for a in args {
            self.compile_expr(fc, a)?;
        }
        fc.emit(Op::Call(args.len()), span);
        Ok(())
    }

    fn compile_closure(&mut self, fc: &mut FnComp, params: &[crate::ast::Param], body: &Expr, span: Span) -> Result<(), CompileError> {
        // Snapshot every binding currently visible in the enclosing frame (matches the interpreter
        // capturing all in-scope local frames).
        let entries = fc.snapshot_entries();
        let captured_names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();

        let mut child = FnComp::new("<closure>".to_string(), params.len(), false);
        child.captured_names = captured_names;
        for p in params {
            child.add_local(p.name.clone());
        }
        self.compile_expr(&mut child, body)?;
        child.emit(Op::Return, span);
        let pid = self.finish(child);

        fc.emit(Op::MakeClosure(pid, entries), span);
        Ok(())
    }

    /// Compile a string literal, splitting `{expr}` interpolations (pre-parsed at compile time)
    /// from literal text. `{{`/`}}` are literal braces. A literal-only string is a single
    /// `ConstStr`; an interpolated one builds its chunks and concatenates with `BuildStr`.
    fn compile_str(&mut self, fc: &mut FnComp, raw: &str, span: Span) -> Result<(), CompileError> {
        let chunks = parse_interpolation(raw, span)?;
        if let [Chunk::Lit(s)] = chunks.as_slice() {
            fc.emit(Op::ConstStr(s.clone()), span);
            return Ok(());
        }
        if chunks.is_empty() {
            fc.emit(Op::ConstStr(String::new()), span);
            return Ok(());
        }
        let n = chunks.len();
        for chunk in chunks {
            match chunk {
                Chunk::Lit(s) => fc.emit(Op::ConstStr(s), span),
                Chunk::Expr(e) => {
                    self.compile_expr(fc, &e)?;
                    fc.emit(Op::ToStr, span);
                }
            }
        }
        fc.emit(Op::BuildStr(n), span);
        Ok(())
    }
}

/// Build a synthesized `obj.method(args)` expression-statement (used to desugar comprehension
/// accumulation into a method call the existing codegen already handles).
fn method_call_stmt(obj: Expr, method: &str, args: Vec<Expr>, span: Span) -> Stmt {
    let callee = Expr {
        kind: ExprKind::Field { obj: Box::new(obj), name: method.to_string() },
        span,
    };
    Stmt {
        kind: StmtKind::Expr(Expr {
            kind: ExprKind::Call {
                callee: Box::new(callee),
                args,
                named: Vec::new(),
                type_args: Vec::new(),
            },
            span,
        }),
        span,
    }
}

fn binary_op(op: BinaryOp) -> Op {
    match op {
        BinaryOp::Add => Op::Add,
        BinaryOp::Sub => Op::Sub,
        BinaryOp::Mul => Op::Mul,
        BinaryOp::Div => Op::Div,
        BinaryOp::Mod => Op::Mod,
        BinaryOp::Lt => Op::Lt,
        BinaryOp::LtEq => Op::LtEq,
        BinaryOp::Gt => Op::Gt,
        BinaryOp::GtEq => Op::GtEq,
        BinaryOp::Eq => Op::Eq,
        BinaryOp::NotEq => Op::NotEq,
        BinaryOp::BitAnd => Op::BitAnd,
        BinaryOp::BitOr => Op::BitOr,
        BinaryOp::BitXor => Op::BitXor,
        BinaryOp::Shl => Op::Shl,
        BinaryOp::Shr => Op::Shr,
        BinaryOp::And | BinaryOp::Or => unreachable!("and/or handled by short-circuit path"),
    }
}

// ===== match lowering helpers =====

/// A `match` arm uniform over the statement form (`MatchArm`) and expression form (`MatchExprArm`),
/// so `compile_match_lit` can drive both.
trait MatchArmLike {
    type Body;
    fn pattern(&self) -> &Pattern;
    fn guard(&self) -> Option<&Expr>;
    fn body(&self) -> &Self::Body;
}

impl MatchArmLike for MatchArm {
    type Body = Block;
    fn pattern(&self) -> &Pattern {
        &self.pattern
    }
    fn guard(&self) -> Option<&Expr> {
        self.guard.as_ref()
    }
    fn body(&self) -> &Block {
        &self.body
    }
}

impl MatchArmLike for MatchExprArm {
    type Body = Expr;
    fn pattern(&self) -> &Pattern {
        &self.pattern
    }
    fn guard(&self) -> Option<&Expr> {
        self.guard.as_ref()
    }
    fn body(&self) -> &Expr {
        &self.body
    }
}

/// Emit a half-open range test `start <= scrut < end`: two comparisons, each jumping to the next
/// arm on failure (jumps pushed onto `fails`). Reuses `GtEq`/`Lt` + `JumpIfFalse` (no new opcode).
fn emit_range_test(fc: &mut FnComp, scrut: usize, start: i64, end: i64, fails: &mut Vec<usize>, span: Span) {
    // scrut >= start
    fc.emit(Op::GetLocal(scrut), span);
    fc.emit(Op::ConstInt(start), span);
    fc.emit(Op::GtEq, span);
    fails.push(fc.emit_jump(Op::JumpIfFalse(0), span));
    // scrut < end
    fc.emit(Op::GetLocal(scrut), span);
    fc.emit(Op::ConstInt(end), span);
    fc.emit(Op::Lt, span);
    fails.push(fc.emit_jump(Op::JumpIfFalse(0), span));
}

/// Emit the constant op that pushes a literal pattern's value (mirrors `compile_expr`'s literal
/// lowering; a pattern's string is plain — never interpolated).
fn emit_lit_const(fc: &mut FnComp, lit: &LitPattern, span: Span) {
    match lit {
        LitPattern::Int(n) => fc.emit(Op::ConstInt(*n), span),
        LitPattern::Str(s) => fc.emit(Op::ConstStr(s.clone()), span),
        LitPattern::Bool(b) => fc.emit(if *b { Op::True } else { Op::False }, span),
    }
}

// ===== string interpolation (compile-time pre-parse) =====

enum Chunk {
    Lit(String),
    Expr(Expr),
}

/// Split an interpolated string literal into literal/expr chunks, mirroring `interp::interpolate`
/// (but at compile time): `{{`/`}}` are literal braces; each `{ … }` is lexed + parsed as an
/// expression. A malformed interpolation surfaces here as a compile error.
fn parse_interpolation(raw: &str, span: Span) -> Result<Vec<Chunk>, CompileError> {
    let mut chunks = Vec::new();
    let mut lit = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                lit.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                lit.push('}');
            }
            '{' => {
                if !lit.is_empty() {
                    chunks.push(Chunk::Lit(std::mem::take(&mut lit)));
                }
                let mut inner = String::new();
                let mut closed = false;
                for ic in chars.by_ref() {
                    if ic == '}' {
                        closed = true;
                        break;
                    }
                    inner.push(ic);
                }
                if !closed {
                    return Err(CompileError {
                        message: "unterminated '{' in interpolated string".to_string(),
                        span,
                    });
                }
                let expr = parse_expr_str(&inner, span)?;
                chunks.push(Chunk::Expr(expr));
            }
            '}' => {
                return Err(CompileError {
                    message: "unmatched '}' in string (use '}}' for a literal brace)".to_string(),
                    span,
                });
            }
            _ => lit.push(c),
        }
    }
    if !lit.is_empty() {
        chunks.push(Chunk::Lit(lit));
    }
    Ok(chunks)
}

fn parse_expr_str(src: &str, span: Span) -> Result<Expr, CompileError> {
    let tokens = lexer::tokenize(src).map_err(|e| CompileError { message: e.to_string(), span })?;
    let mut expr = parser::parse_expr(tokens).map_err(|e| CompileError { message: e.message, span })?;
    // Fragments bypass the module-wide desugar pass; lower `?.`/`??` carriers here (both engines do).
    crate::desugar::lower_carriers(&mut expr);
    Ok(expr)
}

// ===== per-function compile state =====

struct LocalVar {
    name: String,
    depth: usize,
}

/// Compile-time state for one function (or the module toplevel): its code buffer, parallel spans,
/// local-slot allocation, and (for closures) the set of captured names.
/// Pending jumps for the innermost loop being compiled. `break` jumps land at the loop exit;
/// `continue` jumps land at the loop's increment/condition. Both targets are unknown when the
/// jump is emitted, so we collect placeholder `Op::Jump(0)` indices and patch them once the
/// targets are known (see `compile_while` / `compile_for`).
struct LoopCtx {
    continue_jumps: Vec<usize>,
    break_jumps: Vec<usize>,
    /// Number of open defer scopes enclosing this loop (captured at loop entry, before the body's
    /// own defer scope). A `break`/`continue` drains every defer scope from the break point down to
    /// (and including) the loop-body scope — i.e. `fc.defer_scopes - defer_floor` of them.
    defer_floor: usize,
}

/// Whether a block directly contains a `defer` statement (so it needs defer-scope brackets). Nested
/// blocks own their own scope, so they are NOT inspected here.
fn block_has_defer(stmts: &[Stmt]) -> bool {
    stmts.iter().any(|s| matches!(s.kind, StmtKind::Defer(_)))
}

struct FnComp {
    name: String,
    arity: usize,
    is_toplevel: bool,
    code: Vec<Op>,
    lines: Vec<Span>,
    locals: Vec<LocalVar>,
    scope_depth: usize,
    /// Number of slots currently in use (== next free slot).
    slot_count: usize,
    max_slots: usize,
    /// Names this function captures from an enclosing scope (closures only).
    captured_names: Vec<String>,
    /// Stack of enclosing loops (innermost last), for `break`/`continue` jump patching. Empty
    /// outside any loop. A closure compiles in its own `FnComp` with an empty stack, so a
    /// `break`/`continue` there has no current loop (the checker rejects it; we defend anyway).
    loops: Vec<LoopCtx>,
    /// Count of defer scopes currently open during compilation (incremented at `EnterDeferScope`,
    /// decremented at the matching `LeaveDeferScope`). Read by `break`/`continue` to know how many
    /// scopes to drain down to the loop body.
    defer_scopes: usize,
}

impl FnComp {
    fn new(name: String, arity: usize, is_toplevel: bool) -> Self {
        FnComp {
            name,
            arity,
            is_toplevel,
            code: Vec::new(),
            lines: Vec::new(),
            locals: Vec::new(),
            scope_depth: 0,
            slot_count: 0,
            max_slots: 0,
            captured_names: Vec::new(),
            loops: Vec::new(),
            defer_scopes: 0,
        }
    }

    /// The innermost loop being compiled, or `None` outside any loop.
    fn current_loop(&mut self) -> Option<&mut LoopCtx> {
        self.loops.last_mut()
    }

    /// Patch a previously-emitted jump to land at an explicit `target` instruction index (used for
    /// `continue`, whose target — the loop's increment — is *before* the current position).
    fn patch_jump_to(&mut self, at: usize, target: usize) {
        match &mut self.code[at] {
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfFalseKeep(t)
            | Op::JumpIfTrueKeep(t)
            | Op::PushHandler(t)
            | Op::MatchArm { next: t, .. } => *t = target,
            other => panic!("patch_jump_to on non-jump op: {other:?}"),
        }
    }

    fn emit(&mut self, op: Op, span: Span) {
        self.code.push(op);
        self.lines.push(span);
    }

    /// Emit a jump-like op, returning its index so it can be patched once the target is known.
    fn emit_jump(&mut self, op: Op, span: Span) -> usize {
        let at = self.code.len();
        self.emit(op, span);
        at
    }

    /// The current instruction index — a jump target.
    fn here(&self) -> usize {
        self.code.len()
    }

    /// Patch a previously-emitted jump to land at the current position.
    fn patch_jump(&mut self, at: usize) {
        let target = self.code.len();
        match &mut self.code[at] {
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfFalseKeep(t)
            | Op::JumpIfTrueKeep(t)
            | Op::PushHandler(t)
            | Op::MatchArm { next: t, .. } => *t = target,
            other => panic!("patch_jump on non-jump op: {other:?}"),
        }
    }

    /// At global scope only when this is the toplevel proto and no block is open.
    fn is_global_scope(&self) -> bool {
        self.is_toplevel && self.scope_depth == 0
    }

    fn begin_scope(&mut self) {
        self.scope_depth += 1;
    }

    fn end_scope(&mut self) {
        self.scope_depth -= 1;
        while let Some(l) = self.locals.last() {
            if l.depth > self.scope_depth {
                self.locals.pop();
                self.slot_count -= 1;
            } else {
                break;
            }
        }
    }

    fn next_slot(&self) -> usize {
        self.slot_count
    }

    /// Add a named local, returning its slot. A redeclaration in the same scope shadows by getting
    /// a fresh slot (later lookups find the newest).
    fn add_local(&mut self, name: String) -> usize {
        let slot = self.slot_count;
        self.locals.push(LocalVar { name, depth: self.scope_depth });
        self.slot_count += 1;
        if self.slot_count > self.max_slots {
            self.max_slots = self.slot_count;
        }
        slot
    }

    /// A hidden compiler-temp slot (loop bookkeeping, match scrutinee) — no name to collide with.
    fn add_hidden(&mut self) -> usize {
        self.add_local(String::new())
    }

    /// Resolve a name to a local slot (innermost first). `None` ⇒ not a local.
    fn resolve_local(&self, name: &str) -> Option<usize> {
        self.locals
            .iter()
            .enumerate()
            .rev()
            .find(|(_, l)| l.name == name)
            .map(|(slot, _)| slot)
    }

    fn captures(&self, name: &str) -> bool {
        self.captured_names.iter().any(|n| n == name)
    }

    /// All bindings visible in this frame, to snapshot into a closure being created here. Locals
    /// (innermost wins) plus, if this frame is itself a closure, its captured names.
    fn snapshot_entries(&self) -> Vec<CapEntry> {
        let mut seen = std::collections::HashSet::new();
        let mut entries = Vec::new();
        // Innermost local of each name wins; skip the hidden (unnamed) temps.
        for (slot, l) in self.locals.iter().enumerate().rev() {
            if l.name.is_empty() || !seen.insert(l.name.clone()) {
                continue;
            }
            entries.push(CapEntry { name: l.name.clone(), src: CapSrc::Slot(slot) });
        }
        for name in &self.captured_names {
            if seen.insert(name.clone()) {
                entries.push(CapEntry { name: name.clone(), src: CapSrc::Captured });
            }
        }
        entries
    }
}
