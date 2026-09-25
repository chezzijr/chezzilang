//! D4 rule 2: syntactic summaries of writes rooted at a method's `self` parameter.

use super::ChainLink;
use crate::ast::{
    DeferTarget, Expr, ExprKind, SpawnTarget, Stmt, StmtKind, WaitArmKind, WaitTarget,
};
use crate::compiler::{chunk_exprs, interp_exprs};

#[derive(Clone, Debug)]
pub(super) enum SelfOp {
    Store(Vec<ChainLink>),
    Call(Vec<ChainLink>, String),
}

pub(super) fn self_ops(body: &[Stmt]) -> Vec<SelfOp> {
    let mut scan = Scan { ops: Vec::new() };
    scan.block(body);
    scan.ops
}

fn self_chain(expr: &Expr) -> Option<Vec<ChainLink>> {
    match &expr.kind {
        ExprKind::Ident(name) if name == "self" => Some(Vec::new()),
        ExprKind::Field { obj, name, .. } => {
            let mut links = self_chain(obj)?;
            links.push(ChainLink::Field(name.clone()));
            Some(links)
        }
        ExprKind::Index { obj, .. } => {
            let mut links = self_chain(obj)?;
            links.push(ChainLink::Index);
            Some(links)
        }
        _ => None,
    }
}

struct Scan {
    ops: Vec<SelfOp>,
}

impl Scan {
    fn block(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let { value, .. } => self.expr(value),
            StmtKind::Assign { target, value, .. } => {
                if let Some(links) = self_chain(target)
                    && !links.is_empty()
                {
                    self.ops.push(SelfOp::Store(links));
                }
                self.expr(target);
                self.expr(value);
            }
            // A nested function owns a different `self` frame.
            StmtKind::Fn(_) => {}
            StmtKind::Struct { .. }
            | StmtKind::Enum { .. }
            | StmtKind::NewType { .. }
            | StmtKind::NativeStruct { .. } => {}
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (cond, body) in branches {
                    self.expr(cond);
                    self.block(body);
                }
                if let Some(body) = else_block {
                    self.block(body);
                }
            }
            StmtKind::For { iter, body, .. } => {
                self.expr(iter);
                self.block(body);
            }
            StmtKind::While { cond, body } => {
                self.expr(cond);
                self.block(body);
            }
            StmtKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.expr(guard);
                    }
                    self.block(&arm.body);
                }
            }
            StmtKind::Return(Some(expr)) | StmtKind::Yield(expr) | StmtKind::Expr(expr) => {
                self.expr(expr)
            }
            StmtKind::Defer(DeferTarget::Call(expr)) | StmtKind::Spawn(SpawnTarget::Call(expr)) => {
                self.expr(expr)
            }
            StmtKind::Defer(DeferTarget::Block(body))
            | StmtKind::Spawn(SpawnTarget::Block(body))
            | StmtKind::Parallel { body } => self.block(body),
            StmtKind::Wait { arms, else_block } => {
                for arm in arms {
                    match &arm.kind {
                        WaitArmKind::Recv { target, chan } => {
                            self.expr(chan);
                            if let WaitTarget::Assign(expr) = target {
                                self.expr(expr);
                            }
                        }
                        WaitArmKind::Send { call } => self.expr(call),
                    }
                    self.block(&arm.body);
                }
                if let Some(body) = else_block {
                    self.block(body);
                }
            }
            StmtKind::Assert { cond, msg } => {
                self.expr(cond);
                if let Some(msg) = msg {
                    self.expr(msg);
                }
            }
            _ => {}
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Str(raw) => {
                for expr in interp_exprs(raw) {
                    self.expr(&expr);
                }
            }
            ExprKind::Interp(chunks) => {
                for expr in chunk_exprs(chunks) {
                    self.expr(expr);
                }
            }
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Bool(_)
            | ExprKind::Pass
            | ExprKind::Ident(_)
            | ExprKind::TypeApply { .. } => {}
            ExprKind::List(items, _) | ExprKind::Tuple(items) | ExprKind::Set(items) => {
                items.iter().for_each(|item| self.expr(item));
            }
            ExprKind::Map(pairs) => pairs.iter().for_each(|(key, value)| {
                self.expr(key);
                self.expr(value);
            }),
            ExprKind::Comprehension {
                key, elem, clauses, ..
            } => {
                for clause in clauses {
                    self.expr(&clause.iter);
                    clause.guards.iter().for_each(|guard| self.expr(guard));
                }
                if let Some(key) = key {
                    self.expr(key);
                }
                self.expr(elem);
            }
            ExprKind::Unary { expr, .. } | ExprKind::Try(expr) => self.expr(expr),
            ExprKind::Compare { operands, .. } => {
                operands.iter().for_each(|operand| self.expr(operand));
            }
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
                if let ExprKind::Field { obj, name, .. } = &callee.kind
                    && let Some(links) = self_chain(obj)
                {
                    self.ops.push(SelfOp::Call(links, name.clone()));
                }
                self.expr(callee);
                args.iter().for_each(|arg| self.expr(arg));
                named.iter().for_each(|(_, value)| self.expr(value));
            }
            ExprKind::Field { obj, .. } => self.expr(obj),
            ExprKind::OptChain { obj, call, .. } => {
                self.expr(obj);
                if let Some(call) = call {
                    call.args.iter().for_each(|arg| self.expr(arg));
                    call.named.iter().for_each(|(_, value)| self.expr(value));
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
                for expr in [start, end, step].into_iter().flatten() {
                    self.expr(expr);
                }
            }
            ExprKind::DecodeCall { obj, arg, .. } => {
                self.expr(obj);
                self.expr(arg);
            }
            // A closure owns a different capture frame. Rule 3 summarizes it separately.
            ExprKind::Closure { .. } => {}
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    if let Some(guard) = &arm.guard {
                        self.expr(guard);
                    }
                    self.expr(&arm.body);
                }
            }
            ExprKind::IfElse { cond, then, els } => {
                self.expr(cond);
                self.expr(then);
                self.expr(els);
            }
            ExprKind::Recover(body) => self.block(body),
        }
    }
}
