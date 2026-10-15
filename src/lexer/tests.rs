//! Lexer unit tests. Moved out of `src/lexer/mod.rs`; `super::*` is that module.

use super::*;

/// Lex `src` and return just the token kinds (dropping spans) — keeps the layout
/// assertions below readable now that the lexer emits `Tok { kind, span }`.
fn kinds(src: &str) -> Vec<Token> {
    tokenize(src).unwrap().into_iter().map(|t| t.kind).collect()
}

/// The `open_layout` result and the kinds after the first `Colon` of `src`.
fn after_first_colon(src: &str) -> (Result<bool, LexError>, Vec<Token>) {
    let mut toks = tokenize(src).unwrap();
    let colon = toks.iter().position(|t| t.kind == Token::Colon).unwrap();
    let r = open_layout(&mut toks, colon);
    let rest = toks[colon + 1..].iter().map(|t| t.kind.clone()).collect();
    (r, rest)
}

#[test]
fn open_layout_opens_a_block_inside_parens() {
    let (r, rest) = after_first_colon("f(match 1:\n    1: 5\n    _: 6)");
    assert_eq!(r, Ok(true));
    assert_eq!(
        rest,
        vec![
            Token::Newline,
            Token::Indent,
            Token::Int(1),
            Token::Colon,
            Token::Int(5),
            Token::Newline,
            Token::Ident("_".into()),
            Token::Colon,
            Token::Int(6),
            Token::Newline,
            Token::Dedent,
            Token::RParen,
            Token::Newline,
            Token::Eof
        ]
    );
}

#[test]
fn open_layout_closes_at_a_dedented_line() {
    let (r, rest) = after_first_colon("f(x = match 1:\n    _: 6\n)");
    assert_eq!(r, Ok(true));
    let tail: Vec<Token> = rest[rest.len() - 5..].to_vec();
    assert_eq!(
        tail,
        vec![
            Token::Newline,
            Token::Dedent,
            Token::RParen,
            Token::Newline,
            Token::Eof
        ]
    );
}

#[test]
fn open_layout_declines_a_same_line_body() {
    let mut toks = tokenize("f(match 1: 2)").unwrap();
    let before = toks.clone();
    let colon = toks.iter().position(|t| t.kind == Token::Colon).unwrap();
    assert_eq!(open_layout(&mut toks, colon), Ok(false));
    assert_eq!(toks, before);
}

#[test]
fn map_literal_line_end_colon_has_no_layout() {
    let ks = kinds("m := {\"a\":\n    1}");
    assert!(!ks.contains(&Token::Indent), "{ks:?}");
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
    // `\n` escape is a real newline; a literal newline is preserved; `{x}` is a hole, lexed
    // in place (the text before it keeps its resolved escape).
    let ks = kinds("\"\"\"a\\nb {x}\"\"\"");
    let [Token::Str(lit), Token::Newline, Token::Eof] = &ks[..] else {
        panic!("unexpected tokens {ks:?}");
    };
    let [StrPart::Text(text), StrPart::Hole(hole)] = &lit.parts[..] else {
        panic!("expected text then a hole, got {lit:?}");
    };
    assert_eq!(text, "a\nb ");
    let hole_kinds: Vec<&Token> = hole.tokens.iter().map(|t| &t.kind).collect();
    assert_eq!(hole_kinds, vec![&Token::Ident("x".into()), &Token::Eof]);
    assert!(hole.spec.is_none());
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
fn leading_zero_decimal_literal_rejected() {
    // W8-29: `0755` must not silently lex as decimal 755 — CPython/rustc both reject a
    // leading-zero decimal literal outright.
    assert!(tokenize("0755").is_err(), "leading-zero decimal literal");
}

#[test]
fn leading_zero_message_names_the_octal_fix() {
    let err = tokenize("0755").unwrap_err();
    assert_eq!(
        err.message,
        "leading zeros in a decimal integer literal are not permitted; write 0o755 for octal or 755 for decimal"
    );
    let err = tokenize("0999").unwrap_err();
    assert_eq!(
        err.message,
        "leading zeros in a decimal integer literal are not permitted; write 999 for decimal"
    );
}

#[test]
fn leading_zero_rule_accepts_zeros_floats_and_radix() {
    for src in ["0", "00", "0_0", "0.5", "0755.5", "0755e2", "0o755", "0x1f"] {
        assert!(tokenize(src).is_ok(), "expected {src} to lex ok");
    }
}

#[test]
fn bad_radix_digit_errors() {
    assert!(tokenize("0xG").is_err(), "non-hex digit");
    assert!(tokenize("0b2").is_err(), "non-binary digit");
    assert!(tokenize("0o8").is_err(), "non-octal digit");
    assert!(tokenize("0x").is_err(), "empty hex body");
}

// TICKET-144 (W14-34): CPython/Go/Rust accept ONE underscore right after a radix prefix
// (`0x_ff` = 255); a doubled or trailing one stays an error. Replaces the old `0x_FF`-is-an-error
// pin in `bad_radix_digit_errors`, which recorded the divergence this ticket removes.
#[test]
fn radix_literal_accepts_underscore_after_prefix() {
    for (src, n) in [("0x_ff", 255), ("0b_1", 1), ("0o_7", 7), ("0X_Ff", 255)] {
        assert_eq!(
            kinds(src),
            vec![Token::Int(n), Token::Newline, Token::Eof],
            "{src}"
        );
    }
    for src in ["0x__ff", "0x_", "0b__1", "0x_g"] {
        let e = tokenize(src).expect_err(src);
        assert!(
            e.message.contains("'_' in a number must be between digits"),
            "{src}: {}",
            e.message
        );
    }
}

#[test]
fn bare_zero_still_decimal() {
    assert_eq!(kinds("0"), vec![Token::Int(0), Token::Newline, Token::Eof]);
    // W8-29: `007` is now a rejected leading-zero decimal literal, same class as `0755`.
    assert!(tokenize("007").is_err());
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
    assert!(matches!(
        ks[..],
        [Token::Str(_), Token::Newline, Token::Eof]
    ));
    assert!(!ks.contains(&&Token::LBrace) && !ks.contains(&&Token::RBrace));
    // The hole's own tokens sit inside the literal's payload, and its braces are not among them.
    let Token::Str(lit) = ks[0] else {
        panic!("a str token");
    };
    let [StrPart::Text(x), StrPart::Hole(hole), StrPart::Text(z)] = &lit.parts[..] else {
        panic!("expected text, hole, text, got {lit:?}");
    };
    assert_eq!((x.as_str(), z.as_str()), ("x", "z"));
    let hole_kinds: Vec<&Token> = hole.tokens.iter().map(|t| &t.kind).collect();
    assert_eq!(hole_kinds, vec![&Token::Ident("y".into()), &Token::Eof]);
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
fn newtype_is_an_ordinary_identifier() {
    assert_eq!(
        kinds("newtype"),
        vec![
            Token::Ident("newtype".to_string()),
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
        "fn", "return", "if", "else", "elif", "for", "while", "in", "break", "continue", "pass",
        "struct", "enum", "protocol", "type", "match", "recover", "defer", "assert", "test",
        "spawn", "parallel", "wait", "yield", "import", "extern", "from", "as", "and", "or", "not",
        "true", "false", "None",
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

/// The one `str` token of `src`, as the lexer built it.
fn str_lit(src: &str) -> StrLit {
    tokenize(src)
        .unwrap()
        .into_iter()
        .find_map(|t| match t.kind {
            Token::Str(s) => Some(s),
            _ => None,
        })
        .expect("one str token")
}

/// Every identifier token in `lit`'s holes (and nested spec fields) as `(name, line, col)`.
fn hole_idents(lit: &StrLit) -> Vec<(String, u32, u32)> {
    let mut out = Vec::new();
    let mut take = |toks: &[Tok]| {
        for t in toks {
            if let Token::Ident(n) = &t.kind {
                out.push((n.clone(), t.span.line, t.span.col));
            }
        }
    };
    for part in &lit.parts {
        if let StrPart::Hole(h) = part {
            take(&h.tokens);
            for sp in h.spec.iter().flatten() {
                if let SpecPart::Field(toks) = sp {
                    take(toks);
                }
            }
        }
    }
    out
}

/// A hole may contain a REAL newline (a hole is one bracket deep, so layout is off), and so may
/// the literal around it. Every token span the hole produces is the **true physical source
/// position**, after escapes and real newlines alike: the lexer lexed it in place, so there is
/// no map to compose. Two holes never share a span (a span is a cross-half TABLE KEY, see
/// [`Span`]). Columns are hand-counted absolutes.
#[test]
fn hole_tokens_are_lexed_in_place_with_real_spans() {
    // line 1:  s = """{[          line 2:  a, bb]}"""
    // cols:    1234567890                  1234567890
    let lit = str_lit("s = \"\"\"{[\na, bb]}\"\"\"\n");
    assert_eq!(
        hole_idents(&lit),
        vec![("a".into(), 2, 1), ("bb".into(), 2, 4)],
        "hand-counted from the source"
    );
    // line 1:  s = """a\tb{p}          line 2:  c\u{1F600}d{q}"""
    // cols:    1234567890123                    1234567890123456789
    let lit = str_lit("s = \"\"\"a\\tb{p}\nc\\u{1F600}d{q}\"\"\"\n");
    assert_eq!(
        hole_idents(&lit),
        vec![("p".into(), 1, 13), ("q".into(), 2, 13)],
        "after a `\\t`, a real newline and a nine-char escape"
    );
    // s = "a\\b\"c{q}"   `"`=5 `a`=6 `\`=7 `\`=8 `b`=9 `\`=10 `"`=11 `c`=12 `{`=13 `q`=14
    let lit = str_lit("s = \"a\\\\b\\\"c{q}\"\n");
    assert_eq!(hole_idents(&lit), vec![("q".into(), 1, 14)]);
    // Columns are 1-based CHAR columns: `é` costs one, `\u{1F600}` its nine source chars, and a
    // literal 😀 one.
    let lit = str_lit("s = \"héllo {nope}\"\n");
    assert_eq!(hole_idents(&lit), vec![("nope".into(), 1, 13)]);
    let lit = str_lit("s = \"\\u{1F600}{nope}\"\n");
    assert_eq!(hole_idents(&lit), vec![("nope".into(), 1, 16)]);
    let lit = str_lit("s = \"😀{nope}\"\n");
    assert_eq!(hole_idents(&lit), vec![("nope".into(), 1, 8)]);
    // Two holes of one literal, and a nested literal's hole, are three distinct positions.
    let lit = str_lit("s = \"{a}{b}{'{c}'}\"\n");
    let at: Vec<_> = hole_idents(&lit).iter().map(|h| (h.1, h.2)).collect();
    assert_eq!(at.len(), 2, "the nested literal's hole is inside a token");
    assert_ne!(at[0], at[1]);
}

/// A hole is one bracket level: a line break inside it is white space, never a `Newline`, and
/// the hole's tokens carry no layout token at all (they end with `Eof`).
#[test]
fn a_hole_is_one_bracket_deep() {
    for src in [
        "s = \"{x +\n 1}\"\n",
        "s = \"\"\"{x\n + 1}\"\"\"\n",
        "s = \"{x\n  .f()\n  .g()}\"\n",
    ] {
        let lit = str_lit(src);
        let [StrPart::Hole(h)] = &lit.parts[..] else {
            panic!("one hole in {src:?}, got {lit:?}");
        };
        for t in &h.tokens {
            assert!(
                !matches!(t.kind, Token::Newline | Token::Indent | Token::Dedent),
                "layout token {:?} in a hole of {src:?}",
                t.kind
            );
        }
        assert_eq!(h.tokens.last().map(|t| &t.kind), Some(&Token::Eof));
    }
    // …and the line AFTER the literal still starts a fresh logical line.
    let kinds: Vec<Token> = tokenize("s = \"{x\n}\"\nt = 1\n")
        .unwrap()
        .into_iter()
        .map(|t| t.kind)
        .collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == Token::Newline).count(),
        2,
        "one Newline per statement, none from the hole: {kinds:?}"
    );
}

/// A literal with no hole is plain (one `Text` or none); one with a hole is not, and adjacent
/// text merges so equal values are equal tokens. The extent counts the delimiters.
#[test]
fn a_literal_without_a_hole_is_plain() {
    assert_eq!(str_lit("x = \"abc\"\n").plain(), Some("abc"));
    assert_eq!(str_lit("x = \"\"\n").plain(), Some(""));
    assert_eq!(str_lit("x = \"\"\"a\nb\"\"\"\n").plain(), Some("a\nb"));
    // `{{`, `}}` and an all-digit hole (DEC-018) are text, merged with their neighbours.
    assert_eq!(str_lit("x = \"a{{b}}c{4}d\"\n").plain(), Some("a{b}c{4}d"));
    assert!(str_lit("x = \"a{b}\"\n").plain().is_none());
    assert_eq!(str_lit("x = \"abc\"\n").len, 5);
    assert_eq!(str_lit("x = \"\"\"a\\nb\"\"\"\n").len, 10);
    assert_eq!(str_lit("x = \"a{'}}'}\"\n").len, 9);
}

/// A hole may nest another interpolated literal, to [`MAX_HOLE_NEST`] levels and no deeper.
#[test]
fn hole_nesting_is_capped() {
    let nested = |levels: usize| {
        let mut src = String::from("x");
        for _ in 0..levels {
            src = format!("\"{{{src}}}\"");
        }
        format!("s = {src}\n")
    };
    assert!(
        tokenize(&nested(MAX_HOLE_NEST)).is_ok(),
        "{MAX_HOLE_NEST} levels"
    );
    let e = tokenize(&nested(MAX_HOLE_NEST + 1)).unwrap_err();
    assert_eq!(e.message, "too many nested interpolated strings");
}

/// The `:` that starts a format spec is the first one at hole depth; inside a bracket, a
/// nested literal or a nested field it is not, and `:=` at hole depth is spec `=`.
#[test]
fn a_spec_starts_at_the_first_colon_at_hole_depth() {
    let spec_of = |src: &str| -> Option<Vec<SpecPart>> {
        let lit = str_lit(src);
        let [StrPart::Hole(h)] = &lit.parts[..] else {
            panic!("one hole in {src:?}");
        };
        h.spec.clone()
    };
    assert_eq!(spec_of("s = \"{x}\"\n"), None);
    assert_eq!(spec_of("s = \"{x[1..2]}\"\n"), None);
    assert_eq!(spec_of("s = \"{m[':']}\"\n"), None);
    assert_eq!(
        spec_of("s = \"{x:>4}\"\n"),
        Some(vec![SpecPart::Text(">4".into())])
    );
    assert_eq!(
        spec_of("s = \"{x:=5}\"\n"),
        Some(vec![SpecPart::Text("=5".into())]),
        "`:=` at hole depth is a spec, not a walrus"
    );
    // Quotes and brackets in a spec are fill chars; an escape resolves once.
    assert_eq!(
        spec_of("s = \"{x:'>4}\"\n"),
        Some(vec![SpecPart::Text("'>4".into())])
    );
    assert_eq!(
        spec_of("s = \"{x:\\u{7B}>4}\"\n"),
        Some(vec![SpecPart::Text("{>4".into())])
    );
}

// W7-49 — `Span::file` is a cross-half TABLE KEY component (`WitnessKey`/
// `CarrierKey`), so what the stamp must guarantee is: `tokenize` (standalone/synthesized) is 0,
// the base-position form carries whatever the caller assigned, and a hole's tokens carry the
// module's file too (a hole belongs to its literal's module by definition — if it reset to 0
// the fragment keys would alias across modules again).
#[test]
fn span_file_is_stamped_by_the_lex_and_carried_by_hole_tokens() {
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

    // A hole's tokens come from the same lexer, so they carry the module's file.
    let (toks, _) = tokenize_with_comments("s = \"v={a + b}!\"\n", 9).unwrap();
    let lit = toks
        .into_iter()
        .find_map(|t| match t.kind {
            Token::Str(s) => Some(s),
            _ => None,
        })
        .expect("one str token");
    let mut saw_hole = false;
    for part in &lit.parts {
        if let StrPart::Hole(h) = part {
            saw_hole = true;
            for t in &h.tokens {
                assert_eq!(
                    t.span.file, 9,
                    "hole token {t:?} must carry the literal's file"
                );
            }
        }
    }
    assert!(saw_hole, "expected one hole");
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

/// A leading UTF-8 BOM (`U+FEFF`) is a file-entry artifact from Windows/VS Code editors, not
/// source text — Python and Go both strip a leading one. Chezzi currently lexes it as an
/// ordinary character and faults (TICKET-058).
#[test]
fn leading_bom_is_not_a_lex_error() {
    let toks = tokenize("\u{feff}print(1)\n");
    assert!(
        toks.is_ok(),
        "expected a leading BOM to be stripped, got {:?}",
        toks.err()
    );
}

/// A BOM anywhere but the very start of the file is program text, not a file-entry artifact —
/// CPython raises the same `SyntaxError` on a mid-file BOM (measured 2026-09-03).
#[test]
fn a_mid_file_bom_is_still_a_lex_error() {
    let e = tokenize("print(\n\u{feff}x)\n").expect_err("mid-file BOM must still lex-error");
    assert_eq!((e.line, e.col), (2, 1));
}

/// Only the FIRST leading BOM is a file-entry artifact; a second one stays a lex error, so the
/// strip must be `strip_prefix` (removes exactly one), never `trim_start_matches`.
#[test]
fn a_second_leading_bom_is_still_a_lex_error() {
    let e = tokenize("\u{feff}\u{feff}print(1)\n")
        .expect_err("a second leading BOM must still lex-error");
    assert_eq!((e.line, e.col), (1, 1));
}

/// A hole is lexed in place, not as a file — a BOM there is program text a user typed inside
/// `{...}`, so it stays a lex error even though the file that contains it was stripped once at
/// entry.
#[test]
fn a_bom_inside_a_hole_is_still_a_lex_error() {
    let e = tokenize("s = \"{\u{feff}x}\"\n").expect_err("a BOM in a hole must lex-error");
    assert!(
        e.message.starts_with("unexpected character"),
        "{}",
        e.message
    );
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

/// `render_span` with no resolvable path falls back to the historical `Display` form, byte for
/// byte — `path == None` covers both "no table entry" and `span.file == 0` (the synthetic
/// single-module compile path), and neither should ever print anything but this.
#[test]
fn render_span_falls_back_without_a_path() {
    let span = Span {
        line: 3,
        col: 11,
        file: 0,
    };
    assert_eq!(render_span(span, None), "line 3, col 11");
    assert_eq!(render_span(span, None), span.to_string());
}

/// `render_span` with a resolvable path renders `path:line:col`, relativized to the process cwd
/// when the path lives under it, and left absolute otherwise (so a `std/` or out-of-tree module
/// is never ambiguous).
#[test]
fn render_span_emits_path_line_col() {
    let span = Span {
        line: 6,
        col: 5,
        file: 1,
    };
    // Under the cwd: relativized.
    let cwd = std::env::current_dir().unwrap();
    let under = cwd.join("main.chz");
    assert_eq!(render_span(span, Some(&under)), "main.chz:6:5");

    // Not under the cwd: printed as-is (absolute).
    let outside = std::path::Path::new("/definitely/outside/anything.chz");
    assert_eq!(
        render_span(span, Some(outside)),
        "/definitely/outside/anything.chz:6:5"
    );
}

#[test]
fn render_snippet_draws_caret_under_span() {
    let src = "a := 1\nprint(xs)\n";
    let span = Span {
        line: 2,
        col: 7,
        file: 0,
    };
    let out = render_snippet(span, src).expect("line exists");
    assert!(out.contains("2 | print(xs)"), "got: {out}");
    let caret_line = out.lines().last().unwrap();
    assert_eq!(caret_line.trim_end(), caret_line);
    assert!(caret_line.trim_end().ends_with("^^"), "got: {out}");
}

#[test]
fn render_snippet_off_end_is_none() {
    let src = "a := 1\nb := 2\n";
    let span = Span {
        line: 9,
        col: 1,
        file: 0,
    };
    assert_eq!(render_snippet(span, src), None);
}

#[test]
fn render_snippet_line_zero_is_none() {
    let src = "a := 1";
    let span = Span {
        line: 0,
        col: 1,
        file: 0,
    };
    assert_eq!(render_snippet(span, src), None);
}

#[test]
fn render_snippet_tab_keeps_columns() {
    let src = "\tx := 1\n";
    let span = Span {
        line: 1,
        col: 2,
        file: 0,
    };
    let out = render_snippet(span, src).expect("line exists");
    assert!(out.contains("1 |  x := 1"), "got: {out}");
    let caret_line = out.lines().last().unwrap();
    // one leading space before the caret (col 2 == char index 1)
    assert!(caret_line.ends_with(" ^"), "got: {out}");
}
