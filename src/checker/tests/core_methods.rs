//! Checker tests: inference, review hardening, sort, core str/list methods, collection operators,
//! reserved type names, std signatures, named-factory imports.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== inference / generics =====

#[test]
fn result_ok_payload_must_match_return() {
    rejects(
        "fn f() -> int!:\n    return ?\"s\"\n",
        "'?' value: expected int, found str",
    );
}

#[test]
fn result_err_is_generic_ok() {
    ok("fn f(b: int) -> int!:\n    if b == 0:\n        return !\"bad\"\n    return ?b\n");
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
    rejects(
        "fn f() -> int:\n    return 1\nfn f() -> int:\n    return 2\n",
        "already defined",
    );
}

#[test]
fn a_same_module_fn_named_after_a_struct_is_bound_twice() {
    // A module binds a name once, and a type binds it too (owner 2026-10-16; Go: `S redeclared in
    // this block`; Rust: E0428). The constructor wrapper is a static `S.new`, as `std/path.chz` has.
    rejects(
        "struct S:\n    n: int\nfn S(n: int) -> int:\n    return n * 2\nprint(S(3))\n",
        "is already defined",
    );
}

#[test]
fn variant_name_shared_across_enums_is_allowed() {
    // Variants are scoped under their enum (keyed by `(enum, variant)`), so two enums may share a
    // variant name. Each is reached via its qualified form (`A.X` / `B.X`).
    ok(
        "enum A:\n    X(int)\nenum B:\n    X(str)\nfn f() -> A:\n    return A.X(1)\nfn g() -> B:\n    return B.X(\"s\")\n",
    );
}

#[test]
fn duplicate_variant_within_one_enum_is_reported() {
    // A repeat of a variant name *within the same* enum is still a collision.
    rejects(
        "enum A:\n    X(int)\n    X(str)\n",
        "variant 'X' is already defined in enum 'A'",
    );
}

#[test]
fn dup_struct_method_is_reported() {
    // Two instance methods sharing a name silently last-wins; reject it at hoist.
    rejects(
        "struct P:\n    fn f(self) -> int: return 1\n    fn f(self) -> int: return 2\n",
        "method 'f' is already defined",
    );
}

#[test]
fn dup_struct_field_is_reported() {
    // A repeated field name adds a dead-but-positionally-required slot; reject it.
    rejects(
        "struct P:\n    x: int\n    x: int\n",
        "field 'x' is already defined",
    );
}

#[test]
fn struct_field_and_method_same_name_is_reported() {
    // A field and a method may not share a name on the same struct.
    rejects(
        "struct P:\n    f: int\n    fn f(self) -> int: return 1\n",
        "'f' is declared as both a field and a method",
    );
}

#[test]
fn dup_enum_method_is_reported() {
    rejects(
        "enum E:\n    A\n    fn f(self) -> int: return 1\n    fn f(self) -> int: return 2\n",
        "method 'f' is already defined",
    );
}

#[test]
fn dup_method_diagnostic_is_clear_not_return_mismatch() {
    // The duplicate-method error must be the headline, not the misleading return-type cascade.
    let errs = check_src(
        "struct P:\n    fn f(self) -> int: return 1\n    fn f(self) -> str: return \"x\"\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("method 'f' is already defined")),
        "expected the duplicate-method error, got: {errs:?}"
    );
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("expected") && e.message.contains("found str")),
        "should not surface the return-mismatch cascade, got: {errs:?}"
    );
}

#[test]
fn bare_variant_value_is_rejected_with_qualify_hint() {
    // A user variant used bare as a value must be qualified — the error names the enum.
    rejects(
        "enum Color:\n    Red\n    Blue\nc := Red\n",
        "'Red' is a variant of enum 'Color'; write it qualified as 'Color.Red'",
    );
}

#[test]
fn bare_variant_constructor_is_rejected_with_qualify_hint() {
    // A bare payload-variant constructor must be qualified.
    rejects(
        "enum Shape:\n    Circle(int)\n    Dot\ns := Circle(5)\n",
        "write it qualified as 'Shape.Circle'",
    );
}

#[test]
fn bare_generic_variant_turbofish_keeps_qualify_hint_via_graph() {
    // REGRESSION (same keying class as the name_is_generic struct fix): a bare GENERIC-enum
    // variant with a turbofish (`Full[int](5)`) must still get the "write it qualified" hint, not the
    // misleading "takes no type arguments". `variant_owners` stores bare enum names but
    // `enum_type_params` is module-keyed, so `name_is_generic`'s variant arm has to go through
    // `bare_key`. Only the `check_graph` path (module-prefixed keys) exposed this — `rejects()`/
    // `check_src` keep keys bare and mask it, so this MUST use `entry_rejects`.
    entry_rejects(
        "enum Box[T]:\n    Empty\n    Full(T)\nb := Full[int](5)\n",
        "write it qualified as 'Box.Full'",
    );
}

#[test]
fn bare_variant_match_arm_is_rejected_not_a_silent_binding() {
    // The bare→binding trap: a bare known-variant arm must be a hard error, never silently a
    // catch-all binding that swallows the match.
    rejects(
        "enum Color:\n    Red\n    Blue\nfn f(c: Color) -> int:\n    return match c:\n        Red: 0\n        Blue: 1\n",
        "write it qualified as 'Color.Red'",
    );
}

#[test]
fn shared_variant_name_is_qualified_to_distinct_enums() {
    // Two enums may reuse a variant name; each qualified form resolves to its own enum.
    ok(
        "enum Color:\n    Red\nenum Light:\n    Red\nfn f() -> Color:\n    return Color.Red\nfn g() -> Light:\n    return Light.Red\n",
    );
}

#[test]
fn foreign_enum_qualifier_in_match_arm_is_rejected() {
    // A `case Light.Red:` arm against a `Color` scrutinee must be a checker error — owning the name
    // `Red` is not enough; the qualifier must name the scrutinee's enum. Otherwise it type-checks
    // clean, is miscounted toward exhaustiveness, and the genuine `Color.Red` value traps at runtime
    // (the arm carries Light's distinct variant_id). Regression guard for that soundness hole.
    rejects(
        "enum Color:\n    Red\n    Blue\nenum Light:\n    Red\n    Green\nfn f(c: Color) -> str:\n    return match c:\n        Light.Red: \"red\"\n        Color.Blue: \"blue\"\n",
        "variant 'Light.Red' cannot match a value of type Color",
    );
}

#[test]
fn closure_body_violating_return_annotation_rejected() {
    rejects(
        "f := fn(x: int) -> int: \"s\"\n",
        "closure body has type str",
    );
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
    // sort() mutates in place and yields nil — using its result as a value (a binary operand) is
    // now rejected up-front (Part 2: nil in value position).
    rejects("xs := [3, 1, 2]\nn := xs.sort() + 1\n", "no value (None)");
}

#[test]
fn list_sort_non_orderable_rejected() {
    // `where T: Comparable` bound-enforcement diagnostic (bool is not Comparable).
    rejects(
        "xs := [true, false]\nxs.sort()\n",
        "does not satisfy Comparable",
    );
}

// ===== golden: the touchstone program must type-check clean =====

#[test]
fn hello_example_type_checks() {
    let src = include_str!("../../../examples/hello.chz");
    ok(src);
}

#[test]
fn methods_example_type_checks() {
    let src = include_str!("../../../examples/methods.chz");
    ok(src);
}

// ===== multiple errors are collected, not just the first =====

#[test]
fn collects_multiple_errors() {
    let errs = check_src("x := a + b\ny := c - d\n");
    assert!(
        errs.len() >= 4,
        "expected >=4 unknown-name errors, got: {errs:?}"
    );
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
fn bool_ctor_infers_bool() {
    // `bool(x)` accepts any scalar and infers `bool` — verified via a typed target.
    ok("b: bool = bool(3)\nprint(b)\n");
    ok("b: bool = bool(3.0)\nprint(b)\n");
    ok("b: bool = bool(true)\nprint(b)\n");
    ok("b: bool = bool(\"x\")\nprint(b)\n");
}

#[test]
fn bool_ctor_is_not_int() {
    // `bool(x)` yields `bool`, not `int` — a mismatched typed target must reject.
    rejects("n: int = bool(3)\n", "to variable of type int");
}

#[test]
fn parse_int_infers_result_int_str() {
    ok("s := \"5\"\nr: int!str = s.parse_int()\nprint(r)\n");
}

#[test]
fn parse_float_infers_result_float_str() {
    ok("s := \"5.0\"\nr: float!str = s.parse_float()\nprint(r)\n");
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
    ok("parts := \"a,b,c\".split(\",\")\nx: List[str] = parts\nprint(x)\n");
}

#[test]
fn str_split_element_is_str_not_int() {
    rejects(
        "parts: List[int] = \"a,b\".split(\",\")\n",
        "List[str] to variable of type List[int]",
    );
}

#[test]
fn str_chars_returns_list_of_str() {
    ok("cs: List[str] = \"abc\".chars()\nprint(cs)\n");
}

#[test]
fn str_chars_element_is_str_not_int() {
    rejects(
        "cs: List[int] = \"abc\".chars()\n",
        "List[str] to variable of type List[int]",
    );
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
    rejects(
        "r := \",\".join([1, 2])\n",
        "list element: expected str, found int",
    );
}

#[test]
fn str_starts_with_is_bool() {
    ok("s := \"hi\"\nb: bool = s.starts_with(\"h\")\nprint(b)\n");
}

#[test]
fn str_method_wrong_arity_rejected() {
    rejects(
        "s := \"hi\"\nx := s.upper(\"extra\")\n",
        "'upper' expects 0 argument(s), got 1",
    );
}

#[test]
fn str_split_arg_must_be_str() {
    rejects("x := \"a,b\".split(5)\n", "argument 1 of 'split'");
}

#[test]
fn str_new_methods_infer_types_ok() {
    // gap #1 minimal subset: receiver methods forwarding to std.string free fns.
    ok("s := \"hi\"\nb: bool = s.ends_with(\"i\")\nprint(b)\n");
    ok("s := \"aXaX\"\nr: str = s.replace(\"X\", \"y\")\nprint(r)\n");
    ok("s := \"ab\"\nr: str = s.repeat(3)\nprint(r)\n");
    ok("s := \"abc\"\nr: str = s.reversed()\nprint(r)\n");
    ok("s := \"7\"\nr: str = s.pad_left(3, \"0\")\nprint(r)\n");
    ok("s := \"hello\"\nn: int = s.index_of(\"llo\")\nprint(n)\n");
    ok("s := \"aaa\"\nn: int = s.count(\"aa\")\nprint(n)\n");
    ok("s := \"prefix-x\"\nr: str = s.strip_prefix(\"prefix-\")\nprint(r)\n");
    ok("s := \"x-suffix\"\nr: str = s.strip_suffix(\"-suffix\")\nprint(r)\n");
    ok("s := \"a\\nb\"\nr: List[str] = s.split_lines()\nprint(r)\n");
    ok("s := \"  x  \"\nr: str = s.strip()\nprint(r)\n");
}

#[test]
fn str_to_int_is_option() {
    // gap #7: safe parse returns Option, not a bare int.
    ok("x: int? = \"42\".to_int()\nprint(x)\n");
}

#[test]
fn str_to_float_is_option() {
    ok("x: float? = \"4.5\".to_float()\nprint(x)\n");
}

#[test]
fn str_to_int_result_is_option_not_int() {
    // The result is Option[int], so binding it to a bare int must be rejected.
    rejects(
        "n: int = \"4\".to_int()\n",
        "cannot assign int? to variable of type int",
    );
}

#[test]
fn unknown_str_method_rejected() {
    rejects(
        "s := \"hi\"\nx := s.frobnicate()\n",
        "type str has no method 'frobnicate'",
    );
}

#[test]
fn list_push_and_len_ok() {
    ok("xs := [1, 2, 3]\nxs.push(4)\nn := xs.len()\nprint(n)\n");
}

#[test]
fn list_push_element_type_checked() {
    rejects(
        "xs := [1, 2, 3]\nxs.push(\"nope\")\n",
        "argument 1 of 'push'",
    );
}

#[test]
fn list_len_is_int() {
    ok("xs := [1, 2]\nn: int = xs.len()\nprint(n)\n");
}

#[test]
fn unknown_list_method_rejected() {
    rejects(
        "xs := [1, 2]\nx := xs.frobnicate()\n",
        "type List[int] has no method 'frobnicate'",
    );
}

#[test]
fn list_pop_returns_option() {
    ok("xs := [1, 2, 3]\nm := xs.pop()\nmatch m:\n    ?v: print(v)\n    None: print(0)\n");
}

#[test]
fn list_non_hof_methods_ok() {
    ok("xs := [1, 2, 3]\nb := xs.contains(2)\ni := xs.index_of(2)\ns := xs.sum()\nxs.reverse()\n");
}

#[test]
fn list_concat_returns_list() {
    ok("xs := [1, 2]\nys := xs.concat([3, 4])\nn: int = ys[0]\nprint(n)\n");
}

// --- Collection operators (gap #3): list `+`/`*`, set `| & - ^` ---

#[test]
fn list_plus_operator_returns_list() {
    ok("xs := [1, 2] + [3, 4]\nn: int = xs[0]\nprint(n)\n");
}

#[test]
fn list_plus_empty_infers_element_type() {
    // An empty `[]` side must not poison the element type — `[] + [1]` is `List[int]`.
    ok("xs := [] + [1, 2]\nn: int = xs[0]\nprint(n)\n");
    ok("xs := [1, 2] + []\nn: int = xs[0]\nprint(n)\n");
}

#[test]
fn list_plus_mismatched_element_rejected() {
    rejects(
        "xs := [1, 2] + [\"a\"]\n",
        "cannot apply + to List[int] and List[str]",
    );
}

#[test]
fn list_plus_non_list_rejected() {
    rejects("xs := [1, 2] + 1\n", "cannot apply + to List[int] and int");
}

#[test]
fn list_times_int_returns_list() {
    ok("xs := [1] * 3\nn: int = xs[0]\nprint(n)\n");
}

#[test]
fn int_times_list_returns_list() {
    // Commutative, Python-style: `3 * [1]` is also `List[int]`.
    ok("xs := 3 * [1]\nn: int = xs[0]\nprint(n)\n");
}

#[test]
fn list_times_non_int_rejected() {
    rejects(
        "xs := [1] * [2]\n",
        "cannot apply * to List[int] and List[int]",
    );
}

#[test]
fn set_union_operator_returns_set() {
    ok("a: Set[int] = {1}\nb: Set[int] = {2}\nc := a | b\nx: Set[int] = c\nprint(x.len())\n");
}

#[test]
fn set_intersection_difference_xor_operators_return_set() {
    ok(
        "a: Set[int] = {1, 2}\nb: Set[int] = {2, 3}\ni := a & b\nd := a - b\ns := a ^ b\nxi: Set[int] = i\nxd: Set[int] = d\nxs: Set[int] = s\nprint(xi.len() + xd.len() + xs.len())\n",
    );
}

#[test]
fn set_op_mismatched_element_rejected() {
    rejects(
        "a: Set[int] = {1}\nb: Set[str] = {\"a\"}\nc := a | b\n",
        "bitwise operator | requires int operands or two sets",
    );
}

#[test]
fn set_difference_mismatched_element_rejected() {
    rejects(
        "a: Set[int] = {1}\nb: Set[str] = {\"a\"}\nc := a - b\n",
        "cannot apply - to Set[int] and Set[str]",
    );
}

// Compound-assign forms of the collection operators must mirror their binary forms (they lower
// through the same opcodes) — and the bitwise compound diagnostic must not falsely claim sets are
// disallowed when `s = s | t` type-checks.

#[test]
fn list_compound_assign_ops_ok() {
    ok("xs := [1, 2]\nxs += [3, 4]\nxs *= 2\nprint(xs.len())\n");
}

#[test]
fn set_compound_assign_ops_ok() {
    ok(
        "a: Set[int] = {1, 2}\nb: Set[int] = {2, 3}\na |= b\na &= b\na ^= b\na -= b\nprint(a.len())\n",
    );
}

#[test]
fn list_plus_eq_mismatched_element_rejected() {
    rejects(
        "xs := [1, 2]\nxs += [\"a\"]\n",
        "list element: expected int, found str",
    );
}

#[test]
fn set_pipe_eq_mismatched_element_rejected() {
    rejects(
        "a: Set[int] = {1}\nb: Set[str] = {\"a\"}\na |= b\n",
        "bitwise operator | requires int operands or two sets",
    );
}

#[test]
fn list_extend_returns_nil() {
    ok("xs := [1, 2]\nxs.extend([3, 4])\nprint(xs.len())\n");
}

#[test]
fn list_concat_element_type_checked() {
    rejects(
        "xs := [1, 2]\nys := xs.concat([\"a\"])\n",
        "list element: expected int, found str",
    );
}

#[test]
fn list_extend_element_type_checked() {
    rejects(
        "xs := [1, 2]\nxs.extend([\"a\"])\n",
        "list element: expected int, found str",
    );
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
fn list_sum_struct_with_add_still_rejected_at_check() {
    // SOUNDNESS (Option B): `sum` is documented `where T: Add`, but its true requirement is MONOID
    // (Add + a zero/identity for the empty list) and the engine's `sum` cannot mint one. A user struct
    // with a structural `add(self, o) -> Self` SATISFIES the `Add` protocol, so `where T: Add` alone
    // would wrongly admit it — the residual numeric check-time gate must STILL reject it (never
    // check-ok/run-error). This pins that the gate survives alongside the `where T: Add` annotation.
    let src = "\
struct M:
    n: int
    fn add(self, other: M) -> M:
        return M(self.n + other.n)
xs := [M(1), M(2)]
s := xs.sum()
";
    rejects(src, "numeric");
}

#[test]
fn method_on_int_rejected() {
    rejects("x := 5\ny := x.upper()\n", "type int has no method 'upper'");
}

// ===== reserved builtin type names =====

// ===== M6c: native std module signatures =====

/// The escape hatch + no over-rejection: an aliased reserved-named module works, the builtin it
/// would have shadowed still works, and a normal module bind is unaffected.
#[test]
fn reserved_module_bind_alias_escape_hatch() {
    files_ok(&[
        (
            "lib/int.chz",
            "fn twice(n: int) -> int:\n    return n * 2\n",
        ),
        (
            "main.chz",
            "import lib.int as ints\nprint(int(\"5\"))\nprint(ints.twice(2))\n",
        ),
    ]);
    files_ok(&[
        ("lib/geo.chz", "struct Point:\n    x: int\n    y: int\n"),
        (
            "main.chz",
            "import lib.geo\np := geo.Point(1, 2)\nr: int!str = ?5\nprint(p.x)\n",
        ),
    ]);
}

/// A whole-module import binds its last path segment, and a same-module `struct`/`enum` of that name
/// is a second binding of it (Go: `Point redeclared in this block`). The error names the prior
/// import; the alias is the cure.
#[test]
fn a_module_import_and_a_same_named_type_are_bound_twice() {
    let point = ("lib/Point.chz", "x := 1\n");
    let shape = ("lib/Shape.chz", "y := 2\n");
    // a struct ctor …
    files_reject(
        &[
            point,
            (
                "main.chz",
                "import lib.Point\n\nstruct Point:\n    a: int\n\np := Point(1)\nprint(p.a)\n",
            ),
        ],
        "'Point' is already imported",
    );
    // … and an enum: same collision, same diagnostic.
    files_reject(
        &[
            shape,
            (
                "main.chz",
                "import lib.Shape\n\nenum Shape:\n    Dot\n\ns := Shape.Dot\nprint(Shape(1))\n",
            ),
        ],
        "'Shape' is already imported",
    );
    // the escape hatch: alias the import and BOTH the module and the ctor are reachable.
    files_ok(&[
        point,
        (
            "main.chz",
            "import lib.Point as pt\n\nstruct Point:\n    a: int\n\np := Point(1)\nprint(p.a)\nprint(pt.x)\n",
        ),
    ]);
    // no over-rejection: a module bind with NO same-named type keeps the generic not-callable error.
    files_reject(
        &[
            ("lib/geo.chz", "z := 3\n"),
            ("main.chz", "import lib.geo\nprint(geo(1))\n"),
        ],
        "is not callable",
    );
}

/// Bug 2 — a from-imported module global is a SNAPSHOT copy (Python-identical): REBINDING it is
/// rejected, consistent with the qualified form (`st.COUNT = 5`). Mutating THROUGH a from-imported
/// container still works (it is the same heap object).
#[test]
fn rebinding_from_imported_global_rejected() {
    let st = (
        "lib/st.chz",
        "COUNT := 0\nLST := [1]\nfn bump():\n    print(1)\n",
    );
    for (body, needle) in [
        (
            "import COUNT from lib.st\nCOUNT = 99\n",
            "imported from module",
        ),
        (
            "import COUNT from lib.st\nCOUNT += 1\n",
            "imported from module",
        ),
        (
            "import LST from lib.st\nLST = [2]\n",
            "imported from module",
        ),
        // the qualified form keeps its own existing reject
        (
            "import lib.st\nst.COUNT = 5\n",
            "cannot assign to field 'COUNT' of module st",
        ),
        // a from-imported FUNCTION is not a value binding at all
        (
            "import bump from lib.st\nbump = 3\n",
            "cannot assign to undeclared variable",
        ),
    ] {
        files_reject(&[st, ("main.chz", body)], needle);
    }
    // no over-rejection: mutation-through, a local shadow, and plain reads all stay legal
    files_ok(&[
        st,
        (
            "main.chz",
            "import COUNT, LST from lib.st\nLST.push(7)\nprint(COUNT)\nfn f():\n    COUNT := 1\n    COUNT = 2\n    print(COUNT)\nf()\n",
        ),
    ]);
    // …and a module-level `:=` of the name is a second binding (`GlobalBinding::clashes`).
    files_reject(
        &[
            st,
            (
                "main.chz",
                "import COUNT from lib.st\nCOUNT := COUNT + 1\nprint(COUNT)\n",
            ),
        ],
        "'COUNT' is already imported",
    );
}

/// `nil` is an ordinary name (TICKET-231), so a module may be named it or aliased to it. `None`,
/// the one "nothing" word, is still a rejected module alias.
#[test]
fn module_bind_named_nil_is_an_ordinary_name() {
    let m = ("lib/nil.chz", "fn f() -> int:\n    return 1\n");
    files_ok(&[m, ("main.chz", "import lib.nil\nprint(nil.f())\n")]);
    let geo = ("lib/geo.chz", "struct Point:\n    x: int\n");
    files_ok(&[geo, ("main.chz", "import lib.geo as nil\nprint(1)\n")]);
    parse_rejects("import lib.geo as None\nprint(1)\n", "reserved keyword");
}

/// Bug 3 — a module-qualified GENERIC fn whose type param appears only in the return type: the
/// turbofish (`geo.empty_list[int]()`) and the expected-type hint (`xs: List[int] = geo.empty_list()`)
/// must both reach it. Boundaries: wrong type-arg count still errors; a non-generic qualified call is
/// unchanged.
#[test]
fn qualified_generic_fn_turbofish_and_hint() {
    let geo = (
        "lib/geo.chz",
        "fn empty_list[T]() -> List[T]:\n    return []\nfn dist(a: int, b: int) -> int:\n    return a - b\n",
    );
    files_ok(&[
        geo,
        (
            "main.chz",
            "import lib.geo\nxs := geo.empty_list[int]()\nxs.push(1)\nys: List[str] = geo.empty_list()\nys.push(\"a\")\nprint(geo.dist(3, 1))\n",
        ),
    ]);
    files_reject(
        &[
            geo,
            (
                "main.chz",
                "import lib.geo\nxs := geo.empty_list[int, str]()\n",
            ),
        ],
        "type argument",
    );
}

/// **ONE-OFF PROBE, not a regression test — `#[ignore]`d.** Used by hand (W7-55) to find the eq-walk's
/// stack-overflow floor on the 1 GiB frontend stack `check_graph_with_entry` now always runs on:
/// temporarily bump the `EQ_BOUNDS_MAX_IN_PROGRESS` constant in `checker/eq.rs` past whatever depth
/// is under test (no env-var override exists — the cap is a plain `const`, so this means a scratch
/// edit + rebuild, reverted before committing), then invoke this single test directly
/// (`cargo test --lib … -- --ignored --exact`) with `CHEZZI_PROBE_SHAPE=cond|plain` and
/// `CHEZZI_PROBE_N=<depth>` under a memory-capped `systemd-run` scope — a stack overflow aborts the
/// WHOLE process, so this can only ever answer one depth per run. Left in the tree (not deleted after
/// use) because the next time this floor needs re-deriving — a new Rust version, a changed call chain
/// through `satisfies` — this is the harness, not a new one to write from scratch.
///
/// **M7 (adversarial review of W7-55, same day): the original floor measurement only ran the `cond`
/// (hook-`eq`) shape.** After Task 3 (`89c353bd`), a NON-hook `eq` also has its `where` bounds proven
/// AND falls through to `any(args)` + `walk_eq_members`, so it may cost more per level than `cond`
/// does — the `nonhook` shape below (a plain field wrapped in a struct with no custom `eq` at all, so
/// every level goes through the unconditional structural path) exists to re-derive the floor against
/// that shape too. Not re-measured as part of this fix: the cap (`crate::base::words::MAX_STRUCTURAL_DEPTH` =
/// 10 000) sits at roughly 1/12 of the `cond` floor (118 000), so `nonhook` would need to cost >12x
/// more per Rust stack frame than `cond` to threaten the new cap — implausible for "one more pass
/// through a few combinators", not a new backend. Left as a probe rather than asserted because
/// confirming that headroom precisely still means an `#[ignore]`d, cap-lifted, DEBUG-build stack
/// overflow — exactly the resource-risk this harness exists to contain to one on-demand run.
#[test]
#[ignore]
fn stack_probe_eq_bounds_depth() {
    let shape = std::env::var("CHEZZI_PROBE_SHAPE").unwrap_or_else(|_| "plain".to_string());
    let n: usize = std::env::var("CHEZZI_PROBE_N")
        .expect("set CHEZZI_PROBE_N")
        .parse()
        .expect("CHEZZI_PROBE_N must be a usize");
    let mut src = match shape.as_str() {
        // The CHEAP shape: a plain struct chain, one field, no method.
        "plain" => String::from("struct S0:\n    v: int\n"),
        // The EXPENSIVE shape: a chain of CONDITIONAL `eq` types — each level costs a full
        // `satisfies` -> `satisfies_args_d` -> walk -> `eq_where_unsatisfied` -> `satisfies` round
        // trip, matching what the original W7-55 floor measurement used.
        "cond" => String::from(
            "struct Tag:\n    n: int\n\
             struct Box[T]:\n    val: T\n    \
             fn compare(self, other: Self) -> int where T: Comparable:\n        return 0\n    \
             fn eq(self, other: Self) -> bool where T: Comparable:\n        return self.compare(other) == 0\n\
             struct C[T]:\n    v: T\n    \
             fn eq(self, other: Self) -> bool where T: Eq:\n        return true\n\
             struct A0:\n    v: Box[int]\n",
        ),
        // M7's shape: a chain of generic structs with NO custom `eq` at all — every level proves
        // `Eq` through the unconditional `any(args)` + `walk_eq_members` path Task 3 added, rather
        // than hopping out through a `where`-hooked `eq`'s `satisfies` round trip like `cond` does.
        "nonhook" => String::from(
            "struct C[T]:\n    v: T\n\
             struct A0:\n    v: int\n",
        ),
        other => panic!("unknown CHEZZI_PROBE_SHAPE {other:?}, want plain|cond|nonhook"),
    };
    let (prefix, per_level) = match shape.as_str() {
        "plain" => ("S", "struct {name}:\n    v: {prev}\n"),
        "cond" | "nonhook" => ("A", "struct {name}:\n    v: C[{prev}]\n"),
        _ => unreachable!(),
    };
    for i in 1..=n {
        let name = format!("{prefix}{i}");
        let prev = format!("{prefix}{}", i - 1);
        src.push_str(&per_level.replace("{name}", &name).replace("{prev}", &prev));
    }
    let root = format!("{prefix}{n}");
    src.push_str(&format!(
        "fn needs[U: Eq](a: U, b: U) -> bool:\n    return a == b\nfn use_it(a: {root}, b: {root}) -> bool:\n    return needs(a, b)\n"
    ));
    let errs = check_entry(&src);
    // Print rather than assert either way — the probe's answer is whether the PROCESS survived at
    // all (an overflow aborts before this line runs), not what the checker concluded.
    eprintln!("PROBE shape={shape} n={n} survived, errs={}", errs.len());
}

/// A deep parser-accepted AST, as source, with a depth that is EXACTLY known per level — which is
/// what a bytes-per-node probe needs. (It is not the *deepest* accepted shape: `( … .f×499 +1×499 )`
/// spends both fold loops per paren and reaches further; see `parser::MAX_AST_DEPTH`.)
/// `R(0) = "a"`, `R(k) = f(g(R(k-1)).f…×498)` —
/// TWO call layers per level because one layer would let `parse_postfix` merge the consecutive
/// postfix chains into a single loop and trip the fold bound (`MAX_AST_DEPTH`) instead of `MAX_DEPTH`.
/// Each level therefore adds exactly 500 expression nodes to the deepest root-to-leaf path (one
/// `Call` for `f`, 498 `Field`s, one `Call` for `g`), so `depth(R(lv)) = 1 + 500·lv`.
///
/// `f`, `g` and `a` are DECLARED (three extra top-level stmts that add no depth to the deep one):
/// `compiler::global_slot` panics on an undefined global rather than compiling it, because the
/// checker is supposed to have rejected the program first — so an undeclared fixture makes the
/// `compile` phase unprobeable. The `.f` folds still fail to type-check (`int` has no field `f`);
/// that is fine and intended, the walkers descend the whole chain either way.
fn probe_deep_expr_source(lv: usize) -> String {
    let folds = ".f".repeat(498);
    let mut s = String::from("a");
    for _ in 0..lv {
        s = format!("f(g({s}){folds})");
    }
    format!(
        "fn f(v: int) -> int:\n    return v\nfn g(v: int) -> int:\n    return v\na := 0\nx := {s}\n"
    )
}

/// Longest root-to-leaf expression-node path. Deliberately TOTAL over only the three `ExprKind`s
/// [`probe_deep_expr_source`] can produce — anything else panics rather than silently under-counting
/// the very number the probe exists to report.
fn probe_expr_depth(e: &crate::ast::Expr) -> usize {
    use crate::ast::ExprKind;
    1 + match &e.kind {
        ExprKind::Ident(_) => 0,
        ExprKind::Field { obj, .. } => probe_expr_depth(obj),
        ExprKind::Call {
            callee,
            args,
            named,
            ..
        } => {
            assert!(named.is_empty(), "fixture uses no named args");
            std::iter::once(&**callee)
                .chain(args.iter())
                .map(probe_expr_depth)
                .max()
                .unwrap_or(0)
        }
        other => panic!("fixture produced an unexpected ExprKind: {other:?}"),
    }
}

/// **ONE-OFF PROBE, not a regression test — `#[ignore]`d.** Companion to
/// [`stack_probe_eq_bounds_depth`] above, measuring a different axis: **bytes of DEBUG-build stack
/// consumed per AST node by each recursive front-end walker** (W7-50). That number is what decides
/// how far `parser::MAX_DEPTH` can rise now that every production front-end entry point runs on
/// `crate::FRONTEND_STACK_BYTES` (1 GiB) rather than whatever stack the caller happened to have —
/// the binding budget is no longer the ~2 MiB test worker but `vm::VM_STACK_BYTES` (384 MiB), which
/// is what `chezzi run` re-does `build_graph` + `compile_graph` on.
///
/// **Method — fix `N`, bisect the stack.** The fixture is held at one depth and the probe THREAD's
/// stack is the variable: the smallest stack the phase survives, divided by the AST depth, is the
/// per-node cost. A stack overflow aborts the whole process, so exactly one data point fits in one
/// run; drive it from a shell loop over `CHEZZI_PROBE_STACK_MIB`, treating a non-zero exit (the
/// `has overflowed its stack` abort, 134) as "too small" and the `PROBE … survived` line as "big
/// enough". Distinguish that from exit 137, which is the `systemd-run` memory cap, not a stack limit.
///
/// ```sh
/// for m in 1 2 4 8 16 32 64 128; do
///   systemd-run --user --scope --quiet -p MemoryMax=6G -p MemorySwapMax=0 -- \
///     env CHEZZI_PROBE_PHASE=compile CHEZZI_PROBE_N=15 CHEZZI_PROBE_STACK_MIB=$m \
///     cargo test --lib -- --ignored --exact --nocapture \
///       checker::tests::stack_probe_frontend_walker_depth
/// done
/// ```
///
/// `CHEZZI_PROBE_PHASE` is `parse` | `desugar` | `check` | `compile`. Everything a phase does NOT
/// measure is done up-front on the 1 GiB front-end stack and moved into the probe thread, so each
/// number is that walker's own cost and not its predecessor's. Two consequences worth knowing when
/// reading the output:
///
/// * `parse` builds the AST inside the probe thread, so its cost is per **`MAX_DEPTH` depth unit**
///   (4 per fixture level), not per AST node — it is not comparable to the other three.
/// * `compile` goes through `compiler::compile_module_standalone`, which clones the module and runs
///   `desugar::run_standalone` itself, so its floor is the MAX of those peaks, not the compiler
///   walk alone. Deep ASTs are `std::mem::forget`-ed rather than dropped where the probe controls
///   the drop, because recursive drop glue is stack cost that belongs to no walker.
///
/// **`check` has no separable inner fn.** `checker::check` and `check_graph_with_entry` wrap their
/// own bodies in `crate::on_frontend_stack_scoped`, so calling either from the probe thread would
/// measure the 1 GiB stack and report a meaningless number. The `check` arm below therefore inlines
/// the exact body of `checker::check` (`Checker::new` → `seed_native_prelude_sigs` → `check_module`),
/// which `checker::tests` can reach as a child module. If that body ever changes, change it here too.
///
/// **`overlay` is NOT probeable from here and is deliberately absent.** `editor::semantic_overlay`
/// is private to `crate::editor` and its only public route, `editor::semantic_tokens`, wraps itself
/// in `crate::on_frontend_stack`; adding a phase for it would silently measure 1 GiB. It is also not
/// the binding case — the overlay walk always gets the 1 GiB stack in production, unlike the
/// compiler walk, which `chezzi run` performs on the VM's 384 MiB thread.
#[test]
#[ignore]
fn stack_probe_frontend_walker_depth() {
    let phase = std::env::var("CHEZZI_PROBE_PHASE").unwrap_or_else(|_| "parse".to_string());
    let lv: usize = std::env::var("CHEZZI_PROBE_N")
        .map(|s| s.parse().expect("CHEZZI_PROBE_N must be a usize"))
        .unwrap_or(15);
    // `parse` recurses once per `MAX_DEPTH` unit (≤64 of them), so its floor lands far below 1 MiB —
    // `CHEZZI_PROBE_STACK_KIB` exists to bisect it at a resolution `…_MIB` cannot express. Exactly
    // one of the two must be set.
    let stack_kib: usize = match (
        std::env::var("CHEZZI_PROBE_STACK_KIB"),
        std::env::var("CHEZZI_PROBE_STACK_MIB"),
    ) {
        (Ok(k), _) => k.parse().expect("CHEZZI_PROBE_STACK_KIB must be a usize"),
        (_, Ok(m)) => {
            m.parse::<usize>()
                .expect("CHEZZI_PROBE_STACK_MIB must be a usize")
                * 1024
        }
        _ => panic!("set CHEZZI_PROBE_STACK_MIB or CHEZZI_PROBE_STACK_KIB"),
    };

    let src = probe_deep_expr_source(lv);
    // Parse + measure the depth on the BIG stack, so nothing here perturbs the bisection. Also pins
    // the fixture's documented edge: lv=15 is the last level `MAX_DEPTH` accepts.
    let depth = crate::on_frontend_stack_scoped(|| {
        let module =
            parser::parse(lexer::tokenize(&src).expect("lex")).expect("fixture must parse");
        let StmtKind::Let { value, .. } = &module.stmts.last().expect("stmts").kind else {
            panic!("fixture's LAST stmt must be the deep `x := …` let");
        };
        let d = probe_expr_depth(value);
        if lv == 15 {
            let over = probe_deep_expr_source(16);
            let err = parser::parse(lexer::tokenize(&over).expect("lex"))
                .expect_err("lv=16 must exceed MAX_DEPTH");
            assert!(
                err.message.contains("nested too deeply"),
                "lv=16 must fail on depth, got: {}",
                err.message
            );
        }
        assert_eq!(d, 1 + 500 * lv, "derived depth must match 1 + 500·lv");
        d
    });

    // Anything the phase under test does NOT own is prepared on the big stack and moved in.
    let prepared: Option<crate::ast::Module> = match phase.as_str() {
        "parse" => None,
        "desugar" | "check" | "compile" => Some(crate::on_frontend_stack_scoped(|| {
            parser::parse(lexer::tokenize(&src).expect("lex")).expect("fixture must parse")
        })),
        other => panic!("unknown CHEZZI_PROBE_PHASE {other:?}, want parse|desugar|check|compile"),
    };

    let ph = phase.clone();
    let note = std::thread::Builder::new()
        .stack_size(stack_kib * 1024)
        .spawn(move || match ph.as_str() {
            "parse" => {
                let module =
                    parser::parse(lexer::tokenize(&src).expect("lex")).expect("parse must Ok");
                let n = module.stmts.len();
                std::mem::forget(module);
                format!("stmts={n}")
            }
            "desugar" => {
                let mut module = prepared.expect("prepared");
                let r = crate::desugar::run_standalone(&mut module);
                std::mem::forget(module);
                format!("desugar_ok={}", r.is_ok())
            }
            // Verbatim body of `checker::check`, minus its `on_frontend_stack_scoped` wrap.
            "check" => {
                let module = prepared.expect("prepared");
                let mut c = Checker::new();
                c.seed_native_prelude_sigs();
                c.check_module(&module.stmts, None, &[], &[]);
                let errs = c.errors.len();
                std::mem::forget(module);
                std::mem::forget(c);
                format!("errs={errs}")
            }
            "compile" => {
                let module = prepared.expect("prepared");
                let r = crate::compiler::compile_module_standalone(&module);
                let ok = r.is_ok();
                std::mem::forget(r);
                std::mem::forget(module);
                format!("compile_ok={ok}")
            }
            _ => unreachable!(),
        })
        .expect("spawn probe thread")
        .join()
        .unwrap_or_else(|e| std::panic::resume_unwind(e));

    // Reached only if the probe thread did NOT overflow — an overflow aborts the process first.
    eprintln!("PROBE phase={phase} stack_kib={stack_kib} lv={lv} depth={depth} survived {note}");
}

/// BLOCKER 1 — user-facing checker errors naming a user struct/enum must render the BARE display
/// name, NOT the qualified IDENTITY key the redesign introduced (`<module-key>::Name`). Asserts the
/// WHOLE message (`==`, not `contains`) across field / method / type-mismatch sites so a leaked
/// `main::` prefix would fail. (Pre-fix these leaked `main::Point`/`main::Color`.)
#[test]
fn error_messages_render_bare_struct_enum_name() {
    // unknown field on a struct
    let errs = check_entry("struct Point:\n    x: int\np := Point(1)\nprint(p.nope)\n");
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert_eq!(errs[0].message, "type Point has no field 'nope'");

    // unknown method on a struct
    let errs = check_entry("struct Point:\n    x: int\np := Point(1)\np.frob()\n");
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert_eq!(errs[0].message, "type Point has no method 'frob'");

    // type mismatch — a Point where an int is expected renders the BARE struct name
    let errs = check_entry(
        "struct Point:\n    x: int\nfn takes_int(n: int):\n    print(n)\np := Point(1)\ntakes_int(p)\n",
    );
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert_eq!(
        errs[0].message,
        "argument 1 of 'takes_int': expected int, found Point"
    );

    // unknown field on an enum value renders the BARE enum name
    let errs = check_entry("enum Color:\n    Red\nc := Color.Red\nprint(c.nope)\n");
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert_eq!(errs[0].message, "type Color has no field 'nope'");
}

/// W8-17 — an unknown-method/unknown-field diagnostic's span must cover the MEMBER NAME token, not
/// the receiver. Pre-fix, `xs.lenght()` carets `xs` (col 7, 2 chars) instead of `lenght` (col 10,
/// 6 chars); same defect on the field form (`p.z` carets `p`).
#[test]
fn unknown_method_span_covers_member_name_not_receiver() {
    let errs = check_entry("xs := [1, 2, 3]\nprint(xs.lenght())\n");
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert_eq!(
        (errs[0].span.line, errs[0].span.col),
        (2, 10),
        "caret must land on 'lenght', not the receiver 'xs': {errs:?}"
    );

    let errs = check_entry("struct Point:\n    x: int\np := Point(1)\nprint(p.nope)\n");
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert_eq!(
        (errs[0].span.line, errs[0].span.col),
        (4, 9),
        "caret must land on 'nope', not the receiver 'p': {errs:?}"
    );
}

/// W8-17 — the member-miss caret lands on the member NAME at every checker site of that class:
/// method-miss (`expr.rs`), struct/tuple field-miss and the method-read-as-field arm (`pattern.rs`),
/// and the assign-target field-miss (`sig.rs`).
#[test]
fn member_miss_span_covers_the_name_at_every_site() {
    let cases: &[(&str, (u32, u32))] = &[
        ("x := 1\nx.frob()\n", (2, 3)),
        ("t := (1, \"a\")\nprint(t.5)\n", (2, 9)),
        (
            "struct Point:\n    x: int\n    fn get(self) -> int:\n        return self.x\np := Point(1)\nq := p.get\n",
            (6, 8),
        ),
        (
            "struct Point:\n    x: int\np := Point(1)\np.nope = 2\n",
            (4, 3),
        ),
        (
            "import std.net\n\nfn run(l: net.Listener):\n    s := l.accpet()\n",
            (4, 12),
        ),
        ("s := \"ab\"\nprint(s.uper())\n", (2, 9)),
        ("m := {\"a\": 1}\nprint(m.keyz())\n", (2, 9)),
    ];
    for (src, want) in cases {
        let errs = check_entry(src);
        assert_eq!(
            errs.len(),
            1,
            "expected exactly one error for {src:?}, got: {errs:?}"
        );
        assert_eq!(
            (errs[0].span.line, errs[0].span.col),
            *want,
            "wrong caret for {src:?}: {errs:?}"
        );
    }
}

/// BLOCKER 1 — cross-module struct: an imported type's error must still render BARE (`Point`), not
/// the qualified key (`dep::Point`). Uses a two-file graph so the struct carries `<mkey>::Point`.
#[test]
fn error_messages_render_bare_cross_module_struct() {
    let t = TmpDir::new();
    t.write("dep.chz", "struct Point:\n    x: int\n");
    let entry = t.write("main.chz", "import dep\np := dep.Point(1)\nprint(p.nope)\n");
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert_eq!(errs[0].message, "type Point has no field 'nope'");
}

// ----- named-factory-import member resolution (importing a factory FN, not its return TYPE) -----

/// A named import of a FACTORY FUNCTION only (not its return type) must still let the returned
/// value's METHODS resolve: member lookup keys off the value's OWN module-scoped identity, not
/// whether the type name was imported into the current module. Pre-fix: "type Widget has no method
/// 'bump'".
#[test]
fn checker_named_fn_import_resolves_struct_method() {
    let t = TmpDir::new();
    t.write(
        "lib.chz",
        "struct Widget:\n    n: int\n    fn bump(self) -> int:\n        return self.n + 1\nfn make() -> Widget:\n    return Widget(10)\n",
    );
    let entry = t.write(
        "main.chz",
        "import make from lib\nfn main():\n    w := make()\n    print(w.bump())\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
}

/// Same bug, FIELD access (read + compound-write): a named-imported factory result's fields must
/// resolve. Pre-fix: "type Box has no field 'data'".
#[test]
fn checker_named_fn_import_resolves_struct_field() {
    let t = TmpDir::new();
    t.write(
        "lib.chz",
        "struct Box:\n    data: int\nfn make_box() -> Box:\n    return Box(7)\n",
    );
    let entry = t.write(
        "main.chz",
        "import make_box from lib\nfn main():\n    b := make_box()\n    print(b.data)\n    b.data = 5\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
}

/// Same bug, ENUM method: a named-imported factory returning an enum must resolve the enum's method.
/// Pre-fix: "type Shape has no method 'area'".
#[test]
fn checker_named_fn_import_resolves_enum_method() {
    let t = TmpDir::new();
    t.write(
        "lib.chz",
        "enum Shape:\n    Circle(int)\n    fn area(self) -> int:\n        match self:\n            Shape.Circle(r): return r * r\nfn mk() -> Shape:\n    return Shape.Circle(3)\n",
    );
    let entry = t.write(
        "main.chz",
        "import mk from lib\nfn main():\n    print(mk().area())\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
}

/// Boundary: the fix does NOT over-open. Importing only the factory still leaves the type NAME
/// out of scope — naming/constructing it must STILL error "unknown type Widget".
#[test]
fn named_fn_import_does_not_license_type_name() {
    let t = TmpDir::new();
    t.write(
        "lib.chz",
        "struct Widget:\n    n: int\nfn make() -> Widget:\n    return Widget(1)\n",
    );
    let entry = t.write(
        "main.chz",
        "import make from lib\nfn main():\n    w := Widget(1)\n    print(w.n)\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unknown type 'Widget'")),
        "expected 'unknown type Widget', got: {errs:?}"
    );
}

/// Boundary: a same-named LOCAL struct (no import of lib's Widget) is the user's OWN type — the
/// factory-import fallback must not hijack it (distinct identity keys; local table hits first).
#[test]
fn named_fn_import_does_not_hijack_local_same_name() {
    let t = TmpDir::new();
    t.write(
        "lib.chz",
        "struct Widget:\n    n: int\n    fn bump(self) -> int:\n        return self.n\nfn make() -> Widget:\n    return Widget(1)\n",
    );
    // main declares its OWN Widget with a DIFFERENT shape; lib's `bump` must NOT resolve on it.
    let entry = t.write(
        "main.chz",
        "struct Widget:\n    q: str\nfn main():\n    w := Widget(\"hi\")\n    print(w.q)\n    w.bump()\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(
        errs.iter().any(|e| e.message.contains("no method 'bump'")),
        "expected local Widget to lack 'bump', got: {errs:?}"
    );
}

/// The documented equivalence: all THREE import forms of the SAME factory type-check clean — whole
/// module, import-the-type, and (the previously-broken) named-function-only.
#[test]
fn three_import_forms_all_check_ok() {
    let lib = "struct Widget:\n    n: int\n    fn bump(self) -> int:\n        return self.n + 1\nfn make() -> Widget:\n    return Widget(10)\n";
    // whole-module import
    {
        let t = TmpDir::new();
        t.write("lib.chz", lib);
        let entry = t.write(
            "main.chz",
            "import lib\nfn main():\n    print(lib.make().bump())\n",
        );
        let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
        assert!(check_graph(&graph).is_ok(), "whole-module import failed");
    }
    // import the type too
    {
        let t = TmpDir::new();
        t.write("lib.chz", lib);
        let entry = t.write(
            "main.chz",
            "import make, Widget from lib\nfn main():\n    w := make()\n    print(w.bump())\n",
        );
        let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
        assert!(check_graph(&graph).is_ok(), "import-type form failed");
    }
    // named function only (the fix)
    {
        let t = TmpDir::new();
        t.write("lib.chz", lib);
        let entry = t.write(
            "main.chz",
            "import make from lib\nfn main():\n    w := make()\n    print(w.bump())\n",
        );
        let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
        assert!(check_graph(&graph).is_ok(), "named-fn-only form failed");
    }
}

/// Cross-module transitive: `helper` returns a `cancel.Token`; `main` imports ONLY `helper` and
/// calls `.cancelled()` on the result. The Token's owning module (`std.cancel`) is a transitive
/// graph dep — the fallback reaches it by identity key. Pre-fix: "type Token has no method".
#[test]
fn checker_named_fn_import_resolves_transitive_stdlib_method() {
    let t = TmpDir::new();
    t.write(
        "helper.chz",
        "import std.cancel\nfn tok() -> cancel.Token:\n    return cancel.manual()\n",
    );
    let entry = t.write(
        "main.chz",
        "import helper\nfn main():\n    print(helper.tok().cancelled())\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
}

/// Stdlib gate: `import manual from std.cancel; manual().cancelled()` must type-check (checker gate;
/// the runtime arm lives in vm golden_tests). Pre-fix: "type Token has no method 'cancelled'".
#[test]
fn checker_named_stdlib_factory_resolves_method() {
    entry_ok("import manual from std.cancel\nfn main():\n    print(manual().cancelled())\n");
    entry_ok(
        "import min_heap from std.collections\nfn main():\n    h := min_heap()\n    h.push(3)\n    print(h.len())\n",
    );
}

// ----- named-factory-import PROTOCOL SATISFACTION (gap #4, satisfies path) -----

/// A named-fn-imported factory result that STRUCTURALLY SATISFIES a protocol must pass a
/// protocol-bounded generic exactly like a whole-module/type-name import. Pre-fix: `satisfies`
/// resolved the method table off the CURRENT module's imported-type tables, so the value was
/// spuriously rejected ("type Widget does not satisfy Describable") under the named-fn form only.
#[test]
fn named_fn_import_satisfies_protocol_struct_all_forms() {
    let lib = "struct Widget:\n    n: int\n    fn describe(self) -> str:\n        return \"W{self.n}\"\nfn make() -> Widget:\n    return Widget(10)\n";
    let proto_show = "protocol Describable:\n    fn describe(self) -> str\nfn show[T: Describable](x: T):\n    print(x.describe())\n";
    for (label, mainsrc) in [
        (
            "whole-module",
            format!("import lib\n{proto_show}fn main():\n    show(lib.make())\n"),
        ),
        (
            "import-type",
            format!("import make, Widget from lib\n{proto_show}fn main():\n    show(make())\n"),
        ),
        (
            "named-fn-only",
            format!("import make from lib\n{proto_show}fn main():\n    show(make())\n"),
        ),
    ] {
        let t = TmpDir::new();
        t.write("lib.chz", lib);
        let entry = t.write("main.chz", &mainsrc);
        let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
        let errs = match check_graph(&graph) {
            Ok(()) => Vec::new(),
            Err(e) => e,
        };
        assert!(
            errs.is_empty(),
            "{label} form: expected no type errors, got: {errs:?}"
        );
    }
}

/// Same, ENUM: a named-fn-imported enum value whose method satisfies the protocol is accepted at a
/// protocol bound.
#[test]
fn named_fn_import_satisfies_protocol_enum() {
    let t = TmpDir::new();
    t.write(
        "lib.chz",
        "enum Shape:\n    Circle(int)\n    fn describe(self) -> str:\n        match self:\n            Shape.Circle(r): return \"C{r}\"\nfn mk() -> Shape:\n    return Shape.Circle(3)\n",
    );
    let entry = t.write(
        "main.chz",
        "import mk from lib\nprotocol Describable:\n    fn describe(self) -> str\nfn show[T: Describable](x: T):\n    print(x.describe())\nfn main():\n    show(mk())\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
}

/// NEGATIVE: the fallback resolves the REAL method table, so a named-fn-imported value whose type
/// does NOT provide the required protocol method is STILL rejected — no laundering.
#[test]
fn named_fn_import_missing_protocol_method_still_rejected() {
    let t = TmpDir::new();
    t.write(
        "lib.chz",
        "struct Widget:\n    n: int\nfn make() -> Widget:\n    return Widget(10)\n",
    );
    let entry = t.write(
        "main.chz",
        "import make from lib\nprotocol Describable:\n    fn describe(self) -> str\nfn show[T: Describable](x: T):\n    print(x.describe())\nfn main():\n    show(make())\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(
        errs.iter().any(|e| e.message.contains("does not satisfy")),
        "expected 'does not satisfy Describable', got: {errs:?}"
    );
}

/// BLOCKER 1 (match errors) — non-exhaustive / literal-against-enum match errors on a cross-module
/// enum must render the BARE enum name (`Color`), not the qualified identity key (`a::Color`). These
/// errors interpolate the `MatchKind` label (the enum key) directly, so they bypassed the
/// `Ty::Display` fix and leaked the key. Uses a two-module collision so the enum carries `<mkey>::Color`.
#[test]
fn match_error_messages_render_bare_enum_name() {
    let t = TmpDir::new();
    t.write("a.chz", "enum Color:\n    Red\n    Blue\n");
    t.write("b.chz", "enum Color:\n    Red\n    Green\n");

    // non-exhaustive match: missing Blue — must name bare `Color`
    let entry = t.write(
        "main.chz",
        "import a\nimport b\nc := a.Color.Red\nmatch c:\n    Color.Red: print(\"r\")\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert_eq!(
        errs[0].message,
        "non-exhaustive match on Color: missing Blue"
    );

    // literal against an enum — must name bare `Color`
    let entry = t.write(
        "main.chz",
        "import a\nimport b\nc := a.Color.Red\nmatch c:\n    5: print(\"five\")\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(
        errs.iter()
            .any(|e| e.message == "cannot match a literal against Color"),
        "expected bare literal-match error, got: {errs:?}"
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
fn d3_native_math_float_param_rejects_int() {
    // D3: `math.sqrt(16)` is rejected like any user fn; the fix is `math.sqrt(16.0)`. (The native
    // host's `arg_float` runtime leniency stays as defence-in-depth, unreachable through the checker.)
    entry_rejects(
        "import std.math\nfn main():\n    print(math.sqrt(16))\n",
        "write 1.0",
    );
    entry_ok("import std.math\nfn main():\n    print(math.sqrt(16.0))\n");
}

// TICKET-278: collection methods follow the owner's Rust-shaped convention table (U16).

#[test]
fn list_remove_by_index_returns_the_element() {
    ok("xs := [1, 2, 3]\nx: int = xs.remove(0)\nprint(x)\n");
}

#[test]
fn set_insert_returns_bool() {
    ok("s := {1, 2}\nb: bool = s.insert(5)\nprint(b)\n");
}

/// TICKET-278 grid: every row of the collection convention table, from one list. Each cell binds
/// the call's result at its declared type (accepted) and at `str` (rejected with that type named).
/// The removed spellings have no method on any receiver.
#[test]
fn collection_convention_rows_hold() {
    const CMAP: &str =
        "import std.concurrency.collection\nc := collection.ConcurrentMap[str, int].new()";
    const CSET: &str =
        "import std.concurrency.collection\nc := collection.ConcurrentSet[int].new()";
    let rows: [(&str, &str, &str); 9] = [
        ("xs := [1, 2, 3]", "xs.remove(0)", "int"),
        ("s := {1, 2}", "s.insert(5)", "bool"),
        ("s := {1, 2}", "s.remove(1)", "bool"),
        ("m := {\"a\": 1}", "m.insert(\"b\", 2)", "int?"),
        ("m := {\"a\": 1}", "m.remove(\"a\")", "int?"),
        (CMAP, "c.insert(\"a\", 1)", "int?"),
        (CMAP, "c.remove(\"a\")", "int?"),
        (CSET, "c.insert(1)", "bool"),
        (CSET, "c.remove(1)", "bool"),
    ];
    for (setup, call, ty) in rows {
        ok(&format!("{setup}\nx: {ty} = {call}\nprint(x)\n"));
        rejects(
            &format!("{setup}\nx: str = {call}\nprint(x)\n"),
            &format!("cannot assign {ty} to variable of type str"),
        );
    }
    let removed = [
        (
            "xs := [1, 2, 3]",
            "xs.remove_at(0)",
            "List[int]",
            "remove_at",
        ),
        ("s := {1, 2}", "s.add(5)", "Set[int]", "add"),
        (CMAP, "c.set(\"a\", 1)", "ConcurrentMap[str, int]", "set"),
        (CSET, "c.add(1)", "ConcurrentSet[int]", "add"),
    ];
    for (setup, call, ty, name) in removed {
        rejects(
            &format!("{setup}\n_ := {call}\n"),
            &format!("type {ty} has no method '{name}'"),
        );
    }
}
