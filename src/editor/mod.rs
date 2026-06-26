//! Editor-tooling layer: the dependency-free core shared by the `chezzi-lsp` server and the VSCode
//! TextMate-grammar generator. Everything here wraps the existing front-end (lexer / parser /
//! checker / resolver) and contains no async / LSP types, so it runs in the default `cargo test`.
//!
//! Three capabilities:
//!   * [`diagnostics`] — run the real resolve → check pipeline over a live buffer and map every
//!     lex/parse/resolve/type error to a 0-based [`Diag`] range (the LSP push-diagnostics path).
//!   * [`semantic_tokens`] / [`encode_semantic_tokens`] — classify the lexer's token stream into the
//!     standard LSP semantic-token legend and delta-encode it (the neovim highlight path).
//!   * [`tmlanguage_json`] — emit the VSCode TextMate grammar, single-sourced from the lexer's
//!     `KEYWORDS` / `PUNCTUATION` tables.

use std::path::Path;

/// A diagnostic with a **0-based** half-open range, ready to map onto an LSP `Diagnostic`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diag {
    /// 0-based start line.
    pub line: u32,
    /// 0-based start character (UTF-16-agnostic char column; ASCII source in practice).
    pub col: u32,
    /// 0-based end line.
    pub end_line: u32,
    /// 0-based end character (exclusive).
    pub end_col: u32,
    /// Human-readable error message.
    pub message: String,
}

/// Type-check `source` as the entry module at `path` (imports resolve from disk) and return one
/// [`Diag`] per error. A clean program yields an empty vector.
///
/// This mirrors `chezzi check` exactly (resolve → desugar → check_graph) but feeds the **live**
/// buffer in for the entry module, so diagnostics reflect unsaved edits while cross-module imports
/// still resolve against the on-disk project.
pub fn diagnostics(path: &Path, source: &str) -> Vec<Diag> {
    use crate::{checker, resolver};
    match resolver::build_graph_with_entry_source(path, Some(source.to_string())) {
        // A resolve error wraps the fatal lex/parse error (or a missing/cyclic import).
        Err(e) => vec![span_diag(source, e.span.line, e.span.col, e.message)],
        Ok(graph) => match checker::check_graph(&graph) {
            Ok(()) => Vec::new(),
            Err(errs) => errs
                .into_iter()
                .map(|e| span_diag(source, e.span.line, e.span.col, e.message))
                .collect(),
        },
    }
}

/// 1-based `(line, col)` → 0-based [`Diag`], extending the end over an identifier word at the
/// position (so an undefined-name squiggle covers the whole name) or by one char otherwise.
fn span_diag(source: &str, line1: usize, col1: usize, message: String) -> Diag {
    let line0 = line1.saturating_sub(1) as u32;
    let col0 = col1.saturating_sub(1);
    let end_col = word_end_col(source, line1, col1);
    Diag {
        line: line0,
        col: col0 as u32,
        end_line: line0,
        end_col,
        message,
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The 0-based end column of the word starting at 1-based `(line, col)`, or `col` (0-based) + 1 when
/// the position is not on an identifier char.
fn word_end_col(source: &str, line1: usize, col1: usize) -> u32 {
    let col0 = col1.saturating_sub(1);
    let line = source.lines().nth(line1.saturating_sub(1)).unwrap_or("");
    let chars: Vec<char> = line.chars().collect();
    let mut end = col0;
    if end < chars.len() && is_word(chars[end]) {
        while end < chars.len() && is_word(chars[end]) {
            end += 1;
        }
    } else {
        end = col0 + 1;
    }
    end as u32
}

// ---------------------------------------------------------------------------
// Semantic tokens (the neovim highlight path).
// ---------------------------------------------------------------------------

/// The semantic-token legend, in legend order. The `u32` token-type of a [`SemTok`] indexes this
/// slice; the LSP server advertises exactly these names.
pub const SEMANTIC_TOKEN_TYPES: &[&str] = &[
    "keyword", "operator", "string", "number", "comment", "variable",
];

pub const KEYWORD: u32 = 0;
pub const OPERATOR: u32 = 1;
pub const STRING: u32 = 2;
pub const NUMBER: u32 = 3;
pub const COMMENT: u32 = 4;
pub const VARIABLE: u32 = 5;

/// One classified token with an absolute **0-based** position. `len` is in source characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemTok {
    pub line: u32,
    pub start: u32,
    pub len: u32,
    pub token_type: u32,
}

/// Classify `source` into LSP semantic tokens straight off the lexer's `Tok` stream. Keywords →
/// `keyword`, operators/delimiters → `operator`, string/byte/raw literals → `string`, int/float →
/// `number`, identifiers → `variable`; layout tokens are skipped. Comments (which the lexer strips)
/// are recovered by a `#`-scan that skips hashes living inside string literals.
///
/// A lexer error yields no tokens (the editor still shows the error via [`diagnostics`]).
pub fn semantic_tokens(source: &str) -> Vec<SemTok> {
    use crate::lexer::{self, Token};
    let chars: Vec<char> = source.chars().collect();
    let line_starts = line_start_offsets(&chars);
    let toks = match lexer::tokenize(source) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<SemTok> = Vec::new();
    // Absolute char ranges of string literals, so the comment scan can ignore `#` inside them.
    let mut string_extents: Vec<(usize, usize)> = Vec::new();

    for tok in &toks {
        let line = tok.span.line;
        let col = tok.span.col;
        // Absolute char offset of the token's first char.
        let start_abs = line_starts.get(line - 1).copied().unwrap_or(0) + (col - 1);
        let (ttype, len) = match &tok.kind {
            Token::Ident(name) => (VARIABLE, name.chars().count()),
            Token::Int(_) | Token::Float(_) => (NUMBER, measure_number(&chars, start_abs)),
            Token::Str(_) | Token::RawStr(_) | Token::Bytes(_) => {
                let l = measure_string(&chars, start_abs);
                string_extents.push((start_abs, start_abs + l));
                (STRING, l)
            }
            other => match other.lexeme() {
                // Keyword vs operator/delimiter: a keyword is in the KEYWORDS table.
                Some(lex) => {
                    let t = if lexer::KEYWORDS.iter().any(|(_, k)| k == other) {
                        KEYWORD
                    } else {
                        OPERATOR
                    };
                    (t, lex.chars().count())
                }
                // Layout (Newline/Indent/Dedent/Eof) — no highlight.
                None => continue,
            },
        };
        if len == 0 {
            continue;
        }
        out.push(SemTok {
            line: (line - 1) as u32,
            start: (col - 1) as u32,
            len: len as u32,
            token_type: ttype,
        });
    }

    append_comments(&chars, &string_extents, &mut out);
    out.sort_by_key(|t| (t.line, t.start));
    out
}

/// LSP delta-encoding: five `u32`s per token — `[deltaLine, deltaStart, length, tokenType,
/// tokenModifiers]`. `deltaStart` is relative to the previous token on the same line, else absolute.
/// Input is sorted by `semantic_tokens`.
pub fn encode_semantic_tokens(toks: &[SemTok]) -> Vec<u32> {
    let mut out = Vec::with_capacity(toks.len() * 5);
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;
    for t in toks {
        let d_line = t.line - prev_line;
        let d_start = if d_line == 0 {
            t.start - prev_start
        } else {
            t.start
        };
        out.extend_from_slice(&[d_line, d_start, t.len, t.token_type, 0]);
        prev_line = t.line;
        prev_start = t.start;
    }
    out
}

/// Char index in `chars` at which each (0-based) line begins.
fn line_start_offsets(chars: &[char]) -> Vec<usize> {
    let mut v = vec![0usize];
    for (i, &c) in chars.iter().enumerate() {
        if c == '\n' {
            v.push(i + 1);
        }
    }
    v
}

/// Char-length of the number literal starting at `start` — mirrors the lexer's `number()` extent
/// (radix prefixes, digit-group `_`, a single fraction, an `e`/`E` exponent), without re-parsing.
fn measure_number(chars: &[char], start: usize) -> usize {
    let at = |i: usize| chars.get(i).copied();
    let mut i = start;
    // Radix-prefixed integer: 0x / 0b / 0o then alphanumerics + underscores.
    if at(i) == Some('0') && matches!(at(i + 1), Some('x' | 'X' | 'b' | 'B' | 'o' | 'O')) {
        i += 2;
        while matches!(at(i), Some(c) if c.is_ascii_alphanumeric() || c == '_') {
            i += 1;
        }
        return i - start;
    }
    while matches!(at(i), Some(c) if c.is_ascii_digit() || c == '_') {
        i += 1;
    }
    // Fraction — only if the dot is followed by a digit (so `1.` and `0..10` are not eaten).
    if at(i) == Some('.') && matches!(at(i + 1), Some(c) if c.is_ascii_digit()) {
        i += 1;
        while matches!(at(i), Some(c) if c.is_ascii_digit() || c == '_') {
            i += 1;
        }
    }
    // Exponent — `e`/`E`, optional sign, one-or-more digits (else the `e` is not part of the number).
    if matches!(at(i), Some('e' | 'E')) {
        let mut probe = i + 1;
        if matches!(at(probe), Some('+' | '-')) {
            probe += 1;
        }
        if matches!(at(probe), Some(c) if c.is_ascii_digit()) {
            i = probe;
            while matches!(at(i), Some(c) if c.is_ascii_digit()) {
                i += 1;
            }
        }
    }
    i - start
}

/// Char-length of the string literal starting at `start`, covering the optional `b`/`r` prefix,
/// single/triple quotes of either style, and (for non-raw strings) backslash escapes. An unterminated
/// literal is measured to end-of-input (or end-of-line for a single-quoted one).
fn measure_string(chars: &[char], start: usize) -> usize {
    let at = |i: usize| chars.get(i).copied();
    let mut i = start;
    let mut raw = false;
    // Optional b/B (byte) or r/R (raw) prefix immediately before the quote.
    if matches!(at(i), Some('b' | 'B' | 'r' | 'R')) && matches!(at(i + 1), Some('"' | '\'')) {
        if matches!(at(i), Some('r' | 'R')) {
            raw = true;
        }
        i += 1;
    }
    let quote = match at(i) {
        Some(q @ ('"' | '\'')) => q,
        _ => return i - start, // not actually a string; defensive
    };
    let triple = at(i + 1) == Some(quote) && at(i + 2) == Some(quote);
    if triple {
        i += 3;
        loop {
            match at(i) {
                None => break,
                Some(c) if c == quote && at(i + 1) == Some(quote) && at(i + 2) == Some(quote) => {
                    i += 3;
                    break;
                }
                Some('\\') if !raw => i += 2, // skip the escaped char
                Some(_) => i += 1,
            }
        }
    } else {
        i += 1;
        loop {
            match at(i) {
                None | Some('\n') => break,
                Some(c) if c == quote => {
                    i += 1;
                    break;
                }
                Some('\\') if !raw => i += 2,
                Some(_) => i += 1,
            }
        }
    }
    i - start
}

/// Recover comment tokens (the lexer drops them): every `#` not inside a string literal opens a
/// comment that runs to end-of-line. `string_extents` are absolute char ranges to skip.
fn append_comments(chars: &[char], string_extents: &[(usize, usize)], out: &mut Vec<SemTok>) {
    let n = chars.len();
    let mut in_str = vec![false; n];
    for &(s, e) in string_extents {
        for slot in in_str.iter_mut().take(e.min(n)).skip(s.min(n)) {
            *slot = true;
        }
    }
    let mut i = 0usize;
    let mut line = 0u32;
    let mut col = 0u32;
    while i < n {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            col = 0;
            i += 1;
            continue;
        }
        if c == '#' && !in_str[i] {
            let start_col = col;
            let mut j = i;
            while j < n && chars[j] != '\n' {
                j += 1;
            }
            let len = (j - i) as u32;
            out.push(SemTok {
                line,
                start: start_col,
                len,
                token_type: COMMENT,
            });
            col += len;
            i = j;
            continue;
        }
        col += 1;
        i += 1;
    }
}

// ---------------------------------------------------------------------------
// VSCode TextMate grammar generator (single-sourced from the lexer tables).
// ---------------------------------------------------------------------------

/// Render the VSCode TextMate grammar (`.tmLanguage.json`) for Chezzi. The keyword alternation comes
/// from [`crate::lexer::KEYWORDS`] and the operator alternation from [`crate::lexer::PUNCTUATION`] via
/// [`crate::lexer::Token::lexeme`], so adding a keyword/operator to the lexer regenerates the grammar
/// — there is no hand-maintained list. Output is deterministic, zero-dependency JSON (2-space indent).
pub fn tmlanguage_json() -> String {
    use crate::lexer::{KEYWORDS, PUNCTUATION};

    // Keywords are all word characters → a single `\b(...)\b` alternation in declaration order.
    let kw_alt: String = KEYWORDS
        .iter()
        .map(|(w, _)| *w)
        .collect::<Vec<_>>()
        .join("|");

    // Operators/delimiters: regex-escape each lexeme, longest first so `<<=` beats `<<` beats `<`.
    let mut ops: Vec<&'static str> = PUNCTUATION.iter().filter_map(|t| t.lexeme()).collect();
    ops.sort_by(|a, b| b.chars().count().cmp(&a.chars().count()).then(a.cmp(b)));
    let op_alt: String = ops
        .iter()
        .map(|s| regex_escape(s))
        .collect::<Vec<_>>()
        .join("|");

    // Number literal: hex/bin/oct integer, or decimal with optional fraction + exponent.
    let number = "\\b(0[xX][0-9a-fA-F_]+|0[bB][01_]+|0[oO][0-7_]+|[0-9][0-9_]*(\\.[0-9][0-9_]*)?([eE][+-]?[0-9]+)?)\\b";

    let mut o = String::new();
    o.push_str("{\n");
    o.push_str("  \"$schema\": \"https://raw.githubusercontent.com/martinring/tmlanguage/master/tmlanguage.json\",\n");
    o.push_str("  \"name\": \"Chezzi\",\n");
    o.push_str("  \"scopeName\": \"source.chezzi\",\n");
    o.push_str("  \"patterns\": [\n");
    o.push_str("    { \"include\": \"#comments\" },\n");
    o.push_str("    { \"include\": \"#strings\" },\n");
    o.push_str("    { \"include\": \"#numbers\" },\n");
    o.push_str("    { \"include\": \"#keywords\" },\n");
    o.push_str("    { \"include\": \"#operators\" }\n");
    o.push_str("  ],\n");
    o.push_str("  \"repository\": {\n");

    // comments
    o.push_str("    \"comments\": {\n");
    o.push_str("      \"patterns\": [\n");
    o.push_str("        { \"name\": \"comment.line.number-sign.chezzi\", \"match\": ");
    o.push_str(&json_str_lit("#.*$"));
    o.push_str(" }\n");
    o.push_str("      ]\n");
    o.push_str("    },\n");

    // strings
    o.push_str("    \"strings\": {\n");
    o.push_str("      \"patterns\": [\n");
    o.push_str("        { \"name\": \"string.quoted.triple.chezzi\", \"begin\": ");
    o.push_str(&json_str_lit("[bBrR]?\"\"\""));
    o.push_str(", \"end\": ");
    o.push_str(&json_str_lit("\"\"\""));
    o.push_str(" },\n");
    o.push_str("        { \"name\": \"string.quoted.triple.chezzi\", \"begin\": ");
    o.push_str(&json_str_lit("[bBrR]?'''"));
    o.push_str(", \"end\": ");
    o.push_str(&json_str_lit("'''"));
    o.push_str(" },\n");
    o.push_str("        { \"name\": \"string.quoted.double.chezzi\", \"begin\": ");
    o.push_str(&json_str_lit("[bBrR]?\""));
    o.push_str(", \"end\": ");
    o.push_str(&json_str_lit("\""));
    o.push_str(", \"patterns\": [ { \"name\": \"constant.character.escape.chezzi\", \"match\": ");
    o.push_str(&json_str_lit("\\\\."));
    o.push_str(" } ] },\n");
    o.push_str("        { \"name\": \"string.quoted.single.chezzi\", \"begin\": ");
    o.push_str(&json_str_lit("[bBrR]?'"));
    o.push_str(", \"end\": ");
    o.push_str(&json_str_lit("'"));
    o.push_str(", \"patterns\": [ { \"name\": \"constant.character.escape.chezzi\", \"match\": ");
    o.push_str(&json_str_lit("\\\\."));
    o.push_str(" } ] }\n");
    o.push_str("      ]\n");
    o.push_str("    },\n");

    // numbers
    o.push_str("    \"numbers\": {\n");
    o.push_str("      \"patterns\": [\n");
    o.push_str("        { \"name\": \"constant.numeric.chezzi\", \"match\": ");
    o.push_str(&json_str_lit(number));
    o.push_str(" }\n");
    o.push_str("      ]\n");
    o.push_str("    },\n");

    // keywords (single-sourced)
    o.push_str("    \"keywords\": {\n");
    o.push_str("      \"patterns\": [\n");
    o.push_str("        { \"name\": \"keyword.control.chezzi\", \"match\": ");
    o.push_str(&json_str_lit(&format!("\\b({kw_alt})\\b")));
    o.push_str(" }\n");
    o.push_str("      ]\n");
    o.push_str("    },\n");

    // operators (single-sourced)
    o.push_str("    \"operators\": {\n");
    o.push_str("      \"patterns\": [\n");
    o.push_str("        { \"name\": \"keyword.operator.chezzi\", \"match\": ");
    o.push_str(&json_str_lit(&format!("({op_alt})")));
    o.push_str(" }\n");
    o.push_str("      ]\n");
    o.push_str("    }\n");

    o.push_str("  }\n");
    o.push_str("}\n");
    o
}

/// Backslash-escape the regex metacharacters in a fixed operator spelling.
fn regex_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            o.push('\\');
        }
        o.push(c);
    }
    o
}

/// Encode `s` as a JSON string literal (minimal, zero-dep — mirrors the CLI's `json_string`).
fn json_str_lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag(src: &str) -> Vec<Diag> {
        diagnostics(Path::new("/nonexistent/chezzi_editor/buf.chz"), src)
    }

    #[test]
    fn diag_clean_empty() {
        assert_eq!(diag("x := 1\n"), Vec::new());
    }

    #[test]
    fn diag_parse_error_pos() {
        let ds = diag("x := = 5\n");
        assert!(!ds.is_empty(), "broken syntax should produce a diagnostic");
        // The error is on the first line → 0-based line 0; col 6 (1-based) → 5 (0-based).
        assert_eq!(ds[0].line, 0);
        assert_eq!(ds[0].col, 5);
        assert!(ds[0].end_col > ds[0].col);
    }

    #[test]
    fn diag_type_error_pos() {
        // `zzz` is undefined; the error sits on line 2 (1-based) → 0-based 1, col 6 → 5.
        let ds = diag("a := 1\nb := zzz\n");
        assert!(!ds.is_empty(), "undefined name should produce a diagnostic");
        let d = &ds[0];
        assert_eq!(d.line, 1, "0-based line of the second source line");
        assert!(d.message.contains("zzz"));
        // The squiggle covers the whole `zzz` identifier (col 5..8, 0-based).
        assert_eq!(d.col, 5);
        assert_eq!(d.end_col, 8);
    }

    fn st(line: u32, start: u32, len: u32, ty: u32) -> SemTok {
        SemTok {
            line,
            start,
            len,
            token_type: ty,
        }
    }

    #[test]
    fn semtok_fn_keyword() {
        assert_eq!(semantic_tokens("fn"), vec![st(0, 0, 2, KEYWORD)]);
    }

    #[test]
    fn semtok_number_string_ident_op() {
        assert_eq!(semantic_tokens("123"), vec![st(0, 0, 3, NUMBER)]);
        // quotes are part of the highlighted string → `"hi"` is 4 chars.
        assert_eq!(semantic_tokens("\"hi\""), vec![st(0, 0, 4, STRING)]);
        assert_eq!(semantic_tokens("foo"), vec![st(0, 0, 3, VARIABLE)]);
        // `a+b` → variable, operator, variable, all length 1.
        assert_eq!(
            semantic_tokens("a+b"),
            vec![
                st(0, 0, 1, VARIABLE),
                st(0, 1, 1, OPERATOR),
                st(0, 2, 1, VARIABLE)
            ]
        );
    }

    #[test]
    fn semtok_float_and_raw_and_byte_string() {
        assert_eq!(semantic_tokens("3.14"), vec![st(0, 0, 4, NUMBER)]);
        // r"ab" → 4 chars (r + quotes + ab), classified STRING.
        assert_eq!(semantic_tokens("r\"ab\""), vec![st(0, 0, 5, STRING)]);
        // b"x" → 4 chars.
        assert_eq!(semantic_tokens("b\"x\""), vec![st(0, 0, 4, STRING)]);
    }

    #[test]
    fn semtok_comment_and_hash_in_string() {
        // `x # c` → VARIABLE over x, COMMENT over `# c` (3 chars).
        assert_eq!(
            semantic_tokens("x # c"),
            vec![st(0, 0, 1, VARIABLE), st(0, 2, 3, COMMENT)]
        );
        // A `#` inside a string is part of the STRING, never a comment.
        assert_eq!(semantic_tokens("\"#\""), vec![st(0, 0, 3, STRING)]);
    }

    #[test]
    fn semtok_delta_encode() {
        // "fn\nx" → keyword then variable on the next line.
        let toks = semantic_tokens("fn\nx");
        assert_eq!(toks, vec![st(0, 0, 2, KEYWORD), st(1, 0, 1, VARIABLE)]);
        // delta-encoded: [dLine, dStart, len, type, modifiers] per token.
        assert_eq!(
            encode_semantic_tokens(&toks),
            vec![0, 0, 2, KEYWORD, 0, 1, 0, 1, VARIABLE, 0]
        );
    }
}
