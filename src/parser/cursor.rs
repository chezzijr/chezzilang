//! Parser: the token cursor: peek, advance, expect, identifiers and literals, line ends, block
//! ends.
//! Split out of `src/parser/mod.rs`. `super::*` is the parent module.

use super::*;
use crate::lexer::{Span, Tok, Token};

impl Parser {
    /// The current token kind. The stream always ends with `Eof`, so this never goes out of range.
    pub(super) fn peek(&self) -> &Token {
        &self.toks[self.pos].kind
    }

    /// The token kind `n` positions ahead (or `Eof` past the end).
    pub(super) fn peek_at(&self, n: usize) -> &Token {
        self.toks
            .get(self.pos + n)
            .map(|t| &t.kind)
            .unwrap_or(&Token::Eof)
    }

    /// The span of the current token.
    pub(super) fn cur_span(&self) -> Span {
        self.toks[self.pos].span
    }

    /// Consume and return the current token, clamping at the trailing `Eof`.
    pub(super) fn advance(&mut self) -> Tok {
        let tok = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        tok
    }

    pub(super) fn check(&self, kind: &Token) -> bool {
        self.peek() == kind
    }

    /// Consume the current token if it matches `kind`; report whether it did.
    pub(super) fn eat(&mut self, kind: &Token) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// At the `=` that follows a type: a plain `=`, or the `!=` whose `!` that type has taken.
    pub(super) fn at_type_assign(&self) -> bool {
        self.check(&Token::Assign) || (self.check(&Token::NotEq) && self.bang_eq == Some(self.pos))
    }

    /// Consume the `=` that follows a type (see [`Self::at_type_assign`]); report whether it did.
    pub(super) fn eat_type_assign(&mut self) -> bool {
        if self.at_type_assign() {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Consume the current token, erroring if it isn't `kind`.
    pub(super) fn expect(&mut self, kind: &Token) -> PResult<Tok> {
        if self.check(kind) {
            Ok(self.advance())
        } else if let Some(open) = self.unclosed_opener(kind) {
            Err(ParseError {
                message: format!("{} was never closed", describe(&open.kind)),
                span: open.span,
            })
        } else {
            Err(self.err(format!(
                "expected {}, found {}",
                describe(kind),
                describe(self.peek())
            )))
        }
    }

    /// When `close` is a closing delimiter (`)`/`]`/`}`) and the token stream never closes the
    /// nearest matching opener before `self.pos`, return that opener. STATELESS by design — three
    /// backtracking sites restore `self.pos` by hand, so a delimiter stack maintained in `advance`
    /// would desync there. A forward scan runs FIRST: `f(a b)` must keep reporting `expected ')'`,
    /// because its `(` IS closed later, just not where `expect` looked.
    fn unclosed_opener(&self, close: &Token) -> Option<Tok> {
        let open_kind = match close {
            Token::RParen => Token::LParen,
            Token::RBracket => Token::LBracket,
            Token::RBrace => Token::LBrace,
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
            | Token::LBracket
            | Token::LBrace
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
        let mut depth: usize = 0;
        for t in &self.toks[self.pos..] {
            if t.kind == open_kind {
                depth += 1;
            } else if &t.kind == close {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
            }
        }
        let mut depth: usize = 0;
        for t in self.toks[..self.pos].iter().rev() {
            if &t.kind == close {
                depth += 1;
            } else if t.kind == open_kind {
                if depth == 0 {
                    return Some(t.clone());
                }
                depth -= 1;
            }
        }
        None
    }

    /// `expect_ident`, but the `None` keyword is a name too. Only for the two prelude-only
    /// productions that DECLARE the variant (a `native enum` variant, an import member in a file
    /// that declares a native enum) and for a pattern head.
    pub(super) fn expect_ident_or_none(&mut self) -> PResult<String> {
        if self.eat(&Token::NoneKw) {
            return Ok(crate::lexer::NONE.to_string());
        }
        self.expect_ident()
    }

    /// `expect_ident`, but the `from` keyword is a name too. Only for a member name: after `.` /
    /// `?.`, and a method name in a struct, enum, native-struct or protocol body (`T.from(x)`).
    pub(super) fn expect_member_name(&mut self) -> PResult<String> {
        if let Some(name) = super::member_name(self.peek()).map(str::to_string) {
            self.advance();
            return Ok(name);
        }
        self.expect_ident()
    }

    /// Consume an identifier, returning its name.
    pub(super) fn expect_ident(&mut self) -> PResult<String> {
        match self.peek() {
            Token::Ident(_) => {
                if let Token::Ident(name) = self.advance().kind {
                    Ok(name)
                } else {
                    unreachable!()
                }
            }
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
            | t @ Token::Colon
            | t @ Token::Dot
            | t @ Token::DotDot
            | t @ Token::DotDotEq
            | t @ Token::DotDotDot
            | t @ Token::Newline
            | t @ Token::Indent
            | t @ Token::Dedent
            | t @ Token::Eof => Err(self.keyword_name_error(t).unwrap_or_else(|| {
                self.err(format!(
                    "expected identifier, found {}",
                    describe(self.peek())
                ))
            })),
        }
    }

    /// The one error for a keyword in a name position; `None` when `t` is not a keyword.
    pub(super) fn keyword_name_error(&self, t: &Token) -> Option<ParseError> {
        let (w, _) = crate::lexer::KEYWORDS.iter().find(|(_, k)| k == t)?;
        Some(self.err(format!(
            "expected identifier, found reserved keyword '{w}' (a keyword cannot be used as a name)"
        )))
    }

    /// The error for an expression at `:=` that `try_parse_walrus_target` did not take: the one
    /// keyword-name error for a literal keyword (`None`, `true`, `false`), else `otherwise`.
    pub(super) fn bind_target_error(&self, target: Expr, otherwise: &str) -> ParseError {
        let kw = match target.kind {
            ExprKind::NoneLit => Some(Token::NoneKw),
            ExprKind::Bool(true) => Some(Token::True),
            ExprKind::Bool(false) => Some(Token::False),
            ExprKind::Ident(_)
            | ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Interp(_)
            | ExprKind::Bytes(_)
            | ExprKind::RawStr(_)
            | ExprKind::Pass
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
            | ExprKind::ElseGuard { .. } => None,
        };
        let mut e = kw
            .and_then(|t| self.keyword_name_error(&t))
            .unwrap_or_else(|| self.err(otherwise.to_string()));
        e.span = target.span;
        e
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
            // A bare 2^63 in integer position (range bound etc.) is out of i64 range. (The negated
            // i64::MIN form is folded upstream in `expect_pattern_int`, never reaching here.)
            Token::IntMinMagnitude => {
                Err(self.err("number too large to fit in target type".to_string()))
            }
            Token::Float(_)
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
                Err(self.err(format!("expected integer, found {}", describe(self.peek()))))
            }
        }
    }

    /// Expect an integer literal in pattern position, accepting an optional leading `-` for a
    /// negative int literal/range bound (int-only — a float range bound routes to `expect_int`,
    /// which rejects with "expected integer, found float"; a float literal pattern is parsed before
    /// this is reached).
    pub(super) fn expect_pattern_int(&mut self) -> PResult<i64> {
        if self.eat(&Token::Minus) {
            // i64::MIN carve-out in pattern/range position: `-9223372036854775808` folds to i64::MIN
            // directly (negating the 2^63 magnitude would overflow).
            if matches!(self.peek(), Token::IntMinMagnitude) {
                self.advance();
                return Ok(i64::MIN);
            }
            Ok(-self.expect_int()?)
        } else {
            self.expect_int()
        }
    }

    /// Expect a string literal (used for the library name in an `extern "lib":` header). A raw
    /// string (`r"lib"`) is an ordinary `str` literal and is accepted here too.
    pub(super) fn expect_str(&mut self) -> PResult<String> {
        match self.peek() {
            Token::Str(_) | Token::RawStr(_) => match self.advance().kind {
                Token::Str(s) => match s.plain() {
                    Some(text) => Ok(text.to_string()),
                    None => Err(self.err("an extern library name cannot interpolate".to_string())),
                },
                Token::RawStr(s) => Ok(s),
                Token::Int(_)
                | Token::IntMinMagnitude
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
                | Token::Eof => unreachable!(),
            },
            Token::Int(_)
            | Token::IntMinMagnitude
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
            | Token::Eof => Err(self.err(format!(
                "expected string literal, found {}",
                describe(self.peek())
            ))),
        }
    }

    /// The cursor is where a simple statement's line ends: a layout token, or the bare `else:` /
    /// `elif` that ends the one statement of an inline `if` body ([`Self::if_line_else`]).
    pub(super) fn at_line_end(&self) -> bool {
        matches!(self.peek(), Token::Newline | Token::Dedent | Token::Eof) || self.if_line_else()
    }

    /// The previous token is a `Dedent`: a block expression (`match`, `recover:` with an indented
    /// body) just ended, so the expression and its statement end here. The lexer emits no `Newline`
    /// after that `Dedent`, so the next token is the first token of the next line.
    pub(super) fn ended_at_block(&self) -> bool {
        self.pos > 0 && matches!(self.toks[self.pos - 1].kind, Token::Dedent)
    }

    /// Eat `t` as a continuation of the statement on this line (a `,` of a value or target list, the
    /// `:=` / `=` of a target list or a `wait` arm); never across a block's end.
    pub(super) fn eat_in_line(&mut self, t: &Token) -> bool {
        !self.ended_at_block() && self.eat(t)
    }

    /// The rest of a comma list whose first element is `first`: `(, elem)*`, with one optional
    /// trailing comma before a token of `end`. The one tuple shape of a value, a type, a pattern
    /// and a target (S2-7). The bool says the list had a comma: `x,` and `(x,)` are a 1-tuple;
    /// `x` and `(x)` are `x`. An empty `end` admits no trailing comma.
    pub(super) fn comma_tail<T>(
        &mut self,
        first: T,
        end: &[Token],
        mut elem: impl FnMut(&mut Self) -> PResult<T>,
    ) -> PResult<(Vec<T>, bool)> {
        let mut elems = vec![first];
        let mut comma = false;
        while self.eat_in_line(&Token::Comma) {
            comma = true;
            if end.iter().any(|t| self.check(t)) {
                break;
            }
            elems.push(elem(self)?);
        }
        Ok((elems, comma))
    }

    /// A simple statement must end the logical line; otherwise trailing tokens are a syntax error
    /// (e.g. `x := 5 y := 6` packed onto one line).
    pub(super) fn expect_stmt_end(&mut self) -> PResult<()> {
        if self.at_line_end() {
            return Ok(());
        }
        // A simple statement whose value is a block-valued expression (`x := match s:` with
        // indented arms) ended with that block ([`Self::ended_at_block`]): there is no separate
        // line terminator left to require.
        if self.ended_at_block() {
            return Ok(());
        }
        Err(self.err(format!(
            "expected end of line, found {}",
            describe(self.peek())
        )))
    }

    /// A member of a `struct`/`enum`/native body must end its logical line: `docs/reference/grammar.bnf`'s
    /// `<field>`, `<variant>` and `<nativeMethodDecl>` all end in `"NEWLINE"`. A block-valued field
    /// default ends with its block ([`Self::ended_at_block`]), with no NEWLINE of its own.
    pub(super) fn expect_member_end(&self, what: &str, name: &str) -> PResult<()> {
        if matches!(self.peek(), Token::Newline | Token::Dedent | Token::Eof)
            || self.ended_at_block()
        {
            return Ok(());
        }
        Err(self.err(format!(
            "expected newline after {what} '{name}', found {}",
            describe(self.peek())
        )))
    }

    pub(super) fn err(&self, message: String) -> ParseError {
        ParseError {
            message,
            span: self.cur_span(),
        }
    }

    /// Skip any run of `Newline` tokens (blank separation between statements).
    pub(super) fn skip_newlines(&mut self) {
        while self.check(&Token::Newline) {
            self.advance();
        }
    }

    /// Does the rest of the current logical line contain `kind`? (Used to tell the two `import`
    /// forms apart: the `… from …` form always has a `from` before the line's newline.)
    pub(super) fn rest_of_line_has(&self, kind: &Token) -> bool {
        let mut i = self.pos;
        while let Some(t) = self.toks.get(i) {
            match &t.kind {
                Token::Newline | Token::Indent | Token::Dedent | Token::Eof => return false,
                k @ Token::Int(_)
                | k @ Token::IntMinMagnitude
                | k @ Token::Float(_)
                | k @ Token::Str(_)
                | k @ Token::Bytes(_)
                | k @ Token::RawStr(_)
                | k @ Token::Ident(_)
                | k @ Token::Fn
                | k @ Token::Return
                | k @ Token::If
                | k @ Token::Else
                | k @ Token::Elif
                | k @ Token::For
                | k @ Token::While
                | k @ Token::In
                | k @ Token::Break
                | k @ Token::Continue
                | k @ Token::Pass
                | k @ Token::Struct
                | k @ Token::Enum
                | k @ Token::Protocol
                | k @ Token::Type
                | k @ Token::Match
                | k @ Token::Recover
                | k @ Token::Defer
                | k @ Token::Assert
                | k @ Token::Test
                | k @ Token::Spawn
                | k @ Token::Parallel
                | k @ Token::Wait
                | k @ Token::Yield
                | k @ Token::Import
                | k @ Token::Extern
                | k @ Token::Native
                | k @ Token::From
                | k @ Token::As
                | k @ Token::Const
                | k @ Token::And
                | k @ Token::Or
                | k @ Token::Not
                | k @ Token::True
                | k @ Token::False
                | k @ Token::NoneKw
                | k @ Token::Where
                | k @ Token::Plus
                | k @ Token::Minus
                | k @ Token::Star
                | k @ Token::Slash
                | k @ Token::Percent
                | k @ Token::Assign
                | k @ Token::Walrus
                | k @ Token::EqEq
                | k @ Token::NotEq
                | k @ Token::Lt
                | k @ Token::LtEq
                | k @ Token::Gt
                | k @ Token::GtEq
                | k @ Token::PlusEq
                | k @ Token::MinusEq
                | k @ Token::StarEq
                | k @ Token::SlashEq
                | k @ Token::PercentEq
                | k @ Token::AmpEq
                | k @ Token::PipeEq
                | k @ Token::CaretEq
                | k @ Token::ShlEq
                | k @ Token::ShrEq
                | k @ Token::Arrow
                | k @ Token::Question
                | k @ Token::QuestionDot
                | k @ Token::QuestionQuestion
                | k @ Token::Bang
                | k @ Token::Amp
                | k @ Token::Caret
                | k @ Token::BitOr
                | k @ Token::Shl
                | k @ Token::Shr
                | k @ Token::LParen
                | k @ Token::RParen
                | k @ Token::LBracket
                | k @ Token::RBracket
                | k @ Token::LBrace
                | k @ Token::RBrace
                | k @ Token::Comma
                | k @ Token::Colon
                | k @ Token::Dot
                | k @ Token::DotDot
                | k @ Token::DotDotEq
                | k @ Token::DotDotDot
                    if k == kind =>
                {
                    return true;
                }
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
                | Token::DotDotDot => i += 1,
            }
        }
        false
    }

    // ----- top level -----
}

/// Render a token as the user would recognize it, for error messages — `':='` not `Walrus`.
pub(super) fn describe(tok: &Token) -> String {
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
        DotDotEq => "'..='",
        Newline => "end of line",
        Indent => "an indented block",
        Dedent => "a dedent",
        Eof => "end of input",
        Ident(name) => return format!("identifier '{name}'"),
        Int(n) => return format!("integer {n}"),
        IntMinMagnitude => return "integer 9223372036854775808".to_string(),
        Float(f) => return format!("float {f}"),
        Str(_) => "a string literal",
        Bytes(_) => "a byte-string literal",
        RawStr(_) => "a raw-string literal",
        // operators and keywords print their source spelling
        other @ Fn
        | other @ Return
        | other @ If
        | other @ Else
        | other @ Elif
        | other @ For
        | other @ While
        | other @ In
        | other @ Break
        | other @ Continue
        | other @ Pass
        | other @ Struct
        | other @ Enum
        | other @ Protocol
        | other @ Type
        | other @ Match
        | other @ Recover
        | other @ Defer
        | other @ Assert
        | other @ Test
        | other @ Spawn
        | other @ Parallel
        | other @ Wait
        | other @ Yield
        | other @ Import
        | other @ Extern
        | other @ Native
        | other @ From
        | other @ As
        | other @ Const
        | other @ And
        | other @ Or
        | other @ Not
        | other @ True
        | other @ False
        | other @ NoneKw
        | other @ Where
        | other @ StarEq
        | other @ SlashEq
        | other @ PercentEq
        | other @ AmpEq
        | other @ PipeEq
        | other @ CaretEq
        | other @ ShlEq
        | other @ ShrEq
        | other @ QuestionDot
        | other @ QuestionQuestion
        | other @ Amp
        | other @ Caret
        | other @ BitOr
        | other @ Shl
        | other @ Shr
        | other @ DotDotDot => match other.lexeme() {
            Some(l) => return format!("'{l}'"),
            None => return format!("'{}'", format!("{other:?}").to_lowercase()),
        },
    };
    s.to_string()
}

/// [`describe`] without its leading article, for the two messages that put `unexpected` in front
/// (`unexpected indented block in expression`, not `unexpected an indented block ...`). CPython 3.14
/// on a stray indent prints `IndentationError: unexpected indent` — no article. The other callers
/// read `found {}` / `expected {}` / `{} was never closed`, where the article is correct English.
pub(super) fn describe_unexpected(tok: &Token) -> String {
    let d = describe(tok);
    match d.strip_prefix("an ").or_else(|| d.strip_prefix("a ")) {
        Some(rest) => rest.to_string(),
        None => d,
    }
}
