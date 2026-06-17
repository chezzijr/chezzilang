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

/// Parsed call arguments: the positional args, then the named (`name = expr`) args, in source order.
type CallArgs = (Vec<Expr>, Vec<(String, Expr)>);

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
/// instead of letting native stack recursion overflow and abort the process. Each nesting level is
/// several large parser frames (~16 KB), so 64 levels ≈ 1 MiB — comfortably under a small (≈2 MiB)
/// thread stack with real headroom for the guard to fire before the host stack overflows, while
/// still far exceeding any realistic source nesting. (Was 128, which sat right at the test-thread
/// stack edge; see `deep_nesting_errors_not_crash`.)
const MAX_DEPTH: usize = 64;

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

    /// Expect an integer literal (used for the `end` of a range pattern `start..end`).
    fn expect_int(&mut self) -> PResult<i64> {
        match self.peek() {
            Token::Int(_) => {
                if let Token::Int(n) = self.advance().kind {
                    Ok(n)
                } else {
                    unreachable!()
                }
            }
            _ => Err(self.err(format!("expected integer, found {}", describe(self.peek())))),
        }
    }

    /// Expect a string literal (used for the library name in an `extern "lib":` header).
    fn expect_str(&mut self) -> PResult<String> {
        match self.peek() {
            Token::Str(_) => {
                if let Token::Str(s) = self.advance().kind {
                    Ok(s)
                } else {
                    unreachable!()
                }
            }
            _ => Err(self.err(format!("expected string literal, found {}", describe(self.peek())))),
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
        // An `extern "lib":` block is a top-level-only declaration (it dlopens a library + binds
        // module-global C fns at init). `parse_stmt` runs at depth 1 for a top-level statement
        // (entered from `parse_module`) and at depth >1 inside any block, so a nested extern is
        // rejected here — the checker's hoist + the compiler's eager `MakeCffi` only walk top-level
        // stmts, so a nested extern would silently skip marshallability validation and later die
        // with a misleading "unknown name". Reject it at parse time instead.
        if matches!(self.peek(), Token::Extern) && self.depth > 1 {
            return Err(self.err("extern block must be a top-level declaration".to_string()));
        }
        // Compound statements own a block and end at its `Dedent`; line-oriented statements
        // (let/assign/expr/return/import) must be followed by a line terminator.
        let kind = match self.peek() {
            Token::Fn => StmtKind::Fn(self.parse_fn(true)?),
            // `test fn …` — a `test` modifier before `fn` marks an independent test.
            Token::Test => StmtKind::Fn(self.parse_test_fn(true)?),
            Token::Struct => self.parse_struct()?,
            Token::Enum => self.parse_enum()?,
            Token::Protocol => self.parse_protocol()?,
            Token::Extern => self.parse_extern()?,
            Token::Type => {
                let k = self.parse_type_alias()?;
                self.expect_stmt_end()?;
                k
            }
            Token::If => self.parse_if()?,
            Token::For => self.parse_for()?,
            Token::While => self.parse_while()?,
            Token::Match => self.parse_match()?,
            Token::Parallel => self.parse_parallel()?,
            Token::Spawn => self.parse_spawn()?,
            Token::Wait => self.parse_wait()?,
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
            Token::Defer => self.parse_defer()?,
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
            // `r: ref T = …` — a by-reference binding. `ref` is consumed only here (a binding
            // position); `parse_type` never eats it, so it is a parse error in any other type
            // position (return type, generic arg, collection element, struct field, tuple element).
            let is_ref = self.eat(&Token::Ref);
            let ty = self.parse_type()?;
            self.expect(&Token::Assign)?;
            let value = self.parse_expr()?;
            return Ok(StmtKind::Let {
                names: vec![name],
                ty: Some(ty),
                value,
                is_ref,
            });
        }

        let expr = self.parse_expr()?;

        // A comma after the first lvalue introduces a *multi-target* form (a bare-ident list).
        // Two shapes share this seam:
        //   - destructuring let `a, b := expr` (all bare idents, `:=`)
        //   - tuple assignment `a, b = b, a` / `data[0], data[1] = …` (lvalues, `=`; op==Eq only)
        // `for k, v in m:` never reaches here (parse_for handles it); a parenthesized `(a, b)` is a
        // single expr (no top-level comma).
        if self.peek() == &Token::Comma {
            let mut targets = vec![expr];
            while self.eat(&Token::Comma) {
                targets.push(self.parse_expr()?);
            }
            // `a, b := pair()` — destructuring let. Requires every target to be a bare identifier.
            if self.peek() == &Token::Walrus {
                self.advance();
                let value = self.parse_expr()?;
                let mut names = Vec::with_capacity(targets.len());
                for t in targets {
                    match t.kind {
                        ExprKind::Ident(n) => names.push(n),
                        _ => {
                            return Err(ParseError {
                                message: "expected an identifier on the left of ':=' (destructuring binds names)".to_string(),
                                span: t.span,
                            })
                        }
                    }
                }
                return Ok(StmtKind::Let { names, ty: None, value, is_ref: false });
            }
            // `a, b = b, a` — tuple assignment. Only `=` is allowed (compound `+=`, … with multiple
            // targets is rejected); the RHS must be a value list of equal arity.
            if self.peek() != &Token::Assign {
                let msg = if is_compound_assign(self.peek()) {
                    "compound assignment is not allowed with multiple targets; assign each separately"
                } else {
                    "expected '=' after a multi-target assignment list"
                };
                return Err(ParseError { message: msg.to_string(), span: self.cur_span() });
            }
            let target_span = targets[0].span;
            // Every target must be an assignable place.
            for t in &targets {
                if !matches!(t.kind, ExprKind::Ident(_) | ExprKind::Field { .. } | ExprKind::Index { .. }) {
                    return Err(ParseError {
                        message: "invalid assignment target".to_string(),
                        span: t.span,
                    });
                }
            }
            self.advance(); // '='
            let mut values = vec![self.parse_expr()?];
            while self.eat(&Token::Comma) {
                values.push(self.parse_expr()?);
            }
            let value_span = values[0].span;
            // A single RHS expression with multiple targets (`a, b = f()`) destructures a
            // tuple-valued expression at runtime — the value is passed through as-is (the checker
            // enforces it's a tuple of matching arity). A comma-list RHS must have equal arity and
            // is wrapped as a tuple literal.
            let value = if values.len() == 1 && targets.len() > 1 {
                values.into_iter().next().unwrap()
            } else {
                if values.len() != targets.len() {
                    return Err(ParseError {
                        message: format!(
                            "assignment has {} target(s) but {} value(s)",
                            targets.len(),
                            values.len()
                        ),
                        span: target_span,
                    });
                }
                Expr { kind: ExprKind::Tuple(values), span: value_span }
            };
            return Ok(StmtKind::Assign {
                target: Expr { kind: ExprKind::Tuple(targets), span: target_span },
                op: AssignOp::Eq,
                value,
            });
        }

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
                    names: vec![name],
                    ty: None,
                    value,
                    is_ref: false,
                });
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

    /// `test fn …` — consume the `test` modifier, parse the following `fn`, and tag it `is_test`.
    /// Shared by the free-fn site (`parse_stmt`) and the struct-method site (`parse_struct`).
    fn parse_test_fn(&mut self, allow_defaults: bool) -> PResult<FnDecl> {
        self.expect(&Token::Test)?;
        if !self.check(&Token::Fn) {
            return Err(self.err("`test` must be followed by `fn`".to_string()));
        }
        let mut decl = self.parse_fn(allow_defaults)?;
        decl.is_test = true;
        Ok(decl)
    }

    fn parse_fn(&mut self, allow_defaults: bool) -> PResult<FnDecl> {
        self.expect(&Token::Fn)?;
        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;
        self.expect(&Token::LParen)?;
        let params = self.parse_params(allow_defaults)?;
        self.expect(&Token::RParen)?;
        let ret = if self.eat(&Token::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        // The body opens with a `:`; an INLINE body (`: <stmt>` on the same line) has a non-`Newline`
        // token immediately after that colon (an indented block is `Colon Newline Indent …`). `peek`
        // is the body `Colon` here, so `peek_at(1)` is the token right after it.
        let inline = self.peek() == &Token::Colon && self.peek_at(1) != &Token::Newline;
        let body = self.parse_block()?;
        let is_generator = body_contains_yield(&body);
        // An inline body whose single statement is a bare expression implicitly returns that
        // expression (mirroring a closure `fn(x): expr`). Inline non-expr statements (`: x = 5`,
        // `: return e`) stay as-is; a generator never implicitly returns its expr.
        let inline_expr_body =
            inline && !is_generator && matches!(body.as_slice(), [s] if matches!(s.kind, StmtKind::Expr(_)));
        Ok(FnDecl {
            name,
            type_params,
            params,
            ret,
            body,
            is_generator,
            is_test: false,
            inline_expr_body,
        })
    }

    /// Optional `[T, U: Bound, …]` generic-parameter list immediately after a `fn`/`struct` name.
    /// Returns an empty vec when there's no `[`. Decl-site only — distinct from `parse_type`'s use
    /// of `[` for generic *arguments* (`list[int]`).
    fn parse_type_params(&mut self) -> PResult<Vec<TypeParam>> {
        let mut params = Vec::new();
        if self.eat(&Token::LBracket) {
            loop {
                let name = self.expect_ident()?;
                if params.iter().any(|p: &TypeParam| p.name == name) {
                    return Err(self.err(format!("duplicate type parameter '{name}'")));
                }
                // `T`, `T: Comparable`, multi-bound `T: Add + Mul`, or parameterized `S: Iterator[T]`.
                let mut bounds = Vec::new();
                if self.eat(&Token::Colon) {
                    bounds.push(self.parse_bound()?);
                    while self.eat(&Token::Plus) {
                        bounds.push(self.parse_bound()?);
                    }
                }
                params.push(TypeParam { name, bounds });
                if !self.eat(&Token::Comma) {
                    break;
                }
            }
            self.expect(&Token::RBracket)?;
        }
        Ok(params)
    }

    /// A single protocol bound on a type parameter: a name, optionally with `[T, …]` type arguments
    /// (`Iterator[T]`). The args reuse `parse_type`, so any type expression is accepted syntactically;
    /// only `Iterator` gives its args meaning in the checker.
    fn parse_bound(&mut self) -> PResult<Bound> {
        let name = self.expect_ident()?;
        let mut args = Vec::new();
        if self.eat(&Token::LBracket) {
            args.push(self.parse_type()?);
            while self.eat(&Token::Comma) {
                args.push(self.parse_type()?);
            }
            self.expect(&Token::RBracket)?;
        }
        Ok(Bound { name, args })
    }

    /// A body-less method signature inside a `protocol` block: `fn name(params) -> ret` then NEWLINE.
    fn parse_fn_sig(&mut self) -> PResult<MethodSig> {
        self.expect(&Token::Fn)?;
        let name = self.expect_ident()?;
        self.expect(&Token::LParen)?;
        let params = self.parse_params(false)?;
        self.expect(&Token::RParen)?;
        let ret = if self.eat(&Token::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        Ok(MethodSig { name, params, ret })
    }

    fn parse_protocol(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Protocol)?;
        let name = self.expect_ident()?;
        // `protocol Container[T]:` — optional type parameters, reusing the generic fn/struct parser.
        let type_params = self.parse_type_params()?;
        self.open_block()?;
        let mut methods = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            methods.push(self.parse_fn_sig()?);
            self.expect_stmt_end()?;
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(StmtKind::Protocol { name, type_params, methods })
    }

    /// A body-less C function signature inside an `extern "lib":` block: `fn name(params) -> ret`
    /// then NEWLINE. Mirrors [`parse_fn_sig`] but produces an [`ExternFn`] carrying its own span (for
    /// per-fn marshallability diagnostics) and forbids default arguments.
    fn parse_extern_fn(&mut self) -> PResult<ExternFn> {
        let span = self.cur_span();
        self.expect(&Token::Fn)?;
        let name = self.expect_ident()?;
        self.expect(&Token::LParen)?;
        let params = self.parse_params(false)?;
        self.expect(&Token::RParen)?;
        let ret = if self.eat(&Token::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        Ok(ExternFn { name, params, ret, span })
    }

    /// `extern "lib":` then an INDENT block of body-less C function signatures. Mirrors
    /// [`parse_protocol`]: the library name is a string literal, the body is `open_block` + a loop of
    /// `parse_extern_fn` until `Dedent`.
    fn parse_extern(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Extern)?;
        let lib = self.expect_str()?;
        self.open_block()?;
        let mut fns = Vec::new();
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            fns.push(self.parse_extern_fn()?);
            self.expect_stmt_end()?;
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(StmtKind::Extern { lib, fns })
    }

    /// Comma-separated `name[: Type]` until (but not consuming) the closing `)`.
    /// Parse a parameter list. `allow_defaults` is true only for free `fn` declarations — closures,
    /// methods, and protocol signatures reject `= default`. A defaulted param may not be followed by
    /// a required (non-defaulted) one. A default may be any expression; the desugar pass rejects one
    /// that references another parameter (defaults are evaluated at the call site).
    fn parse_params(&mut self, allow_defaults: bool) -> PResult<Vec<Param>> {
        let mut params = Vec::new();
        let mut seen_default = false;
        if !self.check(&Token::RParen) {
            loop {
                let name = self.expect_ident()?;
                // A `ref` modifier is legal only directly after the `:` of a param annotation
                // (`x: ref int`). `parse_type` never consumes `ref`, so it is a parse error in any
                // nested type position (`x: list[ref int]`, `x: (ref int, int)`).
                let mut is_ref = false;
                let ty = if self.eat(&Token::Colon) {
                    is_ref = self.eat(&Token::Ref);
                    Some(self.parse_type()?)
                } else {
                    None
                };
                let default = if self.eat(&Token::Assign) {
                    if !allow_defaults {
                        return Err(self
                            .err("default arguments are not supported here".to_string()));
                    }
                    // Any expression is allowed as a default; the desugar pass rejects one that
                    // references another parameter (it is cloned into the caller's scope at the call
                    // site, where parameters are not bound).
                    let e = self.parse_expr()?;
                    Some(e)
                } else {
                    None
                };
                if default.is_some() {
                    seen_default = true;
                } else if seen_default {
                    return Err(self.err(format!(
                        "required parameter '{name}' cannot follow a default parameter"
                    )));
                }
                params.push(Param { name, ty, default, is_ref });
                if !self.eat(&Token::Comma) {
                    break;
                }
                // optional trailing comma: a `)` right after a comma ends the parameter list
                if self.check(&Token::RParen) {
                    break;
                }
            }
        }
        Ok(params)
    }

    fn parse_struct(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Struct)?;
        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;
        self.open_block()?;
        let mut fields = Vec::new();
        let mut methods = Vec::new();
        let mut seen_default = false;
        self.skip_newlines();
        while !self.check(&Token::Dedent) && !self.check(&Token::Eof) {
            if self.check(&Token::Fn) {
                // Methods accept default params (like free fns); the desugar pass fills omitted args
                // / reorders named args at method call sites and rejects param-referencing defaults.
                methods.push(self.parse_fn(true)?);
            } else if self.check(&Token::Test) {
                // `test fn name(self)` — a suite test method.
                methods.push(self.parse_test_fn(true)?);
            } else {
                let fname = self.expect_ident()?;
                self.expect(&Token::Colon)?;
                let ty = self.parse_type()?;
                let default = if self.eat(&Token::Assign) {
                    // Any expression is allowed; the desugar pass rejects one that references another
                    // field (it is cloned into the caller's scope at the constructor call site).
                    let e = self.parse_expr()?;
                    Some(e)
                } else {
                    None
                };
                if default.is_some() {
                    seen_default = true;
                } else if seen_default {
                    return Err(self.err(format!(
                        "required field '{fname}' cannot follow a default field"
                    )));
                }
                fields.push(Field { name: fname, ty, default });
            }
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        Ok(StmtKind::Struct {
            name,
            type_params,
            fields,
            methods,
        })
    }

    /// `type Name = <type>` — a transparent type alias (one line, terminated by the caller).
    fn parse_type_alias(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Type)?;
        let name = self.expect_ident()?;
        self.expect(&Token::Assign)?;
        let ty = self.parse_type()?;
        Ok(StmtKind::TypeAlias { name, ty })
    }

    fn parse_enum(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Enum)?;
        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;
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
        Ok(StmtKind::Enum {
            name,
            type_params,
            variants,
        })
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
        // One binding (`for x in …`), or a comma-separated list (`for k, v in m:`) to destructure a
        // map's entries. The checker enforces which iterands accept which arities.
        let mut vars = vec![self.expect_ident()?];
        while self.eat(&Token::Comma) {
            vars.push(self.expect_ident()?);
        }
        self.expect(&Token::In)?;
        let iter = self.parse_expr()?;
        let body = self.parse_block()?;
        Ok(StmtKind::For { vars, iter, body })
    }

    /// Parse one comprehension `for` clause: `for <ident>[, <ident>] in <iter> [if <guard>]…`.
    /// Zero or more `if` guards may follow (Python allows an `if` after any clause, even a chain).
    /// The caller loops this while the next token is `for`, then consumes the closing bracket/brace.
    /// Mirrors `parse_for`'s var/`in`/iter parsing.
    fn parse_comp_clause(&mut self) -> PResult<CompClause> {
        self.expect(&Token::For)?;
        let mut vars = vec![self.expect_ident()?];
        while self.eat(&Token::Comma) {
            vars.push(self.expect_ident()?);
        }
        self.expect(&Token::In)?;
        let iter = Box::new(self.parse_expr()?);
        let mut guards = Vec::new();
        while self.eat(&Token::If) {
            guards.push(self.parse_expr()?);
        }
        Ok(CompClause { vars, iter, guards })
    }

    /// Parse one or more comprehension `for` clauses (the caller has already parsed the element and
    /// confirmed the next token is `for`). Returns clauses in source order (first = outermost loop).
    fn parse_comp_clauses(&mut self) -> PResult<Vec<CompClause>> {
        let mut clauses = vec![self.parse_comp_clause()?];
        while self.check(&Token::For) {
            clauses.push(self.parse_comp_clause()?);
        }
        Ok(clauses)
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
            let guard = if self.eat(&Token::If) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            let body = self.parse_block()?;
            arms.push(MatchArm { pattern, guard, body });
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
            let guard = if self.eat(&Token::If) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            self.expect(&Token::Colon)?;
            let body = self.parse_expr()?;
            arms.push(MatchExprArm { pattern, guard, body });
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

    /// Expression-position `recover:` (keyword already consumed): `recover:` then an inline or
    /// indented block. Reuses `parse_block`; the block's trailing expression is its `Ok` value.
    fn parse_recover_expr(&mut self, span: Span) -> PResult<Expr> {
        let body = self.parse_block()?;
        Ok(Expr {
            kind: ExprKind::Recover(body),
            span,
        })
    }

    /// Parse a top-level match-arm pattern. A bare identifier here names a variant (`None`,
    /// `Point`); `(...)` is a tuple pattern. Nested positions use [`parse_subpattern`], where a bare
    /// identifier is a binding instead.
    fn parse_pattern(&mut self) -> PResult<Pattern> {
        self.parse_pattern_impl(true)
    }

    /// Parse a sub-pattern (a variant payload slot or a tuple element). A bare identifier here is a
    /// binding name; `Name(...)` is a nested variant; `(...)` a nested tuple.
    fn parse_subpattern(&mut self) -> PResult<Pattern> {
        self.parse_pattern_impl(false)
    }

    /// Parse one-or-more primaries separated by `|` (Token::BitOr), the or-alternation level.
    /// Returns `Pattern::Or` only when there is more than one alternative; a single primary is
    /// returned unchanged (zero AST regression). Works at both top-arm and sub-pattern positions
    /// (the `top` flag flows into each primary so every alternative disambiguates identically).
    fn parse_pattern_impl(&mut self, top: bool) -> PResult<Pattern> {
        let mut alts = vec![self.parse_pattern_primary(top)?];
        while self.eat(&Token::BitOr) {
            alts.push(self.parse_pattern_primary(top)?);
        }
        if alts.len() == 1 {
            Ok(alts.pop().expect("one alternative"))
        } else {
            Ok(Pattern::Or(alts))
        }
    }

    fn parse_pattern_primary(&mut self, top: bool) -> PResult<Pattern> {
        // Literal patterns: int / str / bool. Float is intentionally not a pattern.
        match self.peek() {
            Token::Int(n) => {
                let n = *n;
                self.advance();
                // `start..end` — a half-open integer range pattern (matches `start <= v < end`).
                if self.eat(&Token::DotDot) {
                    let end = self.expect_int()?;
                    return Ok(Pattern::Range { start: n, end });
                }
                return Ok(Pattern::Literal(LitPattern::Int(n)));
            }
            Token::Str(s) => {
                let s = s.clone();
                self.advance();
                return Ok(Pattern::Literal(LitPattern::Str(s)));
            }
            Token::True => {
                self.advance();
                return Ok(Pattern::Literal(LitPattern::Bool(true)));
            }
            Token::False => {
                self.advance();
                return Ok(Pattern::Literal(LitPattern::Bool(false)));
            }
            // A parenthesised group is a tuple pattern (gap #15).
            Token::LParen => return self.parse_tuple_pattern(),
            // `_` is an identifier; it's a wildcard unless followed by `(` (a payload, i.e. a
            // variant literally named `_`).
            Token::Ident(name) if name == "_" && self.peek_at(1) != &Token::LParen => {
                self.advance();
                return Ok(Pattern::Wildcard);
            }
            _ => {}
        }
        let name = self.expect_ident()?;
        // `Enum.Variant` — a qualified variant pattern. The first ident is the enum qualifier; the
        // ident after `.` is the variant. A qualified pattern is always a variant (never a binding),
        // even in a sub-position.
        let (name, enum_name) = if self.eat(&Token::Dot) {
            (self.expect_ident()?, Some(name))
        } else {
            (name, None)
        };
        if self.eat(&Token::LParen) {
            // `Name(p, …)` — a variant with (possibly nested) sub-patterns.
            let mut bindings = Vec::new();
            if !self.check(&Token::RParen) {
                loop {
                    bindings.push(self.parse_subpattern()?);
                    if !self.eat(&Token::Comma) {
                        break;
                    }
                }
            }
            self.expect(&Token::RParen)?;
            return Ok(Pattern::Variant { name, bindings, enum_name });
        }
        // A bare identifier: a nullary variant at the top of an arm, or a binding in a sub-position.
        // A qualified `Enum.Variant` is unambiguously a nullary variant in either position.
        if top || enum_name.is_some() {
            Ok(Pattern::Variant { name, bindings: Vec::new(), enum_name })
        } else {
            Ok(Pattern::Ident(name))
        }
    }

    /// Parse `( p1, p2, … )`. A single parenthesised pattern `(p)` is grouping (returns `p`);
    /// two-or-more elements form a `Tuple` matching a tuple value of that arity.
    fn parse_tuple_pattern(&mut self) -> PResult<Pattern> {
        self.expect(&Token::LParen)?;
        let mut elems = vec![self.parse_subpattern()?];
        while self.eat(&Token::Comma) {
            elems.push(self.parse_subpattern()?);
        }
        self.expect(&Token::RParen)?;
        if elems.len() == 1 {
            Ok(elems.pop().expect("one element"))
        } else {
            Ok(Pattern::Tuple(elems))
        }
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

    /// `yield <expr>` — always carries a value (unlike `return`). Experimental generator syntax.
    fn parse_yield(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Yield)?;
        Ok(StmtKind::Yield(self.parse_expr()?))
    }

    /// `assert <cond>` or `assert <cond>, <msg>` — line-oriented (the caller requires a stmt end).
    fn parse_assert(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Assert)?;
        let cond = self.parse_expr()?;
        let msg = if self.eat(&Token::Comma) {
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

    /// `spawn:` block (form 2) or `spawn <call>` (form 1). Form 1 must be a call expression (mirrors
    /// `defer`); a non-call is rejected with a clear message. Form 1 is line-oriented (terminated
    /// here); form 2 is compound (its block ends at its own `Dedent`).
    fn parse_spawn(&mut self) -> PResult<StmtKind> {
        self.expect(&Token::Spawn)?;
        if self.check(&Token::Colon) {
            let body = self.parse_block()?;
            return Ok(StmtKind::Spawn(SpawnTarget::Block(body)));
        }
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
            let lhs = self.parse_expr()?;
            let lhs_span = lhs.span;
            let target = if self.eat(&Token::Walrus) {
                match lhs.kind {
                    ExprKind::Ident(n) if n == "_" => WaitTarget::Discard,
                    ExprKind::Ident(n) => WaitTarget::Bind(n),
                    _ => {
                        return Err(ParseError {
                            message: "left side of ':=' in a wait arm must be a name".to_string(),
                            span: lhs_span,
                        })
                    }
                }
            } else if self.eat(&Token::Assign) {
                if matches!(lhs.kind, ExprKind::Ident(ref n) if n == "_") {
                    WaitTarget::Discard
                } else if matches!(
                    lhs.kind,
                    ExprKind::Ident(_) | ExprKind::Field { .. } | ExprKind::Index { .. }
                ) {
                    WaitTarget::Assign(lhs)
                } else {
                    return Err(ParseError {
                        message: "invalid wait-arm assignment target".to_string(),
                        span: lhs_span,
                    });
                }
            } else {
                return Err(self.err(
                    "a wait arm needs `:=` or `=` before the channel `recv`".to_string(),
                ));
            };
            // The RHS must be a bare `<chan>.recv()` — the thing a wait arm blocks on.
            let rhs = self.parse_expr()?;
            let rhs_span = rhs.span;
            let chan = match rhs.kind {
                ExprKind::Call { callee, args, named, .. }
                    if matches!(&callee.kind, ExprKind::Field { name, .. } if name == "recv") =>
                {
                    if !args.is_empty() || !named.is_empty() {
                        return Err(ParseError {
                            message: "`recv` in a wait arm takes no arguments".to_string(),
                            span: rhs_span,
                        });
                    }
                    match callee.kind {
                        ExprKind::Field { obj, .. } => *obj,
                        _ => unreachable!("guarded by the matches! above"),
                    }
                }
                _ => {
                    return Err(ParseError {
                        message: "a wait arm must `recv` from a channel (`v := ch.recv():`)"
                            .to_string(),
                        span: rhs_span,
                    })
                }
            };
            let body = self.parse_block()?;
            arms.push(WaitArm { target, chan, body, span: arm_span });
            self.skip_newlines();
        }
        self.expect(&Token::Dedent)?;
        if arms.is_empty() {
            return Err(self.err("`wait` needs at least one `recv` arm".to_string()));
        }
        Ok(StmtKind::Wait { arms, else_block })
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
        let mut path = vec![self.expect_path_segment()?];
        while self.eat(&Token::Dot) {
            path.push(self.expect_path_segment()?);
        }
        Ok(path)
    }

    /// A module-path segment: an identifier, or the `ref` keyword spelled as a path component (the
    /// `std.ref` module / `ref.chz` filename). `ref` is a binding-modifier keyword, but a module
    /// path is not a type position, so accepting it here keeps `import std.ref` working.
    fn expect_path_segment(&mut self) -> PResult<String> {
        if self.eat(&Token::Ref) {
            return Ok("ref".to_string());
        }
        self.expect_ident()
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
        // `fn(T1, …) -> R` — a function type in type position.
        if self.eat(&Token::Fn) {
            self.expect(&Token::LParen)?;
            let mut params = Vec::new();
            if !self.check(&Token::RParen) {
                params.push(self.parse_type()?);
                while self.eat(&Token::Comma) {
                    params.push(self.parse_type()?);
                }
            }
            self.expect(&Token::RParen)?;
            self.expect(&Token::Arrow)?;
            let ret = self.parse_type()?;
            let mut ty = Type::Func {
                params,
                ret: Box::new(ret),
            };
            ty = self.parse_type_postfix(ty)?;
            self.depth -= 1;
            return Ok(ty);
        }
        // `(T)` unwraps to `T`; `(T1, T2, …)` is a tuple type. The `?`/`!` postfix still applies.
        if self.eat(&Token::LParen) {
            let mut types = vec![self.parse_type()?];
            while self.eat(&Token::Comma) {
                types.push(self.parse_type()?);
            }
            self.expect(&Token::RParen)?;
            let mut ty = if types.len() == 1 {
                types.pop().unwrap()
            } else {
                Type::Tuple(types)
            };
            ty = self.parse_type_postfix(ty)?;
            self.depth -= 1;
            return Ok(ty);
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
        ty = self.parse_type_postfix(ty)?;
        self.depth -= 1;
        Ok(ty)
    }

    /// Postfix shorthand on a fully-parsed base type: `T?` = Option[T], `T!` = Result[T, Error],
    /// `T!E` = Result[T, E]. Stacks left-to-right (`T?!` = Result[Option[T], Error]).
    fn parse_type_postfix(&mut self, mut ty: Type) -> PResult<Type> {
        loop {
            if self.eat(&Token::Question) {
                ty = Type::Generic("Option".to_string(), vec![ty]);
            } else if self.eat(&Token::Bang) {
                // An explicit error type follows only if the next token can start one; otherwise
                // `T!` defaults the error type to `Error` (resolved later by the checker).
                if matches!(self.peek(), Token::Ident(_) | Token::LParen | Token::Fn) {
                    let err = self.parse_type()?;
                    ty = Type::Generic("Result".to_string(), vec![ty, err]);
                } else {
                    ty = Type::Generic("Result".to_string(), vec![ty]);
                }
            } else {
                break;
            }
        }
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
                InfixOp::Coalesce => Expr {
                    kind: ExprKind::NullCoalesce {
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    },
                    span,
                },
                // `lhs |> f(args)` desugars at parse time to `f(lhs, args)` — threading `lhs` as the
                // first argument. The RHS must be a call, so checker/interp/VM see a plain call and
                // need no pipe-specific code.
                InfixOp::Pipe => match rhs.kind {
                    ExprKind::Call { callee, args, named, type_args } => {
                        if !named.is_empty() {
                            return Err(self.err(
                                "named arguments are not supported on the right side of '|>'".to_string(),
                            ));
                        }
                        let mut new_args = Vec::with_capacity(args.len() + 1);
                        new_args.push(lhs);
                        new_args.extend(args);
                        Expr {
                            kind: ExprKind::Call { callee, args: new_args, named, type_args },
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
                    let (args, named) = self.parse_call_args()?;
                    Expr {
                        kind: ExprKind::Call {
                            callee: Box::new(e),
                            args,
                            named,
                            type_args: Vec::new(),
                        },
                        span,
                    }
                }
                Token::Dot => {
                    self.advance();
                    // `obj.field` (struct/module) or `tuple.0` (element access). A numeric field name
                    // is the tuple-element index, stored as its decimal string (reusing `Field`).
                    let name = if let Token::Int(n) = self.peek() {
                        let n = *n;
                        self.advance();
                        n.to_string()
                    } else {
                        self.expect_ident()?
                    };
                    // `.decode[Type](arg)` — the one type-argument call form (JSON decode). We
                    // SPECULATIVELY try to parse `[Type] (` after `.decode`; if that exact shape
                    // isn't present we backtrack and fall back to an ordinary field access (so
                    // `b.decode[1]` indexes a field named `decode`, `b.decode[i](x)` is index+call,
                    // etc.). Only `.decode[<type>](…)` is stolen.
                    let decode = if name == "decode" && self.check(&Token::LBracket) {
                        let save = self.pos;
                        let save_depth = self.depth; // `parse_type` leaks depth on a swallowed fail
                        self.advance(); // '['
                        match self.try_parse_decode_tail(e.clone(), span) {
                            Some(expr) => Some(expr),
                            None => {
                                self.pos = save; // restore — not a decode form
                                self.depth = save_depth;
                                None
                            }
                        }
                    } else {
                        None
                    };
                    match decode {
                        Some(expr) => expr,
                        None => Expr {
                            kind: ExprKind::Field {
                                obj: Box::new(e),
                                name,
                            },
                            span,
                        },
                    }
                }
                Token::LBracket => {
                    // `name[Type, …](args)` — explicit call-site type arguments. Only a bare name
                    // can be a generic fn / struct / variant, so only try the steal there; anything
                    // else (`arr[i]`, `obj.f[i]`) is a plain index. SPECULATIVE: if the `[types](`
                    // shape isn't present we backtrack and parse an index instead, so a numeric or
                    // non-type subscript (`fns[0](x)`) keeps its index+call meaning.
                    let type_call = if matches!(e.kind, ExprKind::Ident(_)) {
                        self.try_parse_type_arg_call(&e, span)?
                    } else {
                        None
                    };
                    match type_call {
                        Some(call) => call,
                        None => {
                            self.advance(); // '['
                            self.parse_subscript(e, span)?
                        }
                    }
                }
                Token::Question => {
                    self.advance();
                    Expr {
                        kind: ExprKind::Try(Box::new(e)),
                        span,
                    }
                }
                Token::QuestionDot => {
                    self.advance();
                    // `obj?.field` or `obj?.method(args)`. Field name mirrors `.` (ident or tuple
                    // index). A following `(` makes it an optional-chained method CALL — but only on
                    // a named method, never a tuple index (`t?.0()` is meaningless; the `(` there is
                    // left to a following postfix iteration, keeping the grammar and parser aligned).
                    let (name, is_ident) = if let Token::Int(n) = self.peek() {
                        let n = *n;
                        self.advance();
                        (n.to_string(), false)
                    } else {
                        (self.expect_ident()?, true)
                    };
                    let call = if is_ident && self.check(&Token::LParen) {
                        self.advance();
                        let (args, named) = self.parse_call_args()?;
                        Some(OptCall { args, named, type_args: Vec::new() })
                    } else {
                        None
                    };
                    Expr {
                        kind: ExprKind::OptChain { obj: Box::new(e), name, call },
                        span,
                    }
                }
                _ => break,
            };
        }
        Ok(e)
    }

    /// Parse the inside of a subscript `[ ... ]` (the `[` already consumed) and the closing `]`,
    /// producing either an `Index` (no `:`) or a `Slice` (one or two `:`). Grammar:
    /// `[ expr? ( ':' expr? ( ':' expr? )? )? ]`. The colon is unambiguous here — a map literal,
    /// type annotation, or match arm `:` never appears inside a subscript bracket — so a context-local
    /// colon parser replaces the old "parse one expr then inspect if it's a `..` Range" rewrite, and
    /// naturally extends to a third (step) component the Range form could not express.
    fn parse_subscript(&mut self, obj: Expr, span: Span) -> PResult<Expr> {
        // A component is present iff the next token is not `:` or `]`.
        let component = |p: &mut Self| -> PResult<Option<Box<Expr>>> {
            if p.check(&Token::Colon) || p.check(&Token::RBracket) {
                Ok(None)
            } else {
                Ok(Some(Box::new(p.parse_expr()?)))
            }
        };
        let start = component(self)?;
        // No colon → plain index. `xs[]` (empty) is rejected as a missing index.
        if !self.check(&Token::Colon) {
            self.expect(&Token::RBracket)?;
            let index = start.ok_or_else(|| ParseError {
                message: "expected an index expression".to_string(),
                span,
            })?;
            return Ok(Expr {
                kind: ExprKind::Index { obj: Box::new(obj), index },
                span,
            });
        }
        self.advance(); // first ':'
        let end = component(self)?;
        let step = if self.check(&Token::Colon) {
            self.advance(); // second ':'
            component(self)?
        } else {
            None
        };
        self.expect(&Token::RBracket)?;
        Ok(Expr {
            kind: ExprKind::Slice { obj: Box::new(obj), start, end, step },
            span,
        })
    }

    /// Parse a comma-separated argument list and the closing `)`, assuming the opening `(` has just
    /// been consumed. A trailing comma is not allowed (the loop breaks on a non-comma). Returns the
    /// positional args and the named args (`name = expr`) separately. A named argument is recognised
    /// by a bare `IDENT` immediately followed by a single `=` (`Token::Assign`, distinct from `==`).
    /// Once a named argument appears, every later argument must also be named.
    fn parse_call_args(&mut self) -> PResult<CallArgs> {
        let mut args = Vec::new();
        let mut named: Vec<(String, Expr)> = Vec::new();
        if !self.check(&Token::RParen) {
            loop {
                // `name = expr` — only when an IDENT is directly followed by a single `=`.
                if let Token::Ident(name) = self.peek()
                    && self.peek_at(1) == &Token::Assign
                {
                    let name = name.clone();
                    self.advance(); // ident
                    self.advance(); // '='
                    let value = self.parse_expr()?;
                    named.push((name, value));
                } else {
                    if !named.is_empty() {
                        return Err(
                            self.err("positional argument after named argument".to_string())
                        );
                    }
                    args.push(self.parse_expr()?);
                }
                if !self.eat(&Token::Comma) {
                    break;
                }
                // optional trailing comma: a `)` right after a comma ends the argument list
                if self.check(&Token::RParen) {
                    break;
                }
            }
        }
        self.expect(&Token::RParen)?;
        Ok((args, named))
    }

    /// Speculatively parse `[Type, …](args)` as a call with explicit type arguments, assuming the
    /// `[` has NOT yet been consumed and `callee` is a bare name. Returns `Ok(None)` (position
    /// restored) when the `[types](` shape isn't present, so the caller falls back to indexing
    /// (`arr[i]`, `fns[0](x)`). Once `]` `(` is confirmed it commits, so a genuine error inside the
    /// argument list propagates instead of silently re-parsing as an index.
    fn try_parse_type_arg_call(&mut self, callee: &Expr, span: Span) -> PResult<Option<Expr>> {
        let save = self.pos;
        // `parse_type` bumps `self.depth` and only unwinds it on success; on a swallowed (backtrack)
        // failure we must restore depth too, else every plain `name[idx]` leaks a level and many
        // such indexes spuriously trip MAX_DEPTH.
        let save_depth = self.depth;
        self.advance(); // '['
        let mut type_args = Vec::new();
        loop {
            match self.parse_type() {
                Ok(t) => type_args.push(t),
                Err(_) => {
                    self.pos = save;
                    self.depth = save_depth;
                    return Ok(None);
                }
            }
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        if !self.eat(&Token::RBracket) || !self.check(&Token::LParen) {
            self.pos = save;
            self.depth = save_depth;
            return Ok(None);
        }
        self.advance(); // '(' — committed to a type-argument call now.
        let (args, named) = self.parse_call_args()?;
        if !named.is_empty() {
            return Err(
                self.err("named arguments are not supported with explicit type arguments".to_string())
            );
        }
        Ok(Some(Expr {
            kind: ExprKind::Call { callee: Box::new(callee.clone()), args, named, type_args },
            span,
        }))
    }

    /// Speculatively parse the tail of a `.decode[Type](arg)` form, assuming the opening `[` has
    /// just been consumed. Returns `None` (so the caller backtracks to a plain field access) if the
    /// exact `<type> ] ( <expr> )` shape isn't present — e.g. `b.decode[1]` indexes a field.
    fn try_parse_decode_tail(&mut self, obj: Expr, span: Span) -> Option<Expr> {
        let ty = self.parse_type().ok()?;
        if !self.eat(&Token::RBracket) || !self.eat(&Token::LParen) {
            return None;
        }
        let arg = self.parse_expr().ok()?;
        if !self.eat(&Token::RParen) {
            return None;
        }
        Some(Expr {
            kind: ExprKind::DecodeCall { obj: Box::new(obj), ty, arg: Box::new(arg) },
            span,
        })
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        let tok = self.advance();
        let span = tok.span;
        let kind = match tok.kind {
            Token::Int(n) => ExprKind::Int(n),
            Token::Float(f) => ExprKind::Float(f),
            Token::Str(s) => ExprKind::Str(s),
            Token::Bytes(b) => ExprKind::Bytes(b),
            Token::True => ExprKind::Bool(true),
            Token::False => ExprKind::Bool(false),
            Token::Ident(name) => ExprKind::Ident(name),
            Token::LParen => {
                // `()` stays unchanged (no inner expr → falls through to the error path below);
                // `(e)` is grouping; `(e1, e2, …)` is a tuple; `(e,)` is a parse error.
                if self.check(&Token::RParen) {
                    return Err(self.err("unexpected ')' in expression".to_string()));
                }
                let first = self.parse_expr()?;
                if self.eat(&Token::Comma) {
                    // A comma after the first element ⇒ a tuple. `(e,)` is a 1-element tuple
                    // (distinct from grouping `(e)`); `(e1, e2,)` allows an optional trailing comma.
                    let mut elems = vec![first];
                    // The just-eaten comma may be the trailing one (`(e,)` or `(…,)`): stop here.
                    while !self.check(&Token::RParen) {
                        elems.push(self.parse_expr()?);
                        if !self.eat(&Token::Comma) {
                            break;
                        }
                    }
                    self.expect(&Token::RParen)?;
                    ExprKind::Tuple(elems)
                } else {
                    self.expect(&Token::RParen)?;
                    return Ok(first); // grouped — keep the inner expr (and its span)
                }
            }
            Token::LBracket => {
                // `[]` is the empty list. A first element followed by `for` is a list
                // comprehension (`[elem for x in xs if g]`); otherwise a plain list literal.
                if self.check(&Token::RBracket) {
                    self.advance();
                    ExprKind::List(Vec::new())
                } else {
                    let first = self.parse_expr()?;
                    if self.check(&Token::For) {
                        let clauses = self.parse_comp_clauses()?;
                        self.expect(&Token::RBracket)?;
                        ExprKind::Comprehension {
                            kind: CompKind::List,
                            key: None,
                            elem: Box::new(first),
                            clauses,
                        }
                    } else {
                        let mut elems = vec![first];
                        while self.eat(&Token::Comma) {
                            // optional trailing comma: a `]` right after a comma ends the literal
                            if self.check(&Token::RBracket) {
                                break;
                            }
                            elems.push(self.parse_expr()?);
                        }
                        self.expect(&Token::RBracket)?;
                        ExprKind::List(elems)
                    }
                }
            }
            Token::LBrace => {
                // `{}` is the empty map. Otherwise the first element decides: a `key: value` pair
                // makes it a map (or, before `for`, a map comprehension); a bare expression makes
                // it a set (or a set comprehension before `for`). The empty set is `set()`.
                if self.check(&Token::RBrace) {
                    self.advance();
                    ExprKind::Map(Vec::new())
                } else {
                    let first = self.parse_expr()?;
                    if self.eat(&Token::Colon) {
                        let value = self.parse_expr()?;
                        if self.check(&Token::For) {
                            // Map comprehension: `{k: v for k, v in m}`.
                            let clauses = self.parse_comp_clauses()?;
                            self.expect(&Token::RBrace)?;
                            ExprKind::Comprehension {
                                kind: CompKind::Map,
                                key: Some(Box::new(first)),
                                elem: Box::new(value),
                                clauses,
                            }
                        } else {
                            // Map literal: finish the first pair, then the rest.
                            let mut entries = vec![(first, value)];
                            while self.eat(&Token::Comma) {
                                // optional trailing comma: a `}` right after a comma ends the map
                                if self.check(&Token::RBrace) {
                                    break;
                                }
                                let key = self.parse_expr()?;
                                self.expect(&Token::Colon)?;
                                let value = self.parse_expr()?;
                                entries.push((key, value));
                            }
                            self.expect(&Token::RBrace)?;
                            ExprKind::Map(entries)
                        }
                    } else if self.check(&Token::For) {
                        // Set comprehension: `{x for x in xs}`.
                        let clauses = self.parse_comp_clauses()?;
                        self.expect(&Token::RBrace)?;
                        ExprKind::Comprehension {
                            kind: CompKind::Set,
                            key: None,
                            elem: Box::new(first),
                            clauses,
                        }
                    } else {
                        // Set literal: a comma-separated list of elements.
                        let mut elems = vec![first];
                        while self.eat(&Token::Comma) {
                            // optional trailing comma: a `}` right after a comma ends the set
                            if self.check(&Token::RBrace) {
                                break;
                            }
                            elems.push(self.parse_expr()?);
                        }
                        self.expect(&Token::RBrace)?;
                        ExprKind::Set(elems)
                    }
                }
            }
            Token::Fn => return self.parse_closure(span),
            // Expression-position `match`/`if` (the keyword was already consumed by `advance`).
            // Statement-position `if`/`match` never reach here — `parse_stmt` dispatches them first.
            Token::Match => return self.parse_match_expr(span),
            Token::If => return self.parse_if_expr(span),
            Token::Recover => return self.parse_recover_expr(span),
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
        let params = self.parse_params(false)?;
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

/// Does this function body contain a `yield` that belongs to *this* function? Recurses through
/// compound statements' sub-blocks but deliberately does NOT descend into a nested `fn`
/// definition (its yields are its own) nor into closure expressions (a closure is an expression,
/// so it is never reached by this statement-only walk — a `yield` inside one stays invisible here
/// and is later flagged by the checker as "yield outside a generator").
fn body_contains_yield(block: &Block) -> bool {
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
        StmtKind::If { branches, else_block } => {
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
        _ => false,
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
        LBrace => "'{'",
        RBrace => "'}'",
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
        Bytes(_) => "a byte-string literal",
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
    /// Null-coalescing `??` — right-associative carrier, lowered to a `match` by the desugar pass.
    Coalesce,
}

/// Map a token to its infix operator and (left, right) binding powers, per `docs/syntax.md` §4.
/// All operators are left-associative (`right = left + 1`). `None` means "not an infix operator".
/// True for the compound-assignment tokens (`+= -= *= /= %= &= |= ^= <<= >>=`). Used to give a
/// clear error when one appears with a multi-target list (`a, b += 1`).
fn is_compound_assign(tok: &Token) -> bool {
    matches!(
        tok,
        Token::PlusEq
            | Token::MinusEq
            | Token::StarEq
            | Token::SlashEq
            | Token::PercentEq
            | Token::AmpEq
            | Token::PipeEq
            | Token::CaretEq
            | Token::ShlEq
            | Token::ShrEq
    )
}

fn infix_op(tok: &Token) -> Option<(InfixOp, u8, u8)> {
    use BinaryOp::*;
    use InfixOp::*;
    // Precedence ladder (loosest → tightest). Bitwise ops follow Python's relative order: comparison
    // is looser than `|` < `^` < `&` < shifts, and shifts are looser than additive (gap #13).
    let (op, l) = match tok {
        // Pipe `|>` is the lowest-precedence infix op (level 1): the whole expression to its left
        // is threaded into the call on its right. Left-associative (`a |> f |> g` = `(a|>f)|>g`).
        Token::Pipe => (Pipe, 1),
        Token::Or => (Bin(Or), 3),
        // Null-coalescing `??`: looser than `and`/comparisons, tighter than `or`. RIGHT-associative
        // (`a ?? b ?? c` = `a ?? (b ?? c)`) — the one exception to the `l+1` left-assoc rule below,
        // so it returns early with equal left/right binding powers.
        Token::QuestionQuestion => return Some((Coalesce, 4, 4)),
        Token::And => (Bin(And), 5),
        Token::EqEq => (Bin(Eq), 7),
        Token::NotEq => (Bin(NotEq), 7),
        // `in` membership — comparison-level precedence (same as `==`). `for x in xs:` never
        // reaches here: `parse_for`/`parse_comp_clause` consume `in` explicitly via `expect`.
        Token::In => (Bin(In), 7),
        Token::Lt => (Bin(Lt), 9),
        Token::LtEq => (Bin(LtEq), 9),
        Token::Gt => (Bin(Gt), 9),
        Token::GtEq => (Bin(GtEq), 9),
        Token::BitOr => (Bin(BitOr), 11),
        Token::Caret => (Bin(BitXor), 13),
        Token::Amp => (Bin(BitAnd), 15),
        Token::Shl => (Bin(Shl), 17),
        Token::Shr => (Bin(Shr), 17),
        Token::DotDot => (Range, 19),
        Token::Plus => (Bin(Add), 21),
        Token::Minus => (Bin(Sub), 21),
        Token::Star => (Bin(Mul), 23),
        Token::Slash => (Bin(Div), 23),
        Token::Percent => (Bin(Mod), 23),
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
    fn assert_no_msg_parses() {
        match only("assert x == 1\n") {
            StmtKind::Assert { cond, msg } => {
                assert!(matches!(cond.kind, ExprKind::Binary { .. }));
                assert!(msg.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn assert_with_msg_parses() {
        match only("assert x, \"boom\"\n") {
            StmtKind::Assert { cond, msg } => {
                assert!(matches!(cond.kind, ExprKind::Ident(_)));
                assert!(matches!(msg.unwrap().kind, ExprKind::Str(_)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn free_test_fn_parses_with_is_test() {
        match only("test fn t():\n    assert true\n") {
            StmtKind::Fn(decl) => {
                assert!(decl.is_test, "free `test fn` should set is_test");
                assert_eq!(decl.name, "t");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn plain_fn_is_not_test() {
        match only("fn t():\n    assert true\n") {
            StmtKind::Fn(decl) => assert!(!decl.is_test),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn struct_test_method_parses_with_is_test() {
        match only("struct S:\n    test fn t(self):\n        assert true\n    fn helper(self):\n        return\n") {
            StmtKind::Struct { methods, .. } => {
                let t = methods.iter().find(|m| m.name == "t").unwrap();
                assert!(t.is_test, "struct `test fn` should set is_test");
                let h = methods.iter().find(|m| m.name == "helper").unwrap();
                assert!(!h.is_test);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tuple_swap_parses() {
        match only("a, b = b, a\n") {
            StmtKind::Assign { target, op, value } => {
                assert_eq!(op, AssignOp::Eq);
                match (&target.kind, &value.kind) {
                    (ExprKind::Tuple(ts), ExprKind::Tuple(vs)) => {
                        assert_eq!(ts.len(), 2);
                        assert_eq!(vs.len(), 2);
                    }
                    other => panic!("expected Tuple targets/values, got {other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn tuple_index_swap_parses() {
        // index/field targets on the left — must NOT become a bare Expr statement.
        match only("data[0], data[1] = data[1], data[0]\n") {
            StmtKind::Assign { target, op, .. } => {
                assert_eq!(op, AssignOp::Eq);
                assert!(matches!(target.kind, ExprKind::Tuple(_)));
            }
            other => panic!("expected tuple-target Assign, got {other:?}"),
        }
    }

    #[test]
    fn tuple_compound_rejected() {
        // compound op with multiple targets is a clean parse error.
        let e = parse_err("a, b += 1\n");
        assert!(
            e.message.contains("compound assignment") || e.message.contains("multiple targets"),
            "got: {}",
            e.message
        );
    }

    #[test]
    fn tuple_arity_mismatch_rejected() {
        let e = parse_err("a, b = 1, 2, 3\n");
        assert!(e.message.contains("target") || e.message.contains("value"), "got: {}", e.message);
    }

    #[test]
    fn destructuring_let_still_walrus() {
        // `a, b := pair()` stays a destructuring Let (not tuple-assign).
        match only("a, b := pair()\n") {
            StmtKind::Let { names, .. } => assert_eq!(names.len(), 2),
            other => panic!("expected Let, got {other:?}"),
        }
    }

    #[test]
    fn in_expr_parses_as_binary() {
        match only("x := 1 in xs\n") {
            StmtKind::Let { value, .. } => match value.kind {
                ExprKind::Binary { op: BinaryOp::In, .. } => {}
                other => panic!("expected Binary(In), got {other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn for_in_still_parses() {
        // `in` as an operator must NOT break `for x in xs:` or `for x in 0..3:`.
        match only("for x in xs:\n    print(x)\n") {
            StmtKind::For { .. } => {}
            other => panic!("expected For, got {other:?}"),
        }
        match only("for x in 0..3:\n    print(x)\n") {
            StmtKind::For { .. } => {}
            other => panic!("expected For, got {other:?}"),
        }
    }

    #[test]
    fn compound_assign_parses() {
        // `*= /= %= &= |= ^= <<= >>=` each lower to a distinct AssignOp.
        let cases = [
            ("x *= 2\n", AssignOp::StarEq),
            ("x /= 2\n", AssignOp::SlashEq),
            ("x %= 2\n", AssignOp::PercentEq),
            ("x &= 2\n", AssignOp::AmpEq),
            ("x |= 2\n", AssignOp::PipeEq),
            ("x ^= 2\n", AssignOp::CaretEq),
            ("x <<= 2\n", AssignOp::ShlEq),
            ("x >>= 2\n", AssignOp::ShrEq),
        ];
        for (src, want) in cases {
            match only(src) {
                StmtKind::Assign { op, .. } => assert_eq!(op, want, "for {src:?}"),
                other => panic!("{src:?} -> {other:?}"),
            }
        }
    }

    #[test]
    fn parses_extern_block() {
        match only("extern \"libm.so.6\":\n    fn cos(x: float) -> float\n    fn sqrt(x: float) -> float\n") {
            StmtKind::Extern { lib, fns } => {
                assert_eq!(lib, "libm.so.6");
                assert_eq!(fns.len(), 2);
                assert_eq!(fns[0].name, "cos");
                assert_eq!(fns[0].params.len(), 1);
                assert_eq!(fns[0].params[0].name, "x");
                assert_eq!(fns[0].params[0].ty, Some(Type::Named("float".to_string())));
                assert_eq!(fns[0].ret, Some(Type::Named("float".to_string())));
                assert_eq!(fns[1].name, "sqrt");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_extern_void_return() {
        match only("extern \"libc.so.6\":\n    fn srand(seed: int)\n") {
            StmtKind::Extern { lib, fns } => {
                assert_eq!(lib, "libc.so.6");
                assert_eq!(fns.len(), 1);
                assert_eq!(fns[0].name, "srand");
                assert_eq!(fns[0].ret, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn extern_braces_is_error() {
        // Chezzi has no brace blocks; `extern "x" {` must not parse.
        parse_err("extern \"libc.so.6\" {\n    fn strlen(s: str) -> int\n}\n");
    }

    #[test]
    fn extern_requires_string_lib() {
        parse_err("extern libc:\n    fn strlen(s: str) -> int\n");
    }

    #[test]
    fn parses_parallel_with_spawn_call() {
        match only("parallel:\n    spawn worker(1)\n    spawn worker(2)\n") {
            StmtKind::Parallel { body } => {
                assert_eq!(body.len(), 2);
                assert!(matches!(body[0].kind, StmtKind::Spawn(SpawnTarget::Call(_))));
                assert!(matches!(body[1].kind, StmtKind::Spawn(SpawnTarget::Call(_))));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_spawn_block_form() {
        match only("parallel:\n    spawn:\n        x := 1\n        f(x)\n") {
            StmtKind::Parallel { body } => match &body[0].kind {
                StmtKind::Spawn(SpawnTarget::Block(b)) => assert_eq!(b.len(), 2),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn spawn_non_call_form1_rejected() {
        assert!(parse_err("parallel:\n    spawn x + 1\n")
            .message
            .contains("spawn requires a function or method call"));
    }

    #[test]
    fn parses_yield_statement() {
        match only("fn g() -> Iterator[int]:\n    yield 1\n") {
            StmtKind::Fn(d) => {
                assert!(d.is_generator, "fn with yield must be a generator");
                assert!(matches!(d.body[0].kind, StmtKind::Yield(_)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn yield_in_nested_block_marks_generator() {
        match only("fn g() -> Iterator[int]:\n    for i in 0..3:\n        yield i\n") {
            StmtKind::Fn(d) => assert!(d.is_generator),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn yield_inside_recover_block_marks_generator() {
        // A `yield` reachable only through a `recover:` expression block still makes the fn a
        // generator (the detection walk descends into recover blocks).
        match only("fn g() -> Iterator[int]:\n    x := recover:\n        yield 1\n        1\n    print(x)\n") {
            StmtKind::Fn(d) => assert!(d.is_generator, "yield in a recover: block must mark the fn a generator"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn fn_without_yield_is_not_generator() {
        match only("fn f() -> int:\n    return 1\n") {
            StmtKind::Fn(d) => assert!(!d.is_generator),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn yield_in_nested_fn_does_not_mark_outer() {
        // The inner `fn` owns its yield; the outer must NOT be flagged a generator.
        let src = "fn outer() -> int:\n    fn inner() -> Iterator[int]:\n        yield 1\n    return 0\n";
        match only(src) {
            StmtKind::Fn(d) => {
                assert!(!d.is_generator, "outer fn must not be a generator");
                match &d.body[0].kind {
                    StmtKind::Fn(inner) => assert!(inner.is_generator, "inner fn is the generator"),
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_wait_with_bind_assign_discard_and_else() {
        let src = "wait:\n    v := orders.recv(): handle(v)\n    result = cancels.recv(): result = 1\n    _ := timer(500).recv(): on_timeout()\n    else: poll_miss()\n";
        match only(src) {
            StmtKind::Wait { arms, else_block } => {
                assert_eq!(arms.len(), 3);
                assert!(matches!(arms[0].target, WaitTarget::Bind(ref n) if n == "v"));
                assert!(matches!(&arms[0].chan.kind, ExprKind::Ident(n) if n == "orders"));
                assert!(matches!(arms[1].target, WaitTarget::Assign(_)));
                assert!(matches!(arms[2].target, WaitTarget::Discard));
                // a timer arm's channel expr is the `timer(500)` call, evaluated once.
                assert!(matches!(&arms[2].chan.kind, ExprKind::Call { .. }));
                assert!(else_block.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_wait_indented_arm_bodies() {
        let src = "wait:\n    v := ch.recv():\n        print(v)\n        f(v)\n";
        match only(src) {
            StmtKind::Wait { arms, else_block } => {
                assert_eq!(arms.len(), 1);
                assert_eq!(arms[0].body.len(), 2);
                assert!(else_block.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn wait_arm_non_recv_rhs_rejected() {
        assert!(parse_err("wait:\n    v := f(): g()\n")
            .message
            .contains("must `recv` from a channel"));
    }

    #[test]
    fn wait_arm_recv_with_args_rejected() {
        assert!(parse_err("wait:\n    v := ch.recv(5): g()\n")
            .message
            .contains("takes no arguments"));
    }

    #[test]
    fn wait_else_must_be_last() {
        assert!(parse_err("wait:\n    else: a()\n    v := ch.recv(): b()\n")
            .message
            .contains("must be the last arm"));
    }

    #[test]
    fn wait_requires_at_least_one_arm() {
        assert!(parse_err("wait:\n    else: a()\n")
            .message
            .contains("at least one `recv` arm"));
    }

    #[test]
    fn parses_null_coalesce_right_assoc() {
        // `a ?? b ?? c` = `a ?? (b ?? c)`.
        match let_value("x := a ?? b ?? c\n").kind {
            ExprKind::NullCoalesce { lhs, rhs } => {
                assert!(matches!(lhs.kind, ExprKind::Ident(ref n) if n == "a"));
                assert!(matches!(rhs.kind, ExprKind::NullCoalesce { .. }), "rhs must nest");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_opt_chain_field() {
        match let_value("x := y?.f\n").kind {
            ExprKind::OptChain { obj, name, call } => {
                assert!(matches!(obj.kind, ExprKind::Ident(ref n) if n == "y"));
                assert_eq!(name, "f");
                assert!(call.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_opt_chain_method() {
        match let_value("x := y?.f(1, 2)\n").kind {
            ExprKind::OptChain { name, call, .. } => {
                assert_eq!(name, "f");
                assert_eq!(call.expect("a call").args.len(), 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn opt_chain_chains_left_assoc() {
        // `y?.a?.b` = `(y?.a)?.b`.
        match let_value("x := y?.a?.b\n").kind {
            ExprKind::OptChain { obj, name, .. } => {
                assert_eq!(name, "b");
                assert!(matches!(obj.kind, ExprKind::OptChain { .. }), "obj must be the inner chain");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn opt_chain_binds_tighter_than_coalesce() {
        // `y?.f ?? z` = `(y?.f) ?? z`.
        match let_value("x := y?.f ?? z\n").kind {
            ExprKind::NullCoalesce { lhs, rhs } => {
                assert!(matches!(lhs.kind, ExprKind::OptChain { .. }));
                assert!(matches!(rhs.kind, ExprKind::Ident(ref n) if n == "z"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn generic_fn_decl_with_bound() {
        match only("fn max[T: Comparable](a: T, b: T) -> T:\n    return a\n") {
            StmtKind::Fn(f) => {
                assert_eq!(f.name, "max");
                assert_eq!(f.type_params.len(), 1);
                assert_eq!(f.type_params[0].name, "T");
                assert_eq!(
                    f.type_params[0].bounds,
                    vec![Bound { name: "Comparable".into(), args: vec![] }]
                );
                assert_eq!(f.params.len(), 2);
                assert_eq!(f.ret, Some(Type::Named("T".into())));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn generic_fn_decl_parameterized_bound() {
        // `S: Iterator[T]` — a parameterized protocol bound records the bound name + its type args.
        match only("fn collect[S: Iterator[T], T](xs: S) -> T:\n    return xs\n") {
            StmtKind::Fn(f) => {
                assert_eq!(f.type_params.len(), 2);
                assert_eq!(f.type_params[0].name, "S");
                assert_eq!(
                    f.type_params[0].bounds,
                    vec![Bound { name: "Iterator".into(), args: vec![Type::Named("T".into())] }]
                );
                assert_eq!(f.type_params[1].name, "T");
                assert_eq!(f.type_params[1].bounds, Vec::<Bound>::new());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn duplicate_type_param_rejected() {
        assert!(parse_err("fn f[T, T](a: T) -> T:\n    return a\n")
            .message
            .contains("duplicate type parameter"));
        assert!(parse_err("struct S[T, T]:\n    x: T\n")
            .message
            .contains("duplicate type parameter"));
        assert!(parse_err("enum E[T, T]:\n    A(T)\n")
            .message
            .contains("duplicate type parameter"));
        // distinct names still parse fine
        parse_ok("fn f[T, U](a: T, b: U) -> T:\n    return a\n");
    }

    #[test]
    fn generic_fn_decl_multi_param_unbounded() {
        match only("fn pair[A, B](a: A, b: B):\n    print(a)\n") {
            StmtKind::Fn(f) => {
                assert_eq!(f.type_params.len(), 2);
                assert_eq!(f.type_params[0].name, "A");
                assert_eq!(f.type_params[0].bounds, Vec::<Bound>::new());
                assert_eq!(f.type_params[1].name, "B");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn generic_struct_decl() {
        match only("struct Pair[A, B]:\n    first: A\n    second: B\n") {
            StmtKind::Struct { name, type_params, fields, .. } => {
                assert_eq!(name, "Pair");
                assert_eq!(type_params.len(), 2);
                assert_eq!(type_params[0].name, "A");
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[0].ty, Type::Named("A".into()));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn protocol_decl_collects_method_sigs() {
        match only("protocol Comparable:\n    fn compare(self, other: Self) -> int\n") {
            StmtKind::Protocol { name, methods, .. } => {
                assert_eq!(name, "Comparable");
                assert_eq!(methods.len(), 1);
                assert_eq!(methods[0].name, "compare");
                assert_eq!(methods[0].params.len(), 2);
                assert_eq!(methods[0].params[1].ty, Some(Type::Named("Self".into())));
                assert_eq!(methods[0].ret, Some(Type::Named("int".into())));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn method_param_default_parses() {
        let src = "struct P:\n    n: int\n    fn add(self, x: int = 5) -> int:\n        return self.n + x\n";
        match only(src) {
            StmtKind::Struct { methods, .. } => {
                assert_eq!(methods.len(), 1);
                assert_eq!(methods[0].params.len(), 2);
                assert!(methods[0].params[1].default.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parameterized_protocol_decl_collects_type_params() {
        match only("protocol Container[T]:\n    fn get(self, i: int) -> T\n") {
            StmtKind::Protocol { name, type_params, methods } => {
                assert_eq!(name, "Container");
                assert_eq!(type_params.len(), 1);
                assert_eq!(type_params[0].name, "T");
                assert_eq!(methods.len(), 1);
                assert_eq!(methods[0].ret, Some(Type::Named("T".into())));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn protocol_with_multiple_method_sigs() {
        let src = "protocol Shape:\n    fn area(self) -> float\n    fn name(self) -> str\n";
        match only(src) {
            StmtKind::Protocol { methods, .. } => assert_eq!(methods.len(), 2),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn walrus_let() {
        match only("x := 5\n") {
            StmtKind::Let { names, ty, value, .. } => {
                assert_eq!(names, vec!["x".to_string()]);
                assert!(ty.is_none());
                assert_eq!(value.kind, ExprKind::Int(5));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn break_and_continue_stmts() {
        assert_eq!(only("break\n"), StmtKind::Break);
        assert_eq!(only("continue\n"), StmtKind::Continue);
    }

    #[test]
    fn break_continue_inline_in_loop() {
        // `for i in 0..3: break` — inline block body is a single simple statement.
        let m = parse_ok("for i in 0..3: break\n");
        assert_eq!(m.stmts.len(), 1);
        match &m.stmts[0].kind {
            StmtKind::For { body, .. } => assert_eq!(body[0].kind, StmtKind::Break),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn typed_let() {
        match only("name: str = \"thuan\"\n") {
            StmtKind::Let { names, ty, value, .. } => {
                assert_eq!(names, vec!["name".to_string()]);
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
            StmtKind::Struct { name, fields, methods, .. } => {
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
            StmtKind::Enum { name, type_params, variants } => {
                assert_eq!(name, "Shape");
                assert!(type_params.is_empty());
                assert_eq!(variants[0].name, "Circle");
                assert_eq!(variants[0].payload, vec![Type::Named("int".into())]);
                assert_eq!(variants[1].name, "Point");
                assert!(variants[1].payload.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn generic_enum_parses_type_params() {
        match only("enum Tree[T]:\n    Leaf\n    Node(T, Tree[T], Tree[T])\n") {
            StmtKind::Enum { name, type_params, variants } => {
                assert_eq!(name, "Tree");
                assert_eq!(type_params.len(), 1);
                assert_eq!(type_params[0].name, "T");
                assert_eq!(variants[0].name, "Leaf");
                assert_eq!(variants[1].name, "Node");
                assert_eq!(variants[1].payload.len(), 3);
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
            StmtKind::For { vars, iter, .. } => {
                assert_eq!(vars, vec!["i".to_string()]);
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

    /// Helper: is this Slice-component a literal int `n`?
    fn is_int(c: &Option<Box<Expr>>, n: i64) -> bool {
        matches!(c.as_deref(), Some(Expr { kind: ExprKind::Int(v), .. }) if *v == n)
    }

    #[test]
    fn subscript_colon_parses_slice() {
        // 0 colons → Index
        match let_value("y := xs[2]\n").kind {
            ExprKind::Index { .. } => {}
            other => panic!("expected Index, got {other:?}"),
        }
        // xs[1:3] → Slice{Some(1), Some(3), None}
        match let_value("y := xs[1:3]\n").kind {
            ExprKind::Slice { start, end, step, .. } => {
                assert!(is_int(&start, 1) && is_int(&end, 3) && step.is_none());
            }
            other => panic!("expected Slice, got {other:?}"),
        }
        // xs[1:] → {Some, None, None}
        match let_value("y := xs[1:]\n").kind {
            ExprKind::Slice { start, end, step, .. } => {
                assert!(is_int(&start, 1) && end.is_none() && step.is_none());
            }
            other => panic!("expected Slice, got {other:?}"),
        }
        // xs[:3] → {None, Some, None}
        match let_value("y := xs[:3]\n").kind {
            ExprKind::Slice { start, end, step, .. } => {
                assert!(start.is_none() && is_int(&end, 3) && step.is_none());
            }
            other => panic!("expected Slice, got {other:?}"),
        }
        // xs[:] → all None
        match let_value("y := xs[:]\n").kind {
            ExprKind::Slice { start, end, step, .. } => {
                assert!(start.is_none() && end.is_none() && step.is_none());
            }
            other => panic!("expected Slice, got {other:?}"),
        }
        // xs[1:5:2] → {Some, Some, Some}
        match let_value("y := xs[1:5:2]\n").kind {
            ExprKind::Slice { start, end, step, .. } => {
                assert!(is_int(&start, 1) && is_int(&end, 5) && is_int(&step, 2));
            }
            other => panic!("expected Slice, got {other:?}"),
        }
        // xs[::-1] → {None, None, Some(-1)}  (unary minus on 1)
        match let_value("y := xs[::-1]\n").kind {
            ExprKind::Slice { start, end, step, .. } => {
                assert!(start.is_none() && end.is_none());
                assert!(matches!(step.as_deref(), Some(Expr { kind: ExprKind::Unary { .. }, .. })));
            }
            other => panic!("expected Slice, got {other:?}"),
        }
    }

    #[test]
    fn dotdot_still_range_outside_subscript() {
        // for-loop iterable stays a Range
        match only("for i in 0..10:\n    print(i)\n") {
            StmtKind::For { iter, .. } => assert!(matches!(iter.kind, ExprKind::Range { .. })),
            other => panic!("{other:?}"),
        }
        // match range-pattern still parses (0..10 => ...)
        assert!(parse(lexer::tokenize("fn f(x: int):\n    match x:\n        0..10: print(1)\n        _: print(2)\n").unwrap()).is_ok());
    }

    #[test]
    fn slice_is_not_an_lvalue() {
        // A slice is not in the lvalue grammar — `xs[1:3] = v` must be a parse error.
        let e = parse_err("xs := [1, 2, 3]\nxs[1:3] = [9]\n");
        assert!(e.message.contains("invalid assignment target"), "got: {}", e.message);
    }

    #[test]
    fn fns_index_then_call_still_parses() {
        // fns[0](x) must remain index-then-call, not a slice.
        match let_value("y := fns[0](x)\n").kind {
            ExprKind::Call { callee, .. } => {
                assert!(matches!(callee.kind, ExprKind::Index { .. }));
            }
            other => panic!("expected Call over Index, got {other:?}"),
        }
    }

    #[test]
    fn match_inline_arms() {
        match only("match s:\n    Circle(r): return r\n    Square(n): return n\n    Point: return 0\n") {
            StmtKind::Match { arms, .. } => {
                assert_eq!(arms.len(), 3);
                match &arms[0].pattern {
                    Pattern::Variant { name, bindings, .. } => {
                        assert_eq!(name, "Circle");
                        assert_eq!(bindings, &vec![Pattern::Ident("r".to_string())]);
                    }
                    other => panic!("{other:?}"),
                }
                assert!(
                    arms[2].pattern
                        == Pattern::Variant { name: "Point".into(), bindings: vec![], enum_name: None }
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn match_literal_and_wildcard_patterns() {
        match only("match n:\n    0: return 1\n    \"a\": return 2\n    true: return 3\n    _: return 4\n") {
            StmtKind::Match { arms, .. } => {
                assert_eq!(arms.len(), 4);
                assert_eq!(arms[0].pattern, Pattern::Literal(LitPattern::Int(0)));
                assert_eq!(arms[1].pattern, Pattern::Literal(LitPattern::Str("a".into())));
                assert_eq!(arms[2].pattern, Pattern::Literal(LitPattern::Bool(true)));
                assert_eq!(arms[3].pattern, Pattern::Wildcard);
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
    fn defer_parses_call_in_fn_body() {
        let StmtKind::Fn(f) = only("fn f():\n    defer cleanup()\n    defer obj.close(1)\n") else {
            panic!()
        };
        assert!(matches!(f.body[0].kind, StmtKind::Defer(_)));
        let StmtKind::Defer(DeferTarget::Call(Expr { kind: ExprKind::Call { .. }, .. })) =
            &f.body[0].kind
        else {
            panic!("first defer not a call")
        };
        let StmtKind::Defer(DeferTarget::Call(Expr { kind: ExprKind::Call { callee, .. }, .. })) =
            &f.body[1].kind
        else {
            panic!("second defer not a call")
        };
        assert!(matches!(callee.kind, ExprKind::Field { .. }));
    }

    #[test]
    fn defer_parses_block_form() {
        let StmtKind::Fn(f) = only("fn f():\n    defer:\n        foo()\n        bar()\n") else {
            panic!()
        };
        let StmtKind::Defer(DeferTarget::Block(body)) = &f.body[0].kind else {
            panic!("defer: not parsed as a block")
        };
        assert_eq!(body.len(), 2);
        assert!(matches!(body[0].kind, StmtKind::Expr(_)));
        assert!(matches!(body[1].kind, StmtKind::Expr(_)));
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
    fn index_vs_slice() {
        // xs[i] → Index (plain subscript)
        let StmtKind::Expr(e) = only("xs[i]\n") else { panic!() };
        assert!(matches!(e.kind, ExprKind::Index { .. }));
        // m["k"] → Index (non-int key still plain index)
        let StmtKind::Expr(e) = only("m[\"k\"]\n") else { panic!() };
        assert!(matches!(e.kind, ExprKind::Index { .. }));
        // xs[1:3] → Slice with int bounds
        let StmtKind::Expr(e) = only("xs[1:3]\n") else { panic!() };
        let ExprKind::Slice { start, end, .. } = e.kind else {
            panic!("expected Slice, got {:?}", e.kind)
        };
        assert!(matches!(start.unwrap().kind, ExprKind::Int(1)));
        assert!(matches!(end.unwrap().kind, ExprKind::Int(3)));
        // nested xs[a:b][0] → Index(Slice)
        let StmtKind::Expr(e) = only("xs[a:b][0]\n") else { panic!() };
        let ExprKind::Index { obj, .. } = e.kind else {
            panic!("expected Index, got {:?}", e.kind)
        };
        assert!(matches!(obj.kind, ExprKind::Slice { .. }));
    }

    #[test]
    fn map_literal() {
        let StmtKind::Expr(e) = only("{}\n") else {
            panic!()
        };
        match e.kind {
            ExprKind::Map(entries) => assert_eq!(entries.len(), 0),
            other => panic!("{other:?}"),
        }
        let StmtKind::Expr(e) = only("{\"a\": 1, \"b\": 2}\n") else {
            panic!()
        };
        match e.kind {
            ExprKind::Map(entries) => {
                assert_eq!(entries.len(), 2);
                assert!(matches!(entries[0].0.kind, ExprKind::Str(_)));
                assert!(matches!(entries[0].1.kind, ExprKind::Int(1)));
            }
            other => panic!("{other:?}"),
        }
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

    #[test]
    fn list_comprehension_with_guard() {
        let StmtKind::Expr(e) = only("[x * 2 for x in xs if x > 0]\n") else {
            panic!()
        };
        match e.kind {
            ExprKind::Comprehension { kind, key, elem, clauses } => {
                assert_eq!(kind, CompKind::List);
                assert!(key.is_none());
                assert!(matches!(elem.kind, ExprKind::Binary { .. }));
                assert_eq!(clauses.len(), 1);
                assert_eq!(clauses[0].vars, vec!["x".to_string()]);
                assert!(matches!(clauses[0].iter.kind, ExprKind::Ident(_)));
                assert_eq!(clauses[0].guards.len(), 1);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn list_comprehension_over_range_no_guard() {
        let StmtKind::Expr(e) = only("[x for x in 0..10]\n") else {
            panic!()
        };
        match e.kind {
            ExprKind::Comprehension { kind, clauses, .. } => {
                assert_eq!(kind, CompKind::List);
                assert_eq!(clauses.len(), 1);
                assert!(matches!(clauses[0].iter.kind, ExprKind::Range { .. }));
                assert!(clauses[0].guards.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn set_comprehension() {
        let StmtKind::Expr(e) = only("{x for x in xs}\n") else {
            panic!()
        };
        match e.kind {
            ExprKind::Comprehension { kind, key, .. } => {
                assert_eq!(kind, CompKind::Set);
                assert!(key.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn map_comprehension_two_vars() {
        let StmtKind::Expr(e) = only("{k: v for k, v in m}\n") else {
            panic!()
        };
        match e.kind {
            ExprKind::Comprehension { kind, key, elem, clauses } => {
                assert_eq!(kind, CompKind::Map);
                assert!(matches!(key.unwrap().kind, ExprKind::Ident(_)));
                assert!(matches!(elem.kind, ExprKind::Ident(_)));
                assert_eq!(clauses.len(), 1);
                assert_eq!(clauses[0].vars, vec!["k".to_string(), "v".to_string()]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn two_for_clauses_parse() {
        let StmtKind::Expr(e) = only("[x for x in xs for y in ys]\n") else {
            panic!()
        };
        match e.kind {
            ExprKind::Comprehension { kind, clauses, .. } => {
                assert_eq!(kind, CompKind::List);
                assert_eq!(clauses.len(), 2);
                assert_eq!(clauses[0].vars, vec!["x".to_string()]);
                assert!(clauses[0].guards.is_empty());
                assert_eq!(clauses[1].vars, vec!["y".to_string()]);
                assert!(clauses[1].guards.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn guard_after_nonfinal_clause_parses() {
        // The `if x > 0` binds to the FIRST clause (the one it follows), not globally.
        let StmtKind::Expr(e) = only("[x for x in xs if x > 0 for y in ys]\n") else {
            panic!()
        };
        match e.kind {
            ExprKind::Comprehension { clauses, .. } => {
                assert_eq!(clauses.len(), 2);
                assert_eq!(clauses[0].guards.len(), 1);
                assert!(clauses[1].guards.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn chained_guards_on_one_clause_parse() {
        let StmtKind::Expr(e) = only("[x for x in xs if x > 0 if x < 10]\n") else {
            panic!()
        };
        match e.kind {
            ExprKind::Comprehension { clauses, .. } => {
                assert_eq!(clauses.len(), 1);
                assert_eq!(clauses[0].guards.len(), 2);
            }
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
    fn inline_expr_body_flag_set() {
        // `fn a(): 10` — inline bare-expr body -> inline_expr_body marker set (implicit return).
        let StmtKind::Fn(f) = only("fn a(): 10\n") else { panic!() };
        assert!(f.inline_expr_body, "inline bare-expr body should set the marker");
        // annotated inline-expr body too.
        let StmtKind::Fn(f) = only("fn a() -> int: 10\n") else { panic!() };
        assert!(f.inline_expr_body);
    }

    #[test]
    fn inline_non_expr_and_multiline_bodies_not_marked() {
        // inline NON-expr stmt (assignment) -> not an implicit return.
        let StmtKind::Fn(f) = only("fn a(): x = 5\n") else { panic!() };
        assert!(!f.inline_expr_body, "inline assignment must not set the marker");
        // inline explicit `return` -> not the implicit-return form.
        let StmtKind::Fn(f) = only("fn a(): return 10\n") else { panic!() };
        assert!(!f.inline_expr_body);
        // a 1-statement MULTILINE expr body -> NOT inline (the Block shape is identical, the marker
        // is what disambiguates).
        let StmtKind::Fn(f) = only("fn a():\n    10\n") else { panic!() };
        assert!(!f.inline_expr_body, "a multiline 1-stmt body must not set the marker");
    }

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

    /// A function type in parameter position: `f: fn(int) -> int`.
    #[test]
    fn fn_param_type() {
        let StmtKind::Fn(f) =
            only("fn apply(f: fn(int) -> int, v: int) -> int:\n    return f(v)\n")
        else {
            panic!()
        };
        match &f.params[0].ty {
            Some(Type::Func { params, ret }) => {
                assert_eq!(params.as_slice(), &[Type::Named("int".into())]);
                assert_eq!(**ret, Type::Named("int".into()));
            }
            other => panic!("{other:?}"),
        }
    }

    /// A zero-argument function type: `fn() -> int`.
    #[test]
    fn fn_type_zero_args() {
        let StmtKind::Fn(f) = only("fn g(f: fn() -> int):\n    return f()\n") else {
            panic!()
        };
        match &f.params[0].ty {
            Some(Type::Func { params, ret }) => {
                assert!(params.is_empty());
                assert_eq!(**ret, Type::Named("int".into()));
            }
            other => panic!("{other:?}"),
        }
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
        // `MAX_DEPTH` (64) trips after ~1 MiB of parser frames — well within a *test* thread's
        // default (~2 MiB) stack, so the guard fires cleanly instead of the host stack overflowing.
        // (Was 128, which sat at the stack edge and needed a 64 MiB thread to test; the lower bound
        // removed that crutch — runs inline now.) Exercises all four recursive entry points.
        let paren = format!("x := {}1{}\n", "(".repeat(500), ")".repeat(500));
        assert!(parse(lexer::tokenize(&paren).unwrap()).unwrap_err().message.contains("too deeply"));

        let unary = format!("x := {}y\n", "not ".repeat(500));
        assert!(parse(lexer::tokenize(&unary).unwrap()).unwrap_err().message.contains("too deeply"));

        let generic = format!("z: {}int{} = w\n", "list[".repeat(500), "]".repeat(500));
        assert!(parse(lexer::tokenize(&generic).unwrap()).unwrap_err().message.contains("too deeply"));

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
        assert!(parse(lexer::tokenize(&blocks).unwrap()).unwrap_err().message.contains("too deeply"));
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
                bindings: vec![Pattern::Ident("r".into())],
                enum_name: None,
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
    fn explicit_type_args_parse() {
        // `max[int](3, 7)` → a Call carrying one type arg.
        let v = let_value("x := max[int](3, 7)\n");
        let ExprKind::Call { callee, args, type_args, .. } = v.kind else {
            panic!("expected a Call, got {:?}", v.kind)
        };
        assert!(matches!(callee.kind, ExprKind::Ident(ref n) if n == "max"));
        assert_eq!(args.len(), 2);
        assert_eq!(type_args, vec![Type::Named("int".into())]);

        // Multi-arg, incl. a compound type — only expressible as type args (comma is not an index).
        let v = let_value("p := Pair[int, str](1, \"a\")\n");
        let ExprKind::Call { type_args, .. } = v.kind else {
            panic!("expected a Call, got {:?}", v.kind)
        };
        assert_eq!(type_args, vec![Type::Named("int".into()), Type::Named("str".into())]);
    }

    #[test]
    fn index_then_call_still_index() {
        // A numeric subscript is NOT stolen — `fns[0](5)` stays index-then-call.
        let v = let_value("x := fns[0](5)\n");
        let ExprKind::Call { callee, type_args, .. } = v.kind else {
            panic!("expected a Call, got {:?}", v.kind)
        };
        assert!(type_args.is_empty());
        assert!(matches!(callee.kind, ExprKind::Index { .. }));

        // A bare index with no trailing call stays an index.
        let v = let_value("x := arr[i]\n");
        assert!(matches!(v.kind, ExprKind::Index { .. }));
    }

    #[test]
    fn speculative_type_arg_steal_does_not_leak_recursion_depth() {
        // Each bare-name index (`a[0]`) attempts the type-arg steal, which speculatively calls
        // `parse_type` and backtracks. That must NOT leak the recursion-depth counter, else many
        // plain indexes spuriously trip MAX_DEPTH. Far more indexes than MAX_DEPTH, all valid.
        let mut src = String::from("a := [1, 2, 3]\n");
        for i in 0..200 {
            src.push_str(&format!("b{i} := a[0]\n"));
        }
        parse_ok(&src);
    }

    #[test]
    fn pipe_desugars_to_call_with_lhs_first() {
        // `x := 5 |> inc()` ⇒ `inc(5)`
        let v = let_value("x := 5 |> inc()\n");
        let ExprKind::Call { callee, args, .. } = v.kind else {
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
        let ExprKind::Call { callee, args, .. } = v.kind else {
            panic!("expected outer Call, got {:?}", v.kind)
        };
        assert!(matches!(callee.kind, ExprKind::Ident(ref n) if n == "dbl"));
        assert_eq!(args.len(), 1);
        let ExprKind::Call { callee: inner_callee, args: inner_args, .. } = &args[0].kind else {
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

    // ===== gap #8: tuples + multi-return + destructuring =====

    #[test]
    fn tuple_literal_two_elements() {
        let StmtKind::Expr(e) = only("(1, 2)\n") else { panic!() };
        match e.kind {
            ExprKind::Tuple(elems) => {
                assert_eq!(elems.len(), 2);
                assert_eq!(elems[0].kind, ExprKind::Int(1));
                assert_eq!(elems[1].kind, ExprKind::Int(2));
            }
            other => panic!("{other:?}"),
        }
    }

    /// `(1 + 2)` is grouping, not a 1-tuple — it stays a `Binary`.
    #[test]
    fn paren_single_expr_is_grouping() {
        let StmtKind::Expr(e) = only("(1 + 2)\n") else { panic!() };
        assert!(matches!(e.kind, ExprKind::Binary { op: BinaryOp::Add, .. }));
    }

    /// `(e,)` is a 1-element tuple (distinct from grouping `(e)`).
    #[test]
    fn one_element_tuple_parses() {
        let StmtKind::Expr(e) = only("(1,)\n") else { panic!() };
        match e.kind {
            ExprKind::Tuple(elems) => {
                assert_eq!(elems.len(), 1);
                assert_eq!(elems[0].kind, ExprKind::Int(1));
            }
            other => panic!("expected 1-tuple, got {other:?}"),
        }
    }

    /// `(x,)` is a 1-tuple, `(x)` is grouping (bare expr), `(x, y,)` is a 2-tuple.
    #[test]
    fn tuple_one_two_grouping_trio() {
        let StmtKind::Expr(e1) = only("(x,)\n") else { panic!() };
        assert!(matches!(&e1.kind, ExprKind::Tuple(es) if es.len() == 1));
        let StmtKind::Expr(e2) = only("(x)\n") else { panic!() };
        assert!(matches!(e2.kind, ExprKind::Ident(_)), "grouping, got {:?}", e2.kind);
        let StmtKind::Expr(e3) = only("(x, y,)\n") else { panic!() };
        assert!(matches!(&e3.kind, ExprKind::Tuple(es) if es.len() == 2));
        // Lone-comma form still errors (parse_err asserts a parse failure occurred).
        parse_err("(,)\n");
    }

    /// Optional trailing comma on list/map/set produces identical AST and `[,]`/`{,}` still error.
    #[test]
    fn collection_trailing_comma_same_ast() {
        // list
        let StmtKind::Expr(a) = only("[1, 2]\n") else { panic!() };
        let StmtKind::Expr(b) = only("[1, 2,]\n") else { panic!() };
        assert_eq!(a.kind, b.kind);
        assert!(matches!(&b.kind, ExprKind::List(es) if es.len() == 2));
        parse_err("[,]\n");
        // map
        let StmtKind::Expr(a) = only("{\"a\": 1}\n") else { panic!() };
        let StmtKind::Expr(b) = only("{\"a\": 1,}\n") else { panic!() };
        assert_eq!(a.kind, b.kind);
        assert!(matches!(&b.kind, ExprKind::Map(es) if es.len() == 1));
        parse_err("{,}\n");
        // set
        let StmtKind::Expr(a) = only("{1, 2}\n") else { panic!() };
        let StmtKind::Expr(b) = only("{1, 2,}\n") else { panic!() };
        assert_eq!(a.kind, b.kind);
        assert!(matches!(&b.kind, ExprKind::Set(es) if es.len() == 2));
    }

    /// Optional trailing comma on call args and fn/closure params; `f(,)` still errors.
    #[test]
    fn call_args_and_params_trailing_comma() {
        // call args
        let StmtKind::Expr(a) = only("f(1, 2)\n") else { panic!() };
        let StmtKind::Expr(b) = only("f(1, 2,)\n") else { panic!() };
        assert_eq!(a.kind, b.kind);
        parse_err("f(,)\n");
        // fn params
        parse_ok("fn g(a, b,):\n    a\n");
        // closure params (closures live in expression position, e.g. a let RHS)
        let StmtKind::Let { value, .. } = only("c := fn(a, b,): a\n") else { panic!() };
        assert!(matches!(value.kind, ExprKind::Closure { .. }), "got {:?}", value.kind);
    }

    #[test]
    fn tuple_return_type_parses() {
        let StmtKind::Fn(f) = only("fn pair() -> (int, int):\n    return (3, 4)\n") else {
            panic!()
        };
        assert_eq!(
            f.ret,
            Some(Type::Tuple(vec![Type::Named("int".into()), Type::Named("int".into())]))
        );
    }

    /// `(T)` in type position unwraps to `T` (not a 1-tuple).
    #[test]
    fn paren_single_type_unwraps() {
        let StmtKind::Fn(f) = only("fn f(x: (int)):\n    return x\n") else { panic!() };
        assert_eq!(f.params[0].ty, Some(Type::Named("int".into())));
    }

    #[test]
    fn destructuring_let_parses() {
        match only("a, b := pair()\n") {
            StmtKind::Let { names, ty, value, .. } => {
                assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
                assert!(ty.is_none());
                assert!(matches!(value.kind, ExprKind::Call { .. }));
            }
            other => panic!("{other:?}"),
        }
    }

    /// `a, 1 := …` — every destructure target must be a bare identifier.
    #[test]
    fn destructuring_non_ident_rejected() {
        assert!(parse_err("a, 1 := pair()\n").message.contains("identifier"));
    }

    #[test]
    fn tuple_element_access_parses() {
        let StmtKind::Expr(e) = only("t.0\n") else { panic!() };
        match e.kind {
            ExprKind::Field { name, .. } => assert_eq!(name, "0"),
            other => panic!("{other:?}"),
        }
        let StmtKind::Expr(e) = only("t.1\n") else { panic!() };
        match e.kind {
            ExprKind::Field { name, .. } => assert_eq!(name, "1"),
            other => panic!("{other:?}"),
        }
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

    // ---- default args + named args (parser layer) ----

    #[test]
    fn param_default_parses() {
        match only("fn f(x: int, y: int = 10):\n    print(x)\n") {
            StmtKind::Fn(f) => {
                assert_eq!(f.params.len(), 2);
                assert!(f.params[0].default.is_none());
                assert!(matches!(f.params[1].default, Some(Expr { kind: ExprKind::Int(10), .. })));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn named_call_args_parse() {
        let v = let_value("r := f(1, y=2, z=3)\n");
        let ExprKind::Call { args, named, .. } = v.kind else {
            panic!("expected a Call, got {:?}", v.kind)
        };
        assert_eq!(args.len(), 1);
        assert!(matches!(args[0].kind, ExprKind::Int(1)));
        assert_eq!(named.len(), 2);
        assert_eq!(named[0].0, "y");
        assert!(matches!(named[0].1.kind, ExprKind::Int(2)));
        assert_eq!(named[1].0, "z");
    }

    #[test]
    fn eqeq_arg_is_not_named() {
        // `f(x == 1)` is a positional boolean expression, NOT a named arg.
        let v = let_value("r := f(x == 1)\n");
        let ExprKind::Call { args, named, .. } = v.kind else { panic!() };
        assert_eq!(args.len(), 1);
        assert!(named.is_empty());
        assert!(matches!(args[0].kind, ExprKind::Binary { op: BinaryOp::Eq, .. }));
    }

    #[test]
    fn positional_after_named_rejected() {
        assert!(parse_err("r := f(y=2, 1)\n")
            .message
            .contains("positional argument after named argument"));
    }

    #[test]
    fn non_const_default_parses() {
        // The parser accepts any expression as a default; the desugar pass (not the parser) rejects
        // a default that references another parameter.
        parse_ok("fn f(x: int = g()):\n    print(x)\n");
        parse_ok("fn f(x: int = 1 + 2):\n    print(x)\n");
    }

    #[test]
    fn const_collection_default_ok() {
        // literal collections of constants are allowed
        parse_ok("fn f(xs: list[int] = [1, 2]):\n    print(xs)\n");
        parse_ok("fn f(b: bool = false, s: str = \"hi\"):\n    print(b)\n");
    }

    #[test]
    fn default_before_required_rejected() {
        assert!(parse_err("fn f(x: int = 1, y: int):\n    print(x)\n")
            .message
            .contains("required parameter"));
    }

    #[test]
    fn closure_default_rejected() {
        assert!(parse_err("c := fn(x: int = 1): x\n")
            .message
            .contains("default"));
    }

    #[test]
    fn struct_field_default_parses() {
        match only("struct S:\n    x: int\n    y: int = 0\n") {
            StmtKind::Struct { fields, .. } => {
                assert_eq!(fields.len(), 2);
                assert!(fields[0].default.is_none());
                assert!(matches!(fields[1].default, Some(Expr { kind: ExprKind::Int(0), .. })));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn struct_field_default_before_required_rejected() {
        assert!(parse_err("struct S:\n    x: int = 0\n    y: int\n")
            .message
            .contains("required field"));
    }

    #[test]
    fn method_default_param_allowed() {
        // Methods now accept constant-literal defaults (filled by the desugar pass at call sites).
        match only("struct S:\n    x: int\n    fn scale(self, k: int = 2) -> int:\n        return self.x * k\n") {
            StmtKind::Struct { methods, .. } => {
                assert!(methods[0].params[1].default.is_some());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn method_required_after_default_rejected() {
        // The trailing-default rule still applies inside methods.
        assert!(parse_err("struct S:\n    x: int\n    fn f(self, a: int = 1, b: int) -> int:\n        return a\n")
            .message
            .contains("required parameter"));
    }

    // --- or-patterns + nested nullary -----------------------------------------------------------

    /// The pattern of the first arm of `match x:` in a one-statement program.
    fn first_arm_pattern(src: &str) -> Pattern {
        match only(src) {
            StmtKind::Match { mut arms, .. } => arms.remove(0).pattern,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn or_pattern_top_level() {
        // `1 | 2 | 3` -> Or of three int literals.
        match first_arm_pattern("match x:\n    1 | 2 | 3: print(1)\n    _: print(0)\n") {
            Pattern::Or(alts) => {
                assert_eq!(alts.len(), 3);
                assert!(matches!(alts[0], Pattern::Literal(LitPattern::Int(1))));
                assert!(matches!(alts[1], Pattern::Literal(LitPattern::Int(2))));
                assert!(matches!(alts[2], Pattern::Literal(LitPattern::Int(3))));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn or_pattern_variants() {
        // `Red | Green` -> Or of two nullary variants (top position).
        match first_arm_pattern("match c:\n    Red | Green: print(1)\n    _: print(0)\n") {
            Pattern::Or(alts) => {
                assert_eq!(alts.len(), 2);
                assert!(matches!(&alts[0], Pattern::Variant { name, bindings, .. } if name == "Red" && bindings.is_empty()));
                assert!(matches!(&alts[1], Pattern::Variant { name, bindings, .. } if name == "Green" && bindings.is_empty()));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn or_pattern_in_tuple() {
        // `(1 | 2, x)` -> Tuple([Or([1,2]), Ident(x)]).
        match first_arm_pattern("match p:\n    (1 | 2, x): print(x)\n    _: print(0)\n") {
            Pattern::Tuple(elems) => {
                assert_eq!(elems.len(), 2);
                assert!(matches!(&elems[0], Pattern::Or(a) if a.len() == 2));
                assert!(matches!(&elems[1], Pattern::Ident(n) if n == "x"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn qualified_variant_pattern_parses() {
        // `Color.Red` (nullary) -> Variant{name: "Red", enum_name: Some("Color")}.
        match first_arm_pattern("match c:\n    Color.Red: print(0)\n    _: print(1)\n") {
            Pattern::Variant { name, bindings, enum_name } => {
                assert_eq!(name, "Red");
                assert!(bindings.is_empty());
                assert_eq!(enum_name.as_deref(), Some("Color"));
            }
            other => panic!("{other:?}"),
        }
        // `Shape.Circle(r)` (payload) -> Variant{name: "Circle", enum_name: Some("Shape"), [Ident r]}.
        match first_arm_pattern("match s:\n    Shape.Circle(r): print(r)\n    _: print(0)\n") {
            Pattern::Variant { name, bindings, enum_name } => {
                assert_eq!(name, "Circle");
                assert_eq!(enum_name.as_deref(), Some("Shape"));
                assert!(matches!(&bindings[0], Pattern::Ident(n) if n == "r"));
            }
            other => panic!("{other:?}"),
        }
        // Bare `None` stays unqualified.
        match first_arm_pattern("match o:\n    None: print(0)\n    _: print(1)\n") {
            Pattern::Variant { name, enum_name, .. } => {
                assert_eq!(name, "None");
                assert_eq!(enum_name, None);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn or_pattern_in_payload() {
        // `Some(a | b)` -> Variant{Some, [Or([Ident a, Ident b])]}.
        match first_arm_pattern("match o:\n    Some(a | b): print(a)\n    _: print(0)\n") {
            Pattern::Variant { name, bindings, .. } => {
                assert_eq!(name, "Some");
                assert_eq!(bindings.len(), 1);
                match &bindings[0] {
                    Pattern::Or(alts) => {
                        assert_eq!(alts.len(), 2);
                        assert!(matches!(&alts[0], Pattern::Ident(n) if n == "a"));
                        assert!(matches!(&alts[1], Pattern::Ident(n) if n == "b"));
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn single_primary_unchanged() {
        // A single primary still parses to that primary unchanged (zero regression — no Or wrapper).
        match first_arm_pattern("match x:\n    1: print(1)\n    _: print(0)\n") {
            Pattern::Literal(LitPattern::Int(1)) => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nested_nullary_parses() {
        // `Some(None)` -> Variant{Some, [Ident("None")]} (parser is type-blind; checker promotes).
        match first_arm_pattern("match o:\n    Some(None): print(0)\n    _: print(1)\n") {
            Pattern::Variant { name, bindings, .. } => {
                assert_eq!(name, "Some");
                assert_eq!(bindings.len(), 1);
                assert!(matches!(&bindings[0], Pattern::Ident(n) if n == "None"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn nested_nullary_in_result_parses() {
        // `Ok(Err(e))` -> Variant{Ok, [Variant{Err, [Ident e]}]}.
        match first_arm_pattern("match r:\n    Ok(Err(e)): print(e)\n    _: print(0)\n") {
            Pattern::Variant { name, bindings, .. } => {
                assert_eq!(name, "Ok");
                match &bindings[0] {
                    Pattern::Variant { name, bindings, .. } => {
                        assert_eq!(name, "Err");
                        assert!(matches!(&bindings[0], Pattern::Ident(n) if n == "e"));
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    // ===== `ref T` binding modifier (ref T) =====

    #[test]
    fn parses_ref_local_and_param() {
        match only("r: ref int = 0\n") {
            StmtKind::Let { is_ref, ty, names, .. } => {
                assert!(is_ref, "typed-let `ref` modifier should set is_ref");
                assert_eq!(names, vec!["r".to_string()]);
                assert_eq!(ty, Some(Type::Named("int".to_string())));
            }
            other => panic!("{other:?}"),
        }
        match only("fn f(x: ref int):\n    return\n") {
            StmtKind::Fn(decl) => {
                let p = &decl.params[0];
                assert!(p.is_ref, "param `ref` modifier should set is_ref");
                assert_eq!(p.ty, Some(Type::Named("int".to_string())));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn plain_local_and_param_are_not_ref() {
        match only("x: int = 0\n") {
            StmtKind::Let { is_ref, .. } => assert!(!is_ref),
            other => panic!("{other:?}"),
        }
        match only("fn f(x: int):\n    return\n") {
            StmtKind::Fn(decl) => assert!(!decl.params[0].is_ref),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rejects_ref_in_type_positions() {
        // `ref` is consumed only in the two binding positions; `parse_type` never eats it, so it is a
        // parse error in any other type position (the lexer keyword can't start a <type>).
        for src in [
            "fn f() -> ref int:\n    return 0\n", // return type
            "xs: list[ref int] = []\n",           // generic arg / collection element
            "struct S:\n    count: ref int\n",   // struct field
            "p: (ref int, int) = (0, 1)\n",      // tuple element
        ] {
            assert!(
                parse_err(src).message.contains("found 'ref'"),
                "expected a `ref` placement error for: {src:?}"
            );
        }
    }

    #[test]
    fn rejects_ref_destructuring() {
        // `ref` is a single-name binding modifier; a destructuring let cannot carry it.
        let _ = parse_err("a, b: ref int := (0, 1)\n");
    }

    #[test]
    fn parses_closure_ref_param() {
        // A `ref` modifier is legal on a closure param (`fn(x: ref int)`), parsed like any param.
        match only("g := fn(x: ref int) -> int: x + 1\n") {
            StmtKind::Let { value, .. } => {
                let ExprKind::Closure { params, .. } = &value.kind else { panic!("closure") };
                assert!(params[0].is_ref, "closure param `ref` modifier should set is_ref");
                assert_eq!(params[0].ty, Some(Type::Named("int".to_string())));
            }
            other => panic!("{other:?}"),
        }
    }
}

