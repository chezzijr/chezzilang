//! Checker tests: M24 static witnesses and forwarding.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== M24 slice A — static-witness passing: a protocol's STATIC (no-`self`) requirement is
// callable through a generic bound (`T.default()` inside `fn reset[T: Default](old: T) -> T`).
// The concrete type's runtime identity key rides as a hidden trailing argument on the generic fn.
//
// SCOPE of slice A: a SAME-MODULE call to a MODULE-LEVEL free generic fn whose `T` is CONCRETE at
// the call site. Every other shape is a deliberate, clear checker error — pinned below so a later
// slice that lifts one has to come here and say so. The RUNNING half (that the checker's identity
// key equals the VM's runtime key) is `tests/chz/spec/static_witness_test.chz`.

/// The accepted surface. Each of these also RUNS in `tests/chz/spec/static_witness_test.chz`; here
/// they only pin that the checker admits them.
#[test]
fn witness_static_call_accepted_shapes_ok() {
    let dflt = "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\nfn reset[T: Default](old: T) -> T:\n    return T.default()\n";
    // the driver shape
    entry_ok(&format!(
        "{dflt}fn main():\n    print(reset(Counter(9)).n)\nmain()\n"
    ));
    // an ENUM can host the static requirement too (not just a struct)
    entry_ok(
        "protocol Default:\n    fn default() -> Self\nenum Col:\n    Red\n    Blue\n    fn default() -> Col:\n        return Col.Blue\nfn reset[T: Default](old: T) -> T:\n    return T.default()\nfn main():\n    print(reset(Col.Red))\nmain()\n",
    );
    // a static requirement WITH ARGUMENTS
    entry_ok(
        "protocol Conv:\n    fn conv(x: int) -> Self\nstruct Wrap:\n    v: int\n    fn conv(x: int) -> Wrap:\n        return Wrap(x * 2)\nfn mk[T: Conv](seed: T, n: int) -> T:\n    return T.conv(n)\nfn main():\n    print(mk(Wrap(0), 21).v)\nmain()\n",
    );
    // the RESERVED `Convert[S]` protocol — the slice-3 close-out this milestone re-opened
    entry_ok(
        "struct Port:\n    v: int\n    fn convert(x: int) -> Port:\n        return Port(x + 1)\nfn make[T: Convert[int]](seed: T, n: int) -> T:\n    return T.convert(n)\nfn main():\n    print(make(Port(0), 41).v)\nmain()\n",
    );
    // no value argument at all: `T` pinned by a turbofish…
    entry_ok(&format!(
        "{dflt}fn empty[T: Default]() -> T:\n    return T.default()\nfn main():\n    print(empty[Counter]().n)\nmain()\n"
    ));
    // …or by an annotated result (the alternative the "not determined here" error suggests)
    entry_ok(&format!(
        "{dflt}fn empty[T: Default]() -> T:\n    return T.default()\nfn main():\n    x: Counter = empty()\n    print(x.n)\nmain()\n"
    ));
    // TWO witness params on one fn — the hidden args are positional, so their order must survive
    entry_ok(&format!(
        "{dflt}struct Other:\n    m: int\n    fn default() -> Other:\n        return Other(7)\nfn two[T: Default, U: Default](a: T, b: U) -> T:\n    y := U.default()\n    return T.default()\nfn main():\n    print(two(Counter(1), Other(2)).n)\nmain()\n"
    ));
    // The CALL SITE (not the witness call) inside a closure / `spawn:` / `defer:`: `T` is concrete
    // there, so the witness is just a constant pushed at that site — nothing has to be captured.
    entry_ok(&format!(
        "{dflt}fn main():\n    c := Counter(9)\n    f := fn(): reset(c).n\n    print(f())\nmain()\n"
    ));
    entry_ok(&format!(
        "{dflt}fn main():\n    c := Counter(9)\n    ch := Channel[int](4)\n    parallel:\n        spawn:\n            ch.send(reset(c).n)\n    print(ch.recv())\nmain()\n"
    ));
    entry_ok(&format!(
        "{dflt}fn main():\n    c := Counter(9)\n    defer:\n        print(reset(c).n)\n    print(1)\nmain()\n"
    ));
    // nested call, and a call inside a string-interpolation fragment — both exercise the per-call
    // witness key, which is the seam most likely to collide.
    entry_ok(&format!(
        "{dflt}fn main():\n    c := Counter(9)\n    print(reset(reset(c)).n)\n    print(\"{{reset(c).n}}\")\nmain()\n"
    ));
}

/// Task 3 — the witness crosses a MODULE boundary. `WitnessTable::fns` is keyed by the DECLARING
/// module and `calls` by the CALLING one, so a call site must resolve its callee's requirement
/// through the import binding that named it (a module bind for `lib.reset(...)`, a `from` bind for
/// the bare spelling). Both spellings reach the same callee, and both must lower.
/// (Its RUNNING half — that the threaded key is the OWNING module's identity key — is
/// `witness_cross_module_runs_both_engines` in `src/vm/tests.rs`.)
#[test]
fn witness_static_call_cross_module_ok() {
    let lib = (
        "lib.chz",
        "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(7)\nfn reset[T: Default](old: T) -> T:\n    return T.default()\n",
    );
    // the module-QUALIFIED spelling, bare and turbofished
    files_ok(&[
        lib,
        (
            "main.chz",
            "import lib\nfn main():\n    print(lib.reset(lib.Counter(9)).n)\n    print(lib.reset[lib.Counter](lib.Counter(9)).n)\nmain()\n",
        ),
    ]);
    // the `from`-imported spelling, bare / turbofished / under an alias
    files_ok(&[
        lib,
        (
            "main.chz",
            "import reset, Counter from lib\nimport reset as again from lib\nfn main():\n    print(reset(Counter(9)).n)\n    print(reset[Counter](Counter(9)).n)\n    print(again(Counter(9)).n)\nmain()\n",
        ),
    ]);
    // …and a forwarding LOCAL generic feeding an IMPORTED one: the caller's own `$w:U` local fills
    // the imported callee's slot, so forwarding crosses the boundary too (both spellings). The bound
    // names the IMPORTED protocol — TICKET-027 module-scopes it, so it must now be imported by name.
    files_ok(&[
        lib,
        (
            "main.chz",
            "import lib\nimport reset, Default from lib\nfn twice[U: Default](x: U) -> U:\n    return reset(lib.reset(x))\nfn main():\n    print(twice(lib.Counter(9)).n)\nmain()\n",
        ),
    ]);
    // the REVERSE direction — a LOCAL type witnessing an IMPORTED generic's bound (its identity key
    // is the CALLING module's), and an IMPORTED type witnessing a LOCAL generic's bound.
    files_ok(&[
        lib,
        (
            "main.chz",
            "import lib\nimport reset, Default from lib\nstruct Local:\n    k: str\n    fn default() -> Local:\n        return Local(\"loc\")\nfn mine[T: Default](x: T) -> T:\n    return T.default()\nfn main():\n    print(reset(Local(\"x\")).k)\n    print(mine(lib.Counter(1)).n)\nmain()\n",
        ),
    ]);
}

/// M24-5 — `defer f(...)` pushes the hidden witness at its own emit site like any other call, so a
/// witness-taking callee is a legal target in the CROSS-MODULE spellings too (Go accepts the same
/// program: `defer reset(c, "defer")` on a `func reset[T Defaulter[T]]`). The `spawn` twin agrees
/// now: a module is a NAMESPACE, so `spawn lib.reset(...)` is a plain call, not a spawn on a
/// non-sendable module receiver (the RUNNING proof is
/// `tests/chz/spec/spawn_module_target_test.chz`).
#[test]
fn witness_fn_is_a_cross_module_defer_target_ok() {
    let lib = (
        "lib.chz",
        "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(7)\nfn reset[T: Default](old: T) -> T:\n    return T.default()\n",
    );
    files_ok(&[
        lib,
        (
            "main.chz",
            "import lib\nfn main():\n    defer lib.reset(lib.Counter(9))\n    print(1)\nmain()\n",
        ),
    ]);
    files_ok(&[
        lib,
        (
            "main.chz",
            "import reset, Counter from lib\nfn main():\n    defer reset(Counter(9))\n    print(1)\nmain()\n",
        ),
    ]);
    files_ok(&[
        lib,
        (
            "main.chz",
            "import lib\nfn main():\n    parallel:\n        spawn lib.reset(lib.Counter(9))\nmain()\n",
        ),
    ]);
}

/// Generic-calls-generic with the caller's `U` still abstract: slice 2 FORWARDS the caller's own
/// `$w:U` local instead of a constant, so this is now accepted. (Its runtime half — that the value
/// forwarded is the OUTERMOST concrete type — is `tests/chz/spec/static_witness_test.chz`; the
/// shapes that still cannot forward are `witness_forwarding_*_rejected` below.)
#[test]
fn witness_static_call_forwarding_ok() {
    entry_ok(
        "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\nfn reset[T: Default](old: T) -> T:\n    return T.default()\nfn g[U: Default](x: U) -> U:\n    return reset(x)\nfn main():\n    print(g(Counter(1)).n)\nmain()\n",
    );
}

/// TICKET-056 — the explicit-turbofish spelling of a static-call forward
/// (`inner[T](n)`) is rejected even though `T` is declared by the enclosing
/// FUNCTION (`outer`) and the call site is directly in that body — exactly
/// the condition the diagnostic says carries a witness. `CallSite` (unlike
/// `MemberCall`) has no `type_args` field, so `call_forwards_a_witness`
/// cannot see the turbofish and never charges `outer`'s `T`.
#[test]
fn witness_static_call_forwarding_via_turbofish_ok() {
    entry_ok(
        "protocol Mk:\n    fn make(n: int) -> Self\n    fn val(self) -> int\nstruct B:\n    n: int\n    fn make(n: int) -> B:\n        return B(n * 10)\n    fn val(self) -> int:\n        return self.n\nfn inner[T: Mk](n: int) -> int:\n    return T.make(n).val()\nfn outer[T: Mk](n: int) -> int:\n    return inner[T](n)\nfn main():\n    print(outer[B](4))\nmain()\n",
    );
}

/// TICKET-056 — the turbofish forward now charges from every position it can be written in: the
/// enclosing declarer can be a MEMBER, the CALLEE can be a member turbofish, and the forward can sit
/// inside a nested `fn` or a closure. All four share the `Mk` preamble above.
#[test]
fn witness_forwarding_via_turbofish_from_every_position_ok() {
    let head = "protocol Mk:\n    fn make(n: int) -> Self\n    fn val(self) -> int\nstruct B:\n    n: int\n    fn make(n: int) -> B:\n        return B(n * 10)\n    fn val(self) -> int:\n        return self.n\nfn inner[T: Mk](n: int) -> int:\n    return T.make(n).val()\n";
    // (a) the enclosing declarer is a MEMBER
    entry_ok(&format!(
        "{head}struct Host:\n    k: int\n    fn go[T: Mk](self, n: int) -> int:\n        return inner[T](n)\nfn main():\n    print(Host(1).go[B](4))\nmain()\n"
    ));
    // (b) the CALLEE is a member turbofish
    entry_ok(&format!(
        "{head}struct Host:\n    k: int\n    fn build[T: Mk](self, n: int) -> int:\n        return T.make(n).val()\nfn outer[T: Mk](h: Host, n: int) -> int:\n    return h.build[T](n)\nfn main():\n    print(outer[B](Host(1), 4))\nmain()\n"
    ));
    // (c) the forward sits inside a nested `fn`
    entry_ok(&format!(
        "{head}fn outer[T: Mk](n: int) -> int:\n    fn nest(m: int) -> int:\n        return inner[T](m)\n    return nest(n)\nfn main():\n    print(outer[B](4))\nmain()\n"
    ));
    // (d) the forward sits inside a closure
    entry_ok(&format!(
        "{head}fn outer[T: Mk](n: int) -> int:\n    f := fn(m: int) -> int: inner[T](m)\n    return f(n)\nfn main():\n    print(outer[B](4))\nmain()\n"
    ));
}

/// TICKET-056 — the widening cannot reach a TYPE-declared param: `static_bounded_type_params` reads
/// `decl.type_params`, which for a member `go(self)` with no `[T]` of its own is empty, so
/// `witness_params_of` returns before the turbofish charge runs.
#[test]
fn witness_forwarding_via_turbofish_of_an_enclosing_type_param_rejected() {
    entry_rejects(
        "protocol Mk:\n    fn make(n: int) -> Self\n    fn val(self) -> int\nstruct B:\n    n: int\n    fn make(n: int) -> B:\n        return B(n * 10)\n    fn val(self) -> int:\n        return self.n\nfn inner[T: Mk](n: int) -> int:\n    return T.make(n).val()\nstruct Bx[T: Mk]:\n    v: int\n    fn go(self) -> int:\n        return inner[T](self.v)\nfn main():\n    print(1)\nmain()\n",
        "A type parameter of an enclosing TYPE",
    );
}

/// TICKET-056 — an unbounded `T` is not a candidate at all, so a turbofish naming it is charged
/// nothing and the failure stays the ordinary bound failure.
#[test]
fn witness_forwarding_via_turbofish_unbounded_param_rejected() {
    entry_rejects(
        "protocol Mk:\n    fn make(n: int) -> Self\n    fn val(self) -> int\nstruct B:\n    n: int\n    fn make(n: int) -> B:\n        return B(n * 10)\n    fn val(self) -> int:\n        return self.n\nfn inner[T: Mk](n: int) -> int:\n    return T.make(n).val()\nfn outer[T](n: int) -> int:\n    return inner[T](n)\nfn main():\n    print(outer[B](4))\nmain()\n",
        "does not satisfy",
    );
}

/// TICKET-056 — the witness-unreachable diagnostic is now split on who DECLARED the type parameter.
/// A LOCAL-carried member forward (`v := x` then `h.build(v)`, `v` is not a parameter so it is not
/// charged) is the one shape that still reaches the guard with a function-declared `T`, and gets the
/// new message naming a working turbofish spelling. A TYPE-declared param keeps the old message.
#[test]
fn witness_unreachable_names_the_declarer_of_the_type_param() {
    let function_declared = "protocol Mk:\n    fn make(n: int) -> Self\n    fn val(self) -> int\nstruct B:\n    n: int\n    fn make(n: int) -> B:\n        return B(n * 10)\n    fn val(self) -> int:\n        return self.n\nstruct Host:\n    k: int\n    fn build[U: Mk](self, y: U) -> int:\n        return U.make(1).val()\nfn outer[T: Mk](x: T, h: Host) -> int:\n    v := x\n    return h.build(v)\nfn main():\n    print(1)\nmain()\n";
    let errs = check_entry(function_declared);
    assert!(
        errs.iter().any(|e| e
            .message
            .contains("IS declared by the enclosing function or member")),
        "expected the function-declared message, got: {errs:?}"
    );
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("A type parameter of an enclosing TYPE")),
        "the function-declared shape must not get the TYPE-declared message, got: {errs:?}"
    );

    entry_rejects(
        "protocol Mk:\n    fn make(n: int) -> Self\n    fn val(self) -> int\nstruct B:\n    n: int\n    fn make(n: int) -> B:\n        return B(n * 10)\n    fn val(self) -> int:\n        return self.n\nfn inner[T: Mk](n: int) -> int:\n    return T.make(n).val()\nstruct Bx[T: Mk]:\n    v: int\n    fn go(self) -> int:\n        return inner[T](self.v)\nfn main():\n    print(1)\nmain()\n",
        "A type parameter of an enclosing TYPE",
    );
}

/// M24 Task 4 — the `$w:T` binding REACHES a nested body: `with_witness_captures` appends it to the
/// capture entries of every closure, nested `fn`, `spawn:` block and `defer:` block, so `T.static()`
/// inside one lowers to the same `CallStaticDyn`. The witness is a `str`, so it crosses BY VALUE —
/// an escaping closure and the `spawn:` airlock both stay correct. The RUNNING half (that the value
/// captured is the right type) is `tests/chz/spec/static_witness_test.chz`.
#[test]
fn witness_static_call_inside_a_nested_body_ok() {
    let head = "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\n";
    // a closure body — including one that ESCAPES its defining frame
    entry_ok(&format!(
        "{head}fn build[T: Default](x: T) -> fn() -> T:\n    return fn(): T.default()\nfn main():\n    print(build(Counter(1))().n)\nmain()\n"
    ));
    // a `spawn:` block (behind the airlock — a `str` deep-copies across it)
    entry_ok(&format!(
        "{head}fn go[T: Default](x: T) -> int:\n    parallel:\n        spawn:\n            y := T.default()\n    return 1\nfn main():\n    print(go(Counter(1)))\nmain()\n"
    ));
    // a `defer:` block (its own child proto)
    entry_ok(&format!(
        "{head}fn go[T: Default](x: T) -> int:\n    defer:\n        y := T.default()\n    return 1\nfn main():\n    print(go(Counter(1)))\nmain()\n"
    ));
    // a nested `fn` (lowered as a closure)
    entry_ok(&format!(
        "{head}fn go[T: Default](x: T) -> T:\n    fn inner() -> T:\n        return T.default()\n    return inner()\nfn main():\n    print(go(Counter(1)).n)\nmain()\n"
    ));
    // a closure inside a closure — the inner one reads the middle frame's CAPTURE (`CapSrc::Captured`)
    entry_ok(&format!(
        "{head}fn go[T: Default](x: T) -> fn() -> fn() -> T:\n    return fn(): fn(): T.default()\nfn main():\n    print(go(Counter(1))()().n)\nmain()\n"
    ));
    // a closure inside a METHOD body, over the member-level `[T]` (Task 5's witness, Task 4's reach)
    entry_ok(&format!(
        "{head}struct Holder:\n    k: int\n    fn go[T: Default](self, x: T) -> T:\n        f := fn() -> T: T.default()\n        return f()\nfn main():\n    print(Holder(1).go(Counter(2)).n)\nmain()\n"
    ));
    // TWO witnesses, both reached from one nested body — the hidden params are positional, so the
    // capture entries must keep them apart.
    entry_ok(&format!(
        "{head}struct Tag:\n    s: str\n    fn default() -> Tag:\n        return Tag(\"none\")\nfn both[T: Default, U: Default](a: T, b: U) -> str:\n    f := fn() -> str: \"{{T.default()}}{{U.default()}}\"\n    return f()\nfn main():\n    print(both(Counter(1), Tag(\"x\")))\nmain()\n"
    ));
}

/// …and what a nested body still cannot reach: a type parameter of the enclosing TYPE
/// (`struct Bx[T]`) has no witness ANYWHERE — Task 4 widens which bodies see a witness, never which
/// declarations have one — so the member-level hint stays the answer, inside a closure too.
#[test]
fn witness_static_call_for_an_enclosing_type_param_still_rejected() {
    let head = "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\n";
    let needle = "is a type parameter of the enclosing type";
    entry_rejects(
        &format!(
            "{head}struct Bx[T: Default]:\n    v: T\n    fn get(self) -> T:\n        return T.default()\nfn main():\n    print(1)\nmain()\n"
        ),
        needle,
    );
    entry_rejects(
        &format!(
            "{head}struct Bx[T: Default]:\n    v: T\n    fn get(self) -> T:\n        f := fn() -> T: T.default()\n        return f()\nfn main():\n    print(1)\nmain()\n"
        ),
        needle,
    );
    // A NESTED `fn` in that method resets `current_self_ty`, so `enclosing_type_declaring` cannot
    // name the host and the GENERAL diagnostic answers instead. It has to be true here too — it may
    // not tell the user that a nested `fn` carries a witness while refusing one inside a nested
    // `fn`. (This is the only shape that reaches it, so it is also its only coverage.)
    entry_rejects(
        &format!(
            "{head}struct Bx[T: Default]:\n    v: T\n    fn get(self) -> T:\n        fn inner() -> T:\n            return T.default()\n        return inner()\nfn main():\n    print(1)\nmain()\n"
        ),
        "A type parameter of an enclosing TYPE",
    );
}

/// M24-5 — `spawn f(..)` / `defer f(..)` push the hidden witness at their own emit sites (widening
/// `Op::SpawnCall`/`DeferCall`'s `argc`), so a witness-taking callee is an ordinary target. Go is
/// the ancestor and accepts both (`go reset(c, "go")` / `defer reset(c, "defer")`, measured
/// 2026-08-14). The RUNNING half — that the type constructed is the caller's — is
/// `tests/chz/spec/static_witness_test.chz`.
#[test]
fn witness_fn_is_a_spawn_or_defer_target_ok() {
    let head = "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\nfn reset[T: Default](old: T) -> T:\n    return T.default()\n";
    entry_ok(&format!(
        "{head}fn main():\n    c := Counter(9)\n    parallel:\n        spawn reset(c)\nmain()\n"
    ));
    entry_ok(&format!(
        "{head}fn main():\n    c := Counter(9)\n    defer reset(c)\n    print(1)\nmain()\n"
    ));
    // the KEYWORD spelling — desugar normalizes it into the positional slot, so the hidden witness
    // still rides last
    entry_ok(&format!(
        "{head}fn main():\n    c := Counter(9)\n    defer reset(old=c)\n    print(1)\nmain()\n"
    ));
}

/// PERMANENT: a function VALUE erases the concrete type, and the witness is exactly that type — so a
/// witness-taking fn can never be one. Both spellings (a binding and a HOF argument) are refused.
#[test]
fn witness_fn_as_function_value_rejected() {
    let head = "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\nfn reset[T: Default](old: T) -> T:\n    return T.default()\n";
    let needle = "cannot be used as a function value";
    entry_rejects(
        &format!("{head}fn main():\n    f := reset\n    print(1)\nmain()\n"),
        needle,
    );
    entry_rejects(
        &format!(
            "{head}fn apply(f: fn(Counter) -> Counter, c: Counter) -> Counter:\n    return f(c)\nfn main():\n    print(apply(reset, Counter(3)).n)\nmain()\n"
        ),
        needle,
    );
    // …and the TURBOFISH value spelling (`reset[Counter]`), which reaches a different value path
    // (`infer_index`'s Scope-B pin). Pinning `T` in the TYPE does not recover the witness — the pin
    // is checker-only and the runtime value is the same erased function — so it is refused too.
    // Both spellings: a binding and a HOF argument.
    entry_rejects(
        &format!("{head}fn main():\n    g := reset[Counter]\n    print(1)\nmain()\n"),
        needle,
    );
    entry_rejects(
        &format!(
            "{head}fn apply(f: fn(Counter) -> Counter, c: Counter) -> Counter:\n    return f(c)\nfn main():\n    print(apply(reset[Counter], Counter(3)).n)\nmain()\n"
        ),
        needle,
    );
    // …and the CROSS-MODULE read (`lib.empty`), which reaches the module-member path. A
    // return-only-generic makes this reachable: nothing downstream rejects the erased `Ty::Func`.
    files_reject(
        &[
            (
                "lib.chz",
                "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\nfn empty[T: Default]() -> T:\n    return T.default()\n",
            ),
            (
                "main.chz",
                "import lib\nfn main():\n    g := lib.empty\n    print(1)\nmain()\n",
            ),
        ],
        needle,
    );
}

/// A witness parameter is ARITY, so it is charged only to a fn that actually calls `T.static()`. A
/// generic whose bound merely CONTAINS a static requirement but whose body never constructs keeps
/// every position a plain generic fn has — all of them legal before M24.
#[test]
fn witness_only_charged_when_the_body_uses_it_ok() {
    // `Spawnable` has a static requirement, but `label` only calls an INSTANCE method: value
    // position (binding + HOF argument), `spawn`/`defer` target, and a same-module call all stay.
    let head = "protocol Spawnable:\n    fn make() -> Self\n    fn tag(self) -> str\nstruct N:\n    v: int\n    fn make() -> N:\n        return N(0)\n    fn tag(self) -> str:\n        return \"n\"\nfn label[T: Spawnable](x: T) -> str:\n    return x.tag()\n";
    entry_ok(&format!(
        "{head}fn apply(f: fn(N) -> str, x: N) -> str:\n    return f(x)\nfn main():\n    g := label[N]\n    print(apply(label, N(2)))\n    print(label[N](N(3)))\nmain()\n"
    ));
    entry_ok(&format!(
        "{head}fn main():\n    defer label(N(1))\n    parallel:\n        spawn label(N(2))\nmain()\n"
    ));
    // …and it is callable ACROSS a module boundary, which needs no witness threading.
    files_ok(&[
        (
            "lib.chz",
            "protocol Spawnable:\n    fn make() -> Self\n    fn tag(self) -> str\nstruct N:\n    v: int\n    fn make() -> N:\n        return N(0)\n    fn tag(self) -> str:\n        return \"n\"\nfn label[T: Spawnable](x: T) -> str:\n    return x.tag()\n",
        ),
        (
            "main.chz",
            "import lib\nfn main():\n    print(lib.label(lib.N(2)))\nmain()\n",
        ),
    ]);
    // The same for the RESERVED `Convert[S]` bound with a body that never converts.
    entry_ok(
        "struct Port:\n    v: int\n    fn convert(x: int) -> Port:\n        return Port(x + 1)\nfn wrap[T: Convert[int]](seed: T, n: int) -> T:\n    return seed\nfn main():\n    g := wrap[Port]\n    print(wrap(Port(1), 2).v)\nmain()\n",
    );
}

/// PERMANENT: if the call site does not pin `T` there is no key to pass. (`empty[Counter]()` and the
/// annotated-result form are the two fixes, both pinned `_ok` above.)
#[test]
fn witness_type_param_unpinned_at_call_site_rejected() {
    entry_rejects(
        "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\nfn empty[T: Default]() -> T:\n    return T.default()\nfn main():\n    x := empty()\n    print(1)\nmain()\n",
        "is not determined here",
    );
}

/// The narrowed pre-M24 error: a bound exists, but no bound's protocol declares that static method,
/// so it is still an erased call with nothing to dispatch to. (`[T]` with no bound at all is the
/// same verdict — pinned in `static_call_through_type_param_is_clear_error`.)
#[test]
fn witness_bound_without_that_static_method_rejected() {
    entry_rejects(
        "fn bad[T: Comparable](x: T) -> int:\n    y := T.empty()\n    return 1\nfn main():\n    print(bad(1))\nmain()\n",
        "no bound on 'T' declares a static method 'empty'",
    );
}

/// REGRESSION GUARD — the feature appends a HIDDEN parameter, so everything that does NOT need one
/// must keep its exact arity and its exact freedoms.
#[test]
fn non_witness_generics_unaffected_by_hidden_param_ok() {
    // A NON-static bound (`Comparable` requires `fn compare(self, ..)`) takes no witness: still
    // callable, still passable as a function VALUE, arity unchanged.
    entry_ok(
        "fn biggest[T: Comparable](a: T, b: T) -> T:\n    if a < b:\n        return b\n    return a\nfn apply2(f: fn(int, int) -> int, a: int, b: int) -> int:\n    return f(a, b)\nfn main():\n    print(biggest(3, 7))\n    g := biggest[int]\n    print(apply2(biggest, 4, 1))\nmain()\n",
    );
    // An UNBOUNDED generic fn is untouched.
    entry_ok(
        "fn ident[T](x: T) -> T:\n    return x\nfn main():\n    print(ident(\"hi\"))\nmain()\n",
    );
    // A NESTED `fn` sharing a module-level witness fn's name must NOT inherit its arity: only
    // module-level free fns are recorded, so this `reset` stays a plain 1-arg fn.
    entry_ok(
        "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\nfn reset[T: Default](old: T) -> T:\n    return T.default()\nfn shadow() -> int:\n    fn reset(k: int) -> int:\n        return k * 10\n    return reset(4)\nfn main():\n    print(shadow())\n    print(reset(Counter(9)).n)\nmain()\n",
    );
}

// ===== M24 Task 5 — a witness for a type param declared BY A MEMBER: `fn make[T: Default](self,
// old: T) -> T` on a struct/enum, and the static form `fn build[T: Default](old: T) -> T` reached as
// `Holder.build(x)`. The hidden argument rides on the MEMBER's frame exactly as on a free fn's.
//
// SCOPE: a type param the MEMBER declares. A type param of the ENCLOSING TYPE (`struct Bx[T]`) is
// OUT and stays a clear error — its witness would have to live in every instance, a different
// mechanism. The RUNNING half is `tests/chz/spec/static_witness_test.chz` (each accepted shape with
// TWO concrete types, so a wrong witness cannot pass by coincidence).

/// The accepted member-level surface.
#[test]
fn witness_member_declared_param_ok() {
    let head = "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\nstruct Tag:\n    s: str\n    fn default() -> Tag:\n        return Tag(\"none\")\n";
    // an INSTANCE method declaring its own witnessed `[T]`…
    entry_ok(&format!(
        "{head}struct Holder:\n    k: int\n    fn make[T: Default](self, old: T) -> T:\n        return T.default()\nfn main():\n    print(Holder(1).make(Counter(9)).n)\n    print(Holder(1).make(Tag(\"x\")).s)\nmain()\n"
    ));
    // …a STATIC method (no `self`), reached as `Type.method(...)` and with a method turbofish
    entry_ok(&format!(
        "{head}struct Holder:\n    k: int\n    fn build[T: Default](old: T) -> T:\n        return T.default()\nfn main():\n    print(Holder.build(Counter(9)).n)\n    print(Holder.build[Tag](Tag(\"x\")).s)\nmain()\n"
    ));
    // …an ENUM host, both forms
    entry_ok(&format!(
        "{head}enum Host:\n    One\n    fn make[T: Default](self, old: T) -> T:\n        return T.default()\n    fn build[T: Default](old: T) -> T:\n        return T.default()\nfn main():\n    print(Host.One.make(Counter(9)).n)\n    print(Host.build(Tag(\"x\")).s)\nmain()\n"
    ));
    // …a GENERIC host: the MEMBER's `[T]` is witnessed, the enclosing `[K]` is not (and needs no
    // witness — nothing constructs through it), incl. the type-level turbofish static spelling.
    entry_ok(&format!(
        "{head}struct Gen[K]:\n    k: K\n    fn make[T: Default](self, old: T) -> T:\n        return T.default()\n    fn build[T: Default](old: T) -> T:\n        return T.default()\nfn main():\n    print(Gen(7).make(Counter(9)).n)\n    print(Gen[int].build(Tag(\"x\")).s)\nmain()\n"
    ));
    // …a witness param that is NOT the leading one: the hidden args are positional and trail the
    // declared ones, so `[A]` (unwitnessed) must not consume `[T]`'s slot.
    entry_ok(&format!(
        "{head}struct Holder:\n    k: int\n    fn tail[A, T: Default](self, a: A, t: T) -> T:\n        return T.default()\nfn main():\n    print(Holder(1).tail(\"a\", Counter(9)).n)\nmain()\n"
    ));
    // …a member FORWARDING its own witness into a free generic fn (the charge is a fixpoint over the
    // module's free fns, so it holds with `reset` declared AFTER the struct)
    entry_ok(&format!(
        "{head}struct Holder:\n    k: int\n    fn fwd[T: Default](self, x: T) -> T:\n        return reset(x)\nfn reset[T: Default](old: T) -> T:\n    return T.default()\nfn main():\n    print(Holder(1).fwd(Counter(9)).n)\nmain()\n"
    ));
    // …and ACROSS a module boundary, in every spelling: an imported host with a local witness type,
    // and a local call on an imported type.
    let lib = (
        "lib.chz",
        "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(7)\nstruct Holder:\n    v: int\n    fn make[T: Default](self, old: T) -> T:\n        return T.default()\n    fn build[T: Default](old: T) -> T:\n        return T.default()\n",
    );
    files_ok(&[
        lib,
        (
            "main.chz",
            "import lib\nimport Holder, Counter from lib\nstruct Local:\n    k: str\n    fn default() -> Local:\n        return Local(\"loc\")\nfn main():\n    print(lib.Holder(1).make(lib.Counter(9)).n)\n    print(lib.Holder(1).make(Local(\"x\")).k)\n    print(lib.Holder.build(Local(\"x\")).k)\n    print(Holder(2).make(Counter(1)).n)\n    print(Holder.build(Local(\"y\")).k)\nmain()\n",
        ),
    ]);
}

/// OUT OF SCOPE, and it must stay a clear error: a type param declared by the ENCLOSING TYPE. Its
/// witness would have to be stored in every instance of that type — a different mechanism, not a
/// missing case — so the message says that and names the member-level form as the way out.
#[test]
fn witness_enclosing_type_param_rejected() {
    let head = "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\n";
    let needle = "is a type parameter of the enclosing type";
    entry_rejects(
        &format!(
            "{head}struct Bx[T: Default]:\n    v: int\n    fn get(self) -> T:\n        return T.default()\nfn main():\n    print(1)\nmain()\n"
        ),
        needle,
    );
    entry_rejects(
        &format!(
            "{head}enum Eb[T: Default]:\n    One\n    fn get(self) -> T:\n        return T.default()\nfn main():\n    print(1)\nmain()\n"
        ),
        needle,
    );
    // the message must point at the member-level form, which is the supported one
    entry_rejects(
        &format!(
            "{head}struct Bx[T: Default]:\n    v: int\n    fn get(self) -> T:\n        return T.default()\nfn main():\n    print(1)\nmain()\n"
        ),
        "Declare the type parameter on the MEMBER instead",
    );
}

/// The VALUE walls, for the member forms. A method is not first-class in this language and a bare
/// type name is not a value, so both reads are already refused — pinned here because a witness-taking
/// member would be exactly the read that cannot be lowered.
#[test]
fn witness_member_as_value_rejected() {
    let head = "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\nstruct Holder:\n    k: int\n    fn make[T: Default](self, old: T) -> T:\n        return T.default()\n    fn build[T: Default](old: T) -> T:\n        return T.default()\n";
    entry_rejects(
        &format!("{head}fn main():\n    g := Holder(1).make\n    print(1)\nmain()\n"),
        "a bound method is not a value",
    );
    entry_rejects(
        &format!("{head}fn main():\n    g := Holder.build\n    print(1)\nmain()\n"),
        "'Holder.build' cannot be used as a function value",
    );
}

/// M24-5 — `spawn`/`defer` lower a member call at `Op::SpawnRecv`/`DeferMethod`, which now push
/// the hidden witness on top of the declared args, so a witness-taking INSTANCE method is an
/// ordinary target. An ARGUMENT that is itself a witness call stays legal: it is evaluated eagerly
/// in this frame. (A STATIC-method target — `Holder.build(c)` — is an ordinary target too, by an
/// unrelated, witness-independent route: those two emit sites record a RECEIVER value and a static
/// method has none, so M24-5b lowers it through a wrapper proto instead. See
/// `vm::tests::a_static_method_head_is_a_spawn_or_defer_target_runs_both_engines`.)
#[test]
fn witness_member_is_a_spawn_or_defer_target_ok() {
    let head = "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\nstruct Holder:\n    k: int\n    fn make[T: Default](self, old: T) -> T:\n        return T.default()\n    fn build[T: Default](old: T) -> T:\n        return T.default()\nfn sink(c: Counter) -> int:\n    return c.n\n";
    entry_ok(&format!(
        "{head}fn main():\n    h := Holder(1)\n    c := Counter(9)\n    parallel:\n        spawn h.make(c)\nmain()\n"
    ));
    entry_ok(&format!(
        "{head}fn main():\n    h := Holder(1)\n    c := Counter(9)\n    defer h.make(c)\n    print(1)\nmain()\n"
    ));
    // the ARGUMENT position is untouched — the witness call runs before the task/deferral exists
    entry_ok(&format!(
        "{head}fn main():\n    h := Holder(1)\n    c := Counter(9)\n    defer print(h.make(c).n)\n    parallel:\n        spawn sink(h.make(c))\nmain()\n"
    ));
    // …and so is the HEAD LINK of a chained VALUE call target (`defer f(a)(b)`): `parse_postfix`
    // gives the outer link the head's span, so keying the refusal on the call span alone rejected a
    // program that lowered fine before Task 5 (the head evaluates eagerly in THIS frame; the target
    // is the already-computed function value, which needs no witness). Both statements, and the
    // member spelling of the head.
    entry_ok(&format!(
        "{head}fn mk[T: Default](old: T) -> fn(int) -> int:\n    y := T.default()\n    return fn(k: int) -> int: k\nfn main():\n    defer mk(Counter(1))(5)\n    print(1)\nmain()\n"
    ));
    entry_ok(&format!(
        "{head}fn mk[T: Default](old: T) -> fn(int) -> int:\n    y := T.default()\n    return fn(k: int) -> int: k\nfn main():\n    parallel:\n        spawn mk(Counter(1))(5)\nmain()\n"
    ));
    entry_ok(&format!(
        "{head}struct W:\n    k: int\n    fn mk[T: Default](self, old: T) -> fn(int) -> int:\n        y := T.default()\n        return fn(k: int) -> int: k\nfn main():\n    defer W(1).mk(Counter(1))(5)\n    print(1)\nmain()\n"
    ));
}

/// THE HUNTED CLASS — a member that takes a hidden witness may not be reached by any dispatch that
/// pushes a FIXED argument count by name: a protocol requirement (protocol methods cannot declare
/// type params, so no requirement can ever carry the witness), the structural `next`/`iter`/`index`/
/// `set_index`/`contains`/`slice` protocols, the `eq` hook `==` dispatches to, and the test runner's
/// `test fn` / lifecycle hooks. Each of these used to be check-OK-then-runtime-fault ("expects 2
/// argument(s), got 1", or a witness slot read off an operand).
#[test]
fn witness_member_not_reachable_by_fixed_arity_dispatch_rejected() {
    let head = "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\n";
    // a protocol requirement: the witness-taking method does not satisfy it
    entry_rejects(
        &format!(
            "{head}protocol Maker:\n    fn shape(self, x: int) -> int\nstruct M:\n    v: int\n    fn shape[T: Default](self, x: int) -> int:\n        c := T.default()\n        return x\nfn use(m: Maker) -> int:\n    return m.shape(1)\nfn main():\n    print(use(M(1)))\nmain()\n"
        ),
        "expected Maker, found M",
    );
    // the structural iterator protocol — `for` drives `next(self)` with no room for a witness
    entry_rejects(
        &format!(
            "{head}struct It:\n    n: int\n    fn next[T: Default](self) -> int?:\n        c := T.default()\n        return None\nfn main():\n    for x in It(1):\n        print(x)\nmain()\n"
        ),
        "cannot iterate over It",
    );
    // …and the index / contains / slice ones
    entry_rejects(
        &format!(
            "{head}struct S:\n    n: int\n    fn index[T: Default](self, k: int) -> int:\n        c := T.default()\n        return k\nfn main():\n    print(S(1)[0])\nmain()\n"
        ),
        "cannot index into S",
    );
    entry_rejects(
        &format!(
            "{head}struct S:\n    n: int\n    fn contains[T: Default](self, x: int) -> bool:\n        c := T.default()\n        return true\nfn main():\n    print(0 in S(1))\nmain()\n"
        ),
        "cannot use `in` on S",
    );
    entry_rejects(
        &format!(
            "{head}struct S:\n    n: int\n    fn slice[T: Default](self, a: int?, b: int?, c: int?) -> int:\n        x := T.default()\n        return 1\nfn main():\n    print(S(1)[0..1])\nmain()\n"
        ),
        "cannot slice S",
    );
    // a witness-taking `eq` is not the `Eq` hook (TICKET-250): an ordinary method, and `==` stays
    // structural, so no operand is ever read as the type key
    entry_ok(&format!(
        "{head}struct S:\n    n: int\n    fn eq[T: Default](self, o: S) -> bool:\n        c := T.default()\n        return true\nfn main():\n    print(S(1) == S(2))\nmain()\n"
    ));
    // the test runner (a free `test fn`, a suite `test fn`, and a lifecycle hook)
    entry_rejects(
        &format!("{head}test fn t[T: Default]():\n    c := T.default()\n    assert true\n"),
        "invoked by the test runner",
    );
    entry_rejects(
        &format!(
            "{head}struct Suite:\n    v: int = 0\n    test fn t[T: Default](self):\n        c := T.default()\n        assert true\n"
        ),
        "invoked by the test runner",
    );
    entry_rejects(
        &format!(
            "{head}struct Suite:\n    v: int = 0\n    fn before_each[T: Default](self):\n        c := T.default()\n    test fn t(self):\n        assert true\n"
        ),
        "invoked by the test runner",
    );
}

// ===== M24 slice 2 — witness FORWARDING: a generic fn passes its OWN still-abstract type param's
// witness into another generic fn (`fn twice[U: Default](x: U) -> U: return reset(reset(x))`).
// The hidden argument stops being a constant and becomes a load of the caller's own `$w:U` local.
//
// SCOPE of slice 2: SAME-MODULE, and only where the caller's `$w:U` local is DIRECTLY reachable
// (`Checker::witness_scope`) — that predicate is exactly what keeps checker-accepts ⊆
// compiler-lowers. The RUNNING half (that the forwarded key is the OUTERMOST concrete type) is
// `tests/chz/spec/static_witness_test.chz`.

/// The head every forwarding test shares: a static-requirement protocol, two hosts, and the leaf
/// generic that actually constructs.
const FWD_HEAD: &str = "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\nstruct Tag:\n    s: str\n    fn default() -> Tag:\n        return Tag(\"none\")\nfn reset[T: Default](old: T) -> T:\n    return T.default()\n";

/// The accepted forwarding surface. Each of these also RUNS in the `.chz` suite.
#[test]
fn witness_forwarding_accepted_shapes_ok() {
    // the driver shape — a forward NESTED inside another forward
    entry_ok(&format!(
        "{FWD_HEAD}fn twice[U: Default](x: U) -> U:\n    return reset(reset(x))\nfn main():\n    print(twice(Counter(9)).n)\nmain()\n"
    ));
    // TRANSITIVE, three levels, with the leaf declared LAST — the charge must be a fixpoint over
    // the module, not a source-order sweep.
    entry_ok(&format!(
        "{FWD_HEAD}fn lvl1[T: Default](x: T) -> T:\n    return lvl2(x)\nfn lvl2[T: Default](x: T) -> T:\n    return lvl3(x)\nfn lvl3[T: Default](x: T) -> T:\n    return T.default()\nfn main():\n    print(lvl1(Counter(1)).n)\nmain()\n"
    ));
    // TWO type params, forwarding the SECOND one — the hidden args are positional, so slot order
    // (declaration order) has to survive.
    entry_ok(&format!(
        "{FWD_HEAD}fn second[T: Default, U: Default](a: T, b: U) -> U:\n    return reset(b)\nfn main():\n    print(second(Counter(1), Tag(\"x\")).s)\nmain()\n"
    ));
    // a forwarding call whose callee ALSO takes real value args: the hidden witness stays LAST
    entry_ok(&format!(
        "{FWD_HEAD}fn mk[T: Default](seed: T, n: int) -> T:\n    return T.default()\nfn mk_fwd[T: Default](seed: T, n: int) -> T:\n    return mk(seed, n + 1)\nfn main():\n    print(mk_fwd(Counter(1), 2).n)\nmain()\n"
    ));
    // RECURSION — a fn forwarding its own witness to ITSELF
    entry_ok(&format!(
        "{FWD_HEAD}fn rec[T: Default](x: T, n: int) -> T:\n    if n <= 0:\n        return T.default()\n    return rec(x, n - 1)\nfn main():\n    print(rec(Counter(1), 3).n)\nmain()\n"
    ));
    // MUTUAL recursion between two forwarding fns (the fixpoint's other direction)
    entry_ok(&format!(
        "{FWD_HEAD}fn ping[T: Default](x: T, n: int) -> T:\n    if n <= 0:\n        return reset(x)\n    return pong(x, n - 1)\nfn pong[T: Default](x: T, n: int) -> T:\n    return ping(x, n - 1)\nfn main():\n    print(ping(Counter(1), 3).n)\nmain()\n"
    ));
}

/// The permit predicate is `witness_scope`: the caller's witness for `U` must be reachable at the
/// call. Task 4 — that now includes every NESTED body of the declaring fn (a closure, a
/// `spawn:`/`defer:` block, a nested `fn`), because the compiler appends the `$w:U` capture entry
/// there; so a forward written inside one lowers, loading the capture instead of the local.
#[test]
fn witness_forwarding_from_a_nested_body_ok() {
    // a closure body — the escaping spelling, so the forward runs after the defining frame is gone
    entry_ok(&format!(
        "{FWD_HEAD}fn go[T: Default](x: T) -> fn() -> T:\n    return fn(): reset(x)\nfn main():\n    print(go(Counter(1))().n)\nmain()\n"
    ));
    // a `spawn:` block
    entry_ok(&format!(
        "{FWD_HEAD}fn go[T: Default](x: T) -> int:\n    parallel:\n        spawn:\n            y := reset(x)\n    return 1\nfn main():\n    print(go(Counter(1)))\nmain()\n"
    ));
    // a `defer:` block
    entry_ok(&format!(
        "{FWD_HEAD}fn go[T: Default](x: T) -> int:\n    defer:\n        y := reset(x)\n    return 1\nfn main():\n    print(go(Counter(1)))\nmain()\n"
    ));
    // a nested `fn`
    entry_ok(&format!(
        "{FWD_HEAD}fn go[T: Default](x: T) -> T:\n    fn inner(y: T) -> T:\n        return reset(y)\n    return inner(x)\nfn main():\n    print(go(Counter(1)).n)\nmain()\n"
    ));
    // a METHOD's OWN type param forwards fine (Task 5): the hidden argument rides on the method's
    // frame exactly as on a free fn's — pinned in `witness_member_declared_param_ok`.
    entry_ok(&format!(
        "{FWD_HEAD}struct Holder:\n    k: int\n    fn go[T: Default](self, x: T) -> T:\n        return reset(x)\nfn main():\n    print(Holder(1).go(Counter(2)).n)\nmain()\n"
    ));
}

/// …and a forward whose param has no witness ANYWHERE still fails clearly. A type parameter of the
/// enclosing TYPE is the case that survives Task 4: no frame in the program holds a witness for it,
/// so neither the method body nor a closure inside it can forward it.
#[test]
fn witness_forwarding_of_an_enclosing_type_param_rejected() {
    // The needle is the clause that has to stay TRUE for this shape: the enclosing TYPE's param has
    // no witness anywhere, so the message must not imply the current body carries one.
    let needle = "A type parameter of an enclosing TYPE";
    entry_rejects(
        &format!(
            "{FWD_HEAD}struct Bx[T: Default]:\n    v: T\n    fn go(self) -> T:\n        return reset(self.v)\nfn main():\n    print(1)\nmain()\n"
        ),
        needle,
    );
    entry_rejects(
        &format!(
            "{FWD_HEAD}struct Bx[T: Default]:\n    v: T\n    fn go(self) -> T:\n        f := fn() -> T: reset(self.v)\n        return f()\nfn main():\n    print(1)\nmain()\n"
        ),
        needle,
    );
}

/// A type param with NO static-carrying bound has nothing to forward — that must stay the ordinary
/// BOUND failure, not a witness diagnostic.
#[test]
fn witness_forwarding_unbounded_param_rejected() {
    entry_rejects(
        &format!(
            "{FWD_HEAD}fn g[U](x: U) -> U:\n    return reset(x)\nfn main():\n    print(g(Counter(1)).n)\nmain()\n"
        ),
        "does not satisfy",
    );
}

/// A fn that becomes witness-needing ONLY through forwarding is charged a hidden parameter exactly
/// like one that constructs directly — so it loses the fn-VALUE position, which is the position the
/// charge genuinely costs (a value erases the very type the witness names).
#[test]
fn witness_forwarding_fn_loses_the_value_position_rejected() {
    let head = format!("{FWD_HEAD}fn fwd[T: Default](x: T) -> T:\n    return reset(x)\n");
    entry_rejects(
        &format!("{head}fn main():\n    g := fwd\n    print(1)\nmain()\n"),
        "cannot be used as a function value",
    );
    entry_rejects(
        &format!("{head}fn main():\n    g := fwd[Counter]\n    print(1)\nmain()\n"),
        "cannot be used as a function value",
    );
    // …but a CALL across a module boundary keeps working (Task 3): the caller pushes the witness at
    // the call site, which is exactly the position a function VALUE cannot offer.
    files_ok(&[
        (
            "lib.chz",
            &format!("{FWD_HEAD}fn fwd[T: Default](x: T) -> T:\n    return reset(x)\n"),
        ),
        (
            "main.chz",
            "import lib\nfn main():\n    print(lib.fwd(lib.Counter(9)).n)\nmain()\n",
        ),
    ]);
}

/// …and the STATEMENT-TARGET position it does NOT lose (M24-5): a forwarding fn is a `spawn`/`defer`
/// target like a directly-constructing one — the transitive charge is threaded at the emit site, not
/// walled there.
#[test]
fn witness_forwarding_fn_keeps_the_spawn_and_defer_positions_ok() {
    let head = format!("{FWD_HEAD}fn fwd[T: Default](x: T) -> T:\n    return reset(x)\n");
    entry_ok(&format!(
        "{head}fn main():\n    c := Counter(9)\n    parallel:\n        spawn fwd(c)\nmain()\n"
    ));
    entry_ok(&format!(
        "{head}fn main():\n    c := Counter(9)\n    defer fwd(c)\n    print(1)\nmain()\n"
    ));
}

/// A generic with a static-carrying bound whose type param this signature can never BIND must not
/// be charged a witness by the forwarding rule — a charged param nothing determines makes the fn
/// UNCALLABLE, so an over-charge here breaks a program that compiles. Nor may a PARAMETER named like
/// a witness-taking fn count as forwarding: the param shadows the module fn, so the body never
/// reaches it.
#[test]
fn witness_forwarding_does_not_overcharge_ok() {
    // `T`/`U` occur in NO parameter type and NO return type, so nothing at a call site can bind
    // them — charging one would make every call fail "is not determined here".
    entry_ok(&format!(
        "{FWD_HEAD}fn note[T: Default]() -> str:\n    c := reset(Counter(1))\n    return \"hi{{c.n}}\"\nfn main():\n    print(note())\nmain()\n"
    ));
    entry_ok(&format!(
        "{FWD_HEAD}fn helper[T: Default, U: Default](x: T) -> int:\n    c := reset(Counter(1))\n    return c.n\nfn main():\n    print(helper(Counter(5)))\nmain()\n"
    ));
    // a PARAM named `reset` shadows the module-level `reset`, so this body forwards nothing and
    // keeps its value position.
    entry_ok(&format!(
        "{FWD_HEAD}fn label[T: Default](x: T, reset: int) -> int:\n    return reset + 1\nfn main():\n    g := label[Counter]\n    print(label(Counter(1), 2))\nmain()\n"
    ));
}

/// M24-2 — a member call on a receiver that can NEVER take a witness. `List.push` / `Map.get` /
/// `Set.add` are BUILTINS, so passing this fn's own `x: T` to one forwards nothing: there is no
/// witness parameter on the other side. Asking only "does this call site carry something of
/// `decl`'s?" charged every one of these, and the charge costs the fn its function-value position —
/// so a program that compiled before stopped compiling. The method NAME must ALSO be declared
/// witness-taking somewhere in the graph.
#[test]
fn witness_member_forward_on_a_builtin_receiver_does_not_charge_ok() {
    // the filed repro: a `List[T]` sink taking the fn's own `T`
    entry_ok(&format!(
        "{FWD_HEAD}fn label[T: Default](x: T, sink: List[T]) -> int:\n    sink.push(x)\n    return sink.len()\nfn main():\n    g := label[Counter]\n    print(label(Counter(1), []))\nmain()\n"
    ));
    // …the same shape through a `Map`, a `Set` and an `Option` — and through a turbofish-free
    // nested argument (`xs[0]`), which the ident walk reads at any depth
    entry_ok(&format!(
        "{FWD_HEAD}fn label[T: Default](x: T, m: Map[str, T]) -> int:\n    m.update({{\"a\": x}})\n    return m.len()\nfn main():\n    g := label[Counter]\n    print(label(Counter(1), {{}}))\nmain()\n"
    ));
    entry_ok(&format!(
        "{FWD_HEAD}fn label[T: Default](x: T, xs: List[T]) -> int:\n    xs.insert(0, x)\n    return xs.len()\nfn main():\n    g := label[Counter]\n    print(label(Counter(1), []))\nmain()\n"
    ));
    entry_ok(&format!(
        "{FWD_HEAD}fn label[T: Default](xs: List[T], sink: List[T]) -> int:\n    sink.push(xs[0])\n    return sink.len()\nfn main():\n    g := label[Counter]\n    print(label([Counter(1)], []))\nmain()\n"
    ));
    // …and the caller that FORWARDS its own `T` into such a fn is uncharged too — the charge used to
    // propagate one call further out.
    entry_ok(&format!(
        "{FWD_HEAD}fn label[T: Default](x: T, sink: List[T]) -> int:\n    sink.push(x)\n    return sink.len()\nfn outer[T: Default](x: T) -> int:\n    return label(x, [])\nfn main():\n    g := outer[Counter]\n    print(outer(Counter(1)))\nmain()\n"
    ));
    // NON-REGRESSION, the unsafe direction: once a USER type really declares a witness-taking
    // `push`, the identical body charges again and loses the value position.
    entry_rejects(
        &format!(
            "{FWD_HEAD}struct Sink:\n    v: int\n    fn push[U: Default](self, old: U) -> U:\n        return U.default()\nfn label[T: Default](x: T, sink: List[T]) -> int:\n    sink.push(x)\n    return sink.len()\nfn main():\n    g := label\n    print(1)\nmain()\n"
        ),
        "cannot be used as a function value",
    );
}

/// A witness call that is the ROOT of a `{…}` interpolation fragment (`"{reset(c)}"`, no trailing
/// `.field`). The checker anchors a fragment root at the STRING LITERAL's span so a fragment error
/// points at the literal — which also moved the witness key, and the compiler looked the call up
/// under the fragment-relative span it still had. Result: `chezzi check` said ok and the run died
/// with `internal: no static-type witness recorded`. Present since slice A (no forwarding needed to
/// reproduce); this pins BOTH halves — the key must agree, and the runtime value must be right (the
/// RUNNING half is `an_interpolation_fragment_root_is_a_real_call_site` in the `.chz` suite).
#[test]
fn witness_call_at_an_interpolation_fragment_root_ok() {
    // the CONCRETE witness (the pre-existing hole)…
    entry_ok(&format!(
        "{FWD_HEAD}fn main():\n    c := Counter(9)\n    print(\"{{reset(c)}}\")\nmain()\n"
    ));
    // …and the FORWARDED one, which reaches the same key
    entry_ok(&format!(
        "{FWD_HEAD}fn fwd[T: Default](x: T) -> str:\n    return \"{{reset(x)}}\"\nfn main():\n    print(fwd(Tag(\"hi\")))\nmain()\n"
    ));
}

// ===== M24 final review — the seven findings the three reviewers filed against the finished branch.
// Each test below is the compile-time half; the ones whose bug is a WRONG VALUE (the pipe key, the
// shadowed type param) run in `tests/chz/spec/static_witness_test.chz`, which is the only half that
// can catch them.

/// A — the witness key is the CALLEE TOKEN. `|>` desugars at PARSE time into a plain `Call` that
/// inherits the whole infix expression's span, so keying on the call NODE aliased every link of
/// `a |> f() |> g()` onto one table slot. When the two links' bounds differ that green
/// `chezzi check` becomes a hard runtime fault; when they agree it is a silently wrong value.
#[test]
fn witness_pipe_chain_links_do_not_alias_ok() {
    // two DIFFERENT static-carrying protocols in one chain: pre-fix the second link's entry
    // overwrote the first's, so link 1 dispatched `default` on `Wrap` — which has none — and the
    // green check became a hard runtime fault.
    entry_ok(&format!(
        "{FWD_HEAD}protocol Conv:\n    fn conv(x: int) -> Self\nstruct Wrap:\n    v: int\n    fn conv(x: int) -> Wrap:\n        return Wrap(x)\nfn to_wrap[T: Default](x: T) -> Wrap:\n    return Wrap(1)\nfn from_wrap[U: Conv](x: U) -> U:\n    return U.conv(1)\nfn main():\n    print((Counter(1) |> to_wrap() |> from_wrap()).v)\nmain()\n"
    ));
    // the same callee twice — one NAME, two distinct callee TOKENS
    entry_ok(&format!(
        "{FWD_HEAD}fn main():\n    print((Counter(1) |> reset() |> reset()).n)\nmain()\n"
    ));
    // …and a pipe chain that is an interpolation fragment's root
    entry_ok(&format!(
        "{FWD_HEAD}fn main():\n    print(\"{{Counter(1) |> reset() |> reset()}}\")\nmain()\n"
    ));
}

// B — a type param whose name SHADOWS a real type used to be pinned here as "the checker keeps
// resolving it to the concrete STRUCT", which was the wrong half to settle on: Rust and Go let the
// parameter shadow. Superseded by
// `witness_type_param_shadows_a_same_named_type_in_static_call_position` (below), which pins the
// ancestor's answer plus its neighbours; the running half is in the `.chz` suite.

/// A module that exports BOTH a witness-taking generic (`mk`) and an ordinary fn (`name`) — the
/// shape the module half of the forwarding charge has to tell apart.
const WITNESS_LIB: (&str, &str) = (
    "lib.chz",
    "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\nfn mk[T: Default](x: T) -> T:\n    return T.default()\nfn name(n: int) -> str:\n    return \"n\"\n",
);

/// D — merely NAMING an imported module must not charge a witness. The module half of the forwarding
/// scan was coarse to the WHOLE module, so importing any module that exports any witness-taking fn
/// cost every static-bounded generic in the file its value / `spawn` / `defer` positions.
#[test]
fn witness_forwarding_naming_a_module_does_not_charge_ok() {
    // `label` names `lib` but calls a NON-witness member of it → uncharged, so it keeps every
    // position a plain generic has (function value, `defer` target).
    files_ok(&[
        WITNESS_LIB,
        (
            "main.chz",
            "import lib\nprotocol Mk:\n    fn mk_it() -> Self\nstruct Q:\n    v: int\n    fn mk_it() -> Q:\n        return Q(1)\nfn label[T: Mk](x: T) -> str:\n    return lib.name(1)\nfn apply(f: fn(Q) -> str, q: Q) -> str:\n    return f(q)\nfn main():\n    print(apply(label, Q(2)))\n    defer label(Q(3))\nmain()\n",
        ),
    ]);
}

/// …and the other direction, which is the unsafe one: a genuine forward into an imported callee must
/// STILL be charged, in every spelling (bare `from`-import, module-qualified, aliased module).
#[test]
fn witness_forwarding_into_an_imported_callee_ok() {
    files_ok(&[
        WITNESS_LIB,
        (
            "main.chz",
            "import lib\nimport lib as alias\nimport mk, Default from lib\nfn qfwd[T: Default](x: T) -> T:\n    return lib.mk(x)\nfn afwd[T: Default](x: T) -> T:\n    return alias.mk(x)\nfn bfwd[T: Default](x: T) -> T:\n    return mk(x)\nfn main():\n    print(qfwd(lib.Counter(1)).n)\n    print(afwd(lib.Counter(1)).n)\n    print(bfwd(lib.Counter(1)).n)\nmain()\n",
        ),
    ]);
    // a charged fn loses its value position — the proof the qualified forward really was charged
    files_reject(
        &[
            WITNESS_LIB,
            (
                "main.chz",
                "import lib\nimport Default from lib\nfn qfwd[T: Default](x: T) -> T:\n    return lib.mk(x)\nfn main():\n    g := qfwd\n    print(1)\nmain()\n",
            ),
        ],
        "cannot be used as a function value",
    );
}

// ===== M24-1 — the witness OVER-CHARGE. The forwarding charge used to be two name-only questions
// over the whole body ("is any name a witness-taking fn?" / "is any name a module that exports one,
// and is any `obj.member` spelled like one of its members?"). Both over-fired, and the cost is not
// cosmetic: the charged fn loses the function-VALUE position permanently. The charge is now asked
// per CALL SITE (`crate::compiler::CallSite`), against real call structure.

/// Shape (a) — a forwarding call whose arguments are ALL concrete forwards nothing of the caller's
/// own `T`, so it costs nothing. Pre-fix `g := concrete` was refused ("its bound on T requires a
/// static protocol method"). Its RUNNING half — the value is really callable, so the compiled
/// proto's arity agrees — is `a_forward_with_only_concrete_arguments_stays_a_function_value` in
/// `tests/chz/spec/static_witness_test.chz`.
#[test]
fn witness_forwarding_with_only_concrete_arguments_does_not_charge_ok() {
    let head = format!(
        "{FWD_HEAD}fn concrete[T: Default](x: T) -> int:\n    return reset(Counter(1)).n\n"
    );
    entry_ok(&format!(
        "{head}fn main():\n    g := concrete[Counter]\n    print(concrete(Counter(3)))\nmain()\n"
    ));
    // …in every function-value spelling: turbofished, and passed to a HOF
    entry_ok(&format!(
        "{head}fn apply(f: fn(Counter) -> int, c: Counter) -> int:\n    return f(c)\nfn main():\n    g := concrete[Counter]\n    print(g(Counter(3)))\n    print(apply(concrete, Counter(4)))\nmain()\n"
    ));
    // a NESTED concrete ctor is still concrete (`Tag("x")` inside `second_of_two`'s arg list)
    entry_ok(&format!(
        "{FWD_HEAD}fn pair[T: Default, U: Default](a: T, b: U) -> U:\n    return U.default()\nfn concrete[T: Default](x: T) -> Tag:\n    return pair(Counter(1), Tag(\"x\"))\nfn main():\n    g := concrete[Counter]\n    print(concrete(Counter(3)).s)\nmain()\n"
    ));
}

/// Shape (b) — a struct METHOD that merely shares a name with an imported witness-taking fn. The
/// module half asked its two halves independently ("is `lib` named?" AND "is some member spelled
/// `reset`?"), so `Holder.reset` — which has nothing to do with `lib.reset` — charged `tagged` for
/// a call it never makes. The pair is now looked up together: `lib.plain` is not witness-taking, so
/// nothing is charged.
#[test]
fn witness_forwarding_module_half_matches_the_exact_callee_ok() {
    let lib = (
        "lib.chz",
        "protocol LibDefault:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(7)\nfn reset[T: LibDefault](old: T) -> T:\n    return T.default()\nfn plain(a: int) -> int:\n    return a + 1\n",
    );
    files_ok(&[
        lib,
        (
            "main.chz",
            "import lib\nprotocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(7)\nstruct Holder:\n    k: int\n    fn reset(self) -> int:\n        return self.k * 10\nfn tagged[T: Default](x: T) -> int:\n    h := Holder(4)\n    return h.reset() + lib.plain(1)\nfn main():\n    print(tagged(Counter(3)))\n    g := tagged[Counter]\n    print(1)\nmain()\n",
        ),
    ]);
    // the isolation control the gap row measured: deleting ` + lib.plain(1)` always passed, so the
    // charge came from naming `lib` at all — the two spellings must now agree
    files_ok(&[
        lib,
        (
            "main.chz",
            "import lib\nprotocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(7)\nstruct Holder:\n    k: int\n    fn reset(self) -> int:\n        return self.k * 10\nfn tagged[T: Default](x: T) -> int:\n    h := Holder(4)\n    return h.reset()\nfn main():\n    print(tagged(Counter(3)))\n    g := tagged[Counter]\n    print(1)\nmain()\n",
        ),
    ]);
    // …and the real qualified forward into that same module STILL charges
    files_reject(
        &[
            lib,
            (
                "main.chz",
                "import lib\nimport LibDefault from lib\nfn qfwd[T: LibDefault](x: T) -> T:\n    return lib.reset(x)\nfn main():\n    g := qfwd\n    print(1)\nmain()\n",
            ),
        ],
        "cannot be used as a function value",
    );
}

/// The must-STILL-charge neighbours. Every one of these was rejected before the narrowing and must
/// stay rejected: an under-charge is a forward the checker then refuses with a worse message, or one
/// the compiler cannot lower. Anything the syntactic walk cannot positively identify as concrete
/// charges.
#[test]
fn witness_forwarding_still_charges_every_unpinned_shape_rejected() {
    let tail = "fn main():\n    g := f\n    print(1)\nmain()\n";
    let charged = |body: &str| {
        entry_rejects(
            &format!("{FWD_HEAD}{body}{tail}"),
            "cannot be used as a function value",
        )
    };
    // the parameter itself
    charged("fn f[T: Default](x: T) -> T:\n    return reset(x)\n");
    // a LOCAL bound to it — never concrete, whatever it holds
    charged("fn f[T: Default](x: T) -> T:\n    y := x\n    return reset(y)\n");
    // a ZERO-ARG witness-taking call: its `T` comes from the expected type, invisible to this walk
    charged(
        "fn empty[T: Default]() -> T:\n    return T.default()\nfn f[T: Default](x: T) -> T:\n    y: T = empty()\n    return y\n",
    );
    // a witness pinned by the callee's RETURN type only — the arguments look concrete but decide
    // nothing, so `T` is still inferred from THIS fn's return position
    charged(
        "fn conv[T: Default, U](a: U) -> T:\n    return T.default()\nfn f[T: Default](x: T) -> T:\n    return conv(1)\n",
    );
    // a witness carried by a DEFAULTED parameter the call site never supplies. `ty_param_in_params`
    // is arity-blind — it sees `b: T?` mention `T` and says the arguments pin it — so this charges
    // only because the inline default is SPLICED IN before the checker looks (`conv(1, None)`, and
    // `None` is not a closed argument). This case is the pin on that ordering: if default splicing
    // ever moves after the signature hoist, it goes green-then-broken here first.
    charged(
        "fn conv[T: Default](a: int, b: T? = None) -> T:\n    return T.default()\nfn f[T: Default](x: T) -> T:\n    return conv(1)\n",
    );
    // a nested ctor whose own argument is not closed
    charged("fn f[T: Default](x: T) -> Counter:\n    k := 1\n    return reset(Counter(k))\n");
    // a GENERIC struct's ctor head — its args could name a type param, so it is not concrete
    charged(
        "struct GBox[U]:\n    v: U\nfn reset2[T: Default](old: T, extra: GBox[int]) -> T:\n    return T.default()\nfn f[T: Default](x: T) -> Counter:\n    return reset2(Counter(1), GBox(2))\n",
    );
    // an argument headed by the type parameter itself
    charged("fn f[T: Default](x: T) -> T:\n    return reset(T.default())\n");
    // an explicit turbofish naming this fn's own type param, with a closed argument list
    charged(
        "fn mk2[T: Default](n: int) -> T:\n    return T.default()\nfn f[T: Default](x: T) -> T:\n    return mk2[T](1)\n",
    );
    // a forward from a NESTED body (`defer:` block / closure), which the walk descends into
    charged(
        "fn f[T: Default](x: T) -> int:\n    defer:\n        y := reset(x)\n        print(y)\n    return 1\n",
    );
    charged(
        "fn apply[V](g: fn() -> V) -> V:\n    return g()\nfn f[T: Default](x: T) -> T:\n    return apply(fn(): reset(x))\n",
    );
}

/// E — an interpolation-fragment diagnostic points at the FRAGMENT EXPRESSION's real column. A
/// fragment was once re-lexed from its own source, and while only its LINE was re-anchored every
/// column restarted at 1; three successive anchors papered over that (one mutating a cloned root's
/// span, one carried beside the AST) and each one broke a table key or a nested fragment. The lexer
/// now lexes a hole in place, so its tokens carry real positions and there is nothing left to anchor.
/// CPython is the ancestor and points at the expression too (3.14.6 carets `nope` in
/// `print(f"hello {nope}")`, not the literal).
#[test]
fn interpolation_fragment_error_points_at_the_fragment() {
    // col:            1234567890123
    let errs = check_src("print(\"ab {nope}\")\n");
    let e = errs
        .iter()
        .find(|e| e.message.contains("unknown name 'nope'"))
        .unwrap_or_else(|| panic!("expected an unknown-name error, got: {errs:?}"));
    assert_eq!(
        (e.span.line, e.span.col),
        (1, 12),
        "a fragment error must point at `nope` itself"
    );
}

/// …and a fragment NESTED in another fragment's literal is a real position too — the case that
/// reported column 1 at every depth. There is nothing to compose: the inner literal is lexed in place
/// by the same lexer, so its holes carry real positions at any depth.
#[test]
fn nested_interpolation_fragment_error_points_at_the_fragment() {
    // col:            1234567890123456789012345
    let errs = check_src("print(\"hello {'inner {nope} x'} world\")\n");
    let e = errs
        .iter()
        .find(|e| e.message.contains("unknown name 'nope'"))
        .unwrap_or_else(|| panic!("expected an unknown-name error, got: {errs:?}"));
    assert_eq!((e.span.line, e.span.col), (1, 23), "the inner `nope`");
}

/// …and the composition hazard: a nested literal AFTER a parent escape. The parent's `\t` is TWO
/// source chars for one content char; a column counted off the post-escape text puts the inner
/// fragment one column LEFT of the truth, which is how a nested fragment lands on a neighbour's
/// table key. The lexer reads the nested literal in place, so its columns are the file's.
#[test]
fn a_nested_literal_after_an_escape_composes() {
    // line 3:  print("{ f('a\tb{nope}c') }")
    // cols:    1234567890123456789
    // `(`=6 `"`=7 `{`=8 ` `=9 `f`=10 `(`=11 `'`=12 `a`=13 `\`=14 `t`=15 `b`=16 `{`=17 → `nope`=18
    let errs = check_src("fn f(s: str) -> str:\n    return s\nprint(\"{ f('a\\tb{nope}c') }\")\n");
    let e = errs
        .iter()
        .find(|e| e.message.contains("unknown name 'nope'"))
        .unwrap_or_else(|| panic!("expected an unknown-name error, got: {errs:?}"));
    assert_eq!(
        (e.span.line, e.span.col),
        (3, 18),
        "the doubly-nested `nope`"
    );
}

/// The `docs/contributing/gaps.md` M24-6 repro itself: a fragment past a REAL newline points at the fragment,
/// not at an offset from the literal's opening delimiter that runs off the end of its line.
#[test]
fn a_fragment_past_a_real_newline_points_at_the_fragment() {
    // line 4:  cc {nope} dd"""
    // cols:    12345
    let errs = check_src("z := 0\ns := \"\"\"aaa\nbbb\ncc {nope} dd\"\"\"\n");
    let e = errs
        .iter()
        .find(|e| e.message.contains("unknown name 'nope'"))
        .unwrap_or_else(|| panic!("expected an unknown-name error, got: {errs:?}"));
    assert_eq!((e.span.line, e.span.col), (4, 5));
}

/// …and a `\n` ESCAPE keeps the literal's line, which is the distinction the old design could not
/// draw at all (`raw` is post-escape, so the two kinds looked identical).
#[test]
fn a_fragment_past_an_escape_newline_keeps_the_literals_line() {
    // line 2:  s := "aaa\nbbb\ncc {nope} dd"
    // `"`=6 `a`=7,8,9 `\`=10 `n`=11 `b`=12,13,14 `\`=15 `n`=16 `c`=17,18 ` `=19 `{`=20 → 21
    let errs = check_src("z := 0\ns := \"aaa\\nbbb\\ncc {nope} dd\"\n");
    let e = errs
        .iter()
        .find(|e| e.message.contains("unknown name 'nope'"))
        .unwrap_or_else(|| panic!("expected an unknown-name error, got: {errs:?}"));
    assert_eq!((e.span.line, e.span.col), (2, 21));
}

/// An escape before the fragment used to shift its reported column LEFT by one per escape, because
/// the payload is post-escape text and the column was an offset into it. The lexer lexes the hole in
/// place, so the escapes now cost exactly the source chars they occupy — asserted as
/// exact hand-counted values, since a uniformly-wrong-by-one column reads as correct to a human.
/// What must still hold is the KEY property: two fragments, two different columns.
#[test]
fn escapes_before_a_fragment_keep_two_fragments_apart() {
    // print("\t{nope} \"x\" {nope}")
    // 1234567890         2         3
    //           1234567890123456789
    // `(`=6, `"`=7, `\`=8, `t`=9, `{`=10 → first `nope` at 11; `}`=15, ` `=16, `\`=17, `"`=18,
    // `x`=19, `\`=20, `"`=21, ` `=22, `{`=23 → second `nope` at 24.
    let errs = check_src("print(\"\\t{nope} \\\"x\\\" {nope}\")\n");
    let cols: Vec<u32> = errs
        .iter()
        .filter(|e| e.message.contains("unknown name 'nope'"))
        .map(|e| e.span.col)
        .collect();
    assert_eq!(cols.len(), 2, "two fragments, got: {errs:?}");
    assert_eq!(cols, vec![11, 24], "hand-counted physical columns");
    assert_ne!(cols[0], cols[1], "two fragments must not share a column");
}

/// Padding inside a fragment (`"{   nope   }"`, legal in Python) is insignificant, and the column
/// must still land on the expression. This is the guard that the leading-whitespace COUNT and the
/// `trim()` that drops it agree: they used to be two derivations in two functions, and any drift
/// between them is a silently-shifted column. They are one derivation now — the lexer skips the
/// padding as white space and the first token carries its own column.
#[test]
fn padded_fragment_column_points_past_the_padding() {
    // print("{   nope   }")
    // `(`=6 `"`=7 `{`=8 ` `=9,10,11 → `nope` at 12
    let errs = check_src("print(\"{   nope   }\")\n");
    let e = errs
        .iter()
        .find(|e| e.message.contains("unknown name 'nope'"))
        .unwrap_or_else(|| panic!("expected an unknown-name error, got: {errs:?}"));
    assert_eq!((e.span.line, e.span.col), (1, 12));
}

/// A LEX error inside a hole reports its REAL line AND column (`docs/contributing/gaps.md` M24-6 + M24-7). The
/// lexer lexes the hole in place, so the position is the lexer's own; there is no second lexer
/// whose cursor says line 1.
#[test]
fn a_lex_error_inside_a_fragment_reports_its_real_line() {
    // The `\u{ZZ}` is not a valid escape, and it is inside a NESTED literal. One backslash: the
    // nested literal's text is read once, so `\u{ZZ}` reaches the escape reader as written.
    // Source line 3 is:  s := "ab {'\u{ZZ}'}"
    let (line, col, message) = front_err("print(1)\nprint(2)\ns := \"ab {'\\u{ZZ}'}\"\n");
    // Column 15 is hand-counted off source line 3 — `s`1 ` `2 `:`3 `=`4 ` `5 `"`6 `a`7 `b`8 ` `9
    // `{`10 `'`11 `\`12 `u`13 `{`14 `Z`←15 — the physical position of the first `Z`. That is what
    // the LSP squiggles and what `--errors=json` emits.
    assert_eq!(
        (line, col),
        (3, 15),
        "the squiggled position must be the offending char, got: {message}"
    );
    assert_eq!(message, "invalid hex digit in unicode escape");
}

/// The PARSE half of the same seam. A hole's tokens carry their real physical spans — the code's
/// end-of-input token sits on the `}` — so `ParseError.span` points inside the hole. CPython 3.14
/// puts the caret inside the fragment too (`compile('y = f"a\tb{1 + }c"', …)` → `offset=14`, `+`).
#[test]
fn a_parse_error_inside_a_fragment_reports_the_fragments_own_position() {
    // Source line 3 is:  s := "a\tb{1 + }c"   — a REAL `\t` escape (two source columns, one content
    // char) sits before the hole, so a column counted off the post-escape content lands one to the
    // left and this test fails.
    // `s`1 ` `2 `:`3 `=`4 ` `5 `"`6 `a`7 `\`8 `t`9 `b`10 `{`11 `1`12 ` `13 `+`14 ` `15 `}`←16
    // — the end of the code is the closing `}`.
    let (line, col, message) = front_err("print(1)\nprint(2)\ns := \"a\\tb{1 + }c\"\n");
    assert_eq!(
        (line, col),
        (3, 16),
        "the squiggled position must be inside the hole, got: {message}"
    );
    // The literal's own span is col 6 (the opening quote) — useless the moment a literal holds two
    // holes.
    assert_ne!(col, 6, "must not fall back to the opening quote");
    assert_eq!(message, "unexpected end of input in expression");
}

/// The interpolation errors that are ABOUT the literal's shape rather than about a hole's
/// expression. Each has a char it can name, so each names it (the third has no CPython analogue: a
/// format spec is a runtime concern there, so the rule is simply "the spec's first char").
#[test]
fn literal_shape_errors_point_at_their_own_char() {
    // Every case carries a `\t` before the offending char, so a raw-content count is off by one.
    // `s`1 ` `2 `:`3 `=`4 ` `5 `"`6 `a`7 `\`8 `t`9 `b`10 …
    for (src, want_col, needle) in [
        // `s := "a\tb{1 + c"` — `{`←11. CPython points at the closing quote (offset 17); the
        // lexer reads on past it as code, so an unclosed hole names its own `{`.
        ("s := \"a\\tb{1 + c\"\n", 11, "unterminated '{'"),
        // `s := "a\tb}c"` — `}`←11. CPython: offset 11, the `}` itself.
        ("s := \"a\\tb}c\"\n", 11, "unmatched '}'"),
        // `s := "a\tb{x:>99999999}c"` — `{`11 `x`12 `:`13 `>`←14, the spec's first char.
        ("s := \"a\\tb{x:>99999999}c\"\n", 14, "format spec:"),
        // `s := "a\tb{x # c}"` — `{`11 `x`12 ` `13 `#`←14, the comment that swallowed the `}`
        ("s := \"a\\tb{x # c}\"\n", 14, "'#'"),
    ] {
        let (line, col, message) = front_err(src);
        assert!(message.contains(needle), "{needle:?}: got {message:?}");
        assert_eq!(
            (line, col),
            (1, want_col),
            "{needle}: wrong position ({message})"
        );
    }
}

/// A `#` inside a hole comments to the end of the PHYSICAL line (CPython 3.12+), not a blanket
/// reject — so a single-line hole is rejected (its closing `}` is commented out) while a multi-line
/// hole's comment ends at the real line break. W12-19.
#[test]
fn hash_comment_in_a_hole_runs_to_the_real_end_of_line() {
    front_rejects("x := 1\ny := \"{x # c}\"\n", "'#'");
    front_rejects("x := 1\ny := \"{x # c\\n}\"\n", "'#'"); // an escaped newline does not end the comment
    ok("x := 1\ny := \"\"\"{x # note: y\n}\"\"\"\n");
    ok("d := {\"#\": 1}\ny := \"{d['#']}{'#'}\"\n");
    ok("n := 255\ny := \"{n:#x}\"\n");
}

/// …and the position it names must EXIST IN THE FILE. An unclosed hole points at its own `{`: the
/// lexer reads the hole as code, so it runs on past the literal's closing quote, and the `{` is the
/// one position that names the hole. (CPython points at the closing quote.)
#[test]
fn unterminated_hole_points_at_its_opening_brace() {
    // `x`1 ` `2 `:`3 `=`4 ` `5 `"`6 `"`7 `"`8 `a`9 / line 2 `{b` ← the `{` is col 1.
    for (src, want) in [
        ("x := \"\"\"a\n{b\n\"\"\"\n", (2, 1)),
        // …with blank lines before the terminator
        ("x := \"\"\"a\n{b\n\n\n\n\"\"\"\n", (2, 1)),
        // `s`1 ` `2 `:`3 `=`4 ` `5 `"`6 `a`7 `{`←8
        ("s := \"a{b\"\n", (1, 8)),
        // …a `\n` ESCAPE before the brace costs two columns: `"`6 `a`7 `\`8 `n`9 `b`10 `{`←11
        ("s := \"a\\nb{c\"\n", (1, 11)),
    ] {
        let (line, col, message) = front_err(src);
        assert!(message.contains("unterminated '{'"), "got: {message}");
        assert_eq!((line, col), want, "must point at the hole's `{{`");
        // …and, independently of the expected value: the position must be a real one — a column no
        // further than one past the end of a line the file actually has.
        let text = src
            .lines()
            .nth(line - 1)
            .unwrap_or_else(|| panic!("line {line} is past the end of the file"));
        assert!(
            col <= text.chars().count() + 1,
            "col {col} does not exist on line {line} ({text:?})"
        );
    }
}

/// …and the anchor must not bring the key aliasing back with it: two witness calls in ONE fragment
/// still resolve to their own witnesses (the key is the callee token, which the re-anchor of the
/// fragment ROOT cannot touch).
#[test]
fn two_witness_calls_in_one_fragment_keep_their_own_witness_ok() {
    entry_ok(&format!(
        "{FWD_HEAD}fn main():\n    print(\"{{reset(reset(Counter(1))).n}} {{reset(Tag(\"x\")).s}}\")\nmain()\n"
    ));
}

/// F — the "not determined here" diagnostic must name a spelling that WORKS. For an instance method
/// both suggestions were dead ends: `make[SomeType](...)` is read as a free call ("'make' takes no
/// type arguments") and an annotated result never reaches a method's own `[T]`. The form that works
/// is the member turbofish.
#[test]
fn witness_undetermined_member_names_the_member_turbofish() {
    let head = format!(
        "{FWD_HEAD}struct Holder:\n    k: int\n    fn make[T: Default](self) -> T:\n        return T.default()\n"
    );
    entry_rejects(
        &format!("{head}fn main():\n    x := Holder(1).make()\nmain()\n"),
        "ON THE METHOD (`<receiver>.make[SomeType](...)`)",
    );
    // …and that spelling is accepted.
    entry_ok(&format!(
        "{head}fn main():\n    print(Holder(1).make[Counter]().n)\nmain()\n"
    ));
    // a FREE fn keeps the (working) name-turbofish + annotated-result advice.
    entry_rejects(
        &format!(
            "{FWD_HEAD}fn empty[T: Default]() -> T:\n    return T.default()\nfn main():\n    x := empty()\nmain()\n"
        ),
        "pin it with a type argument (`empty[SomeType](...)`) or an annotated result",
    );
}

/// F2 — the same diagnostic at the OTHER two callee spellings. A static MEMBER
/// (`Holder.build()`) and a module-QUALIFIED fn (`lib.empty()`) both took their type argument after
/// the member name, and both were told to write the bare `build[SomeType](...)` — which parses as a
/// free call and answers "'build' takes no type arguments". Each suggested spelling is also compiled
/// here, so the advice cannot rot.
#[test]
fn witness_undetermined_names_a_working_spelling_at_every_callee_form() {
    // static MEMBER
    let head = format!(
        "{FWD_HEAD}struct Holder:\n    k: int\n    fn build[T: Default]() -> T:\n        return T.default()\n"
    );
    entry_rejects(
        &format!("{head}fn main():\n    x := Holder.build()\nmain()\n"),
        "pin it with a type argument (`Holder.build[SomeType](...)`)",
    );
    entry_ok(&format!(
        "{head}fn main():\n    print(Holder.build[Counter]().n)\nmain()\n"
    ));
    // module-QUALIFIED free fn
    let lib = (
        "lib.chz",
        "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\nfn empty[T: Default]() -> T:\n    return T.default()\n",
    );
    files_reject(
        &[
            lib,
            (
                "main.chz",
                "import lib\nfn main():\n    x := lib.empty()\nmain()\n",
            ),
        ],
        "pin it with a type argument (`lib.empty[SomeType](...)`)",
    );
    files_ok(&[
        lib,
        (
            "main.chz",
            "import lib\nfn main():\n    print(lib.empty[lib.Counter]().n)\nmain()\n",
        ),
    ]);
    // module-QUALIFIED static MEMBER — the spelling that still named an unspellable prefix. The
    // advice must carry the prefix the callee was REACHED by (`lib.Holder.build[…]`); the bare
    // `Holder.build[…]` it used to suggest answers "unknown type 'Holder'". Third report of this
    // shape, hence the compile-each-suggestion rule below.
    let hlib = (
        "lib.chz",
        "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\nstruct Holder:\n    k: int\n    fn build[T: Default]() -> T:\n        return T.default()\n",
    );
    files_reject(
        &[
            hlib,
            (
                "main.chz",
                "import lib\nfn main():\n    x := lib.Holder.build()\nmain()\n",
            ),
        ],
        "pin it with a type argument (`lib.Holder.build[SomeType](...)`)",
    );
    files_ok(&[
        hlib,
        (
            "main.chz",
            "import lib\nfn main():\n    print(lib.Holder.build[lib.Counter]().n)\nmain()\n",
        ),
    ]);
    // …and through an ALIAS, which is the prefix the user actually wrote.
    files_reject(
        &[
            hlib,
            (
                "main.chz",
                "import lib as L\nfn main():\n    x := L.Holder.build()\nmain()\n",
            ),
        ],
        "pin it with a type argument (`L.Holder.build[SomeType](...)`)",
    );
}

/// M24-3 — a type parameter SHADOWS an outer type of the same name in static-call position, like
/// Rust (`fn f<Item: D>(_x: Item) -> i32 { Item::tag() }` with `f(Counter)` prints `1`, measured
/// 2026-08-10). The checker used to resolve the STRUCT there. The neighbours matter as much as the
/// case: a shadowing param whose bound does NOT declare the method must say so rather than silently
/// falling back to the struct (rustc: "no associated function named `tag` found for type parameter
/// `Item`"), and a NON-shadowing program must be untouched.
#[test]
fn witness_type_param_shadows_a_same_named_type_in_static_call_position() {
    const SHADOW_HEAD: &str = "protocol Tagged:\n    fn tag() -> int\nstruct Item:\n    q: int\n    fn tag() -> int:\n        return 7\nstruct Counter:\n    n: int\n    fn tag() -> int:\n        return 1\n";
    // the param wins — and it type-checks as the param, so a `Counter` argument fits
    entry_ok(&format!(
        "{SHADOW_HEAD}fn f[Item: Tagged](x: Item) -> int:\n    return Item.tag()\nfn main():\n    print(f(Counter(1)))\nmain()\n"
    ));
    // the bound does NOT declare it → the param's own diagnostic, never the struct's method
    entry_rejects(
        &format!(
            "{SHADOW_HEAD}protocol Other:\n    fn other() -> int\nfn f[Item: Other](x: Item) -> int:\n    return Item.tag()\nfn main():\n    print(f(Counter(1)))\nmain()\n"
        ),
        "no bound on 'Item' declares a static method 'tag'",
    );
    // the ANNOTATION position already bound the param; both positions now agree
    entry_ok(&format!(
        "{FWD_HEAD}fn g[Counter: Default](x: Counter) -> Counter:\n    return Counter.default()\nfn main():\n    print(g(Tag(\"hi\")).s)\nmain()\n"
    ));
    // NON-shadowing: a bare struct name that is not a type param still reaches its static method
    entry_ok(&format!(
        "{SHADOW_HEAD}fn f[T: Tagged](x: T) -> int:\n    return T.tag() + Item.tag()\nfn main():\n    print(f(Counter(1)))\nmain()\n"
    ));
}

/// The shadow is a RULE, not one spelling. Round 3 applied it only to bare `T.member(args)`, so the
/// SAME name meant two different things in one expression (`Item.tag() * 100 + Item[int].tag()` →
/// `107`: the param answered `1`, the struct answered `7`). Every type-name position resolves an
/// in-scope type parameter to the PARAMETER. Rust is the reference and rejects each of these on the
/// parameter (measured 2026-08-10 with rustc): `Item::<i32>::tag()` → E0109 *"type arguments are not
/// allowed on type parameter `Item`"*, `let _y: Item = Item(99)` → E0308 *"expected type parameter
/// `Item`, found struct `Item`"*, `Col::Red` → E0599 *"no associated function or constant named
/// `Red` found for type parameter `Col`"*.
#[test]
fn witness_type_param_shadows_in_every_type_name_position() {
    const H: &str = "protocol Tagged:\n    fn tag() -> int\nstruct Item[K]:\n    q: K\n    fn tag() -> int:\n        return 7\nenum Col:\n    Red\n    Blue\n    fn tag() -> int:\n        return 9\nstruct Counter:\n    n: int\n    fn tag() -> int:\n        return 1\n";
    const SHADOW: &str = "resolves to the generic type parameter";
    // the type-level turbofish
    entry_rejects(
        &format!(
            "{H}fn f[Item: Tagged](x: Item) -> int:\n    return Item[int].tag()\nfn main():\n    print(f(Counter(1)))\nmain()\n"
        ),
        SHADOW,
    );
    // the bare constructor
    entry_rejects(
        &format!(
            "{H}fn f[Item: Tagged](x: Item) -> int:\n    y := Item(99)\n    return 1\nfn main():\n    print(f(Counter(1)))\nmain()\n"
        ),
        SHADOW,
    );
    // a nullary variant of a shadowed enum
    entry_rejects(
        &format!(
            "{H}fn f[Col: Tagged](x: Col) -> int:\n    y := Col.Red\n    return 1\nfn main():\n    print(f(Counter(1)))\nmain()\n"
        ),
        SHADOW,
    );
    // …and NONE of those spellings is disturbed when no type parameter shadows the name
    entry_ok(&format!(
        "{H}fn f[T: Tagged](x: T) -> int:\n    y := Item[int](3)\n    z := Col.Red\n    return T.tag() + Item[int].tag() + y.q\nfn main():\n    print(f(Counter(1)))\nmain()\n"
    ));
    // Chezzi has ONE namespace, so the parameter shadows a same-named FUNCTION for the whole body
    // too — which is Go's answer, not Rust's (Rust splits type and value namespaces, so its
    // `foo()` stays the function). Measured 2026-08-10: `func fv[foo any](x foo) int { return
    // foo() }` → *"missing argument in conversion to foo"*. It used to call the function, so one
    // name meant two things here as well.
    entry_rejects(
        "fn foo() -> int:\n    return 5\nfn f[foo](x: foo) -> int:\n    return foo()\nfn main():\n    print(f(1))\nmain()\n",
        SHADOW,
    );
    // …including the BARE-VALUE position, which is where the shadow leaked back out: `g := foo`
    // read the shadowed FUNCTION while `foo()` two lines up resolved to the parameter, and a module
    // GLOBAL did the same (`lookup` reaches scope 0). Go: *"foo (type) is not an expression"*.
    entry_rejects(
        "fn foo() -> int:\n    return 5\nfn f[foo](x: foo) -> int:\n    g := foo\n    return g()\nfn main():\n    print(f(1))\nmain()\n",
        SHADOW,
    );
    entry_rejects(
        "LIM := 10\nfn f[LIM](x: LIM) -> int:\n    return LIM + 1\nfn main():\n    print(f(1))\nmain()\n",
        SHADOW,
    );
    // A module GLOBAL named like the parameter, called bare: still exactly one shadow error.
    let errs = check_entry(
        "foo := fn() -> int: 5\nfn f[foo](x: foo) -> int:\n    return foo()\nprint(f(1))\n",
    );
    assert!(
        errs.len() == 1 && errs[0].message.contains(SHADOW),
        "expected exactly one shadow error, got: {errs:?}"
    );
    // A real LOCAL binding still wins over the parameter — that is the ordinary value/type split,
    // not this rule. (A module GLOBAL does NOT: the parameter is the inner scope.)
    entry_ok(
        "struct Bxx:\n    n: int\nfn f[Bxx](x: Bxx) -> int:\n    Bxx := 3\n    return Bxx\nfn main():\n    print(f(1))\nmain()\n",
    );
}

/// …and every diagnostic in the family must say the name resolved to the TYPE PARAMETER and why
/// that is a dead end, never prescribe a bound that does not help. Both messages below were wrong:
/// an UNBOUNDED param was told to bound itself by "a protocol that requires `fn tag(...) -> Self`"
/// for a method returning `int` (rustc names the trait that already declares the item instead), and
/// a param of the ENCLOSING TYPE got that same advice even though no bound there can ever carry a
/// witness — following it produced a second rejection.
#[test]
fn witness_static_call_dead_ends_name_the_type_parameter() {
    const H: &str = "protocol Tagged:\n    fn tag() -> int\nstruct Item:\n    q: int\n    fn tag() -> int:\n        return 7\n";
    // UNBOUNDED param: name the protocol that already declares the static method (rustc's
    // "the following trait defines an item `tag`, perhaps you need to restrict type parameter
    // `Item` with it"), never an invented `-> Self` signature.
    let errs = check_entry(&format!(
        "{H}fn f[Item](x: Item) -> int:\n    return Item.tag()\nfn main():\n    print(f(1))\nmain()\n"
    ));
    let msg = errs
        .iter()
        .find(|e| e.message.contains("no bound on 'Item'"))
        .map(|e| e.message.clone())
        .unwrap_or_else(|| panic!("expected the no-bound error, got: {errs:?}"));
    assert!(
        msg.contains("'Tagged'"),
        "must name the protocol that declares a static 'tag', got: {msg}"
    );
    assert!(
        !msg.contains("-> Self"),
        "must not invent a signature for a `-> int` requirement, got: {msg}"
    );
    // ENCLOSING-TYPE param: say where it is declared and that only a MEMBER-level one can be
    // witnessed — never "bound it", which cannot help.
    let errs = check_entry(&format!(
        "{H}struct Bx[Item]:\n    k: Item\n    fn peek(self) -> int:\n        return Item.tag()\nfn main():\n    print(Bx(1).peek())\nmain()\n"
    ));
    let msg = errs
        .iter()
        .find(|e| e.message.contains("enclosing type 'Bx'"))
        .map(|e| e.message.clone())
        .unwrap_or_else(|| panic!("expected the enclosing-type error, got: {errs:?}"));
    assert!(
        !msg.contains("bound 'Item' by"),
        "must not prescribe a bound that cannot help, got: {msg}"
    );
    // …and the workaround it names really compiles.
    entry_ok(&format!(
        "{H}struct Bx[K]:\n    k: K\n    fn peek_of[Item: Tagged](self, x: Item) -> int:\n        return Item.tag()\nfn main():\n    print(Bx(1).peek_of(Item(2)))\nmain()\n"
    ));
}

// G — the `spawn`/`defer` witness-target rejection (and the de-dup test that pinned it to ONE
// message) is gone with M24-5: the hidden argument is threaded at those emit sites, so there is
// nothing left to refuse.

// ---------------------------------------------------------------------------------------------
// W7-43 — the `?.` carrier-mode table's key.
// ---------------------------------------------------------------------------------------------

#[test]
fn carrier_key_is_a_pure_tuple_build() {
    // Single-derivation contract: the checker's record site and the compiler's lookup site both
    // call this one helper, so it may never do anything but assemble its four arguments. Pinning
    // that is the point — a "clever" normalization inside it would desynchronize the two halves.
    let frag_ctx = Span {
        line: 3,
        col: 7,
        file: 0,
    };
    let name_span = Span {
        line: 9,
        col: 2,
        file: 0,
    };
    assert_eq!(
        carrier_key(4, frag_ctx, 2, name_span),
        (4, frag_ctx, 2, name_span)
    );
}

#[test]
fn chained_opt_chain_links_get_distinct_carrier_keys() {
    // `parse_postfix` takes `let span = e.span;` ONCE before the postfix match, so BOTH links of
    // `a?.b?.c` carry the primary `a`'s span. Keying the mode table on that span would alias them
    // — and a mixed chain (`a: Result`, `a.b: Option`) has two links whose modes DIFFER, so the
    // later insert would win and the compiler would emit the wrong lowering under a green check.
    // The name TOKEN is distinct per link, which is why it is the key's fourth component.
    let module = parser::parse(lexer::tokenize("a?.b?.c\n").expect("lex")).expect("parse");
    let StmtKind::Expr(outer) = &module.stmts[0].kind else {
        panic!(
            "expected an expression statement, got {:?}",
            module.stmts[0]
        );
    };
    let ExprKind::OptChain {
        obj,
        name_span: outer_name,
        ..
    } = &outer.kind
    else {
        panic!("expected an OptChain, got {:?}", outer.kind);
    };
    let ExprKind::OptChain {
        name_span: inner_name,
        ..
    } = &obj.kind
    else {
        panic!("expected a nested OptChain, got {:?}", obj.kind);
    };
    // The premise: one shared node span, two distinct name tokens.
    assert_eq!(outer.span, obj.span, "both links share the primary's span");
    let ctx = Span::default();
    assert_ne!(
        carrier_key(0, ctx, 0, *outer_name),
        carrier_key(0, ctx, 0, *inner_name),
        "chained `?.` links must not alias onto one CarrierKey"
    );
}
