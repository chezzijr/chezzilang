//! Checker tests: generators, assert, print, test fn, bytes/bytearray, conversions, un-inferable
//! type params, Iterable, return paths, nil, panic, enum methods, casts.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== generators (experimental, VM-only) =====

#[test]
fn generator_basic_ok() {
    // A generator declares `-> Iterator[T]`, yields T, and its result drives a `for` (x: int).
    ok(
        "import Iterator from std.iter\nfn count() -> Iterator[int]:\n    yield 1\n    yield 2\n\nfn use() -> int:\n    s := 0\n    for x in count():\n        s = s + x\n    return s\n",
    );
}

#[test]
fn generator_for_binds_element_type() {
    // `for x in count()` must bind `x: int`, so using it as a str is a type error.
    rejects(
        "import Iterator from std.iter\nfn count() -> Iterator[int]:\n    yield 1\n\nfn use():\n    for x in count():\n        print(x + \"a\")\n",
        "",
    );
}

#[test]
fn generator_yield_type_mismatch_rejected() {
    rejects(
        "import Iterator from std.iter\nfn g() -> Iterator[int]:\n    yield \"nope\"\n",
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
    // No `-> Iterator[T]`: a generator writes its element type; it is not inferred from a yield.
    rejects(
        "fn count():\n    yield 1\n    yield 2\nfn use() -> int:\n    s := 0\n    for x in count():\n        s = s + x\n    return s\n",
        "must declare a return type of `Iterator[T]`",
    );
}

#[test]
fn generator_inferred_element_recovered_not_unknown() {
    // The inferred element is the CONCRETE first-yield type (int), NOT a permissive `Unknown`:
    // `x + "a"` (int + str) must be rejected — proves inference pins a real type.
    rejects(
        "import Iterator from std.iter\nfn count() -> Iterator[int]:\n    yield 1\nfn use():\n    for x in count():\n        print(x + \"a\")\n",
        "",
    );
}

#[test]
fn generator_inferred_struct_method_no_annotation() {
    // The same rule for a method: an un-annotated generator `each` is rejected.
    rejects(
        "struct Box:\n    n: int\n    fn each(self):\n        i := 0\n        while i < self.n:\n            yield i\n            i = i + 1\nfn use() -> int:\n    b := Box(3)\n    s := 0\n    for x in b.each():\n        s = s + x\n    return s\n",
        "must declare a return type of `Iterator[T]`",
    );
}

#[test]
fn generator_explicit_annotation_still_works() {
    // The explicit `-> Iterator[T]` path is untouched: annotation still validates yields against T.
    ok("import Iterator from std.iter\nfn count() -> Iterator[int]:\n    yield 1\n    yield 2\n");
    rejects(
        "import Iterator from std.iter\nfn count() -> Iterator[int]:\n    yield \"x\"\n",
        "expected yield type int",
    );
}

#[test]
fn generator_return_value_rejected() {
    rejects(
        "import Iterator from std.iter\nfn g() -> Iterator[int]:\n    yield 1\n    return 5\n",
        "cannot `return` a value",
    );
}

#[test]
fn generator_explicit_next_ok() {
    // A generator result is an `Iterator[int]`; `.next()` returns `Option[int]`, drivable explicitly.
    ok(
        "import Iterator from std.iter\nfn count() -> Iterator[int]:\n    yield 1\n\nfn use():\n    g := count()\n    match g.next():\n        ?v: print(v)\n        None: print(-1)\n",
    );
}

#[test]
fn iterator_value_unknown_method_rejected() {
    rejects(
        "import Iterator from std.iter\nfn count() -> Iterator[int]:\n    yield 1\n\nfn use():\n    g := count()\n    g.bogus()\n",
        "has no method 'bogus'",
    );
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
        "import Iterator from std.iter\nfn g() -> Iterator[int]:\n    defer print(0)\n    yield 1\n",
        "`defer` is not supported inside a generator",
    );
}

#[test]
fn generator_spawn_rejected() {
    rejects(
        "import Iterator from std.iter\nfn g() -> Iterator[int]:\n    parallel:\n        spawn print(0)\n    yield 1\n",
        "`parallel:` is not supported inside a generator",
    );
}

#[test]
fn generator_yield_only_in_recover_ok() {
    // A generator whose only `yield` lives in a `recover:` block is valid (detected as a generator,
    // so `yield` is legal there) — guards the parser/checker recover-descent fix.
    ok(
        "import Iterator from std.iter\nfn g() -> Iterator[int]:\n    x := recover:\n        yield 1\n        1\n    print(x)\n",
    );
}

#[test]
fn generator_defer_in_recover_rejected() {
    // The `defer` ban must not be bypassable by nesting inside a `recover:` block.
    rejects(
        "import Iterator from std.iter\nfn cleanup():\n    print(\"c\")\nfn g() -> Iterator[int]:\n    yield 0\n    x := recover:\n        defer cleanup()\n        1\n    print(x)\n",
        "`defer` is not supported inside a generator",
    );
}

#[test]
fn generator_bare_return_ok() {
    // A bare `return` inside a generator stops it early — legal.
    ok(
        "import Iterator from std.iter\nfn g() -> Iterator[int]:\n    yield 1\n    return\n    yield 2\n",
    );
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
    rejects_desugared("print(\"a\", sep=1)\n", "expected str, found int");
}

#[test]
fn print_end_non_str_rejected() {
    rejects_desugared("print(\"a\", end=1)\n", "expected str, found int");
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
        "fn main():\n    b := b\"hi\"\n    x: int = b[0]\n    s: bytes = b[0..1]\n    for c in b:\n        print(c)\n    n: int = b.len()\n    print(x + n)\nmain()\n",
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
        "fn main():\n    ba := bytearray([1, 2, 3])\n    x: int = ba[0]\n    ba[0] = 5\n    s: bytearray = ba[0..1]\n    for c in ba:\n        print(c)\n    n: int = ba.len()\n    print(x + n)\nmain()\n",
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
    ok("fn main():\n    a: List[int] = List(0..3)\n    print(a.len())\nmain()\n");
    ok("fn main():\n    a: List[int] = List(bytearray([1, 2]))\n    print(a.len())\nmain()\n");
    // Set() broadened from list-only to any for-iterable.
    ok("fn main():\n    s: Set[str] = Set(\"abc\")\n    print(s.len())\nmain()\n");
    ok("fn main():\n    s: Set[int] = Set(0..3)\n    print(s.len())\nmain()\n");
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
    ok("e: List[int] = List()\ne.push(1)\nprint(e.len())\n");
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
    ok("fn each[T](xs: List[T], f: fn(T) -> None):\n    return\neach([], fn(x): print(x))\n");
    ok(
        "fn mapper[T, U](xs: List[T], f: fn(T) -> U) -> List[U]:\n    return []\nx := mapper([], fn(x): 42)\n",
    );
    // An UNRELATED body error (not about `T`) must surface as itself, not be masked by the
    // inference-deadlock message.
    let errs = check_src(
        "fn each[T](xs: List[T], f: fn(T) -> None):\n    return\neach([], fn(x): nope)\n",
    );
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
    // a map's element is its `(key, value)` pair, so `Map(m)` checks (owner U9).
    ok("fn main():\n    src := {1: \"a\"}\n    m := Map(src)\nmain()\n");
    // a list of keys is not a list of pairs -> static error.
    rejects(
        "fn main():\n    src := {1: \"a\"}\n    m := Map(src.keys())\nmain()\n",
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
    ok("fn main():\n    it := {1, 2}.iter()\n    x: int? = it.next()\n    print(x)\nmain()\n");
    ok(
        "fn main():\n    it := {1: \"a\"}.iter()\n    k: (int, str)? = it.next()\n    print(k)\nmain()\n",
    );
    ok(
        "fn main():\n    it := {1: \"a\"}.keys().iter()\n    k: int? = it.next()\n    print(k)\nmain()\n",
    );
    ok("fn main():\n    it := \"ab\".iter()\n    c: str? = it.next()\n    print(c)\nmain()\n");
    ok("fn main():\n    it := b\"hi\".iter()\n    b: int? = it.next()\n    print(b)\nmain()\n");
    ok(
        "fn main():\n    it := bytearray([1, 2]).iter()\n    b: int? = it.next()\n    print(b)\nmain()\n",
    );
}

#[test]
fn iter_cursor_drives_existing_adapters() {
    // The headline win: a list cursor composes into a struct adapter bounded `[I: Iterator[T]]`.
    ok(
        "import Iterator from std.iter\nstruct Take[I: Iterator[T], T]:\n    inner: I\n    left: int\n    fn next(self) -> T?:\n        if self.left <= 0:\n            return None\n        self.left = self.left - 1\n        return self.inner.next()\nfn main():\n    t := Take([10, 20, 30].iter(), 2)\n    for v in t:\n        print(v)\nmain()\n",
    );
}

#[test]
fn iterable_bound_accepts_list_and_generator() {
    // `[S: Iterable[int]]` accepts a List[int] AND a generator (Iterator[int]).
    ok(
        "import Iterable, Iterator from std.iter\nfn count[S: Iterable[int]](s: S) -> int:\n    n := 0\n    for x in s.iter():\n        n = n + 1\n    return n\nfn gen() -> Iterator[int]:\n    yield 1\n    yield 2\nfn main():\n    print(count([1, 2, 3]))\n    print(count(gen()))\nmain()\n",
    );
}

#[test]
fn iter_idempotent_on_generator_and_cursor() {
    // Every Iterator IS Iterable: iter() returns self, idempotently.
    ok(
        "import Iterator from std.iter\nfn gen() -> Iterator[int]:\n    yield 1\nfn main():\n    it := gen().iter()\n    print(it.next())\nmain()\n",
    );
    ok("fn main():\n    it := [1, 2, 3].iter().iter()\n    print(it.next())\nmain()\n");
}

#[test]
fn iterable_struct_with_only_iter() {
    // A user struct with iter(self) -> Iterator[E] (but no next) satisfies Iterable and is for-iterable.
    ok(
        "import Iterator from std.iter\nstruct Wrap:\n    xs: List[int]\n    fn iter(self) -> Iterator[int]:\n        return self.xs.iter()\nfn main():\n    w := Wrap([1, 2, 3])\n    for x in w:\n        print(x)\nmain()\n",
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
        "import Iterable, Iterator from std.iter\nfn use_iterable[U: Iterable[int]](xs: U) -> int:\n    n := 0\n    for x in xs.iter():\n        n = n + 1\n    return n\nfn pass_through[S: Iterator[int]](xs: S) -> int:\n    return use_iterable(xs)\nfn main():\n    print(pass_through([1, 2, 3].iter()))\nmain()\n",
    );
}

/// `Iterable[T]` in TYPE position (an annotated param/local) is for-iterable — the operation the
/// protocol exists to promise. It resolves to `Ty::Protocol("Iterable", [T])` (only the reserved
/// `Iterator[T]` is intercepted into a `Ty::Struct`), so the iteration union needs a `Ty::Protocol`
/// arm; without it the checker accepted the annotation AND the `List[int]` -> `Iterable[int]` call
/// site, then refused the `for`.
#[test]
fn iterable_in_type_position_is_for_iterable() {
    // List
    ok(
        "import Iterable from std.iter\nfn f(xs: Iterable[int]) -> int:\n    n := 0\n    for v in xs:\n        n += v\n    return n\nfn main():\n    print(str(f([1, 2, 3])))\nmain()\n",
    );
    // Set
    ok(
        "import Iterable from std.iter\nfn f(xs: Iterable[int]) -> int:\n    n := 0\n    for v in xs:\n        n += v\n    return n\nfn main():\n    print(str(f({1, 2, 3})))\nmain()\n",
    );
    // A Map iterates its `(key, value)` pairs (owner U9), so a Map[str,int] is an
    // Iterable[(str, int)] and no longer an Iterable[str] (`m.keys()` is).
    ok(
        "import Iterable from std.iter\nfn f(xs: Iterable[(str, int)]) -> int:\n    n := 0\n    for k, v in xs:\n        n += v + k.len()\n    return n\nfn main():\n    print(str(f({\"a\": 1})))\nmain()\n",
    );
    rejects(
        "import Iterable from std.iter\nfn f(xs: Iterable[str]) -> int:\n    return 0\nfn main():\n    print(str(f({\"a\": 1})))\nmain()\n",
        "does not satisfy Iterable",
    );
    ok(
        "import Iterable from std.iter\nfn f(xs: Iterable[str]) -> int:\n    return 0\nfn main():\n    print(str(f({\"a\": 1}.keys())))\nmain()\n",
    );
    // str iterates to str
    ok(
        "import Iterable from std.iter\nfn f(xs: Iterable[str]) -> int:\n    n := 0\n    for v in xs:\n        n += v.len()\n    return n\nfn main():\n    print(str(f(\"abc\")))\nmain()\n",
    );
    // a cursor
    ok(
        "import Iterable from std.iter\nfn f(xs: Iterable[int]) -> int:\n    n := 0\n    for v in xs:\n        n += v\n    return n\nfn main():\n    print(str(f([1, 2].iter())))\nmain()\n",
    );
    // a generator result
    ok(
        "import Iterable, Iterator from std.iter\nfn gen() -> Iterator[int]:\n    yield 1\n    yield 2\nfn f(xs: Iterable[int]) -> int:\n    n := 0\n    for v in xs:\n        n += v\n    return n\nfn main():\n    print(str(f(gen())))\nmain()\n",
    );
    // a comprehension over an `Iterable[T]` param, and `List(xs)` — the other `iter_elem` consumers.
    ok(
        "import Iterable from std.iter\nfn f(xs: Iterable[int]) -> int:\n    ys := [v * 2 for v in xs]\n    return ys[0]\nfn main():\n    print(str(f([1, 2])))\nmain()\n",
    );
    ok(
        "import Iterable from std.iter\nfn f(xs: Iterable[int]) -> int:\n    return List(xs)[0]\nfn main():\n    print(str(f([1, 2])))\nmain()\n",
    );
    // The pre-fix workaround (`xs.iter()` first) must keep working identically — it now takes the
    // `iter_elem` fast path instead of the Protocol method arm; both yield `Iterator[int]`.
    ok(
        "import Iterable from std.iter\nfn f(xs: Iterable[int]) -> int?:\n    return xs.iter().next()\nfn main():\n    print(f([1, 2]))\nmain()\n",
    );
}

/// The shapes that must STAY rejected around the new arm.
#[test]
fn iterable_type_position_rejects_bad_shapes() {
    // (a) a BARE `Iterable` is an existential with unbound params — not iterable (the arity guard).
    rejects(
        "import Iterable from std.iter\nfn f(xs: Iterable) -> int:\n    n := 0\n    for v in xs:\n        n += 1\n    return n\nfn main():\n    print(str(f([1, 2])))\nmain()\n",
        "cannot iterate over Iterable",
    );
    // (b) the two-variable `for i, v in` form: an existential carries no key/index, so the element
    // (`int`) is not a tuple — see `two_var_for_over_iterable_annotation_names_the_type`.
    rejects(
        "import Iterable from std.iter\nfn f(xs: Iterable[int]) -> int:\n    n := 0\n    for i, v in xs:\n        n += v\n    return n\nfn main():\n    print(str(f([1, 2])))\nmain()\n",
        "tuple pattern cannot match a value of type int",
    );
    // (c) W6-3b: `Iterable` is NOT `Iterator` — an `Iterable[T]` value still cannot call `.next()`.
    rejects(
        "import Iterable from std.iter\nfn f(xs: Iterable[int]) -> int?:\n    return xs.next()\nfn main():\n    print(f([1, 2]))\nmain()\n",
        "next",
    );
    // (d) and it still cannot satisfy an `[S: Iterator[T]]` bound.
    rejects(
        "import Iterable, Iterator from std.iter\nfn g[S: Iterator[int]](xs: S) -> int?:\n    return xs.next()\nfn f(xs: Iterable[int]) -> int?:\n    return g(xs)\nfn main():\n    print(f([1, 2]))\nmain()\n",
        "Iterator",
    );
}

/// CONTAINER INVARIANCE MUST NOT WIDEN. Read-only covariance is deliberately NOT part of making
/// `Iterable[T]` iterable — `List[int]` -> `Iterable[Any]` stays rejected. (The `List[Any]` /
/// `Map[str, Any]` / `Box[Speaker]` fences live in the mutable-container invariance block below.)
#[test]
fn container_invariance_stays_rejected_for_iterable() {
    // List[int] -> Iterable[Any]
    entry_rejects(
        "import Iterable from std.iter\nfn f(xs: Iterable[Any]) -> int:\n    return 0\nfn main():\n    ys: List[int] = [1, 2]\n    print(f(ys))\nmain()\n",
        "expected Iterable[Any], found List[int]",
    );
    // List[Sq] -> List[Shape] (a struct that DOES satisfy the protocol)
    entry_rejects(
        "protocol Shape:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int:\n        return self.s * self.s\nfn f(xs: List[Shape]) -> int:\n    return 0\nfn main():\n    ys: List[Sq] = [Sq(2)]\n    print(f(ys))\nmain()\n",
        "expected List[Shape], found List[Sq]",
    );
    // Iterable[int] -> Iterable[Any] (the NEW neighbour: protocol args stay invariant)
    entry_rejects(
        "import Iterable from std.iter\nfn wide(xs: Iterable[Any]) -> int:\n    return 0\nfn f(xs: Iterable[int]) -> int:\n    return wide(xs)\nfn main():\n    print(f([1, 2]))\nmain()\n",
        "expected Iterable[Any], found Iterable[int]",
    );
    // Iterable[int] -> Iterable[str] (plain mismatch)
    entry_rejects(
        "import Iterable from std.iter\nfn other(xs: Iterable[str]) -> int:\n    return 0\nfn f(xs: Iterable[int]) -> int:\n    return other(xs)\nfn main():\n    print(f([1, 2]))\nmain()\n",
        "expected Iterable[str], found Iterable[int]",
    );
}

/// VALUE-LEVEL widening still works: struct -> protocol, and protocol -> wider protocol.
#[test]
fn value_level_widening_still_ok() {
    entry_ok(
        "protocol Shape:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int:\n        return self.s * self.s\nfn f(x: Shape) -> int:\n    return x.area()\nfn main():\n    print(f(Sq(3)))\nmain()\n",
    );
    entry_ok(
        "protocol Shape:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int:\n        return self.s * self.s\nfn wide(x: Any) -> int:\n    return 0\nfn f(x: Shape) -> int:\n    return wide(x)\nfn main():\n    print(f(Sq(3)))\nmain()\n",
    );
}

/// An `Iterable[T]`-ANNOTATED param forwards into an `[S: Iterable[T], T]` bound, and the bound
/// recovers `T` from it (`satisfies_args`/`recover_iter_elems` both route through `iter_elem`).
#[test]
fn iterable_annotated_param_forwards_into_iterable_bound() {
    ok(
        "import Iterable from std.iter\nfn g[S: Iterable[T], T](xs: S) -> int:\n    n := 0\n    for v in xs:\n        n += 1\n    return n\nfn f(xs: Iterable[int]) -> int:\n    return g(xs)\nfn main():\n    print(str(f([1, 2, 3])))\nmain()\n",
    );
    // the recovered `T` is usable as `int`…
    ok(
        "import Iterable from std.iter\nfn g[S: Iterable[T], T](xs: S, d: T) -> int:\n    return 1\nfn f(xs: Iterable[int]) -> int:\n    return g(xs, 1)\nfn main():\n    print(str(f([1, 2, 3])))\nmain()\n",
    );
    // …and a wrong-element neighbour still rejects.
    rejects(
        "import Iterable from std.iter\nfn g[S: Iterable[T], T](xs: S, d: T) -> int:\n    return 1\nfn f(xs: Iterable[int]) -> int:\n    return g(xs, \"a\")\nfn main():\n    print(str(f([1, 2, 3])))\nmain()\n",
        "does not match the declared element type",
    );
}

/// The DOCUMENTED bound forms (syntax.md:894, spec.md:205) are unchanged.
#[test]
fn documented_iterable_bound_forms_unchanged() {
    ok(
        "import Iterable from std.iter\nfn first_or[S: Iterable[T], T](xs: S, default: T) -> T:\n    for v in xs:\n        return v\n    return default\nfn main():\n    print(str(first_or([1, 2], 0)))\nmain()\n",
    );
    ok(
        "import Iterable from std.iter\nfn f[C: Iterable[int]](xs: C) -> int:\n    n := 0\n    for v in xs:\n        n += v\n    return n\nfn main():\n    print(str(f([1, 2])))\nmain()\n",
    );
}

/// A struct with ONLY `iter` (no `next`):
/// - under an `[S: Iterable[T], T]` BOUND, `T` is recovered from the `iter` hook (TICKET-250, D6:
///   generic recovery reads `iterable_elem`, the reader `for` uses);
/// - passed to a param ANNOTATED `Iterable[int]` it works, because the annotation is the source
///   of truth for `T` and the runtime drives it through the same `IterableToCursor` conversion.
#[test]
fn iter_only_struct_bound_recovers_its_element() {
    // BOUND position: `T` is recovered from the `iter` hook.
    ok(
        "import Iterable, Iterator from std.iter\nstruct Wrap:\n    xs: List[int]\n    fn iter(self) -> Iterator[int]:\n        return self.xs.iter()\nfn to_list[S: Iterable[T], T](xs: S) -> List[T]:\n    out: List[T] = []\n    for x in xs:\n        out.push(x)\n    return out\nfn main():\n    print(to_list(Wrap([1, 2])))\nmain()\n",
    );
    // …and the concrete-arg workaround the docs prescribe still works.
    ok(
        "import Iterable, Iterator from std.iter\nstruct Wrap:\n    xs: List[int]\n    fn iter(self) -> Iterator[int]:\n        return self.xs.iter()\nfn count[S: Iterable[int]](xs: S) -> int:\n    n := 0\n    for x in xs:\n        n += x\n    return n\nfn main():\n    print(str(count(Wrap([1, 2]))))\nmain()\n",
    );
    // ANNOTATION position: works (the edge decision).
    ok(
        "import Iterable, Iterator from std.iter\nstruct Wrap:\n    xs: List[int]\n    fn iter(self) -> Iterator[int]:\n        return self.xs.iter()\nfn f(xs: Iterable[int]) -> int:\n    n := 0\n    for v in xs:\n        n += v\n    return n\nfn main():\n    print(str(f(Wrap([1, 2]))))\nmain()\n",
    );
}

/// TICKET-250 — a `next` that does not satisfy `Iterator`'s requirement is an ordinary method. It
/// does not make the type non-iterable: a conforming `iter` beside it is the hook, for every
/// consumer (`for`, `List()`, the bound, an `Iterable[T]` slot).
#[test]
fn a_nonconforming_next_is_an_ordinary_method() {
    // (a) extra param on `next` + a conforming `iter`: `next` is an ordinary method, `iter` is the hook.
    let odd = "import Iterator from std.iter\nstruct Odd:\n    xs: List[int]\n    n: int\n    fn next(self, k: int) -> int?:\n        self.n += 1\n        if self.n > 3:\n            return None\n        return ?self.n\n    fn iter(self) -> Iterator[int]:\n        return self.xs.iter()\n";
    ok(&format!(
        "import Iterable from std.iter\n{odd}fn viaList(xs: Iterable[int]) -> List[int]:\n    return List(xs)\nfn main():\n    print(str(viaList(Odd([9, 9], 0))))\nmain()\n"
    ));
    ok(&format!(
        "{odd}fn main():\n    for v in Odd([9, 9], 0):\n        print(str(v))\nmain()\n"
    ));
    ok(&format!(
        "import Iterable from std.iter\n{odd}fn count[S: Iterable[int]](xs: S) -> int:\n    return 0\nfn main():\n    print(str(count(Odd([9, 9], 0))))\nmain()\n"
    ));
    // (b) a non-`Option` return on `next` is the same class.
    ok(
        "import Iterable, Iterator from std.iter\nstruct Bad:\n    xs: List[int]\n    fn next(self) -> int:\n        return 1\n    fn iter(self) -> Iterator[int]:\n        return self.xs.iter()\nfn f(xs: Iterable[int]) -> List[int]:\n    return List(xs)\nfn main():\n    print(str(f(Bad([1, 2]))))\nmain()\n",
    );
    // (c) a conforming `next` alongside `iter`: `iter` is the hook `for` drives.
    ok(
        "import Iterable, Iterator from std.iter\nstruct Both:\n    n: int\n    xs: List[int]\n    fn next(self) -> int?:\n        if self.n <= 0:\n            return None\n        self.n -= 1\n        return ?self.n\n    fn iter(self) -> Iterator[int]:\n        return self.xs.iter()\nfn f(xs: Iterable[int]) -> int:\n    n := 0\n    for v in xs:\n        n += v\n    return n\nfn main():\n    print(str(f(Both(3, [9]))))\nmain()\n",
    );
}

/// A two-name `for` over a non-struct iterand must not be told it has a struct. The trailing
/// `iterable_elem` arm is reached by protocol existentials too, so the legacy struct wording is kept
/// only for actual structs.
#[test]
fn two_var_for_over_iterable_annotation_names_the_type() {
    // A TUPLE element destructures (TICKET-113), so this is accepted now; a non-tuple element names its own type in the one tuple message.
    ok(
        "import Iterable from std.iter\nfn pairs(xs: Iterable[(str, int)]) -> int:\n    n := 0\n    for k, v in xs:\n        n += v\n    return n\nfn main():\n    print(str(pairs([(\"a\", 1)])))\nmain()\n",
    );
    rejects(
        "import Iterable from std.iter\nfn kv(xs: Iterable[str]) -> int:\n    n := 0\n    for i, v in xs:\n        n += 1\n    return n\nfn main():\n    print(str(kv([\"a\"])))\nmain()\n",
        "tuple pattern cannot match a value of type str",
    );
    // the same for an `[S: Iterable[T]]` BOUND — the other spelling of the same existential.
    rejects(
        "import Iterable from std.iter\nfn g[S: Iterable[int]](xs: S) -> int:\n    for i, v in xs:\n        return v\n    return 0\nfn main():\n    print(str(g([1, 2])))\nmain()\n",
        "tuple pattern cannot match a value of type int",
    );
    // a struct iterator is one more iterable: its element (`int`) is not a tuple.
    rejects(
        "struct Counter:\n    n: int\n    fn next(self) -> int?:\n        return None\nfn main():\n    for k, v in Counter(0):\n        print(str(k))\nmain()\n",
        "tuple pattern cannot match a value of type int",
    );
}

/// TICKET-113: a struct-iterator `next()` yielding a TUPLE should destructure with `for a, b in`,
/// the same as a `list[(A, B)]` does. Today the struct-iterator arm in `for_bindings` rejects any
/// `vars.len() != 1` before ever looking at the element type, so this is wrongly rejected.
#[test]
fn for_multi_name_over_struct_next_tuple_destructures() {
    ok(
        "struct Pairs:\n    n: int\n    fn next(self) -> (int, str)?:\n        if self.n <= 0:\n            return None\n        self.n -= 1\n        return ?(self.n, \"x\")\nfn main():\n    for a, b in Pairs(2):\n        print(\"{a}:{b}\")\nmain()\n",
    );
}

/// TICKET-113: N names over ANY iterable whose element is statically a tuple of arity N bind each
/// name to its slot type — not only a `List[(A, B)]`. One `ok` per source; the slot types are
/// pinned by the last case of `for_multi_name_rejects_non_tuple_and_wrong_arity`.
#[test]
fn for_multi_name_over_every_tuple_source_checks() {
    let pairs = "import Iterator from std.iter\nfn pairs() -> Iterator[(int, str)]:\n    yield (1, \"x\")\n    yield (2, \"y\")\n";
    // (a) a generator
    ok(&format!(
        "{pairs}fn main():\n    for a, b in pairs():\n        print(a + 1, b.len())\nmain()\n"
    ));
    // (b) an `.iter()` cursor
    ok("fn main():\n    for a, b in [(1, \"x\")].iter():\n        print(a + 1, b.len())\nmain()\n");
    // (c) an `Iterable[(str, int)]` annotation
    ok(
        "import Iterable from std.iter\nfn total(xs: Iterable[(str, int)]) -> int:\n    n := 0\n    for k, v in xs:\n        n += v + k.len()\n    return n\nfn main():\n    print(total([(\"a\", 1)]))\nmain()\n",
    );
    // (d) an `[S: Iterable[(str, int)]]` bound
    ok(
        "import Iterable from std.iter\nfn total[S: Iterable[(str, int)]](xs: S) -> int:\n    n := 0\n    for k, v in xs:\n        n += v + k.len()\n    return n\nfn main():\n    print(total([(\"a\", 1)]))\nmain()\n",
    );
    // (e) arity 3 from a generator
    ok(
        "import Iterator from std.iter\nfn trip() -> Iterator[(int, str, float)]:\n    yield (1, \"x\", 2.5)\nfn main():\n    for a, b, c in trip():\n        print(a + 1, b.len(), c * 2.0)\nmain()\n",
    );
    // (f) a pure-`Iterable` struct (`iter`, no `next`)
    ok(
        "import Iterator from std.iter\nstruct Wrap:\n    xs: List[(int, str)]\n    fn iter(self) -> Iterator[(int, str)]:\n        return self.xs.iter()\nfn main():\n    for a, b in Wrap([(1, \"x\")]):\n        print(a + 1, b.len())\nmain()\n",
    );
    // (g) a `Channel[(int, str)]`
    ok(
        "fn main():\n    ch := Channel[(int, str)](1)\n    ch.send((1, \"x\"))\n    ch.close()\n    for a, b in ch:\n        print(a + 1, b.len())\nmain()\n",
    );
    // (h) a comprehension clause over a generator (`for_bindings` serves both)
    ok(&format!(
        "{pairs}fn main():\n    print([a + b.len() for a, b in pairs()])\nmain()\n"
    ));
}

/// TICKET-113: the N-name widening admits a TUPLE element only. A non-tuple element keeps each
/// source's existing message, a tuple of the wrong arity names both counts from every source, and a
/// range stays refused. The last case pins that the names carry the slot types, not `Unknown`.
#[test]
fn for_multi_name_rejects_non_tuple_and_wrong_arity() {
    rejects(
        "import Iterator from std.iter\nfn gen() -> Iterator[int]:\n    yield 1\nfn main():\n    for a, b in gen():\n        print(a)\nmain()\n",
        "tuple pattern cannot match a value of type int",
    );
    rejects(
        "fn main():\n    for a, b in [1, 2].iter():\n        print(a)\nmain()\n",
        "tuple pattern cannot match a value of type int",
    );
    rejects(
        "fn main():\n    ch := Channel[int](1)\n    ch.close()\n    for a, b in ch:\n        print(a)\nmain()\n",
        "tuple pattern cannot match a value of type int",
    );
    rejects(
        "fn main():\n    for a, b in 0..3:\n        print(a)\nmain()\n",
        "tuple pattern cannot match a value of type int",
    );
    rejects(
        "import Iterator from std.iter\nfn trip() -> Iterator[(int, str, float)]:\n    yield (1, \"x\", 2.5)\nfn main():\n    for a, b in trip():\n        print(a)\nmain()\n",
        "tuple pattern has 2 element(s), but the value has 3",
    );
    rejects(
        "struct Pairs:\n    n: int\n    fn next(self) -> (int, str)?:\n        return None\nfn main():\n    for a, b, c in Pairs(0):\n        print(a)\nmain()\n",
        "tuple pattern has 3 element(s), but the value has 2",
    );
    rejects(
        "import Iterable from std.iter\nfn f(xs: Iterable[(str, int)]) -> int:\n    for a, b, c in xs:\n        return 0\n    return 1\nfn main():\n    print(f([(\"a\", 1)]))\nmain()\n",
        "tuple pattern has 3 element(s), but the value has 2",
    );
    rejects(
        "fn main():\n    ch := Channel[(int, str)](1)\n    ch.close()\n    for a, b, c in ch:\n        print(a)\nmain()\n",
        "tuple pattern has 3 element(s), but the value has 2",
    );
    rejects(
        "import Iterator from std.iter\nfn pairs() -> Iterator[(int, str)]:\n    yield (1, \"x\")\nfn main():\n    for a, b in pairs():\n        s: str = a\n        print(s)\nmain()\n",
        "cannot assign int to variable of type str",
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
        "no value (None)",
    );
}

// ===== nil used as a value is rejected (Part 2) =====

#[test]
fn nil_in_value_position_rejected() {
    // assignment RHS
    rejects(
        "fn main():\n    x := print(\"hi\")\nmain()\n",
        "no value (None)",
    );
    // function/call argument
    rejects(
        "fn main():\n    print(print(\"hi\"))\nmain()\n",
        "no value (None)",
    );
    // collection-literal element
    rejects(
        "fn main():\n    xs := [print(\"hi\")]\nmain()\n",
        "no value (None)",
    );
    // binary operand
    rejects(
        "fn main():\n    x := 1 + print(\"hi\")\nmain()\n",
        "no value (None)",
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
        "no value (None)",
    );
}

#[test]
fn user_fn_arg_nil_rejected() {
    // a void result passed as a USER function's argument (not a builtin).
    rejects(
        "fn takes(n: int):\n    print(\"{n}\")\nfn main():\n    takes(print(\"hi\"))\nmain()\n",
        "no value (None)",
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
        "fn a() -> None: 10\nfn main():\n    a()\nmain()\n",
        "function returns nothing, cannot return a value",
    );
    // A void fn whose inline expr is itself nil-typed stays legal (implicitly returns nil).
    ok("fn a(): print(\"x\")\nfn main():\n    a()\nmain()\n");
    ok("fn a() -> None: print(\"x\")\nfn main():\n    a()\nmain()\n");
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
fn os_exit_typechecks_in_value_position_as_bottom() {
    // TICKET-077: `os.exit` diverges just like `panic`, so a `match` arm that calls it must be
    // bottom-typed, not `nil`. Currently rejected: "branches have incompatible types: int and nil".
    entry_ok(
        "import std.os\nfn main():\n    r: int!str = !\"boom\"\n    x := match r:\n        ?v: v\n        !e: os.exit(2)\n    print(x)\nmain()\n",
    );
}

#[test]
fn int_comparison_chain_typechecks() {
    // TICKET-077: `0 <= i < n` is a chained comparison, Python-style.
    ok("fn main():\n    i := 2\n    n := 5\n    print(0 <= i < n)\nmain()\n");
}

#[test]
fn a_mixed_type_link_in_a_chain_is_rejected() {
    // TICKET-077: chaining makes `1 < 2 == true` a chain over `(1<2)` and `true`, an int/bool link.
    rejects(
        "fn main():\n    print(1 < 2 == true)\nmain()\n",
        "cannot compare int and bool for equality",
    );
}

#[test]
fn os_exit_from_imported_typechecks_in_value_position_as_bottom() {
    // TICKET-077: same as above, but through `import exit from std.os` and a bare call.
    entry_ok(
        "import exit from std.os\nfn main():\n    r: int!str = !\"boom\"\n    x := match r:\n        ?v: v\n        !e: exit(2)\n    print(x)\nmain()\n",
    );
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
        "import Add from std.ops\nenum Money:\n    Cents(int)\n    fn add(self, other: Money) -> Money:\n        match self:\n            Money.Cents(a):\n                match other:\n                    Money.Cents(b): return Money.Cents(a + b)\nfn twice[T: Add](x: T) -> T:\n    return x + x\nfn main():\n    m := twice(Money.Cents(3))\n    print(m.add(m) == Money.Cents(12))\nmain()\n",
    );
}

// ===== scalar casts =====

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
fn scalar_cast_rejects_struct_enum_fn_arg() {
    // W8-31: int/float/bool over a struct, enum or fn value is outside the scalar-cast domain,
    // same as an aggregate, and always faults at runtime (`{cast}() cannot convert struct`). The
    // checker's allow-list already rejects List/Map/Set/tuple but omits struct/enum/fn — this pins
    // the missing arms.
    rejects(
        "struct P:\n    x: int\nfn main():\n    print(int(P(1)))\nmain()\n",
        "cannot convert struct",
    );
    rejects(
        "enum Color:\n    Red\n    Green\nfn main():\n    print(float(Color.Red))\nmain()\n",
        "float() cannot convert enum",
    );
    rejects(
        "fn g():\n    pass\nfn main():\n    print(bool(g))\nmain()\n",
        "bool() cannot convert function",
    );
    rejects(
        "fn main():\n    f := print\n    print(bool(f))\nmain()\n",
        "cannot convert function",
    );
}

#[test]
fn scalar_cast_rejects_option_result_bytes_bytearray_shared_arg() {
    // TICKET-020: the residual half of W8-31 — Option/Result/bytes/ByteArray/Shared are their own
    // `Ty` variants (not `Ty::Enum`), so TICKET-017's enum arm doesn't catch them. Same shape as
    // struct/enum/fn: check-OK today, then a runtime fault (`{cast}() cannot convert {kind}`).
    rejects(
        "fn main():\n    x: int? = 1\n    print(int(x))\nmain()\n",
        "int() cannot convert an optional value (`int?`) —",
    );
    rejects(
        "fn main():\n    r: int!str = ?1\n    print(int(r))\nmain()\n",
        "int() cannot convert a result value (`int!str`) —",
    );
    rejects(
        "fn main():\n    print(int(b\"ab\"))\nmain()\n",
        "int() cannot convert bytes",
    );
    rejects(
        "fn main():\n    print(int(bytearray([1, 2])))\nmain()\n",
        "int() cannot convert bytearray",
    );
    // `Shared` is import-gated, so only `entry_rejects` (which goes through `build_graph`)
    // resolves it; `rejects` is single-module and reports `unknown type 'Shared'` instead.
    entry_rejects(
        "import Shared from std.concurrency\nfn main():\n    print(int(Shared(1)))\nmain()\n",
        "int() cannot convert Shared",
    );
}

#[test]
fn scalar_cast_rejects_handle_family_arg() {
    // TICKET-020: the same hole for the Channel/Shared handle family DEC-017 named as one unit.
    // `Channel` is a global reserved name (`rejects` resolves it); the rest are import-gated
    // (`entry_rejects`, which goes through `build_graph`).
    rejects(
        "fn f(x: Channel[int]):\n    print(int(x))\nfn main():\n    pass\nmain()\n",
        "int() cannot convert Channel",
    );
    entry_rejects(
        "import Atomic from std.concurrency\nfn f(x: Atomic[int]):\n    print(float(x))\nfn main():\n    pass\nmain()\n",
        "float() cannot convert Atomic",
    );
    entry_rejects(
        "import AtomicInt from std.concurrency\nfn f(x: AtomicInt):\n    print(bool(x))\nfn main():\n    pass\nmain()\n",
        "bool() cannot convert AtomicInt",
    );
    entry_rejects(
        "import RwShared from std.concurrency\nfn f(x: RwShared[int]):\n    print(int(x))\nfn main():\n    pass\nmain()\n",
        "int() cannot convert RwShared",
    );
    entry_rejects(
        "import Executor from std.concurrency\nfn f(x: Executor):\n    print(int(x))\nfn main():\n    pass\nmain()\n",
        "int() cannot convert Executor",
    );
    entry_rejects(
        "import Socket from std.net\nfn f(x: Socket):\n    print(int(x))\nfn main():\n    pass\nmain()\n",
        "int() cannot convert Socket",
    );
    entry_rejects(
        "import Listener from std.net\nfn f(x: Listener):\n    print(int(x))\nfn main():\n    pass\nmain()\n",
        "int() cannot convert Listener",
    );
    entry_rejects(
        "import Writer from std.io\nfn f(x: Writer):\n    print(int(x))\nfn main():\n    pass\nmain()\n",
        "int() cannot convert Writer",
    );
    entry_rejects(
        "import Reader from std.io\nfn f(x: Reader):\n    print(int(x))\nfn main():\n    pass\nmain()\n",
        "int() cannot convert Reader",
    );
    entry_rejects(
        "import ptr from std.ffi\nfn f(x: ptr):\n    print(int(x))\nfn main():\n    pass\nmain()\n",
        "int() cannot convert ptr",
    );
}

#[test]
fn scalar_cast_still_accepts_the_payload_of_a_rejected_kind() {
    // TICKET-020: the rule is "the CONTAINER type is outside the cast domain" — its payload must
    // stay inside it, or the reject fires on the wrong thing.
    ok("fn main():\n    x: int? = 1\n    print(str(x))\n    print(int(x ?? 0))\nmain()\n");
    ok(
        "fn take(r: int!str) -> int:\n    match r:\n        ?v:\n            return int(v)\n        !e:\n            return 0\nfn main():\n    r: int!str = ?1\n    print(take(r))\n    print(str(r))\nmain()\n",
    );
    ok("fn main():\n    print(int(b\"ab\"[0]))\n    print(str(b\"ab\"))\nmain()\n");
    ok(
        "fn main():\n    ba := bytearray([1, 2])\n    print(int(ba[0]))\n    print(str(ba))\nmain()\n",
    );
    entry_ok(
        "import Shared from std.concurrency\nfn main():\n    s := Shared(1)\n    print(int(s.get()))\n    print(str(s))\nmain()\n",
    );
    entry_ok(
        "import Atomic, AtomicInt, RwShared from std.concurrency\nfn main():\n    a := Atomic(1)\n    ai := AtomicInt(1)\n    rw := RwShared(1)\n    print(int(a.load()))\n    print(int(ai.load()))\n    print(int(rw.get()))\nmain()\n",
    );
}

#[test]
fn scalar_cast_still_accepts_scalars_and_str() {
    // W8-31 neighbours: the scalar-cast domain (int/float/bool/str) must stay untouched by the
    // struct/enum/fn reject — a generic type param and str-casting a struct/enum stay clean.
    ok("struct P:\n    x: int\nfn main():\n    print(str(P(1)))\nmain()\n");
    ok("enum C:\n    Red\nfn main():\n    print(str(C.Red))\nmain()\n");
    ok("fn conv[T](x: T) -> int:\n    return int(x)\nfn main():\n    print(conv(1))\nmain()\n");
}

#[test]
fn ok_nil_cannot_be_spelled() {
    // W8-30: `Result[nil, E]` is returned all over the stdlib but there is no way to CONSTRUCT its
    // success value. Per `## Decisions` the chosen spelling is zero-arg `Ok()`, not `Ok(nil)` — `nil`
    // has no expression form (`infer_value` rejects any `Ty::Nil` operand as "returns no value").
    // Until the zero-arg checker arm lands, `Ok()` fails arity checking instead.
    ok("fn f() -> None!str:\n    return\nfn main():\n    pass\nmain()\n");
}

#[test]
fn match_unreachable_arm_after_wildcard_not_warned() {
    // W8-40: a `match` arm made unreachable by an earlier `_` wildcard is silently accepted today
    // (no diagnostic at all) — rustc rejects the analogous shape with `unreachable pattern`. This
    // pins the fix: `warns` requires a clean type-check PLUS a matching warning, so it fails today
    // because the checker emits no warning at all for this shape.
    warns(
        "fn main():\n    x := 1\n    match x:\n        _:\n            print(\"any\")\n        1:\n            print(\"one\")\nmain()\n",
        "unreachable",
    );
}

#[test]
fn match_unreachable_arm_table() {
    // W8-40: every arm after a prior unguarded irrefutable arm is unreachable, not just the
    // `_`-then-literal shape. Table measured in TICKET-017 `## Digest`.
    warns(
        "enum C:\n    Red\n    Green\nfn main():\n    c := C.Red\n    match c:\n        _:\n            print(\"any\")\n        C.Red:\n            print(\"red\")\nmain()\n",
        "unreachable",
    );
    warns(
        "fn main():\n    x := 1\n    match x:\n        _:\n            print(\"a\")\n        _:\n            print(\"b\")\nmain()\n",
        "unreachable",
    );
    warns(
        "fn main():\n    x := 1\n    match x:\n        _:\n            print(\"a\")\n        n:\n            print(n)\nmain()\n",
        "unreachable",
    );
    warns(
        "fn main():\n    x := 1\n    match x:\n        n:\n            print(n)\n        1:\n            print(\"one\")\nmain()\n",
        "unreachable",
    );
    warns(
        "fn main():\n    o: int? = ?1\n    match o:\n        _:\n            print(\"any\")\n        ?v:\n            print(v)\nmain()\n",
        "unreachable",
    );
    warns(
        "fn main():\n    x := 1\n    r := match x:\n        _: \"any\"\n        1: \"one\"\n    print(r)\nmain()\n",
        "unreachable",
    );
    warns(
        "fn main():\n    t := (1, 2)\n    match t:\n        (a, b):\n            print(a)\n        (1, 2):\n            print(\"onetwo\")\nmain()\n",
        "unreachable",
    );
    warns(
        "fn main():\n    x := 1\n    match x:\n        1 | _:\n            print(\"a\")\n        2:\n            print(\"b\")\nmain()\n",
        "unreachable",
    );
}

#[test]
fn match_reachable_arms_do_not_warn() {
    // W8-40 neighbours: a literal-then-wildcard, a guarded catch-all, and range subsumption all
    // stay silent — see `## Decisions`.
    no_warn(
        "fn main():\n    x := 1\n    match x:\n        1:\n            print(\"one\")\n        _:\n            print(\"other\")\nmain()\n",
    );
    no_warn(
        "fn main():\n    x := 1\n    c := true\n    match x:\n        _ if c:\n            print(\"a\")\n        1:\n            print(\"b\")\n        _:\n            print(\"c\")\nmain()\n",
    );
    no_warn(
        "fn main():\n    x := 5\n    match x:\n        0..10:\n            print(\"lo\")\n        5:\n            print(\"five\")\n        _:\n            print(\"hi\")\nmain()\n",
    );
}

#[test]
fn raw_string_is_str_type() {
    // A raw string is plain `str` everywhere a normal string is — annotating it `str` is clean.
    ok("fn main():\n    s: str = r\"\\d+\"\n    print(s)\nmain()\n");
    // ...and using it where an `int` is expected is rejected (proves it's classified `str`, not int).
    rejects("fn main():\n    n: int = r\"x\"\nmain()\n", "str");
}
