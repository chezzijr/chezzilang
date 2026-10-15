//! Checker tests: int→float widening, interpolation fragments, compound and tuple assignment, `in`,
//! unknown name.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== one-way int→float implicit widening (C-like) =====

/// `x: float = 3` — D3: an int literal never widens into a float-annotated let binding.
#[test]
fn d3_widen_int_to_float_let_rejected() {
    rejects("x: float = 3\nprint(x)\n", "write 1.0");
    ok("x: float = 3.0\nprint(x)\n");
}

/// D3: neither an int CONSTANT nor a typed int variable passed into a `float` parameter widens.
#[test]
fn d3_widen_int_arg_into_float_param_rejected() {
    rejects("fn f(z: float):\n    print(z)\nf(7)\n", "write 1.0");
    rejects("fn f(z: float):\n    print(z)\nf(1 + 2)\n", "write 1.0");
    rejects("fn f(z: float):\n    print(z)\nf(-1)\n", "write 1.0");
    ok("fn f(z: float):\n    print(z)\nf(7.0)\n");
    rejects(
        "fn f(z: float):\n    print(z)\na := 3\nf(a)\n",
        "write 1.0 (or float(x))",
    );
    ok("fn f(z: float):\n    print(z)\na := 3\nf(float(a))\n");
}

/// D3: an int returned from a `-> float` function never widens, constant or typed.
#[test]
fn d3_widen_int_return_into_float_ret_rejected() {
    rejects(
        "fn g() -> float:\n    return 1 + 2\nprint(g())\n",
        "write 1.0",
    );
    rejects(
        "fn g(n: int) -> float:\n    return n + 1\nprint(g(2))\n",
        "write 1.0 (or float(x))",
    );
    ok("fn g(n: int) -> float:\n    return float(n + 1)\nprint(g(2))\n");
}

/// D3: an int never widens into a `float` struct field.
#[test]
fn d3_widen_int_into_float_struct_field_rejected() {
    rejects_desugared(
        "struct P:\n    v: float\np := P(3)\nprint(p.v)\n",
        "write 1.0",
    );
    ok_desugared("struct P:\n    v: float\np := P(3.0)\nprint(p.v)\n");
}

/// D3: an annotated `List[float]` / `Map[_, float]` rejects int elements. (Float is not Hashable, so
/// `Set[float]` / `Map[float, _]` are independently illegal — not a widening case.)
#[test]
fn d3_widen_int_elems_into_annotated_float_collection_rejected() {
    rejects("xs: List[float] = [1, 2.3]\nprint(xs)\n", "write 1.0");
    rejects(
        "m: Map[str, float] = {\"a\": 1, \"b\": 2.3}\nprint(m)\n",
        "write 1.0",
    );
    ok("xs: List[float] = [1.0, 2.3]\nprint(xs)\n");
    ok("m: Map[str, float] = {\"a\": 1.0, \"b\": 2.3}\nprint(m)\n");
}

/// D3: an int DEFAULT value never widens into a `float` parameter. The default-value-vs-param-type
/// check fires at the DECLARATION, so a wrong-typed default is caught even when every call overrides
/// it. The reverse (float default into an int param) stays a lossy type error.
#[test]
fn d3_widen_int_default_into_float_param_rejected() {
    rejects("fn g(a: float = 3) -> float:\n    return a\n", "write 1.0");
    ok("fn g(a: float = 3.0) -> float:\n    return a\n");
}

/// TICKET-025 / W8-21: implicit success-coercion at a declared `T?`/`T!E` sink is not implemented
/// yet, so a bare success value at those sinks is still a type error instead of coercing to
/// `Some(v)`/`Ok(v)`.
#[test]
fn bare_value_coerces_at_option_sink() {
    ok("fn f() -> int?:\n    return 1\nfn main():\n    pass\n");
}

/// W8-21 — the same coercion on the `T!E` (Result) side of a declared sink.
#[test]
fn bare_value_coerces_at_result_sink() {
    ok("fn f() -> int!:\n    return 1\nfn main():\n    pass\n");
}

/// W8-21 — a bare `return` at a `Result[nil, E]` sink coerces to DEC-017's zero-arg `Ok()`.
#[test]
fn bare_return_coerces_at_result_nil_sink() {
    ok("fn f() -> None!str:\n    return\nfn main():\n    pass\n");
}

/// W8-21 — the coercion applies at a closure's declared return sink too, both for a closure literal
/// and for a fn's own inline-expr body.
#[test]
fn bare_value_coerces_at_a_closure_sink() {
    ok("c := fn() -> int?: 1\nprint(c())\n");
    ok("fn f() -> int?: 1\nprint(f())\n");
}

/// W8-21 exclusion — a value that is ALREADY a carrier is never re-wrapped.
#[test]
fn success_coercion_never_rewraps_a_carrier() {
    rejects(
        "fn have(n: int) -> int?:\n    return n\nfn f() -> int??:\n    return have(1)\n",
        "expected return type",
    );
    rejects(
        "fn f() -> int?!str:\n    return None\n",
        "expected return type",
    );
}

/// W8-21 exclusion — the coercion never chains onto the separate int->float widen.
#[test]
fn success_coercion_does_not_chain_onto_int_float() {
    rejects("fn f() -> float?:\n    return 1\n", "expected return type");
}

/// W8-21 exclusion — a sink mentioning a type param declines (rule c: `ty_fully_concrete`).
#[test]
fn success_coercion_declines_a_generic_sink() {
    rejects(
        "fn f[T](x: T) -> T?:\n    return x\n",
        "expected return type",
    );
}

/// W8-21 — explicit carriers stay legal and unaffected by the coercion.
#[test]
fn success_coercion_keeps_explicit_carriers() {
    ok(
        "fn a() -> int?:\n    return None\nfn b() -> int?:\n    return ?2\nfn c() -> int!str:\n    return !\"x\"\nfn main():\n    pass\n",
    );
}

/// W8-21 exclusion — the coercion fires ONLY at return sinks, never at a `let`, an argument, or a
/// struct field default.
#[test]
fn success_coercion_wraps_at_every_typed_slot() {
    ok("x: int? = 1\nprint(x)\n");
    ok("fn t(x: int?) -> int:\n    return 0\nt(1)\n");
    ok("struct S:\n    n: int? = 1\nfn main():\n    pass\n");
    ok("fn f(x: int? = 1) -> int:\n    return 0\n");
    rejects(
        "fn f(x: float? = 1) -> int:\n    return 0\n",
        "default value for parameter",
    );
    // Operands never own the slot: the sum wraps once, the operands never.
    ok("fn main():\n    x: int? = 1 + 2\n    print(x)\n");
    // An existing carrier is never re-wrapped into a deeper slot.
    rejects(
        "fn have(n: int) -> int?:\n    return n\nx: int?? = have(5)\n",
        "cannot assign int? to variable of type int??",
    );
}

/// TICKET-227 (DEC-025 superseded) — a default provider is a typed slot like any other: both halves
/// of a default wrap together.
#[test]
fn success_coercion_wraps_in_a_default_provider() {
    ok("fn g() -> int:\n    return 5\nfn f(x: int? = g()) -> int:\n    return 0\nf()\n");
}

/// W8-21 regression — a nested ANNOTATED fn inside an un-annotated one is body-checked on every
/// speculative `infer_returns` pass (`check_fn_body`'s comment at the `inferring_ret` reset), and a
/// forward-declared callee is `Unknown` on an early pass, concrete once its own sig resolves. Without
/// rolling back `ret_coerce` on `diag_rollback`, the early pass's `NoWrap` verdict survived past its
/// speculative walk and conflicted with the later `WrapSome` the real walk recorded for the same
/// span, turning a valid program into a hard `internal:` compile error. Runs the real pipeline
/// (`run_capture`), not just `check_src`: the conflict fires in `compile`, past the checker.
#[test]
fn success_coercion_survives_a_speculative_return_inference_pass() {
    let out = crate::compiler::run_for_tests::run_capture(
        "fn outer():\n    fn g() -> int?:\n        return h()\n    match g():\n        ?v:\n            print(\"some {v + 0}\")\n        None:\n            print(\"none\")\n\nfn h(): 1\n\nouter()\n",
    )
    .expect("should run clean, not hit the return-coercion table conflict");
    assert_eq!(out, "some 1\n");
}

/// Lossy / wrong-direction conversions MUST stay type errors (the widen arm is one-way Float←Int only).
#[test]
fn widen_float_into_int_still_rejected() {
    rejects("y: int = 2.3\nprint(y)\n", "cannot assign");
    rejects("fn h(p: int):\n    print(p)\nh(2.3)\n", "expected");
    rejects("fn f() -> int:\n    return 2.3\n", "expected return type");
    rejects(
        "zs: List[int] = [1, 2.3]\nprint(zs)\n",
        "list element: expected int, found float",
    );
    rejects("n: int = 0\nn = 2.3\nprint(n)\n", "cannot assign");
    rejects(
        "fn k(a: int = 2.3):\n    print(a)\nk()\n",
        "default value for parameter",
    );
}

/// Widening is SCALAR-ONLY: a compound/nested/wrapped `float` annotation does NOT accept an
/// int-bearing value, because only a scalar `float` sink is coerced by the compiler — widening a
/// compound would leave an `Int` in a `float` slot (the exact runtime hole this design avoids; the
/// checker cannot distinguish a safe literal `[1, 2]` from an unsafe non-literal `f()`). Collection
/// floats come instead from mixed-literal inference (`[1, 2.3]` ⇒ `List[float]`, accepted above).
#[test]
fn widen_compound_float_positions_rejected() {
    rejects(
        "xs: List[List[float]] = [[1]]\nprint(xs)\n",
        "list element: expected float, found int",
    );
    rejects(
        "m: Map[str, List[float]] = {\"a\": [1]}\nprint(m)\n",
        "list element: expected float, found int",
    );
    rejects(
        "xs: List[Map[str, float]] = [{\"a\": 1}]\nprint(xs)\n",
        "map value: expected float, found int",
    );
    rejects(
        "o: float? = ?3\nprint(o)\n",
        "'?' value: expected float, found int — write 1.0",
    );
    rejects(
        "r: float! = ?3\nprint(r)\n",
        "'?' value: expected float, found int — write 1.0",
    );
    // A non-literal RHS (a fn returning List[int]) into List[float]: no literal to coerce → reject.
    rejects(
        "fn f() -> List[int]:\n    return [1]\nxs: List[float] = f()\nprint(xs)\n",
        "cannot assign",
    );
}

/// TICKET-124 (W13-14/W13-15) superseded this test's premise: a plain `x = <int>` reassignment to a
/// D3: reassigning an int (constant or typed) to a `float`-declared local never widens.
#[test]
fn d3_widen_reassign_int_to_float_local_rejects_untyped_constant() {
    rejects("x: float = 1.0\nx = 3\nprint(x)\n", "write 1.0");
    ok("x: float = 1.0\nx = 3.0\nprint(x)\n");
}

/// D3: a field/index assign target rejects an int constant into a declared `float` slot —
/// `p.x = 3` / `xs[0] = 3` / `m[k] = 3`.
#[test]
fn d3_widen_field_index_assign_targets_reject_untyped_constant() {
    rejects_desugared(
        "struct P:\n    x: float\np := P(1.0)\np.x = 3\nprint(p.x)\n",
        "write 1.0",
    );
    rejects(
        "xs: List[float] = [1.0]\nxs[0] = 3\nprint(xs)\n",
        "write 1.0",
    );
    rejects(
        "m: Map[str, float] = {\"a\": 1.0}\nm[\"a\"] = 3\nprint(m)\n",
        "write 1.0",
    );
}

// ===== string interpolation fragments are type-checked =====

#[test]
fn interpolation_undefined_name_rejected() {
    // An undefined name inside `{...}` must surface as a compile error (was opaque before the fix;
    // it slipped past `check` and panicked the compiler at global_slot).
    rejects("print(\"{nope}\")\n", "unknown name 'nope'");
}

#[test]
fn interpolation_type_error_rejected() {
    // A type error inside `{...}` (int + list) must be reported by `check`, not deferred to runtime.
    let errs = check_src("x: int = 1\ny: List[int] = [1]\nprint(\"{x + y}\")\n");
    assert!(
        !errs.is_empty(),
        "expected a type error for int + list inside interpolation, got none"
    );
}

#[test]
fn interpolation_valid_ok() {
    // No false positives on valid interpolations / plain / literal-brace strings.
    ok("x: int = 1\nprint(\"x is {x}\")\n");
    ok("print(\"plain\")\n");
    ok("print(\"lit braces {{ }}\")\n");
}

#[test]
fn interpolation_spec_type_mismatch_rejected() {
    // A format spec provably wrong for a CONCRETE scalar is a COMPILE error (was runtime-only).
    // Messages must match the runtime backstop wording verbatim (single-sourced in fmtspec).
    rejects(
        "s: str = \"hi\"\nprint(\"{s:.2f}\")\n",
        "type 'f' not valid for a string",
    );
    rejects(
        "x: float = 1.5\nprint(\"{x:d}\")\n",
        "type 'd' not valid for a float",
    );
    rejects(
        "x: int = 3\nprint(\"{x:.3d}\")\n",
        "precision not allowed on an integer",
    );
}

#[test]
fn interpolation_spec_valid_and_generic_ok() {
    // No false positives: valid concrete-scalar specs, no-spec cases, structs, and — critically —
    // a generic body where the value type is a `Param(T)` (T could be float at a call site) must
    // NOT be statically rejected; the runtime keeps the backstop.
    ok("x: float = 1.5\nprint(\"{x:.2f}\")\n");
    ok("n: int = 3\nprint(\"{n:d}\")\n");
    ok("s: str = \"hi\"\nprint(\"{s}\")\n");
    ok("s: str = \"hello\"\nprint(\"{s:.3}\")\n"); // string precision truncates — allowed
    ok("struct P:\n    a: int\np := P(1)\nprint(\"{p}\")\n");
    // Generic body: v: T is Param → NOT statically rejected.
    ok("fn show[T](v: T) -> str:\n    return \"{v:.2f}\"\nfn main():\n    pass\n");
}

#[test]
fn interpolation_spec_grouping_accepted() {
    // W8-42: CPython accepts `,` as a thousands-grouping type char in a format spec; Chezzi
    // rejects it outright. `{n:,}` is the highest-priority of the five missing spec forms.
    ok("n: int = 1000\nprint(\"{n:,}\")\n");
}

#[test]
fn interpolation_spec_grouping_rejects_what_cpython_rejects() {
    rejects(
        "s: str = \"abc\"\nprint(\"{s:,}\")\n",
        "format spec: grouping ',' not allowed on a string",
    );
    rejects(
        "n: int = 1000\nprint(\"{n:,x}\")\n",
        "format spec: grouping ',' not valid with type 'x'",
    );
}

#[test]
fn interpolation_spec_space_sign_rejected_on_a_string() {
    rejects(
        "s: str = \"hi\"\nprint(\"{s: d}\")\n",
        "sign ' ' not allowed on a string",
    );
    ok("n: int = 42\nprint(\"{n: d}\")\n");
}

#[test]
fn interpolation_spec_alternate_form_rejected_on_a_string() {
    rejects(
        "s: str = \"hi\"\nprint(\"{s:#}\")\n",
        "alternate form '#' not allowed on a string",
    );
    ok("n: int = 255\nprint(\"{n:#x}\")\n");
}

#[test]
fn interpolation_spec_pad_align_rejected_on_a_string() {
    rejects(
        "s: str = \"hi\"\nprint(\"{s:=10}\")\n",
        "'=' alignment not allowed on a string",
    );
    ok("n: int = 42\nprint(\"{n:=10}\")\n");
}

#[test]
fn interpolation_spec_general_format_rejected_on_a_string() {
    rejects(
        "s: str = \"hi\"\nprint(\"{s:g}\")\n",
        "type 'g' not valid for a string",
    );
    ok("f: float = 1234.5\nprint(\"{f:g}\")\n");
}

#[test]
fn interpolation_spec_w8_42_missing_forms_accepted() {
    // W8-42: CPython accepts these four spec forms (`#x` alternate form, `g` general float,
    // `=` sign-aware fill, leading-space sign); Chezzi's checker still rejects them all at check
    // time with "format spec: unknown type char '<c>'". This is the coverage gap, not a
    // soundness bug — each `ok` call is expected to pass once the forms are implemented.
    ok("n: int = 255\nprint(\"{n:#x}\")\n");
    ok("f: float = 1234.5\nprint(\"{f:g}\")\n");
    ok("n: int = 42\nprint(\"{n:=10}\")\n");
    ok("n: int = 42\nprint(\"{n: d}\")\n");
}

#[test]
fn interpolation_spec_runtime_width_nested_field_accepted() {
    // TICKET-162: CPython/Rust/Go take a width measured at runtime (`{s:<{w}}`); Chezzi's spec
    // parser sees the inner `{` as a type char.
    ok("w := 6
s := \"ab\"
print(\"|{s:<{w}}|\")
");
}

#[test]
fn interpolation_spec_nested_field_must_be_int() {
    // A nested width/precision field is an `int` (TICKET-162); CPython's text substitution would
    // read `{s:<{2.5}}` as width 2, precision 5 — Chezzi rejects it instead.
    rejects(
        r#"w := "x"
s := "ab"
print("|{s:<{w}}|")
"#,
        "a nested width field must be an int, found str",
    );
    rejects(
        r#"w := 2.5
s := "ab"
print("|{s:<{w}}|")
"#,
        "a nested width field must be an int, found float",
    );
    rejects(
        r#"x := 3.14159
p := true
print("{x:.{p}f}")
"#,
        "a nested precision field must be an int, found bool",
    );
    // The field is inferred like any expression: an unknown name is reported as one.
    rejects(
        r#"s := "ab"
print("{s:<{nope}}")
"#,
        "unknown name 'nope'",
    );
}

#[test]
fn interpolation_spec_nested_field_positions() {
    // A nested field is legal ONLY as the width or the precision.
    let slots = "allowed only as the width or the precision";
    // The spec's shape is the parser's to check now: the lexer found the fields, `fmtspec` places them.
    front_rejects(
        r#"w := 3
s := "ab"
print("{s:{w}<3}")
"#,
        slots,
    );
    front_rejects(
        r#"w := 3
n := 7
print("{n:>5{w}}")
"#,
        slots,
    );
    front_rejects(
        r#"s := "ab"
print("{s:<{}}")
"#,
        "needs an expression",
    );
    // A nested precision is still a precision: the shape check sees it on an int.
    rejects(
        r#"p := 2
n := 7
print("{n:.{p}}")
"#,
        "precision not allowed on an integer",
    );
    ok(r#"x := 3.14159
w := 8
p := 2
print("{x:{w}.{p}f}")
"#);
    ok(r#"n := 42
w := 6
print("{n:0{w}}")
"#);
    ok(r#"s := "ab"
w := 6
print("{s:*^{w}}|{s:<{w + 2}}|{s:>{s.len()}}")
"#);
}

// ===== compound assignment (*= /= %= &= |= ^= <<= >>=) =====

#[test]
fn compound_arith_assign_ok() {
    ok("fn main():\n    x := 5\n    x *= 2\n    x /= 2\n    x %= 3\n    print(x)\nmain()\n");
}

#[test]
fn compound_bitor_int_only() {
    // `&= |= ^= <<= >>=` require int operands.
    rejects(
        "fn main():\n    x := 1.0\n    x |= 2\nmain()\n",
        "requires int",
    );
    ok(
        "fn main():\n    x := 5\n    x |= 2\n    x &= 3\n    x ^= 1\n    x <<= 1\n    x >>= 1\n    print(x)\nmain()\n",
    );
}

#[test]
fn compound_div_rejects_float_into_int_slot() {
    // `/=` inherits the true-division rule: `int /= float` would widen to float — rejected for an int slot.
    rejects(
        "fn main():\n    x: int = 5\n    x /= 2.0\nmain()\n",
        "cannot assign float to int",
    );
}

#[test]
fn compound_mul_rejects_str() {
    rejects(
        "fn main():\n    s := \"a\"\n    s *= 2\nmain()\n",
        "cannot apply",
    );
}

// ===== tuple-swap / multi-target assignment =====

#[test]
fn tuple_swap_checks() {
    ok("fn main():\n    a := 1\n    b := 2\n    a, b = b, a\n    print(a)\n    print(b)\nmain()\n");
}

#[test]
fn tuple_swap_list_elements_check() {
    ok(
        "fn main():\n    data := [1, 2, 3]\n    data[0], data[2] = data[2], data[0]\n    print(data)\nmain()\n",
    );
}

#[test]
fn tuple_swap_type_mismatch_rejected() {
    // swapping an int var with a str var is a type error.
    rejects(
        "fn main():\n    a := 1\n    b := \"x\"\n    a, b = b, a\nmain()\n",
        "cannot assign",
    );
}

// ===== `in` membership operator =====

#[test]
fn in_operator_types() {
    ok("fn main():\n    print(1 in [1, 2, 3])\nmain()\n");
    ok("fn main():\n    print('k' in {'k': 1})\nmain()\n");
    ok("fn main():\n    print('b' in 'abc')\nmain()\n");
    ok("fn main():\n    s := {1, 2, 3}\n    print(2 in s)\nmain()\n");
}

#[test]
fn in_operator_result_is_bool() {
    // Result of `in` must be usable where a Bool is expected.
    ok("fn main():\n    if 1 in [1, 2]:\n        print(\"yes\")\nmain()\n");
}

#[test]
fn in_substring_requires_str_lhs() {
    rejects("fn main():\n    print(1 in \"abc\")\nmain()\n", "in");
}

#[test]
fn in_list_elem_type_mismatch() {
    rejects("fn main():\n    print(\"x\" in [1, 2, 3])\nmain()\n", "in");
}

#[test]
fn in_rejects_non_container() {
    rejects("fn main():\n    print(1 in 5)\nmain()\n", "in");
}

#[test]
fn in_takes_a_range_rhs() {
    // TICKET-253: a range is a value that satisfies `Contains[int]`.
    ok("fn main():\n    print(5 in 1..10)\nmain()\n");
    ok("fn main():\n    r := 1..10\n    print(5 in r)\nmain()\n");
    rejects(
        "x := \"a\" in 1..10\n",
        "cannot test membership of str in Range",
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
