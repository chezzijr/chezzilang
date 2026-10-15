//! Parser tests: const bindings, enum variant newlines (TICKET-122), one-arg call brackets, else
//! guards, block-value defaults, interpolation holes.
//! Split out of `src/parser/mod.rs`. `super::*` is the parent module.

use super::*;

// ===== const binding modifier =====

#[test]
fn const_typed_let_parses() {
    parse_ok("PI: const float = 3.14\n");
}

#[test]
fn const_on_param_rejected() {
    assert!(
        parse_err("fn f(x: const int): print(1)\n")
            .message
            .contains("not valid on parameters")
    );
}

#[test]
fn const_without_type_rejected() {
    // `const` sits in the type slot, so a type must follow it (no inferred-type const in v1).
    assert!(parse(lexer::tokenize("x: const = 5\n").unwrap()).is_err());
}

// ===== TICKET-122: enum body must require a newline after every variant =====

#[test]
fn enum_two_variants_on_one_line_is_a_parse_error() {
    let e = parse_err("enum E:\n    A B\n");
    assert!(e.message.contains("expected newline"), "got: {}", e.message);
}

#[test]
fn enum_variant_then_method_on_one_line_is_a_parse_error() {
    let e = parse_err("enum E:\n    A(int) fn f(self) -> int:\n        return 1\n");
    assert!(
        e.message
            .contains("expected newline after enum variant 'A', found 'fn'"),
        "got: {}",
        e.message
    );
}

#[test]
fn enum_static_fn_does_not_declare_a_phantom_variant() {
    let e = parse_err("enum Lv:\n    Lo\n    static fn zero() -> Lv:\n        return Lv.Lo\n");
    assert!(
        e.message
            .contains("expected newline after enum variant 'static', found 'fn'"),
        "got: {}",
        e.message
    );
}

#[test]
fn enum_variant_before_a_packed_method_beats_the_ordering_error() {
    let e = parse_err("enum E:\n    A fn f(self) -> int:\n        return 1\n    B\n");
    assert!(
        e.message
            .contains("expected newline after enum variant 'A', found 'fn'"),
        "got: {}",
        e.message
    );
}

#[test]
fn struct_two_fields_on_one_line_is_a_parse_error() {
    let e = parse_err("struct S:\n    x: int y: int\n");
    assert!(
        e.message
            .contains("expected newline after struct field 'x', found identifier 'y'"),
        "got: {}",
        e.message
    );
}

#[test]
fn struct_two_defaulted_fields_on_one_line_is_a_parse_error() {
    let e = parse_err("struct S:\n    x: int = 1 y: int = 2\n");
    assert!(
        e.message
            .contains("expected newline after struct field 'x', found identifier 'y'"),
        "got: {}",
        e.message
    );
}

#[test]
fn native_enum_two_variants_on_one_line_is_a_parse_error() {
    let e = parse_err("native enum E:\n    A B\n");
    assert!(
        e.message
            .contains("expected newline after enum variant 'A', found identifier 'B'"),
        "got: {}",
        e.message
    );
}

#[test]
fn native_enum_method_packed_onto_a_variant_line_is_a_parse_error() {
    let e = parse_err("native enum E:\n    A(int) native fn m(self) -> int\n");
    assert!(
        e.message
            .contains("expected newline after enum variant 'A', found 'native'"),
        "got: {}",
        e.message
    );
}

#[test]
fn native_struct_two_fields_on_one_line_is_a_parse_error() {
    let e = parse_err("native struct S:\n    x: int y: int\n");
    assert!(
        e.message
            .contains("expected newline after native struct field 'x', found identifier 'y'"),
        "got: {}",
        e.message
    );
}

#[test]
fn a_block_valued_field_default_still_ends_its_line() {
    let kind =
        only("struct S:\n    x: int = match 1:\n        1: 10\n        _: 20\n    y: int = 3\n");
    let StmtKind::Struct { fields, .. } = kind else {
        panic!("expected a struct");
    };
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].name, "x");
    assert_eq!(fields[1].name, "y");
}

#[test]
fn a_trailing_comment_after_a_variant_still_ends_its_line() {
    let kind = only("enum E:\n    A(int)   # payload\n    B        # unit\n");
    let StmtKind::Enum { variants, .. } = kind else {
        panic!("expected an enum");
    };
    assert_eq!(variants.len(), 2);
}

#[test]
fn native_struct_two_methods_on_one_line_is_a_parse_error() {
    let e = parse_err("native struct S:\n    native fn a(self) -> int native fn b(self) -> int\n");
    assert!(
        e.message
            .contains("expected newline after native method 'a', found 'native'"),
        "got: {}",
        e.message
    );
}

#[test]
fn native_enum_two_methods_on_one_line_is_a_parse_error() {
    let e =
        parse_err("native enum E:\n    A\n    native fn m(self) -> int native fn n(self) -> int\n");
    assert!(
        e.message
            .contains("expected newline after native method 'm', found 'native'"),
        "got: {}",
        e.message
    );
}

#[test]
fn protocol_two_method_sigs_on_one_line_is_a_parse_error() {
    let e = parse_err("protocol P:\n    fn a(self) -> int fn b(self) -> int\n");
    assert!(
        e.message.contains("expected end of line, found 'fn'"),
        "got: {}",
        e.message
    );
}

#[test]
fn extern_two_fn_sigs_on_one_line_is_a_parse_error() {
    let e = parse_err("extern \"libc\":\n    fn getpid() -> int fn getppid() -> int\n");
    assert!(
        e.message.contains("expected end of line, found 'fn'"),
        "got: {}",
        e.message
    );
}

/// TICKET-210: a one-arg `head[X](args)` on a bare-name or member head keeps the bracket's
/// expression reading beside its type reading; every other shape keeps `None`.
#[test]
fn a_one_arg_call_bracket_keeps_its_expression_reading() {
    let bracket = |src: &str| match let_value(src).kind {
        ExprKind::Call {
            type_args, bracket, ..
        } => (type_args.len(), bracket.map(|b| b.kind)),
        other => panic!("expected a call, got {other:?}"),
    };
    assert!(matches!(bracket("x := fs[k](1)\n"), (1, Some(ExprKind::Ident(k))) if k == "k"));
    assert!(matches!(
        bracket("x := h.fs[k](1)\n"),
        (1, Some(ExprKind::Ident(_)))
    ));
    assert!(matches!(bracket("x := f[int, str](1)\n"), (2, None)));
    assert!(matches!(bracket("x := f[fn(int) -> int](1)\n"), (1, None)));
    assert!(matches!(bracket("x := t.0[k](1)\n"), (_, None)));
}

/// TICKET-241 -- the inline body after `:` is exactly `<simpleStmt>`: every simple statement
/// (a closure literal and `spawn <call>` included) is legal, every compound start is refused.
#[test]
fn inline_body_is_exactly_a_simple_stmt() {
    const NESTED: &str = "a nested block must be indented, not written inline after ':'";
    let mut bad = Vec::new();
    for src in [
        "fn a(): return\n",
        "fn b(): pass\n",
        "fn c(n: int): assert n > 0\n",
        "fn d(): x = 1\n",
        "fn e(): x := 1\n",
        "fn g(): yield 1\n",
        "fn h(): f()\n",
        "if true: spawn w()\n",
        "parallel: spawn w()\n",
        "for i in xs: spawn w(i)\n",
        "for i in xs: continue\n",
        "while true: break\n",
        "fn m(): fn(x: int) -> int: x\n",
        "if true: fn(x: int) -> int: x\n",
        "fn(x: int) -> int: x\n",
        "match 1:\n    1: fn(x: int) -> int: x\n    2: spawn w()\n    _: pass\n",
    ] {
        if let Err(e) = parse(crate::lexer::tokenize(src).unwrap()) {
            bad.push(format!("{src:?} must parse: {}", e.message));
        }
    }
    for src in [
        "fn outer(): fn inner(): pass\n",
        "fn o(): parallel: spawn w()\n",
        "fn outer(): defer print(1)\n",
        "if true: spawn:\n    w()\n",
        "if true: defer print(1)\n",
        "fn g(ch: Channel[int]): wait:\n    x := ch.recv(): print(x)\n",
        "parallel: spawn:\n    print(1)\n",
        "if a: for i in xs: pass\n",
        "if a: while b: pass\n",
    ] {
        match parse(crate::lexer::tokenize(src).unwrap()) {
            Ok(_) => bad.push(format!("{src:?} must be rejected, parsed")),
            Err(e) if e.message != NESTED => bad.push(format!(
                "{src:?} must give the nested-block text: {}",
                e.message
            )),
            Err(_) => {}
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    // `if` and `match` after an inline `:` are expression statements (U8): the one-line `if`
    // needs its `else`, and a multi-line `match` is no whole value there
    assert!(
        parse_err("if a: if b: x = 1\n")
            .message
            .contains("expected 'else'")
    );
    assert_eq!(
        parse_err("if a: match b:\n    _: pass\n").message,
        BLOCK_VALUE
    );
    assert_eq!(
        parse_err("fn g(): struct A:\n    x: int\n").message,
        "struct must be a top-level declaration"
    );
    let spawn = parse_err("parallel:\n    spawn 1\n").message;
    assert!(
        spawn.starts_with("spawn requires a function or method call"),
        "{spawn}"
    );
}

#[test]
fn expr_body_fn_may_be_a_closure_literal() {
    parse_ok(
        "fn adder(n: int) -> fn(int) -> int: fn(x: int) -> int: x + n
print(adder(1)(2))
",
    );
}

/// TICKET-241 -- the lexer fuses `!=`; after a type it is the type's `!` and then `=`. The `!`
/// binds where the spaced form binds it, and `!=` in an expression is unchanged.
#[test]
fn fused_bang_eq_is_bang_then_assign() {
    fn is_result(ty: &Type) -> bool {
        matches!(ty, Type::Generic(n, args, _) if n == "Result" && args.len() == 1)
    }
    match only("y: int!= 6\n") {
        StmtKind::Let { ty: Some(ty), .. } => assert!(is_result(&ty), "{ty:?}"),
        other => panic!("expected a typed let, got {other:?}"),
    }
    match only("g: fn() -> int!= one\n") {
        StmtKind::Let {
            ty: Some(Type::Func { ret, .. }),
            ..
        } => assert!(is_result(&ret), "{ret:?}"),
        other => panic!("expected a let of fn type, got {other:?}"),
    }
    let mut bad = Vec::new();
    for src in [
        "y: const int!= 6\n",
        "fn f(a: int!=5): a\n",
        "struct P:\n    a: int!= 5\n",
        "x: bool!= a != b\n",
        "x: bool! = a != b\n",
        "print(xs[a != b])\n",
        "print(f[int](1) != 2)\n",
    ] {
        if let Err(e) = parse(crate::lexer::tokenize(src).unwrap()) {
            bad.push(format!("{src:?} must parse: {}", e.message));
        }
    }
    for (src, want) in [
        ("y: int!== 6\n", "unexpected '=' in expression"),
        ("fn f() -> int!= 5\n", "expected ':', found '!='"),
        ("type X = int!= 5\n", "expected end of line, found '!='"),
    ] {
        let got = parse_err(src).message;
        if !got.contains(want) {
            bad.push(format!("{src:?} must give {want:?}: {got}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn type_postfix_accepts_fused_bang_eq() {
    parse_ok(
        "y: int!= 6
",
    );
}

/// TICKET-241 -- the `else` guard is the value of a let (one name or a destructuring) or of a
/// bare call. Assignment, compound assignment and `return` refuse it with one text.
#[test]
fn else_guard_binds_only_on_a_let_or_a_bare_call() {
    let StmtKind::Fn(g) = only("fn g():\n    a, b := f() else e:\n        return\n") else {
        panic!("expected a fn");
    };
    match &g.body[0].kind {
        StmtKind::Let { target, value, .. } => {
            assert_eq!(target.binder_names().len(), 2);
            assert!(matches!(value.kind, ExprKind::ElseGuard { .. }));
        }
        other => panic!("expected a let, got {other:?}"),
    }
    let mut bad = Vec::new();
    let mut expect = |stmt: &str, want: &str| {
        let src = format!("fn g():\n    {stmt}\n        return\n");
        match parse(lexer::tokenize(&src).unwrap()) {
            Ok(_) => bad.push(format!("{stmt:?} must be rejected, parsed")),
            Err(e) if e.message != want => {
                bad.push(format!("{stmt:?} must give {want:?}: {}", e.message))
            }
            Err(_) => {}
        }
    };
    for stmt in [
        "x = f() else e:",
        "x += f() else e:",
        "a, b = f() else e:",
        "self.f = f() else e:",
        "xs[0] = f() else e:",
        "return f() else e:",
    ] {
        expect(stmt, ELSE_GUARD_PLACE);
    }
    expect("a, b := 1, 2 else:", "expected end of line, found 'else'");
    expect("(a, b) := f() else e:", "left side of ':=' must be a name");
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    parse_ok("fn g(n: int) -> int:\n    return if n > 0: 1 else: 2\n");
}

#[test]
fn else_guard_on_destructuring_binding() {
    parse_ok(
        "fn f() -> (int, int)!str:
    return (1, 2)
fn g():
    a, b := f() else e:
        return
",
    );
}

/// TICKET-242 -- the one-line `if … else` statement is the if-expression in an expression
/// statement. After the one statement of an inline `if` body a bare `else:` or `elif` on the
/// line belongs to the `if`; a nested block on the line, and a later line, keep the `else`
/// reading they have everywhere else.
#[test]
fn one_line_if_statement_is_the_if_expression() {
    /// The value of a let or of an expression statement.
    fn value_of(s: &Stmt) -> &Expr {
        match &s.kind {
            StmtKind::Let { value, .. } | StmtKind::Expr(value) => value,
            other => panic!("expected a let or an expression statement, got {other:?}"),
        }
    }
    /// `e` is a guard (`<call> else …`) held by a let.
    fn guard_on_call(s: &Stmt) -> bool {
        matches!(&s.kind, StmtKind::Let { value, .. }
                if matches!(&value.kind, ExprKind::ElseGuard { value, .. }
                    if matches!(value.kind, ExprKind::Call { .. })))
    }
    /// `e` is a `recover:` whose first statement is a guarded call.
    fn guarded_recover(e: &Expr) -> bool {
        matches!(&e.kind, ExprKind::Recover(block, _) if guard_on_call(&block[0]))
    }
    /// The branches and else block of a statement `if`.
    fn stmt_if(kind: StmtKind) -> (Vec<(Expr, Block)>, Option<Block>) {
        match kind {
            StmtKind::If {
                branches,
                else_block,
            } => (branches, else_block),
            other => panic!("expected a statement `if`, got {other:?}"),
        }
    }
    /// The parts of the if-expression an expression statement holds.
    fn if_expr(kind: StmtKind) -> (Expr, Expr, Expr) {
        match kind {
            StmtKind::Expr(Expr {
                kind: ExprKind::IfElse { cond, then, els },
                ..
            }) => (*cond, *then, *els),
            other => panic!("expected an if-expression statement, got {other:?}"),
        }
    }

    // the statement form and the expression form are one node
    let (cond, then, els) = if_expr(only("if c: print(1) else: print(2)\n"));
    assert!(matches!(cond.kind, ExprKind::Ident(_)));
    assert!(matches!(then.kind, ExprKind::Call { .. }));
    assert!(matches!(els.kind, ExprKind::Call { .. }));
    let (_, _, els) = if_expr(only("if a: print(1) elif b: print(2) else: print(3)\n"));
    assert!(matches!(els.kind, ExprKind::IfElse { .. }));
    let (cond, then, els) = if_expr(only("if c: 1 else: 2\n"));
    let StmtKind::Let { value, .. } = only("x := if c: 1 else: 2\n") else {
        panic!("expected a let");
    };
    let ExprKind::IfElse {
        cond: lc,
        then: lt,
        els: le,
    } = value.kind
    else {
        panic!("expected an if-expression");
    };
    for (stmt, expr) in [(&cond, &*lc), (&then, &*lt), (&els, &*le)] {
        match (&stmt.kind, &expr.kind) {
            (ExprKind::Ident(a), ExprKind::Ident(b)) => assert_eq!(a, b),
            (ExprKind::Int(a), ExprKind::Int(b)) => assert_eq!(a, b),
            other => panic!("the two forms differ: {other:?}"),
        }
    }
    if_expr(only("if c: print((if c: 1 else: 2)) else: print(3)\n"));

    // a later line is not the `if` line
    let (branches, else_block) = stmt_if(only("if c: print(1)\nelse: print(2)\n"));
    assert_eq!(branches.len(), 1);
    assert!(else_block.is_some());
    let (branches, else_block) = stmt_if(only("if c: return\nelse: print(2)\n"));
    assert!(matches!(branches[0].1[0].kind, StmtKind::Return(None)));
    assert!(else_block.is_some());
    let (branches, _) = stmt_if(only(
        "if c: x := match n:\n    1: 10\n    _: 20\nelif d:\n    print(2)\n",
    ));
    assert_eq!(branches.len(), 2);
    let (branches, else_block) = stmt_if(only(
        "if c: x := match n:\n    1: 10\n    _: 20\nelse:\n    return\n",
    ));
    assert_eq!(branches.len(), 1);
    assert!(else_block.is_some());
    assert!(matches!(
        value_of(&branches[0].1[0]).kind,
        ExprKind::Match { .. }
    ));

    // `else e:` stays a guard on the body's statement
    let (branches, else_block) = stmt_if(only("if c: g() else e: return\n"));
    assert!(else_block.is_none());
    assert!(guard_on_call(&branches[0].1[0]));

    // the inline `recover:` body is ONE expression, so a same-line `else:` belongs to the inline
    // `if` (`if_line_else`), and a statement is no branch of it
    let rejects = |src: &str, want: &str| match parse(crate::lexer::tokenize(src).unwrap()) {
        Ok(_) => panic!("{src:?} must be rejected"),
        Err(e) => assert!(e.message.contains(want), "{src:?}: {}", e.message),
    };
    let one_line = "takes an expression in each branch";
    rejects("if true: r := recover: g() else: return\n", one_line);
    rejects("if true: recover: g() else: return\n", one_line);
    rejects("if true: _ := recover: g() else: panic(\"p\")\n", one_line);
    rejects(
        "if false: print(1)\nelif true: r := recover: g() else: panic(\"p\")\n",
        "an `elif` body ends its line",
    );
    rejects(
        "if true: v := match n:\n    1: recover: g() else: panic(\"p\")\n    _: recover: 1\n",
        "expected end of line, found 'else'",
    );
    // in an indented body the `else:` is a guard on the `:=` value
    let (branches, _) = stmt_if(only("if true:\n    r := recover: g() else: panic(\"p\")\n"));
    let ExprKind::ElseGuard { value, .. } = &value_of(&branches[0].1[0]).kind else {
        panic!("expected a guard");
    };
    assert!(matches!(value.kind, ExprKind::Recover(..)));
    // a block-form `recover:` keeps a guard on its own indented line
    let (branches, _) = stmt_if(only(
        "if true: r := recover:\n    g() else: panic(\"p\")\nelif true:\n    print(1)\n",
    ));
    assert!(guarded_recover(value_of(&branches[0].1[0])));
    let (branches, _) = stmt_if(only("if true: k() else e: g() else: return\n"));
    let ExprKind::ElseGuard { body, .. } = &value_of(&branches[0].1[0]).kind else {
        panic!("expected a guard");
    };
    assert!(guard_on_call(&body[0]));
    let (_, then, _) = if_expr(only("if c: recover: 1 else: 2\n"));
    assert!(matches!(then.kind, ExprKind::Recover(..)));

    // `recover: <expr>` as a whole parenthesized value or a whole call argument
    let StmtKind::Expr(call) = only("print(recover: 1)\n") else {
        panic!("expected a call statement");
    };
    let ExprKind::Call { args, .. } = &call.kind else {
        panic!("expected a call");
    };
    let one_expr = |e: &Expr| {
        matches!(&e.kind, ExprKind::Recover(block, _)
                if block.len() == 1 && matches!(block[0].kind, StmtKind::Expr(_)))
    };
    assert!(one_expr(&args[0]));
    let StmtKind::Let { value: paren, .. } = only("x := (recover: 1)\n") else {
        panic!("expected a let");
    };
    assert!(one_expr(&paren));

    let mut bad = Vec::new();
    let mut expect =
        |src: &str, want: &str, exact: bool| match parse(crate::lexer::tokenize(src).unwrap()) {
            Ok(_) => bad.push(format!("{src:?} must be rejected, parsed")),
            Err(e) if (exact && e.message != want) || !e.message.contains(want) => {
                bad.push(format!("{src:?} must give {want:?}: {}", e.message))
            }
            Err(_) => {}
        };
    for src in [
        "if c: print(1) else: x = 2\n",
        "if c: print(1) else: x := 2\n",
        "if c: print(1) else: return 2\n",
        "if i == 0: print(1) else: break\n",
        "if c: print(1) else: print(2) print(3)\n",
        "if c: print(1) else: print(2) else e: return\n",
        "if c: return 1 else: return 2\n",
        "if c: x = 1 else: x = 2\n",
        "if i == 0: break else: continue\n",
        "if c: print(1) else: return\n",
        "if true: g() else: return\n",
        "if true: v := g() else: return\n",
        "x := if c: return 1 else: return 2\n",
        "if true: f := fn(): g() else: return\n",
        "if c: print(1) elif d: print(2) else: print(3) else: return\n",
        "fn f(c: bool):\n    if c: return else: print(2)\n",
        "fn f(c: bool):\n    if c: return else: return\n",
    ] {
        expect(src, IF_BRANCH, true);
    }
    for src in [
        "if a:\n    print(0)\nelif b: print(1) else: print(2)\n",
        "if a: print(0)\nelif b: print(1) else: print(2)\n",
        "if a: print(0)\nelif b: print(1) elif true: print(2)\n",
        "if false: print(0)\nelif true: g() else: return\n",
        "fn f(c: bool):\n    if c: print(1)\n    elif c: return else: return\n",
    ] {
        expect(src, ELIF_LINE, true);
    }
    for src in [
        "fn f[T]() where T: return\n",
        "fn f[T]() where T:\n    pass\n",
        "fn f[T, U]() where T, U: Eq:\n    pass\n",
    ] {
        expect(src, WHERE_BOUND, true);
    }
    expect(
        "fn f[T](x: T) where T: None:\n    pass\n",
        "reserved keyword 'None'",
        false,
    );
    for src in [
        "x := 1[T, U]\n",
        "x := 1[T!]\n",
        "x := 1[T??]\n",
        "x := 1[fn() -> T]\n",
        "x := 1[!]\n",
        "x := (1, 2)[int, str]\n",
    ] {
        expect(src, TYPE_LIST_HEAD, true);
    }
    // the inline `recover:` is an ordinary expression: it parses in every expression position
    for src in [
        "print(1 + recover: 1)\n",
        "x := (?1 == recover: 1)\n",
        "print(not recover: true)\n",
        "f(fn(): recover: 1 / 0)\n",
        "x := (if c: recover: 1 else: 2)\n",
        "if c: print(if d: recover: 1 else: 2)\n",
        "x := (recover: 1, 2)\n",
        "if c: y := if d: recover: 1 else: 2\n",
        "x := if c: recover: 1 else: 2\n",
    ] {
        parse_ok(src);
    }
    expect(
        "if c: x := (if d: recover: 1 else: 2) else: print(3)\n",
        "takes an expression in each branch",
        false,
    );
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// A multi-line `match` is a parameter default: the parser asks the lexer for layout inside the
/// parameter list's parentheses (TICKET-260, rule U8).
#[test]
fn block_value_param_default_spans_lines() {
    let StmtKind::Fn(decl) =
        only("fn f(x: int = match 1:\n    1: 5\n    _: 6) -> int:\n    return x\n")
    else {
        panic!("expected a fn");
    };
    let Some(default) = &decl.params[0].default else {
        panic!("expected a default");
    };
    assert!(matches!(&default.kind, ExprKind::Match { arms, .. } if arms.len() == 2));
}

/// A multi-line `match` is a lambda body inside a call's parentheses.
#[test]
fn block_value_lambda_body_in_call_spans_lines() {
    let StmtKind::Expr(call) = only("print(map_all(fn(x): match x:\n    1: 5\n    _: 6\n))\n")
    else {
        panic!("expected a call statement");
    };
    let ExprKind::Call { args, .. } = &call.kind else {
        panic!("expected a call");
    };
    let ExprKind::Call { args: inner, .. } = &args[0].kind else {
        panic!("expected the inner call");
    };
    let ExprKind::Closure { body, .. } = &inner[0].kind else {
        panic!("expected a lambda");
    };
    assert!(matches!(body.kind, ExprKind::Match { .. }));
}

/// The lexer opens layout only when a block parser asks, so a map literal that breaks after its
/// `:` stays one line.
#[test]
fn map_literal_line_end_colon_is_unchanged() {
    let StmtKind::Let { value, .. } = only("m := {\"a\":\n    1}\n") else {
        panic!("expected a let");
    };
    assert!(matches!(&value.kind, ExprKind::Map(entries) if entries.len() == 1));
}

/// A block expression is a whole value. Anywhere else it reports the one message.
#[test]
fn block_expr_outside_a_value_reports_block_value() {
    for src in [
        "a := 1 + match n:\n    1: 2\n    _: 3\n",
        "print(match n:\n    1: 2\n    _: 3\n)\n",
        "xs := [match n:\n    1: 2\n    _: 3\n]\n",
        "x := (match n:\n    1: 2\n    _: 3\n)\n",
        "x := -match n:\n    1: 2\n    _: 3\n",
        "assert match n:\n    1: 2\n    _: 3\n",
        "x += match n:\n    1: 2\n    _: 3\n",
        "a := 1 + recover:\n    pass\n",
        "print(recover:\n    pass\n)\n",
        "if c: recover:\n    pass\n",
        "y := \"{f(match n: _: 1)}\"\n",
    ] {
        let e = parse(crate::lexer::tokenize(src).unwrap())
            .expect_err(&format!("{src:?} must be rejected"));
        assert_eq!(e.message, BLOCK_VALUE, "{src:?}");
    }
    for src in [
        "a := match n:\n    1: 2\n    _: 3\n",
        "a: int = match n:\n    1: 2\n    _: 3\n",
        "a = match n:\n    1: 2\n    _: 3\n",
        "a, b := match n:\n    1: (2, 3)\n    _: (4, 5)\n",
        "a, b = match n:\n    1: (2, 3)\n    _: (4, 5)\n",
        "fn f() -> int:\n    return match n:\n        1: 2\n        _: 3\n",
        "fn g() -> int: match n:\n    1: 2\n    _: 3\n",
        "f := fn(): match n:\n    1: 2\n    _: 3\n",
        "r := recover:\n    pass\n",
        "recover:\n    pass\n",
    ] {
        parse_ok(src);
    }
}

/// The chunks of the first `x := <literal>` statement of `src`, as the parser built them.
fn chunks_of(src: &str) -> Vec<Chunk> {
    let module = parse_ok(src);
    for stmt in module.stmts {
        let StmtKind::Let { value, .. } = stmt.kind else {
            continue;
        };
        let ExprKind::Interp(chunks) = value.kind else {
            panic!("expected an Interp in {src:?}, got {:?}", value.kind);
        };
        return chunks;
    }
    panic!("no binding in {src:?}");
}

/// TICKET-252 -- the parser builds `ExprKind::Interp` from the holes the lexer found, and every
/// hole expression carries its real source position: a literal with a hole is never an
/// `ExprKind::Str`, so nothing downstream re-reads text.
#[test]
fn a_holed_literal_parses_to_interp_with_real_spans() {
    // line 1: x := "a\tb{p}c{q:>5}"   `"`6 `a`7 `\`8 `t`9 `b`10 `{`11 `p`12 `}`13 `c`14 `{`15 `q`16
    let chunks = chunks_of("x := \"a\\tb{p}c{q:>5}\"\n");
    let [
        Chunk::Lit(a),
        Chunk::Expr(p, None, _),
        Chunk::Lit(c),
        Chunk::Expr(q, Some(spec), _),
    ] = &chunks[..]
    else {
        panic!("unexpected chunks {chunks:?}");
    };
    assert_eq!((a.as_str(), c.as_str()), ("a\tb", "c"));
    assert_eq!((p.span.line, p.span.col), (1, 12));
    assert_eq!((q.span.line, q.span.col), (1, 16));
    assert_eq!(spec.width, 5);
    assert_eq!(spec.align, Some(crate::fmtspec::Align::Right));
    // A literal with no hole stays a plain `Str` of its final text.
    let module = parse_ok("x := \"a{{b}}\"\n");
    let StmtKind::Let { value, .. } = &module.stmts[0].kind else {
        panic!("a binding");
    };
    assert_eq!(value.kind, ExprKind::Str("a{b}".to_string()));
}

/// Where a spec starts and what a nested field is: the lexer decides, the parser hands the
/// pieces to `fmtspec`. Ported from the tests of the scanner this ticket deleted.
#[test]
fn hole_specs_and_nested_fields_parse_as_expressions() {
    let chunks = chunks_of("x := \"{x:{w}.{p}f}\"\n");
    let [Chunk::Expr(value, Some(spec), fields)] = &chunks[..] else {
        panic!("expected one spec'd expr chunk, got {chunks:?}");
    };
    assert!(spec.dyn_width && spec.dyn_precision);
    assert_eq!(spec.ty, Some('f'));
    assert_eq!(fields.len(), 2, "width then precision");
    assert!(matches!(&fields[0].kind, ExprKind::Ident(n) if n == "w"));
    assert!(matches!(&fields[1].kind, ExprKind::Ident(n) if n == "p"));
    // `{x:{w}.{p}f}`: `w` sits 3 chars after `x`, `p` 7 (a real source column, like any hole).
    assert_eq!(fields[0].span.col, value.span.col + 3);
    assert_eq!(fields[1].span.col, value.span.col + 7);
    // Padding inside the nested braces is insignificant, as around the value.
    let chunks = chunks_of("x := \"{x:<{ w }}\"\n");
    let [Chunk::Expr(value, Some(_), fields)] = &chunks[..] else {
        panic!("expected one spec'd expr chunk");
    };
    assert_eq!(fields[0].span.col, value.span.col + 5);
    // A literal spec carries no nested field; a bare hole carries no spec.
    assert!(
        matches!(&chunks_of("x := \"{x:>5}\"\n")[..], [Chunk::Expr(_, Some(_), f)] if f.is_empty())
    );
    assert!(matches!(
        &chunks_of("x := \"{x}\"\n")[..],
        [Chunk::Expr(_, None, _)]
    ));
    // A colon inside an index, a nested literal or a bracket is not the separator.
    let chunks = chunks_of("x := \"{m[\"a:b\"]:>3}\"\n");
    assert!(matches!(&chunks[..], [Chunk::Expr(_, Some(s), _)] if s.width == 3));
    assert!(matches!(
        &chunks_of("x := \"{m[\"a:b\"]}\"\n")[..],
        [Chunk::Expr(_, None, _)]
    ));
    assert!(matches!(
        &chunks_of("x := \"{a[1..2]}\"\n")[..],
        [Chunk::Expr(_, None, _)]
    ));
    // A `}` inside a nested literal or a bracket does not close the hole; padding is insignificant.
    let chunks = chunks_of("x := \"v={d['a}}b']}\"\n");
    assert!(matches!(&chunks[..], [Chunk::Lit(l), Chunk::Expr(_, None, _)] if l == "v="));
    assert!(matches!(
        &chunks_of("x := \"{ {1, 2}.len() }\"\n")[..],
        [Chunk::Expr(_, None, _)]
    ));
    assert!(matches!(
        &chunks_of("x := \"{ x }\"\n")[..],
        [Chunk::Expr(_, None, _)]
    ));
}

/// A malformed hole is a parse error with the hole's own message, and a hole that held a
/// spec colon says where the spec starts.
#[test]
fn a_malformed_hole_is_a_parse_error_inside_the_hole() {
    let e = parse_err("x := \"{x:>99999999}\"\n");
    assert!(e.message.contains("exceeds maximum 4096"), "{}", e.message);
    let e = parse_err("x := \"{if c: 1 else: 2}\"\n");
    assert!(
        e.message
            .contains("wrap an expression that contains ':' in parentheses"),
        "{}",
        e.message
    );
    // The hint belongs to a hole that has a spec: a plain bad hole does not get it.
    let e = parse_err("x := \"{1 +}\"\n");
    assert!(!e.message.contains("parentheses"), "{}", e.message);
    let e = parse_err("x := \"{x y}\"\n");
    assert!(e.message.contains("after expression"), "{}", e.message);
}

/// A hole parses on the same parser as the code around it, so its nesting counts against the
/// same `depth` budget. 150 parentheses fit alone (the bound is about 260 levels), and 150 more
/// inside a hole's code do not fit on top of 150 outside it: a second parser that started at
/// depth zero would have accepted both halves.
#[test]
fn nested_holes_share_the_parser_depth_budget() {
    let parens = |n: usize| ("(".repeat(n), ")".repeat(n));
    let (open_150, close_150) = parens(150);
    let alone = format!("y := {open_150}1{close_150}\n");
    let composed = format!("x := 1\ny := {open_150}\"{{{open_150}x{close_150}}}\"{close_150}\n");
    let (alone_ok, composed_err) = crate::on_frontend_stack(move || {
        let alone_ok = parse(lexer::tokenize(&alone).unwrap()).is_ok();
        let composed_err = parse(lexer::tokenize(&composed).unwrap()).err();
        (alone_ok, composed_err.map(|e| e.message))
    });
    assert!(alone_ok, "150 parentheses must parse on their own");
    let message = composed_err.expect("a hole shares its parser's depth budget");
    assert!(
        message.starts_with("expression nested too deeply"),
        "{message}"
    );
    // The message says why: the hole spends the budget of the code around it.
    assert!(
        message.contains("interpolated") && message.contains(&MAX_AST_DEPTH.to_string()),
        "{message}"
    );
}
/// The value of the expression statement `src`.
fn expr_stmt(src: &str) -> Expr {
    match only(src) {
        StmtKind::Expr(e) => e,
        other => panic!("not an expression statement: {other:?}"),
    }
}

/// TICKET-253 -- `..` binds below shifts and arithmetic and above comparison and `in`.
#[test]
fn range_binds_below_shift_and_above_comparison() {
    let ExprKind::Range { start, end, .. } = expr_stmt("0..1 << 2\n").kind else {
        panic!("`0..1 << 2` is not a range")
    };
    assert!(matches!(start.unwrap().kind, ExprKind::Int(0)));
    assert!(matches!(
        end.unwrap().kind,
        ExprKind::Binary {
            op: BinaryOp::Shl,
            ..
        }
    ));
    let ExprKind::Binary { op, rhs, .. } = expr_stmt("x in 0..3\n").kind else {
        panic!("`x in 0..3` is not a binary")
    };
    assert_eq!(op, BinaryOp::In);
    assert!(matches!(rhs.kind, ExprKind::Range { .. }));
    let ExprKind::Binary { op, lhs, rhs } = expr_stmt("0..3 == 0..3\n").kind else {
        panic!("`0..3 == 0..3` is not a binary")
    };
    assert_eq!(op, BinaryOp::Eq);
    assert!(matches!(lhs.kind, ExprKind::Range { .. }));
    assert!(matches!(rhs.kind, ExprKind::Range { .. }));
    // Arithmetic on either side stays inside the bound.
    let ExprKind::Range { start, end, .. } = expr_stmt("a + 1..b - 1\n").kind else {
        panic!("not a range")
    };
    assert!(matches!(start.unwrap().kind, ExprKind::Binary { .. }));
    assert!(matches!(end.unwrap().kind, ExprKind::Binary { .. }));
}

/// TICKET-253 -- every form of the literal: bound present or omitted, inclusive or not.
#[test]
fn open_and_inclusive_range_forms_parse() {
    for (src, has_start, has_end, inclusive) in [
        ("a..b\n", true, true, false),
        ("a..=b\n", true, true, true),
        ("..b\n", false, true, false),
        ("..=b\n", false, true, true),
        ("a..\n", true, false, false),
        ("..\n", false, false, false),
    ] {
        let ExprKind::Range {
            start,
            end,
            inclusive: inc,
        } = expr_stmt(src).kind
        else {
            panic!("{src:?} is not a range")
        };
        assert_eq!(
            (start.is_some(), end.is_some(), inc),
            (has_start, has_end, inclusive),
            "{src:?}"
        );
    }
    // An open end closes at `)`, `]`, `,` and `:`.
    parse_ok("f(2.., 3)\nxs[2..]\nfor i in 5..:\n    pass\n");
}

/// TICKET-253 -- a range has two bounds; the step lives in `Range(a, b, step)`.
#[test]
fn a_range_with_a_third_bound_or_no_inclusive_end_is_a_parse_error() {
    let third = parse_err("a..b..c\n");
    assert!(
        third.message.contains("no third bound"),
        "{}",
        third.message
    );
    let open = parse_err("a..=\n");
    assert!(
        open.message.contains("inclusive range needs an end"),
        "{}",
        open.message
    );
}
