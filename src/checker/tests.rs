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
    rejects(&format!("{MAX_FN}x: str = max[int](3, 7)\n"), "cannot assign int");
}

#[test]
fn explicit_type_args_mismatch_rejected() {
    // T pinned to str, but the args are int → argument type error.
    rejects(&format!("{MAX_FN}x := max[str](3, 7)\n"), "expected str");
}

#[test]
fn explicit_type_args_wrong_count_rejected() {
    rejects(&format!("{MAX_FN}x := max[int, int](3, 7)\n"), "expects 1 type argument");
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
fn redeclaring_stringable_rejected() {
    rejects("protocol Stringable:\n    fn str(self) -> str\n", "reserved");
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
    ok(&format!("{POINT_H}m: map[Point, str] = {{}}\nm[Point(1, 2)] = \"a\"\n"));
}

#[test]
fn set_of_hashable_struct_ok() {
    ok(&format!("{POINT_H}s: set[Point] = set()\ns.add(Point(1, 2))\n"));
}

#[test]
fn struct_without_hash_rejected_as_map_key() {
    let src = "struct Bare:\n    a: int\nm: map[Bare, int] = {}\n";
    rejects(src, "map key type must implement Hashable");
}

#[test]
fn struct_without_hash_rejected_as_set_element() {
    let src = "struct Bare:\n    a: int\ns: set[Bare] = set()\n";
    rejects(src, "set element type must implement Hashable");
}

#[test]
fn float_still_rejected_as_map_key() {
    rejects("m: map[float, int] = {}\n", "map key type must implement Hashable");
}

#[test]
fn float_still_rejected_as_set_element() {
    rejects("s: set[float] = set()\n", "set element type must implement Hashable");
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
    ok("fn keyed[T: Hashable](v: T) -> T:\n    return v\na := keyed(3)\nb := keyed(\"x\")\nc := keyed(true)\n");
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
    ok(&format!("{VEC2}v := Vec2(1, 2) + Vec2(3, 4) * Vec2(5, 6)\n"));
}

#[test]
fn struct_without_sub_rejects_minus() {
    // Vec2 defines add/mul but not sub ⇒ `-` is not overloaded.
    rejects(&format!("{VEC2}v := Vec2(1, 2) - Vec2(3, 4)\n"), "cannot apply - to Vec2 and Vec2");
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

#[test]
fn redeclaring_add_rejected() {
    rejects("protocol Add:\n    fn add(self, other: Self) -> Self\n", "reserved");
}

// ----- transparent type aliases (M10-G3) -----

#[test]
fn type_alias_transparent_ok() {
    // UserId ≡ int: usable interchangeably in annotations and calls.
    ok("type UserId = int\nfn double(n: int) -> int:\n    return n * 2\nid: UserId = 5\nx: int = id\ny := double(id)\n");
}

#[test]
fn type_alias_mismatch_still_rejected() {
    // The alias is transparent, so a str where the underlying int is expected is still an error
    // (and the message names the resolved type, `int`).
    rejects("type UserId = int\nid: UserId = \"no\"\n", "cannot assign str to variable of type int");
}

#[test]
fn type_alias_to_collection_ok() {
    ok("type Scores = map[str, int]\ns: Scores = {\"a\": 1}\nn: int = s[\"a\"]\n");
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
    ok(&format!("{BOXMAP}b := Box(5)\ns: str = b.map_to(fn(x: int) -> str: \"n{{x}}\")\n"));
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
    items: list[int]
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
    let src = format!(
        "{CONTAINER}fn first[X: Container[int]](c: X) -> str:\n    return c.get(0)\n"
    );
    rejects(&src, "int");
}

#[test]
fn param_protocol_bound_arity_mismatch_rejected() {
    // Container takes one type argument; a bare `Container` bound is an arity error.
    let src = format!(
        "{CONTAINER}fn first[X: Container](c: X) -> int:\n    return c.get(0)\n"
    );
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
    items: list[int]
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
    items: list[str]
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
    items: list[str]
    fn get(self, i: int) -> str:
        return self.items[i]
    fn size(self) -> int:
        return len(self.items)
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
    items: list[int]
    fn get(self, i: int) -> int:
        return self.items[i]
    fn size(self) -> int:
        return len(self.items)
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
    // Review-panel IMPORTANT: a generic method whose first param isn't a receiver must be rejected,
    // not silently bind the receiver to the first declared param.
    let src = "\
struct Box[T]:
    v: T
    fn ident[U]() -> U:
        pass
b := Box(5)
r := b.ident()
";
    rejects(src, "receiver");
}

#[test]
fn param_protocol_as_value_type_rejected() {
    // A parameterized protocol may only be a bound, not an existential value type (out of scope).
    let src = "\
protocol Container[T]:
    fn get(self, i: int) -> T
fn bad(c: Container[int]) -> int:
    return c.get(0)
";
    rejects(src, "value type");
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
    ok(&format!("{TREE}t: Tree[int] = Node(1, Leaf, Leaf)\n"));
}

#[test]
fn generic_enum_construction_type_mismatch_rejected() {
    // First payload is T; an int and a str in the two Node arms can't both be T.
    rejects(&format!("{TREE}t := Node(1, Node(\"x\", Leaf, Leaf), Leaf)\n"), "expected");
}

#[test]
fn generic_enum_annotation_arg_mismatch_rejected() {
    // A Tree[str] slot can't hold a Node whose payload infers T=int.
    rejects(&format!("{TREE}t: Tree[str] = Node(1, Leaf, Leaf)\n"), "cannot assign");
}

#[test]
fn generic_enum_match_substitutes_payload_ok() {
    // The `v` bound by `Node(v, ...)` of a `Tree[int]` is int.
    let src = format!(
        "{TREE}fn first(t: Tree[int]) -> int:\n    match t:\n        Leaf: return 0\n        Node(v, l, r): return v\n"
    );
    ok(&src);
}

#[test]
fn generic_enum_match_payload_type_enforced() {
    // The `v` bound by `Node(v, ...)` of a `Tree[int]` is int, not str.
    let src = format!(
        "{TREE}fn bad(t: Tree[int]):\n    match t:\n        Leaf: print(\"l\")\n        Node(v, l, r):\n            s: str = v\n"
    );
    rejects(&src, "cannot assign int");
}

#[test]
fn generic_enum_wrong_arity_rejected() {
    rejects(&format!("{TREE}t: Tree[int, str] = Leaf\n"), "expects 1 type argument(s)");
}

#[test]
fn bare_generic_enum_without_args_rejected() {
    rejects(&format!("{TREE}fn f(t: Tree) -> int:\n    return 0\n"), "expects 1 type argument(s), got 0");
}

#[test]
fn generic_enum_multi_param_ok() {
    let src = "\
enum Either[A, B]:
    Left(A)
    Right(B)
fn fst(e: Either[int, str]) -> int:
    match e:
        Left(a): return a
        Right(b): return 0
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
        Nil: return 0
        Cons(h, t): return 1
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
b := Has(Plain(1))
";
    rejects(src, "does not satisfy Comparable");
}

#[test]
fn generic_enum_bound_satisfied_ok() {
    let src = "\
enum Box[T: Comparable]:
    Empty
    Has(T)
b: Box[int] = Has(5)
";
    ok(src);
}

#[test]
fn generic_enum_unknown_bound_rejected() {
    rejects("enum Box[T: Nope]:\n    Has(T)\n", "unknown protocol 'Nope'");
}

#[test]
fn struct_and_enum_sharing_a_name_rejected() {
    // Review (Solidity lens): a struct and enum with the same name both registered silently,
    // the enum shadowed; with the merged `Name[args]` Display this surfaced as a nonsense
    // "cannot assign Foo[int] to … Foo[int]". Must be a clean "already defined" instead.
    rejects("struct Foo:\n    n: int\nenum Foo:\n    A\n", "type 'Foo' is already defined");
    rejects("enum Bar:\n    A\nstruct Bar:\n    n: int\n", "type 'Bar' is already defined");
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
fn list_concat_returns_list() {
    ok("xs := [1, 2]\nys := xs.concat([3, 4])\nn: int = ys[0]\nprint(n)\n");
}

#[test]
fn list_extend_returns_nil() {
    ok("xs := [1, 2]\nxs.extend([3, 4])\nprint(xs.len())\n");
}

#[test]
fn list_concat_element_type_checked() {
    rejects("xs := [1, 2]\nys := xs.concat([\"a\"])\n", "argument 1 of 'concat'");
}

#[test]
fn list_extend_element_type_checked() {
    rejects("xs := [1, 2]\nxs.extend([\"a\"])\n", "argument 1 of 'extend'");
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
fn match_nested_nullary_variant_ok() {
    // `Cons(h, Nil)`: a nested nullary variant is now a refutable variant match (the checker
    // promotes the bare `Nil`). Previously rejected (gap #15 limit); now supported.
    let src = "enum L:\n    Nil\n    Cons(int, L)\n\
               fn f(x: L):\n    match x:\n        Cons(h, Nil): print(h)\n        _: print(\"e\")\n";
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
    rejects("xs := [1,2,3]\nfor a, b in xs:\n    print(a)\n", "requires a map");
}

#[test]
fn for_tuple_list_binds_each_element() {
    ok("xs := [(1, \"a\"), (2, \"b\")]\nfor n, s in xs:\n    i: int = n\n    t: str = s\n    print(\"{i}{t}\")\n");
}

#[test]
fn for_tuple_list_one_var_binds_whole_tuple() {
    ok("xs := [(1, \"a\")]\nfor p in xs:\n    i: int = p.0\n    s: str = p.1\n    print(\"{i}{s}\")\n");
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
    rejects("xs := [(1, \"a\")]\nfor n, s in xs:\n    bad: int = s\n", "");
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
    rejects("m := {\"a\": 1}\nfor k, v in m:\n    k = \"z\"\n", "loop variable");
}

#[test]
fn for_map_value_reassign_rejected() {
    rejects("m := {\"a\": 1}\nfor k, v in m:\n    v = 9\n", "loop variable");
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
    rejects("for i in 0..2:\n    for i in 0..2:\n        i = 9\n", "loop variable");
}

#[test]
fn reassign_after_loop_is_undeclared_not_loop_var() {
    // The loop var doesn't leak past the loop; assigning it afterward is plain-undeclared.
    let errs = check_src("for i in 0..3:\n    print(i)\ni = 5\n");
    assert!(
        errs.iter().any(|e| e.message.contains("undeclared variable")),
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
    rejects("m := {1.0: 2}\n", "must implement Hashable");
}

#[test]
fn float_map_key_annotation_rejected() {
    rejects("m: map[float, int] = {}\n", "must implement Hashable");
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
    rejects("a := {\"x\": 1}\nb := {\"y\": \"s\"}\nc := a.merge(b)\n", "argument 1 of 'merge'");
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
fn list_comprehension_infers_element_type() {
    // `[x * 2 for x in [1, 2, 3]]` is a `list[int]` — the loop var binds to the list's element.
    ok("xs: list[int] = [x * 2 for x in [1, 2, 3]]\n");
}

#[test]
fn list_comprehension_wrong_element_type_rejected() {
    rejects("xs: list[str] = [x * 2 for x in [1, 2, 3]]\n", "list[int]");
}

#[test]
fn comprehension_guard_must_be_bool() {
    rejects("xs := [x for x in [1, 2, 3] if x]\n", "comprehension guard must be bool");
}

#[test]
fn list_comprehension_over_range_is_list_int() {
    ok("xs: list[int] = [x * x for x in 0..10]\n");
}

#[test]
fn set_comprehension_infers_element_type() {
    ok("s: set[int] = {x for x in [1, 2, 3]}\n");
}

#[test]
fn map_comprehension_over_map_entries() {
    ok("src: map[str, int] = {\"a\": 1}\nm: map[str, int] = {k: v for k, v in src}\n");
}

#[test]
fn comprehension_var_out_of_scope_after() {
    // The loop variable is scoped to the comprehension; referencing it afterward is unknown.
    rejects("xs := [x for x in [1, 2, 3]]\nprint(x)\n", "x");
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

// ===== M9: std.regex (Match struct) =====

#[test]
fn native_regex_is_match_returns_result_bool() {
    entry_ok("import std.regex\nfn main():\n    match regex.is_match(\"x\", \"xy\"):\n        Ok(b):\n            if b:\n                print(\"yes\")\n        Err(e): print(e)\n");
}

#[test]
fn native_regex_find_returns_match_with_typed_fields() {
    entry_ok("import std.regex\nfn main():\n    match regex.find(\"[0-9]+\", \"a12\"):\n        Ok(opt):\n            match opt:\n                Some(m):\n                    t: str = m.text\n                    st: int = m.start\n                    g: list[str] = m.groups\n                    print(t + str(st) + \",\".join(g))\n                None: print(\"none\")\n        Err(e): print(e)\n");
}

#[test]
fn native_regex_find_all_returns_result_list_match() {
    entry_ok("import std.regex\nfn main():\n    match regex.find_all(\"[0-9]+\", \"1 2\"):\n        Ok(ms):\n            for m in ms:\n                print(m.text)\n        Err(e): print(e)\n");
}

#[test]
fn native_regex_split_and_replace_all_return_strings() {
    entry_ok("import std.regex\nfn main():\n    match regex.replace_all(\"a\", \"banana\", \"o\"):\n        Ok(s): print(s)\n        Err(e): print(e)\n    match regex.split(\",\", \"a,b\"):\n        Ok(xs): print(\"|\".join(xs))\n        Err(e): print(e)\n");
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

// ===== M9: std.request (Response struct) =====

#[test]
fn native_request_get_returns_response_with_typed_fields() {
    entry_ok("import std.request\nfn main():\n    match request.get(\"http://x\"):\n        Ok(resp):\n            st: int = resp.status\n            body: str = resp.body\n            h: map[str, str] = resp.headers\n            print(body + str(st) + h[\"k\"])\n        Err(e): print(e)\n");
}

#[test]
fn native_request_post_takes_url_and_body() {
    entry_ok("import std.request\nfn main():\n    match request.post(\"http://x\", \"payload\"):\n        Ok(resp): print(str(resp.status))\n        Err(e): print(e)\n");
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

// ===== M8-M4: set type =====

#[test]
fn set_literal_infers_set_of_elem() {
    ok("s: set[int] = {1, 2, 3}\nprint(s.len())\n");
}

#[test]
fn set_methods_typecheck() {
    ok("s := {1, 2}\nb: bool = s.has(1)\ns.add(3)\nr: bool = s.remove(1)\nu: set[int] = s.union({4})\nprint(u.len())\n");
}

#[test]
fn set_builtin_empty_and_from_list() {
    ok("e := set()\ne.add(\"x\")\nf: set[int] = set([1, 1, 2])\nprint(f.len())\n");
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
    rejects("s := {1, 2}\nx := s[0]\n", "cannot index into set");
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
    ok("fn q() -> Result[int, str]:\n    return Err(\"bad\")\nfn main():\n    match q():\n        Ok(v): print(v)\n        Err(e): print(e.trim())\nmain()\n");
}

#[test]
fn custom_struct_error_ok() {
    ok("struct DbErr:\n    code: int\n    fn message(self) -> str:\n        return \"db\"\nfn q() -> int!DbErr:\n    return Err(DbErr(503))\n");
}

#[test]
fn error_protocol_existential_accepts_str() {
    // `Error` used as a value type; `str` conforms; only `message()` is available on it.
    ok("fn q() -> Result[int, Error]:\n    return Err(\"bad\")\nfn main():\n    match q():\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()\n");
}

#[test]
fn bang_default_error_is_error_protocol() {
    // `T!` defaults `E` to the `Error` protocol; the payload supports `.message()`.
    ok("fn q() -> int!:\n    return Err(\"bad\")\nfn main():\n    match q():\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()\n");
}

#[test]
fn default_error_existential_rejects_str_methods() {
    // `Error` existential exposes only `message()` — not `str`'s methods.
    rejects("fn q() -> int!:\n    return Err(\"x\")\nfn main():\n    match q():\n        Ok(v): print(v)\n        Err(e): print(e.trim())\nmain()\n", "trim");
}

#[test]
fn struct_error_without_message_rejected_as_error() {
    // A struct lacking `message(self) -> str` does not satisfy `Error`, so it can't be the
    // payload where `Error` is expected — the return-type check flags the mismatch.
    rejects("struct Bad:\n    n: int\nfn q() -> Result[int, Error]:\n    return Err(Bad(1))\n", "Bad");
}

// ===== recover: boundary (M11 Phase B) =====

#[test]
fn recover_yields_result_of_block_value() {
    // `recover:` evaluates to Result[T, Error]; matching Ok/Err is well-typed.
    ok("fn main():\n    r := recover:\n        [1, 2][0]\n    match r:\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()\n");
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
    ok("fn main():\n    r := recover:\n        for i in 0..3:\n            if i == 1: break\n        42\n    match r:\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()\n");
}

#[test]
fn recover_question_mark_allowed_in_non_result_fn() {
    // `?` targets the recover boundary, so the enclosing fn need not return Result.
    ok("fn risky() -> int!:\n    return Err(\"x\")\nfn compute() -> str:\n    r := recover:\n        v := risky()?\n        v\n    match r:\n        Ok(v): return \"ok\"\n        Err(e): return e.message()\n");
}

#[test]
fn recover_question_mark_on_option_rejected() {
    rejects(
        "fn find() -> int?:\n    return None\nfn main():\n    r := recover:\n        v := find()?\n        v\n    print(r)\nmain()\n",
        "Option is not allowed inside a recover block",
    );
}

// ===== `?` inside a closure is checked against the closure's return (soundness fix) =====

#[test]
fn closure_question_mark_on_nonresult_return_rejected() {
    // A closure declared `-> int` may not use `?` — it would leak an Err into a list[int].
    rejects(
        "fn parse(s: str) -> int!:\n    return Err(\"x\")\nfn main():\n    ys := [\"2\"].map(fn(s: str) -> int: parse(s)? * 2)\n    print(ys)\nmain()\n",
        "not Result or Option",
    );
}

#[test]
fn closure_question_mark_on_result_return_ok() {
    // A closure declared to return Result may use `?` (yields the Ok type).
    ok("fn parse(s: str) -> int!:\n    return Ok(2)\nfn main():\n    rs := [\"2\"].map(fn(s: str) -> int!: Ok(parse(s)? * 2))\n    print(rs)\nmain()\n");
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
    ok("struct Counter:\n    n: int\n    limit: int\n    fn next(self) -> Option[int]:\n        if self.n >= self.limit:\n            return None\n        v := self.n\n        self.n = self.n + 1\n        return Some(v)\nfn main():\n    for x in Counter(0, 5):\n        print(x)\nmain()\n");
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
fn match_guard_ok() {
    // A guard sees the pattern's bindings; with a trailing `_` the match is exhaustive.
    ok("fn classify(n: int) -> str:\n    return match n:\n        x if x < 0: \"neg\"\n        0: \"zero\"\n        _: \"pos\"\nclassify(1)\n");
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
    ok("fn grade(n: int) -> str:\n    return match n:\n        0..60: \"F\"\n        60..90: \"B\"\n        _: \"A\"\ngrade(50)\n");
}

// ===== default + named arguments (end-to-end through desugar) =====

#[test]
fn default_arg_typechecks_ok() {
    // The omitted `y` is filled with its default (10:int) before checking.
    entry_ok("fn f(x: int, y: int = 10) -> int:\n    return x + y\nfn main():\n    print(f(1))\nmain()\n");
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
    entry_ok("fn f(x: int, y: int = 7, s: str = \"hi\") -> int:\n    return x + y\nfn main():\n    print(f(1))\nmain()\n");
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
    ok("fn first[S: Iterator[T], T](xs: S, d: T) -> T:\n    for x in xs:\n        return x\n    return d\nv := first([1, 2, 3], 0)\n");
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
    rejects("protocol Iterator:\n    fn next(self) -> int?\n", "reserved");
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
    ok("fn count[S: Iterator[T], T](xs: S) -> int:\n    n := 0\n    for _ in xs:\n        n = n + 1\n    return n\nfn wrap[S: Iterator[T], T](xs: S) -> int:\n    return count(xs)\nv := wrap([1, 2, 3])\n");
}

#[test]
fn iterator_conflicting_explicit_element_arg_rejected() {
    // Explicit `[list[int], str]` pins T=str, but the list element is int — the recovered element
    // must conflict (unsound otherwise: static list[str], runtime list[int]).
    rejects(
        "fn to_list[S: Iterator[T], T](xs: S) -> list[T]:\n    out := []\n    for x in xs:\n        out.push(x)\n    return out\nr := to_list[list[int], str]([1, 2, 3])\n",
        "does not match the declared element type",
    );
}

#[test]
fn iterator_bound_unknown_element_type_rejected() {
    // `Bogus` is neither a declared type param nor a known type — a bound's args are resolved, so
    // this is reported rather than silently accepted.
    rejects(
        "fn f[S: Iterator[Bogus]](xs: S):\n    print(1)\n",
        "Bogus",
    );
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
    ok("xs := [1, 2, 3, 4]\nys: list[int] = xs[1..3]\n");
    rejects(
        "xs := [1, 2, 3, 4]\nys: str = xs[1..3]\n",
        "cannot assign list[int] to variable of type str",
    );
}

#[test]
fn slice_of_str_types_as_str() {
    ok("s := \"hello\"\nt: str = s[0..2]\n");
    rejects("s := \"hello\"\nn: int = s[0..2]\n", "cannot assign str to variable of type int");
}

#[test]
fn slice_bounds_must_be_int() {
    rejects("xs := [1, 2, 3]\nys := xs[\"a\"..2]\n", "slice bound must be int, found str");
    rejects("xs := [1, 2, 3]\nys := xs[0..\"b\"]\n", "slice bound must be int, found str");
}

#[test]
fn map_is_not_sliceable() {
    rejects(
        "m: map[int, int] = {}\nx := m[0..2]\n",
        "cannot slice",
    );
}

const BUF: &str = "\
struct Buf:
    xs: list[int]
    fn index(self, key: int) -> int:
        return self.xs[key]
    fn set_index(self, key: int, val: int):
        self.xs[key] = val
    fn slice(self, start: int, end: int) -> Buf:
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
    ok(&format!("{BUF}b := Buf([1, 2, 3])\nc: Buf = b[0..2]\n"));
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
    xs: list[int]
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
    xs: list[int]
    fn index(self, key: int) -> int:
        return self.xs[key]
";
    rejects(
        &format!("{no_slice}b := NS([1, 2, 3])\nc := b[0..2]\n"),
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
    xs: list[int]
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
    // The `Slice` protocol fixes the bounds as `slice(self, int, int)` — both engines pass int
    // start/end. A `slice` with non-int bounds must NOT count as a valid `Slice` impl (would crash).
    let bad = "\
struct BadSlice:
    xs: list[int]
    fn slice(self, start: str, end: str) -> int:
        return self.xs.len()
";
    rejects(
        &format!("{bad}b := BadSlice([1, 2, 3])\nc := b[0..2]\n"),
        "cannot slice BadSlice",
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
        "{BUF}fn mid[C: Slice[R], R](c: C) -> R:\n    return c[1..2]\n\
         b := Buf([1, 2, 3])\nc: Buf = mid(b)\nd: list[int] = mid([1, 2, 3])\n"
    ));
}

// ===== defer =====

#[test]
fn defer_method_call_ok() {
    ok("struct F:\n    n: int\n    fn close(self):\n        print(\"x\")\nfn w():\n    f := F(1)\n    defer f.close()\n");
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
    rejects("fn w():\n    defer 1 + 2\n", "defer requires a function or method call");
}

#[test]
fn defer_builtin_rejected() {
    // Built-ins are not first-class values — they must be wrapped in a function.
    rejects("fn w():\n    defer print(\"x\")\n", "built-ins and constructors must be wrapped");
}

#[test]
fn defer_constructor_rejected() {
    rejects(
        "struct P:\n    x: int\nfn w():\n    defer P(1)\n",
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
    rejects("fn w():\n    defer:\n        z := nope + 1\n", "unknown name 'nope'");
}

#[test]
fn defer_block_reassign_capture_rejected() {
    // Writing back through the by-value snapshot is rejected (the VM has no `SetCaptured` op; the
    // interp would write a discarded copy — allowing it would crash one engine and no-op the other).
    rejects(
        "fn w():\n    x := 1\n    defer:\n        x = 2\n",
        "cannot reassign captured binding 'x' inside a defer: block",
    );
}

#[test]
fn defer_block_new_binding_and_nonsendable_read_ok() {
    // Reading a capture into a NEW binding is fine, and — unlike a `spawn:` block — reading a
    // non-sendable captured value (a closure) is allowed (same task, no airlock).
    ok("fn w():\n    x := 1\n    g := fn(): print(\"g\")\n    defer:\n        y := x + 1\n        print(\"{y}\")\n        g()\n");
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
    ok("struct M:\n    n: int\n    fn compare(self, o: M) -> int:\n        return self.n - o.n\nxs := [M(2), M(1)]\nxs.sort_by_key(fn(m: M) -> M: m)\n");
}

#[test]
fn sort_by_key_non_comparable_key_rejected() {
    // A key function returning a non-Comparable struct (no `compare`) is rejected.
    rejects(
        "struct B:\n    n: int\nxs := [B(2), B(1)]\nxs.sort_by_key(fn(b: B) -> B: b)\n",
        "sort_by_key key type must be Comparable",
    );
}

#[test]
fn sort_by_key_wrong_arity_rejected() {
    rejects(
        "xs := [1, 2]\nxs.sort_by_key(fn(a: int, b: int) -> int: a - b)\n",
        "sort_by_key expects a key function",
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
    ok("fn w():\n    print(1)\nfn main():\n    parallel:\n        spawn w()\n        spawn w()\nmain()\n");
}

#[test]
fn nested_parallel_ok() {
    ok("fn w():\n    print(1)\nfn main():\n    parallel:\n        parallel:\n            spawn w()\nmain()\n");
}

#[test]
fn spawn_block_form_ok() {
    ok("fn main():\n    parallel:\n        spawn:\n            print(1)\nmain()\n");
}

// ----- concurrency C2: Channel[T] + sendability -----

#[test]
fn channel_construct_and_methods_ok() {
    ok("fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    x := ch.recv()\n    n := ch.len()\n    print(x + n)\nmain()\n");
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
fn channel_non_sendable_element_rejected() {
    rejects(
        "fn main():\n    ch := Channel[fn() -> int]()\n    print(ch.len())\nmain()\n",
        "Channel element type must be sendable",
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
    ok("fn main():\n    ch := Channel[int]()\n    sent: bool = ch.try_send(1)\n    print(sent)\nmain()\n");
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
    ok("fn main():\n    ch := Channel[int]()\n    ch.close()\n    for v in ch:\n        print(v + 1)\nmain()\n");
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
fn spawn_non_sendable_arg_rejected() {
    rejects(
        "fn run(f: fn() -> int):\n    print(f())\nfn main():\n    g := fn(): 1\n    parallel:\n        spawn run(g)\nmain()\n",
        "non-sendable value of type fn() -> int",
    );
}

#[test]
fn spawn_sendable_args_ok() {
    ok("fn worker(id: int, prefix: str, out: Channel[str]):\n    out.send(\"{prefix}-{id}\")\nfn main():\n    ch := Channel[str]()\n    parallel:\n        spawn worker(1, \"t\", ch)\nmain()\n");
}

#[test]
fn spawn_builtin_rejected_like_defer() {
    rejects(
        "fn main():\n    parallel:\n        spawn print(\"hi\")\nmain()\n",
        "spawn requires a function or method call",
    );
}

#[test]
fn spawn_bad_arg_reports_one_error() {
    // The sendability gate must not double-report a type error already raised by inferring the call.
    let errs = check_src("fn w(x: int):\n    print(x)\nfn main():\n    parallel:\n        spawn w(nope)\nmain()\n");
    let dups = errs.iter().filter(|e| e.message.contains("unknown name 'nope'")).count();
    assert_eq!(dups, 1, "expected exactly one 'unknown name' error, got: {errs:?}");
}

#[test]
fn channel_non_sendable_struct_field_rejected() {
    // A closure smuggled inside a struct field must be caught (deep field sendability).
    rejects(
        "struct Holder:\n    f: fn() -> int\nfn main():\n    ch := Channel[Holder]()\n    print(ch.len())\nmain()\n",
        "Channel element type must be sendable",
    );
}

#[test]
fn spawn_non_sendable_struct_field_arg_rejected() {
    rejects(
        "struct Holder:\n    f: fn() -> int\nfn run_it(h: Holder):\n    print(h.f())\nfn main():\n    bump := fn() -> int: 99\n    h := Holder(bump)\n    parallel:\n        spawn run_it(h)\nmain()\n",
        "non-sendable value of type Holder",
    );
}

#[test]
fn sendable_recursive_struct_ok() {
    // A self-referential struct of sendable fields must terminate (cycle guard) and be sendable.
    ok("struct Node:\n    val: int\n    next: Node\nfn use_it(n: Node):\n    print(n.val)\nfn main():\n    parallel:\n        spawn:\n            print(1)\nmain()\n");
}

#[test]
fn reassign_captured_binding_in_spawn_block_rejected() {
    rejects(
        "fn main():\n    counter := 0\n    parallel:\n        spawn:\n            counter = counter + 1\nmain()\n",
        "cannot reassign captured binding 'counter'",
    );
}

#[test]
fn task_local_binding_in_spawn_block_assignable() {
    // A binding declared *inside* the task body is task-local, not a capture — assignable.
    ok("fn main():\n    parallel:\n        spawn:\n            x := 0\n            x = x + 1\n            print(x)\nmain()\n");
}

#[test]
fn spawn_in_plain_fn_ok() {
    // M-C: a `spawn` in a function with no explicit `parallel:` is legal — the function body is an
    // implicit nursery that joins at the function's end. The function-boundary rule still holds at
    // runtime (the task binds to *this* function's nursery, never the caller's), enforced by the
    // compiler/VM emitting a per-function implicit nursery.
    ok("fn w():\n    spawn other()\nfn other():\n    print(1)\nfn main():\n    parallel:\n        w()\nmain()\n");
}

// ----- concurrency C3: Shared[T], the cross-task mutable box -----

#[test]
fn shared_construct_and_methods_ok() {
    // `Shared(v)` infers its element type from the value (no `[T]` type arg, unlike Channel).
    ok("fn main():\n    s := Shared(0)\n    s.set(5)\n    s.update(fn(x): x + 1)\n    print(s.get())\nmain()\n");
}

#[test]
fn shared_get_returns_element_type() {
    // `get()` yields `T`, so it must compose where a `T` is expected (here, str concat).
    ok("fn main():\n    s := Shared(\"hi\")\n    msg := s.get() + \"!\"\n    print(msg)\nmain()\n");
}

#[test]
fn shared_set_wrong_type_rejected() {
    rejects(
        "fn main():\n    s := Shared(0)\n    s.set(\"x\")\nmain()\n",
        "expected int",
    );
}

#[test]
fn shared_update_fn_arity_rejected() {
    // `update` takes `fn(T) -> T`; a two-param closure must not type-check.
    rejects(
        "fn main():\n    s := Shared(0)\n    s.update(fn(x, y): x + y)\nmain()\n",
        "argument 1 of 'update'",
    );
}

#[test]
fn shared_rejects_type_arg() {
    // The element type comes from the value — `Shared[int](...)` is not the constructor form.
    rejects(
        "fn main():\n    s := Shared[int](0)\n    print(s.get())\nmain()\n",
        "'Shared' takes no type arguments",
    );
}

#[test]
fn shared_is_sendable() {
    // A `Shared[T]` handle crosses the airlock — both spawned tasks reach the same box.
    ok("fn bump(s: Shared[int]):\n    s.update(fn(x): x + 1)\nfn main():\n    s := Shared(0)\n    parallel:\n        spawn bump(s)\n        spawn bump(s)\n    print(s.get())\nmain()\n");
}

#[test]
fn shared_handle_sendable_regardless_of_element() {
    // The asymmetry vs Channel: a `Shared` handle is sendable even when its element type isn't
    // (the value never crosses the airlock — only the handle does). Locks in the intent.
    ok("fn use_it(s: Shared[fn() -> int]):\n    f := s.get()\n    print(f())\nfn main():\n    g := fn() -> int: 1\n    s := Shared(g)\n    parallel:\n        spawn use_it(s)\nmain()\n");
}

#[test]
fn ref_is_not_sendable() {
    // `Ref[T]` is the *in-task* box (std.ref); passing it across a spawn would silently copy it,
    // so the checker rejects it — the cross-task box is `Shared[T]`. (Spec §7.)
    entry_rejects(
        "import std.ref\nfn bump(r: Ref[int]):\n    r.set(r.get() + 1)\nfn main():\n    r := Ref(0)\n    parallel:\n        spawn bump(r)\nmain()\n",
        "non-sendable value of type Ref[int]",
    );
}

// ----- Atomic[T]: the generic atomic box -----

#[test]
fn atomic_construct_and_methods_ok() {
    // `Atomic(v)` infers its element type from the value (value-first, like `Shared`).
    ok("fn main():\n    a := Atomic(0)\n    a.store(5)\n    n := a.add(1)\n    m := a.sub(2)\n    old := a.exchange(9)\n    ok := a.cas(9, 10)\n    print(a.load())\nmain()\n");
}

#[test]
fn atomic_load_returns_element_type() {
    // `load()` yields `T`, so it composes where a `T` is expected (here, str concat).
    ok("fn main():\n    a := Atomic(\"hi\")\n    msg := a.load() + \"!\"\n    print(msg)\nmain()\n");
}

#[test]
fn atomic_cas_returns_bool() {
    // `cas(expected, new)` reports whether the swap happened.
    ok("fn main():\n    a := Atomic(0)\n    if a.cas(0, 1):\n        print(\"swapped\")\nmain()\n");
}

#[test]
fn atomic_store_wrong_type_rejected() {
    rejects(
        "fn main():\n    a := Atomic(0)\n    a.store(\"x\")\nmain()\n",
        "expected int",
    );
}

#[test]
fn atomic_add_non_numeric_rejected() {
    // `add`/`sub` are arithmetic — only `int`/`float` boxes have them.
    rejects(
        "fn main():\n    a := Atomic(\"x\")\n    a.add(1)\nmain()\n",
        "no method 'add'",
    );
}

#[test]
fn atomic_rejects_type_arg() {
    // The element type comes from the value — `Atomic[int](...)` is not the constructor form.
    rejects(
        "fn main():\n    a := Atomic[int](0)\n    print(a.load())\nmain()\n",
        "'Atomic' takes no type arguments",
    );
}

#[test]
fn atomic_is_sendable() {
    // An `Atomic[T]` handle crosses the airlock — both spawned tasks reach the same box.
    ok("fn bump(a: Atomic[int]):\n    a.add(1)\nfn main():\n    a := Atomic(0)\n    parallel:\n        spawn bump(a)\n        spawn bump(a)\n    print(a.load())\nmain()\n");
}

// ----- timer(ms): one-shot timeout channel -----

#[test]
fn timer_returns_channel_bool() {
    // `timer(ms)` yields a `Channel[bool]`; `recv()` on it composes where a `bool` is expected.
    ok("fn main():\n    t := timer(50)\n    if t.recv():\n        print(\"fired\")\nmain()\n");
}

#[test]
fn timer_arg_must_be_int() {
    rejects(
        "fn main():\n    t := timer(\"x\")\n    print(t.recv())\nmain()\n",
        "expected int",
    );
}

// ----- C5: the `Executor` escape hatch -----

#[test]
fn executor_construct_and_methods_ok() {
    ok("fn job():\n    print(1)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): job())\n    ex.shutdown()\nmain()\n");
}

#[test]
fn executor_shutdown_now_ok() {
    ok("fn main():\n    ex := Executor()\n    ex.shutdown_now()\nmain()\n");
}

#[test]
fn executor_defer_shutdown_ok() {
    ok("fn job():\n    print(1)\nfn main():\n    ex := Executor()\n    defer ex.shutdown()\n    ex.submit(fn(): job())\nmain()\n");
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
    rejects(
        "fn main():\n    ex := Executor()\n    ex.run()\nmain()\n",
        "has no method 'run'",
    );
}

#[test]
fn executor_is_sendable() {
    // The handle crosses the airlock like Channel/Shared — submitting from a spawned task is legal.
    ok("fn use_ex(ex: Executor):\n    ex.submit(fn(): print(1))\nfn main():\n    ex := Executor()\n    parallel:\n        spawn use_ex(ex)\n    ex.shutdown()\nmain()\n");
}

#[test]
fn executor_user_struct_named_executor_rejected() {
    rejects(
        "struct Executor:\n    n: int\nfn main():\n    print(1)\nmain()\n",
        "reserved",
    );
}

// ----- C5 refinement #1: a non-sendable value merely *read* inside a `spawn:` block -----

#[test]
fn read_captured_closure_in_spawn_block_rejected() {
    // Capturing a closure (non-sendable) and *calling* it inside a task is a read across the
    // airlock — rejected even though it's never reassigned (the gap closed in this milestone).
    rejects(
        "fn main():\n    g := fn() -> int: 1\n    parallel:\n        spawn:\n            print(g())\nmain()\n",
        "non-sendable captured binding 'g'",
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
    ok("fn main():\n    ch := Channel[int]()\n    parallel:\n        spawn:\n            ch.send(1)\nmain()\n");
}

#[test]
fn imported_module_used_in_spawn_block_ok() {
    // Regression: a whole-module import is bound at module scope (a global namespace resolvable in
    // every task, like a free function), not a per-task value capture — the read gate must not flag
    // it even though `Ty::Module` is non-sendable.
    entry_ok("import std.math\nfn main():\n    parallel:\n        spawn:\n            print(math.floor(2.7))\nmain()\n");
}

#[test]
fn top_level_closure_used_in_spawn_block_ok() {
    // Regression: a top-level (module-scope) binding is a global, not a per-task capture — reading
    // it inside a `spawn:` block is fine even when it's non-sendable.
    ok("g := fn() -> int: 7\nfn main():\n    parallel:\n        spawn:\n            print(g())\nmain()\n");
}

#[test]
fn read_captured_closure_through_nested_closure_in_spawn_block_rejected() {
    // (C5 / A2-session regression pin.) A non-sendable function-local closure smuggled into a
    // `spawn:` block through a *nested* closure must still be rejected. This is currently an
    // EMERGENT property, not a dedicated nested-closure walk: `capture_floors` is pushed only at the
    // `spawn:` boundary and is NOT reset by `infer_closure`, so the read gate in `infer_ident`
    // (`is_local_capture` + `!sendable`) fires at any closure-nesting depth. This test locks that
    // behavior so a future refactor of the capture machinery can't silently reopen the hole — see
    // `docs/concurrency.md` §9 (Group A / A3a).
    rejects(
        "fn main():\n    g := fn() -> int: 1\n    parallel:\n        spawn:\n            h := fn() -> int: g()\n            print(h())\nmain()\n",
        "non-sendable captured binding 'g'",
    );
}

// ----- A3b (B3.6): Executor.submit gates its closure captures like `spawn` -----

#[test]
fn submit_non_sendable_capture_rejected() {
    // A submitted closure reading a non-sendable captured binding (a Ref) crosses the airlock to a
    // pool thread under `--parallel` — rejected exactly like a `spawn` capture.
    entry_rejects(
        "import std.ref\nfn main():\n    r := Ref(0)\n    ex := Executor()\n    ex.submit(fn(): r.set(1))\n    ex.shutdown()\nmain()\n",
        "non-sendable captured binding 'r'",
    );
}

#[test]
fn submit_captured_closure_rejected() {
    // Capturing a function-local closure (non-sendable) and calling it inside the submitted task.
    rejects(
        "fn main():\n    g := fn() -> int: 1\n    ex := Executor()\n    ex.submit(fn(): print(g()))\n    ex.shutdown()\nmain()\n",
        "non-sendable captured binding 'g'",
    );
}

#[test]
fn submit_captured_channel_ok() {
    // A Channel handle is sendable — capturing it in a submitted task is fine.
    ok("fn main():\n    ch := Channel[int]()\n    ex := Executor()\n    ex.submit(fn(): ch.send(1))\n    ex.shutdown()\nmain()\n");
}

#[test]
fn submit_captured_int_ok() {
    // A sendable capture (int) gets its own copy — reading it in the task is the whole point.
    ok("fn main():\n    n := 42\n    ex := Executor()\n    ex.submit(fn(): print(n))\n    ex.shutdown()\nmain()\n");
}

#[test]
fn submit_captured_closure_through_nested_closure_rejected() {
    // Regression pin (mirrors `read_captured_closure_through_nested_closure_in_spawn_block_rejected`):
    // a non-sendable function-local closure smuggled into a submitted task through a *nested* closure
    // must still be rejected. Emergent from `capture_floors` not being reset by `infer_closure` — pin
    // it so a future refactor of the capture machinery can't silently reopen the hole.
    rejects(
        "fn main():\n    g := fn() -> int: 1\n    ex := Executor()\n    ex.submit(fn(): print((fn() -> int: g())()))\n    ex.shutdown()\nmain()\n",
        "non-sendable captured binding 'g'",
    );
}

#[test]
fn top_level_closure_submitted_ok() {
    // Regression pin (mirrors `top_level_closure_used_in_spawn_block_ok`): a module-scope binding is a
    // global, not a per-task capture — submitting a closure that reads it is fine even when it's
    // non-sendable (the `is_local_capture` scope-0 exclusion). Locks the intentional gap so a future
    // tightening of the gate can't silently flip it without a test failing.
    ok("g := fn() -> int: 7\nfn main():\n    ex := Executor()\n    ex.submit(fn(): print(g()))\n    ex.shutdown()\nmain()\n");
}

// ----- G1 (B3.3b): module globals are read-only across tasks (`--parallel`) -----

#[test]
fn spawn_transitive_global_mutation_rejected() {
    // A module global reassigned inside a function reachable from `spawn` is illegal — cross-task
    // mutable state must go through Shared[T] (the value → Ref → Shared mutation ladder's top rung).
    rejects(
        "n := 0\nfn bump():\n    n = n + 1\nfn main():\n    parallel:\n        spawn bump()\nmain()\n",
        "use Shared[T]",
    );
}

#[test]
fn spawn_block_calls_global_mutator_rejected() {
    // The mutator is reached through a `spawn:` block that calls it (not a direct `spawn f()`).
    rejects(
        "n := 0\nfn bump():\n    n = n + 1\nfn main():\n    parallel:\n        spawn:\n            bump()\nmain()\n",
        "use Shared[T]",
    );
}

#[test]
fn spawn_deeply_transitive_global_mutation_rejected() {
    // `spawn a()` → `a()` calls `b()` → `b()` mutates the global. Proves transitive reachability.
    rejects(
        "n := 0\nfn b():\n    n = n + 1\nfn a():\n    b()\nfn main():\n    parallel:\n        spawn a()\nmain()\n",
        "use Shared[T]",
    );
}

#[test]
fn sequential_global_mutation_ok() {
    // Flow-scoped: the same mutation reached only from sequential (non-spawn) code stays legal.
    ok("n := 0\nfn bump():\n    n = n + 1\nfn main():\n    bump()\n    print(n)\nmain()\n");
}

#[test]
fn spawn_local_shadows_global_ok() {
    // A spawn-reachable function whose local shadows the global name mutates the LOCAL, not the
    // global — it must not be flagged.
    ok("n := 0\nfn work():\n    n := 5\n    n = n + 1\n    print(n)\nfn main():\n    parallel:\n        spawn work()\nmain()\n");
}

#[test]
fn spawn_reads_global_ok() {
    // Reading a (post-init constant) global from a task is fine; only mutation is gated.
    ok("n := 7\nfn work():\n    print(n)\nfn main():\n    parallel:\n        spawn work()\nmain()\n");
}

#[test]
fn shared_update_in_spawn_ok() {
    // The prescribed cross-task mutation path: a global `Shared`, mutated via `update()` in a task.
    ok("c := Shared(0)\nfn bump():\n    c.update(fn(x): x + 1)\nfn main():\n    parallel:\n        spawn bump()\n    print(c.get())\nmain()\n");
}

#[test]
fn spawn_compound_assign_global_rejected() {
    // `+=` / `-=` are reassignments too — the gate must treat them like `=`.
    rejects(
        "n := 0\nfn bump():\n    n += 1\nfn main():\n    parallel:\n        spawn bump()\nmain()\n",
        "use Shared[T]",
    );
}

#[test]
fn spawn_global_mutation_inside_if_rejected() {
    // Mutation nested in control flow (not at the function-body top level) is still caught.
    rejects(
        "n := 0\nfn bump(c: bool):\n    if c:\n        n = n + 1\nfn main():\n    parallel:\n        spawn bump(true)\nmain()\n",
        "use Shared[T]",
    );
}

#[test]
fn spawn_reaches_mutator_through_arg_expr_rejected() {
    // The call graph follows a callee buried in an argument expression (`print(mutator())`).
    rejects(
        "n := 0\nfn mutate() -> int:\n    n = n + 1\n    return n\nfn caller():\n    print(mutate())\nfn main():\n    parallel:\n        spawn caller()\nmain()\n",
        "use Shared[T]",
    );
}

#[test]
fn spawn_callee_shadowed_by_local_ok() {
    // A local binding shadowing a free function's name at the spawn site means `spawn bump()`
    // targets the LOCAL (inert) closure, not the global-mutating free fn — must not be flagged.
    ok("n := 0\nfn bump():\n    n = n + 1\nfn main():\n    bump := fn(): 1\n    parallel:\n        spawn bump()\nmain()\n");
}

#[test]
fn spawn_global_mutation_inside_recover_rejected() {
    // A `recover:` block is an expression that embeds a full statement block — a global mutation
    // hidden inside one in a spawn-reachable fn must still be caught.
    rejects(
        "n := 0\nfn bump():\n    x := recover:\n        n = n + 1\n    print(x)\nfn main():\n    parallel:\n        spawn bump()\nmain()\n",
        "use Shared[T]",
    );
}

// ----- C5 refinement #2: `Ref` non-sendability keys on origin, not the bare name -----

#[test]
fn user_struct_named_ref_is_sendable() {
    // A *user-defined* struct that happens to be named `Ref` (no std.ref import) is an ordinary
    // sendable struct — the non-sendability gate applies only to the builtin std.ref `Ref[T]`.
    entry_ok("struct Ref:\n    val: int\nfn use_it(r: Ref):\n    print(r.val)\nfn main():\n    r := Ref(1)\n    parallel:\n        spawn use_it(r)\nmain()\n");
}

// ----- D6c: optional `timeout_ms` on net socket read/accept/write -----

#[test]
fn socket_read_with_timeout_type_checks() {
    // `read(n)` and `read(n, timeout_ms)` both type-check (the trailing int is optional).
    ok("fn use_sock(s: Socket) -> str!:\n    a := s.read(64)?\n    b := s.read(64, 100)?\n    return Ok(a + b)\n");
}

#[test]
fn socket_write_with_timeout_type_checks() {
    ok("fn use_sock(s: Socket) -> int!:\n    a := s.write(\"x\")?\n    b := s.write(\"x\", 100)?\n    return Ok(a + b)\n");
}

#[test]
fn listener_accept_with_timeout_type_checks() {
    // `accept()` and `accept(timeout_ms)` both type-check.
    ok("fn use_listener(l: Listener) -> int!:\n    l.accept()?\n    l.accept(100)?\n    return Ok(0)\n");
}

#[test]
fn socket_read_with_non_int_timeout_rejected() {
    // A non-int `timeout_ms` is a type error.
    rejects("fn use_sock(s: Socket):\n    s.read(64, \"x\")\n", "expected int");
}

#[test]
fn socket_read_with_too_few_args_rejected() {
    // `read()` (zero args) is below the 1–2 arg range.
    rejects("fn use_sock(s: Socket):\n    s.read()\n", "argument");
}

#[test]
fn socket_read_with_too_many_args_rejected() {
    // `read(n, t, extra)` exceeds the 1–2 arg range.
    rejects("fn use_sock(s: Socket):\n    s.read(64, 100, 1)\n", "argument");
}

#[test]
fn listener_accept_with_too_many_args_rejected() {
    rejects("fn use_listener(l: Listener):\n    l.accept(100, 1)\n", "argument");
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
    ok(
        "enum E:\n    A(int)\n    B(int)\ne := A(1)\nmatch e:\n    A(a) | B(a): print(a)\n",
    );
}

#[test]
fn enum_or_pattern_exhaustive_without_wildcard() {
    // A 3-variant enum covered by a single or-pattern is exhaustive WITHOUT a `_`.
    ok(
        "enum Color:\n    Red\n    Green\n    Blue\nc := Red\nmatch c:\n    Red | Green | Blue: print(\"c\")\n",
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
    rejects("n := 3\nmatch n:\n    1 | 2: print(\"x\")\n", "non-exhaustive");
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
