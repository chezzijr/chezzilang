//! VM tests: golden parity programs.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

// ----- golden parity -----

#[test]
fn golden_hello_chz_matches_expected() {
    let expected = include_str!("../../../examples/hello.expected");
    assert_eq!(run(include_str!("../../../examples/hello.chz")), expected);
}

/// `pass` no-op-statement golden: a lone-`pass` fn body (== a `return`-only body: runs, falls off
/// the end → nil) plus `pass` as a no-op inside `if`/`for`/`while`. `pass` compiles to no bytecode,
/// so this is byte-identical against the golden.
#[test]
fn golden_pass_noop_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/pass_noop.chz");
    let expected = include_str!("../../../examples/pass_noop.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from pass_noop.expected"
    );
}

/// Empty-protocol golden: `protocol Foo:\n    pass` is an accept-all top type (structural over
/// zero methods ⇒ every type satisfies it), byte-identical to the reserved `Any` — accepts
/// int/str/bool/struct params and types a heterogeneous `List[Foo]`. Erased at runtime, so VM ==
/// interp.
#[test]
fn golden_empty_protocol_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/empty_protocol.chz");
    let expected = include_str!("../../../examples/empty_protocol.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from empty_protocol.expected"
    );
}

/// Empty-struct golden: `struct S:\n    pass` has zero fields — `S()` constructs, prints `S()`,
/// two `S()` compare equal, a distinct empty struct `T()` is not equal, and `S` is usable as a Set
/// element / Map key (zero-field structs are intrinsically Hashable, returning the
/// same constant hash). Byte-identical against the golden.
#[test]
fn golden_empty_struct_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/empty_struct.chz");
    let expected = include_str!("../../../examples/empty_struct.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from empty_struct.expected"
    );
}

/// Container-constructor golden: `examples/container_ctor.chz` exercises `List[T]()` / `Map[K,V]()`
/// / `Set[T]()` turbofish + bare 0-arg `List()`/`Map()`. Type args erase at runtime (an empty
/// `List[int]()` is just an empty list), so this is byte-identical against the golden — the
/// gate for the constructor-turbofish feature.
#[test]
fn golden_container_ctor_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/container_ctor.chz");
    let expected = include_str!("../../../examples/container_ctor.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from container_ctor.expected"
    );
}

/// First-class native-type golden: `examples/native_qualified.chz` exercises the ADDITIVE
/// qualified / aliased module-member path for import-gated native types — `concurrency.Shared(0)`,
/// aliased `c.Shared(0)`, qualified RwShared/Atomic/Executor, `time.timer(0)`, plus a type-alias
/// and a struct over a qualified `concurrency.Shared`. These lower to the SAME opcodes as the
/// bare-after-import names, so output is byte-identical — the gate for the qualified-ctor lowering.
#[test]
fn golden_native_qualified_chz_matches_expected_and_interp() {
    // Imports (`std.concurrency` / `std.time`) require the module-graph path, so drive `run_file`
    // (not `run_capture`, which skips resolution and never populates `imported_modules`). The
    // output must agree byte-for-byte with `.expected`.
    let base = crate::native::std_embed::checkout_root();
    let path = base.join("examples/native_qualified.chz");
    let expected =
        std::fs::read_to_string(base.join("examples/native_qualified.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("native_qualified.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from native_qualified.expected"
    );
    let (_ip_out, _e2, ip_res, _) = run_file(&path);
    ip_res.expect("native_qualified.chz should run on the interp");
    let (mn_out, _e3, mn_res, _) = run_file_with(&path, crate::native::HostConfig::default());
    mn_res.expect("native_qualified.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on native_qualified");
}

/// `where`-clause generic bounds golden: `examples/where_sort_sum.chz` exercises the file-backed
/// List `sort` (`native fn sort(self) -> nil where T: Comparable`) on int/float/struct-with-
/// `compare` lists and `sum` (`where T: Add`) on int/float lists. A `where` clause lowers to
/// NOTHING at runtime (checker-only), so output must agree byte-for-byte with `.expected`.
#[test]
fn golden_conditional_method_chz_matches_expected_and_interp() {
    let base = crate::native::std_embed::checkout_root();
    let path = base.join("examples/conditional_method.chz");
    let expected =
        std::fs::read_to_string(base.join("examples/conditional_method.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("conditional_method.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from conditional_method.expected"
    );
    let (_ip_out, _e2, ip_res, _) = run_file(&path);
    ip_res.expect("conditional_method.chz should run on the interp");
    let (mn_out, _e3, mn_res, _) = run_file_with(&path, crate::native::HostConfig::default());
    mn_res.expect("conditional_method.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on conditional_method");
}

#[test]
fn golden_where_sort_sum_chz_matches_expected_and_interp() {
    let base = crate::native::std_embed::checkout_root();
    let path = base.join("examples/where_sort_sum.chz");
    let expected = std::fs::read_to_string(base.join("examples/where_sort_sum.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("where_sort_sum.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from where_sort_sum.expected"
    );
    let (_ip_out, _e2, ip_res, _) = run_file(&path);
    ip_res.expect("where_sort_sum.chz should run on the interp");
    let (mn_out, _e3, mn_res, _) = run_file_with(&path, crate::native::HostConfig::default());
    mn_res.expect("where_sort_sum.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on where_sort_sum");
}

/// Variadic-parameter + `Any`-top-type golden: `examples/variadic.chz` exercises a variadic user
/// fn (`...xs: int`), a zero-arg variadic call, a pre-variadic positional + keyword-only-default
/// combination, and an `Any` parameter slot. The variadic collapse happens in the desugar pass (a
/// synthesized `List` literal), so the call is an ordinary positional call — output
/// must be byte-identical against the `.expected` golden.
#[test]
fn golden_variadic_chz_matches_expected_and_interp() {
    let base = crate::native::std_embed::checkout_root();
    let path = base.join("examples/variadic.chz");
    let expected = std::fs::read_to_string(base.join("examples/variadic.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("variadic.chz should run on the VM");
    assert_eq!(vm_out, expected, "vm output drifted from variadic.expected");
    let (_ip_out, _e2, ip_res, _) = run_file(&path);
    ip_res.expect("variadic.chz should run on the interp");
    let (mn_out, _e3, mn_res, _) = run_file_with(&path, crate::native::HostConfig::default());
    mn_res.expect("variadic.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on variadic");
}

/// Phase-4d file-backed native-module golden: `examples/std_native_4d.chz` exercises the five
/// migrated pure-function modules (`std.math`/`io`/`os`/`rand`/`fs`) whose signatures now come from
/// real `std/<M>.chz` files (bodyless `native fn` decls) instead of a hand-built `native_module_sig`
/// arm. Dispatch is UNCHANGED (name-keyed `native_members`), so output must be byte-identical
/// against `.expected`. `rand` is seeded and `getcwd` is
/// matched (not printed), so the sequence is deterministic — but ONLY while serialized against the
/// other rand tests: `std_native_4d.chz` does `rand.seed(1)` then `rand.int(0,100)` as two separate
/// native calls on the shared process-global RNG, so the seed→draw sequence must hold
/// `TEST_RNG_LOCK` across the whole run (like `golden_rand_via_run_file`) or the parallel harness
/// interleaves a sibling test's reseed between them and the draw drifts off `65`.
#[test]
fn golden_std_native_4d_chz_matches_expected_and_interp() {
    // Serialize against the rand unit tests / rand goldens (shared process-global RNG); see
    // TEST_RNG_LOCK. Hold it across the whole seed→draw run so the parallel harness cannot
    // interleave another test's reseed between this program's rand.seed(1) and rand.int(0,100).
    let _g = crate::native::rand::TEST_RNG_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let base = crate::native::std_embed::checkout_root();
    let path = base.join("examples/std_native_4d.chz");
    let expected = std::fs::read_to_string(base.join("examples/std_native_4d.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("std_native_4d.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from std_native_4d.expected"
    );
    let (_ip_out, _e2, ip_res, _) = run_file(&path);
    ip_res.expect("std_native_4d.chz should run on the interp");
    let (mn_out, _e3, mn_res, _) = run_file_with(&path, crate::native::HostConfig::default());
    mn_res.expect("std_native_4d.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on std_native_4d");
}

/// Phase-4c file-backed native-module golden: `examples/std_native_4c.chz` exercises the migrated
/// std.ffi C-buffer surface (`alloc`/`alloc_zeroed`/`store_int64_at`/`load_int64_at`/
/// `store_int32_at`/`load_int32_at`/`is_null`/`null`/`free`) whose 59 signatures now come from the
/// real `std/ffi.chz` (bodyless `native fn` decls) instead of a hand-built `native_module_sig` arm.
/// Dispatch is UNCHANGED (name-keyed `native_members("std.ffi")`), so output must be byte-identical
/// against `.expected`. FFI is layout-dependent
/// UB, so this drives a REAL alloc/store/load round-trip (never printing a nondeterministic pointer
/// address, only the round-tripped payload) — a stronger guard than a pure-checker test.
#[test]
fn golden_std_native_4c_chz_matches_expected_and_interp() {
    let base = crate::native::std_embed::checkout_root();
    let path = base.join("examples/std_native_4c.chz");
    let expected = std::fs::read_to_string(base.join("examples/std_native_4c.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("std_native_4c.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from std_native_4c.expected"
    );
    let (_ip_out, _e2, ip_res, _) = run_file(&path);
    ip_res.expect("std_native_4c.chz should run on the interp");
    let (mn_out, _e3, mn_res, _) = run_file_with(&path, crate::native::HostConfig::default());
    mn_res.expect("std_native_4c.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on std_native_4c");
}

/// Phase-5b file-backed native-enum golden: `examples/native_enum_smoke.chz` exercises the reserved
/// `Option`/`Result` surface whose variant SHAPE is now declared in `std/prelude.chz` as
/// `native enum Option[T]` / `native enum Result[T, E]` — Some/None/Ok/Err CONSTRUCTION, `?` on a
/// Result-returning AND an Option-returning fn, and exhaustive `match`. The port is SHAPE-ONLY (the
/// `?`/match/construction wiring stays Rust-inline), so output must be byte-identical
/// against `.expected`. This is the phase-5b behavior-
/// preservation gate — any drift means the file-backed shape decoupled from the Rust wiring.
#[test]
fn golden_native_enum_smoke_chz_matches_expected_and_interp() {
    let base = crate::native::std_embed::checkout_root();
    let path = base.join("examples/native_enum_smoke.chz");
    let expected =
        std::fs::read_to_string(base.join("examples/native_enum_smoke.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("native_enum_smoke.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from native_enum_smoke.expected"
    );
    let (_ip_out, _e2, ip_res, _) = run_file(&path);
    ip_res.expect("native_enum_smoke.chz should run on the interp");
    let (mn_out, _e3, mn_res, _) = run_file_with(&path, crate::native::HostConfig::default());
    mn_res.expect("native_enum_smoke.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on native_enum_smoke");
}

/// Qualified-type-as-static-method-receiver golden: `examples/qualified_static/main.chz` imports a
/// sibling module and calls `counter.Counter.zero()` / `counter.Counter.of(42)` (struct statics)
/// and `counter.Color.first()` (enum static) through a QUALIFIED type. These lower to the SAME
/// `Op::CallStatic` the bare `Counter.zero()` form emits, so output is byte-identical
/// against `.expected` — the gate for the qualified-static
/// lowering. Multi-file (sibling import) needs the module-graph path, so drive `run_file`.
#[test]
fn golden_qualified_static_chz_matches_expected_and_interp() {
    let base = crate::native::std_embed::checkout_root();
    let path = base.join("examples/qualified_static/main.chz");
    let expected =
        std::fs::read_to_string(base.join("examples/qualified_static/main.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("qualified_static/main.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from qualified_static/main.expected"
    );
    let (_ip_out, _e2, ip_res, _) = run_file(&path);
    ip_res.expect("qualified_static/main.chz should run on the interp");
    let (mn_out, _e3, mn_res, _) = run_file_with(&path, crate::native::HostConfig::default());
    mn_res.expect("qualified_static/main.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on qualified_static");
}

/// Qualified-FFI-width golden: `examples/ffi_qualified.chz` declares `extern fn abs(ffi.int32) ->
/// ffi.int32` — a QUALIFIED width name in an extern signature. `resolve_ctype_d` maps it to the
/// SAME `CType::Int32` the bare `int32` resolves to, so the C ABI marshalling is identical. Runs
/// wherever the `libc`/`libm` aliases resolve, otherwise prints a SKIP line naming the reason;
/// drives `run_file` (extern decls need the module-graph) and asserts byte-identical output. FFI is
/// layout-dependent, hence a real C call rather than a unit assert.
#[test]
fn golden_ffi_qualified_chz_via_run_file() {
    if let Some(why) = crate::native::cffi::platform_c_library_missing() {
        eprintln!("SKIP golden_ffi_qualified_chz_via_run_file: {why}");
        return;
    }
    let base = crate::native::std_embed::checkout_root();
    let path = base.join("examples/ffi_qualified.chz");
    let expected = std::fs::read_to_string(base.join("examples/ffi_qualified.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("ffi_qualified.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from ffi_qualified.expected"
    );
}

/// Inline-expr fn body golden: `examples/inline_fn.chz` exercises Option A (inline-only) — a
/// `fn a(): <expr>` (and the annotated `fn a() -> int: <expr>`) implicitly returns its single
/// expression, usable as a value and a `.map` argument. Byte-identical against the
/// checked-in `.expected`.
#[test]
fn golden_inline_fn_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/inline_fn.chz");
    let expected = include_str!("../../../examples/inline_fn.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from inline_fn.expected"
    );
}

/// Swift-style keyword arguments through a function VALUE golden: `examples/keyword_value.chz`
/// exercises by-label / reordered / mixed positional+label / HOF-param-labels / closure-value /
/// slot-order-eval keyword calls. The checker resolves each to a positional slot permutation and
/// the compiler lowers it to the SAME positional `Op::Call`, so output is byte-identical against
/// the golden — the gate for the feature.
#[test]
fn golden_keyword_value_chz_matches_expected_and_interp() {
    let base = crate::native::std_embed::checkout_root();
    let path = base.join("examples/keyword_value.chz");
    let expected = std::fs::read_to_string(base.join("examples/keyword_value.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("keyword_value.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from keyword_value.expected"
    );
    let (_ip_out, _e2, ip_res, _) = run_file(&path);
    ip_res.expect("keyword_value.chz should run on the interp");
    let (mn_out, _e3, mn_res, _) = run_file_with(&path, crate::native::HostConfig::default());
    mn_res.expect("keyword_value.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on keyword_value");
}

/// Cross-module value+keyword parity: `examples/keyword_value_xmod/main.chz` calls an IMPORTED fn
/// through a value with reordered keyword args. The permutation is keyed by the CALL-SITE module
/// index, so this locks that module-scoped keying (the extern-sig precedent) resolves correctly
/// across module boundaries against a literal golden.
#[test]
fn golden_keyword_value_xmod_matches_expected_and_interp() {
    let base = crate::native::std_embed::checkout_root();
    let path = base.join("examples/keyword_value_xmod/main.chz");
    let expected =
        std::fs::read_to_string(base.join("examples/keyword_value_xmod/main.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("keyword_value_xmod should run on the VM");
    assert_eq!(vm_out, expected, "vm output drifted on keyword_value_xmod");
    let (_ip_out, _e2, ip_res, _) = run_file(&path);
    ip_res.expect("keyword_value_xmod should run on the interp");
    let (mn_out, _e3, mn_res, _) = run_file_with(&path, crate::native::HostConfig::default());
    mn_res.expect("keyword_value_xmod should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on keyword_value_xmod");
}

/// Cross-module DEFAULT golden (W7-51): `examples/default_xmod/main.chz` omits arguments whose
/// defaults are declared in `config.chz` and spell names — a global, a fn, and `std.math` through
/// `config`'s own import — that `main` either does not have or deliberately BINDS DIFFERENTLY
/// (`RETRIES := 99`, `label() -> "main"`). The corpus had zero cross-module default coverage before
/// this, which is exactly why the defect shipped. `b1307258` cannot even build it (`type error
/// (line 15, col 25): unknown name 'math'` — the clone landed in `main`); CPython's two-module
/// equivalent prints the same lines this does.
#[test]
fn golden_default_xmod_matches_expected_on_all_engines() {
    let base = crate::native::std_embed::checkout_root();
    let path = base.join("examples/default_xmod/main.chz");
    let expected =
        std::fs::read_to_string(base.join("examples/default_xmod/main.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("default_xmod should run");
    assert_eq!(vm_out, expected, "VM output drifted on default_xmod");
    let (_mn_out, _e2, mn_res, _) = run_file(&path);
    mn_res.expect("default_xmod should run on the M:N engine");
}

/// The protocol golden — the shape the cross-module default fix exists for, and the one the corpus
/// had no coverage of at all. `shapes` declares the protocol and the consumer; `boxes` imports
/// `shapes` and declares the implementor, so `shapes` cannot import `boxes` without a cycle. The
/// method default spells a name only `boxes` can see (`units.factor()`) AND a name `shapes` also
/// declares with a different value (`unit()`), so a caller-scope resolution would both fail to
/// compile and silently disagree. The checked-in `.expected` is byte-identical to what the
/// equivalent CPython program prints.
#[test]
fn golden_default_protocol_matches_expected_on_all_engines() {
    let base = crate::native::std_embed::checkout_root();
    let path = base.join("examples/default_protocol/main.chz");
    let expected =
        std::fs::read_to_string(base.join("examples/default_protocol/main.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("default_protocol should run");
    assert_eq!(vm_out, expected, "VM output drifted on default_protocol");
    let (_mn_out, _e2, mn_res, _) = run_file(&path);
    mn_res.expect("default_protocol should run on the M:N engine");
}

/// Chained `elif` in expression position: `examples/expr_else_if.chz` exercises a multi-arm
/// `if p: a elif q: b else: c` chain (right-associative nesting) selecting each branch.
/// Byte-identical against the checked-in `.expected`.
#[test]
fn golden_expr_else_if_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/expr_else_if.chz");
    let expected = include_str!("../../../examples/expr_else_if.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from expr_else_if.expected"
    );
}

#[test]
fn golden_hello_chz_matches_interpreter() {
    // Same source + golden as `golden_hello_chz_matches_expected` above — this test's own only
    // assertion used to be a self-comparison; reuse the checked-in `.expected` as the real value.
    let src = include_str!("../../../examples/hello.chz");
    let expected = include_str!("../../../examples/hello.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Golden: a user struct/enum method whose name collides with a built-in method name (`add`,
/// `map`) still gets named/default-arg support when the receiver's struct type is statically
/// known (typed local / inline ctor / struct-returning fn), while a genuine builtin receiver
/// (List) routes to the builtin untouched. Byte-identical on the VM, interp, and `.expected`.
#[test]
fn golden_builtin_named_method_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/builtin_named_method.chz");
    let expected = include_str!("../../../examples/builtin_named_method.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from builtin_named_method.expected"
    );
}

/// M22 golden: operator protocols (`div`/`mod`/`neg`), protocol embedding (super-protocols), and
/// the builtin `Arithmetic` bundle — byte-identical on the VM, interp, the M:N parallel engine,
/// and the checked-in `.expected`.
#[test]
fn golden_arithmetic_protocol_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/arithmetic_protocol.chz");
    let expected = include_str!("../../../examples/arithmetic_protocol.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from arithmetic_protocol.expected"
    );
}

/// Generic operator-overload golden: `examples/generic_operator_overload.chz` — a generic struct
/// `Box[T]` and generic enum `Numeral[T]` whose `add`/`neg`/`compare` methods overload `+`/`-`/`<`,
/// satisfy `Add`/`Comparable`, and flow into `twice[T: Add]`. Byte-identical on the VM, the M:N
/// parallel engine, and the checked-in `.expected`.
#[test]
fn golden_generic_operator_overload_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/generic_operator_overload.chz");
    let expected = include_str!("../../../examples/generic_operator_overload.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from generic_operator_overload.expected"
    );
}

/// M22: a struct defining `div`/`mod`/`neg` overloads `/`, `%`, and unary `-`. Runs byte-identical
/// on the VM and the M:N parallel engine.
#[test]
fn struct_div_mod_neg_runs() {
    let src = "struct V:\n    n: int\n    fn div(self, other: V) -> V:\n        return V(self.n / other.n)\n    fn mod(self, other: V) -> V:\n        return V(self.n % other.n)\n    fn neg(self) -> V:\n        return V(-self.n)\nfn main():\n    a := V(7)\n    b := V(2)\n    print((a / b).n)\n    print((a % b).n)\n    print((-a).n)\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "3\n1\n-7\n");
}

/// Static (associated) methods golden: `examples/static_methods.chz` — a named/alternative struct
/// ctor (`Rect.square`), a validating ctor returning `Result` (`Email.parse`), an enum static
/// ctor returning `Option` (`Color.from_str`) alongside a variant that wins over a static name,
/// and a generic static via the type-level turbofish (`Box[int].empty()`). Static dispatch pushes
/// no receiver, so it is behavior-preserving; byte-identical on the VM, interp, and the M:N
/// parallel engine, plus the checked-in `.expected`.
#[test]
fn golden_static_methods_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/static_methods.chz");
    let expected = include_str!("../../../examples/static_methods.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from static_methods.expected"
    );
}

/// M24 static-witness golden: `examples/static_witness.chz` — a protocol's STATIC (no-`self`)
/// requirement called THROUGH a generic bound (`T.default()` inside `fn reset[T: Default]`), on a
/// struct, an enum and the reserved `Convert[int]`; `T` pinned by turbofish and by an annotated
/// result; witness FORWARDING (`twice`); a MEMBER's own `[T]` (instance + static); and the witness
/// reaching an escaping closure, a nested `fn`, a `defer:` block and a `spawn:` block. The witness
/// is a hidden trailing argument, so nothing is monomorphized and the runtime stays erased —
/// byte-identical on the M:N engine against the checked-in `.expected`.
#[test]
fn golden_static_witness_chz_matches_expected_and_parallel() {
    let src = include_str!("../../../examples/static_witness.chz");
    let expected = include_str!("../../../examples/static_witness.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from static_witness.expected"
    );
}

/// A raw string is an ordinary `str` end-to-end: usable as a match-arm literal pattern
/// (regression — the pattern parser previously rejected it) with its braces kept literal,
/// byte-identical.
#[test]
fn raw_string_as_match_pattern_runs() {
    let src = "fn classify(s: str) -> str:\n    return match s:\n        r\"{}\": \"braces\"\n        _: \"other\"\nfn main():\n    print(classify(r\"{}\"))\n    print(classify(\"x\"))\nmain()\n";
    assert_eq!(run_parity(src), "braces\nother\n");
}

/// Raw-string golden: `examples/raw_string.chz` exercises the verbatim `r"..."` forms —
/// literal braces (`r"{}"` → `{}`, the thing that needed `{{}}` before), literal backslashes
/// (`r"\d+\s"`), the single-quote + Windows-path form, the triple form embedding quotes+braces
/// (JSON), and a side-by-side proving a NORMAL string still interpolates (`"{x}"` → 5) while the
/// raw form keeps `{x}` literal. NO interpolation + NO escapes, so VM and interp emit the same
/// compile-time literal — byte-identical on the VM, interp, and the checked-in `.expected`.
#[test]
fn golden_raw_string_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/raw_string.chz");
    let expected = include_str!("../../../examples/raw_string.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from raw_string.expected"
    );
}

/// M19 memory-layout lever #1 golden: `examples/struct_layout.chz` exercises the positional
/// struct-field storage across every observable surface the layout change touches — field
/// read/write, struct `==` (equal / unequal-by-value / unequal-by-type), Display + `{}`
/// interpolation (names recovered from the type in declaration order), nested structs, a generic
/// `Box[T]` with two monomorphs sharing ONE type-erased layout, and a reordered named-field
/// constructor. Byte-identical against the checked-in `.expected`: the layout change is
/// behavior-preserving, so any divergence turns this RED.
#[test]
fn golden_struct_layout_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/struct_layout.chz");
    let expected = include_str!("../../../examples/struct_layout.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from struct_layout.expected"
    );
}

/// M19 memory-layout lever #1 — assert the VM stores struct fields POSITIONALLY (a flat
/// `Vec<Value>`, hidden-class / `__slots__` layout) with NO per-instance field-name strings.
/// Compiles a program with `struct Point(x, y)`, drives the real `new_struct` construction path
/// (push args, call `new_struct`), then pattern-matches the heap object and verifies the field
/// Vec is `[Value::int(1), Value::int(2)]` in declaration order — names live only in the
/// StructDef. This is the type-level guard the layout change is built around (the destructure of
/// `fields` as `&Vec<Value>` would not compile against the old `Vec<(Box<str>,Value)>`).
#[test]
fn struct_positional_layout_no_per_instance_names() {
    let tokens = lexer::tokenize(
        "struct Point:\n    x: int\n    y: int\nfn main():\n    print(0)\nmain()\n",
    )
    .expect("lex");
    let module = parser::parse(tokens).expect("parse");
    let program = crate::compiler::compile_module_standalone(&module).expect("compile");
    let mut vm = Vm::new(Arc::new(program));
    let span = Span::RUNTIME;
    vm.push(Value::int(1));
    vm.push(Value::int(2));
    // ROOT REDESIGN — structs are keyed by the qualified IDENTITY KEY (`<main>::Point` standalone).
    vm.new_struct("<main>::Point", 2, span).expect("new_struct");
    let Some(h) = vm.pop().as_obj() else {
        panic!("expected struct obj")
    };
    match vm.heap.get(h) {
        Obj::Struct { tid, fields, .. } => {
            let tid = *tid;
            // positional: NOT Vec<(Box<str>, Value)>
            assert_eq!(
                fields.as_slice(),
                &[Value::int(1), Value::int(2)],
                "fields must be positional in declaration order, no per-instance names"
            );
            // Name is resolved from tid (no per-instance `name` field), not stored on the instance.
            assert_eq!(vm.struct_name_of_tid(tid), "<main>::Point");
        }
        _ => panic!("expected Obj::Struct"),
    }
}

/// M19 memory-layout lever #2 — assert the VM stores an enum's identity as a dense `variant_id:
/// u32` (NO per-instance `ty`/`variant` Box<str>), the enum analogue of struct `tid` (lever #1).
/// Compiles `enum Color: Red, Green, Blue`, drives the real `new_enum` construction path, then
/// pattern-matches the heap object and verifies it carries only `{ variant_id, payload }` — the
/// destructure of `payload` as `&Vec<Value>` with `variant_id` would not compile against the old
/// `{ ty, variant, payload }` shape (the type-level guard). The id is dense and stable per
/// (enum-type, variant) pair.
#[test]
fn enum_variant_id_stamped_at_construction() {
    let tokens = lexer::tokenize(
        "enum Color:\n    Red\n    Green\n    Blue\nfn main():\n    print(0)\nmain()\n",
    )
    .expect("lex");
    let module = parser::parse(tokens).expect("parse");
    let program = crate::compiler::compile_module_standalone(&module).expect("compile");
    // `Green`'s dense id from the program table (resolved on the cold path). ROOT REDESIGN — the
    // enum is keyed by its qualified IDENTITY KEY (`<main>::Color` on the standalone path).
    let green_id = program
        .variants
        .get(&("<main>::Color".to_string(), "Green".to_string()))
        .expect("Green registered")
        .variant_id;
    let mut vm = Vm::new(Arc::new(program));
    let span = Span::RUNTIME;
    vm.new_enum("Green", green_id, 0, span).expect("new_enum");
    let Some(h) = vm.pop().as_obj() else {
        panic!("expected enum obj")
    };
    match vm.heap.get(h) {
        Obj::Enum {
            variant_id,
            payload,
        } => {
            assert_eq!(
                *variant_id, green_id,
                "variant_id must be the dense id stamped at construction"
            );
            let payload: &Vec<Value> = payload; // no per-instance ty/variant Box<str>
            assert!(payload.is_empty(), "nullary variant has empty payload");
        }
        _ => panic!("expected Obj::Enum"),
    }
}

/// M19 lever #2 — native `Result`/`Option` variants get FIXED low ids assigned first
/// (`Ok`=VID_OK, `Err`=VID_ERR, `Some`=VID_SOME, `None`=VID_NONE_VARIANT) so `?`/error-gating
/// can compare against compile-time constants (JIT-jump-table groundwork). User variants follow.
#[test]
fn native_result_option_have_fixed_variant_ids() {
    use crate::base::op::{VID_ERR, VID_NONE_VARIANT, VID_OK, VID_SOME};
    let tokens = lexer::tokenize("fn main():\n    print(0)\nmain()\n").expect("lex");
    let module = parser::parse(tokens).expect("parse");
    let program = crate::compiler::compile_module_standalone(&module).expect("compile");
    let vid = |e: &str, v: &str| {
        program
            .variants
            .get(&(e.to_string(), v.to_string()))
            .unwrap()
            .variant_id
    };
    assert_eq!(vid("Result", "Ok"), VID_OK);
    assert_eq!(vid("Result", "Err"), VID_ERR);
    assert_eq!(vid("Option", "Some"), VID_SOME);
    assert_eq!(vid("Option", "None"), VID_NONE_VARIANT);
    // A native-built enum (alloc_enum ⇒ Option::Some) must carry the right id.
    let mut vm = Vm::new(Arc::new(program));
    let v = vm.alloc_enum("Option", "Some", vec![Value::int(7)]);
    let Some(h) = v.as_obj() else { panic!() };
    let Obj::Enum { variant_id, .. } = vm.heap.get(h) else {
        panic!("expected enum")
    };
    assert_eq!(
        *variant_id, VID_SOME,
        "native alloc_enum must stamp the fixed Some id"
    );
}

/// M19 lever #2 regression guard — a user enum declaring a variant named `Some` must NOT collapse
/// native Option identity. A native Option::Some (from `pop()`) and a user `Foo::Some` carry
/// DISJOINT ids (native VID_SOME=2 stamped directly by `alloc_enum`; user id at 4..), so `==` is
/// `false` — byte-matching the interp oracle. Before the reserved-id fix the VM printed `true`
/// (user variant shadowed `variants["Some"]`, so native construction stamped the user's id).
#[test]
fn user_variant_shadow_does_not_collapse_native_option_equality() {
    let src = "enum Foo:\n    Some(int)\n    Bar\nfn opt() -> int?:\n    return [5].pop()\nfn main():\n    a := opt()\n    b := Foo.Some(5)\n    print(a == b)\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, "false\n",
        "native Option::Some must not equal user Foo::Some (distinct enums)"
    );
}

/// Scoped variants — two enums may now reuse a variant name (`Color.Red` / `Light.Red`). Each
/// qualified constructor and match arm must dispatch to the right enum's variant (distinct dense
/// `variant_id`s); the interp's `try_bind` enum check must agree with the VM's int compare.
#[test]
fn shared_variant_name_dispatches_per_enum() {
    let src = "enum Color:\n    Red\n    Blue\nenum Light:\n    Red\n    Green\nfn cname(c: Color) -> str:\n    return match c:\n        Color.Red: \"c-red\"\n        Color.Blue: \"c-blue\"\nfn lname(l: Light) -> str:\n    return match l:\n        Light.Red: \"l-red\"\n        Light.Green: \"l-green\"\nfn main():\n    print(cname(Color.Red))\n    print(lname(Light.Red))\n    print(cname(Color.Blue))\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "c-red\nl-red\nc-blue\n");
}

/// M19 lever #2 regression guard — `?` on a GENUINE native Option must still work when a user enum
/// shadows the `Some` name. `pop()` stamps the fixed VID_SOME directly, so `?`'s `variant_id ==
/// VID_SOME` gate hits even though `variants["Some"]` now resolves to the user variant. Before the
/// fix the VM faulted with `'?' expects a `T?` or `T!E` value, found enum`.
#[test]
fn try_operator_works_on_native_option_under_variant_shadow() {
    let src = "enum Foo:\n    Some(int)\n    Bar\nfn first(xs: List[int]) -> int?:\n    v := xs.pop()?\n    return ?v\nfn main():\n    match first([10, 20]):\n        ?v:\n            print(\"first {v + 0}\")\n        None:\n            print(\"none\")\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, "first 20\n",
        "? must unwrap a genuine native Option even under Some shadowing"
    );
}

/// M19 lever #2 — `Op::MatchArm` carries a compile-time `variant_id: u32` so match dispatch is a
/// pure-int compare (no per-arm variant-name string compare; the JIT jump-table groundwork). Asserts
/// the emitted op for a `match` on a user enum carries the dense id, not VID_NONE.
#[test]
fn match_arm_dispatches_by_variant_id() {
    let src = "enum Color:\n    Red\n    Green\n    Blue\nfn pick(c: Color) -> int:\n    match c:\n        Color.Red: return 0\n        Color.Green: return 1\n        Color.Blue: return 2\nfn main():\n    print(pick(Color.Green))\nmain()\n";
    let tokens = lexer::tokenize(src).expect("lex");
    let module = parser::parse(tokens).expect("parse");
    let program = crate::compiler::compile_module_standalone(&module).expect("compile");
    // ROOT REDESIGN — the enum is keyed by its qualified IDENTITY KEY (`<main>::Color`).
    let green_id = program
        .variants
        .get(&("<main>::Color".to_string(), "Green".to_string()))
        .expect("Green registered")
        .variant_id;
    // The emitted MatchArm for the `Green` arm must carry Green's dense id.
    let found = program.protos.iter().flat_map(|p| p.code.iter()).any(|op| {
            matches!(op, Op::MatchArm { variant, variant_id, .. } if variant == "Green" && *variant_id == green_id)
        });
    assert!(
        found,
        "MatchArm for 'Green' must carry its dense variant_id"
    );
    // And it must select the right arm at runtime.
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, "1\n");
}

/// M19 memory-layout lever #2 golden: `examples/enum_layout.chz` exercises every observable
/// surface the variant-id change touches — nullary + payload variants, a generic `Option[T]`/
/// `Result[T,E]`, exhaustive `match`, match-with-binding, a guard, enum `==` (equal /
/// unequal-by-variant / unequal-by-enum-type), Display + `${}` interpolation, nested enums, and
/// `Result`/`Option` + the `?` operator. Byte-identical on the VM, interp, the `--parallel`
/// engine (an enum crosses a `spawn`+`Channel`), and the checked-in `.expected`.
#[test]
fn golden_enum_layout_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/enum_layout.chz");
    let expected = include_str!("../../../examples/enum_layout.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from enum_layout.expected"
    );
}

/// Enum-methods golden: `examples/enum_methods.chz` (a `match self` method, a method returning a
/// new variant, a generic enum method using `T`, an enum `str(self)` satisfying Stringable, and an
/// enum satisfying `Add`/`Comparable` through both a generic bound and direct `+`/`<`/`==`)
/// byte-identical on the VM, the interpreter, the parallel engine, and its `.expected`.
#[test]
fn enum_with_hash_is_usable_as_map_set_key() {
    // Regression: an enum defining `hash(self) -> int` satisfies Hashable at the CHECKER, so
    // `Set[E]`/`Map[E,V]` type-check — but the runtime must also DISPATCH the enum's hash at
    // runtime (previously they raised "enum is not hashable", crashing a check-clean program).
    let src = "enum Color:\n    Red\n    Green\n    Blue\n\
                   \n    fn hash(self) -> int:\n        match self:\n            Color.Red: return 1\n            Color.Green: return 2\n            Color.Blue: return 3\n\
                   s := {Color.Red, Color.Green, Color.Red}\n\
                   print(s.len())\n\
                   m := {Color.Blue: \"b\"}\n\
                   print(m[Color.Blue])\n";
    assert_eq!(run_parity(src), "2\nb\n");
}

#[test]
fn golden_enum_methods_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/enum_methods.chz");
    let expected = include_str!("../../../examples/enum_methods.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from enum_methods.expected"
    );
}

/// Literals golden: `examples/literals.chz` (scientific-notation floats, `\u{…}` unicode
/// escapes, single-quote strings incl. interpolation) byte-identical on the VM and
/// `.expected`. Proves the new lexer forms lex the same (parity by construction).
#[test]
fn golden_literals_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/literals.chz");
    let expected = include_str!("../../../examples/literals.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Large-integral float golden: `examples/float_large_integral.chz`. Floats print with CPython
/// `repr()`/`str()` parity (docs/syntax.md contract): scientific notation once the decimal exponent
/// is `>= 16` (`1.5e23` → `1.5e+23`), fixed with a `.0` below that. Exercises BOTH the engine
/// `format_float` path (bare interpolation) and `fmtspec::repr_float` (bare format-spec, no type
/// char) — now the same `repr_float` helper — and asserts byte-identical against `.expected`.
#[test]
fn golden_float_large_integral_matches_expected_and_interp() {
    let src = include_str!("../../../examples/float_large_integral.chz");
    let expected = include_str!("../../../examples/float_large_integral.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// bytes golden: `examples/bytes.chz` (the full operation table — `b"..."` literal with `\xHH`
/// escapes, index→int, slice→bytes incl. reverse/step/negative, for-loop→int, len, ==/!=, a map
/// key, an out-of-range index under `recover:`, and the `b'...'` Display/str()/interp repr)
/// byte-identical on the VM, the interpreter, and its `.expected` (three-engine parity gate).
#[test]
fn golden_bytes_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/bytes.chz");
    let expected = include_str!("../../../examples/bytes.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// conversions golden: `examples/conversions.chz` (str.encode()/bytes.decode()/bytearray.decode()
/// UTF-8 round-trip incl. a multi-byte char, an invalid-UTF-8 decode under `recover:`, List() over
/// every for-iterable shape — list/set/str/bytes/bytearray/range/user-iterator — Set() dedup, and
/// Map() from 2-tuples with a last-wins dup key) byte-identical on the VM, the interpreter, and its
/// `.expected` (three-engine parity gate).
#[test]
fn golden_conversions_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/conversions.chz");
    let expected = include_str!("../../../examples/conversions.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// encode/decode UTF-8 round-trip (multi-byte char): `str.encode()` then `bytes.decode()` returns
/// the original string, byte-identical on the VM + interp.
#[test]
fn encode_decode_roundtrip_multibyte() {
    let src = "fn main():\n    s := \"héllo\"\n    print(s.encode().decode())\n    print(s.encode().decode() == s)\nmain()\n";
    let out = run(src);
    assert_eq!(out, "héllo\ntrue\n");
}

/// `bytearray.decode()` mirrors `bytes.decode()` exactly (decode the current buffer).
#[test]
fn bytearray_decode_matches_bytes() {
    let src = "fn main():\n    print(bytearray([104, 105]).decode())\n    print(b\"hi\".decode())\nmain()\n";
    let out = run(src);
    assert_eq!(out, "hi\nhi\n");
}

/// Invalid UTF-8 `decode()` is a RECOVERABLE fault (catchable by `recover:`), not a panic — and the
/// error message is byte-identical.
#[test]
fn invalid_utf8_decode_recoverable() {
    let src = "fn main():\n    r := recover:\n        b\"\\xff\\xfe\".decode()\n    match r:\n        ?v: print(v)\n        !e: print(\"caught\")\nmain()\n";
    let out = run(src);
    assert_eq!(out, "caught\n");
    // Uncaught, the same fault propagates as a recoverable RuntimeError with the same message.
    let bare = "fn main():\n    print(b\"\\xff\".decode())\nmain()\n";
    let (_, vm_res) = run_program(bare);
    let (_, it_res) = run_program(bare);
    assert_eq!(vm_res.unwrap_err().message, "invalid UTF-8 in decode()");
    assert_eq!(it_res.unwrap_err().message, "invalid UTF-8 in decode()");
}

/// `List()`/`Set()`/`Map()` over a user `.next()` iterator + dedup + last-wins, byte-identical
/// VM/interp. The map last-wins on a duplicate key mirrors the `{k: v}` literal.
#[test]
fn constructors_over_user_iterator_and_dupkey() {
    let src = "struct C:\n    n: int\n    limit: int\n    fn next(self) -> int?:\n        if self.n >= self.limit:\n            return None\n        v := self.n\n        self.n = self.n + 1\n        return ?v\nfn main():\n    print(List(C(0, 4)).sum())\n    print(Set(C(0, 4)).len())\n    m := Map([(1, \"a\"), (1, \"b\")])\n    print(m.len())\n    print(m[1])\nmain()\n";
    let out = run(src);
    assert_eq!(out, "6\n4\n1\nb\n");
}

/// bytearray golden: `examples/bytearray.chz` (the full mutable-buffer table — all 4 constructor
/// forms, index read + WRITE, out-of-range write under `recover:`, slice→bytearray incl.
/// reverse/step, for-loop→int, push/pop/extend, len, ==/cross bytes==bytearray, shared-mutation
/// through two bindings, the bytes<->bytearray conversion round-trip, and the `bytearray(b'...')`
/// repr) byte-identical on the VM, the interpreter, and its `.expected` (three-engine parity gate).
#[test]
fn golden_bytearray_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/bytearray.chz");
    let expected = include_str!("../../../examples/bytearray.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Qualified enum-variant access golden: `examples/enum_qualified.chz` (the dotted `Enum.Variant`
/// spelling, nullary + payload, in construction AND `match` arms, interleaved with the bare form
/// and a generic enum) byte-identical on the VM, the interpreter, and its `.expected`. Pins that
/// the qualifier is a pure spelling aid resolving to the same variant.
#[test]
fn golden_enum_qualified_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/enum_qualified.chz");
    let expected = include_str!("../../../examples/enum_qualified.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Parity edge: a top-level `fn` named like an enum must NOT shadow qualified-variant access
/// `Enum.Variant` (the checker and VM ignore functions in the precedence gate; the interp must
/// too). Both engines must agree — previously the interp diverged ("cannot read field of
/// function") while the VM constructed the variant.
#[test]
fn qualified_variant_not_shadowed_by_function_parity() {
    let src =
        "enum Color:\n    Red\n    Green\nfn Color() -> int:\n    return 5\nprint(Color.Red)\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "Red\n");
}

/// Polymorphic method-IC golden: `examples/poly_method.chz` (a `List[Shape]` walked at one
/// `.area()` call site across FIVE distinct struct types — four fill the N-way method-call IC,
/// the fifth overflows it to the sticky-generic slow path) byte-identical on the VM, the
/// interpreter, and its `.expected`. Pins that the N-way IC is behavior-preserving (the interp,
/// which has no IC, is the oracle).
#[test]
fn golden_poly_method_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/poly_method.chz");
    let expected = include_str!("../../../examples/poly_method.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// or-pattern golden: `examples/match_or.chz` (or-patterns with + without bindings, a 3-variant
/// enum or-pattern that is exhaustive without `_`, a guard on an or-pattern, and nested nullary
/// variants `Some(None)` / `Ok(Err(e))`) byte-identical on the VM, the interpreter, the
/// `--parallel` engine, and its `.expected`.
#[test]
fn golden_match_or_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/match_or.chz");
    let expected = include_str!("../../../examples/match_or.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// QoL golden: `examples/membership.chz` (the `in` operator across list/set/map-key/substring,
/// true + false cases) byte-identical on the VM, the interpreter, the `--parallel` engine, and
/// its `.expected`.
#[test]
fn golden_membership_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/membership.chz");
    let expected = include_str!("../../../examples/membership.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// QoL golden: `examples/compound_assign.chz` (the 8 compound-assign ops across var/index/field/
/// map-value targets) byte-identical on the VM, the interpreter, the `--parallel` engine, and
/// its `.expected`.
#[test]
fn golden_compound_assign_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/compound_assign.chz");
    let expected = include_str!("../../../examples/compound_assign.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Concurrency demo golden: `examples/demo_spawn.chz` (`spawn` in a `parallel:` nursery, results
/// collected over a `Channel` and summed — order-independent, so byte-identical on the VM, the
/// interpreter, the `--parallel` engine, and its `.expected`). Its twin is `demo_executor`.
#[test]
fn golden_demo_spawn_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/demo_spawn.chz");
    let expected = include_str!("../../../examples/demo_spawn.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Concurrency demo golden: `examples/demo_executor.chz` (the `Executor` twin of `demo_spawn` —
/// detached `submit` + `shutdown` drain producing the same summed result). Byte-identical on the
/// VM, the interpreter, the `--parallel` engine, and its `.expected`.
#[test]
fn golden_demo_executor_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/demo_executor.chz");
    let expected = include_str!("../../../examples/demo_executor.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// QoL golden: `examples/multiline_str.chz` (triple-quoted strings — unescaped quotes, `\n`,
/// literal newlines, interpolation) byte-identical on the VM, the `--parallel`
/// engine, and its `.expected`. (Lexer-only feature; parity is by construction.)
#[test]
fn golden_multiline_str_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/multiline_str.chz");
    let expected = include_str!("../../../examples/multiline_str.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// QoL golden: `examples/tuple_swap.chz` (multi-target assignment — vars, list elements, struct
/// fields, three-way rotation, and `a, b = f()` tuple destructuring; RHS-first eval proven by
/// same-index swaps) byte-identical on the VM, the interpreter, the `--parallel` engine, and its
/// `.expected`.
#[test]
fn golden_tuple_swap_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/tuple_swap.chz");
    let expected = include_str!("../../../examples/tuple_swap.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// M8-M4 golden: `examples/set.chz` (the set type — literals, membership, algebra, iteration)
/// byte-identical on the VM, the interpreter, and its `.expected`.
#[test]
fn golden_set_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/set.chz");
    let expected = include_str!("../../../examples/set.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// `timer(ms)` golden: a one-shot timeout channel delivers `true`. Byte-identical on the
/// VM and `.expected` (inline-sleep to the deadline). `--parallel`
/// §6d golden: `examples/wait_select.chz` (Chezzi's `select` — source-order priority, `else:`,
/// a `timer` arm, `=` assignment, and a skipped closed+empty arm). Uses only non-blocking arms so
/// the VM AND `--parallel` are byte-identical (a truly-blocking `wait` is a
/// VM-only capability tested separately in `vm_wait_blocks_then_wakes_on_second_channel`).
#[test]
fn golden_wait_select_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/wait_select.chz");
    let expected = include_str!("../../../examples/wait_select.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(run_capture(src).expect("parallel"), expected);
}

/// delivers the same value via the background timer `send` (asserted separately).
#[test]
fn golden_timer_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/timer.chz");
    let expected = include_str!("../../../examples/timer.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(run_capture(src).expect("parallel"), expected);
}

/// Phase 4e — the selective `import timer from std.time` form still licenses the bare `timer(ms)`
/// call (opcode-backed, kept via the minimal std.time `native_module_sig` arm) after std.time went
/// file-backed. Byte-identical on the VM, the interpreter, and the M:N `--parallel` engine.
#[test]
fn golden_timer_selective_import_three_engine() {
    let src = "import timer from std.time\nfn main():\n    print(timer(20).recv())\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "true\n");
    assert_eq!(run_capture(src).expect("parallel"), "true\n");
}

/// `timer(ms)` under `--parallel`: a spawned fiber recv-blocks on the timeout channel, PARKS, and
/// is woken by the background timer `send` at the deadline — not a false deadlock (the pending
/// timer is accounted as `inflight`, vetoing the predicate while the lone fiber waits). Proves the
/// async-delivery path + the deadlock veto + the park/wake on the timer channel's key.
#[test]
fn parallel_timer_wakes_blocked_recv() {
    let src = "fn waiter(t: Channel[bool]):\n    print(t.recv())\n\
                   fn main():\n    parallel:\n        spawn waiter(timer(20))\nmain()\n";
    assert_eq!(run_capture(src).expect("parallel"), "true\n");
}

/// `Atomic[T]` golden: single-thread load/store/add/sub/exchange/cas sequence, byte-identical on
/// the VM, the interpreter, and `.expected`.
#[test]
fn golden_atomic_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/atomic.chz");
    let expected = include_str!("../../../examples/atomic.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Multi-line literals golden: `examples/multiline_literals.chz` (newline/indent suppression
/// inside `[]`/`{}`/`()` + optional trailing commas on list/map/tuple/call/params) byte-identical
/// on the VM, interp, and `.expected`.
#[test]
fn golden_multiline_literals_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/multiline_literals.chz");
    let expected = include_str!("../../../examples/multiline_literals.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Format spec: every supported `{expr:spec}` case (align/fill/width/zero-pad/precision/
/// each type char/percent/sign/string-truncate, plus a bare float and a `:`-inside-index) must
/// be byte-identical across the VM and `--parallel`.
#[test]
fn fmt_specs_parity() {
    let src = "\
m := {\"a:b\": 7}
print(\"[{m[\"a:b\"]:>4}]\")
print(\"|{42:>6}|{42:<6}|{42:^6}|\")
print(\"z={42:06}|{-7:06}\")
print(\"f={3.14159:.2f}|e={2.5:e}|p={0.1357:.1%}\")
print(\"x={255:x}|X={255:X}|b={255:b}|o={255:o}\")
print(\"s={5:+d}|{-5:+d}\")
print(\"bare={5.0}|fmt={5.0:.2f}|w={5.0:>8}\")
";
    // Every value verified against CPython's str.format mini-language (the owning ancestor).
    let expected = "[   7]\n|    42|42    |  42  |\nz=000042|-00007\n\
        f=3.14|e=2.500000e+00|p=13.6%\nx=ff|X=FF|b=11111111|o=377\n\
        s=+5|-5\nbare=5.0|fmt=5.00|w=     5.0\n";
    assert_eq!(run_capture(src).expect("vm run"), expected);
}

/// An interpolated ternary is written in parentheses: the first `:` at hole depth starts the format
/// spec, so a parenthesized ternary runs and CAN carry a spec, and a bare `{if b: a else: b}` is an
/// error that says so (TICKET-252, owner answer 2b).
#[test]
fn fmt_interpolated_ternary_parity() {
    let src = "\
b := true
print(\"val={(if b: 10 else: 20)}\")
print(\"fmt={(if b: 1 else: 2):>5}\")
";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "val=10\nfmt=    1\n");
}

/// A pathological field width is rejected (with the cap message) BEFORE any allocation — the
/// fix for the prior OOM. Must error identically (it is a compile-time error on
/// the VM path, a runtime error on the interpreter; the message string is the same).
#[test]
fn pathological_width_rejected() {
    let src = "x := 1\nprint(\"{x:>100000000}\")\n";
    let vm_err = run_capture(src).expect_err("vm must reject pathological width");
    assert!(
        vm_err.to_string().contains("exceeds maximum 4096"),
        "vm: {vm_err}"
    );
    let interp_err = run_capture(src).expect_err("interp must reject");
    assert!(
        interp_err.to_string().contains("exceeds maximum 4096"),
        "interp: {interp_err}"
    );
}

/// Golden: `examples/format_specs.chz` (the full format mini-language) byte-identical on the VM,
/// the interpreter, `--parallel`, and its `.expected`.
#[test]
fn golden_format_specs_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/format_specs.chz");
    let expected = include_str!("../../../examples/format_specs.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// `range` golden: `examples/range_step.chz` (3-arg up/down/by-N, empty / wrong-direction cases,
/// the unchanged 1/2-arg forms, and slicing a `..` range literal with a `::step`) byte-identical
/// on the VM against `.expected` — the guard for stepped ranges.
#[test]
fn golden_range_step_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/range_step.chz");
    let expected = include_str!("../../../examples/range_step.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// `print` kwargs golden: `examples/print_kwargs.chz` (default form, `end=""`, `sep=`, both, and
/// runtime str exprs) byte-identical on the VM against `.expected` — the guard for `print`'s
/// `sep=`/`end=`.
#[test]
fn golden_print_kwargs_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/print_kwargs.chz");
    let expected = include_str!("../../../examples/print_kwargs.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// First-class universe builtin fn golden: `examples/defer_builtin_value.chz` — `defer print(...)`
/// as a bare first-class call, plus value-position use (`f := ord`/`chr`, `p := panic` raising
/// through the value call path). Byte-identical on the VM, the M:N OS-thread engine,
/// and `.expected`.
#[test]
fn golden_defer_builtin_value_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/defer_builtin_value.chz");
    let expected = include_str!("../../../examples/defer_builtin_value.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Phase 3a golden: `examples/native_prelude.chz` exercises the eight universe builtins now
/// DECLARED in std/prelude.chz (int/float/str/bytes/bytearray ctors, ord/chr fns, panic in a
/// recover:) plus synthetic `print` with sep=/end=. Byte-identical on the VM, the M:N
/// OS-thread engine, and `.expected` — the migration must change no output.
#[test]
fn golden_native_prelude_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/native_prelude.chz");
    let expected = include_str!("../../../examples/native_prelude.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// `panic` AS A VALUE (`p := panic; p("boom")`) raises the recoverable `RuntimeError` through the
/// value call path — NOT a returned value / NOT silent nil — byte-identical on VM + interp. If the
/// value path returned `Ok`, an uncaught `p(...)` would fall through instead of faulting.
#[test]
fn panic_as_value_uncaught_raises_both_engines() {
    let src = "fn main():\n    p := panic\n    p(\"boom\")\nmain()\n";
    let vm_err = run_capture(src).expect_err("vm: panic value must fault");
    let it_err = run_capture(src).expect_err("interp: panic value must fault");
    assert_eq!(vm_err.message, "boom");
    assert_eq!(it_err.message, "boom");
}

/// `ord`/`chr` as VALUES (`f := ord`) called back yield the same result as a direct call, VM +
/// interp identical.
#[test]
fn ord_chr_as_value_both_engines() {
    let src =
        "fn main():\n    f := ord\n    g := chr\n    print(f(\"a\"))\n    print(g(66))\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "97\nB\n");
}

/// Regression (bugs 1 & 3): a user binding named like a first-class builtin fn SHADOWS the builtin
/// in value position on BOTH engines — the compiler resolved `LoadBuiltin` before local/global
/// binding lookup (and the interp returned `Value::Builtin` before `env.get`), so a shadowed name
/// type-checked as the binding but printed `<builtin fn …>` at runtime. Param, global (`:=`), and
/// loop-var shadows must each resolve to the BINDING, byte-identical against the golden.
#[test]
fn user_binding_shadows_firstclass_builtin_both_engines() {
    // parameter shadow
    let a = "fn f(ord: int):\n    print(ord)\nf(42)\n";
    assert_eq!(run_capture(a).expect("vm"), "42\n");
    // top-level global (`:=`) shadow, read in value position
    let b = "chr := \"hello\"\nx := chr\nprint(x)\n";
    assert_eq!(run_capture(b).expect("vm"), "hello\n");
    // loop-variable shadow
    let c = "for chr in [\"a\", \"b\"]:\n    print(chr)\n";
    assert_eq!(run_capture(c).expect("vm"), "a\nb\n");
}

/// Bug 2 / bug 4: two first-class builtin-fn VALUES compare by NAME, byte-identical against the golden.
/// Each value-position use emits a fresh `Op::LoadBuiltin` → a distinct `Obj::Builtin` handle, so
/// the VM's `values_equal_guarded` (which short-circuits only on `ha == hb`) must have a dedicated
/// `(Obj::Builtin, Obj::Builtin)` name-compare arm; otherwise it falls to the `_ => Ok(false)`
/// catch-all and returns `false` while the interp (derived `PartialEq` on the name) returns
/// `true`. Covers bound values, the bare-name compare, a mismatch, and list-element recursion.
#[test]
fn builtin_value_equality_both_engines() {
    let src = "fn main():\n    f := ord\n    g := ord\n    print(f == g)\n    print(ord == ord)\n    print(chr == ord)\n    print([print] == [print])\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "true\ntrue\nfalse\ntrue\n");
}

/// Bug 3: all four first-class builtins are SENDABLE — a builtin bound to a local and CAPTURED
/// into a spawned task crosses the airlock (the `SnapValue::Builtin` path) and runs. Typing them
/// as the dedicated `Ty::BuiltinFn` (sendable), not a plain `Ty::Func` (conservatively
/// non-sendable), removes the asymmetry where only `print` (once typed `Unknown`) could cross.
/// Byte-identical on the VM and the M:N engine.
#[test]
fn builtin_value_sendable_across_airlock_both_engines() {
    let src = "import std.concurrency\nfn main():\n    f := ord\n    p := print\n    parallel:\n        spawn:\n            p(f(\"a\"))\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "97\n");
}

/// A first-class builtin fn value spawned as a DIRECT CALL callee (`f := print; spawn f(x)`,
/// distinct from the `spawn:` block form above) must lower + run against a literal golden. The
/// callee reaches `prepare_worker`'s `PendingCall::Call` arm, which only handled Closure/Func —
/// a raw `Obj::Builtin` hit the reject `_` on the M:N engine only (`spawn: 'function' is not an
/// isolable task`) while serial/interp dispatched it fine: a three-engine parity divergence on a
/// checker-accepted program. Fixed by `Lowered::Value` (TICKET-226: every non-closure callable
/// crosses by value through `to_wire`, classified by `Vm::callable`).
#[test]
fn spawn_builtin_fn_value_as_call_callee_both_engines() {
    let src = "import std.concurrency\nfn main():\n    g := print\n    parallel:\n        spawn g(\"from-task\")\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "from-task\n");
}

/// `spawn print("hi")` — a bare first-class builtin spawned DIRECTLY (no intermediate binding),
/// symmetric with `defer print(...)`. `print` lowers to `Op::LoadBuiltin` (value position) then
/// `SpawnCall`, so the callee is an `Obj::Builtin` crossing the airlock by name.
/// The checker gate accepts it (parity-perf-0 fix); runtime prints identically.
#[test]
fn spawn_bare_builtin_print_both_engines() {
    let src = "fn main():\n    parallel:\n        spawn print(\"hi\")\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "hi\n");
}

/// Value-form `print` (`f := print; f(x)`) keeps its (single) arg GC-ROOTED while stringifying —
/// a `Stringable` `str` method runs user code that can `collect()` at a safepoint and would sweep
/// the off-stack arg, a use-after-free. Under `gc_stress` (collect before every instruction) the
/// output must still be correct and match the non-stress run. (The value form is a
/// fixed 1-arg function; the direct call keeps the variadic/`sep=`/`end=` surface.)
#[test]
fn print_as_value_arg_rooted_under_gc_stress() {
    let src = "struct Loud:\n    n: int\n    fn str(self) -> str:\n        return \"L{self.n}\"\n\
                   fn main():\n    f := print\n    f(Loud(7))\nmain()\n";
    let stressed = run_capture_stress(src);
    assert_eq!(stressed, "L7\n");
}

/// `assert` golden: `examples/assert.chz` (bare + message forms, all passing) byte-identical on
/// the VM against `.expected` — the guard for the primitive.
#[test]
fn golden_assert_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/assert.chz");
    let expected = include_str!("../../../examples/assert.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}
