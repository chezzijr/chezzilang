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
    // Running count of physical newlines consumed from `raw` before the current position, so each
    // fragment's re-lexed token spans can be re-anchored to its true source line (Bug E). Best-effort:
    // `raw` is post-escape, so a literal `\n` ESCAPE on one physical line would inflate this — the
    // same inherent ceiling as columns (genuine triple-quoted newlines are counted exactly).
    let mut newlines = 0usize;
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
                // Newlines consumed BEFORE this fragment's first char — its root line. Newlines
                // *inside* the fragment are tracked by the fragment lexer's own `self.line`, which
                // composes on top of this base, so multi-line fragments attribute correctly too.
                let frag_newlines = newlines;
                let mut inner = String::new();
                let mut closed = false;
                for ic in chars.by_ref() {
                    if ic == '}' {
                        closed = true;
                        break;
                    }
                    if ic == '\n' {
                        newlines += 1;
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
                // Re-anchor the fragment's re-lexed spans to the true source line: the string
                // literal opens on `span.line`, and `frag_newlines` physical newlines precede this
                // fragment. `base_line` offsets the fragment lexer's 1-based `self.line`.
                let base_line = span.line.saturating_sub(1) + frag_newlines;
                let expr = parse_expr_str(expr_src, span, base_line)?;
                chunks.push(Chunk::Expr(expr, spec));
            }
            '}' => {
                return Err(InterpError {
                    message: "unmatched '}' in string (use '}}' for a literal brace)".to_string(),
                    span,
                });
            }
            _ => {
                if c == '\n' {
                    newlines += 1;
                }
                lit.push(c);
            }
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
