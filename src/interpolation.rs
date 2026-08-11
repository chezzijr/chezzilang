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
use crate::lexer;
use crate::parser;

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
pub(crate) fn parse_interpolation(raw: &str, span: Span) -> Result<Vec<Chunk>, InterpError> {
    let mut chunks = Vec::new();
    let mut lit = String::new();
    // `i` is the char index in `raw` — it is what re-anchors a fragment's re-lexed spans to a real
    // COLUMN (see the `base_col` computation below), so it must be tracked, not recomputed.
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
                // Re-anchor the fragment's re-lexed spans to the string literal's OPENING source
                // line so a runtime fault inside `"{expr}"` reports a real, never-misleading line
                // (Bug E — previously always `line 1`). We anchor to the opening line rather than the
                // fragment's exact inner line because `raw` is the post-escape payload: a `\n` ESCAPE
                // and a genuine (triple-quoted) source newline are indistinguishable here, so counting
                // newlines in `raw` would inflate the line past an escape and point at unrelated code.
                // `base_line` offsets the fragment lexer's 1-based `self.line`; newlines INSIDE the
                // fragment itself still compose on top (those are real source lines, not escapes).
                let base_line = span.line.saturating_sub(1);
                // …and the same for the COLUMN, which is not cosmetic: a span is a cross-half table
                // key (`WitnessTable`, `KeywordTable`), so while every fragment restarted at column
                // 1, two fragments of two SIBLING nested literals produced the SAME key and the
                // second call silently took the first's witness — a wrong value under a green
                // `chezzi check`. Char `k` of `raw` sits at column `span.col + 1 + k` (the opening
                // delimiter plus the offset); this fragment's expression starts at `k = i + 1 +
                // lead` (past its `{` and the whitespace `parse_expr_str` trims); `span_at` adds a
                // 1-based intra-fragment offset on top, so what we hand it is one less than that.
                // What this is NOT is the physical column in every case, and the deviations are
                // all inherited from `raw` being the post-escape payload with the delimiter already
                // stripped: a TRIPLE-quoted literal is short by 2, each escape consumed before the
                // fragment shortens it by 1, and — the big one — a NEWLINE before the fragment (real
                // or escaped) does not reset it, because `base_line` does not advance for one either
                // (the two kinds are indistinguishable here, and advancing would point the LINE at
                // real, unrelated code). So past a newline this is an offset from the literal's
                // start and can exceed the physical line: filed as `docs/gaps.md` **M24-6**, with an
                // out-of-range column chosen deliberately over an accusing line.
                //
                // What it IS, unconditionally, is strictly increasing in the fragment's offset
                // within the literal — nested literals compose inside their parent's extent — so two
                // call sites can never share a key. That is the load-bearing property; recovering
                // the exact column would mean carrying a per-char source map on every string token.
                let lead = expr_src.chars().take_while(|c| c.is_whitespace()).count();
                let base_col = span.col + i + 1 + lead;
                let expr = parse_expr_str(expr_src, span, base_line, base_col)?;
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

fn parse_expr_str(
    src: &str,
    span: Span,
    base_line: usize,
    base_col: usize,
) -> Result<Expr, InterpError> {
    // TRIM first: the fragment is lexed as its own line, so leading whitespace (`"{ 1 + 2 }"`,
    // legal in Python) would otherwise open an INDENT token — "unexpected an indented block in
    // expression". Padding around a fragment is insignificant. `base_col` already accounts for the
    // leading run this drops.
    let src = src.trim();
    let tokens = lexer::tokenize_at(src, base_line, base_col).map_err(|e| InterpError {
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
