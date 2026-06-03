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

use std::fmt;

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

    // --- delimiters ---
    LParen,     // (
    RParen,     // )
    LBracket,   // [
    RBracket,   // ]
    Comma,      // ,
    Colon,      // :
    Dot,        // .

    // --- layout (the interesting part) ---
    Newline,    // end of a logical line
    Indent,     // increase in indentation
    Dedent,     // decrease in indentation
    Eof,        // end of input
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
    indents: Vec<usize>, // the indentation stack. HINT (1h): starts as vec![0].
    // You may want more fields as you go (e.g. a flag for "at start of line").
    // Add them here when you need them.
}

impl Lexer {
    pub fn new(source: &str) -> Self {
        Lexer {
            chars: source.chars().collect(),
            pos: 0,
            line: 1,
            indents: vec![0],
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
        todo!("1a: return the char at self.pos + 1, or '\\0' if out of range")
    }

    /// Consume the current char and return it. Remember to advance `self.pos`.
    /// HINT: read the char first, then bump `self.pos`, then return it.
    fn advance(&mut self) -> char {
        todo!("1a: read self.peek(), advance self.pos by 1, return the char")
    }

    /// If the current char equals `expected`, consume it and return true. Otherwise false.
    /// LEARN: this is the classic trick for two-char operators like `:=`, `==`, `->`.
    fn match_char(&mut self, expected: char) -> bool {
        todo!("1a: if not at end and peek() == expected, advance() and return true; else false")
    }

    // ----- the main driver -----

    /// Produce all tokens. This outer loop is provided so you can see how the pieces connect:
    /// it just calls `next_token` until it returns `Eof`. The real work is in `next_token`
    /// (and the indentation handling it triggers).
    pub fn tokenize(mut self) -> Result<Vec<Token>, LexError> {
        let mut tokens = Vec::new();
        loop {
            let tok = self.next_token()?;
            let done = tok == Token::Eof;
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
    ///        digit         → number (1c)
    ///        '"'           → string (1d)
    ///        letter or '_' → identifier / keyword (1e)
    ///        else          → operator / delimiter (1b)
    ///
    /// Build this up sub-step by sub-step. Start tiny: get `+` and `Eof` working, run the
    /// first guiding test, then add more. Don't try to write it all at once.
    fn next_token(&mut self) -> Result<Token, LexError> {
        todo!("M1: implement the scanner core — see the LEARN note above and PROGRESS.md")
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
pub fn tokenize(source: &str) -> Result<Vec<Token>, LexError> {
    Lexer::new(source).tokenize()
}

#[cfg(test)]
mod tests {
    use super::*;

    // GUIDING TESTS — make these pass one at a time. They define "correct" for M1.
    // Start with `single_plus`, then work down. Add your own as you go.
    //
    // NOTE: every token stream should END with `Token::Eof`. Most logical lines end with
    // `Token::Newline`. Adjust these expectations if you and Claude agree on different layout
    // rules — they're a starting contract, not gospel.

    #[test]
    fn single_plus() {
        let toks = tokenize("+").unwrap();
        assert_eq!(toks, vec![Token::Plus, Token::Newline, Token::Eof]);
    }

    #[test]
    fn two_char_operators() {
        let toks = tokenize(":= == -> |>").unwrap();
        assert_eq!(
            toks,
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
        let toks = tokenize("x := 42").unwrap();
        assert_eq!(
            toks,
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
        let toks = tokenize("1 # this is ignored").unwrap();
        assert_eq!(toks, vec![Token::Int(1), Token::Newline, Token::Eof]);
    }

    #[test]
    fn indentation_emits_indent_dedent() {
        // Two logical lines; the second is indented under the first.
        let src = "if x:\n    y\n";
        let toks = tokenize(src).unwrap();
        assert_eq!(
            toks,
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
}
