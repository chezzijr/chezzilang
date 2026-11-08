//! VM tests: M24-2 witness reach, W7-49/W7-51 defaults resolved in their declaring module.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== M24-2 — a nested body captures `$w:T` only when it can REACH a witness =====
//
// RUST, not `tests/chz/`: the subject is a proto's CAPTURE LIST, which is bytecode internals no
// `assert` can see. The behavioural half (the witness still WORKS wherever it is genuinely used) is
// `tests/chz/spec/static_witness_test.chz` and the runner below.

/// The `capture_names` of every proto named `proto`, in compile order.
fn capture_names_of(src: &str, proto: &str) -> Vec<Vec<String>> {
    let module = parser::parse(lexer::tokenize(src).unwrap()).unwrap();
    let program = crate::compiler::compile_module_standalone(&module).unwrap();
    program
        .protos
        .iter()
        .filter(|p| p.name == proto)
        .map(|p| p.capture_names.clone())
        .collect()
}

/// The hidden local of `T`'s one witness slot (`T` and its requirement `default()`), as
/// `crate::checker::witness_tag` spells it.
const W_T: &str = "$w:T:default()";

/// The shared preamble: a static requirement, a type that answers it, and a witness-taking free fn.
const W_PRELUDE: &str = "protocol Default:\n    fn default() -> Self\n\
     struct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\n\
     fn reset[T: Default](old: T) -> T:\n    return T.default()\n";

#[test]
fn m24_2_a_nested_body_that_uses_no_witness_captures_none() {
    // `outer` is charged `$w:T` (its own body calls `T.default()`), but the closure only adds ints.
    let src = format!(
        "{W_PRELUDE}fn outer[T: Default](x: int) -> int:\n    c := T.default()\n    \
         f := fn(): x + 1\n    return f() + c.n\nprint(outer[Counter](1))\n"
    );
    let caps = capture_names_of(&src, "<closure>");
    assert_eq!(caps.len(), 1, "one closure proto");
    assert_eq!(
        caps[0],
        vec!["x".to_string()],
        "the closure reaches no witness, so it captures none"
    );
}

#[test]
fn m24_2_a_sibling_closure_that_calls_the_static_keeps_its_witness() {
    // Same frame, two closures: only the one naming `T` carries `$w:T`.
    let src = format!(
        "{W_PRELUDE}fn outer[T: Default](x: int) -> int:\n    f := fn(): x + 1\n    \
         g := fn(): T.default().n\n    return f() + g()\nprint(outer[Counter](1))\n"
    );
    let caps = capture_names_of(&src, "<closure>");
    assert_eq!(caps.len(), 2, "two closure protos");
    assert!(
        !caps[0].iter().any(|n| n.starts_with("$w:")),
        "the witness-free sibling captures no witness: {:?}",
        caps[0]
    );
    assert!(
        caps[1].contains(&W_T.to_string()),
        "the `T.default()` closure keeps its witness: {:?}",
        caps[1]
    );
}

#[test]
fn m24_2_a_witness_two_levels_down_keeps_both_captures() {
    // Only `inner` names `T`; `mid` must still capture, or `inner`'s MakeClosure has nothing to
    // snapshot. The free-name walk flattens a nested body's names into the enclosing one, which is
    // exactly what makes the outer capture survive.
    let src = format!(
        "{W_PRELUDE}fn outer[T: Default]() -> int:\n    fn mid() -> int:\n        \
         fn inner() -> int:\n            return T.default().n\n        return inner()\n    \
         return mid()\nprint(outer[Counter]())\n"
    );
    for name in ["mid", "inner"] {
        let caps = capture_names_of(&src, name);
        assert_eq!(caps.len(), 1, "one `{name}` proto");
        assert!(
            caps[0].contains(&W_T.to_string()),
            "`{name}` keeps the witness: {:?}",
            caps[0]
        );
    }
}

#[test]
fn m24_2_a_nested_body_that_only_forwards_keeps_its_witness() {
    // The closure never spells `T` — it calls a witness-TAKING fn with a non-concrete argument, so
    // the checker records a `Forward(T)` there and the compiler must have `$w:T` to push.
    let src = format!(
        "{W_PRELUDE}fn outer[T: Default](v: T) -> int:\n    f := fn(): reset(v)\n    \
         _ := f()\n    return 0\nprint(outer(Counter(1)))\n"
    );
    let caps = capture_names_of(&src, "<closure>");
    assert_eq!(caps.len(), 1, "one closure proto");
    assert!(
        caps[0].contains(&W_T.to_string()),
        "a forwarding body needs the witness: {:?}",
        caps[0]
    );
}

#[test]
fn m24_2_a_nested_body_forwarding_through_a_member_keeps_its_witness() {
    // The forward goes through an instance method that declares its OWN witnessed `[T]` (Task 5).
    // The call is `h.build[T]()`: `T` appears only as a type ARGUMENT (which no free-name walk
    // collects) and the callee is a RECEIVER, not a module — so neither half of a name-only
    // predicate sees it. The checker still records `Forward(T)` here, so the witness must ride.
    let src = format!(
        "{W_PRELUDE}struct Holder:\n    k: int\n    fn build[T: Default](self) -> T:\n        \
         return T.default()\nfn outer[T: Default](h: Holder) -> int:\n    c := T.default()\n    \
         f := fn(): h.build[T]()\n    _ := f()\n    return 1\nprint(outer[Counter](Holder(1)))\n"
    );
    let caps = capture_names_of(&src, "<closure>");
    assert_eq!(caps.len(), 1, "one closure proto");
    assert!(
        caps[0].contains(&W_T.to_string()),
        "a member forward needs the witness: {:?}",
        caps[0]
    );
    // …and it really runs,. The source is one the REAL pipeline accepts: `run_capture`
    // skips the checker, so a program the checker rejects would "pass" here while `chezzi run` failed.
    assert!(
        crate::checker::check(&parser::parse(lexer::tokenize(&src).unwrap()).unwrap()).is_ok(),
        "the program must type-check, or this test pins nothing `chezzi run` would accept"
    );
    let vo = run_capture(&src).expect("VM run");
    assert_eq!(vo, "1\n", "VM output");
}

/// M24-2 fix pass — the walk's `bound` set holds only the names bound INSIDE the nested body, so a
/// PARAMETER named `lib` is recorded as `(Some("lib"), "build")`, indistinguishable from a real
/// module qualification. Reading that as a module looks `build` up as a module-level fn of `lib`,
/// misses, drops `$w:T`, and this check-clean program dies at runtime with "internal: no type witness
/// in scope to forward as 'T' into the call to 'build'" (parity is blind to it).
///
/// Multi-file on purpose: only a real `import lib` puts `lib` in `imported_modules`, which is the
/// table the shadow has to defeat. The std-module half (`import std.math` + a parameter named
/// `math`) is single-file and lives in `tests/chz/spec/static_witness_test.chz`.
#[test]
fn m24_2_a_parameter_shadowing_a_user_module_keeps_the_witness() {
    let dir = std::env::temp_dir().join(format!("chezzi_m24_2_shadow_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lib.chz"),
        "protocol Default:\n    fn default() -> Self\n\
         struct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\n\
         struct Holder:\n    k: int\n    fn build[T: Default](self, old: T) -> T:\n        \
         return T.default()\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import lib\nimport Counter, Holder, Default from lib\n\
         fn outer[T: Default](v: T, lib: Holder) -> int:\n    z := T.default()\n    \
         f := fn(): lib.build[T](v)\n    g := f()\n    return 14\n\
         print(outer(Counter(1), Holder(2)))\n",
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert_eq!(vo, "14\n", "VM output");
}

#[test]
fn m24_2_a_type_named_like_a_user_module_is_rejected() {
    // `import Shape` beside `struct Shape` gave the head `Shape` two meanings and no source order
    // to pick one, so the checker rejects the pair: "'Shape' is already imported" (TICKET-180; Go:
    // "Shape redeclared in this block"). The witness half keeps its coverage under a struct of its
    // own name, `Form`, beside a live `import Shape`: the head of `Form.mk[T](x)` is the struct,
    // and the closure still forwards `$w:T` (the old fault was "internal: no type witness in scope
    // to forward as 'T' into the call to 'mk'"). Multi-file because a USER module bind takes two
    // files; the std-module spelling is
    // `tests/chz/spec/static_witness_test.chz::a_type_named_like_a_std_module_keeps_the_witness`.
    let dir = std::env::temp_dir().join(format!("chezzi_m24_2_typehead_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("Shape.chz"),
        "fn area(k: int) -> int:\n    return k * 2\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    let main = |ty: &str| {
        format!(
            "protocol Default:\n    fn default() -> Self\n\
             struct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\n\
             import Shape\n\
             struct {ty}:\n    k: int\n    fn mk[T: Default](x: T) -> T:\n        return T.default()\n\
             fn outer[T: Default](x: T) -> T:\n    seed := T.default()\n    \
             f := fn(): {ty}.mk[T](x)\n    return f()\n\
             print(outer(Counter(9)).n)\n\
             print(Shape.area(2))\n"
        )
    };
    std::fs::write(&entry, main("Shape")).unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    let errs = crate::checker::check_graph(&graph)
        .err()
        .unwrap_or_default();
    assert!(
        errs.iter()
            .any(|e| e.message.contains("'Shape' is already imported")),
        "a type named like a module import must be rejected, got: {errs:?}"
    );
    std::fs::write(&entry, main("Form")).unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert_eq!(vo, "0\n4\n", "VM output");
}

#[test]
fn m24_2_a_spawn_and_defer_block_without_a_witness_capture_none() {
    // The other two capture sites (`spawn:` / `defer:` blocks), same rule.
    let src = format!(
        "{W_PRELUDE}fn outer[T: Default](x: int) -> int:\n    c := T.default()\n    \
         defer:\n        print(x)\n    parallel:\n        spawn:\n            print(x + 1)\n    \
         return c.n\nprint(outer[Counter](1))\n"
    );
    for name in ["<deferred block>", "<spawned task>"] {
        let caps = capture_names_of(&src, name);
        assert_eq!(caps.len(), 1, "one `{name}` proto");
        assert_eq!(
            caps[0],
            vec!["x".to_string()],
            "`{name}` reaches no witness, so it captures none"
        );
    }
}

// ===== W7-49 — a spliced default must not alias the caller's own side-table entries =====
//
// These three are RUST tests, not `tests/chz/` ones, for one reason: they are inherently MULTI-FILE.
// The bug only exists because `desugar` splices a callee's default-parameter expression into the
// CALLER's AST as a clone that keeps the DEFINING module's spans, so it takes two files at matched
// `line:col` to express — which `chezzi test` (one file per suite) cannot say. The single-file half
// (both guards' over-fire modes) IS in Chezzi, at `tests/chz/spec/default_splice_keys_test.chz`.
//
// Each one is a `check`-clean program whose ANSWER is wrong on `19f7696a` and right now. The source
// is laid out flush-left in raw strings on purpose: the collision IS the column alignment, so the
// alignment has to be readable. Shift either side by one column and the fault disappears — which is
// what isolates it to the key rather than to the lowering.

/// W7-49 (1/3) — the retired span-keyed keyword table (now `CallPlanTable`, keyed by call NodeId). `lib.chz`'s default `g(a=7, b=9)` and `main.chz`'s own
/// `g(b=1, a=2)` put their FIRST NAMED-ARG VALUE at the same `line:col`, so before `Span::file` they
/// shared one key: the later insert won and lib's identity permutation was applied to
/// main's reversed call.
///
/// Fails on `19f7696a` with `709` / **`102`** — `h2(a=1, b=2)`, lib's permutation on main's call —
/// under a clean `chezzi check`, byte-identical (parity is blind to it).
#[test]
fn a_spliced_default_does_not_alias_the_callers_keyword_call() {
    let dir = std::env::temp_dir().join(format!("chezzi_w749_keyword_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // `fn f`'s first named-arg value `7` sits at line 6, col 19 (the two comments are the padding
    // that puts it there) — the same line:col as main's `1` below.
    std::fs::write(
        dir.join("lib.chz"),
        r#"# lib
# two comment lines put `fn f` on line 6
fn h(a: int, b: int) -> int:
    return a * 100 + b
g := h
fn f(x: int = g(a=7, b=9)) -> int:
    return x
"#,
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        r#"import f from lib
fn h2(a: int, b: int) -> int:
    return a * 100 + b
g := h2
fn probe() -> int:
    vvvvvv := g(b=1, a=2)
    return vvvvvv
print(f())
print(probe())
"#,
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert_eq!(
        vo, "709\n201\n",
        "probe() is g(b=1, a=2) = h2(a=2, b=1) = 201; 102 means lib's permutation was applied"
    );
}

/// W7-49 (2/3) — `CarrierTable`. An Option-mode `?.` inside `lib.chz`'s default-parameter
/// expression and a Result-mode `?.` in `main.chz` land their NAME TOKENs at the same `line:col`
/// (both `5:26`), so before `Span::file` they shared one `CarrierKey` and main's `Result` carrier
/// got the `Option` lowering.
///
/// Fails on `19f7696a` with `3` then `runtime error … no match arm for variant 'Ok'`, under a clean
/// `chezzi check`,. CONTROL: widening main's `vvv…` binder by one character (so its
/// `len` lands at col 27) makes `19f7696a` print `3` / `4` — the fault is the KEY, not the lowering.
#[test]
fn a_spliced_default_carrier_does_not_alias_the_callers_carrier() {
    let dir = std::env::temp_dir().join(format!("chezzi_w749_carrier_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // The default is SELF-CONTAINED (`Some("abc")`, no lib global): a default is cloned into the
    // CALLER's scope, so one that names a lib-only global would not resolve in main at all.
    std::fs::write(
        dir.join("lib.chz"),
        r#"# four pad lines put `fn f` on line 5, so its
# `len` name token lands at exactly 5:26 — the
# same line:col as main's Result-mode carrier.
# The default is SELF-CONTAINED (no lib global).
fn f(x: int? = (?"abc")?.len()) -> int:
    match x:
        ?n: return n
        None: return -1
"#,
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        r#"import f from lib
fn getr() -> str!str:
    return ?"wxyz"
fn probe() -> int!str:
    vvvvvvvvv := getr()?.len()
    return ?vvvvvvvvv
print(f())
match probe():
    ?n: print(n)
    !e: print(e)
"#,
    )
    .unwrap();
    // The two `len` name tokens share `5:26`; a shared CarrierKey gives main's Result carrier the
    // Option lowering. (Before the per-kind hint table, the slot's `int` leaked into the `?x`
    // operand and rejected this default, so the run never happened.)
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted (shared carrier key): {vr:?}");
    assert_eq!(vo, "3\n4\n", "f() is Option-mode, probe() is Result-mode");
}

/// W7-49 (3/3) — `WitnessTable`. `lib.chz`'s default `empty[Counter]()` and `main.chz`'s
/// `empty()` (pinned to `Other` by its annotated result) put their CALLEE TOKEN at the same
/// `line:col` (`17:19`), so before `Span::file` they shared one `WitnessKey`.
///
/// MEASURED on `19f7696a`, not predicted: the mode is **the wrong CONCRETE TYPE constructed** —
/// `chezzi check` is clean, then `probe()` builds lib's `Counter` and faults
/// `no field 's' on Counter(n=7)`. The gaps row had listed three possible modes worst-first and
/// this is the MIDDLE one; neither the wrong-`argc` mode nor the false `internal:` compile error
/// occurs, because both colliding callees are the same one-witness `empty` and both witnesses
/// resolve to a concrete key. CONTROL: renaming main's `vvvv` binder one character longer makes
/// `19f7696a` print `7` / `oth`.
#[test]
fn a_spliced_default_witness_does_not_alias_the_callers_witness() {
    let dir = std::env::temp_dir().join(format!("chezzi_w749_witness_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // `empty` in `fn f`'s default sits at line 17, col 19.
    std::fs::write(
        dir.join("lib.chz"),
        r#"protocol Default:
    fn default() -> Self

struct Counter:
    n: int
    fn default() -> Counter:
        return Counter(7)

struct Other:
    s: str
    fn default() -> Other:
        return Other("oth")

fn empty[T: Default]() -> T:
    return T.default()

fn f(c: Counter = empty[Counter]()) -> int:
    return c.n
"#,
    )
    .unwrap();
    // `Counter` is imported here only to keep the fixture byte-identical to the one that pinned the
    // W7-49 alias. It is no longer NEEDED: since W7-51 the default's turbofish resolves in `lib`,
    // where `Counter` is declared, not in this caller's scope. Left in place so the before/after of
    // this test is a pure key question.
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        r#"import f, empty, Other, Counter from lib
# pad 2
# pad 3
# pad 4
# pad 5
# pad 6
# pad 7
# pad 8
# pad 9
# pad 10
# pad 11
# pad 12
# pad 13
# pad 14
# pad 15
fn probe() -> str:
    vvvv: Other = empty()
    return vvvv.s
print(f())
print(probe())
"#,
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vr.is_ok(),
        "VM faulted (lib's Counter witness on main's Other call): {vr:?}"
    );
    assert_eq!(
        vo, "7\noth\n",
        "f() witnesses Counter; probe() witnesses Other — a shared key builds Counter twice"
    );
}

/// W7-51 — the W7-49 residual DISSOLVED, on the same source, byte for byte. The hole was that one
/// default cloned into two call sites of ONE module keeps one set of spans and therefore one
/// side-table key, while resolving differently at each site: here `g` is the module global
/// `ab(a, b)` at the outer call and the caller-local `ba(b, a)` inside `probe()`.
///
/// Three measured verdicts on one unchanged program:
///   * `19f7696a` — a silent wrong value, `709` / **`907`**, clean `check`.
///   * `b1307258` — the W7-49 backstop refuses the conflicting key: `internal: two different
///     keyword-argument decisions were recorded for one source position …`, no output.
///   * now — `709` / `709`. There is nothing to disagree about: the default is compiled ONCE, in
///     `lib`, where `g` is unambiguously `h`, and the caller's `ba` never enters the picture. The
///     conflict channel stays in place as defence in depth, but the class is now unrepresentable —
///     a non-literal default expression exists exactly once in the whole program.
///
/// This also pins the SCOPE half in its sharpest form: a caller-side local shadowing the definer's
/// global must not change the value. Python agrees (`709` twice).
#[test]
fn a_default_that_a_caller_local_shadows_still_resolves_in_its_definer() {
    let dir = std::env::temp_dir().join(format!("chezzi_w749_residual_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lib.chz"),
        r#"fn h(a: int, b: int) -> int:
    return a * 100 + b
g := h
fn f(x: int = g(a=7, b=9)) -> int:
    return x
"#,
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        r#"import f from lib
fn ab(a: int, b: int) -> int:
    return a * 100 + b
fn ba(b: int, a: int) -> int:
    return a * 100 + b
g := ab
fn probe() -> int:
    g := ba
    return f()
print(f())
print(probe())
"#,
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert_eq!(
        vo, "709\n709\n",
        "the default is `g(a=7, b=9)` in lib, where `g` is `h`; probe's local `ba` is irrelevant"
    );
}

// ===== W7-51 — a default resolves in the module that DECLARES it =====
//
// RUST, not `tests/chz/`, for the same reason as the W7-49 trio above: the defect is inherently
// MULTI-FILE. The single-file half (chained depth, per-call evaluation, the inline class) is in
// `tests/chz/spec/default_scope_chain_test.chz`.
//
// Every row below is measured on `b1307258` in the task report; each `expected` is CPython's answer
// for the equivalent two-module program.

/// W7-51 (1/3) — the sharp one: a default reading the DEFINER's module global, where the caller
/// happens to declare a global of the same name. On `b1307258` this prints **`99`** — the caller's
/// value, silently, with a clean `check`,. Python prints `7`.
///
/// Without the caller's `K` the same program is not merely wrong but unbuildable on `b1307258`
/// (`type error (line 2, col 15): unknown name 'K'`), which is the second row of the table.
#[test]
fn a_cross_module_default_resolves_in_its_definer_not_the_caller() {
    let dir = std::env::temp_dir().join(format!("chezzi_w751_global_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("g.chz"),
        "K := 7\nfn f(x: int = K) -> int:\n    return x\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    // The caller declares its OWN `K`. Under a spliced clone that value won; under a provider the
    // body runs against `g`'s namespace, so it cannot.
    std::fs::write(&entry, "import g\nK := 99\nprint(g.f())\nprint(K)\n").unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert_eq!(
        vo, "7\n99\n",
        "the default reads g's K (7), not the caller's (99); the caller's own K is untouched"
    );
}

/// W8-47 — a generic struct's field default is a provider generic in the struct's own type
/// parameters, resolved in its DECLARING module: `lib.Holder[int](1)` forwards its turbofish to
/// `lib`'s provider, whose body runs in `lib`'s namespace.
#[test]
fn a_generic_structs_field_default_resolves_in_its_declaring_module() {
    let dir = std::env::temp_dir().join(format!("chezzi_w847_ctor_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lib.chz"),
        "struct G[T]:\n    v: T\nfn mkl[T]() -> List[G[T]]:\n    xs: List[G[T]] = []\n    return xs\nstruct Holder[T]:\n    n: int\n    items: List[G[T]] = mkl()\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import lib\nh := lib.Holder[int](1)\nprint(h.n)\nprint(h.items.len())\n",
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert_eq!(vo, "1\n0\n", "got {vo:?}");
}

/// W7-51 (2/3) — a default that CALLS a function the definer declares and the caller never imports.
/// `b1307258`: `type error (line 3, col 15): unknown name 'cval'`. Python: `3`.
#[test]
fn a_cross_module_default_may_call_the_definers_own_function() {
    let dir = std::env::temp_dir().join(format!("chezzi_w751_ownfn_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("g.chz"),
        "fn cval() -> int:\n    return 3\nfn f(x: int = cval()) -> int:\n    return x\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(&entry, "import g\nprint(g.f())\nprint(g.f(8))\n").unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert_eq!(
        vo, "3\n8\n",
        "the definer's own fn runs; an explicit arg bypasses it"
    );
}

/// W7-51 (3/3) — a default that calls through the DEFINER'S OWN IMPORT, three modules deep and via a
/// `from`-import at the omitting site rather than a module alias. `b1307258`: `type error (line 2,
/// col 15): unknown name 'c2'`. Python: `3`.
///
/// This is also the load-order case the synthetic import edge has to get right: `main` gains an edge
/// to `g`, which is already a load-order ancestor because `main` imports it, and `g`'s own provider
/// call to `c2` needs no edge at all (same module).
#[test]
fn a_cross_module_default_may_call_through_the_definers_import() {
    let dir = std::env::temp_dir().join(format!("chezzi_w751_defimport_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("c2.chz"), "fn c() -> int:\n    return 3\n").unwrap();
    std::fs::write(
        dir.join("g.chz"),
        "import c2\nfn f(x: int = c2.c()) -> int:\n    return x\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(&entry, "import f from g\nprint(f())\n").unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert_eq!(
        vo, "3\n",
        "the default resolves g's own import, not main's namespace"
    );
}

/// W7-51 — a default that calls through the definer's own `import std.math`. The synthetic edge the
/// caller gains points at the definer, never at the definer's imports, so a NATIVE module in the
/// definer's namespace has to work with no extra machinery: `math` is licensed for `g` because `g`
/// imports it, and the provider body runs with `home == g`. `b1307258`: `type error (line 2,
/// col 17): unknown name 'math'` — main never imported `std.math`, and the clone landed in main.
#[test]
fn a_cross_module_default_may_call_through_the_definers_std_import() {
    let dir = std::env::temp_dir().join(format!("chezzi_w751_stdimport_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("g.chz"),
        "import std.math\nfn f(x: float = math.sqrt(16.0)) -> float:\n    return x\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(&entry, "import g\nprint(g.f())\nprint(g.f(1.5))\n").unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert_eq!(vo, "4.0\n1.5\n", "std.math resolves in g, not in main");
}

/// W7-51 — the definer is a TRANSITIVE dependency: `main` imports `mid`, `mid` imports `deep`, and
/// the default lives in `deep`. `main` never names `deep`, but reaches its method default through
/// the name-keyed method path (`mid.make()` returns a `deep.S`), so the dependency rule has to admit
/// it on the closure rather than on the direct import list — and the synthetic edge `main` gains
/// points two hops down. `b1307258`: `type error (line 5, col 30): unknown name 'dv'` (the clone
/// landed in `main`, where `deep`'s helper does not exist). Here: `13`.
#[test]
fn a_default_from_a_transitive_dependency_resolves_in_its_definer() {
    let dir = std::env::temp_dir().join(format!("chezzi_w751_transitive_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("deep.chz"),
        "fn dv() -> int:\n    return 3\nstruct S:\n    n: int\n    fn mprobe(self, x: int = dv()) -> int:\n        return x + self.n\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("mid.chz"),
        "import deep\nfn make() -> deep.S:\n    return deep.S(10)\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(&entry, "import mid\nprint(mid.make().mprobe())\n").unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert_eq!(vo, "13\n", "deep's own dv() is 3, plus S.n = 10");
}

/// W7-51 — a DIAMOND (`main` → {`l`, `r`} → `base`), asserted in BOTH orderings of `main`'s two
/// import lines. Order-stability is the property the transitive-closure rule bought and the one that
/// has to stay pinned: the predicate must not read a load-order index, and the two synthetic edges
/// `main` gains (one per call site, deduped by provider name) must not depend on which sibling was
/// visited first.
///
/// This is also the silent-wrong-value defect at its sharpest: `main` declares its own `K := 900`,
/// and `b1307258` printed **`910` / `920`** in both orderings — `main`'s `K` plus each maker's `n` —
/// where `base`'s author wrote `K := 3`. CPython's two-module equivalent prints the definer's value.
/// Here: `13` / `23`, and `main`'s own `K` is untouched.
#[test]
fn a_default_in_a_diamonds_shared_base_is_the_same_in_either_import_order() {
    for (tag, entry_src) in [
        (
            "lfirst",
            "import l\nimport r\nK := 900\nprint(l.make().mprobe())\nprint(r.make().mprobe())\nprint(K)\n",
        ),
        (
            "rfirst",
            "import r\nimport l\nK := 900\nprint(l.make().mprobe())\nprint(r.make().mprobe())\nprint(K)\n",
        ),
    ] {
        let dir =
            std::env::temp_dir().join(format!("chezzi_w751_diamond_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("base.chz"),
            "K := 3\nstruct S:\n    n: int\n    fn mprobe(self, x: int = K) -> int:\n        return x + self.n\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("l.chz"),
            "import base\nfn make() -> base.S:\n    return base.S(10)\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("r.chz"),
            "import base\nfn make() -> base.S:\n    return base.S(20)\n",
        )
        .unwrap();
        let entry = dir.join("main.chz");
        std::fs::write(&entry, entry_src).unwrap();
        let graph = crate::resolver::build_graph(&entry).expect("resolve");
        if let Err(errs) = crate::checker::check_graph(&graph) {
            let _ = std::fs::remove_dir_all(&dir);
            panic!("[{tag}] program must type-check, got: {errs:?}");
        }
        let (vo, _ve, vr, _vc) = run_file(&entry);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(vr.is_ok(), "[{tag}] VM faulted: {vr:?}");
        assert_eq!(
            vo, "13\n23\n900\n",
            "[{tag}] base's K (3) reaches both call sites; main's K is untouched"
        );
    }
}

/// TICKET-187: the check rejects the program with the arity error, before anything runs.
fn assert_protocol_call_omitting_a_default_is_an_arity_error(
    dir: &std::path::Path,
    entry: &std::path::Path,
    tag: &str,
) {
    let graph = crate::resolver::build_graph(entry).expect("resolve");
    let res = crate::checker::check_graph(&graph);
    let _ = std::fs::remove_dir_all(dir);
    let errs = res.expect_err("a protocol call omitting an argument must be rejected");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("'mprobe' expects 1 argument(s), got 0")),
        "[{tag}] expected the arity error, got: {errs:?}"
    );
}

/// **The hazard this row exists to delete.** A method default declared in module `a` and reached
/// from module `z` that does not import `a` used to be CLONED into `z` and resolved in `z`'s scope,
/// so `z`'s own `av()` won: the call printed `510` where `a`'s author wrote `1` (+ the receiver's
/// 10). A clean `chezzi check`, identical — parity is structurally blind
/// to it. Measured on `0104d57b`: **`510`**. The equivalent CPython program, two module objects with
/// `z` never importing `a`, prints **`11`**, because Python binds a default in the scope that
/// DECLARES it.
///
/// It now prints `11`: the default is compiled once, in `a`, and the call site reaches that provider
/// through a direct call-time reference (`Op::MakeFuncIn`) instead of needing an import edge it
/// cannot have — `a` imports `z` for the protocol, so `z` importing `a` would be a cycle.
/// TICKET-187 (supersedes DEC-075): a protocol call binds by the protocol's own parameter list, so
/// `p.mprobe()` omitting `x` is an arity error even though the implementor declares a default (Go:
/// `not enough arguments in call`, Rust: `E0061`).
#[test]
fn a_method_default_from_a_sibling_module_resolves_in_its_definer() {
    for (tag, entry_src) in [
        ("zfirst", "import z\nimport a\nprint(z.use_it(a.S(10)))\n"),
        ("afirst", "import a\nimport z\nprint(z.use_it(a.S(10)))\n"),
    ] {
        let dir =
            std::env::temp_dir().join(format!("chezzi_w751_sibling_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.chz"),
            "fn av() -> int:\n    return 1\nstruct S:\n    n: int\n    fn mprobe(self, x: int = av()) -> int:\n        return x + self.n\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("z.chz"),
            "protocol HasM:\n    fn mprobe(self, x: int) -> int\nfn av() -> int:\n    return 500\nfn use_it(p: HasM) -> int:\n    return p.mprobe()\n",
        )
        .unwrap();
        let entry = dir.join("main.chz");
        std::fs::write(&entry, entry_src).unwrap();
        assert_protocol_call_omitting_a_default_is_an_arity_error(&dir, &entry, tag);
    }
}

/// …and the shape it all exists for: the ordinary protocol/implementation split. `z` declares the
/// protocol and the function that consumes it, `a` declares the struct that satisfies it and gives
/// the method a default. `z` cannot import `a` (that is the dependency direction the protocol
/// exists to avoid, and `a` importing `z` would make it a cycle), so REFUSING here has no
/// workaround other than "never default a protocol method's argument" — which is why the refusal
/// shipped between `e2d9bd4e` and `dfdc7a1b` was reverted.
///
/// Measured: `b1307258` prints `12`, the equivalent Python prints `12`, and `dfdc7a1b` refused with
/// `cannot use the default for 'x' of 'S.mprobe' here: it is declared in module 'a', which module
/// 'z' does not import`. Both import orders. Here `u.av()` is reachable from `z` as
/// well, so this case agreed with the definer even under the old clone; its sibling below is the
/// one that could not.
/// TICKET-187 (supersedes DEC-075): a protocol call binds by the protocol's own parameter list, so
/// `p.mprobe()` omitting `x` is an arity error even though the implementor declares a default (Go:
/// `not enough arguments in call`, Rust: `E0061`).
#[test]
fn a_protocol_call_omitting_an_implementors_default_is_an_arity_error() {
    for (tag, entry_src) in [
        ("zfirst", "import z\nimport a\nprint(z.use(a.S(1)))\n"),
        ("afirst", "import a\nimport z\nprint(z.use(a.S(1)))\n"),
    ] {
        let dir =
            std::env::temp_dir().join(format!("chezzi_w751_proto_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("u.chz"), "fn av() -> int:\n    return 11\n").unwrap();
        std::fs::write(
            dir.join("a.chz"),
            "import u\nstruct S:\n    v: int\n    fn mprobe(self, x: int = u.av()) -> int:\n        return self.v + x\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("z.chz"),
            "import u\nprotocol P:\n    fn mprobe(self, x: int) -> int\nfn use(p: P) -> int:\n    return p.mprobe()\n",
        )
        .unwrap();
        let entry = dir.join("main.chz");
        std::fs::write(&entry, entry_src).unwrap();
        assert_protocol_call_omitting_a_default_is_an_arity_error(&dir, &entry, tag);
    }
}

/// The old clone's CEILING, now removed. Because the default was resolved in the CALLER, a name the
/// caller could not see was a compile error: drop `import u` from `z` in the program above and
/// `b1307258` and `0104d57b` both reported **`in module 'z': unknown name 'u'`** — refusing a
/// program whose definer is perfectly well-formed, purely because the consumer of a protocol cannot
/// see the implementor's imports.
///
/// It now compiles and prints `12`. `z` resolves nothing: the default is compiled once, in `a`,
/// where `u` IS imported, and reached by a direct call-time reference. This is the clearest
/// user-visible consequence of the change — a whole class of program that could not be written now
/// can be.
/// TICKET-187 (supersedes DEC-075): a protocol call binds by the protocol's own parameter list, so
/// `p.mprobe()` omitting `x` is an arity error even though the implementor declares a default (Go:
/// `not enough arguments in call`, Rust: `E0061`).
#[test]
fn a_default_naming_an_import_the_caller_cannot_see_now_compiles_and_resolves_in_its_definer() {
    for (tag, entry_src) in [
        ("zfirst", "import z\nimport a\nprint(z.use(a.S(1)))\n"),
        ("afirst", "import a\nimport z\nprint(z.use(a.S(1)))\n"),
    ] {
        let dir =
            std::env::temp_dir().join(format!("chezzi_w751_unseen_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("u.chz"), "fn av() -> int:\n    return 11\n").unwrap();
        std::fs::write(
            dir.join("a.chz"),
            "import u\nstruct S:\n    v: int\n    fn mprobe(self, x: int = u.av()) -> int:\n        return self.v + x\n",
        )
        .unwrap();
        // NOTE: no `import u` here — that is the whole point.
        std::fs::write(
            dir.join("z.chz"),
            "protocol P:\n    fn mprobe(self, x: int) -> int\nfn use(p: P) -> int:\n    return p.mprobe()\n",
        )
        .unwrap();
        let entry = dir.join("main.chz");
        std::fs::write(&entry, entry_src).unwrap();
        assert_protocol_call_omitting_a_default_is_an_arity_error(&dir, &entry, tag);
    }
}

/// **`f := g; f()` — the shape the deleted `docs/design/roadmap.md` §3d existed for, and the one no call-site rewrite can
/// reach.** `desugar` never rewrites a call through a first-class function VALUE (a local `Ident`
/// callee resolves to no signature), so the omitted default cannot be filled by the caller; the
/// CALLEE fills it, from its own prologue, in its own module. Measured on `0104d57b`:
/// `'closure' expects 1 argument(s), got 0`.
///
/// The neighbours matter as much as the case: supplying the optional argument must still work, a
/// partial suffix must fill only what was omitted, a genuinely missing REQUIRED argument must still
/// be refused, and too many arguments must still be refused.
#[test]
fn a_default_fills_through_a_first_class_function_value() {
    let out = run_capture(
        "fn av() -> int:\n    return 4\nfn g(x: int = av()) -> int:\n    return x\nf := g\nprint(f())\nprint(f(9))\n",
    )
    .expect("run");
    assert_eq!(out, "4\n9\n");

    // A partial suffix: only the omitted tail is filled.
    let out = run_capture(
        "fn g(a: int, x: int = 4, y: int = 5) -> int:\n    return a * 100 + x * 10 + y\nf := g\nprint(f(1))\nprint(f(1, 2))\nprint(f(1, 2, 3))\n",
    )
    .expect("run");
    assert_eq!(out, "145\n125\n123\n");

    // The defaults are the DEFINER's, evaluated per call, on a worker as well as on the host.
    let out = run_capture(
        "fn av() -> int:\n    return 4\nfn g(x: int = av()) -> int:\n    return x\nfn main():\n    ch := Channel[int](1)\n    f := g\n    parallel:\n        spawn:\n            ch.send(f())\n    print(ch.recv())\nmain()\n",
    )
    .expect("run");
    assert_eq!(out, "4\n");
}

/// The last expression still copied into a caller, and its removal. A default whose type names
/// `Self` on a GENERIC host (`Q[T]`) cannot become a free provider `fn` — `Self` is `Q[T]` and `T` is
/// unbound outside the signature — so it kept the inline clone, and with it caller-scope resolution.
/// Measured on `cd9d609b` (i.e. AFTER the cross-module fix), a definer whose `mkl()` prints
/// `definer's mkl` and a caller declaring its own: the CALLER's ran. The callee's prologue owns it
/// now, so the definer's does.
#[test]
fn a_generic_host_self_default_resolves_in_its_definer() {
    let dir = std::env::temp_dir().join(format!("chezzi_carveout_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("a.chz"),
        "struct G[T]:\n    v: T\n    fn all(self, xs: List[Self] = mkl()) -> int:\n        return xs.len()\nfn mkl[T]() -> List[G[T]]:\n    print(\"definer\")\n    return []\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import G from a\nfn mkl[T]() -> List[G[T]]:\n    print(\"caller\")\n    return []\nprint(G(1).all())\n",
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("must type-check, got: {errs:?}");
    }
    let (vo, _e, vr, _c) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert_eq!(
        vo, "definer\n0\n",
        "the generic-host `Self` default must resolve in its DEFINER, not the caller"
    );
}

/// **The M:N half, and the one that a `chezzi check` cannot see.** Here the out-of-closure default
/// is evaluated inside a `spawn` in a `parallel:` nursery, on a worker whose `module_objs` table is
/// the run's (M3.1). It resolves because the provider's own frame reads its home through
/// `Op::GetGlobalSlot` on the definer's one module object.
/// TICKET-187 (supersedes DEC-075): a protocol call binds by the protocol's own parameter list, so
/// `p.mprobe()` omitting `x` is an arity error even though the implementor declares a default (Go:
/// `not enough arguments in call`, Rust: `E0061`).
#[test]
fn an_out_of_closure_default_resolves_on_an_m_n_worker() {
    let dir = std::env::temp_dir().join(format!("chezzi_w751_worker_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("u.chz"), "fn av() -> int:\n    return 11\n").unwrap();
    std::fs::write(
        dir.join("a.chz"),
        "import u\nstruct S:\n    v: int\n    fn mprobe(self, x: int = u.av()) -> int:\n        return self.v + x\n",
    )
    .unwrap();
    // `z` imports neither `a` nor `u`, and evaluates the default on a spawned worker.
    std::fs::write(
        dir.join("z.chz"),
        "protocol P:\n    fn mprobe(self, x: int) -> int\nfn use(p: P) -> int:\n    ch := Channel[int](1)\n    parallel:\n        spawn:\n            ch.send(p.mprobe())\n    return ch.recv()\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(&entry, "import z\nimport a\nprint(z.use(a.S(1)))\n").unwrap();
    assert_protocol_call_omitting_a_default_is_an_arity_error(&dir, &entry, "-");
}

/// The definer's module is still RUNNING when its own default is first needed: `a`'s toplevel calls
/// through `z` on a line ABOVE the `fn av()` the default names. It resolves, because top-level `fn`s
/// are hoisted — the compiler emits every `MakeFunc`/`DefineGlobalSlot` before the module body's
/// other statements — so the provider and the name it spells are both bound before any toplevel
/// expression runs. Pinned because the lazy reference is what made this shape reachable at all, and
/// because it is the closest thing to a load-order counterexample that exists.
/// TICKET-187 (supersedes DEC-075): a protocol call binds by the protocol's own parameter list, so
/// `p.mprobe()` omitting `x` is an arity error even though the implementor declares a default (Go:
/// `not enough arguments in call`, Rust: `E0061`).
#[test]
fn an_out_of_closure_default_resolves_while_its_definers_toplevel_is_running() {
    let dir = std::env::temp_dir().join(format!("chezzi_w751_midrun_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("z.chz"),
        "protocol P:\n    fn mprobe(self, x: int) -> int\nfn use(p: P) -> int:\n    return p.mprobe()\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("a.chz"),
        "import z\nstruct S:\n    v: int\n    fn mprobe(self, x: int = av()) -> int:\n        return self.v + x\nprint(z.use(S(1)))\nfn av() -> int:\n    return 11\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(&entry, "import a\n").unwrap();
    assert_protocol_call_omitting_a_default_is_an_arity_error(&dir, &entry, "-");
}

/// **The relaxed call-site typing is not a bypass.** An out-of-closure provider call is typed from
/// the parameter slot it fills rather than by resolving the definer's signature; that must not leak
/// into the slots the USER wrote. An explicit, wrong-typed argument to the same method is still a
/// type error, at the call site, with the ordinary message.
#[test]
fn an_explicit_argument_is_still_type_checked_where_a_default_would_be_relaxed() {
    let dir = std::env::temp_dir().join(format!("chezzi_w751_nobypass_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("u.chz"), "fn av() -> int:\n    return 11\n").unwrap();
    std::fs::write(
        dir.join("a.chz"),
        "import u\nstruct S:\n    v: int\n    fn mprobe(self, x: int = u.av()) -> int:\n        return self.v + x\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("z.chz"),
        "protocol P:\n    fn mprobe(self, x: int) -> int\nfn use(p: P) -> int:\n    return p.mprobe()\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(&entry, "import z\nimport a\nprint(a.S(1).mprobe(\"x\"))\n").unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    let errs = crate::checker::check_graph(&graph).expect_err("a str where an int is declared");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        errs.iter()
            .any(|e| e.message.contains("expected int, found str")),
        "expected the ordinary argument type error, got: {errs:?}"
    );
    assert!(
        errs.iter().all(|e| !e
            .message
            .contains(crate::base::provider_name::PROVIDER_PREFIX)),
        "an internal provider symbol reached a user-facing message: {errs:?}"
    );
}

/// The half of the premise the fallback must not overstate: give `z` its own `av()` returning the
/// same `1` that `a`'s does, and the clone reads THAT one and still prints `11` — the right answer,
/// by name coincidence. `b1307258` printed `11` here too. Recorded so nobody reads the fallback as
/// "the caller's name is the intended one": it is intended only in that refusing costs more.
/// TICKET-187 (supersedes DEC-075): a protocol call binds by the protocol's own parameter list, so
/// `p.mprobe()` omitting `x` is an arity error even though the implementor declares a default (Go:
/// `not enough arguments in call`, Rust: `E0061`).
#[test]
fn a_default_whose_name_the_caller_also_declares_reads_the_definers() {
    for (tag, entry_src) in [
        ("zfirst", "import z\nimport a\nprint(z.use_it(a.S(10)))\n"),
        ("afirst", "import a\nimport z\nprint(z.use_it(a.S(10)))\n"),
    ] {
        let dir =
            std::env::temp_dir().join(format!("chezzi_w751_coincide_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.chz"),
            "fn av() -> int:\n    return 1\nstruct S:\n    n: int\n    fn mprobe(self, x: int = av()) -> int:\n        return x + self.n\n",
        )
        .unwrap();
        // z declares an `av` too, and it agrees with a's. Under the old clone THIS one was read
        // (right answer, wrong reason); now a's own is read. Kept so nobody re-reads the caller's
        // name as the intended one.
        std::fs::write(
            dir.join("z.chz"),
            "protocol HasM:\n    fn mprobe(self, x: int) -> int\nfn av() -> int:\n    return 1\nfn use_it(p: HasM) -> int:\n    return p.mprobe()\n",
        )
        .unwrap();
        let entry = dir.join("main.chz");
        std::fs::write(&entry, entry_src).unwrap();
        assert_protocol_call_omitting_a_default_is_an_arity_error(&dir, &entry, tag);
    }
}

/// The neighbour of the tests above: add `import a` to `z` and the very same call is legal in BOTH
/// entry import orders, because `a` is now a genuine dependency of `z`. `b1307258` printed `510`
/// here too (z's `av`); the answer is `11` — `a`'s own `av()` (1) plus 10 — which is what `a`'s
/// author wrote. Verified on the release binary in both orders.
#[test]
fn a_method_default_from_an_imported_module_resolves_in_that_module() {
    for (tag, entry_src) in [
        ("zfirst", "import z\nimport a\nprint(z.use_it(a.S(10)))\n"),
        ("afirst", "import a\nimport z\nprint(z.use_it(a.S(10)))\n"),
    ] {
        a_method_default_from_an_imported_module_case(tag, entry_src);
    }
}

fn a_method_default_from_an_imported_module_case(tag: &str, entry_src: &str) {
    let dir =
        std::env::temp_dir().join(format!("chezzi_w751_ancestor_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("a.chz"),
        "fn av() -> int:\n    return 1\nstruct S:\n    n: int\n    fn mprobe(self, x: int = av()) -> int:\n        return x + self.n\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("z.chz"),
        "import a\nfn av() -> int:\n    return 500\nfn use_it(p: a.S) -> int:\n    return p.mprobe()\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(&entry, entry_src).unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert_eq!(vo, "11\n", "[{tag}] a's own av() is 1, not z's 500");
}

/// W7-51 item 5 — a provider frame in a stack trace is rendered as what it is. Its internal name is
/// deliberately unspellable (`$def$2$f.x$`), which also makes it unreadable; `desugar::display_fn_name`
/// turns it back into a phrase at `capture_trace`, so every trace consumer sees the readable form.
#[test]
fn a_provider_frame_in_a_trace_is_readable() {
    let dir = std::env::temp_dir().join(format!("chezzi_w751_trace_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "fn boom() -> int:\n    return [1][9]\nfn f(x: int = boom()) -> int:\n    return x\nprint(f())\n",
    )
    .unwrap();
    let (_o, _e, res, _c) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    let e = res.expect_err("the default faults");
    let trace = format_trace(&e);
    assert!(
        trace.contains("at <default for 'x' of 'f'>"),
        "expected a readable provider frame, got:\n{trace}"
    );
    assert!(
        !trace.contains(crate::base::provider_name::PROVIDER_PREFIX),
        "the internal `$def$` name must not reach a user-visible trace:\n{trace}"
    );
}

/// TICKET-048 (W8-14 residual) — a nursery-deadlock fault's headline must name a real file and
/// position, not the placeholder `Span::RUNTIME` used at `run_mn_nursery_outermost`'s
/// nursery deadlock sites. It rendered `line 1, col 1` with no file, unlike an ordinary in-task
/// fault one line away. Since the deadlock listing, the headline is the first blocked task's own
/// wait (this test and the two below).
#[test]
fn nursery_deadlock_headline_has_a_real_position() {
    let dir = std::env::temp_dir().join(format!("chezzi_t048_deadlock_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "ch := Channel[int](1)\nfn main():\n    parallel:\n        spawn:\n            for i in 0..5: ch.send(i)\n        spawn: print(ch.recv())\nmain()\n",
    )
    .unwrap();
    let (_o, _e, res, _c) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    let e = res.expect_err("the deadlock must fault");
    let trace = format_trace(&e);
    assert!(
        !trace.starts_with("runtime error (line 1, col 1):"),
        "expected a real file/position on the deadlock headline, got:\n{trace}"
    );
    assert!(
        trace.contains("main.chz:5:28): deadlock: 1 task is blocked"),
        "expected the headline to name the first blocked task's own wait, got:\n{trace}"
    );
}

/// TICKET-048 — a bare `spawn` inside a fn body opens an IMPLICIT nursery with no keyword of its
/// own; its deadlock headline must name the fn body's first statement, not `line 1, col 1`.
#[test]
fn implicit_fn_nursery_deadlock_headline_names_the_body_start() {
    let dir = std::env::temp_dir().join(format!("chezzi_t048_implicit_fn_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "fn main():\n    a := Channel[int]()\n    spawn: a.recv()\n    spawn: a.recv()\nmain()\n",
    )
    .unwrap();
    let (_o, _e, res, _c) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    let e = res.expect_err("the deadlock must fault");
    let trace = format_trace(&e);
    assert!(
        !trace.starts_with("runtime error (line 1, col 1):"),
        "expected a real file/position on the deadlock headline, got:\n{trace}"
    );
    assert!(
        trace.contains("main.chz:3:12): deadlock: 2 tasks are blocked"),
        "expected the headline to name the first blocked task's own wait, got:\n{trace}"
    );
}

/// TICKET-048 — a bare `spawn` at module top level opens an IMPLICIT nursery with no keyword of
/// its own; its deadlock headline must name the module's first statement, not `line 1, col 1`.
#[test]
fn implicit_module_nursery_deadlock_headline_names_the_first_statement() {
    let dir = std::env::temp_dir().join(format!("chezzi_t048_implicit_mod_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "# a comment line\n\na := Channel[int]()\nspawn: a.recv()\nspawn: a.recv()\n",
    )
    .unwrap();
    let (_o, _e, res, _c) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    let e = res.expect_err("the deadlock must fault");
    let trace = format_trace(&e);
    assert!(
        !trace.starts_with("runtime error (line 1, col 1):"),
        "expected a real file/position on the deadlock headline, got:\n{trace}"
    );
    assert!(
        trace.contains("main.chz:4:8): deadlock: 2 tasks are blocked"),
        "expected the headline to name the first blocked task's own wait, got:\n{trace}"
    );
}

/// W7-51 — a cross-module default is still evaluated PER CALL (Chezzi's documented divergence from
/// Python's evaluate-at-def-time), and the counter it bumps is the DEFINER's, reached through the
/// provider's `home` module rather than the caller's namespace.
#[test]
fn a_cross_module_default_is_evaluated_per_call_in_the_definers_module() {
    let dir = std::env::temp_dir().join(format!("chezzi_w751_percall_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("g.chz"),
        "n := 0\nfn bump() -> int:\n    n = n + 1\n    return n\nfn f(x: int = bump()) -> int:\n    return x\nfn seen() -> int:\n    return n\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import g\nn := 100\nprint(g.f())\nprint(g.f())\nprint(g.seen())\nprint(n)\n",
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert_eq!(
        vo, "1\n2\n2\n100\n",
        "two omitting calls bump g's own counter twice; main's same-named global is untouched"
    );
}

/// Entry-last backstop — when the ENTRY file IS the always-injected prelude stub
/// (`chezzi run std/prelude.chz`), the resolver dedups the entry's own visit and the entry-last
/// reorder must restore `modules.last() == entry` so the positional-entry consumers designate the
/// right module. The stub is side-effect-free (no top-level output, no test fns), so it
/// must run clean with empty stdout —
/// byte-identical. Proves "runs clean, no panic" against a literal golden.
#[test]
fn entry_is_always_linked_stub_runs_clean() {
    let manifest = crate::native::std_embed::checkout_root();
    let entry = manifest.join("std").join("prelude.chz");
    let (vo, _ve, vr, _vc) = run_file(&entry);
    assert!(vr.is_ok(), "VM faulted on prelude.chz: {vr:?}");
    assert_eq!(vo, "", "VM stdout not empty on prelude.chz");
}

/// Task 4 — `import std.concurrency` then construct + use ALL FOUR runtime concurrency ctors must
/// RUN end-to-end byte-identically. Exercises the native EMPTY-members module-object alloc + the
/// opcode-dispatched ctors (the import gate is checker-only, so runtime is unchanged).
#[test]
fn concurrency_whole_module_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_conc_whole_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
            &entry,
            "import Atomic, Executor, RwShared, Shared from std.concurrency\nfn main():\n    s := Shared(0)\n    s.set(5)\n    r := RwShared(1)\n    a := Atomic(2)\n    ex := Executor()\n    ex.shutdown()\n    print(s.get())\n    print(r.get())\n    print(a.load())\nmain()\n",
        )
        .unwrap();
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    let expected = "5\n1\n2\n";
    assert_eq!(vo, expected, "VM output");
}

/// Task 4 — CRITICAL FIX 2: a SELECTIVE `import Shared from std.concurrency` must RUN.
/// This is the exact case the prior attempt crashed: the from-import type-checks green but
/// the engine `bind_import` would fault `module 'std.concurrency' has no member 'Shared'` without
/// the runtime skip. The ctor is resolved by the compiler name→opcode dispatch, not a bound member.
#[test]
fn concurrency_from_import_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_conc_from_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import Shared from std.concurrency\nfn main():\n    print(Shared(0).get())\nmain()\n",
    )
    .unwrap();
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted (bind_import skip missing?): {vr:?}");
    assert_eq!(vo, "0\n", "VM output");
}

/// Gate `Socket`/`Listener` behind `import std.net` — a `Socket` carries no runtime module-member
/// value (the runtime resolves `Ty::Socket` directly; the ctor is `connect`/`listen`), so a
/// from-import (`import Socket from std.net`) would fault `module 'std.net' has no member 'Socket'`
/// WITHOUT the `bind_import` skip. This pins the skip plus a whole-module twin. Phase 4c-net: the bodies now also call `Socket`/`Listener` METHODS
/// (`read`/`write`/`accept`/`close`) so the harvested method table (the retired bespoke arm's
/// replacement) is exercised at check time; the `use_*` fns are checked though never
/// called (no live I/O — the method resolution is a front-end concern).
#[test]
fn net_from_import_runs() {
    for src in [
        "import Socket from std.net\nfn use_sock(s: Socket) -> int!:\n    a := s.read(64)?\n    n := s.write(a)?\n    s.close()\n    return ?n\nfn main():\n    print(1)\nmain()\n",
        "import Listener, Socket from std.net\nfn use_sock(s: Socket) -> int!:\n    a := s.read(64, 100)?\n    n := s.write(a, 100)?\n    s.close()\n    return ?n\nfn use_listener(l: Listener) -> str!:\n    c := l.accept()?\n    ad := l.addr()?\n    c.close()\n    l.close()\n    return ?ad\nfn main():\n    print(1)\nmain()\n",
    ] {
        let dir = std::env::temp_dir().join(format!("chezzi_vm_net_from_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let entry = dir.join("main.chz");
        std::fs::write(&entry, src).unwrap();
        let (vo, _ve, vr, _vc) = run_file(&entry);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(vr.is_ok(), "VM faulted (bind_import skip missing?): {vr:?}");
        assert_eq!(vo, "1\n", "VM output");
    }
}

/// Gate `timer` behind `import std.time` — a whole-module `import std.time` then `timer(50).recv()`
/// must RUN end-to-end byte-identically. The import gate is checker-only, so the opcode-dispatched `timer` runtime is unchanged;
/// the timer fires after 50ms and `recv()` yields `true`.
#[test]
fn timer_whole_module_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_timer_whole_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import timer from std.time\nfn main():\n    print(timer(50).recv())\nmain()\n",
    )
    .unwrap();
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert_eq!(vo, "true\n", "VM output");
}

/// CRITICAL — a SELECTIVE `import timer from std.time` must RUN. `timer` is opcode-
/// backed with NO runtime module-member value, so the from-import type-checks green but would fault `module 'std.time' has no member 'timer'` WITHOUT the timer-specific `bind_import`
/// skip. The call resolves via the compiler name→opcode dispatch, not a bound member.
#[test]
fn timer_from_import_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_timer_from_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import timer from std.time\nfn main():\n    print(timer(50).recv())\nmain()\n",
    )
    .unwrap();
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted (bind_import skip missing?): {vr:?}");
    assert_eq!(vo, "true\n", "VM output");
}

/// Regression (blocker): a module-QUALIFIED struct type (`cdefs.DivT`) written at the extern
/// return boundary must lower to its C struct just like the bare/named-import spelling. The
/// qualified `Type::Qualified` was previously passed through unchanged → `ctype_of` returned
/// `None` → silent void return → reading a field of the (nil) result faulted. It
/// must yield quot=3, rem=2 for `div(17, 5)`. Linux-only (needs libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_qualified_return_struct_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qret_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(
        core.join("cdefs.chz"),
        "import int32 from std.ffi\n\nstruct DivT:\n    quot: int32\n    rem: int32\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import core.cdefs\nimport int32 from std.ffi\n\nextern \"libc.so.6\":\n    \
             fn div(numer: int32, denom: int32) -> cdefs.DivT\n\nr: cdefs.DivT = div(17, 5)\n\
             print(r.quot)\nprint(r.rem)\n",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on qualified return struct extern: {vm_res:?}"
    );
    assert_eq!(vm_out, "3\n2\n");
}

/// Regression (blocker): a module-QUALIFIED width alias (`w3.Len`, `type Len = int32`) at both
/// the extern PARAM and return boundary must resolve through the alias table like the bare/
/// named-import spelling. Previously the qualified param panicked the VM at the marshal loop's
/// `.expect("checker verified marshallable param")`. It must yield 7 for
/// `abs(-7)`. Linux-only (needs libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_qualified_width_alias_param_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qparam_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(
        core.join("w3.chz"),
        "import int32 from std.ffi\n\ntype Len = int32\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
            &entry,
            "import core.w3\n\nextern \"libc.so.6\":\n    fn abs(n: w3.Len) -> w3.Len\n\nprint(abs(-7))\n",
        )
        .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted/panicked on qualified width-alias param extern: {vm_res:?}"
    );
    assert_eq!(vm_out, "7\n");
}
