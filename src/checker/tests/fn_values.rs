//! Checker tests: closure-param inference, native types, keyword args through fn values, variadics,
//! generic fn values.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ============================================================================
// Closure-parameter type inference (v1) — see docs/plan 2026-06-28.
// ============================================================================

// Phase 1a — expected type from a native HOF that routes through `check_args`
// (`Shared.update`): an unannotated closure param binds to the element type, so
// a body use incompatible with it is rejected (was accepted: param = Unknown).
#[test]
fn closure_param_inferred_from_shared_update_conflict_rejected() {
    entry_rejects(
        "import std.concurrency\nfn main():\n    s := Shared(0)\n    s.update(fn(x): x.upper())\n",
        "has no method 'upper'",
    );
}

#[test]
fn closure_param_inferred_from_shared_update_ok() {
    entry_ok(
        "import std.concurrency\nfn main():\n    s := Shared(0)\n    s.update(fn(x): x + 1)\n",
    );
}

// Native list HOFs (file-backed `map`/`filter` in std/prelude.chz, routed through the generic solver):
// the closure param binds to the element type, so a body use incompatible with it is rejected.
#[test]
fn map_closure_conflict_rejected() {
    entry_rejects(
        "fn main():\n    xs := [1, 2, 3]\n    ys := xs.map(fn(x): x.upper())\n    print(ys)\n",
        "has no method 'upper'",
    );
}

#[test]
fn closure_param_inferred_from_map_ok() {
    entry_ok("fn main():\n    xs := [1, 2, 3]\n    ys := xs.map(fn(x): x + 1)\n    print(ys)\n");
}

#[test]
fn closure_param_inferred_from_filter_conflict_rejected() {
    entry_rejects(
        "fn main():\n    xs := [1, 2, 3]\n    ys := xs.filter(fn(x): x.upper() == \"A\")\n    print(ys)\n",
        "has no method 'upper'",
    );
}

// Phase 6 — generic-solver closure-return LOOP-BACK: a return-position `[U]` inferred ONLY from an
// UNANNOTATED closure's body must resolve to the CONCRETE body-return type, not leak a `Ty::Param`.
const APPLY_DEFS: &str = "struct Box[T]:\n    v: T\n    fn apply[U](self, f: fn(T) -> U) -> U:\n        return f(self.v)\n";

#[test]
fn generic_method_recovers_return_param_from_unannotated_closure_ok() {
    // `U` is bound ONLY from the unannotated closure `fn(x): x + 1`'s body (int). Before the
    // loop-back the return leaked `U` (rendered `?`), so `n: int = ...` failed.
    entry_ok(&format!(
        "{APPLY_DEFS}fn main():\n    n: int = Box(3).apply(fn(x): x + 1)\n    print(n)\n"
    ));
}

#[test]
fn generic_method_recovers_return_param_from_unannotated_closure_is_concrete() {
    // The recovered return is CONCRETE `int` (not a leaked `U`): assigning it to `str` must report a
    // real int/str mismatch — proof the loop-back pinned `U=int`.
    entry_rejects(
        &format!("{APPLY_DEFS}fn main():\n    z: str = Box(3).apply(fn(x): x + 1)\n    print(z)\n"),
        "int",
    );
}

// Phase 6 — file-backed List HOF characterization: the inferred types MUST match the retired bespoke
// `infer_list_hof` exactly (the whole point of the loop-back). Pinned via the assign-mismatch trick.
#[test]
fn list_map_typed_closure_infers_list_str() {
    // Typed closure `fn(x: int) -> str`: map yields List[str]; a str element is fine, an int isn't.
    entry_ok(
        "fn main():\n    ys := [1, 2].map(fn(x: int) -> str: \"a\")\n    s: str = ys[0]\n    print(s)\n",
    );
    entry_rejects(
        "fn main():\n    ys := [1, 2].map(fn(x: int) -> str: \"a\")\n    n: int = ys[0]\n    print(n)\n",
        "cannot assign str to variable of type int",
    );
}

#[test]
fn list_map_unannotated_closure_recovers_int() {
    // THE PART-1 PAYOFF: an UNANNOTATED closure `fn(x): x * 2` → map yields List[int] (recovered from
    // the closure body), NOT a leaked List[?]. Assigning an element to str must mismatch on `int`.
    entry_rejects(
        "fn main():\n    ys := [1, 2].map(fn(x): x * 2)\n    s: str = ys[0]\n    print(s)\n",
        "int",
    );
}

#[test]
fn list_map_empty_list_element_behavior_preserved() {
    // A TYPED-but-empty list: element int, so `map(fn(x): x*2)` degrades cleanly to List[int] via the
    // loop-back (x=int, body int). No error — byte-identical to the retired bespoke arm.
    entry_ok(
        "fn main():\n    xs: List[int] = []\n    ys := xs.map(fn(x): x * 2)\n    n: int = ys[0]\n    print(n)\n",
    );
    // A BARE `[]` receiver still needs an annotation — same diagnostic the old bespoke arm produced
    // (the empty-collection rule is upstream of map dispatch; unchanged by the port).
    entry_rejects(
        "fn main():\n    ys := [].map(fn(x): x * 2)\n    print(ys)\n",
        "cannot infer type of parameter 'x'",
    );
}

#[test]
fn list_map_chained_and_nested_ok() {
    // Nested `xs.map(fn(x): [x])` → List[List[int]]; a second `.map` over it flows the element type.
    entry_ok(
        "fn main():\n    ys := [1, 2].map(fn(x): [x]).map(fn(r): r.len())\n    n: int = ys[0]\n    print(n)\n",
    );
    // Chained map -> filter -> fold: map to str, filter, fold to a joined str.
    entry_ok(
        "fn main():\n    r := [1, 2, 3].map(fn(x): x * 2).filter(fn(x): x > 2).fold(0, fn(a, x): a + x)\n    n: int = r\n    print(n)\n",
    );
}

#[test]
fn list_fold_init_typed_result() {
    // fold's `U` binds from `init` in pass 1 (not the loop-back): a str init → str result.
    entry_ok(
        "fn main():\n    s := [1, 2, 3].fold(\"\", fn(a, x): a + \"!\")\n    t: str = s\n    print(t)\n",
    );
    entry_rejects(
        "fn main():\n    s := [1, 2, 3].fold(0, fn(a, x): a + x)\n    t: str = s\n    print(t)\n",
        "cannot assign int to variable of type str",
    );
}

// Phase 6 — CONFIRMED-BUG regression (adversarial review): the closure-return loop-back's
// still-unbound degrade must be CONDITIONAL. A method type param that appears ONLY in the return
// position and in NO parameter (`fn make[U](self) -> U`) is genuinely un-inferable — no argument can
// bind it. It must stay a leaked `Ty::Param` (rejected on assignment to a concrete type), NOT degrade
// to `Unknown` (which `assignable` treats as universally assignable, silently masking the
// un-inferable-ness and letting a wrong static type escape onto the value). This is the exact base
// behavior an unconditional degrade regressed.
const RETURN_ONLY_PARAM_DEFS: &str =
    "struct Box[T]:\n    v: T\n    fn make[U](self) -> U:\n        return self.make()\n";

#[test]
fn return_only_method_type_param_is_pinned_by_the_annotation() {
    // `U` appears ONLY in `-> U`, in no parameter — so no ARGUMENT can bind it, but a result
    // annotation can, and now does: the method path gained `seed_from_hint`, which the free-fn path
    // has always had (`ys: List[str] = geo.empty_list()`).
    //
    // This assertion was inverted until then: it required `cannot assign U to variable of type str`,
    // encoding the method path's MISSING hint as if it were intended semantics. Rust is the owning
    // ancestor for generics and settles it — `impl<T> Box<T> { fn make<U>(&self) -> U }` with
    // `let z: String = b.make();` COMPILES, and only the unannotated `let z = b.make();` is refused
    // (`E0282: type annotations needed`).
    entry_ok(&format!(
        "{RETURN_ONLY_PARAM_DEFS}fn main():\n    b := Box(1)\n    z: str = b.make()\n    print(z)\n"
    ));

    // The guard this test really exists for is intact: `U` is pinned to the CONCRETE `str`, not
    // degraded to `Unknown` (which `assignable` treats as universally assignable and which would let
    // a wrong static type escape onto the value). `str + 1` is what an `Unknown` would sail through.
    entry_rejects(
        &format!(
            "{RETURN_ONLY_PARAM_DEFS}fn main():\n    b := Box(1)\n    z: str = b.make()\n    print(z + 1)\n"
        ),
        "cannot apply + to str and int",
    );
}

// Phase 6 — Bug D: the closure-return loop-back must recover a method `[U]` when the closure body is
// a NESTED FREE generic call whose return is the callee's own type param. `xs.map(fn(x): ident(x))`
// where `ident[T](x: T) -> T` must yield List[int] (not a leaked List[T]).
const IDENT_DEFS: &str = "fn ident[T](x: T) -> T:\n    return x\n";

#[test]
fn map_closure_body_free_generic_call_recovers_int() {
    // The repro: closure body `ident(x)` returns ident's OWN `T`; the solver knows `x: int` and must
    // unify `int` into ident's `T` via the checking-mode re-inference, so `ys: List[int]` and
    // `ys[0] + 1` type-checks (prints 2 at runtime).
    entry_ok(&format!(
        "{IDENT_DEFS}fn main():\n    xs := [1, 2, 3]\n    ys := xs.map(fn(x): ident(x))\n    print(ys[0] + 1)\n"
    ));
}

#[test]
fn map_closure_body_free_generic_call_stays_sound() {
    // SOUNDNESS guard: `U` must be recovered as CONCRETE `int`, never degraded to `Unknown` (which
    // `assignable` would launder). Assigning the int-typed result to List[str] / List[List[int]] must
    // reject naming `int`.
    entry_rejects(
        &format!(
            "{IDENT_DEFS}fn main():\n    xs := [1, 2, 3]\n    zs: List[str] = xs.map(fn(x): ident(x))\n    print(zs)\n"
        ),
        "int",
    );
    entry_rejects(
        &format!(
            "{IDENT_DEFS}fn main():\n    xs := [1, 2, 3]\n    ws: List[List[int]] = xs.map(fn(x): ident(x))\n    print(ws)\n"
        ),
        "int",
    );
}

#[test]
fn bug_d_boundaries_stay_green() {
    // (1) direct closure body — already worked, must stay green.
    entry_ok("fn main():\n    ys := [1, 2].map(fn(x): x * 2)\n    n: int = ys[0]\n    print(n)\n");
    // (2) generic METHOD in body — receiver carries the type arg, resolves fine today.
    entry_ok(
        "struct W[T]:\n    v: T\n    fn get(self) -> T:\n        return self.v\nfn main():\n    ws := [W(1), W(2)]\n    n: int = ws.map(fn(w): w.get())[0] + 1\n    print(n)\n",
    );
    // (3) downstream-pinned — fold's `+` and filter's `>` pin T=int via the body arithmetic.
    entry_ok(&format!(
        "{IDENT_DEFS}fn main():\n    xs := [1, 2, 3]\n    s := xs.fold(0, fn(acc, x): acc + ident(x))\n    print(s)\n"
    ));
    entry_ok(&format!(
        "{IDENT_DEFS}fn main():\n    xs := [1, 2, 3]\n    ys := xs.filter(fn(x): ident(x) > 1)\n    print(ys)\n"
    ));
    // (4) genuinely ambiguous closure body — must give a clean "cannot infer" diagnostic, not a panic.
    entry_rejects(
        "fn main():\n    ys := [1, 2].map(fn(x): fn(y): x + y)\n    print(ys)\n",
        "cannot infer",
    );
}

#[test]
fn fold_closure_wrong_return_rejected() {
    // SOUNDNESS (the mask must NOT disable the closure-return check when the method's return-position
    // `[U]` is ALREADY pinned by another argument): `fold[U]`'s `U` is pinned to `int` by `init` (arg
    // 0), so the closure's return `fn(U, T) -> U` has the CONCRETE contract `int`. An unannotated
    // closure whose body is a `str` must be rejected at check time — masking its fallback return to
    // `Unknown` (which `assignable` laundered) let a str value escape onto an `int`-typed binding.
    entry_rejects(
        "fn main():\n    xs := [1, 2, 3]\n    s := xs.fold(0, fn(acc, x): \"wrong\")\n    print(s + 1)\n",
        "fold",
    );
    // The same hole via an explicit annotation: `n: int` must not accept a str-returning fold body.
    entry_rejects(
        "fn main():\n    xs := [1, 2, 3]\n    n: int = xs.fold(0, fn(acc, x): \"wrong\")\n    print(n)\n",
        "fold",
    );
}

#[test]
fn fold_closure_body_free_generic_call_recovers_int() {
    // Bug D (adversarial-review fix): a `fold[U]` whose `U` is pinned CONCRETE by `init` (arg 0) AND
    // whose unannotated closure body is a NESTED FREE generic call (`ident(x)` / `ident(acc)`, where
    // `ident[T](x: T) -> T`) must be ACCEPTED — the prepass leaks the callee's own rigid `Ty::Param`,
    // but the checking-mode re-inference types the body `int`. The earlier gate (masking only a still-
    // free `[U]`) spuriously rejected these with `argument to 'fold' has type fn(?, ?) -> T`.
    // (a) body references the closure's element param `x`.
    entry_ok(
        "fn ident[T](x: T) -> T:\n    return x\nfn main():\n    xs := [1, 2, 3]\n    s := xs.fold(0, fn(acc, x): ident(x))\n    print(s + 1)\n",
    );
    // (b) body references the closure's accumulator param `acc` (bare nested generic call).
    entry_ok(
        "fn ident[T](x: T) -> T:\n    return x\nfn main():\n    xs := [1, 2, 3]\n    s := xs.fold(0, fn(acc, x): ident(acc))\n    print(s)\n",
    );
}

// Phase 6b — Bug D FREE-FN analog: the closure-return loop-back must also recover a generic FREE
// FUNCTION's return-only `[U]` from an inferable closure/fn body. Same mechanism as the method path,
// ported into `infer_generic_call` (`recover_return_only_params`). Before the fix these all rejected
// with `cannot apply + to U/B/T and int`.
#[test]
fn free_fn_hof_returnonly_recovers_scalar_and_container() {
    // Scalar return-only U (param pinned by the `fn(int) -> U` slot): `applyone(5, fn(x): x*2)` yields
    // int, so `y + 1` type-checks.
    entry_ok(
        "fn applyone[U](x: int, f: fn(int) -> U) -> U:\n    return f(x)\nfn main():\n    y := applyone(5, fn(x): x * 2)\n    print(y + 1)\n",
    );
    // Container `-> List[U]` form: `mymap([1,2,3], fn(x): x*2)` yields List[int], so `ys[0] + 1` checks.
    entry_ok(
        "fn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := mymap([1, 2, 3], fn(x): x * 2)\n    print(ys[0] + 1)\n",
    );
}

#[test]
fn free_fn_hof_returnonly_pinned_mismatch_rejected() {
    // SOUNDNESS (the laundering hole — free-fn analog of `fold_closure_wrong_return_rejected`): a
    // free HOF whose return-only `U` is pinned by a SIBLING value arg (`init: U` = 0 ⇒ `U = int`) and
    // whose unannotated closure returns a MISMATCHING type (`str`) must be a clean type error, not
    // laundered onto the pinned `int`. Proves the helper's SEPARATE concrete-return check is load-bearing.
    entry_rejects(
        "fn f[U](init: U, g: fn(int) -> U, xs: List[int]) -> U:\n    return g(xs[0])\nfn main():\n    y := f(0, fn(x): str(x), [1, 2, 3])\n    print(y)\n",
        "returns",
    );
    // apply-shape sibling-pin variant: `B` pinned to `int` by the `sink: B` = 99 arg, closure returns str.
    entry_rejects(
        "fn apply[A, B](f: fn(A) -> B, a: A, sink: B) -> B:\n    return sink\nfn main():\n    y := apply(fn(x): str(x), 5, 99)\n    print(y)\n",
        "returns",
    );
}

#[test]
fn free_fn_hof_returnonly_unknown_cored_container_not_laundered() {
    // SOUNDNESS regression (review fix): a param-dependent closure whose body is a CONTAINER of the
    // param (`fn(x): [x]` → prepass `List[Unknown]`) must NOT pre-bind the HOF's return-only `U` to
    // `List[Unknown]` in pass-1 — that would launder `List[str]` onto it (`unify` only skips a
    // TOP-level Unknown, not a nested one). The pre-bind gate is `ty_fully_concrete`, so `List[Unknown]`
    // defers to the loop-back's refined re-inference, which recovers the CONCRETE `List[int]`.
    // (a) recovers the concrete element type — `ys[0][0] + 1` type-checks:
    entry_ok(
        "fn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := mymap([1, 2, 3], fn(x): [x])\n    print(ys[0][0] + 1)\n",
    );
    // (b) does NOT launder — assigning the recovered `List[List[int]]` to `List[List[str]]` is rejected:
    entry_rejects(
        "fn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := mymap([1, 2, 3], fn(x): [x])\n    zs: List[List[str]] = ys\n    print(zs)\n",
        "",
    );
}

#[test]
fn free_fn_hof_returnonly_boundaries() {
    const IDENT: &str = "fn ident[T](x: T) -> T:\n    return x\n";
    // p4 — nested free-generic-call body in a CONTAINER HOF (`fn(x): ident(x)`): the prepass leaks
    // ident's own `T`; checking-mode re-inference recovers int, so `ys[0] + 1` checks.
    entry_ok(&format!(
        "{IDENT}fn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := mymap([1, 2, 3], fn(x): ident(x))\n    print(ys[0] + 1)\n"
    ));
    // p16 — nested free-generic-call body in a SCALAR HOF.
    entry_ok(&format!(
        "{IDENT}fn applyone[U](x: int, f: fn(int) -> U) -> U:\n    return f(x)\nfn main():\n    y := applyone(5, fn(x): ident(x))\n    print(y + 1)\n"
    ));
    // p12 — protocol-bounded return-only `[U: Add]`: recover U=int AND re-enforce the `Add` bound.
    entry_ok(
        "fn mapadd[U: Add](x: int, f: fn(int) -> U) -> U:\n    return f(x)\nfn main():\n    y := mapadd(5, fn(x): x * 2)\n    print(y + 1)\n",
    );
    // p2 — sibling value arg pins A (=int), which flows into the closure PARAM; B is return-only.
    entry_ok(
        "fn apply[A, B](f: fn(A) -> B, a: A) -> B:\n    return f(a)\nfn main():\n    y := apply(fn(x): x * 2, 5)\n    print(y + 1)\n",
    );
    // p11 — multiple return-only params `[U, V]`; only U flows to the result, both recovered.
    entry_ok(
        "fn two[U, V](xs: List[int], f: fn(int) -> U, g: fn(int) -> V) -> U:\n    return f(xs[0])\nfn main():\n    y := two([1, 2, 3], fn(x): x * 2, fn(x): str(x))\n    print(y + 1)\n",
    );
    // p9 — chained/nested free HOFs `mymap(mymap(...))`.
    entry_ok(
        "fn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := mymap(mymap([1, 2, 3], fn(x): x * 2), fn(y): y + 1)\n    print(ys[0] + 1)\n",
    );
    // p17 — str return-only recovered and used via `.len()`.
    entry_ok(
        "fn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := mymap([1, 2, 3], fn(x): str(x))\n    print(ys[0].len())\n",
    );
}

#[test]
fn free_fn_hof_must_not_regress() {
    // These ACCEPT on `main` and must stay accepting through the refactor (behavior-preserving).
    // p5 — named-fn arg with a concrete return (not a closure): `double` is `fn(int) -> int`.
    entry_ok(
        "fn double(x: int) -> int:\n    return x * 2\nfn mymap[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := mymap([1, 2, 3], double)\n    print(ys[0] + 1)\n",
    );
    // p6 — closure body INDEPENDENT of the param (bool), used via `if`.
    entry_ok(
        "fn keep[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := keep([1, 2, 3], fn(x): x > 0)\n    if ys[0]:\n        print(\"y\")\n",
    );
    // p7 — closure body producing str, used via `.len()` / concat.
    entry_ok(
        "fn keep[U](xs: List[int], f: fn(int) -> U) -> List[U]:\n    return xs.map(f)\nfn main():\n    ys := keep([1, 2, 3], fn(x): str(x))\n    print(ys[0].len())\n",
    );
    // annotated closure on a leaking body: `fn(x): x*2` param annotated, return still recovered.
    entry_ok(
        "fn applyone[U](x: int, f: fn(int) -> U) -> U:\n    return f(x)\nfn main():\n    y := applyone(5, fn(x: int): x * 2)\n    print(y + 1)\n",
    );
    // annotated-RETURN closure on a leaking body.
    entry_ok(
        "fn applyone[U](x: int, f: fn(int) -> U) -> U:\n    return f(x)\nfn main():\n    y := applyone(5, fn(x) -> int: x * 2)\n    print(y + 1)\n",
    );
    // param-position type param whose value is Unknown (empty list) — degrade-to-Unknown edge, `-> int`
    // result independent of U: was `ok` pre-fix, must stay `ok`.
    entry_ok(
        "fn g[U](xs: List[U], f: fn(U) -> int) -> int:\n    return f(xs[0])\nfn main():\n    n := g([], fn(x): 0)\n    print(n)\n",
    );
    // genuinely un-inferable return-only param must STILL reject (leaked `Ty::Param`, not degraded).
    entry_rejects(
        "fn make[U](x: int) -> U:\n    return x\nfn main():\n    y := make(5)\n    print(y)\n",
        "U",
    );
}

#[test]
fn free_fn_hof_ambiguous_stays_clean_error() {
    // A genuinely ambiguous closure body (`fn(y): x+y` — `y` un-inferable) must give a clean
    // `cannot infer` diagnostic and NO host panic — and EXACTLY ONE error (the new loop-back /
    // concrete-return check must not swallow, re-order, or double-report it).
    let errs = check_entry(
        "fn applyone[U](x: int, f: fn(int) -> U) -> U:\n    return f(x)\nfn main():\n    y := applyone(5, fn(x): fn(y): x + y)\n    print(y)\n",
    );
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert!(
        errs[0].message.contains("cannot infer"),
        "expected a 'cannot infer' diagnostic, got: {errs:?}"
    );
}

#[test]
fn free_fn_hof_sibling_closure_param_use_recovers() {
    // REGRESSION (adversarial-review bug 1): a return-only `[T]` bound from a bare closure's CONCRETE
    // return (`fn(): 5` → `int`) must be recovered BEFORE the un-inferable-param probe runs, so a
    // SIBLING closure that uses the SAME `[T]` in PARAMETER position (`g: fn(T) -> int`) is not
    // spuriously rejected as an un-inferable deadlock. On the branch before this fix, pass-1 masking
    // left `T` unbound and `report_uninferable_closure_params` fired `cannot infer type parameter T`
    // + `cannot infer type of parameter 'x'`; `main` accepts and runs (prints 6).
    entry_ok(
        "fn pair[T](f: fn() -> T, g: fn(T) -> int) -> int:\n    return g(f())\nfn main():\n    print(pair(fn(): 5, fn(x): x + 1))\n",
    );
    // The same shape with the return-only param flowing through a chain of consumers.
    entry_ok(
        "fn chain[T](f: fn() -> T, g: fn(T) -> int) -> int:\n    return g(f()) + 1\nfn main():\n    print(chain(fn(): 5, fn(x): x + 1))\n",
    );
    // The Bug-1 recovery must NOT pre-empt a sibling VALUE arg that pins the same return-only param: a
    // concrete closure return that CONFLICTS with a value-arg pin is still rejected (no binding race).
    entry_rejects(
        "fn apply[A, B](f: fn(A) -> B, a: A, sink: B) -> B:\n    return sink\nfn main():\n    y := apply(fn(x): str(x), 5, 99)\n    print(y)\n",
        "returns",
    );
}

#[test]
fn free_fn_hof_conflicting_returnonly_closures_rejected() {
    // SOUNDNESS (adversarial-review bug 2): two closure args binding the SAME return-only `[U]` must
    // AGREE or be rejected. The interleaved loop-back binds `[U]` from the first closure, then the
    // second closure's `want` return is CONCRETE and its mismatching body is rejected by the
    // concrete-return soundness check — instead of being silently dropped by only-bind-unbound `unify`.
    // On the branch before this fix both `chezzi check` succeeded and `chezzi run` aborted at runtime.
    entry_rejects(
        "fn pick[U](cond: bool, a: fn() -> U, b: fn() -> U) -> U:\n    if cond:\n        return a()\n    return b()\nfn main():\n    r := pick(false, fn(): 1, fn(): \"hello\")\n    print(r + 1)\n",
        "returns",
    );
    entry_rejects(
        "fn two[U](f: fn(int) -> U, g: fn(int) -> U) -> U:\n    return f(0)\nfn main():\n    r := two(fn(x): x * 2, fn(x): str(x))\n    print(r + 1)\n",
        "returns",
    );
    // Two closures that AGREE on the return-only param still ACCEPT (recovered, no false positive).
    entry_ok(
        "fn two[U](f: fn(int) -> U, g: fn(int) -> U) -> U:\n    return f(0)\nfn main():\n    r := two(fn(x): x * 2, fn(x): x + 1)\n    print(r + 1)\n",
    );
}

#[test]
fn rwshared_read_len_ok() {
    entry_ok(
        "import std.concurrency\nfn main():\n    r := RwShared({\"a\": 1})\n    print(r.read(fn(m): m.len()))\n",
    );
}

// Phase 1c — generic struct constructor: a `fn`-typed field's closure arg is
// re-inferred against the SUBSTITUTED field type, so its param binds concretely.
const MAPPED_DEFS: &str = "struct Mapped[I: Iterator[T], T, U]:\n    inner: I\n    f: fn(T) -> U\n    fn next(self) -> U?:\n        match self.inner.next():\n            ?x:\n                return ?self.f(x)\n            None:\n                return None\n";

#[test]
fn closure_param_inferred_in_generic_struct_ctor_ok() {
    entry_ok(&format!(
        "{MAPPED_DEFS}fn main():\n    m := Mapped([1, 2, 3].iter(), fn(x): x * 2)\n    print(m.next())\n"
    ));
}

#[test]
fn closure_param_inferred_in_generic_struct_ctor_conflict_rejected() {
    entry_rejects(
        &format!(
            "{MAPPED_DEFS}fn main():\n    m := Mapped([1, 2, 3].iter(), fn(x): x.upper())\n    print(m.next())\n"
        ),
        "has no method 'upper'",
    );
}

// Phase 2a — a closure bound to a `fn`-typed `let`/`:=` annotation is inferred
// in checking-mode (source #1).
#[test]
fn closure_inferred_from_fn_typed_let_binding_ok() {
    entry_ok("fn main():\n    cb: fn(int) -> int = fn(x): x + 1\n    print(cb(2))\n");
}

#[test]
fn closure_inferred_from_fn_typed_let_binding_conflict_rejected() {
    entry_rejects(
        "fn main():\n    cb: fn(int) -> int = fn(x): x.upper()\n    print(cb(2))\n",
        "has no method 'upper'",
    );
}

// Phase 2b — struct fn-field assignment + fn-typed return position (source #1).
#[test]
fn closure_inferred_in_struct_fn_field_assignment_ok() {
    entry_ok(
        "struct Box:\n    cb: fn(int) -> int\nfn main():\n    b := Box(fn(x: int) -> int: x)\n    b.cb = fn(x): x + 1\n    print(b.cb(2))\n",
    );
}

#[test]
fn closure_inferred_in_struct_fn_field_assignment_conflict_rejected() {
    entry_rejects(
        "struct Box:\n    cb: fn(int) -> int\nfn main():\n    b := Box(fn(x: int) -> int: x)\n    b.cb = fn(x): x.upper()\n    print(b.cb(2))\n",
        "has no method 'upper'",
    );
}

#[test]
fn closure_inferred_in_fn_typed_return_ok() {
    entry_ok(
        "fn make() -> fn(int) -> int:\n    return fn(x): x + 1\nfn main():\n    f := make()\n    print(f(2))\n",
    );
}

#[test]
fn closure_inferred_in_fn_typed_return_conflict_rejected() {
    entry_rejects(
        "fn make() -> fn(int) -> int:\n    return fn(x): x.upper()\nfn main():\n    f := make()\n    print(f(2))\n",
        "has no method 'upper'",
    );
}

// Phase 3a — free-closure body inference (source #2: a match whose scrutinee is
// the BARE param). Pins the param, so the call site is checked.
#[test]
fn free_closure_match_variant_pins_param_call_site_rejects() {
    entry_rejects(
        "enum E:\n    A\n    B\ng := fn(x): match x:\n    E.A: \"a\"\n    E.B: \"b\"\nfn main(): print(g(5))\n",
        "found int",
    );
}

#[test]
fn free_closure_all_variants_match_accepts() {
    entry_ok(
        "enum E:\n    A\n    B\ng := fn(x): match x:\n    E.A: \"a\"\n    E.B: \"b\"\nfn main(): print(g(E.A))\n",
    );
}

#[test]
fn free_closure_concrete_nested_tuple_not_swept() {
    entry_ok(
        "enum E:\n    A\n    B\nfn main():\n    g := fn(x: (E, int)): match x:\n        (E.A, b): \"a\"\n        _: \"o\"\n    print(g((E.A, 1)))\n",
    );
}

// Phase 4a — a STRUCTURAL sub-pattern over an un-inferable (Unknown) element/payload
// is rejected at `bind_subpattern` (the trap class: matching a tuple/variant shape on
// a value whose type we can't pin). The bare-param match pins x to a tuple/Result/Option
// with Unknown elements, so the nested E.A is structural-over-Unknown → reject.
#[test]
fn nested_tuple_subpattern_over_unknown_rejected() {
    entry_rejects(
        "enum E:\n    A\n    B\ng := fn(x): match x:\n    (E.A, b): \"a\"\n    _: \"o\"\nfn main(): print(g((5, 9)))\n",
        "cannot infer the type of `x`",
    );
}

#[test]
fn nested_ok_payload_subpattern_over_unknown_rejected() {
    entry_rejects(
        "enum E:\n    A\n    B\ng := fn(x): match x:\n    ?E.A: \"a\"\n    _: \"o\"\nfn main(): print(g(?5))\n",
        "un-inferable type",
    );
}

#[test]
fn nested_some_payload_subpattern_over_unknown_rejected() {
    entry_rejects(
        "enum E:\n    A\n    B\ng := fn(x): match x:\n    ?E.A: \"a\"\n    _: \"o\"\nfn main(): print(g(?5))\n",
        "un-inferable type",
    );
}

#[test]
fn nested_guarded_subpattern_over_unknown_rejected() {
    entry_rejects(
        "enum E:\n    A\n    B\ng := fn(x, c: bool): match x:\n    (E.A, b) if c: \"a\"\n    _: \"o\"\nfn main(): print(g((5, 9), true))\n",
        "cannot infer the type of `x`",
    );
}

#[test]
fn nested_or_alt_subpattern_over_unknown_rejected() {
    entry_rejects(
        "enum E:\n    A\n    B\ng := fn(x): match x:\n    (E.A, b) | (E.B, b): \"a\"\n    _: \"o\"\nfn main(): print(g((5, 9)))\n",
        "cannot infer the type of `x`",
    );
}

// Phase 4a — boundary: a concrete-element structural sub-pattern must NOT be swept up.
#[test]
fn concrete_nested_ok_subpattern_accepts() {
    entry_ok(
        "enum E:\n    A\n    B\nfn main():\n    g := fn(x: E!str): match x:\n        ?E.A: \"a\"\n        _: \"o\"\n    print(g(?E.A))\n",
    );
}

// Phase 4b — a residual-Unknown scrutinee (a tuple-element binding) with STRUCTURAL arms
// rejects (the top-level scrutinee arm goes through match_kind/reconstruct, not bind_subpattern).
#[test]
fn residual_unknown_top_level_structural_rejected() {
    // Unannotated `x`: source #2 pins it to `(Unknown, Unknown)` from the `(a, b)` arm, so the
    // tuple-element binding `a` is Unknown; the inner `match a:` is a top-level structural match on a
    // residual-Unknown scrutinee → reject (caught by match_kind/reconstruct, not bind_subpattern).
    entry_rejects(
        "enum E:\n    A\n    B\ng := fn(x): match x:\n    (a, b): match a:\n        E.A: \"a\"\n        E.B: \"b\"\nfn main(): print(g((E.A, 1)))\n",
        "cannot infer the type of `x`",
    );
}

#[test]
fn residual_unknown_top_level_structural_annotated_accepts() {
    entry_ok(
        "enum E:\n    A\n    B\nfn classify(p: (E, int)) -> str:\n    a, b := p\n    return match a:\n        E.A: \"a\"\n        E.B: \"b\"\nfn main(): print(classify((E.A, 1)))\n",
    );
}

// Phase 4b — heterogeneous literal arms over a residual-Unknown scrutinee now reject (the first
// arm pins the literal kind; the `OpenScrutinee` accept-with-`_` path is gone).
#[test]
fn residual_unknown_hetero_literal_rejects() {
    entry_rejects(
        "g := fn(x): match x:\n    (a, b): match a:\n        1: \"x\"\n        \"b\": \"y\"\n        _: \"z\"\n    _: \"o\"\nfn main(): print(g((1, 2)))\n",
        "cannot infer the type of `x`",
    );
}

// Phase 5 — a free closure whose param NOTHING pins (no expected/slot type, no body match-
// scrutinee or unique member) must be annotated; it must not degrade to a runtime `Unknown`.
#[test]
fn free_closure_unresolved_param_arith_requires_annotation() {
    entry_rejects(
        "g := fn(x): x + 1\nfn main(): print(g(2))\n",
        "cannot infer type of parameter 'x'; add a type annotation",
    );
}

#[test]
fn free_closure_unresolved_param_print_requires_annotation() {
    entry_rejects(
        "g := fn(x): print(x)\nfn main(): g(2)\n",
        "cannot infer type of parameter 'x'; add a type annotation",
    );
}

#[test]
fn annotated_free_closure_arith_ok() {
    entry_ok("g := fn(x: int): x + 1\nfn main(): print(g(2))\n");
}

#[test]
fn annotated_free_closure_match_ok() {
    entry_ok(
        "enum E:\n    A\n    B\ng := fn(x: E): match x:\n    E.A: \"a\"\n    E.B: \"b\"\nfn main(): print(g(E.A))\n",
    );
}

// ===== Adversarial-review fixes (closure-param inference) =====

// BUG 1 — the free-closure match-scrutinee scan (source #2) must be scope-aware: a `match x:` whose
// scrutinee `x` is a SHADOWING binding introduced by an enclosing tuple/variant sub-pattern is NOT
// the closure param, so it must not pin the param. Here the param `x` is never used (the inner
// `match x:` reads the `(x, _)` tuple binding), so the param is genuinely un-inferable → annotate.
// Pre-fix: param wrongly pinned to `E` → spurious call-site reject "expected E, found str".
#[test]
fn free_closure_shadowing_match_does_not_pin_param() {
    let errs = check_entry(
        "enum E:\n    A\n    B\nfn foo() -> (E, int):\n    return (E.A, 1)\ng := fn(x): match foo():\n    (x, _): match x:\n        E.A: 1\n        E.B: 2\nfn main(): print(g(\"hello\"))\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("cannot infer type of parameter 'x'")),
        "expected the annotate-param error (param is un-inferable), got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("expected E")),
        "param must NOT be pinned to E from the shadowing inner match, got: {errs:?}"
    );
}

// BUG 2 — a closure literal passed to a `fn`-typed slot with MISMATCHED arity must keep its params
// silently `Unknown` (the call site's assignability check reports the single arity diagnostic); it
// must NOT also route through the free-closure scan and emit "cannot infer type of parameter".
#[test]
fn closure_arity_mismatch_single_diagnostic() {
    let errs = check_entry(
        "fn g(f: fn(int) -> int) -> int:\n    return f(0)\nfn main(): print(g(fn(a, b): a + b))\n",
    );
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("cannot infer type of parameter")),
        "arity mismatch must not emit the annotate-param error, got: {errs:?}"
    );
    assert!(
        !errs.is_empty(),
        "the call-site arity/assignability mismatch must still be reported, got: {errs:?}"
    );
}

// BUG 3 — source #3 (unique member access) must fire even when the only pinning use of the param is
// inside a string-interpolation fragment (`"{x.name}"`). Pre-fix the scan was opaque to `Str` so the
// param was reported un-inferable.
#[test]
fn free_closure_pinned_by_member_in_interpolation() {
    entry_ok(
        "struct P:\n    name: str\nf := fn(x): \"hello {x.name}\"\nfn main(): print(f(P(\"bob\")))\n",
    );
}

// BUG 4/6 — a non-closure return expression must keep using plain `infer` (not `infer_value`): a
// `return <void-call>` must report ONLY "function returns nothing", not ALSO the value-position
// nil-rejection.
#[test]
fn return_void_call_single_diagnostic() {
    let errs = check_entry("fn f():\n    return print(\"x\")\nfn main(): f()\n");
    assert!(
        errs.iter().any(|e| e
            .message
            .contains("declares no return type, so it returns nothing")),
        "expected the 'declares no return type' error, got: {errs:?}"
    );
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("returns no value (None)")),
        "a return expr must not get the value-position None rejection, got: {errs:?}"
    );
}

// BUG 5 — source #3 must scan only USER structs; the always-seeded `Builtin`-origin native structs
// (Match/Response/ProcResult) must never pin a param. `end` is a field of Match (Builtin) and of no
// user struct, so the param is un-inferable → annotate, NOT pinned to the unimported `Match`.
#[test]
fn free_closure_member_does_not_pin_builtin_struct() {
    let errs = check_entry("f := fn(s): s.end\nfn main(): print(f(5))\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("cannot infer type of parameter 's'")),
        "expected the annotate-param error, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("Match")),
        "param must NOT be pinned to the seeded Builtin struct Match, got: {errs:?}"
    );
}

// BUG 7 — an assignment lvalue target must not be inferred as an rvalue (read-side gates / receiver
// double-inference). A closure assigned to a bad field must report the field error ONCE.
#[test]
fn closure_assign_bad_field_single_error() {
    let errs = check_entry(
        "struct Box:\n    cb: fn(int) -> int\nfn main():\n    b := Box(fn(x: int) -> int: x)\n    b.nope = fn(x): x + 1\n",
    );
    let field_errs = errs
        .iter()
        .filter(|e| e.message.contains("no field 'nope'"))
        .count();
    assert_eq!(
        field_errs, 1,
        "the bad-field error must be reported exactly once, got: {errs:?}"
    );
}

// SOUNDNESS — a closure passed to a GENERIC `T` slot whose type param is bound ONLY by the closure
// itself unifies `T` to `fn(Unknown) -> Unknown`; the substituted expected param type is therefore
// `Unknown`. Binding the unannotated closure param to that `Unknown` silently (source #1) leaves the
// call site unchecked → check-passes-then-traps at runtime. An `Unknown` expected
// param type must NOT count as a pin: fall through to the body scan / annotation requirement.
#[test]
fn closure_param_under_generic_unknown_slot_requires_annotation() {
    let errs = check_entry(
        "fn store[T](x: T) -> T:\n    return x\nfn main():\n    f := store(fn(a): a + 1)\n    print(f(\"hello\"))\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("cannot infer type of parameter 'a'")),
        "an unannotated closure param under a generic Unknown slot must require annotation, got: {errs:?}"
    );
}

// The same generic slot WITH an annotated closure param is fine, and the resolved `fn(int) -> int`
// signature is propagated so a later mismatched call is rejected at the call site (no runtime trap).
#[test]
fn closure_param_under_generic_slot_annotated_propagates_to_call_site() {
    let errs = check_entry(
        "fn store[T](x: T) -> T:\n    return x\nfn main():\n    f := store(fn(a: int): a + 1)\n    print(f(\"hello\"))\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("expected int") || e.message.contains("found str")),
        "annotated closure param must propagate fn(int)->int so the str call is rejected, got: {errs:?}"
    );
}

// Source #3 (unique member) — the design's flagship builtin example: `x.upper()` is owned only by
// `str`, so a free closure `fn(x): x.upper()` pins `x: str` and runs. (Previously the scan saw only
// user structs, so this was wrongly reported un-inferable.)
#[test]
fn free_closure_pinned_by_unique_str_method() {
    entry_ok("f := fn(x): x.upper()\nfn main(): print(f(\"hi\"))\n");
}

// …and the resolved `fn(str) -> str` signature propagates: a non-str call is rejected at the call
// site (no permissive Unknown laundering).
#[test]
fn free_closure_str_pin_propagates_to_call_site() {
    entry_rejects(
        "f := fn(x): x.upper()\nfn main(): print(f(5))\n",
        "expected str",
    );
}

// Source #3 negative — a member shared by >1 type (`len` is on str/list/map/set/bytes) must NOT pin,
// EVEN when exactly one USER struct also has it: it is ambiguous → require an annotation. (Pre-fix
// the struct-only scan mis-pinned the param to the lone struct, wrongly rejecting a list call with
// "expected <Struct>".)
#[test]
fn free_closure_shared_member_does_not_pin() {
    let errs = check_entry(
        "struct Counter:\n    n: int\n    fn len(self) -> int:\n        return self.n\nf := fn(x): x.len()\nfn main(): print(f([1, 2, 3]))\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("cannot infer type of parameter 'x'")),
        "a member shared with builtins must be un-inferable, not pinned, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("expected Counter")),
        "param must NOT be mis-pinned to the lone struct, got: {errs:?}"
    );
}

// ===== first-class native (Rust-implemented) types: qualified / aliased module-member path =====
// These pin the ADDITIVE qualified-path surface (`concurrency.Shared[int]` etc.) that makes an
// import-gated native type behave like a `.chz` module type. The bare-after-import path is unchanged
// (covered by the existing concurrency/net/ffi/time tests).

#[test]
fn qualified_native_type_annotation_resolves() {
    // concurrency.Shared[int] in annotation position (whole-module import binds `concurrency`).
    entry_ok("import std.concurrency\nfn f(s: concurrency.Shared[int]):\n    print(s.get())\n");
    // aliased: `import std.concurrency as c` -> `c.Shared[int]`.
    entry_ok("import std.concurrency as c\nfn f(s: c.Shared[int]):\n    print(s.get())\n");
    // net.Socket / net.Listener (type-only, no ctor).
    entry_ok("import std.net\nfn f(s: net.Socket):\n    print(\"ok\")\n");
    entry_ok("import std.net\nfn f(s: net.Listener):\n    print(\"ok\")\n");
    // ffi ptr in annotation position; a qualified width there is a rule-1 error (TICKET-262).
    entry_ok("import std.ffi\nfn f(x: ffi.ptr) -> ffi.ptr:\n    return x\n");
    entry_rejects(
        "import std.ffi\nfn f(x: ffi.int32) -> ffi.ptr:\n    return ffi.null()\n",
        "describes the C boundary only",
    );
    // NEGATIVE: qualified access with NO import still rejects (gate stays sound).
    entry_rejects(
        "fn f(s: concurrency.Shared[int]):\n    print(\"x\")\n",
        "unknown module 'concurrency'",
    );
    // NEGATIVE: timer is a FUNCTION, not a type — reject in type position.
    entry_rejects(
        "import std.time\nfn f(x: time.timer):\n    print(\"x\")\n",
        "'timer' is a function, not a type",
    );
    // NEGATIVE: wrong arity on a qualified generic box.
    entry_rejects(
        "import std.concurrency\nfn f(s: concurrency.Shared):\n    print(\"x\")\n",
        "expects 1 type argument",
    );
}

#[test]
fn qualified_native_ctor_call_infers() {
    // concurrency.Shared(0) / aliased c.Shared(0) construct + method-call.
    entry_ok(
        "import std.concurrency\nfn main():\n    s := concurrency.Shared(0)\n    print(s.get())\nmain()\n",
    );
    entry_ok(
        "import std.concurrency as c\nfn main():\n    s := c.Shared(0)\n    print(s.get())\nmain()\n",
    );
    entry_ok(
        "import std.concurrency\nfn main():\n    r := concurrency.RwShared(0)\n    print(r.read(fn(x): x))\nmain()\n",
    );
    entry_ok(
        "import std.concurrency\nfn main():\n    a := concurrency.Atomic(0)\n    print(a.load())\nmain()\n",
    );
    entry_ok(
        "import std.concurrency\nfn main():\n    ex := concurrency.Executor()\n    ex.shutdown()\nmain()\n",
    );
    // time.timer(ms) -> Channel[bool].
    entry_ok("import std.time\nfn main():\n    t := time.timer(0)\n    print(t.recv())\nmain()\n");
    // NEGATIVE: net.Socket(...) has no from-nothing constructor.
    entry_rejects(
        "import std.net\nfn main():\n    s := net.Socket()\n    print(\"x\")\nmain()\n",
        "has no constructor",
    );
}

#[test]
fn alias_over_qualified_builtin() {
    // type alias over a qualified builtin.
    entry_ok(
        "import std.concurrency\ntype S = concurrency.Shared[int]\nfn f(s: S):\n    print(s.get())\n",
    );
}

#[test]
fn ffi_qualified_width_in_extern_sig() {
    // A qualified FFI width name inside an extern signature must type-check (resolve_ctype_d must map
    // ffi.int32 -> CType::Int32 so the marshal gate accepts it).
    entry_ok(
        "import std.ffi\nextern \"libc.so.6\":\n    fn abs(x: ffi.int32) -> ffi.int32\n\nfn main():\n    print(abs(-5))\nmain()\n",
    );
}

// ===== Swift-style keyword arguments through a function VALUE (SE-0111 labels, surface-only) =====

/// Labels on a `fn(...)` TYPE are SURFACE-ONLY: `fn(str)->nil` and `fn(name:str)->nil` are the same
/// type, mutually assignable (a labelled fn value flows into an unlabelled param and vice-versa).
#[test]
fn kw_labels_are_surface_only() {
    // A labelled user-fn value flows into an UNLABELLED fn param.
    ok(
        "fn use_it(f: fn(str) -> None):\n    f(\"a\")\nfn greet(name: str):\n    print(name)\nuse_it(greet)\n",
    );
    // A closure passed to a LABELLED fn param (labels ignored in the arity/assignability check).
    ok("fn use_it(f: fn(name: str) -> None):\n    f(\"a\")\nuse_it(fn(s: str): print(s))\n");
    entry_ok(
        "fn use_it(f: fn(str) -> None):\n    f(\"a\")\nfn greet(name: str):\n    print(name)\nfn main():\n    use_it(greet)\nmain()\n",
    );
}

/// A value call carrying keyword arguments resolves against the value's labels (both by-name and
/// reordered), through both the single-module and multi-module check paths.
#[test]
fn kw_value_call_accepts() {
    ok(
        "fn greet(name: str, greeting: str):\n    print(greeting, name)\ng := greet\ng(name=\"Bob\", greeting=\"Hi\")\n",
    );
    // Reordered keywords.
    ok(
        "fn greet(name: str, greeting: str):\n    print(greeting, name)\ng := greet\ng(greeting=\"Hi\", name=\"Bob\")\n",
    );
    // Positional still works alongside (named empty → fast path).
    ok(
        "fn greet(name: str, greeting: str):\n    print(greeting, name)\ng := greet\ng(\"Bob\", \"Hi\")\n",
    );
    entry_ok(
        "fn greet(name: str, greeting: str):\n    print(greeting, name)\nfn main():\n    g := greet\n    g(name=\"Bob\", greeting=\"Hi\")\nmain()\n",
    );
}

/// TICKET-139/W14-2: a keyword argument through a HOF PARAMETER is rejected — the annotation's
/// labels are surface-only, so the runtime callee may name its parameters differently.
#[test]
fn kw_value_call_through_a_param_is_rejected() {
    rejects(
        "fn apply(f: fn(name: str) -> None):\n    f(name=\"X\")\napply(fn(name: str): print(name))\n",
        "need a binding that holds one known function",
    );
    entry_rejects(
        "fn apply(f: fn(name: str) -> None):\n    f(name=\"X\")\nfn main():\n    apply(fn(name: str): print(name))\nmain()\n",
        "need a binding that holds one known function",
    );
}

/// An unknown label through a value is a clean type error that NAMES the bad label.
#[test]
fn kw_value_unknown_label_rejected() {
    rejects(
        "fn greet(name: str):\n    print(name)\ng := greet\ng(nope=\"x\")\n",
        "unknown named argument 'nope'",
    );
    entry_rejects(
        "fn greet(name: str):\n    print(name)\nfn main():\n    g := greet\n    g(nope=\"x\")\nmain()\n",
        "unknown named argument 'nope'",
    );
}

/// **The old SCOPE-CUT, now lifted.** A value call omitting a defaulted parameter used to be a type
/// error — *"a call through a function value must supply every parameter (defaults do not apply
/// through a value)"* — because nothing could fill it: the caller has no signature to consult and the
/// callee had no prologue. The callee fills its own trailing defaults now, so `h := hasdefault; h()`
/// is legal and prints `5`. Measured on `0104d57b`: `'closure' expects 1 argument(s), got 0`.
///
/// TICKET-197: a value read from one declaration fills every default it can name at the call site,
/// so a middle hole (`h(1, c=9)`) is legal too. Only a callee-filled default (one naming a type
/// parameter) still refuses a middle hole: a short call pushes fewer values, which cannot express
/// a middle gap, and the message says so.
#[test]
fn kw_value_defaults_fill_a_trailing_gap_but_not_a_middle_one() {
    // Direct call fills the default (a desugar rewrite) — unchanged.
    ok_desugared("fn hasdefault(x: int = 5):\n    print(x)\nhasdefault()\n");
    // Through a value, positionally: the callee fills it.
    ok_desugared("fn hasdefault(x: int = 5):\n    print(x)\nh := hasdefault\nh()\n");
    // Through a value, by keyword, omitting only the TRAILING default.
    ok_desugared(
        "fn f(a: int, b: int = 2, c: int = 3) -> int:\n    return a\nh := f\nprint(h(a=1, b=7))\n",
    );
    // A MIDDLE hole is filled at the call site: the value carries its declaration's defaults.
    ok_desugared(
        "fn f(a: int, b: int = 2, c: int = 3) -> int:\n    return a\nh := f\nprint(h(1, c=9))\n",
    );
    // Only a callee-filled default (one naming a type parameter) still refuses a middle hole.
    rejects_desugared(
        "fn f[T](a: T, b: List[T] = List[T](), c: int = 3) -> int:\n    return c\nh := f[int]\nprint(h(1, c=9))\n",
        "is filled by the callee and can only be omitted from the END of a call",
    );
    // A parameter with NO default is still required through a value.
    rejects_desugared(
        "fn need(a: int, b: int) -> int:\n    return a\nh := need\nprint(h(1))\n",
        "expects 2 argument(s), got 1",
    );
    // …and supplying the optional tail through a value still works (it used to be truncated away
    // for a CROSS-MODULE fn value, making `f(url, ms)` a spurious "too many arguments").
    ok_desugared("fn f(a: int, b: int = 2) -> int:\n    return a\nh := f\nprint(h(1, 5))\n");
}

/// **Optional arity is DIRECTIONAL, and storing a value into a function-typed binding must respect
/// it.** A binding whose type admits a short call may actually be CALLED short, so a value that
/// requires more arguments cannot be stored in it. Because the optional arity rides on the
/// equality-neutral label wrapper, plain type equality says these two `fn(int) -> int`s are the same
/// type — assignability has to carry the direction instead. Without it, `chezzi check` said
/// *ok: no type errors* and the run then faulted with
/// `function 'b' expects 1 argument(s), got 0`.
///
/// The reverse must stay legal: a defaulted fn is strictly more permissive, so it flows into a plain
/// `fn(int) -> int` slot.
#[test]
fn a_function_value_needing_more_arguments_cannot_be_stored_where_fewer_are_promised() {
    // Reassignment: `h` is typed from `a` (may be called with 0), `b` requires 1.
    rejects_desugared(
        "fn a(x: int = 1) -> int:\n    return x\nfn b(x: int) -> int:\n    return x * 2\nfn main():\n    h := a\n    h = b\n    print(h())\n",
        "cannot assign",
    );
    // A CLOSURE is exact-arity at runtime, so the same rule must cover it.
    rejects_desugared(
        "fn g(x: int = 5) -> int:\n    return x\nfn main():\n    c := 2\n    fn h(x: int) -> int:\n        return x + c\n    f := g\n    f = h\n    print(f())\n",
        "cannot assign",
    );
    // A list literal cannot silently join them either.
    rejects_desugared(
        "fn a(x: int = 1) -> int:\n    return x\nfn b(x: int) -> int:\n    return x\nfn main():\n    xs := [a, b]\n    for f in xs:\n        print(f())\n",
        "differ",
    );
    // The permissive direction stays legal: a defaulted fn into a plain `fn(int) -> int` slot.
    ok_desugared(
        "fn a(x: int = 1) -> int:\n    return x\nfn take(f: fn(int) -> int) -> int:\n    return f(2)\nfn main():\n    print(take(a))\n",
    );
}

/// A GENERIC method whose trailing default is callee-filled: `desugar` omits the argument, so the
/// generic method arm must accept the short call like every other method arm. It was the one arm the
/// range was not threaded into, so the checker rejected the user's own call —
/// `'tot' expects 2 argument(s), got 1`.
#[test]
fn a_generic_methods_callee_filled_default_may_be_omitted() {
    ok_desugared(
        "struct G[T]:\n    v: T\n    fn tot[U](self, u: U, xs: List[Self] = mkl()) -> int:\n        return xs.len()\nfn mkl[T]() -> List[G[T]]:\n    return []\nfn main():\n    print(G(1).tot(2))\n",
    );
    // …and the neighbour that must NOT be relaxed with it: a genuinely missing argument.
    rejects_desugared(
        "struct G[T]:\n    v: T\n    fn tot[U](self, u: U, xs: List[Self] = mkl()) -> int:\n        return xs.len()\nfn mkl[T]() -> List[G[T]]:\n    return []\nfn main():\n    print(G(1).tot())\n",
        "argument(s), got 0",
    );
}

/// A first-class BUILTIN function value takes NO keyword arguments (labels are a user-fn surface).
#[test]
fn kw_value_builtin_rejects_keywords() {
    rejects("p := ord\np(x=\"a\")\n", "takes no keyword arguments");
}

// ----- TICKET-139/W14-2: a keyword call through a fn value needs ONE known callee -----

/// Two fns of the SAME type `fn(int, int) -> int` whose parameter names are swapped.
const KW_SWAP_FNS: &str = "fn f(a: int, b: int) -> int:\n    return a * 100 + b\nfn ren(b: int, a: int) -> int:\n    return a * 100 + b\n";

/// Must still fail: a list slot may hold either fn, so its labels are not certain.
#[test]
fn kw_value_call_through_a_list_slot_is_rejected() {
    rejects(
        &format!("{KW_SWAP_FNS}fs := [f, ren]\nprint(fs[1](a=1, b=2))\n"),
        "need a binding that holds one known function",
    );
}

/// Must still fail: a binding that is written anywhere in its lifetime — before the call, after it
/// in a loop, from a nested fn, or (module scope) from a fn body — may hold a different fn.
#[test]
fn kw_value_call_through_a_reassigned_binding_is_rejected() {
    rejects(
        &format!("{KW_SWAP_FNS}h := f\nh = ren\nprint(h(a=1, b=2))\n"),
        "is reassigned",
    );
    rejects(
        &format!(
            "{KW_SWAP_FNS}fn main():\n    h := f\n    for i in 0..2:\n        print(h(a=1, b=2))\n        h = ren\nmain()\n"
        ),
        "is reassigned",
    );
    rejects(
        &format!(
            "{KW_SWAP_FNS}fn main():\n    h := f\n    fn w():\n        h = ren\n    w()\n    print(h(a=1, b=2))\nmain()\n"
        ),
        "is reassigned",
    );
    rejects(
        &format!("{KW_SWAP_FNS}h := f\nfn w():\n    h = ren\nprint(h(a=1, b=2))\n"),
        "is reassigned",
    );
}

/// Must still pass: a binding certain to hold one known fn keeps its keyword call, and a write to a
/// same-named binding in ANOTHER fn does not leak into it.
#[test]
fn kw_value_call_through_one_known_fn_stays_ok() {
    ok(&format!("{KW_SWAP_FNS}h := ren\nprint(h(a=1, b=2))\n"));
    ok("g := fn(a: int, b: int) -> int: a * 100 + b\nprint(g(b=2, a=1))\n");
    ok(
        "fn main():\n    fn inner(a: int, b: int) -> int:\n        return a * 100 + b\n    print(inner(b=2, a=1))\nmain()\n",
    );
    ok(&format!(
        "{KW_SWAP_FNS}fn p():\n    h := f\n    print(h(a=1, b=2))\nfn q():\n    h := ren\n    h = f\n    print(h(1, 2))\n"
    ));
}

// ===== Variadic parameters + `Any` top type (M-variadic) =====

/// `Any` is an empty structural protocol → every type (scalars included) satisfies it.
#[test]
fn any_top_type_accepts_scalars() {
    ok("fn w():\n    x: Any = 1\n    y: Any = \"s\"\n    z: Any = true\n    f: Any = 1.5\n");
}

/// `Any` as a param type accepts an int and a struct value.
#[test]
fn any_param_accepts_anything() {
    entry_ok(
        "struct P:\n    x: int\n\nfn g(v: Any) -> None:\n    return\n\nfn main():\n    g(1)\n    g(P(x=1))\n    g(\"hi\")\n",
    );
}

/// A direct `satisfies` unit check: every scalar satisfies the empty `Any` protocol.
#[test]
fn any_satisfied_by_all_scalars_unit() {
    let c = Checker::new();
    for ty in [Ty::Int, Ty::Float, Ty::Bool, Ty::Str, Ty::Nil] {
        assert!(
            c.satisfies_args(&ty, "Any", &[]).is_ok(),
            "{ty} should satisfy Any"
        );
    }
}

/// `fn_sig` collapses a variadic param to a `List[T]` slot and records its index.
#[test]
fn fn_sig_variadic_collapses_to_list() {
    let module =
        parser::parse(lexer::tokenize("fn f(...xs: int) -> int:\n    return 0\n").unwrap())
            .unwrap();
    let StmtKind::Fn(decl) = &module.stmts[0].kind else {
        panic!("expected fn");
    };
    let mut c = Checker::new();
    let sig = c.fn_sig(decl, Span::default(), None);
    assert_eq!(sig.params, vec![Ty::List(Box::new(Ty::Int))]);
    assert_eq!(sig.variadic, Some(0));
}

/// `fn_sig` keeps pre-variadic + post-variadic (keyword-only) slots around the collapsed list.
#[test]
fn fn_sig_variadic_middle_slot() {
    let module = parser::parse(
        lexer::tokenize("fn f(a: str, ...xs: int, flag: bool = false) -> int:\n    return 0\n")
            .unwrap(),
    )
    .unwrap();
    let StmtKind::Fn(decl) = &module.stmts[0].kind else {
        panic!("expected fn");
    };
    let mut c = Checker::new();
    let sig = c.fn_sig(decl, Span::default(), None);
    assert_eq!(
        sig.params,
        vec![Ty::Str, Ty::List(Box::new(Ty::Int)), Ty::Bool]
    );
    assert_eq!(sig.variadic, Some(1));
}

/// A variadic user fn collapses its call to a `List[T]` slot and type-checks end-to-end (desugared).
#[test]
fn variadic_call_typechecks_desugared() {
    let errs = check_desugared(
        "fn sum_all(...xs: int) -> int:\n    total := 0\n    for x in xs:\n        total = total + x\n    return total\n\nfn main():\n    a := sum_all(1, 2, 3)\n    b := sum_all()\n",
    );
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

/// A heterogeneous variadic arg (`bool` into `...xs: int`) is a compile error (via the collapsed
/// `List` literal's element-type check).
#[test]
fn variadic_element_type_enforced() {
    let errs = check_desugared("fn f(...xs: int):\n    return\n\nfn main():\n    f(1, true)\n");
    assert!(
        !errs.is_empty(),
        "expected a type error for bool into ...xs: int"
    );
}

/// A variadic typed with the `Any` top type accepts HETEROGENEOUS arguments — the flagship reason
/// `Any` is the "honest element type". `f(1, "a", true)` collapses to a `List[Any]` whose every
/// element vacuously satisfies the empty `Any` protocol, so the synthesized list literal must be
/// checked against the declared `List[Any]` slot (expected-type-directed), NOT unified bottom-up.
/// (Regression: it previously failed with "list elements differ: int vs str".)
#[test]
fn variadic_any_accepts_heterogeneous() {
    let errs = check_desugared(
        "fn describe(...xs: Any) -> int:\n    return xs.len()\n\nfn main():\n    n := describe(1, \"a\", true)\n",
    );
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

/// The same expected-type-directed rescue for an EXPLICIT `List[Any]` annotation binding a
/// heterogeneous list literal (the non-variadic surface of the same fix).
#[test]
fn list_any_annotation_accepts_heterogeneous() {
    let errs =
        check_desugared("fn main():\n    xs: List[Any] = [1, \"a\", true]\n    n := xs.len()\n");
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

/// The same expected-type-directed path generalizes beyond `Any`: a `List[Shape]` annotation accepts
/// a literal of DIFFERING concrete types when each satisfies the declared protocol element type
/// (sound — each element is genuinely assignable to `Shape`).
#[test]
fn list_protocol_annotation_accepts_mixed_concrete() {
    let errs = check_desugared(
        "protocol Shape:\n    fn area(self) -> int\n\nstruct Circle:\n    r: int\n    fn area(self) -> int:\n        return self.r\n\nstruct Square:\n    s: int\n    fn area(self) -> int:\n        return self.s\n\nfn main():\n    shapes: List[Shape] = [Circle(r=2), Square(s=3)]\n    n := shapes.len()\n",
    );
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

/// A heterogeneous literal into a NON-top concrete element slot (`List[int]`) still errors — the
/// expected-type path only rescues when EVERY element is assignable to the declared element type.
#[test]
fn list_int_annotation_still_rejects_heterogeneous() {
    let errs = check_desugared("fn main():\n    xs: List[int] = [1, \"a\"]\n");
    assert!(
        !errs.is_empty(),
        "expected a type error for str into List[int]"
    );
}

// ===== print ported to a variadic `native fn` decl =====

#[test]
fn print_zero_args_ok() {
    ok("fn w():\n    print()\n");
}

#[test]
fn print_variadic_with_sep_end_ok() {
    ok("fn w():\n    print(1, \"a\", true, sep=\"-\", end=\"!\")\n");
}

#[test]
fn print_sep_non_str_rejects() {
    rejects("fn w():\n    print(1, sep=5)\n", "str");
}

#[test]
fn print_builtin_sig_present_after_port() {
    // print retains a builtin_sig (hover) after retiring the synthetic sig_print.
    let mut c = Checker::new();
    c.seed_native_prelude_sigs();
    assert!(c.builtin_sig("print").is_some());
}

/// W7-51 lifted the old v1 limit this test used to pin. A variadic call in a parameter DEFAULT was a
/// compile error (`'sum_all' expects 1 argument(s), got 3`) because the variadic collapse was gated
/// to desugar's FIRST pass while a default was spliced raw AFTER it, so the spliced call never
/// collapsed. The default now lives in a provider function walked exactly once, like any other body,
/// so the collapse fires normally. Measured on `b1307258`: two type errors; here: clean, and
/// `chezzi run` prints `0`.
#[test]
fn variadic_call_as_param_default_collapses() {
    let errs = check_desugared(
        "fn sum_all(...xs: int) -> int:\n    return 0\n\nfn g(x: int = sum_all(1, 2, 3)) -> int:\n    return x\n\nfn main():\n    print(g())\n",
    );
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

/// W7-51, the one ACCEPTED narrowing. A `?` inside a default used to propagate out of the CALLER —
/// the default was cloned into the caller's body, so its `Err` early-returned from whatever function
/// omitted the argument. A default is now compiled once, in its defining module, where there is no
/// caller frame to propagate into, so the shape is refused. The message is specific: the generic
/// `'?' used in a function that returns int` would name a return type the user never wrote (the
/// provider is declared `-> <the parameter's type>`).
#[test]
fn a_try_inside_a_default_is_rejected_with_the_defining_module_reason() {
    // Exactly ONE message, the tailored one, whatever the ENCLOSING fn returns. The decl-site copy
    // of a default is not in that fn's body — it runs in its provider — so validating its `?`
    // against the enclosing `current_ret` both named a return type that does not describe where the
    // default runs and duplicated this diagnostic. Measured on `e2d9bd4e`: the `-> int` shape below
    // emitted TWO errors at one span (the stale `'?' used in a function that returns int` then this
    // one) while `-> int!str` emitted only this one.
    for ret in ["int", "int!str"] {
        let body = if ret == "int" {
            "return x"
        } else {
            "return ?x"
        };
        let errs = check_desugared(&format!(
            "fn getr() -> str!str:\n    return ?\"wxyz\"\n\nfn f(x: int = getr()?.len()) -> {ret}:\n    {body}\n\nfn main():\n    print(1)\n"
        ));
        let tailored: Vec<_> = errs
            .iter()
            .filter(|e| {
                e.message
                    .contains("a default expression cannot propagate with `?`")
            })
            .collect();
        assert_eq!(
            tailored.len(),
            1,
            "[-> {ret}] expected exactly one tailored message, got: {errs:?}"
        );
        assert_eq!(
            errs.len(),
            1,
            "[-> {ret}] the tailored message must be the ONLY error, got: {errs:?}"
        );
    }
}

/// W7-51 — a struct FIELD default's `?` gets the same single tailored message. The field decl-site
/// copy is inferred at module level (`current_ret == Nil`, `!in_fn_body`), so it never duplicated;
/// this pins that the parameter-side fix did not perturb it.
#[test]
fn a_try_inside_a_field_default_is_rejected_once() {
    let errs = check_desugared(
        "fn getr() -> str!str:\n    return ?\"wxyz\"\n\nstruct S:\n    n: int = getr()?.len()\n\nfn main():\n    print(1)\n",
    );
    assert_eq!(
        errs.len(),
        1,
        "expected exactly one error for a field default's `?`, got: {errs:?}"
    );
    assert!(
        errs[0]
            .message
            .contains("a default expression cannot propagate with `?`"),
        "got: {errs:?}"
    );
}

/// W7-51 — a default whose EXPRESSION mentions an enclosing type parameter keeps the inline clone.
/// `dflt_for` used to key only on the parameter's TYPE, so `x: int = mk[T]().n` got a provider
/// declared `-> int` with `T` unbound and picked up two extra errors at the same span. Measured:
/// `b1307258` 2 errors, `e2d9bd4e` 4 (`unknown type 'T'` + an undetermined-witness message), here 2.
/// The shape is rejected either way — this is about the diagnostic being legible.
#[test]
fn a_default_expression_mentioning_a_type_param_stays_inline() {
    let errs = check_desugared(
        "protocol Default:\n    fn default() -> Self\n\nstruct N:\n    n: int\n    fn default() -> N:\n        return N(0)\n\nfn mk[T: Default]() -> T:\n    return T.default()\n\nfn g[T: Default](x: int = mk[T]().n) -> int:\n    return x\n\nfn main():\n    print(1)\n",
    );
    assert_eq!(
        errs.len(),
        2,
        "expected the 2 pre-existing errors, got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("unknown type 'T'")),
        "a provider must not be synthesized with T unbound, got: {errs:?}"
    );
}

/// …and the neighbour that keeps the carve-out from over-firing: a generic fn whose default mentions
/// NO type parameter still gets a provider, so it still resolves in its defining module.
#[test]
fn a_generic_fns_type_param_free_default_still_gets_a_provider() {
    ok_desugared(
        "fn sep() -> str:\n    return \",\"\n\nfn join_all[T](xs: List[T], s: str = sep()) -> str:\n    return s\n\nfn main():\n    print(join_all([1, 2]))\n",
    );
}

/// …and the neighbours the premise implies must stay legal, since this rule WIDENS what is rejected
/// (project memory: "a widening is untested by its own suite"). An Option-mode `?.` and a `??` carry
/// no propagation, so both still work inside a default; and the very same `?` expression is still
/// fine when it is NOT a default (an ordinary body in a Result-returning fn).
#[test]
fn a_non_propagating_carrier_in_a_default_and_a_try_outside_one_stay_legal() {
    ok_desugared(
        "fn geto() -> str?:\n    return ?\"abc\"\n\nfn f(x: int? = geto()?.len()) -> int:\n    return 1\n\nfn main():\n    print(f())\n",
    );
    ok_desugared(
        "fn geto() -> int?:\n    return ?3\n\nfn f(x: int = geto() ?? 0) -> int:\n    return x\n\nfn main():\n    print(f())\n",
    );
    ok_desugared(
        "fn getr() -> str!str:\n    return ?\"wxyz\"\n\nfn f() -> int!str:\n    return ?(getr()?.len())\n\nfn main():\n    print(1)\n",
    );
}

/// W7-51 — the provider NAME must be injective. `$def$<file>$<owner>.<param>$` was not: a struct
/// field (owner `S`, param `m`) and a same-named free function's parameter (owner `S`, param `m`)
/// hashed to one name, so `desugar` emitted two `fn`s called `$def$2$S.m$` and the checker reported
/// `function '$def$2$S.m$' is already defined` — an internal symbol in a user-facing message, and in
/// `--errors=json`, i.e. the editor squiggle. Both declarations are legal (`b1307258`: `ok: no type
/// errors`; `S(3)` resolves to the constructor). The name now carries the slot KIND.
#[test]
fn a_struct_field_and_a_same_named_fn_parameter_do_not_share_a_provider() {
    let errs = check_desugared(
        "fn g() -> int:\n    return 7\n\nstruct S:\n    m: int = g()\n\nfn S(m: int = g()) -> int:\n    return m\n",
    );
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
    // The neighbours that were already fine, so the fix is not just "the collision moved": two
    // structs sharing a field name, two fns sharing a param name, a field plus a METHOD param.
    ok_desugared(
        "fn g() -> int:\n    return 7\n\nstruct A:\n    m: int = g()\n\nstruct B:\n    m: int = g()\n",
    );
    ok_desugared(
        "fn g() -> int:\n    return 7\n\nfn p(m: int = g()) -> int:\n    return m\n\nfn q(m: int = g()) -> int:\n    return m\n",
    );
    ok_desugared(
        "fn g() -> int:\n    return 7\n\nstruct T:\n    m: int = g()\n    fn mm(self, m: int = g()) -> int:\n        return m\n",
    );
}

/// …and the property behind it: no diagnostic may name a provider. The symbol is unspellable on
/// purpose, so a message carrying one is unreadable AND unactionable — the user cannot see the
/// declaration it names. Checked over the erroring default shapes this branch touches.
#[test]
fn no_diagnostic_leaks_a_provider_symbol() {
    for src in [
        // the collision above, plus a call site
        "fn g() -> int:\n    return 7\n\nstruct S:\n    m: int = g()\n\nfn S(m: int = g()) -> int:\n    return m\n\nfn main():\n    print(S(3))\n",
        // a wrong-typed non-literal default
        "fn g() -> str:\n    return \"s\"\n\nfn f(x: int = g()) -> int:\n    return x\n",
        // a default whose expression is an unknown name
        "fn f(x: int = nope()) -> int:\n    return x\n",
        // `?` propagating out of a default
        "fn getr() -> str!str:\n    return ?\"w\"\n\nfn f(x: int = getr()?.len()) -> int:\n    return x\n",
    ] {
        for e in check_desugared(src) {
            assert!(
                !e.message
                    .contains(crate::base::provider_name::PROVIDER_PREFIX),
                "internal provider symbol reached a user-facing message: {}",
                e.message
            );
        }
    }
}

/// W7-51 — a `?` in a default that does NOT become a provider must still be judged where it is
/// written. `check_fn_body` neutralises `current_ret`/`in_fn_body` over the decl-site copy because
/// the provider body judges the `?` instead — but `dflt_for` emits no provider for an un-annotated
/// parameter, nor for one whose type or expression mentions an enclosing type parameter or `Self`,
/// so for those the neutralisation silenced the only judge there was. Measured: `b1307258` **1 type
/// error**, `dfdc7a1b` **`ok: no type errors`** (the error reappearing only if someone CALLED `f`).
#[test]
fn a_try_in_a_carve_out_default_is_still_rejected_at_the_declaration() {
    // The type-parameter carve-out: no provider, so the decl-site check is the only one.
    rejects_desugared(
        "fn mk[T]() -> T!str:\n    return !\"no\"\n\nfn f[T](x: T = mk[T]()?) -> T:\n    return x\n",
        "'?' used in a function that returns T",
    );
    // The `Self` carve-out, same reasoning — but only a GENERIC host still takes it. On a
    // non-generic host `Self` names one concrete type, so `desugar` now substitutes it and the
    // default gets a real provider (see the sibling test below); `Q[T]`'s `Self` is `Q[T]`, whose
    // `T` is unbound in the free `fn` a provider is, so that one keeps the inline clone and the
    // decl-site copy stays the only judge there is.
    rejects_desugared(
        "struct Q[T]:\n    v: T\n    fn c(self, o: Self = mkq()?) -> int:\n        return 1\n\nfn mkq[T]() -> Q[T]!str:\n    return !\"no\"\n",
        "'?' used in a function that returns int",
    );
    // …and the provider path keeps its ONE tailored message (the reason the neutralisation exists).
    let errs = check_desugared(
        "fn getr() -> str!str:\n    return ?\"wxyz\"\n\nfn f(x: int = getr()?.len()) -> int:\n    return x\n\nfn main():\n    print(1)\n",
    );
    assert_eq!(
        errs.len(),
        1,
        "expected one tailored message, got: {errs:?}"
    );
    assert!(
        errs[0]
            .message
            .contains("a default expression cannot propagate with `?`"),
        "got: {errs:?}"
    );
}

/// A `Self`-typed parameter with a non-literal default must keep type-checking. W7-51 made these
/// shapes keep the INLINE clone, because a provider is a free top-level `fn` and `Self` is not
/// spellable in one (`docs/syntax.md`: naming `Self` as a free-fn parameter is `unknown type
/// 'Self'`); between `e2d9bd4e` and `dfdc7a1b` they stopped compiling altogether — `type error
/// (line 3, col 36): unknown type 'Self'` — where `b1307258` ran them (`6`, `6`, `2`), and no test
/// in the tree paired `Self` with a default, which is why that shipped green.
///
/// They now get a real **provider** instead: on a NON-generic host `Self` names exactly one
/// concrete type, so `desugar::subst_self_ty` rewrites it in the provider's declared return type.
/// The assertions below are unchanged — what changed is that the default now resolves in its
/// DEFINING module rather than being cloned into the caller. Measured, release CLI, a definer
/// declaring `fn mkq() -> Q: return Q(5)` and a caller declaring its own `fn mkq() -> Q: return
/// Q(900)`: `0104d57b` printed **901** (the caller's), and this prints **6** (the definer's).
/// The runnable half lives in `tests/chz/spec/default_scope_chain_test.chz`.
#[test]
fn a_self_typed_parameter_default_still_type_checks() {
    for src in [
        // `Self` in the type, provider-shaped expression
        "struct Q:\n    n: int\n    fn combine(self, other: Self = mkq()) -> int:\n        return self.n + other.n\n\nfn mkq() -> Q:\n    return Q(5)\n\nfn main():\n    print(Q(1).combine())\n",
        // `Self` in the type, ctor-call expression
        "struct Q:\n    n: int\n    fn combine(self, other: Self = Q(5)) -> int:\n        return self.n + other.n\n\nfn main():\n    print(Q(1).combine())\n",
        // `Self` NESTED in the type
        "struct Q:\n    n: int\n    fn tot(self, xs: List[Self] = mkl()) -> int:\n        return self.n + xs.len()\n\nfn mkl() -> List[Q]:\n    return [Q(2)]\n\nfn main():\n    print(Q(1).tot())\n",
        // an ENUM host
        "enum E:\n    A\n    B\n    fn pick(self, other: Self = mke()) -> int:\n        return 1\n\nfn mke() -> E:\n    return E.B\n\nfn main():\n    print(E.A.pick())\n",
    ] {
        ok_desugared(src);
    }
    // …and the neighbour that keeps the carve-out from over-firing: the SAME default under the
    // spelled-out receiver type still gets a provider, so it still resolves in its own module.
    ok_desugared(
        "struct Q:\n    n: int\n    fn combine(self, other: Q = mkq()) -> int:\n        return self.n + other.n\n\nfn mkq() -> Q:\n    return Q(5)\n\nfn main():\n    print(Q(1).combine())\n",
    );
    // …and the GENERIC host, which still cannot be substituted (`Self` is `Q[T]`, and `T` is
    // unbound in a free `fn`) and therefore still takes the inline carve-out. Pinned so a later
    // cleanup cannot assume the carve-out is gone.
    ok_desugared(
        "struct Q[T]:\n    v: T\n    fn tot(self, xs: List[Self] = mkl()) -> int:\n        return xs.len()\n\nfn mkl[T]() -> List[Q[T]]:\n    return []\n\nfn main():\n    print(Q(1).tot())\n",
    );
}

/// A METHOD default's `?` must produce **exactly one** diagnostic — the tailored provider one.
///
/// `check_fn_body` decides whether to neutralise the decl-site copy by asking whether `desugar`
/// emitted a provider, looking it up under the name `desugar` would have chosen. `desugar` names a
/// method's provider with the type's **bare** name, but `Ty::Struct`'s key is the module-scoped
/// IDENTITY key (`<module-key>::Name`), so the two never matched and the decl-site copy re-judged a
/// `?` the provider body had already judged. Measured on `0104d57b`, release CLI:
/// `struct Q: fn c(self, o: Q = mkq()?)` gave **2 type errors** — the stale `'?' used in a function
/// that returns int` plus the tailored one — where the free-fn shape correctly gave **1**.
///
/// **This must go through `check_entry`, not `check_desugared`.** The single-module helpers key
/// types by their BARE name, under which the lookup accidentally matched, so the bug was invisible
/// to every `*_desugared` test and visible only to the CLI or a multi-module program.
#[test]
fn a_method_defaults_try_is_judged_once_not_twice() {
    for src in [
        // struct host
        "struct Q:\n    n: int\n    fn c(self, o: Q = mkq()?) -> int:\n        return self.n + o.n\n\nfn mkq() -> Q!str:\n    return ?Q(5)\n",
        // `Self`-typed on a non-generic host — the same path, now that `Self` substitutes
        "struct Q:\n    n: int\n    fn c(self, o: Self = mkq()?) -> int:\n        return self.n + o.n\n\nfn mkq() -> Q!str:\n    return ?Q(5)\n",
    ] {
        let errs = check_entry(src);
        assert_eq!(
            errs.len(),
            1,
            "expected exactly one diagnostic, got: {errs:?}"
        );
        assert!(
            errs[0]
                .message
                .contains("a default expression cannot propagate with `?`"),
            "expected the tailored provider message, got: {errs:?}"
        );
    }
    // The free-fn shape was always correct; pinned as the control.
    let errs = check_entry(
        "fn getr() -> str!str:\n    return ?\"wxyz\"\n\nfn f(x: int = getr()?.len()) -> int:\n    return x\n",
    );
    assert_eq!(errs.len(), 1, "free-fn control, got: {errs:?}");
}

// ===== variadic METHODS under same-name collisions across structs (regression) =====

/// A variadic method call `recv.m(a,b,c)` must collapse the surplus positionals even when ANOTHER
/// struct defines `m` with a different (non-variadic) param list. The receiver's struct type is
/// knowable pre-type (`a := A()`), so desugar resolves the exact variadic spec and collapses.
#[test]
fn variadic_method_positional_with_name_collision() {
    let errs = check_desugared(
        "struct A:\n    fn m(self, ...xs: int) -> int:\n        return xs.len()\n\nstruct B:\n    fn m(self, x: int) -> int:\n        return x\n\nfn main():\n    a := A()\n    print(a.m(1, 2, 3))\n",
    );
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

/// Same collision, but the receiver is a TYPED PARAMETER (`x: A`) rather than a let-bound local —
/// the desugar pass must still resolve the receiver's struct type to collapse the variadic call.
#[test]
fn variadic_method_typed_param_receiver_with_collision() {
    let errs = check_desugared(
        "struct A:\n    fn m(self, ...xs: int) -> int:\n        return xs.len()\n\nstruct B:\n    fn m(self, a: int, b: int) -> int:\n        return a + b\n\nfn use_a(x: A) -> int:\n    return x.m(1, 2, 3)\n\nfn main():\n    print(use_a(A()))\n",
    );
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

/// Two BOTH-variadic methods that differ only in the variadic param's NAME (`...xs` vs `...ys`) —
/// the name-keyed method table sees a disagreement, but the receiver-aware resolution picks the
/// right sibling and collapses.
#[test]
fn variadic_method_both_variadic_differ_by_param_name() {
    let errs = check_desugared(
        "struct A:\n    fn m(self, ...xs: int) -> int:\n        return xs.len()\n\nstruct B:\n    fn m(self, ...ys: int) -> int:\n        return ys.len() * 2\n\nfn main():\n    a := A()\n    print(a.m(1, 2, 3))\n",
    );
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

/// A keyword-only post-variadic param on a name-colliding method: `a.m(1, 2, flag=true)` fills the
/// variadic positionally and the keyword-only slot by name. Previously the collision guard emitted
/// an unsatisfiable "pass arguments positionally" error for a keyword-ONLY parameter.
#[test]
fn variadic_method_keyword_only_with_collision() {
    let errs = check_desugared(
        "struct A:\n    fn m(self, ...xs: int, flag: bool) -> int:\n        if flag:\n            return xs.len()\n        return 0\n\nstruct B:\n    fn m(self, x: int) -> int:\n        return x\n\nfn main():\n    a := A()\n    print(a.m(1, 2, flag=true))\n",
    );
    assert!(errs.is_empty(), "expected clean, got: {errs:?}");
}

// ===== Generic fn as a VALUE (scope A + B): pin type params from a known concrete fn type or an
// explicit turbofish; runtime is generic-ERASED. A bare un-pinned generic fn value stays an error.

#[test]
fn generic_fn_value_turbofish_ok() {
    // B — turbofish on a fn value: `g := ident[int]` → g : fn(int) -> int
    ok(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    g := ident[int]\n    print(g(5) + 1)\n",
    );
}

#[test]
fn generic_fn_value_annot_ok() {
    // A1 — annotated binding pins T=int from the annotation.
    ok(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    g: fn(int) -> int = ident\n    print(g(5) + 1)\n",
    );
}

#[test]
fn generic_fn_value_hofarg_ok() {
    // A2 — HOF argument pins T against the param type.
    ok(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn applyit(f: fn(int) -> int, x: int) -> int:\n    return f(x)\n\nfn main():\n    print(applyit(ident, 5) + 1)\n",
    );
}

#[test]
fn generic_fn_value_return_ok() {
    // A3 — return position pins T against the declared return type.
    ok(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn getf() -> fn(int) -> int:\n    return ident\n\nfn main():\n    g := getf()\n    print(g(5) + 1)\n",
    );
}

// ---- soundness rejects ----

#[test]
fn generic_fn_value_unsatisfiable_rejected() {
    // ident is fn(T)->T; can't be both str and int → concrete subst is fn(str)->str, rejected vs fn(str)->int.
    rejects(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    g: fn(str) -> int = ident\n",
        "cannot assign fn(str) -> str to variable of type fn(str) -> int",
    );
}

#[test]
fn generic_fn_value_turbofish_bound_violation_rejected() {
    rejects(
        "fn addone[T: Add](x: T) -> T:\n    return x + x\n\nfn main():\n    h := addone[bool]\n    print(h(true))\n",
        "Add",
    );
}

#[test]
fn generic_fn_value_annot_bound_violation_rejected() {
    rejects(
        "fn addone[T: Add](x: T) -> T:\n    return x + x\n\nfn main():\n    h: fn(bool) -> bool = addone\n",
        "Add",
    );
}

#[test]
fn generic_fn_value_turbofish_arity_mismatch_rejected() {
    rejects(
        "fn pair[A, B](a: A, b: B) -> A:\n    return a\n\nfn main():\n    p := pair[int]\n",
        "expects 2 type argument(s), found 1",
    );
}

#[test]
fn generic_fn_value_downstream_misuse_rejected() {
    // g := ident[int]; g(5) is int, not str.
    rejects(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    g := ident[int]\n    s: str = g(5)\n",
        "cannot assign int to variable of type str",
    );
}

// ---- must-not-regress ----

#[test]
fn bare_unpinned_generic_fn_value_stays_error() {
    // TICKET-238: a bare generic-fn value with no expected type is an error at its binding,
    // with or without a later call.
    rejects(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    g := ident\n    print(g(5))\n",
        "`ident[<T>]`",
    );
    let errs = check_src(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    g := ident\n    print(g)\n",
    );
    assert!(
        !errs.is_empty(),
        "bare un-pinned generic fn value must stay an error"
    );
}

/// …and the error lands at the BINDING, naming the fn and its undetermined type parameter — Go's
/// rule (`cannot use generic function id without instantiation`, reported at the read), with both
/// working spellings added. Before this, the binding was accepted and the CALL below reported
/// "argument 1 of 'closure': expected T, found int" — a `closure` the user never wrote, naming a `T`
/// there is no way to act on. A read that is never called at all was accepted silently.
#[test]
fn bare_unpinned_generic_fn_value_rejected_at_the_binding() {
    // TICKET-238: a later call does not pin the binding; the fix goes on its own line.
    rejects(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    g := ident\n    print(g(5))\n",
        "`ident[<T>]`",
    );
    ok(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    g := ident[int]\n    print(g(5))\n",
    );
    {
        let src =
            "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    g := ident\n    print(1)\n";
        let errs = check_src(src);
        let joined = errs
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            joined.contains("'ident' is generic and T is not determined here"),
            "expected the binding-site diagnostic, got: {joined}"
        );
        assert!(
            joined.contains("`ident[<T>]`") && joined.contains("fn(<T>) -> <T>"),
            "expected both working spellings, got: {joined}"
        );
        assert!(
            !joined.contains("closure"),
            "must not blame a 'closure' the user never wrote: {joined}"
        );
    }
    // TWO type params: both named, and the turbofish names both.
    let errs = check_src(
        "fn pair[A, B](a: A, b: B) -> A:\n    return a\n\nfn main():\n    g := pair\n    print(1)\n",
    );
    let joined = errs
        .iter()
        .map(|e| e.message.clone())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        joined.contains("'pair' is generic and A, B are not determined here")
            && joined.contains("fn(<A>, <B>) -> <A>")
            && joined.contains("instantiate it (`pair[<A>, <B>]`)"),
        "expected a two-param diagnostic with the two-arg turbofish, got: {joined}"
    );
    // …and the same read in a collection literal, where no element hint reaches it either. EXACTLY
    // ONE error: the guard returns `Ty::Unknown`, which made the one-element list look like an
    // unrefined EMPTY collection and drew a second, factually false *"cannot infer element type of
    // empty collection; add a type annotation"* (a pre-existing trap — the witness wall reached it too
    // via `xs := [reset]` — that this rule made far more reachable).
    for src in [
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    xs := [ident]\n    print(xs.len())\n",
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    m := {\"a\": ident}\n    print(m.len())\n",
    ] {
        let errs = check_src(src);
        assert!(
            errs.iter()
                .any(|e| e.message.contains("T is not determined here")),
            "a collection element is a value position too: {errs:?}"
        );
        assert!(
            !errs.iter().any(|e| e.message.contains("empty collection")),
            "a ONE-element literal is not an empty collection: {errs:?}"
        );
    }
    // A parameter DEFAULT now DOES carry its declared type into `expected_hint`, so a generic fn
    // value there is pinned by the slot it fills rather than reported. Measured: this program went
    // from *'ident' is generic and T is not determined here* to running and printing `5`.
    ok(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn run(x: int, f: fn(int) -> int = ident) -> int:\n    return f(x)\n\nfn main():\n    print(run(5))\n",
    );
    // The message must still stay TRUE in a position with no binding to annotate at all (`print`):
    // it may not claim "nothing determines T" or tell the user to annotate a binding.
    let joined =
        check_src("fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    print(ident)\n")
            .iter()
            .map(|e| e.message.clone())
            .collect::<Vec<_>>()
            .join(" | ");
    assert!(
        joined.contains("T is not determined here")
            && !joined.contains("nothing here determines")
            && !joined.contains("annotate the binding"),
        "the message must not claim more than it knows: {joined}"
    );
    // A DEFAULTED parameter must not be advertised with a stricter arity than a plain fn read gives:
    // `fn rep[T](x: T, n: int = 2)` suggests `fn(<T>, int) -> str`, which must still accept `g(1)`.
    let joined = check_src(
        "fn rep[T](x: T, n: int = 2) -> str:\n    return \"{x}{n}\"\n\nfn main():\n    g := rep\n    print(1)\n",
    )
    .iter()
    .map(|e| e.message.clone())
    .collect::<Vec<_>>()
    .join(" | ");
    assert!(
        joined.contains("fn(<T>, int = ) -> str")
            || joined.contains("fn(<T>, int) -> str")
            || joined.contains("fn(<T>, int = …) -> str"),
        "rendered signature: {joined}"
    );
}

/// The suppression that lets `[1,2,3].map(conv)` through belongs to the ONE prepass that re-pins its
/// argument. Set inside the shared `infer_generic_arg_tys` it covered all seven callers, so a generic
/// CTOR argument — where nothing pins afterwards — stayed silent and delivered the old
/// blame-the-later-call message this rule exists to replace.
#[test]
fn uninstantiated_generic_fn_value_as_a_generic_ctor_arg_rejected() {
    // TICKET-238: a later call does not pin it; the binding is rejected on its own line.
    rejects(
        "fn ident[T](x: T) -> T:\n    return x\n\nstruct Bx[T]:\n    f: T\n\nfn main():\n    b := Bx(ident)\n    print(b.f(3))\n",
        "'ident' is generic and T is not determined here",
    );
    rejects(
        "fn ident[T](x: T) -> T:\n    return x\n\nstruct Bx[T]:\n    f: T\n\nfn main():\n    b := Bx(ident)\n    print(b)\n",
        "'ident' is generic and T is not determined here",
    );
    // …the generic FREE-FN argument spelling of the same thing.
    rejects(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn take[U](f: U) -> int:\n    return 1\n\nfn main():\n    print(take(ident))\n",
        "'ident' is generic and T is not determined here",
    );
    // …while the pinning path is untouched: `.map`/`.fold`/`.filter` still pin a bare generic fn.
    ok(
        "fn conv[T](x: T) -> str:\n    return str(x)\n\nfn main():\n    xs := [1, 2, 3].map(conv)\n    s: str = xs[0]\n    print(s)\n",
    );
    ok(
        "fn add[T: Add](a: T, b: T) -> T:\n    return a + b\n\nfn main():\n    n: int = [1, 2, 3].fold(0, add)\n    print(n)\n",
    );
}

/// The INFERRED-return bypass: a first cut of the rule gated on `expected_hint.is_none()` and was
/// cancelled by its own `Ty::Unknown`. `fn get(): return id` has no declared return, so the
/// return-inference pass reads the body, takes the rule's `Unknown` as the inferred return type, and
/// the real pass re-checks the SAME `return id` against a `Some(Unknown)` hint — no hint-free read
/// left, no error. Measured: `g := get(); g(1)` printed `1`, `chezzi check` clean. `Unknown`
/// determines nothing and must count as nothing.
#[test]
fn uninstantiated_generic_fn_value_through_an_inferred_return_rejected() {
    rejects(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn get(): ident\n\nfn main():\n    g := get()\n    print(g(1))\n",
        "cannot infer return type of 'get'; add a -> annotation",
    );
    // …and a return-only type param, which no argument can ever bind either. Pre-change this printed
    // `[]` — an accepted `List[T]` value with T undetermined.
    rejects(
        "fn mk[T](n: int) -> List[T]:\n    return []\n\nfn main():\n    g := mk\n    print(g(1))\n",
        "'mk' is generic and T is not determined here",
    );
}

/// Every neighbour the rule's premise implies but does NOT cover — one condition of "nothing
/// determines the type parameters" undone at a time. All of these were measured accepted before the
/// change and must stay accepted.
#[test]
fn a_determined_generic_fn_value_is_untouched_ok() {
    let id = "fn ident[T](x: T) -> T:\n    return x\n";
    // a struct FIELD with a concrete fn type — the ctor argument's slot is the hint
    ok(&format!(
        "{id}\nstruct Bx:\n    f: fn(int) -> int\n\nfn main():\n    b := Bx(ident)\n    print(b.f(3))\n"
    ));
    // a return-only type param, pinned by an annotation and by a turbofish
    ok(
        "fn mk[T](n: int) -> List[T]:\n    return []\n\nfn main():\n    h: fn(int) -> List[int] = mk\n    print(h(1).len())\n",
    );
    ok(
        "fn mk[T](n: int) -> List[T]:\n    return []\n\nfn main():\n    g := mk[int]\n    print(g(1).len())\n",
    );
    // TWO type params, both pinned by the annotation (a turbofish cannot do this — one type arg)
    ok(
        "fn pair[A, B](a: A, b: B) -> A:\n    return a\n\nfn main():\n    h: fn(int, str) -> int = pair\n    print(h(1, \"x\"))\n",
    );
    // a NON-generic fn, and a first-class BUILTIN fn, as values
    ok("fn plain(x: int) -> int:\n    return x\n\nfn main():\n    g := plain\n    print(g(5))\n");
    ok("fn main():\n    p := ord\n    print(p(\"a\"))\n");
    // a PARAM and a LOCAL that merely SHADOW the generic fn's name: the read is theirs, not the fn's
    ok(&format!(
        "{id}\nfn use1(ident: int) -> int:\n    g := ident\n    return g\n\nfn main():\n    print(use1(4))\n"
    ));
    ok(&format!(
        "{id}\nfn use2() -> int:\n    ident := 7\n    g := ident\n    return g\n\nfn main():\n    print(use2())\n"
    ));
}

/// The heads the argument-position cases share: `T` in NO position (`pred`), `T` in the RETURN only
/// (`mk`), `T` pinnable from the argument (`ident`), and a two-argument `pick` whose `T` the
/// ACCUMULATOR pins.
const ARGPOS_HEAD: &str = "fn pred[T](n: int) -> bool:\n    return true\nfn mk[T](n: int) -> List[T]:\n    return []\nfn ident[T](x: T) -> T:\n    return x\nfn pick[T](a: T, b: T) -> T:\n    return b\n";

/// An ARGUMENT-position read of a generic fn is the same read as a binding, so it owes the same
/// answer. Measured on `69aa9efc` (pre-change), all accepted-and-running: `take_bool(pred)` → `1`,
/// `[1,2,3].filter(pred)` → `[1, 2, 3]`, `[1,2,3].map(mk)` → `[[], [], []]` — one function with two
/// verdicts depending on the position it is read in. Go refuses each (`in call to takeBool, cannot
/// infer T`); `[1,2,3].map(mk)`'s `[[], [], []]` is a `List[List[T]]` with `T` never determined.
#[test]
fn undetermined_generic_fn_value_rejected_in_argument_position() {
    // a USER HOF with a concrete declared parameter type (the hint reaches the read, and cannot
    // determine `T`)
    rejects(
        &format!(
            "{ARGPOS_HEAD}fn take_bool(f: fn(int) -> bool) -> int:\n    return 1\nfn main():\n    print(take_bool(pred))\n"
        ),
        "'pred' is generic and T is not determined here",
    );
    // every BUILTIN higher-order method: the NON-generic ones (hint path) …
    for (call, who) in [
        ("[1,2,3].filter(pred)", "pred"),
        ("[1,2,3].take_while(pred)", "pred"),
        ("[1,2,3].drop_while(pred)", "pred"),
        ("[1,2,3].count(pred)", "pred"),
        ("[1,2,3].position(pred)", "pred"),
        ("[1,2,3].sort_by(cmp2)", "cmp2"),
        // … and the GENERIC ones (`map[U]`/`fold[U]`/`sort_by_key[K]`/`min_by[K]`/`max_by[K]`),
        // whose verdict is the DEFERRED end-of-call one.
        ("[1,2,3].map(mk)", "mk"),
        ("[1,2,3].fold(0, acc)", "acc"),
        ("[1,2,3].sort_by_key(key)", "key"),
        ("[1,2,3].min_by(key)", "key"),
        ("[1,2,3].max_by(key)", "key"),
    ] {
        rejects(
            &format!(
                "{ARGPOS_HEAD}fn key[T](n: int) -> int:\n    return n\nfn cmp2[T](a: int, b: int) -> int:\n    return a - b\nfn acc[T](a: int, b: int) -> int:\n    return a + b\nfn main():\n    print({call})\n"
            ),
            &format!("'{who}' is generic and T is not determined here"),
        );
    }
    // …and the `std.concurrency` handles' function slots, including `RwShared[List]`'s `for_each`,
    // whose slot return is deliberately `?` (any return accepted) while its PARAMETER is concrete —
    // an `Unknown` return must not excuse an undetermined type param.
    for call in [
        "s := Shared(1)\n    s.update(bump)",
        "s := RwShared(1)\n    s.write(bump)",
        "s := RwShared([1,2,3])\n    s.for_each(sink)",
        "s := RwShared([1,2,3])\n    print(s.fold(0, acc))",
    ] {
        entry_rejects(
            &format!(
                "import std.concurrency\n{ARGPOS_HEAD}fn bump[T](n: int) -> int:\n    return n + 1\nfn acc[T](a: int, b: int) -> int:\n    return a + b\nfn sink[T](n: int):\n    print(n)\nfn main():\n    {call}\nmain()\n"
            ),
            "is generic and T is not determined here",
        );
    }
    // The diagnostic is the one the BINDING already gives — same sentence, same spellings, and never
    // a 'closure' the user did not write. `mk`'s `T` occurs in its signature, so the concrete-fn-type
    // advice has a `<T>` hole to point at …
    let joined = joined_errs(&format!(
        "{ARGPOS_HEAD}fn main():\n    print([1,2,3].map(mk))\n"
    ));
    assert!(
        joined.contains("'mk' is generic and T is not determined here")
            && joined.contains("instantiate it (`mk[<T>]`)")
            && joined
                .contains("give this position a concrete function type (`fn(int) -> List[<T>]`)")
            && !joined.contains("closure"),
        "argument position must reuse the binding's diagnostic verbatim, got: {joined}"
    );
    // … and `pred`'s `T` occurs NOWHERE, so it must NOT: `fn(int) -> bool` is the type the slot
    // already has, so "give this position a concrete function type (`fn(int) -> bool`)" would be
    // advice the user has already followed, with no `<…>` for the tail to point at. Only the
    // turbofish and deleting the parameter can actually reach it.
    let joined = joined_errs(&format!(
        "{ARGPOS_HEAD}fn main():\n    print([1,2,3].filter(pred))\n"
    ));
    assert!(
        joined.contains("T appears nowhere in its signature (`fn(int) -> bool`)")
            && joined.contains("instantiate it (`pred[<T>]`)")
            && joined.contains("drop the unused type parameter")
            && !joined.contains("give this position a concrete function type"),
        "an unused type parameter must not be told to annotate the position, got: {joined}"
    );
    // …and with TWO unused parameters, the turbofish names both (TICKET-204).
    let joined = joined_errs(
        "fn two[A, B](n: int) -> bool:\n    return true\nfn main():\n    print([1,2,3].filter(two))\n",
    );
    assert!(
        joined.contains("'two' is generic and A, B are not determined here")
            && joined.contains("A, B appear nowhere in its signature (`fn(int) -> bool`)")
            && joined.contains("drop the unused type parameters")
            && joined.contains("`two[<A>, <B>]`"),
        "two unused parameters take the turbofish or deletion, got: {joined}"
    );
}

/// Every check error of `src`, joined — for asserting on a diagnostic's exact wording.
fn joined_errs(src: &str) -> String {
    check_src(src)
        .iter()
        .map(|e| e.message.clone())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// THE ordering constraint. `[1,2,3].fold(0, pick)` pins `pick`'s `T` from the FIRST argument (the
/// accumulator) while `pick` is the SECOND, so the check can only run at the END of the call's
/// inference — a per-argument eager check rejects a program that runs today (`3`) and that Go
/// accepts. Every other way a parameter can get pinned is here too, one per line.
#[test]
fn a_generic_fn_argument_pinned_by_anything_stays_ok() {
    let head = ARGPOS_HEAD;
    // pinned by a SIBLING argument inferred EARLIER (the accumulator) — the deferred-check proof
    ok(&format!(
        "{head}fn main():\n    print([1,2,3].fold(0, pick))\n"
    ));
    // …and BOTH argument orders on a user generic METHOD, whose slots can be spelled either way
    // round (no builtin can): the fn LAST, like `fold`, and the fn FIRST, pinned by an argument the
    // check only sees later. Pre-change the fn-FIRST spelling reported "argument to 'app' has type
    // int, expected T" — pass-1 `unify` had already bound `U` to the rigid callee's leaked `T`.
    let bx = "struct Bx:\n    n: int\n    fn app[U](self, f: fn(U) -> U, x: U) -> U:\n        return f(x)\n    fn app2[U](self, x: U, f: fn(U) -> U) -> U:\n        return f(x)\n";
    ok(&format!(
        "{head}{bx}fn main():\n    b := Bx(0)\n    print(b.app(ident, 5))\n    print(b.app2(5, ident))\n"
    ));
    // …and the same two orders still REJECT the fn nothing can determine.
    rejects(
        &format!("{head}{bx}fn main():\n    b := Bx(0)\n    print(b.app(mk, 5))\n"),
        "'mk' is generic and T is not determined here",
    );
    // pinned by the RECEIVER's element type
    ok(&format!(
        "{head}fn main():\n    print([1,2,3].map(ident))\n"
    ));
    // pinned by a TURBOFISH at the argument
    ok(&format!(
        "{head}fn main():\n    print([1,2].map(mk[str]))\n"
    ));
    ok(&format!(
        "{head}fn main():\n    print([1,2,3].filter(pred[int]))\n"
    ));
    // pinned by the HOF parameter's DECLARED type (including a return-only `T`)
    ok(&format!(
        "{head}fn take_strs(f: fn(int) -> List[str]) -> int:\n    return 0\nfn main():\n    print(take_strs(mk))\n"
    ));
    ok(&format!(
        "{head}fn apply(f: fn(int) -> int, x: int) -> int:\n    return f(x)\nfn main():\n    print(apply(ident, 7))\n"
    ));
    // pinned by the enclosing BINDING's annotation, and by a turbofish at the binding
    ok(&format!(
        "{head}fn main():\n    h: fn(int) -> int = ident\n    print(h(5))\n"
    ));
    ok(&format!(
        "{head}fn main():\n    g := ident[int]\n    print(g(5))\n"
    ));
    // pinned by a RETURN annotation
    ok(&format!(
        "{head}fn get() -> fn(int) -> int:\n    return ident\nfn main():\n    print(get()(9))\n"
    ));
    // pinned by a struct FIELD's concrete fn type
    ok(&format!(
        "{head}struct Bx:\n    f: fn(int) -> int\nfn main():\n    b := Bx(ident)\n    print(b.f(3))\n"
    ));
    // a DIRECT call is not a value read at all
    ok(&format!(
        "{head}fn main():\n    print(ident(5))\n    print(ident(\"a\"))\n"
    ));
    // every builtin HOF again, with a generic fn whose `T` the slot DOES determine
    for call in [
        "[1,2,3].filter(gp)",
        "[1,2,3].take_while(gp)",
        "[1,2,3].drop_while(gp)",
        "[1,2,3].count(gp)",
        "[1,2,3].position(gp)",
        "[1,2,3].map(gi)",
        "[1,2,3].fold(0, pick)",
        "[1,2,3].min_by(gi)",
        "[1,2,3].max_by(gi)",
    ] {
        ok(&format!(
            "{head}fn gp[T](x: T) -> bool:\n    return true\nfn gi[T](x: T) -> T:\n    return x\nfn main():\n    print({call})\n"
        ));
    }
    // the two in-place sorts, whose slot is a `-> nil` statement, not a value
    ok(&format!(
        "{head}fn gc[T](a: T, b: T) -> int:\n    return 0\nfn gi[T](x: T) -> T:\n    return x\nfn main():\n    xs := [3,1,2]\n    xs.sort_by(gc)\n    xs.sort_by_key(gi)\n    print(xs)\n"
    ));
    // `std.concurrency`'s slots, including `for_each`'s deliberately-`?` return
    entry_ok(&format!(
        "import std.concurrency\nimport ConcurrentList from std.concurrency.collection\n{head}fn gi[T](x: T) -> T:\n    return x\nfn gs[T](x: T):\n    print(x)\nfn main():\n    s := Shared(1)\n    s.update(gi)\n    r := RwShared(1)\n    r.write(gi)\n    q := ConcurrentList(RwShared([1,2,3]))\n    q.for_each(gs)\nmain()\n"
    ));
    // an EMPTY receiver leaves the slot's own parameter `Unknown` — the empty-collection sentinel,
    // not evidence the fn is un-instantiable. Both of these print `[]` today and must keep doing so.
    ok(&format!("{head}fn main():\n    print([].map(ident))\n"));
    ok(&format!("{head}fn main():\n    print([].map(mk))\n"));
    // …and the empty receiver must suppress the REPORT ONLY, never the PIN. `map`'s slot has no
    // concrete parameter to expose that — `fold`'s does: `fn(A, ?) -> A` reads non-concrete because
    // the receiver is empty, yet argument ZERO (the accumulator) determines `T` completely, and Go's
    // `Fold([]int{}, 0, add)` returns `0`. Killing the pin here reported the pre-rule wording
    // ("argument to 'fold' has type fn(T, T) -> T") that this whole rule exists to replace.
    ok(&format!(
        "{head}fn add[T: Add](a: T, b: T) -> T:\n    return a + b\nfn main():\n    print([].fold(0, add))\n    print([].fold(\"\", pick))\n"
    ));
    entry_ok(&format!(
        "import std.concurrency\nimport ConcurrentList from std.concurrency.collection\n{head}fn add[T: Add](a: T, b: T) -> T:\n    return a + b\nfn main():\n    s: ConcurrentList[int] = ConcurrentList(RwShared([]))\n    print(s.fold(0, add))\nmain()\n"
    ));
}

/// The rule's own subject, escaping through the carve-out meant for the EMPTY-collection sentinel:
/// `two`'s `[U]` is determined by nothing, `recover_return_only_params` degrades it to `?`, and a
/// carve-out that reads the slot AFTER substitution cannot tell that `?` from an empty receiver's.
/// Measured pre-fix: `check` said `ok: no type errors` and `run` printed `[]` — a `List[U]` with `U`
/// determined by nothing. Go: `in call to two, cannot infer U`.
#[test]
fn a_method_type_param_degraded_to_unknown_still_reports_its_fn_value_args() {
    rejects(
        "struct Bx[T]:\n    v: T\n    fn two[U](self, f: fn(U) -> U, g: fn(U) -> U) -> List[U]:\n        return []\nfn ident[T](x: T) -> T:\n    return x\nfn main():\n    print(Bx(0).two(ident, ident))\n",
        "'ident' is generic and T is not determined here",
    );
    // …and the same method still accepts every spelling that DOES determine `U`.
    ok(
        "struct Bx[T]:\n    v: T\n    fn two[U](self, f: fn(U) -> U, g: fn(U) -> U) -> List[U]:\n        return []\nfn ident[T](x: T) -> T:\n    return x\nfn main():\n    print(Bx(0).two(ident[int], ident[int]))\n",
    );
    ok(
        "struct Bx[T]:\n    v: T\n    fn two[U](self, x: U, f: fn(U) -> U) -> List[U]:\n        return []\nfn ident[T](x: T) -> T:\n    return x\nfn main():\n    print(Bx(0).two(5, ident))\n",
    );
}

/// The heads for the GENERIC-HOF cases: a generic fn read as a value (`ident`), one whose `T` is
/// return-only (`mk`), and four generic HOFs whose OWN type parameter must be pinned by the other
/// arguments before the passed fn's parameters can unify against the slot.
const GHOF_HEAD: &str = "fn ident[T](x: T) -> T:\n    return x\nfn mk[T](n: int) -> List[T]:\n    return []\nfn applyg[U](f: fn(U) -> U, n: U) -> U:\n    return f(n)\nfn applyr[U](n: U, f: fn(U) -> U) -> U:\n    return f(n)\nfn nopin[U](f: fn(U) -> U) -> int:\n    return 1\nfn twop[A, B](f: fn(A) -> B, a: A) -> B:\n    return f(a)\n";

/// A bare generic fn passed to a **generic** HOF's fn-typed slot: the callee's own `[U]` is pinned by
/// the OTHER arguments first, and only then can the passed fn's parameters unify against the
/// now-concrete slot. The three shapes below were REFUSED before this (`'ident' is generic and T is
/// not determined here`) while both ancestors infer them — measured this session, Go 1.26.5 and Rust
/// 1.97 each print `5` for `applyg(id, 5)` / `applyr(5, id)` / `twop(id, 5)`. The deferral that makes
/// it work is the SAME one `infer_generic_method` uses for `[1,2,3].fold(0, pick)`, now on the
/// free-fn path too; the running half (asserting the VALUE) is
/// `tests/chz/spec/list_test.chz`.
#[test]
fn a_generic_fn_argument_to_a_generic_hof_is_pinned_by_the_other_arguments() {
    // pinned by a LATER argument (the fn is argument 0) …
    ok(&format!(
        "{GHOF_HEAD}fn main():\n    print(applyg(ident, 5))\n"
    ));
    // … by an EARLIER one (the fn is argument 1) …
    ok(&format!(
        "{GHOF_HEAD}fn main():\n    print(applyr(5, ident))\n"
    ));
    // … and across TWO callee parameters: `A` from the value argument, `B` only through the fn.
    ok(&format!(
        "{GHOF_HEAD}fn main():\n    print(twop(ident, 5))\n"
    ));
    // A turbofish ON THE CALL pins the callee's `[U]` with no value argument in sight.
    ok(&format!(
        "{GHOF_HEAD}fn main():\n    print(applyg[int](ident, 5))\n"
    ));
    // A turbofish on the ARGUMENT is not a bare read at all — unchanged, and still fine.
    ok(&format!(
        "{GHOF_HEAD}fn main():\n    print(applyg(ident[int], 5))\n"
    ));
    // The fn's OWN return can pin a callee parameter no argument reaches (`B` ← `str`).
    ok(&format!(
        "{GHOF_HEAD}fn to_text[T](x: T) -> str:\n    return \"{{x}}\"\nfn main():\n    print(twop(to_text, 5))\n"
    ));
    // Two generic HOFs nested, and one fn value used at TWO different types in one call — each pin
    // runs against a FRESH substitution map, so neither can launder into the other.
    ok(&format!(
        "{GHOF_HEAD}fn main():\n    print(applyg(ident, applyg(ident, 5)))\n"
    ));
    ok(&format!(
        "{GHOF_HEAD}fn twice[A, B](f: fn(A) -> A, g: fn(B) -> B, a: A, b: B) -> str:\n    return \"{{f(a)}}{{g(b)}}\"\nfn main():\n    print(twice(ident, ident, 1, \"x\"))\n"
    ));
    // A passed fn with TWO type parameters, BOTH reachable from the slot.
    ok(&format!(
        "{GHOF_HEAD}fn pair2[T, S](a: T, b: S) -> T:\n    return a\nfn app2[U](f: fn(U, U) -> U, a: U) -> U:\n    return f(a, a)\nfn main():\n    print(app2(pair2, 5))\n"
    ));
    // The enclosing annotation pins the callee's return-only `[U]`, which then pins the fn — the same
    // `seed_from_hint` precedence (turbofish > args > annotation) every other generic call uses, which
    // is why the deferred pin runs AFTER it. Rust infers this too; Go has no assignment-context
    // inference at all and refuses it, as it refuses `xs: List[int] = empty()`.
    ok(&format!(
        "{GHOF_HEAD}fn mklist[U](f: fn(U) -> U) -> List[U]:\n    return []\nfn main():\n    xs: List[int] = mklist(ident)\n    print(xs)\n"
    ));
}

/// …and the half that must KEEP rejecting, because the information genuinely is not there. Go 1.26.5,
/// measured: `cannot infer U` / `type func[T any](n int) []T of mk does not match inferred type
/// func(int) int` / `cannot infer B`. Rust 1.97 agrees on all three (`E0282`, `E0308`, `E0282`).
#[test]
fn a_generic_hof_that_pins_nothing_still_refuses_the_generic_fn_value() {
    // NOTHING pins the callee's `[U]`: no value argument, no turbofish, no annotation.
    rejects(
        &format!("{GHOF_HEAD}fn main():\n    print(nopin(ident))\n"),
        "'ident' is generic and T is not determined here",
    );
    // The slot IS concrete (`fn(int) -> int`) and the shapes still cannot match: `mk`'s `T` is
    // return-only and the slot's return is `int`.
    rejects(
        &format!("{GHOF_HEAD}fn main():\n    print(applyg(mk, 5))\n"),
        "'mk' is generic and T is not determined here",
    );
    // `B` is reachable ONLY through `mk`'s own undetermined `T`, so pinning `A` does not help.
    rejects(
        &format!("{GHOF_HEAD}fn main():\n    print(twop(mk, 5))\n"),
        "'mk' is generic and T is not determined here",
    );
    // A type parameter that appears NOWHERE in the passed fn's signature is unreachable from any
    // slot (Go: `in call to applyg, cannot infer S`).
    rejects(
        &format!(
            "{GHOF_HEAD}fn konst[T, S](x: T) -> T:\n    return x\nfn main():\n    print(applyg(konst, 5))\n"
        ),
        "'konst' is generic and T, S are not determined here",
    );
    // A generic callee whose slot is a BARE type parameter — nothing about that position is a
    // function type, so the read can never be pinned there (this is the pre-existing `take(ident)`
    // case, which the free-fn deferral must not swallow).
    rejects(
        &format!(
            "{GHOF_HEAD}fn take[U](f: U) -> int:\n    return 1\nfn main():\n    print(take(ident))\n"
        ),
        "'ident' is generic and T is not determined here",
    );
    // …and one where the pinned slot is concrete but WRONG (`fn(int) -> str` into `fn(int) -> int`):
    // the pin succeeds and the ordinary assignability diagnostic owns the verdict, as it does for a
    // non-generic HOF. Go rejects it too (`cannot use 5 … as string value`).
    rejects(
        &format!(
            "{GHOF_HEAD}fn to_text[T](x: T) -> str:\n    return \"{{x}}\"\nfn main():\n    print(applyg(to_text, 5))\n"
        ),
        "expected fn(int) -> int",
    );
}

/// The "this read is re-pinned afterwards" licence covers the IMMEDIATE bare-identifier arguments and
/// nothing deeper. Silencing the wall for a whole generic-argument prepass leaked it into every NESTED
/// read: `take2(Bx(ident), 5)` check-cleanly built a `Bx[fn(T) -> T]` — a stored value whose type
/// nothing determines — the moment the free-fn path started silencing (measured), and the METHOD
/// spelling had been accepting it since the licence was introduced (measured on `3f8b6d06`).
#[test]
fn the_repin_licence_does_not_leak_into_a_nested_argument() {
    let head = "fn ident[T](x: T) -> T:\n    return x\nstruct Bx[T]:\n    f: T\n";
    rejects(
        &format!(
            "{head}fn take2[U](a: U, b: int) -> int:\n    return 1\nfn main():\n    print(take2(Bx(ident), 5))\n"
        ),
        "'ident' is generic and T is not determined here",
    );
    rejects(
        &format!(
            "{head}struct Holder:\n    n: int\n    fn m[U](self, a: U, b: int) -> int:\n        return 1\nfn main():\n    print(Holder(0).m(Bx(ident), 5))\n"
        ),
        "'ident' is generic and T is not determined here",
    );
    // …and the licence still works for the argument it IS for, in both spellings.
    ok(&format!(
        "{head}fn applyg[U](f: fn(U) -> U, n: U) -> U:\n    return f(n)\nfn main():\n    print(applyg(ident, 5))\n"
    ));
    ok(&format!(
        "{head}fn main():\n    print([1, 2, 3].map(ident))\n"
    ));
}

#[test]
fn generic_fn_direct_call_unchanged() {
    ok("fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    print(ident(5) + 1)\n");
}

#[test]
fn generic_fn_call_site_turbofish_unchanged() {
    ok("fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    print(ident[int](5) + 1)\n");
}

#[test]
fn nongeneric_fn_indexed_rejected() {
    // A NON-generic fn indexed is NOT a turbofish — checker must reject (the compiler-erase relies on
    // this: only a checker-accepted generic-fn turbofish ever reaches the erase path).
    rejects(
        "fn g(x: int) -> int:\n    return x\n\nfn main():\n    y := g[0]\n",
        "cannot index into fn(int) -> int",
    );
}

#[test]
fn local_shadowing_generic_fn_name_indexes_normally() {
    // A local/param that shadows a top-level generic fn name is a REAL index, not an erased turbofish.
    ok(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn h():\n    ident := [10, 20, 30]\n    print(ident[1])\n",
    );
    ok("fn ident[T](x: T) -> T:\n    return x\n\nfn h(ident: List[int]):\n    print(ident[0])\n");
}

// ---- builtin-HOF slot pins a bare same-module generic fn's own [T] (scope A through .map/.fold) ----

#[test]
fn generic_fn_value_into_builtin_map_return_concrete_ok() {
    // `.map`'s slot fn(int) -> U pins conv's own T=int; the map result is List[str].
    ok(
        "fn conv[T](x: T) -> str:\n    return str(x)\n\nfn main():\n    xs := [1, 2, 3].map(conv)\n    s: str = xs[0]\n    print(s)\n",
    );
}

#[test]
fn generic_fn_value_into_builtin_map_return_only_ok() {
    // ident's return-only T is pinned = int from the .map element type; result is List[int].
    ok(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn main():\n    xs := [1, 2, 3].map(ident)\n    n: int = xs[0]\n    print(n + 1)\n",
    );
}

#[test]
fn generic_fn_value_into_builtin_fold_ok() {
    // .fold(0, add): the accumulator U is pinned by `init`=int FIRST, then add's own T=int is pinned
    // from the resulting fn(int, int) -> int slot; the bound [T: Add] is enforced under the pin.
    ok(
        "fn add[T: Add](a: T, b: T) -> T:\n    return a + b\n\nfn main():\n    n: int = [1, 2, 3].fold(0, add)\n    print(n)\n",
    );
}

#[test]
fn generic_fn_value_into_builtin_filter_still_ok() {
    // .filter's slot is fully concrete fn(int) -> bool; a bare generic keep[T] still pins T=int.
    ok(
        "fn keep[T](x: T) -> bool:\n    return true\n\nfn main():\n    xs := [1, 2, 3].filter(keep)\n    print(xs)\n",
    );
}

#[test]
fn generic_fn_value_two_distinct_pins_no_launder_ok() {
    // Same conv pinned once into .map (str-returning slot) and once into a user HOF with a
    // fn(int) -> str param; each call site gets a FRESH substitution map so neither launders.
    ok(concat!(
        "fn conv[T](x: T) -> str:\n    return str(x)\n",
        "\nfn use2(f: fn(int) -> str) -> str:\n    return f(9)\n",
        "\nfn main():\n    a := [1, 2, 3].map(conv)\n    sa: str = a[0]\n    b := use2(conv)\n    print(sa)\n    print(b)\n",
    ));
}

#[test]
fn generic_fn_value_into_map_bound_violation_rejected() {
    // A [T: Comparable]-bounded fn pinned to a non-Comparable element type must reject via
    // enforce_bounds under the pin (the helper mirrors Scope A's bound enforcement).
    rejects(
        "struct Tag:\n    n: int\n\nfn cmp[T: Comparable](x: T) -> T:\n    return x\n\nfn main():\n    xs := [Tag(1), Tag(2)].map(cmp)\n    print(xs)\n",
        "does not satisfy Comparable (missing method 'compare')",
    );
}

#[test]
fn generic_fn_value_map_closure_still_infers_ok() {
    // Regression: the unannotated-closure loop-back is untouched — still infers List[int].
    ok("fn main():\n    xs := [1, 2, 3].map(fn(x): x * 2)\n    n: int = xs[0]\n    print(n)\n");
}

// ---- arity guard: a generic method called with TOO FEW args reports cleanly, never panics ----
// (adversarial-review bugs 1 & 2: the pass-1 pin loop must clamp to `arg_tys.len()`, not index
// `expected.len()` out of bounds when the arity check reports-but-does-not-return.)

#[test]
fn generic_method_too_few_args_reports_not_panics_fold() {
    // `.fold` expects (init, f) — passing only `init` must report the arity error, not index-panic.
    rejects(
        "fn main():\n    print([1, 2, 3].fold(0))\n",
        "'fold' expects 2 argument(s), got 1",
    );
}

#[test]
fn generic_method_too_few_args_reports_not_panics_map() {
    // `.map` expects (f) — passing none must report the arity error, not index-panic.
    rejects(
        "fn main():\n    print([1, 2, 3].map())\n",
        "'map' expects 1 argument(s), got 0",
    );
}

#[test]
fn generic_user_method_too_few_args_reports_not_panics() {
    // A user generic method routed through infer_generic_method with too few args reports cleanly.
    rejects(
        "struct B[T]:\n    v: T\n    fn map_to[U](self, f: fn(T) -> U) -> U:\n        return f(self.v)\nfn main():\n    b := B(1)\n    print(b.map_to())\n",
        "'map_to' expects 1 argument(s), got 0",
    );
}

// ── Parameterized protocols in value/annotation position (Q1) ───────────────────────────────

/// A `Container[int]` param annotation is ACCEPTED (was a hard "can only be used as a bound" error);
/// a struct whose `get` returns `int` conforms and the call type-checks.
#[test]
fn param_parameterized_protocol_accepts() {
    ok(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    fn get(self, i: int) -> int:\n        return 7\nfn f(c: Container[int]) -> int:\n    return c.get(0)\nfn main():\n    print(f(Bag()))\n",
    );
}

/// A `Container[str]` slot must REJECT an int-returning Bag — the carried arg is witnessed at the
/// call boundary (no over-accept), so the value fails the parameter's assignability check.
#[test]
fn param_protocol_wrong_arg_rejected() {
    rejects(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    fn get(self, i: int) -> int:\n        return 7\nfn f(c: Container[str]) -> int:\n    return 0\nfn main():\n    f(Bag())\n",
        "expected Container[str], found Bag",
    );
}

/// A bare NON-GENERIC protocol existential still ACCEPTS a conforming struct (0-arg existential
/// preserved — the `Error`/`Show` case, unchanged by the parameterization work).
#[test]
fn bare_protocol_still_accepts() {
    ok(
        "protocol Show:\n    fn show(self) -> str\nstruct Bag:\n    fn show(self) -> str:\n        return \"bag\"\nfn f(c: Show) -> str:\n    return c.show()\nfn main():\n    print(f(Bag()))\n",
    );
}

/// A bare `Container` value is NOT assignable into a `Container[int]` slot (strict invariance / arity).
#[test]
fn bare_not_equal_parameterized_reject_into_param() {
    rejects(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    fn get(self, i: int) -> int:\n        return 7\nfn g(c: Container) -> Container[int]:\n    return c\nfn main():\n    pass\n",
        "expected",
    );
}

/// A `Container[int]` value is NOT assignable into a bare `Container` slot (strict invariance / arity).
#[test]
fn parameterized_not_equal_bare_reject_into_bare() {
    rejects(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    fn get(self, i: int) -> int:\n        return 7\nfn g(c: Container[int]) -> Container:\n    return c\nfn main():\n    pass\n",
        "expected",
    );
}

/// Method-return element RECOVERY: `c: Container[int]; c.get(0)` infers to `int`, usable in int arithmetic.
#[test]
fn param_protocol_method_return_recovered() {
    ok(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    fn get(self, i: int) -> int:\n        return 7\nfn f(c: Container[int]) -> int:\n    x := c.get(0)\n    return x + 1\nfn main():\n    print(f(Bag()))\n",
    );
}

/// Write-site (a): a `-> Container[int]` return with a str-returning value REJECTS.
#[test]
fn param_protocol_return_site_rejects() {
    rejects(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct StrBag:\n    fn get(self, i: int) -> str:\n        return \"x\"\nfn g() -> Container[int]:\n    return StrBag()\nfn main():\n    pass\n",
        "expected return type Container[int], found StrBag",
    );
}

/// Write-site (b): a `struct S: c: Container[int]` field initialised with a str-returning value REJECTS.
#[test]
fn param_protocol_field_site_rejects() {
    rejects(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct StrBag:\n    fn get(self, i: int) -> str:\n        return \"x\"\nstruct S:\n    c: Container[int]\nfn main():\n    S(StrBag())\n",
        "expected Container[int], found StrBag",
    );
}

/// Write-site (c): reassigning a str-returning value into a `Container[int]` local REJECTS.
#[test]
fn param_protocol_reassign_site_rejects() {
    rejects(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct IntBag:\n    fn get(self, i: int) -> int:\n        return 1\nstruct StrBag:\n    fn get(self, i: int) -> str:\n        return \"x\"\nfn f(c: Container[int]):\n    c = StrBag()\nfn main():\n    f(IntBag())\n",
        "cannot assign StrBag to Container[int]",
    );
}

/// Nesting: `List[Container[int]]` threads the args into the nested Protocol; a wrong-arg nested variant REJECTS.
#[test]
fn param_protocol_nesting_accepts_and_wrong_rejects() {
    ok(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    fn get(self, i: int) -> int:\n        return 7\nfn h(xs: List[Container[int]]) -> int:\n    return xs[0].get(0)\nfn main():\n    print(h([Bag()]))\n",
    );
    rejects(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    fn get(self, i: int) -> int:\n        return 7\nfn h(xs: List[Container[str]]) -> int:\n    return 0\nfn main():\n    h([Bag()])\n",
        "list element: expected Container[str], found Bag",
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// MUTABLE-CONTAINER GENERIC INVARIANCE (soundness hole): a `G[Sub]` value must
// NOT be assignable where `G[Super]` is expected for a MUTABLE, by-reference
// container (List/Set/Map, user generic struct). The reverse was already
// rejected; these lock the covariant direction shut. All use the REAL graph
// helpers (entry_ok/entry_rejects), NOT single-module check_src.
// ─────────────────────────────────────────────────────────────────────────────

/// W7-8 — `str`/`bytes`/`bytearray` satisfy `PathLike` INTRINSICALLY (they have no `as_path` method
/// of their own), through a protocol-typed value slot and an erased generic bound. `path.Path`
/// satisfies it structurally. NOT through a container element slot — see
/// [`pathlike_grant_does_not_widen_container_invariance`], which is why `path.join` is generic over
/// its element type instead of taking a `List[PathLike]`.
#[test]
fn pathlike_is_satisfied_by_the_three_byte_spellings() {
    ok("fn f(p: PathLike) -> bytes:\n    return p.as_path()\nprint(f(\"a\").len())\n");
    ok("fn f(p: PathLike) -> bytes:\n    return p.as_path()\nprint(f(b\"a\").len())\n");
    ok("fn f(p: PathLike) -> bytes:\n    return p.as_path()\nprint(f(bytearray(b\"a\")).len())\n");
    ok("fn f[T: PathLike](p: T) -> bytes:\n    return p.as_path()\nprint(f(\"a\").len())\n");
    // A struct with its OWN `as_path(self) -> bytes` conforms structurally (this is `path.Path`).
    ok(
        "struct P:\n    raw: bytes\n    fn as_path(self) -> bytes:\n        return self.raw\nfn f(p: PathLike) -> bytes:\n    return p.as_path()\nprint(f(P(b\"a\")).len())\n",
    );
    // ...and a type WITHOUT one still does not.
    rejects(
        "struct Q:\n    n: int\nfn f(p: PathLike) -> bytes:\n    return p.as_path()\nprint(f(Q(1)).len())\n",
        "PathLike",
    );
}

/// W7-8 REGRESSION FENCE — the `PathLike` grant is a VALUE-level early-out keyed on the concrete
/// scalar `Ty`, so it must be UNREACHABLE from any container element comparison. These four were
/// rejected before the grant landed and must stay rejected: a leak here would be a silent widening of
/// container invariance, which this task changes NOTHING about.
#[test]
fn pathlike_grant_does_not_widen_container_invariance() {
    entry_rejects(
        "fn poison(xs: List[Any]):\n    xs.push(\"s\")\nfn main():\n    ns: List[int] = [1]\n    poison(ns)\nmain()\n",
        "expected List[Any], found List[int]",
    );
    entry_rejects(
        "protocol Shape:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int:\n        return self.s * self.s\nfn f(xs: List[Shape]):\n    print(xs.len())\nfn main():\n    sqs: List[Sq] = [Sq(2)]\n    f(sqs)\nmain()\n",
        "expected List[Shape], found List[Sq]",
    );
    entry_rejects(
        "fn stash(m: Map[str, Any]):\n    m[\"x\"] = 1\nfn main():\n    d: Map[str, int] = {\"a\": 1}\n    stash(d)\nmain()\n",
        "expected Map[str, Any], found Map[str, int]",
    );
    entry_rejects(
        "fn f(xs: Iterable[Any]):\n    print(xs)\nfn main():\n    ns: List[int] = [1]\n    f(ns)\nmain()\n",
        "Iterable[Any]",
    );
    // The grant does NOT reach a container element either: `List[str]` is not `List[PathLike]`. This
    // is precisely why `path.join` is `[T](List[T]) where T: PathLike` — a `List[PathLike]` parameter
    // would be callable with a list LITERAL and with no variable at all (W7-8 review).
    entry_rejects(
        "fn f(xs: List[PathLike]):\n    print(xs.len())\nfn main():\n    ss: List[str] = [\"a\"]\n    f(ss)\nmain()\n",
        "expected List[PathLike], found List[str]",
    );
    // ...and the generic-bound spelling that replaced it DOES accept that same variable.
    entry_ok(
        "fn f[T](xs: List[T]) -> int where T: PathLike:\n    return xs[0].as_path().len()\nfn main():\n    ss: List[str] = [\"ab\"]\n    print(f(ss))\nmain()\n",
    );
}

/// Repro A (was check-ok → runtime trap): passing a `List[Cat]` VARIABLE where
/// `List[Any]` is expected must REJECT (the callee can `.push` a non-Cat).
#[test]
fn invariance_rejects_list_covariance_launder() {
    entry_rejects(
        "struct Cat:\n    name: str\nfn poison(xs: List[Any]):\n    xs.push(123)\nfn main():\n    cats: List[Cat] = [Cat(\"felix\")]\n    poison(cats)\n    for c in cats:\n        print(c.name)\nmain()\n",
        "expected List[Any], found List[Cat]",
    );
}

/// Map VALUE type is invariant: a `Map[str, Cat]` VARIABLE must NOT flow into a
/// `Map[str, Any]` slot (the callee can insert a non-Cat value).
#[test]
fn invariance_rejects_map_value_covariance() {
    entry_rejects(
        "struct Cat:\n    name: str\nfn stash(m: Map[str, Any]):\n    m[\"x\"] = 123\nfn main():\n    cats: Map[str, Cat] = {\"felix\": Cat(\"felix\")}\n    stash(cats)\nmain()\n",
        "expected Map[str, Any], found Map[str, Cat]",
    );
}

/// User generic struct field via a mutator `set(self, x: T)` — `Box[Cat]` var
/// must NOT pass where `Box[Any]` is expected. (Any top-type variant.)
#[test]
fn invariance_rejects_user_generic_struct_covariance_any() {
    entry_rejects(
        "struct Cat:\n    name: str\nstruct Box[T]:\n    v: T\n    fn set(self, x: T):\n        self.v = x\nfn f(b: Box[Any]):\n    b.set(123)\nfn main():\n    bc: Box[Cat] = Box(Cat(\"felix\"))\n    f(bc)\nmain()\n",
        "expected Box[Any], found Box[Cat]",
    );
}

/// Same, but the supertype is a USER protocol (Cat <: Speaker), not `Any`.
#[test]
fn invariance_rejects_user_generic_struct_covariance_protocol() {
    entry_rejects(
        "protocol Speaker:\n    fn speak(self) -> str\nstruct Cat:\n    name: str\n    fn speak(self) -> str:\n        return \"meow\"\nstruct Dog:\n    fn speak(self) -> str:\n        return \"woof\"\nstruct Box[T]:\n    v: T\n    fn set(self, x: T):\n        self.v = x\nfn f(b: Box[Speaker]):\n    b.set(Dog())\nfn main():\n    bc: Box[Cat] = Box(Cat(\"felix\"))\n    f(bc)\nmain()\n",
        "expected Box[Speaker], found Box[Cat]",
    );
}

/// Assignment boundary (not a fn-arg): `b: List[Any] = a` where `a: List[Cat]`
/// must REJECT — the fix covers plain let/`:=` sinks, not just call args.
#[test]
fn invariance_rejects_let_assign_container_covariance() {
    entry_rejects(
        "struct Cat:\n    name: str\nfn main():\n    a: List[Cat] = [Cat(\"felix\")]\n    b: List[Any] = a\n    print(b)\nmain()\n",
        "cannot assign List[Cat] to variable of type List[Any]",
    );
}

/// BOUNDARY GUARD — legitimate container neighbors that MUST stay check-clean
/// (no over-rejection). Same-arg containers, nested equal generics, and a
/// literal `List[Any]` built and used as `List[Any]`.
#[test]
fn invariance_preserves_legit_container_neighbors() {
    // List[int] -> List[int]
    entry_ok(
        "fn f(xs: List[int]) -> int:\n    return xs[0]\nfn main():\n    ys: List[int] = [1, 2]\n    print(f(ys))\nmain()\n",
    );
    // List[Cat] -> List[Cat]
    entry_ok(
        "struct Cat:\n    name: str\nfn f(xs: List[Cat]) -> str:\n    return xs[0].name\nfn main():\n    cs: List[Cat] = [Cat(\"felix\")]\n    print(f(cs))\nmain()\n",
    );
    // Map[str, int] -> Map[str, int]
    entry_ok(
        "fn f(m: Map[str, int]) -> int:\n    return m[\"a\"]\nfn main():\n    d: Map[str, int] = {\"a\": 1}\n    print(f(d))\nmain()\n",
    );
    // Box[Cat] -> Box[Cat]
    entry_ok(
        "struct Cat:\n    name: str\nstruct Box[T]:\n    v: T\nfn f(b: Box[Cat]) -> str:\n    return b.v.name\nfn main():\n    bc: Box[Cat] = Box(Cat(\"felix\"))\n    print(f(bc))\nmain()\n",
    );
    // nested Map[str, List[int]] -> same
    entry_ok(
        "fn f(m: Map[str, List[int]]) -> int:\n    return m[\"a\"][0]\nfn main():\n    d: Map[str, List[int]] = {\"a\": [1, 2]}\n    print(f(d))\nmain()\n",
    );
    // a literal List[Any] built and used as List[Any]
    entry_ok(
        "fn f(xs: List[Any]):\n    xs.push(1)\nfn main():\n    ys: List[Any] = [1, \"a\", true]\n    f(ys)\nmain()\n",
    );
}
