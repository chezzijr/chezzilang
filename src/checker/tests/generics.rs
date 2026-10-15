//! Checker tests: generics, structural protocols, call-site type args, Stringable/Hashable/numeric
//! protocols, generic operator overloading.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== generics & structural protocols (G1) =====

const POINT: &str = "\
struct Point:
    x: int
    y: int
    fn compare(self, other: Point) -> int:
        return (self.x + self.y) - (other.x + other.y)
    fn eq(self, other: Point) -> bool:
        return (self.x + self.y) == (other.x + other.y)
";

#[test]
fn generic_max_over_int_ok() {
    ok(
        "fn max[T: Comparable](a: T, b: T) -> T:\n    if a < b:\n        return b\n    return a\nm := max(3, 7)\n",
    );
}

/// M23/B3 — `Comparable` embeds `Eq`, but scalars must stay unaffected: int/float/str get
/// `Comparable` from the intrinsic-grant early-out in `satisfies_args_d` (`protocol == "Comparable"
/// && matches!(ty, Ty::Int | Ty::Float | Ty::Str)`), so a `[T: Comparable]` generic must still accept
/// all three WITHOUT requiring a user-defined `eq` (there is none to define on a scalar).
#[test]
fn generic_max_over_float_and_str_ok_after_eq_embed() {
    ok(
        "fn max[T: Comparable](a: T, b: T) -> T:\n    if a < b:\n        return b\n    return a\nf := max(1.5, 2.5)\ns := max(\"a\", \"b\")\n",
    );
}

#[test]
fn generic_max_over_comparable_struct_ok() {
    let src = format!(
        "{POINT}fn max[T: Comparable](a: T, b: T) -> T:\n    if a < b:\n        return b\n    return a\np := max(Point(1, 2), Point(3, 0))\n"
    );
    ok(&src);
}

#[test]
fn generic_max_result_type_is_substituted() {
    // The result of max(3,7) is int, so a str-typed binding must be rejected.
    rejects(
        "fn max[T: Comparable](a: T, b: T) -> T:\n    return a\nx: str = max(3, 7)\n",
        "cannot assign int",
    );
}

#[test]
fn ordering_on_unbounded_type_param_rejected() {
    rejects(
        "fn pick[T](a: T, b: T) -> T:\n    if a < b:\n        return b\n    return a\n",
        "cannot compare",
    );
}

#[test]
fn calling_comparable_generic_on_non_comparable_struct_rejected() {
    // Plain (no `compare`/`eq` method) ⇒ does not satisfy Comparable (via its `Eq` embed).
    let src = "\
struct Plain:
    n: int
fn max[T: Comparable](a: T, b: T) -> T:
    return a
p := max(Plain(1), Plain(2))
";
    rejects(
        src,
        "does not satisfy Comparable (missing method 'compare')",
    );
}

#[test]
fn generic_call_with_mismatched_type_args_rejected() {
    rejects(
        "fn max[T: Comparable](a: T, b: T) -> T:\n    return a\nx := max(3, \"a\")\n",
        "expected int",
    );
}

#[test]
fn parameterized_protocol_bound_infers_its_arg_from_the_witnessing_method() {
    const SRC: &str = "\
protocol Produces[R]:
    fn produce(self) -> R

struct IntProducer:
    fn produce(self) -> int:
        return 7

fn produce_as[R, T: Produces[R]](x: T) -> R:
    return x.produce()
";

    // `T` binds to `IntProducer` from the argument; `R` must then fall out of `IntProducer.produce`'s
    // own return type, exactly as Rust infers `impl Produces<i32> for IntProducer` with no
    // annotation. This used to blame the perfectly valid `produce` for a "wrong signature".
    ok(&format!("{SRC}\nx := produce_as(IntProducer())\n"));

    // …and `R` must be the CONCRETE `int`, not an `Unknown` that merely stops the complaint — the
    // `+ 1` is what a laundered `Unknown` would sail through.
    ok(&format!("{SRC}\nx := produce_as(IntProducer()) + 1\n"));

    // The pre-existing escapes keep working, and keep precedence over the recovery.
    ok(&format!(
        "{SRC}\nx: int = produce_as(IntProducer())\ny := produce_as[int, IntProducer](IntProducer())\n"
    ));

    // Every protocol param is recovered independently, including one no argument mentions.
    ok("\
protocol Pair[A, B]:
    fn first(self) -> A
    fn second(self) -> B

struct P:
    fn first(self) -> int:
        return 1
    fn second(self) -> str:
        return \"s\"

fn both[A, B, T: Pair[A, B]](x: T) -> A:
    return x.first()
y := both(P()) + 1
");

    // `StrProducer` genuinely satisfies `Produces[str]`, so the conformance is REAL and the error
    // belongs to the annotation, not to `produce`. Rust reports the identical shape on the same
    // program — `expected i32, found String` (E0308), not a trait error — where Chezzi used to
    // blame `produce` for a "wrong signature". Recovering `R` is what moved the blame to the truth.
    rejects(
        &format!(
            "{SRC}\nstruct StrProducer:\n    fn produce(self) -> str:\n        return \"no\"\nx: int = produce_as(StrProducer())\n"
        ),
        "cannot assign str to variable of type int",
    );

    // An explicit binding wins over the recovery: `R` is pinned to `str` by `seed`, so the
    // recovered `int` is dropped and the real conformance failure is still reported.
    rejects(
        "\
protocol Produces[R]:
    fn produce(self) -> R
struct IntProducer:
    fn produce(self) -> int:
        return 7
fn produce_as[R, T: Produces[R]](x: T, seed: R) -> R:
    return x.produce()
y := produce_as(IntProducer(), \"hello\")
",
        "method 'produce' has the wrong signature",
    );

    // Not a general ban on a free result param — it just has to be pinnable, and here it is.
    ok("fn empty[T]() -> List[T]:\n    return []\nxs: List[int] = empty()\n");
    ok("fn empty[T]() -> List[T]:\n    return []\nxs := empty[int]()\n");
}

/// The same recovery on the generic-METHOD path, which has no `seed_from_hint` — before this,
/// even a result annotation could not pin `R` there (`y: int = w.produce_as(…)` reported BOTH a
/// false "wrong signature" and "cannot assign R to variable of type int"); only turbofish worked.
#[test]
fn parameterized_protocol_bound_infers_its_arg_on_the_method_path() {
    const SRC: &str = "\
protocol Produces[R]:
    fn produce(self) -> R

struct IntProducer:
    fn produce(self) -> int:
        return 7

struct Wrapper:
    fn produce_as[R, T: Produces[R]](self, x: T) -> R:
        return x.produce()
";
    ok(&format!(
        "{SRC}\ny := Wrapper().produce_as(IntProducer()) + 1\n"
    ));
    ok(&format!(
        "{SRC}\ny: int = Wrapper().produce_as(IntProducer())\n"
    ));
    ok(&format!(
        "{SRC}\ny := Wrapper().produce_as[int, IntProducer](IntProducer())\n"
    ));
}

/// The method path reaches the same verdict as the free-fn path on every shape — it now has BOTH
/// halves the free-fn path had: `seed_from_hint` (so a result annotation can pin a return-only
/// param) and the probe-gated inference diagnostic. Before, a recovery miss on a method call printed
/// the ORIGINAL false `has the wrong signature` message, or nothing at all with the param left free.
#[test]
fn the_method_path_matches_the_free_fn_path_on_every_shape() {
    // (a) a BUILTIN witness. The annotation pins `R` here, as on a free fn (measured before that:
    //     `wrong signature` PLUS `cannot assign R to variable of type Option[int]`) — and since the
    //     recovery now reads the native method table too, the UN-annotated spelling works as well.
    ok("\
protocol Popper[R]:
    fn pop(self) -> R
struct W:
    fn take[R, T: Popper[R]](self, x: T) -> R:
        return x.pop()
v: int? = W().take([1, 2, 3])
");
    ok("\
protocol Popper[R]:
    fn pop(self) -> R
struct W:
    fn take[R, T: Popper[R]](self, x: T) -> R:
        return x.pop()
v := W().take([1, 2, 3])
");

    // (b) nothing can reach `R` -> NAME it, rather than reporting `ok: no type errors` with `R` free.
    rejects(
        "\
protocol Tagged[R]:
    fn tag(self) -> str
struct S:
    fn tag(self) -> str:
        return \"s\"
struct W:
    fn go[R, T: Tagged[R]](self, x: T) -> R:
        return self.go(x)
y := W().go(S())
",
        "cannot infer type parameter R for 'go'; add a result annotation or explicit type arguments",
    );

    // (c) the too-wide-gate neighbours, on this path too: `R` is un-inferred in each, but the method
    //     cannot conform for ANY instantiation, so the true message must be the ONLY one.
    let only = |src: &str, needle: &str| {
        let errs = check_src(src);
        assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
        assert!(
            errs[0].message.contains(needle),
            "expected {needle:?}: {errs:?}"
        );
        assert!(
            !errs[0].message.contains("cannot infer type parameter"),
            "the un-inferred param is not the cause here: {errs:?}"
        );
    };
    const P: &str = "\
protocol Produces[R]:
    fn produce(self) -> R
struct W:
    fn go[R, T: Produces[R]](self, x: T) -> R:
        return x.produce()
";
    only(
        &format!(
            "{P}struct NoMethod:\n    fn other(self) -> int:\n        return 7\ny := W().go(NoMethod())\n"
        ),
        "missing method 'produce'",
    );
    only(
        &format!(
            "{P}struct ArityBad:\n    fn produce(self, extra: int) -> int:\n        return 7\ny := W().go(ArityBad())\n"
        ),
        "method 'produce' has the wrong signature",
    );
}

/// The bound-arg recovery reaches a BUILTIN witness and an EMBEDDED protocol. Both used to give an
/// honest `cannot infer R` with working escapes — no wrong answers, but an annotation the ancestor
/// does not ask for. Each `ok` below was measured as that error before.
#[test]
fn the_bound_arg_recovery_reaches_builtin_and_embedded_witnesses() {
    const POPPER: &str = "\
protocol Popper[R]:
    fn pop(self) -> R

fn take[R, T: Popper[R]](x: T) -> R:
    return x.pop()
";
    // A builtin conforms through the native table, whose sigs have the RECEIVER STRIPPED; the
    // recovery rebuilds it prepended, the way `satisfies_native` does. Without that the arity test
    // is off by one and nothing is recovered.
    ok(&format!("{POPPER}v := take([1, 2, 3])\n"));
    ok(&format!("{POPPER}v := take([\"a\"])\n"));
    // A builtin that genuinely does not conform still reports its own true message, alone.
    let only = |src: &str, needle: &str| {
        let errs = check_src(src);
        assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
        assert!(
            errs[0].message.contains(needle),
            "expected {needle:?}: {errs:?}"
        );
        assert!(
            !errs[0].message.contains("cannot infer type parameter"),
            "the un-inferred param is not the cause here: {errs:?}"
        );
    };
    only(&format!("{POPPER}v := take(1)\n"), "missing method 'pop'");
    only(
        &format!("{POPPER}v := take(\"abc\")\n"),
        "missing method 'pop'",
    );

    // An EMBEDDED protocol supplies the requirement. The embed's param is named `S` where the
    // embedded protocol names it `R`, so this passes only if `protocol_method_sig` re-spelled the
    // signature in `Q`'s vocabulary — a name-coincidence version would prove nothing.
    const EMBED: &str = "\
protocol Produces[R]:
    fn produce(self) -> R

protocol Q[S]:
    Produces[S]

struct IntProducer:
    fn produce(self) -> int:
        return 7

fn use_q[S, T: Q[S]](x: T) -> S:
    return x.produce()
";
    ok(&format!("{EMBED}v := use_q(IntProducer())\n"));
    // …and the recovered `S` is the CONCRETE `int`, not a universally-assignable hole.
    rejects(
        &format!("{EMBED}v: str = use_q(IntProducer())\n"),
        "cannot assign int to variable of type str",
    );
    // A type that does not satisfy the EMBEDDED requirement still reports that, alone.
    only(
        &format!(
            "{EMBED}struct NoProduce:\n    fn other(self) -> int:\n        return 1\nv := use_q(NoProduce())\n"
        ),
        "missing method 'produce'",
    );

    // A protocol EXISTENTIAL witnesses out of the protocol's own requirement signatures. Measured
    // before: `cannot infer type parameter R for 'produce_as'`, while the annotated spelling ran.
    const EXIST: &str = "protocol Produces[R]:
    fn produce(self) -> R

struct IntProducer:
    fn produce(self) -> int:
        return 7

fn produce_as[R, T: Produces[R]](x: T) -> R:
    return x.produce()

p: Produces[int] = IntProducer()
";
    ok(&format!(
        "{EXIST}n := produce_as(p) + 1
"
    ));
    // …and the recovered `R` is the CONCRETE carried arg, not a hole.
    rejects(
        &format!(
            "{EXIST}s: str = produce_as(p)
"
        ),
        "cannot assign int to variable of type str",
    );
}

/// An annotation REACHES THROUGH a collection literal onto each element. It did not, and the gap was
/// not merely a missing inference: an element whose type carried an `Unknown` laundered the WHOLE
/// literal, because the expected-type gate passes an `Unknown`-carrying element and the bottom-up
/// fallback then finds it compatible with anything. Both silent wrong answers below were measured
/// check-clean at rc=0 and faulted at run time.
#[test]
fn d3_an_annotation_reaches_through_a_collection_literal() {
    const EMPTY: &str = "fn empty_list[E]() -> List[E]:\n    return []\n";

    // `a[1][0] + 1` was `ok: no type errors`, then *cannot apply Add to str and int* at run time.
    // Note the SAME literal without the generic call (`[["x"]]`) was already correctly rejected —
    // the `empty()` element is what laundered it.
    rejects(
        &format!("{EMPTY}a: List[List[int]] = [empty_list(), [\"x\"]]\nn := a[1][0] + 1\n"),
        "list element: expected int, found str",
    );
    // The `Map` twin, identical mechanism: *cannot apply Add to str and int* at run time.
    rejects(
        &format!(
            "{EMPTY}m: Map[str, List[int]] = {{\"k\": empty_list(), \"j\": [\"x\"]}}\nn := m[\"j\"][0] + 1\n"
        ),
        "list element: expected int, found str",
    );
    // …and the `Set` twin, which reaches it through the element hint rather than a homogeneity check.
    // (TICKET-106 W12-7: `infer_set` now checks each element's ASSIGNABILITY to the expected element
    // type per-entry, mirroring `infer_map`, so the message is "set element: expected …" rather than
    // the whole-collection "cannot assign Set[str] to variable of type Set[int]".)
    rejects(
        "fn one[T](x: T) -> T:\n    return x\ns: Set[int] = {one(\"a\")}\n",
        "set element: expected int, found str",
    );

    // The AGREEING shapes must be untouched — this is a hint, not a new rejection rule.
    // (D3: an int at a `float` element slot is the one new rejection, with the `write 1.0` note.)
    ok(&format!("{EMPTY}a: List[List[int]] = [empty_list()]\n"));
    ok(&format!(
        "{EMPTY}m: Map[str, List[int]] = {{\"k\": empty_list()}}\n"
    ));
    ok("fn one[T](x: T) -> T:\n    return x\ns: Set[int] = {one(1), one(2)}\n");

    // Every other expected-type-directed position, measured unchanged: the `Any` top type (whose
    // element widen is deliberately suppressed), int→float widening in both a list and a map value
    // column, a protocol element slot, a struct ctor argument, a declared return, and the
    // synthesized variadic pack.
    ok("xs: List[Any] = [1, \"a\", true]\n");
    rejects("xs: List[float] = [1, 2, 2.5]\n", "write 1.0");
    rejects("m: Map[str, float] = {\"a\": 1, \"b\": 2.5}\n", "write 1.0");
    ok(
        "protocol Show:\n    fn show(self) -> str\nstruct A:\n    n: int\n    fn show(self) -> str:\n        return \"a\"\nstruct B:\n    fn show(self) -> str:\n        return \"b\"\nxs: List[Show] = [A(1), B()]\n",
    );
    ok("struct Bag:\n    items: List[int]\nb := Bag([1, 2])\n");
    ok("fn mk() -> List[List[int]]:\n    return [[1], []]\n");
    // (the variadic pack is synthesized by desugar, so this one needs the desugaring helper)
    ok_desugared(
        "fn f(...xs: int) -> int:\n    return xs.len()\nfn main():\n    print(f(1, 2, 3))\n",
    );
    // A genuinely mistyped literal keeps its own diagnostic.
    rejects(
        "a: List[List[int]] = [[\"x\"]]\n",
        "list element: expected int, found str",
    );

    // The hint must reach through a `T?` / `T!E` sink too — a bare literal there coerces to
    // `Some(v)`/`Ok(v)` (W8-21), so the carrier's PAYLOAD is the literal's real expected type.
    // Measured before the unwrap, check-clean at rc=0 and faulting at run time:
    rejects(
        &format!(
            "{EMPTY}fn mk() -> List[List[int]]?:\n    return [empty_list(), [\"x\"]]\nfn go() -> int?:\n    xs := mk()?\n    return xs[1][0] + 1\n"
        ),
        "list element: expected int, found str",
    );
    // …and it un-breaks a FALSE REJECTION that predates the propagation: a protocol element slot
    // behind a carrier was *list elements differ: C vs S* where the bare slot accepts it.
    ok(
        "protocol Shape:\n    fn area(self) -> int\nstruct C:\n    fn area(self) -> int:\n        return 1\nstruct S:\n    fn area(self) -> int:\n        return 2\nfn opt() -> List[Shape]?:\n    return [C(), S()]\n",
    );

    // CEILING CLOSED (TICKET-032 A2): the hint used to reach only an element that CONSUMES it, so
    // one method call away the same literal laundered. Reporting per ELEMENT instead of falling
    // through to bottom-up homogeneity closes it.
    rejects(
        &format!("{EMPTY}a: List[List[int]] = [empty_list().reversed(), [\"x\"]]\n"),
        "list element: expected int, found str",
    );

    // CEILING, second (TICKET-033): the int→float element widen does not reach through a carrier,
    // because the license is computed from the SINK TYPE before `slot_payload` unwraps it — a
    // `List[float]?` sink resolves to `Ty::Option(..)`, on which `float_elem_hint_ty` answers `None`
    // by construction, never `Ty::List(Float)`. `docs/spec.md` and `docs/syntax.md` document this as
    // deliberate.
    rejects(
        "fn f() -> List[float]?:\n    return [1, 2]\n",
        "list element: expected float, found int",
    );
}

/// TICKET-033: the int->float ELEMENT widen fires only at an annotated `let`. The same widen must
/// also fire at a call argument, a `return`, and a struct ctor argument — the SCALAR widen is
/// already accepted at all three. Currently all three wrongly reject: `float_elem_hint_ty`'s
/// checker-side widen and its compiler twin `Compiler::elem_hint` are installed at exactly one
/// site, `StmtKind::Let`. This test asserts the CORRECT (fixed) behavior, so it fails today.
#[test]
fn d3_ticket_033_element_int_rejected_at_non_let_sinks() {
    rejects(
        "fn f(xs: List[float]):\n    print(xs)\nfn main():\n    f([1, 2])\n",
        "write 1.0",
    );
    rejects("fn f() -> List[float]:\n    return [1, 2]\n", "write 1.0");
    rejects(
        "struct S:\n    v: List[float]\nfn main():\n    S([1, 2])\n",
        "write 1.0",
    );
}

/// TICKET-033: every remaining position a slot type reaches a literal also licenses the widen — a
/// method argument, a desugared keyword argument, the `Map[str, float]` twin at each sink, and a
/// whole-collection alias sink.
#[test]
fn d3_element_int_rejected_at_every_argument_and_return_sink() {
    rejects(
        "struct S:\n    fn m(self, xs: List[float]):\n        print(xs)\nfn main():\n    S().m([1, 2])\n",
        "write 1.0",
    );
    // keyword calls only desugar under the harvest pass (`harvest_keywords`), which `check_graph`
    // runs and the bare `check()` `ok()` helper does not — use `entry_rejects`.
    entry_rejects(
        "fn f(xs: List[float], n: int):\n    print(xs, n)\nfn main():\n    f(n = 1, xs = [1, 2])\n",
        "write 1.0",
    );
    rejects(
        "fn f(m: Map[str, float]):\n    print(m)\nfn main():\n    f({\"a\": 1})\n",
        "write 1.0",
    );
    rejects(
        "struct S:\n    v: Map[str, float]\nfn main():\n    S({\"a\": 1})\n",
        "write 1.0",
    );
    rejects(
        "fn f() -> Map[str, float]:\n    return {\"a\": 1}\n",
        "write 1.0",
    );
    rejects(
        "type LF = List[float]\nfn f(xs: LF):\n    print(xs)\nfn main():\n    f([1, 2])\n",
        "write 1.0",
    );
}

/// TICKET-033: a carrier (`List[float]?`), a nested element (`List[List[float]]`), an erased
/// generic-param slot and a reassignment all stay declined — the widen only licenses the sink types
/// `float_elem_hint_ty` maps directly, computed BEFORE any carrier unwrap.
#[test]
fn element_widen_still_declines_carrier_nested_and_erased_sinks() {
    rejects(
        "fn f() -> List[float]?:\n    return [1, 2]\n",
        "list element: expected float, found int",
    );
    rejects(
        "fn f(xs: List[float]?):\n    print(xs)\nfn main():\n    f([1, 2])\n",
        "list element: expected float, found int",
    );
    rejects(
        "fn f(xs: List[List[float]]):\n    print(xs)\nfn main():\n    f([[1, 2]])\n",
        "list element: expected float, found int",
    );
    rejects(
        "xs: List[int] = [1, 2]\nfn f(zs: List[float]):\n    print(zs)\nfn main():\n    xs = [1, 2]\n    f(xs)\n",
        "expected List[float]",
    );
    entry_rejects(
        "fn f(...zs: float):\n    print(zs)\nfn main():\n    f(1, 2)\n",
        "list element: expected float, found int",
    );
}

/// TICKET-094 defect B — the element widen must reach a parameter default, a struct-field default,
/// a `Map[_, float]` value default and a method default, matching the scalar default's widen and
/// `docs/syntax.md`'s stated sink list.
#[test]
fn d3_element_int_rejected_at_parameter_and_field_defaults() {
    rejects(
        "fn f(xs: List[float] = [1, 2]):\n    print(xs)\nfn main():\n    f()\n",
        "write 1.0",
    );
    entry_rejects(
        "struct S:\n    v: List[float] = [1, 2]\nfn main():\n    print(S().v)\n",
        "write 1.0",
    );
    rejects(
        "fn f(m: Map[str, float] = {\"a\": 1}):\n    print(m)\nfn main():\n    f()\n",
        "write 1.0",
    );
    entry_rejects(
        "struct S:\n    fn m(self, xs: List[float] = [1, 2.5]):\n        print(xs)\nfn main():\n    S().m()\n",
        "write 1.0",
    );
}

/// TICKET-094 defect C — a generic callee must keep expected-type inference on every CONCRETE
/// argument slot (a slot declared `float`/`List[float]`/`Map[str, float]`/`List[Any]`, never the
/// type variable itself), matching what the same slot does on a non-generic twin. Includes two
/// REGRESSION PINS the human approval note required (2026-09-08): row 9 is a NON-generic method on
/// a generic struct, which already widened before this ticket and must keep doing so; row 10 is a
/// generic fn's RETURN position, which already widened before this ticket via the `sig.rs` return
/// sink this ticket's defect-A/B fix also touches.
#[test]
fn d3_generic_callee_rejects_int_on_concrete_float_slots() {
    rejects(
        "fn g[T](a: float, b: T) -> float:\n    return a\nfn main():\n    print(g(1, \"x\"))\n",
        "write 1.0",
    );
    rejects(
        "fn g[T](a: List[float], b: T) -> List[float]:\n    return a\nfn main():\n    print(g([1, 2], \"x\"))\n",
        "write 1.0",
    );
    rejects(
        "fn g[T](a: Map[str, float], b: T) -> Map[str, float]:\n    return a\nfn main():\n    print(g({\"a\": 1}, \"x\"))\n",
        "write 1.0",
    );
    ok(
        "fn g[T](a: List[Any], b: T) -> List[Any]:\n    return a\nfn main():\n    print(g([1, -2.5], \"x\"))\n",
    );
    ok(
        "fn g[T](a: List[Any], b: T) -> List[Any]:\n    return a\nfn main():\n    print(g([1, \"a\"], \"x\"))\n",
    );
    rejects(
        "struct S:\n    fn g[U](self, x: float, u: U) -> float:\n        return x\nfn main():\n    print(S().g(1, \"z\"))\n",
        "write 1.0",
    );
    rejects(
        "struct S:\n    fn mk[U](x: float, u: U) -> float:\n        return x\nfn main():\n    print(S.mk(1, \"z\"))\n",
        "write 1.0",
    );
    rejects(
        "struct P[T]:\n    a: float\n    b: T\nfn main():\n    p := P(1, \"x\")\n    print(p.a)\n",
        "write 1.0",
    );
    // Regression pin 9 (human approval note) — a NON-generic method on a generic struct.
    rejects(
        "struct P[T]:\n    v: T\n    fn g(self, xs: List[float]) -> List[float]:\n        return xs\nfn main():\n    p := P(1)\n    print(p.g([1, 2]))\n",
        "write 1.0",
    );
    // Regression pin 10 (human approval note) — a generic fn's RETURN position.
    rejects(
        "fn f[T](b: T) -> List[float]:\n    return [1, 2]\nfn main():\n    print(f(\"x\"))\n",
        "write 1.0",
    );
}

/// TICKET-094 defect C, negative half — the generic-erased slot exception must keep declining: a
/// slot declared as the callee's OWN type variable `T`, never a concrete slot, is never widened.
/// (TICKET-106 W12-15: a bare `T` slot with an untyped int CONSTANT beside a `float`-binding
/// sibling argument is the one narrow exception — Go's `Max(1, 2.5)` widens the same way — so
/// `g(1, 2.5)` now accepts with `T = float`. The other three cases here are untouched: none is a
/// bare-`T`-slot untyped-int-constant-beside-a-float shape.)
#[test]
fn d3_generic_callee_still_declines_an_erased_slot() {
    rejects(
        "fn g[T](a: T, b: T) -> T:\n    return a\nfn main():\n    print(g(1, 2.5))\n",
        "write 1.0",
    );
    rejects(
        "struct Box[T]:\n    v: T\n    fn set(self, x: T):\n        self.v = x\nfn main():\n    Box(1.0).set(1)\n",
        "expected float, found int",
    );
    entry_rejects(
        "fn f(...zs: float):\n    print(zs)\nfn main():\n    f(1, 2)\n",
        "list element: expected float, found int",
    );
    rejects(
        "fn g[T](a: List[float], b: T) -> List[float]:\n    return a\nfn main():\n    print(g([1, \"a\"], \"x\"))\n",
        "str",
    );
}

/// A GENERIC method cannot witness a protocol requirement. Its signature is spelled in binders that
/// exist in no scope the requirement can see, so `method_matches` was comparing two independently
/// scoped `Ty::Param`s by their NAME STRING — and alpha-renaming, which must never change meaning,
/// did. Rust refuses the same shape outright (`E0049: method 'show' has 1 type parameter but its
/// trait declaration has 0`, measured), and `satisfies_native` has always refused it for builtins.
#[test]
fn a_generic_method_cannot_witness_a_protocol() {
    // THE CAPTURE. Measured before this rule: the first spelling was `ok: no type errors` and the
    // second — differing ONLY in the witness method's own binder name — was rejected.
    let src = |own: &str| {
        format!(
            "\
protocol Sink[U]:
    fn put(self, v: U) -> int

struct GS:
    fn put[{own}](self, v: {own}) -> int:
        return 1

fn use_sink[U, T: Sink[U]](x: T) -> int:
    return 0

n := use_sink(GS())
"
        )
    };
    for own in ["U", "W"] {
        rejects(
            &src(own),
            "method 'put' is generic and cannot witness a protocol requirement",
        );
    }

    // The cost of the narrowing, stated: a generic method witnessing a NON-parameterized protocol
    // used to check clean and print `g`. Rust rejects the identical program with E0049, so this is
    // drift removed, not a feature lost.
    rejects(
        "\
protocol Show:
    fn show(self) -> str

struct G:
    fn show[U](self) -> str:
        return \"g\"

fn render[T: Show](x: T) -> str:
    return x.show()

s := render(G())
",
        "method 'show' is generic and cannot witness a protocol requirement",
    );

    // …and a NON-generic witness of the same shape is untouched.
    ok("\
protocol Show:
    fn show(self) -> str

struct G:
    fn show(self) -> str:
        return \"g\"

fn render[T: Show](x: T) -> str:
    return x.show()

s := render(G())
");
}

/// The inference diagnostic's gate is a PROBE, not a structural test. In each of these `R` is also
/// un-inferred, but the method cannot conform for ANY instantiation — so "add a result annotation"
/// is advice that does not work (measured: annotating each of these still fails), and the true
/// conformance error must be the ONLY one reported. A structural gate reads green on the tests
/// above while getting every one of these wrong.
#[test]
fn uninferred_protocol_param_never_masks_a_real_conformance_failure() {
    const P: &str = "\
protocol Produces[R]:
    fn produce(self) -> R

fn produce_as[R, T: Produces[R]](x: T) -> R:
    return x.produce()
";
    let only = |src: &str, needle: &str| {
        let errs = check_src(src);
        assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
        assert!(
            errs[0].message.contains(needle),
            "expected {needle:?}, got: {errs:?}"
        );
        assert!(
            !errs[0].message.contains("cannot infer type parameter"),
            "the un-inferred param is not the cause here: {errs:?}"
        );
    };

    // The method is absent entirely.
    only(
        &format!(
            "{P}\nstruct NoMethod:\n    fn other(self) -> int:\n        return 7\ny := produce_as(NoMethod())\n"
        ),
        "missing method 'produce'",
    );

    // Wrong ARITY — no instantiation of `R` can fix it.
    only(
        &format!(
            "{P}\nstruct ArityBad:\n    fn produce(self, extra: int) -> int:\n        return 7\ny := produce_as(ArityBad())\n"
        ),
        "method 'produce' has the wrong signature",
    );

    // Wrong PARAM type, with `R` still only in return position.
    only(
        "\
protocol Conv[R]:
    fn conv(self, k: str) -> R
struct C:
    fn conv(self, k: int) -> int:
        return 7
fn go[R, T: Conv[R]](x: T) -> R:
    return x.conv(\"a\")
y := go(C())
",
        "method 'conv' has the wrong signature",
    );

    // A non-parameterized protocol is untouched by any of this.
    only(
        "\
protocol Show:
    fn show(self) -> str
struct Bad:
    fn show(self) -> int:
        return 1
fn p[T: Show](x: T) -> str:
    return x.show()
y := p(Bad())
",
        "method 'show' has the wrong signature",
    );
}

/// A GENERIC witness method must recover NOTHING. Its signature is spelled in terms of its own
/// type params, which exist in no scope the caller can see, so binding one into the call's
/// substitution is name capture: measured on the first cut, `fn produce[U](self) -> List[U]`
/// witnessing `Produces[R]` pinned `R = List[U]` and that `U` leaked out as `expected List[U],
/// found List[W]` — a binder present in no caller scope. `satisfies_native` already refuses a
/// generic method as a witness outright, for exactly this reason; this is that policy on the
/// recovery side.
///
/// The matching capture in the CONFORMANCE check is now closed too, so a generic witness is refused
/// outright and this call's diagnostic names that reason instead of the downstream inference miss.
/// Both are correct; the conformance one is the true cause. See `a_generic_method_cannot_witness_a_protocol`.
#[test]
fn a_generic_witness_method_recovers_nothing() {
    const SRC: &str = "\
protocol Produces[R]:
    fn produce(self) -> R

struct G:
    fn produce[U](self) -> List[U]:
        return []

fn produce_as[R, T: Produces[R]](x: T) -> R:
    return x.produce()
";
    // The recovery must leave `R` free rather than bind it to `G`'s own `U`; the conformance check
    // then refuses the generic witness and names THAT as the cause. What this test really guards is
    // the line below — no diagnostic may mention a binder from a scope the caller cannot see.
    let errs = check_src(&format!("{SRC}\ny := produce_as(G())\n"));
    assert!(
        errs.iter().any(|e| e
            .message
            .contains("is generic and cannot witness a protocol requirement")),
        "expected the generic-witness refusal, got: {errs:?}"
    );
    for e in &errs {
        assert!(
            !e.message.contains("List[U]"),
            "leaked the witness method's own binder into the call site: {e:?}"
        );
    }
}

/// BOTH pre-`enforce_bounds` passes resolve the bound args speculatively — they only decide what to
/// bind/probe — so neither may leave diagnostics behind for `enforce_bounds` to report a second time.
/// Measured on the first cut: a bad bound arg printed `unknown type 'Bogus'` twice on the SAME
/// call-site span.
///
/// The two cases below are NOT redundant, and the first version of this test had only the second.
/// `recover_protocol_args` runs on every call; the PROBE runs only when the signature has a
/// result-only type param, so a `-> int` signature exercises one pass and silently skips the other.
/// That is exactly how the first fix shipped with the probe's duplicate still live and this test
/// green — a widening is untested by its own suite unless the test hits every pass it widened.
#[test]
fn neither_speculative_pass_duplicates_bound_arg_diagnostics() {
    // TICKET-079: `unknown type` now carets the type name's own span (line 6, where `Bogus` is
    // written in the bound), not the call-site span (line 8) `resolve_bound_arg` used to pass
    // through — the duplicate-diagnostic question this test asks is unaffected by which line.
    let at_call_site = |src: &str| -> usize {
        let errs = check_src(src);
        let n = errs
            .iter()
            .filter(|e| e.message.contains("Bogus") && e.span.line == 6)
            .count();
        assert!(
            n > 0,
            "expected the bad bound arg at the bound's own span: {errs:?}"
        );
        n
    };
    const HEAD: &str = "\
protocol Produces[R]:
    fn produce(self) -> R
struct IntP:
    fn produce(self) -> int:
        return 1
";
    // (a) recovery pass only — no result-only type param, so the probe never runs.
    assert_eq!(
        at_call_site(&format!(
            "{HEAD}fn go[T: Produces[Bogus]](x: T) -> int:\n    return 1\ny := go(IntP())\n"
        )),
        1,
        "recovery pass duplicated the bound-arg diagnostic"
    );
    // (b) recovery pass AND the probe — `R` is result-only, which is what turns the probe on.
    assert_eq!(
        at_call_site(&format!(
            "{HEAD}fn go[R, T: Produces[Bogus]](x: T) -> R:\n    return x.produce()\ny := go(IntP())\n"
        )),
        1,
        "the probe duplicated the bound-arg diagnostic"
    );
}

/// TICKET-032 A2 — `infer_list`'s expected-type gate is all-or-nothing: one element assignable to the
/// expected type and one that is not (an `Unknown`-cored empty-collection producer) abandons the
/// expected-type path, falls through to bottom-up homogeneity, and `compatible(List[Unknown],
/// List[str])` is true there, laundering the whole literal to `List[List[Unknown]]`. Measured on
/// `b87db7d7`: `chezzi check` says `ok: no type errors`, then `chezzi run` faults `cannot apply Add to
/// str and int`. `infer_map` (`m: Map[str, List[int]] = {...}`) carries the identical gate.
#[test]
fn a_list_or_map_literal_with_one_unrefined_empty_producer_element_is_not_laundered() {
    const EMPTY_FN: &str = "fn empty[T]() -> List[T]:\n    xs: List[T] = []\n    return xs\n";

    rejects(
        &format!(
            "{EMPTY_FN}a: List[List[int]] = [empty().reversed(), [\"x\"]]\nprint(a[1][0] + 1)\n"
        ),
        "expected",
    );

    rejects(
        &format!(
            "{EMPTY_FN}m: Map[str, List[int]] = {{\"k\": empty().reversed(), \"j\": [\"x\"]}}\nprint(m[\"j\"][0] + 1)\n"
        ),
        "expected",
    );
}

/// TICKET-032 A2 — the two NON-literal shapes `infer_list`/`infer_map`'s per-element/per-entry loop
/// is the only guard for: an element/value that is a plain IDENT reading an already-typed binding
/// (not a literal), so `infer_list`'s own inner recursion into a nested literal never runs. Both
/// measured `ok: no type errors` at `6365d13a`, then faulting at run time with *cannot apply Add to
/// str and int* at `:4:6`.
#[test]
fn a_literal_element_is_reported_against_the_declared_element_type() {
    rejects(
        "other := [\"x\"]\na: List[List[int]] = [empty_list().reversed(), other]\nfn empty_list() -> List[int]:\n    return []\n",
        "list element: expected List[int], found List[str]",
    );
    rejects(
        "other := [\"x\"]\nm: Map[str, List[int]] = {\"k\": empty_list().reversed(), \"j\": other}\nfn empty_list() -> List[int]:\n    return []\n",
        "map value: expected List[int], found List[str]",
    );
    // gotcha 1: the comprehension hint barrier — the outer sink type must not reach a clause iterand.
    ok("ys: List[int] = [y for xs in [[1, 2], [3]] for y in xs]\n");
}

/// A DECL-SITE default copy is checked once at the declaration, where the enclosing generic's params
/// are not bound yet; the expression's real home is the provider / the splice at each call. Firing
/// the return-only rule there rejected a declaration that is correct at every real call site.
///
/// A DECL-SITE default copy is checked once at the declaration, where the enclosing generic's params
/// are not bound yet; the expression's real home is the provider / the splice at each call. Firing
/// the return-only rule there rejected a declaration that is correct at every real call site.
///
/// The exemption IS load-bearing — without it this reports `cannot infer type parameter for 'mkl'`.
/// It is asserted with the provider's own binder named BOTH ways: the decl-site comparison used to be
/// by NAME, so `mkl[T]` against `G[T]` was accepted while the alpha-renamed `mkl[Z]` gave *default
/// value for parameter 'xs': expected List[G[T]], found List[G[Z]]*. See
/// `a_decl_site_default_binder_is_alpha_renameable`.
#[test]
fn a_decl_site_default_is_exempt_from_the_return_only_rule() {
    for own in ["T", "Z"] {
        ok_desugared(&format!(
            "struct G[T]:\n    v: T\n    fn tot[U](self, u: U, xs: List[Self] = mkl()) -> int:\n        return xs.len()\nfn mkl[{own}]() -> List[G[{own}]]:\n    return []\nfn main():\n    print(G(1).tot(2))\n"
        ));
    }
}

/// **A generic provider's own binders are universally quantified at the decl site**, so the
/// comparison there must resolve them against the SLOT, never match them by spelling. It used to
/// match by spelling: one declaration, two verdicts, decided by a letter.
///
/// Seeding the declared type as the hint resolves them — `seed_from_hint` unifies the provider's
/// return against the slot — and, because the binding then EXISTS, `enforce_bounds` inside the call
/// can finally check the provider's own `where` bound, which nothing checked before. A genuinely
/// wrong default still fails (`a_decl_site_default_is_still_type_checked`).
#[test]
fn a_decl_site_default_binder_is_alpha_renameable() {
    // PARAMETER default. Measured before: `T` → `ok`, `Z` → *expected List[G[T]], found List[G[Z]]*.
    for own in ["T", "Z"] {
        ok_desugared(&format!(
            "struct G[T]:\n    v: T\n    fn tot[U](self, u: U, xs: List[G[T]] = mkl()) -> int:\n        return xs.len()\nfn mkl[{own}]() -> List[G[{own}]]:\n    return []\nfn main():\n    print(G(1).tot(2))\n"
        ));
    }
    // FIELD default, same seam: both spellings must reach the SAME verdict. (That verdict is a
    // separate, pre-existing limitation — a generic struct's provider-defaulted field is not
    // optional in the positional ctor — measured identical for `T` on both binaries; what this
    // pins is that the two spellings AGREE, which they did not before.)
    let field = |own: &str| {
        format!(
            "struct G[T]:\n    v: T\nfn mkl[{own}]() -> List[G[{own}]]:\n    return []\nstruct Holder[T]:\n    n: int\n    items: List[G[T]] = mkl()\nfn main():\n    h := Holder(1)\n    print(h.items.len())\n"
        )
    };
    let msgs: Vec<String> = ["T", "Z"]
        .iter()
        .map(|own| {
            check_src(&field(own))
                .iter()
                .map(|e| e.message.clone())
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .collect();
    assert_eq!(
        msgs[0], msgs[1],
        "alpha-renaming the provider's binder changed the verdict: {msgs:?}"
    );

    // The provider's own `where` BOUND is enforced now that the binding exists. Before, nothing
    // checked it on EITHER spelling — `fn mkl[Z: Show]() -> List[G[Z]]` defaulting a `List[G[T]]`
    // was accepted with nothing showing `T: Show`, while the SAME call written out was correctly
    // rejected (*type int does not satisfy Show*). Both spellings are asserted.
    const SHOW: &str = "protocol Show:\n    fn show(self) -> str\nstruct A:\n    fn show(self) -> str:\n        return \"a\"\nstruct G[T]:\n    v: T\n";
    for own in ["T", "Z"] {
        rejects(
            &format!(
                "{SHOW}fn mkl[{own}: Show]() -> List[G[{own}]]:\n    return []\nstruct H[T]:\n    fn tot(self, xs: List[G[T]] = mkl()) -> int:\n        return xs.len()\n"
            ),
            "type T does not satisfy Show",
        );
    }
    // …and the escape works: give the ENCLOSING param the bound and it compiles and runs. Both of
    // these were rejected before, by the name capture.
    ok_desugared(&format!(
        "{SHOW}fn mkl[Z: Show]() -> List[G[Z]]:\n    return []\nstruct H[T: Show]:\n    fn tot(self, xs: List[G[T]] = mkl()) -> int:\n        return xs.len()\nfn main():\n    print(H[A]().tot())\n"
    ));
    // An UNBOUNDED provider under a bounded enclosing param is fine — the bound is the provider's
    // to demand, not to inherit.
    ok_desugared(&format!(
        "{SHOW}fn mkl[Z]() -> List[G[Z]]:\n    return []\nstruct H[T: Show]:\n    fn tot(self, xs: List[G[T]] = mkl()) -> int:\n        return xs.len()\nfn main():\n    print(H[A]().tot())\n"
    ));

    // The provider need not be the WHOLE default. The first cut gated the seed on
    // `matches!(def.kind, ExprKind::Call { .. })` — a syntactic test where the rule is about the
    // TYPE — so a call inside anything fell back to the spelling-matched comparison. Both of these
    // were spelling-dependent then: `= idl(mkl())` gave `ok` for `mkl[T]` and *expected List[G[T]],
    // found List[G[Z]]* for `mkl[Z]`, and the wrapped BOUND shape went unenforced.
    const NEST: &str =
        "struct G[T]:\n    v: T\nfn idl[A](xs: List[A]) -> List[A]:\n    return xs\n";
    for own in ["T", "Z"] {
        ok_desugared(&format!(
            "{NEST}fn mkl[{own}]() -> List[G[{own}]]:\n    return []\nstruct H[T]:\n    fn tot(self, xs: List[G[T]] = idl(mkl())) -> int:\n        return xs.len()\nfn main():\n    print(H[int]().tot())\n"
        ));
        // …and a slice of the provider call, the other non-tail position.
        ok_desugared(&format!(
            "struct G[T]:\n    v: T\nfn mkl[{own}]() -> List[G[{own}]]:\n    return []\nstruct H[T]:\n    fn tot(self, xs: List[G[T]] = mkl()[0..0]) -> int:\n        return xs.len()\nfn main():\n    print(H[int]().tot())\n"
        ));
        // The wrapped BOUND shape is enforced now too — it was accepted before, on both spellings.
        rejects(
            &format!(
                "{SHOW}fn mkl[{own}: Show]() -> List[G[{own}]]:\n    return []\nstruct H[T]:\n    fn tot(self, xs: List[G[T]] = mkl() + mkl()) -> int:\n        return xs.len()\n"
            ),
            "type T does not satisfy Show",
        );
    }

    // A CLOSURE default at a free-param slot now gets the slot as its hint, so its params bind from
    // it and the TRUE error surfaces instead of a cascade: measured before, *cannot infer type of
    // parameter 'y'; add a type annotation*; now *cannot apply + to T and int*, which is the real
    // problem (`T` has no `Add`).
    rejects(
        "fn f[T](x: T, g: fn(T) -> T = fn(y): y + 1) -> T:\n    return g(x)\n",
        "cannot apply + to T and int",
    );

    // TICKET-034 closed this ceiling by re-installing the hint per binary OPERAND (`infer_binary`,
    // src/checker/pattern.rs), the same hop `infer_if_else_chain` makes per arm. Before it: `mkl[T]`
    // was `ok`, `mkl[Z]` reported *cannot apply + to List[G[T]] and List[G[Z]]* — the first `mkl()`
    // took the slot and the second kept spelling its own binder. Both spellings now agree.
    for own in ["T", "Z"] {
        ok_desugared(&format!(
            "struct G[T]:\n    v: T\nfn mkl[{own}]() -> List[G[{own}]]:\n    return []\nstruct H[T]:\n    fn tot(self, xs: List[G[T]] = mkl() + mkl()) -> int:\n        return xs.len()\nfn main():\n    print(H[int]().tot())\n"
        ));
    }
}

/// W8-47 -- a non-inline default whose slot type mentions a type parameter is refused as arity-
/// filling even when the binder is already PINNED by an explicit turbofish. `min_callable_params`
/// treats the trailing defaulted param as required whenever its declared type mentions a struct/fn
/// type parameter, with no exception for a call site that pins the binder some other way (turbofish
/// or an earlier argument). This is the ctor half of the pair; `w8_47_free_fn_default_arity_is_
/// binder_blind` is the free-fn half.
///
/// Measured before this fix: both `Holder[int](1)` (turbofish-pinned) and `Holder(1)` (unpinned) are
/// rejected identically with *Holder() expects 2 argument(s), got 1*.
///
/// Uses `ok_desugared`, not `ok`: `ok`/`check_src` runs `lexer` + `parser` + `check` and SKIPS
/// desugar, which production always runs (`resolver::build_graph`). This defect's ctor half lives in
/// `desugar`'s slot-fill loop, so under `check_src` every defaulted field is unfilled and this test
/// cannot express the fix at all.
#[test]
fn w8_47_struct_ctor_default_arity_is_binder_blind() {
    let src = "struct G[T]:\n    v: T\nfn mkl[T]() -> List[G[T]]:\n    xs: List[G[T]] = []\n    return xs\nstruct Holder[T]:\n    n: int\n    items: List[G[T]] = mkl()\nfn main():\n    h := Holder[int](1)\n    print(h.n)\nmain()\n";
    ok_desugared(src);
}

/// W8-47 -- the free-fn half of the same defect: `fn conv[T](a: int, b: List[T] = mkl())` called as
/// `conv[int](1)`, binder pinned by turbofish, is rejected the same as the unpinned `conv(1)`.
#[test]
fn w8_47_free_fn_default_arity_is_binder_blind() {
    let src = "struct G[T]:\n    v: T\nfn mkl[T]() -> List[G[T]]:\n    xs: List[G[T]] = []\n    return xs\nfn conv[T](a: int, b: List[G[T]] = mkl()) -> int:\n    return a\nfn main():\n    print(conv[int](1))\nmain()\n";
    ok(src);
}

/// W8-47's free-fn fix reads `sig.min_params`, computed by `ast::min_callable_params`. That fn
/// returns `params.len()` (the FULL arity, no shrink) whenever the sig carries witness params, so a
/// witness-taking callee keeps its exact arity refusal even though its trailing default is
/// callee-filled. Pin: `conv() expects 2 argument(s), got 1` must still fire here.
///
/// Goes red if step 2's gate is widened to ignore `sig.min_params`, or if
/// `min_callable_params`'s `has_witness_params` early return is deleted.
#[test]
fn w8_47_a_witness_taking_callee_keeps_its_full_arity() {
    rejects_desugared(
        "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(7)\nfn conv[T: Default](a: int, b: List[T] = List()) -> T:\n    return T.default()\nfn f[T: Default](x: T) -> T:\n    return conv(1)\n",
        "conv() expects 2 argument(s), got 1",
    );
}

/// W8-47 -- an UNPINNED generic ctor default is still refused: `Checker::bind_call` has no type
/// arguments to forward, so it leaves the `Dflt::GenericProvider` slot unfilled and the field stays
/// required. Measured before this fix: `Holder() expects 2 argument(s), got 1`. The wording matches
/// Rust's `error[E0282]: type annotations needed for Holder<_>` on the same shape.
///
/// Goes red if step 11's `call_targs.len() != *tps` guard is dropped (the splice would happen,
/// giving `cannot infer type parameter T for 'mkl'` instead) or if step 14's message is dropped
/// (the error would stay `Holder() expects 2 argument(s), got 1`).
#[test]
fn w8_47_an_unpinned_generic_ctor_default_is_still_refused() {
    let src = "struct G[T]:\n    v: T\nfn mkl[T]() -> List[G[T]]:\n    xs: List[G[T]] = []\n    return xs\nstruct Holder[T]:\n    n: int\n    items: List[G[T]] = mkl()\nfn main():\n    h := Holder(1)\n    print(h.n)\nmain()\n";
    rejects_desugared(src, "cannot infer type parameter T for 'Holder'");
}

/// W8-47 -- a BOUNDED struct type parameter keeps today's callee-filled refusal even with a
/// turbofish: a generic provider carrying the bound would raise a witness question `dflt_for` cannot
/// answer before types exist. "Add explicit type arguments" would be FALSE advice here — the
/// turbofish is already written and does not help — so this deliberately keeps the plain arity
/// message instead of the W8-45-style wording.
///
/// Goes red if step 8's `otps.iter().all(|t| t.bounds.is_empty())` condition is dropped, because the
/// bounded field would then be spliced.
#[test]
fn w8_47_a_bounded_struct_type_param_keeps_the_callee_filled_refusal() {
    let src = "protocol Show:\n    fn show(self) -> str\nstruct G[T]:\n    v: T\nfn mkl[T]() -> List[G[T]]:\n    xs: List[G[T]] = []\n    return xs\nstruct Holder[T: Show]:\n    n: int\n    items: List[G[T]] = mkl()\nfn main():\n    h := Holder[int](1)\n    print(h.n)\nmain()\n";
    rejects_desugared(src, "Holder() expects 2 argument(s), got 1");
}

/// W8-47's `?` widening: giving a generic struct field a provider means the provider BODY is now
/// judged for a `?` in the default, which the field decl-site copy never did (DEC-025). Measured
/// before this fix: `ok: no type errors`.
///
/// Goes red if step 9 stops emitting a provider for a `Dflt::GenericProvider` field, because the
/// field decl-site copy alone accepts the `?`.
#[test]
fn a_try_in_a_generic_struct_field_default_is_now_judged_by_its_provider() {
    let src = "struct G[T]:\n    v: T\nfn mkl[T]() -> List[G[T]]!str:\n    return !\"no\"\nstruct Holder[T]:\n    n: int\n    items: List[G[T]] = mkl()?\nfn main():\n    h := Holder[int](1)\n    print(h.n)\nmain()\n";
    rejects_desugared(src, "a default expression cannot propagate with");
}

/// A decl-site default is pinned by its slot: a generic fn value there is instantiated instead of
/// reported, when the slot is concrete (`fn(int) -> int`) and when it names only the enclosing
/// item's own params (`fn(U) -> U` inside `fn g[U]` or `struct H[U]`), which pin like concrete
/// types. A default whose callee has a param the slot cannot determine (`two`'s `B`) still reports.
/// Both verdicts are independent of the binder's spelling.
#[test]
fn a_decl_site_default_is_pinned_by_its_slot() {
    ok_desugared(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn run(x: int, f: fn(int) -> int = ident) -> int:\n    return f(x)\n\nfn main():\n    print(run(5))\n",
    );
    for own in ["U", "T"] {
        ok_desugared(&format!(
            "fn ident[{own}](x: {own}) -> {own}:\n    return x\nfn g[U](x: U, f: fn(U) -> U = ident) -> U:\n    return f(x)\nprint(g(1))\n"
        ));
        ok_desugared(&format!(
            "fn ident[{own}](x: {own}) -> {own}:\n    return x\nstruct H[U]:\n    n: U\n    f: fn(U) -> U = ident\nh := H[int](3)\nprint(h.f(h.n))\n"
        ));
    }
    for own in ["U", "T"] {
        rejects(
            &format!(
                "fn two[{own}, B](a: {own}) -> {own}:\n    return a\nfn g[U](x: U, f: fn(U) -> U = two) -> U:\n    return f(x)\nn := g(1)\n"
            ),
            "is generic and",
        );
    }
}

/// A decl-site default is still CHECKED — seeding the declared type as a hint must not turn the
/// check into a rubber stamp. Each of these is a genuinely wrong-typed default and stays rejected.
#[test]
fn a_decl_site_default_is_still_type_checked() {
    rejects(
        "fn f(n: int = \"x\") -> int:\n    return n\n",
        "default value for parameter 'n'",
    );
    rejects(
        "struct S:\n    n: int = \"x\"\n",
        "default value for field 'n'",
    );
    // A generic provider whose return genuinely cannot fill the slot is still caught, hint or not.
    rejects(
        "struct G[T]:\n    v: T\nfn mkl[Z]() -> List[G[Z]]:\n    return []\nfn f(xs: List[int] = mkl()) -> int:\n    return xs.len()\n",
        "default value for parameter 'xs'",
    );
    // A CONCRETE slot pins the provider rather than rubber-stamping it: a wrong element type there
    // is still a mismatch even though the hint bound the provider's own param.
    rejects(
        "fn mkl[Z]() -> List[Z]:\n    return []\nfn f(xs: List[int] = mkl()) -> int:\n    return xs.len()\nfn g(ys: List[str] = mkl()) -> int:\n    return f(ys)\n",
        "expected List[int], found List[str]",
    );
}

/// The residual the diagnostic still owns: a protocol param named by NO requirement method, so
/// there is nothing to recover it from and the bound conforms with a hole in it.
#[test]
fn protocol_param_no_method_mentions_reports_the_inference_failure() {
    rejects(
        "\
protocol Tagged[R]:
    fn tag(self) -> str
struct S:
    fn tag(self) -> str:
        return \"s\"
fn go[R, T: Tagged[R]](x: T) -> R:
    return x.tag()
y := go(S())
",
        "cannot infer type parameter R for 'go'; add a result annotation or explicit type arguments",
    );
}

#[test]
fn unbounded_generic_passthrough_ok() {
    ok("fn first[T](a: T, b: T) -> T:\n    return a\nx := first(1, 2)\ny := first(\"a\", \"b\")\n");
}

// ===== explicit call-site type arguments (`max[int](…)`) =====

const MAX_FN: &str = "fn max[T: Comparable](a: T, b: T) -> T:\n    return a\n";

#[test]
fn explicit_type_args_fn_ok() {
    // Pin T=int explicitly; result is int, so an int-typed binding is fine.
    ok(&format!("{MAX_FN}x: int = max[int](3, 7)\n"));
}

#[test]
fn explicit_type_args_pin_result_type() {
    // `max[int]` returns int → a str binding must be rejected (proves the pin flows to the return).
    rejects(
        &format!("{MAX_FN}x: str = max[int](3, 7)\n"),
        "cannot assign int",
    );
}

#[test]
fn explicit_type_args_mismatch_rejected() {
    // T pinned to str, but the args are int → argument type error.
    rejects(&format!("{MAX_FN}x := max[str](3, 7)\n"), "expected str");
}

#[test]
fn explicit_type_args_wrong_count_rejected() {
    rejects(
        &format!("{MAX_FN}x := max[int, int](3, 7)\n"),
        "expects 1 type argument",
    );
}

#[test]
fn explicit_type_args_on_non_generic_rejected() {
    rejects(
        "fn inc(a: int) -> int:\n    return a + 1\nx := inc[int](3)\n",
        "takes no type arguments",
    );
}

#[test]
fn explicit_type_args_struct_ok() {
    let src = "\
struct Pair[A, B]:
    a: A
    b: B
p := Pair[int, str](1, \"a\")
";
    ok(src);
}

#[test]
fn explicit_type_args_struct_mismatch_rejected() {
    let src = "\
struct Pair[A, B]:
    a: A
    b: B
p := Pair[str, str](1, \"a\")
";
    rejects(src, "expected str");
}

#[test]
fn explicit_type_args_struct_ok_via_graph() {
    // REGRESSION: `name_is_generic` looked up `structs` by the BARE name, but under the real
    // `check_graph` path (which `chezzi check`/`run` use) user structs are keyed by the
    // module-prefixed `bare_key`. So a generic struct ctor with explicit call-site type args was
    // wrongly reported non-generic and rejected with "takes no type arguments" — even though the
    // struct-ctor branch fully supports them. `explicit_type_args_struct_ok` missed this because
    // `ok()`/`check_src` use the single-module path where keys stay bare. Must use `entry_ok`.
    entry_ok(
        "\
struct Pair[A, B]:
    a: A
    b: B
struct Box[T]:
    v: T
p := Pair[int, str](1, \"one\")
b := Box[int](3)
print(p.a)
print(b.v)
",
    );
}

#[test]
fn ordering_on_comparable_struct_directly_ok() {
    ok(&format!("{POINT}b := Point(1, 2) < Point(3, 4)\n"));
}

#[test]
fn ordering_on_non_comparable_struct_rejected() {
    rejects(
        "struct Plain:\n    n: int\nb := Plain(1) < Plain(2)\n",
        "cannot compare",
    );
}

#[test]
fn calling_protocol_method_on_type_param_ok() {
    ok("fn cmp[T: Comparable](a: T, b: T) -> int:\n    return a.compare(b)\n");
}

#[test]
fn user_protocol_bound_ok() {
    let src = "\
protocol Shape:
    fn area(self) -> float
struct Circle:
    r: float
    fn area(self) -> float:
        return self.r
fn biggest[T: Shape](a: T) -> float:
    return a.area()
x := biggest(Circle(2.0))
";
    ok(src);
}

#[test]
fn unknown_protocol_bound_rejected() {
    rejects(
        "fn f[T: Bogus](a: T) -> T:\n    return a\n",
        "unknown protocol 'Bogus'",
    );
}

#[test]
fn scalar_where_bound_is_equality_constraint() {
    // `where T: <scalar>` is an EQUALITY bound, not a protocol — `T` must be exactly that scalar.
    ok("fn f[T: bool](a: T) -> T:\n    return a\nfn main():\n    print(f(true))\n");
    ok("fn f[T: str](a: T) -> T:\n    return a\nfn main():\n    print(f(\"hi\"))\n");
    rejects(
        "fn f[T: bool](a: T) -> T:\n    return a\nfn main():\n    print(f(5))\n",
        "expected bool, found int",
    );
    // A scalar bound takes no type args.
    rejects(
        "fn f[T: bool[int]](a: T) -> T:\n    return a\n",
        "takes no type arguments",
    );
}

#[test]
fn container_where_bound_is_head_constructor_constraint() {
    // `where T: List/Map/Set` is a CONSTRUCTOR-KIND bound — `T`'s head must be exactly that container
    // (element/key/value types free). The surface form of the RwShared read-view gate.
    ok("fn f[T: List](a: T) -> T:\n    return a\nfn main():\n    print(f([1, 2, 3]))\n");
    ok("fn f[T: Map](a: T) -> T:\n    return a\nfn main():\n    print(f({\"a\": 1}))\n");
    ok("fn f[T: Set](a: T) -> T:\n    return a\nfn main():\n    print(f(Set([1, 2])))\n");
    rejects(
        "fn f[T: List](a: T) -> T:\n    return a\nfn main():\n    print(f(5))\n",
        "expected List[...], found int",
    );
    rejects(
        "fn f[T: Map](a: T) -> T:\n    return a\nfn main():\n    print(f([1, 2]))\n",
        "expected Map[...], found",
    );
    // A container bound takes no type args (no element binder).
    rejects(
        "fn f[T: List[int]](a: T) -> T:\n    return a\n",
        "takes no type arguments",
    );
}

#[test]
fn channel_trip_gated_to_bool() {
    // `trip()` is `where T: bool` (its level-trigger latch only ever delivers `bool true`), so it is
    // sound only on `Channel[bool]` — the hole where `Channel[int].trip(); .recv()` leaked a `bool`
    // through an `int`-typed value is now a compile error.
    ok("fn main():\n    c := Channel[bool]()\n    c.trip()\n    print(c.recv())\n");
    rejects(
        "fn main():\n    c := Channel[int]()\n    c.trip()\n    print(c.recv())\n",
        "expected bool, found int",
    );
}

#[test]
fn redeclaring_comparable_rejected() {
    rejects(
        "protocol Comparable:\n    fn compare(self, other: Self) -> int\n",
        "reserved",
    );
}

#[test]
fn redeclaring_convert_rejected() {
    // `Convert` is a reserved (builtin) protocol (slice 1 of the Convert/From type-conversion work) —
    // a user `protocol Convert[S]:` redeclaration is rejected, exactly like `Comparable`. Both the
    // parameterized form (matching the prelude shape) and a bare `protocol Convert:` are reserved on
    // the name alone.
    rejects(
        "protocol Convert[S]:\n    fn convert(x: S) -> Self\n",
        "reserved",
    );
    rejects(
        "protocol Convert:\n    fn convert(x: int) -> int\n",
        "reserved",
    );
}

// ----- Stringable protocol (M10-G1) -----

#[test]
fn stringable_struct_satisfies_ok() {
    let src = "\
struct Point:
    x: int
    y: int
    fn str(self) -> str:
        return \"({self.x}, {self.y})\"
fn show[T: Stringable](v: T) -> str:
    return v.str()
s := show(Point(1, 2))
";
    ok(src);
}

#[test]
fn stringable_wrong_signature_is_invisible_to_the_bound() {
    // `str` returns int, not str: it is not the hook, so `print` and the bound ignore it (the type
    // is graded like one with no `str`; TICKET-257, pinned by `hook_conformance_grid`). The run
    // prints the default display, never the int.
    let src = "\
struct Bad:
    n: int
    fn str(self) -> int:
        return self.n
fn show[T: Stringable](v: T) -> str:
    return v.str()
x := show(Bad(1))
";
    ok(src);
}

#[test]
fn stringable_bare_struct_and_non_hook_str_accepted() {
    // TICKET-257 (owner rule U5): `print`, `str()` and `"{x}"` accept every value, so a struct with
    // NO `str` method satisfies `Stringable`, and so does one whose `str` is not the hook (a method
    // that is not the hook is invisible to `print`; `hook_conformance_grid` pins the same).
    let bare = "\
struct Bare:
    a: int
fn show[T: Stringable](v: T) -> str:
    return v.str()
x := show(Bare(1))
";
    ok(bare);
    let wrong = "\
struct Bad:
    a: int
    fn str(self) -> int:
        return 1
fn show[T: Stringable](v: T) -> str:
    return v.str()
x := show(Bad(1))
";
    ok(wrong);
}

#[test]
fn stringable_scalar_satisfies_ok() {
    // int/float/bool/str intrinsically satisfy Stringable (sole method `str(self) -> str`),
    // mirroring the Comparable/Hashable/Add intrinsic scalar arms.
    for lit in ["show(5)", "show(3.14)", "show(true)", "show(\"hi\")"] {
        let src = format!("fn show[T: Stringable](v: T) -> str:\n    return v.str()\ns := {lit}\n");
        ok(&src);
    }
}

#[test]
fn scalar_str_direct_call_is_bound_only() {
    // The intrinsic Stringable arm is BOUND-only, exactly like the Comparable arm: a direct
    // concrete-receiver `(5).str()` stays a compile error (no direct scalar method), matching
    // `(5).compare(3)`. The free `str(5)` builtin covers direct use. Keeps str consistent with
    // every sibling intrinsic protocol (none adds a first-class direct scalar method).
    rejects("x := (5).str()\n", "type int has no method 'str'");
    rejects("x := (5).compare(3)\n", "type int has no method 'compare'");
}

#[test]
fn redeclaring_stringable_rejected() {
    rejects(
        "protocol Stringable:\n    fn str(self) -> str\n",
        "reserved",
    );
}

// ----- Hashable struct keys for map/set (key restriction lifted) -----

/// A Hashable struct (defines `hash(self) -> int`) usable as a map key / set element.
const POINT_H: &str = "\
struct Point:
    x: int
    y: int
    fn hash(self) -> int:
        return self.x * 31 + self.y
";

#[test]
fn struct_with_hash_is_valid_map_key() {
    ok(&format!(
        "{POINT_H}m: Map[Point, str] = {{}}\nm[Point(1, 2)] = \"a\"\n"
    ));
}

#[test]
fn set_of_hashable_struct_ok() {
    ok(&format!(
        "{POINT_H}s: Set[Point] = Set()\ns.add(Point(1, 2))\n"
    ));
}

#[test]
fn struct_without_hash_rejected_as_map_key() {
    let src = "struct Bare:\n    a: int\nm: Map[Bare, int] = {}\n";
    rejects(src, "Map key type must implement Hashable");
}

#[test]
fn struct_without_hash_rejected_as_set_element() {
    let src = "struct Bare:\n    a: int\ns: Set[Bare] = Set()\n";
    rejects(src, "Set element type must implement Hashable");
}

#[test]
fn float_still_rejected_as_map_key() {
    rejects(
        "m: Map[float, int] = {}\n",
        "Map key type must implement Hashable",
    );
}

#[test]
fn float_still_rejected_as_set_element() {
    rejects(
        "s: Set[float] = Set()\n",
        "Set element type must implement Hashable",
    );
}

#[test]
fn non_hashable_map_key_reports_exactly_once() {
    // TICKET-044: `resolve_type` on a single-name annotated `let` ran twice (once to seed the
    // expected-type hint, once for the declared type), so `key_ty_reject` fired twice for one
    // annotation and the diagnostic was byte-identically duplicated.
    let errs = check_src("m: Map[float, str] = {}\n");
    let matching: Vec<_> = errs
        .iter()
        .filter(|e| e.message.contains("Map key type must implement Hashable"))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one Hashable diagnostic, got: {errs:?}"
    );
}

#[test]
fn non_hashable_set_element_reports_exactly_once() {
    let errs = check_src("s: Set[(int, float)] = Set()\n");
    let matching: Vec<_> = errs
        .iter()
        .filter(|e| {
            e.message
                .contains("Set element type must implement Hashable")
        })
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one Hashable diagnostic, got: {errs:?}"
    );
}

#[test]
fn two_distinct_hashable_violations_both_report() {
    // Guard against a fix that dedupes too aggressively: two genuinely different violations must
    // still produce two diagnostics.
    let errs = check_src("a: Map[float, str] = {}\nb: Map[float, int] = {}\n");
    let matching: Vec<_> = errs
        .iter()
        .filter(|e| e.message.contains("Map key type must implement Hashable"))
        .collect();
    assert_eq!(
        matching.len(),
        2,
        "expected two Hashable diagnostics (one per statement), got: {errs:?}"
    );
}

#[test]
fn annotated_let_unknown_type_reports_exactly_once() {
    // Regression for TICKET-044: the duplication is not Hashable-specific. Any diagnostic
    // `resolve_type` emits while resolving an annotated `let` was doubled.
    let errs = check_src("x: Nope = 1\n");
    let matching: Vec<_> = errs
        .iter()
        .filter(|e| e.message.contains("unknown type 'Nope'"))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one unknown-type diagnostic, got: {errs:?}"
    );
}

#[test]
fn non_hashable_annotation_in_a_re_walked_body_reports_exactly_once() {
    // The checker walks a fn body more than once (generic-argument prepass, `infer_fn_ret`,
    // DEC-033). Neither re-walk should add another copy of the diagnostic.
    let sources = [
        "fn g[T](x: T):\n    m: Map[float, str] = {}\n\ng(1)\n",
        "fn h() -> int:\n    m: Map[float, str] = {}\n    return 1\n\nprint(h())\n",
    ];
    for src in sources {
        let errs = check_src(src);
        let matching: Vec<_> = errs
            .iter()
            .filter(|e| e.message.contains("Map key type must implement Hashable"))
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "expected exactly one Hashable diagnostic for {src:?}, got: {errs:?}"
        );
    }
}

#[test]
fn annotation_and_ctor_each_report_their_own_hashable_error() {
    // Two distinct occurrences in one statement (the annotation and the constructor's own type
    // argument) must both survive; a fix that dedupes by message or span must not swallow either.
    let errs = check_src("s: Set[(int, float)] = Set[(int, float)]()\n");
    let cols: Vec<u32> = errs
        .iter()
        .filter(|e| {
            e.message
                .contains("Set element type must implement Hashable")
        })
        .map(|e| e.span.col)
        .collect();
    assert_eq!(cols, vec![1u32, 24]);
}

// ----- Hashable protocol (M10-G2: bound; M10 map-model: wired to map/set keys) -----

#[test]
fn hashable_struct_satisfies_ok() {
    let src = "\
struct Id:
    n: int
    fn hash(self) -> int:
        return self.n
fn keyed[T: Hashable](v: T) -> int:
    return v.hash()
x := keyed(Id(7))
";
    ok(src);
}

#[test]
fn hashable_intrinsic_for_scalars_ok() {
    // int/str/bool satisfy Hashable intrinsically, so a `[T: Hashable]` bound accepts them.
    ok(
        "fn keyed[T: Hashable](v: T) -> T:\n    return v\na := keyed(3)\nb := keyed(\"x\")\nc := keyed(true)\n",
    );
}

#[test]
fn hashable_missing_method_rejected() {
    let src = "\
struct Bare:
    a: int
fn keyed[T: Hashable](v: T) -> T:
    return v
x := keyed(Bare(1))
";
    rejects(src, "does not satisfy Hashable");
}

#[test]
fn tuple_key_rejection_names_the_list_element() {
    rejects(
        "m := {(1, [2]): \"a\"}\n",
        "element List[int] is not Hashable",
    );
}

#[test]
fn tuple_key_rejection_names_a_nested_element() {
    rejects(
        "m := {((1, [2]), 3): \"a\"}\n",
        "element List[int] is not Hashable",
    );
}

#[test]
fn tuple_key_rejection_names_a_set_element() {
    rejects(
        "m := {(1, Set([2])): \"a\"}\n",
        "element Set[int] is not Hashable",
    );
}

#[test]
fn tuple_key_rejection_names_a_no_hash_struct() {
    rejects(
        "struct P:\n    x: int\nm := {(1, P(1)): \"a\"}\n",
        "element P is not Hashable",
    );
}

#[test]
fn tuple_key_rejection_names_a_float_element() {
    rejects("m := {(1, 0.0): \"a\"}\n", "element float is not Hashable");
}

#[test]
fn tuple_key_hashable_bound_rejects_an_unhashable_element() {
    rejects(
        "fn f[T: Hashable](x: T) -> int:\n    return x.hash()\nprint(f((1, [2])))\n",
        "element List[int] is not Hashable",
    );
}

#[test]
fn tuple_key_of_hashable_elements_checks_ok() {
    ok("m := {(1, 2): \"a\"}\n");
    ok("m := {(\"a\", true): 1}\n");
    ok("m := {((1, 2), 3): \"a\"}\n");
    ok("s: Set[(int, str)] = Set()\n");
    ok("s := Set([(1, 2)])\n");
    ok("m := {(i, i): i for i in 0..3}\n");
    ok("struct Z:\n    pass\nm := {(1, Z()): \"a\"}\n");
    ok("fn f[T: Hashable](x: T) -> int:\n    return x.hash()\nprint(f((1, 2)))\n");
}

#[test]
fn redeclaring_hashable_rejected() {
    rejects("protocol Hashable:\n    fn hash(self) -> int\n", "reserved");
}

// ----- numeric operator protocols + multi-bound (M10-G3) -----

const VEC2: &str = "\
struct Vec2:
    x: int
    y: int
    fn add(self, other: Vec2) -> Vec2:
        return Vec2(self.x + other.x, self.y + other.y)
    fn mul(self, other: Vec2) -> Vec2:
        return Vec2(self.x * other.x, self.y * other.y)
";

#[test]
fn struct_add_mul_overload_ok() {
    ok(&format!(
        "{VEC2}v := Vec2(1, 2) + Vec2(3, 4) * Vec2(5, 6)\n"
    ));
}

#[test]
fn struct_without_sub_rejects_minus() {
    // Vec2 defines add/mul but not sub ⇒ `-` is not overloaded.
    rejects(
        &format!("{VEC2}v := Vec2(1, 2) - Vec2(3, 4)\n"),
        "cannot apply - to Vec2 and Vec2",
    );
}

#[test]
fn multi_bound_add_mul_ok() {
    ok(&format!(
        "{VEC2}fn fma[T: Add + Mul](a: T, b: T, c: T) -> T:\n    return a + b * c\nv := fma(Vec2(1,2), Vec2(3,4), Vec2(5,6))\nn := fma(2, 3, 4)\n"
    ));
}

#[test]
fn multi_bound_missing_one_protocol_rejected() {
    // Point has add but no mul ⇒ fails the `Mul` half of `T: Add + Mul`.
    let src = "\
struct PointA:
    x: int
    fn add(self, other: PointA) -> PointA:
        return PointA(self.x + other.x)
fn fma[T: Add + Mul](a: T, b: T, c: T) -> T:
    return a + b * c
v := fma(PointA(1), PointA(2), PointA(3))
";
    rejects(src, "does not satisfy Mul");
}

// ----- generic struct/enum operator overloading + protocol satisfaction (the receiving type's
// own type params, e.g. `T->int` from `Box[int]`, must be threaded into the protocol-method
// signature comparison, in addition to `Self` and the protocol's own params) -----

#[test]
fn generic_struct_add_satisfies_and_overloads() {
    // `Box[T]` defines `add(self, o: Box[T]) -> Box[T]`; the `+` operator AND a `T: Add`-bounded
    // generic must both accept `Box[int]`. Previously rejected ("cannot apply + ..." / "does not
    // satisfy Add (method 'add' has the wrong signature)") because `T` was never bound to `int`.
    let src = "\
struct Box[T]:
    v: T
    fn add(self, other: Box[T]) -> Box[T]:
        return Box(self.v)
fn twice[T: Add](x: T) -> T:
    return x + x
a := (Box(5) + Box(10)).v
b := twice(Box(7)).v
";
    entry_ok(src);
}

#[test]
fn generic_struct_neg_and_compare() {
    // Unary `neg`/`-` and `compare`/`<`/Comparable over a generic struct.
    let src = "\
struct Box[T]:
    v: T
    fn neg(self) -> Box[T]:
        return Box(self.v)
    fn compare(self, other: Box[T]) -> int:
        return 0
    fn eq(self, other: Box[T]) -> bool:
        return true
a := (-Box(5)).v
b := Box(5) < Box(10)
fn smallest[T: Comparable](x: T, y: T) -> T:
    if x < y:
        return x
    return y
c := smallest(Box(1), Box(2)).v
";
    entry_ok(src);
}

#[test]
fn generic_enum_add_satisfies() {
    // Generic-enum analogue: `enum Numeral[T]` with `add` overloads `+` and satisfies `Add`.
    let src = "\
enum Numeral[T]:
    Val(T)
    fn add(self, other: Numeral[T]) -> Numeral[T]:
        return self
fn twice[T: Add](x: T) -> T:
    return x + x
a := Numeral.Val(1) + Numeral.Val(2)
b := twice(Numeral.Val(3))
";
    entry_ok(src);
}

#[test]
fn multi_param_generic_operator() {
    // A 2-param generic exercises a multi-entry receiving-type substitution {A->int, B->str}.
    let src = "\
struct Pair[A, B]:
    a: A
    b: B
    fn add(self, other: Pair[A, B]) -> Pair[A, B]:
        return self
p := Pair(1, \"x\") + Pair(2, \"y\")
";
    entry_ok(src);
}

#[test]
fn generic_struct_wrong_add_sig_rejected() {
    // BOUNDARY / soundness: a generic struct whose `add` has a genuinely WRONG signature
    // (`o: int -> int`, not `Box[T] -> Box[T]`) must STILL be rejected — no type laundering.
    let src = "\
struct Box[T]:
    v: T
    fn add(self, o: int) -> int:
        return 0
fn twice[T: Add](x: T) -> T:
    return x + x
v := twice(Box(5))
";
    entry_rejects(src, "does not satisfy Add");
}

#[test]
fn generic_struct_heterogeneous_add_rejected() {
    // BOUNDARY / soundness (no type laundering across type args): `Box[int] + Box[str]` must NOT
    // overload `add`. The user's `add(self, o: Box[T]) -> Box[T]` on a `Box[int]` receiver requires
    // `o: Box[int]`; admitting `Box[str]` would infer the result `Box[int]` for a value built from a
    // `Box[str]` → static type the checker cannot honor → runtime type confusion. The operator must
    // only fire when the operands' type args match (an exact-`Box[int]` pair).
    let src = "\
struct Box[T]:
    v: T
    fn add(self, other: Box[T]) -> Box[T]:
        return other
x := Box(5) + Box(\"hello\")
";
    entry_rejects(src, "cannot apply + to Box[int] and Box[str]");
}

#[test]
fn generic_struct_heterogeneous_compare_rejected() {
    // BOUNDARY (Bug 1, ordering variant): `Box[int] < Box[str]` must NOT overload `compare` — the
    // same heterogeneous-type-args laundering as `+`, via `ordering_allowed`.
    let src = "\
struct Box[T]:
    v: T
    fn compare(self, other: Box[T]) -> int:
        return 0
b := Box(5) < Box(\"hello\")
";
    entry_rejects(src, "cannot compare Box[int] and Box[str]");
}

#[test]
fn generic_enum_heterogeneous_add_rejected() {
    // BOUNDARY (Bug 1, enum analogue): a heterogeneous generic-enum pair must not overload `add`.
    let src = "\
enum Numeral[T]:
    Val(T)
    fn add(self, other: Numeral[T]) -> Numeral[T]:
        return other
x := Numeral.Val(1) + Numeral.Val(\"hello\")
";
    entry_rejects(src, "cannot apply + to Numeral[int] and Numeral[str]");
}

/// `List.min`/`max`/`min_by`/`max_by` return `Option[T]` (empty is `None`, not a fault) — two type
/// shapes nothing else in `std/prelude.chz` had, so both are pinned here rather than assumed:
/// (a) `min`/`max` combine a `where T: Comparable` bound with an `Option` return — the bound must
///     still reject a non-Comparable element (`sort` had the bound but returns `nil`);
/// (b) `min_by`/`max_by` carry their OWN `[K: Comparable]` type-param, so they route through
///     `infer_generic_method`, and the `Option[T]`'s `T` comes from the RECEIVER, not from `K` —
///     `subst` must map it (`Ty::Option(t) => Ty::option(subst(t, map))`).
#[test]
fn list_min_max_return_option_and_keep_their_bound() {
    // (a) the where-bound still fires through the new `Option` return
    rejects(
        "xs := [[true], [false]]\nxs.min()\n",
        "does not satisfy Comparable",
    );
    rejects(
        "xs := [[true], [false]]\nxs.max()\n",
        "does not satisfy Comparable",
    );
    // …and a Comparable element is still accepted
    ok("xs := [3, 1, 2]\nprint(xs.min() ?? 0)\nprint(xs.max() ?? 0)\n");
    // (b) the return really IS `Option[T]` with `T` from the receiver, at BOTH paths: a bare `T`
    // annotation must be refused, an `Option[T]` one accepted.
    for call in ["min()", "max()", "min_by(fn(x: int) -> int: x)"] {
        rejects(
            &format!("xs := [3, 1, 2]\nv: int = xs.{call}\n"),
            "cannot assign int? to variable of type int",
        );
        ok(&format!("xs := [3, 1, 2]\nv: int? = xs.{call}\n"));
    }
    // the `min_by` key type `K` is independent of the element type — `Option[str]` here, from `T`
    ok("xs := [\"bb\", \"a\"]\nv: str? = xs.max_by(fn(s: str) -> int: s.len())\n");
}

#[test]
fn redeclaring_add_rejected() {
    rejects(
        "protocol Add:\n    fn add(self, other: Self) -> Self\n",
        "reserved",
    );
}
