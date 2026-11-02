//! TICKET-184 — the one control-flow summary: "can control fall through this statement, and which
//! `break` leaves it?". Every consumer derives from [`stmt`] / [`block`]:
//!
//! 1. missing-return (`check_fn_body_inner`: an annotated non-nil fn whose body falls through),
//! 2. the inline `recover: expr` tail (`infer_recover`: a body that cannot fall through is bottom-typed),
//! 3. inline-body inference (`infer_inline_fn_ret`, via [`super::Checker::call_diverges`]),
//! 4. the `else` guard's must-leave check (`infer_else_guard`).
//!
//! The walker's one non-AST input is the divergence oracle: whether an expression statement is a
//! call whose RESOLVED callee never returns ([`super::Checker::call_diverges`]). It never tests a
//! name, so a user fn, method, local or parameter named `exit`/`panic` returns normally.
//!
//! `wait:` falls through only if an arm or its `else` does: a `wait:` without `else` runs exactly one
//! arm or faults ("all-closed + no ready send + no `else` faults", `docs/reference/syntax/`). If `wait:` ever
//! completes without running an arm, the `Wait` arm of [`stmt`] must join [`Flow::NEXT`].
//!
//! A `break` is lexical: one after a `return` in the same block still counts. A `spawn:` /
//! `defer:` block and a nested `fn` are their own frames, so nothing inside them escapes this
//! statement. What may cross a block is `boundary::RULES`.

use crate::ast::{Expr, ExprKind, Stmt, StmtKind};
use crate::lexer::Span;

/// The control-flow summary of a statement or block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Flow {
    /// Control can reach the end of the statement normally.
    pub falls_through: bool,
    /// A `break` that leaves the statement (targets an enclosing loop).
    pub breaks: Option<Span>,
}

impl Flow {
    /// Falls through, escapes nowhere.
    pub(super) const NEXT: Flow = Flow {
        falls_through: true,
        breaks: None,
    };
    /// Cannot fall through, escapes nowhere.
    pub(super) const STOP: Flow = Flow {
        falls_through: false,
        ..Flow::NEXT
    };

    /// `self` followed by `next`.
    fn then(self, next: Flow) -> Flow {
        Flow {
            falls_through: self.falls_through && next.falls_through,
            breaks: self.breaks.or(next.breaks),
        }
    }

    /// Either `self` or `other` runs.
    fn join(self, other: Flow) -> Flow {
        Flow {
            falls_through: self.falls_through || other.falls_through,
            breaks: self.breaks.or(other.breaks),
        }
    }
}

/// The flow of `body`: its statements in sequence.
pub(super) fn block(body: &[Stmt], diverges: &dyn Fn(&Expr) -> bool) -> Flow {
    body.iter()
        .fold(Flow::NEXT, |acc, s| acc.then(stmt(s, diverges)))
}

/// The flow of one statement. `diverges` says whether an expression statement is a call that never
/// returns.
pub(super) fn stmt(s: &Stmt, diverges: &dyn Fn(&Expr) -> bool) -> Flow {
    match &s.kind {
        StmtKind::Return(_) | StmtKind::Continue => Flow::STOP,
        StmtKind::Break => Flow {
            breaks: Some(s.span),
            ..Flow::STOP
        },
        StmtKind::If {
            branches,
            else_block,
        } => {
            let tail = match else_block {
                Some(eb) => block(eb, diverges),
                None => Flow::NEXT,
            };
            branches
                .iter()
                .fold(tail, |acc, (_, b)| acc.join(block(b, diverges)))
        }
        // Exhaustiveness is enforced separately, so some arm always runs.
        StmtKind::Match { arms, .. } => arms
            .iter()
            .fold(Flow::STOP, |acc, a| acc.join(block(&a.body, diverges))),
        StmtKind::Wait { arms, else_block } => {
            let arms = arms
                .iter()
                .fold(Flow::STOP, |acc, a| acc.join(block(&a.body, diverges)));
            match else_block {
                Some(eb) => arms.join(block(eb, diverges)),
                None => arms,
            }
        }
        // The loop owns its own `break` / `continue`.
        StmtKind::While { cond, body } => {
            let b = block(body, diverges);
            Flow {
                falls_through: !matches!(cond.kind, ExprKind::Bool(true)) || b.breaks.is_some(),
                ..Flow::NEXT
            }
        }
        StmtKind::For { .. } => Flow::NEXT,
        // A `parallel:` body runs in this fn's frame.
        StmtKind::Parallel { body } => block(body, diverges),
        StmtKind::Spawn(_) | StmtKind::Defer(_) | StmtKind::Fn(_) => Flow::NEXT,
        StmtKind::Expr(e) => Flow {
            falls_through: !diverges(e),
            ..Flow::NEXT
        },
        // An `else` guard: the statement falls through on success, and its block leaves on
        // failure, so the block's `break` is this statement's.
        StmtKind::Let { value, .. } => match &value.kind {
            ExprKind::ElseGuard { body, .. } => {
                let b = block(body, diverges);
                Flow {
                    breaks: b.breaks,
                    ..Flow::NEXT
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
            | ExprKind::Ident(_)
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
            | ExprKind::Field { .. }
            | ExprKind::Index { .. }
            | ExprKind::Try(_)
            | ExprKind::OptChain { .. }
            | ExprKind::NullCoalesce { .. }
            | ExprKind::Closure { .. }
            | ExprKind::Match { .. }
            | ExprKind::IfElse { .. }
            | ExprKind::Recover(..) => Flow::NEXT,
        },
        StmtKind::Assign { .. }
        | StmtKind::Struct { .. }
        | StmtKind::Protocol { .. }
        | StmtKind::Enum { .. }
        | StmtKind::TypeAlias { .. }
        | StmtKind::Yield(_)
        | StmtKind::Pass
        | StmtKind::Import(_)
        | StmtKind::Extern { .. }
        | StmtKind::Native(_)
        | StmtKind::NativeStruct { .. }
        | StmtKind::NativeEnum { .. }
        | StmtKind::NativeType { .. }
        | StmtKind::Assert { .. } => Flow::NEXT,
    }
}
