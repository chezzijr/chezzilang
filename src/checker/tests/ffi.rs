//! Checker tests: or-patterns, extern / C-ABI FFI.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== or-patterns + nested nullary =====

#[test]
fn or_pattern_mismatched_bindings_rejected() {
    // `Some(a) | None` — one alternative binds `a`, the other binds nothing.
    rejects(
        "o := ?5\nmatch o:\n    ?a | None: print(\"x\")\n",
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
fn bool_or_pattern_closes_the_domain() {
    // `true | false` closes the bool domain by the SAME mechanism as two separate arms: the
    // or-pattern arm threads `covered` through every alternative (src/checker/pattern.rs:348), so
    // both `lit:btrue` and `lit:bfalse` land in `covered` here. rustc accepts
    // `match b { true | false => .. }`, so this is not a special case, just the same rule applied
    // to an or-pattern's alternatives.
    ok("b := true\nmatch b:\n    true | false: print(\"b\")\n");
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
    // `?None` where the inner type is `int` — `None` is the variant pattern, and int has none.
    rejects(
        "o := ?5\nmatch o:\n    ?None: print(0)\n    _: print(1)\n",
        "variant pattern 'None' cannot match a value of type int",
    );
}

#[test]
fn non_nullary_variant_without_payload_rejected() {
    // A nested non-nullary variant used without its payload — `E.A` requires `E.A(...)`.
    rejects(
        "enum E:\n    A(int)\n    B\noo: E? = ?E.B\nmatch oo:\n    ?E.A: print(0)\n    _: print(1)\n",
        "variant 'A' binds 1 value(s), but 0 given",
    );
}

#[test]
fn nested_nullary_correct_ok() {
    // `Some(None)` over `Option[Option[int]]` — the inner `None` is a nullary variant of the
    // inner Option type, a refutable variant match. (One `Some` arm + `_` to be exhaustive.)
    ok("oo: int?? = ?None\nmatch oo:\n    ?None: print(0)\n    _: print(-1)\n");
}

// ===== extern / C-ABI FFI =====

#[test]
fn extern_call_typechecks() {
    // An extern fn's signature is hoisted; calling it type-checks like any named fn.
    ok("extern \"libm.so.6\":\n    fn cos(x: float) -> float\n\ny: float = cos(0.0)\nprint(y)\n");
}

#[test]
fn d3_extern_call_int_into_float_param_rejected() {
    // D3 (matches std.math): `cos(2)` is rejected at check time and names `write 1.0`; `cos(2.0)`
    // checks. The FFI host's `arg_float` runtime promotion stays as defence-in-depth.
    rejects(
        "extern \"libm.so.6\":\n    fn cos(x: float) -> float\n\nprint(cos(2))\n",
        "write 1.0",
    );
    ok("extern \"libm.so.6\":\n    fn cos(x: float) -> float\n\nprint(cos(2.0))\n");
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
    entry_rejects(
        "import int32 from std.ffi\ntype W = int32\nprint(1)\n",
        "describes the C boundary only",
    );
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
fn extern_callback_void_return_accepted() {
    // TICKET-061 W10-7: a callback param's return is `nil` (void) — the commonest C callback shape
    // (foreach/twalk-style). Must be accepted like the top-level extern-fn return-slot `allow_void`
    // case; the checker currently rejects a `nil` callback RETURN even though it accepts `nil` as
    // the extern fn's own return type.
    ok("extern \"libt.so\":\n    fn each(n: int, f: fn(int) -> None)\n");
}

#[test]
fn extern_callback_nil_param_still_rejected() {
    // The widening is on the callback's RETURN slot only — a `nil` callback PARAM has no `CType`
    // lowering and must stay rejected.
    rejects(
        "extern \"libt.so\":\n    fn each(n: int, f: fn(None) -> int)\n",
        "'None' is not a value type",
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
        "extern \"libc.so.6\":\n    fn getenv(s: str) -> str?\n\nmatch getenv(\"PATH\"):\n    ?v: print(v)\n    None: print(\"unset\")\n",
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
    // The prebuilt PROTOCOL names may be used as bounds (`[T: Comparable]`) but must NOT be
    // redeclared as a struct/enum/type alias — left ungated such a decl silently shadowed
    // the protocol and produced a self-contradictory diagnostic ("type Comparable does not satisfy
    // Comparable"). Mirrors the reserved-TYPE-name reservation (Result/Channel/...). The DECL of the
    // name is reserved; the protocol BOUND of the same name stays legal (see the boundary test).
    // Driven from `RESERVED_PROTOCOLS` rather than a hand-copied subset, so a 22nd protocol is covered
    // here the day it is declared.
    for &name in crate::checker::RESERVED_PROTOCOLS {
        for src in [
            format!("struct {name}:\n    x: int\nfn main():\n    print(1)\nmain()\n"),
            format!("enum {name}:\n    A\nfn main():\n    print(1)\nmain()\n"),
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
        "fn takes(c: Convert[int]?):\n    pass\nfn main():\n    pass\nmain()\n",
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
    // M24 INVERSION. This test used to encode the pre-M24 contract: `T.<static>()` on an in-scope
    // generic type parameter was ALWAYS a clear error, because generics are erased and there was no
    // concrete type to dispatch to. M24 (static-witness passing) makes exactly one shape legal — a
    // bound whose protocol DECLARES that static method, called directly in the body of a
    // module-level generic fn — so the first case below flipped from `_rejects` to `_ok`. The error
    // survives, narrowed: it now fires when NO bound declares the static method, which is still an
    // erased call with nothing to dispatch to. The `Box[int].empty()` keep-working case is
    // unchanged (a concrete turbofish type was never the erased case).
    // NOW OK — `T.convert(x)` under `[T: Convert[int]]`, the Convert/From driver.
    entry_ok(
        "struct Port:\n    n: int\n    fn convert(x: int) -> Port:\n        return Port(n=x)\nfn build[T: Convert[int]](x: int) -> T:\n    return T.convert(x)\nfn main():\n    print(build[Port](5).n)\nmain()\n",
    );
    // STILL AN ERROR — `mk[T]` has NO bound at all, so nothing declares a static `empty`.
    entry_rejects(
        "struct Box[T]:\n    items: List[T]\n    fn empty() -> Box[T]:\n        return Box(items=[])\nfn mk[T]() -> Box[T]:\n    return T.empty()\nfn main():\n    print(1)\nmain()\n",
        "no bound on 'T' declares a static method 'empty'",
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
        "struct P:\n    v: int\n    fn compare(self, other: P) -> int:\n        return self.v - other.v\n    fn eq(self, other: P) -> bool:\n        return self.v == other.v\nfn pick[T: Comparable](a: T) -> T:\n    return a\nfn main():\n    print(pick(P(v=5)).compare(P(v=3)))\nmain()\n",
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
        "import int32 from std.ffi\n\nstruct DivT:\n    quot: int32\n    rem: int32\n",
    );
    let entry = t.write(
        "main.chz",
        "import cdefs\nimport int32 from std.ffi\n\nextern \"libc.so.6\":\n    \
         fn div(numer: int32, denom: int32) -> cdefs.DivT\n",
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
        "extern \"lib\":\n    fn g(s: str) -> owned_str?\n\nmatch g(\"x\"):\n    ?v: print(v)\n    None: print(\"none\")\n",
    );
}

#[test]
fn extern_fixed_width_int_param_and_return_ok() {
    // Each fixed-width int marshalling name (int8..uint64) resolves to `Ty::Width` (a tag on `int`) and
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
        "returns no value (None)",
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
        "returns no value (None)",
    );
}

#[test]
fn extern_fixed_width_int_via_alias_ok() {
    // A transparent `type Len = int32` used in an extern sig must behave identically to bare `int32`
    // (the alias trap from the prior FFI task): resolve_type maps Len -> int32 -> Ty::Int (program
    // sees a plain int), and the backends' ctype_of resolves the alias one hop to the int32 leaf. The
    // alias's target `int32` is resolved in THIS module, so it only works because int32 is imported
    // here — alias coherence (hazard a): a width alias resolves iff the width name is visible.
    entry_rejects(
        "import int32 from std.ffi\ntype Len = int32\nextern \"libc.so.6\":\n    fn f(x: Len) -> Len\n\nn: int = f(5)\nprint(n)\n",
        "describes the C boundary only",
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
    // both `import int32 as W` (name unusable) and `import int8 as int32` (silently wrong width). The
    // second alias target, `int32`, is a reserved builtin type name, so the reserved-name rule (one
    // set at every declaration site) rejects it first.
    entry_rejects(
        "import int32 as W from std.ffi\nfn main():\n    x: W = 5\n    print(x)\n",
        "cannot be renamed on import",
    );
    entry_rejects(
        "import int8 as int32 from std.ffi\nfn main():\n    x: int32 = 5\n    print(x)\n",
        "reserved (builtin)",
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
    entry_rejects(
        "import int32 from std.ffi\ntype Len = int32\n\
         extern \"libc.so.6\":\n    fn f(x: Len) -> Len\n\
         \nfn main():\n    n: int = f(5)\n    print(n)\n",
        "describes the C boundary only",
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
    entry_rejects(
        "import int32 from std.ffi\ntype Len = int32\n\
         extern \"libc.so.6\":\n    fn f(x: Len) -> Len\n\
         \nfn main():\n    n: int = f(5)\n    print(n)\n",
        "describes the C boundary only",
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
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(
        errs.iter()
            .any(|e| e.message.contains("describes the C boundary only")),
        "a composite alias body carrying a width must be rejected at its declaration, got: {errs:?}"
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
    entry_ok(
        "import int32 as int32 from std.ffi\nstruct P:\n    x: int32\nfn main():\n    print(P(5).x)\n",
    );
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
    // WITHOUT B importing int32 — the field types were resolved to `Ty::Width` during A's checking, so B
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
        "extern \"libc.so.6\":\n    fn List(x: int) -> int\n",
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
    for v in ["Ok", "Err", "Some"] {
        rejects(
            &format!("extern \"libm.so.6\":\n    fn {v}(x: float) -> float\n"),
            "builtin/reserved name",
        );
    }
    // `None` is a keyword (TICKET-237): the parser rejects the name before the checker sees it.
    parse_rejects(
        "extern \"libm.so.6\":\n    fn None(x: float) -> float\n",
        "reserved keyword",
    );
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
        "extern \"libc.so.6\":\n    fn f(x: None) -> int\n",
        "'None' is not a value type",
    );
}
