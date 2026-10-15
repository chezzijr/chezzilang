//! Checker tests: the `Eq` protocol, aliases, generic structs/enums, method type params,
//! parameterized protocols, `where`.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ----- M23 — the `Eq` protocol (checker foundation; the `==` dispatch itself lands later) -----

/// The intrinsic scalar grant: ALL FOUR scalars satisfy `Eq` with no user method — `==` is defined
/// on every scalar (unlike `Comparable`, which excludes `bool`). What a NON-scalar does is D1's
/// question, pinned by `eq_is_satisfied_by_every_type_whose_equality_is_structural` below.
#[test]
fn scalars_satisfy_eq_intrinsically() {
    for lit in ["1", "1.5", "\"a\"", "true"] {
        ok(&format!(
            "fn same[T: Eq](a: T, b: T) -> bool:\n    return a.eq(b)\nprint(same({lit}, {lit}))\n"
        ));
    }
}

/// **D1 — `Eq` satisfaction IS what `==` accepts.** Chezzi's structural `==` is an automatic derive
/// that never told the protocol system, so every one of these had a WORKING `==` and a FAILING
/// `[T: Eq]` bound. **Ancestor, measured, `rustc 1.97.0`:** `Vec<i32>`, `(i32, i32)`, `Option<i32>`,
/// `Vec<u8>` and a `#[derive(PartialEq, Eq)] struct P` all satisfy `Eq` and all compile under
/// `struct Boxy<T: Eq>`; only a type with no `PartialEq` at all (`Tag`) gives `E0277`.
///
/// The bound is spelled over `a == b` (not `a.eq(b)`) on purpose — the rule is defined by the
/// operator, and this is the shape a user writes on a conditional `fn eq(…) where T: Eq`.
#[test]
fn eq_is_satisfied_by_every_type_whose_equality_is_structural() {
    let same = "fn same[T: Eq](a: T, b: T) -> bool:\n    return a == b\n";
    for (prelude, lit) in [
        ("", "[1]"),
        ("", "(1, 2)"),
        ("fn ho() -> int?:\n    return 1\n", "ho()"),
        ("fn hr() -> int!str:\n    return 1\n", "hr()"),
        ("", "b\"ab\""),
        ("", "bytearray(b\"ab\")"),
        ("", "{\"k\": 1}"),
        ("", "Set([1])"),
        ("", "1"),
        ("struct P:\n    x: int\n", "P(1)"),
        ("enum Color:\n    Red\n    Blue\n", "Color.Red"),
        // GENERIC nominals — the shape the feature exists for, and the one the first cut of the
        // walk got wrong in BOTH directions (it walked DECLARED field types, so the decl-site `T`
        // reached the `Ty::Param` arm unbound and refused every one of these).
        ("struct Wrap[T]:\n    v: T\n", "Wrap(1)"),
        ("struct Pair[A, B]:\n    a: A\n    b: B\n", "Pair(1, \"s\")"),
        (
            "enum Opt[T]:\n    Nothing\n    Just(T)\n",
            "Opt[int].Just(1)",
        ),
        // …including one whose payload nests a param inside a container.
        (
            "struct Bag[T]:\n    items: List[T]\n    seen: Map[str, T]\n",
            "Bag([1], {\"k\": 1})",
        ),
    ] {
        entry_ok(&format!("{prelude}{same}print(same({lit}, {lit}))\n"));
    }
    // A generic from the real stdlib, reached through a normal import.
    entry_ok(
        "import std.collections\nfn same[T: Eq](a: T, b: T) -> bool:\n    return a == b\nd := collections.Deque[int]([], [])\nprint(same(d, d))\n",
    );
}

/// **Conformance may not depend on the CALLER's lexical scope.** The walk resolved declaration-site
/// params against whatever was in scope where the question was asked, so `Wrap[int]` satisfied `Eq`
/// inside `fn outer[T: Eq]` and not at module scope — the same type, two answers, and one of them a
/// grant. Both call sites must agree.
#[test]
fn eq_conformance_does_not_depend_on_the_callers_scope() {
    entry_ok(
        "\
struct Wrap[T]:
    v: T
fn same[U: Eq](a: U, b: U) -> bool:
    return a == b
fn outer[T: Eq](z: T) -> bool:
    return same(Wrap(1), Wrap(2))
print(outer(1))
print(same(Wrap(1), Wrap(2)))
",
    );
}

/// **The BUILT-IN CURSOR is excluded, and the ratchet cannot see it.** `Ty::Struct("Iterator", [E])`
/// — what `.iter()` mints and a generator returns — has kind `"struct"`, so D1 granted it `Eq` and
/// `intrinsic_grants_all_have_vm_arms` stayed green: its `"struct"` probe receiver is a real user
/// struct, whose dispatch arm HAS the intrinsic fallback. The cursor's and the generator's arms
/// deliberately do not (`only next()/iter()`), so the grant was check-OK-then-run-fault — found by
/// running the pre-change binary, not by the suite. `c == d` still WORKS (identity), so this is a
/// deliberate narrowing of "Eq is what `==` accepts"; **the ancestors agree** — Rust's
/// `slice::Iter` has no `PartialEq`, and CPython's iterators compare by identity with no protocol.
#[test]
fn the_builtin_cursor_satisfies_eq_by_identity() {
    // TICKET-257 (owner rule U5) reverses the earlier narrowing: `c == d` is accepted on a cursor, so
    // the `Eq` bound accepts it too. The `Iterator` rows of `INTRINSIC_PROTO_METHODS` promise the
    // runtime arm (`x.eq(y)` is `x == y`), which `intrinsic_grants_all_have_vm_arms` runs.
    for recv in ["[1, 2].iter()", "gen()"] {
        entry_ok(&format!(
            "fn gen() -> Iterator[int]:\n    yield 1\nfn eqm[T: Eq](a: T, b: T) -> bool:\n    return a.eq(b)\nprint(eqm({recv}, {recv}))\n"
        ));
    }
    // A USER struct with `next` is a genuine `Obj::Struct` at runtime and keeps the grant.
    entry_ok(
        "struct Cur:\n    fn next(self) -> int?:\n        return None\nfn eqm[T: Eq](a: T, b: T) -> bool:\n    return a.eq(b)\nprint(eqm(Cur(), Cur()))\n",
    );
}

/// **The `Ty::Param` exclusion.** `may_be_equal(Param(_), _)` is `true` by ERASURE (a generic body is
/// checked once with `T` abstract), so reading the D1 rule naively would make every UNBOUNDED `T`
/// satisfy `Eq` — the hole that lets an un-equatable type in through a forwarding call. A `Ty::Param`
/// must keep falling through to its declared-bounds arm.
#[test]
fn unbounded_type_param_does_not_satisfy_eq() {
    rejects(
        "fn g[U: Eq](a: U, b: U) -> bool:\n    return a.eq(b)\nfn f[T](a: List[T], b: List[T]) -> bool:\n    return g(a, b)\n",
        "does not satisfy Eq",
    );
    // …and a param that DOES declare the bound still forwards (the arm is not simply disabled).
    ok(
        "fn g[U: Eq](a: U, b: U) -> bool:\n    return a.eq(b)\nfn f[T: Eq](a: T, b: T) -> bool:\n    return g(a, b)\n",
    );
}

/// **W7-54 — a function value satisfies `Eq`.** `Ty::Func` had no `intrinsic_recv_kind` arm, so it
/// fell to `"?"` and the D1 grant (above) excluded it: `f == g` (structural derive, `Vm::values_equal`)
/// already answered `true`/`false` by IDENTITY, matching CPython's `f == g` exactly, but neither
/// `.eq()` nor a `[T: Eq]` bound could say the same thing — a working operator and a failing method
/// spelling for ONE relation. **rustc 1.97.0, measured:** `#[derive(PartialEq, Eq)] struct
/// Boxy<T: Eq>(T)` over a `fn(i32) -> i32` payload builds clean and prints `true` — fn pointers
/// implement `PartialEq`, so granting `Eq` to `Ty::Func` is the Rust-agreeing answer (Go, the one
/// ancestor that differs, rejects `f == g` outright: `func can only be compared to nil`).
#[test]
fn function_value_satisfies_eq_by_identity() {
    const G: &str = "fn g(x: int) -> int:\n    return x\n";
    // the method spelling now agrees with the operator
    ok(&format!(
        "{G}fn same[T: Eq](a: T, b: T) -> bool:\n    return a.eq(b)\nprint(same(g, g))\n"
    ));
    // …and the bound is satisfiable spelled over the operator too (D1's own shape)
    ok(&format!(
        "{G}fn same[T: Eq](a: T, b: T) -> bool:\n    return a == b\nprint(same(g, g))\n"
    ));
}

/// **W7-54 follow-up — `Ty::BuiltinFn` is the same defect as `Ty::Func`, same fix.** A first-class
/// universe builtin (`ord`/`chr`/`panic`/`print`) types as the DISTINCT `Ty::BuiltinFn` variant (kept
/// apart from `Ty::Func` only so it can be sendable across `spawn`), so `Ty::Func { .. } => "func"`
/// alone did not cover it — it still fell to `"?"` and the D1 grant still excluded it, even though
/// `Ty::BuiltinFn` renders identically ("fn(str) -> int") to `Ty::Func` in every diagnostic. Fixed by
/// classifying both variants to the same `"func"` kind rather than minting a second one.
#[test]
fn builtin_function_value_satisfies_eq_by_identity() {
    ok("fn same[T: Eq](a: T, b: T) -> bool:\n    return a.eq(b)\nprint(same(ord, ord))\n");
    ok("fn same[T: Eq](a: T, b: T) -> bool:\n    return a == b\nprint(same(ord, ord))\n");
}

/// **W7-52 — a protocol-typed (existential) value satisfies `Eq` too, so `==`, `.eq()` and a
/// `[T: Eq]` bound give one answer over it exactly as they do over every other receiver kind.**
/// `Ty::Protocol` had no `intrinsic_recv_kind` arm (same root cause as `Ty::Func`/W7-54), so the D1
/// grant excluded it: `x == y` (which never asks the protocol system) already worked, but a
/// `[T: Eq]` bound fed a protocol-typed value answered *does not satisfy Eq* — one relation, two
/// answers. **Go, measured, 1.26** (`comparable` was widened in 1.20 to admit interface types):
/// `func eqg[T comparable](a, b T) bool { return a == b }` fed two `Sized` interface values compiles
/// and runs; fed an uncomparable witness it compiles and panics at the comparison. TICKET-053:
/// Chezzi's own shape for an uncomparable (unsatisfied-`where`) witness is no longer a runtime fault —
/// the erasure into the protocol slot is refused at COMPILE time instead (`Checker::assignable`'s
/// protocol arm) — so `tests/chz/spec/eq_protocol_existential_test.chz` now pins only the SATISFIED
/// half of this relation; the unsatisfied half moved to
/// `checker::tests::protocol_erasure_eq_where_bound_boundaries`.
const SIZED_PRELUDE: &str = "\
protocol Sized_:
    fn size(self) -> int
struct Tag:
    n: int
    fn size(self) -> int:
        return self.n
fn mkp() -> Sized_:
    return Tag(1)
";

#[test]
fn protocol_typed_value_satisfies_eq() {
    // The bound itself, both spellings.
    ok(&format!(
        "{SIZED_PRELUDE}fn same[T: Eq](a: T, b: T) -> bool:\n    return a == b\nprint(same(mkp(), mkp()))\n"
    ));
    ok(&format!(
        "{SIZED_PRELUDE}fn same[T: Eq](a: T, b: T) -> bool:\n    return a.eq(b)\nprint(same(mkp(), mkp()))\n"
    ));
    // The Critical's own repro: two DIFFERENT protocol-typed locals through a generic `[T: Eq]` fn,
    // matching what the bare `==` already answered before this fix.
    ok("\
protocol Sized_:
    fn size(self) -> int
struct Tag:
    n: int
    fn size(self) -> int:
        return self.n
fn generic_eq[T: Eq](a: T, b: T) -> bool:
    return a == b
a: Sized_ = Tag(1)
b: Sized_ = Tag(2)
print(a == b)
print(generic_eq(a, b))
");
}

/// **Guard rail — ONLY `Eq` is granted to a protocol-typed receiver.** No other protocol row was
/// registered for kind `"protocol"`, so a bound requiring a method `Sized_` does not structurally
/// provide must still reject — widening `Eq` alone must not have widened the receiver-kind gate for
/// every other protocol along with it.
#[test]
fn protocol_typed_value_does_not_satisfy_other_protocols() {
    for (bound, needle) in [
        ("Hashable", "does not satisfy Hashable"),
        ("Iterable[int]", "does not satisfy Iterable"),
        ("Add", "does not satisfy Add"),
    ] {
        rejects(
            &format!("{SIZED_PRELUDE}fn probe[T: {bound}](a: T):\n    pass\nprobe(mkp())\n"),
            needle,
        );
    }
}

/// **Guard rail — `Comparable` (which EMBEDS `Eq`) must reject a protocol-typed value on the REAL
/// reason (no `compare`), not on the now-satisfied `Eq` embed.** Before this fix `[T: Comparable]`
/// over `Sized_` reported the confusing *"does not satisfy Eq"* — Sized_ failed the embed, not its
/// own `compare` method, which is not what a reader asking "why doesn't my Sized_ satisfy
/// Comparable" wants to hear. After the fix the `Eq` embed passes (Sized_'s WITNESS defers to
/// runtime, same as the bare `Eq` bound above) and the rejection names the actual gap.
#[test]
fn protocol_typed_value_rejects_comparable_on_the_real_reason() {
    rejects(
        &format!("{SIZED_PRELUDE}fn probe[T: Comparable](a: T):\n    pass\nprobe(mkp())\n"),
        "does not satisfy Comparable",
    );
    // …and NOT the pre-fix message, which named the embed instead of the missing method.
    let errs = check_src(&format!(
        "{SIZED_PRELUDE}fn probe[T: Comparable](a: T):\n    pass\nprobe(mkp())\n"
    ));
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("does not satisfy Eq")),
        "expected the Comparable rejection to name the real reason, not the Eq embed: {errs:?}"
    );
}

/// **Guard rail — protocol-to-protocol assignability is untouched.** `[T: Sized_]` fed a `Sized_`
/// value is Go's interface-to-interface assignment, decided by `Ty::Protocol`'s own structural arm,
/// never by the D1 kind gate this row widens — pinned so a future change to D1 can't be mistaken for
/// what makes this case work.
#[test]
fn protocol_typed_value_still_satisfies_its_own_protocol() {
    ok(&format!(
        "{SIZED_PRELUDE}fn takes_sized[T: Sized_](x: T):\n    pass\ntakes_sized(mkp())\n"
    ));
}

/// **W7-41 — `==` / `!=` enforce the receiver's `eq` `where` bounds.** The explicit method spelling
/// `a.eq(b)` was already rejected (the instance-method dispatch path runs `enforce_bounds`); the
/// OPERATOR routed through `may_be_equal`, which asks co-inhabitance only. Same program, two answers:
/// `check` was clean and the run faulted with *"struct 'Tag' has no 'compare' method"* — the
/// `checker-superset-of-compiler` class parity is structurally blind to.
///
/// **Rust owns conditional conformance; measured, rustc 1.97.0** on the mirror
/// `impl<T: Ord> PartialEq for Boxy<T>`: `error[E0369]: binary operation == cannot be applied to type
/// Boxy<Tag>` / `note: an implementation of Ord might be missing for Tag`, while the satisfying
/// instantiation `Boxy(1) == Boxy(2)` still compiles. Compile-time rejection AND a working satisfying
/// instantiation is the shape asserted here.
///
/// The `ok()` rows are not decoration: this change WIDENS what is rejected, so the suite that passed
/// before it contained no example of what it newly affects. Every one of them was measured on the
/// pre-change binary first (`rule-fires-is-not-rule-is-right`); a previous attempt at this exact fix
/// shipped green on 3952 tests and was reverted the same day for breaking these.
#[test]
fn eq_operator_enforces_the_receivers_eq_where_bounds() {
    // The fixture's `eq` must call the BOUNDED operation. An `eq` body of `self.val == o.val` falls
    // back to structural equality on the payload and exits 0 — the reverted attempt shipped exactly
    // that fixture, with a docstring quoting a fault its own program never produced. This one was
    // verified to fault on the pre-change binary before the assertion was written.
    const COND: &str = "\
struct Tag:
    n: int
struct Box[T]:
    val: T
    fn compare(self, other: Self) -> int where T: Comparable:
        if self.val < other.val:
            return -1
        return 0
    fn eq(self, other: Self) -> bool where T: Comparable:
        return self.compare(other) == 0
    fn hash(self) -> int:
        return 1
a := Box(Tag(1))
b := Box(Tag(2))
";
    const WHY: &str = "cannot compare Box[Tag] and Box[Tag] for equality — Box[Tag]'s `eq` requires Tag: Comparable";
    for expr in [
        "a == b",
        "a != b",
        "[a] == [b]",
        "?a == ?b",
        "(a,) == (b,)",
        "{\"k\": a} == {\"k\": b}",
    ] {
        let errs = check_entry(&format!("{COND}print({expr})\n"));
        assert_eq!(
            errs.len(),
            1,
            "`{expr}`: expected exactly one error, got {errs:?}"
        );
        assert!(
            errs[0].message.starts_with("cannot compare ")
                && errs[0]
                    .message
                    .ends_with("Box[Tag]'s `eq` requires Tag: Comparable"),
            "`{expr}`: got {:?}",
            errs[0].message
        );
    }
    // A `Set` of the same element carries THREE errors, not one, and all three are real: W7-45 made
    // `Box[Tag]` an illegal set ELEMENT, so each of the two constructions is now rejected on its own
    // line — as rustc does, one diagnostic per site. What this arm owns is that the comparison still
    // reports exactly once.
    let errs = check_entry(&format!("{COND}print(Set([a]) == Set([b]))\n"));
    assert_eq!(
        errs.iter()
            .filter(|e| e.message.starts_with("cannot compare "))
            .count(),
        1,
        "expected exactly one comparison error, got {errs:?}"
    );
    assert_eq!(
        errs.iter()
            .filter(|e| e.message.starts_with("Set element type "))
            .count(),
        2,
        "expected both constructions rejected, got {errs:?}"
    );
    // `Result` needs an annotation to land as `Result[Box[Tag]]`, and a struct FIELD is the
    // recursion Task 1's walk owns — both reached through the same single guard.
    entry_rejects(
        &format!("{COND}ra: Box[Tag]! = ?a\nrb: Box[Tag]! = ?b\nprint(ra == rb)\n"),
        "'s `eq` requires Tag: Comparable",
    );
    entry_rejects(
        &format!("{COND}struct Wrap:\n    b: Box[Tag]\nprint(Wrap(a) == Wrap(b))\n"),
        "cannot compare Wrap and Wrap for equality — Box[Tag]'s `eq` requires Tag: Comparable",
    );
    // The rule cannot key on the literal `Self` token: spelling the operand `Box[T]` is the same hook.
    entry_rejects(
        &format!(
            "{}print(a == b)\n",
            COND.replace("other: Self", "other: Box[T]")
        ),
        WHY,
    );

    // ---- the boundary: every row measured green on the PRE-CHANGE binary too ----

    // A satisfying instantiation still works — conditional conformance, not a ban.
    entry_ok(&format!("{COND}print(Box(1) == Box(2))\n"));
    // …and both instantiations in ONE program report exactly one error.
    let errs = check_entry(&format!("{COND}print(Box(1) == Box(2))\nprint(a == b)\n"));
    assert_eq!(errs.len(), 1, "expected exactly one error, got {errs:?}");
    assert_eq!(errs[0].message, WHY);
    // A prior error must not cascade a bogus second diagnostic.
    let errs = check_entry(&format!("{COND}x := nosuchfn(1)\nprint(x == a)\n"));
    assert_eq!(errs.len(), 1, "expected exactly one error, got {errs:?}");
    assert!(
        errs[0].message.contains("unknown name 'nosuchfn'"),
        "got {errs:?}"
    );
    // An `eq` with NO `where` is unconditional, whatever the payload.
    entry_ok(
        "struct Tag:\n    n: int\nstruct Box[T]:\n    val: T\n    fn eq(self, other: Self) -> bool:\n        return true\nprint(Box(Tag(1)) == Box(Tag(2)))\n",
    );
    // The two in-tree ORDINARY-METHOD escape hatches (`tests/chz/spec/eq_protocol_test.chz`): an
    // operand that is not `Self` is not the hook, and their `where_bounds` are empty by construction
    // (a `where` naming the method's own `[U]` merges into `type_params`). Rejecting these is what
    // got the 2026-08-10 attempt reverted.
    entry_ok(
        "enum Opt2[T]:\n    Some(T)\n    Nothing\n    fn eq(self, x: T) -> bool:\n        return true\nstruct Holder[T]:\n    v: T\n    fn eq[U](self, o: U) -> bool:\n        return true\na := Opt2[int].Some(1)\nprint(a == Opt2[int].Some(2))\nprint(Holder(1) == Holder(1))\n",
    );
    // A hook-shaped `eq` whose `where` names its OWN `[U]`: the receiver's type arg must not be
    // substituted into `U`, so the bound is not this rule's business.
    entry_ok(
        "struct Tag:\n    n: int\nstruct Box[T]:\n    val: T\n    fn eq[U](self, other: Self) -> bool where U: Comparable:\n        return true\nprint(Box(Tag(1)) == Box(Tag(2)))\n",
    );
    // `where T: Eq` over the six structurally-equatable-or-Eq-granted payloads the 2026-08-10 revert
    // was caught by. They are green only because Task 1 landed `Eq`-satisfaction-is-what-`==`-accepts
    // first. Plus a function value — all measured `true` today.
    const EQB: &str = "\
struct P:
    n: int
struct Box[T]:
    val: T
    fn eq(self, other: Box[T]) -> bool where T: Eq:
        return true
fn g(x: int) -> int:
    return x
fn ho() -> int?:
    return 1
";
    for payload in [
        "[1,2]", "(1,2)", "P(1)", "b\"ab\"", "ho()",
        // W7-54 — a function value now satisfies `Eq` (identity, not structural), so `Box[fn]` is
        // satisfiable again. **rustc 1.97.0, measured:** `Boxy(f) == Boxy(f)` for `f: fn(i32) -> i32`
        // COMPILES — fn pointers implement `PartialEq`. This spelling printed `true` before W7-41,
        // was regressed to a reject by W7-41 (the `Eq` grant excluded `Ty::Func` entirely, filed as a
        // residual), and is restored here by widening the grant instead of special-casing the walk.
        "g",
    ] {
        entry_ok(&format!("{EQB}print(Box({payload}) == Box({payload}))\n"));
    }
    // TICKET-257 (owner rule U5): a payload Chezzi compares by IDENTITY through a HANDLE — a
    // `Channel` — satisfies `Eq`, exactly as `ch == ch` is accepted, so `where T: Eq` over it holds.
    // Before this ticket the bound rejected it while the operator accepted it.
    entry_ok(&format!(
        "{EQB}print(Box(Channel[int](1)) == Box(Channel[int](1)))\n"
    ));
    // W7-53: an UNBOUNDED operand is no longer tolerated at either nesting — `f`'s own definition must
    // carry `where T: Eq` (matching rustc's `E0369`/Go's "incomparable types"), so both now reject
    // AT THE DEFINITION instead of type-checking clean and faulting at `f`'s call three lines later
    // (the former documented ceiling: `f(Box(Tag(1)), Box(Tag(2)))` used to fault at runtime because
    // inside the body both operands were `Ty::Param` and `f` declared no bound to check).
    // Here the walk reaches `Box[T]`'s OWN declared `eq` (`where T: Comparable`) before ever asking
    // whether `T` itself is `Eq`-bounded, so the reported reason names the MORE SPECIFIC obligation
    // (`Comparable`, not `Eq`) — same specificity rule `eq_where_unsatisfied` already had.
    entry_rejects(
        &format!(
            "{COND}fn f[T](x: Box[T], y: Box[T]) -> bool:\n    return x == y\nprint(f(Box(1), Box(2)))\n"
        ),
        "Box[T]'s `eq` requires T: Comparable",
    );
    entry_rejects(
        &format!("{COND}fn f[T](x: T, y: T) -> bool:\n    return x == y\nprint(f(a, b))\n"),
        "T is not bounded by Eq (add an `Eq` bound to T: `[T: Eq]`, or `where T: Eq` on a fn)",
    );

    // ---- the coupling Task 1's walk depends on ----
    //
    // `eq_bounds_unsatisfied` stops at a SATISFIED declared `eq` (at runtime a declared `eq` replaces
    // the structural walk), which is only sound because the `==` sites INSIDE that `eq`'s body are
    // guarded by this same rule. `W`'s own `eq` is unconditional, so the walk grants `W`; the inner
    // `self.b == o.b` is where the fault lives, and it is caught when the body is checked.
    entry_rejects(
        &format!(
            "{COND}struct W:\n    b: Box[Tag]\n    fn eq(self, other: Self) -> bool:\n        return self.b == other.b\nprint(W(a) == W(b))\n"
        ),
        WHY,
    );
}

/// **TICKET-053 — the `Eq` grant for a protocol-typed receiver (`("Eq", "eq", "protocol")`,
/// `src/checker/proto.rs:321`) ignores the WITNESS's own `where` bound.** Laundering a `Box[Tag]`
/// (Tag is not `Comparable`) through a `Tagged`-typed slot before comparing skips the same-shape
/// direct-comparison rejection `eq_operator_enforces_the_receivers_eq_where_bounds` pins above.
/// Measured on the release binary at 5b2a7937: `chezzi check` reports `ok: no type errors` and
/// `chezzi run` prints `true` then `false` — the unsatisfied `eq`'s own body answer, not a
/// structural fallback (structural `==` on the same values is `false`). Expected, matching the
/// direct-comparison rejection above: the assignment `a: Tagged = Box(Tag(1))` is refused, naming
/// the unsatisfied bound. Currently FAILS — the checker grants `Eq` to `Ty::Protocol`
/// unconditionally, without checking the witness's `eq` bound.
#[test]
fn eq_where_bound_is_unenforced_through_a_protocol_slot() {
    const SRC: &str = "\
protocol Tagged:
    fn tag(self) -> int
struct Tag:
    n: int
struct Box[T]:
    v: T
    fn tag(self) -> int:
        return 0
    fn eq(self, other: Box[T]) -> bool where T: Comparable:
        return true
a: Tagged = Box(Tag(1))
b: Tagged = Box(Tag(2))
";
    entry_rejects(&format!("{SRC}print(a == b)\n"), "Tag: Comparable");
}

/// **TICKET-053 — boundary suite for the erasure gate.** Reuses
/// [`eq_where_bound_is_unenforced_through_a_protocol_slot`]'s fixture (`protocol Tagged`, `struct
/// Tag`, `struct Box[T]` whose `eq` requires `T: Comparable`). Rejection rows (a)-(d) hit the gate at
/// four different erasure sites with no comparison anywhere in the program, proving the refusal fires
/// at the ERASURE, not at a later `==`. Acceptance rows (e)-(h) are the over-fire canaries: (e) proves
/// the receiver's own substitution is consulted (`int` satisfies `Comparable`), (f) proves a witness
/// with no `eq` at all is never asked, (g) proves the `[T: Comparable]` escape works, and (h) is the
/// one that goes red if the gate ever runs the STRICT walk instead of the where-only mode, because the
/// strict `Ty::Param` arm refuses an unbounded `P[T]` even though nothing compares it. The last two
/// rows repeat (d)-style rejection and (h)-style acceptance over the `Result` error slot
/// (`Ty::Protocol("Error")`), which is the same erasure arm.
#[test]
fn protocol_erasure_eq_where_bound_boundaries() {
    const SRC: &str = "\
protocol Tagged:
    fn tag(self) -> int
struct Tag:
    n: int
struct Box[T]:
    v: T
    fn tag(self) -> int:
        return 0
    fn eq(self, other: Box[T]) -> bool where T: Comparable:
        return true
";
    // (a) bare erasure, no comparison anywhere.
    entry_rejects(
        &format!("{SRC}a: Tagged = Box(Tag(1))\n"),
        "Tag: Comparable",
    );
    // (b) erasure into a List push.
    entry_rejects(
        &format!("{SRC}xs: List[Tagged] = []\nxs.push(Box(Tag(1)))\n"),
        "Tag: Comparable",
    );
    // (c) erasure into an Option payload. `protocol_note` only unwraps a DIRECT `Ty::Protocol`
    // expected type (its existing scope, untouched here — see the 15 sites
    // `protocol_note_reaches_every_value_slot` enumerates, none of them nested); the outer var-decl's
    // `expected` here is `Tagged?`, not `Tagged`, so the em-dash clause does not surface. The
    // erasure is still refused, which is the property this row proves.
    entry_rejects(
        &format!("{SRC}o: Tagged? = ?Box(Tag(1))\n"),
        "'?' value: expected Tagged, found Box[Tag]",
    );
    // (d) erasure through a struct field.
    entry_rejects(
        &format!(
            "{SRC}struct Holder:\n    b: Box[Tag]\n    fn tag(self) -> int:\n        return 1\nh: Tagged = Holder(Box(Tag(1)))\n"
        ),
        "Tag: Comparable",
    );
    // (e) int IS Comparable — the receiver's own substitution must be consulted.
    entry_ok(&format!(
        "{SRC}a: Tagged = Box(1)\nb: Tagged = Box(2)\nprint(a == b)\n"
    ));
    // (f) a witness declaring no `eq` at all is never asked.
    entry_ok(
        "protocol Tagged:\n    fn tag(self) -> int\nstruct Plain:\n    n: int\n    fn tag(self) -> int:\n        return 0\na: Tagged = Plain(1)\n",
    );
    // (g) the `[T: Comparable]` escape.
    entry_ok(&format!(
        "{SRC}fn wrap[T: Comparable](x: Box[T]) -> Tagged:\n    return x\n"
    ));
    // (h) an unbounded generic wrapper over a witness with NO `eq` must stay accepted — the where-only
    // mode must not fall back to the strict walk's `Ty::Param` refusal.
    entry_ok(
        "protocol Tagged:\n    fn tag(self) -> int\nstruct P[T]:\n    v: T\n    fn tag(self) -> int:\n        return 0\nfn wrap2[T](x: P[T]) -> Tagged:\n    return x\n",
    );
    // Result error slot: same erasure arm via `Ty::Protocol(\"Error\")`.
    const ERR_SRC: &str = "\
struct Tag:
    n: int
struct MyErr[T]:
    v: T
    fn message(self) -> str:
        return \"boom\"
    fn eq(self, other: MyErr[T]) -> bool where T: Comparable:
        return true
";
    // The `!e` operand is checked against `Error` itself, so the erasure is refused at the error
    // value; the em-dash clause does not surface.
    entry_rejects(
        &format!("{ERR_SRC}fn f() -> int!:\n    return !MyErr(Tag(1))\n"),
        "error value: expected Error, found MyErr[Tag]",
    );
    entry_ok(&format!("{ERR_SRC}fn f() -> int!:\n    return !MyErr(7)\n"));

    // TICKET-053 step 8: the three shapes `tests/chz/spec/eq_protocol_existential_test.chz` used to
    // pin as RUNTIME faults (`recover:` + a message check) no longer compile at all, since
    // `x: Sized_ = Box(Tag(1), "x")` is refused at the erasure. CLAUDE.md puts a compile-time
    // REJECTION in the Rust suite, so these are compile-time rows here instead, over that file's own
    // `Sized_`/`Tag`/`Box[T]` fixture.
    const SIZED_SRC: &str = "\
protocol Sized_:
    fn size(self) -> int
struct Tag:
    n: int
    fn size(self) -> int:
        return self.n
struct Box[T]:
    val: T
    tag: str
    fn eq(self, other: Box[T]) -> bool where T: Comparable:
        return self.val.compare(other.val) == 0
    fn size(self) -> int:
        return 1
";
    // was: `protocol_typed_operator_eq_faults_on_an_unsatisfied_witness`.
    entry_rejects(
        &format!("{SIZED_SRC}x: Sized_ = Box(Tag(1), \"x\")\n"),
        "Tag: Comparable",
    );
    // was: `protocol_typed_contains_faults_on_an_unsatisfied_witness`.
    entry_rejects(
        &format!(
            "{SIZED_SRC}x: Sized_ = Box(Tag(1), \"x\")\ny: Sized_ = Box(Tag(2), \"y\")\nxs: List[Sized_] = [x]\nxs.contains(y)\n"
        ),
        "Tag: Comparable",
    );
    // was: `a_generic_eq_bound_faults_on_an_unsatisfied_witness_same_as_the_operator`.
    entry_rejects(
        &format!(
            "{SIZED_SRC}fn generic_eq[T: Eq](a: T, b: T) -> bool:\n    return a == b\nx: Sized_ = Box(Tag(1), \"x\")\ny: Sized_ = Box(Tag(2), \"y\")\ngeneric_eq(x, y)\n"
        ),
        "Tag: Comparable",
    );
}

/// **W7-45 — the `List` builtins whose runtime is `values_equal`, and `in`.** W7-41 closed the hole
/// for the OPERATOR; `contains`/`index_of`/`dedup`/`unique`/`in` reach the same `values_equal` with a
/// signature validated by `assignable` (or, for `in`, by `compatible`) alone, so the guard was never
/// consulted. Every rejected row below was measured check-clean-then-faulting on the pre-change
/// binary with *"struct 'Tag' has no 'compare' method"*, rc=1.
///
/// The fixture's traps, both measured: an `eq` body of `self.val == o.val` falls back to structural
/// equality and exits 0 (so the body must call the BOUNDED `compare`), and `[a].contains(a)` hits the
/// identity shortcut in `elem_equal` (`src/vm/arith.rs`) and never dispatches (so the two values must
/// be distinct).
///
/// **Rust owns the seam and agrees:** `[Boxy(Tag(1))].contains(&Boxy(Tag(2)))` needs
/// `Boxy<Tag>: PartialEq`, which the `impl<T: Ord> PartialEq for Boxy<T>` mirror does not provide —
/// E0277 at compile time, with the satisfying `[Boxy(1)].contains(&Boxy(2))` still compiling.
///
/// The `ok()` rows are the evidence, not decoration: this WIDENS what is rejected, so the suite that
/// passed before it contains no example of what it newly affects (`widening-untested-by-its-own-suite`).
#[test]
fn list_equality_builtins_enforce_the_element_eq_where_bounds() {
    const COND: &str = "\
struct Tag:
    n: int
struct Box[T]:
    val: T
    fn compare(self, other: Self) -> int where T: Comparable:
        if self.val < other.val:
            return -1
        return 0
    fn eq(self, other: Self) -> bool where T: Comparable:
        return self.compare(other) == 0
    fn hash(self) -> int:
        return 1
a := Box(Tag(1))
b := Box(Tag(2))
";
    const WHY: &str = "Box[Tag]'s `eq` requires Tag: Comparable";
    for (expr, head) in [
        (
            "[a].contains(b)",
            "contains() compares List[Box[Tag]] elements for equality — ",
        ),
        (
            "[a].index_of(b)",
            "index_of() compares List[Box[Tag]] elements for equality — ",
        ),
        (
            "[a, b].dedup()",
            "dedup() compares List[Box[Tag]] elements for equality — ",
        ),
        (
            "[a, b].unique()",
            "unique() compares List[Box[Tag]] elements for equality — ",
        ),
        (
            "a in [b]",
            "cannot test membership of Box[Tag] in List[Box[Tag]] — ",
        ),
    ] {
        let errs = check_entry(&format!("{COND}print({expr})\n"));
        assert_eq!(
            errs.len(),
            1,
            "`{expr}`: expected exactly one error, got {errs:?}"
        );
        assert_eq!(errs[0].message, format!("{head}{WHY}"), "`{expr}`");
    }
    // Through a BINDING, not only a literal receiver — the element type is read off the type, so a
    // named list is the same hook (and is what a real program writes).
    entry_rejects(&format!("{COND}xs := [a]\nprint(xs.contains(b))\n"), WHY);
    // NESTED: the walk already recurses, so a list-of-lists needs no second hook.
    entry_rejects(&format!("{COND}print([[a]].contains([b]))\n"), WHY);

    // ---- the boundary: every row measured green on the PRE-CHANGE binary too ----

    // Scalars, a plain struct, a tuple, `bytes` — nothing here declares a conditional `eq`.
    for elem in ["1", "\"s\"", "true", "(1, 2)", "b\"ab\"", "P(1)"] {
        entry_ok(&format!(
            "struct P:\n    n: int\nprint([{elem}].contains({elem}))\nprint([{elem}].index_of({elem}))\nprint([{elem}, {elem}].dedup().len())\nprint([{elem}, {elem}].unique().len())\nprint({elem} in [{elem}])\n"
        ));
    }
    // The SATISFIED instantiation of the very same struct — conditional conformance, not a ban.
    entry_ok(&format!(
        "{COND}print([Box(1)].contains(Box(2)))\nprint([Box(1)].index_of(Box(2)))\nprint([Box(1), Box(2)].dedup().len())\nprint([Box(1), Box(2)].unique().len())\nprint(Box(1) in [Box(2)])\n"
    ));
    // An empty literal's element type is still `Unknown` — must not cascade.
    entry_ok(&format!(
        "{COND}xs: List[int] = []\nprint(xs.contains(1))\n"
    ));
    // NOT ERASED any more (W7-53): a free `T` is judged directly, same rule as the `==` gate
    // (`eq_bounds_unsatisfied`) — both now reject AT THE DEFINITION (rustc/Go's answer) instead of
    // type-checking clean and faulting at `f([Box(Tag(1))], Box(Tag(2)))` three lines later.
    entry_rejects(
        &format!(
            "{COND}fn f[T](xs: List[T], x: T) -> bool:\n    return xs.contains(x)\nprint(f([1], 2))\n"
        ),
        "T is not bounded by Eq (add an `Eq` bound to T: `[T: Eq]`, or `where T: Eq` on a fn)",
    );
    entry_rejects(
        &format!(
            "{COND}fn f[T](xs: List[T], x: T) -> bool:\n    return x in xs\nprint(f([1], 2))\n"
        ),
        "T is not bounded by Eq (add an `Eq` bound to T: `[T: Eq]`, or `where T: Eq` on a fn)",
    );
    // `count` takes a PREDICATE, so it never routes through `values_equal` — unaffected.
    entry_ok(&format!(
        "{COND}fn pos(x: Box[Tag]) -> bool:\n    return true\nprint([a].count(pos))\n"
    ));
    // The gated methods are not the whole surface: a non-comparing method on the SAME bad element
    // type stays legal, which is what makes this a per-method gate rather than a ban on the type.
    entry_ok(&format!(
        "{COND}print([a].len())\nprint([a, b].chunk(1).len())\n"
    ));
    // A prior error must not cascade a bogus second diagnostic: an ELEMENT type that is already
    // `Unknown` is not walked at all. (A bad ARGUMENT is a different matter and deliberately still
    // reports both — the receiver's element type is concrete and genuinely uncomparable there.)
    let errs = check_entry(&format!(
        "{COND}xs := [nosuchfn(1)]\nprint(xs.contains(1))\n"
    ));
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unknown name 'nosuchfn'"))
            && !errs.iter().any(|e| e.message.contains("for equality")),
        "got {errs:?}"
    );
    // A genuinely incompatible `in` still reports ONE diagnostic, the co-inhabitance one — the new
    // gate is an `else if` so the two questions never both fire at one site.
    let errs = check_entry(&format!("{COND}print(1 in [a])\n"));
    assert_eq!(errs.len(), 1, "expected exactly one error, got {errs:?}");
    assert!(
        errs[0]
            .message
            .starts_with("cannot test membership of int in"),
        "got {errs:?}"
    );
}

/// **W7-53 — a generic body that compares must declare the bound.** Fixes AT THE DEFINITION, matching
/// both owning ancestors (measured): rustc 1.97.0 rejects `fn f<T>(a: T, b: T) -> bool { a == b }`
/// with `error[E0369]: binary operation \`==\` cannot be applied to type \`T\`` and *help: consider
/// restricting type parameter \`T\` with trait \`PartialEq\``; Go 1.26 rejects the mirror with `invalid
/// operation: a == b (incomparable types in type set)`. Before this fix all three instances below
/// were `ok: no type errors` and then faulted at runtime with *"struct 'Tag' has no method
/// 'compare'"* — the checker's own `Ty::Param` arm already refused an unbounded `T`,
/// but the four use-site gates erased free params to `Ty::Unknown` first (`eq_bounds_unsatisfied_erased`,
/// since deleted — no caller survived switching all four to the plain, non-erasing
/// `eq_bounds_unsatisfied`), so the refusal never fired.
#[test]
fn a_generic_body_that_compares_must_bound_its_type_param_by_eq() {
    const TAG_BOX: &str = "\
struct Tag:
    n: int
struct Box[T]:
    val: T
    fn hash(self) -> int:
        return 0
    fn eq(self, other: Box[T]) -> bool where T: Comparable:
        return self.val.compare(other.val) == 0
";
    // Instance 1 — the `==` operator, both operands bare `T`.
    rejects(
        &format!(
            "{TAG_BOX}fn f[T](a: T, b: T) -> bool:\n    return a == b\nprint(f(Box(Tag(1)), Box(Tag(2))))\n"
        ),
        "T is not bounded by Eq (add an `Eq` bound to T: `[T: Eq]`, or `where T: Eq` on a fn)",
    );
    // Instance 2 — `List[T].contains`.
    rejects(
        &format!(
            "{TAG_BOX}fn f[T](xs: List[T], b: T) -> bool:\n    return xs.contains(b)\nprint(f([Box(Tag(1))], Box(Tag(2))))\n"
        ),
        "T is not bounded by Eq (add an `Eq` bound to T: `[T: Eq]`, or `where T: Eq` on a fn)",
    );
    // Instance 3 — `[T: Hashable]` alone, `Set(xs)` construction inside the body: `Hashable` does
    // NOT imply `Eq` (measured: embedding it broke the `Holder`-shaped ordinary-`eq`-method escape
    // hatch — see `key_ty_reject`'s doc), so the map/set-key gate demands both bounds explicitly.
    rejects(
        &format!(
            "{TAG_BOX}fn f[T: Hashable](xs: List[T]) -> int:\n    return Set(xs).len()\nprint(f([Box(Tag(1)), Box(Tag(2))]))\n"
        ),
        "T is not bounded by Eq (add an `Eq` bound to T: `[T: Eq]`, or `where T: Eq` on a fn)",
    );

    // ---- ok() neighbours derived from the same premise ----

    // `[T: Eq]` — the direct fix.
    ok("fn f[T: Eq](a: T, b: T) -> bool:\n    return a == b\nprint(f(1, 2))\n");
    // `[T: Comparable]` — already works: `Comparable` embeds `Eq` (`std/prelude.chz`), untouched by
    // this change.
    ok("fn f[T: Comparable](a: T, b: T) -> bool:\n    return a == b\nprint(f(1, 2))\n");
    // A generic fn that takes `T` but never compares it — the bound obligation is per-USE, not a tax
    // on every type parameter.
    ok("fn f[T](a: T) -> T:\n    return a\nprint(f(1))\n");
    // A partially-generic type, `Map[T, Box[Tag]]`: `T` is bounded (deferred to its OWN declared
    // bound, cheap — `Map`'s own key bound is `Hashable + Eq`, W7-53), so only the CONCRETE half
    // (`Box[Tag]`, which has no declared `eq` and is structurally sound — `Tag` is a plain `int`
    // field) is actually walked structurally.
    ok(
        "struct Tag:\n    n: int\nstruct Box[T]:\n    val: T\nfn same_map[T: Hashable + Eq](a: Map[T, Box[Tag]], b: Map[T, Box[Tag]]) -> bool:\n    return a == b\n",
    );
    // A generic fn whose `T` is never bare-compared, only reached via a declared conditional `eq`
    // whose `where` bound IS satisfied by `T`'s own declared bound (`Comparable`, not `Eq` — the
    // `eq_where_unsatisfied` obligation is more specific than the `Ty::Param` arm's).
    ok(&format!(
        "{TAG_BOX}fn f[T: Comparable](x: Box[T], y: Box[T]) -> bool:\n    return x == y\nprint(f(Box(1), Box(2)))\n"
    ));
}

/// **W7-45, the map/set half — a key type must be `Eq`, not merely `Hashable`.** A `Map`/`Set` probe
/// runs `values_equal` on a hash COLLISION, so a key whose `eq` has unsatisfied `where` bounds
/// check-cleaned and then faulted. It is the worse half of the row because the verdict depended on
/// hash values: the SAME unsound program with a constant `hash` faulted and with distinct hashes
/// printed a silent wrong `false`, rc=0.
///
/// **Rust owns this seam and spells it in the bound:** `impl<T: Eq + Hash> HashSet<T>` /
/// `impl<K: Eq + Hash, V> HashMap<K, V>` — `Hash` alone is never enough. Chezzi's `Hashable`
/// protocol does not embed `Eq` (`embeds: Vec::new()`), so the key check has to ask both.
///
/// One line in `key_ty_reject` is the whole fix, because EVERY map-key and set-element position in
/// the checker funnels through it. This test is the verification of that claim rather than an
/// assumption about it: the literal, the comprehension, `Set(list)` construction (a site distinct
/// from the `Set([…])` literal form), the annotation, `m[k]` read and write, and each container
/// method are all asserted from the one change.
#[test]
fn a_map_key_or_set_element_must_be_eq_not_merely_hashable() {
    const COND: &str = "\
struct Tag:
    n: int
struct Box[T]:
    val: T
    fn compare(self, other: Self) -> int where T: Comparable:
        if self.val < other.val:
            return -1
        return 0
    fn eq(self, other: Self) -> bool where T: Comparable:
        return self.compare(other) == 0
    fn hash(self) -> int:
        return 1
a := Box(Tag(1))
b := Box(Tag(2))
";
    const WHY: &str = "Box[Tag]'s `eq` requires Tag: Comparable";
    // Each of these was measured check-clean-then-faulting on the pre-change binary.
    for tail in [
        "print(Set([a]).len())\n",                        // Set literal-ish ctor
        "s := Set([a])\ns.add(b)\nprint(s.len())\n",      // .add
        "s := Set([a])\nprint(s.contains(b))\n",          // .has
        "s := Set([a])\nprint(b in s)\n",                 // `in` over a set
        "print({a: 1}.len())\n",                          // map literal
        "m := {a: 1}\nprint(m.contains(b))\n",            // Map.has
        "m := {a: 1}\nprint(m.get(b))\n",                 // Map.get
        "m := {a: 1}\nm[b] = 2\nprint(m.len())\n",        // index write
        "m := {a: 1}\nprint(m[b])\n",                     // index read
        "m := {a: 1}\nprint(b in m)\n",                   // `in` over a map
        "print({x for x in [a]}.len())\n",                // set comprehension
        "print({x: 1 for x in [a]}.len())\n",             // map comprehension
        "xs := [a]\nprint(Set(xs).len())\n",              // Set(list) CONSTRUCTION
        "s: Set[Box[Tag]] = Set([])\nprint(s.len())\n",   // Set annotation
        "m: Map[Box[Tag], int] = {}\nprint(m.len())\n",   // Map annotation
        "s: Set[Box[Tag]] = Set([])\ns2 := s.union(s)\n", // set algebra
        "struct W:\n    s: Set[Box[Tag]]\nprint(W(Set([a])).s.len())\n", // a struct FIELD's type
    ] {
        let errs = check_entry(&format!("{COND}{tail}"));
        assert!(
            errs.iter().any(|e| e.message.ends_with(WHY)),
            "`{tail}`: expected the eq-bound reason, got {errs:?}"
        );
    }
    // The message TAIL changes with the reason; the four prefixes do not, so no existing needle moves.
    let errs = check_entry(&format!("{COND}print(Set([a]).len())\n"));
    assert_eq!(
        errs[0].message,
        format!("Set element type {WHY}"),
        "got {errs:?}"
    );
    let errs = check_entry(&format!("{COND}print({{a: 1}}.len())\n"));
    assert_eq!(
        errs[0].message,
        format!("map key type {WHY}"),
        "got {errs:?}"
    );
    // The Hashable half still reports the Hashable text — the new conjunct does not swallow it, and
    // a type that fails BOTH reports only the first (`Hashable` is checked first, as before).
    rejects(
        "struct Bare:\n    a: int\ns: Set[Bare] = Set()\n",
        "Set element type must implement Hashable",
    );

    // ---- the boundary: every row measured green on the PRE-CHANGE binary too ----

    // Ordinary key types, at the literal / comprehension / ctor / annotation / index sites.
    entry_ok(
        "struct Z:\n    pass\nstruct H:\n    n: int\n    fn hash(self) -> int:\n        return self.n\nprint(Set([1]).len())\nprint(Set([\"s\"]).len())\nprint(Set([true]).len())\nprint(Set([b\"a\"]).len())\nprint(Set([Z()]).len())\nprint(Set([H(1)]).len())\nprint({\"k\": 1}.len())\nprint({x for x in [1]}.len())\nprint({x: 1 for x in [1]}.len())\nxs := [1, 2]\nprint(Set(xs).len())\nm := {\"k\": 1}\nprint(m[\"k\"])\nm[\"j\"] = 2\n",
    );
    // The SATISFIED instantiation of the very same conditional-`eq` struct, everywhere.
    entry_ok(&format!(
        "{COND}s := Set([Box(1)])\ns.add(Box(2))\nprint(s.contains(Box(1)))\nm := {{Box(1): 1}}\nm[Box(2)] = 2\nprint(m[Box(1)])\nprint(Box(1) in m)\n"
    ));
    entry_ok(&format!(
        "{COND}s: Set[Box[int]] = Set([])\nm: Map[Box[int], int] = {{}}\nprint(s.len() + m.len())\n"
    ));
    // An empty literal's element/key type is still `Unknown` — must not cascade.
    entry_ok(&format!("{COND}print(Set([]).len())\nprint({{}}.len())\n"));
    // NOT ERASED any more (W7-53): a free `T` reaching a spelled `Set[T]`/`Map[T, _]` position inside
    // a generic body is judged directly, so `[T: Hashable]` alone no longer accepts a body that itself
    // builds/indexes the container — `Hashable` does not imply `Eq` (measured: embedding it broke the
    // `Holder`-shaped ordinary-`eq`-method escape hatch, `key_ty_reject`'s doc), so the DECLARATION
    // must spell both, exactly as Rust's own `HashSet<T>`/`HashMap<K, V>` require `Eq + Hash`.
    entry_ok(
        "fn h[T: Hashable + Eq](xs: Set[T]) -> int:\n    return xs.len()\nfn k[T: Hashable + Eq](m: Map[T, int]) -> int:\n    return m.len()\nfn s2[T: Hashable + Eq](x: T) -> int:\n    s := Set([x])\n    s.add(x)\n    m := {x: 1}\n    return s.len() + m.len()\nprint(h(Set([1])) + k({1: 2}) + s2(1))\n",
    );
    rejects(
        "fn h[T](xs: Set[T]) -> int:\n    return xs.len()\n",
        "Set element type must implement Hashable",
    );
    // W7-53's third measured instance: `[T: Hashable]` alone, with a `Set(xs)` construction inside
    // the body, is now rejected at ITS OWN definition (`s2`'s `Set([x])`/`{x: 1}` above are the
    // `+ Eq` fix; this is the same body with the bound removed).
    rejects(
        "fn f[T: Hashable](xs: List[T]) -> int:\n    return Set(xs).len()\n",
        "T is not bounded by Eq (add an `Eq` bound to T: `[T: Eq]`, or `where T: Eq` on a fn)",
    );
}

/// **W7-41's enabling half.** `Eq` is granted to a type whose structural equality walk stays sound —
/// NOT to one that reaches a declared `eq` whose `where` bounds do not hold for this instantiation.
/// `Box[Tag]`'s `eq` calls `compare`, which `Tag` does not have, so `Box[Tag]` is not `Eq` and
/// neither is a `List` of them (the walk recurses exactly where `values_equal` does).
#[test]
fn eq_is_refused_when_the_walk_reaches_an_unsatisfied_conditional_eq() {
    const COND: &str = "\
struct Tag:
    n: int
struct Box[T]:
    val: T
    fn compare(self, other: Self) -> int where T: Comparable:
        if self.val < other.val:
            return -1
        return 0
    fn eq(self, other: Self) -> bool where T: Comparable:
        return self.compare(other) == 0
fn same[T: Eq](a: T, b: T) -> bool:
    return a == b
";
    // the receiver itself
    entry_rejects(
        &format!("{COND}print(same(Box(Tag(1)), Box(Tag(2))))\n"),
        "does not satisfy Eq",
    );
    // …and REACHED through a container, exactly as `values_equal` reaches it
    entry_rejects(
        &format!("{COND}print(same([Box(Tag(1))], [Box(Tag(2))]))\n"),
        "does not satisfy Eq",
    );
    // …and reached through a FIELD, which is the arm that was broken: type ARGS were substituted
    // correctly, declared FIELD types were not, so a `Wrap[Tag]` whose field is `Box[Z]` walked the
    // decl-site `Z` instead of `Tag`.
    entry_rejects(
        &format!(
            "{COND}struct Wrap[Z]:\n    v: Box[Z]\nprint(same(Wrap(Box(Tag(1))), Wrap(Box(Tag(2)))))\n"
        ),
        "does not satisfy Eq (Box[Tag]'s `eq` requires Tag: Comparable)",
    );
    // NAME CAPTURE — the check-OK-then-run-fault the unsubstituted walk shipped: put a same-named
    // param carrying the needed bound in the CALLER's scope and the decl-site `Z` resolved against
    // IT, so `Wrap[Tag]` was granted and `Tag.compare` faulted at runtime.
    entry_rejects(
        &format!(
            "{COND}struct Wrap[Z]:\n    v: Box[Z]\nfn trick[Z: Comparable](z: Z, a: Wrap[Tag], b: Wrap[Tag]) -> bool:\n    return same(a, b)\nprint(trick(1, Wrap(Box(Tag(1))), Wrap(Box(Tag(2)))))\n"
        ),
        "type Wrap[Tag] does not satisfy Eq",
    );
    // A SATISFYING instantiation still works — the rule is conditional conformance, not a ban.
    entry_ok(&format!("{COND}print(same(Box(1), Box(2)))\n"));
    // And a type whose own `eq` is unconditional keeps it even when a type ARG is not equatable
    // through its own bounded `eq`: `Plain`'s `eq` never touches `Box[Tag]`'s.
    entry_ok(&format!(
        "{COND}struct Plain[T]:\n    v: T\n    fn eq(self, other: Self) -> bool:\n        return true\nprint(same(Plain(Box(Tag(1))), Plain(Box(Tag(2)))))\n"
    ));
}

/// The `where T: Eq` bound check hops OUT of the structural walk (`satisfies` → the walk →
/// `satisfies` again), and the walk's own name-stack does not span that hop. A type whose type
/// ARGUMENT is itself closes the loop, so the guard must be across the boundary.
///
/// Re-asking an obligation already in progress answers "assume it holds" — a COINDUCTIVE assumption,
/// not a decline: the walk it returns to still checks every remaining field. **Rust agrees, measured
/// (rustc 1.97.0):** `struct C<T: Eq>` + `struct D { x: Vec<C<D>> }` with manual `Eq` impls compiles
/// and runs under `fn needs<U: Eq>`.
#[test]
fn conditional_eq_bound_recursion_terminates() {
    const CYCLE: &str = "\
struct C[T]:
    v: T
    fn eq(self, other: Self) -> bool where T: Eq:
        return true
struct D:
    x: C[D]
fn same[T: Eq](a: T, b: T) -> bool:
    return a == b
fn use_it(a: D, b: D) -> bool:
    return same(a, b)
";
    entry_ok(CYCLE);
    // POLYMORPHIC recursion — `N[int]` → `N[List[int]]` → `N[List[List[int]]]` → … never repeats an
    // instantiation, so the identity-keyed guard cannot close it and the SIZE backstop must: this
    // terminates, and it REFUSES rather than granting. It used to be accepted, but only because the
    // inner guard was keyed on the bare NAME — the same defect that let a different instantiation of
    // an in-progress name through unchecked, so its acceptance here was never a proof of anything.
    //
    // **Rust refuses the same shape**, measured (rustc 1.97.0, `scratchpad/polyrec.rs`):
    // `#[derive(PartialEq, Eq)] struct N<T: Eq> { v: T, next: Option<Box<N<Vec<T>>>> }` is
    // `error[E0320]: overflow while adding drop-check rules for N<i32>`. Prehead rejected it too, so
    // this is not a regression — it is the fail-closed answer agreeing with the ancestor.
    entry_rejects(
        "\
struct N[T]:
    v: T
    next: N[List[T]]?
fn same[T: Eq](a: T, b: T) -> bool:
    return a == b
fn use_it(a: N[int], b: N[int]) -> bool:
    return same(a, b)
",
        "nests too deeply",
    );
}

/// **A DIFFERENT INSTANTIATION of a type already being walked is a different obligation.** The inner
/// cycle guard was keyed on the bare NAME, so re-entering `R` with another argument was assumed
/// sound and its fields were never walked — `R[int]` type-checked clean and then faulted at runtime.
/// The bad type has to arrive by SUBSTITUTION into a field, which is why fixing the
/// substitution is what exposed this: before that, an unbound decl-site param refused it by accident.
#[test]
fn a_different_instantiation_of_an_in_progress_type_is_still_walked() {
    entry_rejects(
        "\
struct Tag:
    n: int
struct Bad[T]:
    val: T
    fn compare(self, other: Self) -> int where T: Comparable:
        return 0
    fn eq(self, other: Self) -> bool where T: Comparable:
        return self.compare(other) == 0
struct R[T]:
    v: Bad[T]
    w: R[Tag]?
fn needs[U: Eq](a: U, b: U) -> bool:
    return a == b
fn use_it(a: R[int], b: R[int]) -> bool:
    return needs(a, b)
",
        "type R[int] does not satisfy Eq (Bad[Tag]'s `eq` requires Tag: Comparable)",
    );
}

/// **A cursor nested inside another type has nothing to prove.** The fail-closed miss→refuse path
/// caught the built-in cursor, whose `Ty::Struct("Iterator", …)` is in no struct table — so a `List`
/// of cursors, or a struct with a cursor field, stopped satisfying `Eq` even though comparing them
/// works (by identity). A regression against the previous commit, in the widening direction.
#[test]
fn a_nested_cursor_does_not_poison_its_container() {
    entry_ok(
        "fn eqm[T: Eq](a: T, b: T) -> bool:\n    return a == b\nprint(eqm([[1, 2].iter()], [[1, 2].iter()]))\n",
    );
    entry_ok(
        "\
struct HasIt:
    it: Iterator[int]
fn eqm[T: Eq](a: T, b: T) -> bool:
    return a == b
fn use_it(a: HasIt, b: HasIt) -> bool:
    return eqm(a, b)
",
    );
    // The cursor ITSELF satisfies `Eq` by identity (TICKET-257).
    entry_ok(
        "fn eqm[T: Eq](a: T, b: T) -> bool:\n    return a == b\nprint(eqm([1, 2].iter(), [1, 2].iter()))\n",
    );
}

/// **A chain of DISTINCT conditional types must be walked to its true answer, and running out of
/// budget must REFUSE.** The first cut guarded the outward hop with a fixed re-entrancy DEPTH cap
/// that returned "sound" when exceeded — so a 16-deep chain whose innermost link genuinely fails
/// type-checked clean and then faulted at runtime. The guard is now keyed on the
/// instantiated type: distinct types never trip it, so the chain is decided correctly at depth, and
/// the size backstop that remains refuses instead of granting.
///
/// **The SOUND chain is what makes this test mean anything.** An unsound chain rejects whether the
/// walk found the bad link or merely ran out of budget, so on its own it cannot tell the two apart —
/// it passed vacuously at N=100 while a 63-deep SOUND chain was being wrongly refused. The pair is
/// the assertion: same shape, one link different, opposite verdicts, at every depth.
#[test]
fn a_long_chain_of_conditional_eq_types_is_decided_not_waved_through() {
    // `sound` swaps the innermost link from `Box[Tag]` (no `compare`, so the chain's `where` bounds
    // cannot hold) to `Box[int]` (Comparable, so every link is satisfiable). Nothing else differs.
    let chain = |n: usize, sound: bool| {
        let innermost = if sound { "Box[int]" } else { "Box[Tag]" };
        let mut s = String::from(
            "struct Tag:\n    n: int\n\
             struct Box[T]:\n    val: T\n    \
             fn compare(self, other: Self) -> int where T: Comparable:\n        return 0\n    \
             fn eq(self, other: Self) -> bool where T: Comparable:\n        return self.compare(other) == 0\n\
             struct C[T]:\n    v: T\n    \
             fn eq(self, other: Self) -> bool where T: Eq:\n        return true\n",
        );
        s.push_str(&format!("struct A0:\n    v: {innermost}\n"));
        for i in 1..=n {
            s.push_str(&format!("struct A{i}:\n    v: C[A{}]\n", i - 1));
        }
        s.push_str(&format!(
            "fn needs[U: Eq](a: U, b: U) -> bool:\n    return a == b\nfn use_it(a: A{n}, b: A{n}) -> bool:\n    return needs(a, b)\n"
        ));
        s
    };
    // Well past the old depth cap of 16, and past the old backstop of 64 that refused sound chains.
    for n in [4, 20, 100] {
        entry_rejects(
            &chain(n, false),
            &format!(
                "type A{n} does not satisfy Eq (C[A{}]'s `eq` requires A{}: Eq)",
                n - 1,
                n - 1
            ),
        );
        entry_ok(&chain(n, true));
    }
    // This test used to end with `entry_rejects(&chain(N, true), "nests too deeply")` — "past the
    // backstop, even a SOUND chain is refused". **That assertion is RETIRED, not silently dropped,
    // because its premise stopped being true** when the depth cap became a cumulative NODE budget
    // over the in-progress path (`EQ_BOUNDS_MAX_NODES`; see that constant for why the shape
    // classifier in front of it was deleted rather than patched a third time).
    //
    // Measured, release binary, this exact `chain` shape: N = 10 000 / 16 000 / 25 000 all check
    // CLEAN (135 ms / 240 ms / 353 ms), because `walk_eq_members` pushes a chain link's MEMBERS, and
    // each member here is one node — so a non-growing chain consumes budget linearly and only
    // reaches 50 000 at a depth whose *parse* cost dominates by an order of magnitude. Widening the
    // per-link type does not help either: a 40-wide tuple per link at 1 500 links is still `ok`
    // (478 ms), since the wide tuple is pushed once and popped before its members are walked.
    //
    // That is the DESIGNED behaviour, not a gap: the budget exists to bound a walk whose types GROW,
    // and a non-growing graph is exactly what W7-55 was filed about the checker wrongly refusing. So
    // the budget refusal is pinned where it is actually reachable — on growing shapes — by
    // `polymorphic_recursion_is_refused_in_bounded_work` and its permutation / `Ty::Func` siblings,
    // which reach it in 37-48 ms and 22-27 MB. Re-adding a "sound chain is refused" assertion here
    // would require a fixture whose cost is dominated by parsing, to assert a property the design
    // deliberately does not have.
    //
    // What IS worth pinning here is the other direction, and it is the whole point of W7-55: a
    // deep-but-finite chain is NOT refused. 3 000 links is ~19x the depth the original cap (160)
    // rejected, and the VM compares such a value happily.
    entry_ok(&chain(3_000, true));
}

/// **The budget refusal must not print a multi-KB type name.** This diagnostic is streamed to the
/// editor, and the shape that trips a refusal can have an enormous name — a `"nests too deeply"`
/// needle cannot see length, so assert length.
///
/// **M6 (adversarial review of W7-55, same day): the FIRST half below is near-vacuous now, and
/// deliberately kept anyway, with the explanation inline instead of a silent skip.** Its original
/// premise was that polymorphic recursion (`N[List[List[…]]]`) organically grows the type name huge
/// before it is refused. Guard A refuses growth in BOUNDED work (at most two consecutive growing
/// re-entries), so the walk never gets deep enough to build a multi-KB name through recursion alone
/// — the assertion below is true, but not because elision fired; a short root just stays short. There
/// is no way to reconstruct the original premise without defeating Guard A (nested generic args are
/// the only thing that makes a `Ty`'s printed name large, and that is exactly the shape Guard A now
/// intercepts early) — so the SECOND half, which builds an already-huge ROOT by hand instead of
/// growing one, is what actually exercises the elision path.
#[test]
fn the_budget_refusal_elides_a_runaway_type_name() {
    let budget_msg = |src: &str| {
        let errs = check_entry(src);
        errs.iter()
            .find(|e| e.message.contains("nests too deeply"))
            .map(|e| e.message.clone())
            .unwrap_or_else(|| panic!("expected a budget refusal, got: {errs:?}"))
    };
    // Vacuous per the doc above: the root (`N[int]`) is short on its own, so this stays short
    // whether or not elision fires. Kept as a cheap regression tripwire, not as elision coverage.
    let msg = budget_msg(
        "\
struct N[T]:
    v: T
    next: N[List[T]]?
fn same[T: Eq](a: T, b: T) -> bool:
    return a == b
fn use_it(a: N[int], b: N[int]) -> bool:
    return same(a, b)
",
    );
    assert!(
        msg.len() < 200,
        "the budget refusal must stay short, got {} chars: {msg}",
        msg.len()
    );
    // …and when the ROOT ITSELF is written with a huge name, the elision is what bounds the clause
    // this predicate owns. (The enclosing `type X does not satisfy Eq (…)` wrapper still renders the
    // type in full — that is `satisfies_args_d`'s shared wording for EVERY protocol rejection, not
    // this refusal's, and is out of scope here.)
    let deep = (0..40).fold("int".to_string(), |acc, _| format!("List[{acc}]"));
    let msg = budget_msg(&format!(
        "struct N[T]:\n    v: T\n    next: N[List[T]]?\nfn same[T: Eq](a: T, b: T) -> bool:\n    return a == b\nfn use_it(a: N[{deep}], b: N[{deep}]) -> bool:\n    return same(a, b)\n"
    ));
    let clause = msg
        .rfind(" (")
        .map(|i| &msg[i..])
        .expect("the refusal is a parenthesised clause");
    assert!(
        clause.len() < 200 && clause.contains("(elided)"),
        "the budget clause must elide a long root type, got {} chars: {clause}",
        clause.len()
    );
}

/// **The walk must be linear in a SHARED-FIELD type graph, not exponential.** The cycle guard is
/// PATH-based: it stops a type being re-entered while it is on the stack, and says nothing about one
/// already finished. So a struct with two fields of the same nested type re-walks that subtree twice
/// per level — 4x per level measured, i.e. 2^N. This is a HANG, on the path the LSP runs on every
/// keystroke, and the depth backstop cannot see it because the shape stays shallow.
///
/// Measured on the release binary before the memo: N=20 → 0.54s, N=22 → 2.4s, N=24 → 9.5s,
/// N=26 → 37.2s; `chezzi-prehead` 0.003s at every size. N=24 is used here because it is ~9.5s
/// unfixed — unambiguous against any plausible machine — while staying a fraction of a second fixed.
#[test]
fn a_shared_field_type_graph_is_walked_once_per_type() {
    let mut src = String::from("struct B0:\n    x: int\n");
    for i in 1..=24 {
        src.push_str(&format!(
            "struct B{i}:\n    a: B{}\n    b: B{}\n",
            i - 1,
            i - 1
        ));
    }
    src.push_str(
        "fn needs[U: Eq](p: U, q: U) -> bool:\n    return p == q\nfn use_it(p: B24, q: B24) -> bool:\n    return needs(p, q)\n",
    );
    let (errs, entries) = eq_obligations_for(&src);
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
    let want = 24..=8 * 24;
    assert!(
        want.contains(&entries),
        "the shared-field walk is exponential again: B24 entered {entries} Eq obligations (want \
         {want:?}; path-guard-only: 33554431)"
    );
}

/// **The same shape with a CYCLE in it — and this is the one that matters.** The acyclic test above
/// passed while this took 26 s at N=24, because the first memo refused to cache any result derived
/// under a coinductive assumption, and one back-pointer anywhere below a node marks the whole path
/// above it as assumption-derived. So the memo switched itself off on exactly the graphs it was
/// written for: a node holding a reference back to its root is an ordinary shape, not a pathology.
///
/// Measured on the release binary: `abd15c1e` N=18 0.39s / N=20 1.57s / N=22 6.43s / N=24 26.2s,
/// while the identical ACYCLIC graph was 0.003s throughout — which is precisely why the acyclic
/// test could not see it. Pre-D1 and fixed: 0.003s at every size.
#[test]
fn a_cyclic_shared_field_type_graph_is_also_walked_once_per_type() {
    let mut src = String::from("struct L0:\n    r: Root?\n");
    for i in 1..=22 {
        src.push_str(&format!(
            "struct L{i}:\n    x: L{}\n    y: L{}\n",
            i - 1,
            i - 1
        ));
    }
    src.push_str("struct Root:\n    a: L22\n    b: L22\n");
    src.push_str(
        "fn needs[U: Eq](p: U, q: U) -> bool:\n    return p == q\nfn use_it(p: Root, q: Root) -> bool:\n    return needs(p, q)\n",
    );
    let (errs, entries) = eq_obligations_for(&src);
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
    let want = 22..=8 * 22;
    assert!(
        want.contains(&entries),
        "the CYCLIC shared-field walk is exponential again: entered {entries} Eq obligations (want \
         {want:?}; assumption-poisoned memo: 16777215)"
    );
}

/// The budget refusal must name the type the walk STARTED from. For a chain `S{N} → … → S0` the
/// entry that actually exhausts the budget is `S0` — which nests not at all — so naming it told the
/// user about the one link that is definitely fine.
///
/// W7-55: the backstop became a cumulative NODE budget rather than a depth cap, so the chain that
/// exhausts it is written with a type ARGUMENT (four nodes per link: `S{i}` plus `Map[int, int]`)
/// instead of by sheer length. The property under test is unchanged.
#[test]
fn the_budget_refusal_names_the_type_the_walk_started_from() {
    const N: usize = 12_600;
    let mut src = String::from("struct S0[T]:\n    v: T\n");
    for i in 1..=N {
        src.push_str(&format!(
            "struct S{i}[T]:\n    v: T\n    next: S{}[T]\n",
            i - 1
        ));
    }
    src.push_str(&format!(
        "fn needs[U: Eq](a: U, b: U) -> bool:\n    return a == b\nfn use_it(a: S{N}[Map[int, int]], b: S{N}[Map[int, int]]) -> bool:\n    return needs(a, b)\n"
    ));
    let errs = check_entry(&src);
    let msg = errs
        .iter()
        .find(|e| e.message.contains(eq::EQ_BUDGET_MARKER))
        .map(|e| e.message.clone())
        .unwrap_or_else(|| panic!("expected a budget refusal, got: {errs:?}"));
    assert!(
        msg.contains(&format!("S{N}[Map[int, int]] {}", eq::EQ_BUDGET_MARKER)),
        "the refusal must name the walk's ROOT, not the innermost entry: {msg}"
    );
    assert!(
        !msg.contains("S0[Map[int, int]] "),
        "S0 is the bottom of the chain — it nests not at all: {msg}"
    );
}

/// **The depth cap must not refuse an ordinary deep chain.** It exists for shapes that cannot
/// terminate (polymorphic recursion), and a plain 140-link chain is not one — the VM's own equality
/// has no cap and runs it. The ceiling that remains is filed as `docs/gaps.md` W7-55.
#[test]
fn a_deep_but_finite_chain_stays_within_the_eq_budget() {
    let mut src = String::from("struct S0:\n    v: int\n");
    for i in 1..=140 {
        src.push_str(&format!("struct S{i}:\n    v: S{}\n", i - 1));
    }
    src.push_str(
        "fn needs[U: Eq](a: U, b: U) -> bool:\n    return a == b\nfn use_it(a: S140, b: S140) -> bool:\n    return needs(a, b)\n",
    );
    entry_ok(&src);
}

/// **A 10 000-deep PLAIN chain — the depth the VM\'s own `values_equal` walks — must check clean.**
/// This is W7-55\'s original complaint in one assertion: the checker used to refuse at `S160` a type
/// graph the runtime compares to 10 000. A plain `S{k}: v: S{k-1}` link costs one node, so this sits
/// at a fifth of `EQ_BOUNDS_MAX_NODES` — the headroom the budget was sized for. Bare `==` reaches
/// [`Checker::eq_bounds_unsatisfied`] directly (`checker/pattern.rs`\'s binary-op arm calls it on both
/// operands before falling back to `may_be_equal`), the same path the original W7-55 measurement used.
///
/// Deliberately CHECK-only (`check_src`, not a compile+run through [`crate::vm::run_capture`]) — see
/// [`runtime_boundary_is_honoured_at_a_depth_the_old_cap_refused`] below for the "the grant is
/// honoured at runtime, not just claimed" half at an affordable depth instead.
#[test]
fn a_ten_thousand_deep_plain_chain_checks_clean() {
    let mut src = String::from("struct S0:\n    v: int\n");
    for i in 1..=10_000 {
        src.push_str(&format!("struct S{i}:\n    v: S{}\n", i - 1));
    }
    src.push_str("fn use_it(a: S10000, b: S10000) -> bool:\n    return a == b\n");
    let errs = check_src(&src);
    assert!(
        errs.is_empty(),
        "a 10 000-deep plain chain is exactly what the VM compares happily: {errs:?}"
    );
}

/// **The ACCEPTED half of the cap must actually RUN, not just check clean** — `entry_ok` alone would
/// not have caught a bug in how the walk's GRANT interacts with, say, codegen for a deep field chain.
/// Not run at the exact new cap boundary (see the test above for why that pair stays check-only); this
/// depth is chosen to be well past the OLD 160/240 caps — the shape that used to be wrongly refused —
/// while staying cheap enough to compile and execute in the normal test suite.
#[test]
fn runtime_boundary_is_honoured_at_a_depth_the_old_cap_refused() {
    const N: usize = 2_000;
    let mut src = String::from("struct S0:\n    v: int\nfn mk0() -> S0:\n    return S0(v=1)\n");
    for i in 1..=N {
        src.push_str(&format!(
            "struct S{i}:\n    v: S{}\nfn mk{i}() -> S{i}:\n    return S{i}(v=mk{}())\n",
            i - 1,
            i - 1
        ));
    }
    src.push_str(&format!("print(mk{N}() == mk{N}())\n"));
    let out = crate::vm::run_capture(&src).expect("well within the new cap, should run clean");
    assert_eq!(out, "true\n");
}

/// **Guard A's relation is DIRECTIONAL — the SHRINKING half must still be accepted.** An ordinary
/// generic struct instantiated with ITSELF as the type argument (`Pair[Pair[int]]`) makes the walk
/// re-enter the same nominal name (`Pair`) under a SMALLER instantiation one level down (`Pair[int]`,
/// walking `Pair[Pair[int]]`'s field `v: T`). That is not polymorphic recursion — it terminates in one
/// more step on its own — so growth detection must not refuse it. A guard built from a SYMMETRIC "the
/// args differ" test (the recorded failure mode, `docs/gaps.md` W7-42) would wrongly reject this: it
/// cannot tell shrinking from growing, only that the two instantiations are not equal.
#[test]
fn a_self_nested_generic_instantiation_shrinking_inward_is_accepted() {
    entry_ok(
        "\
struct Pair[T]:
    v: T
fn needs[U: Eq](a: U, b: U) -> bool:
    return a == b
fn use_it(a: Pair[Pair[int]], b: Pair[Pair[int]]) -> bool:
    return needs(a, b)
",
    );
    // Two levels of shrinking in a row — the neighbour a symmetric guard fails on FIRST, because it
    // trips at the very first "different args" comparison (`Pair[Pair[int]]` vs `Pair[Pair[Pair[int]]]`)
    // without ever asking which direction the difference runs.
    entry_ok(
        "\
struct Pair[T]:
    v: T
fn needs[U: Eq](a: U, b: U) -> bool:
    return a == b
fn use_it(a: Pair[Pair[Pair[int]]], b: Pair[Pair[Pair[int]]]) -> bool:
    return needs(a, b)
",
    );
}

/// **Guard A refuses growth immediately — not after walking to the depth cap.** Same polymorphic-
/// recursion shape as `conditional_eq_bound_recursion_terminates`, but timed: growth detection must
/// catch `N[int]` → `N[List[int]]` at depth ~2, so this stays fast and cheap REGARDLESS of how high
/// `EQ_BOUNDS_MAX_IN_PROGRESS` is set — the O(cap²)-memory failure mode this guard exists to prevent
/// (`docs/gaps.md` W7-55) only shows up if the walk is left to grow the `Ty` all the way to the cap.
#[test]
fn polymorphic_recursion_is_refused_in_bounded_time_by_growth_detection() {
    let (errs, entries) = eq_obligations_for(
        "\
struct N[T]:
    v: T
    next: N[List[T]]?
fn same[T: Eq](a: T, b: T) -> bool:
    return a == b
fn use_it(a: N[int], b: N[int]) -> bool:
    return same(a, b)
",
    );
    assert!(
        errs.iter().any(|e| e.message.contains("nests too deeply")),
        "expected an error containing \"nests too deeply\", got: {errs:?}"
    );
    assert!(
        entries <= 400,
        "growth must be refused by the node budget in bounded work: took {entries} obligations \
         (measured 314 on 2026-09-04)"
    );
}

/// **CRITICAL 1 (adversarial review of W7-55, same day): a nominal re-entering under a LARGER
/// instantiation is not always polymorphic recursion.** `Wrapper`'s field ignores its own type
/// parameter — it always nests `Wrapper[List[int]]`, a CONSTANT — so the graph is finite (rustc
/// accepts the mirror: `struct Wrapper<T> { v: T, ints: Option<Box<Wrapper<Vec<i32>>>> }` compiles)
/// even though `Wrapper[int]` → `Wrapper[List[int]]` is one genuine SIZE increase by
/// `Checker::ty_nodes`. A guard that refuses on the FIRST growing step cannot tell this from real
/// polymorphic recursion; it walks `Wrapper[List[int]]` → `Wrapper[List[int]]` next, an EXACT match
/// the ordinary cycle guard closes, so growth never repeats twice in a row here.
///
/// The verdict must not depend on the unrelated ROOT instantiation — before this fix, `Wrapper[int]`
/// was refused while `Wrapper[str]` (which never relates to `List[int]` by size either way) was
/// accepted for the same declaration, purely because of which root the caller happened to pick.
/// Asserted together so a regression that reintroduces that asymmetry trips both halves of the pair.
#[test]
fn a_growing_re_entry_through_a_constant_type_argument_is_accepted() {
    entry_ok(
        "\
struct Wrapper[T]:
    v: T
    ints: Wrapper[List[int]]?
fn same[U: Eq](a: U, b: U) -> bool:
    return a == b
fn use_it(a: Wrapper[int], b: Wrapper[int]) -> bool:
    return same(a, b)
",
    );
    entry_ok(
        "\
struct Wrapper[T]:
    v: T
    ints: Wrapper[List[int]]?
fn same[U: Eq](a: U, b: U) -> bool:
    return a == b
fn use_it(a: Wrapper[str], b: Wrapper[str]) -> bool:
    return same(a, b)
",
    );
}

/// **CRITICAL 2 (adversarial review of W7-55, same day): growth detection must see EVERY `Ty`
/// variant that can carry a growing child, not just the ones a containment check happened to list.**
/// The predecessor of `Checker::ty_nodes` (`ty_contains_or_eq`) had a `_ => false` default that
/// silently treated `Ty::Func` (a legal `Eq` type argument since W7-54, same branch) as a childless
/// leaf, so a type parameter substituted into a fn type grew INVISIBLY to Guard A and walked all the
/// way to the depth cap with an O(k)-sized `Ty` per level — the exact O(cap²) memory blow-up Guard A
/// exists to prevent, reintroduced through the one check that wasn't exhaustive. `ty_nodes` has no
/// catch-all arm (a new `Ty` variant is now a compile error here until sized), so this must refuse
/// just as fast as the `List[T]` shape.
#[test]
fn polymorphic_recursion_through_a_func_type_argument_is_refused_in_bounded_time() {
    // This assertion must come FIRST: a fail-open `ty_nodes` arm makes the walk below not terminate
    // (measured: killed at 900s, `signal: 15, SIGTERM`), so nothing placed after `eq_obligations_for`
    // can ever fire on that regression. Placed first, the same revert fails in 0.00s instead.
    assert_eq!(
        Checker::ty_nodes(&Ty::Func {
            params: vec![Ty::List(Box::new(Ty::Int))],
            ret: Box::new(Ty::Int),
            labels: FnLabels::default(),
        }),
        4,
        "ty_nodes must charge a Func's params and return type; a fail-open arm makes polymorphic \
         recursion through a fn type invisible to the budget"
    );
    let (errs, entries) = eq_obligations_for(
        "\
struct N[T]:
    v: T
    next: N[fn(T) -> int]?
fn same[T: Eq](a: T, b: T) -> bool:
    return a == b
fn use_it(a: N[int], b: N[int]) -> bool:
    return same(a, b)
",
    );
    assert!(
        errs.iter().any(|e| e.message.contains("nests too deeply")),
        "expected an error containing \"nests too deeply\", got: {errs:?}"
    );
    assert!(
        entries <= 300,
        "growth through a Func type argument must be refused by the node budget in bounded work: \
         took {entries} obligations (measured 223 on 2026-09-04)"
    );
}

/// **A memoized `None` must never escape the coinductive assumption it was derived under.** "Assume
/// `A: Eq` while proving `A: Eq`" is sound only while that proof is running. A descendant whose walk
/// came back clean *because of* that assumption is proven only conditionally — and if `A` then turns
/// out unsound by another branch, caching the descendant hands a later query a type that was assumed
/// sound and never proven. That is C4/C5 in a third disguise.
///
/// The shape, and it is exact: `A` reaches `M` (first field) and `Bad[Tag]` (second, genuinely
/// unsound); `M` reaches back to `A` through a `where T: Eq` bound. Asking `A` walks `M` first — `M`
/// comes back clean, but only via the assumption on `A` — and only THEN discovers `A`'s bad second
/// field. Asking `M` afterwards must still reject.
///
/// **Measured — this is the fixture, not a plausible-looking one.** `q2`'s error is the one that
/// disappears. The defence is the PER-QUERY RESET of `EQ_BOUNDS_PROVEN`: within one query the first
/// `Some` unwinds everything, so a conditional result can never be consulted after its assumption
/// was invalidated; across queries only the reset stops it. Remove the reset and `q2` is silently
/// granted — 1 type error instead of 2 — which is what this test catches.
///
/// An earlier build carried a second defence, a flag refusing to memoize any assumption-derived
/// result. It was measured redundant with the reset (removing either alone still rejected both
/// queries here) **and it disabled the memo on every cyclic type graph**, putting a 2^N walk back —
/// see `a_cyclic_shared_field_type_graph_is_also_walked_once_per_type`. It is gone; this test now
/// guards the single mechanism that actually carries the weight.
#[test]
fn a_memoized_eq_result_never_escapes_its_coinductive_assumption() {
    let errs = check_entry(
        "\
struct Tag:
    n: int
struct Bad[T]:
    val: T
    fn compare(self, other: Self) -> int where T: Comparable:
        return 0
    fn eq(self, other: Self) -> bool where T: Comparable:
        return self.compare(other) == 0
struct Cond[T]:
    v: T
    fn eq(self, other: Self) -> bool where T: Eq:
        return true
struct A:
    m: M
    bad: Bad[Tag]
struct M:
    back: Cond[A]
fn needs[U: Eq](p: U, q: U) -> bool:
    return p == q
fn q1(x: A, y: A) -> bool:
    return needs(x, y)
fn q2(x: M, y: M) -> bool:
    return needs(x, y)
",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("type A does not satisfy Eq")),
        "q1 must reject: A's second field is unsound. got: {errs:?}"
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("type M does not satisfy Eq")),
        "q2 must reject — M is clean only under the assumption that A is sound, and A is not. \
         A memo that cached that conditional result would grant it. got: {errs:?}"
    );
}

/// Struct/enum conformance through a DECLARED `eq` stays structural (the `Comparable` model): the
/// method must be the hook `eq(self, Self) -> bool`. A type WITHOUT one is now granted intrinsically
/// (D1) — its `==` is the structural derive — which is `struct_satisfies_eq_without_a_method` below.
#[test]
fn struct_with_eq_satisfies_eq() {
    entry_ok(
        "struct P:\n    x: int\n    fn eq(self, other: P) -> bool:\n        return self.x == other.x\nfn same[T: Eq](a: T, b: T) -> bool:\n    return a.eq(b)\nprint(same(P(1), P(2)))\n",
    );
    entry_ok(
        "struct Q:\n    x: int\nfn same[T: Eq](a: T, b: T) -> bool:\n    return a.eq(b)\nprint(same(Q(1), Q(2)))\n",
    );
    // The ORDINARY-METHOD escape hatch (`fn eq(self, x: T)`, a generic operand — not the hook, and
    // `==` leaves it structural) MUST now be granted `Eq` (C1, W7-53 follow-up review): before this,
    // `==`/`Map`/`Set` all treated such a type as structurally equal (measured, working), while
    // `[T: Eq]`/`.eq()` refused it — the exact two-spellings-disagree defect Tasks 1/2 closed for
    // `Ty::Func`/`Ty::BuiltinFn`/`Ty::Protocol`. `eq_sig_is_hook` now tells the escape hatch apart
    // from the hook BEFORE the D1 gate falls to the structural check, so a non-hook `eq` is judged
    // the same as no `eq` at all — granted here since `Opt2[int]`'s payload (`int`) is trivially
    // `Eq`. `a.eq(b)` dispatches by NAME to the escape-hatch method regardless (unchanged), so the
    // call itself still runs; only the BOUND obligation is what changed.
    entry_ok(
        "enum Opt2[T]:\n    Some(T)\n    Nothing\n    fn eq(self, x: T) -> bool:\n        return true\nfn same[T: Eq](a: T, b: T) -> bool:\n    return a.eq(b)\nprint(same(Opt2[int].Some(1), Opt2[int].Some(2)))\n",
    );
    // …but the grant above must NOT skip the escape hatch's own `where` bounds — C1' (the mirror of
    // C1, found by re-review). The first cut of the C1 fix short-circuited `eq_where_unsatisfied`
    // for a non-hook `eq` on the reasoning "`==` stays structural for it, so it cannot fault". That
    // is true of the OPERATOR and false of the BOUND: an erased `[T: Eq]` body's `a.eq(b)` still
    // dispatches BY NAME to the escape-hatch method, so its `where` clause runs unproven. Measured
    // on the release binary at that cut: BOTH programs below were `ok: no type errors` then
    // `runtime error: struct 'Tag' has no method 'compare'` — verbatim the
    // check-OK-then-fault class W7-41/W7-45/W7-53 exist to eliminate, re-opened while closing C1.
    //
    // So the walk now proves the bounds for ANY declared `eq`, and only the HOOK shape ends the
    // walk there; a non-hook `eq` falls through to the structural member walk, which is what its
    // `==` genuinely does. The `where`-less hatch above is unaffected — it has no bounds to prove.
    //
    // Both operand spellings that make an `eq` non-hook are covered, since they take different
    // routes through `eq_sig_is_hook`: a METHOD type param (`[U]`) and the OWNER's type param (`T`).
    let escape_hatch_with_a_bound = |operand: &str, method_params: &str| {
        format!(
            "struct Tag:\n    n: int\nstruct Box[T]:\n    val: T\n    \
             fn eq{method_params}(self, o: {operand}) -> bool where T: Comparable:\n        \
             return self.val.compare(self.val) == 0\n\
             fn eqm[T: Eq](a: T, b: T) -> bool:\n    return a.eq(b)\n\
             print(eqm(Box(Tag(1)), Box(Tag(2))))\n"
        )
    };
    entry_rejects(
        &escape_hatch_with_a_bound("U", "[U]"),
        "requires Tag: Comparable",
    );
    entry_rejects(
        &escape_hatch_with_a_bound("T", ""),
        "requires Tag: Comparable",
    );
    // The premise-derived `ok()` neighbour: the SAME shapes over a payload that DOES satisfy the
    // bound must still be accepted, or the rule above is an over-reject rather than a proof.
    entry_ok(
        "struct Box[T]:\n    val: T\n    fn eq[U](self, o: U) -> bool where T: Comparable:\n        return self.val.compare(self.val) == 0\nfn eqm[T: Eq](a: T, b: T) -> bool:\n    return a.eq(b)\nprint(eqm(Box(1), Box(2)))\n",
    );
}

/// TICKET-250 — an `eq` that does not satisfy `Eq`'s requirement (`(self, other: Self) -> bool`)
/// is an ordinary method, not a declaration error. The type is `Eq` structurally, and `a.eq(b)`
/// through a `[T: Eq]` bound is `==`, which never calls it.
#[test]
fn eq_wrong_return_type_is_an_ordinary_method() {
    entry_ok(
        "struct P:\n    x: int\n    fn eq(self, other: P) -> int:\n        return 0\nfn same[T: Eq](a: T, b: T) -> bool:\n    return a.eq(b)\nprint(same(P(1), P(2)))\n",
    );
    // The arity half: a one-parameter `eq` is the same class.
    entry_ok(
        "struct P:\n    x: int\n    fn eq(self) -> bool:\n        return true\nfn same[T: Eq](a: T, b: T) -> bool:\n    return a.eq(b)\nprint(same(P(1), P(2)))\n",
    );
}

/// TICKET-250 — every `eq` shape that is not the hook checks clean at the declaration, and `==`
/// on its type stays structural (the run-time half is `tests/hook_conformance_grid.rs`). One
/// decider, `Checker::hook_method`, says which `eq` is the hook; there is no `eq`-only shape rule.
#[test]
fn malformed_eq_is_an_ordinary_method() {
    // no operand at all — the headline silently-wrong case
    entry_ok(
        "struct A:\n    v: int\n    fn eq(self) -> bool:\n        return true\nprint(A(1) == A(2))\n",
    );
    // an EXTRA operand the dispatch would have filled with nil
    entry_ok(
        "struct A:\n    v: int\n    fn eq(self, o: A, extra: int) -> bool:\n        return true\nprint(A(1) == A(2))\n",
    );
    // a CONCRETE non-Self operand — the body was handed a struct where it declared an int
    entry_ok(
        "struct A:\n    v: int\n    fn eq(self, o: int) -> bool:\n        return o > 0\nprint(A(1) == A(2))\n",
    );
    // a non-bool return, now caught at the DECLARATION (it was only reachable via a `[T: Eq]` bound)
    entry_ok(
        "struct A:\n    v: int\n    fn eq(self, other: A) -> int:\n        return 1\nprint(A(1) == A(2))\n",
    );
    // the enum arm of the same rule
    entry_ok("enum E:\n    A\n    B\n\n    fn eq(self) -> bool:\n        return true\n");
    entry_ok("enum E:\n    A\n    B\n\n    fn eq(self, o: int) -> bool:\n        return true\n");
}

/// The BOUNDARY `malformed_eq_rejected_at_decl` must not cross. `eq` is NOT a reserved name — Rust
/// keeps it in `PartialEq` and still allows an inherent `eq`, Python namespaces the hook as `__eq__`,
/// Go has no overloading at all — so a method that merely shares the name stays legal. The separator
/// is the OPERAND: a type PARAMETER means "ordinary method" (the operator leaves it alone and stays
/// structural, asserted in `tests/chz/spec/eq_protocol_test.chz`), `Self` means "the hook".
#[test]
fn generic_operand_eq_stays_an_ordinary_method() {
    // the in-tree shape this rule exists to preserve (see `widen_generic_method_param_at_float_rejected`)
    entry_ok(
        "enum Opt[T]:\n    Some(T)\n    Nothing\n\n    fn eq(self, x: T) -> bool:\n        return true\n\no := Opt[int].Some(1)\nprint(o.eq(1))\n",
    );
    // a METHOD's own type param is equally not `Self`
    entry_ok(
        "struct H[T]:\n    v: T\n    fn eq[U](self, o: U) -> bool:\n        return true\nprint(H(1).eq(\"s\"))\n",
    );
    // …and every spelling of the hook itself stays legal: the bare name, `Self`, and a generic Self
    entry_ok(
        "struct P:\n    x: int\n    fn eq(self, other: P) -> bool:\n        return self.x == other.x\nprint(P(1) == P(2))\n",
    );
    entry_ok(
        "struct P:\n    x: int\n    fn eq(self, other: Self) -> bool:\n        return self.x == other.x\nprint(P(1) == P(2))\n",
    );
    entry_ok(
        "struct Box[T]:\n    v: T\n    fn eq(self, other: Box[T]) -> bool:\n        return true\nprint(Box(1) == Box(2))\n",
    );
    // a type ALIAS of the enclosing type is still the hook — the rule is the operand's TYPE, not its
    // spelling (the backend's syntactic twin dispatches it too: `alias` is not a type-param name)
    entry_ok(
        "struct Ver:\n    maj: int\n    fn eq(self, other: V2) -> bool:\n        return self.maj == other.maj\ntype V2 = Ver\nprint(Ver(1) == Ver(2))\n",
    );
}

/// Mirror of `generic_struct_heterogeneous_compare_rejected`: `Box[int] == Box[str]` must not
/// launder through `eq` — heterogeneous type args are provably-disjoint operands (B2).
#[test]
fn generic_struct_heterogeneous_eq_rejected() {
    let src = "\
struct Box[T]:
    v: T
    fn eq(self, other: Box[T]) -> bool:
        return true
b := Box(5) == Box(\"hello\")
";
    entry_rejects(src, "cannot compare Box[int] and Box[str] for equality");
}

/// **B2** (`docs/gaps.md`) — `==`/`!=` between provably-disjoint types is rejected at check time.
/// Python answers `False` at runtime; a statically-typed language rejects it (mypy
/// `--strict-equality`, Go/Rust compile errors), so this is a deliberate, documented divergence.
#[test]
fn disjoint_types_equality_rejected() {
    entry_rejects(
        "fn main():\n    print(1 == \"a\")\nmain()\n",
        "cannot compare int and str for equality",
    );
    entry_rejects(
        "fn main():\n    print([1] != Set([1]))\nmain()\n",
        "cannot compare",
    );
    entry_rejects(
        "struct A:\n    x: int\nstruct B:\n    x: int\nfn main():\n    print(A(1) == B(1))\nmain()\n",
        "cannot compare",
    );
    entry_rejects(
        "fn main():\n    o: int? = 1\n    print(o == 1)\nmain()\n",
        "cannot compare",
    );
    // The two shapes the shipped examples used to spell directly (`examples/empty_struct.chz`,
    // `examples/enum_layout.chz`) — both now route through `Any`, and BOTH must stay errors when
    // spelled disjointly, or B2 has stopped rejecting what it exists to reject.
    entry_rejects(
        "struct S:\n    pass\nstruct T:\n    pass\nfn main():\n    print(S() == T())\nmain()\n",
        "cannot compare S and T for equality",
    );
    entry_rejects(
        "enum Shadow:\n    A\n    B\nfn main():\n    o: int? = ?1\n    print(o == Shadow.A)\nmain()\n",
        "cannot compare int? and Shadow for equality",
    );
    // A protocol existential is NOT a free pass: a concrete that does NOT conform stays disjoint.
    entry_rejects(
        "protocol Shape:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int:\n        return self.s\nfn main():\n    sh: Shape = Sq(2)\n    print(sh == \"a\")\nmain()\n",
        "cannot compare Shape and str for equality",
    );
    // …and it is not a free pass NESTED either: `may_be_equal` recurses a container's type args
    // CO-variantly, which is not the same as waving them through. A non-conforming element keeps
    // the pair provably disjoint one and two levels down.
    entry_rejects(
        "protocol Shape:\n    fn area(self) -> int\nfn cmp(a: List[Shape], b: List[str]) -> bool:\n    return a == b\nfn main():\n    pass\nmain()\n",
        "cannot compare List[Shape] and List[str] for equality",
    );
    entry_rejects(
        "fn cmp(a: int?, b: str?) -> bool:\n    return a == b\nfn main():\n    pass\nmain()\n",
        "cannot compare int? and str? for equality",
    );
    entry_rejects(
        "fn cmp(a: Map[str, int], b: Map[str, bool]) -> bool:\n    return a == b\nfn main():\n    pass\nmain()\n",
        "cannot compare Map[str, int] and Map[str, bool] for equality",
    );
    // A `where T: <scalar>` bound is an EQUALITY constraint, so `T` is exactly `str` — the param is
    // NOT erased and the pair is provable after all (the diagnostic names the pinned scalar).
    entry_rejects(
        "fn f[T](a: T, b: int) -> bool where T: str:\n    return a == b\nfn main():\n    print(f(\"a\", 1))\nmain()\n",
        "cannot compare str and int for equality",
    );
    // The native generic HANDLES joined the co-variant recursion (see the `_ok` twin) — recursing is
    // not waving through: two handles whose element types are disjoint are still disjoint handles.
    entry_rejects(
        "import std.concurrency\nfn cmp(a: Channel[int], b: Channel[str]) -> bool:\n    return a == b\nfn main():\n    pass\nmain()\n",
        "cannot compare Channel[int] and Channel[str] for equality",
    );
    entry_rejects(
        "import std.concurrency\nfn cmp(a: Shared[int], b: Shared[str]) -> bool:\n    return a == b\nfn main():\n    pass\nmain()\n",
        "cannot compare Shared[int] and Shared[str] for equality",
    );
    // A PARAMETERIZED protocol erases only its ARGS — the method SET is still decided, so a concrete
    // that cannot conform however the args are chosen stays provably disjoint.
    entry_rejects(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nfn cmp[T](a: Container[T], b: int) -> bool:\n    return a == b\nfn main():\n    pass\nmain()\n",
        "cannot compare Container[T] and int for equality",
    );
}

/// The B2 rejection's PREMISE is "provably disjoint" — every pair that can legitimately be equal must
/// still be accepted. These are the neighbours the rule must not eat (numeric widening, `Unknown`
/// silencing, container/`Option`/protocol-existential pairs, same struct/enum).
#[test]
fn comparable_types_equality_still_ok() {
    entry_ok(
        "fn main():\n    print(1 == 1.0)\n    print(2.5 != 2)\n    print(\"a\" == \"b\")\n    print(true == false)\n    print([1] == [2])\n    print({\"a\": 1} == {})\n    print(Set([1]) == Set([2]))\n    print((1, \"a\") == (2, \"b\"))\nmain()\n",
    );
    entry_ok(
        "fn main():\n    o: int? = ?1\n    print(o == None)\n    print(o == ?2)\n    r: int! = ?1\n    print(r == ?2)\nmain()\n",
    );
    entry_ok(
        "struct P:\n    x: int\nenum C:\n    Red\n    Blue\nfn main():\n    print(P(1) == P(2))\n    print(C.Red != C.Blue)\nmain()\n",
    );
    // A `str` IS an `Error` existential (the intrinsic Go-style conformance), in either operand order.
    entry_ok(
        "fn boom() -> int!:\n    return !\"nope\"\nfn main():\n    match boom():\n        ?v: print(v)\n        !e: print(e == \"nope\")\nmain()\n",
    );
    // A generic body comparing two values of its own type param must bound it by Eq (W7-53) — rustc/Go
    // both reject the unbounded form at its OWN definition; see
    // `a_generic_body_that_compares_must_bound_its_type_param_by_eq`.
    entry_ok("fn same[T: Eq](a: T, b: T) -> bool:\n    return a == b\nprint(same(1, 2))\n");
    // ----- the shapes `compatible` alone CANNOT see (it is registry-blind), i.e. everything that
    // is NOT provably disjoint because one side is a SUPERTYPE of the other or is erased. Each of
    // these ran and printed `true` before B2 landed; all five regressed on the first cut.
    // (1) a protocol existential vs a conforming concrete, in the existential's own direction.
    entry_ok(
        "protocol Shape:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int:\n        return self.s * self.s\nfn main():\n    sh: Shape = Sq(2)\n    print(sh == Sq(2))\n    print(Sq(2) == sh)\nmain()\n",
    );
    // (2) the `Error` existential vs a conforming user error struct (not just the `str` special case).
    entry_ok(
        "struct MyErr:\n    m: str\n    fn message(self) -> str:\n        return self.m\nfn boom() -> int!:\n    return !MyErr(\"x\")\nfn main():\n    match boom():\n        ?v: print(v)\n        !e: print(e == MyErr(\"x\"))\nmain()\n",
    );
    // (3) `Any` is the TOP type — the documented escape hatch, and it must work with only ONE side
    // widened (the docs spell it `u: Any = a`).
    entry_ok("fn main():\n    u: Any = 1\n    print(u == 1)\n    print(1 == u)\nmain()\n");
    // (4) a `Ty::Param` vs a concrete: `may_be_equal`'s co-inhabitance erasure still tolerates the
    // mismatched shape (that axis is unaffected by W7-53), but the WALK now requires `T: Eq`, same as
    // every other free-param comparison — so this needs the bound where it once needed none.
    entry_ok(
        "fn f[T: Eq](a: T) -> bool:\n    return a == 1\nfn main():\n    print(f(1))\nmain()\n",
    );
    // (5) a NESTED existential — the recursion `compatible` cannot do.
    entry_ok(
        "struct MyErr:\n    m: str\n    fn message(self) -> str:\n        return self.m\nfn main():\n    o: Error? = ?MyErr(\"x\")\n    p: MyErr? = ?MyErr(\"x\")\n    print(o == p)\nmain()\n",
    );
    // ----- and the shapes `assignable` cannot see either (it answers the STORAGE question):
    // (6) TWO existentials over ONE concrete. `Sq` has both `area` and `message`, so it is
    // simultaneously a `Shape` and an `Error` — the pair is inhabited even though neither protocol
    // embeds the other, which is all `assignable`'s `(Protocol, Protocol)` path can see.
    entry_ok(
        "protocol Shape:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int:\n        return self.s\n    fn message(self) -> str:\n        return \"sq\"\nfn main():\n    sh: Shape = Sq(2)\n    e: Error = Sq(2)\n    print(sh == e)\n    print(e == sh)\nmain()\n",
    );
    // (7) an existential nested inside a MUTABLE container / generic struct. Their type args are
    // compared INVARIANTLY for assignability (a `G[Sub]` aliased as `G[Super]` can be written
    // through) — a soundness rule for STORAGE. `==` never writes, so the same list/box/tuple really
    // can inhabit both types and the comparison must compile.
    entry_ok(
        "struct MyErr:\n    m: str\n    fn message(self) -> str:\n        return self.m\nstruct Box[T]:\n    v: T\nfn boxes(a: Box[Error], b: Box[MyErr]) -> bool:\n    return a == b\nfn maps(a: Map[str, Error], b: Map[str, MyErr]) -> bool:\n    return a == b\nfn main():\n    o: List[Error] = [MyErr(\"x\")]\n    p: List[MyErr] = [MyErr(\"x\")]\n    print(o == p)\n    t: (Error, int) = (MyErr(\"x\"), 1)\n    u: (MyErr, int) = (MyErr(\"x\"), 1)\n    print(t == u)\nmain()\n",
    );
    // (8) same as (4), one level down (`List[T]`): the co-inhabitance erasure is unaffected, the
    // walk still needs `T: Eq`.
    entry_ok(
        "fn f[T: Eq](a: List[T]) -> bool:\n    return a == [1]\nfn main():\n    print(f([1]))\nmain()\n",
    );
    // (9) the runtime's own CROSS-TYPE equality arms compose through the recursion, because they
    // are arms of `may_be_equal` rather than a top-level special case: `[1.0] == [1]` and
    // `{"k": 1.0} == {"k": 1}` both print `true` (CPython agrees), so rejecting
    // them would contradict "only a PROVABLY disjoint pair".
    entry_ok(
        "fn main():\n    a: List[float] = [1.0]\n    b: List[int] = [1]\n    print(a == b)\n    m: Map[str, float] = {\"k\": 1.0}\n    n: Map[str, int] = {\"k\": 1}\n    print(m == n)\n    t: (float, int) = (1.0, 2)\n    s: (int, int) = (1, 2)\n    print(t == s)\nmain()\n",
    );
    // (10) the native generic HANDLES (`Channel`/`Shared`/`RwShared`/`Atomic`). Their `==` is the
    // identity shortcut at the top of `values_equal_guarded` (`cmp(ch, ch)` is `true`, two distinct
    // channels `false`) — live, working code. They had no `may_be_equal` arm and fell through to
    // `compatible`, which is neither `Param`-tolerant nor conformance-aware, so BOTH the erased-param
    // escape (8) and the existential-nesting case (7) stopped one constructor short.
    entry_ok(
        "import std.concurrency\nfn chans[T](a: Channel[T], b: Channel[int]) -> bool:\n    return a == b\nfn atoms[T: StructuralEq](a: Atomic[T], b: Atomic[int]) -> bool:\n    return a == b\nfn rws[T](a: RwShared[T], b: RwShared[int]) -> bool:\n    return a == b\nfn main():\n    pass\nmain()\n",
    );
    entry_ok(
        "import std.concurrency\nstruct MyErr:\n    m: str\n    fn message(self) -> str:\n        return self.m\nfn shr(a: Shared[Error], b: Shared[MyErr]) -> bool:\n    return a == b\nfn nested(a: List[Shared[Error]], b: List[Shared[MyErr]]) -> bool:\n    return a == b\nfn main():\n    pass\nmain()\n",
    );
    // (11) a PARAMETERIZED protocol existential vs a conforming concrete, with a free `T` in the
    // protocol's args — the erasure escape did not reach into `Protocol`'s own `pargs` either, so the
    // args (not the method set) were deciding a question they cannot answer inside an erased body.
    // `cmp`'s `a: Container[T]` is a PROTOCOL existential — `eq_bounds_unsatisfied_rec` never recurses
    // into a protocol's args (the catch-all `_ => None`, deferred to the witness), so `T` there needs
    // no bound. `flip`'s `a: Bag[T]` is a CONCRETE struct with a bare `T` FIELD that the walk reaches
    // directly, so (W7-53) it needs `T: Eq`.
    entry_ok(
        "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag[T]:\n    item: T\n    fn get(self, i: int) -> T:\n        return self.item\nfn cmp[T](a: Container[T], b: Bag[int]) -> bool:\n    return a == b\nfn flip[T: Eq](a: Bag[T], b: Container[int]) -> bool:\n    return a == b\nfn main():\n    pass\nmain()\n",
    );
}

/// **Decision 5** — a type with a user `eq` is rejected as an `Atomic[T]` payload: `Atomic.cas`
/// compares the stored value with the runtime's STRUCTURAL equality, which is not the user's `eq`, so
/// admitting it would silently answer a different question than `==` does.
#[test]
fn atomic_payload_with_eq_rejected() {
    let decl = "import std.concurrency\nstruct P:\n    x: int\n    fn eq(self, other: P) -> bool:\n        return true\n";
    entry_rejects(
        &format!("{decl}fn main():\n    a := Atomic(P(1))\n    print(a.load().x)\nmain()\n"),
        "Atomic[P] payload",
    );
    entry_rejects(
        &format!(
            "{decl}fn hold(a: Atomic[P]):\n    print(a.load().x)\nfn main():\n    pass\nmain()\n"
        ),
        "Atomic[P] payload",
    );
    // BOUNDARY: the payloads that DON'T define `eq` stay legal — scalars (whose `Eq` is intrinsic and
    // IS structural equality) and a struct without an `eq` method.
    entry_ok(
        "import std.concurrency\nstruct Q:\n    x: int\nfn main():\n    a := Atomic(1)\n    b := Atomic(\"s\")\n    c := Atomic(Q(1))\n    print(a.load() + c.load().x)\n    print(b.load())\nmain()\n",
    );
}

/// TICKET-144 (W14-24): `Atomic.cas` on a payload that holds a `fn` value can never succeed — every
/// `load()` returns a fresh copy of the closure and closures compare by identity — so the standard
/// `cas(a.load(), new)` retry loop spins forever. Go's `atomic.Value.CompareAndSwap` panics
/// `comparing uncomparable type` on the same shape. Rejected at the `cas` CALL (`load`/`store`/
/// `exchange` of a fn stay legal, as Go's `Value` Store/Load of a func do); a builtin fn value
/// (`ord`) compares equal after a load and stays allowed.
#[test]
fn atomic_cas_on_fn_payload_rejected() {
    let decl = "import std.concurrency\nfn one() -> int:\n    return 1\nstruct H:\n    n: int\n    f: fn() -> int\n";
    let needle = "cas cannot compare a payload that holds a function value";
    entry_rejects(
        &format!(
            "{decl}fn main():\n    a := Atomic[fn() -> int](one)\n    print(a.cas(a.load(), one))\nmain()\n"
        ),
        needle,
    );
    entry_rejects(
        &format!(
            "{decl}fn main():\n    a := Atomic(H(1, one))\n    print(a.cas(a.load(), H(2, one)))\nmain()\n"
        ),
        needle,
    );
    // The fn nested in a container/option is the same payload.
    entry_rejects(
        &format!(
            "{decl}fn main():\n    a := Atomic([one])\n    print(a.cas(a.load(), [one]))\nmain()\n"
        ),
        needle,
    );
    // A std native fn (`math.sqrt`) types as `fn(float) -> float` and loads back UNEQUAL to itself
    // (measured on the base binary: `Atomic(math.sqrt).cas(a.load(), math.sqrt)` is `false`), so it
    // is the same bug and is rejected too.
    entry_rejects(
        "import std.concurrency\nimport std.math\nfn main():\n    a := Atomic(math.sqrt)\n    print(a.cas(a.load(), math.sqrt))\nmain()\n",
        needle,
    );
    // MUST STILL PASS: a fn payload with only `store`/`load`/`exchange`, and a builtin fn with `cas`.
    entry_ok(&format!(
        "{decl}fn main():\n    a := Atomic[fn() -> int](one)\n    a.store(one)\n    f := a.exchange(one)\n    print(f() + a.load()())\nmain()\n"
    ));
    entry_ok(
        "import std.concurrency\nfn main():\n    a := Atomic(ord)\n    print(a.cas(a.load(), ord))\nmain()\n",
    );
    // A payload with no fn in it keeps `cas`.
    entry_ok(&format!(
        "{decl}struct Q:\n    x: int\nfn main():\n    a := Atomic(Q(1))\n    print(a.cas(a.load(), Q(2)))\nmain()\n"
    ));
}

/// The gate must see through EVERY type structural equality recurses into, not just the payload's
/// own methods. `cas` compares `Atomic[List[P]]` element-by-element, so it reaches `P`'s `eq` on
/// exactly the compare a bare `Atomic[P]` does — and the `_ => false` arm this replaces let the
/// container, `Option`, tuple, struct-field and enum-payload spellings all walk
/// straight past a gate that was the SOLE stated safety argument for holding the value mutex across
/// the compare (`vm/netio.rs`). M23 adversarial review, CRITICAL 2.
#[test]
fn atomic_payload_reaching_a_nested_eq_rejected() {
    let decl = "import std.concurrency\nstruct P:\n    x: int\n    fn eq(self, other: P) -> bool:\n        return true\nstruct W:\n    p: P\nenum E:\n    Has(P)\n    Nope\n";
    for (payload, rendered) in [
        ("[P(1)]", "Atomic[List[P]]"),
        ("[[P(1)]]", "Atomic[List[List[P]]]"),
        ("(1, P(1))", "Atomic[(int, P)]"),
        ("{1: P(1)}", "Atomic[Map[int, P]]"),
        ("{P(1)}", "Atomic[Set[P]]"),
        ("W(P(1))", "Atomic[W]"),
        ("E.Has(P(1))", "Atomic[E]"),
    ] {
        entry_rejects(
            &format!("{decl}fn main():\n    a := Atomic({payload})\n    print(a.load())\nmain()\n"),
            rendered,
        );
    }
    // BOUNDARY: the same shapes over an `eq`-free element stay legal — the walk must reject on
    // REACHING an `eq`, not on being a container. A self-referential struct must also terminate.
    entry_ok(
        "import std.concurrency\nstruct Q:\n    x: int\nstruct Node:\n    v: int\n    next: List[Node]\nfn main():\n    a := Atomic([1, 2])\n    b := Atomic({\"k\": [Q(1)]})\n    c := Atomic((1, \"x\"))\n    d := Atomic(Node(1, []))\n    print(a.load().len() + b.load().len() + d.load().v)\n    print(c.load())\nmain()\n",
    );
}

// ----- transparent type aliases (M10-G3) -----

#[test]
fn type_alias_transparent_ok() {
    // UserId ≡ int: usable interchangeably in annotations and calls.
    ok(
        "type UserId = int\nfn double(n: int) -> int:\n    return n * 2\nid: UserId = 5\nx: int = id\ny := double(id)\n",
    );
}

#[test]
fn type_alias_mismatch_still_rejected() {
    // The alias is transparent, so a str where the underlying int is expected is still an error
    // (and the message names the resolved type, `int`).
    rejects(
        "type UserId = int\nid: UserId = \"no\"\n",
        "cannot assign str to variable of type int",
    );
}

#[test]
fn type_alias_to_collection_ok() {
    ok("type Scores = Map[str, int]\ns: Scores = {\"a\": 1}\nn: int = s[\"a\"]\n");
}

#[test]
fn recursive_type_alias_rejected() {
    rejects("type A = B\ntype B = A\nx: A = 1\n", "recursive type alias");
}

#[test]
fn type_alias_redeclared_rejected() {
    rejects("type T1 = int\ntype T1 = str\n", "already defined");
}

// ----- generic structs (G2) -----

const PAIR: &str = "\
struct Pair[A, B]:
    first: A
    second: B
    fn left(self) -> A:
        return self.first
";

#[test]
fn generic_struct_field_type_substituted() {
    // first is A=int; assigning it to a str binding must be rejected.
    rejects(
        &format!("{PAIR}p := Pair(1, \"x\")\nn: str = p.first\n"),
        "cannot assign int",
    );
}

#[test]
fn generic_struct_construction_and_field_ok() {
    ok(&format!(
        "{PAIR}p := Pair(1, \"x\")\nn: int = p.first\ns: str = p.second\n"
    ));
}

#[test]
fn generic_struct_method_return_substituted() {
    ok(&format!("{PAIR}p := Pair(7, \"x\")\nn: int = p.left()\n"));
    rejects(
        &format!("{PAIR}p := Pair(7, \"x\")\nn: str = p.left()\n"),
        "cannot assign int",
    );
}

#[test]
fn generic_struct_explicit_type_args_ok() {
    ok(&format!("{PAIR}p: Pair[str, int] = Pair(\"k\", 9)\n"));
}

#[test]
fn generic_struct_wrong_arity_rejected() {
    rejects(
        &format!("{PAIR}p: Pair[int] = Pair(1, 2)\n"),
        "expects 2 type argument(s)",
    );
}

#[test]
fn generic_struct_method_arg_checked_against_type_arg() {
    let src = "\
struct Box[T]:
    val: T
    fn set(self, x: T) -> T:
        return x
b := Box(5)
y := b.set(\"nope\")
";
    rejects(src, "expected int");
}

#[test]
fn generic_struct_method_arg_ok() {
    let src = "\
struct Box[T]:
    val: T
    fn set(self, x: T) -> T:
        return x
b := Box(5)
y := b.set(9)
";
    ok(src);
}

// ----- method-level type parameters (F1) -----

// A method introducing its own fresh `[U]` beyond the struct's `[T]`.
const BOXMAP: &str = "\
struct Box[T]:
    v: T
    fn map_to[U](self, f: fn(T) -> U) -> U:
        return f(self.v)
";

#[test]
fn method_type_param_inferred_from_closure_ok() {
    // U is inferred from the closure's return type (str); the call type-checks and yields str.
    ok(&format!(
        "{BOXMAP}b := Box(5)\ns: str = b.map_to(fn(x: int) -> str: \"n{{x}}\")\n"
    ));
}

#[test]
fn method_type_param_result_type_substituted() {
    // U=str, so binding the result to an int must be rejected (proves U flowed into the return).
    rejects(
        &format!("{BOXMAP}b := Box(5)\nn: int = b.map_to(fn(x: int) -> str: \"n{{x}}\")\n"),
        "cannot assign str",
    );
}

#[test]
fn method_type_param_bound_satisfied_ok() {
    let src = "\
struct Box[T]:
    v: T
    fn biggest[U: Comparable](self, a: U, b: U) -> U:
        if a < b:
            return b
        return a
b := Box(0)
r: int = b.biggest(3, 7)
";
    ok(src);
}

#[test]
fn method_type_param_bound_enforced() {
    // U: Comparable, but Plain isn't Comparable — the method bound must be enforced at the call.
    let src = "\
struct Plain:
    n: int
struct Box[T]:
    v: T
    fn biggest[U: Comparable](self, a: U, b: U) -> U:
        if a < b:
            return b
        return a
b := Box(0)
r := b.biggest(Plain(1), Plain(2))
";
    rejects(
        src,
        "does not satisfy Comparable (missing method 'compare')",
    );
}

#[test]
fn method_type_param_arity_checked() {
    let src = "\
struct Box[T]:
    v: T
    fn one[U](self, a: U) -> U:
        return a
b := Box(0)
r := b.one(1, 2)
";
    rejects(src, "expects");
}

#[test]
fn method_type_param_shadowing_struct_param_rejected() {
    // A method type param reusing the struct's `T` is a confusing double-binding — reject it.
    let src = "\
struct Box[T]:
    v: T
    fn weird[T](self, x: T) -> T:
        return x
";
    rejects(src, "shadows");
}

// ----- user-defined parameterized protocols (F3, concrete-arg bounds) -----

// A user protocol with its own type param `T`, plus a struct conforming at `Container[int]`.
const CONTAINER: &str = "\
protocol Container[T]:
    fn get(self, i: int) -> T
    fn put(self, x: T)
struct IntBox:
    items: List[int]
    fn get(self, i: int) -> int:
        return self.items[i]
    fn put(self, x: int):
        self.items.push(x)
";

#[test]
fn param_protocol_decl_ok() {
    ok("protocol Container[T]:\n    fn get(self, i: int) -> T\n");
}

#[test]
fn param_protocol_bound_conformance_ok() {
    // IntBox satisfies Container[int] (T substituted to int), so first(b) type-checks.
    let src = format!(
        "{CONTAINER}fn first[X: Container[int]](c: X) -> int:\n    return c.get(0)\nb := IntBox([10, 20])\nn: int = first(b)\n"
    );
    ok(&src);
}

#[test]
fn param_protocol_body_return_substituted() {
    // Inside `first`, `c.get(0)` is `T` = int (from the bound's arg). Returning it as str is wrong.
    let src =
        format!("{CONTAINER}fn first[X: Container[int]](c: X) -> str:\n    return c.get(0)\n");
    rejects(&src, "int");
}

#[test]
fn param_protocol_bound_arity_mismatch_rejected() {
    // Container takes one type argument; a bare `Container` bound is an arity error.
    let src = format!("{CONTAINER}fn first[X: Container](c: X) -> int:\n    return c.get(0)\n");
    rejects(&src, "type argument");
}

#[test]
fn param_protocol_missing_method_rejected() {
    // A struct missing `put` does not satisfy Container[int].
    let src = "\
protocol Container[T]:
    fn get(self, i: int) -> T
    fn put(self, x: T)
struct Half:
    items: List[int]
    fn get(self, i: int) -> int:
        return self.items[i]
fn first[X: Container[int]](c: X) -> int:
    return c.get(0)
b := Half([1])
n := first(b)
";
    rejects(src, "does not satisfy Container");
}

#[test]
fn param_protocol_wrong_substituted_signature_rejected() {
    // `get` returns str, but Container[int] requires it return int.
    let src = "\
protocol Container[T]:
    fn get(self, i: int) -> T
struct StrBox:
    items: List[str]
    fn get(self, i: int) -> str:
        return self.items[i]
fn first[X: Container[int]](c: X) -> int:
    return 0
b := StrBox([\"a\"])
n := first(b)
";
    rejects(src, "does not satisfy Container");
}

#[test]
fn param_protocol_forwarding_wrong_arg_rejected() {
    // Review-panel CRITICAL: forwarding a `Container[str]` value into a `Container[int]` bound must
    // be rejected — the bound's type args have to match, not just the protocol name.
    let src = "\
protocol Container[T]:
    fn get(self, i: int) -> T
    fn size(self) -> int
struct StrBox:
    items: List[str]
    fn get(self, i: int) -> str:
        return self.items[i]
    fn size(self) -> int:
        return self.items.len()
fn total[X: Container[int]](c: X) -> int:
    return c.size()
fn forward[Y: Container[str]](c: Y) -> int:
    return total(c)
n := forward(StrBox([\"a\"]))
";
    rejects(src, "does not satisfy Container");
}

#[test]
fn param_protocol_forwarding_matching_arg_ok() {
    // The same forward with matching args (`Container[int]` → `Container[int]`) must still be accepted.
    let src = "\
protocol Container[T]:
    fn get(self, i: int) -> T
    fn size(self) -> int
struct IntBox:
    items: List[int]
    fn get(self, i: int) -> int:
        return self.items[i]
    fn size(self) -> int:
        return self.items.len()
fn total[X: Container[int]](c: X) -> int:
    return c.size()
fn forward[Y: Container[int]](c: Y) -> int:
    return total(c)
n := forward(IntBox([1]))
";
    ok(src);
}

#[test]
fn generic_method_without_receiver_rejected() {
    // Review-panel IMPORTANT: a method whose first param isn't a receiver must NOT silently bind the
    // receiver to the first declared param. Under the "no self ⇒ static" rule, a no-`self` method is
    // a STATIC method — calling it on an INSTANCE (`b.ident()`) is rejected (it is reached only as
    // `Box.ident(...)`). (`ident` here also declares its own `[U]`, but the instance-call rejection
    // fires first.)
    let src = "\
struct Box[T]:
    v: T
    fn ident[U]() -> U:
        pass
b := Box(5)
r := b.ident()
";
    rejects(src, "is a static method");
}

#[test]
fn param_protocol_as_value_type_accepted() {
    // A parameterized protocol is now a first-class value/annotation type (Q1). The carried arg is
    // witnessed at every store/pass boundary; a method returning the protocol's param recovers to the
    // carried concrete type (`c.get(0)` on a `Container[int]` yields `int`).
    let src = "\
protocol Container[T]:
    fn get(self, i: int) -> T
fn good(c: Container[int]) -> int:
    return c.get(0)
";
    ok(src);
}

// ----- review-panel regressions (G1/G2) -----

#[test]
fn bounded_type_param_forwards_to_bounded_call_ok() {
    // Review #1: a `T: Comparable` value must satisfy Comparable when forwarded to another
    // `[U: Comparable]` call (generic composition).
    let src = "\
fn max[T: Comparable](a: T, b: T) -> T:
    if a < b:
        return b
    return a
fn pick[T: Comparable](a: T, b: T) -> T:
    return max(a, b)
print(pick(3, 7))
";
    ok(src);
}

#[test]
fn generic_struct_bound_enforced_at_construction() {
    // Review C1: a struct type-param bound must be enforced, not just on generic fns.
    let src = "\
struct Plain:
    n: int
struct Box[T: Comparable]:
    a: T
b := Box(Plain(1))
";
    rejects(
        src,
        "does not satisfy Comparable (missing method 'compare')",
    );
}

#[test]
fn generic_struct_bound_enforced_on_explicit_type_arg() {
    let src = "\
struct Plain:
    n: int
struct Box[T: Comparable]:
    a: T
b: Box[Plain] = Box(Plain(1))
";
    rejects(
        src,
        "does not satisfy Comparable (missing method 'compare')",
    );
}

#[test]
fn generic_struct_bound_satisfied_ok() {
    let src = "\
struct Box[T: Comparable]:
    a: T
b := Box(5)
c: Box[str] = Box(\"hi\")
";
    ok(src);
}

#[test]
fn type_param_shadows_same_named_struct() {
    // Review I2: an in-scope type parameter shadows a same-named type.
    let src = "\
struct T:
    n: int
fn id[T](x: T) -> T:
    return x
y := id(5)
";
    ok(src);
}

#[test]
fn bare_generic_struct_without_args_rejected() {
    // Review I3: using a generic struct as a type without its arguments is an error.
    let src = "\
struct Box[T]:
    v: T
fn unwrap(b: Box) -> int:
    return 0
";
    rejects(src, "expects 1 type argument(s), got 0");
}

// ----- generic enums (type-erased) -----

/// A generic binary tree, the workhorse fixture for the generic-enum tests.
const TREE: &str = "\
enum Tree[T]:
    Leaf
    Node(T, Tree[T], Tree[T])
";

#[test]
fn generic_enum_construction_infers_type_arg_ok() {
    // Node(1, Leaf, Leaf) infers T=int; the value flows into a `Tree[int]` slot.
    ok(&format!(
        "{TREE}t: Tree[int] = Tree.Node(1, Tree.Leaf, Tree.Leaf)\n"
    ));
}

#[test]
fn generic_enum_construction_type_mismatch_rejected() {
    // First payload is T; an int and a str in the two Node arms can't both be T.
    rejects(
        &format!("{TREE}t := Tree.Node(1, Tree.Node(\"x\", Tree.Leaf, Tree.Leaf), Tree.Leaf)\n"),
        "expected",
    );
}

#[test]
fn generic_enum_annotation_arg_mismatch_rejected() {
    // A Tree[str] slot can't hold a Node whose payload infers T=int.
    rejects(
        &format!("{TREE}t: Tree[str] = Tree.Node(1, Tree.Leaf, Tree.Leaf)\n"),
        "cannot assign",
    );
}

#[test]
fn generic_enum_match_substitutes_payload_ok() {
    // The `v` bound by `Node(v, ...)` of a `Tree[int]` is int.
    let src = format!(
        "{TREE}fn first(t: Tree[int]) -> int:\n    match t:\n        Tree.Leaf: return 0\n        Tree.Node(v, l, r): return v\n"
    );
    ok(&src);
}

#[test]
fn generic_enum_match_payload_type_enforced() {
    // The `v` bound by `Node(v, ...)` of a `Tree[int]` is int, not str.
    let src = format!(
        "{TREE}fn bad(t: Tree[int]):\n    match t:\n        Tree.Leaf: print(\"l\")\n        Tree.Node(v, l, r):\n            s: str = v\n"
    );
    rejects(&src, "cannot assign int");
}

#[test]
fn generic_enum_wrong_arity_rejected() {
    rejects(
        &format!("{TREE}t: Tree[int, str] = Tree.Leaf\n"),
        "expects 1 type argument(s)",
    );
}

#[test]
fn bare_generic_enum_without_args_rejected() {
    rejects(
        &format!("{TREE}fn f(t: Tree) -> int:\n    return 0\n"),
        "expects 1 type argument(s), got 0",
    );
}

#[test]
fn generic_enum_multi_param_ok() {
    let src = "\
enum Either[A, B]:
    Left(A)
    Right(B)
fn fst(e: Either[int, str]) -> int:
    match e:
        Either.Left(a): return a
        Either.Right(b): return 0
";
    ok(src);
}

#[test]
fn generic_enum_nested_self_referential_ok() {
    // Cons holds T and a nested LinkedList[T]? — the payload references the enum's own param.
    let src = "\
enum LinkedList[T]:
    Nil
    Cons(T, LinkedList[T]?)
fn len(l: LinkedList[int]) -> int:
    match l:
        LinkedList.Nil: return 0
        LinkedList.Cons(h, t): return 1
";
    ok(src);
}

#[test]
fn generic_enum_bound_enforced_at_construction() {
    let src = "\
struct Plain:
    n: int
enum Box[T: Comparable]:
    Empty
    Has(T)
b := Box.Has(Plain(1))
";
    rejects(
        src,
        "does not satisfy Comparable (missing method 'compare')",
    );
}

#[test]
fn generic_enum_bound_satisfied_ok() {
    let src = "\
enum Box[T: Comparable]:
    Empty
    Has(T)
b: Box[int] = Box.Has(5)
";
    ok(src);
}

#[test]
fn generic_enum_unknown_bound_rejected() {
    rejects(
        "enum Box[T: Nope]:\n    Has(T)\n",
        "unknown protocol 'Nope'",
    );
}

#[test]
fn struct_and_enum_sharing_a_name_rejected() {
    // Review (Solidity lens): a struct and enum with the same name both registered silently,
    // the enum shadowed; with the merged `Name[args]` Display this surfaced as a nonsense
    // "cannot assign Foo[int] to … Foo[int]". Must be a clean "already defined" instead.
    rejects(
        "struct Foo:\n    n: int\nenum Foo:\n    A\n",
        "type 'Foo' is already defined",
    );
    rejects(
        "enum Bar:\n    A\nstruct Bar:\n    n: int\n",
        "type 'Bar' is already defined",
    );
}

// ----- sort() widened to Comparable (G3) -----

#[test]
fn sort_on_comparable_struct_list_ok() {
    let src = "\
struct P:
    n: int
    fn compare(self, other: P) -> int:
        return self.n - other.n
    fn eq(self, other: P) -> bool:
        return self.n == other.n
xs := [P(2), P(1)]
xs.sort()
";
    ok(src);
}

// ----- M23's "a type defining `compare` must define `eq` too" rule: DROPPED with D1 -----
//
// `compare_without_eq_rejected`, `ordering_on_compare_without_eq_names_the_missing_eq` and
// `comparable_bound_on_compare_without_eq_names_the_missing_eq` are DELETED. The rule they pinned
// was enforced only through the `Comparable`→`Eq` embed, and D1 makes a plain struct satisfy `Eq`,
// so it no longer fires. It is dropped DELIBERATELY, not re-homed: its premise was falsified in both
// directions by measurement — a `compare` covering every field in declaration order agrees with
// structural `==` exactly (the rule would fire, wrongly), while the repo's own motivating `Ver`
// DOES define both and still disagrees (the rule would not fire). **Rust owns this** and permits a
// manual `Ord` beside a derived `Eq` — a clippy lint, not an error. `docs/syntax.md` already
// concedes the checker cannot verify the coherence the rule pretended to enforce.
//
// The two surviving halves of those tests — the BARE wordings, which are now the only wordings —
// are kept here.

/// A `compare`-declaring struct now satisfies `Comparable` (D1: its `Eq` embed is the structural
/// derive), so `<` WORKS on it — the M23 rejection is gone. A type with no comparator at all still
/// gets the bare "cannot compare X and Y" (there is no note left to append).
#[test]
fn ordering_on_a_compare_only_struct_is_allowed() {
    ok("\
struct Ver:
    maj: int
    fn compare(self, other: Ver) -> int:
        return self.maj - other.maj
print(Ver(1) < Ver(2))
");
    let errs = check_src("struct P:\n    n: int\nprint(P(1) < P(2))\n");
    assert!(
        errs.iter().any(|e| e.message == "cannot compare P and P"),
        "a comparator-less type must keep the bare wording, got: {errs:?}"
    );
}

/// Same, on the BOUND path: a `[T: Comparable]` bound is satisfied by a `compare`-only struct.
#[test]
fn comparable_bound_on_a_compare_only_struct_is_allowed() {
    ok("\
struct Ver:
    maj: int
    fn compare(self, other: Ver) -> int:
        return self.maj - other.maj
fn mx[T: Comparable](a: T, b: T) -> T:
    if a < b:
        return b
    return a
print(mx(Ver(1), Ver(2)))
");
    // …and transitively through a user protocol that EMBEDS `Comparable`.
    ok("\
protocol Ord2:
    Comparable
    fn tag(self) -> str
struct Ver:
    maj: int
    fn compare(self, other: Ver) -> int:
        return self.maj - other.maj
    fn tag(self) -> str:
        return \"v\"
fn u[T: Ord2](a: T) -> int:
    return 0
print(u(Ver(1)))
");
}

/// The `Comparable` rejections stay bare. Three
/// controls: no `compare` at all (Comparable's own requirement is what is missing), a bare `T: Eq`
/// bound over a struct (now SATISFIED — D1), and a user protocol whose `compare` is mis-typed.
#[test]
fn comparable_bound_hint_does_not_over_fire() {
    let bare = |src: &str, msg: &str| {
        let errs = check_src(src);
        assert!(
            errs.iter().any(|e| e.message == msg),
            "expected the bare wording {msg:?}, got: {errs:?}"
        );
    };
    // (1) No comparator at all — the missing half is `compare`, and it is named, bare.
    bare(
        "\
struct Ver:
    maj: int
fn mx[T: Comparable](a: T, b: T) -> T:
    return a
print(mx(Ver(1), Ver(2)))
",
        "type Ver does not satisfy Comparable (missing method 'compare')",
    );
    // (2) A bare `T: Eq` bound over a struct whose `==` is the structural derive is SATISFIED (D1) —
    // this used to be the "not the ratchet, keep it bare" control and is now simply legal code.
    ok("\
struct Ver:
    maj: int
    fn compare(self, other: Ver) -> int:
        return self.maj - other.maj
fn u[T: Eq](a: T, b: T) -> bool:
    return a == b
print(u(Ver(1), Ver(2)))
");
    // (3) A user protocol requiring only `compare` — no `Eq` anywhere, so the message stays bare.
    bare(
        "\
protocol MyCmp:
    fn compare(self, other: Self) -> int
struct Ver:
    maj: int
    fn compare(self, other: Ver) -> str:
        return \"x\"
fn u[T: MyCmp](a: T) -> int:
    return 0
print(u(Ver(1)))
",
        "type Ver does not satisfy MyCmp (method 'compare' has the wrong signature)",
    );
}

#[test]
fn sort_on_non_comparable_struct_list_rejected() {
    let src = "\
struct P:
    n: int
xs := [P(2), P(1)]
xs.sort()
";
    // `sort` is now file-backed as `where T: Comparable`; a non-Comparable element fails via the
    // standard bound-satisfaction diagnostic (retired the bespoke `sort() requires …` text).
    rejects(
        src,
        "does not satisfy Comparable (missing method 'compare')",
    );
}

#[test]
fn sort_on_primitive_list_still_ok() {
    ok("xs := [3, 1, 2]\nxs.sort()\nys := [\"b\", \"a\"]\nys.sort()\n");
}

#[test]
fn sort_with_args_rejected() {
    rejects("xs := [3, 1]\nxs.sort(5)\n", "expects 0 argument(s)");
}

// ----- user free-fn `where` clause enforcement (merged into type_params) -----

#[test]
fn user_fn_where_bound_enforced_at_call_site() {
    // `where T: Comparable` merges into the `[T]` param's bounds; a non-Comparable arg is rejected.
    let src = "\
struct Q:
    n: int
fn needs_cmp[T](x: T) where T: Comparable:
    y := x
q := Q(1)
needs_cmp(q)
";
    rejects(
        src,
        "does not satisfy Comparable (missing method 'compare')",
    );
}

#[test]
fn user_fn_where_bound_accepts_comparable_arg() {
    ok("fn needs_cmp[T](x: T) where T: Comparable:\n    y := x\nneeds_cmp(5)\n");
}

#[test]
fn user_fn_where_bound_used_inside_body() {
    // A `where`-bounded param may use the bounded operation in the body (`<` needs Comparable) —
    // proves the merge feeds the in-scope bound set, not just call-site enforcement.
    ok(
        "fn pick[T](a: T, b: T) -> T where T: Comparable:\n    if a < b:\n        return b\n    return a\nx := pick(1, 2)\n",
    );
}

#[test]
fn user_fn_where_names_unknown_param_rejected() {
    rejects(
        "fn bad[T]() where Q: Comparable:\n    y := 1\n",
        "unknown type parameter 'Q' in where-clause",
    );
}

#[test]
fn user_fn_where_bound_duplicating_decl_bound_reports_once() {
    // A bound named in BOTH `[T: Comparable]` and `where T: Comparable` must be deduped on merge —
    // a failing call emits the diagnostic exactly ONCE, not once per duplicated bound.
    let src = "\
fn f[T: Comparable](x: T) -> int where T: Comparable:
    return 1
f(true)
";
    let n = check_src(src)
        .iter()
        .filter(|e| e.message.contains("does not satisfy Comparable"))
        .count();
    assert_eq!(n, 1, "expected exactly one Comparable diagnostic, got {n}");
}

// ----- conditional methods: `where` on a user struct/enum method's RECEIVER type param -----

#[test]
fn conditional_method_where_receiver_param_accepted() {
    // A method may `where`-bound the ENCLOSING struct's own type param `T` (not the method's own
    // `[U]`). This is ACCEPTED (no "unknown type parameter") — it is a conditional method.
    ok("\
struct Box[T]:
    val: T
    fn top(self) -> T where T: Comparable:
        return self.val
b := Box(5)
x := b.top()
");
}

#[test]
fn conditional_method_accepts_comparable_receiver() {
    // int satisfies Comparable → the conditional method is callable.
    ok("\
struct Box[T]:
    val: T
    fn top(self) -> T where T: Comparable:
        return self.val
b := Box(5)
print(b.top())
");
}

#[test]
fn conditional_method_rejects_non_comparable_receiver() {
    // A `Box` of a plain (non-Comparable) struct → the conditional method's receiver bound fails
    // at the CALL site with the standard bound-satisfaction diagnostic.
    rejects(
        "\
struct Q:
    n: int
struct Box[T]:
    val: T
    fn top(self) -> T where T: Comparable:
        return self.val
b := Box(Q(1))
x := b.top()
",
        "does not satisfy Comparable (missing method 'compare')",
    );
}

#[test]
fn conditional_method_operator_dispatch_enforces_receiver_bound() {
    // SOUNDNESS regression: a conditional method that IMPLEMENTS an operator protocol's method
    // (`compare` ⇒ Comparable) makes the type STRUCTURALLY satisfy that protocol — but only when the
    // receiver's `where T: Comparable` holds. Operator syntax (`a < b`) resolves the method through
    // `satisfies` (NOT the explicit-call path), so the bound must be enforced INSIDE satisfies or the
    // operator bypasses it (check-ok / run-diverge). `Box[Q]` (Q not Comparable) must reject `<`.
    let prog = |val: &str, op: &str| {
        format!(
            "\
struct Q:
    n: int
struct Box[T]:
    val: T
    # BOUND-ENFORCEMENT STUB, not a model comparator: it answers 0 for `greater` as well as
    # `equal`, so no `eq` can literally agree with it. The test only exercises `<`; `eq` reads the
    # intent (compare by `val`). Don't copy this shape into real code.
    fn compare(self, other: Box[T]) -> int where T: Comparable:
        if self.val < other.val:
            return -1
        return 0
    fn eq(self, other: Box[T]) -> bool where T: Eq:
        return self.val == other.val
a := Box({val})
b := Box({val})
{op}
"
        )
    };
    // int IS Comparable → Box[int] is conditionally Comparable → `<` is allowed.
    ok(&prog("1", "print(a < b)"));
    // Q is NOT Comparable → Box[Q] must NOT satisfy Comparable → `<` is rejected at CHECK time.
    rejects(&prog("Q(1)", "print(a < b)"), "cannot compare");
}

#[test]
fn conditional_method_as_generic_bound_arg_enforces() {
    // SOUNDNESS regression (the other structural-satisfies path): passing `Box[Q]` where a
    // `[U: Comparable]` bound is expected must reject — `satisfies(Box[Q], Comparable)` has to see
    // the receiver `where` fail. `Box[int]` is accepted.
    let prog = |val: &str| {
        format!(
            "\
struct Q:
    n: int
struct Box[T]:
    val: T
    fn compare(self, other: Box[T]) -> int where T: Comparable:
        return 0
    fn eq(self, other: Box[T]) -> bool:
        return true
fn need_cmp[U: Comparable](x: U) -> U:
    return x
y := need_cmp(Box({val}))
"
        )
    };
    ok(&prog("1"));
    rejects(&prog("Q(1)"), "does not satisfy Comparable");
}

#[test]
fn conditional_method_enum_where_receiver_param() {
    // Enum receiver-param conditional method: accept on a Comparable payload, reject on a
    // non-Comparable one.
    ok("\
enum Opt[T]:
    Some(T)
    Nothing
    fn peek(self) -> int where T: Comparable:
        return 1
o := Opt.Some(5)
x := o.peek()
");
    rejects(
        "\
struct Q:
    n: int
enum Opt[T]:
    Some(T)
    Nothing
    fn peek(self) -> int where T: Comparable:
        return 1
o := Opt.Some(Q(1))
x := o.peek()
",
        "does not satisfy Comparable (missing method 'compare')",
    );
}

#[test]
fn conditional_method_own_u_param_where_still_merges() {
    // REGRESSION: a method with its OWN `[U]` and `where U: Comparable` still merges into the
    // method's generic-param bounds and enforces via the generic-method path (unchanged by the
    // receiver-bound diversion).
    ok("\
struct Box[T]:
    val: T
    fn cmp[U](self, other: U) -> int where U: Comparable:
        return 1
b := Box(5)
x := b.cmp(3)
");
    rejects(
        "\
struct Q:
    n: int
struct Box[T]:
    val: T
    fn cmp[U](self, other: U) -> int where U: Comparable:
        return 1
b := Box(5)
x := b.cmp(Q(1))
",
        "does not satisfy Comparable (missing method 'compare')",
    );
}

#[test]
fn conditional_method_pinned_receiver_arg_ok() {
    // Companion: when the receiver's type arg IS pinned to a Comparable concrete type (via a typed
    // field annotation), the conditional method is callable — the defer path does not over-reject a
    // resolved-but-late arg.
    ok("\
struct Box[T]:
    val: List[T]
    fn top(self) -> List[T] where T: Comparable:
        return self.val
b: Box[int] = Box([])
x := b.top()
");
}

#[test]
fn conditional_static_method_rejects_non_comparable() {
    // A STATIC method (no `self`) may carry a receiver-param `where` too — `fn_sig` is shared and
    // cannot tell a static from an instance method. It MUST be enforced on the static-dispatch path
    // (`infer_static_call`), else a conditional factory silently accepts a non-satisfying type arg.
    ok("\
struct Box[T]:
    val: T
    fn of(x: T) -> Box[T] where T: Comparable:
        return Box(x)
b := Box.of(5)
");
    rejects(
        "\
struct Q:
    n: int
struct Box[T]:
    val: T
    fn of(x: T) -> Box[T] where T: Comparable:
        return Box(x)
b := Box.of(Q(1))
",
        "does not satisfy Comparable (missing method 'compare')",
    );
}

#[test]
fn conditional_static_enum_method_rejects_non_comparable() {
    // Mirror for an enum static factory reached as `Enum.method(...)`.
    ok("\
enum Opt[T]:
    Some(T)
    Nothing
    fn build(x: T) -> Opt[T] where T: Comparable:
        return Opt.Some(x)
o := Opt.build(5)
");
    rejects(
        "\
struct Q:
    n: int
enum Opt[T]:
    Some(T)
    Nothing
    fn build(x: T) -> Opt[T] where T: Comparable:
        return Opt.Some(x)
o := Opt.build(Q(1))
",
        "does not satisfy Comparable (missing method 'compare')",
    );
}

#[test]
fn conditional_static_method_on_value_single_diagnostic() {
    // A static method that carries a receiver `where`-bound, wrongly called on a VALUE, must emit
    // ONLY the static-method diagnostic — NOT also a spurious "does not satisfy" from the receiver
    // bound (the enforcement must fire AFTER the `is_static` rejection, not before).
    let src = "\
struct Q:
    n: int
struct Box[T]:
    val: T
    fn mk(x: T) -> T where T: Comparable:
        return x
b := Box(Q(1))
y := b.mk(Q(1))
";
    let errs = check_src(src);
    let bound = errs
        .iter()
        .filter(|e| e.message.contains("does not satisfy"))
        .count();
    assert_eq!(
        bound, 0,
        "no spurious bound error on a static-on-value call, got: {errs:?}"
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("is a static method")),
        "expected the static-method diagnostic, got: {errs:?}"
    );
}

#[test]
fn conditional_method_body_uses_receiver_bound() {
    // BUG-2 REGRESSION: a conditional method whose BODY uses the receiver bound (`<` needs
    // Comparable) must type-check — the receiver `where T: Comparable` puts the bound in scope on
    // the ENCLOSING param during the body check, exactly as a free fn's `where` does (symmetry with
    // `user_fn_where_bound_used_inside_body`). Before the fix the body errored `cannot compare
    // T and T` because the receiver bound was recorded on `sig.where_bounds` (call-site only) and
    // never merged into the in-scope enclosing param `T`.
    ok("\
struct Box[T]:
    val: T
    fn max2(self, other: T) -> T where T: Comparable:
        if self.val < other:
            return other
        return self.val
b := Box(5)
x := b.max2(3)
");
}

#[test]
fn conditional_enum_method_body_uses_receiver_bound() {
    // BUG-2 mirror for an enum conditional method (check_fn_body is shared across struct/enum, so
    // the single fix covers both).
    ok("\
enum Wrap[T]:
    V(T)
    fn bigger(self, a: T, b: T) -> T where T: Comparable:
        if a < b:
            return b
        return a
w := Wrap.V(5)
x := w.bigger(1, 2)
");
}

#[test]
fn conditional_static_method_redundant_decl_bound_reports_once() {
    // BUG-1 REGRESSION: when the enclosing type already declares `[T: Comparable]` AND the static
    // method repeats `where T: Comparable`, the static-dispatch path (`infer_static_call`) must not
    // enforce the bound TWICE — once via the struct decl bound (`tps`) and once via the receiver
    // where-bound (`sig.where_bounds`). The failing factory call must emit the diagnostic exactly
    // ONCE. Fixed by deduping the receiver-bound against the enclosing param's declared bounds in
    // `fn_sig`.
    let src = "\
struct Q:
    n: int
struct Box[T: Comparable]:
    val: T
    fn of(x: T) -> Box[T] where T: Comparable:
        return Box(x)
b := Box.of(Q(1))
";
    let n = check_src(src)
        .iter()
        .filter(|e| {
            e.message
                .contains("does not satisfy Comparable (missing method 'compare')")
        })
        .count();
    assert_eq!(n, 1, "expected exactly one Comparable diagnostic, got {n}");
}

// ----- file-backed `sort` via `where T: Comparable` (port off the bespoke arm) -----

#[test]
fn sort_where_clause_returns_nil() {
    // The file-backed `native fn sort(self) -> nil where T: Comparable` resolves to a nil return —
    // using it as a value is rejected exactly like before (regression guard on the ported sig).
    rejects("xs := [3, 1, 2]\nn := xs.sort() + 1\n", "no value (None)");
}

#[test]
fn sort_where_clause_non_comparable_reports_satisfies_diagnostic() {
    // A non-Comparable element list now fails via the `where T: Comparable` bound-enforcement path,
    // so the message is the standard `does not satisfy <protocol>` diagnostic (not the retired
    // bespoke `sort() requires …` text) — `Eq`, since Comparable's embed is checked first (M23).
    let src = "\
struct Q:
    n: int
xs := [Q(2), Q(1)]
xs.sort()
";
    rejects(
        src,
        "does not satisfy Comparable (missing method 'compare')",
    );
}

#[test]
fn sort_where_clause_bool_list_rejected() {
    rejects(
        "xs := [true, false]\nxs.sort()\n",
        "does not satisfy Comparable",
    );
}
