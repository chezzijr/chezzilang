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

/// A neutral interpolation-parse error: a message and the (whole-string) span. Callers map this to
/// their own error type.
#[derive(Debug)]
pub(crate) struct InterpError {
    pub message: String,
    pub span: Span,
}

#[derive(Debug)]
pub(crate) enum Chunk {
    Lit(String),
    /// An interpolated `{expr}` or `{expr:spec}`; the format spec (parsed at compile time) is
    /// `None` for a bare `{expr}`.
    Expr(Expr, Option<crate::fmtspec::FormatSpec>),
}

/// Split an interpolated string literal into literal/expr chunks, mirroring `interp::interpolate`
/// (but at compile time): `{{`/`}}` are literal braces; each `{ … }` is lexed + parsed as an
/// expression. A malformed interpolation surfaces here as an error.
pub(crate) fn parse_interpolation(raw: &str, span: Span) -> Result<Vec<Chunk>, InterpError> {
    let mut chunks = Vec::new();
    let mut lit = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                lit.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                lit.push('}');
            }
            '{' => {
                if !lit.is_empty() {
                    chunks.push(Chunk::Lit(std::mem::take(&mut lit)));
                }
                let mut inner = String::new();
                let mut closed = false;
                for ic in chars.by_ref() {
                    if ic == '}' {
                        closed = true;
                        break;
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
                let expr = parse_expr_str(expr_src, span, base_line)?;
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

fn parse_expr_str(src: &str, span: Span, base_line: usize) -> Result<Expr, InterpError> {
    let tokens = lexer::tokenize_at(src, base_line).map_err(|e| InterpError {
        message: e.to_string(),
        span,
    })?;
    let mut expr = parser::parse_expr(tokens).map_err(|e| InterpError {
        message: e.message,
        span,
    })?;
    // Fragments bypass the module-wide desugar pass; lower `?.`/`??` carriers here (both engines do).
    crate::desugar::lower_carriers(&mut expr);
    Ok(expr)
}
