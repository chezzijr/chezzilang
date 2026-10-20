//! Parser: expressions: value sites, the Pratt loop, unary, postfix, ranges, subscripts, call args,
//! brackets, primaries.
//! Split out of `src/parser/mod.rs`. `super::*` is the parent module.

use super::cursor::describe_unexpected;
use super::*;
use crate::lexer::{Span, Token};

impl Parser {
    /// Parse an expression that starts a whole value at `_site`: the one place a block expression may
    /// start ([`Parser::block_ok`]).
    pub(super) fn parse_value(&mut self, site: ValueSite) -> PResult<Expr> {
        self.open_value(site, self.pos);
        self.parse_expr()
    }

    /// Mark the token at `at` as the start of a whole value at `site`: the one writer of
    /// `value_start`. `parse_fn` calls it for an inline body, whose first token is not `self.pos`.
    pub(super) fn open_value(&mut self, _site: ValueSite, at: usize) {
        self.value_start = Some(at);
    }

    /// May the block expression whose keyword is the token at `kw` start here? Only the token where a
    /// whole value starts qualifies, which excludes every operand, prefix, argument and element.
    pub(super) fn block_ok(&self, kw: usize) -> bool {
        self.value_start == Some(kw)
    }

    pub(super) fn parse_expr(&mut self) -> PResult<Expr> {
        self.parse_bp(0)
    }

    /// Precedence-climbing core. `min_bp` is the minimum left binding power that may bind here.
    fn parse_bp(&mut self, min_bp: u8) -> PResult<Expr> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.too_deep());
        }
        // Each fold below adds one level to a left-leaning AST that the recursive front-end walkers
        // descend. Iterative, so the `self.depth` guard above never sees it — the bound is
        // `MAX_AST_DEPTH` over the synthesized `fold_depth` (see that constant). Reset before the
        // children so their fold-depths accumulate here, restore `max`-combined on the way out.
        let outer_fold = self.fold_depth;
        self.fold_depth = 0;
        // `not` sits between `and` and the comparisons (Python's grammar), not in the unary tier,
        // so `not x in xs` is `not (x in xs)` and `1 + not y` is a parse error. Gated on `min_bp`
        // rather than a token check alone, so `not` still parses in every loose position (an `if`
        // condition, an `and`/`or` operand, a call argument, the operand of another `not`).
        let mut lhs = if matches!(self.peek(), Token::Not) && min_bp <= NOT_BP {
            let not_span = self.cur_span();
            self.advance();
            let expr = self.parse_bp(NOT_BP)?;
            Expr {
                id: crate::ast::NodeId::fresh(),
                kind: ExprKind::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(expr),
                },
                span: not_span,
            }
        } else if matches!(self.peek(), Token::DotDot | Token::DotDotEq) && min_bp <= RANGE_BP {
            // `..b`, `..=b` and `..`: a range with no start.
            let span = self.cur_span();
            let inclusive = self.check(&Token::DotDotEq);
            self.advance();
            let end = self.parse_range_end(inclusive, span)?;
            Expr {
                id: crate::ast::NodeId::fresh(),
                kind: ExprKind::Range {
                    start: None,
                    end,
                    inclusive,
                },
                span,
            }
        } else {
            self.parse_unary()?
        };
        let mut chain = 0usize;
        while let Some((op, l_bp, r_bp)) = infix_op(self.peek()) {
            if l_bp < min_bp || self.ended_at_block() {
                break;
            }
            chain += 1;
            if self.depth + self.fold_depth + chain > MAX_AST_DEPTH {
                return Err(self.too_deep());
            }
            let op_span = self.cur_span();
            let inclusive = self.check(&Token::DotDotEq);
            self.advance(); // the operator
            if let InfixOp::Range = op {
                if matches!(lhs.kind, ExprKind::Range { .. }) {
                    return Err(ParseError {
                        message: "a range has no third bound; write `Range(a, b, step)` for a step"
                            .to_string(),
                        span: op_span,
                    });
                }
                let end = self.parse_range_end(inclusive, op_span)?;
                let span = lhs.span;
                lhs = Expr {
                    id: crate::ast::NodeId::fresh(),
                    kind: ExprKind::Range {
                        start: Some(Box::new(lhs)),
                        end,
                        inclusive,
                    },
                    span,
                };
                continue;
            }
            let rhs = self.parse_bp(r_bp)?;
            let span = lhs.span;
            lhs = match op {
                // TICKET-077: a comparison operator CHAINS with any further comparison at the same
                // level, Python-style — consume every adjacent link here rather than letting the
                // outer `while` fold them pairwise (which would nest as `(a < b) == c`).
                InfixOp::Bin(op) if is_comparison(op) => {
                    let mut operands = vec![lhs, rhs];
                    let mut ops = vec![op];
                    while let Some((InfixOp::Bin(next_op), next_l, next_r)) = infix_op(self.peek())
                        .filter(|(o, ..)| matches!(o, InfixOp::Bin(nop) if is_comparison(*nop)))
                    {
                        if next_l < min_bp || self.ended_at_block() {
                            break;
                        }
                        chain += 1;
                        if self.depth + self.fold_depth + chain > MAX_AST_DEPTH {
                            return Err(self.too_deep());
                        }
                        self.advance();
                        let next_rhs = self.parse_bp(next_r)?;
                        operands.push(next_rhs);
                        ops.push(next_op);
                    }
                    if ops.len() == 1 {
                        Expr {
                            id: crate::ast::NodeId::fresh(),
                            kind: ExprKind::Binary {
                                op: ops[0],
                                lhs: Box::new(operands.remove(0)),
                                rhs: Box::new(operands.remove(0)),
                            },
                            span,
                        }
                    } else {
                        Expr {
                            id: crate::ast::NodeId::fresh(),
                            kind: ExprKind::Compare { operands, ops },
                            span,
                        }
                    }
                }
                InfixOp::Bin(op) => Expr {
                    id: crate::ast::NodeId::fresh(),
                    kind: ExprKind::Binary {
                        op,
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    },
                    span,
                },
                InfixOp::Range => unreachable!("a range is built before the generic operand parse"),
                InfixOp::Coalesce => Expr {
                    id: crate::ast::NodeId::fresh_block(crate::ast::CARRIER_IDS),
                    kind: ExprKind::NullCoalesce {
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                        op_span,
                    },
                    span,
                },
                // `lhs |> f(args)` desugars at parse time to `f(lhs, args)` — threading `lhs` as the
                // first argument. The RHS must be a call, so checker/interp/VM see a plain call and
                // need no pipe-specific code.
                InfixOp::Pipe => match rhs.kind {
                    ExprKind::Call {
                        callee,
                        args,
                        named,
                        type_args,
                        bracket,
                    } => {
                        if !named.is_empty() {
                            return Err(self.err(
                                "named arguments are not supported on the right side of '|>'"
                                    .to_string(),
                            ));
                        }
                        let mut new_args = Vec::with_capacity(args.len() + 1);
                        new_args.push(lhs);
                        new_args.extend(args);
                        Expr {
                            id: crate::ast::NodeId::fresh(),
                            kind: ExprKind::Call {
                                callee,
                                args: new_args,
                                named,
                                type_args,
                                bracket,
                            },
                            span,
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
                    | ExprKind::Field { .. }
                    | ExprKind::Index { .. }
                    | ExprKind::Try(_)
                    | ExprKind::OptChain { .. }
                    | ExprKind::NullCoalesce { .. }
                    | ExprKind::Closure { .. }
                    | ExprKind::Match { .. }
                    | ExprKind::IfElse { .. }
                    | ExprKind::Recover(..)
                    | ExprKind::ElseGuard { .. } => {
                        return Err(
                            self.err("right side of '|>' must be a function call".to_string())
                        );
                    }
                },
            };
        }
        self.fold_depth = outer_fold.max(self.fold_depth + chain);
        self.depth -= 1;
        Ok(lhs)
    }

    /// Prefix unary operators (`-`, `!` error value, `?` present/success value), then a postfix
    /// chain. `not` is parsed one level up.
    fn parse_unary(&mut self) -> PResult<Expr> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.too_deep());
        }
        let span = self.cur_span();
        let op = match self.peek() {
            Token::Minus => Some(UnaryOp::Neg),
            Token::Bang => Some(UnaryOp::ErrVal),
            Token::Question => Some(UnaryOp::Wrap),
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
            | Token::Eof => None,
        };
        let result = if let Some(op) = op {
            self.advance();
            // i64::MIN carve-out: `-9223372036854775808` folds straight to Int(i64::MIN). The lexer
            // emits the 2^63 magnitude as IntMinMagnitude (it does not fit i64 as a positive value);
            // runtime-negating it would overflow, so emit the literal directly here.
            if matches!(op, UnaryOp::Neg) && matches!(self.peek(), Token::IntMinMagnitude) {
                self.advance();
                Ok(Expr {
                    id: crate::ast::NodeId::fresh(),
                    kind: ExprKind::Int(i64::MIN),
                    span,
                })
            } else {
                let expr = self.parse_unary()?;
                Ok(Expr {
                    id: crate::ast::NodeId::fresh(),
                    kind: ExprKind::Unary {
                        op,
                        expr: Box::new(expr),
                    },
                    span,
                })
            }
        } else {
            self.parse_postfix()
        };
        self.depth -= 1;
        result
    }

    /// A primary expression followed by any chain of `(call)`, `.field`, `[index]`, `?`.
    fn parse_postfix(&mut self) -> PResult<Expr> {
        // Same accounting as the infix loop in `parse_bp`: `x.f.f…` / `a[0][0]…` / `f().g()…` build a
        // left-leaning AST the walkers descend, and this loop never bumps `self.depth`. Count every
        // iteration (incl. the turbofish `continue` and the final non-folding one — an overcount of
        // at most one per frame, itself bounded by `MAX_DEPTH`).
        let outer_fold = self.fold_depth;
        self.fold_depth = 0;
        let mut e = self.parse_primary()?;
        let mut chain = 0usize;
        loop {
            chain += 1;
            if self.depth + self.fold_depth + chain > MAX_AST_DEPTH {
                return Err(self.too_deep());
            }
            let span = e.span;
            if self.ended_at_block() {
                break;
            }
            e = match self.peek() {
                Token::LParen => {
                    self.advance();
                    let (args, named) = self.parse_call_args()?;
                    Expr {
                        id: crate::ast::NodeId::fresh(),
                        kind: ExprKind::Call {
                            callee: Box::new(e),
                            args,
                            named,
                            type_args: Vec::new(),
                            bracket: None,
                        },
                        span,
                    }
                }
                Token::Dot => {
                    self.advance();
                    // `obj.field` (struct/module) or `tuple.0` (element access). A numeric field name
                    // is the tuple-element index, stored as its decimal string (reusing `Field`).
                    // Capture the field-name token's TRUE span before consuming it (editor hover keys on it).
                    let name_span = self.cur_span();
                    let name = if let Token::Int(n) = self.peek() {
                        let n = *n;
                        self.advance();
                        n.to_string()
                    } else {
                        self.expect_ident()?
                    };
                    // TICKET-187: `.decode[T](s)` is no longer special here. It parses as an
                    // ordinary member call with a type argument (`try_parse_type_arg_call`), and
                    // the checker decides that only `decode` on the `std.json` module is JSON.
                    Expr {
                        id: crate::ast::NodeId::fresh(),
                        kind: ExprKind::Field {
                            obj: Box::new(e),
                            name,
                            name_span,
                        },
                        span,
                    }
                }
                Token::LBracket => {
                    let headed = e.kind.is_named_head();
                    // `name[Type, …](args)` — explicit call-site type arguments. A bare name can be a
                    // generic fn / struct / variant; ANY member access (`recv.name`, a `Field` over an
                    // arbitrary receiver) can be a method turbofish (`recv.name[U](args)`) — the receiver
                    // need NOT be a bare ident (`W(1).cast[str]("a")`, `xs[0].cast[U](x)`, `h.w.cast[U](x)`
                    // all qualify). SPECULATIVE: `try_parse_type_arg_call` only commits when it sees the
                    // exact `[types](` shape, restoring pos+depth otherwise, so a genuine subscript that
                    // is NOT a type-then-call (`obj.items[0]`, `m.data[k]`, the numeric `arr[0].h[0](x)`)
                    // backtracks and keeps its plain index (then-call) meaning. A one-arg bracket keeps
                    // its expression reading too (`bracket`), and the checker chooses (TICKET-210): a
                    // head that denotes data (`fs[k](10)`, `h.fs[k](10)`) indexes then calls, with no
                    // parens. The parser reads no scope (DEC-204).
                    let type_call = if headed {
                        self.try_parse_type_arg_call(&e, span)?
                    } else {
                        None
                    };
                    match type_call {
                        Some(call) => call,
                        None => self.parse_bracket(e, span)?,
                    }
                }
                Token::Question => {
                    self.advance();
                    Expr {
                        id: crate::ast::NodeId::fresh(),
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
                    // Capture the name token's TRUE span before consuming it — `span` above is the
                    // PRIMARY's span, shared by every link of `a?.b?.c`, so `name_span` is the only
                    // per-link key this node has (mirrors the `.` arm).
                    let name_span = self.cur_span();
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
                        Some(OptCall {
                            args,
                            named,
                            type_args: Vec::new(),
                        })
                    } else {
                        None
                    };
                    Expr {
                        id: crate::ast::NodeId::fresh_block(crate::ast::CARRIER_IDS),
                        kind: ExprKind::OptChain {
                            obj: Box::new(e),
                            name,
                            name_span,
                            call,
                        },
                        span,
                    }
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
                | Token::Pipe
                | Token::QuestionQuestion
                | Token::Bang
                | Token::Amp
                | Token::Caret
                | Token::BitOr
                | Token::Shl
                | Token::Shr
                | Token::RParen
                | Token::RBracket
                | Token::LBrace
                | Token::RBrace
                | Token::Comma
                | Token::Colon
                | Token::DotDot
                | Token::DotDotEq
                | Token::DotDotDot
                | Token::Newline
                | Token::Indent
                | Token::Dedent
                | Token::Eof => break,
            };
        }
        self.fold_depth = outer_fold.max(self.fold_depth + chain);
        Ok(e)
    }

    /// The end bound of a range whose operator was just consumed: an expression above `..`'s own
    /// level, or none when the next token cannot start one (`5..`, `..`). `a..=` needs an end.
    fn parse_range_end(&mut self, inclusive: bool, span: Span) -> PResult<Option<Box<Expr>>> {
        if token_ends_range(self.peek()) || self.ended_at_block() {
            if inclusive {
                return Err(ParseError {
                    message: "an inclusive range needs an end".to_string(),
                    span,
                });
            }
            return Ok(None);
        }
        Ok(Some(Box::new(self.parse_bp(RANGE_BP + 1)?)))
    }

    /// Parse the inside of a subscript `[ ... ]` (the `[` already consumed) and the closing `]`,
    /// producing an `Index` whose index is one expression: an int, a key, or a `Range` (`xs[a..b]`
    /// is a subscript by a range value; the checker decides whether it slices).
    fn parse_subscript(&mut self, obj: Expr, span: Span) -> PResult<Expr> {
        // `xs[]` (empty) is rejected as a missing index.
        if self.check(&Token::RBracket) {
            return Err(ParseError {
                message: "expected an index expression".to_string(),
                span,
            });
        }
        let index = Box::new(self.parse_expr()?);
        self.expect(&Token::RBracket)?;
        Ok(Expr {
            id: crate::ast::NodeId::fresh(),
            kind: ExprKind::Index {
                obj: Box::new(obj),
                index: Some(index),
                types: Vec::new(),
            },
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

    /// Parse a bracket `obj[…]` that is not a `[types](` call, the `[` not yet consumed, into one
    /// `Index` carrying every reading that parses (TICKET-222): `types` by the type grammar
    /// (`parse_type`, comma-separated; only on an `Ident` or non-tuple `Field` head) and `index` by
    /// `parse_subscript`'s expression grammar, kept only when it ends at the same `]`. A slice is
    /// returned as `parse_subscript` builds it. When no reading parses, `parse_subscript` runs
    /// again non-speculatively so its error surfaces. The parser never chooses; the checker does.
    /// On any other head the type reading is tried only to name the mistake: a type list that
    /// parses where the subscript does not (`1[T, U]`) reports [`TYPE_LIST_HEAD`].
    fn parse_bracket(&mut self, obj: Expr, span: Span) -> PResult<Expr> {
        let headed = obj.kind.is_named_head();
        let save = self.pos;
        // `parse_type` bumps `self.depth` and only unwinds it on success; restore on backtrack.
        // `fold_depth` rides along for the same reason (see `MAX_AST_DEPTH`).
        let save_depth = self.depth;
        let save_fold = self.fold_depth;
        let restore = |p: &mut Self, pos: usize| {
            p.pos = pos;
            p.depth = save_depth;
            p.fold_depth = save_fold;
        };
        let mut types = Vec::new();
        let mut end = None;
        self.advance(); // '['
        loop {
            match self.parse_type() {
                Ok(t) => types.push(t),
                Err(_) => {
                    types.clear();
                    break;
                }
            }
            if !self.eat(&Token::Comma) {
                break;
            }
        }
        if !types.is_empty() && self.eat(&Token::RBracket) {
            end = Some(self.pos);
        } else {
            types.clear();
        }
        restore(self, save);
        let Some(end) = end.filter(|_| headed) else {
            let bracket = self.cur_span();
            self.advance(); // '['
            return self.parse_subscript(obj, span).map_err(|e| {
                if end.is_some() {
                    ParseError {
                        message: TYPE_LIST_HEAD.to_string(),
                        span: bracket,
                    }
                } else {
                    e
                }
            });
        };
        self.advance(); // '['
        let index = match self.parse_subscript(obj.clone(), span) {
            Ok(Expr {
                kind: ExprKind::Index { index, .. },
                ..
            }) if self.pos == end => index,
            _ => None,
        };
        restore(self, end);
        Ok(Expr {
            id: crate::ast::NodeId::fresh(),
            kind: ExprKind::Index {
                obj: Box::new(obj),
                index,
                types,
            },
            span,
        })
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
        // such indexes spuriously trip MAX_DEPTH. `fold_depth` rides along for the same reason.
        let save_depth = self.depth;
        let save_fold = self.fold_depth;
        self.advance(); // '['
        let mut type_args = Vec::new();
        loop {
            match self.parse_type() {
                Ok(t) => type_args.push(t),
                Err(_) => {
                    self.pos = save;
                    self.depth = save_depth;
                    self.fold_depth = save_fold;
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
            self.fold_depth = save_fold;
            return Ok(None);
        }
        // A one-arg bracket on a bare-name or member head keeps its expression reading too: the
        // checker, which knows the scope, chooses index-then-call when the head denotes data
        // (`fs[k](10)`). The parser reads no scope (DEC-204).
        let bracket = self.bracket_expression(callee, &type_args, save, save_depth, save_fold);
        self.advance(); // '(' — committed to a type-argument call now.
        let (args, named) = self.parse_call_args()?;
        Ok(Some(Expr {
            id: crate::ast::NodeId::fresh(),
            kind: ExprKind::Call {
                callee: Box::new(callee.clone()),
                args,
                named,
                type_args,
                bracket,
            },
            span,
        }))
    }

    /// The expression reading of the bracket `[X]` that `try_parse_type_arg_call` just read as one
    /// type, from `save` (the `[`) to the `(` at the current position. `Some` only for an `Ident`
    /// head or a non-tuple-index `Field` head, and only when `X` parses as an expression that ends
    /// at the `]`. Leaves the position at the `(`, with `depth` and `fold_depth` restored.
    fn bracket_expression(
        &mut self,
        callee: &Expr,
        type_args: &[Type],
        save: usize,
        save_depth: usize,
        save_fold: usize,
    ) -> Option<Box<Expr>> {
        let headed = callee.kind.is_named_head();
        if !headed || type_args.len() != 1 {
            return None;
        }
        let end = self.pos;
        self.pos = save + 1;
        self.depth = save_depth;
        self.fold_depth = save_fold;
        let parsed = self.parse_expr();
        let fits = self.pos + 1 == end;
        self.pos = end;
        self.depth = save_depth;
        self.fold_depth = save_fold;
        match parsed {
            Ok(e) if fits => Some(Box::new(e)),
            _ => None,
        }
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        let tok = self.advance();
        let span = tok.span;
        let kind = match tok.kind {
            Token::Int(n) => ExprKind::Int(n),
            // Bare (un-negated) 2^63: legal ONLY as the operand of a unary minus (folded in
            // `parse_unary`). Reaching here means it stands alone → out of i64 range.
            Token::IntMinMagnitude => {
                return Err(ParseError {
                    message: "number too large to fit in target type".to_string(),
                    span,
                });
            }
            Token::Float(f) => ExprKind::Float(f),
            Token::Str(s) => match s.plain() {
                Some(text) => ExprKind::Str(text.to_string()),
                None => ExprKind::Interp(self.interp_chunks(s)?),
            },
            Token::Bytes(b) => ExprKind::Bytes(b),
            Token::RawStr(s) => ExprKind::RawStr(s),
            Token::True => ExprKind::Bool(true),
            Token::False => ExprKind::Bool(false),
            Token::NoneKw => ExprKind::NoneLit,
            Token::Ident(name) => ExprKind::Ident(name),
            Token::LParen => {
                // `()` is an error; `(e)` is grouping; `(e,)` and `(e1, e2, …)` are tuples.
                if self.check(&Token::RParen) {
                    return Err(self.err("unexpected ')' in expression".to_string()));
                }
                let first = self.parse_expr()?;
                let (mut elems, comma) =
                    self.comma_tail(first, &[Token::RParen], Self::parse_expr)?;
                self.expect(&Token::RParen)?;
                if !comma {
                    // grouped — keep the inner expr (and its span)
                    return Ok(elems.pop().expect("one element"));
                }
                ExprKind::Tuple(elems)
            }
            Token::LBracket => {
                // `[]` is the empty list. A first element followed by `for` is a list
                // comprehension (`[elem for x in xs if g]`); otherwise a plain list literal.
                if self.check(&Token::RBracket) {
                    self.advance();
                    ExprKind::List(Vec::new(), None)
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
                        ExprKind::List(elems, None)
                    }
                }
            }
            Token::LBrace => {
                // `{}` is the empty map. Otherwise the first element decides: a `key: value` pair
                // makes it a map (or, before `for`, a map comprehension); a bare expression makes
                // it a set (or a set comprehension before `for`). The empty set is `Set()`.
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
            other @ Token::Return
            | other @ Token::Else
            | other @ Token::Elif
            | other @ Token::For
            | other @ Token::While
            | other @ Token::In
            | other @ Token::Break
            | other @ Token::Continue
            | other @ Token::Pass
            | other @ Token::Struct
            | other @ Token::Enum
            | other @ Token::Protocol
            | other @ Token::Type
            | other @ Token::Defer
            | other @ Token::Assert
            | other @ Token::Test
            | other @ Token::Spawn
            | other @ Token::Parallel
            | other @ Token::Wait
            | other @ Token::Yield
            | other @ Token::Import
            | other @ Token::Extern
            | other @ Token::Native
            | other @ Token::From
            | other @ Token::As
            | other @ Token::Const
            | other @ Token::And
            | other @ Token::Or
            | other @ Token::Not
            | other @ Token::Where
            | other @ Token::Plus
            | other @ Token::Minus
            | other @ Token::Star
            | other @ Token::Slash
            | other @ Token::Percent
            | other @ Token::Assign
            | other @ Token::Walrus
            | other @ Token::EqEq
            | other @ Token::NotEq
            | other @ Token::Lt
            | other @ Token::LtEq
            | other @ Token::Gt
            | other @ Token::GtEq
            | other @ Token::PlusEq
            | other @ Token::MinusEq
            | other @ Token::StarEq
            | other @ Token::SlashEq
            | other @ Token::PercentEq
            | other @ Token::AmpEq
            | other @ Token::PipeEq
            | other @ Token::CaretEq
            | other @ Token::ShlEq
            | other @ Token::ShrEq
            | other @ Token::Arrow
            | other @ Token::Pipe
            | other @ Token::Question
            | other @ Token::QuestionDot
            | other @ Token::QuestionQuestion
            | other @ Token::Bang
            | other @ Token::Amp
            | other @ Token::Caret
            | other @ Token::BitOr
            | other @ Token::Shl
            | other @ Token::Shr
            | other @ Token::RParen
            | other @ Token::RBracket
            | other @ Token::RBrace
            | other @ Token::Comma
            | other @ Token::Colon
            | other @ Token::Dot
            | other @ Token::DotDot
            | other @ Token::DotDotEq
            | other @ Token::DotDotDot
            | other @ Token::Newline
            | other @ Token::Indent
            | other @ Token::Dedent
            | other @ Token::Eof => {
                return Err(ParseError {
                    message: format!("unexpected {} in expression", describe_unexpected(&other)),
                    span,
                });
            }
        };
        Ok(Expr {
            id: crate::ast::NodeId::fresh(),
            kind,
            span,
        })
    }
}

/// An infix operator: a normal binary op, the range marker `..`, or the pipe `|>` (M6).
enum InfixOp {
    Bin(BinaryOp),
    Range,
    Pipe,
    /// Null-coalescing `??` — right-associative carrier. It SURVIVES desugar (W7-43); the checker
    /// lowers it to a `match` via `desugar::lower_carrier_option`, and rejects a non-`Option` lhs.
    Coalesce,
}

/// Binding power of prefix `not`, gating `parse_bp`'s `min_bp <= NOT_BP` check. It sits between
/// `and` (5) and the comparisons (7), matching Python: `not x in xs` is `not (x in xs)`.
const NOT_BP: u8 = 6;

/// Binding power of `..` / `..=` (TICKET-253): below arithmetic, shifts and the bitwise operators,
/// above comparison, `in`, `not`, `and` and `or`. `0..1 << 2` is `0..(1 << 2)`; `x in 0..3` and
/// `0..3 == 0..3` need no parentheses. A range does not chain: `a..b..c` is an error.
const RANGE_BP: u8 = 9;

/// A range with no end bound (`5..`, `xs[2..]`, `..`) is a range whose next token cannot start an
/// expression. The list names every token that closes or continues a surrounding construct.
fn token_ends_range(t: &Token) -> bool {
    matches!(
        t,
        Token::Colon
            | Token::RBracket
            | Token::RParen
            | Token::RBrace
            | Token::Comma
            | Token::Newline
            | Token::Eof
            | Token::Dedent
            | Token::And
            | Token::Or
            | Token::Pipe
            | Token::Assign
            | Token::EqEq
            | Token::NotEq
            | Token::Lt
            | Token::LtEq
            | Token::Gt
            | Token::GtEq
            | Token::In
            | Token::Else
            | Token::Elif
            | Token::For
            | Token::If
            | Token::BitOr
            | Token::Caret
            | Token::Amp
            | Token::Shl
            | Token::Shr
            | Token::Plus
            | Token::Star
            | Token::Slash
            | Token::Percent
    )
}

/// Map a token to its infix operator and (left, right) binding powers, per `docs/reference/syntax/` §4.
/// All operators are left-associative (`right = left + 1`). `None` means "not an infix operator".
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
        Token::And => (Bin(And), 5),
        Token::EqEq => (Bin(Eq), 7),
        Token::NotEq => (Bin(NotEq), 7),
        // `in` membership — comparison-level precedence (same as `==`). `for x in xs:` never
        // reaches here: `parse_for`/`parse_comp_clause` consume `in` explicitly via `expect`.
        Token::In => (Bin(In), 7),
        // TICKET-077: all seven comparisons (`==`/`!=`/`in`/`<`/`<=`/`>`/`>=`) share ONE level and
        // CHAIN like Python (`a < b == c` means `a < b and b == c`) — see `parse_bp`'s comparison
        // fold and `ExprKind::Compare`.
        Token::Lt => (Bin(Lt), 7),
        Token::LtEq => (Bin(LtEq), 7),
        Token::Gt => (Bin(Gt), 7),
        Token::GtEq => (Bin(GtEq), 7),
        Token::BitOr => (Bin(BitOr), 11),
        Token::Caret => (Bin(BitXor), 13),
        Token::Amp => (Bin(BitAnd), 15),
        Token::Shl => (Bin(Shl), 17),
        Token::Shr => (Bin(Shr), 17),
        Token::DotDot | Token::DotDotEq => (Range, 9),
        Token::Plus => (Bin(Add), 21),
        Token::Minus => (Bin(Sub), 21),
        Token::Star => (Bin(Mul), 23),
        Token::Slash => (Bin(Div), 23),
        Token::Percent => (Bin(Mod), 23),
        // Null-coalescing `??`: binds tighter than every binary operator and looser than unary
        // `-` and the postfix chain, so `a ?? b + 1` is `(a ?? b) + 1`. RIGHT-associative
        // (`a ?? b ?? c` = `a ?? (b ?? c)`) — the one exception to the `l+1` left-assoc rule
        // above, so it returns early with equal left/right binding powers.
        Token::QuestionQuestion => return Some((Coalesce, 25, 25)),
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
        | Token::Not
        | Token::True
        | Token::False
        | Token::NoneKw
        | Token::Where
        | Token::Assign
        | Token::Walrus
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
        | Token::Bang
        | Token::LParen
        | Token::RParen
        | Token::LBracket
        | Token::RBracket
        | Token::LBrace
        | Token::RBrace
        | Token::Comma
        | Token::Colon
        | Token::Dot
        | Token::DotDotDot
        | Token::Newline
        | Token::Indent
        | Token::Dedent
        | Token::Eof => return None,
    };
    Some((op, l, l + 1))
}

/// True for the seven comparison operators, which all share one precedence level and CHAIN
/// (TICKET-077) — see `infix_op`'s comment and `parse_bp`'s comparison fold.
fn is_comparison(op: BinaryOp) -> bool {
    use BinaryOp::*;
    matches!(op, Eq | NotEq | In | Lt | LtEq | Gt | GtEq)
}
