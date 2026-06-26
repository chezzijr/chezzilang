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
    /// 0-based start character, in UTF-16 code units (the LSP default position encoding).
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
    let col0_char = col1.saturating_sub(1);
    let end_char = word_end_col(source, line1, col1) as usize;
    // LSP positions default to UTF-16 code units; the lexer counts char columns. Convert both ends
    // so a non-BMP character earlier on the line doesn't shift the squiggle off the real token.
    let line = source.lines().nth(line1.saturating_sub(1)).unwrap_or("");
    let line_chars: Vec<char> = line.chars().collect();
    let to_utf16 = |upto: usize| -> u32 {
        line_chars
            .iter()
            .take(upto)
            .map(|c| c.len_utf16() as u32)
            .sum()
    };
    Diag {
        line: line0,
        col: to_utf16(col0_char),
        end_line: line0,
        end_col: to_utf16(end_char),
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
// Hover (the `K` / `textDocument/hover` path).
// ---------------------------------------------------------------------------

/// The result of a hover query: the checker-inferred type rendered for display, plus the symbol's
/// classification (local/param/fn/field/struct/literal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoverInfo {
    /// The inferred type, ready to drop into a ```` ```chezzi ```` code block (e.g. `int`,
    /// `fn(int) -> int`, `List[str]`).
    pub display: String,
    /// What kind of symbol the cursor landed on (secondary metadata; `display` is the payload).
    pub kind: crate::checker::HoverKind,
}

/// Hover at a **0-based** LSP position (UTF-16 code units, the encoding the server emits — see
/// `span_diag` / `push_utf16_tokens`). Reverses the UTF-16 → char-column conversion, finds the lexer
/// token under the cursor, then runs the SAME resolve → desugar → check pipeline as [`diagnostics`]
/// over the live `source` and returns the type the checker inferred for the smallest expression /
/// binding / field-name at that token. `None` when the cursor is off any symbol, the program fails to
/// resolve/check, or the position carries no type (operators, keywords, desugared `?.`/`??`).
pub fn hover(path: &Path, source: &str, line: u32, character: u32) -> Option<HoverInfo> {
    use crate::{checker, resolver};
    // Reverse the UTF-16 column to a char column on the cursor's line.
    let line_str = source.lines().nth(line as usize)?;
    let char_col = utf16_to_char_col(line_str, character);
    // The lexer token covering that char position → its 1-based (line, col) probe key.
    let (l1, c1) = token_at(source, line as usize, char_col)?;
    let graph = resolver::build_graph_with_entry_source(path, Some(source.to_string())).ok()?;
    let (display, kind) = checker::hover_type(&graph, l1, c1)?;
    Some(HoverInfo { display, kind })
}

/// Reverse of the `to_utf16` mapping in `span_diag`: given a line and a 0-based UTF-16 column, return
/// the 0-based **char** column. Walks chars summing `len_utf16` until reaching the target; a column
/// that lands mid-surrogate (between an astral char's two code units) clamps to that char's start.
fn utf16_to_char_col(line: &str, utf16_col: u32) -> usize {
    let mut acc: u32 = 0;
    for (i, c) in line.chars().enumerate() {
        if acc >= utf16_col {
            return i;
        }
        // If the target column falls strictly inside this char's surrogate pair, clamp to its
        // start rather than overshooting to the next char.
        let next = acc + c.len_utf16() as u32;
        if next > utf16_col {
            return i;
        }
        acc = next;
    }
    line.chars().count()
}

/// Find the lexer token whose char-extent contains the 0-based `(line0, char_col)` cursor and return
/// its **1-based** `(line, col)` start (the key the checker's hover probe matches). Each token's char
/// length reuses the exact per-kind logic from [`semantic_tokens`] (Ident → `chars().count()`,
/// numbers → [`measure_number`], strings → [`measure_string`], else the lexeme length); layout tokens
/// (Newline/Indent/Dedent/Eof) and a lex error yield `None`.
fn token_at(source: &str, line0: usize, char_col: usize) -> Option<(usize, usize)> {
    use crate::lexer::{self, Token};
    let chars: Vec<char> = source.chars().collect();
    let line_starts = line_start_offsets(&chars);
    let toks = lexer::tokenize(source).ok()?;
    let target_line = line0 + 1; // tokens use 1-based lines
    for tok in &toks {
        if tok.span.line != target_line {
            continue;
        }
        let start_abs =
            line_starts.get(tok.span.line - 1).copied().unwrap_or(0) + (tok.span.col - 1);
        let char_len = match &tok.kind {
            Token::Ident(name) => name.chars().count(),
            Token::Int(_) | Token::Float(_) => measure_number(&chars, start_abs),
            Token::Str(_) | Token::RawStr(_) | Token::Bytes(_) => measure_string(&chars, start_abs),
            other => match other.lexeme() {
                Some(lex) => lex.chars().count(),
                None => continue, // layout token — no extent
            },
        };
        let start_col0 = tok.span.col - 1;
        if char_col >= start_col0 && char_col < start_col0 + char_len {
            return Some((tok.span.line, tok.span.col));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Semantic tokens (the neovim highlight path).
// ---------------------------------------------------------------------------

/// The semantic-token legend, in legend order. The `u32` token-type of a [`SemTok`] indexes this
/// slice; the LSP server advertises exactly these names.
pub const SEMANTIC_TOKEN_TYPES: &[&str] = &[
    "keyword",
    "operator",
    "string",
    "number",
    "comment",
    "variable",
    // Role-based extras (Deliverable B): the AST overlay refines an Ident from `variable` to one of
    // these. Appended (not inserted) so the existing indices 0..=5 never shift.
    "function",
    "type",
    "property",
    "parameter",
];

pub const KEYWORD: u32 = 0;
pub const OPERATOR: u32 = 1;
pub const STRING: u32 = 2;
pub const NUMBER: u32 = 3;
pub const COMMENT: u32 = 4;
pub const VARIABLE: u32 = 5;
pub const FUNCTION: u32 = 6;
pub const TYPE: u32 = 7;
pub const PROPERTY: u32 = 8;
pub const PARAMETER: u32 = 9;

/// One classified token with an absolute **0-based** position. `start` and `len` are in UTF-16 code
/// units (the LSP default position encoding); every token stays within a single line.
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
    // AST-derived role overlay (Deliverable B): refines an Ident from the default `variable` to
    // function / type / property / parameter. Built once; EMPTY (→ all idents stay `variable`) when
    // the buffer doesn't parse, so a mid-edit document never errors or loses its base highlighting.
    let overlay = semantic_overlay(source);

    for tok in &toks {
        let line = tok.span.line;
        let col = tok.span.col;
        // Absolute char offset of the token's first char.
        let start_abs = line_starts.get(line - 1).copied().unwrap_or(0) + (col - 1);
        let (ttype, char_len) = match &tok.kind {
            // The token's 1-based source `(line, col)` keys the overlay (AST spans are 1-based char
            // positions too); default to `variable` when no role applies.
            Token::Ident(name) => (
                overlay.get(&(line, col)).copied().unwrap_or(VARIABLE),
                name.chars().count(),
            ),
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
        // Split the token's char range across lines (LSP semantic tokens are single-line — a
        // multi-line/triple-quoted string must NOT be one over-long token) and emit each segment's
        // start/length in UTF-16 code units (the LSP default position encoding).
        push_utf16_tokens(
            &mut out,
            &chars,
            &line_starts,
            start_abs,
            start_abs + char_len,
            ttype,
        );
    }

    append_comments(&chars, &string_extents, &mut out);
    out.sort_by_key(|t| (t.line, t.start));
    out
}

// ---------------------------------------------------------------------------
// AST-derived semantic-token overlay (Deliverable B).
// ---------------------------------------------------------------------------

/// Map from a token's **1-based** `(line, col)` source position to the semantic-token role that the
/// AST assigns it (one of `FUNCTION` / `TYPE` / `PROPERTY` / `PARAMETER`). Built by lexing + parsing
/// the SINGLE buffer (no resolver / no disk — robust to missing imports and fast) and walking the
/// decls/exprs/types. Spans synthesized by other phases (`Span::default()`, line 0) are never
/// inserted, and a buffer that fails to lex/parse yields an EMPTY map so `semantic_tokens` degrades
/// gracefully to lexer-only highlighting (every ident `variable`) — it never errors on a mid-edit
/// document. The keys match the 1-based `(span.line, span.col)` that `semantic_tokens` reads off the
/// lexer stream, so an emitted Ident is refined by a direct lookup.
fn semantic_overlay(source: &str) -> std::collections::HashMap<(usize, usize), u32> {
    use crate::{lexer, parser};
    let mut map = std::collections::HashMap::new();
    let Ok(toks) = lexer::tokenize(source) else {
        return map;
    };
    let Ok(module) = parser::parse(toks) else {
        return map;
    };
    for stmt in &module.stmts {
        overlay_stmt(stmt, &mut map);
    }
    map
}

/// Insert `role` at `span`'s 1-based `(line, col)`, skipping the synthesized-span sentinel (line 0)
/// so a checker/desugar-built node never colors a real token. First write wins (the call-callee
/// `function` mark is applied before the generic field-access `property` rule for the same name).
fn overlay_mark(
    map: &mut std::collections::HashMap<(usize, usize), u32>,
    span: crate::ast::Span,
    role: u32,
) {
    if span.line == 0 {
        return;
    }
    map.entry((span.line, span.col)).or_insert(role);
}

/// Walk a type annotation, marking every `Type::Named` reference `TYPE` (covers struct/enum names in
/// type position + generic bounds). A `Qualified` type has no span (Deliverable B scope) — only its
/// args are walked.
fn overlay_type(ty: &crate::ast::Type, map: &mut std::collections::HashMap<(usize, usize), u32>) {
    use crate::ast::Type;
    match ty {
        Type::Named { span, .. } => overlay_mark(map, *span, TYPE),
        Type::Qualified { args, .. } | Type::Generic(_, args) => {
            for a in args {
                overlay_type(a, map);
            }
        }
        Type::Func { params, ret } => {
            for p in params {
                overlay_type(p, map);
            }
            overlay_type(ret, map);
        }
        Type::Tuple(elems) => {
            for e in elems {
                overlay_type(e, map);
            }
        }
    }
}

/// Mark a function/method's signature: the name (`FUNCTION`), each param name (`PARAMETER`) and its
/// type, the generic-bound args, the return type, and recurse the body.
fn overlay_fndecl(
    decl: &crate::ast::FnDecl,
    map: &mut std::collections::HashMap<(usize, usize), u32>,
) {
    overlay_mark(map, decl.name_span, FUNCTION);
    for tp in &decl.type_params {
        for b in &tp.bounds {
            for a in &b.args {
                overlay_type(a, map);
            }
        }
    }
    overlay_params(&decl.params, map);
    if let Some(ret) = &decl.ret {
        overlay_type(ret, map);
    }
    overlay_block(&decl.body, map);
}

/// Mark each parameter's name (`PARAMETER`), its annotation, and its default expression.
fn overlay_params(
    params: &[crate::ast::Param],
    map: &mut std::collections::HashMap<(usize, usize), u32>,
) {
    for p in params {
        overlay_mark(map, p.name_span, PARAMETER);
        if let Some(ty) = &p.ty {
            overlay_type(ty, map);
        }
        if let Some(d) = &p.default {
            overlay_expr(d, map);
        }
    }
}

fn overlay_block(
    block: &crate::ast::Block,
    map: &mut std::collections::HashMap<(usize, usize), u32>,
) {
    for s in block {
        overlay_stmt(s, map);
    }
}

fn overlay_stmt(stmt: &crate::ast::Stmt, map: &mut std::collections::HashMap<(usize, usize), u32>) {
    use crate::ast::{DeferTarget, SpawnTarget, StmtKind, WaitTarget};
    match &stmt.kind {
        StmtKind::Let { ty, value, .. } => {
            if let Some(t) = ty {
                overlay_type(t, map);
            }
            overlay_expr(value, map);
        }
        StmtKind::Assign { target, value, .. } => {
            overlay_expr(target, map);
            overlay_expr(value, map);
        }
        StmtKind::Fn(decl) => overlay_fndecl(decl, map),
        StmtKind::Struct {
            fields, methods, ..
        } => {
            for f in fields {
                overlay_mark(map, f.name_span, PROPERTY);
                overlay_type(&f.ty, map);
                if let Some(d) = &f.default {
                    overlay_expr(d, map);
                }
            }
            for m in methods {
                overlay_fndecl(m, map);
            }
        }
        StmtKind::Protocol { methods, .. } => {
            // A `MethodSig` carries no name span (Deliverable B scope), so only its params/return
            // type are roled.
            for sig in methods {
                overlay_params(&sig.params, map);
                if let Some(ret) = &sig.ret {
                    overlay_type(ret, map);
                }
            }
        }
        StmtKind::Enum {
            variants, methods, ..
        } => {
            for v in variants {
                for t in &v.payload {
                    overlay_type(t, map);
                }
            }
            for m in methods {
                overlay_fndecl(m, map);
            }
        }
        StmtKind::TypeAlias { ty, .. } => overlay_type(ty, map),
        StmtKind::NewType {
            underlying,
            methods,
            ..
        } => {
            overlay_type(underlying, map);
            for m in methods {
                overlay_fndecl(m, map);
            }
        }
        StmtKind::If {
            branches,
            else_block,
        } => {
            for (cond, body) in branches {
                overlay_expr(cond, map);
                overlay_block(body, map);
            }
            if let Some(b) = else_block {
                overlay_block(b, map);
            }
        }
        StmtKind::For { iter, body, .. } => {
            overlay_expr(iter, map);
            overlay_block(body, map);
        }
        StmtKind::While { cond, body } => {
            overlay_expr(cond, map);
            overlay_block(body, map);
        }
        StmtKind::Match { scrutinee, arms } => {
            overlay_expr(scrutinee, map);
            for a in arms {
                if let Some(g) = &a.guard {
                    overlay_expr(g, map);
                }
                overlay_block(&a.body, map);
            }
        }
        StmtKind::Return(Some(e)) | StmtKind::Yield(e) | StmtKind::Expr(e) => overlay_expr(e, map),
        StmtKind::Return(None) | StmtKind::Break | StmtKind::Continue | StmtKind::Import(_) => {}
        StmtKind::Defer(DeferTarget::Call(e)) => overlay_expr(e, map),
        StmtKind::Defer(DeferTarget::Block(b)) => overlay_block(b, map),
        StmtKind::Parallel { body } => overlay_block(body, map),
        StmtKind::Spawn(SpawnTarget::Call(e)) => overlay_expr(e, map),
        StmtKind::Spawn(SpawnTarget::Block(b)) => overlay_block(b, map),
        StmtKind::Wait { arms, else_block } => {
            for a in arms {
                if let WaitTarget::Assign(e) = &a.target {
                    overlay_expr(e, map);
                }
                overlay_expr(&a.chan, map);
                overlay_block(&a.body, map);
            }
            if let Some(b) = else_block {
                overlay_block(b, map);
            }
        }
        StmtKind::Extern { fns, .. } => {
            for f in fns {
                overlay_params(&f.params, map);
                if let Some(ret) = &f.ret {
                    overlay_type(ret, map);
                }
            }
        }
        StmtKind::Assert { cond, msg } => {
            overlay_expr(cond, map);
            if let Some(m) = msg {
                overlay_expr(m, map);
            }
        }
    }
}

fn overlay_expr(expr: &crate::ast::Expr, map: &mut std::collections::HashMap<(usize, usize), u32>) {
    use crate::ast::ExprKind;
    match &expr.kind {
        // A CALL callee is a function reference, not a value/property: an `Ident` callee colors the
        // name `function`; a `Field` callee (`obj.method(...)`) colors the METHOD name `function`
        // (handled BEFORE the generic field-access `property` rule, so it never mis-colors). Other
        // callee shapes (an index, a parenthesized expr) recurse normally.
        ExprKind::Call {
            callee,
            args,
            named,
            type_args,
        } => {
            match &callee.kind {
                ExprKind::Ident(_) => overlay_mark(map, callee.span, FUNCTION),
                ExprKind::Field { obj, name_span, .. } => {
                    overlay_mark(map, *name_span, FUNCTION);
                    overlay_expr(obj, map);
                }
                _ => overlay_expr(callee, map),
            }
            for a in args {
                overlay_expr(a, map);
            }
            for (_, v) in named {
                overlay_expr(v, map);
            }
            for t in type_args {
                overlay_type(t, map);
            }
        }
        // A non-call field access colors the field NAME `property`.
        ExprKind::Field { obj, name_span, .. } => {
            overlay_mark(map, *name_span, PROPERTY);
            overlay_expr(obj, map);
        }
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Bytes(_)
        | ExprKind::RawStr(_)
        | ExprKind::Bool(_)
        | ExprKind::Ident(_)
        | ExprKind::TypeApply { .. } => {}
        ExprKind::List(es) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
            for e in es {
                overlay_expr(e, map);
            }
        }
        ExprKind::Map(pairs) => {
            for (k, v) in pairs {
                overlay_expr(k, map);
                overlay_expr(v, map);
            }
        }
        ExprKind::Comprehension {
            key, elem, clauses, ..
        } => {
            if let Some(k) = key {
                overlay_expr(k, map);
            }
            overlay_expr(elem, map);
            for c in clauses {
                overlay_expr(&c.iter, map);
                for g in &c.guards {
                    overlay_expr(g, map);
                }
            }
        }
        ExprKind::Unary { expr, .. } => overlay_expr(expr, map),
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::NullCoalesce { lhs, rhs } => {
            overlay_expr(lhs, map);
            overlay_expr(rhs, map);
        }
        ExprKind::Range { start, end } => {
            overlay_expr(start, map);
            overlay_expr(end, map);
        }
        ExprKind::Index { obj, index } => {
            overlay_expr(obj, map);
            overlay_expr(index, map);
        }
        ExprKind::Slice {
            obj,
            start,
            end,
            step,
        } => {
            overlay_expr(obj, map);
            for c in [start, end, step].into_iter().flatten() {
                overlay_expr(c, map);
            }
        }
        ExprKind::Try(e) => overlay_expr(e, map),
        ExprKind::OptChain { obj, call, .. } => {
            overlay_expr(obj, map);
            if let Some(c) = call {
                for a in &c.args {
                    overlay_expr(a, map);
                }
                for (_, v) in &c.named {
                    overlay_expr(v, map);
                }
            }
        }
        ExprKind::DecodeCall { obj, ty, arg } => {
            overlay_expr(obj, map);
            overlay_type(ty, map);
            overlay_expr(arg, map);
        }
        ExprKind::Closure { params, ret, body } => {
            overlay_params(params, map);
            if let Some(r) = ret {
                overlay_type(r, map);
            }
            overlay_expr(body, map);
        }
        ExprKind::Match { scrutinee, arms } => {
            overlay_expr(scrutinee, map);
            for a in arms {
                if let Some(g) = &a.guard {
                    overlay_expr(g, map);
                }
                overlay_expr(&a.body, map);
            }
        }
        ExprKind::IfElse { cond, then, els } => {
            overlay_expr(cond, map);
            overlay_expr(then, map);
            overlay_expr(els, map);
        }
        ExprKind::Recover(block) => overlay_block(block, map),
    }
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

/// Emit LSP semantic tokens for the absolute char range `[start_abs, end_abs)` of `chars`, all of
/// type `ttype`. The range is split at newlines (LSP tokens may not cross a line boundary — so a
/// triple-quoted multi-line string yields one token per line) and every column/length is measured
/// in **UTF-16 code units**, the LSP default position encoding, so non-BMP characters (emoji, astral
/// plane) keep highlights aligned with the editor's view of the line.
fn push_utf16_tokens(
    out: &mut Vec<SemTok>,
    chars: &[char],
    line_starts: &[usize],
    start_abs: usize,
    end_abs: usize,
    ttype: u32,
) {
    if end_abs <= start_abs {
        return;
    }
    // The line `start_abs` falls on, and its UTF-16 column within that line.
    let mut line = match line_starts.binary_search(&start_abs) {
        Ok(i) => i,
        Err(i) => i - 1,
    };
    let mut seg_start: u32 = chars[line_starts[line]..start_abs]
        .iter()
        .map(|c| c.len_utf16() as u32)
        .sum();
    let mut seg_len: u32 = 0;
    for &c in &chars[start_abs..end_abs] {
        if c == '\n' {
            if seg_len > 0 {
                out.push(SemTok {
                    line: line as u32,
                    start: seg_start,
                    len: seg_len,
                    token_type: ttype,
                });
            }
            line += 1;
            seg_start = 0;
            seg_len = 0;
        } else {
            seg_len += c.len_utf16() as u32;
        }
    }
    if seg_len > 0 {
        out.push(SemTok {
            line: line as u32,
            start: seg_start,
            len: seg_len,
            token_type: ttype,
        });
    }
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
            let mut len = 0u32;
            while j < n && chars[j] != '\n' {
                len += chars[j].len_utf16() as u32;
                j += 1;
            }
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
        col += c.len_utf16() as u32;
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

    fn hov(src: &str, line: u32, ch: u32) -> Option<HoverInfo> {
        hover(
            Path::new("/nonexistent/chezzi_editor/buf.chz"),
            src,
            line,
            ch,
        )
    }

    #[test]
    fn hover_local_type() {
        // `x := 1` — hovering the binding `x` reports its inferred type `int`.
        let h = hov("x := 1\n", 0, 0).expect("hover on local");
        assert_eq!(h.display, "int");
    }

    #[test]
    fn hover_fn_param_type() {
        // The use of param `a` inside the body reports the declared param type `str`.
        let h = hov("fn f(a: str):\n    a\n", 1, 4).expect("hover on param use");
        assert_eq!(h.display, "str");
    }

    #[test]
    fn hover_fn_name() {
        // A bare use of a function name reports its function type.
        let h = hov("fn f(a: int) -> int:\n    return a\nf\n", 2, 0).expect("hover on fn name");
        assert_eq!(h.display, "fn(int) -> int");
    }

    #[test]
    fn hover_literal() {
        // An integer literal reports `int`.
        let h = hov("y := 123\n", 0, 5).expect("hover on literal");
        assert_eq!(h.display, "int");
    }

    #[test]
    fn hover_none_on_operator() {
        // Hovering the `+` operator (a non-leaf, non-token position) yields no type.
        assert_eq!(hov("a := 1 + 2\n", 0, 7), None);
    }

    #[test]
    fn hover_no_check_returns_none() {
        // The program does not type-check (`zzz` undefined) → hover returns None.
        assert_eq!(hov("x := zzz\n", 0, 0), None);
    }

    #[test]
    fn hover_field_access() {
        // Hovering the field name `x` in `p.x` reports the field's type (`int`) and `Field` kind.
        // The Field node anchors recording on the field-name span, not the receiver's start span.
        let src = "struct P:\n    x: int\np := P(1)\np.x\n";
        let h = hov(src, 3, 2).expect("hover on field name");
        assert_eq!(h.display, "int");
        assert_eq!(h.kind, crate::checker::HoverKind::Field);
    }

    #[test]
    fn hover_utf16_emoji() {
        // 🙂 is one char but TWO UTF-16 units. The cursor on `n` arrives as a UTF-16 column; the
        // reverse conversion must land on `n` (str), not slip a column because of the astral char.
        // line 1: `out := "🙂" + n` — `n` is at char col 13, UTF-16 col 14 (🙂 adds one unit).
        let src = "n := \"a\"\nout := \"\u{1f642}\" + n\n";
        let h = hov(src, 1, 14).expect("hover on n after emoji");
        assert_eq!(h.display, "str");
    }

    #[test]
    fn utf16_to_char_col_clamps_mid_surrogate() {
        // "a🙂": 'a' is 1 UTF-16 unit, 🙂 is 2 (cols 1 and 2 both fall within the emoji).
        assert_eq!(utf16_to_char_col("a\u{1f642}", 0), 0); // 'a'
        assert_eq!(utf16_to_char_col("a\u{1f642}", 1), 1); // boundary: start of 🙂
        assert_eq!(utf16_to_char_col("a\u{1f642}", 2), 1); // MID-surrogate → clamp to 🙂's start
        assert_eq!(utf16_to_char_col("a\u{1f642}", 3), 2); // past end → char count
    }

    #[test]
    fn hover_free_fn_callee() {
        // Hovering the CALLEE `inc` of a call `inc(3)` reports the function's signature.
        let src = "fn inc(x: int) -> int:\n    return x + 1\ninc(3)\n";
        let h = hov(src, 2, 0).expect("hover on free-fn callee");
        assert_eq!(h.display, "fn(int) -> int");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_struct_ctor_callee() {
        // Hovering the ctor callee `Vec2` of `Vec2(1, 2)` reports a fn from fields to the struct.
        let src = "struct Vec2:\n    x: int\n    y: int\nVec2(1, 2)\n";
        let h = hov(src, 3, 0).expect("hover on struct-ctor callee");
        assert_eq!(h.display, "fn(int, int) -> Vec2");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_generic_fn_callee() {
        // A generic fn callee reports its DECLARED signature with the type parameters intact.
        let src = "fn combine[T: Arithmetic](a: T, b: T) -> T:\n    return a + b\ncombine(1, 2)\n";
        let h = hov(src, 2, 0).expect("hover on generic-fn callee");
        assert_eq!(h.display, "fn(T, T) -> T");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_builtin_callee_none() {
        // A builtin callee (`print`) carries no recordable signature → hover stays None.
        assert_eq!(hov("print(\"hi\")\n", 0, 0), None);
    }

    #[test]
    fn hover_method_callee() {
        // Hovering a method name in `c.foo(2)` reports the method's CALL signature (receiver stripped).
        let src = "struct C:\n    n: int\n    fn foo(self, n: int) -> int:\n        return n\nc := C(1)\nc.foo(2)\n";
        let h = hov(src, 5, 2).expect("hover on method callee");
        assert_eq!(h.display, "fn(int) -> int");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    /// The `token_type` of the semantic token starting at 0-based `(line, start)`, or `None`.
    fn role_at(toks: &[SemTok], line: u32, start: u32) -> Option<u32> {
        toks.iter()
            .find(|t| t.line == line && t.start == start)
            .map(|t| t.token_type)
    }

    #[test]
    fn overlay_roles() {
        // One buffer exercising every AST-derived role. Columns are 0-based UTF-16 (all ASCII here).
        let src = "fn f(p: int) -> int:\n    return p\nstruct S:\n    fld: int\nm := S(1)\na := m.fld\nb := f(2)\nc := 5\n";
        let toks = semantic_tokens(src);
        // fn-decl name → function; param decl → parameter; type annotations (param + return) → type.
        assert_eq!(role_at(&toks, 0, 3), Some(FUNCTION), "fn-decl name");
        assert_eq!(role_at(&toks, 0, 5), Some(PARAMETER), "param decl");
        assert_eq!(role_at(&toks, 0, 8), Some(TYPE), "param type annotation");
        assert_eq!(role_at(&toks, 0, 16), Some(TYPE), "return type annotation");
        // struct field decl → property; its type annotation → type.
        assert_eq!(role_at(&toks, 3, 4), Some(PROPERTY), "struct field decl");
        assert_eq!(role_at(&toks, 3, 9), Some(TYPE), "field type annotation");
        // ctor / fn-call callee → function; field access → property.
        assert_eq!(role_at(&toks, 4, 5), Some(FUNCTION), "ctor callee");
        assert_eq!(role_at(&toks, 5, 7), Some(PROPERTY), "field access");
        assert_eq!(role_at(&toks, 6, 5), Some(FUNCTION), "fn-call callee");
        // A plain local binding + a param USE stay variable (only DECL sites are roled).
        assert_eq!(role_at(&toks, 7, 0), Some(VARIABLE), "plain local");
        assert_eq!(role_at(&toks, 1, 11), Some(VARIABLE), "param use");
    }

    #[test]
    fn overlay_graceful_on_parse_error() {
        // A mid-edit unparseable buffer: the overlay is empty and semantic_tokens degrades to
        // lexer-only (every ident VARIABLE) WITHOUT panicking and still emits tokens.
        let toks = semantic_tokens("fn f( := = bad\n");
        assert!(
            !toks.is_empty(),
            "tokens must still be emitted on a parse error"
        );
        for t in &toks {
            assert_ne!(
                t.token_type, FUNCTION,
                "no overlay role should leak from an unparseable buffer"
            );
        }
        // The identifiers `f` and `bad` are present and classified VARIABLE (no role override).
        assert_eq!(
            role_at(&toks, 0, 3),
            Some(VARIABLE),
            "ident f stays variable"
        );
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

    #[test]
    fn semtok_multiline_string_splits_per_line() {
        // A triple-quoted string spanning lines must NOT emit one token whose length runs past
        // end-of-line (LSP tokens are single-line). It splits into one STRING token per line.
        let toks = semantic_tokens("\"\"\"\nab\n\"\"\"\n");
        let strs: Vec<SemTok> = toks
            .into_iter()
            .filter(|t| t.token_type == STRING)
            .collect();
        assert_eq!(
            strs,
            vec![
                st(0, 0, 3, STRING), // opening """
                st(1, 0, 2, STRING), // ab
                st(2, 0, 3, STRING), // closing """
            ]
        );
    }

    #[test]
    fn semtok_utf16_astral_columns() {
        // 🙂 is one Unicode scalar but TWO UTF-16 code units. LSP positions default to UTF-16, so
        // the `+` and `x` after the string must land at UTF-16 columns 4 and 5, not char columns 3/4.
        let toks = semantic_tokens("\"🙂\"+x");
        assert_eq!(
            toks,
            vec![
                st(0, 0, 4, STRING),   // "🙂" = 1 + 2 + 1 UTF-16 units
                st(0, 4, 1, OPERATOR), // +
                st(0, 5, 1, VARIABLE), // x
            ]
        );
    }

    #[test]
    fn diag_utf16_astral_column() {
        // An emoji before the error token shifts every later column by one UTF-16 unit.
        let ds = diag("a := \"🙂\" + zzz\n");
        assert!(!ds.is_empty(), "undefined name should produce a diagnostic");
        let d = &ds[0];
        assert!(d.message.contains("zzz"));
        // `zzz` starts at char col 11 (0-based); UTF-16 col is 12 (🙂 adds one extra unit).
        assert_eq!(d.col, 12);
        assert_eq!(d.end_col, 15); // 12 + 3 chars of "zzz"
    }
}
