//! Parser: control flow: if, for, comprehension clauses, while, match, `if`/`match` expressions,
//! else guards, `recover`.
//! Split out of `src/parser/mod.rs`. `super::*` is the parent module.

use super::*;
use crate::lexer::{Span, Token};

impl Parser {
    /// The body of an `if` / `elif` (the cursor is on its ':'), and whether a bare `else:` or `elif`
    /// follows its one statement on the same line.
    fn if_body(&mut self) -> PResult<(Block, bool)> {
        // the cursor is on the ':'; `parse_stmt_in` runs the body's one statement one level down
        let start = self.pos + 1;
        let outer = self.if_line.replace((start, self.depth + 1));
        let body = self.parse_block();
        self.if_line = outer;
        let body = body?;
        Ok((body, self.bare_else_on_line(start)))
    }

    pub(super) fn parse_if(&mut self) -> PResult<StmtKind> {
        let span = self.cur_span();
        self.expect(&Token::If)?;
        let cond = self.parse_expr()?;
        let (mut body, on_line) = self.if_body()?;
        if on_line {
            // TICKET-242 -- `if c: a() else: b()` on one line is the if-expression with its value
            // discarded: the body is its first branch, and the expression form's tail reads the rest.
            let Some(Stmt {
                kind: StmtKind::Expr(then),
                ..
            }) = body.pop()
            else {
                return Err(self.err(IF_BRANCH.to_string()));
            };
            let expr = self.parse_if_expr_tail(span, cond, then)?;
            // anything left on the line means the else branch was not an expression
            // (`else: x = 2`, `else: return 2`)
            self.expect_stmt_end().map_err(|e| ParseError {
                message: IF_BRANCH.to_string(),
                span: e.span,
            })?;
            return Ok(StmtKind::Expr(expr));
        }
        let mut branches = vec![(cond, body)];
        let mut else_block = None;
        loop {
            // An indented body lands the cursor on `elif`/`else` directly (the `Dedent` precedes it);
            // an inline body (`if x: y`) leaves a `Newline` first, so step over newlines before testing.
            self.skip_newlines();
            if self.check(&Token::Elif) {
                // `elif` → another branch (Python-style single keyword; `else if` no longer parses)
                self.expect(&Token::Elif)?;
                let cond = self.parse_expr()?;
                let (body, on_line) = self.if_body()?;
                if on_line {
                    return Err(self.err(ELIF_LINE.to_string()));
                }
                branches.push((cond, body));
            } else if self.check(&Token::Else) {
                self.expect(&Token::Else)?;
                else_block = Some(self.parse_block()?);
                break;
            } else {
                break;
            }
        }
        Ok(StmtKind::If {
            branches,
            else_block,
        })
    }

    pub(super) fn parse_for(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::For)?;
        // One binding (`for x in …`), or a comma-separated target (`for k, (a, b) in m:`) that
        // unpacks each element. The checker enforces which iterands yield which shapes.
        let target = self.parse_loop_target()?;
        self.expect(&Token::In)?;
        let iter = self.parse_expr()?;
        let body = self.parse_block()?;
        Ok(StmtKind::For { target, iter, body })
    }

    /// Parse one comprehension `for` clause: `for <target> in <iter> [if <guard>]…`.
    /// Zero or more `if` guards may follow (Python allows an `if` after any clause, even a chain).
    /// The caller loops this while the next token is `for`, then consumes the closing bracket/brace.
    /// Mirrors `parse_for`'s var/`in`/iter parsing.
    fn parse_comp_clause(&mut self) -> PResult<CompClause> {
        self.expect(&Token::For)?;
        let target = self.parse_loop_target()?;
        self.expect(&Token::In)?;
        let iter = Box::new(self.parse_expr()?);
        let mut guards = Vec::new();
        while self.eat(&Token::If) {
            guards.push(self.parse_expr()?);
        }
        Ok(CompClause {
            target,
            iter,
            guards,
        })
    }

    /// Parse one or more comprehension `for` clauses (the caller has already parsed the element and
    /// confirmed the next token is `for`). Returns clauses in source order (first = outermost loop).
    pub(super) fn parse_comp_clauses(&mut self) -> PResult<Vec<CompClause>> {
        let mut clauses = vec![self.parse_comp_clause()?];
        while self.check(&Token::For) {
            clauses.push(self.parse_comp_clause()?);
        }
        Ok(clauses)
    }

    pub(super) fn parse_while(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::While)?;
        let cond = self.parse_expr()?;
        let body = self.parse_block()?;
        Ok(StmtKind::While { cond, body })
    }

    pub(super) fn parse_match(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Match)?;
        let scrutinee = self.parse_expr()?;
        self.open_block()?;
        let mut arms = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            let arm_span = self.cur_span();
            let pattern = self.parse_pattern()?;
            let guard = if self.eat(&Token::If) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            let body = self.parse_block()?;
            arms.push(MatchArm {
                span: arm_span,
                pattern,
                guard,
                body,
            });
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(StmtKind::Match { scrutinee, arms })
    }

    /// Expression-position `match` (keyword already consumed): `match scrut:` then indented
    /// `pattern: value-expr` arms. Each arm body is a single expression (not a block).
    pub(super) fn parse_match_expr(&mut self, span: Span) -> PResult<Expr> {
        if !self.block_ok(self.pos - 1) {
            return Err(ParseError {
                message: BLOCK_VALUE.to_string(),
                span,
            });
        }
        let scrutinee = self.parse_expr()?;
        self.expect(&Token::Colon)?;
        self.request_layout(self.pos - 1)?;
        self.expect(&Token::Newline)?;
        self.expect(&Token::Indent)?;
        let mut arms = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            let arm_span = self.cur_span();
            let pattern = self.parse_pattern()?;
            let guard = if self.eat(&Token::If) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            self.expect(&Token::Colon)?;
            let body = self.parse_value(ValueSite::MatchArm)?;
            self.expect_stmt_end()?;
            arms.push(MatchExprArm {
                span: arm_span,
                pattern,
                guard,
                body,
            });
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(Expr {
            id: crate::ast::NodeId::fresh(),
            kind: ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            span,
        })
    }

    /// Expression-position `if` (keyword already consumed): inline `if cond: then else: els`.
    /// `else` is mandatory; the `then`/`els` expressions stop at the `else` keyword / line end.
    pub(super) fn parse_if_expr(&mut self, span: Span) -> PResult<Expr> {
        let cond = self.parse_expr()?;
        self.expect(&Token::Colon)?;
        let then = self.if_branch()?;
        self.parse_if_expr_tail(span, cond, then)
    }

    /// One branch of an if-expression. A branch that does not start an expression (`return`, `break`)
    /// reports [`IF_BRANCH`].
    fn if_branch(&mut self) -> PResult<Expr> {
        let start = self.pos;
        self.parse_expr().map_err(|e| {
            if self.pos <= start + 1 && e.span == self.toks[start].span && e.message != BLOCK_VALUE
            {
                ParseError {
                    message: IF_BRANCH.to_string(),
                    span: e.span,
                }
            } else {
                e
            }
        })
    }

    /// The `elif` / `else:` tail of an if-expression whose condition and first branch are parsed.
    /// The one-line statement form ([`Self::parse_if`]) enters here too.
    fn parse_if_expr_tail(&mut self, span: Span, cond: Expr, then: Expr) -> PResult<Expr> {
        // `elif ...` chains: recurse into another expression-`if` for the else-branch
        // (right-associative nesting), mirroring statement-form `parse_if`. Otherwise the
        // plain `else: <expr>` tail — `else` is mandatory (no optional-else leak).
        let els = if self.check(&Token::Elif) {
            let elif_span = self.cur_span();
            self.expect(&Token::Elif)?;
            self.parse_if_expr(elif_span)?
        } else {
            self.expect(&Token::Else)?;
            self.expect(&Token::Colon)?;
            self.if_branch()?
        };
        Ok(Expr {
            id: crate::ast::NodeId::fresh(),
            kind: ExprKind::IfElse {
                cond: Box::new(cond),
                then: Box::new(then),
                els: Box::new(els),
            },
            span,
        })
    }

    /// A bare `else:` or `elif` on the logical line that started at token `start`. The lexer emits no
    /// layout token inside brackets, so "no `Newline` / `Indent` / `Dedent` since `start`" is "same
    /// logical line". A body that ends in a block expression (`x := match n:` + arms) ends at a
    /// `Dedent`, and an indented body starts with a `Newline`; both make it false.
    fn bare_else_on_line(&self, start: usize) -> bool {
        (self.check(&Token::Elif) || (self.check(&Token::Else) && self.peek_at(1) == &Token::Colon))
            && !self.toks[start..self.pos]
                .iter()
                .any(|t| matches!(t.kind, Token::Newline | Token::Indent | Token::Dedent))
    }

    /// TICKET-242 -- a bare `else:` or `elif` that ends the one statement of an inline `if` body: it
    /// belongs to the `if`. The one test the statement-level `else` readers share. The depth test
    /// keeps a statement of a nested block (a `recover:` body, a guard body) out: it runs deeper,
    /// and keeps the `else` reading it has everywhere else.
    pub(super) fn if_line_else(&self) -> bool {
        self.if_line
            .is_some_and(|(start, depth)| self.depth == depth && self.bare_else_on_line(start))
    }

    /// An `else` here is THIS statement's guard. After a block expression the `else` starts a line at
    /// the column of the line that holds the block's head; an inline-body statement starts deeper, so
    /// that `else` belongs to the header (`if`, `elif`, a `wait` arm), not to the statement.
    pub(super) fn else_is_guard(&self) -> bool {
        self.check(&Token::Else)
            && !self.if_line_else()
            && !(self.ended_at_block() && self.stmt_inline)
    }

    /// `<value> else e: <block>` -- the `else` guard, read only after a let's whole value (an if
    /// expression has already taken its own `else` inside `parse_expr`). Returns `value` unchanged
    /// when no `else` follows.
    pub(super) fn parse_else_guard(&mut self, value: Expr) -> PResult<Expr> {
        if !self.else_is_guard() {
            return Ok(value);
        }
        let span = self.cur_span();
        self.advance();
        let err = match self.peek() {
            Token::Ident(_) => {
                let name_span = self.cur_span();
                let name = self.expect_ident()?;
                // `else _:` binds nothing, the same as `else:`.
                (!is_blank_name(&name)).then_some((name, name_span))
            }
            Token::Colon => None,
            t @ Token::Int(_)
            | t @ Token::IntMinMagnitude
            | t @ Token::Float(_)
            | t @ Token::Str(_)
            | t @ Token::Bytes(_)
            | t @ Token::RawStr(_)
            | t @ Token::Fn
            | t @ Token::Return
            | t @ Token::If
            | t @ Token::Else
            | t @ Token::Elif
            | t @ Token::For
            | t @ Token::While
            | t @ Token::In
            | t @ Token::Break
            | t @ Token::Continue
            | t @ Token::Pass
            | t @ Token::Struct
            | t @ Token::Enum
            | t @ Token::Protocol
            | t @ Token::Type
            | t @ Token::Match
            | t @ Token::Recover
            | t @ Token::Defer
            | t @ Token::Assert
            | t @ Token::Test
            | t @ Token::Spawn
            | t @ Token::Parallel
            | t @ Token::Wait
            | t @ Token::Yield
            | t @ Token::Import
            | t @ Token::Extern
            | t @ Token::Native
            | t @ Token::From
            | t @ Token::As
            | t @ Token::Const
            | t @ Token::And
            | t @ Token::Or
            | t @ Token::Not
            | t @ Token::True
            | t @ Token::False
            | t @ Token::NoneKw
            | t @ Token::Where
            | t @ Token::Plus
            | t @ Token::Minus
            | t @ Token::Star
            | t @ Token::Slash
            | t @ Token::Percent
            | t @ Token::Assign
            | t @ Token::Walrus
            | t @ Token::EqEq
            | t @ Token::NotEq
            | t @ Token::Lt
            | t @ Token::LtEq
            | t @ Token::Gt
            | t @ Token::GtEq
            | t @ Token::PlusEq
            | t @ Token::MinusEq
            | t @ Token::StarEq
            | t @ Token::SlashEq
            | t @ Token::PercentEq
            | t @ Token::AmpEq
            | t @ Token::PipeEq
            | t @ Token::CaretEq
            | t @ Token::ShlEq
            | t @ Token::ShrEq
            | t @ Token::Arrow
            | t @ Token::Pipe
            | t @ Token::Question
            | t @ Token::QuestionDot
            | t @ Token::QuestionQuestion
            | t @ Token::Bang
            | t @ Token::Amp
            | t @ Token::Caret
            | t @ Token::BitOr
            | t @ Token::Shl
            | t @ Token::Shr
            | t @ Token::LParen
            | t @ Token::RParen
            | t @ Token::LBracket
            | t @ Token::RBracket
            | t @ Token::LBrace
            | t @ Token::RBrace
            | t @ Token::Comma
            | t @ Token::Dot
            | t @ Token::DotDot
            | t @ Token::DotDotEq
            | t @ Token::DotDotDot
            | t @ Token::Newline
            | t @ Token::Indent
            | t @ Token::Dedent
            | t @ Token::Eof => {
                return Err(self
                    .keyword_name_error(t)
                    .unwrap_or_else(|| self.err("expected ':'".to_string())));
            }
        };
        let body = self.parse_block()?;
        Ok(Expr {
            id: crate::ast::NodeId::fresh(),
            kind: ExprKind::ElseGuard {
                value: Box::new(value),
                err,
                body,
            },
            span,
        })
    }

    /// An `else` after the value of an assignment or a `return`: no guard exists there.
    pub(super) fn reject_else_guard(&self) -> PResult<()> {
        if self.else_is_guard() {
            return Err(self.err(ELSE_GUARD_PLACE.to_string()));
        }
        Ok(())
    }

    /// Expression-position `recover:` (keyword already consumed). `recover:` then a line break is the
    /// block form (it has type `!`); `recover:` then an expression on the same line is the inline
    /// form (`recover: expr`, type `T!`), an ordinary expression that extends to the right like a
    /// lambda body.
    pub(super) fn parse_recover_expr(&mut self, span: Span) -> PResult<Expr> {
        let kw = self.pos - 1;
        let next = &self.toks[(self.pos + 1).min(self.toks.len() - 1)];
        let block =
            self.check(&Token::Colon) && (next.kind == Token::Newline || next.indent.is_some());
        let (body, form) = if block {
            if !self.block_ok(kw) {
                return Err(ParseError {
                    message: BLOCK_VALUE.to_string(),
                    span,
                });
            }
            self.request_layout(self.pos)?;
            (self.parse_block()?, RecoverForm::Block)
        } else {
            self.expect(&Token::Colon)?;
            let body_span = self.cur_span();
            let value = self.parse_expr()?;
            (
                vec![Stmt {
                    kind: StmtKind::Expr(value),
                    span: body_span,
                }],
                RecoverForm::Inline,
            )
        };
        Ok(Expr {
            id: crate::ast::NodeId::fresh(),
            kind: ExprKind::Recover(body, form),
            span,
        })
    }
}
