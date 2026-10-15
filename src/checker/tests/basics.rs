//! Checker tests: numbered basics: unknown type, arity, arithmetic, assignment, return inference,
//! nested fn, field, index, match, `?`.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== 2. unknown type =====

#[test]
fn unknown_type_annotation_rejected() {
    rejects("x: Widget = 5\n", "unknown type 'Widget'");
}

#[test]
fn unknown_param_type_rejected() {
    rejects(
        "fn f(a: Widget) -> int:\n    return 1\n",
        "unknown type 'Widget'",
    );
}

/// TICKET-079/TICKET-080 (b): an unknown type gets a near-miss suggestion from the type
/// namespace, like every other kind of miss already does.
#[test]
fn unknown_type_spans_the_type_name_and_suggests() {
    rejects_help(
        "x: Lst = [1]\n",
        "unknown type 'Lst'",
        "did you mean 'List'",
    );
}

/// TICKET-253: `range` is no type (the builtin is removed; the name stays reserved only to carry the
/// `range()` error), so a misspelt type must not be suggested `range`.
#[test]
fn an_unknown_type_is_never_suggested_the_removed_name_range() {
    let errs = check_src("x: rnge = 1\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unknown type 'rnge'")),
        "expected `unknown type 'rnge'`, got: {errs:?}"
    );
    assert!(
        errs.iter()
            .all(|e| !e.help.as_deref().is_some_and(|h| h.contains("'range'"))),
        "a help suggests the removed name `range`: {errs:?}"
    );
}

/// TICKET-079/TICKET-080 (b): the span carets the TYPE NAME, not column 1. `Strng` starts at
/// 1-based column 9 in `fn f(a: Strng) -> int:`.
#[test]
fn unknown_type_in_a_param_spans_the_type_name() {
    let errs = check_src("fn f(a: Strng) -> int:\n    return 1\n");
    let e = errs
        .iter()
        .find(|e| e.message.contains("unknown type 'Strng'"))
        .unwrap_or_else(|| panic!("expected an 'Strng' miss, got: {errs:?}"));
    assert_eq!(e.span.col, 9, "got: {e:?}");
}

// ===== 3. arity =====

#[test]
fn too_few_args_rejected() {
    rejects(
        "fn add(a: int, b: int) -> int:\n    return a + b\nx := add(1)\n",
        "expects 2 argument",
    );
}

#[test]
fn struct_ctor_arity_rejected() {
    rejects(
        "struct P:\n    x: int\n    y: int\np := P(1)\n",
        "expects 2 argument",
    );
}

#[test]
fn builtin_arity_rejected() {
    // free `len()` is gone — it is no longer a builtin, so it resolves as an unknown name.
    rejects("x := len([1, 2, 3])\n", "unknown name 'len'");
}

#[test]
fn len_not_reserved() {
    assert!(!is_reserved_name("len"));
}

#[test]
fn user_fn_len_ok() {
    // `len` is no longer reserved, so a user may declare a top-level `fn len`.
    ok("fn len(x: int) -> int:\n    return x\nprint(len(5))\n");
}

#[test]
fn bytes_len_method_ok() {
    ok("b := \"hi\".encode()\nn: int = b.len()\nprint(n)\n");
}

#[test]
fn correct_arity_ok() {
    ok("fn add(a: int, b: int) -> int:\n    return a + b\nx := add(1, 2)\n");
}

// ===== 4. not callable =====

#[test]
fn calling_a_number_rejected() {
    rejects("x := 5\ny := x(1)\n", "not callable");
}

// ===== 5. arithmetic =====

#[test]
fn string_minus_int_rejected() {
    rejects("x := \"a\" - 1\n", "cannot apply - to str and int");
}

#[test]
fn bool_times_int_rejected() {
    rejects("x := true * 2\n", "cannot apply * to bool and int");
}

#[test]
fn string_concat_ok() {
    ok("x := \"a\" + \"b\"\n");
}

#[test]
fn int_float_promotes_ok() {
    ok("x := 1 + 2.0\n");
}

// ===== 6. comparison =====

#[test]
fn comparing_bool_ordering_rejected() {
    rejects("x := true < false\n", "cannot compare bool and bool");
}

#[test]
fn numeric_comparison_ok() {
    ok("x := 1 < 2\ny := 1.5 >= 2\n");
}

// ===== 7. bool context =====

#[test]
fn if_condition_must_be_bool() {
    rejects("if 1:\n    x := 2\n", "if condition must be bool");
}

#[test]
fn while_condition_must_be_bool() {
    rejects("while \"x\":\n    y := 1\n", "while condition must be bool");
}

#[test]
fn and_requires_bool() {
    rejects("x := 1 and true\n", "logical operator expects bool");
}

// ===== 8. assignment =====

#[test]
fn typed_let_mismatch_rejected() {
    rejects(
        "x: int = \"s\"\n",
        "cannot assign str to variable of type int",
    );
}

#[test]
fn reassign_wrong_type_rejected() {
    rejects("x := 5\nx = \"s\"\n", "cannot assign str to int");
}

#[test]
fn assign_to_undeclared_rejected() {
    rejects("x = 5\n", "undeclared variable 'x'");
}

#[test]
fn plus_eq_on_bool_rejected() {
    rejects("x := true\nx += 1\n", "cannot apply + to bool and int");
}

#[test]
fn typed_let_ok() {
    ok("x: int = 5\nx += 1\n");
}

// ===== 8b. compound assignment must not widen int -> float (gap #9) =====

#[test]
fn plus_eq_float_into_int_var_rejected() {
    rejects("x: int = 5\nx += 1.5\n", "cannot assign float to int");
}

#[test]
fn minus_eq_float_into_int_var_rejected() {
    rejects("x: int = 5\nx -= 1.5\n", "cannot assign float to int");
}

#[test]
fn plus_eq_float_into_int_index_rejected() {
    rejects(
        "xs := [1, 2, 3]\nxs[0] += 1.5\n",
        "cannot assign float to int",
    );
}

#[test]
fn plus_eq_float_into_int_field_rejected() {
    rejects(
        "struct P:\n    x: int\np := P(1)\np.x += 1.5\n",
        "cannot assign float to int",
    );
}

#[test]
fn plus_eq_int_into_float_var_ok() {
    // widening the *other* way (int into a float slot) stays allowed.
    ok("f: float = 1.0\nf += 1\n");
}

#[test]
fn plus_eq_float_into_float_ok() {
    ok("f: float = 1.0\nf += 1.5\n");
}

// ===== 9. return =====

#[test]
fn return_wrong_type_rejected() {
    rejects(
        "fn f() -> int:\n    return \"s\"\n",
        "expected return type int, found str",
    );
}

#[test]
fn missing_return_value_rejected() {
    rejects(
        "fn f() -> int:\n    return\n",
        "expected a return value of type int",
    );
}

#[test]
fn return_matches_signature_ok() {
    ok("fn f(a: int) -> int:\n    return a + 1\n");
}

// ===== 9b. return-type inference (un-annotated `-> T`) =====

#[test]
fn inferred_return_type_used_as_int() {
    // No `-> T`: the body's `return 5` makes f infer `int`, so `x + 1` type-checks.
    ok("fn f() -> int:\n    return 5\nx := f()\ny := x + 1\n");
}

#[test]
fn inferred_return_from_expression() {
    rejects(
        "fn add(a: int, b: int):\n    return a + b\nx := add(1, 2)\ny := x + 1\n",
        "declares no return type, so it returns nothing",
    );
}

#[test]
fn void_preserved_when_no_value_return() {
    // No value-return → infers `nil`; binding the void result to a value (`x := log(...)`) is now
    // rejected up-front (Part 2: nil in value position), before it can flow into an arithmetic op.
    rejects(
        "fn log(m: str):\n    print(m)\nx := log(\"h\")\ny := x + 1\n",
        "no value (None)",
    );
}

#[test]
fn inferred_return_in_if_branch() {
    rejects(
        "fn f(c: bool):\n    if c:\n        return 1\n    return 2\nx := f(true)\ny := x + 1\n",
        "declares no return type, so it returns nothing",
    );
}

#[test]
fn inferred_return_from_accumulator_local() {
    ok(
        "fn sum(xs: List[int]) -> int:\n    total := 0\n    for x in xs:\n        total += x\n    return total\nn := sum([1, 2, 3])\nm := n + 1\n",
    );
}

#[test]
fn inferred_return_recursive() {
    rejects(
        "fn fib(n: int): if n < 2: n else: fib(n - 1) + fib(n - 2)\nx := fib(10)\ny := x + 1\n",
        "a call in its body has no known type yet",
    );
}

#[test]
fn inferred_result_return() {
    rejects(
        "fn d(a: int, b: int):\n    if b == 0:\n        return !(\"divide by zero\")\n    return ?(a / b)\nmatch d(10, 2):\n    ?v: print(\"got {v}\")\n    !e: print(e)\n",
        "declares no return type, so it returns nothing",
    );
}

#[test]
fn inferred_return_feeds_typed_let_mismatch() {
    // The inferred `int` return is checked against an explicit `let` annotation.
    rejects(
        "fn f() -> int:\n    return 5\nx: str = f()\n",
        "cannot assign int to variable of type str",
    );
}

#[test]
fn inferred_list_return() {
    rejects(
        "fn mk():\n    return [1, 2, 3]\nxs := mk()\ny := xs[0] + 1\n",
        "declares no return type, so it returns nothing",
    );
}

#[test]
fn inferred_struct_return() {
    rejects(
        "struct P:\n    x: int\nfn mk():\n    return P(1)\np := mk()\nq := p.x + 1\n",
        "declares no return type, so it returns nothing",
    );
}

#[test]
fn inferred_recursion_only_with_annotation_ok() {
    // An explicit `-> int` annotation bypasses inference entirely (the cleanest way to type a
    // self-recursive-only function).
    ok("fn loopy(n: int) -> int:\n    return loopy(n - 1)\n");
}

#[test]
fn non_recursive_unknown_return_not_falsely_rejected() {
    // Regression: a NON-recursive un-annotated fn whose return infers `Unknown` for a reason
    // unrelated to recursion must NOT be rejected by the recursive-return inference. PART A now
    // requires the empty `x := []` to be annotated, so the annotated form drives this: `x[0]` is a
    // concrete `int`, the return infers `int`, and the recursive-return fixpoint must not regress.
    ok("fn f() -> int:\n    x: List[int] = []\n    return x[0]\nprint(\"ok\")\n");
    // The un-annotated empty itself is the sibling producer's domain — it is now its own error.
    rejects(
        "fn f() -> int:\n    x := []\n    return x[0]\nf()\n",
        "cannot infer the",
    );
}

#[test]
fn errored_body_unknown_return_reports_once() {
    // Regression: a fn whose body has a real error (undefined name) infers `Unknown`; the fixpoint
    // change must not pile a spurious "cannot infer return type" on top of the genuine error.
    let errs = check_src("fn f() -> int:\n    return undefined_fn()\n");
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert!(
        errs[0].message.contains("unknown name"),
        "got: {:?}",
        errs[0]
    );
}

#[test]
fn fact_still_infers_int() {
    // A self-recursive inline body is never typed from its own call: it asks for `->`.
    rejects(
        "fn fact(n: int): if n <= 1: 1 else: n * fact(n - 1)\nx := fact(5)\ny := x + 1\n",
        "a call in its body has no known type yet",
    );
}

// ===== 9c. multi-branch return-type inference: JOIN merge + finalize (leak fix) =====

#[test]
fn infer_ok_err_branches_merge_slotwise() {
    // (a) Ok/Err sibling branches merge the T-slot: Ok("h")=Result[str,?] ⊔ Err("a")=Result[?,str]
    // → Result[str, Error] (the E-slot is NOT pinned from the Err payload — an inferred error slot
    // always defaults to `Error`). So `res()?` is str and `y: int = x` must ERROR.
    entry_rejects(
        "fn res() -> str!:\n    if false:\n        return !\"a\"\n    return ?\"h\"\nfn caller() -> str!:\n    x := res()?\n    y: int = x\n    return ?x\nfn main():\n    pass\n",
        "cannot assign str to variable of type int",
    );
}

#[test]
fn infer_ok_only_defaults_error_e() {
    // (b) `fn ok(): return Ok(5)` must finalize to Result[int, Error] (E defaults to the Error
    // protocol, matching `T!`). Propagating `ok()?` into a `-> int!DbErr` fn must then mismatch
    // (Error vs DbErr) exactly like the annotated `-> int!` version. Today ok() leaks
    // Result[int, Unknown] so `?` launders into DbErr with no error.
    entry_rejects(
        "struct DbErr:\n    code: int\n    fn message(self) -> str:\n        return \"db\"\nfn ok() -> int!:\n    return ?5\nfn caller() -> int!DbErr:\n    x := ok()?\n    return ?x\nfn main():\n    pass\n",
        "propagates error Error",
    );
}

#[test]
fn infer_err_only_uninferable_errors() {
    // (c) a block fn with no `->` returning only `!"x"`: the `!` value has no type to take.
    entry_rejects(
        "fn err():\n    return !\"x\"\nfn main():\n    pass\n",
        "a `!` value needs its type from an annotation",
    );
}

#[test]
fn infer_concurrency_box_with_inferable_element_ok() {
    // NEIGHBOR: a box whose element IS inferable from the constructor value stays legal (the fix
    // only flags a residual Unknown, never a resolved element).
    entry_ok(
        "import std.concurrency\nfn f() -> Shared[List[int]]:\n    return Shared([1])\nfn main():\n    s := f()\n    a: List[int] = s.get()\n    print(a)\n",
    );
}

#[test]
fn multibranch_return_ok_err_no_error() {
    // A block fn with no `->` returns nothing: no `T!E` type is joined from its `return`s.
    entry_rejects(
        "fn f(c: bool):\n    if c:\n        return ?5\n    return !\"x\"\nfn main():\n    match f(true):\n        ?v: print(v)\n        !e: print(e)\n",
        "declares no return type, so it returns nothing",
    );
}

#[test]
fn multibranch_return_empty_and_nonempty_list_no_error() {
    // NEIGHBOR: [] + [1, 2] → List[int], no error (empty element filled from the non-empty sibling).
    entry_ok(
        "fn f(c: bool) -> List[int]:\n    if c:\n        return []\n    return [1, 2]\nfn main():\n    xs := f(true)\n    print(xs)\n",
    );
}

#[test]
fn multibranch_void_stays_nil_no_error() {
    // NEIGHBOR: a fn with no value-return stays void/nil (nil != Unknown → no "cannot infer").
    entry_ok("fn f():\n    print(1)\nfn main():\n    f()\n");
}

#[test]
fn infer_ok_err_mixed_defaults_e_to_error() {
    // A declared `-> str!` has the error type `Error`, whatever the `!e` payload. Proof: propagating `res()?` into a `-> str!DbErr` fn hits the
    // Error-vs-DbErr mismatch exactly like the annotated `-> str!` version would.
    entry_rejects(
        &format!(
            "{DBERR}fn res(c: bool) -> str!:\n    if c:\n        return !\"a\"\n    return ?\"h\"\nfn caller() -> str!DbErr:\n    x := res(true)?\n    return ?x\nfn main():\n    pass\n"
        ),
        "propagates error Error",
    );
}

#[test]
fn infer_distinct_err_payloads_no_conflict() {
    // Two branches with DIFFERENT Err payload types no longer conflict on the E-slot (both finalize
    // to `Error`); the Ok branch pins T=int. `e` binds as `Error` → `e.message()` is available.
    entry_ok(
        "struct EA:\n    a: int\n    fn message(self) -> str:\n        return \"EA\"\nstruct EB:\n    b: int\n    fn message(self) -> str:\n        return \"EB\"\nfn f(k: int) -> int!:\n    if k == 0:\n        return !EA(1)\n    if k == 1:\n        return !EB(2)\n    return ?5\nfn main():\n    match f(2):\n        ?v: print(v)\n        !e: print(e.message())\n",
    );
}

#[test]
fn infer_expr_non_error_payload_not_laundered() {
    // SOUNDNESS (adversarial-review): the if/match-EXPRESSION E-default must NOT force a concrete
    // NON-Error payload to `Error` — that path has no post-hoc assignability re-check, so laundering
    // `MyErr` (no `message`) into the `Error` existential would make `e.message()` check-pass then
    // fault at runtime. E is kept concrete → the method-call check rejects it at check time.
    entry_rejects(
        "struct MyErr:\n    code: int\nfn foo(c: bool) -> int!MyErr:\n    if c:\n        return !MyErr(1)\n    return ?5\nfn main():\n    c := true\n    x := if c: foo(true) else: foo(false)\n    match x:\n        ?v: print(v)\n        !e: print(e.message())\n",
        "no method 'message'",
    );
    // A bare non-Error scalar payload (`Err(42)`) is likewise preserved as `int`, not laundered.
    entry_rejects(
        "fn main():\n    c := true\n    x := if c: !42 else: !43\n    match x:\n        ?v: print(v)\n        !e: print(e.message())\n",
        "a `!` value needs its type",
    );
}

// TICKET-026 -- W8-22. THE NAME RECORDS THE BUG, NOT THE ASSERTION. A caught `Error` carries no
// line/col/file today, so `e.line()` is rejected with `type Error has no method 'line'`. This test
// asserts the FIXED behaviour, so it is RED until the three location accessors land. Do not rename
// it: the ticket's `test_file` selects it by this exact string. Both helpers are called because
// `ok()` keys types bare while `entry_ok()` keys them module-prefixed, so an `ok()`-only test can
// pass while the CLI still rejects.
#[test]
fn caught_error_has_no_location_accessor() {
    let src = "fn boom() -> int!:\n    xs := [1]\n    return ?xs[9]\nfn main():\n    r := recover: boom()\n    match r:\n        ?v: print(v)\n        !e: print(e.line())\n";
    ok(src);
    entry_ok(src);

    let src_col = "fn boom() -> int!:\n    xs := [1]\n    return ?xs[9]\nfn main():\n    r := recover: boom()\n    match r:\n        ?v: print(v)\n        !e: print(e.col())\n";
    entry_ok(src_col);

    let src_file = "fn boom() -> int!:\n    xs := [1]\n    return ?xs[9]\nfn main():\n    r := recover: boom()\n    match r:\n        ?v: print(v)\n        !e: print(e.file())\n";
    entry_ok(src_file);

    // `line()` returns `int?`: a declared `int?` binding must accept it.
    let src_typed = "fn boom() -> int!:\n    xs := [1]\n    return ?xs[9]\nfn main():\n    r := recover: boom()\n    match r:\n        ?v: print(v)\n        !e:\n            x: int? = e.line()\n            print(x)\n";
    entry_ok(src_typed);

    // The accessors take 0 arguments; arity is still enforced.
    let src_arity = "fn boom() -> int!:\n    xs := [1]\n    return ?xs[9]\nfn main():\n    r := recover: boom()\n    match r:\n        ?v: print(v)\n        !e: print(e.line(1))\n";
    entry_rejects(src_arity, "expects 0 argument(s)");
}

#[test]
fn explicit_return_result_keeps_concrete_e() {
    // REGRESSION GUARD: the E-default is inference-ONLY. An EXPLICIT `-> Result[str, str]` annotation
    // (resolved by `resolve_type`, bypassing inference) keeps the concrete `str` error slot — so
    // matching `Err(e)` gives `e: str` and `e.trim()` (a str method, not on `Error`) type-checks.
    entry_ok(
        "fn res(c: bool) -> str!str:\n    if c:\n        return !\"a\"\n    return ?\"h\"\nfn main():\n    match res(true):\n        ?v: print(v)\n        !e: print(e.trim())\n",
    );
}

// A `struct DbErr` satisfying the `Error` protocol, used by the if/match-expr E-default tests below.
const DBERR: &str =
    "struct DbErr:\n    msg: str\n    fn message(self) -> str:\n        return self.msg\n";

#[test]
fn if_expr_all_ok_defaults_result_e_to_error() {
    // An unannotated all-`Ok` if-EXPRESSION folds to `Result[int, Unknown]` (no `Err` branch pins E);
    // the E-slot must default to the `Error` protocol, not leak `Unknown`. Proof: propagating `x?`
    // into a `-> int!DbErr` fn now hits the `'?' propagates error Error, but ... DbErr` mismatch that
    // a leaked `Unknown` (compatible with anything) would silently pass.
    entry_rejects(
        &format!(
            "{DBERR}fn g() -> int!DbErr:\n    x: int! = if true: 1 else: 2\n    return x?\nfn main():\n    pass\n"
        ),
        "propagates error Error",
    );
}

#[test]
fn match_expr_all_ok_defaults_result_e_to_error() {
    // Same E-default on the match-EXPRESSION surface.
    entry_rejects(
        &format!(
            "{DBERR}fn g() -> int!DbErr:\n    k := 1\n    x: int! = match k:\n        1: 1\n        _: 2\n    return x?\nfn main():\n    pass\n"
        ),
        "propagates error Error",
    );
}

#[test]
fn if_expr_edefault_does_not_over_reject_error_str_merge() {
    // MUST-NOT-BREAK (the regression the auto-task's stricter merge introduced): mixing a
    // `Result[_, Error]` branch with a `Result[_, str]` branch stays ACCEPTED — `str` conforms to the
    // `Error` protocol, and `unify_branch`'s `compatible`-based fold is left untouched by the
    // E-default (which only fills a top-level `Unknown` E-slot, never re-checks branch acceptance).
    entry_ok(
        "fn get_err() -> int!:\n    return !\"boom\"\nfn main():\n    c := true\n    x: int! = if c: get_err() else: !\"other\"\n    print(\"ok\")\n",
    );
}

#[test]
fn if_expr_edefault_keeps_binding_leniency_and_neighbors() {
    // The E-default must NOT import return-position strictness: an un-inferable if-expr bound via `:=`
    // stays as lenient as the equivalent direct binding.
    entry_rejects(
        "fn main():\n    x := if true: None else: None\n    print(x)\n",
        "cannot infer the",
    ); // like `x := None`
    // T-merge across Ok/Err still works; annotated form still works.
    entry_ok("fn main():\n    x := if true: ?3 else: None\n    print(x)\n");
    entry_ok("fn main():\n    x: str!str = if true: ?\"a\" else: !\"b\"\n    print(x)\n");
    // D3: an int-CONST branch beside a float-CONST branch is a conflict, not a widen.
    entry_rejects(
        "fn main():\n    x := if true: 3 else: 4.0\n    print(x)\n",
        "write 1.0",
    );
}

#[test]
fn d3_multibranch_annotated_float_rejects_int() {
    // D3: the ANNOTATED-return path does not widen either (`-> float: return 3` is an error).
    entry_rejects(
        "fn f() -> float:\n    return 3\nfn main():\n    print(f())\n",
        "write 1.0",
    );
    entry_ok("fn f() -> float:\n    return 3.0\nfn main():\n    print(f())\n");
}

#[test]
fn multibranch_map_hof_loopback_intact() {
    // NEIGHBOR: the proto.rs return-only HOF loop-back still infers `map`'s closure return.
    entry_ok("fn main():\n    ys := [1, 2, 3].map(fn(x): x * 2)\n    print(ys)\n");
}

#[test]
fn closure_free_uninferable_errors() {
    // CLOSURE: a genuinely-free closure literal whose body is un-inferable errors on finalize.
    entry_rejects(
        "fn main():\n    f := fn(): !\"x\"\n    print(f)\n",
        "a `!` value needs its type from an annotation",
    );
}

#[test]
fn closure_free_ok_defaults_error_e() {
    // CLOSURE: a free lambda writes its `T!E` type; `-> int!` has the error type `Error`.
    entry_ok(
        "fn main():\n    f := fn() -> int!: ?5\n    x := f()\n    match x:\n        ?v: print(v)\n        !e: print(e.message())\n",
    );
}

#[test]
fn inferred_divergent_returns_accepted_when_annotated_protocol() {
    // Divergent concrete returns are the user's job to annotate: an explicit `-> Shape` protocol
    // existential accepts struct returns that each satisfy the protocol (no union types). Annotated
    // fns bypass inference, so the fixpoint must not break this.
    ok(
        "protocol Shape:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int:\n        return self.s * self.s\nstruct Ci:\n    r: int\n    fn area(self) -> int:\n        return 3 * self.r * self.r\nfn pick(c: bool) -> Shape:\n    if c:\n        return Sq(2)\n    return Ci(1)\n",
    );
}

#[test]
fn inferred_nested_fn_does_not_pollute_outer() {
    // A nested fn whose name collides with a top-level fn must not feed the outer inference:
    // `outer` infers `int` from its OWN `return 42`, so `x + 1` type-checks.
    ok(
        "fn helper() -> str:\n    return \"top\"\nfn outer(c: bool) -> int:\n    fn helper() -> str:\n        return \"nested\"\n    return 42\nx := outer(true)\ny := x + 1\n",
    );
}

// ----- Nested `fn` decls are first-class local functions (lexical nearest-scope, body-checked,
// recursive, uniform by-reference capture). These lock the checker half of that behavior on the
// real entry/graph path. -----

/// Symptom 1 — a nested `fn f(x:int)` shadowing a top-level `fn f()` must resolve NEAREST-scope:
/// a 0-arg call `f()` inside its scope is a CHECK-TIME arity error (was validated against the
/// global 0-arg `f` → check-OK then run-fault "expects 1 argument, got 0").
#[test]
fn nested_fn_shadows_global_arity_checked() {
    entry_rejects(
        "fn f():\n    print(\"g\")\nfn outer():\n    fn f(x: int):\n        print(x + 1)\n    f()\nfn main():\n    outer()\nmain()\n",
        "'closure' expects",
    );
}

/// Symptom 1 (positive) — the same shadowing nested `fn f(x:int)` called CORRECTLY (`f(5)`) checks.
#[test]
fn nested_fn_shadows_global_called_right_ok() {
    entry_ok(
        "fn f():\n    print(\"g\")\nfn outer():\n    fn f(x: int):\n        print(x + 1)\n    f(5)\nfn main():\n    outer()\nmain()\n",
    );
}

/// Symptom 2 — a no-collision nested fn that is called resolves to itself (was a false
/// `unknown name`).
#[test]
fn nested_fn_no_collision_call_ok() {
    entry_ok(
        "fn outer():\n    fn helper(x: int) -> int:\n        return x + 1\n    print(helper(4))\nfn main():\n    outer()\nmain()\n",
    );
}

/// Symptom 3 — a no-collision nested fn's BODY is type-checked: a wrong-typed return is rejected
/// (was never checked → check-OK).
#[test]
fn nested_fn_body_return_type_checked() {
    entry_rejects(
        "fn outer():\n    fn bad() -> int:\n        return \"x\"\n    print(bad())\nfn main():\n    outer()\nmain()\n",
        "expected return type int, found str",
    );
}

/// Recursion — a nested fn may call itself (letrec via cell); the self-call type-checks against the
/// nested sig.
#[test]
fn nested_fn_recursion_type_checks() {
    entry_ok(
        "fn outer() -> int:\n    fn fact(n: int) -> int:\n        if n <= 1:\n            return 1\n        return n * fact(n - 1)\n    return fact(5)\nfn main():\n    print(outer())\nmain()\n",
    );
}

/// Mutual recursion between SIBLING nested fns is OUT OF SCOPE for v1: `a` referencing a
/// later-declared `b` (no global `b`) is a CLEAN forward-reference error, not check-OK/run-fault or
/// a host panic.
#[test]
fn nested_fn_mutual_recursion_clean_reject() {
    entry_rejects(
        "fn outer() -> int:\n    fn a() -> int:\n        return b()\n    fn b() -> int:\n        return 1\n    return a()\nfn main():\n    print(outer())\nmain()\n",
        "unknown name 'b'",
    );
}

/// Nested GENERIC fns are OUT OF SCOPE for v1: a clean reject, not a panic or a silent accept.
#[test]
fn nested_generic_fn_clean_reject() {
    entry_rejects(
        "fn outer():\n    fn id[T](x: T) -> T:\n        return x\n    print(id(5))\nfn main():\n    outer()\nmain()\n",
        "nested generic functions are not supported",
    );
}

/// A nested fn named after a RESERVED builtin/ctor (`print`, `range`, `int`, `List`, `Channel`, …)
/// is a shadowing local (TICKET-180 deleted the veto that rejected it: the backend now reads the
/// checker's recorded resolution, so the local wins at runtime too; the grid in
/// `tests/name_resolution_grid.rs` runs these shapes).
#[test]
fn nested_fn_shadows_reserved_builtin_accepted() {
    // `fn print(...)` inside a wrapper: checker typed `print(5)` as the nested int fn, VM ran the
    // builtin print → `v` nil → run-fault. Now the nested fn wins.
    entry_ok(
        "fn wrapper() -> int:\n    fn print(x: int) -> int:\n        return x + 1\n    return print(5)\nv := wrapper()\nprint(v)\n",
    );
    // `fn Set(...)` — same family, silent wrong value on base.
    entry_ok(
        "fn wrapper() -> int:\n    fn Set(x: int) -> int:\n        return x\n    return Set(5)\nprint(wrapper())\n",
    );
}

/// A nested fn named after a same-module STRUCT constructor is accepted as a local (TICKET-180).
#[test]
fn nested_fn_shadows_struct_ctor_accepted() {
    entry_ok(
        "struct P:\n    x: int\nfn outer() -> int:\n    fn P() -> int:\n        return 99\n    return P()\nprint(outer())\n",
    );
}

/// TICKET-228 (D6) — a nested fn may still be DECLARED with a removed carrier name, but the name is
/// reserved in bare-use position, so calling it bare gets the removal message.
#[test]
fn nested_fn_named_after_a_removed_name_cannot_be_called_bare() {
    let src = "fn outer() -> int:\n    fn Ok() -> int:\n        return 99\n    return Ok()\nprint(outer())\n";
    let err = parser::parse(lexer::tokenize(src).unwrap()).unwrap_err();
    assert_eq!(err.span.line, 4, "{err}");
    assert!(err.message.contains("`Ok(x)` is removed"), "{err}");
}

// SOUNDNESS: an inferred (un-annotated) struct method return must FLOW to call sites through the
// build_graph/check_graph path (module-prefixed keys), not just the single-module bare-key path.
// Pre-fix the single-module `inferred_method_return` above passes while the CLI/entry path silently
// accepts `int` assigned to `str` (the bare-key vs module-key divergence).
#[test]
fn inferred_struct_method_return_flows_to_callsite() {
    entry_rejects(
        "struct P:\n    x: int\n    fn val(self) -> int:\n        return 5\nfn main():\n    s: str = P(3).val()\n    print(s)\nmain()\n",
        "cannot assign int to variable of type str",
    );
}

#[test]
fn inferred_struct_method_return_correct_site_ok() {
    // BOUNDARY: an inferred method return used at a correctly-typed site must still compile.
    entry_ok(
        "struct P:\n    x: int\n    fn val(self) -> int:\n        return 5\nfn main():\n    n: int = P(3).val()\n    print(n)\nmain()\n",
    );
}

#[test]
fn explicit_struct_method_return_flows_to_callsite() {
    // BOUNDARY: an EXPLICIT annotation must still flow (already worked) on the entry path.
    entry_rejects(
        "struct P:\n    x: int\n    fn val(self) -> int:\n        return 5\nfn main():\n    s: str = P(3).val()\n    print(s)\nmain()\n",
        "cannot assign int to variable of type str",
    );
}

#[test]
fn struct_method_body_is_typechecked() {
    // DISCOVERED HOLE: in the build_graph path struct method bodies were entirely UNCHECKED
    // (the pass-2 guard read the bare key, missing the module-keyed slot), so a body type error
    // like `y: str = self.x` was silently accepted.
    entry_rejects(
        "struct P:\n    x: int\n    fn val(self) -> int:\n        y: str = self.x\n        return 5\nfn main():\n    print(\"hi\")\nmain()\n",
        "cannot assign int to variable of type str",
    );
}

#[test]
fn struct_method_body_correct_ok() {
    // BOUNDARY control for the body-check fix: a correct struct method body still compiles.
    entry_ok(
        "struct P:\n    x: int\n    fn val(self) -> int:\n        y: int = self.x\n        return y\nfn main():\n    print(\"hi\")\nmain()\n",
    );
}

#[test]
fn inferred_struct_compare_rejected_for_comparable() {
    // An inferred `compare(self,o)` body yielding bool must be REJECTED where Comparable (needs
    // `-> int`) is required (the `<` operator), exactly like an explicit `-> bool`.
    entry_rejects(
        "struct P:\n    x: int\n    fn compare(self, other: P) -> bool:\n        return self.x < other.x\n    fn eq(self, other: P) -> bool:\n        return self.x == other.x\nfn main():\n    a := P(1)\n    b := P(2)\n    c := a < b\n    print(c)\nmain()\n",
        "compare",
    );
}

#[test]
fn inferred_compare_generic_bound_rejected() {
    // A generic bound `[T: Comparable]` over a struct whose `compare` infers bool must reject at
    // check, not fault later.
    entry_rejects(
        "struct P:\n    x: int\n    fn compare(self, other: P) -> bool:\n        return self.x < other.x\n    fn eq(self, other: P) -> bool:\n        return self.x == other.x\nfn cmp[T: Comparable](a: T, b: T) -> int:\n    return a.compare(b)\nfn main():\n    print(cmp(P(1), P(2)))\nmain()\n",
        "Comparable",
    );
}

#[test]
fn inferred_enum_method_return_flows_to_callsite() {
    // Enum methods have the same hole: an un-annotated `fn val(self) -> int: return 5` returns int, which
    // must not be silently assignable to a str slot.
    entry_rejects(
        "enum Color:\n    Red\n    Blue\n    fn val(self) -> int:\n        return 5\nfn main():\n    s: str = Color.Red.val()\n    print(s)\nmain()\n",
        "cannot assign int to variable of type str",
    );
}

#[test]
fn inferred_enum_method_return_correct_site_ok() {
    entry_ok(
        "enum Color:\n    Red\n    Blue\n    fn val(self) -> int:\n        return 5\nfn main():\n    n: int = Color.Red.val()\n    print(n)\nmain()\n",
    );
}

#[test]
fn recursive_inferred_struct_method_no_spurious_error() {
    // BOUNDARY: a recursive un-annotated method must still infer `int` via the fixpoint and not
    // start spuriously erroring.
    entry_ok(
        "struct P:\n    x: int\n    fn f(self, c: bool) -> int:\n        if c:\n            return self.f(false)\n        return 0\nfn main():\n    n: int = P(1).f(true)\n    print(n)\nmain()\n",
    );
}

#[test]
fn repeated_none_in_tuple_pattern_not_duplicate_binder() {
    // A nullary variant (`None`) binds nothing, so `(None, None, None)` is NOT a duplicate binding.
    // Pre-fix the duplicate-binder pre-pass naively counted each `None` ident as a binder. (This
    // pattern only got exercised in struct method bodies, which were unchecked in the entry path.)
    entry_ok(
        "fn slice(a: int? = None, b: int? = None, c: int? = None) -> int:\n    match (a, b, c):\n        (None, None, None): return 0\n        _: return 1\nfn main():\n    print(slice())\nmain()\n",
    );
}

// ===== 9b'. struct method calls — the receiver `self` is implicit, not an explicit argument =====

const BOX: &str = "struct Box:\n    v: int\n    fn get(self) -> int:\n        return self.v\n    fn add(self, k: int) -> int:\n        return self.v + k\n";

#[test]
fn method_call_binds_self_implicitly() {
    // `b.get()` passes zero explicit args; `self` is bound from the receiver, not counted.
    ok(&format!("{BOX}b := Box(5)\nx := b.get()\ny := x + 1\n"));
}

#[test]
fn method_call_with_args_ok() {
    ok(&format!("{BOX}b := Box(5)\nx := b.add(3)\ny := x + 1\n"));
}

#[test]
fn method_call_wrong_arity_rejected() {
    rejects(
        &format!("{BOX}b := Box(5)\nx := b.add()\n"),
        "expects 1 argument",
    );
}

#[test]
fn method_call_wrong_arg_type_rejected() {
    rejects(
        &format!("{BOX}b := Box(5)\nx := b.add(\"s\")\n"),
        "expected int",
    );
}

#[test]
fn method_call_too_many_args_rejected() {
    rejects(
        &format!("{BOX}b := Box(5)\nx := b.get(1)\n"),
        "expects 0 argument",
    );
}

#[test]
fn method_without_receiver_param_rejected() {
    // A method with no params is a STATIC method (no `self`); calling it on an instance must be
    // rejected at check time — it is reached only as `Box.ping()`. (Otherwise the runtime would error
    // "expects 0 argument(s), got 1" since the instance-call path would prepend the receiver.)
    rejects(
        "struct Box:\n    v: int\n    fn ping():\n        print(\"x\")\nb := Box(5)\nb.ping()\n",
        "is a static method",
    );
}

#[test]
fn method_calls_another_method_via_self() {
    // The motivating case: `self.dbl()` inside a method body — a `self`-method call with 0 args.
    ok(
        "struct Box:\n    v: int\n    fn dbl(self) -> int:\n        return self.v * 2\n    fn quad(self) -> int:\n        return self.dbl() + self.dbl()\n",
    );
}

#[test]
fn method_call_multi_arg_arity() {
    let src = "struct C:\n    v: int\n    fn f(self, a: int, b: int) -> int:\n        return self.v + a + b\n";
    ok(&format!("{src}c := C(1)\nx := c.f(2, 3)\n"));
    rejects(
        &format!("{src}c := C(1)\nx := c.f(2)\n"),
        "expects 2 argument",
    );
}

#[test]
fn static_method_first_param_not_self_is_static() {
    // The "no self ⇒ static" rule: a method whose first param is NOT named `self` is a STATIC
    // method — it is NOT an instance method with a positionally-bound receiver. So `Point.getx(p)`
    // is the (static) call shape; `p.getx()` (the old positional-receiver convention) is now illegal.
    ok(
        "struct Point:\n    x: int\n    fn getx(p: Point) -> int:\n        return p.x\np := Point(7)\nn := Point.getx(p)\nm := n + 1\n",
    );
    rejects(
        "struct Point:\n    x: int\n    fn getx(p: Point) -> int:\n        return p.x\np := Point(7)\nn := p.getx()\n",
        "is a static method",
    );
}

// ===== 9c. T? / T! type shorthand (sugar for Option[T] / Result[T]) =====

#[test]
fn type_shorthand_checks_like_long_form() {
    // `int?` (param) and `int!` (return) desugar to Option[int] / Result[int].
    ok(
        "fn f(x: int?) -> int!:\n    match x:\n        ?v: return ?v\n        None: return !\"none\"\n",
    );
}

#[test]
fn optional_shorthand_accepts_some_and_none() {
    ok("x: int? = ?1\ny: int? = None\n");
}

#[test]
fn optional_shorthand_wraps_a_bare_value() {
    ok("x: int? = 5\n");
}

// ===== 9d. expression-valued match / if (Part 3) =====

#[test]
fn match_expression_unifies_arms() {
    ok("s := ?5\nx := match s:\n    ?v: v\n    None: 0\ny := x + 1\n");
}

#[test]
fn match_expression_incompatible_arms_rejected() {
    rejects(
        "s := ?5\nx := match s:\n    ?v: v\n    None: \"z\"\n",
        "incompatible types",
    );
}

#[test]
fn match_expression_nonexhaustive_rejected() {
    rejects("s := ?5\nx := match s:\n    ?v: v\n", "non-exhaustive");
}

#[test]
fn if_expression_unifies_branches() {
    ok("x := if true: 1 else: 2\ny := x + 1\n");
}

#[test]
fn if_expression_incompatible_branches_rejected() {
    rejects("x := if true: 1 else: \"z\"\n", "incompatible types");
}

#[test]
fn d3_if_expression_int_float_const_mix_rejected() {
    // D3: an int branch beside a float branch is a conflict (like the list literal `[1, 2.5]`), in
    // either order and anywhere in an elif chain.
    rejects("x := if true: 1 else: 2.5\ny := x + 0.5\n", "write 1.0");
    rejects("x := if false: 2.5 else: 1\ny := x + 0.5\n", "write 1.0");
    rejects(
        "x := if false: 1 elif false: 2 else: 3.5\ny := x + 0.5\n",
        "write 1.0",
    );
    rejects(
        "x := if false: 2.5 elif false: 1 else: 2\ny := x + 0.5\n",
        "write 1.0",
    );
    rejects(
        "x := if false: 1 elif false: 2.5 else: 3\ny := x + 0.5\n",
        "write 1.0",
    );
    // The `.0` spelling checks, and an all-int if-expr stays int.
    ok("x := if false: 1.0 elif false: 2.0 else: 3.5\ny := x + 0.5\n");
    // both branches int (no float sibling) is UNCHANGED — stays `int`.
    ok("x := if true: 1 else: 2\ny := x + 1\n");
}

#[test]
fn w10_2_bool_match_both_arms_no_wildcard_is_exhaustive() {
    // W10-2: `bool` has exactly two values; a match with both `true` and `false` arms is
    // exhaustive without a `_`, like Rust's `match c { true => .., false => .. }`. The checker's
    // literal-exhaustiveness path currently treats `bool` like `int`/`str` (infinite domain) and
    // always demands a `_`, which is false for `bool`.
    ok(
        "fn f(c: bool) -> str:\n    match c:\n        true: return \"t\"\n        false: return \"f\"\n",
    );
}

#[test]
fn w10_4_type_alias_resolves_in_enum_variant_pattern() {
    // W10-4: a `type` alias must be usable in pattern position, not just type position.
    ok(
        "enum E:\n    A(int)\n    B\ntype F = E\ny: F = E.A(5)\nmatch y:\n    F.A(n): print(n)\n    F.B: print(\"b\")\n",
    );
}

#[test]
fn alias_resolves_in_struct_ctor_and_static_call() {
    ok(
        "struct P:\n    x: int\n    fn zero() -> P:\n        return P(0)\ntype Q = P\nprint(Q(1), Q.zero())\n",
    );
}

#[test]
fn alias_resolves_in_enum_variant_ctor_and_value() {
    ok("enum E:\n    A(int)\n    B\ntype F = E\nz := F.A(7)\nw := F.B\nprint(z, w)\n");
}

#[test]
fn alias_of_generic_instantiation_resolves_static_call() {
    ok(
        "struct Box[T]:\n    v: T\n    fn of(v: T) -> Box[T]:\n        return Box(v)\ntype IB = Box[int]\nprint(IB.of(3))\n",
    );
}

#[test]
fn alias_of_generic_pins_its_type_arguments() {
    rejects(
        "struct Box[T]:\n    v: T\n    fn of(v: T) -> Box[T]:\n        return Box(v)\ntype IS = Box[str]\nx := IS.of(3)\n",
        "expected",
    );
}

#[test]
fn alias_of_scalar_is_not_a_constructor() {
    rejects("type M = int\nx := M(5)\n", "'M' is a type, not a value");
}

#[test]
fn alias_cycle_still_rejected() {
    rejects("type A = B\ntype B = A\nx: A = 1\n", "recursive type alias");
}

#[test]
fn alias_variant_typo_names_the_alias() {
    rejects(
        "enum E:\n    A(int)\ntype F = E\nz := F.Nope\n",
        "enum 'F' has no variant 'Nope'",
    );
}

/// TICKET-172 (was TICKET-065's negative pin): a local alias of a QUALIFIED struct (`type P =
/// geo.Point`) constructs and calls statics — the compiler now re-points the alias's runtime key to
/// `geo`'s `Point`, so the checker no longer has to refuse it.
#[test]
fn alias_of_qualified_type_constructs() {
    files_ok(&[
        (
            "geo.chz",
            "struct Point:\n    x: int\n    y: int\n    fn zero() -> Point:\n        return Point(0, 0)\n",
        ),
        (
            "main.chz",
            "import geo\ntype P = geo.Point\nx: geo.Point = P(1, 2)\ny: geo.Point = P.zero()\n",
        ),
    ]);
}

/// TICKET-172 (was TICKET-065's negative pin): a local alias of a QUALIFIED enum (`type F = geo.E`)
/// resolves its payload and nullary variants.
#[test]
fn alias_of_qualified_enum_variant_resolves() {
    files_ok(&[
        ("geo.chz", "enum E:\n    A(int)\n    B\n"),
        (
            "main.chz",
            "import geo\ntype F = geo.E\nz: geo.E = F.A(5)\nw: geo.E = F.B\n",
        ),
    ]);
}

#[test]
fn imported_type_alias_resolves_in_expression_position() {
    files_ok(&[
        (
            "aliases.chz",
            "struct Point:\n    x: int\ntype Position = Point\n",
        ),
        (
            "main.chz",
            "import Position from aliases\np := Position(1)\n",
        ),
    ]);
}

/// TICKET-172 fixture: nominal types, the aliases a caller imports, and one function per type that
/// accepts ONLY the canonical type — so a call like `show(Position(1, 2))` proves nominal identity
/// without the caller ever importing `Point` itself.
const ALIAS_LIB: &str = "\
struct Point:
    x: int
    y: int
    fn origin() -> Point:
        return Point(0, 0)
enum Shade:
    Light
    Dark(int)
struct UserId:
    id: int
struct Box[T]:
    v: T
    fn of(v: T) -> Box[T]:
        return Box(v)
type Position = Point
type Tone = Shade
type UserRef = UserId
type IntBox = Box[int]
type Spot = Position
type Count = int
fn show(p: Point) -> int:
    return p.x
fn shade_n(s: Shade) -> int:
    return 1
fn uid(u: UserId) -> int:
    return 2
fn unbox(b: Box[int]) -> int:
    return b.v
";

#[test]
fn imported_alias_heads_preserve_nominal_identity() {
    // Named imports, a renamed import, an exported chain (`Spot`), and a local alias of a named
    // import — none of them imports the canonical type names.
    files_ok(&[
        ("aliases.chz", ALIAS_LIB),
        (
            "main.chz",
            "import Position, Tone, UserRef, IntBox, Spot, show, shade_n, uid, unbox from aliases\n\
             import Position as Pos from aliases\n\
             type Local = Pos\n\
             a := show(Position(1, 2)) + show(Position.origin()) + show(Pos(3, 4))\n\
             b := show(Spot(5, 6)) + show(Local(7, 8)) + show(Local.origin())\n\
             c := shade_n(Tone.Dark(3)) + shade_n(Tone.Light)\n\
             d := uid(UserRef(7)) + unbox(IntBox.of(3))\n",
        ),
    ]);
    // Qualified access through a whole-module import.
    files_ok(&[
        ("aliases.chz", ALIAS_LIB),
        (
            "main.chz",
            "import aliases as a\n\
             x := a.show(a.Position(1, 2)) + a.show(a.Position.origin()) + a.show(a.Spot(1, 1))\n\
             y := a.shade_n(a.Tone.Dark(3)) + a.shade_n(a.Tone.Light)\n\
             z := a.uid(a.UserRef(7)) + a.unbox(a.IntBox.of(3))\n",
        ),
    ]);
    // A LOCAL alias whose body is `module.Type` (the former TICKET-174 half), including a
    // qualified alias of an alias and a pinned generic.
    files_ok(&[
        ("aliases.chz", ALIAS_LIB),
        (
            "main.chz",
            "import aliases as a\n\
             type P = a.Point\n\
             type PP = a.Position\n\
             type T = a.Shade\n\
             type U = a.UserId\n\
             type LB = a.Box[int]\n\
             x := a.show(P(1, 2)) + a.show(P.origin()) + a.show(PP(3, 4))\n\
             y := a.shade_n(T.Dark(2)) + a.shade_n(T.Light)\n\
             z := a.uid(U(3)) + a.unbox(LB.of(4))\n",
        ),
    ]);
}

#[test]
fn imported_alias_scalar_is_not_a_constructor() {
    // Must still fail: a scalar alias stays a type spelling, in every spelling.
    files_reject(
        &[
            ("aliases.chz", ALIAS_LIB),
            ("main.chz", "import Count from aliases\nx := Count(3)\n"),
        ],
        "'Count' is a type, not a value",
    );
    files_reject(
        &[
            ("aliases.chz", ALIAS_LIB),
            ("main.chz", "import aliases as a\nx := a.Count(3)\n"),
        ],
        "module 'a' has no member 'Count'",
    );
    files_reject(
        &[
            ("aliases.chz", ALIAS_LIB),
            (
                "main.chz",
                "import aliases as a\ntype C = a.Count\nx := C(3)\n",
            ),
        ],
        "'C' is a type, not a value",
    );
}

#[test]
fn imported_alias_shadowed_by_a_local_is_a_value() {
    // Must still fail: a parameter named like the alias shadows it, so `Position(1, 2)` calls the
    // `int` parameter.
    files_reject(
        &[
            ("aliases.chz", ALIAS_LIB),
            (
                "main.chz",
                "import Position from aliases\nfn f(Position: int) -> int:\n    x := Position(1, 2)\n    return 0\n",
            ),
        ],
        "not callable",
    );
}

#[test]
fn imported_alias_patterns_resolve() {
    files_ok(&[
        ("aliases.chz", ALIAS_LIB),
        (
            "main.chz",
            "import Tone, Position from aliases\n\
             import aliases as a\n\
             type T = a.Shade\n\
             type TT = a.Tone\n\
             type P = a.Point\n\
             fn named(s: a.Shade) -> int:\n    match s:\n        Tone.Dark(n): return n\n        Tone.Light: return 0\n\
             fn local_q(s: a.Shade) -> int:\n    match s:\n        T.Dark(n): return n\n        T.Light: return 0\n\
             fn chain_q(s: a.Shade) -> int:\n    match s:\n        TT.Dark(n): return n\n        TT.Light: return 0\n\
             fn three_part(s: a.Shade) -> int:\n    match s:\n        a.Tone.Dark(n): return n\n        a.Tone.Light: return 0\n\
             fn nested(o: a.Shade?) -> int:\n    match o:\n        ?Tone.Dark(n): return n\n        ?Tone.Light: return 1\n        None: return 2\n\
             fn st(p: a.Point) -> int:\n    match p:\n        Position(x, y): return x + y\n\
             fn st_q(p: a.Point) -> int:\n    match p:\n        a.Position(x, y): return x + y\n\
             fn st_l(p: a.Point) -> int:\n    match p:\n        P(x, y): return x + y\n",
        ),
    ]);
}

#[test]
fn imported_alias_does_not_license_the_target_enum_name() {
    // Must still fail (DEC-066): importing `Tone` hydrates `Shade`'s variants so `Tone.Dark`
    // resolves, but the bare target name `Shade` stays unimported in a pattern too.
    files_reject(
        &[
            ("aliases.chz", ALIAS_LIB),
            (
                "main.chz",
                "import Tone from aliases\nx := Tone.Dark(3)\nmatch x:\n    Shade.Dark(n): print(n)\n    Shade.Light: print(0)\n",
            ),
        ],
        "enum 'Shade' has no variant 'Dark'",
    );
}

#[test]
fn imported_alias_defaults_select_owner() {
    const LIB: &str = "struct Cfg:\n    n: int = 4\n    fn make(k: int = 9) -> Cfg:\n        return Cfg(k)\ntype Conf = Cfg\n";
    files_ok(&[
        ("lib.chz", LIB),
        (
            "main.chz",
            "import Conf from lib\nimport Conf as C2 from lib\nimport lib as l\ntype LC = l.Cfg\n\
             a := Conf()\nb := Conf.make()\nc := C2()\nd := C2.make()\n\
             e := l.Conf()\nf := l.Conf.make()\ng := LC()\nh := LC.make()\n",
        ),
    ]);
}

#[test]
fn local_alias_ctor_fills_struct_defaults() {
    // `files_ok`, not `ok`: defaults are filled by desugar, which only the graph path runs.
    files_ok(&[(
        "main.chz",
        "struct P:\n    x: int = 5\n    fn mk(a: int = 9) -> P:\n        return P(a)\ntype Q = P\nprint(Q(), Q(1), Q.mk())\n",
    )]);
}

#[test]
fn alias_static_default_collision_binds_the_aliased_declaration() {
    // Two modules declare `CC.new` with DIFFERENT defaults. The checker binds against the one the
    // alias resolves to (lib_c1's, default 31), as CPython does; `call_binding_grid`'s
    // `collision/*` cells run these programs and pin the printed value.
    let c1 = "struct CC:\n    v: int\n    fn new(n: int = 31) -> CC:\n        return CC(n)\ntype AliasCC = CC\n";
    let c2 = "struct CC:\n    v: int\n    fn new(n: int = 32) -> CC:\n        return CC(n)\n";
    files_ok(&[
        ("lib_c1.chz", c1),
        ("lib_c2.chz", c2),
        (
            "main.chz",
            "import lib_c1\nimport lib_c2\nprint(lib_c1.AliasCC.new().v)\n",
        ),
    ]);
    files_ok(&[
        ("lib_c1.chz", c1),
        ("lib_c2.chz", c2),
        (
            "main.chz",
            "import AliasCC as AC from lib_c1\nimport lib_c2\nprint(AC.new().v)\n",
        ),
    ]);
}

#[test]
fn imported_alias_pinned_generic_rejects_wrong_argument() {
    for main in [
        "import IntBox from aliases\nx := IntBox.of(\"bad\")\n",
        "import aliases as a\nx := a.IntBox.of(\"bad\")\n",
        "import aliases as a\ntype LB = a.Box[int]\nx := LB.of(\"bad\")\n",
        "import aliases as a\ntype LB = a.IntBox\nx := LB(\"bad\")\n",
    ] {
        files_reject(
            &[("aliases.chz", ALIAS_LIB), ("main.chz", main)],
            "expected",
        );
    }
    for main in [
        "import IntBox from aliases\nx := IntBox[str](\"s\")\n",
        "import aliases as a\ntype LB = a.Box[int]\nx := LB[str](\"s\")\n",
    ] {
        files_reject(
            &[("aliases.chz", ALIAS_LIB), ("main.chz", main)],
            "already fixes its type arguments",
        );
    }
}

#[test]
fn missing_imported_alias_member_is_precise() {
    files_reject(
        &[
            ("aliases.chz", ALIAS_LIB),
            ("main.chz", "import aliases as a\nx := a.Missing(1)\n"),
        ],
        "module 'a' has no member 'Missing'",
    );
    files_reject(
        &[
            ("aliases.chz", ALIAS_LIB),
            (
                "main.chz",
                "import aliases as a\nfn f(s: a.Shade) -> int:\n    match s:\n        missing.Tone.Dark(n): return n\n        _: return 0\n",
            ),
        ],
        "unknown module 'missing'",
    );
    files_reject(
        &[
            ("aliases.chz", ALIAS_LIB),
            ("main.chz", "import Tone from aliases\nx := Tone.Nope\n"),
        ],
        "enum 'Tone' has no variant 'Nope'",
    );
    files_reject(
        &[
            ("aliases.chz", ALIAS_LIB),
            ("main.chz", "import aliases as a\nx := a.Tone.Nope(1)\n"),
        ],
        "type 'a.Tone' has no static method 'Nope'",
    );
    files_reject(
        &[
            ("aliases.chz", ALIAS_LIB),
            ("main.chz", "import aliases as a\nx := a.Tone.Nope\n"),
        ],
        "enum 'a.Tone' has no variant 'Nope'",
    );
}

#[test]
fn bool_match_expression_both_arms_no_wildcard_is_exhaustive() {
    ok("b := true\nx := match b:\n    true: 1\n    false: 2\n");
}

#[test]
fn bool_match_one_arm_without_wildcard_still_rejected() {
    rejects(
        "b := true\nmatch b:\n    true: print(\"t\")\n",
        "non-exhaustive match: add a `_` arm",
    );
}

#[test]
fn bool_match_guarded_true_arm_does_not_close_domain() {
    rejects(
        "b := true\nmatch b:\n    true if b: print(\"t\")\n    false: print(\"f\")\n",
        "non-exhaustive match: add a `_` arm",
    );
}

#[test]
fn bool_match_wildcard_after_both_literals_warns() {
    warns(
        "b := true\nmatch b:\n    true: print(\"t\")\n    false: print(\"f\")\n    _: print(\"o\")\n",
        "unreachable match arm",
    );
}

#[test]
fn bool_match_binding_arm_after_both_literals_warns() {
    warns(
        "b := true\nmatch b:\n    true: print(\"t\")\n    false: print(\"f\")\n    other: print(other)\n",
        "unreachable match arm",
    );
}

#[test]
fn bool_match_both_literals_no_extra_arm_no_warn() {
    no_warn("b := true\nmatch b:\n    true: print(\"t\")\n    false: print(\"f\")\n");
}

#[test]
fn bool_match_guarded_true_then_wildcard_no_warn() {
    no_warn(
        "b := true\nmatch b:\n    true if b: print(\"t\")\n    false: print(\"f\")\n    _: print(\"o\")\n",
    );
}

#[test]
fn int_match_two_literals_without_wildcard_still_rejected() {
    rejects(
        "n := 1\nmatch n:\n    0: print(\"z\")\n    1: print(\"o\")\n",
        "non-exhaustive match: add a `_` arm",
    );
}

#[test]
fn d3_match_expression_int_float_const_mix_rejected() {
    rejects(
        "x := match true:\n    true: 1\n    _: 2.5\ny := x + 0.5\n",
        "write 1.0",
    );
    ok("x := match true:\n    true: 1.0\n    _: 2.5\ny := x + 0.5\n");
}

#[test]
fn if_match_expression_typed_int_float_mix_still_rejected() {
    // SOUNDNESS: a TYPED int (a variable, a call result) never widens — the compiler cannot see its
    // type, so accepting it would leave an `Int` under a static `float` (the V1 hole). Must still
    // reject, exactly like a typed-int element in a mixed list literal.
    rejects("a := 5\nx := if true: a else: 2.5\n", "incompatible types");
    rejects(
        "a := 5\nx := match true:\n    true: a\n    false: 2.5\n",
        "incompatible types",
    );
}

#[test]
fn if_expression_condition_must_be_bool() {
    rejects("x := if 5: 1 else: 2\n", "if condition must be bool");
}

#[test]
fn match_expression_duplicate_arm_rejected() {
    rejects(
        "s := ?5\nx := match s:\n    ?v: v\n    ?w: w\n    None: 0\n",
        "duplicate match arm",
    );
}

#[test]
fn match_duplicate_literal_arm_rejected() {
    // An exact-duplicate literal arm is dead (first-match) and now errors, matching enum-variant
    // dup detection (was silently accepted — a diagnostic inconsistency).
    rejects(
        "fn f(n: int) -> int:\n    match n:\n        1: return 1\n        1: return 2\n        _: return 0\n",
        "duplicate match arm",
    );
    rejects(
        "fn f(s: str) -> int:\n    match s:\n        \"x\": return 1\n        \"x\": return 2\n        _: return 0\n",
        "duplicate match arm",
    );
    rejects(
        "fn f(b: bool) -> int:\n    match b:\n        true: return 1\n        true: return 2\n        _: return 0\n",
        "duplicate match arm",
    );
    // or-pattern duplicate `1 | 1` (coverage threads through alternatives).
    rejects(
        "fn f(n: int) -> int:\n    match n:\n        1 | 1: return 1\n        _: return 0\n",
        "duplicate match arm",
    );
}

#[test]
fn match_duplicate_literal_guard_carveout_and_distinct_ok() {
    // A GUARDED arm never closes the literal, so `1 if c: … / 1: …` is legal (must NOT over-reject).
    ok(
        "fn f(n: int) -> int:\n    c := true\n    match n:\n        1 if c: return 1\n        1: return 2\n        _: return 0\n",
    );
    // Distinct literals are fine; a literal inside an earlier covering RANGE is NOT flagged (range
    // subsumption is deliberately out of scope — only exact literal dups are detected).
    ok(
        "fn f(n: int) -> int:\n    match n:\n        1: return 1\n        2: return 2\n        _: return 0\n",
    );
    ok(
        "fn f(n: int) -> int:\n    match n:\n        0..10: return 1\n        5: return 2\n        _: return 0\n",
    );
}

#[test]
fn if_expression_unknown_branch_does_not_poison() {
    // One branch is Unknown (undefined name — reported on its own), the other concrete. The result
    // takes the concrete type, so there's no spurious "incompatible types" error.
    let errs = check_src("x := if true: 1 else: undef\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unknown name 'undef'"))
    );
    assert!(!errs.iter().any(|e| e.message.contains("incompatible")));
}

// ===== 10. field access =====

#[test]
fn field_on_non_struct_rejected() {
    rejects("x := 5\ny := x.foo\n", "type int has no field 'foo'");
}

#[test]
fn unknown_struct_field_rejected() {
    rejects(
        "struct P:\n    x: int\np := P(1)\nq := p.y\n",
        "has no field 'y'",
    );
}

/// TICKET-079/TICKET-080: `lenght` is distance 3 from `len` against a limit of 2 (no longer
/// discounted by the 3-character length gap), matching CPython 3.14.7's `[].lenght` — no
/// suggestion.
#[test]
fn method_typo_far_from_every_method_suggests_nothing() {
    let errs = check_src("xs := [1, 2, 3]\nxs.lenght()\n");
    let e = errs
        .iter()
        .find(|e| e.message.contains("has no method 'lenght'"))
        .unwrap_or_else(|| panic!("expected a 'lenght' miss, got: {errs:?}"));
    assert_eq!(e.help, None, "expected no suggestion, got: {e:?}");
}

/// TICKET-007 criterion 2: a struct field typo suggests the near-miss field name.
#[test]
fn field_typo_suggests_near_miss() {
    rejects_help(
        "struct P:\n    x: int\n    y: int\np := P(1, 2)\nq := p.z\n",
        "has no field 'z'",
        "did you mean",
    );
}

/// TICKET-007 criterion 3: a user struct method typo suggests the near-miss method name.
#[test]
fn struct_method_typo_suggests_near_miss() {
    rejects_help(
        "struct T:\n    fn tick(self) -> int:\n        return 1\nt := T()\nt.tik()\n",
        "has no method 'tik'",
        "did you mean 'tick'",
    );
}

/// TICKET-007 criterion 4: a field-assign typo suggests the near-miss field name too.
#[test]
fn field_assign_typo_suggests_near_miss() {
    rejects_help(
        "struct P:\n    x: int\n    y: int\np := P(1, 2)\np.yy = 3\n",
        "cannot assign to 'yy'",
        "did you mean 'y'",
    );
}

/// TICKET-079/TICKET-080: `xqq` against `xs` is distance 2 with a limit of 1 — no longer
/// discounted by the length gap, matching CPython 3.14.7's `NameError` with no suggestion.
#[test]
fn a_two_letter_binding_is_not_suggested_for_a_three_letter_miss() {
    let errs = check_src("xs := [1]\nprint(xqq)\n");
    let e = errs
        .iter()
        .find(|e| e.message.contains("unknown name 'xqq'"))
        .unwrap_or_else(|| panic!("expected an 'xqq' miss, got: {errs:?}"));
    assert_eq!(e.help, None, "expected no suggestion, got: {e:?}");
}

/// TICKET-007 criterion 5: an unrelated method name gets no invented suggestion.
#[test]
fn unrelated_method_name_suggests_nothing() {
    let errs = check_src("xs := [1, 2, 3]\nxs.qqqqq()\n");
    let e = errs
        .iter()
        .find(|e| e.message.contains("has no method 'qqqqq'"))
        .unwrap_or_else(|| panic!("expected a 'qqqqq' miss, got: {errs:?}"));
    assert_eq!(e.help, None, "expected no suggestion, got: {e:?}");
}

/// TICKET-007 criterion 13: an enum method typo suggests the enum's own method.
#[test]
fn enum_method_typo_suggests_near_miss() {
    rejects_help(
        "enum Opt[T]:\n    Some(T)\n    Nothing\n    fn peek(self) -> int:\n        return 1\no := Opt.Some(5)\no.pek()\n",
        "has no method 'pek'",
        "did you mean 'peek'",
    );
}

/// TICKET-007 criterion 7: candidates are sorted before scoring, so a tie is deterministic. All
/// eight methods score 1 against `aa`; without `.sort()` in `Checker::method_names` this fails in
/// 7 runs out of 8 because `StructInfo.methods` is a `HashMap`.
#[test]
fn sorted_candidates_break_ties_deterministically() {
    rejects_help(
        "struct S:\n    fn ab(self) -> int:\n        return 1\n    fn ac(self) -> int:\n        return 1\n    fn ad(self) -> int:\n        return 1\n    fn ae(self) -> int:\n        return 1\n    fn ba(self) -> int:\n        return 1\n    fn ca(self) -> int:\n        return 1\n    fn da(self) -> int:\n        return 1\n    fn ea(self) -> int:\n        return 1\ns := S()\ns.aa()\n",
        "has no method 'aa'",
        "did you mean 'ab'",
    );
}

#[test]
fn struct_field_access_ok() {
    ok("struct P:\n    x: int\np := P(1)\nq := p.x + 1\n");
}

// ===== 11. index =====

#[test]
fn index_non_list_rejected() {
    rejects("x := 5\ny := x[0]\n", "cannot index into int");
}

#[test]
fn index_with_string_rejected() {
    rejects("xs := [1, 2]\ny := xs[\"a\"]\n", "index must be int");
}

#[test]
fn list_index_ok() {
    ok("xs := [1, 2, 3]\ny := xs[0] + 1\n");
}

// ===== 11b. index / field assignment targets =====

#[test]
fn index_assign_ok() {
    ok("xs := [1, 2, 3]\nxs[0] = 9\n");
}

#[test]
fn index_compound_assign_ok() {
    ok("xs := [1, 2, 3]\nxs[0] += 1\nxs[1] -= 2\n");
}

#[test]
fn index_assign_type_mismatch_rejected() {
    rejects("xs := [1, 2, 3]\nxs[0] = \"a\"\n", "cannot assign");
}

#[test]
fn string_index_assign_rejected() {
    rejects("s := \"hi\"\ns[0] = \"x\"\n", "strings are immutable");
}

#[test]
fn field_assign_ok() {
    ok("struct P:\n    x: int\np := P(1)\np.x = 5\n");
}

#[test]
fn field_compound_assign_ok() {
    ok("struct P:\n    x: int\np := P(1)\np.x += 4\np.x -= 1\n");
}

#[test]
fn field_assign_type_mismatch_rejected() {
    rejects(
        "struct P:\n    x: int\np := P(1)\np.x = \"a\"\n",
        "cannot assign",
    );
}

#[test]
fn unknown_field_assign_rejected() {
    rejects(
        "struct P:\n    x: int\np := P(1)\np.y = 5\n",
        "has no field 'y'",
    );
}

#[test]
fn method_assign_rejected() {
    rejects(
        "struct P:\n    x: int\n    fn get(self) -> int:\n        return self.x\np := P(1)\np.get = 5\n",
        "cannot assign",
    );
}

// ===== 12. match =====

#[test]
fn non_exhaustive_match_rejected() {
    let src = "enum Shape:\n    Circle(int)\n    Square(int)\n\
               fn area(s: Shape) -> int:\n    match s:\n        Shape.Circle(r): return r\n";
    rejects(src, "non-exhaustive match on Shape: missing Square");
}

#[test]
fn unknown_variant_in_match_rejected() {
    let src = "enum Shape:\n    Circle(int)\n\
               fn f(s: Shape) -> int:\n    match s:\n        Circle(r): return r\n        Triangle(t): return t\n";
    rejects(src, "'Triangle' is not a variant of Shape");
}

#[test]
fn wrong_binding_arity_rejected() {
    let src = "enum Shape:\n    Circle(int)\n\
               fn f(s: Shape) -> int:\n    match s:\n        Shape.Circle(r, extra): return r\n";
    rejects(src, "binds 1 value");
}

#[test]
fn match_variant_against_int_rejected() {
    // A *variant* pattern against an int scrutinee is a type error (int is matched by literals).
    rejects(
        "x := 5\nmatch x:\n    Circle(r): print(r)\n",
        "cannot match a variant against int",
    );
}

#[test]
fn exhaustive_match_ok() {
    let src = "enum Shape:\n    Circle(int)\n    Square(int)\n\
               fn area(s: Shape) -> int:\n    match s:\n        Shape.Circle(r): return r * r\n        Shape.Square(n): return n * n\n";
    ok(src);
}

// ===== 12b. literal + wildcard match =====

#[test]
fn match_int_literals_with_wildcard_ok() {
    ok("n := 2\nmatch n:\n    0: print(\"zero\")\n    1: print(\"one\")\n    _: print(\"many\")\n");
}

#[test]
fn match_str_literals_with_wildcard_ok() {
    ok("c := \"x\"\nmatch c:\n    \"a\": print(\"first\")\n    _: print(\"other\")\n");
}

#[test]
fn match_bool_literals_with_wildcard_ok() {
    ok(
        "b := true\nmatch b:\n    true: print(\"yes\")\n    false: print(\"no\")\n    _: print(\"?\")\n",
    );
}

#[test]
fn match_int_expr_with_wildcard_ok() {
    ok(
        "code := 200\nlabel := match code:\n    200: \"ok\"\n    404: \"missing\"\n    _: \"other\"\nprint(label)\n",
    );
}

#[test]
fn neg_literal_arm_ok_with_wildcard() {
    // Negative int literal arms are refutable like positive ones; a `_` arm makes it exhaustive.
    ok("n := -3\nmatch n:\n    -3: print(\"a\")\n    -5: print(\"b\")\n    _: print(\"c\")\n");
}

#[test]
fn neg_literal_arm_non_exhaustive_without_wildcard() {
    // A negative literal arm does NOT close the int domain — `_` is still required.
    rejects(
        "n := -3\nmatch n:\n    -3: print(\"a\")\n    -5: print(\"b\")\n",
        "non-exhaustive",
    );
}

#[test]
fn match_int_without_wildcard_rejected() {
    rejects(
        "n := 2\nmatch n:\n    0: print(\"zero\")\n    1: print(\"one\")\n",
        "non-exhaustive",
    );
}

#[test]
fn match_str_arm_against_int_scrutinee_rejected() {
    rejects(
        "n := 2\nmatch n:\n    \"a\": print(\"x\")\n    _: print(\"y\")\n",
        "literal",
    );
}

#[test]
fn match_variant_arm_in_int_match_rejected() {
    rejects(
        "n := 2\nmatch n:\n    Circle(r): print(r)\n    _: print(\"y\")\n",
        "cannot match a variant against int",
    );
}

// ----- struct patterns in `match` (L2) -----

#[test]
fn struct_match_single_arm_exhaustive_ok() {
    // A struct has exactly ONE constructor, so a lone all-binding `Point(x, y)` arm is irrefutable
    // ⇒ exhaustive with NO `_` needed.
    ok(
        "struct Point:\n    x: int\n    y: int\nfn f(p: Point) -> int:\n    match p:\n        Point(a, b): return a + b\n",
    );
}

#[test]
fn struct_match_arity_short_rejected() {
    rejects(
        "struct Point:\n    x: int\n    y: int\nfn f(p: Point):\n    match p:\n        Point(a): print(a)\n",
        "binds 2 field(s), but 1 given",
    );
}

#[test]
fn struct_match_arity_long_rejected() {
    rejects(
        "struct Point:\n    x: int\n    y: int\nfn f(p: Point):\n    match p:\n        Point(a, b, c): print(a)\n",
        "binds 2 field(s), but 3 given",
    );
}

#[test]
fn struct_match_wrong_constructor_rejected() {
    rejects(
        "struct Point:\n    x: int\n    y: int\nfn f(p: Point):\n    match p:\n        Foo(a, b): print(a)\n",
        "'Foo' is not a constructor of Point",
    );
}

#[test]
fn struct_match_literal_field_without_wildcard_rejected() {
    // A refutable literal-field arm `Point(0, y)` does NOT close the domain — `_` is required.
    rejects(
        "struct Point:\n    x: int\n    y: int\nfn f(p: Point):\n    match p:\n        Point(0, y): print(y)\n",
        "non-exhaustive match",
    );
}

#[test]
fn struct_match_generic_field_binds_instantiated_type_ok() {
    // `Box[int]` field `v` must bind as `int` (not `Unknown`/`T`), so `v + 1` type-checks.
    ok(
        "struct Box[T]:\n    v: T\nfn f(b: Box[int]) -> int:\n    match b:\n        Box(v): return v + 1\n",
    );
}

#[test]
fn struct_match_generic_catchall_keeps_targs_ok() {
    // Regression (bug #1): a generic-struct scrutinee `Box[int]` with a refutable field arm plus a
    // whole-value catch-all `rest:` — the catch-all binding must reconstruct the FULL type
    // `Box[int]` (keeping the scrutinee's type args), so `rest.v` resolves to the instantiated field
    // type `int`, not the bare param `T`. Pre-fix the catch-all dropped the targs and this valid
    // program was wrongly rejected with `cannot apply + to T and int`.
    ok(
        "struct Box[T]:\n    v: T\nfn f(b: Box[int]) -> int:\n    match b:\n        Box(0): return 100\n        rest: return rest.v + 1\n",
    );
}

#[test]
fn struct_match_nested_struct_field_ok() {
    ok(
        "struct Point:\n    x: int\n    y: int\nstruct Line:\n    a: Point\n    b: Point\nfn f(l: Line) -> int:\n    match l:\n        Line(Point(x, y), _): return x + y\n",
    );
}

#[test]
fn struct_match_qualified_pattern_unknown_module_rejected() {
    // A qualified struct pattern's qualifier must be an imported module binder. `Foo` is neither a
    // module nor the struct's own name → a clean error, not a mis-bind (bug #2: the message must
    // suggest valid syntax, never the internal `::` identity key).
    rejects(
        "struct Point:\n    x: int\n    y: int\nfn f(p: Point):\n    match p:\n        Foo.Point(a, b): print(a)\n",
        "'Foo' is not a module",
    );
}

#[test]
fn struct_match_duplicate_arm_rejected() {
    // Bug #3: two `Point(x, y)` arms — the first is irrefutable and closes the match, so the second
    // is dead code. Mirrors the enum/literal `duplicate match arm` diagnostic (was silently accepted).
    rejects(
        "struct Point:\n    x: int\n    y: int\nfn f(p: Point) -> int:\n    match p:\n        Point(x, y): return x + y\n        Point(a, b): return a * b\n",
        "duplicate match arm 'Point'",
    );
}

#[test]
fn struct_match_nested_enum_qualifier_rejected() {
    // Bug #4: a NESTED struct sub-pattern qualified with an ENUM name (`E.Point`, a name collision)
    // is check-accepted-then-VM-crashes without this guard (the compiler lowers it as an enum-variant
    // MatchArm with no EnsureEnum guard → `unreachable!` in the VM). `E` is not a module binder, so
    // it must be a clean checker reject (not a runtime panic) — the checker-superset trap.
    rejects(
        "struct Point:\n    x: int\n    y: int\nstruct Line:\n    a: Point\n    b: Point\nenum E:\n    Point(int)\nfn f(l: Line) -> int:\n    match l:\n        Line(E.Point(x, y), _): return x + y\n",
        "'E' is not a module",
    );
}

#[test]
fn match_literal_arm_in_enum_match_rejected() {
    let src = "enum Shape:\n    Circle(int)\n    Square(int)\n\
               fn f(s: Shape):\n    match s:\n        0: print(\"x\")\n        _: print(\"y\")\n";
    rejects(src, "cannot match a literal against Shape");
}

#[test]
fn match_on_float_rejected() {
    rejects(
        "x := 1.5\nmatch x:\n    0: print(\"x\")\n    _: print(\"y\")\n",
        "cannot match on non-enum type float",
    );
}

#[test]
fn match_int_with_wildcard_in_enum_match_ok() {
    // Wildcard makes an enum match exhaustive even with a missing variant.
    let src = "enum Shape:\n    Circle(int)\n    Square(int)\n\
               fn f(s: Shape):\n    match s:\n        Shape.Circle(r): print(\"c\")\n        _: print(\"other\")\n";
    ok(src);
}

// ===== 13. `?` operator =====

#[test]
fn try_on_non_result_rejected() {
    rejects(
        "fn f() -> int!:\n    x := 5?\n    return ?x\n",
        "'?' expects a `T?` or `T!E` value",
    );
}

#[test]
fn try_in_int_function_rejected() {
    let src = "fn g() -> int!:\n    return ?1\n\
               fn f() -> int:\n    x := g()?\n    return x\n";
    rejects(src, "'?' used in a function that returns int");
}

#[test]
fn try_in_result_function_ok() {
    let src = "fn g() -> int!:\n    return ?1\n\
               fn f() -> int!:\n    x := g()?\n    return ?(x + 1)\n";
    ok(src);
}

// SOUNDNESS: `?` in a nil-returning fn silently swallows the propagated Err/None (check-OK-then-
// data-loss). A named nil fn (including `main`) must REJECT — no `fn main` exception; a fn must
// return Result/Option to use `?`. (2026-07-18 bug-hunt.)
#[test]
fn try_in_named_nil_fn_rejected() {
    let src = "fn g() -> int!:\n    return ?1\n\
               fn f():\n    x := g()?\n    print(x)\n";
    rejects(
        src,
        "'?' used in a function that returns None, not a `T?` or `T!E` value",
    );
}

#[test]
fn try_in_named_nil_main_rejected() {
    // No `fn main` exception — main is just a nil fn here.
    let src = "fn g() -> int!:\n    return ?1\n\
               fn main():\n    x := g()?\n    print(x)\n";
    rejects(
        src,
        "'?' used in a function that returns None, not a `T?` or `T!E` value",
    );
}

// A NESTED nil fn (nil `inner` inside a Result-returning `outer`) must also reject — the flag rides
// the fn-body boundary, not just the top-level named fn.
#[test]
fn try_in_nested_nil_fn_rejected() {
    let src = "fn helper() -> int!:\n    return ?1\n\
               fn outer() -> int!:\n    fn inner():\n        x := helper()?\n        print(x)\n    inner()\n    return ?0\n";
    rejects(
        src,
        "'?' used in a function that returns None, not a `T?` or `T!E` value",
    );
}

// GUARD: `?` at MODULE TOP-LEVEL (outside any fn) stays valid — the runtime unwinds the Err at the
// program boundary. Must NOT regress (the flag must be false at module scope).
#[test]
fn try_at_module_top_level_still_accepted() {
    let src = "fn g() -> int!:\n    return ?1\n\
               x := g()?\nprint(x)\n";
    ok(src);
}

// GUARD: an Option-`?` at module top-level stays valid too.
#[test]
fn try_option_at_module_top_level_still_accepted() {
    let src = "fn h() -> int?:\n    return ?1\n\
               x := h()?\nprint(x)\n";
    ok(src);
}

// SOUNDNESS: `?` must match the enclosing function's sum-type KIND (Result vs Option). A Result-`?`
// inside an Option-returning fn (or vice versa) would make the fn return the wrong sum-type and fault
// a downstream exhaustive `match`/`??` at runtime even though `check` passed.
#[test]
fn try_result_in_option_fn_rejected() {
    let src = "fn pr() -> int!:\n    return !\"bad\"\n\
               fn f() -> int?:\n    x := pr()?\n    return ?x\n\
               fn main():\n    match f():\n        ?v: print(\"some {v}\")\n        None: print(\"none\")\n\
               main()\n";
    entry_rejects(src, "returns a `T?` value, not a `T!E` value");
}

#[test]
fn try_option_in_result_fn_rejected() {
    let src = "fn find() -> int?:\n    return None\n\
               fn f() -> int!:\n    x := find()?\n    return ?x\n\
               fn main():\n    match f():\n        ?v: print(v)\n        !e: print(e.message())\n\
               main()\n";
    entry_rejects(src, "returns a `T!E` value, not a `T?` value");
}

#[test]
fn try_result_in_result_compatible_ok() {
    let src = "fn g() -> int!:\n    return ?1\n\
               fn f() -> int!:\n    x := g()?\n    return ?(x + 1)\n";
    entry_ok(src);
}

#[test]
fn try_option_in_option_ok() {
    let src = "fn h() -> int?:\n    return ?1\n\
               fn f() -> int?:\n    x := h()?\n    return ?x\n";
    entry_ok(src);
}
