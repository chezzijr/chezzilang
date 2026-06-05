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
    Match,
    Import,
    From,
    As,
    And,
    Or,
    Not,
    True,
    False,

    // --- operators ---
    Plus,       // +
    Minus,      // -
    Star,       // *
    Slash,      // /
    Percent,    // %
    Assign,     // =
    Walrus,     // :=
    EqEq,       // ==
    NotEq,      // !=
    Lt,         // <
    LtEq,       // <=
    Gt,         // >
    GtEq,       // >=
    PlusEq,     // +=
    MinusEq,    // -=
    Arrow,      // ->
    Pipe,       // |>
    Question,   // ?
    Bang,       // !  (used in type position: `T!` = Result[T])

    // --- delimiters ---
    LParen,     // (
    RParen,     // )
    LBracket,   // [
    RBracket,   // ]
    Comma,      // ,
    Colon,      // :
    Dot,        // .
    DotDot,     // ..  (range, e.g. 0..10)

    // --- layout (the interesting part) ---
    Newline,    // end of a logical line
    Indent,     // increase in indentation
    Dedent,     // decrease in indentation
    Eof,        // end of input
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
        "match" => Token::Match,
        "import" => Token::Import,
        "from" => Token::From,
        "as" => Token::As,
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
    pos: usize,        // index of the next char to read
    line: usize,       // current line, 1-based (for error messages)
    line_start: usize, // char index where the current line begins (for column tracking)
    indents: Vec<usize>, // the indentation stack. Starts as vec![0].
    at_line_start: bool, // true when the next char begins a fresh logical line
    pending: VecDeque<Tok>, // layout tokens computed together but emitted one-per-call (Dedents)
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

        // === STEP A: start-of-line indentation (1h) ===
        if self.at_line_start {
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
                return Ok(Tok { kind: Token::Newline, span });
            }
            // 2. ...then emit one Dedent per still-open indent level...
            if *self.indents.last().unwrap() > 0 {
                self.indents.pop();
                return Ok(Tok { kind: Token::Dedent, span });
            }
            // 3. ...and finally Eof.
            return Ok(Tok { kind: Token::Eof, span });
        }

        // === STEP D: newline ===
        // (Blank/comment-only lines never reach here — STEP A's scan_indentation skips them,
        // so when we see '\n' we're always ending a line that had real content.)
        if self.peek() == '\n' {
            let span = self.span_at(self.pos);
            self.advance();
            self.line += 1;
            self.line_start = self.pos;
            self.at_line_start = true;
            return Ok(Tok { kind: Token::Newline, span });
        }

        // === STEP E: a real token starts here ===
        self.at_line_start = false;
        let start = self.pos; // index of the first char of this lexeme
        let span = self.span_at(start);
        let c = self.advance();
        let kind = match c {
            // single- and two-char operators (LEARN: match_char does the 2-char lookahead)
            '+' => if self.match_char('=') { Token::PlusEq } else { Token::Plus },
            '-' => {
                if self.match_char('>') {
                    Token::Arrow
                } else if self.match_char('=') {
                    Token::MinusEq
                } else {
                    Token::Minus
                }
            }
            '*' => Token::Star,
            '/' => Token::Slash,
            '%' => Token::Percent,
            ':' => if self.match_char('=') { Token::Walrus } else { Token::Colon },
            '=' => if self.match_char('=') { Token::EqEq } else { Token::Assign },
            '!' => if self.match_char('=') { Token::NotEq } else { Token::Bang },
            '<' => if self.match_char('=') { Token::LtEq } else { Token::Lt },
            '>' => if self.match_char('=') { Token::GtEq } else { Token::Gt },
            '|' => {
                if self.match_char('>') {
                    Token::Pipe
                } else {
                    return Err(self.error("expected '>' after '|'"));
                }
            }
            '?' => Token::Question,
            '(' => Token::LParen,
            ')' => Token::RParen,
            '[' => Token::LBracket,
            ']' => Token::RBracket,
            ',' => Token::Comma,
            '.' => if self.match_char('.') { Token::DotDot } else { Token::Dot },

            // delegate the "munching" token kinds to the helpers below
            '"' => self.string()?,
            c if c.is_ascii_digit() => self.number(start)?,
            c if c.is_alphabetic() || c == '_' => self.identifier(start),

            other => return Err(self.error(&format!("unexpected character {other:?}"))),
        };
        Ok(Tok { kind, span })
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
                return Ok(Some(vec![Tok { kind: Token::Indent, span }]));
            } else if width < top {
                // shallower → close as many levels as needed, one Dedent each
                let mut dedents = Vec::new();
                while *self.indents.last().unwrap() > width {
                    self.indents.pop();
                    dedents.push(Tok { kind: Token::Dedent, span });
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
            None => Token::Ident(identifier)
        }
    }

    /// (1c) Scan a number literal (int, or float if it has a single '.'). First digit already
    /// consumed; `start` is its index. `_` is allowed as a digit-group separator
    /// (`10_000_000`) but only **between two digits** — leading/trailing/doubled/dot-adjacent
    /// underscores are a `LexError`.
    fn number(&mut self, start: usize) -> Result<Token, LexError> {
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

        let word: Vec<char> = self.chars[start..self.pos].to_vec();
        // Validate underscores: every '_' must be flanked by a digit on both sides.
        for (i, &c) in word.iter().enumerate() {
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

    /// (1d) Scan a string literal. The opening `"` is already consumed.
    ///
    /// Munches up to the closing `"`, translating backslash escapes (`\n \t \r \\ \" \0`). An
    /// unknown escape, or a `\` at end-of-input, is a `LexError`. The stored contents are the
    /// *processed* text (escapes resolved, quotes stripped); brace interpolation (`{…}`) is a
    /// separate, later pass in the interpreter — not handled here.
    fn string(&mut self) -> Result<Token, LexError> {
        let mut text = String::new();
        while !self.is_at_end() && self.peek() != '"' {
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
                    '0' => '\0',
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
        self.advance(); // consume the closing '"'
        Ok(Token::Str(text))
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
        assert_eq!(kinds("1 # this is ignored"), vec![Token::Int(1), Token::Newline, Token::Eof]);
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
            vec![Token::Str("a\nb\tc\\d\"e".to_string()), Token::Newline, Token::Eof]
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

    #[test]
    fn trailing_backslash_is_unterminated() {
        // "abc\  with no closing quote
        assert!(tokenize("\"abc\\").is_err());
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
            vec![Token::Int(0), Token::DotDot, Token::Int(10), Token::Newline, Token::Eof]
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
}
