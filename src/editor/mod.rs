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

/// Re-exported so an LSP server maps severities without depending on `checker` directly, and so
/// there is exactly ONE severity type in the tree.
pub use crate::checker::Severity;

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
    /// Error (squiggle, blocks the build) vs warning (advisory).
    pub severity: Severity,
    /// The module this diagnostic belongs to, resolved from `Span::file`. `None` means the entry
    /// buffer the caller passed in — a diagnostic with no module coordinate, or one from the entry
    /// module itself. A consumer publishing to a URI MUST honour this: everything in a module graph
    /// is reported through the entry's check, but only some of it belongs to the entry's buffer.
    pub file: Option<std::path::PathBuf>,
}

/// Type-check `source` as the entry module at `path` (imports resolve from disk) and return one
/// [`Diag`] per diagnostic — errors AND non-fatal warnings, told apart by [`Diag::severity`]. A
/// clean program yields an empty vector.
///
/// This mirrors `chezzi check` exactly (resolve → desugar → check_graph) but feeds the **live**
/// buffer in for the entry module, so diagnostics reflect unsaved edits while cross-module imports
/// still resolve against the on-disk project.
pub fn diagnostics(path: &Path, source: &str) -> Vec<Diag> {
    // Run resolve+desugar+check on the dedicated front-end stack: this is the ~2 MiB LSP tokio-worker
    // path, which a deep-but-valid AST would overflow (SIGABRT the language server) — see
    // `crate::on_frontend_stack`.
    let path = path.to_path_buf();
    let source = source.to_string();
    crate::on_frontend_stack(move || diagnostics_inner(&path, &source))
}

fn diagnostics_inner(path: &Path, source: &str) -> Vec<Diag> {
    use crate::{checker, resolver};
    match resolver::build_graph_with_entry_source(path, Some(source.to_string())) {
        // A resolve error wraps the fatal lex/parse error (or a missing/cyclic import) — always fatal.
        // `e.path` self-attributes the failing module (resolver::ResolveError doc): when it names some
        // OTHER file than the entry `path` we were given, the failure is inside an IMPORTED module, and
        // must be reported against THAT module's text, not the live entry buffer's.
        //
        // F1 fix (was a raw `p.as_path() != path` — a Critical regression): `e.path` for the ENTRY's own
        // failure is `id.0 = canonical_or_abs(entry_abs)`, which resolves symlinks whenever the file
        // exists on disk; `path` (from the LSP's `uri.to_file_path()`) never touches disk. On any project
        // reached through a symlinked path component the two differ even though it IS the entry, so raw
        // equality wrongly took the imported-module arm for the entry's own error — reading a stale
        // on-disk copy instead of the live buffer, and tagging `file: Some(canonical_path)`, which
        // `chezzi-lsp::publish` maps to a URI nothing is listening on. Normalize BOTH sides through the
        // same `canonical_or_abs` before comparing, so the comparison is exact rather than heuristic.
        // This arm produces exactly one diagnostic per call (and the imported-module branch already does
        // a `std::fs::read_to_string`), so the extra `stat` here is the same order of cost, not a new one.
        // Do NOT canonicalize elsewhere — `graph_diag` below still matches by the resolver's own numeric
        // `Span::file` id, never by path, so it stays exempt from this filesystem hit.
        Err(e) => match &e.path {
            Some(p) if resolver::canonical_or_abs(p) != resolver::canonical_or_abs(path) => {
                let mut cache = std::collections::HashMap::new();
                vec![module_text_diag(
                    &mut cache,
                    p,
                    e.span.line as usize,
                    e.span.col as usize,
                    e.message,
                    Severity::Error,
                )]
            }
            _ => vec![span_diag(
                source,
                e.span.line as usize,
                e.span.col as usize,
                e.message,
                Severity::Error,
            )],
        },
        // M24 — the manifest-entrypoint gate is a property of the PROJECT, so the editor reports it
        // like `chezzi check` does: one derivation (`manifest::entry_fn_for`), every consumer.
        Ok(graph) => {
            let (res, warns) =
                checker::check_graph_diags(&graph, crate::manifest::entry_fn_for(path).as_deref());
            let errs = res.err().unwrap_or_default();
            // Distinct-module text, read from disk at most ONCE each (the resolver drops source after
            // parsing, so there's nothing cached upstream to reuse) — a module with several diagnostics
            // must not re-read its file per diagnostic.
            let mut cache: std::collections::HashMap<std::path::PathBuf, Option<String>> =
                std::collections::HashMap::new();
            errs.into_iter()
                .chain(warns)
                .map(|e| {
                    // TICKET-007: a near-miss `help` suggestion has no field of its own in `Diag`, so
                    // it rides `message` — the LSP's only text surface. `graph_diag` sees just the
                    // combined string.
                    let message = match &e.help {
                        Some(h) => format!("{}\nhelp: {h}", e.message),
                        None => e.message,
                    };
                    graph_diag(&graph, source, &mut cache, e.span, message, e.severity)
                })
                .collect()
        }
    }
}

/// Map one checker diagnostic to a [`Diag`], resolving `span.file` through the [`resolver::ModuleGraph`]
/// (SCANNING `graph.modules` — never indexing by id, see the module doc) so the range is computed
/// against the OWNING module's text. The entry module (`m.id == graph.entry`), or a `span.file` that
/// doesn't resolve in the graph at all (id `0`, or a synthesized diagnostic), uses the live `source`
/// and `file: None` exactly as before. Any other module reads its text from disk via [`module_text_diag`].
fn graph_diag(
    graph: &crate::resolver::ModuleGraph,
    source: &str,
    cache: &mut std::collections::HashMap<std::path::PathBuf, Option<String>>,
    span: crate::lexer::Span,
    message: String,
    severity: Severity,
) -> Diag {
    match graph.modules.iter().find(|m| m.file == span.file) {
        Some(m) if m.id != graph.entry => module_text_diag(
            cache,
            &m.id.0,
            span.line as usize,
            span.col as usize,
            message,
            severity,
        ),
        _ => span_diag(
            source,
            span.line as usize,
            span.col as usize,
            message,
            severity,
        ),
    }
}

/// A diagnostic attributed to a module OTHER than the live entry buffer: reads `path`'s text off disk
/// once (through `cache`, keyed on the path — a module with several diagnostics is read only once) and
/// computes the range against it, tagging [`Diag::file`] with `path`. A read failure still reports the
/// diagnostic — never dropped, never silently re-attributed to the entry — as a `col..col+1` range,
/// since there is no text to measure a word boundary or convert to UTF-16 against.
fn module_text_diag(
    cache: &mut std::collections::HashMap<std::path::PathBuf, Option<String>>,
    path: &Path,
    line1: usize,
    col1: usize,
    message: String,
    severity: Severity,
) -> Diag {
    let text = cache
        .entry(path.to_path_buf())
        .or_insert_with(|| std::fs::read_to_string(path).ok());
    let mut d = match text {
        Some(t) => span_diag(t, line1, col1, message, severity),
        None => {
            let line0 = line1.saturating_sub(1) as u32;
            let col0 = col1.saturating_sub(1) as u32;
            Diag {
                line: line0,
                col: col0,
                end_line: line0,
                end_col: col0 + 1,
                message,
                severity,
                file: None,
            }
        }
    };
    d.file = Some(path.to_path_buf());
    d
}

/// 1-based `(line, col)` → 0-based [`Diag`], extending the end over an identifier word at the
/// position (so an undefined-name squiggle covers the whole name) or by one char otherwise.
fn span_diag(source: &str, line1: usize, col1: usize, message: String, severity: Severity) -> Diag {
    let source = crate::lexer::strip_bom(source);
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
        severity,
        file: None,
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The 0-based end column of the word starting at 1-based `(line, col)`, or `col` (0-based) + 1 when
/// the position is not on an identifier char. `pub`: also the `--errors=json` `end_col` renderer
/// (`main.rs::diags_json`) reuses this rather than writing a second word-boundary scanner — its
/// `line`/`col` are 1-based, so it adds 1 to this fn's 0-based result.
pub fn word_end_col(source: &str, line1: usize, col1: usize) -> u32 {
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
    /// The symbol's doc-comment: the contiguous run of `#` comment lines immediately above its
    /// declaration (blank line detaches; stacked lines join with `\n`, one leading `# ` stripped).
    /// `None` when the declaration has no adjacent comment. Rendered ABOVE the type code-fence.
    pub doc: Option<String>,
}

/// Hover at a **0-based** LSP position (UTF-16 code units, the encoding the server emits — see
/// `span_diag` / `push_utf16_tokens`). Reverses the UTF-16 → char-column conversion, finds the lexer
/// token under the cursor, then runs the SAME resolve → desugar → check pipeline as [`diagnostics`]
/// over the live `source` and returns the type the checker inferred for the smallest expression /
/// binding / field-name at that token. `None` when the cursor is off any symbol, the program fails to
/// resolve/check, or the position carries no type (operators, keywords, desugared `?.`/`??`).
pub fn hover(path: &Path, source: &str, line: u32, character: u32) -> Option<HoverInfo> {
    let source = crate::lexer::strip_bom(source);
    // Reverse the UTF-16 column to a char column on the cursor's line.
    let line_str = source.lines().nth(line as usize)?;
    let char_col = utf16_to_char_col(line_str, character);
    // The lexer token covering that char position → its 1-based (line, col) probe key.
    let (l1, c1) = token_at(source, line as usize, char_col)?;
    // Same dedicated front-end stack as `diagnostics`, and for the same reason: this runs resolve →
    // desugar → check, on the ~2 MiB `chezzi-lsp` tokio worker (`#[tokio::main]`, `rt-multi-thread`,
    // no `stack_size`). It was the ONE checker entry point still on the caller's stack, so a
    // deep-but-valid AST — or any deep checker recursion — would SIGABRT the language server here
    // while the identical program hovered fine through `diagnostics`.
    let path = path.to_path_buf();
    let source = source.to_string();
    crate::on_frontend_stack(move || hover_inner(&path, &source, l1, c1))
}

fn hover_inner(path: &Path, source: &str, l1: u32, c1: u32) -> Option<HoverInfo> {
    use crate::{checker, resolver};
    let graph = resolver::build_graph_with_entry_source(path, Some(source.to_string())).ok()?;
    let (display, kind, doc) = checker::hover_type(&graph, l1, c1)?;
    Some(HoverInfo { display, kind, doc })
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
fn token_at(source: &str, line0: usize, char_col: usize) -> Option<(u32, u32)> {
    use crate::lexer::{self, Token};
    let chars: Vec<char> = source.chars().collect();
    let line_starts = line_start_offsets(&chars);
    let toks = lexer::tokenize(source).ok()?;
    let target_line = (line0 + 1) as u32; // tokens use 1-based lines
    for tok in &toks {
        if tok.span.line != target_line {
            continue;
        }
        // `as usize`: `Span` is `u32`-wide (see `Span`'s doc) and u32 → usize never truncates.
        let (tl, tc) = (tok.span.line as usize, tok.span.col as usize);
        let start_abs = line_starts.get(tl - 1).copied().unwrap_or(0) + (tc - 1);
        let char_len = match &tok.kind {
            Token::Ident(name) => name.chars().count(),
            Token::Int(_) | Token::IntMinMagnitude | Token::Float(_) => {
                measure_number(&chars, start_abs)
            }
            Token::Str(_) | Token::RawStr(_) | Token::Bytes(_) => measure_string(&chars, start_abs),
            other => match other.lexeme() {
                Some(lex) => lex.chars().count(),
                None => continue, // layout token — no extent
            },
        };
        let start_col0 = tc - 1;
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
///
/// Runs on the dedicated front-end stack: unlike [`diagnostics`]/[`hover`] this never touches the
/// resolver/checker, but it still lexes + parses the buffer (`semantic_overlay`, below) and then
/// walks the resulting AST with `overlay_expr`'s per-node recursion — the same class of
/// deep-but-valid-AST overflow those two guard against, on the same ~2 MiB LSP tokio worker
/// (`textDocument/semanticTokens/full`). The `Module` is built, walked and **dropped** entirely
/// inside `semantic_overlay` (recursive `Drop` is the same stack trap as a recursive walk), so
/// wrapping the overlay call alone would also be sound; the hop sits at THIS outer public function
/// because that is one spawn per request and it matches [`diagnostics`]/[`hover`], which is the
/// convention a future entry point should copy. See `crate::on_frontend_stack`.
pub fn semantic_tokens(source: &str) -> Vec<SemTok> {
    let source = source.to_string();
    crate::on_frontend_stack(move || semantic_tokens_inner(&source))
}

fn semantic_tokens_inner(source: &str) -> Vec<SemTok> {
    use crate::lexer::{self, Token};
    let source = crate::lexer::strip_bom(source);
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
        // `as usize`: `Span` is `u32`-wide (see `Span`'s doc); u32 → usize never truncates.
        let line = tok.span.line as usize;
        let col = tok.span.col as usize;
        // Absolute char offset of the token's first char.
        let start_abs = line_starts.get(line - 1).copied().unwrap_or(0) + (col - 1);
        let (ttype, char_len) = match &tok.kind {
            // The token's 1-based source `(line, col)` keys the overlay (AST spans are 1-based char
            // positions too); default to `variable` when no role applies.
            Token::Ident(name) => (
                overlay.get(&(line, col)).copied().unwrap_or(VARIABLE),
                name.chars().count(),
            ),
            Token::Int(_) | Token::IntMinMagnitude | Token::Float(_) => {
                (NUMBER, measure_number(&chars, start_abs))
            }
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
    map.entry((span.line as usize, span.col as usize))
        .or_insert(role);
}

/// Walk a type annotation, marking every `Type::Named` reference `TYPE` (covers struct/enum names in
/// type position + generic bounds). A `Qualified` type has no span (Deliverable B scope) — only its
/// args are walked.
fn overlay_type(ty: &crate::ast::Type, map: &mut std::collections::HashMap<(usize, usize), u32>) {
    use crate::ast::Type;
    match ty {
        Type::Named { span, .. } => overlay_mark(map, *span, TYPE),
        Type::Qualified { args, .. } | Type::Generic(_, args, ..) => {
            for a in args {
                overlay_type(a, map);
            }
        }
        Type::Func { params, ret, .. } => {
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
    use crate::ast::{DeferTarget, SpawnTarget, StmtKind, WaitArmKind, WaitTarget};
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
        StmtKind::Return(None)
        | StmtKind::Break
        | StmtKind::Continue
        | StmtKind::Pass
        | StmtKind::Import(_) => {}
        StmtKind::Defer(DeferTarget::Call(e)) => overlay_expr(e, map),
        StmtKind::Defer(DeferTarget::Block(b)) => overlay_block(b, map),
        StmtKind::Parallel { body } => overlay_block(body, map),
        StmtKind::Spawn(SpawnTarget::Call(e)) => overlay_expr(e, map),
        StmtKind::Spawn(SpawnTarget::Block(b)) => overlay_block(b, map),
        StmtKind::Wait { arms, else_block } => {
            for a in arms {
                match &a.kind {
                    WaitArmKind::Recv { target, chan } => {
                        if let WaitTarget::Assign(e) = target {
                            overlay_expr(e, map);
                        }
                        overlay_expr(chan, map);
                    }
                    WaitArmKind::Send { call } => overlay_expr(call, map),
                }
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
        StmtKind::Native(decl) => {
            // A `native fn`/`native ctor` decl carries param/return type annotations to role (like an
            // extern fn). It's prelude/std-only, so this rarely fires on a user buffer, but the overlay
            // is a total function over `StmtKind`.
            overlay_params(&decl.params, map);
            if let Some(ret) = &decl.ret {
                overlay_type(ret, map);
            }
        }
        StmtKind::NativeStruct { fields, .. } => {
            // A `native struct` decl carries body-less field type annotations (prelude/std-only, so this
            // rarely fires on a user buffer). Role the field names as properties + their types, like a
            // real struct's fields; the overlay is a total function over `StmtKind`.
            for f in fields {
                overlay_mark(map, f.name_span, PROPERTY);
                overlay_type(&f.ty, map);
            }
        }
        StmtKind::NativeEnum {
            variants, methods, ..
        } => {
            // A `native enum` decl (Option/Result shape mirror, prelude/std-only) carries body-less
            // variant payload types + body-less `native fn` method sigs. Role them like a real enum's
            // variants + methods; the overlay is a total function over `StmtKind`.
            for v in variants {
                for t in &v.payload {
                    overlay_type(t, map);
                }
            }
            for m in methods {
                overlay_params(&m.params, map);
                if let Some(ret) = &m.ret {
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
        | ExprKind::Pass
        | ExprKind::Ident(_)
        | ExprKind::TypeApply { .. } => {}
        // Interpolation fragments are ordinary expressions — color them like any other child.
        ExprKind::Interp(chunks) => {
            for c in chunks {
                if let crate::ast::Chunk::Expr(e, _) = c {
                    overlay_expr(e, map);
                }
            }
        }
        ExprKind::List(es, _) | ExprKind::Tuple(es) | ExprKind::Set(es) => {
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
        ExprKind::Binary { lhs, rhs, .. } | ExprKind::NullCoalesce { lhs, rhs, .. } => {
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

    /// TICKET-007 criterion 14: a near-miss `help` suggestion rides the LSP's `message` field.
    #[test]
    fn diag_help_appended_to_message() {
        let ds = diag("xs := [1, 2, 3]\nxs.lenght()\n");
        assert!(!ds.is_empty(), "method typo should produce a diagnostic");
        let d = &ds[0];
        assert!(d.message.contains("has no method 'lenght'"), "got: {d:?}");
        assert!(
            d.message.contains("help: did you mean 'len'?"),
            "got: {d:?}"
        );
    }

    /// W8-17 — the LSP squiggle covers the member NAME, not the receiver. `lenght` is a 6-char
    /// token starting at 0-based col 3 (1-based col 10, JSON `end_col` 16 minus one at each end).
    #[test]
    fn diag_member_miss_squiggles_the_member_name() {
        let ds = diag("xs := [1, 2, 3]\nxs.lenght()\n");
        assert!(!ds.is_empty(), "method typo should produce a diagnostic");
        let d = &ds[0];
        assert_eq!(d.line, 1);
        assert_eq!(d.col, 3);
        assert_eq!(d.end_col, 9);
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
    fn hover_through_a_leading_bom_finds_the_number_token() {
        let h = hov("\u{feff}x := 12345\n", 0, 7).expect("hover on int through a leading BOM");
        assert_eq!(h.display, "int");
    }

    #[test]
    fn semantic_tokens_ignore_a_leading_bom() {
        let src = "x := 12345\ns := \"ab\" # c\n";
        assert_eq!(
            semantic_tokens(&format!("\u{feff}{src}")),
            semantic_tokens(src)
        );
    }

    #[test]
    fn diag_columns_are_counted_from_the_first_visible_char_after_a_bom() {
        let ds = diag("\u{feff}print(nope)\n");
        assert_eq!((ds[0].line, ds[0].col, ds[0].end_col), (0, 6, 10));
    }

    #[test]
    fn hover_local_type() {
        // `x := 1` — hovering the binding `x` reports its inferred type `int`.
        let h = hov("x := 1\n", 0, 0).expect("hover on local");
        assert_eq!(h.display, "int");
    }

    #[test]
    fn hover_for_binding_decl() {
        // `for i in [1,2,3]:` — hovering the binding `i` at the decl site (right after `for `)
        // reports its inferred element type `int`.
        let h = hov("for i in [1,2,3]:\n    print(i)\n", 0, 4).expect("hover on for binding");
        assert_eq!(h.display, "int");
    }

    #[test]
    fn hover_for_binding_body() {
        // The body use-site of the loop var still resolves (additive decl-site span must not regress).
        let h = hov("for i in [1,2,3]:\n    print(i)\n", 1, 10).expect("hover on for body use");
        assert_eq!(h.display, "int");
    }

    #[test]
    fn hover_destructure_first() {
        // `a, b := (1, 2)` — hovering the first binding `a` (col 0) reports its tuple-element type.
        let h = hov("a, b := (1, 2)\nprint(a)\n", 0, 0).expect("hover on destructure first");
        assert_eq!(h.display, "int");
    }

    #[test]
    fn hover_destructure_second() {
        // The second binding `b` (col 3) reports its own tuple-element type.
        let h = hov("a, b := (1, 2)\nprint(a)\n", 0, 3).expect("hover on destructure second");
        assert_eq!(h.display, "int");
    }

    #[test]
    fn hover_single_let_regression() {
        // The single-name let path must NOT regress when destructure plumbing is added.
        let h = hov("x := 1\n", 0, 0).expect("hover on single let");
        assert_eq!(h.display, "int");
    }

    #[test]
    fn hover_refined_empty_decl_shows_final_type() {
        // PART B: hovering the `b := []` DECL site, after a later `b.push(0)` refines b to List[int],
        // shows the FINAL refined type List[int], not the provisional List[Unknown].
        let src = "fn main():\n b := []\n b.push(0)\n print(b)\nmain()\n";
        let h = hov(src, 1, 1).expect("hover on empty-list decl");
        assert_eq!(h.display, "List[int]");
    }

    #[test]
    fn hover_refined_empty_pre_use_shows_final_type() {
        // PART B: hovering a USE of b that occurs BEFORE the refining op also shows the final
        // List[int] (not the provisional List[Unknown] recorded at probe time).
        let src = "fn main():\n b := []\n print(b)\n b.push(0)\nmain()\n";
        let h = hov(src, 2, 7).expect("hover on pre-refine use of b");
        assert_eq!(h.display, "List[int]");
    }

    #[test]
    fn hover_refined_empty_decl_intervening_fn_shows_final_type() {
        // PART B / correctness-0 regression: a fn decl BETWEEN the module-level `b := []` and its
        // refining `b.push(0)` must NOT let the inner fn's check_fn_body finalize seam lock the hover
        // to the still-unrefined List[Unknown]. Only the module seam (which owns b) resolves it — so
        // the decl-site hover shows the FINAL List[int]. Was "List[?]" before the owning-scope gate.
        let src = "b := []\nfn foo():\n print(\"x\")\nb.push(0)\nprint(b)\n";
        let h = hov(src, 0, 0).expect("hover on module-level empty-list decl");
        assert_eq!(h.display, "List[int]");
    }

    #[test]
    fn hover_match_variant_bind() {
        // The `n` binding inside a variant pattern `Col.Val(n)` reports the payload type.
        let src = "enum Col:\n    Val(int)\n\nc := Col.Val(3)\nmatch c:\n    Col.Val(n):\n        print(n)\n";
        let h = hov(src, 5, 12).expect("hover on match variant bind");
        assert_eq!(h.display, "int");
    }

    #[test]
    fn hover_match_tuple_bind() {
        // The `a` binding inside a tuple pattern `(a, b)` reports its element type.
        let src = "p := (1, 2)\nmatch p:\n    (a, b):\n        print(a)\n";
        let h = hov(src, 2, 5).expect("hover on match tuple bind");
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
    fn hover_type_alias_transparent() {
        // Hovering the type token `Id` in `x: Id` shows the RESOLVED type (`int`, the alias body).
        let h =
            hov("type Id = int\nx: Id = 5\nprint(x)\n", 1, 3).expect("hover on alias type token");
        assert_eq!(h.display, "int");
    }

    #[test]
    fn hover_param_type_token() {
        // The `int` param-type token (line0 col8) hovers to `int`.
        let h = hov("fn f(a: int) -> int:\n    return a\nprint(f(1))\n", 0, 8)
            .expect("hover on param type token");
        assert_eq!(h.display, "int");
    }

    #[test]
    fn hover_return_type_token() {
        // The return-type `int` token (line0 col16) hovers to `int`.
        let h = hov("fn f(a: int) -> int:\n    return a\nprint(f(1))\n", 0, 16)
            .expect("hover on return type token");
        assert_eq!(h.display, "int");
    }

    #[test]
    fn hover_field_type_token() {
        // The struct field-type `int` token (line1 col7) hovers to `int`.
        let h = hov("struct S:\n    x: int\ns := S(1)\nprint(s.x)\n", 1, 7)
            .expect("hover on field type token");
        assert_eq!(h.display, "int");
    }

    #[test]
    fn hover_struct_name_type_token() {
        // The `P` param-type token (line2 col8) hovers to the struct `P`.
        let h = hov(
            "struct P:\n    v: int\nfn g(p: P):\n    print(p.v)\ng(P(1))\n",
            2,
            8,
        )
        .expect("hover on struct-name type token");
        assert_eq!(h.display, "P");
    }

    #[test]
    fn hover_generic_inner_type_token() {
        // The inner `int` of `List[int]` (line0 col9) hovers via composite recursion.
        let h = hov("xs: List[int] = [1]\nprint(xs)\n", 0, 9)
            .expect("hover on generic inner type token");
        assert_eq!(h.display, "int");
    }

    #[test]
    fn hover_generic_fn_param_type_no_latch() {
        // The param-type `T` (line0 col11) must show `T` (Ty::Param), not `?` (prepass-latch guard).
        let h = hov("fn f[T](x: T) -> T:\n    return x\nprint(f(1))\n", 0, 11)
            .expect("hover on generic fn param type token");
        assert_eq!(h.display, "T");
    }

    #[test]
    fn hover_native_type_import_shows_doc() {
        // A per-name import of a native/reserved TYPE (`import Shared from std.concurrency`) must
        // show its builtin blurb on the IMPORT-LINE token — not "No information available". `Shared`
        // is at line0 col7. (The annotation use `Shared[int]` already worked via builtin_type_doc.)
        let src = "import Shared from std.concurrency\nfn main():\n    s: Shared[int] = Shared(0)\n    print(s.get())\n";
        let h = hov(src, 0, 7).expect("hover on native-type import token");
        assert!(h.display.contains("Shared"), "display: {:?}", h.display);
        assert!(
            h.doc
                .as_deref()
                .is_some_and(|d| d.contains("cross-task shared cell")),
            "import-line hover should carry the Shared blurb, got: {:?}",
            h.doc
        );
    }

    #[test]
    fn hover_timer_import_shows_func_doc() {
        // `timer` is a reserved FUNCTION (`timer(ms) -> Channel[bool]`); its import-line token
        // (`import timer from std.time`, `timer` at line0 col7) shows a function-style hover.
        let src = "import timer from std.time\nfn main():\n    t := timer(10)\n    print(1)\n";
        let h = hov(src, 0, 7).expect("hover on timer import token");
        assert!(
            h.doc
                .as_deref()
                .is_some_and(|d| d.contains("one-shot timeout channel")),
            "timer import should carry its blurb, got: {:?}",
            h.doc
        );
    }

    #[test]
    fn hover_generic_param_shadowing_decl_no_doc_leak() {
        // A type param that SHADOWS a documented same-named top-level decl resolves to Ty::Param —
        // an unrelated entity — so its annotation-token hover must NOT borrow the decl's docstring
        // (the name_docs fallback is keyed by bare name). `v: Item` is at line3 col20; the param
        // `Item` shadows the documented `struct Item`.
        let src = "# an item\nstruct Item:\n    x: int\nfn process[Item](v: Item) -> Item:\n    return v\nprint(1)\n";
        let h = hov(src, 3, 20).expect("hover on shadowing param type token");
        assert_eq!(h.display, "Item");
        assert_eq!(
            h.doc, None,
            "type param must not show the shadowed struct's doc"
        );
    }

    #[test]
    fn hover_type_kind_is_type() {
        // A type-annotation hover is classified `HoverKind::Type`.
        let h = hov("x: int = 5\nprint(x)\n", 0, 3).expect("hover on let annotation type token");
        assert_eq!(h.kind, crate::checker::HoverKind::Type);
    }

    #[test]
    fn hover_fn_shows_doc() {
        // A `#` comment immediately above a `fn` becomes its doc, surfaced on hover of the fn's use.
        let h = hov("# greet the world\nfn f():\n    1\nf\n", 3, 0).expect("hover on fn use");
        assert_eq!(h.display, "fn() -> nil");
        assert_eq!(h.doc.as_deref(), Some("greet the world"));
    }

    #[test]
    fn hover_doc_multiline_joins() {
        // Stacked `#` lines (no gap) join with a newline.
        let h = hov("# line one\n# line two\nfn f():\n    1\nf\n", 4, 0).expect("hover on fn use");
        assert_eq!(h.doc.as_deref(), Some("line one\nline two"));
    }

    #[test]
    fn hover_doc_blank_line_detaches() {
        // A blank line between two comment blocks detaches the earlier one.
        let h =
            hov("# detached\n\n# attached\nfn f():\n    1\nf\n", 5, 0).expect("hover on fn use");
        assert_eq!(h.doc.as_deref(), Some("attached"));
    }

    #[test]
    fn hover_doc_on_struct() {
        // Doc on a struct surfaces when hovering the struct constructor.
        let h =
            hov("# a 2D point\nstruct P:\n    x: int\nP(1)\n", 3, 0).expect("hover on struct ctor");
        assert_eq!(h.doc.as_deref(), Some("a 2D point"));
    }

    #[test]
    fn hover_doc_on_method() {
        // Doc on a method surfaces when hovering the method call.
        let src = "struct C:\n    # double it\n    fn dbl(self) -> int:\n        return 2\nc := C()\nc.dbl()\n";
        let h = hov(src, 5, 2).expect("hover on method call");
        assert_eq!(h.doc.as_deref(), Some("double it"));
    }

    #[test]
    fn hover_doc_on_top_level_let() {
        // Doc on a top-level let surfaces on the binding.
        let h = hov("# the answer\nanswer := 42\n", 1, 0).expect("hover on let binding");
        assert_eq!(h.display, "int");
        assert_eq!(h.doc.as_deref(), Some("the answer"));
    }

    #[test]
    fn hover_inline_trailing_comment_not_doc() {
        // An inline trailing comment on the decl line is not picked up as a doc.
        let h = hov("fn f(): 1  # not a doc\nf\n", 1, 0).expect("hover on fn use");
        assert_eq!(h.doc, None);
    }

    #[test]
    fn hover_local_shadowing_documented_global_has_no_doc() {
        // A param/local that shadows a documented top-level name must NOT borrow the global's doc
        // (`name_docs` is keyed by bare name). Repro from the adversarial review.
        let src = "# the seed value\nseed := 42\nfn f(seed: int) -> int:\n    return seed\n";
        // The param USE inside the body resolves to the local `seed` (scope 1) → no doc.
        let use_h = hov(src, 3, 11).expect("hover on param use");
        assert_eq!(use_h.kind, crate::checker::HoverKind::Local);
        assert_eq!(use_h.doc, None, "local use must not show the global's doc");
        // Guard: the genuine top-level `seed` binding still shows its doc.
        let global_h = hov(src, 1, 0).expect("hover on top-level let");
        assert_eq!(global_h.doc.as_deref(), Some("the seed value"));
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
    fn hover_builtin_callee_print() {
        // `print` is now the file-backed variadic decl `native fn print(...args: Any, sep, end)`, so
        // hover reflects that harvested signature: the collapsed variadic slot `List[Any]` plus the
        // keyword-only `sep`/`end` (both `str`), returning `nil`. (More informative than the old
        // synthetic `fn(?) -> nil`.)
        let h = hov("print(\"hi\")\n", 0, 0).expect("hover on builtin print callee");
        assert_eq!(h.display, "fn(List[Any], str, str) -> nil");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_builtin_callee_range() {
        // `range` is overload-collapsed to the canonical `range(end) -> List[int]` display.
        let h = hov("range(10)\n", 0, 0).expect("hover on builtin range callee");
        assert_eq!(h.display, "fn(int) -> List[int]");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_builtin_method_str() {
        // CASE 1: a builtin `str` method reports the inference-source signature (receiver stripped).
        let src = "s := \"abc\".upper()\n";
        // `upper` starts at char col 11 (`s := "abc".` is 11 chars).
        let h = hov(src, 0, 11).expect("hover on str builtin method");
        assert_eq!(h.display, "fn() -> str");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_builtin_method_list_len() {
        // `len` is a METHOD (not a free fn): `xs.len()` reports `fn() -> int` via list_method_sig.
        let src = "xs := [1, 2]\nn := xs.len()\n";
        // line 1 `n := xs.` → `len` at char col 8.
        let h = hov(src, 1, 8).expect("hover on list len method");
        assert_eq!(h.display, "fn() -> int");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_stdlib_module_fn() {
        // CASE 2: a stdlib module fn (`math.sqrt`) reports its native FnSig.
        let src = "import std.math\nx := math.sqrt(2.0)\n";
        // line 1 `x := math.` → `sqrt` at char col 10.
        let h = hov(src, 1, 10).expect("hover on std.math.sqrt");
        assert_eq!(h.display, "fn(float) -> float");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_method_callee() {
        // Hovering a method name in `c.foo(2)` reports the method's CALL signature (receiver stripped).
        let src = "struct C:\n    n: int\n    fn foo(self, n: int) -> int:\n        return n\nc := C(1)\nc.foo(2)\n";
        let h = hov(src, 5, 2).expect("hover on method callee");
        assert_eq!(h.display, "fn(int) -> int");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_newtype_ctor_callee() {
        // Hovering the ctor callee `UserId` of `UserId(10)` reports a fn from the underlying to the
        // newtype (mirrors the struct-ctor path).
        let src = "newtype UserId = int\nUserId(10)\n";
        let h = hov(src, 1, 0).expect("hover on newtype-ctor callee");
        assert_eq!(h.display, "fn(int) -> UserId");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_enum_variant_callee() {
        // Hovering the variant-name `Val` of `Col.Val(3)` reports the variant's ctor signature.
        let src = "enum Col:\n    Red\n    Val(int)\nCol.Val(3)\n";
        let h = hov(src, 3, 4).expect("hover on enum-variant callee");
        assert_eq!(h.display, "fn(int) -> Col");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_enum_variant_decl_name() {
        // Hovering the variant-name `Val` AT ITS DECLARATION (line 2, col 4) reports the variant's
        // ctor signature, matching the use-site hover (`fn(int) -> Col`).
        let src = "enum Col:\n    Red\n    Val(int)\nCol.Val(3)\n";
        let h = hov(src, 2, 4).expect("hover on enum-variant decl name");
        assert_eq!(h.display, "fn(int) -> Col");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_generic_enum_variant_decl_name() {
        // LATCH/shape guard: a generic enum's variant decl preserves the enum's `Ty::Param` shape —
        // `Full(T)` Displays "fn(T) -> Box[T]" (T not `?`/Unknown). `Full` token starts at line 1 col 4.
        let src = "enum Box[T]:\n    Full(T)\n    Empty\nx := Box[int].Full(3)\n";
        let h = hov(src, 1, 4).expect("hover on generic enum-variant decl name");
        assert_eq!(h.display, "fn(T) -> Box[T]");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_nullary_enum_variant_decl_name() {
        // A no-payload variant decl `Red` (line 1, col 4) Displays "fn() -> Col" — the nullary
        // convention locked by hover_enum_variant_callee.
        let src = "enum Col:\n    Red\n    Val(int)\nCol.Val(3)\n";
        let h = hov(src, 1, 4).expect("hover on nullary enum-variant decl name");
        assert_eq!(h.display, "fn() -> Col");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_enum_variant_receiver() {
        // Hovering the receiver `Col` of `Col.Val(3)` reports the enum type.
        let src = "enum Col:\n    Red\n    Val(int)\nCol.Val(3)\n";
        let h = hov(src, 3, 0).expect("hover on enum-variant receiver");
        assert_eq!(h.display, "Col");
    }

    #[test]
    fn hover_static_method_callee() {
        // Hovering the static-method name `default` of `Foo.default()` reports its call signature.
        let src = "struct Foo:\n    x: int\n    fn default() -> Foo:\n        return Foo(0)\nFoo.default()\n";
        let h = hov(src, 4, 4).expect("hover on static-method callee");
        assert_eq!(h.display, "fn() -> Foo");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_static_method_receiver() {
        // Hovering the receiver `Foo` of `Foo.default()` reports the struct type.
        let src = "struct Foo:\n    x: int\n    fn default() -> Foo:\n        return Foo(0)\nFoo.default()\n";
        let h = hov(src, 4, 0).expect("hover on static-method receiver");
        assert_eq!(h.display, "Foo");
    }

    #[test]
    fn hover_builtin_callee_chr() {
        // CONFIRMING (already green): a bare-name builtin callee (`chr`) records its display sig via
        // callee_display_ty -> builtin_sig. `len(...)` is method-only (not a free fn / not reserved-
        // callable), so `len(x)` is an undefined-name error and is out of scope here.
        let h = hov("chr(65)\n", 0, 0).expect("hover on builtin chr callee");
        assert_eq!(h.display, "fn(int) -> str");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_fn_param_decl() {
        // Hovering the param `a` at its DECL site in the signature reports its declared type.
        let h = hov("fn f(a: str):\n    a\n", 0, 5).expect("hover on fn param decl");
        assert_eq!(h.display, "str");
        assert_eq!(h.kind, crate::checker::HoverKind::Param);
    }

    #[test]
    fn hover_method_param_decl() {
        // The same check_fn_body record also covers METHOD param decls.
        let src = "struct C:\n    n: int\n    fn foo(self, n: int) -> int:\n        return n\nc := C(1)\nc.foo(2)\n";
        let h = hov(src, 2, 17).expect("hover on method param decl");
        assert_eq!(h.display, "int");
        assert_eq!(h.kind, crate::checker::HoverKind::Param);
    }

    #[test]
    fn hover_closure_param_decl() {
        // Hovering a closure param `a` at its DECL site reports its annotated type.
        let h = hov("f := fn(a: int): a + 1\nprint(f(2))\n", 0, 8)
            .expect("hover on closure param decl");
        assert_eq!(h.display, "int");
        assert_eq!(h.kind, crate::checker::HoverKind::Param);
    }

    #[test]
    fn hover_unannotated_closure_param_decl_in_generic_arg() {
        // An UNANNOTATED closure param passed to a generic fn: its decl-site hover must report the
        // INFERRED type (`int`), not the `?` the generic-arg unification prepass forces. Regression
        // guard for the prepass first-hit-wins latch (the record is gated on `!generic_arg_prepass`).
        let src = "fn apply[T](x: T, g: fn(T) -> T) -> T:\n    return g(x)\nprint(apply(1, fn(a): a + 1))\n";
        // line 2: `print(apply(1, fn(a): a + 1))` — the closure param `a` is at char 18.
        let h = hov(src, 2, 18).expect("hover on unannotated closure param decl");
        assert_eq!(h.display, "int");
        assert_eq!(h.kind, crate::checker::HoverKind::Param);
    }

    #[test]
    fn hover_struct_field_decl() {
        // Hovering the field `x` at its DECL site reports its declared type.
        let h =
            hov("struct P:\n    x: int\np := P(1)\n", 1, 4).expect("hover on struct field decl");
        assert_eq!(h.display, "int");
        assert_eq!(h.kind, crate::checker::HoverKind::Field);
    }

    #[test]
    fn hover_container_ctor_turbofish_callee() {
        // CONFIRMING (already green): `List[int]()` callee is a bare Ident reaching builtin_sig.
        let h = hov("a := List[int]()\n", 0, 5).expect("hover on List[int] ctor callee");
        assert_eq!(h.display, "fn(?) -> List[?]");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_container_ctor_bare_callee() {
        // CONFIRMING (already green): bare `List()` ctor callee records the same display sig.
        // (`b.push(1)` constrains the otherwise-unannotated empty so PART A doesn't error — an error
        // would make `hover_type` short-circuit to None before the callee hit is returned.)
        let h = hov("b := List()\nb.push(1)\n", 0, 5).expect("hover on bare List ctor callee");
        assert_eq!(h.display, "fn(?) -> List[?]");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_map_ctor_turbofish_callee() {
        // CONFIRMING (already green): `Map[str, int]()` callee records its display sig.
        let h = hov("m := Map[str, int]()\n", 0, 5).expect("hover on Map ctor callee");
        assert_eq!(h.display, "fn(?) -> Map[?, ?]");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    // ===== builtin / stdlib doc-on-hover (Tier C) =====

    #[test]
    fn hover_builtin_type_list_shows_methods() {
        // Hovering the `List` ctor callee surfaces its usage blurb + the "methods:" line.
        let h = hov("xs := List[int]()\n", 0, 6).expect("hover on List ctor callee");
        let doc = h
            .doc
            .as_deref()
            .expect("List ctor should carry a Tier-C doc");
        assert!(doc.contains("methods:"), "missing methods line: {doc:?}");
        assert!(doc.contains("push"), "missing push method: {doc:?}");
    }

    #[test]
    fn hover_builtin_type_token_str_shows_doc() {
        // Hovering the `str` type token in an annotation surfaces its usage blurb + methods.
        let h = hov("s: str = \"x\"\n", 0, 3).expect("hover on str type token");
        let doc = h
            .doc
            .as_deref()
            .expect("str token should carry a Tier-C doc");
        assert!(doc.contains("methods:"), "missing methods line: {doc:?}");
        assert!(doc.contains("upper"), "missing upper method: {doc:?}");
    }

    #[test]
    fn hover_module_fn_sqrt_shows_doc() {
        // Hovering `math.sqrt` surfaces the authored stdlib-fn blurb.
        let h = hov("import std.math\nr := math.sqrt(2.0)\n", 1, 10).expect("hover on math.sqrt");
        let doc = h
            .doc
            .as_deref()
            .expect("math.sqrt should carry a Tier-C doc");
        assert!(
            doc.to_lowercase().contains("square root"),
            "expected 'square root' blurb, got: {doc:?}"
        );
    }

    #[test]
    fn hover_builtin_does_not_break_user_doc() {
        // The `.or_else(builtin_type_doc)` fallback must not shadow a user fn's own docstring:
        // a documented user fn still surfaces ITS doc at the call-callee hover (Tier A intact).
        let src = "# doubles n\nfn dbl(n: int) -> int:\n    return n * 2\ndbl(3)\n";
        let h = hov(src, 3, 0).expect("hover on user fn callee");
        assert_eq!(h.doc.as_deref(), Some("doubles n"));
    }

    #[test]
    fn hover_generic_annotation_head_shows_doc() {
        // Hovering the GENERIC head `List` in `xs: List[int]` (goes through Type::Generic) surfaces
        // the builtin usage+methods blurb (Tier-C), same as the non-generic `str` token does.
        let h = hov("xs: List[int] = []\n", 0, 4).expect("hover on List generic head");
        let doc = h
            .doc
            .as_deref()
            .expect("List head should carry a Tier-C doc");
        assert!(doc.contains("methods:"), "missing methods line: {doc:?}");
        assert!(doc.contains("push"), "missing push method: {doc:?}");
    }

    #[test]
    fn hover_imported_type_shows_doc() {
        // Hovering the imported user type `Heap` on the import line surfaces a doc (its own decl
        // docstring carried across the module boundary, else a kind+module fallback).
        let h = hov("import Heap from std.collections\n", 0, 7).expect("hover on imported Heap");
        assert!(
            h.doc.is_some(),
            "imported type should carry a doc, got: {:?}",
            h.doc
        );
    }

    #[test]
    fn hover_imported_generic_head_shows_doc() {
        // Case (a)+(b) together: a later GENERIC use of the imported type (`Heap[int]` head) surfaces
        // the imported doc carried into name_docs by the import-line binding.
        let src = "import Heap from std.collections\nfn f(h: Heap[int]):\n    print(h.len())\n";
        let h = hov(src, 1, 8).expect("hover on Heap generic head in param annotation");
        assert!(
            h.doc.is_some(),
            "imported generic head should carry a doc, got: {:?}",
            h.doc
        );
    }

    // ===== decl-site NAME-token hovers (Tier A) =====

    #[test]
    fn hover_struct_decl_name() {
        // Hovering the declared name `P` at `struct P:` reports the struct type.
        let h = hov("struct P:\n    x: int\n", 0, 7).expect("hover on struct decl name");
        assert_eq!(h.display, "P");
        assert_eq!(h.kind, crate::checker::HoverKind::Struct);
    }

    #[test]
    fn hover_struct_decl_name_shows_doc() {
        // A `#` doc immediately above the struct surfaces on its decl-name hover.
        let h = hov("# a point\nstruct P:\n    x: int\n", 1, 7)
            .expect("hover on documented struct decl name");
        assert_eq!(h.display, "P");
        assert_eq!(h.doc.as_deref(), Some("a point"));
    }

    #[test]
    fn hover_enum_decl_name() {
        let h = hov("enum Col:\n    Val(int)\n", 0, 5).expect("hover on enum decl name");
        assert_eq!(h.display, "Col");
        assert_eq!(h.kind, crate::checker::HoverKind::Struct);
    }

    #[test]
    fn hover_newtype_decl_name() {
        let h = hov("newtype UserId = int\n", 0, 8).expect("hover on newtype decl name");
        assert_eq!(h.display, "UserId");
        assert_eq!(h.kind, crate::checker::HoverKind::Struct);
    }

    #[test]
    fn hover_type_alias_decl_name() {
        // The alias name reports the aliased type it stands for.
        let h = hov("type Id = int\n", 0, 5).expect("hover on type alias decl name");
        assert_eq!(h.display, "int");
        assert_eq!(h.kind, crate::checker::HoverKind::Struct);
    }

    #[test]
    fn hover_protocol_decl_name() {
        let h = hov("protocol Bar:\n    fn f(self)\n", 0, 9).expect("hover on protocol decl name");
        assert_eq!(h.display, "Bar");
        assert_eq!(h.kind, crate::checker::HoverKind::Struct);
    }

    #[test]
    fn hover_type_param_decl_fn() {
        // Hovering `T` at the fn DECLARATION `fn id[T](...)` reports the param name.
        let h =
            hov("fn id[T](x: T) -> T:\n    return x\n", 0, 6).expect("hover on fn type-param decl");
        assert_eq!(h.display, "T");
    }

    #[test]
    fn hover_type_param_decl_struct() {
        // The same enter_type_params record covers a struct's `[T]`.
        let h = hov("struct Box[T]:\n    x: T\n", 0, 11).expect("hover on struct type-param decl");
        assert_eq!(h.display, "T");
    }

    #[test]
    fn hover_assign_lhs() {
        // Hovering the LHS `i` of a reassignment reports its type.
        let h = hov("i := 0\ni = i + 1\n", 1, 0).expect("hover on assign lhs");
        assert_eq!(h.display, "int");
        assert_eq!(h.kind, crate::checker::HoverKind::Local);
    }

    #[test]
    fn hover_method_decl_name() {
        // Hovering the method name at its definition reports the call signature (receiver stripped),
        // matching the call-site method hover.
        let src = "struct C:\n    n: int\n    fn dbl(self) -> int:\n        return self.n * 2\n";
        let h = hov(src, 2, 7).expect("hover on method decl name");
        assert_eq!(h.display, "fn() -> int");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_free_fn_decl_name() {
        // Hovering the free-function name at its definition reports the function signature (no `self`
        // to strip for a free fn), matching the call-site hover. `foo` token starts at col 3.
        let h = hov("fn foo(bar: int) -> int:\n    return bar\n", 0, 3)
            .expect("hover on free fn decl name");
        assert_eq!(h.display, "fn(int) -> int");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_free_fn_decl_name_shows_doc() {
        // The fn-name token surfaces the decl's doc-comment (unlike a param, which has none).
        let h = hov(
            "# doubles it\nfn foo(bar: int) -> int:\n    return bar\n",
            1,
            3,
        )
        .expect("hover on free fn decl name with doc");
        assert_eq!(h.display, "fn(int) -> int");
        assert_eq!(h.doc.as_deref(), Some("doubles it"));
    }

    #[test]
    fn hover_generic_free_fn_decl_name() {
        // LATCH guard: the generic-arg prepass must NOT latch a `?`/Unknown at the fn name; the
        // generic sig Displays its type params. `pick` token starts at col 3.
        let h = hov("fn pick[T](a: T, b: T) -> T:\n    return a\n", 0, 3)
            .expect("hover on generic free fn decl name");
        assert_eq!(h.display, "fn(T, T) -> T");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    #[test]
    fn hover_import_module() {
        let h = hov("import std.math\n", 0, 11).expect("hover on import module name");
        assert_eq!(h.display, "module math");
    }

    #[test]
    fn hover_import_module_alias() {
        let h = hov("import std.math as m\n", 0, 19).expect("hover on import alias");
        assert_eq!(h.display, "module m");
    }

    #[test]
    fn hover_from_import_name() {
        let h = hov("import sqrt from std.math\n", 0, 7).expect("hover on from-import name");
        assert_eq!(h.display, "fn(float) -> float");
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

    /// Build the measured worst-parser-accepted-depth source: `R(0) = "a"`, `R(k) = "f(g(" +
    /// R(k-1) + ")" + ".f".repeat(498) + ")"`. `lv = 15` is the deepest level the parser still
    /// accepts; `lv = 16` trips the parser's own "expression nested too deeply" — this is a
    /// MEASURED value, not re-derived here.
    fn deep_but_legal_source(lv: usize) -> String {
        fn r(k: usize) -> String {
            if k == 0 {
                "a".to_string()
            } else {
                format!("f(g({}){})", r(k - 1), ".f".repeat(498))
            }
        }
        format!("x := {}\n", r(lv))
    }

    /// Regression for W7-50: `semantic_tokens` used to run its post-parse AST walk (`overlay_expr`,
    /// recursing once per node) on the CALLER's stack. `chezzi-lsp`'s `textDocument/semanticTokens/full`
    /// calls it from a `#[tokio::main]` worker with the default ~2 MiB stack — far smaller than the
    /// 8 MiB CLI main thread — so a deep-but-PARSER-ACCEPTED buffer (a left-leaning chain the parser's
    /// own recursive `MAX_DEPTH` guard never sees, only its `MAX_AST_DEPTH` fold-depth bound) would
    /// SIGABRT the language server. `semantic_tokens` now hops onto the dedicated 1 GiB front-end
    /// stack (`crate::on_frontend_stack`), same as `diagnostics`/`hover`.
    ///
    /// On regression this test ABORTS the whole test-process run rather than failing a single
    /// assertion — the same honesty `parser::tests::deep_nesting_errors_not_crash`'s doc-comment
    /// gives its own stack-overflow class: a red run here looks like a killed `cargo test` process,
    /// not a reported failure line.
    #[test]
    fn deep_but_legal_input_does_not_crash_semantic_tokens() {
        use crate::{lexer, parser};
        let src = deep_but_legal_source(15);
        // Fails loudly (not silently going shallow) if `MAX_DEPTH`/`MAX_AST_DEPTH` ever move.
        assert!(
            parser::parse(lexer::tokenize(&src).unwrap()).is_ok(),
            "fixture must still be parser-accepted at lv=15"
        );
        let handle = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024) // matches the tokio default the LSP worker actually gets
            .spawn(move || crate::editor::semantic_tokens(&src))
            .expect("spawn 2 MiB probe thread");
        let toks = handle
            .join()
            .expect("semantic_tokens must not crash the 2 MiB worker");
        assert!(!toks.is_empty());
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
    fn semtok_i64_min_magnitude_is_number() {
        // `-9223372036854775808`: the minus is an OPERATOR, the 2^63 magnitude (an IntMinMagnitude
        // token) still highlights as a NUMBER (19 digit chars), same as any int literal.
        assert_eq!(
            semantic_tokens("-9223372036854775808"),
            vec![st(0, 0, 1, OPERATOR), st(0, 1, 19, NUMBER)]
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

    /// W7-21 — a CALL through a module global that holds a fn value (`l.BARE()`) hovers the member
    /// name with its function type, exactly like a call on a declared module `fn`. The value form
    /// (`x := l.BARE`) always hovered; the call form did not exist until the member became callable,
    /// so it would have shipped as a silent hole. Needs real files on disk: `hover` resolves the
    /// module graph from the entry PATH.
    #[test]
    fn hover_module_fn_value_member_call() {
        let dir = std::env::temp_dir().join(format!("chezzi_hov_w721_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("k.chz"), "fn one() -> int:\n    return 1\n").unwrap();
        std::fs::write(dir.join("l.chz"), "import k\nBARE := k.one\n").unwrap();
        let src = "import l\nprint(l.BARE())\n";
        let entry = dir.join("main.chz");
        std::fs::write(&entry, src).unwrap();
        // Line 1 (0-based), the `BARE` token starts at char col 8: `print(l.BARE())`.
        let h = hover(&entry, src, 1, 8);
        let _ = std::fs::remove_dir_all(&dir);
        let h = h.expect("hover on a module fn-value member call");
        assert_eq!(h.display, "fn() -> int");
        assert_eq!(h.kind, crate::checker::HoverKind::Func);
    }

    /// A5 — a type error inside an IMPORTED module carries its own path (not the entry's), and its
    /// range is computed against the imported module's own text. The entry's `import core.badmod` line
    /// (19 chars) and badmod's own line 1 (16 chars) deliberately differ — and, at the error's column,
    /// badmod's char is a quote (not a word char, so `word_end_col` stops one past it) while the
    /// entry's char at that same column sits mid-identifier (`core`, so it would run on) — so computing
    /// the range against the WRONG source is detectable rather than coincidentally correct.
    #[test]
    fn cross_module_diagnostic_carries_its_own_file_and_range() {
        let dir =
            std::env::temp_dir().join(format!("chezzi_editor_a5_cross_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("core")).unwrap();
        let badmod_path = dir.join("core").join("badmod.chz");
        std::fs::write(&badmod_path, "y: int = \"oops\"\n").unwrap();
        let expected_file = std::fs::canonicalize(&badmod_path).unwrap();
        let entry = dir.join("app.chz");
        let src = "import core.badmod\n";
        let ds = diagnostics(&entry, src);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(ds.len(), 1, "expected exactly one diagnostic: {ds:?}");
        let d = &ds[0];
        assert_eq!(
            d.file.as_deref(),
            Some(expected_file.as_path()),
            "diagnostic must be attributed to the imported module, not the entry"
        );
        assert!(d.message.contains("core.badmod"), "{}", d.message);
        assert_eq!(d.line, 0);
        assert_eq!(d.col, 9);
        assert_eq!(
            d.end_col, 10,
            "range must come from badmod's OWN text (a quote char, not a word) — computed against \
             the entry's text at the same column it would wrongly run to the end of 'core' (11)"
        );
    }

    /// Negative control for the test above: a diagnostic that genuinely originates in the entry module
    /// itself must NOT claim a module path — `Diag::file` stays `None`.
    #[test]
    fn entry_module_diagnostic_has_no_file() {
        let ds = diag("a := 1\nb := zzz\n");
        assert!(!ds.is_empty(), "undefined name should produce a diagnostic");
        assert_eq!(
            ds[0].file, None,
            "an entry-module diagnostic must not claim a module path"
        );
    }

    /// A5 — a lex/parse (resolve) error INSIDE an imported module is also attributed to that module,
    /// not the entry buffer: the `Err(e)` arm of `diagnostics_inner` gets the same treatment as the
    /// `Ok(graph)` arm above.
    #[test]
    fn resolve_error_in_an_imported_module_is_attributed_to_it() {
        let dir = std::env::temp_dir().join(format!(
            "chezzi_editor_a5_resolve_err_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("core")).unwrap();
        let badmod_path = dir.join("core").join("badmod.chz");
        // Syntactically broken — a parse error, not a type error.
        std::fs::write(&badmod_path, "x := = 5\n").unwrap();
        let expected_file = std::fs::canonicalize(&badmod_path).unwrap();
        let entry = dir.join("app.chz");
        let src = "import core.badmod\n";
        let ds = diagnostics(&entry, src);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(ds.len(), 1, "expected exactly one diagnostic: {ds:?}");
        let d = &ds[0];
        assert_eq!(
            d.file.as_deref(),
            Some(expected_file.as_path()),
            "a parse error in an imported module must be attributed to it, not the entry"
        );
        assert_eq!(d.severity, Severity::Error);
        // Same fixture as `diag_parse_error_pos`: col 6 (1-based) → 5 (0-based), computed off badmod's
        // OWN line 1 ("x := = 5"), not the entry's ("import core.badmod").
        assert_eq!(d.line, 0);
        assert_eq!(d.col, 5);
    }

    /// F1 (Critical regression) — the `Err(e)` arm decided entry-vs-imported with RAW path equality
    /// (`p.as_path() != path`). `e.path` for the entry's OWN parse failure is `id.0 =
    /// canonical_or_abs(entry_abs)`, which resolves symlinks whenever the file exists; the caller's
    /// `path` (from `uri.to_file_path()`) never does. So on a project reached through a symlinked
    /// component the two differ even though it IS the entry, the guard wrongly takes the
    /// imported-module arm, and the entry's own error is (a) computed against a stale on-disk read
    /// instead of the live buffer, and (b) tagged `file: Some(canonical_path)`, which
    /// `chezzi-lsp::publish` maps to a DIFFERENT URI than the one the editor has open — the diagnostic
    /// is published where nothing listens and silently vanishes.
    ///
    /// Repro: a real symlinked directory, an entry file that EXISTS on disk with stale content, and a
    /// live (unsaved) buffer that differs from that stale content but has a parse error at the same
    /// structural position. `expected` is computed by running the SAME live source through an ordinary
    /// (non-symlinked, non-existent-on-disk) path — that's what a correct implementation must also
    /// produce for the symlinked entry: `file: None`, and a range computed from the LIVE buffer, not
    /// the stale disk text (the disk text is deliberately built so a wrong-source read is detectable —
    /// its char at the error column starts an 8-char word, so a buggy end_col overruns to 14 instead of
    /// the live buffer's 7).
    #[cfg(unix)]
    #[test]
    fn entry_own_error_survives_a_symlinked_project_path() {
        use std::os::unix::fs::symlink;

        let base =
            std::env::temp_dir().join(format!("chezzi_editor_a5_symlink_{}", std::process::id()));
        let real_dir = base.join("real");
        let link_dir = base.join("link");
        std::fs::create_dir_all(&real_dir).unwrap();

        // Stale on-disk content: deliberately NOT what the live buffer holds, and its char at the
        // error column (index 6) starts a long word — so misreading it produces a clearly wrong,
        // over-long end_col instead of a merely-off-by-one one.
        let stale_disk_src = "012345abcdefgh\n";
        let entry_real = real_dir.join("app.chz");
        std::fs::write(&entry_real, stale_disk_src).unwrap();

        symlink(&real_dir, &link_dir).unwrap();
        let entry_via_symlink = link_dir.join("app.chz");

        // Live (unsaved) buffer: same broken-`:=` shape as `diag_parse_error_pos`'s fixture
        // ("x := = 5"), with the identifier lengthened by one char so its error column (7, 1-based)
        // lands on the stale disk text's word-starting index.
        let live_src = "xx := = 5\n";

        // Control: the SAME live source at an ordinary path with nothing on disk — ground truth for
        // what the symlinked entry's diagnostic must also look like.
        let expected = diagnostics(
            Path::new("/nonexistent/chezzi_editor/control_app.chz"),
            live_src,
        );
        let actual = diagnostics(&entry_via_symlink, live_src);
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(
            expected.len(),
            1,
            "control fixture should produce exactly one diagnostic: {expected:?}"
        );
        assert_eq!(
            actual.len(),
            1,
            "expected exactly one diagnostic: {actual:?}"
        );
        let (e, a) = (&expected[0], &actual[0]);

        assert_eq!(
            a.file, None,
            "the entry's OWN parse error must be attributed to the live buffer (file: None), not \
             misrouted to the imported-module arm because canonicalize() resolved the symlink; got {a:?}"
        );
        assert_eq!(a.line, e.line);
        assert_eq!(
            a.col, e.col,
            "start column must be computed from the LIVE buffer, not the stale on-disk copy"
        );
        assert_eq!(
            a.end_col, e.end_col,
            "range must be computed from the LIVE buffer, not the stale on-disk copy"
        );
        assert_eq!(a.message, e.message);
    }
}
