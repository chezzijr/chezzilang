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

/// Lives in the lexer, the one owner of where a token ends; re-exported for the editor surface.
pub use crate::lexer::word_end_col;
use crate::lexer::{measure_number, measure_string};

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
    let chars: Vec<char> = crate::lexer::source_chars(source);
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
            Token::Str(s) => s.len,
            Token::RawStr(_) | Token::Bytes(_) => measure_string(&chars, start_abs),
            other @ Token::Fn
            | other @ Token::Return
            | other @ Token::If
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
            | other @ Token::Match
            | other @ Token::Recover
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
            | other @ Token::True
            | other @ Token::False
            | other @ Token::NoneKw
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
            | other @ Token::LParen
            | other @ Token::RParen
            | other @ Token::LBracket
            | other @ Token::RBracket
            | other @ Token::LBrace
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
            | other @ Token::Eof => match other.lexeme() {
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
    let chars: Vec<char> = crate::lexer::source_chars(source);
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

    for (i, tok) in toks.iter().enumerate() {
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
            // `from` is a keyword, but the parser takes it as a member or method name after `.`,
            // `?.` and `fn` (`Q.from(3)`, `fn from(x: int)`): the AST role if it has one, else a
            // method name there (a protocol sig has no name span), else the keyword.
            Token::From => {
                let after_member = i.checked_sub(1).is_some_and(|j| {
                    matches!(toks[j].kind, Token::Dot | Token::QuestionDot | Token::Fn)
                });
                let role = overlay.get(&(line, col)).copied();
                let fallback = if after_member { FUNCTION } else { KEYWORD };
                (role.unwrap_or(fallback), "from".len())
            }
            Token::Int(_) | Token::IntMinMagnitude | Token::Float(_) => {
                (NUMBER, measure_number(&chars, start_abs))
            }
            Token::Str(s) => {
                string_extents.push((start_abs, start_abs + s.len));
                (STRING, s.len)
            }
            Token::RawStr(_) | Token::Bytes(_) => {
                let l = measure_string(&chars, start_abs);
                string_extents.push((start_abs, start_abs + l));
                (STRING, l)
            }
            other @ Token::Fn
            | other @ Token::Return
            | other @ Token::If
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
            | other @ Token::Match
            | other @ Token::Recover
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
            | other @ Token::As
            | other @ Token::Const
            | other @ Token::And
            | other @ Token::Or
            | other @ Token::Not
            | other @ Token::True
            | other @ Token::False
            | other @ Token::NoneKw
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
            | other @ Token::LParen
            | other @ Token::RParen
            | other @ Token::LBracket
            | other @ Token::RBracket
            | other @ Token::LBrace
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
            | other @ Token::Eof => match other.lexeme() {
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
        Type::Named { span, .. } | Type::Nil(span) => overlay_mark(map, *span, TYPE),
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
    use crate::ast::{DeferTarget, SpawnTarget, StmtKind, WaitArmKind};
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
                if let Some(g) = &a.guard {
                    overlay_expr(g, map);
                }
                match &a.kind {
                    WaitArmKind::Recv { chan, .. } => overlay_expr(chan, map),
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
        StmtKind::NativeType { underlying, .. } => {
            if let Some(t) = underlying {
                overlay_type(t, map);
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
        // `bracket` spans the same text as `type_args`, which color it.
        ExprKind::Call {
            callee,
            args,
            named,
            type_args,
            bracket: _,
        } => {
            match &callee.kind {
                ExprKind::Ident(_) => overlay_mark(map, callee.span, FUNCTION),
                ExprKind::Field { obj, name_span, .. } => {
                    overlay_mark(map, *name_span, FUNCTION);
                    overlay_expr(obj, map);
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
                | ExprKind::Index { .. }
                | ExprKind::Try(_)
                | ExprKind::OptChain { .. }
                | ExprKind::NullCoalesce { .. }
                | ExprKind::Closure { .. }
                | ExprKind::Match { .. }
                | ExprKind::IfElse { .. }
                | ExprKind::Recover(..)
                | ExprKind::ElseGuard { .. } => overlay_expr(callee, map),
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
        | ExprKind::NoneLit
        | ExprKind::Pass
        | ExprKind::Ident(_) => {}
        // Interpolation fragments are ordinary expressions — color them like any other child.
        ExprKind::Interp(chunks) => {
            for c in chunks {
                if let crate::ast::Chunk::Expr(e, _, fields) = c {
                    overlay_expr(e, map);
                    for f in fields {
                        overlay_expr(f, map);
                    }
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
        ExprKind::Compare { operands, .. } => {
            for o in operands {
                overlay_expr(o, map);
            }
        }
        ExprKind::Range { start, end, .. } => {
            for bound in [start, end].into_iter().flatten() {
                overlay_expr(bound, map);
            }
        }
        ExprKind::Index { obj, index, .. } => {
            overlay_expr(obj, map);
            if let Some(index) = index {
                overlay_expr(index, map);
            }
        }
        ExprKind::Try(e) => overlay_expr(e, map),
        // `bracket` spans the same text as `type_args`, which color it (as `Call`).
        ExprKind::OptChain { obj, call, .. } => {
            overlay_expr(obj, map);
            if let Some(c) = call {
                for a in &c.args {
                    overlay_expr(a, map);
                }
                for (_, v) in &c.named {
                    overlay_expr(v, map);
                }
                for t in &c.type_args {
                    overlay_type(t, map);
                }
            }
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
        ExprKind::Recover(block, _) => overlay_block(block, map),
        ExprKind::ElseGuard { value, body, .. } => {
            overlay_expr(value, map);
            overlay_block(body, map);
        }
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

    // strings. A `b"…"` / `r"…"` literal has no hole, so its rules are verbatim. A plain literal
    // may hold `{expr}` holes, and a hole may nest a literal of the SAME quote (`"{d["k"]}"`): its
    // rules skip `{{` / `}}`, and a hole is a begin/end region that includes `#strings` again, so
    // the inner quote opens a nested string instead of ending the outer one.
    let esc = format!(
        "{{ \"name\": \"constant.character.escape.chezzi\", \"match\": {} }}",
        json_str_lit("\\\\.")
    );
    let hole = format!(
        "{{ \"match\": {} }}, {{ \"name\": \"meta.interpolation.chezzi\", \"begin\": {}, \"end\": {}, \"patterns\": [ {{ \"include\": \"#strings\" }}, {{ \"begin\": {}, \"end\": {} }} ] }}",
        json_str_lit("\\{\\{|\\}\\}"),
        json_str_lit("\\{"),
        json_str_lit("\\}"),
        json_str_lit("\\{"),
        json_str_lit("\\}"),
    );
    // (name, begin, end, escapes, holes)
    let rules: [(&str, &str, &str, bool, bool); 8] = [
        (
            "string.quoted.triple.chezzi",
            "[bBrR]\"\"\"",
            "\"\"\"",
            false,
            false,
        ),
        (
            "string.quoted.triple.chezzi",
            "\"\"\"",
            "\"\"\"",
            false,
            true,
        ),
        (
            "string.quoted.triple.chezzi",
            "[bBrR]'''",
            "'''",
            false,
            false,
        ),
        ("string.quoted.triple.chezzi", "'''", "'''", false, true),
        ("string.quoted.double.chezzi", "[bBrR]\"", "\"", true, false),
        ("string.quoted.double.chezzi", "\"", "\"", true, true),
        ("string.quoted.single.chezzi", "[bBrR]'", "'", true, false),
        ("string.quoted.single.chezzi", "'", "'", true, true),
    ];
    o.push_str("    \"strings\": {\n");
    o.push_str("      \"patterns\": [\n");
    for (i, (name, begin, end, escapes, holes)) in rules.iter().enumerate() {
        o.push_str(&format!(
            "        {{ \"name\": \"{name}\", \"begin\": {}, \"end\": {}",
            json_str_lit(begin),
            json_str_lit(end)
        ));
        let mut inner: Vec<&str> = Vec::new();
        if *escapes {
            inner.push(&esc);
        }
        if *holes {
            inner.push(&hole);
        }
        if !inner.is_empty() {
            o.push_str(&format!(", \"patterns\": [ {} ]", inner.join(", ")));
        }
        o.push_str(if i + 1 < rules.len() { " },\n" } else { " }\n" });
    }
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
mod tests;
