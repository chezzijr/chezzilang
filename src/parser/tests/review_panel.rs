//! Parser tests: regressions from the review-panel findings.
//! Split out of `src/parser/mod.rs`. `super::*` is the parent module.

use super::*;

// ===== regression tests for the review-panel findings =====

/// An inline `if` body must still allow an `else` on the next line (was misparsed).
#[test]
fn inline_expr_body_flag_set() {
    // `fn a(): 10` — inline bare-expr body -> inline_expr_body marker set (implicit return).
    let StmtKind::Fn(f) = only("fn a(): 10\n") else {
        panic!()
    };
    assert!(
        f.inline_expr_body,
        "inline bare-expr body should set the marker"
    );
    // annotated inline-expr body too.
    let StmtKind::Fn(f) = only("fn a() -> int: 10\n") else {
        panic!()
    };
    assert!(f.inline_expr_body);
}

#[test]
fn inline_non_expr_and_multiline_bodies_not_marked() {
    // inline NON-expr stmt (assignment) -> not an implicit return.
    let StmtKind::Fn(f) = only("fn a(): x = 5\n") else {
        panic!()
    };
    assert!(
        !f.inline_expr_body,
        "inline assignment must not set the marker"
    );
    // inline explicit `return` -> not the implicit-return form.
    let StmtKind::Fn(f) = only("fn a(): return 10\n") else {
        panic!()
    };
    assert!(!f.inline_expr_body);
    // a 1-statement MULTILINE expr body -> NOT inline (the Block shape is identical, the marker
    // is what disambiguates).
    let StmtKind::Fn(f) = only("fn a():\n    10\n") else {
        panic!()
    };
    assert!(
        !f.inline_expr_body,
        "a multiline 1-stmt body must not set the marker"
    );
}

#[test]
fn inline_if_then_else() {
    let StmtKind::Fn(f) = only("fn m():\n    if x: y = 1\n    else: z = 2\n") else {
        panic!()
    };
    match &f.body[0].kind {
        StmtKind::If {
            branches,
            else_block,
        } => {
            assert_eq!(branches.len(), 1);
            assert!(else_block.is_some());
        }
        other => panic!("{other:?}"),
    }
}

/// `elif` chains longer than one branch must keep every branch.
#[test]
fn elif_chain_three_branches() {
    match only("if a:\n    f()\nelif b:\n    g()\nelif c:\n    h()\nelse:\n    i()\n") {
        StmtKind::If {
            branches,
            else_block,
        } => {
            assert_eq!(branches.len(), 3);
            assert!(else_block.is_some());
        }
        other => panic!("{other:?}"),
    }
}

/// Assignment to a non-lvalue is a syntax error, not a silently-wrong AST.
#[test]
fn rejects_non_lvalue_assignment() {
    assert!(parse_err("1 = 2\n").message.contains("assignment target"));
    assert!(parse_err("f() = 3\n").message.contains("assignment target"));
    // a field/index target is fine
    assert!(matches!(only("p.x = 1\n"), StmtKind::Assign { .. }));
    assert!(matches!(only("xs[0] = 1\n"), StmtKind::Assign { .. }));
}

/// `:=` left side must be a bare name.
#[test]
fn rejects_walrus_on_non_ident() {
    assert!(parse_err("a.b := 1\n").message.contains("must be a name"));
}

/// Two statements packed onto one physical line is an error (no terminator between them).
#[test]
fn rejects_trailing_tokens() {
    assert!(parse_err("x := 5 y := 6\n").message.contains("end of line"));
}

/// A primary that starts with a non-expression token reports a readable error.
#[test]
fn primary_error_is_readable() {
    let err = parse_err(", 1\n");
    assert!(err.message.contains("unexpected"), "{}", err.message);
    assert!(
        err.message.contains("','"),
        "should name the token: {}",
        err.message
    );
}

/// Error messages render tokens in source form, not Rust enum names.
#[test]
fn error_messages_use_source_spelling() {
    // `for in xs:` — missing loop variable; expected identifier, found keyword `in`
    let err = parse_err("for in xs:\n    f()\n");
    assert!(err.message.contains("'in'"), "{}", err.message);
    assert!(
        !err.message.contains("In"),
        "leaked enum name: {}",
        err.message
    );
}

/// Generic types with multiple arguments parse (the comma loop in `parse_type`).
#[test]
fn generic_type_multiple_args() {
    let StmtKind::Fn(f) = only("fn g(m: Map[str, List[int]]):\n    return\n") else {
        panic!()
    };
    match &f.params[0].ty {
        Some(Type::Generic(name, args, _)) => {
            assert_eq!(name, "Map");
            assert_eq!(args.len(), 2);
            assert_eq!(args[0], Type::named("str"));
            assert_eq!(
                args[1],
                Type::Generic("List".into(), vec![Type::named("int")], Span::default())
            );
        }
        other => panic!("{other:?}"),
    }
}

/// Unary `not` / `-` build `Unary` nodes.
#[test]
fn unary_operators() {
    let StmtKind::Expr(e) = only("not done\n") else {
        panic!()
    };
    assert!(matches!(
        e.kind,
        ExprKind::Unary {
            op: UnaryOp::Not,
            ..
        }
    ));
    let StmtKind::Expr(e) = only("-x\n") else {
        panic!()
    };
    assert!(matches!(
        e.kind,
        ExprKind::Unary {
            op: UnaryOp::Neg,
            ..
        }
    ));
}

/// A fully-chained postfix expression nests correctly: `a.b()[0]?`.
#[test]
fn chained_postfix() {
    let StmtKind::Expr(e) = only("a.b()[0]?\n") else {
        panic!()
    };
    // Try( Index( Call( Field(a, b) ), 0 ) )
    let ExprKind::Try(inner) = e.kind else {
        panic!("outermost should be Try")
    };
    let ExprKind::Index { obj, .. } = inner.kind else {
        panic!("next should be Index")
    };
    let ExprKind::Call { callee, .. } = obj.kind else {
        panic!("next should be Call")
    };
    assert!(matches!(callee.kind, ExprKind::Field { .. }));
}

/// Expression spans point at the start of the construct (lhs for binaries, primary for postfix).
#[test]
fn expr_spans_are_populated() {
    // `a + b` indented in a fn body: the binary's span is the lhs `a` at line 2, col 5.
    let StmtKind::Fn(f) = only("fn m():\n    a + b\n") else {
        panic!()
    };
    let StmtKind::Expr(e) = &f.body[0].kind else {
        panic!()
    };
    assert!(matches!(e.kind, ExprKind::Binary { .. }));
    assert_eq!(e.span.line, 2);
    assert_eq!(e.span.col, 5);
}

/// Closure without a declared return type parses (`ret: None`).
#[test]
fn closure_without_return_type() {
    let StmtKind::Expr(e) = only("nums.map(fn(x): x * 2)\n") else {
        panic!()
    };
    let ExprKind::Call { args, .. } = e.kind else {
        panic!()
    };
    match &args[0].kind {
        ExprKind::Closure { ret, params, .. } => {
            assert!(ret.is_none());
            assert!(params[0].ty.is_none());
        }
        other => panic!("{other:?}"),
    }
}

/// `T?` is sugar for `Option[T]` in type position.
#[test]
fn optional_type_shorthand() {
    let StmtKind::Fn(decl) = only("fn f(x: int?):\n    return x\n") else {
        panic!()
    };
    assert_eq!(
        decl.params[0].ty,
        Some(Type::Generic(
            "Option".into(),
            vec![Type::named("int")],
            Span::default()
        ))
    );
}

/// `T!` is sugar for `Result[T]` in type position.
#[test]
fn result_type_shorthand() {
    let StmtKind::Fn(decl) = only("fn f() -> int!:\n    return ?1\n") else {
        panic!()
    };
    assert_eq!(
        decl.ret,
        Some(Type::Generic(
            "Result".into(),
            vec![Type::named("int")],
            Span::default()
        ))
    );
}

/// The shorthand applies to a fully-parsed base type, including a generic like `List[int]`.
#[test]
fn optional_shorthand_on_generic_base() {
    let StmtKind::Fn(decl) = only("fn f(x: List[int]?):\n    return x\n") else {
        panic!()
    };
    assert_eq!(
        decl.params[0].ty,
        Some(Type::Generic(
            "Option".into(),
            vec![Type::Generic(
                "List".into(),
                vec![Type::named("int")],
                Span::default()
            )],
            Span::default()
        ))
    );
}

/// A module-qualified type annotation: `x: geo.Point` parses to `Type::Qualified`.
#[test]
fn qualified_type_annotation() {
    let StmtKind::Fn(decl) = only("fn f(x: geo.Point):\n    return x\n") else {
        panic!()
    };
    assert_eq!(
        decl.params[0].ty,
        Some(Type::Qualified {
            module: "geo".into(),
            name: "Point".into(),
            args: vec![],
        })
    );
}

/// A generic qualified type: `g.Box[int]` carries its type args.
#[test]
fn qualified_generic_type_annotation() {
    let StmtKind::Fn(decl) = only("fn f(x: g.Box[int]):\n    return x\n") else {
        panic!()
    };
    assert_eq!(
        decl.params[0].ty,
        Some(Type::Qualified {
            module: "g".into(),
            name: "Box".into(),
            args: vec![Type::named("int")],
        })
    );
}

/// A qualified type nested inside a generic: `List[geo.Point]`.
#[test]
fn qualified_type_inside_generic() {
    let StmtKind::Fn(decl) = only("fn f(x: List[geo.Point]):\n    return x\n") else {
        panic!()
    };
    assert_eq!(
        decl.params[0].ty,
        Some(Type::Generic(
            "List".into(),
            vec![Type::Qualified {
                module: "geo".into(),
                name: "Point".into(),
                args: vec![],
            }],
            Span::default()
        ))
    );
}

/// A function type in parameter position: `f: fn(int) -> int`.
#[test]
fn fn_param_type() {
    let StmtKind::Fn(f) = only("fn apply(f: fn(int) -> int, v: int) -> int:\n    return f(v)\n")
    else {
        panic!()
    };
    match &f.params[0].ty {
        Some(Type::Func { params, ret, .. }) => {
            assert_eq!(params.as_slice(), &[Type::named("int")]);
            assert_eq!(**ret, Type::named("int"));
        }
        other => panic!("{other:?}"),
    }
}

/// A zero-argument function type: `fn() -> int`.
#[test]
fn fn_type_zero_args() {
    let StmtKind::Fn(f) = only("fn g(f: fn() -> int):\n    return f()\n") else {
        panic!()
    };
    match &f.params[0].ty {
        Some(Type::Func { params, ret, .. }) => {
            assert!(params.is_empty());
            assert_eq!(**ret, Type::named("int"));
        }
        other => panic!("{other:?}"),
    }
}

/// `match` in expression position parses to `ExprKind::Match` with value-expression arms.
#[test]
fn match_expression_parses() {
    let StmtKind::Let { value, .. } = only("x := match s:\n    ?v: v\n    None: 0\n") else {
        panic!()
    };
    let ExprKind::Match { arms, .. } = value.kind else {
        panic!("{value:?}")
    };
    assert_eq!(arms.len(), 2);
    assert!(matches!(arms[1].body.kind, ExprKind::Int(0)));
}

/// `if c: a else: b` in expression position parses to `ExprKind::IfElse` (inline, else required).
#[test]
fn if_expression_parses() {
    let StmtKind::Let { value, .. } = only("x := if c: 1 else: 2\n") else {
        panic!()
    };
    let ExprKind::IfElse { then, els, .. } = value.kind else {
        panic!("{value:?}")
    };
    assert!(matches!(then.kind, ExprKind::Int(1)));
    assert!(matches!(els.kind, ExprKind::Int(2)));
}

#[test]
fn if_expression_requires_else() {
    assert!(parse_err("x := if c: 1\n").message.contains("else"));
}

/// `if c: 1 elif d: 2 else: 3` chains in expression position, nesting right-associatively:
/// the outer `els` is itself an `IfElse` (`then`=2, `els`=3); the outer `then` is 1.
#[test]
fn if_expr_elif_chain() {
    let StmtKind::Let { value, .. } = only("x := if c: 1 elif d: 2 else: 3\n") else {
        panic!()
    };
    let ExprKind::IfElse { then, els, .. } = value.kind else {
        panic!("{value:?}")
    };
    assert!(matches!(then.kind, ExprKind::Int(1)));
    let ExprKind::IfElse { then, els, .. } = els.kind else {
        panic!("else branch should be a nested IfElse, got {els:?}")
    };
    assert!(matches!(then.kind, ExprKind::Int(2)));
    assert!(matches!(els.kind, ExprKind::Int(3)));
}

/// A chained expression `elif` still demands a final `else` (no optional-else leak).
#[test]
fn if_expr_elif_still_requires_final_else() {
    assert!(
        parse_err("x := if c: 1 elif d: 2\n")
            .message
            .contains("else")
    );
}

/// `else if` is no longer valid in expression position either — `elif` is required.
#[test]
fn if_expr_rejects_bare_else_if() {
    assert!(
        parse_err("x := if c: 1 else if d: 2 else: 3\n")
            .message
            .contains(':')
    );
}

/// Guards the `expect_stmt_end` Dedent-acceptance: a sibling statement may follow a
/// (Dedent-terminated) match-expression value — both statements must parse.
#[test]
fn match_expr_followed_by_sibling_statement() {
    let m = parse_ok("x := match s:\n    A: 1\n    B: 2\ny := x\n");
    assert_eq!(m.stmts.len(), 2);
}

/// match-expr as the last statement in a block: the arm Dedent and the block Dedent stack.
#[test]
fn match_expr_last_in_fn_body() {
    let m =
        parse_ok("fn f(s: Color):\n    x := match s:\n        A: 1\n        B: 2\n    print(x)\n");
    assert_eq!(m.stmts.len(), 1); // just the fn
}

/// An if-expression composes inside a larger expression.
#[test]
fn if_expr_nested_in_arithmetic() {
    let StmtKind::Let { value, .. } = only("x := 1 + if c: 2 else: 3\n") else {
        panic!()
    };
    assert!(matches!(value.kind, ExprKind::Binary { .. }));
}

/// Arm bodies are single-line expressions; an indented block body is a parse error.
#[test]
fn match_expr_arm_body_must_be_inline() {
    parse_err("x := match s:\n    A:\n        1\n    B: 2\n");
}

/// Prefix `!e` is an error value (TICKET-227); an infix `!` has no meaning and must stay a
/// parse error, not silently consumed.
#[test]
fn bang_is_prefix_only_in_expressions() {
    parse_ok("x := !y\n");
    assert!(parse_err("z := 1 ! 2\n").message.contains("'!'"));
}

/// Deeply nested input returns a `ParseError` instead of overflowing the native stack —
/// across all five recursive entry points: parens (parse_bp), unary chains (parse_unary),
/// nested generics (parse_type), nested blocks (parse_stmt), and nested match patterns
/// (parse_pattern_impl — variant payloads + tuple elements).
///
/// This is the **guard-fires** test: every production must REJECT rather than abort. It is no
/// longer the sizing oracle — [`max_depth_boundary_accepts_then_rejects`] is (it bisects the real
/// boundary per production), and `check_errors_json::worst_accepted_nesting_never_signal_crashes`
/// is the margin oracle (worst accepted program, end to end, through the real binary).
#[test]
fn deep_nesting_errors_not_crash() {
    // At `MAX_DEPTH = 512` the guard fires ~4 MB into the parser's stack, which is over a test
    // worker's ~2 MiB default — so the body runs on the production front-end stack, the same one
    // every real parse path now takes (W7-50 Task 1). Nesting counts are `MAX_DEPTH * 2` so every
    // production is comfortably over its own per-level cost (the cheapest is ~1 unit/level).
    let n = MAX_DEPTH * 2;
    crate::on_frontend_stack(move || deep_nesting_errors_not_crash_body(n));
}

fn deep_nesting_errors_not_crash_body(n: usize) {
    let paren = format!("x := {}1{}\n", "(".repeat(n), ")".repeat(n));
    assert!(
        parse(lexer::tokenize(&paren).unwrap())
            .unwrap_err()
            .message
            .contains("too deeply")
    );

    let unary = format!("x := {}y\n", "not ".repeat(n));
    assert!(
        parse(lexer::tokenize(&unary).unwrap())
            .unwrap_err()
            .message
            .contains("too deeply")
    );

    let generic = format!("z: {}int{} = w\n", "List[".repeat(n), "]".repeat(n));
    assert!(
        parse(lexer::tokenize(&generic).unwrap())
            .unwrap_err()
            .message
            .contains("too deeply")
    );

    let blocks = {
        let mut s = String::new();
        for i in 0..n {
            s.push_str(&"    ".repeat(i));
            s.push_str("if x:\n");
        }
        s.push_str(&"    ".repeat(n));
        s.push_str("y = 1\n");
        s
    };
    assert!(
        parse(lexer::tokenize(&blocks).unwrap())
            .unwrap_err()
            .message
            .contains("too deeply")
    );

    // Deeply nested match-arm patterns (the un-guarded fifth entry point): deep VARIANT
    // payload nesting `E.S(E.S(...))` and deep TUPLE nesting `((( ... )))`, both inside a
    // `match` scrutinee context (a bare `(((...)))` would route through parse_bp — the wrong
    // guard). Before the guard these overflow the native stack (SIGABRT); after, a clean error.
    let variant_pat = format!(
        "x := match o:\n    {}0{}: 1\n    _: 0\n",
        "E.S(".repeat(n),
        ")".repeat(n)
    );
    assert!(
        parse(lexer::tokenize(&variant_pat).unwrap())
            .unwrap_err()
            .message
            .contains("too deeply")
    );

    let tuple_pat = format!(
        "x := match o:\n    {}0{}: 1\n    _: 0\n",
        "(".repeat(n),
        ")".repeat(n)
    );
    assert!(
        parse(lexer::tokenize(&tuple_pat).unwrap())
            .unwrap_err()
            .message
            .contains("too deeply")
    );
}

/// The ITERATIVE sibling of `deep_nesting_errors_not_crash`, and like it a **guard-fires** test,
/// not a sizing oracle: a long left-associative binary chain (`1+1+…`) and a long postfix chain
/// (`x.f.f…`, `a[0][0]…`) fold in the `parse_bp` / `parse_postfix` LOOPS, which never bump
/// `self.depth`, so the `MAX_DEPTH` guard cannot see them. [`MAX_AST_DEPTH`] over the synthesized
/// `fold_depth` must reject a chain past the cap (else the deep left-leaning AST overflows the
/// recursive front-end walkers), while a chain UNDER the cap parses fine.
///
/// Three properties pinned below. Two the per-loop `MAX_CHAIN_DEPTH` counter this replaced could
/// not express: sibling BREADTH still costs nothing (the `max`-combine), and folds COMPOSE up the
/// spine — `f(g(deep) + 1 + 1 + …)` per level is rejected once the levels add up, which the old
/// per-loop proxy (and an ambient counter decremented on loop exit) both let through. Third, the
/// DOUBLE FOLD: one level can spend BOTH loops (`.f…` through `parse_postfix`, `+1…` through
/// `parse_bp`) for ~2N nodes rather than N — measured as a ratio between two bisections, because
/// that unmeasured property is what made W7-50 ship a false "no regression" claim.
#[test]
fn deep_iterative_chains_error_not_crash() {
    // Runs on the production front-end stack: the accepted side of these fixtures builds a
    // 16 000-node AST whose recursive `Drop` is the same stack hazard as the parse.
    crate::on_frontend_stack(deep_iterative_chains_error_not_crash_body);
}

fn deep_iterative_chains_error_not_crash_body() {
    // Binary chain over the cap → clean "too deeply", not an accepted N-deep AST.
    let bin = format!("x := 1{}\n", "+1".repeat(MAX_AST_DEPTH + 5));
    assert!(
        parse(lexer::tokenize(&bin).unwrap())
            .unwrap_err()
            .message
            .contains("too deeply")
    );
    // Postfix field chain over the cap.
    let field = format!("x := a{}\n", ".f".repeat(MAX_AST_DEPTH + 5));
    assert!(
        parse(lexer::tokenize(&field).unwrap())
            .unwrap_err()
            .message
            .contains("too deeply")
    );
    // Postfix index chain over the cap.
    let index = format!("x := a{}\n", "[0]".repeat(MAX_AST_DEPTH + 5));
    assert!(
        parse(lexer::tokenize(&index).unwrap())
            .unwrap_err()
            .message
            .contains("too deeply")
    );
    // The THIRD iterative fold loop, and the one the per-production sizing oracle skipped:
    // `parse_type_postfix` (`T?`/`T!`) wraps a `Type::Generic` per operator and bumps no
    // recursion counter, exactly like the two expression loops. Before it was counted,
    // `fn f(x: int` + `?!` × 350 000 + `)` reached the checker's recursive type walk and the
    // PROCESS died — `fatal runtime error: stack overflow, aborting` — on `b1307258` and on
    // `dfdc7a1b` alike (250 000 was still fine, so it is a real boundary, not a fixture artefact).
    // (`?!` alternating, not `?` repeated: the LEXER folds `??` into the null-coalescing token.)
    let ty_chain = format!(
        "fn f(x: int{}) -> int:\n    return 0\n",
        "?!".repeat(MAX_AST_DEPTH / 2 + 5)
    );
    assert!(
        parse(lexer::tokenize(&ty_chain).unwrap())
            .unwrap_err()
            .message
            .contains("too deeply"),
        "a type-postfix chain over the cap must be a clean refusal, not a host abort"
    );
    // …and the same shape under the cap still parses (the loop must not over-reject).
    let ty_ok = format!(
        "fn f(x: int{}) -> int:\n    return 0\n",
        "?!".repeat((MAX_AST_DEPTH - 100) / 2)
    );
    assert!(parse(lexer::tokenize(&ty_ok).unwrap()).is_ok());
    // …and type BREADTH is not depth: many separate one-postfix annotations are `max`-combined,
    // not summed, so a signature far wider than the cap is fine.
    let wide_ty = format!(
        "fn f({}) -> int:\n    return 0\n",
        (0..MAX_AST_DEPTH * 2)
            .map(|i| format!("p{i}: int?"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert!(parse(lexer::tokenize(&wide_ty).unwrap()).is_ok());

    // A chain WELL UNDER the cap parses cleanly (no over-rejection of long-but-legal chains).
    let ok = format!("x := 1{}\n", "+1".repeat(MAX_AST_DEPTH - 100));
    assert!(parse(lexer::tokenize(&ok).unwrap()).is_ok());
    // Sibling BREADTH must NOT be conflated with chain depth: a wide list literal far exceeding
    // the cap in element count is fine (each element is its own shallow, `max`-combined parse).
    let wide = format!("x := [{}]\n", "1,".repeat(MAX_AST_DEPTH * 3));
    assert!(parse(lexer::tokenize(&wide).unwrap()).is_ok());
    // …and neither is a WIDE argument list (the other breadth axis, through `parse_postfix`).
    let wide_args = format!("x := f({}1)\n", "1,".repeat(MAX_AST_DEPTH * 3));
    assert!(parse(lexer::tokenize(&wide_args).unwrap()).is_ok());

    // COMPOSITION: folds stacked on top of an already-deep subtree add. Each level of
    // `f(g(inner) + 1 …×99)` contributes ~101 AST levels, so ~160 levels blow the 16 000 cap even
    // though no single loop folds more than 101 times and no ambient counter ever holds more
    // than one level's worth (the folds run AFTER the descent has popped its own counts).
    let compose = |lv: usize| {
        let mut s = String::from("0");
        for _ in 0..lv {
            s = format!("f(g({s}){})", "+1".repeat(99));
        }
        format!("x := {s}\n")
    };
    assert!(parse(lexer::tokenize(&compose(20)).unwrap()).is_ok());
    assert!(
        parse(lexer::tokenize(&compose(400)).unwrap())
            .unwrap_err()
            .message
            .contains("too deeply"),
        "composed fold depth must be bounded, not just per-loop chain length"
    );

    // DOUBLE FOLD: one nesting level can spend BOTH fold loops — `parse_postfix`'s (`.f…`) and
    // `parse_bp`'s (`+1…`) — so it costs ~2N nodes, not N. Measured here, not re-guessed by the
    // next person: this is the property whose absence made W7-50 first ship a FALSE "no
    // regression" claim in five places. The before-ceiling had been measured on the paren+infix
    // shape (~499 nodes/level) when the real worst shape is this one (~998/level): re-bisected
    // on release binaries, `b1307258` accepted k = 30 (~29 940 nodes) where HEAD accepts k = 16
    // (~15 968). `MAX_AST_DEPTH` is a deliberate NARROWING — see that constant.
    //
    // The assertion is a RATIO — the double-fold shape must accept about HALF the levels of the
    // single-fold one — so a refactor that makes one of the two loops stop counting converges
    // the two bisections and fails this test.
    //
    // The ratio holds only while BOTH bisections are bounded by `MAX_AST_DEPTH`. Each `(…)`
    // level also costs ~2 frames of parse recursion, so the single-fold bisection saturates
    // against `MAX_DEPTH` at ~254 levels; past that it stops growing with `MAX_AST_DEPTH` while
    // the double-fold one keeps shrinking, and the ratio assert goes red blaming a fold loop
    // when the CONSTANT is what moved (measured: `MAX_AST_DEPTH = 60_000` fails exactly so).
    // Hence the explicit precondition below. It is a `const` block, so past ~51 200 the lib test
    // target fails to BUILD with that message rather than going red at run time — earlier and
    // louder, but note it is a compile error, not a test failure.
    const N: usize = 200;
    const {
        assert!(
            MAX_AST_DEPTH / N < MAX_DEPTH / 2,
            "PRECONDITION, not a fold-loop bug: the ratio assert below needs BOTH bisections \
                 bounded by MAX_AST_DEPTH, but MAX_AST_DEPTH/N has reached the MAX_DEPTH \
                 paren-recursion saturation point (~MAX_DEPTH/2 levels). Raise N, or bound the \
                 claim — the fold loops are not at fault."
        )
    };
    let bisect = |mk: &dyn Fn(usize) -> String| -> usize {
        let accepts = |k: usize| parse(lexer::tokenize(&mk(k)).unwrap()).is_ok();
        let (mut lo, mut hi) = (1usize, 2usize);
        while accepts(hi) {
            lo = hi;
            hi *= 2;
        }
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if accepts(mid) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    };
    let nest = |k: usize, tail: &str| {
        let mut s = String::from("1");
        for _ in 0..k {
            s = format!("({s}{tail})");
        }
        format!("x := {s}\n")
    };
    let single = bisect(&|k| nest(k, &"+1".repeat(N)));
    let double = bisect(&|k| nest(k, &format!("{}{}", ".f".repeat(N), "+1".repeat(N))));
    eprintln!(
        "DOUBLE-FOLD: single-fold levels = {single}, double-fold levels = {double} (N = {N})"
    );
    assert!(
        double < single,
        "a level spending BOTH fold loops must accept strictly fewer levels than one spending \
             only the infix loop, else one of the two loops is not counted \
             (single={single}, double={double})"
    );
    assert!(
        double * 2 + 2 >= single && double * 2 <= single + 2,
        "each fold loop must contribute its own {N} nodes per level, so the double-fold shape \
             should accept ~half the levels of the single-fold one (single={single}, double={double})"
    );
    assert!(
        parse(
            lexer::tokenize(&nest(
                double + 1,
                &format!("{}{}", ".f".repeat(N), "+1".repeat(N))
            ))
            .unwrap()
        )
        .unwrap_err()
        .message
        .contains("too deeply"),
        "the double-fold boundary must be the depth guard, not some other parse error"
    );
}

/// FLOOR DETECTOR for [`MAX_AST_DEPTH`] — the only depth test that fails if the constant is
/// silently **shrunk**.
///
/// Every other depth test is structurally blind to that.
/// `max_depth_boundary_accepts_then_rejects` sizes [`MAX_DEPTH`] (all seven of its forms are
/// recursion-guarded); `deep_iterative_chains_error_not_crash` above and
/// `check_errors_json::worst_accepted_nesting_never_signal_crashes` both DERIVE their fixtures
/// from `MAX_AST_DEPTH`, so they stay green at any value it takes. The only other floor in the
/// tree is a hard-coded 12 500 in `vm::tests::deep_accepted_chains_run_without_stack_overflow`
/// — exactly the kind of rotting number W7-50 set out to abolish.
///
/// **The fixture is FIXED on purpose. Do not re-derive it from the constant** — deriving it is
/// what makes a test unable to see the constant move. It pins the floor the shipped 16 000
/// promises.
///
/// **When this fails,** `MAX_AST_DEPTH` was lowered, which narrows what programs parse. Treat it
/// the way 16 000 was: bisect the worst accepted AST (`x := ` + *k* × `( … .f×499 +1×499 )`, the
/// double-fold shape) on the pre-change binary AND after, state both numbers and the new margin
/// against the measured ~33 100-node walker cliff, and say plainly that programs between the two
/// depths stop parsing. Do not just retune the number below.
#[test]
fn max_ast_depth_floor_holds() {
    crate::on_frontend_stack(|| {
        // 15 950, not 15 000: the flat chain's last accepted length is 15 997 (bisected,
        // release), so 15 000 left a 998-wide blind spot in which `MAX_AST_DEPTH` could be
        // shrunk to 15 100 with every depth test still green. This is ~0.3% off the real
        // ceiling, close enough that any meaningful shrink trips it and far enough that a
        // one-node accounting change in the parser does not.
        let flat = format!("x := 1{}\n", "+1".repeat(15_950));
        assert!(
            parse(lexer::tokenize(&flat).unwrap()).is_ok(),
            "a flat 15 950-fold chain must still parse — MAX_AST_DEPTH has been shrunk below \
                 the floor it shipped promising (16 000). See this test's doc comment."
        );
        // NOTE the floor's GLOBAL half is not testable here: `parse` leaves a `{…}` fragment as
        // raw literal text, so the composed budget (`desugar`, see `MAX_AST_DEPTH`) never runs.
        // `check_errors_json::composed_interp_depth_is_bounded_globally` covers that end to end.
    });
}

/// **THE SIZING ORACLE for [`MAX_DEPTH`].** Bisects the accepted nesting count for each guarded
/// production and asserts (a) a boundary exists at all — `k` parses, `k+1` rejects with
/// `"too deeply"`, never a host abort — and (b) `k` clears CPython 3.14's own 200-level limit for
/// the four directly comparable forms. The boundary is MEASURED, never hard-coded: a hard-coded
/// number rots into prose the moment a parser frame changes width, which is exactly how
/// `MAX_DEPTH = 64` came to sit one level from the cliff (W7-49/W7-50).
///
/// On failure the message prints the measured `k`, so a red run says "update the constant", not
/// "go chase the parser".
#[test]
fn max_depth_boundary_accepts_then_rejects() {
    crate::on_frontend_stack(max_depth_boundary_body);
}

fn max_depth_boundary_body() {
    /// (name, source builder, CPython 3.14's limit for the comparable construct — 0 = no peer)
    type Form = (&'static str, Box<dyn Fn(usize) -> String>, usize);
    let forms: Vec<Form> = vec![
        (
            "parens",
            Box::new(|n| format!("x := {}1{}\n", "(".repeat(n), ")".repeat(n))),
            200,
        ),
        (
            "lists",
            Box::new(|n| format!("x := {}{}\n", "[".repeat(n), "]".repeat(n))),
            200,
        ),
        (
            "calls",
            Box::new(|n| format!("x := {}1{}\n", "f(".repeat(n), ")".repeat(n))),
            200,
        ),
        (
            "blocks",
            Box::new(|n| {
                let mut s = String::new();
                for i in 0..n {
                    s.push_str(&"    ".repeat(i));
                    s.push_str("if x:\n");
                }
                s.push_str(&"    ".repeat(n));
                s.push_str("y = 1\n");
                s
            }),
            // CPython's own nested-`if` ceiling is 99 (its compiler's block depth, lower than the
            // 200 parser limit); Chezzi must clear the 200 bar here too, not merely the 99.
            200,
        ),
        (
            "unary",
            Box::new(|n| format!("x := {}y\n", "not ".repeat(n))),
            200,
        ),
        (
            "generic types",
            Box::new(|n| format!("z: {}int{} = w\n", "List[".repeat(n), "]".repeat(n))),
            0,
        ),
        (
            "match patterns",
            Box::new(|n| {
                format!(
                    "x := match o:\n    {}0{}: 1\n    _: 0\n",
                    "E.S(".repeat(n),
                    ")".repeat(n)
                )
            }),
            0,
        ),
    ];

    for (name, build, cpython) in forms {
        let parses = |n: usize| parse(lexer::tokenize(&build(n)).unwrap()).is_ok();
        assert!(parses(1), "{name}: the 1-level case must parse");
        // Bisect: double to find a rejecting bound, then narrow. `MAX_DEPTH * 8` is far past any
        // per-level cost ≥ 1/8 of a depth unit, so the doubling always terminates.
        let mut lo = 1usize;
        let mut hi = MAX_DEPTH * 8;
        assert!(!parses(hi), "{name}: no boundary below {hi} levels");
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if parses(mid) { lo = mid } else { hi = mid }
        }
        let k = lo;
        eprintln!("BOUNDARY {name}: max accepted nesting = {k} (CPython {cpython})");
        let err = parse(lexer::tokenize(&build(k + 1)).unwrap()).unwrap_err();
        assert!(
            err.message.contains("too deeply"),
            "{name}: the level past the boundary (k = {k}) must be a depth diagnostic, got: {}",
            err.message
        );
        if cpython > 0 {
            assert!(
                k >= cpython,
                "{name}: max accepted nesting is {k}, below CPython 3.14's {cpython} — \
                     raise parser::MAX_DEPTH (currently {MAX_DEPTH}) until it clears"
            );
        }
    }
}

/// A compound statement cannot be the inline body of a block — that would make a trailing
/// `else` ambiguous. Force indentation instead.
#[test]
fn rejects_inline_nested_block() {
    // an `if` after an inline `:` is the one-line if-expression, and its `else` is mandatory
    let err = parse_err("fn m():\n    if a: if b: x = 1\n");
    assert!(err.message.contains("expected 'else'"), "{}", err.message);
}

/// Strengthened golden check: walk into hello.chz bodies, not just top-level shape.
#[test]
fn hello_example_inner_structure() {
    let src = include_str!("../../../examples/hello.chz");
    let module = parse(lexer::tokenize(src).unwrap()).unwrap();

    // index 4 = `fn safe_div(...) -> Result[int]:` — check its return type and `?` usage.
    let StmtKind::Fn(safe_div) = &module.stmts[4].kind else {
        panic!("stmts[4] should be safe_div")
    };
    assert_eq!(safe_div.name, "safe_div");
    assert_eq!(
        safe_div.ret,
        Some(Type::Generic(
            "Result".into(),
            vec![Type::named("int")],
            Span::default()
        ))
    );
    // body: `if b == 0:` then `return Ok(a / b)`
    assert!(matches!(safe_div.body[0].kind, StmtKind::If { .. }));
    assert!(matches!(safe_div.body[1].kind, StmtKind::Return(Some(_))));

    // index 3 = `fn area(s: Shape) -> float:` containing a `match` with 2 arms.
    let StmtKind::Fn(area) = &module.stmts[3].kind else {
        panic!("stmts[3] should be area")
    };
    let StmtKind::Match { arms, .. } = &area.body[0].kind else {
        panic!("area body should be a match")
    };
    assert_eq!(arms.len(), 2);
    match &arms[0].pattern {
        Pattern::Variant {
            id: _,
            name,
            bindings,
            enum_name,
            module_name,
        } => {
            assert_eq!(name, "Circle");
            assert_eq!(enum_name.as_deref(), Some("Shape"));
            assert_eq!(module_name, &None);
            assert_eq!(bindings.len(), 1);
            assert!(matches!(&bindings[0], Pattern::Ident(n, _, _) if n == "r"));
        }
        other => panic!("{other:?}"),
    }
}
