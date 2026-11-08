//! Checker tests: Result/Error, recover, closures and `?`, struct iterators, guards, range
//! patterns, Iterator[T], slicing, defer, `?.`/`??`, sort_by_key.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== Go-style Result[T, E] + Error protocol (M11 Phase A) =====

#[test]
fn result_two_type_params_ok() {
    ok("fn q() -> int!str:\n    return !\"bad\"\n");
}

#[test]
fn bang_shorthand_with_error_type_ok() {
    // `T!E` == `Result[T, E]`.
    ok("fn q() -> int!str:\n    return !\"bad\"\n");
}

#[test]
fn err_payload_typed_as_concrete_err() {
    // When `E` is `str`, the bound `Err` payload is a `str` — str methods available.
    ok(
        "fn q() -> int!str:\n    return !\"bad\"\nfn main():\n    match q():\n        ?v: print(v)\n        !e: print(e.trim())\nmain()\n",
    );
}

#[test]
fn custom_struct_error_ok() {
    ok(
        "struct DbErr:\n    code: int\n    fn message(self) -> str:\n        return \"db\"\nfn q() -> int!DbErr:\n    return !DbErr(503)\n",
    );
}

#[test]
fn error_protocol_existential_accepts_str() {
    // `Error` used as a value type; `str` conforms; only `message()` is available on it.
    ok(
        "fn q() -> int!Error:\n    return !\"bad\"\nfn main():\n    match q():\n        ?v: print(v)\n        !e: print(e.message())\nmain()\n",
    );
}

#[test]
fn bang_default_error_is_error_protocol() {
    // `T!` defaults `E` to the `Error` protocol; the payload supports `.message()`.
    ok(
        "fn q() -> int!:\n    return !\"bad\"\nfn main():\n    match q():\n        ?v: print(v)\n        !e: print(e.message())\nmain()\n",
    );
}

#[test]
fn default_error_existential_rejects_str_methods() {
    // `Error` existential exposes only `message()` — not `str`'s methods.
    rejects(
        "fn q() -> int!:\n    return !\"x\"\nfn main():\n    match q():\n        ?v: print(v)\n        !e: print(e.trim())\nmain()\n",
        "trim",
    );
}

#[test]
fn struct_error_without_message_rejected_as_error() {
    // A struct lacking `message(self) -> str` does not satisfy `Error`, so it can't be the
    // payload where `Error` is expected — the return-type check flags the mismatch.
    rejects(
        "struct Bad:\n    n: int\nfn q() -> int!Error:\n    return !Bad(1)\n",
        "Bad",
    );
}

// ===== recover: boundary (M11 Phase B) =====

#[test]
fn recover_yields_result_of_block_value() {
    // `recover:` evaluates to Result[T, Error]; matching Ok/Err is well-typed.
    ok(
        "fn main():\n    r := recover: [1, 2][0]\n    match r:\n        ?v: print(v)\n        !e: print(e.message())\nmain()\n",
    );
}

#[test]
fn recover_value_composes_with_question_mark() {
    // The recover result is an ordinary Result, usable with `?`.
    ok("fn run() -> int!:\n    r := recover: 99\n    v := r?\n    return ?v\n");
}

#[test]
fn recover_block_rejects_return() {
    rejects(
        "fn f() -> int!:\n    r := recover:\n        return ?1\n    return r\n",
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
        "fn main():\n    r := recover:\n        for i in 0..3:\n            if i == 1: break\n    match r:\n        ?_: print(1)\n        !e: print(e.message())\nmain()\n",
    );
}

#[test]
fn recover_question_mark_allowed_in_non_result_fn() {
    // `?` targets the recover boundary, so the enclosing fn need not return Result.
    ok(
        "fn risky() -> int!:\n    return !\"x\"\nfn compute() -> str:\n    r := recover:\n        v := risky()?\n        v\n    match r:\n        ?v: return \"ok\"\n        !e: return e.message()\n",
    );
}

#[test]
fn recover_question_mark_on_option_rejected() {
    rejects(
        "fn find() -> int?:\n    return None\nfn main():\n    r := recover:\n        v := find()?\n        v\n    print(r)\nmain()\n",
        "'?' is not allowed inside a recover block: the operand is a `T?` value",
    );
}

// The consistency invariant the bug violated: a direct-panic recover and a match-all-panic recover
// must both accept (both have a bottom `Ok` payload). Pre-fix the second was rejected at (1,1).
#[test]
fn recover_payload_consistent_direct_vs_match_panic() {
    // r1: the inline form's payload is bottom, so it binds to `int!`.
    entry_ok(
        "fn main():\n    r: int! = recover: panic(\"x\")\n    match r:\n        ?v: print(\"got {v}\")\n        !e: print(\"err\")\nmain()\n",
    );
    // r2: the block form has no tail value (TICKET-260): it is `!`, so it binds to `!` and not `int!`.
    entry_ok(
        "fn main():\n    r: ! = recover:\n        match 1:\n            _: panic(\"boom\")\n    match r:\n        ?_: print(\"got\")\n        !e: print(\"err\")\nmain()\n",
    );
}

// Regression fence: a concrete-tail recover keeps its concrete payload (the divergence upgrade only
// fires on a provably-diverging nil tail), and a non-diverging statement tail (a `let`) still yields
// `Result[nil]` so its `Ok(v)` is correctly nil-banned in value position.
#[test]
fn recover_non_never_value_unaffected() {
    // Concrete value tail -> Ok payload is `int`, interpolation accepts.
    entry_ok(
        "fn main():\n    r := recover: 5\n    match r:\n        ?v: print(\"v={v}\")\n        !e: print(\"err\")\nmain()\n",
    );
    // Non-diverging statement tail (a `let`) -> Ok payload stays nil -> value use rejected.
    entry_rejects(
        "fn main():\n    r := recover:\n        x := 5\n    match r:\n        ?v: print(\"{v}\")\n        !e: print(\"err\")\nmain()\n",
        "expression returns no value (None) and cannot be used as a value",
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
        "fn foo(cmd: str):\n    r := recover:\n        match cmd:\n            \"a\": \"hello\"\n            _: 42\n    match r:\n        ?_: print(\"done\")\n        !e: print(\"failed\")\nfoo(\"a\")\n",
    );
    // void-call arm (nil) mixed with an int arm — same fall-back to `Result[nil]`.
    entry_ok(
        "fn logit():\n    print(\"log\")\nfn foo(cmd: str):\n    r := recover:\n        match cmd:\n            \"log\": logit()\n            _: 42\n    match r:\n        ?_: print(\"done\")\n        !e: print(\"failed\")\nfoo(\"log\")\n",
    );
    // Because the block is `Result[nil]`, binding the value and USING it is still nil-banned (proves
    // the fall-back really is nil, so the heterogeneous runtime payload is never observable).
    entry_rejects(
        "fn foo(cmd: str):\n    r := recover:\n        match cmd:\n            \"a\": \"hello\"\n            _: 42\n    match r:\n        ?v: print(v)\n        !e: print(\"failed\")\nfoo(\"a\")\n",
        "expression returns no value (None) and cannot be used as a value",
    );
}

// REGRESSION GUARD (if analog): heterogeneous `if/else` branches fall back to `Result[nil]`, not
// rejected.
#[test]
fn recover_tail_stmt_if_heterogeneous_branches_falls_back_to_nil() {
    entry_ok(
        "fn foo(n: int):\n    r := recover:\n        if n == 0:\n            \"zero\"\n        else:\n            n\n    match r:\n        ?_: print(\"done\")\n        !e: print(\"failed\")\nfoo(0)\n",
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
        "expected exactly one None error, got: {errs:?}"
    );
    let span = nil_errs[0].span;
    assert_eq!(
        span.line, 4,
        "None-fragment error should point at the print line, got: {span}"
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
        "fn parse(s: str) -> int!:\n    return !\"x\"\nfn main():\n    ys := [\"2\"].map(fn(s: str) -> int: parse(s)? * 2)\n    print(ys)\nmain()\n",
        "not a `T?` or `T!E` value",
    );
}

#[test]
fn closure_question_mark_on_result_return_ok() {
    // A closure declared to return Result may use `?` (yields the Ok type).
    ok(
        "fn parse(s: str) -> int!:\n    return ?2\nfn main():\n    rs := [\"2\"].map(fn(s: str) -> int!: ?(parse(s)? * 2))\n    print(rs)\nmain()\n",
    );
}

#[test]
fn closure_question_mark_inferred_return_rejected() {
    // No return annotation → `?` has no Result/Option context → rejected (annotate to allow).
    rejects(
        "fn parse(s: str) -> int!:\n    return !\"x\"\nfn main():\n    ys := [\"2\"].map(fn(s): parse(s)?)\n    print(ys)\nmain()\n",
        "a `T?` or `T!E` value",
    );
}

// ===== struct iterator protocol: `for x in s` when `s` has `next(self) -> Option[T]` =====

#[test]
fn iterates_struct_with_next_ok() {
    // A struct with `next(self) -> Option[int]` is iterable; `x` binds the element type (int).
    ok(
        "struct Counter:\n    n: int\n    limit: int\n    fn next(self) -> int?:\n        if self.n >= self.limit:\n            return None\n        v := self.n\n        self.n = self.n + 1\n        return ?v\nfn main():\n    for x in Counter(0, 5):\n        print(x)\nmain()\n",
    );
}

#[test]
fn struct_iter_two_vars_rejected() {
    // A struct iterator binds exactly one loop variable (no key/value form).
    rejects(
        "struct Counter:\n    n: int\n    limit: int\n    fn next(self) -> int?:\n        if self.n >= self.limit:\n            return None\n        v := self.n\n        self.n = self.n + 1\n        return ?v\nfn main():\n    for k, v in Counter(0, 5):\n        print(k)\nmain()\n",
        "tuple pattern cannot match a value of type int",
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
        "struct Counter:\n    n: int\n    limit: int\n    fn next(self) -> int?:\n        if self.n >= self.limit:\n            return None\n        v := self.n\n        self.n = self.n + 1\n        return ?v\nfn main():\n    for x in Counter(0, 5):\n        print(x + \"s\")\nmain()\n",
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
        "fn f(x: int?) -> str:\n    match x:\n        None: return \"none\"\n        ?0: return \"zero\"\nf(?5)\n",
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
        "fn f(x: int??) -> int:\n    match x:\n        None: return -1\n        ?(?v): return v\nf(?None)\n",
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
    // `None` is a registered variant; binding it against an int would trap
    // on the VM (the compiler routes by the variant registry). Reject at check time instead.
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

/// The position of the first error whose message contains `needle`.
fn error_pos(src: &str, needle: &str) -> (usize, usize) {
    let errs = check_src(src);
    let e = errs
        .iter()
        .find(|e| e.message.contains(needle))
        .unwrap_or_else(|| panic!("expected an error containing {needle:?}, got: {errs:?}"));
    (e.span.line as usize, e.span.col as usize)
}

#[test]
fn duplicate_match_arm_points_at_the_arm() {
    // TICKET-149 (1): the error names the offending arm, not the scrutinee.
    let pos = error_pos(
        "enum E:\n    A\n    B\ne := E.A\nmatch e:\n    E.A: print(1)\n    E.B: print(2)\n    E.A: print(3)\n",
        "duplicate match arm 'A'",
    );
    assert_eq!(pos, (8, 5));
}

#[test]
fn not_a_constructor_points_at_the_arm() {
    let pos = error_pos(
        "struct IA:\n    x: int\nv := IA(1)\ny := match v:\n    IA(x): x\n    IB(z): z\n",
        "'IB' is not a constructor of IA",
    );
    assert_eq!(pos, (6, 5));
}

#[test]
fn recover_tail_duplicate_arm_points_at_the_arm() {
    let pos = error_pos(
        "enum E:\n    A\n    B\ne := E.A\nr := recover:\n    match e:\n        E.A: 1\n        E.B: 2\n        E.A: 3\n",
        "duplicate match arm 'A'",
    );
    assert_eq!(pos, (9, 9));
}

#[test]
fn duplicate_variant_points_at_the_variant() {
    let pos = error_pos(
        "enum E:\n    A\n    B\n    A\n",
        "variant 'A' is already defined in enum 'E'",
    );
    assert_eq!(pos, (4, 5));
}

#[test]
fn range_pattern_empty_or_inverted_rejected() {
    // TICKET-149 (2): `5..1` (inverted) matches nothing; rustc rejects with E0579.
    rejects(
        "n := 3\nmatch n:\n    5..1: print(\"never\")\n    _: pass\n",
        "empty range pattern",
    );
}

#[test]
fn range_pattern_empty_rejected_at_every_position() {
    // Top level, inside an or-alternative, nested in a payload, a negative inverted bound, and an
    // expression-position match arm. Each is reported once, at its own arm's line.
    let errs = check_src(
        "o: int? = ?3\nmatch o:\n    ?5..1: print(1)\n    _: pass\nn := 3\nmatch n:\n    1 | 5..1: print(1)\n    3..3: print(2)\n    0..-5: print(3)\n    _: pass\nk := match n:\n    9..2: 1\n    _: 0\n",
    );
    let mut lines: Vec<u32> = errs
        .iter()
        .filter(|e| e.message.contains("empty range pattern"))
        .map(|e| e.span.line)
        .collect();
    lines.sort();
    assert_eq!(lines, vec![3, 7, 8, 9, 12], "all errors: {errs:?}");
}

#[test]
fn range_pattern_nonempty_neighbours_ok() {
    // One-wide and negative ranges are non-empty: `start < end` on each side of the line.
    ok(
        "n := 3\nmatch n:\n    0..1: print(1)\n    -10..-5: print(2)\n    -1..0: print(3)\n    _: pass\n",
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
    // `Range(a, b, step)` of ints type-checks and iterates; the removed `range()` call is a parse error.
    ok(
        "fn main():\n    a: List[int] = List.from_iter(Range(0, 10, 2))\n    print(a.len())\nmain()\n",
    );
    ok(
        "fn main():\n    a: List[int] = List.from_iter(Range(10, 0, -1))\n    print(a.len())\nmain()\n",
    );
    ok("fn main():\n    for i in Range(0, 10, 2):\n        print(i)\nmain()\n");
}

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
        return ?self.n
";

#[test]
fn iterator_bound_over_list_ok() {
    // `[S: Iterator[T], T]` accepts a list CURSOR and recovers its element type. (A raw list is
    // `Iterable`, not `Iterator` — see `raw_collection_does_not_satisfy_iterator`.)
    ok(
        "import Iterator from std.iter\nfn first[S: Iterator[T], T](xs: S, d: T) -> T:\n    for x in xs:\n        return x\n    return d\nv := first([1, 2, 3].iter(), 0)\n",
    );
}

#[test]
fn iterator_bound_recovers_element_type() {
    // first([1,2,3].iter(), 0) is int, so binding the result to a str must be rejected — proves T = int
    // flowed out of the iterand's element type, not stayed erased.
    rejects(
        "import Iterator from std.iter\nfn first[S: Iterator[T], T](xs: S, d: T) -> T:\n    for x in xs:\n        return x\n    return d\ns: str = first([1, 2, 3].iter(), 0)\n",
        "cannot assign",
    );
}

#[test]
fn iterator_bound_over_noniterable_rejected() {
    rejects(
        "import Iterator from std.iter\nfn first[S: Iterator[T], T](xs: S, d: T) -> T:\n    return d\nv := first(5, 0)\n",
        "does not satisfy Iterator",
    );
}

#[test]
fn raw_collection_does_not_satisfy_iterator() {
    // W6-3b: a RAW collection holds no cursor position, so `next` on it can never work (it faulted at
    // runtime). `Iterator` now means cursor / generator / `next`-struct; a raw collection satisfies
    // only `Iterable`. The element arg matches the receiver's element type in each probe, so the ONE
    // error is the grant refusal, not an element-recovery mismatch.
    for (elem, recv) in [
        ("int", "[1, 2, 3]"),
        ("int", "Set.from_iter([1, 2])"),
        ("(str, int)", "{\"a\": 1}"), // a map's element is its `(key, value)` pair (owner U9)
        ("str", "\"abc\""),
        ("int", "b\"abc\""),
        ("int", "bytearray.from_iter(b\"abc\")"),
    ] {
        rejects(
            &format!(
                "import Iterator from std.iter\nfn f[T: Iterator[{elem}]](c: T):\n    pass\nf({recv})\n"
            ),
            "Iterable",
        );
    }
}

/// W6-3b review (charge `correctness-1`) — the migration advice is only appended for a receiver the
/// advice APPLIES to. A raw collection is iterable-but-position-less, so `for` / `.iter()` /
/// `[S: Iterable[T], T]` are all real remedies. A NON-iterable receiver (`int`, a plain struct) can do
/// none of them, so it keeps the bare pre-W6-3b message instead of being misdirected.
#[test]
fn iterator_bound_advice_only_when_the_receiver_is_iterable() {
    let joined = |src: &str| -> String {
        let errs = check_src(src);
        assert!(!errs.is_empty(), "expected a type error for: {src:?}");
        errs.iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
            .join(" | ")
    };
    let msg_list = joined(
        "import Iterator from std.iter\nfn f[T: Iterator[int]](c: T):\n    pass\nf([1, 2, 3])\n",
    );
    assert!(
        msg_list.contains("Iterable") && msg_list.contains(".iter()"),
        "an iterable receiver should get the migration advice, got: {msg_list}"
    );
    for src in [
        "import Iterator from std.iter\nfn f[T: Iterator[int]](c: T):\n    pass\nf(42)\n",
        "import Iterator from std.iter\nstruct P:\n    v: int\nfn f[T: Iterator[int]](c: T):\n    pass\nf(P(1))\n",
    ] {
        let msg = joined(src);
        assert!(
            msg.contains("does not satisfy Iterator"),
            "expected the plain refusal, got: {msg}"
        );
        assert!(
            !msg.contains(".iter()") && !msg.contains("Iterable"),
            "a NON-iterable receiver must not be told to use `for`/`.iter()`/`Iterable`, got: {msg}"
        );
    }
}

#[test]
fn iterator_bound_accepts_cursor_generator_and_next_struct() {
    // The narrowed grant's ACCEPT side: a `.iter()` cursor, a generator, a structural `next` struct.
    ok(
        "import Iterator from std.iter\nfn f[T: Iterator[int]](c: T):\n    pass\nf([1, 2, 3].iter())\n",
    );
    ok(
        "import Iterator from std.iter\nfn f[T: Iterator[int]](c: T):\n    pass\nfn g() -> Iterator[int]:\n    yield 1\nf(g())\n",
    );
    ok(&format!(
        "import Iterator from std.iter\n{COUNTER}fn f[T: Iterator[int]](c: T):\n    pass\nf(Counter(3))\n"
    ));
    // `for` over a raw collection is untouched.
    ok("for x in [1, 2, 3]:\n    print(x)\n");
}

#[test]
fn iterable_bound_recovers_element_type() {
    // W6-3b's migration target: `[S: Iterable[T], T]` must recover T from the iterand, exactly like
    // the `Iterator` bound did — otherwise there is nowhere for the raw-collection callers to go.
    ok(
        "import Iterable from std.iter\nfn count[S: Iterable[T], T](xs: S) -> int:\n    n := 0\n    for _ in xs:\n        n = n + 1\n    return n\nv := count([1, 2, 3])\n",
    );
    // The `List[str]` annotation proves T=str was really recovered, not erased to Unknown.
    ok(
        "import Iterable from std.iter\nfn to_list[S: Iterable[T], T](xs: S) -> List[T]:\n    out: List[T] = []\n    for x in xs:\n        out.push(x)\n    return out\nr: List[str] = to_list(\"ab\")\n",
    );
    // …and a WRONG element still rejects — the recovered `int` is checked against the already-pinned
    // `str` (widening the bound must not widen what conforms). RED before the `Iterable` recovery: the
    // element check never ran for an `Iterable` bound, so this message did not exist.
    rejects(
        "import Iterable from std.iter\nfn f[S: Iterable[T], T](xs: S, d: T):\n    print(d)\nf([1, 2, 3], \"a\")\n",
        "iterator element type int does not match the declared element type str",
    );
}

#[test]
fn iterator_bound_over_user_struct_ok() {
    let src = format!(
        "import Iterator from std.iter\n{COUNTER}fn first[S: Iterator[T], T](xs: S, d: T) -> T:\n    for x in xs:\n        return x\n    return d\nv := first(Counter(3), 0)\n"
    );
    ok(&src);
}

#[test]
fn iterator_loop_var_typed_as_element() {
    // Inside the generic body the loop var must be the element type `T` (a param), NOT `Unknown`:
    // assigning it to an `int` slot is rejected at definition time. If it were `Unknown` this would
    // type-check (Unknown is assignable to anything), so this discriminates element-typing.
    rejects(
        "import Iterator from std.iter\nfn f[S: Iterator[T], T](xs: S):\n    for x in xs:\n        y: int = x\n        print(y)\n",
        "cannot assign",
    );
}

#[test]
fn iterator_bound_wrong_arity_rejected() {
    rejects(
        "import Iterator from std.iter\nfn f[S: Iterator](xs: S):\n    print(1)\n",
        "Iterator' takes 1 type argument",
    );
}

#[test]
fn nonparam_protocol_with_args_rejected() {
    rejects(
        "import Comparable from std.cmp\nfn f[T: Comparable[int]](a: T) -> T:\n    return a\n",
        "takes no type arguments",
    );
}

#[test]
fn iterator_adapter_struct_ok() {
    // A `Take` adapter wraps an inner iterator bounded `I: Iterator[T]`. Inside `next`,
    // `self.inner.next()` must yield `T?` (the element), so `return Some(x)` type-checks against
    // the declared `T?` return. This proves `.next()` on a bounded param recovers the element type.
    let src = "\
import Iterator from std.iter
struct Take[I: Iterator[T], T]:
    inner: I
    left: int
    fn next(self) -> T?:
        if self.left <= 0:
            return None
        self.left -= 1
        return self.inner.next()
fn main():
    t := Take([1, 2, 3].iter(), 2)
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
        "import Iterator from std.iter\nfn count[S: Iterator[T], T](xs: S) -> int:\n    n := 0\n    for _ in xs:\n        n = n + 1\n    return n\nfn wrap[S: Iterator[T], T](xs: S) -> int:\n    return count(xs)\nv := wrap([1, 2, 3].iter())\n",
    );
}

#[test]
fn iterator_conflicting_explicit_element_arg_rejected() {
    // Explicit `[Iterator[int], str]` pins T=str, but the cursor's element is int — the recovered element
    // must conflict (unsound otherwise: static List[str], runtime List[int]).
    rejects(
        "import Iterator from std.iter\nfn to_list[S: Iterator[T], T](xs: S) -> List[T]:\n    out := []\n    for x in xs:\n        out.push(x)\n    return out\nr := to_list[Iterator[int], str]([1, 2, 3].iter())\n",
        "does not match the declared element type",
    );
}

#[test]
fn iterator_bound_unknown_element_type_rejected() {
    // `Bogus` is neither a declared type param nor a known type — a bound's args are resolved, so
    // this is reported rather than silently accepted.
    rejects(
        "import Iterator from std.iter\nfn f[S: Iterator[Bogus]](xs: S):\n    print(1)\n",
        "Bogus",
    );
}

#[test]
fn iterator_adapter_element_mismatch_rejected() {
    // `self.inner.next()` is `T?`; returning a literal `?\"x\"` (str) where the declared return
    // is `T?` is caught — guards the element typing of `.next()`.
    let src = "\
import Iterator from std.iter
struct Bad[I: Iterator[T], T]:
    inner: I
    fn next(self) -> T?:
        return ?\"x\"
fn main():
    b := Bad([1, 2, 3].iter())
    for x in b:
        print(x)
main()
";
    rejects(src, "'?' value: expected T, found str");
}

// ===== slicing + Index/IndexSet/Slice protocols =====

#[test]
fn slice_of_list_types_as_list() {
    ok("xs := [1, 2, 3, 4]\nys: List[int] = xs[1..3]\n");
    // Optional bounds / step still type as the same container type.
    ok("xs := [1, 2, 3, 4]\nys: List[int] = xs[1..]\n");
    ok("xs := [1, 2, 3, 4]\nys: List[int] = xs[..3]\n");
    ok("xs := [1, 2, 3, 4]\nys: List[int] = xs[Range(None, None, 2)]\n");
    rejects(
        "xs := [1, 2, 3, 4]\nys: str = xs[1..3]\n",
        "cannot assign List[int] to variable of type str",
    );
}

#[test]
fn slice_of_str_types_as_str() {
    ok("s := \"hello\"\nt: str = s[0..2]\n");
    rejects(
        "s := \"hello\"\nn: int = s[0..2]\n",
        "cannot assign str to variable of type int",
    );
}

#[test]
fn slice_bounds_must_be_int() {
    rejects(
        "xs := [1, 2, 3]\nys := xs[\"a\"..2]\n",
        "range bound must be int, found str",
    );
    rejects(
        "xs := [1, 2, 3]\nys := xs[0..\"b\"]\n",
        "range bound must be int, found str",
    );
    // `Range(a, b, step)` takes ints and `None`: a str step is an argument type error.
    rejects(
        "xs := [1, 2, 3]\nys := xs[Range(None, None, \"c\")]\n",
        "Range",
    );
}

#[test]
fn map_is_not_sliceable() {
    // A map subscript is a key lookup: a `Range` is not an `int` key.
    rejects(
        "m: Map[int, int] = {}\nx := m[0..2]\n",
        "map key must be int, found Range",
    );
    // A map keyed by `Range` looks the key up.
    ok("m: Map[Range, int] = {}\nx := m[0..2]\n");
}

const BUF: &str = "\
struct Buf:
    xs: List[int]
    fn index(self, k: int) -> int:
        return self.xs[k]
    fn set_index(self, k: int, v: int):
        self.xs[k] = v
    fn slice(self, r: Range) -> Buf:
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
    // Open bounds and a stepped range all route through the same protocol method.
    ok(&format!("{BUF}b := Buf([1, 2, 3])\nc: Buf = b[..]\n"));
    ok(&format!(
        "{BUF}b := Buf([1, 2, 3])\nc: Buf = b[Range(None, None, -1)]\n"
    ));
    // A range VALUE slices the same way, and so does the old three-parameter shape NOT.
    ok(&format!(
        "{BUF}b := Buf([1, 2, 3])\nr := 1..3\nc: Buf = b[r]\n"
    ));
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
    fn index(self, k: int) -> int:
        return self.xs[k]
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
    fn index(self, k: int) -> int:
        return self.xs[k]
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
        "import Index from std.ops\n{BUF}fn first[C: Index[int, V], V](c: C) -> V:\n    return c[0]\n\
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
    fn set_index(self, k: int, v: int):
        self.xs[k] = v
";
    rejects(
        &format!("{set_only}b := WO([1, 2, 3])\nb[0] = 9\n"),
        "cannot index-assign into WO",
    );
}

#[test]
fn struct_slice_wrong_bound_types_rejected() {
    // The `Slice` protocol fixes the bounds as `slice(self, int?, int?, int?)` — the VM passes
    // three `Option[int]` components. A `slice` with non-`int?` bounds (or wrong arity) must NOT
    // count as a valid `Slice` impl (would crash).
    let bad = "\
struct BadSlice:
    xs: List[int]
    fn slice(self, start: str, end: str) -> int:
        return self.xs.len()
";
    rejects(
        &format!("{bad}b := BadSlice([1, 2, 3])\nc := b[0..2]\n"),
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
        &format!("{two_arg}b := TwoArg([1, 2, 3])\nc := b[0..2]\n"),
        "cannot slice TwoArg",
    );
}

#[test]
fn generic_indexset_param_assign_ok() {
    // A bounded `[C: IndexSet[int, int]]` param is index-assignable inside the generic body.
    ok(&format!(
        "import IndexSet from std.ops\n{BUF}fn put[C: IndexSet[int, int]](c: C):\n    c[0] = 42\n\
         b := Buf([1, 2, 3])\nput(b)\n"
    ));
}

#[test]
fn generic_indexset_rejects_str() {
    // `str` is immutable — it satisfies `Index` but NOT `IndexSet`, so a generic bounded by
    // `IndexSet` must reject a str argument.
    rejects(
        &format!(
            "import IndexSet from std.ops\n{BUF}fn put[C: IndexSet[int, int]](c: C):\n    c[0] = 42\nput(\"hi\")\n"
        ),
        "does not satisfy IndexSet",
    );
}

#[test]
fn generic_over_slice_protocol_ok() {
    // A struct AND a built-in list both satisfy `Slice[R]`; the bound recovers `R`.
    ok(&format!(
        "import Slice from std.ops\n{BUF}fn mid[C: Slice[R], R](c: C) -> R:\n    return c[1..2]\n\
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
    // then-branch (an `Obj::Builtin` is truthy). `print` now types as a `Ty::Func`, which
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
fn top_level_read_above_a_global_is_the_builtin() {
    // CPython `x = chr` above `chr = 'z'` binds the builtin; the compiler reads the checker's
    // `Builtin` record, so the old VM `nil`-slot reason is gone.
    ok("x := chr\nchr := \"z\"\nprint(x(97))\n");
    // A name with no builtin meaning read above its `:=` is still unknown at top level.
    rejects("x := y\ny := 5\nprint(x)\n", "unknown name 'y'");
    // A body sees the seeded global in both orders (TICKET-180 step 7).
    ok("fn w() -> str:\n    return chr(97)\nchr := fn(n: int) -> str: \"z\"\nprint(w())\n");
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
    // The container ctors (List/Map/Set) are `Builtin` ids with `first_class() == false`, so they
    // are NOT first-class values — a
    // value-position use stays REJECTED, on the same fall-through path as the scalar ctors (`f := int`)
    // and user struct ctors (`f := Point`). The `first_class == false` gate keeps them off the
    // `LoadBuiltin` arm.
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
    // The scalar-conversion ctors (int/float/str/bytes/bytearray) are `Builtin` ids with
    // `first_class() == false`, so they are NOT first-class values — a
    // value-position use (`f := int`) stays REJECTED, on the same fall-through path as `f := List`
    // (the `first_class == false` gate keeps them off the `LoadBuiltin` arm).
    rejects("fn w():\n    f := int\n", "int");
    rejects("fn w():\n    f := float\n", "float");
    rejects("fn w():\n    f := str\n", "str");
    rejects("fn w():\n    f := bytes\n", "bytes");
    rejects("fn w():\n    f := bytearray\n", "bytearray");
    // A scalar ctor in `defer` single-call position must still be wrapped (not first-class).
    rejects(
        "fn w():\n    defer str()\n",
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
    // Reading a capture into a NEW binding is fine, and so is reading a captured closure (same
    // task).
    ok(
        "fn w():\n    x := 1\n    g := fn(): print(\"g\")\n    defer:\n        y := x + 1\n        print(\"{y}\")\n        g()\n",
    );
}

// ===== optional chaining `?.` + null-coalescing `??` =====
//
// The carrier survives desugar (W7-43); the checker picks the lowering from the operand's type —
// `match Some/None` for an Option, `?` then `.` for a Result. `??` takes both carriers (TICKET-039):
// `Option` unwraps to the payload, `Result` unwraps and DISCARDS the error (Rust's `unwrap_or`).

#[test]
fn null_coalesce_some_picks_value() {
    ok_desugared("o := ?5\nx: int = o ?? 0\nprint(x)\n");
}

#[test]
fn null_coalesce_result_type() {
    // `a ?? b` evaluates to the inner type, usable in arithmetic.
    ok_desugared("o := ?5\ny := (o ?? 0) + 1\nprint(y)\n");
}

#[test]
fn null_coalesce_lhs_may_be_a_result() {
    // OLD expectation (pre-TICKET-039): `r ?? 0` on a `Result` was one error,
    // "'??' applies to an Option, found int!". TICKET-039 widened `??` to accept a `Result`,
    // discarding the error like Rust's `unwrap_or`.
    ok_desugared("r := ?5\nx := r ?? 0\n");
}

#[test]
fn null_coalesce_on_result_one_error() {
    // OLD expectation (pre-TICKET-039): the full W7-43 message, "'??' applies to an Option, found
    // str!str — a Result carries an error that must be handled: use a match with Ok/Err
    // arms". TICKET-039's `Result` arm now type-checks `f() ?? "d"` against `f()`'s `Ok` payload —
    // this is the branch-compatibility check the old uniform rejection MASKED.
    let errs = check_desugared("fn f() -> int!str:\n    return ?5\nx := f() ?? \"d\"\n");
    assert_eq!(errs.len(), 1, "exactly one error, got: {errs:?}");
    assert!(
        errs[0]
            .message
            .contains("'??' sides have incompatible types: int and str"),
        "got: {errs:?}"
    );
}

#[test]
fn null_coalesce_result_discards_error_ticket_039() {
    // TICKET-039: `??` gives Result the same explicit-discard fallback Option has
    // (Rust: `r.unwrap_or(0)`). Was RED before that landed — `??` used to be Option-only
    // and this program was rejected with *'??' applies to an Option, found Result[int, str]*.
    ok_desugared("fn g() -> int!str:\n    return !\"boom\"\nr := g()\nx := r ?? 0\nprint(x)\n");
}

#[test]
fn null_coalesce_nested_option_over_result_keeps_two_modes() {
    // `(o() ?? fb) ?? 0`: the inner `??`'s operand is `Option[int!str]` (Option mode), the outer's
    // operand is `int!str` (ResultCoalesce mode). Both nodes share `a`'s `Expr::span` — this is the
    // program that hard-errors under an `internal:` CompileError if the two carriers shared one
    // `CarrierKey` instead of being keyed on `op_span`.
    ok_desugared(
        "fn o() -> (int!str)?:\n    return None\nfb: int!str = ?7\nx := (o() ?? fb) ?? 0\n",
    );
}

#[test]
fn null_coalesce_on_a_non_carrier_one_error() {
    let errs = check_desugared("x := 5\ny := x ?? 0\n");
    assert_eq!(errs.len(), 1, "exactly one error, got: {errs:?}");
    assert!(
        errs[0]
            .message
            .contains("'??' applies to a `T?` or `T!E` value, found int"),
        "got: {errs:?}"
    );
}

#[test]
fn null_coalesce_operand_error_is_reported_exactly_once() {
    // `infer_null_coalesce` infers the LHS ONCE and substitutes a pre-typed scratch into the lowered
    // clone, so the clone never re-infers it. The zero half: an undefined operand must still report.
    let errs = check_desugared("y := nope ?? 0\n");
    assert_eq!(
        errs.len(),
        1,
        "the operand's error, reported once — not zero, not twice: {errs:?}"
    );
    assert!(errs[0].message.contains("unknown name 'nope'"), "{errs:?}");

    // …and the twice half: the operand types fine as an Option, so the clone IS inferred — the
    // operand's own error must survive exactly once, not twice and not zero times.
    let errs = check_desugared("fn mk(a: int) -> int?:\n    return ?a\nr := mk(\"bad\") ?? 0\n");
    assert_eq!(errs.len(), 1, "reported once, not twice: {errs:?}");
    assert!(
        errs[0]
            .message
            .contains("argument 1 of 'mk': expected int, found str"),
        "{errs:?}"
    );
}

#[test]
fn opt_chain_on_non_option_does_not_leak_temp_name() {
    // `x?.f` on a non-Option is an error, but the desugared match's internal `__opt` binding must
    // NOT leak into a spurious "unknown name '__opt…'" cascade.
    let errs = check_desugared("x := 5\ny := x?.f\n");
    assert!(!errs.is_empty(), "non-option opt-chain should error");
    assert!(
        !errs.iter().any(|e| e
            .message
            .contains(crate::ast::CarrierTemp::Payload.prefix())),
        "internal temp name leaked into a diagnostic: {errs:?}"
    );
}

/// A call under `?.` that fails reports what `.` reports, once: no `__opt` temp in the message
/// (the receiver is not printed) and no "returns no value" / index cascade after it. A void call
/// with a bad argument still reports the argument once (TICKET-239).
#[test]
fn a_failed_optchain_call_reports_once_and_names_no_temp() {
    let k = "import Into from std.convert\nstruct K:\n    n: int\n    fn into(self) -> int:\n        return self.n\n    fn into(self) -> str:\n        return \"k\"\n    fn get[U](self, u: U) -> U:\n        return u\n    fn put(self, x: int):\n        pass\n";
    let cells = [
        (
            "    r: K? = K(1)\n    x := r?.into()\n    print(x)\n",
            "into on K needs a target type",
        ),
        (
            "    r: K? = K(1)\n    xs := [10]\n    g := fn(): r?.get[xs[0]](3)\n    print(g())\n",
            "'get' is a method",
        ),
        (
            "    r: K? = K(1)\n    r?.put(\"s\")\n",
            "expected int, found str",
        ),
    ];
    let mut failed = Vec::new();
    for (body, needle) in cells {
        let src = format!("{k}fn main():\n{body}main()\n");
        let errs = check_desugared(&src);
        if errs.len() != 1
            || !errs[0].message.contains(needle)
            || errs[0]
                .message
                .contains(crate::ast::CarrierTemp::Payload.prefix())
        {
            failed.push(format!("{body}=> wanted one {needle:?}, got {errs:?}"));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// A failed pick under `?.` in statement position reports what `.` reports, once: the pick error
/// and no discarded-value warning (the failed call has no value to discard).
#[test]
fn a_failed_optchain_pick_statement_reports_only_the_pick_error() {
    let src = "import Into from std.convert\nstruct R:\n    n: int\n    fn into(self) -> float:\n        return 1.0\n    fn into(self) -> str:\n        return \"s\"\nr: R? = ?R(1)\nr?.into()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let (res, warns) = check_graph_diags(&graph, None);
    let errs = res.expect_err("the pick fails");
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(
        errs[0].message.contains("into on R needs a target type"),
        "{errs:?}"
    );
    assert!(
        warns.is_empty(),
        "no warning beside the pick error: {warns:?}"
    );
}

/// A user name that merely starts like a compiler temp (`__options`) is the user's name: the
/// bound-method help spells it back.
#[test]
fn a_user_name_with_the_temp_prefix_is_named_in_help() {
    let src = "struct Bx[T]:\n    v: T\n    fn get(self) -> T:\n        return self.v\n__options := Bx(1)\nf := __options.get\n";
    let errs = check_desugared(src);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].message.contains("`__options.get(…)`"), "{errs:?}");
}

/// A double fault under `?.` (an unknown method AND an unknown name in its argument) reports the
/// errors the plain `.` call reports, no more: the receiver type reads `K` in both.
#[test]
fn a_double_fault_under_optchain_reports_what_the_plain_call_reports() {
    let k = "struct K:\n    n: int\n";
    let run = |call: &str| {
        let src =
            format!("{k}fn main():\n    r: K? = K(1)\n    x := {call}\n    print(x)\nmain()\n");
        let mut msgs: Vec<String> = check_desugared(&src)
            .into_iter()
            .map(|e| e.message)
            .collect();
        msgs.sort();
        msgs
    };
    let plain = run("r.nope2(nope())");
    let opt = run("r?.nope2(nope())");
    assert_eq!(plain.len(), 2, "{plain:?}");
    assert_eq!(
        opt,
        plain
            .iter()
            .map(|m| m.replace("K?", "K"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn carrier_operand_scratch_name_never_leaks_into_a_diagnostic() {
    // The operand-scratch binding `Checker::scratch_operand` declares (so the lowered clone infers
    // the operand ZERO extra times) is invisible to users — no diagnostic may name it. Covers all
    // three arms that create one: `?.` on a Result, `?.` on an Option, and `??`.
    for src in [
        // Result arm: the clone's `?`-then-`.` errors on the missing field.
        "fn f() -> int!str:\n    return ?1\nfn g() -> int!str:\n    return ?(f()?.zzz)\n",
        // Option arm: the clone's match-arm body errors on the missing method.
        "struct P:\n    x: int\nop: P? = ?P(1)\nr := op?.zzz()\n",
        // `??` arm: the clone's `None` arm errors on the mismatched rhs.
        "op: int? = ?1\nr := op ?? \"s\"\n",
        // …and a nested chain, where every link declares its own scratch.
        "struct P:\n    x: int\n    fn m(self) -> P?:\n        return ?self\nop: P? = ?P(1)\nr := op?.m()?.m()?.zzz\n",
    ] {
        let errs = check_desugared(src);
        assert!(!errs.is_empty(), "expected a diagnostic for: {src}");
        assert!(
            !errs.iter().any(|e| e
                .message
                .contains(crate::ast::CarrierTemp::Operand.prefix())),
            "operand-scratch name leaked into a diagnostic: {errs:?}"
        );
    }
}

#[test]
fn opt_chain_check_time_is_linear_in_chain_length() {
    // `?.` chains left-nest, so `a?.b?.c`'s operand IS the previous carrier. When the lowered clone
    // still contained the operand it was inferred twice per link — `T(n) = 2·T(n-1)`, measured at
    // 10s of `chezzi check` for 22 links and ~4× per 2 more, with the parser's fold bound sitting 15
    // orders of magnitude above the point it becomes a hang. No wall-clock assert (flaky); the guard
    // is that this finishes AT ALL — at 60 links the doubling shape is ~2^38 × the 22-link cost.
    let mut src = String::from(
        "struct A:\n    v: str\n    fn m(self) -> A!str:\n        return ?self\n\
         fn f() -> A!str:\n    return ?A(\"x\")\nfn g() -> str!str:\n    return ?(f()",
    );
    for _ in 0..60 {
        src.push_str("?.m()");
    }
    src.push_str("?.v)\n");
    ok_desugared(&src);
}

#[test]
fn opt_chain_returns_option_of_field() {
    ok_desugared("struct P:\n    x: int\nop := ?P(1)\nr: int? = op?.x\n");
}

#[test]
fn opt_chain_method_returns_option() {
    ok_desugared(
        "struct P:\n    x: int\n    fn get(self) -> int:\n        return self.x\nop := ?P(1)\nr: int? = op?.get()\n",
    );
}

#[test]
fn opt_chain_chains_nested() {
    // `a?.b?.c` — each layer operates on the Option result of the inner.
    ok_desugared(
        "struct Inner:\n    v: int\nstruct Outer:\n    inner: Inner\no := ?Outer(Inner(7))\nr: int? = o?.inner?.v\n",
    );
}

#[test]
fn opt_chain_double_option_not_flattened() {
    // A field that is itself `Option[int]` yields `Option[Option[int]]` — NOT flattened.
    ok_desugared("struct P:\n    maybe: int?\nop := ?P(?1)\nr: int?? = op?.maybe\n");
    // ...so binding the same expression to `Option[int]` must fail. (Real needle, not the empty
    // one this used to pass — an empty needle asserts only "some error", which is how W7-43's
    // three bogus Option-variant errors survived so long.)
    rejects_desugared(
        "struct P:\n    maybe: int?\nop := ?P(?1)\nbad: int? = op?.maybe\n",
        "cannot assign int?? to variable of type int?",
    );
}

// ===== W7-43 — `?.` on a Result is `?` then `.` =====
//
// The carrier survives desugar now; the CHECKER picks the lowering from the operand's type. Use
// the `_desugared` helpers throughout — that is the production pipeline.

/// `fn f() -> str!str` + `f()?.len()` inside a `Result`-returning fn, in both spellings.
const RESULT_CARRIER: &str =
    "fn f() -> str!str:\n    return ?\"hi\"\nfn g() -> int!str:\n    return ?(f()?.len())\n";
const RESULT_CARRIER_SPACED: &str =
    "fn f() -> str!str:\n    return ?\"hi\"\nfn g() -> int!str:\n    return ?(f()? .len())\n";

#[test]
fn opt_chain_on_result_is_try_then_dot() {
    // The gap's own repro: `?.` on a Result means `?` then `.`, exactly as Rust's `f()?.len()`
    // does — and exactly as the spaced `f()? .len()` spelling always did here.
    ok_desugared(RESULT_CARRIER);
    ok_desugared(RESULT_CARRIER_SPACED);
}

#[test]
fn opt_chain_on_result_emits_no_option_variant_errors() {
    // THE regression gate for W7-43. Before the fix this exact program emitted THREE errors, none
    // of which mentioned `?.`, because desugar rewrote the carrier into a `match` with hard-coded
    // Some/None arms before anything was typed. Assert their ABSENCE by name, not just that the
    // program is clean — a future re-lowering could produce a clean program by luck, but it could
    // not produce these three messages without having taken the Option lowering on a Result.
    let errs = check_desugared(RESULT_CARRIER);
    for needle in [
        "'Some' is not a variant",
        "'None' is not a variant",
        "non-exhaustive match on Result",
    ] {
        assert!(
            !errs.iter().any(|e| e.message.contains(needle)),
            "the Option lowering leaked onto a Result operand ({needle:?}): {errs:?}"
        );
    }
    assert!(errs.is_empty(), "expected a clean program, got: {errs:?}");
}

#[test]
fn opt_chain_on_non_carrier_one_error() {
    // ONE error naming `?.` itself — the count matters as much as the needle: the whole gap was
    // three errors where one belonged.
    let errs = check_desugared("x := 5\ny := x?.foo\n");
    assert_eq!(errs.len(), 1, "exactly one error, got: {errs:?}");
    assert!(
        errs[0]
            .message
            .contains("'?.' applies to a `T?` or `T!E` value, found int"),
        "got: {errs:?}"
    );
}

#[test]
fn opt_chain_diagnostic_equals_the_spaced_spellings_message_and_span() {
    // The load-bearing claim of the design: `f()?.len()` and `f()? .len()` are the SAME program.
    // A rejected program is where that is observable in the checker — both must produce the same
    // message at the same span. (`parse_postfix` reuses the primary's span for every postfix link,
    // so the two spellings' spans coincide by construction; this pins it.)
    let nil_fn = |carrier: &str| {
        format!(
            "fn f() -> str!str:\n    return ?\"hi\"\nfn g():\n    n := {carrier}\n    print(n)\n"
        )
    };
    let dot = check_desugared(&nil_fn("f()?.len()"));
    let spaced = check_desugared(&nil_fn("f()? .len()"));
    assert_eq!(dot.len(), 1, "got: {dot:?}");
    assert_eq!(spaced.len(), 1, "got: {spaced:?}");
    assert_eq!(dot[0].message, spaced[0].message);
    assert_eq!(dot[0].span, spaced[0].span, "spans must coincide");
}

#[test]
fn opt_chain_on_result_inherits_infer_trys_gates() {
    // The Result lowering builds a real `ExprKind::Try`, so all three of `infer_try`'s gates apply
    // verbatim with zero new gate code. Each is asserted in BOTH spellings — the point of the
    // design is that they are one program.
    for carrier in ["f()?.len()", "f()? .len()"] {
        // (a) a nil-returning fn body: the propagated Err would be silently swallowed.
        rejects_desugared(
            &format!(
                "fn f() -> str!str:\n    return ?\"hi\"\nfn g():\n    n := {carrier}\n    print(n)\n"
            ),
            "'?' used in a function that returns None, not a `T?` or `T!E` value",
        );
        // (b) an Option-returning fn: kinds may not be mixed.
        rejects_desugared(
            &format!(
                "fn f() -> str!str:\n    return ?\"hi\"\nfn g() -> int?:\n    return ?{carrier}\n"
            ),
            "'?' propagates an error, but the enclosing function returns a `T?` value, not a `T!E` value",
        );
        // (c) inside `recover:` the `?` short-circuits to the boundary, whose error slot is the
        // `Error` existential — so the propagated error must satisfy `Error`.
        rejects_desugared(
            &format!(
                "struct Bare:\n    x: int\nfn f() -> str!Bare:\n    return ?\"hi\"\nr := recover: {carrier}\nprint(r)\n"
            ),
            "'?' inside a recover block propagates error Bare, which must satisfy Error",
        );
    }
}

#[test]
fn opt_chain_operand_error_is_reported_exactly_once() {
    // `infer_opt_chain` speculatively infers the OPERAND and rolls the errors back before
    // re-inferring the lowered clone (`error()` does not dedupe). The rollback runs ONLY in the
    // arms that re-infer — truncating in the Unknown / non-carrier arms would swallow the
    // operand's only diagnostic, which is the zero-error half of this test.
    let errs = check_desugared("y := x?.f\n");
    assert_eq!(
        errs.len(),
        1,
        "an undefined operand must still report, exactly once: {errs:?}"
    );
    assert!(errs[0].message.contains("unknown name 'x'"), "{errs:?}");

    // …and the twice half: the operand types fine as a Result, so the clone IS re-inferred — the
    // operand's own error must survive the truncate exactly once, not twice.
    let errs = check_desugared(
        "fn f(a: int) -> str!str:\n    return ?\"hi\"\nfn g() -> int!str:\n    return ?(f(\"x\")?.len())\n",
    );
    assert_eq!(errs.len(), 1, "reported once, not twice: {errs:?}");
    assert!(
        errs[0]
            .message
            .contains("argument 1 of 'f': expected int, found str"),
        "{errs:?}"
    );

    // The same twice-half for the OPTION arm — a different lowering (`match`, not `?`-then-`.`),
    // so it needs its own case or the arm is untested by this suite.
    let errs = check_desugared(
        "struct P:\n    x: int\nfn mk(a: int) -> P?:\n    return ?P(a)\nr := mk(\"bad\")?.x\n",
    );
    assert_eq!(errs.len(), 1, "reported once, not twice: {errs:?}");
    assert!(
        errs[0]
            .message
            .contains("argument 1 of 'mk': expected int, found str"),
        "{errs:?}"
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
        "struct M:\n    n: int\n    fn compare(self, other: M) -> int:\n        return self.n - other.n\n    fn eq(self, other: M) -> bool:\n        return self.n == other.n\nxs := [M(2), M(1)]\nxs.sort_by_key(fn(m: M) -> M: m)\n",
    );
}

#[test]
fn sort_by_key_non_comparable_key_rejected() {
    // A key function returning a non-Comparable struct (no `compare`) is rejected. File-backed as
    // `sort_by_key[K: Comparable](...)`: the `K: Comparable` bound is recovered from the closure body
    // by the loop-back, then enforced — the uniform protocol-conformance diagnostic (was bespoke).
    rejects(
        "struct B:\n    n: int\nxs := [B(2), B(1)]\nxs.sort_by_key(fn(b: B) -> B: b)\n",
        "does not satisfy Comparable (missing method 'compare')",
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
