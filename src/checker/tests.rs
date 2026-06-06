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

// ===== generics & structural protocols (G1) =====

const POINT: &str = "\
struct Point:
    x: int
    y: int
    fn compare(self, other: Point) -> int:
        return (self.x + self.y) - (other.x + other.y)
";

#[test]
fn generic_max_over_int_ok() {
    ok("fn max[T: Comparable](a: T, b: T) -> T:\n    if a < b:\n        return b\n    return a\nm := max(3, 7)\n");
}

#[test]
fn generic_max_over_comparable_struct_ok() {
    let src = format!(
        "{POINT}fn max[T: Comparable](a: T, b: T) -> T:\n    if a < b:\n        return b\n    return a\np := max(Point(1, 2), Point(3, 0))\n"
    );
    ok(&src);
}

#[test]
fn generic_max_result_type_is_substituted() {
    // The result of max(3,7) is int, so a str-typed binding must be rejected.
    rejects(
        "fn max[T: Comparable](a: T, b: T) -> T:\n    return a\nx: str = max(3, 7)\n",
        "cannot assign int",
    );
}

#[test]
fn ordering_on_unbounded_type_param_rejected() {
    rejects(
        "fn pick[T](a: T, b: T) -> T:\n    if a < b:\n        return b\n    return a\n",
        "cannot compare",
    );
}

#[test]
fn calling_comparable_generic_on_non_comparable_struct_rejected() {
    // Plain (no `compare` method) ⇒ does not satisfy Comparable.
    let src = "\
struct Plain:
    n: int
fn max[T: Comparable](a: T, b: T) -> T:
    return a
p := max(Plain(1), Plain(2))
";
    rejects(src, "does not satisfy Comparable");
}

#[test]
fn generic_call_with_mismatched_type_args_rejected() {
    rejects(
        "fn max[T: Comparable](a: T, b: T) -> T:\n    return a\nx := max(3, \"a\")\n",
        "expected int",
    );
}

#[test]
fn unbounded_generic_passthrough_ok() {
    ok("fn first[T](a: T, b: T) -> T:\n    return a\nx := first(1, 2)\ny := first(\"a\", \"b\")\n");
}

#[test]
fn ordering_on_comparable_struct_directly_ok() {
    ok(&format!("{POINT}b := Point(1, 2) < Point(3, 4)\n"));
}

#[test]
fn ordering_on_non_comparable_struct_rejected() {
    rejects(
        "struct Plain:\n    n: int\nb := Plain(1) < Plain(2)\n",
        "cannot compare",
    );
}

#[test]
fn calling_protocol_method_on_type_param_ok() {
    ok("fn cmp[T: Comparable](a: T, b: T) -> int:\n    return a.compare(b)\n");
}

#[test]
fn user_protocol_bound_ok() {
    let src = "\
protocol Shape:
    fn area(self) -> float
struct Circle:
    r: float
    fn area(self) -> float:
        return self.r
fn biggest[T: Shape](a: T) -> float:
    return a.area()
x := biggest(Circle(2.0))
";
    ok(src);
}

#[test]
fn unknown_protocol_bound_rejected() {
    rejects("fn f[T: Bogus](a: T) -> T:\n    return a\n", "unknown protocol 'Bogus'");
}

#[test]
fn redeclaring_comparable_rejected() {
    rejects(
        "protocol Comparable:\n    fn compare(self, other: Self) -> int\n",
        "reserved",
    );
}

// ----- generic structs (G2) -----

const PAIR: &str = "\
struct Pair[A, B]:
    first: A
    second: B
    fn left(self) -> A:
        return self.first
";

#[test]
fn generic_struct_field_type_substituted() {
    // first is A=int; assigning it to a str binding must be rejected.
    rejects(&format!("{PAIR}p := Pair(1, \"x\")\nn: str = p.first\n"), "cannot assign int");
}

#[test]
fn generic_struct_construction_and_field_ok() {
    ok(&format!("{PAIR}p := Pair(1, \"x\")\nn: int = p.first\ns: str = p.second\n"));
}

#[test]
fn generic_struct_method_return_substituted() {
    ok(&format!("{PAIR}p := Pair(7, \"x\")\nn: int = p.left()\n"));
    rejects(&format!("{PAIR}p := Pair(7, \"x\")\nn: str = p.left()\n"), "cannot assign int");
}

#[test]
fn generic_struct_explicit_type_args_ok() {
    ok(&format!("{PAIR}p: Pair[str, int] = Pair(\"k\", 9)\n"));
}

#[test]
fn generic_struct_wrong_arity_rejected() {
    rejects(&format!("{PAIR}p: Pair[int] = Pair(1, 2)\n"), "expects 2 type argument(s)");
}

#[test]
fn generic_struct_method_arg_checked_against_type_arg() {
    let src = "\
struct Box[T]:
    val: T
    fn set(self, x: T) -> T:
        return x
b := Box(5)
y := b.set(\"nope\")
";
    rejects(src, "expected int");
}

#[test]
fn generic_struct_method_arg_ok() {
    let src = "\
struct Box[T]:
    val: T
    fn set(self, x: T) -> T:
        return x
b := Box(5)
y := b.set(9)
";
    ok(src);
}

// ----- review-panel regressions (G1/G2) -----

#[test]
fn bounded_type_param_forwards_to_bounded_call_ok() {
    // Review #1: a `T: Comparable` value must satisfy Comparable when forwarded to another
    // `[U: Comparable]` call (generic composition).
    let src = "\
fn max[T: Comparable](a: T, b: T) -> T:
    if a < b:
        return b
    return a
fn pick[T: Comparable](a: T, b: T) -> T:
    return max(a, b)
print(pick(3, 7))
";
    ok(src);
}

#[test]
fn generic_struct_bound_enforced_at_construction() {
    // Review C1: a struct type-param bound must be enforced, not just on generic fns.
    let src = "\
struct Plain:
    n: int
struct Box[T: Comparable]:
    a: T
b := Box(Plain(1))
";
    rejects(src, "does not satisfy Comparable");
}

#[test]
fn generic_struct_bound_enforced_on_explicit_type_arg() {
    let src = "\
struct Plain:
    n: int
struct Box[T: Comparable]:
    a: T
b: Box[Plain] = Box(Plain(1))
";
    rejects(src, "does not satisfy Comparable");
}

#[test]
fn generic_struct_bound_satisfied_ok() {
    let src = "\
struct Box[T: Comparable]:
    a: T
b := Box(5)
c: Box[str] = Box(\"hi\")
";
    ok(src);
}

#[test]
fn type_param_shadows_same_named_struct() {
    // Review I2: an in-scope type parameter shadows a same-named type.
    let src = "\
struct T:
    n: int
fn id[T](x: T) -> T:
    return x
y := id(5)
";
    ok(src);
}

#[test]
fn bare_generic_struct_without_args_rejected() {
    // Review I3: using a generic struct as a type without its arguments is an error.
    let src = "\
struct Box[T]:
    v: T
fn unwrap(b: Box) -> int:
    return 0
";
    rejects(src, "expects 1 type argument(s), got 0");
}

// ----- sort() widened to Comparable (G3) -----

#[test]
fn sort_on_comparable_struct_list_ok() {
    let src = "\
struct P:
    n: int
    fn compare(self, o: P) -> int:
        return self.n - o.n
xs := [P(2), P(1)]
xs.sort()
";
    ok(src);
}

#[test]
fn sort_on_non_comparable_struct_list_rejected() {
    let src = "\
struct P:
    n: int
xs := [P(2), P(1)]
xs.sort()
";
    rejects(src, "sort() requires a list of Comparable");
}

#[test]
fn sort_on_primitive_list_still_ok() {
    ok("xs := [3, 1, 2]\nxs.sort()\nys := [\"b\", \"a\"]\nys.sort()\n");
}

#[test]
fn sort_with_args_rejected() {
    rejects("xs := [3, 1]\nxs.sort(5)\n", "expects 0 argument(s)");
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

// ===== 8b. compound assignment must not widen int -> float (gap #9) =====

#[test]
fn plus_eq_float_into_int_var_rejected() {
    rejects("x: int = 5\nx += 1.5\n", "cannot apply += to int and float");
}

#[test]
fn minus_eq_float_into_int_var_rejected() {
    rejects("x: int = 5\nx -= 1.5\n", "cannot apply -= to int and float");
}

#[test]
fn plus_eq_float_into_int_index_rejected() {
    rejects("xs := [1, 2, 3]\nxs[0] += 1.5\n", "cannot apply += to int and float");
}

#[test]
fn plus_eq_float_into_int_field_rejected() {
    rejects(
        "struct P:\n    x: int\np := P(1)\np.x += 1.5\n",
        "cannot apply += to int and float",
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
    // The un-annotated method infers `int` from `return self.v`; the later `return "x"` conflicts.
    // The conflict proves inference ran on the method body.
    rejects(
        "struct Box:\n    v: int\n    fn get(self):\n        if true:\n            return self.v\n        return \"x\"\n",
        "expected return type int, found str",
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
    rejects(&format!("{BOX}b := Box(5)\nx := b.add()\n"), "expects 1 argument");
}

#[test]
fn method_call_wrong_arg_type_rejected() {
    rejects(&format!("{BOX}b := Box(5)\nx := b.add(\"s\")\n"), "expected int");
}

#[test]
fn method_call_too_many_args_rejected() {
    rejects(&format!("{BOX}b := Box(5)\nx := b.get(1)\n"), "expects 0 argument");
}

#[test]
fn method_without_receiver_param_rejected() {
    // A method with no params has no receiver slot; calling it on an instance must be rejected at
    // check time — otherwise the runtime errors ("expects 0 argument(s), got 1") since both engines
    // prepend the receiver.
    rejects(
        "struct Box:\n    v: int\n    fn ping():\n        print(\"x\")\nb := Box(5)\nb.ping()\n",
        "no receiver",
    );
}

#[test]
fn method_calls_another_method_via_self() {
    // The motivating case: `self.dbl()` inside a method body — a `self`-method call with 0 args.
    ok("struct Box:\n    v: int\n    fn dbl(self) -> int:\n        return self.v * 2\n    fn quad(self) -> int:\n        return self.dbl() + self.dbl()\n");
}

#[test]
fn method_call_multi_arg_arity() {
    let src = "struct C:\n    v: int\n    fn f(self, a: int, b: int) -> int:\n        return self.v + a + b\n";
    ok(&format!("{src}c := C(1)\nx := c.f(2, 3)\n"));
    rejects(&format!("{src}c := C(1)\nx := c.f(2)\n"), "expects 2 argument");
}

#[test]
fn method_call_first_param_not_named_self_ok() {
    // The receiver is positional, not keyed on the name `self`.
    ok("struct Point:\n    x: int\n    fn getx(p: Point) -> int:\n        return p.x\np := Point(7)\nn := p.getx()\nm := n + 1\n");
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

// ===== 9d. expression-valued match / if (Part 3) =====

#[test]
fn match_expression_unifies_arms() {
    ok("s := Some(5)\nx := match s:\n    Some(v): v\n    None: 0\ny := x + 1\n");
}

#[test]
fn match_expression_incompatible_arms_rejected() {
    rejects(
        "s := Some(5)\nx := match s:\n    Some(v): v\n    None: \"z\"\n",
        "incompatible types",
    );
}

#[test]
fn match_expression_nonexhaustive_rejected() {
    rejects("s := Some(5)\nx := match s:\n    Some(v): v\n", "non-exhaustive");
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
fn if_expression_condition_must_be_bool() {
    rejects("x := if 5: 1 else: 2\n", "if condition must be bool");
}

#[test]
fn match_expression_duplicate_arm_rejected() {
    rejects(
        "s := Some(5)\nx := match s:\n    Some(v): v\n    Some(w): w\n    None: 0\n",
        "duplicate match arm",
    );
}

#[test]
fn if_expression_unknown_branch_does_not_poison() {
    // One branch is Unknown (undefined name — reported on its own), the other concrete. The result
    // takes the concrete type, so there's no spurious "incompatible types" error.
    let errs = check_src("x := if true: 1 else: undef\n");
    assert!(errs.iter().any(|e| e.message.contains("unknown name 'undef'")));
    assert!(!errs.iter().any(|e| e.message.contains("incompatible")));
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
    rejects("struct P:\n    x: int\np := P(1)\np.x = \"a\"\n", "cannot assign");
}

#[test]
fn unknown_field_assign_rejected() {
    rejects("struct P:\n    x: int\np := P(1)\np.y = 5\n", "has no field 'y'");
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
fn match_variant_against_int_rejected() {
    // A *variant* pattern against an int scrutinee is a type error (int is matched by literals).
    rejects("x := 5\nmatch x:\n    Circle(r): print(r)\n", "cannot match a variant against int");
}

#[test]
fn exhaustive_match_ok() {
    let src = "enum Shape:\n    Circle(int)\n    Square(int)\n\
               fn area(s: Shape) -> int:\n    match s:\n        Circle(r): return r * r\n        Square(n): return n * n\n";
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
    ok("b := true\nmatch b:\n    true: print(\"yes\")\n    false: print(\"no\")\n    _: print(\"?\")\n");
}

#[test]
fn match_int_expr_with_wildcard_ok() {
    ok("code := 200\nlabel := match code:\n    200: \"ok\"\n    404: \"missing\"\n    _: \"other\"\nprint(label)\n");
}

#[test]
fn match_int_without_wildcard_rejected() {
    rejects("n := 2\nmatch n:\n    0: print(\"zero\")\n    1: print(\"one\")\n", "non-exhaustive");
}

#[test]
fn match_str_arm_against_int_scrutinee_rejected() {
    rejects("n := 2\nmatch n:\n    \"a\": print(\"x\")\n    _: print(\"y\")\n", "literal");
}

#[test]
fn match_variant_arm_in_int_match_rejected() {
    rejects("n := 2\nmatch n:\n    Circle(r): print(r)\n    _: print(\"y\")\n", "cannot match a variant against int");
}

#[test]
fn match_literal_arm_in_enum_match_rejected() {
    let src = "enum Shape:\n    Circle(int)\n    Square(int)\n\
               fn f(s: Shape):\n    match s:\n        0: print(\"x\")\n        _: print(\"y\")\n";
    rejects(src, "cannot match a literal against Shape");
}

#[test]
fn match_on_float_rejected() {
    rejects("x := 1.5\nmatch x:\n    0: print(\"x\")\n    _: print(\"y\")\n", "cannot match on non-enum type float");
}

#[test]
fn match_int_with_wildcard_in_enum_match_ok() {
    // Wildcard makes an enum match exhaustive even with a missing variant.
    let src = "enum Shape:\n    Circle(int)\n    Square(int)\n\
               fn f(s: Shape):\n    match s:\n        Circle(r): print(\"c\")\n        _: print(\"other\")\n";
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
fn closure_body_violating_return_annotation_rejected() {
    rejects("f := fn(x: int) -> int: \"s\"\n", "closure body has type str");
}

#[test]
fn closure_body_matching_return_annotation_ok() {
    ok("f := fn(x: int) -> int: x * 2\ny := f(3)\n");
}

// ===== list sort() — orderable element types only (gap #4) =====

#[test]
fn list_sort_int_ok() {
    ok("xs := [3, 1, 2]\nxs.sort()\n");
}

#[test]
fn list_sort_str_ok() {
    ok("xs := [\"b\", \"a\"]\nxs.sort()\n");
}

#[test]
fn list_sort_float_ok() {
    ok("xs := [3.0, 1.0]\nxs.sort()\n");
}

#[test]
fn list_sort_returns_nil_rejected_as_value() {
    // sort() mutates in place and yields nil — using its result as a number is rejected.
    rejects("xs := [3, 1, 2]\nn := xs.sort() + 1\n", "cannot apply + to nil and int");
}

#[test]
fn list_sort_non_orderable_rejected() {
    rejects("xs := [true, false]\nxs.sort()\n", "sort() requires");
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
fn str_chars_returns_list_of_str() {
    ok("cs: list[str] = \"abc\".chars()\nprint(cs)\n");
}

#[test]
fn str_chars_element_is_str_not_int() {
    rejects("cs: list[int] = \"abc\".chars()\n", "list[str] to variable of type list[int]");
}

#[test]
fn for_over_str_binds_str() {
    ok("for c in \"abc\":\n    u: str = c.upper()\n    print(u)\n");
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
fn list_pop_returns_option() {
    ok("xs := [1, 2, 3]\nm := xs.pop()\nmatch m:\n    Some(v): print(v)\n    None: print(0)\n");
}

#[test]
fn list_non_hof_methods_ok() {
    ok("xs := [1, 2, 3]\nb := xs.contains(2)\ni := xs.index_of(2)\ns := xs.sum()\nxs.reverse()\n");
}

#[test]
fn list_sum_float_is_float() {
    ok("xs := [1.0, 2.0]\ns := xs.sum()\nt := s + 0.5\n");
}

#[test]
fn list_sum_non_numeric_rejected() {
    rejects("xs := [\"a\"]\ns := xs.sum()\n", "numeric");
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

// ===== M6c: native std module signatures =====

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static CHECKER_TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = CHECKER_TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_chk_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.0.join(rel);
        std::fs::write(&p, contents).unwrap();
        p
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Type-check an entry program that may import native std modules (resolved via `build_graph`).
fn check_entry(src: &str) -> Vec<CheckError> {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    }
}

fn entry_ok(src: &str) {
    let errs = check_entry(src);
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
}

fn entry_rejects(src: &str, needle: &str) {
    let errs = check_entry(src);
    assert!(
        errs.iter().any(|e| e.message.contains(needle)),
        "expected an error containing {needle:?}, got: {errs:?}"
    );
}

#[test]
fn native_math_floor_typechecks_and_returns_float() {
    entry_ok("import std.math\nfn main():\n    x: float = math.floor(2.7)\n    print(x)\n");
}

#[test]
fn native_math_unknown_member_rejected() {
    entry_rejects(
        "import std.math\nfn main():\n    print(math.nope(1.0))\n",
        "has no member 'nope'",
    );
}

#[test]
fn native_math_wrong_arity_rejected() {
    entry_rejects(
        "import std.math\nfn main():\n    print(math.pow(2.0))\n",
        "expects 2 argument",
    );
}

#[test]
fn native_math_constant_pi_is_float() {
    entry_ok("import std.math\nfn main():\n    r: float = math.pi\n    print(r)\n");
}

#[test]
fn native_math_from_import_binds_member() {
    entry_ok("import floor from std.math\nfn main():\n    x: float = floor(2.7)\n    print(x)\n");
}

#[test]
fn native_math_float_param_rejects_int() {
    // The language has no implicit int->float; math.sqrt(int) is a type error.
    entry_rejects(
        "import std.math\nfn main():\n    print(math.sqrt(16))\n",
        "",
    );
}

// ===== int+float polymorphic math (gap #12) =====

#[test]
fn cmp_max_int_returns_int() {
    entry_ok("import std.cmp\nfn main():\n    x: int = cmp.max(3, 5)\n    print(x)\n");
}

#[test]
fn cmp_max_float_returns_float() {
    entry_ok("import std.cmp\nfn main():\n    x: float = cmp.max(3.0, 5.0)\n    print(x)\n");
}

#[test]
fn cmp_min_int_returns_int() {
    entry_ok("import std.cmp\nfn main():\n    x: int = cmp.min(3, 5)\n    print(x)\n");
}

#[test]
fn cmp_max_over_comparable_struct_ok() {
    entry_ok(
        "import std.cmp\nstruct P:\n    n: int\n    fn compare(self, o: P) -> int:\n        return self.n - o.n\nfn main():\n    p := cmp.max(P(1), P(2))\n    print(p.n)\n",
    );
}

#[test]
fn cmp_max_over_non_comparable_struct_rejected() {
    entry_rejects(
        "import std.cmp\nstruct P:\n    n: int\nfn main():\n    p := cmp.max(P(1), P(2))\n    print(p.n)\n",
        "does not satisfy Comparable",
    );
}

#[test]
fn native_math_abs_int_returns_int() {
    entry_ok("import std.math\nfn main():\n    x: int = math.abs(-5)\n    print(x)\n");
}

#[test]
fn native_math_abs_float_returns_float() {
    entry_ok("import std.math\nfn main():\n    x: float = math.abs(-5.0)\n    print(x)\n");
}

#[test]
fn cmp_max_int_result_not_float() {
    // The int instantiation must NOT be a float — assigning to a float slot is rejected.
    entry_rejects(
        "import std.cmp\nfn main():\n    x: float = cmp.max(3, 5)\n    print(x)\n",
        "",
    );
}

#[test]
fn cmp_max_mixed_int_float_rejected() {
    // No implicit int->float widening: T unifies to int, so a float second arg is rejected.
    entry_rejects(
        "import std.cmp\nfn main():\n    print(cmp.max(3, 5.0))\n",
        "",
    );
}

#[test]
fn cmp_from_import_max_int_returns_int() {
    // The `from`-import form routes through the same generic-call path as qualified `cmp.max`.
    entry_ok("import max from std.cmp\nfn main():\n    x: int = max(3, 5)\n    print(x)\n");
}

#[test]
fn cmp_from_import_max_float_returns_float() {
    entry_ok("import max from std.cmp\nfn main():\n    x: float = max(3.0, 5.0)\n    print(x)\n");
}

#[test]
fn cmp_from_import_max_int_not_float() {
    entry_rejects(
        "import max from std.cmp\nfn main():\n    x: float = max(3, 5)\n    print(x)\n",
        "",
    );
}

#[test]
fn native_math_floor_still_float_only() {
    // Only abs/min/max became polymorphic; floor stays float-only.
    entry_rejects(
        "import std.math\nfn main():\n    print(math.floor(2))\n",
        "",
    );
}

// ===== higher-order-function parameter types =====

#[test]
fn hof_param_type_ok() {
    ok("fn apply(f: fn(int) -> int, v: int) -> int:\n    return f(v)\ninc := fn(x: int) -> int: x + 1\nn := apply(inc, 4)\n");
}

#[test]
fn hof_param_type_wrong_fn_rejected() {
    rejects(
        "fn apply(f: fn(int) -> int) -> int:\n    return f(0)\nbad := fn(x: str) -> int: 0\nn := apply(bad)\n",
        "argument",
    );
}

// ===== higher-order list methods: map / filter / fold =====

#[test]
fn list_map_ok() {
    ok("xs := [1,2,3]\nys := xs.map(fn(x: int) -> int: x * 2)\nz := ys[0] + 1\n");
}

#[test]
fn list_filter_ok() {
    ok("xs := [1,2,3]\nys := xs.filter(fn(x: int) -> bool: x > 1)\n");
}

#[test]
fn list_fold_ok() {
    ok("xs := [1,2,3]\ns := xs.fold(0, fn(a: int, x: int) -> int: a + x)\nt := s + 1\n");
}

#[test]
fn list_map_changes_element_type() {
    // map int -> bool produces list[bool]; indexing yields a bool.
    ok("xs := [1,2]\nys := xs.map(fn(x: int) -> bool: x > 0)\nb := ys[0]\n");
}

#[test]
fn list_filter_predicate_must_return_bool() {
    rejects(
        "xs := [1,2,3]\nys := xs.filter(fn(x: int) -> int: x)\n",
        "predicate",
    );
}

#[test]
fn list_map_function_param_must_match_element() {
    rejects(
        "xs := [1,2,3]\nys := xs.map(fn(x: str) -> int: 0)\n",
        "map",
    );
}

#[test]
fn list_fold_function_acc_must_match_init() {
    rejects(
        "xs := [1,2,3]\ns := xs.fold(0, fn(a: str, x: int) -> str: a)\n",
        "fold",
    );
}

#[test]
fn list_sort_by_ok() {
    ok("xs := [3,1,2]\nxs.sort_by(fn(a: int, b: int) -> int: a - b)\n");
}

#[test]
fn list_sort_by_comparator_param_must_match_element() {
    rejects(
        "xs := [1,2,3]\nxs.sort_by(fn(a: str, b: str) -> int: 0)\n",
        "sort_by",
    );
}

#[test]
fn list_sort_by_comparator_must_return_int() {
    rejects(
        "xs := [1,2,3]\nxs.sort_by(fn(a: int, b: int) -> bool: a < b)\n",
        "sort_by",
    );
}

#[test]
fn list_sort_by_comparator_must_take_two_args() {
    rejects(
        "xs := [1,2,3]\nxs.sort_by(fn(a: int) -> int: a)\n",
        "sort_by",
    );
}


// ===== ord / chr builtins (gap #10) =====

#[test]
fn ord_of_str_is_int() {
    ok("n: int = ord(\"a\")\n");
}

#[test]
fn chr_of_int_is_str() {
    ok("s: str = chr(65)\n");
}

#[test]
fn ord_roundtrip_chr() {
    ok("s: str = chr(ord(\"z\"))\n");
}

#[test]
fn ord_rejects_int_arg() {
    rejects("n := ord(5)\n", "ord");
}

#[test]
fn chr_rejects_str_arg() {
    rejects("s := chr(\"x\")\n", "chr");
}

#[test]
fn ord_rejects_wrong_arity() {
    rejects("n := ord(\"a\", \"b\")\n", "ord");
}

#[test]
fn ord_result_is_int_not_str() {
    // `ord(c) - ord("0")` — the digit-value idiom — type-checks as int arithmetic.
    ok("c := \"7\"\nd: int = ord(c) - ord(\"0\")\n");
}


// ===== bitwise operators (gap #13) =====

#[test]
fn bitwise_int_ops_ok() {
    ok("a := 5 & 3\nb := 5 | 2\nc := 5 ^ 3\nd := 1 << 4\ne := 255 >> 4\n");
}

#[test]
fn bitwise_result_is_int() {
    ok("x: int = (1 << 8) | 3\n");
}

#[test]
fn bitwise_on_float_rejected() {
    rejects("x := 5 & 3.0\n", "bitwise");
}

#[test]
fn shift_on_float_rejected() {
    rejects("x := 1.0 << 2\n", "bitwise");
}

#[test]
fn bitwise_on_str_rejected() {
    rejects("x := \"a\" ^ \"b\"\n", "bitwise");
}


// ===== nested / tuple match patterns (gap #15) =====

#[test]
fn match_tuple_pattern_ok() {
    ok("t := (1, 2)\nmatch t:\n    (a, b): print(a + b)\n");
}

#[test]
fn match_tuple_binds_element_types() {
    // Tuple elements bind with their element types: `s` is str, `n` is int.
    ok("t := (\"x\", 2)\nmatch t:\n    (s, n): print(\"{s}{n + 1}\")\n");
}

#[test]
fn match_nested_some_tuple_ok() {
    ok("o: (int, int)? = Some((1, 2))\nmatch o:\n    None: print(\"n\")\n    Some((a, b)): print(a + b)\n");
}

#[test]
fn match_tuple_with_literal_ok() {
    ok("t := (1, 2)\nmatch t:\n    (1, n): print(n)\n    _: print(0)\n");
}

#[test]
fn match_single_tuple_arm_is_exhaustive() {
    // A tuple pattern of all bindings is irrefutable → exhaustive with one arm.
    ok("t := (1, 2)\nmatch t:\n    (a, b): print(a + b)\n");
}

#[test]
fn match_tuple_with_literal_needs_wildcard() {
    rejects("t := (1, 2)\nmatch t:\n    (1, n): print(n)\n", "non-exhaustive");
}

#[test]
fn match_tuple_wrong_arity_rejected() {
    rejects("t := (1, 2)\nmatch t:\n    (a, b, c): print(a)\n", "element");
}

#[test]
fn match_nested_tuple_element_type_mismatch_rejected() {
    rejects("t := (\"x\", 2)\nmatch t:\n    (s, n): m: int = s\n", "");
}

#[test]
fn match_nested_nullary_variant_rejected() {
    // `Cons(h, None)`-style: a nested nullary variant isn't supported; the checker guides the user.
    let src = "enum L:\n    Nil\n    Cons(int, L)\n\
               fn f(x: L):\n    match x:\n        Nil: print(\"e\")\n        Cons(h, Nil): print(h)\n";
    rejects(src, "nested nullary-variant");
}


// ===== map iteration in for (gap #14) =====

#[test]
fn for_over_map_binds_key() {
    ok("m := {\"a\": 1}\nfor k in m:\n    s: str = k\n    print(s)\n");
}

#[test]
fn for_over_map_key_value() {
    ok("m := {\"a\": 1}\nfor k, v in m:\n    s: str = k\n    n: int = v\n    print(\"{s}{n}\")\n");
}

#[test]
fn for_over_map_value_type_is_v() {
    // The value binding has the map's value type — assigning to a mismatched slot is rejected.
    rejects("m := {\"a\": 1}\nfor k, v in m:\n    s: str = v\n", "");
}

#[test]
fn for_kv_over_list_rejected() {
    rejects("xs := [1,2,3]\nfor a, b in xs:\n    print(a)\n", "requires a map");
}

#[test]
fn for_kv_over_range_rejected() {
    rejects("for a, b in 0..3:\n    print(a)\n", "range");
}

#[test]
fn for_over_int_still_rejected() {
    rejects("for x in 5:\n    print(x)\n", "cannot iterate over int");
}


// ===== break / continue =====

#[test]
fn break_outside_loop_rejected() {
    rejects("break\n", "break outside loop");
}

#[test]
fn continue_outside_loop_rejected() {
    rejects("continue\n", "continue outside loop");
}

#[test]
fn break_continue_in_for_ok() {
    ok("for i in 0..5:\n    if i == 3: break\n    continue\n");
}

#[test]
fn break_continue_in_while_ok() {
    ok("while true:\n    break\n");
    ok("c := 0\nwhile c < 5:\n    c += 1\n    continue\n");
}

#[test]
fn break_in_if_in_loop_ok() {
    ok("for i in 0..5:\n    if i == 3:\n        break\n");
}

#[test]
fn break_after_loop_rejected() {
    // `loop_depth` is decremented when the loop body ends, so a `break` *after* the loop is illegal.
    rejects("for i in 0..3:\n    print(i)\nbreak\n", "break outside loop");
}

#[test]
fn nested_loops_break_legal_in_both() {
    // Nested loops: an inner break/continue is legal, and the outer loop is still a loop afterward.
    ok("for i in 0..3:\n    for j in 0..3:\n        break\n    continue\n");
}

// NOTE on functions/closures defined inside a loop: `check_fn_body`/`infer_closure` SAVE-ZERO-
// RESTORE `loop_depth` so a `break`/`continue` in a nested function/closure body can't see an
// outer loop. This rule is defensive: the current language can't route a checked statement-form
// `break` into such a body (nested `fn` bodies aren't type-checked, and closure bodies are single
// expressions that can't hold statements), so no source program exercises it through the checker.
// The compiler is the enforcing layer — see its `break outside loop` CompileError on an empty
// loop stack (closures compile in their own `FnComp` with an empty `loops` stack).

// ===== map / dictionary (gap #5) =====

#[test]
fn map_literal_infers_str_int() {
    // A `map[str, int]` annotation must accept a `{"a": 1}` literal.
    ok("m: map[str, int] = {\"a\": 1, \"b\": 2}\n");
}

#[test]
fn empty_map_assignable_to_any_map() {
    ok("m: map[str, int] = {}\n");
}

#[test]
fn map_index_read_is_value_type() {
    // `m["a"]` has the value type (int) — pushing it into an int slot must check clean.
    ok("m := {\"a\": 1}\nx: int = m[\"a\"]\n");
}

#[test]
fn map_index_assign_ok() {
    ok("m := {\"a\": 1}\nm[\"c\"] = 3\n");
}

#[test]
fn map_index_assign_wrong_value_type_rejected() {
    rejects("m := {\"a\": 1}\nm[\"c\"] = \"x\"\n", "cannot assign");
}

#[test]
fn map_index_wrong_key_type_rejected() {
    // String map keyed by an int — incompatible key.
    rejects("m := {\"a\": 1}\ny := m[5]\n", "key");
}

#[test]
fn float_map_key_literal_rejected() {
    rejects("m := {1.0: 2}\n", "hashable");
}

#[test]
fn float_map_key_annotation_rejected() {
    rejects("m: map[float, int] = {}\n", "hashable");
}

#[test]
fn heterogeneous_map_values_rejected() {
    rejects("m := {\"a\": 1, \"b\": \"x\"}\n", "differ");
}

#[test]
fn heterogeneous_map_keys_rejected() {
    rejects("m := {\"a\": 1, 2: 3}\n", "differ");
}

#[test]
fn int_and_bool_map_keys_ok() {
    ok("m: map[int, str] = {1: \"a\"}\n");
    ok("m: map[bool, int] = {true: 1}\n");
}

#[test]
fn map_keys_method_is_list_of_key() {
    ok("m := {\"a\": 1}\nks: list[str] = m.keys()\n");
}

#[test]
fn map_values_method_is_list_of_value() {
    ok("m := {\"a\": 1}\nvs: list[int] = m.values()\n");
}

#[test]
fn map_get_method_is_option_of_value() {
    ok("m := {\"a\": 1}\no: Option[int] = m.get(\"a\")\n");
}

#[test]
fn map_has_method_is_bool() {
    ok("m := {\"a\": 1}\nb: bool = m.has(\"a\")\n");
}

#[test]
fn map_len_method_is_int() {
    ok("m := {\"a\": 1}\nn: int = m.len()\n");
}

#[test]
fn map_remove_method_is_option_of_value() {
    ok("m := {\"a\": 1}\nr: Option[int] = m.remove(\"a\")\n");
}

#[test]
fn map_unknown_method_rejected() {
    rejects("m := {\"a\": 1}\nm.frobnicate()\n", "no method");
}

// ===== gap #8: tuples + multi-return + destructuring =====

#[test]
fn tuple_literal_infers_tuple_type() {
    ok("t := (1, 2)\nx := t.0 + t.1\n");
}

#[test]
fn tuple_return_type_matching_ok() {
    ok("fn pair() -> (int, int):\n    return (3, 4)\n");
}

#[test]
fn tuple_return_type_mismatch_rejected() {
    rejects(
        "fn pair() -> (int, int):\n    return (3, \"x\")\n",
        "expected return type",
    );
}

#[test]
fn destructure_binds_element_types() {
    // `a` is int, so `a + 1` type-checks; `b` is str, so `b + \"!\"` does too.
    ok("fn pair() -> (int, str):\n    return (1, \"x\")\nfn main():\n    a, b := pair()\n    c := a + 1\n    d := b + \"!\"\n");
}

#[test]
fn destructure_arity_mismatch_rejected() {
    rejects(
        "fn pair() -> (int, int):\n    return (1, 2)\nfn main():\n    a, b, c := pair()\n",
        "destructuring binds 3",
    );
}

#[test]
fn destructure_non_tuple_rejected() {
    rejects("fn main():\n    a, b := 5\n", "cannot destructure non-tuple");
}

#[test]
fn tuple_element_typed() {
    ok("t := (1, \"x\")\nn := t.0 + 1\ns := t.1 + \"!\"\n");
}

#[test]
fn tuple_element_out_of_range_rejected() {
    rejects("t := (1, 2)\nx := t.2\n", "has no element '.2'");
}

// ===== M8-M3: native trio std.process / std.fs / std.time signatures =====

#[test]
fn native_process_cmd_returns_result_str() {
    entry_ok("import std.process\nfn main():\n    match process.cmd(\"echo hi\"):\n        Ok(s): print(s)\n        Err(e): print(e)\n");
}

#[test]
fn native_process_cmd_arg_must_be_str() {
    entry_rejects(
        "import std.process\nfn main():\n    print(process.cmd(5))\n",
        "argument 1 of 'cmd'",
    );
}

#[test]
fn native_fs_predicates_are_bool_and_size_is_result_int() {
    entry_ok("import std.fs\nfn main():\n    b: bool = fs.is_file(\"x\")\n    e: bool = fs.exists(\"x\")\n    match fs.size(\"x\"):\n        Ok(n): print(str(n))\n        Err(m): print(m)\n");
}

#[test]
fn native_fs_list_dir_returns_result_list_str() {
    entry_ok("import std.fs\nfn main():\n    match fs.list_dir(\".\"):\n        Ok(xs): print(\",\".join(xs))\n        Err(e): print(e)\n");
}

#[test]
fn native_fs_unknown_member_rejected() {
    entry_rejects(
        "import std.fs\nfn main():\n    print(fs.touch(\"x\"))\n",
        "has no member 'touch'",
    );
}

#[test]
fn native_time_now_is_int_monotonic_is_float() {
    entry_ok("import std.time\nfn main():\n    t: int = time.now()\n    m: float = time.monotonic()\n    time.sleep_ms(0)\n    s: str = time.format(t)\n    print(s)\n");
}

#[test]
fn native_time_format_arg_must_be_int() {
    entry_rejects(
        "import std.time\nfn main():\n    print(time.format(\"x\"))\n",
        "argument 1 of 'format'",
    );
}

// ===== M8-M5: type-directed json.decode[T] =====

#[test]
fn json_decode_into_struct_is_result_of_struct() {
    entry_ok("import std.json\nstruct P:\n    x: int\n    y: int\nfn main():\n    match json.decode[P](\"x\"):\n        Ok(p): print(str(p.x))\n        Err(e): print(e)\n");
}

#[test]
fn json_decode_into_typed_map_and_list() {
    entry_ok("import std.json\nfn main():\n    a := json.decode[map[str, int]](\"x\")\n    b := json.decode[list[float]](\"y\")\n    print(\"ok\")\n");
}

#[test]
fn json_decode_scalar_result_type_flows() {
    entry_ok("import std.json\nfn main():\n    match json.decode[int](\"3\"):\n        Ok(n): print(str(n + 1))\n        Err(e): print(e)\n");
}

#[test]
fn json_decode_source_must_be_str() {
    entry_rejects(
        "import std.json\nfn main():\n    print(json.decode[int](5))\n",
        "decode source must be str",
    );
}

#[test]
fn json_decode_rejects_function_target() {
    entry_rejects(
        "import std.json\nfn main():\n    x := json.decode[fn(int) -> int](\"x\")\n",
        "cannot decode into",
    );
}

#[test]
fn json_decode_rejects_unknown_target_type() {
    entry_rejects(
        "import std.json\nfn main():\n    x := json.decode[Nope](\"x\")\n",
        "unknown type 'Nope'",
    );
}

#[test]
fn json_decode_rejects_recursive_struct() {
    entry_rejects(
        "import std.json\nstruct Node:\n    val: int\n    next: Node?\nfn main():\n    x := json.decode[Node](\"x\")\n",
        "recursive struct",
    );
}

#[test]
fn json_decode_rejects_map_with_non_str_key() {
    entry_rejects(
        "import std.json\nfn main():\n    x := json.decode[map[int, int]](\"x\")\n",
        "map keys must be str",
    );
}
