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

/// Type-check a source string after running the desugar pass (which lowers `?.`/`??` carriers to
/// `match`). Production always desugars before the checker (`resolver::build_graph`); these tests
/// otherwise bypass it. Returns the collected errors.
fn check_desugared(src: &str) -> Vec<CheckError> {
    let tokens = lexer::tokenize(src).expect("lex should succeed");
    let mut module = parser::parse(tokens).expect("parse should succeed");
    crate::desugar::run_standalone(&mut module).expect("desugar should succeed");
    match check(&module) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    }
}

/// Assert the desugared source type-checks clean.
fn ok_desugared(src: &str) {
    let errs = check_desugared(src);
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
}

/// Assert the desugared source produces an error containing `needle`.
fn rejects_desugared(src: &str, needle: &str) {
    let errs = check_desugared(src);
    assert!(
        errs.iter().any(|e| e.message.contains(needle)),
        "expected an error containing {needle:?}, got: {errs:?}"
    );
}

// ===== one-way int→float implicit widening (C-like) =====

/// `x: float = 3` — an int literal widens into a float-annotated let binding (the checker ACCEPTS it).
#[test]
fn widen_int_to_float_let_accepted() {
    ok("x: float = 3\nprint(x)\n");
}

/// An untyped int CONSTANT passed into a `float` parameter widens at the callee boundary. A TYPED
/// int variable does NOT (BREAKING, was accepted → left an `Int` under a static `float`): write
/// `float(a)`.
#[test]
fn widen_int_arg_into_float_param_accepted() {
    ok("fn f(z: float):\n    print(z)\nf(7)\nf(1 + 2)\nf(-1)\n");
    rejects(
        "fn f(z: float):\n    print(z)\na := 3\nf(a)\n",
        "a typed int never widens to float — write float(x)",
    );
    ok("fn f(z: float):\n    print(z)\na := 3\nf(float(a))\n");
}

/// A TYPED int expression returned from a `-> float` function no longer widens (BREAKING): `n + 1`
/// with `n: int` is a typed int. An untyped int CONSTANT return still widens.
#[test]
fn widen_int_return_into_float_ret_accepted() {
    ok("fn g() -> float:\n    return 1 + 2\nprint(g())\n");
    rejects(
        "fn g(n: int) -> float:\n    return n + 1\nprint(g(2))\n",
        "a typed int never widens to float — write float(x)",
    );
    ok("fn g(n: int) -> float:\n    return float(n + 1)\nprint(g(2))\n");
}

/// An int field value widens into a `float` struct field.
#[test]
fn widen_int_into_float_struct_field_accepted() {
    ok_desugared("struct P:\n    v: float\np := P(3)\nprint(p.v)\n");
}

/// An annotated `List[float]` accepts int elements (widened); a `map` VALUE position too. (Float is
/// not Hashable, so `Set[float]` / `Map[float, _]` are independently illegal — not a widening case.)
#[test]
fn widen_int_elems_into_annotated_float_collection_accepted() {
    ok("xs: List[float] = [1, 2.3]\nprint(xs)\n");
    ok("m: Map[str, float] = {\"a\": 1, \"b\": 2.3}\nprint(m)\n");
}

/// An int DEFAULT value widens into a `float` parameter (scalar sink; coerced at the callee prologue
/// when the default is desugar-spliced into a call). The reverse (float default into an int param)
/// stays a lossy type error (covered in `widen_float_into_int_still_rejected`).
#[test]
fn widen_int_default_into_float_param_accepted() {
    // The default-value-vs-param-type check fires at the DECLARATION (so a wrong-typed default is
    // caught even when every call overrides it); declaration-only keeps this off the desugar/arity
    // path. The omitted-default RUNTIME coercion is covered by vm::parity_tests::widen_default_param_division.
    ok("fn g(a: float = 3) -> float:\n    return a\n");
}

/// Lossy / wrong-direction conversions MUST stay type errors (the widen arm is one-way Float←Int only).
#[test]
fn widen_float_into_int_still_rejected() {
    rejects("y: int = 2.3\nprint(y)\n", "cannot assign");
    rejects("fn h(p: int):\n    print(p)\nh(2.3)\n", "expected");
    rejects("fn f() -> int:\n    return 2.3\n", "expected return type");
    rejects("zs: List[int] = [1, 2.3]\nprint(zs)\n", "cannot assign");
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
        "cannot assign",
    );
    rejects(
        "m: Map[str, List[float]] = {\"a\": [1]}\nprint(m)\n",
        "cannot assign",
    );
    rejects(
        "xs: List[Map[str, float]] = [{\"a\": 1}]\nprint(xs)\n",
        "cannot assign",
    );
    rejects("o: float? = Some(3)\nprint(o)\n", "cannot assign");
    rejects("r: float! = Ok(3)\nprint(r)\n", "cannot assign");
    // A non-literal RHS (a fn returning List[int]) into List[float]: no literal to coerce → reject.
    rejects(
        "fn f() -> List[int]:\n    return [1]\nxs: List[float] = f()\nprint(xs)\n",
        "cannot assign",
    );
}

/// A plain `x = <int>` reassignment to a `float`-declared local is a STRICT assign target (no
/// widening — the documented carve-out): the checker rejects it. (Annotated/param/return/field DO
/// widen; a reassignment target is type-blind for the same reason `p.x = 3` is.)
#[test]
fn widen_reassign_int_to_float_local_rejected() {
    rejects("x: float = 1.0\nx = 3\nprint(x)\n", "cannot assign");
}

/// Type-blind assign TARGETS stay strict (no runtime hole): `p.x = 3` / `xs[0] = 3` / `m[k] = 3`
/// into a float container reject, because the compiler has no field/elem type to coerce against.
#[test]
fn widen_typeblind_assign_targets_still_reject() {
    rejects_desugared(
        "struct P:\n    x: float\np := P(1.0)\np.x = 3\nprint(p.x)\n",
        "cannot assign",
    );
    rejects(
        "xs: List[float] = [1.0]\nxs[0] = 3\nprint(xs)\n",
        "cannot assign",
    );
    rejects(
        "m: Map[str, float] = {\"a\": 1.0}\nm[\"a\"] = 3\nprint(m)\n",
        "cannot assign",
    );
}

/// A newtype boundary stays nominal — NO int→float widening into a `float`-backed newtype ctor.
#[test]
fn widen_no_int_into_float_newtype() {
    rejects_desugared(
        "newtype Celsius = float\nc := Celsius(3)\nprint(c)\n",
        "expected",
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
        "cannot apply",
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
fn in_rejects_range_rhs() {
    // A range has no runtime value, so it can't be an `in` container. This used to need an ad-hoc
    // guard in the `In` arm; the generic `ExprKind::Range` rejection in `infer_kind` now subsumes
    // it. Exactly ONE diagnostic — a leftover ad-hoc guard would double-report.
    let errs = check_entry("fn main():\n    print(5 in 1..10)\nmain()\n");
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert!(errs[0].message.contains("range"), "got: {errs:?}");
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
    ok(
        "fn max[T: Comparable](a: T, b: T) -> T:\n    if a < b:\n        return b\n    return a\nm := max(3, 7)\n",
    );
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

// ===== explicit call-site type arguments (`max[int](…)`) =====

const MAX_FN: &str = "fn max[T: Comparable](a: T, b: T) -> T:\n    return a\n";

#[test]
fn explicit_type_args_fn_ok() {
    // Pin T=int explicitly; result is int, so an int-typed binding is fine.
    ok(&format!("{MAX_FN}x: int = max[int](3, 7)\n"));
}

#[test]
fn explicit_type_args_pin_result_type() {
    // `max[int]` returns int → a str binding must be rejected (proves the pin flows to the return).
    rejects(
        &format!("{MAX_FN}x: str = max[int](3, 7)\n"),
        "cannot assign int",
    );
}

#[test]
fn explicit_type_args_mismatch_rejected() {
    // T pinned to str, but the args are int → argument type error.
    rejects(&format!("{MAX_FN}x := max[str](3, 7)\n"), "expected str");
}

#[test]
fn explicit_type_args_wrong_count_rejected() {
    rejects(
        &format!("{MAX_FN}x := max[int, int](3, 7)\n"),
        "expects 1 type argument",
    );
}

#[test]
fn explicit_type_args_on_non_generic_rejected() {
    rejects(
        "fn inc(a: int) -> int:\n    return a + 1\nx := inc[int](3)\n",
        "takes no type arguments",
    );
}

#[test]
fn explicit_type_args_struct_ok() {
    let src = "\
struct Pair[A, B]:
    a: A
    b: B
p := Pair[int, str](1, \"a\")
";
    ok(src);
}

#[test]
fn explicit_type_args_struct_mismatch_rejected() {
    let src = "\
struct Pair[A, B]:
    a: A
    b: B
p := Pair[str, str](1, \"a\")
";
    rejects(src, "expected str");
}

#[test]
fn explicit_type_args_struct_ok_via_graph() {
    // REGRESSION: `name_is_generic` looked up `structs` by the BARE name, but under the real
    // `check_graph` path (which `chezzi check`/`run` use) user structs are keyed by the
    // module-prefixed `bare_key`. So a generic struct ctor with explicit call-site type args was
    // wrongly reported non-generic and rejected with "takes no type arguments" — even though the
    // struct-ctor branch fully supports them. `explicit_type_args_struct_ok` missed this because
    // `ok()`/`check_src` use the single-module path where keys stay bare. Must use `entry_ok`.
    entry_ok(
        "\
struct Pair[A, B]:
    a: A
    b: B
struct Box[T]:
    v: T
p := Pair[int, str](1, \"one\")
b := Box[int](3)
print(p.a)
print(b.v)
",
    );
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
    rejects(
        "fn f[T: Bogus](a: T) -> T:\n    return a\n",
        "unknown protocol 'Bogus'",
    );
}

#[test]
fn scalar_where_bound_is_equality_constraint() {
    // `where T: <scalar>` is an EQUALITY bound, not a protocol — `T` must be exactly that scalar.
    ok("fn f[T: bool](a: T) -> T:\n    return a\nfn main():\n    print(f(true))\n");
    ok("fn f[T: str](a: T) -> T:\n    return a\nfn main():\n    print(f(\"hi\"))\n");
    rejects(
        "fn f[T: bool](a: T) -> T:\n    return a\nfn main():\n    print(f(5))\n",
        "expected bool, found int",
    );
    // A scalar bound takes no type args.
    rejects(
        "fn f[T: bool[int]](a: T) -> T:\n    return a\n",
        "takes no type arguments",
    );
}

#[test]
fn container_where_bound_is_head_constructor_constraint() {
    // `where T: List/Map/Set` is a CONSTRUCTOR-KIND bound — `T`'s head must be exactly that container
    // (element/key/value types free). The surface form of the RwShared read-view gate.
    ok("fn f[T: List](a: T) -> T:\n    return a\nfn main():\n    print(f([1, 2, 3]))\n");
    ok("fn f[T: Map](a: T) -> T:\n    return a\nfn main():\n    print(f({\"a\": 1}))\n");
    ok("fn f[T: Set](a: T) -> T:\n    return a\nfn main():\n    print(f(Set([1, 2])))\n");
    rejects(
        "fn f[T: List](a: T) -> T:\n    return a\nfn main():\n    print(f(5))\n",
        "expected List[...], found int",
    );
    rejects(
        "fn f[T: Map](a: T) -> T:\n    return a\nfn main():\n    print(f([1, 2]))\n",
        "expected Map[...], found",
    );
    // A container bound takes no type args (no element binder).
    rejects(
        "fn f[T: List[int]](a: T) -> T:\n    return a\n",
        "takes no type arguments",
    );
}

#[test]
fn channel_trip_gated_to_bool() {
    // `trip()` is `where T: bool` (its level-trigger latch only ever delivers `bool true`), so it is
    // sound only on `Channel[bool]` — the hole where `Channel[int].trip(); .recv()` leaked a `bool`
    // through an `int`-typed value is now a compile error.
    ok("fn main():\n    c := Channel[bool]()\n    c.trip()\n    print(c.recv())\n");
    rejects(
        "fn main():\n    c := Channel[int]()\n    c.trip()\n    print(c.recv())\n",
        "expected bool, found int",
    );
}

#[test]
fn redeclaring_comparable_rejected() {
    rejects(
        "protocol Comparable:\n    fn compare(self, other: Self) -> int\n",
        "reserved",
    );
}

#[test]
fn redeclaring_convert_rejected() {
    // `Convert` is a reserved (builtin) protocol (slice 1 of the Convert/From type-conversion work) —
    // a user `protocol Convert[S]:` redeclaration is rejected, exactly like `Comparable`. Both the
    // parameterized form (matching the prelude shape) and a bare `protocol Convert:` are reserved on
    // the name alone.
    rejects(
        "protocol Convert[S]:\n    fn convert(x: S) -> Self\n",
        "reserved",
    );
    rejects(
        "protocol Convert:\n    fn convert(x: int) -> int\n",
        "reserved",
    );
}

// ----- Stringable protocol (M10-G1) -----

#[test]
fn stringable_struct_satisfies_ok() {
    let src = "\
struct Point:
    x: int
    y: int
    fn str(self) -> str:
        return \"({self.x}, {self.y})\"
fn show[T: Stringable](v: T) -> str:
    return v.str()
s := show(Point(1, 2))
";
    ok(src);
}

#[test]
fn stringable_wrong_signature_rejected() {
    // `str` returns int, not str ⇒ does not satisfy Stringable.
    let src = "\
struct Bad:
    n: int
    fn str(self) -> int:
        return self.n
fn show[T: Stringable](v: T) -> str:
    return v.str()
x := show(Bad(1))
";
    rejects(src, "does not satisfy Stringable");
}

#[test]
fn stringable_missing_method_rejected() {
    let src = "\
struct Bare:
    a: int
fn show[T: Stringable](v: T) -> str:
    return v.str()
x := show(Bare(1))
";
    rejects(src, "does not satisfy Stringable");
}

#[test]
fn stringable_scalar_satisfies_ok() {
    // int/float/bool/str intrinsically satisfy Stringable (sole method `str(self) -> str`),
    // mirroring the Comparable/Hashable/Add intrinsic scalar arms.
    for lit in ["show(5)", "show(3.14)", "show(true)", "show(\"hi\")"] {
        let src = format!("fn show[T: Stringable](v: T) -> str:\n    return v.str()\ns := {lit}\n");
        ok(&src);
    }
}

#[test]
fn scalar_str_direct_call_is_bound_only() {
    // The intrinsic Stringable arm is BOUND-only, exactly like the Comparable arm: a direct
    // concrete-receiver `(5).str()` stays a compile error (no direct scalar method), matching
    // `(5).compare(3)`. The free `str(5)` builtin covers direct use. Keeps str consistent with
    // every sibling intrinsic protocol (none adds a first-class direct scalar method).
    rejects("x := (5).str()\n", "type int has no method 'str'");
    rejects("x := (5).compare(3)\n", "type int has no method 'compare'");
}

#[test]
fn redeclaring_stringable_rejected() {
    rejects(
        "protocol Stringable:\n    fn str(self) -> str\n",
        "reserved",
    );
}

// ----- Hashable struct keys for map/set (key restriction lifted) -----

/// A Hashable struct (defines `hash(self) -> int`) usable as a map key / set element.
const POINT_H: &str = "\
struct Point:
    x: int
    y: int
    fn hash(self) -> int:
        return self.x * 31 + self.y
";

#[test]
fn struct_with_hash_is_valid_map_key() {
    ok(&format!(
        "{POINT_H}m: Map[Point, str] = {{}}\nm[Point(1, 2)] = \"a\"\n"
    ));
}

#[test]
fn set_of_hashable_struct_ok() {
    ok(&format!(
        "{POINT_H}s: Set[Point] = Set()\ns.add(Point(1, 2))\n"
    ));
}

#[test]
fn struct_without_hash_rejected_as_map_key() {
    let src = "struct Bare:\n    a: int\nm: Map[Bare, int] = {}\n";
    rejects(src, "Map key type must implement Hashable");
}

#[test]
fn struct_without_hash_rejected_as_set_element() {
    let src = "struct Bare:\n    a: int\ns: Set[Bare] = Set()\n";
    rejects(src, "Set element type must implement Hashable");
}

#[test]
fn float_still_rejected_as_map_key() {
    rejects(
        "m: Map[float, int] = {}\n",
        "Map key type must implement Hashable",
    );
}

#[test]
fn float_still_rejected_as_set_element() {
    rejects(
        "s: Set[float] = Set()\n",
        "Set element type must implement Hashable",
    );
}

// ----- Hashable protocol (M10-G2: bound; M10 map-model: wired to map/set keys) -----

#[test]
fn hashable_struct_satisfies_ok() {
    let src = "\
struct Id:
    n: int
    fn hash(self) -> int:
        return self.n
fn keyed[T: Hashable](v: T) -> int:
    return v.hash()
x := keyed(Id(7))
";
    ok(src);
}

#[test]
fn hashable_intrinsic_for_scalars_ok() {
    // int/str/bool satisfy Hashable intrinsically, so a `[T: Hashable]` bound accepts them.
    ok(
        "fn keyed[T: Hashable](v: T) -> T:\n    return v\na := keyed(3)\nb := keyed(\"x\")\nc := keyed(true)\n",
    );
}

#[test]
fn hashable_missing_method_rejected() {
    let src = "\
struct Bare:
    a: int
fn keyed[T: Hashable](v: T) -> T:
    return v
x := keyed(Bare(1))
";
    rejects(src, "does not satisfy Hashable");
}

#[test]
fn redeclaring_hashable_rejected() {
    rejects("protocol Hashable:\n    fn hash(self) -> int\n", "reserved");
}

// ----- numeric operator protocols + multi-bound (M10-G3) -----

const VEC2: &str = "\
struct Vec2:
    x: int
    y: int
    fn add(self, o: Vec2) -> Vec2:
        return Vec2(self.x + o.x, self.y + o.y)
    fn mul(self, o: Vec2) -> Vec2:
        return Vec2(self.x * o.x, self.y * o.y)
";

#[test]
fn struct_add_mul_overload_ok() {
    ok(&format!(
        "{VEC2}v := Vec2(1, 2) + Vec2(3, 4) * Vec2(5, 6)\n"
    ));
}

#[test]
fn struct_without_sub_rejects_minus() {
    // Vec2 defines add/mul but not sub ⇒ `-` is not overloaded.
    rejects(
        &format!("{VEC2}v := Vec2(1, 2) - Vec2(3, 4)\n"),
        "cannot apply - to Vec2 and Vec2",
    );
}

#[test]
fn multi_bound_add_mul_ok() {
    ok(&format!(
        "{VEC2}fn fma[T: Add + Mul](a: T, b: T, c: T) -> T:\n    return a + b * c\nv := fma(Vec2(1,2), Vec2(3,4), Vec2(5,6))\nn := fma(2, 3, 4)\n"
    ));
}

#[test]
fn multi_bound_missing_one_protocol_rejected() {
    // Point has add but no mul ⇒ fails the `Mul` half of `T: Add + Mul`.
    let src = "\
struct PointA:
    x: int
    fn add(self, o: PointA) -> PointA:
        return PointA(self.x + o.x)
fn fma[T: Add + Mul](a: T, b: T, c: T) -> T:
    return a + b * c
v := fma(PointA(1), PointA(2), PointA(3))
";
    rejects(src, "does not satisfy Mul");
}

// ----- generic struct/enum operator overloading + protocol satisfaction (the receiving type's
// own type params, e.g. `T->int` from `Box[int]`, must be threaded into the protocol-method
// signature comparison, in addition to `Self` and the protocol's own params) -----

#[test]
fn generic_struct_add_satisfies_and_overloads() {
    // `Box[T]` defines `add(self, o: Box[T]) -> Box[T]`; the `+` operator AND a `T: Add`-bounded
    // generic must both accept `Box[int]`. Previously rejected ("cannot apply + ..." / "does not
    // satisfy Add (method 'add' has the wrong signature)") because `T` was never bound to `int`.
    let src = "\
struct Box[T]:
    v: T
    fn add(self, o: Box[T]) -> Box[T]:
        return Box(self.v)
fn twice[T: Add](x: T) -> T:
    return x + x
a := (Box(5) + Box(10)).v
b := twice(Box(7)).v
";
    entry_ok(src);
}

#[test]
fn generic_struct_neg_and_compare() {
    // Unary `neg`/`-` and `compare`/`<`/Comparable over a generic struct.
    let src = "\
struct Box[T]:
    v: T
    fn neg(self) -> Box[T]:
        return Box(self.v)
    fn compare(self, o: Box[T]) -> int:
        return 0
a := (-Box(5)).v
b := Box(5) < Box(10)
fn smallest[T: Comparable](x: T, y: T) -> T:
    if x < y:
        return x
    return y
c := smallest(Box(1), Box(2)).v
";
    entry_ok(src);
}

#[test]
fn generic_enum_add_satisfies() {
    // Generic-enum analogue: `enum Num[T]` with `add` overloads `+` and satisfies `Add`.
    let src = "\
enum Num[T]:
    Val(T)
    fn add(self, o: Num[T]) -> Num[T]:
        return self
fn twice[T: Add](x: T) -> T:
    return x + x
a := Num.Val(1) + Num.Val(2)
b := twice(Num.Val(3))
";
    entry_ok(src);
}

#[test]
fn multi_param_generic_operator() {
    // A 2-param generic exercises a multi-entry receiving-type substitution {A->int, B->str}.
    let src = "\
struct Pair[A, B]:
    a: A
    b: B
    fn add(self, o: Pair[A, B]) -> Pair[A, B]:
        return self
p := Pair(1, \"x\") + Pair(2, \"y\")
";
    entry_ok(src);
}

#[test]
fn generic_struct_wrong_add_sig_rejected() {
    // BOUNDARY / soundness: a generic struct whose `add` has a genuinely WRONG signature
    // (`o: int -> int`, not `Box[T] -> Box[T]`) must STILL be rejected — no type laundering.
    let src = "\
struct Box[T]:
    v: T
    fn add(self, o: int) -> int:
        return 0
fn twice[T: Add](x: T) -> T:
    return x + x
v := twice(Box(5))
";
    entry_rejects(src, "does not satisfy Add");
}

#[test]
fn generic_struct_heterogeneous_add_rejected() {
    // BOUNDARY / soundness (no type laundering across type args): `Box[int] + Box[str]` must NOT
    // overload `add`. The user's `add(self, o: Box[T]) -> Box[T]` on a `Box[int]` receiver requires
    // `o: Box[int]`; admitting `Box[str]` would infer the result `Box[int]` for a value built from a
    // `Box[str]` → static type the checker cannot honor → runtime type confusion. The operator must
    // only fire when the operands' type args match (an exact-`Box[int]` pair).
    let src = "\
struct Box[T]:
    v: T
    fn add(self, o: Box[T]) -> Box[T]:
        return o
x := Box(5) + Box(\"hello\")
";
    entry_rejects(src, "cannot apply + to Box[int] and Box[str]");
}

#[test]
fn generic_struct_heterogeneous_compare_rejected() {
    // BOUNDARY (Bug 1, ordering variant): `Box[int] < Box[str]` must NOT overload `compare` — the
    // same heterogeneous-type-args laundering as `+`, via `ordering_allowed`.
    let src = "\
struct Box[T]:
    v: T
    fn compare(self, o: Box[T]) -> int:
        return 0
b := Box(5) < Box(\"hello\")
";
    entry_rejects(src, "cannot compare Box[int] and Box[str]");
}

#[test]
fn generic_enum_heterogeneous_add_rejected() {
    // BOUNDARY (Bug 1, enum analogue): a heterogeneous generic-enum pair must not overload `add`.
    let src = "\
enum Num[T]:
    Val(T)
    fn add(self, o: Num[T]) -> Num[T]:
        return o
x := Num.Val(1) + Num.Val(\"hello\")
";
    entry_rejects(src, "cannot apply + to Num[int] and Num[str]");
}

#[test]
fn generic_newtype_compare_via_method_rejected() {
    // BOUNDARY / soundness (Bug 2): a GENERIC newtype's `compare` method is NEVER dispatched at
    // runtime — same-newtype `<` ALWAYS auto-flows to the underlying's NATIVE ordering (vm
    // `compare_op` / interp `eval_binop`), ignoring the user `compare`. So the checker must NOT
    // accept `Comparable`/`<` for a generic newtype via its method (it would be check-ok / run-
    // divergent: a numeric underlying silently uses the native order; a non-orderable underlying
    // faults at runtime). The newtype operator-soundness gate must reject `Comparable` too, not
    // just Add/Sub/Mul/Div/Mod/Neg.
    let src = "\
newtype Wrap[T] = T:
    fn compare(self, o: Wrap[T]) -> int:
        return 0
b := Wrap(3) < Wrap(5)
";
    entry_rejects(src, "cannot compare");
}

#[test]
fn generic_newtype_compare_satisfies_comparable_rejected() {
    // Bug 2, the protocol-bound form: a generic newtype must NOT satisfy `Comparable` via its
    // `compare` method (no runtime dispatch path), so it cannot flow into a `[T: Comparable]` bound.
    let src = "\
newtype Wrap[T] = T:
    fn compare(self, o: Wrap[T]) -> int:
        return 0
fn pick[T: Comparable](x: T, y: T) -> T:
    if x < y:
        return x
    return y
v := pick(Wrap(1), Wrap(2))
";
    entry_rejects(src, "does not satisfy Comparable");
}

#[test]
fn redeclaring_add_rejected() {
    rejects(
        "protocol Add:\n    fn add(self, other: Self) -> Self\n",
        "reserved",
    );
}

// ----- transparent type aliases (M10-G3) -----

#[test]
fn type_alias_transparent_ok() {
    // UserId ≡ int: usable interchangeably in annotations and calls.
    ok(
        "type UserId = int\nfn double(n: int) -> int:\n    return n * 2\nid: UserId = 5\nx: int = id\ny := double(id)\n",
    );
}

#[test]
fn type_alias_mismatch_still_rejected() {
    // The alias is transparent, so a str where the underlying int is expected is still an error
    // (and the message names the resolved type, `int`).
    rejects(
        "type UserId = int\nid: UserId = \"no\"\n",
        "cannot assign str to variable of type int",
    );
}

#[test]
fn type_alias_to_collection_ok() {
    ok("type Scores = Map[str, int]\ns: Scores = {\"a\": 1}\nn: int = s[\"a\"]\n");
}

#[test]
fn type_alias_reserved_name_rejected() {
    rejects("type int = str\n", "reserved");
}

#[test]
fn recursive_type_alias_rejected() {
    rejects("type A = B\ntype B = A\nx: A = 1\n", "recursive type alias");
}

#[test]
fn type_alias_redeclared_rejected() {
    rejects("type T1 = int\ntype T1 = str\n", "already defined");
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
    rejects(
        &format!("{PAIR}p := Pair(1, \"x\")\nn: str = p.first\n"),
        "cannot assign int",
    );
}

#[test]
fn generic_struct_construction_and_field_ok() {
    ok(&format!(
        "{PAIR}p := Pair(1, \"x\")\nn: int = p.first\ns: str = p.second\n"
    ));
}

#[test]
fn generic_struct_method_return_substituted() {
    ok(&format!("{PAIR}p := Pair(7, \"x\")\nn: int = p.left()\n"));
    rejects(
        &format!("{PAIR}p := Pair(7, \"x\")\nn: str = p.left()\n"),
        "cannot assign int",
    );
}

#[test]
fn generic_struct_explicit_type_args_ok() {
    ok(&format!("{PAIR}p: Pair[str, int] = Pair(\"k\", 9)\n"));
}

#[test]
fn generic_struct_wrong_arity_rejected() {
    rejects(
        &format!("{PAIR}p: Pair[int] = Pair(1, 2)\n"),
        "expects 2 type argument(s)",
    );
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

// ----- method-level type parameters (F1) -----

// A method introducing its own fresh `[U]` beyond the struct's `[T]`.
const BOXMAP: &str = "\
struct Box[T]:
    v: T
    fn map_to[U](self, f: fn(T) -> U) -> U:
        return f(self.v)
";

#[test]
fn method_type_param_inferred_from_closure_ok() {
    // U is inferred from the closure's return type (str); the call type-checks and yields str.
    ok(&format!(
        "{BOXMAP}b := Box(5)\ns: str = b.map_to(fn(x: int) -> str: \"n{{x}}\")\n"
    ));
}

#[test]
fn method_type_param_result_type_substituted() {
    // U=str, so binding the result to an int must be rejected (proves U flowed into the return).
    rejects(
        &format!("{BOXMAP}b := Box(5)\nn: int = b.map_to(fn(x: int) -> str: \"n{{x}}\")\n"),
        "cannot assign str",
    );
}

#[test]
fn method_type_param_bound_satisfied_ok() {
    let src = "\
struct Box[T]:
    v: T
    fn biggest[U: Comparable](self, a: U, b: U) -> U:
        if a < b:
            return b
        return a
b := Box(0)
r: int = b.biggest(3, 7)
";
    ok(src);
}

#[test]
fn method_type_param_bound_enforced() {
    // U: Comparable, but Plain isn't Comparable — the method bound must be enforced at the call.
    let src = "\
struct Plain:
    n: int
struct Box[T]:
    v: T
    fn biggest[U: Comparable](self, a: U, b: U) -> U:
        if a < b:
            return b
        return a
b := Box(0)
r := b.biggest(Plain(1), Plain(2))
";
    rejects(src, "does not satisfy Comparable");
}

#[test]
fn method_type_param_arity_checked() {
    let src = "\
struct Box[T]:
    v: T
    fn one[U](self, a: U) -> U:
        return a
b := Box(0)
r := b.one(1, 2)
";
    rejects(src, "expects");
}

#[test]
fn method_type_param_shadowing_struct_param_rejected() {
    // A method type param reusing the struct's `T` is a confusing double-binding — reject it.
    let src = "\
struct Box[T]:
    v: T
    fn weird[T](self, x: T) -> T:
        return x
";
    rejects(src, "shadows");
}

// ----- user-defined parameterized protocols (F3, concrete-arg bounds) -----

// A user protocol with its own type param `T`, plus a struct conforming at `Container[int]`.
const CONTAINER: &str = "\
protocol Container[T]:
    fn get(self, i: int) -> T
    fn put(self, x: T)
struct IntBox:
    items: List[int]
    fn get(self, i: int) -> int:
        return self.items[i]
    fn put(self, x: int):
        self.items.push(x)
";

#[test]
fn param_protocol_decl_ok() {
    ok("protocol Container[T]:\n    fn get(self, i: int) -> T\n");
}

#[test]
fn param_protocol_bound_conformance_ok() {
    // IntBox satisfies Container[int] (T substituted to int), so first(b) type-checks.
    let src = format!(
        "{CONTAINER}fn first[X: Container[int]](c: X) -> int:\n    return c.get(0)\nb := IntBox([10, 20])\nn: int = first(b)\n"
    );
    ok(&src);
}

#[test]
fn param_protocol_body_return_substituted() {
    // Inside `first`, `c.get(0)` is `T` = int (from the bound's arg). Returning it as str is wrong.
    let src =
        format!("{CONTAINER}fn first[X: Container[int]](c: X) -> str:\n    return c.get(0)\n");
    rejects(&src, "int");
}

#[test]
fn param_protocol_bound_arity_mismatch_rejected() {
    // Container takes one type argument; a bare `Container` bound is an arity error.
    let src = format!("{CONTAINER}fn first[X: Container](c: X) -> int:\n    return c.get(0)\n");
    rejects(&src, "type argument");
}

#[test]
fn param_protocol_missing_method_rejected() {
    // A struct missing `put` does not satisfy Container[int].
    let src = "\
protocol Container[T]:
    fn get(self, i: int) -> T
    fn put(self, x: T)
struct Half:
    items: List[int]
    fn get(self, i: int) -> int:
        return self.items[i]
fn first[X: Container[int]](c: X) -> int:
    return c.get(0)
b := Half([1])
n := first(b)
";
    rejects(src, "does not satisfy Container");
}

#[test]
fn param_protocol_wrong_substituted_signature_rejected() {
    // `get` returns str, but Container[int] requires it return int.
    let src = "\
protocol Container[T]:
    fn get(self, i: int) -> T
struct StrBox:
    items: List[str]
    fn get(self, i: int) -> str:
        return self.items[i]
fn first[X: Container[int]](c: X) -> int:
    return 0
b := StrBox([\"a\"])
n := first(b)
";
    rejects(src, "does not satisfy Container");
}

#[test]
fn param_protocol_forwarding_wrong_arg_rejected() {
    // Review-panel CRITICAL: forwarding a `Container[str]` value into a `Container[int]` bound must
    // be rejected — the bound's type args have to match, not just the protocol name.
    let src = "\
protocol Container[T]:
    fn get(self, i: int) -> T
    fn size(self) -> int
struct StrBox:
    items: List[str]
    fn get(self, i: int) -> str:
        return self.items[i]
    fn size(self) -> int:
        return self.items.len()
fn total[X: Container[int]](c: X) -> int:
    return c.size()
fn forward[Y: Container[str]](c: Y) -> int:
    return total(c)
n := forward(StrBox([\"a\"]))
";
    rejects(src, "does not satisfy Container");
}

#[test]
fn param_protocol_forwarding_matching_arg_ok() {
    // The same forward with matching args (`Container[int]` → `Container[int]`) must still be accepted.
    let src = "\
protocol Container[T]:
    fn get(self, i: int) -> T
    fn size(self) -> int
struct IntBox:
    items: List[int]
    fn get(self, i: int) -> int:
        return self.items[i]
    fn size(self) -> int:
        return self.items.len()
fn total[X: Container[int]](c: X) -> int:
    return c.size()
fn forward[Y: Container[int]](c: Y) -> int:
    return total(c)
n := forward(IntBox([1]))
";
    ok(src);
}

#[test]
fn generic_method_without_receiver_rejected() {
    // Review-panel IMPORTANT: a method whose first param isn't a receiver must NOT silently bind the
    // receiver to the first declared param. Under the "no self ⇒ static" rule, a no-`self` method is
    // a STATIC method — calling it on an INSTANCE (`b.ident()`) is rejected (it is reached only as
    // `Box.ident(...)`). (`ident` here also declares its own `[U]`, but the instance-call rejection
    // fires first.)
    let src = "\
struct Box[T]:
    v: T
    fn ident[U]() -> U:
        pass
b := Box(5)
r := b.ident()
";
    rejects(src, "is a static method");
}

#[test]
fn param_protocol_as_value_type_accepted() {
    // A parameterized protocol is now a first-class value/annotation type (Q1). The carried arg is
    // witnessed at every store/pass boundary; a method returning the protocol's param recovers to the
    // carried concrete type (`c.get(0)` on a `Container[int]` yields `int`).
    let src = "\
protocol Container[T]:
    fn get(self, i: int) -> T
fn good(c: Container[int]) -> int:
    return c.get(0)
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

// ----- generic enums (type-erased) -----

/// A generic binary tree, the workhorse fixture for the generic-enum tests.
const TREE: &str = "\
enum Tree[T]:
    Leaf
    Node(T, Tree[T], Tree[T])
";

#[test]
fn generic_enum_construction_infers_type_arg_ok() {
    // Node(1, Leaf, Leaf) infers T=int; the value flows into a `Tree[int]` slot.
    ok(&format!(
        "{TREE}t: Tree[int] = Tree.Node(1, Tree.Leaf, Tree.Leaf)\n"
    ));
}

#[test]
fn generic_enum_construction_type_mismatch_rejected() {
    // First payload is T; an int and a str in the two Node arms can't both be T.
    rejects(
        &format!("{TREE}t := Tree.Node(1, Tree.Node(\"x\", Tree.Leaf, Tree.Leaf), Tree.Leaf)\n"),
        "expected",
    );
}

#[test]
fn generic_enum_annotation_arg_mismatch_rejected() {
    // A Tree[str] slot can't hold a Node whose payload infers T=int.
    rejects(
        &format!("{TREE}t: Tree[str] = Tree.Node(1, Tree.Leaf, Tree.Leaf)\n"),
        "cannot assign",
    );
}

#[test]
fn generic_enum_match_substitutes_payload_ok() {
    // The `v` bound by `Node(v, ...)` of a `Tree[int]` is int.
    let src = format!(
        "{TREE}fn first(t: Tree[int]) -> int:\n    match t:\n        Tree.Leaf: return 0\n        Tree.Node(v, l, r): return v\n"
    );
    ok(&src);
}

#[test]
fn generic_enum_match_payload_type_enforced() {
    // The `v` bound by `Node(v, ...)` of a `Tree[int]` is int, not str.
    let src = format!(
        "{TREE}fn bad(t: Tree[int]):\n    match t:\n        Tree.Leaf: print(\"l\")\n        Tree.Node(v, l, r):\n            s: str = v\n"
    );
    rejects(&src, "cannot assign int");
}

#[test]
fn generic_enum_wrong_arity_rejected() {
    rejects(
        &format!("{TREE}t: Tree[int, str] = Tree.Leaf\n"),
        "expects 1 type argument(s)",
    );
}

#[test]
fn bare_generic_enum_without_args_rejected() {
    rejects(
        &format!("{TREE}fn f(t: Tree) -> int:\n    return 0\n"),
        "expects 1 type argument(s), got 0",
    );
}

#[test]
fn generic_enum_multi_param_ok() {
    let src = "\
enum Either[A, B]:
    Left(A)
    Right(B)
fn fst(e: Either[int, str]) -> int:
    match e:
        Either.Left(a): return a
        Either.Right(b): return 0
";
    ok(src);
}

#[test]
fn generic_enum_nested_self_referential_ok() {
    // Cons holds T and a nested LinkedList[T]? — the payload references the enum's own param.
    let src = "\
enum LinkedList[T]:
    Nil
    Cons(T, LinkedList[T]?)
fn len(l: LinkedList[int]) -> int:
    match l:
        LinkedList.Nil: return 0
        LinkedList.Cons(h, t): return 1
";
    ok(src);
}

#[test]
fn generic_enum_bound_enforced_at_construction() {
    let src = "\
struct Plain:
    n: int
enum Box[T: Comparable]:
    Empty
    Has(T)
b := Box.Has(Plain(1))
";
    rejects(src, "does not satisfy Comparable");
}

#[test]
fn generic_enum_bound_satisfied_ok() {
    let src = "\
enum Box[T: Comparable]:
    Empty
    Has(T)
b: Box[int] = Box.Has(5)
";
    ok(src);
}

#[test]
fn generic_enum_unknown_bound_rejected() {
    rejects(
        "enum Box[T: Nope]:\n    Has(T)\n",
        "unknown protocol 'Nope'",
    );
}

#[test]
fn struct_and_enum_sharing_a_name_rejected() {
    // Review (Solidity lens): a struct and enum with the same name both registered silently,
    // the enum shadowed; with the merged `Name[args]` Display this surfaced as a nonsense
    // "cannot assign Foo[int] to … Foo[int]". Must be a clean "already defined" instead.
    rejects(
        "struct Foo:\n    n: int\nenum Foo:\n    A\n",
        "type 'Foo' is already defined",
    );
    rejects(
        "enum Bar:\n    A\nstruct Bar:\n    n: int\n",
        "type 'Bar' is already defined",
    );
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
    // `sort` is now file-backed as `where T: Comparable`; a non-Comparable element fails via the
    // standard bound-satisfaction diagnostic (retired the bespoke `sort() requires …` text).
    rejects(src, "does not satisfy Comparable");
}

#[test]
fn sort_on_primitive_list_still_ok() {
    ok("xs := [3, 1, 2]\nxs.sort()\nys := [\"b\", \"a\"]\nys.sort()\n");
}

#[test]
fn sort_with_args_rejected() {
    rejects("xs := [3, 1]\nxs.sort(5)\n", "expects 0 argument(s)");
}

// ----- user free-fn `where` clause enforcement (merged into type_params) -----

#[test]
fn user_fn_where_bound_enforced_at_call_site() {
    // `where T: Comparable` merges into the `[T]` param's bounds; a non-Comparable arg is rejected.
    let src = "\
struct Q:
    n: int
fn needs_cmp[T](x: T) where T: Comparable:
    y := x
q := Q(1)
needs_cmp(q)
";
    rejects(src, "does not satisfy Comparable");
}

#[test]
fn user_fn_where_bound_accepts_comparable_arg() {
    ok("fn needs_cmp[T](x: T) where T: Comparable:\n    y := x\nneeds_cmp(5)\n");
}

#[test]
fn user_fn_where_bound_used_inside_body() {
    // A `where`-bounded param may use the bounded operation in the body (`<` needs Comparable) —
    // proves the merge feeds the in-scope bound set, not just call-site enforcement.
    ok(
        "fn pick[T](a: T, b: T) -> T where T: Comparable:\n    if a < b:\n        return b\n    return a\nx := pick(1, 2)\n",
    );
}

#[test]
fn user_fn_where_names_unknown_param_rejected() {
    rejects(
        "fn bad[T]() where Q: Comparable:\n    y := 1\n",
        "unknown type parameter 'Q' in where-clause",
    );
}

#[test]
fn user_fn_where_bound_duplicating_decl_bound_reports_once() {
    // A bound named in BOTH `[T: Comparable]` and `where T: Comparable` must be deduped on merge —
    // a failing call emits the diagnostic exactly ONCE, not once per duplicated bound.
    let src = "\
fn f[T: Comparable](x: T) -> int where T: Comparable:
    return 1
f(true)
";
    let n = check_src(src)
        .iter()
        .filter(|e| e.message.contains("does not satisfy Comparable"))
        .count();
    assert_eq!(n, 1, "expected exactly one Comparable diagnostic, got {n}");
}

// ----- conditional methods: `where` on a user struct/enum/newtype method's RECEIVER type param -----

#[test]
fn conditional_method_where_receiver_param_accepted() {
    // A method may `where`-bound the ENCLOSING struct's own type param `T` (not the method's own
    // `[U]`). This is ACCEPTED (no "unknown type parameter") — it is a conditional method.
    ok("\
struct Box[T]:
    val: T
    fn top(self) -> T where T: Comparable:
        return self.val
b := Box(5)
x := b.top()
");
}

#[test]
fn conditional_method_accepts_comparable_receiver() {
    // int satisfies Comparable → the conditional method is callable.
    ok("\
struct Box[T]:
    val: T
    fn top(self) -> T where T: Comparable:
        return self.val
b := Box(5)
print(b.top())
");
}

#[test]
fn conditional_method_rejects_non_comparable_receiver() {
    // A `Box` of a plain (non-Comparable) struct → the conditional method's receiver bound fails
    // at the CALL site with the standard bound-satisfaction diagnostic.
    rejects(
        "\
struct Q:
    n: int
struct Box[T]:
    val: T
    fn top(self) -> T where T: Comparable:
        return self.val
b := Box(Q(1))
x := b.top()
",
        "does not satisfy Comparable",
    );
}

#[test]
fn conditional_method_operator_dispatch_enforces_receiver_bound() {
    // SOUNDNESS regression: a conditional method that IMPLEMENTS an operator protocol's method
    // (`compare` ⇒ Comparable) makes the type STRUCTURALLY satisfy that protocol — but only when the
    // receiver's `where T: Comparable` holds. Operator syntax (`a < b`) resolves the method through
    // `satisfies` (NOT the explicit-call path), so the bound must be enforced INSIDE satisfies or the
    // operator bypasses it (check-ok / run-diverge). `Box[Q]` (Q not Comparable) must reject `<`.
    let prog = |val: &str, op: &str| {
        format!(
            "\
struct Q:
    n: int
struct Box[T]:
    val: T
    fn compare(self, other: Box[T]) -> int where T: Comparable:
        if self.val < other.val:
            return -1
        return 0
a := Box({val})
b := Box({val})
{op}
"
        )
    };
    // int IS Comparable → Box[int] is conditionally Comparable → `<` is allowed.
    ok(&prog("1", "print(a < b)"));
    // Q is NOT Comparable → Box[Q] must NOT satisfy Comparable → `<` is rejected at CHECK time.
    rejects(&prog("Q(1)", "print(a < b)"), "cannot compare");
}

#[test]
fn conditional_method_as_generic_bound_arg_enforces() {
    // SOUNDNESS regression (the other structural-satisfies path): passing `Box[Q]` where a
    // `[U: Comparable]` bound is expected must reject — `satisfies(Box[Q], Comparable)` has to see
    // the receiver `where` fail. `Box[int]` is accepted.
    let prog = |val: &str| {
        format!(
            "\
struct Q:
    n: int
struct Box[T]:
    val: T
    fn compare(self, other: Box[T]) -> int where T: Comparable:
        return 0
fn need_cmp[U: Comparable](x: U) -> U:
    return x
y := need_cmp(Box({val}))
"
        )
    };
    ok(&prog("1"));
    rejects(&prog("Q(1)"), "does not satisfy Comparable");
}

#[test]
fn conditional_method_enum_where_receiver_param() {
    // Enum receiver-param conditional method: accept on a Comparable payload, reject on a
    // non-Comparable one.
    ok("\
enum Opt[T]:
    Some(T)
    None
    fn peek(self) -> int where T: Comparable:
        return 1
o := Opt.Some(5)
x := o.peek()
");
    rejects(
        "\
struct Q:
    n: int
enum Opt[T]:
    Some(T)
    None
    fn peek(self) -> int where T: Comparable:
        return 1
o := Opt.Some(Q(1))
x := o.peek()
",
        "does not satisfy Comparable",
    );
}

#[test]
fn conditional_method_newtype_where_receiver_enforced() {
    // A generic newtype method may carry a receiver-param `where` too — `fn_sig` is shared, so it
    // must be ENFORCED at the newtype arm (else accept-without-enforce soundness hole).
    ok("\
newtype Stack[T] = List[T]:
    fn top(self) -> int where T: Comparable:
        return 1
s := Stack([5])
x := s.top()
");
    rejects(
        "\
struct Q:
    n: int
newtype Stack[T] = List[T]:
    fn top(self) -> int where T: Comparable:
        return 1
s := Stack([Q(1)])
x := s.top()
",
        "does not satisfy Comparable",
    );
}

#[test]
fn conditional_method_own_u_param_where_still_merges() {
    // REGRESSION: a method with its OWN `[U]` and `where U: Comparable` still merges into the
    // method's generic-param bounds and enforces via the generic-method path (unchanged by the
    // receiver-bound diversion).
    ok("\
struct Box[T]:
    val: T
    fn cmp[U](self, other: U) -> int where U: Comparable:
        return 1
b := Box(5)
x := b.cmp(3)
");
    rejects(
        "\
struct Q:
    n: int
struct Box[T]:
    val: T
    fn cmp[U](self, other: U) -> int where U: Comparable:
        return 1
b := Box(5)
x := b.cmp(Q(1))
",
        "does not satisfy Comparable",
    );
}

#[test]
fn conditional_method_unknown_receiver_arg_defers() {
    // CRITICAL characterization: enforce_bounds DEFERS on a receiver type-arg that is still
    // `Unknown` — exactly like `[].sort()` (`satisfies_args_d` returns Ok for `Ty::Unknown`, "don't
    // cascade"). `Box([])` leaves T un-pinnable here (calling `.top()` on the empty-constructed
    // receiver freezes its elem as Unknown — a PRE-EXISTING scope-wide-pin limitation, reproducible
    // with a NO-`where` `top`). The invariant is that the conditional bound adds NO spurious
    // "does not satisfy" — a genuinely-never-pinned receiver fails only at the pre-existing
    // "cannot infer element type" binding error, NOT a bound error.
    let src = "\
struct Box[T]:
    val: List[T]
    fn top(self) -> List[T] where T: Comparable:
        return self.val
b := Box([])
x := b.top()
";
    let errs = check_src(src);
    assert!(
        !errs.iter().any(|e| e.message.contains("does not satisfy")),
        "conditional bound must DEFER on an Unknown receiver arg (no spurious reject), got: {errs:?}"
    );
    // Documents the observed pre-existing behavior (independent of the where-clause).
    assert!(
        errs.iter()
            .any(|e| e.message.contains("cannot infer element type")),
        "expected the pre-existing never-pinned binding error, got: {errs:?}"
    );
}

#[test]
fn conditional_method_pinned_receiver_arg_ok() {
    // Companion: when the receiver's type arg IS pinned to a Comparable concrete type (via a typed
    // field annotation), the conditional method is callable — the defer path does not over-reject a
    // resolved-but-late arg.
    ok("\
struct Box[T]:
    val: List[T]
    fn top(self) -> List[T] where T: Comparable:
        return self.val
b: Box[int] = Box([])
x := b.top()
");
}

#[test]
fn conditional_static_method_rejects_non_comparable() {
    // A STATIC method (no `self`) may carry a receiver-param `where` too — `fn_sig` is shared and
    // cannot tell a static from an instance method. It MUST be enforced on the static-dispatch path
    // (`infer_static_call`), else a conditional factory silently accepts a non-satisfying type arg.
    ok("\
struct Box[T]:
    val: T
    fn of(x: T) -> Box[T] where T: Comparable:
        return Box(x)
b := Box.of(5)
");
    rejects(
        "\
struct Q:
    n: int
struct Box[T]:
    val: T
    fn of(x: T) -> Box[T] where T: Comparable:
        return Box(x)
b := Box.of(Q(1))
",
        "does not satisfy Comparable",
    );
}

#[test]
fn conditional_static_enum_method_rejects_non_comparable() {
    // Mirror for an enum static factory reached as `Enum.method(...)`.
    ok("\
enum Opt[T]:
    Some(T)
    None
    fn build(x: T) -> Opt[T] where T: Comparable:
        return Opt.Some(x)
o := Opt.build(5)
");
    rejects(
        "\
struct Q:
    n: int
enum Opt[T]:
    Some(T)
    None
    fn build(x: T) -> Opt[T] where T: Comparable:
        return Opt.Some(x)
o := Opt.build(Q(1))
",
        "does not satisfy Comparable",
    );
}

#[test]
fn conditional_static_method_on_value_single_diagnostic() {
    // A static method that carries a receiver `where`-bound, wrongly called on a VALUE, must emit
    // ONLY the static-method diagnostic — NOT also a spurious "does not satisfy" from the receiver
    // bound (the enforcement must fire AFTER the `is_static` rejection, not before).
    let src = "\
struct Q:
    n: int
struct Box[T]:
    val: T
    fn mk(x: T) -> T where T: Comparable:
        return x
b := Box(Q(1))
y := b.mk(Q(1))
";
    let errs = check_src(src);
    let bound = errs
        .iter()
        .filter(|e| e.message.contains("does not satisfy"))
        .count();
    assert_eq!(
        bound, 0,
        "no spurious bound error on a static-on-value call, got: {errs:?}"
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("is a static method")),
        "expected the static-method diagnostic, got: {errs:?}"
    );
}

#[test]
fn conditional_method_body_uses_receiver_bound() {
    // BUG-2 REGRESSION: a conditional method whose BODY uses the receiver bound (`<` needs
    // Comparable) must type-check — the receiver `where T: Comparable` puts the bound in scope on
    // the ENCLOSING param during the body check, exactly as a free fn's `where` does (symmetry with
    // `user_fn_where_bound_used_inside_body`). Before the fix the body errored `cannot compare
    // T and T` because the receiver bound was recorded on `sig.where_bounds` (call-site only) and
    // never merged into the in-scope enclosing param `T`.
    ok("\
struct Box[T]:
    val: T
    fn max2(self, other: T) -> T where T: Comparable:
        if self.val < other:
            return other
        return self.val
b := Box(5)
x := b.max2(3)
");
}

#[test]
fn conditional_enum_method_body_uses_receiver_bound() {
    // BUG-2 mirror for an enum conditional method (check_fn_body is shared across struct/enum/
    // newtype, so the single fix covers all three).
    ok("\
enum Wrap[T]:
    V(T)
    fn bigger(self, a: T, b: T) -> T where T: Comparable:
        if a < b:
            return b
        return a
w := Wrap.V(5)
x := w.bigger(1, 2)
");
}

#[test]
fn conditional_static_method_redundant_decl_bound_reports_once() {
    // BUG-1 REGRESSION: when the enclosing type already declares `[T: Comparable]` AND the static
    // method repeats `where T: Comparable`, the static-dispatch path (`infer_static_call`) must not
    // enforce the bound TWICE — once via the struct decl bound (`tps`) and once via the receiver
    // where-bound (`sig.where_bounds`). The failing factory call must emit the diagnostic exactly
    // ONCE. Fixed by deduping the receiver-bound against the enclosing param's declared bounds in
    // `fn_sig`.
    let src = "\
struct Q:
    n: int
struct Box[T: Comparable]:
    val: T
    fn of(x: T) -> Box[T] where T: Comparable:
        return Box(x)
b := Box.of(Q(1))
";
    let n = check_src(src)
        .iter()
        .filter(|e| e.message.contains("does not satisfy Comparable"))
        .count();
    assert_eq!(n, 1, "expected exactly one Comparable diagnostic, got {n}");
}

// ----- file-backed `sort` via `where T: Comparable` (port off the bespoke arm) -----

#[test]
fn sort_where_clause_returns_nil() {
    // The file-backed `native fn sort(self) -> nil where T: Comparable` resolves to a nil return —
    // using it as a value is rejected exactly like before (regression guard on the ported sig).
    rejects("xs := [3, 1, 2]\nn := xs.sort() + 1\n", "no value (nil)");
}

#[test]
fn sort_where_clause_non_comparable_reports_satisfies_diagnostic() {
    // A non-Comparable element list now fails via the `where T: Comparable` bound-enforcement path,
    // so the message is the standard `does not satisfy Comparable` diagnostic (not the retired
    // bespoke `sort() requires …` text).
    let src = "\
struct Q:
    n: int
xs := [Q(2), Q(1)]
xs.sort()
";
    rejects(src, "does not satisfy Comparable");
}

#[test]
fn sort_where_clause_bool_list_rejected() {
    rejects(
        "xs := [true, false]\nxs.sort()\n",
        "does not satisfy Comparable",
    );
}

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
    rejects(
        "xs := [1, 2, 3]\nxs[0] += 1.5\n",
        "cannot apply += to int and float",
    );
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
    ok("fn f():\n    return 5\nx := f()\ny := x + 1\n");
}

#[test]
fn inferred_return_from_expression() {
    ok("fn add(a: int, b: int):\n    return a + b\nx := add(1, 2)\ny := x + 1\n");
}

#[test]
fn void_preserved_when_no_value_return() {
    // No value-return → infers `nil`; binding the void result to a value (`x := log(...)`) is now
    // rejected up-front (Part 2: nil in value position), before it can flow into an arithmetic op.
    rejects(
        "fn log(m: str):\n    print(m)\nx := log(\"h\")\ny := x + 1\n",
        "no value (nil)",
    );
}

#[test]
fn inferred_return_in_if_branch() {
    ok("fn f(c: bool):\n    if c:\n        return 1\n    return 2\nx := f(true)\ny := x + 1\n");
}

#[test]
fn inferred_return_from_accumulator_local() {
    ok(
        "fn sum(xs: List[int]):\n    total := 0\n    for x in xs:\n        total += x\n    return total\nn := sum([1, 2, 3])\nm := n + 1\n",
    );
}

#[test]
fn inferred_return_recursive() {
    ok(
        "fn fib(n: int):\n    if n < 2:\n        return n\n    return fib(n - 1) + fib(n - 2)\nx := fib(10)\ny := x + 1\n",
    );
}

#[test]
fn inferred_return_conflict_rejected() {
    // Multi-branch JOIN: int and str do not merge (no common-supertype search) → a CONFLICT that
    // names both branches, rather than the old first-branch-wins `expected int, found str`.
    rejects(
        "fn f(c: bool):\n    if c:\n        return 1\n    return \"x\"\n",
        "conflicting branches (int vs str)",
    );
}

#[test]
fn inferred_result_return() {
    ok(
        "fn d(a: int, b: int):\n    if b == 0:\n        return Err(\"divide by zero\")\n    return Ok(a / b)\nmatch d(10, 2):\n    Ok(v): print(\"got {v}\")\n    Err(e): print(e)\n",
    );
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
    ok(
        "fn g(n: int):\n    return n * 2\nfn f(n: int):\n    return g(n) + 1\nx := f(3)\ny := x + 1\n",
    );
}

#[test]
fn inferred_forward_ref_callee_later_is_permissive() {
    // Callee defined *after* the caller (both un-annotated): no fixpoint, so the caller infers
    // `Unknown` and stays permissive — crucially NOT a spurious "returns nothing" error.
    ok(
        "fn f(n: int):\n    return g(n) + 1\nfn g(n: int):\n    return n * 2\nx := f(3)\ny := x + 1\n",
    );
}

#[test]
fn inferred_recursion_only_rejected() {
    // A body whose only return is a self-recursive call has NO concrete base, so its return type is
    // genuinely un-inferable: the fixpoint leaves it `Unknown`, and the FINALIZE pass now REJECTS
    // that residual (the leak-fix: `Unknown` must not leak permissively out of a return). Annotate it.
    rejects(
        "fn loopy(n: int):\n    return loopy(n - 1)\n",
        "cannot infer return type of 'loopy'",
    );
}

#[test]
fn inferred_recursion_only_with_annotation_ok() {
    // An explicit `-> int` annotation bypasses inference entirely (the cleanest way to type a
    // self-recursive-only function).
    ok("fn loopy(n: int) -> int:\n    return loopy(n - 1)\n");
}

#[test]
fn inferred_forward_ref_recursive_rejects_wrong_slot() {
    // o2.chz: `rec` forward-references `base` (defined AFTER) and self-recurses. Order-independent
    // fixpoint inference resolves `base -> str`, then `rec -> str`, so feeding `rec(2)` into an
    // `int` slot is correctly rejected (was wrongly accepted under single-pass source-order infer).
    rejects(
        "fn rec(n: int):\n    if n <= 0:\n        return base(0)\n    return rec(n - 1)\nfn base(n: int):\n    return \"hello\"\nv: int = rec(2)\n",
        "cannot assign str to variable of type int",
    );
}

#[test]
fn inferred_mutual_recursion_with_base_resolves() {
    // Mutual recursion with a concrete base: `a` has base `return 1` (int) but also forward+mutual
    // calls `b`; `b` returns `a(...)`. Only the fixpoint resolves `a -> int` then `b -> int`, so
    // `v: str = b(5)` is rejected.
    rejects(
        "fn a(n: int):\n    if n <= 0:\n        return 1\n    return b(n - 1)\nfn b(n: int):\n    return a(n - 1)\nv: str = b(5)\n",
        "cannot assign int to variable of type str",
    );
}

#[test]
fn inferred_pure_mutual_recursion_rejected() {
    // Pure mutual recursion with NO concrete base anywhere: both returns stay `Unknown` after the
    // fixpoint, and the FINALIZE pass now REJECTS each residual (the leak-fix — same policy as the
    // self-recursive-only case above). Both need a `-> T` annotation.
    rejects(
        "fn a(n: int):\n    return b(n - 1)\nfn b(n: int):\n    return a(n - 1)\n",
        "cannot infer return type",
    );
}

#[test]
fn non_recursive_unknown_return_not_falsely_rejected() {
    // Regression: a NON-recursive un-annotated fn whose return infers `Unknown` for a reason
    // unrelated to recursion must NOT be rejected by the recursive-return inference. PART A now
    // requires the empty `x := []` to be annotated, so the annotated form drives this: `x[0]` is a
    // concrete `int`, the return infers `int`, and the recursive-return fixpoint must not regress.
    ok("fn f():\n    x: List[int] = []\n    return x[0]\nprint(\"ok\")\n");
    // The un-annotated empty itself is the sibling producer's domain — it is now its own error.
    rejects(
        "fn f():\n    x := []\n    return x[0]\nf()\n",
        "empty collection",
    );
}

#[test]
fn errored_body_unknown_return_reports_once() {
    // Regression: a fn whose body has a real error (undefined name) infers `Unknown`; the fixpoint
    // change must not pile a spurious "cannot infer return type" on top of the genuine error.
    let errs = check_src("fn f():\n    return undefined_fn()\n");
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert!(
        errs[0].message.contains("unknown name"),
        "got: {:?}",
        errs[0]
    );
}

#[test]
fn fact_still_infers_int() {
    // Regression guard: a self-recursive function with a CONCRETE literal base case already infers
    // correctly today (base case `int` wins; the self-call's `Unknown` is ignored). The fixpoint
    // must not perturb this.
    ok(
        "fn fact(n: int):\n    if n <= 1:\n        return 1\n    return n * fact(n - 1)\nx := fact(5)\ny := x + 1\n",
    );
}

// ===== 9c. multi-branch return-type inference: JOIN merge + finalize (leak fix) =====

#[test]
fn infer_ok_err_branches_merge_slotwise() {
    // (a) Ok/Err sibling branches merge the T-slot: Ok("h")=Result[str,?] ⊔ Err("a")=Result[?,str]
    // → Result[str, Error] (the E-slot is NOT pinned from the Err payload — an inferred error slot
    // always defaults to `Error`). So `res()?` is str and `y: int = x` must ERROR.
    entry_rejects(
        "fn res():\n    if false:\n        return Err(\"a\")\n    return Ok(\"h\")\nfn caller() -> str!:\n    x := res()?\n    y: int = x\n    return Ok(x)\nfn main():\n    pass\n",
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
        "struct DbErr:\n    code: int\n    fn message(self) -> str:\n        return \"db\"\nfn ok():\n    return Ok(5)\nfn caller() -> int!DbErr:\n    x := ok()?\n    return Ok(x)\nfn main():\n    pass\n",
        "propagates error Error",
    );
}

#[test]
fn infer_err_only_uninferable_errors() {
    // (c) `fn err(): return Err("x")` — T is un-inferable (no default for the value slot), so it
    // must ERROR like the empty-collection diagnostic. Today it leaks Result[Unknown, str].
    entry_rejects(
        "fn err():\n    return Err(\"x\")\nfn main():\n    pass\n",
        "cannot infer return type of 'err'",
    );
}

#[test]
fn infer_none_only_uninferable_errors() {
    // (d) `fn none(): return None` — T un-inferable → ERROR (return position is stricter than the
    // binding-position `x := None`, which stays legal).
    entry_rejects(
        "fn none():\n    return None\nfn main():\n    pass\n",
        "cannot infer return type",
    );
}

#[test]
fn infer_empty_list_return_uninferable_errors() {
    // (e) `fn f(): return []` — element un-inferable → ERROR. Today leaks List[Unknown] (the
    // empty-collection error is binding-scoped and never fires at a return position).
    entry_rejects(
        "fn f():\n    return []\nfn main():\n    pass\n",
        "cannot infer return type",
    );
}

#[test]
fn infer_uninferable_unknown_in_concurrency_box_errors() {
    // REGRESSION (adversarial review parity-perf-0): a residual `Unknown` nested inside a
    // concurrency box (Shared/Atomic/RwShared/Channel) or a function type must ALSO be rejected —
    // `fill_ret`'s original catch-all skipped these containers, so `return Shared([])` laundered a
    // `Shared[List[Unknown]]` past the rejector (`.get()` then assignable to both List[int] and
    // List[str]). Each un-inferable box now errors like `return []` does.
    for box_ctor in ["Shared([])", "Atomic([])", "RwShared([])"] {
        entry_rejects(
            &format!(
                "import std.concurrency\nfn f():\n    return {box_ctor}\nfn main():\n    pass\n"
            ),
            "cannot infer return type",
        );
    }
    // the full laundering the leak enabled — both incompatible assignments off one `.get()`.
    entry_rejects(
        "import std.concurrency\nfn f():\n    return Shared([])\nfn main():\n    s := f()\n    a: List[int] = s.get()\n    b: List[str] = s.get()\n",
        "cannot infer return type",
    );
}

#[test]
fn infer_concurrency_box_with_inferable_element_ok() {
    // NEIGHBOR: a box whose element IS inferable from the constructor value stays legal (the fix
    // only flags a residual Unknown, never a resolved element).
    entry_ok(
        "import std.concurrency\nfn f():\n    return Shared([1])\nfn main():\n    s := f()\n    a: List[int] = s.get()\n    print(a)\n",
    );
}

#[test]
fn multibranch_return_ok_err_no_error() {
    // NEIGHBOR: Ok(5) + Err("x") → Result[int, Error] (T fills from the Ok branch; the E-slot
    // always defaults to `Error`, not the Err payload's `str`). No error; `e` binds as `Error` and
    // `print(e)` accepts it.
    entry_ok(
        "fn f(c: bool):\n    if c:\n        return Ok(5)\n    return Err(\"x\")\nfn main():\n    match f(true):\n        Ok(v): print(v)\n        Err(e): print(e)\n",
    );
}

#[test]
fn multibranch_return_some_none_no_error() {
    // NEIGHBOR: Some(5) + None → Option[int], no error (T filled from the Some branch).
    entry_ok(
        "fn f(c: bool):\n    if c:\n        return Some(5)\n    return None\nfn main():\n    match f(true):\n        Some(v): print(v)\n        None: print(\"none\")\n",
    );
}

#[test]
fn multibranch_return_empty_and_nonempty_list_no_error() {
    // NEIGHBOR: [] + [1, 2] → List[int], no error (empty element filled from the non-empty sibling).
    entry_ok(
        "fn f(c: bool):\n    if c:\n        return []\n    return [1, 2]\nfn main():\n    xs := f(true)\n    print(xs)\n",
    );
}

#[test]
fn multibranch_void_stays_nil_no_error() {
    // NEIGHBOR: a fn with no value-return stays void/nil (nil != Unknown → no "cannot infer").
    entry_ok("fn f():\n    print(1)\nfn main():\n    f()\n");
}

#[test]
fn multibranch_int_float_conflicts_not_inferred() {
    // Mixed int/float sibling branches CONFLICT (annotate `-> float` to opt in). Inferring `float`
    // here would set the static type to float WITHOUT the compiler emitting `Op::CoerceFloat` (it
    // reads `decl.ret`, the annotation, not the inferred ret), leaving a runtime int under a float
    // type — `x / 2` would do integer division. Widening is a SINK-only rule (an inferred return is
    // not a sink).
    entry_rejects(
        "fn f(c: bool):\n    if c:\n        return 1\n    return 2.0\nfn main():\n    pass\n",
        "conflicting branches",
    );
    // …but an explicit `-> float` annotation DOES widen (the real sink emits the coercion).
    entry_ok(
        "fn f(c: bool) -> float:\n    if c:\n        return 1\n    return 2.0\nfn main():\n    x := f(true)\n    print(x / 2)\n",
    );
}

#[test]
fn multibranch_generic_identity_preserves_param() {
    // NEIGHBOR: generic `fn id[T](x: T): return x` → T (Ty::Param preserved, no finalize error).
    entry_ok("fn id[T](x: T):\n    return x\nfn main():\n    print(id(5))\n    print(id(\"h\"))\n");
}

#[test]
fn multibranch_two_structs_conflict() {
    // NEIGHBOR (must ERROR): two distinct concrete structs across branches CONFLICT — never join to
    // a shared protocol or Any. A protocol return must be spelled explicitly.
    entry_rejects(
        "struct A:\n    x: int\n    fn speak(self) -> str:\n        return \"a\"\nstruct B:\n    y: int\n    fn speak(self) -> str:\n        return \"b\"\nfn f(c: bool):\n    if c:\n        return A(1)\n    return B(2)\nfn main():\n    pass\n",
        "conflicting branches",
    );
}

#[test]
fn infer_ok_err_mixed_defaults_e_to_error() {
    // The Err-branch payload does NOT pin E: `Ok("h")` + `Err("a")` infers `Result[str, Error]`,
    // not `Result[str, str]`. Proof: propagating `res()?` into a `-> str!DbErr` fn hits the
    // Error-vs-DbErr mismatch exactly like the annotated `-> str!` version would.
    entry_rejects(
        &format!(
            "{DBERR}fn res(c: bool):\n    if c:\n        return Err(\"a\")\n    return Ok(\"h\")\nfn caller() -> str!DbErr:\n    x := res(true)?\n    return Ok(x)\nfn main():\n    pass\n"
        ),
        "propagates error Error",
    );
}

#[test]
fn infer_distinct_err_payloads_no_conflict() {
    // Two branches with DIFFERENT Err payload types no longer conflict on the E-slot (both finalize
    // to `Error`); the Ok branch pins T=int. `e` binds as `Error` → `e.message()` is available.
    entry_ok(
        "struct EA:\n    a: int\n    fn message(self) -> str:\n        return \"EA\"\nstruct EB:\n    b: int\n    fn message(self) -> str:\n        return \"EB\"\nfn f(k: int):\n    if k == 0:\n        return Err(EA(1))\n    if k == 1:\n        return Err(EB(2))\n    return Ok(5)\nfn main():\n    match f(2):\n        Ok(v): print(v)\n        Err(e): print(e.message())\n",
    );
}

#[test]
fn infer_expr_non_error_payload_not_laundered() {
    // SOUNDNESS (adversarial-review): the if/match-EXPRESSION E-default must NOT force a concrete
    // NON-Error payload to `Error` — that path has no post-hoc assignability re-check, so laundering
    // `MyErr` (no `message`) into the `Error` existential would make `e.message()` check-pass then
    // fault at runtime. E is kept concrete → the method-call check rejects it at check time.
    entry_rejects(
        "struct MyErr:\n    code: int\nfn foo(c: bool) -> Result[int, MyErr]:\n    if c:\n        return Err(MyErr(1))\n    return Ok(5)\nfn main():\n    c := true\n    x := if c: foo(true) else: foo(false)\n    match x:\n        Ok(v): print(v)\n        Err(e): print(e.message())\n",
        "no method 'message'",
    );
    // A bare non-Error scalar payload (`Err(42)`) is likewise preserved as `int`, not laundered.
    entry_rejects(
        "fn main():\n    c := true\n    x := if c: Err(42) else: Err(43)\n    match x:\n        Ok(v): print(v)\n        Err(e): print(e.message())\n",
        "no method 'message'",
    );
}

#[test]
fn infer_return_non_error_payload_preserved_no_over_reject() {
    // NO OVER-REJECTION (adversarial-review): forwarding a `Result` whose E does NOT satisfy `Error`
    // must still type-check — the inferred E is kept concrete (`MyErr`), not forced to `Error` (which
    // pass-2 would then reject as `Result[int, Error]` vs the actual `Result[int, MyErr]`).
    entry_ok(
        "struct MyErr:\n    code: int\nfn foo(c: bool) -> Result[int, MyErr]:\n    if c:\n        return Err(MyErr(1))\n    return Ok(5)\nfn wrap(c: bool):\n    return foo(c)\nfn main():\n    match wrap(true):\n        Ok(v): print(v)\n        Err(e): print(e.code)\n",
    );
    // …but calling an Error-only method on that preserved concrete `MyErr` is still rejected (sound).
    entry_rejects(
        "struct MyErr:\n    code: int\nfn foo(c: bool) -> Result[int, MyErr]:\n    if c:\n        return Err(MyErr(1))\n    return Ok(5)\nfn wrap(c: bool):\n    return foo(c)\nfn main():\n    match wrap(true):\n        Ok(v): print(v)\n        Err(e): print(e.message())\n",
        "no method 'message'",
    );
}

#[test]
fn explicit_return_result_keeps_concrete_e() {
    // REGRESSION GUARD: the E-default is inference-ONLY. An EXPLICIT `-> Result[str, str]` annotation
    // (resolved by `resolve_type`, bypassing inference) keeps the concrete `str` error slot — so
    // matching `Err(e)` gives `e: str` and `e.trim()` (a str method, not on `Error`) type-checks.
    entry_ok(
        "fn res(c: bool) -> Result[str, str]:\n    if c:\n        return Err(\"a\")\n    return Ok(\"h\")\nfn main():\n    match res(true):\n        Ok(v): print(v)\n        Err(e): print(e.trim())\n",
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
            "{DBERR}fn g() -> int!DbErr:\n    x := if true: Ok(1) else: Ok(2)\n    return x?\nfn main():\n    pass\n"
        ),
        "propagates error Error",
    );
}

#[test]
fn match_expr_all_ok_defaults_result_e_to_error() {
    // Same E-default on the match-EXPRESSION surface.
    entry_rejects(
        &format!(
            "{DBERR}fn g() -> int!DbErr:\n    k := 1\n    x := match k:\n        1: Ok(1)\n        _: Ok(2)\n    return x?\nfn main():\n    pass\n"
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
        "fn get_err() -> int!:\n    return Err(\"boom\")\nfn main():\n    c := true\n    x := if c: get_err() else: Err(\"other\")\n    print(\"ok\")\n",
    );
}

#[test]
fn if_expr_edefault_keeps_binding_leniency_and_neighbors() {
    // The E-default must NOT import return-position strictness: an un-inferable if-expr bound via `:=`
    // stays as lenient as the equivalent direct binding.
    entry_ok("fn main():\n    x := if true: None else: None\n    print(x)\n"); // like `x := None`
    // T-merge across Ok/Err still works; annotated form still works.
    entry_ok("fn main():\n    x := if true: Some(3) else: None\n    print(x)\n");
    entry_ok(
        "fn main():\n    x: Result[str, str] = if true: Ok(\"a\") else: Err(\"b\")\n    print(x)\n",
    );
    // An untyped int-CONST branch beside a float-CONST branch now WIDENS to float (the
    // `literal_numeric_mix` peephole — consistent with the list literal `[3, 4.0]`; the compiler
    // emits `Op::CoerceFloat` on the int branch, so it is sound, not int-under-float).
    entry_ok("fn main():\n    x := if true: 3 else: 4.0\n    print(x)\n");
}

#[test]
fn multibranch_annotated_float_still_widens() {
    // NEIGHBOR: the ANNOTATED-return widening path is untouched (`-> float: return 3` still widens).
    entry_ok("fn f() -> float:\n    return 3\nfn main():\n    print(f())\n");
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
        "fn main():\n    f := fn(): Err(\"x\")\n    print(f)\n",
        "cannot infer return type",
    );
}

#[test]
fn closure_free_ok_defaults_error_e() {
    // CLOSURE: a free `fn(): Ok(5)` finalizes to Result[int, Error] (E default), no leak error.
    entry_ok(
        "fn main():\n    f := fn(): Ok(5)\n    x := f()\n    match x:\n        Ok(v): print(v)\n        Err(e): print(e.message())\n",
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
        "fn helper() -> str:\n    return \"top\"\nfn outer(c: bool):\n    fn helper() -> str:\n        return \"nested\"\n    return 42\nx := outer(true)\ny := x + 1\n",
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
/// must be REJECTED, not declared as a shadowing local: the compiler resolves the builtin BEFORE a
/// local value-call, so binding it would type calls to the nested fn while the VM runs the builtin —
/// the exact check-OK/run-divergent hole this task exists to close. (Base branch bug 2.)
#[test]
fn nested_fn_shadows_reserved_builtin_rejected() {
    // `fn print(...)` inside a wrapper: checker typed `print(5)` as the nested int fn, VM ran the
    // builtin print → `v` nil → run-fault. Now a clean check-time reject.
    entry_rejects(
        "fn wrapper() -> int:\n    fn print(x: int) -> int:\n        return x + 1\n    return print(5)\nv := wrapper()\nprint(v)\n",
        "reserved",
    );
    // `fn range(...)` — same family, silent wrong value on base.
    entry_rejects(
        "fn wrapper() -> int:\n    fn range(x: int) -> int:\n        return x\n    return range(5)\nprint(wrapper())\n",
        "reserved",
    );
}

/// A nested fn named after a same-module STRUCT constructor is REJECTED (the compiler resolves the
/// bare struct ctor before a local → check-OK/run-fault on base).
#[test]
fn nested_fn_shadows_struct_ctor_rejected() {
    entry_rejects(
        "struct P:\n    x: int\nfn outer() -> int:\n    fn P() -> int:\n        return 99\n    return P()\nprint(outer())\n",
        "reserved",
    );
}

/// A nested fn named after a same-module NEWTYPE constructor is REJECTED (bare newtype ctor wins in
/// the compiler → check-OK/run-divergent on base, printed `UserId(<closure>)`).
#[test]
fn nested_fn_shadows_newtype_ctor_rejected() {
    entry_rejects(
        "newtype UserId = int\nfn outer() -> int:\n    fn UserId() -> int:\n        return 99\n    return UserId()\nprint(outer())\n",
        "reserved",
    );
}

/// A nested fn named after a BUILTIN variant ctor (`Ok`/`Err`/`Some`/`None`) is REJECTED (the
/// compiler resolves the bare builtin variant before a local → check-OK/run-fault on base).
#[test]
fn nested_fn_shadows_builtin_variant_rejected() {
    entry_rejects(
        "fn outer() -> int:\n    fn Ok() -> int:\n        return 99\n    return Ok()\nprint(outer())\n",
        "reserved",
    );
}

#[test]
fn inferred_method_return() {
    // The un-annotated method infers from `return self.v` (int) and `return "x"` (str); the two
    // branches CONFLICT under the multi-branch JOIN. The conflict proves inference ran on the body.
    rejects(
        "struct Box:\n    v: int\n    fn get(self):\n        if true:\n            return self.v\n        return \"x\"\n",
        "conflicting branches (int vs str)",
    );
}

// SOUNDNESS: an inferred (un-annotated) struct method return must FLOW to call sites through the
// build_graph/check_graph path (module-prefixed keys), not just the single-module bare-key path.
// Pre-fix the single-module `inferred_method_return` above passes while the CLI/entry path silently
// accepts `int` assigned to `str` (the bare-key vs module-key divergence).
#[test]
fn inferred_struct_method_return_flows_to_callsite() {
    entry_rejects(
        "struct P:\n    x: int\n    fn val(self):\n        return 5\nfn main():\n    s: str = P(3).val()\n    print(s)\nmain()\n",
        "cannot assign int to variable of type str",
    );
}

#[test]
fn inferred_struct_method_return_correct_site_ok() {
    // BOUNDARY: an inferred method return used at a correctly-typed site must still compile.
    entry_ok(
        "struct P:\n    x: int\n    fn val(self):\n        return 5\nfn main():\n    n: int = P(3).val()\n    print(n)\nmain()\n",
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
        "struct P:\n    x: int\n    fn compare(self, o: P):\n        return self.x < o.x\nfn main():\n    a := P(1)\n    b := P(2)\n    c := a < b\n    print(c)\nmain()\n",
        "compare",
    );
}

#[test]
fn inferred_compare_generic_bound_rejected() {
    // A generic bound `[T: Comparable]` over a struct whose `compare` infers bool must reject at
    // check, not fault later.
    entry_rejects(
        "struct P:\n    x: int\n    fn compare(self, o: P):\n        return self.x < o.x\nfn cmp[T: Comparable](a: T, b: T) -> int:\n    return a.compare(b)\nfn main():\n    print(cmp(P(1), P(2)))\nmain()\n",
        "Comparable",
    );
}

#[test]
fn inferred_enum_method_return_flows_to_callsite() {
    // Enum methods have the same hole: an un-annotated `fn val(self): return 5` returns int, which
    // must not be silently assignable to a str slot.
    entry_rejects(
        "enum Color:\n    Red\n    Blue\n    fn val(self):\n        return 5\nfn main():\n    s: str = Color.Red.val()\n    print(s)\nmain()\n",
        "cannot assign int to variable of type str",
    );
}

#[test]
fn inferred_enum_method_return_correct_site_ok() {
    entry_ok(
        "enum Color:\n    Red\n    Blue\n    fn val(self):\n        return 5\nfn main():\n    n: int = Color.Red.val()\n    print(n)\nmain()\n",
    );
}

#[test]
fn recursive_inferred_struct_method_no_spurious_error() {
    // BOUNDARY: a recursive un-annotated method must still infer `int` via the fixpoint and not
    // start spuriously erroring.
    entry_ok(
        "struct P:\n    x: int\n    fn f(self, c: bool):\n        if c:\n            return self.f(false)\n        return 0\nfn main():\n    n: int = P(1).f(true)\n    print(n)\nmain()\n",
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
        "fn f(x: int?) -> int!:\n    match x:\n        Some(v): return Ok(v)\n        None: return Err(\"none\")\n",
    );
}

#[test]
fn optional_shorthand_accepts_some_and_none() {
    ok("x: int? = Some(1)\ny: int? = None\n");
}

#[test]
fn optional_shorthand_rejects_bare_value() {
    rejects(
        "x: int? = 5\n",
        "cannot assign int to variable of type Option[int]",
    );
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
    rejects(
        "s := Some(5)\nx := match s:\n    Some(v): v\n",
        "non-exhaustive",
    );
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
fn if_expression_int_float_const_mix_widens() {
    // QoL + consistency with list literals (`[1, 2.5]`): an untyped int-CONSTANT branch widens to
    // float when a float-CONSTANT sibling branch is present — the same `literal_numeric_mix` peephole
    // the list/map literals use. The compiler emits `Op::CoerceFloat` on the int branch under the
    // identical predicate, so this is sound (no `Int` under a static `float`).
    ok("x := if true: 1 else: 2.5\ny := x + 0.5\n");
    ok("x := if false: 2.5 else: 1\ny := x + 0.5\n");
    // elif chain: a float const anywhere licenses widening the int-const arms, ORDER-INDEPENDENTLY
    // (the whole-chain mix is threaded through the recursion — `infer_if_else_chain`). Both a float in
    // the tail/else AND a float in the head (before an all-int suffix) must widen, matching `[.., ..]`.
    ok("x := if false: 1 elif false: 2 else: 3.5\ny := x + 0.5\n");
    ok("x := if false: 2.5 elif false: 1 else: 2\ny := x + 0.5\n");
    ok("x := if false: 1 elif false: 2.5 else: 3\ny := x + 0.5\n");
    // A separate if-expr AFTER a mixed chain must NOT inherit the chain's mix (no leak): both-int
    // stays int.
    ok("x := if false: 1 elif false: 2 else: 3.5\nz := if true: 4 else: 5\nw := z + 1\n");
    // both branches int (no float sibling) is UNCHANGED — stays `int`.
    ok("x := if true: 1 else: 2\ny := x + 1\n");
}

#[test]
fn match_expression_int_float_const_mix_widens() {
    ok("x := match true:\n    true: 1\n    _: 2.5\ny := x + 0.5\n");
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
        "s := Some(5)\nx := match s:\n    Some(v): v\n    Some(w): w\n    None: 0\n",
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

// SOUNDNESS: `?` in a nil-returning fn silently swallows the propagated Err/None (check-OK-then-
// data-loss). A named nil fn (including `main`) must REJECT — no `fn main` exception; a fn must
// return Result/Option to use `?`. (2026-07-18 bug-hunt.)
#[test]
fn try_in_named_nil_fn_rejected() {
    let src = "fn g() -> Result[int]:\n    return Ok(1)\n\
               fn f():\n    x := g()?\n    print(x)\n";
    rejects(
        src,
        "'?' used in a function that returns nil, not Result or Option",
    );
}

#[test]
fn try_in_named_nil_main_rejected() {
    // No `fn main` exception — main is just a nil fn here.
    let src = "fn g() -> Result[int]:\n    return Ok(1)\n\
               fn main():\n    x := g()?\n    print(x)\n";
    rejects(
        src,
        "'?' used in a function that returns nil, not Result or Option",
    );
}

// A NESTED nil fn (nil `inner` inside a Result-returning `outer`) must also reject — the flag rides
// the fn-body boundary, not just the top-level named fn.
#[test]
fn try_in_nested_nil_fn_rejected() {
    let src = "fn helper() -> Result[int]:\n    return Ok(1)\n\
               fn outer() -> Result[int]:\n    fn inner():\n        x := helper()?\n        print(x)\n    inner()\n    return Ok(0)\n";
    rejects(
        src,
        "'?' used in a function that returns nil, not Result or Option",
    );
}

// GUARD: `?` at MODULE TOP-LEVEL (outside any fn) stays valid — the runtime unwinds the Err at the
// program boundary. Must NOT regress (the flag must be false at module scope).
#[test]
fn try_at_module_top_level_still_accepted() {
    let src = "fn g() -> Result[int]:\n    return Ok(1)\n\
               x := g()?\nprint(x)\n";
    ok(src);
}

// GUARD: an Option-`?` at module top-level stays valid too.
#[test]
fn try_option_at_module_top_level_still_accepted() {
    let src = "fn h() -> int?:\n    return Some(1)\n\
               x := h()?\nprint(x)\n";
    ok(src);
}

// SOUNDNESS: `?` must match the enclosing function's sum-type KIND (Result vs Option). A Result-`?`
// inside an Option-returning fn (or vice versa) would make the fn return the wrong sum-type and fault
// a downstream exhaustive `match`/`??` at runtime even though `check` passed.
#[test]
fn try_result_in_option_fn_rejected() {
    let src = "fn pr() -> int!:\n    return Err(\"bad\")\n\
               fn f() -> int?:\n    x := pr()?\n    return Some(x)\n\
               fn main():\n    match f():\n        Some(v): print(\"some {v}\")\n        None: print(\"none\")\n\
               main()\n";
    entry_rejects(src, "returns Option, not Result");
}

#[test]
fn try_option_in_result_fn_rejected() {
    let src = "fn find() -> int?:\n    return None\n\
               fn f() -> int!:\n    x := find()?\n    return Ok(x)\n\
               fn main():\n    match f():\n        Ok(v): print(v)\n        Err(e): print(e.message())\n\
               main()\n";
    entry_rejects(src, "returns Result, not Option");
}

#[test]
fn try_result_in_result_compatible_ok() {
    let src = "fn g() -> int!:\n    return Ok(1)\n\
               fn f() -> int!:\n    x := g()?\n    return Ok(x + 1)\n";
    entry_ok(src);
}

#[test]
fn try_option_in_option_ok() {
    let src = "fn h() -> int?:\n    return Some(1)\n\
               fn f() -> int?:\n    x := h()?\n    return Some(x)\n";
    entry_ok(src);
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
    ok(
        "fn f(b: int) -> Result[int]:\n    if b == 0:\n        return Err(\"bad\")\n    return Ok(b)\n",
    );
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
fn dup_newtype_method_is_reported() {
    rejects(
        "newtype N = int:\n    fn f(self) -> int: return 1\n    fn f(self) -> int: return 2\n",
        "method 'f' is already defined",
    );
}

#[test]
fn newtype_static_method_is_rejected_with_clear_message() {
    // Static (associated) methods on a newtype are a deferred v1 limit; reject with a clear
    // message at the decl site instead of a cryptic 'unknown name' at the call site.
    let errs = check_src(
        "newtype Meters = float:\n    fn zero() -> Meters: return Meters(0.0)\nm := Meters.zero()\n",
    );
    assert!(
        errs.iter().any(|e| {
            e.message.contains("static")
                && e.message.contains("newtype")
                && e.message.contains("not supported")
        }),
        "expected a clear not-supported message, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("unknown name")),
        "should not surface the cryptic 'unknown name' error, got: {errs:?}"
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
    // REGRESSION (same keying class as the name_is_generic struct/newtype fix): a bare GENERIC-enum
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
        "variant 'Light.Red' cannot match a value of enum 'Color'",
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
    rejects("xs := [3, 1, 2]\nn := xs.sort() + 1\n", "no value (nil)");
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
    ok("s := \"5\"\nr: Result[int, str] = s.parse_int()\nprint(r)\n");
}

#[test]
fn parse_float_infers_result_float_str() {
    ok("s := \"5.0\"\nr: Result[float, str] = s.parse_float()\nprint(r)\n");
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
    rejects("r := \",\".join([1, 2])\n", "argument 1 of 'join'");
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
    ok("s := \"abc\"\nr: str = s.reverse()\nprint(r)\n");
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
        "Option[int] to variable of type int",
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
    ok("xs := [1, 2, 3]\nm := xs.pop()\nmatch m:\n    Some(v): print(v)\n    None: print(0)\n");
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
        "cannot apply += to List[int] and List[str]",
    );
}

#[test]
fn set_pipe_eq_mismatched_element_rejected() {
    rejects(
        "a: Set[int] = {1}\nb: Set[str] = {\"a\"}\na |= b\n",
        "bitwise operator |= requires int operands or two sets",
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
        "argument 1 of 'concat'",
    );
}

#[test]
fn list_extend_element_type_checked() {
    rejects(
        "xs := [1, 2]\nxs.extend([\"a\"])\n",
        "argument 1 of 'extend'",
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
    // (Add + a zero/identity for the empty list) and both runtimes are numeric-only. A user struct
    // with a structural `add(self, o) -> Self` SATISFIES the `Add` protocol, so `where T: Add` alone
    // would wrongly admit it — the residual numeric check-time gate must STILL reject it (never
    // check-ok/run-error). This pins that the gate survives alongside the `where T: Add` annotation.
    let src = "\
struct M:
    n: int
    fn add(self, o: M) -> M:
        return M(self.n + o.n)
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

#[test]
fn user_enum_named_result_rejected() {
    rejects(
        "enum Result:\n    A\n",
        "type 'Result' is reserved (builtin)",
    );
}

#[test]
fn user_struct_named_option_rejected() {
    rejects(
        "struct Option:\n    x: int\n",
        "type 'Option' is reserved (builtin)",
    );
}

#[test]
fn user_decl_named_tuple_rejected() {
    // `tuple` is a reserved global (structural tuple type). No decl form may shadow it — matching its
    // container siblings (`struct List`/`range`/…). Covers all five decl keywords in one place.
    rejects(
        "struct tuple:\n    x: int\n",
        "type 'tuple' is reserved (builtin)",
    );
    rejects("enum tuple:\n    A\n", "type 'tuple' is reserved (builtin)");
    rejects(
        "newtype tuple = int\n",
        "type 'tuple' is reserved (builtin)",
    );
    rejects("type tuple = int\n", "type 'tuple' is reserved (builtin)");
    rejects(
        "protocol tuple:\n    fn f(self)\n",
        "type 'tuple' is reserved (builtin)",
    );
    // Regression: reserving the NAME must not touch tuple LITERALS (they never route through the
    // decl-name guard) — a real tuple value + destructure still type-checks.
    ok("p := (1, 2)\na, b := p\nprint(a + b)\n");
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

/// Bug 4B — a module bind (aliased OR the un-aliased last path segment) whose name is a reserved
/// builtin/ctor is REJECTED. That is what makes shadowing the builtin impossible.
#[test]
fn reserved_module_bind_rejected() {
    let int_mod = (
        "lib/int.chz",
        "fn twice(n: int) -> int:\n    return n * 2\n",
    );
    let geo = ("lib/geo.chz", "struct Point:\n    x: int\n    y: int\n");
    // un-aliased last path segment == a reserved builtin callable
    files_reject(
        &[int_mod, ("main.chz", "import lib.int\nprint(1)\n")],
        "reserved (builtin)",
    );
    files_reject(&[int_mod, ("main.chz", "import lib.int\nprint(1)\n")], "as");
    // aliased to a builtin VARIANT ctor
    files_reject(
        &[geo, ("main.chz", "import lib.geo as Ok\nprint(1)\n")],
        "import alias 'Ok' is reserved (builtin)",
    );
    // un-aliased last path segment == a builtin variant ctor
    files_reject(
        &[
            ("lib/Ok.chz", "fn f() -> int:\n    return 1\n"),
            ("main.chz", "import lib.Ok\nprint(1)\n"),
        ],
        "reserved (builtin)",
    );
}

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
            "import lib.geo\np := geo.Point(1, 2)\nr: Result[int, str] = Ok(5)\nprint(p.x)\n",
        ),
    ]);
}

/// Bug 4B, third door: a FROM-import binds into the same VALUE namespace, so a reserved bound name —
/// the ALIAS *or* the bare MEMBER name — is rejected there too. Otherwise `import str from lib.sh`
/// (a module global named `str`) destroys the `str()` ctor in expression position exactly like
/// `import std.str` did. Only the value/function binds are gated; the TYPE members that license a
/// reserved builtin (`import Shared from std.concurrency`) are untouched.
#[test]
fn reserved_from_import_member_rejected() {
    let sh = ("lib/sh.chz", "str := 5\nOk := 7\nint := 9\n");
    for member in ["str", "Ok", "int"] {
        files_reject(
            &[
                sh,
                (
                    "main.chz",
                    &format!("import {member} from lib.sh\nprint(1)\n"),
                ),
            ],
            "reserved (builtin)",
        );
    }
    // a FUNCTION member with a reserved-variant name (`fn Ok` is legal at its decl site) is the same
    // hazard — today the builtin ctor silently wins and the import is dead code.
    files_reject(
        &[
            ("lib/fo.chz", "fn Ok(x: int) -> int:\n    return x\n"),
            ("main.chz", "import Ok from lib.fo\nprint(Ok(5))\n"),
        ],
        "reserved (builtin)",
    );
    // escape hatch + no over-rejection: alias it, and the builtins it would have shadowed still work.
    files_ok(&[
        sh,
        (
            "main.chz",
            "import str as s, Ok as k from lib.sh\nprint(s + k)\nprint(str(5))\nr: Result[int, str] = Ok(1)\nprint(r)\n",
        ),
    ]);
    // the reserved TYPE members that LICENSE a builtin still import un-aliased.
    files_ok(&[(
        "main.chz",
        "import Shared from std.concurrency\ns := Shared(1)\nprint(s.get())\n",
    )]);
    files_ok(&[(
        "main.chz",
        "import timer from std.time\nt := timer(1)\nprint(1)\n",
    )]);
}

/// Wave-5 residual 4, DIAGNOSTIC-ONLY: `is_reserved_module_bind` gates the 34 reserved names, so a
/// module bind colliding with a same-named USER `struct`/`enum` ctor is NOT rejected at the import —
/// the module (a VALUE-namespace bind) simply wins in expression position and the ctor call is a hard
/// type error. That is the Python-normal outcome and the alias is the cure, so this stays a diagnostic
/// (not a resolver-level module namespace): NAME the collision instead of the bare, mystifying
/// `module Point is not callable`.
#[test]
fn module_bind_shadowing_user_type_names_the_collision() {
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
        "shadows the same-named type 'Point'",
    );
    // … and an enum ctor: same VALUE-namespace collision, same diagnostic.
    files_reject(
        &[
            shape,
            (
                "main.chz",
                "import lib.Shape\n\nenum Shape:\n    Dot\n\ns := Shape.Dot\nprint(Shape(1))\n",
            ),
        ],
        "shadows the same-named type 'Shape'",
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
    // …and RE-DECLARING the name at MODULE scope (`:=`, which `declare` sanctions as a fresh, mutable
    // binding) hands the name back to the module: the import bind is gone, so assigning is legal
    // again. The gate must key on the CURRENT binding, not on the name having ever been imported.
    files_ok(&[
        st,
        (
            "main.chz",
            "import COUNT from lib.st\nCOUNT := COUNT + 1\nCOUNT = COUNT + 1\nprint(COUNT)\n",
        ),
    ]);
}

/// `nil` is a value-builtin resolved through the scope stack, so a module bound to it wins in
/// EXPRESSION position and the `nil` literal silently becomes a module — the exact Bug-4 failure
/// mode. It is a rejected module bind (un-aliased AND aliased), even though it stays a legal
/// from-import ALIAS target (`import x as nil` binds a value, and a value still works as a value).
#[test]
fn module_bind_named_nil_rejected() {
    let m = ("lib/nil.chz", "fn f() -> int:\n    return 1\n");
    files_reject(
        &[m, ("main.chz", "import lib.nil\nx := nil\nprint(x)\n")],
        "reserved (builtin)",
    );
    files_reject(
        &[
            ("lib/geo.chz", "struct Point:\n    x: int\n"),
            ("main.chz", "import lib.geo as nil\nprint(1)\n"),
        ],
        "reserved (builtin)",
    );
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

fn entry_rejects(src: &str, needle: &str) {
    let errs = check_entry(src);
    assert!(
        errs.iter().any(|e| e.message.contains(needle)),
        "expected an error containing {needle:?}, got: {errs:?}"
    );
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

/// Same bug, NEWTYPE method: a named-imported factory returning a newtype must resolve its method.
#[test]
fn checker_named_fn_import_resolves_newtype_method() {
    let t = TmpDir::new();
    t.write(
        "lib.chz",
        "newtype Meters = int:\n    fn doubled(self) -> int:\n        return int(self) * 2\nfn mk() -> Meters:\n    return Meters(21)\n",
    );
    let entry = t.write(
        "main.chz",
        "import mk from lib\nfn main():\n    print(mk().doubled())\n",
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
/// the runtime arm lives in vm parity_tests). Pre-fix: "type Token has no method 'cancelled'".
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

/// Same, NEWTYPE: a named-fn-imported newtype value whose own method satisfies the protocol is
/// accepted at a protocol bound.
#[test]
fn named_fn_import_satisfies_protocol_newtype() {
    let t = TmpDir::new();
    t.write(
        "lib.chz",
        "newtype Meters = int:\n    fn describe(self) -> str:\n        return \"M{int(self)}\"\nfn mk() -> Meters:\n    return Meters(5)\n",
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
fn native_math_float_param_accepts_int() {
    // One-way int->float widening: `math.sqrt(16)` widens the int arg to a float (the native host's
    // `arg_float` already runtime-promotes int, so this is hole-free — resolves the old
    // "inconsistent" gap where the runtime promoted but the checker rejected).
    entry_ok("import std.math\nfn main():\n    print(math.sqrt(16))\n");
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
fn cmp_max_int_result_widens_into_float_let() {
    // BREAKING (was accepted): `cmp.max(3, 5)` instantiates `T = int` and returns a TYPED int — a fn
    // RESULT is typed even with constant args (Go). It no longer adapts to a `float` let; the fix is
    // `float(...)` (or `cmp.max(3.0, 5.0)`).
    entry_rejects(
        "import std.cmp\nfn main():\n    x: float = cmp.max(3, 5)\n    print(x)\n",
        "a typed int never widens to float — write float(x)",
    );
    entry_ok("import std.cmp\nfn main():\n    x: float = float(cmp.max(3, 5))\n    print(x)\n");
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
fn cmp_from_import_max_int_widens_into_float_let() {
    // BREAKING, same as `cmp_max_int_result_widens_into_float_let` through the `from`-import path.
    entry_rejects(
        "import max from std.cmp\nfn main():\n    x: float = max(3, 5)\n    print(x)\n",
        "a typed int never widens to float — write float(x)",
    );
}

#[test]
fn native_math_floor_widens_int_arg() {
    // `floor`/`ceil`/`sqrt` keep a float-only RESULT (not numeric-polymorphic like abs/min/max), but
    // one-way int->float widening now lets an int ARG flow into their float param (hole-free: the
    // native host promotes int). `floor(2)` is `floor(2.0)`.
    entry_ok("import std.math\nfn main():\n    print(math.floor(2))\n");
}

// ===== higher-order-function parameter types =====

#[test]
fn hof_param_type_ok() {
    ok(
        "fn apply(f: fn(int) -> int, v: int) -> int:\n    return f(v)\ninc := fn(x: int) -> int: x + 1\nn := apply(inc, 4)\n",
    );
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
    // map int -> bool produces List[bool]; indexing yields a bool.
    ok("xs := [1,2]\nys := xs.map(fn(x: int) -> bool: x > 0)\nb := ys[0]\n");
}

#[test]
fn list_filter_predicate_must_return_bool() {
    // Uniform general-path diagnostic (file-backed `filter(self, p: fn(T) -> bool)`): the retired
    // bespoke "predicate" wording is replaced by the standard argument-mismatch message.
    rejects(
        "xs := [1,2,3]\nys := xs.filter(fn(x: int) -> int: x)\n",
        "argument 1 of 'filter': expected fn(int) -> bool, found fn(int) -> int",
    );
}

#[test]
fn list_map_function_param_must_match_element() {
    rejects("xs := [1,2,3]\nys := xs.map(fn(x: str) -> int: 0)\n", "map");
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
    ok(
        "o: (int, int)? = Some((1, 2))\nmatch o:\n    None: print(\"n\")\n    Some((a, b)): print(a + b)\n",
    );
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
    rejects(
        "t := (1, 2)\nmatch t:\n    (1, n): print(n)\n",
        "non-exhaustive",
    );
}

#[test]
fn match_tuple_wrong_arity_rejected() {
    rejects(
        "t := (1, 2)\nmatch t:\n    (a, b, c): print(a)\n",
        "element",
    );
}

#[test]
fn match_nested_tuple_element_type_mismatch_rejected() {
    rejects("t := (\"x\", 2)\nmatch t:\n    (s, n): m: int = s\n", "");
}

#[test]
fn match_nested_nullary_variant_ok() {
    // `Cons(h, Nil)`: a nested nullary variant is now a refutable variant match (the checker
    // promotes the bare `Nil`). Previously rejected (gap #15 limit); now supported.
    let src = "enum L:\n    Nil\n    Cons(int, L)\n\
               fn f(x: L):\n    match x:\n        L.Cons(h, L.Nil): print(h)\n        _: print(\"e\")\n";
    ok(src);
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
    // A list of plain ints (not tuples) still can't bind two names.
    rejects(
        "xs := [1,2,3]\nfor a, b in xs:\n    print(a)\n",
        "requires a map",
    );
}

#[test]
fn for_tuple_list_binds_each_element() {
    ok(
        "xs := [(1, \"a\"), (2, \"b\")]\nfor n, s in xs:\n    i: int = n\n    t: str = s\n    print(\"{i}{t}\")\n",
    );
}

#[test]
fn for_tuple_list_one_var_binds_whole_tuple() {
    ok(
        "xs := [(1, \"a\")]\nfor p in xs:\n    i: int = p.0\n    s: str = p.1\n    print(\"{i}{s}\")\n",
    );
}

#[test]
fn for_tuple_list_three_names_over_triple() {
    ok("xs := [(1, 2, 3)]\nfor a, b, c in xs:\n    print(a + b + c)\n");
}

#[test]
fn for_tuple_arity_mismatch_rejected() {
    rejects("xs := [(1, \"a\")]\nfor a, b, c in xs:\n    print(a)\n", "");
}

#[test]
fn for_tuple_element_types_checked() {
    // `s` is the str element; assigning it to an int slot must fail.
    rejects(
        "xs := [(1, \"a\")]\nfor n, s in xs:\n    bad: int = s\n",
        "",
    );
}

#[test]
fn for_kv_over_range_rejected() {
    rejects("for a, b in 0..3:\n    print(a)\n", "range");
}

#[test]
fn for_over_int_still_rejected() {
    rejects("for x in 5:\n    print(x)\n", "cannot iterate over int");
}

// ===== loop variable is immutable (gap: cross-engine divergence on reassignment) =====

#[test]
fn for_range_var_reassign_rejected() {
    rejects("for i in 0..3:\n    i = i + 100\n", "loop variable");
}

#[test]
fn for_range_var_compound_assign_rejected() {
    rejects("for i in 0..3:\n    i += 1\n", "loop variable");
}

#[test]
fn for_list_var_reassign_rejected() {
    rejects("for x in [1, 2, 3]:\n    x = 0\n", "loop variable");
}

#[test]
fn for_map_key_reassign_rejected() {
    rejects(
        "m := {\"a\": 1}\nfor k, v in m:\n    k = \"z\"\n",
        "loop variable",
    );
}

#[test]
fn for_map_value_reassign_rejected() {
    rejects(
        "m := {\"a\": 1}\nfor k, v in m:\n    v = 9\n",
        "loop variable",
    );
}

#[test]
fn for_body_local_var_still_assignable() {
    // Only the loop variable is frozen — locals declared in the body remain mutable.
    ok("for i in 0..3:\n    x := i * 2\n    x = x + 1\n    print(x)\n");
}

#[test]
fn loop_var_shadowed_in_inner_block_assignable() {
    // A name re-bound by `:=` in an inner scope is a fresh local, not the loop var → assignable.
    ok("for i in 0..3:\n    if i >= 0:\n        i := 99\n        i = i + 1\n        print(i)\n");
}

#[test]
fn for_set_var_reassign_rejected() {
    rejects("s := {1, 2, 3}\nfor x in s:\n    x = 0\n", "loop variable");
}

#[test]
fn for_str_var_reassign_rejected() {
    rejects("for c in \"ab\":\n    c = \"z\"\n", "loop variable");
}

#[test]
fn nested_loop_same_name_inner_reassign_rejected() {
    // `is_loop_var` resolves to the inner loop's binding — still a loop var → rejected.
    rejects(
        "for i in 0..2:\n    for i in 0..2:\n        i = 9\n",
        "loop variable",
    );
}

#[test]
fn reassign_after_loop_is_undeclared_not_loop_var() {
    // The loop var doesn't leak past the loop; assigning it afterward is plain-undeclared.
    let errs = check_src("for i in 0..3:\n    print(i)\ni = 5\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("undeclared variable")),
        "expected an 'undeclared variable' error, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("loop variable")),
        "loop-var mark must not leak past the loop, got: {errs:?}"
    );
}

#[test]
fn outer_local_shadowed_by_later_loop_var_stays_mutable() {
    // A pre-existing local reused as a later loop's var name stays assignable after the loop.
    ok("i := 5\ni = 7\nfor i in 0..3:\n    print(i)\ni = 9\nprint(i)\n");
}

// ===== const bindings (L4: immutable binding modifier) =====

#[test]
fn const_global_reassign_rejected() {
    rejects("PI: const float = 3.14\nPI = 3.0\n", "const");
}

#[test]
fn const_global_compound_assign_rejected() {
    rejects("N: const int = 10\nN += 1\n", "const");
}

#[test]
fn const_local_reassign_rejected() {
    rejects("fn f():\n    x: const int = 1\n    x = 2\n", "const");
}

#[test]
fn const_runtime_init_ok() {
    // `const` is a binding modifier, not a compile-time constant — a runtime RHS is fine.
    ok("fn compute() -> int:\n    return 5\nX: const int = compute()\nprint(X)\n");
}

#[test]
fn const_shallow_list_mutation_ok() {
    // Shallow: the NAME is frozen, but the object it points at is still mutable.
    ok("xs: const List[int] = [1, 2]\nxs.push(3)\nxs[0] = 9\nprint(xs)\n");
}

#[test]
fn const_captured_in_closure_reassign_rejected() {
    // A nested fn that reassigns a captured const must be rejected inside the closure body.
    rejects(
        "fn outer():\n    C: const int = 1\n    fn inner():\n        C = 2\n    inner()\n",
        "const",
    );
}

#[test]
fn const_same_scope_walrus_redeclare_rejected() {
    // `:=` must NOT be able to un-const a live const in the same scope (for a module global it is
    // the SAME storage slot, so this would silently defeat the guarantee, not shadow it).
    rejects("X: const int = 1\nX := 2\nX = 3\n", "const");
}

#[test]
fn const_same_scope_typed_redeclare_rejected() {
    rejects("X: const int = 1\nX: int = 2\n", "const");
}

#[test]
fn const_shadow_in_inner_scope_ok() {
    // A genuine inner-scope shadow (a fresh local named like an outer const) is still fine — the
    // outer const is untouched; only same-scope re-declaration is the escape we reject.
    ok("PI: const float = 3.14\nfn f():\n    PI := 9.0\n    PI = 10.0\n    print(PI)\n");
}

#[test]
fn const_read_and_alias_ok() {
    // Reading a const, and binding a fresh (mutable) name to its value, are both fine.
    ok("K: const int = 7\ny := K + 1\ny = 100\nprint(K)\nprint(y)\n");
}

#[test]
fn const_from_imported_rebind_names_const() {
    // A from-imported const, rebound, gets a const-specific message (not the snapshot-mutator one
    // whose "call a mutator fn" advice is wrong for an immutable value). `math.pi` is a native const.
    let errs = check_entry("import pi from std.math\npi = 3.0\n");
    assert!(
        errs.iter().any(|e| e.message.contains("const")),
        "expected a const-specific message, got: {errs:?}"
    );
}

#[test]
fn const_qualified_module_write_names_const() {
    let errs = check_entry("import std.math\nmath.pi = 3.0\n");
    assert!(
        errs.iter().any(|e| e.message.contains("const")),
        "expected a const-specific message, got: {errs:?}"
    );
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
    rejects(
        "for i in 0..3:\n    print(i)\nbreak\n",
        "break outside loop",
    );
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
//
// `spawn:` / `defer:` blocks ARE the reachable case of the same rule: each compiles to a fresh
// child proto with an empty loop stack, so a `break`/`continue` lexically nested in an enclosing
// loop but placed inside the block is illegal at runtime in both engines. The checker mirrors the
// fn/closure save-zero-restore of `loop_depth` across these block arms so the `break/continue
// outside loop` guard fires at check time (was a three-way divergence: `check` passed, the VM
// raised at runtime, the interp silently treated it as a block exit). A legitimate loop INSIDE the
// block re-increments `loop_depth` from 0, so its own break/continue stay legal.

#[test]
fn break_in_defer_block_in_loop_rejected() {
    rejects(
        "fn w():\n    for i in 0..3:\n        defer:\n            break\n",
        "break outside loop",
    );
}

#[test]
fn continue_in_defer_block_in_loop_rejected() {
    rejects(
        "fn w():\n    for i in 0..3:\n        defer:\n            continue\n",
        "continue outside loop",
    );
}

#[test]
fn break_in_spawn_block_in_loop_rejected() {
    rejects(
        "fn w():\n    for i in 0..3:\n        spawn:\n            break\n",
        "break outside loop",
    );
}

#[test]
fn continue_in_spawn_block_in_loop_rejected() {
    rejects(
        "fn w():\n    for i in 0..3:\n        spawn:\n            continue\n",
        "continue outside loop",
    );
}

#[test]
fn break_in_loop_inside_defer_block_ok() {
    // A loop INSIDE the defer block re-opens a loop context; its own break is legal.
    ok("fn w():\n    defer:\n        for j in 0..3:\n            break\n");
}

#[test]
fn break_continue_in_loop_inside_spawn_block_ok() {
    // A loop INSIDE the spawn block re-opens a loop context; its own break/continue are legal.
    ok(
        "fn main():\n    parallel:\n        spawn:\n            for j in 0..3:\n                if j == 1: break\n                continue\nmain()\n",
    );
}

#[test]
fn spawn_call_form_unaffected_ok() {
    // The Call-form spawn evaluates in the outer scope and holds no statement block; it is not gated.
    ok("fn t():\n    print(1)\nfn main():\n    for i in 0..3:\n        spawn t()\nmain()\n");
}

// ===== map / dictionary (gap #5) =====

#[test]
fn map_literal_infers_str_int() {
    // A `Map[str, int]` annotation must accept a `{"a": 1}` literal.
    ok("m: Map[str, int] = {\"a\": 1, \"b\": 2}\n");
}

#[test]
fn empty_map_assignable_to_any_map() {
    ok("m: Map[str, int] = {}\n");
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
    rejects("m := {1.0: 2}\n", "must implement Hashable");
}

#[test]
fn float_map_key_annotation_rejected() {
    rejects("m: Map[float, int] = {}\n", "must implement Hashable");
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
    ok("m: Map[int, str] = {1: \"a\"}\n");
    ok("m: Map[bool, int] = {true: 1}\n");
}

#[test]
fn map_keys_method_is_list_of_key() {
    ok("m := {\"a\": 1}\nks: List[str] = m.keys()\n");
}

#[test]
fn map_values_method_is_list_of_value() {
    ok("m := {\"a\": 1}\nvs: List[int] = m.values()\n");
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

#[test]
fn map_merge_returns_map() {
    ok("a := {\"x\": 1}\nb := {\"y\": 2}\nc := a.merge(b)\nv: int = c[\"x\"]\nprint(v)\n");
}

#[test]
fn map_update_returns_nil() {
    ok("a := {\"x\": 1}\nb := {\"y\": 2}\na.update(b)\nprint(a.len())\n");
}

#[test]
fn map_merge_value_type_checked() {
    rejects(
        "a := {\"x\": 1}\nb := {\"y\": \"s\"}\nc := a.merge(b)\n",
        "argument 1 of 'merge'",
    );
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
    ok(
        "fn pair() -> (int, str):\n    return (1, \"x\")\nfn main():\n    a, b := pair()\n    c := a + 1\n    d := b + \"!\"\n",
    );
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
    rejects(
        "fn main():\n    a, b := 5\n",
        "cannot destructure non-tuple",
    );
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
    entry_ok(
        "import std.process\nfn main():\n    match process.cmd(\"echo hi\"):\n        Ok(s): print(s)\n        Err(e): print(e)\n",
    );
}

#[test]
fn native_process_cmd_arg_must_be_str() {
    entry_rejects(
        "import std.process\nfn main():\n    print(process.cmd(5))\n",
        "argument 1 of 'cmd'",
    );
}

#[test]
fn native_process_run_returns_result_proc_result() {
    entry_ok(
        "import std.process\nfn main():\n    match process.run(\"echo hi\"):\n        Ok(r):\n            o: str = r.stdout\n            e: str = r.stderr\n            c: int = r.code\n            print(o + e + str(c))\n        Err(msg): print(msg)\n",
    );
}

#[test]
fn native_process_run_args_takes_prog_and_list_str() {
    entry_ok(
        "import std.process\nfn main():\n    match process.run_args(\"echo\", [\"a\", \"b\"]):\n        Ok(r): print(r.stdout + str(r.code))\n        Err(msg): print(msg)\n",
    );
}

#[test]
fn native_process_run_args_argv_must_be_list_str() {
    entry_rejects(
        "import std.process\nfn main():\n    print(process.run_args(\"echo\", \"notalist\"))\n",
        "argument 2 of 'run_args'",
    );
}

#[test]
fn list_comprehension_infers_element_type() {
    // `[x * 2 for x in [1, 2, 3]]` is a `List[int]` — the loop var binds to the list's element.
    ok("xs: List[int] = [x * 2 for x in [1, 2, 3]]\n");
}

#[test]
fn list_comprehension_wrong_element_type_rejected() {
    rejects("xs: List[str] = [x * 2 for x in [1, 2, 3]]\n", "List[int]");
}

#[test]
fn comprehension_guard_must_be_bool() {
    rejects(
        "xs := [x for x in [1, 2, 3] if x]\n",
        "comprehension guard must be bool",
    );
}

#[test]
fn list_comprehension_over_range_is_list_int() {
    ok("xs: List[int] = [x * x for x in 0..10]\n");
}

#[test]
fn set_comprehension_infers_element_type() {
    ok("s: Set[int] = {x for x in [1, 2, 3]}\n");
}

#[test]
fn map_comprehension_over_map_entries() {
    ok("src: Map[str, int] = {\"a\": 1}\nm: Map[str, int] = {k: v for k, v in src}\n");
}

#[test]
fn comprehension_var_out_of_scope_after() {
    // The loop variable is scoped to the comprehension; referencing it afterward is unknown.
    rejects("xs := [x for x in [1, 2, 3]]\nprint(x)\n", "x");
}

#[test]
fn nested_comp_later_clause_sees_earlier_var_typechecks() {
    // The second clause's iterable references the first clause's binding (a list-of-lists flatten).
    ok("ys: List[int] = [y for xs in [[1, 2], [3]] for y in xs]\n");
}

#[test]
fn nested_comp_two_clause_element_type() {
    ok("ps: List[int] = [x + y for x in [1, 2] for y in [10, 20]]\n");
}

#[test]
fn nested_comp_unbound_in_later_clause_errors() {
    // `zzz` is bound nowhere; a later clause's iterable must still resolve names normally.
    rejects(
        "ys := [y for x in [1, 2] for y in zzz]\n",
        "unknown name 'zzz'",
    );
}

#[test]
fn nested_comp_guard_after_nonfinal_clause_typechecks() {
    ok("ps: List[int] = [x * y for x in [1, 2, 3] if x > 0 for y in [10, 20]]\n");
}

#[test]
fn nested_comp_channel_in_later_clause_rejected() {
    // The channel-drain rejection must run per clause, not just the first.
    entry_rejects(
        "fn main():\n    ch := Channel[int]()\n    xs := [c for x in [1, 2] for c in ch]\n",
        "a channel cannot be drained in a comprehension",
    );
}

#[test]
fn native_os_exit_takes_int() {
    entry_ok("import std.os\nfn main():\n    os.exit(1)\n");
}

#[test]
fn native_os_exit_arg_must_be_int() {
    entry_rejects(
        "import std.os\nfn main():\n    os.exit(\"nope\")\n",
        "argument 1 of 'exit'",
    );
}

#[test]
fn native_fs_predicates_are_bool_and_size_is_result_int() {
    entry_ok(
        "import std.fs\nfn main():\n    b: bool = fs.is_file(\"x\")\n    e: bool = fs.exists(\"x\")\n    match fs.size(\"x\"):\n        Ok(n): print(str(n))\n        Err(m): print(m)\n",
    );
}

#[test]
fn native_fs_list_dir_returns_result_list_str() {
    entry_ok(
        "import std.fs\nfn main():\n    match fs.list_dir(\".\"):\n        Ok(xs): print(\",\".join(xs))\n        Err(e): print(e)\n",
    );
}

#[test]
fn native_fs_mutations_typecheck_as_result_nil() {
    entry_ok(
        "import std.fs\nfn main():\n    match fs.mkdir(\"d\"):\n        Ok(_): print(\"made\")\n        Err(e): print(e)\n    match fs.append(\"f\", \"x\"):\n        Ok(_): print(\"app\")\n        Err(e): print(e)\n    match fs.rename(\"a\", \"b\"):\n        Ok(_): print(\"ren\")\n        Err(e): print(e)\n    match fs.copy(\"a\", \"b\"):\n        Ok(_): print(\"cp\")\n        Err(e): print(e)\n    match fs.remove_file(\"f\"):\n        Ok(_): print(\"rmf\")\n        Err(e): print(e)\n    match fs.remove_dir(\"d\"):\n        Ok(_): print(\"rmd\")\n        Err(e): print(e)\n",
    );
}

#[test]
fn native_fs_mutation_wrong_arity_rejected() {
    entry_rejects(
        "import std.fs\nfn main():\n    print(str(fs.mkdir(\"d\", \"extra\")))\n",
        "mkdir",
    );
}

#[test]
fn native_fs_unknown_member_rejected() {
    entry_rejects(
        "import std.fs\nfn main():\n    print(fs.touch(\"x\"))\n",
        "has no member 'touch'",
    );
}

// ===== M9: std.regex (Match struct) =====

#[test]
fn native_regex_is_match_returns_result_bool() {
    entry_ok(
        "import std.regex\nfn main():\n    match regex.is_match(\"x\", \"xy\"):\n        Ok(b):\n            if b:\n                print(\"yes\")\n        Err(e): print(e)\n",
    );
}

#[test]
fn native_regex_find_returns_match_with_typed_fields() {
    entry_ok(
        "import std.regex\nfn main():\n    match regex.find(\"[0-9]+\", \"a12\"):\n        Ok(opt):\n            match opt:\n                Some(m):\n                    t: str = m.text\n                    st: int = m.start\n                    g: List[str] = m.groups\n                    print(t + str(st) + \",\".join(g))\n                None: print(\"none\")\n        Err(e): print(e)\n",
    );
}

#[test]
fn native_regex_find_all_returns_result_list_match() {
    entry_ok(
        "import std.regex\nfn main():\n    match regex.find_all(\"[0-9]+\", \"1 2\"):\n        Ok(ms):\n            for m in ms:\n                print(m.text)\n        Err(e): print(e)\n",
    );
}

#[test]
fn native_regex_split_and_replace_all_return_strings() {
    entry_ok(
        "import std.regex\nfn main():\n    match regex.replace_all(\"a\", \"banana\", \"o\"):\n        Ok(s): print(s)\n        Err(e): print(e)\n    match regex.split(\",\", \"a,b\"):\n        Ok(xs): print(\"|\".join(xs))\n        Err(e): print(e)\n",
    );
}

#[test]
fn native_regex_match_unknown_field_rejected() {
    entry_rejects(
        "import std.regex\nfn main():\n    match regex.find(\"a\", \"a\"):\n        Ok(opt):\n            match opt:\n                Some(m): print(m.nope)\n                None: print(\"\")\n        Err(e): print(e)\n",
        "has no field 'nope'",
    );
}

#[test]
fn native_regex_unknown_member_rejected() {
    entry_rejects(
        "import std.regex\nfn main():\n    print(regex.compile(\"x\"))\n",
        "has no member 'compile'",
    );
}

// ===== Phase 4b: std.regex is now a FILE-BACKED `std/regex.chz` whose `native struct Match` + 5
// `native fn`s are declared in-module. Both the phase-4a companion stub (std/regex.stub.chz) and the
// `native_module_sig` regex arm are RETIRED — the checker harvests the module's whole SIGNATURE (type +
// fns) from the parsed in-module decls via `harvest_native_module`. Behavior is byte-identical. =====

/// Parse the real `std/regex.chz` (the SIGNATURE source for phase 4b).
#[cfg(test)]
fn parse_regex_chz() -> crate::ast::Module {
    let path = crate::resolver::std_root().join("regex.chz");
    let src = std::fs::read_to_string(&path).expect("std/regex.chz must exist");
    let toks = crate::lexer::tokenize(&src).expect("std/regex.chz must lex");
    crate::parser::parse(toks).expect("std/regex.chz must parse")
}

/// The effective ModuleSig of native module `std.<module>` produced by a full graph check that
/// `import std.<module>` — i.e. the sig the CLI actually uses (harvested-from-file for a file-backed
/// module + `attach_native_module_metadata`), NOT the raw `native_module_sig` table. `module` is the
/// bare submodule name, e.g. "math" / "io" / "regex".
#[cfg(test)]
fn native_module_sig_via_graph(module: &str) -> ModuleSig {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        &format!("import std.{module}\nfn main(): print(1)\n"),
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    let mut c = Checker::new();
    c.run_graph_pass(&graph, false);
    assert!(c.errors.is_empty(), "graph check errored: {:?}", c.errors);
    let id = graph
        .modules
        .iter()
        .find(|m| m.dotted == ["std", module])
        .unwrap_or_else(|| panic!("std.{module} in graph"))
        .id
        .clone();
    c.module_sigs
        .get(&id)
        .unwrap_or_else(|| panic!("std.{module} ModuleSig"))
        .clone()
}

/// The std.regex ModuleSig produced by a full graph check that `import std.regex`.
#[cfg(test)]
fn regex_module_sig_via_graph() -> ModuleSig {
    native_module_sig_via_graph("regex")
}

#[test]
fn harvest_native_method_own_type_params() {
    // A native method's own `[U]` lands on the harvested `FnSig.type_params`, and `U` resolves to
    // `Ty::Param("U")` in the method's params/ret (nested inside the struct's `[T]` scope).
    let src = "native struct Foo[T]:\n    native fn map[U](self, f: fn(T) -> U) -> List[U]\n";
    let module = parser::parse(lexer::tokenize(src).unwrap()).unwrap();
    let mut c = Checker::new();
    let info = c
        .harvest_native_struct_table(&module, "Foo")
        .expect("harvest Foo");
    let sig = info.methods.get("map").expect("Foo.map harvested");
    assert_eq!(sig.type_params.len(), 1, "method own [U] on the sig");
    assert_eq!(sig.type_params[0].name, "U");
    // `self` stripped → one param `fn(T) -> U`, with U as a `Ty::Param`.
    assert_eq!(sig.params.len(), 1);
    match &sig.params[0] {
        Ty::Func { params, ret, .. } => {
            assert_eq!(params, &vec![Ty::Param("T".to_string())]);
            assert_eq!(**ret, Ty::Param("U".to_string()));
        }
        other => panic!("expected fn param, got {other}"),
    }
    assert_eq!(sig.ret, Ty::list(Ty::Param("U".to_string())));
}

/// Phase 5a-containers — a `Checker` after a trivial graph check, with the always-linked prelude's
/// `List`/`Map`/`Set` method tables (harvested into `container_seeds`) re-seeded into `self.structs`.
/// Lets a test drive `native_handle_method("List"/"Map"/"Set", …)` exactly as the `Ty::List`/`Ty::Map`/
/// `Ty::Set` dispatch arms do.
#[cfg(test)]
fn prelude_container_checker() -> Checker {
    let t = TmpDir::new();
    let entry = t.write("main.chz", "fn main(): print(1)\n");
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    let mut c = Checker::new();
    c.run_graph_pass(&graph, false);
    assert!(c.errors.is_empty(), "graph check errored: {:?}", c.errors);
    assert!(
        c.container_seeds.contains_key("List")
            && c.container_seeds.contains_key("Map")
            && c.container_seeds.contains_key("Set")
            && c.container_seeds.contains_key("Channel")
            && c.container_seeds.contains_key("str")
            && c.container_seeds.contains_key("bytes")
            && c.container_seeds.contains_key("bytearray"),
        "prelude List/Map/Set/Channel/str/bytes/bytearray container seeds must be harvested"
    );
    // container_seeds is populated; re-seed it into self.structs for the method-table lookup.
    c.seed_stdlib_structs();
    c
}

/// PROVENANCE — the `"std.regex" =>` arm is DELETED from `native_module_sig`; the whole regex signature
/// (Match type + the 5 fns) now comes from parsing `std/regex.chz`. So `native_module_sig("std.regex")`
/// exports NOTHING (empty functions, no Match), while a full graph check that `import std.regex` still
/// resolves both the fns and `Match`. The sibling native modules (Response/ProcResult) stay hand-built.
#[test]
fn regex_sig_from_file_not_native_module_sig() {
    let sig = native_module_sig("std.regex");
    assert!(
        sig.functions.is_empty(),
        "regex fns must no longer be hand-built in native_module_sig (harvested from std/regex.chz)"
    );
    assert!(
        !sig.struct_defs.contains_key("Match"),
        "Match must no longer be hand-built in native_module_sig (harvested from std/regex.chz)"
    );
    assert!(
        !sig.types.contains("Match"),
        "Match must not be in native_module_sig's `types` (harvested from std/regex.chz)"
    );
    // Phase 4f — the sibling native modules std.request/std.process are ALSO file-backed now: their
    // `native_module_sig` arms (fns) AND `export_struct` arms (Response/ProcResult) are RETIRED, so
    // `native_module_sig` exports NOTHING for them (the whole sig comes from the parsed .chz).
    let req = native_module_sig("std.request");
    assert!(
        req.functions.is_empty(),
        "std.request fns must be harvested from std/request.chz, not native_module_sig"
    );
    assert!(
        !req.struct_defs.contains_key("Response"),
        "Response must be harvested from std/request.chz, not native_module_sig"
    );
    assert!(!req.types.contains("Response"));
    let proc = native_module_sig("std.process");
    assert!(
        proc.functions.is_empty(),
        "std.process fns must be harvested from std/process.chz, not native_module_sig"
    );
    assert!(
        !proc.struct_defs.contains_key("ProcResult"),
        "ProcResult must be harvested from std/process.chz, not native_module_sig"
    );
    assert!(!proc.types.contains("ProcResult"));
    // A full graph check that imports std.regex still resolves regex.Match + typed fields.
    entry_ok(
        "import std.regex\nfn main():\n    m: Match = regex.Match(\"x\", 0, 1, [])\n    t: str = m.text\n    s: int = m.start\n    g: List[str] = m.groups\n    print(t + str(s) + \",\".join(g))\n",
    );
}

/// The 5 regex fn FnSigs + the Match StructInfo in the module's ModuleSig must EXACTLY equal what
/// `native_module_sig` used to hand-build — byte-identical provenance move from the deleted arm to the
/// parsed `std/regex.chz`. (`params`/`ret` compared directly; labels/doc are surface-only.)
#[test]
fn regex_fn_sigs_exact() {
    let sig = regex_module_sig_via_graph();
    let m = || Ty::Struct("Match".to_string(), vec![]);
    let expected: Vec<(&str, Vec<Ty>, Ty)> = vec![
        ("is_match", vec![Ty::Str, Ty::Str], Ty::result(Ty::Bool)),
        ("find", vec![Ty::Str, Ty::Str], Ty::result(Ty::option(m()))),
        (
            "find_all",
            vec![Ty::Str, Ty::Str],
            Ty::result(Ty::list(m())),
        ),
        (
            "replace_all",
            vec![Ty::Str, Ty::Str, Ty::Str],
            Ty::result(Ty::Str),
        ),
        (
            "split",
            vec![Ty::Str, Ty::Str],
            Ty::result(Ty::list(Ty::Str)),
        ),
    ];
    assert_eq!(
        sig.functions.len(),
        expected.len(),
        "std.regex must export exactly the 5 regex fns"
    );
    for (name, params, ret) in &expected {
        let fs = sig
            .functions
            .get(*name)
            .unwrap_or_else(|| panic!("std.regex ModuleSig missing fn '{name}'"));
        assert_eq!(&fs.params, params, "fn '{name}' params drifted");
        assert_eq!(&fs.ret, ret, "fn '{name}' return drifted");
        assert_eq!(fs.min_params, params.len(), "fn '{name}' arity drifted");
    }
    // Match's StructInfo comes from the in-module `native struct`, origin Builtin (load-bearing for the
    // both-engines pure-type `bind_import` skip).
    let mi = sig
        .struct_defs
        .get("Match")
        .expect("std.regex ModuleSig must export Match");
    assert_eq!(
        mi.fields,
        vec![
            ("text".to_string(), Ty::Str),
            ("start".to_string(), Ty::Int),
            ("end".to_string(), Ty::Int),
            ("groups".to_string(), Ty::list(Ty::Str)),
        ]
    );
    assert!(matches!(mi.origin, StructOrigin::Builtin));
    assert!(sig.types.contains("Match"));
}

/// DRIFT GUARD — the file-harvested Match StructInfo (fields, in positional order) must byte-match the
/// remaining hand-built layout copies. Field ORDER is load-bearing (positional across compiler
/// `Compiler::new`, interp finalize, `native/regex.rs` match_to_ret, and seed_stdlib_structs), so a
/// silent drift here would trap at runtime on a field read. Assemble the expected layout once and
/// compare both the file harvest and seed_stdlib_structs against it.
#[test]
fn regex_chz_match_matches_handbuilt_layouts() {
    let expected: Vec<(String, Ty)> = vec![
        ("text".into(), Ty::Str),
        ("start".into(), Ty::Int),
        ("end".into(), Ty::Int),
        ("groups".into(), Ty::list(Ty::Str)),
    ];
    // The in-module `native struct` harvest (from the real std/regex.chz).
    let mut c = Checker::new();
    let mut sig = ModuleSig::default();
    let ast = parse_regex_chz();
    c.harvest_native_module(&ast, &mut sig);
    let harvested = sig
        .struct_defs
        .get("Match")
        .expect("std/regex.chz must harvest a `Match` struct");
    assert_eq!(harvested.fields, expected, "harvested Match layout drifted");
    assert!(harvested.type_params.is_empty());
    assert!(harvested.methods.is_empty());
    assert!(matches!(harvested.origin, StructOrigin::Builtin));
    // The harvest must ALSO have populated the 5 fn sigs (whole-module signature source).
    assert_eq!(sig.functions.len(), 5, "std/regex.chz must harvest 5 fns");
    // seed_stdlib_structs's hand-built Match copy (globally-present layout) must agree.
    let seeded = c.structs.get("Match").expect("Match must be seeded");
    assert_eq!(
        seeded.fields, expected,
        "seeded Match layout drifted from std/regex.chz"
    );
    // The harvest must NOT leak `Match` into module-scoped `struct_names` (import-gating preserved):
    // the transient insert during fn-sig resolution is removed.
    assert!(
        !c.struct_names.contains("Match"),
        "harvest_native_module must not leave Match bare-visible (import-gating leak)"
    );
}

// ===== Phase 4f: std.process / std.request are FILE-BACKED (`std/process.chz`, `std/request.chz`)
// whose fields-only `native struct` (ProcResult / Response) + `native fn`s are declared in-module and
// harvested via `harvest_native_module`, retiring BOTH their `native_module_sig` fn arms AND their
// `export_struct` type arms. The one subtlety over regex: get/post/request carry an OPTIONAL trailing
// `timeout_ms: int = 0`, which harvest lowers to `FnSig::optional_tail` from the trailing default. =====

/// Parse a native-module source string into an AST (the SIGNATURE source for a file-backed module).
#[cfg(test)]
fn parse_native_src(src: &str) -> crate::ast::Module {
    let toks = crate::lexer::tokenize(src).expect("native module src must lex");
    crate::parser::parse(toks).expect("native module src must parse")
}

/// Parse the real `std/process.chz` (the phase-4f SIGNATURE source).
#[cfg(test)]
fn parse_process_chz() -> crate::ast::Module {
    let path = crate::resolver::std_root().join("process.chz");
    let src = std::fs::read_to_string(&path).expect("std/process.chz must exist");
    parse_native_src(&src)
}

/// Parse the real `std/request.chz` (the phase-4f SIGNATURE source).
#[cfg(test)]
fn parse_request_chz() -> crate::ast::Module {
    let path = crate::resolver::std_root().join("request.chz");
    let src = std::fs::read_to_string(&path).expect("std/request.chz must exist");
    parse_native_src(&src)
}

/// HARVEST — a `native fn` whose trailing param carries a `= default` marker lowers to an
/// optional-tail FnSig (min_params = len - trailing-defaults). Byte-identical to the deleted
/// hand-built `FnSig::optional_tail(...)` install for std.request's get/post/request.
#[test]
fn harvest_optional_tail_from_trailing_default() {
    let ast = parse_native_src("native fn f(a: str, b: int = 0) -> Result[bool]\n");
    let mut c = Checker::new();
    let mut sig = ModuleSig::default();
    c.harvest_native_module(&ast, &mut sig);
    let fs = sig.functions.get("f").expect("harvest must export fn f");
    assert_eq!(fs.params, vec![Ty::Str, Ty::Int], "params drifted");
    assert_eq!(fs.min_params, 1, "trailing default → min_params = len-1");
    assert_eq!(fs.params.len(), 2);
    // A fn with NO defaults stays plain (min_params == len).
    let ast2 = parse_native_src("native fn g(a: str, b: int) -> Result[bool]\n");
    let mut sig2 = ModuleSig::default();
    c.harvest_native_module(&ast2, &mut sig2);
    let gs = sig2.functions.get("g").unwrap();
    assert_eq!(gs.min_params, 2, "no defaults → min_params == len");
}

/// PROVENANCE + exact sigs — std.process's 3 fns + the `ProcResult` StructInfo come from the
/// file-backed `std/process.chz` (harvested), byte-identical to the deleted hand-built arm.
#[test]
fn process_fn_sigs_exact() {
    let sig = native_module_sig_via_graph("process");
    let proc = || Ty::Struct("ProcResult".to_string(), vec![]);
    let expected: Vec<(&str, Vec<Ty>, Ty, usize)> = vec![
        ("cmd", vec![Ty::Str], Ty::result(Ty::Str), 1),
        ("run", vec![Ty::Str], Ty::result(proc()), 1),
        (
            "run_args",
            vec![Ty::Str, Ty::list(Ty::Str)],
            Ty::result(proc()),
            2,
        ),
    ];
    assert_eq!(sig.functions.len(), expected.len(), "std.process fn count");
    for (name, params, ret, minp) in &expected {
        let fs = sig
            .functions
            .get(*name)
            .unwrap_or_else(|| panic!("std.process missing fn '{name}'"));
        assert_eq!(&fs.params, params, "fn '{name}' params drifted");
        assert_eq!(&fs.ret, ret, "fn '{name}' return drifted");
        assert_eq!(fs.min_params, *minp, "fn '{name}' arity drifted");
    }
    let mi = sig
        .struct_defs
        .get("ProcResult")
        .expect("std.process must export ProcResult");
    assert_eq!(
        mi.fields,
        vec![
            ("stdout".to_string(), Ty::Str),
            ("stderr".to_string(), Ty::Str),
            ("code".to_string(), Ty::Int),
        ]
    );
    assert!(matches!(mi.origin, StructOrigin::Builtin));
    assert!(sig.types.contains("ProcResult"));
}

/// PROVENANCE + exact sigs — std.request's 7 fns (incl. get/post/request's OPTIONAL trailing
/// `timeout_ms`) + the `Response` StructInfo come from the file-backed `std/request.chz`.
#[test]
fn request_fn_sigs_exact() {
    let sig = native_module_sig_via_graph("request");
    let resp = || Ty::Struct("Response".to_string(), vec![]);
    // (name, params, ret, min_params). get/post/request are optional-tail (min = len-1).
    let expected: Vec<(&str, Vec<Ty>, Ty, usize)> = vec![
        ("get", vec![Ty::Str, Ty::Int], Ty::result(resp()), 1),
        (
            "get_bytes",
            vec![Ty::Str, Ty::Int],
            Ty::result(Ty::Bytes),
            1,
        ),
        (
            "post",
            vec![Ty::Str, Ty::Str, Ty::Int],
            Ty::result(resp()),
            2,
        ),
        (
            "request",
            vec![
                Ty::Str,
                Ty::Str,
                Ty::Str,
                Ty::Map(Box::new(Ty::Str), Box::new(Ty::Str)),
                Ty::Int,
            ],
            Ty::result(resp()),
            4,
        ),
        ("put", vec![Ty::Str, Ty::Str], Ty::result(resp()), 2),
        ("patch", vec![Ty::Str, Ty::Str], Ty::result(resp()), 2),
        ("delete", vec![Ty::Str], Ty::result(resp()), 1),
        ("head", vec![Ty::Str], Ty::result(resp()), 1),
    ];
    assert_eq!(sig.functions.len(), expected.len(), "std.request fn count");
    for (name, params, ret, minp) in &expected {
        let fs = sig
            .functions
            .get(*name)
            .unwrap_or_else(|| panic!("std.request missing fn '{name}'"));
        assert_eq!(&fs.params, params, "fn '{name}' params drifted");
        assert_eq!(&fs.ret, ret, "fn '{name}' return drifted");
        assert_eq!(fs.min_params, *minp, "fn '{name}' arity drifted");
    }
    let ri = sig
        .struct_defs
        .get("Response")
        .expect("std.request must export Response");
    assert_eq!(
        ri.fields,
        vec![
            ("status".to_string(), Ty::Int),
            ("body".to_string(), Ty::Str),
            (
                "headers".to_string(),
                Ty::Map(Box::new(Ty::Str), Box::new(Ty::Str))
            ),
        ]
    );
    assert!(matches!(ri.origin, StructOrigin::Builtin));
    assert!(sig.types.contains("Response"));
}

/// Phase 4f — request.get/post/request accept the OPTIONAL trailing `timeout_ms` BOTH ways (the
/// harvested optional-tail sig, min_params = len-1). Byte-identical to the deleted hand-built
/// `FnSig::optional_tail(...)` install.
#[test]
fn request_optional_timeout_arg_typechecks() {
    // With and without the trailing timeout_ms — both must check.
    entry_ok(
        "import std.request\nfn main():\n    a := request.get(\"http://x\")\n    b := request.get(\"http://x\", 500)\n    c := request.post(\"http://x\", \"b\")\n    d := request.post(\"http://x\", \"b\", 500)\n    e := request.request(\"GET\", \"http://x\", \"\", {\"h\": \"v\"})\n    f := request.request(\"GET\", \"http://x\", \"\", {\"h\": \"v\"}, 500)\n    print(\"ok\")\n",
    );
    // Too many args is still rejected (arity ceiling = params.len()).
    entry_rejects(
        "import std.request\nfn main():\n    x := request.get(\"http://x\", 1, 2)\n    print(\"x\")\n",
        "get",
    );
}

/// DRIFT GUARD — the file-harvested `ProcResult` StructInfo (fields, positional order) must byte-match
/// the remaining hand-built layout copies (seed_stdlib_structs + the runtime copies). A silent reorder
/// would trap at runtime on a field read.
#[test]
fn procresult_chz_matches_handbuilt_layouts() {
    let expected: Vec<(String, Ty)> = vec![
        ("stdout".into(), Ty::Str),
        ("stderr".into(), Ty::Str),
        ("code".into(), Ty::Int),
    ];
    let mut c = Checker::new();
    let mut sig = ModuleSig::default();
    let ast = parse_process_chz();
    c.harvest_native_module(&ast, &mut sig);
    let harvested = sig
        .struct_defs
        .get("ProcResult")
        .expect("std/process.chz must harvest a `ProcResult` struct");
    assert_eq!(harvested.fields, expected, "harvested ProcResult drifted");
    assert!(harvested.type_params.is_empty());
    assert!(harvested.methods.is_empty());
    assert!(matches!(harvested.origin, StructOrigin::Builtin));
    assert_eq!(sig.functions.len(), 3, "std/process.chz must harvest 3 fns");
    // seed_stdlib_structs's copy must agree.
    let seeded = c.structs.get("ProcResult").expect("ProcResult seeded");
    assert_eq!(seeded.fields, expected, "seeded ProcResult drifted");
    // Import-gating preserved: no bare-name leak.
    assert!(
        !c.struct_names.contains("ProcResult"),
        "harvest must not leave ProcResult bare-visible"
    );
}

/// DRIFT GUARD — the file-harvested `Response` StructInfo must byte-match the hand-built copies.
#[test]
fn response_chz_matches_handbuilt_layouts() {
    let expected: Vec<(String, Ty)> = vec![
        ("status".into(), Ty::Int),
        ("body".into(), Ty::Str),
        (
            "headers".into(),
            Ty::Map(Box::new(Ty::Str), Box::new(Ty::Str)),
        ),
    ];
    let mut c = Checker::new();
    let mut sig = ModuleSig::default();
    let ast = parse_request_chz();
    c.harvest_native_module(&ast, &mut sig);
    let harvested = sig
        .struct_defs
        .get("Response")
        .expect("std/request.chz must harvest a `Response` struct");
    assert_eq!(harvested.fields, expected, "harvested Response drifted");
    assert!(harvested.type_params.is_empty());
    assert!(harvested.methods.is_empty());
    assert!(matches!(harvested.origin, StructOrigin::Builtin));
    assert_eq!(sig.functions.len(), 8, "std/request.chz must harvest 8 fns");
    let seeded = c.structs.get("Response").expect("Response seeded");
    assert_eq!(seeded.fields, expected, "seeded Response drifted");
    assert!(
        !c.struct_names.contains("Response"),
        "harvest must not leave Response bare-visible"
    );
}

// ===== phase 4d: std.math/io/os/rand/fs are FILE-BACKED (native fn decls in std/<M>.chz) =====

/// PROVENANCE — the `"std.math"` / `"std.io"` / `"std.os"` / `"std.rand"` / `"std.fs"` arms are DELETED
/// from `native_module_sig`; each module's whole function signature now comes from parsing its real
/// `std/<M>.chz`. So `native_module_sig("std.<M>")` exports NO functions/values, while a full graph
/// check that `import std.<M>` still resolves every fn (and math's pi/e values). A sibling native
/// module that stays hand-built (std.encoding) is unaffected.
#[test]
fn math_io_os_rand_fs_sig_from_file_not_native_module_sig() {
    for m in ["std.math", "std.io", "std.os", "std.rand", "std.fs"] {
        let sig = native_module_sig(m);
        assert!(
            sig.functions.is_empty(),
            "{m} fns must no longer be hand-built in native_module_sig (harvested from std/<M>.chz)"
        );
        assert!(
            sig.values.is_empty(),
            "{m} values must no longer be hand-built in native_module_sig"
        );
        assert!(
            sig.numeric_poly.is_empty(),
            "{m} numeric_poly must no longer be hand-built in native_module_sig"
        );
    }
    // native_module_sig is still the home for the residual type-license modules — std.ffi keeps its
    // opaque `ptr` handle + fixed-width C-ABI integer names there (no runtime value, no .chz syntax for
    // a bare type-license alias). std.concurrency is FILE-BACKED now (phase 4c-concurrency): its arm is
    // DELETED entirely, so `native_module_sig("std.concurrency").types` is EMPTY.
    assert!(!native_module_sig("std.ffi").types.is_empty());
    assert!(native_module_sig("std.concurrency").types.is_empty());
}

/// The effective (graph-built) sigs for a representative fn of each migrated module must EXACTLY equal
/// what the deleted arm used to hand-build — byte-identical provenance move from arm → parsed .chz.
#[test]
fn math_io_os_rand_fs_representative_sigs_exact() {
    // (module, fn, params, ret)
    let math = native_module_sig_via_graph("math");
    let sqrt = math.functions.get("sqrt").expect("math.sqrt");
    assert_eq!(sqrt.params, vec![Ty::Float]);
    assert_eq!(sqrt.ret, Ty::Float);
    let pow = math.functions.get("pow").expect("math.pow");
    assert_eq!(pow.params, vec![Ty::Float, Ty::Float]);
    assert_eq!(pow.ret, Ty::Float);
    let is_nan = math.functions.get("is_nan").expect("math.is_nan");
    assert_eq!(is_nan.ret, Ty::Bool);
    // math.pi / math.e are float module VALUES (not fns) — reattached from native_consts.
    assert_eq!(math.values.get("pi"), Some(&Ty::Float));
    assert_eq!(math.values.get("e"), Some(&Ty::Float));
    // math.abs is numeric-polymorphic — reattached from MODULE_NUMERIC_POLY.
    assert!(math.numeric_poly.contains("abs"));
    // `divmod` is a BODIED Chezzi fn harvested as a module member alongside the native decls (the
    // hybrid native+Chezzi module form) — it counts as an exported fn.
    let divmod = math.functions.get("divmod").expect("math.divmod");
    assert_eq!(divmod.params, vec![Ty::Int, Ty::Int]);
    assert_eq!(divmod.ret, Ty::Tuple(vec![Ty::Int, Ty::Int]));
    assert_eq!(
        math.functions.len(),
        32,
        "std.math must export exactly 32 fns (31 native + bodied `divmod`)"
    );

    let io = native_module_sig_via_graph("io");
    let print = io.functions.get("print").expect("io.print");
    assert_eq!(print.params, vec![Ty::Str]);
    assert_eq!(print.ret, Ty::Nil, "io.print must return nil, not Unknown");
    let write_file = io.functions.get("write_file").expect("io.write_file");
    assert_eq!(write_file.params, vec![Ty::Str, Ty::Str]);
    assert_eq!(write_file.ret, Ty::result(Ty::Nil));
    assert_eq!(
        io.functions.get("read_line").unwrap().ret,
        Ty::option(Ty::Str)
    );
    assert_eq!(io.functions.get("input").unwrap().params, vec![Ty::Str]);
    assert_eq!(io.functions.get("input").unwrap().ret, Ty::option(Ty::Str));
    assert_eq!(io.functions.get("flush").unwrap().ret, Ty::Nil);
    // R1 — the binary whole-file twins.
    let read_bytes = io.functions.get("read_bytes").expect("io.read_bytes");
    assert_eq!(read_bytes.params, vec![Ty::Str]);
    assert_eq!(read_bytes.ret, Ty::result(Ty::Bytes));
    let write_bytes = io.functions.get("write_bytes").expect("io.write_bytes");
    assert_eq!(write_bytes.params, vec![Ty::Str, Ty::Bytes]);
    assert_eq!(write_bytes.ret, Ty::result(Ty::Nil));
    // R2 — the Writer openers/handles. `create`/`append` -> Result[Writer]; `stdout`/`stderr` -> Writer;
    // `buffered(w, size = 8192)` -> Writer (optional-tail size ⇒ min_params 1).
    assert_eq!(io.functions.get("create").unwrap().params, vec![Ty::Str]);
    assert_eq!(
        io.functions.get("create").unwrap().ret,
        Ty::result(Ty::Writer)
    );
    assert_eq!(
        io.functions.get("append").unwrap().ret,
        Ty::result(Ty::Writer)
    );
    assert_eq!(io.functions.get("stdout").unwrap().ret, Ty::Writer);
    let buffered = io.functions.get("buffered").expect("io.buffered");
    assert_eq!(buffered.params, vec![Ty::Writer, Ty::Int]);
    assert_eq!(buffered.min_params, 1);
    assert_eq!(buffered.ret, Ty::Writer);
    // The Writer method table (harvested from the `native struct Writer`).
    let writer = io.struct_defs.get("Writer").expect("io Writer struct_def");
    assert_eq!(
        writer.methods.get("write").unwrap().ret,
        Ty::result(Ty::Int)
    );
    assert_eq!(
        writer.methods.get("close").unwrap().ret,
        Ty::result(Ty::Nil)
    );
    // R2b — the Reader opener + method table. `open` -> Result[Reader]; `read_line` -> Option[str];
    // `read_bytes(n)` -> Result[bytes]; `close` -> Result[nil].
    assert_eq!(io.functions.get("open").unwrap().params, vec![Ty::Str]);
    assert_eq!(
        io.functions.get("open").unwrap().ret,
        Ty::result(Ty::Reader)
    );
    let reader = io.struct_defs.get("Reader").expect("io Reader struct_def");
    assert_eq!(
        reader.methods.get("read_line").unwrap().ret,
        Ty::option(Ty::Str)
    );
    assert_eq!(
        reader.methods.get("read_bytes").unwrap().ret,
        Ty::result(Ty::Bytes)
    );
    assert_eq!(
        reader.methods.get("close").unwrap().ret,
        Ty::result(Ty::Nil)
    );
    assert_eq!(io.functions.get("isatty").unwrap().params, Vec::<Ty>::new());
    assert_eq!(io.functions.get("isatty").unwrap().ret, Ty::Bool);
    assert_eq!(io.functions.len(), 20);

    let os = native_module_sig_via_graph("os");
    assert_eq!(os.functions.get("getcwd").unwrap().ret, Ty::result(Ty::Str));
    assert_eq!(os.functions.get("args").unwrap().ret, Ty::list(Ty::Str));
    assert_eq!(os.functions.get("env").unwrap().ret, Ty::option(Ty::Str));
    let exit = os.functions.get("exit").expect("os.exit");
    assert_eq!(exit.params, vec![Ty::Int]);
    assert_eq!(exit.ret, Ty::Nil);
    // gaps §6 system query + mutation fns.
    assert_eq!(os.functions.get("getpid").unwrap().ret, Ty::Int);
    assert_eq!(os.functions.get("platform").unwrap().ret, Ty::Str);
    assert_eq!(os.functions.get("hostname").unwrap().ret, Ty::Str);
    assert_eq!(
        os.functions.get("home_dir").unwrap().ret,
        Ty::option(Ty::Str)
    );
    assert_eq!(os.functions.get("temp_dir").unwrap().ret, Ty::Str);
    assert_eq!(
        os.functions.get("environ").unwrap().ret,
        Ty::map(Ty::Str, Ty::Str)
    );
    let setenv = os.functions.get("setenv").expect("os.setenv");
    assert_eq!(setenv.params, vec![Ty::Str, Ty::Str]);
    assert_eq!(setenv.ret, Ty::Nil);
    assert_eq!(os.functions.get("chdir").unwrap().ret, Ty::result(Ty::Nil));
    assert_eq!(os.functions.len(), 12);

    let rand = native_module_sig_via_graph("rand");
    let ri = rand.functions.get("int").expect("rand.int");
    assert_eq!(ri.params, vec![Ty::Int, Ty::Int]);
    assert_eq!(ri.ret, Ty::Int);
    assert_eq!(rand.functions.get("float").unwrap().ret, Ty::Float);
    assert_eq!(rand.functions.get("bool").unwrap().ret, Ty::Bool);
    assert_eq!(rand.functions.get("seed").unwrap().ret, Ty::Nil);
    assert_eq!(rand.functions.len(), 4);

    let fs = native_module_sig_via_graph("fs");
    let list_dir = fs.functions.get("list_dir").expect("fs.list_dir");
    assert_eq!(list_dir.params, vec![Ty::Str]);
    assert_eq!(list_dir.ret, Ty::result(Ty::list(Ty::Str)));
    assert_eq!(fs.functions.get("exists").unwrap().ret, Ty::Bool);
    assert_eq!(fs.functions.get("size").unwrap().ret, Ty::result(Ty::Int));
    assert_eq!(fs.functions.get("mkdir").unwrap().ret, Ty::result(Ty::Nil));
    // fs-trio (fs grab-bag): canonicalize -> Result[str], chmod(str,int) -> Result[nil], atomic_write.
    assert_eq!(
        fs.functions.get("canonicalize").unwrap().ret,
        Ty::result(Ty::Str)
    );
    assert_eq!(
        fs.functions.get("chmod").unwrap().params,
        vec![Ty::Str, Ty::Int]
    );
    assert_eq!(
        fs.functions.get("atomic_write").unwrap().ret,
        Ty::result(Ty::Nil)
    );
    // --- fs.stat/fs.walk (gaps §6 metadata READ + recursive walk): 15 + stat + walk = 17.
    // (FileInfo is a native struct, not a function — not counted here.)
    assert_eq!(fs.functions.len(), 17);
}

/// Hybrid native+Chezzi module: a BODIED `fn` (`math.divmod`) declared alongside the bodyless
/// `native fn`s in `std/math.chz` is harvested as a real module member — callable qualified, via
/// `import NAME from PATH`, and coexisting with a native sibling in the same module. (Its body's type
/// safety is guarded end-to-end in `tests/hybrid_native_module.rs`; here we pin that it resolves as a
/// member at all — the harvest PASS 2b + native-arm body-check wiring.)
#[test]
fn hybrid_native_module_bodied_fn_is_a_member() {
    entry_ok("import std.math\nx := math.divmod(17, 5)\nprint(x)\n");
    entry_ok("import divmod from std.math\nprint(divmod(20, 6))\n");
    // coexists with a native sibling fn (`gcd`) in the SAME module.
    entry_ok("import std.math\nprint(math.divmod(9, 2))\nprint(math.gcd(12, 18))\n");
}

/// Hover doc preserved after migration: math.sqrt (and an io/os fn) still carry the authored blurb via
/// `attach_native_module_metadata`, even though math/io/os fns are now harvested (absent from the raw
/// `native_module_sig`).
#[test]
fn math_io_os_fn_hover_doc_preserved() {
    let math = native_module_sig_via_graph("math");
    assert_eq!(
        math.functions.get("sqrt").unwrap().doc.as_deref(),
        Some("square root (NaN for a negative argument)")
    );
    let io = native_module_sig_via_graph("io");
    assert_eq!(
        io.functions.get("print").unwrap().doc.as_deref(),
        Some("write a line to stdout (with a trailing newline)")
    );
    let os = native_module_sig_via_graph("os");
    assert_eq!(
        os.functions.get("getcwd").unwrap().doc.as_deref(),
        Some("the current working directory (Result)")
    );
}

/// Runtime dispatch is UNTOUCHED by the refactor: the native member/const tables still carry the same
/// impls, name-keyed. (The `native fn` decl only feeds the front-end signature.)
#[test]
fn math_io_os_rand_fs_runtime_tables_unchanged() {
    assert_eq!(crate::native::native_members("std.math").len(), 31);
    // std.io: 9 original + R2's 5 Writer openers + R2b's `open` (all → intercepted) + read_all +
    // read_char (R2 grab-bag stdin twins) + 3 isatty TTY-detection variants (gaps §6) = 20.
    assert_eq!(crate::native::native_members("std.io").len(), 20);
    // std.os: 4 original (args/env/getcwd/exit) + gaps §6's 8 system fns
    // (getpid/platform/hostname/home_dir/temp_dir/environ/setenv/chdir) = 12.
    assert_eq!(crate::native::native_members("std.os").len(), 12);
    assert_eq!(crate::native::native_members("std.rand").len(), 4);
    // --- fs.stat/fs.walk (gaps §6): 15 original + stat + walk = 17 runtime members.
    assert_eq!(crate::native::native_members("std.fs").len(), 17);
    let consts: Vec<&str> = crate::native::native_consts("std.math")
        .iter()
        .map(|(n, _)| *n)
        .collect();
    assert_eq!(consts, vec!["pi", "e", "inf", "nan"]);
    assert!(crate::native::is_file_backed_native("std.math"));
    assert!(crate::native::is_file_backed_native("std.regex"));
    // std.net (4c-net) + std.ffi (4c-ffi) + std.concurrency (4c-concurrency) are FILE-BACKED now —
    // std.concurrency was the LAST virtual native std module, so EVERY native std module is now file-
    // backed (there is no virtual one left to assert `!is_file_backed_native` against).
    assert!(crate::native::is_file_backed_native("std.net"));
    assert!(crate::native::is_file_backed_native("std.ffi"));
    assert!(crate::native::is_file_backed_native("std.concurrency"));
}

/// Phase 4c-net — std.net is FILE-BACKED: its `Socket`/`Listener` native structs carry a harvested
/// METHOD table (the new native-method-binding capability) and its `connect`/`listen` free fns come
/// from `std/net.chz`, NOT the retired hand-built `native_module_sig` arm.
#[test]
fn net_sig_from_file_not_native_module_sig() {
    // The hand-built arm is retired — `native_module_sig("std.net")` exports NOTHING now.
    let raw = native_module_sig("std.net");
    assert!(
        raw.functions.is_empty() && raw.types.is_empty(),
        "std.net must no longer be hand-built in native_module_sig (harvested from std/net.chz)"
    );
    // The graph-built sig carries the free fns + both native types WITH their method tables.
    let sig = native_module_sig_via_graph("net");
    let connect = sig.functions.get("connect").expect("net.connect");
    assert_eq!(connect.params, vec![Ty::Str]);
    assert_eq!(connect.ret, Ty::result(Ty::Socket));
    let listen = sig.functions.get("listen").expect("net.listen");
    assert_eq!(listen.params, vec![Ty::Str]);
    assert_eq!(listen.ret, Ty::result(Ty::Listener));
    let socket = sig
        .struct_defs
        .get("Socket")
        .expect("net.Socket struct_def");
    let mut smeths: Vec<&str> = socket.methods.keys().map(String::as_str).collect();
    smeths.sort();
    assert_eq!(
        smeths,
        ["close", "read", "read_bytes", "write", "write_bytes"]
    );
    // R1 — the binary twins: `read_bytes -> Result[bytes]`, `write_bytes(bytes) -> Result[int]`.
    let read_bytes = socket.methods.get("read_bytes").unwrap();
    assert_eq!(read_bytes.params, vec![Ty::Int, Ty::Int]);
    assert_eq!(read_bytes.ret, Ty::result(Ty::Bytes));
    let write_bytes = socket.methods.get("write_bytes").unwrap();
    assert_eq!(write_bytes.params, vec![Ty::Bytes, Ty::Int]);
    assert_eq!(write_bytes.ret, Ty::result(Ty::Int));
    // read/write are optional-tail (the trailing `timeout_ms`); close is nil.
    let read = socket.methods.get("read").unwrap();
    assert_eq!(read.params, vec![Ty::Int, Ty::Int]);
    assert_eq!(read.ret, Ty::result(Ty::Str));
    assert_eq!(read.min_params, 1);
    let write = socket.methods.get("write").unwrap();
    assert_eq!(write.params, vec![Ty::Str, Ty::Int]);
    assert_eq!(write.ret, Ty::result(Ty::Int));
    assert_eq!(write.min_params, 1);
    let close = socket.methods.get("close").unwrap();
    assert_eq!(close.params, Vec::<Ty>::new());
    assert_eq!(close.ret, Ty::Nil);
    let listener = sig
        .struct_defs
        .get("Listener")
        .expect("net.Listener struct_def");
    let mut lmeths: Vec<&str> = listener.methods.keys().map(String::as_str).collect();
    lmeths.sort();
    assert_eq!(lmeths, ["accept", "addr", "close"]);
    let accept = listener.methods.get("accept").unwrap();
    assert_eq!(accept.params, vec![Ty::Int]);
    assert_eq!(accept.ret, Ty::result(Ty::Socket));
    assert_eq!(accept.min_params, 0);
    let addr = listener.methods.get("addr").unwrap();
    assert_eq!(addr.params, Vec::<Ty>::new());
    assert_eq!(addr.ret, Ty::result(Ty::Str));
}

// ===== M9: std.request (Response struct) =====

#[test]
fn native_request_get_returns_response_with_typed_fields() {
    entry_ok(
        "import std.request\nfn main():\n    match request.get(\"http://x\"):\n        Ok(resp):\n            st: int = resp.status\n            body: str = resp.body\n            h: Map[str, str] = resp.headers\n            print(body + str(st) + h[\"k\"])\n        Err(e): print(e)\n",
    );
}

#[test]
fn native_request_post_takes_url_and_body() {
    entry_ok(
        "import std.request\nfn main():\n    match request.post(\"http://x\", \"payload\"):\n        Ok(resp): print(str(resp.status))\n        Err(e): print(e)\n",
    );
}

#[test]
fn native_request_get_arg_must_be_str() {
    entry_rejects(
        "import std.request\nfn main():\n    print(request.get(5))\n",
        "argument 1 of 'get'",
    );
}

#[test]
fn native_request_response_unknown_field_rejected() {
    entry_rejects(
        "import std.request\nfn main():\n    match request.get(\"http://x\"):\n        Ok(resp): print(resp.nope)\n        Err(e): print(e)\n",
        "has no field 'nope'",
    );
}

// ===== Task 3/5: Match/Response/ProcResult are module-owned, not global-reserved =====

// (2) The bare TYPE NAME (annotation/construction) requires importing the owning module.
#[test]
fn bare_match_unknown_without_import() {
    entry_rejects(
        "fn main():\n    m: Match = Match(\"x\", 0, 1, [])\n    print(m.text)\n",
        "unknown type 'Match'",
    );
}

#[test]
fn bare_response_unknown_without_import() {
    entry_rejects(
        "fn main():\n    r: Response = Response(200, \"\", {})\n    print(str(r.status))\n",
        "unknown type 'Response'",
    );
}

#[test]
fn bare_procresult_unknown_without_import() {
    entry_rejects(
        "fn main():\n    p: ProcResult = ProcResult(\"\", \"\", 0)\n    print(str(p.code))\n",
        "unknown type 'ProcResult'",
    );
}

// A bare `FileInfo` without `import std.fs` gets the actionable import HINT (the types_by_name
// reverse-index 4th touch point — omitting it degrades to a generic "unknown type" with no hint).
#[test]
fn bare_fileinfo_hints_std_fs() {
    entry_rejects(
        "fn f(x: FileInfo):\n    print(\"hi\")\n",
        "import it from std.fs",
    );
}

// The unknown-type error hints the owning module.
#[test]
fn unknown_match_hint_points_at_module() {
    entry_rejects(
        "fn main():\n    m: Match = ProcResult(\"\", \"\", 0)\n    print(m.text)\n",
        "std.regex",
    );
}

// (3) `import std.regex` exposes `Match` bare for annotation AND qualified ctor `regex.Match(...)`.
#[test]
fn import_licenses_bare_match() {
    entry_ok(
        "import std.regex\nfn main():\n    m: Match = regex.Match(\"x\", 0, 1, [])\n    print(m.text)\n",
    );
}

#[test]
fn import_licenses_bare_response() {
    entry_ok(
        "import std.request\nfn main():\n    r: Response = request.Response(200, \"ok\", {})\n    print(r.body)\n",
    );
}

#[test]
fn import_licenses_bare_procresult() {
    entry_ok(
        "import std.process\nfn main():\n    p: ProcResult = process.ProcResult(\"o\", \"e\", 0)\n    print(p.stdout)\n",
    );
}

// (4) `import Match from std.regex` (selective from-import) exposes the bare name.
#[test]
fn from_import_licenses_bare_match() {
    entry_ok(
        "import Match from std.regex\nfn main():\n    m: Match = Match(\"x\", 0, 1, [])\n    print(m.text)\n",
    );
}

#[test]
fn from_import_licenses_bare_response() {
    entry_ok(
        "import Response from std.request\nfn main():\n    r: Response = Response(200, \"ok\", {})\n    print(r.body)\n",
    );
}

// (5) The names are FREED for user types: a user `struct Response` (no import) is their own type.
#[test]
fn user_struct_response_without_import_ok() {
    entry_ok(
        "struct Response:\n    code: int\nfn main():\n    r := Response(7)\n    print(str(r.code))\n",
    );
}

#[test]
fn user_struct_match_without_import_ok() {
    entry_ok(
        "struct Match:\n    score: int\nfn main():\n    m := Match(3)\n    print(str(m.score))\n",
    );
}

// COLLISION: `import X from M` + a same-named user `struct X` (for the four native struct-modeled
// types Ref/Match/Response/ProcResult, and every other import-gated std struct) must be a CLEAN
// check-time `already defined` error — NEVER accept-then-trap at runtime. The import licenses the
// bare name as a Builtin-origin layout; a user `struct X` would overwrite the seed and carry the
// user layout while the runtime returns/constructs the native shape → field trap. These types are
// NOT reserved (a bare unimported `struct Match` is legal) — the import is an ordinary name
// collision, exactly like the enum/newtype/typealias siblings already report.
#[test]
fn import_plus_same_name_struct_decl_rejected() {
    // from-import form, the struct-modeled natives
    for (src, name) in [
        (
            "import Match from std.regex\nstruct Match:\n    v: int\nfn main():\n    print(1)\nmain()\n",
            "Match",
        ),
        (
            "import Response from std.request\nstruct Response:\n    v: int\nfn main():\n    print(1)\nmain()\n",
            "Response",
        ),
        (
            "import ProcResult from std.process\nstruct ProcResult:\n    v: int\nfn main():\n    print(1)\nmain()\n",
            "ProcResult",
        ),
    ] {
        let errs = check_entry(src);
        assert!(
            errs.iter().any(|e| e.message.contains("already defined")
                && e.message.contains(name)
                && !e.message.contains("reserved")),
            "expected `{name}` already-defined error, got: {errs:?}"
        );
    }
    // whole-module form
    entry_rejects(
        "import std.regex\nstruct Match:\n    x: int\nfn main():\n    print(1)\nmain()\n",
        "already defined",
    );
}

// BOUNDARY: a similar-but-distinct user struct name stays legal even with the import present —
// the gate keys on the exact imported name, not a prefix/substring.
#[test]
fn import_does_not_over_reject_distinct_struct_name() {
    entry_ok(
        "import Match from std.regex\nstruct MatchBox:\n    v: int\nfn main():\n    b := MatchBox(5)\n    print(str(b.v))\nmain()\n",
    );
}

// BOUNDARY: module-owned intent preserved — the bare name is FREE for a user struct when the owning
// module is NOT imported (ProcResult lacked a bare-ok test; Ref/Match/Response covered elsewhere).
#[test]
fn bare_struct_procresult_without_import_ok() {
    entry_ok(
        "struct ProcResult:\n    x: int\nfn main():\n    r := ProcResult(5)\n    print(str(r.x))\nmain()\n",
    );
}

// BOUNDARY: `import Match as M` binds only `M` (not `Match`) into `imported_builtin_types`, so a
// same-named `struct Match` is a FREE name, not a collision — must stay accepted.
#[test]
fn import_alias_native_struct_same_name_ok() {
    entry_ok(
        "import Match as M from std.regex\nstruct Match:\n    v: int\nfn main():\n    print(str(Match(1).v))\nmain()\n",
    );
}

// PARITY TARGET: the enum sibling arm already reports `already defined` for the same import
// collision (it collides via `struct_names`) — pin it so the struct arm keeps matching it.
#[test]
fn import_plus_same_name_enum_still_already_defined() {
    entry_rejects(
        "import Match from std.regex\nenum Match:\n    A\nfn main():\n    print(1)\nmain()\n",
        "already defined",
    );
}

// REGRESSION: genuine GLOBAL reserved types stay `reserved (builtin)` — the collision-message fix
// for imported natives must not touch the true-reserved path.
#[test]
fn struct_reserved_global_still_reserved() {
    entry_rejects(
        "struct int:\n    x: int\nfn main():\n    print(1)\nmain()\n",
        "reserved (builtin)",
    );
    entry_rejects(
        "struct Channel:\n    x: int\nfn main():\n    print(1)\nmain()\n",
        "reserved (builtin)",
    );
}

// PHASE-4b BUG: `harvest_native_module` resolves the regex fns' return types (which reference the
// import-gated `Match`, e.g. `find -> Result[Option[Match]]`) via `resolve_type` while running in the
// native-module arm WITHOUT `begin_module` — so `self.structs` is LEFTOVER from the previously-checked
// module. If a sibling user module declared a generic `struct Match[T]` (overwriting the seeded
// nparams-0 native Match), `resolve_type`'s struct arm fires a spurious `type 'Match' expects 1 type
// argument(s), got 0`, falsely rejecting a valid program. The harvest must resolve `Match` against its
// OWN native layout, immune to sibling-module / graph-order state (zero-observable-change contract).
#[test]
fn regex_harvest_immune_to_sibling_generic_match() {
    let t = TmpDir::new();
    // A user module legally declaring a generic `struct Match[T]` (it does NOT whole-module import
    // std.regex, so the decl is allowed). Checked immediately before the std.regex native arm.
    t.write("helper.chz", "struct Match[T]:\n    val: T\n");
    let entry = t.write(
        "main.chz",
        "import helper\nimport std.regex\nfn main():\n    match regex.find(\"a\", \"a\"):\n        Ok(opt): print(\"ok\")\n        Err(e): print(e)\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(
        errs.is_empty(),
        "regex harvest must be immune to a sibling `struct Match[T]`; got spurious errors: {errs:?}"
    );
}

// ===== Phase 4e — std.encoding / std.crypto / std.uuid / std.time file-backed native modules =====

/// PROVENANCE (phase 4e) — the encoding/crypto/uuid arms are DELETED from `native_module_sig`, and the
/// time arm keeps ONLY the `timer` opcode-license (no `func()` calls). Their function sigs now come from
/// parsing `std/<module>.chz` (harvested via `harvest_native_module`). So `native_module_sig` exports NO
/// functions for these four, while `native_module_sig("std.time").types` still licenses `timer`.
#[test]
fn enc_crypto_uuid_time_sig_from_file_not_native_module_sig() {
    for m in ["std.encoding", "std.crypto", "std.uuid", "std.time"] {
        assert!(
            native_module_sig(m).functions.is_empty(),
            "{m} fns must no longer be hand-built in native_module_sig (harvested from std/*.chz)"
        );
    }
    // std.time keeps its opcode-license for `timer` (NOT a native fn — no runtime value).
    let time = native_module_sig("std.time");
    assert!(
        time.types.contains("timer"),
        "std.time must keep `timer` in its type-license set (opcode-backed builtin)"
    );
    // std.net + std.concurrency are FILE-BACKED now (phase 4c): their arms are retired entirely
    // (asserted in net_sig_from_file_not_native_module_sig / concurrency_sig_from_file_not_native_module_sig).
    // std.ffi keeps a residual type-license arm — native_module_sig is still its home.
    assert!(!native_module_sig("std.ffi").types.is_empty());
    // std.ffi is FILE-BACKED now (phase 4c-ffi): its 59 fns are harvested from std/ffi.chz, so
    // `native_module_sig("std.ffi")` exports NO functions. Its arm is REDUCED to only the type-license
    // tail — the opaque `ptr` handle type + the eight fixed-width C-ABI integer names (int8..uint64) —
    // which have no runtime value and cannot be spelled as a `native fn` decl (there is no .chz syntax
    // for a bare type-license name aliasing Ty::Int/Ty::Ptr).
    let ffi = native_module_sig("std.ffi");
    assert!(
        ffi.functions.is_empty(),
        "std.ffi fns must be harvested from std/ffi.chz, not native_module_sig"
    );
    assert!(
        ffi.types.contains("ptr"),
        "std.ffi must keep `ptr` in its type-license set (opaque C-ABI handle, no runtime value)"
    );
    for tn in crate::native::ffi::TYPE_NAMES {
        assert!(
            ffi.types.contains(*tn),
            "std.ffi must keep the fixed-width C-ABI type name `{tn}` in its type-license set"
        );
    }
}

/// EXACT SIGS — the harvested fn sigs must byte-match what `native_module_sig` used to hand-build.
#[test]
fn enc_fn_sigs_exact() {
    let sig = native_module_sig_via_graph("encoding");
    let expected: Vec<(&str, Vec<Ty>, Ty)> = vec![
        ("base64_encode", vec![Ty::Str], Ty::Str),
        ("base64_encode_url", vec![Ty::Str], Ty::Str),
        ("base64_decode", vec![Ty::Str], Ty::result(Ty::Str)),
        ("base64_decode_url", vec![Ty::Str], Ty::result(Ty::Str)),
        ("base64_encode_bytes", vec![Ty::Bytes], Ty::Str),
        ("base64_decode_bytes", vec![Ty::Str], Ty::result(Ty::Bytes)),
        ("hex_encode", vec![Ty::Str], Ty::Str),
        ("hex_decode", vec![Ty::Str], Ty::result(Ty::Str)),
        ("url_encode", vec![Ty::Str], Ty::Str),
        ("url_decode", vec![Ty::Str], Ty::result(Ty::Str)),
        (
            "query_encode",
            vec![Ty::Map(Box::new(Ty::Str), Box::new(Ty::Str))],
            Ty::Str,
        ),
        (
            "query_decode",
            vec![Ty::Str],
            Ty::Map(Box::new(Ty::Str), Box::new(Ty::Str)),
        ),
        (
            "url_parse",
            vec![Ty::Str],
            Ty::Map(Box::new(Ty::Str), Box::new(Ty::Str)),
        ),
    ];
    assert_eq!(sig.functions.len(), expected.len(), "std.encoding fn count");
    for (name, params, ret) in &expected {
        let fs = sig
            .functions
            .get(*name)
            .unwrap_or_else(|| panic!("std.encoding missing fn '{name}'"));
        assert_eq!(&fs.params, params, "fn '{name}' params drifted");
        assert_eq!(&fs.ret, ret, "fn '{name}' return drifted");
        assert_eq!(fs.min_params, params.len(), "fn '{name}' arity drifted");
    }
}

// ===== phase 4c-concurrency: std.concurrency is FILE-BACKED (native struct decls in std/concurrency.chz)

/// PROVENANCE — the `"std.concurrency"` arm is DELETED from `native_module_sig`; the whole signature
/// (the four `Shared`/`RwShared`/`Atomic`/`Executor` native structs WITH their harvested method tables)
/// now comes from parsing `std/concurrency.chz`. So `native_module_sig("std.concurrency")` exports
/// NOTHING (empty types + struct_defs), while a full graph check that `import std.concurrency` resolves
/// all four types and their methods. This was the LAST virtual native module — after it,
/// `native_module_sig` retains only the ffi (ptr/width) + time (timer) type-license tails.
#[test]
fn concurrency_sig_from_file_not_native_module_sig() {
    let raw = native_module_sig("std.concurrency");
    assert!(
        raw.types.is_empty(),
        "std.concurrency type names must no longer be hand-built in native_module_sig (harvested from std/concurrency.chz), got: {:?}",
        raw.types
    );
    assert!(
        raw.struct_defs.is_empty(),
        "std.concurrency struct_defs must no longer be hand-built in native_module_sig"
    );
    // A full graph check harvests the four native structs with their method tables.
    let sig = native_module_sig_via_graph("concurrency");
    let expected: &[(&str, &[&str])] = &[
        ("Shared", &["get", "set", "update"]),
        ("RwShared", &["get", "set", "write", "read"]),
        (
            "Atomic",
            &["load", "store", "exchange", "cas", "add", "sub"],
        ),
        (
            "AtomicInt",
            &["load", "store", "exchange", "cas", "add", "sub"],
        ),
        ("Executor", &["submit", "shutdown", "shutdown_now"]),
    ];
    for (tname, methods) in expected {
        let info = sig
            .struct_defs
            .get(*tname)
            .unwrap_or_else(|| panic!("std.concurrency must harvest native struct '{tname}'"));
        for m in *methods {
            assert!(
                info.methods.contains_key(*m),
                "std.concurrency '{tname}' must harvest method '{m}', got: {:?}",
                info.methods.keys().collect::<Vec<_>>()
            );
        }
    }
}

/// METADATA PORT (phase 4c-concurrency) — two method sigs a plain harvested sig cannot express are
/// re-attached in `attach_native_module_metadata`: `RwShared.read`'s closure param becomes `fn(T) -> ?`
/// (any return; R recovered at the call site) and `Executor.submit`'s becomes `fn() -> ?` (any return,
/// zero-arity). The GENERIC `Shared[T]`/`RwShared[T]`/`Atomic[T]` method sigs carry `Ty::Param("T")` so
/// they substitute at each call site (the net-style native-method-binding capability extended to generics).
#[test]
fn concurrency_harvested_method_sigs_shape() {
    let sig = native_module_sig_via_graph("concurrency");
    // Shared[T].set(v: T) -> nil — the param is the box's type param, unsubstituted in the table.
    let shared = sig.struct_defs.get("Shared").expect("Shared");
    let set = shared.methods.get("set").expect("Shared.set");
    assert_eq!(set.params, vec![Ty::Param("T".into())], "Shared.set param");
    assert_eq!(set.ret, Ty::Nil, "Shared.set ret");
    // RwShared.read(f) — metadata retyped the closure param to fn(T) -> ? (any return).
    let rw = sig.struct_defs.get("RwShared").expect("RwShared");
    let read = rw.methods.get("read").expect("RwShared.read");
    match read.params.first() {
        Some(Ty::Func { params, ret, .. }) => {
            assert_eq!(params, &vec![Ty::Param("T".into())], "read closure param");
            assert_eq!(**ret, Ty::Unknown, "read closure return must be ? (any R)");
        }
        other => panic!("RwShared.read param 0 must be fn(T) -> ?, got: {other:?}"),
    }
    // Executor.submit(f) — metadata retyped the closure param to fn() -> ? (zero-arity, any return).
    let ex = sig.struct_defs.get("Executor").expect("Executor");
    let submit = ex.methods.get("submit").expect("Executor.submit");
    match submit.params.first() {
        Some(Ty::Func { params, ret, .. }) => {
            assert!(params.is_empty(), "submit closure must be zero-arity");
            assert_eq!(**ret, Ty::Unknown, "submit closure return must be ? (any)");
        }
        other => panic!("Executor.submit param 0 must be fn() -> ?, got: {other:?}"),
    }
}

/// Phase 4c-followup — native instance methods declare a leading bare `self` (mirroring user structs)
/// that harvest STRIPS: the harvested method-table sig must be byte-identical to the pre-`self`
/// spelling (behavior-preserving). Anti-launder: the stripped `self` must NOT surface as a leading
/// `Ty::Unknown` receiver param.
#[test]
fn native_method_harvest_strips_self() {
    // net.Socket.read(self, n: int, timeout_ms: int = 0) — self stripped ⇒ params == [int, int].
    let net = native_module_sig_via_graph("net");
    let read = net
        .struct_defs
        .get("Socket")
        .expect("Socket")
        .methods
        .get("read")
        .expect("Socket.read");
    assert_eq!(read.params, vec![Ty::Int, Ty::Int], "self must be stripped");
    assert_eq!(
        read.min_params, 1,
        "optional-tail count unaffected by strip"
    );
    assert!(
        !read.params.contains(&Ty::Unknown),
        "stripped self must not surface as a Ty::Unknown receiver"
    );
    // concurrency.Shared.get(self) -> T — self stripped ⇒ zero params, ret is the box's Ty::Param.
    let conc = native_module_sig_via_graph("concurrency");
    let get = conc
        .struct_defs
        .get("Shared")
        .expect("Shared")
        .methods
        .get("get")
        .expect("Shared.get");
    assert!(
        get.params.is_empty(),
        "self-only method harvests to zero params"
    );
    assert_eq!(get.ret, Ty::Param("T".into()), "Shared.get ret == T");
}

/// The harvested table substitutes `T` at each call site (net's capability extended to generics): a
/// wrong-typed `set`/`store` is rejected against the SUBSTITUTED element type, not the raw `Ty::Param`.
#[test]
fn concurrency_methods_resolve_via_harvested_table_with_subst() {
    // Shared[int].set("x") — the param `T` substitutes to `int`, so a str arg is rejected.
    entry_rejects(
        "import std.concurrency\nfn main():\n    s := Shared(0)\n    s.set(\"x\")\nmain()\n",
        "expected int",
    );
    // Shared[str].get() composes as a str (T substitutes to str).
    entry_ok(
        "import std.concurrency\nfn main():\n    s := Shared(\"hi\")\n    print(s.get() + \"!\")\nmain()\n",
    );
    // Atomic add/sub numeric gate residual survives the harvest (str box has no `add`).
    entry_rejects(
        "import std.concurrency\nfn main():\n    a := Atomic(\"x\")\n    a.add(1)\nmain()\n",
        "no method 'add'",
    );
    // Shared[int].update(fn(x): x+1) — the leading `self` is stripped, so the update closure is the
    // only arg the sig expects; it still type-checks after the 4c-followup strip.
    entry_ok(
        "import std.concurrency\nfn main():\n    s := Shared(0)\n    s.update(fn(x): x+1)\nmain()\n",
    );
}

/// Executor.submit accepts a closure with ANY return type (its result is discarded) but STILL rejects a
/// wrong-arity closure — the metadata port types the param `fn() -> ?`, so a 1-arg closure is an error.
#[test]
fn executor_submit_accepts_any_return_rejects_arity() {
    entry_ok(
        "import std.concurrency\nfn main():\n    ex := Executor()\n    ex.submit(fn(): 42)\n    ex.shutdown()\nmain()\n",
    );
    entry_rejects(
        "import std.concurrency\nfn main():\n    ex := Executor()\n    ex.submit(fn(x): x)\nmain()\n",
        "argument 1 of 'submit'",
    );
}

/// EXACT SIGS (phase 4c) — the 59 harvested std.ffi fn sigs must byte-match what the deleted
/// `native_module_sig("std.ffi")` arm used to hand-build (the load_*/store_* for-loops, expanded here).
/// Also asserts the type-license tail (`ptr` + the 8 fixed-width names) survives in `sig.types`.
#[test]
fn ffi_fn_sigs_exact() {
    let sig = native_module_sig_via_graph("ffi");
    // Build the expected sigs the SAME way the old arm did, so this is a byte-for-byte provenance move.
    let mut expected: Vec<(String, Vec<Ty>, Ty)> = vec![
        ("null".to_string(), vec![], Ty::Ptr),
        ("is_null".to_string(), vec![Ty::Ptr], Ty::Bool),
    ];
    for (n, t) in [
        ("load_int", Ty::Int),
        ("load_int8", Ty::Int),
        ("load_int16", Ty::Int),
        ("load_int32", Ty::Int),
        ("load_int64", Ty::Int),
        ("load_uint8", Ty::Int),
        ("load_uint16", Ty::Int),
        ("load_uint32", Ty::Int),
        ("load_uint64", Ty::Int),
        ("load_float", Ty::Float),
        ("load_float32", Ty::Float),
        ("load_bool", Ty::Bool),
        ("load_ptr", Ty::Ptr),
        ("load_str", Ty::Str),
    ] {
        expected.push((n.to_string(), vec![Ty::Ptr], t.clone()));
        expected.push((format!("{n}_at"), vec![Ty::Ptr, Ty::Int], t));
    }
    for (n, v) in [
        ("store_int", Ty::Int),
        ("store_int8", Ty::Int),
        ("store_int16", Ty::Int),
        ("store_int32", Ty::Int),
        ("store_int64", Ty::Int),
        ("store_uint8", Ty::Int),
        ("store_uint16", Ty::Int),
        ("store_uint32", Ty::Int),
        ("store_uint64", Ty::Int),
        ("store_float", Ty::Float),
        ("store_float32", Ty::Float),
        ("store_bool", Ty::Bool),
        ("store_ptr", Ty::Ptr),
    ] {
        expected.push((n.to_string(), vec![Ty::Ptr, v.clone()], Ty::Nil));
        expected.push((format!("{n}_at"), vec![Ty::Ptr, Ty::Int, v], Ty::Nil));
    }
    expected.push(("alloc".to_string(), vec![Ty::Int], Ty::Ptr));
    expected.push(("alloc_zeroed".to_string(), vec![Ty::Int], Ty::Ptr));
    expected.push(("free".to_string(), vec![Ty::Ptr], Ty::Nil));

    assert_eq!(expected.len(), 59, "expected exactly 59 std.ffi fns");
    assert_eq!(
        sig.functions.len(),
        59,
        "std.ffi must harvest exactly 59 native fns from std/ffi.chz"
    );
    for (name, params, ret) in &expected {
        let fs = sig
            .functions
            .get(name)
            .unwrap_or_else(|| panic!("std.ffi missing fn '{name}'"));
        assert_eq!(&fs.params, params, "fn '{name}' params drifted");
        assert_eq!(&fs.ret, ret, "fn '{name}' return drifted");
        assert_eq!(fs.min_params, params.len(), "fn '{name}' arity drifted");
    }
    // The type-license tail is UNCHANGED by the migration: `ptr` + the 8 fixed-width names survive.
    assert!(
        sig.types.contains("ptr"),
        "std.ffi must keep `ptr` licensed"
    );
    for tn in crate::native::ffi::TYPE_NAMES {
        assert!(
            sig.types.contains(*tn),
            "std.ffi must keep the fixed-width type name `{tn}` licensed"
        );
    }
    // The runtime member table cross-checks the harvested surface 1:1.
    assert_eq!(
        crate::native::native_members("std.ffi").len(),
        59,
        "std.ffi runtime MEMBERS must stay 59 (dispatch untouched)"
    );
}

#[test]
fn crypto_fn_sigs_exact() {
    let sig = native_module_sig_via_graph("crypto");
    let expected: Vec<(&str, Vec<Ty>, Ty)> = vec![
        ("sha256", vec![Ty::Str], Ty::Str),
        ("sha256_bytes", vec![Ty::Bytes], Ty::Str),
        ("sha1", vec![Ty::Str], Ty::Str),
        ("sha1_bytes", vec![Ty::Bytes], Ty::Str),
        ("sha512", vec![Ty::Str], Ty::Str),
        ("sha512_bytes", vec![Ty::Bytes], Ty::Str),
        ("md5", vec![Ty::Str], Ty::Str),
        ("hmac_sha256", vec![Ty::Bytes, Ty::Bytes], Ty::Str),
        ("secure_bytes", vec![Ty::Int], Ty::Bytes),
        ("token_hex", vec![Ty::Int], Ty::Str),
    ];
    assert_eq!(sig.functions.len(), expected.len(), "std.crypto fn count");
    for (name, params, ret) in &expected {
        let fs = sig
            .functions
            .get(*name)
            .unwrap_or_else(|| panic!("std.crypto missing fn '{name}'"));
        assert_eq!(&fs.params, params, "fn '{name}' params drifted");
        assert_eq!(&fs.ret, ret, "fn '{name}' return drifted");
    }
}

#[test]
fn uuid_fn_sigs_exact() {
    let sig = native_module_sig_via_graph("uuid");
    let expected: Vec<(&str, Vec<Ty>, Ty)> = vec![
        ("v4", vec![], Ty::Str),
        ("uuid_seed", vec![Ty::Int], Ty::Nil),
    ];
    assert_eq!(sig.functions.len(), expected.len(), "std.uuid fn count");
    for (name, params, ret) in &expected {
        let fs = sig
            .functions
            .get(*name)
            .unwrap_or_else(|| panic!("std.uuid missing fn '{name}'"));
        assert_eq!(&fs.params, params, "fn '{name}' params drifted");
        assert_eq!(&fs.ret, ret, "fn '{name}' return drifted");
    }
}

#[test]
fn time_fn_sigs_exact() {
    let sig = native_module_sig_via_graph("time");
    let expected: Vec<(&str, Vec<Ty>, Ty)> = vec![
        ("now", vec![], Ty::Int),
        ("monotonic", vec![], Ty::Float),
        ("sleep_ms", vec![Ty::Int], Ty::Nil),
        ("format", vec![Ty::Int], Ty::Str),
    ];
    assert_eq!(
        sig.functions.len(),
        expected.len(),
        "std.time must export exactly the 4 native fns (timer is NOT a native fn)"
    );
    for (name, params, ret) in &expected {
        let fs = sig
            .functions
            .get(*name)
            .unwrap_or_else(|| panic!("std.time missing fn '{name}'"));
        assert_eq!(&fs.params, params, "fn '{name}' params drifted");
        assert_eq!(&fs.ret, ret, "fn '{name}' return drifted");
    }
    // `timer` stays licensed via sig.types (opcode-backed, NOT a native fn value).
    assert!(
        sig.types.contains("timer"),
        "std.time module sig must still license `timer`"
    );
    assert!(
        !sig.functions.contains_key("timer"),
        "`timer` must NOT be a native fn (it has no runtime member value)"
    );
}

/// The `timer` opcode-license survives the arm reduction — both the whole-module `import std.time` and
/// the selective `import timer from std.time` forms still license the bare `timer(ms)` call.
#[test]
fn import_timer_from_std_time_still_licensed_both_forms() {
    entry_ok("import std.time\nfn main():\n    print(timer(20).recv())\nmain()\n");
    entry_ok("import timer from std.time\nfn main():\n    print(timer(20).recv())\nmain()\n");
}

/// A `native fn` decl in a USER (non-stdlib) .chz file is still a clear error — the file-backed std
/// files take the native arm (bypassing check_module's guard), user files do not (phase-4e regression
/// guard, complementing `native_decl_in_user_file_rejected`).
#[test]
fn phase4e_user_file_native_fn_still_rejected() {
    entry_rejects(
        "native fn sha256(s: str) -> str\nfn main():\n    print(1)\nmain()\n",
        "native fn/ctor declarations are only allowed in standard-library modules",
    );
}

#[test]
fn native_time_now_is_int_monotonic_is_float() {
    entry_ok(
        "import std.time\nfn main():\n    t: int = time.now()\n    m: float = time.monotonic()\n    time.sleep_ms(0)\n    s: str = time.format(t)\n    print(s)\n",
    );
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
    entry_ok(
        "import std.json\nstruct P:\n    x: int\n    y: int\nfn main():\n    match json.decode[P](\"x\"):\n        Ok(p): print(str(p.x))\n        Err(e): print(e)\n",
    );
}

#[test]
fn json_decode_into_typed_map_and_list() {
    entry_ok(
        "import std.json\nfn main():\n    a := json.decode[Map[str, int]](\"x\")\n    b := json.decode[List[float]](\"y\")\n    print(\"ok\")\n",
    );
}

#[test]
fn json_decode_scalar_result_type_flows() {
    entry_ok(
        "import std.json\nfn main():\n    match json.decode[int](\"3\"):\n        Ok(n): print(str(n + 1))\n        Err(e): print(e)\n",
    );
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
        "import std.json\nfn main():\n    x := json.decode[Map[int, int]](\"x\")\n",
        "map keys must be str",
    );
}

// ===== M8-M4: set type =====

#[test]
fn set_literal_infers_set_of_elem() {
    ok("s: Set[int] = {1, 2, 3}\nprint(s.len())\n");
}

#[test]
fn set_methods_typecheck() {
    ok(
        "s := {1, 2}\nb: bool = s.has(1)\ns.add(3)\nr: bool = s.remove(1)\nu: Set[int] = s.union({4})\nprint(u.len())\n",
    );
}

#[test]
fn set_builtin_empty_and_from_list() {
    ok("e := Set()\ne.add(\"x\")\nf: Set[int] = Set([1, 1, 2])\nprint(f.len())\n");
}

#[test]
fn set_iteration_binds_element() {
    ok("for x in {1, 2, 3}:\n    y: int = x\n    print(y)\n");
}

#[test]
fn set_mixed_element_types_rejected() {
    rejects("s := {1, \"two\"}\n", "set elements differ");
}

#[test]
fn set_non_hashable_element_rejected() {
    rejects("s := {[1], [2]}\n", "must implement Hashable");
}

#[test]
fn set_union_arg_must_be_set() {
    rejects("s := {1, 2}\nx := s.union([3])\n", "argument 1 of 'union'");
}

#[test]
fn set_not_indexable() {
    rejects("s := {1, 2}\nx := s[0]\n", "cannot index into Set");
}

// ===== Go-style Result[T, E] + Error protocol (M11 Phase A) =====

#[test]
fn result_two_type_params_ok() {
    ok("fn q() -> Result[int, str]:\n    return Err(\"bad\")\n");
}

#[test]
fn bang_shorthand_with_error_type_ok() {
    // `T!E` == `Result[T, E]`.
    ok("fn q() -> int!str:\n    return Err(\"bad\")\n");
}

#[test]
fn err_payload_typed_as_concrete_err() {
    // When `E` is `str`, the bound `Err` payload is a `str` — str methods available.
    ok(
        "fn q() -> Result[int, str]:\n    return Err(\"bad\")\nfn main():\n    match q():\n        Ok(v): print(v)\n        Err(e): print(e.trim())\nmain()\n",
    );
}

#[test]
fn custom_struct_error_ok() {
    ok(
        "struct DbErr:\n    code: int\n    fn message(self) -> str:\n        return \"db\"\nfn q() -> int!DbErr:\n    return Err(DbErr(503))\n",
    );
}

#[test]
fn error_protocol_existential_accepts_str() {
    // `Error` used as a value type; `str` conforms; only `message()` is available on it.
    ok(
        "fn q() -> Result[int, Error]:\n    return Err(\"bad\")\nfn main():\n    match q():\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()\n",
    );
}

#[test]
fn bang_default_error_is_error_protocol() {
    // `T!` defaults `E` to the `Error` protocol; the payload supports `.message()`.
    ok(
        "fn q() -> int!:\n    return Err(\"bad\")\nfn main():\n    match q():\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()\n",
    );
}

#[test]
fn default_error_existential_rejects_str_methods() {
    // `Error` existential exposes only `message()` — not `str`'s methods.
    rejects(
        "fn q() -> int!:\n    return Err(\"x\")\nfn main():\n    match q():\n        Ok(v): print(v)\n        Err(e): print(e.trim())\nmain()\n",
        "trim",
    );
}

#[test]
fn struct_error_without_message_rejected_as_error() {
    // A struct lacking `message(self) -> str` does not satisfy `Error`, so it can't be the
    // payload where `Error` is expected — the return-type check flags the mismatch.
    rejects(
        "struct Bad:\n    n: int\nfn q() -> Result[int, Error]:\n    return Err(Bad(1))\n",
        "Bad",
    );
}

// ===== recover: boundary (M11 Phase B) =====

#[test]
fn recover_yields_result_of_block_value() {
    // `recover:` evaluates to Result[T, Error]; matching Ok/Err is well-typed.
    ok(
        "fn main():\n    r := recover:\n        [1, 2][0]\n    match r:\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()\n",
    );
}

#[test]
fn recover_value_composes_with_question_mark() {
    // The recover result is an ordinary Result, usable with `?`.
    ok("fn run() -> int!:\n    r := recover:\n        99\n    v := r?\n    return Ok(v)\n");
}

#[test]
fn recover_block_rejects_return() {
    rejects(
        "fn f() -> int!:\n    r := recover:\n        return Ok(1)\n    return r\n",
        "'return' is not allowed inside a recover block",
    );
}

#[test]
fn recover_block_rejects_escaping_break() {
    rejects(
        "fn main():\n    for i in 0..3:\n        r := recover:\n            break\n        print(r)\nmain()\n",
        "'break' is not allowed inside a recover block",
    );
}

#[test]
fn recover_allows_inner_loop_break() {
    // A break that targets a loop *inside* the recover block is fine.
    ok(
        "fn main():\n    r := recover:\n        for i in 0..3:\n            if i == 1: break\n        42\n    match r:\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()\n",
    );
}

#[test]
fn recover_question_mark_allowed_in_non_result_fn() {
    // `?` targets the recover boundary, so the enclosing fn need not return Result.
    ok(
        "fn risky() -> int!:\n    return Err(\"x\")\nfn compute() -> str:\n    r := recover:\n        v := risky()?\n        v\n    match r:\n        Ok(v): return \"ok\"\n        Err(e): return e.message()\n",
    );
}

#[test]
fn recover_question_mark_on_option_rejected() {
    rejects(
        "fn find() -> int?:\n    return None\nfn main():\n    r := recover:\n        v := find()?\n        v\n    print(r)\nmain()\n",
        "Option is not allowed inside a recover block",
    );
}

// A `recover:` whose tail statement provably diverges (here a statement-form `match` whose every
// arm `panic`s) yields no normal value, so its `Ok` payload is bottom (`Unknown`), not `nil` — and
// `Ok(v)`'s `v` must be usable in value position (e.g. interpolated), exactly like a direct
// `recover: panic(...)`. Pins the Never/bottom-payload inconsistency fix.
#[test]
fn recover_diverging_match_tail_payload_is_bottom() {
    entry_ok(
        "fn main():\n    r := recover:\n        match 1:\n            _: panic(\"boom\")\n    match r:\n        Ok(v): print(\"got {v}\")\n        Err(e): print(\"err\")\nmain()\n",
    );
}

// The consistency invariant the bug violated: a direct-panic recover and a match-all-panic recover
// must both accept (both have a bottom `Ok` payload). Pre-fix the second was rejected at (1,1).
#[test]
fn recover_payload_consistent_direct_vs_match_panic() {
    // r1: direct panic tail (already accepted pre-fix).
    entry_ok(
        "fn main():\n    r := recover:\n        panic(\"x\")\n    match r:\n        Ok(v): print(\"got {v}\")\n        Err(e): print(\"err\")\nmain()\n",
    );
    // r2: panic reached through an extra statement-form match layer (rejected pre-fix).
    entry_ok(
        "fn main():\n    r := recover:\n        match 1:\n            _: panic(\"boom\")\n    match r:\n        Ok(v): print(\"got {v}\")\n        Err(e): print(\"err\")\nmain()\n",
    );
}

// Regression fence: a concrete-tail recover keeps its concrete payload (the divergence upgrade only
// fires on a provably-diverging nil tail), and a non-diverging statement tail (a `let`) still yields
// `Result[nil]` so its `Ok(v)` is correctly nil-banned in value position.
#[test]
fn recover_non_never_value_unaffected() {
    // Concrete value tail -> Ok payload is `int`, interpolation accepts.
    entry_ok(
        "fn main():\n    r := recover:\n        5\n    match r:\n        Ok(v): print(\"v={v}\")\n        Err(e): print(\"err\")\nmain()\n",
    );
    // Non-diverging statement tail (a `let`) -> Ok payload stays nil -> value use rejected.
    entry_rejects(
        "fn main():\n    r := recover:\n        x := 5\n    match r:\n        Ok(v): print(\"{v}\")\n        Err(e): print(\"err\")\nmain()\n",
        "expression returns no value (nil) and cannot be used as a value",
    );
}

// A `recover:` whose TAIL is a statement-form `match` with value-producing arms is typed by the
// unified arm type (`Result[int]` here), not `Result[nil]` — so binding `Ok(v)` gives `v: int` and
// using it as a value (interpolation) is accepted instead of nil-banned. (Result has no `is_ok()`
// method in Chezzi — it is consumed via `match`; the task's `r.is_ok()` was just illustrating that
// the block type was wrong. The sound observable is that `v` is the arm value type, not nil.)
#[test]
fn recover_tail_stmt_match_value_is_result_of_arm_type() {
    entry_ok(
        "fn main():\n    r := recover:\n        x := 3\n        match x:\n            3: 100\n            _: 200\n    match r:\n        Ok(v): print(\"v={v}\")\n        Err(e): print(\"err\")\nmain()\n",
    );
}

// A `recover:` whose TAIL is a statement-form `if/else` with value-producing branches is typed by
// the unified branch type — same as the trailing-`match` analog.
#[test]
fn recover_tail_stmt_if_value_is_result_of_branch_type() {
    entry_ok(
        "fn main():\n    r := recover:\n        x := 3\n        if x == 3:\n            100\n        else:\n            200\n    match r:\n        Ok(v): print(\"v={v}\")\n        Err(e): print(\"err\")\nmain()\n",
    );
    // A trailing `if` WITHOUT an `else` is not total -> stays `Result[nil]` (value use rejected).
    entry_rejects(
        "fn main():\n    r := recover:\n        x := 3\n        if x == 3:\n            100\n    match r:\n        Ok(v): print(\"{v}\")\n        Err(e): print(\"err\")\nmain()\n",
        "expression returns no value (nil) and cannot be used as a value",
    );
}

// REGRESSION GUARD: a `recover:` tail `match`/`if` whose arms are genuinely HETEROGENEOUS (a
// `str` arm and an `int` arm, or a void `print(...)` arm mixed with a value arm) has no single value
// type. It must FALL BACK to `Result[nil]` (value dropped, consumed only via `Ok(_)`), NOT be rejected
// with "branches have incompatible types" — which was the over-strict regression of the first cut of
// this feature (previously-valid fault-isolation `recover:`s stopped compiling).
#[test]
fn recover_tail_stmt_match_heterogeneous_arms_falls_back_to_nil() {
    // str vs int arms — value ignored (`Ok(_)`): accepted, typed `Result[nil]`, no incompat error.
    entry_ok(
        "fn foo(cmd: str):\n    r := recover:\n        match cmd:\n            \"a\": \"hello\"\n            _: 42\n    match r:\n        Ok(_): print(\"done\")\n        Err(e): print(\"failed\")\nfoo(\"a\")\n",
    );
    // void-call arm (nil) mixed with an int arm — same fall-back to `Result[nil]`.
    entry_ok(
        "fn logit():\n    print(\"log\")\nfn foo(cmd: str):\n    r := recover:\n        match cmd:\n            \"log\": logit()\n            _: 42\n    match r:\n        Ok(_): print(\"done\")\n        Err(e): print(\"failed\")\nfoo(\"log\")\n",
    );
    // Because the block is `Result[nil]`, binding the value and USING it is still nil-banned (proves
    // the fall-back really is nil, so the heterogeneous runtime payload is never observable).
    entry_rejects(
        "fn foo(cmd: str):\n    r := recover:\n        match cmd:\n            \"a\": \"hello\"\n            _: 42\n    match r:\n        Ok(v): print(v)\n        Err(e): print(\"failed\")\nfoo(\"a\")\n",
        "expression returns no value (nil) and cannot be used as a value",
    );
}

// REGRESSION GUARD (if analog): heterogeneous `if/else` branches fall back to `Result[nil]`, not
// rejected.
#[test]
fn recover_tail_stmt_if_heterogeneous_branches_falls_back_to_nil() {
    entry_ok(
        "fn foo(n: int):\n    r := recover:\n        if n == 0:\n            \"zero\"\n        else:\n            n\n    match r:\n        Ok(_): print(\"done\")\n        Err(e): print(\"failed\")\nfoo(0)\n",
    );
}

// A void-call fragment inside an interpolated string is a real nil-in-value-position error, but the
// span must point at the string literal (the print call line), never the fallback (1,1).
#[test]
fn interpolation_void_fragment_error_span_is_not_one_one() {
    let errs =
        check_entry("fn f():\n    print(\"hi\")\nfn main():\n    print(\"got {f()}\")\nmain()\n");
    let nil_errs: Vec<_> = errs
        .iter()
        .filter(|e| e.message.contains("cannot be used as a value"))
        .collect();
    assert_eq!(
        nil_errs.len(),
        1,
        "expected exactly one nil error, got: {errs:?}"
    );
    let span = nil_errs[0].span;
    assert_eq!(
        span.line, 4,
        "nil-fragment error should point at the print line, got: {span}"
    );
    assert_ne!(
        (span.line, span.col),
        (1, 1),
        "span must not be the (1,1) fallback"
    );
}

// ===== `?` inside a closure is checked against the closure's return (soundness fix) =====

#[test]
fn closure_question_mark_on_nonresult_return_rejected() {
    // A closure declared `-> int` may not use `?` — it would leak an Err into a List[int].
    rejects(
        "fn parse(s: str) -> int!:\n    return Err(\"x\")\nfn main():\n    ys := [\"2\"].map(fn(s: str) -> int: parse(s)? * 2)\n    print(ys)\nmain()\n",
        "not Result or Option",
    );
}

#[test]
fn closure_question_mark_on_result_return_ok() {
    // A closure declared to return Result may use `?` (yields the Ok type).
    ok(
        "fn parse(s: str) -> int!:\n    return Ok(2)\nfn main():\n    rs := [\"2\"].map(fn(s: str) -> int!: Ok(parse(s)? * 2))\n    print(rs)\nmain()\n",
    );
}

#[test]
fn closure_question_mark_inferred_return_rejected() {
    // No return annotation → `?` has no Result/Option context → rejected (annotate to allow).
    rejects(
        "fn parse(s: str) -> int!:\n    return Err(\"x\")\nfn main():\n    ys := [\"2\"].map(fn(s): parse(s)?)\n    print(ys)\nmain()\n",
        "Result or Option",
    );
}

// ===== struct iterator protocol: `for x in s` when `s` has `next(self) -> Option[T]` =====

#[test]
fn iterates_struct_with_next_ok() {
    // A struct with `next(self) -> Option[int]` is iterable; `x` binds the element type (int).
    ok(
        "struct Counter:\n    n: int\n    limit: int\n    fn next(self) -> Option[int]:\n        if self.n >= self.limit:\n            return None\n        v := self.n\n        self.n = self.n + 1\n        return Some(v)\nfn main():\n    for x in Counter(0, 5):\n        print(x)\nmain()\n",
    );
}

#[test]
fn struct_iter_two_vars_rejected() {
    // A struct iterator binds exactly one loop variable (no key/value form).
    rejects(
        "struct Counter:\n    n: int\n    limit: int\n    fn next(self) -> Option[int]:\n        if self.n >= self.limit:\n            return None\n        v := self.n\n        self.n = self.n + 1\n        return Some(v)\nfn main():\n    for k, v in Counter(0, 5):\n        print(k)\nmain()\n",
        "single loop variable",
    );
}

#[test]
fn struct_without_next_not_iterable_still_errors() {
    // A struct lacking `next` is not iterable — the original "cannot iterate" error stands.
    rejects(
        "struct Point:\n    x: int\n    y: int\nfn main():\n    for p in Point(1, 2):\n        print(p)\nmain()\n",
        "cannot iterate over",
    );
}

#[test]
fn struct_iter_binds_element_type() {
    // The bound element is `int`, so using it as a str (`x + \"s\"`) is a type error.
    rejects(
        "struct Counter:\n    n: int\n    limit: int\n    fn next(self) -> Option[int]:\n        if self.n >= self.limit:\n            return None\n        v := self.n\n        self.n = self.n + 1\n        return Some(v)\nfn main():\n    for x in Counter(0, 5):\n        print(x + \"s\")\nmain()\n",
        "cannot apply + to int and str",
    );
}

// ===== match guards (pattern if cond) =====

#[test]
fn match_guard_non_bool_rejected() {
    // A guard expression must be bool; an int guard is a type error.
    rejects(
        "n := 3\nmatch n:\n    x if x: print(\"y\")\n    _: print(\"n\")\n",
        "must be bool",
    );
}

#[test]
fn match_guard_str_rejected() {
    rejects(
        "n := 3\nmatch n:\n    x if \"hi\": print(\"y\")\n    _: print(\"n\")\n",
        "must be bool",
    );
}

#[test]
fn guarded_wildcard_does_not_make_exhaustive() {
    // The only catch-all is `_ if cond:` — guarded, so refutable; the match is non-exhaustive.
    rejects(
        "n := 3\nmatch n:\n    0: print(\"z\")\n    _ if n > 0: print(\"pos\")\n",
        "non-exhaustive",
    );
}

#[test]
fn guarded_binding_does_not_make_exhaustive() {
    rejects(
        "n := 3\nmatch n:\n    x if x > 0: print(\"pos\")\n",
        "non-exhaustive",
    );
}

#[test]
fn guarded_variant_arm_does_not_close_variant() {
    // A guarded variant arm (`E.A if false`) is refutable — it can't close variant A.
    rejects(
        "enum E:\n    A\n    B\nfn f(e: E) -> int:\n    match e:\n        E.A if false: return 1\n        E.B: return 2\nf(E.A)\n",
        "non-exhaustive match on E: missing A",
    );
}

#[test]
fn refutable_literal_payload_does_not_close_variant() {
    // `Some(0)` only covers the value 0, not every `Some(n)` — Some stays open.
    rejects(
        "fn f(x: Option[int]) -> str:\n    match x:\n        None: return \"none\"\n        Some(0): return \"zero\"\nf(Some(5))\n",
        "non-exhaustive",
    );
}

#[test]
fn refutable_literal_in_multifield_variant_does_not_close() {
    // A literal sub-pattern in a multi-field variant payload keeps the variant open.
    rejects(
        "enum P:\n    Pair(int, int)\nfn f(p: P) -> str:\n    match p:\n        P.Pair(0, y): return \"zero-x\"\nf(P.Pair(1, 2))\n",
        "non-exhaustive match on P: missing Pair",
    );
}

#[test]
fn guarded_variant_then_fallback_accepted() {
    // A guarded variant arm followed by an unguarded fallback on the same variant is the
    // standard idiom and must be accepted (the guarded arm doesn't close the variant).
    ok(
        "enum E:\n    A(int)\n    B\nfn f(e: E) -> str:\n    match e:\n        E.A(n) if n > 0: return \"pos\"\n        E.A(n): return \"nonpos\"\n        E.B: return \"b\"\nf(E.A(1))\n",
    );
}

#[test]
fn unguarded_variant_duplicate_still_rejected() {
    // A genuine duplicate (a prior unguarded+irrefutable arm already closed A) still fires.
    rejects(
        "enum E:\n    A(int)\n    B\nfn f(e: E) -> str:\n    match e:\n        E.A(n): return \"a\"\n        E.A(m): return \"a2\"\n        E.B: return \"b\"\nf(E.A(1))\n",
        "duplicate match arm",
    );
}

#[test]
fn nested_single_variant_payload_is_exhaustive() {
    // A nested single-variant enum pattern covers its whole domain, so `Outer.Wrap(Inner.Only(x))`
    // is irrefutable and closes Wrap — a single-variant Outer match is exhaustive (no `_` needed).
    // Regression guard: the exhaustiveness fix must not over-reject genuinely-total nested matches.
    ok(
        "enum Inner:\n    Only(int)\nenum Outer:\n    Wrap(Inner)\nfn f(o: Outer) -> int:\n    match o:\n        Outer.Wrap(Inner.Only(x)): return x\nf(Outer.Wrap(Inner.Only(5)))\n",
    );
}

#[test]
fn nested_multivariant_payload_stays_refutable() {
    // `Some(Some(v))` does not cover `Some(None)` — the inner Option has 2 variants, so the nested
    // pattern is refutable and Some stays open.
    rejects(
        "fn f(x: Option[Option[int]]) -> int:\n    match x:\n        None: return -1\n        Some(Some(v)): return v\nf(Some(None))\n",
        "non-exhaustive",
    );
}

#[test]
fn match_guard_ok() {
    // A guard sees the pattern's bindings; with a trailing `_` the match is exhaustive.
    ok(
        "fn classify(n: int) -> str:\n    return match n:\n        x if x < 0: \"neg\"\n        0: \"zero\"\n        _: \"pos\"\nclassify(1)\n",
    );
}

#[test]
fn match_guard_stmt_ok() {
    ok("n := 5\nmatch n:\n    x if x > 0: print(\"pos\")\n    _: print(\"other\")\n");
}

#[test]
fn bare_ident_binding_colliding_with_variant_rejected() {
    // `None` is a registered variant; binding it against an int would bind in the interp but trap
    // on the VM (the compiler routes by the variant registry). Reject so all engines agree.
    rejects(
        "match 5:\n    None: print(\"bound\")\n",
        "is a variant name",
    );
}

// ===== integer range patterns (start..end) =====

#[test]
fn range_pattern_on_str_rejected() {
    rejects(
        "s := \"x\"\nmatch s:\n    0..10: print(\"lo\")\n    _: print(\"hi\")\n",
        "range pattern",
    );
}

#[test]
fn range_pattern_non_exhaustive_without_wildcard() {
    // Ranges are refutable and never close the int domain — a `_` is still required.
    rejects(
        "n := 3\nmatch n:\n    0..10: print(\"lo\")\n    10..20: print(\"hi\")\n",
        "non-exhaustive",
    );
}

#[test]
fn range_pattern_ok() {
    ok(
        "fn grade(n: int) -> str:\n    return match n:\n        0..60: \"F\"\n        60..90: \"B\"\n        _: \"A\"\ngrade(50)\n",
    );
}

#[test]
fn range_three_arg_typechecks() {
    // 1/2/3-arg `range()` of ints all type-check to `List[int]`; 0 or >3 args reject.
    ok("fn main():\n    a: List[int] = range(0, 10, 2)\n    print(a.len())\nmain()\n");
    ok("fn main():\n    a: List[int] = range(10, 0, -1)\n    print(a.len())\nmain()\n");
    rejects("fn main():\n    print(range())\nmain()\n", "range");
    rejects(
        "fn main():\n    print(range(0, 1, 2, 3))\nmain()\n",
        "range",
    );
    rejects(
        "fn main():\n    print(range(0, 10, \"x\"))\nmain()\n",
        "int",
    );
}

#[test]
fn range_slice_typechecks() {
    // Slicing a range literal infers `List[int]` (the SECONDARY range-slicing path).
    ok("fn main():\n    a: List[int] = (0..10)[::2]\n    print(a.len())\nmain()\n");
}

// ===== default + named arguments (end-to-end through desugar) =====

#[test]
fn default_arg_typechecks_ok() {
    // The omitted `y` is filled with its default (10:int) before checking.
    entry_ok(
        "fn f(x: int, y: int = 10) -> int:\n    return x + y\nfn main():\n    print(f(1))\nmain()\n",
    );
}

#[test]
fn named_arg_type_mismatch_rejected() {
    // `f(1, y="bad")` desugars to `f(1, "bad")`; arg 2 must fail int vs str.
    entry_rejects(
        "fn f(x: int, y: int):\n    print(x)\nfn main():\n    f(1, y=\"bad\")\nmain()\n",
        "argument 2 of 'f'",
    );
}

#[test]
fn default_arg_type_mismatch_rejected() {
    // A defaulted value of the wrong type is still checked against the param type when filled in.
    entry_rejects(
        "fn f(x: int, y: int = 10):\n    print(x)\nfn main():\n    f(\"oops\")\nmain()\n",
        "argument 1 of 'f'",
    );
}

#[test]
fn named_struct_field_type_mismatch_rejected() {
    entry_rejects(
        "struct P:\n    x: int\n    y: int = 0\nfn main():\n    p := P(x=\"bad\")\n    print(p.x)\nmain()\n",
        "expected int",
    );
}

#[test]
fn wrong_typed_param_default_rejected_even_when_overridden() {
    // `y: int = true` is invalid at the declaration even though every call passes `y`.
    entry_rejects(
        "fn f(x: int, y: int = true) -> int:\n    return x\nfn main():\n    print(f(1, 2))\nmain()\n",
        "default value for parameter 'y'",
    );
}

#[test]
fn wrong_typed_field_default_rejected() {
    entry_rejects(
        "struct P:\n    x: int\n    y: int = \"no\"\nfn main():\n    print(0)\nmain()\n",
        "default value for field 'y'",
    );
}

#[test]
fn valid_param_default_ok() {
    entry_ok(
        "fn f(x: int, y: int = 7, s: str = \"hi\") -> int:\n    return x + y\nfn main():\n    print(f(1))\nmain()\n",
    );
}

// ===== Iterator[T] — the parameterized protocol bound =====

// A struct iterator used as a conformance witness across the tests below.
const COUNTER: &str = "\
struct Counter:
    n: int
    fn next(self) -> int?:
        if self.n <= 0:
            return None
        self.n -= 1
        return Some(self.n)
";

#[test]
fn iterator_bound_over_list_ok() {
    // `[S: Iterator[T], T]` accepts a list and recovers its element type.
    ok(
        "fn first[S: Iterator[T], T](xs: S, d: T) -> T:\n    for x in xs:\n        return x\n    return d\nv := first([1, 2, 3], 0)\n",
    );
}

#[test]
fn iterator_bound_recovers_element_type() {
    // first([1,2,3], 0) is int, so binding the result to a str must be rejected — proves T = int
    // flowed out of the iterand's element type, not stayed erased.
    rejects(
        "fn first[S: Iterator[T], T](xs: S, d: T) -> T:\n    for x in xs:\n        return x\n    return d\ns: str = first([1, 2, 3], 0)\n",
        "cannot assign",
    );
}

#[test]
fn iterator_bound_over_noniterable_rejected() {
    rejects(
        "fn first[S: Iterator[T], T](xs: S, d: T) -> T:\n    return d\nv := first(5, 0)\n",
        "does not satisfy Iterator",
    );
}

#[test]
fn iterator_bound_over_user_struct_ok() {
    let src = format!(
        "{COUNTER}fn first[S: Iterator[T], T](xs: S, d: T) -> T:\n    for x in xs:\n        return x\n    return d\nv := first(Counter(3), 0)\n"
    );
    ok(&src);
}

#[test]
fn iterator_loop_var_typed_as_element() {
    // Inside the generic body the loop var must be the element type `T` (a param), NOT `Unknown`:
    // assigning it to an `int` slot is rejected at definition time. If it were `Unknown` this would
    // type-check (Unknown is assignable to anything), so this discriminates element-typing.
    rejects(
        "fn f[S: Iterator[T], T](xs: S):\n    for x in xs:\n        y: int = x\n        print(y)\n",
        "cannot assign",
    );
}

#[test]
fn iterator_protocol_redeclaration_rejected() {
    rejects(
        "protocol Iterator:\n    fn next(self) -> int?\n",
        "reserved",
    );
}

#[test]
fn iterator_bound_wrong_arity_rejected() {
    rejects(
        "fn f[S: Iterator](xs: S):\n    print(1)\n",
        "Iterator' takes 1 type argument",
    );
}

#[test]
fn nonparam_protocol_with_args_rejected() {
    rejects(
        "fn f[T: Comparable[int]](a: T) -> T:\n    return a\n",
        "takes no type arguments",
    );
}

#[test]
fn iterator_adapter_struct_ok() {
    // A `Take` adapter wraps an inner iterator bounded `I: Iterator[T]`. Inside `next`,
    // `self.inner.next()` must yield `T?` (the element), so `return Some(x)` type-checks against
    // the declared `T?` return. This proves `.next()` on a bounded param recovers the element type.
    let src = "\
struct Take[I: Iterator[T], T]:
    inner: I
    left: int
    fn next(self) -> T?:
        if self.left <= 0:
            return None
        self.left -= 1
        return self.inner.next()
fn main():
    t := Take([1, 2, 3], 2)
    for x in t:
        print(x)
main()
";
    entry_ok(src);
}

#[test]
fn iterator_bound_forwards_into_another_iterator_call_ok() {
    // A `[S: Iterator[T]]` value must satisfy `Iterator` when forwarded into another iterator-generic
    // (the `Ty::Param` declared-bounds path), not be rejected. Regression for the satisfies/for-loop
    // drift.
    ok(
        "fn count[S: Iterator[T], T](xs: S) -> int:\n    n := 0\n    for _ in xs:\n        n = n + 1\n    return n\nfn wrap[S: Iterator[T], T](xs: S) -> int:\n    return count(xs)\nv := wrap([1, 2, 3])\n",
    );
}

#[test]
fn iterator_conflicting_explicit_element_arg_rejected() {
    // Explicit `[List[int], str]` pins T=str, but the list element is int — the recovered element
    // must conflict (unsound otherwise: static List[str], runtime List[int]).
    rejects(
        "fn to_list[S: Iterator[T], T](xs: S) -> List[T]:\n    out := []\n    for x in xs:\n        out.push(x)\n    return out\nr := to_list[List[int], str]([1, 2, 3])\n",
        "does not match the declared element type",
    );
}

#[test]
fn iterator_bound_unknown_element_type_rejected() {
    // `Bogus` is neither a declared type param nor a known type — a bound's args are resolved, so
    // this is reported rather than silently accepted.
    rejects("fn f[S: Iterator[Bogus]](xs: S):\n    print(1)\n", "Bogus");
}

#[test]
fn iterator_adapter_element_mismatch_rejected() {
    // `self.inner.next()` is `T?`; returning a literal `Some(\"x\")` (str) where the declared return
    // is `T?` is caught once `T` is pinned — guards the element typing of `.next()`.
    let src = "\
struct Bad[I: Iterator[T], T]:
    inner: I
    fn next(self) -> T?:
        return Some(\"x\")
fn main():
    b := Bad([1, 2, 3])
    for x in b:
        print(x)
main()
";
    rejects(src, "expected return type Option[T], found Option[str]");
}

// ===== slicing + Index/IndexSet/Slice protocols =====

#[test]
fn slice_of_list_types_as_list() {
    ok("xs := [1, 2, 3, 4]\nys: List[int] = xs[1:3]\n");
    // Optional bounds / step still type as the same container type.
    ok("xs := [1, 2, 3, 4]\nys: List[int] = xs[1:]\n");
    ok("xs := [1, 2, 3, 4]\nys: List[int] = xs[:3]\n");
    ok("xs := [1, 2, 3, 4]\nys: List[int] = xs[::2]\n");
    rejects(
        "xs := [1, 2, 3, 4]\nys: str = xs[1:3]\n",
        "cannot assign List[int] to variable of type str",
    );
}

#[test]
fn slice_of_str_types_as_str() {
    ok("s := \"hello\"\nt: str = s[0:2]\n");
    rejects(
        "s := \"hello\"\nn: int = s[0:2]\n",
        "cannot assign str to variable of type int",
    );
}

#[test]
fn slice_bounds_must_be_int() {
    rejects(
        "xs := [1, 2, 3]\nys := xs[\"a\":2]\n",
        "slice bound must be int, found str",
    );
    rejects(
        "xs := [1, 2, 3]\nys := xs[0:\"b\"]\n",
        "slice bound must be int, found str",
    );
    rejects(
        "xs := [1, 2, 3]\nys := xs[::\"c\"]\n",
        "slice bound must be int, found str",
    );
}

#[test]
fn map_is_not_sliceable() {
    rejects("m: Map[int, int] = {}\nx := m[0:2]\n", "cannot slice");
}

const BUF: &str = "\
struct Buf:
    xs: List[int]
    fn index(self, key: int) -> int:
        return self.xs[key]
    fn set_index(self, key: int, val: int):
        self.xs[key] = val
    fn slice(self, start: int? = None, end: int? = None, step: int? = None) -> Buf:
        return self
";

#[test]
fn struct_index_read_ok() {
    ok(&format!("{BUF}b := Buf([1, 2, 3])\nn: int = b[0]\n"));
    rejects(
        &format!("{BUF}b := Buf([1, 2, 3])\ns: str = b[0]\n"),
        "cannot assign int to variable of type str",
    );
}

#[test]
fn struct_index_assign_ok() {
    ok(&format!("{BUF}b := Buf([1, 2, 3])\nb[0] = 9\n"));
}

#[test]
fn struct_slice_ok() {
    ok(&format!("{BUF}b := Buf([1, 2, 3])\nc: Buf = b[0:2]\n"));
    // Optional bounds / step all route through the same protocol method.
    ok(&format!("{BUF}b := Buf([1, 2, 3])\nc: Buf = b[:]\n"));
    ok(&format!("{BUF}b := Buf([1, 2, 3])\nc: Buf = b[::-1]\n"));
}

#[test]
fn struct_without_index_rejected() {
    rejects(
        "struct Bad:\n    x: int\nb := Bad(1)\nn := b[0]\n",
        "cannot index into Bad",
    );
}

#[test]
fn struct_without_set_index_assign_rejected() {
    // Has `index` (read) but no `set_index` — `b[0] = 9` must be rejected.
    let read_only = "\
struct RO:
    xs: List[int]
    fn index(self, key: int) -> int:
        return self.xs[key]
";
    rejects(
        &format!("{read_only}b := RO([1, 2, 3])\nb[0] = 9\n"),
        "cannot index-assign into RO",
    );
}

#[test]
fn struct_without_slice_rejected() {
    let no_slice = "\
struct NS:
    xs: List[int]
    fn index(self, key: int) -> int:
        return self.xs[key]
";
    rejects(
        &format!("{no_slice}b := NS([1, 2, 3])\nc := b[0:2]\n"),
        "cannot slice NS",
    );
}

#[test]
fn generic_over_index_protocol_ok() {
    // A struct AND a built-in list both satisfy `Index[int, V]`; the bound recovers `V`.
    ok(&format!(
        "{BUF}fn first[C: Index[int, V], V](c: C) -> V:\n    return c[0]\n\
         b := Buf([1, 2, 3])\nn: int = first(b)\nm: int = first([10, 20])\n"
    ));
}

#[test]
fn index_assign_requires_index_not_just_set_index() {
    // IndexSet requires BOTH `index` and `set_index` (Rust IndexMut: Index). A struct with only
    // `set_index` must NOT be index-assignable — otherwise a compound `b[k] += v` (which reads via
    // `index` first) type-checks then crashes at runtime.
    let set_only = "\
struct WO:
    xs: List[int]
    fn set_index(self, key: int, val: int):
        self.xs[key] = val
";
    rejects(
        &format!("{set_only}b := WO([1, 2, 3])\nb[0] = 9\n"),
        "cannot index-assign into WO",
    );
}

#[test]
fn struct_slice_wrong_bound_types_rejected() {
    // The `Slice` protocol fixes the bounds as `slice(self, int?, int?, int?)` — both engines pass
    // three `Option[int]` components. A `slice` with non-`int?` bounds (or wrong arity) must NOT
    // count as a valid `Slice` impl (would crash).
    let bad = "\
struct BadSlice:
    xs: List[int]
    fn slice(self, start: str, end: str) -> int:
        return self.xs.len()
";
    rejects(
        &format!("{bad}b := BadSlice([1, 2, 3])\nc := b[0:2]\n"),
        "cannot slice BadSlice",
    );
    // Old 2-arg `slice(self, int, int)` is no longer a conforming `Slice` impl.
    let two_arg = "\
struct TwoArg:
    xs: List[int]
    fn slice(self, start: int, end: int) -> int:
        return self.xs.len()
";
    rejects(
        &format!("{two_arg}b := TwoArg([1, 2, 3])\nc := b[0:2]\n"),
        "cannot slice TwoArg",
    );
}

#[test]
fn generic_indexset_param_assign_ok() {
    // A bounded `[C: IndexSet[int, int]]` param is index-assignable inside the generic body.
    ok(&format!(
        "{BUF}fn put[C: IndexSet[int, int]](c: C):\n    c[0] = 42\n\
         b := Buf([1, 2, 3])\nput(b)\n"
    ));
}

#[test]
fn generic_indexset_rejects_str() {
    // `str` is immutable — it satisfies `Index` but NOT `IndexSet`, so a generic bounded by
    // `IndexSet` must reject a str argument.
    rejects(
        &format!("{BUF}fn put[C: IndexSet[int, int]](c: C):\n    c[0] = 42\nput(\"hi\")\n"),
        "does not satisfy IndexSet",
    );
}

#[test]
fn generic_over_slice_protocol_ok() {
    // A struct AND a built-in list both satisfy `Slice[R]`; the bound recovers `R`.
    ok(&format!(
        "{BUF}fn mid[C: Slice[R], R](c: C) -> R:\n    return c[1:2]\n\
         b := Buf([1, 2, 3])\nc: Buf = mid(b)\nd: List[int] = mid([1, 2, 3])\n"
    ));
}

// ===== defer =====

#[test]
fn defer_method_call_ok() {
    ok(
        "struct F:\n    n: int\n    fn close(self):\n        print(\"x\")\nfn w():\n    f := F(1)\n    defer f.close()\n",
    );
}

#[test]
fn defer_value_call_ok() {
    // A call to a first-class function value is allowed.
    ok("fn cleanup():\n    print(\"x\")\nfn w():\n    defer cleanup()\n");
}

#[test]
fn defer_at_top_level_ok() {
    // Block-scoped defer: the module body is the outermost scope, so top-level defer is legal.
    ok("fn cleanup():\n    print(\"x\")\ndefer cleanup()\n");
}

#[test]
fn defer_non_call_rejected() {
    rejects(
        "fn w():\n    defer 1 + 2\n",
        "defer requires a function or method call",
    );
}

#[test]
fn defer_builtin_accepted() {
    // The universe builtin FUNCTIONS print/ord/chr/panic are first-class values now — a bare
    // `defer print(...)` (etc.) is accepted (no wrapping needed).
    ok("fn w():\n    defer print(\"x\")\n");
    ok("fn w():\n    defer ord(\"a\")\n");
    ok("fn w():\n    defer chr(65)\n");
    ok("fn w():\n    defer panic(\"boom\")\n");
}

#[test]
fn defer_constructor_rejected() {
    rejects(
        "struct P:\n    x: int\nfn w():\n    defer P(1)\n",
        "built-ins and constructors must be wrapped",
    );
}

#[test]
fn defer_type_rejected() {
    // A reserved TYPE/ctor builtin (not one of the first-class fns) is still NOT first-class in
    // defer position — must be wrapped.
    rejects(
        "fn w():\n    defer int(1)\n",
        "built-ins and constructors must be wrapped",
    );
}

#[test]
fn firstclass_builtin_fn_value_position() {
    // The 4 universe fns type-check in value position (bound to a name, used as a HOF arg).
    ok("fn w():\n    f := ord\n    print(f(\"a\"))\n");
    ok("fn w():\n    p := panic\n    p(\"boom\")\n");
}

#[test]
fn print_value_not_usable_as_bool_condition() {
    // Bug 5: a `print` value in a bool-required position must be a TYPE ERROR (it is a function, not
    // bool) — it must NOT be silently accepted. Typing `print` as `Ty::Unknown` (an earlier attempt
    // at variadic value-form print) let `if p:` slip through the checker, and then the VM ran the
    // then-branch (an `Obj::Builtin` is truthy) while the interp faulted `expected bool, found
    // function` — a VM≠interp parity break. `print` now types as the dedicated `Ty::BuiltinFn`, which
    // (like the other three) `expect_bool` rejects. `if print:` (the bare form) is rejected too.
    rejects(
        "fn main():\n    p := print\n    if p:\n        print(\"t\")\nmain()\n",
        "must be bool",
    );
    rejects(
        "fn main():\n    if print:\n        print(\"t\")\nmain()\n",
        "must be bool",
    );
}

#[test]
fn print_value_form_is_fixed_arity() {
    // The VALUE form of `print` (`p := print`) is a fixed 1-arg function, NOT variadic — the
    // `sep=`/`end=` named args AND the variadic 0/many-arg shapes stay DIRECT-CALL-ONLY (they need
    // the specialized `CallPrintSep`/`CallPrint` opcodes, unreachable through a bound value). The
    // dedicated `Ty::BuiltinFn` carries `print`'s canonical 1-arg signature, so the direct call
    // `print(a, b)` still works but `p("a", "b")` / `p()` on a bound value do not.
    ok("fn w():\n    p := print\n    p(\"a\")\n");
    ok("fn w():\n    print(\"a\", \"b\", \"c\")\n"); // direct call stays variadic
    rejects("fn w():\n    p := print\n    p()\n", "argument");
    rejects("fn w():\n    p := print\n    p(\"a\", \"b\")\n", "argument");
}

#[test]
fn use_before_def_global_shadowing_builtin_rejected() {
    // Bug 1: a top-level global named like a first-class builtin fn, READ BEFORE its definition line,
    // must be a use-before-def error — EXACTLY like any other global (`x := y` before `y := 5` errors
    // `unknown name 'y'`). It must NOT silently resolve to the builtin: otherwise the VM (whose
    // `collect_globals` pre-scans every top-level `let` into a slot pre-initialised to `nil`) prints
    // `nil`, while the interp (source-order env; the name isn't defined yet) returns `Value::Builtin`
    // — a VM≠interp divergence on a program that (wrongly) type-checked. Suppress the first-class arm
    // whenever the name is a declared module-level global; a genuine `f := print` (no such global)
    // still resolves to the builtin.
    rejects("x := chr\nchr := \"z\"\nprint(x)\n", "unknown name 'chr'");
    // Sanity: the plain non-builtin case behaves identically (base semantics we mirror).
    rejects("x := y\ny := 5\nprint(x)\n", "unknown name 'y'");
}

#[test]
fn user_binding_shadows_firstclass_builtin_typechecks() {
    // Regression (bugs 1 & 3): a user binding named like a first-class builtin fn is legal
    // (`is_reserved_name` bans only `fn`/type/import-alias decls) and the checker types it as the
    // BINDING, not the builtin. A param, a top-level `:=`, and a loop var each shadow.
    ok("fn f(ord: int):\n    print(ord)\nf(42)\n");
    ok("chr := \"hi\"\nx := chr\nprint(x)\n");
    ok("for chr in [\"a\", \"b\"]:\n    print(chr)\n");
}

#[test]
fn type_name_not_firstclass_value() {
    // A reserved TYPE/ctor name is still NOT a first-class value (`f := List` fails).
    rejects("fn w():\n    f := List\n", "List");
}

#[test]
fn container_ctor_not_firstclass_value() {
    // Phase 2b: folding the four container ctors (range/List/Map/Set) into the native-prelude table as
    // `Intrinsic::Ctor` rows (`first_class: false`) must NOT make them first-class values — a
    // value-position use stays REJECTED, on the same fall-through path as the scalar ctors (`f := int`)
    // and user struct ctors (`f := Point`). The `first_class == false` gate keeps them off the
    // `LoadBuiltin`/`Ty::BuiltinFn` arm.
    rejects("fn w():\n    f := List\n", "List");
    rejects("fn w():\n    f := Map\n", "Map");
    rejects("fn w():\n    f := Set\n", "Set");
    rejects("fn w():\n    f := range\n", "range");
    // A container ctor in `defer` single-call position must still be wrapped (not first-class).
    rejects(
        "fn w():\n    defer List()\n",
        "built-ins and constructors must be wrapped",
    );
    // Table membership is orthogonal to genericity: `range` is a Ctor row but NON-generic, so a
    // turbofish (`range[int]()`) is still rejected — the error stays in `infer_named_call`, not the
    // table. (List/Map/Set ARE generic and accept a turbofish; only `range` rejects it.)
    rejects("x := range[int](5)\n", "takes no type arguments");
}

#[test]
fn scalar_ctor_not_firstclass_value() {
    // Phase 2a: folding the five scalar-conversion ctors (int/float/str/bytes/bytearray) into the
    // native-prelude table as `Intrinsic::Ctor` rows must NOT make them first-class values — a
    // value-position use (`f := int`) stays REJECTED, on the same fall-through path as `f := List`
    // (the `first_class == false` gate keeps them off the `LoadBuiltin`/`Ty::BuiltinFn` arm).
    rejects("fn w():\n    f := int\n", "int");
    rejects("fn w():\n    f := float\n", "float");
    rejects("fn w():\n    f := str\n", "str");
    rejects("fn w():\n    f := bytes\n", "bytes");
    rejects("fn w():\n    f := bytearray\n", "bytearray");
    // A scalar ctor in `defer` single-call position must still be wrapped (not first-class).
    rejects(
        "fn w():\n    defer str(\"x\")\n",
        "built-ins and constructors must be wrapped",
    );
}

#[test]
fn defer_block_form_ok() {
    // The block body is ordinary statements — built-ins like `print` are fine here (the call-only
    // restriction applies only to the single-call form).
    ok("fn w():\n    defer:\n        print(\"a\")\n        print(\"b\")\n");
}

#[test]
fn defer_block_reads_outer_local_ok() {
    // Same-task block: reading an enclosing local is allowed (no read-only capture floor).
    ok("fn w():\n    x := 1\n    defer:\n        print(\"{x}\")\n");
}

#[test]
fn defer_block_type_errors_still_caught() {
    // The block is checked like any nested scope — a use of an unbound name is still an error.
    rejects(
        "fn w():\n    defer:\n        z := nope + 1\n",
        "unknown name 'nope'",
    );
}

#[test]
fn defer_block_reassign_capture_ok() {
    // Uniform by-reference capture: a `defer:` block runs in the same task and shares the enclosing
    // binding's cell, so reassigning a captured local is now allowed (it mutates the shared cell,
    // visible in the owner — A2/A3/E1). No reassign gate.
    ok("fn w():\n    x := 1\n    defer:\n        x = 2\n");
}

#[test]
fn defer_block_new_binding_and_nonsendable_read_ok() {
    // Reading a capture into a NEW binding is fine, and — unlike a `spawn:` block — reading a
    // non-sendable captured value (a closure) is allowed (same task, no airlock).
    ok(
        "fn w():\n    x := 1\n    g := fn(): print(\"g\")\n    defer:\n        y := x + 1\n        print(\"{y}\")\n        g()\n",
    );
}

// ===== optional chaining `?.` + null-coalescing `??` (desugared to `match`) =====

#[test]
fn null_coalesce_some_picks_value() {
    ok_desugared("o := Some(5)\nx: int = o ?? 0\nprint(x)\n");
}

#[test]
fn null_coalesce_result_type() {
    // `a ?? b` evaluates to the inner type, usable in arithmetic.
    ok_desugared("o := Some(5)\ny := (o ?? 0) + 1\nprint(y)\n");
}

#[test]
fn null_coalesce_lhs_must_be_option() {
    // A Result LHS has Ok/Err, not Some/None — the desugared match is rejected.
    rejects_desugared("r := Ok(5)\nx := r ?? 0\n", "");
}

#[test]
fn opt_chain_on_non_option_does_not_leak_temp_name() {
    // `x?.f` on a non-Option is an error, but the desugared match's internal `__opt` binding must
    // NOT leak into a spurious "unknown name '__opt…'" cascade.
    let errs = check_desugared("x := 5\ny := x?.f\n");
    assert!(!errs.is_empty(), "non-option opt-chain should error");
    assert!(
        !errs.iter().any(|e| e.message.contains("__opt")),
        "internal temp name leaked into a diagnostic: {errs:?}"
    );
}

#[test]
fn opt_chain_returns_option_of_field() {
    ok_desugared("struct P:\n    x: int\nop := Some(P(1))\nr: Option[int] = op?.x\n");
}

#[test]
fn opt_chain_method_returns_option() {
    ok_desugared(
        "struct P:\n    x: int\n    fn get(self) -> int:\n        return self.x\nop := Some(P(1))\nr: Option[int] = op?.get()\n",
    );
}

#[test]
fn opt_chain_chains_nested() {
    // `a?.b?.c` — each layer operates on the Option result of the inner.
    ok_desugared(
        "struct Inner:\n    v: int\nstruct Outer:\n    inner: Inner\no := Some(Outer(Inner(7)))\nr: Option[int] = o?.inner?.v\n",
    );
}

#[test]
fn opt_chain_double_option_not_flattened() {
    // A field that is itself `Option[int]` yields `Option[Option[int]]` — NOT flattened.
    ok_desugared(
        "struct P:\n    maybe: Option[int]\nop := Some(P(Some(1)))\nr: Option[Option[int]] = op?.maybe\n",
    );
    // ...so binding the same expression to `Option[int]` must fail.
    rejects_desugared(
        "struct P:\n    maybe: Option[int]\nop := Some(P(Some(1)))\nbad: Option[int] = op?.maybe\n",
        "",
    );
}

// ===== sort_by_key =====

#[test]
fn sort_by_key_int_key_ok() {
    ok("xs := [3, 1, 2]\nxs.sort_by_key(fn(x: int) -> int: -x)\n");
}

#[test]
fn sort_by_key_struct_key_ok() {
    // A key function returning a Comparable struct is accepted (compared via `compare`).
    ok(
        "struct M:\n    n: int\n    fn compare(self, o: M) -> int:\n        return self.n - o.n\nxs := [M(2), M(1)]\nxs.sort_by_key(fn(m: M) -> M: m)\n",
    );
}

#[test]
fn sort_by_key_non_comparable_key_rejected() {
    // A key function returning a non-Comparable struct (no `compare`) is rejected. File-backed as
    // `sort_by_key[K: Comparable](...)`: the `K: Comparable` bound is recovered from the closure body
    // by the loop-back, then enforced — the uniform protocol-conformance diagnostic (was bespoke).
    rejects(
        "struct B:\n    n: int\nxs := [B(2), B(1)]\nxs.sort_by_key(fn(b: B) -> B: b)\n",
        "does not satisfy Comparable",
    );
}

#[test]
fn sort_by_key_wrong_arity_rejected() {
    // A 2-arg closure where a `fn(T) -> K` is expected: uniform argument-mismatch diagnostic.
    rejects(
        "xs := [1, 2]\nxs.sort_by_key(fn(a: int, b: int) -> int: a - b)\n",
        "argument to 'sort_by_key' has type fn(int, int) -> int, expected fn(int) -> K",
    );
}

// ===== calling a function-typed field =====

#[test]
fn fn_typed_field_call_through_self_ok() {
    ok_desugared(
        "struct A:\n    op: fn(int) -> int\n    fn run(self, x: int) -> int:\n        return self.op(x)\na := A(fn(x: int) -> int: x + 1)\nprint(a.run(5))\n",
    );
}

#[test]
fn fn_typed_field_call_on_external_receiver_ok() {
    ok_desugared(
        "struct A:\n    op: fn(int) -> int\na := A(fn(x: int) -> int: x + 1)\nprint(a.op(7))\n",
    );
}

#[test]
fn fn_typed_field_call_arg_type_checked() {
    // The field's fn type is enforced: a str where an int is expected is rejected.
    rejects_desugared(
        "struct A:\n    op: fn(int) -> int\na := A(fn(x: int) -> int: x + 1)\nprint(a.op(\"no\"))\n",
        "",
    );
}

#[test]
fn non_fn_field_call_still_rejected() {
    // A non-function field is not callable: still a "no method" error (no spurious field-call).
    rejects_desugared(
        "struct A:\n    n: int\na := A(1)\nprint(a.n(2))\n",
        "has no method 'n'",
    );
}

// ----- concurrency C1: spawn / parallel: nursery scoping -----

#[test]
fn spawn_at_function_scope_ok() {
    // M-C implicit nurseries: every function body is an implicit nursery, so a bare `spawn`
    // (no enclosing `parallel:`) is legal anywhere in a function. It joins at the function's end.
    ok("fn w():\n    print(1)\nfn main():\n    spawn w()\nmain()\n");
}

#[test]
fn spawn_at_module_toplevel_ok() {
    // M-C: the module top level is itself an implicit nursery (joins at program exit).
    ok("fn w():\n    print(1)\nspawn w()\n");
}

#[test]
fn spawn_inside_parallel_ok() {
    ok(
        "fn w():\n    print(1)\nfn main():\n    parallel:\n        spawn w()\n        spawn w()\nmain()\n",
    );
}

#[test]
fn nested_parallel_ok() {
    ok(
        "fn w():\n    print(1)\nfn main():\n    parallel:\n        parallel:\n            spawn w()\nmain()\n",
    );
}

#[test]
fn spawn_block_form_ok() {
    ok("fn main():\n    parallel:\n        spawn:\n            print(1)\nmain()\n");
}

// ----- concurrency C2: Channel[T] + sendability -----

#[test]
fn channel_construct_and_methods_ok() {
    ok(
        "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    x := ch.recv()\n    n := ch.len()\n    print(x + n)\nmain()\n",
    );
}

#[test]
fn channel_bounded_capacity_and_cap_method_ok() {
    // `Channel[T](cap)` accepts an int capacity; `cap()` infers int on both bounded and unbounded.
    ok(
        "fn main():\n    b := Channel[int](4)\n    print(b.cap())\n    u := Channel[int]()\n    print(u.cap())\nmain()\n",
    );
}

#[test]
fn channel_capacity_must_be_int_rejected() {
    rejects(
        "fn main():\n    ch := Channel[int](\"x\")\n    print(ch.len())\nmain()\n",
        "Channel capacity must be int",
    );
}

#[test]
fn channel_too_many_ctor_args_rejected() {
    rejects(
        "fn main():\n    ch := Channel[int](1, 2)\n    print(ch.len())\nmain()\n",
        "takes an optional capacity",
    );
}

#[test]
fn channel_send_wrong_type_rejected() {
    rejects(
        "fn main():\n    ch := Channel[int]()\n    ch.send(\"x\")\nmain()\n",
        "expected int",
    );
}

#[test]
fn channel_needs_element_type() {
    rejects(
        "fn main():\n    ch := Channel()\n    print(ch.len())\nmain()\n",
        "Channel() needs an element type",
    );
}

#[test]
fn channel_protocol_element_is_sendable() {
    // Task 2 (option a): a protocol existential element type is now SENDABLE (Go `chan interface`
    // parity) — the erased witness crosses by value. A witness that genuinely can't serialize (an
    // FFI/native handle) is rejected at the RUNTIME airlock, not here — see
    // `vm::parity_tests::ffi_handle_cannot_cross_airlock_three_engine`. (Was rejected under the old
    // Error-only sendable-bounded rule.)
    entry_ok(
        "protocol NS:\n    fn tag(self) -> int\nfn main():\n    ch := Channel[NS]()\n    print(ch.len())\nmain()\n",
    );
}

// ----- concurrency §6d: `wait` (select) -----

#[test]
fn wait_binds_element_type_in_arm() {
    ok(
        "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    wait:\n        v := ch.recv(): print(v + 1)\nmain()\n",
    );
}

#[test]
fn wait_assign_target_typechecks() {
    ok(
        "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    n := 0\n    wait:\n        n = ch.recv(): print(n)\nmain()\n",
    );
}

#[test]
fn wait_send_arm_typechecks() {
    // A bare `ch.send(v):` arm is legal — `v` must match the channel element type.
    ok(
        "fn main():\n    ch := Channel[int](1)\n    wait:\n        ch.send(9): print(\"sent\")\n        else: print(\"full\")\nmain()\n",
    );
}

#[test]
fn wait_send_arm_wrong_value_type_rejected() {
    // `send` value must match the element type (`int`), reusing the ordinary channel-send check.
    rejects(
        "fn main():\n    ch := Channel[int](1)\n    wait:\n        ch.send(\"x\"): print(1)\nmain()\n",
        "int",
    );
}

#[test]
fn wait_bare_non_send_arm_rejected() {
    // A bare arm that is not `ch.send(v)` (here `try_send`) lists the legal arm forms.
    rejects(
        "fn main():\n    ch := Channel[int](1)\n    wait:\n        ch.try_send(1): print(1)\nmain()\n",
        "a wait arm must be a recv (`x := ch.recv()`), a send (`ch.send(v)`)",
    );
}

#[test]
fn wait_bare_arbitrary_expr_arm_rejected() {
    rejects(
        "fn f() -> int:\n    return 0\nfn main():\n    wait:\n        f(): print(1)\nmain()\n",
        "a wait arm must be a recv",
    );
}

#[test]
fn wait_discard_and_else_ok() {
    ok(
        "fn main():\n    ch := Channel[int]()\n    wait:\n        _ := ch.recv(): print(1)\n        else: print(0)\nmain()\n",
    );
}

#[test]
fn wait_arm_bound_var_wrong_use_rejected() {
    // `v: int` bound by the arm — using it as a str is a type error.
    rejects(
        "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    wait:\n        v := ch.recv(): print(v + \"x\")\nmain()\n",
        "int",
    );
}

#[test]
fn wait_assign_wrong_target_type_rejected() {
    rejects(
        "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    s := \"a\"\n    wait:\n        s = ch.recv(): print(s)\nmain()\n",
        "int",
    );
}

#[test]
fn wait_arm_binding_is_arm_local() {
    // `v` is scoped to its arm — referencing it after the `wait` is undeclared.
    rejects(
        "fn main():\n    ch := Channel[int]()\n    wait:\n        v := ch.recv(): print(v)\n    print(v)\nmain()\n",
        "v",
    );
}

#[test]
fn wait_non_channel_arm_rejected() {
    rejects(
        "fn main():\n    f := fn() -> int: 1\n    wait:\n        v := f.recv(): print(v)\nmain()\n",
        "Channel",
    );
}

#[test]
fn wait_send_arm_non_channel_receiver_rejected() {
    // A send-arm receiver that merely HAS a `send` method (a user struct) but is not a `Channel[T]`
    // MUST be rejected — the compiler lowers a send-arm as a raw channel op, so a non-channel
    // receiver panics the VM (`channel_core on non-channel`) at runtime. Mirrors the recv-arm guard.
    rejects(
        "struct Box:\n    n: int\n    fn send(self, v: int): print(v)\nfn main():\n    b := Box(0)\n    wait:\n        b.send(5): print(\"x\")\n        else: print(\"idle\")\nmain()\n",
        "Channel",
    );
}

#[test]
fn channel_close_ok() {
    ok("fn main():\n    ch := Channel[int]()\n    ch.close()\nmain()\n");
}

#[test]
fn channel_close_rejects_args() {
    rejects(
        "fn main():\n    ch := Channel[int]()\n    ch.close(1)\nmain()\n",
        "close",
    );
}

#[test]
fn channel_try_send_returns_bool() {
    ok(
        "fn main():\n    ch := Channel[int]()\n    sent: bool = ch.try_send(1)\n    print(sent)\nmain()\n",
    );
}

#[test]
fn channel_try_send_wrong_type_rejected() {
    rejects(
        "fn main():\n    ch := Channel[int]()\n    ch.try_send(\"x\")\nmain()\n",
        "expected int",
    );
}

#[test]
fn for_over_channel_binds_element() {
    ok(
        "fn main():\n    ch := Channel[int]()\n    ch.close()\n    for v in ch:\n        print(v + 1)\nmain()\n",
    );
}

#[test]
fn for_over_channel_two_vars_rejected() {
    rejects(
        "fn main():\n    ch := Channel[int]()\n    for k, v in ch:\n        print(k)\nmain()\n",
        "single loop variable",
    );
}

#[test]
fn close_on_non_channel_rejected() {
    rejects("fn main():\n    x := 1\n    x.close()\nmain()\n", "close");
}

#[test]
fn comprehension_over_channel_rejected() {
    // A comprehension over a channel would diverge between engines (VM drains, interp oracle can't);
    // reject it on both — `for v in ch:` is the channel-draining form.
    rejects(
        "fn main():\n    ch := Channel[int]()\n    xs := [v for v in ch]\n    print(xs)\nmain()\n",
        "channel cannot be drained in a comprehension",
    );
}

#[test]
fn spawn_capture_free_closure_arg_ok() {
    // B3.3 (Task 2a): a CAPTURE-FREE closure passed as a spawn arg is sendable — it crosses the
    // airlock by value — so it is accepted (was rejected under the old "Func non-sendable" rule).
    ok(
        "fn run(f: fn() -> int):\n    print(f())\nfn main():\n    g := fn() -> int: 1\n    parallel:\n        spawn run(g)\nmain()\n",
    );
}

#[test]
fn spawn_arg_closure_capturing_protocol_local_ok() {
    // Task 2 (option a): a closure ARG capturing a protocol-typed local `p: NS = Boxy(0)` is now
    // ACCEPTED — a protocol existential is sendable (its witness `Boxy` crosses by deep value copy).
    // The capture gate routes through `self.sendable()`, so this flipped when protocols became
    // sendable; the old rejection was a false positive. A capture that genuinely can't cross (an
    // FFI/native handle) is still rejected at the RUNTIME airlock — see
    // `vm::parity_tests::ffi_handle_cannot_cross_airlock_three_engine`.
    entry_ok(
        "protocol NS:\n    fn tag(self) -> int\nstruct Boxy:\n    v: int\n    fn tag(self) -> int:\n        return self.v\nfn run(f: fn() -> int):\n    print(f())\nfn main():\n    p: NS = Boxy(0)\n    g := fn() -> int: p.tag()\n    parallel:\n        spawn run(g)\nmain()\n",
    );
}

#[test]
fn spawn_keyword_arg_closure_capturing_protocol_local_ok() {
    // Task 2 (option a): a closure capturing a protocol-typed local, passed by LABEL or positionally
    // through a function VALUE, is now ACCEPTED — a protocol existential is sendable (witness `Boxy`
    // crosses by deep value copy). Both spellings agree (the capture gate routes both through
    // `self.sendable()`). Genuine non-sendability (FFI handle) is caught at the runtime airlock.
    let prelude = "protocol NS:\n    fn tag(self) -> int\nstruct Boxy:\n    v: int\n    fn tag(self) -> int:\n        return self.v\nfn run(f: fn() -> int):\n    print(f())\n";
    entry_ok(&format!(
        "{prelude}fn main():\n    h := run\n    p: NS = Boxy(0)\n    g := fn() -> int: p.tag()\n    parallel:\n        spawn h(f=g)\nmain()\n"
    ));
    // The positional form of the SAME call is accepted identically — parity of the two spellings.
    entry_ok(&format!(
        "{prelude}fn main():\n    h := run\n    p: NS = Boxy(0)\n    g := fn() -> int: p.tag()\n    parallel:\n        spawn h(g)\nmain()\n"
    ));
}

#[test]
fn spawn_protocol_value_keyword_arg_ok() {
    // Task 2 (option a): a protocol-typed value passed by label OR positionally to a spawned function
    // value is now ACCEPTED — a protocol existential is sendable (its witness `Boxy` crosses by deep
    // value copy, agrees serial==M:N). Both spellings agree. (Was rejected under the old rule.)
    let prelude = "protocol NS:\n    fn tag(self) -> int\nstruct Boxy:\n    v: int\n    fn tag(self) -> int:\n        return self.v\nfn run(r: NS):\n    print(r.tag())\n";
    entry_ok(&format!(
        "{prelude}fn main():\n    h := run\n    b: NS = Boxy(0)\n    parallel:\n        spawn h(r=b)\nmain()\n"
    ));
    // Positional form of the same call — accepted identically.
    entry_ok(&format!(
        "{prelude}fn main():\n    h := run\n    b: NS = Boxy(0)\n    parallel:\n        spawn h(b)\nmain()\n"
    ));
}

#[test]
fn spawn_sendable_args_ok() {
    ok(
        "fn worker(id: int, prefix: str, out: Channel[str]):\n    out.send(\"{prefix}-{id}\")\nfn main():\n    ch := Channel[str]()\n    parallel:\n        spawn worker(1, \"t\", ch)\nmain()\n",
    );
}

#[test]
fn spawn_firstclass_builtin_accepted_like_defer() {
    // The universe fns print/ord/chr/panic are first-class values that cross the airlock by name,
    // so `spawn print(...)` is accepted — symmetric with `defer print(...)`. A non-first-class
    // builtin/ctor still must be wrapped.
    ok("fn main():\n    parallel:\n        spawn print(\"hi\")\nmain()\n");
    rejects(
        "fn main():\n    parallel:\n        spawn int(1)\nmain()\n",
        "spawn requires a function or method call",
    );
}

#[test]
fn defer_spawn_builtin_named_args_rejected() {
    // sep=/end= are direct-call-only (the specialized print opcode); a deferred/spawned print runs
    // its value form and can't carry them, so they are rejected rather than silently dropped.
    rejects(
        "fn w():\n    defer print(\"a\", sep=\"-\")\n",
        "named arguments (sep=/end=) are only supported on a direct print(...) call, not a deferred one",
    );
    rejects(
        "fn main():\n    parallel:\n        spawn print(\"a\", end=\"!\")\nmain()\n",
        "named arguments (sep=/end=) are only supported on a direct print(...) call, not a spawned one",
    );
}

#[test]
fn spawn_bad_arg_reports_one_error() {
    // The sendability gate must not double-report a type error already raised by inferring the call.
    let errs = check_src(
        "fn w(x: int):\n    print(x)\nfn main():\n    parallel:\n        spawn w(nope)\nmain()\n",
    );
    let dups = errs
        .iter()
        .filter(|e| e.message.contains("unknown name 'nope'"))
        .count();
    assert_eq!(
        dups, 1,
        "expected exactly one 'unknown name' error, got: {errs:?}"
    );
}

#[test]
fn channel_protocol_struct_field_is_sendable() {
    // Task 2 (option a): a struct with a protocol-typed field is now a SENDABLE channel element — the
    // field's erased witness crosses by deep value copy. (Was rejected under the old rule.)
    entry_ok(
        "protocol NS:\n    fn tag(self) -> int\nstruct Holder:\n    f: NS\nfn main():\n    ch := Channel[Holder]()\n    print(ch.len())\nmain()\n",
    );
}

#[test]
fn spawn_protocol_struct_field_arg_ok() {
    // Task 2 (option a): a struct carrying a protocol-typed field is a SENDABLE spawn arg — the whole
    // aggregate (`Holder{f: Boxy(0)}`) crosses by deep value copy. (Was rejected under the old rule.)
    entry_ok(
        "protocol NS:\n    fn tag(self) -> int\nstruct Boxy:\n    v: int\n    fn tag(self) -> int:\n        return self.v\nstruct Holder:\n    f: NS\nfn run_it(h: Holder):\n    print(h.f.tag())\nfn main():\n    h := Holder(Boxy(0))\n    parallel:\n        spawn run_it(h)\nmain()\n",
    );
}

#[test]
fn sendable_recursive_struct_ok() {
    // A self-referential struct of sendable fields must terminate (cycle guard) and be sendable.
    ok(
        "struct Node:\n    val: int\n    next: Node\nfn use_it(n: Node):\n    print(n.val)\nfn main():\n    parallel:\n        spawn:\n            print(1)\nmain()\n",
    );
}

#[test]
fn reassign_captured_local_in_spawn_block_ok() {
    // Uniform by-reference capture (F1): a `spawn:` task gets its OWN per-task copy of a captured
    // LOCAL (the airlock deep-copies its cell), so reassigning it is allowed — the write mutates the
    // isolated copy and is not visible to the parent (the one deliberate divergence from Go).
    ok(
        "fn main():\n    counter := 0\n    parallel:\n        spawn:\n            counter = counter + 1\nmain()\n",
    );
}

// ----- B3: in-place mutation of a captured MODULE GLOBAL aggregate is frozen too -----

#[test]
fn mutate_fn_local_aggregate_in_spawn_block_ok() {
    // A fn-LOCAL aggregate captured into a spawn: is deep-copied per task (agrees serial==M:N), so its
    // in-place mutation stays ACCEPTED — only MODULE-GLOBAL roots are frozen.
    ok(
        "fn main():\n    xs := [1, 2, 3]\n    parallel:\n        spawn:\n            xs.push(99)\n    print(xs.len())\nmain()\n",
    );
    ok(
        "fn main():\n    m := {1: 2}\n    parallel:\n        spawn:\n            m[1] = 9\nmain()\n",
    );
}

#[test]
fn read_method_on_captured_module_global_in_spawn_ok() {
    // A non-mutating method (`.len()`, `.get()`, `.map()`) on a captured module global is a READ — it
    // must stay accepted (guards against over-listing the mutator set).
    ok(
        "xs := [1, 2, 3]\nfn main():\n    parallel:\n        spawn:\n            print(xs.len())\nmain()\n",
    );
}

#[test]
fn mutate_module_global_aggregate_sequentially_ok() {
    // Outside any spawned task the module global is not frozen — a top-level `.push` stays fine.
    ok("xs := [1, 2, 3]\nxs.push(4)\nprint(xs.len())\n");
}

#[test]
fn shared_update_on_captured_module_global_in_spawn_ok() {
    // The collision guard: `Shared.update` shares the mutator name `update` with `Map`, but the gate is
    // typed on the receiver so a captured module-global Shared stays accepted (that IS the cross-task box).
    entry_ok(
        "import std.concurrency\ng := Shared(0)\nfn main():\n    parallel:\n        spawn:\n            g.update(fn(x): x + 1)\nmain()\n",
    );
}

#[test]
fn task_local_binding_in_spawn_block_assignable() {
    // A binding declared *inside* the task body is task-local, not a capture — assignable.
    ok(
        "fn main():\n    parallel:\n        spawn:\n            x := 0\n            x = x + 1\n            print(x)\nmain()\n",
    );
}

#[test]
fn spawn_in_plain_fn_ok() {
    // M-C: a `spawn` in a function with no explicit `parallel:` is legal — the function body is an
    // implicit nursery that joins at the function's end. The function-boundary rule still holds at
    // runtime (the task binds to *this* function's nursery, never the caller's), enforced by the
    // compiler/VM emitting a per-function implicit nursery.
    ok(
        "fn w():\n    spawn other()\nfn other():\n    print(1)\nfn main():\n    parallel:\n        w()\nmain()\n",
    );
}

// ----- concurrency C3: Shared[T], the cross-task mutable box -----

#[test]
fn shared_construct_and_methods_ok() {
    // `Shared(v)` infers its element type from the value (no `[T]` type arg, unlike Channel).
    entry_ok(
        "import std.concurrency\nfn main():\n    s := Shared(0)\n    s.set(5)\n    s.update(fn(x): x + 1)\n    print(s.get())\nmain()\n",
    );
}

#[test]
fn shared_get_returns_element_type() {
    // `get()` yields `T`, so it must compose where a `T` is expected (here, str concat).
    entry_ok(
        "import std.concurrency\nfn main():\n    s := Shared(\"hi\")\n    msg := s.get() + \"!\"\n    print(msg)\nmain()\n",
    );
}

#[test]
fn shared_set_wrong_type_rejected() {
    entry_rejects(
        "import std.concurrency\nfn main():\n    s := Shared(0)\n    s.set(\"x\")\nmain()\n",
        "expected int",
    );
}

#[test]
fn shared_update_fn_arity_rejected() {
    // `update` takes `fn(T) -> T`; a two-param closure must not type-check.
    entry_rejects(
        "import std.concurrency\nfn main():\n    s := Shared(0)\n    s.update(fn(x, y): x + y)\nmain()\n",
        "argument 1 of 'update'",
    );
}

#[test]
fn shared_accepts_turbofish() {
    // `Shared[T](v)` — the turbofish is OPTIONAL (value-first), and when present pins the element
    // type. It must agree with the value's inferred type.
    entry_ok(
        "import std.concurrency\nfn main():\n    s := Shared[int](0)\n    print(s.get())\nmain()\n",
    );
}

#[test]
fn rwshared_list_view_methods_accept_on_list_element() {
    // The zero-copy read-view methods (`len`/`at`/`slice`/`for_each`/`fold`) are valid when the box
    // element is a list. `fold`'s R is inferred from the concrete `init` accumulator.
    entry_ok(
        "import std.concurrency\nfn main():\n    box := RwShared([1, 2, 3])\n    print(box.len())\n    print(box.at(0))\n    print(box.slice(0, 2))\n    box.for_each(fn(x): print(x))\n    print(box.fold(0, fn(a, x): a + x))\nmain()\n",
    );
    // R is not pinned to the element type — a str accumulator folds a list of ints to a str.
    entry_ok(
        "import std.concurrency\nfn main():\n    box := RwShared([1, 2, 3])\n    s := box.fold(\"\", fn(a, x): a + str(x))\n    print(s)\nmain()\n",
    );
}

#[test]
fn rwshared_list_view_methods_reject_on_non_list_element() {
    // Element-shape gate at the CHECKER (no check-OK-then-run-fault): the read-view methods are gated
    // to a list element, so a non-list `RwShared[int]` cleanly reports "no method".
    entry_rejects(
        "import std.concurrency\nfn main():\n    box := RwShared(0)\n    print(box.fold(0, fn(a, x): a + x))\nmain()\n",
        "type RwShared[int] has no method 'fold'",
    );
    entry_rejects(
        "import std.concurrency\nfn main():\n    box := RwShared(0)\n    print(box.at(0))\nmain()\n",
        "type RwShared[int] has no method 'at'",
    );
    entry_rejects(
        "import std.concurrency\nfn main():\n    box := RwShared(0)\n    print(box.len())\nmain()\n",
        "type RwShared[int] has no method 'len'",
    );
}

#[test]
fn rwshared_readview_gate_rejects_non_container_and_wrong_method() {
    // Constructor-kind gate: a scalar/tuple element head is not a recognized container, so every
    // read-view method cleanly reports "no method" (no check-OK-then-run-fault). A Map/Set-only method
    // name on the wrong container head also misses.
    // int element — Set/Map methods miss.
    entry_rejects(
        "import std.concurrency\nfn main():\n    box := RwShared(0)\n    print(box.contains(3))\nmain()\n",
        "type RwShared[int] has no method 'contains'",
    );
    // str element — fold_entries (a Map method) misses.
    entry_rejects(
        "import std.concurrency\nfn main():\n    box := RwShared(\"hi\")\n    print(box.fold_entries(0, fn(a, k, v): a))\nmain()\n",
        "has no method 'fold_entries'",
    );
    // Tuple element is heterogeneous — EXCLUDED entirely: len/fold both miss.
    entry_rejects(
        "import std.concurrency\nfn main():\n    box := RwShared((1, \"a\"))\n    print(box.len())\nmain()\n",
        "has no method 'len'",
    );
    entry_rejects(
        "import std.concurrency\nfn main():\n    box := RwShared((1, \"a\"))\n    print(box.fold(0, fn(a, x): a))\nmain()\n",
        "has no method 'fold'",
    );
    // A List element rejects Map/Set-specific method names (fold_entries/contains) — head branches first.
    entry_rejects(
        "import std.concurrency\nfn main():\n    box := RwShared([1, 2, 3])\n    print(box.fold_entries(0, fn(a, k, v): a))\nmain()\n",
        "has no method 'fold_entries'",
    );
    // A Map element rejects the List-only `fold`/`at` names (Map uses fold_entries).
    entry_rejects(
        "import std.concurrency\nfn main():\n    box := RwShared({\"a\": 1})\n    print(box.fold(0, fn(a, x): a))\nmain()\n",
        "has no method 'fold'",
    );
}

#[test]
fn rwshared_map_readview_methods() {
    // Map read-view: len/get_key/has/for_each_entry/fold_entries. K/V arm-recovered from the concrete
    // Map[K,V]; fold_entries's R pins from the concrete init.
    entry_ok(
        "import std.concurrency\nfn main():\n    box := RwShared({\"a\": 1, \"b\": 2})\n    print(box.len())\n    print(box.has(\"a\"))\n    box.for_each_entry(fn(k, v): print(k + str(v)))\n    print(box.fold_entries(0, fn(a, k, v): a + v))\nmain()\n",
    );
    // get_key returns Option[V] — matchable, V concrete (no unbound Param escapes).
    entry_ok(
        "import std.concurrency\nfn main():\n    box := RwShared({\"a\": 1})\n    match box.get_key(\"a\"):\n        Some(v): print(v + 1)\n        None: print(-1)\nmain()\n",
    );
    // Nesting: V = List[int] recovered.
    entry_ok(
        "import std.concurrency\nfn main():\n    box := RwShared({\"a\": [1, 2]})\n    match box.get_key(\"a\"):\n        Some(v): print(v.len())\n        None: print(-1)\nmain()\n",
    );
    // fold_entries R is not pinned to V — a str accumulator over a Map[str,int] folds to str.
    entry_ok(
        "import std.concurrency\nfn main():\n    box := RwShared({\"a\": 1})\n    s := box.fold_entries(\"\", fn(a, k, v): a + k)\n    print(s)\nmain()\n",
    );
    // Wrong key type on get_key/has is rejected (K = str; int arg mismatches).
    entry_rejects(
        "import std.concurrency\nfn main():\n    box := RwShared({\"a\": 1})\n    print(box.get_key(3))\nmain()\n",
        "expected",
    );
}

#[test]
fn rwshared_set_readview_methods() {
    // Set read-view: len/contains/for_each/fold. E arm-recovered from the concrete Set[E].
    entry_ok(
        "import std.concurrency\nfn main():\n    box := RwShared(Set([1, 2, 3]))\n    print(box.len())\n    print(box.contains(2))\n    box.for_each(fn(x): print(x))\n    print(box.fold(0, fn(a, x): a + x))\nmain()\n",
    );
    // contains with the wrong element type is rejected (E = int; str arg mismatches).
    entry_rejects(
        "import std.concurrency\nfn main():\n    box := RwShared(Set([1, 2, 3]))\n    print(box.contains(\"x\"))\nmain()\n",
        "expected",
    );
    // Set fold R not pinned to E — str accumulator over Set[int] folds to str.
    entry_ok(
        "import std.concurrency\nfn main():\n    box := RwShared(Set([1, 2, 3]))\n    s := box.fold(\"\", fn(a, x): a + str(x))\n    print(s)\nmain()\n",
    );
}

#[test]
fn shared_is_sendable() {
    // A `Shared[T]` handle crosses the airlock — both spawned tasks reach the same box.
    entry_ok(
        "import std.concurrency\nfn bump(s: Shared[int]):\n    s.update(fn(x): x + 1)\nfn main():\n    s := Shared(0)\n    parallel:\n        spawn bump(s)\n        spawn bump(s)\n    print(s.get())\nmain()\n",
    );
}

#[test]
fn shared_handle_sendable_regardless_of_element() {
    // The asymmetry vs Channel: a `Shared` handle is sendable even when its element type isn't
    // (the value never crosses the airlock — only the handle does). Locks in the intent.
    entry_ok(
        "import std.concurrency\nfn use_it(s: Shared[fn() -> int]):\n    f := s.get()\n    print(f())\nfn main():\n    g := fn() -> int: 1\n    s := Shared(g)\n    parallel:\n        spawn use_it(s)\nmain()\n",
    );
}

// ----- concurrency: RwShared[T], the cross-task read-write box -----

#[test]
fn rwshared_construct_and_methods_ok() {
    // `RwShared(v)` infers its element type from the value (value-first, like `Shared`).
    entry_ok(
        "import std.concurrency\nfn main():\n    r := RwShared(0)\n    r.set(5)\n    r.write(fn(x): x + 1)\n    print(r.read(fn(x): x))\n    print(r.get())\nmain()\n",
    );
}

#[test]
fn rwshared_read_returns_closure_result_type() {
    // `read(f: fn(T) -> R) -> R` — R is the closure's return type, distinct from T here (int -> str).
    entry_ok(
        "import std.concurrency\nfn main():\n    r := RwShared(0)\n    msg := r.read(fn(x): str(x)) + \"!\"\n    print(msg)\nmain()\n",
    );
}

#[test]
fn rwshared_read_closure_body_error_reported_once() {
    // A type error inside the read() closure body must be reported ONCE. `read` recovers R by
    // re-inferring the closure after `check_args` already inferred it; that recovery must not
    // double-emit the body's errors (regression: `nope` was reported twice).
    let errs = check_entry(
        "import std.concurrency\nfn main():\n    r := RwShared(0)\n    print(r.read(fn(x): nope + x))\nmain()\n",
    );
    let n = errs.iter().filter(|e| e.message.contains("nope")).count();
    assert_eq!(
        n, 1,
        "expected the 'nope' error exactly once, got: {errs:?}"
    );
}

#[test]
fn rwshared_get_returns_element_type() {
    entry_ok(
        "import std.concurrency\nfn main():\n    r := RwShared(\"hi\")\n    msg := r.get() + \"!\"\n    print(msg)\nmain()\n",
    );
}

#[test]
fn rwshared_set_wrong_type_rejected() {
    entry_rejects(
        "import std.concurrency\nfn main():\n    r := RwShared(0)\n    r.set(\"x\")\nmain()\n",
        "expected int",
    );
}

#[test]
fn rwshared_write_fn_arity_rejected() {
    entry_rejects(
        "import std.concurrency\nfn main():\n    r := RwShared(0)\n    r.write(fn(x, y): x + y)\nmain()\n",
        "argument 1 of 'write'",
    );
}

#[test]
fn rwshared_unknown_method_rejected() {
    entry_rejects(
        "import std.concurrency\nfn main():\n    r := RwShared(0)\n    r.update(fn(x): x + 1)\nmain()\n",
        "has no method 'update'",
    );
}

#[test]
fn rwshared_accepts_turbofish() {
    entry_ok(
        "import std.concurrency\nfn main():\n    r := RwShared[int](0)\n    print(r.get())\nmain()\n",
    );
}

#[test]
fn rwshared_is_sendable() {
    entry_ok(
        "import std.concurrency\nfn bump(r: RwShared[int]):\n    r.write(fn(x): x + 1)\nfn main():\n    r := RwShared(0)\n    parallel:\n        spawn bump(r)\n        spawn bump(r)\n    print(r.get())\nmain()\n",
    );
}

#[test]
fn rwshared_annotation_with_map_ok() {
    entry_ok(
        "import std.concurrency\nfn put(r: RwShared[Map[str, int]], k: str):\n    r.write(fn(m): m)\nfn main():\n    r := RwShared({\"a\": 1})\n    put(r, \"b\")\n    print(r.get())\nmain()\n",
    );
}

// ----- Atomic[T]: the generic atomic box -----

#[test]
fn atomic_construct_and_methods_ok() {
    // `Atomic(v)` infers its element type from the value (value-first, like `Shared`).
    entry_ok(
        "import std.concurrency\nfn main():\n    a := Atomic(0)\n    a.store(5)\n    n := a.add(1)\n    m := a.sub(2)\n    old := a.exchange(9)\n    ok := a.cas(9, 10)\n    print(a.load())\nmain()\n",
    );
}

#[test]
fn atomic_load_returns_element_type() {
    // `load()` yields `T`, so it composes where a `T` is expected (here, str concat).
    entry_ok(
        "import std.concurrency\nfn main():\n    a := Atomic(\"hi\")\n    msg := a.load() + \"!\"\n    print(msg)\nmain()\n",
    );
}

#[test]
fn atomic_cas_returns_bool() {
    // `cas(expected, new)` reports whether the swap happened.
    entry_ok(
        "import std.concurrency\nfn main():\n    a := Atomic(0)\n    if a.cas(0, 1):\n        print(\"swapped\")\nmain()\n",
    );
}

#[test]
fn atomic_store_wrong_type_rejected() {
    entry_rejects(
        "import std.concurrency\nfn main():\n    a := Atomic(0)\n    a.store(\"x\")\nmain()\n",
        "expected int",
    );
}

#[test]
fn atomic_add_non_numeric_rejected() {
    // `add`/`sub` are arithmetic — only `int`/`float` boxes have them.
    entry_rejects(
        "import std.concurrency\nfn main():\n    a := Atomic(\"x\")\n    a.add(1)\nmain()\n",
        "no method 'add'",
    );
}

#[test]
fn atomic_accepts_turbofish() {
    // `Atomic[T](v)` — turbofish optional (value-first); when present it pins the element type.
    entry_ok(
        "import std.concurrency\nfn main():\n    a := Atomic[int](0)\n    print(a.load())\nmain()\n",
    );
}

#[test]
fn concurrency_ctor_turbofish_checks_value() {
    // A turbofish element type that disagrees with the value's inferred type is rejected.
    entry_rejects(
        "import std.concurrency\nfn main():\n    s := Shared[str](0)\nmain()\n",
        "expected element type str, found int",
    );
    entry_rejects(
        "import std.concurrency\nfn main():\n    r := RwShared[str](0)\nmain()\n",
        "expected element type str, found int",
    );
    entry_rejects(
        "import std.concurrency\nfn main():\n    a := Atomic[str](0)\nmain()\n",
        "expected element type str, found int",
    );
}

#[test]
fn concurrency_ctor_turbofish_arity() {
    entry_rejects(
        "import std.concurrency\nfn main():\n    s := Shared[int, str](0)\nmain()\n",
        "Shared[T]() takes exactly one type argument",
    );
    entry_rejects(
        "import std.concurrency\nfn main():\n    r := RwShared[int, str](0)\nmain()\n",
        "RwShared[T]() takes exactly one type argument",
    );
    entry_rejects(
        "import std.concurrency\nfn main():\n    a := Atomic[int, str](0)\nmain()\n",
        "Atomic[T]() takes exactly one type argument",
    );
}

#[test]
fn atomic_is_sendable() {
    // An `Atomic[T]` handle crosses the airlock — both spawned tasks reach the same box.
    entry_ok(
        "import std.concurrency\nfn bump(a: Atomic[int]):\n    a.add(1)\nfn main():\n    a := Atomic(0)\n    parallel:\n        spawn bump(a)\n        spawn bump(a)\n    print(a.load())\nmain()\n",
    );
}

// ----- timer(ms): one-shot timeout channel -----

#[test]
fn timer_returns_channel_bool() {
    // `timer(ms)` yields a `Channel[bool]`; `recv()` on it composes where a `bool` is expected.
    // `timer` now requires `import std.time` (gated builtin) — see `timer_requires_import`.
    entry_ok(
        "import std.time\nfn main():\n    t := timer(50)\n    if t.recv():\n        print(\"fired\")\nmain()\n",
    );
}

#[test]
fn timer_arg_must_be_int() {
    entry_rejects(
        "import std.time\nfn main():\n    t := timer(\"x\")\n    print(t.recv())\nmain()\n",
        "expected int",
    );
}

#[test]
fn timer_requires_import() {
    // Bare `timer(ms)` with NO `import std.time` is an unknown-function error carrying the import hint.
    let errs = check_entry("fn main():\n    t := timer(50)\n    print(t.recv())\nmain()\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unknown function 'timer'")
                && e.message.contains("import std.time")),
        "expected unknown-function + import hint, got: {errs:?}"
    );
}

#[test]
fn timer_whole_module_import_ok() {
    // A whole-module `import std.time` licenses the bare `timer(ms)` builtin.
    entry_ok("import std.time\nfn main():\n    t := timer(50)\n    print(t.recv())\nmain()\n");
}

#[test]
fn timer_from_import_licenses_and_rejects_rename() {
    // A selective `import timer from std.time` licenses the bare `timer(ms)` call...
    entry_ok("import timer from std.time\nfn main():\n    print(timer(50).recv())\nmain()\n");
    // ...but the opcode-backed `timer` cannot be renamed on import (the runtime skip keys on the name).
    entry_rejects(
        "import timer as t2 from std.time\nfn main():\n    print(1)\nmain()\n",
        "cannot be renamed",
    );
}

#[test]
fn timer_still_reserved() {
    // The import gate (must-import-to-use) is SEPARATE from the reserved-name gate (can't-shadow):
    // `timer` STAYS reserved, so a user `struct timer` / `fn timer` is rejected even with the import.
    entry_rejects(
        "struct timer:\n    n: int\nfn main():\n    print(1)\nmain()\n",
        "reserved",
    );
    entry_rejects(
        "fn timer():\n    print(1)\nfn main():\n    print(1)\nmain()\n",
        "reserved",
    );
}

#[test]
fn import_alias_to_reserved_int_from_rejected() {
    // `import sqrt as int from std.math` silently rebinds the reserved builtin `int` callable:
    // at runtime the `int()` conversion wins and the `as int` binding is dead — a SILENT WRONG
    // RESULT. Aliasing an import TO a reserved builtin name must be rejected as `reserved (builtin)`,
    // exactly like `fn int()` / an extern named `int`.
    entry_rejects(
        "import sqrt as int from std.math\nfn main():\n    print(int(9.0))\nmain()\n",
        "reserved",
    );
}

#[test]
fn import_module_as_reserved_int_rejected() {
    // `import std.math as int` was accepted, then `int(9.0)` failed with the confusing
    // `module int is not callable`. Reject the alias-to-reserved binding up front.
    entry_rejects(
        "import std.math as int\nfn main():\n    print(int(9.0))\nmain()\n",
        "reserved",
    );
}

#[test]
fn import_alias_to_reserved_type_from_rejected() {
    // Finding B: `import who as Result from lib` silently rebound a reserved TYPE name (Result/
    // Option/Iterator/Socket/Listener/ptr/owned_str) — the alias guard only covered reserved
    // CALLABLE names, so the type-name case slipped through while `import who as int` was rejected.
    // Now symmetric with the decl-site guard (`is_reserved_type`): reject as `reserved (builtin)`.
    for ty in [
        "Result",
        "Option",
        "Iterator",
        "Socket",
        "Listener",
        "ptr",
        "owned_str",
    ] {
        entry_rejects(
            &format!("import sqrt as {ty} from std.math\nfn main():\n    print(1)\nmain()\n"),
            "reserved",
        );
    }
}

#[test]
fn import_module_as_reserved_type_rejected() {
    // Same asymmetry on the whole-module aliased path: `import std.math as Result` silently rebound
    // the reserved TYPE name. Reject it up front, mirroring `import std.math as int`.
    entry_rejects(
        "import std.math as Result\nfn main():\n    print(1)\nmain()\n",
        "reserved",
    );
}

#[test]
fn import_alias_nil_from_accepted() {
    // BOUNDARY (carve-out): `nil` is a shadowable value-builtin (`nil := 5` is accepted), NOT a type
    // name to reject as an alias target. The widened guard must exclude it — a naive
    // `|| is_reserved_type(a)` would over-reject since `is_reserved_type("nil")` is true.
    let t = TmpDir::new();
    t.write("lib.chz", "fn who() -> int:\n    return 1\n");
    let entry = t.write(
        "main.chz",
        "import who as nil from lib\nfn main():\n    print(1)\nmain()\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    // THIS guard must not reject `nil`; if some UNRELATED path errors, at least it's not `reserved`.
    assert!(
        !errs.iter().any(|e| e.message.contains("reserved")),
        "nil alias must not be rejected as reserved, got: {errs:?}"
    );
}

#[test]
fn import_alias_nonreserved_helper_from_accepted_and_usable() {
    // OVER-REJECT BOUNDARY: a fresh non-reserved alias must still bind and be usable (the widened
    // guard must not short-circuit the bind logic for a legit alias).
    let t = TmpDir::new();
    t.write("lib.chz", "fn who() -> int:\n    return 1\n");
    let entry = t.write(
        "main.chz",
        "import who as Helper from lib\nfn main():\n    print(Helper())\nmain()\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    assert!(
        check_graph(&graph).is_ok(),
        "non-reserved alias Helper must check clean and be usable"
    );
}

#[test]
fn reserved_name_local_shadow_still_ok() {
    // BOUNDARY: only the IMPORT-ALIAS binding target is gated. Value-level local shadowing of a
    // reserved name (Python-style) MUST still check clean — it goes through `declare`, not
    // `bind_import`.
    ok("range := 5\nprint(range)\n");
    ok("fn f(range: int) -> int:\n    return range\nprint(f(3))\n");
    // A reserved *member* imported UN-aliased (bind == member) must still pass — the `a != member`
    // guard keeps `import Shared from std.concurrency` legal.
    entry_ok(
        "import Shared from std.concurrency\nfn main():\n    s := Shared(0)\n    print(s.get())\nmain()\n",
    );
}

// ----- C5: the `Executor` escape hatch -----

#[test]
fn executor_construct_and_methods_ok() {
    entry_ok(
        "import std.concurrency\nfn job():\n    print(1)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): job())\n    ex.shutdown()\nmain()\n",
    );
}

#[test]
fn executor_shutdown_now_ok() {
    entry_ok(
        "import std.concurrency\nfn main():\n    ex := Executor()\n    ex.shutdown_now()\nmain()\n",
    );
}

#[test]
fn executor_defer_shutdown_ok() {
    entry_ok(
        "import std.concurrency\nfn job():\n    print(1)\nfn main():\n    ex := Executor()\n    defer ex.shutdown()\n    ex.submit(fn(): job())\nmain()\n",
    );
}

#[test]
fn executor_type_arg_rejected() {
    rejects(
        "fn main():\n    ex := Executor[int]()\nmain()\n",
        "takes no type arguments",
    );
}

#[test]
fn executor_unknown_method_rejected() {
    entry_rejects(
        "import std.concurrency\nfn main():\n    ex := Executor()\n    ex.run()\nmain()\n",
        "has no method 'run'",
    );
}

#[test]
fn executor_is_sendable() {
    // The handle crosses the airlock like Channel/Shared — submitting from a spawned task is legal.
    entry_ok(
        "import std.concurrency\nfn use_ex(ex: Executor):\n    ex.submit(fn(): print(1))\nfn main():\n    ex := Executor()\n    parallel:\n        spawn use_ex(ex)\n    ex.shutdown()\nmain()\n",
    );
}

#[test]
fn executor_user_struct_named_executor_rejected() {
    rejects(
        "struct Executor:\n    n: int\nfn main():\n    print(1)\nmain()\n",
        "reserved",
    );
}

// ----- Task 4: the four runtime concurrency ctors require `import std.concurrency` -----

#[test]
fn concurrency_types_require_import() {
    // Value-position ctor use without the import is an unknown-name error with the import hint.
    for (src, name) in [
        (
            "fn main():\n    s := Shared(0)\n    print(s.get())\nmain()\n",
            "Shared",
        ),
        (
            "fn main():\n    r := RwShared(0)\n    print(r.get())\nmain()\n",
            "RwShared",
        ),
        (
            "fn main():\n    a := Atomic(0)\n    print(a.load())\nmain()\n",
            "Atomic",
        ),
        (
            "fn main():\n    ex := Executor()\n    ex.shutdown()\nmain()\n",
            "Executor",
        ),
    ] {
        let errs = check_entry(src);
        assert!(
            errs.iter()
                .any(|e| e.message.contains(&format!("unknown type '{name}'"))
                    && e.message.contains("import std.concurrency")),
            "expected unknown-type+import hint for {name}, got: {errs:?}"
        );
    }
    // Type-position annotation use without the import is also an unknown-type error with the hint.
    for (src, name) in [
        ("fn f(s: Shared[int]):\n    print(s.get())\n", "Shared"),
        ("fn f(r: RwShared[int]):\n    print(r.get())\n", "RwShared"),
        ("fn f(a: Atomic[int]):\n    print(a.load())\n", "Atomic"),
        ("fn f(ex: Executor):\n    ex.shutdown()\n", "Executor"),
    ] {
        let errs = check_entry(src);
        assert!(
            errs.iter()
                .any(|e| e.message.contains(&format!("unknown type '{name}'"))
                    && e.message.contains("import std.concurrency")),
            "expected unknown-type+import hint (annotation) for {name}, got: {errs:?}"
        );
    }
}

#[test]
fn bare_concurrency_type_without_import_hints_import() {
    // A BARE (no type-arg) `Shared`/`RwShared`/`Atomic` annotation without the import must give the
    // SAME unknown-type+import-hint error the parameterized `Shared[T]` form gives — not a bare
    // hint-less "unknown type" (the catch-all regression).
    for (src, name) in [
        ("fn f(s: Shared):\n    print(1)\n", "Shared"),
        ("fn f(r: RwShared):\n    print(1)\n", "RwShared"),
        ("fn f(a: Atomic):\n    print(1)\n", "Atomic"),
    ] {
        let errs = check_entry(src);
        assert!(
            errs.iter()
                .any(|e| e.message.contains(&format!("unknown type '{name}'"))
                    && e.message.contains("import std.concurrency")),
            "expected unknown-type+import hint for bare {name}, got: {errs:?}"
        );
    }
}

#[test]
fn bare_concurrency_type_with_import_needs_type_arg() {
    // A BARE `Shared`/`RwShared`/`Atomic` annotation WITH the import is licensed but missing its
    // type argument — it must report the missing-type-arg error (matching the user-generic
    // precedent), NOT "unknown type".
    for (src, name) in [
        (
            "import std.concurrency\nfn f(s: Shared):\n    print(1)\nfn main():\n    print(1)\nmain()\n",
            "Shared",
        ),
        (
            "import std.concurrency\nfn f(r: RwShared):\n    print(1)\nfn main():\n    print(1)\nmain()\n",
            "RwShared",
        ),
        (
            "import std.concurrency\nfn f(a: Atomic):\n    print(1)\nfn main():\n    print(1)\nmain()\n",
            "Atomic",
        ),
    ] {
        let errs = check_entry(src);
        assert!(
            errs.iter().any(|e| e
                .message
                .contains(&format!("type '{name}' expects 1 type argument(s), got 0"))),
            "expected missing-type-arg error for licensed bare {name}, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|e| e.message.contains("unknown type")),
            "licensed bare {name} must not report unknown type, got: {errs:?}"
        );
    }
}

#[test]
fn type_param_named_like_reserved_type_rejected() {
    // ONE-WAY RATCHET: a reserved builtin TYPE name used as a generic type PARAMETER is rejected with
    // the same `type 'X' is reserved (builtin)` error `struct int` gives — it is NOT silently
    // shadowed. Pre-fix these all type-checked clean and shadowed kind-dependently (a scalar param =
    // dead/unreferenceable, a container/enum-builtin param = real shadowing generic). Covers the
    // bare-concurrency names (Shared/RwShared/Atomic), the license-gated names (Executor, ptr), and
    // the runtime-handle names (Socket, Listener, owned_str) — all in `is_reserved_type`.
    for (src, name) in [
        (
            "fn id[Shared](x: Shared) -> Shared:\n    return x\nfn main():\n    print(id(1))\nmain()\n",
            "Shared",
        ),
        (
            "fn id[RwShared](x: RwShared) -> RwShared:\n    return x\nfn main():\n    print(id(1))\nmain()\n",
            "RwShared",
        ),
        (
            "fn id[Atomic](x: Atomic) -> Atomic:\n    return x\nfn main():\n    print(id(1))\nmain()\n",
            "Atomic",
        ),
        (
            "fn id[Executor](x: Executor) -> Executor:\n    return x\nfn main():\n    print(id(1))\nmain()\n",
            "Executor",
        ),
        (
            "fn id[ptr](x: ptr) -> ptr:\n    return x\nfn main():\n    print(id(1))\nmain()\n",
            "ptr",
        ),
        (
            "fn id[Socket](x: Socket) -> Socket:\n    return x\nfn main():\n    print(id(1))\nmain()\n",
            "Socket",
        ),
        (
            "fn id[Listener](x: Listener) -> Listener:\n    return x\nfn main():\n    print(id(1))\nmain()\n",
            "Listener",
        ),
        (
            // Int arg (not a str literal) so this is a GENUINE red-before-green guard: pre-fix
            // `owned_str` hijacks to Ty::Str, making `id(1)` an int-vs-str error; the fix makes it a
            // free type param so `id(1)` type-checks. A str arg would pass in BOTH states (no guard).
            "fn id[owned_str](x: owned_str) -> owned_str:\n    return x\nfn main():\n    print(id(1))\nmain()\n",
            "owned_str",
        ),
    ] {
        let errs = check_entry(src);
        assert!(
            errs.iter()
                .any(|e| e.message.contains("reserved (builtin)")),
            "{name} as a type param must be rejected as reserved (builtin), got: {errs:?}"
        );
    }
}

#[test]
fn reserved_builtin_type_names_rejected_as_type_params() {
    // A reserved builtin type name used as a generic type-PARAMETER identifier must be rejected with
    // the same `type 'X' is reserved (builtin)` error `struct int` produces — across struct, enum,
    // newtype, free fn, struct method (its own `[U]`), protocol, AND the fixed-width FFI integer
    // names. Pre-fix every one of these type-checked clean (then shadowed kind-dependently). Uses the
    // real build_graph + check_graph entrypoint path (module-prefixed keys), guarding the CLI path.
    for (src, name) in [
        // struct, scalar param (dead/unreferenceable shadow pre-fix)
        ("struct Box[int]:\n    v: int\n", "int"),
        // struct, container builtin (real shadowing generic pre-fix)
        ("struct Box[List]:\n    v: int\n", "List"),
        // struct, enum builtin
        ("struct Box[Result]:\n    v: int\n", "Result"),
        // enum
        ("enum E[int]:\n    A\n", "int"),
        // newtype
        ("newtype N[List] = int\n", "List"),
        // free fn
        ("fn id[int](x: int) -> int:\n    return x\n", "int"),
        // struct method's OWN type param (covered at fn_sig, distinct from the struct's `[T]`)
        (
            "struct Box[T]:\n    v: T\n    fn get[str](self) -> T:\n        return self.v\n",
            "str",
        ),
        // protocol type param
        ("protocol P[int]:\n    fn f(self)\n", "int"),
        // FFI fixed-width name (reserved via native::ffi::TYPE_NAMES)
        ("struct Box[int32]:\n    v: int\n", "int32"),
    ] {
        entry_rejects(src, "reserved (builtin)");
        let _ = name;
    }
}

#[test]
fn reserved_typeparam_fix_does_not_overreject() {
    // BOUNDARY (must-NOT-over-reject): the reserved-type-param guard checks param NAMES only. A
    // normal `[T]`, multi-param `[K, V]`, a word param, a free generic fn, and a protocol-BOUNDED
    // param `[T: Comparable]` (the bound — not the param name — is the reserved protocol) must all
    // still type-check clean. Stays GREEN both before and after the fix.
    entry_ok("struct Box[T]:\n    v: T\n");
    entry_ok("struct Pair[K, V]:\n    k: K\n    v: V\n");
    entry_ok("struct Box[Item]:\n    v: Item\n");
    entry_ok("fn id[T](x: T) -> T:\n    return x\n");
    entry_ok("fn pick[T: Comparable](a: T, b: T) -> T:\n    return a\n");
}

#[test]
fn bare_reserved_type_without_typeparam_still_errors() {
    // The hoist of the `type_params` arm must NOT over-shadow a reserved/module name that is NOT an
    // in-scope type param: a bare annotation with no matching type param still emits its import hint
    // exactly as before. Guards the behavior-preserving half of the fix.
    let errs = check_entry("fn f(ex: Executor):\n    print(1)\nfn main():\n    print(1)\nmain()\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unknown type 'Executor'")
                && e.message.contains("import std.concurrency")),
        "bare Executor (no type param) must still emit the std.concurrency import hint, got: {errs:?}"
    );
    let errs = check_entry("fn f(p: ptr):\n    print(1)\nfn main():\n    print(1)\nmain()\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unknown type 'ptr'")
                && e.message.contains("import std.ffi")),
        "bare ptr (no type param) must still emit the std.ffi import hint, got: {errs:?}"
    );
    // Declaration-site reservedness: `struct Executor` rejected (reserved builtin); `struct Socket` is
    // ALSO reserved now (Hole A — the std.net handle name can't be shadowed by a user struct, same as
    // every other builtin TYPE name).
    let errs = check_entry("struct Executor:\n    x: int\nfn main():\n    print(1)\nmain()\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("Executor") && e.message.contains("reserved")),
        "struct Executor must stay reserved, got: {errs:?}"
    );
    let errs = check_entry("struct Socket:\n    x: int\nfn main():\n    print(1)\nmain()\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("Socket") && e.message.contains("reserved")),
        "struct Socket must be reserved, got: {errs:?}"
    );
}

#[test]
fn concurrency_whole_module_import_ok() {
    // A whole-module `import std.concurrency` licenses all four (value + annotation positions).
    entry_ok(
        "import std.concurrency\nfn main():\n    s := Shared(0)\n    r := RwShared(0)\n    a := Atomic(0)\n    ex := Executor()\n    print(s.get())\nmain()\n",
    );
    entry_ok(
        "import std.concurrency\nfn f(s: Shared[int], r: RwShared[int], a: Atomic[int], ex: Executor):\n    print(s.get())\nfn main():\n    print(1)\nmain()\n",
    );
}

#[test]
fn concurrency_from_import_licenses_named() {
    // A selective `import Shared from std.concurrency` licenses the named member.
    entry_ok(
        "import Shared from std.concurrency\nfn main():\n    print(Shared(0).get())\nmain()\n",
    );
    // But NOT the others it didn't import.
    let errs =
        check_entry("import Shared from std.concurrency\nfn main():\n    a := Atomic(0)\nmain()\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unknown type 'Atomic'")),
        "from-importing Shared must not license Atomic, got: {errs:?}"
    );
}

#[test]
fn concurrency_partial_import_collection_does_not_license() {
    // `import std.concurrency.collection` (len-3, the real file) must NOT license the four bare
    // ctors — only the whole-module `import std.concurrency` (len-2) does.
    let errs = check_entry(
        "import std.concurrency.collection\nfn main():\n    s := Shared(0)\n    print(s.get())\nmain()\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unknown type 'Shared'")),
        "importing the collection submodule must not license bare Shared, got: {errs:?}"
    );
}

#[test]
fn concurrency_names_still_reserved() {
    // CRITICAL FIX 1 — the four stay RESERVED names: a user cannot declare a struct over them even
    // after the import gate landed. This is a SEPARATE gate from the import requirement; both apply.
    for name in ["Shared", "RwShared", "Atomic", "AtomicInt", "Executor"] {
        rejects(
            &format!("struct {name}:\n    n: int\nfn main():\n    print(1)\nmain()\n"),
            "reserved",
        );
    }
}

#[test]
fn atomic_int_bare_unlicensed_errors() {
    // AtomicInt is import-gated: a bare `AtomicInt(0)` WITHOUT `import std.concurrency` must emit the
    // std.concurrency import hint (the reserved-name hole check's static half). Both the ctor call and
    // a bare type annotation must gate.
    let errs = check_entry("fn main():\n    a := AtomicInt(0)\n    print(a.load())\nmain()\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unknown type 'AtomicInt'")
                && e.message.contains("import std.concurrency")),
        "bare AtomicInt ctor (no import) must emit the std.concurrency import hint, got: {errs:?}"
    );
    let errs = check_entry("fn f(a: AtomicInt):\n    print(1)\nfn main():\n    print(1)\nmain()\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unknown type 'AtomicInt'")
                && e.message.contains("import std.concurrency")),
        "bare AtomicInt annotation (no import) must emit the std.concurrency import hint, got: {errs:?}"
    );
    // Licensed: `import std.concurrency` makes it resolve cleanly.
    let errs = check_entry(
        "import std.concurrency\nfn main():\n    a := AtomicInt(0)\n    a.add(1)\n    print(a.load())\nmain()\n",
    );
    assert!(
        errs.is_empty(),
        "licensed AtomicInt must check clean, got: {errs:?}"
    );
    // A non-int arg is rejected.
    let errs = check_entry(
        "import std.concurrency\nfn main():\n    a := AtomicInt(3.5)\n    print(a.load())\nmain()\n",
    );
    assert!(!errs.is_empty(), "AtomicInt(3.5) must be a type error");
}

// ----- namespace name-leak: builtin TYPE names reserved at declaration -----

#[test]
fn reserved_builtin_type_names_rejected_at_decl() {
    // HOLE A — every bare name `resolve_type` maps to a builtin (scalar / container / handle) must be
    // rejected at the DECLARATION site (`struct X` / `enum X`), not silently shadow the builtin at the
    // use-site. Previously only `is_reserved_type`'s 8 names were blocked, so `struct int` / `enum List`
    // / `struct Socket` type-checked clean then mis-resolved the use-site to the builtin.
    for name in [
        "int",
        "float",
        "bool",
        "str",
        "bytes",
        "bytearray",
        "nil",
        "List",
        "Set",
        "Map",
        "Channel",
        "range",
        "Socket",
        "Listener",
        "ptr",
        "owned_str",
    ] {
        let struct_src = format!("struct {name}:\n    x: int\nfn main():\n    print(1)\nmain()\n");
        let errs = check_entry(&struct_src);
        assert!(
            errs.iter()
                .any(|e| e.message.contains(name) && e.message.contains("reserved (builtin)")),
            "struct {name} must be rejected as reserved, got: {errs:?}"
        );
        let enum_src = format!("enum {name}:\n    A\nfn main():\n    print(1)\nmain()\n");
        let errs = check_entry(&enum_src);
        assert!(
            errs.iter()
                .any(|e| e.message.contains(name) && e.message.contains("reserved (builtin)")),
            "enum {name} must be rejected as reserved, got: {errs:?}"
        );
    }
    // An FFI fixed-width type name (`int32`) is reserved too (via TYPE_NAMES) — `struct int32` / `enum
    // int32` must be rejected, matching the NewType/TypeAlias guards.
    for src in [
        "struct int32:\n    x: int\nfn main():\n    print(1)\nmain()\n",
        "enum int32:\n    A\nfn main():\n    print(1)\nmain()\n",
    ] {
        let errs = check_entry(src);
        assert!(
            errs.iter()
                .any(|e| e.message.contains("int32") && e.message.contains("reserved (builtin)")),
            "int32 decl must be rejected as reserved, got: {errs:?}"
        );
    }
}

#[test]
fn protocol_named_reserved_type_rejected_at_decl() {
    // A `protocol X` whose name is a reserved builtin TYPE must be rejected at the DECL site, exactly
    // like `struct X` / `enum X`. Previously the protocol guard only checked `is_reserved_protocol`,
    // so `protocol List` / `protocol int` type-checked clean then surfaced as a self-contradictory
    // `type int does not satisfy int` when used as a generic bound.
    for name in [
        "int",
        "float",
        "bool",
        "str",
        "bytes",
        "bytearray",
        "nil",
        "List",
        "Set",
        "Map",
        "Channel",
        "range",
        "Result",
        "Option",
        "Socket",
        "Shared",
        "Executor",
        "Atomic",
    ] {
        let src = format!(
            "protocol {name}:\n    fn foo(self) -> int\nfn main():\n    print(1)\nmain()\n"
        );
        let errs = check_entry(&src);
        assert!(
            errs.iter()
                .any(|e| e.message.contains(name) && e.message.contains("reserved (builtin)")),
            "protocol {name} must be rejected as reserved, got: {errs:?}"
        );
    }
    // Boundary #2 — a NON-reserved protocol name still type-checks clean (guard is not over-broad).
    entry_ok("protocol Drawable:\n    fn draw(self)\nfn main():\n    print(1)\nmain()\n");
    entry_ok("protocol Eqz:\n    fn eqz(self) -> bool\nfn main():\n    print(1)\nmain()\n");
    // Boundary #3 — `Iterator` is BOTH a reserved protocol and a reserved type; the reserved-protocol
    // arm wins by ordering, so exactly ONE error fires, keeping the `protocol ...` wording (no double
    // diagnostic).
    let errs =
        check_entry("protocol Iterator:\n    fn next(self)\nfn main():\n    print(1)\nmain()\n");
    let reserved: Vec<_> = errs
        .iter()
        .filter(|e| e.message.contains("reserved (builtin)"))
        .collect();
    assert_eq!(
        reserved.len(),
        1,
        "protocol Iterator must give exactly one reserved error, got: {errs:?}"
    );
    assert!(
        reserved[0].message.contains("protocol"),
        "Iterator keeps protocol wording, got: {:?}",
        reserved[0].message
    );
}

#[test]
fn bare_net_type_without_import_hints_import() {
    // HOLE B — bare `Socket` / `Listener` annotations require `import std.net` (mirrors the
    // Executor/Shared/ptr gates). Without the import, the use must emit the import hint, not resolve
    // unconditionally to Ty::Socket/Ty::Listener.
    for (src, name) in [
        (
            "fn f(s: Socket):\n    print(1)\nfn main():\n    print(1)\nmain()\n",
            "Socket",
        ),
        (
            "fn f(l: Listener):\n    print(1)\nfn main():\n    print(1)\nmain()\n",
            "Listener",
        ),
    ] {
        let errs = check_entry(src);
        assert!(
            errs.iter()
                .any(|e| e.message.contains(&format!("unknown type '{name}'"))
                    && e.message.contains("import std.net")),
            "bare {name} without import must hint import std.net, got: {errs:?}"
        );
    }
}

#[test]
fn net_type_with_import_ok() {
    // A whole-module `import std.net` licenses both bare net handles in annotation position.
    entry_ok(
        "import std.net\nfn f(s: Socket, l: Listener):\n    print(1)\nfn main():\n    print(1)\nmain()\n",
    );
    // A selective per-name import licenses each named handle.
    entry_ok(
        "import Socket from std.net\nimport Listener from std.net\nfn f(s: Socket, l: Listener):\n    print(1)\nfn main():\n    print(1)\nmain()\n",
    );
}

#[test]
fn net_handle_bare_construction_rejected_after_import() {
    // Phase 4c-net regression — `Socket`/`Listener` now carry a `sig.struct_defs` entry (for their
    // harvested METHOD tables), but they are NOT constructible nominal structs. A whole-module
    // `import std.net` must NOT make bare `Socket(...)`/`Listener(...)` a from-nothing constructor
    // (a value comes only from `connect`/`listen`/`accept`). Guarded because the import struct-defs
    // loop otherwise seeds them into `struct_names`.
    for name in ["Socket", "Listener"] {
        let errs = check_entry(&format!(
            "import std.net\nfn main():\n    x := {name}()\n    print(1)\nmain()\n"
        ));
        assert!(
            !errs.is_empty(),
            "bare {name}() construction must be rejected after import std.net, got no errors"
        );
    }
}

#[test]
fn net_type_from_import_partial_does_not_license_other() {
    // `import Socket from std.net` licenses Socket but NOT Listener.
    let errs = check_entry(
        "import Socket from std.net\nfn f(l: Listener):\n    print(1)\nfn main():\n    print(1)\nmain()\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unknown type 'Listener'")),
        "from-importing Socket must not license Listener, got: {errs:?}"
    );
}

#[test]
fn net_type_rename_rejected() {
    // A net type carries no runtime value (the runtime resolves Ty::Socket directly), so renaming it
    // on import would bind nothing usable — reject the rename (mirrors the concurrency/timer gate).
    let errs = check_entry("import Socket as S from std.net\nfn main():\n    print(1)\nmain()\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("Socket") && e.message.contains("cannot be renamed")),
        "renaming a net type on import must be rejected, got: {errs:?}"
    );
}

// ----- C5 refinement #1: a non-sendable value merely *read* inside a `spawn:` block -----

#[test]
fn read_captured_capturefree_closure_in_spawn_block_ok() {
    // B3.3 (Task 2a): capturing a CAPTURE-FREE closure and calling it inside a task is now ACCEPTED —
    // `sendable(Func)` is permissive because a closure crosses the airlock BY VALUE (runs on both
    // engines). A closure that ITSELF captures a `ref` is a runtime-backstop concern (Task 2b); a
    // directly-captured `ref` is still rejected (`ref_binding_captured_in_spawn_rejected`).
    ok(
        "fn main():\n    g := fn() -> int: 1\n    parallel:\n        spawn:\n            print(g())\nmain()\n",
    );
}

#[test]
fn read_captured_int_in_spawn_block_ok() {
    // A sendable capture (int) gets its own copy — reading it freely is the whole point.
    ok("fn main():\n    n := 42\n    parallel:\n        spawn:\n            print(n)\nmain()\n");
}

#[test]
fn read_captured_channel_in_spawn_block_ok() {
    // A Channel handle is sendable — capturing and using it inside a task is fine.
    ok(
        "fn main():\n    ch := Channel[int]()\n    parallel:\n        spawn:\n            ch.send(1)\nmain()\n",
    );
}

#[test]
fn imported_module_used_in_spawn_block_ok() {
    // Regression: a whole-module import is bound at module scope (a global namespace resolvable in
    // every task, like a free function), not a per-task value capture — the read gate must not flag
    // it even though `Ty::Module` is non-sendable.
    entry_ok(
        "import std.math\nfn main():\n    parallel:\n        spawn:\n            print(math.floor(2.7))\nmain()\n",
    );
}

#[test]
fn top_level_closure_used_in_spawn_block_ok() {
    // Regression: a top-level (module-scope) binding is a global, not a per-task capture — reading
    // it inside a `spawn:` block is fine even when it's non-sendable.
    ok(
        "g := fn() -> int: 7\nfn main():\n    parallel:\n        spawn:\n            print(g())\nmain()\n",
    );
}

#[test]
fn read_captured_capturefree_closure_through_nested_closure_in_spawn_block_ok() {
    // B3.3 (Task 2a): a capture-free function-local closure reached through a NESTED closure inside a
    // `spawn:` block is now ACCEPTED — both closures are sendable (they cross by value; runs on both
    // engines). Previously rejected under the old "Func non-sendable" rule. The only remaining hole —
    // a nested closure that ITSELF captures a `ref` — is caught by the runtime backstop (Task 2b).
    ok(
        "fn main():\n    g := fn() -> int: 1\n    parallel:\n        spawn:\n            h := fn() -> int: g()\n            print(h())\nmain()\n",
    );
}

// ----- A3b (B3.6): Executor.submit gates its closure captures like `spawn` -----

#[test]
fn submit_protocol_capture_ok() {
    // Task 2 (option a): a submitted closure capturing a protocol-typed binding is now ACCEPTED — a
    // protocol existential is sendable (witness `Boxy` crosses by deep value copy to the pool thread).
    // (Was rejected under the old rule; the submit capture gate routes through `self.sendable()`.)
    entry_ok(
        "import std.concurrency\nprotocol NS:\n    fn tag(self) -> int\nstruct Boxy:\n    v: int\n    fn tag(self) -> int:\n        return self.v\nfn main():\n    p: NS = Boxy(0)\n    ex := Executor()\n    ex.submit(fn(): print(p.tag()))\n    ex.shutdown()\nmain()\n",
    );
}

#[test]
fn submit_capturefree_closure_ok() {
    // B3.3 (Task 2a): submitting a closure that captures a CAPTURE-FREE sibling closure is now
    // ACCEPTED — closures cross the airlock by value (runs on both engines). A submitted closure that
    // captures a `ref` is still rejected (`submit_non_sendable_capture_rejected`).
    entry_ok(
        "import std.concurrency\nfn main():\n    g := fn() -> int: 1\n    ex := Executor()\n    ex.submit(fn(): print(g()))\n    ex.shutdown()\nmain()\n",
    );
}

#[test]
fn submit_captured_channel_ok() {
    // A Channel handle is sendable — capturing it in a submitted task is fine.
    entry_ok(
        "import std.concurrency\nfn main():\n    ch := Channel[int]()\n    ex := Executor()\n    ex.submit(fn(): ch.send(1))\n    ex.shutdown()\nmain()\n",
    );
}

#[test]
fn submit_captured_int_ok() {
    // A sendable capture (int) gets its own copy — reading it in the task is the whole point.
    entry_ok(
        "import std.concurrency\nfn main():\n    n := 42\n    ex := Executor()\n    ex.submit(fn(): print(n))\n    ex.shutdown()\nmain()\n",
    );
}

#[test]
fn submit_capturefree_closure_through_nested_closure_ok() {
    // B3.3 (Task 2a): a capture-free closure reached through a NESTED closure inside a submitted task
    // is now ACCEPTED (both closures cross by value; runs on both engines). Previously rejected under
    // the old "Func non-sendable" rule; a nested closure that captures a `ref` is Task 2b's backstop.
    entry_ok(
        "import std.concurrency\nfn main():\n    g := fn() -> int: 1\n    ex := Executor()\n    ex.submit(fn(): print((fn() -> int: g())()))\n    ex.shutdown()\nmain()\n",
    );
}

#[test]
fn top_level_closure_submitted_ok() {
    // Regression pin (mirrors `top_level_closure_used_in_spawn_block_ok`): a module-scope binding is a
    // global, not a per-task capture — submitting a closure that reads it is fine even when it's
    // non-sendable (the `is_local_capture` scope-0 exclusion). Locks the intentional gap so a future
    // tightening of the gate can't silently flip it without a test failing.
    entry_ok(
        "import std.concurrency\ng := fn() -> int: 7\nfn main():\n    ex := Executor()\n    ex.submit(fn(): print(g()))\n    ex.shutdown()\nmain()\n",
    );
}

// ----- B3.3 (Task 2a): capture-sendability gate at the spawn CALLEE + ARG sites -----
// A closure/nested-fn value crosses the airlock BY VALUE (`sendable(Func)` is permissive), so the
// bare `fn` type type-checks; the per-closure capture check moves to the airlock SITES. A captured
// NON-SENDABLE LOCAL (a protocol existential etc.) at a spawn callee/arg is a clean compile error,
// matching the `spawn:` block form. A MODULE-GLOBAL non-sendable is a read-only global, NOT a
// capture — never gated. (The probe is a protocol existential — a genuinely non-sendable value.)

#[test]
fn protocol_local_captured_by_spawn_callee_closure_ok() {
    // Task 2 (option a): a function-local protocol-typed value captured by a closure VALUE that is the
    // spawn CALLEE is now ACCEPTED — a protocol existential is sendable (witness `Boxy` crosses by
    // deep value copy). (Was rejected under the old rule.)
    entry_ok(
        "protocol NS:\n    fn tag(self) -> int\nstruct Boxy:\n    v: int\n    fn tag(self) -> int:\n        return self.v\nfn main():\n    p: NS = Boxy(0)\n    grab := fn() -> int: p.tag()\n    parallel:\n        spawn grab()\nmain()\n",
    );
}

#[test]
fn module_global_nonsendable_spawn_callee_ok() {
    // CRITICAL NON-REGRESSION: a non-sendable value declared at MODULE scope is a read-only global
    // resolvable in every task (like a free fn), NOT a per-task local capture — the scope-0 exclusion
    // must keep it out of the gate even when a task READS it.
    entry_ok(
        "protocol NS:\n    fn tag(self) -> int\nstruct Boxy:\n    v: int\n    fn tag(self) -> int:\n        return self.v\ng: NS = Boxy(42)\nfn work(ch: Channel[int]):\n    ch.send(g.tag())\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn work(ch)\n    print(ch.recv())\nmain()\n",
    );
}

#[test]
fn spawn_callee_capturing_sendable_closure_ok() {
    // A callee whose captured environment holds a CAPTURE-FREE sibling closure (`double`) is sendable
    // (it crosses by value) — must NOT be gated (example #1, already runs; pin the check side).
    ok(
        "fn main():\n    double := fn(x: int) -> int: x * 2\n    ch := Channel[int]()\n    work := fn(): ch.send(double(21))\n    parallel:\n        spawn work()\n    print(ch.recv())\nmain()\n",
    );
}

#[test]
fn channel_of_closures_typechecks() {
    // `sendable(Func)` permissive → a `Channel[fn() -> int]` (closures as data) now type-checks; the
    // producer sends a capture-free closure and the consumer calls it (runs 42 — see parity test).
    ok(
        "fn producer(ch: Channel[fn() -> int]):\n    ch.send(fn() -> int: 42)\nfn main():\n    ch := Channel[fn() -> int]()\n    parallel:\n        spawn producer(ch)\n    f := ch.recv()\n    print(f())\nmain()\n",
    );
}

#[test]
fn closure_returned_across_task_typechecks() {
    // A capturing closure returned from a factory and sent over a `Channel[fn(int) -> int]` — the
    // closure carries a SENDABLE capture (an int `n`), so it crosses by value (runs 105 — parity test).
    ok(
        "fn adder(n: int) -> fn(int) -> int:\n    return fn(x: int) -> int: x + n\nfn producer(ch: Channel[fn(int) -> int]):\n    ch.send(adder(100))\nfn main():\n    ch := Channel[fn(int) -> int]()\n    parallel:\n        spawn producer(ch)\n    f := ch.recv()\n    print(f(5))\nmain()\n",
    );
}

// ----- G1 (B3.3b): module globals are read-only across tasks (`--parallel`) -----

#[test]
fn sequential_global_mutation_ok() {
    // Flow-scoped: the same mutation reached only from sequential (non-spawn) code stays legal.
    ok("n := 0\nfn bump():\n    n = n + 1\nfn main():\n    bump()\n    print(n)\nmain()\n");
}

#[test]
fn spawn_local_shadows_global_ok() {
    // A spawn-reachable function whose local shadows the global name mutates the LOCAL, not the
    // global — it must not be flagged.
    ok(
        "n := 0\nfn work():\n    n := 5\n    n = n + 1\n    print(n)\nfn main():\n    parallel:\n        spawn work()\nmain()\n",
    );
}

#[test]
fn spawn_reads_global_ok() {
    // Reading a (post-init constant) global from a task is fine; only mutation is gated.
    ok(
        "n := 7\nfn work():\n    print(n)\nfn main():\n    parallel:\n        spawn work()\nmain()\n",
    );
}

#[test]
fn shared_update_in_spawn_ok() {
    // The prescribed cross-task mutation path: a global `Shared`, mutated via `update()` in a task.
    entry_ok(
        "import std.concurrency\nc := Shared(0)\nfn bump():\n    c.update(fn(x): x + 1)\nfn main():\n    parallel:\n        spawn bump()\n    print(c.get())\nmain()\n",
    );
}

#[test]
fn spawn_callee_shadowed_by_local_ok() {
    // A local binding shadowing a free function's name at the spawn site means `spawn bump()`
    // targets the LOCAL (inert) closure, not the global-mutating free fn — must not be flagged.
    ok(
        "n := 0\nfn bump():\n    n = n + 1\nfn main():\n    bump := fn(): 1\n    parallel:\n        spawn bump()\nmain()\n",
    );
}

// `Ref` is now an ORDINARY identifier (the `ref` keyword and `Ref[T]` box were removed): a user
// `struct Ref` is legal, no longer reserved.
#[test]
fn user_struct_named_ref_is_legal() {
    entry_ok(
        "struct Ref:\n    val: int\nfn main():\n    r := Ref(5)\n    print(str(r.val))\nmain()\n",
    );
}

// ----- D6c: optional `timeout_ms` on net socket read/accept/write -----
// NOTE: bare `Socket`/`Listener` now require `import std.net` (Hole B), so these moved off the
// single-module `ok`/`rejects` helpers (which don't resolve native imports) onto the graph helpers
// `entry_ok`/`check_entry` with an `import std.net` prefix + a trivial `main` tail (the `use_*` fn is
// type-checked though never called). `check_entry` also desugars, so `?` lowers correctly.

#[test]
fn socket_read_with_timeout_type_checks() {
    // `read(n)` and `read(n, timeout_ms)` both type-check (the trailing int is optional).
    entry_ok(
        "import std.net\nfn use_sock(s: Socket) -> str!:\n    a := s.read(64)?\n    b := s.read(64, 100)?\n    return Ok(a + b)\nfn main():\n    print(1)\nmain()\n",
    );
}

#[test]
fn socket_write_with_timeout_type_checks() {
    entry_ok(
        "import std.net\nfn use_sock(s: Socket) -> int!:\n    a := s.write(\"x\")?\n    b := s.write(\"x\", 100)?\n    return Ok(a + b)\nfn main():\n    print(1)\nmain()\n",
    );
}

#[test]
fn listener_accept_with_timeout_type_checks() {
    // `accept()` and `accept(timeout_ms)` both type-check.
    entry_ok(
        "import std.net\nfn use_listener(l: Listener) -> int!:\n    l.accept()?\n    l.accept(100)?\n    return Ok(0)\nfn main():\n    print(1)\nmain()\n",
    );
}

/// Assert `check_entry(src)` reports `needle` AND did not leak an `unknown type` (i.e. the `import
/// std.net` licensed the handle, so the real method-arity/type error surfaces — not a swallowed
/// `Socket → Ty::Unknown` cascade).
fn check_entry_rejects_net(src: &str, needle: &str) {
    let errs = check_entry(src);
    assert!(
        errs.iter().any(|e| e.message.contains(needle)),
        "expected an error containing {needle:?}, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("unknown type")),
        "a leaked 'unknown type' means the net handle was not licensed, got: {errs:?}"
    );
}

#[test]
fn socket_read_with_non_int_timeout_rejected() {
    // A non-int `timeout_ms` is a type error.
    check_entry_rejects_net(
        "import std.net\nfn use_sock(s: Socket):\n    s.read(64, \"x\")\nfn main():\n    print(1)\nmain()\n",
        "expected int",
    );
}

#[test]
fn socket_read_with_too_few_args_rejected() {
    // `read()` (zero args) is below the 1–2 arg range.
    check_entry_rejects_net(
        "import std.net\nfn use_sock(s: Socket):\n    s.read()\nfn main():\n    print(1)\nmain()\n",
        "argument",
    );
}

#[test]
fn socket_read_with_too_many_args_rejected() {
    // `read(n, t, extra)` exceeds the 1–2 arg range.
    check_entry_rejects_net(
        "import std.net\nfn use_sock(s: Socket):\n    s.read(64, 100, 1)\nfn main():\n    print(1)\nmain()\n",
        "argument",
    );
}

#[test]
fn listener_accept_with_too_many_args_rejected() {
    check_entry_rejects_net(
        "import std.net\nfn use_listener(l: Listener):\n    l.accept(100, 1)\nfn main():\n    print(1)\nmain()\n",
        "argument",
    );
}

// ===== or-patterns + nested nullary =====

#[test]
fn or_pattern_mismatched_bindings_rejected() {
    // `Some(a) | None` — one alternative binds `a`, the other binds nothing.
    rejects(
        "o := Some(5)\nmatch o:\n    Some(a) | None: print(\"x\")\n",
        "must bind the same variables",
    );
}

#[test]
fn or_pattern_consistent_bindings_ok() {
    // Two enum variants whose single payload is the same type both bind `a`.
    ok("enum E:\n    A(int)\n    B(int)\ne := E.A(1)\nmatch e:\n    E.A(a) | E.B(a): print(a)\n");
}

#[test]
fn enum_or_pattern_exhaustive_without_wildcard() {
    // A 3-variant enum covered by a single or-pattern is exhaustive WITHOUT a `_`.
    ok(
        "enum Color:\n    Red\n    Green\n    Blue\nc := Color.Red\nmatch c:\n    Color.Red | Color.Green | Color.Blue: print(\"c\")\n",
    );
}

#[test]
fn bool_or_not_exhaustive() {
    // `true | false` does NOT close the bool domain — a `_` is still required (resolved decision:
    // one rule, no bool special-case). Asserts the existing exhaustiveness rule is preserved.
    rejects(
        "b := true\nmatch b:\n    true | false: print(\"b\")\n",
        "non-exhaustive",
    );
}

#[test]
fn or_pattern_with_wildcard_is_exhaustive() {
    // `1 | _` — the `_` alternative is irrefutable, so the or-pattern is irrefutable and closes
    // the int domain with no further arm. Regression guard: an or-pattern's irrefutability is
    // OR-of-alternatives (ANY irrefutable alt suffices), not AND-of-alternatives.
    ok("n := 3\nmatch n:\n    1 | _: print(\"x\")\n");
    ok("s := \"hi\"\nmatch s:\n    \"a\" | \"b\" | _: print(\"y\")\n");
    // Soundness guard: without an irrefutable alternative it is still non-exhaustive.
    rejects(
        "n := 3\nmatch n:\n    1 | 2: print(\"x\")\n",
        "non-exhaustive",
    );
}

#[test]
fn nested_nullary_wrong_type_rejected() {
    // `Some(None)` where the inner type is `int` — `None` is not a variant of int.
    rejects(
        "o := Some(5)\nmatch o:\n    Some(None): print(0)\n    _: print(1)\n",
        "not a variant of int",
    );
}

#[test]
fn non_nullary_variant_without_payload_rejected() {
    // A nested non-nullary variant used without its payload — `Some` requires `Some(...)`.
    rejects(
        "oo: Option[Option[int]] = Some(Some(3))\nmatch oo:\n    Some(Some): print(0)\n    _: print(1)\n",
        "requires its payload",
    );
}

#[test]
fn nested_nullary_correct_ok() {
    // `Some(None)` over `Option[Option[int]]` — the inner `None` is a nullary variant of the
    // inner Option type, a refutable variant match. (One `Some` arm + `_` to be exhaustive.)
    ok(
        "oo: Option[Option[int]] = Some(None)\nmatch oo:\n    Some(None): print(0)\n    _: print(-1)\n",
    );
}

// ===== extern / C-ABI FFI =====

#[test]
fn extern_call_typechecks() {
    // An extern fn's signature is hoisted; calling it type-checks like any named fn.
    ok("extern \"libm.so.6\":\n    fn cos(x: float) -> float\n\ny: float = cos(0.0)\nprint(y)\n");
}

#[test]
fn extern_call_int_into_float_param_widens() {
    // One-way int->float widening (matches std.math): `cos(2)` widens the int literal into the C
    // `double` param. Hole-free — the FFI host's `arg_float` promotes an int arg to f64 before
    // marshalling, so the C function receives `2.0`. A non-numeric arg (str/bool) is still rejected.
    ok("extern \"libm.so.6\":\n    fn cos(x: float) -> float\n\nprint(cos(2))\n");
}

#[test]
fn extern_str_param_and_int_return_ok() {
    ok(
        "extern \"libc.so.6\":\n    fn strlen(s: str) -> int\n\nn: int = strlen(\"hello\")\nprint(n)\n",
    );
}

#[test]
fn extern_void_return_ok() {
    ok("extern \"libc.so.6\":\n    fn srand(seed: int)\n\nsrand(1)\n");
}

#[test]
fn extern_non_marshallable_param_rejected() {
    rejects(
        "extern \"libc.so.6\":\n    fn f(xs: List[int]) -> int\n",
        "not C-marshallable",
    );
}

#[test]
fn extern_non_marshallable_return_rejected() {
    rejects(
        "extern \"libc.so.6\":\n    fn f(x: int) -> List[int]\n",
        "not C-marshallable",
    );
}

#[test]
fn extern_ptr_param_and_return_ok() {
    // The opaque `ptr` handle is C-marshallable: an extern fn can return one and take one back.
    // `ptr` requires `import std.ffi` (one import covers both the extern block and the annotation).
    entry_ok(
        "import std.ffi\nextern \"libc.so.6\":\n    fn tmpfile() -> ptr\n    fn fclose(f: ptr) -> int\n\nh: ptr = tmpfile()\nprint(fclose(h))\n",
    );
}

#[test]
fn ptr_annotation_requires_ffi_import() {
    // The opaque `ptr` type is NOT a global builtin: a bare `ptr` annotation without `import std.ffi`
    // is an unknown type, with an FFI-specific hint. (Consistent with the width types int8..uint64.)
    rejects(
        "extern \"libc.so.6\":\n    fn tmpfile() -> ptr\n",
        "unknown type 'ptr'",
    );
    rejects(
        "extern \"libc.so.6\":\n    fn tmpfile() -> ptr\n",
        "import std.ffi",
    );
}

/// Phase 4c leak-guard — harvesting std/ffi.chz transiently licenses `ptr`/`int8..uint64` into
/// `imported_ffi_types` (so `native fn null() -> ptr` resolves without `begin_module`). That transient
/// MUST be restored, or the license leaks into a sibling module that never imported std.ffi. A
/// per-name import (`import ptr from std.ffi`) must still license bare `ptr`, and a sibling that does
/// NOT import it must still reject bare `ptr` — even in the SAME graph where another module harvested it.
#[test]
fn ffi_ptr_license_does_not_leak_past_harvest() {
    // Per-name imports of the type-license names license the bare annotations.
    entry_ok(
        "import ptr from std.ffi\nextern \"libc.so.6\":\n    fn tmpfile() -> ptr\n\nh: ptr = tmpfile()\nprint(1)\n",
    );
    entry_ok("import int32 from std.ffi\ntype W = int32\nprint(1)\n");
    // A multi-module graph where a HELPER imports std.ffi (triggering the harvest + transient license)
    // and the ENTRY does NOT — the entry's bare `ptr` must still be rejected (no cross-module leak).
    let t = TmpDir::new();
    t.write(
        "helper.chz",
        "import std.ffi\nfn get() -> ptr:\n    return ffi.null()\n",
    );
    let entry = t.write(
        "main.chz",
        "import helper\nextern \"libc.so.6\":\n    fn tmpfile() -> ptr\n\nfn main():\n    print(1)\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unknown type 'ptr'")),
        "the entry never imported std.ffi, so bare `ptr` must reject despite the helper's harvest; got: {errs:?}"
    );
}

#[test]
fn ptr_whole_module_import_ok() {
    // A whole-module `import std.ffi` licenses bare `ptr` in BOTH extern signatures and annotations.
    entry_ok(
        "import std.ffi\nextern \"libc.so.6\":\n    fn tmpfile() -> ptr\n    fn fclose(f: ptr) -> int\n\nh: ptr = tmpfile()\nprint(fclose(h))\n",
    );
}

#[test]
fn ptr_selective_from_import_ok() {
    // `from std.ffi import ptr` (selective) also licenses bare `ptr`.
    entry_ok(
        "import ptr from std.ffi\nextern \"libc.so.6\":\n    fn tmpfile() -> ptr\n    fn fclose(f: ptr) -> int\n\nh: ptr = tmpfile()\nprint(fclose(h))\n",
    );
}

#[test]
fn ptr_rename_on_import_rejected() {
    // `ptr` CANNOT be renamed on import — the backends key off the literal surface name.
    entry_rejects("import ptr as P from std.ffi\n", "cannot be renamed");
}

#[test]
fn extern_callback_param_accepts_scalar_fn() {
    // A function-typed extern param whose params + return are all C scalars is a sync callback —
    // accepted with no error (callbacks #4). `apply(x: int, f: fn(int) -> int) -> int`.
    ok("extern \"libapply.so\":\n    fn apply(x: int, f: fn(int) -> int) -> int\n");
}

#[test]
fn extern_callback_param_accepts_float_scalar_fn() {
    ok("extern \"libapply.so\":\n    fn applyd(x: float, f: fn(float) -> float) -> float\n");
}

#[test]
fn extern_callback_nonscalar_param_part_rejected() {
    // A callback param whose own parameter is non-scalar (`str`) is not C-marshallable.
    rejects(
        "extern \"libapply.so\":\n    fn apply(f: fn(str) -> int) -> int\n",
        "not C-marshallable",
    );
}

#[test]
fn extern_callback_nonscalar_ret_part_rejected() {
    // A callback param whose return is non-scalar (`str`) is not C-marshallable.
    rejects(
        "extern \"libapply.so\":\n    fn apply(f: fn(int) -> str) -> int\n",
        "not C-marshallable",
    );
}

#[test]
fn extern_callback_nested_callback_rejected() {
    // A nested callback (a callback param taking a callback) is not supported (v1) — rejected.
    rejects(
        "extern \"libapply.so\":\n    fn apply(f: fn(fn(int) -> int) -> int) -> int\n",
        "not C-marshallable",
    );
}

#[test]
fn extern_callback_as_return_rejected() {
    // A callback is PARAM-ONLY: a function-typed RETURN is rejected (no C marshalling for a returned
    // function pointer in v1).
    rejects(
        "extern \"libapply.so\":\n    fn make() -> fn(int) -> int\n",
        "not C-marshallable",
    );
}

#[test]
fn extern_str_optional_return_is_marshallable() {
    // `str?` (Option[str]) is a valid RETURN type — the nullable opt-in. The program sees an
    // `Option[str]`, so it must be matched/`?`-handled, not used as a bare str.
    ok(
        "extern \"libc.so.6\":\n    fn getenv(s: str) -> str?\n\nmatch getenv(\"PATH\"):\n    Some(v): print(v)\n    None: print(\"unset\")\n",
    );
}

#[test]
fn extern_str_optional_param_rejected() {
    // `str?` is RETURN-ONLY; a `str?` parameter has no C marshalling and is rejected.
    rejects(
        "extern \"libc.so.6\":\n    fn f(x: str?) -> int\n",
        "not C-marshallable",
    );
}

#[test]
fn extern_owned_str_return_marshallable() {
    // `owned_str` is a RETURN-ONLY marshalling type that resolves to a plain `str` for the program
    // (the ownership/free is a runtime-only distinction). `strdup(str) -> owned_str` checks clean.
    ok(
        "extern \"libc.so.6\":\n    fn strdup(s: str) -> owned_str\n\ns: str = strdup(\"hi\")\nprint(s)\n",
    );
}

#[test]
fn extern_owned_str_param_rejected() {
    // `owned_str` is RETURN-ONLY — there is no owned-in (ownership transfer into C is unsupported).
    rejects(
        "extern \"libc.so.6\":\n    fn f(s: owned_str) -> int\n",
        "not C-marshallable",
    );
}

#[test]
fn extern_owned_str_param_via_alias_rejected() {
    // A transparent alias to `owned_str` must be rejected as a PARAM too — the surface guard has to
    // resolve alias chains, because the backends' `ctype_of` does and would otherwise emit
    // `CType::OwnedStr` for a param (a return-only CType), which the param loop cannot lower.
    rejects(
        "type O = owned_str\nextern \"libc.so.6\":\n    fn f(s: O) -> int\n",
        "not C-marshallable",
    );
}

#[test]
fn protocol_named_types_rejected_at_decl() {
    // The 16 prebuilt PROTOCOL names may be used as bounds (`[T: Comparable]`) but must NOT be
    // redeclared as a struct/enum/newtype/type alias — left ungated such a decl silently shadowed
    // the protocol and produced a self-contradictory diagnostic ("type Comparable does not satisfy
    // Comparable"). Mirrors the reserved-TYPE-name reservation (Result/Channel/...). The DECL of the
    // name is reserved; the protocol BOUND of the same name stays legal (see the boundary test).
    for name in [
        "Comparable",
        "Stringable",
        "Hashable",
        "Error",
        "Add",
        "Sub",
        "Mul",
        "Div",
        "Mod",
        "Neg",
        "Arithmetic",
        "Iterable",
        "Index",
        "IndexSet",
        "Slice",
        "Convert",
    ] {
        for src in [
            format!("struct {name}:\n    x: int\nfn main():\n    print(1)\nmain()\n"),
            format!("enum {name}:\n    A\nfn main():\n    print(1)\nmain()\n"),
            format!("newtype {name} = int\nfn main():\n    print(1)\nmain()\n"),
            format!("type {name} = int\nfn main():\n    print(1)\nmain()\n"),
        ] {
            let errs = check_entry(&src);
            assert!(
                errs.iter()
                    .any(|e| e.message.contains(name) && e.message.contains("reserved (builtin)")),
                "decl of protocol name {name} must be reserved, got: {errs:?}\nsrc: {src}"
            );
        }
    }
    // The literal repro from the task: a `struct Comparable` shadow used as a bound + ctor. Pre-fix
    // this mis-errored "type Comparable does not satisfy Comparable (missing method 'compare')".
    let errs = check_entry(
        "struct Comparable:\n    x: int\nfn pick[T: Comparable](a: T) -> T:\n    return a\nfn main():\n    c := Comparable(x=5)\n    print(pick(c).x)\nmain()\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("Comparable") && e.message.contains("reserved (builtin)")),
        "struct Comparable repro must reject as reserved, got: {errs:?}"
    );
}

#[test]
fn convert_bound_binds_and_arity_checked() {
    // Slice 1 of Convert/From: the reserved `Convert[S]` protocol binds as a generic bound. This checks
    // ONLY that the bound RESOLVES (no "unknown protocol") and is arity-checked against its 1 type param.
    // NOT tested here: that a concrete type SATISFIES it (witnessing = slice 2), nor `T.convert(..)`
    // through the bound (slice 3 — still errors "unknown name 'T'"). So `foo` stays UNCALLED — invoking
    // it would fire slice-2 conformance, which is intentionally not wired yet.
    entry_ok(
        "fn foo[T: Convert[int]](x: T) -> T:\n    return x\nfn main():\n    print(1)\nmain()\n",
    );
    // `[T: Convert]` — 0 args for a 1-param protocol → arity error (mirrors `Iterator[Elem]`).
    let errs = check_entry(
        "fn foo[T: Convert](x: T) -> T:\n    return x\nfn main():\n    print(1)\nmain()\n",
    );
    assert!(
        errs.iter().any(|e| e.message.contains("Convert")
            && e.message.contains("takes 1 type argument(s), found 0")),
        "[T: Convert] must be an arity error, got: {errs:?}"
    );
    // `[T: Convert[int, str]]` — 2 args for a 1-param protocol → arity error.
    let errs = check_entry(
        "fn foo[T: Convert[int, str]](x: T) -> T:\n    return x\nfn main():\n    print(1)\nmain()\n",
    );
    assert!(
        errs.iter().any(|e| e.message.contains("Convert")
            && e.message.contains("takes 1 type argument(s), found 2")),
        "[T: Convert[int, str]] must be an arity error, got: {errs:?}"
    );
}

#[test]
fn convert_witness_static_slot_only() {
    // Slice 2 of Convert/From — SOUND structural witnessing. A concrete type witnesses `Convert[int]`
    // (i.e. satisfies the bound `[T: Convert[int]]` at a call site) IFF it has a STATIC method
    // `convert(x: int) -> Self`. `use2` has a trivial body (does NOT call `T.convert` — that's slice 3),
    // so the bound-check runs at the call site then erases.
    // POSITIVE — a genuine static ctor witness.
    entry_ok(
        "struct Port:\n    n: int\n    fn convert(x: int) -> Port:\n        return Port(n=x)\nfn use2[T: Convert[int]](x: int) -> int:\n    return x\nfn main():\n    print(use2[Port](5))\nmain()\n",
    );
    // IMPOSTER 1 — self-only `convert(self) -> Self`: arity matches the static `convert(x: S)` slot (1
    // param each), so ONLY the `is_static` check rejects it. This is the exact value-model hole: a value
    // cannot invoke a static ctor. Pre-fix this FALSELY witnessed (self param → Unknown, compatible with
    // int, ret matches, is_static never compared).
    entry_rejects(
        "struct Bad:\n    n: int\n    fn convert(self) -> Bad:\n        return self\nfn use2[T: Convert[int]](x: int) -> int:\n    return x\nfn main():\n    print(use2[Bad](5))\nmain()\n",
        "does not satisfy Convert",
    );
    // IMPOSTER 2 — instance-slot `convert(self, x: int) -> Self` (arity 2 vs the static 1): rejected.
    entry_rejects(
        "struct Bad2:\n    n: int\n    fn convert(self, x: int) -> Bad2:\n        return self\nfn use2[T: Convert[int]](x: int) -> int:\n    return x\nfn main():\n    print(use2[Bad2](5))\nmain()\n",
        "does not satisfy Convert",
    );
    // IMPOSTER 3 — wrong source: static `convert(x: str)` under `Convert[int]` → rejected.
    entry_rejects(
        "struct Bad3:\n    n: int\n    fn convert(x: str) -> Bad3:\n        return Bad3(n=1)\nfn use2[T: Convert[int]](x: int) -> int:\n    return x\nfn main():\n    print(use2[Bad3](5))\nmain()\n",
        "does not satisfy Convert",
    );
    // IMPOSTER 4 — wrong return: static `convert(x: int) -> Other` (not Self) → rejected.
    entry_rejects(
        "struct Other:\n    z: int\nstruct Bad4:\n    n: int\n    fn convert(x: int) -> Other:\n        return Other(z=x)\nfn use2[T: Convert[int]](x: int) -> int:\n    return x\nfn main():\n    print(use2[Bad4](5))\nmain()\n",
        "does not satisfy Convert",
    );
    // IMPOSTER 5 — missing `convert` entirely → rejected.
    entry_rejects(
        "struct Bad5:\n    n: int\nfn use2[T: Convert[int]](x: int) -> int:\n    return x\nfn main():\n    print(use2[Bad5](5))\nmain()\n",
        "does not satisfy Convert",
    );
}

#[test]
fn convert_bound_only_not_value_type() {
    // Slice 2 — BOUND-ONLY enforcement. `Convert[S]` has a STATIC method requirement, so a VALUE cannot
    // witness it (a value can't invoke a static ctor). It is usable ONLY as a generic bound
    // `[T: Convert[S]]`, and REJECTED in every value-annotation position. The gate keys on the STRUCTURAL
    // property (protocol has a static-slot requirement), not the literal name.
    let needle = "has a static method";
    // param type
    entry_rejects(
        "fn takes(c: Convert[int]):\n    pass\nfn main():\n    pass\nmain()\n",
        needle,
    );
    // struct field
    entry_rejects(
        "struct S:\n    c: Convert[int]\nfn main():\n    pass\nmain()\n",
        needle,
    );
    // return type
    entry_rejects(
        "fn f() -> Convert[int]:\n    pass\nfn main():\n    pass\nmain()\n",
        needle,
    );
    // let-binding annotation
    entry_rejects(
        "struct Port:\n    n: int\n    fn convert(x: int) -> Port:\n        return Port(n=x)\nfn main():\n    x: Convert[int] = Port(n=1)\n    print(x)\nmain()\n",
        needle,
    );
    // bare protocol name (no args) used as a value type
    entry_rejects(
        "fn takes(c: Convert):\n    pass\nfn main():\n    pass\nmain()\n",
        needle,
    );
    // nested inside a container / Option / tuple / type alias — the resolve_type recursion must gate
    // every position, not just the top-level annotation.
    entry_rejects(
        "fn takes(c: List[Convert[int]]):\n    pass\nfn main():\n    pass\nmain()\n",
        needle,
    );
    entry_rejects(
        "fn takes(c: Option[Convert[int]]):\n    pass\nfn main():\n    pass\nmain()\n",
        needle,
    );
    entry_rejects(
        "fn takes(c: (int, Convert[int])):\n    pass\nfn main():\n    pass\nmain()\n",
        needle,
    );
    entry_rejects(
        "type A = Convert[int]\nfn takes(c: A):\n    pass\nfn main():\n    pass\nmain()\n",
        needle,
    );
    // KEEP-WORKING: the legal bound `[T: Convert[int]]` is UNAFFECTED (its args, not the protocol name,
    // flow through resolve_type).
    entry_ok(
        "fn foo[T: Convert[int]](x: T) -> T:\n    return x\nfn main():\n    print(1)\nmain()\n",
    );
}

#[test]
fn static_call_through_type_param_is_clear_error() {
    // Slice 3 CLOSE-OUT (Option A) — `T.convert(x)` (or any `T.<static>()`) on an in-scope generic type
    // parameter is NOT supported: generics are erased + the body is checked once abstractly, so there is
    // no concrete type to construct through at runtime (the "restricted construction" bet delivers nothing
    // under single-pass erased generics; deferred pending witness-passing). It must give a CLEAR
    // actionable error, not the cryptic "unknown name 'T'".
    let needle = "cannot call a static method through the generic type parameter";
    // `T.convert` — the Convert/From driver.
    entry_rejects(
        "struct Port:\n    n: int\n    fn convert(x: int) -> Port:\n        return Port(n=x)\nfn build[T: Convert[int]](x: int) -> T:\n    return T.convert(x)\nfn main():\n    print(build[Port](5).n)\nmain()\n",
        needle,
    );
    // The SAME gap for any generic static method (not Convert-specific): `T.empty()`.
    entry_rejects(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box(items=[])\nfn mk[T]() -> Box[T]:\n    return T.empty()\nfn main():\n    print(1)\nmain()\n",
        needle,
    );
    // KEEP-WORKING: a CONCRETE turbofish static call `Box[int].empty()` is fine (a real type, not a param).
    entry_ok(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box(items=[])\nfn main():\n    b := Box[int].empty()\n    print(1)\nmain()\n",
    );
}

#[test]
fn user_static_ctor_protocol_bound_only_and_instance_regression() {
    // The gate keys on the STRUCTURAL static-method property, so a USER static-ctor protocol is bound-only
    // too (closes the background spike: a static-ctor-only protocol used to FALSELY match a VALUE).
    entry_rejects(
        "protocol Ctor[S]:\n    fn make(x: S) -> Self\nstruct Port:\n    n: int\n    fn make(x: int) -> Port:\n        return Port(n=x)\nfn takes(c: Ctor[int]):\n    pass\nfn main():\n    pass\nmain()\n",
        "has a static method",
    );
    // REGRESSION GUARD — an ordinary INSTANCE-method parameterized protocol still works as a value
    // annotation (the bound-only gate must NOT over-reject it).
    entry_ok(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nfn takes2(c: Container[int]):\n    pass\nfn main():\n    pass\nmain()\n",
    );
}

#[test]
fn convert_embed_launders_static_requirement_bound_only() {
    // BUG 1 (adversarial): a protocol that EMBEDS a static-ctor protocol inherits the static requirement
    // transitively, so it too is bound-only — a VALUE still cannot witness the embedded static ctor.
    // Pre-fix `protocol_has_static_method` scanned only OWN methods, so `protocol MakeInt: Convert[int]`
    // (no own static method) returned false and was admitted as a value-annotation type, re-opening the
    // exact spike hole (a plain value inhabiting a static-ctor existential).
    entry_rejects(
        "protocol MakeInt:\n    Convert[int]\nstruct Port:\n    n: int\n    fn convert(x: int) -> Port:\n        return Port(n=x)\nfn takes(c: MakeInt) -> int:\n    return 0\nfn main():\n    p := Port(n=7)\n    print(takes(p))\nmain()\n",
        "has a static method",
    );
    // Two levels deep — a bundle embedding a bundle that embeds `Convert` is still gated.
    entry_rejects(
        "protocol MakeInt:\n    Convert[int]\nprotocol MakeInt2:\n    MakeInt\nfn takes(c: MakeInt2):\n    pass\nfn main():\n    pass\nmain()\n",
        "has a static method",
    );
    // REGRESSION — a bundle embedding ONLY an instance-method protocol stays a valid value type (the
    // transitive gate must not over-reject).
    entry_ok(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nprotocol Box2:\n    Container[int]\nfn takes(c: Box2):\n    pass\nfn main():\n    pass\nmain()\n",
    );
}

#[test]
fn convert_cross_module_alias_bound_only() {
    // BUG 2 (adversarial): a static-ctor protocol reaches VALUE position through a CROSS-MODULE type
    // alias whose body was computed by the read-only resolver (which cannot emit the gate). The
    // `import Foo from a` / `import a; a.Foo` consumers return the pre-resolved `Ty::Protocol('Convert',
    // [int])` DIRECTLY, so the mutable resolve_type arm gate never runs. Pre-fix both forms type-checked
    // a plain VALUE in a static-ctor annotation.
    let t = TmpDir::new();
    t.write("a.chz", "type Foo = Convert[int]\n");
    let check = |src: &str| -> Vec<CheckError> {
        let entry = t.write("main.chz", src);
        let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
        match check_graph(&graph) {
            Ok(()) => Vec::new(),
            Err(e) => e,
        }
    };
    // selective-import form (`import X from mod`)
    let errs = check(
        "import Foo from a\nstruct Port:\n    n: int\n    fn convert(x: int) -> Port:\n        return Port(n=x)\nfn takes(c: Foo):\n    pass\nfn main():\n    takes(Port(n=1))\nmain()\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("has a static method")),
        "selective-import alias to a static-ctor protocol must be bound-only, got: {errs:?}"
    );
    // qualified form `import a` + `a.Foo`
    let errs = check(
        "import a\nstruct Port:\n    n: int\n    fn convert(x: int) -> Port:\n        return Port(n=x)\nfn takes(c: a.Foo):\n    pass\nfn main():\n    takes(Port(n=1))\nmain()\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("has a static method")),
        "qualified alias to a static-ctor protocol must be bound-only, got: {errs:?}"
    );
    // nested — a cross-module alias body wrapping the protocol in a container must be gated too.
    t.write("c.chz", "type Bar = List[Convert[int]]\n");
    let errs =
        check("import Bar from c\nfn takes(c: Bar):\n    pass\nfn main():\n    pass\nmain()\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("has a static method")),
        "nested cross-module alias (List[Convert[int]]) must be bound-only, got: {errs:?}"
    );
}

#[test]
fn protocol_bound_and_typeparam_named_protocol_still_ok() {
    // Boundary: the FIX-A reservation must NOT leak into the protocol BOUND or a type PARAM named
    // like a protocol. (a) a user type that satisfies a prebuilt protocol used via its bound; and
    // (b) a type param spelled `Comparable` shadowing the protocol locally — both stay legal.
    entry_ok(
        "struct P:\n    v: int\n    fn compare(self, o: P) -> int:\n        return self.v - o.v\nfn pick[T: Comparable](a: T) -> T:\n    return a\nfn main():\n    print(pick(P(v=5)).compare(P(v=3)))\nmain()\n",
    );
    entry_ok(
        "fn id[Comparable](x: Comparable) -> Comparable:\n    return x\nfn main():\n    print(id(1))\nmain()\n",
    );
}

#[test]
fn bare_owned_str_outside_extern_rejected() {
    // FIX B: `owned_str` is a RETURN-ONLY extern marshalling form. Used as a bare type annotation
    // OUTSIDE an extern signature it silently collapsed to `str` with no import (its sibling `ptr`
    // correctly errors). Now rejected with a return-only hint.
    let errs = check_entry(
        "fn f(x: owned_str) -> owned_str:\n    return x\nfn main():\n    print(f(\"hi\"))\nmain()\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("owned_str") && e.message.contains("return-only")),
        "bare non-extern owned_str must be rejected, got: {errs:?}"
    );
}

#[test]
fn extern_owned_str_return_still_ok_no_import() {
    // FIX B boundary on the graph path: an extern `owned_str` RETURN still type-checks with NO
    // import (the context flag licenses it only inside the extern signature). Locks 7186 on the
    // real CLI (build_graph + check_graph) path, not just the single-module `ok()` helper.
    entry_ok(
        "extern \"libc.so.6\":\n    fn strdup(s: str) -> owned_str\n\nfn main():\n    s: str = strdup(\"hi\")\n    print(s)\nmain()\n",
    );
}

/// The checker is the true marshallability gate for a module-QUALIFIED extern type. A `mod.Struct`
/// whose field is non-marshallable (`List[int]`) must be rejected at `check` time (the compiler's
/// graceful backstop is only for paths that bypass the checker). Guards that resolving `Qualified`
/// → struct still runs `assert_marshallable`. Uses a two-file graph so `bag.Bag` carries an
/// identity key.
#[test]
fn extern_qualified_non_marshallable_struct_rejected() {
    let t = TmpDir::new();
    t.write("bag.chz", "struct Bag:\n    items: List[int]\n");
    let entry = t.write(
        "main.chz",
        "import bag\n\nextern \"libc.so.6\":\n    fn use_it(b: bag.Bag) -> int\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(
        errs.iter()
            .any(|e| e.message.contains("not C-marshallable")),
        "expected a marshallability error, got: {errs:?}"
    );
}

/// A module-QUALIFIED struct/width-alias used at an extern boundary type-checks (the qualified
/// spelling is now first-class at the FFI boundary, like the named-import spelling). Guards the
/// checker accepts the valid forms the lowering fix supports.
#[test]
fn extern_qualified_struct_and_alias_typecheck() {
    let t = TmpDir::new();
    t.write(
        "cdefs.chz",
        "import int32 from std.ffi\n\nstruct DivT:\n    quot: int32\n    rem: int32\n\ntype Len = int32\n",
    );
    let entry = t.write(
        "main.chz",
        "import cdefs\nimport int32 from std.ffi\n\nextern \"libc.so.6\":\n    \
         fn div(numer: int32, denom: int32) -> cdefs.DivT\n    fn abs(n: cdefs.Len) -> cdefs.Len\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
}

/// FFI ROOT FIX (fix4) — the dual-resolver-drift guard: `resolve_extern_signatures` must produce the
/// exact width-bearing `CType` per param/return, resolved in the DEFINING module's scope, for EVERY
/// alias spelling. These assert the actual `CType` (not just that `check` passes), so a divergence
/// between `resolve_ctype` and `resolve_type` (which `check` exercises) is caught here, not silently
/// at runtime. The entry module is last in graph order; its key is `(graph.modules.len()-1, fn)`.
mod resolve_extern_ctype {
    use super::*;
    use crate::native::cffi::CType;

    /// Resolve the entry module's extern table and return one fn's `(params, ret)`.
    fn entry_sig(files: &[(&str, &str)]) -> crate::checker::ExternCSig {
        let t = TmpDir::new();
        let mut entry = None;
        for (rel, src) in files {
            if let Some(parent) = std::path::Path::new(rel).parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(t.0.join(parent)).unwrap();
            }
            let p = t.write(rel, src);
            if *rel == "main.chz" {
                entry = Some(p);
            }
        }
        let graph = crate::resolver::build_graph(&entry.expect("a main.chz"))
            .expect("resolve should succeed");
        let table = crate::checker::resolve_extern_signatures(&graph);
        let last = graph.modules.len() - 1;
        // Each test below declares exactly one extern fn named `f` (or `abs`) in main.
        table
            .iter()
            .find(|((idx, _), _)| *idx == last)
            .map(|(_, s)| s.clone())
            .expect("entry module has an extern fn signature")
    }

    #[test]
    fn single_hop_width_alias_resolves_to_width() {
        // `type Len = int32` (same file) → int32, not collapsed to Int.
        let s = entry_sig(&[(
            "main.chz",
            "import int32 from std.ffi\n\ntype Len = int32\n\nextern \"libc.so.6\":\n    fn f(n: Len) -> Len\n",
        )]);
        assert_eq!(s.params, vec![Some(CType::Int32)]);
        assert_eq!(s.ret, Some(CType::Int32));
    }

    #[test]
    fn named_import_hop_resolves_to_defining_width_not_local_collision() {
        // main declares a colliding `type W = int8`; the true chain is `w3.Len -> W(from widths) ->
        // int64`. The resolved CType must be int64 (the named-import hop's defining width), NOT int8.
        let s = entry_sig(&[
            (
                "core/widths.chz",
                "import int64 from std.ffi\n\ntype W = int64\n",
            ),
            ("core/w3.chz", "import W from core.widths\n\ntype Len = W\n"),
            (
                "main.chz",
                "import core.w3\nimport int8 from std.ffi\n\ntype W = int8\n\nextern \"libc.so.6\":\n    fn f(n: w3.Len) -> w3.Len\n",
            ),
        ]);
        assert_eq!(s.params, vec![Some(CType::Int64)]);
        assert_eq!(s.ret, Some(CType::Int64));
    }

    #[test]
    fn qualified_return_struct_preserves_field_widths_and_identity_key() {
        // `cdefs.DivT` (two int32 fields) → CType::Struct keyed by the identity key, each field int32
        // (NOT collapsed to Int — the by-value struct layout depends on the real width).
        let s = entry_sig(&[
            (
                "cdefs.chz",
                "import int32 from std.ffi\n\nstruct DivT:\n    quot: int32\n    rem: int32\n",
            ),
            (
                "main.chz",
                "import cdefs\nimport int32 from std.ffi\n\nextern \"libc.so.6\":\n    fn f(numer: int32, denom: int32) -> cdefs.DivT\n",
            ),
        ]);
        assert_eq!(s.params, vec![Some(CType::Int32), Some(CType::Int32)]);
        match s.ret {
            Some(CType::Struct {
                name,
                field_names,
                fields,
            }) => {
                assert!(name.ends_with("::DivT"), "identity key, got {name}");
                assert_eq!(field_names, vec!["quot".to_string(), "rem".to_string()]);
                assert_eq!(fields, vec![CType::Int32, CType::Int32]);
            }
            other => panic!("expected a struct return, got {other:?}"),
        }
    }

    #[test]
    fn qualified_return_struct_with_aliased_field_resolves_field_width() {
        // REGRESSION (fix4): a qualified return struct whose FIELDS are typed via the DEFINING
        // module's LOCAL alias (`type Half = int32`). On fix4 the importer's scope (where `Half` is
        // invisible) was used to resolve the fields → field None → struct CType None → void return.
        // The single-resolver fix computes the struct's CType in ITS defining module's scope, so each
        // field keeps its true int32 width.
        let s = entry_sig(&[
            (
                "core/cdefs.chz",
                "import int32 from std.ffi\n\ntype Half = int32\n\nstruct DivT:\n    quot: Half\n    rem: Half\n",
            ),
            (
                "main.chz",
                "import core.cdefs\nimport int32 from std.ffi\n\nextern \"libc.so.6\":\n    fn f(numer: int32, denom: int32) -> cdefs.DivT\n",
            ),
        ]);
        assert_eq!(s.params, vec![Some(CType::Int32), Some(CType::Int32)]);
        match s.ret {
            Some(CType::Struct {
                name,
                field_names,
                fields,
            }) => {
                assert!(name.ends_with("::DivT"), "identity key, got {name}");
                assert_eq!(field_names, vec!["quot".to_string(), "rem".to_string()]);
                assert_eq!(fields, vec![CType::Int32, CType::Int32]);
            }
            other => panic!("expected a struct return, got {other:?}"),
        }
    }

    #[test]
    fn struct_field_named_import_qualified_and_nested_defining_width_wins() {
        // Module A's struct has three fields whose types resolve ONLY in A's defining scope:
        //   * `w` via a NAMED-IMPORTED alias (`import W from widths`, widths: `type W = int32`),
        //   * `w2` via a QUALIFIED `widths.W2` (`type W2 = int16`),
        //   * `n` whose type is a struct from a THIRD module (`mods.Inner`, an int8 field) — nested.
        // main declares COLLIDING `type W = uint64`/`type W2 = uint64` and a colliding `struct Inner`.
        // Each field's CType must be the DEFINING width, never main's collisions.
        let s = entry_sig(&[
            (
                "core/widths.chz",
                "import int32 from std.ffi\nimport int16 from std.ffi\n\ntype W = int32\ntype W2 = int16\n",
            ),
            (
                "core/mods.chz",
                "import int8 from std.ffi\n\nstruct Inner:\n    b: int8\n",
            ),
            (
                "core/a.chz",
                "import W from core.widths\nimport core.widths\nimport core.mods\n\nstruct Outer:\n    w: W\n    w2: widths.W2\n    n: mods.Inner\n",
            ),
            (
                "main.chz",
                "import core.a\nimport uint64 from std.ffi\nimport int8 from std.ffi\n\ntype W = uint64\ntype W2 = uint64\n\nstruct Inner:\n    z: uint64\n\nextern \"libc.so.6\":\n    fn f() -> a.Outer\n",
            ),
        ]);
        match s.ret {
            Some(CType::Struct {
                name,
                field_names,
                fields,
            }) => {
                assert!(name.ends_with("::Outer"), "identity key, got {name}");
                assert_eq!(
                    field_names,
                    vec!["w".to_string(), "w2".to_string(), "n".to_string()]
                );
                assert_eq!(fields[0], CType::Int32, "named-import alias W -> int32");
                assert_eq!(fields[1], CType::Int16, "qualified widths.W2 -> int16");
                match &fields[2] {
                    CType::Struct {
                        name: inner,
                        fields: ifields,
                        ..
                    } => {
                        assert!(
                            inner.ends_with("::Inner"),
                            "nested identity key, got {inner}"
                        );
                        assert_eq!(ifields, &vec![CType::Int8], "nested Inner.b -> int8");
                    }
                    other => panic!("expected nested struct field, got {other:?}"),
                }
            }
            other => panic!("expected a struct return, got {other:?}"),
        }
    }

    #[test]
    fn local_chain_resolves_through_every_hop() {
        // `type Len = A; type A = B; type B = int64` (same file) → int64 (each hop in this scope).
        let s = entry_sig(&[(
            "main.chz",
            "import int64 from std.ffi\n\ntype B = int64\ntype A = B\ntype Len = A\n\nextern \"libc.so.6\":\n    fn f(n: Len) -> Len\n",
        )]);
        assert_eq!(s.params, vec![Some(CType::Int64)]);
        assert_eq!(s.ret, Some(CType::Int64));
    }

    #[test]
    fn cyclic_alias_resolves_to_none_no_overflow() {
        // `type A = B; type B = A` (no leaf) → None (the depth guard terminates, no stack overflow).
        // `check` rejects this separately; here we assert the width carrier cleanly yields None.
        let s = entry_sig(&[(
            "main.chz",
            "type A = B\ntype B = A\n\nextern \"libc.so.6\":\n    fn f(n: A) -> A\n",
        )]);
        assert_eq!(s.params, vec![None]);
        assert_eq!(s.ret, None);
    }

    #[test]
    fn plain_scalars_and_void_return() {
        // Plain scalars resolve directly; a missing return annotation is void (`None`).
        let s = entry_sig(&[(
            "main.chz",
            "extern \"libc.so.6\":\n    fn f(a: int, b: float, c: bool, s: str, p: ptr)\n",
        )]);
        assert_eq!(
            s.params,
            vec![
                Some(CType::Int),
                Some(CType::Float),
                Some(CType::Bool),
                Some(CType::Str),
                Some(CType::Ptr),
            ]
        );
        assert_eq!(s.ret, None);
    }

    #[test]
    fn return_only_owned_and_nullable_str() {
        // `owned_str` → OwnedStr; `str?` → OptStr; `owned_str?` → OptOwnedStr (return-only forms).
        let s = entry_sig(&[(
            "main.chz",
            "extern \"libc.so.6\":\n    fn f() -> owned_str\n",
        )]);
        assert_eq!(s.ret, Some(CType::OwnedStr));
        let s2 = entry_sig(&[("main.chz", "extern \"libc.so.6\":\n    fn f() -> str?\n")]);
        assert_eq!(s2.ret, Some(CType::OptStr));
        let s3 = entry_sig(&[(
            "main.chz",
            "extern \"libc.so.6\":\n    fn f() -> owned_str?\n",
        )]);
        assert_eq!(s3.ret, Some(CType::OptOwnedStr));
    }
}

#[test]
fn extern_cyclic_option_alias_param_no_overflow() {
    // A cyclic alias routed through an `Option`/`?` form (`type A = A?`) must be diagnosed as a
    // recursive alias, NOT overflow the stack: the return-only guard's `seen` set has to span the
    // `Named`→`Option`→`Named` recursion boundary. (Regression: an earlier per-loop guard restarted
    // empty across that boundary and recursed forever.)
    rejects(
        "type A = A?\nextern \"libc.so.6\":\n    fn foo(x: A) -> int\n",
        "recursive type alias",
    );
}

#[test]
fn extern_owned_nullable_str_return_marshallable() {
    // `owned_str?` composes owned + nullable: program sees `Option[str]`, runtime frees + nulls.
    ok(
        "extern \"lib\":\n    fn g(s: str) -> owned_str?\n\nmatch g(\"x\"):\n    Some(v): print(v)\n    None: print(\"none\")\n",
    );
}

#[test]
fn extern_fixed_width_int_param_and_return_ok() {
    // Each fixed-width int marshalling name (int8..uint64) resolves to a plain `int` (`Ty::Int`) and
    // is BIDIRECTIONAL — valid as BOTH a param and a return. abs/atoi are stand-ins; the point is the
    // type-checker accepts the name in both positions and the program sees an `int`. The width names
    // are NOT global builtins — each module that names one must `import <name> from std.ffi` first.
    for name in [
        "int8", "int16", "int32", "int64", "uint8", "uint16", "uint32", "uint64",
    ] {
        entry_ok(&format!(
            "import {name} from std.ffi\nextern \"libc.so.6\":\n    fn f(x: {name}) -> {name}\n\nn: int = f(5)\nprint(n)\n"
        ));
    }
}

#[test]
fn ffi_deref_load_sigs_typecheck() {
    // The memory deref builtins resolve through the `std.ffi` module member dispatch: a load of an int
    // width returns `int`, a float width returns `float`, bool/ptr/str their kind. `ffi.null()` gives a
    // ptr value to feed them (no extern needed for type-checking).
    entry_ok(
        "import std.ffi\nfn main():\n    p := ffi.null()\n    n: int = ffi.load_int(p)\n    print(n)\n",
    );
    entry_ok(
        "import std.ffi\nfn main():\n    p := ffi.null()\n    x: int = ffi.load_int32_at(p, 8)\n    print(x)\n",
    );
    entry_ok(
        "import std.ffi\nfn main():\n    p := ffi.null()\n    f: float = ffi.load_float(p)\n    print(f)\n",
    );
    entry_ok(
        "import std.ffi\nfn main():\n    p := ffi.null()\n    g: float = ffi.load_float32_at(p, 4)\n    print(g)\n",
    );
    entry_ok(
        "import std.ffi\nfn main():\n    p := ffi.null()\n    b: bool = ffi.load_bool(p)\n    print(b)\n",
    );
    entry_ok(
        "import std.ffi\nfn main():\n    p := ffi.null()\n    q: ptr = ffi.load_ptr(p)\n    print(ffi.is_null(q))\n",
    );
    entry_ok(
        "import std.ffi\nfn main():\n    p := ffi.null()\n    s: str = ffi.load_str_at(p, 2)\n    print(s)\n",
    );
}

#[test]
fn ffi_deref_store_returns_nil_and_takes_matching_value() {
    entry_ok(
        "import std.ffi\nfn main():\n    p := ffi.null()\n    ffi.store_int(p, 42)\n    ffi.store_int32_at(p, 8, -7)\n    ffi.store_float(p, 2.5)\n    ffi.store_bool(p, true)\n    ffi.store_ptr_at(p, 0, p)\n",
    );
}

#[test]
fn ffi_alloc_layer_typechecks() {
    // The C-buffer alloc layer: alloc/alloc_zeroed take an int and return a ptr; free takes a ptr
    // and returns nil. `defer ffi.free(p)` is the manual-free idiom.
    entry_ok(
        "import std.ffi\nfn main():\n    p := ffi.alloc(64)\n    q := ffi.alloc_zeroed(32)\n    ffi.store_int64_at(p, 0, 7)\n    n: int = ffi.load_int64_at(p, 0)\n    print(n)\n    ffi.free(p)\n    ffi.free(q)\n",
    );
    // alloc returns a ptr (usable where a ptr is expected, e.g. is_null).
    entry_ok(
        "import std.ffi\nfn main():\n    p: ptr = ffi.alloc(8)\n    print(ffi.is_null(p))\n    ffi.free(p)\n",
    );
    // free returns nil — using it as an int is a type error.
    entry_rejects(
        "import std.ffi\nfn main():\n    p := ffi.alloc(8)\n    n: int = ffi.free(p)\n    print(n)\n",
        "nil",
    );
    // alloc's arg is an int; passing a str is a type error.
    entry_rejects(
        "import std.ffi\nfn main():\n    print(ffi.is_null(ffi.alloc(\"x\")))\n",
        "alloc",
    );
}

#[test]
fn ffi_deref_wrong_arg_type_rejected() {
    // load_int's param is `ptr`; passing an int is a type error.
    entry_rejects(
        "import std.ffi\nfn main():\n    print(ffi.load_int(5))\n",
        "load_int",
    );
    // store_int's value must be an int; a str value is rejected.
    entry_rejects(
        "import std.ffi\nfn main():\n    p := ffi.null()\n    ffi.store_int(p, \"x\")\n",
        "store_int",
    );
    // store returns nil — using it as an int is a type error.
    entry_rejects(
        "import std.ffi\nfn main():\n    p := ffi.null()\n    n: int = ffi.store_int(p, 1)\n    print(n)\n",
        "nil",
    );
}

#[test]
fn extern_fixed_width_int_via_alias_ok() {
    // A transparent `type Len = int32` used in an extern sig must behave identically to bare `int32`
    // (the alias trap from the prior FFI task): resolve_type maps Len -> int32 -> Ty::Int (program
    // sees a plain int), and the backends' ctype_of resolves the alias one hop to the int32 leaf. The
    // alias's target `int32` is resolved in THIS module, so it only works because int32 is imported
    // here — alias coherence (hazard a): a width alias resolves iff the width name is visible.
    entry_ok(
        "import int32 from std.ffi\ntype Len = int32\nextern \"libc.so.6\":\n    fn f(x: Len) -> Len\n\nn: int = f(5)\nprint(n)\n",
    );
}

#[test]
fn extern_cyclic_int_alias_no_overflow() {
    // A cyclic alias (`type A = B`, `type B = A`) used in an extern sig must be diagnosed as a
    // recursive type alias, NOT stack-overflow resolve_type/ctype_of. Adding the fixed-width leaf
    // names introduces no new recursion; the checker rejects the cycle before any extern body runs.
    rejects(
        "type A = B\ntype B = A\nextern \"libc.so.6\":\n    fn f(x: A) -> int\n",
        "recursive type alias",
    );
}

#[test]
fn extern_struct_param_and_return_typecheck() {
    // A flat-scalar struct is C-marshallable BY VALUE as both a param and a return. `Point` (two
    // int fields) and `Mixed` (an int32 + a float field) both type-check in an extern signature. The
    // int32 field name requires this module to `import int32 from std.ffi`.
    entry_ok(
        "import int32 from std.ffi\nstruct Point:\n    x: int\n    y: int\n\
         \nstruct Mixed:\n    a: int32\n    b: float\n\
         \nextern \"libc.so.6\":\n    fn id_point(p: Point) -> Point\n    fn id_mixed(m: Mixed) -> Mixed\n",
    );
}

#[test]
fn extern_struct_param_forward_ref_typechecks() {
    // A by-value struct used in an extern signature may be DECLARED AFTER the extern block — extern
    // marshallability is validated in a post-hoist sweep, once every struct's field info is
    // registered. (Regression: it was validated inline in source order, so a forward reference fell
    // through to a spurious "not C-marshallable" rejection.) Mirrors how a plain `fn` forward-refs.
    ok(
        "extern \"libc.so.6\":\n    fn id_point(p: Point) -> Point\n\
         \nstruct Point:\n    x: int\n    y: int\n",
    );
}

#[test]
fn extern_struct_with_str_field_is_rejected() {
    // A struct with a `str` field is NOT C-marshallable by value (v1 flat-scalar limit) — reject it
    // with a message naming the struct AND the offending field.
    rejects(
        "struct Bad:\n    name: str\n    age: int\n\
         \nextern \"libc.so.6\":\n    fn f(b: Bad) -> int\n",
        "field 'name'",
    );
}

#[test]
fn extern_struct_with_no_fields_is_rejected() {
    // W6-5 — a ZERO-field struct at an extern boundary used to pass `check` and then PANIC the VM
    // unrecoverably (libffi's `prep_cif` → `Typedef`, `recover:` cannot catch a Rust panic). C itself
    // has no empty struct (GCC/Clang size-1 extension), and libffi cannot build a CIF for one, so
    // reject it at check time like the other 7 marshalling rejects. BOTH directions (the guard lives
    // in the shared `struct_fields_marshallable`, which both flow through).
    rejects(
        "struct Empty:\n    pass\n\nextern \"libc.so.6\":\n    fn abs(x: int) -> Empty\n",
        "has no fields",
    );
    rejects(
        "struct Empty:\n    pass\n\nextern \"libc.so.6\":\n    fn abs(x: Empty) -> int\n",
        "has no fields",
    );
    // …and on the graph path, where the struct is keyed `main::Empty` — the message must still render
    // the BARE name.
    let errs = check_entry(
        "struct Empty:\n    pass\n\nextern \"libc.so.6\":\n    fn abs(x: int) -> Empty\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("struct 'Empty' has no fields")),
        "expected a bare-named zero-field reject, got: {errs:?}"
    );
}

#[test]
fn extern_struct_with_nested_struct_field_is_rejected() {
    // A struct field that is itself a struct (nested by value) is deferred in v1 — reject with the
    // struct + field named.
    rejects(
        "struct Inner:\n    x: int\n\
         \nstruct Outer:\n    inner: Inner\n    y: int\n\
         \nextern \"libc.so.6\":\n    fn f(o: Outer) -> int\n",
        "field 'inner'",
    );
}

#[test]
fn extern_struct_alias_behaves_like_bare() {
    // `type P = Point` used as an extern param/return type behaves identically to bare `Point` — a
    // transparent alias to a flat-scalar struct is C-marshallable.
    ok("struct Point:\n    x: int\n    y: int\n\
         \ntype P = Point\n\
         \nextern \"libc.so.6\":\n    fn id_point(p: P) -> P\n");
}

#[test]
fn extern_cyclic_alias_struct_no_overflow() {
    // A cyclic alias used as an extern type must report a clean error (no stack overflow). The
    // alias-resolution cycle guard catches it before marshallability recursion.
    rejects(
        "type A = B\ntype B = A\n\
         \nextern \"libc.so.6\":\n    fn f(x: A) -> int\n",
        "recursive type alias",
    );
}

#[test]
fn width_name_without_import_rejected() {
    // The eight fixed-width C-ABI integer type names are NOT global builtins — they only resolve in a
    // module that imports them per-name from `std.ffi`. A bare `int32` annotation with no import is an
    // UNKNOWN type. (Failing-then-green: before gating, resolve_type mapped int32 -> Ty::Int
    // unconditionally so this compiled clean.) `check_src` is a lone module with no imports possible.
    rejects("x: int32 = 5\nprint(x)\n", "unknown type 'int32'");
}

#[test]
fn ffi_type_import_cannot_be_renamed() {
    // An FFI width type cannot be aliased on import: the backends' `ctype_of` keys off the literal
    // surface name, so a renamed import would resolve to a type the marshaller can't lower. Reject
    // both `import int32 as W` (name unusable) and `import int8 as int32` (silently wrong width).
    entry_rejects(
        "import int32 as W from std.ffi\nfn main():\n    x: W = 5\n    print(x)\n",
        "cannot be renamed on import",
    );
    entry_rejects(
        "import int8 as int32 from std.ffi\nfn main():\n    x: int32 = 5\n    print(x)\n",
        "cannot be renamed on import",
    );
}

#[test]
fn width_name_cannot_be_redefined_as_alias() {
    // A user `type int32 = str` must not silently shadow the FFI width name (it would flip int32's
    // meaning from int to str). The name is reserved.
    rejects(
        "type int32 = str\nx: int32 = \"hi\"\nprint(x)\n",
        "reserved",
    );
}

#[test]
fn alias_to_width_resolves_without_bare_import() {
    // A transparent alias whose body is a width name resolves wherever the alias is used, even if the
    // using site did not `import int32` directly — the alias is the explicit opt-in (an alias body is
    // a deliberate definition, unlike an accidental bare `int32` in ordinary code, which still needs
    // the import). This keeps a `type Len = int32` usable across modules (alias is program-global; the
    // per-module import set is not).
    entry_ok(
        "import int32 from std.ffi\ntype Len = int32\n\
         extern \"libc.so.6\":\n    fn f(x: Len) -> Len\n\
         \nfn main():\n    n: int = f(5)\n    print(n)\n",
    );
}

#[test]
fn width_alias_without_any_import_rejected() {
    // The alias opt-in is PRECISE, not a blanket gate bypass: `type Len = int32` only lets a width
    // name resolve through it if the alias's DEFINING module imported the width. With NO module
    // importing int32 at all, the alias must NOT launder the width name — it stays an unknown type.
    // (Failing-then-green: before the precise gate, resolve_type accepted any width body while
    // `alias_resolving` was non-empty, so this compiled clean — the gate hole.)
    rejects(
        "type Len = int32\nextern \"libc.so.6\":\n    fn f(x: Len) -> int\n",
        "unknown type 'int32'",
    );
}

#[test]
fn width_alias_defined_with_import_resolves_in_extern() {
    // PRECISE rule (the licensing half): an alias whose DEFINING module imported the width resolves
    // through the alias wherever the alias is used — including inside an extern signature. This is the
    // documented opt-in (`alias_to_width_resolves_without_bare_import`), now licensed by the defining
    // module's import rather than by a blanket `alias_resolving`-non-empty bypass. (Type aliases are
    // not exportable across modules — the alias is program-global but the import licence is what is
    // recorded per-alias at definition time, so any later use, even after the import set is cleared
    // for another module, still resolves.)
    entry_ok(
        "import int32 from std.ffi\ntype Len = int32\n\
         extern \"libc.so.6\":\n    fn f(x: Len) -> Len\n\
         \nfn main():\n    n: int = f(5)\n    print(n)\n",
    );
}

#[test]
fn composite_width_alias_licensed_cross_module() {
    // PRECISE licensing extends to COMPOSITE alias bodies, not just a bare `type Len = int32`. Module A
    // imports the widths and defines `type Pair = (int32, int32)`; module B uses `Pair` without its own
    // import. The alias is program-global, so B sees the name; because A (the defining module) imported
    // every width the body embeds, the alias is licensed and the widths resolve through it in B.
    // (Regression: the first cut only recorded bare-`Named` bodies, so a composite licensed alias was
    // wrongly rejected at a non-importing use site with "unknown type 'int32'".)
    let t = TmpDir::new();
    t.write(
        "lib.chz",
        "import int32 from std.ffi\ntype Pair = (int32, int32)\nfn mk() -> Pair:\n    return (1, 2)\n",
    );
    let entry = t.write(
        "main.chz",
        "import mk from lib\nimport Pair from lib\nfn use_pair(p: Pair) -> int:\n    return 0\n\
         fn main():\n    print(use_pair(mk()))\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    assert!(
        check_graph(&graph).is_ok(),
        "imported composite width alias must carry its license to the importing use site"
    );
}

#[test]
fn composite_width_alias_partial_import_not_laundered() {
    // The licence is precise: an alias is licensed only when its DEFINING module imported EVERY width it
    // embeds. `type Mixed = (int32, int64)` in a module that imported only int32 is NOT licensed — so
    // int64 cannot ride in on int32's opt-in. Using `Mixed` from a module that never imported int64 is
    // rejected (the width it failed to import stays an unknown type).
    let t = TmpDir::new();
    t.write(
        "lib.chz",
        "import int32 from std.ffi\ntype Mixed = (int32, int64)\nfn dummy() -> int:\n    return 0\n",
    );
    let entry = t.write(
        "main.chz",
        "import dummy from lib\nimport Mixed from lib\nfn use_mixed(m: Mixed) -> int:\n    return 0\nfn main():\n    print(dummy())\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unknown type 'int64'")),
        "partial-import composite alias must not launder the un-imported width, got: {errs:?}"
    );
}

#[test]
fn width_import_redundant_self_rename_ok() {
    // `import int32 as int32` is a redundant but harmless self-rename — the as-name is identical to
    // the member, so it carries no wrong-width risk and must be accepted (it just imports int32). A
    // true rename (`as W`) or a wrong-width trap (`int8 as int32`) still rejects — see
    // `ffi_type_import_cannot_be_renamed`. (Failing-then-green: before, `alias.is_some()` rejected any
    // as-clause, so even the identical name errored "cannot be renamed".)
    entry_ok("import int32 as int32 from std.ffi\nfn main():\n    x: int32 = 5\n    print(x)\n");
}

#[test]
fn ffi_type_import_then_extern_and_struct_ok() {
    // `import int8, int32, uint32 from std.ffi` makes the width names resolvable in THIS module — as an
    // extern param/return AND as a struct field type. They resolve to a plain `int` (the program sees
    // an `int`); the width is a runtime-only marshalling distinction.
    entry_ok(
        "import int8, int32, uint32 from std.ffi\n\
         struct Mixed:\n    a: int32\n    b: float\n\
         extern \"libc.so.6\":\n    fn f(x: int8) -> uint32\n    fn id(m: Mixed) -> Mixed\n\
         \nfn main():\n    n: int = f(5)\n    print(n)\n",
    );
}

#[test]
fn ffi_bogus_type_import_rejected() {
    // Importing a name that is neither a callable member nor one of the eight exported width TYPE names
    // errors like any bad import — `std.ffi` has no member `int99`.
    entry_rejects(
        "import int99 from std.ffi\nfn main():\n    print(1)\n",
        "has no member 'int99'",
    );
}

#[test]
fn width_name_not_leaked_across_modules() {
    // A width name imported by module A does NOT become visible in module B's own source: B writing a
    // bare `int32` annotation without its own import is an unknown type, even though it imports A.
    let t = TmpDir::new();
    t.write(
        "lib.chz",
        "import int32 from std.ffi\nfn id32(x: int32) -> int32:\n    return x\n",
    );
    let entry = t.write(
        "main.chz",
        "import id32 from lib\nfn main():\n    x: int32 = 5\n    print(id32(x))\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unknown type 'int32'")),
        "expected B to be rejected for using int32 without its own import, got: {errs:?}"
    );
}

#[test]
fn cross_module_struct_with_width_field_usable_without_import() {
    // A struct declared in module A (which imports int32) with int32 fields is usable from module B
    // WITHOUT B importing int32 — the field types were resolved to `Ty::Int` during A's checking, so B
    // never re-resolves the width NAME. B reads `.x` as a plain int.
    let t = TmpDir::new();
    t.write(
        "geo.chz",
        "import int32 from std.ffi\nstruct Pt:\n    x: int32\n    y: int32\n",
    );
    let entry = t.write(
        "main.chz",
        "import Pt from geo\nfn main():\n    p := Pt(3, 4)\n    n: int = p.x\n    print(n)\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(
        errs.is_empty(),
        "expected B to use A's struct without importing int32, got: {errs:?}"
    );
}

#[test]
fn extern_generic_struct_by_value_rejected() {
    // A generic struct has no fixed C layout — instantiated `Pair[int]` is rejected as
    // non-marshallable (generic structs are out of v1 scope).
    rejects(
        "struct Pair[T]:\n    a: T\n    b: T\n\
         \nextern \"libc.so.6\":\n    fn f(p: Pair[int]) -> int\n",
        "not C-marshallable",
    );
}

#[test]
fn ffi_null_and_is_null_typecheck() {
    // `std.ffi.null()` is `ptr`; `is_null(p)` is `bool`; `ptr == ptr` (incl. vs `null()`) type-checks.
    entry_ok(
        "import ptr, null, is_null from std.ffi\n\
         extern \"libc.so.6\":\n    fn tmpfile() -> ptr\n\n\
         fn main():\n    h: ptr = tmpfile()\n    b: bool = is_null(h)\n    print(b)\n    print(h == null())\n",
    );
}

#[test]
fn ptr_arithmetic_rejected() {
    // A `ptr` is opaque — no arithmetic. `null() + null()` is a type error (only `==`/`!=` + pass).
    entry_rejects(
        "import null from std.ffi\nfn main():\n    print(null() + null())\n",
        "",
    );
}

#[test]
fn extern_duplicate_name_rejected() {
    rejects(
        "extern \"libm.so.6\":\n    fn cos(x: float) -> float\n    fn cos(x: float) -> float\n",
        "already defined",
    );
}

#[test]
fn extern_named_after_builtin_rejected() {
    // An extern fn named after a builtin (range/int/float/str/ord/chr/set) would be silently
    // shadowed: `compile_call`/`eval_call` resolve the name to the builtin op before a plain call,
    // so the extern is dead — and the compiler's eager `MakeCffi` dlsyms a symbol it can never call.
    // Reject the collision at hoist with a clear message.
    rejects(
        "extern \"libc.so.6\":\n    fn range(x: int) -> int\n",
        "builtin/reserved name",
    );
}

#[test]
fn extern_named_after_constructor_rejected() {
    // `Channel`/`Shared`/`Atomic`/`timer`/`Executor` are constructor names the backends special-case
    // before a plain call, so an extern with that name is unreachable. Reject it.
    rejects(
        "extern \"libc.so.6\":\n    fn Channel() -> int\n",
        "builtin/reserved name",
    );
}

#[test]
fn extern_named_after_print_rejected() {
    rejects(
        "extern \"libc.so.6\":\n    fn print(x: int) -> int\n",
        "builtin/reserved name",
    );
}

#[test]
fn extern_named_after_struct_rejected() {
    // A struct registers a same-named constructor the backends resolve before a plain call, so an
    // extern colliding with a struct name is dead. The guard is order-independent: the struct is
    // declared AFTER the extern, yet the collision must still fire.
    rejects(
        "extern \"libc.so.6\":\n    fn S(x: int) -> int\n\nstruct S:\n    a: int\n",
        "builtin/reserved name",
    );
}

#[test]
fn extern_named_after_struct_rejected_entry_path() {
    // W6-6 — the bare-keyed single-module `ok()`/`rejects()` helper CANNOT catch this class: the CLI
    // graph path keys `self.structs` module-scoped (`main::S`), so a bare `structs.contains_key(name)`
    // lookup always missed and `struct strlen` + `extern fn strlen` silently called the CTOR. The
    // sweep now consults `struct_names` (the BARE-visible ctor set), which is bare in BOTH paths.
    // Both decl orders, because the sweep runs after the hoist loop.
    entry_rejects(
        "extern \"libc.so.6\":\n    fn strlen(s: str) -> int\n\nstruct strlen:\n    s: str\n",
        "builtin/reserved name",
    );
    entry_rejects(
        "struct S:\n    a: int\n\nextern \"libc.so.6\":\n    fn S(x: int) -> int\n",
        "builtin/reserved name",
    );
}

#[test]
fn extern_named_after_newtype_rejected() {
    // W6-6, sibling arm: a `newtype` registers a bare-visible one-arg ctor exactly like a struct, so
    // it shadows an extern the same way. Pre-fix this checked OK and then silently called the CTOR:
    // `newtype abs = int` + `extern fn abs(x: int) -> int` printed `abs(-7)` instead of `7` on BOTH
    // engines. Caught by the adversarial review of the first cut of this fix, whose predicate covered
    // `struct_names` but not `newtype_names` — the same partial-coverage class the sweep closes.
    entry_rejects(
        "newtype abs = int\n\nextern \"libc.so.6\":\n    fn abs(x: int) -> int\n",
        "builtin/reserved name",
    );
    // Both decl orders (the sweep runs after the hoist loop, so it must be order-independent).
    entry_rejects(
        "extern \"libc.so.6\":\n    fn abs(x: int) -> int\n\nnewtype abs = int\n",
        "builtin/reserved name",
    );
    rejects(
        "newtype abs = int\n\nextern \"libc.so.6\":\n    fn abs(x: int) -> int\n",
        "builtin/reserved name",
    );
    // Control: a newtype whose name collides with NOTHING leaves the extern reachable.
    entry_ok("newtype Meters = int\n\nextern \"libc.so.6\":\n    fn abs(x: int) -> int\n");
}

#[test]
fn extern_named_after_unimported_native_struct_ok() {
    // Deliberate delta of the W6-6 fix: `seed_stdlib_structs` seeds the `Match`/`Response`/
    // `ProcResult`/`FileInfo` LAYOUTS into `self.structs` bare-keyed and UN-licensed, so keying the
    // sweep off `self.structs` would over-reject. With no `import std.regex`, bare `Match(...)` is
    // `unknown type 'Match'; import it from std.regex` — nothing shadows the extern, so it is
    // reachable and must be ACCEPTED. (Paired with the imported case below.)
    entry_ok("extern \"libc.so.6\":\n    fn Match(x: int) -> int\n");
}

#[test]
fn extern_named_after_imported_native_struct_rejected() {
    // …and the import DOES license the bare ctor name (`struct_names.insert` on the is_std arm), so
    // the collision still fires there. This pins that the `struct_names` predicate did not widen the
    // hole open.
    entry_rejects(
        "import std.regex\n\nextern \"libc.so.6\":\n    fn Match(x: int) -> int\n",
        "builtin/reserved name",
    );
}

#[test]
fn extern_named_after_variant_rejected() {
    // An enum variant is keyed globally and resolved as a constructor before a plain call.
    rejects(
        "extern \"libc.so.6\":\n    fn Leaf(x: int) -> int\n\nenum Tree:\n    Leaf\n    Node\n",
        "builtin/reserved name",
    );
}

#[test]
fn extern_reserved_name_reported_once() {
    // W6-16 — a name that is BOTH in `RESERVED_CALLABLE` *and* a prelude `native struct` landing in
    // the module's bare tables (`str`/`bytes`/`bytearray`/`Channel`/`List`/`Map`/`Set`) was reported
    // by BOTH the in-loop reserved guard and the post-loop collision sweep → doubled diagnostics,
    // incl. under `--errors=json` (doubled LSP squiggles). Report it exactly once.
    for n in [
        "str",
        "bytes",
        "bytearray",
        "Channel",
        "List",
        "Map",
        "Set",
        // …and the already-single ones stay single.
        "int",
        "print",
    ] {
        let errs = check_entry(&format!(
            "extern \"libm.so.6\":\n    fn {n}(x: int) -> int\n"
        ));
        assert_eq!(
            errs.iter()
                .filter(|e| e.message.contains("builtin/reserved name"))
                .count(),
            1,
            "expected exactly one reserved-name diagnostic for {n:?}, got: {errs:?}"
        );
    }
}

#[test]
fn extern_named_after_builtin_variant_rejected() {
    // W6-11 — the four BUILTIN variant ctors (`Result`/`Option`'s) are resolved as ctors before a
    // plain call exactly like a user enum's variants, but they are absent from `variant_owners`
    // (their identity stays in `resolve_type`), so the sweep missed them. Probe that filed it:
    // `extern fn Ok(x: float) -> float` then `y: float = Ok(2.0)` → "cannot assign Result[float] to
    // variable of type float" — i.e. the call site resolves to the VARIANT, the extern is dead.
    for v in ["Ok", "Err", "Some", "None"] {
        rejects(
            &format!("extern \"libm.so.6\":\n    fn {v}(x: float) -> float\n"),
            "builtin/reserved name",
        );
    }
    entry_rejects(
        "extern \"libm.so.6\":\n    fn Ok(x: float) -> float\n",
        "builtin/reserved name",
    );
}

#[test]
fn extern_named_after_result_option_type_ok() {
    // …but the builtin TYPE names `Result`/`Option` are NOT callable, so an extern taking one is
    // genuinely reachable and must stay ACCEPTED — the same rule as `extern_named_after_enum_type_ok`
    // below. Probe: `extern fn Result(x: float) -> float` then `y: float = Result(2.0)` type-checks
    // (and `run` dlsym-errors on the missing C symbol, i.e. it really dispatches to the extern),
    // unlike the `Ok` probe above. Rejecting these would be a NEW over-rejection.
    ok("extern \"libm.so.6\":\n    fn Result(x: float) -> float\n");
    ok("extern \"libm.so.6\":\n    fn Option(x: float) -> float\n");
}

#[test]
fn extern_named_after_enum_type_ok() {
    // An enum TYPE name is NOT callable in either backend (only its variants are), so an extern
    // sharing the enum's type name is reachable and must be ACCEPTED — symmetric with a plain
    // `fn Tree` alongside `enum Tree`. Only struct names and variant names are real collisions.
    ok("extern \"libc.so.6\":\n    fn Tree(x: int) -> int\n\nenum Tree:\n    Leaf\n    Node\n");
}

#[test]
fn extern_type_alias_param_ok() {
    // A transparent alias resolving to a marshallable scalar is accepted (check runs on resolved Ty).
    ok(
        "type Len = int\nextern \"libc.so.6\":\n    fn strlen(s: str) -> Len\n\nprint(strlen(\"hi\"))\n",
    );
}

#[test]
fn extern_nil_param_rejected() {
    // `nil` is a valid VOID return but NOT a valid parameter (the backend's `ctype_of` has no nil
    // case, so accepting it as a param would panic every engine on a checked program). A
    // void-returning extern yields a `Nil` value, which would otherwise satisfy a `nil` param.
    rejects(
        "extern \"libc.so.6\":\n    fn f(x: nil) -> int\n",
        "not C-marshallable",
    );
}

// ===== generators (experimental, VM-only) =====

#[test]
fn generator_basic_ok() {
    // A generator declares `-> Iterator[T]`, yields T, and its result drives a `for` (x: int).
    ok(
        "fn count() -> Iterator[int]:\n    yield 1\n    yield 2\n\nfn use() -> int:\n    s := 0\n    for x in count():\n        s = s + x\n    return s\n",
    );
}

#[test]
fn generator_for_binds_element_type() {
    // `for x in count()` must bind `x: int`, so using it as a str is a type error.
    rejects(
        "fn count() -> Iterator[int]:\n    yield 1\n\nfn use():\n    for x in count():\n        print(x + \"a\")\n",
        "",
    );
}

#[test]
fn generator_yield_type_mismatch_rejected() {
    rejects(
        "fn g() -> Iterator[int]:\n    yield \"nope\"\n",
        "expected yield type int",
    );
}

#[test]
fn generator_missing_iterator_return_rejected() {
    rejects(
        "fn g() -> int:\n    yield 1\n",
        "must declare a return type of `Iterator[T]`",
    );
}

#[test]
fn generator_infers_element_type_no_annotation() {
    // No `-> Iterator[T]`: the element type is inferred from the first yield (strict-first-yield),
    // and callers see it — `for x in count()` binds `x: int`, so `s + x` type-checks.
    ok(
        "fn count():\n    yield 1\n    yield 2\nfn use() -> int:\n    s := 0\n    for x in count():\n        s = s + x\n    return s\n",
    );
}

#[test]
fn generator_inferred_element_recovered_not_unknown() {
    // The inferred element is the CONCRETE first-yield type (int), NOT a permissive `Unknown`:
    // `x + "a"` (int + str) must be rejected — proves inference pins a real type.
    rejects(
        "fn count():\n    yield 1\nfn use():\n    for x in count():\n        print(x + \"a\")\n",
        "",
    );
}

#[test]
fn generator_inferred_int_then_float_rejected() {
    // CONSTRAINT 1: strict-first-yield pins `T = int` from the first yield; a later `yield 2.0`
    // (float) must be REJECTED at check time, NOT silently coerced to float. There is no CoerceFloat
    // plumbed through `yield`, so a silent int->float join would leave a runtime int under a float
    // type. This program is check-REJECTED, so there is deliberately no runtime arm — accepting it
    // (the bug) is exactly what this test forbids. Checked via the full module-graph entry path.
    let errs = check_entry("fn count():\n    yield 1\n    yield 2.0\nfn main():\n    pass\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("expected yield type int, found float")),
        "expected int-vs-float yield rejection, got: {errs:?}"
    );
}

#[test]
fn generator_uninferable_element_rejected() {
    // CONSTRAINT 2: a generator whose only yield is an un-inferable empty `[]` leaves `T`
    // un-inferable (List[Unknown]). It MUST be a clear error, NOT a silent `Iterator[List[Unknown]]`
    // leak (the residual-Unknown type-check-bypass class, cf. commit 29513bd).
    rejects(
        "fn g():\n    yield []\n",
        "cannot infer generator element type",
    );
}

#[test]
fn generator_inferred_struct_method_no_annotation() {
    // The struct/enum-method arm of `infer_returns` also infers a generator's element type: an
    // un-annotated `each` yields `int`, so `for x in b.each()` binds `x: int`.
    ok(
        "struct Box:\n    n: int\n    fn each(self):\n        i := 0\n        while i < self.n:\n            yield i\n            i = i + 1\nfn use() -> int:\n    b := Box(3)\n    s := 0\n    for x in b.each():\n        s = s + x\n    return s\n",
    );
}

#[test]
fn generator_explicit_annotation_still_works() {
    // The explicit `-> Iterator[T]` path is untouched: annotation still validates yields against T.
    ok("fn count() -> Iterator[int]:\n    yield 1\n    yield 2\n");
    rejects(
        "fn count() -> Iterator[int]:\n    yield \"x\"\n",
        "expected yield type int",
    );
}

#[test]
fn generator_return_value_rejected() {
    rejects(
        "fn g() -> Iterator[int]:\n    yield 1\n    return 5\n",
        "cannot `return` a value",
    );
}

#[test]
fn generator_explicit_next_ok() {
    // A generator result is an `Iterator[int]`; `.next()` returns `Option[int]`, drivable explicitly.
    ok(
        "fn count() -> Iterator[int]:\n    yield 1\n\nfn use():\n    g := count()\n    match g.next():\n        Some(v): print(v)\n        None: print(-1)\n",
    );
}

#[test]
fn iterator_value_unknown_method_rejected() {
    rejects(
        "fn count() -> Iterator[int]:\n    yield 1\n\nfn use():\n    g := count()\n    g.bogus()\n",
        "has no method 'bogus'",
    );
}

#[test]
fn user_struct_named_iterator_rejected() {
    // `Iterator` is reserved (it names the generator existential value type); a user `struct
    // Iterator[T]` would be silently shadowed and crash on a phantom `.next()` — reject it.
    rejects("struct Iterator[T]:\n    val: T\n", "is reserved");
}

#[test]
fn yield_outside_generator_rejected() {
    rejects(
        "yield 1\n",
        "`yield` can only appear inside a generator function",
    );
}

#[test]
fn generator_defer_rejected() {
    rejects(
        "fn g() -> Iterator[int]:\n    defer print(0)\n    yield 1\n",
        "`defer` is not supported inside a generator",
    );
}

#[test]
fn generator_spawn_rejected() {
    rejects(
        "fn g() -> Iterator[int]:\n    parallel:\n        spawn print(0)\n    yield 1\n",
        "`parallel:` is not supported inside a generator",
    );
}

#[test]
fn generator_yield_only_in_recover_ok() {
    // A generator whose only `yield` lives in a `recover:` block is valid (detected as a generator,
    // so `yield` is legal there) — guards the parser/checker recover-descent fix.
    ok("fn g() -> Iterator[int]:\n    x := recover:\n        yield 1\n        1\n    print(x)\n");
}

#[test]
fn generator_defer_in_recover_rejected() {
    // The `defer` ban must not be bypassable by nesting inside a `recover:` block.
    rejects(
        "fn cleanup():\n    print(\"c\")\nfn g() -> Iterator[int]:\n    yield 0\n    x := recover:\n        defer cleanup()\n        1\n    print(x)\n",
        "`defer` is not supported inside a generator",
    );
}

#[test]
fn generator_bare_return_ok() {
    // A bare `return` inside a generator stops it early — legal.
    ok("fn g() -> Iterator[int]:\n    yield 1\n    return\n    yield 2\n");
}

// ===== assert =====

#[test]
fn assert_non_bool_cond_rejected() {
    rejects("assert 1\n", "assert condition must be bool");
}

#[test]
fn assert_non_str_msg_rejected() {
    rejects("assert true, 5\n", "assert message must be str");
}

#[test]
fn assert_ok() {
    ok("x := 1\nassert x == 1\n");
    ok("assert true, \"m\"\n");
}

// ===== print sep=/end= =====

#[test]
fn print_sep_end_str_ok() {
    // The `sep`/`end` named args (kept on the Call by desugar) type-check when str.
    ok_desugared("print(\"a\", \"b\", sep=\"-\", end=\"!\")\n");
    ok_desugared("print(\"a\", end=\"\")\n");
    // str expressions (not just literals) are fine.
    ok_desugared("s := \"-\"\nprint(\"a\", \"b\", sep=s)\n");
}

#[test]
fn print_sep_non_str_rejected() {
    rejects_desugared("print(\"a\", sep=1)\n", "print() sep/end must be str");
}

#[test]
fn print_end_non_str_rejected() {
    rejects_desugared("print(\"a\", end=1)\n", "print() sep/end must be str");
}

// ===== test fn =====

#[test]
fn free_test_fn_with_params_rejected() {
    rejects(
        "test fn t(x: int):\n    assert true\n",
        "test function must take no parameters",
    );
}

#[test]
fn free_test_fn_with_return_rejected() {
    rejects(
        "test fn t() -> int:\n    return 1\n",
        "test function must not return a value",
    );
}

#[test]
fn method_test_fn_with_extra_param_rejected() {
    rejects(
        "struct S:\n    test fn t(self, x: int):\n        assert true\n",
        "test method must take only self",
    );
}

#[test]
fn test_fn_valid_forms_ok() {
    ok("test fn t():\n    assert true\n");
    ok("struct S:\n    test fn t(self):\n        assert true\n");
}

#[test]
fn test_fn_body_is_still_checked() {
    // A type error inside a valid-shaped test fn is still reported.
    rejects(
        "test fn t():\n    assert 1\n",
        "assert condition must be bool",
    );
}

#[test]
fn suite_lifecycle_hook_wrong_shape_rejected() {
    // In a suite, a lifecycle-named method with the wrong signature is a hard error.
    rejects(
        "struct S:\n    test fn t(self):\n        assert true\n    fn before_each(self, x: int):\n        return\n",
        "lifecycle hook 'before_each' must take only self",
    );
}

#[test]
fn suite_lifecycle_hook_valid_ok() {
    ok(
        "struct S:\n    test fn t(self):\n        assert true\n    fn before_each(self):\n        return\n",
    );
}

#[test]
fn lifecycle_name_in_non_suite_struct_not_validated() {
    // A `before_each` method in a struct that is NOT a suite (no test fn) is an ordinary method —
    // no special signature rule applies.
    ok("struct S:\n    fn before_each(self, x: int):\n        return\n");
}

// ===== bytes type (b"..." literal + Index/Slice/Iterator protocols) =====

#[test]
fn bytes_literal_infers_bytes_and_protocols() {
    // literal infers `bytes`; b[i] -> int; b[a:b] -> bytes; for c in b -> int; len -> int
    ok(
        "fn main():\n    b := b\"hi\"\n    x: int = b[0]\n    s: bytes = b[0:1]\n    for c in b:\n        print(c)\n    n: int = b.len()\n    print(x + n)\nmain()\n",
    );
}

#[test]
fn bytes_annotation_and_equality_ok() {
    ok(
        "fn main():\n    a: bytes = b\"\\x01\\x02\"\n    eq := a == b\"\\x01\\x02\"\n    print(eq)\nmain()\n",
    );
}

#[test]
fn bytes_is_immutable_index_set_rejected() {
    // bytes is immutable — `b[i] = x` must be a type error (no IndexSet conformance).
    rejects("fn main():\n    b := b\"hi\"\n    b[0] = 1\nmain()\n", "");
}

#[test]
fn bytes_not_assignable_to_str() {
    rejects("fn main():\n    b := b\"hi\"\n    s: str = b\nmain()\n", "");
}

#[test]
fn bytes_key_in_map_ok() {
    // bytes is Hashable — valid map key.
    ok("fn main():\n    m: Map[bytes, int] = {b\"a\": 1}\n    print(m[b\"a\"])\nmain()\n");
}

// ===== bytearray type (mutable sibling of bytes — constructor-only, Index/IndexSet/Slice/Iterator) =====

#[test]
fn bytearray_constructor_and_ty() {
    // `bytearray([..])` infers `bytearray`; ba[i] -> int; ba[i] = int ok; ba[a:b] -> bytearray;
    // for x in ba -> int; len ok.
    ok(
        "fn main():\n    ba := bytearray([1, 2, 3])\n    x: int = ba[0]\n    ba[0] = 5\n    s: bytearray = ba[0:1]\n    for c in ba:\n        print(c)\n    n: int = ba.len()\n    print(x + n)\nmain()\n",
    );
}

#[test]
fn bytearray_constructor_overloads_infer_bytearray() {
    // All four constructor forms infer `bytearray`.
    ok(
        "fn main():\n    a: bytearray = bytearray()\n    b: bytearray = bytearray(4)\n    c: bytearray = bytearray(b\"x\")\n    d: bytearray = bytearray([1, 2])\n    print(a.len() + b.len() + c.len() + d.len())\nmain()\n",
    );
}

#[test]
fn bytearray_conversion_bridge_typechecks() {
    // bytes(ba) -> bytes; bytearray(b) -> bytearray.
    ok(
        "fn main():\n    ba := bytearray([1, 2])\n    b: bytes = bytes(ba)\n    ba2: bytearray = bytearray(b)\n    print(b.len() + ba2.len())\nmain()\n",
    );
}

#[test]
fn bytearray_index_set_typechecks() {
    // The NEW capability bytes lacks: `ba[i] = x` is a valid IndexSet assignment.
    ok("fn main():\n    ba := bytearray([1, 2])\n    ba[0] = 200\n    print(ba[0])\nmain()\n");
}

#[test]
fn bytearray_constructor_rejects_str_arg() {
    rejects("fn main():\n    ba := bytearray(\"s\")\nmain()\n", "");
}

#[test]
fn bytearray_not_hashable_map_key_rejected() {
    // bytearray is MUTABLE -> NOT Hashable -> not a valid map key (like list).
    rejects(
        "fn main():\n    m: Map[bytearray, int] = {bytearray(): 1}\nmain()\n",
        "",
    );
}

#[test]
fn bytearray_not_assignable_to_bytes() {
    rejects(
        "fn main():\n    ba := bytearray([1])\n    b: bytes = ba\nmain()\n",
        "",
    );
}

// ===== conversions: str.encode()/bytes.decode()/bytearray.decode() (UTF-8) =====

#[test]
fn encode_decode_types() {
    // str.encode() -> bytes; bytes.decode() -> str; bytearray.decode() -> str.
    ok(
        "fn main():\n    b: bytes = \"x\".encode()\n    s1: str = b\"x\".decode()\n    s2: str = bytearray([120]).decode()\n    print(s1 + s2 + str(b.len()))\nmain()\n",
    );
}

#[test]
fn encode_only_on_str_decode_only_on_bytes() {
    // encode is str-only: bytes/bytearray have no encode.
    rejects("fn main():\n    x := b\"x\".encode()\nmain()\n", "");
    rejects("fn main():\n    x := bytearray([1]).encode()\nmain()\n", "");
    // decode is bytes/bytearray-only: str has no decode.
    rejects("fn main():\n    x := \"x\".decode()\nmain()\n", "");
}

// ===== conversions: List()/Set()/Map() constructors over any for-iterable =====

#[test]
fn constructor_iter_types() {
    // List() over every for-iterable shape; element type flows through iter_elem.
    ok("fn main():\n    a: List[int] = List([1, 2])\n    print(a.len())\nmain()\n");
    ok("fn main():\n    s := {1, 2}\n    a: List[int] = List(s)\n    print(a.len())\nmain()\n");
    ok("fn main():\n    a: List[int] = List(b\"hi\")\n    print(a.len())\nmain()\n");
    ok("fn main():\n    a: List[str] = List(\"ab\")\n    print(a.len())\nmain()\n");
    ok("fn main():\n    a: List[int] = List(range(3))\n    print(a.len())\nmain()\n");
    ok("fn main():\n    a: List[int] = List(bytearray([1, 2]))\n    print(a.len())\nmain()\n");
    // Set() broadened from list-only to any for-iterable.
    ok("fn main():\n    s: Set[str] = Set(\"abc\")\n    print(s.len())\nmain()\n");
    ok("fn main():\n    s: Set[int] = Set(range(3))\n    print(s.len())\nmain()\n");
    ok("fn main():\n    s: Set[int] = Set([1, 1, 2])\n    print(s.len())\nmain()\n");
    // Map() from a list of 2-tuples.
    ok(
        "fn main():\n    m: Map[int, str] = Map([(1, \"a\"), (2, \"b\")])\n    print(m.len())\nmain()\n",
    );
}

#[test]
fn container_ctor_turbofish_binds_elem_type() {
    // `List[T]()` / `Map[K,V]()` / `Set[T]()` — turbofish on the container constructors yields a
    // typed empty container (no longer "takes no type arguments").
    ok(
        "a: List[int] = List[int]()\nb: Map[str, int] = Map[str, int]()\nc: Set[int] = Set[int]()\n",
    );
    // The turbofish actually BINDS the element type (not Unknown): a mismatched annotation rejects.
    rejects("a: List[str] = List[int]()\n", "List[int]");
    rejects("a: Set[str] = Set[int]()\n", "Set[int]");
    rejects("a: Map[str, str] = Map[str, int]()\n", "Map[str, int]");
}

#[test]
fn container_ctor_turbofish_checks_elements() {
    // `List[int]([1, 2])` checks supplied elements against the turbofish elem type.
    ok("x := List[int]([1, 2])\n");
    ok("x := Set[int]([1, 2])\n");
    ok("x := Map[str, int]([(\"a\", 1)])\n");
    rejects("x := List[int]([\"a\"])\n", "expected");
    rejects("x := Set[int]([\"a\"])\n", "expected");
    rejects("x := Map[str, int]([(\"a\", \"b\")])\n", "expected");
}

#[test]
fn container_ctor_turbofish_arity() {
    rejects(
        "x := List[int, str]()\n",
        "List[T]() takes exactly one type argument",
    );
    rejects(
        "x := Set[int, str]()\n",
        "Set[T]() takes exactly one type argument",
    );
    rejects(
        "x := Map[int]()\n",
        "Map[K, V]() takes exactly two type arguments",
    );
}

#[test]
fn list_map_zero_arg_now_legal() {
    // A still allows the literal `[]`/`{}`, but bare `List()`/`Map()` are now legal too (mirror
    // `Set()`), refined by the expected type / first use.
    ok("a: List[int] = List()\nb: Map[str, int] = Map()\n");
    // Refined by first use, like `Set()`.
    ok("e := List()\ne.push(1)\nprint(e.len())\n");
}

// ===== B: un-inferable type-parameter deadlock diagnostic (generic ctor / fn with closure arg) =====

#[test]
fn uninferable_closure_param_ctor_emits_clear_error() {
    // Both inference sources are empty: the `[]` gives no element type AND the comparator params
    // are unannotated → a genuine two-way deadlock. The error must NAME the type parameter, not
    // leak the misleading "cannot compare T and T" from inside the lambda.
    let errs = check_src(
        "struct H[T]:\n    data: List[T]\n    less: fn(T, T) -> bool\nx := H([], fn(a, b): a < b)\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("cannot infer type parameter")),
        "expected an inference-deadlock error, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("cannot compare")),
        "the misleading 'cannot compare' must be suppressed, got: {errs:?}"
    );
}

#[test]
fn uninferable_closure_param_free_fn_emits_clear_error() {
    // Same deadlock through a free generic function with a closure parameter.
    let errs = check_src(
        "fn build[T](xs: List[T], less: fn(T, T) -> bool) -> int:\n    return 0\nx := build([], fn(a, b): a < b)\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("cannot infer type parameter")),
        "expected an inference-deadlock error, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("cannot compare")),
        "the misleading 'cannot compare' must be suppressed, got: {errs:?}"
    );
}

#[test]
fn inferable_closure_ctor_forms_still_ok() {
    // The guard must NOT over-fire: every form that pins `T` from SOME source stays clean.
    // Turbofish pins T.
    ok(
        "struct H[T]:\n    data: List[T]\n    less: fn(T, T) -> bool\nx := H[int]([], fn(a, b): a < b)\n",
    );
    // Elements pin T.
    ok(
        "struct H[T]:\n    data: List[T]\n    less: fn(T, T) -> bool\nx := H([1, 2, 3], fn(a, b): a < b)\n",
    );
    // Annotated closure params pin T backward.
    ok(
        "struct H[T]:\n    data: List[T]\n    less: fn(T, T) -> bool\nx := H([], fn(a: int, b: int): a < b)\n",
    );
}

#[test]
fn uninferable_guard_does_not_overfire_on_harmless_closure_body() {
    // REGRESSION (confirmed bug #1): the un-inferable-closure-param guard must fire ONLY on a
    // genuine deadlock (a body that NEEDS `T` known, e.g. `a < b`), not whenever an unannotated
    // closure-param slot textually mentions an unbound `T`. A body that imposes NO constraint on
    // `T` (e.g. `print(x)` / a constant) stays inferable-free and must keep type-checking clean —
    // these compiled and ran on `main`; rejecting them breaks the don't-reject-valid-code contract.
    ok("fn each[T](xs: List[T], f: fn(T) -> nil):\n    return\neach([], fn(x): print(x))\n");
    ok(
        "fn mapper[T, U](xs: List[T], f: fn(T) -> U) -> List[U]:\n    return []\nx := mapper([], fn(x): 42)\n",
    );
    // An UNRELATED body error (not about `T`) must surface as itself, not be masked by the
    // inference-deadlock message.
    let errs =
        check_src("fn each[T](xs: List[T], f: fn(T) -> nil):\n    return\neach([], fn(x): nope)\n");
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("cannot infer type parameter")),
        "an unrelated body error must not be reported as an inference deadlock, got: {errs:?}"
    );
    assert!(
        errs.iter().any(|e| e.message.contains("nope")
            || e.message.to_lowercase().contains("undefined")
            || e.message.to_lowercase().contains("unknown")),
        "expected the real undefined-name error to surface, got: {errs:?}"
    );
}

#[test]
fn uninferable_closure_param_qualified_ctor_emits_clear_error() {
    // REGRESSION (confirmed bug #2): the module-qualified generic struct-ctor path (`c.Heap(...)`)
    // must get the SAME clear inference-deadlock message as the bare ctor / free-fn paths, not leak
    // the misleading "cannot compare T and T" from inside the lambda.
    let errs =
        check_entry("import std.collections as c\nx := c.Heap([], fn(a, b): a < b)\nprint(x)\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("cannot infer type parameter")),
        "expected an inference-deadlock error from the qualified ctor, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("cannot compare")),
        "the misleading 'cannot compare' must be suppressed on the qualified path, got: {errs:?}"
    );
}

#[test]
fn map_requires_two_tuple() {
    // element not a 2-tuple is a static error.
    rejects("fn main():\n    m := Map([1, 2])\nmain()\n", "");
    // a map's element is its key (not a 2-tuple) -> static error.
    rejects(
        "fn main():\n    src := {1: \"a\"}\n    m := Map(src)\nmain()\n",
        "",
    );
    // a 3-tuple is not a 2-tuple.
    rejects("fn main():\n    m := Map([(1, 2, 3)])\nmain()\n", "");
}

#[test]
fn set_map_hashable_key_gate_preserved() {
    // float is not Hashable -> set/map key must reject it.
    rejects("fn main():\n    s := Set([3.0])\nmain()\n", "Hashable");
    rejects(
        "fn main():\n    m := Map([(3.0, \"a\")])\nmain()\n",
        "Hashable",
    );
}

// ===== Iterable[T] protocol + `.iter()` cursor =====

#[test]
fn iter_method_on_collections_types_as_iterator() {
    // `.iter()` on each collection types as Iterator[elem] (the existing existential cursor type).
    ok("fn main():\n    it := [1, 2, 3].iter()\n    print(it.next())\nmain()\n");
    ok(
        "fn main():\n    it := {1, 2}.iter()\n    x: Option[int] = it.next()\n    print(x)\nmain()\n",
    );
    ok(
        "fn main():\n    it := {1: \"a\"}.iter()\n    k: Option[int] = it.next()\n    print(k)\nmain()\n",
    );
    ok(
        "fn main():\n    it := \"ab\".iter()\n    c: Option[str] = it.next()\n    print(c)\nmain()\n",
    );
    ok(
        "fn main():\n    it := b\"hi\".iter()\n    b: Option[int] = it.next()\n    print(b)\nmain()\n",
    );
    ok(
        "fn main():\n    it := bytearray([1, 2]).iter()\n    b: Option[int] = it.next()\n    print(b)\nmain()\n",
    );
}

#[test]
fn iter_cursor_drives_existing_adapters() {
    // The headline win: a list cursor composes into a struct adapter bounded `[I: Iterator[T]]`.
    ok(
        "struct Take[I: Iterator[T], T]:\n    inner: I\n    left: int\n    fn next(self) -> Option[T]:\n        if self.left <= 0:\n            return None\n        self.left = self.left - 1\n        return self.inner.next()\nfn main():\n    t := Take([10, 20, 30].iter(), 2)\n    for v in t:\n        print(v)\nmain()\n",
    );
}

#[test]
fn iterable_bound_accepts_list_and_generator() {
    // `[S: Iterable[int]]` accepts a List[int] AND a generator (Iterator[int]).
    ok(
        "fn count[S: Iterable[int]](s: S) -> int:\n    n := 0\n    for x in s.iter():\n        n = n + 1\n    return n\nfn gen() -> Iterator[int]:\n    yield 1\n    yield 2\nfn main():\n    print(count([1, 2, 3]))\n    print(count(gen()))\nmain()\n",
    );
}

#[test]
fn iter_idempotent_on_generator_and_cursor() {
    // Every Iterator IS Iterable: iter() returns self, idempotently.
    ok(
        "fn gen() -> Iterator[int]:\n    yield 1\nfn main():\n    it := gen().iter()\n    print(it.next())\nmain()\n",
    );
    ok("fn main():\n    it := [1, 2, 3].iter().iter()\n    print(it.next())\nmain()\n");
}

#[test]
fn iterable_struct_with_only_iter() {
    // A user struct with iter(self) -> Iterator[E] (but no next) satisfies Iterable and is for-iterable.
    ok(
        "struct Wrap:\n    xs: List[int]\n    fn iter(self) -> Iterator[int]:\n        return self.xs.iter()\nfn main():\n    w := Wrap([1, 2, 3])\n    for x in w:\n        print(x)\nmain()\n",
    );
}

#[test]
fn iter_no_method_on_non_iterable() {
    rejects("fn main():\n    x := 5.iter()\nmain()\n", "iter");
}

#[test]
fn iterator_bound_forwards_into_iterable_bound() {
    // Every Iterator IS Iterable: an `[S: Iterator[T]]` value must satisfy an `[U: Iterable[T]]`
    // bound it is forwarded into (the cross-protocol relationship the spec promises).
    ok(
        "fn use_iterable[U: Iterable[int]](xs: U) -> int:\n    n := 0\n    for x in xs.iter():\n        n = n + 1\n    return n\nfn pass_through[S: Iterator[int]](xs: S) -> int:\n    return use_iterable(xs)\nfn main():\n    print(pass_through([1, 2, 3]))\nmain()\n",
    );
}

// ===== non-void fn must return a value on every path (Option B) =====

#[test]
fn non_void_fn_must_return() {
    // (a) inline-body declared non-void fn whose only statement is a NON-expr (assignment) ->
    // does not implicitly return -> must reject (it falls off the end).
    rejects(
        "fn a() -> int:\n    x := 1\nfn main():\n    print(a())\nmain()\n",
        "fall off the end",
    );
    // (b) multiline declared non-void fn whose only statement is a `print` -> must reject.
    rejects(
        "fn a() -> int:\n    print(\"hi\")\nfn main():\n    a()\nmain()\n",
        "fall off the end",
    );
    // NEGATIVE: no return annotation infers void (nil) -> must NOT be rejected.
    ok("fn a(): 10\nfn main():\n    a()\nmain()\n");
    // NEGATIVE: explicit value `return` on the only path -> ok.
    ok("fn a() -> int:\n    return 10\nfn main():\n    print(a())\nmain()\n");
}

#[test]
fn non_void_fn_termination_no_false_positive() {
    // if/else where BOTH arms return a value -> terminates.
    ok(
        "fn a(x: int) -> int:\n    if x > 0:\n        return 1\n    else:\n        return 2\nfn main():\n    print(a(1))\nmain()\n",
    );
    // exhaustive statement-match where every arm returns -> terminates.
    ok(
        "fn a(x: int) -> int:\n    match x:\n        0: return 1\n        _: return 2\nfn main():\n    print(a(0))\nmain()\n",
    );
    // fn ending in `while true:` with no break never falls off the end -> terminates.
    ok("fn a() -> int:\n    while true:\n        return 1\nfn main():\n    print(a())\nmain()\n");
}

#[test]
fn non_void_fn_exit_terminates() {
    // fn ending in `os.exit(1)` (std.os.exit never returns) -> terminates, not a fall-through.
    entry_ok("import std.os\nfn a() -> int:\n    os.exit(1)\nfn main():\n    print(a())\nmain()\n");
}

#[test]
fn non_void_fn_while_true_with_break_still_rejected() {
    // a `while true:` that CAN break does fall through -> must reject (soundness guard).
    rejects(
        "fn a() -> int:\n    while true:\n        break\nfn main():\n    print(a())\nmain()\n",
        "fall off the end",
    );
}

#[test]
fn non_void_fn_unannotated_conditional_return_not_rejected() {
    // REGRESSION: Option B must fire only for a DECLARED (`-> T`) non-void return. An UN-annotated
    // fn that returns a value on *some* path (the common early-return / `find` idiom) infers a
    // non-nil `sig.ret`, but the user declared no annotation, so it must stay legal.
    // (a) conditional value-return, no `-> T` annotation.
    ok(
        "fn a(x: bool):\n    if x:\n        return helper()\nfn helper() -> int:\n    return 5\nfn main():\n    a(true)\nmain()\n",
    );
    // (b) `find`-style early-return-in-loop, no annotation.
    ok(
        "fn find(xs: List[int], t: int):\n    for x in xs:\n        if x == t:\n            return x\nfn main():\n    find([1, 2, 3], 2)\nmain()\n",
    );
}

// ===== inline-expr fn body implicitly returns its expression (Option A, inline-only) =====

#[test]
fn inline_expr_body_returns_value() {
    // `fn a(): 10` — inline bare-expr body implicitly returns; inferred `-> int`. Binding the
    // result to a typed `int` slot proves the inferred return is int (would be nil before).
    ok("fn a(): 10\nfn main():\n    x: int = a()\n    print(x)\nmain()\n");
    // an inline-expr body used in a value position (the whole point of Option A inline).
    ok("fn dbl(x: int): x * 2\nfn main():\n    print([1, 2, 3].map(dbl))\nmain()\n");
}

#[test]
fn inline_annotated_ret_ok() {
    // `fn a() -> int: 10` is now VALID — the inline expr is the implicit return; Option B must NOT
    // fire on it (the expr terminates the body).
    ok("fn a() -> int: 10\nfn main():\n    print(a())\nmain()\n");
}

#[test]
fn multiline_nonvoid_still_required() {
    // A 1-statement MULTILINE body does NOT implicitly return — Option B still requires an explicit
    // `return` for a declared non-void fn.
    rejects(
        "fn a() -> int:\n    10\nfn main():\n    print(a())\nmain()\n",
        "fall off the end",
    );
}

#[test]
fn inline_non_expr_body_stays_nil() {
    // An inline NON-expression statement (assignment) does NOT implicitly return; it stays void.
    // Using the void result as a value is then rejected (Part 2).
    rejects(
        "fn a():\n    x := 5\nfn main():\n    y := a()\nmain()\n",
        "no value (nil)",
    );
}

// ===== nil used as a value is rejected (Part 2) =====

#[test]
fn nil_in_value_position_rejected() {
    // assignment RHS
    rejects(
        "fn main():\n    x := print(\"hi\")\nmain()\n",
        "no value (nil)",
    );
    // function/call argument
    rejects(
        "fn main():\n    print(print(\"hi\"))\nmain()\n",
        "no value (nil)",
    );
    // collection-literal element
    rejects(
        "fn main():\n    xs := [print(\"hi\")]\nmain()\n",
        "no value (nil)",
    );
    // binary operand
    rejects(
        "fn main():\n    x := 1 + print(\"hi\")\nmain()\n",
        "no value (nil)",
    );
}

#[test]
fn bare_void_call_statement_ok() {
    // A void call AS A STATEMENT is legal — statement position, not value position.
    ok("fn main():\n    print(\"hi\")\nmain()\n");
}

#[test]
fn void_fn_then_used_as_value() {
    // `fn a(): print("x")` infers `-> nil` (a void fn) — the declaration itself is fine.
    ok("fn a(): print(\"x\")\nfn main():\n    a()\nmain()\n");
    // but using its void result as a value is rejected.
    rejects(
        "fn a(): print(\"x\")\nfn main():\n    y := a()\nmain()\n",
        "no value (nil)",
    );
}

#[test]
fn user_fn_arg_nil_rejected() {
    // a void result passed as a USER function's argument (not a builtin).
    rejects(
        "fn takes(n: int):\n    print(\"{n}\")\nfn main():\n    takes(print(\"hi\"))\nmain()\n",
        "no value (nil)",
    );
}

#[test]
fn inline_expr_error_reported_once() {
    // An error inside an inline-expr body with a declared return type must be reported EXACTLY
    // ONCE, not twice. Before the fix the body-statement walk inferred the expr (reporting the
    // error) and the return-assignability check re-inferred it (reporting it again).
    let errs = check_src("fn a() -> int: nope(5)\nfn main():\n    print(a())\nmain()\n");
    let n = errs
        .iter()
        .filter(|e| e.message.contains("unknown name 'nope'"))
        .count();
    assert_eq!(
        n, 1,
        "expected exactly one 'unknown name' error, got: {errs:?}"
    );
    // A type mismatch inside the inline expr is likewise reported once.
    let errs = check_src("fn a() -> int: \"x\" + 1\nfn main():\n    print(a())\nmain()\n");
    assert_eq!(
        errs.len(),
        1,
        "expected exactly one type error for the inline expr, got: {errs:?}"
    );
}

#[test]
fn inline_nonnil_expr_against_nil_ret_rejected() {
    // A NON-nil inline expr against an explicit `-> nil` is a soundness hole if accepted: the
    // engines emit Return(10) for a void-typed fn. Reject it with the same diagnostic the
    // multiline path uses.
    rejects(
        "fn a() -> nil: 10\nfn main():\n    a()\nmain()\n",
        "function returns nothing, cannot return a value",
    );
    // A void fn whose inline expr is itself nil-typed stays legal (implicitly returns nil).
    ok("fn a(): print(\"x\")\nfn main():\n    a()\nmain()\n");
    ok("fn a() -> nil: print(\"x\")\nfn main():\n    a()\nmain()\n");
}

// ===== user-callable panic(msg) builtin (raises a recoverable RuntimeError; bottom-typed) =====

#[test]
fn panic_typechecks_in_tail_position_no_missing_return() {
    // A fn body ending in `panic(...)` diverges — no explicit `return` required.
    ok("fn f() -> int:\n    panic(\"x\")\nfn main():\n    print(\"ok\")\nmain()\n");
    // A branch ending in `panic(...)` satisfies the all-paths-return rule.
    ok(
        "fn f(b: bool) -> int:\n    if b:\n        return 1\n    panic(\"x\")\nfn main():\n    print(\"ok\")\nmain()\n",
    );
}

#[test]
fn panic_typechecks_in_value_position_as_bottom() {
    // `if cond: a else: panic(...)` types as `a`'s type (bottom absorbs into the concrete branch).
    ok("fn main():\n    x := if true: 1 else: panic(\"no\")\n    print(x + 1)\nmain()\n");
    // Bare-statement and value-binding forms both type-check.
    ok("fn main():\n    panic(\"boom\")\nmain()\n");
}

#[test]
fn panic_requires_a_str_argument() {
    rejects(
        "fn main():\n    panic(123)\nmain()\n",
        "panic() expects a str",
    );
}

#[test]
fn panic_arity_is_exactly_one() {
    rejects("fn main():\n    panic()\nmain()\n", "panic");
    rejects("fn main():\n    panic(\"a\", \"b\")\nmain()\n", "panic");
}

#[test]
fn inline_diverging_body_infers_nil() {
    // L3-1: a fn whose SOLE body is a diverging call (`fn f(): panic(...)`) has no annotation and
    // no `return` — its inline-expr type is bottom (Unknown). It must default to `-> nil` (matching
    // a void body), NOT trip "cannot infer return type". The caller can't use a value anyway.
    ok("fn boom(): panic(\"x\")\nfn main():\n    boom()\nmain()\n");
    // Regression: a void body still infers nil; an annotated diverging body still ok.
    ok("fn v(): print(\"x\")\nfn main():\n    v()\nmain()\n");
    ok("fn b() -> int: panic(\"x\")\nfn main():\n    print(\"ok\")\nmain()\n");
    // panic's arg checks still fire through the inline body (Unknown-default doesn't skip pass 2).
    rejects(
        "fn boom(): panic(123)\nfn main():\n    boom()\nmain()\n",
        "panic() expects a str",
    );
}

#[test]
fn panic_is_reserved_against_extern_shadowing() {
    assert!(is_reserved_name("panic"));
}

#[test]
fn user_method_named_panic_does_not_suppress_missing_return() {
    // A user method literally named `panic` compiles to CallMethod and RETURNS normally — it does
    // NOT diverge. A `-> int` body whose tail is `p.panic(...)` must still be rejected for falling
    // off the end (only the bare builtin call `panic(...)` is a divergence).
    rejects(
        "struct P:\n    x: int\n    fn panic(self, m: str) -> int:\n        return -1\nfn f(p: P, ok: bool) -> int:\n    if ok:\n        return 100\n    p.panic(\"bad\")\nfn main():\n    print(\"ok\")\nmain()\n",
        "can fall off the end",
    );
}

// ===== enum methods =====

#[test]
fn enum_method_call_typechecks() {
    ok(
        "enum Color:\n    Red\n    Green\n    fn area(self) -> int:\n        match self:\n            Color.Red: return 1\n            Color.Green: return 2\nfn main():\n    c := Color.Red\n    x: int = c.area()\n    print(x)\nmain()\n",
    );
}

#[test]
fn enum_method_missing_rejected() {
    rejects(
        "enum Color:\n    Red\n    Green\nfn main():\n    c := Color.Red\n    c.foo()\nmain()\n",
        "no method 'foo'",
    );
}

#[test]
fn enum_method_returns_new_variant_ok() {
    ok(
        "enum Sw:\n    On\n    Off\n    fn flip(self) -> Sw:\n        match self:\n            Sw.On: return Sw.Off\n            Sw.Off: return Sw.On\nfn main():\n    s := Sw.On\n    print(s.flip().flip() == s)\nmain()\n",
    );
}

#[test]
fn generic_enum_method_uses_type_param_ok() {
    ok(
        "enum Box[T]:\n    Val(T)\n    fn get(self) -> T:\n        match self:\n            Box.Val(x): return x\nfn main():\n    b := Box.Val(5)\n    n: int = b.get()\n    print(n)\nmain()\n",
    );
}

#[test]
fn enum_str_satisfies_stringable_ok() {
    ok(
        "enum Color:\n    Red\n    Green\n    fn str(self) -> str:\n        match self:\n            Color.Red: return \"red\"\n            Color.Green: return \"green\"\nfn main():\n    print(Color.Red)\nmain()\n",
    );
}

#[test]
fn enum_add_bound_into_generic_fn_ok() {
    ok(
        "enum Money:\n    Cents(int)\n    fn add(self, o: Money) -> Money:\n        match self:\n            Money.Cents(a):\n                match o:\n                    Money.Cents(b): return Money.Cents(a + b)\nfn twice[T: Add](x: T) -> T:\n    return x + x\nfn main():\n    m := twice(Money.Cents(3))\n    print(m.add(m) == Money.Cents(12))\nmain()\n",
    );
}

// ===== newtype (M21): nominal distinct types =====

#[test]
fn newtype_construct_ok() {
    ok("newtype UserId = int\nfn main():\n    uid := UserId(10)\n    print(uid)\nmain()\n");
}

#[test]
fn newtype_construct_wrong_arg_rejected() {
    rejects(
        "newtype UserId = int\nfn main():\n    uid := UserId(\"hi\")\nmain()\n",
        "UserId",
    );
}

#[test]
fn newtype_not_assignable_from_underlying_literal() {
    // A bare int literal is NOT assignable to a UserId binding (nominal distinctness).
    rejects(
        "newtype UserId = int\nfn main():\n    x: UserId = 10\nmain()\n",
        "UserId",
    );
}

#[test]
fn newtype_passed_where_underlying_expected_rejected() {
    // needs_int wants a raw int; a UserId must NOT flow in.
    rejects(
        "newtype UserId = int\nfn needs_int(x: int) -> int:\n    return x\nfn main():\n    uid := UserId(10)\n    print(needs_int(uid))\nmain()\n",
        "expected int",
    );
}

#[test]
fn newtype_cast_unwrap_ok() {
    // int(uid) unwraps to the inner int; float(meters) for Meters=float unwraps.
    ok(
        "newtype UserId = int\nnewtype Meters = float\nfn main():\n    uid := UserId(10)\n    n: int = int(uid)\n    m := Meters(2.5)\n    f: float = float(m)\n    print(n)\n    print(f)\nmain()\n",
    );
}

#[test]
fn scalar_cast_rejects_aggregate_arg() {
    // int/float/bool over an aggregate (List/Map/Set/tuple) is outside the scalar-cast domain and
    // always faults at runtime (`{cast}() cannot convert List`). Reject at check — a check-OK-then-
    // run-fault hole. (`str` of an aggregate is a legal display and must still pass.)
    rejects(
        "fn main():\n    print(float([1, 2]))\nmain()\n",
        "float() cannot convert List",
    );
    rejects(
        "fn main():\n    print(int((1, 2)))\nmain()\n",
        "int() cannot convert tuple",
    );
    rejects(
        "fn main():\n    print(bool({1, 2}))\nmain()\n",
        "bool() cannot convert Set",
    );
    rejects(
        "fn main():\n    m := {\"a\": 1}\n    print(int(m))\nmain()\n",
        "int() cannot convert Map",
    );
    // str-of-aggregate is a display, not a scalar cast — still accepted:
    ok("fn main():\n    print(str([1, 2]))\nmain()\n");
}

#[test]
fn newtype_str_underlying_unwrap_ok() {
    // For newtype N = str, str(n) unwraps to the inner str.
    ok(
        "newtype Name = str\nfn main():\n    n := Name(\"bob\")\n    s: str = str(n)\n    print(s)\nmain()\n",
    );
}

#[test]
fn newtype_same_type_arithmetic_ok() {
    // Meters + Meters -> Meters; Meters < Meters -> bool.
    ok(
        "newtype Meters = float\nfn main():\n    a := Meters(1.0)\n    b := Meters(2.0)\n    c: Meters = a + b\n    lt: bool = a < b\n    print(lt)\nmain()\n",
    );
}

#[test]
fn newtype_plus_raw_underlying_rejected() {
    rejects(
        "newtype Meters = float\nfn main():\n    a := Meters(1.0)\n    c := a + 2.0\nmain()\n",
        "cannot apply",
    );
}

#[test]
fn newtype_plus_other_newtype_rejected() {
    rejects(
        "newtype Meters = float\nnewtype Seconds = float\nfn main():\n    a := Meters(1.0)\n    b := Seconds(2.0)\n    c := a + b\nmain()\n",
        "cannot apply",
    );
}

#[test]
fn newtype_method_dispatch_ok() {
    ok(
        "newtype Meters = float:\n    fn double(self) -> Meters:\n        return self + self\nfn main():\n    m := Meters(2.0)\n    d: Meters = m.double()\n    print(d)\nmain()\n",
    );
}

#[test]
fn newtype_add_into_generic_add_bound_ok() {
    // A newtype with its native same-type + passes into fn twice[T: Add].
    ok(
        "newtype Meters = float\nfn twice[T: Add](x: T) -> T:\n    return x + x\nfn main():\n    m := twice(Meters(3.0))\n    print(m)\nmain()\n",
    );
}

#[test]
fn newtype_as_map_key_requires_hash() {
    // Without hash(self), a newtype is NOT a map/set key even if underlying int is hashable.
    rejects(
        "newtype UserId = int\nfn main():\n    m: Map[UserId, str] = {}\nmain()\n",
        "Hashable",
    );
}

#[test]
fn newtype_with_hash_is_map_key_ok() {
    ok(
        "newtype UserId = int:\n    fn hash(self) -> int:\n        return int(self)\nfn main():\n    m: Map[UserId, str] = {}\n    m[UserId(1)] = \"a\"\n    print(m.len())\nmain()\n",
    );
}

#[test]
fn newtype_aggregate_underlying_no_method_inherit() {
    // newtype Names = List[str] does NOT inherit .push() (v1 limit).
    rejects(
        "newtype Names = List[str]\nfn main():\n    ns := Names([\"a\"])\n    ns.push(\"b\")\nmain()\n",
        "push",
    );
}

#[test]
fn newtype_scalar_aggregate_cast_unwrap_ok() {
    // A scalar (non-generic) aggregate newtype crosses the boundary the same explicit way a scalar
    // newtype does: the matching aggregate cast builtin unwraps it. `List(ns)` for `Names = List[str]`
    // yields `List[str]` (mirrors `int(uid)` for `UserId = int`) — distinct type, explicit cast.
    ok(
        "newtype Names = List[str]\nfn main():\n    ns := Names([\"a\", \"b\"])\n    xs: List[str] = List(ns)\n    print(xs.len())\nmain()\n",
    );
    // set / map underlyings unwrap via Set() / Map() likewise (the annotated binding is the assertion
    // that the unwrap yields the matching aggregate type).
    ok(
        "newtype Tags = Set[str]\nfn main():\n    t := Tags({\"x\"})\n    s: Set[str] = Set(t)\n    print(s)\nmain()\n",
    );
    ok(
        "newtype Counts = Map[str, int]\nfn main():\n    c := Counts({\"a\": 1})\n    m: Map[str, int] = Map(c)\n    print(m)\nmain()\n",
    );
}

#[test]
fn newtype_scalar_aggregate_cast_unwrap_wrong_target_rejected() {
    // The unwrap must match the underlying aggregate: `Set(ns)` on a list-backed newtype is rejected
    // (no cross-aggregate coercion) — the explicit cast still respects the underlying's shape.
    rejects(
        "newtype Names = List[str]\nfn main():\n    ns := Names([\"a\"])\n    s := Set(ns)\n    print(s)\nmain()\n",
        "Set",
    );
}

#[test]
fn raw_string_is_str_type() {
    // A raw string is plain `str` everywhere a normal string is — annotating it `str` is clean.
    ok("fn main():\n    s: str = r\"\\d+\"\n    print(s)\nmain()\n");
    // ...and using it where an `int` is expected is rejected (proves it's classified `str`, not int).
    rejects("fn main():\n    n: int = r\"x\"\nmain()\n", "str");
}

// ---- generic newtype (M21): type params, methods-only, turbofish ctor, cast-unwrap ----

#[test]
fn generic_newtype_decl_ok() {
    // `newtype Stack[T] = List[T]` with methods referencing T type-checks.
    ok(
        "newtype Stack[T] = List[T]:\n    fn peek(self) -> Option[T]:\n        return None\nfn main():\n    s := Stack([1, 2])\n    print(List(s).len())\nmain()\n",
    );
}

#[test]
fn generic_newtype_method_body_and_dispatch_ok() {
    // Inside a method `self` is Stack[T]; at the call site Stack[int].peek() returns Option[int].
    ok(
        "newtype Stack[T] = List[T]:\n    fn peek(self) -> Option[T]:\n        return None\nfn main():\n    s := Stack([1, 2])\n    x: Option[int] = s.peek()\n    print(x == None)\nmain()\n",
    );
}

#[test]
fn generic_newtype_dispatch_substitutes_targs() {
    // The substituted return type is enforced: assigning Stack[int].peek() to Option[str] is rejected.
    rejects(
        "newtype Stack[T] = List[T]:\n    fn peek(self) -> Option[T]:\n        return None\nfn main():\n    s := Stack([1, 2])\n    x: Option[str] = s.peek()\nmain()\n",
        "Option[str]",
    );
}

#[test]
fn generic_newtype_ctor_infer_ok() {
    // `Stack([1, 2])` infers Stack[int].
    ok(
        "newtype Stack[T] = List[T]\nfn main():\n    s: Stack[int] = Stack([1, 2])\n    print(List(s).len())\nmain()\n",
    );
}

#[test]
fn generic_newtype_ctor_turbofish_ok() {
    // `Stack[int]([])` — the empty list can't bind T, so the turbofish supplies it.
    ok(
        "newtype Stack[T] = List[T]\nfn main():\n    s: Stack[int] = Stack[int]([])\n    print(List(s).len())\nmain()\n",
    );
}

#[test]
fn generic_newtype_ctor_infer_set_underlying_ok() {
    // A set-underlying generic newtype infers its param from the arg just like list/map —
    // `Bag({1, 2, 3})` ⇒ Bag[int] with NO turbofish (regression: `unify` lacked a `Ty::Set` arm).
    ok(
        "newtype Bag[T: Hashable] = Set[T]\nfn main():\n    b: Bag[int] = Bag({1, 2, 3})\n    s: Set[int] = Set(b)\n    print(s)\nmain()\n",
    );
}

#[test]
fn generic_newtype_ctor_wrong_arg_rejected() {
    // `Stack[int](["a"])` — arg element str vs declared int.
    rejects(
        "newtype Stack[T] = List[T]\nfn main():\n    s := Stack[int]([\"a\"])\nmain()\n",
        "expected",
    );
}

#[test]
fn generic_newtype_cast_unwrap_propagates() {
    // `List(s)` for s: Stack[int] yields List[int].
    ok(
        "newtype Stack[T] = List[T]\nfn main():\n    s := Stack([1, 2])\n    xs: List[int] = List(s)\n    print(xs.len())\nmain()\n",
    );
}

#[test]
fn generic_newtype_cast_unwrap_wrong_elem_rejected() {
    // `List(s)` for s: Stack[int] is NOT List[str].
    rejects(
        "newtype Stack[T] = List[T]\nfn main():\n    s := Stack([1, 2])\n    xs: List[str] = List(s)\nmain()\n",
        "List[str]",
    );
}

#[test]
fn generic_newtype_box_scalar_cast_unwrap_ok() {
    // `newtype Box[T] = T`; int(Box(5)) unwraps to the substituted underlying int.
    ok(
        "newtype Box[T] = T\nfn main():\n    b: Box[int] = Box(5)\n    n: int = int(b)\n    print(n)\nmain()\n",
    );
}

#[test]
fn generic_newtype_methods_only_no_operator_autoflow() {
    // `newtype Box[T] = T` — Box(1) + Box(2) is REJECTED (methods-only, no native auto-flow), even
    // though the underlying int is numeric. Operators come only from the newtype's own methods.
    rejects(
        "newtype Box[T] = T\nfn main():\n    a := Box(1)\n    b := Box(2)\n    c := a + b\nmain()\n",
        "cannot apply",
    );
}

#[test]
fn generic_newtype_not_into_add_bound() {
    // A generic newtype over a numeric T does NOT satisfy the intrinsic Add bound (methods-only).
    rejects(
        "newtype Box[T] = T\nfn twice[U: Add](x: U) -> U:\n    return x + x\nfn main():\n    print(twice(Box(3)))\nmain()\n",
        "Add",
    );
}

#[test]
fn generic_newtype_own_method_dispatch_ok() {
    // A generic newtype's OWN method is its only operator surface (methods-only). The method
    // dispatches with the newtype's type args substituted (`Box[int].combine` returns `Box[int]`).
    // (NB: satisfying a protocol BOUND from a generic instantiation is a pre-existing limitation
    // shared with generic structs — `compatible(Box[int], Box[T])` fails — so we test the direct
    // dispatch, not the `[U: Add]` bound.)
    ok(
        "newtype Box[T] = T:\n    fn combine(self, other: Box[T]) -> Box[T]:\n        return self\nfn main():\n    a := Box(1)\n    b := Box(2)\n    c: Box[int] = a.combine(b)\n    print(int(c))\nmain()\n",
    );
}

#[test]
fn generic_newtype_bound_smoke_ok() {
    // Bounds on newtype params come along for free via enter_type_params/check_bounds/enforce_bounds.
    ok(
        "newtype Keyed[T: Hashable] = List[T]\nfn main():\n    k: Keyed[int] = Keyed([1, 2])\n    print(List(k).len())\nmain()\n",
    );
}

#[test]
fn generic_newtype_bound_violation_rejected() {
    // A type arg that violates the param bound is rejected at the annotation site.
    rejects(
        "newtype Keyed[T: Hashable] = List[T]\nfn main():\n    k: Keyed[fn(int) -> int] = Keyed([])\nmain()\n",
        "Hashable",
    );
}

#[test]
fn generic_newtype_missing_targs_rejected() {
    // Bare `Stack` as an annotation (no args) on a generic newtype is rejected.
    rejects(
        "newtype Stack[T] = List[T]\nfn main():\n    s: Stack = Stack([1])\nmain()\n",
        "type argument",
    );
}

// ============================================================================
// refine-on-first-use: empty-collection element/key/value Unknown slots
// (the empty-slot half of the Ty::Unknown soundness family). An empty literal's
// element/key/value (or nullary-variant type arg / native None) is Unknown; the
// FIRST mutating op that supplies a concrete type re-pins the binding, so a later
// conflicting op is a normal error and the Hashable/float-key ban runs when the
// key/element becomes concrete. Refinement is BLOCK-LOCAL flow-sensitive: a
// refinement inside one branch arm does not leak into a sibling arm or post-branch.
// ============================================================================

// ---- step 1: straight-line refine pins the element, later conflict rejected ----

#[test]
fn empty_list_push_pins_element_then_mixed_rejected() {
    // x:=[]; x.push(1) pins List[int]; x.push("s") is then a normal mismatch.
    rejects(
        "fn main():\n x := []\n x.push(1)\n x.push(\"s\")\nmain()",
        "pinned",
    );
}

#[test]
fn refine_erroring_push_arg_reports_once() {
    // Regression: the speculative arg-infer in refine_receiver must roll its diagnostics back so an
    // erroring mutator arg (`xs.push(undefined_v)`) is reported exactly ONCE by the real dispatch
    // path, not duplicated. (Empty-collection refine over-reported on base of this branch.)
    let errs = check_src("fn main():\n xs := []\n xs.push(undefined_v)\nmain()");
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert!(
        errs[0].message.contains("unknown name"),
        "got: {:?}",
        errs[0]
    );
}

#[test]
fn refine_erroring_index_key_reports_once() {
    // Same rollback for the index-assign refine path (`m[undefined_k] = 1`).
    let errs = check_src("fn main():\n m := {}\n m[undefined_k] = 1\nmain()");
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert!(
        errs[0].message.contains("unknown name"),
        "got: {:?}",
        errs[0]
    );
}

#[test]
fn empty_list_of_none_then_conflicting_some_rejected() {
    // [None] is List[Option[Unknown]]; push(Some(5)) refines to List[Option[int]];
    // push(Some("hi")) then conflicts (nested-typeparam + native None producer).
    rejects(
        "fn main():\n xs := [None]\n xs.push(Some(5))\n xs.push(Some(\"hi\"))\nmain()",
        "expected",
    );
}

#[test]
fn empty_list_of_nullary_enum_then_conflicting_variant_rejected() {
    // [Box.Empty] is List[Box[Unknown]]; push(Box.Full("hi")) refines to List[Box[str]];
    // push(Box.Full(5)) then conflicts (nullary-variant producer).
    rejects(
        "enum Box[T]:\n Full(T)\n Empty\nfn main():\n xs := [Box.Empty]\n xs.push(Box.Full(\"hi\"))\n xs.push(Box.Full(5))\nmain()",
        "expected",
    );
}

// ---- step 2: insertion-site Hashable / float-key ban on empty {}/Set() ----

#[test]
fn empty_map_float_key_rejected() {
    // m:={}; m[1.5]="b" — float key must be rejected even though key type is Unknown.
    rejects("fn main():\n m := {}\n m[1.5] = \"b\"\nmain()", "Hashable");
}

#[test]
fn empty_set_float_and_nan_rejected() {
    // s:=Set(); s.add(1.5) and the inf-inf NaN add — both non-Hashable, rejected.
    rejects("fn main():\n s := Set()\n s.add(1.5)\nmain()", "Hashable");
    rejects(
        "fn main():\n big := 1e308\n inf := big * 10.0\n nan := inf - inf\n s := Set()\n s.add(nan)\nmain()",
        "Hashable",
    );
}

// ---- step 3: un-annotated heterogeneous struct list rejected with annotation hint ----

#[test]
fn heterogeneous_struct_list_unannotated_rejected() {
    // shapes:=[]; push Sq; push Rect — the pin makes the 2nd push a mismatch; the
    // diagnostic must hint at annotating List[<protocol>].
    let errs = check_src(
        "struct Sq:\n s: int\nstruct Rect:\n w: int\n h: int\nfn main():\n shapes := []\n shapes.push(Sq(3))\n shapes.push(Rect(2, 4))\nmain()",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("pinned") && e.message.contains("annotate")),
        "expected a pinned/annotate hint, got: {errs:?}"
    );
}

#[test]
fn leaked_param_push_emits_uninferred_param_msg() {
    // `empty[T]()` has a return-only type param T with nothing to infer it from, so `xs` is
    // `List[<unbound Param T>]`. The FIRST push then mismatches. The diagnostic must NOT use the
    // wrong "earlier push" narrative (this is the first push) and must accurately point at the
    // un-inferred type parameter / construction-site fix.
    let errs = check_src("fn empty[T]() -> List[T]:\n return []\nxs := empty()\nxs.push(5)\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("un-inferred type parameter")),
        "expected the un-inferred-param message, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("earlier")),
        "must not use the 'earlier push' narrative, got: {errs:?}"
    );
}

#[test]
fn pinned_hint_preserved_for_concrete_collection() {
    // The genuine first-push-pins case (concrete element type Int) must KEEP the original
    // "pinned by an earlier push" narrative and NOT use the un-inferred-param message.
    let errs = check_src("fn main():\n xs := []\n xs.push(1)\n xs.push(\"s\")\nmain()");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("pinned") && e.message.contains("earlier")),
        "expected the original pinned/earlier hint, got: {errs:?}"
    );
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("un-inferred type parameter")),
        "concrete pin must not use the un-inferred-param message, got: {errs:?}"
    );
}

#[test]
fn pinned_hint_preserved_for_bound_generic_param() {
    // Inside a generic fn, the first push pins the element type to the IN-SCOPE, legitimately-bound
    // type param T; a later wrong-typed push is a genuine "earlier push" pin. The expected type is
    // a `Ty::Param`, but because T is bound, the diagnostic must KEEP the original earlier-push
    // narrative and NOT the un-inferred-param message (which only fits an un-bound/leaked param).
    let errs = check_src("fn f[T](x: T):\n xs := []\n xs.push(x)\n xs.push(\"s\")\nf(1)\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("pinned") && e.message.contains("earlier")),
        "bound-param pin must keep the original earlier-push hint, got: {errs:?}"
    );
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("un-inferred type parameter")),
        "bound-param pin must not use the un-inferred-param message, got: {errs:?}"
    );
}

// ---- PART A: a never-constrained empty collection requires an annotation ----

#[test]
fn unconstrained_empty_list_rejected() {
    // `b := []` whose element type is NEVER inferred (only read) is now a static error.
    rejects(
        "fn main():\n b := []\n print(b)\nmain()",
        "empty collection",
    );
}

#[test]
fn unconstrained_empty_map_rejected() {
    rejects(
        "fn main():\n b := {}\n print(b)\nmain()",
        "empty collection",
    );
}

#[test]
fn unconstrained_empty_set_rejected() {
    rejects(
        "fn main():\n b := Set()\n print(b)\nmain()",
        "empty collection",
    );
}

#[test]
fn unconstrained_empty_at_module_level_rejected() {
    // top-level script binding, never constrained → error (caught at the module seam).
    rejects("b := []\nprint(b)\n", "empty collection");
}

// false-positive matrix: a typed sink unifies the Unknown away → NO error.

#[test]
fn typed_annotation_empty_list_ok() {
    ok("fn main():\n b: List[int] = []\n print(b)\nmain()");
}

#[test]
fn typed_annotation_empty_map_ok() {
    ok("fn main():\n m: Map[str, int] = {}\n print(m)\nmain()");
}

#[test]
fn typed_annotation_empty_set_ok() {
    ok("fn main():\n s: Set[int] = Set()\n print(s)\nmain()");
}

#[test]
fn typed_param_empty_arg_ok() {
    // `f([])` where the param is List[int] — the literal flows into a typed sink, no local site.
    ok("fn f(xs: List[int]):\n print(xs.len())\nfn main():\n f([])\nmain()");
}

#[test]
fn typed_return_empty_ok() {
    ok("fn g() -> List[int]:\n return []\nfn main():\n print(g().len())\nmain()");
}

#[test]
fn turbofish_empty_ctor_ok() {
    ok("fn main():\n b := List[int]()\n print(b)\nmain()");
}

#[test]
fn turbofish_empty_ctor_from_list_ok() {
    ok("fn main():\n b := List[int]([])\n print(b)\nmain()");
}

#[test]
fn empty_push_then_read_no_false_error() {
    // refine-on-first-use constrains the binding → no annotation required.
    ok("fn main():\n out := []\n out.push(1)\n print(out)\nmain()");
}

// false-positive matrix #2 — a binding constrained by a CONCRETE value flowing into it (NOT just the
// two refine-on-first-use gates: push/add/insert/extend + index-assign) must NOT be flagged. These
// were rejected by the original impl (drop_empty_site wired only into the two refine gates).

#[test]
fn empty_then_plain_reassign_concrete_ok() {
    // `b := []` then a whole-binding reassignment `b = [1, 2, 3]` determines the element type → no
    // annotation required (the binding IS constrained).
    ok("fn main():\n b := []\n b = [1, 2, 3]\n print(b)\nmain()");
}

#[test]
fn empty_then_compound_assign_concrete_ok() {
    // `b := []` then `b += [1, 2, 3]` (compound list-extend) constrains the element type.
    ok("fn main():\n b := []\n b += [1, 2, 3]\n print(b)\nmain()");
}

#[test]
fn empty_then_tuple_assign_concrete_ok() {
    // tuple-assignment `a, b = [1], [2]` constrains both bindings (recurses into the Ident arm).
    ok("fn main():\n a := []\n b := []\n a, b = [1], [2]\n print(a)\n print(b)\nmain()");
}

#[test]
fn empty_then_reassign_from_call_ok() {
    // `result := []` then `result = compute()` where `compute() -> List[int]` constrains it.
    ok(
        "fn compute() -> List[int]:\n return [1, 2]\nfn main():\n result := []\n result = compute()\n print(result)\nmain()",
    );
}

#[test]
fn empty_binding_into_typed_param_ok() {
    // the spec's typed-parameter false-positive guard, one binding away: `f(b)` where the param is
    // `List[int]` constrains `b` (the direct-literal form `f([])` is covered separately above).
    ok("fn f(xs: List[int]):\n print(xs.len())\nfn main():\n b := []\n f(b)\nmain()");
}

#[test]
fn empty_then_conditional_reassign_ok() {
    // 'declare empty, fill in a branch' idiom — the reassignment lives in an inner block but
    // constrains the fn-scope binding.
    ok("fn main():\n out := []\n if true:\n  out = [\"x\"]\n print(out)\nmain()");
}

#[test]
fn empty_binding_into_typed_return_ok() {
    // the typed-return false-positive guard, one binding away: `b := []` then `return b` where the
    // return type is `List[int]` constrains `b` (the direct-literal form `return []` is covered above).
    ok("fn g() -> List[int]:\n b := []\n return b\nfn main():\n print(g().len())\nmain()");
}

#[test]
fn empty_then_reassign_still_empty_rejected() {
    // GUARD: reassigning ANOTHER empty literal does NOT constrain — still no element type → error.
    rejects(
        "fn main():\n b := []\n b = []\n print(b)\nmain()",
        "empty collection",
    );
}

#[test]
fn empty_into_typed_binding_value_ok() {
    // REGRESSION (bug #1): `b := []` then `c: List[int] = b` — binding the empty into a
    // CONCRETE-typed annotated let constrains b's element type (the spec's typed-binding
    // false-positive guard, one binding away from `b: List[int] = []`). The annotated-let branch
    // must drop b's pending site; base accepts this.
    ok("fn main():\n b := []\n c: List[int] = b\n print(c.len())\nmain()");
}

#[test]
fn empty_captured_then_push_ok() {
    // REGRESSION (bug #2): `acc := []` then a `spawn:` body that supplies the element via
    // `acc.push(1)` constrains acc — the capture early-return in `refine_receiver` must still drop
    // the pending annotation site. Base accepts this; the element type IS supplied (via push).
    ok("fn main():\n acc := []\n spawn:\n  acc.push(1)\n print(acc)\nmain()");
}

// false-positive matrix #3 — an empty binding READ AS A VALUE that ESCAPES into another binding or
// structure (RHS of plain/field assign, untyped alias, or nested in a collection literal) is no
// longer provably-unconstrained and must NOT be flagged. These regressed when the feature landed:
// the drop-guard covered only typed sinks + the LHS-target of reassign, never the RHS source.

#[test]
fn empty_into_plain_assign_target_ok() {
    // REGRESSION: `c := [1]` (List[int]) then `c = b` (b := []) flows b into a typed slot via plain
    // `=`. Sound (annotated `c: List[int] = b` accepts); must not error on b.
    ok("fn main():\n c := [1]\n b := []\n c = b\n print(c)\nmain()");
}

#[test]
fn empty_into_field_assign_ok() {
    // REGRESSION: assigning an empty binding into a CONCRETE-typed struct field via `bx.items = b`
    // constrains b's element type — must not error on b.
    ok(
        "struct Box:\n items: List[int]\nfn main():\n bx := Box([1])\n b := []\n bx.items = b\n print(bx.items)\nmain()",
    );
}

#[test]
fn empty_alias_then_push_ok() {
    // REGRESSION: `c := b` aliases the same list; `c.push(1)` establishes the element type. b escapes
    // into the alias, so its pending site must drop (annotated `b: List[int] = []` runs, prints [1]).
    ok("fn main():\n b := []\n c := b\n c.push(1)\n print(b)\nmain()");
}

#[test]
fn empty_nested_in_list_literal_then_push_ok() {
    // REGRESSION: `c := [b]` nests b in a list literal; b escapes and its site must drop.
    ok("fn main():\n b := []\n c := [b]\n c[0].push(1)\n print(b)\nmain()");
}

#[test]
fn empty_alias_both_unconstrained_still_rejected() {
    // GUARD (no new false-negative): aliasing drops the SOURCE site, but the alias `c` is itself an
    // unrefined empty — the requirement moves to c, it does not vanish. Still an error.
    rejects(
        "fn main():\n b := []\n c := b\n print(c)\nmain()",
        "empty collection",
    );
}

// ---- step 4: PERSISTENT refine-on-first-use — the first use pins the element/key/value type
// for the binding's whole scope, even across sibling STATEMENT branches/arms. Building a
// heterogeneous collection split across branches is now a type error, exactly like `[1, "s"]`. ----

#[test]
fn flow_sensitive_if_else_int_vs_str_rejects() {
    // First-use pin PERSISTS: the then-arm pins xs to List[int]; the else-arm's str push is a
    // cross-branch element-type conflict — rejected (a sound static over-approximation).
    rejects(
        "fn main():\n c := true\n xs := []\n if c:\n  xs.push(1)\n else:\n  xs.push(\"s\")\nmain()",
        "argument 1 of 'push': expected int, found str",
    );
}

#[test]
fn flow_sensitive_map_if_elif_rejects() {
    // The first arm pins cfg to Map[str,int]; the else-if writes a float value. float→int is the
    // LOSSY direction (NOT widened — consistent with one-way int→float widening) → rejected.
    rejects(
        "fn main():\n c := 1\n cfg := {}\n if c == 1:\n  cfg[\"x\"] = 1\n elif c == 2:\n  cfg[\"y\"] = 2.0\nmain()",
        "cannot assign float to int",
    );
}

#[test]
fn flow_sensitive_set_if_else_rejects() {
    // First-use pin persists across sibling arms: set pinned to Set[int], the str add is rejected.
    rejects(
        "fn main():\n c := true\n s := Set()\n if c:\n  s.add(1)\n else:\n  s.add(\"x\")\nmain()",
        "argument 1 of 'add': expected int, found str",
    );
}

// ---- step 5: an inner-block first-use pin PERSISTS; a later conflicting use is rejected ----

#[test]
fn refine_inside_block_persists_then_conflict_rejected() {
    // The if-arm's push(1) pins xs to List[int] for the whole scope (the pin is written to the
    // OWNING outer scope by `repin` and survives the block's `pop_scope`). The post-if push("s")
    // is therefore a real element-type conflict — rejected. (Persistent first-use pinning replaces
    // the old block-local "does not leak" design.)
    rejects(
        "fn main():\n xs := []\n if true:\n  xs.push(1)\n xs.push(\"s\")\nmain()",
        "argument 1 of 'push': expected int, found str",
    );
}

#[test]
fn refine_inside_block_on_outer_list_ok() {
    // repin targets the OWNING (outer) scope; a HOMOGENEOUS build across block boundary stays fine
    // — the persistent pin only rejects a CONFLICTING later use, not a matching one.
    ok("fn main():\n xs := []\n if true:\n  xs.push(1)\n xs.push(2)\nmain()");
}

// ---- repros 2-5: persistent-pin rejections (single arm then concrete use, second-arm conflict,
// statement-match arm conflict, loop-body pin then post-loop conflict) ----

#[test]
fn refine_single_arm_then_concrete_use_rejects() {
    // One arm pins xs to List[int]; the annotated read `s: str = xs[0]` then mismatches.
    rejects(
        "fn main():\n c := true\n xs := []\n if c:\n  xs.push(1)\n s: str = xs[0]\nmain()",
        "cannot assign int to variable of type str",
    );
}

#[test]
fn refine_conflict_in_second_arm_rejects() {
    // Homogeneous first arm pins List[int]; the conflict lands in the SECOND (else-if) arm.
    rejects(
        "fn main():\n c := 1\n xs := []\n if c == 1:\n  xs.push(1)\n elif c == 2:\n  xs.push(\"s\")\nmain()",
        "argument 1 of 'push': expected int, found str",
    );
}

#[test]
fn refine_stmt_match_arm_conflict_rejects() {
    // Statement-`match` arms mirror if/else statements (Option B): the `1:` arm pins List[int],
    // the `_:` arm's str push is a hard cross-arm conflict.
    rejects(
        "fn main():\n c := 1\n xs := []\n match c:\n  1:\n   xs.push(1)\n  _:\n   xs.push(\"s\")\nmain()",
        "argument 1 of 'push': expected int, found str",
    );
}

#[test]
fn refine_loop_body_pin_then_post_loop_conflict_rejects() {
    // The for-loop body pins xs to List[int]; the post-loop str push conflicts. Accepts the
    // zero-trip / always-runs over-approximation by design (sound static over-approximation).
    rejects(
        "fn main():\n xs := []\n for i in [1,2]:\n  xs.push(i)\n xs.push(\"s\")\nmain()",
        "argument 1 of 'push': expected int, found str",
    );
    // while variant: condition guards the body, but the body's first-use pin still persists.
    rejects(
        "fn main():\n n := 3\n xs := []\n while n > 0:\n  xs.push(1)\n  n = n - 1\n xs.push(\"s\")\nmain()",
        "argument 1 of 'push': expected int, found str",
    );
}

#[test]
fn refine_zero_trip_loop_over_approximation_rejects() {
    // Zero-trip over-approximation made explicit: the loop body never runs at runtime, yet the
    // static pin still fires — `xs:=[]; for i in []: xs.push(1); xs.push("s")` REJECTS by design.
    rejects(
        "fn main():\n xs := []\n for i in []:\n  xs.push(1)\n xs.push(\"s\")\nmain()",
        "argument 1 of 'push': expected int, found str",
    );
}

#[test]
fn expr_arm_pin_independence_ok() {
    // GUARD: the EXPRESSION-position arms (`infer_if_else`/`infer_match`) keep their
    // snapshot/restore barrier so a refinable empty produced/refined inside one value-arm refines
    // independently from its sibling — value-arm inference must not be disturbed by the persistent-
    // pin change. An if-EXPRESSION whose two arms each yield an empty list must unify to
    // List[Unknown] and then refine cleanly on first use; a later conflicting use is rejected
    // because the RESULT binding's first-use pin persists (statement-position). This fails if an
    // expression-site restore is wrongly removed (sibling-arm pins would leak and corrupt inference).
    ok("fn main():\n c := true\n xs := (if c: [] else: [])\n xs.push(1)\n xs.push(2)\nmain()");
    rejects(
        "fn main():\n c := true\n xs := (if c: [] else: [])\n xs.push(1)\n xs.push(\"s\")\nmain()",
        "argument 1 of 'push': expected int, found str",
    );
    // Expression-`match` arms yielding empties unify the same way (exercises `infer_match`).
    ok("fn main():\n c := 1\n xs := match c:\n  1: []\n  _: []\n xs.push(1)\n xs.push(2)\nmain()");
}

// ---- step 6: invariants — never-refined empties, homogeneous builds, residual hole ----

#[test]
fn never_refined_empty_needs_annotation() {
    // PART A: a never-constrained empty (only read, never refined) now requires an annotation —
    // un-annotated it errors, and the annotated form is the escape hatch.
    rejects(
        "fn main():\n empty := {}\n print(empty)\nmain()",
        "empty collection",
    );
    ok(
        "fn main():\n empty: Map[str, int] = {}\n print(empty)\n xs: List[int] = []\n print(xs)\nmain()",
    );
}

#[test]
fn idiomatic_homogeneous_push_ok() {
    ok(
        "fn main():\n out := []\n out.push(1)\n out.push(2)\n s := Set()\n s.add(\"a\")\n s.add(\"b\")\n m := {}\n m[\"k\"] = 1\n m[\"j\"] = 2\nmain()",
    );
}

#[test]
fn single_nullary_enum_push_stays_ok() {
    // v7.chz shape: one non-conflicting Box.Full push after Box.Empty stays accepted.
    ok(
        "enum Box[T]:\n Full(T)\n Empty\nfn main():\n c := Box.Empty\n xs := [c]\n xs.push(Box.Full(\"hi\"))\nmain()",
    );
}

#[test]
fn annotated_heterogeneous_list_ok() {
    // The intended escape hatch: an explicit List[Shape] annotation accepts mixed structs
    // sharing the protocol (refinement never engages on an already-concrete element type).
    ok(
        "protocol Shape:\n fn area(self) -> int\nstruct Sq:\n s: int\n fn area(self) -> int:\n  return self.s * self.s\nstruct Rect:\n w: int\n h: int\n fn area(self) -> int:\n  return self.w * self.h\nfn main():\n shapes: List[Shape] = []\n shapes.push(Sq(3))\n shapes.push(Rect(2, 4))\nmain()",
    );
}

#[test]
fn nonident_receiver_not_refined_documented_hole() {
    // Residual hole: refine only fires on a simple-variable receiver. A non-Ident receiver
    // (Index expr xss[0]) is never refined — the mixed push stays accepted (documented).
    ok("fn main():\n xss := [[]]\n xss[0].push(1)\n xss[0].push(\"s\")\nmain()");
}

// ---- step 7: golden-test checker-bypass fix — every shipped example type-checks ----

#[test]
fn all_shipped_examples_typecheck() {
    // The golden example tests drive run_capture, which BYPASSES the Checker — so a checker
    // regression on a shipped example would ship FALSELY GREEN. This routes every
    // examples/*.chz through the real checked path (build_graph + check_graph, mirroring
    // `chezzi check`) so example type-errors are caught from now on.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    // `panic.chz` is an INTENTIONAL `chezzi check` failure on base (a deliberately run-only demo the
    // golden VM tests exercise via `run`, which bypasses the checker): a top-level `panic(...)` demo
    // whose result is used in value position. Allow-listed so this test catches NEW checker
    // regressions on the OTHER examples without being blocked by that known hole.
    // (`explicit_type_args.chz` USED to be allow-listed too — its struct-ctor turbofish was wrongly
    // rejected by a `name_is_generic` keying bug; fixed, so it now type-checks and is verified here.)
    let known_check_failures = ["panic.chz"];
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("examples dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("chz"))
        .filter(|p| {
            !p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| known_check_failures.contains(&n))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();
    assert!(!entries.is_empty(), "no examples found in {dir:?}");
    let mut failures = Vec::new();
    for path in &entries {
        match crate::resolver::build_graph(path) {
            Ok(graph) => {
                if let Err(errs) = crate::checker::check_graph(&graph) {
                    failures.push(format!("{}: {:?}", path.display(), errs));
                }
            }
            Err(e) => failures.push(format!("{}: resolve/parse error: {e}", path.display())),
        }
    }
    assert!(
        failures.is_empty(),
        "these shipped examples fail `chezzi check`:\n{}",
        failures.join("\n")
    );
}

#[test]
fn all_std_files_standalone_typecheck() {
    // Std modules rely on stdlib AUTO-PRIVILEGE (e.g. `std/concurrency/collection.chz` uses bare
    // `RwShared[Map[K, V]]` field types with no `import` of its own). That privilege used to be
    // granted ONLY on the import path, so opening a std file directly (`chezzi check std/foo.chz`,
    // or an editor/LSP) reported phantom "unknown type 'RwShared'" errors. The path-aware
    // `LoadedModule::is_std` now grants it on the standalone-entry path too — assert every shipped
    // `std/**/*.chz` type-checks clean as an ENTRY (build_graph + check_graph, mirroring `chezzi check`).
    fn collect_chz(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("std dir readable").flatten() {
            let p = entry.path();
            if p.is_dir() {
                collect_chz(&p, out);
            } else if p.extension().and_then(|s| s.to_str()) == Some("chz") {
                out.push(p);
            }
        }
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("std");
    let mut entries = Vec::new();
    collect_chz(&dir, &mut entries);
    entries.sort();
    assert!(!entries.is_empty(), "no std files found in {dir:?}");
    let mut failures = Vec::new();
    for path in &entries {
        match crate::resolver::build_graph(path) {
            Ok(graph) => {
                if let Err(errs) = crate::checker::check_graph(&graph) {
                    failures.push(format!("{}: {:?}", path.display(), errs));
                }
            }
            Err(e) => failures.push(format!("{}: resolve/parse error: {e}", path.display())),
        }
    }
    assert!(
        failures.is_empty(),
        "these std files fail standalone `chezzi check`:\n{}",
        failures.join("\n")
    );
}

#[test]
fn all_bench_corpus_typecheck() {
    // The bench corpus `benches/chz/*.chz` is run by `benches/run.chz` via `chezzi run`, which does
    // a pre-run type check (src/main.rs) and BLOCKS execution on any type error. `cargo test` does
    // not otherwise exercise these files, so a stale lowercase `list[...]`/`map[...]`/`set[...]`
    // type annotation (now rejected after the hard list->List/map->Map/set->Set rename) would ship
    // FALSELY GREEN and break the documented perf-measurement entrypoint. Route every bench file
    // through the real checked path so such regressions are caught.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benches")
        .join("chz");
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("benches/chz dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("chz"))
        .collect();
    entries.sort();
    assert!(!entries.is_empty(), "no bench corpus found in {dir:?}");
    let mut failures = Vec::new();
    for path in &entries {
        match crate::resolver::build_graph(path) {
            Ok(graph) => {
                if let Err(errs) = crate::checker::check_graph(&graph) {
                    failures.push(format!("{}: {:?}", path.display(), errs));
                }
            }
            Err(e) => failures.push(format!("{}: resolve/parse error: {e}", path.display())),
        }
    }
    assert!(
        failures.is_empty(),
        "these bench corpus files fail `chezzi check`:\n{}",
        failures.join("\n")
    );
}

// === Import name collisions (soundness) + duplicate binder in one pattern ===

/// Build a multi-file graph from (rel, src) pairs (first must be `main.chz`) and check it.
fn check_files(files: &[(&str, &str)]) -> Vec<CheckError> {
    let t = TmpDir::new();
    let mut entry = None;
    for (rel, src) in files {
        if let Some(parent) = std::path::Path::new(rel).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(t.0.join(parent)).unwrap();
        }
        let p = t.write(rel, src);
        if *rel == "main.chz" {
            entry = Some(p);
        }
    }
    let graph =
        crate::resolver::build_graph(&entry.expect("a main.chz")).expect("resolve should succeed");
    match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    }
}

fn files_reject(files: &[(&str, &str)], needle: &str) {
    let errs = check_files(files);
    assert!(
        errs.iter().any(|e| e.message.contains(needle)),
        "expected an error containing {needle:?}, got: {errs:?}"
    );
}

fn files_ok(files: &[(&str, &str)]) {
    let errs = check_files(files);
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
}

#[test]
fn struct_match_qualified_whole_module_ok() {
    // Bug #1: a struct reached via a WHOLE-module import (`import geo`, referenced `geo.Point`) is
    // only spellable qualified — the bare name `Point` is not in scope. The qualified struct pattern
    // `geo.Point(x, y)` must type-check (symmetric with qualified construction `geo.Point(3, 4)`).
    files_ok(&[
        ("geo.chz", "struct Point:\n    x: int\n    y: int\n"),
        (
            "main.chz",
            "import geo\nfn f(p: geo.Point) -> int:\n    match p:\n        geo.Point(x, y): return x + y\n",
        ),
    ]);
}

/// UNSOUND case: `import v from vmod` (a value) then `import v from fmod` (a fn) — checker resolved
/// `v` to the value, runtime binds the function → `v + 1` faulted at runtime. Must be a check error.
#[test]
fn import_value_then_fn_same_name_rejected() {
    files_reject(
        &[
            ("vmod.chz", "v := 42\n"),
            ("fmod.chz", "fn v() -> int:\n    return 7\n"),
            (
                "main.chz",
                "import v from vmod\nimport v from fmod\nfn main():\n    print(v + 1)\n",
            ),
        ],
        "is already imported",
    );
}

/// WRONG/SILENT case: two `fn f` from different modules, last-wins, no error. Must be a check error.
#[test]
fn duplicate_from_import_fn_rejected() {
    files_reject(
        &[
            ("lib.chz", "fn f() -> int:\n    return 1\n"),
            ("lib2.chz", "fn f() -> int:\n    return 99\n"),
            (
                "main.chz",
                "import f from lib\nimport f from lib2\nprint(f())\n",
            ),
        ],
        "is already imported",
    );
}

/// Two structs of the same name from different modules. Must be a check error.
#[test]
fn duplicate_from_import_struct_rejected() {
    files_reject(
        &[
            ("a.chz", "struct P:\n    x: int\n"),
            ("b.chz", "struct P:\n    y: int\n"),
            ("main.chz", "import P from a\nimport P from b\n"),
        ],
        "is already imported",
    );
}

/// Distinct from-imports must still pass.
#[test]
fn distinct_from_imports_ok() {
    files_ok(&[
        ("lib.chz", "fn f() -> int:\n    return 1\n"),
        ("lib2.chz", "fn g() -> int:\n    return 2\n"),
        (
            "main.chz",
            "import f from lib\nimport g from lib2\nprint(f() + g())\n",
        ),
    ]);
}

/// Two whole-module imports with distinct bind names (one aliased) must still pass.
#[test]
fn import_module_as_alias_ok() {
    files_ok(&[
        ("a.chz", "fn f() -> int:\n    return 1\n"),
        ("b.chz", "fn f() -> int:\n    return 2\n"),
        (
            "main.chz",
            "import a\nimport b as c\nprint(a.f() + c.f())\n",
        ),
    ]);
}

/// `import v as w` + `import f as w` (both alias to `w`) must collide on the bind name.
#[test]
fn import_alias_collision_rejected() {
    files_reject(
        &[
            ("vmod.chz", "v := 42\n"),
            ("fmod.chz", "fn f() -> int:\n    return 7\n"),
            (
                "main.chz",
                "import v as w from vmod\nimport f as w from fmod\n",
            ),
        ],
        "is already imported",
    );
}

/// A tuple pattern binding the same name twice must be a compile error.
#[test]
fn duplicate_tuple_pattern_binder_rejected() {
    rejects(
        "fn f(t: (int, int)) -> int:\n    match t:\n        (x, x): return x\n        _: return -1\n",
        "is bound more than once",
    );
}

/// An enum-variant payload binding the same name twice must be a compile error.
#[test]
fn duplicate_enum_payload_binder_rejected() {
    rejects(
        "enum E:\n    V(int, int)\nfn f(e: E) -> int:\n    match e:\n        E.V(a, a): return a\n        _: return -1\n",
        "is bound more than once",
    );
}

/// A nested duplicate binder (tuple inside a variant) must be caught too.
#[test]
fn duplicate_nested_pattern_binder_rejected() {
    rejects(
        "enum E:\n    V((int, int))\nfn f(e: E) -> int:\n    match e:\n        E.V((a, a)): return a\n        _: return -1\n",
        "is bound more than once",
    );
}

/// A binder shared between an OUTER slot and a nested OR-pattern is a single-pattern duplicate
/// (the or binds `x` and the outer tuple slot binds `x` → `x` twice on a matching path).
#[test]
fn duplicate_binder_across_outer_and_or_rejected() {
    rejects(
        "enum E:\n    A(int)\n    B(int)\nfn f(t: (int, E)) -> int:\n    match t:\n        (x, E.A(x) | E.B(x)): return 100\n        _: return -1\n",
        "is bound more than once",
    );
}

/// A duplicate INSIDE one or-alternative must still be caught.
#[test]
fn duplicate_binder_within_or_alternative_rejected() {
    rejects(
        "enum E:\n    V(int, int)\n    W(int)\nfn f(e: E) -> int:\n    match e:\n        E.V(a, a) | E.W(a): return a\n        _: return -1\n",
        "is bound more than once",
    );
}

/// Cross-alternative reuse of the SAME name (`A(x) | B(x)`) is NOT a duplicate — each alternative
/// is its own binding context (guards the or-fix against over-rejection).
#[test]
fn or_alternatives_reuse_binder_ok() {
    ok(
        "enum E:\n    A(int)\n    B(int)\nfn f(e: E) -> int:\n    match e:\n        E.A(x) | E.B(x): return x\n        _: return -1\n",
    );
}

/// `_` repeated in a pattern binds nothing → must still be OK.
#[test]
fn wildcard_repeated_in_pattern_ok() {
    ok("fn f(t: (int, int)) -> int:\n    match t:\n        (_, _): return 0\n");
}

/// The same binder name reused across SEPARATE arms is fine (distinct scopes).
#[test]
fn same_binder_across_arms_ok() {
    ok("fn f(t: int) -> int:\n    match t:\n        0: return 0\n        x: return x\n");
}

/// An or-pattern binding the same names across DIFFERENT alternatives is legal.
#[test]
fn or_pattern_same_names_across_alts_ok() {
    ok(
        "enum E:\n    A(int)\n    B(int)\nfn f(e: E) -> int:\n    match e:\n        E.A(x) | E.B(x): return x\n",
    );
}

// ===== static (associated) methods: the "no self ⇒ static" rule =====

/// BASELINE INVARIANT: a static method (no `self`) called on an INSTANCE must error — calling a
/// static method on a value remains illegal. It must point at the `Type.method(...)` call form.
#[test]
fn static_method_instance_call_still_errors() {
    rejects(
        "struct Rect:\n    w: int\n    h: int\n    fn square(s: int) -> Rect:\n        return Rect(s, s)\nfn main():\n    r := Rect(1, 2)\n    _ := r.square(5)\n",
        "is a static method",
    );
}

/// A ZERO-PARAM method is a STATIC method (no `self`); calling it on an INSTANCE errors — it must
/// be called `Type.method()`. The instance-call remains illegal (the receiver-error sites stay).
#[test]
fn zero_param_method_instance_call_errors() {
    rejects(
        "struct Rect:\n    w: int\n    h: int\n    fn blank() -> Rect:\n        return Rect(0, 0)\nfn main():\n    r := Rect(1, 2)\n    _ := r.blank()\n",
        "is a static method",
    );
}

/// A struct static method called `Type.method(args)` type-checks to the method's return type.
#[test]
fn struct_static_method_call_typechecks() {
    ok(
        "struct Rect:\n    w: int\n    h: int\n    fn square(s: int) -> Rect:\n        return Rect(s, s)\nfn main():\n    r := Rect.square(5)\n    print(r.w)\n",
    );
}

/// An enum static method called `Enum.method(args)` type-checks (returns Option[Enum] here).
#[test]
fn enum_static_method_call_typechecks() {
    ok(
        "enum Color:\n    Red\n    Green\n    fn from_str(s: str) -> Option[Color]:\n        if s == \"red\":\n            return Some(Color.Red)\n        return None\nfn main():\n    c := Color.from_str(\"red\")\n    print(c == Some(Color.Red))\n",
    );
}

/// A variant name always wins over a static method name: `Color.Red` stays the variant.
#[test]
fn enum_variant_wins_over_static() {
    ok(
        "enum Color:\n    Red\n    Green\n    fn from_str(s: str) -> Option[Color]:\n        return None\nfn main():\n    c := Color.Red\n    print(c == Color.Red)\n",
    );
}

/// A static method whose name collides with a variant is a decl-time error (disjointness).
#[test]
fn enum_variant_static_collision_errors() {
    rejects(
        "enum E:\n    Red\n    fn Red(x: int) -> E:\n        return E.Red\nfn main():\n    print(1)\n",
        "is already a variant of enum",
    );
}

/// PART 2: a static method declaring its OWN `[U]` type params is now ALLOWED; `U` is inferred from
/// the argument like a generic free fn. `Box[int].make(5)` (make[U](x:U)->Box[U]) checks to Box[int]
/// (the method param wins the value's type), and a non-generic `Plain.make(5)` infers `U` too.
#[test]
fn static_own_type_params_inferred_ok() {
    ok(
        "struct Box[T]:\n    val: T\n    fn make[U](x: U) -> Box[U]:\n        return Box(x)\nfn main():\n    b := Box[int].make(5)\n    n: int = b.val\n    print(n)\n",
    );
    ok(
        "struct Plain:\n    val: int\n    fn make[U](x: U) -> U:\n        return x\nfn main():\n    n: int = Plain.make(5)\n    print(n)\n",
    );
}

/// PART 2 (no-leak): a static method param that cannot be bound by any argument or turbofish degrades
/// to `Ty::Unknown`, never a free `Ty::Param`. `Box[int].make()` for `make[U]()->List[U]` (U unbound)
/// must type-check and push refine cleanly (mirrors `generic_static_no_turbofish_degrades_param`).
#[test]
fn static_own_type_params_no_leak_unknown() {
    ok(
        "struct Box[T]:\n    val: T\n    fn make[U]() -> List[U]:\n        return []\nfn main():\n    xs := Box[int].make()\n    xs.push(\"x\")\n    print(xs.len())\n",
    );
}

/// PART 2 (combined turbofish): `Box[int].make[str](\"hi\")` — enclosing-type targs from `Box[int]`
/// AND the method targ from `.make[str]` compose. `make[U](x:U)->Box[U]` ⇒ Box[str].
#[test]
fn combined_type_and_method_turbofish_ok() {
    ok(
        "struct Box[T]:\n    val: T\n    fn make[U](x: U) -> Box[U]:\n        return Box(x)\nfn main():\n    b := Box[int].make[str](\"hi\")\n    s: str = b.val\n    print(s)\n",
    );
}

/// PART 2 (combined mismatch): the explicit method turbofish targ that conflicts with the argument
/// type is an error (`Box[int].make[str](5)` — U=str but the arg is int).
#[test]
fn combined_method_turbofish_mismatch_errors() {
    rejects(
        "struct Box[T]:\n    val: T\n    fn make[U](x: U) -> Box[U]:\n        return Box(x)\nfn main():\n    _ := Box[int].make[str](5)\n",
        "expected str",
    );
}

/// PART 2 (combined variant guard): a method-level turbofish on a generic VARIANT ctor is an error —
/// `Box[int].Has[str](5)` (a variant takes no method type args). Under the broadened parser steal this
/// arrives as a Field callee at the `type_apply_head` branch; without the explicit guard the `targs`
/// would be silently dropped (the old Index-over-Field block that errored is now bypassed).
#[test]
fn combined_variant_method_turbofish_errors() {
    rejects(
        "enum Box[T]:\n    Has(T)\n    Empty\nfn main():\n    _ := Box[int].Has[str](5)\nmain()\n",
        "takes no method type arguments",
    );
}

/// PART 2: a static method param that SHADOWS the enclosing struct's type param name is rejected
/// (the existing `fn_sig` guard fires for static methods now that they're allowed).
#[test]
fn static_method_param_shadows_enclosing_rejected() {
    rejects(
        "struct Box[T]:\n    val: T\n    fn make[T](x: T) -> Box[T]:\n        return Box(x)\nfn main():\n    print(1)\n",
        "shadows the struct's type parameter",
    );
}

/// A generic static factory called WITHOUT a type-level turbofish AND without an annotation leaves the
/// ENCLOSING type's param un-inferred — this must be REJECTED, not silently degraded to `Ty::Unknown`
/// (which swallowed any later argument and defeated homogeneity checking). `Box.empty() -> Box[T]` (no
/// turbofish, no arg binding `T`, no hint) leaks `T` as a `Ty::Param`; the first mismatching mutating use
/// (`b.items.push("x")`, element slot `Ty::Param(T)` out of scope) fires the construction-site diagnostic.
/// Mirrors the already-sound generic FREE-FUNCTION path (`mkbox()` used heterogeneously rejects the same
/// way). Method-OWN `[U]` params still degrade to `Ty::Unknown` (see `static_own_type_params_no_leak_unknown`).
#[test]
fn generic_static_no_turbofish_rejects_uninferred_param() {
    rejects(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box([])\nfn main():\n    b := Box.empty()\n    b.items.push(\"x\")\n    print(b.items.len())\n",
        "un-inferred type parameter",
    );
}

/// A `let` annotation pins the ENCLOSING type param of an un-turbofished static factory from the
/// expected type (`b: Box[int] = Box.empty()` seeds `T=int`), then homogeneous use is accepted.
#[test]
fn annotation_pins_static_factory_ok() {
    ok(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box([])\n    fn add(self, x: T):\n        self.items.push(x)\n    fn first(self) -> T:\n        return self.items[0]\nfn main():\n    b: Box[int] = Box.empty()\n    b.add(5)\n    print(b.first())\n",
    );
}

/// Same annotation-pinned factory: once `T=int` is fixed from the annotation, a heterogeneous add is
/// rejected (`b.add(\"hello\")` — expected int).
#[test]
fn annotation_pins_static_factory_then_rejects_heterogeneous() {
    rejects(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box([])\n    fn add(self, x: T):\n        self.items.push(x)\n    fn first(self) -> T:\n        return self.items[0]\nfn main():\n    b: Box[int] = Box.empty()\n    b.add(\"hello\")\n",
        "expected int",
    );
}

/// SAME HOLE in generic ENUMS: an un-turbofished, un-annotated enum static factory (`Wrap.none()` for
/// `fn none() -> Wrap[T]`) leaks the enclosing `T`; a heterogeneous instance-method use (`w.put(\"s\")`
/// where `fn put(self, x: T)`) is then rejected. The struct fix (shared degrade loop + `seed_from_hint`)
/// covers enums with no extra code.
#[test]
fn generic_enum_static_factory_rejects_heterogeneous() {
    rejects(
        "enum Wrap[T]:\n    Has(T)\n    Empty\n    fn none() -> Wrap[T]:\n        return Wrap.Empty\n    fn put(self, x: T):\n        print(x)\nfn main():\n    w := Wrap.none()\n    w.put(\"s\")\nmain()\n",
        "expected T",
    );
}

/// GRAPH-PATH regression (REPRO A, the full check-ok → runtime-trap program): a static factory called
/// with no pin, a heterogeneous add, then arithmetic on the read-back value. Proves the fix holds
/// through `build_graph` → `check_graph` (the real CLI path), not only single-module `check_src`.
#[test]
fn graph_static_factory_uninferred_rejects_repro_a() {
    entry_rejects(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box([])\n    fn add(self, x: T):\n        self.items.push(x)\n    fn first(self) -> T:\n        return self.items[0]\nfn main():\n    b := Box.empty()\n    b.add(\"hello\")\n    x := b.first()\n    print(x + 1)\nmain()\n",
        "found str",
    );
}

/// GRAPH-PATH regression (enum variant of REPRO A): the enum static-factory hole rejects through the
/// real graph path too.
#[test]
fn graph_enum_static_factory_uninferred_rejects() {
    entry_rejects(
        "enum Wrap[T]:\n    Has(T)\n    Empty\n    fn none() -> Wrap[T]:\n        return Wrap.Empty\n    fn put(self, x: T):\n        print(x)\nfn main():\n    w := Wrap.none()\n    w.put(1)\n    w.put(\"s\")\nmain()\n",
        "expected T",
    );
}

/// OVER-REJECTION guard: a static factory whose RETURN omits the enclosing param (`fn count() -> int`)
/// leaves no free param — `subst` is a no-op, so it must NOT falsely reject. Confirms the degrade-drop
/// was scoped to params that actually appear in the return.
#[test]
fn static_factory_return_omits_param_still_ok() {
    ok(
        "struct Box[T]:\n    items: List[T]\n    fn count() -> int:\n        return 0\nfn main():\n    n := Box.count()\n    print(n)\n",
    );
}

/// A generic static method INFERS the enclosing type's params from its arguments, like the ctor:
/// `Box.wrap(5)` for `fn wrap(x: T) -> Box[T]` yields `Box[int]` with no turbofish.
#[test]
fn generic_static_infers_type_param_from_arg() {
    ok(
        "struct Box[T]:\n    items: List[T]\n    fn wrap(x: T) -> Box[T]:\n        return Box([x])\nfn main():\n    b := Box.wrap(5)\n    n: int = b.items[0]\n    print(n)\n",
    );
}

/// A generic static via the TYPE-level turbofish `Box[int].empty()` type-checks (v1's supported form).
#[test]
fn generic_static_turbofish_box_int_empty_typechecks() {
    ok(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box([])\n    fn len(self) -> int:\n        return self.items.len()\nfn main():\n    b := Box[int].empty()\n    print(b.len())\n",
    );
}

/// PART 2: a method-level turbofish on a static method that declares NO `[U]` is an arity error —
/// `Box.empty[int]()` for `fn empty()->Box[T]` (no own type params) takes no method type arguments.
#[test]
fn method_level_turbofish_on_non_generic_static_errors() {
    rejects(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box([])\nfn main():\n    _ := Box.empty[int]()\n",
        "type argument",
    );
}

/// PART 2 (instance multi-turbofish): an instance method declaring its OWN `[A, B]` takes a multi
/// type-arg + multi value-arg turbofish `s.m[int, str](1, "x")`.
#[test]
fn instance_method_multi_turbofish_ok() {
    ok(
        "struct S:\n    n: int\n    fn m[A, B](self, x: A, y: B) -> A:\n        return x\nfn main():\n    s := S(0)\n    r: int = s.m[int, str](1, \"x\")\n    print(r)\n",
    );
}

/// PART 2 (instance turbofish mismatch): the explicit instance-method turbofish targ that conflicts
/// with the arg is an error (previously the targs were DROPPED, so this was silently accepted).
#[test]
fn instance_method_turbofish_mismatch_errors() {
    rejects(
        "struct S:\n    n: int\n    fn m[A](self, x: A) -> A:\n        return x\nfn main():\n    s := S(0)\n    _ := s.m[str](5)\n",
        "expected str",
    );
}

/// PART 2 (soundness): a method type param in RECEIVER position (`fn m[U](self: U)`) turbofished to a
/// type that contradicts the actual receiver must ERROR — without the post-subst receiver check the
/// turbofish `b.idret[str]()` on a `Box[int]` certified `str` for a struct value (a soundness hole).
#[test]
fn method_param_in_receiver_turbofish_contradiction_errors() {
    rejects(
        "struct Box[T]:\n    val: T\n    fn idret[U](self: U) -> U:\n        return self\nfn main():\n    b := Box(5)\n    x: str = b.idret[str]()\n    print(x)\n",
        "receiver",
    );
}

/// PART 2 (.iter swallow fix): a method-level turbofish on a BUILTIN/non-generic member must error
/// like `xs.len[int]()` — including the `.iter` fast-path (`xs.iter[int]()` was silently accepted).
#[test]
fn iter_method_turbofish_errors() {
    rejects(
        "fn main():\n    xs := [1, 2, 3]\n    _ := xs.iter[int]()\n",
        "type argument",
    );
}

/// AUTHORIZED REGRESSION (broadened parser steal): the new UNIFORM rule is `recv.name[X](args)`
/// parses as a method turbofish on ANY receiver. So a VARIABLE-index fn-field index-then-call on a
/// non-bare receiver — `arr[i].handlers[k](10)` — now parses as a method turbofish (`k` reads as a
/// type name) and errors. This is intentionally uniform with the bare-ident case `w.handlers[k](10)`,
/// which ALREADY required parens. Workaround: parens, `(arr[i].handlers[k])(10)` (see the `_ok` test).
#[test]
fn var_index_then_call_on_indexed_receiver_now_turbofish_errors() {
    rejects(
        "struct Cell:\n    handlers: List[fn(int) -> int]\nfn main():\n    arr := [Cell([fn(x: int) -> int: x + 1])]\n    i := 0\n    k := 0\n    print(arr[i].handlers[k](10))\n",
        "type argument",
    );
}

/// The documented workaround for the authorized regression: parenthesize the index-then-call —
/// `(arr[i].handlers[k])(10)` — so the `(` no longer immediately follows the `]` and the steal does
/// not fire. Must type-check (the fn-valued field is indexed then the value is called).
#[test]
fn parenthesized_var_index_then_call_ok() {
    ok(
        "struct Cell:\n    handlers: List[fn(int) -> int]\nfn main():\n    arr := [Cell([fn(x: int) -> int: x + 1])]\n    i := 0\n    k := 0\n    print((arr[i].handlers[k])(10))\n",
    );
}

/// A NUMERIC index-then-call on an indexed receiver stays ordinary index-then-call: `0` is not a
/// type, so `try_parse_type_arg_call` backtracks and `arr[0].handlers[0](20)` keeps its meaning.
#[test]
fn numeric_index_then_call_on_indexed_receiver_stays() {
    ok(
        "struct Cell:\n    handlers: List[fn(int) -> int]\nfn main():\n    arr := [Cell([fn(x: int) -> int: x + 1])]\n    print(arr[0].handlers[0](20))\n",
    );
}

/// BOUNDARY: a plain subscript with NO following call stays an Index on a non-bare receiver — neither
/// `obj.items[0]` (numeric) nor `m.data[k]` (variable) is followed by `(`, so the steal never fires.
#[test]
fn subscript_without_call_on_non_bare_receiver_stays_index() {
    ok(
        "struct S:\n    items: List[int]\nfn main():\n    s := S([1, 2, 3])\n    print(s.items[0])\n",
    );
    ok(
        "struct M:\n    data: Map[str, int]\nfn main():\n    m := M({\"a\": 1})\n    k := \"a\"\n    print(m.data[k])\n",
    );
}

// ===== member-level turbofish on a NON-BARE receiver (broadened parser steal) =====
// `recv.name[X](args)` now parses as a method turbofish on ANY receiver (not just a bare ident),
// so a call-result / factory-result / field / index receiver can all take a member turbofish.

/// Call-result receiver: `W(1).cast[str]("a")` — the receiver is a struct ctor CALL, not a bare
/// ident. Previously mis-parsed as `Index{Field, str}` then a value-call ⇒ "unknown name 'str'".
#[test]
fn member_turbofish_on_call_result_receiver_ok() {
    ok(
        "struct W[T]:\n    v: T\n    fn cast[U](self, x: U) -> W[U]:\n        return W(x)\nfn main():\n    a := W(1).cast[str](\"a\")\n    s: str = a.v\n    print(s)\nmain()\n",
    );
}

/// Factory-result receiver: `mk().cast[str]("a")` — receiver is a free-fn call.
#[test]
fn member_turbofish_on_factory_result_receiver_ok() {
    ok(
        "struct W[T]:\n    v: T\n    fn cast[U](self, x: U) -> W[U]:\n        return W(x)\nfn mk() -> W[int]:\n    return W(1)\nfn main():\n    a := mk().cast[str](\"a\")\n    s: str = a.v\n    print(s)\nmain()\n",
    );
}

/// Field receiver: `h.w.cast[str]("a")` — receiver is a (deeper) field access, NOT a bare ident.
#[test]
fn member_turbofish_on_field_receiver_ok() {
    ok(
        "struct W[T]:\n    v: T\n    fn cast[U](self, x: U) -> W[U]:\n        return W(x)\nstruct H:\n    w: W[int]\nfn main():\n    h := H(W(1))\n    a := h.w.cast[str](\"a\")\n    s: str = a.v\n    print(s)\nmain()\n",
    );
}

/// Index receiver: `xs[0].cast[str]("a")` — receiver is an index expression.
#[test]
fn member_turbofish_on_index_receiver_ok() {
    ok(
        "struct W[T]:\n    v: T\n    fn cast[U](self, x: U) -> W[U]:\n        return W(x)\nfn main():\n    xs := [W(1)]\n    a := xs[0].cast[str](\"a\")\n    s: str = a.v\n    print(s)\nmain()\n",
    );
}

/// Chained: `W(1).cast("a").cast[bool](true)` — a member turbofish on the result of a prior method
/// call (the receiver is itself a method-call expression).
#[test]
fn member_turbofish_chained_on_method_result_ok() {
    ok(
        "struct W[T]:\n    v: T\n    fn cast[U](self, x: U) -> W[U]:\n        return W(x)\nfn main():\n    a := W(1).cast(\"a\").cast[bool](true)\n    b: bool = a.v\n    print(b)\nmain()\n",
    );
}

/// Multi-type-arg member turbofish on a NON-BARE receiver: `mk().pair[int, str](1, "x")`. The comma
/// in the type list means this can NEVER ride the old single-arg Index-reinterpret path — it REQUIRES
/// the broadened parser steal.
#[test]
fn member_multi_turbofish_on_non_bare_receiver_ok() {
    ok(
        "struct W[T]:\n    v: T\n    fn pair[A, B](self, x: A, y: B) -> A:\n        return x\nfn mk() -> W[int]:\n    return W(1)\nfn main():\n    r: int = mk().pair[int, str](1, \"x\")\n    print(r)\nmain()\n",
    );
}

/// Nested-generic member turbofish on a NON-BARE receiver: `W(1).cast[Map[str, int]](m)`. The inner
/// `[..]` of the type arg likewise cannot ride the Index-reinterpret path — REQUIRES the broaden.
#[test]
fn member_nested_generic_turbofish_on_non_bare_receiver_ok() {
    ok(
        "struct W[T]:\n    v: T\n    fn cast[U](self, x: U) -> W[U]:\n        return W(x)\nfn main():\n    m: Map[str, int] = {\"a\": 1}\n    a := W(1).cast[Map[str, int]](m)\n    n: int = a.v[\"a\"]\n    print(n)\nmain()\n",
    );
}

// ===== type-side declaration-site turbofish (PART 1): `Type[T].Variant` / `Type[T].static()` =====

/// `Box[int].Has(5)` — a generic enum VARIANT constructor via the type-level turbofish (single arg,
/// rides the `Index` reinterpretation path). Resolves to `Box[int]`.
#[test]
fn turbofish_type_variant_single_arg_ok() {
    ok(
        "enum Box[T]:\n    Has(T)\n    Empty\nfn main():\n    b: Box[int] = Box[int].Has(5)\n    print(1)\nmain()\n",
    );
}

/// The resolved variant carries the EXPLICIT type args — `Box[int].Has(5)` is `Box[int]`, so binding
/// it to `Box[str]` must error (proving the targs are seeded, not left `Unknown`).
#[test]
fn turbofish_type_variant_targs_seeded_not_unknown() {
    rejects(
        "enum Box[T]:\n    Has(T)\n    Empty\nfn main():\n    b: Box[str] = Box[int].Has(5)\n    print(1)\nmain()\n",
        "Box[int]",
    );
}

/// `E[int, str].Pair(1, \"x\")` — MULTI type-arg turbofish (the `TypeApply` carrier path) on a
/// 2-param enum resolves to `E[int, str]`.
#[test]
fn turbofish_type_variant_multi_arg_ok() {
    ok(
        "enum E[T, U]:\n    Pair(T, U)\nfn main():\n    p: E[int, str] = E[int, str].Pair(1, \"x\")\n    print(1)\nmain()\n",
    );
}

/// `Box[int, str].Has(5)` — too many type args for a single-param enum errors via the existing
/// arity check (`seed_targs`).
#[test]
fn turbofish_type_variant_arity_mismatch_rejected() {
    rejects(
        "enum Box[T]:\n    Has(T)\nfn main():\n    _ := Box[int, str].Has(5)\nmain()\n",
        "type argument",
    );
}

/// Nullary value form `Box[int].Empty` resolves to `Box[int]` (the explicit args seeded), NOT
/// `Box[Unknown]` — binding to `Box[str]` must error.
#[test]
fn turbofish_type_nullary_variant_seeded() {
    rejects(
        "enum Box[T]:\n    Has(T)\n    Empty\nfn main():\n    b: Box[str] = Box[int].Empty\n    print(1)\nmain()\n",
        "Box[int]",
    );
    ok(
        "enum Box[T]:\n    Has(T)\n    Empty\nfn main():\n    b: Box[int] = Box[int].Empty\n    print(1)\nmain()\n",
    );
}

/// The OLD gliding form `Box.Full[int](9)` (type args on the VARIANT) is REMOVED — it must error
/// with a redirect to the type-side form.
#[test]
fn old_gliding_variant_turbofish_redirects() {
    rejects(
        "enum Box[T]:\n    Full(T)\n    Empty\nfn main():\n    _ := Box.Full[int](9)\nmain()\n",
        "put the type arguments on the type: Box[int].Full(",
    );
}

/// Regression: a generic STATIC method via the type-level turbofish still resolves (variant-first
/// falls through to the static path when no variant matches the member name).
#[test]
fn turbofish_type_static_method_regression() {
    ok(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box([])\n    fn len(self) -> int:\n        return self.items.len()\nfn main():\n    b := Box[int].empty()\n    print(b.len())\nmain()\n",
    );
}

/// Task 5 (HARD rename): the builtin container TYPE names are now PascalCase `List`/`Map`/`Set`.
/// These resolve as types in annotations, nested forms, fn params/returns, and struct fields.
#[test]
fn pascal_containers_resolve() {
    ok(
        "fn main():\n    x: List[int] = [1]\n    m: Map[str, int] = {\"a\": 1}\n    s: Set[int] = {1}\n    nested: List[Map[str, Set[int]]] = []\n    print(x.len())\n    print(m.len())\n    print(s.len())\n    print(nested.len())\nmain()\n",
    );
}

/// The lowercase `list`/`map`/`set` are GONE as type names (hard rename, no alias): they fall to the
/// unknown-type branch.
#[test]
fn lowercase_containers_rejected() {
    rejects(
        "fn main():\n    x: list[int] = [1]\n    print(x.len())\nmain()\n",
        "unknown",
    );
    rejects(
        "fn main():\n    m: map[str, int] = {}\n    print(m.len())\nmain()\n",
        "unknown",
    );
    rejects(
        "fn main():\n    s: set[int] = {}\n    print(s.len())\nmain()\n",
        "unknown",
    );
}

/// The PascalCase constructors `List(it)`/`Set(it)`/`Map(it)` + empty `Set()` type-check.
#[test]
fn pascal_ctor_calls() {
    ok(
        "fn main():\n    a := List([1, 2])\n    b := Set([1, 2, 3])\n    c := Map([(\"a\", 1)])\n    d: Set[int] = Set()\n    print(a.len())\n    print(b.len())\n    print(c.len())\n    print(d.len())\nmain()\n",
    );
}

// ===== M22: operator protocols Div/Mod/Neg + protocol embedding + Arithmetic =====

/// A struct defining div/mod/neg overloads `/`, `%`, and unary `-`.
#[test]
fn div_mod_neg_struct_overload_typechecks() {
    ok(
        "struct V:\n    n: int\n    fn div(self, o: V) -> V:\n        return V(self.n / o.n)\n    fn mod(self, o: V) -> V:\n        return V(self.n % o.n)\n    fn neg(self) -> V:\n        return V(-self.n)\nfn main():\n    a := V(7)\n    b := V(2)\n    print((a / b).n)\n    print((a % b).n)\n    print((-a).n)\nmain()\n",
    );
}

/// Error must be a reserved protocol name (it's prebuilt) — previously omitted.
#[test]
fn error_is_reserved_protocol() {
    rejects("protocol Error:\n    fn message(self) -> str\n", "reserved");
}

/// A user cannot REDECLARE the reserved `Any` protocol — even with the new empty-body `pass` form.
/// The prelude mirror is exempt (validate-and-no-op stdlib hoist); a user module's is rejected.
#[test]
fn user_redeclare_of_any_rejected() {
    rejects("protocol Any:\n    pass\n", "reserved");
}

/// GENERALIZATION GUARD: a USER empty protocol (`protocol Foo:\n    pass`) is an accept-all top type
/// byte-identical to the reserved `Any` — accepts scalars + structs as a param and types a
/// heterogeneous list. The accept-all behaviour is NOT special-cased on the literal name "Any".
#[test]
fn empty_protocol_accepts_all_like_any() {
    ok("protocol Foo:\n    pass\n\
        struct P:\n    x: int\n\
        fn takes_foo(v: Foo) -> int:\n    return 1\n\
        fn takes_any(v: Any) -> int:\n    return 1\n\
        fn main():\n\
        \x20   takes_foo(1)\n\
        \x20   takes_foo(\"a\")\n\
        \x20   takes_foo(true)\n\
        \x20   takes_foo(P(1))\n\
        \x20   takes_any(1)\n\
        \x20   xs: List[Foo] = [1, \"a\", true]\n\
        \x20   ys: List[Any] = [1, \"a\", true]\n\
        \x20   print(xs.len() + ys.len())\n\
        main()\n");
}

/// A zero-field struct (`struct S:\n    pass`) is intrinsically `Hashable`: usable as a Set element
/// and a Map key with no explicit `hash(self)` method.
#[test]
fn empty_struct_is_hashable() {
    ok("struct S:\n    pass\n\
        fn main():\n\
        \x20   seen: Set[S] = {S(), S()}\n\
        \x20   m: Map[S, int] = {S(): 1}\n\
        \x20   print(seen.len() + m.len())\n\
        main()\n");
}

/// A zero-field struct that DEFINES a `hash` method must fall through to the STRUCTURAL Hashable
/// check — the zero-field intrinsic only fires when there is NO `hash` method (mirrors the runtime
/// `struct_hash` guard `fields.is_empty() && !methods.contains_key("hash")`). A mis-typed `hash`
/// (wrong return type) must be REJECTED at check time, not accepted then faulted at runtime
/// (`hash() must return int, got str`). Regression for the check-ok/run-diverge soundness hole.
#[test]
fn empty_struct_with_mistyped_hash_rejected() {
    rejects(
        "struct S:\n    fn hash(self) -> str:\n        return \"x\"\n\
        fn main():\n\
        \x20   m: Map[S, int] = {S(): 1}\n\
        \x20   print(m.len())\n\
        main()\n",
        "Hashable",
    );
    // Arity variant: an extra param makes the `hash` method un-dispatchable by `struct_hash`
    // (runtime `hash() must return int, got nil`); must be a check-time reject too.
    rejects(
        "struct S:\n    fn hash(self, k: int) -> int:\n        return k\n\
        fn main():\n\
        \x20   seen: Set[S] = {S()}\n\
        \x20   print(seen.len())\n\
        main()\n",
        "Hashable",
    );
}

/// A zero-field struct WITH a correctly-typed `hash(self) -> int` method stays Hashable (the
/// structural check passes) — the intrinsic fix must not reject a valid explicit hash.
#[test]
fn empty_struct_with_valid_hash_ok() {
    ok("struct S:\n    fn hash(self) -> int:\n        return 42\n\
        fn main():\n\
        \x20   seen: Set[S] = {S()}\n\
        \x20   print(seen.len())\n\
        main()\n");
}

/// Div/Mod/Neg are reserved protocol names.
#[test]
fn div_mod_neg_are_reserved_protocols() {
    rejects(
        "protocol Div:\n    fn div(self, o: Self) -> Self\n",
        "reserved",
    );
    rejects(
        "protocol Mod:\n    fn mod(self, o: Self) -> Self\n",
        "reserved",
    );
    rejects("protocol Neg:\n    fn neg(self) -> Self\n", "reserved");
    rejects("protocol Arithmetic:\n    Add\n", "reserved");
}

/// `[T: Div]` / `[T: Mod]` / `[T: Neg]` generic bounds flow, with int instantiation.
#[test]
fn div_mod_neg_bound_flows() {
    ok(
        "fn d[T: Div](a: T, b: T) -> T:\n    return a / b\nfn main():\n    print(d(7, 2))\nmain()\n",
    );
    ok(
        "fn m[T: Mod](a: T, b: T) -> T:\n    return a % b\nfn main():\n    print(m(7, 2))\nmain()\n",
    );
    ok("fn n[T: Neg](a: T) -> T:\n    return -a\nfn main():\n    print(n(5))\nmain()\n");
}

/// `[T: Arithmetic]` accepts a struct with all four ops and uses them in the body; rejects a struct
/// missing div (error mentions div).
#[test]
fn arithmetic_bundle_accepts_and_rejects() {
    let prelude = "struct V:\n    n: int\n    fn add(self, o: V) -> V:\n        return V(self.n + o.n)\n    fn sub(self, o: V) -> V:\n        return V(self.n - o.n)\n    fn mul(self, o: V) -> V:\n        return V(self.n * o.n)\n    fn div(self, o: V) -> V:\n        return V(self.n / o.n)\n";
    ok(&format!(
        "{prelude}fn calc[T: Arithmetic](a: T, b: T) -> T:\n    return a + b - a * b\nfn main():\n    print(calc(V(6), V(2)).n)\nmain()\n"
    ));
    // A struct lacking div fails an Arithmetic bound, mentioning div.
    let no_div = "struct W:\n    n: int\n    fn add(self, o: W) -> W:\n        return W(self.n + o.n)\n    fn sub(self, o: W) -> W:\n        return W(self.n - o.n)\n    fn mul(self, o: W) -> W:\n        return W(self.n * o.n)\nfn calc[T: Arithmetic](a: T, b: T) -> T:\n    return a + b\nfn main():\n    print(calc(W(1), W(2)).n)\nmain()\n";
    rejects(no_div, "div");
}

/// Inside a `[T: Arithmetic]` body, `+ - * /` all type-check (transitive bound flattening).
#[test]
fn arithmetic_body_uses_ops() {
    ok(
        "fn f[T: Arithmetic](a: T, b: T) -> T:\n    return ((a + b) - (a * b)) / a\nfn main():\n    print(f(8, 2))\nmain()\n",
    );
}

/// A user protocol embedding Arithmetic plus its own methods (transitive embedding).
#[test]
fn user_protocol_embeds_arithmetic_and_own_methods() {
    ok(
        "protocol Field:\n    Arithmetic\n    fn zero(self) -> Self\nstruct V:\n    n: int\n    fn add(self, o: V) -> V:\n        return V(self.n + o.n)\n    fn sub(self, o: V) -> V:\n        return V(self.n - o.n)\n    fn mul(self, o: V) -> V:\n        return V(self.n * o.n)\n    fn div(self, o: V) -> V:\n        return V(self.n / o.n)\n    fn zero(self) -> V:\n        return V(0)\nfn g[T: Field](a: T, b: T) -> T:\n    return a / b\nfn main():\n    print(g(V(9), V(3)).n)\nmain()\n",
    );
}

/// Diamond dedup: embedding Arithmetic AND Add (both supply the same `add` sig) is legal.
#[test]
fn embed_diamond_dedup_ok() {
    ok("protocol P:\n    Arithmetic + Add\n");
}

/// An own `fn` colliding with an embedded-required method name is a declare-time error.
#[test]
fn own_fn_vs_embed_collision_errors() {
    rejects(
        "protocol P:\n    Add\n    fn add(self, o: Self) -> Self\n",
        "conflicts with embedded",
    );
}

/// Two embeds pulling the same method name with differing signatures is an error.
#[test]
fn embed_sig_conflict_errors() {
    rejects(
        "protocol P1:\n    fn m(self) -> int\nprotocol P2:\n    fn m(self, o: Self) -> int\nprotocol Q:\n    P1 + P2\n",
        "conflicting signature",
    );
}

/// Cyclic embedding (A embeds B, B embeds A) is rejected at declare time.
#[test]
fn embed_cycle_errors() {
    rejects("protocol A:\n    B\nprotocol B:\n    A\n", "cyclic");
}

// ===== M22 soundness: a newtype operator METHOD is never dispatched at runtime (the same-newtype
// arm always auto-flows to the underlying's native op), so the checker must NOT type-check an
// operator overload defined on a newtype — doing so would accept a program that crashes at runtime
// on every engine (`check` ok / `run` faults). The ONLY newtype operator support is the numeric
// underlying's auto-flow. =====

/// A newtype defining a `neg` method must NOT make unary `-` type-check (no newtype Neg dispatch on
/// any engine; Neg is out of scope for newtypes). Regression: M22 added a `satisfies(Neg)` path that
/// wrongly admitted newtypes (`check` ok, `run`/`run --serial` → "cannot apply Neg to newtype").
#[test]
fn newtype_neg_method_rejected() {
    rejects(
        "newtype Foo = int:\n    fn neg(self) -> Foo:\n        return Foo(-int(self))\nfn main():\n    print(int(-Foo(5)))\nmain()\n",
        "cannot negate",
    );
}

/// A NON-numeric newtype (`= str`) defining a `div` method must NOT make `/` type-check (the runtime
/// same-newtype arm auto-flows to `str / str`, which faults; the user `div` is never dispatched).
/// Regression: M22's `op_overload_result` admitted it (`check` ok, `run` → "cannot apply Div to str
/// and str").
#[test]
fn newtype_nonnumeric_div_method_rejected() {
    rejects(
        "newtype Name = str:\n    fn div(self, o: Name) -> Name:\n        return self\nfn use(a: Name) -> Name:\n    return a / a\n",
        "cannot apply /",
    );
}

/// Same as above for `mod` (`%`).
#[test]
fn newtype_nonnumeric_mod_method_rejected() {
    rejects(
        "newtype Name = str:\n    fn mod(self, o: Name) -> Name:\n        return self\nfn use(a: Name) -> Name:\n    return a % a\n",
        "cannot apply %",
    );
}

/// A numeric scalar newtype STILL gets `/` and `%` via the underlying's native auto-flow (no method
/// needed) — the fix must not regress the legitimate numeric-newtype operator path.
#[test]
fn numeric_newtype_div_mod_still_ok() {
    ok(
        "newtype Meters = float\nfn main():\n    a := Meters(7.0)\n    b := Meters(2.0)\n    print(float(a / b))\n    print(float(a % b))\nmain()\n",
    );
}

/// A newtype must not satisfy a `[T: Div]` / `[T: Neg]` generic bound via a structural operator
/// method (bound-site soundness: forwarding such a newtype into the generic would crash at runtime).
#[test]
fn newtype_operator_method_fails_generic_bound() {
    rejects(
        "newtype Name = str:\n    fn div(self, o: Name) -> Name:\n        return self\nfn d[T: Div](a: T, b: T) -> T:\n    return a / b\nfn use(x: Name) -> Name:\n    return d(x, x)\n",
        "Div",
    );
}

/// Drift guard (editor hover): EVERY reserved callable builtin must have a `builtin_sig` entry, so a
/// future builtin added to `RESERVED_CALLABLE` can't silently lose hover. The set is exactly the
/// CALLABLE reserved names — the free functions (`print`/`panic`/`range`/`int`/`float`/`str`/`ord`/
/// `chr`) and the container/runtime constructors (`List`/`Set`/`Map`/`bytes`/`bytearray`/`Channel`/
/// `Shared`/`RwShared`/`Atomic`/`timer`/`Executor`); none is a pure type marker, so each one is
/// callable and must have a display signature. (`Ok`/`Err`/`Some` are deliberately NOT reserved — a
/// user may shadow them — so they're out of scope and keep hovering `None` for v1.) `is_reserved_name`
/// is the single source: `builtin_sig` is keyed off the same names.
#[test]
fn reserved_callables_all_have_builtin_sig() {
    // `builtin_sig` is now a Checker METHOD (phase 3a): the nine migrated universe builtins source
    // their sig from the always-linked std/prelude.chz, seeded here via `seed_native_prelude_sigs`
    // (the single-module `check` path does the same). `print` + the container/runtime ctors stay
    // hard-coded, so they resolve without the seed too.
    let mut c = Checker::new();
    c.seed_native_prelude_sigs();
    for name in RESERVED_CALLABLE {
        assert!(
            c.builtin_sig(name).is_some(),
            "reserved callable builtin '{name}' has no builtin_sig entry → it would lose editor hover"
        );
        // And the const slice IS the reserved-name set (refactor stayed behavior-identical).
        assert!(is_reserved_name(name), "'{name}' must be a reserved name");
    }
    // Sanity: a non-reserved name has no builtin sig (no accidental over-coverage).
    assert!(c.builtin_sig("Ok").is_none());
    assert!(c.builtin_sig("totally_not_a_builtin").is_none());
}

/// Native-prelude phase 1 — DRIFT GUARD (the real payoff of this track). The synthetic `PRELUDE`
/// table is the SINGLE SOURCE OF TRUTH for the four first-class universe FUNCTIONS
/// (`print`/`ord`/`chr`/`panic`). This test locks every phase that used to hard-code these names to
/// the table, so a future edit to one phase that silently diverges from the table (the whack-a-mole
/// bug class this track kills) fails here instead of shipping a three-engine skew.
#[test]
fn prelude_table_is_single_source_of_truth() {
    use std::collections::BTreeSet;

    // (1) Phase 2a: the table now holds two families — the four FIRST-CLASS universe pure fns AND the
    //     NON-first-class scalar-conversion CTORS (`Intrinsic::Ctor`). The container/reserved-type
    //     ctors (List/Map/Set/range) are GENERIC / carry reserved-type identity → phase 2b, still OUT.
    let firstclass: BTreeSet<&str> = PRELUDE
        .iter()
        .filter(|p| p.first_class)
        .map(|p| p.name)
        .collect();
    assert_eq!(
        firstclass,
        BTreeSet::from(["print", "ord", "chr", "panic"]),
        "the first-class PRELUDE rows must be exactly the four universe fns"
    );
    // Phase 2b folded the four GENERIC / reserved-type container ctors (List/Map/Set/range) into the
    // table as `Intrinsic::Ctor` rows for DISPATCH single-source; their generic TYPE-IDENTITY still
    // lives in `resolve_type`/`infer_named_call` (not expressible as a flat `FnSig`).
    let ctors: BTreeSet<&str> = PRELUDE
        .iter()
        .filter(|p| p.intrinsic == Intrinsic::Ctor)
        .map(|p| p.name)
        .collect();
    assert_eq!(
        ctors,
        BTreeSet::from([
            "int",
            "float",
            "bool",
            "str",
            "bytes",
            "bytearray",
            "List",
            "Map",
            "Set",
            "range"
        ]),
        "the Ctor PRELUDE rows must be the six scalar-conversion ctors plus the four container ctors"
    );
    // The four container/reserved-type ctors are now IN the table (phase 2b), each a NON-first-class
    // `Intrinsic::Ctor` row (dispatch single-source; generic identity stays in resolve_type).
    for c in ["List", "Map", "Set", "range"] {
        let p = prelude_fn(c).unwrap_or_else(|| {
            panic!("'{c}' must be a PRELUDE row (container ctor folded in, 2b)")
        });
        assert_eq!(
            p.intrinsic,
            Intrinsic::Ctor,
            "'{c}' must be an Intrinsic::Ctor row"
        );
        assert!(
            !p.first_class,
            "container ctor '{c}' must be non-first-class"
        );
    }

    // (2) `is_firstclass_builtin_fn` is the table's `.first_class` view — over the WHOLE reserved
    //     callable universe, so a table row (or its absence) is the one truth for first-classness.
    for name in RESERVED_CALLABLE {
        assert_eq!(
            is_firstclass_builtin_fn(name),
            prelude_fn(name).is_some_and(|p| p.first_class),
            "'{name}': is_firstclass_builtin_fn must equal PRELUDE .first_class"
        );
    }
    // A container ctor name (no first-class row) is not first-class.
    assert!(!is_firstclass_builtin_fn("List"));
    // A scalar-ctor name IS now in the table but is NON-first-class (types are not first-class values).
    assert!(prelude_fn("int").is_some());
    assert!(!is_firstclass_builtin_fn("int"));

    // (3) Every table row has a checker `builtin_sig` (drives editor hover + value-position typing).
    //     `first_class` is true IFF the row lowers to a first-class runtime value — Print|Builtin — and
    //     is NEVER true for a Ctor (the hard invariant: no scalar ctor is first-class). `builtin_sig`
    //     is now a Checker METHOD sourcing the nine migrated sigs from std/prelude.chz (seeded here).
    let mut chk = Checker::new();
    chk.seed_native_prelude_sigs();
    for p in PRELUDE {
        assert!(
            chk.builtin_sig(p.name).is_some(),
            "PRELUDE row '{}' has no builtin_sig",
            p.name
        );
        let expect_fc = matches!(p.intrinsic, Intrinsic::Print | Intrinsic::Builtin);
        assert_eq!(
            p.first_class, expect_fc,
            "'{}': first_class must be true iff intrinsic is Print|Builtin",
            p.name
        );
    }
    // Explicit invariant: NO `Intrinsic::Ctor` row is first-class (guards against a ctor accidentally
    // leaking a `LoadBuiltin`/`Ty::BuiltinFn` and becoming a first-class value).
    for p in PRELUDE {
        if p.intrinsic == Intrinsic::Ctor {
            assert!(
                !p.first_class,
                "Ctor row '{}' must be non-first-class",
                p.name
            );
        }
    }

    // (4) Cross-phase invariant: the `intrinsic` column decides `compiler::is_builtin` membership —
    //     `Builtin`/`Ctor` ⇒ handled by `is_builtin` (CallBuiltin-dispatched); `Print` ⇒ excluded
    //     (its own CallPrint/CallPrintSep opcodes).
    for p in PRELUDE {
        let comp = crate::compiler::is_builtin(p.name);
        match p.intrinsic {
            Intrinsic::Builtin | Intrinsic::Ctor => assert!(
                comp,
                "'{}' is CallBuiltin-dispatched but is_builtin is false",
                p.name
            ),
            Intrinsic::Print => assert!(
                !comp,
                "'{}' is Intrinsic::Print but is_builtin is true (must keep CallPrint path)",
                p.name
            ),
        }
    }

    // (5) PHASE 3a — the nine migrated universe builtins are now DECLARED in std/prelude.chz. Build a
    //     real graph (which always-links std.prelude), read the prelude module's `native` decls, and
    //     cross-check: the parsed native-decl NAME SET equals the nine migrated names; `native fn` ⇒
    //     Builtin & first_class, `native ctor` ⇒ Ctor & !first_class (matching the hollow Rust table);
    //     the union {parsed 9} ∪ {print} equals the whole first-class-or-ctor prelude surface; and each
    //     parsed FnSig equals its HISTORICAL hand-built shape (behavior-preserving migration).
    let t = TmpDir::new();
    let entry = t.write("main.chz", "print(\"hi\")\n");
    let graph =
        crate::resolver::build_graph(&entry).expect("resolve should always-link std.prelude");
    let prelude = graph
        .modules
        .iter()
        .find(|m| m.dotted == ["std", "prelude"])
        .expect("std.prelude must be always-linked into the graph");
    let mut parsed: Vec<(String, crate::ast::NativeKind)> = Vec::new();
    for s in &prelude.ast.stmts {
        if let StmtKind::Native(d) = &s.kind {
            parsed.push((d.name.clone(), d.kind));
        }
    }
    let parsed_names: BTreeSet<&str> = parsed.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        parsed_names,
        BTreeSet::from([
            "print", // ported to a variadic `native fn print(...args: Any, sep, end)` decl
            "ord",
            "chr",
            "panic",
            "int",
            "float",
            "bool",
            "str",
            "bytes",
            "bytearray"
        ]),
        "std/prelude.chz native decls must be the nine migrated universe builtins plus ported `print`"
    );
    // Metadata agreement: the parsed decl KIND must match the hollow Rust table's intrinsic/first_class.
    for (name, kind) in &parsed {
        let p = prelude_fn(name).unwrap_or_else(|| panic!("'{name}' missing from PRELUDE table"));
        match kind {
            crate::ast::NativeKind::Fn => {
                // `print` is a first-class native fn whose intrinsic is the specialized `Print`
                // (it lowers to `CallPrint`/`CallPrintSep`, not the generic `Builtin` dispatch); every
                // other native fn (`ord`/`chr`/`panic`) is `Builtin`.
                let want = if *name == "print" {
                    Intrinsic::Print
                } else {
                    Intrinsic::Builtin
                };
                assert_eq!(p.intrinsic, want, "'{name}': native fn intrinsic");
                assert!(p.first_class, "'{name}': native fn ⇒ first_class");
            }
            crate::ast::NativeKind::Ctor => {
                assert_eq!(p.intrinsic, Intrinsic::Ctor, "'{name}': native ctor ⇒ Ctor");
                assert!(!p.first_class, "'{name}': native ctor ⇒ !first_class");
            }
        }
    }
    // The `.chz`-DECLARED surface = {parsed 9} ∪ {print, synthetic}. Phase 2b folded the four GENERIC
    // container ctors into the table for DISPATCH single-source, but they are deliberately NOT `.chz`
    // decls (they are generic — native ctor generic-decl support is a later, maybe-never concern), so
    // they are the exact set by which the TABLE surface exceeds the `.chz`-declared surface. Asserting
    // that split pins the design: table-sourced dispatch ⊋ native-declared, differing by exactly the
    // four container ctors, and each is a NON-first-class Ctor row that is NOT parsed from `.chz`.
    const CONTAINER_CTORS: [&str; 4] = ["List", "Map", "Set", "range"];
    let mut whole: BTreeSet<&str> = parsed_names.clone();
    whole.insert("print");
    let table_surface: BTreeSet<&str> = PRELUDE.iter().map(|p| p.name).collect();
    let mut table_minus_containers = table_surface.clone();
    for c in CONTAINER_CTORS {
        table_minus_containers.remove(c);
    }
    assert_eq!(
        whole, table_minus_containers,
        "the PRELUDE table MINUS the four container ctors must equal the nine std/prelude.chz decls plus synthetic `print`"
    );
    for c in CONTAINER_CTORS {
        let p = prelude_fn(c).unwrap_or_else(|| panic!("'{c}' must be a PRELUDE row (2b)"));
        assert_eq!(p.intrinsic, Intrinsic::Ctor, "'{c}' must be a Ctor row");
        assert!(
            !p.first_class,
            "container ctor '{c}' must be non-first-class"
        );
        assert!(
            !parsed_names.contains(c),
            "container ctor '{c}' is table-sourced for DISPATCH, deliberately NOT a std/prelude.chz native decl (generic)"
        );
    }
    // Each parsed FnSig must equal its HISTORICAL hand-built shape (byte-identical migration). Read the
    // sigs through a prelude-seeded checker (same path production uses).
    let mut sc = Checker::new();
    sc.seed_native_prelude_sigs();
    let hist: &[(&str, Vec<Ty>, Ty)] = &[
        ("ord", vec![Ty::Str], Ty::Int),
        ("chr", vec![Ty::Int], Ty::Str),
        ("panic", vec![Ty::Str], Ty::Unknown),
        ("int", vec![Ty::Unknown], Ty::Int),
        ("float", vec![Ty::Unknown], Ty::Float),
        ("str", vec![Ty::Unknown], Ty::Str),
        ("bytes", vec![Ty::Unknown], Ty::Bytes),
        ("bytearray", vec![Ty::Unknown], Ty::ByteArray),
    ];
    for (name, params, ret) in hist {
        let sig = sc
            .builtin_sig(name)
            .unwrap_or_else(|| panic!("'{name}' has no builtin_sig after prelude seed"));
        assert_eq!(
            &sig.params, params,
            "'{name}' params drifted from historical sig"
        );
        assert_eq!(&sig.ret, ret, "'{name}' return drifted from historical sig");
    }
}

/// Phase 3a — a `native fn`/`native ctor` decl is PRELUDE/STD-ONLY: in a user (non-stdlib) module it
/// is a clear checker error (a user can't bind a name to a nonexistent intrinsic — a footgun guard).
#[test]
fn native_decl_in_user_file_rejected() {
    rejects(
        "native fn foo(x: int) -> int\n",
        "native fn/ctor declarations are only allowed in standard-library modules",
    );
    entry_rejects(
        "native ctor bar(x) -> int\n",
        "native fn/ctor declarations are only allowed in standard-library modules",
    );
    // Phase 4f — flipping parse_native to parse_params(true) makes a DEFAULTED native-fn param PARSE
    // in a user file too; the checker's stdlib-only hoist rejection must still fire (the default never
    // matters — the decl itself is rejected). Guards against the parser-strictness relaxation
    // weakening the user-file guard.
    rejects(
        "native fn baz(x: int = 0) -> int\n",
        "native fn/ctor declarations are only allowed in standard-library modules",
    );
}

/// Phase 4a — a `native struct` decl is likewise PRELUDE/STD-ONLY: in a user (non-stdlib) module it is
/// a clear checker error (a user can't declare a native type whose layout the runtime doesn't know).
#[test]
fn native_struct_in_user_file_rejected() {
    rejects(
        "native struct Foo:\n    a: int\n",
        "native struct declarations are only allowed in standard-library modules",
    );
    entry_rejects(
        "native struct Foo:\n    a: int\n",
        "native struct declarations are only allowed in standard-library modules",
    );
}

/// Phase 5b — a `native enum` decl is likewise PRELUDE/STD-ONLY: in a user (non-stdlib) module it is a
/// clear checker error (a user can't declare a reserved builtin enum's variant shape). The ONLY native
/// enums are `std/prelude.chz`'s `Option`/`Result`.
#[test]
fn native_enum_in_user_file_rejected() {
    rejects(
        "native enum Foo:\n    A\n    B\n",
        "native enum declarations are only allowed in standard-library modules",
    );
    entry_rejects(
        "native enum Foo:\n    A\n    B\n",
        "native enum declarations are only allowed in standard-library modules",
    );
}

/// Phase 5b BEHAVIOR-PRESERVING DRIFT GUARD: the reserved `Option`/`Result` variant SHAPE is now ALSO
/// declared in `std/prelude.chz` as `native enum Option[T]` / `native enum Result[T, E]`, but their
/// identity, `?` propagation, match exhaustiveness, and `Ok`/`Err`/`Some`/`None` construction stay
/// 100% Rust-inline (`variants_of`/`match_kind`/`resolve_type`, untouched). This asserts the
/// parsed+resolved variant set from the `.chz` decl BYTE-EQUALS the inline `variants_of` maps, so the
/// two source-of-truth expressions can never drift. Compared with EXPLICIT `E` (the `Result`'s
/// `Error`-protocol surface default is injected by `resolve_type`, NOT encoded in the variant), and
/// asserts NO ported methods (Option/Result carry zero bespoke method arms).
#[test]
fn native_enum_option_result_shape_matches_inline() {
    let path = crate::resolver::std_root().join("prelude.chz");
    let src = std::fs::read_to_string(&path).expect("read std/prelude.chz");
    let toks = crate::lexer::tokenize(&src).expect("tokenize prelude");
    let module = crate::parser::parse(toks).expect("parse prelude");
    let mut c = Checker::new();
    c.current_module_is_stdlib = true;
    // Option[T]: Some(T)/None — the harvested shape must equal variants_of(Ty::option(Param "T")).
    let (opt_vmap, opt_methods) = c
        .harvest_native_enum_table(&module, "Option")
        .expect("native enum Option must be declared in std/prelude.chz");
    assert!(opt_methods.is_empty(), "Option carries no ported methods");
    let opt_inline = c
        .variants_of(&Ty::option(Ty::Param("T".to_string())))
        .expect("inline Option variants_of");
    assert_eq!(
        opt_vmap, opt_inline,
        "native enum Option drifted from inline variants_of"
    );
    // Result[T, E]: Ok(T)/Err(E) — must equal variants_of(Ty::result_e(Param "T", Param "E")).
    let (res_vmap, res_methods) = c
        .harvest_native_enum_table(&module, "Result")
        .expect("native enum Result must be declared in std/prelude.chz");
    assert!(res_methods.is_empty(), "Result carries no ported methods");
    let res_inline = c
        .variants_of(&Ty::result_e(
            Ty::Param("T".to_string()),
            Ty::Param("E".to_string()),
        ))
        .expect("inline Result variants_of");
    assert_eq!(
        res_vmap, res_inline,
        "native enum Result drifted from inline variants_of"
    );
}

/// Phase 5c-protocols BEHAVIOR-PRESERVING DRIFT GUARD: all 18 reserved protocols are now ALSO declared
/// in `std/prelude.chz` as plain `protocol` decls, but `prebuilt_protocols` stays the live runtime source
/// (conformance / operator lowering / `check_bounds` untouched). This asserts each file-backed protocol's
/// harvested SHAPE (`type_params`, `embeds`, ordered method `FnSig`s) BYTE-EQUALS the Rust seed, so the two
/// source expressions can never silently drift. `Iterable` completes the set: its `iter(self) ->
/// Iterator[Elem]` return type resolves via `resolve_type`'s dedicated `Iterator[T]` value arm to the same
/// `Ty::Struct("Iterator",[Elem])` the seed uses, so its shape byte-matches like the other 16.
#[test]
fn native_protocol_shapes_match_prebuilt_seed() {
    let path = crate::resolver::std_root().join("prelude.chz");
    let src = std::fs::read_to_string(&path).expect("read std/prelude.chz");
    let toks = crate::lexer::tokenize(&src).expect("tokenize prelude");
    let module = crate::parser::parse(toks).expect("parse prelude");
    let mut c = Checker::new();
    c.current_module_is_stdlib = true;
    let seed = prebuilt_protocols();
    for name in [
        "Comparable",
        "Stringable",
        "Error",
        "Hashable",
        "Add",
        "Sub",
        "Mul",
        "Div",
        "Mod",
        "Neg",
        "Arithmetic",
        "Iterator",
        "Iterable",
        "Index",
        "IndexSet",
        "Slice",
        "Convert",
    ] {
        let got = c.harvest_protocol_shape(&module, name).unwrap_or_else(|| {
            panic!("reserved protocol '{name}' must be declared in std/prelude.chz")
        });
        let want = seed.get(name).expect("reserved protocol in prebuilt seed");
        assert_eq!(
            got.type_params, want.type_params,
            "protocol '{name}' type_params drift"
        );
        assert_eq!(got.embeds, want.embeds, "protocol '{name}' embeds drift");
        assert_eq!(
            got.methods.len(),
            want.methods.len(),
            "protocol '{name}' method count drift"
        );
        for ((gn, gs), (wn, ws)) in got.methods.iter().zip(&want.methods) {
            assert_eq!(gn, wn, "protocol '{name}' method name/order drift");
            assert!(
                fn_sig_eq(gs, ws),
                "protocol '{name}' method '{gn}' sig drift"
            );
        }
    }
}

/// Phase 3a — the migrated builtins keep their historical first-classness through the `.chz` decls.
/// `native fn` (ord/chr/panic) stays a first-class VALUE (binds to a name, types `Ty::BuiltinFn`);
/// `native ctor` (int/str/…) stays NON-first-class (value-position use is a checker error). This is
/// the graph path (prelude always-linked), complementing the single-module `ok()`-based tests.
#[test]
fn native_prelude_firstclassness_preserved_on_graph_path() {
    entry_ok("fn w():\n    f := ord\n    print(f(\"a\"))\nw()\n");
    entry_ok("fn w():\n    p := panic\n    p(\"boom\")\nw()\n");
    // A `native ctor` (scalar constructor) is NOT first-class — value-position use is rejected
    // (same fall-through path as `f := List`; the message names the offending `int`).
    entry_rejects("fn w():\n    f := int\nw()\n", "int");
}

/// Phase 5a-containers BEHAVIOR-PRESERVING GUARD: every `List`/`Map`/`Set` method sig harvested from
/// `std/prelude.chz`'s `native struct` decls (looked up via `native_handle_method` with the value's
/// element/key/value type substituted) must BYTE-MATCH the sig the retired bespoke `list_method_sig`/
/// `map_method_sig`/`set_method_sig` arms produced. `Map` uses distinct `Int`/`Str` for K/V so a K↔V
/// swap in the declaration order would be caught. A mismatch here is a behavior change (the whole port
/// is behavior-preserving), so this pins the ported surface exactly.
#[test]
fn container_method_sigs_byte_match() {
    let c = prelude_container_checker();
    // Assert one method's harvested+substituted sig equals the hand-written expected (params/ret/
    // min_params) — the retired-arm shape. `Map` uses distinct `Int`/`Str` for K/V so a K↔V swap in the
    // declaration order would be caught.
    let chk = |ty: &str, method: &str, targs: &[Ty], exp_params: Vec<Ty>, exp_ret: Ty| {
        let sig = c
            .native_handle_method(ty, method, targs)
            .unwrap_or_else(|| panic!("{ty}.{method} must resolve via the harvested table"));
        assert_eq!(sig.params, exp_params, "{ty}.{method} params");
        assert_eq!(sig.ret, exp_ret, "{ty}.{method} ret");
        assert_eq!(
            sig.min_params,
            exp_params.len(),
            "{ty}.{method} min_params (all required)"
        );
    };
    let li = || Ty::list(Ty::Int);
    let int = || vec![Ty::Int];
    let kv = || vec![Ty::Int, Ty::Str]; // Map targs: K=Int, V=Str.
    // List[Int] — the 9 flat methods (map/filter/fold/sort/sort_by/sort_by_key stay residual).
    chk("List", "len", &int(), vec![], Ty::Int);
    chk("List", "push", &int(), vec![Ty::Int], Ty::Nil);
    chk("List", "pop", &int(), vec![], Ty::option(Ty::Int));
    chk("List", "reverse", &int(), vec![], Ty::Nil);
    chk("List", "contains", &int(), vec![Ty::Int], Ty::Bool);
    chk("List", "index_of", &int(), vec![Ty::Int], Ty::Int);
    chk("List", "concat", &int(), vec![li()], li());
    chk("List", "extend", &int(), vec![li()], Ty::Nil);
    chk("List", "sum", &int(), vec![], Ty::Int);
    // `sort` is now file-backed (`native fn sort(self) -> nil where T: Comparable`): it resolves via
    // the harvested table with a nil return, and carries a `where T: Comparable` bound (enforced at
    // the call site by the Ty::List arm's `enforce_bounds`).
    chk("List", "sort", &int(), vec![], Ty::Nil);
    let sort_sig = c.native_handle_method("List", "sort", &int()).unwrap();
    assert_eq!(
        sort_sig.where_bounds.len(),
        1,
        "List.sort carries one where bound"
    );
    assert_eq!(sort_sig.where_bounds[0].name, "T");
    assert_eq!(sort_sig.where_bounds[0].bounds[0].name, "Comparable");
    // `sum` gains a `where T: Add` annotation (documentation; the numeric check-gate is the real
    // enforcement — see `list_sum_struct_with_add_still_rejected_at_check`).
    let sum_sig = c.native_handle_method("List", "sum", &int()).unwrap();
    assert_eq!(
        sum_sig.where_bounds.len(),
        1,
        "List.sum carries one where bound"
    );
    assert_eq!(sum_sig.where_bounds[0].bounds[0].name, "Add");
    // Map[Int, Str] — K=Int, V=Str (distinct so the K/V order is pinned).
    chk("Map", "len", &kv(), vec![], Ty::Int);
    chk("Map", "has", &kv(), vec![Ty::Int], Ty::Bool);
    chk("Map", "get", &kv(), vec![Ty::Int], Ty::option(Ty::Str));
    chk("Map", "keys", &kv(), vec![], Ty::list(Ty::Int));
    chk("Map", "values", &kv(), vec![], Ty::list(Ty::Str));
    chk("Map", "remove", &kv(), vec![Ty::Int], Ty::option(Ty::Str));
    chk(
        "Map",
        "merge",
        &kv(),
        vec![Ty::map(Ty::Int, Ty::Str)],
        Ty::map(Ty::Int, Ty::Str),
    );
    chk(
        "Map",
        "update",
        &kv(),
        vec![Ty::map(Ty::Int, Ty::Str)],
        Ty::Nil,
    );
    // Set[Int] — the 7 flat methods.
    chk("Set", "len", &int(), vec![], Ty::Int);
    chk("Set", "has", &int(), vec![Ty::Int], Ty::Bool);
    chk("Set", "add", &int(), vec![Ty::Int], Ty::Nil);
    chk("Set", "remove", &int(), vec![Ty::Int], Ty::Bool);
    chk(
        "Set",
        "union",
        &int(),
        vec![Ty::set(Ty::Int)],
        Ty::set(Ty::Int),
    );
    chk(
        "Set",
        "intersection",
        &int(),
        vec![Ty::set(Ty::Int)],
        Ty::set(Ty::Int),
    );
    chk(
        "Set",
        "difference",
        &int(),
        vec![Ty::set(Ty::Int)],
        Ty::set(Ty::Int),
    );
    // `map`/`filter`/`fold`/`sort_by`/`sort_by_key` are now file-backed too (the closure-return
    // loop-back landed), so they resolve via the harvested table. `map`/`fold`/`sort_by_key` carry
    // their own `[U]`/`[K]` type param (routed through `infer_generic_method`); `filter`/`sort_by` are
    // flat. `get` is genuinely not a List method (it stays a must-not).
    let map_sig = c.native_handle_method("List", "map", &[Ty::Int]).unwrap();
    assert_eq!(map_sig.type_params.len(), 1, "List.map carries its own [U]");
    assert_eq!(map_sig.type_params[0].name, "U");
    let fold_sig = c.native_handle_method("List", "fold", &[Ty::Int]).unwrap();
    assert_eq!(fold_sig.type_params[0].name, "U");
    let sbk_sig = c
        .native_handle_method("List", "sort_by_key", &[Ty::Int])
        .unwrap();
    assert_eq!(sbk_sig.type_params[0].name, "K");
    assert_eq!(sbk_sig.type_params[0].bounds[0].name, "Comparable");
    // `filter`/`sort_by` are flat (no own type param) but still harvested (was residual).
    assert!(
        c.native_handle_method("List", "filter", &[Ty::Int])
            .is_some()
    );
    assert!(
        c.native_handle_method("List", "sort_by", &[Ty::Int])
            .is_some()
    );
    assert!(
        c.native_handle_method("List", "get", &[Ty::Int]).is_none(),
        "List.get is not a method"
    );
}

/// Drift guard (editor hover, Tier C): every method NAME in each authored `*_METHODS` slice MUST
/// resolve to `Some` from its owning `*_method_sig` lookup — so the per-type "methods: …" line
/// `builtin_type_doc` renders can only list methods that provably EXIST. An out-of-date slice (a
/// renamed/removed method, or a typo) fails here instead of shipping a hover that lies. `Ty::Int` is
/// the sampled element type for the generic tables so the numeric-gated entries (`sum`/`add`/`sub`)
/// resolve. (`list.sort` and `bytes`/`bytearray.extend` are handled in `infer_method_call`, NOT these
/// tables, so they are deliberately absent from the slices — see PROGRESS.md.)
#[test]
fn builtin_method_slices_all_resolve() {
    // Phase 5a-containers — List/Map/Set/Channel and the str/bytes/bytearray scalar method sigs are now
    // HARVESTED from std/prelude.chz into each reserved type's method table (the bespoke
    // list_/map_/set_/channel_/str_/bytes_/bytearray_method_sig arms are retired). Resolve the hover
    // slices against those seeded tables.
    let cc = prelude_container_checker();
    let chk_container = |ty: &str, slice: &[&str]| {
        let methods = &cc
            .structs
            .get(ty)
            .unwrap_or_else(|| panic!("seeded {ty} struct"))
            .methods;
        for m in slice {
            assert!(
                methods.contains_key(*m),
                "{ty} hover slice lists '{m}' but the harvested std/prelude.chz {ty} has no such method (drift)"
            );
        }
    };
    chk_container("List", LIST_METHODS);
    chk_container("Map", MAP_METHODS);
    chk_container("Set", SET_METHODS);
    chk_container("Channel", CHANNEL_METHODS);
    chk_container("str", STR_METHODS);
    chk_container("bytes", BYTES_METHODS);
    chk_container("bytearray", BYTEARRAY_METHODS);
    // Phase 4c-net / 4c-concurrency — Socket/Listener (net) and Shared/RwShared/Atomic/Executor
    // (concurrency) method sigs are now HARVESTED from std/net.chz / std/concurrency.chz into each
    // type's method table (the bespoke socket_/listener_/shared_/rwshared_/atomic_/executor_method_sig
    // arms are retired). Resolve the hover slices against those harvested tables via a graph check.
    let harvested = |sig: &ModuleSig, ty: &str| -> std::collections::HashMap<String, FnSig> {
        sig.struct_defs
            .get(ty)
            .unwrap_or_else(|| panic!("harvested {ty} struct_def"))
            .methods
            .clone()
    };
    let chk_harvested = |methods: &std::collections::HashMap<String, FnSig>,
                         slice: &[&str],
                         ty: &str| {
        for m in slice {
            assert!(
                methods.contains_key(*m),
                "{ty} hover slice lists '{m}' but the harvested std/*.chz {ty} has no such method (drift)"
            );
        }
    };
    let net = native_module_sig_via_graph("net");
    chk_harvested(&harvested(&net, "Socket"), SOCKET_METHODS, "Socket");
    chk_harvested(&harvested(&net, "Listener"), LISTENER_METHODS, "Listener");
    let conc = native_module_sig_via_graph("concurrency");
    chk_harvested(&harvested(&conc, "Shared"), SHARED_METHODS, "Shared");
    chk_harvested(&harvested(&conc, "RwShared"), RWSHARED_METHODS, "RwShared");
    chk_harvested(&harvested(&conc, "Atomic"), ATOMIC_METHODS, "Atomic");
    chk_harvested(
        &harvested(&conc, "AtomicInt"),
        ATOMIC_INT_METHODS,
        "AtomicInt",
    );
    chk_harvested(&harvested(&conc, "Executor"), EXECUTOR_METHODS, "Executor");
}

/// Drift guard (editor hover, Tier C): every `(module, fn)` named in an authored module-fn doc slice
/// MUST exist in that module's `native_module_sig`, so the module-fn hover doc can only annotate a
/// function that is really exported. A renamed/removed native fn fails here.
#[test]
fn module_fn_docs_all_resolve() {
    for (module, docs) in MODULE_FN_DOCS {
        // Build the EFFECTIVE sig via the graph: math/io/os are file-backed (phase 4d), so their fns
        // are harvested from `std/<M>.chz` and the docs are re-attached by `attach_native_module_metadata`
        // — `native_module_sig(module)` alone would be empty for them.
        let bare = module.strip_prefix("std.").unwrap_or(module);
        let sig = native_module_sig_via_graph(bare);
        for (fname, doc) in *docs {
            let f = sig.functions.get(*fname).unwrap_or_else(|| {
                panic!("{module} doc slice lists fn '{fname}' but the module has no such function")
            });
            assert_eq!(
                f.doc.as_deref(),
                Some(*doc),
                "{module}.{fname} hover doc not attached after phase-4d migration"
            );
        }
    }
}

// ============================================================================
// Closure-parameter type inference (v1) — see docs/plan 2026-06-28.
// ============================================================================

// Phase 1a — expected type from a native HOF that routes through `check_args`
// (`Shared.update`): an unannotated closure param binds to the element type, so
// a body use incompatible with it is rejected (was accepted: param = Unknown).
#[test]
fn closure_param_inferred_from_shared_update_conflict_rejected() {
    entry_rejects(
        "import std.concurrency\nfn main():\n    s := Shared(0)\n    s.update(fn(x): x.upper())\n",
        "has no method 'upper'",
    );
}

#[test]
fn closure_param_inferred_from_shared_update_ok() {
    entry_ok(
        "import std.concurrency\nfn main():\n    s := Shared(0)\n    s.update(fn(x): x + 1)\n",
    );
}

// Native list HOFs (file-backed `map`/`filter` in std/prelude.chz, routed through the generic solver):
// the closure param binds to the element type, so a body use incompatible with it is rejected.
#[test]
fn map_closure_conflict_rejected() {
    entry_rejects(
        "fn main():\n    xs := [1, 2, 3]\n    ys := xs.map(fn(x): x.upper())\n    print(ys)\n",
        "has no method 'upper'",
    );
}

#[test]
fn closure_param_inferred_from_map_ok() {
    entry_ok("fn main():\n    xs := [1, 2, 3]\n    ys := xs.map(fn(x): x + 1)\n    print(ys)\n");
}

#[test]
fn closure_param_inferred_from_filter_conflict_rejected() {
    entry_rejects(
        "fn main():\n    xs := [1, 2, 3]\n    ys := xs.filter(fn(x): x.upper() == \"A\")\n    print(ys)\n",
        "has no method 'upper'",
    );
}

// Phase 6 — generic-solver closure-return LOOP-BACK: a return-position `[U]` inferred ONLY from an
// UNANNOTATED closure's body must resolve to the CONCRETE body-return type, not leak a `Ty::Param`.
const APPLY_DEFS: &str = "struct Box[T]:\n    v: T\n    fn apply[U](self, f: fn(T) -> U) -> U:\n        return f(self.v)\n";

#[test]
fn generic_method_recovers_return_param_from_unannotated_closure_ok() {
    // `U` is bound ONLY from the unannotated closure `fn(x): x + 1`'s body (int). Before the
    // loop-back the return leaked `U` (rendered `?`), so `n: int = ...` failed.
    entry_ok(&format!(
        "{APPLY_DEFS}fn main():\n    n: int = Box(3).apply(fn(x): x + 1)\n    print(n)\n"
    ));
}

#[test]
fn generic_method_recovers_return_param_from_unannotated_closure_is_concrete() {
    // The recovered return is CONCRETE `int` (not a leaked `U`): assigning it to `str` must report a
    // real int/str mismatch — proof the loop-back pinned `U=int`.
    entry_rejects(
        &format!("{APPLY_DEFS}fn main():\n    z: str = Box(3).apply(fn(x): x + 1)\n    print(z)\n"),
        "int",
    );
}

// Phase 6 — file-backed List HOF characterization: the inferred types MUST match the retired bespoke
// `infer_list_hof` exactly (the whole point of the loop-back). Pinned via the assign-mismatch trick.
#[test]
fn list_map_typed_closure_infers_list_str() {
    // Typed closure `fn(x: int) -> str`: map yields List[str]; a str element is fine, an int isn't.
    entry_ok(
        "fn main():\n    ys := [1, 2].map(fn(x: int) -> str: \"a\")\n    s: str = ys[0]\n    print(s)\n",
    );
    entry_rejects(
        "fn main():\n    ys := [1, 2].map(fn(x: int) -> str: \"a\")\n    n: int = ys[0]\n    print(n)\n",
        "cannot assign str to variable of type int",
    );
}

#[test]
fn list_map_unannotated_closure_recovers_int() {
    // THE PART-1 PAYOFF: an UNANNOTATED closure `fn(x): x * 2` → map yields List[int] (recovered from
    // the closure body), NOT a leaked List[?]. Assigning an element to str must mismatch on `int`.
    entry_rejects(
        "fn main():\n    ys := [1, 2].map(fn(x): x * 2)\n    s: str = ys[0]\n    print(s)\n",
        "int",
    );
}

#[test]
fn list_map_empty_list_element_behavior_preserved() {
    // A TYPED-but-empty list: element int, so `map(fn(x): x*2)` degrades cleanly to List[int] via the
    // loop-back (x=int, body int). No error — byte-identical to the retired bespoke arm.
    entry_ok(
        "fn main():\n    xs: List[int] = []\n    ys := xs.map(fn(x): x * 2)\n    n: int = ys[0]\n    print(n)\n",
    );
    // A BARE `[]` receiver still needs an annotation — same diagnostic the old bespoke arm produced
    // (the empty-collection rule is upstream of map dispatch; unchanged by the port).
    entry_rejects(
        "fn main():\n    ys := [].map(fn(x): x * 2)\n    print(ys)\n",
        "cannot infer element type of empty collection",
    );
}

#[test]
fn list_map_chained_and_nested_ok() {
    // Nested `xs.map(fn(x): [x])` → List[List[int]]; a second `.map` over it flows the element type.
    entry_ok(
        "fn main():\n    ys := [1, 2].map(fn(x): [x]).map(fn(r): r.len())\n    n: int = ys[0]\n    print(n)\n",
    );
    // Chained map -> filter -> fold: map to str, filter, fold to a joined str.
    entry_ok(
        "fn main():\n    r := [1, 2, 3].map(fn(x): x * 2).filter(fn(x): x > 2).fold(0, fn(a, x): a + x)\n    n: int = r\n    print(n)\n",
    );
}

#[test]
fn list_fold_init_typed_result() {
    // fold's `U` binds from `init` in pass 1 (not the loop-back): a str init → str result.
    entry_ok(
        "fn main():\n    s := [1, 2, 3].fold(\"\", fn(a, x): a + \"!\")\n    t: str = s\n    print(t)\n",
    );
    entry_rejects(
        "fn main():\n    s := [1, 2, 3].fold(0, fn(a, x): a + x)\n    t: str = s\n    print(t)\n",
        "cannot assign int to variable of type str",
    );
}

// Phase 6 — CONFIRMED-BUG regression (adversarial review): the closure-return loop-back's
// still-unbound degrade must be CONDITIONAL. A method type param that appears ONLY in the return
// position and in NO parameter (`fn make[U](self) -> U`) is genuinely un-inferable — no argument can
// bind it. It must stay a leaked `Ty::Param` (rejected on assignment to a concrete type), NOT degrade
// to `Unknown` (which `assignable` treats as universally assignable, silently masking the
// un-inferable-ness and letting a wrong static type escape onto the value). This is the exact base
// behavior an unconditional degrade regressed.
const RETURN_ONLY_PARAM_DEFS: &str =
    "struct Box[T]:\n    v: T\n    fn make[U](self) -> U:\n        return self.make()\n";

#[test]
fn return_only_method_type_param_stays_uninferable_rejected() {
    // `U` appears ONLY in `-> U`, in no parameter → un-inferable → assigning the result to a concrete
    // type must be REJECTED (naming the un-inferable `U`). An unconditional degrade wrongly accepted.
    entry_rejects(
        &format!(
            "{RETURN_ONLY_PARAM_DEFS}fn main():\n    b := Box(1)\n    z: str = b.make()\n    print(z)\n"
        ),
        "cannot assign U to variable of type str",
    );
}

// Phase 6 — Bug D: the closure-return loop-back must recover a method `[U]` when the closure body is
// a NESTED FREE generic call whose return is the callee's own type param. `xs.map(fn(x): ident(x))`
// where `ident[T](x: T) -> T` must yield List[int] (not a leaked List[T]).
const IDENT_DEFS: &str = "fn ident[T](x: T) -> T:\n    return x\n";

#[test]
fn map_closure_body_free_generic_call_recovers_int() {
    // The repro: closure body `ident(x)` returns ident's OWN `T`; the solver knows `x: int` and must
    // unify `int` into ident's `T` via the checking-mode re-inference, so `ys: List[int]` and
    // `ys[0] + 1` type-checks (prints 2 at runtime).
    entry_ok(&format!(
        "{IDENT_DEFS}fn main():\n    xs := [1, 2, 3]\n    ys := xs.map(fn(x): ident(x))\n    print(ys[0] + 1)\n"
    ));
}

#[test]
fn map_closure_body_free_generic_call_stays_sound() {
    // SOUNDNESS guard: `U` must be recovered as CONCRETE `int`, never degraded to `Unknown` (which
    // `assignable` would launder). Assigning the int-typed result to List[str] / List[List[int]] must
    // reject naming `int`.
    entry_rejects(
        &format!(
            "{IDENT_DEFS}fn main():\n    xs := [1, 2, 3]\n    zs: List[str] = xs.map(fn(x): ident(x))\n    print(zs)\n"
        ),
        "int",
    );
    entry_rejects(
        &format!(
            "{IDENT_DEFS}fn main():\n    xs := [1, 2, 3]\n    ws: List[List[int]] = xs.map(fn(x): ident(x))\n    print(ws)\n"
        ),
        "int",
    );
}

#[test]
fn bug_d_boundaries_stay_green() {
    // (1) direct closure body — already worked, must stay green.
    entry_ok("fn main():\n    ys := [1, 2].map(fn(x): x * 2)\n    n: int = ys[0]\n    print(n)\n");
    // (2) generic METHOD in body — receiver carries the type arg, resolves fine today.
    entry_ok(
        "struct W[T]:\n    v: T\n    fn get(self) -> T:\n        return self.v\nfn main():\n    ws := [W(1), W(2)]\n    n: int = ws.map(fn(w): w.get())[0] + 1\n    print(n)\n",
    );
    // (3) downstream-pinned — fold's `+` and filter's `>` pin T=int via the body arithmetic.
    entry_ok(&format!(
        "{IDENT_DEFS}fn main():\n    xs := [1, 2, 3]\n    s := xs.fold(0, fn(acc, x): acc + ident(x))\n    print(s)\n"
    ));
    entry_ok(&format!(
        "{IDENT_DEFS}fn main():\n    xs := [1, 2, 3]\n    ys := xs.filter(fn(x): ident(x) > 1)\n    print(ys)\n"
    ));
    // (4) genuinely ambiguous closure body — must give a clean "cannot infer" diagnostic, not a panic.
    entry_rejects(
        "fn main():\n    ys := [1, 2].map(fn(x): fn(y): x + y)\n    print(ys)\n",
        "cannot infer",
    );
}

#[test]
fn fold_closure_wrong_return_rejected() {
    // SOUNDNESS (the mask must NOT disable the closure-return check when the method's return-position
    // `[U]` is ALREADY pinned by another argument): `fold[U]`'s `U` is pinned to `int` by `init` (arg
    // 0), so the closure's return `fn(U, T) -> U` has the CONCRETE contract `int`. An unannotated
    // closure whose body is a `str` must be rejected at check time — masking its fallback return to
    // `Unknown` (which `assignable` laundered) let a str value escape onto an `int`-typed binding.
    entry_rejects(
        "fn main():\n    xs := [1, 2, 3]\n    s := xs.fold(0, fn(acc, x): \"wrong\")\n    print(s + 1)\n",
        "fold",
    );
    // The same hole via an explicit annotation: `n: int` must not accept a str-returning fold body.
    entry_rejects(
        "fn main():\n    xs := [1, 2, 3]\n    n: int = xs.fold(0, fn(acc, x): \"wrong\")\n    print(n)\n",
        "fold",
    );
}

#[test]
fn fold_closure_body_free_generic_call_recovers_int() {
    // Bug D (adversarial-review fix): a `fold[U]` whose `U` is pinned CONCRETE by `init` (arg 0) AND
    // whose unannotated closure body is a NESTED FREE generic call (`ident(x)` / `ident(acc)`, where
    // `ident[T](x: T) -> T`) must be ACCEPTED — the prepass leaks the callee's own rigid `Ty::Param`,
    // but the checking-mode re-inference types the body `int`. The earlier gate (masking only a still-
    // free `[U]`) spuriously rejected these with `argument to 'fold' has type fn(?, ?) -> T`.
    // (a) body references the closure's element param `x`.
    entry_ok(
        "fn ident[T](x: T) -> T:\n    return x\nfn main():\n    xs := [1, 2, 3]\n    s := xs.fold(0, fn(acc, x): ident(x))\n    print(s + 1)\n",
    );
    // (b) body references the closure's accumulator param `acc` (bare nested generic call).
    entry_ok(
        "fn ident[T](x: T) -> T:\n    return x\nfn main():\n    xs := [1, 2, 3]\n    s := xs.fold(0, fn(acc, x): ident(acc))\n    print(s)\n",
    );
}

// Phase 6b — Bug D FREE-FN analog: the closure-return loop-back must also recover a generic FREE
// FUNCTION's return-only `[U]` from an inferable closure/fn body. Same mechanism as the method path,
// ported into `infer_generic_call` (`recover_return_only_params`). Before the fix these all rejected
// with `cannot apply + to U/B/T and int`.
#[test]
fn free_fn_hof_returnonly_recovers_scalar_and_container() {
    // Scalar return-only U (param pinned by the `fn(int) -> U` slot): `applyone(5, fn(x): x*2)` yields
    // int, so `y + 1` type-checks.
    entry_ok(
        "fn applyone[U](x: int, f: fn(int) -> U) -> U:\n    return f(x)\nfn main():\n    y := applyone(5, fn(x): x * 2)\n    print(y + 1)\n",
    );
    // Container `-> List[U]` form: `mymap([1,2,3], fn(x): x*2)` yields List[int], so `ys[0] + 1` checks.
    entry_ok(
        "fn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := mymap([1, 2, 3], fn(x): x * 2)\n    print(ys[0] + 1)\n",
    );
}

#[test]
fn free_fn_hof_returnonly_pinned_mismatch_rejected() {
    // SOUNDNESS (the laundering hole — free-fn analog of `fold_closure_wrong_return_rejected`): a
    // free HOF whose return-only `U` is pinned by a SIBLING value arg (`init: U` = 0 ⇒ `U = int`) and
    // whose unannotated closure returns a MISMATCHING type (`str`) must be a clean type error, not
    // laundered onto the pinned `int`. Proves the helper's SEPARATE concrete-return check is load-bearing.
    entry_rejects(
        "fn f[U](init: U, g: fn(int) -> U, xs: List[int]) -> U:\n    return g(xs[0])\nfn main():\n    y := f(0, fn(x): str(x), [1, 2, 3])\n    print(y)\n",
        "returns",
    );
    // apply-shape sibling-pin variant: `B` pinned to `int` by the `sink: B` = 99 arg, closure returns str.
    entry_rejects(
        "fn apply[A, B](f: fn(A) -> B, a: A, sink: B) -> B:\n    return sink\nfn main():\n    y := apply(fn(x): str(x), 5, 99)\n    print(y)\n",
        "returns",
    );
}

#[test]
fn free_fn_hof_returnonly_unknown_cored_container_not_laundered() {
    // SOUNDNESS regression (review fix): a param-dependent closure whose body is a CONTAINER of the
    // param (`fn(x): [x]` → prepass `List[Unknown]`) must NOT pre-bind the HOF's return-only `U` to
    // `List[Unknown]` in pass-1 — that would launder `List[str]` onto it (`unify` only skips a
    // TOP-level Unknown, not a nested one). The pre-bind gate is `ty_fully_concrete`, so `List[Unknown]`
    // defers to the loop-back's refined re-inference, which recovers the CONCRETE `List[int]`.
    // (a) recovers the concrete element type — `ys[0][0] + 1` type-checks:
    entry_ok(
        "fn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := mymap([1, 2, 3], fn(x): [x])\n    print(ys[0][0] + 1)\n",
    );
    // (b) does NOT launder — assigning the recovered `List[List[int]]` to `List[List[str]]` is rejected:
    entry_rejects(
        "fn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := mymap([1, 2, 3], fn(x): [x])\n    zs: List[List[str]] = ys\n    print(zs)\n",
        "",
    );
}

#[test]
fn free_fn_hof_returnonly_boundaries() {
    const IDENT: &str = "fn ident[T](x: T) -> T:\n    return x\n";
    // p4 — nested free-generic-call body in a CONTAINER HOF (`fn(x): ident(x)`): the prepass leaks
    // ident's own `T`; checking-mode re-inference recovers int, so `ys[0] + 1` checks.
    entry_ok(&format!(
        "{IDENT}fn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := mymap([1, 2, 3], fn(x): ident(x))\n    print(ys[0] + 1)\n"
    ));
    // p16 — nested free-generic-call body in a SCALAR HOF.
    entry_ok(&format!(
        "{IDENT}fn applyone[U](x: int, f: fn(int) -> U) -> U:\n    return f(x)\nfn main():\n    y := applyone(5, fn(x): ident(x))\n    print(y + 1)\n"
    ));
    // p12 — protocol-bounded return-only `[U: Add]`: recover U=int AND re-enforce the `Add` bound.
    entry_ok(
        "fn mapadd[U: Add](x: int, f: fn(int) -> U) -> U:\n    return f(x)\nfn main():\n    y := mapadd(5, fn(x): x * 2)\n    print(y + 1)\n",
    );
    // p2 — sibling value arg pins A (=int), which flows into the closure PARAM; B is return-only.
    entry_ok(
        "fn apply[A, B](f: fn(A) -> B, a: A) -> B:\n    return f(a)\nfn main():\n    y := apply(fn(x): x * 2, 5)\n    print(y + 1)\n",
    );
    // p11 — multiple return-only params `[U, V]`; only U flows to the result, both recovered.
    entry_ok(
        "fn two[U, V](xs: List[int], f: fn(int) -> U, g: fn(int) -> V) -> U:\n    return f(xs[0])\nfn main():\n    y := two([1, 2, 3], fn(x): x * 2, fn(x): str(x))\n    print(y + 1)\n",
    );
    // p9 — chained/nested free HOFs `mymap(mymap(...))`.
    entry_ok(
        "fn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := mymap(mymap([1, 2, 3], fn(x): x * 2), fn(y): y + 1)\n    print(ys[0] + 1)\n",
    );
    // p17 — str return-only recovered and used via `.len()`.
    entry_ok(
        "fn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := mymap([1, 2, 3], fn(x): str(x))\n    print(ys[0].len())\n",
    );
}

#[test]
fn free_fn_hof_must_not_regress() {
    // These ACCEPT on `main` and must stay accepting through the refactor (behavior-preserving).
    // p5 — named-fn arg with a concrete return (not a closure): `double` is `fn(int) -> int`.
    entry_ok(
        "fn double(x: int) -> int:\n    return x * 2\nfn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := mymap([1, 2, 3], double)\n    print(ys[0] + 1)\n",
    );
    // p6 — closure body INDEPENDENT of the param (bool), used via `if`.
    entry_ok(
        "fn keep[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := keep([1, 2, 3], fn(x): x > 0)\n    if ys[0]:\n        print(\"y\")\n",
    );
    // p7 — closure body producing str, used via `.len()` / concat.
    entry_ok(
        "fn keep[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := keep([1, 2, 3], fn(x): str(x))\n    print(ys[0].len())\n",
    );
    // annotated closure on a leaking body: `fn(x): x*2` param annotated, return still recovered.
    entry_ok(
        "fn applyone[U](x: int, f: fn(int) -> U) -> U:\n    return f(x)\nfn main():\n    y := applyone(5, fn(x: int): x * 2)\n    print(y + 1)\n",
    );
    // annotated-RETURN closure on a leaking body.
    entry_ok(
        "fn applyone[U](x: int, f: fn(int) -> U) -> U:\n    return f(x)\nfn main():\n    y := applyone(5, fn(x) -> int: x * 2)\n    print(y + 1)\n",
    );
    // param-position type param whose value is Unknown (empty list) — degrade-to-Unknown edge, `-> int`
    // result independent of U: was `ok` pre-fix, must stay `ok`.
    entry_ok(
        "fn g[U](xs: List[U], f: fn(U) -> int) -> int:\n    return f(xs[0])\nfn main():\n    n := g([], fn(x): 0)\n    print(n)\n",
    );
    // genuinely un-inferable return-only param must STILL reject (leaked `Ty::Param`, not degraded).
    entry_rejects(
        "fn make[U](x: int) -> U:\n    return x\nfn main():\n    y := make(5)\n    print(y)\n",
        "U",
    );
}

#[test]
fn free_fn_hof_ambiguous_stays_clean_error() {
    // A genuinely ambiguous closure body (`fn(y): x+y` — `y` un-inferable) must give a clean
    // `cannot infer` diagnostic and NO host panic — and EXACTLY ONE error (the new loop-back /
    // concrete-return check must not swallow, re-order, or double-report it).
    let errs = check_entry(
        "fn applyone[U](x: int, f: fn(int) -> U) -> U:\n    return f(x)\nfn main():\n    y := applyone(5, fn(x): fn(y): x + y)\n    print(y)\n",
    );
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert!(
        errs[0].message.contains("cannot infer"),
        "expected a 'cannot infer' diagnostic, got: {errs:?}"
    );
}

#[test]
fn free_fn_generic_empty_arg_return_param_stays_rejected() {
    // REGRESSION (adversarial-review bugs 1 & 2): a generic FREE-FN whose type param appears in
    // PARAMETER position but is bound to `Unknown`-nothing by an empty-collection arg must NOT be
    // silently degraded to `Ty::Unknown` on the free-fn path (the method-path degrade is scoped to the
    // method path). Its return-flowing `Ty::Param` must stay leaked so downstream concrete use rejects
    // — matching `main`. The Category-2 empty-collection diagnostic is intended (out of scope of the
    // closure-return recovery) and must survive the shared-helper refactor.
    //
    // `first([]) + 1`: on `main` this is `cannot apply + to U and int`; the branch wrongly degraded U
    // to Unknown and accepted (then panicked at runtime `index 0 out of bounds`).
    entry_rejects(
        "fn first[U](xs: List[U]) -> U:\n    return xs[0]\nfn main():\n    x := first([])\n    print(x + 1)\n",
        "cannot apply + to U and int",
    );
    // `pick([], 0).nonexistent_method()`: U in param position, return-only method use — must reject on
    // the leaked `Ty::Param` (`type parameter U has no method`), not silently accept a method on
    // `Unknown`.
    entry_rejects(
        "fn pick[U](xs: List[U], i: int) -> U:\n    return xs[i]\nfn main():\n    y := pick([], 0)\n    print(y.nonexistent_method())\n",
        "type parameter U has no method",
    );
    // (Note: `takes_str(pick([], 0))` — a leaked `Ty::Param` passed to a concrete `str` slot — is
    // accepted by `main` too, a SEPARATE pre-existing `assignable(concrete, Ty::Param)` leniency, so it
    // is NOT asserted here; this fix only restores the operator/method-use rejections that the branch's
    // Unknown-degrade had laundered.)
    // `tag([])` then heterogeneous pushes: must emit the deliberate Category-2 "un-inferred type
    // parameter U; bind it at the construction site" diagnostic on EACH push (2 errors), not degrade
    // to `List[Unknown]` and backward-pin the element to the first push's type.
    let errs = check_entry(
        "fn tag[U](xs: List[U]) -> List[U]:\n    return xs\nfn main():\n    x := tag([])\n    x.push(\"hello\")\n    x.push(42)\n",
    );
    assert_eq!(errs.len(), 2, "expected exactly two errors, got: {errs:?}");
    assert!(
        errs.iter()
            .all(|e| e.message.contains("un-inferred type parameter U")),
        "expected the un-inferred-U construction-site diagnostic, got: {errs:?}"
    );
}

#[test]
fn free_fn_hof_sibling_closure_param_use_recovers() {
    // REGRESSION (adversarial-review bug 1): a return-only `[T]` bound from a bare closure's CONCRETE
    // return (`fn(): 5` → `int`) must be recovered BEFORE the un-inferable-param probe runs, so a
    // SIBLING closure that uses the SAME `[T]` in PARAMETER position (`g: fn(T) -> int`) is not
    // spuriously rejected as an un-inferable deadlock. On the branch before this fix, pass-1 masking
    // left `T` unbound and `report_uninferable_closure_params` fired `cannot infer type parameter T`
    // + `cannot infer type of parameter 'x'`; `main` accepts and runs (prints 6).
    entry_ok(
        "fn pair[T](f: fn() -> T, g: fn(T) -> int) -> int:\n    return g(f())\nfn main():\n    print(pair(fn(): 5, fn(x): x + 1))\n",
    );
    // The same shape with the return-only param flowing through a chain of consumers.
    entry_ok(
        "fn chain[T](f: fn() -> T, g: fn(T) -> int) -> int:\n    return g(f()) + 1\nfn main():\n    print(chain(fn(): 5, fn(x): x + 1))\n",
    );
    // The Bug-1 recovery must NOT pre-empt a sibling VALUE arg that pins the same return-only param: a
    // concrete closure return that CONFLICTS with a value-arg pin is still rejected (no binding race).
    entry_rejects(
        "fn apply[A, B](f: fn(A) -> B, a: A, sink: B) -> B:\n    return sink\nfn main():\n    y := apply(fn(x): str(x), 5, 99)\n    print(y)\n",
        "returns",
    );
}

#[test]
fn free_fn_hof_conflicting_returnonly_closures_rejected() {
    // SOUNDNESS (adversarial-review bug 2): two closure args binding the SAME return-only `[U]` must
    // AGREE or be rejected. The interleaved loop-back binds `[U]` from the first closure, then the
    // second closure's `want` return is CONCRETE and its mismatching body is rejected by the
    // concrete-return soundness check — instead of being silently dropped by only-bind-unbound `unify`.
    // On the branch before this fix both `chezzi check` succeeded and `chezzi run` aborted at runtime.
    entry_rejects(
        "fn pick[U](cond: bool, a: fn() -> U, b: fn() -> U) -> U:\n    if cond:\n        return a()\n    return b()\nfn main():\n    r := pick(false, fn(): 1, fn(): \"hello\")\n    print(r + 1)\n",
        "returns",
    );
    entry_rejects(
        "fn two[U](f: fn(int) -> U, g: fn(int) -> U) -> U:\n    return f(0)\nfn main():\n    r := two(fn(x): x * 2, fn(x): str(x))\n    print(r + 1)\n",
        "returns",
    );
    // Two closures that AGREE on the return-only param still ACCEPT (recovered, no false positive).
    entry_ok(
        "fn two[U](f: fn(int) -> U, g: fn(int) -> U) -> U:\n    return f(0)\nfn main():\n    r := two(fn(x): x * 2, fn(x): x + 1)\n    print(r + 1)\n",
    );
}

#[test]
fn rwshared_read_len_ok() {
    entry_ok(
        "import std.concurrency\nfn main():\n    r := RwShared({\"a\": 1})\n    print(r.read(fn(m): m.len()))\n",
    );
}

// Phase 1c — generic struct constructor: a `fn`-typed field's closure arg is
// re-inferred against the SUBSTITUTED field type, so its param binds concretely.
const MAPPED_DEFS: &str = "struct Mapped[I: Iterator[T], T, U]:\n    inner: I\n    f: fn(T) -> U\n    fn next(self) -> Option[U]:\n        match self.inner.next():\n            Some(x):\n                return Some(self.f(x))\n            None:\n                return None\n";

#[test]
fn closure_param_inferred_in_generic_struct_ctor_ok() {
    entry_ok(&format!(
        "{MAPPED_DEFS}fn main():\n    m := Mapped([1, 2, 3].iter(), fn(x): x * 2)\n    print(m.next())\n"
    ));
}

#[test]
fn closure_param_inferred_in_generic_struct_ctor_conflict_rejected() {
    entry_rejects(
        &format!(
            "{MAPPED_DEFS}fn main():\n    m := Mapped([1, 2, 3].iter(), fn(x): x.upper())\n    print(m.next())\n"
        ),
        "has no method 'upper'",
    );
}

// Phase 2a — a closure bound to a `fn`-typed `let`/`:=` annotation is inferred
// in checking-mode (source #1).
#[test]
fn closure_inferred_from_fn_typed_let_binding_ok() {
    entry_ok("fn main():\n    cb: fn(int) -> int = fn(x): x + 1\n    print(cb(2))\n");
}

#[test]
fn closure_inferred_from_fn_typed_let_binding_conflict_rejected() {
    entry_rejects(
        "fn main():\n    cb: fn(int) -> int = fn(x): x.upper()\n    print(cb(2))\n",
        "has no method 'upper'",
    );
}

// Phase 2b — struct fn-field assignment + fn-typed return position (source #1).
#[test]
fn closure_inferred_in_struct_fn_field_assignment_ok() {
    entry_ok(
        "struct Box:\n    cb: fn(int) -> int\nfn main():\n    b := Box(fn(x: int) -> int: x)\n    b.cb = fn(x): x + 1\n    print(b.cb(2))\n",
    );
}

#[test]
fn closure_inferred_in_struct_fn_field_assignment_conflict_rejected() {
    entry_rejects(
        "struct Box:\n    cb: fn(int) -> int\nfn main():\n    b := Box(fn(x: int) -> int: x)\n    b.cb = fn(x): x.upper()\n    print(b.cb(2))\n",
        "has no method 'upper'",
    );
}

#[test]
fn closure_inferred_in_fn_typed_return_ok() {
    entry_ok(
        "fn make() -> fn(int) -> int:\n    return fn(x): x + 1\nfn main():\n    f := make()\n    print(f(2))\n",
    );
}

#[test]
fn closure_inferred_in_fn_typed_return_conflict_rejected() {
    entry_rejects(
        "fn make() -> fn(int) -> int:\n    return fn(x): x.upper()\nfn main():\n    f := make()\n    print(f(2))\n",
        "has no method 'upper'",
    );
}

// Phase 3a — free-closure body inference (source #2: a match whose scrutinee is
// the BARE param). Pins the param, so the call site is checked.
#[test]
fn free_closure_match_variant_pins_param_call_site_rejects() {
    entry_rejects(
        "enum E:\n    A\n    B\ng := fn(x): match x:\n    E.A: \"a\"\n    E.B: \"b\"\nfn main(): print(g(5))\n",
        "found int",
    );
}

#[test]
fn free_closure_all_variants_match_accepts() {
    entry_ok(
        "enum E:\n    A\n    B\ng := fn(x): match x:\n    E.A: \"a\"\n    E.B: \"b\"\nfn main(): print(g(E.A))\n",
    );
}

#[test]
fn free_closure_concrete_nested_tuple_not_swept() {
    entry_ok(
        "enum E:\n    A\n    B\nfn main():\n    g := fn(x: (E, int)): match x:\n        (E.A, b): \"a\"\n        _: \"o\"\n    print(g((E.A, 1)))\n",
    );
}

// Phase 4a — a STRUCTURAL sub-pattern over an un-inferable (Unknown) element/payload
// is rejected at `bind_subpattern` (the trap class: matching a tuple/variant shape on
// a value whose type we can't pin). The bare-param match pins x to a tuple/Result/Option
// with Unknown elements, so the nested E.A is structural-over-Unknown → reject.
#[test]
fn nested_tuple_subpattern_over_unknown_rejected() {
    entry_rejects(
        "enum E:\n    A\n    B\ng := fn(x): match x:\n    (E.A, b): \"a\"\n    _: \"o\"\nfn main(): print(g((5, 9)))\n",
        "un-inferable type",
    );
}

#[test]
fn nested_ok_payload_subpattern_over_unknown_rejected() {
    entry_rejects(
        "enum E:\n    A\n    B\ng := fn(x): match x:\n    Ok(E.A): \"a\"\n    _: \"o\"\nfn main(): print(g(Ok(5)))\n",
        "un-inferable type",
    );
}

#[test]
fn nested_some_payload_subpattern_over_unknown_rejected() {
    entry_rejects(
        "enum E:\n    A\n    B\ng := fn(x): match x:\n    Some(E.A): \"a\"\n    _: \"o\"\nfn main(): print(g(Some(5)))\n",
        "un-inferable type",
    );
}

#[test]
fn nested_guarded_subpattern_over_unknown_rejected() {
    entry_rejects(
        "enum E:\n    A\n    B\ng := fn(x, c: bool): match x:\n    (E.A, b) if c: \"a\"\n    _: \"o\"\nfn main(): print(g((5, 9), true))\n",
        "un-inferable type",
    );
}

#[test]
fn nested_or_alt_subpattern_over_unknown_rejected() {
    entry_rejects(
        "enum E:\n    A\n    B\ng := fn(x): match x:\n    (E.A, b) | (E.B, b): \"a\"\n    _: \"o\"\nfn main(): print(g((5, 9)))\n",
        "un-inferable type",
    );
}

// Phase 4a — boundary: a concrete-element structural sub-pattern must NOT be swept up.
#[test]
fn concrete_nested_ok_subpattern_accepts() {
    entry_ok(
        "enum E:\n    A\n    B\nfn main():\n    g := fn(x: Result[E, str]): match x:\n        Ok(E.A): \"a\"\n        _: \"o\"\n    print(g(Ok(E.A)))\n",
    );
}

// Phase 4b — a residual-Unknown scrutinee (a tuple-element binding) with STRUCTURAL arms
// rejects (the top-level scrutinee arm goes through match_kind/reconstruct, not bind_subpattern).
#[test]
fn residual_unknown_top_level_structural_rejected() {
    // Unannotated `x`: source #2 pins it to `(Unknown, Unknown)` from the `(a, b)` arm, so the
    // tuple-element binding `a` is Unknown; the inner `match a:` is a top-level structural match on a
    // residual-Unknown scrutinee → reject (caught by match_kind/reconstruct, not bind_subpattern).
    entry_rejects(
        "enum E:\n    A\n    B\ng := fn(x): match x:\n    (a, b): match a:\n        E.A: \"a\"\n        E.B: \"b\"\nfn main(): print(g((E.A, 1)))\n",
        "un-inferable type",
    );
}

#[test]
fn residual_unknown_top_level_structural_annotated_accepts() {
    entry_ok(
        "enum E:\n    A\n    B\nfn classify(p: (E, int)) -> str:\n    a, b := p\n    return match a:\n        E.A: \"a\"\n        E.B: \"b\"\nfn main(): print(classify((E.A, 1)))\n",
    );
}

// Phase 4b — heterogeneous literal arms over a residual-Unknown scrutinee now reject (the first
// arm pins the literal kind; the `OpenScrutinee` accept-with-`_` path is gone).
#[test]
fn residual_unknown_hetero_literal_rejects() {
    entry_rejects(
        "g := fn(x): match x:\n    (a, b): match a:\n        1: \"x\"\n        \"b\": \"y\"\n        _: \"z\"\n    _: \"o\"\nfn main(): print(g((1, 2)))\n",
        "literal of type str cannot match scrutinee of type int",
    );
}

// Phase 5 — a free closure whose param NOTHING pins (no expected/slot type, no body match-
// scrutinee or unique member) must be annotated; it must not degrade to a runtime `Unknown`.
#[test]
fn free_closure_unresolved_param_arith_requires_annotation() {
    entry_rejects(
        "g := fn(x): x + 1\nfn main(): print(g(2))\n",
        "cannot infer type of parameter 'x'; add a type annotation",
    );
}

#[test]
fn free_closure_unresolved_param_print_requires_annotation() {
    entry_rejects(
        "g := fn(x): print(x)\nfn main(): g(2)\n",
        "cannot infer type of parameter 'x'; add a type annotation",
    );
}

#[test]
fn annotated_free_closure_arith_ok() {
    entry_ok("g := fn(x: int): x + 1\nfn main(): print(g(2))\n");
}

#[test]
fn annotated_free_closure_match_ok() {
    entry_ok(
        "enum E:\n    A\n    B\ng := fn(x: E): match x:\n    E.A: \"a\"\n    E.B: \"b\"\nfn main(): print(g(E.A))\n",
    );
}

// ===== Adversarial-review fixes (closure-param inference) =====

// BUG 1 — the free-closure match-scrutinee scan (source #2) must be scope-aware: a `match x:` whose
// scrutinee `x` is a SHADOWING binding introduced by an enclosing tuple/variant sub-pattern is NOT
// the closure param, so it must not pin the param. Here the param `x` is never used (the inner
// `match x:` reads the `(x, _)` tuple binding), so the param is genuinely un-inferable → annotate.
// Pre-fix: param wrongly pinned to `E` → spurious call-site reject "expected E, found str".
#[test]
fn free_closure_shadowing_match_does_not_pin_param() {
    let errs = check_entry(
        "enum E:\n    A\n    B\nfn foo() -> (E, int):\n    return (E.A, 1)\ng := fn(x): match foo():\n    (x, _): match x:\n        E.A: 1\n        E.B: 2\nfn main(): print(g(\"hello\"))\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("cannot infer type of parameter 'x'")),
        "expected the annotate-param error (param is un-inferable), got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("expected E")),
        "param must NOT be pinned to E from the shadowing inner match, got: {errs:?}"
    );
}

// BUG 2 — a closure literal passed to a `fn`-typed slot with MISMATCHED arity must keep its params
// silently `Unknown` (the call site's assignability check reports the single arity diagnostic); it
// must NOT also route through the free-closure scan and emit "cannot infer type of parameter".
#[test]
fn closure_arity_mismatch_single_diagnostic() {
    let errs = check_entry(
        "fn g(f: fn(int) -> int) -> int:\n    return f(0)\nfn main(): print(g(fn(a, b): a + b))\n",
    );
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("cannot infer type of parameter")),
        "arity mismatch must not emit the annotate-param error, got: {errs:?}"
    );
    assert!(
        !errs.is_empty(),
        "the call-site arity/assignability mismatch must still be reported, got: {errs:?}"
    );
}

// BUG 3 — source #3 (unique member access) must fire even when the only pinning use of the param is
// inside a string-interpolation fragment (`"{x.name}"`). Pre-fix the scan was opaque to `Str` so the
// param was reported un-inferable.
#[test]
fn free_closure_pinned_by_member_in_interpolation() {
    entry_ok(
        "struct P:\n    name: str\nf := fn(x): \"hello {x.name}\"\nfn main(): print(f(P(\"bob\")))\n",
    );
}

// BUG 4/6 — a non-closure return expression must keep using plain `infer` (not `infer_value`): a
// `return <void-call>` must report ONLY "function returns nothing", not ALSO the value-position
// nil-rejection.
#[test]
fn return_void_call_single_diagnostic() {
    let errs = check_entry("fn f():\n    return print(\"x\")\nfn main(): f()\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("function returns nothing")),
        "expected the 'function returns nothing' error, got: {errs:?}"
    );
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("returns no value (nil)")),
        "a return expr must not get the value-position nil rejection, got: {errs:?}"
    );
}

// BUG 5 — source #3 must scan only USER structs; the always-seeded `Builtin`-origin native structs
// (Match/Response/ProcResult) must never pin a param. `end` is a field of Match (Builtin) and of no
// user struct, so the param is un-inferable → annotate, NOT pinned to the unimported `Match`.
#[test]
fn free_closure_member_does_not_pin_builtin_struct() {
    let errs = check_entry("f := fn(s): s.end\nfn main(): print(f(5))\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("cannot infer type of parameter 's'")),
        "expected the annotate-param error, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("Match")),
        "param must NOT be pinned to the seeded Builtin struct Match, got: {errs:?}"
    );
}

// BUG 7 — an assignment lvalue target must not be inferred as an rvalue (read-side gates / receiver
// double-inference). A closure assigned to a bad field must report the field error ONCE.
#[test]
fn closure_assign_bad_field_single_error() {
    let errs = check_entry(
        "struct Box:\n    cb: fn(int) -> int\nfn main():\n    b := Box(fn(x: int) -> int: x)\n    b.nope = fn(x): x + 1\n",
    );
    let field_errs = errs
        .iter()
        .filter(|e| e.message.contains("no field 'nope'"))
        .count();
    assert_eq!(
        field_errs, 1,
        "the bad-field error must be reported exactly once, got: {errs:?}"
    );
}

// SOUNDNESS — a closure passed to a GENERIC `T` slot whose type param is bound ONLY by the closure
// itself unifies `T` to `fn(Unknown) -> Unknown`; the substituted expected param type is therefore
// `Unknown`. Binding the unannotated closure param to that `Unknown` silently (source #1) leaves the
// call site unchecked → check-passes-then-traps at runtime on both engines. An `Unknown` expected
// param type must NOT count as a pin: fall through to the body scan / annotation requirement.
#[test]
fn closure_param_under_generic_unknown_slot_requires_annotation() {
    let errs = check_entry(
        "fn store[T](x: T) -> T:\n    return x\nfn main():\n    f := store(fn(a): a + 1)\n    print(f(\"hello\"))\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("cannot infer type of parameter 'a'")),
        "an unannotated closure param under a generic Unknown slot must require annotation, got: {errs:?}"
    );
}

// The same generic slot WITH an annotated closure param is fine, and the resolved `fn(int) -> int`
// signature is propagated so a later mismatched call is rejected at the call site (no runtime trap).
#[test]
fn closure_param_under_generic_slot_annotated_propagates_to_call_site() {
    let errs = check_entry(
        "fn store[T](x: T) -> T:\n    return x\nfn main():\n    f := store(fn(a: int): a + 1)\n    print(f(\"hello\"))\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("expected int") || e.message.contains("found str")),
        "annotated closure param must propagate fn(int)->int so the str call is rejected, got: {errs:?}"
    );
}

// Source #3 (unique member) — the design's flagship builtin example: `x.upper()` is owned only by
// `str`, so a free closure `fn(x): x.upper()` pins `x: str` and runs. (Previously the scan saw only
// user structs, so this was wrongly reported un-inferable.)
#[test]
fn free_closure_pinned_by_unique_str_method() {
    entry_ok("f := fn(x): x.upper()\nfn main(): print(f(\"hi\"))\n");
}

// …and the resolved `fn(str) -> str` signature propagates: a non-str call is rejected at the call
// site (no permissive Unknown laundering).
#[test]
fn free_closure_str_pin_propagates_to_call_site() {
    entry_rejects(
        "f := fn(x): x.upper()\nfn main(): print(f(5))\n",
        "expected str",
    );
}

// Source #3 negative — a member shared by >1 type (`len` is on str/list/map/set/bytes) must NOT pin,
// EVEN when exactly one USER struct also has it: it is ambiguous → require an annotation. (Pre-fix
// the struct-only scan mis-pinned the param to the lone struct, wrongly rejecting a list call with
// "expected <Struct>".)
#[test]
fn free_closure_shared_member_does_not_pin() {
    let errs = check_entry(
        "struct Counter:\n    n: int\n    fn len(self) -> int:\n        return self.n\nf := fn(x): x.len()\nfn main(): print(f([1, 2, 3]))\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("cannot infer type of parameter 'x'")),
        "a member shared with builtins must be un-inferable, not pinned, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("expected Counter")),
        "param must NOT be mis-pinned to the lone struct, got: {errs:?}"
    );
}

// ===== first-class native (Rust-implemented) types: qualified / aliased module-member path =====
// These pin the ADDITIVE qualified-path surface (`concurrency.Shared[int]` etc.) that makes an
// import-gated native type behave like a `.chz` module type. The bare-after-import path is unchanged
// (covered by the existing concurrency/net/ffi/time tests).

#[test]
fn qualified_native_type_annotation_resolves() {
    // concurrency.Shared[int] in annotation position (whole-module import binds `concurrency`).
    entry_ok("import std.concurrency\nfn f(s: concurrency.Shared[int]):\n    print(s.get())\n");
    // aliased: `import std.concurrency as c` -> `c.Shared[int]`.
    entry_ok("import std.concurrency as c\nfn f(s: c.Shared[int]):\n    print(s.get())\n");
    // net.Socket / net.Listener (type-only, no ctor).
    entry_ok("import std.net\nfn f(s: net.Socket):\n    print(\"ok\")\n");
    entry_ok("import std.net\nfn f(s: net.Listener):\n    print(\"ok\")\n");
    // ffi width + ptr in annotation position.
    entry_ok("import std.ffi\nfn f(x: ffi.int32) -> ffi.ptr:\n    return ffi.null()\n");
    // NEGATIVE: qualified access with NO import still rejects (gate stays sound).
    entry_rejects(
        "fn f(s: concurrency.Shared[int]):\n    print(\"x\")\n",
        "unknown module 'concurrency'",
    );
    // NEGATIVE: timer is a FUNCTION, not a type — reject in type position.
    entry_rejects(
        "import std.time\nfn f(x: time.timer):\n    print(\"x\")\n",
        "'timer' is a function, not a type",
    );
    // NEGATIVE: wrong arity on a qualified generic box.
    entry_rejects(
        "import std.concurrency\nfn f(s: concurrency.Shared):\n    print(\"x\")\n",
        "expects 1 type argument",
    );
}

#[test]
fn qualified_native_ctor_call_infers() {
    // concurrency.Shared(0) / aliased c.Shared(0) construct + method-call.
    entry_ok(
        "import std.concurrency\nfn main():\n    s := concurrency.Shared(0)\n    print(s.get())\nmain()\n",
    );
    entry_ok(
        "import std.concurrency as c\nfn main():\n    s := c.Shared(0)\n    print(s.get())\nmain()\n",
    );
    entry_ok(
        "import std.concurrency\nfn main():\n    r := concurrency.RwShared(0)\n    print(r.read(fn(x): x))\nmain()\n",
    );
    entry_ok(
        "import std.concurrency\nfn main():\n    a := concurrency.Atomic(0)\n    print(a.load())\nmain()\n",
    );
    entry_ok(
        "import std.concurrency\nfn main():\n    ex := concurrency.Executor()\n    ex.shutdown()\nmain()\n",
    );
    // time.timer(ms) -> Channel[bool].
    entry_ok("import std.time\nfn main():\n    t := time.timer(0)\n    print(t.recv())\nmain()\n");
    // NEGATIVE: net.Socket(...) has no from-nothing constructor.
    entry_rejects(
        "import std.net\nfn main():\n    s := net.Socket()\n    print(\"x\")\nmain()\n",
        "has no constructor",
    );
}

#[test]
fn alias_and_newtype_over_qualified_builtin() {
    // type alias over a qualified builtin.
    entry_ok(
        "import std.concurrency\ntype S = concurrency.Shared[int]\nfn f(s: S):\n    print(s.get())\n",
    );
    // newtype over a qualified builtin (generic).
    entry_ok(
        "import std.concurrency\nnewtype MyS[T] = concurrency.Shared[T]\nfn main():\n    print(\"ok\")\nmain()\n",
    );
}

#[test]
fn ffi_qualified_width_in_extern_sig() {
    // A qualified FFI width name inside an extern signature must type-check (resolve_ctype_d must map
    // ffi.int32 -> CType::Int32 so the marshal gate accepts it).
    entry_ok(
        "import std.ffi\nextern \"libc.so.6\":\n    fn abs(x: ffi.int32) -> ffi.int32\n\nfn main():\n    print(abs(-5))\nmain()\n",
    );
}

// ===== Swift-style keyword arguments through a function VALUE (SE-0111 labels, surface-only) =====

/// Labels on a `fn(...)` TYPE are SURFACE-ONLY: `fn(str)->nil` and `fn(name:str)->nil` are the same
/// type, mutually assignable (a labelled fn value flows into an unlabelled param and vice-versa).
#[test]
fn kw_labels_are_surface_only() {
    // A labelled user-fn value flows into an UNLABELLED fn param.
    ok(
        "fn use_it(f: fn(str) -> nil):\n    f(\"a\")\nfn greet(name: str):\n    print(name)\nuse_it(greet)\n",
    );
    // A closure passed to a LABELLED fn param (labels ignored in the arity/assignability check).
    ok("fn use_it(f: fn(name: str) -> nil):\n    f(\"a\")\nuse_it(fn(s: str): print(s))\n");
    entry_ok(
        "fn use_it(f: fn(str) -> nil):\n    f(\"a\")\nfn greet(name: str):\n    print(name)\nfn main():\n    use_it(greet)\nmain()\n",
    );
}

/// A value call carrying keyword arguments resolves against the value's labels (both by-name and
/// reordered), through both the single-module and multi-module check paths.
#[test]
fn kw_value_call_accepts() {
    ok(
        "fn greet(name: str, greeting: str):\n    print(greeting, name)\ng := greet\ng(name=\"Bob\", greeting=\"Hi\")\n",
    );
    // Reordered keywords.
    ok(
        "fn greet(name: str, greeting: str):\n    print(greeting, name)\ng := greet\ng(greeting=\"Hi\", name=\"Bob\")\n",
    );
    // Positional still works alongside (named empty → fast path).
    ok(
        "fn greet(name: str, greeting: str):\n    print(greeting, name)\ng := greet\ng(\"Bob\", \"Hi\")\n",
    );
    entry_ok(
        "fn greet(name: str, greeting: str):\n    print(greeting, name)\nfn main():\n    g := greet\n    g(name=\"Bob\", greeting=\"Hi\")\nmain()\n",
    );
}

/// A keyword argument through a HOF parameter resolves against the ANNOTATION's labels.
#[test]
fn kw_value_call_hof_param_labels() {
    ok(
        "fn apply(f: fn(name: str) -> nil):\n    f(name=\"X\")\napply(fn(name: str): print(name))\n",
    );
    entry_ok(
        "fn apply(f: fn(name: str) -> nil):\n    f(name=\"X\")\nfn main():\n    apply(fn(name: str): print(name))\nmain()\n",
    );
}

/// An unknown label through a value is a clean type error that NAMES the bad label.
#[test]
fn kw_value_unknown_label_rejected() {
    rejects(
        "fn greet(name: str):\n    print(name)\ng := greet\ng(nope=\"x\")\n",
        "unknown parameter label 'nope'",
    );
    entry_rejects(
        "fn greet(name: str):\n    print(name)\nfn main():\n    g := greet\n    g(nope=\"x\")\nmain()\n",
        "unknown parameter label 'nope'",
    );
}

/// SCOPE-CUT: a value call omitting a defaulted parameter is a TYPE ERROR (defaults do NOT fill
/// through a value), while a DIRECT call still fills the default.
#[test]
fn kw_value_scope_cut_defaults() {
    // Direct call fills the default (a desugar feature) — clean.
    ok_desugared("fn hasdefault(x: int = 5):\n    print(x)\nhasdefault()\n");
    // Through a value: every parameter must be supplied (defaults do NOT fill through a value).
    rejects_desugared(
        "fn hasdefault(x: int = 5):\n    print(x)\nh := hasdefault\nh()\n",
        "argument",
    );
}

/// A first-class BUILTIN function value takes NO keyword arguments (labels are a user-fn surface).
#[test]
fn kw_value_builtin_rejects_keywords() {
    rejects("p := ord\np(x=\"a\")\n", "takes no keyword arguments");
}

// ===== Variadic parameters + `Any` top type (M-variadic) =====

/// `Any` is an empty structural protocol → every type (scalars included) satisfies it.
#[test]
fn any_top_type_accepts_scalars() {
    ok("fn w():\n    x: Any = 1\n    y: Any = \"s\"\n    z: Any = true\n    f: Any = 1.5\n");
}

/// `Any` as a param type accepts an int and a struct value.
#[test]
fn any_param_accepts_anything() {
    entry_ok(
        "struct P:\n    x: int\n\nfn g(v: Any) -> nil:\n    return\n\nfn main():\n    g(1)\n    g(P(x=1))\n    g(\"hi\")\n",
    );
}

/// A user cannot redeclare the reserved `Any` protocol.
#[test]
fn any_is_reserved() {
    rejects("protocol Any:\n    fn foo(self) -> int\n", "reserved");
}

/// A direct `satisfies` unit check: every scalar satisfies the empty `Any` protocol.
#[test]
fn any_satisfied_by_all_scalars_unit() {
    let c = Checker::new();
    for ty in [Ty::Int, Ty::Float, Ty::Bool, Ty::Str, Ty::Nil] {
        assert!(
            c.satisfies_args(&ty, "Any", &[]).is_ok(),
            "{ty} should satisfy Any"
        );
    }
}

/// `fn_sig` collapses a variadic param to a `List[T]` slot and records its index.
#[test]
fn fn_sig_variadic_collapses_to_list() {
    let module =
        parser::parse(lexer::tokenize("fn f(...xs: int) -> int:\n    return 0\n").unwrap())
            .unwrap();
    let StmtKind::Fn(decl) = &module.stmts[0].kind else {
        panic!("expected fn");
    };
    let mut c = Checker::new();
    let sig = c.fn_sig(decl, Span::default());
    assert_eq!(sig.params, vec![Ty::List(Box::new(Ty::Int))]);
    assert_eq!(sig.variadic, Some(0));
}

/// `fn_sig` keeps pre-variadic + post-variadic (keyword-only) slots around the collapsed list.
#[test]
fn fn_sig_variadic_middle_slot() {
    let module = parser::parse(
        lexer::tokenize("fn f(a: str, ...xs: int, flag: bool = false) -> int:\n    return 0\n")
            .unwrap(),
    )
    .unwrap();
    let StmtKind::Fn(decl) = &module.stmts[0].kind else {
        panic!("expected fn");
    };
    let mut c = Checker::new();
    let sig = c.fn_sig(decl, Span::default());
    assert_eq!(
        sig.params,
        vec![Ty::Str, Ty::List(Box::new(Ty::Int)), Ty::Bool]
    );
    assert_eq!(sig.variadic, Some(1));
}

/// A variadic user fn collapses its call to a `List[T]` slot and type-checks end-to-end (desugared).
#[test]
fn variadic_call_typechecks_desugared() {
    let errs = check_desugared(
        "fn sum_all(...xs: int) -> int:\n    total := 0\n    for x in xs:\n        total = total + x\n    return total\n\nfn main():\n    a := sum_all(1, 2, 3)\n    b := sum_all()\n",
    );
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

/// A heterogeneous variadic arg (`bool` into `...xs: int`) is a compile error (via the collapsed
/// `List` literal's element-type check).
#[test]
fn variadic_element_type_enforced() {
    let errs = check_desugared("fn f(...xs: int):\n    return\n\nfn main():\n    f(1, true)\n");
    assert!(
        !errs.is_empty(),
        "expected a type error for bool into ...xs: int"
    );
}

/// A variadic typed with the `Any` top type accepts HETEROGENEOUS arguments — the flagship reason
/// `Any` is the "honest element type". `f(1, "a", true)` collapses to a `List[Any]` whose every
/// element vacuously satisfies the empty `Any` protocol, so the synthesized list literal must be
/// checked against the declared `List[Any]` slot (expected-type-directed), NOT unified bottom-up.
/// (Regression: it previously failed with "list elements differ: int vs str".)
#[test]
fn variadic_any_accepts_heterogeneous() {
    let errs = check_desugared(
        "fn describe(...xs: Any) -> int:\n    return xs.len()\n\nfn main():\n    n := describe(1, \"a\", true)\n",
    );
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

/// The same expected-type-directed rescue for an EXPLICIT `List[Any]` annotation binding a
/// heterogeneous list literal (the non-variadic surface of the same fix).
#[test]
fn list_any_annotation_accepts_heterogeneous() {
    let errs =
        check_desugared("fn main():\n    xs: List[Any] = [1, \"a\", true]\n    n := xs.len()\n");
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

/// The same expected-type-directed path generalizes beyond `Any`: a `List[Shape]` annotation accepts
/// a literal of DIFFERING concrete types when each satisfies the declared protocol element type
/// (sound — each element is genuinely assignable to `Shape`).
#[test]
fn list_protocol_annotation_accepts_mixed_concrete() {
    let errs = check_desugared(
        "protocol Shape:\n    fn area(self) -> int\n\nstruct Circle:\n    r: int\n    fn area(self) -> int:\n        return self.r\n\nstruct Square:\n    s: int\n    fn area(self) -> int:\n        return self.s\n\nfn main():\n    shapes: List[Shape] = [Circle(r=2), Square(s=3)]\n    n := shapes.len()\n",
    );
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

/// A heterogeneous literal into a NON-top concrete element slot (`List[int]`) still errors — the
/// expected-type path only rescues when EVERY element is assignable to the declared element type.
#[test]
fn list_int_annotation_still_rejects_heterogeneous() {
    let errs = check_desugared("fn main():\n    xs: List[int] = [1, \"a\"]\n");
    assert!(
        !errs.is_empty(),
        "expected a type error for str into List[int]"
    );
}

// ===== print ported to a variadic `native fn` decl =====

#[test]
fn print_zero_args_ok() {
    ok("fn w():\n    print()\n");
}

#[test]
fn print_variadic_with_sep_end_ok() {
    ok("fn w():\n    print(1, \"a\", true, sep=\"-\", end=\"!\")\n");
}

#[test]
fn print_sep_non_str_rejects() {
    rejects("fn w():\n    print(1, sep=5)\n", "str");
}

#[test]
fn print_builtin_sig_present_after_port() {
    // print retains a builtin_sig (hover) after retiring the synthetic sig_print.
    let mut c = Checker::new();
    c.seed_native_prelude_sigs();
    assert!(c.builtin_sig("print").is_some());
}

/// KNOWN v1 LIMIT (pinned): a variadic call used as a parameter DEFAULT is not collapsed (the desugar
/// collapse runs on pass 1 only; a default is spliced after pass 1), so it is a compile error — the
/// same on both engines, not a parity divergence. Documented in PROGRESS.md; wrap in a fixed-arity
/// helper. This test locks the behavior so a future collapse-idempotency fix updates it deliberately.
#[test]
fn variadic_call_as_param_default_is_compile_error() {
    let errs = check_desugared(
        "fn sum_all(...xs: int) -> int:\n    return 0\n\nfn g(x: int = sum_all(1, 2, 3)) -> int:\n    return x\n\nfn main():\n    print(g())\n",
    );
    assert!(
        !errs.is_empty(),
        "a variadic call as a param default is a known-limit compile error"
    );
}

// ===== variadic METHODS under same-name collisions across structs (regression) =====

/// A variadic method call `recv.m(a,b,c)` must collapse the surplus positionals even when ANOTHER
/// struct defines `m` with a different (non-variadic) param list. The receiver's struct type is
/// knowable pre-type (`a := A()`), so desugar resolves the exact variadic spec and collapses.
#[test]
fn variadic_method_positional_with_name_collision() {
    let errs = check_desugared(
        "struct A:\n    fn m(self, ...xs: int) -> int:\n        return xs.len()\n\nstruct B:\n    fn m(self, x: int) -> int:\n        return x\n\nfn main():\n    a := A()\n    print(a.m(1, 2, 3))\n",
    );
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

/// Same collision, but the receiver is a TYPED PARAMETER (`x: A`) rather than a let-bound local —
/// the desugar pass must still resolve the receiver's struct type to collapse the variadic call.
#[test]
fn variadic_method_typed_param_receiver_with_collision() {
    let errs = check_desugared(
        "struct A:\n    fn m(self, ...xs: int) -> int:\n        return xs.len()\n\nstruct B:\n    fn m(self, a: int, b: int) -> int:\n        return a + b\n\nfn use_a(x: A) -> int:\n    return x.m(1, 2, 3)\n\nfn main():\n    print(use_a(A()))\n",
    );
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

/// Two BOTH-variadic methods that differ only in the variadic param's NAME (`...xs` vs `...ys`) —
/// the name-keyed method table sees a disagreement, but the receiver-aware resolution picks the
/// right sibling and collapses.
#[test]
fn variadic_method_both_variadic_differ_by_param_name() {
    let errs = check_desugared(
        "struct A:\n    fn m(self, ...xs: int) -> int:\n        return xs.len()\n\nstruct B:\n    fn m(self, ...ys: int) -> int:\n        return ys.len() * 2\n\nfn main():\n    a := A()\n    print(a.m(1, 2, 3))\n",
    );
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

/// A keyword-only post-variadic param on a name-colliding method: `a.m(1, 2, flag=true)` fills the
/// variadic positionally and the keyword-only slot by name. Previously the collision guard emitted
/// an unsatisfiable "pass arguments positionally" error for a keyword-ONLY parameter.
#[test]
fn variadic_method_keyword_only_with_collision() {
    let errs = check_desugared(
        "struct A:\n    fn m(self, ...xs: int, flag: bool) -> int:\n        if flag:\n            return xs.len()\n        return 0\n\nstruct B:\n    fn m(self, x: int) -> int:\n        return x\n\nfn main():\n    a := A()\n    print(a.m(1, 2, flag=true))\n",
    );
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

// ===== Generic fn as a VALUE (scope A + B): pin type params from a known concrete fn type or an
// explicit turbofish; runtime is generic-ERASED. A bare un-pinned generic fn value stays an error.

#[test]
fn generic_fn_value_turbofish_ok() {
    // B — turbofish on a fn value: `g := ident[int]` → g : fn(int) -> int
    ok(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    g := ident[int]\n    print(g(5) + 1)\n",
    );
}

#[test]
fn generic_fn_value_annot_ok() {
    // A1 — annotated binding pins T=int from the annotation.
    ok(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    g: fn(int) -> int = ident\n    print(g(5) + 1)\n",
    );
}

#[test]
fn generic_fn_value_hofarg_ok() {
    // A2 — HOF argument pins T against the param type.
    ok(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn applyit(f: fn(int) -> int, x: int) -> int:\n    return f(x)\n\nfn main():\n    print(applyit(ident, 5) + 1)\n",
    );
}

#[test]
fn generic_fn_value_return_ok() {
    // A3 — return position pins T against the declared return type.
    ok(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn getf() -> fn(int) -> int:\n    return ident\n\nfn main():\n    g := getf()\n    print(g(5) + 1)\n",
    );
}

// ---- soundness rejects ----

#[test]
fn generic_fn_value_unsatisfiable_rejected() {
    // ident is fn(T)->T; can't be both str and int → concrete subst is fn(str)->str, rejected vs fn(str)->int.
    rejects(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    g: fn(str) -> int = ident\n",
        "cannot assign fn(str) -> str to variable of type fn(str) -> int",
    );
}

#[test]
fn generic_fn_value_turbofish_bound_violation_rejected() {
    rejects(
        "fn addone[T: Add](x: T) -> T:\n    return x + x\n\nfn main():\n    h := addone[str]\n    print(h(\"a\"))\n",
        "Add",
    );
}

#[test]
fn generic_fn_value_annot_bound_violation_rejected() {
    rejects(
        "fn addone[T: Add](x: T) -> T:\n    return x + x\n\nfn main():\n    h: fn(str) -> str = addone\n",
        "Add",
    );
}

#[test]
fn generic_fn_value_turbofish_arity_mismatch_rejected() {
    rejects(
        "fn pair[A, B](a: A, b: B) -> A:\n    return a\n\nfn main():\n    p := pair[int]\n",
        "expects 2 type argument(s), found 1",
    );
}

#[test]
fn generic_fn_value_downstream_misuse_rejected() {
    // g := ident[int]; g(5) is int, not str.
    rejects(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    g := ident[int]\n    s: str = g(5)\n",
        "cannot assign int to variable of type str",
    );
}

// ---- must-not-regress ----

#[test]
fn bare_unpinned_generic_fn_value_stays_error() {
    // OUT OF SCOPE (scope C) — a bare generic-fn value with no expected type + no turbofish, then called.
    let errs = check_src(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    g := ident\n    print(g(5))\n",
    );
    assert!(
        !errs.is_empty(),
        "bare un-pinned generic fn value must stay an error"
    );
}

#[test]
fn generic_fn_direct_call_unchanged() {
    ok("fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    print(ident(5) + 1)\n");
}

#[test]
fn generic_fn_call_site_turbofish_unchanged() {
    ok("fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    print(ident[int](5) + 1)\n");
}

#[test]
fn nongeneric_fn_indexed_rejected() {
    // A NON-generic fn indexed is NOT a turbofish — checker must reject (the compiler-erase relies on
    // this: only a checker-accepted generic-fn turbofish ever reaches the erase path).
    rejects(
        "fn g(x: int) -> int:\n    return x\n\nfn main():\n    y := g[0]\n",
        "cannot index into fn(int) -> int",
    );
}

#[test]
fn local_shadowing_generic_fn_name_indexes_normally() {
    // A local/param that shadows a top-level generic fn name is a REAL index, not an erased turbofish.
    ok(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn h():\n    ident := [10, 20, 30]\n    print(ident[1])\n",
    );
    ok("fn ident[T](x: T) -> T:\n    return x\n\nfn h(ident: List[int]):\n    print(ident[0])\n");
}

// ---- builtin-HOF slot pins a bare same-module generic fn's own [T] (scope A through .map/.fold) ----

#[test]
fn generic_fn_value_into_builtin_map_return_concrete_ok() {
    // `.map`'s slot fn(int) -> U pins conv's own T=int; the map result is List[str].
    ok(
        "fn conv[T](x: T) -> str:\n    return str(x)\n\nfn main():\n    xs := [1, 2, 3].map(conv)\n    s: str = xs[0]\n    print(s)\n",
    );
}

#[test]
fn generic_fn_value_into_builtin_map_return_only_ok() {
    // ident's return-only T is pinned = int from the .map element type; result is List[int].
    ok(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    xs := [1, 2, 3].map(ident)\n    n: int = xs[0]\n    print(n + 1)\n",
    );
}

#[test]
fn generic_fn_value_into_builtin_fold_ok() {
    // .fold(0, add): the accumulator U is pinned by `init`=int FIRST, then add's own T=int is pinned
    // from the resulting fn(int, int) -> int slot; the bound [T: Add] is enforced under the pin.
    ok(
        "fn add[T: Add](a: T, b: T) -> T:\n    return a + b\n\nfn main():\n    n: int = [1, 2, 3].fold(0, add)\n    print(n)\n",
    );
}

#[test]
fn generic_fn_value_into_builtin_filter_still_ok() {
    // .filter's slot is fully concrete fn(int) -> bool; a bare generic keep[T] still pins T=int.
    ok(
        "fn keep[T](x: T) -> bool:\n    return true\n\nfn main():\n    xs := [1, 2, 3].filter(keep)\n    print(xs)\n",
    );
}

#[test]
fn generic_fn_value_two_distinct_pins_no_launder_ok() {
    // Same conv pinned once into .map (str-returning slot) and once into a user HOF with a
    // fn(int) -> str param; each call site gets a FRESH substitution map so neither launders.
    ok(concat!(
        "fn conv[T](x: T) -> str:\n    return str(x)\n",
        "\nfn use2(f: fn(int) -> str) -> str:\n    return f(9)\n",
        "\nfn main():\n    a := [1, 2, 3].map(conv)\n    sa: str = a[0]\n    b := use2(conv)\n    print(sa)\n    print(b)\n",
    ));
}

#[test]
fn generic_fn_value_into_map_bound_violation_rejected() {
    // A [T: Comparable]-bounded fn pinned to a non-Comparable element type must reject via
    // enforce_bounds under the pin (the helper mirrors Scope A's bound enforcement).
    rejects(
        "struct Tag:\n    n: int\n\nfn cmp[T: Comparable](x: T) -> T:\n    return x\n\nfn main():\n    xs := [Tag(1), Tag(2)].map(cmp)\n    print(xs)\n",
        "Comparable",
    );
}

#[test]
fn generic_fn_value_map_closure_still_infers_ok() {
    // Regression: the unannotated-closure loop-back is untouched — still infers List[int].
    ok("fn main():\n    xs := [1, 2, 3].map(fn(x): x * 2)\n    n: int = xs[0]\n    print(n)\n");
}

// ---- arity guard: a generic method called with TOO FEW args reports cleanly, never panics ----
// (adversarial-review bugs 1 & 2: the pass-1 pin loop must clamp to `arg_tys.len()`, not index
// `expected.len()` out of bounds when the arity check reports-but-does-not-return.)

#[test]
fn generic_method_too_few_args_reports_not_panics_fold() {
    // `.fold` expects (init, f) — passing only `init` must report the arity error, not index-panic.
    rejects(
        "fn main():\n    print([1, 2, 3].fold(0))\n",
        "'fold' expects 2 argument(s), got 1",
    );
}

#[test]
fn generic_method_too_few_args_reports_not_panics_map() {
    // `.map` expects (f) — passing none must report the arity error, not index-panic.
    rejects(
        "fn main():\n    print([1, 2, 3].map())\n",
        "'map' expects 1 argument(s), got 0",
    );
}

#[test]
fn generic_user_method_too_few_args_reports_not_panics() {
    // A user generic method routed through infer_generic_method with too few args reports cleanly.
    rejects(
        "struct B[T]:\n    v: T\n    fn map_to[U](self, f: fn(T) -> U) -> U:\n        return f(self.v)\nfn main():\n    b := B(1)\n    print(b.map_to())\n",
        "'map_to' expects 1 argument(s), got 0",
    );
}

// ── Parameterized protocols in value/annotation position (Q1) ───────────────────────────────

/// A `Container[int]` param annotation is ACCEPTED (was a hard "can only be used as a bound" error);
/// a struct whose `get` returns `int` conforms and the call type-checks.
#[test]
fn param_parameterized_protocol_accepts() {
    ok(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    fn get(self, i: int) -> int:\n        return 7\nfn f(c: Container[int]) -> int:\n    return c.get(0)\nfn main():\n    print(f(Bag()))\n",
    );
}

/// A `Container[str]` slot must REJECT an int-returning Bag — the carried arg is witnessed at the
/// call boundary (no over-accept), so the value fails the parameter's assignability check.
#[test]
fn param_protocol_wrong_arg_rejected() {
    rejects(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    fn get(self, i: int) -> int:\n        return 7\nfn f(c: Container[str]) -> int:\n    return 0\nfn main():\n    f(Bag())\n",
        "expected Container[str], found Bag",
    );
}

/// A bare NON-GENERIC protocol existential still ACCEPTS a conforming struct (0-arg existential
/// preserved — the `Error`/`Show` case, unchanged by the parameterization work).
#[test]
fn bare_protocol_still_accepts() {
    ok(
        "protocol Show:\n    fn show(self) -> str\nstruct Bag:\n    fn show(self) -> str:\n        return \"bag\"\nfn f(c: Show) -> str:\n    return c.show()\nfn main():\n    print(f(Bag()))\n",
    );
}

/// A bare `Container` value is NOT assignable into a `Container[int]` slot (strict invariance / arity).
#[test]
fn bare_not_equal_parameterized_reject_into_param() {
    rejects(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    fn get(self, i: int) -> int:\n        return 7\nfn g(c: Container) -> Container[int]:\n    return c\nfn main():\n    pass\n",
        "expected",
    );
}

/// A `Container[int]` value is NOT assignable into a bare `Container` slot (strict invariance / arity).
#[test]
fn parameterized_not_equal_bare_reject_into_bare() {
    rejects(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    fn get(self, i: int) -> int:\n        return 7\nfn g(c: Container[int]) -> Container:\n    return c\nfn main():\n    pass\n",
        "expected",
    );
}

/// Method-return element RECOVERY: `c: Container[int]; c.get(0)` infers to `int`, usable in int arithmetic.
#[test]
fn param_protocol_method_return_recovered() {
    ok(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    fn get(self, i: int) -> int:\n        return 7\nfn f(c: Container[int]) -> int:\n    x := c.get(0)\n    return x + 1\nfn main():\n    print(f(Bag()))\n",
    );
}

/// Write-site (a): a `-> Container[int]` return with a str-returning value REJECTS.
#[test]
fn param_protocol_return_site_rejects() {
    rejects(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct StrBag:\n    fn get(self, i: int) -> str:\n        return \"x\"\nfn g() -> Container[int]:\n    return StrBag()\nfn main():\n    pass\n",
        "expected return type Container[int], found StrBag",
    );
}

/// Write-site (b): a `struct S: c: Container[int]` field initialised with a str-returning value REJECTS.
#[test]
fn param_protocol_field_site_rejects() {
    rejects(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct StrBag:\n    fn get(self, i: int) -> str:\n        return \"x\"\nstruct S:\n    c: Container[int]\nfn main():\n    S(StrBag())\n",
        "expected Container[int], found StrBag",
    );
}

/// Write-site (c): reassigning a str-returning value into a `Container[int]` local REJECTS.
#[test]
fn param_protocol_reassign_site_rejects() {
    rejects(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct IntBag:\n    fn get(self, i: int) -> int:\n        return 1\nstruct StrBag:\n    fn get(self, i: int) -> str:\n        return \"x\"\nfn f(c: Container[int]):\n    c = StrBag()\nfn main():\n    f(IntBag())\n",
        "cannot assign StrBag to Container[int]",
    );
}

/// Nesting: `List[Container[int]]` threads the args into the nested Protocol; a wrong-arg nested variant REJECTS.
#[test]
fn param_protocol_nesting_accepts_and_wrong_rejects() {
    ok(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    fn get(self, i: int) -> int:\n        return 7\nfn h(xs: List[Container[int]]) -> int:\n    return xs[0].get(0)\nfn main():\n    print(h([Bag()]))\n",
    );
    rejects(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    fn get(self, i: int) -> int:\n        return 7\nfn h(xs: List[Container[str]]) -> int:\n    return 0\nfn main():\n    h([Bag()])\n",
        "expected List[Container[str]], found List[Bag]",
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// MUTABLE-CONTAINER GENERIC INVARIANCE (soundness hole): a `G[Sub]` value must
// NOT be assignable where `G[Super]` is expected for a MUTABLE, by-reference
// container (List/Set/Map, user generic struct). The reverse was already
// rejected; these lock the covariant direction shut. All use the REAL graph
// helpers (entry_ok/entry_rejects), NOT single-module check_src.
// ─────────────────────────────────────────────────────────────────────────────

/// Repro A (was check-ok → runtime trap): passing a `List[Cat]` VARIABLE where
/// `List[Any]` is expected must REJECT (the callee can `.push` a non-Cat).
#[test]
fn invariance_rejects_list_covariance_launder() {
    entry_rejects(
        "struct Cat:\n    name: str\nfn poison(xs: List[Any]):\n    xs.push(123)\nfn main():\n    cats: List[Cat] = [Cat(\"felix\")]\n    poison(cats)\n    for c in cats:\n        print(c.name)\nmain()\n",
        "expected List[Any], found List[Cat]",
    );
}

/// Map VALUE type is invariant: a `Map[str, Cat]` VARIABLE must NOT flow into a
/// `Map[str, Any]` slot (the callee can insert a non-Cat value).
#[test]
fn invariance_rejects_map_value_covariance() {
    entry_rejects(
        "struct Cat:\n    name: str\nfn stash(m: Map[str, Any]):\n    m[\"x\"] = 123\nfn main():\n    cats: Map[str, Cat] = {\"felix\": Cat(\"felix\")}\n    stash(cats)\nmain()\n",
        "expected Map[str, Any], found Map[str, Cat]",
    );
}

/// User generic struct field via a mutator `set(self, x: T)` — `Box[Cat]` var
/// must NOT pass where `Box[Any]` is expected. (Any top-type variant.)
#[test]
fn invariance_rejects_user_generic_struct_covariance_any() {
    entry_rejects(
        "struct Cat:\n    name: str\nstruct Box[T]:\n    v: T\n    fn set(self, x: T):\n        self.v = x\nfn f(b: Box[Any]):\n    b.set(123)\nfn main():\n    bc: Box[Cat] = Box(Cat(\"felix\"))\n    f(bc)\nmain()\n",
        "expected Box[Any], found Box[Cat]",
    );
}

/// Same, but the supertype is a USER protocol (Cat <: Speaker), not `Any`.
#[test]
fn invariance_rejects_user_generic_struct_covariance_protocol() {
    entry_rejects(
        "protocol Speaker:\n    fn speak(self) -> str\nstruct Cat:\n    name: str\n    fn speak(self) -> str:\n        return \"meow\"\nstruct Dog:\n    fn speak(self) -> str:\n        return \"woof\"\nstruct Box[T]:\n    v: T\n    fn set(self, x: T):\n        self.v = x\nfn f(b: Box[Speaker]):\n    b.set(Dog())\nfn main():\n    bc: Box[Cat] = Box(Cat(\"felix\"))\n    f(bc)\nmain()\n",
        "expected Box[Speaker], found Box[Cat]",
    );
}

/// Assignment boundary (not a fn-arg): `b: List[Any] = a` where `a: List[Cat]`
/// must REJECT — the fix covers plain let/`:=` sinks, not just call args.
#[test]
fn invariance_rejects_let_assign_container_covariance() {
    entry_rejects(
        "struct Cat:\n    name: str\nfn main():\n    a: List[Cat] = [Cat(\"felix\")]\n    b: List[Any] = a\n    print(b)\nmain()\n",
        "cannot assign List[Cat] to variable of type List[Any]",
    );
}

/// BOUNDARY GUARD — legitimate container neighbors that MUST stay check-clean
/// (no over-rejection). Same-arg containers, nested equal generics, and a
/// literal `List[Any]` built and used as `List[Any]`.
#[test]
fn invariance_preserves_legit_container_neighbors() {
    // List[int] -> List[int]
    entry_ok(
        "fn f(xs: List[int]) -> int:\n    return xs[0]\nfn main():\n    ys: List[int] = [1, 2]\n    print(f(ys))\nmain()\n",
    );
    // List[Cat] -> List[Cat]
    entry_ok(
        "struct Cat:\n    name: str\nfn f(xs: List[Cat]) -> str:\n    return xs[0].name\nfn main():\n    cs: List[Cat] = [Cat(\"felix\")]\n    print(f(cs))\nmain()\n",
    );
    // Map[str, int] -> Map[str, int]
    entry_ok(
        "fn f(m: Map[str, int]) -> int:\n    return m[\"a\"]\nfn main():\n    d: Map[str, int] = {\"a\": 1}\n    print(f(d))\nmain()\n",
    );
    // Box[Cat] -> Box[Cat]
    entry_ok(
        "struct Cat:\n    name: str\nstruct Box[T]:\n    v: T\nfn f(b: Box[Cat]) -> str:\n    return b.v.name\nfn main():\n    bc: Box[Cat] = Box(Cat(\"felix\"))\n    print(f(bc))\nmain()\n",
    );
    // nested Map[str, List[int]] -> same
    entry_ok(
        "fn f(m: Map[str, List[int]]) -> int:\n    return m[\"a\"][0]\nfn main():\n    d: Map[str, List[int]] = {\"a\": [1, 2]}\n    print(f(d))\nmain()\n",
    );
    // a literal List[Any] built and used as List[Any]
    entry_ok(
        "fn f(xs: List[Any]):\n    xs.push(1)\nfn main():\n    ys: List[Any] = [1, \"a\", true]\n    f(ys)\nmain()\n",
    );
}

// ============================================================================
// FIX A — `Self` usable in struct/enum/newtype inherent-method signatures/bodies
// ============================================================================

#[test]
fn self_type_in_struct_method_sig() {
    // `-> Self` and a `Self` param resolve to the enclosing struct.
    entry_ok(
        "struct P:\n    x: int\n    fn dup(self) -> Self:\n        return self\n    fn add(self, o: Self) -> Self:\n        return P(self.x + o.x)\nfn main():\n    print(P(5).dup().x)\n    print(P(1).add(P(2)).x)\nmain()\n",
    );
}

#[test]
fn self_type_in_enum_method_sig() {
    // An enum method returning `Self` resolves to the enclosing enum.
    entry_ok(
        "enum Money:\n    Cents(int)\n    fn double(self) -> Self:\n        match self:\n            Money.Cents(c): return Money.Cents(c * 2)\nfn main():\n    m := Money.Cents(50).double()\n    match m:\n        Money.Cents(c): print(c)\nmain()\n",
    );
}

#[test]
fn self_type_in_newtype_method_sig() {
    // A newtype method returning `Self` resolves to the enclosing newtype.
    entry_ok(
        "newtype Meters = float:\n    fn twice(self) -> Self:\n        return Meters(float(self) * 2.0)\nfn main():\n    print(float(Meters(3.0).twice()))\nmain()\n",
    );
}

#[test]
fn self_type_rejected_outside_method() {
    // Free-fn param `Self` — no enclosing type → still unknown.
    entry_rejects(
        "fn f(x: Self) -> int:\n    return 0\nfn main():\n    pass\nmain()\n",
        "unknown type 'Self'",
    );
    // Struct field typed `Self` — a field is not a method sig → still unknown.
    entry_rejects(
        "struct P:\n    y: Self\nfn main():\n    pass\nmain()\n",
        "unknown type 'Self'",
    );
    // Top-level variable annotation `Self`.
    entry_rejects(
        "fn main():\n    x: Self = 0\nmain()\n",
        "unknown type 'Self'",
    );
}

#[test]
fn self_type_enforced_as_concrete_enclosing_type() {
    // `-> Self` is the concrete enclosing struct; returning a different struct is a type error.
    entry_rejects(
        "struct A:\n    x: int\nstruct B:\n    y: int\n    fn make(self) -> Self:\n        return A(1)\nfn main():\n    pass\nmain()\n",
        "return",
    );
}

#[test]
fn self_type_protocol_behavior_unchanged() {
    // A protocol method using `Self` still checks (its `Self` is `Ty::Param`, unchanged path).
    entry_ok(
        "protocol Dottable:\n    fn dot(self, o: Self) -> int\nstruct V:\n    x: int\n    fn dot(self, o: Self) -> int:\n        return self.x * o.x\nfn main():\n    print(V(2).dot(V(3)))\nmain()\n",
    );
}

#[test]
fn self_type_generic_struct_method() {
    // Generic struct: `-> Self` carries the struct's own type args, so `return self` type-checks.
    entry_ok(
        "struct Box[T]:\n    v: T\n    fn same(self) -> Self:\n        return self\nfn main():\n    b := Box(5).same()\n    print(b.v)\nmain()\n",
    );
}

// ============================================================================
// FIX B — compound assignment honors struct/enum/newtype operator overloading
// ============================================================================

#[test]
fn compound_assign_struct_overload() {
    // `a += V(10)` accepted exactly when `a = a + V(10)` is (V has an `add` overload).
    entry_ok(
        "struct V:\n    x: int\n    fn add(self, o: V) -> V:\n        return V(self.x + o.x)\n    fn str(self) -> str:\n        return \"V({self.x})\"\nfn main():\n    a := V(1)\n    a = a + V(10)\n    a += V(10)\n    print(a)\nmain()\n",
    );
}

#[test]
fn compound_assign_newtype_numeric() {
    // A numeric newtype supports `+=` via its underlying-numeric auto-flow.
    entry_ok(
        "newtype Meters = float\nfn main():\n    m := Meters(1.0)\n    m += Meters(2.0)\n    print(float(m))\nmain()\n",
    );
}

#[test]
fn compound_assign_enum_sub_overload() {
    // `-=` on an enum with a matching `sub` overload.
    entry_ok(
        "enum Cnt:\n    N(int)\n    fn amt(self) -> int:\n        match self:\n            Cnt.N(a): return a\n    fn sub(self, o: Cnt) -> Cnt:\n        return Cnt.N(self.amt() - o.amt())\nfn main():\n    c := Cnt.N(10)\n    c -= Cnt.N(3)\n    print(c.amt())\nmain()\n",
    );
}

#[test]
fn compound_assign_rejected_no_overload() {
    // Struct with no `add` → `a += W(..)` rejected (mirrors `a = a + W(..)` failing).
    entry_rejects(
        "struct W:\n    x: int\nfn main():\n    a := W(1)\n    a += W(2)\nmain()\n",
        "cannot apply +=",
    );
}

#[test]
fn compound_assign_rejected_heterogeneous() {
    // `V += int` where `V + int` is a type error → still rejected.
    entry_rejects(
        "struct V:\n    x: int\n    fn add(self, o: V) -> V:\n        return V(self.x + o.x)\nfn main():\n    a := V(1)\n    a += 5\nmain()\n",
        "cannot apply +=",
    );
}

#[test]
fn compound_assign_existing_forms_still_work() {
    // Regression: the pre-existing accepted forms must all still check.
    entry_ok(
        "fn main():\n    i := 1\n    i += 1\n    s := \"a\"\n    s += \"b\"\n    l := [1]\n    l += [2]\n    l *= 3\n    st := {1, 2}\n    st -= {1}\n    print(i)\nmain()\n",
    );
}

// ----- B1: qualified generic turbofish in expression position -----

const B1_SHAPES: &str = "enum Tree[T]:\n    Leaf(T)\n    Branch(Tree[T], Tree[T])\n    fn first(self) -> T:\n        match self:\n            Tree.Leaf(x): return x\n            Tree.Branch(l, r): return l.first()\n\nstruct Box[T]:\n    v: T\n    fn make(x: T) -> Box[T]:\n        return Box(x)\n";

#[test]
fn qualified_turbofish_variant_and_static_ok() {
    files_ok(&[
        ("shapes.chz", B1_SHAPES),
        (
            "main.chz",
            "import shapes\nfn main():\n    x := shapes.Tree[int].Leaf(9)\n    b := shapes.Box[int].make(5)\n    print(x.first() + b.v)\nmain()\n",
        ),
    ]);
}

#[test]
fn qualified_turbofish_regressions_ok() {
    // Must stay working: qualified NO-turbofish variant ctor; annotation form; a real qualified
    // value-subscript that must NOT be stolen as a turbofish head.
    files_ok(&[
        ("shapes.chz", B1_SHAPES),
        (
            "main.chz",
            "import shapes\nfn mk() -> List[int]:\n    return [1, 2, 3]\nfn main():\n    a := shapes.Tree.Leaf(1)\n    y: shapes.Tree[int] = shapes.Tree.Leaf(2)\n    z := mk()[0]\n    print(z)\nmain()\n",
        ),
    ]);
}

#[test]
fn qualified_not_a_type_turbofish_clean_error() {
    // `shapes.NotAType[int].X` — NotAType is not a type in `shapes`: a clean, truthful error, no lie.
    files_reject(
        &[
            ("shapes.chz", B1_SHAPES),
            (
                "main.chz",
                "import shapes\nfn main():\n    x := shapes.NotAType[int].Leaf(9)\n    print(x)\nmain()\n",
            ),
        ],
        "no member",
    );
}

// --- `return` inside a `defer:` / `spawn:` block: a check-time error (was: silently discarded at
// runtime). Chezzi has no named return values, so such a `return` could never mean anything — the
// defer block is its own closure and a spawned task outlives the frame. Same escaping-flow guard
// `recover:` uses.

#[test]
fn defer_block_rejects_return() {
    entry_rejects(
        "fn f() -> int!:\n    defer:\n        return Err(\"hijack\")\n    return Ok(1)\nprint(f())\n",
        "'return' is not allowed inside a defer block",
    );
}

#[test]
fn defer_block_rejects_bare_return() {
    entry_rejects(
        "fn f() -> int:\n    defer:\n        return\n    return 1\nprint(f())\n",
        "'return' is not allowed inside a defer block",
    );
}

#[test]
fn spawn_block_rejects_return() {
    entry_rejects(
        "fn f() -> int:\n    parallel:\n        spawn:\n            return 7\n    return 1\nprint(f())\n",
        "'return' is not allowed inside a spawn block",
    );
}

// --- boundary: the guard must NOT over-reject.

#[test]
fn defer_block_allows_return_in_nested_fn() {
    // A nested `fn` declared inside the block has its own control flow — its `return` returns from
    // IT, so it stays legal (the guard stops at `StmtKind::Fn`).
    entry_ok(
        "fn f() -> int:\n    defer:\n        fn g() -> int:\n            return 5\n        print(g())\n    return 1\nprint(f())\n",
    );
}

#[test]
fn spawn_block_allows_return_in_nested_fn() {
    entry_ok(
        "fn f() -> int:\n    parallel:\n        spawn:\n            fn g() -> int:\n                return 5\n            print(g())\n    return 1\nprint(f())\n",
    );
}

#[test]
fn parallel_body_still_allows_return() {
    entry_ok("fn f() -> int:\n    parallel:\n        return 7\n    return 1\nprint(f())\n");
}

#[test]
fn defer_block_q_still_allowed() {
    // `?` inside a `defer:` block short-circuits the block and is discarded (docs/syntax.md) — an
    // expression, not a statement: the escaping-flow guard never sees it.
    entry_ok(
        "fn g() -> int!:\n    return Ok(2)\nfn f() -> int!:\n    defer:\n        v := g()?\n        print(v)\n    return Ok(1)\nprint(f())\n",
    );
}

#[test]
fn defer_block_q_discards_regardless_of_enclosing_return() {
    // The `?`-in-defer contract is DISCARD — the enclosing fn's return type is irrelevant (the block
    // is its own closure). Before the fix, `infer_try` validated the defer-block `?` against the
    // enclosing `current_ret`, so it over-rejected under a nil/int-returning fn and only accepted
    // under a Result-returning one by coincidence. All of these must check clean now:
    // nil-returning enclosing fn:
    entry_ok(
        "fn g() -> int!:\n    return Err(\"x\")\nfn f():\n    defer:\n        v := g()?\n        print(v)\n    print(\"body\")\nf()\n",
    );
    // plain int-returning enclosing fn:
    entry_ok(
        "fn g() -> int!:\n    return Ok(2)\nfn f() -> int:\n    defer:\n        v := g()?\n        print(v)\n    return 7\nprint(f())\n",
    );
    // Option `?` discarded under a nil-returning fn (its kind need not match the enclosing return):
    entry_ok(
        "fn h() -> int?:\n    return None\nfn f():\n    defer:\n        v := h()?\n        print(v)\n    print(\"body\")\nf()\n",
    );
    // module top-level defer block:
    entry_ok(
        "fn g() -> int!:\n    return Err(\"x\")\ndefer:\n    v := g()?\n    print(v)\nprint(\"top\")\n",
    );
}

#[test]
fn defer_block_q_still_rejects_non_sum_operand() {
    // The discard arm must still reject a `?` on a non-Result/Option operand.
    let errs = check_entry(
        "fn f():\n    defer:\n        v := (1 + 2)?\n        print(v)\n    print(\"b\")\nf()\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("expects Result or Option")),
        "expected non-sum `?` rejection in defer block, got: {errs:?}"
    );
}

#[test]
fn fn_declared_in_defer_block_gets_own_q_context() {
    // `in_defer_block` must reset across the fn boundary: a nil-returning fn DECLARED inside a defer
    // block still rejects `?` (it is not itself a defer block), while a Result-returning one accepts.
    let errs = check_entry(
        "fn g() -> int!:\n    return Err(\"x\")\nfn f():\n    defer:\n        fn inner():\n            v := g()?\n            print(v)\n        inner()\n    print(\"b\")\nf()\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("returns nil, not Result or Option")),
        "a nil fn declared inside a defer block must still reject `?`, got: {errs:?}"
    );
    entry_ok(
        "fn src() -> int!:\n    return Ok(5)\nfn f():\n    defer:\n        fn inner() -> int!:\n            return Ok(src()? + 1)\n        print(\"in {inner()}\")\n    print(\"b\")\nf()\n",
    );
}

#[test]
fn spawn_block_in_defer_does_not_inherit_q_discard() {
    // `in_defer_block` must NOT leak across a spawn task boundary: a spawned task is its own closure,
    // so a `?` inside it targets the task (nil-returning → reject), exactly like a bare
    // `spawn: v := g()?` — NOT the enclosing defer's discard. (Regression for the leak the F1 flag
    // introduced.) Both the defer-wrapped and the bare spawn form must reject identically.
    let wrapped = check_entry(
        "import std.concurrency\nfn g() -> int!:\n    return Err(\"x\")\nfn f():\n    defer:\n        spawn:\n            v := g()?\n            print(v)\n    print(\"b\")\nf()\n",
    );
    assert!(
        wrapped
            .iter()
            .any(|e| e.message.contains("returns nil, not Result or Option")),
        "a `?` in a spawn block inside a defer must still reject (task is nil-returning), got: {wrapped:?}"
    );
}

#[test]
fn defer_block_break_still_says_break_outside_loop() {
    let errs = check_entry(
        "fn f() -> int:\n    for i in 0..2:\n        defer:\n            break\n    return 1\nprint(f())\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("break outside loop")),
        "expected 'break outside loop', got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("defer block")),
        "the loop_depth guard must own break/continue — no double diagnostic: {errs:?}"
    );
}

// --- the guard must be exhaustive over block-bearing statements: a `wait:` arm body (and its
// `else`) is inside the block too, so a `return` there escapes the same way.

#[test]
fn defer_block_rejects_return_in_wait_arm() {
    entry_rejects(
        "fn f() -> int!:\n    ch := Channel[int]()\n    ch.send(9)\n    defer:\n        wait:\n            v := ch.recv():\n                return Err(\"hijack {v}\")\n    return Ok(1)\nprint(f())\n",
        "'return' is not allowed inside a defer block",
    );
}

#[test]
fn defer_block_rejects_return_in_wait_else() {
    entry_rejects(
        "fn f() -> int!:\n    ch := Channel[int]()\n    defer:\n        wait:\n            v := ch.recv():\n                print(v)\n            else:\n                return Err(\"hijack\")\n    return Ok(1)\nprint(f())\n",
        "'return' is not allowed inside a defer block",
    );
}

#[test]
fn spawn_block_rejects_return_in_wait_arm() {
    entry_rejects(
        "fn f() -> int:\n    ch := Channel[int]()\n    ch.send(9)\n    parallel:\n        spawn:\n            wait:\n                v := ch.recv():\n                    return v\n    return 1\nprint(f())\n",
        "'return' is not allowed inside a spawn block",
    );
}

#[test]
fn recover_block_rejects_return_in_wait_arm() {
    entry_rejects(
        "fn f() -> int!:\n    ch := Channel[int]()\n    ch.send(9)\n    r := recover:\n        wait:\n            v := ch.recv():\n                return Ok(v)\n        7\n    return r\nprint(f())\n",
        "'return' is not allowed inside a recover block",
    );
}

// --- one `return`, one diagnostic, naming the block it is LEXICALLY in (the innermost guarded
// block owns it — the outer walker must not also claim it under the wrong noun).

#[test]
fn return_in_spawn_nested_in_defer_reports_spawn_only() {
    let errs = check_entry(
        "fn f() -> int:\n    defer:\n        parallel:\n            spawn:\n                return 7\n    return 1\nprint(f())\n",
    );
    let flow: Vec<_> = errs
        .iter()
        .filter(|e| e.message.contains("is not allowed inside a"))
        .collect();
    assert_eq!(flow.len(), 1, "one return, one diagnostic: {errs:?}");
    assert!(
        flow[0].message.contains("spawn block"),
        "the return is lexically in a spawn block: {errs:?}"
    );
}

#[test]
fn return_in_spawn_nested_in_recover_reports_spawn_only() {
    let errs = check_entry(
        "fn f() -> int:\n    x := recover:\n        parallel:\n            spawn:\n                return 7\n        1\n    return 1\nprint(f())\n",
    );
    let flow: Vec<_> = errs
        .iter()
        .filter(|e| e.message.contains("is not allowed inside a"))
        .collect();
    assert_eq!(flow.len(), 1, "one return, one diagnostic: {errs:?}");
    assert!(
        flow[0].message.contains("spawn block"),
        "the return is lexically in a spawn block: {errs:?}"
    );
}

// ===== SOUNDNESS: no runtime Int under a static `float` (untyped-constant widening rule) =====
//
// The rule (Go's): an untyped CONSTANT adapts to a float context; a TYPED int value never implicitly
// converts (write `float(x)`). The checker's accepted set is now a SUBSET of what the type-blind
// compiler can lower, so no sink can end up holding an `Int` under a static `float`.

const WIDEN_NOTE: &str = "a typed int never widens to float — write float(x)";

/// V1 — a non-const int element in a `List[float]` call-arg slot. Checked clean before, ran as an
/// `Int` under a static `float` (`xs[0] / 2` → `0.0` instead of `0.5`).
#[test]
fn widen_v1_nonconst_int_element_in_float_list_param_rejected() {
    entry_rejects(
        "fn f(xs: List[float]) -> float:\n    return xs[0] / 2\nfn main():\n    a := 1\n    print(f([a, 2.5]))\n",
        "list elements differ: int vs float",
    );
}

/// V2 — un-annotated mixed literal whose float sibling is a VARIABLE: no type context ⇒ no adaptation.
#[test]
fn widen_v2_unannotated_mixed_nonliteral_float_rejected() {
    entry_rejects(
        "fn main():\n    f := 2.5\n    xs := [1, f]\n    print(xs[0] / 2)\n",
        "list elements differ: int vs float",
    );
}

/// V2 (annotated) — the same literal WITH a `List[float]` annotation stays legal: the annotation is
/// the type context, and the compiler's element hint coerces the untyped int constant.
#[test]
fn widen_annotated_list_const_int_with_float_var_ok() {
    entry_ok("fn main():\n    f := 2.5\n    xs: List[float] = [1, f]\n    print(xs[0] / 2)\n");
}

/// V3 — a non-const int in a `Map[str, float]` VALUE position.
#[test]
fn widen_v3_nonconst_int_map_value_rejected() {
    entry_rejects(
        "fn f(m: Map[str, float]) -> float:\n    return m[\"k\"] / 2\nfn main():\n    a := 1\n    print(f({\"k\": a, \"j\": 2.5}))\n",
        "map values differ: int vs float",
    );
}

/// The `-> List[float]` RETURN slot.
#[test]
fn widen_nonconst_int_element_in_float_list_return_rejected() {
    entry_rejects(
        "fn mk() -> List[float]:\n    a := 1\n    return [a, 2.5]\nfn main():\n    print(mk())\n",
        "list elements differ: int vs float",
    );
}

/// A `List[float]` STRUCT FIELD.
#[test]
fn widen_nonconst_int_element_in_float_list_field_rejected() {
    entry_rejects(
        "struct P:\n    v: List[float]\nfn main():\n    a := 1\n    p := P([a, 2.5])\n    print(p.v)\n",
        "list elements differ: int vs float",
    );
}

/// Across a `Channel[List[float]]` — the poisoned Int used to cross the spawn airlock inside the
/// payload. (A SCALAR `Channel[float]`'s `send(1)` was already rejected: builtin method args never
/// widened. Unchanged — only the diagnostic now names the `float(x)` fix.)
#[test]
fn widen_nonconst_int_into_float_channel_rejected() {
    entry_rejects(
        "fn main():\n    ch := Channel[List[float]]()\n    a := 1\n    ch.send([a, 2.5])\n    print(ch.recv())\n",
        "list elements differ: int vs float",
    );
}

/// PROOF 1 — a `float`-typed value raising an INTEGER overflow (floats saturate to inf; they cannot
/// overflow). Must be a check error now.
#[test]
fn widen_proof_int_overflow_under_float_rejected() {
    entry_rejects(
        "fn main():\n    a := 9223372036854775807\n    xs := [a, 1.5]\n    print(xs[0] + xs[0])\n",
        "list elements differ: int vs float",
    );
}

/// PROOF 2 — `.sort()` on a `List[float]` silently returning an UNSORTED list (Int/Float compare).
#[test]
fn widen_proof_unsorted_float_list_rejected() {
    entry_rejects(
        "fn mk() -> List[float]:\n    a := 1\n    return [a, 2.5, 0.5]\nfn main():\n    xs := mk()\n    xs.sort()\n    print(xs)\n",
        "list elements differ: int vs float",
    );
}

/// A typed int VALUE never widens at a SCALAR sink either — each of these used to check clean and
/// leave an `Int` under a `float` (or, for the let/param sinks, silently coerce only by luck).
#[test]
fn widen_typed_int_at_scalar_sinks_rejected() {
    entry_rejects(
        "fn main():\n    i := 1\n    x: float = i\n    print(x)\n",
        WIDEN_NOTE,
    );
    entry_rejects(
        "fn main():\n    i := 1\n    x: float = i + 1\n    print(x)\n",
        WIDEN_NOTE,
    );
    // a fn RESULT is a TYPED int, even with constant args (correct Go)
    entry_rejects(
        "import std.cmp\nfn main():\n    x: float = cmp.max(1, 2)\n    print(x)\n",
        WIDEN_NOTE,
    );
    // param sink
    entry_rejects(
        "fn f(z: float):\n    print(z)\nfn main():\n    a := 3\n    f(a)\n",
        WIDEN_NOTE,
    );
    // return sink
    entry_rejects(
        "fn g(n: int) -> float:\n    return n + 1\nfn main():\n    print(g(2))\n",
        WIDEN_NOTE,
    );
    // native float param — the CHECKER rejects an int VARIABLE exactly like a user fn (the runtime
    // `Host::arg_float` leniency stays as defence-in-depth, but it is no longer reachable this way).
    entry_rejects(
        "import std.math\nfn main():\n    i := 4\n    print(math.sqrt(i))\n",
        WIDEN_NOTE,
    );
}

/// OVER-REJECTION GUARD — every untyped-int-CONSTANT case still adapts to a float context.
#[test]
fn widen_untyped_int_const_still_adapts() {
    entry_ok("fn main():\n    x: float = 1\n    print(x)\n");
    entry_ok("fn main():\n    x: float = -5\n    print(x)\n");
    entry_ok("fn main():\n    x: float = 1 + 2\n    print(x)\n");
    entry_ok("fn main():\n    x: float = 2 * 3\n    print(x)\n");
    entry_ok("fn f(z: float):\n    print(z)\nfn main():\n    f(7)\n    f(1 + 2)\n");
    entry_ok("fn f() -> float:\n    return 1 + 2\nfn main():\n    print(f())\n");
    entry_ok("struct P:\n    v: float\nfn main():\n    p := P(3)\n    print(p.v)\n");
    entry_ok("fn g(a: float = 3) -> float:\n    return a\nfn main():\n    print(g())\n");
    entry_ok("fn main():\n    xs: List[float] = [1, 2.3]\n    print(xs)\n");
    entry_ok("fn main():\n    m: Map[str, float] = {\"a\": 1, \"b\": 2.3}\n    print(m)\n");
    entry_ok("fn main():\n    xs := [1, 2.5]\n    print(xs[0])\n");
    entry_ok("fn main():\n    xs := [1 + 1, 2.5]\n    print(xs[0])\n");
    entry_ok("fn main():\n    xs := [1, -2.5]\n    print(xs[0])\n");
    entry_ok("import std.math\nfn main():\n    print(math.floor(2))\n");
    entry_ok(
        "fn main():\n    ch := Channel[List[float]]()\n    ch.send([1, 2.5])\n    print(ch.recv())\n",
    );
}

/// HINT-LEAK GUARD — the `List[float]` let annotation licenses THIS literal's elements only; a nested
/// literal / call argument inside the annotated value does NOT inherit the license.
#[test]
fn widen_let_hint_does_not_leak_into_nested_literal() {
    // the ANNOTATED let licenses `xs`'s own elements — NOT the list literal nested inside a call
    // argument (the compiler `take()`s its hint at the same point, so it would not coerce there).
    entry_rejects(
        "fn g(ys: List[float]) -> float:\n    return ys[0]\nfn main():\n    a := 1\n    xs: List[float] = [g([a, 2.5]), 1.0]\n    print(xs)\n",
        "list elements differ: int vs float",
    );
    // a nested collection literal keeps its own (un-licensed) inference
    entry_rejects(
        "fn main():\n    a := 1\n    xs: List[List[float]] = [[a, 2.5]]\n    print(xs)\n",
        "list elements differ: int vs float",
    );
}

// ===== SOUNDNESS follow-ups (adversarial review): the sinks the first cut still leaked =====

/// A GENERIC fn instantiated at float and used as a fn VALUE (`f := id[float]`) is generic-ERASED at
/// runtime: its declared param is `T`, so the callee prologue emits NO `Op::CoerceFloat`. An int
/// argument would sit in the slot under a static `float` (`f(1) / 2` → `0`, and a `List[float]` built
/// from it sorted UNSORTED). A function-VALUE call therefore never widens — write `f(1.0)`.
#[test]
fn widen_through_fn_value_rejected() {
    entry_rejects(
        "fn id[T](x: T) -> T:\n    return x\nfn main():\n    f := id[float]\n    print(f(1) / 2)\n",
        "expected float, found int",
    );
    entry_rejects(
        "fn id[T](x: T) -> T:\n    return x\nfn main():\n    f: fn(float) -> float = id[float]\n    print(f(1) / 2)\n",
        "expected float, found int",
    );
    // …and a plain (non-generic) fn reached through a VALUE is strict too (the checker cannot tell
    // the two apart from the `Ty::Func` alone).
    entry_rejects(
        "fn h(z: float) -> float:\n    return z\nfn main():\n    f := h\n    print(f(1))\n",
        "expected float, found int",
    );
    entry_ok(
        "fn id[T](x: T) -> T:\n    return x\nfn main():\n    f := id[float]\n    print(f(1.0) / 2)\n",
    );
}

/// A float sink spelled through a type ALIAS (`type F = float`) is a real float sink: the compiler
/// resolves the alias at every coercion site, so the untyped-constant widen still lowers to an f64.
#[test]
fn widen_through_float_alias_ok() {
    entry_ok(
        "type F = float\nfn g(z: F) -> F:\n    return z\nstruct P:\n    v: F\nfn main():\n    x: F = 1\n    xs: List[F] = [1, 2]\n    print(x / 2)\n    print(g(3) / 2)\n    print(P(3).v / 2)\n    print(xs)\n",
    );
}

/// The `float(x)` note is attached ONLY to a mismatch whose expression is a TYPED int. An untyped int
/// CONSTANT at a NON-widening float sink (a builtin-method arg, an enum payload) is rejected because
/// the sink does not widen at all — telling the user "a typed int never widens" would be a lie.
#[test]
fn widen_note_absent_for_untyped_const_at_nonwidening_sink() {
    let errs = check_entry("fn main():\n    ch := Channel[float]()\n    ch.send(1)\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("expected float, found int")),
        "expected the send() mismatch, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains(WIDEN_NOTE)),
        "an untyped int constant must not be blamed as a TYPED int: {errs:?}"
    );
}

/// A `List[float]` / `Map[_, float]` annotation is a type CONTEXT: an ALL-int-constant literal adapts
/// to it (the docs say so — and now the checker agrees; it used to be a spurious error).
#[test]
fn widen_annotated_all_int_collection_adapts() {
    entry_ok("fn main():\n    xs: List[float] = [1, 2]\n    print(xs)\n");
    entry_ok("fn main():\n    m: Map[str, float] = {\"a\": 1}\n    print(m)\n");
}

// ===== adversarial-review fixes: generic erasure + collection-alias annotations =====

/// GENERIC ERASURE at a method call: a method param declared as the struct's type VARIABLE `T`,
/// instantiated at `float`, is NOT a float sink the backend can lower — `emit_float_param_prologue`
/// keys on the DECLARED syntactic type (`T`), which is erased, so it emits no `Op::CoerceFloat`. The
/// checker must therefore refuse to widen there (same rule as a call through a fn VALUE), or an `Int`
/// lands in a slot whose static type is `float`.
#[test]
fn widen_generic_method_param_at_float_rejected() {
    entry_rejects(
        "struct Box[T]:\n    v: T\n\n    fn set(self, x: T):\n        self.v = x\n\nb := Box[float](1.0)\nb.set(1)\n",
        "expected float, found int",
    );
    // enum method, same shape
    entry_rejects(
        "enum Opt[T]:\n    Some(T)\n    None\n\n    fn eq(self, x: T) -> bool:\n        return true\n\no := Opt[float].Some(1.0)\nprint(o.eq(1))\n",
        "expected float, found int",
    );
}

/// A collection type spelled through an ALIAS (`type LF = List[float]`) is NOT an element-widening
/// hint: the backend's `float_elem_hint` matches the SYNTACTIC `List[…]`/`Map[…]` shape only, so it
/// would emit no `Op::CoerceFloat` for the elements. The checker must key its own hint on the same
/// syntactic shape (an aliased ELEMENT — `List[F]` with `type F = float` — still widens: the backend
/// resolves float aliases at the element).
#[test]
fn widen_collection_alias_annotation_rejected() {
    entry_rejects("type LF = List[float]\nxs: LF = [1, 2]\n", "cannot assign");
    entry_rejects(
        "type MF = Map[str, float]\nm: MF = {\"k\": 1}\n",
        "cannot assign",
    );
    // the ELEMENT spelled through an alias keeps working (the backend's `is_float` is alias-aware)
    entry_ok("type F = float\nxs: List[F] = [1, 2.5]\nprint(xs)\n");
}

/// KNOWN LIMIT (pinned): a VARIADIC `float` param adapts an untyped int constant only when an untyped
/// FLOAT constant sibling is present (the list peephole is the only coercion the type-blind backend
/// can emit for the synthesized pack — the callee prologue cannot `Op::CoerceFloat` a List slot).
/// `f(1, 2)` is therefore rejected while the identical scalar sink `fn f(z: float); f(1)` adapts.
/// Upgrade path: make `Op::CoerceFloat` list-aware and emit the prologue for the variadic slot.
#[test]
fn widen_variadic_float_param_all_int_consts_rejected_known_limit() {
    entry_rejects(
        "fn f(...zs: float):\n    print(zs)\nf(1, 2)\n",
        "expected List[float], found List[int]",
    );
    entry_ok("fn f(...zs: float):\n    print(zs)\nf(1, 2.5)\n");
}

// ---------------------------------------------------------------------------
// Bound methods are NOT first-class values (check-OK -> runtime-fault hole).
// A struct field-read that misses the data fields must NOT fall back to the
// method table: it used to hand back a `Ty::Func` still carrying the un-bound
// `self` slot typed `Ty::Unknown`, which (a) has no runtime lowering (the
// compiler emits a plain field load -> VM "no field 'get' on S") and (b)
// LAUNDERS types -- the `?` self slot unifies with anything.
// ---------------------------------------------------------------------------

const METH_S: &str = "\
struct S:
    n: int
    fn get(self) -> int:
        return self.n
";

#[test]
fn bound_method_as_value_rejected() {
    entry_rejects(
        &format!("{METH_S}s := S(5)\ng := s.get\n"),
        "type S has no field 'get'",
    );
    // ...and the message must say WHY (it's a method), not read as a typo.
    entry_rejects(&format!("{METH_S}s := S(5)\ng := s.get\n"), "is a method");
}

#[test]
fn bound_method_launder_rejected() {
    // Each of these type-checked (the `?` self slot unified with anything) then faulted at runtime.
    // Each must now be exactly ONE error -- no `'closure' expects 1 argument(s)` cascade.
    for src in [
        format!("{METH_S}s := S(5)\ng := s.get\nprint(g(s))\n"),
        format!("{METH_S}s := S(5)\ng := s.get\nprint(g(\"anything\"))\n"),
        format!(
            "{METH_S}fn apply(f: fn(S) -> int, v: S) -> int:\n    return f(v)\ns := S(5)\nprint(apply(s.get, s))\n"
        ),
    ] {
        let errs = check_entry(&src);
        assert_eq!(
            errs.len(),
            1,
            "expected exactly one error for {src:?}, got: {errs:?}"
        );
        assert!(
            errs[0].message.contains("has no field 'get'"),
            "unexpected error for {src:?}: {errs:?}"
        );
    }
    // `xs := [s.get, s.get]` used to build a `List[fn(?) -> int]` and fault at LIST CONSTRUCTION.
    // Now each element is rejected at its own span; the elements are then `Unknown`, so the
    // element-type inference reports its usual follow-on. No `'closure' expects 1 argument(s)`.
    let errs = check_entry(&format!(
        "{METH_S}s := S(5)\nxs := [s.get, s.get]\nprint(xs.len())\n"
    ));
    assert_eq!(
        errs.iter()
            .filter(|e| e.message.contains("has no field 'get'"))
            .count(),
        2,
        "expected one reject per element, got: {errs:?}"
    );
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("expects 1 argument")),
        "no arity cascade expected, got: {errs:?}"
    );
}

#[test]
fn bound_method_via_self_rejected() {
    entry_rejects(
        "struct S:\n    n: int\n    fn get(self) -> int:\n        return self.n\n    fn leak(self) -> int:\n        g := self.get\n        return g(self)\ns := S(5)\nprint(s.leak())\n",
        "has no field 'get'",
    );
}

#[test]
fn bound_method_on_generic_struct_rejected() {
    // The error path on a GENERIC receiver still renders (no panic on the shape lookup).
    entry_rejects(
        "struct Box[T]:\n    v: T\n    fn get(self) -> T:\n        return self.v\nb := Box[int](1)\ng := b.get\n",
        "has no field 'get'",
    );
}

#[test]
fn method_neighbors_still_ok() {
    // NO-OVER-REJECTION guards -- everything adjacent to the bound-method reject keeps working.
    // 1. a normal call, a chained call, a call on a nested field.
    entry_ok(
        "struct Inner:\n    n: int\n    fn get(self) -> int:\n        return self.n\nstruct Outer:\n    i: Inner\n    fn inner(self) -> Inner:\n        return self.i\no := Outer(Inner(5))\nprint(o.i.get())\nprint(o.inner().get())\n",
    );
    // 2. THE closest neighbor -- a genuinely fn-TYPED field. Both `h.f(3)` and `g := h.f; g(3)`.
    entry_ok(
        "struct H:\n    f: fn(int) -> int\nfn dbl(x: int) -> int:\n    return x * 2\nh := H(dbl)\nprint(h.f(3))\ng := h.f\nprint(g(3))\n",
    );
    // 3. a fn-typed FIELD whose name collides with a method-ish name -- the field still wins.
    entry_ok(
        "struct C:\n    get: fn(int) -> int\n    fn other(self) -> int:\n        return 1\nfn dbl(x: int) -> int:\n    return x * 2\nc := C(dbl)\nprint(c.get(3))\nprint(c.other())\n",
    );
    // 4. a static/associated CALL (a bare `P.make` VALUE is not legal today -- `unknown name 'P'`).
    entry_ok(
        "struct P:\n    n: int\n    fn make(n: int) -> P:\n        return P(n)\np := P.make(3)\nprint(p.n)\n",
    );
}

// ---------------------------------------------------------------------------
// `index` / `set_index` V-coherence. `x OP= v` is EXACTLY `x = x OP v`
// (docs/syntax.md, compound assignment), so a compound index-assign's LHS is
// typed from `index`'s RETURN. The direct index-assign path used to take it
// from `set_index`'s `val` param instead, so an incoherent pair
// (`index -> str` / `set_index(_, val: int)`) type-checked, then faulted with
// "cannot apply Add to str and int" at runtime.
// ---------------------------------------------------------------------------

/// index -> str, but set_index takes an int val. Incoherent: not an `IndexSet[int, V]`.
const INCOHERENT_V: &str = "\
struct S:
    d: List[str]
    fn index(self, key: int) -> str:
        return self.d[key]
    fn set_index(self, key: int, val: int):
        print(\"set {val}\")
s := S([\"a\", \"b\"])
";

#[test]
fn index_set_incoherent_rejected() {
    // Exactly ONE error, and it names the incoherence (no "cannot apply += to str and int" cascade,
    // no bogus "index must be int" from the not-indexable path).
    let errs = check_entry(&format!("{INCOHERENT_V}s[0] += 1\n"));
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert!(
        errs[0]
            .message
            .contains("does not satisfy IndexSet (index returns str but set_index's val is int)"),
        "unexpected error: {errs:?}"
    );
    // K mismatch is a compound-path incoherence too: the read keys by int, the write-back by str.
    entry_rejects(
        "struct S:\n    d: List[str]\n    fn index(self, key: int) -> str:\n        return self.d[key]\n    fn set_index(self, key: str, val: str):\n        print(\"set {val}\")\ns := S([\"a\", \"b\"])\ns[0] += \"x\"\n",
        "does not satisfy IndexSet (index's key is int but set_index's key is str)",
    );
    // A NON-int key routes through the same arm — no spurious "index must be int" companion.
    let errs = check_entry(
        "struct M:\n    d: Map[str, str]\n    fn index(self, key: str) -> str:\n        return self.d[key]\n    fn set_index(self, key: str, val: int):\n        print(\"set {val}\")\nm := M({\"a\": \"x\"})\nm[\"a\"] += 1\n",
    );
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert!(
        errs[0].message.contains("does not satisfy IndexSet"),
        "unexpected error: {errs:?}"
    );
}

#[test]
fn index_set_asymmetric_plain_write_still_ok() {
    // NO-OVER-REJECTION: a plain `=` NEVER reads through `index`, so an asymmetric pair stays legal
    // (it type-checks AND runs today). Only the COMPOUND form reads, and only it is gated.
    // (a) a safe-read container: `index -> V?`, `set_index(_, V)`.
    entry_ok(
        "struct T:\n    d: Map[int, int]\n    fn index(self, key: int) -> int?:\n        return self.d.get(key)\n    fn set_index(self, key: int, val: int):\n        self.d[key] = val\nt := T({})\nt[0] = 9\nprint(t[0])\n",
    );
    // (b) a widening writer: `index -> str`, `set_index(_, val: int)` — write-only use is sound.
    entry_ok(&format!("{INCOHERENT_V}s[0] = 1\n"));
}

#[test]
fn index_set_missing_method_messages_unchanged() {
    // The vague wording is PRESERVED for the missing-method cases; only a disagreeing PAIR gets the
    // new coherence message.
    entry_rejects(
        "struct RO:\n    xs: List[int]\n    fn index(self, key: int) -> int:\n        return self.xs[key]\nb := RO([1, 2, 3])\nb[0] = 9\n",
        "cannot index-assign into RO",
    );
    entry_rejects(
        "struct WO:\n    xs: List[int]\n    fn set_index(self, key: int, val: int):\n        self.xs[key] = val\nb := WO([1, 2, 3])\nb[0] = 9\n",
        "cannot index-assign into WO",
    );
}

#[test]
fn index_set_coherent_still_ok() {
    // NO-OVER-REJECTION: a coherent pair -- read / write / compound / negative index.
    entry_ok(
        "struct S:\n    d: List[str]\n    fn index(self, key: int) -> str:\n        return self.d[key]\n    fn set_index(self, key: int, val: str):\n        self.d[key] = val\ns := S([\"a\", \"b\"])\nprint(s[0])\ns[0] = \"x\"\ns[1] += \"y\"\nprint(s[-1])\n",
    );
    // A GENERIC coherent pair must survive EVERY instantiation (the compare happens AFTER the
    // struct param substitution).
    let g = "struct Buf[T]:\n    xs: List[T]\n    fn index(self, key: int) -> T:\n        return self.xs[key]\n    fn set_index(self, key: int, val: T):\n        self.xs[key] = val\n";
    entry_ok(&format!(
        "{g}b := Buf[int]([1, 2])\nb[0] = 9\nb[1] += 1\nprint(b[0])\n"
    ));
    entry_ok(&format!(
        "{g}b := Buf[str]([\"a\"])\nb[0] = \"z\"\nb[0] += \"!\"\nprint(b[0])\n"
    ));
    // Builtin index/field targets must not regress.
    entry_ok(
        "struct F:\n    n: int\nm := {\"a\": 1}\nm[\"a\"] += 1\nxs := [1, 2]\nxs[0] += 1\nf := F(1)\nf.n += 1\nprint(m[\"a\"] + xs[0] + f.n)\n",
    );
}

// ===== a range is NOT a value (check-OK-then-cannot-run class) =====

/// A range literal has no runtime value: the compiler lowers it ONLY as a `for`/comprehension
/// iterable or a slice receiver, and rejects it everywhere else ("a range can only be used as the
/// iterable of a `for` loop"). The checker used to type `a..b` as `List[int]`, so every value
/// position type-checked clean and then FAILED TO COMPILE at run time (zero output, exit 1).
/// The checker's accepted set must be a SUBSET of what the compiler can lower.
#[test]
fn range_in_value_position_is_a_type_error() {
    let m = "range";
    // (a) the repro: bound to a variable, then printed.
    entry_rejects(
        "fn main():\n    print(\"before\")\n    x := 0..3\n    print(x)\nmain()\n",
        m,
    );
    // (b)/(c) the "materialize it" ctors — `range(a, b)` is the real escape hatch, not these.
    entry_rejects("fn main():\n    y := List(0..3)\n    print(y)\nmain()\n", m);
    entry_rejects("fn main():\n    y := Set(0..3)\n    print(y)\nmain()\n", m);
    // (d)/(e) method receiver.
    entry_rejects(
        "fn main():\n    n := (0..5).len()\n    print(n)\nmain()\n",
        m,
    );
    entry_rejects("fn main():\n    (0..3).push(7)\nmain()\n", m);
    // (f) binary operand.
    entry_rejects(
        "fn main():\n    y := (0..3) + [7]\n    print(y)\nmain()\n",
        m,
    );
    // (g) equality — the permissive `Eq | NotEq => Ty::Bool` arm must still INFER both operands,
    // or the error never fires and the compiler backstop stays reachable from check-clean code.
    entry_rejects(
        "fn main():\n    b := (0..3) == [0, 1, 2]\n    print(b)\nmain()\n",
        m,
    );
    // (h)/(i) collection literal element / map value.
    entry_rejects(
        "fn main():\n    xs := [0..3, [9]]\n    print(xs)\nmain()\n",
        m,
    );
    entry_rejects(
        "fn main():\n    m := {\"a\": 0..3}\n    print(m)\nmain()\n",
        m,
    );
    // (j) call argument against a `List[int]` param.
    entry_rejects(
        "fn total(xs: List[int]) -> int:\n    s := 0\n    for x in xs:\n        s = s + x\n    return s\nfn main():\n    print(total(0..4))\nmain()\n",
        m,
    );
    // (k) through an `[S: Iterator[T], T]` bound.
    entry_rejects(
        "fn first_or[S: Iterator[T], T](xs: S, d: T) -> T:\n    for x in xs:\n        return x\n    return d\nfn main():\n    print(first_or(0..3, 9))\nmain()\n",
        m,
    );
    // (l) plain INDEX (not a slice) — the compiler has no lowering for this either.
    entry_rejects("fn main():\n    print((0..10)[2])\nmain()\n", m);
    // (m) `in` on a range RHS (was a piecemeal ad-hoc guard; the generic arm subsumes it).
    entry_rejects("fn main():\n    print(5 in 1..10)\nmain()\n", m);
    // (n) an annotated binding must name the RANGE, not leak the old `List[int]` laundering.
    let errs = check_entry("fn main():\n    y: str = 0..3\n    print(y)\nmain()\n");
    assert!(
        errs.iter().any(|e| e.message.contains("range")),
        "expected a range error, got: {errs:?}"
    );
    // Pre-fix this said "cannot assign List[int] to variable of type str" — the laundering, and the
    // very diagnostic that proved the root cause. It must be gone (the hint's own "List[int]" is
    // fine, hence matching on the assignment wording, not the type name).
    assert!(
        !errs.iter().any(|e| e.message.contains("cannot assign")),
        "a range must not be laundered as List[int]: {errs:?}"
    );
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
}

/// The sanctioned positions — the ONLY three places a `Range` expr reaches the compiler
/// (`for`-iterable, comprehension-clause iterable, slice receiver) plus the `match` range PATTERN
/// (a different AST node entirely). An over-broad fix has twice rejected a legitimate neighbor in
/// this repo, so every one gets a boundary assertion.
#[test]
fn range_sanctioned_positions_still_check() {
    // for-iterable: literal bounds, a call in the bound, parens, expression bounds.
    entry_ok("fn main():\n    for i in 0..10:\n        print(i)\nmain()\n");
    entry_ok(
        "fn main():\n    xs := [1, 2, 3]\n    for i in 0..xs.len():\n        print(xs[i])\nmain()\n",
    );
    entry_ok("fn main():\n    for i in (0..3):\n        print(i)\nmain()\n");
    entry_ok(
        "fn main():\n    a := 1\n    b := a + 4\n    for i in a..b:\n        print(i)\nmain()\n",
    );
    // comprehension-clause iterable: plain, guarded, nested, map- and set-comprehension.
    entry_ok("fn main():\n    print([i for i in 0..3])\nmain()\n");
    entry_ok("fn main():\n    print([i for i in 0..10 if i % 2 == 0])\nmain()\n");
    entry_ok(
        "fn main():\n    print([x * y for x in 1..4 if x % 2 == 1 for y in [10, 20]])\nmain()\n",
    );
    entry_ok("fn main():\n    print({i: i * i for i in 0..3})\nmain()\n");
    entry_ok("fn main():\n    print({i % 3 for i in 0..9})\nmain()\n");
    // slice receiver — a range literal is sliceable (it materializes, then slices).
    entry_ok("fn main():\n    a: List[int] = (0..10)[::2]\n    print(a.len())\nmain()\n");
    entry_ok("fn main():\n    print((0..10)[1:8:3])\nmain()\n");
    entry_ok("fn main():\n    print((0..5)[::-1])\nmain()\n");
    // match range PATTERNS (`Pattern::Range`, never reaches `infer_kind`), incl. negative bounds.
    entry_ok(
        "fn grade(n: int) -> str:\n    match n:\n        0..60: return \"F\"\n        60..90: return \"B\"\n        _: return \"A\"\nfn main():\n    print(grade(70))\nmain()\n",
    );
    entry_ok(
        "fn side(n: int) -> str:\n    match n:\n        -10..-5: return \"lo\"\n        _: return \"hi\"\nfn main():\n    print(side(-7))\nmain()\n",
    );
    // the documented escape hatch: `range(a, b)` really does materialize a `List[int]`.
    entry_ok(
        "fn main():\n    xs: List[int] = range(0, 3)\n    print(Set(range(0, 3)).len() + xs.len())\nmain()\n",
    );
}

/// R1 — the bytes native seam is `bytes`-ONLY at every param. A `bytearray` is NOT assignable to a
/// `bytes` sink (the deliberate rule from 7b29552 — `bytearray_not_assignable_to_bytes`; a mutable
/// buffer aliased under an immutable `bytes` type is the hole it closes), so a caller converts with
/// `bytes(ba)` — an explicit copy, exactly like CPython's `bytes(ba)`. This pins the rule ACROSS the
/// new seam (io / crypto / encoding / Socket), which is what the shipped doc comments now say.
#[test]
fn bytes_native_seam_takes_bytes_only_bytearray_needs_an_explicit_convert() {
    for (call, what) in [
        ("crypto.sha256_bytes(ba)", "sha256_bytes"),
        ("encoding.base64_encode_bytes(ba)", "base64_encode_bytes"),
        ("io.write_bytes(\"/dev/null\", ba)", "write_bytes"),
    ] {
        let errs = check_entry(&format!(
            "import std.crypto\nimport std.encoding\nimport std.io\n\nfn main():\n    ba := bytearray(b\"hi\")\n    print({call})\nmain()\n"
        ));
        assert!(
            errs.iter()
                .any(|e| e.message.contains("expected bytes, found bytearray")),
            "{what} must reject a bytearray, got: {errs:?}"
        );
    }
    // Socket.write_bytes too (the same rule on the VM-intercepted method sig).
    let errs = check_entry(
        "import std.net\n\nfn go(sock: net.Socket) -> int!:\n    ba := bytearray(b\"hi\")\n    n := sock.write_bytes(ba)?\n    return Ok(n)\n\nfn main():\n    pass\nmain()\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("expected bytes, found bytearray")),
        "Socket.write_bytes must reject a bytearray, got: {errs:?}"
    );
    // …and `bytes(ba)` is the documented convert — it type-checks everywhere the seam takes bytes.
    entry_ok(
        "import std.crypto\nimport std.encoding\nimport std.io\n\nfn main():\n    ba := bytearray(b\"hi\")\n    print(crypto.sha256_bytes(bytes(ba)))\n    print(encoding.base64_encode_bytes(bytes(ba)))\n    match io.write_bytes(\"/dev/null\", bytes(ba)):\n        Ok(_): pass\n        Err(e): print(e.message())\nmain()\n",
    );
    entry_ok(
        "import std.net\n\nfn go(sock: net.Socket) -> int!:\n    ba := bytearray(b\"hi\")\n    n := sock.write_bytes(bytes(ba))?\n    return Ok(n)\n\nfn main():\n    pass\nmain()\n",
    );
}

// ----- Contains operator protocol (L5): `x in some_struct` -----

/// A struct with a `contains(self, item) -> bool` method accepts `x in it` when the LHS type
/// matches the declared item type.
#[test]
fn contains_protocol_struct_ok() {
    ok(
        "struct Bag:\n    xs: List[int]\n    fn contains(self, x: int) -> bool:\n        return false\nfn main():\n    b := Bag([1])\n    print(2 in b)\nmain()\n",
    );
}

/// Item-type mismatch (`"s" in bag_of_int`) is a CLEAN checker error, never a runtime panic (req 2).
#[test]
fn contains_item_type_mismatch_rejects() {
    rejects(
        "struct Bag:\n    xs: List[int]\n    fn contains(self, x: int) -> bool:\n        return false\nfn main():\n    b := Bag([1])\n    print(\"s\" in b)\nmain()\n",
        "membership",
    );
}

/// A struct WITHOUT a `contains` method still cleanly rejects `in`, with a message that hints at the
/// `contains` protocol (req 4).
#[test]
fn contains_missing_method_rejects_with_hint() {
    rejects(
        "struct NoC:\n    n: int\nfn main():\n    print(2 in NoC(0))\nmain()\n",
        "contains",
    );
}

/// A `contains` whose return type is not `bool` does not satisfy the protocol — `in` rejects with the
/// same protocol hint rather than green-lighting a non-bool result.
#[test]
fn contains_wrong_return_rejects_with_hint() {
    rejects(
        "struct Bag:\n    n: int\n    fn contains(self, x: int) -> int:\n        return 0\nfn main():\n    print(2 in Bag(0))\nmain()\n",
        "contains",
    );
}

/// Generic `Box[T]`: the `contains` param type is INSTANTIATED to `int` — `2 in Box[int](5)` is OK
/// but `"s" in Box[int](5)` is rejected (subst must not leave `Param(T)`/`Unknown`) (req 3).
#[test]
fn contains_generic_struct_substitutes_item() {
    ok(
        "struct Box[T]:\n    v: T\n    fn contains(self, x: T) -> bool:\n        return x == self.v\nfn main():\n    print(2 in Box[int](5))\nmain()\n",
    );
    rejects(
        "struct Box[T]:\n    v: T\n    fn contains(self, x: T) -> bool:\n        return x == self.v\nfn main():\n    print(\"s\" in Box[int](5))\nmain()\n",
        "membership",
    );
}

/// `in` through a `Contains[int]` generic bound is accepted (analog of `<` through `Comparable`),
/// and a mismatched item type (`str` LHS against a `Contains[int]` bound) is still cleanly rejected —
/// the bound's item arg is recovered, not left `Unknown`.
#[test]
fn contains_through_bound_ok_and_item_mismatch_rejects() {
    ok(
        "struct Bag:\n    xs: List[int]\n    fn contains(self, x: int) -> bool:\n        return false\nfn has[C: Contains[int]](c: C, n: int) -> bool:\n    return n in c\nfn main():\n    print(has(Bag([1]), 1))\nmain()\n",
    );
    rejects(
        "struct Bag:\n    xs: List[int]\n    fn contains(self, x: int) -> bool:\n        return false\nfn has[C: Contains[int]](c: C, s: str) -> bool:\n    return s in c\nfn main():\n    print(has(Bag([1]), \"x\"))\nmain()\n",
        "membership",
    );
}

// ===== F2/F3/F4 — checker over-rejection / diagnostic fixes =====

/// Task 2 (option a) — EVERY protocol existential is sendable now (Go `chan interface` parity), not
/// just the built-in `Error`: `Channel[Drawable]`, `Channel[int!]`, `Channel[Error]`, and
/// `Channel[NS]` over any user protocol all type-check. A genuinely-unserializable element (one
/// carrying an FFI/native handle) is rejected at the RUNTIME airlock, not at construction — see
/// `vm::parity_tests::ffi_handle_cannot_cross_airlock_three_engine`.
#[test]
fn channel_of_any_protocol_existential_is_sendable() {
    entry_ok(
        "protocol Drawable:\n    fn draw(self) -> str\nc := Channel[Drawable]()\nprint(\"x\")\n",
    );
    entry_ok("import std.concurrency\nc := Channel[int!]()\nprint(\"x\")\n");
    entry_ok("import std.concurrency\nc := Channel[Error]()\nprint(\"x\")\n");
    entry_ok(
        "import std.concurrency\nprotocol NS:\n    fn tag(self) -> int\nc := Channel[NS]()\nprint(\"x\")\n",
    );
}

/// Task 2 (option a) — a user protocol existential is now SENDABLE across the airlock (Go
/// `chan interface` parity): the erased witness crosses by value like any other type; a witness that
/// genuinely can't cross (one carrying an FFI/native handle) is rejected at the runtime airlock, not
/// at construction (see `vm::parity_tests::ffi_handle_cannot_cross_airlock_three_engine`).
#[test]
fn protocol_existential_is_sendable_across_airlock() {
    // Bare `Channel[UserProto]()` type-checks.
    entry_ok(
        "protocol Drawable:\n    fn draw(self) -> str\nc := Channel[Drawable]()\nprint(\"x\")\n",
    );
    // A protocol-typed struct field carried as a channel element type-checks.
    entry_ok(
        "protocol Drawable:\n    fn draw(self) -> str\nstruct H:\n    f: Drawable\nc := Channel[H]()\nprint(\"x\")\n",
    );
    // A protocol-typed spawn arg type-checks.
    entry_ok(
        "protocol Drawable:\n    fn draw(self) -> str\nstruct Sq:\n    fn draw(self) -> str:\n        return \"sq\"\nfn use_it(d: Drawable):\n    print(d.draw())\nfn main():\n    parallel:\n        spawn use_it(Sq())\nmain()\n",
    );
}

/// Task 2 (option a) — `GErr{w: Odd}` where `Odd` is a user protocol is now SENDABLE (`Odd` is
/// sendable ⇒ `GErr` is sendable), so a DIRECT-LITERAL `Err(GErr(..))` sent over `Channel[int!]`
/// type-checks (and runs: prints `sent`). This was rejected under the old Error-only rule, where a
/// protocol-field struct was the canonical "non-sendable Error witness"; that class no longer exists
/// at the checker level. A witness carrying an FFI/native handle is checker-sendable and rejected at
/// the RUNTIME airlock instead (`vm::parity_tests::ffi_handle_cannot_cross_airlock_three_engine`).
#[test]
fn channel_send_sendable_error_literal_ok() {
    entry_ok(
        "import std.concurrency\nprotocol Odd:\n    fn tag(self) -> int\nstruct Impl:\n    fn tag(self) -> int:\n        return 1\nstruct GErr:\n    w: Odd\n    fn message(self) -> str:\n        return \"x\"\nc := Channel[int!]()\nc.send(Err(GErr(Impl())))\n",
    );
}

/// Task 2 (option a) — a `?` propagating `GErr{w: Odd}` inside a `recover:` block is now ACCEPTED:
/// `GErr` satisfies `Error` (has `message`) AND is sendable (its `Odd` field is a sendable protocol),
/// so the recover result `Result[_, Error]` accepts it. Was rejected with the "satisfies Error but
/// isn't sendable" split message under the old rule, when a protocol field made the witness
/// non-sendable.
#[test]
fn recover_try_sendable_error_ok() {
    entry_ok(
        "protocol Odd:\n    fn tag(self) -> int\nstruct Impl:\n    fn tag(self) -> int:\n        return 1\nstruct GErr:\n    w: Odd\n    fn message(self) -> str:\n        return \"x\"\nfn bar(x: int) -> Result[int, GErr]:\n    if x == 0:\n        return Ok(1)\n    return Err(GErr(Impl()))\nfn main():\n    r := recover: bar(1)?\n    print(\"unreached\")\nmain()\n",
    );
}

/// Task 2 (option a) — an EXPLICIT `-> int!` (`Result[int, Error]`) return whose concrete `Err`
/// branch is `GErr{w: Odd}` is now ACCEPTED at the `return` widening site: `GErr` is sendable (its
/// `Odd` field is a sendable protocol), so the `assignable`-to-`Error` guard admits it. Was rejected
/// under the old rule.
#[test]
fn explicit_bang_annotation_over_sendable_error_ok() {
    entry_ok(
        "protocol Odd:\n    fn tag(self) -> int\nstruct Impl:\n    fn tag(self) -> int:\n        return 1\nstruct GErr:\n    w: Odd\n    fn message(self) -> str:\n        return \"x\"\nfn f(x: int) -> int!:\n    if x == 0:\n        return Ok(1)\n    return Err(GErr(Impl()))\nprint(\"x\")\n",
    );
}

/// Task 2 (option a) — a generic `fn wrap[E](e: E) -> int!E` instantiated with concrete `E = GErr`
/// (sendable, since its `Odd` field is a sendable protocol) and SENT over `Channel[int!]` is now
/// ACCEPTED. Was rejected under the old rule at the `Channel.send` boundary (`GErr` counted as
/// non-sendable). The generic-param deferral (`Ty::Param => true` re-checked at instantiation) is
/// unchanged; the concrete substitution is simply sendable now.
#[test]
fn generic_fn_sendable_err_instantiation_ok_at_channel_send() {
    entry_ok(
        "import std.concurrency\nprotocol Odd:\n    fn tag(self) -> int\nstruct Impl:\n    fn tag(self) -> int:\n        return 1\nstruct GErr:\n    w: Odd\n    fn message(self) -> str:\n        return \"x\"\nfn wrap[E](e: E) -> int!E:\n    return Err(e)\nc := Channel[int!]()\nc.send(wrap(GErr(Impl())))\nprint(\"x\")\n",
    );
}

/// Task 2 (option a) — a 3+ branch inferred `Result` return mixing `EA`/`GErr{w:Odd}`/`EB` now folds
/// cleanly to `Result[int, Error]` and is ACCEPTED. All three witnesses are sendable now (`GErr`'s
/// `Odd` field is a sendable protocol), so `join_err_slot` unifies them to the `Error` existential
/// instead of hard-conflicting on the old sendable-vs-non-sendable split. Was rejected under the old
/// rule with "cannot infer return type: conflicting branches" — the order-sensitivity that caused it
/// only existed because a protocol-field struct read as non-sendable.
#[test]
fn three_branch_mixed_error_inference_ok() {
    entry_ok(
        "protocol Odd:\n    fn tag(self) -> int\nstruct Impl:\n    fn tag(self) -> int:\n        return 1\nstruct GErr:\n    w: Odd\n    fn message(self) -> str:\n        return \"x\"\nstruct EA:\n    fn message(self) -> str:\n        return \"a\"\nstruct EB:\n    fn message(self) -> str:\n        return \"b\"\nfn f(x: int):\n    if x == 0:\n        return Ok(1)\n    elif x == 1:\n        return Err(EA())\n    elif x == 2:\n        return Err(GErr(Impl()))\n    else:\n        return Err(EB())\nprint(\"x\")\n",
    );
}

/// F3 — a generic fn over a native reserved handle (`Shared`/`Channel`/`Atomic`/`RwShared`) binds its
/// type param `T` from the argument, exactly like the identical shape over `List[T]`.
#[test]
fn generic_fn_over_native_handles_infers_param() {
    entry_ok(
        "import std.concurrency\nfn peek[T](s: Shared[T]) -> T:\n    return s.get()\nx := peek(Shared(9))\nprint(x)\n",
    );
    entry_ok(
        "import std.concurrency\nfn first[T](c: Channel[T]) -> T:\n    return c.recv()\nch := Channel[int]()\nch.send(1)\nprint(first(ch))\n",
    );
    entry_ok(
        "import std.concurrency\nfn look[T](a: Atomic[T]) -> T:\n    return a.load()\nprint(look(Atomic(3)))\n",
    );
    entry_ok(
        "import std.concurrency\nfn peekrw[T](s: RwShared[T]) -> T:\n    return s.get()\nprint(peekrw(RwShared(4)))\n",
    );
}

/// F3 (subst side) — a generic wrapper struct holding a `Channel[T]` substitutes `Channel[T]→Channel[int]`
/// so its channel field/method type resolves after construction.
#[test]
fn generic_wrapper_struct_holding_channel_substitutes() {
    entry_ok(
        "import std.concurrency\nstruct Box[T]:\n    ch: Channel[T]\nfn main():\n    b := Box(Channel[int]())\n    b.ch.send(7)\n    x: int = b.ch.recv()\n    print(x)\nmain()\n",
    );
}

/// F4 — `Atomic.add` type mismatch must NOT show the List/Set collection element-pin hint (wrong
/// domain); the mismatch error itself is unchanged. `Set.add`'s hint still fires.
#[test]
fn atomic_add_mismatch_no_collection_hint() {
    let errs = check_entry("import std.concurrency\na := Atomic(0)\na.add(1.5)\n");
    let msg = errs
        .iter()
        .map(|e| e.message.as_str())
        .find(|m| m.contains("expected int, found float"))
        .expect("Atomic.add float mismatch should be reported");
    assert!(
        !msg.contains("List[<protocol>]"),
        "Atomic.add hint should NOT mention the List collection pin: {msg:?}"
    );
    // Set.add's collection hint STILL fires (a Set first-use-pinned to int, mismatched later add).
    rejects(
        "fn main():\n    s := Set()\n    s.add(1)\n    s.add(\"x\")\nmain()\n",
        "List[<protocol>]",
    );
}

// ===== `ref` keyword / `Ref[T]` box / `import std.ref` REMOVED (minimalism cleanup) =====
// Boundary pin: after removal, none of the three surfaces exist. Each program must FAIL to compile
// with a clean error (a resolve/parse/type error, never a panic), not silently work.
#[test]
fn ref_surface_removed_fails_to_compile() {
    fn compile_fails(src: &str) -> String {
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        match crate::resolver::build_graph(&entry) {
            Err(e) => format!("{e:?}"),
            Ok(graph) => match check_graph(&graph) {
                Ok(()) => String::new(),
                Err(errs) => format!("{errs:?}"),
            },
        }
    }
    for src in [
        "import std.ref\nfn main():\n    print(1)\nmain()\n",
        "fn main():\n    x: ref int = 0\n    print(x)\nmain()\n",
        "fn main():\n    x: Ref[int] = 0\n    print(x)\nmain()\n",
    ] {
        let msg = compile_fails(src);
        assert!(!msg.is_empty(), "expected a compile error for: {src:?}");
    }
}

// ===== FIX 1a — member-level turbofish on a RESERVED built-in receiver whose harvested method
// declares its OWN `[U]` params (`List.map`) is ACCEPTED, not rejected "takes no type argument(s)".
// Before the fix `method_has_own_type_params` fell to `_ => false` for reserved receivers, so the
// turbofish gate rejected even a shipped generic method. =====
#[test]
fn reserved_receiver_generic_method_turbofish_ok() {
    ok("print([1, 2, 3].map[int](fn(x): x * 2))\n");
}

/// FIX 1a boundary — a NON-generic reserved-receiver method still rejects a turbofish (`List.filter`
/// declares no own `[U]`). The fix must not blanket-accept turbofish on all reserved methods.
#[test]
fn reserved_receiver_nongeneric_method_turbofish_rejected() {
    rejects(
        "print([1, 2, 3].filter[int](fn(x): x > 1))\n",
        "takes no type argument(s)",
    );
}

// ===== FIX 1b — a BODIED generic method on a concurrency handle (`Executor.submit_result[T]`) opens
// its own `[T]` and infers T from the closure return, instead of failing "expected fn()->T, found
// fn()->int". Before the fix the `Ty::Executor` arm only `native_handle_method`+`check_args_range`,
// so a harvested sig carrying `[T]` never routed through `infer_generic_method`. =====
#[test]
fn executor_bodied_generic_method_infers_from_closure() {
    entry_ok(
        "import std.concurrency\n\
         fn main():\n\
         \x20   ex := Executor()\n\
         \x20   ch := ex.submit_result(fn() -> int: 5)\n\
         \x20   ex.shutdown()\n\
         \x20   print(ch.recv())\n\
         main()\n",
    );
}
