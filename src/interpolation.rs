//! Shared compile-time parser for interpolated string literals.
//!
//! Both the **checker** (to type-check `{...}` fragment expressions and surface undefined names /
//! type errors as compile errors) and the **compiler** (to emit interpolation bytecode) split a
//! raw string literal into literal/expr chunks here. Keeping a single parser is parity-critical: if
//! the checker and the compiler diverged on how a string is chunked, the checker could pass a
//! program the compiler then mis-emits (or vice versa). One parser, two callers.
//!
//! Mirrors `interp::interpolate` (the runtime path) at compile time: `{{`/`}}` are literal braces;
//! each `{ … }` is lexed + parsed as an expression (with an optional `:spec` format spec).
//!
//! Errors are returned as a neutral [`InterpError`] (message + span); each caller maps it to its own
//! error type (`CompileError` / `CheckError`).

use crate::ast::{Expr, Span};
use crate::lexer::{self, PosMap, StrLit};
use crate::parser;
use std::sync::Arc;

/// Re-export: a parsed chunk lives in the AST (`ExprKind::Interp` carries them), so `desugar` can
/// store what this parser produced instead of every consumer re-parsing the raw text.
pub(crate) use crate::ast::Chunk;

/// A neutral interpolation-parse error: a message and the (whole-string) span. Callers map this to
/// their own error type.
#[derive(Debug)]
pub(crate) struct InterpError {
    pub message: String,
    pub span: Span,
}

/// Split an interpolated string literal into literal/expr chunks, mirroring `interp::interpolate`
/// (but at compile time): `{{`/`}}` are literal braces; each `{ … }` is lexed + parsed as an
/// expression. A malformed interpolation surfaces here as an error.
pub(crate) fn parse_interpolation(lit_tok: &StrLit, span: Span) -> Result<Vec<Chunk>, InterpError> {
    let raw: &str = lit_tok;
    // The literal's content-index → source-`Span` map, resolved ONCE for all its fragments.
    let map = map_for(lit_tok, span);
    let mut chunks = Vec::new();
    let mut lit = String::new();
    // `i` is the char index in `raw` — it is the fragment's key into `map`, so it must be tracked,
    // not recomputed. `{{`/`}}` are consumed by `chars.next()` below, so `i` stays a true `raw`
    // index across them.
    let mut chars = raw.chars().enumerate().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '{' if chars.peek().map(|(_, c)| *c) == Some('{') => {
                chars.next();
                lit.push('{');
            }
            '}' if chars.peek().map(|(_, c)| *c) == Some('}') => {
                chars.next();
                lit.push('}');
            }
            '{' => {
                if !lit.is_empty() {
                    chunks.push(Chunk::Lit(std::mem::take(&mut lit)));
                }
                // Scan to the fragment's CLOSING `}` with the same state machine
                // `fmtspec::split_spec` uses one line below: a `}` inside a `"`/`'` string literal
                // or nested inside `([{` is part of the expression, NOT the terminator. Without
                // this, `{d['a}b']}` cut at the quoted brace and `{ {1,2}.len() }` at the set
                // literal's — both hard compile errors on valid code.
                let mut inner = String::new();
                let mut closed = false;
                let mut depth: i32 = 0;
                let mut in_str: Option<char> = None;
                // Past the top-level `:` the rest of the fragment is the FORMAT SPEC — literal text,
                // not an expression. Quote/bracket tracking must stop there or a spec whose fill
                // char is `'`, `(` or `)` (`"{x:'>5}"`, `"{x:(>5}"` — both legal, both CPython) is
                // read as an unterminated string / unbalanced bracket and the fragment never closes.
                // Only brace nesting still counts in spec text (CPython's nested `{width}` field).
                let mut in_spec = false;
                for (_, ic) in chars.by_ref() {
                    if in_spec {
                        match ic {
                            '{' => depth += 1,
                            '}' if depth == 0 => {
                                closed = true;
                                break;
                            }
                            '}' => depth -= 1,
                            _ => {}
                        }
                        inner.push(ic);
                        continue;
                    }
                    if let Some(q) = in_str {
                        if ic == q {
                            in_str = None;
                        }
                        inner.push(ic);
                        continue;
                    }
                    match ic {
                        '"' | '\'' => in_str = Some(ic),
                        '(' | '[' | '{' => depth += 1,
                        ')' | ']' => depth -= 1,
                        '}' if depth == 0 => {
                            closed = true;
                            break;
                        }
                        '}' => depth -= 1,
                        // The spec separator — the same top-level `:` `split_spec` splits on, and
                        // skipped for the same reason on a ternary, whose colons are structural.
                        ':' if depth == 0 && !crate::fmtspec::is_ternary_head(&inner) => {
                            in_spec = true;
                        }
                        _ => {}
                    }
                    inner.push(ic);
                }
                if !closed {
                    return Err(InterpError {
                        message: "unterminated '{' in interpolated string".to_string(),
                        span,
                    });
                }
                // Split on the first top-level `:` into (expr, spec); a `:` inside brackets/quotes
                // (e.g. `{m["a:b"]}`, slices `a[1:2]`) is NOT a separator. Spec parse errors are
                // surfaced as compile errors (good UX); type/value mismatches are deferred to the VM.
                let (expr_src, spec_src) = crate::fmtspec::split_spec(&inner);
                let spec = match spec_src {
                    Some(s) => Some(
                        crate::fmtspec::parse(s)
                            .map_err(|message| InterpError { message, span })?,
                    ),
                    None => None,
                };
                // Re-lex the fragment against the enclosing literal's source map, so every token
                // span it produces is the REAL physical position of that char in the file — line
                // AND column, past real newlines, `\n` escapes, `\u{…}` escapes and any nesting
                // depth alike (`docs/gaps.md` M24-6).
                //
                // This is not cosmetic. A span is a cross-half TABLE KEY (`WitnessTable`,
                // `KeywordTable`, `CarrierTable`), so two fragments sharing a span means the second
                // call silently takes the first's entry — a wrong value under a green `chezzi
                // check`, measured and fixed once already in `2a27697e` (which bought the property
                // by keeping a monotone OFFSET; this buys it outright, because two distinct source
                // chars are two distinct positions by construction). Do not "reset" anything here.
                // `PosMap`'s doc carries the injectivity proof.
                //
                // The trim lives HERE, not in `parse_expr_str`, so `lead` and the trim cannot
                // disagree about what whitespace is — one derivation, not two.
                let lead = expr_src.chars().take_while(|c| c.is_whitespace()).count();
                let off = i + 1 + lead;
                let expr = parse_expr_str(expr_src.trim(), span, map.clone(), off)?;
                chunks.push(Chunk::Expr(expr, spec));
            }
            '}' => {
                return Err(InterpError {
                    message: "unmatched '}' in string (use '}}' for a literal brace)".to_string(),
                    span,
                });
            }
            _ => lit.push(c),
        }
    }
    if !lit.is_empty() {
        chunks.push(Chunk::Lit(lit));
    }
    Ok(chunks)
}

/// The map a fragment of `lit` re-lexes against.
///
/// A brace-free literal never reaches here, so `None` means the literal was SYNTHESIZED (no lexer
/// built it). The fallback is the affine map anchored one column past the literal's own span, which
/// is **exactly** the truth for a single-delimiter, escape-free, newline-free literal — not an
/// approximation of it. A fragment belongs to its enclosing literal's file by definition, so the
/// fallback inherits `span.file` too (`docs/gaps.md` W7-49).
fn map_for(lit: &StrLit, span: Span) -> Arc<PosMap> {
    match &lit.map {
        Some(m) => m.clone(),
        None => Arc::new(PosMap::flat(Span {
            line: span.line,
            col: span.col + 1,
            file: span.file,
        })),
    }
}

/// `src` is the fragment's expression text, ALREADY TRIMMED by the caller (which also folded the
/// dropped leading run into `off`). It must be trimmed: the fragment is lexed as its own line, so
/// leading whitespace (`"{ 1 + 2 }"`, legal in Python) would otherwise open an INDENT token.
fn parse_expr_str(
    src: &str,
    span: Span,
    map: Arc<PosMap>,
    off: usize,
) -> Result<Expr, InterpError> {
    let tokens = lexer::tokenize_frag(src, map, off).map_err(|e| InterpError {
        message: e.to_string(),
        span,
    })?;
    // W7-43 — no carrier lowering here any more: `?.`/`??` are ordinary expressions that survive to
    // the checker and the compiler, and both set the `kw_frag_ctx`/`kw_frag_ord` discriminators a
    // fragment's `CarrierKey` needs (`checker::check_interp_chunks`, `compiler::compile_interp`).
    parser::parse_expr(tokens).map_err(|e| InterpError {
        message: e.message,
        span,
    })
}
