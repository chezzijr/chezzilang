//! Checker tests: polymorphic math, HOFs, ord/chr, bitwise, tuple match, map iteration, loop var,
//! const, module redeclaration, break/continue, map, tuples.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

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
        "import std.cmp\nstruct P:\n    n: int\n    fn compare(self, other: P) -> int:\n        return self.n - other.n\n    fn eq(self, other: P) -> bool:\n        return self.n == other.n\nfn main():\n    p := cmp.max(P(1), P(2))\n    print(p.n)\n",
    );
}

#[test]
fn cmp_max_over_non_comparable_struct_rejected() {
    entry_rejects(
        "import std.cmp\nstruct P:\n    n: int\nfn main():\n    p := cmp.max(P(1), P(2))\n    print(p.n)\n",
        "does not satisfy Comparable (missing method 'compare')",
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
        "write 1.0 (or x.to_float())",
    );
    entry_ok("import std.cmp\nfn main():\n    x: float = cmp.max(3, 5).to_float()\n    print(x)\n");
}

#[test]
fn d3_cmp_max_mixed_int_float_rejected() {
    // D3 (supersedes TICKET-106 W12-15): a bare `T` slot never widens the untyped int constant `3`
    // beside `5.0`; the fix is `cmp.max(3.0, 5.0)`.
    entry_rejects(
        "import std.cmp\nfn main():\n    print(cmp.max(3, 5.0))\n",
        "write 1.0",
    );
    entry_ok("import std.cmp\nfn main():\n    print(cmp.max(3.0, 5.0))\n");
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
        "write 1.0 (or x.to_float())",
    );
}

#[test]
fn d3_native_math_floor_rejects_int_arg() {
    // D3: `floor`/`ceil`/`sqrt` take a float-only param; `floor(2)` is rejected, `floor(2.0)` checks.
    entry_rejects(
        "import std.math\nfn main():\n    print(math.floor(2))\n",
        "write 1.0",
    );
    entry_ok("import std.math\nfn main():\n    print(math.floor(2.0))\n");
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
    ok("o: (int, int)? = ?(1, 2)\nmatch o:\n    None: print(\"n\")\n    ?(a, b): print(a + b)\n");
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
fn for_over_map_binds_a_pair() {
    // A Map iterates `(key, value)` pairs (owner U9); `m.keys()` gives the keys.
    ok("m := {\"a\": 1}\nfor kv in m:\n    p: (str, int) = kv\n    print(p)\n");
    rejects("m := {\"a\": 1}\nfor k in m:\n    s: str = k\n", "");
    ok("m := {\"a\": 1}\nfor k in m.keys():\n    s: str = k\n    print(s)\n");
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
        "tuple pattern cannot match a value of type int",
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
    rejects(
        "for a, b in 0..3:\n    print(a)\n",
        "tuple pattern cannot match a value of type int",
    );
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

// ===== module-scope re-declaration: may REBIND the global, may not RETYPE it (W7-42) =====

#[test]
fn fn_local_redeclare_changing_type_ok() {
    // `add_local` pushes a FRESH slot for a fn-local re-declare, so it is a genuine Rust-style shadow
    // and may change type (Rust, measured: the pre-shadow closure keeps the old binding). Untouched.
    ok("fn f():\n    y := 1\n    y := \"9\"\n    print(y)\n");
}

#[test]
fn top_level_block_redeclare_changing_type_ok() {
    // A top-level `if:`/`for:` body is scope > 1 and routes to `add_local`, NOT to a global slot —
    // which is why `scopes.len() == 1` is the discriminator and `in_fn_body` would over-reject here.
    ok("x := 1\nc := true\nif c:\n    x := \"s\"\n    print(x)\n");
    ok("x := \"s\"\nfor x in 0..3:\n    print(x)\n");
}

#[test]
fn fn_local_shadow_of_module_global_ok() {
    ok("x := 1\nfn f():\n    x := \"s\"\n    print(x)\n");
}

#[test]
fn value_read_above_its_own_import_rejected() {
    // W7-42r shape (b). Imports are HOISTED — `check_module` runs `bind_import` for every import
    // BEFORE the `check_stmt` loop — so an imported name sits in `scopes[0]` from line 1 whatever
    // line its `import` is on, and a read above the `import` silently resolves to the imported
    // binding. Measured on the pre-fix release binary: `chezzi check` said "ok: no type errors" and
    // the program PRINTED `1` out of a closure declared `-> str` (the `x := 1` on line 2 fills the
    // one slot the hoisted import bound, and the closure on line 1 was typed against the import).
    // The W7-42 re-declaration rule cannot reach this: its import gate is deliberately one-way
    // (fires only when the `import` is source-EARLIER), and inverting it would break the sound
    // `module_scope_redeclare_over_hoisted_import_ok` below. So the forward READ is the error.
    files_reject(
        &[
            ("lib/st.chz", "COUNT := \"sv\"\n"),
            (
                "main.chz",
                "f := fn() -> str: x\nx := 1\nimport COUNT as x from lib.st\nprint(f())\n",
            ),
        ],
        "'x' is used before its `import` on line 3",
    );
    // The simple form. This one is TECHNICALLY SOUND in Chezzi today — the hoist means it prints
    // `sv` — and it is rejected anyway, deliberately, because it reads as a use-before-definition
    // and BOTH owning ancestors refuse it (measured this session):
    //   CPython 3.14.6: `print(COUNT)` then `from math import pi as COUNT`
    //                   -> NameError: name 'COUNT' is not defined
    //   Go 1.26.5:      a `func` then `import "math"`
    //                   -> syntax error: imports must appear before other declarations
    // Do not "fix" this back to accepting it. (Pre-fix binary: "ok: no type errors".)
    files_reject(
        &[
            ("lib/st.chz", "COUNT := \"sv\"\n"),
            ("main.chz", "print(COUNT)\nimport COUNT from lib.st\n"),
        ],
        "'COUNT' is used before its `import` on line 2",
    );
    // A whole-module bind is the same read (`Ty::Module`), and it is the shape that actually RAN
    // wrong pre-fix: `chezzi check` said ok and `chezzi run` printed `m` from a module bound below
    // the call.
    files_reject(
        &[
            ("lib/st.chz", "fn g() -> str:\n    return \"m\"\n"),
            ("main.chz", "print(st.g())\nimport lib.st\n"),
        ],
        "'st' is used before its `import` on line 2",
    );
    // A FROM-IMPORTED FN read above its own `import` is the same use-before-import, and must give
    // the SAME verdict as the value form — an internal split ("`print(COUNT)` rejected but `g := h`
    // accepted") is arbitrary from the user's side. Ancestors agree here too: CPython raises
    // `NameError` for `g = h` before `from lib import h`, Go refuses the late `import` outright.
    // (Pre-fix binary AND the first cut of this rule: "ok: no type errors".)
    files_reject(
        &[
            ("lib/fns.chz", "fn h() -> str:\n    return \"fnval\"\n"),
            ("main.chz", "g := h\nimport h from lib.fns\nprint(g())\n"),
        ],
        "'h' is used before its `import` on line 2",
    );
    // …and the DIRECT call spelling of the same thing. A direct callee never reaches `infer_ident`,
    // so without its own guard `h()` would be accepted while `g := h` is rejected — the very split
    // this case exists to close. Exactly one error, not two (the callee is not re-inferred).
    let errs = check_files(&[
        ("lib/fns.chz", "fn h() -> str:\n    return \"fnval\"\n"),
        ("main.chz", "print(h())\nimport h from lib.fns\n"),
    ]);
    assert_eq!(
        errs.iter()
            .filter(|e| e.message.contains("is used before its `import`"))
            .count(),
        1,
        "expected exactly one use-before-import error, got: {errs:?}"
    );
    // A DEFERRED read — a top-level `fn` body above the import — is rejected too, deliberately, and
    // here the ancestors SPLIT (both measured this session):
    //   CPython 3.14.6: accepts it — the body runs after the import, so there is no NameError.
    //   Go 1.26.5:      refuses — `syntax error: imports must appear before other declarations`.
    // We follow Go, because the hoist makes the sound and the unsound case indistinguishable AT THE
    // READ SITE: this exact program is fine until some later `let` refills the slot (which is the
    // closure defect above), and nothing at the read tells the two apart. Do NOT loosen this to
    // "only immediate reads". (Pre-fix binary: "ok: no type errors".)
    files_reject(
        &[
            ("lib/st.chz", "COUNT := \"sv\"\n"),
            (
                "main.chz",
                "fn body() -> str:\n    return COUNT\nimport COUNT from lib.st\nprint(body())\n",
            ),
        ],
        "'COUNT' is used before its `import` on line 3",
    );
}

#[test]
fn value_read_above_its_own_import_boundaries_ok() {
    // The overwhelmingly normal case: every read BELOW its import.
    files_ok(&[
        ("lib/st.chz", "COUNT := \"s\"\n"),
        ("main.chz", "import COUNT from lib.st\nprint(COUNT)\n"),
    ]);
    // …including through an interpolated fragment, which re-lexes with its own spans.
    files_ok(&[
        ("lib/st.chz", "COUNT := \"s\"\n"),
        ("main.chz", "import COUNT from lib.st\nprint(\"{COUNT}\")\n"),
    ]);
    // A read above an import of a DIFFERENT name is untouched — it is the name collision that
    // matters, not the mere presence of a late import.
    files_ok(&[
        ("lib/st.chz", "COUNT := \"s\"\n"),
        (
            "main.chz",
            "x := 1\nprint(x)\nimport COUNT as y from lib.st\nprint(y)\n",
        ),
    ]);
    // ORDER neighbour for the from-imported fn: the same read placed BELOW its import is legal.
    files_ok(&[
        ("lib/fns.chz", "fn h() -> str:\n    return \"fnval\"\n"),
        ("main.chz", "import h from lib.fns\ng := h\nprint(g())\n"),
    ]);
    // …and a SAME-MODULE top-level `fn` is untouched wherever it is read: it is not in
    // `import_binds`, and it is legitimately position-independent (its slot is filled before any
    // statement runs — `compiler/mod.rs:1404`, `desugar/mod.rs:689`), so reading it above its own
    // declaration, and above an unrelated later let, stays legal.
    ok("g := h\nk := 1\nfn h() -> str:\n    return \"m\"\nprint(g())\nprint(k)\n");
    // VALUE/CALLABLE reads only: a bare TYPE name resolves through `bare_types`/`resolve_type`,
    // never `infer_ident`, so a type used above its import stays legal (the shipped
    // `tests/chz/spec/eq_protocol_test.chz` is the real-world instance).
    files_ok(&[
        ("lib/st.chz", "struct P:\n    n: int\n"),
        (
            "main.chz",
            "fn mk() -> P:\n    return P(n=1)\nimport P from lib.st\nprint(mk().n)\n",
        ),
    ]);
}

#[test]
fn read_above_import_shadowed_by_an_inner_binding_ok() {
    // The over-rejection this gate shipped with: `imported_values` is keyed by BARE NAME, so a read
    // that resolved to a PARAMETER / fn-local / loop var / block local fired the gate anyway, and the
    // diagnostic's own sentence ("this reads the imported binding") was simply untrue — the import's
    // binding is shadowed at the read. All five programs RUN correctly on the fixed binary (measured:
    // `6 s` / `5 s` / `3 s` / `9 s` / `4 s`); on the pre-fix binary each was a type error.
    let st = ("lib/st.chz", "COUNT := \"s\"\n");
    // parameter
    files_ok(&[
        st,
        (
            "main.chz",
            "fn area(COUNT: int) -> int:\n    return COUNT * 2\nimport COUNT from lib.st\nprint(area(3))\nprint(COUNT)\n",
        ),
    ]);
    // fn-local `:=`
    files_ok(&[
        st,
        (
            "main.chz",
            "fn f() -> int:\n    COUNT := 5\n    return COUNT\nimport COUNT from lib.st\nprint(f())\nprint(COUNT)\n",
        ),
    ]);
    // loop variable
    files_ok(&[
        st,
        (
            "main.chz",
            "fn f() -> int:\n    s := 0\n    for COUNT in [1, 2]:\n        s = s + COUNT\n    return s\nimport COUNT from lib.st\nprint(f())\nprint(COUNT)\n",
        ),
    ]);
    // `if:` block scope
    files_ok(&[
        st,
        (
            "main.chz",
            "fn f() -> int:\n    if true:\n        COUNT := 9\n        return COUNT\n    return 0\nimport COUNT from lib.st\nprint(f())\nprint(COUNT)\n",
        ),
    ]);
    // `while:` block scope
    files_ok(&[
        st,
        (
            "main.chz",
            "fn f() -> int:\n    n := 0\n    while n < 1:\n        COUNT := 4\n        n = n + COUNT\n    return n\nimport COUNT from lib.st\nprint(f())\nprint(COUNT)\n",
        ),
    ]);

    // The same four shapes for a from-imported FN name, in BOTH spellings (`g := h` and `h()`).
    // These are gated a scope earlier — the `functions` arm and the `infer_call` callee funnel are
    // reached only when `lookup(name)` is `None`, so an inner binding wins before the gate is
    // consulted — but they are the same user-visible concept and must never regress into the value
    // arm's defect.
    let fns = ("lib/f.chz", "fn h() -> int:\n    return 7\n");
    // parameter, call spelling
    files_ok(&[
        fns,
        (
            "main.chz",
            "fn f(h: fn() -> int) -> int:\n    return h()\nimport h from lib.f\nprint(f(h))\n",
        ),
    ]);
    // fn-local, bare-read spelling
    files_ok(&[
        fns,
        (
            "main.chz",
            "fn f() -> int:\n    h := 3\n    g := h\n    return g\nimport h from lib.f\nprint(f())\nprint(h())\n",
        ),
    ]);
    // fn-local of fn type, call spelling
    files_ok(&[
        fns,
        (
            "main.chz",
            "fn f() -> int:\n    h := fn() -> int: 4\n    return h()\nimport h from lib.f\nprint(f())\nprint(h())\n",
        ),
    ]);
    // loop variable
    files_ok(&[
        fns,
        (
            "main.chz",
            "fn f() -> int:\n    s := 0\n    for h in [1, 2]:\n        s = s + h\n    return s\nimport h from lib.f\nprint(f())\nprint(h())\n",
        ),
    ]);
    // `if:` block scope, call spelling
    files_ok(&[
        fns,
        (
            "main.chz",
            "fn f() -> int:\n    if true:\n        h := fn() -> int: 9\n        return h()\n    return 0\nimport h from lib.f\nprint(f())\nprint(h())\n",
        ),
    ]);
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
// The compiler's empty loop stack is an internal error: the checker rejects every such `break`.

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
    ok("m := {\"a\": 1}\no: int? = m.get(\"a\")\n");
}

#[test]
fn map_has_method_is_bool() {
    ok("m := {\"a\": 1}\nb: bool = m.contains(\"a\")\n");
}

#[test]
fn map_len_method_is_int() {
    ok("m := {\"a\": 1}\nn: int = m.len()\n");
}

#[test]
fn map_remove_method_is_option_of_value() {
    ok("m := {\"a\": 1}\nr: int? = m.remove(\"a\")\n");
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
fn destructuring_value_list_arity_mismatch_is_a_type_error() {
    rejects(
        "fn main():\n    a, b := 1, 2, 3\n",
        "tuple pattern has 2 element(s), but the value has 3",
    );
}

#[test]
fn tuple_assign_value_list_arity_mismatch_is_a_type_error() {
    rejects(
        "fn main():\n    a := 0\n    b := 0\n    a, b = 1, 2, 3\n",
        "assignment has 2 target(s) but the value has 3 element(s)",
    );
}

#[test]
fn destructure_arity_mismatch_rejected() {
    rejects(
        "fn pair() -> (int, int):\n    return (1, 2)\nfn main():\n    a, b, c := pair()\n",
        "tuple pattern has 3 element(s), but the value has 2",
    );
}

#[test]
fn destructure_non_tuple_rejected() {
    rejects(
        "fn main():\n    a, b := 5\n",
        "tuple pattern cannot match a value of type int",
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

#[test]
fn a_module_level_name_is_bound_once() {
    // Go (`x redeclared in this block`) and Rust (E0428 / E0255; no top-level `let`, a `static`
    // twice is E0428) bind a top-level name once, whatever the types: a second `:=`, typed let or
    // destructure of a module name is "already defined", in either order against a hoisted `fn`
    // or `import`. `=` reassigns (`GlobalBinding::clashes`).
    for (src, needle) in [
        ("x := 1\nx := 2\n", "variable 'x' is already defined"),
        ("x := 1\nx := \"s\"\n", "variable 'x' is already defined"),
        (
            "x: int = 1\nx: int = 2\n",
            "variable 'x' is already defined",
        ),
        (
            "v: Any = 1\nv: Any = \"s\"\n",
            "variable 'v' is already defined",
        ),
        (
            "x := 1\nx, y := (2, 3)\nprint(y)\n",
            "variable 'x' is already defined",
        ),
        (
            "X: const int = 1\nX := 2\n",
            "variable 'X' is already defined",
        ),
        (
            "X := 1\nX: const int = 2\n",
            "variable 'X' is already defined",
        ),
        // The message names the prior binding.
        (
            "fn h() -> int:\n    return 7\nh := fn() -> int: 9\n",
            "function 'h' is already defined",
        ),
        (
            "h := fn() -> int: 9\nfn h() -> int:\n    return 7\n",
            "variable 'h' is already defined",
        ),
        (
            "extern \"libc\":\n    fn abs(x: int) -> int\nabs := fn(x: int) -> int: x\n",
            "function 'abs' is already defined",
        ),
        ("import std.math\nmath := 1\n", "'math' is already imported"),
    ] {
        let errs = check_src(src);
        assert!(
            errs.len() == 1 && errs[0].message.contains(needle),
            "{src:?}: want one error containing {needle:?}, got {errs:?}"
        );
    }
    // The error points at the clashing name of a destructure.
    let errs = check_src("x := 1\ny, x := (2, 3)\nprint(y)\n");
    assert_eq!((errs[0].span.line, errs[0].span.col), (2, 4), "{errs:?}");
    for (lib, main) in [
        (
            "COUNT := 0\n",
            "import COUNT from lib.st\nCOUNT := COUNT + 1\n",
        ),
        ("Y: const int = 5\n", "import Y from lib.st\nY := 7\n"),
        (
            "fn g() -> str:\n    return \"m\"\n",
            "import g from lib.st\ng := fn() -> str: \"n\"\n",
        ),
    ] {
        files_reject(
            &[("lib/st.chz", lib), ("main.chz", main)],
            "is already imported",
        );
    }
    // An import after a let of its name: the message names the let.
    files_reject(
        &[
            ("lib/st.chz", "COUNT := 0\n"),
            ("main.chz", "x := 1\nimport COUNT as x from lib.st\n"),
        ],
        "variable 'x' is already defined",
    );
    // What stays legal: `=`, and a binding in a fn or block body (a fresh shadow, Rust's `let`).
    ok("x := 1\nx = 2\nprint(x)\n");
    ok("x := 1\nfn f():\n    x := \"s\"\n    print(x)\nf()\nprint(x)\n");
    ok("fn f():\n    y := 1\n    y := \"9\"\n    print(y)\nf()\n");
    ok("x := 1\nif true:\n    x := \"s\"\n    print(x)\nprint(x)\n");
    // The const rule still holds inside a fn body.
    rejects(
        "fn f():\n    X: const int = 1\n    X, y := (2, 3)\n    print(X)\n    print(y)\nf()\n",
        "cannot re-declare const binding",
    );
    rejects(
        "fn f():\n    X: const int = 1\n    X: int = 2\n    print(X)\nf()\n",
        "cannot re-declare const binding",
    );
}
