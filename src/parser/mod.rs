//! Parser (M2): turns the lexer's `Tok` stream into an AST `Module`.
//!
//! Two interlocking techniques:
//!   - **recursive descent** for statements / declarations (one `parse_*` fn per form), and
//!   - a **Pratt** (precedence-climbing) parser for expressions, following the binding powers
//!     in `docs/syntax.md` §4.
//!
//! Layout tokens (`Newline`/`Indent`/`Dedent`) from the indentation-aware lexer drive block
//! structure: a block opens after a `:` as either an indented `Newline Indent … Dedent` group or
//! a single inline statement on the same line (used by `match` arms like `Circle(r): return …`).

use crate::ast::*;
use crate::lexer::{Span, Tok, Token};
use std::fmt;

/// A syntax error, with the source location it was detected at.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse error ({}): {}", self.span, self.message)
    }
}

type PResult<T> = Result<T, ParseError>;

/// Cap on parser recursion (nested expressions and blocks). Past this we return a `ParseError`
/// instead of letting native stack recursion overflow and abort the process. Kept well below the
/// depth that overflows a small (≈2 MB) thread stack — each nesting level is several frames deep —
/// while far exceeding any realistic source nesting.
const MAX_DEPTH: usize = 128;

/// Parse a full token stream into a `Module`.
pub fn parse(tokens: Vec<Tok>) -> PResult<Module> {
    Parser::new(tokens).parse_module()
}

/// Parse a token stream containing exactly one expression into an `Expr`.
///
/// Used by the M3 interpreter to evaluate the inner `{…}` fragments of an interpolated string
/// by reusing the real lexer + Pratt parser. Tolerates surrounding `Newline`s; anything other
/// than a single expression followed by `Eof` is a `ParseError`.
pub fn parse_expr(tokens: Vec<Tok>) -> PResult<Expr> {
    let mut p = Parser::new(tokens);
    p.skip_newlines();
    let expr = p.parse_expr()?;
    p.skip_newlines();
    if !p.check(&Token::Eof) {
        return Err(p.err(format!(
            "unexpected {} after expression",
            describe(p.peek())
        )));
    }
    Ok(expr)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    depth: usize,
}

impl Parser {
    fn new(toks: Vec<Tok>) -> Self {
        Parser {
            toks,
            pos: 0,
            depth: 0,
        }
    }

    // ----- cursor helpers -----

    /// The current token kind. The stream always ends with `Eof`, so this never goes out of range.
    fn peek(&self) -> &Token {
        &self.toks[self.pos].kind
    }

    /// The token kind `n` positions ahead (or `Eof` past the end).
    fn peek_at(&self, n: usize) -> &Token {
        self.toks
            .get(self.pos + n)
            .map(|t| &t.kind)
            .unwrap_or(&Token::Eof)
    }

    /// The span of the current token.
    fn cur_span(&self) -> Span {
        self.toks[self.pos].span
    }

    /// Consume and return the current token, clamping at the trailing `Eof`.
    fn advance(&mut self) -> Tok {
        let tok = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        tok
    }

    fn check(&self, kind: &Token) -> bool {
        self.peek() == kind
    }

    /// Consume the current token if it matches `kind`; report whether it did.
    fn eat(&mut self, kind: &Token) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Consume the current token, erroring if it isn't `kind`.
    fn expect(&mut self, kind: &Token) -> PResult<Tok> {
        if self.check(kind) {
            Ok(self.advance())
        } else {
            Err(self.err(format!(
                "expected {}, found {}",
                describe(kind),
                describe(self.peek())
            )))
        }
    }

    /// Consume an identifier, returning its name.
    fn expect_ident(&mut self) -> PResult<String> {
        match self.peek() {
            Token::Ident(_) => {
                if let Token::Ident(name) = self.advance().kind {
                    Ok(name)
                } else {
                    unreachable!()
                }
            }
            _ => Err(self.err(format!("expected identifier, found {}", describe(self.peek())))),
        }
    }

    /// A simple statement must end the logical line; otherwise trailing tokens are a syntax error
    /// (e.g. `x := 5 y := 6` packed onto one line).
    fn expect_stmt_end(&mut self) -> PResult<()> {
        if matches!(
            self.peek(),
            Token::Newline | Token::Dedent | Token::Eof
        ) {
            return Ok(());
        }
        // A simple statement whose value is a block-valued expression (`x := match s:` with
        // indented arms) is terminated by the arm block's `Dedent`, which the expression already
        // consumed — so there's no separate line terminator left to require.
        if self.pos > 0 && matches!(self.toks[self.pos - 1].kind, Token::Dedent) {
            return Ok(());
        }
        Err(self.err(format!(
            "expected end of line, found {}",
            describe(self.peek())
        )))
    }

    fn err(&self, message: String) -> ParseError {
        ParseError {
            message,
            span: self.cur_span(),
        }
    }

    /// Skip any run of `Newline` tokens (blank separation between statements).
    fn skip_newlines(&mut self) {
        while self.check(&Token::Newline) {
            self.advance();
        }
    }

    /// Does the rest of the current logical line contain `kind`? (Used to tell the two `import`
    /// forms apart: the `… from …` form always has a `from` before the line's newline.)
    fn rest_of_line_has(&self, kind: &Token) -> bool {
        let mut i = self.pos;
        while let Some(t) = self.toks.get(i) {
            match &t.kind {
                Token::Newline | Token::Indent | Token::Dedent | Token::Eof => return false,
                k if k == kind => return true,
                _ => i += 1,
            }
        }
        false
    }

    // ----- top level -----

    fn parse_module(&mut self) -> PResult<Module> {
        let mut stmts = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Eof) {
            stmts.push(self.parse_stmt()?);
            self.skip_newlines();
        }
        Ok(Module { stmts })
    }

    // ----- statements -----

    fn parse_stmt(&mut self) -> PResult<Stmt> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("statement nested too deeply".to_string()));
        }
        let span = self.cur_span();
        // Compound statements own a block and end at its `Dedent`; line-oriented statements
        // (let/assign/expr/return/import) must be followed by a line terminator.
        let kind = match self.peek() {
            Token::Fn => StmtKind::Fn(self.parse_fn()?),
            Token::Struct => self.parse_struct()?,
            Token::Enum => self.parse_enum()?,
            Token::If => self.parse_if()?,
            Token::For => self.parse_for()?,
            Token::While => self.parse_while()?,
            Token::Match => self.parse_match()?,
            Token::Return => {
                let k = self.parse_return()?;
                self.expect_stmt_end()?;
                k
            }
            Token::Import => {
                let k = StmtKind::Import(self.parse_import()?);
                self.expect_stmt_end()?;
                k
            }
            _ => {
                let k = self.parse_simple_stmt()?;
                self.expect_stmt_end()?;
                k
            }
        };
        self.depth -= 1;
        Ok(Stmt { kind, span })
    }

    /// `let` (`:=` or typed `name: T = …`), assignment (`= += -=`), or a bare expression statement.
    fn parse_simple_stmt(&mut self) -> PResult<StmtKind> {
        // typed let: `name: Type = value`
        if matches!(self.peek(), Token::Ident(_)) && self.peek_at(1) == &Token::Colon {
            let name = self.expect_ident()?;
            self.expect(&Token::Colon)?;
            let ty = self.parse_type()?;
            self.expect(&Token::Assign)?;
            let value = self.parse_expr()?;
            return Ok(StmtKind::Let {
                name,
                ty: Some(ty),
                value,
            });
        }

        let expr = self.parse_expr()?;
        let op = match self.peek() {
            Token::Walrus => {
                self.advance();
                let value = self.parse_expr()?;
                let name = match expr.kind {
                    ExprKind::Ident(n) => n,
                    _ => {
                        return Err(ParseError {
                            message: "left side of ':=' must be a name".to_string(),
                            span: expr.span,
                        })
                    }
                };
                return Ok(StmtKind::Let {
                    name,
                    ty: None,
                    value,
                });
            }
            Token::Assign => AssignOp::Eq,
            Token::PlusEq => AssignOp::PlusEq,
            Token::MinusEq => AssignOp::MinusEq,
            _ => return Ok(StmtKind::Expr(expr)),
        };
        // only an assignable place — a name, field, or index — can be on the left of `= += -=`
        if !matches!(
            expr.kind,
            ExprKind::Ident(_) | ExprKind::Field { .. } | ExprKind::Index { .. }
        ) {
            return Err(ParseError {
                message: "invalid assignment target".to_string(),
                span: expr.span,
            });
        }
        self.advance(); // the assignment operator
        let value = self.parse_expr()?;
        Ok(StmtKind::Assign {
            target: expr,
            op,
            value,
        })
    }

    fn parse_fn(&mut self) -> PResult<FnDecl> {
        self.expect(&Token::Fn)?;
        let name = self.expect_ident()?;
        self.expect(&Token::LParen)?;
        let params = self.parse_params()?;
        self.expect(&Token::RParen)?;
        let ret = if self.eat(&Token::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let body = self.parse_block()?;
        Ok(FnDecl {
            name,
            params,
            ret,
            body,
        })
    }

    /// Comma-separated `name[: Type]` until (but not consuming) the closing `)`.
    fn parse_params(&mut self) -> PResult<Vec<Param>> {
        let mut params = Vec::new();
        if !self.check(&Token::RParen) {
            loop {
                let name = self.expect_ident()?;
                let ty = if self.eat(&Token::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                params.push(Param { name, ty });
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
        }
        Ok(params)
    }

    fn parse_struct(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Struct)?;
        let name = self.expect_ident()?;
        self.open_block()?;
        let mut fields = Vec::new();
        let mut methods = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            if self.check(&Token::Fn) {
                methods.push(self.parse_fn()?);
            } else {
                let fname = self.expect_ident()?;
                self.expect(&Token::Colon)?;
                let ty = self.parse_type()?;
                fields.push(Field { name: fname, ty });
            }
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(StmtKind::Struct {
            name,
            fields,
            methods,
        })
    }

    fn parse_enum(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Enum)?;
        let name = self.expect_ident()?;
        self.open_block()?;
        let mut variants = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            let vname = self.expect_ident()?;
            let mut payload = Vec::new();
            if self.eat(&Token::LParen) {
                if !self.check(&Token::RParen) {
                    loop {
                        payload.push(self.parse_type()?);
                        if !self.eat(&Token::Comma) {
                            break;
                        }
                    }
                }
                self.expect(&Token::RParen)?;
            }
            variants.push(Variant {
                name: vname,
                payload,
            });
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(StmtKind::Enum { name, variants })
    }

    fn parse_if(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::If)?;
        let cond = self.parse_expr()?;
        let body = self.parse_block()?;
        let mut branches = vec![(cond, body)];
        let mut else_block = None;
        loop {
            // An indented body lands the cursor on `else` directly (the `Dedent` precedes it); an
            // inline body (`if x: y`) leaves a `Newline` first, so step over newlines before testing.
            self.skip_newlines();
            if !self.check(&Token::Else) {
                break;
            }
            self.expect(&Token::Else)?;
            if self.check(&Token::If) {
                // `else if` → another branch
                self.expect(&Token::If)?;
                let cond = self.parse_expr()?;
                let body = self.parse_block()?;
                branches.push((cond, body));
            } else {
                else_block = Some(self.parse_block()?);
                break;
            }
        }
        Ok(StmtKind::If {
            branches,
            else_block,
        })
    }

    fn parse_for(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::For)?;
        let var = self.expect_ident()?;
        self.expect(&Token::In)?;
        let iter = self.parse_expr()?;
        let body = self.parse_block()?;
        Ok(StmtKind::For { var, iter, body })
    }

    fn parse_while(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::While)?;
        let cond = self.parse_expr()?;
        let body = self.parse_block()?;
        Ok(StmtKind::While { cond, body })
    }

    fn parse_match(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Match)?;
        let scrutinee = self.parse_expr()?;
        self.open_block()?;
        let mut arms = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            let pattern = self.parse_pattern()?;
            let body = self.parse_block()?;
            arms.push(MatchArm { pattern, body });
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(StmtKind::Match { scrutinee, arms })
    }

    /// Expression-position `match` (keyword already consumed): `match scrut:` then indented
    /// `pattern: value-expr` arms. Each arm body is a single expression (not a block).
    fn parse_match_expr(&mut self, span: Span) -> PResult<Expr> {
        let scrutinee = self.parse_expr()?;
        self.open_block()?; // ':' Newline Indent
        let mut arms = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            let pattern = self.parse_pattern()?;
            self.expect(&Token::Colon)?;
            let body = self.parse_expr()?;
            arms.push(MatchExprArm { pattern, body });
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(Expr {
            kind: ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            span,
        })
    }

    /// Expression-position `if` (keyword already consumed): inline `if cond: then else: els`.
    /// `else` is mandatory; the `then`/`els` expressions stop at the `else` keyword / line end.
    fn parse_if_expr(&mut self, span: Span) -> PResult<Expr> {
        let cond = self.parse_expr()?;
        self.expect(&Token::Colon)?;
        let then = self.parse_expr()?;
        self.expect(&Token::Else)?;
        self.expect(&Token::Colon)?;
        let els = self.parse_expr()?;
        Ok(Expr {
            kind: ExprKind::IfElse {
                cond: Box::new(cond),
                then: Box::new(then),
                els: Box::new(els),
            },
            span,
        })
    }

    fn parse_pattern(&mut self) -> PResult<Pattern> {
        let name = self.expect_ident()?;
        let mut bindings = Vec::new();
        if self.eat(&Token::LParen) {
            if !self.check(&Token::RParen) {
                loop {
                    bindings.push(self.expect_ident()?);
                    if !self.eat(&Token::Comma) {
                        break;
                    }
                }
            }
            self.expect(&Token::RParen)?;
        }
        Ok(Pattern::Variant { name, bindings })
    }

    fn parse_return(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Return)?;
        // a bare `return` ends the line — no value
        if matches!(self.peek(), Token::Newline | Token::Dedent | Token::Eof) {
            Ok(StmtKind::Return(None))
        } else {
            Ok(StmtKind::Return(Some(self.parse_expr()?)))
        }
    }

    fn parse_import(&mut self) -> PResult<Import> {
        self.expect(&Token::Import)?;
        if self.rest_of_line_has(&Token::From) {
            // `import n1[, n2 as a], … from a.b.c`
            let mut names = Vec::new();
            loop {
                let name = self.expect_ident()?;
                let alias = if self.eat(&Token::As) {
                    Some(self.expect_ident()?)
                } else {
                    None
                };
                names.push((name, alias));
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            self.expect(&Token::From)?;
            let path = self.parse_dotted_path()?;
            Ok(Import::From { path, names })
        } else {
            // `import a.b.c [as alias]`
            let path = self.parse_dotted_path()?;
            let alias = if self.eat(&Token::As) {
                Some(self.expect_ident()?)
            } else {
                None
            };
            Ok(Import::Module { path, alias })
        }
    }

    fn parse_dotted_path(&mut self) -> PResult<Vec<String>> {
        let mut path = vec![self.expect_ident()?];
        while self.eat(&Token::Dot) {
            path.push(self.expect_ident()?);
        }
        Ok(path)
    }

    // ----- blocks -----

    /// Open + parse a block after a `:`. Either an indented `Newline Indent … Dedent` group, or a
    /// single inline statement on the same line.
    fn parse_block(&mut self) -> PResult<Block> {
        self.expect(&Token::Colon)?;
        if self.check(&Token::Newline) {
            self.advance(); // the Newline
            self.expect(&Token::Indent)?;
            let mut stmts = Vec::new();
            self.skip_newlines();
            while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
                stmts.push(self.parse_stmt()?);
                self.skip_newlines();
            }
            self.expect(&Token::Dedent)?;
            Ok(stmts)
        } else {
            // inline single statement (e.g. a one-line `match` arm). A compound statement that
            // opens its own block is not allowed inline — `if a: if b: x` would make a trailing
            // `else` ambiguous (which `if`?). Force such nesting to use an indented block.
            if matches!(
                self.peek(),
                Token::If | Token::For | Token::While | Token::Match
            ) {
                return Err(self.err(
                    "a nested block must be indented, not written inline after ':'".to_string(),
                ));
            }
            Ok(vec![self.parse_stmt()?])
        }
    }

    /// Open an indented block for declarations whose bodies are not statements (`struct`, `enum`):
    /// consumes `: Newline Indent`. The caller parses members until `Dedent`.
    fn open_block(&mut self) -> PResult<()> {
        self.expect(&Token::Colon)?;
        self.expect(&Token::Newline)?;
        self.expect(&Token::Indent)?;
        Ok(())
    }

    // ----- types -----

    fn parse_type(&mut self) -> PResult<Type> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("type nested too deeply".to_string()));
        }
        let name = self.expect_ident()?;
        let mut ty = if self.eat(&Token::LBracket) {
            let mut args = vec![self.parse_type()?];
            while self.eat(&Token::Comma) {
                args.push(self.parse_type()?);
            }
            self.expect(&Token::RBracket)?;
            Type::Generic(name, args)
        } else {
            Type::Named(name)
        };
        // Postfix shorthand on a fully-parsed base type: `T?` = Option[T], `T!` = Result[T].
        // Stacks left-to-right (`T?!` = Result[Option[T]]).
        loop {
            if self.eat(&Token::Question) {
                ty = Type::Generic("Option".to_string(), vec![ty]);
            } else if self.eat(&Token::Bang) {
                ty = Type::Generic("Result".to_string(), vec![ty]);
            } else {
                break;
            }
        }
        self.depth -= 1;
        Ok(ty)
    }

    // ----- expressions (Pratt) -----

    fn parse_expr(&mut self) -> PResult<Expr> {
        self.parse_bp(0)
    }

    /// Precedence-climbing core. `min_bp` is the minimum left binding power that may bind here.
    fn parse_bp(&mut self, min_bp: u8) -> PResult<Expr> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("expression nested too deeply".to_string()));
        }
        let mut lhs = self.parse_unary()?;
        while let Some((op, l_bp, r_bp)) = infix_op(self.peek()) {
            if l_bp < min_bp {
                break;
            }
            self.advance(); // the operator
            let rhs = self.parse_bp(r_bp)?;
            let span = lhs.span;
            lhs = match op {
                InfixOp::Bin(op) => Expr {
                    kind: ExprKind::Binary {
                        op,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    },
                    span,
                },
                InfixOp::Range => Expr {
                    kind: ExprKind::Range {
                        start: Box::new(lhs),
                        end: Box::new(rhs),
                    },
                    span,
                },
                // `lhs |> f(args)` desugars at parse time to `f(lhs, args)` — threading `lhs` as the
                // first argument. The RHS must be a call, so checker/interp/VM see a plain call and
                // need no pipe-specific code.
                InfixOp::Pipe => match rhs.kind {
                    ExprKind::Call { callee, args } => {
                        let mut new_args = Vec::with_capacity(args.len() + 1);
                        new_args.push(lhs);
                        new_args.extend(args);
                        Expr {
                            kind: ExprKind::Call { callee, args: new_args },
                            span,
                        }
                    }
                    _ => return Err(self.err("right side of '|>' must be a function call".to_string())),
                },
            };
        }
        self.depth -= 1;
        Ok(lhs)
    }

    /// Prefix unary operators (`not`, `-`), then a postfix chain.
    fn parse_unary(&mut self) -> PResult<Expr> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("expression nested too deeply".to_string()));
        }
        let span = self.cur_span();
        let op = match self.peek() {
            Token::Not => Some(UnaryOp::Not),
            Token::Minus => Some(UnaryOp::Neg),
            _ => None,
        };
        let result = if let Some(op) = op {
            self.advance();
            let expr = self.parse_unary()?;
            Ok(Expr {
                kind: ExprKind::Unary {
                    op,
                    expr: Box::new(expr),
                },
                span,
            })
        } else {
            self.parse_postfix()
        };
        self.depth -= 1;
        result
    }

    /// A primary expression followed by any chain of `(call)`, `.field`, `[index]`, `?`.
    fn parse_postfix(&mut self) -> PResult<Expr> {
        let mut e = self.parse_primary()?;
        loop {
            let span = e.span;
            e = match self.peek() {
                Token::LParen => {
                    self.advance();
                    let mut args = Vec::new();
                    if !self.check(&Token::RParen) {
                        loop {
                            args.push(self.parse_expr()?);
                            if !self.eat(&Token::Comma) {
                                break;
                            }
                        }
                    }
                    self.expect(&Token::RParen)?;
                    Expr {
                        kind: ExprKind::Call {
                            callee: Box::new(e),
                            args,
                        },
                        span,
                    }
                }
                Token::Dot => {
                    self.advance();
                    let name = self.expect_ident()?;
                    Expr {
                        kind: ExprKind::Field {
                            obj: Box::new(e),
                            name,
                        },
                        span,
                    }
                }
                Token::LBracket => {
                    self.advance();
                    let index = self.parse_expr()?;
                    self.expect(&Token::RBracket)?;
                    Expr {
                        kind: ExprKind::Index {
                            obj: Box::new(e),
                            index: Box::new(index),
                        },
                        span,
                    }
                }
                Token::Question => {
                    self.advance();
                    Expr {
                        kind: ExprKind::Try(Box::new(e)),
                        span,
                    }
                }
                _ => break,
            };
        }
        Ok(e)
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        let tok = self.advance();
        let span = tok.span;
        let kind = match tok.kind {
            Token::Int(n) => ExprKind::Int(n),
            Token::Float(f) => ExprKind::Float(f),
            Token::Str(s) => ExprKind::Str(s),
            Token::True => ExprKind::Bool(true),
            Token::False => ExprKind::Bool(false),
            Token::Ident(name) => ExprKind::Ident(name),
            Token::LParen => {
                let e = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                return Ok(e); // grouped — keep the inner expr (and its span)
            }
            Token::LBracket => {
                let mut elems = Vec::new();
                if !self.check(&Token::RBracket) {
                    loop {
                        elems.push(self.parse_expr()?);
                        if !self.eat(&Token::Comma) {
                            break;
                        }
                    }
                }
                self.expect(&Token::RBracket)?;
                ExprKind::List(elems)
            }
            Token::Fn => return self.parse_closure(span),
            // Expression-position `match`/`if` (the keyword was already consumed by `advance`).
            // Statement-position `if`/`match` never reach here — `parse_stmt` dispatches them first.
            Token::Match => return self.parse_match_expr(span),
            Token::If => return self.parse_if_expr(span),
            other => {
                return Err(ParseError {
                    message: format!("unexpected {} in expression", describe(&other)),
                    span,
                })
            }
        };
        Ok(Expr { kind, span })
    }

    /// `fn(params) [-> ret]: expr` — the `fn` keyword has already been consumed.
    fn parse_closure(&mut self, span: Span) -> PResult<Expr> {
        self.expect(&Token::LParen)?;
        let params = self.parse_params()?;
        self.expect(&Token::RParen)?;
        let ret = if self.eat(&Token::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(&Token::Colon)?;
        let body = self.parse_expr()?;
        Ok(Expr {
            kind: ExprKind::Closure {
                params,
                ret,
                body: Box::new(body),
            },
            span,
        })
    }
}

/// Render a token as the user would recognize it, for error messages — `':='` not `Walrus`.
fn describe(tok: &Token) -> String {
    use Token::*;
    let s = match tok {
        Plus => "'+'",
        Minus => "'-'",
        Star => "'*'",
        Slash => "'/'",
        Percent => "'%'",
        Assign => "'='",
        Walrus => "':='",
        EqEq => "'=='",
        NotEq => "'!='",
        Lt => "'<'",
        LtEq => "'<='",
        Gt => "'>'",
        GtEq => "'>='",
        PlusEq => "'+='",
        MinusEq => "'-='",
        Arrow => "'->'",
        Pipe => "'|>'",
        Question => "'?'",
        Bang => "'!'",
        LParen => "'('",
        RParen => "')'",
        LBracket => "'['",
        RBracket => "']'",
        Comma => "','",
        Colon => "':'",
        Dot => "'.'",
        DotDot => "'..'",
        Newline => "end of line",
        Indent => "an indented block",
        Dedent => "a dedent",
        Eof => "end of input",
        Ident(name) => return format!("identifier '{name}'"),
        Int(n) => return format!("integer {n}"),
        Float(f) => return format!("float {f}"),
        Str(_) => "a string literal",
        // keywords print as their lowercase source spelling
        other => return format!("'{}'", format!("{other:?}").to_lowercase()),
    };
    s.to_string()
}

/// An infix operator: a normal binary op, the range marker `..`, or the pipe `|>` (M6).
enum InfixOp {
    Bin(BinaryOp),
    Range,
    Pipe,
}

/// Map a token to its infix operator and (left, right) binding powers, per `docs/syntax.md` §4.
/// All operators are left-associative (`right = left + 1`). `None` means "not an infix operator".
fn infix_op(tok: &Token) -> Option<(InfixOp, u8, u8)> {
    use BinaryOp::*;
    use InfixOp::*;
    let (op, l) = match tok {
        // Pipe `|>` is the lowest-precedence infix op (level 0): the whole expression to its left
        // is threaded into the call on its right. Left-associative (`a |> f |> g` = `(a|>f)|>g`).
        Token::Pipe => (Pipe, 0),
        Token::Or => (Bin(Or), 1),
        Token::And => (Bin(And), 3),
        Token::EqEq => (Bin(Eq), 5),
        Token::NotEq => (Bin(NotEq), 5),
        Token::Lt => (Bin(Lt), 7),
        Token::LtEq => (Bin(LtEq), 7),
        Token::Gt => (Bin(Gt), 7),
        Token::GtEq => (Bin(GtEq), 7),
        Token::DotDot => (Range, 9),
        Token::Plus => (Bin(Add), 11),
        Token::Minus => (Bin(Sub), 11),
        Token::Star => (Bin(Mul), 13),
        Token::Slash => (Bin(Div), 13),
        Token::Percent => (Bin(Mod), 13),
        _ => return None,
    };
    Some((op, l, l + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer;

    fn parse_ok(src: &str) -> Module {
        parse(lexer::tokenize(src).unwrap()).unwrap_or_else(|e| panic!("parse failed: {e}"))
    }

    /// The error from a source that must fail to parse.
    fn parse_err(src: &str) -> ParseError {
        parse(lexer::tokenize(src).unwrap()).expect_err("expected a parse error")
    }

    /// The single statement in a one-statement module.
    fn only(src: &str) -> StmtKind {
        let mut m = parse_ok(src);
        assert_eq!(m.stmts.len(), 1, "expected exactly one statement");
        m.stmts.remove(0).kind
    }

    #[test]
    fn walrus_let() {
        match only("x := 5\n") {
            StmtKind::Let { name, ty, value } => {
                assert_eq!(name, "x");
                assert!(ty.is_none());
                assert_eq!(value.kind, ExprKind::Int(5));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn typed_let() {
        match only("name: str = \"thuan\"\n") {
            StmtKind::Let { name, ty, value } => {
                assert_eq!(name, "name");
                assert_eq!(ty, Some(Type::Named("str".into())));
                assert_eq!(value.kind, ExprKind::Str("thuan".into()));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn assign_ops() {
        assert!(matches!(
            only("count += 1\n"),
            StmtKind::Assign {
                op: AssignOp::PlusEq,
                ..
            }
        ));
        assert!(matches!(
            only("count -= 1\n"),
            StmtKind::Assign {
                op: AssignOp::MinusEq,
                ..
            }
        ));
        assert!(matches!(
            only("count = 1\n"),
            StmtKind::Assign {
                op: AssignOp::Eq,
                ..
            }
        ));
    }

    #[test]
    fn fn_decl_with_and_without_return() {
        match only("fn add(a: int, b: int) -> int:\n    return a + b\n") {
            StmtKind::Fn(f) => {
                assert_eq!(f.name, "add");
                assert_eq!(f.params.len(), 2);
                assert_eq!(f.ret, Some(Type::Named("int".into())));
                assert_eq!(f.body.len(), 1);
            }
            other => panic!("{other:?}"),
        }
        match only("fn log(msg: str):\n    print(msg)\n") {
            StmtKind::Fn(f) => assert!(f.ret.is_none()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn struct_with_field_and_method() {
        match only("struct Point:\n    x: int\n    y: int\n\n    fn dist(self) -> float:\n        return self.x\n") {
            StmtKind::Struct { name, fields, methods } => {
                assert_eq!(name, "Point");
                assert_eq!(fields.len(), 2);
                assert_eq!(methods.len(), 1);
                assert_eq!(methods[0].name, "dist");
                // `self` param carries no type annotation
                assert!(methods[0].params[0].ty.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn enum_with_and_without_payload() {
        match only("enum Shape:\n    Circle(int)\n    Point\n") {
            StmtKind::Enum { name, variants } => {
                assert_eq!(name, "Shape");
                assert_eq!(variants[0].name, "Circle");
                assert_eq!(variants[0].payload, vec![Type::Named("int".into())]);
                assert_eq!(variants[1].name, "Point");
                assert!(variants[1].payload.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn if_else_if_else() {
        match only("if x > 0:\n    print(1)\nelse if x == 0:\n    print(2)\nelse:\n    print(3)\n") {
            StmtKind::If { branches, else_block } => {
                assert_eq!(branches.len(), 2);
                assert!(else_block.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn for_over_range_and_list() {
        match only("for i in 0..10:\n    print(i)\n") {
            StmtKind::For { var, iter, .. } => {
                assert_eq!(var, "i");
                assert!(matches!(iter.kind, ExprKind::Range { .. }));
            }
            other => panic!("{other:?}"),
        }
        match only("for item in items:\n    print(item)\n") {
            StmtKind::For { iter, .. } => assert!(matches!(iter.kind, ExprKind::Ident(_))),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn while_loop() {
        assert!(matches!(
            only("while cond:\n    cond = step()\n"),
            StmtKind::While { .. }
        ));
    }

    #[test]
    fn match_inline_arms() {
        match only("match s:\n    Circle(r): return r\n    Square(n): return n\n    Point: return 0\n") {
            StmtKind::Match { arms, .. } => {
                assert_eq!(arms.len(), 3);
                match &arms[0].pattern {
                    Pattern::Variant { name, bindings } => {
                        assert_eq!(name, "Circle");
                        assert_eq!(bindings, &vec!["r".to_string()]);
                    }
                }
                assert!(arms[2].pattern == Pattern::Variant { name: "Point".into(), bindings: vec![] });
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn return_with_and_without_value() {
        assert!(matches!(
            only("fn f():\n    return\n"),
            StmtKind::Fn(_)
        ));
        // dig into the body of `fn f(): return`
        let StmtKind::Fn(f) = only("fn f():\n    return\n") else {
            panic!()
        };
        assert!(matches!(f.body[0].kind, StmtKind::Return(None)));
        let StmtKind::Fn(g) = only("fn g():\n    return 1\n") else {
            panic!()
        };
        assert!(matches!(g.body[0].kind, StmtKind::Return(Some(_))));
    }

    #[test]
    fn all_import_forms() {
        assert_eq!(
            only("import std.io\n"),
            StmtKind::Import(Import::Module {
                path: vec!["std".into(), "io".into()],
                alias: None,
            })
        );
        assert_eq!(
            only("import std.io as fs\n"),
            StmtKind::Import(Import::Module {
                path: vec!["std".into(), "io".into()],
                alias: Some("fs".into()),
            })
        );
        assert_eq!(
            only("import read, write from std.io\n"),
            StmtKind::Import(Import::From {
                path: vec!["std".into(), "io".into()],
                names: vec![("read".into(), None), ("write".into(), None)],
            })
        );
        assert_eq!(
            only("import read as r from std.io\n"),
            StmtKind::Import(Import::From {
                path: vec!["std".into(), "io".into()],
                names: vec![("read".into(), Some("r".into()))],
            })
        );
    }

    #[test]
    fn closure_expr() {
        let StmtKind::Let { value, .. } = only("double := fn(x: int) -> int: x * 2\n") else {
            panic!()
        };
        match value.kind {
            ExprKind::Closure { params, ret, body } => {
                assert_eq!(params.len(), 1);
                assert_eq!(ret, Some(Type::Named("int".into())));
                assert!(matches!(body.kind, ExprKind::Binary { op: BinaryOp::Mul, .. }));
            }
            other => panic!("{other:?}"),
        }
    }

    /// `1 + 2 * 3` must parse as `1 + (2 * 3)`.
    #[test]
    fn precedence_mul_over_add() {
        let StmtKind::Expr(e) = only("1 + 2 * 3\n") else {
            panic!()
        };
        match e.kind {
            ExprKind::Binary {
                op: BinaryOp::Add,
                rhs,
                ..
            } => assert!(matches!(rhs.kind, ExprKind::Binary { op: BinaryOp::Mul, .. })),
            other => panic!("{other:?}"),
        }
    }

    /// `a and b or c` must parse as `(a and b) or c`.
    #[test]
    fn precedence_and_over_or() {
        let StmtKind::Expr(e) = only("a and b or c\n") else {
            panic!()
        };
        match e.kind {
            ExprKind::Binary {
                op: BinaryOp::Or,
                lhs,
                ..
            } => assert!(matches!(lhs.kind, ExprKind::Binary { op: BinaryOp::And, .. })),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn postfix_call_field_index_try() {
        // p.dist() → Call(Field(p, dist))
        let StmtKind::Expr(e) = only("p.dist()\n") else {
            panic!()
        };
        match e.kind {
            ExprKind::Call { callee, .. } => assert!(matches!(callee.kind, ExprKind::Field { .. })),
            other => panic!("{other:?}"),
        }
        // safe_div(1, 2)? → Try(Call(...))
        let StmtKind::Expr(e) = only("safe_div(1, 2)?\n") else {
            panic!()
        };
        assert!(matches!(e.kind, ExprKind::Try(_)));
        // xs[0] → Index
        let StmtKind::Expr(e) = only("xs[0]\n") else {
            panic!()
        };
        assert!(matches!(e.kind, ExprKind::Index { .. }));
    }

    #[test]
    fn list_literal() {
        let StmtKind::Expr(e) = only("[1, 2, 3]\n") else {
            panic!()
        };
        match e.kind {
            ExprKind::List(elems) => assert_eq!(elems.len(), 3),
            other => panic!("{other:?}"),
        }
    }

    /// The golden target: the whole touchstone program parses, with the expected top-level shape.
    #[test]
    fn parses_hello_example() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/hello.chz"
        ))
        .unwrap();
        let module = parse(lexer::tokenize(&src).unwrap()).expect("hello.chz should parse");
        // greet, Point, Shape, area, safe_div, main, then the top-level `main()` call
        assert_eq!(module.stmts.len(), 7);
        assert!(matches!(module.stmts[0].kind, StmtKind::Fn(_)));
        assert!(matches!(module.stmts[1].kind, StmtKind::Struct { .. }));
        assert!(matches!(module.stmts[2].kind, StmtKind::Enum { .. }));
        assert!(matches!(module.stmts[5].kind, StmtKind::Fn(_)));
        assert!(matches!(module.stmts[6].kind, StmtKind::Expr(_)));
    }

    #[test]
    fn reports_error_with_location() {
        // `fn (` — missing function name
        let err = parse(lexer::tokenize("fn (\n").unwrap()).unwrap_err();
        assert!(err.message.contains("identifier"), "{}", err.message);
        assert_eq!(err.span.line, 1);
    }

    // ===== regression tests for the review-panel findings =====

    /// An inline `if` body must still allow an `else` on the next line (was misparsed).
    #[test]
    fn inline_if_then_else() {
        let StmtKind::Fn(f) = only("fn m():\n    if x: y = 1\n    else: z = 2\n") else {
            panic!()
        };
        match &f.body[0].kind {
            StmtKind::If {
                branches,
                else_block,
            } => {
                assert_eq!(branches.len(), 1);
                assert!(else_block.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    /// `else if` chains longer than one branch must keep every branch.
    #[test]
    fn else_if_chain_three_branches() {
        match only("if a:\n    f()\nelse if b:\n    g()\nelse if c:\n    h()\nelse:\n    i()\n") {
            StmtKind::If {
                branches,
                else_block,
            } => {
                assert_eq!(branches.len(), 3);
                assert!(else_block.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    /// Assignment to a non-lvalue is a syntax error, not a silently-wrong AST.
    #[test]
    fn rejects_non_lvalue_assignment() {
        assert!(parse_err("1 = 2\n").message.contains("assignment target"));
        assert!(parse_err("f() = 3\n").message.contains("assignment target"));
        // a field/index target is fine
        assert!(matches!(only("p.x = 1\n"), StmtKind::Assign { .. }));
        assert!(matches!(only("xs[0] = 1\n"), StmtKind::Assign { .. }));
    }

    /// `:=` left side must be a bare name.
    #[test]
    fn rejects_walrus_on_non_ident() {
        assert!(parse_err("a.b := 1\n").message.contains("must be a name"));
    }

    /// Two statements packed onto one physical line is an error (no terminator between them).
    #[test]
    fn rejects_trailing_tokens() {
        assert!(parse_err("x := 5 y := 6\n").message.contains("end of line"));
    }

    /// A primary that starts with a non-expression token reports a readable error.
    #[test]
    fn primary_error_is_readable() {
        let err = parse_err(", 1\n");
        assert!(err.message.contains("unexpected"), "{}", err.message);
        assert!(err.message.contains("','"), "should name the token: {}", err.message);
    }

    /// Error messages render tokens in source form, not Rust enum names.
    #[test]
    fn error_messages_use_source_spelling() {
        // `for in xs:` — missing loop variable; expected identifier, found keyword `in`
        let err = parse_err("for in xs:\n    f()\n");
        assert!(err.message.contains("'in'"), "{}", err.message);
        assert!(!err.message.contains("In"), "leaked enum name: {}", err.message);
    }

    /// Generic types with multiple arguments parse (the comma loop in `parse_type`).
    #[test]
    fn generic_type_multiple_args() {
        let StmtKind::Fn(f) = only("fn g(m: map[str, list[int]]):\n    return\n") else {
            panic!()
        };
        match &f.params[0].ty {
            Some(Type::Generic(name, args)) => {
                assert_eq!(name, "map");
                assert_eq!(args.len(), 2);
                assert_eq!(args[0], Type::Named("str".into()));
                assert_eq!(args[1], Type::Generic("list".into(), vec![Type::Named("int".into())]));
            }
            other => panic!("{other:?}"),
        }
    }

    /// Unary `not` / `-` build `Unary` nodes.
    #[test]
    fn unary_operators() {
        let StmtKind::Expr(e) = only("not done\n") else {
            panic!()
        };
        assert!(matches!(e.kind, ExprKind::Unary { op: UnaryOp::Not, .. }));
        let StmtKind::Expr(e) = only("-x\n") else { panic!() };
        assert!(matches!(e.kind, ExprKind::Unary { op: UnaryOp::Neg, .. }));
    }

    /// A fully-chained postfix expression nests correctly: `a.b()[0]?`.
    #[test]
    fn chained_postfix() {
        let StmtKind::Expr(e) = only("a.b()[0]?\n") else {
            panic!()
        };
        // Try( Index( Call( Field(a, b) ), 0 ) )
        let ExprKind::Try(inner) = e.kind else {
            panic!("outermost should be Try")
        };
        let ExprKind::Index { obj, .. } = inner.kind else {
            panic!("next should be Index")
        };
        let ExprKind::Call { callee, .. } = obj.kind else {
            panic!("next should be Call")
        };
        assert!(matches!(callee.kind, ExprKind::Field { .. }));
    }

    /// Expression spans point at the start of the construct (lhs for binaries, primary for postfix).
    #[test]
    fn expr_spans_are_populated() {
        // `a + b` indented in a fn body: the binary's span is the lhs `a` at line 2, col 5.
        let StmtKind::Fn(f) = only("fn m():\n    a + b\n") else {
            panic!()
        };
        let StmtKind::Expr(e) = &f.body[0].kind else {
            panic!()
        };
        assert!(matches!(e.kind, ExprKind::Binary { .. }));
        assert_eq!(e.span.line, 2);
        assert_eq!(e.span.col, 5);
    }

    /// Closure without a declared return type parses (`ret: None`).
    #[test]
    fn closure_without_return_type() {
        let StmtKind::Expr(e) = only("nums.map(fn(x): x * 2)\n") else {
            panic!()
        };
        let ExprKind::Call { args, .. } = e.kind else {
            panic!()
        };
        match &args[0].kind {
            ExprKind::Closure { ret, params, .. } => {
                assert!(ret.is_none());
                assert!(params[0].ty.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    /// `T?` is sugar for `Option[T]` in type position.
    #[test]
    fn optional_type_shorthand() {
        let StmtKind::Fn(decl) = only("fn f(x: int?):\n    return x\n") else {
            panic!()
        };
        assert_eq!(
            decl.params[0].ty,
            Some(Type::Generic("Option".into(), vec![Type::Named("int".into())]))
        );
    }

    /// `T!` is sugar for `Result[T]` in type position.
    #[test]
    fn result_type_shorthand() {
        let StmtKind::Fn(decl) = only("fn f() -> int!:\n    return Ok(1)\n") else {
            panic!()
        };
        assert_eq!(
            decl.ret,
            Some(Type::Generic("Result".into(), vec![Type::Named("int".into())]))
        );
    }

    /// The shorthand applies to a fully-parsed base type, including a generic like `list[int]`.
    #[test]
    fn optional_shorthand_on_generic_base() {
        let StmtKind::Fn(decl) = only("fn f(x: list[int]?):\n    return x\n") else {
            panic!()
        };
        assert_eq!(
            decl.params[0].ty,
            Some(Type::Generic(
                "Option".into(),
                vec![Type::Generic("list".into(), vec![Type::Named("int".into())])]
            ))
        );
    }

    /// `match` in expression position parses to `ExprKind::Match` with value-expression arms.
    #[test]
    fn match_expression_parses() {
        let StmtKind::Let { value, .. } = only("x := match s:\n    Some(v): v\n    None: 0\n")
        else {
            panic!()
        };
        let ExprKind::Match { arms, .. } = value.kind else {
            panic!("{value:?}")
        };
        assert_eq!(arms.len(), 2);
        assert!(matches!(arms[1].body.kind, ExprKind::Int(0)));
    }

    /// `if c: a else: b` in expression position parses to `ExprKind::IfElse` (inline, else required).
    #[test]
    fn if_expression_parses() {
        let StmtKind::Let { value, .. } = only("x := if c: 1 else: 2\n") else {
            panic!()
        };
        let ExprKind::IfElse { then, els, .. } = value.kind else {
            panic!("{value:?}")
        };
        assert!(matches!(then.kind, ExprKind::Int(1)));
        assert!(matches!(els.kind, ExprKind::Int(2)));
    }

    #[test]
    fn if_expression_requires_else() {
        assert!(parse_err("x := if c: 1\n").message.contains("else"));
    }

    /// Guards the `expect_stmt_end` Dedent-acceptance: a sibling statement may follow a
    /// (Dedent-terminated) match-expression value — both statements must parse.
    #[test]
    fn match_expr_followed_by_sibling_statement() {
        let m = parse_ok("x := match s:\n    A: 1\n    B: 2\ny := x\n");
        assert_eq!(m.stmts.len(), 2);
    }

    /// match-expr as the last statement in a block: the arm Dedent and the block Dedent stack.
    #[test]
    fn match_expr_last_in_fn_body() {
        let m = parse_ok("fn f(s: Color):\n    x := match s:\n        A: 1\n        B: 2\n    print(x)\n");
        assert_eq!(m.stmts.len(), 1); // just the fn
    }

    /// An if-expression composes inside a larger expression.
    #[test]
    fn if_expr_nested_in_arithmetic() {
        let StmtKind::Let { value, .. } = only("x := 1 + if c: 2 else: 3\n") else {
            panic!()
        };
        assert!(matches!(value.kind, ExprKind::Binary { .. }));
    }

    /// Arm bodies are single-line expressions; an indented block body is a parse error.
    #[test]
    fn match_expr_arm_body_must_be_inline() {
        parse_err("x := match s:\n    A:\n        1\n    B: 2\n");
    }

    /// Bare `!` is a token now (for the `T!` type shorthand) but has no meaning in expression
    /// position — it must still be a parse error, not silently consumed.
    #[test]
    fn bang_in_expression_rejected() {
        assert!(parse_err("x := !y\n").message.contains("unexpected '!'"));
        assert!(parse_err("z := 1 ! 2\n").message.contains("'!'"));
    }

    /// Deeply nested input returns a `ParseError` instead of overflowing the native stack —
    /// across all four recursive entry points: parens (parse_bp), unary chains (parse_unary),
    /// nested generics (parse_type), and nested blocks (parse_stmt).
    #[test]
    fn deep_nesting_errors_not_crash() {
        let paren = format!("x := {}1{}\n", "(".repeat(500), ")".repeat(500));
        assert!(parse(lexer::tokenize(&paren).unwrap())
            .unwrap_err()
            .message
            .contains("too deeply"));

        let unary = format!("x := {}y\n", "not ".repeat(500));
        assert!(parse(lexer::tokenize(&unary).unwrap())
            .unwrap_err()
            .message
            .contains("too deeply"));

        let generic = format!("z: {}int{} = w\n", "list[".repeat(500), "]".repeat(500));
        assert!(parse(lexer::tokenize(&generic).unwrap())
            .unwrap_err()
            .message
            .contains("too deeply"));

        let blocks = {
            let mut s = String::new();
            for i in 0..500 {
                s.push_str(&"    ".repeat(i));
                s.push_str("if x:\n");
            }
            s.push_str(&"    ".repeat(500));
            s.push_str("y = 1\n");
            s
        };
        assert!(parse(lexer::tokenize(&blocks).unwrap())
            .unwrap_err()
            .message
            .contains("too deeply"));
    }

    /// A compound statement cannot be the inline body of a block — that would make a trailing
    /// `else` ambiguous. Force indentation instead.
    #[test]
    fn rejects_inline_nested_block() {
        let err = parse_err("fn m():\n    if a: if b: x = 1\n");
        assert!(err.message.contains("indented"), "{}", err.message);
    }

    /// Strengthened golden check: walk into hello.chz bodies, not just top-level shape.
    #[test]
    fn hello_example_inner_structure() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/hello.chz"
        ))
        .unwrap();
        let module = parse(lexer::tokenize(&src).unwrap()).unwrap();

        // index 4 = `fn safe_div(...) -> Result[int]:` — check its return type and `?` usage.
        let StmtKind::Fn(safe_div) = &module.stmts[4].kind else {
            panic!("stmts[4] should be safe_div")
        };
        assert_eq!(safe_div.name, "safe_div");
        assert_eq!(
            safe_div.ret,
            Some(Type::Generic("Result".into(), vec![Type::Named("int".into())]))
        );
        // body: `if b == 0:` then `return Ok(a / b)`
        assert!(matches!(safe_div.body[0].kind, StmtKind::If { .. }));
        assert!(matches!(safe_div.body[1].kind, StmtKind::Return(Some(_))));

        // index 3 = `fn area(s: Shape) -> float:` containing a `match` with 2 arms.
        let StmtKind::Fn(area) = &module.stmts[3].kind else {
            panic!("stmts[3] should be area")
        };
        let StmtKind::Match { arms, .. } = &area.body[0].kind else {
            panic!("area body should be a match")
        };
        assert_eq!(arms.len(), 2);
        assert_eq!(
            arms[0].pattern,
            Pattern::Variant {
                name: "Circle".into(),
                bindings: vec!["r".into()],
            }
        );
    }

    // ===== M6: pipe operator desugars to a call =====

    /// The value expression of a `x := <expr>` one-statement module.
    fn let_value(src: &str) -> Expr {
        match only(src) {
            StmtKind::Let { value, .. } => value,
            other => panic!("expected a let, got {other:?}"),
        }
    }

    #[test]
    fn pipe_desugars_to_call_with_lhs_first() {
        // `x := 5 |> inc()` ⇒ `inc(5)`
        let v = let_value("x := 5 |> inc()\n");
        let ExprKind::Call { callee, args } = v.kind else {
            panic!("expected a Call, got {:?}", v.kind)
        };
        assert!(matches!(callee.kind, ExprKind::Ident(ref n) if n == "inc"));
        assert_eq!(args.len(), 1);
        assert_eq!(args[0].kind, ExprKind::Int(5));
    }

    #[test]
    fn pipe_prepends_before_existing_args() {
        // `x := 5 |> add(2)` ⇒ `add(5, 2)`
        let v = let_value("x := 5 |> add(2)\n");
        let ExprKind::Call { args, .. } = v.kind else {
            panic!("expected a Call, got {:?}", v.kind)
        };
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].kind, ExprKind::Int(5));
        assert_eq!(args[1].kind, ExprKind::Int(2));
    }

    #[test]
    fn pipe_chain_is_left_associative() {
        // `x := 5 |> inc() |> dbl()` ⇒ `dbl(inc(5))`
        let v = let_value("x := 5 |> inc() |> dbl()\n");
        let ExprKind::Call { callee, args } = v.kind else {
            panic!("expected outer Call, got {:?}", v.kind)
        };
        assert!(matches!(callee.kind, ExprKind::Ident(ref n) if n == "dbl"));
        assert_eq!(args.len(), 1);
        let ExprKind::Call { callee: inner_callee, args: inner_args } = &args[0].kind else {
            panic!("expected inner Call, got {:?}", args[0].kind)
        };
        assert!(matches!(inner_callee.kind, ExprKind::Ident(ref n) if n == "inc"));
        assert_eq!(inner_args[0].kind, ExprKind::Int(5));
    }

    #[test]
    fn pipe_non_call_rhs_rejected() {
        let e = parse_err("x := 5 |> 7\n");
        assert!(e.to_string().contains("right side of '|>' must be a function call"), "{e}");
    }

    #[test]
    fn pipe_bare_identifier_rhs_rejected() {
        let e = parse_err("x := 5 |> f\n");
        assert!(e.to_string().contains("right side of '|>' must be a function call"), "{e}");
    }

    #[test]
    fn pipe_binds_looser_than_arithmetic() {
        // `x := 1 + 2 |> f()` ⇒ `f(1 + 2)`, not `1 + f(2)`.
        let v = let_value("x := 1 + 2 |> f()\n");
        let ExprKind::Call { args, .. } = v.kind else {
            panic!("expected a Call, got {:?}", v.kind)
        };
        assert_eq!(args.len(), 1);
        assert!(matches!(args[0].kind, ExprKind::Binary { op: BinaryOp::Add, .. }));
    }
}
