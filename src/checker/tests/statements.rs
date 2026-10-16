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
        "write 1.0 (or float(x))",
    );
    entry_ok("import std.cmp\nfn main():\n    x: float = float(cmp.max(3, 5))\n    print(x)\n");
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
        "write 1.0 (or float(x))",
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

// ===== module-scope re-declaration: may REBIND the global, may not RETYPE it (W7-42) =====

#[test]
fn module_scope_redeclare_changing_type_rejected() {
    // The check-clean unsoundness: `collect_globals` is idempotent by NAME, so both `x`s are the ONE
    // global slot — the closure keeps reading it and hands a `str` out of a `fn() -> int`. Measured
    // pre-fix: `chezzi check` said "ok: no type errors" and the program printed `9`.
    rejects(
        "x := 1\nf := fn() -> int: x\nx := \"9\"\nprint(f())\n",
        "cannot re-declare module-level binding 'x' with a different type (int -> str)",
    );
}

#[test]
fn module_scope_redeclare_same_type_ok() {
    // Python-style late binding is preserved: a re-bind of the SAME type stays legal, and a closure
    // over the slot legitimately observes the new value.
    ok("x := 1\nx := 2\nf := fn() -> int: x\nprint(f())\n");
}

#[test]
fn module_scope_typed_redeclare_changing_type_rejected() {
    // The typed-`let` path routes through the same binding site, so it is covered too.
    rejects("x: int = 1\nx: str = \"9\"\n", "int -> str");
}

#[test]
fn module_scope_redeclare_int_to_float_rejected() {
    // int -> float is a type change like any other — measured pre-fix, `x := 1; f := fn() -> int: x;
    // x := 1.0` gave `y: int = f()` the value `1.0` and `print(y + 1)` printed `2.0`.
    rejects("x := 1\nx := 1.0\n", "int -> float");
}

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
fn module_scope_redeclare_unknown_slot_ok() {
    // `Unknown` — the bare sentinel or one sitting in a type slot — never fires: it is the checker's
    // don't-know value, and refining an empty literal is a script-writer idiom, not a retype.
    ok("x: List[int] = []\nx := [1]\nprint(x)\n");
    // …but two CONCRETE element types are the same one-slot lie as any other retype.
    rejects("xs := [1]\nxs := [\"a\"]\n", "List[int] -> List[str]");
}

#[test]
fn module_scope_redeclare_unknown_carve_out_is_one_sided() {
    // The carve-out asks "is `declared` a REFINEMENT of `prev`?" — NOT "does either side hold an
    // `Unknown`?". The symmetric form silenced the rule in BOTH directions, so all four of these
    // were check-clean and wrong at runtime (measured on the pre-fix release binary):
    //   `x := 1` / `f := fn() -> int: x` / `x := None`   → `y: int = f()` accepted, printed `None`
    //   `x := []` / `f := fn() -> List[int]: x` / `x := 42`      → printed `42`
    //   `x := None` / `f := fn() -> Option[int]: x` / `x := 42`  → printed `42`
    //   `x := 1` / `f := fn() -> int: x` / `x := []` / `x.push(3)` → `Add to List and int` at runtime
    rejects("x := 1\nf := fn() -> int: x\nx := None\n", "int -> None");
    rejects(
        "x := []\nf := fn() -> List[int]: x\nx := 42\n",
        "cannot infer the",
    );
    rejects(
        "x := None\nf := fn() -> int?: x\nx := 42\n",
        "cannot infer the",
    );
    rejects(
        "x := 1\nf := fn() -> int: x\nx := []\nx.push(3)\n",
        "int -> List[?]",
    );
    // TICKET-238: an open first declaration is rejected on its own line at every shape; a later
    // declaration never fills it in.
    rejects("x := []\nx := [1]\nprint(x)\n", "cannot infer the");
    rejects("y := {}\ny := {\"a\": 1}\nprint(y)\n", "cannot infer the");
    rejects("z := None\nz := ?1\nprint(z)\n", "cannot infer the");
    rejects("s := Set()\ns.add(1)\nprint(s)\n", "cannot infer the");
    ok("w := 1\nw := 2\nprint(w)\n");
    // A deeper open slot is rejected the same way, and a same-shape retype still rejects.
    rejects("m := {}\nm := {\"a\": [1]}\nprint(m)\n", "cannot infer the");
    rejects(
        "m := {\"a\": 1}\nm := {\"a\": \"s\"}\n",
        "Map[str, int] -> Map[str, str]",
    );
}

#[test]
fn module_scope_redeclare_const_keeps_const_message() {
    // The const carve-out wins: a live const reports the const message and ONLY that one.
    rejects(
        "X: const int = 1\nX := \"s\"\n",
        "cannot re-declare const binding",
    );
    let errs = check_src("X: const int = 1\nX := \"s\"\n");
    assert!(
        !errs.iter().any(|e| e.message.contains("module-level")),
        "a live const must not ALSO report the retype error, got: {errs:?}"
    );
}

#[test]
fn module_scope_redeclare_inferred_return_no_double_fire() {
    // Return inference truncates errors and re-walks a body inside one open scope, so the rule is
    // gated on `!inferring_ret` — it must neither false-fire nor double-fire around an inferred fn.
    ok("x := 1\nfn g() -> int:\n    return x\nx := 2\nprint(g())\n");
    let errs = check_src("x := 1\nfn g() -> int:\n    return x\nx := \"s\"\nprint(g())\n");
    assert_eq!(
        errs.iter()
            .filter(|e| e.message.contains("module-level binding"))
            .count(),
        1,
        "expected exactly one retype error, got: {errs:?}"
    );
}

#[test]
fn module_scope_redeclare_over_hoisted_import_rejected() {
    // Imports are HOISTED, so the import is the slot's first binding wherever it sits, and a let of
    // the name at another type is a retype in either source order (Go rejects `var x` beside a
    // dot-imported `x` in both orders). The same type stays legal.
    files_reject(
        &[
            ("lib/st.chz", "COUNT := \"s\"\n"),
            (
                "main.chz",
                "x := 1\nf := fn() -> int: x\nimport COUNT as x from lib.st\nprint(f())\n",
            ),
        ],
        "cannot re-declare module-level binding 'x' with a different type (str -> int)",
    );
    files_reject(
        &[
            ("lib/st.chz", "COUNT := \"s\"\n"),
            ("main.chz", "st := 1\nimport lib.st\nprint(1)\n"),
        ],
        "cannot re-declare module-level binding 'st'",
    );
    files_ok(&[
        ("lib/st.chz", "COUNT := 0\n"),
        (
            "main.chz",
            "x := 1\nf := fn() -> int: x\nimport COUNT as x from lib.st\nprint(f())\n",
        ),
    ]);
}

#[test]
fn module_scope_redeclare_under_source_earlier_import_rejected() {
    // The mirror image, and the shape the first cut of this rule LET THROUGH: when the `import` is
    // source-EARLIER, the closure between it and the `let` really does read the one slot, so this is
    // the W7-42 defect verbatim. Measured on the pre-fix binary: `chezzi check` said "ok", the
    // program printed `s` out of a `fn() -> int`, and `print(y + 1)` died with
    // "cannot apply Add to str and int".
    files_reject(
        &[
            ("lib/st.chz", "COUNT := 0\n"),
            (
                "main.chz",
                "import COUNT from lib.st\nf := fn() -> int: COUNT\nCOUNT := \"s\"\nprint(f())\n",
            ),
        ],
        "cannot re-declare module-level binding 'COUNT' with a different type (int -> str)",
    );
    // Same for a whole-module bind: `import lib.st` then `st := 1` retypes the slot a source-earlier
    // closure resolves `st.g()` against (measured pre-fix: check-clean, then "type int has no method").
    files_reject(
        &[
            ("lib/st.chz", "fn g() -> str:\n    return \"m\"\n"),
            ("main.chz", "import lib.st\nst := 1\nprint(st)\n"),
        ],
        "module st -> int",
    );
}

#[test]
fn module_scope_redeclare_not_suppressed_by_an_unrelated_later_import() {
    // The import span is consulted ONLY while the previous binding is still the IMPORT's. Keying on it
    // unconditionally turned a source-LATER import into a per-NAME suppressor: the `let` had already
    // handed the name back (`declare` clears `imported_values`), so `prev` was the first LET's type and
    // the import's position was irrelevant — yet it silenced the rule and re-opened W7-42 verbatim.
    // Measured on the intermediate binary: check-clean, `f()` printed `s` out of a `fn() -> int`, and
    // `print(y + 1)` died with "cannot apply Add to str and int".
    files_reject(
        &[
            ("lib/st.chz", "COUNT := \"s\"\n"),
            (
                "main.chz",
                "x := 1\nf := fn() -> int: x\nx := \"s\"\nimport COUNT as x from lib.st\nprint(f())\n",
            ),
        ],
        "cannot re-declare module-level binding 'x' with a different type (int -> str)",
    );
    // Same for the whole-module form (`prev` is no longer `Ty::Module` by the second let).
    files_reject(
        &[
            ("lib/st.chz", "VAL := 1\n"),
            (
                "main.chz",
                "st := 1\nf := fn() -> int: st\nst := \"s\"\nimport lib.st\nprint(f())\n",
            ),
        ],
        "cannot re-declare module-level binding 'st' with a different type (int -> str)",
    );
    // Control: the same program whose later import binds a DIFFERENT name always fired — which is what
    // identifies the name collision, not the source order, as the thing that was being suppressed.
    files_reject(
        &[
            ("lib/st.chz", "COUNT := \"s\"\n"),
            (
                "main.chz",
                "x := 1\nf := fn() -> int: x\nx := \"s\"\nimport COUNT as other from lib.st\nprint(f())\n",
            ),
        ],
        "cannot re-declare module-level binding 'x' with a different type (int -> str)",
    );
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
    // The closure reads `x` AFTER `x := 1` has handed the name back to this module (a module-scope
    // `declare` clears `imported_values`), so the read is of the LET's binding and the gate must not
    // fire even though the `import` is source-later. Same two statements as the rejected program,
    // in the other ORDER; the import is an `int`, so the let is no retype.
    files_ok(&[
        ("lib/st.chz", "COUNT := 0\n"),
        (
            "main.chz",
            "x := 1\nf := fn() -> int: x\nimport COUNT as x from lib.st\nprint(f())\n",
        ),
    ]);
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
fn destructuring_module_scope_redeclare_changing_type_rejected() {
    // W7-42r shape (a): the destructuring `let` returns BEFORE both carve-outs, so it re-typed the
    // module slot silently. Measured on the pre-fix release binary: `chezzi check` said "ok: no type
    // errors" and the program printed `s` TWICE — once out of `f()` declared `-> int`, once out of
    // the `z: int = f()` that was also accepted.
    rejects(
        "x := 1\nf := fn() -> int: x\nx, y := (\"s\", 2)\n",
        "cannot re-declare module-level binding 'x' with a different type (int -> str)",
    );
}

#[test]
fn destructuring_module_scope_redeclare_boundaries_ok() {
    // Same-type rebind stays legal, exactly as for the single-name let.
    ok("x := 1\nx, y := (2, 3)\nprint(x)\nprint(y)\n");
    // The one-sided `merge_unknown` refinement carve-out must survive the extraction: `x: List[int] = []` then
    // a destructure binding `[1]` REFINES the slot, it does not retype it.
    ok("x: List[int] = []\nx, y := ([1], 2)\nprint(x)\nprint(y)\n");
    // scope > 1: a top-level `if:` body routes to `add_local`, a fresh shadow that may change type.
    ok("x := 1\nc := true\nif c:\n    x, y := (\"s\", 2)\n    print(x)\n    print(y)\n");
    // fn-local is a fresh slot too.
    ok("fn f():\n    x := 1\n    x, y := (\"s\", 2)\n    print(x)\n    print(y)\n");
    // Only the FRESH name changes type — `x` keeps `int`, so nothing is retyped.
    ok("x := 1\nx, y := (2, \"s\")\nprint(x)\nprint(y)\n");
    // ORDER neighbour: swap the tuple elements and the SAME two statements must now reject.
    rejects("x := 1\nx, y := (\"s\", 2)\n", "int -> str");
    // INSERT-AN-UNUSED-STATEMENT neighbour: an unrelated let between the two changes neither verdict.
    ok("x := 1\nz := 0\nx, y := (2, \"s\")\nprint(x)\nprint(y)\nprint(z)\n");
    rejects("x := 1\nz := 0\nx, y := (\"s\", 2)\n", "int -> str");
}

#[test]
fn destructuring_module_scope_redeclare_inferred_return_no_double_fire() {
    // The `!inferring_ret` gate must come through the extraction: return inference truncates errors
    // and re-walks the body inside one open scope, so the rule must fire exactly once.
    ok("x := 1\nfn g() -> int:\n    return x\nx, y := (2, 3)\nprint(g())\n");
    let errs = check_src("x := 1\nfn g() -> int:\n    return x\nx, y := (\"s\", 2)\nprint(g())\n");
    assert_eq!(
        errs.iter()
            .filter(|e| e.message.contains("module-level binding"))
            .count(),
        1,
        "expected exactly one retype error, got: {errs:?}"
    );
}

#[test]
fn destructuring_over_const_keeps_const_message() {
    // The parser rejects `const` ON a destructuring let, but a destructure could silently UN-const a
    // prior one (`declare` clears the const mark). Measured pre-fix: `X: const int = 1` then
    // `X, y := (2, 3)` was check-clean and printed `2`. The const branch wins exclusively, as for the
    // single-name let — a type-CHANGING tuple, so the exclusivity is meaningful.
    rejects(
        "X: const int = 1\nX, y := (\"s\", 2)\n",
        "cannot re-declare const binding",
    );
    let errs = check_src("X: const int = 1\nX, y := (\"s\", 2)\n");
    assert!(
        !errs.iter().any(|e| e.message.contains("module-level")),
        "a live const must not ALSO report the retype error, got: {errs:?}"
    );
    // The same-type un-const is caught too (the const branch fires at ANY scope depth).
    rejects(
        "X: const int = 1\nX, y := (2, 3)\n",
        "cannot re-declare const binding",
    );
    rejects(
        "fn f():\n    X: const int = 1\n    X, y := (2, 3)\n    print(X)\n",
        "cannot re-declare const binding",
    );
}

#[test]
fn destructuring_repeated_name_is_judged_on_the_element_that_lands() {
    // A name repeated inside ONE destructure is one slot written twice with no code in between, so
    // only the LAST element can ever be read — judging the earlier element fired on a transient the
    // program cannot observe (CPython, measured: `x, x = (1, "s")` prints `s`; `x = "a"` then
    // `x, x = (1, "b")` prints `b`). Both are sound and must stay legal.
    ok("x, x := (1, \"s\")\nprint(x)\n");
    ok("x := \"a\"\nf := fn() -> str: x\nx, x := (1, \"b\")\nprint(f())\n");
    // ORDER neighbour — swap which element lands and the SAME shape must reject, because now a
    // closure declared `-> int` really does hand out the `str` the slot ends up holding.
    rejects(
        "x := 1\nf := fn() -> int: x\nx, x := (2, \"s\")\n",
        "int -> str",
    );
    // …and a first-occurrence-only cut would pass the two `ok`s above while missing exactly that.
    // The const branch is deduped the same way: one report, not one per occurrence.
    let errs = check_src("X: const int = 1\nX, X := (2, 3)\n");
    assert_eq!(
        errs.len(),
        1,
        "a repeated const name must report once, got: {errs:?}"
    );
}

#[test]
fn destructuring_error_arms_do_not_double_report() {
    // The other three `check_destructure` arms declare `Unknown` to suppress a cascade; the helper is
    // called from the Tuple SUCCESS arm only, so an already-errored destructure reports once.
    // TICKET-186: a module-scope const redeclaration is decided once per slot, before the walk, so
    // it is reported next to the destructure's own error; nothing else cascades.
    for src in [
        "X: const int = 1\nX, y := (1, 2, 3)\n",
        "X: const int = 1\nX, y := 5\n",
    ] {
        let errs = check_src(src);
        let consts = errs
            .iter()
            .filter(|e| e.message.contains("cannot re-declare const binding 'X'"))
            .count();
        assert!(
            errs.len() == 2 && consts == 1,
            "an errored destructure over a const must report its own error and the const once, got: {errs:?}"
        );
    }
}

#[test]
fn from_import_hand_back_at_a_different_type_rejected_deliberately() {
    // The blessed hand-back idiom (`import COUNT from lib` / `COUNT := COUNT + 1`) keeps working, but
    // ONLY at the same type — handing the name back at a DIFFERENT type is rejected, deliberately, and
    // there is no annotation escape (`COUNT: int = 0` has the same `declared`); the escape is a rename.
    // It has to be: the import's binding IS the module slot's first declaration, so everything typed
    // against the imported `str` — including a `fn` that writes it — is still typed that way. Both
    // pre-existing hand-back fixtures are `int -> int`, so the suite would otherwise never sample this.
    files_reject(
        &[
            ("lib/st.chz", "COUNT := \"s\"\n"),
            (
                "main.chz",
                "import COUNT from lib.st\nCOUNT := 0\nCOUNT = COUNT + 1\nprint(COUNT)\n",
            ),
        ],
        "cannot re-declare module-level binding 'COUNT' with a different type (str -> int)",
    );
    // The same-type hand-back — the idiom the rule must not break — stays legal.
    files_ok(&[
        ("lib/st.chz", "COUNT := 0\n"),
        (
            "main.chz",
            "import COUNT from lib.st\nCOUNT := COUNT + 1\nCOUNT = COUNT + 1\nprint(COUNT)\n",
        ),
    ]);
}

#[test]
fn module_scope_redeclare_to_a_subtype_rejected_deliberately() {
    // DELIBERATE, not an oversight: these three narrow the slot's type rather than widening it, and a
    // `fn() -> Any` handing back a `str` is no lie. They are still rejected because the check is on the
    // slot, not on the readers — the type is frozen at the FIRST declaration, so an earlier WRITER
    // typed against the old type can put a non-conforming value in. Measured pre-fix, check-clean and
    // dead at runtime: `v: Any = 1` / `fn setv(): v = 42` / `v := "s"` / `y: str = v` / `y.upper()`
    // → "type int has no method 'upper'". Loosening the test to `assignable` would re-admit exactly
    // that, so `prev != declared` stands and this family is collateral we accept and document
    // (`docs/syntax.md`); the escape is to re-annotate (`v: Any = "s"`), which stays legal.
    rejects(
        "v: Any = 1\nf := fn() -> Any: v\nv := \"s\"\n",
        "Any -> str",
    );
    rejects(
        "protocol Shape:\n    fn area(self) -> int\n\nstruct Circle:\n    r: int\n    fn area(self) -> int:\n        return self.r\n\ns: Shape = Circle(1)\ns := Circle(2)\n",
        "Shape -> Circle",
    );
    rejects(
        "xs: List[Any] = [1]\nxs := [\"a\"]\n",
        "List[Any] -> List[str]",
    );
    // The escape the diagnostic names, on the same programs: re-annotating declares the slot afresh
    // at the type it already has, so it is accepted.
    ok("v: Any = 1\nf := fn() -> Any: v\nv: Any = \"s\"\nprint(f())\n");
    ok(
        "protocol Shape:\n    fn area(self) -> int\n\nstruct Circle:\n    r: int\n    fn area(self) -> int:\n        return self.r\n\ns: Shape = Circle(1)\ns: Shape = Circle(2)\nprint(s.area())\n",
    );
}

#[test]
fn module_scope_redeclare_of_a_top_level_fn_rejected() {
    // W7-42r shape (d). A top-level `fn` and a module global are the SAME storage slot —
    // `collect_globals` (`compiler/mod.rs:1015`) feeds imports, then fns (`:1050-1053`), then lets
    // (`:1080-1086`) through ONE idempotent-by-name `add` — but the checker keeps fns in `functions`,
    // a namespace disjoint from `scopes`, so `scopes[0].get(name)` was `None` and the rule
    // short-circuited. Measured on the pre-change release binary: `chezzi check` printed "ok: no type
    // errors" and the program died with "runtime error (line 3, col 19): 'int' is not callable".
    rejects(
        "fn helper() -> int:\n    return 7\nf := fn() -> int: helper()\nhelper := 3\nprint(f())\n",
        "cannot re-declare module-level binding 'helper'",
    );
    // (d′) — the SAME statements with the `fn` moved BELOW the let, and the reason this rule is
    // SYMMETRIC with NO source-position test (unlike the import half above). A top-level fn's slot is
    // defined before ANY statement runs: `compiler/mod.rs:1404` — "top-level `fn`s are hoisted as
    // globals before the body" — emits `Op::MakeFunc` + `Op::DefineGlobalSlot` for the whole statement
    // list at `:1415-1436` first, and `desugar/mod.rs:689` says it outright: "declaration position is
    // irrelevant". A "the fn is source-earlier than the let" gate would let this exact program
    // through. Measured pre-change: check-clean, then "runtime error (line 1, col 19): 'int' is not
    // callable".
    rejects(
        "f := fn() -> int: helper()\nhelper := 3\nfn helper() -> int:\n    return 7\nprint(f())\n",
        "cannot re-declare module-level binding 'helper'",
    );
}

#[test]
fn module_scope_redeclare_of_a_from_imported_fn_rejected() {
    // W7-42r shape (c): a from-imported fn lands in `functions` too (`setup.rs:1322`) and takes the
    // same one global slot, so it is the same defect through the import path. Measured on the
    // pre-change binary: "ok: no type errors", then "runtime error (line 2, col 19): 'int' is not
    // callable". Note there is no import-span gate on this arm — a from-imported FN is not in
    // `imported_values` (only a from-imported VALUE is, `setup.rs:1351`), and it must not be: the fn's
    // slot, like a same-module fn's, is filled before any statement runs.
    files_reject(
        &[
            ("lib/st.chz", "fn g() -> str:\n    return \"m\"\n"),
            (
                "main.chz",
                "import g from lib.st\nf := fn() -> str: g()\ng := 1\nprint(f())\n",
            ),
        ],
        "cannot re-declare module-level binding 'g'",
    );
}

#[test]
fn module_scope_redeclare_of_a_fn_at_a_stricter_arity_rejected() {
    // `FnLabels`'s `PartialEq` is deliberately always-`true` (`ty.rs:178-182`), so two `Ty::Func`s that
    // differ only in OPTIONAL ARITY are equal and `prev != declared` cannot see this: the caller's
    // prologue was compiled to omit `helper`'s trailing default and now hands that DELETED fn's
    // default to a closure that never declared one. Measured on the pre-change binary: "ok: no type
    // errors", and the program printed `154` — i.e. `77 * 2`, the dead default times the new body.
    rejects(
        "fn helper(a: int = 77) -> int:\n    return a\nf := fn() -> int: helper()\nhelper := fn(a: int) -> int: a * 2\nprint(f())\n",
        "cannot re-declare module-level binding 'helper'",
    );
    // DIRECTIONAL, not symmetric: going LOOSER (the new binding accepts every call the old one
    // promised) keeps every compiled call site valid and must stay legal. Measured pre-change AND
    // post-change: prints `2`. A symmetric arity test would reject this sound program.
    ok(
        "fn helper(a: int) -> int:\n    return a\nfn looser(a: int = 5) -> int:\n    return a\nhelper := looser\nprint(helper(2))\n",
    );
}

#[test]
fn module_scope_redeclare_of_a_fn_boundaries_ok() {
    // THE load-bearing neighbour: a same-type re-bind of a top-level fn is Python's late binding
    // through the one slot and stays legal — in BOTH orders, because the rule is symmetric (measured
    // pre-change and post-change: both print `9`).
    ok("fn helper() -> int:\n    return 7\nhelper := fn() -> int: 9\nprint(helper())\n");
    ok("helper := fn() -> int: 9\nfn helper() -> int:\n    return 7\nprint(helper())\n");
    // INSERT-AN-UNUSED-STATEMENT neighbour: a `z := 0` between them changes no verdict.
    ok(
        "fn helper() -> int:\n    return 7\nz := 0\nhelper := fn() -> int: 9\nprint(helper())\nprint(z)\n",
    );
    // scope > 1 — a fn-local `helper := 3` is `add_local`'s fresh slot, not the global one.
    ok("fn helper() -> int:\n    return 7\nfn f():\n    helper := 3\n    print(helper)\n");
    // …and so is a top-level `if:` body.
    ok("fn helper() -> int:\n    return 7\nc := true\nif c:\n    helper := 3\n    print(helper)\n");
    // A WHOLE-module import injects no bare fn name (`functions` is cleared per module at
    // `setup.rs:932`), so `helper := 3` in the importer is a first declaration. This is the test a
    // span map keyed on "where the fn was declared" would have failed.
    files_ok(&[
        ("lib/st.chz", "fn helper() -> int:\n    return 7\n"),
        ("main.chz", "import lib.st\nhelper := 3\nprint(helper)\n"),
    ]);
    // The blessed same-type from-import hand-back still passes — the `functions` fallback must not
    // disturb the VALUE path (a from-imported value is in `scopes[0]`, which still wins).
    files_ok(&[
        ("lib/st.chz", "COUNT := 0\n"),
        (
            "main.chz",
            "import COUNT from lib.st\nCOUNT := COUNT + 1\nprint(COUNT)\n",
        ),
    ]);
}

#[test]
fn module_scope_redeclare_of_a_fn_is_judged_on_the_slot_not_the_readers() {
    // A top-level fn's slot is frozen at the fn's type, like a let's: a let of the name at another
    // type is rejected wherever the readers sit (Go: `f redeclared in this block`; Rust: E0428).
    // The rule once fired only when a reader sat above the let, which left `import f from lib` +
    // `f := 1` accepted while `import COUNT from lib` + `COUNT := "s"` was rejected.
    for src in [
        // every reader follows the let
        "fn f(a: int, b: int = 2) -> int:\n    return a + b\nf := fn(a: int) -> int: a * 100\nprint(\"{f(1)}\")\n",
        // a reader above the let
        "fn f(a: int, b: int = 2) -> int:\n    return a + b\ng := fn() -> int: f(1)\nf := fn(a: int) -> int: a * 100\nprint(g())\n",
        // a reader in a fn body declared below the let
        "fn f(a: int, b: int = 2) -> int:\n    return a + b\nf := fn(a: int) -> int: a * 100\nfn g() -> int:\n    return f(1)\nprint(g())\n",
        // no reader at all, and the fn below the let
        "helper := 3\nfn helper() -> int:\n    return 7\nprint(helper)\n",
    ] {
        rejects_desugared(src, "cannot re-declare module-level binding");
    }
    // The same-type re-bind stays legal in both orders.
    ok("fn helper() -> int:\n    return 7\nhelper := fn() -> int: 9\nprint(helper())\n");
    ok("helper := fn() -> int: 9\nfn helper() -> int:\n    return 7\nprint(helper())\n");
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
// loop but placed inside the block is illegal at runtime. The checker mirrors the
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
