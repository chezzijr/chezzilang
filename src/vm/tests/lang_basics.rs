//! VM tests: arithmetic, superinstructions, display, control flow, closures, `?`, for, match,
//! field/index, builtins, interpolation, generators.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

// ----- arithmetic -----

#[test]
fn int_div_truncates() {
    assert_eq!(run("print(7 / 2)"), "3\n");
    assert_eq!(run("print(-7 / 2)"), "-3\n"); // Rust trunc-toward-zero, matching interp
}

#[test]
fn int_overflow_is_error_not_wrap() {
    // A wrapping VM would print a negative number; we must error like the interpreter.
    assert!(run_err("print(9223372036854775807 + 1)").contains("integer overflow in Add"));
}

#[test]
fn int_min_neg_and_div_overflow() {
    // The two other unrepresentable results: -i64::MIN and i64::MIN / -1. Both must error.
    let neg = "fn main():\n    x := -9223372036854775807 - 1\n    print(-x)\nmain()\n";
    assert!(run_err(neg).contains("integer overflow"));
    let div = "fn main():\n    x := -9223372036854775807 - 1\n    print(x / -1)\nmain()\n";
    assert!(run_err(div).contains("integer overflow"));
}

#[test]
fn float_promotion_when_either_side_float() {
    assert_eq!(run("print(1 + 2.0)"), "3.0\n");
    assert_eq!(run("print(7.0 / 2.0)"), "3.5\n");
    assert_eq!(run("print(7 / 2.0)"), "3.5\n");
}

#[test]
fn division_and_modulo_by_zero_error() {
    // INTEGER division/modulo by zero still faults (unchanged).
    assert_eq!(run_err("print(1 / 0)"), "division by zero");
    assert_eq!(run_err("print(1 % 0)"), "modulo by zero");
    // Float by zero is IEEE-754 — never faults; produces inf/-inf/NaN.
    assert_eq!(run("print(1.0 / 0.0)"), "inf\n");
    assert_eq!(run("print(5.0 % 0.0)"), "NaN\n");
}

#[test]
fn float_div_mod_by_zero_is_ieee() {
    assert_eq!(run("print(1.0 / 0.0)"), "inf\n");
    assert_eq!(run("print(-1.0 / 0.0)"), "-inf\n");
    assert_eq!(run("print(0.0 / 0.0)"), "NaN\n");
    assert_eq!(run("print(5.0 % 0.0)"), "NaN\n");
}

#[test]
fn int_div_mod_still_fault() {
    // Regression tripwire: INTEGER arithmetic stays total-faulting.
    assert_eq!(run_err("print(1 / 0)"), "division by zero");
    assert_eq!(run_err("print(1 % 0)"), "modulo by zero");
}

#[test]
fn string_concatenation() {
    assert_eq!(run(r#"print("a" + "b" + "c")"#), "abc\n");
}

#[test]
fn comparison_and_equality_across_numeric_types() {
    assert_eq!(run("print(1 < 2.0)"), "true\n");
    assert_eq!(run("print(2 == 2.0)"), "true\n");
    assert_eq!(run("print(2 != 3)"), "true\n");
    assert_eq!(run(r#"print("a" < "b")"#), "true\n");
    // Cross-type equality is false, never an error.
    assert_eq!(run(r#"print(1 == "1")"#), "false\n");
}

#[test]
fn arithmetic_type_error_message() {
    assert!(run_err(r#"print(1 + "x")"#).contains("cannot apply Add to int and str"));
}

// ----- M19 superinstructions: operands are LOCALS (inside `fn`), so the fused ops actually
// execute (top-level `:=` is a global → `GetGlobal`, never fused). -----

#[test]
fn superinstruction_loop_sum_correct() {
    // `i < 5` → BinLocalConst{Lt}; `total += i` → BinLocalLocal{Add}+SetLocal; `i += 1` → IncLocal.
    let src = "fn main():\n    total := 0\n    i := 0\n    while i < 5:\n        total += i\n        i += 1\n    print(total)\nmain()";
    assert_eq!(run(src), "10\n");
}

#[test]
fn compound_assign_lowers_to_inc_local_end_to_end() {
    // `x OP= v` lowers through the plain binary opcode, so `i += 1` fuses to `Op::IncLocal` and
    // `total += i` fuses to `Op::BinLocalLocal{Add}` (the M19 superinstructions).
    let src = "fn main():\n    total := 0\n    i := 0\n    while i < 5:\n        total += i\n        i += 1\n    print(total)\nmain()";
    let module = parser::parse(lexer::tokenize(src).unwrap()).unwrap();
    let program = crate::compiler::compile_module_standalone(&module).unwrap();
    let main_proto = program
        .protos
        .iter()
        .find(|p| p.name == "main")
        .expect("a `main` proto exists");
    assert!(
        main_proto
            .code
            .iter()
            .any(|op| matches!(op, Op::IncLocal { delta: 1, .. })),
        "expected an IncLocal in {:?}",
        main_proto.code
    );
    assert!(
        main_proto.code.iter().any(|op| matches!(
            op,
            Op::BinLocalLocal {
                kind: crate::vm::op::BinKind::Add,
                ..
            }
        )),
        "expected a BinLocalLocal{{Add}} in {:?}",
        main_proto.code
    );
}

#[test]
fn superinstruction_div_mod_by_zero_via_locals() {
    // BinLocalLocal fast path must raise the same message as `arith`.
    assert_eq!(
        run_err("fn main():\n    x := 1\n    y := 0\n    print(x / y)\nmain()"),
        "division by zero"
    );
    assert_eq!(
        run_err("fn main():\n    x := 1\n    y := 0\n    print(x % y)\nmain()"),
        "modulo by zero"
    );
}

#[test]
fn superinstruction_overflow_via_inc_and_mul() {
    // IncLocal overflow.
    assert!(
        run_err("fn main():\n    i := 9223372036854775807\n    i += 1\n    print(i)\nmain()")
            .contains("integer overflow in Add")
    );
    // BinLocalConst Mul overflow.
    assert!(
        run_err("fn main():\n    x := 9223372036854775807\n    print(x * 2)\nmain()")
            .contains("integer overflow in Mul")
    );
}

#[test]
fn superinstructions_run_under_parallel_engine() {
    // Drives BinLocalConst / BinLocalLocal / IncLocal through the M:N engine (reduction path).
    let out = run_capture("fn main():\n    total := 0\n    i := 0\n    while i < 1000:\n        total += i\n        i += 1\n    print(total)\nmain()").unwrap();
    assert_eq!(out, "499500\n");
}

// ----- and / or short-circuit -----

#[test]
fn and_short_circuits_rhs() {
    // If `and` did not short-circuit, the `1/0` would raise a div-by-zero error.
    assert_eq!(run("print(false and (1 / 0 == 0))"), "false\n");
}

#[test]
fn or_short_circuits_rhs() {
    assert_eq!(run("print(true or (1 / 0 == 0))"), "true\n");
}

#[test]
fn logical_operand_must_be_bool() {
    assert_eq!(run_err("print(1 and true)"), "expected bool, found int");
}

// ----- display formatting -----

#[test]
fn float_display_keeps_one_decimal_for_integral() {
    assert_eq!(run("print(5.0)"), "5.0\n");
    assert_eq!(run("print(5.5)"), "5.5\n");
    assert_eq!(run("print(2.5 * 2.0)"), "5.0\n");
}

#[test]
fn list_display() {
    assert_eq!(run("print([1, 2, 3])"), "[1, 2, 3]\n");
    assert_eq!(run("print([])"), "[]\n");
    assert_eq!(run(r#"print(["a", "b"])"#), "['a', 'b']\n");
}

#[test]
fn struct_display_in_declaration_order() {
    let src = "\
struct Point:
    x: int
    y: int
print(Point(3, 4))";
    assert_eq!(run(src), "Point(x=3, y=4)\n");
}

#[test]
fn enum_display_nullary_and_payload() {
    let src = "\
enum Shape:
    Circle(int)
    Dot
print(Shape.Circle(2))
print(Shape.Dot)";
    assert_eq!(run(src), "Circle(2)\nDot\n");
}

#[test]
fn print_joins_args_with_space() {
    assert_eq!(run(r#"print("a", 1, true)"#), "a 1 true\n");
}

#[test]
fn print_end_suppresses_newline() {
    assert_eq!(run("print(\"a\", end=\"\")\n"), "a");
}

#[test]
fn print_sep_joins_args() {
    assert_eq!(run("print(\"a\", \"b\", sep=\"-\")\n"), "a-b\n");
}

#[test]
fn print_sep_and_end_together() {
    assert_eq!(run("print(\"a\", \"b\", sep=\"-\", end=\"!\")\n"), "a-b!");
}

#[test]
fn print_default_unchanged_with_no_kwargs() {
    // The no-kwarg path must stay byte-identical: space-joined, newline-terminated.
    assert_eq!(run("print(\"a\", \"b\")\n"), "a b\n");
    assert_eq!(run("print(\"a\")\n"), "a\n");
}

#[test]
fn print_end_with_runtime_str_expr() {
    // `end` can be a runtime str expression, not just a literal.
    assert_eq!(run("e := \"?\"\nprint(\"a\", end=e)\n"), "a?");
}

#[test]
fn print_only_end_keeps_default_space_sep() {
    assert_eq!(run("print(\"a\", \"b\", end=\"\")\n"), "a b");
}

// ----- functions / control flow -----

#[test]
fn nested_calls_and_return() {
    let src = "\
fn add(a: int, b: int) -> int:
    return a + b
fn main():
    print(add(add(1, 2), 3))
main()";
    assert_eq!(run(src), "6\n");
}

#[test]
fn forward_reference_between_top_level_fns() {
    // `main` is defined before `helper`; hoisting must make the forward ref resolve.
    let src = "\
fn main():
    print(helper(21))
fn helper(n: int) -> int:
    return n * 2
main()";
    assert_eq!(run(src), "42\n");
}

#[test]
fn infinite_recursion_hits_depth_limit() {
    let src = "\
fn loop(n: int) -> int:
    return loop(n + 1)
fn main():
    print(loop(0))
main()";
    assert!(run_err(src).contains("maximum call depth"));
}

/// M10-G1: a self-referential `Stringable` `str` must hit the depth guard, not loop forever.
#[test]
fn self_referential_stringable_hits_depth_limit() {
    let src = "struct Loop:\n    n: int\n    fn str(self) -> str:\n        return str(self)\nprint(Loop(1))\n";
    assert!(run_err(src).contains("maximum call depth"));
}

#[test]
fn if_elif_else() {
    let src = "\
fn classify(n: int) -> str:
    if n < 0:
        return \"neg\"
    elif n == 0:
        return \"zero\"
    else:
        return \"pos\"
fn main():
    print(classify(-1))
    print(classify(0))
    print(classify(5))
main()";
    assert_eq!(run(src), "neg\nzero\npos\n");
}

#[test]
fn while_loop_with_compound_assign() {
    let src = "\
fn main():
    i := 0
    total := 0
    while i < 5:
        total += i
        i += 1
    print(total)
main()";
    assert_eq!(run(src), "10\n");
}

#[test]
fn unary_neg_and_not() {
    assert_eq!(run("print(-5)"), "-5\n");
    assert_eq!(run("print(not true)"), "false\n");
    assert_eq!(run_err("print(-true)"), "cannot apply Neg to bool");
}

// ----- closures -----

#[test]
fn closure_shares_captured_binding() {
    // Uniform by-reference capture: the closure shares the binding of `n`, so a reassignment made
    // after the closure was created IS visible when the closure later runs (`n = 20` → `x + 20`).
    // (Under the old value-semantics rule this snapshotted `n = 10` and printed `15`.)
    let src = "\
fn make() -> fn(int) -> int:
    n := 10
    f := fn(x: int) -> int: x + n
    n = 20
    return f
fn main():
    g := make()
    print(g(5))
main()";
    assert_eq!(run(src), "25\n");
}

#[test]
fn closure_captures_distinct_environments() {
    let src = "\
fn adder(n: int) -> fn(int) -> int:
    return fn(x: int) -> int: x + n
fn main():
    add10 := adder(10)
    add100 := adder(100)
    print(add10(1))
    print(add100(1))
main()";
    assert_eq!(run(src), "11\n101\n");
}

// ----- ? operator -----

#[test]
fn try_unwraps_ok() {
    let src = "\
fn safe_div(a: int, b: int) -> int!:
    if b == 0:
        return !\"divide by zero\"
    return ?(a / b)
fn main():
    r := safe_div(10, 2)?
    print(r)
main()";
    assert_eq!(run(src), "5\n");
}

#[test]
fn try_propagates_err_to_caller() {
    let src = "\
fn safe_div(a: int, b: int) -> int!:
    if b == 0:
        return !\"zero\"
    return ?(a / b)
fn use() -> int!:
    r := safe_div(1, 0)?
    return ?(r + 1)
fn main():
    match use():
        ?v: print(\"ok {v}\")
        !e: print(\"err {e}\")
main()";
    assert_eq!(run(src), "err zero\n");
}

#[test]
fn try_on_non_result_is_error() {
    let src = "\
fn f() -> int:
    x := (5)?
    return x";
    // Reaching `?` on an int is a runtime error.
    assert!(
        run_err(&format!("{src}\nfn main():\n    print(f())\nmain()"))
            .contains("'?' expects a `T?` or `T!E` value, found int")
    );
}

#[test]
fn top_level_try_err_is_unhandled_error() {
    // A `?` at the top level whose Err reaches the top is an unhandled error (no main needed).
    assert_eq!(run_err(r#"x := (!"oops")?"#), "unhandled error: oops");
}

#[test]
fn top_level_try_err_renders_a_one_tuple_with_trailing_comma() {
    // display_guarded (the error-message renderer) must carry the same trailing-comma rule as
    // stringify_obj_into (the str()/print path) — TICKET-069.
    assert_eq!(run_err(r#"x := (!(1,))?"#), "unhandled error: (1,)");
}

#[test]
fn top_level_try_err_reports_real_line() {
    // The `?` is on line 3 — report there, not at a hard-coded line 1.
    let e = run_capture("fn d() -> int!:\n    return !\"x\"\nx := d()?\n").unwrap_err();
    assert_eq!(e.message, "unhandled error: x");
    assert_eq!(e.span.line, 3, "expected the `?` line, got {}", e.span.line);
}

#[test]
fn bare_discarded_result_call_at_top_level_does_not_abort() {
    // TICKET-038: a bare `Result`-returning call at module top level, with its value neither
    // bound nor `?`-propagated, is expected to behave like Rust's `#[must_use]` lint on an
    // ignored `Result` — warn (checker, not asserted here) and keep running, rc=0 — not abort
    // the program. It currently aborts with `unhandled error`.
    let src = "\
fn g() -> int!str:
    return !\"boom\"
g()
print(\"after\")";
    assert_eq!(run_capture(src).unwrap(), "after\n");
}

/// L3-1: a pure-panic assertion helper with NO return annotation (`fn boom(): panic(...)`) now
/// type-checks (infers `-> nil`) — verify the CALLER runs and FAULTS with the panic message on
/// BOTH engines, and that a `recover:`-wrapped call yields `Err("x")`.
#[test]
fn inline_panic_body_faults_both_engines() {
    let src = "fn boom(): panic(\"x\")\nfn main():\n    print(\"start\")\n    boom()\nmain()\n";
    let (s_out, s_res) = run_program(src);
    let (m_out, m_res) = run_program(src);
    assert_eq!(s_out, "start\n");
    assert_eq!(m_out, "start\n");
    assert_eq!(s_res.unwrap_err().message, "x");
    assert_eq!(m_res.unwrap_err().message, "x");
    // A `recover:` around the call catches it as `Err("x")` — recoverable.
    let rec = "fn boom(): panic(\"x\")\nfn main():\n    r := recover:\n        boom()\n        0\n    match r:\n        ?v: print(\"ok {v}\")\n        !e: print(\"caught {e.message()}\")\nmain()\n";
    assert_eq!(run_capture(rec).unwrap(), "caught x\n");
    assert_eq!(run_capture(rec).unwrap(), "caught x\n");
}

// ----- for loops -----

#[test]
fn for_range_sums() {
    let src = "\
fn main():
    total := 0
    for i in 0..1000:
        total += i
    print(total)
main()";
    assert_eq!(run(src), "499500\n");
}

#[test]
fn for_range_is_lazy_not_materialized() {
    // A billion-element range would exhaust memory if materialized; the lazy counting loop
    // returns on the first iteration instantly.
    let src = "\
fn first() -> int:
    for i in 0..1000000000:
        return i
    return -1
fn main():
    print(first())
main()";
    assert_eq!(run(src), "0\n");
}

#[test]
fn for_over_list() {
    let src = "\
fn main():
    total := 0
    for x in [10, 20, 30]:
        total += x
    print(total)
main()";
    assert_eq!(run(src), "60\n");
}

#[test]
fn for_over_non_iterable_errors() {
    assert!(run_err("for x in 5:\n    print(x)").contains("cannot iterate over int"));
}

// ----- match -----

#[test]
fn match_binds_payload() {
    let src = "\
enum Shape:
    Circle(int)
    Square(int)
fn area(s: Shape) -> int:
    match s:
        Shape.Circle(r): return r * r * 3
        Shape.Square(n): return n * n
fn main():
    print(area(Shape.Circle(2)))
    print(area(Shape.Square(3)))
main()";
    assert_eq!(run(src), "12\n9\n");
}

#[test]
fn match_no_arm_is_error() {
    let src = "\
enum Color:
    Red
    Green
    Blue
fn name(c: Color) -> str:
    match c:
        Color.Red: return \"r\"
        Color.Green: return \"g\"
fn main():
    print(name(Color.Blue))
main()";
    assert_eq!(run_err(src), "no match arm for variant 'Blue'");
}

#[test]
fn match_bare_ident_on_non_enum_binds_value() {
    // A bare top-level identifier against a non-enum value is a binding capturing the whole
    // value (the checker permits this only for literal scrutinees) — not an enum-match error.
    let src = "\
fn main():
    match 5:
        x: print(x)
main()";
    assert_eq!(run(src), "5\n");
}

// ----- struct patterns in `match` (L2) -----

#[test]
fn struct_match_binds_fields() {
    let src = "\
struct Point:
    x: int
    y: int
fn main():
    p := Point(1, 2)
    match p:
        Point(a, b):
            print(a)
            print(b)
main()
";
    assert_mc_parity(src, "1\n2\n");
}

#[test]
fn struct_match_generic_and_nested() {
    // Generic field (`Box[T]` → `v:int`) + nested struct field (`Line(Point(x,y), _)`) both bind.
    let src = "\
struct Box[T]:
    v: T
struct Point:
    x: int
    y: int
struct Line:
    a: Point
    b: Point
fn main():
    match Box(7):
        Box(v): print(v)
    l := Line(Point(3, 4), Point(5, 6))
    match l:
        Line(Point(x, y), _): print(x + y)
main()
";
    assert_mc_parity(src, "7\n7\n");
}

#[test]
fn struct_match_literal_field_refutable() {
    // A literal-field arm `Point(0, y)` is refutable → needs a trailing `_`.
    let src = "\
struct Point:
    x: int
    y: int
fn describe(p: Point) -> str:
    match p:
        Point(0, y): return \"on-y-axis\"
        _: return \"off-axis\"
fn main():
    print(describe(Point(0, 5)))
    print(describe(Point(3, 5)))
main()
";
    assert_mc_parity(src, "on-y-axis\noff-axis\n");
}

#[test]
fn struct_match_generic_catchall_keeps_targs() {
    // Regression (bug #1): a generic-struct scrutinee `Box[int]` with a refutable field arm plus a
    // whole-value catch-all `rest:` — the catch-all binding must keep the scrutinee's type args so
    // `rest.v` resolves to the INSTANTIATED field type `int`, not the unsubstituted param `T`.
    let src = "\
struct Box[T]:
    v: T
fn f(b: Box[int]) -> int:
    match b:
        Box(0): return 100
        rest: return rest.v + 1
fn main():
    print(f(Box(0)))
    print(f(Box(41)))
main()
";
    assert_mc_parity(src, "100\n42\n");
}

#[test]
fn struct_match_catchall_name_shadows_struct() {
    // Regression (bug #2): a whole-value catch-all binding whose NAME happens to resolve to another
    // in-scope struct (`Node`) must bind the whole scrutinee — not be mis-lowered as a zero-field
    // struct destructure that binds nothing (which panicked the compiler: `global has no slot`).
    let src = "\
struct Point:
    x: int
    y: int
struct Node:
    v: int
fn describe(p: Point) -> str:
    match p:
        Point(1, y): return \"one-{y}\"
        Node: return \"other-{Node.x}\"
    return \"\"
fn main():
    print(describe(Point(1, 2)))
    print(describe(Point(3, 4)))
main()
";
    assert_mc_parity(src, "one-2\nother-3\n");
}

// ----- field / index -----

#[test]
fn index_list_and_out_of_bounds() {
    assert_eq!(run("print([10, 20, 30][1])"), "20\n");
    assert_eq!(run_err("print([1, 2][5])"), "index 5 out of bounds (len 2)");
}

#[test]
fn index_string_returns_char() {
    assert_eq!(run(r#"print("hello"[1])"#), "e\n");
}

#[test]
fn index_assign_mutates_in_place() {
    assert_eq!(
        run("xs := [1, 2, 3]\nxs[1] = 9\nprint(xs)\n"),
        "[1, 9, 3]\n"
    );
}

#[test]
fn index_compound_assign() {
    assert_eq!(
        run("xs := [1, 2, 3]\nxs[0] += 5\nxs[2] -= 1\nprint(xs)\n"),
        "[6, 2, 2]\n"
    );
}

#[test]
fn compound_assign_all_ops_all_targets_parity() {
    // Every compound op across ident / index / field / map-key targets; VM == interp == --parallel.
    let src = "\
struct P:
    f: int
fn main():
    x := 100
    x *= 3
    x /= 2
    x %= 40
    x &= 12
    x |= 1
    x ^= 5
    x <<= 2
    x >>= 1
    print(x)
    xs := [8]
    xs[0] *= 2
    xs[0] <<= 1
    print(xs)
    p := P(7)
    p.f *= 6
    p.f %= 5
    print(p.f)
    m := {\"k\": 10}
    m[\"k\"] |= 4
    m[\"k\"] *= 2
    print(m[\"k\"])
main()";
    // x: 100*3=300 /2=150 %40=30 &12=12 |1=13 ^5=8 <<2=32 >>1=16, hand-computed under Chezzi's
    // OWN int semantics (`/` truncates by design, docs/syntax.md:1982) — NOT CPython-verified:
    // measured, CPython's `/=` always promotes to float (`100*3/2` → `150.0`), so `x &= 12` two
    // steps later raises `TypeError: unsupported operand type(s) for &=: 'float' and 'int'`; the
    // sequence can't even run to completion under CPython's semantics, let alone match this golden.
    assert_eq!(run_capture(src).expect("vm"), "16\n[32]\n2\n28\n");
}

#[test]
fn membership_in_runtime_parity() {
    // list / set / map-key / substring, true + false; VM == interp == --parallel.
    let src = "\
fn main():
    print(2 in [1, 2, 3])
    print(9 in [1, 2, 3])
    s := {10, 20, 30}
    print(20 in s)
    print(99 in s)
    m := {\"a\": 1, \"b\": 2}
    print(\"a\" in m)
    print(\"z\" in m)
    print(\"ell\" in \"hello\")
    print(\"xyz\" in \"hello\")
main()";
    let vm_out = run_capture(src).expect("vm");
    assert_eq!(
        vm_out,
        "true\nfalse\ntrue\nfalse\ntrue\nfalse\ntrue\nfalse\n"
    );
}

#[test]
fn tuple_swap_runtime_parity() {
    // vars, list elements (same indices appear on both sides → proves RHS-first eval),
    // and struct fields; VM == interp == --parallel.
    let src = "\
struct P:
    x: int
    y: int
fn main():
    a := 1
    b := 2
    a, b = b, a
    print(a)
    print(b)
    data := [10, 20, 30]
    data[0], data[2] = data[2], data[0]
    print(data)
    p := P(7, 9)
    p.x, p.y = p.y, p.x
    print(p.x)
    print(p.y)
main()";
    let vm_out = run_capture(src).expect("vm");
    assert_eq!(vm_out, "2\n1\n[30, 20, 10]\n9\n7\n");
}

#[test]
fn index_assign_out_of_bounds_errors() {
    assert_eq!(
        run_err("xs := [1, 2, 3]\nxs[5] = 0\n"),
        "index 5 out of bounds (len 3)"
    );
}

#[test]
fn field_assign_mutates_in_place() {
    let src = "\
struct P:
    x: int
    y: int
fn main():
    p := P(1, 2)
    p.x = 9
    print(p.x)
    print(p.y)
main()";
    assert_eq!(run(src), "9\n2\n");
}

#[test]
fn field_compound_assign() {
    let src = "\
struct P:
    x: int
fn main():
    p := P(10)
    p.x += 5
    p.x -= 3
    print(p.x)
main()";
    assert_eq!(run(src), "12\n");
}

#[test]
fn field_access_and_unknown_field() {
    let src = "\
struct P:
    x: int
    y: int
fn main():
    p := P(1, 2)
    print(p.x)
    print(p.y)
main()";
    assert_eq!(run(src), "1\n2\n");
}

#[test]
fn struct_method_call_binds_self() {
    let src = "\
struct Counter:
    n: int
    fn doubled(self) -> int:
        return self.n * 2
fn main():
    c := Counter(21)
    print(c.doubled())
main()";
    assert_eq!(run(src), "42\n");
}

// ----- builtins -----

#[test]
fn method_len() {
    assert_eq!(run("print([1, 2, 3].len())"), "3\n");
    assert_eq!(run(r#"print("hello".len())"#), "5\n");
}

#[test]
fn bytes_len_method() {
    assert_eq!(
        run("fn main():\n    b := b\"\\x01\\x02\\x03\"\n    print(b.len())\nmain()\n"),
        "3\n"
    );
}

#[test]
fn range_values_up_down_and_empty_vm() {
    assert_eq!(run("print(List(0..3))"), "[0, 1, 2]\n");
    assert_eq!(run("print(List(2..5))"), "[2, 3, 4]\n");
    assert_eq!(run("print(List(Range(0, 10, 2)))"), "[0, 2, 4, 6, 8]\n");
    assert_eq!(run("print(List(Range(1, 7, 3)))"), "[1, 4]\n");
    assert_eq!(
        run("print(List(Range(10, 0, -1)))"),
        "[10, 9, 8, 7, 6, 5, 4, 3, 2, 1]\n"
    );
    assert_eq!(run("print(List(Range(10, 2, -3)))"), "[10, 7, 4]\n");
    assert_eq!(run("print(List(Range(5, 5, 1)))"), "[]\n");
    assert_eq!(run("print(List(Range(5, 5, -1)))"), "[]\n");
    assert_eq!(run("print(List(Range(0, 10, -1)))"), "[]\n");
    assert_eq!(run("print(List(Range(10, 0, 1)))"), "[]\n");
}

#[test]
fn range_step_zero_faults_at_construction_vm() {
    let msg = run_err("print(Range(0, 5, 0))");
    assert!(msg.contains("Range() step cannot be zero"), "msg: {msg}");
}

#[test]
fn range_slice_of_a_list_with_a_step_vm() {
    assert_eq!(
        run("print(List(0..10)[Range(None, None, 2)])"),
        "[0, 2, 4, 6, 8]\n"
    );
    assert_eq!(run("print(List(0..10)[Range(1, 8, 3)])"), "[1, 4, 7]\n");
    assert_eq!(run("print(List(0..5).reversed())"), "[4, 3, 2, 1, 0]\n");
}

#[test]
fn builtin_casts() {
    assert_eq!(run(r#"print(int("42"))"#), "42\n");
    assert_eq!(run("print(float(3))"), "3.0\n");
    assert_eq!(run("print(str(5))"), "5\n");
    assert!(run_err(r#"print(int("notnum"))"#).contains("cannot parse 'notnum'"));
}

// ----- construction arity / nullary variant -----

#[test]
fn struct_arity_error() {
    let src = "\
struct Point:
    x: int
    y: int
fn main():
    p := Point(1)
main()";
    assert!(run_err(src).contains("struct 'Point' expects 2 field(s), got 1"));
}

#[test]
fn variant_arity_error() {
    assert!(
        run_err("enum E:\n    A(int)\nfn main():\n    x := E.A(1, 2)\nmain()")
            .contains("variant 'A' expects 1 value(s), got 2")
    );
}

#[test]
fn nullary_variant_used_as_value() {
    assert_eq!(run("print(None)"), "None\n");
    let src = "\
enum Light:
    On
    Off
fn main():
    print(Light.Off)
main()";
    assert_eq!(run(src), "Off\n");
}

// ----- string interpolation -----

#[test]
fn interpolation_and_literal_braces() {
    let src = "\
fn main():
    name := \"chezzi\"
    print(\"hi {name}, {{not interpolated}}\")
main()";
    assert_eq!(run(src), "hi chezzi, {not interpolated}\n");
}

// ----- or-patterns + nested nullary -----

/// A literal or-pattern (`1 | 2 | 3`) routes any alternative to the body; the interp agrees.
#[test]
fn vm_or_pattern_literals() {
    let src = "fn f(n: int) -> str:\n    return match n:\n        1 | 2 | 3: \"low\"\n        _: \"high\"\nprint(f(2))\nprint(f(5))\n";
    let out = run(src);
    assert_eq!(out, "low\nhigh\n");
}

/// A 3-variant enum or-pattern is exhaustive and matches each alternative; the interp agrees.
#[test]
fn vm_or_pattern_enum_variants() {
    let src = "enum Color:\n    Red\n    Green\n    Blue\nfn name(c: Color) -> str:\n    return match c:\n        Color.Red | Color.Green | Color.Blue: \"primary\"\nprint(name(Color.Green))\nprint(name(Color.Blue))\n";
    let out = run(src);
    assert_eq!(out, "primary\nprimary\n");
}

/// A binding or-pattern (`A(a) | B(a)`) writes `a` into the same slot regardless of alternative.
#[test]
fn vm_or_pattern_binding() {
    let src = "enum E:\n    A(int)\n    B(int)\nfn val(e: E) -> int:\n    return match e:\n        E.A(a) | E.B(a): a\nprint(val(E.A(7)))\nprint(val(E.B(9)))\n";
    let out = run(src);
    assert_eq!(out, "7\n9\n");
}

/// A guard on an or-pattern: `p | q if cond:` falls through to the next arm when the guard fails.
#[test]
fn vm_or_pattern_with_guard() {
    let src = "fn f(n: int) -> str:\n    return match n:\n        1 | 2 | 3 if n == 2: \"two\"\n        _: \"other\"\nprint(f(2))\nprint(f(1))\n";
    let out = run(src);
    assert_eq!(out, "two\nother\n");
}

/// A nested nullary variant (`Some(None)`) is a refutable variant match; the interp agrees.
#[test]
fn vm_nested_nullary_variant() {
    // Nested nullary `None` inside `Some(...)` is a refutable variant match: `Some(None)` matches
    // only the inner-none case; everything else falls to `_`. A single outer `Some` arm + `_` keeps
    // this CLI-valid (the checker allows one arm per outer variant), so the test reflects a program
    // a user can actually run, not just the checker-skipping runtime harness.
    let src = "fn f(oo: int??) -> str:\n    return match oo:\n        ?None: \"inner-none\"\n        _: \"other\"\nx: int?? = ?None\ny: int?? = ?(?5)\nprint(f(x))\nprint(f(y))\n";
    let out = run(src);
    assert_eq!(out, "inner-none\nother\n");
}

// ----- experimental generators (VM-only) -----

/// Milestone: a 3-yield counting generator drives a `for` loop, printing each value.
#[test]
fn vm_generator_basic_for_loop() {
    let src = "fn count() -> Iterator[int]:\n    yield 1\n    yield 2\n    yield 3\nfn main():\n    for x in count():\n        print(x)\nmain()\n";
    assert_eq!(run(src), "1\n2\n3\n");
}

/// Golden: the `examples/generators.chz` showcase (free-fn + struct-method generators, `for`,
/// explicit `.next()`, and an `Iterator[T]`-bounded generic) produces exactly this output.
#[test]
fn golden_generators_chz() {
    let out = run(include_str!("../../../examples/generators.chz"));
    assert_eq!(out, "0\n1\n2\n10\n11\n12\n0\n1\nNone\n5\n");
}

/// Driving a generator by explicit `.next()` yields `Some(v)` per yield, then `None` forever.
#[test]
fn vm_generator_explicit_next() {
    let src = "fn two() -> Iterator[int]:\n    yield 10\n    yield 20\nfn main():\n    g := two()\n    print(g.next())\n    print(g.next())\n    print(g.next())\n    print(g.next())\nmain()\n";
    assert_eq!(run(src), "10\n20\nNone\nNone\n");
}

/// A generator whose `yield` is never reached drains immediately: the `for` body never runs.
#[test]
fn vm_generator_never_yields() {
    let src = "fn empty() -> Iterator[int]:\n    if false:\n        yield 1\nfn main():\n    print(\"before\")\n    for x in empty():\n        print(x)\n    print(\"after\")\nmain()\n";
    assert_eq!(run(src), "before\nafter\n");
}

/// Q1: a generator with NO `-> Iterator[T]` annotation — element type inferred by strict-first-yield
/// — drives a `for` loop and RUNS identically on BOTH engines (serial + M:N), not just type-checks.
#[test]
fn vm_generator_inferred_no_annotation() {
    let src = "fn count() -> Iterator[int]:\n    yield 1\n    yield 2\n    yield 3\nfn main():\n    for x in count():\n        print(x)\nmain()\n";
    assert_eq!(run(src), "1\n2\n3\n");
    assert_eq!(run_capture(src).unwrap(), "1\n2\n3\n");
}

/// Q1: the struct-method arm of generator inference also RUNS — an un-annotated
/// `each` generator method infers `Iterator[int]` and drives a `for`.
#[test]
fn vm_generator_inferred_struct_method() {
    let src = "struct Box:\n    n: int\n    fn each(self) -> Iterator[int]:\n        i := 0\n        while i < self.n:\n            yield i\n            i = i + 1\nfn main():\n    b := Box(3)\n    for x in b.each():\n        print(x)\nmain()\n";
    assert_eq!(run(src), "0\n1\n2\n");
    assert_eq!(run_capture(src).unwrap(), "0\n1\n2\n");
}

/// Q1 golden: the inference-only `examples/generators_inferred.chz` showcase (free-fn + struct-method
/// generators, no `-> Iterator[T]` anywhere) produces exactly this output on BOTH engines.
#[test]
fn golden_generators_inferred_chz() {
    let src = include_str!("../../../examples/generators_inferred.chz");
    let expect = "0\n1\n2\n3\n4\n2\n4\n6\n";
    assert_eq!(run(src), expect);
    assert_eq!(run_capture(src).unwrap(), expect);
}

/// F3 path C: a PENDING generator passed DIRECTLY as a spawn arg now CROSSES the airlock BY VALUE
/// (deep copy) — the callee runs on its own copy instead of faulting.
#[test]
fn generator_passed_to_spawn_crosses_by_value() {
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "fn work(g: Iterator[int]):\n",
        "    print(\"hi\")\n",
        "fn main():\n",
        "    parallel:\n",
        "        spawn work(gen())\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).expect("serial"), "hi\n");
    assert_eq!(run_capture(src).expect("M:N"), "hi\n");
}

/// F3 path C: a generator nested inside a LIST spawn arg crosses BY VALUE too (the container recursion
/// in `to_wire` serializes the leaf generator instead of faulting).
#[test]
fn generator_in_list_arg_to_spawn_crosses_by_value() {
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "fn work(g: List[Iterator[int]]):\n",
        "    print(\"hi\")\n",
        "fn main():\n",
        "    parallel:\n",
        "        spawn work([gen()])\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).expect("serial"), "hi\n");
    assert_eq!(run_capture(src).expect("M:N"), "hi\n");
}

/// F3 path C: a generator stored into `Shared(...)` now crosses BY VALUE (deep copy into the box)
/// instead of faulting; the program runs to completion.
#[test]
fn generator_into_shared_crosses_by_value() {
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "fn main():\n",
        "    s := Shared(gen())\n",
        "    print(\"stored\")\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).expect("serial"), "stored\n");
    assert_eq!(run_capture(src).expect("M:N"), "stored\n");
}

/// F3 path C: a generator stored into `Atomic(...)` now crosses BY VALUE (deep copy into the box).
#[test]
fn generator_into_atomic_crosses_by_value() {
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "fn main():\n",
        "    a := Atomic(gen())\n",
        "    print(\"stored\")\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).expect("serial"), "stored\n");
    assert_eq!(run_capture(src).expect("M:N"), "stored\n");
}

/// F3 path C: a closure CAPTURING a local generator submitted to an `Executor` under `--parallel` now
/// crosses BY VALUE — the submitted task drives its own copy instead of faulting. Runs to completion.
#[test]
fn generator_captured_in_executor_submit_crosses_by_value() {
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "fn main():\n",
        "    g := gen()\n",
        "    ex := Executor()\n",
        "    ex.submit(fn(): print(g.next()))\n",
        "    ex.shutdown()\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).expect("M:N"), "1\n");
}

/// F3 path C: an `Executor` task that RETURNS a generator under `--parallel` now crosses the return
/// value BY VALUE (deep copy back to the parent). The Executor discards task returns, so no output.
#[test]
fn generator_returned_from_executor_task_crosses_by_value() {
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "fn make() -> Iterator[int]:\n",
        "    return gen()\n",
        "fn main():\n",
        "    ex := Executor()\n",
        "    ex.submit(make)\n",
        "    ex.shutdown()\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).expect("M:N"), "");
}

/// F3 path C: a generator into `Channel.send(...)` now crosses BY VALUE (deep copy into the queue)
/// instead of faulting; the program runs to completion.
#[test]
fn generator_into_channel_crosses_by_value() {
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "fn main():\n",
        "    ch := Channel[Iterator[int]]()\n",
        "    ch.send(gen())\n",
        "    print(\"sent\")\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).expect("serial"), "sent\n");
    assert_eq!(run_capture(src).expect("M:N"), "sent\n");
}

/// Captured args mutate across yields; an infinite generator terminates via `break`.
#[test]
fn vm_generator_captured_args_and_break() {
    let src = "fn count_from(n: int) -> Iterator[int]:\n    while true:\n        yield n\n        n = n + 1\nfn main():\n    for x in count_from(10):\n        if x > 13:\n            break\n        print(x)\nmain()\n";
    assert_eq!(run(src), "10\n11\n12\n13\n");
}

/// Nested generators: an outer generator's body drives an inner generator, each with its own
/// suspended context. Proves the private-context swap + host-rooting compose under recursion.
#[test]
fn vm_generator_nested() {
    let src = "fn inner(n: int) -> Iterator[int]:\n    yield n\n    yield n * 10\nfn outer() -> Iterator[int]:\n    for a in inner(1):\n        yield a\n    for b in inner(2):\n        yield b\nfn main():\n    for x in outer():\n        print(x)\nmain()\n";
    assert_eq!(run(src), "1\n10\n2\n20\n");
}

/// A generator declared as a STRUCT METHOD must also allocate-not-run: dispatched through
/// `do_method_call`, it returns a suspendable generator (regression — the method path once ran
/// the body inline, poisoning the host with a stray `gen_yielding`).
#[test]
fn vm_generator_struct_method() {
    let src = "struct C:\n    n: int\n    fn items(self) -> Iterator[int]:\n        yield self.n\n        yield self.n + 1\nfn main():\n    c := C(5)\n    for x in c.items():\n        print(x)\nmain()\n";
    assert_eq!(run(src), "5\n6\n");
}

/// A generator yielding heap values (strings) survives GC stress between/within `.next()` calls:
/// the suspended frames + yielded objects must stay rooted across collections.
#[test]
fn vm_generator_survives_gc_stress() {
    let src = "fn words() -> Iterator[str]:\n    yield \"alpha\"\n    yield \"beta\"\n    yield \"gamma\"\nfn main():\n    for w in words():\n        print(w)\nmain()\n";
    assert_eq!(run_capture_stress(src), "alpha\nbeta\ngamma\n");
}

/// `int_binop` against the real VM: each op x each operand pair runs as `a OP b` on runtime
/// values inside `recover:`, and the VM's value or fault must be what `int_binop` predicts.
#[test]
fn consteval_matches_vm_arith() {
    use crate::ast::BinaryOp;
    use crate::ast::consteval::int_binop;
    let ops = [
        (BinaryOp::Add, "+"),
        (BinaryOp::Sub, "-"),
        (BinaryOp::Mul, "*"),
        (BinaryOp::Div, "/"),
        (BinaryOp::Mod, "%"),
        (BinaryOp::BitAnd, "&"),
        (BinaryOp::BitOr, "|"),
        (BinaryOp::BitXor, "^"),
        (BinaryOp::Shl, "<<"),
        (BinaryOp::Shr, ">>"),
    ];
    let vals = [0, 1, -1, i64::MAX, i64::MIN, 63, 64];
    let lit = |v: i64| {
        if v == i64::MIN {
            "(0 - 9223372036854775807 - 1)".to_string()
        } else {
            v.to_string()
        }
    };
    let mut src = String::new();
    for (i, (_, sym)) in ops.iter().enumerate() {
        src += &format!(
            "fn t{i}(a: int, b: int):\n    r := recover: a {sym} b\n    match r:\n        ?v: print(str(v))\n        !e: print(\"F \" + e.message())\n"
        );
    }
    let mut want = Vec::new();
    for (i, (op, _)) in ops.iter().enumerate() {
        for a in vals {
            for b in vals {
                src += &format!("t{i}({}, {})\n", lit(a), lit(b));
                want.push((*op, a, b, int_binop(*op, a, b)));
            }
        }
    }
    let out = run_capture(&src).expect("the program runs");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), want.len(), "one line per op pair");
    let mut red = Vec::new();
    for ((op, a, b, w), got) in want.iter().zip(&lines) {
        let agrees = match w {
            Some(Ok(v)) => *got == v.to_string(),
            Some(Err(name)) => {
                got.starts_with("F ") && got.contains(&format!("integer overflow in {name}"))
            }
            None => got.starts_with("F ") && !got.contains("integer overflow"),
        };
        if !agrees {
            red.push(format!("{op:?}({a}, {b}): int_binop {w:?}, vm {got:?}"));
        }
    }
    assert!(red.is_empty(), "{}", red.join("\n"));
}
