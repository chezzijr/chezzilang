//! Parser: blocks and layout, types and type postfixes.
//! Split out of `src/parser/mod.rs`. `super::*` is the parent module.

use super::*;
use crate::lexer::{Span, Token};

impl Parser {
    /// Open + parse a block after a `:`. Either an indented `Newline Indent … Dedent` group, or a
    /// single inline statement on the same line.
    pub(super) fn parse_block(&mut self) -> PResult<Block> {
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
            // inline single statement (e.g. a one-line `match` arm): one `<simpleStmt>`.
            Ok(vec![self.parse_stmt_in(true)?])
        }
    }

    /// The `:` at token `colon` ends a block expression's head. Inside brackets the lexer emitted no
    /// layout, so ask it to open the block now ([`crate::lexer::open_layout`]); at bracket depth 0 the
    /// next token is already `Newline`, and a same-line body is left for the caller to reject.
    pub(super) fn request_layout(&mut self, colon: usize) -> PResult<()> {
        if self.toks[colon + 1].kind == Token::Newline {
            return Ok(());
        }
        crate::lexer::open_layout(&mut self.toks, colon).map_err(|e| ParseError {
            message: e.message,
            span: Span {
                line: e.line as u32,
                col: e.col as u32,
                file: self.toks[self.pos].span.file,
            },
        })?;
        Ok(())
    }

    /// Open an indented block for declarations whose bodies are not statements (`struct`, `enum`):
    /// consumes `: Newline Indent`. The caller parses members until `Dedent`.
    pub(super) fn open_block(&mut self) -> PResult<()> {
        self.expect(&Token::Colon)?;
        self.expect(&Token::Newline)?;
        self.expect(&Token::Indent)?;
        Ok(())
    }

    // ----- types -----

    /// One parameter slot of a `fn(...)` TYPE, with an OPTIONAL Swift-style label: `name: T` yields
    /// `(Some("name"), Some(T), No)`; a bare `T` yields `(None, Some(T), No)`. A label is an `IDENT`
    /// IMMEDIATELY followed by `:` — disambiguated by lookahead so a bare qualified/generic type head
    /// (`m.Type` = `IDENT Dot`, `List[int]` = `IDENT LBracket`) is never mistaken for a label.
    /// A variadic slot is `...T` or `...name: T` (`Pack`); a bare `...` (a C vararg, no type) yields
    /// `(None, None, C)`.
    fn parse_fn_type_param(&mut self) -> PResult<(Option<String>, Option<Type>, Variadic)> {
        if self.eat(&Token::DotDotDot) {
            if self.check(&Token::Comma) || self.check(&Token::RParen) {
                return Ok((None, None, Variadic::C));
            }
            let (label, ty) = self.parse_labeled_type()?;
            return Ok((label, Some(ty), Variadic::Pack));
        }
        let (label, ty) = self.parse_labeled_type()?;
        Ok((label, Some(ty), Variadic::No))
    }

    /// `name: T` or a bare `T`.
    fn parse_labeled_type(&mut self) -> PResult<(Option<String>, Type)> {
        if matches!(self.peek(), Token::Ident(_)) && *self.peek_at(1) == Token::Colon {
            let label = self.expect_ident()?;
            self.expect(&Token::Colon)?;
            let ty = self.parse_type()?;
            Ok((Some(label), ty))
        } else {
            Ok((None, self.parse_type()?))
        }
    }

    /// One type, with its own **fold scope**: `parse_type_postfix` is the parser's THIRD iterative
    /// fold loop (`T?!?…` wraps a `Type::Generic` around the tree it already built), and like the two
    /// expression loops it must be charged against [`MAX_AST_DEPTH`] — a recursion counter never sees
    /// it. Reset before the children so sibling type arguments cost the depth of the DEEPEST one
    /// rather than their sum, `max`-combined into the caller on the way out, so a postfix chain
    /// stacked on an already-deep type pays for both. Measured before this: `fn f(x: int` + `?!` ×
    /// 350 000 + `)` reached `chezzi check` and SIGABRTed with `fatal runtime error: stack overflow`
    /// (both `b1307258` and `dfdc7a1b`); it is now a clean `type nested too deeply`.
    pub(super) fn parse_type(&mut self) -> PResult<Type> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("type nested too deeply".to_string()));
        }
        let outer_fold = self.fold_depth;
        self.fold_depth = 0;
        let ty = self.parse_type_body()?;
        self.fold_depth = outer_fold.max(self.fold_depth);
        self.depth -= 1;
        Ok(ty)
    }

    /// [`Self::parse_type`]'s body — everything but the depth/fold bookkeeping.
    fn parse_type_body(&mut self) -> PResult<Type> {
        // `fn(T1, …) -> R` — a function type in type position.
        if self.eat(&Token::Fn) {
            self.expect(&Token::LParen)?;
            let mut params = Vec::new();
            let mut labels: Vec<Option<String>> = Vec::new();
            let mut variadic = Variadic::No;
            if !self.check(&Token::RParen) {
                loop {
                    if variadic != Variadic::No {
                        return Err(self.err(
                            "a variadic parameter must be the last parameter of a function type"
                                .to_string(),
                        ));
                    }
                    let (l, t, v) = self.parse_fn_type_param()?;
                    variadic = v;
                    if let Some(t) = t {
                        labels.push(l);
                        params.push(t);
                    }
                    if !self.eat(&Token::Comma) {
                        break;
                    }
                }
            }
            self.expect(&Token::RParen)?;
            self.expect(&Token::Arrow)?;
            let ret = self.parse_type()?;
            let mut ty = Type::Func {
                params,
                ret: Box::new(ret),
                labels,
                variadic,
            };
            ty = self.parse_type_postfix(ty)?;
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
            return Ok(ty);
        }
        // Prefix `!E` (and a bare `!`): `None!E`, the type of a fallible call with no value. The
        // `!` is left for `parse_type_postfix`, so both spellings are one production.
        if self.check(&Token::Bang) {
            let ty = Type::Nil(self.cur_span());
            return self.parse_type_postfix(ty);
        }
        if self.check(&Token::NoneKw) {
            let ty = Type::Nil(self.cur_span());
            self.advance();
            return self.parse_type_postfix(ty);
        }
        let name_span = self.cur_span();
        let name = self.expect_ident()?;
        // A module-qualified type `module.Type` (mirrors how `module.func()` is reached): after the
        // first ident, a `.` introduces the type's name in the bound module. A longer path
        // (`pkg.deep.Point`) is the full path of an imported module (TICKET-175): the LAST segment is
        // the type and the rest, dot-joined, is the module. Any trailing `[args]` then belongs to the
        // qualified type (`geo.Box[int]`).
        if self.eat(&Token::Dot) {
            let mut segments = vec![name, self.expect_ident()?];
            while self.eat(&Token::Dot) {
                segments.push(self.expect_ident()?);
            }
            let member = segments.pop().unwrap_or_default();
            let name = segments.join(".");
            let args = if self.eat(&Token::LBracket) {
                let mut args = vec![self.parse_type()?];
                while self.eat(&Token::Comma) {
                    args.push(self.parse_type()?);
                }
                self.expect(&Token::RBracket)?;
                args
            } else {
                Vec::new()
            };
            let ty = Type::Qualified {
                module: name,
                name: member,
                args,
            };
            let ty = self.parse_type_postfix(ty)?;
            return Ok(ty);
        }
        let ty = if self.eat(&Token::LBracket) {
            let mut args = vec![self.parse_type()?];
            while self.eat(&Token::Comma) {
                args.push(self.parse_type()?);
            }
            self.expect(&Token::RBracket)?;
            // The head-name token's OWN span (captured before `expect_ident`) rides along for the
            // editor hover overlay; equality-neutral, never read by the engines.
            Type::Generic(name, args, name_span)
        } else {
            Type::Named {
                name,
                span: name_span,
            }
        };
        self.parse_type_postfix(ty)
    }

    /// Postfix shorthand on a fully-parsed base type: `T?` = Option[T], `T!` = Result[T, Error],
    /// `T!E` = Result[T, E]. Stacks left-to-right (`T?!` = Result[Option[T], Error]).
    ///
    /// Same accounting as the two expression fold loops: each iteration adds one level to a type
    /// tree the front-end walkers descend recursively, and this loop bumps neither `self.depth` nor
    /// anything else. The base type's own fold-depth is already in `self.fold_depth` (the caller
    /// [`Self::parse_type`] opened the scope), so the chain SUMS on top of it.
    /// `T?` and `T!E` name the carriers by their private spelling (`ast::private_name`): they mean
    /// the prelude's carrier whatever this module declares, and `Option`/`Result` are no names a
    /// user module sees.
    fn parse_type_postfix(&mut self, mut ty: Type) -> PResult<Type> {
        let mut chain = 0usize;
        loop {
            if self.depth + self.fold_depth + chain > MAX_AST_DEPTH {
                return Err(self.err("type nested too deeply".to_string()));
            }
            if self.eat(&Token::Question) {
                chain += 1;
                ty = Type::Generic(private_name("Option"), vec![ty], Span::default());
            } else if self.eat(&Token::Bang) {
                chain += 1;
                // An explicit error type follows only if the next token can start one; otherwise
                // `T!` defaults the error type to `Error` (resolved later by the checker).
                if matches!(
                    self.peek(),
                    Token::Ident(_) | Token::LParen | Token::Fn | Token::NoneKw
                ) {
                    let err = self.parse_type()?;
                    ty = Type::Generic(private_name("Result"), vec![ty, err], Span::default());
                } else {
                    ty = Type::Generic(private_name("Result"), vec![ty], Span::default());
                }
            } else if self.check(&Token::NotEq) && self.bang_eq != Some(self.pos) {
                // Adjacent `!=` lexes as ONE token: `int!= 6` is `int! = 6`. The token stays; the
                // position test stops an outer postfix loop wrapping a second time.
                chain += 1;
                self.bang_eq = Some(self.pos);
                ty = Type::Generic(private_name("Result"), vec![ty], Span::default());
                break;
            } else if self.eat(&Token::QuestionQuestion) {
                // Adjacent `??` lexes as ONE token: `int??` is `Option[Option[int]]`.
                chain += 2;
                ty = Type::Generic(private_name("Option"), vec![ty], Span::default());
                ty = Type::Generic(private_name("Option"), vec![ty], Span::default());
            } else {
                break;
            }
        }
        self.fold_depth += chain;
        Ok(ty)
    }

    // ----- expressions (Pratt) -----
}
