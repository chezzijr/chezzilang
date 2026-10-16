//! Parser: the module, item and statement dispatch, simple statements,
//! return/yield/assert/defer/parallel/spawn/wait/recv, imports and dotted paths.
//! Split out of `src/parser/mod.rs`. `super::*` is the parent module.

use super::pattern::bind_leaf;
use super::pattern::is_compound_assign;
use super::*;
use crate::lexer::{Span, Token};

impl Parser {
    pub(super) fn parse_module(&mut self) -> PResult<Module> {
        let mut stmts = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Eof) {
            stmts.push(self.parse_item()?);
            self.skip_newlines();
        }
        Ok(Module { stmts })
    }

    /// THE list of top-level-only declarations (`<item>` in `docs/reference/grammar.bnf`): each item keyword,
    /// the name its block-position diagnostic uses, and its parser. [`Self::parse_item`] dispatches
    /// through it at module level; [`Self::parse_stmt`], which parses every block, rejects every
    /// keyword on it. The checker, compiler and resolver harvest declarations from module-level
    /// statements only, so an item in a block would otherwise parse and do nothing (TICKET-178).
    /// `fn` is not listed: a nested `fn` is a first-class local function, owned by `parse_stmt`.
    pub(super) fn item_keyword(tok: &Token) -> Option<(&'static str, ItemParser)> {
        let item: (&'static str, ItemParser) = match tok {
            Token::Test => ("test fn", |p| Ok(StmtKind::Fn(p.parse_test_fn(true)?))),
            Token::Struct => ("struct", Parser::parse_struct),
            Token::Enum => ("enum", Parser::parse_enum),
            Token::Protocol => ("protocol", Parser::parse_protocol),
            Token::Type => ("type", |p| {
                let k = p.parse_type_alias()?;
                p.expect_stmt_end()?;
                Ok(k)
            }),
            Token::Import => ("import", |p| {
                let k = StmtKind::Import(p.parse_import()?);
                p.expect_stmt_end()?;
                Ok(k)
            }),
            Token::Extern => ("extern block", Parser::parse_extern),
            Token::Native => ("native declaration", Parser::parse_native_decl),
            Token::Int(_)
            | Token::IntMinMagnitude
            | Token::Float(_)
            | Token::Str(_)
            | Token::Bytes(_)
            | Token::RawStr(_)
            | Token::Ident(_)
            | Token::Fn
            | Token::Return
            | Token::If
            | Token::Else
            | Token::Elif
            | Token::For
            | Token::While
            | Token::In
            | Token::Break
            | Token::Continue
            | Token::Pass
            | Token::Match
            | Token::Recover
            | Token::Defer
            | Token::Assert
            | Token::Spawn
            | Token::Parallel
            | Token::Wait
            | Token::Yield
            | Token::From
            | Token::As
            | Token::Const
            | Token::And
            | Token::Or
            | Token::Not
            | Token::True
            | Token::False
            | Token::NoneKw
            | Token::Where
            | Token::Plus
            | Token::Minus
            | Token::Star
            | Token::Slash
            | Token::Percent
            | Token::Assign
            | Token::Walrus
            | Token::EqEq
            | Token::NotEq
            | Token::Lt
            | Token::LtEq
            | Token::Gt
            | Token::GtEq
            | Token::PlusEq
            | Token::MinusEq
            | Token::StarEq
            | Token::SlashEq
            | Token::PercentEq
            | Token::AmpEq
            | Token::PipeEq
            | Token::CaretEq
            | Token::ShlEq
            | Token::ShrEq
            | Token::Arrow
            | Token::Pipe
            | Token::Question
            | Token::QuestionDot
            | Token::QuestionQuestion
            | Token::Bang
            | Token::Amp
            | Token::Caret
            | Token::BitOr
            | Token::Shl
            | Token::Shr
            | Token::LParen
            | Token::RParen
            | Token::LBracket
            | Token::RBracket
            | Token::LBrace
            | Token::RBrace
            | Token::Comma
            | Token::Colon
            | Token::Dot
            | Token::DotDot
            | Token::DotDotEq
            | Token::DotDotDot
            | Token::Newline
            | Token::Indent
            | Token::Dedent
            | Token::Eof => return None,
        };
        Some(item)
    }

    /// `<topLevelStmt>`: an `<item>` at an item keyword, else a `<stmt>`. Charges `depth` like
    /// `parse_stmt`, so a method body nests exactly as deep as it did before the split.
    fn parse_item(&mut self) -> PResult<Stmt> {
        let Some((_, parse)) = Self::item_keyword(self.peek()) else {
            return self.parse_stmt();
        };
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("statement nested too deeply".to_string()));
        }
        let span = self.cur_span();
        let kind = parse(self)?;
        self.depth -= 1;
        Ok(Stmt { kind, span })
    }

    // ----- statements -----

    /// THE list of `<compoundStmt>` starts (`docs/reference/grammar.bnf`): the parser of each statement that
    /// owns a block and ends at its `Dedent`. [`Self::parse_stmt_in`] dispatches through it, and
    /// refuses every start on it as an inline body. `next` is the token after `tok`: `fn (` opens a
    /// closure literal (an expression) and `spawn <call>` is a simple statement, so neither is here.
    /// Both `defer` forms are: `defer` is block-scoped, so an inline body would be its whole block.
    fn compound_keyword(tok: &Token, next: &Token) -> Option<ItemParser> {
        let parse: ItemParser = match tok {
            Token::Fn if next != &Token::LParen => |p| Ok(StmtKind::Fn(p.parse_fn(true)?)),
            Token::If => Parser::parse_if,
            Token::For => Parser::parse_for,
            Token::While => Parser::parse_while,
            Token::Match => Parser::parse_match,
            Token::Parallel => Parser::parse_parallel,
            Token::Wait => Parser::parse_wait,
            Token::Defer => Parser::parse_defer,
            Token::Spawn if next == &Token::Colon => Parser::parse_spawn,
            Token::Int(_)
            | Token::IntMinMagnitude
            | Token::Float(_)
            | Token::Str(_)
            | Token::Bytes(_)
            | Token::RawStr(_)
            | Token::Ident(_)
            | Token::Fn
            | Token::Return
            | Token::Else
            | Token::Elif
            | Token::In
            | Token::Break
            | Token::Continue
            | Token::Pass
            | Token::Struct
            | Token::Enum
            | Token::Protocol
            | Token::Type
            | Token::Recover
            | Token::Assert
            | Token::Test
            | Token::Spawn
            | Token::Yield
            | Token::Import
            | Token::Extern
            | Token::Native
            | Token::From
            | Token::As
            | Token::Const
            | Token::And
            | Token::Or
            | Token::Not
            | Token::True
            | Token::False
            | Token::NoneKw
            | Token::Where
            | Token::Plus
            | Token::Minus
            | Token::Star
            | Token::Slash
            | Token::Percent
            | Token::Assign
            | Token::Walrus
            | Token::EqEq
            | Token::NotEq
            | Token::Lt
            | Token::LtEq
            | Token::Gt
            | Token::GtEq
            | Token::PlusEq
            | Token::MinusEq
            | Token::StarEq
            | Token::SlashEq
            | Token::PercentEq
            | Token::AmpEq
            | Token::PipeEq
            | Token::CaretEq
            | Token::ShlEq
            | Token::ShrEq
            | Token::Arrow
            | Token::Pipe
            | Token::Question
            | Token::QuestionDot
            | Token::QuestionQuestion
            | Token::Bang
            | Token::Amp
            | Token::Caret
            | Token::BitOr
            | Token::Shl
            | Token::Shr
            | Token::LParen
            | Token::RParen
            | Token::LBracket
            | Token::RBracket
            | Token::LBrace
            | Token::RBrace
            | Token::Comma
            | Token::Colon
            | Token::Dot
            | Token::DotDot
            | Token::DotDotEq
            | Token::DotDotDot
            | Token::Newline
            | Token::Indent
            | Token::Dedent
            | Token::Eof => return None,
        };
        Some(parse)
    }

    pub(super) fn parse_stmt(&mut self) -> PResult<Stmt> {
        self.parse_stmt_in(false)
    }

    /// `<stmt>`, or with `inline` the one statement written on the line of its block's `:`, which
    /// is exactly a `<simpleStmt>`.
    pub(super) fn parse_stmt_in(&mut self, inline: bool) -> PResult<Stmt> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("statement nested too deeply".to_string()));
        }
        // A block never holds an item: `<stmts>` does not reach `<item>` (`docs/reference/grammar.bnf`).
        if let Some((kind, _)) = Self::item_keyword(self.peek()) {
            return Err(self.err(format!("{kind} must be a top-level declaration")));
        }
        let span = self.cur_span();
        let outer_inline = std::mem::replace(&mut self.stmt_inline, inline);
        // Compound statements own a block and end at its `Dedent`; line-oriented statements
        // (let/assign/expr/return) must be followed by a line terminator.
        let kind = match Self::compound_keyword(self.peek(), self.peek_at(1)) {
            // `if a: if b: x` would make a trailing `else` ambiguous (which `if`?), and a nested
            // block has no indentation to end at. Force such nesting to use an indented block.
            Some(_) if inline && matches!(self.peek(), Token::If | Token::Match) => {
                self.parse_line_stmt()?
            }
            Some(_) if inline => {
                return Err(self.err(
                    "a nested block must be indented, not written inline after ':'".to_string(),
                ));
            }
            Some(parse) => parse(self)?,
            None => self.parse_line_stmt()?,
        };
        self.stmt_inline = outer_inline;
        self.depth -= 1;
        Ok(Stmt { kind, span })
    }

    /// `<simpleStmt>`: a line-oriented statement, followed by a line terminator.
    fn parse_line_stmt(&mut self) -> PResult<StmtKind> {
        let kind = match self.peek() {
            Token::Spawn => self.parse_spawn_call()?,
            Token::Return => {
                let k = self.parse_return()?;
                self.expect_stmt_end()?;
                k
            }
            Token::Yield => {
                let k = self.parse_yield()?;
                self.expect_stmt_end()?;
                k
            }
            Token::Assert => {
                let k = self.parse_assert()?;
                self.expect_stmt_end()?;
                k
            }
            Token::Break => {
                self.advance();
                self.expect_stmt_end()?;
                StmtKind::Break
            }
            Token::Continue => {
                self.advance();
                self.expect_stmt_end()?;
                StmtKind::Continue
            }
            Token::Pass => {
                // `pass` is a reserved keyword; the likely misuse is trying to use it as a name
                // (`pass := 5`, `pass = 5`, `pass: int`). Give a targeted message for those forms;
                // otherwise it's a no-op statement.
                if matches!(
                    self.peek_at(1),
                    Token::Walrus | Token::Assign | Token::Colon
                ) {
                    return Err(self
                        .keyword_name_error(&Token::Pass)
                        .expect("pass is a keyword"));
                }
                self.advance();
                self.expect_stmt_end()?;
                StmtKind::Pass
            }
            Token::Int(_)
            | Token::IntMinMagnitude
            | Token::Float(_)
            | Token::Str(_)
            | Token::Bytes(_)
            | Token::RawStr(_)
            | Token::Ident(_)
            | Token::Fn
            | Token::If
            | Token::Else
            | Token::Elif
            | Token::For
            | Token::While
            | Token::In
            | Token::Struct
            | Token::Enum
            | Token::Protocol
            | Token::Type
            | Token::Match
            | Token::Recover
            | Token::Defer
            | Token::Test
            | Token::Parallel
            | Token::Wait
            | Token::Import
            | Token::Extern
            | Token::Native
            | Token::From
            | Token::As
            | Token::Const
            | Token::And
            | Token::Or
            | Token::Not
            | Token::True
            | Token::False
            | Token::NoneKw
            | Token::Where
            | Token::Plus
            | Token::Minus
            | Token::Star
            | Token::Slash
            | Token::Percent
            | Token::Assign
            | Token::Walrus
            | Token::EqEq
            | Token::NotEq
            | Token::Lt
            | Token::LtEq
            | Token::Gt
            | Token::GtEq
            | Token::PlusEq
            | Token::MinusEq
            | Token::StarEq
            | Token::SlashEq
            | Token::PercentEq
            | Token::AmpEq
            | Token::PipeEq
            | Token::CaretEq
            | Token::ShlEq
            | Token::ShrEq
            | Token::Arrow
            | Token::Pipe
            | Token::Question
            | Token::QuestionDot
            | Token::QuestionQuestion
            | Token::Bang
            | Token::Amp
            | Token::Caret
            | Token::BitOr
            | Token::Shl
            | Token::Shr
            | Token::LParen
            | Token::RParen
            | Token::LBracket
            | Token::RBracket
            | Token::LBrace
            | Token::RBrace
            | Token::Comma
            | Token::Colon
            | Token::Dot
            | Token::DotDot
            | Token::DotDotEq
            | Token::DotDotDot
            | Token::Newline
            | Token::Indent
            | Token::Dedent
            | Token::Eof => {
                let k = self.parse_simple_stmt()?;
                self.expect_stmt_end()?;
                k
            }
        };
        Ok(kind)
    }

    /// `let` (`:=` or typed `name: T = …`), assignment (`= += -=`), or a bare expression statement.
    fn parse_simple_stmt(&mut self) -> PResult<StmtKind> {
        // Doc-comment for a `let`: the contiguous comment run above this statement's first line.
        // (Computed for every simple-stmt; only consumed by the `Let` arms below.)
        let stmt_doc = self.doc_above(self.cur_span().line);
        // typed let: `name: Type = value`
        if matches!(
            self.peek(),
            Token::Ident(_) | Token::True | Token::False | Token::NoneKw
        ) && self.peek_at(1) == &Token::Colon
        {
            // Capture the binding-name token span (parallel to `names`) for decl-site hover.
            let name_span = self.cur_span();
            let name = self.expect_ident()?;
            self.expect(&Token::Colon)?;
            // `PI: const T = …` — the immutable-binding modifier in the type slot. `const` is
            // consumed only here; `parse_type` never eats it, so it is a parse error in any other
            // type position.
            let is_const = self.eat(&Token::Const);
            let ty = self.parse_type()?;
            if !self.eat_type_assign() {
                self.expect(&Token::Assign)?;
            }
            let (value, listed) = self.parse_value_list(ValueSite::Let)?;
            let value = if listed {
                value
            } else {
                self.parse_else_guard(value)?
            };
            return Ok(StmtKind::Let {
                target: bind_leaf(name, name_span),
                ty: Some(ty),
                value,
                is_const,
                doc: stmt_doc,
            });
        }

        // `a := v`, `_ := v`, `a, (b, c) := v`: the one binding-target parse. The right side of a
        // single name is one value (`ValueSite::Let`); a target list takes `DestructLet`. A comma
        // list is a tuple written without parentheses and takes no `else` guard (DEC-241).
        if let Some(target) = self.try_parse_walrus_target()? {
            let site = if matches!(target, Pattern::Tuple(_)) {
                ValueSite::DestructLet
            } else {
                ValueSite::Let
            };
            let (value, listed) = self.parse_value_list(site)?;
            let value = if listed {
                value
            } else {
                self.parse_else_guard(value)?
            };
            return Ok(StmtKind::Let {
                target,
                ty: None,
                value,
                is_const: false,
                doc: stmt_doc,
            });
        }

        let expr = if self.stmt_inline {
            self.parse_expr()?
        } else {
            self.parse_value(ValueSite::Stmt)?
        };

        // `f() else e: <block>` -- the guard in statement form: the success value is discarded.
        if self.else_is_guard() {
            let value = self.parse_else_guard(expr)?;
            return Ok(StmtKind::Let {
                target: Pattern::Wildcard,
                ty: None,
                value,
                is_const: false,
                doc: None,
            });
        }

        // A comma after the first lvalue introduces a *multi-target* form. `a, (b, c) := expr`
        // never reaches here (`try_parse_walrus_target` took it); a `:=` here has a target that is
        // not a name list, and `bind_target_error` reports it. The other shape is tuple assignment
        // `a, b = b, a` / `data[0], (data[1], x.f) = …` (lvalues, `=`; op==Eq only).
        // `for k, v in m:` never reaches here (parse_for handles it); a parenthesized `(a, b)` is a
        // single expr (no top-level comma).
        if self.ended_at_block() {
            return Ok(StmtKind::Expr(expr));
        }
        if self.peek() == &Token::Comma {
            let mut targets = vec![expr];
            while self.eat_in_line(&Token::Comma) {
                targets.push(self.parse_expr()?);
            }
            if self.eat_in_line(&Token::Walrus) {
                let _ = self.parse_value_list(ValueSite::DestructLet)?;
                let msg = "expected an identifier on the left of ':=' (destructuring binds names)";
                // The first target that is no name; `try_parse_walrus_target` took an all-name list.
                let at = targets
                    .iter()
                    .position(|t| !matches!(t.kind, ExprKind::Ident(_)))
                    .unwrap_or(0);
                return Err(self.bind_target_error(targets.swap_remove(at), msg));
            }
            // `a, b = b, a` — tuple assignment. Only `=` is allowed (compound `+=`, … with multiple
            // targets is rejected). The checker owns the arity of the right side.
            if self.ended_at_block() || self.peek() != &Token::Assign {
                let msg = if is_compound_assign(self.peek()) {
                    "compound assignment is not allowed with multiple targets; assign each separately"
                } else {
                    "expected '=' after a multi-target assignment list"
                };
                return Err(ParseError {
                    message: msg.to_string(),
                    span: self.cur_span(),
                });
            }
            let target_span = targets[0].span;
            let target = Expr {
                id: crate::ast::NodeId::fresh(),
                kind: ExprKind::Tuple(targets),
                span: target_span,
            };
            if !target.kind.is_place(true) {
                return Err(ParseError {
                    message: "invalid assignment target".to_string(),
                    span: target_span,
                });
            }
            self.advance(); // '='
            let (value, _) = self.parse_value_list(ValueSite::TupleAssign)?;
            self.reject_else_guard()?;
            return Ok(StmtKind::Assign {
                target,
                op: AssignOp::Eq,
                value,
            });
        }

        let op = match self.peek() {
            // `try_parse_walrus_target` took every valid `:=` target, so this arm only reports.
            Token::Walrus => {
                return Err(self.bind_target_error(expr, "left side of ':=' must be a name"));
            }
            Token::Assign => AssignOp::Eq,
            Token::PlusEq => AssignOp::PlusEq,
            Token::MinusEq => AssignOp::MinusEq,
            Token::StarEq => AssignOp::StarEq,
            Token::SlashEq => AssignOp::SlashEq,
            Token::PercentEq => AssignOp::PercentEq,
            Token::AmpEq => AssignOp::AmpEq,
            Token::PipeEq => AssignOp::PipeEq,
            Token::CaretEq => AssignOp::CaretEq,
            Token::ShlEq => AssignOp::ShlEq,
            Token::ShrEq => AssignOp::ShrEq,
            Token::Int(_)
            | Token::IntMinMagnitude
            | Token::Float(_)
            | Token::Str(_)
            | Token::Bytes(_)
            | Token::RawStr(_)
            | Token::Ident(_)
            | Token::Fn
            | Token::Return
            | Token::If
            | Token::Else
            | Token::Elif
            | Token::For
            | Token::While
            | Token::In
            | Token::Break
            | Token::Continue
            | Token::Pass
            | Token::Struct
            | Token::Enum
            | Token::Protocol
            | Token::Type
            | Token::Match
            | Token::Recover
            | Token::Defer
            | Token::Assert
            | Token::Test
            | Token::Spawn
            | Token::Parallel
            | Token::Wait
            | Token::Yield
            | Token::Import
            | Token::Extern
            | Token::Native
            | Token::From
            | Token::As
            | Token::Const
            | Token::And
            | Token::Or
            | Token::Not
            | Token::True
            | Token::False
            | Token::NoneKw
            | Token::Where
            | Token::Plus
            | Token::Minus
            | Token::Star
            | Token::Slash
            | Token::Percent
            | Token::EqEq
            | Token::NotEq
            | Token::Lt
            | Token::LtEq
            | Token::Gt
            | Token::GtEq
            | Token::Arrow
            | Token::Pipe
            | Token::Question
            | Token::QuestionDot
            | Token::QuestionQuestion
            | Token::Bang
            | Token::Amp
            | Token::Caret
            | Token::BitOr
            | Token::Shl
            | Token::Shr
            | Token::LParen
            | Token::RParen
            | Token::LBracket
            | Token::RBracket
            | Token::LBrace
            | Token::RBrace
            | Token::Comma
            | Token::Colon
            | Token::Dot
            | Token::DotDot
            | Token::DotDotEq
            | Token::DotDotDot
            | Token::Newline
            | Token::Indent
            | Token::Dedent
            | Token::Eof => return Ok(StmtKind::Expr(expr)),
        };
        // only an assignable place — a name, field, or index — can be on the left of `= += -=`
        if !expr.kind.is_place(false) {
            return Err(ParseError {
                message: "invalid assignment target".to_string(),
                span: expr.span,
            });
        }
        self.advance(); // the assignment operator
        let value = if matches!(op, AssignOp::Eq) {
            self.parse_value_list(ValueSite::Assign)?.0
        } else {
            self.parse_expr()?
        };
        self.reject_else_guard()?;
        Ok(StmtKind::Assign {
            target: expr,
            op,
            value,
        })
    }

    fn parse_return(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Return)?;
        // a bare `return` ends the line — no value
        if self.at_line_end() {
            Ok(StmtKind::Return(None))
        } else {
            let (value, _) = self.parse_value_list(ValueSite::Return)?;
            self.reject_else_guard()?;
            Ok(StmtKind::Return(Some(value)))
        }
    }

    /// `yield <expr>` — always carries a value (unlike `return`). Generator syntax (VM-only; shipped).
    fn parse_yield(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Yield)?;
        let (value, _) = self.parse_value_list(ValueSite::Yield)?;
        Ok(StmtKind::Yield(value))
    }

    /// `assert <cond>` or `assert <cond>, <msg>` — line-oriented (the caller requires a stmt end).
    fn parse_assert(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Assert)?;
        let cond = self.parse_expr()?;
        let msg = if self.eat_in_line(&Token::Comma) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(StmtKind::Assert { cond, msg })
    }

    /// `defer:` block (form 2) or `defer <call>` (form 1). Mirrors `parse_spawn`: form 1 is
    /// line-oriented (terminated here); form 2 is compound (its block ends at its own `Dedent`). The
    /// call-only restriction on form 1 is enforced by the checker (context-sensitive).
    fn parse_defer(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Defer)?;
        if self.check(&Token::Colon) {
            let body = self.parse_block()?;
            return Ok(StmtKind::Defer(DeferTarget::Block(body)));
        }
        let expr = self.parse_expr()?;
        self.expect_stmt_end()?;
        Ok(StmtKind::Defer(DeferTarget::Call(expr)))
    }

    /// `parallel:` — a nursery whose body is an indented (or inline) block. Compound: ends at its
    /// own `Dedent`, so the caller does not require a line terminator.
    fn parse_parallel(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Parallel)?;
        let body = self.parse_block()?;
        Ok(StmtKind::Parallel { body })
    }

    /// `spawn:` block. Compound: its block ends at its own `Dedent`.
    fn parse_spawn(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Spawn)?;
        let body = self.parse_block()?;
        Ok(StmtKind::Spawn(SpawnTarget::Block(body)))
    }

    /// `spawn <call>`. A simple statement, terminated here; a non-call is rejected with a clear
    /// message.
    fn parse_spawn_call(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Spawn)?;
        let expr = self.parse_expr()?;
        if !matches!(expr.kind, ExprKind::Call { .. }) {
            return Err(self.err(
                "spawn requires a function or method call (`spawn f(x)`) or a block (`spawn:`)"
                    .to_string(),
            ));
        }
        self.expect_stmt_end()?;
        Ok(StmtKind::Spawn(SpawnTarget::Call(expr)))
    }

    /// `wait:` — Chezzi's `select` (see [`StmtKind::Wait`]). Indented arms `<target> (:=|=)
    /// <chan>.recv(): <body>`, with an optional non-blocking `else:` that must be the last arm. The
    /// RHS must be a bare `.recv()` (no args); a non-`recv` RHS or `recv(args)` is a parse error.
    fn parse_wait(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Wait)?;
        self.open_block()?; // ':' Newline Indent
        let mut arms = Vec::new();
        let mut else_block = None;
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            if self.check(&Token::Else) {
                self.advance();
                else_block = Some(self.parse_block()?);
                self.skip_newlines();
                if !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
                    return Err(self.err("`else` must be the last arm of a `wait`".to_string()));
                }
                break;
            }
            let arm_span = self.cur_span();
            let mut assign_first: Option<Stmt> = None;
            // `a := ch.recv():`, `_ := …`, `a, (b, c) := …`: the one binding-target parse.
            let kind = if let Some(target) = self.try_parse_walrus_target()? {
                WaitArmKind::Recv {
                    target,
                    chan: self.parse_recv_chan()?,
                }
            } else {
                self.parse_wait_arm_head(arm_span, &mut assign_first)?
            };
            let mut body = self.parse_block()?;
            if let Some(stmt) = assign_first {
                body.insert(0, stmt);
            }
            arms.push(WaitArm {
                kind,
                body,
                span: arm_span,
            });
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        if arms.is_empty() {
            return Err(self.err("`wait` needs at least one `recv` arm".to_string()));
        }
        Ok(StmtKind::Wait { arms, else_block })
    }

    /// A wait arm whose head is an expression: a `:=` with a target that is not a name list (the
    /// error path, `bind_target_error` reports it), a `=` recv arm (`y = ch.recv():`, `a, (b.f, c) =
    /// ch.recv():`), or a bare send arm (`ch.send(v):`).
    /// A `=` arm binds a fresh `$waitN` and sets `assign_first` to the assignment `lhs = $waitN`
    /// for the arm body's first statement (TICKET-227), so the assigned value has a node (it can
    /// wrap into a carrier target like any assignment). The parser stays lenient on a send arm: the
    /// CHECKER validates it is exactly `chan.send(value)`.
    fn parse_wait_arm_head(
        &mut self,
        arm_span: Span,
        assign_first: &mut Option<Stmt>,
    ) -> PResult<WaitArmKind> {
        let mut lhs = self.parse_expr()?;
        let mut listed = false;
        if self.peek() == &Token::Comma {
            let list_span = lhs.span;
            let mut places = vec![lhs];
            while self.eat_in_line(&Token::Comma) {
                places.push(self.parse_expr()?);
            }
            lhs = Expr {
                id: crate::ast::NodeId::fresh(),
                kind: ExprKind::Tuple(places),
                span: list_span,
            };
            listed = true;
        }
        let lhs_span = lhs.span;
        if self.eat_in_line(&Token::Walrus) {
            // `try_parse_walrus_target` took every valid `:=` target, so this branch only reports.
            return Err(
                self.bind_target_error(lhs, "left side of ':=' in a wait arm must be a name")
            );
        }
        if !self.eat_in_line(&Token::Assign) {
            // Bare expr, no `:=`/`=` — a send arm.
            return Ok(WaitArmKind::Send { call: lhs });
        }
        let target = if lhs.is_blank() {
            Pattern::Wildcard
        } else if lhs.kind.is_place(listed) {
            let id = crate::ast::NodeId::fresh();
            let tmp = format!("$wait{}", id.0);
            let value = Expr {
                id: crate::ast::NodeId::fresh(),
                kind: ExprKind::Ident(tmp.clone()),
                span: lhs_span,
            };
            *assign_first = Some(Stmt {
                kind: StmtKind::Assign {
                    target: lhs,
                    op: AssignOp::Eq,
                    value,
                },
                span: arm_span,
            });
            Pattern::Ident(tmp, lhs_span, crate::ast::NodeId::fresh())
        } else {
            return Err(ParseError {
                message: "invalid assignment target".to_string(),
                span: lhs_span,
            });
        };
        Ok(WaitArmKind::Recv {
            target,
            chan: self.parse_recv_chan()?,
        })
    }

    /// The RHS of a recv `wait:` arm: a bare `<chan>.recv()` (no args). Returns the channel expr.
    fn parse_recv_chan(&mut self) -> PResult<Expr> {
        let rhs = self.parse_expr()?;
        let rhs_span = rhs.span;
        match rhs.kind {
            ExprKind::Call {
                callee,
                args,
                named,
                ..
            } if matches!(&callee.kind, ExprKind::Field { name, .. } if name == "recv") => {
                if !args.is_empty() || !named.is_empty() {
                    return Err(ParseError {
                        message: "`recv` in a wait arm takes no arguments".to_string(),
                        span: rhs_span,
                    });
                }
                match callee.kind {
                    ExprKind::Field { obj, .. } => Ok(*obj),
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
                    | ExprKind::Index { .. }
                    | ExprKind::Try(_)
                    | ExprKind::OptChain { .. }
                    | ExprKind::NullCoalesce { .. }
                    | ExprKind::Closure { .. }
                    | ExprKind::Match { .. }
                    | ExprKind::IfElse { .. }
                    | ExprKind::Recover(..)
                    | ExprKind::ElseGuard { .. } => unreachable!("guarded by the matches! above"),
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
            | ExprKind::Recover(..)
            | ExprKind::ElseGuard { .. } => Err(ParseError {
                message: "a wait arm must `recv` from a channel (`v := ch.recv():`)".to_string(),
                span: rhs_span,
            }),
        }
    }

    fn parse_import(&mut self) -> PResult<Import> {
        self.expect(&Token::Import)?;
        if self.rest_of_line_has(&Token::From) {
            // `import n1[, n2 as a], … from a.b.c`
            let mut names = Vec::new();
            // BOUND-NAME spans, parallel to `names`: the `as` alias token when present, else the
            // member token. Diagnostic-only (decl-site hover); equality-neutral on `Import`.
            let mut name_spans = Vec::new();
            loop {
                let member_span = self.cur_span();
                let name = if self.native_enums.is_empty() {
                    self.expect_ident()?
                } else {
                    self.expect_ident_or_none()?
                };
                let (alias, bound_span) = if self.eat(&Token::As) {
                    let alias_span = self.cur_span();
                    (Some(self.expect_ident()?), alias_span)
                } else {
                    (None, member_span)
                };
                names.push((name, alias));
                name_spans.push(bound_span);
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            self.expect(&Token::From)?;
            let path = self.parse_dotted_path()?;
            Ok(Import::From {
                path,
                names,
                name_spans,
            })
        } else {
            // `import a.b.c [as alias]`
            let (path, last_seg_span) = self.parse_dotted_path_spanned()?;
            let (alias, name_span) = if self.eat(&Token::As) {
                let alias_span = self.cur_span();
                (Some(self.expect_ident()?), alias_span)
            } else {
                (None, last_seg_span)
            };
            Ok(Import::Module {
                path,
                alias,
                name_span,
            })
        }
    }

    fn parse_dotted_path(&mut self) -> PResult<Vec<String>> {
        let mut path = vec![self.expect_path_segment()?];
        while self.eat(&Token::Dot) {
            path.push(self.expect_path_segment()?);
        }
        Ok(path)
    }

    /// Like [`parse_dotted_path`] but also returns the source span of the LAST segment token — the
    /// bound name of a whole-module import when there is no `as` alias (`math` in `import std.math`).
    /// Diagnostic-only (the LSP records a decl-site hover there); the span is equality-neutral.
    fn parse_dotted_path_spanned(&mut self) -> PResult<(Vec<String>, Span)> {
        let mut last_span = self.cur_span();
        let mut path = vec![self.expect_path_segment()?];
        while self.eat(&Token::Dot) {
            last_span = self.cur_span();
            path.push(self.expect_path_segment()?);
        }
        Ok((path, last_span))
    }

    /// A module-path segment: an identifier.
    fn expect_path_segment(&mut self) -> PResult<String> {
        self.expect_ident()
    }

    // ----- blocks -----
}

/// Does this function body contain a `yield` that belongs to *this* function? Recurses through
/// compound statements' sub-blocks but deliberately does NOT descend into a nested `fn`
/// definition (its yields are its own) nor into closure expressions (a closure is an expression,
/// so it is never reached by this statement-only walk — a `yield` inside one stays invisible here
/// and is later flagged by the checker as "yield outside a generator").
pub(super) fn body_contains_yield(block: &Block) -> bool {
    block.iter().any(stmt_contains_yield)
}

fn stmt_contains_yield(s: &Stmt) -> bool {
    // A `yield` can also live in a `recover:` block in expression position (`x := recover: … yield …`),
    // which the statement structure does not reach — scan those too. Stops at closures (a closure's
    // yields are its own). See `ast::expr_recover_blocks`.
    let mut recover_blocks = Vec::new();
    stmt_expr_recover_blocks(s, &mut recover_blocks);
    if recover_blocks.iter().any(|b| body_contains_yield(b)) {
        return true;
    }
    match &s.kind {
        StmtKind::Yield(_) => true,
        StmtKind::If {
            branches,
            else_block,
        } => {
            branches.iter().any(|(_, b)| body_contains_yield(b))
                || else_block.as_ref().is_some_and(body_contains_yield)
        }
        StmtKind::For { body, .. } | StmtKind::While { body, .. } | StmtKind::Parallel { body } => {
            body_contains_yield(body)
        }
        StmtKind::Match { arms, .. } => arms.iter().any(|a| body_contains_yield(&a.body)),
        StmtKind::Defer(DeferTarget::Block(b)) | StmtKind::Spawn(SpawnTarget::Block(b)) => {
            body_contains_yield(b)
        }
        StmtKind::Wait { arms, else_block } => {
            arms.iter().any(|a| body_contains_yield(&a.body))
                || else_block.as_ref().is_some_and(body_contains_yield)
        }
        // A nested `fn` owns its own yields — do not descend.
        StmtKind::Let { .. }
        | StmtKind::Assign { .. }
        | StmtKind::Fn(_)
        | StmtKind::Struct { .. }
        | StmtKind::Protocol { .. }
        | StmtKind::Enum { .. }
        | StmtKind::TypeAlias { .. }
        | StmtKind::Return(_)
        | StmtKind::Defer(_)
        | StmtKind::Spawn(_)
        | StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Pass
        | StmtKind::Import(_)
        | StmtKind::Extern { .. }
        | StmtKind::Native(_)
        | StmtKind::NativeStruct { .. }
        | StmtKind::NativeEnum { .. }
        | StmtKind::NativeType { .. }
        | StmtKind::Assert { .. }
        | StmtKind::Expr(_) => false,
    }
}
