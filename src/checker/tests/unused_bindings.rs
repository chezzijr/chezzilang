//! Checker tests: box reads (TICKET-089, deleted in M3.3), unused `:=` (TICKET-090), qualified protocol
//! bounds (TICKET-173), argument binding (TICKET-182).
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== TICKET-089 (deleted in M3.3): a write to a box read is a real write =====

/// M3.3 (Q4): `s.get()`, `r.get()` and `a.load()` return the live value, so a mutating call or a
/// store on one lands in the box, and no warning fires (Go `vet` has no such check). One row per
/// shape the deleted TICKET-089 warning covered: each method family, a field and an index store, the
/// top level and a `spawn:` body. `remove` returns an `Option`, so only the carrier warning is left.
#[test]
fn a_write_to_a_box_read_is_not_warned() {
    for src in [
        "import Shared from std.concurrency\nfn main():\n    s := Shared[List[int]]([])\n    s.get().push(1)\nmain()\n",
        "import Shared from std.concurrency\nfn main():\n    s := Shared(bytearray.from_iter(b\"a\"))\n    s.get().extend(b\"b\")\nmain()\n",
        "import RwShared from std.concurrency\nfn main():\n    r := RwShared[List[int]]([])\n    r.get().push(1)\nmain()\n",
        "import Atomic from std.concurrency\nfn main():\n    a := Atomic[List[int]]([])\n    a.load().push(1)\nmain()\n",
        "import Shared from std.concurrency\nstruct P:\n    v: int\nfn main():\n    b := Shared(P(0))\n    b.get().v = 9\nmain()\n",
        "import Shared from std.concurrency\nfn main():\n    s := Shared([7])\n    s.get()[0] = 9\nmain()\n",
        "import Shared from std.concurrency\ns := Shared[List[int]]([])\ns.get().push(1)\n",
        "import Shared from std.concurrency\nfn main():\n    s := Shared[List[int]]([])\n    parallel:\n        spawn:\n            s.get().push(1)\nmain()\n",
        "import Shared from std.concurrency\nfn main():\n    s := Shared[Set[int]](Set[int]())\n    s.get().insert(1)\nmain()\n",
    ] {
        entry_no_warn(src);
    }
    entry_warns_once(
        "import Shared from std.concurrency\nfn main():\n    s := Shared[Map[str, int]]({\"a\": 1})\n    s.get().remove(\"a\")\nmain()\n",
        "discarded",
    );
}

/// A bound box read or a value-used read is silent.
#[test]
fn a_used_or_bound_read_temporary_is_not_warned() {
    entry_no_warn(
        "import Shared from std.concurrency\nfn main():\n    s := Shared[List[int]]([])\n    v := s.get()\n    v.push(1)\n    print(v.len())\nmain()\n",
    );
    entry_no_warn(
        "import Shared from std.concurrency\nfn main():\n    s := Shared[List[int]]([1, 2])\n    print(s.get().len())\nmain()\n",
    );
}

/// The working spellings, and the same method name on a plain local, never warn.
#[test]
fn a_correct_write_through_spelling_and_a_plain_local_are_not_warned() {
    entry_no_warn(
        "import Shared from std.concurrency\nfn main():\n    s := Shared[List[int]]([])\n    s.update(fn(xs): xs + [1])\n    print(s.get().len())\nmain()\n",
    );
    entry_no_warn(
        "import RwShared from std.concurrency\nfn main():\n    r := RwShared[List[int]]([])\n    r.write(fn(xs): xs + [1])\n    print(r.get().len())\nmain()\n",
    );
    entry_no_warn(
        "import Shared from std.concurrency\nfn main():\n    s := Shared(0)\n    s.set(5)\n    print(s.get())\nmain()\n",
    );
    entry_no_warn(
        "fn main():\n    xs: List[int] = []\n    xs.push(1)\n    print(xs.len())\nmain()\n",
    );
}

/// A user struct method, a non-mutating bare call and a `defer` on a box read: silent (M3.3: each
/// write lands in the box).
#[test]
fn a_struct_method_a_nonmutating_call_and_a_defer_on_a_read_temporary_are_not_warned() {
    entry_no_warn(
        "import Shared from std.concurrency\nstruct P:\n    v: int\n    fn bump(self):\n        self.v = self.v + 1\nfn main():\n    b := Shared(P(0))\n    b.get().bump()\nmain()\n",
    );
    entry_no_warn(
        "import Shared from std.concurrency\nfn main():\n    s := Shared[List[int]]([1])\n    s.get().len()\nmain()\n",
    );
    entry_no_warn(
        "import Shared from std.concurrency\nfn main():\n    s := Shared[List[int]]([])\n    defer s.get().push(1)\n    print(1)\nmain()\n",
    );
}

// ===== TICKET-090 — an unused `:=` binding should warn (Rust's shape: warning, not Go's error) =====

/// `unused := 42` inside a fn body is never read. TICKET-090's whole premise: this is currently
/// silent (no warning, exit code unchanged), which lets a `:=` typo in place of `=` (the classic
/// shadowing accumulator bug) print a silently wrong answer at rc=0.
#[test]
fn unused_local_binding_warns() {
    warns("fn f():\n    unused := 42\nf()\n", "unused");
}

/// The `:=` typo (`total := total + x` instead of `total = total + x`) makes the ticket's block-scoped
/// `:=` shadow the outer `total` every iteration — the accumulator loses every update but still prints
/// `0` at rc=0. `docs/reference/syntax/`'s measured warn table names this the flagship shape.
#[test]
fn unused_local_accumulator_typo_warns_once_at_the_inner_binding() {
    let (errs, warns) = warn_src(
        "fn total(xs: List[int]) -> int:\n    total := 0\n    for x in xs:\n        total := total + x\n    return total\nprint(total([1,2,3]))\n",
    );
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
    assert_eq!(
        warns.len(),
        1,
        "expected exactly one warning, got: {warns:?}"
    );
    assert!(
        warns[0].message.contains("unused variable 'total'"),
        "got: {:?}",
        warns[0]
    );
    assert_eq!(warns[0].span.line, 4, "got: {:?}", warns[0]);
    assert_eq!(warns[0].span.col, 9, "got: {:?}", warns[0]);
}

/// The measured fire half of `docs/reference/syntax/`'s warn table — one program per shape.
#[test]
fn unused_local_warn_table_fires() {
    warns(
        "fn f():\n    for x in 0..2:\n        pass\nf()\n",
        "unused variable 'x'",
    );
    warns(
        "fn f():\n    x := 1\n    x = 2\nf()\n",
        "unused variable 'x'",
    );
    warns(
        "fn p() -> (int, int):\n    return (1, 2)\nfn f():\n    a, b := p()\n    print(a)\nf()\n",
        "unused variable 'b'",
    );
    warns(
        "fn f():\n    z := 1\n    z := 2\n    print(z)\nf()\n",
        "unused variable 'z'",
    );
    warns("test fn t():\n    y := 3\n", "unused variable 'y'");
    warns("for i in 0..1:\n    pass\n", "unused variable 'i'");
    warns(
        "struct P:\n    a: int\n    fn m(self) -> int:\n        k := 1\n        return self.a\nprint(P(1).m())\n",
        "unused variable 'k'",
    );
    warns(
        "fn f():\n    r := recover:\n        1\n    print(\"x\")\nf()\n",
        "unused variable 'r'",
    );
}

/// The measured silent half of `docs/reference/syntax/`'s warn table — every deliberate under-warn.
#[test]
fn unused_local_warn_table_stays_silent() {
    no_warn("fn f():\n    _u := 42\n    _ := 1\nf()\n");
    no_warn("fn f(a: int):\n    pass\nf(1)\n");
    no_warn("fn f():\n    for _ in 0..2:\n        pass\nf()\n");
    no_warn("fn f():\n    x := 1\n    g := fn() -> int: x\n    print(g())\nf()\n");
    no_warn("fn f():\n    x := 1\n    defer:\n        print(x)\nf()\n");
    no_warn("fn f():\n    x := 1\n    parallel:\n        spawn:\n            print(x)\nf()\n");
    no_warn("fn f():\n    x := 1\n    fn g() -> int:\n        return x\n    print(g())\nf()\n");
    no_warn("fn f():\n    x := 1\n    print(\"{x}\")\nf()\n");
    no_warn("g := 1\nfn f():\n    pass\nf()\n");
    no_warn(
        "fn f():\n    x := 1\n    if true:\n        x := 2\n        print(x)\n    print(x)\nf()\n",
    );
    no_warn("fn f():\n    c := 0\n    c += 1\nf()\n");
    no_warn(
        "fn f():\n    o: int? = 1\n    match o:\n        ?x:\n            print(\"s\")\n        None:\n            pass\nf()\n",
    );
    no_warn("struct P:\n    a: int\nfn f():\n    p := P(1)\n    p.a = 2\nf()\n");
    no_warn("fn f():\n    w := 5\n    s := \"x\"\n    print(\"{s:<{w}}\")\nf()\n");
    no_warn("fn f():\n    xs := [1]\n    xs.push(2)\nf()\n");
}

// === TICKET-173: module-qualified protocol bounds ===

const QPB_LIB: &str = "protocol QpbFactory:\n    fn default() -> Self\nprotocol QpbTagged:\n    fn tag(self) -> str\nstruct QpbPoint:\n    x: int\n    fn default() -> QpbPoint:\n        return QpbPoint(0)\n";

#[test]
fn a_qualified_bound_checks_through_a_whole_module_import() {
    // `[T: qp.QpbFactory]` names the same protocol as `import QpbFactory from lib`.
    let errs = check_files(&[
        ("lib.chz", QPB_LIB),
        (
            "main.chz",
            "import lib as qp\nfn make[T: qp.QpbFactory](s: T) -> T:\n    return T.default()\nprint(make(qp.QpbPoint(7)).x)\n",
        ),
    ]);
    let msgs: Vec<&str> = errs.iter().map(|e| e.message.as_str()).collect();
    assert!(errs.is_empty(), "expected no errors, got {msgs:?}");
}

/// Check `src` as `main.chz` beside `QPB_LIB` and assert some error contains every needle — and,
/// when `line` is given, sits on that line (TICKET-173's qualified-bound diagnostics).
fn assert_qpb_error(src: &str, needles: &[&str], line: Option<u32>) {
    let errs = check_files(&[("lib.chz", QPB_LIB), ("main.chz", src)]);
    let msgs: Vec<String> = errs
        .iter()
        .map(|e| format!("{} @{}", e.message, e.span.line))
        .collect();
    assert!(
        errs.iter()
            .any(|e| needles.iter().all(|n| e.message.contains(n))
                && line.is_none_or(|l| e.span.line == l)),
        "expected an error containing {needles:?} on line {line:?}, got {msgs:?}"
    );
}

#[test]
fn a_qualified_bound_through_an_unimported_module_is_refused() {
    assert_qpb_error(
        "import lib as qp\nfn make[T: zz.QpbFactory](s: T) -> T:\n    return s\n",
        &["unknown module 'zz' (import it to use `zz.QpbFactory`)"],
        Some(2),
    );
}

#[test]
fn a_qualified_bound_in_a_where_clause_through_an_unimported_module_is_refused() {
    assert_qpb_error(
        "import lib as qp\nfn make[T](s: T) -> T where T: zz.QpbFactory:\n    return s\n",
        &["unknown module 'zz' (import it to use `zz.QpbFactory`)"],
        Some(2),
    );
}

#[test]
fn a_qualified_bound_naming_a_missing_member_is_refused() {
    assert_qpb_error(
        "import lib as qp\nfn make[T: qp.Nope](s: T) -> T:\n    return s\n",
        &["unknown protocol 'qp.Nope' in bound on 'T'"],
        Some(2),
    );
}

#[test]
fn a_qualified_bound_naming_a_struct_is_refused() {
    assert_qpb_error(
        "import lib as qp\nfn make[T: qp.QpbPoint](s: T) -> T:\n    return s\n",
        &["unknown protocol 'qp.QpbPoint' in bound on 'T'"],
        Some(2),
    );
}

#[test]
fn a_qualified_bound_is_enforced_at_the_call_site() {
    assert_qpb_error(
        "import lib as qp\nfn make[T: qp.QpbFactory](s: T) -> T:\n    return T.default()\nprint(make(5))\n",
        &[
            "type int does not satisfy",
            "QpbFactory (missing method 'default')",
        ],
        None,
    );
}

#[test]
fn a_qualified_bound_on_a_bare_protocol_rejects_type_arguments() {
    assert_qpb_error(
        "import lib as qp\nfn f[T: qp.QpbTagged[int]](s: T) -> int:\n    return 1\n",
        &["protocol 'qp.QpbTagged' takes no type arguments"],
        Some(2),
    );
}

#[test]
fn a_qualified_bound_with_a_three_level_path_is_rejected() {
    // A 3-segment bound reads module `qp.x` (TICKET-175), which is not imported.
    assert_qpb_error(
        "import lib as qp\nfn make[T: qp.x.QpbFactory](s: T) -> T:\n    return s\n",
        &["unknown module 'qp.x' (import it to use `qp.x.QpbFactory`)"],
        Some(2),
    );
}

// TICKET-175: Python full-path spelling of an imported module, and `import X from std`.

#[test]
fn std_full_path_checks_clean() {
    files_ok(&[("main.chz", "import std.math\nprint(std.math.sqrt(9.0))\n")]);
}

#[test]
fn import_math_from_std_checks_clean() {
    files_ok(&[("main.chz", "import math from std\nprint(math.sqrt(9.0))\n")]);
}

#[test]
fn pkg_full_path_annotation_checks_clean() {
    files_ok(&[
        ("pkg/deep.chz", "struct Point:\n    x: int\n"),
        (
            "main.chz",
            "import pkg.deep\nx: pkg.deep.Point = deep.Point(1)\nprint(x.x)\n",
        ),
    ]);
}

const FP_DEEP: &str = "fn double(n: int) -> int:\n    return n * 2\nstruct Point:\n    x: int\nprotocol Factory:\n    fn make() -> Self\n";
const FP_OTHER: &str = "fn double(n: int) -> int:\n    return n * 2\n";

#[test]
fn full_path_after_as_import_names_the_bound_name() {
    files_reject(
        &[
            ("pkg/deep.chz", FP_DEEP),
            ("pkg/other.chz", FP_OTHER),
            (
                "main.chz",
                "import pkg.deep as d\nprint(pkg.deep.double(1))\n",
            ),
        ],
        "`pkg.deep` is imported as `d`",
    );
}

#[test]
fn full_path_of_an_unimported_module_is_rejected() {
    files_reject(
        &[
            ("pkg/deep.chz", FP_DEEP),
            ("pkg/other.chz", FP_OTHER),
            ("main.chz", "import pkg.deep\nprint(pkg.other.double(1))\n"),
        ],
        "needs `import pkg.<module>`",
    );
}

#[test]
fn full_path_head_shadowed_by_a_local_is_rejected() {
    files_reject(
        &[
            ("pkg/deep.chz", FP_DEEP),
            (
                "main.chz",
                "import pkg.deep\nfn f() -> int:\n    pkg := 3\n    return pkg.deep.double(1)\n",
            ),
        ],
        "type int has no field 'deep'",
    );
}

#[test]
fn full_path_head_shadowed_by_a_param_is_rejected() {
    files_reject(
        &[
            ("pkg/deep.chz", FP_DEEP),
            (
                "main.chz",
                "import pkg.deep\nfn g(pkg: int) -> int:\n    return pkg.deep.double(1)\n",
            ),
        ],
        "type int has no field 'deep'",
    );
}

#[test]
fn full_path_unknown_member_carets_the_member() {
    let errs = check_files(&[
        ("pkg/deep.chz", FP_DEEP),
        ("main.chz", "import pkg.deep\nprint(pkg.deep.nope(1))\n"),
    ]);
    assert!(
        errs.iter().any(
            |e| e.message.contains("module 'pkg.deep' has no member 'nope'")
                && e.span.line == 2
                && e.span.col == 16
        ),
        "got: {errs:?}"
    );
}

#[test]
fn full_path_unknown_module_in_annotation_is_rejected() {
    let errs = check_files(&[
        ("pkg/deep.chz", FP_DEEP),
        (
            "main.chz",
            "import pkg.deep\nx: pkg.nope.Point = deep.Point(1)\n",
        ),
    ]);
    assert!(
        errs.iter().any(|e| e
            .message
            .contains("unknown module 'pkg.nope' (import it to use `pkg.nope.Point`)")
            && e.span.line == 2),
        "got: {errs:?}"
    );
}

#[test]
fn full_path_bound_resolves_the_protocol() {
    files_ok(&[
        ("pkg/deep.chz", FP_DEEP),
        (
            "main.chz",
            "import pkg.deep\nfn build[T: pkg.deep.Factory]() -> T:\n    return T.make()\n",
        ),
    ]);
}

const FP_A_MATH: &str = "fn f() -> int:\n    return 1\n";
const FP_B_MATH: &str = "fn f() -> int:\n    return 2\n";

#[test]
fn same_last_segment_imports_keep_both_full_paths() {
    files_ok(&[
        ("a/math.chz", FP_A_MATH),
        ("b/math.chz", FP_B_MATH),
        (
            "main.chz",
            "import a.math\nimport b.math\nprint(a.math.f() + b.math.f())\n",
        ),
    ]);
}

#[test]
fn same_last_segment_bare_use_is_ambiguous() {
    let errs = check_files(&[
        ("a/math.chz", FP_A_MATH),
        ("b/math.chz", FP_B_MATH),
        (
            "main.chz",
            "import a.math\nimport b.math\nprint(a.math.f() + b.math.f())\nprint(math.f())\n",
        ),
    ]);
    assert!(
        errs.iter().any(|e| [
            "'math' is ambiguous",
            "a.math",
            "b.math",
            "import a.math as am"
        ]
        .iter()
        .all(|n| e.message.contains(n))),
        "got: {errs:?}"
    );
}

#[test]
fn same_module_under_two_spellings_is_one_bind() {
    files_ok(&[(
        "main.chz",
        "import std.math\nimport math from std\nprint(math.sqrt(4.0) + std.math.sqrt(4.0))\n",
    )]);
}

#[test]
fn duplicate_import_of_one_module_is_rejected() {
    files_reject(
        &[
            ("pkg/deep.chz", FP_DEEP),
            (
                "main.chz",
                "import pkg.deep\nimport pkg.deep\nprint(deep.double(1))\n",
            ),
        ],
        "'deep' is already imported",
    );
}

#[test]
fn duplicate_import_from_std_is_rejected() {
    files_reject(
        &[(
            "main.chz",
            "import math from std\nimport math from std\nprint(math.sqrt(4.0))\n",
        )],
        "'math' is already imported",
    );
}

#[test]
fn full_path_head_shadowed_by_a_type_keeps_the_import_hints_quiet() {
    // `struct pkg` hides the head, so the fold leaves `pkg.deep` alone. `pkg.deep` IS imported, so
    // neither the not-imported nor the imported-as hint may fire.
    let errs = check_files(&[
        ("pkg/deep.chz", FP_DEEP),
        (
            "main.chz",
            "import pkg.deep\nimport pkg.deep as d\nstruct pkg:\n    x: int\nprint(pkg.deep.double(1))\n",
        ),
    ]);
    assert!(!errs.is_empty(), "the shadowed head must still be an error");
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("is not imported") || e.message.contains("is imported as")),
        "got: {errs:?}"
    );
}

#[test]
fn full_path_head_shadowed_by_a_type_param_is_rejected() {
    let errs = check_files(&[
        ("pkg/deep.chz", FP_DEEP),
        (
            "main.chz",
            "import pkg.deep\nfn g[pkg]() -> int:\n    return pkg.deep.double(1)\n",
        ),
    ]);
    assert!(
        !errs.is_empty(),
        "a type param `pkg` must hide the full path"
    );
}

#[test]
fn full_path_head_shadowed_by_a_later_global_is_rejected() {
    // The global is declared AFTER the fn that reads `pkg.deep`: module-level shadowing must not
    // depend on source position (hoisted-decl rule).
    let errs = check_files(&[
        ("pkg/deep.chz", FP_DEEP),
        (
            "main.chz",
            "import pkg.deep\nfn f() -> int:\n    return pkg.deep.double(1)\npkg := 3\n",
        ),
    ]);
    assert!(errs.iter().any(|e| e.span.line == 3), "got: {errs:?}");
}

#[test]
fn full_path_longest_imported_prefix_wins() {
    // `a.b` has a member `c`, and `a.b.c` is a module too: `a.b.c.f()` is module `a.b.c`.
    files_ok(&[
        ("a/b.chz", "c := 5\n"),
        ("a/b/c.chz", "fn f() -> int:\n    return 1\n"),
        (
            "main.chz",
            "import a.b\nimport a.b.c\nx: int = a.b.c.f()\nprint(x + a.b.c.f())\n",
        ),
    ]);
}

#[test]
fn full_path_head_shadowed_by_a_wait_recv_bind_reads_the_field() {
    // A `wait` recv arm binds `pkg` (a `Conf!RecvError`) in the arm scope, and the `?pkg` match arm
    // rebinds it to the received struct: `pkg.deep.x` reads its field, not module `pkg.deep`
    // (owner rule 5).
    files_ok(&[
        ("pkg/deep.chz", FP_DEEP),
        (
            "main.chz",
            "import pkg.deep\nstruct Inner:\n    x: int\nstruct Conf:\n    deep: Inner\nch: Channel[Conf] = Channel(1)\nch.send(Conf(Inner(9)))\nwait:\n    pkg := ch.recv():\n        match pkg:\n            ?pkg: print(pkg.deep.x)\n            !e: panic(e.message())\n",
        ),
    ]);
}

// TICKET-183: module scope is order-free for function bodies.
#[test]
fn fn_body_reads_global_declared_below() {
    ok("fn f() -> int:\n    return x\nx := 5\nprint(f())\n");
}

// TICKET-183: an inferred return that reads a global must carry its type.
#[test]
fn inferred_return_of_global_is_typed() {
    rejects(
        "x := \"s\"\nfn f() -> str:\n    return x\ny: int = f()\nprint(y)\n",
        "str",
    );
}

// TICKET-183: `x := f()` where `f` returns `x` has no type to start from -- a named cycle.
#[test]
fn module_global_initialization_cycle_is_named() {
    rejects("x := f()\nfn f(): x\nprint(x)\n", "initialization cycle");
}

// The cycle is reported ONCE, as the cause: `f`'s derived "cannot infer return type" error (its
// consequence) is dropped, in both declaration orders.
#[test]
fn module_global_initialization_cycle_is_the_only_error() {
    for src in [
        "x := f()\nfn f(): x\nprint(x)\n",
        "fn f(): x\nx := f()\nprint(x)\n",
    ] {
        let errs = check_src(src);
        assert_eq!(errs.len(), 1, "{src:?}: {errs:?}");
        assert!(errs[0].message.contains("initialization cycle"), "{errs:?}");
    }
}

// TICKET-183: an annotation breaks the typing cycle; the read faults at run time instead.
#[test]
fn annotated_initialization_cycle_checks_clean() {
    ok("x: int = f()\nfn f() -> int:\n    return x\n");
}

// TICKET-183: an empty-collection global's element type is pinned by walk-order code a body
// above it cannot see, so the body read is declined with "annotate it".
#[test]
fn fn_body_above_unrefined_empty_global_is_declined() {
    rejects(
        "fn f() -> List[str]:\n    return xs\nxs := []\nxs.push(1)\n",
        "is declared below",
    );
}

// TICKET-183: top-level statements stay lexical, and the hidden global is no "did you mean".
#[test]
fn top_level_read_above_global_stays_unknown() {
    let errs = check_src("print(x)\nx := 5\n");
    let e = errs
        .iter()
        .find(|e| e.message.contains("unknown name 'x'"))
        .unwrap_or_else(|| panic!("expected unknown name 'x', got: {errs:?}"));
    assert_eq!(e.help, None, "expected no suggestion, got: {e:?}");
}

// TICKET-183: a top-level write above the global's let stays an undeclared write.
#[test]
fn top_level_write_above_global_stays_undeclared() {
    rejects(
        "X = 7\nX := 3\n",
        "cannot assign to undeclared variable 'X'",
    );
}

// TICKET-183: an inferred return that reads a typed global carries the annotation's type.
#[test]
fn typed_global_return_is_typed() {
    rejects(
        "x: int = 5\nfn f() -> int:\n    return x\ny: str = f()\n",
        "int",
    );
}

// TICKET-183: closure and method bodies see a global declared below them.
#[test]
fn closure_and_method_bodies_read_global_declared_below() {
    ok(
        "h := fn() -> int: x\nstruct S:\n    v: int\n    fn get(self) -> int:\n        return x\nx := 5\nprint(h())\nprint(S(1).get())\n",
    );
}

// TICKET-183: a below-position import stays rejected in a body (Go's rule).
#[test]
fn fn_body_above_import_stays_rejected() {
    entry_rejects(
        "fn f() -> float:\n    return pi\nimport pi from std.math\n",
        "before its `import`",
    );
}

// TICKET-183: a body's write to a global declared below is checked against its type.
#[test]
fn fn_body_write_above_global_is_typed() {
    rejects("fn w():\n    X = \"s\"\nX := 3\n", "cannot assign str");
}

// TICKET-183: a body's write to a `const` global declared below is rejected.
#[test]
fn fn_body_write_above_const_global_is_rejected() {
    rejects(
        "fn w():\n    X = 7\nX: const int = 3\n",
        "cannot reassign const binding 'X'",
    );
}

// TICKET-183: a keyword call through a closure global declared below keeps keyword certainty.
#[test]
fn fn_body_keyword_call_through_closure_global_below_checks() {
    ok(
        "fn g() -> int:\n    return h(b=1, a=2)\nh := fn(a: int, b: int) -> int: a - b\nprint(g())\n",
    );
}

/// TICKET-180 — the recorded [`Resolution`] of the name head at `line`:`col` in `src`. A call node
/// shares its callee's span, so the head is the one node at that position with an entry.
fn head_resolution(src: &str, line: u32, col: u32) -> Resolution {
    let mut m = parser::parse(lexer::tokenize(src).expect("lex")).expect("parse");
    crate::desugar::run_standalone(&mut m).expect("desugar");
    let table = resolve_call_tables_standalone(&m.stmts).6;
    let hits: Vec<Resolution> = crate::ast::node_ids(&m)
        .into_iter()
        .filter(|(_, sp)| sp.line == line && sp.col == col)
        .filter_map(|(id, _)| table.get(&(0, id)).cloned())
        .collect();
    assert_eq!(hits.len(), 1, "entries at {line}:{col}: {hits:?}");
    hits[0].clone()
}

/// TICKET-180 K1 — a local closure named like a builtin is the local, not the builtin.
#[test]
fn resolution_records_local_for_a_closure_named_like_a_builtin() {
    let r = head_resolution(
        "fn g() -> int:\n    ord := fn(s: str) -> int: 1000\n    return ord(\"a\")\nprint(g())\n",
        3,
        12,
    );
    assert_eq!(r, Resolution::Local);
}

/// TICKET-180 K2 — a recursive call inside a fn, beside `type Q = P`, is the fn.
#[test]
fn resolution_records_fn_for_a_recursive_call_inside_a_fn() {
    let r = head_resolution(
        "struct P:\n    x: int\ntype Q = P\nfn walk(s: str) -> P:\n    if s == \"\":\n        return P(0)\n    return walk(s[1..])\nprint(walk(\"abc\"))\n",
        7,
        12,
    );
    assert!(
        matches!(&r, Resolution::Fn { name, .. } if name == "walk"),
        "{r:?}"
    );
}

/// TICKET-180 — the bare `P(...)` of a struct `P` is its constructor, inside a fn too.
#[test]
fn resolution_records_the_struct_ctor_inside_a_fn() {
    let r = head_resolution(
        "struct P:\n    x: int\nfn mk(n: int) -> P:\n    return P(n)\nprint(mk(4))\n",
        4,
        12,
    );
    assert!(matches!(&r, Resolution::StructCtor(_)), "{r:?}");
}

/// TICKET-180 step 7: a let beside a type or named like a builtin is seeded like any other module
/// global (DEC-183), so a fn body reads the global whether the let is above or below the fn. (A
/// let named like a type is a second binding: `a_type_name_and_a_value_name_never_share_a_module_name`.)
#[test]
fn a_fn_body_resolves_a_global_in_both_orders() {
    const P_BIND: &str = "q := fn(n: int) -> str: \"v{n}\"\n";
    const ORD_BIND: &str = "ord := fn(s: str) -> int: 1000\n";
    const G_ORD: &str = "fn g() -> int:\n    h := ord\n    return h(\"a\")\n";
    // (1) an inferred body above the let returns the global's `str`, not the struct.
    rejects(
        &format!("struct P:\n    x: int\nfn g(): q(4)\n{P_BIND}y: int = g()\n"),
        "cannot assign str to variable of type int",
    );
    // (2) an annotated body above the let calls the global.
    ok(&format!(
        "struct P:\n    x: int\nfn g() -> str:\n    return q(4)\n{P_BIND}print(g())\n"
    ));
    // (3) a builtin-named global read in a body above its let.
    rejects(
        &format!("{G_ORD}{ORD_BIND}y: str = g()\n"),
        "cannot assign int to variable of type str",
    );
    // (4) the same with the let above the fn.
    rejects(
        &format!("{ORD_BIND}{G_ORD}y: str = g()\n"),
        "cannot assign int to variable of type str",
    );
    // (5) a top-level `fn f` and a later `f := ..` are two bindings of one name.
    rejects(
        "fn f() -> int:\n    return 1\nfn g() -> int:\n    return f()\nf := fn() -> int: 2\nprint(g())\n",
        "function 'f' is already defined",
    );
}

/// A whole-module import bind and a same-module top-level `fn` or type may not share a name, in
/// either source order (owner decisions 2026-09-29; Go: `f redeclared in this block`). The error
/// lands on the later of the two declarations. A from-imported type binds its own name, not the
/// module's (`a_type_import_beside_a_fn_is_bound_twice` rejects `fn P` beside it).
#[test]
fn a_fn_or_type_named_like_a_module_import_is_rejected() {
    const LIB: &str = "fn h(n: int) -> str:\n    return \"L{n}\"\nstruct P:\n    x: int\n";
    // The error lands on the later of the two and names the prior one.
    let reject_at = |main: &str, name: &str, line: u32, prior_import: bool| {
        let errs = check_files(&[("main.chz", main), ("lib.chz", LIB)]);
        let want = if prior_import {
            format!("'{name}' is already imported")
        } else {
            format!("'{name}' is already defined")
        };
        assert!(
            errs.iter()
                .any(|e| e.message.contains(&want) && e.span.line == line),
            "expected {want:?} at line {line} for {main:?}, got: {errs:?}"
        );
    };
    let decls = [
        ("fn f(a: int) -> int:\n    return a\n", "f"),
        ("struct f:\n    x: int\n", "f"),
        ("enum f:\n    A\n", "f"),
        ("type f = int\n", "f"),
        ("protocol f:\n    fn m(self) -> int\n", "f"),
    ];
    for (decl, name) in decls {
        let lines = decl.lines().count() as u32;
        // Import first: the declaration (line 2) is the later one.
        reject_at(&format!("import lib as f\n{decl}print(1)\n"), name, 2, true);
        // Declaration first: the import is the later one.
        reject_at(
            &format!("{decl}import lib as f\nprint(1)\n"),
            name,
            lines + 1,
            false,
        );
    }
    // The un-aliased spelling binds the last path segment.
    reject_at(
        "import lib\nfn lib(a: int) -> int:\n    return a\nprint(1)\n",
        "lib",
        2,
        true,
    );
    reject_at(
        "struct lib:\n    x: int\nimport lib\nprint(1)\n",
        "lib",
        3,
        false,
    );
    // A from-imported type is not a module bind.
    files_ok(&[
        (
            "main.chz",
            "import P from lib\nfn p(a: int) -> int:\n    return a\nprint(1)\n",
        ),
        ("lib.chz", LIB),
    ]);
}

/// TICKET-180 step 11 — the checker records what every pattern head names: a variant (keyed by the
/// scrutinee's enum), a struct destructure, or a binding. A nested bare `None` is the variant, a
/// nested bare `v` is a binding, and a bare top-level name over an `int` is a binding.
#[test]
fn resolution_records_every_pattern_head() {
    let src = "enum C:\n    Red\n    Val(int)\nstruct P:\n    x: int\n    y: int\n\
               fn f(c: C) -> int:\n    return match c:\n        C.Red: 0\n        C.Val(n): n\n\
               fn g(o: int??) -> int:\n    return match o:\n        ?None: 1\n        ?(?v): v\n        None: 0\n\
               fn h(p: P) -> int:\n    return match p:\n        P(a, b): a + b\n\
               fn k(n: int) -> int:\n    return match n:\n        1: 1\n        other: other\n";
    let mut m = parser::parse(lexer::tokenize(src).expect("lex")).expect("parse");
    crate::desugar::run_standalone(&mut m).expect("desugar");
    let table = resolve_call_tables_standalone(&m.stmts).6;
    let mut got: Vec<String> = crate::ast::node_ids(&m)
        .into_iter()
        .filter_map(|(id, _)| table.get(&(0, id)))
        .filter(|r| {
            matches!(
                r,
                Resolution::Variant { .. } | Resolution::PatStruct(_) | Resolution::PatBinding
            )
        })
        .map(|r| format!("{r:?}"))
        .collect();
    got.sort();
    let v = |e: &str, n: &str| {
        format!(
            "{:?}",
            Resolution::Variant {
                enum_key: e.to_string(),
                variant: n.to_string(),
            }
        )
    };
    // A user type's key is its runtime identity key (`<main>::C` in a standalone module).
    let mut want = vec![v("<main>::C", "Red"), v("<main>::C", "Val")];
    want.extend([
        v("Option", "Some"),
        v("Option", "Some"),
        v("Option", "Some"),
    ]);
    want.extend([v("Option", "None"), v("Option", "None")]);
    want.push(format!(
        "{:?}",
        Resolution::PatStruct("<main>::P".to_string())
    ));
    want.extend(std::iter::repeat_n(
        format!("{:?}", Resolution::PatBinding),
        5,
    ));
    want.sort();
    assert_eq!(got, want);
}

/// TICKET-180 step 12 — the checker builds the `json.decode[T]` descriptor from the type it
/// resolved and records it; the compiler emits it. `lib.Q` is `lib`'s alias of `lib`'s `P`, and
/// `main` declares its own `P` too, so the descriptor must carry `lib`'s key and fields.
#[test]
fn decode_target_is_recorded_for_an_imported_alias() {
    use crate::base::json_desc::{FieldDesc, TypeDescriptor};
    let t = TmpDir::new();
    t.write("lib.chz", "struct P:\n    x: int\ntype Q = P\n");
    let main = t.write(
        "main.chz",
        "import lib\nimport std.json\nstruct P:\n    y: str\nr := json.decode[lib.Q](\"{{}}\")\nprint(r)\nprint(P(\"a\"))\n",
    );
    let graph = crate::resolver::build_graph(&main).expect("resolve");
    assert!(check_graph(&graph).is_ok(), "{:?}", check_graph(&graph));
    let table = resolve_call_tables(&graph).6;
    let decodes: Vec<&TypeDescriptor<ArgFill>> = table
        .values()
        .filter_map(|r| match r {
            Resolution::Decode(d) => Some(d),
            _ => None,
        })
        .collect();
    assert_eq!(decodes.len(), 1, "{decodes:?}");
    let TypeDescriptor::Struct {
        key,
        display,
        fields,
    } = decodes[0]
    else {
        panic!("{decodes:?}");
    };
    assert_eq!(display, "P");
    assert_eq!(
        fields,
        &vec![FieldDesc {
            name: "x".to_string(),
            desc: TypeDescriptor::Int,
            default: None,
        }]
    );
    let main_p = table.values().find_map(|r| match r {
        Resolution::StructCtor(k) => Some(k.clone()),
        _ => None,
    });
    assert_ne!(
        Some(key.clone()),
        main_p,
        "the descriptor is main's P, not lib's"
    );
}

/// TICKET-198 — `json.decode[P]` fills a missing defaulted field with the SAME fill `P()` uses:
/// the descriptor's per-field defaults equal the ctor's `CallPlanTable` fills, one inline literal
/// and one provider (a module-global read).
#[test]
fn decode_descriptor_fills_equal_the_ctor_plan_fills() {
    use crate::base::json_desc::TypeDescriptor;
    let t = TmpDir::new();
    let main = t.write(
        "main.chz",
        "import std.json\nK := 2\nstruct P:\n    a: int = 3\n    b: int = K\nprint(P())\nprint(json.decode[P](r'{}'))\n",
    );
    let graph = crate::resolver::build_graph(&main).expect("resolve");
    assert!(check_graph(&graph).is_ok(), "{:?}", check_graph(&graph));
    let tables = resolve_call_tables(&graph);
    let plans: Vec<&Vec<ArgFill>> = tables.0.values().filter(|p| p.len() == 2).collect();
    assert_eq!(plans.len(), 1, "{plans:?}");
    let decodes: Vec<&TypeDescriptor<ArgFill>> = tables
        .6
        .values()
        .filter_map(|r| match r {
            Resolution::Decode(d) => Some(d),
            _ => None,
        })
        .collect();
    assert_eq!(decodes.len(), 1, "{decodes:?}");
    let TypeDescriptor::Struct { fields, .. } = decodes[0] else {
        panic!("{decodes:?}");
    };
    let fills: Vec<Option<ArgFill>> = fields.iter().map(|f| f.default.clone()).collect();
    let want: Vec<Option<ArgFill>> = plans[0].iter().cloned().map(Some).collect();
    assert_eq!(fills, want);
    assert!(
        matches!(fills[0], Some(ArgFill::Inline { .. })),
        "{fills:?}"
    );
    assert!(matches!(fills[1], Some(ArgFill::Provider(_))), "{fills:?}");
}

/// TICKET-180 P2 — an unpinned alias of a generic struct takes the target's type arguments, and
/// they seed the target's parameters. A non-generic alias still takes none: it already fixes its (zero) arguments.
#[test]
fn a_generic_alias_takes_its_targets_type_arguments() {
    const BB: &str = "struct Box[T]:\n    v: T\ntype BB = Box\n";
    ok(&format!("{BB}print(BB[int](9))\n"));
    rejects(
        &format!("{BB}x := BB[int](\"no\")\n"),
        "argument to 'BB' has type str, expected int",
    );
    rejects(
        "struct P:\n    x: int\ntype Q = P\nprint(Q[int](1))\n",
        "already fixes its type arguments",
    );
}

// TICKET-182: desugar binds a call's defaults through its own resolver, which misses a nested fn
// shadowing a defaulted top-level fn and splices the top-level fn's default into the call.
#[test]
fn a_nested_fn_shadowing_a_defaulted_fn_binds_to_the_nested_fn() {
    ok_desugared(
        "fn f(a: int, b: int = 10) -> int:\n    return a + b\n\nfn g() -> str:\n    fn f(n: int) -> str:\n        return \"B{n}\"\n    return f(1)\n",
    );
}

// ===== TICKET-182: the checker binds every call's arguments =====

/// Every message of `Checker::bind_call`, once for a direct call and once for a call through a
/// `kw_certain` function value (DEC-066: one message set for every call shape).
#[test]
fn bind_call_reports_each_named_argument_error() {
    let f = "fn f(a: int, b: int = 2, c: int = 3) -> int:\n    return a\n";
    let v = "fn v(a: int, ...rest: int) -> int:\n    return a\n";
    let kw = "fn k(a: int, ...rest: int, t: int) -> int:\n    return a\n";
    for (decl, call, needle) in [
        (
            f,
            "f(nope=1)",
            "unknown named argument 'nope' (its parameters are: a, b, c)",
        ),
        (
            f,
            "f(1, a=2)",
            "argument 'a' specified both positionally and by name",
        ),
        (f, "f(b=1, b=2)", "duplicate named argument 'b'"),
        (f, "f(b=1)", "missing required argument 'a'"),
        (
            f,
            "f(1, 2, 3, 4, c=5)",
            "too many arguments: expected at most 3, got 4",
        ),
        (
            v,
            "v(1, rest=2)",
            "argument 'rest' is the variadic parameter",
        ),
        (kw, "k(1, 2)", "missing required keyword argument 't'"),
    ] {
        rejects(&format!("{decl}print({call})\n"), needle);
        if decl != f {
            continue;
        }
        let g = call.replacen(&call[..1], "g", 1);
        rejects(
            &format!(
                "{decl}fn main():\n    g := {}\n    print({g})\nmain()\n",
                &call[..1]
            ),
            needle,
        );
    }
    // The callee-filled hole: through a value, a default naming a type parameter is callee-filled.
    rejects(
        "fn f2[T](a: T, b: List[T] = List[T](), c: int = 3) -> int:\n    return c\nfn main():\n    g := f2[int]\n    print(g(1, c=9))\nmain()\n",
        "the default for 'b' is filled by the callee and can only be omitted from the END of a call",
    );
}

/// A native binds named arguments by its decl's parameter names, like a user fn (owner ruling
/// 2026-11-03, which lifts DEC-182's refusal); a builtin with no decl (`Set[int](..)`) still takes
/// none and says so by name.
#[test]
fn a_native_callee_binds_named_arguments_by_its_decl() {
    for src in [
        "s := Set.from_iter([1, 2])\ns.insert(v=3)\n",
        "import std.net\nfn f(sk: net.Socket) -> str:\n    r := sk.read(10, timeout_ms=5)\n    return \"{r}\"\n",
        "import std.request\nfn f() -> str:\n    r := request.get(\"http://x\", timeout_ms=5)\n    return \"{r}\"\n",
        "print(chr(n=66))\n",
        "s: Set[int] = Set.from_iter(xs=[1])\nprint(s.len())\n",
    ] {
        entry_ok(src);
    }
    entry_rejects(
        "s := Set.from_iter([1, 2])\ns.insert(x=3)\n",
        "unknown named argument 'x' (its parameters are: v)",
    );
    entry_rejects(
        "s := Set[int](xs=[1])\nprint(s.len())\n",
        "'Set' takes no named arguments",
    );
    // A user struct that declares a same-named method changes nothing for a builtin receiver.
    let src = "struct Counter:\n    n: int\n    fn insert(self, amount: int = 1) -> int:\n        return self.n + amount\ns := Set.from_iter([1, 2])\ns.insert(x=3)\n";
    let errs = check_entry(src);
    assert!(
        errs.iter().any(|e| e
            .message
            .contains("unknown named argument 'x' (its parameters are: v)")),
        "got: {errs:?}"
    );
}

/// A default whose provider fills its own slot again is refused at compile time, naming the slot
/// (moved from desugar: the checker's binder now records the provider edges).
#[test]
fn a_self_referencing_default_is_a_cycle_error() {
    rejects_desugared(
        "fn f(x: int = f()) -> int:\n    return x\nr := f()\n",
        "the default for 'x' of 'f' is cyclic",
    );
    // A field is not called a parameter: the label is noun-free.
    rejects_desugared(
        "struct S:\n    n: int = S().n\nr := S().n\n",
        "the default for 'n' of 'S' is cyclic",
    );
}

/// A call through a fn-typed FIELD binds no declaration: `apply_value_call` refuses a keyword call (TICKET-264).
#[test]
fn fn_field_named_call_keeps_the_generic_refusal() {
    let src =
        "struct S:\n    map: fn(int) -> int\ns := S(fn(x: int) -> int: x)\nprint(s.map(arg=1))\n";
    entry_rejects(
        src,
        "keyword arguments through a function value need a binding",
    );
    let errs = check_entry(src);
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("takes no named arguments")),
        "got: {errs:?}"
    );
}

// TICKET-184 (Family E, wave 17): divergence must come from the resolved callee, not a name.
#[test]
fn ticket184_user_fn_named_exit_does_not_diverge() {
    rejects(
        "fn exit(code: int) -> int:\n    return code\nfn f(x: int) -> int:\n    if x > 0:\n        return x\n    exit(1)\nprint(f(-1))\n",
        "fall off the end",
    );
}

// TICKET-184: a `break` inside a `wait:` arm leaves `while true:`, so the fn can fall off.
#[test]
fn ticket184_break_in_wait_arm_leaves_while_true() {
    rejects(
        "fn g(c: Channel[int]) -> int:\n    while true:\n        wait:\n            _ := c.recv():\n                break\nprint(g(Channel[int](1)))\n",
        "fall off the end",
    );
}

// TICKET-184: the control-flow summary grid. Statement kind x {break, continue, return, diverging
// call, non-diverging call named exit/panic} x nesting position x consumer (C1 missing-return, C2
// recover tail, C3 inline-body inference, C4 escape checks, C5 fn_writes `left`, C6 value-position
// typing). `want` is "ok" (no errors) or a needle one error must contain.
#[test]
fn ticket184_flow_grid() {
    let rows: &[(&str, &str, &str)] = &[
        (
            "B1",
            "fn g() -> int:\n    while true:\n        break\nprint(g())\n",
            "fall off the end",
        ),
        (
            "B2",
            "fn g(c: bool) -> int:\n    while true:\n        if c:\n            break\nprint(g(true))\n",
            "fall off the end",
        ),
        (
            "B3",
            "fn g(n: int) -> int:\n    while true:\n        match n:\n            0: break\n            _: pass\nprint(g(0))\n",
            "fall off the end",
        ),
        (
            "B4",
            "fn g(c: Channel[int]) -> int:\n    while true:\n        wait:\n            _ := c.recv():\n                break\nprint(g(Channel[int](1)))\n",
            "fall off the end",
        ),
        (
            "B5",
            "fn g() -> int:\n    while true:\n        parallel:\n            break\nprint(g())\n",
            "fall off the end",
        ),
        (
            "B6",
            "fn g(xs: List[int]) -> int:\n    while true:\n        for x in xs:\n            break\nprint(g([1]))\n",
            "ok",
        ),
        (
            "B7",
            "fn g() -> int:\n    while true:\n        while true:\n            break\nprint(g())\n",
            "ok",
        ),
        (
            "B8",
            "fn g() -> int:\n    while true:\n        continue\nprint(g())\n",
            "ok",
        ),
        (
            "R1",
            "fn g(c: bool) -> int:\n    if c:\n        return 1\n    else:\n        return 2\nprint(g(true))\n",
            "ok",
        ),
        (
            "R2",
            "fn g(n: int) -> int:\n    match n:\n        0: return 1\n        _: return 2\nprint(g(0))\n",
            "ok",
        ),
        (
            "R3",
            "fn g(c: Channel[int]) -> int:\n    wait:\n        v := c.recv():\n            n := v else: panic(\"c closed\")\n            return n\nprint(g(Channel[int](1)))\n",
            "ok",
        ),
        (
            "R4",
            "fn g(c: Channel[int]) -> int:\n    wait:\n        v := c.recv():\n            n := v else: panic(\"c closed\")\n            return n\n        else:\n            return 0\nprint(g(Channel[int](1)))\n",
            "ok",
        ),
        (
            "R5",
            "fn g(c: Channel[int]) -> int:\n    wait:\n        v := c.recv():\n            n := v else: panic(\"c closed\")\n            return n\n        else:\n            pass\nprint(g(Channel[int](1)))\n",
            "fall off the end",
        ),
        (
            "R6",
            "fn g() -> int:\n    parallel:\n        return 1\nprint(g())\n",
            "ok",
        ),
        (
            "R7",
            "fn g(c: bool) -> int:\n    if c:\n        return 1\nprint(g(true))\n",
            "fall off the end",
        ),
        ("D1", "fn g() -> int:\n    panic(\"x\")\nprint(g())\n", "ok"),
        (
            "D2",
            "import std.os\nfn g() -> int:\n    os.exit(1)\n    panic(\"unreachable\")\nprint(g())\n",
            "ok",
        ),
        (
            "D3",
            "import exit from std.os\nfn g() -> int:\n    exit(1)\n    panic(\"unreachable\")\nprint(g())\n",
            "ok",
        ),
        (
            "D4",
            "import exit as quit from std.os\nfn g() -> int:\n    quit(1)\n    panic(\"unreachable\")\nprint(g())\n",
            "ok",
        ),
        (
            "D5",
            "fn g(c: bool) -> int:\n    if c:\n        return 1\n    else:\n        panic(\"x\")\nprint(g(true))\n",
            "ok",
        ),
        (
            "D6",
            "fn g(c: Channel[int]) -> int:\n    wait:\n        v := c.recv():\n            panic(\"x\")\nprint(g(Channel[int](1)))\n",
            "ok",
        ),
        (
            "S1",
            "fn exit(code: int) -> int:\n    return code\nfn f(x: int) -> int:\n    if x > 0:\n        return x\n    exit(1)\nprint(f(-1))\n",
            "fall off the end",
        ),
        (
            "S2",
            "struct D:\n    n: int\n    fn exit(self) -> int:\n        return self.n\nfn f(d: D) -> int:\n    d.exit()\nprint(f(D(1)))\n",
            "fall off the end",
        ),
        (
            "S3",
            "fn f() -> int:\n    panic := fn(s: str): print(s)\n    panic(\"x\")\nprint(f())\n",
            "fall off the end",
        ),
        (
            "S4",
            "fn f(exit: fn(int) -> int) -> int:\n    exit(1)\nprint(f(fn(c: int) -> int: c))\n",
            "fall off the end",
        ),
        (
            "S5",
            "fn f() -> int:\n    exit := fn(c: int): print(c)\n    exit(1)\nprint(f())\n",
            "fall off the end",
        ),
        (
            "T1",
            "fn exit(c: int):\n    print(c)\nfn f():\n    r := recover:\n        exit(1)\n    match r:\n        ?v: print(v)\n        !e: print(e.message())\nf()\n",
            "returns no value",
        ),
        (
            "T2",
            "fn f(c: Channel[int]):\n    r: ! = recover:\n        wait:\n            v := c.recv():\n                panic(\"x\")\n    match r:\n        ?_: print(1)\n        !e: print(e.message())\nf(Channel[int](1))\n",
            "ok",
        ),
        (
            "T3",
            "fn f():\n    r: int! = recover: panic(\"x\")\n    match r:\n        ?v: print(v)\n        !e: print(e.message())\nf()\n",
            "ok",
        ),
        (
            "I1",
            "import exit as quit from std.os\nfn g(): quit(1)\ng()\n",
            "ok",
        ),
        ("I2", "fn g(): panic(\"x\")\ng()\n", "ok"),
        (
            "E1",
            "fn f(c: Channel[int]):\n    defer:\n        wait:\n            _ := c.recv():\n                return\nf(Channel[int](1))\n",
            "'return' is not allowed inside a defer block",
        ),
        (
            "E2",
            "fn f():\n    spawn:\n        parallel:\n            return\nf()\n",
            "'return' is not allowed inside a spawn block",
        ),
        (
            "E3",
            "fn f():\n    for i in 0..2:\n        r := recover:\n            parallel:\n                break\n        print(r)\nf()\n",
            "'break' is not allowed inside a recover block",
        ),
        (
            "E4",
            "fn f():\n    r := recover:\n        for i in 0..2:\n            break\n    print(r)\nf()\n",
            "ok",
        ),
        (
            "E5",
            "fn f():\n    defer:\n        spawn:\n            return\nf()\n",
            "'return' is not allowed inside a spawn block",
        ),
        (
            "W1",
            "fn f(xs: List[int], c: Channel[int]):\n    wait:\n        _ := c.recv():\n            return\n    xs.push(1)\nxs := [0]\nc := Channel[int](1)\nc.send(1)\nspawn f(xs, c)\nprint(xs)\n",
            "ok",
        ),
        (
            "W2",
            "fn f(xs: List[int]):\n    xs.push(1)\nxs := [0]\nspawn f(xs)\nprint(xs)\n",
            "ok",
        ),
        (
            "W3",
            "fn f(xs: List[int], c: bool):\n    if c:\n        return\n    xs.push(1)\nxs := [0]\nspawn f(xs, true)\nprint(xs)\n",
            "ok",
        ),
        (
            "V1",
            "import exit as quit from std.os\nfn main():\n    r: int!str = !\"boom\"\n    x := match r:\n        ?v: v\n        !e: quit(2)\n    print(x)\nmain()\n",
            "branches have incompatible types: int and None",
        ),
        (
            "V2",
            "fn exit(c: int) -> int:\n    return c\nfn main():\n    r: int!str = !\"boom\"\n    x := match r:\n        ?v: v\n        !e: exit(2)\n    print(x)\nmain()\n",
            "ok",
        ),
        // A `for` body may run zero times: its `return` does not stop the fall-off.
        (
            "N1",
            "fn g(xs: List[int]) -> int:\n    for x in xs:\n        return x\nprint(g([1]))\n",
            "fall off the end",
        ),
        // A diverging call in one `match` arm and a `return` in the other.
        (
            "N2",
            "fn g(n: int) -> int:\n    match n:\n        0: return 1\n        _: panic(\"x\")\nprint(g(0))\n",
            "ok",
        ),
        // Each escape in a recover block reports at its own statement.
        (
            "N3",
            "fn f():\n    for i in 0..2:\n        r := recover:\n            if i == 0:\n                continue\n            return\n        print(r)\nf()\n",
            "'continue' is not allowed inside a recover block",
        ),
    ];
    let mut bad = Vec::new();
    for (label, src, want) in rows {
        let errs = check_entry(src);
        let pass = if *want == "ok" {
            errs.is_empty()
        } else {
            errs.iter().any(|e| e.message.contains(want))
        };
        if !pass {
            bad.push(format!("{label}: want {want:?}, got {errs:?}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} cells differ:\n{}",
        bad.len(),
        rows.len(),
        bad.join("\n")
    );
}

// TICKET-186 (Family D, wave 17): module scope has one record per global. Each program below is
// check-OK today and runs to a wrong value; each must be a compile error.
fn t186_rejected(src: &str) {
    let errs = check_src(src);
    assert!(
        !errs.is_empty(),
        "TICKET-186: expected a compile error, check accepted the program"
    );
}

#[test]
fn t186_option_refined_by_body_above_let_rejected() {
    // Runs to `abab` from an `-> int` fn (Go: `var z = nil` does not compile).
    t186_rejected(
        "fn have() -> str?:\n    return \"ab\"\nfn w():\n    z = have()\nz := None\nw()\nfn r() -> int:\n    return z ?? 0\nprint(r() + r())\n",
    );
    t186_rejected(
        "fn w():\n    z = ?\"ab\"\nz := None\nw()\nfn r() -> int:\n    return z ?? 0\nprint(r() + r())\n",
    );
}

#[test]
fn t186_fn_redeclared_with_other_labels_rejected() {
    // Prints 21 (bound by the OLD fn's labels); CPython binds by the live fn's labels (12).
    t186_rejected(
        "fn f(a: int, b: int) -> int:\n    return a * 10 + b\nfn call() -> int:\n    return f(a=1, b=2)\nf := fn(b: int, a: int) -> int: a * 10 + b\nprint(call())\n",
    );
}

#[test]
fn t186_plain_then_const_redeclare_rejected() {
    // Owner decision 2026-09-30: plain + const declaration of one global is an error, either order.
    t186_rejected(
        "PI := 2.0\nfn bump():\n    PI = 3.0\nPI: const float = 3.14\nbump()\nprint(PI)\n",
    );
    t186_rejected("PI: const float = 3.14\nPI := 2.0\nprint(PI)\n");
}

#[test]
fn t186_module_level_return_rejected() {
    // Owner decision 2026-09-30: CPython `SyntaxError: 'return' outside function`.
    rejects("return\nx := 5\n", "'return' outside a function");
}

#[test]
fn module_return_in_top_level_block_reports_once() {
    // Named outside the `t186_` filter, which the acceptance criteria pin at 6 tests.
    // A top-level `defer:` / `recover:` / `spawn:` block rejects its own `return`; the
    // module-level rule must not report the same token a second time.
    for (src, want) in [
        ("defer:\n    return\nprint(1)\n", "inside a defer block"),
        (
            "r := recover:\n    return\nprint(r)\n",
            "inside a recover block",
        ),
        ("spawn:\n    return\n", "inside a spawn block"),
    ] {
        let errs = check_src(src);
        assert!(
            errs.len() == 1 && errs[0].message.contains(want),
            "want one {want:?} error, got: {errs:?}"
        );
    }
}

/// TICKET-186 grid: one record per module slot. Axes: the fact (Option refinement,
/// empty-collection refinement, const, keyword labels, init) x the writer's position (a body
/// above the first let, below it, between two lets, none) x the reader's position (a fn body, a
/// closure, a `defer:`/`spawn:` block, a top-level statement). `None` = check clean, `Some("")` =
/// any error, `Some(s)` = an error containing `s`.
#[test]
fn t186_module_global_record_grid() {
    // A module name is bound once (`GlobalBinding::clashes`), so every cell that declares `f` or
    // `PI` twice is that error, whatever reads it.
    const DUP: &str = "is already defined";
    const RET: &str = "'return' outside a function";
    let f = "fn f(a: int, b: int) -> int:\n    return a * 10 + b\n";
    let l1 = "f := fn(a: int, b: int) -> int: a * 10 + b\n";
    let l2 = "f := fn(b: int, a: int) -> int: a * 10 + b\n";
    let cells: Vec<(&str, String, Option<&str>)> = vec![
        ("c01", "fn have() -> str?:\n    return \"ab\"\nfn w():\n    z = have()\nz := None\nw()\nfn r() -> int:\n    return z ?? 0\nprint(r())\n".into(), Some("")),
        ("c02", "z := None\nfn have() -> str?:\n    return \"ab\"\nfn w():\n    z = have()\nw()\nfn r() -> int:\n    return z ?? 0\nprint(r())\n".into(), Some("")),
        ("c03", "z := None\nfn have() -> str?:\n    return \"ab\"\nfn w():\n    z = have()\nz := None\nw()\nfn r() -> int:\n    return z ?? 0\nprint(r())\n".into(), Some("")),
        ("c01q", "fn w():\n    z = ?\"ab\"\nz := None\nw()\nfn r() -> int:\n    return z ?? 0\nprint(r())\n".into(), Some("")),
        ("c02q", "z := None\nfn w():\n    z = ?\"ab\"\nw()\nfn r() -> int:\n    return z ?? 0\nprint(r())\n".into(), Some("")),
        ("c03q", "z := None\nfn w():\n    z = ?\"ab\"\nz := None\nw()\nfn r() -> int:\n    return z ?? 0\nprint(r())\n".into(), Some("")),
        ("c04", "fn w():\n    xs = [\"a\"]\nxs := []\nw()\nfn r() -> int:\n    return xs[0]\nprint(r())\n".into(), Some("")),
        ("c05", "xs := []\nfn w():\n    xs = [\"a\"]\nw()\nfn r() -> int:\n    return xs[0]\nprint(r())\n".into(), Some("")),
        ("c06", "xs := []\nfn w():\n    xs = [\"a\"]\nxs := []\nw()\nfn r() -> int:\n    return xs[0]\nprint(r())\n".into(), Some("")),
        ("c07", "PI := 2.0\nfn bump():\n    PI = 3.0\nPI: const float = 3.14\nbump()\nprint(PI)\n".into(), Some(DUP)),
        ("c20", "PI := 1.0\nPI: const float = 2.0\nprint(PI)\n".into(), Some(DUP)),
        ("c08", "PI: const float = 3.14\nPI := 2.0\nprint(PI)\n".into(), Some(DUP)),
        ("c09", "fn bump():\n    PI = 3.0\nPI: const float = 3.14\nbump()\nprint(PI)\n".into(), Some("cannot reassign const binding 'PI'")),
        ("c10", "PI: const float = 3.14\nfn bump():\n    PI = 3.0\nbump()\nprint(PI)\n".into(), Some("cannot reassign const binding 'PI'")),
        ("c11", "PI: const float = 1.0\nPI: const float = 2.0\nprint(PI)\n".into(), Some(DUP)),
        ("c12", format!("{f}fn call() -> int:\n    return f(a=1, b=2)\n{l2}print(call())\n"), Some(DUP)),
        ("c13", format!("{f}x := call()\n{l2}fn call() -> int:\n    return f(a=1, b=2)\nprint(x)\n"), Some(DUP)),
        ("c14", format!("{l1}fn call() -> int:\n    return f(a=1, b=2)\n{l2}print(call())\n"), Some(DUP)),
        ("c15", format!("{f}fn call() -> int:\n    return f(1, 2)\n{l2}print(call())\n"), Some(DUP)),
        ("c16", format!("{f}fn call() -> int:\n    return f(a=1, b=2)\nprint(call())\n"), None),
        ("c22", format!("{f}print(f(a=1, b=2))\n{l2}"), Some(DUP)),
        ("c23", format!("{f}{l2}print(f(a=1, b=2))\n"), Some(DUP)),
        ("c24", format!("{l1}{l2}print(f(a=1, b=2))\n"), Some(DUP)),
        ("c25", format!("{l1}{l2}fn call() -> int:\n    return f(a=1, b=2)\nprint(call())\n"), Some(DUP)),
        ("c26", format!("{f}defer print(f(a=1, b=2))\n{l2}print(0)\n"), Some(DUP)),
        ("c27", format!("{l1}g := fn() -> int: f(a=1, b=2)\n{l2}print(g())\n"), Some(DUP)),
        ("c28", format!("{f}defer:\n    print(f(a=1, b=2))\n{l2}"), Some(DUP)),
        ("c29", format!("{f}spawn:\n    print(f(a=1, b=2))\n{l2}"), Some(DUP)),
        ("c30", format!("{f}parallel:\n    print(f(a=1, b=2))\n{l2}"), Some(DUP)),
        ("c17", "return\nx := 5\n".into(), Some(RET)),
        ("c18", "if true:\n    return\nx := 5\n".into(), Some(RET)),
        ("c19", "fn f():\n    return\nf()\n".into(), None),
    ];
    let mut bad = Vec::new();
    for (label, src, want) in &cells {
        let errs = check_src(src);
        let pass = match want {
            None => errs.is_empty(),
            Some("") => !errs.is_empty(),
            Some(s) => errs.iter().any(|e| e.message.contains(s)),
        };
        if !pass {
            bad.push(format!("{label}: want {want:?}, got {errs:?}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} cells differ:\n{}",
        bad.len(),
        cells.len(),
        bad.join("\n")
    );
}

/// TICKET-263 — a nested `fn` name is a candidate, and a call inside the fn's own body is not a read
/// (rustc `dead_code`): the warning reads `function 'lone' is never used`. A recursive nested fn that
/// the enclosing fn calls stays silent, and so does a `_`-prefixed one.
#[test]
fn a_nested_fn_used_only_by_itself_is_reported() {
    warns(
        "fn m():\n    fn lone(n: int) -> int:\n        return lone(n)\nm()\n",
        "function 'lone' is never used",
    );
    no_warn(
        "fn m():\n    fn fact(n: int) -> int:\n        if n == 0:\n            return 1\n        return n * fact(n - 1)\n    print(fact(3))\nm()\n",
    );
    no_warn("fn m():\n    fn _lone(n: int) -> int:\n        return _lone(n)\nm()\n");
}

/// TICKET-283 — an unused nested `fn` is named a function, in rustc's wording (`function 'f' is never
/// used`), not `unused variable`. A plain unused local keeps `unused variable`.
#[test]
fn an_unused_nested_fn_is_named_a_function() {
    let msgs = |src: &str| -> Vec<String> {
        let (errs, warns) = warn_src(src);
        assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
        warns.into_iter().map(|w| w.message).collect()
    };
    let unused = msgs("fn m():\n    fn f() -> int:\n        return 1\n    print(1)\nm()\n");
    let rec = msgs(
        "fn m():\n    fn rec(n: int) -> int:\n        if n == 0:\n            return 0\n        return rec(n - 1)\n    print(1)\nm()\n",
    );
    let once = msgs("fn m():\n    fn f() -> int:\n        return 1\n    print(f())\nm()\n");
    let local = msgs("fn m():\n    x := 1\n    print(1)\nm()\n");
    let all = format!("unused={unused:?} rec={rec:?} once={once:?} local={local:?}");
    assert!(
        unused
            .iter()
            .any(|m| m.contains("function 'f' is never used")),
        "unused nested fn: {all}"
    );
    assert!(
        rec.iter()
            .any(|m| m.contains("function 'rec' is never used")),
        "self-recursive nested fn: {all}"
    );
    assert!(once.is_empty(), "nested fn read once: {all}");
    assert!(
        local.iter().any(|m| m.contains("unused variable 'x'")),
        "unused local: {all}"
    );
}
