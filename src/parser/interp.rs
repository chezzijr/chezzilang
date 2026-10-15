//! Parser: string interpolation chunks and holes, closures.
//! Split out of `src/parser/mod.rs`. `super::*` is the parent module.

use super::cursor::describe_unexpected;
use super::*;
use crate::fmtspec::SpecItem;
use crate::lexer::{Span, SpecPart, StrLit, StrPart, Tok, Token};

impl Parser {
    /// Build the chunks of an interpolated literal from what the lexer found: its text parts and
    /// its holes. The lexer owns where a hole starts and ends (`Lexer::quoted`); this only parses
    /// the tokens it handed over, so nothing here reads a brace, a quote or a colon.
    pub(super) fn interp_chunks(&mut self, s: StrLit) -> PResult<Vec<Chunk>> {
        let mut chunks = Vec::with_capacity(s.parts.len());
        for part in s.parts {
            match part {
                StrPart::Text(text) => chunks.push(Chunk::Lit(text)),
                StrPart::Hole(hole) => {
                    let expr = self.parse_hole(hole.tokens).map_err(|mut e| {
                        if hole.spec.is_some() {
                            e.message.push_str(
                                "; the first ':' outside brackets starts the format spec, so wrap an expression that contains ':' in parentheses",
                            );
                        }
                        e
                    })?;
                    let (spec, fields) = match hole.spec {
                        None => (None, Vec::new()),
                        Some(parts) => {
                            let mut items = Vec::new();
                            let mut fields = Vec::new();
                            for part in parts {
                                match part {
                                    SpecPart::Text(text) => {
                                        items.extend(text.chars().map(SpecItem::Ch));
                                    }
                                    SpecPart::Field(tokens) => {
                                        if let [only] = &tokens[..] {
                                            return Err(ParseError {
                                                message: "format spec: a nested field needs an expression, e.g. '{w}'".to_string(),
                                                span: only.span,
                                            });
                                        }
                                        items.push(SpecItem::Field);
                                        fields.push(self.parse_hole(tokens)?);
                                    }
                                }
                            }
                            let spec = crate::fmtspec::parse_nested(&items).map_err(|message| {
                                ParseError {
                                    message,
                                    span: hole.spec_at,
                                }
                            })?;
                            (Some(spec), fields)
                        }
                    };
                    chunks.push(Chunk::Expr(expr, spec, fields));
                }
            }
        }
        Ok(chunks)
    }

    /// Parse the tokens of one hole (or one nested spec field) as exactly one expression, on this
    /// parser: `depth` and `fold_depth` carry on, so a hole counts against the same bounds as the
    /// code around it. The hole's tokens stand in for the module's while it parses; token-indexed
    /// state (`if_line`, `bang_eq`) belongs to the module's tokens and is set aside.
    fn parse_hole(&mut self, tokens: Vec<Tok>) -> PResult<Expr> {
        self.hole_parsed = true;
        let outer_toks = std::mem::replace(&mut self.toks, tokens);
        let outer_pos = std::mem::replace(&mut self.pos, 0);
        let outer_if_line = self.if_line.take();
        let outer_value = self.value_start.take();
        let outer_inline = std::mem::replace(&mut self.stmt_inline, false);
        let outer_bang = self.bang_eq.take();
        let result = self.hole_expr();
        self.toks = outer_toks;
        self.pos = outer_pos;
        self.if_line = outer_if_line;
        self.value_start = outer_value;
        self.stmt_inline = outer_inline;
        self.bang_eq = outer_bang;
        result
    }

    fn hole_expr(&mut self) -> PResult<Expr> {
        self.skip_newlines();
        let expr = self.parse_expr()?;
        self.skip_newlines();
        if !self.check(&Token::Eof) {
            return Err(self.err(format!(
                "unexpected {} after expression",
                describe_unexpected(self.peek())
            )));
        }
        Ok(expr)
    }

    /// `fn(params) [-> ret]: expr` — the `fn` keyword has already been consumed.
    pub(super) fn parse_closure(&mut self, span: Span) -> PResult<Expr> {
        self.expect(&Token::LParen)?;
        let params = self.parse_params(false)?;
        self.expect(&Token::RParen)?;
        let ret = if self.eat(&Token::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(&Token::Colon)?;
        let body = if self.check(&Token::Pass) {
            let pass_span = self.cur_span();
            self.advance();
            Expr {
                id: crate::ast::NodeId::fresh(),
                kind: ExprKind::Pass,
                span: pass_span,
            }
        } else {
            self.parse_value(ValueSite::Lambda)?
        };
        Ok(Expr {
            id: crate::ast::NodeId::fresh(),
            kind: ExprKind::Closure {
                params,
                ret,
                body: Box::new(body),
            },
            span,
        })
    }
}
