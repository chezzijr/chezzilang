//! Parser tests: the pipe operator, tuples and destructuring, default and named args, or-patterns.
//! Split out of `src/parser/mod.rs`. `super::*` is the parent module.

use super::*;

// ===== M6: pipe operator desugars to a call =====

#[test]
fn explicit_type_args_parse() {
    // `max[int](3, 7)` → a Call carrying one type arg.
    let v = let_value("x := max[int](3, 7)\n");
    let ExprKind::Call {
        callee,
        args,
        type_args,
        ..
    } = v.kind
    else {
        panic!("expected a Call, got {:?}", v.kind)
    };
    assert!(matches!(callee.kind, ExprKind::Ident(ref n) if n == "max"));
    assert_eq!(args.len(), 2);
    assert_eq!(type_args, vec![Type::named("int")]);

    // Multi-arg, incl. a compound type — only expressible as type args (comma is not an index).
    let v = let_value("p := Pair[int, str](1, \"a\")\n");
    let ExprKind::Call { type_args, .. } = v.kind else {
        panic!("expected a Call, got {:?}", v.kind)
    };
    assert_eq!(type_args, vec![Type::named("int"), Type::named("str")]);
}

#[test]
fn index_then_call_still_index() {
    // A numeric subscript is NOT stolen — `fns[0](5)` stays index-then-call.
    let v = let_value("x := fns[0](5)\n");
    let ExprKind::Call {
        callee, type_args, ..
    } = v.kind
    else {
        panic!("expected a Call, got {:?}", v.kind)
    };
    assert!(type_args.is_empty());
    assert!(matches!(callee.kind, ExprKind::Index { .. }));

    // A bare index with no trailing call stays an index.
    let v = let_value("x := arr[i]\n");
    assert!(matches!(v.kind, ExprKind::Index { .. }));
}

#[test]
fn speculative_type_arg_steal_does_not_leak_recursion_depth() {
    // Each bare-name index (`a[0]`) attempts the type-arg steal, which speculatively calls
    // `parse_type` and backtracks. That must NOT leak the recursion-depth counter, else many
    // plain indexes spuriously trip MAX_DEPTH. Far more indexes than MAX_DEPTH, all valid.
    let mut src = String::from("a := [1, 2, 3]\n");
    for i in 0..200 {
        src.push_str(&format!("b{i} := a[0]\n"));
    }
    parse_ok(&src);
}

#[test]
fn pipe_desugars_to_call_with_lhs_first() {
    // `x := 5 |> inc()` ⇒ `inc(5)`
    let v = let_value("x := 5 |> inc()\n");
    let ExprKind::Call { callee, args, .. } = v.kind else {
        panic!("expected a Call, got {:?}", v.kind)
    };
    assert!(matches!(callee.kind, ExprKind::Ident(ref n) if n == "inc"));
    assert_eq!(args.len(), 1);
    assert_eq!(args[0].kind, ExprKind::Int(5));
}

#[test]
fn pipe_prepends_before_existing_args() {
    // `x := 5 |> add(2)` ⇒ `add(5, 2)`
    let v = let_value("x := 5 |> add(2)\n");
    let ExprKind::Call { args, .. } = v.kind else {
        panic!("expected a Call, got {:?}", v.kind)
    };
    assert_eq!(args.len(), 2);
    assert_eq!(args[0].kind, ExprKind::Int(5));
    assert_eq!(args[1].kind, ExprKind::Int(2));
}

#[test]
fn pipe_chain_is_left_associative() {
    // `x := 5 |> inc() |> dbl()` ⇒ `dbl(inc(5))`
    let v = let_value("x := 5 |> inc() |> dbl()\n");
    let ExprKind::Call { callee, args, .. } = v.kind else {
        panic!("expected outer Call, got {:?}", v.kind)
    };
    assert!(matches!(callee.kind, ExprKind::Ident(ref n) if n == "dbl"));
    assert_eq!(args.len(), 1);
    let ExprKind::Call {
        callee: inner_callee,
        args: inner_args,
        ..
    } = &args[0].kind
    else {
        panic!("expected inner Call, got {:?}", args[0].kind)
    };
    assert!(matches!(inner_callee.kind, ExprKind::Ident(ref n) if n == "inc"));
    assert_eq!(inner_args[0].kind, ExprKind::Int(5));
}

#[test]
fn pipe_non_call_rhs_rejected() {
    let e = parse_err("x := 5 |> 7\n");
    assert!(
        e.to_string()
            .contains("right side of '|>' must be a function call"),
        "{e}"
    );
}

#[test]
fn pipe_bare_identifier_rhs_rejected() {
    let e = parse_err("x := 5 |> f\n");
    assert!(
        e.to_string()
            .contains("right side of '|>' must be a function call"),
        "{e}"
    );
}

// ===== gap #8: tuples + multi-return + destructuring =====

#[test]
fn tuple_literal_two_elements() {
    let StmtKind::Expr(e) = only("(1, 2)\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Tuple(elems) => {
            assert_eq!(elems.len(), 2);
            assert_eq!(elems[0].kind, ExprKind::Int(1));
            assert_eq!(elems[1].kind, ExprKind::Int(2));
        }
        other => panic!("{other:?}"),
    }
}

/// `(1 + 2)` is grouping, not a 1-tuple — it stays a `Binary`.
#[test]
fn paren_single_expr_is_grouping() {
    let StmtKind::Expr(e) = only("(1 + 2)\n") else {
        panic!()
    };
    assert!(matches!(
        e.kind,
        ExprKind::Binary {
            op: BinaryOp::Add,
            ..
        }
    ));
}

/// `(e,)` is a 1-element tuple (distinct from grouping `(e)`).
#[test]
fn one_element_tuple_parses() {
    let StmtKind::Expr(e) = only("(1,)\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Tuple(elems) => {
            assert_eq!(elems.len(), 1);
            assert_eq!(elems[0].kind, ExprKind::Int(1));
        }
        other => panic!("expected 1-tuple, got {other:?}"),
    }
}

/// `(x,)` is a 1-tuple, `(x)` is grouping (bare expr), `(x, y,)` is a 2-tuple.
#[test]
fn tuple_one_two_grouping_trio() {
    let StmtKind::Expr(e1) = only("(x,)\n") else {
        panic!()
    };
    assert!(matches!(&e1.kind, ExprKind::Tuple(es) if es.len() == 1));
    let StmtKind::Expr(e2) = only("(x)\n") else {
        panic!()
    };
    assert!(
        matches!(e2.kind, ExprKind::Ident(_)),
        "grouping, got {:?}",
        e2.kind
    );
    let StmtKind::Expr(e3) = only("(x, y,)\n") else {
        panic!()
    };
    assert!(matches!(&e3.kind, ExprKind::Tuple(es) if es.len() == 2));
    // Lone-comma form still errors (parse_err asserts a parse failure occurred).
    parse_err("(,)\n");
}

/// Optional trailing comma on list/map/set produces identical AST and `[,]`/`{,}` still error.
#[test]
fn collection_trailing_comma_same_ast() {
    // list
    let StmtKind::Expr(a) = only("[1, 2]\n") else {
        panic!()
    };
    let StmtKind::Expr(b) = only("[1, 2,]\n") else {
        panic!()
    };
    assert_eq!(a.kind, b.kind);
    assert!(matches!(&b.kind, ExprKind::List(es, _) if es.len() == 2));
    parse_err("[,]\n");
    // map
    let StmtKind::Expr(a) = only("{\"a\": 1}\n") else {
        panic!()
    };
    let StmtKind::Expr(b) = only("{\"a\": 1,}\n") else {
        panic!()
    };
    assert_eq!(a.kind, b.kind);
    assert!(matches!(&b.kind, ExprKind::Map(es) if es.len() == 1));
    parse_err("{,}\n");
    // set
    let StmtKind::Expr(a) = only("{1, 2}\n") else {
        panic!()
    };
    let StmtKind::Expr(b) = only("{1, 2,}\n") else {
        panic!()
    };
    assert_eq!(a.kind, b.kind);
    assert!(matches!(&b.kind, ExprKind::Set(es) if es.len() == 2));
}

/// Optional trailing comma on call args and fn/closure params; `f(,)` still errors.
#[test]
fn call_args_and_params_trailing_comma() {
    // call args
    let StmtKind::Expr(a) = only("f(1, 2)\n") else {
        panic!()
    };
    let StmtKind::Expr(b) = only("f(1, 2,)\n") else {
        panic!()
    };
    assert_eq!(a.kind, b.kind);
    parse_err("f(,)\n");
    // fn params
    parse_ok("fn g(a, b,):\n    a\n");
    // closure params (closures live in expression position, e.g. a let RHS)
    let StmtKind::Let { value, .. } = only("c := fn(a, b,): a\n") else {
        panic!()
    };
    assert!(
        matches!(value.kind, ExprKind::Closure { .. }),
        "got {:?}",
        value.kind
    );
}

#[test]
fn tuple_return_type_parses() {
    let StmtKind::Fn(f) = only("fn pair() -> (int, int):\n    return (3, 4)\n") else {
        panic!()
    };
    assert_eq!(
        f.ret,
        Some(Type::Tuple(vec![Type::named("int"), Type::named("int")]))
    );
}

/// `(T)` in type position unwraps to `T` (not a 1-tuple).
#[test]
fn paren_single_type_unwraps() {
    let StmtKind::Fn(f) = only("fn f(x: (int)):\n    return x\n") else {
        panic!()
    };
    assert_eq!(f.params[0].ty, Some(Type::named("int")));
}

#[test]
fn destructuring_let_parses() {
    match only("a, b := pair()\n") {
        StmtKind::Let {
            target, ty, value, ..
        } => {
            assert_eq!(
                target.binder_names(),
                vec!["a".to_string(), "b".to_string()]
            );
            assert!(ty.is_none());
            assert!(matches!(value.kind, ExprKind::Call { .. }));
        }
        other => panic!("{other:?}"),
    }
}

/// `a, 1 := …` — every destructure target must be a bare identifier.
#[test]
fn destructuring_non_ident_rejected() {
    assert!(parse_err("a, 1 := pair()\n").message.contains("identifier"));
}

#[test]
fn tuple_element_access_parses() {
    let StmtKind::Expr(e) = only("t.0\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Field { name, .. } => assert_eq!(name, "0"),
        other => panic!("{other:?}"),
    }
    let StmtKind::Expr(e) = only("t.1\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Field { name, .. } => assert_eq!(name, "1"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn field_carries_name_span() {
    // `p.x` — the Field node's `name_span` must point at the field name `x` (line 1, col 3),
    // NOT the receiver `p`'s span; the LSP hover lookup keys on this true field-name position.
    let StmtKind::Expr(e) = only("p.x\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Field {
            name, name_span, ..
        } => {
            assert_eq!(name, "x");
            assert_eq!(name_span.line, 1);
            assert_eq!(name_span.col, 3);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn fn_decl_carries_name_span() {
    // `fn foo(...)` — the FnDecl's `name_span` points at `foo` (line 1, col 4), for the LSP
    // semantic-token overlay (mark the decl name `function`).
    let StmtKind::Fn(decl) = only("fn foo(x: int):\n    x\n") else {
        panic!()
    };
    assert_eq!(decl.name, "foo");
    assert_eq!(decl.name_span.line, 1);
    assert_eq!(decl.name_span.col, 4);
}

#[test]
fn param_carries_name_span() {
    // `fn f(abc: int)` — param `abc` starts at col 6 (`fn f(` is 5 chars).
    let StmtKind::Fn(decl) = only("fn f(abc: int):\n    abc\n") else {
        panic!()
    };
    assert_eq!(decl.params[0].name, "abc");
    assert_eq!(decl.params[0].name_span.line, 1);
    assert_eq!(decl.params[0].name_span.col, 6);
}

#[test]
fn struct_field_carries_name_span() {
    // `struct S:` then a 4-space-indented `fld: int` — the field name is at line 2, col 5.
    let StmtKind::Struct { fields, .. } = only("struct S:\n    fld: int\n") else {
        panic!()
    };
    assert_eq!(fields[0].name, "fld");
    assert_eq!(fields[0].name_span.line, 2);
    assert_eq!(fields[0].name_span.col, 5);
}

#[test]
fn type_named_carries_span() {
    // `x: int = 5` — the `Type::Named` annotation's span points at `int` (line 1, col 4).
    let StmtKind::Let { ty: Some(t), .. } = only("x: int = 5\n") else {
        panic!()
    };
    let Type::Named { name, span } = t else {
        panic!("expected Type::Named, got {t:?}")
    };
    assert_eq!(name, "int");
    assert_eq!(span.line, 1);
    assert_eq!(span.col, 4);
}

#[test]
fn pipe_binds_looser_than_arithmetic() {
    // `x := 1 + 2 |> f()` ⇒ `f(1 + 2)`, not `1 + f(2)`.
    let v = let_value("x := 1 + 2 |> f()\n");
    let ExprKind::Call { args, .. } = v.kind else {
        panic!("expected a Call, got {:?}", v.kind)
    };
    assert_eq!(args.len(), 1);
    assert!(matches!(
        args[0].kind,
        ExprKind::Binary {
            op: BinaryOp::Add,
            ..
        }
    ));
}

// ---- default args + named args (parser layer) ----

#[test]
fn param_default_parses() {
    match only("fn f(x: int, y: int = 10):\n    print(x)\n") {
        StmtKind::Fn(f) => {
            assert_eq!(f.params.len(), 2);
            assert!(f.params[0].default.is_none());
            assert!(matches!(
                f.params[1].default,
                Some(Expr {
                    kind: ExprKind::Int(10),
                    ..
                })
            ));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn named_call_args_parse() {
    let v = let_value("r := f(1, y=2, z=3)\n");
    let ExprKind::Call { args, named, .. } = v.kind else {
        panic!("expected a Call, got {:?}", v.kind)
    };
    assert_eq!(args.len(), 1);
    assert!(matches!(args[0].kind, ExprKind::Int(1)));
    assert_eq!(named.len(), 2);
    assert_eq!(named[0].0, "y");
    assert!(matches!(named[0].1.kind, ExprKind::Int(2)));
    assert_eq!(named[1].0, "z");
}

#[test]
fn eqeq_arg_is_not_named() {
    // `f(x == 1)` is a positional boolean expression, NOT a named arg.
    let v = let_value("r := f(x == 1)\n");
    let ExprKind::Call { args, named, .. } = v.kind else {
        panic!()
    };
    assert_eq!(args.len(), 1);
    assert!(named.is_empty());
    assert!(matches!(
        args[0].kind,
        ExprKind::Binary {
            op: BinaryOp::Eq,
            ..
        }
    ));
}

#[test]
fn positional_after_named_rejected() {
    assert!(
        parse_err("r := f(y=2, 1)\n")
            .message
            .contains("positional argument after named argument")
    );
}

#[test]
fn non_const_default_parses() {
    // The parser accepts any expression as a default; the desugar pass (not the parser) rejects
    // a default that references another parameter.
    parse_ok("fn f(x: int = g()):\n    print(x)\n");
    parse_ok("fn f(x: int = 1 + 2):\n    print(x)\n");
}

#[test]
fn const_collection_default_ok() {
    // literal collections of constants are allowed
    parse_ok("fn f(xs: List[int] = [1, 2]):\n    print(xs)\n");
    parse_ok("fn f(b: bool = false, s: str = \"hi\"):\n    print(b)\n");
}

#[test]
fn default_before_required_rejected() {
    assert!(
        parse_err("fn f(x: int = 1, y: int):\n    print(x)\n")
            .message
            .contains("required parameter")
    );
}

#[test]
fn closure_default_rejected() {
    assert!(
        parse_err("c := fn(x: int = 1): x\n")
            .message
            .contains("default")
    );
}

#[test]
fn struct_field_default_parses() {
    match only("struct S:\n    x: int\n    y: int = 0\n") {
        StmtKind::Struct { fields, .. } => {
            assert_eq!(fields.len(), 2);
            assert!(fields[0].default.is_none());
            assert!(matches!(
                fields[1].default,
                Some(Expr {
                    kind: ExprKind::Int(0),
                    ..
                })
            ));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn struct_field_default_before_required_rejected() {
    assert!(
        parse_err("struct S:\n    x: int = 0\n    y: int\n")
            .message
            .contains("required field")
    );
}

#[test]
fn method_default_param_allowed() {
    // Methods now accept constant-literal defaults (filled by the desugar pass at call sites).
    match only(
        "struct S:\n    x: int\n    fn scale(self, k: int = 2) -> int:\n        return self.x * k\n",
    ) {
        StmtKind::Struct { methods, .. } => {
            assert!(methods[0].params[1].default.is_some());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn method_required_after_default_rejected() {
    // The trailing-default rule still applies inside methods.
    assert!(
        parse_err(
            "struct S:\n    x: int\n    fn f(self, a: int = 1, b: int) -> int:\n        return a\n"
        )
        .message
        .contains("required parameter")
    );
}

// --- or-patterns + nested nullary -----------------------------------------------------------

/// The pattern of the first arm of `match x:` in a one-statement program.
fn first_arm_pattern(src: &str) -> Pattern {
    match only(src) {
        StmtKind::Match { mut arms, .. } => arms.remove(0).pattern,
        other => panic!("{other:?}"),
    }
}

#[test]
fn or_pattern_top_level() {
    // `1 | 2 | 3` -> Or of three int literals.
    match first_arm_pattern("match x:\n    1 | 2 | 3: print(1)\n    _: print(0)\n") {
        Pattern::Or(alts) => {
            assert_eq!(alts.len(), 3);
            assert!(matches!(alts[0], Pattern::Literal(LitPattern::Int(1))));
            assert!(matches!(alts[1], Pattern::Literal(LitPattern::Int(2))));
            assert!(matches!(alts[2], Pattern::Literal(LitPattern::Int(3))));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn neg_int_literal_and_range_patterns_parse() {
    // Negative int literal pattern.
    assert_eq!(
        first_arm_pattern("match x:\n    -3: print(1)\n    _: print(0)\n"),
        Pattern::Literal(LitPattern::Int(-3))
    );
    // Negative-bounded range (both bounds negative).
    assert_eq!(
        first_arm_pattern("match x:\n    -10..-5: print(1)\n    _: print(0)\n"),
        Pattern::Range {
            start: -10,
            end: -5,
            inclusive: false
        }
    );
    // Mixed range bounds: negative start, positive end.
    assert_eq!(
        first_arm_pattern("match x:\n    -10..5: print(1)\n    _: print(0)\n"),
        Pattern::Range {
            start: -10,
            end: 5,
            inclusive: false
        }
    );
    // Positive start, negative end (still parses; semantics: empty range).
    assert_eq!(
        first_arm_pattern("match x:\n    0..-5: print(1)\n    _: print(0)\n"),
        Pattern::Range {
            start: 0,
            end: -5,
            inclusive: false
        }
    );
    // Negatives compose with or-patterns.
    match first_arm_pattern("match x:\n    -1 | -2: print(1)\n    _: print(0)\n") {
        Pattern::Or(alts) => {
            assert_eq!(alts.len(), 2);
            assert_eq!(alts[0], Pattern::Literal(LitPattern::Int(-1)));
            assert_eq!(alts[1], Pattern::Literal(LitPattern::Int(-2)));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn float_patterns_parse_and_a_float_range_is_rejected() {
    // A float literal pattern, plain or negative (owner decision 2026-11-03).
    for (src, want) in [
        ("1.5", 1.5),
        ("-3.0", -3.0),
        ("-0.0", -0.0),
        ("2e3", 2000.0),
    ] {
        match first_arm_pattern(&format!("match x:\n    {src}: print(1)\n    _: print(0)\n")) {
            Pattern::Literal(LitPattern::Float(f)) => {
                assert_eq!(f.to_bits(), f64::to_bits(want), "{src}");
            }
            other => panic!("{src}: {other:?}"),
        }
    }
    // A range pattern stays int-only. A float lower bound is named as written, sign included.
    for (src, want) in [
        ("1.5..2.5", "found float 1.5"),
        ("1.0..2.0", "found float 1.0"),
        ("-1.0..=2", "found float -1.0"),
        ("-1.5..=2", "found float -1.5"),
        ("1..2.5", "float"),
    ] {
        let e = parse_err(&format!("match x:\n    {src}: print(1)\n    _: print(0)\n"));
        assert!(
            e.to_string().contains(want),
            "{src}: expected {want:?}, got: {e}"
        );
    }
}

#[test]
fn or_pattern_variants() {
    // `Red | Green` -> Or of two nullary variants (top position).
    match first_arm_pattern("match c:\n    Red | Green: print(1)\n    _: print(0)\n") {
        Pattern::Or(alts) => {
            assert_eq!(alts.len(), 2);
            assert!(
                matches!(&alts[0], Pattern::Variant { name, bindings, .. } if name == "Red" && bindings.is_empty())
            );
            assert!(
                matches!(&alts[1], Pattern::Variant { name, bindings, .. } if name == "Green" && bindings.is_empty())
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn or_pattern_in_tuple() {
    // `(1 | 2, x)` -> Tuple([Or([1,2]), Ident(x)]).
    match first_arm_pattern("match p:\n    (1 | 2, x): print(x)\n    _: print(0)\n") {
        Pattern::Tuple(elems) => {
            assert_eq!(elems.len(), 2);
            assert!(matches!(&elems[0], Pattern::Or(a) if a.len() == 2));
            assert!(matches!(&elems[1], Pattern::Ident(n, _, _) if n == "x"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn qualified_variant_pattern_parses() {
    // `Color.Red` (nullary) -> Variant{name: "Red", enum_name: Some("Color")}.
    match first_arm_pattern("match c:\n    Color.Red: print(0)\n    _: print(1)\n") {
        Pattern::Variant {
            name,
            bindings,
            enum_name,
            ..
        } => {
            assert_eq!(name, "Red");
            assert!(bindings.is_empty());
            assert_eq!(enum_name.as_deref(), Some("Color"));
        }
        other => panic!("{other:?}"),
    }
    // `Shape.Circle(r)` (payload) -> Variant{name: "Circle", enum_name: Some("Shape"), [Ident r]}.
    match first_arm_pattern("match s:\n    Shape.Circle(r): print(r)\n    _: print(0)\n") {
        Pattern::Variant {
            name,
            bindings,
            enum_name,
            ..
        } => {
            assert_eq!(name, "Circle");
            assert_eq!(enum_name.as_deref(), Some("Shape"));
            assert!(matches!(&bindings[0], Pattern::Ident(n, _, _) if n == "r"));
        }
        other => panic!("{other:?}"),
    }
    // Bare `None` stays unqualified.
    match first_arm_pattern("match o:\n    None: print(0)\n    _: print(1)\n") {
        Pattern::Variant {
            name, enum_name, ..
        } => {
            assert_eq!(name, "None");
            assert_eq!(enum_name, None);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn four_segment_variant_pattern_keeps_a_dotted_module() {
    // `a.b.Color.Blue(n)` -> module `a.b`, enum `Color`, variant `Blue` (TICKET-175).
    match first_arm_pattern("match c:\n    a.b.Color.Blue(n): print(n)\n    _: print(1)\n") {
        Pattern::Variant {
            name,
            enum_name,
            module_name,
            ..
        } => {
            assert_eq!(name, "Blue");
            assert_eq!(enum_name.as_deref(), Some("Color"));
            assert_eq!(module_name.as_deref(), Some("a.b"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn module_qualified_variant_pattern_parses() {
    // `geo.Color.Red` (nullary, module-qualified) ->
    // Variant{name:"Red", enum_name:Some("Color"), module_name:Some("geo")}.
    match first_arm_pattern("match c:\n    geo.Color.Red: print(0)\n    _: print(1)\n") {
        Pattern::Variant {
            id: _,
            name,
            bindings,
            enum_name,
            module_name,
        } => {
            assert_eq!(name, "Red");
            assert!(bindings.is_empty());
            assert_eq!(enum_name.as_deref(), Some("Color"));
            assert_eq!(module_name.as_deref(), Some("geo"));
        }
        other => panic!("{other:?}"),
    }
    // Aliased binder `g.Color.Red`.
    match first_arm_pattern("match c:\n    g.Color.Red: print(0)\n    _: print(1)\n") {
        Pattern::Variant {
            name,
            enum_name,
            module_name,
            ..
        } => {
            assert_eq!(name, "Red");
            assert_eq!(enum_name.as_deref(), Some("Color"));
            assert_eq!(module_name.as_deref(), Some("g"));
        }
        other => panic!("{other:?}"),
    }
    // Payload binding `geo.Shape.Circle(r)`.
    match first_arm_pattern("match s:\n    geo.Shape.Circle(r): print(r)\n    _: print(0)\n") {
        Pattern::Variant {
            id: _,
            name,
            bindings,
            enum_name,
            module_name,
        } => {
            assert_eq!(name, "Circle");
            assert_eq!(enum_name.as_deref(), Some("Shape"));
            assert_eq!(module_name.as_deref(), Some("geo"));
            assert!(matches!(&bindings[0], Pattern::Ident(n, _, _) if n == "r"));
        }
        other => panic!("{other:?}"),
    }
    // Bare `Color.Red` keeps module_name:None.
    match first_arm_pattern("match c:\n    Color.Red: print(0)\n    _: print(1)\n") {
        Pattern::Variant {
            module_name,
            enum_name,
            ..
        } => {
            assert_eq!(module_name, None);
            assert_eq!(enum_name.as_deref(), Some("Color"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn or_pattern_in_payload() {
    // `?(a | b)` -> Carrier{Present, Or([Ident a, Ident b])}.
    match first_arm_pattern("match o:\n    ?(a | b): print(a)\n    _: print(0)\n") {
        Pattern::Carrier { tag, inner, .. } => {
            assert_eq!(tag, CarrierTag::Present);
            match &*inner {
                Pattern::Or(alts) => {
                    assert_eq!(alts.len(), 2);
                    assert!(matches!(&alts[0], Pattern::Ident(n, _, _) if n == "a"));
                    assert!(matches!(&alts[1], Pattern::Ident(n, _, _) if n == "b"));
                }
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn single_primary_unchanged() {
    // A single primary still parses to that primary unchanged (zero regression — no Or wrapper).
    match first_arm_pattern("match x:\n    1: print(1)\n    _: print(0)\n") {
        Pattern::Literal(LitPattern::Int(1)) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn nested_nullary_parses() {
    // `?None` -> Carrier{Present, Variant("None")}: `None` is a keyword, never a binding.
    match first_arm_pattern("match o:\n    ?None: print(0)\n    _: print(1)\n") {
        Pattern::Carrier { tag, inner, .. } => {
            assert_eq!(tag, CarrierTag::Present);
            assert!(matches!(
                &*inner,
                Pattern::Variant { name, bindings, .. } if name == "None" && bindings.is_empty()
            ));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn nested_nullary_in_result_parses() {
    // `?(!e)` -> Carrier{Present, Carrier{Error, Ident e}}.
    match first_arm_pattern("match r:\n    ?(!e): print(e)\n    _: print(0)\n") {
        Pattern::Carrier { tag, inner, .. } => {
            assert_eq!(tag, CarrierTag::Present);
            match &*inner {
                Pattern::Carrier { tag, inner, .. } => {
                    assert_eq!(*tag, CarrierTag::Error);
                    assert!(matches!(&**inner, Pattern::Ident(n, _, _) if n == "e"));
                }
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
}

// A three-level type path `std.concurrency.Shared[int]` is ONE qualified type whose module is the
// dotted `std.concurrency` (TICKET-175).
#[test]
fn multilevel_type_path_parses_as_one_qualified_module() {
    let StmtKind::Let { ty, .. } = only("x: std.concurrency.Shared[int] = 0\n") else {
        panic!("expected a let");
    };
    match ty {
        Some(Type::Qualified { module, name, args }) => {
            assert_eq!(module, "std.concurrency");
            assert_eq!(name, "Shared");
            assert_eq!(args.len(), 1);
        }
        other => panic!("{other:?}"),
    }
}

// A correct two-level qualified type still parses fine (no false positive).
#[test]
fn two_level_type_path_parses() {
    // `c.Shared[int]` is a valid qualified type annotation.
    parse_ok("fn f(x: c.Shared[int]): print(1)\n");
}

// ===== TICKET-273: the 1-tuple has a type form and a pattern form =====

#[test]
fn one_tuple_type_parses() {
    let StmtKind::Fn(f) = only("fn one() -> (int,):\n    return (1,)\n") else {
        panic!()
    };
    assert_eq!(f.ret, Some(Type::Tuple(vec![Type::named("int")])));
}

#[test]
fn one_tuple_unpack_target_parses() {
    let m = parse_ok("t := (1,)\na, := t\n(b,), c := (t, 2)\n");
    assert_eq!(m.stmts.len(), 3);
}

#[test]
fn one_tuple_match_pattern_parses() {
    match first_arm_pattern("match t:\n    (a,): print(a)\n    _: print(0)\n") {
        Pattern::Tuple(elems) => assert_eq!(elems.len(), 1),
        other => panic!("{other:?}"),
    }
}

/// `for a, in xs` and `[a for a, in xs]` bind the element of a 1-tuple (S2-7).
#[test]
fn one_tuple_loop_and_comprehension_targets_parse() {
    match only("for a, in xs:\n    print(a)\n") {
        StmtKind::For {
            target: Pattern::Tuple(elems),
            ..
        } => assert_eq!(elems.len(), 1),
        other => panic!("{other:?}"),
    }
    match let_value("ys := [a for a, in xs]\n").kind {
        ExprKind::Comprehension { clauses, .. } => {
            assert!(matches!(&clauses[0].target, Pattern::Tuple(e) if e.len() == 1));
        }
        other => panic!("{other:?}"),
    }
}

/// `a, = t` assigns, and a wait arm takes `a, :=` and `a, =` (S2-7).
#[test]
fn one_tuple_assignment_and_wait_arm_targets_parse() {
    match only("a, = t\n") {
        StmtKind::Assign { target, .. } => {
            assert!(matches!(target.kind, ExprKind::Tuple(ref e) if e.len() == 1));
        }
        other => panic!("{other:?}"),
    }
    parse_ok("wait:\n    a, := ch.recv():\n        print(a)\n");
    parse_ok("wait:\n    a, = ch.recv():\n        print(a)\n");
}

/// `(int,)` is a 1-tuple type in every type position; `(int)` stays `int` (`paren_single_type_unwraps`).
#[test]
fn one_tuple_type_in_every_type_position() {
    match only("x: (int,) = (1,)\n") {
        StmtKind::Let { ty, .. } => assert_eq!(ty, Some(Type::Tuple(vec![Type::named("int")]))),
        other => panic!("{other:?}"),
    }
    parse_ok(
        "l: List[(int,)] = []\no: (int,)? = None\nstruct S:\n    p: (int,)\nfn f(t: (str,)) -> (int, str,):\n    return 1, t.0\n",
    );
}

/// A 1-tuple target takes no outer parentheses, as `(a, b) := t` takes none (U9); a value list
/// takes no trailing comma (`x := 1,` is an error, not a 1-tuple).
#[test]
fn one_tuple_outer_parens_and_bare_trailing_comma_rejected() {
    assert!(parse_err("(a,) := t\n").message.contains("must be a name"));
    assert!(
        parse_err("for (a,) in xs:\n    print(a)\n")
            .message
            .contains("outer parentheses")
    );
    assert!(
        parse_err("x := 1,\n")
            .message
            .contains("unexpected end of line")
    );
}
