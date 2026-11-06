//! Parser tests: `in`, yield marking, `?.`/`??` binding, protocol decls, if/elif/else, defer,
//! bracket readings, comprehension guards.
//! Split out of `src/parser/mod.rs`. `super::*` is the parent module.

use super::*;

#[test]
fn in_expr_parses_as_binary() {
    match only("x := 1 in xs\n") {
        StmtKind::Let { value, .. } => match value.kind {
            ExprKind::Binary {
                op: BinaryOp::In, ..
            } => {}
            other => panic!("expected Binary(In), got {other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn for_in_still_parses() {
    // `in` as an operator must NOT break `for x in xs:` or `for x in 0..3:`.
    match only("for x in xs:\n    print(x)\n") {
        StmtKind::For { .. } => {}
        other => panic!("expected For, got {other:?}"),
    }
    match only("for x in 0..3:\n    print(x)\n") {
        StmtKind::For { .. } => {}
        other => panic!("expected For, got {other:?}"),
    }
}

#[test]
fn compound_assign_parses() {
    // `*= /= %= &= |= ^= <<= >>=` each lower to a distinct AssignOp.
    let cases = [
        ("x *= 2\n", AssignOp::StarEq),
        ("x /= 2\n", AssignOp::SlashEq),
        ("x %= 2\n", AssignOp::PercentEq),
        ("x &= 2\n", AssignOp::AmpEq),
        ("x |= 2\n", AssignOp::PipeEq),
        ("x ^= 2\n", AssignOp::CaretEq),
        ("x <<= 2\n", AssignOp::ShlEq),
        ("x >>= 2\n", AssignOp::ShrEq),
    ];
    for (src, want) in cases {
        match only(src) {
            StmtKind::Assign { op, .. } => assert_eq!(op, want, "for {src:?}"),
            other => panic!("{src:?} -> {other:?}"),
        }
    }
}

#[test]
fn parses_extern_block() {
    match only(
        "extern \"libm.so.6\":\n    fn cos(x: float) -> float\n    fn sqrt(x: float) -> float\n",
    ) {
        StmtKind::Extern { lib, fns } => {
            assert_eq!(lib, "libm.so.6");
            assert_eq!(fns.len(), 2);
            assert_eq!(fns[0].name, "cos");
            assert_eq!(fns[0].params.len(), 1);
            assert_eq!(fns[0].params[0].name, "x");
            assert_eq!(fns[0].params[0].ty, Some(Type::named("float")));
            assert_eq!(fns[0].ret, Some(Type::named("float")));
            assert_eq!(fns[1].name, "sqrt");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn parses_extern_void_return() {
    match only("extern \"libc.so.6\":\n    fn srand(seed: int)\n") {
        StmtKind::Extern { lib, fns } => {
            assert_eq!(lib, "libc.so.6");
            assert_eq!(fns.len(), 1);
            assert_eq!(fns[0].name, "srand");
            assert_eq!(fns[0].ret, None);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn extern_braces_is_error() {
    // Chezzi has no brace blocks; `extern "x" {` must not parse.
    parse_err("extern \"libc.so.6\" {\n    fn strlen(s: str) -> int\n}\n");
}

#[test]
fn extern_requires_string_lib() {
    parse_err("extern libc:\n    fn strlen(s: str) -> int\n");
}

/// TICKET-217: a bare `...` as the last extern param marks a C variadic function.
#[test]
fn extern_bare_ellipsis_parses() {
    match only("extern \"libc\":\n    fn printf(fmt: str, ...) -> int\n") {
        StmtKind::Extern { fns, .. } => {
            assert_eq!(fns[0].params.len(), 1);
            assert!(fns[0].c_variadic);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn bare_ellipsis_rejected_outside_extern() {
    let e = parse_err("fn f(a: int, ...):\n    pass\n");
    assert!(
        e.to_string()
            .contains("only allowed as the last parameter of an extern fn"),
        "{e}"
    );
}

#[test]
fn bare_ellipsis_must_be_last_in_extern() {
    let e = parse_err("extern \"libc\":\n    fn g(..., a: int)\n");
    assert!(
        e.to_string()
            .contains("must be the last parameter of an extern fn"),
        "{e}"
    );
}

#[test]
fn parses_parallel_with_spawn_call() {
    match only("parallel:\n    spawn worker(1)\n    spawn worker(2)\n") {
        StmtKind::Parallel { body } => {
            assert_eq!(body.len(), 2);
            assert!(matches!(
                body[0].kind,
                StmtKind::Spawn(SpawnTarget::Call(_))
            ));
            assert!(matches!(
                body[1].kind,
                StmtKind::Spawn(SpawnTarget::Call(_))
            ));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn parses_spawn_block_form() {
    match only("parallel:\n    spawn:\n        x := 1\n        f(x)\n") {
        StmtKind::Parallel { body } => match &body[0].kind {
            StmtKind::Spawn(SpawnTarget::Block(b)) => assert_eq!(b.len(), 2),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn spawn_non_call_form1_rejected() {
    assert!(
        parse_err("parallel:\n    spawn x + 1\n")
            .message
            .contains("spawn requires a function or method call")
    );
}

#[test]
fn parses_yield_statement() {
    match only("fn g() -> Iterator[int]:\n    yield 1\n") {
        StmtKind::Fn(d) => {
            assert!(d.is_generator, "fn with yield must be a generator");
            assert!(matches!(d.body[0].kind, StmtKind::Yield(_)));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn yield_in_nested_block_marks_generator() {
    match only("fn g() -> Iterator[int]:\n    for i in 0..3:\n        yield i\n") {
        StmtKind::Fn(d) => assert!(d.is_generator),
        other => panic!("{other:?}"),
    }
}

#[test]
fn yield_inside_recover_block_marks_generator() {
    // A `yield` reachable only through a `recover:` expression block still makes the fn a
    // generator (the detection walk descends into recover blocks).
    match only(
        "fn g() -> Iterator[int]:\n    x := recover:\n        yield 1\n        1\n    print(x)\n",
    ) {
        StmtKind::Fn(d) => assert!(
            d.is_generator,
            "yield in a recover: block must mark the fn a generator"
        ),
        other => panic!("{other:?}"),
    }
}

#[test]
fn fn_without_yield_is_not_generator() {
    match only("fn f() -> int:\n    return 1\n") {
        StmtKind::Fn(d) => assert!(!d.is_generator),
        other => panic!("{other:?}"),
    }
}

#[test]
fn yield_in_nested_fn_does_not_mark_outer() {
    // The inner `fn` owns its yield; the outer must NOT be flagged a generator.
    let src =
        "fn outer() -> int:\n    fn inner() -> Iterator[int]:\n        yield 1\n    return 0\n";
    match only(src) {
        StmtKind::Fn(d) => {
            assert!(!d.is_generator, "outer fn must not be a generator");
            match &d.body[0].kind {
                StmtKind::Fn(inner) => assert!(inner.is_generator, "inner fn is the generator"),
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn parses_wait_with_bind_assign_discard_and_else() {
    let src = "wait:\n    v := orders.recv(): handle(v)\n    result = cancels.recv(): result = 1\n    _ := timer(500).recv(): on_timeout()\n    else: poll_miss()\n";
    match only(src) {
        StmtKind::Wait { arms, else_block } => {
            assert_eq!(arms.len(), 3);
            let recv = |a: &WaitArm| match &a.kind {
                WaitArmKind::Recv { target, chan } => (target.clone(), chan.clone()),
                other => panic!("expected recv arm, got {other:?}"),
            };
            let (t0, c0) = recv(&arms[0]);
            assert!(matches!(t0, Pattern::Ident(ref n, _, _) if n == "v"));
            assert!(matches!(&c0.kind, ExprKind::Ident(n) if n == "orders"));
            // `result = ...` binds a fresh `$waitN`, then assigns it as the body's first statement.
            let (t1, _) = recv(&arms[1]);
            assert!(matches!(t1, Pattern::Ident(ref n, _, _) if n.starts_with("$wait")));
            assert!(matches!(&arms[1].body[0].kind, StmtKind::Assign { .. }));
            let (t2, c2) = recv(&arms[2]);
            assert!(matches!(t2, Pattern::Wildcard));
            // a timer arm's channel expr is the `timer(500)` call, evaluated once.
            assert!(matches!(&c2.kind, ExprKind::Call { .. }));
            assert!(else_block.is_some());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn parses_wait_bare_send_arm() {
    // A bare `ch.send(v):` arm (no `:=`/`=`) parses to a lenient `Send{call}` — the checker
    // validates the `.send()` shape, not the parser.
    let src = "wait:\n    v := ch.recv(): use(v)\n    out.send(7): done()\n";
    match only(src) {
        StmtKind::Wait { arms, else_block } => {
            assert_eq!(arms.len(), 2);
            assert!(matches!(arms[0].kind, WaitArmKind::Recv { .. }));
            match &arms[1].kind {
                WaitArmKind::Send { call } => {
                    assert!(matches!(&call.kind, ExprKind::Call { .. }));
                }
                other => panic!("expected send arm, got {other:?}"),
            }
            assert!(else_block.is_none());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn parses_wait_indented_arm_bodies() {
    let src = "wait:\n    v := ch.recv():\n        print(v)\n        f(v)\n";
    match only(src) {
        StmtKind::Wait { arms, else_block } => {
            assert_eq!(arms.len(), 1);
            assert_eq!(arms[0].body.len(), 2);
            assert!(else_block.is_none());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn wait_arm_non_recv_rhs_rejected() {
    assert!(
        parse_err("wait:\n    v := f(): g()\n")
            .message
            .contains("must `recv` from a channel")
    );
}

#[test]
fn wait_arm_recv_with_args_rejected() {
    assert!(
        parse_err("wait:\n    v := ch.recv(5): g()\n")
            .message
            .contains("takes no arguments")
    );
}

#[test]
fn parses_wait_arm_guards() {
    // `if <expr>` after the arm head: recv `:=`, `=`, `_ :=` and send arms. The guard is the
    // whole expression after `if`; the channel expr stops before it.
    let src = "wait:\n    v := a.recv() if not paused: f(v)\n    n = b.recv() if ok: g()\n    _ := c.recv() if x > 1: h()\n    out.send(v) if ready: k()\n    w := d.recv(): m()\n";
    match only(src) {
        StmtKind::Wait { arms, else_block } => {
            assert_eq!(arms.len(), 5);
            assert!(matches!(
                &arms[0].guard,
                Some(Expr {
                    kind: ExprKind::Unary { .. },
                    ..
                })
            ));
            assert!(
                matches!(&arms[1].guard, Some(Expr { kind: ExprKind::Ident(n), .. }) if n == "ok")
            );
            assert!(matches!(
                &arms[2].guard,
                Some(Expr {
                    kind: ExprKind::Binary { .. },
                    ..
                })
            ));
            assert!(
                matches!(&arms[3].guard, Some(Expr { kind: ExprKind::Ident(n), .. }) if n == "ready")
            );
            assert!(arms[4].guard.is_none());
            match &arms[0].kind {
                WaitArmKind::Recv { chan, .. } => {
                    assert!(matches!(&chan.kind, ExprKind::Ident(n) if n == "a"))
                }
                other => panic!("expected recv arm, got {other:?}"),
            }
            // The `=` arm still assigns its `$waitN` as the body's first statement.
            assert!(matches!(&arms[1].body[0].kind, StmtKind::Assign { .. }));
            assert!(matches!(arms[3].kind, WaitArmKind::Send { .. }));
            assert!(else_block.is_none());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn wait_else_takes_no_guard() {
    assert!(
        parse_err("wait:\n    v := a.recv(): f(v)\n    else if c: g()\n")
            .message
            .contains("expected")
    );
}

#[test]
fn wait_else_must_be_last() {
    assert!(
        parse_err("wait:\n    else: a()\n    v := ch.recv(): b()\n")
            .message
            .contains("must be the last arm")
    );
}

#[test]
fn wait_requires_at_least_one_arm() {
    assert!(
        parse_err("wait:\n    else: a()\n")
            .message
            .contains("at least one `recv` or `send` arm")
    );
}

#[test]
fn parses_null_coalesce_right_assoc() {
    // `a ?? b ?? c` = `a ?? (b ?? c)`.
    match let_value("x := a ?? b ?? c\n").kind {
        ExprKind::NullCoalesce { lhs, rhs, .. } => {
            assert!(matches!(lhs.kind, ExprKind::Ident(ref n) if n == "a"));
            assert!(
                matches!(rhs.kind, ExprKind::NullCoalesce { .. }),
                "rhs must nest"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn parses_opt_chain_field() {
    match let_value("x := y?.f\n").kind {
        ExprKind::OptChain {
            obj, name, call, ..
        } => {
            assert!(matches!(obj.kind, ExprKind::Ident(ref n) if n == "y"));
            assert_eq!(name, "f");
            assert!(call.is_none());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn parses_opt_chain_method() {
    match let_value("x := y?.f(1, 2)\n").kind {
        ExprKind::OptChain { name, call, .. } => {
            assert_eq!(name, "f");
            assert_eq!(call.expect("a call").args.len(), 2);
        }
        other => panic!("{other:?}"),
    }
}

/// `?.` shares the `.` arm's `[types](` parse: `r?.m[int]()` is an optional-chained call with a
/// type argument, and a one-arg bracket keeps its expression reading (`bracket`), so the checker
/// can index then call when the member is data (`r?.fs[k](1)`, TICKET-210).
#[test]
fn optchain_type_args() {
    let opt_call = |src: &str| match let_value(src).kind {
        ExprKind::OptChain {
            name,
            call: Some(c),
            ..
        } => (name, c),
        other => panic!("{src}: {other:?}"),
    };
    let (name, c) = opt_call("x := r?.m[int]()\n");
    assert_eq!(name, "m");
    assert!(matches!(c.type_args.as_slice(), [Type::Named { name, .. }] if name == "int"));
    assert!(c.args.is_empty());
    let (_, c) = opt_call("x := r?.m[int, str](1)\n");
    assert_eq!((c.type_args.len(), c.args.len()), (2, 1));
    assert!(
        c.bracket.is_none(),
        "two type args have no expression reading"
    );
    let (name, c) = opt_call("x := r?.fs[k](1)\n");
    assert_eq!(name, "fs");
    assert!(matches!(c.bracket.map(|b| b.kind), Some(ExprKind::Ident(k)) if k == "k"));
    // A bracket that is no type list stays an index on the chain.
    assert!(matches!(
        let_value("x := r?.fs[0](1)\n").kind,
        ExprKind::Call { .. }
    ));
}

#[test]
fn opt_chain_chains_left_assoc() {
    // `y?.a?.b` = `(y?.a)?.b`.
    match let_value("x := y?.a?.b\n").kind {
        ExprKind::OptChain { obj, name, .. } => {
            assert_eq!(name, "b");
            assert!(
                matches!(obj.kind, ExprKind::OptChain { .. }),
                "obj must be the inner chain"
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn opt_chain_links_share_a_span_but_not_a_name_span() {
    // `parse_postfix` reuses the PRIMARY's span for every postfix link, so `span` cannot key a
    // per-link table — `name_span` (the `?.`-following name token) is what distinguishes them.
    // `x := a?.b?.c` → cols: a=6, b=9, c=12.
    let outer = let_value("x := a?.b?.c\n");
    let ExprKind::OptChain {
        obj,
        name,
        name_span,
        ..
    } = outer.kind
    else {
        panic!("outer must be an OptChain");
    };
    assert_eq!(name, "c");
    let inner_span = obj.span;
    let ExprKind::OptChain {
        name: inner_name,
        name_span: inner_name_span,
        ..
    } = obj.kind
    else {
        panic!("inner must be an OptChain");
    };
    assert_eq!(inner_name, "b");
    assert_eq!(
        outer.span, inner_span,
        "both links carry the primary `a`'s span"
    );
    assert_eq!(
        outer.span,
        Span {
            line: 1,
            col: 6,
            file: 0
        }
    );
    assert_eq!(
        inner_name_span,
        Span {
            line: 1,
            col: 9,
            file: 0
        }
    );
    assert_eq!(
        name_span,
        Span {
            line: 1,
            col: 12,
            file: 0
        }
    );
    assert_ne!(name_span, inner_name_span, "each link needs its own key");
}

#[test]
fn opt_chain_binds_tighter_than_coalesce() {
    // `y?.f ?? z` = `(y?.f) ?? z`.
    match let_value("x := y?.f ?? z\n").kind {
        ExprKind::NullCoalesce { lhs, rhs, .. } => {
            assert!(matches!(lhs.kind, ExprKind::OptChain { .. }));
            assert!(matches!(rhs.kind, ExprKind::Ident(ref n) if n == "z"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn null_coalesce_op_span_is_the_operator_token_not_the_lhs() {
    // `(a ?? b) ?? c`: both nodes share `a`'s `Expr::span` (parse_bp reuses `lhs.span`, and
    // `(e)` grouping keeps the inner span), but their `op_span`s must differ — that's the real
    // `CarrierKey` coordinate.
    let outer = let_value("x := (a ?? b) ?? c\n");
    let ExprKind::NullCoalesce {
        lhs: outer_lhs,
        op_span: outer_op_span,
        ..
    } = outer.kind
    else {
        panic!("{:?}", outer.kind);
    };
    let ExprKind::NullCoalesce {
        op_span: inner_op_span,
        ..
    } = outer_lhs.kind
    else {
        panic!("{:?}", outer_lhs.kind);
    };
    assert_eq!(outer.span, outer_lhs.span, "both nodes share lhs's span");
    assert_ne!(
        outer_op_span, inner_op_span,
        "each '??' needs its own op_span"
    );
}

#[test]
fn generic_fn_decl_with_bound() {
    match only("fn max[T: Comparable](a: T, b: T) -> T:\n    return a\n") {
        StmtKind::Fn(f) => {
            assert_eq!(f.name, "max");
            assert_eq!(f.type_params.len(), 1);
            assert_eq!(f.type_params[0].name, "T");
            assert_eq!(
                f.type_params[0].bounds,
                vec![Bound {
                    name: "Comparable".into(),
                    args: vec![]
                }]
            );
            assert_eq!(f.params.len(), 2);
            assert_eq!(f.ret, Some(Type::named("T")));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn generic_fn_decl_parameterized_bound() {
    // `S: Iterator[T]` — a parameterized protocol bound records the bound name + its type args.
    match only("fn collect[S: Iterator[T], T](xs: S) -> T:\n    return xs\n") {
        StmtKind::Fn(f) => {
            assert_eq!(f.type_params.len(), 2);
            assert_eq!(f.type_params[0].name, "S");
            assert_eq!(
                f.type_params[0].bounds,
                vec![Bound {
                    name: "Iterator".into(),
                    args: vec![Type::named("T")]
                }]
            );
            assert_eq!(f.type_params[1].name, "T");
            assert_eq!(f.type_params[1].bounds, Vec::<Bound>::new());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn qualified_bound_parses_as_a_dotted_name() {
    // TICKET-173: `m.P` in a bound keeps its dotted source spelling; the checker resolves it.
    match only("fn f[T: m.P + Q, S: m.Box[int]](x: T, s: S) -> T where T: m.R:\n    return x\n") {
        StmtKind::Fn(f) => {
            let names: Vec<&str> = f.type_params[0]
                .bounds
                .iter()
                .map(|b| b.name.as_str())
                .collect();
            assert_eq!(names, ["m.P", "Q"]);
            assert_eq!(
                f.type_params[1].bounds,
                vec![Bound {
                    name: "m.Box".into(),
                    args: vec![Type::named("int")]
                }]
            );
            assert_eq!(f.where_bounds[0].bounds[0].name, "m.R");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn qualified_protocol_embed_parses_as_a_dotted_name() {
    match only("protocol Q:\n    m.Tagged + Add\n") {
        StmtKind::Protocol { embeds, .. } => {
            let names: Vec<&str> = embeds.iter().map(|b| b.name.as_str()).collect();
            assert_eq!(names, ["m.Tagged", "Add"]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_three_level_bound_path_parses_as_a_dotted_name() {
    // The parser reads any dotted path; the CHECKER refuses a 3+ segment bound with the
    // two-level hint (TICKET-173), so multi-level module paths only change resolution.
    match only("fn f[T: a.b.C](x: T) -> T:\n    return x\n") {
        StmtKind::Fn(f) => assert_eq!(f.type_params[0].bounds[0].name, "a.b.C"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn duplicate_type_param_rejected() {
    assert!(
        parse_err("fn f[T, T](a: T) -> T:\n    return a\n")
            .message
            .contains("duplicate type parameter")
    );
    assert!(
        parse_err("struct S[T, T]:\n    x: T\n")
            .message
            .contains("duplicate type parameter")
    );
    assert!(
        parse_err("enum E[T, T]:\n    A(T)\n")
            .message
            .contains("duplicate type parameter")
    );
    // distinct names still parse fine
    parse_ok("fn f[T, U](a: T, b: U) -> T:\n    return a\n");
}

#[test]
fn generic_fn_decl_multi_param_unbounded() {
    match only("fn pair[A, B](a: A, b: B):\n    print(a)\n") {
        StmtKind::Fn(f) => {
            assert_eq!(f.type_params.len(), 2);
            assert_eq!(f.type_params[0].name, "A");
            assert_eq!(f.type_params[0].bounds, Vec::<Bound>::new());
            assert_eq!(f.type_params[1].name, "B");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn generic_struct_decl() {
    match only("struct Pair[A, B]:\n    first: A\n    second: B\n") {
        StmtKind::Struct {
            name,
            type_params,
            fields,
            ..
        } => {
            assert_eq!(name, "Pair");
            assert_eq!(type_params.len(), 2);
            assert_eq!(type_params[0].name, "A");
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].ty, Type::named("A"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn protocol_decl_collects_method_sigs() {
    match only("protocol Comparable:\n    fn compare(self, other: Self) -> int\n") {
        StmtKind::Protocol { name, methods, .. } => {
            assert_eq!(name, "Comparable");
            assert_eq!(methods.len(), 1);
            assert_eq!(methods[0].name, "compare");
            assert_eq!(methods[0].params.len(), 2);
            assert_eq!(methods[0].params[1].ty, Some(Type::named("Self")));
            assert_eq!(methods[0].ret, Some(Type::named("int")));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn method_param_default_parses() {
    let src =
        "struct P:\n    n: int\n    fn add(self, x: int = 5) -> int:\n        return self.n + x\n";
    match only(src) {
        StmtKind::Struct { methods, .. } => {
            assert_eq!(methods.len(), 1);
            assert_eq!(methods[0].params.len(), 2);
            assert!(methods[0].params[1].default.is_some());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn parameterized_protocol_decl_collects_type_params() {
    match only("protocol Container[T]:\n    fn get(self, i: int) -> T\n") {
        StmtKind::Protocol {
            name,
            type_params,
            methods,
            ..
        } => {
            assert_eq!(name, "Container");
            assert_eq!(type_params.len(), 1);
            assert_eq!(type_params[0].name, "T");
            assert_eq!(methods.len(), 1);
            assert_eq!(methods[0].ret, Some(Type::named("T")));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn protocol_with_multiple_method_sigs() {
    let src = "protocol Shape:\n    fn area(self) -> float\n    fn name(self) -> str\n";
    match only(src) {
        StmtKind::Protocol { methods, .. } => assert_eq!(methods.len(), 2),
        other => panic!("{other:?}"),
    }
}

/// M22: a protocol body may carry embed lines (`Add + Sub`, parameterized refs) interleaved with
/// `fn` sigs, order-free. The embeds collect into `embeds`, the methods into `methods`.
#[test]
fn protocol_with_embed_lines_parses() {
    let src = "protocol Vector:\n    Add + Sub\n    fn dot(self, o: Self) -> float\n    Mul\n    fn norm(self) -> float\n";
    match only(src) {
        StmtKind::Protocol {
            name,
            methods,
            embeds,
            ..
        } => {
            assert_eq!(name, "Vector");
            let enames: Vec<&str> = embeds.iter().map(|b| b.name.as_str()).collect();
            assert_eq!(enames, vec!["Add", "Sub", "Mul"]);
            let mnames: Vec<&str> = methods.iter().map(|m| m.name.as_str()).collect();
            assert_eq!(mnames, vec!["dot", "norm"]);
        }
        other => panic!("{other:?}"),
    }
}

/// M22: a bundle is a body of only embed lines (`Arithmetic: Add + Sub + Mul + Div`).
#[test]
fn protocol_pure_bundle_parses() {
    match only("protocol Arithmetic:\n    Add + Sub + Mul + Div\n") {
        StmtKind::Protocol {
            methods, embeds, ..
        } => {
            assert!(methods.is_empty());
            let enames: Vec<&str> = embeds.iter().map(|b| b.name.as_str()).collect();
            assert_eq!(enames, vec!["Add", "Sub", "Mul", "Div"]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn walrus_let() {
    match only("x := 5\n") {
        StmtKind::Let {
            target, ty, value, ..
        } => {
            assert_eq!(target.binder_names(), vec!["x".to_string()]);
            assert!(ty.is_none());
            assert_eq!(value.kind, ExprKind::Int(5));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn break_and_continue_stmts() {
    assert_eq!(only("break\n"), StmtKind::Break);
    assert_eq!(only("continue\n"), StmtKind::Continue);
}

#[test]
fn break_continue_inline_in_loop() {
    // `for i in 0..3: break` — inline block body is a single simple statement.
    let m = parse_ok("for i in 0..3: break\n");
    assert_eq!(m.stmts.len(), 1);
    match &m.stmts[0].kind {
        StmtKind::For { body, .. } => assert_eq!(body[0].kind, StmtKind::Break),
        other => panic!("{other:?}"),
    }
}

#[test]
fn typed_let() {
    match only("name: str = \"chezzi\"\n") {
        StmtKind::Let {
            target, ty, value, ..
        } => {
            assert_eq!(target.binder_names(), vec!["name".to_string()]);
            assert_eq!(ty, Some(Type::named("str")));
            assert_eq!(value.kind, ExprKind::Str("chezzi".into()));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn assign_ops() {
    assert!(matches!(
        only("count += 1\n"),
        StmtKind::Assign {
            op: AssignOp::PlusEq,
            ..
        }
    ));
    assert!(matches!(
        only("count -= 1\n"),
        StmtKind::Assign {
            op: AssignOp::MinusEq,
            ..
        }
    ));
    assert!(matches!(
        only("count = 1\n"),
        StmtKind::Assign {
            op: AssignOp::Eq,
            ..
        }
    ));
}

#[test]
fn fn_decl_with_and_without_return() {
    match only("fn add(a: int, b: int) -> int:\n    return a + b\n") {
        StmtKind::Fn(f) => {
            assert_eq!(f.name, "add");
            assert_eq!(f.params.len(), 2);
            assert_eq!(f.ret, Some(Type::named("int")));
            assert_eq!(f.body.len(), 1);
        }
        other => panic!("{other:?}"),
    }
    match only("fn log(msg: str):\n    print(msg)\n") {
        StmtKind::Fn(f) => assert!(f.ret.is_none()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn struct_with_field_and_method() {
    match only(
        "struct Point:\n    x: int\n    y: int\n\n    fn dist(self) -> float:\n        return self.x\n",
    ) {
        StmtKind::Struct {
            name,
            fields,
            methods,
            ..
        } => {
            assert_eq!(name, "Point");
            assert_eq!(fields.len(), 2);
            assert_eq!(methods.len(), 1);
            assert_eq!(methods[0].name, "dist");
            // `self` param carries no type annotation
            assert!(methods[0].params[0].ty.is_none());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn enum_with_and_without_payload() {
    match only("enum Shape:\n    Circle(int)\n    Point\n") {
        StmtKind::Enum {
            name,
            type_params,
            variants,
            ..
        } => {
            assert_eq!(name, "Shape");
            assert!(type_params.is_empty());
            assert_eq!(variants[0].name, "Circle");
            assert_eq!(variants[0].payload, vec![Type::named("int")]);
            assert_eq!(variants[1].name, "Point");
            assert!(variants[1].payload.is_empty());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn generic_enum_parses_type_params() {
    match only("enum Tree[T]:\n    Leaf\n    Node(T, Tree[T], Tree[T])\n") {
        StmtKind::Enum {
            name,
            type_params,
            variants,
            ..
        } => {
            assert_eq!(name, "Tree");
            assert_eq!(type_params.len(), 1);
            assert_eq!(type_params[0].name, "T");
            assert_eq!(variants[0].name, "Leaf");
            assert_eq!(variants[1].name, "Node");
            assert_eq!(variants[1].payload.len(), 3);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn if_elif_else() {
    match only("if x > 0:\n    print(1)\nelif x == 0:\n    print(2)\nelse:\n    print(3)\n") {
        StmtKind::If {
            branches,
            else_block,
        } => {
            assert_eq!(branches.len(), 2);
            assert!(else_block.is_some());
        }
        other => panic!("{other:?}"),
    }
}

/// `else if` is no longer valid syntax — `elif` is the one obvious way. The else-block's
/// `parse_block` expects `:` and hits the `if` keyword instead.
#[test]
fn rejects_bare_else_if_stmt() {
    assert!(
        parse_err("if a:\n    f()\nelse if b:\n    g()\n")
            .message
            .contains(':')
    );
}

#[test]
fn for_over_range_and_list() {
    match only("for i in 0..10:\n    print(i)\n") {
        StmtKind::For { target, iter, .. } => {
            assert_eq!(target.binder_names(), vec!["i".to_string()]);
            assert!(matches!(iter.kind, ExprKind::Range { .. }));
        }
        other => panic!("{other:?}"),
    }
    match only("for item in items:\n    print(item)\n") {
        StmtKind::For { iter, .. } => assert!(matches!(iter.kind, ExprKind::Ident(_))),
        other => panic!("{other:?}"),
    }
}

#[test]
fn while_loop() {
    assert!(matches!(
        only("while cond:\n    cond = step()\n"),
        StmtKind::While { .. }
    ));
}

/// Helper: is this range bound a literal int `n`?
fn is_int(c: &Option<Box<Expr>>, n: i64) -> bool {
    matches!(c.as_deref(), Some(Expr { kind: ExprKind::Int(v), .. }) if *v == n)
}

/// TICKET-253 -- a subscript is one `Index` whose index is an expression; `xs[a..b]` holds a
/// `Range` value, and the colon spelling is a parse error that names its replacement.
#[test]
fn subscript_holds_one_expression() {
    match let_value("y := xs[2]\n").kind {
        ExprKind::Index { index, .. } => {
            assert!(matches!(index.unwrap().kind, ExprKind::Int(2)));
        }
        other => panic!("expected Index, got {other:?}"),
    }
    for (src, has_start, has_end, inclusive) in [
        ("y := xs[1..3]\n", true, true, false),
        ("y := xs[1..]\n", true, false, false),
        ("y := xs[..3]\n", false, true, false),
        ("y := xs[..]\n", false, false, false),
        ("y := xs[1..=3]\n", true, true, true),
    ] {
        let ExprKind::Index { index, .. } = let_value(src).kind else {
            panic!("{src:?}: not an Index")
        };
        let ExprKind::Range {
            start,
            end,
            inclusive: inc,
        } = index.unwrap().kind
        else {
            panic!("{src:?}: the index is not a Range")
        };
        assert_eq!(
            (start.is_some(), end.is_some(), inc),
            (has_start, has_end, inclusive),
            "{src:?}"
        );
    }
    // A `:` in a subscript is no syntax: an ordinary parse error.
    for src in [
        "y := xs[1:3]\n",
        "y := xs[1:]\n",
        "y := xs[:]\n",
        "y := xs[1:5:2]\n",
        "y := xs[::-1]\n",
    ] {
        let e = parse_err(src);
        assert!(!e.message.contains("removed"), "{src:?}: {}", e.message);
    }
    // A bound is any expression the range operator takes.
    let ExprKind::Index { index, .. } = let_value("y := xs[i + 1..n - 1]\n").kind else {
        panic!("not an Index")
    };
    assert!(is_int_free_range(&index.unwrap()));
}

fn is_int_free_range(e: &Expr) -> bool {
    matches!(
        &e.kind,
        ExprKind::Range { start: Some(a), end: Some(b), .. }
            if matches!(a.kind, ExprKind::Binary { .. }) && matches!(b.kind, ExprKind::Binary { .. })
    )
}

#[test]
fn dotdot_still_range_outside_subscript() {
    // for-loop iterable stays a Range
    match only("for i in 0..10:\n    print(i)\n") {
        StmtKind::For { iter, .. } => assert!(matches!(iter.kind, ExprKind::Range { .. })),
        other => panic!("{other:?}"),
    }
    // match range-pattern still parses (0..10 => ...)
    assert!(
        parse(
            lexer::tokenize(
                "fn f(x: int):\n    match x:\n        0..10: print(1)\n        _: print(2)\n"
            )
            .unwrap()
        )
        .is_ok()
    );
}

#[test]
fn a_range_index_assignment_parses() {
    // `xs[1..3] = v` parses (an index assignment by a Range); the checker rejects it
    // ("cannot assign to a slice"), see `checker::tests`.
    parse_ok("xs := [1, 2, 3]\nxs[1..3] = [9]\n");
}

#[test]
fn fns_index_then_call_still_parses() {
    // fns[0](x) must remain index-then-call, not a slice.
    match let_value("y := fns[0](x)\n").kind {
        ExprKind::Call { callee, .. } => {
            assert!(matches!(callee.kind, ExprKind::Index { .. }));
        }
        other => panic!("expected Call over Index, got {other:?}"),
    }
}

#[test]
fn match_inline_arms() {
    match only("match s:\n    Circle(r): return r\n    Square(n): return n\n    Point: return 0\n")
    {
        StmtKind::Match { arms, .. } => {
            assert_eq!(arms.len(), 3);
            match &arms[0].pattern {
                Pattern::Variant { name, bindings, .. } => {
                    assert_eq!(name, "Circle");
                    assert_eq!(bindings.len(), 1);
                    assert!(matches!(&bindings[0], Pattern::Ident(n, _, _) if n == "r"));
                }
                other => panic!("{other:?}"),
            }
            assert!(
                arms[2].pattern
                    == Pattern::Variant {
                        id: crate::ast::NodeId::fresh(),
                        name: "Point".into(),
                        bindings: vec![],
                        enum_name: None,
                        module_name: None
                    }
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn match_literal_and_wildcard_patterns() {
    match only(
        "match n:\n    0: return 1\n    \"a\": return 2\n    true: return 3\n    _: return 4\n",
    ) {
        StmtKind::Match { arms, .. } => {
            assert_eq!(arms.len(), 4);
            assert_eq!(arms[0].pattern, Pattern::Literal(LitPattern::Int(0)));
            assert_eq!(
                arms[1].pattern,
                Pattern::Literal(LitPattern::Str("a".into()))
            );
            assert_eq!(arms[2].pattern, Pattern::Literal(LitPattern::Bool(true)));
            assert_eq!(arms[3].pattern, Pattern::Wildcard);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn return_with_and_without_value() {
    assert!(matches!(only("fn f():\n    return\n"), StmtKind::Fn(_)));
    // dig into the body of `fn f(): return`
    let StmtKind::Fn(f) = only("fn f():\n    return\n") else {
        panic!()
    };
    assert!(matches!(f.body[0].kind, StmtKind::Return(None)));
    let StmtKind::Fn(g) = only("fn g():\n    return 1\n") else {
        panic!()
    };
    assert!(matches!(g.body[0].kind, StmtKind::Return(Some(_))));
}

#[test]
fn defer_parses_call_in_fn_body() {
    let StmtKind::Fn(f) = only("fn f():\n    defer cleanup()\n    defer obj.close(1)\n") else {
        panic!()
    };
    assert!(matches!(f.body[0].kind, StmtKind::Defer(_)));
    let StmtKind::Defer(DeferTarget::Call(Expr {
        kind: ExprKind::Call { .. },
        ..
    })) = &f.body[0].kind
    else {
        panic!("first defer not a call")
    };
    let StmtKind::Defer(DeferTarget::Call(Expr {
        kind: ExprKind::Call { callee, .. },
        ..
    })) = &f.body[1].kind
    else {
        panic!("second defer not a call")
    };
    assert!(matches!(callee.kind, ExprKind::Field { .. }));
}

#[test]
fn defer_parses_block_form() {
    let StmtKind::Fn(f) = only("fn f():\n    defer:\n        foo()\n        bar()\n") else {
        panic!()
    };
    let StmtKind::Defer(DeferTarget::Block(body)) = &f.body[0].kind else {
        panic!("defer: not parsed as a block")
    };
    assert_eq!(body.len(), 2);
    assert!(matches!(body[0].kind, StmtKind::Expr(_)));
    assert!(matches!(body[1].kind, StmtKind::Expr(_)));
}

#[test]
fn all_import_forms() {
    // `Import`'s `PartialEq` is equality-neutral on the bound-name spans, so the default spans
    // here compare equal to the parser's real spans.
    assert_eq!(
        only("import std.io\n"),
        StmtKind::Import(Import::Module {
            path: vec!["std".into(), "io".into()],
            alias: None,
            name_span: Span::default(),
        })
    );
    assert_eq!(
        only("import std.io as fs\n"),
        StmtKind::Import(Import::Module {
            path: vec!["std".into(), "io".into()],
            alias: Some("fs".into()),
            name_span: Span::default(),
        })
    );
    assert_eq!(
        only("import read, write from std.io\n"),
        StmtKind::Import(Import::From {
            path: vec!["std".into(), "io".into()],
            names: vec![("read".into(), None), ("write".into(), None)],
            name_spans: vec![Span::default(), Span::default()],
        })
    );
    assert_eq!(
        only("import read as r from std.io\n"),
        StmtKind::Import(Import::From {
            path: vec!["std".into(), "io".into()],
            names: vec![("read".into(), Some("r".into()))],
            name_spans: vec![Span::default()],
        })
    );
}

#[test]
fn closure_expr() {
    let StmtKind::Let { value, .. } = only("double := fn(x: int) -> int: x * 2\n") else {
        panic!()
    };
    match value.kind {
        ExprKind::Closure { params, ret, body } => {
            assert_eq!(params.len(), 1);
            assert_eq!(ret, Some(Type::named("int")));
            assert!(matches!(
                body.kind,
                ExprKind::Binary {
                    op: BinaryOp::Mul,
                    ..
                }
            ));
        }
        other => panic!("{other:?}"),
    }
}

/// `1 + 2 * 3` must parse as `1 + (2 * 3)`.
#[test]
fn precedence_mul_over_add() {
    let StmtKind::Expr(e) = only("1 + 2 * 3\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Binary {
            op: BinaryOp::Add,
            rhs,
            ..
        } => assert!(matches!(
            rhs.kind,
            ExprKind::Binary {
                op: BinaryOp::Mul,
                ..
            }
        )),
        other => panic!("{other:?}"),
    }
}

/// `a and b or c` must parse as `(a and b) or c`.
#[test]
fn precedence_and_over_or() {
    let StmtKind::Expr(e) = only("a and b or c\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Binary {
            op: BinaryOp::Or,
            lhs,
            ..
        } => assert!(matches!(
            lhs.kind,
            ExprKind::Binary {
                op: BinaryOp::And,
                ..
            }
        )),
        other => panic!("{other:?}"),
    }
}

#[test]
fn postfix_call_field_index_try() {
    // p.dist() → Call(Field(p, dist))
    let StmtKind::Expr(e) = only("p.dist()\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Call { callee, .. } => assert!(matches!(callee.kind, ExprKind::Field { .. })),
        other => panic!("{other:?}"),
    }
    // safe_div(1, 2)? → Try(Call(...))
    let StmtKind::Expr(e) = only("safe_div(1, 2)?\n") else {
        panic!()
    };
    assert!(matches!(e.kind, ExprKind::Try(_)));
    // xs[0] → Index
    let StmtKind::Expr(e) = only("xs[0]\n") else {
        panic!()
    };
    assert!(matches!(e.kind, ExprKind::Index { .. }));
}

#[test]
fn index_vs_slice() {
    // xs[i] → Index (plain subscript)
    let StmtKind::Expr(e) = only("xs[i]\n") else {
        panic!()
    };
    assert!(matches!(e.kind, ExprKind::Index { .. }));
    // m["k"] → Index (non-int key still plain index)
    let StmtKind::Expr(e) = only("m[\"k\"]\n") else {
        panic!()
    };
    assert!(matches!(e.kind, ExprKind::Index { .. }));
    // xs[1..3] → Index by a Range with int bounds
    let StmtKind::Expr(e) = only("xs[1..3]\n") else {
        panic!()
    };
    let ExprKind::Index { index, .. } = e.kind else {
        panic!("expected Index, got {:?}", e.kind)
    };
    let ExprKind::Range { start, end, .. } = index.unwrap().kind else {
        panic!("expected a Range index")
    };
    assert!(is_int(&start, 1) && is_int(&end, 3));
    // nested xs[a..b][0] → Index(Index)
    let StmtKind::Expr(e) = only("xs[a..b][0]\n") else {
        panic!()
    };
    let ExprKind::Index { obj, .. } = e.kind else {
        panic!("expected Index, got {:?}", e.kind)
    };
    assert!(matches!(obj.kind, ExprKind::Index { .. }));
}

#[test]
fn type_apply_turbofish_at_type() {
    // Multi-type-arg turbofish on the TYPE: `Pair[int, str].Mk(5)` parses as a Call whose
    // callee is a Field over a type-only `Index` carrying the parsed types.
    let StmtKind::Expr(e) = only("Pair[int, str].Mk(5)\n") else {
        panic!()
    };
    let ExprKind::Call { callee, args, .. } = e.kind else {
        panic!("expected Call, got {:?}", e.kind)
    };
    assert_eq!(args.len(), 1);
    let ExprKind::Field { obj, name, .. } = callee.kind else {
        panic!("expected Field callee, got {:?}", callee.kind)
    };
    assert_eq!(name, "Mk");
    let ExprKind::Index {
        obj: head,
        index: None,
        types: args,
    } = obj.kind
    else {
        panic!("expected a type-only Index obj, got {:?}", obj.kind)
    };
    assert!(matches!(&head.kind, ExprKind::Ident(n) if n == "Pair"));
    assert_eq!(args.len(), 2);

    // Single-type-arg in CALL position stays on the Index path (no comma to disambiguate):
    // `Box[int].Has(5)` → Call{ callee: Field{ obj: Index{Ident(Box), int}, Has } }.
    let StmtKind::Expr(e) = only("Box[int].Has(5)\n") else {
        panic!()
    };
    let ExprKind::Call { callee, .. } = e.kind else {
        panic!("expected Call, got {:?}", e.kind)
    };
    let ExprKind::Field { obj, name, .. } = callee.kind else {
        panic!("expected Field callee, got {:?}", callee.kind)
    };
    assert_eq!(name, "Has");
    assert!(matches!(obj.kind, ExprKind::Index { .. }));

    // Nullary value form (multi-arg): `Pair[int, str].Empty` → Field{ obj: Index, Empty }.
    let StmtKind::Expr(e) = only("Pair[int, str].Empty\n") else {
        panic!()
    };
    let ExprKind::Field { obj, name, .. } = e.kind else {
        panic!("expected Field, got {:?}", e.kind)
    };
    assert_eq!(name, "Empty");
    let ExprKind::Index {
        index: None,
        types: args,
        ..
    } = obj.kind
    else {
        panic!("expected a type-only Index obj, got {:?}", obj.kind)
    };
    assert_eq!(args.len(), 2);
}

/// TICKET-222: one bracket node keeps every reading that parses. The type reading uses the
/// real type grammar (tuple, fn, optional, nested generic); the expression reading is kept
/// only when it ends at the `]`; a non-name head never gets a type reading.
#[test]
fn bracket_keeps_both_readings() {
    let readings = |src: &str| {
        let StmtKind::Expr(e) = only(src) else {
            panic!("{src}: not an expression statement")
        };
        let ExprKind::Index { index, types, .. } = e.kind else {
            panic!("{src}: expected Index, got {:?}", e.kind)
        };
        (index.is_some(), types.len())
    };
    for src in [
        "g[(int, str)]\n",
        "g[fn(int) -> int]\n",
        "g[int?]\n",
        "g[Map[str, int]]\n",
        "lib.g[lib.Bx[int]]\n",
    ] {
        assert_eq!(readings(src).1, 1, "{src}: one type");
    }
    assert_eq!(readings("g[fn(int) -> int]\n"), (false, 1));
    assert_eq!(readings("g[(int, str)]\n"), (true, 1));
    assert_eq!(readings("xs[i]\n"), (true, 1));
    assert_eq!(readings("xs[i + 1]\n"), (true, 0));
    assert_eq!(readings("xs[0]\n"), (true, 0));
    assert_eq!(readings("f()[k]\n"), (true, 0));
    assert_eq!(readings("t.0[k]\n"), (true, 0));
    assert_eq!(readings("g[int, str]\n"), (false, 2));
    parse_err("xs[0, 1]\n");
    parse_err("xs[]\n");
}

#[test]
fn type_apply_value_form() {
    // TICKET-204/222: a multi-arg type application is a type-only `Index` in value position,
    // on an `Ident` or a `Field` head; `[types](` stays a call.
    let StmtKind::Expr(e) = only("pair[str, int]\n") else {
        panic!()
    };
    let ExprKind::Index {
        obj: head,
        index: None,
        types: args,
    } = e.kind
    else {
        panic!("expected a type-only Index, got {:?}", e.kind)
    };
    assert!(matches!(&head.kind, ExprKind::Ident(n) if n == "pair"));
    assert_eq!(args.len(), 2);

    let StmtKind::Expr(e) = only("lib.tri[str, int, bool]\n") else {
        panic!()
    };
    let ExprKind::Index {
        obj: head,
        index: None,
        types: args,
    } = e.kind
    else {
        panic!("expected a type-only Index, got {:?}", e.kind)
    };
    assert!(matches!(&head.kind, ExprKind::Field { name, .. } if name == "tri"));
    assert_eq!(args.len(), 3);

    let StmtKind::Expr(e) = only("pair[str, int](\"a\", 1)\n") else {
        panic!()
    };
    let ExprKind::Call { type_args, .. } = e.kind else {
        panic!("expected Call, got {:?}", e.kind)
    };
    assert_eq!(type_args.len(), 2);

    let StmtKind::Expr(e) = only("xs[i]\n") else {
        panic!()
    };
    assert!(matches!(e.kind, ExprKind::Index { .. }), "{:?}", e.kind);
}

#[test]
fn map_literal() {
    let StmtKind::Expr(e) = only("{}\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Map(entries) => assert_eq!(entries.len(), 0),
        other => panic!("{other:?}"),
    }
    let StmtKind::Expr(e) = only("{\"a\": 1, \"b\": 2}\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Map(entries) => {
            assert_eq!(entries.len(), 2);
            assert!(matches!(entries[0].0.kind, ExprKind::Str(_)));
            assert!(matches!(entries[0].1.kind, ExprKind::Int(1)));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn list_literal() {
    let StmtKind::Expr(e) = only("[1, 2, 3]\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::List(elems, _) => assert_eq!(elems.len(), 3),
        other => panic!("{other:?}"),
    }
}

#[test]
fn list_comprehension_with_guard() {
    let StmtKind::Expr(e) = only("[x * 2 for x in xs if x > 0]\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Comprehension {
            kind,
            key,
            elem,
            clauses,
        } => {
            assert_eq!(kind, CompKind::List);
            assert!(key.is_none());
            assert!(matches!(elem.kind, ExprKind::Binary { .. }));
            assert_eq!(clauses.len(), 1);
            assert_eq!(clauses[0].target.binder_names(), vec!["x".to_string()]);
            assert!(matches!(clauses[0].iter.kind, ExprKind::Ident(_)));
            assert_eq!(clauses[0].guards.len(), 1);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn list_comprehension_over_range_no_guard() {
    let StmtKind::Expr(e) = only("[x for x in 0..10]\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Comprehension { kind, clauses, .. } => {
            assert_eq!(kind, CompKind::List);
            assert_eq!(clauses.len(), 1);
            assert!(matches!(clauses[0].iter.kind, ExprKind::Range { .. }));
            assert!(clauses[0].guards.is_empty());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn set_comprehension() {
    let StmtKind::Expr(e) = only("{x for x in xs}\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Comprehension { kind, key, .. } => {
            assert_eq!(kind, CompKind::Set);
            assert!(key.is_none());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn map_comprehension_two_vars() {
    let StmtKind::Expr(e) = only("{k: v for k, v in m}\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Comprehension {
            kind,
            key,
            elem,
            clauses,
        } => {
            assert_eq!(kind, CompKind::Map);
            assert!(matches!(key.unwrap().kind, ExprKind::Ident(_)));
            assert!(matches!(elem.kind, ExprKind::Ident(_)));
            assert_eq!(clauses.len(), 1);
            assert_eq!(
                clauses[0].target.binder_names(),
                vec!["k".to_string(), "v".to_string()]
            );
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn two_for_clauses_parse() {
    let StmtKind::Expr(e) = only("[x for x in xs for y in ys]\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Comprehension { kind, clauses, .. } => {
            assert_eq!(kind, CompKind::List);
            assert_eq!(clauses.len(), 2);
            assert_eq!(clauses[0].target.binder_names(), vec!["x".to_string()]);
            assert!(clauses[0].guards.is_empty());
            assert_eq!(clauses[1].target.binder_names(), vec!["y".to_string()]);
            assert!(clauses[1].guards.is_empty());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn guard_after_nonfinal_clause_parses() {
    // The `if x > 0` binds to the FIRST clause (the one it follows), not globally.
    let StmtKind::Expr(e) = only("[x for x in xs if x > 0 for y in ys]\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Comprehension { clauses, .. } => {
            assert_eq!(clauses.len(), 2);
            assert_eq!(clauses[0].guards.len(), 1);
            assert!(clauses[1].guards.is_empty());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn chained_guards_on_one_clause_parse() {
    let StmtKind::Expr(e) = only("[x for x in xs if x > 0 if x < 10]\n") else {
        panic!()
    };
    match e.kind {
        ExprKind::Comprehension { clauses, .. } => {
            assert_eq!(clauses.len(), 1);
            assert_eq!(clauses[0].guards.len(), 2);
        }
        other => panic!("{other:?}"),
    }
}

/// The golden target: the whole touchstone program parses, with the expected top-level shape.
#[test]
fn parses_hello_example() {
    let src = include_str!("../../../examples/hello.chz");
    let module = parse(lexer::tokenize(src).unwrap()).expect("hello.chz should parse");
    // greet, Point, Shape, area, safe_div, main, then the top-level `main()` call
    assert_eq!(module.stmts.len(), 7);
    assert!(matches!(module.stmts[0].kind, StmtKind::Fn(_)));
    assert!(matches!(module.stmts[1].kind, StmtKind::Struct { .. }));
    assert!(matches!(module.stmts[2].kind, StmtKind::Enum { .. }));
    assert!(matches!(module.stmts[5].kind, StmtKind::Fn(_)));
    assert!(matches!(module.stmts[6].kind, StmtKind::Expr(_)));
}

#[test]
fn reports_error_with_location() {
    // `fn :` — missing function name (`fn (` opens a closure literal)
    let err = parse(lexer::tokenize("fn :\n").unwrap()).unwrap_err();
    assert!(err.message.contains("identifier"), "{}", err.message);
    assert_eq!(err.span.line, 1);
}

/// S2-3: `??` after an operand is two postfix `?`, not a coalesce with a missing right side.
#[test]
fn postfix_double_question_is_two_tries() {
    match only("y := oo(x)??\n") {
        StmtKind::Let { value, .. } => match value.kind {
            ExprKind::Try(inner) => assert!(matches!(inner.kind, ExprKind::Try(_))),
            other => panic!("expected Try(Try(..)), got {other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// S2-3: `?..` is postfix `?` then `..`.
#[test]
fn postfix_question_then_range() {
    match only("r := o(x)?..5\n") {
        StmtKind::Let { value, .. } => match value.kind {
            ExprKind::Range { start, .. } => {
                assert!(matches!(start.expect("start").kind, ExprKind::Try(_)))
            }
            other => panic!("expected Range, got {other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// S2-3: `a ?? b` with an operand on the right stays a coalesce.
#[test]
fn coalesce_with_right_operand_still_coalesces() {
    match only("y := a ?? b\n") {
        StmtKind::Let { value, .. } => {
            assert!(matches!(value.kind, ExprKind::NullCoalesce { .. }))
        }
        other => panic!("{other:?}"),
    }
}

/// S2-3 grid: what a run of `?` means in every position the parser reads one. A cell is the
/// source and either the count of each named node in the module's `Debug` text, or a fragment of
/// the lex or parse error. The hole cells pin that a hole's code starts with no operand before
/// it, whatever token closed the hole before it (`Lexer::hole_code`). The `? ??` cells pin that a
/// `?` keeps the reading of the token before it, and the glued-type cells pin that a `??` glued to
/// a char that starts no operand (`=`, `:`, `]`) is two `?`.
#[test]
fn question_run_position_grid() {
    type Want = Result<&'static [(&'static str, usize)], &'static str>;
    let try2: Want = Ok(&[("Try(", 2), ("NullCoalesce", 0)]);
    let wrap2: Want = Ok(&[("Wrap", 2), ("NullCoalesce", 0)]);
    let wrap3: Want = Ok(&[("Wrap", 3), ("NullCoalesce", 0)]);
    let coalesce: Want = Ok(&[("NullCoalesce", 1), ("Try(", 0)]);
    let opt2: Want = Ok(&[("Option", 2), ("Try(", 0), ("NullCoalesce", 0)]);
    let cells: &[(&str, Want)] = &[
        ("y := oo(x)??\n", try2),
        (
            "y := oo(x)?? - 1\n",
            Ok(&[("Try(", 2), ("NullCoalesce", 0), ("Sub", 1)]),
        ),
        ("y := ooo(x)???\n", Ok(&[("Try(", 3), ("NullCoalesce", 0)])),
        ("y := f(oo(x)??, 2)\n", try2),
        ("y := [oo(x)??]\n", try2),
        ("y := d[oo(x)??]\n", try2),
        ("y := {k: oo(x)??}\n", try2),
        ("y := \"{oo(x)??}\"\n", try2),
        (
            "y := oo(x)??.v\n",
            Ok(&[("Try(", 1), ("OptChain", 1), ("NullCoalesce", 0)]),
        ),
        ("r := o(x)?..5\n", Ok(&[("Try(", 1), ("Range", 1)])),
        (
            "r := p?.v?..3\n",
            Ok(&[("Try(", 1), ("OptChain", 1), ("Range", 1)]),
        ),
        ("y := a ?? b\n", coalesce),
        ("y := a??b\n", coalesce),
        ("y := (a\n    ?? b)\n", coalesce),
        ("y := a??-1\n", coalesce),
        (
            "y := a??!=b\n",
            Ok(&[("Try(", 2), ("NullCoalesce", 0), ("NotEq", 1)]),
        ),
        ("y := a ?? ??b\n", Ok(&[("NullCoalesce", 1), ("Wrap", 2)])),
        ("y := ??x\n", wrap2),
        ("y := ?? x\n", wrap2),
        ("y := ???x\n", wrap3),
        ("y := ? ??x\n", wrap3),
        ("y := ? ?? x\n", wrap3),
        ("y := x? ?? 1\n", Ok(&[("Try(", 1), ("NullCoalesce", 1)])),
        ("y := f(??x)\n", wrap2),
        ("y := \"{a}{??b}\"\n", wrap2),
        ("y := \"{a} {??b}\"\n", wrap2),
        ("y := \"{x}{?? b}\"\n", wrap2),
        ("y := \"{a:>{w}}{??b}\"\n", wrap2),
        ("y := v + \"{??b}\"\n", wrap2),
        ("v: int?? = None\n", opt2),
        ("v: int??=None\n", opt2),
        ("xs: List[int??]=[]\n", opt2),
        ("fn f() -> int??:\n    pass\n", opt2),
        (
            "fn f(a: int??) -> int??:\n    return a\n",
            Ok(&[("Option", 4), ("Try(", 0)]),
        ),
        (
            "match o:\n    ??v: print(v)\n",
            Ok(&[("Present", 2), ("NullCoalesce", 0)]),
        ),
        ("y := a ??b\n", Err("same spacing on both sides")),
        ("y := a?? b\n", Err("")),
        ("a??=b\n", Err("invalid assignment target")),
    ];
    let mut red = Vec::new();
    for (src, want) in cells {
        let got = match lexer::tokenize(src) {
            Ok(toks) => parse(toks).map(|m| format!("{m:?}")).map_err(|e| e.message),
            Err(e) => Err(e.message),
        };
        let ok = match (want, &got) {
            (Ok(counts), Ok(text)) => counts.iter().all(|(n, c)| text.matches(n).count() == *c),
            (Err(frag), Err(msg)) => msg.contains(frag),
            (Ok(_), Err(_)) | (Err(_), Ok(_)) => false,
        };
        if !ok {
            let got = got.map(|t| {
                let n = ["Try(", "Wrap", "NullCoalesce", "Option", "Present"];
                n.map(|k| format!("{k}{}", t.matches(k).count())).join(" ")
            });
            red.push(format!("{src:?}: want {want:?}, got {got:?}"));
        }
    }
    assert!(
        red.is_empty(),
        "{} of {} cells red:\n{}",
        red.len(),
        cells.len(),
        red.join("\n")
    );
}
