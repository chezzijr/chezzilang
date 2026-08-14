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
    /// Contents only, quotes stripped, plus the [`PosMap`] a re-lexed interpolation fragment needs
    /// to report real source positions. `Deref<Target = str>` — read it like the `String` it was.
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
    NewType,
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
    QuestionDot,      // ?.  (optional chaining — only when the chars are adjacent)
    QuestionQuestion, // ??  (null-coalescing — only when the chars are adjacent)
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
/// `file` is what makes that key injective across modules. `KeywordKey`, `WitnessKey` and
/// `CarrierKey` (`src/checker/ty.rs:24,43,109`) are all `(graph_module_idx, frag_ctx: Span,
/// frag_ord, key_span: Span)`: the checker records a decision under one and the type-blind compiler
/// looks it up under the same one. `desugar` splices a callee's default-parameter expression into
/// the CALLER's AST as a clone that keeps the DEFINING module's spans, so without `file` two
/// unrelated call sites at the same `line:col` in two different files collapse onto one entry, the
/// later `insert` wins, and the survivor's decision is applied to both — a silent wrong value under
/// a green `chezzi check`, identical on both engines (parity is blind to it). See `docs/gaps.md`
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
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
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, col {}", self.line, self.col)
    }
}

/// A sparse map from a string literal's CONTENT char index (an index into the post-escape,
/// delimiter-stripped payload) to that char's **physical source [`Span`]**.
///
/// It exists because `raw` is lossy: escapes are already resolved and the delimiters are gone, so a
/// `\n` escape and a real source newline are indistinguishable downstream, and a `\u{1F600}` is nine
/// source chars wearing one content char's clothes. An interpolation fragment is re-lexed out of
/// `raw`, so without this map its token spans can only be an *offset* from the literal's start
/// (`docs/gaps.md` M24-6). With it they are real positions.
///
/// **Injectivity on `[0, raw_len)` is the load-bearing property, not the diagnostics.** A `Span` is
/// a cross-half TABLE KEY (`WitnessKey`/`KeywordKey`/`CarrierKey` — see [`Span`]), so two fragments
/// that share a span silently share a table entry: a wrong value under a green `chezzi check`
/// (measured 2026-08-10, commit `2a27697e`). Proof that `at` is injective here:
///
/// * every checkpoint `Span` is `Lexer::span_at(self.pos)` taken at a strictly increasing `self.pos`
///   of the SAME lexer, so checkpoints are distinct, increasing physical positions;
/// * between two checkpoints `note` fired and declined, which is exactly the statement that each
///   intermediate char sits one column right of its predecessor on one line — so the interpolated
///   run is a contiguous, strictly increasing column range;
/// * therefore `at(idx)` is the true physical position of content char `idx`, and distinct content
///   chars occupy distinct source chars.
///
/// Corollary: distinct fragments of one literal occupy disjoint `raw` sub-ranges → distinct spans; a
/// fragment of a NESTED literal is physically inside its parent's extent at a strictly later
/// position → distinct again; there are no zero-width escapes (`\` + newline is a `LexError`); and
/// `{{`/`}}` are consumed as ordinary content chars, so a fragment's `raw` index stays true.
#[derive(Debug, Clone, PartialEq)]
pub struct PosMap {
    /// Where content char 0 sits. Held INLINE rather than as `pts[0]` so the overwhelmingly common
    /// literal — no escapes, no newlines — leaves `pts` empty, and an empty `Vec` does not heap-
    /// allocate. That is what makes the zero-cost claim on [`str_token`] true rather than aspirational.
    start: Span,
    /// Checkpoints AFTER content char 0, strictly increasing in `.0`. One per discontinuity (an
    /// escape or a real newline), so a literal's cost is proportional to its escapes, not its length.
    pts: Vec<(usize, Span)>,
}

impl PosMap {
    /// A map that says only "content char 0 is at `first`" — i.e. the affine fallback
    /// `col = first.col + idx` on one line. This is the EXACT truth for a single-delimiter,
    /// escape-free, newline-free literal, not an approximation of it. Allocation-free.
    pub fn flat(first: Span) -> Self {
        PosMap {
            start: first,
            pts: Vec::new(),
        }
    }

    /// Record that content char `idx` sits at `at` — but ONLY if that is not what the previous
    /// checkpoint already implies. This is the whole rule.
    ///
    /// **Call it for EVERY content char, and do not "optimise" it into a switch on escape kinds.**
    /// The reason is composition, not tidiness: a nested literal inside a fragment builds its own
    /// map out of `span_at`, which routes through the PARENT's map, so a parent `\t`/`\r`/`\0`/
    /// `\u{…}` escape shows up to the inner lexer as an ordinary char with a column jump under it.
    /// A switch on escape kinds cannot see that jump and would put `{g()}` in
    /// `"{ f('a\tb{g()}c') }"` one column left of the truth — the aliasing class above, wearing the
    /// costume of a harmless optimisation.
    fn note(&mut self, idx: usize, at: Span) {
        // O(1): `note` is called with non-decreasing `idx`, so the governing checkpoint is the last
        // — or `start` while none has been pushed yet.
        let (i0, s0) = self.pts.last().copied().unwrap_or((0, self.start));
        if s0.line != at.line || s0.col + (idx - i0) as u32 != at.col {
            self.pts.push((idx, at));
        }
    }

    /// The physical source position of content char `idx`. Beyond the last content char this
    /// extrapolates from the final checkpoint (the fragment lexer's EOF span lands there).
    pub fn at(&self, idx: usize) -> Span {
        let (i0, s0) = match self.pts.partition_point(|(i, _)| *i <= idx) {
            0 => (0, self.start),
            k => self.pts[k - 1],
        };
        Span {
            line: s0.line,
            col: s0.col + (idx - i0) as u32,
            file: s0.file,
        }
    }
}

/// The payload of a `str` literal: its post-escape contents plus, when the literal carries a `{`/`}`
/// (so a fragment may be re-lexed out of it), the [`PosMap`] that turns a content index back into a
/// real source position.
///
/// `Deref<Target = str>` means every existing `raw.contains('{')` / `&raw` / `raw == "x"` site keeps
/// working, and `PartialEq` compares only `raw` so the lexer's `vec![Token::Str("…".into())]` tests
/// stay one `.into()` away from what they were.
///
/// `Debug` is hand-written to print **exactly what a `String` would**, so `chezzi tokens` and
/// `chezzi ast` — which dump derived `Debug` (`src/main.rs`) — stay byte-identical to what they were
/// before the map existed. The map is lexer machinery, not literal content: a derived `Debug` turned
/// one interpolated literal into a screenful of `Span` blocks under `{:#?}`, one per checkpoint.
#[derive(Clone, Default)]
pub struct StrLit {
    /// The literal's contents: escapes resolved, delimiters stripped.
    pub raw: String,
    /// `None` means "the affine fallback from the literal's own span is EXACT" — not "unknown".
    /// It is `None` for every brace-free literal (nothing will ever re-lex a fragment out of it)
    /// and for a synthesized `ExprKind::Str`, which by construction has no interesting geometry.
    pub map: Option<std::sync::Arc<PosMap>>,
}

impl std::ops::Deref for StrLit {
    type Target = str;
    fn deref(&self) -> &str {
        &self.raw
    }
}

impl PartialEq for StrLit {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl PartialEq<str> for StrLit {
    fn eq(&self, other: &str) -> bool {
        self.raw == other
    }
}

impl From<&str> for StrLit {
    fn from(s: &str) -> Self {
        StrLit {
            raw: s.to_string(),
            map: None,
        }
    }
}

impl From<String> for StrLit {
    fn from(raw: String) -> Self {
        StrLit { raw, map: None }
    }
}

impl fmt::Display for StrLit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl fmt::Debug for StrLit {
    /// Prints as the bare `String` it wraps — see the type's doc for why the map is deliberately
    /// invisible here (`chezzi tokens` / `chezzi ast` dump derived `Debug`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.raw, f)
    }
}

/// Finish a scanned `str` literal. The map rides along only when the literal can actually spawn a
/// fragment — the same `contains('{') || contains('}')` guard `desugar` uses to decide whether to
/// call `parse_interpolation` at all.
///
/// Cost, stated exactly rather than optimistically: a literal with **no escapes and no newlines**
/// builds a [`PosMap`] whose `pts` stays empty, so it allocates NOTHING and this guard just drops a
/// 12-byte `Span`. A literal WITH escapes or newlines pushes one checkpoint per discontinuity, and
/// this guard discards that `Vec` if the literal turns out to be brace-free — work proportional to
/// escapes, thrown away. Building the map only for brace-carrying literals would need a second,
/// escape-aware pre-scan to find the closing delimiter; not worth it until a profile says so.
// ponytail: brace-free escape-heavy literals build a checkpoint vec and drop it. Pre-scan for
// `{`/`}` before scanning if lexing ever shows up in a profile.
fn str_token(text: String, map: PosMap) -> Token {
    let map = (text.contains('{') || text.contains('}')).then(|| std::sync::Arc::new(map));
    Token::Str(StrLit { raw: text, map })
}

/// A token paired with where it came from. The lexer emits these; the `Token` payload is the
/// `kind`. (`tokens` CLI output prints only `kind`, so M1 output is unchanged.)
#[derive(Debug, Clone, PartialEq)]
pub struct Tok {
    pub kind: Token,
    pub span: Span,
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
    ("newtype", Token::NewType),
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
            DotDotDot => "...",
            _ => return None,
        })
    }
}

/// Look up whether an identifier is actually a keyword.
/// HINT (sub-step 1e): call this after you've scanned a whole word.
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
    /// Set only when this lexer is re-lexing a string-interpolation FRAGMENT: the enclosing
    /// literal's [`PosMap`] plus this fragment's content offset within that literal. When it is set,
    /// [`Lexer::span_at`] asks the map instead of counting columns, so every fragment token span is
    /// a real physical source position — which is what makes it a safe cross-half table key (see
    /// [`Span`]) as well as an honest diagnostic (`docs/gaps.md` M24-6).
    ///
    /// It COMPOSES: a literal nested inside a fragment builds its own map out of `span_at`, i.e. out
    /// of the parent's map, so positions stay true at any nesting depth. `None` → normal lexing, and
    /// `span_at`'s formula is byte-identical to what it always was.
    origin: Option<(std::sync::Arc<PosMap>, usize)>,
    /// Module id stamped into every emitted span's [`Span::file`] (default 0 = standalone /
    /// synthesized). Assigned by the one production graph lex seam, `resolver::Builder::parse`.
    /// Ignored while `origin` is set — a fragment inherits its literal's file through the map.
    file: u32,
    line_start: usize, // char index where the current line begins (for column tracking)
    indents: Vec<usize>, // the indentation stack. Starts as vec![0].
    at_line_start: bool, // true when the next char begins a fresh logical line
    pending: VecDeque<Tok>, // layout tokens computed together but emitted one-per-call (Dedents)
    bracket_depth: usize, // nesting depth of (), [], {}; >0 suppresses layout (multi-line literals)
    /// Side-channel for doc-comments: `(line, stripped_text)` for every comment-ONLY line
    /// (a `#` line with no preceding tokens). Inline trailing comments (STEP B) are NOT recorded,
    /// so they can never become docs. The token stream is unaffected — this is a pure side table the
    /// parser consults for the contiguous comment run immediately above a declaration.
    doc_comments: Vec<DocComment>,
    /// Memoized char index of the `|` of a resolved leading-`|>` continuation (see
    /// [`Lexer::pipe_continues_next_line`]): scan a blank/comment run once, not once per line.
    pipe_cont: Option<usize>,
}

impl Lexer {
    pub fn new(source: &str) -> Self {
        Lexer::new_in(source, 0, None)
    }

    /// Like [`Lexer::new`] but stamps every emitted span with the module id `file` (see
    /// [`Span::file`]). `0` is identical to [`Lexer::new`].
    pub fn new_file(source: &str, file: u32) -> Self {
        Lexer::new_in(source, file, None)
    }

    fn new_in(source: &str, file: u32, origin: Option<(std::sync::Arc<PosMap>, usize)>) -> Self {
        Lexer {
            chars: source.chars().collect(),
            pos: 0,
            line: 1,
            origin,
            file,
            line_start: 0,
            indents: vec![0],
            at_line_start: true,
            pending: VecDeque::new(),
            bracket_depth: 0,
            doc_comments: Vec::new(),
            pipe_cont: None,
        }
    }

    /// The span pointing at character index `pos` on the current line.
    fn span_at(&self, pos: usize) -> Span {
        match &self.origin {
            // A fragment: ask the enclosing literal's map for the char's REAL position.
            Some((map, off)) => map.at(off + pos),
            None => Span {
                // `as u32`: both are 1-based source counters bounded by the source length — a file
                // with 4 billion lines (or a line with 4 billion columns) is not reachable.
                line: self.line as u32,
                col: (pos - self.line_start + 1) as u32,
                file: self.file,
            },
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

    /// Produce the next single token (it's fine for ONE call to also emit layout tokens —
    /// see the indentation design note below; you may buffer pending Dedents in a field).
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
    fn next_token(&mut self) -> Result<Tok, LexError> {
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
                    });
                }
                // 2. ...then emit one Dedent per still-open indent level...
                if *self.indents.last().unwrap() > 0 {
                    self.indents.pop();
                    return Ok(Tok {
                        kind: Token::Dedent,
                        span,
                    });
                }
                // 3. ...and finally Eof.
                return Ok(Tok {
                    kind: Token::Eof,
                    span,
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
                });
            }

            // === STEP E: a real token starts here ===
            self.at_line_start = false;
            let start = self.pos; // index of the first char of this lexeme
            let span = self.span_at(start);
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
                    if self.match_char('=') {
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
                // `??` / `?.` are recognized only when adjacent (no whitespace): `match_char` checks the
                // very next char. `x? .field` (space) stays `Question` + `Dot` (try-then-field).
                '?' => {
                    if self.match_char('?') {
                        Token::QuestionQuestion
                    } else if self.match_char('.') {
                        Token::QuestionDot
                    } else {
                        Token::Question
                    }
                }
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
                    self.triple_string(c)?
                }
                '"' => self.string('"')?,
                '\'' => self.string('\'')?,
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

                // `start`, not the cursor: the char was consumed at the top of this match.
                other => {
                    return Err(self.error_at(start, &format!("unexpected character {other:?}")));
                }
            };
            return Ok(Tok { kind, span });
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
    /// It asks `span_at` rather than reading `self.line`/`self.line_start`, because inside a
    /// re-lexed interpolation FRAGMENT those are fragment-local — so a lex error in
    /// `"{ 'unterminated }"` on line 40 of a real program surfaced as `lex error (line 1)`.
    /// `span_at` composes through the enclosing literal's `PosMap`, so a column inside a fragment
    /// is right for free. At top level it returns `self.line` verbatim.
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
    /// Consumes the leading spaces of the upcoming line, skipping blank and comment-only lines
    /// (which never affect indentation). Then compares the line's indent width against the indent
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
            let top = *self.indents.last().unwrap();
            if width > top {
                // deeper → open one level
                self.indents.push(width);
                return Ok(Some(vec![Tok {
                    kind: Token::Indent,
                    span,
                }]));
            } else if width < top {
                // shallower → close as many levels as needed, one Dedent each
                let mut dedents = Vec::new();
                while *self.indents.last().unwrap() > width {
                    self.indents.pop();
                    dedents.push(Tok {
                        kind: Token::Dedent,
                        span,
                    });
                }
                // the new width must line up with some outer level we landed on
                if *self.indents.last().unwrap() != width {
                    return Err(self.error(
                        "inconsistent dedent — does not match any outer indentation level",
                    ));
                }
                return Ok(Some(dedents));
            } else {
                // same level → no layout token
                return Ok(Some(Vec::new()));
            }
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
                        let prev_ok = i > 0 && is_digit(body[i - 1]);
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

        // integer part (digits + group separators)
        while self.peek().is_ascii_digit() || self.peek() == '_' {
            self.advance();
        }

        // fractional part — ONLY if the dot is followed by a digit
        // (so `1.` keeps its dot, and `0..10` is not eaten as a number)
        if self.peek() == '.' && self.peek_next().is_ascii_digit() {
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
        if self.peek() == 'e' || self.peek() == 'E' {
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

    /// (1d) Scan a string literal. The opening `quote` (`"` or `'`) is already consumed.
    ///
    /// Munches up to the matching closing `quote`, translating backslash escapes
    /// (`\n \t \r \\ \" \' \0 \u{HEX}`). An unknown escape, or a `\` at end-of-input, is a
    /// `LexError`. Both quote styles produce the same `Token::Str` and share all escape
    /// handling, so `'…'` and `"…"` are interchangeable: in a single-quoted string `"` is a
    /// literal char and `\'` escapes the quote; in a double-quoted string `'` is a literal char
    /// and `\"` escapes the quote (both `\'` and `\"` are accepted in either style). The stored
    /// contents are the *processed* text (escapes resolved, quotes stripped); brace
    /// interpolation (`{…}`) is a separate, later pass — not handled here.
    fn string(&mut self, quote: char) -> Result<Token, LexError> {
        let mut text = String::new();
        // An unterminated literal points at its OPENER, which by then may be lines behind the
        // cursor — so capture the SPAN now, while the opener is still on the current line.
        let open = self.span_at(self.pos - 1);
        // Checkpoint 0 is taken HERE, where `self.pos` already sits past the opening delimiter — so
        // the map is right for `'…'` and `"""…"""` alike with no delimiter-width arithmetic.
        let mut map = PosMap::flat(self.span_at(self.pos));
        let mut n = 0usize;
        while !self.is_at_end() && self.peek() != quote {
            // Exactly one content char is produced per iteration (the `\u` arm `continue`s after
            // pushing one). See `PosMap::note` for why this is per-char and not per-escape.
            map.note(n, self.span_at(self.pos));
            n += 1;
            if self.peek() == '\\' {
                self.advance(); // consume the backslash
                if self.is_at_end() {
                    return Err(
                        self.error_span(open, "unterminated string literal (trailing '\\')")
                    );
                }
                let esc = self.advance();
                let translated = match esc {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '\\' => '\\',
                    '"' => '"',
                    '\'' => '\'',
                    '0' => '\0',
                    // `\u{HEX}` — 1-6 hex digits naming a Unicode scalar value.
                    'u' => {
                        text.push(self.unicode_escape()?);
                        continue;
                    }
                    // The `\` (2 back: the backslash and the newline are both consumed).
                    '\n' | '\r' => {
                        return Err(self.error_at(
                            self.pos - 2,
                            "line continuations are not supported; close the string or use \\n",
                        ));
                    }
                    // The escape CHAR, not the `\` before it (rustc points here).
                    other => {
                        return Err(
                            self.error_at(self.pos - 1, &format!("unknown escape '\\{other}'"))
                        );
                    }
                };
                text.push(translated);
            } else {
                if self.peek() == '\n' {
                    self.line += 1; // a *literal* newline → multi-line string; keep line count honest
                    self.line_start = self.pos + 1; // next char begins the new line
                }
                text.push(self.advance());
            }
        }
        if self.is_at_end() {
            return Err(self.error_span(open, "unterminated string literal"));
        }
        self.advance(); // consume the closing quote
        Ok(str_token(text, map))
    }

    /// (1d′) Scan a *triple-quoted* string literal (`"""…"""` or `'''…'''`). The opening triple
    /// `quote` is already consumed. Identical to [`string`] — same backslash escapes, `\u{…}`, and
    /// literal-newline handling, leaving `{…}` interpolation to the later pass — except the
    /// terminator is a triple of `quote`, so a single or double `quote` inside is an ordinary char.
    /// Produces a normal `Token::Str`.
    fn triple_string(&mut self, quote: char) -> Result<Token, LexError> {
        let mut text = String::new();
        // See `string()`: the opener (here 3 chars wide) may be lines behind the cursor by the time
        // an unterminated literal is detected, so capture its span up front.
        let open = self.span_at(self.pos - 3);
        // `self.pos` is already past the opening `"""`, so checkpoint 0 is right.
        let mut map = PosMap::flat(self.span_at(self.pos));
        let mut n = 0usize;
        // Closes only when the next THREE chars are all `quote`.
        while !(self.peek() == quote
            && self.peek_next() == quote
            && self.chars.get(self.pos + 2) == Some(&quote))
        {
            if self.is_at_end() {
                return Err(self.error_span(open, "unterminated triple-quoted string literal"));
            }
            map.note(n, self.span_at(self.pos));
            n += 1;
            if self.peek() == '\\' {
                self.advance(); // consume the backslash
                if self.is_at_end() {
                    return Err(
                        self.error_span(open, "unterminated string literal (trailing '\\')")
                    );
                }
                let esc = self.advance();
                let translated = match esc {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '\\' => '\\',
                    '"' => '"',
                    '\'' => '\'',
                    '0' => '\0',
                    'u' => {
                        text.push(self.unicode_escape()?);
                        continue;
                    }
                    // The `\` (2 back: the backslash and the newline are both consumed).
                    '\n' | '\r' => {
                        return Err(self.error_at(
                            self.pos - 2,
                            "line continuations are not supported; close the string or use \\n",
                        ));
                    }
                    // The escape CHAR, not the `\` before it.
                    other => {
                        return Err(
                            self.error_at(self.pos - 1, &format!("unknown escape '\\{other}'"))
                        );
                    }
                };
                text.push(translated);
            } else {
                if self.peek() == '\n' {
                    self.line += 1; // keep the line count honest across embedded newlines
                    self.line_start = self.pos + 1;
                }
                text.push(self.advance());
            }
        }
        // consume the closing triple
        self.advance();
        self.advance();
        self.advance();
        Ok(str_token(text, map))
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

/// Re-lex a string-interpolation FRAGMENT: `source` is the fragment's expression text, `map` the
/// enclosing literal's [`PosMap`] and `off` the fragment's content offset within that literal. Every
/// emitted span is the real physical source position of the char it points at — the fragment's file
/// comes along inside the map's spans, so nothing has to re-stamp it.
pub fn tokenize_frag(
    source: &str,
    map: std::sync::Arc<PosMap>,
    off: usize,
) -> Result<Vec<Tok>, LexError> {
    let file = map.at(off).file;
    Lexer::new_in(source, file, Some((map, off))).tokenize()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Lex `src` and return just the token kinds (dropping spans) — keeps the layout
    /// assertions below readable now that the lexer emits `Tok { kind, span }`.
    fn kinds(src: &str) -> Vec<Token> {
        tokenize(src).unwrap().into_iter().map(|t| t.kind).collect()
    }

    // GUIDING TESTS — make these pass one at a time. They define "correct" for M1.
    // Start with `single_plus`, then work down. Add your own as you go.
    //
    // NOTE: every token stream should END with `Token::Eof`. Most logical lines end with
    // `Token::Newline`. Adjust these expectations if you and Claude agree on different layout
    // rules — they're a starting contract, not gospel.

    #[test]
    fn single_plus() {
        assert_eq!(kinds("+"), vec![Token::Plus, Token::Newline, Token::Eof]);
    }

    #[test]
    fn variadic_ellipsis_token() {
        // `...` is a single DotDotDot token (variadic param marker).
        assert_eq!(
            kinds("...xs"),
            vec![
                Token::DotDotDot,
                Token::Ident("xs".into()),
                Token::Newline,
                Token::Eof
            ]
        );
        // `..` (range) still lexes as DotDot with no regression.
        assert_eq!(
            kinds("0..10"),
            vec![
                Token::Int(0),
                Token::DotDot,
                Token::Int(10),
                Token::Newline,
                Token::Eof
            ]
        );
        // A single `.` between idents is still Dot.
        assert_eq!(
            kinds("a.b"),
            vec![
                Token::Ident("a".into()),
                Token::Dot,
                Token::Ident("b".into()),
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn triple_double_quote_unescaped_quote() {
        // Inside a triple-quoted string, a lone `"` is an ordinary char (the only added value
        // over a regular string). `"""say "hi"""` lexes to Str(`say "hi`).
        assert_eq!(
            kinds("\"\"\"say \"hi\"\"\""),
            vec![Token::Str("say \"hi".into()), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn triple_string_newline_and_interp() {
        // `\n` escape is a real newline; a literal newline is preserved; `{x}` is left
        // un-processed (interpolation is a later pass, same as regular strings).
        assert_eq!(
            kinds("\"\"\"a\\nb {x}\"\"\""),
            vec![Token::Str("a\nb {x}".into()), Token::Newline, Token::Eof]
        );
        // Literal newline inside the triple string is preserved verbatim.
        assert_eq!(
            kinds("\"\"\"a\nb\"\"\""),
            vec![Token::Str("a\nb".into()), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn raw_string_short_verbatim() {
        // `r"..."` is verbatim: braces are literal, backslashes are literal (no escapes).
        assert_eq!(
            kinds("r\"{}\""),
            vec![Token::RawStr("{}".into()), Token::Newline, Token::Eof]
        );
        assert_eq!(
            kinds("r'\\d+'"),
            vec![Token::RawStr("\\d+".into()), Token::Newline, Token::Eof]
        );
        // Uppercase `R` behaves identically (mirrors `b`/`B`).
        assert_eq!(
            kinds("R\"x\""),
            vec![Token::RawStr("x".into()), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn raw_string_triple_embeds_quotes() {
        // Triple raw string: embedded double-quotes + braces are literal.
        assert_eq!(
            kinds("r\"\"\"{\"k\": [1,2]}\"\"\""),
            vec![
                Token::RawStr("{\"k\": [1,2]}".into()),
                Token::Newline,
                Token::Eof
            ]
        );
        // A literal `\n` inside a raw triple stays two chars (backslash, n) — no escapes.
        assert_eq!(
            kinds("r\"\"\"a\\nb\"\"\""),
            vec![Token::RawStr("a\\nb".into()), Token::Newline, Token::Eof]
        );
        // A real embedded newline is preserved verbatim.
        assert_eq!(
            kinds("r\"\"\"a\nb\"\"\""),
            vec![Token::RawStr("a\nb".into()), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn bare_r_is_identifier() {
        // A bare `r`/`rx` (no adjacent quote) is still an identifier — adjacency rule preserved.
        assert_eq!(
            kinds("r := 5"),
            vec![
                Token::Ident("r".into()),
                Token::Walrus,
                Token::Int(5),
                Token::Newline,
                Token::Eof
            ]
        );
        assert_eq!(
            kinds("rx + 1"),
            vec![
                Token::Ident("rx".into()),
                Token::Plus,
                Token::Int(1),
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn triple_single_quote_string() {
        assert_eq!(
            kinds("'''it's \"quoted\"'''"),
            vec![
                Token::Str("it's \"quoted\"".into()),
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn empty_triple_string() {
        assert_eq!(
            kinds("\"\"\"\"\"\""),
            vec![Token::Str("".into()), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn empty_regular_string_not_triple() {
        // `""` is an empty regular string, NOT a triple opener.
        assert_eq!(
            kinds("\"\""),
            vec![Token::Str("".into()), Token::Newline, Token::Eof]
        );
        // `"" x` — empty string then an ident.
        assert_eq!(
            kinds("\"\" x"),
            vec![
                Token::Str("".into()),
                Token::Ident("x".into()),
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn extern_keyword() {
        assert_eq!(
            kinds("extern"),
            vec![Token::Extern, Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn bang_is_a_token() {
        // Bare `!` lexes to `Bang` (consumed only in type position, for the `T!` Result shorthand).
        assert_eq!(kinds("!"), vec![Token::Bang, Token::Newline, Token::Eof]);
    }

    #[test]
    fn bang_vs_not_eq() {
        assert_eq!(
            kinds("! !="),
            vec![Token::Bang, Token::NotEq, Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn break_and_continue_keywords() {
        assert_eq!(
            kinds("break continue"),
            vec![Token::Break, Token::Continue, Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn braces_lex_for_maps() {
        assert_eq!(
            kinds("{}"),
            vec![Token::LBrace, Token::RBrace, Token::Newline, Token::Eof]
        );
        assert_eq!(
            kinds("{\"a\": 1}"),
            vec![
                Token::LBrace,
                Token::Str("a".into()),
                Token::Colon,
                Token::Int(1),
                Token::RBrace,
                Token::Newline,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn two_char_operators() {
        assert_eq!(
            kinds(":= == -> |>"),
            vec![
                Token::Walrus,
                Token::EqEq,
                Token::Arrow,
                Token::Pipe,
                Token::Newline,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn numbers_and_ident_and_keyword() {
        assert_eq!(
            kinds("x := 42"),
            vec![
                Token::Ident("x".to_string()),
                Token::Walrus,
                Token::Int(42),
                Token::Newline,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn comment_is_skipped() {
        assert_eq!(
            kinds("1 # this is ignored"),
            vec![Token::Int(1), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn doc_comments_captured_with_line_numbers() {
        // Comment-only lines are captured on the side channel with their line numbers + stripped body
        // (one leading space removed). The token stream is unchanged.
        let (toks, comments) = tokenize_with_comments("# a\n# b\nfn f():\n    1\n", 0).unwrap();
        assert_eq!(comments, vec![(1, "a".to_string()), (2, "b".to_string())]);
        // token stream byte-identical to plain tokenize (no Comment tokens leaked in)
        let plain = tokenize("# a\n# b\nfn f():\n    1\n").unwrap();
        assert_eq!(toks, plain);
    }

    #[test]
    fn inline_trailing_comment_not_captured() {
        // A trailing comment on a content line goes through STEP B, never the side channel.
        let (_toks, comments) = tokenize_with_comments("fn f(): 1  # not a doc\n", 0).unwrap();
        assert!(comments.is_empty());
    }

    #[test]
    fn indentation_emits_indent_dedent() {
        // Two logical lines; the second is indented under the first.
        assert_eq!(
            kinds("if x:\n    y\n"),
            vec![
                Token::If,
                Token::Ident("x".to_string()),
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::Ident("y".to_string()),
                Token::Newline,
                Token::Dedent,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn spans_track_line_and_column() {
        // `y` sits on line 2, indented 4 spaces → column 5.
        let toks = tokenize("if x:\n    y\n").unwrap();
        let y = toks
            .iter()
            .find(|t| t.kind == Token::Ident("y".to_string()))
            .unwrap();
        assert_eq!(
            y.span,
            Span {
                line: 2,
                col: 5,
                file: 0
            }
        );

        // `if` is the first token: line 1, column 1.
        assert_eq!(toks[0].span, Span::RUNTIME);
    }

    #[test]
    fn layout_token_spans() {
        // if x:\n    y\n  →  the Indent is at the start of line 2, the trailing Dedent + Eof
        // land at the final position on line 3 (end of input).
        let toks = tokenize("if x:\n    y\n").unwrap();
        let indent = toks.iter().find(|t| t.kind == Token::Indent).unwrap();
        assert_eq!(indent.span.line, 2);

        let eof = toks.last().unwrap();
        assert_eq!(eof.kind, Token::Eof);
        assert_eq!(eof.span.line, 3);
    }

    // ----- string escapes -----

    #[test]
    fn string_escapes_translate() {
        // Chezzi source: "a\nb\tc\\d\"e"  (backslashes literal in the raw string below)
        assert_eq!(
            kinds(r#""a\nb\tc\\d\"e""#),
            vec![
                Token::Str("a\nb\tc\\d\"e".into()),
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn string_escape_nul() {
        assert_eq!(
            kinds(r#""x\0y""#),
            vec![Token::Str("x\0y".into()), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn unknown_escape_is_an_error() {
        assert!(tokenize(r#""\q""#).is_err());
    }

    // ----- \u{...} unicode escapes (Task 2) -----

    #[test]
    fn unicode_escape_basic() {
        assert_eq!(
            kinds(r#""\u{41}""#),
            vec![Token::Str("A".into()), Token::Newline, Token::Eof]
        );
        assert_eq!(
            kinds(r#""\u{e9}""#),
            vec![Token::Str("é".into()), Token::Newline, Token::Eof]
        );
        assert_eq!(
            kinds(r#""\u{1F600}""#),
            vec![Token::Str("😀".into()), Token::Newline, Token::Eof]
        );
        // surrounded by ordinary text
        assert_eq!(
            kinds(r#""x\u{41}y""#),
            vec![Token::Str("xAy".into()), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn unicode_escape_rejects_malformed() {
        assert!(tokenize(r#""\u41""#).is_err(), "missing brace");
        assert!(tokenize(r#""\u{}""#).is_err(), "empty");
        assert!(tokenize(r#""\u{D800}""#).is_err(), "surrogate");
        assert!(tokenize(r#""\u{110000}""#).is_err(), ">10FFFF");
        assert!(tokenize(r#""\u{1234567}""#).is_err(), "7 hex digits");
        assert!(tokenize(r#""\u{GG}""#).is_err(), "non-hex");
        assert!(tokenize(r#""\u{41""#).is_err(), "unterminated brace");
    }

    #[test]
    fn trailing_backslash_is_unterminated() {
        // "abc\  with no closing quote
        assert!(tokenize("\"abc\\").is_err());
    }

    // ----- b"..." byte-string literals (bytes type) -----

    #[test]
    fn byte_string_literal_and_escapes() {
        // plain ASCII -> bytes
        assert_eq!(
            kinds(r#"b"AB""#),
            vec![Token::Bytes(vec![65, 66]), Token::Newline, Token::Eof]
        );
        // \xHH hex byte escapes (out-of-ASCII range OK: 0..=255)
        assert_eq!(
            kinds(r#"b"\xFF\x00""#),
            vec![Token::Bytes(vec![255, 0]), Token::Newline, Token::Eof]
        );
        // standard escapes push their ASCII byte
        assert_eq!(
            kinds(r#"b"\n\t""#),
            vec![Token::Bytes(vec![10, 9]), Token::Newline, Token::Eof]
        );
        // uppercase prefix and single quotes both work
        assert_eq!(
            kinds(r#"B'A'"#),
            vec![Token::Bytes(vec![65]), Token::Newline, Token::Eof]
        );
        // empty byte literal
        assert_eq!(
            kinds(r#"b"""#),
            vec![Token::Bytes(vec![]), Token::Newline, Token::Eof]
        );
        // triple-quoted byte literal
        assert_eq!(
            kinds("b\"\"\"AB\"\"\""),
            vec![Token::Bytes(vec![65, 66]), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn bare_bytearray_is_identifier_not_literal() {
        // `bytearray` is a CONSTRUCTOR (a builtin call), NOT a literal — there is no `ba"..."` lexer
        // form (bytes already owns the `b"..."` literal). So `bytearray` lexes as a plain identifier,
        // and the `b` prefix only fires when immediately followed by a quote (documents the design).
        assert_eq!(
            kinds("bytearray"),
            vec![
                Token::Ident("bytearray".to_string()),
                Token::Newline,
                Token::Eof
            ]
        );
        // `bytearray(...)` is just IDENT LPAREN ... RPAREN — the existing call production.
        assert_eq!(
            kinds("bytearray()"),
            vec![
                Token::Ident("bytearray".to_string()),
                Token::LParen,
                Token::RParen,
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn byte_string_rejects_unicode_and_non_ascii() {
        // \u{...} is not valid in a byte literal
        let e = tokenize(r#"b"\u{41}""#).unwrap_err();
        assert!(
            e.message.contains("\\u not allowed in a byte literal"),
            "{}",
            e.message
        );
        // a raw non-ASCII char (byte >= 0x80) must be rejected
        let e = tokenize("b\"é\"").unwrap_err();
        assert!(
            e.message.contains("non-ASCII byte in byte literal"),
            "{}",
            e.message
        );
        // a malformed \x (not two hex digits) errors
        assert!(tokenize(r#"b"\xG0""#).is_err(), "non-hex \\x");
        assert!(tokenize(r#"b"\x""#).is_err(), "\\x at end");
        assert!(tokenize(r#"b"\q""#).is_err(), "unknown escape");
    }

    #[test]
    fn bare_b_is_identifier() {
        // `b` not followed by a quote is an ordinary identifier, never a byte-string prefix.
        assert_eq!(
            kinds("b + 1"),
            vec![
                Token::Ident("b".to_string()),
                Token::Plus,
                Token::Int(1),
                Token::Newline,
                Token::Eof
            ]
        );
        assert_eq!(
            kinds("by = 2"),
            vec![
                Token::Ident("by".to_string()),
                Token::Assign,
                Token::Int(2),
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn escaped_newline_does_not_advance_source_line() {
        // The `\n` here is an ESCAPE (two source chars on one line), so `z` stays on line 1.
        let src = r#"x := "a\nb" z"#.to_string() + "\n";
        let toks = tokenize(&src).unwrap();
        let z = toks
            .iter()
            .find(|t| t.kind == Token::Ident("z".to_string()))
            .unwrap();
        assert_eq!(z.span.line, 1);
    }

    // ----- single-quote strings (Task 3) -----

    #[test]
    fn single_quote_basic() {
        assert_eq!(
            kinds("'hello'"),
            vec![Token::Str("hello".into()), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn single_quote_equals_double() {
        // Same escape handling: `\n` in a single-quoted string resolves identically.
        assert_eq!(kinds(r"'a\nb'"), kinds(r#""a\nb""#));
        // and the new \u{} escape works in single quotes too
        assert_eq!(
            kinds(r"'\u{41}'"),
            vec![Token::Str("A".into()), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn single_quote_inner_double_literal() {
        // A `"` inside a single-quoted string is a literal char (no escape needed).
        assert_eq!(
            kinds(r#"'say "hi"'"#),
            vec![Token::Str("say \"hi\"".into()), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn single_quote_escapes() {
        // `\'` escapes the closing quote inside a single-quoted string.
        assert_eq!(
            kinds(r"'it\'s'"),
            vec![Token::Str("it's".into()), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn double_quote_single_literal() {
        // A `'` inside a double-quoted string is a literal char.
        assert_eq!(
            kinds(r#""it's""#),
            vec![Token::Str("it's".into()), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn lexes_concurrency_keywords() {
        assert_eq!(
            kinds("spawn parallel"),
            vec![Token::Spawn, Token::Parallel, Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn lexes_yield_keyword() {
        assert_eq!(
            kinds("yield 1"),
            vec![Token::Yield, Token::Int(1), Token::Newline, Token::Eof]
        );
    }

    // ----- numeric underscores -----

    #[test]
    fn int_with_underscores() {
        assert_eq!(
            kinds("10_000_000"),
            vec![Token::Int(10_000_000), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn float_with_underscores() {
        assert_eq!(
            kinds("1_000.000_5"),
            vec![Token::Float(1000.0005), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn bad_underscores_are_errors() {
        assert!(tokenize("1__0").is_err(), "double underscore");
        assert!(tokenize("1_").is_err(), "trailing underscore");
        assert!(tokenize("1_.5").is_err(), "underscore before dot");
    }

    #[test]
    fn underscores_do_not_break_range() {
        assert_eq!(
            kinds("0..10"),
            vec![
                Token::Int(0),
                Token::DotDot,
                Token::Int(10),
                Token::Newline,
                Token::Eof
            ]
        );
    }

    // ----- radix-prefixed integer literals (hex / binary / octal) -----

    #[test]
    fn lexes_hex_literal() {
        assert_eq!(
            kinds("0xFF"),
            vec![Token::Int(255), Token::Newline, Token::Eof]
        );
        assert_eq!(
            kinds("0x1a"),
            vec![Token::Int(26), Token::Newline, Token::Eof]
        );
        assert_eq!(
            kinds("0XfF"),
            vec![Token::Int(255), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn lexes_binary_literal() {
        assert_eq!(
            kinds("0b1010"),
            vec![Token::Int(10), Token::Newline, Token::Eof]
        );
        assert_eq!(
            kinds("0B1111"),
            vec![Token::Int(15), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn lexes_octal_literal() {
        assert_eq!(
            kinds("0o17"),
            vec![Token::Int(15), Token::Newline, Token::Eof]
        );
        assert_eq!(
            kinds("0O777"),
            vec![Token::Int(511), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn lexes_i64_min_magnitude() {
        // 2^63 == i64::MAX + 1 overflows i64, but instead of a lex error it becomes a distinct
        // token so the parser can fold `-9223372036854775808` into i64::MIN.
        assert_eq!(
            kinds("9223372036854775808"),
            vec![Token::IntMinMagnitude, Token::Newline, Token::Eof]
        );
        // The neighbour that DOES fit stays a normal Int(i64::MAX).
        assert_eq!(
            kinds("9223372036854775807"),
            vec![Token::Int(i64::MAX), Token::Newline, Token::Eof]
        );
        // Preceded by a minus: the lexer still emits [Minus, IntMinMagnitude]; the fold is the parser's job.
        assert_eq!(
            kinds("-9223372036854775808"),
            vec![
                Token::Minus,
                Token::IntMinMagnitude,
                Token::Newline,
                Token::Eof
            ]
        );
        // 2^63 + 1 and beyond are genuinely unrepresentable even when negated → still a lex error.
        assert!(tokenize("9223372036854775809").is_err());
        // Underscored form of exactly 2^63 also folds.
        assert_eq!(
            kinds("9_223_372_036_854_775_808"),
            vec![Token::IntMinMagnitude, Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn radix_literals_allow_underscores() {
        assert_eq!(
            kinds("0xFF_FF"),
            vec![Token::Int(65535), Token::Newline, Token::Eof]
        );
        assert_eq!(
            kinds("0b1010_0101"),
            vec![Token::Int(165), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn bad_radix_digit_errors() {
        assert!(tokenize("0xG").is_err(), "non-hex digit");
        assert!(tokenize("0b2").is_err(), "non-binary digit");
        assert!(tokenize("0o8").is_err(), "non-octal digit");
        assert!(tokenize("0x").is_err(), "empty hex body");
        assert!(
            tokenize("0x_FF").is_err(),
            "leading underscore after prefix"
        );
    }

    #[test]
    fn bare_zero_still_decimal() {
        assert_eq!(kinds("0"), vec![Token::Int(0), Token::Newline, Token::Eof]);
        assert_eq!(
            kinds("007"),
            vec![Token::Int(7), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn zero_dot_float_unaffected() {
        assert_eq!(
            kinds("0.5"),
            vec![Token::Float(0.5), Token::Newline, Token::Eof]
        );
    }

    // ----- optional chaining `?.` and null-coalescing `??` (adjacency-sensitive) -----

    #[test]
    fn lexes_question_question_adjacent() {
        assert_eq!(
            kinds("a ?? b"),
            vec![
                Token::Ident("a".into()),
                Token::QuestionQuestion,
                Token::Ident("b".into()),
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn lexes_question_dot_adjacent() {
        assert_eq!(
            kinds("x?.f"),
            vec![
                Token::Ident("x".into()),
                Token::QuestionDot,
                Token::Ident("f".into()),
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn space_keeps_question_and_dot_separate() {
        // `x? .field` is try-then-field, NOT optional chaining: a bare `?` then a `.`.
        assert_eq!(
            kinds("x? .f"),
            vec![
                Token::Ident("x".into()),
                Token::Question,
                Token::Dot,
                Token::Ident("f".into()),
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn bare_question_still_try() {
        assert_eq!(
            kinds("x?"),
            vec![
                Token::Ident("x".into()),
                Token::Question,
                Token::Newline,
                Token::Eof
            ]
        );
    }

    // ----- scientific notation (Task 1) -----

    #[test]
    fn lexes_scientific_notation() {
        assert_eq!(
            kinds("1e3"),
            vec![Token::Float(1000.0), Token::Newline, Token::Eof]
        );
        assert_eq!(
            kinds("1.5e-9"),
            vec![Token::Float(1.5e-9), Token::Newline, Token::Eof]
        );
        assert_eq!(
            kinds("2E10"),
            vec![Token::Float(2e10), Token::Newline, Token::Eof]
        );
        assert_eq!(
            kinds("1e+5"),
            vec![Token::Float(1e5), Token::Newline, Token::Eof]
        );
        assert_eq!(
            kinds("6.022e23"),
            vec![Token::Float(6.022e23), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn scientific_trailing_e_not_consumed() {
        // A bare `e` with no valid exponent must NOT be eaten as part of the number.
        assert_eq!(
            kinds("1e"),
            vec![
                Token::Int(1),
                Token::Ident("e".into()),
                Token::Newline,
                Token::Eof
            ]
        );
        // `1e+` — the `e` falls to identifier(), the `+` to the operator dispatch.
        assert_eq!(
            kinds("1e+"),
            vec![
                Token::Int(1),
                Token::Ident("e".into()),
                Token::Plus,
                Token::Newline,
                Token::Eof
            ]
        );
        // `1.5e` — float mantissa already committed, then a bare `e`.
        assert_eq!(
            kinds("1.5e"),
            vec![
                Token::Float(1.5),
                Token::Ident("e".into()),
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn scientific_no_exponent_underscores() {
        // Exponent digit run consumes only ascii digits (no '_'); `1e1_0` -> Float(10.0) then `_0`.
        assert_eq!(
            kinds("1e1_0"),
            vec![
                Token::Float(10.0),
                Token::Ident("_0".into()),
                Token::Newline,
                Token::Eof
            ]
        );
        // regression: hex literal never reaches the exponent block.
        assert_eq!(
            kinds("0xFF"),
            vec![Token::Int(255), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn hex_field_access_not_eaten() {
        // `0xFF.bit_length` style — a `.` after a hex literal is postfix, not a fraction.
        assert_eq!(
            kinds("0xFF..0x2"),
            vec![
                Token::Int(255),
                Token::DotDot,
                Token::Int(2),
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn span_after_multiline_string() {
        // A string spanning two lines must keep the column honest for the token after it.
        // line 1: x := "a   (opens) ... line 2: b"  z
        let toks = tokenize("x := \"a\nb\" z\n").unwrap();
        let z = toks
            .iter()
            .find(|t| t.kind == Token::Ident("z".to_string()))
            .unwrap();
        // `b" z` → z is the 4th char on line 2 (1-based col 4).
        assert_eq!(
            z.span,
            Span {
                line: 2,
                col: 4,
                file: 0
            }
        );
    }

    // ===== Multi-line collection literals (layout suppression inside brackets) =====

    /// OOM TRIPWIRE: an unclosed bracket must terminate at Eof, never spin the tokenize loop.
    /// Passes on the unmodified lexer (no suppression yet) — a forward-progress regression guard.
    #[test]
    fn unclosed_bracket_terminates_at_eof() {
        for src in ["[1, 2", "(", "{a: 1", "[[\n1\n", "(((\n"] {
            let toks = tokenize(src).expect("unclosed bracket should still tokenize");
            assert_eq!(
                toks.last().map(|t| &t.kind),
                Some(&Token::Eof),
                "input {src:?} must terminate at Eof"
            );
        }
    }

    /// String interpolation braces live inside Token::Str content — they must NEVER be
    /// tokenized as LBrace/RBrace and so can never reach the bracket-depth counter.
    #[test]
    fn string_braces_not_counted() {
        let toks = tokenize("\"x{y}z\"\n").unwrap();
        let ks: Vec<&Token> = toks.iter().map(|t| &t.kind).collect();
        assert_eq!(
            ks,
            vec![&Token::Str("x{y}z".into()), &Token::Newline, &Token::Eof]
        );
        assert!(!ks.contains(&&Token::LBrace) && !ks.contains(&&Token::RBrace));
    }

    /// Newlines/Indent/Dedent inside `[...]` are suppressed; a Newline appears after the closer.
    #[test]
    fn newline_suppressed_inside_brackets() {
        assert_eq!(
            kinds("x = [\n1,\n2\n]\n"),
            vec![
                Token::Ident("x".to_string()),
                Token::Assign,
                Token::LBracket,
                Token::Int(1),
                Token::Comma,
                Token::Int(2),
                Token::RBracket,
                Token::Newline,
                Token::Eof,
            ]
        );
    }

    /// Same suppression for `(...)` and `{...}`.
    #[test]
    fn newline_suppressed_inside_paren_and_brace() {
        assert_eq!(
            kinds("(\n1,\n2\n)\n"),
            vec![
                Token::LParen,
                Token::Int(1),
                Token::Comma,
                Token::Int(2),
                Token::RParen,
                Token::Newline,
                Token::Eof,
            ]
        );
        assert_eq!(
            kinds("{\n1: 2\n}\n"),
            vec![
                Token::LBrace,
                Token::Int(1),
                Token::Colon,
                Token::Int(2),
                Token::RBrace,
                Token::Newline,
                Token::Eof,
            ]
        );
    }

    /// REGRESSION GUARD: statement newlines OUTSIDE brackets are still emitted.
    #[test]
    fn statement_newlines_outside_brackets_unchanged() {
        assert_eq!(
            kinds("a\nb\n"),
            vec![
                Token::Ident("a".to_string()),
                Token::Newline,
                Token::Ident("b".to_string()),
                Token::Newline,
                Token::Eof,
            ]
        );
    }

    /// REGRESSION GUARD: an indented `if` body still lexes with Indent/Dedent.
    #[test]
    fn indented_body_lexes_identically() {
        assert_eq!(
            kinds("if x:\n    y\n"),
            vec![
                Token::If,
                Token::Ident("x".to_string()),
                Token::Colon,
                Token::Newline,
                Token::Indent,
                Token::Ident("y".to_string()),
                Token::Newline,
                Token::Dedent,
                Token::Eof,
            ]
        );
    }

    /// Nested brackets across lines: layout suppressed throughout; depth returns to 0.
    #[test]
    fn nested_multiline_brackets() {
        assert_eq!(
            kinds("[[\n1\n], {\n}]\n"),
            vec![
                Token::LBracket,
                Token::LBracket,
                Token::Int(1),
                Token::RBracket,
                Token::Comma,
                Token::LBrace,
                Token::RBrace,
                Token::RBracket,
                Token::Newline,
                Token::Eof,
            ]
        );
    }

    /// A stray closer must not underflow the depth counter (saturating_sub) nor panic.
    #[test]
    fn stray_closer_no_underflow() {
        // After the stray `]`, depth stays clamped at 0, so the following newline is emitted.
        assert_eq!(
            kinds("]\nx\n"),
            vec![
                Token::RBracket,
                Token::Newline,
                Token::Ident("x".to_string()),
                Token::Newline,
                Token::Eof,
            ]
        );
    }

    #[test]
    fn lexes_assert_keyword() {
        assert_eq!(
            kinds("assert x"),
            vec![
                Token::Assert,
                Token::Ident("x".into()),
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn lexes_test_keyword() {
        assert_eq!(
            kinds("test fn"),
            vec![Token::Test, Token::Fn, Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn lexes_pass_keyword() {
        assert_eq!(keyword("pass"), Some(Token::Pass));
        assert_eq!(kinds("pass"), vec![Token::Pass, Token::Newline, Token::Eof]);
        // `pass` is a keyword, never an identifier.
        assert!(!matches!(kinds("pass").first(), Some(Token::Ident(_))));
    }

    #[test]
    fn lexes_newtype_keyword() {
        assert_eq!(keyword("newtype"), Some(Token::NewType));
        assert_eq!(
            kinds("newtype Foo = int"),
            vec![
                Token::NewType,
                Token::Ident("Foo".to_string()),
                Token::Assign,
                Token::Ident("int".to_string()),
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn lexes_elif_keyword() {
        assert_eq!(keyword("elif"), Some(Token::Elif));
        assert_eq!(
            kinds("elif x:"),
            vec![
                Token::Elif,
                Token::Ident("x".to_string()),
                Token::Colon,
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn kw_table_is_single_source() {
        // Every entry in the table must round-trip through keyword(); a non-keyword is None.
        for (w, t) in KEYWORDS {
            assert_eq!(keyword(w), Some(t.clone()), "keyword({w:?}) mismatch");
        }
        assert_eq!(keyword("notakw"), None);
        // The table must cover the full keyword surface the lexer recognizes.
        for w in [
            "fn", "return", "if", "else", "elif", "for", "while", "in", "break", "continue",
            "pass", "struct", "enum", "protocol", "type", "newtype", "match", "recover", "defer",
            "assert", "test", "spawn", "parallel", "wait", "yield", "import", "extern", "from",
            "as", "and", "or", "not", "true", "false",
        ] {
            assert!(
                KEYWORDS.iter().any(|(k, _)| *k == w),
                "KEYWORDS missing {w:?}"
            );
        }
    }

    #[test]
    fn token_lexeme_covers_punctuation_and_keywords() {
        assert_eq!(Token::Plus.lexeme(), Some("+"));
        assert_eq!(Token::Walrus.lexeme(), Some(":="));
        assert_eq!(Token::ShlEq.lexeme(), Some("<<="));
        assert_eq!(Token::Fn.lexeme(), Some("fn"));
        assert_eq!(Token::NewType.lexeme(), Some("newtype"));
        // literals / idents / layout have no fixed spelling
        assert_eq!(Token::Ident("x".into()).lexeme(), None);
        assert_eq!(Token::Int(1).lexeme(), None);
        assert_eq!(Token::Str("s".into()).lexeme(), None);
        assert_eq!(Token::Newline.lexeme(), None);
        assert_eq!(Token::Eof.lexeme(), None);
        // every punctuation variant has a lexeme
        for t in PUNCTUATION {
            assert!(t.lexeme().is_some(), "{t:?} should have a lexeme");
        }
        // every keyword variant has a lexeme too
        for (w, t) in KEYWORDS {
            assert_eq!(t.lexeme(), Some(*w), "{t:?} lexeme mismatch");
        }
    }

    // ===== Leading-`|>` line continuation (layout suppression) =====

    /// A line whose first token is `|>` continues the previous logical line: no Newline,
    /// no Indent/Dedent for it.
    #[test]
    fn pipe_continuation_suppresses_layout() {
        let ks = kinds("r := 5\n    |> dbl()\n    |> inc()\nprint(r)\n");
        assert!(
            !ks.contains(&Token::Indent) && !ks.contains(&Token::Dedent),
            "{ks:?}"
        );
        assert_eq!(
            ks.iter().filter(|k| **k == Token::Newline).count(),
            2,
            "one Newline closes the chain, one closes `print(r)`: {ks:?}"
        );
        // same token stream as the single-line form
        assert_eq!(ks, kinds("r := 5 |> dbl() |> inc()\nprint(r)\n"));
    }

    /// Only the exact two-char `|>` continues — `|`, `||`, `|=` do not.
    #[test]
    fn pipe_continuation_only_for_exact_pipe_arrow() {
        for lead in ["| 2", "|| 2", "|= 2"] {
            let ks = kinds(&format!("a := 1\n    {lead}\n"));
            assert!(
                ks.contains(&Token::Indent) && ks.contains(&Token::Dedent),
                "{lead:?} must NOT continue the line: {ks:?}"
            );
        }
    }

    /// Blank and comment lines interleaved in the chain are skipped by the lookahead.
    #[test]
    fn pipe_continuation_skips_blank_and_comment_lines() {
        let ks = kinds("r := 5\n\n    # double it\n    |> dbl()\n\n    |> inc()\nprint(r)\n");
        assert!(
            !ks.contains(&Token::Indent) && !ks.contains(&Token::Dedent),
            "{ks:?}"
        );
        assert_eq!(ks.iter().filter(|k| **k == Token::Newline).count(), 2);
    }

    /// Inside a nested block the indent stack is untouched: the multi-line chain lexes to the
    /// exact same token stream as the one-line chain (so the line after it still dedents).
    #[test]
    fn pipe_continuation_inside_nested_block_keeps_indent_stack() {
        let multi = "fn f(x: int) -> int:\n    if x > 0:\n        r := x\n            |> dbl()\n            |> inc()\n        return r\n    return 0\n";
        let single = "fn f(x: int) -> int:\n    if x > 0:\n        r := x |> dbl() |> inc()\n        return r\n    return 0\n";
        assert_eq!(kinds(multi), kinds(single));
    }

    /// Spans still report true source lines across a suppressed region.
    #[test]
    fn pipe_continuation_spans_track_true_lines() {
        // 1: r := 5 / 2: blank / 3: comment / 4: |> dbl() / 5: |> inc() / 6: print(r)
        let toks = tokenize("r := 5\n\n    # c\n    |> dbl()\n    |> inc()\nprint(r)\n").unwrap();
        let line_of = |name: &str| {
            toks.iter()
                .find(|t| t.kind == Token::Ident(name.to_string()))
                .unwrap()
                .span
                .line
        };
        assert_eq!(line_of("dbl"), 4);
        assert_eq!(line_of("inc"), 5);
        assert_eq!(line_of("print"), 6);
    }

    /// The offside rule still binds: a `|>` line SHALLOWER than the open block closes that block
    /// (Dedent) instead of being absorbed into it — otherwise a column-0 `|>` after a function
    /// body would silently rewrite that body's last statement.
    #[test]
    fn pipe_continuation_respects_the_indent_floor() {
        // col-0 `|>` under an indented body: must dedent out, not continue `return 1`.
        let ks = kinds("fn f() -> int:\n    return 1\n|> dbl()\n");
        assert!(
            ks.contains(&Token::Indent) && ks.contains(&Token::Dedent),
            "a col-0 `|>` must close the block: {ks:?}"
        );
        // equal-indent `|>` inside a body IS a continuation (nothing to close)
        let ks = kinds("fn f() -> int:\n    r := 1\n    |> dbl()\n    return r\n");
        assert_eq!(
            ks.iter().filter(|k| **k == Token::Indent).count(),
            1,
            "only the body's Indent: {ks:?}"
        );
        // tabs never indent (scan_indentation rejects them) → not a continuation either
        assert!(tokenize("fn f() -> int:\n    r := 1\n\t|> dbl()\n").is_err());
    }

    /// Forward-progress tripwire: pathological input must terminate at Eof, never spin — and the
    /// memoized lookahead must keep it LINEAR (this file lexed in ~9s before the memo landed).
    #[test]
    fn pipe_continuation_pathological_terminates() {
        let mut src = String::from("r := 5\n");
        src.push_str(&"\n".repeat(200_000));
        src.push_str("    |> dbl()\n");
        let toks = tokenize(&src).unwrap();
        assert_eq!(toks.last().map(|t| &t.kind), Some(&Token::Eof));

        // the expensive variant: long comment lines (re-walked in full by an un-memoized scan)
        let mut src = String::from("r := 5\n");
        for _ in 0..50_000 {
            src.push_str("    # filler comment line here\n");
        }
        src.push_str("    |> dbl()\n");
        let toks = tokenize(&src).unwrap();
        assert_eq!(toks.last().map(|t| &t.kind), Some(&Token::Eof));

        // a file ending in a bare `|>` still lexes (and stays a parse error)
        let toks = tokenize("r := 5\n    |>\n").unwrap();
        assert_eq!(toks.last().map(|t| &t.kind), Some(&Token::Eof));
    }

    /// `Span` is 12 bytes and that is load-bearing, not incidental — `Proto.lines` holds one per
    /// OPCODE, so its width is the cache footprint of every compiled function, and it sets the
    /// calibrated headroom of `parser::MAX_DEPTH`, `parser::MAX_AST_DEPTH` and `vm::VM_STACK_BYTES`.
    /// When this fails, a field was added: measure `benches/run.chz` (the `map` bench moved 1.07× at
    /// 24 bytes) and re-probe those constants before changing the number here. The re-probe is four
    /// things now, not a hand-run: `parser::tests::max_depth_boundary_accepts_then_rejects` (the
    /// per-production sizing oracle), `check_errors_json::worst_accepted_nesting_never_signal_crashes`
    /// (the end-to-end margin oracle), `checker::tests::stack_probe_frontend_walker_depth`
    /// (`--ignored`, re-derives bytes/node) and `self_referential_stringable_hits_depth_limit`.
    #[test]
    fn span_stays_twelve_bytes() {
        assert_eq!(std::mem::size_of::<Span>(), 12);
    }

    /// A fragment may contain a REAL newline (brackets suppress layout), and so may the literal
    /// around it. Every token span the fragment produces is the **true physical source position**.
    ///
    /// That is a strictly stronger statement than what this test used to assert. The old lexer took
    /// a base COLUMN that kept counting past a newline, on purpose: resetting it put two sibling
    /// fragments' second lines both on `(line, 1)` — one witness-table key for two call sites, the
    /// second silently taking the first's type (`2a27697e`). Real positions get the non-aliasing
    /// property for free, from a property of the FILE rather than of an offset we chose, and without
    /// a column that runs off the end of its line (`docs/gaps.md` M24-6).
    #[test]
    fn a_fragment_spanning_a_newline_gets_real_source_positions() {
        // line 1:  s = """{[          line 2:  a, bb]}"""
        // cols:    1234567890                  1234567890
        let src = "s = \"\"\"{[\na, bb]}\"\"\"\n";
        let lit = tokenize(src)
            .unwrap()
            .into_iter()
            .find_map(|t| match t.kind {
                Token::Str(s) => Some(s),
                _ => None,
            })
            .expect("one str token");
        assert_eq!(lit.raw, "{[\na, bb]}");
        let map = lit.map.clone().expect("a braced literal carries a map");

        // The fragment is `[\na, bb]`, starting at content index 1 (past its `{`).
        let toks = tokenize_frag("[\na, bb]", map.clone(), 1).unwrap();
        let spans: Vec<(u32, u32)> = toks
            .iter()
            .filter(|t| matches!(t.kind, Token::Ident(_)))
            .map(|t| (t.span.line, t.span.col))
            .collect();
        assert_eq!(spans, vec![(2, 1), (2, 4)], "hand-counted from the source");

        // …and two fragments of ONE literal cannot collide, even on a line they share: content
        // index 3 is `a` at (2,1) and index 6 is the first `b` at (2,4) — distinct source chars.
        let a = tokenize_frag("a", map.clone(), 3).unwrap();
        let b = tokenize_frag("a", map.clone(), 6).unwrap();
        assert_ne!(a[0].span, b[0].span, "two fragments must stay distinct");
    }

    /// The only `str` token that carries a map is one that can actually spawn a fragment. This is
    /// the zero-cost claim, verified rather than asserted: an ordinary literal — including a
    /// multi-line triple-quoted one, whose map would be the least trivial — allocates nothing.
    #[test]
    fn pos_map_is_absent_for_a_plain_literal() {
        let one_map = |src: &str| {
            tokenize(src)
                .unwrap()
                .into_iter()
                .find_map(|t| match t.kind {
                    Token::Str(s) => Some(s),
                    _ => None,
                })
                .expect("one str token")
                .map
        };
        assert!(one_map("x = \"abc\"\n").is_none(), "plain literal");
        assert!(
            one_map("x = \"\"\"a\nb\"\"\"\n").is_none(),
            "brace-free triple-quoted literal"
        );
        // …and the moment a brace appears, the map is there.
        assert!(one_map("x = \"a{b}\"\n").is_some(), "braced literal");
    }

    /// `PosMap::at` is exact at the geometry `raw` throws away: the opening `"""` width, each escape
    /// width (`\u{1F600}` is NINE source chars for one content char), and a real newline resetting
    /// the column. Columns are hand-counted absolutes, not deltas — a uniformly-wrong-by-one map
    /// reads as correct to a human.
    #[test]
    fn pos_map_tracks_delimiter_escape_and_newline_widths() {
        // line 1:  s = """a\tb{p}          line 2:  c\u{1F600}d{q}"""
        // cols:    1234567890123                    1234567890123456789
        let src = "s = \"\"\"a\\tb{p}\nc\\u{1F600}d{q}\"\"\"\n";
        let lit = tokenize(src)
            .unwrap()
            .into_iter()
            .find_map(|t| match t.kind {
                Token::Str(s) => Some(s),
                _ => None,
            })
            .expect("one str token");
        assert_eq!(lit.raw, "a\tb{p}\nc\u{1F600}d{q}");
        let m = lit.map.expect("a braced literal carries a map");

        // content 0 = `a`, at col 8 (`s`,` `,`=`,` `,`"`,`"`,`"` are cols 1-7) — the `"""` width
        // costs nothing because checkpoint 0 is taken past the delimiter.
        assert_eq!((m.at(0).line, m.at(0).col), (1, 8));
        // content 1 = the tab, written `\t` at cols 9-10; content 2 = `b` at col 11.
        assert_eq!((m.at(1).line, m.at(1).col), (1, 9));
        assert_eq!((m.at(2).line, m.at(2).col), (1, 11));
        // content 3 = `{`, 4 = `p`, 5 = `}` at cols 12-14; content 6 is the real newline at col 15.
        assert_eq!((m.at(4).line, m.at(4).col), (1, 13));
        assert_eq!((m.at(6).line, m.at(6).col), (1, 15));
        // …and the next char restarts at column 1 of line 2.
        assert_eq!((m.at(7).line, m.at(7).col), (2, 1));
        // content 8 = 😀, written `\u{1F600}` at cols 2-10; content 9 = `d` at col 11.
        assert_eq!((m.at(8).line, m.at(8).col), (2, 2));
        assert_eq!((m.at(9).line, m.at(9).col), (2, 11));
        // content 11 = `q`, physically at col 13.
        assert_eq!((m.at(11).line, m.at(11).col), (2, 13));
    }

    /// The two-source-char escapes, in one literal, each costing exactly its own width: `\\` and
    /// `\"` join `\t` (above) — they are the ones an "offset into `raw`" column got wrong by one
    /// apiece, cumulatively.
    #[test]
    fn pos_map_tracks_two_char_escape_widths() {
        // s = "a\\b\"c{q}"
        // cols: 1234567890123456
        // `"`=5 `a`=6 `\`=7 `\`=8 `b`=9 `\`=10 `"`=11 `c`=12 `{`=13 `q`=14
        let src = "s = \"a\\\\b\\\"c{q}\"\n";
        let lit = tokenize(src)
            .unwrap()
            .into_iter()
            .find_map(|t| match t.kind {
                Token::Str(s) => Some(s),
                _ => None,
            })
            .expect("one str token");
        assert_eq!(lit.raw, "a\\b\"c{q}");
        let m = lit.map.expect("a braced literal carries a map");
        assert_eq!((m.at(0).line, m.at(0).col), (1, 6), "`a`");
        assert_eq!((m.at(1).line, m.at(1).col), (1, 7), "the `\\\\` backslash");
        assert_eq!((m.at(2).line, m.at(2).col), (1, 9), "`b`");
        assert_eq!((m.at(3).line, m.at(3).col), (1, 10), "the `\\\"` quote");
        assert_eq!((m.at(4).line, m.at(4).col), (1, 12), "`c`");
        assert_eq!((m.at(6).line, m.at(6).col), (1, 14), "`q`");
    }

    /// Columns are 1-based CHAR columns, not byte offsets — the lexer counts `chars`, and the map
    /// inherits that. A multi-byte char written literally (`é`, 2 bytes) and one written as an
    /// escape (`😀`, 4 bytes / 9 source chars) both cost exactly their SOURCE-char width.
    #[test]
    fn unicode_columns_are_chars_not_bytes() {
        let cols = |src: &str, n: usize| {
            let m = tokenize(src)
                .unwrap()
                .into_iter()
                .find_map(|t| match t.kind {
                    Token::Str(s) => s.map,
                    _ => None,
                })
                .expect("a braced literal carries a map");
            (m.at(n).line, m.at(n).col)
        };
        // s = "héllo {nope}"   — `"`=5 `h`=6 `é`=7 `l`=8 `l`=9 `o`=10 ` `=11 `{`=12 `n`=13
        assert_eq!(
            cols("s = \"héllo {nope}\"\n", 7),
            (1, 13),
            "`é` costs one col"
        );
        // s = "\u{1F600}{nope}" — `"`=5, the escape is cols 6-14, `{`=15, `n`=16
        assert_eq!(
            cols("s = \"\\u{1F600}{nope}\"\n", 2),
            (1, 16),
            "the escape costs its nine SOURCE chars"
        );
        // …and a 😀 written literally costs exactly one column: `"`=5 `😀`=6 `{`=7 `n`=8
        assert_eq!(
            cols("s = \"😀{nope}\"\n", 2),
            (1, 8),
            "a literal astral char costs one col"
        );
    }

    // W7-49 — `Span::file` is a cross-half TABLE KEY component (`KeywordKey`/`WitnessKey`/
    // `CarrierKey`), so what the stamp must guarantee is: `tokenize` (standalone/synthesized) is 0,
    // the base-position form carries whatever the caller assigned, and a re-lexed interpolation
    // fragment INHERITS its enclosing literal's file (a fragment belongs to its literal's module by
    // definition — if it reset to 0 the fragment keys would alias across modules again).
    #[test]
    fn span_file_is_stamped_by_the_base_position_lex_and_inherited_by_fragments() {
        let src = "x := 1\nfn f() -> int:\n    return x\n";

        // plain `tokenize` — the standalone/synthesized sentinel, 0 everywhere.
        for t in tokenize(src).unwrap() {
            assert_eq!(t.span.file, 0, "tokenize must stamp 0, got {:?}", t);
        }
        // file-stamped form — every span carries the caller's id.
        for t in Lexer::new_file(src, 7).tokenize().unwrap() {
            assert_eq!(t.span.file, 7, "new_file must stamp its file, got {:?}", t);
        }
        // …and so does the doc-comment/graph form the resolver uses.
        let (toks, _) = tokenize_with_comments("# d\nfn f(): 1\n", 5).unwrap();
        for t in toks {
            assert_eq!(t.span.file, 5, "tokenize_with_comments must stamp its file");
        }

        // An interpolation fragment inherits the enclosing literal's file.
        let lit_span = Span {
            line: 4,
            col: 11,
            file: 9,
        };
        let chunks =
            crate::interpolation::parse_interpolation(&StrLit::from("v={a + b}!"), lit_span)
                .expect("fragment should parse");
        let mut saw_expr = false;
        for c in &chunks {
            if let crate::ast::Chunk::Expr(e, _) = c {
                saw_expr = true;
                assert_eq!(
                    e.span.file, 9,
                    "fragment expr span must inherit the literal's file"
                );
            }
        }
        assert!(saw_expr, "expected one Expr chunk");
    }

    /// A `LexError` points at the offending CHARACTER, on both axes (`docs/gaps.md` M24-7).
    ///
    /// Every expected column below is hand-counted from the source, from the position rule the
    /// ancestors set (measured 2026-08-14, rustc 1.x):
    ///
    /// | family                              | position               | evidence            |
    /// |-------------------------------------|------------------------|---------------------|
    /// | bad char inside a `\u{…}` escape    | the offending char     | rustc `badu.rs:2:20`|
    /// | unknown / invalid escape            | the escape char        | rustc `num.rs:3:16` |
    /// | malformed number                    | the literal's start    | rustc `num.rs:2:13` |
    /// | …**except** a misplaced `'_'`       | the offending `_`      | CPython 3.14.6: `y = 1__0` → offset **6**, the first `_` (rustc has no opinion — it ACCEPTS `1__0`) |
    /// | unterminated string / triple / byte | the OPENING delimiter  | rustc `unterm.rs:2:13`, CPython |
    /// | a line continuation (`\` + newline) | the `\`                | no ancestor — rustc and CPython both SUPPORT it; Chezzi rejects, and points at the `\` that opens the unsupported escape |
    /// | an error about a `\u{…}` escape AS A WHOLE | the escape's `{` | Chezzi's own rule — see below |
    ///
    /// The unterminated cases also pin the LINE: the cursor has run to EOF, so reading the position
    /// off `self.pos` reported the line the file ENDS on, not the line the literal opens on.
    ///
    /// **The last row diverges from rustc by 1-2 columns, deliberately.** Measured rustc 1.97.0: the
    /// four whole-escape diagnostics (`"x\uZ"` incorrect, `"y\u{}"` empty, a 7-digit body overlong,
    /// `"w\u{D800}"` surrogate) all anchor at the `\` and CARET-SPAN the whole escape. `LexError`
    /// carries a point, not a span, so it names a char inside the escape instead: the offending char
    /// where there is one (the `Z`, the 7th digit), and otherwise the escape's opening `{` — the same
    /// "an unterminated delimiter points at its opener" rule the string families use. Every one is a
    /// real character on the right line and inside the construct the message is about, which is the
    /// bar; matching rustc's anchor exactly would need a span, not a column.
    #[test]
    fn lex_error_points_at_the_offending_character() {
        // (source, expected line, expected col, message substring)
        let cases: &[(&str, usize, usize, &str)] = &[
            // z := "ab\u{12zz}cd"   — cols: z1 ␠2 :3 =4 ␠5 "6 a7 b8 \9 u10 {11 1←12 2←13 z←14
            (
                "print(1)\nprint(2)\nz := \"ab\\u{12zz}cd\"\n",
                3,
                14,
                "invalid hex digit in unicode escape",
            ),
            // y := "abc   — the opening quote is col 6, and the line is 2 even though EOF is line 3
            ("x := 1\ny := \"abc\n", 2, 6, "unterminated string literal"),
            // y := """abc  — the opening triple starts at col 6
            (
                "x := 1\ny := \"\"\"abc\n",
                2,
                6,
                "unterminated triple-quoted string literal",
            ),
            // y := 0x   — the literal starts at col 6 (the `0`)
            ("x := 1\ny := 0x\n", 2, 6, "empty hexadecimal literal"),
            // blank line in between: the line axis still holds. d := 0b starts at col 6.
            ("a := 1\n\nb := 2\nd := 0b\n", 4, 6, "empty binary literal"),
            // y := "a\qb"   — cols: y1 ␠2 :3 =4 ␠5 "6 a7 \8 q←9
            ("x := 1\ny := \"a\\qb\"\n", 2, 9, "unknown escape '\\q'"),
            // y := b"a\qb"  — the `b` prefix shifts everything one right: \9 q←10
            ("x := 1\ny := b\"a\\qb\"\n", 2, 10, "unknown escape '\\q'"),
            // y := b"\xZZ"  — cols: y1 ␠2 :3 =4 ␠5 b6 "7 \8 x9 Z←10
            (
                "x := 1\ny := b\"\\xZZ\"\n",
                2,
                10,
                "invalid hex digit 'Z' in \\x escape",
            ),
            // y := r"abc   — a raw literal's opener is its `r`, at col 6
            (
                "x := 1\ny := r\"abc\n",
                2,
                6,
                "unterminated raw string literal",
            ),
            // y := ~   — the char itself, at col 6 (the cursor is already past it)
            ("x := 1\ny := ~\n", 2, 6, "unexpected character '~'"),
            // ----- the positions this change CHOSE (no prior measurement covered them) -----
            // A misplaced `_` in a number → the `_`, NOT the literal's start. Ancestor: CPython
            // 3.14.6, `y = 1__0` → `lineno 2 offset 6`, the FIRST underscore (measured; rustc has
            // no opinion here — it accepts `1__0` and emits nothing).
            // y := 1__0   — cols: y1 ␠2 :3 =4 ␠5 1←6 _←7
            (
                "x := 1\ny := 1__0\n",
                2,
                7,
                "'_' in a number must be between digits",
            ),
            // …and the radix body takes the same rule: CPython `y = 0b1__0` → offset 8, its first
            // underscore. y := 0b1__0 — cols: y1 ␠2 :3 =4 ␠5 0←6 b7 1←8 _←9
            (
                "x := 1\ny := 0b1__0\n",
                2,
                9,
                "'_' in a number must be between digits",
            ),
            // A line continuation → the `\`. No ancestor: rustc and CPython both SUPPORT `\`+newline
            // in a string; Chezzi rejects it, so the position is its own choice — the `\` that opens
            // the escape, which is where the construct starts.
            // y := "ab\⏎   — cols: y1 ␠2 :3 =4 ␠5 "6 a7 b8 \←9
            (
                "x := 1\ny := \"ab\\\ncd\"\n",
                2,
                9,
                "line continuations are not supported",
            ),
            // …inside a TRIPLE-quoted literal the `\` is on a later line than the opener, which is
            // the case that pins the line axis of this arm: the arm errors right after consuming the
            // newline, so `self.line`/`line_start` still describe the `\`'s own line.
            // line 3 is `cd\`   — cols: c1 d2 \←3
            (
                "x := 1\ny := \"\"\"ab\ncd\\\nef\"\"\"\n",
                3,
                3,
                "line continuations are not supported",
            ),
            // …and in a byte-string, where the `b` prefix shifts everything one right.
            // y := b"ab\⏎   — cols: y1 ␠2 :3 =4 ␠5 b6 "7 a8 b9 \←10
            (
                "x := 1\ny := b\"ab\\\ncd\"\n",
                2,
                10,
                "line continuations are not supported",
            ),
            // An error about a `\u{…}` escape AS A WHOLE → the escape's opening `{` (see the doc
            // comment: rustc anchors these at the `\` and spans the escape; a column can only name
            // one char, and the `{` is the escape body's opener).
            // y := "a\u{12   — cols: y1 ␠2 :3 =4 ␠5 "6 a7 \8 u9 {←10. No trailing newline: the
            // escape must hit EOF, or the newline is just a non-hex char and site #31 fires first.
            (
                "x := 1\ny := \"a\\u{12",
                2,
                10,
                "unterminated unicode escape",
            ),
            // y := "a\u{}b"  — same count: the `{` is col 10
            ("x := 1\ny := \"a\\u{}b\"\n", 2, 10, "empty unicode escape"),
            // y := "a\u{D800}b" — a surrogate; again the `{` at col 10
            (
                "x := 1\ny := \"a\\u{D800}b\"\n",
                2,
                10,
                "invalid unicode code point",
            ),
            // (The fourth whole-escape site, `invalid unicode escape: {e}`, is unreachable: its
            // `from_str_radix` is fed 1-6 chars already validated as ASCII hex, which always parses.
            // It shares `brace` with the three above by construction.)
        ];
        for (src, line, col, msg) in cases {
            let e = tokenize(src).expect_err(&format!("expected a lex error for {src:?}"));
            assert!(
                e.message.contains(msg),
                "wrong message for {src:?}: got {:?}, want {msg:?}",
                e.message
            );
            assert_eq!(
                (e.line, e.col),
                (*line, *col),
                "wrong position for {src:?} ({})",
                e.message
            );
        }
    }

    /// …and the `Display` carries both axes, exactly like every other diagnostic in the compiler
    /// (`impl Display for Span` is `line {}, col {}`).
    #[test]
    fn lex_error_display_carries_the_column() {
        let e = tokenize("x := 1\ny := 0x\n").unwrap_err();
        assert_eq!(
            e.to_string(),
            "lex error (line 2, col 6): empty hexadecimal literal"
        );
    }
}
