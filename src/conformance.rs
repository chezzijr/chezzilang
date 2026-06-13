//! Grammar conformance harness (test-only).
//!
//! Treats `docs/grammar.bnf` as the canonical grammar and proves the hand-written parser agrees
//! with it. The grammar is *executed* with the `bnf` crate (an Earley parser, dev-dependency) and
//! differential-tested against `parser::parse` on the corpus under `tests/corpus/`.
//!
//! The grammar is written over the lexer's token stream: terminals are token *classes*
//! (`WALRUS`, `IDENT`, `NEWLINE`, …). `bnf` matches terminals character-by-character with no
//! whitespace tokenization, so we feed it one unique private-use char per token and substitute the
//! same char for each terminal name in the grammar — keeping `docs/grammar.bnf` human-readable
//! while the engine sees an unambiguous symbol stream.

use crate::lexer::{tokenize, Token};
use crate::parser::parse;
use std::collections::{BTreeMap, BTreeSet};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

fn read(rel: &str) -> String {
    std::fs::read_to_string(format!("{ROOT}/{rel}"))
        .unwrap_or_else(|e| panic!("cannot read {rel}: {e}"))
}

// ----- token class name for every Token (exhaustive — the compiler enforces completeness) -----

/// The grammar terminal name for a token. Exhaustive match: adding a `Token` variant without a
/// mapping here is a compile error, so this bridge can never silently fall behind the enum.
fn symbol(tok: &Token) -> &'static str {
    match tok {
        Token::Int(_) => "INT",
        Token::Float(_) => "FLOAT",
        Token::Str(_) => "STR",
        Token::Ident(_) => "IDENT",
        Token::Fn => "FN",
        Token::Return => "RETURN",
        Token::If => "IF",
        Token::Else => "ELSE",
        Token::For => "FOR",
        Token::While => "WHILE",
        Token::In => "IN",
        Token::Break => "BREAK",
        Token::Continue => "CONTINUE",
        Token::Struct => "STRUCT",
        Token::Enum => "ENUM",
        Token::Protocol => "PROTOCOL",
        Token::Type => "TYPE",
        Token::Match => "MATCH",
        Token::Recover => "RECOVER",
        Token::Defer => "DEFER",
        Token::Spawn => "SPAWN",
        Token::Parallel => "PARALLEL",
        Token::Wait => "WAIT",
        Token::Yield => "YIELD",
        Token::Import => "IMPORT",
        Token::Extern => "EXTERN",
        Token::From => "FROM",
        Token::As => "AS",
        Token::And => "AND",
        Token::Or => "OR",
        Token::Not => "NOT",
        Token::True => "TRUE",
        Token::False => "FALSE",
        Token::Plus => "PLUS",
        Token::Minus => "MINUS",
        Token::Star => "STAR",
        Token::Slash => "SLASH",
        Token::Percent => "PERCENT",
        Token::Assign => "ASSIGN",
        Token::Walrus => "WALRUS",
        Token::EqEq => "EQEQ",
        Token::NotEq => "NOTEQ",
        Token::Lt => "LT",
        Token::LtEq => "LTEQ",
        Token::Gt => "GT",
        Token::GtEq => "GTEQ",
        Token::PlusEq => "PLUSEQ",
        Token::MinusEq => "MINUSEQ",
        Token::Arrow => "ARROW",
        Token::Pipe => "PIPE",
        Token::Amp => "AMP",
        Token::Caret => "CARET",
        Token::BitOr => "BITOR",
        Token::Shl => "SHL",
        Token::Shr => "SHR",
        Token::Question => "QUESTION",
        Token::QuestionDot => "QUESTIONDOT",
        Token::QuestionQuestion => "QUESTIONQUESTION",
        Token::Bang => "BANG",
        Token::LParen => "LPAREN",
        Token::RParen => "RPAREN",
        Token::LBracket => "LBRACKET",
        Token::RBracket => "RBRACKET",
        Token::LBrace => "LBRACE",
        Token::RBrace => "RBRACE",
        Token::Comma => "COMMA",
        Token::Colon => "COLON",
        Token::Dot => "DOT",
        Token::DotDot => "DOTDOT",
        Token::Newline => "NEWLINE",
        Token::Indent => "INDENT",
        Token::Dedent => "DEDENT",
        Token::Eof => "EOF",
    }
}

// ----- reading the canonical grammar -----

/// Strip `#` comments, then join wrapped alternative lines so each `<rule> ::= …` is one line
/// (a line is a new rule iff it contains `::=`; everything else continues the current rule).
fn normalize_grammar(raw: &str) -> String {
    let mut rules: Vec<String> = Vec::new();
    for line in raw.lines() {
        let line = match line.find('#') {
            Some(i) => &line[..i],
            None => line,
        };
        if line.trim().is_empty() {
            continue;
        }
        if line.contains("::=") {
            rules.push(line.trim().to_string());
        } else if let Some(last) = rules.last_mut() {
            last.push(' ');
            last.push_str(line.trim());
        }
    }
    rules.join("\n")
}

/// Distinct terminal names ("FOO") referenced anywhere in the grammar text.
fn grammar_terminals(grammar: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let bytes = grammar.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"'
            && let Some(end) = grammar[i + 1..].find('"')
        {
            out.insert(grammar[i + 1..i + 1 + end].to_string());
            i += end + 2;
            continue;
        }
        i += 1;
    }
    out
}

/// Nonterminal names (`<name>`) that appear on the left of `::=`.
fn grammar_rule_names(grammar: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in grammar.lines() {
        if let Some(eq) = line.find("::=") {
            let lhs = line[..eq].trim();
            if let Some(name) = lhs.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
                out.insert(name.to_string());
            }
        }
    }
    out
}

/// The token-class names, derived from the `Token` enum variants in the lexer source so the check
/// is tied to the real enum (variant `Walrus` → class `WALRUS`).
fn token_classes_from_source() -> BTreeSet<String> {
    let src = read("src/lexer/mod.rs");
    let start = src.find("pub enum Token {").expect("Token enum");
    let body = &src[start..];
    let mut out = BTreeSet::new();
    for line in body.lines().skip(1) {
        let t = line.trim();
        if t.starts_with('}') {
            break;
        }
        if t.starts_with("//") || t.is_empty() {
            continue;
        }
        let name: String = t
            .chars()
            .take_while(|c| c.is_alphanumeric())
            .collect();
        if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
            out.insert(name.to_uppercase());
        }
    }
    out
}

/// name → unique private-use char, over the canonical token classes.
fn symbol_chars() -> BTreeMap<String, char> {
    token_classes_from_source()
        .into_iter()
        .enumerate()
        .map(|(i, name)| (name, char::from_u32(0xE000 + i as u32).unwrap()))
        .collect()
}

/// Parse the grammar, substituting terminal names for their chars, and build an Earley parser.
fn engine_grammar(chars: &BTreeMap<String, char>) -> bnf::Grammar {
    let mut text = normalize_grammar(&read("docs/grammar.bnf"));
    for (name, ch) in chars {
        text = text.replace(&format!("\"{name}\""), &format!("\"{ch}\""));
    }
    text.parse::<bnf::Grammar>()
        .unwrap_or_else(|e| panic!("docs/grammar.bnf is not valid BNF: {e}"))
}

/// Encode a source string as the engine's symbol stream (one char per token). `None` if it doesn't
/// even lex (an upstream failure the grammar can't model).
fn encode(src: &str, chars: &BTreeMap<String, char>) -> Option<String> {
    let toks = tokenize(src).ok()?;
    Some(toks.iter().map(|t| chars[symbol(&t.kind)]).collect())
}

// ----- corpus loading -----

struct Case {
    name: String,
    src: String,
    rules: Vec<String>,
    expect: Option<String>,
}

fn load_corpus(dir: &str) -> Vec<Case> {
    let mut cases = Vec::new();
    let path = format!("{ROOT}/tests/corpus/{dir}");
    let mut entries: Vec<_> = std::fs::read_dir(&path)
        .unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "chz"))
        .collect();
    entries.sort();
    for p in entries {
        let src = std::fs::read_to_string(&p).unwrap();
        let mut rules = Vec::new();
        let mut expect = None;
        for line in src.lines() {
            let t = line.trim();
            if let Some(r) = t.strip_prefix("# rule:") {
                rules = r.split(',').map(|s| s.trim().to_string()).collect();
            } else if let Some(e) = t.strip_prefix("# expect:") {
                expect = Some(e.trim().to_string());
            }
        }
        cases.push(Case {
            name: p.file_name().unwrap().to_string_lossy().into_owned(),
            src,
            rules,
            expect,
        });
    }
    cases
}

// ===== tests =====

#[test]
fn grammar_is_valid_bnf() {
    let chars = symbol_chars();
    let _ = engine_grammar(&chars); // panics if the grammar doesn't parse
}

/// Every terminal in the grammar is a real token class, and every token class appears in the
/// grammar (PIPE joined the cascade with the M6 pipe operator).
#[test]
fn terminals_match_token_enum() {
    let classes = token_classes_from_source();
    let terminals = grammar_terminals(&normalize_grammar(&read("docs/grammar.bnf")));

    let unknown: Vec<_> = terminals.difference(&classes).collect();
    assert!(unknown.is_empty(), "grammar terminals not in Token enum: {unknown:?}");

    let missing: Vec<_> = classes.difference(&terminals).collect();
    assert!(missing.is_empty(), "every token class must appear in the grammar; missing: {missing:?}");
}

/// Grammar nonterminals correspond to parser functions (and vice versa), within a documented map.
#[test]
fn parser_rules_match_fns() {
    // grammar rule -> parser fn
    let rule_to_fn: BTreeMap<&str, &str> = [
        ("module", "parse_module"),
        ("stmt", "parse_stmt"),
        ("fnDecl", "parse_fn"),
        ("params", "parse_params"),
        ("structDecl", "parse_struct"),
        ("enumDecl", "parse_enum"),
        ("protocolDecl", "parse_protocol"),
        ("externDecl", "parse_extern"),
        ("externFn", "parse_extern_fn"),
        ("typeAliasDecl", "parse_type_alias"),
        ("typeParams", "parse_type_params"),
        ("bound", "parse_bound"),
        ("fnSig", "parse_fn_sig"),
        ("ifStmt", "parse_if"),
        ("forStmt", "parse_for"),
        ("compClause", "parse_comp_clause"),
        ("whileStmt", "parse_while"),
        ("matchStmt", "parse_match"),
        ("pattern", "parse_pattern"),
        ("patternPrimary", "parse_pattern_primary"),
        ("subpattern", "parse_subpattern"),
        ("tuplePattern", "parse_tuple_pattern"),
        ("returnStmt", "parse_return"),
        ("yieldStmt", "parse_yield"),
        ("deferStmt", "parse_defer"),
        ("parallelStmt", "parse_parallel"),
        ("spawnStmt", "parse_spawn"),
        ("waitStmt", "parse_wait"),
        ("importStmt", "parse_import"),
        ("dottedPath", "parse_dotted_path"),
        ("block", "parse_block"),
        ("type", "parse_type"),
        ("expr", "parse_expr"),
    ]
    .into_iter()
    .collect();

    // parser fns with no 1:1 grammar rule: the Pratt cascade + structural helpers + test helpers.
    let helper_fns: BTreeSet<&str> = [
        "parse_simple_stmt",
        "parse_pattern_impl",
        "parse_bp",
        "parse_unary",
        "parse_postfix",
        "parse_subscript",
        "parse_call_args",
        "parse_type_postfix",
        "parse_primary",
        "parse_closure",
        "parse_match_expr",
        "parse_if_expr",
        "parse_recover_expr",
        "parse_ok",
        "parse_err",
    ]
    .into_iter()
    .collect();

    let rules = grammar_rule_names(&normalize_grammar(&read("docs/grammar.bnf")));
    let src = read("src/parser/mod.rs");
    let fns: BTreeSet<String> = src
        .match_indices("fn parse_")
        .map(|(i, _)| {
            src[i + 3..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect()
        })
        .collect();

    // every mapping target exists, on both sides
    for (rule, func) in &rule_to_fn {
        assert!(rules.contains(*rule), "RULE_TO_FN references missing grammar rule '{rule}'");
        assert!(fns.contains(*func), "RULE_TO_FN references missing parser fn '{func}'");
    }
    // every parser fn is either mapped or an allowlisted helper
    let mapped: BTreeSet<&str> = rule_to_fn.values().copied().collect();
    for f in &fns {
        assert!(
            mapped.contains(f.as_str()) || helper_fns.contains(f.as_str()),
            "parser fn '{f}' is neither mapped to a grammar rule nor allowlisted — update the map"
        );
    }
}

/// Every `# rule:` annotation names a real grammar rule, and the headline constructs are covered.
#[test]
fn corpus_covers_the_grammar() {
    let rules = grammar_rule_names(&normalize_grammar(&read("docs/grammar.bnf")));
    let mut covered = BTreeSet::new();
    for case in load_corpus("accept") {
        for r in case.rules {
            assert!(rules.contains(&r), "{}: '# rule: {r}' is not a grammar rule", case.name);
            covered.insert(r);
        }
    }
    let required = [
        "letStmt", "assignStmt", "returnStmt", "importStmt", "fnDecl", "structDecl", "enumDecl",
        "ifStmt", "forStmt", "whileStmt", "matchStmt", "closure", "type", "postfix", "rangeExpr",
        "bound",
    ];
    for r in required {
        assert!(covered.contains(r), "no accept corpus file exercises '{r}'");
    }
}

/// THE core check: for every corpus file the executable grammar and the hand parser must agree on
/// accept/reject. Accept files must be accepted by both; reject files rejected by both.
#[test]
fn grammar_and_parser_agree() {
    let chars = symbol_chars();
    let grammar = engine_grammar(&chars);
    let engine = grammar.build_parser().expect("build Earley parser");

    let check = |dir: &str, should_accept: bool| {
        for case in load_corpus(dir) {
            let hand_ok = match tokenize(&case.src) {
                Ok(toks) => parse(toks).is_ok(),
                Err(_) => false, // lex failure is upstream of the grammar; treat as reject
            };
            let engine_ok = match encode(&case.src, &chars) {
                Some(s) => engine.parse_input(&s).next().is_some(),
                None => false,
            };
            assert_eq!(
                hand_ok, engine_ok,
                "{dir}/{}: hand parser {} but grammar {}",
                case.name,
                if hand_ok { "accepted" } else { "rejected" },
                if engine_ok { "accepted" } else { "rejected" },
            );
            assert_eq!(
                hand_ok, should_accept,
                "{dir}/{}: expected the parser to {} this file",
                case.name,
                if should_accept { "accept" } else { "reject" },
            );
        }
    };

    check("accept", true);
    check("reject", false);
}

/// Reject files must fail with the specific error named in their `# expect:` annotation (the
/// grammar engine only yields accept/reject, so messages are checked against the real parser).
#[test]
fn reject_messages_are_specific() {
    for case in load_corpus("reject") {
        let want = case.expect.unwrap_or_else(|| panic!("{}: missing '# expect:'", case.name));
        let toks = tokenize(&case.src).expect("reject corpus should still lex");
        let err = parse(toks).expect_err(&format!("{} should fail to parse", case.name));
        assert!(
            err.message.contains(&want),
            "{}: error '{}' does not contain expected '{want}'",
            case.name,
            err.message
        );
    }
}
