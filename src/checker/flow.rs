//! TICKET-184 — the one control-flow summary: "can control fall through this statement, and which
//! `break` / `continue` / `return` leave it?". Every consumer derives from [`stmt`] / [`block`]:
//!
//! 1. missing-return (`check_fn_body_inner`: an annotated non-nil fn whose body falls through),
//! 2. the `recover:` tail (`infer_recover`: a tail that cannot fall through is bottom-typed),
//! 3. inline-body inference (`infer_fn_ret`, via [`super::Checker::call_diverges`]),
//! 4. the escape checks of `recover:` / `defer:` / `spawn:` blocks,
//! 5. fn_writes' "left" (a write after a possible early exit is not definite).
//!
//! The walker's one non-AST input is the divergence oracle: whether an expression statement is a
//! call whose RESOLVED callee never returns ([`super::Checker::call_diverges`]). It never tests a
//! name, so a user fn, method, local or parameter named `exit`/`panic` returns normally.
//!
//! `wait:` falls through only if an arm or its `else` does: a `wait:` without `else` runs exactly one
//! arm or faults ("all-closed + no ready send + no `else` faults", `docs/syntax.md`). If `wait:` ever
//! completes without running an arm, the `Wait` arm of [`stmt`] must join [`Flow::NEXT`].
//!
//! Escapes are lexical: a `break` after a `return` in one block still counts. A `spawn:` / `defer:`
//! block and a nested `fn` are their own frames, so nothing inside them escapes this statement; each
//! block is escape-checked at its own site.

use crate::ast::{Expr, ExprKind, Stmt, StmtKind};
use crate::lexer::Span;

/// The control-flow summary of a statement or block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Flow {
    /// Control can reach the end of the statement normally.
    pub falls_through: bool,
    /// A `break` that leaves the statement (targets an enclosing loop).
    pub breaks: Option<Span>,
    /// A `continue` that leaves the statement (targets an enclosing loop).
    pub continues: Option<Span>,
    /// A `return` that leaves the statement (returns from the enclosing fn).
    pub returns: Option<Span>,
}

impl Flow {
    /// Falls through, escapes nowhere.
    pub(super) const NEXT: Flow = Flow {
        falls_through: true,
        breaks: None,
        continues: None,
        returns: None,
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
            continues: self.continues.or(next.continues),
            returns: self.returns.or(next.returns),
        }
    }

    /// Either `self` or `other` runs.
    fn join(self, other: Flow) -> Flow {
        Flow {
            falls_through: self.falls_through || other.falls_through,
            breaks: self.breaks.or(other.breaks),
            continues: self.continues.or(other.continues),
            returns: self.returns.or(other.returns),
        }
    }

    /// Some `break` / `continue` / `return` leaves the statement.
    pub(super) fn escapes(&self) -> bool {
        self.breaks.is_some() || self.continues.is_some() || self.returns.is_some()
    }

    /// The earliest escape in source order, with its keyword.
    pub(super) fn first_escape(&self) -> Option<(Span, &'static str)> {
        [
            (self.returns, "return"),
            (self.breaks, "break"),
            (self.continues, "continue"),
        ]
        .into_iter()
        .filter_map(|(sp, kw)| sp.map(|sp| (sp, kw)))
        .min_by_key(|(sp, _)| (sp.line, sp.col))
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
        StmtKind::Return(_) => Flow {
            returns: Some(s.span),
            ..Flow::STOP
        },
        StmtKind::Break => Flow {
            breaks: Some(s.span),
            ..Flow::STOP
        },
        StmtKind::Continue => Flow {
            continues: Some(s.span),
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
        // The loop owns its own `break` / `continue`; a `return` inside still leaves it.
        StmtKind::While { cond, body } => {
            let b = block(body, diverges);
            Flow {
                falls_through: !matches!(cond.kind, ExprKind::Bool(true)) || b.breaks.is_some(),
                returns: b.returns,
                ..Flow::NEXT
            }
        }
        StmtKind::For { body, .. } => Flow {
            returns: block(body, diverges).returns,
            ..Flow::NEXT
        },
        // A `parallel:` body runs in this fn's frame: its `return` returns from the fn.
        StmtKind::Parallel { body } => block(body, diverges),
        StmtKind::Spawn(_) | StmtKind::Defer(_) | StmtKind::Fn(_) => Flow::NEXT,
        StmtKind::Expr(e) => Flow {
            falls_through: !diverges(e),
            ..Flow::NEXT
        },
        _ => Flow::NEXT,
    }
}
