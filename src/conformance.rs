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

use crate::lexer::{Token, tokenize};
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
        // Same grammar terminal as a normal integer literal — the i64::MIN-magnitude carve-out is a
        // lexer/parser value concern, not a distinct grammar production, so it maps to INT (no drift).
        Token::IntMinMagnitude => "INT",
        Token::Float(_) => "FLOAT",
        Token::Str(_) => "STR",
        Token::Bytes(_) => "BYTES",
        Token::RawStr(_) => "RAWSTR",
        Token::Ident(_) => "IDENT",
        Token::Fn => "FN",
        Token::Return => "RETURN",
        Token::If => "IF",
        Token::Else => "ELSE",
        Token::Elif => "ELIF",
        Token::For => "FOR",
        Token::While => "WHILE",
        Token::In => "IN",
        Token::Break => "BREAK",
        Token::Continue => "CONTINUE",
        Token::Pass => "PASS",
        Token::Struct => "STRUCT",
        Token::Enum => "ENUM",
        Token::Protocol => "PROTOCOL",
        Token::Type => "TYPE",
        Token::Match => "MATCH",
        Token::Recover => "RECOVER",
        Token::Defer => "DEFER",
        Token::Assert => "ASSERT",
        Token::Test => "TEST",
        Token::Spawn => "SPAWN",
        Token::Parallel => "PARALLEL",
        Token::Wait => "WAIT",
        Token::Yield => "YIELD",
        Token::Import => "IMPORT",
        Token::Extern => "EXTERN",
        Token::Native => "NATIVE",
        Token::From => "FROM",
        Token::As => "AS",
        Token::Const => "CONST",
        Token::And => "AND",
        Token::Or => "OR",
        Token::Not => "NOT",
        Token::True => "TRUE",
        Token::False => "FALSE",
        Token::NoneKw => "NONEKW",
        Token::Where => "WHERE",
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
        Token::StarEq => "STAREQ",
        Token::SlashEq => "SLASHEQ",
        Token::PercentEq => "PERCENTEQ",
        Token::AmpEq => "AMPEQ",
        Token::PipeEq => "PIPEEQ",
        Token::CaretEq => "CARETEQ",
        Token::ShlEq => "SHLEQ",
        Token::ShrEq => "SHREQ",
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
        Token::DotDotDot => "DOTDOTDOT",
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
        let name: String = t.chars().take_while(|c| c.is_alphanumeric()).collect();
        // `IntMinMagnitude` is a value carve-out for the i64::MIN literal, not a distinct grammar
        // terminal — `symbol()` folds it into `INT`, so it never appears as its own terminal in
        // grammar.bnf. Skip it here so the bidirectional terminals⇔classes check stays exact.
        if name == "IntMinMagnitude" {
            continue;
        }
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
    assert!(
        unknown.is_empty(),
        "grammar terminals not in Token enum: {unknown:?}"
    );

    let missing: Vec<_> = classes.difference(&terminals).collect();
    assert!(
        missing.is_empty(),
        "every token class must appear in the grammar; missing: {missing:?}"
    );
}

/// Grammar nonterminals correspond to parser functions (and vice versa), within a documented map.
#[test]
fn parser_rules_match_fns() {
    // grammar rule -> parser fn
    let rule_to_fn: BTreeMap<&str, &str> = [
        ("module", "parse_module"),
        ("item", "parse_item"),
        ("stmt", "parse_stmt"),
        ("fnDecl", "parse_fn"),
        ("params", "parse_params"),
        ("structDecl", "parse_struct"),
        ("enumDecl", "parse_enum"),
        ("protocolDecl", "parse_protocol"),
        ("externDecl", "parse_extern"),
        ("externFn", "parse_extern_fn"),
        ("cVarParams", "parse_params_ext"),
        ("nativeDecl", "parse_native"),
        ("nativeStructDecl", "parse_native_struct"),
        ("nativeEnumDecl", "parse_native_enum"),
        ("nativeTypeDecl", "parse_native_type"),
        ("typeAliasDecl", "parse_type_alias"),
        ("typeParams", "parse_type_params"),
        ("whereClause", "parse_where_bounds"),
        ("bound", "parse_bound"),
        ("fnSig", "parse_fn_sig"),
        ("ifStmt", "parse_if"),
        ("forStmt", "parse_for"),
        ("compClause", "parse_comp_clause"),
        ("compClauses", "parse_comp_clauses"),
        ("whileStmt", "parse_while"),
        ("matchStmt", "parse_match"),
        ("pattern", "parse_pattern"),
        ("patternPrimary", "parse_pattern_primary"),
        ("subpattern", "parse_subpattern"),
        ("tuplePattern", "parse_tuple_pattern"),
        ("returnStmt", "parse_return"),
        ("elseGuard", "parse_else_guard"),
        ("yieldStmt", "parse_yield"),
        ("deferStmt", "parse_defer"),
        ("assertStmt", "parse_assert"),
        ("testFnDecl", "parse_test_fn"),
        ("parallelStmt", "parse_parallel"),
        ("spawnStmt", "parse_spawn"),
        ("spawnCallStmt", "parse_spawn_call"),
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
        // `parse_stmt_in` is `parse_stmt` with the inline-body flag; `parse_line_stmt` is the
        // `<simpleStmt>` dispatch, and each alternative it reaches has its own rule.
        "parse_stmt_in",
        "parse_line_stmt",
        // `parse_native_decl` is the `native …` DISPATCH helper (out-of-lined so its StmtKind-sized
        // call slots stay off the recursive `parse_stmt` frame); it routes to `parse_native` /
        // `parse_native_struct` / `parse_native_enum`, each with its own grammar rule — it has none.
        "parse_native_decl",
        "parse_pattern_impl",
        "parse_bp",
        "parse_unary",
        "parse_postfix",
        "parse_subscript",
        "parse_bracket",
        "parse_call_args",
        "parse_type_postfix",
        // `parse_type_body` is `parse_type`'s body, out-of-lined so the wrapper can own the depth +
        // fold-scope bookkeeping in one place; the `<type>` rule is `parse_type`'s.
        "parse_type_body",
        // `parse_fn_type_param` parses ONE `fn(...)` type parameter with its optional Swift-style
        // label (`<fnParam>` in the grammar) — a structural helper of `parse_type`, no distinct rule.
        "parse_fn_type_param",
        "parse_primary",
        "parse_closure",
        "parse_match_expr",
        "parse_if_expr",
        "parse_recover_expr",
        "parse_ok",
        "parse_err",
        // `parse_with_docs` is the doc-comment-carrying entry point (same grammar as `parse`, plus the
        // lexer's doc-comment side-channel); it maps to no distinct grammar rule.
        "parse_with_docs",
        // `parse_dotted_path_spanned` is the span-tracking sibling of `parse_dotted_path` (same
        // `dottedPath` grammar; it only also returns the last-segment span for the import bound-name
        // hover) — a structural helper with no distinct grammar rule.
        "parse_dotted_path_spanned",
        // `parse_recv_chan` parses the RHS of a recv `wait:` arm (the bare `chan.recv()`) — a
        // structural helper of `parse_wait`, part of the `<waitArm>` rule, no distinct rule of its own.
        "parse_recv_chan",
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
        assert!(
            rules.contains(*rule),
            "RULE_TO_FN references missing grammar rule '{rule}'"
        );
        assert!(
            fns.contains(*func),
            "RULE_TO_FN references missing parser fn '{func}'"
        );
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
            assert!(
                rules.contains(&r),
                "{}: '# rule: {r}' is not a grammar rule",
                case.name
            );
            covered.insert(r);
        }
    }
    let required = [
        "letStmt",
        "assignStmt",
        "returnStmt",
        "importStmt",
        "fnDecl",
        "structDecl",
        "enumDecl",
        "ifStmt",
        "forStmt",
        "whileStmt",
        "matchStmt",
        "closure",
        "type",
        "postfix",
        "rangeExpr",
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
                hand_ok,
                engine_ok,
                "{dir}/{}: hand parser {} but grammar {}",
                case.name,
                if hand_ok { "accepted" } else { "rejected" },
                if engine_ok { "accepted" } else { "rejected" },
            );
            assert_eq!(
                hand_ok,
                should_accept,
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
        let want = case
            .expect
            .unwrap_or_else(|| panic!("{}: missing '# expect:'", case.name));
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

// ----- alternative coverage: every grammar alternative, in every statement-level rule -----

/// One symbol of a grammar alternative: a terminal name or a nonterminal name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Sym {
    T(String),
    N(String),
}

/// `docs/grammar.bnf` as data: nonterminal -> its alternatives, read from the normalized text.
fn parse_rules(grammar: &str) -> BTreeMap<String, Vec<Vec<Sym>>> {
    let mut rules = BTreeMap::new();
    for line in grammar.lines() {
        let (lhs, rhs) = line.split_once("::=").expect("a normalized rule line");
        let name = lhs.trim().trim_start_matches('<').trim_end_matches('>');
        let alts = rhs
            .split('|')
            .map(|alt| {
                alt.split_whitespace()
                    .map(|s| match s.strip_prefix('"') {
                        Some(t) => Sym::T(t.trim_end_matches('"').to_string()),
                        None => Sym::N(s.trim_start_matches('<').trim_end_matches('>').to_string()),
                    })
                    .collect()
            })
            .collect();
        rules.insert(name.to_string(), alts);
    }
    rules
}

/// The shortest sentence (terminal names) each nonterminal derives: a fixpoint over the rules.
fn min_sentences(rules: &BTreeMap<String, Vec<Vec<Sym>>>) -> BTreeMap<String, Vec<String>> {
    let mut min: BTreeMap<String, Vec<String>> = BTreeMap::new();
    loop {
        let mut changed = false;
        for (name, alts) in rules {
            for alt in alts {
                let Some(sentence) = expand(alt, &min) else {
                    continue;
                };
                if min.get(name).is_none_or(|old| sentence.len() < old.len()) {
                    min.insert(name.clone(), sentence);
                    changed = true;
                }
            }
        }
        if !changed {
            return min;
        }
    }
}

/// `syms` with every nonterminal replaced by its shortest sentence; `None` while one has none yet.
fn expand(syms: &[Sym], min: &BTreeMap<String, Vec<String>>) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for s in syms {
        match s {
            Sym::T(t) => out.push(t.clone()),
            Sym::N(n) => out.extend(min.get(n)?.iter().cloned()),
        }
    }
    Some(out)
}

/// The token for terminal `name` at position `k`. A fixed token comes from the lexer's own
/// `KEYWORDS` / `PUNCTUATION` lists; identifiers are distinct per position, so a sentence never
/// trips a duplicate-name rule the grammar cannot state. Panics on a terminal with no token.
fn sample_token(name: &str, k: usize) -> Token {
    use crate::lexer::{KEYWORDS, PUNCTUATION};
    match name {
        "NEWLINE" => Token::Newline,
        "INDENT" => Token::Indent,
        "DEDENT" => Token::Dedent,
        "EOF" => Token::Eof,
        "IDENT" => Token::Ident(format!("v{k}")),
        "INT" => Token::Int(1),
        "FLOAT" => Token::Float(1.5),
        "STR" => Token::Str("s".into()),
        "BYTES" => Token::Bytes(b"s".to_vec()),
        "RAWSTR" => Token::RawStr("s".into()),
        _ => KEYWORDS
            .iter()
            .map(|(_, t)| t)
            .chain(PUNCTUATION)
            .find(|t| symbol(t) == name)
            .unwrap_or_else(|| panic!("no sample token for terminal {name}"))
            .clone(),
    }
}

fn sample_tokens(names: &[String]) -> Vec<crate::lexer::Tok> {
    names
        .iter()
        .enumerate()
        .map(|(k, name)| crate::lexer::Tok {
            kind: sample_token(name, k),
            span: crate::lexer::Span {
                line: 1,
                col: k as u32 + 1,
                file: 0,
            },
        })
        .collect()
}

/// A `prefix X suffix` context per nonterminal, as terminal names.
type Frames = BTreeMap<String, (Vec<String>, Vec<String>)>;

/// For each nonterminal reachable from `root`, the cheapest context whose minimal sentence the
/// hand parser accepts. Cheapest-first, so the frame of a nonterminal is the shortest accepted one.
fn frames(
    root: &str,
    rules: &BTreeMap<String, Vec<Vec<Sym>>>,
    min: &BTreeMap<String, Vec<String>>,
) -> Frames {
    let mut framed = Frames::new();
    let mut queue: BTreeSet<(usize, String, Vec<String>, Vec<String>)> = BTreeSet::new();
    queue.insert((min[root].len(), root.to_string(), Vec::new(), Vec::new()));
    while let Some((_, name, prefix, suffix)) = queue.pop_first() {
        if framed.contains_key(&name) {
            continue;
        }
        let mut sentence = prefix.clone();
        sentence.extend(min[&name].iter().cloned());
        sentence.extend(suffix.iter().cloned());
        sentence.push("EOF".to_string());
        if parse(sample_tokens(&sentence)).is_err() {
            continue;
        }
        for alt in &rules[&name] {
            for (i, sym) in alt.iter().enumerate() {
                let Sym::N(child) = sym else { continue };
                if framed.contains_key(child) || child == &name {
                    continue;
                }
                let mut p = prefix.clone();
                p.extend(expand(&alt[..i], min).expect("every rule derives a sentence"));
                let mut s = expand(&alt[i + 1..], min).expect("every rule derives a sentence");
                s.extend(suffix.iter().cloned());
                queue.insert((p.len() + min[child].len() + s.len(), child.clone(), p, s));
            }
        }
        framed.insert(name, (prefix, suffix));
    }
    framed
}

/// Where `docs/grammar.bnf` derives a sentence the hand parser refuses: `(nonterminal whose
/// alternative the sentence enumerates, fragment of the parser's message, reason)`. A context rule
/// the BNF cannot state stays here; a real divergence is listed with its finding id in TICKET-241
/// and is a bug to fix. Every entry must still be hit, so a fixed divergence forces its entry out.
/// Do not add an entry for a new rejection without deciding whether the parser or the grammar is
/// wrong.
const GRAMMAR_LOOSER: &[(&str, &str, &str)] = &[
    // ----- context rules the BNF cannot state -----
    (
        "param",
        "default arguments are not supported here",
        "context: a closure, extern or protocol param takes no default",
    ),
    (
        "param",
        "variadic parameters are not supported here",
        "context: a closure, extern or protocol param is never variadic",
    ),
    (
        "simpleStmt",
        "spawn requires a function or method call",
        "context: the call form of `spawn` takes a call, the BNF says <expr>",
    ),
    (
        "compoundStmt",
        "`wait` needs at least one `recv` arm",
        "context: a `wait:` of one `else:` arm has nothing to race",
    ),
    (
        "nativeStructMember",
        "native instance method must declare `self`",
        "context: the first param of a native method is `self`",
    ),
    (
        "nativeEnumMembers",
        "native instance method must declare `self`",
        "context: the first param of a native method is `self`",
    ),
    (
        "nativeHead",
        "expected 'fn' or 'ctor' after 'native'",
        "context: the BNF shares <nativeHead> with the contextual `ctor` spelled as IDENT",
    ),
    // ----- real divergences, listed in TICKET-241 and not fixed there -----
    (
        "fnDecl",
        "expected identifier, found reserved keyword 'return'",
        "F1: a `where` entry with no bound before the body colon (`fn f[T]() where T: return`)",
    ),
    (
        "type",
        "unexpected ']' in expression",
        "F2: a type in brackets on a non-name head (`1[!]`)",
    ),
    (
        "type",
        "expected ']', found '!'",
        "F2: a type in brackets on a non-name head (`1[T!]`)",
    ),
    (
        "type",
        "expected ':', found ']'",
        "F2: a type in brackets on a non-name head (`1[fn(T) -> U]`)",
    ),
    (
        "typeList",
        "expected ']', found ','",
        "F2: a type list in brackets on a non-name head (`1[T, U]`)",
    ),
    (
        "primary",
        "a nested block must be indented",
        "F3: an inline `match` expression body (`fn f(): match 1:`); DEC-145 keeps it rejected",
    ),
    (
        "primary",
        "expected ':', found end of line",
        "F4: an inline `recover:` block inside a block header's expression",
    ),
    (
        "primary",
        "expected ')', found end of line",
        "F4: an inline `recover:` block inside parentheses",
    ),
    (
        "ifExprTail",
        "expected end of line, found 'elif'",
        "F5: a statement-initial if-expression with `elif` on one line",
    ),
    (
        "importName",
        "found reserved keyword 'None'",
        "F6: <importName> lists NONEKW, the parser rejects `import None from m`",
    ),
];

/// TICKET-241 -- `grammar_and_parser_agree` runs only the hand-written corpus, so a grammar
/// alternative no corpus file spells is unchecked (an inline closure-literal body was). This
/// enumerates instead: for each statement-level rule and each nonterminal reachable from it, one
/// sentence per alternative, every other symbol minimally expanded. The grammar engine must accept
/// each one, and so must the hand parser, except where [`GRAMMAR_LOOSER`] says why not.
#[test]
fn grammar_alternatives_parse() {
    let rules = parse_rules(&normalize_grammar(&read("docs/grammar.bnf")));
    let min = min_sentences(&rules);
    let chars = symbol_chars();
    let engine_grammar = engine_grammar(&chars);
    let engine = engine_grammar.build_parser().expect("build Earley parser");

    // sentence -> (root, nonterminal) of the first alternative that derives it
    let mut sentences: BTreeMap<Vec<String>, (String, String)> = BTreeMap::new();
    for stmt_rule in ["simpleStmt", "compoundStmt", "item"] {
        for alt in &rules[stmt_rule] {
            let [Sym::N(root)] = alt.as_slice() else {
                panic!("<{stmt_rule}> alternative is not one nonterminal: {alt:?}");
            };
            for (name, (prefix, suffix)) in frames(root, &rules, &min) {
                for alt in &rules[&name] {
                    let mut sentence = prefix.clone();
                    sentence.extend(expand(alt, &min).expect("every rule derives a sentence"));
                    sentence.extend(suffix.iter().cloned());
                    sentence.push("EOF".to_string());
                    sentences
                        .entry(sentence)
                        .or_insert_with(|| (root.clone(), name.clone()));
                }
            }
        }
    }

    let mut hit = vec![false; GRAMMAR_LOOSER.len()];
    let mut failures = Vec::new();
    for (sentence, (root, name)) in &sentences {
        let encoded: String = sentence.iter().map(|t| chars[t.as_str()]).collect();
        if engine.parse_input(&encoded).next().is_none() {
            failures.push(format!(
                "[{root}/{name}] the grammar engine rejects its own sentence :: {}",
                sentence.join(" ")
            ));
        }
        let Err(e) = parse(sample_tokens(sentence)) else {
            continue;
        };
        match GRAMMAR_LOOSER
            .iter()
            .position(|(n, frag, _)| n == name && e.message.contains(frag))
        {
            Some(i) => hit[i] = true,
            None => failures.push(format!(
                "[{root}/{name}] {} :: {}",
                e.message,
                sentence.join(" ")
            )),
        }
    }
    for (i, (name, frag, _)) in GRAMMAR_LOOSER.iter().enumerate() {
        if !hit[i] {
            failures.push(format!(
                "GRAMMAR_LOOSER entry ({name}, {frag:?}) matched no sentence: remove it"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} sentences disagree:\n{}",
        failures.len(),
        sentences.len(),
        failures.join("\n")
    );
}
