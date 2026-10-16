//! Checker tests: `Self` in methods (FIX A), compound-op overloading (FIX B), block-scoped returns,
//! int under float, collection aliases, ranges, Contains, F2–F4, FIX 1a/1b, W7-21/24.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ============================================================================
// FIX A — `Self` usable in struct/enum inherent-method signatures/bodies
// ============================================================================

#[test]
fn self_type_in_struct_method_sig() {
    // `-> Self` and a `Self` param resolve to the enclosing struct.
    entry_ok(
        "struct P:\n    x: int\n    fn dup(self) -> Self:\n        return self\n    fn add(self, other: Self) -> Self:\n        return P(self.x + other.x)\nfn main():\n    print(P(5).dup().x)\n    print(P(1).add(P(2)).x)\nmain()\n",
    );
}

#[test]
fn self_type_in_enum_method_sig() {
    // An enum method returning `Self` resolves to the enclosing enum.
    entry_ok(
        "enum Money:\n    Cents(int)\n    fn double(self) -> Self:\n        match self:\n            Money.Cents(c): return Money.Cents(c * 2)\nfn main():\n    m := Money.Cents(50).double()\n    match m:\n        Money.Cents(c): print(c)\nmain()\n",
    );
}

#[test]
fn self_type_rejected_outside_method() {
    // Free-fn param `Self` — no enclosing type → still unknown.
    entry_rejects(
        "fn f(x: Self) -> int:\n    return 0\nfn main():\n    pass\nmain()\n",
        "unknown type 'Self'",
    );
    // Struct field typed `Self` — a field is not a method sig → still unknown.
    entry_rejects(
        "struct P:\n    y: Self\nfn main():\n    pass\nmain()\n",
        "unknown type 'Self'",
    );
    // Top-level variable annotation `Self`.
    entry_rejects(
        "fn main():\n    x: Self = 0\nmain()\n",
        "unknown type 'Self'",
    );
}

#[test]
fn self_type_enforced_as_concrete_enclosing_type() {
    // `-> Self` is the concrete enclosing struct; returning a different struct is a type error.
    entry_rejects(
        "struct A:\n    x: int\nstruct B:\n    y: int\n    fn make(self) -> Self:\n        return A(1)\nfn main():\n    pass\nmain()\n",
        "return",
    );
}

#[test]
fn self_type_protocol_behavior_unchanged() {
    // A protocol method using `Self` still checks (its `Self` is `Ty::Param`, unchanged path).
    entry_ok(
        "protocol Dottable:\n    fn dot(self, o: Self) -> int\nstruct V:\n    x: int\n    fn dot(self, o: Self) -> int:\n        return self.x * o.x\nfn main():\n    print(V(2).dot(V(3)))\nmain()\n",
    );
}

#[test]
fn self_type_generic_struct_method() {
    // Generic struct: `-> Self` carries the struct's own type args, so `return self` type-checks.
    entry_ok(
        "struct Box[T]:\n    v: T\n    fn same(self) -> Self:\n        return self\nfn main():\n    b := Box(5).same()\n    print(b.v)\nmain()\n",
    );
}

// ============================================================================
// FIX B — compound assignment honors struct/enum operator overloading
// ============================================================================

#[test]
fn compound_assign_struct_overload() {
    // `a += V(10)` accepted exactly when `a = a + V(10)` is (V has an `add` overload).
    entry_ok(
        "struct V:\n    x: int\n    fn add(self, other: V) -> V:\n        return V(self.x + other.x)\n    fn str(self) -> str:\n        return \"V({self.x})\"\nfn main():\n    a := V(1)\n    a = a + V(10)\n    a += V(10)\n    print(a)\nmain()\n",
    );
}

#[test]
fn compound_assign_enum_sub_overload() {
    // `-=` on an enum with a matching `sub` overload.
    entry_ok(
        "enum Cnt:\n    N(int)\n    fn amt(self) -> int:\n        match self:\n            Cnt.N(a): return a\n    fn sub(self, other: Cnt) -> Cnt:\n        return Cnt.N(self.amt() - other.amt())\nfn main():\n    c := Cnt.N(10)\n    c -= Cnt.N(3)\n    print(c.amt())\nmain()\n",
    );
}

#[test]
fn compound_assign_rejected_no_overload() {
    // Struct with no `add` → `a += W(..)` rejected (mirrors `a = a + W(..)` failing).
    entry_rejects(
        "struct W:\n    x: int\nfn main():\n    a := W(1)\n    a += W(2)\nmain()\n",
        "cannot apply + to",
    );
}

#[test]
fn compound_assign_rejected_heterogeneous() {
    // `V += int` where `V + int` is a type error → still rejected.
    entry_rejects(
        "struct V:\n    x: int\n    fn add(self, other: V) -> V:\n        return V(self.x + other.x)\nfn main():\n    a := V(1)\n    a += 5\nmain()\n",
        "cannot apply + to",
    );
}

#[test]
fn compound_assign_existing_forms_still_work() {
    // Regression: the pre-existing accepted forms must all still check.
    entry_ok(
        "fn main():\n    i := 1\n    i += 1\n    s := \"a\"\n    s += \"b\"\n    l := [1]\n    l += [2]\n    l *= 3\n    st := {1, 2}\n    st -= {1}\n    print(i)\nmain()\n",
    );
}

// ----- B1: qualified generic turbofish in expression position -----

const B1_SHAPES: &str = "enum Tree[T]:\n    Leaf(T)\n    Branch(Tree[T], Tree[T])\n    fn first(self) -> T:\n        match self:\n            Tree.Leaf(x): return x\n            Tree.Branch(l, r): return l.first()\n\nstruct Box[T]:\n    v: T\n    fn make(x: T) -> Box[T]:\n        return Box(x)\n";

#[test]
fn qualified_turbofish_variant_and_static_ok() {
    files_ok(&[
        ("shapes.chz", B1_SHAPES),
        (
            "main.chz",
            "import shapes\nfn main():\n    x := shapes.Tree[int].Leaf(9)\n    b := shapes.Box[int].make(5)\n    print(x.first() + b.v)\nmain()\n",
        ),
    ]);
}

#[test]
fn qualified_turbofish_regressions_ok() {
    // Must stay working: qualified NO-turbofish variant ctor; annotation form; a real qualified
    // value-subscript that must NOT be stolen as a turbofish head.
    files_ok(&[
        ("shapes.chz", B1_SHAPES),
        (
            "main.chz",
            "import shapes\nfn mk() -> List[int]:\n    return [1, 2, 3]\nfn main():\n    a := shapes.Tree.Leaf(1)\n    y: shapes.Tree[int] = shapes.Tree.Leaf(2)\n    z := mk()[0]\n    print(z)\nmain()\n",
        ),
    ]);
}

#[test]
fn qualified_not_a_type_turbofish_clean_error() {
    // `shapes.NotAType[int].X` — NotAType is not a type in `shapes`: a clean, truthful error, no lie.
    files_reject(
        &[
            ("shapes.chz", B1_SHAPES),
            (
                "main.chz",
                "import shapes\nfn main():\n    x := shapes.NotAType[int].Leaf(9)\n    print(x)\nmain()\n",
            ),
        ],
        "no member",
    );
}

// --- `return` inside a `defer:` / `spawn:` block: a check-time error (was: silently discarded at
// runtime). Chezzi has no named return values, so such a `return` could never mean anything — the
// defer block is its own closure and a spawned task outlives the frame. Same escaping-flow guard
// `recover:` uses.

#[test]
fn defer_block_rejects_return() {
    entry_rejects(
        "fn f() -> int!:\n    defer:\n        return !\"hijack\"\n    return ?1\nprint(f())\n",
        "'return' is not allowed inside a defer block",
    );
}

#[test]
fn defer_block_rejects_bare_return() {
    entry_rejects(
        "fn f() -> int:\n    defer:\n        return\n    return 1\nprint(f())\n",
        "'return' is not allowed inside a defer block",
    );
}

#[test]
fn spawn_block_rejects_return() {
    entry_rejects(
        "fn f() -> int:\n    parallel:\n        spawn:\n            return 7\n    return 1\nprint(f())\n",
        "'return' is not allowed inside a spawn block",
    );
}

// --- boundary: the guard must NOT over-reject.

#[test]
fn defer_block_allows_return_in_nested_fn() {
    // A nested `fn` declared inside the block has its own control flow — its `return` returns from
    // IT, so it stays legal (the guard stops at `StmtKind::Fn`).
    entry_ok(
        "fn f() -> int:\n    defer:\n        fn g() -> int:\n            return 5\n        print(g())\n    return 1\nprint(f())\n",
    );
}

#[test]
fn spawn_block_allows_return_in_nested_fn() {
    entry_ok(
        "fn f() -> int:\n    parallel:\n        spawn:\n            fn g() -> int:\n                return 5\n            print(g())\n    return 1\nprint(f())\n",
    );
}

#[test]
fn parallel_body_still_allows_return() {
    entry_ok("fn f() -> int:\n    parallel:\n        return 7\n    return 1\nprint(f())\n");
}

#[test]
fn defer_block_q_still_allowed() {
    // `?` inside a `defer:` block short-circuits the block and is discarded (docs/reference/syntax.md) — an
    // expression, not a statement: the escaping-flow guard never sees it.
    entry_ok(
        "fn g() -> int!:\n    return ?2\nfn f() -> int!:\n    defer:\n        v := g()?\n        print(v)\n    return ?1\nprint(f())\n",
    );
}

#[test]
fn defer_block_q_discards_regardless_of_enclosing_return() {
    // The `?`-in-defer contract is DISCARD — the enclosing fn's return type is irrelevant (the block
    // is its own closure). Before the fix, `infer_try` validated the defer-block `?` against the
    // enclosing `current_ret`, so it over-rejected under a nil/int-returning fn and only accepted
    // under a Result-returning one by coincidence. All of these must check clean now:
    // nil-returning enclosing fn:
    entry_ok(
        "fn g() -> int!:\n    return !\"x\"\nfn f():\n    defer:\n        v := g()?\n        print(v)\n    print(\"body\")\nf()\n",
    );
    // plain int-returning enclosing fn:
    entry_ok(
        "fn g() -> int!:\n    return ?2\nfn f() -> int:\n    defer:\n        v := g()?\n        print(v)\n    return 7\nprint(f())\n",
    );
    // Option `?` discarded under a nil-returning fn (its kind need not match the enclosing return):
    entry_ok(
        "fn h() -> int?:\n    return None\nfn f():\n    defer:\n        v := h()?\n        print(v)\n    print(\"body\")\nf()\n",
    );
    // module top-level defer block:
    entry_ok(
        "fn g() -> int!:\n    return !\"x\"\ndefer:\n    v := g()?\n    print(v)\nprint(\"top\")\n",
    );
}

#[test]
fn defer_block_q_still_rejects_non_sum_operand() {
    // The discard arm must still reject a `?` on a non-Result/Option operand.
    let errs = check_entry(
        "fn f():\n    defer:\n        v := (1 + 2)?\n        print(v)\n    print(\"b\")\nf()\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("expects a `T?` or `T!E` value")),
        "expected non-sum `?` rejection in defer block, got: {errs:?}"
    );
}

#[test]
fn fn_declared_in_defer_block_gets_own_q_context() {
    // `in_defer_block` must reset across the fn boundary: a nil-returning fn DECLARED inside a defer
    // block still rejects `?` (it is not itself a defer block), while a Result-returning one accepts.
    let errs = check_entry(
        "fn g() -> int!:\n    return !\"x\"\nfn f():\n    defer:\n        fn inner():\n            v := g()?\n            print(v)\n        inner()\n    print(\"b\")\nf()\n",
    );
    assert!(
        errs.iter().any(|e| e
            .message
            .contains("returns None, not a `T?` or `T!E` value")),
        "a None fn declared inside a defer block must still reject `?`, got: {errs:?}"
    );
    entry_ok(
        "fn src() -> int!:\n    return ?5\nfn f():\n    defer:\n        fn inner() -> int!:\n            return ?(src()? + 1)\n        print(\"in {inner()}\")\n    print(\"b\")\nf()\n",
    );
}

#[test]
fn spawn_block_in_defer_does_not_inherit_q_discard() {
    // `in_defer_block` must NOT leak across a spawn task boundary: a spawned task is its own closure,
    // so a `?` inside it targets the task (nil-returning → reject), exactly like a bare
    // `spawn: v := g()?` — NOT the enclosing defer's discard. (Regression for the leak the F1 flag
    // introduced.) Both the defer-wrapped and the bare spawn form must reject identically.
    let wrapped = check_entry(
        "import std.concurrency\nfn g() -> int!:\n    return !\"x\"\nfn f():\n    defer:\n        spawn:\n            v := g()?\n            print(v)\n    print(\"b\")\nf()\n",
    );
    assert!(
        wrapped.iter().any(|e| e.message.contains(SPAWN_Q)),
        "a `?` in a spawn block inside a defer must still reject (the task has no caller), got: {wrapped:?}"
    );
}

/// W7-48 — the spawn-block `?` diagnostic.
const SPAWN_Q: &str = "'?' is not allowed inside a spawn block";

/// W7-48 — a `?`/`?.` inside a `spawn:` BLOCK is rejected regardless of the enclosing fn's return
/// kind. Pre-fix the arm carried the enclosing frame's `current_ret`/`in_fn_body`/`recover_depth`
/// across the task boundary, so a carrier-returning enclosing fn (or the module top level) made the
/// `?` check clean — and at runtime the task's `Err` was discarded by the nursery. A silent swallow.
#[test]
fn q_inside_a_spawn_block_is_rejected_whatever_the_enclosing_fn_returns() {
    let prog = |ret: &str, tail: &str| {
        format!(
            "import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency\nfn g() -> int!str:\n    return !\"x\"\nfn main() -> {ret}:\n    spawn:\n        v := g()?\n        print(v)\n    {tail}\nprint(main())\n"
        )
    };
    // Enclosing `-> Result` — pre-fix `ok: no type errors` (the swallow).
    rejects_entry(&prog("int!str", "return ?0"), SPAWN_Q);
    // Enclosing `-> Option` — pre-fix rejected, but naming the ENCLOSING fn ("returns Option, not
    // Result"), which is the bug in one sentence.
    rejects_entry(&prog("int?", "return ?0"), SPAWN_Q);
    // The `?.` spelling (W7-43) lowers to a real `Try`, so it inherits the gate.
    rejects_entry(
        "import std.concurrency\nfn s() -> str!str:\n    return ?\"hi\"\nfn main() -> int!str:\n    spawn:\n        n := s()?.len()\n        print(n)\n    return ?0\nprint(main())\n",
        SPAWN_Q,
    );
    // Module top level (no enclosing fn) — pre-fix `ok`, because `!in_fn_body` let `Nil` accept.
    rejects_entry(
        "import std.concurrency\nfn g() -> int!str:\n    return !\"x\"\nspawn:\n    v := g()?\n    print(v)\n",
        SPAWN_Q,
    );
    // Nil-returning enclosing fn — pre-fix rejected by luck, with the enclosing-fn wording.
    rejects_entry(
        "import std.concurrency\nfn g() -> int!str:\n    return !\"x\"\nfn main():\n    spawn:\n        v := g()?\n        print(v)\nmain()\n",
        SPAWN_Q,
    );
    // The second hole: a `recover:` OUTSIDE the spawn used to catch the `?` via `recover_depth`, so
    // the whole shape checked clean and the recover reported `Ok(nil)` at runtime while the error
    // vanished. The task boundary now zeroes `recover_depth`, so this falls through to the gate.
    rejects_entry(
        "import std.concurrency\nfn g() -> int!str:\n    return !\"x\"\nfn main() -> int!str:\n    r := recover:\n        parallel:\n            spawn:\n                v := g()?\n                print(v)\n    print(r)\n    return ?0\nprint(main())\n",
        SPAWN_Q,
    );
}

/// W7-48 — the neighbours the reject must NOT touch: every one of these has a real frame to
/// propagate to, and every one runs correctly today.
#[test]
fn q_next_to_a_spawn_block_stays_legal() {
    let head = "import std.concurrency\nfn g() -> int!str:\n    return ?7\n";
    // A `parallel:` body runs in the PARENT frame — its `?` targets the enclosing fn.
    entry_ok(&format!(
        "{head}fn main() -> int!str:\n    parallel:\n        v := g()?\n        print(v)\n    return ?0\nprint(main())\n"
    ));
    // A nested fn DECLARED inside a spawn block has its own caller.
    entry_ok(&format!(
        "{head}fn main() -> int!str:\n    spawn:\n        fn inner() -> int!str:\n            return ?(g()? + 1)\n        print(inner())\n    return ?0\nprint(main())\n"
    ));
    // Same for a closure declared inside the spawn block (`infer_closure`'s reset).
    entry_ok(&format!(
        "{head}fn main() -> int!str:\n    spawn:\n        c := fn() -> int!str: ?(g()? + 1)\n        print(c())\n    return ?0\nprint(main())\n"
    ));
    // `SpawnTarget::Call` — the argument evaluates in the PARENT frame before the task starts.
    entry_ok(&format!(
        "{head}fn h(n: int):\n    print(n)\nfn main() -> int!str:\n    spawn h(g()?)\n    return ?0\nprint(main())\n"
    ));
    // A `defer:` INSIDE the spawn: the discard contract is per-frame and this defer is in the
    // task's frame, so its `?` is discarded there exactly as in any other frame.
    entry_ok(&format!(
        "{head}fn main() -> int!str:\n    spawn:\n        defer:\n            v := g()?\n            print(v)\n        print(\"body\")\n    return ?0\nprint(main())\n"
    ));
    // A `recover:` INSIDE the spawn: its boundary is in the same frame as the `?`, so
    // short-circuiting to it is correct.
    entry_ok(&format!(
        "{head}fn main() -> int!str:\n    spawn:\n        r := recover:\n            v := g()?\n            print(v)\n        print(r)\n    return ?0\nprint(main())\n"
    ));
}

/// W7-48 — the reject must not cost a SECOND, false diagnostic. An earlier cut of this fix zeroed
/// `current_ret` at the task boundary; `check_return` reads the same field, so `spawn: return 5`
/// inside `fn main() -> int` also reported "function returns nothing, cannot return a value" — a
/// claim about `main`, which returns `int`. `in_spawn_block` gates `infer_try` before it reads
/// `current_ret`, so the reset buys nothing and the enclosing return type stays honest.
#[test]
fn spawn_block_return_does_not_claim_the_enclosing_fn_returns_nothing() {
    let errs = check_entry(
        "import std.concurrency\nfn main() -> int:\n    spawn:\n        return 5\n    return 1\nprint(main())\n",
    );
    assert!(
        errs.iter().any(|e| e
            .message
            .contains("'return' is not allowed inside a spawn block")),
        "the escaping-flow guard must still own `return` in a task: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("returns nothing")),
        "the enclosing fn returns `int` — no second diagnostic may claim otherwise: {errs:?}"
    );
}

/// W7-48 — the spawn gate is shaped like the `recover:`/`defer:` gates above it: a NON-CARRIER
/// operand keeps the type diagnostic (the spawn message would hide the real defect), and an
/// already-`Unknown` operand stays silent rather than cascading a second error.
#[test]
fn q_on_a_non_carrier_in_a_spawn_block_still_reports_the_type() {
    let errs = check_entry(
        "import std.concurrency\nfn main() -> int!str:\n    spawn:\n        v := 5?\n        print(v)\n    return ?0\nprint(main())\n",
    );
    assert!(
        errs.iter().any(|e| e
            .message
            .contains("'?' expects a `T?` or `T!E` value, found int")),
        "a non-carrier operand must keep its own diagnostic, not be masked by the spawn message: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains(SPAWN_Q)),
        "the spawn message must not fire on an operand that is not a carrier at all: {errs:?}"
    );
    // An operand whose type is already `Unknown` (its own error was reported) must not cascade.
    let cascade = check_entry(
        "import std.concurrency\nfn main() -> int!str:\n    spawn:\n        v := nope()?\n        print(v)\n    return ?0\nprint(main())\n",
    );
    assert_eq!(
        cascade.len(),
        1,
        "an unknown operand must report once (the unknown name), not twice: {cascade:?}"
    );
}

#[test]
fn defer_block_break_still_says_break_outside_loop() {
    let errs = check_entry(
        "fn f() -> int:\n    for i in 0..2:\n        defer:\n            break\n    return 1\nprint(f())\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("break outside loop")),
        "expected 'break outside loop', got: {errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("defer block")),
        "the loop_depth guard must own break/continue — no double diagnostic: {errs:?}"
    );
}

// --- the guard must be exhaustive over block-bearing statements: a `wait:` arm body (and its
// `else`) is inside the block too, so a `return` there escapes the same way.

#[test]
fn defer_block_rejects_return_in_wait_arm() {
    entry_rejects(
        "fn f() -> int!:\n    ch := Channel[int]()\n    ch.send(9)\n    defer:\n        wait:\n            v := ch.recv():\n                return !\"hijack {v}\"\n    return ?1\nprint(f())\n",
        "'return' is not allowed inside a defer block",
    );
}

#[test]
fn defer_block_rejects_return_in_wait_else() {
    entry_rejects(
        "fn f() -> int!:\n    ch := Channel[int]()\n    defer:\n        wait:\n            v := ch.recv():\n                print(v)\n            else:\n                return !\"hijack\"\n    return ?1\nprint(f())\n",
        "'return' is not allowed inside a defer block",
    );
}

#[test]
fn spawn_block_rejects_return_in_wait_arm() {
    entry_rejects(
        "fn f() -> int:\n    ch := Channel[int]()\n    ch.send(9)\n    parallel:\n        spawn:\n            wait:\n                v := ch.recv():\n                    return v\n    return 1\nprint(f())\n",
        "'return' is not allowed inside a spawn block",
    );
}

#[test]
fn recover_block_rejects_return_in_wait_arm() {
    entry_rejects(
        "fn f() -> int!:\n    ch := Channel[int]()\n    ch.send(9)\n    r := recover:\n        wait:\n            v := ch.recv():\n                return ?v\n        7\n    return r\nprint(f())\n",
        "'return' is not allowed inside a recover block",
    );
}

// --- one `return`, one diagnostic, naming the block it is LEXICALLY in (the innermost guarded
// block owns it — the outer walker must not also claim it under the wrong noun).

#[test]
fn return_in_spawn_nested_in_defer_reports_spawn_only() {
    let errs = check_entry(
        "fn f() -> int:\n    defer:\n        parallel:\n            spawn:\n                return 7\n    return 1\nprint(f())\n",
    );
    let flow: Vec<_> = errs
        .iter()
        .filter(|e| e.message.contains("is not allowed inside a"))
        .collect();
    assert_eq!(flow.len(), 1, "one return, one diagnostic: {errs:?}");
    assert!(
        flow[0].message.contains("spawn block"),
        "the return is lexically in a spawn block: {errs:?}"
    );
}

#[test]
fn return_in_spawn_nested_in_recover_reports_spawn_only() {
    let errs = check_entry(
        "fn f() -> int:\n    x := recover:\n        parallel:\n            spawn:\n                return 7\n        1\n    return 1\nprint(f())\n",
    );
    let flow: Vec<_> = errs
        .iter()
        .filter(|e| e.message.contains("is not allowed inside a"))
        .collect();
    assert_eq!(flow.len(), 1, "one return, one diagnostic: {errs:?}");
    assert!(
        flow[0].message.contains("spawn block"),
        "the return is lexically in a spawn block: {errs:?}"
    );
}

// ===== SOUNDNESS: no runtime Int under a static `float` (untyped-constant widening rule) =====
//
// The rule (Go's): an untyped CONSTANT adapts to a float context; a TYPED int value never implicitly
// converts (write `float(x)`). The checker's accepted set is now a SUBSET of what the type-blind
// compiler can lower, so no sink can end up holding an `Int` under a static `float`.

const WIDEN_NOTE: &str = "write 1.0 (or float(x))";

/// V1 — a non-const int element in a `List[float]` call-arg slot. Checked clean before, ran as an
/// `Int` under a static `float` (`xs[0] / 2` → `0.0` instead of `0.5`).
#[test]
fn widen_v1_nonconst_int_element_in_float_list_param_rejected() {
    entry_rejects(
        "fn f(xs: List[float]) -> float:\n    return xs[0] / 2\nfn main():\n    a := 1\n    print(f([a, 2.5]))\n",
        "list element: expected float, found int",
    );
}

/// V2 — un-annotated mixed literal whose float sibling is a VARIABLE: no type context ⇒ no adaptation.
#[test]
fn widen_v2_unannotated_mixed_nonliteral_float_rejected() {
    entry_rejects(
        "fn main():\n    f := 2.5\n    xs := [1, f]\n    print(xs[0] / 2)\n",
        "list elements differ: int vs float",
    );
}

/// V2 (annotated) — the same literal WITH a `List[float]` annotation stays legal: the annotation is
/// the type context, and the compiler's element hint coerces the untyped int constant.
#[test]
fn d3_widen_annotated_list_const_int_with_float_var_rejected() {
    entry_rejects(
        "fn main():\n    f := 2.5\n    xs: List[float] = [1, f]\n    print(xs[0] / 2)\n",
        "write 1.0",
    );
    entry_ok("fn main():\n    f := 2.5\n    xs: List[float] = [1.0, f]\n    print(xs[0] / 2)\n");
}

/// V3 — a non-const int in a `Map[str, float]` VALUE position.
#[test]
fn widen_v3_nonconst_int_map_value_rejected() {
    entry_rejects(
        "fn f(m: Map[str, float]) -> float:\n    return m[\"k\"] / 2\nfn main():\n    a := 1\n    print(f({\"k\": a, \"j\": 2.5}))\n",
        "map value: expected float, found int",
    );
}

/// The `-> List[float]` RETURN slot.
#[test]
fn widen_nonconst_int_element_in_float_list_return_rejected() {
    entry_rejects(
        "fn mk() -> List[float]:\n    a := 1\n    return [a, 2.5]\nfn main():\n    print(mk())\n",
        "list element: expected float, found int",
    );
}

/// A `List[float]` STRUCT FIELD.
#[test]
fn widen_nonconst_int_element_in_float_list_field_rejected() {
    entry_rejects(
        "struct P:\n    v: List[float]\nfn main():\n    a := 1\n    p := P([a, 2.5])\n    print(p.v)\n",
        "list element: expected float, found int",
    );
}

/// Across a `Channel[List[float]]` — the poisoned Int used to cross the spawn airlock inside the
/// payload. (A SCALAR `Channel[float]`'s `send(1)` was already rejected: builtin method args never
/// widened. Unchanged — only the diagnostic now names the `float(x)` fix.)
#[test]
fn widen_nonconst_int_into_float_channel_rejected() {
    entry_rejects(
        "fn main():\n    ch := Channel[List[float]]()\n    a := 1\n    ch.send([a, 2.5])\n    print(ch.recv())\n",
        "list element: expected float, found int",
    );
}

/// PROOF 1 — a `float`-typed value raising an INTEGER overflow (floats saturate to inf; they cannot
/// overflow). Must be a check error now.
#[test]
fn widen_proof_int_overflow_under_float_rejected() {
    entry_rejects(
        "fn main():\n    a := 9223372036854775807\n    xs := [a, 1.5]\n    print(xs[0] + xs[0])\n",
        "list elements differ: int vs float",
    );
}

/// PROOF 2 — `.sort()` on a `List[float]` silently returning an UNSORTED list (Int/Float compare).
#[test]
fn widen_proof_unsorted_float_list_rejected() {
    entry_rejects(
        "fn mk() -> List[float]:\n    a := 1\n    return [a, 2.5, 0.5]\nfn main():\n    xs := mk()\n    xs.sort()\n    print(xs)\n",
        "list element: expected float, found int",
    );
}

/// A typed int VALUE never widens at a SCALAR sink either — each of these used to check clean and
/// leave an `Int` under a `float` (or, for the let/param sinks, silently coerce only by luck).
#[test]
fn widen_typed_int_at_scalar_sinks_rejected() {
    entry_rejects(
        "fn main():\n    i := 1\n    x: float = i\n    print(x)\n",
        WIDEN_NOTE,
    );
    entry_rejects(
        "fn main():\n    i := 1\n    x: float = i + 1\n    print(x)\n",
        WIDEN_NOTE,
    );
    // a fn RESULT is a TYPED int, even with constant args (correct Go)
    entry_rejects(
        "import std.cmp\nfn main():\n    x: float = cmp.max(1, 2)\n    print(x)\n",
        WIDEN_NOTE,
    );
    // param sink
    entry_rejects(
        "fn f(z: float):\n    print(z)\nfn main():\n    a := 3\n    f(a)\n",
        WIDEN_NOTE,
    );
    // return sink
    entry_rejects(
        "fn g(n: int) -> float:\n    return n + 1\nfn main():\n    print(g(2))\n",
        WIDEN_NOTE,
    );
    // native float param — the CHECKER rejects an int VARIABLE exactly like a user fn (the runtime
    // `Host::arg_float` leniency stays as defence-in-depth, but it is no longer reachable this way).
    entry_rejects(
        "import std.math\nfn main():\n    i := 4\n    print(math.sqrt(i))\n",
        WIDEN_NOTE,
    );
}

/// OVER-REJECTION GUARD — every untyped-int-CONSTANT case still adapts to a float context.
#[test]
fn d3_widen_untyped_int_literal_rejected() {
    for src in [
        "fn main():\n    x: float = 1\n    print(x)\n",
        "fn main():\n    x: float = -5\n    print(x)\n",
        "fn main():\n    x: float = 1 + 2\n    print(x)\n",
        "fn main():\n    x: float = 2 * 3\n    print(x)\n",
        "fn f(z: float):\n    print(z)\nfn main():\n    f(7)\n    f(1 + 2)\n",
        "fn f() -> float:\n    return 1 + 2\nfn main():\n    print(f())\n",
        "struct P:\n    v: float\nfn main():\n    p := P(3)\n    print(p.v)\n",
        "fn g(a: float = 3) -> float:\n    return a\nfn main():\n    print(g())\n",
        "fn main():\n    xs: List[float] = [1, 2.3]\n    print(xs)\n",
        "fn main():\n    m: Map[str, float] = {\"a\": 1, \"b\": 2.3}\n    print(m)\n",
        "fn main():\n    xs := [1, 2.5]\n    print(xs[0])\n",
        "fn main():\n    xs := [1 + 1, 2.5]\n    print(xs[0])\n",
        "fn main():\n    xs := [1, -2.5]\n    print(xs[0])\n",
        "import std.math\nfn main():\n    print(math.floor(2))\n",
        "fn main():\n    ch := Channel[List[float]]()\n    ch.send([1, 2.5])\n    print(ch.recv())\n",
    ] {
        entry_rejects(src, "write 1.0");
    }
}

/// HINT-LEAK GUARD — the `List[float]` let annotation licenses THIS literal's elements only; a nested
/// literal / call argument inside the annotated value does NOT inherit the license.
#[test]
fn widen_let_hint_does_not_leak_into_nested_literal() {
    // the ANNOTATED let licenses `xs`'s own elements — NOT the list literal nested inside a call
    // argument (the compiler `take()`s its hint at the same point, so it would not coerce there).
    entry_rejects(
        "fn g(ys: List[float]) -> float:\n    return ys[0]\nfn main():\n    a := 1\n    xs: List[float] = [g([a, 2.5]), 1.0]\n    print(xs)\n",
        "list element: expected float, found int",
    );
    // a nested collection literal keeps its own (un-licensed) inference
    entry_rejects(
        "fn main():\n    a := 1\n    xs: List[List[float]] = [[a, 2.5]]\n    print(xs)\n",
        "list element: expected float, found int",
    );
}

// ===== SOUNDNESS follow-ups (adversarial review): the sinks the first cut still leaked =====

/// A GENERIC fn instantiated at float and used as a fn VALUE (`f := id[float]`) is generic-ERASED at
/// runtime. An int argument would sit in the slot under a static `float` (`f(1) / 2` → `0`, and a
/// `List[float]` built from it sorted UNSORTED). A function-VALUE call never widens — write `f(1.0)`.
#[test]
fn widen_through_fn_value_rejected() {
    entry_rejects(
        "fn id[T](x: T) -> T:\n    return x\nfn main():\n    f := id[float]\n    print(f(1) / 2)\n",
        "expected float, found int",
    );
    entry_rejects(
        "fn id[T](x: T) -> T:\n    return x\nfn main():\n    f: fn(float) -> float = id[float]\n    print(f(1) / 2)\n",
        "expected float, found int",
    );
    // …and a plain (non-generic) fn reached through a VALUE is strict too (the checker cannot tell
    // the two apart from the `Ty::Func` alone).
    entry_rejects(
        "fn h(z: float) -> float:\n    return z\nfn main():\n    f := h\n    print(f(1))\n",
        "expected float, found int",
    );
    entry_ok(
        "fn id[T](x: T) -> T:\n    return x\nfn main():\n    f := id[float]\n    print(f(1.0) / 2)\n",
    );
}

/// A float sink spelled through a type ALIAS (`type F = float`) is a real float sink: the compiler
/// resolves the alias at every coercion site, so the untyped-constant widen still lowers to an f64.
#[test]
fn d3_widen_through_float_alias_rejected() {
    entry_rejects(
        "type F = float\nfn g(z: F) -> F:\n    return z\nstruct P:\n    v: F\nfn main():\n    x: F = 1\n    xs: List[F] = [1, 2]\n    print(x / 2)\n    print(g(3) / 2)\n    print(P(3).v / 2)\n    print(xs)\n",
        "write 1.0",
    );
    entry_ok(
        "type F = float\nfn g(z: F) -> F:\n    return z\nstruct P:\n    v: F\nfn main():\n    x: F = 1.0\n    xs: List[F] = [1.0, 2.0]\n    print(x / 2)\n    print(g(3.0) / 2)\n    print(P(3.0).v / 2)\n    print(xs)\n",
    );
}

/// The `float(x)` note is attached ONLY to a mismatch whose expression is a TYPED int. An untyped int
/// CONSTANT at a NON-widening float sink (a builtin-method arg, an enum payload) is rejected because
/// the sink does not widen at all — telling the user "a typed int never widens" would be a lie.
#[test]
fn d3_untyped_const_send_carries_the_float_fix_note() {
    let errs = check_entry("fn main():\n    ch := Channel[float]()\n    ch.send(1)\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("expected float, found int")),
        "expected the send() mismatch, got: {errs:?}"
    );
    assert!(
        errs.iter().any(|e| e.message.contains(WIDEN_NOTE)),
        "D3: an untyped int constant at a slot sink names the `write 1.0` fix: {errs:?}"
    );
}

/// A `List[float]` / `Map[_, float]` annotation is a type CONTEXT: an ALL-int-constant literal adapts
/// to it (the docs say so — and now the checker agrees; it used to be a spurious error).
#[test]
fn d3_widen_annotated_all_int_collection_rejected() {
    entry_rejects(
        "fn main():\n    xs: List[float] = [1, 2]\n    print(xs)\n",
        "write 1.0",
    );
    entry_rejects(
        "fn main():\n    m: Map[str, float] = {\"a\": 1}\n    print(m)\n",
        "write 1.0",
    );
}

// ===== adversarial-review fixes: generic erasure + collection-alias annotations =====

/// GENERIC ERASURE at a method call: a method param declared as the struct's type VARIABLE `T`,
/// instantiated at `float`, never accepts an int (same rule as every other sink, D3), or an `Int`
/// would land in a slot whose static type is `float`.
#[test]
fn widen_generic_method_param_at_float_rejected() {
    entry_rejects(
        "struct Box[T]:\n    v: T\n\n    fn set(self, x: T):\n        self.v = x\n\nb := Box[float](1.0)\nb.set(1)\n",
        "expected float, found int",
    );
    // enum method, same shape
    entry_rejects(
        "enum Opt[T]:\n    Some(T)\n    Nothing\n\n    fn eq(self, x: T) -> bool:\n        return true\n\no := Opt[float].Some(1.0)\nprint(o.eq(1))\n",
        "expected float, found int",
    );
}

/// D3 — a collection type spelled through an ALIAS (`type LF = List[float]`) rejects int elements
/// exactly like the spelled-out `List[float]`: the checker judges the RESOLVED slot type.
#[test]
fn d3_widen_collection_alias_annotation_rejects() {
    entry_rejects(
        "type LF = List[float]\nxs: LF = [1, 2]\nprint(xs)\n",
        "write 1.0",
    );
    entry_rejects(
        "type MF = Map[str, float]\nm: MF = {\"k\": 1}\nprint(m)\n",
        "write 1.0",
    );
    // the ELEMENT spelled through an alias is rejected the same way
    entry_rejects(
        "type F = float\nxs: List[F] = [1, 2.5]\nprint(xs)\n",
        "write 1.0",
    );
}

/// D3: a VARIADIC `float` param rejects int constants, like the scalar sink `fn f(z: float); f(1)`.
#[test]
fn d3_widen_variadic_float_param_int_consts_rejected() {
    entry_rejects(
        "fn f(...zs: float):\n    print(zs)\nf(1, 2)\n",
        "list element: expected float, found int",
    );
    entry_rejects(
        "fn f(...zs: float):\n    print(zs)\nf(1, 2.5)\n",
        "write 1.0",
    );
    entry_ok("fn f(...zs: float):\n    print(zs)\nf(1.0, 2.5)\n");
}

// ---------------------------------------------------------------------------
// Bound methods are NOT first-class values (check-OK -> runtime-fault hole); a method named
// through its TYPE is (Rust's path-value rule, TICKET-204).
// A struct field-read that misses the data fields must NOT fall back to the
// method table: it used to hand back a `Ty::Func` still carrying the un-bound
// `self` slot typed `Ty::Unknown`, which (a) has no runtime lowering (the
// compiler emits a plain field load -> VM "no field 'get' on S") and (b)
// LAUNDERS types -- the `?` self slot unifies with anything.
// ---------------------------------------------------------------------------

const METH_S: &str = "\
struct S:
    n: int
    fn get(self) -> int:
        return self.n
";

#[test]
fn bound_method_as_value_rejected() {
    entry_rejects(
        &format!("{METH_S}s := S(5)\ng := s.get\n"),
        "type S has no field 'get'",
    );
    // ...and the message must say WHY (it's a method), not read as a typo.
    entry_rejects(&format!("{METH_S}s := S(5)\ng := s.get\n"), "is a method");
}

#[test]
fn static_method_is_a_value_on_a_local_struct() {
    // TICKET-204: a static method read through its type head is a fn value (Rust `S::zero`).
    const STATIC_S: &str = "\
struct S:
    n: int
    fn zero() -> S:
        return S(0)
";
    entry_ok(&format!("{STATIC_S}h := S.zero\nprint(h().n)\n"));
}

#[test]
fn static_method_is_a_value_through_an_import() {
    // TICKET-204: the two import-qualified static-head spellings are fn values too.
    const LIB: &str = "\
struct Cnt:
    n: int
    fn zero() -> Cnt:
        return Cnt(0)
";
    files_ok(&[
        ("lib/types.chz", LIB),
        (
            "main.chz",
            "import Cnt from lib.types\nh := Cnt.zero\nprint(h().n)\n",
        ),
    ]);
    files_ok(&[
        ("lib/types.chz", LIB),
        (
            "main.chz",
            "import lib.types\nh := types.Cnt.zero\nprint(h().n)\n",
        ),
    ]);
}

#[test]
fn instance_method_through_the_type_head_is_a_value() {
    // TICKET-204: an INSTANCE method read through the TYPE head is a fn value that takes the
    // receiver first (Rust `S::get`).
    const S: &str = "\
struct S:
    n: int
    fn get(self) -> int:
        return self.n
";
    entry_ok(&format!("{S}h := S.get\nprint(h(S(1)))\n"));
}

#[test]
fn conditional_method_value_enforces_its_receiver_where_bound() {
    // TICKET-204 review: a method value read through its type keeps the receiver `where` bound the
    // call form enforces (Rust rejects `Bx::<P>::total`, E0599). Each position must reject.
    const S: &str = "\
struct Bx[T]:
    v: T
    fn total(self, o: Bx[T]) -> T where T: Add:
        return self.v + o.v
struct P:
    n: int
";
    const AP: &str = "fn ap(f: fn(Bx[P], Bx[P]) -> P) -> int:\n    return 0\n";
    const AP2: &str = "fn ap2[X](f: fn(Bx[P], X) -> P, x: X) -> int:\n    return 0\n";
    for tail in [
        "f := Bx[P].total\n".to_string(),
        "g: fn(Bx[P], Bx[P]) -> P = Bx.total\n".to_string(),
        "type B = Bx[P]\nf := B.total\n".to_string(),
        format!("{AP}print(ap(Bx.total))\n"),
        format!("{AP}print(ap(Bx[P].total))\n"),
        format!("{AP2}print(ap2(Bx[P].total, Bx(v=P(n=1))))\n"),
        format!("{AP2}print(ap2(Bx.total, Bx(v=P(n=1))))\n"),
    ] {
        entry_rejects(&format!("{S}{tail}"), "type P does not satisfy Add");
    }
    // Neighbours: a satisfying head arg stays a value, written or pinned by the slot.
    entry_ok(&format!(
        "{S}f := Bx[int].total\nprint(f(Bx(v=1), Bx(v=2)))\n"
    ));
    entry_ok(&format!(
        "{S}g: fn(Bx[int], Bx[int]) -> int = Bx.total\nprint(g(Bx(v=1), Bx(v=2)))\n"
    ));
}

#[test]
fn type_head_member_value_keeps_its_decline_neighbours() {
    // W12-22 scope fence: a shadowing parameter keeps `type int has no field`, and an unknown
    // member on a real struct head reports the type-as-value message (TICKET-204).
    const S: &str = "\
struct S:
    n: int
    fn zero() -> S:
        return S(0)
";
    entry_rejects(
        &format!("{S}fn f(S: int) -> int:\n    return S.zero\n"),
        "type int has no field 'zero'",
    );
    entry_rejects(&format!("{S}h := S.nosuch\n"), "'S' is a type, not a value");
}

#[test]
fn bound_method_launder_rejected() {
    // Each of these type-checked (the `?` self slot unified with anything) then faulted at runtime.
    // Each must now be exactly ONE error -- no `'closure' expects 1 argument(s)` cascade.
    for src in [
        format!("{METH_S}s := S(5)\ng := s.get\nprint(g(s))\n"),
        format!("{METH_S}s := S(5)\ng := s.get\nprint(g(\"anything\"))\n"),
        format!(
            "{METH_S}fn apply(f: fn(S) -> int, v: S) -> int:\n    return f(v)\ns := S(5)\nprint(apply(s.get, s))\n"
        ),
    ] {
        let errs = check_entry(&src);
        assert_eq!(
            errs.len(),
            1,
            "expected exactly one error for {src:?}, got: {errs:?}"
        );
        assert!(
            errs[0].message.contains("has no field 'get'"),
            "unexpected error for {src:?}: {errs:?}"
        );
    }
    // `xs := [s.get, s.get]` used to build a `List[fn(?) -> int]` and fault at LIST CONSTRUCTION.
    // Now each element is rejected at its own span; the elements are then `Unknown`, so the
    // element-type inference reports its usual follow-on. No `'closure' expects 1 argument(s)`.
    let errs = check_entry(&format!(
        "{METH_S}s := S(5)\nxs := [s.get, s.get]\nprint(xs.len())\n"
    ));
    assert_eq!(
        errs.iter()
            .filter(|e| e.message.contains("has no field 'get'"))
            .count(),
        2,
        "expected one reject per element, got: {errs:?}"
    );
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("expects 1 argument")),
        "no arity cascade expected, got: {errs:?}"
    );
}

#[test]
fn bound_method_via_self_rejected() {
    entry_rejects(
        "struct S:\n    n: int\n    fn get(self) -> int:\n        return self.n\n    fn leak(self) -> int:\n        g := self.get\n        return g(self)\ns := S(5)\nprint(s.leak())\n",
        "has no field 'get'",
    );
}

#[test]
fn bound_method_on_generic_struct_rejected() {
    // The error path on a GENERIC receiver still renders (no panic on the shape lookup).
    entry_rejects(
        "struct Box[T]:\n    v: T\n    fn get(self) -> T:\n        return self.v\nb := Box[int](1)\ng := b.get\n",
        "has no field 'get'",
    );
}

#[test]
fn method_neighbors_still_ok() {
    // NO-OVER-REJECTION guards -- everything adjacent to the bound-method reject keeps working.
    // 1. a normal call, a chained call, a call on a nested field.
    entry_ok(
        "struct Inner:\n    n: int\n    fn get(self) -> int:\n        return self.n\nstruct Outer:\n    i: Inner\n    fn inner(self) -> Inner:\n        return self.i\no := Outer(Inner(5))\nprint(o.i.get())\nprint(o.inner().get())\n",
    );
    // 2. THE closest neighbor -- a genuinely fn-TYPED field. Both `h.f(3)` and `g := h.f; g(3)`.
    entry_ok(
        "struct H:\n    f: fn(int) -> int\nfn dbl(x: int) -> int:\n    return x * 2\nh := H(dbl)\nprint(h.f(3))\ng := h.f\nprint(g(3))\n",
    );
    // 3. a fn-typed FIELD whose name collides with a method-ish name -- the field still wins.
    entry_ok(
        "struct C:\n    get: fn(int) -> int\n    fn other(self) -> int:\n        return 1\nfn dbl(x: int) -> int:\n    return x * 2\nc := C(dbl)\nprint(c.get(3))\nprint(c.other())\n",
    );
    // 4. a static/associated CALL (a bare `P.make` is a fn value too, TICKET-204).
    entry_ok(
        "struct P:\n    n: int\n    fn make(n: int) -> P:\n        return P(n)\np := P.make(3)\nprint(p.n)\n",
    );
}

// ---------------------------------------------------------------------------
// `index` / `set_index` V-coherence. `x OP= v` is EXACTLY `x = x OP v`
// (docs/reference/syntax.md, compound assignment), so a compound index-assign's LHS is
// typed from `index`'s RETURN. The direct index-assign path used to take it
// from `set_index`'s `val` param instead, so an incoherent pair
// (`index -> str` / `set_index(_, val: int)`) type-checked, then faulted with
// "cannot apply Add to str and int" at runtime.
// ---------------------------------------------------------------------------

/// index -> str, but set_index takes an int val. Incoherent: not an `IndexSet[int, V]`.
const INCOHERENT_V: &str = "\
struct S:
    d: List[str]
    fn index(self, k: int) -> str:
        return self.d[k]
    fn set_index(self, k: int, v: int):
        print(\"set {v}\")
s := S([\"a\", \"b\"])
";

#[test]
fn index_set_incoherent_rejected() {
    // TICKET-258 (U15): `s[0] += 1` is `s[0] = s[0] + 1`, so the error is the desugar's. Exactly ONE
    // error (no bogus "index must be int" from the not-indexable path).
    let errs = check_entry(&format!("{INCOHERENT_V}s[0] += 1\n"));
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert!(
        errs[0].message.contains("cannot apply + to str and int"),
        "unexpected error: {errs:?}"
    );
    // K mismatch: the write-back keys by str, so the int key meets `set_index`'s key as `=` does.
    entry_rejects(
        "struct S:\n    d: List[str]\n    fn index(self, k: int) -> str:\n        return self.d[k]\n    fn set_index(self, k: str, v: str):\n        print(\"set {v}\")\ns := S([\"a\", \"b\"])\ns[0] += \"x\"\n",
        "index must be str, found int",
    );
    // A NON-int key routes through the same arm — no spurious "index must be int" companion.
    let errs = check_entry(
        "struct M:\n    d: Map[str, str]\n    fn index(self, k: str) -> str:\n        return self.d[k]\n    fn set_index(self, k: str, v: int):\n        print(\"set {v}\")\nm := M({\"a\": \"x\"})\nm[\"a\"] += 1\n",
    );
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert!(
        errs[0].message.contains("cannot apply + to str and int"),
        "unexpected error: {errs:?}"
    );
}

// TICKET-258 (U15): the compound path types `x OP v` from `index`'s return and stores the result
// through `set_index`'s `val`, exactly as the desugar does. No compound-only coherence rule exists.
#[test]
fn index_set_split_pair_compound_equals_its_desugar() {
    let pair = "struct S:\n    f: float\n    fn index(self, k: int) -> int:\n        return int(self.f)\n    fn set_index(self, k: int, v: float):\n        self.f = v\ns := S(1.0)\n";
    entry_ok(&format!("{pair}s[0] = s[0] + 1.5\n"));
    entry_ok(&format!("{pair}s[0] += 1.5\n"));
}

// TICKET-258 (U15): a compound operand gets the target-type hint the desugar's operand gets, so a
// return-only type parameter pins from the slot in both forms.
#[test]
fn compound_rhs_gets_the_slot_hint_like_its_desugar() {
    let mk = "fn mk[T]() -> List[T]:\n    return []\n";
    ok(&format!(
        "{mk}fn main():\n    xs: List[int] = [1]\n    xs = xs + mk()\n    print(xs)\nmain()\n"
    ));
    ok(&format!(
        "{mk}fn main():\n    xs: List[int] = [1]\n    xs += mk()\n    print(xs)\nmain()\n"
    ));
    rejects(
        &format!(
            "{mk}fn main():\n    xs: List[int] = [1]\n    xs += mk()\n    xs += [\"a\"]\nmain()\n"
        ),
        "list element: expected int, found str",
    );
}

// TICKET-258 (U15): a compound operand meets the i64 constant-overflow scan the desugar's does.
#[test]
fn compound_rhs_constant_overflow_rejected() {
    let needle = "does not fit in int";
    rejects("x := 1\nx += 1 << 63\n", needle);
    rejects("x := 1\nx *= -(-9223372036854775807 - 1)\n", needle);
    rejects("x := 1\nx += 9223372036854775807 + 1\n", needle);
}

#[test]
fn index_set_asymmetric_plain_write_still_ok() {
    // NO-OVER-REJECTION: a plain `=` NEVER reads through `index`, so an asymmetric pair stays legal
    // (it type-checks AND runs today). Only the COMPOUND form reads, and only it is gated.
    // (a) a safe-read container: `index -> V?`, `set_index(_, V)`.
    entry_ok(
        "struct T:\n    d: Map[int, int]\n    fn index(self, k: int) -> int?:\n        return self.d.get(k)\n    fn set_index(self, k: int, v: int):\n        self.d[k] = v\nt := T({})\nt[0] = 9\nprint(t[0])\n",
    );
    // (b) a widening writer: `index -> str`, `set_index(_, val: int)` — write-only use is sound.
    entry_ok(&format!("{INCOHERENT_V}s[0] = 1\n"));
}

#[test]
fn index_set_missing_method_messages_unchanged() {
    // The vague wording is PRESERVED for the missing-method cases; only a disagreeing PAIR gets the
    // new coherence message.
    entry_rejects(
        "struct RO:\n    xs: List[int]\n    fn index(self, k: int) -> int:\n        return self.xs[k]\nb := RO([1, 2, 3])\nb[0] = 9\n",
        "cannot index-assign into RO",
    );
    entry_rejects(
        "struct WO:\n    xs: List[int]\n    fn set_index(self, k: int, v: int):\n        self.xs[k] = v\nb := WO([1, 2, 3])\nb[0] = 9\n",
        "cannot index-assign into WO",
    );
}

#[test]
fn index_set_coherent_still_ok() {
    // NO-OVER-REJECTION: a coherent pair -- read / write / compound / negative index.
    entry_ok(
        "struct S:\n    d: List[str]\n    fn index(self, k: int) -> str:\n        return self.d[k]\n    fn set_index(self, k: int, v: str):\n        self.d[k] = v\ns := S([\"a\", \"b\"])\nprint(s[0])\ns[0] = \"x\"\ns[1] += \"y\"\nprint(s[-1])\n",
    );
    // A GENERIC coherent pair must survive EVERY instantiation (the compare happens AFTER the
    // struct param substitution).
    let g = "struct Buf[T]:\n    xs: List[T]\n    fn index(self, k: int) -> T:\n        return self.xs[k]\n    fn set_index(self, k: int, v: T):\n        self.xs[k] = v\n";
    entry_ok(&format!(
        "{g}b := Buf[int]([1, 2])\nb[0] = 9\nb[1] += 1\nprint(b[0])\n"
    ));
    entry_ok(&format!(
        "{g}b := Buf[str]([\"a\"])\nb[0] = \"z\"\nb[0] += \"!\"\nprint(b[0])\n"
    ));
    // Builtin index/field targets must not regress.
    entry_ok(
        "struct F:\n    n: int\nm := {\"a\": 1}\nm[\"a\"] += 1\nxs := [1, 2]\nxs[0] += 1\nf := F(1)\nf.n += 1\nprint(m[\"a\"] + xs[0] + f.n)\n",
    );
}

// ===== a range is a value (TICKET-253) =====

/// A range is an ordinary value of the prelude struct `Range` in every position; what a value of that
/// type cannot do is still an error. The grid of `tests/range_value_grid.rs` runs the accepted cells.
#[test]
fn range_in_value_position_is_a_value() {
    entry_ok("fn main():\n    x := 0..3\n    print(x)\nmain()\n");
    entry_ok(
        "fn main():\n    y := List(0..3)\n    z := Set(0..3)\n    print(y)\n    print(z)\nmain()\n",
    );
    entry_ok("fn main():\n    n := (0..5).len()\n    print(n)\nmain()\n");
    entry_ok(
        "fn main():\n    xs := [0..3, 5..6]\n    m := {\"a\": 0..3}\n    print(xs)\n    print(m)\nmain()\n",
    );
    entry_ok(
        "fn total(xs: Iterable[int]) -> int:\n    s := 0\n    for x in xs:\n        s = s + x\n    return s\nfn main():\n    print(total(0..4))\nmain()\n",
    );
    entry_ok(
        "fn first_or[S: Iterable[int]](xs: S, d: int) -> int:\n    for x in xs:\n        return x\n    return d\nfn main():\n    print(first_or(0..3, 9))\nmain()\n",
    );
    // What a `Range` is not.
    entry_rejects(
        "fn main():\n    (0..3).push(7)\nmain()\n",
        "no method 'push'",
    );
    entry_rejects(
        "fn main():\n    y := (0..3) + [7]\n    print(y)\nmain()\n",
        "Range",
    );
    entry_rejects(
        "fn main():\n    b := (0..3) == [0, 1, 2]\n    print(b)\nmain()\n",
        "Range",
    );
    // A list parameter does not take a range; the old laundering as `List[int]` is gone.
    entry_rejects(
        "fn total(xs: List[int]) -> int:\n    return 0\nfn main():\n    print(total(0..4))\nmain()\n",
        "Range",
    );
    entry_rejects(
        "fn main():\n    y: str = 0..3\n    print(y)\nmain()\n",
        "Range",
    );
    // No `r[i]`, no `r[a..b]` (owner answer 4).
    entry_rejects("fn main():\n    print((0..10)[2])\nmain()\n", "Range");
}

/// The sanctioned positions — the ONLY three places a `Range` expr reaches the compiler
/// (`for`-iterable, comprehension-clause iterable, slice receiver) plus the `match` range PATTERN
/// (a different AST node entirely). An over-broad fix has twice rejected a legitimate neighbor in
/// this repo, so every one gets a boundary assertion.
#[test]
fn range_sanctioned_positions_still_check() {
    // for-iterable: literal bounds, a call in the bound, parens, expression bounds.
    entry_ok("fn main():\n    for i in 0..10:\n        print(i)\nmain()\n");
    entry_ok(
        "fn main():\n    xs := [1, 2, 3]\n    for i in 0..xs.len():\n        print(xs[i])\nmain()\n",
    );
    entry_ok("fn main():\n    for i in (0..3):\n        print(i)\nmain()\n");
    entry_ok(
        "fn main():\n    a := 1\n    b := a + 4\n    for i in a..b:\n        print(i)\nmain()\n",
    );
    // comprehension-clause iterable: plain, guarded, nested, map- and set-comprehension.
    entry_ok("fn main():\n    print([i for i in 0..3])\nmain()\n");
    entry_ok("fn main():\n    print([i for i in 0..10 if i % 2 == 0])\nmain()\n");
    entry_ok(
        "fn main():\n    print([x * y for x in 1..4 if x % 2 == 1 for y in [10, 20]])\nmain()\n",
    );
    entry_ok("fn main():\n    print({i: i * i for i in 0..3})\nmain()\n");
    entry_ok("fn main():\n    print({i % 3 for i in 0..9})\nmain()\n");
    // match range PATTERNS (`Pattern::Range`, never reaches `infer_kind`), incl. negative bounds.
    entry_ok(
        "fn grade(n: int) -> str:\n    match n:\n        0..60: return \"F\"\n        60..90: return \"B\"\n        _: return \"A\"\nfn main():\n    print(grade(70))\nmain()\n",
    );
    entry_ok(
        "fn side(n: int) -> str:\n    match n:\n        -10..-5: return \"lo\"\n        _: return \"hi\"\nfn main():\n    print(side(-7))\nmain()\n",
    );
    // the documented escape hatch: `range(a, b)` really does materialize a `List[int]`.
    entry_ok(
        "fn main():\n    xs: List[int] = List(0..3)\n    print(Set(0..3).len() + xs.len())\nmain()\n",
    );
}

/// R1 — the bytes native seam is `bytes`-ONLY at every param. A `bytearray` is NOT assignable to a
/// `bytes` sink (the deliberate rule from 7b29552 — `bytearray_not_assignable_to_bytes`; a mutable
/// buffer aliased under an immutable `bytes` type is the hole it closes), so a caller converts with
/// `bytes(ba)` — an explicit copy, exactly like CPython's `bytes(ba)`. This pins the rule ACROSS the
/// new seam (io / crypto / encoding / Socket), which is what the shipped doc comments now say.
#[test]
fn bytes_native_seam_takes_bytes_only_bytearray_needs_an_explicit_convert() {
    for (call, what) in [
        ("crypto.sha256_bytes(ba)", "sha256_bytes"),
        ("encoding.base64_encode_bytes(ba)", "base64_encode_bytes"),
        ("io.write_bytes(\"/dev/null\", ba)", "write_bytes"),
    ] {
        let errs = check_entry(&format!(
            "import std.crypto\nimport std.encoding\nimport std.io\n\nfn main():\n    ba := bytearray(b\"hi\")\n    print({call})\nmain()\n"
        ));
        assert!(
            errs.iter()
                .any(|e| e.message.contains("expected bytes, found bytearray")),
            "{what} must reject a bytearray, got: {errs:?}"
        );
    }
    // Socket.write_bytes too (the same rule on the VM-intercepted method sig).
    let errs = check_entry(
        "import std.net\n\nfn go(sock: net.Socket) -> int!:\n    ba := bytearray(b\"hi\")\n    n := sock.write_bytes(ba)?\n    return ?n\n\nfn main():\n    pass\nmain()\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("expected bytes, found bytearray")),
        "Socket.write_bytes must reject a bytearray, got: {errs:?}"
    );
    // …and `bytes(ba)` is the documented convert — it type-checks everywhere the seam takes bytes.
    entry_ok(
        "import std.crypto\nimport std.encoding\nimport std.io\n\nfn main():\n    ba := bytearray(b\"hi\")\n    print(crypto.sha256_bytes(bytes(ba)))\n    print(encoding.base64_encode_bytes(bytes(ba)))\n    match io.write_bytes(\"/dev/null\", bytes(ba)):\n        ?_: pass\n        !e: print(e.message())\nmain()\n",
    );
    entry_ok(
        "import std.net\n\nfn go(sock: net.Socket) -> int!:\n    ba := bytearray(b\"hi\")\n    n := sock.write_bytes(bytes(ba))?\n    return ?n\n\nfn main():\n    pass\nmain()\n",
    );
}

// ----- Contains operator protocol (L5): `x in some_struct` -----

/// A struct with a `contains(self, item) -> bool` method accepts `x in it` when the LHS type
/// matches the declared item type.
#[test]
fn contains_protocol_struct_ok() {
    ok(
        "struct Bag:\n    xs: List[int]\n    fn contains(self, item: int) -> bool:\n        return false\nfn main():\n    b := Bag([1])\n    print(2 in b)\nmain()\n",
    );
}

/// Item-type mismatch (`"s" in bag_of_int`) is a CLEAN checker error, never a runtime panic (req 2).
#[test]
fn contains_item_type_mismatch_rejects() {
    rejects(
        "struct Bag:\n    xs: List[int]\n    fn contains(self, item: int) -> bool:\n        return false\nfn main():\n    b := Bag([1])\n    print(\"s\" in b)\nmain()\n",
        "membership",
    );
}

/// A struct WITHOUT a `contains` method still cleanly rejects `in`, with a message that hints at the
/// `contains` protocol (req 4).
#[test]
fn contains_missing_method_rejects_with_hint() {
    rejects(
        "struct NoC:\n    n: int\nfn main():\n    print(2 in NoC(0))\nmain()\n",
        "contains",
    );
}

/// A `contains` whose return type is not `bool` does not satisfy the protocol — `in` rejects with the
/// same protocol hint rather than green-lighting a non-bool result.
#[test]
fn contains_wrong_return_rejects_with_hint() {
    rejects(
        "struct Bag:\n    n: int\n    fn contains(self, item: int) -> int:\n        return 0\nfn main():\n    print(2 in Bag(0))\nmain()\n",
        "contains",
    );
}

/// Generic `Box[T]`: the `contains` param type is INSTANTIATED to `int` — `2 in Box[int](5)` is OK
/// but `"s" in Box[int](5)` is rejected (subst must not leave `Param(T)`/`Unknown`) (req 3).
#[test]
fn contains_generic_struct_substitutes_item() {
    // `Box[T]`'s own `contains` body compares `x == self.v`, both `T` — W7-53 requires `T: Eq` at
    // the struct's OWN declaration, same as a generic fn.
    ok(
        "struct Box[T: Eq]:\n    v: T\n    fn contains(self, item: T) -> bool:\n        return item == self.v\nfn main():\n    print(2 in Box[int](5))\nmain()\n",
    );
    rejects(
        "struct Box[T: Eq]:\n    v: T\n    fn contains(self, item: T) -> bool:\n        return item == self.v\nfn main():\n    print(\"s\" in Box[int](5))\nmain()\n",
        "membership",
    );
}

/// `in` through a `Contains[int]` generic bound is accepted (analog of `<` through `Comparable`),
/// and a mismatched item type (`str` LHS against a `Contains[int]` bound) is still cleanly rejected —
/// the bound's item arg is recovered, not left `Unknown`.
#[test]
fn contains_through_bound_ok_and_item_mismatch_rejects() {
    ok(
        "struct Bag:\n    xs: List[int]\n    fn contains(self, item: int) -> bool:\n        return false\nfn has[C: Contains[int]](c: C, n: int) -> bool:\n    return n in c\nfn main():\n    print(has(Bag([1]), 1))\nmain()\n",
    );
    rejects(
        "struct Bag:\n    xs: List[int]\n    fn contains(self, item: int) -> bool:\n        return false\nfn has[C: Contains[int]](c: C, s: str) -> bool:\n    return s in c\nfn main():\n    print(has(Bag([1]), \"x\"))\nmain()\n",
        "membership",
    );
}

// ===== F2/F3/F4 — checker over-rejection / diagnostic fixes =====

/// Task 2 (option a) — EVERY protocol existential is sendable now (Go `chan interface` parity), not
/// just the built-in `Error`: `Channel[Drawable]`, `Channel[int!]`, `Channel[Error]`, and
/// `Channel[NS]` over any user protocol all type-check. A genuinely-unserializable element (one
/// carrying an FFI/native handle) is rejected at the RUNTIME airlock, not at construction — see
/// `vm::golden_tests::ffi_handle_cannot_cross_airlock_three_engine`.
#[test]
fn channel_of_any_protocol_existential_is_sendable() {
    entry_ok(
        "protocol Drawable:\n    fn draw(self) -> str\nc := Channel[Drawable]()\nprint(\"x\")\n",
    );
    entry_ok("import std.concurrency\nc := Channel[int!]()\nprint(\"x\")\n");
    entry_ok("import std.concurrency\nc := Channel[Error]()\nprint(\"x\")\n");
    entry_ok(
        "import std.concurrency\nprotocol NS:\n    fn tag(self) -> int\nc := Channel[NS]()\nprint(\"x\")\n",
    );
}

/// Task 2 (option a) — a user protocol existential is now SENDABLE across the airlock (Go
/// `chan interface` parity): the erased witness crosses by value like any other type; a witness that
/// genuinely can't cross (one carrying an FFI/native handle) is rejected at the runtime airlock, not
/// at construction (see `vm::golden_tests::ffi_handle_cannot_cross_airlock_three_engine`).
#[test]
fn protocol_existential_is_sendable_across_airlock() {
    // Bare `Channel[UserProto]()` type-checks.
    entry_ok(
        "protocol Drawable:\n    fn draw(self) -> str\nc := Channel[Drawable]()\nprint(\"x\")\n",
    );
    // A protocol-typed struct field carried as a channel element type-checks.
    entry_ok(
        "protocol Drawable:\n    fn draw(self) -> str\nstruct H:\n    f: Drawable\nc := Channel[H]()\nprint(\"x\")\n",
    );
    // A protocol-typed spawn arg type-checks.
    entry_ok(
        "protocol Drawable:\n    fn draw(self) -> str\nstruct Sq:\n    fn draw(self) -> str:\n        return \"sq\"\nfn use_it(d: Drawable):\n    print(d.draw())\nfn main():\n    parallel:\n        spawn use_it(Sq())\nmain()\n",
    );
}

/// Task 2 (option a) — `GErr{w: Odd}` where `Odd` is a user protocol is now SENDABLE (`Odd` is
/// sendable ⇒ `GErr` is sendable), so a DIRECT-LITERAL `Err(GErr(..))` sent over `Channel[int!]`
/// type-checks (and runs: prints `sent`). This was rejected under the old Error-only rule, where a
/// protocol-field struct was the canonical "non-sendable Error witness"; that class no longer exists
/// at the checker level. A witness carrying an FFI/native handle is checker-sendable and rejected at
/// the RUNTIME airlock instead (`vm::golden_tests::ffi_handle_cannot_cross_airlock_three_engine`).
#[test]
fn channel_send_sendable_error_literal_ok() {
    entry_ok(
        "import std.concurrency\nprotocol Odd:\n    fn tag(self) -> int\nstruct Impl:\n    fn tag(self) -> int:\n        return 1\nstruct GErr:\n    w: Odd\n    fn message(self) -> str:\n        return \"x\"\nc := Channel[int!]()\nc.send(!GErr(Impl()))\n",
    );
}

/// Task 2 (option a) — a `?` propagating `GErr{w: Odd}` inside a `recover:` block is now ACCEPTED:
/// `GErr` satisfies `Error` (has `message`) AND is sendable (its `Odd` field is a sendable protocol),
/// so the recover result `Result[_, Error]` accepts it. Was rejected with the "satisfies Error but
/// isn't sendable" split message under the old rule, when a protocol field made the witness
/// non-sendable.
#[test]
fn recover_try_sendable_error_ok() {
    entry_ok(
        "protocol Odd:\n    fn tag(self) -> int\nstruct Impl:\n    fn tag(self) -> int:\n        return 1\nstruct GErr:\n    w: Odd\n    fn message(self) -> str:\n        return \"x\"\nfn bar(x: int) -> int!GErr:\n    if x == 0:\n        return ?1\n    return !GErr(Impl())\nfn main():\n    r := recover: bar(1)?\n    print(\"unreached\")\nmain()\n",
    );
}

/// Task 2 (option a) — an EXPLICIT `-> int!` (`Result[int, Error]`) return whose concrete `Err`
/// branch is `GErr{w: Odd}` is now ACCEPTED at the `return` widening site: `GErr` is sendable (its
/// `Odd` field is a sendable protocol), so the `assignable`-to-`Error` guard admits it. Was rejected
/// under the old rule.
#[test]
fn explicit_bang_annotation_over_sendable_error_ok() {
    entry_ok(
        "protocol Odd:\n    fn tag(self) -> int\nstruct Impl:\n    fn tag(self) -> int:\n        return 1\nstruct GErr:\n    w: Odd\n    fn message(self) -> str:\n        return \"x\"\nfn f(x: int) -> int!:\n    if x == 0:\n        return ?1\n    return !GErr(Impl())\nprint(\"x\")\n",
    );
}

/// Task 2 (option a) — a generic `fn wrap[E](e: E) -> int!E` instantiated with concrete `E = GErr`
/// (sendable, since its `Odd` field is a sendable protocol) and SENT over `Channel[int!]` is now
/// ACCEPTED. Was rejected under the old rule at the `Channel.send` boundary (`GErr` counted as
/// non-sendable). The generic-param deferral (`Ty::Param => true` re-checked at instantiation) is
/// unchanged; the concrete substitution is simply sendable now.
#[test]
fn generic_fn_sendable_err_instantiation_ok_at_channel_send() {
    entry_ok(
        "import std.concurrency\nprotocol Odd:\n    fn tag(self) -> int\nstruct Impl:\n    fn tag(self) -> int:\n        return 1\nstruct GErr:\n    w: Odd\n    fn message(self) -> str:\n        return \"x\"\nfn wrap[E: Error](e: E) -> int!E:\n    return !e\nc := Channel[int!]()\nc.send(wrap(GErr(Impl())))\nprint(\"x\")\n",
    );
}

/// Task 2 (option a) — a 3+ branch inferred `Result` return mixing `EA`/`GErr{w:Odd}`/`EB` now folds
/// cleanly to `Result[int, Error]` and is ACCEPTED. All three witnesses are sendable now (`GErr`'s
/// `Odd` field is a sendable protocol), so `join_err_slot` unifies them to the `Error` existential
/// instead of hard-conflicting on the old sendable-vs-non-sendable split. Was rejected under the old
/// rule with "cannot infer return type: conflicting branches" — the order-sensitivity that caused it
/// only existed because a protocol-field struct read as non-sendable.
#[test]
fn three_branch_mixed_error_inference_ok() {
    entry_ok(
        "protocol Odd:\n    fn tag(self) -> int\nstruct Impl:\n    fn tag(self) -> int:\n        return 1\nstruct GErr:\n    w: Odd\n    fn message(self) -> str:\n        return \"x\"\nstruct EA:\n    fn message(self) -> str:\n        return \"a\"\nstruct EB:\n    fn message(self) -> str:\n        return \"b\"\nfn f(x: int) -> int!:\n    if x == 0:\n        return ?1\n    elif x == 1:\n        return !EA()\n    elif x == 2:\n        return !GErr(Impl())\n    else:\n        return !EB()\nprint(\"x\")\n",
    );
}

/// F3 — a generic fn over a native reserved handle (`Shared`/`Channel`/`Atomic`/`RwShared`) binds its
/// type param `T` from the argument, exactly like the identical shape over `List[T]`.
#[test]
fn generic_fn_over_native_handles_infers_param() {
    entry_ok(
        "import Shared from std.concurrency\nfn peek[T](s: Shared[T]) -> T:\n    return s.get()\nx := peek(Shared(9))\nprint(x)\n",
    );
    entry_ok(
        "import std.concurrency\nfn first[T](c: Channel[T]) -> T:\n    return c.recv()\nch := Channel[int]()\nch.send(1)\nprint(first(ch))\n",
    );
    entry_ok(
        "import Atomic, StructuralEq from std.concurrency\nfn look[T: StructuralEq](a: Atomic[T]) -> T:\n    return a.load()\nprint(look(Atomic(3)))\n",
    );
    entry_ok(
        "import RwShared from std.concurrency\nfn peekrw[T](s: RwShared[T]) -> T:\n    return s.get()\nprint(peekrw(RwShared(4)))\n",
    );
}

/// F3 (subst side) — a generic wrapper struct holding a `Channel[T]` substitutes `Channel[T]→Channel[int]`
/// so its channel field/method type resolves after construction.
#[test]
fn generic_wrapper_struct_holding_channel_substitutes() {
    entry_ok(
        "import std.concurrency\nstruct Box[T]:\n    ch: Channel[T]\nfn main():\n    b := Box(Channel[int]())\n    b.ch.send(7)\n    x: int = b.ch.recv()\n    print(x)\nmain()\n",
    );
}

/// F4 — `Atomic.add` type mismatch must NOT show the List/Set collection element-pin hint (wrong
/// domain); the mismatch error itself is unchanged. `Set.add`'s hint still fires.
#[test]
fn atomic_add_mismatch_no_collection_hint() {
    let errs = check_entry("import Atomic from std.concurrency\na := Atomic(0)\na.add(1.5)\n");
    let msg = errs
        .iter()
        .map(|e| e.message.as_str())
        .find(|m| m.contains("expected int, found float"))
        .expect("Atomic.add float mismatch should be reported");
    assert!(
        !msg.contains("List[<protocol>]"),
        "Atomic.add hint should NOT mention the List collection pin: {msg:?}"
    );
    // Set.add's collection hint STILL fires (a Set first-use-pinned to int, mismatched later add).
    rejects(
        "fn main():\n    s := Set()\n    s.add(1)\n    s.add(\"x\")\nmain()\n",
        "cannot infer the",
    );
}

// ===== `ref` keyword / `Ref[T]` box / `import std.ref` REMOVED (minimalism cleanup) =====
// Boundary pin: after removal, none of the three surfaces exist. Each program must FAIL to compile
// with a clean error (a resolve/parse/type error, never a panic), not silently work.
#[test]
fn ref_surface_removed_fails_to_compile() {
    fn compile_fails(src: &str) -> String {
        let t = TmpDir::new();
        let entry = t.write("main.chz", src);
        match crate::resolver::build_graph(&entry) {
            Err(e) => format!("{e:?}"),
            Ok(graph) => match check_graph(&graph) {
                Ok(()) => String::new(),
                Err(errs) => format!("{errs:?}"),
            },
        }
    }
    for src in [
        "import std.ref\nfn main():\n    print(1)\nmain()\n",
        "fn main():\n    x: ref int = 0\n    print(x)\nmain()\n",
        "fn main():\n    x: Ref[int] = 0\n    print(x)\nmain()\n",
    ] {
        let msg = compile_fails(src);
        assert!(!msg.is_empty(), "expected a compile error for: {src:?}");
    }
}

// ===== FIX 1a — member-level turbofish on a RESERVED built-in receiver whose harvested method
// declares its OWN `[U]` params (`List.map`) is ACCEPTED, not rejected "takes no type argument(s)".
// Before the fix `member_own_type_params` fell to `_ => false` for reserved receivers, so the
// turbofish gate rejected even a shipped generic method. =====
#[test]
fn reserved_receiver_generic_method_turbofish_ok() {
    ok("print([1, 2, 3].map[int](fn(x): x * 2))\n");
}

/// FIX 1a boundary — a NON-generic reserved-receiver method still rejects a turbofish (`List.filter`
/// declares no own `[U]`). The fix must not blanket-accept turbofish on all reserved methods.
#[test]
fn reserved_receiver_nongeneric_method_turbofish_rejected() {
    rejects(
        "print([1, 2, 3].filter[int](fn(x): x > 1))\n",
        "takes no type argument(s)",
    );
}

// ===== W6-17 — FIX 1a's sibling hole: the `RwShared` read-view `fold`/`fold_entries` are genuinely
// generic in `R` but ARM-ONLY (hand-built in the `Ty::RwShared` dispatch arm, since `R` is not
// nameable in `RwShared[T]`'s harvested surface), so `member_own_type_params`' `structs` lookup
// answered false and the turbofish gate rejected them — while the un-turbofished form and the
// harvested `[1,2].fold[int](…)` both worked. Over-rejection, so the fix only ACCEPTS more. =====
#[test]
fn rwshared_readview_fold_turbofish_ok() {
    entry_ok(
        "import std.concurrency\n\
         import ConcurrentList, ConcurrentMap from std.concurrency.collection\n\
         fn main():\n\
         \x20   rw := ConcurrentList(concurrency.RwShared([1, 2, 3]))\n\
         \x20   print(rw.fold[int](0, fn(a, x): a + x))\n\
         main()\n",
    );
    entry_ok(
        "import std.concurrency\n\
         import ConcurrentList, ConcurrentMap from std.concurrency.collection\n\
         fn main():\n\
         \x20   m := ConcurrentMap(concurrency.RwShared({\"a\": 1}))\n\
         \x20   print(m.fold_entries[int](0, fn(a, k, v): a + v))\n\
         main()\n",
    );
}

/// W6-17 boundary — a NON-generic read-view method still rejects a turbofish. The fix names exactly
/// `fold`/`fold_entries`, it does not blanket-accept turbofish on every `RwShared` method.
#[test]
fn rwshared_readview_nongeneric_method_turbofish_rejected() {
    rejects(
        "import RwShared from std.concurrency\n\
         fn main():\n\
         \x20   rw := RwShared([1, 2, 3])\n\
         \x20   print(rw.len[int]())\n\
         main()\n",
        "takes no type argument(s)",
    );
}

// ===== FIX 1b — a BODIED generic method on a concurrency handle (`Executor.submit_result[T]`) opens
// its own `[T]` and infers T from the closure return, instead of failing "expected fn()->T, found
// fn()->int". Before the fix the `Ty::Executor` arm only `native_handle_method`+`check_args_range`,
// so a harvested sig carrying `[T]` never routed through `infer_generic_method`. =====
#[test]
fn executor_bodied_generic_method_infers_from_closure() {
    entry_ok(
        "import Executor from std.concurrency\n\
         fn main():\n\
         \x20   ex := Executor()\n\
         \x20   ch := ex.submit_result(fn() -> int: 5)\n\
         \x20   ex.shutdown()\n\
         \x20   print(ch.recv())\n\
         main()\n",
    );
}

// ===== W7-21 — a module GLOBAL holding a fn VALUE is callable through the module (`m.G()`). The
// call arm read only `ModuleSig::functions` (declared `fn`s), so a `Ty::Func` sitting in `values` (a
// top-level `let`/`:=`) resolved as a value but not as a call — and the diagnostic denied the member
// existed at all. Both ancestors accept the direct call: CPython `m.G()` → 1, Go `pkg.G()` → 1. =====

#[test]
fn module_global_of_fn_type_is_callable_qualified() {
    files_ok(&[
        ("k.chz", "fn one() -> int:\n    return 1\n"),
        ("l.chz", "import k\nBARE := k.one\n"),
        (
            "main.chz",
            "import l\nx := l.BARE\ny := l.BARE()\nprint(x)\nprint(y)\n",
        ),
    ]);
}

/// The fallback is STRICT about arity/types, exactly like the fn-value and fn-field call paths — it
/// widens nothing just because the callee arrived through a module member.
#[test]
fn module_global_fn_value_call_still_checks_args() {
    files_reject(
        &[
            ("k.chz", "fn one() -> int:\n    return 1\n"),
            ("l.chz", "import k\nBARE := k.one\n"),
            ("main.chz", "import l\nprint(l.BARE(5))\n"),
        ],
        "expects 0 argument(s), got 1",
    );
    files_reject(
        &[
            ("k.chz", "fn twice(n: int) -> int:\n    return n * 2\n"),
            ("l.chz", "import k\nTW := k.twice\n"),
            ("main.chz", "import l\nprint(l.TW(\"x\"))\n"),
        ],
        "argument 1 of 'TW': expected int, found str",
    );
    // An int literal into a `float` param through a function VALUE is rejected (D3), and so is the
    // declared-fn spelling of the same call below: no int ever widens into a `float` slot.
    files_reject(
        &[
            ("k.chz", "fn half(x: float) -> float:\n    return x / 2.0\n"),
            ("l.chz", "import k\nFL := k.half\n"),
            ("main.chz", "import l\nprint(l.FL(2))\n"),
        ],
        "argument 1 of 'FL': expected float, found int",
    );
    files_reject(
        &[
            ("k.chz", "fn half(x: float) -> float:\n    return x / 2.0\n"),
            ("main.chz", "import k\nprint(k.half(2))\n"),
        ],
        "write 1.0",
    );
    files_ok(&[
        ("k.chz", "fn half(x: float) -> float:\n    return x / 2.0\n"),
        ("main.chz", "import k\nprint(k.half(2.0))\n"),
    ]);
}

/// A module member whose own initializer already errored is `Unknown`-typed; calling it must stay
/// SILENT (one root-cause error, no cascade), matching the two-step spelling `f := l.X; f()` and the
/// checker's `Ty::Unknown` suppression convention.
#[test]
fn module_unknown_typed_member_call_does_not_cascade() {
    let errs = check_files(&[
        ("k.chz", "fn one() -> int:\n    return 1\n"),
        ("l.chz", "import k\nX := k.nope\n"),
        ("main.chz", "import l\nprint(l.X())\n"),
    ]);
    assert_eq!(errs.len(), 1, "expected only the root cause, got: {errs:?}");
    assert!(
        errs[0].message.contains("module 'k' has no member 'nope'"),
        "unexpected error: {errs:?}"
    );
}

/// A member that EXISTS but is not callable gets a truthful diagnostic — the old one claimed the
/// module had no such member, which was wrong independently of the fn-value fix.
#[test]
fn module_global_non_fn_call_says_not_callable() {
    files_reject(
        &[
            ("l.chz", "N := 7\n"),
            ("main.chz", "import l\nprint(l.N())\n"),
        ],
        "module 'l' member 'N' is not callable (it has type int)",
    );
}

/// A genuinely absent member keeps the original message.
#[test]
fn module_missing_member_call_message_unchanged() {
    files_reject(
        &[
            ("l.chz", "N := 7\n"),
            ("main.chz", "import l\nprint(l.NOPE())\n"),
        ],
        "module 'l' has no member 'NOPE'",
    );
}

/// TICKET-023: a `protocol` declared in one module is unreachable from another module, by either
/// crossing spelling. `ModuleSig` has no `protocol_defs` field (unlike `struct_defs`/`enum_defs`),
/// so a qualified `shapes.Drawable` bound reports "has no type" even though the
/// protocol is declared and importable structurally within its own module.
#[test]
fn protocol_reachable_via_qualified_path() {
    files_ok(&[
        (
            "shapes.chz",
            "protocol Drawable:\n    fn draw(self) -> str\n",
        ),
        (
            "main.chz",
            "import shapes\nfn describe(d: shapes.Drawable) -> str:\n    return d.draw()\n",
        ),
    ]);
}

/// Same gap via the `from`-import spelling.
#[test]
fn protocol_reachable_via_named_import() {
    files_ok(&[
        (
            "shapes.chz",
            "protocol Drawable:\n    fn draw(self) -> str\n",
        ),
        (
            "main.chz",
            "import Drawable from shapes\nfn describe(d: Drawable) -> str:\n    return d.draw()\n",
        ),
    ]);
}

/// A named import may rename the protocol; the bind name is what resolves.
#[test]
fn protocol_named_import_alias_binds_the_protocol() {
    files_ok(&[
        (
            "shapes.chz",
            "protocol Drawable:\n    fn draw(self) -> str\n",
        ),
        (
            "main.chz",
            "import Drawable as D from shapes\nfn describe(d: D) -> str:\n    return d.draw()\n",
        ),
    ]);
}

/// A qualified generic protocol mirrors the bare form: a bare qualified reference is an unbound
/// existential (no arity check), a written arg count must match, and a wrong count still errors.
#[test]
fn protocol_generic_qualified_arity_mirrors_the_bare_form() {
    let shapes = (
        "shapes.chz",
        "protocol Container[T]:\n    fn get(self) -> T\n",
    );
    files_ok(&[
        shapes,
        (
            "main.chz",
            "import shapes\nfn f(c: shapes.Container[int]) -> int:\n    return c.get()\n",
        ),
    ]);
    files_ok(&[
        shapes,
        (
            "main.chz",
            "import shapes\nfn f(c: shapes.Container) -> int:\n    return 1\n",
        ),
    ]);
    files_reject(
        &[
            shapes,
            (
                "main.chz",
                "import shapes\nfn f(c: shapes.Container[int, str]) -> int:\n    return 1\n",
            ),
        ],
        "expects 1 type argument(s), got 2",
    );
}

/// A protocol with a static requirement stays bound-only across the qualified spelling too.
#[test]
fn protocol_with_a_static_method_stays_bound_only_when_qualified() {
    files_reject(
        &[
            (
                "shapes.chz",
                "protocol Maker:\n    fn make(x: int) -> int\n",
            ),
            (
                "main.chz",
                "import shapes\nfn f(m: shapes.Maker) -> int:\n    return 1\n",
            ),
        ],
        "has a static method and can only be used as a bound",
    );
}

/// Exporting a protocol must not widen the module surface for a name that isn't declared.
#[test]
fn protocol_export_does_not_widen_a_missing_member() {
    let shapes = (
        "shapes.chz",
        "protocol Drawable:\n    fn draw(self) -> str\n",
    );
    files_reject(
        &[shapes, ("main.chz", "import Nope from shapes\n")],
        "module 'shapes' has no member 'Nope'",
    );
    files_reject(
        &[
            shapes,
            (
                "main.chz",
                "import shapes\nfn f(x: shapes.Nope) -> int:\n    return 1\n",
            ),
        ],
        "module 'shapes' has no type 'Nope'",
    );
}

/// A protocol still carries no runtime value, even when reached across a module boundary.
#[test]
fn protocol_is_still_not_a_value() {
    files_reject(
        &[
            (
                "shapes.chz",
                "protocol Drawable:\n    fn draw(self) -> str\n",
            ),
            ("main.chz", "import shapes\nd := shapes.Drawable(1)\n"),
        ],
        "module 'shapes' has no member 'Drawable'",
    );
}

/// TICKET-027: two unrelated modules that each declare the SAME protocol name must coexist, exactly
/// like two modules each declaring the same struct name. `hoist_protocol` keys `self.protocols` on
/// the bare name instead of a module-qualified key, so the second module's declaration is rejected
/// as a duplicate even though it lives in a different module.
#[test]
fn same_protocol_name_in_two_modules_does_not_conflict() {
    files_ok(&[
        ("a.chz", "protocol Drawable:\n    fn draw(self) -> str\n"),
        ("b.chz", "protocol Drawable:\n    fn draw(self) -> str\n"),
        ("main.chz", "import a\nimport b\nprint(1)\n"),
    ]);
}

/// TICKET-027: two modules each declaring `Drawable` are distinct types once keyed. A struct that
/// satisfies `a.Drawable` (has `draw`) must be accepted against a bound importing `a`'s `Drawable`
/// and rejected against a bound importing `b`'s `Drawable` (which additionally requires `paint`).
#[test]
fn two_modules_same_protocol_name_are_distinct_types() {
    let a = ("a.chz", "protocol Drawable:\n    fn draw(self) -> str\n");
    let b = ("b.chz", "protocol Drawable:\n    fn paint(self) -> str\n");
    let dot = "struct Dot:\n    n: int\n    fn draw(self) -> str:\n        return \"dot\"\n";
    files_ok(&[
        a,
        (
            "main.chz",
            &format!(
                "import Drawable from a\n{dot}\nfn show[T: Drawable](x: T):\n    print(x.draw())\nshow(Dot(1))\n"
            ),
        ),
    ]);
    files_reject(
        &[
            b,
            (
                "main.chz",
                &format!(
                    "import Drawable from b\n{dot}\nfn show[T: Drawable](x: T):\n    print(x.draw())\nshow(Dot(1))\n"
                ),
            ),
        ],
        "does not satisfy Drawable",
    );
}

/// TICKET-027: keying closes a leak -- a bare, unimported protocol name no longer resolves across a
/// module boundary. Before this change the table was program-global, so `import a` alone made `a`'s
/// `Drawable` visible bare in `main.chz`.
#[test]
fn protocol_is_not_visible_bare_without_an_import() {
    files_reject(
        &[
            ("a.chz", "protocol Drawable:\n    fn draw(self) -> str\n"),
            (
                "main.chz",
                "import a\nfn f(d: Drawable) -> str:\n    return d.draw()\n",
            ),
        ],
        "unknown type 'Drawable'",
    );
}

/// TICKET-027 ceiling pin: the dup guard must stay re-keyed, not deleted -- a SAME-module
/// redeclaration is still an error.
#[test]
fn same_module_duplicate_protocol_is_still_an_error() {
    files_reject(
        &[(
            "main.chz",
            "protocol Drawable:\n    fn draw(self) -> str\n\nprotocol Drawable:\n    fn draw(self) -> str\n",
        )],
        "protocol 'Drawable' is already defined",
    );
}

/// TICKET-027 ceiling pin: a reserved protocol bound needs no import in ANY module -- the 21
/// `RESERVED_PROTOCOLS` are never keyed.
#[test]
fn reserved_protocol_bound_needs_no_import_in_any_module() {
    files_ok(&[
        ("a.chz", "fn pick[T: Eq](x: T, y: T) -> T:\n    return x\n"),
        ("b.chz", "fn pick[T: Eq](x: T, y: T) -> T:\n    return x\n"),
        ("main.chz", "import a\nimport b\nprint(1)\n"),
    ]);
}

/// TICKET-027 ceiling pin: a qualified protocol with a transitively-embedded static requirement
/// stays bound-only. This pins that step 10's fix re-identifies the protocol by KEY rather than
/// re-implementing `protocol_has_static_method`'s embed flatten.
#[test]
fn qualified_embed_static_requirement_stays_bound_only() {
    files_reject(
        &[
            ("shapes.chz", "protocol MakeInt:\n    Convert[int]\n"),
            (
                "main.chz",
                "import shapes\nfn f(m: shapes.MakeInt) -> int:\n    return 1\n",
            ),
        ],
        "has a static method and can only be used as a bound",
    );
}

/// TICKET-027 ceiling pin (step 15 funnel: `StructInfo::type_params`): a generic struct's stored
/// protocol bound must cross an import re-spelled to a key, not bare.
#[test]
fn imported_generic_struct_with_a_user_protocol_bound_still_checks() {
    let lib = (
        "lib.chz",
        "protocol Describable:\n    fn describe(self) -> str\n\nstruct Box[T: Describable]:\n    v: T\n",
    );
    files_ok(&[
        lib,
        (
            "main.chz",
            "import Box from lib\n\nstruct Dot:\n    n: int\n    fn describe(self) -> str:\n        return \"dot\"\n\nfn main():\n    b := Box[Dot](Dot(1))\n    print(b.v.describe())\nmain()\n",
        ),
    ]);
    files_reject(
        &[
            lib,
            (
                "main.chz",
                "import Box from lib\n\nstruct Dot:\n    n: int\n\nfn main():\n    b := Box[Dot](Dot(1))\n    print(b.v.n)\nmain()\n",
            ),
        ],
        "does not satisfy Describable",
    );
}

/// TICKET-027 ceiling pin (step 15 funnel: `enum_type_params`): a generic enum's stored protocol
/// bound must cross an import re-spelled to a key, not bare.
#[test]
fn imported_generic_enum_with_a_user_protocol_bound_still_checks() {
    let lib = (
        "lib.chz",
        "protocol Describable:\n    fn describe(self) -> str\n\nenum Eb[T: Describable]:\n    Empty\n    Has(T)\n",
    );
    files_ok(&[
        lib,
        (
            "main.chz",
            "import Eb from lib\n\nstruct Dot:\n    n: int\n    fn describe(self) -> str:\n        return \"dot\"\n\nfn main():\n    e: Eb[Dot] = Eb.Has(Dot(1))\n    print(1)\nmain()\n",
        ),
    ]);
    files_reject(
        &[
            lib,
            (
                "main.chz",
                "import Eb from lib\n\nstruct Dot:\n    n: int\n\nfn main():\n    e: Eb[Dot] = Eb.Has(Dot(1))\n    print(1)\nmain()\n",
            ),
        ],
        "does not satisfy Describable",
    );
}

// ===== W7-24 — an interpolation fragment is a normalized call site like any other =====

/// Named args / defaults / variadic sweeping reach a call inside `"{…}"`. Before `desugar` parsed
/// fragments (they were re-parsed AFTER the normalization pass), every one of these was a type
/// error inside a string while the identical call outside one type-checked.
#[test]
fn interpolation_fragment_call_args_are_normalized() {
    ok_desugared("fn f(a: int, b: int = 2) -> int:\n    return a + b\nprint(\"{f(1)}\")\n");
    ok_desugared("fn s(x: int, y: int) -> int:\n    return x - y\nprint(\"{s(y=1, x=10)}\")\n");
    ok_desugared("fn v(...xs: int) -> int:\n    return xs.len()\nprint(\"{v(1, 2, 3)}\")\n");
    ok_desugared("struct P:\n    x: int\n    y: int = 5\nprint(\"{P(x=1).y}\")\n");
}

/// …and the rule did not go permissive: a genuinely wrong-arity fragment call is still rejected,
/// now with the ACCURATE count (the pre-fix diagnostic for a named-arg call said "got 0", because
/// the named args had never been converted to positional ones).
#[test]
fn interpolation_fragment_wrong_arity_still_rejected_with_real_count() {
    rejects_desugared(
        "fn f(a: int, b: int) -> int:\n    return a + b\nprint(\"{f(1)}\")\n",
        "expects 2 argument(s), got 1",
    );
    rejects_desugared(
        "fn f(a: int, b: int) -> int:\n    return a + b\nprint(\"{f(1, 2, 3)}\")\n",
        "expects 2 argument(s), got 3",
    );
}

/// An unknown named argument inside a fragment is now caught by `desugar` itself — the same error,
/// from the same pass, as the identical call outside a string (it used to reach the checker as a
/// raw `Call` with `named` still populated, which the checker counted as "got 0" positional args).
#[test]
fn interpolation_fragment_unknown_named_arg_rejected_by_the_checker() {
    rejects(
        "fn f(a: int, b: int = 2) -> int:\n    return a + b\nprint(\"{f(nope=1)}\")\n",
        "unknown named argument 'nope'",
    );
}

/// A local binding shadows the module fn, so a fragment call through it is a plain value call and
/// must NOT have the fn's default spliced in — the scope-awareness `desugar` already applies
/// outside strings, applied inside one.
#[test]
fn interpolation_fragment_respects_local_shadowing() {
    ok_desugared(
        "fn f(a: int, b: int = 2) -> int:\n    return a + b\nfn g():\n    f := fn(a: int) -> int: a * 100\n    print(\"{f(1)}\")\ng()\n",
    );
}

/// The parser builds `ExprKind::Interp` itself, so a caller that skips `desugar::run` still
/// type-checks its fragments; a malformed interpolation fails in the lexer, before the checker.
#[test]
fn interpolation_fragment_checked_without_desugar() {
    rejects("print(\"{nope}\")\n", "unknown name 'nope'");
    front_rejects("print(\"a}b\")\n", "unmatched '}'");
}
