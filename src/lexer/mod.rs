//! Lexer (a.k.a. scanner): turns source text into a flat stream of `Token`s.
//!
//! This is YOUR implementation task (M1). The types, struct, and a couple of worked-example
//! helpers are provided so you can focus on the real learning: the scanning state machine and,
//! the tricky part, indentation → INDENT/DEDENT tokens.
//!
//! Fill in every `todo!(...)`. Follow the `// HINT:` and `// LEARN:` comments.
//! Run `cargo test` to check yourself against the guiding tests at the bottom.
//!
//! See PROGRESS.md for the ordered sub-steps (1a … 1k).

use std::{collections::VecDeque, fmt};

/// A single lexical token. This enum is provided as reference — you don't need to invent it.
/// (Spans/line numbers are intentionally omitted for M1 to keep it simple; we add them later.)
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // --- literals ---
    Int(i64),
    Float(f64),
    Str(String), // contents only, quotes stripped. Interpolation handled later, not in M1.
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
    For,
    While,
    In,
    Break,
    Continue,
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
    From,
    As,
    /// `ref` — the by-reference binding modifier (`r: ref int = 0`, `fn f(x: ref int)`).
    /// A full keyword (corpus-safe: no `.chz` uses `ref` as a bare identifier). Legal only as a
    /// type prefix in the two binding positions; the parser rejects it everywhere else.
    Ref,
    And,
    Or,
    Not,
    True,
    False,

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
    LParen,   // (
    RParen,   // )
    LBracket, // [
    RBracket, // ]
    LBrace,   // {  (map literal)
    RBrace,   // }
    Comma,    // ,
    Colon,    // :
    Dot,      // .
    DotDot,   // ..  (range, e.g. 0..10)

    // --- layout (the interesting part) ---
    Newline, // end of a logical line
    Indent,  // increase in indentation
    Dedent,  // decrease in indentation
    Eof,     // end of input
}

/// Source location of a token: 1-based line and 1-based column (column counts characters
/// from the start of the line). Added in M2 so the parser can point at exact positions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Span {
    pub line: usize,
    pub col: usize,
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}, col {}", self.line, self.col)
    }
}

/// A token paired with where it came from. The lexer emits these; the `Token` payload is the
/// `kind`. (`tokens` CLI output prints only `kind`, so M1 output is unchanged.)
#[derive(Debug, Clone, PartialEq)]
pub struct Tok {
    pub kind: Token,
    pub span: Span,
}

/// Error returned when the source can't be tokenized.
#[derive(Debug, Clone, PartialEq)]
pub struct LexError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lex error (line {}): {}", self.line, self.message)
    }
}

/// Look up whether an identifier is actually a keyword.
/// HINT (sub-step 1e): call this after you've scanned a whole word.
fn keyword(word: &str) -> Option<Token> {
    let tok = match word {
        "fn" => Token::Fn,
        "return" => Token::Return,
        "if" => Token::If,
        "else" => Token::Else,
        "for" => Token::For,
        "while" => Token::While,
        "in" => Token::In,
        "break" => Token::Break,
        "continue" => Token::Continue,
        "struct" => Token::Struct,
        "enum" => Token::Enum,
        "protocol" => Token::Protocol,
        "type" => Token::Type,
        "newtype" => Token::NewType,
        "match" => Token::Match,
        "recover" => Token::Recover,
        "defer" => Token::Defer,
        "assert" => Token::Assert,
        "test" => Token::Test,
        "spawn" => Token::Spawn,
        "parallel" => Token::Parallel,
        "wait" => Token::Wait,
        "yield" => Token::Yield,
        "import" => Token::Import,
        "extern" => Token::Extern,
        "from" => Token::From,
        "as" => Token::As,
        "ref" => Token::Ref,
        "and" => Token::And,
        "or" => Token::Or,
        "not" => Token::Not,
        "true" => Token::True,
        "false" => Token::False,
        _ => return None,
    };
    Some(tok)
}

/// The lexer. Holds the source as a list of chars plus a cursor.
///
/// LEARN: we collect into `Vec<char>` so indexing is by *character*, not byte. Rust `String`
/// is UTF-8 and you can't index it by character cheaply. For a learning lexer, `Vec<char>` is
/// the simplest correct choice. (A perf-tuned lexer would scan bytes — a later optimization.)
pub struct Lexer {
    chars: Vec<char>,
    pos: usize,             // index of the next char to read
    line: usize,            // current line, 1-based (for error messages)
    line_start: usize,      // char index where the current line begins (for column tracking)
    indents: Vec<usize>,    // the indentation stack. Starts as vec![0].
    at_line_start: bool,    // true when the next char begins a fresh logical line
    pending: VecDeque<Tok>, // layout tokens computed together but emitted one-per-call (Dedents)
    bracket_depth: usize, // nesting depth of (), [], {}; >0 suppresses layout (multi-line literals)
}

impl Lexer {
    pub fn new(source: &str) -> Self {
        Lexer {
            chars: source.chars().collect(),
            pos: 0,
            line: 1,
            line_start: 0,
            indents: vec![0],
            at_line_start: true,
            pending: VecDeque::new(),
            bracket_depth: 0,
        }
    }

    /// The span pointing at character index `pos` on the current line.
    fn span_at(&self, pos: usize) -> Span {
        Span {
            line: self.line,
            col: pos - self.line_start + 1,
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
                        Token::DotDot
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

                other => return Err(self.error(&format!("unexpected character {other:?}"))),
            };
            return Ok(Tok { kind, span });
        }
    }

    /// Small helper to build a `LexError` at the current line.
    fn error(&self, message: &str) -> LexError {
        LexError {
            line: self.line,
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
            // 3. comment-only line → does not count, skip to its newline
            if self.peek() == '#' {
                while !self.is_at_end() && self.peek() != '\n' {
                    self.advance();
                }
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

    // ----- YOUR helpers (the real M1 learning) -----

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
                    return Err(self.error(&format!("empty {name} literal")));
                }
                // Underscores: only between two valid digits (mirrors decimal rule).
                let is_digit = |c: char| c.is_digit(radix);
                for (i, &c) in body.iter().enumerate() {
                    if c == '_' {
                        let prev_ok = i > 0 && is_digit(body[i - 1]);
                        let next_ok = body.get(i + 1).is_some_and(|n| is_digit(*n));
                        if !(prev_ok && next_ok) {
                            return Err(self.error("'_' in a number must be between digits"));
                        }
                    }
                }
                let digits: String = body.into_iter().filter(|c| *c != '_').collect();
                let v = i64::from_str_radix(&digits, radix)
                    .map_err(|e| self.error(&format!("invalid {name} literal: {e}")))?;
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
                    return Err(self.error("'_' in a number must be between digits"));
                }
            }
        }

        let num: String = word.into_iter().filter(|c| *c != '_').collect();
        if is_float {
            let v = num.parse::<f64>().map_err(|e| self.error(&e.to_string()))?;
            Ok(Token::Float(v))
        } else {
            let v = num.parse::<i64>().map_err(|e| self.error(&e.to_string()))?;
            Ok(Token::Int(v))
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
        while !self.is_at_end() && self.peek() != quote {
            if self.peek() == '\\' {
                self.advance(); // consume the backslash
                if self.is_at_end() {
                    return Err(self.error("unterminated string literal (trailing '\\')"));
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
                    '\n' | '\r' => {
                        return Err(self.error(
                            "line continuations are not supported; close the string or use \\n",
                        ));
                    }
                    other => return Err(self.error(&format!("unknown escape '\\{other}'"))),
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
            return Err(self.error("unterminated string literal"));
        }
        self.advance(); // consume the closing quote
        Ok(Token::Str(text))
    }

    /// (1d′) Scan a *triple-quoted* string literal (`"""…"""` or `'''…'''`). The opening triple
    /// `quote` is already consumed. Identical to [`string`] — same backslash escapes, `\u{…}`, and
    /// literal-newline handling, leaving `{…}` interpolation to the later pass — except the
    /// terminator is a triple of `quote`, so a single or double `quote` inside is an ordinary char.
    /// Produces a normal `Token::Str`.
    fn triple_string(&mut self, quote: char) -> Result<Token, LexError> {
        let mut text = String::new();
        // Closes only when the next THREE chars are all `quote`.
        while !(self.peek() == quote
            && self.peek_next() == quote
            && self.chars.get(self.pos + 2) == Some(&quote))
        {
            if self.is_at_end() {
                return Err(self.error("unterminated triple-quoted string literal"));
            }
            if self.peek() == '\\' {
                self.advance(); // consume the backslash
                if self.is_at_end() {
                    return Err(self.error("unterminated string literal (trailing '\\')"));
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
                    '\n' | '\r' => {
                        return Err(self.error(
                            "line continuations are not supported; close the string or use \\n",
                        ));
                    }
                    other => return Err(self.error(&format!("unknown escape '\\{other}'"))),
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
        Ok(Token::Str(text))
    }

    /// Scan an `r"..."` / `r'...'` raw-string literal. The `r`/`R` prefix and the opening `quote`
    /// are already consumed. Identical to [`string`] EXCEPT every char is pushed verbatim — there is
    /// NO backslash-escape branch, so `\n` is two chars (backslash, n), `\` is a literal backslash,
    /// and a brace `{`/`}` is an ordinary char (the later interpolation pass never sees `RawStr`).
    /// The short form cannot contain the closing `quote` (no escaping — use the other quote style or
    /// the triple form). Produces a [`Token::RawStr`].
    fn raw_string(&mut self, quote: char) -> Result<Token, LexError> {
        let mut text = String::new();
        while !self.is_at_end() && self.peek() != quote {
            if self.peek() == '\n' {
                self.line += 1; // a *literal* newline → multi-line string; keep line count honest
                self.line_start = self.pos + 1; // next char begins the new line
            }
            text.push(self.advance());
        }
        if self.is_at_end() {
            return Err(self.error("unterminated raw string literal"));
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
        // Closes only when the next THREE chars are all `quote`.
        while !(self.peek() == quote
            && self.peek_next() == quote
            && self.chars.get(self.pos + 2) == Some(&quote))
        {
            if self.is_at_end() {
                return Err(self.error("unterminated triple-quoted raw string literal"));
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
        while !self.is_at_end() && self.peek() != quote {
            if self.peek() == '\\' {
                self.byte_escape(&mut bytes)?;
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
            return Err(self.error("unterminated byte-string literal"));
        }
        self.advance(); // closing quote
        Ok(Token::Bytes(bytes))
    }

    /// Triple-quoted byte literal (`b"""…"""` / `b'''…'''`). Same escapes as [`byte_string`]; closes
    /// only on a triple of `quote`, so a lone quote inside is an ordinary byte.
    fn byte_triple_string(&mut self, quote: char) -> Result<Token, LexError> {
        let mut bytes = Vec::new();
        while !(self.peek() == quote
            && self.peek_next() == quote
            && self.chars.get(self.pos + 2) == Some(&quote))
        {
            if self.is_at_end() {
                return Err(self.error("unterminated triple-quoted byte-string literal"));
            }
            if self.peek() == '\\' {
                self.byte_escape(&mut bytes)?;
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
            Err(self.error("non-ASCII byte in byte literal; use \\xHH escape"))
        }
    }

    /// Process one backslash escape inside a byte literal. The cursor sits on the `\`.
    fn byte_escape(&mut self, bytes: &mut Vec<u8>) -> Result<(), LexError> {
        self.advance(); // consume the backslash
        if self.is_at_end() {
            return Err(self.error("unterminated byte-string literal (trailing '\\')"));
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
            'u' => {
                return Err(self.error("\\u not allowed in a byte literal; use \\xHH"));
            }
            '\n' | '\r' => {
                return Err(self
                    .error("line continuations are not supported; close the literal or use \\n"));
            }
            other => return Err(self.error(&format!("unknown escape '\\{other}'"))),
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
        c.to_digit(16)
            .map(|d| d as u8)
            .ok_or_else(|| self.error(&format!("invalid hex digit '{c}' in {who} escape")))
    }

    /// Scan the body of a `\u{HEX}` escape. The `\u` is already consumed; the cursor sits on
    /// what must be `{`. Reads 1-6 hex digits naming a Unicode scalar value and returns it.
    /// Rejects a missing `{`, an empty `{}`, more than 6 hex digits, any non-hex char, an
    /// unterminated brace, and invalid code points (surrogates D800-DFFF, > 10FFFF).
    fn unicode_escape(&mut self) -> Result<char, LexError> {
        if !self.match_char('{') {
            return Err(self.error("expected '{' after \\u in unicode escape"));
        }
        let mut digits = String::new();
        loop {
            if self.is_at_end() {
                return Err(self.error("unterminated unicode escape"));
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
        if digits.is_empty() {
            return Err(self.error("empty unicode escape"));
        }
        let cp = u32::from_str_radix(&digits, 16)
            .map_err(|e| self.error(&format!("invalid unicode escape: {e}")))?;
        char::from_u32(cp).ok_or_else(|| self.error("invalid unicode code point"))
    }

    // ----- you'll likely add private helpers below as you go -----
    // e.g. fn number(&mut self) -> Result<Token, LexError>
    //      fn string(&mut self) -> Result<Token, LexError>
    //      fn identifier(&mut self) -> Token
    //      fn handle_line_start(&mut self) -> ...   // measures indent, pushes/pops self.indents
    //
    // HINT (1h) — the indentation algorithm, the trickiest part of M1:
    //   Keep an indent stack, starting `vec![0]`.
    //   At the START of each logical (non-blank, non-comment-only) line:
    //     1. Count leading spaces → `width`. (Pick spaces-only for v1; reject tabs with a LexError.)
    //     2. If width > top of stack: push width, emit ONE `Indent`.
    //     3. If width < top: pop while top > width, emitting ONE `Dedent` per pop.
    //          If after popping the top != width → indentation error ("inconsistent dedent").
    //     4. If width == top: emit nothing.
    //   Blank lines and comment-only lines produce NO Newline and NO indent change — skip them.
    //   At EOF: emit a `Dedent` for every indent level still on the stack above 0, then `Eof`.
    //   Because one source position can require emitting several Dedents, a common trick is a
    //   small queue/counter field that `next_token` drains before scanning more source.
}

/// Convenience free function: `lexer::tokenize(src)`.
pub fn tokenize(source: &str) -> Result<Vec<Tok>, LexError> {
    Lexer::new(source).tokenize()
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
                Token::Str("a".to_string()),
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
        assert_eq!(y.span, Span { line: 2, col: 5 });

        // `if` is the first token: line 1, column 1.
        assert_eq!(toks[0].span, Span { line: 1, col: 1 });
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
                Token::Str("a\nb\tc\\d\"e".to_string()),
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn string_escape_nul() {
        assert_eq!(
            kinds(r#""x\0y""#),
            vec![Token::Str("x\0y".to_string()), Token::Newline, Token::Eof]
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
            vec![Token::Str("A".to_string()), Token::Newline, Token::Eof]
        );
        assert_eq!(
            kinds(r#""\u{e9}""#),
            vec![Token::Str("é".to_string()), Token::Newline, Token::Eof]
        );
        assert_eq!(
            kinds(r#""\u{1F600}""#),
            vec![Token::Str("😀".to_string()), Token::Newline, Token::Eof]
        );
        // surrounded by ordinary text
        assert_eq!(
            kinds(r#""x\u{41}y""#),
            vec![Token::Str("xAy".to_string()), Token::Newline, Token::Eof]
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
            vec![Token::Str("hello".to_string()), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn single_quote_equals_double() {
        // Same escape handling: `\n` in a single-quoted string resolves identically.
        assert_eq!(kinds(r"'a\nb'"), kinds(r#""a\nb""#));
        // and the new \u{} escape works in single quotes too
        assert_eq!(
            kinds(r"'\u{41}'"),
            vec![Token::Str("A".to_string()), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn single_quote_inner_double_literal() {
        // A `"` inside a single-quoted string is a literal char (no escape needed).
        assert_eq!(
            kinds(r#"'say "hi"'"#),
            vec![
                Token::Str("say \"hi\"".to_string()),
                Token::Newline,
                Token::Eof
            ]
        );
    }

    #[test]
    fn single_quote_escapes() {
        // `\'` escapes the closing quote inside a single-quoted string.
        assert_eq!(
            kinds(r"'it\'s'"),
            vec![Token::Str("it's".to_string()), Token::Newline, Token::Eof]
        );
    }

    #[test]
    fn double_quote_single_literal() {
        // A `'` inside a double-quoted string is a literal char.
        assert_eq!(
            kinds(r#""it's""#),
            vec![Token::Str("it's".to_string()), Token::Newline, Token::Eof]
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
        assert_eq!(z.span, Span { line: 2, col: 4 });
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
            vec![
                &Token::Str("x{y}z".to_string()),
                &Token::Newline,
                &Token::Eof
            ]
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
}
