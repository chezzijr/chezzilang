//! TICKET-090 — the unused-local warning (Rust's shape: `unused variable`, never Go's error).
//!
//! A separate SYNTACTIC pass over one module's statements, run once per module after the type-check
//! loop. It is not folded into inference because inference re-checks fn bodies (return-inference
//! pass, pass 2, speculative probes) and calls `lookup` for non-reading purposes; a lexical walk
//! reads each identifier exactly once.
//!
//! The warn/silent table is measured against the runtime (`docs/syntax.md` §"Unused locals warn"):
//! candidates are fn-local and block-local `:=`/`let` names and `for` loop variables. Never a
//! candidate: module-scope bindings (an importer may read them), parameters (Go does not warn),
//! `match`/`wait:`/comprehension bindings, nested `fn` names, and any `_`-prefixed name. A plain `=`
//! is a write, not a read; a compound `+=` counts as a read (Go counts it as a use).

use crate::ast::{
    Block, DeferTarget, Expr, ExprKind, FnDecl, Param, Pattern, Span, SpawnTarget, Stmt, StmtKind,
    WaitArmKind, WaitTarget,
};
use crate::compiler::{chunk_exprs, interp_exprs, pattern_binds};
use std::collections::HashSet;

struct Binding {
    name: String,
    span: Span,
    /// `false` for a binding that shadows but is never reported (a parameter, a `match` binding…).
    tracked: bool,
    read: bool,
}

#[derive(Default)]
struct Scan {
    /// Lexical scope stack of indices into `all`. Empty at module scope, whose bindings are never
    /// candidates, so a name that resolves nowhere is a module global or a builtin.
    scopes: Vec<Vec<usize>>,
    all: Vec<Binding>,
}

/// Every never-read local of the module, as `(name, span of the binding)`, in source order.
pub(super) fn unused_locals(stmts: &[Stmt]) -> Vec<(String, Span)> {
    let mut s = Scan::default();
    s.block_items(stmts);
    s.all
        .into_iter()
        .filter(|b| b.tracked && !b.read)
        .map(|b| (b.name, b.span))
        .collect()
}

impl Scan {
    fn bind(&mut self, name: &str, span: Span, tracked: bool) {
        // Module scope binds nothing: a global may be read by an importer.
        let Some(top) = self.scopes.last_mut() else {
            return;
        };
        let tracked = tracked && !name.starts_with('_');
        top.push(self.all.len());
        self.all.push(Binding {
            name: name.to_string(),
            span,
            tracked,
            read: false,
        });
    }

    /// Mark the innermost visible binding of `name` read. A same-scope `:=` is a FRESH binding
    /// (`docs/syntax.md`: a fn-local re-declare is Rust-style shadowing), so the latest one wins.
    fn read(&mut self, name: &str) {
        for scope in self.scopes.iter().rev() {
            for &i in scope.iter().rev() {
                if self.all[i].name == name {
                    self.all[i].read = true;
                    return;
                }
            }
        }
    }

    fn scoped(&mut self, f: impl FnOnce(&mut Self)) {
        self.scopes.push(Vec::new());
        f(self);
        self.scopes.pop();
    }

    fn block(&mut self, b: &Block) {
        self.scoped(|s| s.block_items(b));
    }

    fn params(&mut self, params: &[Param]) {
        for p in params {
            if let Some(d) = &p.default {
                self.expr(d);
            }
        }
        for p in params {
            self.bind(&p.name, p.name_span, false);
        }
    }

    fn fn_decl(&mut self, d: &FnDecl) {
        self.scoped(|s| {
            s.params(&d.params);
            s.block_items(&d.body);
        });
    }

    fn pattern(&mut self, p: &Pattern) {
        let mut names = HashSet::new();
        pattern_binds(p, &mut names);
        for n in names {
            self.bind(&n, Span::RUNTIME, false);
        }
    }

    fn block_items(&mut self, stmts: &[Stmt]) {
        for st in stmts {
            self.stmt(st);
        }
    }

    fn stmt(&mut self, st: &Stmt) {
        match &st.kind {
            StmtKind::Let {
                names,
                name_spans,
                value,
                ..
            } => {
                self.expr(value);
                for (i, n) in names.iter().enumerate() {
                    if n != "_" {
                        self.bind(n, name_spans.get(i).copied().unwrap_or(st.span), true);
                    }
                }
            }
            StmtKind::Assign { target, op, value } => {
                self.expr(value);
                match &target.kind {
                    // A plain `x = e` writes `x` and reads nothing; `_ = e` names no binding.
                    ExprKind::Ident(_) if *op == crate::ast::AssignOp::Eq => {}
                    // `x += e` reads `x` (Go counts it as a use); `p.f = e` / `xs[i] = e` read `p`/`xs`.
                    _ => self.expr(target),
                }
            }
            StmtKind::Fn(d) => {
                self.bind(&d.name, d.name_span, false);
                self.fn_decl(d);
            }
            StmtKind::Struct { methods, .. }
            | StmtKind::Enum { methods, .. }
            | StmtKind::NewType { methods, .. } => {
                for m in methods {
                    self.fn_decl(m);
                }
            }
            StmtKind::NativeStruct { bodied_methods, .. } => {
                for m in bodied_methods {
                    self.fn_decl(m);
                }
            }
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (c, b) in branches {
                    self.expr(c);
                    self.block(b);
                }
                if let Some(b) = else_block {
                    self.block(b);
                }
            }
            StmtKind::For {
                vars,
                var_spans,
                iter,
                body,
            } => {
                self.expr(iter);
                self.scoped(|s| {
                    for (i, v) in vars.iter().enumerate() {
                        if v != "_" {
                            s.bind(v, var_spans.get(i).copied().unwrap_or(st.span), true);
                        }
                    }
                    s.block_items(body);
                });
            }
            StmtKind::While { cond, body } => {
                self.expr(cond);
                self.block(body);
            }
            StmtKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    self.scoped(|s| {
                        s.pattern(&arm.pattern);
                        if let Some(g) = &arm.guard {
                            s.expr(g);
                        }
                        s.block_items(&arm.body);
                    });
                }
            }
            StmtKind::Return(Some(e)) | StmtKind::Yield(e) | StmtKind::Expr(e) => self.expr(e),
            StmtKind::Defer(DeferTarget::Call(e)) | StmtKind::Spawn(SpawnTarget::Call(e)) => {
                self.expr(e)
            }
            StmtKind::Defer(DeferTarget::Block(b))
            | StmtKind::Spawn(SpawnTarget::Block(b))
            | StmtKind::Parallel { body: b } => self.block(b),
            StmtKind::Wait { arms, else_block } => {
                for arm in arms {
                    self.scoped(|s| {
                        match &arm.kind {
                            WaitArmKind::Recv { target, chan } => {
                                s.expr(chan);
                                match target {
                                    WaitTarget::Bind(n) => s.bind(n, arm.span, false),
                                    WaitTarget::Assign(e) => s.expr(e),
                                    WaitTarget::Discard => {}
                                }
                            }
                            WaitArmKind::Send { call } => s.expr(call),
                        }
                        s.block_items(&arm.body);
                    });
                }
                if let Some(b) = else_block {
                    self.block(b);
                }
            }
            StmtKind::Assert { cond, msg } => {
                self.expr(cond);
                if let Some(m) = msg {
                    self.expr(m);
                }
            }
            // Returns/break/pass, and type/import/native/extern declarations, read no local.
            _ => {}
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Ident(n) => self.read(n),
            // A test source is not desugared, so its `{x}` fragments are still raw text.
            ExprKind::Str(raw) => {
                for ie in interp_exprs(raw) {
                    self.expr(&ie);
                }
            }
            ExprKind::Interp(chunks) => {
                for ie in chunk_exprs(chunks) {
                    self.expr(ie);
                }
            }
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::Pass
            | ExprKind::TypeApply { .. } => {}
            ExprKind::List(es, _) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
                es.iter().for_each(|x| self.expr(x))
            }
            ExprKind::Map(pairs) => pairs.iter().for_each(|(k, v)| {
                self.expr(k);
                self.expr(v);
            }),
            ExprKind::Comprehension {
                key, elem, clauses, ..
            } => self.scoped(|s| {
                for c in clauses {
                    s.expr(&c.iter);
                    for v in &c.vars {
                        s.bind(v, Span::RUNTIME, false);
                    }
                    c.guards.iter().for_each(|g| s.expr(g));
                }
                if let Some(k) = key {
                    s.expr(k);
                }
                s.expr(elem);
            }),
            ExprKind::Unary { expr, .. } | ExprKind::Try(expr) => self.expr(expr),
            ExprKind::Compare { operands, .. } => operands.iter().for_each(|o| self.expr(o)),
            ExprKind::Binary { lhs, rhs, .. } | ExprKind::NullCoalesce { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Range { start, end } => {
                self.expr(start);
                self.expr(end);
            }
            ExprKind::Call {
                callee,
                args,
                named,
                ..
            } => {
                self.expr(callee);
                args.iter().for_each(|a| self.expr(a));
                named.iter().for_each(|(_, v)| self.expr(v));
            }
            ExprKind::Field { obj, .. } => self.expr(obj),
            ExprKind::OptChain { obj, call, .. } => {
                self.expr(obj);
                if let Some(c) = call {
                    c.args.iter().for_each(|a| self.expr(a));
                    c.named.iter().for_each(|(_, v)| self.expr(v));
                }
            }
            ExprKind::Index { obj, index } => {
                self.expr(obj);
                self.expr(index);
            }
            ExprKind::Slice {
                obj,
                start,
                end,
                step,
            } => {
                self.expr(obj);
                for o in [start, end, step].into_iter().flatten() {
                    self.expr(o);
                }
            }
            ExprKind::DecodeCall { obj, arg, .. } => {
                self.expr(obj);
                self.expr(arg);
            }
            ExprKind::Closure { params, body, .. } => self.scoped(|s| {
                s.params(params);
                s.expr(body);
            }),
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    self.scoped(|s| {
                        s.pattern(&arm.pattern);
                        if let Some(g) = &arm.guard {
                            s.expr(g);
                        }
                        s.expr(&arm.body);
                    });
                }
            }
            ExprKind::IfElse { cond, then, els } => {
                self.expr(cond);
                self.expr(then);
                self.expr(els);
            }
            ExprKind::Recover(b) => self.block(b),
        }
    }
}
