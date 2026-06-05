//! Type-checker tests. Each `*_rejected` test pins a real bug class the checker must catch (the
//! "red" that drove the code); the `*_ok` tests guard against false positives on valid programs.

use super::*;
use crate::{lexer, parser};

/// Type-check a source string, returning the collected errors (empty = clean).
fn check_src(src: &str) -> Vec<CheckError> {
    let tokens = lexer::tokenize(src).expect("lex should succeed");
    let module = parser::parse(tokens).expect("parse should succeed");
    match check(&module) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    }
}

/// Assert the source type-checks clean.
fn ok(src: &str) {
    let errs = check_src(src);
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
}

/// Assert the source produces at least one error whose message contains `needle`.
fn rejects(src: &str, needle: &str) {
    let errs = check_src(src);
    assert!(
        errs.iter().any(|e| e.message.contains(needle)),
        "expected an error containing {needle:?}, got: {errs:?}"
    );
}

// ===== 1. unknown name =====

#[test]
fn unknown_variable_rejected() {
    rejects("x := y + 1\n", "unknown name 'y'");
}

#[test]
fn declared_variable_ok() {
    ok("x := 5\ny := x + 1\n");
}

// ===== 2. unknown type =====

#[test]
fn unknown_type_annotation_rejected() {
    rejects("x: Widget = 5\n", "unknown type 'Widget'");
}

#[test]
fn unknown_param_type_rejected() {
    rejects("fn f(a: Widget) -> int:\n    return 1\n", "unknown type 'Widget'");
}

// ===== 3. arity =====

#[test]
fn too_few_args_rejected() {
    rejects("fn add(a: int, b: int) -> int:\n    return a + b\nx := add(1)\n", "expects 2 argument");
}

#[test]
fn struct_ctor_arity_rejected() {
    rejects("struct P:\n    x: int\n    y: int\np := P(1)\n", "expects 2 argument");
}

#[test]
fn builtin_arity_rejected() {
    rejects("x := len()\n", "len() expects 1 argument");
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
    rejects("x: int = \"s\"\n", "cannot assign str to variable of type int");
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
    rejects("x := true\nx += 1\n", "cannot apply += to bool and int");
}

#[test]
fn typed_let_ok() {
    ok("x: int = 5\nx += 1\n");
}

// ===== 9. return =====

#[test]
fn return_wrong_type_rejected() {
    rejects("fn f() -> int:\n    return \"s\"\n", "expected return type int, found str");
}

#[test]
fn missing_return_value_rejected() {
    rejects("fn f() -> int:\n    return\n", "expected a return value of type int");
}

#[test]
fn return_matches_signature_ok() {
    ok("fn f(a: int) -> int:\n    return a + 1\n");
}

// ===== 9b. return-type inference (un-annotated `-> T`) =====

#[test]
fn inferred_return_type_used_as_int() {
    // No `-> T`: the body's `return 5` makes f infer `int`, so `x + 1` type-checks.
    ok("fn f():\n    return 5\nx := f()\ny := x + 1\n");
}

#[test]
fn inferred_return_from_expression() {
    ok("fn add(a: int, b: int):\n    return a + b\nx := add(1, 2)\ny := x + 1\n");
}

#[test]
fn void_preserved_when_no_value_return() {
    // No value-return → infers `nil`; using the result as a number is rejected.
    rejects(
        "fn log(m: str):\n    print(m)\nx := log(\"h\")\ny := x + 1\n",
        "cannot apply + to nil and int",
    );
}

#[test]
fn inferred_return_in_if_branch() {
    ok("fn f(c: bool):\n    if c:\n        return 1\n    return 2\nx := f(true)\ny := x + 1\n");
}

#[test]
fn inferred_return_from_accumulator_local() {
    ok("fn sum(xs: list[int]):\n    total := 0\n    for x in xs:\n        total += x\n    return total\nn := sum([1, 2, 3])\nm := n + 1\n");
}

#[test]
fn inferred_return_recursive() {
    ok("fn fib(n: int):\n    if n < 2:\n        return n\n    return fib(n - 1) + fib(n - 2)\nx := fib(10)\ny := x + 1\n");
}

#[test]
fn inferred_return_conflict_rejected() {
    rejects(
        "fn f(c: bool):\n    if c:\n        return 1\n    return \"x\"\n",
        "expected return type int, found str",
    );
}

#[test]
fn inferred_result_return() {
    ok("fn d(a: int, b: int):\n    if b == 0:\n        return Err(\"divide by zero\")\n    return Ok(a / b)\nmatch d(10, 2):\n    Ok(v): print(\"got {v}\")\n    Err(e): print(e)\n");
}

#[test]
fn inferred_return_feeds_typed_let_mismatch() {
    // The inferred `int` return is checked against an explicit `let` annotation.
    rejects(
        "fn f():\n    return 5\nx: str = f()\n",
        "cannot assign int to variable of type str",
    );
}

#[test]
fn inferred_list_return() {
    ok("fn mk():\n    return [1, 2, 3]\nxs := mk()\ny := xs[0] + 1\n");
}

#[test]
fn inferred_struct_return() {
    ok("struct P:\n    x: int\nfn mk():\n    return P(1)\np := mk()\nq := p.x + 1\n");
}

#[test]
fn inferred_forward_ref_callee_first_is_precise() {
    // Callee defined before the caller: the caller infers the precise `int`.
    ok("fn g(n: int):\n    return n * 2\nfn f(n: int):\n    return g(n) + 1\nx := f(3)\ny := x + 1\n");
}

#[test]
fn inferred_forward_ref_callee_later_is_permissive() {
    // Callee defined *after* the caller (both un-annotated): no fixpoint, so the caller infers
    // `Unknown` and stays permissive — crucially NOT a spurious "returns nothing" error.
    ok("fn f(n: int):\n    return g(n) + 1\nfn g(n: int):\n    return n * 2\nx := f(3)\ny := x + 1\n");
}

#[test]
fn inferred_recursion_only_no_spurious_error() {
    // A body whose only return is a self-recursive call infers `Unknown` (not `nil`), so it does
    // not wrongly report "function returns nothing".
    ok("fn loopy(n: int):\n    return loopy(n - 1)\n");
}

#[test]
fn inferred_nested_fn_does_not_pollute_outer() {
    // A nested fn whose name collides with a top-level fn must not feed the outer inference:
    // `outer` infers `int` from its OWN `return 42`, so `x + 1` type-checks.
    ok("fn helper() -> str:\n    return \"top\"\nfn outer(c: bool):\n    fn helper() -> str:\n        return \"nested\"\n    return 42\nx := outer(true)\ny := x + 1\n");
}

#[test]
fn inferred_method_return() {
    // The un-annotated method infers `int` from `return self.v`; the later `return "x"` then
    // conflicts. Observing the conflict proves inference ran on the method (no call site needed —
    // statement-level `self`-method calls are blocked by a separate pre-existing arity bug).
    rejects(
        "struct Box:\n    v: int\n    fn get(self):\n        if true:\n            return self.v\n        return \"x\"\n",
        "expected return type int, found str",
    );
}

// ===== 9c. T? / T! type shorthand (sugar for Option[T] / Result[T]) =====

#[test]
fn type_shorthand_checks_like_long_form() {
    // `int?` (param) and `int!` (return) desugar to Option[int] / Result[int].
    ok("fn f(x: int?) -> int!:\n    match x:\n        Some(v): return Ok(v)\n        None: return Err(\"none\")\n");
}

#[test]
fn optional_shorthand_accepts_some_and_none() {
    ok("x: int? = Some(1)\ny: int? = None\n");
}

#[test]
fn optional_shorthand_rejects_bare_value() {
    rejects("x: int? = 5\n", "cannot assign int to variable of type Option[int]");
}

// ===== 10. field access =====

#[test]
fn field_on_non_struct_rejected() {
    rejects("x := 5\ny := x.foo\n", "type int has no field 'foo'");
}

#[test]
fn unknown_struct_field_rejected() {
    rejects("struct P:\n    x: int\np := P(1)\nq := p.y\n", "has no field 'y'");
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

// ===== 12. match =====

#[test]
fn non_exhaustive_match_rejected() {
    let src = "enum Shape:\n    Circle(int)\n    Square(int)\n\
               fn area(s: Shape) -> int:\n    match s:\n        Circle(r): return r\n";
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
               fn f(s: Shape) -> int:\n    match s:\n        Circle(r, extra): return r\n";
    rejects(src, "binds 1 value");
}

#[test]
fn match_on_int_rejected() {
    rejects("x := 5\nmatch x:\n    Circle(r): print(r)\n", "cannot match on non-enum type int");
}

#[test]
fn exhaustive_match_ok() {
    let src = "enum Shape:\n    Circle(int)\n    Square(int)\n\
               fn area(s: Shape) -> int:\n    match s:\n        Circle(r): return r * r\n        Square(n): return n * n\n";
    ok(src);
}

// ===== 13. `?` operator =====

#[test]
fn try_on_non_result_rejected() {
    rejects(
        "fn f() -> Result[int]:\n    x := 5?\n    return Ok(x)\n",
        "'?' expects Result or Option",
    );
}

#[test]
fn try_in_int_function_rejected() {
    let src = "fn g() -> Result[int]:\n    return Ok(1)\n\
               fn f() -> int:\n    x := g()?\n    return x\n";
    rejects(src, "'?' used in a function that returns int");
}

#[test]
fn try_in_result_function_ok() {
    let src = "fn g() -> Result[int]:\n    return Ok(1)\n\
               fn f() -> Result[int]:\n    x := g()?\n    return Ok(x + 1)\n";
    ok(src);
}

#[test]
fn try_in_main_ok() {
    // `?` in a nothing-returning function (e.g. main) is allowed — matches interpreter semantics.
    let src = "fn g() -> Result[int]:\n    return Ok(1)\n\
               fn main():\n    x := g()?\n    print(x)\n";
    ok(src);
}

// ===== inference / generics =====

#[test]
fn result_ok_payload_must_match_return() {
    rejects(
        "fn f() -> Result[int]:\n    return Ok(\"s\")\n",
        "expected return type Result[int], found Result[str]",
    );
}

#[test]
fn result_err_is_generic_ok() {
    ok("fn f(b: int) -> Result[int]:\n    if b == 0:\n        return Err(\"bad\")\n    return Ok(b)\n");
}

#[test]
fn heterogeneous_list_rejected() {
    rejects("xs := [1, \"two\"]\n", "list elements differ");
}

#[test]
fn closure_inferred_and_called_ok() {
    ok("double := fn(x: int) -> int: x * 2\ny := double(21)\n");
}

#[test]
fn for_over_list_binds_element_type_ok() {
    ok("xs := [1, 2, 3]\nfor n in xs:\n    print(n + 1)\n");
}

#[test]
fn for_over_range_binds_int_ok() {
    ok("total := 0\nfor i in 0..10:\n    total += i\n");
}

// ===== review-panel hardening: redeclaration, assignment targets, closure returns =====

#[test]
fn duplicate_struct_does_not_panic_and_is_reported() {
    // Regression: pass-2 used `structs[name].methods[m.name]` which panicked when a struct name
    // was declared twice (the surviving entry lacked the shadowed struct's method key).
    let src = "struct P:\n    x: int\n    fn a(self) -> int:\n        return self.x\n\
               struct P:\n    y: int\n    fn b(self) -> int:\n        return self.y\n";
    rejects(src, "type 'P' is already defined");
}

#[test]
fn duplicate_function_is_reported() {
    rejects("fn f() -> int:\n    return 1\nfn f() -> int:\n    return 2\n", "already defined");
}

#[test]
fn variant_name_shared_across_enums_is_reported() {
    // `variants` is keyed by bare name; a collision would otherwise silently mis-type.
    rejects("enum A:\n    X(int)\nenum B:\n    X(str)\n", "variant 'X' is already defined");
}

#[test]
fn field_assignment_rejected() {
    // The interpreter only assigns to bare variables; the checker must match that.
    rejects("struct P:\n    x: int\np := P(1)\np.x = 2\n", "invalid assignment target");
}

#[test]
fn index_assignment_rejected() {
    rejects("xs := [1, 2]\nxs[0] = 9\n", "invalid assignment target");
}

#[test]
fn closure_body_violating_return_annotation_rejected() {
    rejects("f := fn(x: int) -> int: \"s\"\n", "closure body has type str");
}

#[test]
fn closure_body_matching_return_annotation_ok() {
    ok("f := fn(x: int) -> int: x * 2\ny := f(3)\n");
}

// ===== golden: the touchstone program must type-check clean =====

#[test]
fn hello_example_type_checks() {
    let src = include_str!("../../examples/hello.chz");
    ok(src);
}

#[test]
fn methods_example_type_checks() {
    let src = include_str!("../../examples/methods.chz");
    ok(src);
}

// ===== multiple errors are collected, not just the first =====

#[test]
fn collects_multiple_errors() {
    let errs = check_src("x := a + b\ny := c - d\n");
    assert!(errs.len() >= 4, "expected >=4 unknown-name errors, got: {errs:?}");
}

// ===== M6a: core-type methods (str / list) =====

#[test]
fn str_methods_infer_types_ok() {
    ok("s := \"Hi\"\nn := s.len()\nu := s.upper()\nl := s.lower()\nt := s.trim()\n");
    // bool-returning methods used in a bool context
    ok("s := \"abc\"\nif s.starts_with(\"a\"):\n    print(s)\n");
    ok("s := \"abc\"\nif s.contains(\"b\"):\n    print(s)\n");
}

#[test]
fn str_len_is_int() {
    // len() must be int — use it where an int is required.
    ok("s := \"hi\"\nn: int = s.len()\nprint(n)\n");
}

#[test]
fn str_upper_is_str() {
    ok("s := \"hi\"\nu: str = s.upper()\nprint(u)\n");
}

#[test]
fn str_split_returns_list_of_str() {
    ok("parts := \"a,b,c\".split(\",\")\nx: list[str] = parts\nprint(x)\n");
}

#[test]
fn str_split_element_is_str_not_int() {
    rejects("parts: list[int] = \"a,b\".split(\",\")\n", "list[str] to variable of type list[int]");
}

#[test]
fn str_join_takes_list_of_str_returns_str() {
    ok("xs := [\"a\", \"b\"]\nr: str = \",\".join(xs)\nprint(r)\n");
}

#[test]
fn str_join_rejects_list_of_int() {
    rejects("r := \",\".join([1, 2])\n", "argument 1 of 'join'");
}

#[test]
fn str_starts_with_is_bool() {
    ok("s := \"hi\"\nb: bool = s.starts_with(\"h\")\nprint(b)\n");
}

#[test]
fn str_method_wrong_arity_rejected() {
    rejects("s := \"hi\"\nx := s.upper(\"extra\")\n", "'upper' expects 0 argument(s), got 1");
}

#[test]
fn str_split_arg_must_be_str() {
    rejects("x := \"a,b\".split(5)\n", "argument 1 of 'split'");
}

#[test]
fn unknown_str_method_rejected() {
    rejects("s := \"hi\"\nx := s.frobnicate()\n", "type str has no method 'frobnicate'");
}

#[test]
fn list_push_and_len_ok() {
    ok("xs := [1, 2, 3]\nxs.push(4)\nn := xs.len()\nprint(n)\n");
}

#[test]
fn list_push_element_type_checked() {
    rejects("xs := [1, 2, 3]\nxs.push(\"nope\")\n", "argument 1 of 'push'");
}

#[test]
fn list_len_is_int() {
    ok("xs := [1, 2]\nn: int = xs.len()\nprint(n)\n");
}

#[test]
fn unknown_list_method_rejected() {
    rejects("xs := [1, 2]\nx := xs.frobnicate()\n", "type list[int] has no method 'frobnicate'");
}

#[test]
fn method_on_int_rejected() {
    rejects("x := 5\ny := x.upper()\n", "type int has no method 'upper'");
}

// ===== reserved builtin type names =====

#[test]
fn user_enum_named_result_rejected() {
    rejects("enum Result:\n    A\n", "type 'Result' is reserved (builtin)");
}

#[test]
fn user_struct_named_option_rejected() {
    rejects("struct Option:\n    x: int\n", "type 'Option' is reserved (builtin)");
}
