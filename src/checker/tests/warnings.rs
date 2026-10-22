//! Checker tests: warnings: severity channel, discarded Result/Option, airlock staleness.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== the severity channel (no rule emits a warning yet — Tasks 1/2 wire the first two) =====

/// `warn` and `error` land in DIFFERENT vectors, share the module attribution, and render with their
/// own prefixes. This is the whole contract the warning rules are built on: one that leaked into
/// `errors` would fail the build, and one that rendered as `type error (...)` would be
/// indistinguishable from a real error.
#[test]
fn warn_is_a_separate_non_fatal_channel() {
    let span = Span {
        line: 3,
        col: 7,
        file: 0,
    };
    let mut c = Checker::new();
    c.warn(span, "hello");
    c.error(span, "boom");
    assert_eq!(c.errors.len(), 1);
    assert_eq!(c.warnings.len(), 1);
    assert_eq!(c.errors[0].severity, Severity::Error);
    assert_eq!(c.warnings[0].severity, Severity::Warning);
    // Byte-identical to what an error has always rendered as — hundreds of tests, the LSP and
    // `--errors=json` all key on this string.
    assert_eq!(c.errors[0].to_string(), "type error (line 3, col 7): boom");
    assert_eq!(c.warnings[0].to_string(), "warning (line 3, col 7): hello");

    // A warning raised while checking an imported module names it, exactly like an error.
    c.current_module_label = Some("core.db".to_string());
    c.warn(span, "attributed");
    c.error(span, "attributed");
    assert_eq!(c.warnings[1].message, "in module 'core.db': attributed");
    assert_eq!(c.errors[1].message, "in module 'core.db': attributed");
}

/// The speculative-inference idiom — mark, run a probe re-inference, roll back — must cover the
/// WARNING channel too. A dozen sites in the checker probe an expression only to learn its type and
/// then discard what the probe reported, because the real path re-runs it and reports for keeps; a
/// warning left behind by such a probe (or by an abandoned branch) double-reports. Written at the
/// `Checker` level because no rule warns yet: with `diag_rollback` truncating `errors` alone, the
/// first assertion below sees 2 warnings.
#[test]
fn diag_rollback_discards_speculative_warnings_and_keeps_the_rest() {
    let span = Span {
        line: 1,
        col: 1,
        file: 0,
    };
    let mut c = Checker::new();
    // Recorded BEFORE the speculative window — must survive the rollback.
    c.warn(span, "kept");
    c.error(span, "kept");
    let mark = c.diag_mark();
    c.warn(span, "speculative");
    c.error(span, "speculative");
    c.diag_rollback(mark);
    assert_eq!(
        c.warnings.len(),
        1,
        "the speculative warning must be rolled back, got: {:?}",
        c.warnings
    );
    assert_eq!(c.errors.len(), 1, "got: {:?}", c.errors);
    assert_eq!(c.warnings[0].message, "kept");
    assert_eq!(c.errors[0].message, "kept");
    // A rollback to a mark taken on empty channels clears both.
    let zero = Checker::new().diag_mark();
    c.diag_rollback(zero);
    assert!(c.warnings.is_empty() && c.errors.is_empty());
}

/// The graph entry point keeps the channels apart: warnings never reach the `Result`, and an
/// erroring program's `Err` carries only `Severity::Error`.
///
/// The third case is the one that pins the PLUMBING: a warning program returns `Ok(())` **alongside a
/// non-empty warning vec**, so replacing `check_graph_diags`' `let warnings = std::mem::take(&mut
/// c.warnings);` with `Vec::new()` now fails here. It has to be spelled out at THIS entry point —
/// `warns`/`no_warn` go through the single-module `check_diags`, which is a different (test-only)
/// wire, and the CLI / LSP / test runner all land on this one.
#[test]
fn check_graph_diags_keeps_warnings_out_of_the_result() {
    let t = TmpDir::new();

    let clean = t.write("clean.chz", "print(1)\n");
    let graph = crate::resolver::build_graph(&clean).expect("resolve should succeed");
    let (res, warns) = check_graph_diags(&graph, None);
    assert!(res.is_ok(), "clean program should check clean: {res:?}");
    assert!(warns.is_empty(), "clean program warns: {warns:?}");

    let bad = t.write("bad.chz", "x: int = \"s\"\nprint(x)\n");
    let graph = crate::resolver::build_graph(&bad).expect("resolve should succeed");
    let (res, warns) = check_graph_diags(&graph, None);
    let errs = res.expect_err("should not type-check");
    assert!(errs.iter().all(|e| e.severity == Severity::Error));
    assert!(warns.is_empty(), "erroring program warns: {warns:?}");

    let warned = t.write(
        "warned.chz",
        "fn g() -> int!Error:\n    return ?1\nfn f():\n    g()\nf()\n",
    );
    let graph = crate::resolver::build_graph(&warned).expect("resolve should succeed");
    let (res, warns) = check_graph_diags(&graph, None);
    assert!(res.is_ok(), "a warning must not fail the check: {res:?}");
    assert_eq!(warns.len(), 1, "got: {warns:?}");
    assert_eq!(warns[0].severity, Severity::Warning);
    assert!(warns[0].message.contains("is discarded"), "got: {warns:?}");
}

// ===== W8-2 — the discarded-`Result`/`Option` warning (`sig.rs`, the `StmtKind::Expr` arm) =====

/// A `Result`/`Option` dropped on the floor **where it is genuinely lost** — inside a non-top-level
/// proto, the positions filed as `docs/contributing/gaps.md` W8-2. Rust owns both carriers, marks them
/// `#[must_use]`, and warns on the drop. Every case here still type-checks CLEAN (that is `warns`'
/// first assertion), so the exit code is unchanged. The top-level counterpart, which now warns
/// too since the runtime no longer checks a top-level drop, is `a_top_level_drop_warns`.
#[test]
fn discarded_carrier_warns() {
    let g = "fn g() -> int!Error:\n    return ?1\n";
    let o = "fn o() -> int?:\n    return ?1\n";
    // Inside a function body — the position that is silent today.
    warns(
        &format!("{g}fn f():\n    g()\nf()\n"),
        "the `int!` value returned by 'g' is discarded",
    );
    warns(
        &format!("{o}fn f():\n    o()\nf()\n"),
        "the `int?` value returned by 'o' is discarded",
    );
    // A `spawn:` block and a `defer:` block each compile to their OWN child proto, so a drop in one
    // is swallowed even at MODULE TOP LEVEL (measured: both print the following statement and exit
    // 0). These are the two cases an `in_fn_body`-only gate would wrongly suppress — `in_fn_body` is
    // deliberately not set at a `spawn:` block boundary.
    warns(
        &format!("{g}parallel:\n    spawn:\n        g()\n"),
        "the `int!` value returned by 'g' is discarded",
    );
    warns(
        &format!("{g}defer:\n    g()\nprint(1)\n"),
        "the `int!` value returned by 'g' is discarded",
    );
    // …and the same two nested inside a function, their neighbours.
    warns(
        &format!("{g}fn f():\n    parallel:\n        spawn:\n            g()\nf()\n"),
        "the `int!` value returned",
    );
    warns(
        &format!("{g}fn f():\n    defer:\n        g()\n    print(1)\nf()\n"),
        "the `int!` value returned",
    );
    // Nested inside a function, both block kinds — the neighbours of the fn-body case above.
    warns(
        &format!("{g}fn f():\n    if true:\n        g()\nf()\n"),
        "the `int!` value returned",
    );
    warns(
        &format!("{g}fn f():\n    for i in 0..1:\n        g()\nf()\n"),
        "the `int!` value returned",
    );
    // A method call names the METHOD, not the receiver expression.
    warns(
        "fn f():\n    xs := [1, 2]\n    xs.pop()\nf()\n",
        "the `int?` value returned by 'pop' is discarded",
    );
    // Not a call, so there is no callee to name — the message falls back to the position.
    warns(
        &format!("{o}fn f():\n    x := o()\n    x\nf()\n"),
        "the `int?` value here is discarded",
    );
    // Every message carries both escapes, or it is not actionable — and for a plain `g()` callee
    // the hint spells the call back rather than eliding it.
    warns(
        &format!("{g}fn f():\n    g()\nf()\n"),
        "bind it (`r := g()`)",
    );
    warns(
        &format!("{g}fn f():\n    g()\nf()\n"),
        "discard it explicitly (`_ := g()`)",
    );
    // A METHOD call keeps the elision: reconstructing the receiver expression would be guesswork.
    warns(
        "fn f():\n    xs := [1, 2]\n    xs.pop()\nf()\n",
        "bind it (`r := …`)",
    );
    // …as does a non-call carrier, which has no callee name at all.
    warns(
        &format!("{o}fn f():\n    x := o()\n    x\nf()\n"),
        "bind it (`r := …`)",
    );
    // A hint the user cannot TYPE is worse than no hint. The spelled-back form dropped the call's
    // ARGUMENTS, so `takes(1, "a")` suggested `r := takes()` — which is itself a type error
    // (`'takes' expects 2 argument(s), got 0`). Only a nullary plain-name call is reproducible from
    // the callee name alone; anything with arguments elides like the method-call case.
    warns(
        "fn takes(n: int, s: str) -> int!Error:\n    return ?n\nfn f():\n    takes(1, \"a\")\nf()\n",
        "bind it (`r := …`), or discard it explicitly (`_ := …`)",
    );
    // …and the subject still names the callee, which is what points at the culprit.
    warns(
        "fn takes(n: int, s: str) -> int!Error:\n    return ?n\nfn f():\n    takes(1, \"a\")\nf()\n",
        "the `int!` value returned by 'takes' is discarded",
    );
}

/// The scope correction, reversed by TICKET-038. **Module top level** used to be exempt because the
/// runtime checked a dropped carrier there (the top-level pop opcode's check, since deleted); warning "is
/// discarded" would have been factually false, and obeying it (`_ := g()`) DISABLED that check —
/// turning a failing program into a silent rc=0. The runtime no longer checks a top-level drop at
/// all, so the warning is now the ONLY check on it, and it fires in every position, top level
/// included — there is no longer a "does this statement lower into a non-top-level proto" gate.
///
/// Measured on the release binary with `g()` returning `Err`, each of these now WARNS on stderr and
/// exits **0**, printing whatever follows — the runtime abort is gone.
#[test]
fn a_top_level_drop_warns() {
    let g = "fn g() -> int!Error:\n    return ?1\n";
    let o = "fn o() -> int?:\n    return ?1\n";
    warns(&format!("{g}g()\n"), "is discarded — bind it");
    warns(&format!("{o}o()\n"), "is discarded — bind it");
    // Every block kind that can hold a statement WITHOUT opening a child proto.
    warns(&format!("{g}if true:\n    g()\n"), "is discarded — bind it");
    warns(
        &format!("{g}for i in 0..1:\n    g()\n"),
        "is discarded — bind it",
    );
    warns(
        &format!("{g}n := 0\nwhile n < 1:\n    n = n + 1\n    g()\n"),
        "is discarded — bind it",
    );
    warns(
        &format!("{g}match 1:\n    1:\n        g()\n    _:\n        pass\n"),
        "is discarded — bind it",
    );
    warns(
        &format!("{g}parallel:\n    g()\n"),
        "is discarded — bind it",
    );
    warns(
        &format!("{g}r := recover:\n    g()\n    1\nprint(r)\n"),
        "is discarded — bind it",
    );
    warns(
        &format!("{g}ch := Channel[int](1)\nch.send(1)\nwait:\n    v := ch.recv():\n        g()\n"),
        "is discarded — bind it",
    );
    // Two levels deep, still the top-level proto.
    warns(
        &format!("{g}if true:\n    for i in 0..1:\n        g()\n"),
        "is discarded — bind it",
    );
}

/// The escapes. `_` is an ordinary identifier in LET position (it is special-cased only in pattern
/// position and as a `wait:` arm target), so `_ := g()` parses as a plain `StmtKind::Let` and never
/// reaches the `StmtKind::Expr` arm at all — no special-casing in the rule, asserted rather than
/// assumed. Binding for real is the other escape.
#[test]
fn a_bound_carrier_does_not_warn() {
    let g = "fn g() -> int!Error:\n    return ?1\n";
    no_warn(&format!("{g}fn f():\n    _ := g()\nf()\n"));
    no_warn(&format!("{g}fn f():\n    r := g()\n    print(r)\nf()\n"));
    no_warn(&format!("{g}_ := g()\n"));
    // The neighbours: `_` shadowed/reassigned, and `_` in the nested positions that warn above.
    no_warn(&format!(
        "{g}fn f():\n    if true:\n        _ := g()\nf()\n"
    ));
    no_warn(&format!(
        "{g}fn f():\n    for _ in 0..1:\n        _ := g()\nf()\n"
    ));
    // A carrier consumed by `match` / `?` is handled, not discarded.
    no_warn(&format!(
        "{g}fn f():\n    match g():\n        ?v: print(v)\n        !e: print(e.message())\nf()\n"
    ));
    no_warn(&format!(
        "{g}fn f() -> int!Error:\n    v := g()?\n    return ?v\nprint(f())\n"
    ));
}

/// The four positions where a bare carrier expression is NOT a drop. Each is inferred off a path
/// other than `check_stmt`'s `StmtKind::Expr` arm, and a warning in any of them would fire on
/// correct, idiomatic code.
#[test]
fn a_carrier_that_is_not_discarded_does_not_warn() {
    let g = "fn g() -> int!Error:\n    return ?1\n";
    // 1. An inline-expr body implicitly RETURNS its expression (`check_fn_body`, not `check_stmt`).
    no_warn(&format!("{g}fn f() -> int!Error: g()\nprint(f())\n"));
    // …and the same body written as an explicit `return`, its neighbour.
    no_warn(&format!(
        "{g}fn f() -> int!Error:\n    return g()\nprint(f())\n"
    ));
    // 2. A `recover:` block has no tail value (TICKET-260), so its last statement is an ordinary
    //    statement: a carrier dropped there warns like any other, in a plain tail, a `match` arm
    //    and an `if` branch.
    let dropped = "the `int!` value returned by 'g' is discarded";
    warns(
        &format!("{g}fn f():\n    r := recover:\n        g()\n    print(r)\nf()\n"),
        dropped,
    );
    warns(
        &format!(
            "{g}fn f(n: int):\n    r := recover:\n        match n:\n            0: g()\n            _: g()\n    print(r)\nf(0)\n"
        ),
        dropped,
    );
    warns(
        &format!(
            "{g}fn f(n: int):\n    r := recover:\n        if n == 0:\n            g()\n        else:\n            g()\n    print(r)\nf(0)\n"
        ),
        dropped,
    );
    // The order matters: a carrier in a NON-final position of the same block is a real drop.
    warns(
        &format!("{g}fn f():\n    r := recover:\n        g()\n        1\n    print(r)\nf()\n"),
        "the `int!` value returned by 'g' is discarded",
    );
    // 3. `?` / `??` yield the UNWRAPPED payload, so the statement's type is not a carrier.
    no_warn(&format!(
        "{g}fn f() -> int!Error:\n    g()?\n    return ?2\nprint(f())\n"
    ));
    no_warn("fn o() -> int?:\n    return ?1\nfn f():\n    o() ?? 0\nf()\n");
    // `?.` is the exception in that family and DOES warn: optional chaining re-wraps, so
    // `o()?.len()` is an `int?` and dropping it drops a carrier. Rust agrees (`Option::map` is
    // `#[must_use]`). Pinned here, next to its siblings, because the shape reads like theirs.
    warns(
        "fn o() -> str?:\n    return ?\"a\"\nfn f():\n    o()?.len()\nf()\n",
        "the `int?` value here is discarded",
    );
    // 4. `defer` is its own statement arm and stays excluded on purpose — `defer f.Close()` is Go's
    //    canonical unchecked idiom, and the corpus has 27 of them.
    no_warn(&format!("{g}fn f():\n    defer g()\n    print(1)\nf()\n"));
    no_warn(&format!("{g}defer g()\nprint(1)\n"));
    // …and `spawn <call>`, the same fire-and-forget shape: a spawned task's return value is
    // discarded by construction (measured: `parallel:\n    spawn g()` with `g()` returning `Err`
    // prints the following statement and exits 0). Silent on purpose, like `defer`.
    no_warn(&format!("{g}parallel:\n    spawn g()\nprint(1)\n"));
}

/// The `-> nil` / `-> bool` methods a naive "bare method call statement" rule would drown in. None of
/// them returns a carrier, so none may warn — a false positive on `ch.send(v)` would cost the whole
/// diagnostic its credibility.
#[test]
fn a_non_carrier_statement_does_not_warn() {
    no_warn("fn f():\n    ch := Channel[int](1)\n    ch.send(1)\n    ch.close()\nf()\n");
    no_warn("fn f():\n    s := {1, 2}\n    s.remove(1)\nf()\n"); // Set.remove -> bool
    no_warn("fn f():\n    xs := [1]\n    xs.push(2)\n    print(xs)\nf()\n");
    no_warn("fn f():\n    print(1)\nf()\n");
    // A void call in the very positions that warn for a carrier.
    no_warn("fn v():\n    print(1)\nif true:\n    v()\nfor _ in 0..1:\n    v()\n");
}

/// An `Unknown`-typed expression statement stays silent: it has already produced a hard error, and a
/// warning on top of it is cascade noise. Cannot go through `no_warn` (which requires a clean check),
/// so it asserts on both channels directly.
#[test]
fn an_unknown_expression_statement_does_not_warn() {
    let (errs, warns) = warn_src("fn f():\n    nosuch()\nf()\n");
    assert!(!errs.is_empty(), "expected a hard error");
    assert!(warns.is_empty(), "expected no warning, got: {warns:?}");
}

/// The rule sits inside the checker's speculative-inference machinery: a function with NO declared
/// return type is inferred by a probe pass whose diagnostics are rolled back, then checked for real
/// in pass 2. Reporting once is the whole contract — `diag_rollback` truncating `errors` alone (its
/// shape before Task 0) makes this exactly 2.
#[test]
fn a_warning_under_return_inference_is_reported_once() {
    let src = "fn g() -> int!Error:\n    return ?1\n\
               fn f():\n    g()\nfn h():\n    f()\nh()\n";
    let (errs, warns) = warn_src(src);
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
    assert_eq!(
        warns.len(),
        1,
        "expected exactly one warning, got: {warns:?}"
    );
}

/// Exercise the `no_warn` helper itself on the simplest clean program.
#[test]
fn no_warn_on_a_clean_program() {
    no_warn("print(1)\n");
}

// ===== W8-3 — the airlock-staleness warning (a task-side write read after the join) =====
//
// Every shape below was MEASURED on the release binary before the rule was written; the warn/silent
// split is that table, not a guess. Task-side write → value after the join:
//   reassign 0 · push 0 · `xs[i]=v` 0 · `p.f=v` 1(unchanged) · `m[k]=v` 1(unchanged) ·
//   top-level spawn 0 · implicit nursery 0 · module global 0     → LOST, warn
//   `Shared.update` 1 · `Channel.send` 7 · `defer:` block push 1 · parent overwrite 2 → visible, silent
//
// The filed defect's failure mode is why it ranks: `for r in results:` over the stale empty list ran
// ZERO iterations, so every `assert` inside was skipped and the program exited 0.

/// D4 rule 1 rejects each direct task-body write to an airlock copy.
#[test]
fn a_task_side_write_read_after_the_join_is_an_error() {
    // Reassignment — the filed repro.
    rejects(
        "fn f():\n    results: List[str] = []\n    parallel:\n        spawn:\n            results = [\"a\"]\n    print(results.len())\nf()\n",
        "'results' is this task's copy",
    );
    // Mutator method call.
    rejects(
        "fn f():\n    xs: List[int] = []\n    parallel:\n        spawn:\n            xs.push(1)\n    print(xs.len())\nf()\n",
        "'xs' is this task's copy",
    );
    // Index assign, read WHOLE. A captured `List` deep-copies across the airlock, so the parent's
    // whole-value read observes the lost element write — the checker can tell, so it reports. (The
    // granular read `xs[0]` of this same granular write now warns too; see
    // `a_same_path_read_of_a_projected_task_write_warns`.)
    rejects(
        "fn f():\n    xs := [0]\n    parallel:\n        spawn:\n            xs[0] = 9\n    print(xs)\nf()\n",
        "'xs' is this task's copy",
    );
    // Field assign — a captured struct deep-copies across the airlock too.
    rejects(
        "struct P:\n    x: int\nfn f():\n    p := P(1)\n    parallel:\n        spawn:\n            p.x = 9\n    print(p)\nf()\n",
        "'p' is this task's copy",
    );
    // Map key assign.
    rejects(
        "fn f():\n    m := {\"a\": 1}\n    parallel:\n        spawn:\n            m[\"a\"] = 9\n    print(m)\nf()\n",
        "'m' is this task's copy",
    );
    // The other mixed pair: a WHOLE-binding task write, read back through a projection. The whole
    // copy is stale, so every field of it is — the checker can tell here too.
    rejects(
        "struct P:\n    x: int\n    s: str\nfn f():\n    p := P(1, \"a\")\n    parallel:\n        spawn:\n            p = P(9, \"z\")\n    print(p.s)\nf()\n",
        "'p' is this task's copy",
    );
    // An IMPLICIT nursery (a bare `spawn:` with no `parallel:`) is the same airlock.
    rejects(
        "fn f():\n    xs: List[int] = []\n    spawn:\n        xs.push(1)\n    print(xs.len())\nf()\n",
        "'xs' is this task's copy",
    );
}

/// D4 layer A (TICKET-170) — a direct write to a captured binding inside a `spawn:` task body must
/// be a COMPILE-TIME ERROR, not the W8-3 warning above. As of this commit the checker still only
/// warns on this shape (see `a_task_side_write_read_after_the_join_warns`'s first case, the exact
/// same program), so this pins the gap: it currently fails because `check_src` returns zero errors.
#[test]
fn spawn_body_direct_write_to_captured_binding_is_a_compile_time_error() {
    rejects(
        "fn f():\n    results: List[str] = []\n    parallel:\n        spawn:\n            results = [\"a\"]\n    print(results.len())\nf()\n",
        "'results' is this task's copy: a write to it would be lost at the join",
    );
}

/// D4 must follow a statically known free function's parameter write back to the task capture.
/// This currently returns no checker errors; the runtime faults at `xs.push(2)` instead.
#[test]
fn spawn_body_named_function_parameter_write_to_captured_binding_is_a_compile_time_error() {
    rejects(
        "fn bump(xs: List[int]):\n    xs.push(2)\n\nfn main():\n    xs := [1]\n    parallel:\n        spawn:\n            bump(xs)\n\nmain()\n",
        "'xs' is this task's copy: a write to it would be lost at the join",
    );
}

#[test]
fn d4_named_helper_transitive_and_recursive_writes() {
    rejects_help_at(
        "fn bump(xs: List[int], n: int):\n    xs.push(2)\n    if n > 0:\n        bump(xs, n - 1)\nfn relay(xs: List[int]):\n    bump(xs, 1)\nfn main():\n    xs := [1]\n    parallel:\n        spawn:\n            relay(xs)\nmain()\n",
        "'xs' is this task's copy: a write to it would be lost at the join",
        "push",
        11,
    );
    // TICKET-179: a write reached only under `if` is left to layer C, even when every branch
    // reaches it through recursion.
    ok(
        "fn bump(xs: List[int], n: int):\n    if n > 0:\n        bump(xs, n - 1)\n    else:\n        xs.push(2)\nfn relay(xs: List[int]):\n    bump(xs, 1)\nfn main():\n    xs := [1]\n    parallel:\n        spawn:\n            relay(xs)\nmain()\n",
    );
}

#[test]
fn d4_named_helper_loop_shadow_does_not_write_caller() {
    ok(
        "fn bump(xs: List[int]):\n    for xs in [[2]]:\n        xs.push(3)\nfn main():\n    xs := [1]\n    parallel:\n        spawn:\n            bump(xs)\nmain()\n",
    );
}

#[test]
fn d4_nested_named_callee_writes_outer_parameter() {
    rejects_help_at(
        "fn outer(xs: List[int]):\n    fn inner():\n        xs.push(2)\n    inner()\nfn main():\n    xs := [1]\n    parallel:\n        spawn:\n            outer(xs)\nmain()\n",
        "'xs' is this task's copy: a write to it would be lost at the join",
        "push",
        9,
    );
}

#[test]
fn d4_block_scoped_named_callee_writes_outer_parameter() {
    // TICKET-179: the call sits under `if`, so layer A declines it and leaves it to layer C.
    ok(
        "fn outer(xs: List[int]):\n    if true:\n        fn inner():\n            xs.push(2)\n        inner()\nfn main():\n    xs := [1]\n    parallel:\n        spawn:\n            outer(xs)\nmain()\n",
    );
}

#[test]
fn d4_rebound_block_scoped_helper_does_not_report_its_old_write() {
    ok(
        "fn read():\n    print(1)\nfn outer(xs: List[int]):\n    if true:\n        fn inner():\n            xs.push(2)\n        inner := read\n        inner()\nfn main():\n    xs := [1]\n    parallel:\n        spawn:\n            outer(xs)\nmain()\n",
    );
}

#[test]
fn d4_assigned_block_scoped_helper_does_not_report_its_old_write() {
    ok(
        "fn read():\n    print(1)\nfn outer(xs: List[int]):\n    if true:\n        fn inner():\n            xs.push(2)\n        inner = read\n        inner()\nfn main():\n    xs := [1]\n    parallel:\n        spawn:\n            outer(xs)\nmain()\n",
    );
}

#[test]
fn d4_inner_if_assignment_invalidates_outer_helper_write() {
    ok(
        "fn read():\n    print(1)\nfn outer(xs: List[int]):\n    fn inner():\n        xs.push(2)\n    if true:\n        inner = read\n    inner()\nfn main():\n    xs := [1]\n    parallel:\n        spawn:\n            outer(xs)\nmain()\n",
    );
}

#[test]
fn d4_inner_for_assignment_invalidates_outer_helper_write() {
    ok(
        "fn read():\n    print(1)\nfn outer(xs: List[int]):\n    fn inner():\n        xs.push(2)\n    for _ in 0..1:\n        inner = read\n    inner()\nfn main():\n    xs := [1]\n    parallel:\n        spawn:\n            outer(xs)\nmain()\n",
    );
}

#[test]
fn d4_nested_named_capture_write() {
    rejects_help_at(
        "fn main():\n    xs := [1]\n    fn bump():\n        xs.push(2)\n    parallel:\n        spawn:\n            bump()\nmain()\n",
        "'xs' is this task's copy: a write to it would be lost at the join",
        "push",
        7,
    );
}

#[test]
fn d4_named_global_and_imported_writes() {
    rejects_help_at(
        "xs := [1]\nfn bump():\n    xs.push(2)\nfn main():\n    parallel:\n        spawn:\n            bump()\nmain()\n",
        "'xs' is this task's copy: a write to it would be lost at the join",
        "push",
        7,
    );
    files_reject(
        &[
            ("lib_w.chz", "fn bump(xs: List[int]):\n    xs.push(2)\n"),
            (
                "main.chz",
                "import lib_w\nfn main():\n    xs := [1]\n    parallel:\n        spawn:\n            lib_w.bump(xs)\nmain()\n",
            ),
        ],
        "'xs' is this task's copy: a write to it would be lost at the join",
    );
}

#[test]
fn d4_spawn_named_callee_write() {
    rejects_help_at(
        "fn bump(xs: List[int]):\n    xs.push(2)\nfn main():\n    xs := [1]\n    parallel:\n        spawn bump(xs)\nmain()\n",
        "'xs' is this task's copy: a write to it would be lost at the join",
        "push",
        6,
    );
}

#[test]
fn d4_executor_submit_named_capture_write_and_read_only() {
    let errs = check_entry(
        "import Executor from std.concurrency\nfn main():\n    xs := [1]\n    fn bump():\n        xs.push(2)\n    ex := Executor()\n    ex.submit(bump)\n    ex.shutdown()\nmain()\n",
    );
    assert!(
        errs.iter().any(|e| e
            .message
            .contains("'xs' is this task's copy: a write to it would be lost at the join")
            && e.help.as_deref().is_some_and(|h| h.contains("push"))
            && e.span.line == 7),
        "expected submit-argument error with push help at line 7, got: {errs:?}"
    );
    entry_ok(
        "import Executor from std.concurrency\nfn main():\n    xs := [1]\n    fn read():\n        print(xs.len())\n    ex := Executor()\n    ex.submit(read)\n    ex.shutdown()\nmain()\n",
    );
}

/// Each row must stay clean if a named write runs on an ordinary value, or never runs by name.
#[test]
fn d4_named_helper_valid_calls_and_indirection() {
    for src in [
        // The same helper may write an ordinary parent-side local.
        "fn bump(xs: List[int]):\n    xs.push(2)\nfn main():\n    xs := [1]\n    bump(xs)\nmain()\n",
        // The task creates its own unmarked copy before the call.
        "fn bump(xs: List[int]):\n    xs.push(2)\nfn main():\n    xs := [1]\n    parallel:\n        spawn:\n            ys := xs.copy()\n            bump(ys)\nmain()\n",
        // A Channel handle writes shared state, not a task copy.
        "fn put(c: Channel[int]):\n    c.send(2)\nfn main():\n    c := Channel[int](1)\n    parallel:\n        spawn:\n            put(c)\nmain()\n",
        // Reading a function value does not execute its body.
        "fn bump(xs: List[int]):\n    xs.push(2)\nfn main():\n    xs := [1]\n    parallel:\n        spawn:\n            f := bump\n            print(f)\nmain()\n",
        // An indirect call stays behind the runtime copy-mark backstop.
        "fn bump(xs: List[int]):\n    xs.push(2)\nfn main():\n    xs := [1]\n    parallel:\n        spawn:\n            f := bump\n            f(xs)\nmain()\n",
        // A recursive call with no write must not acquire an effect.
        "fn read(xs: List[int], n: int) -> int:\n    if n == 0:\n        return xs.len()\n    return read(xs, n - 1)\nfn main():\n    xs := [1]\n    parallel:\n        spawn:\n            print(read(xs, 2))\nmain()\n",
    ] {
        ok(src);
    }
}

/// Removing D4 rule 1 makes this source return zero matching errors. Speculative checking must not
/// report the same write twice.
#[test]
fn d4_rule1_reports_each_write_once() {
    let src = "fn f():\n    results: List[str] = []\n    parallel:\n        spawn:\n            results = [\"a\"]\n    print(results.len())\nf()\n";
    let errs = check_src(src);
    let matching = errs
        .iter()
        .filter(|e| e.message.contains("'results' is this task's copy"))
        .count();
    assert_eq!(matching, 1, "expected one D4 error, got: {errs:?}");
}

/// Removing each corresponding write hook makes that row return zero D4 errors.
#[test]
fn d4_rule1_every_task_body_write_form_is_an_error() {
    let rows = [
        (
            "fn f():\n    n := 0\n    parallel:\n        spawn:\n            n = 5\nf()\n",
            "n",
        ),
        (
            "fn f():\n    n := 0\n    parallel:\n        spawn:\n            n += 1\nf()\n",
            "n",
        ),
        (
            "fn f():\n    xs := [1]\n    parallel:\n        spawn:\n            xs.push(2)\nf()\n",
            "xs",
        ),
        (
            "fn f():\n    xs := [1]\n    parallel:\n        spawn:\n            xs[0] = 2\nf()\n",
            "xs",
        ),
        (
            "fn f():\n    xs := [[1]]\n    parallel:\n        spawn:\n            xs[0].push(2)\nf()\n",
            "xs",
        ),
        (
            "struct S:\n    v: int\nfn f():\n    s := S(1)\n    parallel:\n        spawn:\n            s.v = 2\nf()\n",
            "s",
        ),
        (
            "fn f():\n    m := {\"a\": 1}\n    parallel:\n        spawn:\n            m[\"a\"] += 1\nf()\n",
            "m",
        ),
        (
            "fn f():\n    xs := [1]\n    parallel:\n        spawn:\n            defer:\n                xs.push(2)\nf()\n",
            "xs",
        ),
        ("n := 0\nparallel:\n    spawn:\n        n = 5\n", "n"),
        (
            "n := 0\nfn main():\n    parallel:\n        spawn:\n            n = 5\nmain()\n",
            "n",
        ),
        (
            "fn f():\n    ch := Channel[int](1)\n    parallel:\n        spawn:\n            ch = Channel[int](2)\nf()\n",
            "ch",
        ),
        (
            "fn f():\n    parallel:\n        spawn:\n            ys := [1]\n            parallel:\n                spawn:\n                    ys.push(2)\nf()\n",
            "ys",
        ),
        (
            "struct B:\n    inner: List[int]\nfn f():\n    b := B([1])\n    parallel:\n        spawn:\n            b.inner[0] = 5\nf()\n",
            "b",
        ),
        (
            "struct Box[T]:\n    value: T\nfn f():\n    x := Box([1])\n    parallel:\n        spawn:\n            x.value[0] = 5\nf()\n",
            "x",
        ),
        (
            "fn f():\n    ba := bytearray(b\"a\")\n    parallel:\n        spawn:\n            ba.extend(b\"xy\")\nf()\n",
            "ba",
        ),
    ];
    for (src, name) in rows {
        rejects(src, &format!("'{name}' is this task's copy"));
    }
}

/// Each row is either not a lost write or uses a declared-type link D4 cannot prove. Tightening any
/// decline makes that row fail with an unexpected type error.
#[test]
fn d4_rule1_silent_set_stays_clean() {
    for src in [
        "fn f():\n    xs := [1]\n    parallel:\n        spawn:\n            print(xs[0])\n    print(xs[0])\nf()\n",
        "fn f():\n    parallel:\n        spawn:\n            ys := [1]\n            ys.push(2)\nf()\n",
        "fn f():\n    xs := [1]\n    parallel:\n        spawn:\n            ys := xs.copy()\n            ys.push(2)\n    print(xs[0])\nf()\n",
        "fn f():\n    ch := Channel[int](1)\n    parallel:\n        spawn:\n            ch.send(2)\nf()\n",
        "fn f():\n    xs := [1]\n    parallel:\n        spawn:\n            xs := [7]\n            xs.push(2)\n    print(xs[0])\nf()\n",
        "fn f():\n    xs := [1]\n    xs.push(2)\n    n := 0\n    n = 9\n    print(xs.len() + n)\nf()\n",
        "fn g(n: int):\n    print(n)\nfn f():\n    xs := [1, 2]\n    parallel:\n        spawn g(xs.pop() ?? 0)\nf()\n",
        "import IndexSet from std.ops\nfn put[C: IndexSet[int, int]](c: C):\n    parallel:\n        spawn:\n            c[0] = 5\nput([1])\n",
        "struct Grid:\n    data: List[List[int]]\n    fn index(self, k: int) -> List[int]:\n        return self.data[k]\nfn f():\n    g := Grid([[1, 2]])\n    parallel:\n        spawn:\n            g[0][1] = 5\nf()\n",
    ] {
        no_warn(src);
    }
    entry_no_warn(
        "import Shared from std.concurrency\nfn f():\n    sh := Shared(1)\n    parallel:\n        spawn:\n            sh.update(fn(x: int) -> int: x + 1)\nf()\n",
    );
    entry_no_warn(
        "import Shared from std.concurrency\nstruct T:\n    value: Shared[int]\n    fn index(self, k: int) -> int:\n        return self.value.get()\n    fn set_index(self, k: int, v: int):\n        self.value.set(v)\nfn f():\n    t := T(Shared(1))\n    parallel:\n        spawn:\n            t[0] = 5\nf()\n",
    );
}

/// Without the self-write fixed point, every row returns zero D4 errors. The transitive and mutual
/// recursion rows fail if inference stops after one pass.
#[test]
fn d4_user_method_writing_self_is_an_error_in_a_task() {
    let rows = [
        (
            "struct C:\n    n: int\n    fn bump(self):\n        self.n = self.n + 1\nfn f():\n    c := C(0)\n    parallel:\n        spawn:\n            c.bump()\nf()\n",
            "c",
        ),
        (
            "struct B:\n    v: int\n    fn index(self, k: int) -> int:\n        return self.v\n    fn set_index(self, k: int, v: int):\n        self.v = v\nfn f():\n    b := B(1)\n    parallel:\n        spawn:\n            b[0] = 5\nf()\n",
            "b",
        ),
        (
            "struct B:\n    xs: List[int]\n    fn add(self, v: int):\n        self.xs.push(v)\n    fn via(self, v: int):\n        self.add(v)\nfn f():\n    b := B([1])\n    parallel:\n        spawn:\n            b.via(3)\nf()\n",
            "b",
        ),
        (
            "struct C:\n    n: int\n    fn a(self):\n        self.b()\n    fn b(self):\n        if self.n > 0:\n            self.a()\n        self.n = self.n + 1\nfn f():\n    c := C(0)\n    parallel:\n        spawn:\n            c.a()\nf()\n",
            "c",
        ),
        (
            "struct Inner:\n    n: int\n    fn bump(self):\n        self.n = self.n + 1\nstruct Outer:\n    inner: Inner\n    fn go(self):\n        self.inner.bump()\nfn f():\n    o := Outer(Inner(0))\n    parallel:\n        spawn:\n            o.go()\nf()\n",
            "o",
        ),
        (
            "struct Inner:\n    n: int\n    fn bump(self):\n        self.n = self.n + 1\nstruct Outer:\n    inner: Inner\nfn f():\n    o := Outer(Inner(0))\n    parallel:\n        spawn:\n            o.inner.bump()\nf()\n",
            "o",
        ),
        (
            "struct Box[T]:\n    value: T\n    fn set(self, v: T):\n        self.value = v\nfn f():\n    b := Box(1)\n    parallel:\n        spawn:\n            b.set(2)\nf()\n",
            "b",
        ),
        (
            "struct C:\n    n: int\n    fn receive(self, ch: Channel[int]):\n        wait:\n            self.n = ch.recv():\n                print(self.n)\nfn f():\n    ch := Channel[int](1)\n    ch.send(2)\n    c := C(0)\n    parallel:\n        spawn:\n            c.receive(ch)\nf()\n",
            "c",
        ),
    ];
    for (src, name) in rows {
        rejects(src, &format!("'{name}' is this task's copy"));
    }
}

/// Each row contains a call the syntactic fixed point must decline. Descending through any listed
/// unknown edge makes the corresponding clean program fail.
#[test]
fn d4_user_method_write_inference_declines() {
    for src in [
        "struct C:\n    n: int\n    fn get(self) -> int:\n        return self.n\nfn f():\n    c := C(1)\n    parallel:\n        spawn:\n            print(c.get())\nf()\n",
        "struct C:\n    n: int\n    fn local(self):\n        x := 0\n        x = 1\n        print(x)\nfn f():\n    c := C(1)\n    parallel:\n        spawn:\n            c.local()\nf()\n",
        "G := 0\nstruct C:\n    fn global(self):\n        G = 1\nfn f():\n    c := C()\n    parallel:\n        spawn:\n            c.global()\nf()\n",
        "struct C:\n    inc: fn() -> None\nfn f():\n    c := C(fn(): print(1))\n    parallel:\n        spawn:\n            c.inc()\nf()\n",
        "protocol P:\n    fn bump(self)\nfn f(p: P):\n    parallel:\n        spawn:\n            p.bump()\n",
        "protocol P:\n    fn bump(self)\nfn f[T: P](v: T):\n    parallel:\n        spawn:\n            v.bump()\n",
        "struct C:\n    xs: List[int]\n    fn later(self):\n        f := fn(): self.xs.push(1)\n        f()\nfn run():\n    c := C([1])\n    parallel:\n        spawn:\n            c.later()\nrun()\n",
        "import Iterator from std.iter\nfn f(it: Iterator[int]):\n    parallel:\n        spawn:\n            print(it.next())\n",
    ] {
        no_warn(src);
    }
    entry_no_warn(
        "import Shared from std.concurrency\nstruct Counter:\n    n: Shared[int]\n    fn bump(self):\n        self.n.update(fn(v: int) -> int: v + 1)\nfn f():\n    c := Counter(Shared(0))\n    parallel:\n        spawn:\n            c.bump()\nf()\n",
    );
}

/// Without rule 3, each closure writes its named capture but crosses an executing task boundary
/// without a compile-time error.
#[test]
fn d4_rule3_writing_closure_crossing_a_task_is_an_error() {
    rejects(
        "fn f():\n    xs := [1]\n    g := fn(): xs.push(2)\n    parallel:\n        spawn:\n            g()\nf()\n",
        "'xs' is this task's copy",
    );
    rejects(
        "fn f():\n    xs := [1]\n    g := fn(): xs.push(2)\n    parallel:\n        spawn g()\nf()\n",
        "'xs' is this task's copy",
    );
    entry_rejects(
        "import Executor from std.concurrency\nfn f():\n    xs := [1]\n    ex := Executor()\n    ex.submit(fn(): xs.push(3))\n    ex.shutdown()\nf()\n",
        "'xs' is this task's copy",
    );
    entry_rejects(
        "import Executor from std.concurrency\nfn f():\n    xs := [1]\n    g := fn(): xs.push(3)\n    ex := Executor()\n    ex.submit(g)\n    ex.shutdown()\nf()\n",
        "'xs' is this task's copy",
    );
    rejects(
        "G := [1]\ng := fn(): G.push(2)\nparallel:\n    spawn:\n        g()\n",
        "'G' is this task's copy",
    );
    rejects(
        "fn f():\n    xs := [1]\n    parallel:\n        spawn (fn(): xs.push(2))()\nf()\n",
        "'xs' is this task's copy",
    );
}

/// These values either do not execute at the crossing or carry no provable captured write. Treating
/// a spawn argument or Channel send as execution makes the corresponding row fail.
#[test]
fn d4_rule3_declines() {
    for src in [
        "fn run(g: fn() -> None):\n    print(\"not called\")\nfn f():\n    xs := [1]\n    g := fn(): xs.push(3)\n    parallel:\n        spawn run(g)\n    print(xs)\nf()\n",
        "fn run(g: fn() -> None):\n    print(\"not called\")\nfn f():\n    xs := [1]\n    parallel:\n        spawn run(fn(): xs.push(3))\n    print(xs)\nf()\n",
        "fn f():\n    xs := [1]\n    ch := Channel[fn() -> None](1)\n    ch.send(fn(): xs.push(2))\n    g := ch.recv()\n    print(xs)\n    print(g)\nf()\n",
        "fn f():\n    xs := [1]\n    g := fn(): print(xs[0])\n    parallel:\n        spawn g()\nf()\n",
        "fn f():\n    xs := [1]\n    parallel:\n        spawn:\n            g := fn(): xs.push(2)\n            g()\n    print(xs)\nf()\n",
        "fn f():\n    xs := [1]\n    g := fn(): xs.push(2)\n    g = fn(): print(1)\n    parallel:\n        spawn g()\n    print(xs)\nf()\n",
    ] {
        no_warn(src);
    }
    entry_no_warn(
        "import Atomic, Executor from std.concurrency\nfn f():\n    seen := Atomic(0)\n    ex := Executor()\n    _ := ex.submit(fn(): seen.add(3))\n    ex.shutdown()\nf()\n",
    );
}

/// TICKET-137 (W14-25, owner decision D2) — the warning is TRUE by construction for a MODULE GLOBAL
/// too. A `spawn:` write to a global lands in the task's own copy, and a closure sent back over a
/// `Channel` reads the RECEIVER's copy, so neither the closure nor the later read sees the write —
/// exactly what the warning says. Pinned on the ticket's own repro shape (a global pushed inside a
/// task, a closure over it sent out, both read after the join); the runtime side is
/// `tests/chz/spec/airlock_received_closure_globals_test.chz::received_closure_reads_the_receivers_in_place_copy`,
/// which asserts the pair prints `[1] [1]`. If the runtime ever installs the sender's value again,
/// that test fails; if the warning stops firing here, this one does.
#[test]
fn a_module_global_written_in_a_task_warns_where_a_received_closure_reads_the_receivers_copy() {
    rejects(
        "import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency\ng: List[int] = [1]\nfn main():\n    c := Channel[fn() -> str](1)\n    parallel:\n        spawn:\n            g.push(2)\n            c.send(fn() -> str: \"{g}\")\n    f := c.recv()\n    print(\"{f()} {g}\")\nmain()\n",
        "'g' is this task's copy",
    );
}

/// A taint describes ONE binding, and `spawn_stale` is keyed by bare name — so a taint recorded on a
/// BLOCK-LOCAL SHADOW must die with that block rather than be charged to the outer binding of the
/// same name. Both were measured warning on the pre-fix binary while printing the CORRECT answer,
/// which is the rule's "under-warning, never over-warning" invariant negated, not a documented
/// ceiling. `pop_scope` now drops every entry whose owning scope is the one going away.
#[test]
fn a_shadows_taint_is_not_charged_to_the_outer_binding() {
    // The outer `xs` is a different list; the task wrote the SHADOW. Correctly prints 2.
    rejects(
        "fn f():\n    xs := [10, 20]\n    if true:\n        xs := [1]\n        parallel:\n            spawn:\n                xs.push(99)\n    print(xs.len())\nf()\n",
        "'xs' is this task's copy",
    );
    // The outer binding is an `int` that never entered a task at all. Correctly returns 42.
    rejects(
        "fn f() -> int:\n    n := 41\n    if true:\n        n := [1]\n        parallel:\n            spawn:\n                n.push(99)\n    return n + 1\nprint(f())\n",
        "'n' is this task's copy",
    );
    // The premise cuts one way only. A read of the SHADOW, inside the shadow's own scope, is a read
    // of the binding the task actually wrote — it still reports (measured: prints 1).
    rejects(
        "fn f():\n    xs := [10, 20]\n    if true:\n        xs := [1]\n        parallel:\n            spawn:\n                xs.push(99)\n        print(xs.len())\nf()\n",
        "'xs' is this task's copy",
    );
    // …and an intervening block that does NOT shadow leaves the outer taint intact (measured: the
    // read prints the stale 2). The cut is scope-keyed, not "any block boundary clears it".
    rejects(
        "fn f():\n    xs := [10, 20]\n    if true:\n        parallel:\n            spawn:\n                xs.push(99)\n    print(xs.len())\nf()\n",
        "'xs' is this task's copy",
    );
    // A block that pops BETWEEN the task write and the read must not clear a fn-body-scope taint.
    rejects(
        "fn f():\n    xs: List[int] = []\n    parallel:\n        spawn:\n            xs.push(1)\n    if true:\n        print(\"mid\")\n    print(xs.len())\nf()\n",
        "'xs' is this task's copy",
    );
}

/// The narrowed SEVENTH ceiling (TICKET-165) — a granular read whose path is DISJOINT from the
/// granular write's path (a different field, a different constant key, or a computed key) declines,
/// because the checker cannot tell whether the two overlap. This is NOT "any granular read of a
/// granular write" any more: the SAME field or key now warns (see
/// `a_same_path_read_of_a_projected_task_write_warns`). `correct > silent > wrong`: when the paths
/// might or might not overlap, it declines.
#[test]
fn a_granular_read_of_a_granular_task_write_declines() {
    // Different field. Correctly prints "bob".
    rejects(
        "struct P:\n    count: int\n    name: str\nfn f(p: P):\n    parallel:\n        spawn:\n            p.count = p.count + 1\n    print(p.name)\nf(P(0, \"bob\"))\n",
        "'p' is this task's copy",
    );
    // The decisive form: still silent once the user has applied the warning's OWN advice and carried
    // the value out on a Channel. A warning that fires after its own fix is applied is the worst
    // possible outcome for the rule's credibility.
    rejects(
        "struct P:\n    count: int\n    name: str\nfn f(p: P, ch: Channel[int]):\n    parallel:\n        spawn:\n            p.count = p.count + 1\n            ch.send(p.count)\n    print(p.name)\n    print(ch.recv())\nf(P(0, \"bob\"), Channel[int](4))\n",
        "'p' is this task's copy",
    );
    // Different map key — the write side's own stated example, now answered the same way on the read
    // side. Correctly prints 7.
    rejects(
        "fn f(m: Map[str, int]):\n    parallel:\n        spawn:\n            m[\"a\"] = 1\n    print(m[\"b\"])\nm := {\"a\": 0, \"b\": 7}\nf(m)\n",
        "'m' is this task's copy",
    );
    // The decline is per-BINDING, not a blanket "inside a projection" mute: a stale `i` read within
    // the INDEX EXPRESSION of a shielded `m[...]` still reports.
    rejects(
        "fn f(m: Map[str, int], i: int):\n    parallel:\n        spawn:\n            m[\"a\"] = 1\n            i = 3\n    print(m[str(i)])\nm := {\"a\": 0, \"0\": 7}\nf(m, 0)\n",
        "'i' is this task's copy",
    );
    // Only a MUTATOR write is whole-container, so a granular read of one still reports: every member
    // of `mutates_receiver` is a read-modify-write over the whole container.
    rejects(
        "fn f():\n    xs := [0]\n    parallel:\n        spawn:\n            xs.push(9)\n    print(xs[0])\nf()\n",
        "'xs' is this task's copy",
    );
}

/// Build the fn-body row shape shared by the TICKET-165 airlock table tests: `PRE` (the `T`/`S`
/// structs the table's INIT strings construct against) + `fn main():` + `init` + a `parallel: /
/// spawn:` block holding `writes` (12-space indent) + `reads` after the join (4-space indent).
fn airlock_src(init: &str, writes: &[&str], reads: &[&str]) -> String {
    const PRE: &str =
        "struct T:\n    v: int\nstruct S:\n    v: int\n    w: int\n    xs: List[int]\n    t: T\n";
    let mut src = format!("{PRE}fn main():\n{init}    parallel:\n        spawn:\n");
    for w in writes {
        src.push_str(&format!("            {w}\n"));
    }
    for r in reads {
        src.push_str(&format!("    {r}\n"));
    }
    src.push_str("main()\n");
    src
}

/// TICKET-165 (W11-13) — the gate is keyed on the READ SHAPE (a bare whole-binding interpolation)
/// rather than on "a captured binding written inside `spawn:` is read after the join". Measured on
/// the release binary at 00af4e3b: a task-side `s.v = 2` read back via `print(s.v)` prints `1` (the
/// pre-spawn value, genuinely stale — CPython's `threading` equivalent prints `2`), the same as the
/// `print("{s}")` shape that already warns. `a_granular_read_of_a_granular_task_write_declines`
/// above is about a DIFFERENT field/key than the one written — this is the SAME field, where the
/// checker can in fact tell, and must not decline.
#[test]
fn a_granular_read_of_the_same_field_a_granular_task_write_wrote_warns() {
    rejects(
        "struct S:\n    v: int\nfn f():\n    s := S(1)\n    parallel:\n        spawn:\n            s.v = 2\n    print(s.v)\nf()\n",
        "'s' is this task's copy",
    );
}

/// TICKET-165 rows W1-W11: a read along the SAME constant field/index/key path a task write wrote
/// warns, whether the read is a bare field, a nested field, a compound RMW, or a second write to a
/// DIFFERENT field ahead of the overlapping one (W6, which must cite the overlapping write's line,
/// not the first write's).
#[test]
fn a_same_path_read_of_a_projected_task_write_warns() {
    const MK: &str = "    s := S(1, 5, [1], T(1))\n";
    const XS: &str = "    xs := [[1], [7]]\n";
    const M: &str = "    m := {\"a\": 1, \"b\": 7}\n";
    const TS: &str = "    ts := [T(1)]\n";
    // W1
    rejects(
        &airlock_src(MK, &["s.v = 2"], &["print(s.v)"]),
        "'s' is this task's copy",
    );
    // W2
    rejects(
        &airlock_src(MK, &["s.v = 2"], &["print(\"{s.v}\")"]),
        "'s' is this task's copy",
    );
    // W3
    rejects(
        &airlock_src(MK, &["s.t.v = 2"], &["print(s.t.v)"]),
        "'s' is this task's copy",
    );
    // W4
    rejects(
        &airlock_src(MK, &["s.t.v = 2"], &["print(s.t)"]),
        "'s' is this task's copy",
    );
    // W5
    rejects(
        &airlock_src(MK, &["s.v += 1"], &["print(s.v)"]),
        "'s' is this task's copy",
    );
    // W6 — a DIFFERENT field write (line 12) precedes the overlapping one (line 13); the report
    // must cite the overlapping write, not the first.
    let w6 = airlock_src(MK, &["s.w = 2", "s.v = 2"], &["print(s.v)"]);
    rejects(&w6, "'s' is this task's copy");
    // W7
    rejects(
        &airlock_src(XS, &["xs[0] = [2]"], &["print(xs[0])"]),
        "'xs' is this task's copy",
    );
    // W8
    rejects(
        &airlock_src(XS, &["xs[0] = [2]"], &["print(xs[0][0])"]),
        "'xs' is this task's copy",
    );
    // W9
    rejects(
        &airlock_src(M, &["m[\"a\"] = 2"], &["print(m[\"a\"])"]),
        "'m' is this task's copy",
    );
    // W10
    rejects(
        &airlock_src(M, &["m[\"a\"] += 1"], &["print(m[\"a\"])"]),
        "'m' is this task's copy",
    );
    // W11
    rejects(
        &airlock_src(TS, &["ts[0].v = 2"], &["print(ts[0].v)"]),
        "'ts' is this task's copy",
    );
}

/// TICKET-165 rows W12-W19: a mutator called on an element or field of a binding (`xs[0].push(2)`,
/// `s.xs.push(2)`, `m["a"].push(3)`) is a WRITE through the root, so a read of it — whole, of the
/// element itself, or via a follow-on parent-side mutator (W19) — warns.
#[test]
fn a_mutator_on_a_projected_receiver_is_a_task_write() {
    const MK: &str = "    s := S(1, 5, [1], T(1))\n";
    const XS: &str = "    xs := [[1], [7]]\n";
    const ML: &str = "    m := {\"a\": [1]}\n";
    // W12
    rejects(
        &airlock_src(XS, &["xs[0].push(2)"], &["print(xs)"]),
        "'xs' is this task's copy",
    );
    // W13
    rejects(
        &airlock_src(XS, &["xs[0].push(2)"], &["print(\"{xs}\")"]),
        "'xs' is this task's copy",
    );
    // W14
    rejects(
        &airlock_src(XS, &["xs[0].push(2)"], &["print(xs[0])"]),
        "'xs' is this task's copy",
    );
    // W15
    rejects(
        &airlock_src(XS, &["xs[0].push(2)"], &["print(xs[0].len())"]),
        "'xs' is this task's copy",
    );
    // W16
    rejects(
        &airlock_src(MK, &["s.xs.push(2)"], &["print(s.xs)"]),
        "'s' is this task's copy",
    );
    // W17
    rejects(
        &airlock_src(MK, &["s.xs.push(2)"], &["print(s)"]),
        "'s' is this task's copy",
    );
    // W18
    rejects(
        &airlock_src(ML, &["m[\"a\"].push(3)"], &["print(m[\"a\"])"]),
        "'m' is this task's copy",
    );
    // W19 — the parent-side follow-on mutator is itself a read-modify-write; it observes the stale
    // task write before superseding it.
    rejects(
        &airlock_src(XS, &["xs[0].push(2)"], &["xs[0].push(3)", "print(xs)"]),
        "'xs' is this task's copy",
    );
}

/// TICKET-165: the warning fires at every position a task write can occur — module top level, a
/// nested `parallel:`, and a `spawn:` inside a loop — for both a whole-field write (P1-P3) and a
/// projected-mutator write on a list element (P4-P5).
#[test]
fn a_projected_task_write_warns_at_every_position() {
    const PRE: &str =
        "struct T:\n    v: int\nstruct S:\n    v: int\n    w: int\n    xs: List[int]\n    t: T\n";
    const MK: &str = "    s := S(1, 5, [1], T(1))\n";
    // P1 — module top level.
    rejects(
        &format!(
            "{PRE}s := S(1, 5, [1], T(1))\nparallel:\n    spawn:\n        s.v = 2\nprint(s.v)\n"
        ),
        "'s' is this task's copy",
    );
    // P2 — nested parallel:/spawn:.
    rejects(
        &format!(
            "{PRE}fn main():\n{MK}    parallel:\n        spawn:\n            parallel:\n                spawn:\n                    s.v = 2\n    print(s.v)\nmain()\n"
        ),
        "'s' is this task's copy",
    );
    // P3 — spawn: inside a loop.
    rejects(
        &format!(
            "{PRE}fn main():\n{MK}    for _ in 0..2:\n        spawn:\n            s.v = 2\n    print(s.v)\nmain()\n"
        ),
        "'s' is this task's copy",
    );
    // P4 — module top level, projected mutator on a list element.
    rejects(
        "xs := [[1]]\nparallel:\n    spawn:\n        xs[0].push(2)\nprint(xs[0])\n",
        "'xs' is this task's copy",
    );
    // P5 — spawn: inside a loop, projected mutator.
    rejects(
        "fn main():\n    xs := [[1]]\n    for _ in 0..2:\n        spawn:\n            xs[0].push(2)\n    print(xs)\nmain()\n",
        "'xs' is this task's copy",
    );
}

/// TICKET-165 rows S1-S5: a read through a DIFFERENT constant field or key than the one a task
/// wrote is correctly silent — the checker can tell the two paths never overlap.
#[test]
fn a_disjoint_path_read_of_a_projected_task_write_stays_silent() {
    const MK: &str = "    s := S(1, 5, [1], T(1))\n";
    const XS: &str = "    xs := [[1], [7]]\n";
    const M: &str = "    m := {\"a\": 1, \"b\": 7}\n";
    // S1
    rejects(
        &airlock_src(XS, &["xs[0] = [2]"], &["print(xs[1])"]),
        "'xs' is this task's copy",
    );
    // S2
    rejects(
        &airlock_src(M, &["m[\"a\"] = 2"], &["print(m[\"b\"])"]),
        "'m' is this task's copy",
    );
    // S3
    rejects(
        &airlock_src(MK, &["s.xs.push(2)"], &["print(s.v)"]),
        "'s' is this task's copy",
    );
    // S4
    rejects(
        &airlock_src(XS, &["xs[0].push(2)"], &["print(xs[1])"]),
        "'xs' is this task's copy",
    );
    // S5
    rejects(
        &airlock_src(MK, &["s.t.v = 2"], &["print(s.w)"]),
        "'s' is this task's copy",
    );
}

/// TICKET-165 rows C1-C2: the narrowed seventh ceiling. A computed or negative index cannot be
/// matched against a constant path, so it declines rather than guess — both print a stale value on
/// today's binary and stay silent by design.
#[test]
fn an_unmatchable_projected_path_declines() {
    const XS: &str = "    xs := [[1], [7]]\n";
    // C1 — a computed index on the write side.
    rejects(
        &airlock_src(
            &format!("{XS}    n := 1\n"),
            &["xs[n - 1] = [2]"],
            &["print(xs[0])"],
        ),
        "'xs' is this task's copy",
    );
    // C2 — a negative index literal on the read side.
    rejects(
        &airlock_src("    xs := [1]\n", &["xs[0] = 2"], &["print(xs[-1])"]),
        "'xs' is this task's copy",
    );
}

/// TICKET-165 rows H1-H5, L1-L3: a projected write through a handle type (`Shared`/`RwShared`/
/// `Atomic`/`Channel`, bare or behind a field) carries the write for real, so it never taints; a
/// task-local `:=` shadow is a different binding than the outer one of the same name; and a read
/// before the nursery opens sees no write at all.
#[test]
fn a_projected_write_through_a_handle_a_task_local_or_before_the_nursery_stays_silent() {
    const MK: &str = "    s := S(1, 5, [1], T(1))\n";
    const XS: &str = "    xs := [[1], [7]]\n";
    // H1 — Shared. `entry_no_warn` (not `no_warn`): `build_graph` is the only wire that resolves a
    // native `std.concurrency` import (`checker-test-helper-key-divergence`).
    entry_no_warn(
        "import Shared from std.concurrency\nfn main():\n    hs := [Shared[int](1)]\n    parallel:\n        spawn:\n            hs[0].set(2)\n    print(hs[0].get())\nmain()\n",
    );
    // H2 — RwShared.
    entry_no_warn(
        "import RwShared from std.concurrency\nfn main():\n    hs := [RwShared[int](1)]\n    parallel:\n        spawn:\n            hs[0].set(2)\n    print(hs[0].get())\nmain()\n",
    );
    // H3 — Atomic.
    entry_no_warn(
        "import Atomic from std.concurrency\nfn main():\n    hs := [Atomic[int](1)]\n    parallel:\n        spawn:\n            hs[0].add(1)\n    print(hs[0].load())\nmain()\n",
    );
    // H4 — Channel (a global reserved type, no import needed).
    no_warn(
        "fn main():\n    chs := [Channel[int](1)]\n    parallel:\n        spawn:\n            chs[0].send(7)\n    print(chs[0].recv())\nmain()\n",
    );
    // H5 — a Shared behind a struct field.
    entry_no_warn(
        "import Shared from std.concurrency\nstruct H:\n    h: Shared[int]\nfn main():\n    s := H(Shared[int](1))\n    parallel:\n        spawn:\n            s.h.set(2)\n    print(s.h.get())\nmain()\n",
    );
    // L1 — the task re-declares both bindings locally with `:=`; its writes are to the SHADOW.
    no_warn(&airlock_src(
        &format!("{MK}{XS}"),
        &[
            "s := S(9, 9, [9], T(9))",
            "s.v = 2",
            "xs := [[5]]",
            "xs[0].push(2)",
            "print(s.v + xs[0].len())",
        ],
        &["print(s.v)", "print(xs[0])"],
    ));
    // L2 — the read is lexically BEFORE the nursery opens.
    rejects(
        &format!(
            "struct T:\n    v: int\nstruct S:\n    v: int\n    w: int\n    xs: List[int]\n    t: T\nfn main():\n{MK}    print(s.v)\n    parallel:\n        spawn:\n            s.v = 2\nmain()\n"
        ),
        "'s' is this task's copy",
    );
    // L3 — the task only READS the bindings; there is no write to taint.
    no_warn(&airlock_src(
        &format!("{MK}{XS}"),
        &["print(s.v + xs[0][0])"],
        &["print(s.v)", "print(xs[0])"],
    ));
}

/// At MODULE TOP LEVEL the binding is scope 0, and this is exactly where the sibling W8-2 rule stays
/// silent because the runtime backstops it. There is NO backstop here — measured, the program prints
/// 0 and exits 0 with nothing reported — so the rule uses `is_captured` (which includes scope 0)
/// rather than `is_local_capture`. Same for a module global captured by a task inside a fn.
#[test]
fn a_task_side_write_warns_at_module_top_level_and_on_a_module_global() {
    rejects(
        "results: List[int] = []\nparallel:\n    spawn:\n        results.push(1)\nprint(results.len())\n",
        "'results' is this task's copy",
    );
    rejects(
        "g: List[int] = []\nfn f():\n    parallel:\n        spawn:\n            g.push(1)\n    print(g.len())\nf()\n",
        "'g' is this task's copy",
    );
}

/// The false-positive guard, and the highest-value case in the rule: the handle types cross the
/// airlock BY HANDLE, so a task-side write through one IS visible after the join (measured:
/// `Shared` 1, `Channel` 7). Telling someone their correct `Shared` code is broken is worse than the
/// missing warning, so `mutates_receiver` is keyed on the receiver TYPE and none of them is in it.
#[test]
fn a_write_through_a_handle_type_never_warns() {
    entry_no_warn(
        "import Shared from std.concurrency\nfn f():\n    s := Shared(0)\n    parallel:\n        spawn:\n            s.update(fn(x: int) -> int: x + 1)\n    print(s.get())\nf()\n",
    );
    entry_no_warn(
        "fn f():\n    ch := Channel[int](1)\n    parallel:\n        spawn:\n            ch.send(7)\n    print(ch.recv())\nf()\n",
    );
    entry_no_warn(
        "import RwShared from std.concurrency\nfn f():\n    s := RwShared(0)\n    parallel:\n        spawn:\n            s.set(3)\n    print(s.get())\nf()\n",
    );
    entry_no_warn(
        "import AtomicInt from std.concurrency\nfn f():\n    a := AtomicInt(0)\n    parallel:\n        spawn:\n            a.add(1)\n    print(a.load())\nf()\n",
    );
    // The `Shared` REWRITE of the filed repro — the very fix the warning's hint recommends must
    // itself be silent, or the diagnostic sends the user in a circle.
    entry_no_warn(
        "import Shared from std.concurrency\nfn f():\n    out := Shared(0)\n    parallel:\n        spawn:\n            out.set(2)\n    print(out.get())\nf()\n",
    );
}

/// Every "does not fire" claim, with the neighbours its premise implies (per
/// `widening-untested-by-its-own-suite`): swap the order, add an unused statement, move the read a
/// block deeper.
#[test]
fn the_airlock_warning_stays_silent_where_the_write_is_not_lost() {
    // A `defer:` block in the PARENT runs in the parent frame and shares the enclosing cell, so its
    // write is not a task write and taints nothing (measured: `defer: xs.push(1)` beside a later
    // `defer: print(xs.len())` prints 1). Its task-side twin — a `defer:` nested INSIDE the `spawn:` —
    // does warn; see `a_defer_inside_the_task_is_on_the_far_side_of_the_airlock`.
    no_warn(
        "fn f():\n    xs: List[int] = []\n    parallel:\n        spawn:\n            print(1)\n    defer:\n        xs.push(1)\n    print(xs.len())\nf()\n",
    );
    no_warn(
        "fn g(xs: List[int]):\n    fn inner():\n        defer:\n            xs.push(1)\n    inner()\n    print(xs.len())\ng([])\n",
    );
    // A task-LOCAL write (declared inside the task) is not a capture at all.
    no_warn(
        "fn f():\n    parallel:\n        spawn:\n            xs: List[int] = []\n            xs.push(1)\n            print(xs.len())\nf()\n",
    );
    // A captured write never read outside the task.
    rejects(
        "fn f():\n    xs: List[int] = []\n    parallel:\n        spawn:\n            xs.push(1)\n    print(1)\nf()\n",
        "'xs' is this task's copy",
    );
    // A read only INSIDE the task — the copy IS what is being read there.
    rejects(
        "fn f():\n    xs: List[int] = []\n    parallel:\n        spawn:\n            xs.push(1)\n            print(xs.len())\nf()\n",
        "'xs' is this task's copy",
    );
    // A parent-side overwrite supersedes the lost write (measured: prints the parent's 2).
    rejects(
        "fn f():\n    xs: List[int] = []\n    parallel:\n        spawn:\n            xs.push(1)\n    xs = [5, 6]\n    print(xs.len())\nf()\n",
        "'xs' is this task's copy",
    );
    // …and a parent-side INDEX assign, which untaints before its own receiver read reports.
    rejects(
        "fn f():\n    xs := [0]\n    parallel:\n        spawn:\n            xs[0] = 9\n    xs[0] = 1\n    print(xs[0])\nf()\n",
        "'xs' is this task's copy",
    );
    // A nested `fn` declared inside the task writing its OWN local.
    no_warn(
        "fn f():\n    parallel:\n        spawn:\n            fn h():\n                ys: List[int] = []\n                ys.push(1)\n                print(ys.len())\n            h()\n    print(1)\nf()\n",
    );
    // NEIGHBOUR (order swap): the read placed BEFORE the spawn is a declared ceiling — lexical order,
    // not dataflow. Silent, and the write that follows taints nothing that is ever read.
    rejects(
        "fn f():\n    xs: List[int] = []\n    print(xs.len())\n    parallel:\n        spawn:\n            xs.push(1)\nf()\n",
        "'xs' is this task's copy",
    );
    // NEIGHBOUR (no spawn at all): the same statements without the task must stay silent.
    no_warn("fn f():\n    xs: List[int] = []\n    xs.push(1)\n    print(xs.len())\nf()\n");
    // NEIGHBOUR (a NON-mutating method inside the task): reading a capture in a task is not a write.
    no_warn(
        "fn f():\n    xs := [1]\n    parallel:\n        spawn:\n            print(xs.len())\n    print(xs.len())\nf()\n",
    );
    // NEIGHBOUR: `unique`/`dedup`/`merge` RETURN a new collection (`std/prelude.chz`) — the near
    // misses the mutator grep ruled out. Their result is discarded, which is fine here.
    no_warn(
        "fn f():\n    xs := [1, 1]\n    parallel:\n        spawn:\n            ys := xs.unique()\n            print(ys.len())\n    print(xs.len())\nf()\n",
    );
    // NEIGHBOUR: `str.reverse` shares a NAME with `List.reverse` but returns a new `str` — the reason
    // `mutates_receiver` is keyed on the receiver type, not the name alone.
    no_warn(
        "fn f():\n    s := \"ab\"\n    parallel:\n        spawn:\n            print(s.reversed())\n    print(s)\nf()\n",
    );
}

/// The read a block DEEPER than the spawn still reports — the taint lives on the checker, not on a
/// scope, so an `if`/`for` body after the join is not an escape.
#[test]
fn a_stale_read_nested_in_a_later_block_still_warns() {
    rejects(
        "fn f():\n    xs: List[int] = []\n    parallel:\n        spawn:\n            xs.push(1)\n    if true:\n        print(xs.len())\nf()\n",
        "'xs' is this task's copy",
    );
    // The filed failure mode itself: the `for` head reads the stale (empty) list.
    rejects(
        "fn f():\n    results: List[str] = []\n    parallel:\n        spawn:\n            results.push(\"a\")\n    for r in results:\n        print(r)\nf()\n",
        "'results' is this task's copy",
    );
}

/// Reported exactly ONCE. Two ways this rule could double-report: the checker's speculative
/// return-type inference walks a body whose diagnostics are rolled back and then walks it again for
/// real (`diag_rollback` truncates `warnings`, and the taint is re-derived on the second walk), and a
/// second read of the same name after the first report (the entry is consumed).
#[test]
fn the_airlock_warning_is_reported_exactly_once() {
    // No declared return type on `f` → the speculative inference pass runs over this body. `zz := 99`
    // is the NEIGHBOUR (an unused statement inserted between the write and the read): an unrelated
    // binding in between must neither be tainted nor untaint `xs`, and the count assertion is what
    // proves it.
    let src = "fn f():\n    xs: List[int] = []\n    parallel:\n        spawn:\n            xs.push(1)\n    zz := 99\n    print(zz)\n    print(xs.len())\n    print(xs.len())\n    n := xs.len()\n    print(n)\nf()\n";
    let errs = check_src(src);
    let matching = errs
        .iter()
        .filter(|e| e.message.contains("'xs' is this task's copy"))
        .count();
    assert_eq!(matching, 1, "expected one D4 error, got: {errs:?}");
}

/// A COMPOUND assign in the parent reads the stale binding before it writes it (`n += 1` after a
/// task-side `n = n + 1` measured 1, not 2), so it reports rather than silently untainting — unlike
/// the plain `=` form, whose neighbour test above asserts silence.
#[test]
fn a_parent_compound_assign_reads_the_stale_value_and_warns() {
    rejects(
        "fn f():\n    n := 0\n    parallel:\n        spawn:\n            n = n + 1\n    n += 1\n    print(n)\nf()\n",
        "'n' is this task's copy",
    );
    // Inside the task the compound assign reads the task's OWN copy — silent.
    rejects(
        "fn f():\n    n := 0\n    parallel:\n        spawn:\n            n += 1\n            print(n)\nf()\n",
        "'n' is this task's copy",
    );
}

/// …and never reported ZERO times. Reporting CONSUMES the taint, so a SPECULATIVE walk that reads the
/// name eats it and the rolled-back warning is gone for good. `ys := []` (an unrefined empty literal)
/// makes the receiver refine speculatively infer `ys.push(...)`'s argument — measured before
/// `diag_mark`/`diag_rollback` snapshotted the taint: the warning vanished, while the byte-identical
/// program with `ys` already concrete still warned.
#[test]
fn a_speculative_walk_that_rolls_back_does_not_swallow_the_airlock_warning() {
    let stale = "fn f():\n    xs: List[int] = []\n    ys := []\n    parallel:\n        spawn:\n            xs.push(1)\n    ys.push(xs.len())\n    print(ys.len())\nf()\n";
    rejects(stale, "'xs' is this task's copy");
    // The control: identical but for `ys` being concrete, so no speculative refine runs at all.
    let concrete = "fn f():\n    xs: List[int] = []\n    ys: List[int] = [0]\n    parallel:\n        spawn:\n            xs.push(1)\n    ys.push(xs.len())\n    print(ys.len())\nf()\n";
    rejects(concrete, "'xs' is this task's copy");
    // …and still exactly once through the speculative path.
    let errs = check_src(stale);
    let matching = errs
        .iter()
        .filter(|e| e.message.contains("'xs' is this task's copy"))
        .count();
    assert_eq!(matching, 1, "expected one D4 error, got: {errs:?}");
}

/// One function's taint must not leak into the next: the map is taken and restored around every fn
/// body, so a second function reading a same-named binding of its own is clean.
#[test]
fn the_airlock_taint_does_not_leak_across_function_bodies() {
    rejects(
        "fn writer():\n    xs: List[int] = []\n    parallel:\n        spawn:\n            xs.push(1)\nfn reader():\n    xs: List[int] = []\n    print(xs.len())\nwriter()\nreader()\n",
        "'xs' is this task's copy",
    );
}

/// A body DECLARED INSIDE the task runs inside it, where the write IS visible — so it must not report,
/// and (the double failure) must not CONSUME the entry either, or the parent's real stale read after
/// the join goes silent. Both halves, `fn` and closure: only the `fn` half was written first, and the
/// closure half is exactly what shipped broken (`infer_closure` cleared `in_spawn_block` without
/// taking the taint). Measured on the pre-fix binary for the closure form: a FALSE warning at the
/// closure body's `xs.len()`, then `1` printed inside the task and `0` after the join with nothing
/// reported at all.
#[test]
fn a_body_declared_inside_the_task_neither_reports_nor_swallows_the_parents_warning() {
    for inner in [
        // closure
        "            g := fn() -> int: xs.len()\n            print(g())\n",
        // nested fn
        "            fn h() -> int:\n                return xs.len()\n            print(h())\n",
    ] {
        let src = format!(
            "fn f():\n    xs: List[int] = []\n    parallel:\n        spawn:\n            xs.push(1)\n{inner}    print(xs.len())\nf()\n"
        );
        rejects(&src, "'xs' is this task's copy");
    }
    // NEIGHBOUR the premise implies (and the reason the taint is not dropped unconditionally): a
    // closure declared in the PARENT reads the same stale copy the parent would — measured, `g()`
    // returns 0 — so it must still warn.
    rejects(
        "fn f():\n    xs: List[int] = []\n    parallel:\n        spawn:\n            xs.push(1)\n    g := fn() -> int: xs.len()\n    print(g())\nf()\n",
        "'xs' is this task's copy",
    );
}

/// A `defer:` block nested INSIDE a `spawn:` body is on the far side of the airlock like any other
/// task statement — its write really is lost, so it must taint. Measured on the release binary:
/// `spawn: defer: xs.push(1)` leaves `xs.len() == 0` after the join, and `spawn: defer: n = 5` leaves
/// `n == 0`. The code comment and `docs/reference/syntax/` both claimed the opposite (that a `defer:` never
/// reaches the write path as a task write) — a claim written without running the program.
#[test]
fn a_defer_inside_the_task_is_on_the_far_side_of_the_airlock() {
    rejects(
        "fn f():\n    xs: List[int] = []\n    parallel:\n        spawn:\n            defer:\n                xs.push(1)\n            print(1)\n    print(xs.len())\nf()\n",
        "'xs' is this task's copy",
    );
    // The reassignment spelling, which only a `defer:`/`spawn:` statement body can hold.
    rejects(
        "fn f():\n    n := 0\n    parallel:\n        spawn:\n            defer:\n                n = 5\n            print(1)\n    print(n)\nf()\n",
        "'n' is this task's copy",
    );
}

/// A closure value still declines; a direct named nested call reports its proven captured write.
/// A read inside a parent-side nested function still reports the earlier lost task write.
#[test]
fn the_two_frame_shaped_ceilings_under_warn() {
    // Ceiling 6 — the write never leaves the nested body's frame.
    no_warn(
        "fn f():\n    xs: List[int] = []\n    parallel:\n        spawn:\n            bump := fn(): xs.push(1)\n            bump()\n    print(xs.len())\nf()\n",
    );
    rejects(
        "fn f():\n    xs: List[int] = []\n    parallel:\n        spawn:\n            fn bump():\n                xs.push(1)\n            bump()\n    print(xs.len())\nf()\n",
        "'xs' is this task's copy",
    );
    // Ceiling 1 — the read never enters the parent-side nested `fn`'s frame.
    rejects(
        "fn f():\n    xs: List[int] = []\n    parallel:\n        spawn:\n            xs.push(1)\n    fn g() -> int:\n        return xs.len()\n    print(g())\nf()\n",
        "'xs' is this task's copy",
    );
}

/// The taint is keyed by BARE NAME, so a NEW binding of that name is a different binding and a read of
/// it loses nothing (`scope-blind-guard-over-rejects-shadows`). Measured on the pre-fix binary: `for n
/// in range(2):` after a task-side `n = 5` warned that the loop variable was a stale read — factually
/// false. One case per binding form that can introduce a name into a scope; all of them route through
/// `Checker::declare`, which is where the untaint lives.
#[test]
fn a_fresh_binding_of_the_name_untaints_it() {
    let head =
        "fn f():\n    n: int = 0\n    print(n)\n    parallel:\n        spawn:\n            n = 5\n";
    for tail in [
        // `for` loop variable — the reported repro
        "    for n in 0..2:\n        print(n)\n",
        // `for` DESTRUCTURING target
        "    ps := [(1, 2)]\n    for n, m in ps:\n        print(n + m)\n",
        // block-local `:=` shadow
        "    n := 9\n    print(n)\n",
        // destructuring `let`
        "    t := (7, 8)\n    n, y := t\n    print(n + y)\n",
        // `match` payload binding
        "    o: int? = ?3\n    match o:\n        ?n: print(n)\n        None:    print(0)\n",
        // `wait:` arm binding
        "    ch := Channel[int](1)\n    ch.send(4)\n    wait:\n        n := ch.recv(): print(n)\n",
        // closure parameter
        "    g := fn(n: int) -> int: n + 1\n    print(g(2))\n",
        // nested-fn parameter
        "    fn h(n: int) -> int:\n        return n + 1\n    print(h(2))\n",
    ] {
        rejects(&format!("{head}{tail}f()\n"), "'n' is this task's copy");
    }
    // NEIGHBOUR: a loop whose variable is a DIFFERENT name must still warn — the untaint is keyed on
    // the name bound, not on "a binding happened".
    rejects(
        "fn f():\n    xs: List[int] = []\n    parallel:\n        spawn:\n            xs.push(1)\n    for i in 0..2:\n        print(xs.len())\nf()\n",
        "'xs' is this task's copy",
    );
}

/// A parent-side in-place MUTATOR reads the stale copy before it writes it, exactly like the compound
/// assign `n += 1` — every method in `mutates_receiver` is a read-modify-write and the set contains no
/// whole-container replacement (`clear` does not exist), so none of them can legitimately supersede
/// the task's write. Measured: task `xs.push("a")` then parent `xs.push("b")` prints 1, not 2.
#[test]
fn a_parent_side_mutator_reads_the_stale_value_and_warns() {
    rejects(
        "fn f():\n    xs: List[str] = []\n    parallel:\n        spawn:\n            xs.push(\"a\")\n    xs.push(\"b\")\n    print(xs.len())\nf()\n",
        "'xs' is this task's copy",
    );
    // …still exactly once: reporting consumes the entry, so the receiver read that follows is silent.
    let errs = check_src(
        "fn f():\n    xs: List[str] = []\n    parallel:\n        spawn:\n            xs.push(\"a\")\n    xs.push(\"b\")\n    print(xs.len())\nf()\n",
    );
    let matching = errs
        .iter()
        .filter(|e| e.message.contains("'xs' is this task's copy"))
        .count();
    assert_eq!(matching, 1, "expected one D4 error, got: {errs:?}");
    // Inside the task the mutator writes the task's OWN copy — silent, as before.
    rejects(
        "fn f():\n    xs: List[str] = []\n    parallel:\n        spawn:\n            xs.push(\"a\")\n            xs.push(\"b\")\n            print(xs.len())\nf()\n",
        "'xs' is this task's copy",
    );
    // NEIGHBOUR: a whole-binding overwrite genuinely supersedes and stays silent (the asymmetry that
    // makes this rule a claim about read-modify-write, not about "any parent write").
    rejects(
        "fn f():\n    xs: List[str] = []\n    parallel:\n        spawn:\n            xs.push(\"a\")\n    xs = [\"b\"]\n    print(xs.len())\nf()\n",
        "'xs' is this task's copy",
    );
}

/// TICKET-289: an unused std `Task` warns like an unused `T!` (`Checker::is_must_use`), because its
/// job's fault appears only in `t.get()`. A user `struct Task` never warns: a std struct keeps its
/// bare key and `StructOrigin::Builtin`.
#[test]
fn an_unused_task_warns_like_an_unused_carrier() {
    let pre = "import Executor, Task from std.concurrency\nfn seven() -> int:\n    return 7\nfn start(ex: Executor) -> Task[int]:\n    return ex.submit(seven)\n";
    let needle = "the `Task[int]` value returned by 'submit' is discarded";
    entry_warns(
        &format!("{pre}fn f(ex: Executor):\n    ex.submit(seven)\nf(Executor())\n"),
        needle,
    );
    entry_warns(
        &format!("{pre}ex := Executor()\nex.submit(seven)\n"),
        needle,
    );
    entry_warns(
        &format!(
            "{pre}fn f(ex: Executor, c: bool):\n    if c:\n        ex.submit(seven)\n    else:\n        ex.submit(seven)\nf(Executor(), true)\n"
        ),
        needle,
    );
    entry_warns(
        &format!("{pre}ex := Executor()\nstart(ex)\n"),
        "returned by 'start' is discarded",
    );
    entry_no_warn(&format!(
        "{pre}ex := Executor()\nt := ex.submit(seven)\nprint(t.get())\n"
    ));
    entry_no_warn(&format!("{pre}ex := Executor()\n_ := ex.submit(seven)\n"));
    entry_no_warn(&format!(
        "{pre}ex := Executor()\nprint(ex.submit(seven).get())\n"
    ));
    entry_no_warn("struct Task:\n    n: int\nfn mine() -> Task:\n    return Task(n=1)\nmine()\n");
}
