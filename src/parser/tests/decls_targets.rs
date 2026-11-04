//! Parser tests: error wording, else ownership, assignment targets, variadic params, item
//! placement, native structs, doc comments.
//! Split out of `src/parser/mod.rs`. `super::*` is the parent module.

use super::cursor::describe;
use super::cursor::describe_unexpected;
use super::*;

/// `unexpected` reads without an article; `found` keeps it. A string literal in `describe`
/// itself must keep `a`, or the six `found {}` callers lose their grammar.
#[test]
fn describe_unexpected_drops_only_the_leading_article() {
    assert_eq!(describe_unexpected(&Token::Indent), "indented block");
    assert_eq!(describe_unexpected(&Token::Dedent), "dedent");
    assert_eq!(
        describe_unexpected(&Token::Str("a".into())),
        "string literal"
    );
    assert_eq!(describe_unexpected(&Token::Newline), "end of line");
    assert_eq!(describe_unexpected(&Token::Plus), "'+'");
    assert_eq!(describe(&Token::Str("a".into())), "a string literal");
}

/// TICKET-249 -- indentation ends a block expression: the first token of the next line starts a
/// NEW statement, it never continues the block's value. Each cell asserts the tree shape
/// (statement count of the fn body), not accept/reject.
#[test]
fn next_line_after_block_expr_starts_a_new_statement() {
    let blocks = [
        "match n:\n        1: 10\n        _: 20\n",
        "recover:\n        10 / n\n",
    ];
    for head in ["x := ", "x: int = "] {
        for block in blocks {
            for next in ["(c.bump())", "[1, 2]", "-1", "(1)"] {
                let src = format!("fn f(n: int):\n    {head}{block}    {next}\n");
                let m = parse_ok(&src);
                let StmtKind::Fn(f) = &m.stmts[0].kind else {
                    panic!("not a fn: {src}")
                };
                assert_eq!(
                    f.body.len(),
                    2,
                    "next line must be its own statement:\n{src}"
                );
            }
        }
    }
}

/// TICKET-249 -- an arm after an arm whose value is a nested `match` is a new arm of the outer
/// match, whatever its first token (`?_`, `(x, _)`, `-1`).
#[test]
fn arm_after_nested_match_arm_is_an_outer_arm() {
    for arm in ["?_: 3", "(x, _): 3", "-1: 3"] {
        let src = format!(
            "fn f(t: int):\n    r := match t:\n        0: match t:\n            1: 1\n            _: 2\n        {arm}\n"
        );
        parse_ok(&src);
    }
}

/// Parse without panicking: the module, or the error's line and message. A lexer error has
/// line 0.
fn try_parse(src: &str) -> Result<Module, (usize, String)> {
    let toks = lexer::tokenize(src).map_err(|e| (0, format!("lexer: {e}")))?;
    parse(toks).map_err(|e| (e.span.line as usize, e.message))
}

/// Panic once with the count of failing grid cells and the first twenty.
fn report_cells(grid: &str, total: usize, failed: &[String]) {
    assert!(
        failed.is_empty(),
        "{} of {total} {grid} cells failed; first {}:\n{}",
        failed.len(),
        failed.len().min(20),
        failed[..failed.len().min(20)].join("\n")
    );
}

/// TICKET-249 -- the family's grid: block kind x position x first token of the next line. A
/// block expression ends at its dedent, so the next line is its own statement or arm (Split),
/// an `else` guard on the block-valued binding (Guard), or a parse error on that line (Reject).
/// Each cell asserts where the line lands in the tree, never accept / reject alone. A new first
/// token or position is a new row of these tables.
#[test]
fn block_end_grid() {
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Want {
        Split,
        Guard,
        Reject,
        /// The block head is no whole value (U8): [`BLOCK_VALUE`] on the head's line.
        BlockValue,
    }
    const ARMS: &[&str] = &["    1: 10", "    _: 20"];
    // (head, body lines one level deeper than the statement)
    const KINDS: &[(&str, &[&str])] = &[
        ("match n:", ARMS),
        ("recover:", &["    10"]),
        ("if n == 1: 5 else: match n:", ARMS),
        ("fn(a: int) -> int: match a:", ARMS),
        ("-match n:", ARMS),
        ("1 + match n:", ARMS),
        // the right operand of a comparison: the comparison-chain loop reads the block's end
        ("1 < match n:", ARMS),
    ];
    const POSITIONS: &[&str] = &[
        "x := ",
        "x: int = ",
        "x = ",
        "a, b := ",
        "a, b = ",
        "assert ",
        "return ",
        "yield ",
        "field",
        "outer arm",
        "statement arm",
        "",
        "a, ",
        "wait arm",
    ];
    // (the next line in a statement position, the arm line used in the two arm positions)
    const TOKENS: &[(&str, Option<&str>)] = &[
        ("?g()", Some("?_: 1")),
        ("(g())", Some("(a, b): 1")),
        ("[1, 2]", None),
        (".abs()", None),
        ("?.abs()", None),
        ("-1", Some("-1: 1")),
        ("!b", None),
        ("?? 1", None),
        ("+ 1", None),
        ("* 2", None),
        ("/ 2", None),
        ("% 2", None),
        ("== 1", None),
        ("!= 1", None),
        ("< 1", None),
        ("<= 1", None),
        ("> 1", None),
        (">= 1", None),
        ("in xs", None),
        ("and b", None),
        ("or b", None),
        ("| 1", None),
        ("^ 1", None),
        ("& 1", None),
        ("<< 1", None),
        (">> 1", None),
        (".. 3", None),
        ("|> inc()", None),
        (", 2", None),
        ("else: 0", None),
        ("else e: 0", None),
        ("elif n == 2: 3 else: 4", None),
        ("= 5", None),
        (":= 5", None),
        ("+= 1", None),
        ("-= 1", None),
        ("*= 1", None),
        ("/= 1", None),
        ("%= 1", None),
        ("&= 1", None),
        ("|= 1", None),
        ("^= 1", None),
        ("<<= 1", None),
        (">>= 1", None),
        (", b = 1, 2", None),
        (", b := 1, 2", None),
    ];
    const STARTS_A_LINE: &[&str] = &["?g()", "(g())", "[1, 2]", "-1", "!b", "?? 1", ".. 3"];
    const ELSE: &[&str] = &["else: 0", "else e: 0"];

    /// The lines of a cell and the index of the next line among them.
    fn build(pos: &str, kind: usize, tok: &str, arm: Option<&str>) -> (Vec<String>, usize) {
        let (head, body) = KINDS[kind];
        let blk = |ind: &str, prefix: &str| {
            let mut v = vec![format!("{ind}{prefix}{head}")];
            v.extend(body.iter().map(|b| format!("{ind}{b}")));
            v
        };
        let (before, block, next, after): (&[&str], _, _, &[&str]) = match pos {
            "wait arm" => (
                &["ch := Channel[int](1)", "n := 1", "wait:"],
                blk("    ", ""),
                format!("    {tok}"),
                &["    else: pass", "z := 0"],
            ),
            "return " | "yield " => (
                &["fn f(n: int) -> int:"],
                blk("    ", pos),
                format!("    {tok}"),
                &["    z := 0"],
            ),
            "field" => (
                &["struct S:"],
                blk("    ", "a: int = "),
                format!("    {tok}"),
                &["    b: int = 0"],
            ),
            "outer arm" => (
                &["n := 1", "y := match n:"],
                blk("    ", "0: "),
                format!("    {}", arm.unwrap_or(tok)),
                &["    _: 0", "z := 0"],
            ),
            "statement arm" => (
                &["n := 1", "match n:"],
                blk("    ", "0: x := "),
                format!("    {}", arm.unwrap_or(tok)),
                &["    _: pass", "z := 0"],
            ),
            _ => (&["n := 1"], blk("", pos), tok.to_string(), &["z := 0"]),
        };
        let mut lines: Vec<String> = before.iter().map(|l| l.to_string()).collect();
        lines.extend(block);
        let at = lines.len();
        lines.push(next);
        lines.extend(after.iter().map(|l| l.to_string()));
        (lines, at)
    }

    /// The spans of the list the next line must join: statements or arms.
    fn entries(m: &Module, pos: &str) -> Vec<Span> {
        match (pos, m.stmts.get(1).map(|s| &s.kind)) {
            ("return " | "yield ", _) => match &m.stmts[0].kind {
                StmtKind::Fn(f) => f.body.iter().map(|s| s.span).collect(),
                other => panic!("not a fn: {other:?}"),
            },
            ("outer arm", Some(StmtKind::Let { value, .. })) => match &value.kind {
                ExprKind::Match { arms, .. } => arms.iter().map(|a| a.span).collect(),
                other => panic!("not a match expression: {other:?}"),
            },
            ("statement arm", Some(StmtKind::Match { arms, .. })) => {
                arms.iter().map(|a| a.span).collect()
            }
            _ => m.stmts.iter().map(|s| s.span).collect(),
        }
    }

    let want = |pos: &str, kind: usize, tok: &str, arm: Option<&str>| match pos {
        // an operand, an else branch, or a value no position starts (U8)
        _ if matches!(kind, 2 | 4 | 5 | 6) => Want::BlockValue,
        "assert " | "a, " | "wait arm" if kind != 3 => Want::BlockValue,
        "field" | "a, " | "wait arm" => Want::Reject,
        "outer arm" | "statement arm" if arm.is_some() => Want::Split,
        "outer arm" | "statement arm" => Want::Reject,
        _ if STARTS_A_LINE.contains(&tok) => Want::Split,
        // a bare `match` / `if` is a statement, and a statement takes no guard
        "" if ELSE.contains(&tok) && kind != 0 && kind != 2 => Want::Guard,
        "x := " | "x: int = " | "a, b := " if ELSE.contains(&tok) => Want::Guard,
        _ => Want::Reject,
    };

    let mut failed = Vec::new();
    let mut total = 0;
    for pos in POSITIONS {
        for (kind, &(head, _)) in KINDS.iter().enumerate() {
            for &(tok, arm) in TOKENS {
                total += 1;
                let (lines, at) = build(pos, kind, tok, arm);
                let src = lines.join("\n") + "\n";
                let want = want(pos, kind, tok, arm);
                let cell = format!("{pos:?} / {:?} / {:?}: want {want:?}", head, lines[at]);
                let base = || {
                    let mut without = lines.clone();
                    without.remove(at);
                    entries(&parse_ok(&(without.join("\n") + "\n")), pos).len()
                };
                let head_line = at - KINDS[kind].1.len();
                let problem = match (want, try_parse(&src)) {
                    (Want::BlockValue, Err((line, msg))) => (line != head_line
                        || msg != BLOCK_VALUE)
                        .then(|| format!("error on line {line}, not {head_line}: {msg}")),
                    (Want::BlockValue, Ok(_)) => Some("parsed".to_string()),
                    (Want::Reject, Err((line, _))) if line == at + 1 => None,
                    (Want::Reject, Err((line, msg))) => {
                        Some(format!("error on line {line}, not {}: {msg}", at + 1))
                    }
                    (Want::Reject, Ok(_)) => Some("parsed".to_string()),
                    (_, Err((line, msg))) => Some(format!("error on line {line}: {msg}")),
                    (Want::Split, Ok(m)) => {
                        let spans = entries(&m, pos);
                        if spans.len() != base() + 1 {
                            Some(format!("{} entries, want {}", spans.len(), base() + 1))
                        } else if !spans.iter().any(|s| s.line as usize == at + 1) {
                            Some("no entry starts on the next line".to_string())
                        } else {
                            None
                        }
                    }
                    (Want::Guard, Ok(m)) => {
                        let guarded = matches!(&m.stmts[1].kind,
                                StmtKind::Let { target, value, .. }
                                    if matches!(value.kind, ExprKind::ElseGuard { .. })
                                        && (!pos.is_empty() || matches!(target, Pattern::Wildcard)));
                        if m.stmts.len() != base() {
                            Some(format!("{} statements, want {}", m.stmts.len(), base()))
                        } else if !guarded {
                            Some("the binding's value is not an `else` guard".to_string())
                        } else {
                            None
                        }
                    }
                };
                if let Some(p) = problem {
                    failed.push(format!("{cell}; got {p}"));
                }
            }
        }
    }
    assert_eq!(total, 4508);
    report_cells("block-end", total, &failed);
}

/// TICKET-249 -- after a block expression in an inline body, a dedented `else` belongs to the
/// header that starts the line, never to the inline statement: header x inline statement x
/// block kind x next line. Each cell asserts the owner in the tree or the error's line.
#[test]
fn else_owner_after_block_grid() {
    const KINDS: &[(&str, &[&str])] = &[
        ("match n:", &["    1: 10", "    _: 20"]),
        ("recover:", &["    10"]),
    ];
    const STMTS: &[&str] = &[
        "x := ",
        "x: int = ",
        "x = ",
        "x += ",
        "return ",
        "yield ",
        "assert ",
        "1 + ",
        "",
    ];
    // (name, lines before, header on the statement's line, lines after)
    const HEADERS: &[(&str, &[&str], &str, &[&str])] = &[
        ("if", &[], "if c: ", &[]),
        ("elif", &["if d:", "    pass"], "elif c: ", &[]),
        ("wait", &["wait:"], "    v := ch.recv(): ", &[]),
        ("match", &["match n:"], "    0: ", &["    _: pass"]),
        ("for", &[], "for i in xs: ", &[]),
        ("while", &[], "while c: ", &[]),
    ];
    const NEXT: &[&str] = &["else: pass", "else e: pass", "elif d: pass", "  else: pass"];

    let mut failed = Vec::new();
    let mut total = 0;
    for &(header, before, prefix, after) in HEADERS {
        for stmt in STMTS {
            for &(head, body) in KINDS {
                // the bare statement is a block expression only for `recover:`
                if stmt.is_empty() && head != "recover:" {
                    continue;
                }
                for next in NEXT {
                    total += 1;
                    let ind = if matches!(header, "wait" | "match") {
                        "    "
                    } else {
                        ""
                    };
                    let mut lines = vec!["fn f(n: int, c: bool, d: bool):".to_string()];
                    lines.extend(before.iter().map(|l| format!("    {l}")));
                    lines.push(format!("    {prefix}{stmt}{head}"));
                    lines.extend(body.iter().map(|b| format!("    {ind}{b}")));
                    let at = lines.len() + 1;
                    lines.push(format!("    {ind}{next}"));
                    lines.extend(after.iter().map(|l| format!("    {l}")));
                    lines.push("    z := 0".to_string());
                    let src = lines.join("\n") + "\n";
                    let cell = format!("{header} / {stmt:?} / {head:?} / {next:?}");
                    if next.starts_with(' ') {
                        if lexer::tokenize(&src).is_ok() {
                            failed.push(format!("{cell}: want a lexer error; got tokens"));
                        }
                        continue;
                    }
                    // an inline body is no whole value, and neither is an operand, a compound
                    // assignment's value or an `assert` operand (U8): the one message, on the
                    // block's head line, whatever the next line is
                    if matches!(*stmt, "x += " | "assert " | "1 + " | "") {
                        let head_line = at - body.len() - 1;
                        match try_parse(&src) {
                                Err((line, msg)) if line == head_line && msg == BLOCK_VALUE => {}
                                Err((line, msg)) => failed.push(format!(
                                    "{cell}: want BLOCK_VALUE on line {head_line}; got line {line}: {msg}"
                                )),
                                Ok(_) => failed.push(format!("{cell}: want BLOCK_VALUE; parsed")),
                            }
                        continue;
                    }
                    // the header's branch count and else block once it owns the next line
                    let owner = match (header, *next) {
                        ("if", "else: pass") => Some((1, true)),
                        ("elif", "else: pass") => Some((2, true)),
                        ("if", "elif d: pass") => Some((2, false)),
                        ("elif", "elif d: pass") => Some((3, false)),
                        ("wait", "else: pass") => Some((1, true)),
                        _ => None,
                    };
                    let problem = match (owner, try_parse(&src)) {
                        (None, Err((line, _))) if line == at => None,
                        (None, Err((line, msg))) => {
                            Some(format!("error on line {line}, not {at}: {msg}"))
                        }
                        (None, Ok(_)) => Some("parsed".to_string()),
                        (Some(_), Err((line, msg))) => Some(format!("error on line {line}: {msg}")),
                        (Some((n, has_else)), Ok(m)) => {
                            let StmtKind::Fn(f) = &m.stmts[0].kind else {
                                panic!("not a fn: {src}")
                            };
                            // the header's branch count, its else block, its inline bodies
                            let (len, else_block, bodies) = match &f.body[0].kind {
                                StmtKind::If {
                                    branches,
                                    else_block,
                                } => (
                                    branches.len(),
                                    else_block,
                                    branches.iter().map(|b| &b.1).collect::<Vec<_>>(),
                                ),
                                StmtKind::Wait { arms, else_block } => (
                                    arms.len(),
                                    else_block,
                                    arms.iter().map(|a| &a.body).collect(),
                                ),
                                other => panic!("not an `if` or a `wait`: {other:?}"),
                            };
                            if format!("{bodies:?}").contains("ElseGuard") {
                                Some("the inline statement took the `else` as a guard".into())
                            } else if len != n || else_block.is_some() != has_else {
                                Some(format!(
                                    "{len} branches, else block {}",
                                    else_block.is_some()
                                ))
                            } else {
                                None
                            }
                        }
                    };
                    if let Some(p) = problem {
                        failed.push(format!("{cell}: got {p}"));
                    }
                }
            }
        }
    }
    assert_eq!(total, 408);
    report_cells("else-owner", total, &failed);
}

/// Parse with the lexer's doc-comment side-channel attached, returning the FIRST statement's kind.
fn first_with_docs(src: &str) -> StmtKind {
    let (toks, comments) = lexer::tokenize_with_comments(src, 0).unwrap();
    let mut m = parse_with_docs(toks, comments).unwrap_or_else(|e| panic!("parse failed: {e}"));
    m.stmts.remove(0).kind
}

/// TICKET-218 -- `native type NAME` is an opaque handle, `native type NAME = T` a C width
/// whose Chezzi scalar is `T`; a dangling `=` is a parse error.
#[test]
fn native_type_decl_parses() {
    let StmtKind::NativeType {
        name, underlying, ..
    } = only("native type ptr\n")
    else {
        panic!("expected StmtKind::NativeType");
    };
    assert_eq!(name, "ptr");
    assert!(underlying.is_none());
    let StmtKind::NativeType {
        name, underlying, ..
    } = only("native type int8 = int\n")
    else {
        panic!("expected StmtKind::NativeType");
    };
    assert_eq!(name, "int8");
    assert!(
        matches!(&underlying, Some(Type::Named { name, .. }) if name == "int"),
        "got {underlying:?}"
    );
    parse_err("native type x =\n");
}

/// The shape of a binding target, as `a,(b,c)`; `_` is a skipped slot.
fn target_shape(p: &Pattern) -> String {
    match p {
        Pattern::Ident(n, _, _) => n.clone(),
        Pattern::Wildcard => "_".to_string(),
        Pattern::Tuple(subs) => format!(
            "({})",
            subs.iter().map(target_shape).collect::<Vec<_>>().join(",")
        ),
        other => panic!("not a binding target: {other:?}"),
    }
}

/// U9: one binding-target parser at `:=`, `for`, a comprehension clause and `=`.
#[test]
fn nested_bind_targets_parse_at_every_site() {
    match only("a, (b, c) := t\n") {
        StmtKind::Let { target, .. } => assert_eq!(target_shape(&target), "(a,(b,c))"),
        other => panic!("{other:?}"),
    }
    match only("a, _ := t\n") {
        StmtKind::Let { target, .. } => assert_eq!(target_shape(&target), "(a,_)"),
        other => panic!("{other:?}"),
    }
    match only("_ := t\n") {
        StmtKind::Let { target, .. } => assert_eq!(target, Pattern::Wildcard),
        other => panic!("{other:?}"),
    }
    match only("for a, (b, c) in xs:\n    pass\n") {
        StmtKind::For { target, .. } => assert_eq!(target_shape(&target), "(a,(b,c))"),
        other => panic!("{other:?}"),
    }
    let StmtKind::Expr(e) = only("[a for a, (b, c) in xs]\n") else {
        panic!("expected an expr stmt");
    };
    let ExprKind::Comprehension { clauses, .. } = e.kind else {
        panic!("expected a comprehension");
    };
    assert_eq!(target_shape(&clauses[0].target), "(a,(b,c))");
    match only("a, (b, c) = t\n") {
        StmtKind::Assign { target, .. } => {
            let ExprKind::Tuple(outer) = &target.kind else {
                panic!("expected a tuple target");
            };
            assert!(matches!(&outer[1].kind, ExprKind::Tuple(inner) if inner.len() == 2));
        }
        other => panic!("{other:?}"),
    }
}

/// D2: a wait recv arm binds one value, `T!RecvError`, which no tuple target can unpack (Go and
/// Rust bind one value in a select arm too). A tuple target is a parse error at `:=` and `=`; the
/// message names the fix.
#[test]
fn wait_arm_tuple_target_rejected() {
    for src in [
        "wait:\n    a, (b, c) := ch.recv(): pass\n",
        "wait:\n    a, b := ch.recv(): pass\n",
        "wait:\n    a, := ch.recv(): pass\n",
        "wait:\n    a, = ch.recv():\n        print(a)\n",
        "wait:\n    a, _ := ch.recv(): pass\n",
        "wait:\n    a, (b.f, c[0]) = ch.recv(): pass\n",
        "wait:\n    a, (b, c) = ch.recv():\n        pass\n",
    ] {
        let e = parse_err(src);
        assert!(
            e.message.contains("a wait arm binds one value"),
            "{src:?}: {}",
            e.message
        );
    }
}

/// U9: `=` takes no outer parentheses either, at a statement and at a wait arm, and a nested
/// tuple still parses at a statement. One function (`ExprKind::is_place`) owns the rule.
#[test]
fn assign_outer_parens_rejected_at_every_site() {
    for src in [
        "(a, b) = t\n",
        "wait:\n    (a, b) = ch.recv():\n        pass\n",
    ] {
        let e = parse_err(src);
        assert!(
            e.message.contains("invalid assignment target"),
            "{src:?}: {}",
            e.message
        );
    }
    parse_ok("a, (b, c) = t\n");
}

/// U9: an unpacking target takes no outer parentheses; `(a, b) :=` keeps its own message.
#[test]
fn for_outer_parens_rejected() {
    for src in ["for (a, b) in xs:\n    pass\n", "[a for (a, b) in xs]\n"] {
        let e = parse_err(src);
        assert!(
            e.message.contains("takes no outer parentheses"),
            "{src:?}: {}",
            e.message
        );
    }
    let e = parse_err("(a, b) := t\n");
    assert!(
        e.message.contains("left side of ':=' must be a name"),
        "got: {}",
        e.message
    );
    // A one-name group is that name at every site (CPython: `a, (b) = t`, `for (z) in xs`).
    for src in [
        "a, (b) := t\n",
        "(y) := 1, 2\n",
        "for (z) in xs:\n    pass\n",
        "[z for (z) in xs]\n",
        "wait:\n    (x) := ch.recv():\n        pass\n",
    ] {
        parse_ok(src);
    }
}

/// A nested binding target spends the pattern depth budget: a target nested past `MAX_DEPTH` is
/// a parse error at `:=` and at `for`, never a stack overflow.
#[test]
fn deeply_nested_bind_target_is_a_parse_error() {
    crate::on_frontend_stack(deeply_nested_bind_target_body);
}

fn deeply_nested_bind_target_body() {
    let n = MAX_DEPTH + 100;
    let target = format!("a, {}x, y{}", "(x, ".repeat(n), ")".repeat(n));
    for src in [
        format!("{target} := t\n"),
        format!("for {target} in xs:\n    pass\n"),
    ] {
        let e = parse_err(&src);
        assert!(e.message.contains("too deeply"), "got: {}", e.message);
    }
}

/// A bare tuple ends a statement at `:=`, a typed let, `=`, `return` and `yield`; a block value
/// followed by `,` keeps DEC-260's end-of-line error and a value list takes no `else` guard.
#[test]
fn bare_tuple_ends_a_statement_at_parse() {
    for src in [
        "x := 1, 2\n",
        "x: (int, int) = 1, 2\n",
        "x = 3, 4\n",
        "fn f():\n    return 1, 2\n",
        "fn f():\n    yield 1, 2\n",
    ] {
        let m = parse(lexer::tokenize(src).unwrap());
        assert!(m.is_ok(), "{src:?}: {:?}", m.err());
    }
    let e = parse_err("a, b := 1, 2 else:\n    pass\n");
    assert!(
        e.message.contains("expected end of line, found 'else'"),
        "got: {}",
        e.message
    );
}

#[test]
fn comparison_chain_parses_as_one_compare_node() {
    let StmtKind::Expr(e) = only("a < b == c\n") else {
        panic!("expected an expr stmt");
    };
    let ExprKind::Compare { operands, ops } = e.kind else {
        panic!("expected ExprKind::Compare, got {:?}", e.kind);
    };
    assert_eq!(operands.len(), 3);
    assert_eq!(ops, vec![BinaryOp::Lt, BinaryOp::Eq]);

    // Parentheses stop a chain: still an ordinary `Binary`.
    let StmtKind::Expr(e) = only("(a < b) == c\n") else {
        panic!("expected an expr stmt");
    };
    assert!(matches!(
        e.kind,
        ExprKind::Binary {
            op: BinaryOp::Eq,
            ..
        }
    ));
}

#[test]
fn pass_parses_as_noop_stmt() {
    assert_eq!(only("pass\n"), StmtKind::Pass);
    // inside a fn body
    let StmtKind::Fn(d) = only("fn f():\n    pass\n") else {
        panic!("expected fn");
    };
    assert_eq!(d.body.len(), 1);
    assert_eq!(d.body[0].kind, StmtKind::Pass);
}

#[test]
fn closure_body_pass_parses() {
    let StmtKind::Let { value, .. } = only("f := fn(): pass\n") else {
        panic!("expected a let");
    };
    let ExprKind::Closure { body, .. } = value.kind else {
        panic!("expected a closure");
    };
    assert_eq!(body.kind, ExprKind::Pass);
}

#[test]
fn pass_reserved_as_name_rejected() {
    // walrus / assign / annotation forms get the targeted keyword message
    assert!(
        parse_err("pass := 5\n")
            .to_string()
            .contains("reserved keyword")
    );
    // name positions get the standard "expected identifier" (by-construction, like `ref`)
    parse_err("fn pass():\n    return\n");
    parse_err("fn f(pass: int):\n    return\n");
    parse_err("struct S:\n    pass: int\n");
    parse_err("import m as pass\n");
}

#[test]
fn empty_protocol_via_pass() {
    let StmtKind::Protocol {
        methods, embeds, ..
    } = only("protocol Foo:\n    pass\n")
    else {
        panic!("expected protocol");
    };
    assert!(methods.is_empty());
    assert!(embeds.is_empty());
    // pass + a method, and pass pass, both reject
    parse_err("protocol Foo:\n    pass\n    fn m(self)\n");
    parse_err("protocol Foo:\n    pass\n    pass\n");
}

#[test]
fn empty_struct_via_pass() {
    let StmtKind::Struct {
        fields, methods, ..
    } = only("struct S:\n    pass\n")
    else {
        panic!("expected struct");
    };
    assert!(fields.is_empty());
    assert!(methods.is_empty());
    parse_err("struct S:\n    pass\n    x: int\n");
    parse_err("struct S:\n    pass\n    pass\n");
}

#[test]
fn empty_enum_via_pass_rejected() {
    let e = parse_err("enum E:\n    pass\n");
    assert!(
        e.to_string().contains("at least one variant") || e.to_string().contains("empty enum"),
        "unexpected message: {e}"
    );
}

#[test]
fn variadic_param_parses() {
    let StmtKind::Fn(d) = only("fn f(...xs: int):\n    return\n") else {
        panic!("expected fn");
    };
    assert_eq!(d.params.len(), 1);
    assert_eq!(d.params[0].name, "xs");
    assert!(d.params[0].is_variadic);
}

#[test]
fn variadic_param_native_and_keyword_only_tail() {
    // native decl may be variadic; a post-variadic param is legal (keyword-only).
    let StmtKind::Native(d) = only("native fn print(...args: int, sep: str = \" \") -> None\n")
    else {
        panic!("expected native");
    };
    assert!(d.params[0].is_variadic);
    assert!(!d.params[1].is_variadic);
}

#[test]
fn variadic_rejects_second_variadic() {
    let e = parse_err("fn f(...a: int, ...b: int):\n    return\n");
    assert!(e.to_string().contains("variadic"), "got: {e}");
}

#[test]
fn variadic_rejects_default() {
    let e = parse_err("fn f(...a: int = []):\n    return\n");
    assert!(e.to_string().contains("variadic"), "got: {e}");
}

#[test]
fn variadic_requires_element_type() {
    let e = parse_err("fn f(...a):\n    return\n");
    assert!(e.to_string().contains("variadic"), "got: {e}");
}

#[test]
fn variadic_rejected_in_closure() {
    let e = parse_err("g := fn(...x: int): x\n");
    assert!(e.to_string().contains("variadic"), "got: {e}");
}

#[test]
fn native_fn_and_ctor_parse_to_stmtkind_native() {
    // `native fn` → NativeKind::Fn, annotated param carries its type, `-> ret` present.
    let StmtKind::Native(d) = only("native fn ord(c: str) -> int\n") else {
        panic!("expected StmtKind::Native");
    };
    assert_eq!(d.name, "ord");
    assert_eq!(d.kind, NativeKind::Fn);
    assert_eq!(d.params.len(), 1);
    assert_eq!(d.params[0].name, "c");
    assert!(d.params[0].ty.is_some());
    assert!(d.ret.is_some());

    // `native ctor` → NativeKind::Ctor; an UNANNOTATED param (`x`) has no type (dynamic).
    let StmtKind::Native(d) = only("native ctor int(x) -> int\n") else {
        panic!("expected StmtKind::Native");
    };
    assert_eq!(d.name, "int");
    assert_eq!(d.kind, NativeKind::Ctor);
    assert_eq!(d.params.len(), 1);
    assert_eq!(d.params[0].name, "x");
    assert!(d.params[0].ty.is_none(), "unannotated param → ty None");
    assert!(d.ret.is_some());

    // A `native fn` with NO `-> ret` (like `panic`) parses with `ret == None`.
    let StmtKind::Native(d) = only("native fn panic(msg: str)\n") else {
        panic!("expected StmtKind::Native");
    };
    assert_eq!(d.name, "panic");
    assert_eq!(d.kind, NativeKind::Fn);
    assert!(d.ret.is_none(), "no arrow → ret None (native/never)");
}

#[test]
fn native_fn_method_own_type_params_parse() {
    // A native METHOD may declare its OWN `[U]` after the name (before the param list); the
    // enclosing native struct's `[T]` and the method's `[U]` coexist.
    let StmtKind::NativeStruct {
        name,
        type_params,
        methods,
        ..
    } = only("native struct List[T]:\n    native fn map[U](self, f: fn(T) -> U) -> List[U]\n")
    else {
        panic!("expected StmtKind::NativeStruct");
    };
    assert_eq!(name, "List");
    assert_eq!(type_params.len(), 1, "struct [T]");
    assert_eq!(type_params[0].name, "T");
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0].name, "map");
    assert_eq!(methods[0].type_params.len(), 1, "method own [U]");
    assert_eq!(methods[0].type_params[0].name, "U");

    // A bounded method type param (`sort_by_key[K: Comparable]`) carries its bound.
    let StmtKind::NativeStruct { methods, .. } = only(
        "native struct List[T]:\n    native fn sort_by_key[K: Comparable](self, f: fn(T) -> K) -> None\n",
    ) else {
        panic!("expected StmtKind::NativeStruct");
    };
    assert_eq!(methods[0].type_params[0].name, "K");
    assert_eq!(methods[0].type_params[0].bounds[0].name, "Comparable");

    // A plain native method (no `[...]`) has empty type_params.
    let StmtKind::Native(d) = only("native fn ord(c: str) -> int\n") else {
        panic!("expected StmtKind::Native");
    };
    assert!(d.type_params.is_empty());
}

#[test]
fn fn_where_clause_parses_into_where_bounds() {
    // A `where` clause after the return type folds its bounds into `FnDecl.where_bounds`
    // (kept SEPARATE from `type_params` at parse time; the checker merges them).
    let StmtKind::Fn(d) = only("fn f[T]() -> int where T: Comparable: 0\n") else {
        panic!("expected StmtKind::Fn");
    };
    assert_eq!(d.where_bounds.len(), 1);
    assert_eq!(d.where_bounds[0].name, "T");
    assert_eq!(d.where_bounds[0].bounds.len(), 1);
    assert_eq!(d.where_bounds[0].bounds[0].name, "Comparable");

    // A plain fn has an empty `where_bounds`.
    let StmtKind::Fn(d) = only("fn g[T]():\n    return 0\n") else {
        panic!("expected StmtKind::Fn");
    };
    assert!(d.where_bounds.is_empty());

    // A `where` clause with NO `-> ret` (the `where` sits directly after `)`), block body.
    let StmtKind::Fn(d) = only("fn h[T]() where T: Comparable:\n    return 0\n") else {
        panic!("expected StmtKind::Fn");
    };
    assert_eq!(d.where_bounds.len(), 1);
    assert_eq!(d.where_bounds[0].name, "T");

    // Multi-entry + multi-bound where clause parses (comma-separated, `+`-joined bounds).
    let StmtKind::Fn(d) = only("fn k[A, B]() where A: Add + Mul, B: Comparable: 0\n") else {
        panic!("expected StmtKind::Fn");
    };
    assert_eq!(d.where_bounds.len(), 2);
    assert_eq!(d.where_bounds[0].name, "A");
    assert_eq!(d.where_bounds[0].bounds.len(), 2);
    assert_eq!(d.where_bounds[1].name, "B");

    // A `native fn` method sig inside a `native struct` carries `where_bounds` too.
    let StmtKind::NativeStruct { methods, .. } =
        only("native struct L[T]:\n    native fn sort(self) -> None where T: Comparable\n")
    else {
        panic!("expected StmtKind::NativeStruct");
    };
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0].name, "sort");
    assert_eq!(methods[0].where_bounds.len(), 1);
    assert_eq!(methods[0].where_bounds[0].name, "T");
    assert_eq!(methods[0].where_bounds[0].bounds[0].name, "Comparable");
}

#[test]
fn native_struct_mixes_bodyless_and_bodied_methods() {
    // Phase 4c-followup — a `native struct` may carry BOTH bodyless `native fn` sigs (native
    // dispatch) AND a bodied `fn` method (compiled to bytecode). A `yield` in the body flags a
    // generator.
    let StmtKind::NativeStruct {
        methods,
        bodied_methods,
        ..
    } = only(
        "native struct R:\n    native fn read_line(self) -> str?\n    fn lines(self) -> Iterator[str]:\n        yield \"x\"\n",
    )
    else {
        panic!("expected StmtKind::NativeStruct");
    };
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0].name, "read_line");
    assert_eq!(bodied_methods.len(), 1);
    assert_eq!(bodied_methods[0].name, "lines");
    assert!(
        bodied_methods[0].is_generator,
        "a `yield`-bearing bodied method is a generator"
    );
}

#[test]
fn native_fn_allows_optional_trailing_default() {
    // Phase 4f — a `native fn` may carry a DEFAULTED trailing param (`timeout_ms: int = 0`). The
    // parser accepts a default in a native decl; a call fills it from the decl.
    let StmtKind::Native(d) = only("native fn get(url: str, timeout_ms: int = 0) -> Response!\n")
    else {
        panic!("expected StmtKind::Native");
    };
    assert_eq!(d.name, "get");
    assert_eq!(d.params.len(), 2);
    assert!(d.params[0].default.is_none(), "url has no default");
    assert!(
        d.params[1].default.is_some(),
        "timeout_ms carries its `= 0` default"
    );
}

#[test]
fn native_decl_nested_is_error() {
    // A `native` decl is TOP-LEVEL-only: nested inside a fn body it is a parse error.
    let e = parse_err("fn f():\n    native fn g()\n");
    assert!(
        e.message
            .contains("native declaration must be a top-level declaration"),
        "unexpected error: {}",
        e.message
    );
}

/// TICKET-178 grid: every item kind x every block position must be the parse error
/// "`<kind>` must be a top-level declaration"; every kind at module level and a nested `fn`
/// anywhere must parse. Owner decision 2026-09-28: types are top-level only.
#[test]
fn item_declarations_are_top_level_only_grid() {
    let kinds: &[(&str, &str)] = &[
        ("struct", "struct S:\n    x: int\n"),
        ("enum", "enum E:\n    A\n"),
        ("protocol", "protocol P:\n    fn m(self) -> int\n"),
        ("type", "type T = int\n"),
        ("test fn", "test fn t():\n    pass\n"),
        ("import", "import lib.x\n"),
        (
            "extern block",
            "extern \"libm.so.6\":\n    fn foo() -> int\n",
        ),
        ("native declaration", "native fn foo() -> int\n"),
    ];
    let mut listed: Vec<&str> = lexer::KEYWORDS
        .iter()
        .filter_map(|(_, t)| Parser::item_keyword(t).map(|(k, _)| k))
        .collect();
    let mut rows: Vec<&str> = kinds.iter().map(|(k, _)| *k).collect();
    listed.sort_unstable();
    rows.sort_unstable();
    assert_eq!(
        listed, rows,
        "grid kinds must be exactly Parser::item_keyword's list"
    );
    // (position name, header lines, indent of the body under the header)
    let positions: &[(&str, &str, usize)] = &[
        ("fn body", "fn f():\n", 4),
        ("method body", "struct H:\n    x: int\n    fn m(self):\n", 8),
        ("if", "fn f():\n    if true:\n", 8),
        ("while", "fn f():\n    while true:\n", 8),
        ("for", "fn f():\n    for i in 0..3:\n", 8),
        ("match arm", "fn f():\n    match 1:\n        _:\n", 12),
        ("parallel", "fn f():\n    parallel:\n", 8),
        ("spawn", "fn f():\n    spawn:\n", 8),
        ("defer", "fn f():\n    defer:\n", 8),
        ("recover", "fn f():\n    r := recover:\n", 8),
        (
            "wait arm",
            "fn f(ch: Channel[int]):\n    wait:\n        v := ch.recv():\n",
            12,
        ),
        ("nested fn body", "fn f():\n    fn g():\n", 8),
        ("top-level if", "if true:\n", 4),
    ];
    let indent = |src: &str, n: usize| -> String {
        src.lines()
            .map(|l| format!("{}{}\n", " ".repeat(n), l))
            .collect()
    };
    let mut wrong = Vec::new();
    for (kind, decl) in kinds {
        if let Err(e) = parse(lexer::tokenize(decl).unwrap()) {
            wrong.push(format!("{kind} @ module level: rejected: {}", e.message));
        }
        for (pos, header, n) in positions {
            let src = format!("{header}{}", indent(decl, *n));
            let want = format!("{kind} must be a top-level declaration");
            match parse(lexer::tokenize(&src).unwrap()) {
                Ok(_) => wrong.push(format!("{kind} @ {pos}: accepted")),
                Err(e) if !e.message.contains(&want) => {
                    wrong.push(format!("{kind} @ {pos}: wrong error: {}", e.message))
                }
                Err(e) => {
                    let at = (header.lines().count() as u32 + 1, *n as u32 + 1);
                    if (e.span.line, e.span.col) != at {
                        wrong.push(format!(
                            "{kind} @ {pos}: error at {}:{}, want {}:{}",
                            e.span.line, e.span.col, at.0, at.1
                        ));
                    }
                }
            }
        }
    }
    for (pos, header, n) in positions {
        let src = format!("{header}{}", indent("fn k() -> int:\n    return 1\n", *n));
        if let Err(e) = parse(lexer::tokenize(&src).unwrap()) {
            wrong.push(format!("fn @ {pos}: rejected: {}", e.message));
        }
    }
    assert!(
        wrong.is_empty(),
        "top-level-only grid violations:\n{}",
        wrong.join("\n")
    );
}

#[test]
fn import_nested_is_error() {
    // An `import` is TOP-LEVEL-only: nested inside a fn body/block it is a parse error (it used
    // to parse + check clean + be a complete no-op — the resolver only scans top-level stmts).
    let e = parse_err("fn f() -> int:\n    import lib.x\n    return 1\n");
    assert!(
        e.message.contains("import must be a top-level declaration"),
        "unexpected error: {}",
        e.message
    );
}

#[test]
fn native_bad_kind_word_is_error() {
    // `native` must be followed by `fn` or `ctor`.
    let e = parse_err("native thing foo()\n");
    assert!(
        e.message.contains("expected 'fn' or 'ctor' after 'native'"),
        "unexpected error: {}",
        e.message
    );
}

#[test]
fn native_struct_parses_fields_only() {
    // `native struct Name:` with an indented block of body-less field decls → StmtKind::NativeStruct.
    let StmtKind::NativeStruct {
        name,
        type_params,
        fields,
        ..
    } = only(
        "native struct Match:\n    text: str\n    start: int\n    end: int\n    groups: List[str]\n",
    )
    else {
        panic!("expected StmtKind::NativeStruct");
    };
    assert_eq!(name, "Match");
    assert!(type_params.is_empty());
    let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["text", "start", "end", "groups"]);
    assert!(fields.iter().all(|f| f.default.is_none()));
}

#[test]
fn native_struct_nested_rejected() {
    // A `native struct` is TOP-LEVEL-only: nested inside a fn body it is a parse error (a block
    // rejects the `native` keyword through `item_keyword` in `parse_stmt`).
    let e = parse_err("fn f():\n    native struct X:\n        a: int\n");
    assert!(
        e.message
            .contains("native declaration must be a top-level declaration"),
        "unexpected error: {}",
        e.message
    );
}

#[test]
fn native_struct_test_fn_rejected() {
    // Phase 4c-followup — a BODIED `fn` is now admitted (compiled to bytecode, `native_methods`),
    // but a `test fn` inside a native-struct body is still a parse error (a native handle is not a
    // test suite).
    let e = parse_err("native struct M:\n    a: int\n    test fn m(self):\n        pass\n");
    assert!(
        e.message.contains("cannot carry a `test fn`"),
        "unexpected error: {}",
        e.message
    );
}

#[test]
fn native_struct_parses_native_methods() {
    // Phase 4c-followup — a `native fn` inside a native-struct body is an INSTANCE method: it
    // declares a leading bare `self` (mirroring user structs), harvested into `methods` with the
    // `self` KEPT in the parsed decl (harvest strips it). A trailing `= default` param is an
    // ordinary default (like a free native fn's).
    let StmtKind::NativeStruct {
        name,
        fields,
        methods,
        ..
    } = only(
        "native struct Socket:\n    native fn read(self, n: int, timeout_ms: int = 0) -> str!\n    native fn close(self)\n",
    )
    else {
        panic!("expected StmtKind::NativeStruct");
    };
    assert_eq!(name, "Socket");
    assert!(fields.is_empty());
    let mnames: Vec<&str> = methods.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(mnames, ["read", "close"]);
    let read = &methods[0];
    assert_eq!(read.params.len(), 3, "self kept in parsed decl");
    assert_eq!(read.params[0].name, "self");
    assert!(read.params[0].ty.is_none(), "self is untyped (bare)");
    assert!(read.params[2].default.is_some(), "trailing default marker");
    assert!(read.ret.is_some());
    let close = &methods[1];
    assert_eq!(close.params.len(), 1);
    assert_eq!(close.params[0].name, "self");
    assert!(close.ret.is_none());
}

#[test]
fn native_struct_method_without_self_is_static() {
    // A `native fn` with no `self` in a native struct is a static method, as in a user struct.
    let m = parse_ok("native struct S:\n    native fn read(n: int)\n");
    let StmtKind::NativeStruct { methods, .. } = &m.stmts[0].kind else {
        panic!("expected a native struct, got {:?}", m.stmts[0].kind);
    };
    assert_eq!(methods[0].name, "read");
    assert!(!methods[0].params.first().is_some_and(|p| p.is_self()));
}

#[test]
fn native_struct_method_rejects_self_nonfirst() {
    // `self` is valid ONLY as the first parameter.
    let e = parse_err("native struct S:\n    native fn f(self, a: int, self)\n");
    assert!(
        e.message
            .contains("`self` is only valid as the first parameter"),
        "unexpected error: {}",
        e.message
    );
}

#[test]
fn module_native_fn_rejects_self() {
    // A module-level (free) `native fn` must NOT take `self`.
    let e = parse_err("native fn g(self) -> int\n");
    assert!(
        e.message
            .contains("module-level `native fn` cannot take `self`"),
        "unexpected error: {}",
        e.message
    );
}

#[test]
fn native_struct_field_default_rejected() {
    // A native struct field carries no default (the layout is native).
    let e = parse_err("native struct M:\n    a: int = 0\n");
    assert!(
        e.message
            .contains("native struct fields cannot have default values"),
        "unexpected error: {}",
        e.message
    );
}

#[test]
fn native_enum_parses_variants_and_generics() {
    // Phase 5b — `native enum Name[T]:` with an indented block of body-less variants →
    // StmtKind::NativeEnum. Mirrors `native_struct_parses_fields_only` but for the enum shape.
    let StmtKind::NativeEnum {
        name,
        type_params,
        variants,
        methods,
        ..
    } = only("native enum Option[T]:\n    Some(T)\n    None\n")
    else {
        panic!("expected StmtKind::NativeEnum");
    };
    assert_eq!(name, "Option");
    assert_eq!(type_params.len(), 1);
    assert_eq!(type_params[0].name, "T");
    let vnames: Vec<&str> = variants.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(vnames, ["Some", "None"]);
    assert_eq!(
        variants[0].payload.len(),
        1,
        "Some carries one payload slot"
    );
    assert!(variants[1].payload.is_empty(), "None is nullary");
    assert!(methods.is_empty());
}

#[test]
fn native_enum_parses_two_type_params() {
    // `native enum Result[T, E]:` — two type params, Err carries E.
    let StmtKind::NativeEnum {
        name,
        type_params,
        variants,
        ..
    } = only("native enum Result[T, E]:\n    Ok(T)\n    Err(E)\n")
    else {
        panic!("expected StmtKind::NativeEnum");
    };
    assert_eq!(name, "Result");
    let pnames: Vec<&str> = type_params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(pnames, ["T", "E"]);
    let vnames: Vec<&str> = variants.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(vnames, ["Ok", "Err"]);
    assert_eq!(variants[0].payload.len(), 1);
    assert_eq!(variants[1].payload.len(), 1);
}

#[test]
fn native_enum_parses_self_methods() {
    // Phase 5b — a `native fn` inside a native-enum body is an INSTANCE method: it declares a
    // leading bare `self` (mirroring native structs), harvested into `methods` with `self` KEPT in
    // the parsed decl (harvest strips it). Variants must precede methods.
    let StmtKind::NativeEnum {
        name,
        variants,
        methods,
        ..
    } = only(
        "native enum Option[T]:\n    Some(T)\n    None\n    native fn is_some(self) -> bool\n",
    )
    else {
        panic!("expected StmtKind::NativeEnum");
    };
    assert_eq!(name, "Option");
    let vnames: Vec<&str> = variants.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(vnames, ["Some", "None"]);
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0].name, "is_some");
    assert_eq!(methods[0].params.len(), 1, "self kept in parsed decl");
    assert_eq!(methods[0].params[0].name, "self");
    assert!(methods[0].params[0].ty.is_none(), "self is untyped (bare)");
}

#[test]
fn native_enum_method_requires_self() {
    // A native enum has no static methods: a self-less method inside one is rejected.
    let e = parse_err("native enum E[T]:\n    A(T)\n    native fn foo() -> int\n");
    assert!(
        e.message
            .contains("a native enum method must declare `self` as its first parameter"),
        "unexpected error: {}",
        e.message
    );
}

#[test]
fn native_enum_plain_method_rejected() {
    // A PLAIN (non-native) method sig inside a native-enum body is a parse error; only bodyless
    // `native fn` methods are admitted (harvested into the enum's method table).
    let e = parse_err("native enum E[T]:\n    A(T)\n    fn m(self) -> int\n");
    assert!(
        e.message.contains("native enum methods"),
        "unexpected error: {}",
        e.message
    );
}

#[test]
fn native_enum_variant_after_method_rejected() {
    // Variants must precede methods (mirror `parse_enum`).
    let e = parse_err("native enum E[T]:\n    A(T)\n    native fn m(self) -> int\n    B\n");
    assert!(
        e.message
            .contains("variants must be declared before methods"),
        "unexpected error: {}",
        e.message
    );
}

#[test]
fn native_enum_nested_rejected() {
    // A `native enum` is TOP-LEVEL-only: nested inside a fn body it is a parse error (a block
    // rejects the `native` keyword through `item_keyword` in `parse_stmt`).
    let e = parse_err("fn f():\n    native enum X:\n        A\n");
    assert!(
        e.message
            .contains("native declaration must be a top-level declaration"),
        "unexpected error: {}",
        e.message
    );
}

#[test]
fn fn_carries_adjacent_doc() {
    // A contiguous comment block immediately above a `fn` joins (top-down, `\n`) into its doc.
    let StmtKind::Fn(decl) = first_with_docs("# line A\n# line B\nfn foo():\n    1\n") else {
        panic!("expected fn");
    };
    assert_eq!(decl.doc.as_deref(), Some("line A\nline B"));
}

#[test]
fn blank_line_detaches_doc() {
    // A blank line between two comment blocks detaches the earlier one — only the adjacent block
    // (here "attached") attaches to the decl.
    let StmtKind::Fn(decl) = first_with_docs("# detached\n\n# attached\nfn foo():\n    1\n") else {
        panic!("expected fn");
    };
    assert_eq!(decl.doc.as_deref(), Some("attached"));
}

#[test]
fn inline_trailing_comment_is_not_doc() {
    // A trailing comment on the decl line itself is not a doc (it never reaches the side channel).
    let StmtKind::Fn(decl) = first_with_docs("fn foo(): 1  # not a doc\n") else {
        panic!("expected fn");
    };
    assert_eq!(decl.doc, None);
}

#[test]
fn doc_attaches_to_struct_and_let() {
    let StmtKind::Struct { doc, .. } = first_with_docs("# a point\nstruct P:\n    x: int\n") else {
        panic!("expected struct");
    };
    assert_eq!(doc.as_deref(), Some("a point"));

    let StmtKind::Let { doc, .. } = first_with_docs("# the answer\nanswer := 42\n") else {
        panic!("expected let");
    };
    assert_eq!(doc.as_deref(), Some("the answer"));
}

#[test]
fn plain_parse_has_no_docs() {
    // The default `parse` path (empty doc map) never attaches a doc — zero behavior change.
    let StmtKind::Fn(decl) = only("# nope\nfn foo():\n    1\n") else {
        panic!("expected fn");
    };
    assert_eq!(decl.doc, None);
}

#[test]
fn raw_str_parses_to_rawstr_expr() {
    // A raw string parses to a distinct `ExprKind::RawStr` carrying verbatim text.
    match only("r\"{}\"\n") {
        StmtKind::Expr(e) => match e.kind {
            ExprKind::RawStr(s) => assert_eq!(s, "{}"),
            other => panic!("expected RawStr, got {other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn raw_str_usable_as_match_pattern() {
    // Regression: a raw string is a valid literal pattern in a match arm (was rejected with a
    // misleading "expected identifier" because parse_pattern_primary lacked a RawStr arm).
    let src = "fn f(s: str) -> int:\n    return match s:\n        r\"{}\": 1\n        _: 0\nfn main():\n    print(f(r\"{}\"))\nmain()\n";
    parse(crate::lexer::tokenize(src).expect("lex")).expect("raw string should parse as a pattern");
}

#[test]
fn assert_no_msg_parses() {
    match only("assert x == 1\n") {
        StmtKind::Assert { cond, msg } => {
            assert!(matches!(cond.kind, ExprKind::Binary { .. }));
            assert!(msg.is_none());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn assert_with_msg_parses() {
    match only("assert x, \"boom\"\n") {
        StmtKind::Assert { cond, msg } => {
            assert!(matches!(cond.kind, ExprKind::Ident(_)));
            assert!(matches!(msg.unwrap().kind, ExprKind::Str(_)));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn free_test_fn_parses_with_is_test() {
    match only("test fn t():\n    assert true\n") {
        StmtKind::Fn(decl) => {
            assert!(decl.is_test, "free `test fn` should set is_test");
            assert_eq!(decl.name, "t");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn plain_fn_is_not_test() {
    match only("fn t():\n    assert true\n") {
        StmtKind::Fn(decl) => assert!(!decl.is_test),
        other => panic!("{other:?}"),
    }
}

#[test]
fn struct_test_method_parses_with_is_test() {
    match only(
        "struct S:\n    test fn t(self):\n        assert true\n    fn helper(self):\n        return\n",
    ) {
        StmtKind::Struct { methods, .. } => {
            let t = methods.iter().find(|m| m.name == "t").unwrap();
            assert!(t.is_test, "struct `test fn` should set is_test");
            let h = methods.iter().find(|m| m.name == "helper").unwrap();
            assert!(!h.is_test);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn enum_with_method_parses() {
    match only("enum Color:\n    Red\n    Green\n    fn area(self) -> int:\n        return 5\n") {
        StmtKind::Enum {
            variants, methods, ..
        } => {
            assert_eq!(variants.len(), 2);
            let a = methods.iter().find(|m| m.name == "area").unwrap();
            assert_eq!(a.params.len(), 1);
            assert_eq!(a.params[0].name, "self");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn enum_variant_after_method_rejected() {
    // Methods must follow ALL variants; a variant after a method is a structuring error.
    let toks =
        crate::lexer::tokenize("enum E:\n    A\n    fn f(self) -> int:\n        return 1\n    B\n")
            .expect("lex");
    let err = parse(toks).expect_err("variant after method should be rejected");
    assert!(
        err.message
            .contains("variants must be declared before methods"),
        "got: {}",
        err.message
    );
}

#[test]
fn enum_test_method_rejected() {
    // Enum test suites are not wired (compiler/runner build discovery for structs only), so a
    // `test fn` in an enum body would silently never run. The parser rejects it outright.
    let toks =
        crate::lexer::tokenize("enum E:\n    A\n    test fn t(self):\n        assert true\n")
            .expect("lex");
    let err = parse(toks).expect_err("enum `test fn` should be rejected");
    assert!(
        err.message
            .contains("test methods are not supported on enums"),
        "got: {}",
        err.message
    );
}

#[test]
fn tuple_swap_parses() {
    match only("a, b = b, a\n") {
        StmtKind::Assign { target, op, value } => {
            assert_eq!(op, AssignOp::Eq);
            match (&target.kind, &value.kind) {
                (ExprKind::Tuple(ts), ExprKind::Tuple(vs)) => {
                    assert_eq!(ts.len(), 2);
                    assert_eq!(vs.len(), 2);
                }
                other => panic!("expected Tuple targets/values, got {other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn tuple_index_swap_parses() {
    // index/field targets on the left — must NOT become a bare Expr statement.
    match only("data[0], data[1] = data[1], data[0]\n") {
        StmtKind::Assign { target, op, .. } => {
            assert_eq!(op, AssignOp::Eq);
            assert!(matches!(target.kind, ExprKind::Tuple(_)));
        }
        other => panic!("expected tuple-target Assign, got {other:?}"),
    }
}

#[test]
fn tuple_compound_rejected() {
    // compound op with multiple targets is a clean parse error.
    let e = parse_err("a, b += 1\n");
    assert!(
        e.message.contains("compound assignment") || e.message.contains("multiple targets"),
        "got: {}",
        e.message
    );
}

#[test]
fn destructuring_let_still_walrus() {
    // `a, b := pair()` stays a destructuring Let (not tuple-assign).
    match only("a, b := pair()\n") {
        StmtKind::Let { target, .. } => assert_eq!(target.binder_names().len(), 2),
        other => panic!("expected Let, got {other:?}"),
    }
}

#[test]
fn i64_min_literal_folds_to_int_min() {
    // `-9223372036854775808` is the ONE case where the magnitude 2^63 is legal: the leading
    // minus folds it straight into ExprKind::Int(i64::MIN) — NOT Unary(Neg, ...) (runtime-negating
    // the magnitude would overflow).
    match only("x := -9223372036854775808\n") {
        StmtKind::Let { value, .. } => match value.kind {
            ExprKind::Int(n) => assert_eq!(n, i64::MIN),
            other => panic!("expected Int(i64::MIN), got {other:?}"),
        },
        other => panic!("{other:?}"),
    }
    // A bare (un-negated) 2^63 is out of i64 range → a clear parse error.
    assert!(
        parse_err("x := 9223372036854775808\n")
            .to_string()
            .contains("number too large")
    );
    // 2^63 + 1 is unrepresentable even negated → still errors at lex time.
    assert!(lexer::tokenize("x := -9223372036854775809\n").is_err());
    // The i64::MIN-magnitude fold also works in match-pattern position.
    match only("match x:\n    -9223372036854775808: pass\n    _: pass\n") {
        StmtKind::Match { arms, .. } => {
            assert!(matches!(
                arms[0].pattern,
                Pattern::Literal(LitPattern::Int(n)) if n == i64::MIN
            ));
        }
        other => panic!("expected Match, got {other:?}"),
    }
}

#[test]
fn a_name_is_bound_once_per_target_and_parameter_list() {
    // Go (`a repeated on left side of :=`, `duplicate argument a`) and Rust (E0416, E0415) agree.
    for src in [
        "a, a := (1, 2)\n",
        "a, (b, a) := (1, (2, 3))\n",
        "for a, a in xs:\n    pass\n",
        "ys := [a for a, a in xs]\n",
    ] {
        let e = parse_err(src);
        assert_eq!(
            e.message, "identifier 'a' is bound more than once in this pattern",
            "{src:?}"
        );
    }
    for src in [
        "fn g(a: int, a: int):\n    pass\n",
        "f := fn(a: int, a: int): 1\n",
    ] {
        let e = parse_err(src);
        assert_eq!(
            e.message, "identifier 'a' is bound more than once in this parameter list",
            "{src:?}"
        );
    }
    // `_` may repeat in both.
    parse_ok("_, _ := (1, 2)\n");
    parse_ok("fn g(_: int, _: int):\n    pass\n");
}

/// An enum needs at least one variant: every body with no variant (`pass`, methods only, `test`
/// blocks only, any mix, a `native enum` too) gives the same error at the enum name (line 1); an
/// enum with a variant plus a method stays legal.
#[test]
fn enum_without_variant_is_rejected_for_every_body() {
    let msg = "empty enums are not supported; an enum needs at least one variant";
    let f = "    fn f(self) -> int:\n        return 1\n";
    let t = "    test fn t(self):\n        assert true\n";
    for src in [
        format!("enum Z:\n{f}"),
        "enum Z:\n    pass\n".to_string(),
        format!("enum Z:\n{t}"),
        format!("enum Z:\n{f}{t}"),
        format!("enum Z:\n{t}{f}"),
        format!("enum Z:\n    pass\n{f}"),
        format!("enum Z[T]:\n{f}"),
        "native enum Z:\n    native fn f(self) -> int\n".to_string(),
    ] {
        match try_parse(&src) {
            Err(e) => assert_eq!(e, (1, msg.to_string()), "wrong error for {src:?}"),
            Ok(_) => panic!("variant-less enum parsed: {src:?}"),
        }
    }
    parse_ok(&format!("enum Z:\n    A\n{f}"));
}
