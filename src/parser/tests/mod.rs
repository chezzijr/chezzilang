use super::*;
use crate::lexer;

/// The value expression of a `x := <expr>` one-statement module.
fn let_value(src: &str) -> Expr {
    match only(src) {
        StmtKind::Let { value, .. } => value,
        other => panic!("expected a let, got {other:?}"),
    }
}

/// The single statement in a one-statement module.
fn only(src: &str) -> StmtKind {
    let mut m = parse_ok(src);
    assert_eq!(m.stmts.len(), 1, "expected exactly one statement");
    m.stmts.remove(0).kind
}

/// The error from a source that must fail to parse.
fn parse_err(src: &str) -> ParseError {
    parse(crate::lexer::tokenize(src).unwrap()).expect_err("expected a parse error")
}

fn parse_ok(src: &str) -> Module {
    parse(crate::lexer::tokenize(src).unwrap()).unwrap_or_else(|e| panic!("parse failed: {e}"))
}

mod const_enum_holes;
mod decls_targets;
mod exprs_stmts;
mod pipe_tuples_defaults;
mod review_panel;
