//! Parser: patterns and binding targets: `parse_pattern`, bind targets, loop targets, walrus
//! targets, value lists.
//! Split out of `src/parser/mod.rs`. `super::*` is the parent module.

use super::*;
use crate::lexer::{Span, Token};

impl Parser {
    /// Parse a top-level match-arm pattern. A bare identifier here names a variant (`None`,
    /// `Point`); `(...)` is a tuple pattern. Nested positions use [`parse_subpattern`], where a bare
    /// identifier is a binding instead.
    pub(super) fn parse_pattern(&mut self) -> PResult<Pattern> {
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
        // Depth guard — the fifth recursive entry point (mirrors parse_stmt/parse_type/parse_bp/
        // parse_unary). Nested variant payloads (`Some(Some(...))`) and tuple elements (`((( ... )))`)
        // recurse through here per level; without this a deeply-nested pattern overflows the native
        // stack (SIGABRT) before check/run. Leaks the increment on the `?`/guard-Err paths — the
        // pattern callers (parse_match/parse_match_expr) call with `?` and never backtrack, so that
        // matches the four existing sites.
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("pattern nested too deeply".to_string()));
        }
        let mut alts = vec![self.parse_pattern_primary(top)?];
        while self.eat(&Token::BitOr) {
            alts.push(self.parse_pattern_primary(top)?);
        }
        let result = if alts.len() == 1 {
            alts.pop().expect("one alternative")
        } else {
            Pattern::Or(alts)
        };
        self.depth -= 1;
        Ok(result)
    }

    fn parse_pattern_primary(&mut self, top: bool) -> PResult<Pattern> {
        // Literal patterns: int / str / bool. Float is intentionally not a pattern. A leading `-`
        // begins a negative int literal/range pattern (int-only; `-3.0` routes through
        // `expect_pattern_int` → `expect_int` and is rejected as "expected integer, found float").
        match self.peek() {
            Token::Int(_) | Token::Minus => {
                let start = self.expect_pattern_int()?;
                // `start..end` — a half-open integer range pattern (matches `start <= v < end`);
                // `start..=end` is the inclusive form.
                let inclusive = self.check(&Token::DotDotEq);
                if self.eat(&Token::DotDot) || self.eat(&Token::DotDotEq) {
                    let end = self.expect_pattern_int()?;
                    return Ok(Pattern::Range {
                        start,
                        end,
                        inclusive,
                    });
                }
                return Ok(Pattern::Literal(LitPattern::Int(start)));
            }
            // A string pattern is decoded exactly like the same literal in expression position
            // (TICKET-139/W14-22): `{{`/`}}` are literal braces and an all-digit hole is text
            // (DEC-018), but a real `{expr}` hole is a parse error -- a pattern matches literals,
            // it never interpolates. This must stay HERE: `Token::RawStr` below lands in the same
            // `LitPattern::Str`, so no later phase can tell `r"{x}"` from a holed `"{x}"`.
            Token::Str(s) => {
                let Some(text) = s.plain() else {
                    return Err(self.err(
                        "a string pattern cannot interpolate: `{...}` is a hole, not text; write `{{` and `}}` for literal braces, or bind the value and compare it in a guard".to_string(),
                    ));
                };
                let text = text.to_string();
                self.advance();
                return Ok(Pattern::Literal(LitPattern::Str(text)));
            }
            // A raw string is an ordinary `str` literal in pattern position too.
            Token::RawStr(s) => {
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
            // `?p` / `!p` -- a carrier pattern. The prefixes are collected in a loop, so a long run
            // costs no native stack. The operand is always a sub-position: a bare name there binds.
            Token::Question | Token::Bang => {
                use crate::ast::CarrierTag;
                let mut tags = Vec::new();
                loop {
                    match self.peek() {
                        Token::Question => tags.push(CarrierTag::Present),
                        Token::Bang => tags.push(CarrierTag::Error),
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
                        | Token::QuestionDot
                        | Token::QuestionQuestion
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
                        | Token::Eof => break,
                    }
                    self.advance();
                }
                let mut pat = if self.check(&Token::LParen) {
                    self.parse_tuple_pattern()?
                } else {
                    self.parse_pattern_primary(false)?
                };
                for tag in tags.into_iter().rev() {
                    pat = Pattern::Carrier {
                        tag,
                        inner: Box::new(pat),
                        id: crate::ast::NodeId::fresh(),
                    };
                }
                return Ok(pat);
            }
            // `_` is an identifier; it's a wildcard unless followed by `(` (a payload, i.e. a
            // variant literally named `_`).
            Token::Ident(name) if is_blank_name(name) && self.peek_at(1) != &Token::LParen => {
                self.advance();
                return Ok(Pattern::Wildcard);
            }
            Token::IntMinMagnitude
            | Token::Float(_)
            | Token::Bytes(_)
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
            | Token::NoneKw
            | Token::Where
            | Token::Plus
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
            | Token::QuestionDot
            | Token::QuestionQuestion
            | Token::Amp
            | Token::Caret
            | Token::BitOr
            | Token::Shl
            | Token::Shr
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
            | Token::Eof => {}
        }
        // Span of the binding-name token (used only when this resolves to a bare-ident binding,
        // below) for the decl-site hover. A qualified/variant pattern ignores it.
        let name_span = self.cur_span();
        let none_kw = self.check(&Token::NoneKw);
        let name = self.expect_ident_or_none()?;
        // `Enum.Variant` — a qualified variant pattern. The first ident is the enum qualifier; the
        // ident after `.` is the variant. A qualified pattern is always a variant (never a binding),
        // even in a sub-position. A THIRD ident after a second `.` (`module.Enum.Variant`) shifts the
        // first ident to a module binder, the second to the enum, the third to the variant —
        // symmetric with module-qualified construction (`geo.Color.Red`). This is unambiguous: a
        // 2-part variant pattern can't be followed by a `.` today, so a 3rd dot deterministically
        // means module-qualified. Four or more segments (`pkg.deep.Color.Red`) name the module by
        // its full dotted path (TICKET-175): the last two are the enum and the variant.
        let mut segments = vec![name];
        while self.eat(&Token::Dot) {
            segments.push(self.expect_ident()?);
        }
        let (name, enum_name, module_name) = match segments.len() {
            1 => (segments.pop().unwrap_or_default(), None, None),
            2 => {
                // `Enum.Variant`
                let variant = segments.pop().unwrap_or_default();
                (variant, segments.pop(), None)
            }
            _ => {
                // `module.Enum.Variant`, where `module` may be a dotted full path
                let variant = segments.pop().unwrap_or_default();
                let enum_name = segments.pop();
                (variant, enum_name, Some(segments.join(".")))
            }
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
            return Ok(Pattern::Variant {
                id: crate::ast::NodeId::fresh(),
                name,
                bindings,
                enum_name,
                module_name,
            });
        }
        // A bare identifier: a nullary variant at the top of an arm, or a binding in a sub-position.
        // A qualified `Enum.Variant` is unambiguously a nullary variant in either position.
        if top || none_kw || enum_name.is_some() {
            Ok(Pattern::Variant {
                id: crate::ast::NodeId::fresh(),
                name,
                bindings: Vec::new(),
                enum_name,
                module_name,
            })
        } else {
            Ok(Pattern::Ident(name, name_span, crate::ast::NodeId::fresh()))
        }
    }

    /// Parse `( p1, p2, … )`. A single parenthesised pattern `(p)` is grouping (returns `p`);
    /// two-or-more elements form a `Tuple` matching a tuple value of that arity.
    fn parse_tuple_pattern(&mut self) -> PResult<Pattern> {
        self.parse_paren_list(Self::parse_subpattern)
    }

    /// Parse `( elem (, elem)* ,? )`, the one parenthesized pattern list (a match tuple pattern and a
    /// nested binding target share it). One element with no comma is grouping and returns it; a
    /// comma makes a `Tuple`, so `(a,)` is a 1-tuple.
    fn parse_paren_list(&mut self, elem: fn(&mut Self) -> PResult<Pattern>) -> PResult<Pattern> {
        self.expect(&Token::LParen)?;
        let first = elem(self)?;
        let (mut elems, comma) = self.comma_tail(first, &[Token::RParen], elem)?;
        self.expect(&Token::RParen)?;
        if comma {
            Ok(Pattern::Tuple(elems))
        } else {
            Ok(elems.pop().expect("one element"))
        }
    }

    /// One element of a binding target: a name, `_` (skips a slot), or a parenthesized nested
    /// target. A keyword in a name slot gets DEC-237's one message from `expect_ident`.
    fn parse_bind_target(&mut self) -> PResult<Pattern> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.err("pattern nested too deeply".to_string()));
        }
        let target = if self.check(&Token::LParen) {
            // A one-name group `(x)` is that name, as in CPython.
            self.parse_paren_list(Self::parse_bind_target)?
        } else {
            let span = self.cur_span();
            let name = self.expect_ident()?;
            bind_leaf(name, span)
        };
        self.depth -= 1;
        Ok(target)
    }

    /// A comma list of binding-target elements (`a, (b, c)`) that ends before `end`; the bool says it
    /// had a comma, so `a,` is a 1-tuple target.
    fn parse_bind_target_list(&mut self, end: &Token) -> PResult<(Vec<Pattern>, bool)> {
        let first = self.parse_bind_target()?;
        self.comma_tail(first, std::slice::from_ref(end), Self::parse_bind_target)
    }

    /// `for` and comprehension targets: a bare name, `_`, or a comma list. A target that is one
    /// parenthesized tuple (`for (a, b) in xs`) is an error: an unpacking target takes no outer
    /// parentheses. One element with no comma is itself; a comma makes a `Tuple` (`for a, in xs`).
    pub(super) fn parse_loop_target(&mut self) -> PResult<Pattern> {
        let span = self.cur_span();
        let parenthesized = self.check(&Token::LParen);
        let (mut elems, comma) = self.parse_bind_target_list(&Token::In)?;
        let target = if comma {
            Pattern::Tuple(elems)
        } else {
            let only = elems.pop().expect("one element");
            if parenthesized && matches!(only, Pattern::Tuple(_)) {
                return Err(ParseError {
                    message: "an unpacking target takes no outer parentheses; write the names separated by commas".to_string(),
                    span,
                });
            }
            only
        };
        reject_repeated_binders(&target)?;
        Ok(target)
    }

    /// Speculatively read `<target> :=`. Commits (`Some`, the `:=` eaten) only for a name, `_` (each
    /// possibly grouped, `(x)`) or a top-level comma list; every other shape restores `pos`, `depth` and `fold_depth` and
    /// returns `None`, so the expression path reports the error (`bind_target_error`).
    pub(super) fn try_parse_walrus_target(&mut self) -> PResult<Option<Pattern>> {
        if !matches!(self.peek(), Token::Ident(_) | Token::LParen) {
            return Ok(None);
        }
        let save = self.pos;
        let save_depth = self.depth;
        let save_fold = self.fold_depth;
        if let Ok((mut elems, comma)) = self.parse_bind_target_list(&Token::Walrus)
            && self.check(&Token::Walrus)
            && (comma || !matches!(elems[0], Pattern::Tuple(_)))
        {
            self.advance(); // ':='
            let target = if comma {
                Pattern::Tuple(elems)
            } else {
                elems.pop().expect("one element")
            };
            reject_repeated_binders(&target)?;
            return Ok(Some(target));
        }
        self.pos = save;
        self.depth = save_depth;
        self.fold_depth = save_fold;
        Ok(None)
    }

    /// The right side of `:=`, `=`, `return` and `yield`: one value, or a comma list that is a
    /// tuple written without parentheses. Only the first value is a value position (DEC-260).
    /// The bool says the right side was a list.
    pub(super) fn parse_value_list(&mut self, site: ValueSite) -> PResult<(Expr, bool)> {
        let first = self.parse_value(site)?;
        if self.ended_at_block() || self.peek() != &Token::Comma {
            return Ok((first, false));
        }
        let span = first.span;
        // No trailing comma here: `x := 1,` is an error, not a 1-tuple.
        let (values, _) = self.comma_tail(first, &[], Self::parse_expr)?;
        Ok((
            Expr {
                id: crate::ast::NodeId::fresh(),
                kind: ExprKind::Tuple(values),
                span,
            },
            true,
        ))
    }
}

/// A binding target binds each name once (Go: `a repeated on left side of :=`; Rust: E0416), as a
/// match pattern does (`Checker::bind_match_arm`).
fn reject_repeated_binders(target: &Pattern) -> PResult<()> {
    let names = target.binders();
    for (i, (name, span)) in names.iter().enumerate() {
        if names[..i].iter().any(|(n, _)| n == name) {
            return Err(ParseError {
                message: format!("identifier '{name}' is bound more than once in this pattern"),
                span: *span,
            });
        }
    }
    Ok(())
}

/// The one leaf of a binding target: `_` skips a slot (`Pattern::Wildcard`), any other name binds
/// (`Pattern::Ident`). Every target the parser builds (`:=`, typed let, `for`, comprehension, wait
/// arm) goes through it, so a blank is encoded one way.
pub(super) fn bind_leaf(name: String, span: Span) -> Pattern {
    if is_blank_name(&name) {
        Pattern::Wildcard
    } else {
        Pattern::Ident(name, span, crate::ast::NodeId::fresh())
    }
}

/// True for the compound-assignment tokens (`+= -= *= /= %= &= |= ^= <<= >>=`). Used to give a
/// clear error when one appears with a multi-target list (`a, b += 1`).
pub(super) fn is_compound_assign(tok: &Token) -> bool {
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
