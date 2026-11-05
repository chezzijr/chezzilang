//! Lexer (a.k.a. scanner): turns source text into a flat stream of `Token`s.
//!
//! Two parts: the scanning state machine, and the tricky one — indentation →
//! INDENT/DEDENT tokens (the offside rule), suppressed inside bracket depth.

use std::{collections::VecDeque, fmt};

/// A single lexical token. This enum is provided as reference — you don't need to invent it.
/// (Spans/line numbers are intentionally omitted for M1 to keep it simple; we add them later.)
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // --- literals ---
    Int(i64),
    /// The bare decimal magnitude `9223372036854775808` (== `i64::MAX + 1` == `-i64::MIN`), which
    /// does NOT fit in an `i64`. It is legal ONLY when immediately negated: the parser folds
    /// `-9223372036854775808` into `Int(i64::MIN)`. A bare, un-negated occurrence is a parse error
    /// ("number too large to fit in target type"), so this value can never leak as a positive int.
    /// Larger magnitudes (`9223372036854775809`+) still error at lex time and never reach here.
    IntMinMagnitude,
    Float(f64),
    /// A `str` literal: its text parts (escapes resolved once, quotes stripped) and its
    /// interpolation holes, already lexed. See [`StrLit`].
    Str(StrLit),
    /// A `b"..."` byte-string literal: the resolved raw bytes (escapes applied, quotes stripped).
    /// Lexer-only, like the radix-prefixed int literals — no interpolation, no `\u`.
    Bytes(Vec<u8>),
    /// An `r"..."` / `r'...'` (and triple `r"""..."""`) raw-string literal: verbatim contents,
    /// quotes stripped. NO interpolation (braces `{`/`}` are literal, never a `{expr}` site) and
    /// NO escape processing (`\n` is backslash-n, `\` is a literal backslash). Its type downstream
    /// is plain `str` — a distinct token only so the later interpolation pass NEVER sees it.
    RawStr(String),
    Ident(String),

    // --- keywords ---
    Fn,
    Return,
    If,
    Else,
    /// `elif` — Python-style single-token else-if. NOT `else`+`if`: a bare `else if` no longer parses.
    Elif,
    For,
    While,
    In,
    Break,
    Continue,
    /// `pass` — a no-op statement (empty fn/method/control-flow body; alternative to `return`) and
    /// the sole-line empty-body marker for `protocol`/`struct` declarations. A real reserved keyword
    /// (never an identifier), so it can't be used as a name by construction.
    Pass,
    Struct,
    Enum,
    Protocol,
    Type,
    Match,
    Recover,
    Defer,
    Assert,
    Test,
    Spawn,
    Parallel,
    Wait,
    Yield,
    Import,
    Extern,
    /// `native` — the prelude/std-only `native fn` / `native ctor` declaration keyword. Declares a
    /// body-less universe-builtin SIGNATURE whose body is bound natively (name-keyed `do_builtin`).
    /// A full keyword (corpus-safe: no `.chz` uses `native` as a bare identifier).
    Native,
    From,
    As,
    /// `const` — the immutable-binding modifier (`PI: const float = 3.14`). A full keyword
    /// (corpus-safe: no `.chz` uses `const` as a bare identifier). Legal only as a type prefix in
    /// a single-name typed let; the parser rejects it on params, `:=`, and destructuring.
    Const,
    And,
    Or,
    Not,
    True,
    False,
    /// `None`. Named `NoneKw`: a variant `None` would shadow `Option::None` under `use Token::*`.
    NoneKw,
    /// `where` — the generic-bound clause keyword (`fn f[T]() where T: Comparable`). A full keyword
    /// (corpus-safe: no `.chz` uses `where` as a bare identifier). Introduces a comma-separated list
    /// of `IDENT (: bound (+ bound)*)` entries after a fn/native-fn signature; the checker merges
    /// each entry's bounds into the matching type parameter (see `parse_where_bounds`).
    Where,

    // --- operators ---
    Plus,             // +
    Minus,            // -
    Star,             // *
    Slash,            // /
    Percent,          // %
    Assign,           // =
    Walrus,           // :=
    EqEq,             // ==
    NotEq,            // !=
    Lt,               // <
    LtEq,             // <=
    Gt,               // >
    GtEq,             // >=
    PlusEq,           // +=
    MinusEq,          // -=
    StarEq,           // *=
    SlashEq,          // /=
    PercentEq,        // %=
    AmpEq,            // &=
    PipeEq,           // |=
    CaretEq,          // ^=
    ShlEq,            // <<=
    ShrEq,            // >>=
    Arrow,            // ->
    Pipe,             // |>
    Question,         // ?
    QuestionDot,      // ?.  (optional chaining; `?..` is `?` then `..`)
    QuestionQuestion, // ??  (null-coalescing; `Lexer::question` decides by spacing)
    Bang,             // !  (used in type position: `T!` = Result[T])
    Amp,              // &  (bitwise and)
    Caret,            // ^  (bitwise xor)
    BitOr,            // |  (bitwise or)
    Shl,              // << (left shift)
    Shr,              // >> (right shift)

    // --- delimiters ---
    LParen,    // (
    RParen,    // )
    LBracket,  // [
    RBracket,  // ]
    LBrace,    // {  (map literal)
    RBrace,    // }
    Comma,     // ,
    Colon,     // :
    Dot,       // .
    DotDot,    // ..  (range, e.g. 0..10)
    DotDotEq,  // ..= (inclusive range, e.g. 0..=10)
    DotDotDot, // ... (variadic param marker, e.g. `...args: T`)

    // --- layout (the interesting part) ---
    Newline, // end of a logical line
    Indent,  // increase in indentation
    Dedent,  // decrease in indentation
    Eof,     // end of input
}

/// Source location of a token: 1-based line and 1-based column (column counts characters
/// from the start of the line). Added in M2 so the parser can point at exact positions.
///
/// `Default` (line 0, col 0) is the SENTINEL for a SYNTHESIZED span — a span filled in by a phase
/// other than the parser (e.g. a `Type::Named` the checker builds from an inferred name). Because
/// the lexer's lines/cols are 1-based, `(0, 0)` can never collide with a real source position, so a
/// synthesized span never matches an editor hover/overlay key. Diagnostic-only; runtime-inert.
///
/// `file` is NOT diagnostic sugar and it is NOT dead — **a `Span` is a cross-half TABLE KEY**, and
/// `file` is what makes that key injective across modules. `WitnessKey` and
/// `CarrierKey` (`src/checker/ty.rs:24,43,109`) are all `(graph_module_idx, frag_ctx: Span,
/// frag_ord, key_span: Span)`: the checker records a decision under one and the type-blind compiler
/// looks it up under the same one. `desugar` splices a callee's default-parameter expression into
/// the CALLER's AST as a clone that keeps the DEFINING module's spans, so without `file` two
/// unrelated call sites at the same `line:col` in two different files collapse onto one entry, the
/// later `insert` wins, and the survivor's decision is applied to both — a silent wrong value under
/// a green `chezzi check` (parity is blind to it). See `docs/contributing/gaps.md`
/// **W7-49** for the measured repro. Deleting this field re-opens that hole silently.
///
/// `0` = synthesized / standalone single-file lex (`tokenize`, the editor overlay, `chezzi tokens`);
/// real resolver-loaded modules get `1..n`, assigned once at `resolver::Builder::parse`.
///
/// **Keep this type SMALL — it is 12 bytes (3 × `u32`) and that is deliberate, not incidental.**
/// `Proto.lines` (`src/vm/op.rs:502`) holds one `Span` PER OPCODE, so `sizeof(Span)` is hot VM data:
/// it sets the cache footprint of every compiled function. It is also a cross-half table key (see
/// above), which means it is cloned and hashed on the checker→compiler path. Growing it is not free
/// and the cost is not local — at 24 bytes (`usize` line/col + `file`) the `map` bench regressed
/// 1.07× AND the extra AST-node width pushed both `parser::MAX_DEPTH` (64 → 48) and
/// `vm::VM_STACK_BYTES` (384 → 512 MiB) off their calibrated margins. `u32` line/col is not a limit
/// in practice: a source file cannot realistically reach 4 billion lines or columns. A fourth field
/// costs 4 more bytes plus padding — measure `benches/run.chz` and re-probe those two constants
/// before adding one.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, borsh::BorshSerialize, borsh::BorshDeserialize,
)]
pub struct Span {
    pub line: u32,
    pub col: u32,
    /// Module identity — see the type doc. `0` = synthesized / standalone.
    pub file: u32,
}

impl Span {
    /// The span every runtime-origin error and every compiler-synthesized opcode carries: "line 1,
    /// col 1 of nowhere in particular". One constant so the next `Span` field costs one edit, not
    /// forty-five.
    pub const RUNTIME: Span = Span {
        line: 1,
        col: 1,
        file: 0,
    };

    /// The span of an import no one wrote (the `import Range from std.prelude` the resolver adds to
    /// every module): no source position, and no editor probe, equals it. No registry that a message
    /// reads may hold it, so no message prints a line the file does not have.
    pub const NOWHERE: Span = Span {
        line: u32::MAX,
        col: u32::MAX,
        file: 0,
    };

    pub fn is_nowhere(self) -> bool {
        self == Span::NOWHERE
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, col {}", self.line, self.col)
    }
}

/// Render a source coordinate for a human: `path:line:col` when the span's module resolves to a
/// path, else the historical `line N, col M`. THE single source of truth for the form — the runtime
/// trace, the checker's plain-text diagnostics and the `--errors=json` `file` key all go through it,
/// so they cannot drift.
///
/// The path is made relative to the process's current directory when it lives under it (what rustc
/// and tsc do, and what makes the coordinate clickable in a terminal); absolute otherwise, so a
/// `std/` or out-of-tree module is never ambiguous.
pub fn render_span(span: Span, path: Option<&std::path::Path>) -> String {
    match path {
        None => format!("line {}, col {}", span.line, span.col),
        Some(p) => format!("{}:{}:{}", display_path(p).display(), span.line, span.col),
    }
}

/// Relativize `p` to the process's current directory when it lives under it; otherwise return it
/// as-is (already absolute, or relativizing failed). Never canonicalizes — that hits the filesystem
/// and would turn a path to a since-deleted file into an error. `pub`: `--errors=json`'s `file` key
/// (`main.rs::diags_json`) renders just the path half of what [`render_span`] renders combined, so it
/// calls this directly rather than parsing a `path:line:col` string back apart.
pub fn display_path(p: &std::path::Path) -> std::path::PathBuf {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| p.strip_prefix(&cwd).ok())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| p.to_path_buf())
}

/// Render a three-line source echo for a diagnostic: a blank gutter line, the source line at
/// `span.line`, then a caret row underlining the span's column through the end of its identifier
/// word, string literal or number literal (via [`word_end_col`], the SAME scan
/// `--errors=json`'s `end_col` uses — caret width and `end_col` cannot drift apart). Returns `None` when `span.line` is past the end of
/// `source`.
/// Removes at most one leading U+FEFF; a second BOM and a mid-file BOM stay lex errors, matching
/// CPython measured 2026-09-03.
pub fn strip_bom(source: &str) -> &str {
    source.strip_prefix('\u{feff}').unwrap_or(source)
}

/// The source as the lexer reads it: one leading BOM removed ([`strip_bom`]) and each CRLF pair
/// read as one `\n`. The one owner of "what is a line break" in source text: every lexer site
/// (indentation, blank and comment lines, literal bodies) sees only `\n`, and the editor indexes
/// lexer columns and literal lengths into this same list. A lone `\r` stays, so it keeps its old
/// reading. Columns do not move: the dropped `\r` ends its line.
pub fn source_chars(source: &str) -> Vec<char> {
    let mut chars: Vec<char> = Vec::with_capacity(source.len());
    let mut it = strip_bom(source).chars().peekable();
    while let Some(c) = it.next() {
        if c == '\r' && it.peek() == Some(&'\n') {
            continue;
        }
        chars.push(c);
    }
    chars
}

pub fn render_snippet(span: Span, source: &str) -> Option<String> {
    // A Span::default() has line: 0; only the fatal path (a synthesized import span) can reach
    // this function with one.
    if span.line == 0 {
        return None;
    }
    let raw = source.lines().nth(span.line as usize - 1)?;
    let text: String = raw
        .trim_end_matches('\r')
        .chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .collect();
    let end = word_end_col(source, span.line as usize, span.col as usize) as usize;
    let col0 = span.col.saturating_sub(1) as usize;
    let width = end.saturating_sub(col0).max(1);
    let line_no = span.line.to_string();
    let pad = " ".repeat(line_no.len());
    Some(format!(
        "{pad} |\n{line_no} | {text}\n{pad} | {}{}",
        " ".repeat(col0),
        "^".repeat(width)
    ))
}

/// One piece of a `str` literal: text with its escapes resolved, or an interpolation hole.
#[derive(Clone, PartialEq)]
pub enum StrPart {
    Text(String),
    Hole(Hole),
}

/// One `{expr}` / `{expr:spec}` hole of an interpolated literal. The lexer finds it while it scans
/// the literal ([`Lexer::quoted`]) and lexes its code once, in place, one bracket deep: `tokens`
/// carry their real source spans and end with an `Eof` token. Nothing re-reads the hole's text.
#[derive(Clone, PartialEq)]
pub struct Hole {
    /// The hole's expression tokens, then `Eof`.
    pub tokens: Vec<Tok>,
    /// The format spec after the first `:` at hole depth, `None` for a bare `{expr}`.
    pub spec: Option<Vec<SpecPart>>,
    /// Where the spec's first char sits (the position a spec error reports).
    pub spec_at: Span,
}

/// One piece of a format spec: literal text (escapes resolved once) or a nested `{expr}` field,
/// lexed like a hole's code (tokens, then `Eof`).
#[derive(Clone, PartialEq)]
pub enum SpecPart {
    Text(String),
    Field(Vec<Tok>),
}

/// The payload of a `str` literal: its parts, in source order, and its extent in the source.
///
/// A literal with no hole holds zero parts (`""`) or one `Text`; adjacent text always merges, so
/// two literals that spell the same value are equal. `PartialEq` compares `parts` only.
///
/// `Debug` prints a literal with no hole exactly as the `String` it holds, so `chezzi tokens`
/// output of a plain literal stays what it was.
#[derive(Clone, Default)]
pub struct StrLit {
    pub parts: Vec<StrPart>,
    /// The literal's length in source chars, delimiters included. `0` for a synthesized literal.
    pub len: usize,
}

impl StrLit {
    /// The literal's text when it holds no hole, `None` otherwise.
    pub fn plain(&self) -> Option<&str> {
        match self.parts.as_slice() {
            [] => Some(""),
            [StrPart::Text(t)] => Some(t),
            _ => None,
        }
    }
}

impl PartialEq for StrLit {
    fn eq(&self, other: &Self) -> bool {
        self.parts == other.parts
    }
}

impl From<&str> for StrLit {
    fn from(s: &str) -> Self {
        StrLit::from(s.to_string())
    }
}

impl From<String> for StrLit {
    fn from(text: String) -> Self {
        let parts = if text.is_empty() {
            Vec::new()
        } else {
            vec![StrPart::Text(text)]
        };
        StrLit { parts, len: 0 }
    }
}

impl fmt::Debug for StrPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StrPart::Text(t) => fmt::Debug::fmt(t, f),
            StrPart::Hole(h) => f
                .debug_struct("Hole")
                .field(
                    "tokens",
                    &h.tokens.iter().map(|t| &t.kind).collect::<Vec<_>>(),
                )
                .field("spec", &h.spec)
                .finish(),
        }
    }
}

impl fmt::Debug for SpecPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpecPart::Text(t) => fmt::Debug::fmt(t, f),
            SpecPart::Field(toks) => f
                .debug_tuple("Field")
                .field(&toks.iter().map(|t| &t.kind).collect::<Vec<_>>())
                .finish(),
        }
    }
}

impl fmt::Debug for StrLit {
    /// A literal with no hole prints as the bare `String` (see the type's doc); one with holes
    /// prints its parts.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.plain() {
            Some(text) => fmt::Debug::fmt(text, f),
            None => f.debug_list().entries(&self.parts).finish(),
        }
    }
}

/// The escapes a literal's text and a format spec's text accept after a backslash, each with the
/// char it stands for. `\u{HEX}` is the eighth, read by [`Lexer::escape`] itself. This table is the
/// one source of "which letters are escapes": [`Lexer::escape`] reads it and the interpolation grid
/// test enumerates it.
pub const ESCAPES: [(char, char); 7] = [
    ('n', '\n'),
    ('t', '\t'),
    ('r', '\r'),
    ('\\', '\\'),
    ('"', '"'),
    ('\'', '\''),
    ('0', '\0'),
];

/// The deepest chain of interpolated literals nested in holes (`"{"{"{x}"}"}"` is three). CPython
/// 3.14.7 accepts 149 levels and rejects 150 ("too many nested f-strings or t-strings").
pub const MAX_HOLE_NEST: usize = 149;

/// The holes [`literal_len`] follows: far below [`MAX_HOLE_NEST`], and beyond what a line holds.
pub const ONE_LINE_HOLE_NEST: usize = 8;

/// A token paired with where it came from. The lexer emits these; the `Token` payload is the
/// `kind`. (`tokens` CLI output prints only `kind`, so M1 output is unchanged.)
#[derive(Debug, Clone, PartialEq)]
pub struct Tok {
    pub kind: Token,
    pub span: Span,
    /// The count of leading spaces of the physical line, set only on the first token of a line whose
    /// leading whitespace is spaces only. The parser reads it through [`open_layout`].
    pub indent: Option<u32>,
}

/// One step of the indent stack for a content line at column `width`: `[Indent]` when deeper,
/// one `Dedent` per closed level when shallower, `[]` at the same level. `scan_indentation` and
/// [`open_layout`] share it, so the layout rule has one owner.
fn indent_step(indents: &mut Vec<usize>, width: usize) -> Result<Vec<Token>, &'static str> {
    let top = *indents.last().unwrap();
    if width > top {
        indents.push(width);
        return Ok(vec![Token::Indent]);
    }
    if width == top {
        return Ok(Vec::new());
    }
    let mut dedents = Vec::new();
    while *indents.last().unwrap() > width {
        indents.pop();
        dedents.push(Token::Dedent);
    }
    // the new width must line up with some outer level we landed on
    if *indents.last().unwrap() != width {
        return Err("inconsistent dedent — does not match any outer indentation level");
    }
    Ok(dedents)
}

/// Open layout for a block whose line-ending `:` is `toks[colon]`, inside brackets where the lexer
/// emitted none. The parser asks for it, so a map literal `{"a":` + newline + `1}` never gets layout.
/// Returns `Ok(false)` and leaves `toks` unchanged when the next token is on the same line or is
/// not indented deeper than the opener's line. Otherwise it splices `Newline` / `Indent` / `Dedent`
/// tokens up to the block's end: a closer at its own bracket depth, `Eof`, or a line indented no
/// deeper than the opener's line.
pub fn open_layout(toks: &mut Vec<Tok>, colon: usize) -> Result<bool, LexError> {
    let first = colon + 1;
    let Some(first_indent) = toks.get(first).and_then(|t| t.indent) else {
        return Ok(false);
    };
    let Some(base) = toks[..=colon].iter().rev().find_map(|t| t.indent) else {
        return Ok(false);
    };
    let (base, first_indent) = (base as usize, first_indent as usize);
    if first_indent <= base {
        return Ok(false);
    }
    let mut stack = vec![base];
    let mut depth = 0usize;
    let mut out: Vec<Tok> = Vec::new();
    let mut j = first;
    let mk = |kind: Token, at: &Tok| Tok {
        kind,
        span: at.span,
        indent: None,
    };
    loop {
        let t = &toks[j];
        let closer = matches!(t.kind, Token::RParen | Token::RBracket | Token::RBrace);
        if t.kind == Token::Eof || (closer && depth == 0) {
            out.push(mk(Token::Newline, t));
            for _ in 1..stack.len() {
                out.push(mk(Token::Dedent, t));
            }
            break;
        }
        if depth == 0
            && t.kind != Token::Pipe
            && let Some(w) = t.indent
        {
            let w = w as usize;
            out.push(mk(Token::Newline, t));
            if w <= base {
                for _ in 1..stack.len() {
                    out.push(mk(Token::Dedent, t));
                }
                break;
            }
            match indent_step(&mut stack, w) {
                Ok(layout) => out.extend(layout.into_iter().map(|k| mk(k, t))),
                Err(msg) => {
                    return Err(LexError {
                        line: t.span.line as usize,
                        col: t.span.col as usize,
                        message: msg.to_string(),
                    });
                }
            }
        }
        if matches!(t.kind, Token::LParen | Token::LBracket | Token::LBrace) {
            depth += 1;
        } else if closer {
            depth -= 1;
        }
        out.push(t.clone());
        j += 1;
    }
    toks.splice(first..j, out);
    Ok(true)
}

/// A captured doc-comment line: its 1-based source `line` and the stripped comment text (the chars
/// after `#`, with one optional leading space removed). Collected on the lexer side-channel so the
/// parser can attach the contiguous run above a declaration as its doc; never enters the token stream.
pub type DocComment = (usize, String);

/// Error returned when the source can't be tokenized.
#[derive(Debug, Clone, PartialEq)]
pub struct LexError {
    pub line: usize,
    /// 1-based column of the offending character. Built from the same [`Span`] as `line` — see
    /// [`Lexer::error_span`] for the position rule (point at the offending char; an unterminated
    /// delimiter points at its opener).
    pub col: usize,
    pub message: String,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "lex error (line {}, col {}): {}",
            self.line, self.col, self.message
        )
    }
}

/// The spelling of the `None` keyword: the absent value, the void type, and the name of the
/// prelude variant it denotes. Its `KEYWORDS` row is the one owner of that fact; later phases read
/// the node the parser builds from the token and never test this spelling.
pub const NONE: &str = "None";

/// The single source of truth for the language's keyword surface: every reserved word paired with
/// the [`Token`] it lexes to. `keyword()` looks itself up here, and the editor tooling (the VSCode
/// TextMate grammar generator and the LSP semantic-token lengths) derives the keyword set from this
/// same table — so adding a keyword here flows through to highlighting with no second edit.
pub const KEYWORDS: &[(&str, Token)] = &[
    ("fn", Token::Fn),
    ("return", Token::Return),
    ("if", Token::If),
    ("else", Token::Else),
    ("elif", Token::Elif),
    ("for", Token::For),
    ("while", Token::While),
    ("in", Token::In),
    ("break", Token::Break),
    ("continue", Token::Continue),
    ("pass", Token::Pass),
    ("struct", Token::Struct),
    ("enum", Token::Enum),
    ("protocol", Token::Protocol),
    ("type", Token::Type),
    ("match", Token::Match),
    ("recover", Token::Recover),
    ("defer", Token::Defer),
    ("assert", Token::Assert),
    ("test", Token::Test),
    ("spawn", Token::Spawn),
    ("parallel", Token::Parallel),
    ("wait", Token::Wait),
    ("yield", Token::Yield),
    ("import", Token::Import),
    ("extern", Token::Extern),
    ("native", Token::Native),
    ("from", Token::From),
    ("as", Token::As),
    ("const", Token::Const),
    ("and", Token::And),
    ("or", Token::Or),
    ("not", Token::Not),
    ("true", Token::True),
    ("false", Token::False),
    (NONE, Token::NoneKw),
    ("where", Token::Where),
];

/// Every operator + delimiter token variant, paired (implicitly, via [`Token::lexeme`]) with its
/// one fixed spelling. Single-sources the operator set for the TextMate generator and the
/// semantic-token operator class. Ordering is longest-first within each char family is NOT required
/// here (callers that build regex alternations sort by length); this list only needs to be complete.
///
/// Consumed only by the editor-tooling layer (`src/editor`, reachable via `src/lib.rs`), so the
/// `chezzi` binary's copy of the lexer never reads it — `allow(dead_code)` for that build.
#[allow(dead_code)]
pub const PUNCTUATION: &[Token] = &[
    // operators
    Token::Plus,
    Token::Minus,
    Token::Star,
    Token::Slash,
    Token::Percent,
    Token::Assign,
    Token::Walrus,
    Token::EqEq,
    Token::NotEq,
    Token::Lt,
    Token::LtEq,
    Token::Gt,
    Token::GtEq,
    Token::PlusEq,
    Token::MinusEq,
    Token::StarEq,
    Token::SlashEq,
    Token::PercentEq,
    Token::AmpEq,
    Token::PipeEq,
    Token::CaretEq,
    Token::ShlEq,
    Token::ShrEq,
    Token::Arrow,
    Token::Pipe,
    Token::Question,
    Token::QuestionDot,
    Token::QuestionQuestion,
    Token::Bang,
    Token::Amp,
    Token::Caret,
    Token::BitOr,
    Token::Shl,
    Token::Shr,
    // delimiters
    Token::LParen,
    Token::RParen,
    Token::LBracket,
    Token::RBracket,
    Token::LBrace,
    Token::RBrace,
    Token::Comma,
    Token::Colon,
    Token::Dot,
    Token::DotDot,
    Token::DotDotEq,
    Token::DotDotDot,
];

impl Token {
    /// The fixed source spelling of a keyword / operator / delimiter token (e.g. `Token::Fn` →
    /// `"fn"`, `Token::Walrus` → `":="`). `None` for tokens whose text varies (literals, idents) or
    /// that have no spelling at all (layout: `Newline`/`Indent`/`Dedent`/`Eof`).
    ///
    /// This is the single source for both the TextMate operator alternation and the fixed-token
    /// length used by the LSP semantic-token encoder.
    ///
    /// Used only by the editor-tooling layer (via `src/lib.rs`); the `chezzi` binary's lexer copy
    /// never calls it, hence `allow(dead_code)` for that build.
    #[allow(dead_code)]
    pub fn lexeme(&self) -> Option<&'static str> {
        if let Some((w, _)) = KEYWORDS.iter().find(|(_, t)| t == self) {
            return Some(w);
        }
        use Token::*;
        Some(match self {
            // operators
            Plus => "+",
            Minus => "-",
            Star => "*",
            Slash => "/",
            Percent => "%",
            Assign => "=",
            Walrus => ":=",
            EqEq => "==",
            NotEq => "!=",
            Lt => "<",
            LtEq => "<=",
            Gt => ">",
            GtEq => ">=",
            PlusEq => "+=",
            MinusEq => "-=",
            StarEq => "*=",
            SlashEq => "/=",
            PercentEq => "%=",
            AmpEq => "&=",
            PipeEq => "|=",
            CaretEq => "^=",
            ShlEq => "<<=",
            ShrEq => ">>=",
            Arrow => "->",
            Pipe => "|>",
            Question => "?",
            QuestionDot => "?.",
            QuestionQuestion => "??",
            Bang => "!",
            Amp => "&",
            Caret => "^",
            BitOr => "|",
            Shl => "<<",
            Shr => ">>",
            // delimiters
            LParen => "(",
            RParen => ")",
            LBracket => "[",
            RBracket => "]",
            LBrace => "{",
            RBrace => "}",
            Comma => ",",
            Colon => ":",
            Dot => ".",
            DotDot => "..",
            DotDotEq => "..=",
            DotDotDot => "...",
            Int(_) | IntMinMagnitude | Float(_) | Str(_) | Bytes(_) | RawStr(_) | Ident(_) | Fn
            | Return | If | Else | Elif | For | While | In | Break | Continue | Pass | Struct
            | Enum | Protocol | Type | Match | Recover | Defer | Assert | Test | Spawn
            | Parallel | Wait | Yield | Import | Extern | Native | From | As | Const | And | Or
            | Not | True | False | NoneKw | Where | Newline | Indent | Dedent | Eof => return None,
        })
    }
}

/// Look up whether an identifier is actually a keyword.
/// HINT (sub-step 1e): call this after you've scanned a whole word.
/// Whether `word` is a keyword: the only names a program may not declare.
pub fn is_keyword(word: &str) -> bool {
    keyword(word).is_some()
}

fn keyword(word: &str) -> Option<Token> {
    KEYWORDS
        .iter()
        .find(|(w, _)| *w == word)
        .map(|(_, t)| t.clone())
}

/// The lexer. Holds the source as a list of chars plus a cursor.
///
/// LEARN: we collect into `Vec<char>` so indexing is by *character*, not byte. Rust `String`
/// is UTF-8 and you can't index it by character cheaply. For a learning lexer, `Vec<char>` is
/// the simplest correct choice. (A perf-tuned lexer would scan bytes — a later optimization.)
pub struct Lexer {
    chars: Vec<char>,
    pos: usize,  // index of the next char to read
    line: usize, // current line, 1-based (for error messages)
    /// Module id stamped into every emitted span's [`Span::file`] (default 0 = standalone /
    /// synthesized). Assigned by the one production graph lex seam, `resolver::Builder::parse`.
    file: u32,
    line_start: usize, // char index where the current line begins (for column tracking)
    indents: Vec<usize>, // the indentation stack. Starts as vec![0].
    at_line_start: bool, // true when the next char begins a fresh logical line
    pending: VecDeque<Tok>, // layout tokens computed together but emitted one-per-call (Dedents)
    bracket_depth: usize, // nesting depth of (), [], {}; >0 suppresses layout (multi-line literals)
    /// `bracket_depth` of the innermost hole whose code is being lexed, `0` outside every hole.
    /// A `:` at this depth is the format-spec separator, so it never munches into `:=`; a backslash
    /// at any depth while this is set is an error (hole text is code, and code has no escapes).
    hole_base: usize,
    /// How many holes enclose the cursor, bounded by [`MAX_HOLE_NEST`].
    hole_nest: usize,
    /// Where the first `#` of the current hole sits. A `#` comments out the rest of its line,
    /// including the hole's closing `}`, which is what an unterminated hole reports.
    hole_comment: Option<Span>,
    /// Side-channel for doc-comments: `(line, stripped_text)` for every comment-ONLY line
    /// (a `#` line with no preceding tokens). Inline trailing comments (STEP B) are NOT recorded,
    /// so they can never become docs. The token stream is unaffected — this is a pure side table the
    /// parser consults for the contiguous comment run immediately above a declaration.
    doc_comments: Vec<DocComment>,
    /// Memoized char index of the `|` of a resolved leading-`|>` continuation (see
    /// [`Lexer::pipe_continues_next_line`]): scan a blank/comment run once, not once per line.
    pipe_cont: Option<usize>,
    /// Whether the last emitted token ends an operand ([`ends_operand`]): a `??` after one can
    /// be the coalesce, a `??` after anything else is two prefix `?` ([`Lexer::question`]).
    after_operand: bool,
}

impl Lexer {
    pub fn new(source: &str) -> Self {
        Lexer::new_file(source, 0)
    }

    /// Like [`Lexer::new`] but stamps every emitted span with the module id `file` (see
    /// [`Span::file`]). `0` is identical to [`Lexer::new`].
    pub fn new_file(source: &str, file: u32) -> Self {
        Lexer {
            chars: source_chars(source),
            pos: 0,
            line: 1,
            file,
            line_start: 0,
            indents: vec![0],
            at_line_start: true,
            pending: VecDeque::new(),
            bracket_depth: 0,
            hole_base: 0,
            hole_nest: 0,
            hole_comment: None,
            doc_comments: Vec::new(),
            pipe_cont: None,
            after_operand: false,
        }
    }

    /// The span pointing at character index `pos` on the current line.
    fn span_at(&self, pos: usize) -> Span {
        Span {
            // `as u32`: both are 1-based source counters bounded by the source length — a file
            // with 4 billion lines (or a line with 4 billion columns) is not reachable.
            line: self.line as u32,
            col: (pos - self.line_start + 1) as u32,
            file: self.file,
        }
    }

    // ----- char cursor helpers (sub-step 1a) -----
    // Two are done for you as worked examples. Finish the others.

    /// Return the current char without consuming it, or `'\0'` at end of input.
    /// (Worked example — study how this reads `self.chars` safely.)
    fn peek(&self) -> char {
        *self.chars.get(self.pos).unwrap_or(&'\0')
    }

    /// True when there's nothing left to read. (Worked example.)
    fn is_at_end(&self) -> bool {
        self.pos >= self.chars.len()
    }

    /// Return the char one past the current one (lookahead of 2), or `'\0'`.
    /// HINT: like `peek`, but at `self.pos + 1`.
    fn peek_next(&self) -> char {
        *self.chars.get(self.pos + 1).unwrap_or(&'\0')
    }

    /// Non-consuming lookahead from `self.pos` (which sits just past a '\n'): does the next real
    /// content begin with the two chars `|>`, indented at least as deep as the block currently
    /// open? Blank lines, indentation and comment-only lines are skipped. Exactly `|>` — `|`,
    /// `||`, `|=` are not continuations.
    ///
    /// The indent floor keeps the offside rule intact: a `|>` line SHALLOWER than the open block
    /// must close that block (a Dedent) rather than be silently absorbed into it — otherwise a
    /// column-0 `|>` after an indented body would rewrite that body's last statement with no
    /// diagnostic. Below the floor we return false and let the normal layout path run, so the
    /// parser reports the stray `|>`. Indentation is spaces-only (`scan_indentation` rejects
    /// tabs), so a tab-indented `|>` line is likewise not a continuation — it falls through to
    /// that error.
    ///
    /// The resolved target is memoized: inside a continuation region `at_line_start` stays false,
    /// so every blank/comment line's '\n' re-enters this lookahead. Without the memo a run of k
    /// such lines costs O(k²) chars (a 50k-blank-line file lexed in ~9s); with it, each run is
    /// scanned once.
    fn pipe_continues_next_line(&mut self) -> bool {
        match self.pipe_cont {
            Some(t) if self.pos <= t => return true,
            Some(_) => self.pipe_cont = None,
            None => {}
        }
        let mut i = self.pos;
        let mut line_start = self.pos; // index just past the most recent '\n'
        loop {
            match self.chars.get(i) {
                Some(' ') | Some('\t') | Some('\r') => i += 1,
                Some('\n') => {
                    i += 1;
                    line_start = i;
                }
                Some('#') => {
                    while self.chars.get(i).is_some_and(|c| *c != '\n') {
                        i += 1;
                    }
                }
                _ => break,
            }
        }
        if self.chars.get(i) != Some(&'|') || self.chars.get(i + 1) != Some(&'>') {
            return false;
        }
        let indent = &self.chars[line_start..i];
        if indent.iter().any(|c| *c != ' ') || indent.len() < *self.indents.last().unwrap() {
            return false;
        }
        self.pipe_cont = Some(i);
        true
    }

    /// Consume the current char and return it. Remember to advance `self.pos`.
    /// HINT: read the char first, then bump `self.pos`, then return it.
    fn advance(&mut self) -> char {
        let char = self.peek();
        self.pos += 1;
        char
    }

    /// If the current char equals `expected`, consume it and return true. Otherwise false.
    /// LEARN: this is the classic trick for two-char operators like `:=`, `==`, `->`.
    fn match_char(&mut self, expected: char) -> bool {
        if self.peek() == expected {
            self.advance();
            true
        } else {
            false
        }
    }

    /// The token a `?` at `start` begins: the one place a run of `?` becomes tokens (S2-3).
    /// A run of exactly two after an operand is the coalesce `??` when the spacing says binary,
    /// Swift's rule: glued on both sides (`a??b`) or on neither (`a ?? b`). Glued on the left only
    /// is two `?`: postfix `oo(x)??`, type `int??`, `int??=None`. With no operand before it
    /// (`??x`, `x := ?? 5`) the run is prefix, two `?`. Any other run is one `?` per char. `?.` is
    /// the optional chain unless a second `.` follows: `o(x)?..5` is `?` then `..`. A value then
    /// `??b` is an error: no prefix `??` follows a value, so the spacing is a mistyped coalesce.
    fn question(&mut self, start: usize) -> Result<Token, LexError> {
        let before = start.checked_sub(1).map(|i| self.chars[i]);
        let run = self.chars[start..]
            .iter()
            .take_while(|&&c| c == '?')
            .count();
        if self.after_operand && before != Some('?') && run == 2 {
            let left = before.is_some_and(|c| !c.is_whitespace());
            let right = self.starts_operand(start + 2);
            if left == right {
                self.advance();
                return Ok(Token::QuestionQuestion);
            }
            if right {
                let msg =
                    "'??' after a value takes the same spacing on both sides: `a ?? b` or `a??b`";
                return Err(self.error_at(start, msg));
            }
        }
        if self.peek() == '.' && self.peek_next() != '.' {
            self.advance();
            return Ok(Token::QuestionDot);
        }
        Ok(Token::Question)
    }

    /// Whether the char at `i` can start an operand: a word char (a name, a keyword, a number, a
    /// `b"` or `r"` prefix), a quote, an opener, or a prefix `-`, `!` or `?` that does not begin
    /// `->`, `-=` or `!=`. [`Lexer::question`] reads a `??` glued to such a char as the coalesce.
    fn starts_operand(&self, i: usize) -> bool {
        let next = self.chars.get(i + 1).copied();
        match self.chars.get(i).copied() {
            Some('-') => !matches!(next, Some('>' | '=')),
            Some('!') => next != Some('='),
            Some(c) => is_word(c) || matches!(c, '"' | '\'' | '(' | '[' | '{' | '?'),
            None => false,
        }
    }

    // ----- the main driver -----

    /// Produce all tokens. This outer loop is provided so you can see how the pieces connect:
    /// it just calls `next_token` until it returns `Eof`. The real work is in `next_token`
    /// (and the indentation handling it triggers).
    pub fn tokenize(mut self) -> Result<Vec<Tok>, LexError> {
        let mut tokens = Vec::new();
        loop {
            let tok = self.next_token()?;
            let done = tok.kind == Token::Eof;
            tokens.push(tok);
            if done {
                break;
            }
        }
        Ok(tokens)
    }

    /// Like [`tokenize`](Self::tokenize), but also returns the captured doc-comment side table
    /// (`(line, stripped_text)` for each comment-only line). The token stream is byte-identical to
    /// `tokenize` — the comments live purely on the side channel — so the only callers that need
    /// this are the ones threading docs into the AST (the resolver). `tokenize` keeps its exact
    /// signature so every existing caller and the `tokens` CLI snapshot stay unchanged.
    pub fn tokenize_with_comments(mut self) -> Result<(Vec<Tok>, Vec<DocComment>), LexError> {
        let mut tokens = Vec::new();
        loop {
            let tok = self.next_token()?;
            let done = tok.kind == Token::Eof;
            tokens.push(tok);
            if done {
                break;
            }
        }
        Ok((tokens, self.doc_comments))
    }

    /// Produce the next single token, and record whether it ends an operand ([`ends_operand`]).
    /// Every scanned token comes out of here, so [`Lexer::question`] reads the flag of the token
    /// before it.
    fn next_token(&mut self) -> Result<Tok, LexError> {
        let tok = self.scan_token()?;
        self.after_operand = ends_operand(&tok.kind, self.after_operand);
        Ok(tok)
    }

    /// The body of [`Self::next_token`]: scan one token (it's fine for ONE call to also emit
    /// layout tokens — see the indentation design note below; you may buffer pending Dedents in a
    /// field).
    ///
    /// LEARN — the overall shape of a scanner's core:
    ///   1. Handle start-of-line indentation (1g/1h) — this is what makes Chezzi special.
    ///   2. Skip inline whitespace and comments (1f).
    ///   3. If at end of input → flush dedents, then `Eof` (1i).
    ///   4. Otherwise look at the current char and dispatch:
    ///      digit         → number (1c)
    ///      '"'           → string (1d)
    ///      letter or '_' → identifier / keyword (1e)
    ///      else          → operator / delimiter (1b)
    ///
    /// Build this up sub-step by sub-step. Start tiny: get `+` and `Eof` working, run the
    /// first guiding test, then add more. Don't try to write it all at once.
    ///
    /// WORKED SKELETON: the structure + operator dispatch are provided as a teaching example
    /// (like `peek`/`is_at_end` were). YOUR remaining work is the three helpers below —
    /// `identifier` (1e), `number` (1c), `string` (1d) — and INDENTATION (1h), whose seam is
    /// marked with `TODO(1h)` inside this function.
    fn scan_token(&mut self) -> Result<Tok, LexError> {
        // === STEP 0: drain queued layout tokens ===
        // One source position can need several tokens at once (closing N blocks → N Dedents).
        // We compute them together in `scan_indentation` and queue the extras here.
        if let Some(tok) = self.pending.pop_front() {
            return Ok(tok);
        }

        // The suppressed-newline path inside brackets `continue 'scan`s here (instead of
        // recursing) so an unbounded run of blank lines inside a literal cannot overflow the
        // stack. Every other path returns, so the loop runs at most once per emitted token.
        'scan: loop {
            // === STEP A: start-of-line indentation (1h) ===
            // Inside brackets (multi-line literals) layout is suppressed: skip scan_indentation so
            // the indent stack stays frozen. We deliberately leave `at_line_start` untouched —
            // STEP B/C/D/E below still run, and STEP C's EOF guard depends on at_line_start, so
            // clobbering it here would spin the loop on an unclosed bracket (the OOM tripwire).
            if self.at_line_start && self.bracket_depth == 0 {
                match self.scan_indentation()? {
                    Some(layout) => {
                        // a real content line begins here; emit its Indent/Dedent(s) before its tokens
                        self.at_line_start = false;
                        self.pending.extend(layout);
                        if let Some(tok) = self.pending.pop_front() {
                            return Ok(tok);
                        }
                        // empty layout = same indent level → fall through and scan the first token
                    }
                    None => {
                        // only blank/comment lines remain, then EOF. Leave at_line_start = true;
                        // STEP C closes the file out (trailing Dedents + Eof).
                    }
                }
            }

            // === STEP B: skip inline whitespace and `# comments` (NOT newlines) ===
            loop {
                match self.peek() {
                    ' ' | '\t' | '\r' => {
                        self.advance();
                    }
                    '#' => {
                        if self.hole_base > 0 && self.hole_comment.is_none() {
                            self.hole_comment = Some(self.span_at(self.pos));
                        }
                        // comment runs to end of line; leave the '\n' for the newline rule below
                        while !self.is_at_end() && self.peek() != '\n' {
                            self.advance();
                        }
                    }
                    _ => break,
                }
            }

            // === STEP C: end of input ===
            if self.is_at_end() {
                let span = self.span_at(self.pos);
                // 1. close the final logical line with a Newline (once)...
                if !self.at_line_start {
                    self.at_line_start = true;
                    return Ok(Tok {
                        kind: Token::Newline,
                        span,
                        indent: None,
                    });
                }
                // 2. ...then emit one Dedent per still-open indent level...
                if *self.indents.last().unwrap() > 0 {
                    self.indents.pop();
                    return Ok(Tok {
                        kind: Token::Dedent,
                        span,
                        indent: None,
                    });
                }
                // 3. ...and finally Eof.
                return Ok(Tok {
                    kind: Token::Eof,
                    span,
                    indent: None,
                });
            }

            // === STEP D: newline ===
            // (Blank/comment-only lines never reach here — STEP A's scan_indentation skips them,
            // so when we see '\n' we're always ending a line that had real content.)
            if self.peek() == '\n' {
                let span = self.span_at(self.pos);
                // CRITICAL forward-progress: always advance past '\n' so the loop cannot spin.
                self.advance();
                self.line += 1;
                self.line_start = self.pos;
                if self.bracket_depth > 0 {
                    // Inside a multi-line literal: keep spans honest (line/line_start bumped above)
                    // but suppress the Newline. `continue` (not recurse) so thousands of blank lines
                    // inside a bracket can't overflow the stack; `at_line_start` stays false so layout
                    // is frozen. Forward progress is guaranteed: we already advance()d past '\n'.
                    continue 'scan;
                }
                if self.pipe_continues_next_line() {
                    // Leading-`|>` line continuation: the next content line starts with `|>`, so it
                    // continues THIS logical line — suppress the Newline and (by leaving
                    // `at_line_start` false) its Indent/Dedent too, exactly like the bracket case.
                    // Forward progress: we already advance()d past '\n'.
                    continue 'scan;
                }
                self.at_line_start = true;
                return Ok(Tok {
                    kind: Token::Newline,
                    span,
                    indent: None,
                });
            }

            // === STEP E: a real token starts here ===
            self.at_line_start = false;
            let start = self.pos; // index of the first char of this lexeme
            let span = self.span_at(start);
            // the first token of a physical line with spaces-only leading whitespace carries its width
            let lead = &self.chars[self.line_start..start];
            let indent = lead.iter().all(|&c| c == ' ').then_some(lead.len() as u32);
            let c = self.advance();
            let kind = match c {
                // single- and two-char operators (LEARN: match_char does the 2-char lookahead)
                '+' => {
                    if self.match_char('=') {
                        Token::PlusEq
                    } else {
                        Token::Plus
                    }
                }
                '-' => {
                    if self.match_char('>') {
                        Token::Arrow
                    } else if self.match_char('=') {
                        Token::MinusEq
                    } else {
                        Token::Minus
                    }
                }
                '*' => {
                    if self.match_char('=') {
                        Token::StarEq
                    } else {
                        Token::Star
                    }
                }
                '/' => {
                    if self.match_char('=') {
                        Token::SlashEq
                    } else {
                        Token::Slash
                    }
                }
                '%' => {
                    if self.match_char('=') {
                        Token::PercentEq
                    } else {
                        Token::Percent
                    }
                }
                ':' => {
                    // At hole depth a `:` is the format-spec separator: `"{x:=5}"` is spec `=5`.
                    let spec_colon = self.hole_base > 0 && self.bracket_depth == self.hole_base;
                    if !spec_colon && self.match_char('=') {
                        Token::Walrus
                    } else {
                        Token::Colon
                    }
                }
                '=' => {
                    if self.match_char('=') {
                        Token::EqEq
                    } else {
                        Token::Assign
                    }
                }
                '!' => {
                    if self.match_char('=') {
                        Token::NotEq
                    } else {
                        Token::Bang
                    }
                }
                '<' => {
                    if self.match_char('=') {
                        Token::LtEq
                    } else if self.match_char('<') {
                        // `<<` then an optional `=` → `<<=`.
                        if self.match_char('=') {
                            Token::ShlEq
                        } else {
                            Token::Shl
                        }
                    } else {
                        Token::Lt
                    }
                }
                '>' => {
                    if self.match_char('=') {
                        Token::GtEq
                    } else if self.match_char('>') {
                        if self.match_char('=') {
                            Token::ShrEq
                        } else {
                            Token::Shr
                        }
                    } else {
                        Token::Gt
                    }
                }
                '|' => {
                    if self.match_char('>') {
                        Token::Pipe
                    } else if self.match_char('=') {
                        Token::PipeEq
                    } else {
                        Token::BitOr
                    }
                }
                '&' => {
                    if self.match_char('=') {
                        Token::AmpEq
                    } else {
                        Token::Amp
                    }
                }
                '^' => {
                    if self.match_char('=') {
                        Token::CaretEq
                    } else {
                        Token::Caret
                    }
                }
                '?' => self.question(start)?,
                // Openers/closers also drive `bracket_depth` (multi-line literal layout suppression).
                '(' => {
                    self.bracket_depth += 1;
                    Token::LParen
                }
                '[' => {
                    self.bracket_depth += 1;
                    Token::LBracket
                }
                // `{` in token position is always a map/set literal — Chezzi blocks are
                // `: NEWLINE INDENT`, never brace-delimited; interpolation `{}` lives inside
                // Token::Str content and never reaches here.
                '{' => {
                    self.bracket_depth += 1;
                    Token::LBrace
                }
                // saturating_sub clamps a stray closer at 0 (no LexError; the parser reports the
                // unmatched closer).
                ')' => {
                    self.bracket_depth = self.bracket_depth.saturating_sub(1);
                    Token::RParen
                }
                ']' => {
                    self.bracket_depth = self.bracket_depth.saturating_sub(1);
                    Token::RBracket
                }
                '}' => {
                    self.bracket_depth = self.bracket_depth.saturating_sub(1);
                    Token::RBrace
                }
                ',' => Token::Comma,
                '.' => {
                    if self.match_char('.') {
                        if self.match_char('.') {
                            Token::DotDotDot
                        } else if self.match_char('=') {
                            Token::DotDotEq
                        } else {
                            Token::DotDot
                        }
                    } else {
                        Token::Dot
                    }
                }

                // delegate the "munching" token kinds to the helpers below. A triple of the same
                // quote char (`"""` / `'''`) opens a *triple-quoted* string in which a lone quote is
                // an ordinary char (same escapes + interpolation as a regular string; only unescaped
                // quotes differ). Detect it before the single-quote path by peeking two ahead.
                '"' | '\'' if self.peek() == c && self.peek_next() == c => {
                    self.advance(); // second quote
                    self.advance(); // third quote
                    self.quoted(c, true)?
                }
                '"' => self.quoted('"', false)?,
                '\'' => self.quoted('\'', false)?,
                // `b"..."` / `b'...'` byte-string literal — fire ONLY when the `b`/`B` is immediately
                // followed by a quote (mirrors how `number()` detects the `0x` radix prefix). A bare
                // `b` (e.g. `b + 1`, `by = 2`) falls through to `identifier()` unchanged.
                'b' | 'B' if matches!(self.peek(), '"' | '\'') => {
                    let quote = self.advance(); // consume the opening quote
                    if self.peek() == quote && self.peek_next() == quote {
                        self.advance(); // second quote
                        self.advance(); // third quote
                        self.byte_triple_string(quote)?
                    } else {
                        self.byte_string(quote)?
                    }
                }
                // `r"..."` / `r'...'` raw-string literal — verbatim, no interpolation, no escapes.
                // Fires ONLY when the `r`/`R` is immediately followed by a quote (mirrors the `b`
                // byte-string trigger above). A bare `r` (e.g. `r := 5`, `rx + 1`) falls through to
                // `identifier()` because the guard requires an adjacent quote.
                'r' | 'R' if matches!(self.peek(), '"' | '\'') => {
                    let quote = self.advance(); // consume the opening quote
                    if self.peek() == quote && self.peek_next() == quote {
                        self.advance(); // second quote
                        self.advance(); // third quote
                        self.raw_triple_string(quote)?
                    } else {
                        self.raw_string(quote)?
                    }
                }
                c if c.is_ascii_digit() => self.number(start)?,
                c if c.is_alphabetic() || c == '_' => self.identifier(start),

                // Hole text is code, and code has no escapes: the quote of a nested literal is
                // written directly (`"{d["k"]}"`), never escaped.
                '\\' if self.hole_base > 0 => {
                    return Err(self.error_at(
                        start,
                        "a backslash is not allowed in interpolation hole code; write the quote directly, as in \"{d[\"k\"]}\"",
                    ));
                }
                // `start`, not the cursor: the char was consumed at the top of this match.
                other => {
                    return Err(self.error_at(start, &format!("unexpected character {other:?}")));
                }
            };
            return Ok(Tok { kind, span, indent });
        }
    }

    /// Build a `LexError` at the CURRENT cursor. Use it only where the cursor still sits on the
    /// offending character; everywhere the scanner has already munched past it, use [`error_at`]
    /// (or [`error_span`]) with the real position.
    ///
    /// [`error_at`]: Lexer::error_at
    /// [`error_span`]: Lexer::error_span
    fn error(&self, message: &str) -> LexError {
        self.error_at(self.pos, message)
    }

    /// Build a `LexError` pointing at char index `pos` **on the current line**.
    ///
    /// It asks `span_at` rather than reading `self.line`/`self.line_start`, so every error site
    /// derives its position the same way. A hole is lexed in place, by this lexer, so a column
    /// inside a hole is the real one with no further composition.
    ///
    /// **`pos` must not predate `self.line_start`** — `span_at` derives the column as
    /// `pos - self.line_start`, so a position on an EARLIER line underflows. When the offending
    /// position can be on an earlier line (an unterminated literal, whose opener is often lines
    /// back), capture its `Span` at scan-entry and use [`error_span`] instead.
    ///
    /// [`error_span`]: Lexer::error_span
    fn error_at(&self, pos: usize, message: &str) -> LexError {
        self.error_span(self.span_at(pos), message)
    }

    /// The sole `LexError` constructor. `span` is the position the diagnostic points at, and the
    /// rule for choosing it is: **point at the offending character; an unterminated delimiter
    /// points at its opener** (rustc/CPython — see the `lex_error_points_at_the_offending_character`
    /// test for the measured table). Never invent a position: if it is genuinely unknown, report
    /// the token's start, never a filler `1`.
    fn error_span(&self, span: Span, message: &str) -> LexError {
        LexError {
            line: span.line as usize,
            col: span.col as usize,
            message: message.to_string(),
        }
    }

    /// (1h) Handle the indentation at the start of a logical line — the heart of an
    /// indentation-sensitive lexer.
    ///
    /// Consumes the leading spaces of the upcoming line, skipping blank lines (empty, or only
    /// spaces and tabs) and comment-only lines (which never affect indentation). Then compares the line's indent width against the indent
    /// stack `self.indents` and returns the layout tokens to emit:
    ///   - `Ok(Some(vec![Indent]))`      — deeper than before (one level opened)
    ///   - `Ok(Some(vec![Dedent, ...]))` — shallower (one Dedent per level closed)
    ///   - `Ok(Some(vec![]))`            — same level (nothing to emit)
    ///   - `Ok(None)`                    — no content line remains (only blanks/comments + EOF);
    ///     caller falls through to end-of-input handling
    ///
    /// On return for a content line, the cursor sits at that line's first non-space char.
    fn scan_indentation(&mut self) -> Result<Option<Vec<Tok>>, LexError> {
        loop {
            // 0. U14: a line that holds only spaces or tabs, or only them and then a comment, never
            //    affects indentation (CPython). Consume its whitespace, so step 2, 3 or 4 skips it
            //    before the tab check. A tab before code stays an error.
            let ws = self.chars[self.pos..]
                .iter()
                .take_while(|c| matches!(c, ' ' | '\t'))
                .count();
            if matches!(self.chars.get(self.pos + ws), None | Some('\n') | Some('#')) {
                self.pos += ws;
            }
            // 1. measure indentation by consuming leading spaces
            let mut width = 0;
            while self.peek() == ' ' {
                self.advance();
                width += 1;
            }
            // spaces only — a tab in the indentation is an error (avoids the classic tab/space mess)
            if self.peek() == '\t' {
                return Err(self.error("tabs are not allowed for indentation — use spaces"));
            }

            // 2. blank line (only whitespace) → does not count, skip it
            if self.peek() == '\n' {
                self.advance();
                self.line += 1;
                self.line_start = self.pos;
                continue;
            }
            // 3. comment-only line → does not count, skip to its newline. We DO capture its text
            //    in the doc-comment side-channel (line + stripped body) so the parser can attach a
            //    contiguous run of comment lines above a declaration as its doc. `self.line` here is
            //    this comment's line: a comment line never bumps `self.line` (only the blank-line
            //    branch above, when it later consumes the trailing `\n`, does), so it's still correct.
            if self.peek() == '#' {
                let hash = self.pos; // index of '#'
                while !self.is_at_end() && self.peek() != '\n' {
                    self.advance();
                }
                // body = chars after '#', with at most one leading space stripped (so `# foo` → `foo`).
                let mut body_start = hash + 1;
                if self.chars.get(body_start) == Some(&' ') {
                    body_start += 1;
                }
                let body: String = self.chars[body_start..self.pos].iter().collect();
                self.doc_comments.push((self.line, body));
                continue;
            }
            // 4. end of input → no content line; let the caller close out
            if self.is_at_end() {
                return Ok(None);
            }

            // 5. a real content line at column `width` — compare against the indent stack
            let span = self.span_at(self.pos);
            return match indent_step(&mut self.indents, width) {
                Ok(layout) => Ok(Some(
                    layout
                        .into_iter()
                        .map(|kind| Tok {
                            kind,
                            span,
                            indent: None,
                        })
                        .collect(),
                )),
                Err(msg) => Err(self.error(msg)),
            };
        }
    }

    // ----- scanning helpers -----

    /// (1e) Scan an identifier or keyword. The first char is already consumed; `start` is its
    /// index in `self.chars`.
    /// HINT: munch (advance) while `self.peek()` is alphanumeric or `_`. Then build the word with
    /// `self.chars[start..self.pos].iter().collect::<String>()`, and return
    /// `keyword(&word).unwrap_or(Token::Ident(word))`.
    fn identifier(&mut self, start: usize) -> Token {
        while self.peek().is_alphanumeric() || self.peek() == '_' {
            self.advance();
        }

        let identifier: String = self.chars[start..self.pos].iter().collect();
        match keyword(&identifier) {
            Some(tok) => tok,
            None => Token::Ident(identifier),
        }
    }

    /// (1c) Scan a number literal (int, or float if it has a single '.'). First digit already
    /// consumed; `start` is its index. `_` is allowed as a digit-group separator
    /// (`10_000_000`) but only **between two digits** — leading/trailing/doubled/dot-adjacent
    /// underscores are a `LexError`.
    fn number(&mut self, start: usize) -> Result<Token, LexError> {
        // Radix-prefixed integer literals: `0x`/`0X` (hex), `0b`/`0B` (binary), `0o`/`0O` (octal).
        // The leading `0` is already consumed; `start` indexes it and the cursor sits on the marker.
        // A `.` after the body is postfix (field/range), never a fraction — so no float path here.
        if self.chars[start] == '0' {
            let (radix, name) = match self.peek() {
                'x' | 'X' => (16, "hexadecimal"),
                'b' | 'B' => (2, "binary"),
                'o' | 'O' => (8, "octal"),
                _ => (0, ""),
            };
            if radix != 0 {
                self.advance(); // consume the radix marker
                let body_start = self.pos;
                while self.peek().is_ascii_alphanumeric() || self.peek() == '_' {
                    self.advance();
                }
                let body: Vec<char> = self.chars[body_start..self.pos].to_vec();
                if body.is_empty() {
                    // The whole `0x` is consumed → anchor on the literal's first char.
                    return Err(self.error_at(start, &format!("empty {name} literal")));
                }
                // Underscores: only between two valid digits (mirrors decimal rule).
                let is_digit = |c: char| c.is_digit(radix);
                for (i, &c) in body.iter().enumerate() {
                    if c == '_' {
                        // `i == 0`: the radix marker (`0x`) precedes `body[0]`, so ONE underscore right
                        // after the prefix is legal (`0x_ff`, PEP 515); `next_ok` still rejects `0x_`
                        // and `0x__ff`.
                        let prev_ok = i == 0 || is_digit(body[i - 1]);
                        let next_ok = body.get(i + 1).is_some_and(|n| is_digit(*n));
                        if !(prev_ok && next_ok) {
                            // The offending `_` itself: body index `i` is char `body_start + i`.
                            return Err(self.error_at(
                                body_start + i,
                                "'_' in a number must be between digits",
                            ));
                        }
                    }
                }
                let digits: String = body.into_iter().filter(|c| *c != '_').collect();
                let v = i64::from_str_radix(&digits, radix)
                    .map_err(|e| self.error_at(start, &format!("invalid {name} literal: {e}")))?;
                return Ok(Token::Int(v));
            }
        }

        let mut is_float = false;

        // A number right after a single `.` is a tuple-element index (`t.0.1`, Rust's split of the
        // `0.1` token): it never takes a fraction or an exponent. `0..1.5` keeps its float (`..`).
        let field_index = start >= 1
            && self.chars[start - 1] == '.'
            && (start < 2 || self.chars[start - 2] != '.');

        // integer part (digits + group separators)
        while self.peek().is_ascii_digit() || self.peek() == '_' {
            self.advance();
        }

        // fractional part — ONLY if the dot is followed by a digit
        // (so `1.` keeps its dot, and `0..10` is not eaten as a number)
        if !field_index && self.peek() == '.' && self.peek_next().is_ascii_digit() {
            is_float = true;
            self.advance(); // consume the '.'
            while self.peek().is_ascii_digit() || self.peek() == '_' {
                self.advance();
            }
        }

        // exponent part — `e`/`E` then an optional sign then one-or-more ascii digits.
        // Peek-ahead-before-commit so a bare `e` (e.g. `1e`, `1e+`) is never half-consumed:
        // only advance once we know a full, valid exponent follows. Any number with an
        // exponent is a float (even `1e3` → 1000.0). No underscores in the exponent.
        if !field_index && (self.peek() == 'e' || self.peek() == 'E') {
            // index (relative to the cursor) of the first exponent digit candidate
            let mut probe = self.pos + 1;
            if matches!(self.chars.get(probe), Some('+') | Some('-')) {
                probe += 1;
            }
            if self.chars.get(probe).is_some_and(|c| c.is_ascii_digit()) {
                is_float = true;
                self.advance(); // consume 'e'/'E'
                if matches!(self.peek(), '+' | '-') {
                    self.advance(); // consume the sign
                }
                while self.peek().is_ascii_digit() {
                    self.advance();
                }
            }
            // else: leave the 'e' for identifier()/next_token — no error (matches Python/Rust).
        }

        let word: Vec<char> = self.chars[start..self.pos].to_vec();
        // Validate underscores over the MANTISSA only — the exponent (its sign + digits) is
        // underscore-free by construction, and a '+'/'-' sign there must not trip this check.
        let mantissa_end = word
            .iter()
            .position(|&c| c == 'e' || c == 'E')
            .unwrap_or(word.len());
        // Validate underscores: every '_' must be flanked by a digit on both sides.
        for (i, &c) in word[..mantissa_end].iter().enumerate() {
            if c == '_' {
                let prev_ok = i > 0 && word[i - 1].is_ascii_digit();
                let next_ok = word.get(i + 1).is_some_and(|n| n.is_ascii_digit());
                if !(prev_ok && next_ok) {
                    // The offending `_` itself: `word` is `chars[start..pos]`, so index `i` is
                    // char `start + i`.
                    return Err(self.error_at(start + i, "'_' in a number must be between digits"));
                }
            }
        }

        let num: String = word.into_iter().filter(|c| *c != '_').collect();
        if !is_float
            && num.len() > 1
            && num.starts_with('0')
            && num.chars().any(|c| ('1'..='9').contains(&c))
        {
            let trimmed = num.trim_start_matches('0');
            let msg = if num.chars().all(|c| ('0'..='7').contains(&c)) {
                format!(
                    "leading zeros in a decimal integer literal are not permitted; write 0o{trimmed} for octal or {trimmed} for decimal"
                )
            } else {
                format!(
                    "leading zeros in a decimal integer literal are not permitted; write {trimmed} for decimal"
                )
            };
            return Err(self.error_at(start, &msg));
        }
        if is_float {
            let v = num
                .parse::<f64>()
                .map_err(|e| self.error_at(start, &e.to_string()))?;
            Ok(Token::Float(v))
        } else {
            match num.parse::<i64>() {
                Ok(v) => Ok(Token::Int(v)),
                // `9223372036854775808` (== i64::MAX + 1 == -i64::MIN) overflows i64 as a positive
                // value, but it is the magnitude of i64::MIN. Emit a distinct token so the parser can
                // fold a leading unary minus into `Int(i64::MIN)`; a bare occurrence errors there.
                // Any larger magnitude is unrepresentable even negated, so keep the original error.
                Err(e) => {
                    if num.parse::<u64>() == Ok(9_223_372_036_854_775_808) {
                        Ok(Token::IntMinMagnitude)
                    } else {
                        // The whole literal is consumed → anchor on its first char.
                        Err(self.error_at(start, &e.to_string()))
                    }
                }
            }
        }
    }

    /// True when the cursor sits on the literal's closing delimiter: the `quote`, or three of it
    /// for a triple-quoted literal.
    fn at_close(&self, quote: char, triple: bool) -> bool {
        self.peek() == quote
            && (!triple
                || (self.peek_next() == quote && self.chars.get(self.pos + 2) == Some(&quote)))
    }

    /// Consume one escape and return the char it stands for. The cursor sits on the backslash.
    /// This is the ONLY reader of an escape: a literal's text and a format spec's text both call it,
    /// and hole code never does (a backslash in hole code is an error, see `next_token`). `open` is
    /// the literal's opener, which a trailing backslash reports.
    fn escape(&mut self, open: Span) -> Result<char, LexError> {
        self.advance(); // consume the backslash
        if self.is_at_end() {
            return Err(self.error_span(open, "unterminated string literal (trailing '\\')"));
        }
        let esc = self.advance();
        // `\u{HEX}` — 1-6 hex digits naming a Unicode scalar value.
        if esc == 'u' {
            return self.unicode_escape();
        }
        if let Some((_, resolved)) = ESCAPES.iter().find(|(letter, _)| *letter == esc) {
            return Ok(*resolved);
        }
        if esc == '\n' || esc == '\r' {
            // The `\` (2 back: the backslash and the newline are both consumed).
            return Err(self.error_at(
                self.pos - 2,
                "line continuations are not supported; close the string or use \\n",
            ));
        }
        // The escape CHAR, not the `\` before it (rustc points here).
        Err(self.error_at(self.pos - 1, &format!("unknown escape '\\{esc}'")))
    }

    /// Scan a `str` literal. The opening delimiter (`quote`, or three of it when `triple`) is
    /// already consumed. This is the ONE owner of "what did the user write in this hole, and where
    /// does it end": it reads the literal's text (escapes resolved once, by [`escape`]), and on a
    /// `{` it lexes the hole's code in place with [`hole`], so the hole's text is never read again.
    ///
    /// * `{{` / `}}` are one literal brace; a lone `}` is an error.
    /// * `{` + ASCII digits + `}` is text as written (DEC-018): a regex quantifier `\d{4}` stays.
    /// * An unterminated literal points at its OPENER, which may be lines behind the cursor, so the
    ///   span is captured up front.
    ///
    /// [`escape`]: Lexer::escape
    /// [`hole`]: Lexer::hole
    fn quoted(&mut self, quote: char, triple: bool) -> Result<Token, LexError> {
        let width = if triple { 3 } else { 1 };
        let start = self.pos - width;
        let open = self.span_at(start);
        let mut parts: Vec<StrPart> = Vec::new();
        let mut text = String::new();
        loop {
            if self.is_at_end() {
                let message = if triple {
                    "unterminated triple-quoted string literal"
                } else {
                    "unterminated string literal"
                };
                return Err(self.error_span(open, message));
            }
            if self.at_close(quote, triple) {
                break;
            }
            let c = self.peek();
            if c == '\\' {
                text.push(self.escape(open)?);
            } else if (c == '{' || c == '}') && self.peek_next() == c {
                self.advance();
                self.advance();
                text.push(c);
            } else if c == '{' {
                if let Some(end) = self.digit_hole() {
                    text.extend(&self.chars[self.pos..=end]);
                    self.pos = end + 1;
                } else {
                    if !text.is_empty() {
                        parts.push(StrPart::Text(std::mem::take(&mut text)));
                    }
                    parts.push(StrPart::Hole(self.hole(quote, triple)?));
                }
            } else if c == '}' {
                return Err(self.error("unmatched '}' in string (use '}}' for a literal brace)"));
            } else {
                if c == '\n' {
                    self.line += 1; // a *literal* newline → multi-line string; keep line count honest
                    self.line_start = self.pos + 1; // next char begins the new line
                }
                text.push(self.advance());
            }
        }
        for _ in 0..width {
            self.advance(); // the closing delimiter
        }
        if !text.is_empty() {
            parts.push(StrPart::Text(text));
        }
        Ok(Token::Str(StrLit {
            parts,
            len: self.pos - start,
        }))
    }

    /// The index of the `}` when the cursor sits on `{` + one or more ASCII digits + `}` (a digit
    /// hole, DEC-018), else `None`.
    fn digit_hole(&self) -> Option<usize> {
        let mut i = self.pos + 1;
        while self.chars.get(i).is_some_and(char::is_ascii_digit) {
            i += 1;
        }
        (i > self.pos + 1 && self.chars.get(i) == Some(&'}')).then_some(i)
    }

    /// The error for a hole that ran to the end of the input. When the hole held a `#`, the comment
    /// swallowed the rest of its line, so that is the diagnosis; otherwise the `{` is unclosed.
    fn unterminated_hole(&self, brace: Span) -> LexError {
        match self.hole_comment {
            Some(hash) => self.error_span(
                hash,
                "'#' starts a comment inside an interpolation hole and the comment runs to the end of the line, so this hole's closing '}' is commented out",
            ),
            None => self.error_span(brace, "unterminated '{' in interpolated string"),
        }
    }

    /// Scan one hole. The cursor sits on its `{`; the literal's delimiter is `quote` / `triple`.
    /// Returns with the cursor past the hole's closing `}`.
    fn hole(&mut self, quote: char, triple: bool) -> Result<Hole, LexError> {
        let brace = self.span_at(self.pos);
        self.advance(); // the `{`
        if self.hole_nest >= MAX_HOLE_NEST {
            return Err(self.error_span(brace, "too many nested interpolated strings"));
        }
        self.hole_nest += 1;
        let outer_comment = self.hole_comment.take();
        let result = self.hole_body(brace, quote, triple);
        self.hole_nest -= 1;
        self.hole_comment = outer_comment;
        result
    }

    /// The body of [`hole`]: the code, then the spec when the code ended on a `:`.
    ///
    /// [`hole`]: Lexer::hole
    fn hole_body(&mut self, brace: Span, quote: char, triple: bool) -> Result<Hole, LexError> {
        let (tokens, has_spec) = self.hole_code(brace)?;
        if !has_spec {
            return Ok(Hole {
                tokens,
                spec: None,
                spec_at: brace,
            });
        }
        // Past the first `:` at hole depth the rest is the FORMAT SPEC: literal text, so a quote,
        // a bracket or a `#` is a fill char (`"{x:'>5}"`, `"{x:(>5}"`). Only a `{` (a nested
        // field), a backslash (an escape) and the closing `}` mean anything. The literal's own
        // delimiter ends the literal, so it ends the spec too.
        let spec_at = self.span_at(self.pos);
        let mut spec: Vec<SpecPart> = Vec::new();
        let mut text = String::new();
        loop {
            if self.is_at_end() || self.at_close(quote, triple) {
                return Err(self.unterminated_hole(brace));
            }
            let c = self.peek();
            if c == '}' {
                self.advance();
                break;
            } else if c == '{' {
                if !text.is_empty() {
                    spec.push(SpecPart::Text(std::mem::take(&mut text)));
                }
                let field = self.span_at(self.pos);
                self.advance();
                let (field_tokens, nested_spec) = self.hole_code(field)?;
                if nested_spec {
                    return Err(self.error_span(
                        field,
                        "format spec: a nested field cannot carry its own format spec",
                    ));
                }
                spec.push(SpecPart::Field(field_tokens));
            } else if c == '\\' {
                text.push(self.escape(brace)?);
            } else {
                if c == '\n' {
                    self.line += 1;
                    self.line_start = self.pos + 1;
                }
                text.push(self.advance());
            }
        }
        if !text.is_empty() {
            spec.push(SpecPart::Text(text));
        }
        Ok(Hole {
            tokens,
            spec: Some(spec),
            spec_at,
        })
    }

    /// Lex a hole's code in place. The cursor sits just past the `{` (`brace`). The hole counts as
    /// one bracket level, like `()`, `[]` and `{}`, so layout is off inside it: a line break is
    /// white space, never a `Newline`. Returns the tokens (ending in `Eof`) and whether the code
    /// ended on the first `:` at hole depth (the spec separator) instead of on the closing `}`.
    /// Either way the cursor sits past the token that ended the code.
    fn hole_code(&mut self, brace: Span) -> Result<(Vec<Tok>, bool), LexError> {
        self.bracket_depth += 1;
        let base = self.bracket_depth;
        let outer_base = std::mem::replace(&mut self.hole_base, base);
        // A hole's code starts a new expression: no operand sits before its first token.
        self.after_operand = false;
        let result = self.hole_tokens(brace, base);
        self.hole_base = outer_base;
        result
    }

    fn hole_tokens(&mut self, brace: Span, base: usize) -> Result<(Vec<Tok>, bool), LexError> {
        let mut tokens = Vec::new();
        loop {
            let tok = match self.next_token() {
                Ok(tok) => tok,
                // A nested literal that ran to the end of the input: the hole never closed.
                Err(_) if self.is_at_end() => return Err(self.unterminated_hole(brace)),
                Err(e) => return Err(e),
            };
            let kind = &tok.kind;
            // Layout tokens appear only when the input ends (layout is off inside the hole).
            if *kind == Token::Eof
                || *kind == Token::Newline
                || *kind == Token::Indent
                || *kind == Token::Dedent
            {
                return Err(self.unterminated_hole(brace));
            }
            let closed_hole = self.bracket_depth < base;
            // The `Eof` that ends the tokens sits where the code ended: on the `}` or the `:`.
            if closed_hole && *kind == Token::RBrace {
                tokens.push(Tok {
                    kind: Token::Eof,
                    span: tok.span,
                    indent: None,
                });
                return Ok((tokens, false));
            }
            if closed_hole && (*kind == Token::RParen || *kind == Token::RBracket) {
                return Err(self.error_span(
                    tok.span,
                    "unmatched closing bracket in an interpolation hole",
                ));
            }
            if *kind == Token::Colon && self.bracket_depth == base {
                self.bracket_depth -= 1;
                tokens.push(Tok {
                    kind: Token::Eof,
                    span: tok.span,
                    indent: None,
                });
                return Ok((tokens, true));
            }
            tokens.push(tok);
        }
    }

    /// Scan an `r"..."` / `r'...'` raw-string literal. The `r`/`R` prefix and the opening `quote`
    /// are already consumed. Identical to [`string`] EXCEPT every char is pushed verbatim — there is
    /// NO backslash-escape branch, so `\n` is two chars (backslash, n), `\` is a literal backslash,
    /// and a brace `{`/`}` is an ordinary char (the later interpolation pass never sees `RawStr`).
    /// The short form cannot contain the closing `quote` (no escaping — use the other quote style or
    /// the triple form). Produces a [`Token::RawStr`].
    fn raw_string(&mut self, quote: char) -> Result<Token, LexError> {
        let mut text = String::new();
        // The opener is `r"` — 2 chars, both already consumed. See `string()`.
        let open = self.span_at(self.pos - 2);
        while !self.is_at_end() && self.peek() != quote {
            if self.peek() == '\n' {
                self.line += 1; // a *literal* newline → multi-line string; keep line count honest
                self.line_start = self.pos + 1; // next char begins the new line
            }
            text.push(self.advance());
        }
        if self.is_at_end() {
            return Err(self.error_span(open, "unterminated raw string literal"));
        }
        self.advance(); // consume the closing quote
        Ok(Token::RawStr(text))
    }

    /// Scan a *triple-quoted* raw-string literal (`r"""…"""` / `r'''…'''`). The `r`/`R` prefix and
    /// the opening triple `quote` are already consumed. Like [`triple_string`] but verbatim: no
    /// escapes, no interpolation; a single/double `quote` inside is an ordinary char, so this is how
    /// quote-heavy data (e.g. JSON) is embedded. Closes only on the next triple `quote`.
    fn raw_triple_string(&mut self, quote: char) -> Result<Token, LexError> {
        let mut text = String::new();
        // The opener is `r"""` — 4 chars, all already consumed. See `string()`.
        let open = self.span_at(self.pos - 4);
        // Closes only when the next THREE chars are all `quote`.
        while !(self.peek() == quote
            && self.peek_next() == quote
            && self.chars.get(self.pos + 2) == Some(&quote))
        {
            if self.is_at_end() {
                return Err(self.error_span(open, "unterminated triple-quoted raw string literal"));
            }
            if self.peek() == '\n' {
                self.line += 1; // keep the line count honest across embedded newlines
                self.line_start = self.pos + 1;
            }
            text.push(self.advance());
        }
        // consume the closing triple
        self.advance();
        self.advance();
        self.advance();
        Ok(Token::RawStr(text))
    }

    /// Scan a `b"..."` / `b'...'` byte-string literal. The `b`/`B` prefix and the opening `quote`
    /// are already consumed. Produces a [`Token::Bytes`] holding the resolved raw bytes.
    ///
    /// Escapes mirror [`string`] but push BYTES, not chars: `\n \t \r \\ \" \' \0` push their ASCII
    /// byte, and `\xHH` (exactly two hex digits) pushes one byte `0x00..=0xFF` (Python parity — this
    /// is the only way to encode a byte ≥ 0x80). A `\u{...}` escape and any raw non-ASCII source
    /// char (a code point > 0x7F) are REJECTED — a byte literal is byte-exact, never UTF-8 text.
    fn byte_string(&mut self, quote: char) -> Result<Token, LexError> {
        let mut bytes = Vec::new();
        // The opener is `b"` — 2 chars, both already consumed. See `string()`.
        let open = self.span_at(self.pos - 2);
        while !self.is_at_end() && self.peek() != quote {
            if self.peek() == '\\' {
                self.byte_escape(&mut bytes, open)?;
            } else {
                let c = self.advance();
                if c == '\n' {
                    self.line += 1;
                    self.line_start = self.pos;
                }
                self.push_raw_byte(&mut bytes, c)?;
            }
        }
        if self.is_at_end() {
            return Err(self.error_span(open, "unterminated byte-string literal"));
        }
        self.advance(); // closing quote
        Ok(Token::Bytes(bytes))
    }

    /// Triple-quoted byte literal (`b"""…"""` / `b'''…'''`). Same escapes as [`byte_string`]; closes
    /// only on a triple of `quote`, so a lone quote inside is an ordinary byte.
    fn byte_triple_string(&mut self, quote: char) -> Result<Token, LexError> {
        let mut bytes = Vec::new();
        // The opener is `b"""` — 4 chars, all already consumed. See `string()`.
        let open = self.span_at(self.pos - 4);
        while !(self.peek() == quote
            && self.peek_next() == quote
            && self.chars.get(self.pos + 2) == Some(&quote))
        {
            if self.is_at_end() {
                return Err(self.error_span(open, "unterminated triple-quoted byte-string literal"));
            }
            if self.peek() == '\\' {
                self.byte_escape(&mut bytes, open)?;
            } else {
                let c = self.advance();
                if c == '\n' {
                    self.line += 1;
                    self.line_start = self.pos;
                }
                self.push_raw_byte(&mut bytes, c)?;
            }
        }
        self.advance();
        self.advance();
        self.advance();
        Ok(Token::Bytes(bytes))
    }

    /// Push a *raw* (unescaped) source char into a byte literal. ASCII (≤ 0x7F) → its byte; a
    /// non-ASCII code point is rejected (CPython 3 parity — use `\xHH`).
    fn push_raw_byte(&self, bytes: &mut Vec<u8>, c: char) -> Result<(), LexError> {
        if (c as u32) <= 0x7F {
            bytes.push(c as u8);
            Ok(())
        } else {
            // `c` is already consumed by the caller, and it is non-ASCII so it is never a newline.
            Err(self.error_at(
                self.pos - 1,
                "non-ASCII byte in byte literal; use \\xHH escape",
            ))
        }
    }

    /// Process one backslash escape inside a byte literal. The cursor sits on the `\`; `open` is the
    /// enclosing literal's opening-delimiter span (only an unterminated literal points there).
    fn byte_escape(&mut self, bytes: &mut Vec<u8>, open: Span) -> Result<(), LexError> {
        self.advance(); // consume the backslash
        if self.is_at_end() {
            return Err(self.error_span(open, "unterminated byte-string literal (trailing '\\')"));
        }
        let esc = self.advance();
        let byte = match esc {
            'n' => b'\n',
            't' => b'\t',
            'r' => b'\r',
            '\\' => b'\\',
            '"' => b'"',
            '\'' => b'\'',
            '0' => 0,
            // `\xHH` — exactly two hex digits → one byte 0x00..=0xFF.
            'x' => {
                let hi = self.hex_digit("\\x")?;
                let lo = self.hex_digit("\\x")?;
                bytes.push((hi << 4) | lo);
                return Ok(());
            }
            // The `u`, not the `\` before it.
            'u' => {
                return Err(
                    self.error_at(self.pos - 1, "\\u not allowed in a byte literal; use \\xHH")
                );
            }
            // The `\` (2 back: the backslash and the newline are both consumed).
            '\n' | '\r' => {
                return Err(self.error_at(
                    self.pos - 2,
                    "line continuations are not supported; close the literal or use \\n",
                ));
            }
            // The escape CHAR, not the `\` before it.
            other => {
                return Err(self.error_at(self.pos - 1, &format!("unknown escape '\\{other}'")));
            }
        };
        bytes.push(byte);
        Ok(())
    }

    /// Read exactly one hex digit (for `\xHH`), returning its 0..=15 value. Errors on EOF or a
    /// non-hex char (`who` names the escape for the message).
    fn hex_digit(&mut self, who: &str) -> Result<u8, LexError> {
        if self.is_at_end() {
            return Err(self.error(&format!("{who} escape needs two hex digits")));
        }
        let c = self.advance();
        c.to_digit(16).map(|d| d as u8).ok_or_else(|| {
            // The bad digit itself — `advance` already stepped past it.
            self.error_at(
                self.pos - 1,
                &format!("invalid hex digit '{c}' in {who} escape"),
            )
        })
    }

    /// Scan the body of a `\u{HEX}` escape. The `\u` is already consumed; the cursor sits on
    /// what must be `{`. Reads 1-6 hex digits naming a Unicode scalar value and returns it.
    /// Rejects a missing `{`, an empty `{}`, more than 6 hex digits, any non-hex char, an
    /// unterminated brace, and invalid code points (surrogates D800-DFFF, > 10FFFF).
    fn unicode_escape(&mut self) -> Result<char, LexError> {
        // The `{` opens the escape: the errors that are about the escape AS A WHOLE point here.
        // `unicode_escape` never touches `self.line_start`, so this index stays on the current line.
        let brace = self.pos;
        if !self.match_char('{') {
            // The cursor still sits on the offending char (`match_char` declined).
            return Err(self.error("expected '{' after \\u in unicode escape"));
        }
        let mut digits = String::new();
        loop {
            if self.is_at_end() {
                return Err(self.error_at(brace, "unterminated unicode escape"));
            }
            let c = self.peek();
            if c == '}' {
                self.advance();
                break;
            }
            if !c.is_ascii_hexdigit() {
                return Err(self.error("invalid hex digit in unicode escape"));
            }
            if digits.len() == 6 {
                return Err(self.error("unicode escape too long (max 6 hex digits)"));
            }
            digits.push(self.advance());
        }
        // These three are about the escape as a whole (its body is already consumed) → the `{`.
        if digits.is_empty() {
            return Err(self.error_at(brace, "empty unicode escape"));
        }
        let cp = u32::from_str_radix(&digits, 16)
            .map_err(|e| self.error_at(brace, &format!("invalid unicode escape: {e}")))?;
        char::from_u32(cp).ok_or_else(|| self.error_at(brace, "invalid unicode code point"))
    }

    // Indentation (offside rule) — implemented in `scan_indentation`:
    //   Indent stack starts `vec![0]`. At the start of each logical (non-blank, non-comment-only)
    //   line: width = leading spaces (tabs rejected); width > top → push + one `Indent`; width < top
    //   → pop while top > width, one `Dedent` per pop (top != width after → "inconsistent dedent");
    //   width == top → nothing. Blank/comment-only lines emit no `Newline` and no indent change.
    //   At EOF: one `Dedent` per remaining level above 0, then `Eof`. One position can require
    //   several `Dedent`s, so a small queue field is drained before scanning more source.
    //   Indentation is suppressed inside bracket depth (`([{`).
}

/// Convenience free function: `lexer::tokenize(src)`.
pub fn tokenize(source: &str) -> Result<Vec<Tok>, LexError> {
    Lexer::new(source).tokenize()
}

/// TICKET-265 — call `each` with the line and the text of each identifier of `toks`, in token
/// order. The tokens of a string hole and of a spec field count; a keyword, a comment and string
/// text do not. The module graph reads it to find the std fns a program names.
pub fn ident_words(toks: &[Tok], each: &mut impl FnMut(u32, &str)) {
    each_token(toks, &mut |tok, _| {
        if let Token::Ident(word) = &tok.kind {
            each(tok.span.line, word);
        }
    });
}

/// Call `each` with each token of `toks` and the token after it in the same run, in token order.
/// The tokens of a string hole and of a spec field count, each hole its own run; a comment and
/// string text are no tokens.
pub fn each_token(toks: &[Tok], each: &mut impl FnMut(&Tok, Option<&Tok>)) {
    for (i, tok) in toks.iter().enumerate() {
        each(tok, toks.get(i + 1));
        if let Token::Str(lit) = &tok.kind {
            for part in &lit.parts {
                match part {
                    StrPart::Text(_) => {}
                    StrPart::Hole(hole) => {
                        each_token(&hole.tokens, each);
                        for spec in hole.spec.iter().flatten() {
                            match spec {
                                SpecPart::Text(_) => {}
                                SpecPart::Field(field) => each_token(field, each),
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The length in source chars of the plain `str` literal that starts `rest_of_line` (a `"` or `'`
/// is its first char), delimiters included, or `None` when the lexer cannot close it within
/// `rest_of_line`. The one answer to "where does this literal end": a caret width and an LSP token
/// extent ask here, so neither scans for a quote itself. It lexes ONE line, so a literal that runs
/// onto the next line is `None`, and it allows [`ONE_LINE_HOLE_NEST`] levels of nested holes: the
/// editor calls it on a small thread stack, where the full [`MAX_HOLE_NEST`] recursion is not safe.
pub fn literal_len(rest_of_line: &str) -> Option<usize> {
    let mut lexer = Lexer::new(rest_of_line);
    lexer.hole_nest = MAX_HOLE_NEST - ONE_LINE_HOLE_NEST;
    let tok = lexer.next_token().ok()?;
    if let Token::Str(s) = tok.kind {
        Some(s.len)
    } else {
        None
    }
}

/// Convenience free function: `lexer::tokenize_with_comments(src)` — tokens plus the doc-comment
/// side table, with every span stamped with the module id `file` ([`Span::file`]). The token stream
/// is identical to [`tokenize`] modulo that stamp. This is the graph lex seam (`resolver`).
pub fn tokenize_with_comments(
    source: &str,
    file: u32,
) -> Result<(Vec<Tok>, Vec<DocComment>), LexError> {
    Lexer::new_file(source, file).tokenize_with_comments()
}

/// The 0-based end column of the token starting at 1-based `(line, col)`. Four branches, tested in
/// this order: a STRING literal (a quote, or a `b`/`r` prefix directly before one — this must come
/// before the word test, since the prefix letter is a word char), a NUMBER literal, an identifier
/// word, and otherwise `col` (0-based) + 1. That last branch is also DEC-001's fallback for an
/// unreadable source (a one-shot fd read returns nothing, so the scan runs on an empty line).
/// Measures ONE line, so a triple-quoted literal stops at end of line. `pub`: also the
/// `--errors=json` `end_col` renderer (`main.rs::diags_json`) and the plain-text caret width
/// (`lexer::render_snippet`) reuse this rather than writing a second scanner — their `line`/`col`
/// are 1-based, so `diags_json` adds 1 to this fn's 0-based result. Never strips a BOM (DEC-058):
/// the outermost caller strips once.
pub fn word_end_col(source: &str, line1: usize, col1: usize) -> u32 {
    let col0 = col1.saturating_sub(1);
    let line = source.lines().nth(line1.saturating_sub(1)).unwrap_or("");
    let chars: Vec<char> = line.chars().collect();
    let at = |i: usize| chars.get(i).copied();
    let is_quote = |c: Option<char>| matches!(c, Some('"' | '\''));
    let end = if is_quote(at(col0)) {
        // A plain `str` literal may hold holes with nested literals of its own quote, so its
        // extent is the lexer's: the one place that knows where a hole ends. A literal the lexer
        // cannot close on this line (unterminated, or a hole that runs on) stops at end of line.
        let rest: String = chars[col0..].iter().collect();
        col0 + literal_len(&rest).unwrap_or(chars.len() - col0)
    } else if matches!(at(col0), Some('b' | 'B' | 'r' | 'R')) && is_quote(at(col0 + 1)) {
        col0 + measure_string(&chars, col0)
    } else if matches!(at(col0), Some(c) if c.is_ascii_digit()) {
        col0 + measure_number(&chars, col0)
    } else if matches!(at(col0), Some(c) if is_word(c)) {
        let mut end = col0;
        while matches!(at(end), Some(c) if is_word(c)) {
            end += 1;
        }
        end
    } else {
        col0 + 1
    };
    end as u32
}

/// Whether `t` can be the last token of an operand: a literal, a name or a closer. A `?` is
/// postfix exactly when an operand ends before it, so it keeps `before`, the reading of the token
/// before it: `x?` still ends an operand, a prefix `?` still starts one.
fn ends_operand(t: &Token, before: bool) -> bool {
    use Token::*;
    match t {
        Question => before,
        Int(_) | IntMinMagnitude | Float(_) | Str(_) | Bytes(_) | RawStr(_) | Ident(_) | True
        | False | NoneKw | RParen | RBracket | RBrace => true,
        Fn | Return | If | Else | Elif | For | While | In | Break | Continue | Pass | Struct
        | Enum | Protocol | Type | Match | Recover | Defer | Assert | Test | Spawn | Parallel
        | Wait | Yield | Import | Extern | Native | From | As | Const | And | Or | Not | Where
        | Plus | Minus | Star | Slash | Percent | Assign | Walrus | EqEq | NotEq | Lt | LtEq
        | Gt | GtEq | PlusEq | MinusEq | StarEq | SlashEq | PercentEq | AmpEq | PipeEq
        | CaretEq | ShlEq | ShrEq | Arrow | Pipe | QuestionDot | QuestionQuestion | Bang | Amp
        | Caret | BitOr | Shl | Shr | LParen | LBracket | LBrace | Comma | Colon | Dot | DotDot
        | DotDotEq | DotDotDot | Newline | Indent | Dedent | Eof => false,
    }
}

pub fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Char-length of the number literal starting at `start` — mirrors the lexer's `number()` extent
/// (radix prefixes, digit-group `_`, a single fraction, an `e`/`E` exponent), without re-parsing.
pub fn measure_number(chars: &[char], start: usize) -> usize {
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

/// Char-length of the `b"…"` / `r"…"` literal starting at `start`, covering the prefix, single/triple
/// quotes of either style, and (for a byte string) backslash escapes. An unterminated literal is
/// measured to end-of-input (or end-of-line for a single-quoted one). Neither kind holds an
/// interpolation hole, so a quote scan is exact for them; a plain `str` literal's extent is
/// `StrLit::len`, which only the lexer can know (a hole may nest a literal of the same quote).
pub fn measure_string(chars: &[char], start: usize) -> usize {
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

/// PEP-515: strip single underscores between digits from a numeric string before a Rust `parse`,
/// so `to_int`/`to_float`/`parse_int`/`parse_float`/`math.parse_int_base` agree
/// with the lexer, which already accepts `1_000` and rejects `1__0`/`_1`/`1_`
/// (`src/lexer/mod.rs:1447`'s predicate). Returns `None` when an `'_'` is not flanked by a digit of
/// `radix` on both sides.
pub fn strip_num_underscores_radix(s: &str, radix: u32) -> Option<std::borrow::Cow<'_, str>> {
    if !s.contains('_') {
        return Some(std::borrow::Cow::Borrowed(s));
    }
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' {
            let prev_ok = i > 0 && chars[i - 1].is_digit(radix);
            let next_ok = chars.get(i + 1).is_some_and(|n| n.is_digit(radix));
            if !(prev_ok && next_ok) {
                return None;
            }
        }
    }
    Some(std::borrow::Cow::Owned(
        chars.into_iter().filter(|c| *c != '_').collect(),
    ))
}

/// TICKET-265 — one module's identifier words: `text` holds them back to back, `at` the line and
/// byte range of each. `members` are the names read after a `.` / `?.` (`from` in `int.from`),
/// sorted and deduped.
#[derive(Debug, Default, Clone, PartialEq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub struct IdentWords {
    pub text: String,
    pub at: Vec<(u32, std::ops::Range<usize>)>,
    pub members: Vec<String>,
}

#[cfg(test)]
mod tests;
