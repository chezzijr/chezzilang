//! Checker tests: receiver-less defer/spawn, TICKET-124 expected-type widening.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== M24-5b — a receiver-less `defer`/`spawn` head. A STATIC METHOD is an ordinary target (Go
// accepts `defer pkg.F(x)`, and Chezzi already accepted `defer print(H.build(3))`); a CONSTRUCTOR is
// not. Both halves of that split are decided HERE, in the checker: the refusal used to live in the
// compiler, so `chezzi check` printed "ok: no type errors" on a program `chezzi run` then refused —
// this project's named checker-superset-of-compiler class, which the editor/LSP shows green on.

/// A dotted CONSTRUCTOR head — a variant ctor in either turbofish carrier, and a module-qualified
/// struct/variant ctor — earns the message the bare `defer P(3)` rule already uses, because
/// it IS that rule: the call builds a value and throws it away.
#[test]
fn a_dotted_constructor_is_not_a_spawn_or_defer_target_rejected() {
    const NEEDLE: &str = "constructors must be wrapped in a function";
    let e = "enum E:\n    A(int)\n    B\n";
    let g = "enum G[T]:\n    A(int)\n    B\n";
    rejects(&format!("{e}defer E.A(3)\n"), NEEDLE);
    rejects(&format!("{e}parallel:\n    spawn E.A(3)\n"), NEEDLE);
    rejects(&format!("{g}defer G[int].A(3)\n"), NEEDLE);
    rejects(&format!("{g}parallel:\n    spawn G[int].A(3)\n"), NEEDLE);
    // …and the same three constructor kinds reached through a bound MODULE name.
    let lib = (
        "lib.chz",
        "enum Col:\n    Val(int)\n    Red\nenum GCol[T]:\n    Val(int)\n    Red\nstruct Pt:\n    x: int\n",
    );
    for target in ["lib.Col.Val(3)", "lib.GCol[int].Val(3)", "lib.Pt(3)"] {
        files_reject(
            &[lib, ("main.chz", &format!("import lib\ndefer {target}\n"))],
            NEEDLE,
        );
    }
}

/// M24-5 — the LAST spelling of that same constructor rule: a NATIVE ctor reached through its std
/// module object (`concurrency.Shared(0)`, `time.timer(10)`, whole-module or ALIASED). One concept,
/// one verdict — this used to be `check`-clean and then fault at run time with "module
/// 'std.concurrency' has no member 'Shared'", the bare `defer Shared(0)` spelling having been
/// refused correctly all along.
#[test]
fn a_qualified_native_constructor_is_not_a_spawn_or_defer_target_rejected() {
    const NEEDLE: &str = "constructors must be wrapped in a function";
    for (import, target) in [
        ("import std.concurrency", "concurrency.Shared(0)"),
        ("import std.concurrency", "concurrency.RwShared(0)"),
        ("import std.concurrency", "concurrency.Atomic(0)"),
        ("import std.concurrency", "concurrency.AtomicInt(0)"),
        ("import std.concurrency", "concurrency.Executor()"),
        ("import std.concurrency as cc", "cc.Shared(0)"),
        ("import Shared as S from std.concurrency", "S(0)"),
    ] {
        entry_rejects(
            &format!("{import}\nfn main():\n    defer {target}\nmain()\n"),
            NEEDLE,
        );
        entry_rejects(
            &format!("{import}\nfn main():\n    spawn {target}\nmain()\n"),
            NEEDLE,
        );
    }
}

/// …and the control the same arm must NOT touch: an ordinary native module FUNCTION is a plain call,
/// so it stays a legal `defer` target. (That it still RUNS — the other half of check/run agreement —
/// is `tests/chz/stdlib/math_test.chz::deferred_native_module_fn_runs`.)
#[test]
fn a_native_module_function_is_a_defer_target_ok() {
    entry_ok("import std.math\nfn main():\n    defer math.abs(-3)\n    print(\"done\")\nmain()\n");
    // `time.timer` is an ordinary std.time function too.
    entry_ok(
        "import std.time\nfn main():\n    defer time.timer(10)\n    print(\"done\")\nmain()\n",
    );
    // …and a LOCAL that merely SHADOWS a std module's bound name is a RECEIVER, not a module: this
    // `time.timer(10)` is an ordinary method call and must stay one. (The arm's `head_is_value` test
    // guard — a scope-blind name test would reject this with a factually false message.)
    entry_ok(
        "import std.time\nstruct Clock:\n    n: int\n    fn timer(self, ms: int) -> int:\n        return ms\nfn main(time: Clock):\n    defer time.timer(10)\nmain(Clock(1))\n",
    );
}

/// …and the neighbours the same rule must NOT touch — derived from its premise ("the call CONSTRUCTS
/// and discards"), not from the rejected list: a STATIC METHOD in every head spelling still builds a
/// value too, and is still an ordinary call. A `from`-imported head is here because its global slot
/// EXISTS (holding `Nil`), which is exactly what let it slip past the compiler's old head test and
/// fault late with "type nil has no method".
#[test]
fn a_dotted_static_method_is_a_spawn_or_defer_target_ok() {
    let h = "struct Holder:\n    v: int\n    fn build(old: int) -> int:\n        return old\n";
    let g = "struct Gen[K]:\n    k: K\n    fn build(old: int) -> int:\n        return old\n";
    ok(&format!("{h}defer Holder.build(3)\n"));
    ok(&format!("{h}parallel:\n    spawn Holder.build(3)\n"));
    ok(&format!("{g}defer Gen[int].build(3)\n"));
    ok(&format!("{g}parallel:\n    spawn Gen[int].build(3)\n"));
    // an enum STATIC (not a variant) keeps the accepting side of the same enum head
    ok(
        "enum E:\n    A(int)\n    B\n    fn build(old: int) -> int:\n        return old\ndefer E.build(3)\n",
    );
    let lib = (
        "lib.chz",
        "struct Holder:\n    v: int\n    fn build(old: int) -> int:\n        return old\nstruct Gen[K]:\n    k: K\n    fn build(old: int) -> int:\n        return old\n",
    );
    for main in [
        "import Holder from lib\ndefer Holder.build(3)\n",
        "import lib\ndefer lib.Holder.build(3)\n",
        "import lib\ndefer lib.Gen[int].build(3)\n",
    ] {
        files_ok(&[lib, ("main.chz", main)]);
    }
}

/// **TICKET-008 / docs/contributing/gaps.md W8-16 — a struct failing a protocol reports differently through a
/// value slot than through a generic bound.** Same program, same unmet requirement (`Dog` has no
/// `name` method against `Named`): the generic-bound call names the missing method
/// (`does not satisfy Named (missing method 'name')`), the value-slot argument call names only the
/// two type names. Expected: the value-slot message carries the same `(missing method '...')`
/// clause as the generic-bound one.
#[test]
fn protocol_mismatch_via_value_slot_names_missing_method() {
    let src = "protocol Named:\n    fn name(self) -> str\nstruct Dog:\n    x: int\nfn viaSlot(n: Named) -> str: return n.name()\nd := Dog(1)\nviaSlot(d)\n";
    rejects(src, "does not satisfy Named (missing method 'name')");
}

#[test]
fn value_slot_protocol_note_matches_the_generic_bound_wording() {
    let src = "protocol Named:\n    fn name(self) -> str\nstruct Dog:\n    x: int\nfn viaSlot(n: Named) -> str: return n.name()\nd := Dog(1)\nviaSlot(d)\n";
    let errs = check_src(src);
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert_eq!(
        errs[0].message,
        "argument 1 of 'viaSlot': expected Named, found Dog \u{2014} type Dog does not satisfy Named (missing method 'name')"
    );
}

#[test]
fn protocol_note_reaches_every_value_slot() {
    let prelude = "protocol Named:\n    fn name(self) -> str\nstruct Dog:\n    x: int\nstruct Holder:\n    n: Named\nd := Dog(1)\n";
    let tails = [
        "a: Named = d\n",
        "h := Holder(Dog(2))\nh.n = d\n",
        "fn f(x: Dog) -> Named: return x\n",
        "fn f2(x: Dog) -> Named: x\n",
        "import Iterator from std.iter\nfn g() -> Iterator[Named]:\n    yield d\n",
        "xs := List[Named].from_iter([d])\n",
        "ys: List[Named] = []\nys.push(d)\n",
        "fn k(n: Named = d) -> str: return n.name()\n",
        "struct H2:\n    n: Named = d\n",
        "fv := viaSlot\nfn viaSlot(n: Named) -> str: return n.name()\nfv(d)\n",
        "fv2 := viaSlot\nfn viaSlot(n: Named) -> str: return n.name()\nfv2(n=d)\n",
        "s := Set[Named].from_iter([d])\n",
        "mk := Map[Named, str].from_iter([(d, \"a\")])\n",
        "mv := Map[str, Named].from_iter([(\"a\", d)])\n",
    ];
    for tail in tails {
        let src = format!("{prelude}{tail}");
        rejects(
            &src,
            "type Dog does not satisfy Named (missing method 'name')",
        );
    }
    let src = format!("import Shared from std.concurrency\n{prelude}sh := Shared[Named](d)\n");
    entry_rejects(
        &src,
        "type Dog does not satisfy Named (missing method 'name')",
    );
}

#[test]
fn value_slot_protocol_note_names_a_wrong_signature() {
    let src = "protocol Named:\n    fn name(self) -> str\nstruct Dog:\n    x: int\n    fn name(self) -> int: return 0\nfn viaSlot(n: Named) -> str: return n.name()\nd := Dog(1)\nviaSlot(d)\n";
    rejects(
        src,
        "type Dog does not satisfy Named (method 'name' has the wrong signature)",
    );
}

#[test]
fn value_slot_protocol_note_absent_when_the_type_satisfies() {
    let prelude = "protocol Named:\n    fn name(self) -> str\nstruct Dog:\n    x: int\n    fn name(self) -> str: return \"dog\"\nstruct Holder:\n    n: Named\nd := Dog(1)\n";
    let tails = [
        "a: Named = d\n",
        "h := Holder(Dog(2))\nh.n = d\n",
        "fn f(x: Dog) -> Named: return x\n",
        "ys: List[Named] = []\nys.push(d)\n",
    ];
    for tail in tails {
        ok(&format!("{prelude}{tail}"));
    }
}

#[test]
fn value_slot_protocol_note_absent_for_a_non_protocol_mismatch() {
    let errs = check_src("x: int = \"s\"\n");
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert_eq!(errs[0].message, "cannot assign str to variable of type int");

    let src = "fn f(x: float): pass\ni := 1\nf(i)\n";
    let errs = check_src(src);
    assert!(
        errs.iter()
            .any(|e| e.message.ends_with(" — write 1.0 (or float(x))")),
        "expected a widen-note error, got: {errs:?}"
    );
}

// TICKET-021: an unknown variable near a real one gets no "did you mean" help, unlike a
// method/field typo (see the `suggest.rs` near-miss suite above).
#[test]
fn unknown_variable_gets_no_did_you_mean_ticket_021() {
    let src = "count := 3\nprint(coutn)\n";
    rejects_help(src, "unknown name 'coutn'", "did you mean 'count'");
}

// TICKET-021: an unknown module member near a real one gets no "did you mean" help.
#[test]
fn unknown_module_member_gets_no_did_you_mean_ticket_021() {
    let src =
        "import std.math\nfn main():\n    print(math.sqrt(4.0))\n    print(math.sqrtt(4.0))\n";
    let errs = check_entry(src);
    assert!(
        errs.iter().any(
            |e| e.message.contains("module 'math' has no member 'sqrtt'")
                && e.help
                    .as_deref()
                    .is_some_and(|h| h.contains("did you mean 'sqrt'"))
        ),
        "expected an error containing \"module 'math' has no member 'sqrtt'\" with help \
         containing \"did you mean 'sqrt'\", got: {errs:?}"
    );
}

// TICKET-021: an unknown enum variant near a real one gets no "did you mean" help.
#[test]
fn unknown_enum_variant_gets_no_did_you_mean_ticket_021() {
    let src = "enum Color:\n    Red\n    Green\n    Blue\nc := Color.Bluee\n";
    rejects_help(
        src,
        "enum 'Color' has no variant 'Bluee'",
        "did you mean 'Blue'",
    );
}

// TICKET-021: a bare module bind used in value position (not called, not dotted) is silently
// accepted at check time instead of being rejected.
#[test]
fn module_in_value_position_is_accepted_ticket_021() {
    let src = "import std.math\nx := math\nprint(x)\n";
    let errs = check_entry(src);
    assert!(
        !errs.is_empty(),
        "expected a module-in-value-position error, but the checker accepted it clean"
    );
}

// TICKET-021 item (d): `int(io)` on a module is accepted at check time and faults at runtime
// instead ("int() cannot convert module"). Same underlying hole as the test above, pinned with
// the ticket's own repro shape.
#[test]
fn int_cast_on_module_is_accepted_at_check_time_ticket_021() {
    let src = "import std.io\nprint(int(io))\n";
    let errs = check_entry(src);
    assert!(
        !errs.is_empty(),
        "expected int(io) to be rejected at check time, but the checker accepted it clean"
    );
}

// TICKET-021: legal module-qualifier reads (never a value on their own) stay clean under the
// module-in-value-position reject — the rule must fire only when the module itself is the value.
#[test]
fn module_qualifier_positions_stay_clean_ticket_021() {
    entry_ok(
        "import std.math\nfn main():\n    x := math.pi\n    print(x)\n    print(math.abs(-3))\n    parallel:\n        spawn math.abs(-3)\nmain()\n",
    );
}

// TICKET-021 (Gotcha 3): a cast arm used to infer its argument up to three times, so a diagnostic
// raised inside `infer_value` printed once per inference. Pin it printing exactly once.
#[test]
fn cast_argument_diagnostic_prints_once_ticket_021() {
    let errs = check_src("print(int(nope))\n");
    let matching: Vec<_> = errs
        .iter()
        .filter(|e| e.message.contains("unknown name 'nope'"))
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one 'unknown name' error, got: {errs:?}"
    );
}

// TICKET-021: the module-member-miss caret now spans the MEMBER name, not the module name.
#[test]
fn module_member_miss_carets_the_member_name_ticket_021() {
    let src = "import std.json\nprint(json.encodee(\"x\"))\n";
    let errs = check_entry(src);
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert_eq!(
        errs[0].span.col, 12,
        "expected the caret at col 12 (under 'encodee'), got: {errs:?}"
    );
}

// TICKET-021: the enum-variant-miss caret now spans the VARIANT name, not the enum name.
#[test]
fn enum_variant_miss_carets_the_variant_name_ticket_021() {
    let src = "enum C:\n    Red\n    Green\nc := C.Redd\n";
    let errs = check_src(src);
    assert_eq!(errs.len(), 1, "expected exactly one error, got: {errs:?}");
    assert_eq!(
        errs[0].span.col, 8,
        "expected the caret at col 8 (under 'Redd'), got: {errs:?}"
    );
}

// TICKET-024 -- W8-32: `satisfies_args_d`'s final match falls to `_ => Err(...)` for any `Ty`
// that is not Struct/Enum/Param/Protocol, so a built-in `List[int]` can never satisfy a
// USER protocol even though the checker's own method table (`self.list_methods` / equivalent)
// already answers `len()` for it on a direct call. Reproduces docs/contributing/gaps.md W8-32 verbatim.
#[test]
fn builtin_list_satisfies_user_protocol_with_matching_method_ticket_024() {
    let src = "protocol Sized:\n    fn len(self) -> int\n\nfn total(x: Sized) -> int:\n    return x.len()\n\nfn main():\n    print(total([1,2,3]))\n";
    ok(src);
}

// TICKET-024 negative neighbour: a built-in that genuinely lacks the method must still be
// rejected, naming the missing method -- this must keep passing once the widening lands.
#[test]
fn builtin_int_missing_protocol_method_still_rejected_ticket_024() {
    let src = "protocol Sized:\n    fn len(self) -> int\n\nfn total(x: Sized) -> int:\n    return x.len()\n\nfn main():\n    print(total(1))\n";
    rejects(
        src,
        "type int does not satisfy Sized (missing method 'len')",
    );
}

// TICKET-024: Map/Set/str/bytes/bytearray each witness the same user protocol out of their own
// harvested `native struct` method table -- not just List.
#[test]
fn builtin_map_set_str_bytes_satisfy_user_protocol_ticket_024() {
    ok(
        "protocol Sized:\n    fn len(self) -> int\n\nfn total(x: Sized) -> int:\n    return x.len()\n\nfn main():\n    print(total({\"a\": 1}))\n",
    );
    ok(
        "protocol Sized:\n    fn len(self) -> int\n\nfn total(x: Sized) -> int:\n    return x.len()\n\nfn main():\n    print(total({1, 2}))\n",
    );
    ok(
        "protocol Sized:\n    fn len(self) -> int\n\nfn total(x: Sized) -> int:\n    return x.len()\n\nfn main():\n    print(total(\"abc\"))\n",
    );
    ok(
        "protocol Sized:\n    fn len(self) -> int\n\nfn total(x: Sized) -> int:\n    return x.len()\n\nfn main():\n    print(total(b\"ab\"))\n",
    );
    ok(
        "protocol Sized:\n    fn len(self) -> int\n\nfn total(x: Sized) -> int:\n    return x.len()\n\nfn main():\n    print(total(bytearray.from_iter(b\"ab\")))\n",
    );
}

// TICKET-024: the same widening works through a generic bound, both the `[T: Sized]` and
// `where T: Sized` spellings.
#[test]
fn builtin_satisfies_user_protocol_through_generic_bound_ticket_024() {
    ok(
        "protocol Sized:\n    fn len(self) -> int\n\nfn total[T: Sized](x: T) -> int:\n    return x.len()\n\nfn main():\n    print(total([1,2,3]))\n",
    );
    ok(
        "protocol Sized:\n    fn len(self) -> int\n\nfn total[T](x: T) -> int where T: Sized:\n    return x.len()\n\nfn main():\n    print(total([1,2,3]))\n",
    );
}

// TICKET-024: a native method whose SIGNATURE doesn't match the requirement is refused, not
// silently widened -- and the handle ceiling (no method table at all) still falls to the bare
// catch-all message.
#[test]
fn builtin_user_protocol_shape_mismatch_rejected_ticket_024() {
    rejects(
        "protocol Sized:\n    fn len(self) -> str\n\nfn total(x: Sized) -> str:\n    return x.len()\n\nfn main():\n    print(total([1,2,3]))\n",
        "type List[int] does not satisfy Sized (method 'len' has the wrong signature)",
    );
    rejects(
        "protocol Pushy:\n    fn push(self, v: int) -> None\n\nfn total(x: Pushy):\n    x.push(1)\n\nfn main():\n    xs: List[str] = []\n    total(xs)\n",
        "type List[str] does not satisfy Pushy (method 'push' has the wrong signature)",
    );
    rejects(
        "protocol Mapper:\n    fn map(self, f: fn(int) -> int) -> List[int]\n\nfn total(x: Mapper):\n    x.map(fn(v: int) -> int: v)\n\nfn main():\n    total([1,2,3])\n",
        "type List[int] does not satisfy Mapper (native method 'map' is generic and cannot witness a protocol requirement)",
    );
    rejects(
        "import std.concurrency\n\nprotocol Sized:\n    fn len(self) -> int\n\nfn total(x: Sized) -> int:\n    return x.len()\n\nfn main():\n    c := Channel[int](1)\n    print(total(c))\n",
        "type Channel[int] does not satisfy Sized",
    );
}

// TICKET-024: a native method's dispatch-time residual (a `where` bound on the receiver's own
// element type, or a numeric-only gate) is enforced through the protocol path exactly as it is
// on a direct call.
#[test]
fn builtin_native_method_gates_bound_protocol_satisfaction_ticket_024() {
    ok(
        "protocol Sortable:\n    fn sort(self) -> None\n\nfn srt(x: Sortable):\n    x.sort()\n\nfn main():\n    xs := [3,1,2]\n    srt(xs)\n",
    );
    rejects(
        "struct Foo:\n    v: int\n\nprotocol Sortable:\n    fn sort(self) -> None\n\nfn srt(x: Sortable):\n    x.sort()\n\nfn main():\n    xs: List[Foo] = [Foo(v=1)]\n    srt(xs)\n",
        "type List[Foo] does not satisfy Sortable (method 'sort' requires Foo: Comparable)",
    );
    ok(
        "protocol Summable:\n    fn sum(self) -> int\n\nfn total(x: Summable) -> int:\n    return x.sum()\n\nfn main():\n    print(total([1,2,3]))\n",
    );
    rejects(
        "struct Cents:\n    v: int\n\nprotocol SumC:\n    fn sum(self) -> Cents\n\nfn total(x: SumC) -> Cents:\n    return x.sum()\n\nfn main():\n    xs: List[Cents] = [Cents(1), Cents(2)]\n    print(total(xs))\n",
        "type List[Cents] does not satisfy SumC (native method 'sum' needs a numeric element type)",
    );
}

// TICKET-030: a struct has no `copy()` — the only way to duplicate one is to retype every field.
#[test]
fn struct_copy_method_missing_ticket_030() {
    // DEC-026: the NAME states the bug, the BODY asserts the FIX. This is RED until the intrinsic
    // `copy()` lands in `Checker::infer_method_call`'s `Ty::Struct` arm.
    ok("struct P:\n    x: int\n\nfn main():\n    p := P(1)\n    q: P = p.copy()\n    print(q)\n");
}

#[test]
fn struct_copy_rejects_an_argument() {
    rejects(
        "struct P:\n    x: int\n\nfn main():\n    p := P(1)\n    print(p.copy(1))\n",
        "'copy' expects 0 argument(s), got 1",
    );
}

#[test]
fn struct_copy_returns_the_receiver_type() {
    rejects(
        "struct P:\n    x: int\n\nfn main():\n    p := P(1)\n    n: int = p.copy()\n    print(n)\n",
        "cannot assign P to variable of type int",
    );
}

#[test]
fn struct_copy_is_not_on_an_enum() {
    rejects(
        "enum E:\n    A\n    B\n\nfn main():\n    e := E.A\n    print(e.copy())\n",
        "type E has no method 'copy'",
    );
}

#[test]
fn struct_copy_does_not_witness_a_user_protocol() {
    rejects(
        "protocol Dup:\n    fn copy(self) -> int\n\nstruct P:\n    x: int\n\nfn take(d: Dup) -> int:\n    return d.copy()\n\nfn main():\n    print(take(P(1)))\n",
        "type P does not satisfy Dup (missing method 'copy')",
    );
}

#[test]
fn struct_copy_is_suggested_on_a_near_miss() {
    rejects_help(
        "struct P:\n    x: int\n\nfn main():\n    p := P(1)\n    print(p.cpy())\n",
        "type P has no method 'cpy'",
        "did you mean 'copy'?",
    );
}

#[test]
fn struct_copy_field_wins_over_the_intrinsic_and_stays_uncallable() {
    // A non-fn field named `copy` (review finding on `3faa6948`): the fn-typed-field fallback
    // matches only `Ty::Func`, so `copy: int` must NOT fall through to the intrinsic arm — that
    // was accepted at check time and then faulted at runtime with "'{}' is not callable".
    rejects(
        "struct S:\n    copy: int\n\nfn main():\n    s := S(7)\n    print(s.copy())\n",
        "has no method 'copy' ('copy' is a field -- read it without parentheses)",
    );
}

/// TICKET-079/TICKET-080 (c): a method-miss name that is a FIELD reports its kind instead of
/// suggesting itself — `did you mean 'copy'?` would answer with the identical spelling the user
/// already typed.
#[test]
fn a_field_named_like_a_method_is_never_suggested() {
    let errs = check_src("struct S:\n    copy: int\nfn main():\n    s := S(7)\n    s.copy()\n");
    let e = errs
        .iter()
        .find(|e| e.message.contains("has no method 'copy'"))
        .unwrap_or_else(|| panic!("expected a 'copy' miss, got: {errs:?}"));
    assert_eq!(e.help, None, "expected no suggestion, got: {e:?}");
}

#[test]
fn binary_op_hint_reaches_only_the_left_operand_ticket_034() {
    // TICKET-034: `expected_hint` is a single `take()`d slot (src/checker/expr.rs:33), so a
    // binary expr in a hinted position hands it to the LEFT operand only. The right operand's
    // generic call then can't be pinned, even though the annotation determines both sides.
    // Fix (per-operand re-install, matching infer_if_else_chain / infer_list) must make this
    // program type-check clean.
    ok(
        "struct G[T]:\n    v: T\n\nfn mkl[T]() -> List[G[T]]:\n    xs: List[G[T]] = []\n    return xs\n\nfn use[Z]():\n    xs: List[G[Z]] = mkl() + mkl()\n    print(xs.len())\n\nuse[int]()\n",
    );
}

#[test]
fn binary_op_hint_reaches_both_operands_in_every_hinted_position() {
    // TICKET-034: the fix re-installs `expected_hint` per operand, so a generic call on the
    // RIGHT side of a binary op is pinned in every hinted position, not just the left.
    const PRELUDE: &str = "struct G[T]:\n    v: T\nfn mkl[T]() -> List[G[T]]:\n    ys: List[G[T]] = []\n    return ys\nfn takeI(xs: List[G[int]]) -> int:\n    return xs.len()\nstruct SI:\n    xs: List[G[int]]\n";
    // annotated `let`
    ok(&format!(
        "{PRELUDE}fn use[Z]():\n    a: List[G[Z]] = mkl() + mkl()\n    print(a.len())\nuse[int]()\n"
    ));
    // `return`
    ok(&format!(
        "{PRELUDE}fn ret[Z]() -> List[G[Z]]:\n    return mkl() + mkl()\nprint(ret[int]().len())\n"
    ));
    // non-generic call argument
    ok(&format!(
        "{PRELUDE}fn use[Z]():\n    print(takeI(mkl() + mkl()))\nuse[int]()\n"
    ));
    // struct-ctor argument
    ok(&format!(
        "{PRELUDE}fn use[Z]():\n    print(SI(mkl() + mkl()).xs.len())\nuse[int]()\n"
    ));
    // List element
    ok(&format!(
        "{PRELUDE}fn use[Z]():\n    d: List[List[G[Z]]] = [mkl() + mkl()]\n    print(d.len())\nuse[int]()\n"
    ));
    // if else-arm
    ok(&format!(
        "{PRELUDE}fn use[Z](c: bool):\n    e: List[G[Z]] = if c: mkl() else: mkl() + mkl()\n    print(e.len())\nuse[int](true)\n"
    ));
    // match arm
    ok(&format!(
        "{PRELUDE}fn use[Z](k: int):\n    f: List[G[Z]] = match k:\n        0: mkl() + mkl()\n        _: mkl()\n    print(f.len())\nuse[int](0)\n"
    ));
    // chain
    ok(&format!(
        "{PRELUDE}fn use[Z]():\n    a: List[G[Z]] = mkl() + mkl() + mkl()\n    print(a.len())\nuse[int]()\n"
    ));

    // operator column: arithmetic overloads on Box[T]
    const BOX_PRELUDE: &str = "struct Box[T]:\n    v: T\n    fn add(self, other: Box[T]) -> Box[T]:\n        return self\n    fn sub(self, other: Box[T]) -> Box[T]:\n        return self\n    fn mul(self, other: Box[T]) -> Box[T]:\n        return self\n    fn div(self, other: Box[T]) -> Box[T]:\n        return self\n    fn mod(self, other: Box[T]) -> Box[T]:\n        return self\nfn mkb[T]() -> Box[T]:\n    return mkb[T]()\n";
    for op in ["+", "-", "*", "/", "%"] {
        ok(&format!(
            "{BOX_PRELUDE}fn use[Z]():\n    a: Box[Z] = mkb() {op} mkb()\n    print(1)\nuse[int]()\n"
        ));
    }

    // operator column: set operators on Set[H[T]]
    const SET_PRELUDE: &str = "import Eq from std.cmp\nstruct H[T: Eq]:\n    v: T\n    fn hash(self) -> int:\n        return 1\n    fn eq(self, other: Self) -> bool:\n        return self.v == other.v\nfn mks[T: Eq]() -> Set[H[T]]:\n    return mks[T]()\n";
    for op in ["|", "&", "^", "-"] {
        ok(&format!(
            "{SET_PRELUDE}fn use[Z: Eq]():\n    a: Set[H[Z]] = mks() {op} mks()\n    print(1)\nuse[int]()\n"
        ));
    }
}

#[test]
fn d3_binary_op_hint_neighbours() {
    // Four cells whose verdict AND message are unchanged by TICKET-034's fix — passes both
    // before and after step 5's re-install. `pick() == false` is the load-bearing one: it pins
    // that the re-install stays UNIFORM over every operator (never filtered to arithmetic-only),
    // since this cell's left operand already relies on the hint reaching a comparison.
    rejects(
        "xs := [1, 2] + [\"a\"]\nprint(xs)\n",
        "cannot apply + to List[int] and List[str]",
    );
    rejects(
        "q := [1] + [2, 2.5]\nprint(q)\n",
        "list elements differ: int vs float",
    );
    rejects(
        "fn takeF(xs: List[float]) -> int:\n    return xs.len()\nprint(takeF([2]))\n",
        "write 1.0",
    );
    ok(
        "fn pick[T]() -> T:\n    return pick[T]()\nfn use():\n    b: bool = pick() == false\n    print(b)\n",
    );
}

#[test]
fn binary_op_hint_does_not_widen_an_int_literal_into_a_float_slot() {
    // PRE-fix message: `cannot apply + to List[float] and List[int]`. This cell must stay
    // rejected after the fix, with its message moving to the per-element form — the hint reaching
    // the right operand must never license the int->float widen. If it does, the program is
    // accepted and raises no error at all.
    rejects(
        "b: List[float] = [1.0] + [2]\nprint(b)\n",
        "list element: expected float, found int",
    );
}

#[test]
fn binary_op_hint_widens_an_any_slot_concat() {
    // Three programs rejected before the fix (each `cannot apply + to List[Any] and ...`) become
    // `ok` once the right-hand literal sees the List[Any] slot too.
    ok("fn main():\n    c: List[Any] = [1] + [\"a\"]\n    print(c)\nmain()\n");
    ok("fn main():\n    p: List[Any] = [1] + [2, 2.5]\n    print(p)\nmain()\n");
    ok("fn main():\n    r: List[Any] = [1, \"a\"] + [2]\n    print(r)\nmain()\n");
}

// TICKET-054 (1): an `if`-EXPRESSION bound to an annotated protocol-typed slot must push the
// annotation into the branch unifier, same as the statement-form `if` and the list-literal sibling
// two lines above it already do. Currently the unifier joins the branches against EACH OTHER only
// (`pattern.rs:1166`) and ignores the known expected type, so this is falsely rejected.
#[test]
fn ticket_054_if_expr_uses_annotation_on_own_binding() {
    entry_ok(
        "protocol Sh:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int: self.s * self.s\nstruct Tr:\n    b: int\n    fn area(self) -> int: self.b\nx: Sh = if true: Sq(2) else: Tr(9)\nprint(x.area())\n",
    );
}

// TICKET-054 (2): a closure's OWN `-> float` return type must widen an untyped int constant body,
// same as a named fn's and a method's `-> float` return already do (`pattern.rs:4663`).
#[test]
fn d3_ticket_054_closure_own_return_type_rejects_int() {
    entry_rejects("g := fn() -> float: 3\nprint(g())\n", "write 1.0");
    entry_ok("g := fn() -> float: 3.0\nprint(g())\n");
}

// TICKET-054 (3): a `float` parameter reached through a protocol-typed receiver must widen an
// untyped int argument, same as the identical method called on the concrete receiver already does.
#[test]
fn d3_ticket_054_protocol_receiver_rejects_float_param() {
    entry_rejects(
        "protocol P:\n    fn m(self, x: float) -> float\nstruct S:\n    fn m(self, x: float) -> float:\n        return x\np: P = S()\nprint(p.m(1))\n",
        "write 1.0",
    );
    entry_ok(
        "protocol P:\n    fn m(self, x: float) -> float\nstruct S:\n    fn m(self, x: float) -> float:\n        return x\np: P = S()\nprint(p.m(1.0))\n",
    );
}

// TICKET-054 step 1: neighbours the sink-1 fix must NOT move. The branch-unifier hint must be
// consulted only through plain `assignable`, never `assignable_w`, and only on a mismatch (never
// folded into the accumulator), and an un-annotated branch must still require agreement.
#[test]
fn ticket_054_if_expr_hint_never_widens_or_launders() {
    entry_rejects(
        "x: float = if true: 1 else: 2\n",
        "cannot assign int to variable of type float",
    );
    entry_rejects(
        "n := 3\nx: float = if true: n else: 2.5\n",
        "branches have incompatible types: int and float",
    );
    entry_rejects(
        "x: int = if true: \"a\" else: \"b\"\n",
        "cannot assign str to variable of type int",
    );
    entry_rejects(
        "protocol Sh:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int: self.s * self.s\nx: Sh = if true: Sq(2) else: 5\n",
        "branches have incompatible types: Sq and int",
    );
    entry_rejects(
        "protocol Sh:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int: self.s * self.s\nstruct Tr:\n    b: int\n    fn area(self) -> int: self.b\nx := if true: Sq(2) else: Tr(9)\n",
        "branches have incompatible types: Sq and Tr",
    );
    entry_rejects(
        "protocol Sh:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int: self.s * self.s\nstruct Tr:\n    b: int\n    fn area(self) -> int: self.b\nx := match 1:\n    1: Sq(2)\n    _: Tr(9)\n",
        "branches have incompatible types: Sq and Tr",
    );
}

// TICKET-054 step 1: sink-2 neighbours the fix must NOT move. A genuine mismatch, an alias-free
// plain `int`, a value/int-param body, `-> Option[float]`, and the sibling `Option[int]` widen.
#[test]
fn ticket_054_closure_float_return_neighbours() {
    entry_rejects(
        "g := fn() -> float: \"s\"\nprint(g())\n",
        "closure body has type str",
    );
    entry_rejects(
        "n := 3\ng := fn() -> float: n\nprint(g())\n",
        "closure body has type int",
    );
    entry_rejects(
        "g := fn(a: int) -> float: a\nprint(g(3))\n",
        "closure body has type int",
    );
    entry_rejects(
        "g := fn() -> float?: 3\nprint(g())\n",
        "closure body has type int, but its return type is float?",
    );
    entry_ok("g := fn() -> int?: 3\nprint(g())\n");
}

// TICKET-054 step 1: sink-3 neighbours the fix must NOT move. A typed int, a str, and a
// generic-erased `G[T]` parameter must all stay rejected.
#[test]
fn ticket_054_protocol_float_param_neighbours() {
    entry_rejects(
        "protocol P:\n    fn m(self, x: float) -> float\nstruct S:\n    fn m(self, x: float) -> float:\n        return x\np: P = S()\nn := 3\nprint(p.m(n))\n",
        "expected float, found int",
    );
    entry_rejects(
        "protocol P:\n    fn m(self, x: float) -> float\nstruct S:\n    fn m(self, x: float) -> float:\n        return x\np: P = S()\nprint(p.m(\"a\"))\n",
        "expected float, found str",
    );
    entry_rejects(
        "protocol G[T]:\n    fn m(self, x: T) -> T\nstruct SG:\n    fn m(self, x: float) -> float:\n        return x\ng: G[float] = SG()\nprint(g.m(1))\n",
        "expected float, found int",
    );
}

// TICKET-054 step 2: the sink-1 fix must accept every expected-type spelling, not only the `if`
// binding form the triage test covers -- the `match`-expression form, a `return match`, a call
// argument, and `Any` (DEC-034's existing `List[Any]` precedent extended to a scalar branch mix).
#[test]
fn ticket_054_if_expr_hint_accepts_every_expected_type_spelling() {
    entry_ok(
        "protocol Sh:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int: self.s * self.s\nstruct Tr:\n    b: int\n    fn area(self) -> int: self.b\nz: Sh = match 1:\n    1: Sq(3)\n    _: Tr(4)\nprint(z.area())\n",
    );
    entry_ok(
        "protocol Sh:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int: self.s * self.s\nstruct Tr:\n    b: int\n    fn area(self) -> int: self.b\nfn pick(n: int) -> Sh:\n    return match n:\n        0: Sq(3)\n        _: Tr(4)\nprint(pick(0).area())\n",
    );
    entry_ok(
        "protocol Sh:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int: self.s * self.s\nstruct Tr:\n    b: int\n    fn area(self) -> int: self.b\nfn f(s: Sh):\n    print(s.area())\nf(if true: Sq(2) else: Tr(9))\n",
    );
    // DEC-034 gotcha 7: Any is the empty top protocol; x: Any = 1 and y: Any = \"a\" both run today.
    entry_ok("x: Any = if true: 1 else: \"a\"\nprint(x)\n");
    entry_ok(
        "protocol Sh:\n    fn area(self) -> int\nstruct Sq:\n    s: int\n    fn area(self) -> int: self.s * self.s\nstruct Tr:\n    b: int\n    fn area(self) -> int: self.b\nxs: List[Sh] = [Sq(2), Tr(9)]\nprint(xs[1].area())\n",
    );
}

// TICKET-054 step 3: sink-2's checker AND compiler halves land together. A closure's own `-> float`
// return widens an untyped int constant reached through a `type` alias, and a HOF callback closure
// widens the same way.
#[test]
fn d3_ticket_054_closure_float_return_alias_and_hof() {
    entry_rejects(
        "type F = float\ng := fn() -> F: 3\nprint(g())\n",
        "write 1.0",
    );
    entry_rejects(
        "xs := [1, 2]\nprint(xs.map(fn(x) -> float: 3))\n",
        "write 1.0",
    );
}

// TICKET-054 step 4: sink-3's widen must reach a bound generic type parameter (not only a
// protocol-typed receiver), and must carry DEC-033's element license (a `List[float]` requirement).
#[test]
fn d3_ticket_054_protocol_and_bound_receiver_reject_more_spellings() {
    entry_rejects(
        "protocol P:\n    fn m(self, x: float) -> float\nstruct S:\n    fn m(self, x: float) -> float:\n        return x\nfn viabound[T: P](t: T) -> float:\n    return t.m(1)\nprint(viabound(S()))\n",
        "write 1.0",
    );
    entry_rejects(
        "protocol Q:\n    fn n(self, xs: List[float]) -> float\nstruct S:\n    fn n(self, xs: List[float]) -> float:\n        return xs[0]\nq: Q = S()\nprint(q.n([1, 2]))\n",
        "write 1.0",
    );
}

// TICKET-054 review fix — a GENERIC struct witness (`struct GS[T]` with `fn m2(self, x: T) -> T`)
// satisfying a protocol requirement declared `x: float` DECLARES its own param `T`, so its own
// prologue coerces nothing. D3: the checker rejects the int argument (`entry_rejects` here); the
// `.chz` sibling test in `tests/chz/spec/expected_type_sinks_test.chz` pins the `1.0` spelling.
#[test]
fn d3_ticket_054_protocol_receiver_rejects_through_generic_witness() {
    entry_rejects(
        "protocol P2:\n    fn m2(self, x: float) -> float\nstruct GS[T]:\n    v: T\n    fn m2(self, x: T) -> T:\n        return x\np2: P2 = GS(1.0)\nprint(p2.m2(1))\n",
        "write 1.0",
    );
    // The DIRECT (non-protocol) call to the same generic witness rejects too.
    entry_rejects(
        "protocol G:\n    fn m2(self, x: float) -> float\nstruct GS[T]:\n    v: T\n    fn m2(self, x: T) -> T:\n        return x\ns := GS(1.0)\nprint(s.m2(1))\n",
        "argument 1 of 'm2': expected float, found int",
    );
}

// TICKET-066 W10-3: a module-qualified turbofish on a NULLARY enum variant (no call) is falsely
// rejected. `Box[int].Full(1)` (a call) and the un-qualified `Box[int].Empty` (via `import Box from
// lib`) both work; only the qualified no-call bare-member shape falls into the index path and
// reports "module 'lib' has no member 'Box'".
#[test]
fn ticket_066_qualified_turbofish_nullary_variant() {
    files_ok(&[
        ("lib.chz", "enum Box[T]:\n    Full(T)\n    Empty\n"),
        (
            "main.chz",
            "import lib\nx := lib.Box[int].Empty\nprint(x)\n",
        ),
    ]);
}

#[test]
fn ticket_066_qualified_turbofish_unknown_variant_names_the_enum() {
    files_reject(
        &[
            ("lib.chz", "enum Box[T]:\n    Full(T)\n    Empty\n"),
            ("main.chz", "import lib\nx := lib.Box[int].Nope\nprint(x)\n"),
        ],
        "enum 'Box' has no variant 'Nope'",
    );
}

// TICKET-066 W10-17: a named argument for a parameter that PRECEDES a variadic. See the desugar
// test `named_arg_before_variadic_is_not_missing` (src/desugar/mod.rs) — the false rejection is
// raised by desugar, before the checker ever sees the call.

// TICKET-066 W10-20: docs/reference/syntax/ and docs/reference/spec/ claim `fn(): nil` is a no-op closure.
// There is NO working spelling of an empty closure today: the documented one fails with "unknown
// name 'nil'" (there is no value-level `nil`), and the natural alternative `fn(): pass` fails too
// ("unexpected 'pass' in expression" -- a single-line closure body must be an expression). This
// pins the fix's target: at least ONE spelling must compile and call as a no-op.
#[test]
fn ticket_066_noop_closure_has_no_working_spelling() {
    entry_ok("f := fn(): pass\nf()\n");
}

// TICKET-066 W10-20 boundary: `pass` is legal ONLY as a closure body, right after `:`. It stays
// statement-only everywhere else in expression position, including a closure whose return type
// does not accept `nil`.
#[test]
fn ticket_066_pass_stays_statement_only_outside_a_closure_body() {
    let toks = lexer::tokenize("x := pass\n").expect("lex should succeed");
    let err = parser::parse(toks).expect_err("expected a parse error");
    assert!(
        err.message.contains("unexpected 'pass' in expression"),
        "got: {}",
        err.message
    );

    rejects(
        "f := fn() -> int: pass\n",
        "closure body has type None, but its return type is int",
    );
}

// TICKET-075: an unrelated struct's UNIQUE same-named method (here `Deco.f`, which does not satisfy
// `P`) has its trailing default splice into a protocol-typed call before the arity check runs, so
// `x.f(2)` (one argument, matching P.f's exact arity) is checked as a two-argument call and
// rejected with a false count. `Deco` never satisfies `P` and never participates in this call.
#[test]
fn ticket_075_unrelated_struct_default_does_not_splice_into_protocol_call() {
    ok_desugared(
        "protocol P:\n    fn f(self, a: int) -> int\nstruct Deco:\n    z: int\n    fn f(self, a: int, b: int = 10) -> int:\n        return a + b\nfn use(x: P) -> int:\n    return x.f(2)\n",
    );
}

// TICKET-075: a `[T: P]` bound receiver takes the same name-keyed fallback as a plain protocol-typed
// receiver, so it must be filtered the same way.
#[test]
fn ticket_075_a_bound_receiver_does_not_splice_an_unrelated_structs_default() {
    ok_desugared(
        "protocol P:\n    fn f(self, a: int) -> int\nstruct Deco:\n    z: int\n    fn f(self, a: int, b: int = 10) -> int:\n        return a + b\nfn use1[T: P](x: T) -> int:\n    return x.f(2)\n",
    );
}

// TICKET-075: a `let`-annotated protocol-typed local (`x: P = w`) has no concrete struct receiver,
// exactly like a bare param, so it must be filtered too.
#[test]
fn ticket_075_a_let_annotated_protocol_local_does_not_splice_an_unrelated_structs_default() {
    ok_desugared(
        "protocol P:\n    fn f(self, a: int) -> int\nstruct Deco:\n    z: int\n    fn f(self, a: int, b: int = 10) -> int:\n        return a + b\nfn use1(w: P) -> int:\n    x: P = w\n    return x.f(2)\n",
    );
}

// TICKET-187 (supersedes DEC-075's lending): a witness's default is not reachable through a
// protocol receiver; the protocol's parameter list is the whole contract, as in Go and Rust.
#[test]
fn ticket_075_a_matching_witnesss_default_does_not_fill_a_protocol_call() {
    rejects_desugared(
        "protocol P:\n    fn f(self, a: int) -> int\nstruct W:\n    v: int\n    fn f(self, a: int = 7) -> int:\n        return self.v + a\nfn use1(x: P) -> int:\n    return x.f()\n",
        "expects",
    );
}

// TICKET-075 ceiling: a named argument still binds through a protocol receiver.
#[test]
fn ticket_075_a_named_argument_still_binds_through_a_protocol_receiver() {
    ok_desugared(
        "protocol P:\n    fn f(self, a: int) -> int\nstruct W:\n    v: int\n    fn f(self, a: int) -> int:\n        return self.v + a\nfn use1(x: P) -> int:\n    return x.f(a=2)\n",
    );
}

// TICKET-075 ceiling: a genuine over-arity call through a protocol receiver is still rejected --
// the filter must not swallow real arity errors.
#[test]
fn ticket_075_a_genuine_over_arity_protocol_call_is_still_rejected() {
    rejects_desugared(
        "protocol P:\n    fn f(self, a: int) -> int\nstruct W:\n    v: int\n    fn f(self, a: int) -> int:\n        return self.v + a\nfn use1(x: P) -> int:\n    return x.f(2, 3)\n",
        "'f' expects 1 argument(s), got 2",
    );
}

// TICKET-076: a match over a tuple whose arms cover the full cartesian product of the constituent
// enums is falsely rejected as non-exhaustive today (the checker only tracks a flat top-level
// covered-key set, which a tuple scrutinee has none of). rustc accepts the identical program.
#[test]
fn tuple_match_cartesian_product_of_two_enums_is_exhaustive() {
    ok(
        "enum E:\n    A\n    B\nenum F:\n    X\n    Y\nfn f(a: E, b: F) -> str:\n    match (a, b):\n        (E.A, F.X): return \"ax\"\n        (E.A, F.Y): return \"ay\"\n        (E.B, F.X): return \"bx\"\n        (E.B, F.Y): return \"by\"\n",
    );
}

// TICKET-076 neighbour n2: same headers, the (E.B, F.Y) arm deleted — still non-exhaustive.
#[test]
fn tuple_product_missing_one_combination_still_rejected() {
    rejects(
        "enum E:\n    A\n    B\nenum F:\n    X\n    Y\nfn f(a: E, b: F) -> str:\n    match (a, b):\n        (E.A, F.X): return \"ax\"\n        (E.A, F.Y): return \"ay\"\n        (E.B, F.X): return \"bx\"\n",
        "non-exhaustive match: add a `_` arm",
    );
}

// TICKET-076 neighbour n3: nested Option — Some(None)/Some(Some(v))/None is exhaustive.
#[test]
fn nested_option_some_none_some_some_none_is_exhaustive() {
    ok(
        "fn f(x: int??) -> str:\n    match x:\n        ?None: return \"in\"\n        ?(?v): return \"v {v}\"\n        None: return \"on\"\n",
    );
}

// TICKET-076 neighbour n4: n1 with a guard on the first arm — a guard never covers, still rejected.
#[test]
fn tuple_product_with_a_guarded_arm_still_rejected() {
    rejects(
        "enum E:\n    A\n    B\nenum F:\n    X\n    Y\nfn f(a: E, b: F) -> str:\n    match (a, b):\n        (E.A, F.X) if true: return \"ax\"\n        (E.A, F.Y): return \"ay\"\n        (E.B, F.X): return \"bx\"\n        (E.B, F.Y): return \"by\"\n",
        "non-exhaustive match: add a `_` arm",
    );
}

// TICKET-076 neighbour n5: enum x bool product is exhaustive.
#[test]
fn tuple_product_over_an_enum_and_a_bool_is_exhaustive() {
    ok(
        "enum E:\n    A\n    B\nfn f(a: E, b: bool) -> str:\n    match (a, b):\n        (E.A, true): return \"at\"\n        (E.A, false): return \"af\"\n        (E.B, _): return \"b\"\n",
    );
}

// TICKET-076 neighbour n6: enum x int with a literal column — int is infinite, still rejected.
#[test]
fn tuple_product_with_an_int_literal_column_still_rejected() {
    rejects(
        "enum E:\n    A\n    B\nfn f(a: E, n: int) -> str:\n    match (a, n):\n        (E.A, 0): return \"a0\"\n        (E.B, _): return \"b\"\n",
        "non-exhaustive match: add a `_` arm",
    );
}

// TICKET-076 neighbour n7: an or-pattern covering every variant of a 3-variant enum is exhaustive.
#[test]
fn tuple_product_of_or_patterns_covers_every_variant() {
    ok(
        "enum G:\n    A\n    B\n    C\nfn f(a: G, b: G) -> str:\n    match (a, b):\n        (G.A | G.B, _): return \"ab\"\n        (G.C, _): return \"c\"\n",
    );
}

// TICKET-076 neighbour n8: an all-binding tuple arm stays irrefutable (unchanged).
#[test]
fn all_binding_tuple_arm_stays_irrefutable() {
    ok("fn f(p: (int, int)) -> str:\n    match p:\n        (x, y): return \"{x},{y}\"\n");
}

// TICKET-076 neighbour n9: a range column refined by a wildcard sibling is exhaustive.
#[test]
fn tuple_product_with_a_wildcard_column_is_exhaustive() {
    ok(
        "enum E:\n    A\n    B\nfn f(a: E, n: int) -> str:\n    match (a, n):\n        (E.A, 0..5): return \"a-lo\"\n        (E.A, _): return \"a-hi\"\n        (E.B, _): return \"b\"\n",
    );
}

// TICKET-076 variant-add regression: n1 with a third variant `C` added to `enum E`, arms unchanged —
// must reject again, with a `help:` line naming `E.C`.
#[test]
fn adding_a_variant_makes_the_product_non_exhaustive_again() {
    rejects_help(
        "enum E:\n    A\n    B\n    C\nenum F:\n    X\n    Y\nfn f(a: E, b: F) -> str:\n    match (a, b):\n        (E.A, F.X): return \"ax\"\n        (E.A, F.Y): return \"ay\"\n        (E.B, F.X): return \"bx\"\n        (E.B, F.Y): return \"by\"\n",
        "non-exhaustive match: add a `_` arm",
        "E.C",
    );
}

// TICKET-076 review finding: exh_lower's depth bail must NOT widen to Pat::Wild. A 7-deep nested
// Option chain with an or-pattern at each level lowers the innermost literal past MAX_DEPTH while
// the witness search reaches that column well within it, so a Pat::Wild bail reads the whole
// pattern as covering its column and falsely accepts a match missing the deep non-zero payload.
#[test]
fn deeply_nested_or_pattern_beyond_max_depth_still_rejected() {
    rejects(
        "fn f(x: int???????) -> int:\n    match x:\n        ?(?(?(?(?(?(?0 | None) | None) | None) | None) | None) | None): return 1\n        None: return 0\n",
        "non-exhaustive match on int???????: missing ?_",
    );
}

// TICKET-093: `fn`-type params are compared COVARIANTLY (src/checker/proto.rs:1233), so a narrower
// `fn(Dog) -> Dog` is wrongly accepted where `fn(Any) -> Dog` is declared. Parameters must be
// contravariant; this must be a type error.
#[test]
fn fn_type_param_covariance_hole_rejected() {
    rejects(
        "struct Dog:\n    name: str\nstruct Cat:\n    name: int\nfn idd(d: Dog) -> Dog:\n    return d\nfn f():\n    h: fn(Any) -> Dog = idd\n",
        "cannot assign",
    );
}

// TICKET-093: a narrower fn value must be rejected at every sink that holds a fn-typed slot, not
// just a plain `:=` binding — HOF argument, List element, Map value, struct field and declared
// return type all zip parameters the same way.
#[test]
fn fn_type_param_invariance_rejects_a_narrow_fn_at_every_sink() {
    rejects(
        "struct Dog:\n    name: str\nfn idd(d: Dog) -> Dog:\n    return d\nfn hof(f: fn(Any) -> Dog) -> Dog:\n    return f(1)\nz := hof(idd)\n",
        "argument 1 of 'hof': expected fn(Any) -> Dog, found fn(Dog) -> Dog",
    );
    rejects(
        "struct Dog:\n    name: str\nfn idd(d: Dog) -> Dog:\n    return d\nfs: List[fn(Any) -> Dog] = [idd]\n",
        "list element: expected fn(Any) -> Dog, found fn(Dog) -> Dog",
    );
    rejects(
        "struct Dog:\n    name: str\nfn idd(d: Dog) -> Dog:\n    return d\nm: Map[str, fn(Any) -> Dog] = {\"a\": idd}\n",
        "map value: expected fn(Any) -> Dog, found fn(Dog) -> Dog",
    );
    rejects(
        "struct Dog:\n    name: str\nstruct Box:\n    f: fn(Any) -> Dog\nfn idd(d: Dog) -> Dog:\n    return d\nb := Box(idd)\n",
        "argument 1 of 'Box': expected fn(Any) -> Dog, found fn(Dog) -> Dog",
    );
    rejects(
        "struct Dog:\n    name: str\nfn idd(d: Dog) -> Dog:\n    return d\nfn mk() -> fn(Any) -> Dog:\n    return idd\n",
        "expected return type fn(Any) -> Dog, found fn(Dog) -> Dog",
    );
}

// TICKET-093: `src/checker/ty.rs:640`'s `compatible`-based `Func`/`Func` arm is reached by protocol
// method conformance and by container type arguments, not just `assignable` — both were measured
// ACCEPTED on the base binary.
#[test]
fn fn_type_param_invariance_pins_the_compatible_site() {
    rejects(
        "protocol Wants:\n    fn go(self, f: fn(Error) -> int) -> int\nstruct S:\n    id: int\n    fn go(self, f: fn(str) -> int) -> int:\n        return f(\"x\")\nw: Wants = S(1)\n",
        "type S does not satisfy Wants (method 'go' has the wrong signature)",
    );
    rejects(
        "fn g(s: str) -> int:\n    return s.len()\nxs := [g]\nys: List[fn(Error) -> int] = xs\n",
        "cannot assign List[fn(str) -> int] to variable of type List[fn(Error) -> int]",
    );
}

// TICKET-093: the intrinsic `(Protocol("Error"), Str)` grant at `src/checker/ty.rs:595` must not
// smuggle a builtin fn value into a fn(Error)-typed parameter — a one-way `compatible` call leaves
// this open because the grant is asymmetric.
#[test]
fn fn_type_error_grant_does_not_smuggle_a_builtin_into_a_param() {
    rejects(
        "struct MyErr:\n    code: int\n    fn message(self) -> str:\n        return \"e\"\nh: fn(Error) -> int = ord\n",
        "cannot assign fn(str) -> int to variable of type fn(Error) -> int",
    );
}

// TICKET-093: invariance must reach a nested type argument of a fn parameter, not just the fn
// parameter's own top-level type.
#[test]
fn fn_type_param_invariance_reaches_a_nested_type_argument() {
    rejects(
        "struct Dog:\n    name: str\nfn idd(d: Dog) -> Dog:\n    return d\nh: fn(Any?) -> Dog = fn(o: Dog?): idd(Dog(\"x\"))\n",
        "cannot assign fn(Dog?) -> Dog to variable of type fn(Any?) -> Dog",
    );
}

// TICKET-093: contravariance is deliberately refused. The direction table's row 2 — the
// contravariant-legal case — must keep rejecting under invariance too.
#[test]
fn fn_type_param_invariance_still_rejects_a_wide_fn_into_a_narrow_slot() {
    rejects(
        "fn wide(a: Any) -> str:\n    return \"w\"\nh: fn(int) -> str = wide\n",
        "cannot assign fn(Any) -> str to variable of type fn(int) -> str",
    );
}

// TICKET-093: the parameter fix must not move the optional-arity rule, protocol conformance for a
// fn-typed parameter that already matches, or the common HOF/closure-literal shapes.
#[test]
fn fn_type_param_invariance_leaves_the_neighbours_alone() {
    rejects(
        "fn a(x: int = 1) -> int:\n    return x\nfn b(x: int) -> int:\n    return x\nh := a\nh = b\n",
        "the value requires 1 argument(s) but the target may be called with as few as 0",
    );
    rejects(
        "struct Dog:\n    name: str\nprotocol Feeder:\n    fn feed(self, a: Any) -> int\nstruct Keeper:\n    id: int\n    fn feed(self, a: Dog) -> int:\n        return 1\nk: Feeder = Keeper(1)\n",
        "type Keeper does not satisfy Feeder (method 'feed' has the wrong signature)",
    );
    ok("fn a(x: int = 1) -> int:\n    return x\nh: fn(int) -> int = a\nprint(h(2))\n");
    ok(
        "fn applyit(f: fn(int) -> int, x: int) -> int:\n    return f(x)\nfn dbl(n: int) -> int:\n    return n * 2\nprint(applyit(dbl, 3))\nprint(applyit(fn(x): x + 1, 3))\nprint([1, 2, 3].map(fn(x): x * 2))\n",
    );
}

// TICKET-098 sub-bug A: a `Dom::Prod`'s label is BOTH its sole constructor name and its display
// prefix, so `display_of` joins the module-mangled key to itself instead of printing the bare
// struct name once.
#[test]
fn struct_match_witness_is_spellable_not_module_mangled() {
    let errs = check_entry(
        "struct S:\n    a: int\n    b: int\n\nfn f(s: S) -> int:\n    match s:\n        S(1, 2): return 0\n\n    return 1\n",
    );
    assert_eq!(
        errs.len(),
        1,
        "expected one non-exhaustive-match error, got: {errs:?}"
    );
    let help = errs[0].help.as_deref().expect("expected a help message");
    assert_eq!(help, "pattern `S(_, _)` is not covered");
}

// TICKET-098 sub-bug A: the same `Dom::Prod` defect also hits a struct nested in an enum payload,
// since `exh_new`'s `MatchKind::Struct` label is only one of two constructors that build a
// `Dom::Prod` -- `dom_of_ty`'s `Ty::Struct` arm builds the other.
#[test]
fn struct_witness_inside_an_enum_payload_is_spellable() {
    let errs = check_entry(
        "struct P:\n    x: int\n\nenum W:\n    K(P)\n\nfn k(w: W) -> int:\n    match w:\n        W.K(P(1)): return 0\n\n    return 1\n",
    );
    assert_eq!(
        errs.len(),
        1,
        "expected one non-exhaustive-match error, got: {errs:?}"
    );
    let help = errs[0].help.as_deref().expect("expected a help message");
    assert_eq!(help, "pattern `W.K(P(_))` is not covered");
}

// TICKET-098 sub-bug A: an enum witness's prefix must survive the fix -- only `Dom::Prod` drops its
// prefix, `Dom::Sum` keeps `display_of(dom_label(dom), name)`.
#[test]
fn enum_match_witness_stays_bare_prefixed() {
    let errs = check_entry(
        "enum E:\n    A(int)\n    B\n\nfn g(e: E) -> int:\n    match e:\n        E.B: return 0\n\n    return 1\n",
    );
    assert_eq!(
        errs.len(),
        1,
        "expected one non-exhaustive-match error, got: {errs:?}"
    );
    let help = errs[0].help.as_deref().expect("expected a help message");
    assert_eq!(help, "pattern `E.A(_)` is not covered");
}

// TICKET-098 sub-bug A: a tuple's witness must survive the fix -- a tuple's `Dom::Prod` has an
// empty label, so its empty ctor name hits `render_wit`'s empty-name arm and stays parenthesised.
#[test]
fn tuple_match_witness_stays_parenthesised() {
    let errs = check_entry(
        "enum E:\n    A(int)\n    B\n\nfn h(t: (E, E)) -> int:\n    match t:\n        (E.B, E.B): return 0\n\n    return 1\n",
    );
    assert_eq!(
        errs.len(),
        1,
        "expected one non-exhaustive-match error, got: {errs:?}"
    );
    let help = errs[0].help.as_deref().expect("expected a help message");
    assert_eq!(help, "pattern `(E.A(_), _)` is not covered");
}

/// W12-7 (TICKET-106): a protocol-typed `Map`/`Set` key rejects a literal concrete-type value
/// even though the same value already type-checks through a typed intermediate (`k: Keyed = A(1);
/// m[k] = "ok"`) and through `List[Keyed]`. `m[A(1)] = "lit"` and `Set[Keyed] = {A(1), A(2)}` must
/// both be accepted, mirroring Go's `map[Keyed]string{}; m[A{1}] = "a1"`.
#[test]
fn w12_7_protocol_typed_map_set_key_accepts_literal_satisfying_value() {
    ok(
        "protocol Keyed:\n    fn hash(self) -> int\nstruct A:\n    n: int\n    fn hash(self) -> int:\n        return self.n\n    fn eq(self, other: A) -> bool:\n        return self.n == other.n\nm: Map[Keyed, str] = {}\nm[A(1)] =\"lit\"\ns: Set[Keyed] = {A(1), A(2)}\nprint(m)\nprint(s)\n",
    );
}

/// W12-7 (TICKET-106) neighbours: a protocol-typed map/set key must accept any literal that
/// satisfies the protocol (via `assignable`), keep rejecting one that does not, and leave the
/// existing `List[Keyed]` and `Map[Hashable, int]` spellings unchanged.
#[test]
fn w12_7_protocol_key_neighbours_accept_and_reject() {
    let prelude = "protocol Keyed:\n    fn hash(self) -> int\nstruct A:\n    n: int\n    fn hash(self) -> int:\n        return self.n\n    fn eq(self, other: A) -> bool:\n        return self.n == other.n\nstruct B:\n    n: int\nm: Map[Keyed, str] = {}\n";
    ok(&format!("{prelude}print(m[A(1)])\n"));
    ok(&format!("{prelude}print(A(1) in m)\n"));
    ok(&format!(
        "{prelude}s: Set[Keyed] = {{A(1)}}\nprint(A(1) in s)\n"
    ));
    ok(&format!(
        "{prelude}xs: List[Keyed] = [A(1)]\nprint(A(1) in xs)\n"
    ));
    ok("import Hashable from std.cmp\nh: Map[Hashable, int] = {}\nh[1] = 1\nprint(h[1])\n");
    rejects(
        &format!("{prelude}print(m[B(1)])\n"),
        "map key must be Keyed, found B",
    );
    rejects(
        &format!("{prelude}m[B(1)] = \"no\"\n"),
        "map key must be Keyed, found B",
    );
    rejects(
        &format!("{prelude}s2: Set[Keyed] = {{A(1), B(2)}}\n"),
        "set element: expected Keyed, found B",
    );
    rejects(
        &format!("{prelude}print(B(1) in m)\n"),
        "cannot test membership of B in Map[Keyed, str]",
    );
}

/// W12-8 (TICKET-106): `[S: Iterable[(A, B)], A, B]` fails to recover `A`/`B` from a
/// `List[(int, str)]` argument, even though the bare `[S: Iterable[T], T]` shape recovers `T`
/// fine. `docs/reference/syntax/` §7b promises `Iterable[T]` recovers `T` by unifying against the
/// iterand's element type; a tuple element must unify the same way, mirroring Rust's
/// `I: IntoIterator<Item = (A, B)>` inference.
#[test]
fn w12_8_iterable_tuple_bound_recovers_element_type_params() {
    ok(
        "import Iterable from std.iter\nfn firsts[S: Iterable[(A, B)], A, B](it: S) -> List[A]:\n    out: List[A] = []\n    for p in it:\n        out.push(p.0)\n    return out\nr := firsts([(1, \"a\"), (2, \"b\")])\nprint(r)\n",
    );
}

/// W12-8 (TICKET-106) neighbours: recovery must unify STRUCTURALLY, not just a bare `Ty::Param`, so
/// `Option[A]`, `List[A]` and `(A, B)` all recover their nested params, an explicit turbofish still
/// works, and a genuinely mismatched element still rejects.
#[test]
fn w12_8_structured_bound_arg_recovery_neighbours() {
    ok(
        "import Iterable from std.iter\nfn f1[S: Iterable[A?], A](it: S) -> int:\n    n := 0\n    for _ in it:\n        n = n + 1\n    return n\nprint(f1([?1, None]))\n",
    );
    ok(
        "import Iterator from std.iter\nfn f2[S: Iterator[List[A]], A](it: S) -> int:\n    n := 0\n    for _ in it:\n        n = n + 1\n    return n\nprint(f2([[1], [2]].iter()))\n",
    );
    ok(
        "import Index from std.ops\nfn f3[C: Index[int, (A, B)], A, B](c: C) -> int:\n    return 1\nprint(f3([(1, \"a\")]))\n",
    );
    ok(
        "import Iterable from std.iter\nfn firsts[S: Iterable[(A, B)], A, B](it: S) -> List[A]:\n    out: List[A] = []\n    for p in it:\n        out.push(p.0)\n    return out\nr := firsts[List[(int, str)], int, str]([(1, \"a\")])\nprint(r)\n",
    );
    rejects(
        "import Iterable from std.iter\nfn f4[S: Iterable[(A, B)], A, B](it: S) -> int:\n    n := 0\n    for _ in it:\n        n = n + 1\n    return n\nprint(f4([1, 2]))\n",
        "iterator element type int does not match the declared element type (A, B)",
    );
}

/// W12-9 (TICKET-106): a user generic ctor (`Box[T]`) does not thread the expected type
/// (`Box[Named]`) into its argument's inference the way `Option`/`Some` already does
/// (`o: Option[Named] = Some(A())` is accepted). `Box(A())` at a `Box[Named]` sink must widen
/// `A` to `Named`, mirroring Rust's `let b: Box<dyn Named> = Box::new(A)` coercion.
#[test]
fn w12_9_generic_struct_ctor_uses_expected_type_hint() {
    ok(
        "protocol Named:\n    fn name(self) -> str\nstruct A:\n    fn name(self) -> str:\n        return \"a\"\nstruct Box[T]:\n    v: T\nb: Box[Named] = Box(A())\nprint(b)\n",
    );
}

/// W12-9 (TICKET-106) neighbours: an expected type widens an argument-bound type arg at every hint
/// site — enum variant, `Box[Any]` argument slot, list element, return annotation, a free-fn/static
/// -method generic constructor, and an instance generic method.
#[test]
fn w12_9_expected_type_widens_generic_targs_at_every_sink() {
    let prelude = "protocol Named:\n    fn name(self) -> str\nstruct A:\n    fn name(self) -> str:\n        return \"a\"\nstruct Box[T]:\n    v: T\nenum E[T]:\n    V(T)\n";
    ok(&format!("{prelude}e: E[Named] = E.V(A())\nprint(e)\n"));
    ok(&format!(
        "{prelude}fn f(x: Box[Any]) -> int:\n    return 1\nprint(f(Box(1)))\n"
    ));
    ok(&format!(
        "{prelude}xs: List[Box[Named]] = [Box(A())]\nprint(xs)\n"
    ));
    ok(&format!(
        "{prelude}fn g() -> Box[Any]:\n    return Box(1)\nprint(g())\n"
    ));
    ok(&format!(
        "{prelude}fn mk[T](x: T) -> Box[T]:\n    return Box(x)\nc: Box[Named] = mk(A())\nprint(c)\n"
    ));
    ok(&format!(
        "{prelude}fn mkl[T](x: T) -> List[T]:\n    return [x]\nys: List[Named] = mkl(A())\nprint(ys)\n"
    ));
    ok(&format!(
        "{prelude}struct Box2[T]:\n    v: T\n    fn of(x: T) -> Box2[T]:\n        return Box2(x)\nb2: Box2[Named] = Box2.of(A())\nprint(b2)\n"
    ));
    ok(&format!(
        "{prelude}struct Maker:\n    fn wrap[T](self, x: T) -> Box[T]:\n        return Box(x)\nw: Box[Named] = Maker().wrap(A())\nprint(w)\n"
    ));
}

/// W12-9 (TICKET-106) neighbours: the widen must decline where it would move an already-typed
/// binding — a mismatched scalar type arg, or an aliased mutable value whose declared type would
/// silently change.
#[test]
fn w12_9_expected_type_widen_declines_unsound_neighbours() {
    let prelude = "struct Box[T]:\n    v: T\n";
    rejects(
        &format!("{prelude}bi: Box[int] = Box(\"s\")\n"),
        "cannot assign Box[str] to variable of type Box[int]",
    );
    rejects(
        "protocol Named:\n    fn name(self) -> str\nstruct A:\n    fn name(self) -> str:\n        return \"a\"\nstruct Bag[T]:\n    items: List[T]\nzs := [A()]\nd: Bag[Named] = Bag(zs)\n",
        "cannot assign Bag[A] to variable of type Bag[Named]",
    );
}

/// W12-15 (TICKET-106): a `[T: Comparable]` generic call rejects the same int/float widening
/// granted at an untyped sink (`x := [1, 2.5]`, `if c: 1 else: 2.5`) once both constants land at
/// a generic call's two positions instead of one. Go's `Max(1, 2.5)` under `[T cmp.Ordered]`
/// widens and returns `2.5`.
#[test]
fn d3_w12_15_generic_call_rejects_mixed_int_float_constants() {
    rejects(
        "import Comparable from std.cmp\nfn mx[T: Comparable](a: T, b: T) -> T:\n    if a > b:\n        return a\n    return b\nprint(mx(1, 2.5))\n",
        "write 1.0",
    );
}

/// W12-15 (TICKET-106) neighbours: the widen fires for `float`-either-side and through a typed
/// local, keeps rejecting a mismatched type and an explicit turbofish, and keeps rejecting a
/// TYPED int (not a bare constant) beside a float.
#[test]
fn d3_w12_15_mixed_constant_widen_neighbours() {
    let prelude = "import Comparable from std.cmp\nfn mx[T: Comparable](a: T, b: T) -> T:\n    if a > b:\n        return a\n    return b\n";
    rejects(&format!("{prelude}print(mx(2.5, 1))\n"), "write 1.0");
    rejects(
        &format!("{prelude}x := 2.5\nprint(mx(1, x))\n"),
        "write 1.0",
    );
    rejects(
        "print([1.5, 2.5].fold(0, fn(a: float, b: float) -> float: a + b))\n",
        "write 1.0",
    );
    ok("print([1.5, 2.5].fold(0.0, fn(a: float, b: float) -> float: a + b))\n");
    rejects(
        &format!("{prelude}print(mx(1, \"x\"))\n"),
        "argument to 'mx' has type str, expected int",
    );
    rejects(
        &format!("{prelude}print(mx[int](1, 2.5))\n"),
        "argument to 'mx' has type float, expected int",
    );
    rejects(
        &format!("{prelude}n := 1\nprint(mx(n, 2.5))\n"),
        "argument to 'mx' has type float, expected int",
    );
}

/// W12-15 (TICKET-106) neighbour: a default value on a bare `T` slot stays rejected — this is the
/// rule that keeps a spliced default from ever sharing a call-table span across callers
/// (see `## Digest` gotcha 1).
#[test]
fn w12_15_bare_type_param_default_stays_rejected() {
    rejects(
        "import Comparable from std.cmp\nfn md[T: Comparable](a: T, b: T = 1) -> T:\n    if a > b:\n        return a\n    return b\nprint(md(3))\n",
        "default value for parameter 'b': expected T, found int",
    );
}

#[test]
fn bare_name_catch_all_ok_on_enum_scrutinee() {
    ok(
        "enum E:\n    A(int)\n    B\n\nfn f(e: E, n: int) -> str:\n    return match e:\n        x if n > 0: \"g\"\n        E.A(1): \"one\"\n        E.A(_): \"a\"\n        E.B: \"b\"\n\nfn main():\n    print(f(E.A(1), 1))\n",
    );
}

#[test]
fn bare_name_catch_all_ok_on_option_scrutinee() {
    ok(
        "fn f(o: int?) -> str:\n    return match o:\n        whole: \"w{whole}\"\n\nfn main():\n    print(f(?1))\n",
    );
}

#[test]
fn if_expr_success_coerces_at_option_return_sink() {
    ok("fn opt(n: int) -> int?: (if n > 0: n else: None)\n\nfn main():\n    print(opt(2))\n");
}

// TICKET-258 (U15): `List += List` rebinds like every compound form, so both guards reject it.
#[test]
fn list_plus_equals_rejected_through_loop_variable() {
    rejects(
        "fn main():\n    lst := [[1], [2]]\n    for row in lst:\n        row += [0]\n    print(lst)\n",
        "cannot assign to loop variable 'row'",
    );
}

#[test]
fn list_plus_equals_rejected_through_const_binding() {
    rejects(
        "fn main():\n    xs: const List[int] = [1]\n    xs += [3]\n    print(xs)\n",
        "cannot reassign const binding 'xs'",
    );
}

#[test]
fn ticket_107_bare_catch_all_on_enum_closes_the_match_without_underscore() {
    ok(
        "enum E:\n    A(int)\n    B\n\nfn f(e: E) -> int:\n    match e:\n        E.A(1): return 1\n        rest: return 2\n\nfn main():\n    print(f(E.B))\n",
    );
}

#[test]
fn ticket_107_guarded_bare_catch_all_on_enum_does_not_close_the_match() {
    rejects(
        "enum E:\n    A(int)\n    B\n\nfn f(e: E, n: int) -> int:\n    match e:\n        x if n > 0: return 1\n        E.A(_): return 2\n    return 0\n",
        "non-exhaustive match on E: missing B",
    );
}

#[test]
fn ticket_107_unguarded_bare_catch_all_warns_a_later_arm_unreachable() {
    warns(
        "enum E:\n    A(int)\n    B\n\nfn f(e: E) -> int:\n    match e:\n        rest: return 1\n        E.B: return 2\n",
        "unreachable match arm",
    );
}

#[test]
fn ticket_107_bare_catch_all_on_option_binds_the_whole_option() {
    rejects(
        "fn f(o: int?) -> int:\n    match o:\n        whole: return whole\n",
        "expected return type int, found int?",
    );
}

#[test]
fn ticket_107_bare_catch_all_on_a_generic_enum_keeps_its_type_args() {
    ok(
        "enum T[X]:\n    Leaf(X)\n    Nil\n\nfn g(t: T[int]) -> int:\n    match t:\n        T.Leaf(v): return v\n        _: return 0\n\nfn h(t: T[int]) -> int:\n    match t:\n        T.Nil: return 0\n        other: return g(other)\n\nfn main():\n    print(h(T.Leaf(3)))\n",
    );
}

#[test]
fn ticket_107_bare_variant_names_stay_rejected_on_enum_scrutinees() {
    rejects(
        "enum E:\n    A\n    B\n\nfn f(e: E) -> int:\n    match e:\n        None: return 1\n        _: return 2\n",
        "'None' is not a variant of E",
    );
    rejects(
        "enum E:\n    A\n    B\n\nfn f(e: E) -> int:\n    match e:\n        A: return 1\n        _: return 2\n",
        "'A' is a variant of enum 'E'; write it qualified as 'E.A'",
    );
    rejects(
        "enum E:\n    A\n    B\n\nenum F:\n    X\n\nfn f(e: E) -> int:\n    match e:\n        X: return 1\n        _: return 2\n",
        "'X' is a variant of enum 'F'; write it qualified as 'F.X'",
    );
}

#[test]
fn ticket_107_inline_if_expr_success_coerces_at_result_sink() {
    ok("fn res(n: int) -> int!: (if n > 0: n else: !\"neg\")\n\nfn main():\n    print(res(2))\n");
}

#[test]
fn ticket_107_match_expr_success_coerces_at_option_sink() {
    ok(
        "enum S:\n    C(int)\n    P\n\nfn g(s: S) -> int?:\n    return match s:\n        S.C(r): r\n        S.P: None\n\nfn main():\n    print(g(S.P))\n",
    );
}

#[test]
fn ticket_107_closure_if_expr_success_coerces_at_option_sink() {
    ok("fn main():\n    g := fn(n: int) -> int?: if n > 0: n else: None\n    print(g(3))\n");
}

#[test]
fn ticket_107_elif_chain_success_coerces_at_option_sink() {
    ok(
        "fn f(n: int) -> int?:\n    return if n > 0: n elif n == 0: None else: 0 - n\n\nfn main():\n    print(f(0))\n",
    );
}

#[test]
fn ticket_107_mixed_branch_coercion_wraps_at_every_typed_slot() {
    ok("fn main():\n    x: int? = if true: 1 else: None\n");
    ok("fn t(x: int?) -> int:\n    return 0\n\nfn main():\n    print(t(if true: 1 else: None))\n");
    // A literal `None` beside a value gives the inline body an `int?` (TICKET-234).
    ok("fn f(c: bool): if c: 1 else: None\n");
    rejects(
        "fn f(c: bool) -> float?:\n    return if c: 1 else: None\n",
        "branches have incompatible types: int and None",
    );
    rejects(
        "fn f(o: int?) -> int?:\n    return o ?? None\n",
        "'??' sides have incompatible types: int and None",
    );
    rejects(
        "fn f[T](x: T, c: bool) -> T?:\n    return if c: x else: None\n",
        "branches have incompatible types: T and None",
    );
    rejects(
        "fn have(n: int) -> int?:\n    return n\nfn f(c: bool) -> int??:\n    return if c: have(1) else: None\n",
        "expected return type int??, found int?",
    );
}

#[test]
fn ticket_107_rebinding_compound_assign_stays_rejected_on_loop_var_and_const() {
    rejects(
        "fn main():\n    for row in [[1]]:\n        row *= 2\n",
        "cannot assign to loop variable 'row'",
    );
    rejects(
        "fn main():\n    for row in [[1]]:\n        row = [2]\n",
        "cannot assign to loop variable 'row'",
    );
    rejects(
        "fn main():\n    for row in [[1]]:\n        row += 5\n",
        "cannot assign to loop variable 'row'",
    );
    rejects(
        "fn main():\n    for f in [1.5]:\n        f += 1.0\n",
        "cannot assign to loop variable 'f'",
    );
    rejects(
        "fn main():\n    xs: const List[int] = [1]\n    xs *= 2\n",
        "cannot reassign const binding 'xs'",
    );
    rejects(
        "fn main():\n    s: const str = \"a\"\n    s += \"b\"\n",
        "cannot reassign const binding 's'",
    );
}

/// TICKET-108 / W12-10 repro -- two structs each declaring a same-named static method with a
/// default (`fn new(n: int = 1) -> Self`) must resolve `Type.new()` from the struct it is
/// called on. Desugar's old receiver lookup (gone since TICKET-182) had no arm for a bare
/// struct-name head, so `A.new()` fell into the name-keyed `methods` table and bailed.
#[test]
fn a_same_named_static_method_on_two_structs_type_checks() {
    ok_desugared(
        "struct W108A:\n    v: int\n    fn new(n: int = 1) -> W108A:\n        return W108A(n)\nstruct W108B:\n    v: int\n    fn new(n: int = 1) -> W108B:\n        return W108B(n)\nprint(W108A.new().v)\nprint(W108B.new().v)\n",
    );
}

/// TICKET-108 / W12-10 guard -- a type parameter shadows a struct name. `f[A: Mk]() -> A: return
/// A.new()` must keep resolving `A.new()` through the PROTOCOL's arity, not splice in struct
/// `A`'s own default -- that would be check-clean but bind the wrong type's default.
#[test]
fn a_type_param_shadowing_a_struct_keeps_the_protocol_arity() {
    rejects_desugared(
        "protocol Mk:\n    fn new(n: int) -> Self\nstruct A:\n    v: int\n    fn new(n: int = 1) -> A:\n        return A(n)\nstruct B:\n    v: int\n    fn new(n: int = 7) -> B:\n        return B(n)\nfn f[A: Mk]() -> A:\n    return A.new()\nprint(f[B]().v)\n",
        "'new' expects 1 argument(s), got 0",
    );
}

/// TICKET-108 / W12-16(a) repro -- a fn value sitting in a tuple slot must be directly
/// callable, `t.0(3)`, the same as a fn-typed struct field / list element / map value already
/// are. `infer_call`'s method-call arm has no guard for a decimal tuple-slot member name, so
/// `t.0(3)` is routed to method-call resolution and rejected with `has no method '0'`.
#[test]
fn a_fn_value_in_a_tuple_slot_is_callable_by_member_syntax() {
    ok_desugared("fn dbl(x: int) -> int:\n    return x * 2\nt := (dbl, 1)\nprint(t.0(3))\n");
}

/// TICKET-108 / W12-16(d) -- an fn-type diagnostic must show which parameter is omittable, so two
/// distinct fn types whose only difference is one optional argument don't print identically.
#[test]
fn fn_type_display_marks_an_omittable_parameter() {
    rejects(
        "fn a(x: int = 1) -> int:\n    return x\nfn b(x: int) -> int:\n    return x\nxs := [a, b]\n",
        "list elements differ: fn(int = …) -> int vs fn(int) -> int",
    );
}

/// TICKET-108 / W12-16(c) -- a protocol type alias must be a valid bound: `type N = Named` then
/// `fn show[T: N]` binds `T` exactly as `[T: Named]` would, same errors, same messages.
#[test]
fn a_protocol_alias_bound_is_enforced_like_the_protocol() {
    const SRC_ALIAS: &str = "protocol Named:\n    fn name(self) -> str\ntype N = Named\nfn show[T: N](x: T) -> str:\n    return x.name()\ny := show(5)\n";
    const SRC_PLAIN: &str = "protocol Named:\n    fn name(self) -> str\nfn show[T: Named](x: T) -> str:\n    return x.name()\ny := show(5)\n";
    let plain = check_src(SRC_PLAIN);
    assert!(
        !plain.is_empty(),
        "expected a type error over the plain bound"
    );
    let alias = check_src(SRC_ALIAS);
    let plain_msgs: Vec<_> = plain.iter().map(|e| e.message.clone()).collect();
    let alias_msgs: Vec<_> = alias.iter().map(|e| e.message.clone()).collect();
    assert_eq!(
        plain_msgs, alias_msgs,
        "alias bound must report the identical errors as the plain protocol bound"
    );
}

/// TICKET-108 / W12-16(c) -- a GENERIC protocol alias (`type IntBag = Bag[int]`) applies type
/// arguments, so it cannot name a bound by itself: the substitution a bound would need is not done.
#[test]
fn a_bound_naming_a_generic_protocol_alias_is_refused_by_name() {
    rejects(
        "protocol Bag[T]:\n    fn get(self) -> T\ntype IntBag = Bag[int]\nfn f[T: IntBag](x: T) -> int:\n    return 1\n",
        "type alias 'IntBag' applies type arguments",
    );
}

/// TICKET-109 (W12-12): N nested `fn` declarations (`fn f0(): fn f1(): ... pass`) made `check`
/// exponential in N. The checker walks an un-annotated nested fn's body twice (`infer_fn_ret`, then
/// `check_fn_body`), and every enclosing inference walk repeats both. TICKET-157 memoizes the
/// speculative `infer_fn_ret` per nested-fn decl span, so the body is walked once per encounter.
/// Before the memo N=20 took 42.790s in the dev profile and N=24 never finished; with it N=30 is
/// 0.089s. TICKET-228 then stopped the speculative walk of a block body. This test drives the
/// production order (`resolver::build_graph`: desugar, then check) at N=16 and requires a clean
/// verdict and a COUNT (TICKET-233): each body is checked once, where a walk repeated per enclosing
/// fn checks 2^N.
#[test]
fn nested_fn_decl_check_is_not_exponential() {
    const N: usize = 16;
    let mut src = String::new();
    for i in 0..N {
        src.push_str(&"    ".repeat(i));
        src.push_str(&format!("fn f{i}():\n"));
    }
    src.push_str(&"    ".repeat(N));
    src.push_str("pass\n");

    let tokens = lexer::tokenize(&src).expect("lex should succeed");
    let mut module = parser::parse(tokens).expect("parse should succeed");
    let verdict = crate::desugar::run_standalone(&mut module)
        .map(|()| check_counting_fn_body_checks(&module));

    let checks = match verdict {
        Ok((Ok(()), checks)) => checks,
        other => panic!("{N} nested fn declarations must check clean, got: {other:?}"),
    };
    assert!(
        (N..=2 * N).contains(&checks),
        "checking {N} nested fn declarations checked {checks} fn bodies (want {N}..={}) -- \
         exponential checker cost in nested-fn-declaration depth (TICKET-109 / W12-12)",
        2 * N
    );
}

/// A HashMap's entries as sorted `key => value` Debug lines, so two runs compare deterministically.
fn sorted_debug<K: std::fmt::Debug, V: std::fmt::Debug>(m: &HashMap<K, V>) -> Vec<String> {
    let mut v: Vec<String> = m.iter().map(|(k, x)| format!("{k:?} => {x:?}")).collect();
    v.sort();
    v
}

/// TICKET-157 -- the nested-fn return memo (`Checker::ret_memo`) must be INVISIBLE: the same program
/// through the same entry point with `memo_enabled` off renders identical diagnostics AND identical
/// resolved side tables. The tables are what `DiagMark` deliberately does not snapshot (`carriers`,
/// `call_plans`, `witnesses`, `next_opt_tmp`), so only this comparison sees a memo that drops a
/// write one of them needed. A memo entry served in the wrong context (a stale finalize-pass type, a
/// pin the skipped walk would have written) makes one of the eleven programs render differently.
#[test]
fn the_nested_fn_ret_memo_is_invisible_to_diagnostics_and_tables() {
    let programs: [(&str, &str); 11] = [
        (
            "p1 canary",
            "fn outer(c: bool):\n    zs := []\n    fn g(d: bool) -> List[str]:\n        if d:\n            return [\"a\"]\n        return zs\n    g(c)\n    if c:\n        return zs[0]\n    return 1\nprint(outer(true))\n",
        ),
        (
            "clean 6-deep chain",
            "fn f0():\n    fn f1():\n        fn f2():\n            fn f3():\n                fn f4():\n                    fn f5():\n                        return 1\n                    return f5()\n                return f4()\n            return f3()\n        return f2()\n    return f1()\nprint(f0())\n",
        ),
        (
            "reads an enclosing local",
            "fn outer() -> int:\n    x := 5\n    fn b(): x + 1\n    fn a(): b()\n    return a()\nprint(outer())\n",
        ),
        (
            "reads a module global",
            "G := 3\nfn outer() -> int:\n    fn b(): G\n    fn a(): b()\n    return a()\nprint(outer())\n",
        ),
        (
            "self-recursive nested fn",
            "fn outer(n: int):\n    fn fact(k: int):\n        if k <= 1:\n            return 1\n        return k * fact(k - 1)\n    fn wrap(k: int):\n        return fact(k)\n    return wrap(n)\nprint(outer(4))\n",
        ),
        (
            "nested generator",
            "fn outer():\n    fn gen():\n        yield 1\n        yield 2\n    fn wrap():\n        for v in gen():\n            yield v\n    return wrap()\nprint(outer())\n",
        ),
        (
            "type error in a nested fn",
            "fn outer():\n    fn a():\n        fn b():\n            return 1 + \"x\"\n        return b()\n    return a()\nprint(outer())\n",
        ),
        (
            "?. carrier on an Option",
            "fn geto() -> str?:\n    return ?\"abc\"\nfn outer():\n    fn a():\n        fn b():\n            return geto()?.len()\n        return b()\n    return a()\nprint(outer())\n",
        ),
        (
            "keyword call through a fn value",
            "fn kw(a: int, b: int = 2) -> int:\n    return a + b\nfn outer():\n    fn h(x: int, y: int):\n        return x - y\n    fn a():\n        fn b():\n            return h(y=1, x=5) + kw(b=5, a=1)\n        return b()\n    return a()\nprint(outer())\n",
        ),
        (
            "generic call through a static-protocol bound",
            "protocol Default:\n    fn default() -> Self\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(0)\nfn reset[T: Default](old: T) -> T:\n    return T.default()\nfn outer():\n    fn a():\n        fn b():\n            return reset(Counter(5))\n        return b()\n    return a()\nprint(outer().n)\n",
        ),
        (
            "finalize-pass shape",
            "fn f(c: bool):\n    if c:\n        return ?1\n    return !\"bad\"\nfn h():\n    fn g() -> List[str]:\n        return f(true)\n    return g()\nx: int!int = h()\nprint(x)\n",
        ),
    ];
    for (name, src) in programs {
        let tokens = lexer::tokenize(src).expect("lex should succeed");
        let mut m = parser::parse(tokens).unwrap_or_else(|e| panic!("{name}: parse failed: {e:?}"));
        crate::desugar::run_standalone(&mut m)
            .unwrap_or_else(|e| panic!("{name}: desugar failed: {e:?}"));
        assert_eq!(
            format!("{:?}", check_diags(&m)),
            format!("{:?}", check_diags_no_memo(&m)),
            "{name}: memo on vs off changed the diagnostics"
        );
        let on = resolve_call_tables_standalone(&m.stmts);
        let off = resolve_call_tables_standalone_no_memo(&m.stmts);
        assert_eq!(
            sorted_debug(&on.0),
            sorted_debug(&off.0),
            "{name}: call_plans"
        );
        assert_eq!(
            sorted_debug(&on.1.fns),
            sorted_debug(&off.1.fns),
            "{name}: witnesses.fns"
        );
        assert_eq!(
            sorted_debug(&on.1.calls),
            sorted_debug(&off.1.calls),
            "{name}: witnesses.calls"
        );
        assert_eq!(
            sorted_debug(&on.2),
            sorted_debug(&off.2),
            "{name}: carriers"
        );
        assert_eq!(
            sorted_debug(&on.10),
            sorted_debug(&off.10),
            "{name}: hook_calls"
        );
        assert_eq!(
            sorted_debug(&on.9),
            sorted_debug(&off.9),
            "{name}: hook_table"
        );
        assert_eq!(
            sorted_debug(&on.3),
            sorted_debug(&off.3),
            "{name}: sum_seeds"
        );
        assert_eq!(
            sorted_debug(&on.4),
            sorted_debug(&off.4),
            "{name}: ret_coerce"
        );
        assert_eq!(sorted_debug(&on.8), sorted_debug(&off.8), "{name}: wraps");
        assert_eq!(on.5, off.5, "{name}: table_conflicts");
    }
}

/// TICKET-157 / W12-12 -- the fn-nesting cap is a guard, not a fix: a 24-deep chain of nested `fn`
/// declarations must CHECK (CPython compiles 50 deep in 0.02s), and cheaply: each body is checked
/// once (a count, TICKET-233). Before the fix the cap refuses it at 16.
#[test]
fn deep_nested_fn_decl_chain_checks_clean_and_fast() {
    const N: usize = 24;
    let mut src = String::new();
    for i in 0..N {
        src.push_str(&"    ".repeat(i));
        src.push_str(&format!("fn f{i}():\n"));
    }
    src.push_str(&"    ".repeat(N));
    src.push_str("pass\n");

    let tokens = lexer::tokenize(&src).expect("lex should succeed");
    let mut module = parser::parse(tokens).expect("parse should succeed");
    let verdict = crate::desugar::run_standalone(&mut module)
        .map(|()| check_counting_fn_body_checks(&module));

    let checks = match verdict {
        Ok((_, checks)) => checks,
        Err(err) => panic!("24 nested fn declarations were refused: {}", err.message),
    };
    assert!(
        (N..=2 * N).contains(&checks),
        "checking {N} nested fn declarations checked {checks} fn bodies (want {N}..={})",
        2 * N
    );
}

/// TICKET-120 / W13-11 repro -- two structs each declaring a same-named static method with a
/// default, resolved through a QUALIFIED `module.Type.new()` head. TICKET-108 (W12-10) added the
/// bare type-NAME head to desugar's receiver lookup (gone since TICKET-182) but not the qualified
/// `module.Type` head, so this fell into the name-keyed `methods` table and bailed, same as the
/// bare-name form did before TICKET-108.
#[test]
fn a_same_named_static_method_via_qualified_module_type_head_type_checks() {
    files_ok(&[
        (
            "lib_s3.chz",
            "struct A:\n    v: int\n    fn new(n: int = 1) -> A:\n        return A(n)\nstruct L:\n    v: int\n    fn new(n: int = 11) -> L:\n        return L(n)\n",
        ),
        ("main.chz", "import lib_s3\nprint(lib_s3.L.new().v)\n"),
    ]);
}

/// TICKET-120 step 1 -- the alias-import spelling of the qualified head must resolve too.
#[test]
fn a_qualified_module_type_head_resolves_a_static_default_through_an_import_alias() {
    files_ok(&[
        (
            "lib_s3.chz",
            "struct A:\n    v: int\n    fn new(n: int = 1) -> A:\n        return A(n)\nstruct L:\n    v: int\n    fn new(n: int = 11) -> L:\n        return L(n)\n",
        ),
        ("main.chz", "import lib_s3 as m\nprint(m.L.new().v)\n"),
    ]);
}

/// TICKET-120 step 1 -- an enum's static default resolves through the qualified head too.
#[test]
fn a_qualified_module_enum_head_resolves_a_static_default() {
    files_ok(&[
        (
            "lib_e.chz",
            "enum E:\n    A(int)\n    B\n    fn new(n: int = 7) -> E:\n        return E.A(n)\nenum F:\n    C(int)\n    D\n    fn new(n: int = 8) -> F:\n        return F.C(n)\n",
        ),
        ("main.chz", "import lib_e\nprint(lib_e.E.new())\n"),
    ]);
}

/// TICKET-120 step 1 -- a generic struct's static default resolves through the qualified head.
#[test]
fn a_qualified_generic_struct_head_resolves_a_static_default() {
    files_ok(&[
        (
            "lib_g.chz",
            "struct Box[T]:\n    v: T\n    fn make(n: int = 4) -> Box[int]:\n        return Box(n)\nstruct Bin[T]:\n    v: T\n    fn make(n: int = 9) -> Bin[int]:\n        return Bin(n)\n",
        ),
        ("main.chz", "import lib_g\nprint(lib_g.Box.make().v)\n"),
    ]);
}

/// TICKET-120 step 1 -- a static name colliding with a builtin still resolves its default.
#[test]
fn a_qualified_head_with_a_builtin_colliding_static_name_resolves_a_default() {
    files_ok(&[
        (
            "lib_b.chz",
            "struct SB:\n    n: int\n    fn add(n: int = 51) -> SB:\n        return SB(n)\nstruct SC:\n    n: int\n    fn add(n: int = 52) -> SC:\n        return SC(n)\n",
        ),
        ("main.chz", "import lib_b\nprint(lib_b.SB.add().n)\n"),
    ]);
}

/// TICKET-120 step 2 -- a defaultless argument through the qualified head still rejects on arity.
#[test]
fn a_qualified_static_call_missing_a_defaultless_argument_still_rejects() {
    files_reject(
        &[
            (
                "lib_b.chz",
                "struct SB:\n    n: int\n    fn add(n: int = 51) -> SB:\n        return SB(n)\nstruct SC:\n    n: int\n    fn add(n: int = 52) -> SC:\n        return SC(n)\nstruct SD:\n    n: int\n    fn need(n: int) -> SD:\n        return SD(n)\n",
            ),
            ("main.chz", "import lib_b\nprint(lib_b.SD.need().n)\n"),
        ],
        "'need' expects 1 argument(s), got 0",
    );
}

/// TICKET-120 step 2 -- an instance method reached through a qualified TYPE head is called receiver first, so a call with no receiver is an arity error.
#[test]
fn an_instance_method_through_a_qualified_type_head_needs_its_receiver() {
    files_reject(
        &[
            (
                "lib_s3.chz",
                "struct A:\n    v: int\n    fn new(n: int = 1) -> A:\n        return A(n)\nstruct L:\n    v: int\n    fn new(n: int = 11) -> L:\n        return L(n)\n    fn bump(self, k: int = 5) -> int:\n        return self.v + k\n",
            ),
            ("main.chz", "import lib_s3\nprint(lib_s3.L.bump())\n"),
        ],
        "'bump' expects",
    );
}

/// TICKET-120 step 2 -- a qualified head naming an unknown module member still rejects.
#[test]
fn a_qualified_head_naming_an_unknown_module_member_still_rejects() {
    files_reject(
        &[
            (
                "lib_s3.chz",
                "struct A:\n    v: int\n    fn new(n: int = 1) -> A:\n        return A(n)\nstruct L:\n    v: int\n    fn new(n: int = 11) -> L:\n        return L(n)\n",
            ),
            ("main.chz", "import lib_s3\nprint(lib_s3.Nope.new())\n"),
        ],
        "has no member 'Nope'",
    );
}

/// TICKET-120 step 2 -- too many arguments through the qualified head still rejects on arity.
#[test]
fn a_qualified_static_call_with_too_many_arguments_still_rejects() {
    files_reject(
        &[
            (
                "lib_s3.chz",
                "struct A:\n    v: int\n    fn new(n: int = 1) -> A:\n        return A(n)\nstruct L:\n    v: int\n    fn new(n: int = 11) -> L:\n        return L(n)\n",
            ),
            ("main.chz", "import lib_s3\nprint(lib_s3.L.new(1, 2).v)\n"),
        ],
        "got 2",
    );
}

/// TICKET-120 step 2 -- a cross-module struct-name collision keeps the qualified static call
/// rejected: the two declarations disagree, and the new arm must not invent an agreement that
/// isn't there.
#[test]
fn a_cross_module_struct_name_collision_binds_each_qualified_static_call() {
    // Each `lib.CC.new()` binds against its own module's declaration; `call_binding_grid`'s
    // `collision/qualified_static` cell runs it and pins `31` then `32`.
    files_ok(&[
        (
            "lib_c1.chz",
            "struct CC:\n    v: int\n    fn new(n: int = 31) -> CC:\n        return CC(n)\n",
        ),
        (
            "lib_c2.chz",
            "struct CC:\n    v: int\n    fn new(n: int = 32) -> CC:\n        return CC(n)\n",
        ),
        (
            "main.chz",
            "import lib_c1\nimport lib_c2\nprint(lib_c1.CC.new().v)\nprint(lib_c2.CC.new().v)\n",
        ),
    ]);
}

// ===== TICKET-124: expected-type widening missing at generic ctor / nested ctor / =====
// ===== reassignment / float-slot method-param sinks, plus a check-OK format spec =====

#[test]
fn d3_generic_ctor_rejects_untyped_int_to_float_slot() {
    rejects(
        "struct Pair[T]:\n    a: T\n    b: T\n\nfn main():\n    p := Pair(1, 2.5)\n    print(p)\n",
        "write 1.0",
    );
    ok(
        "struct Pair[T]:\n    a: T\n    b: T\n\nfn main():\n    p := Pair(1.0, 2.5)\n    print(p)\n",
    );
}

#[test]
fn nested_generic_ctor_widens_at_declared_protocol_type() {
    ok(
        "protocol Named:\n    fn name(self) -> str\n\nstruct A:\n    fn name(self) -> str:\n        return \"a\"\n\nstruct Box[T]:\n    v: T\n\nfn main():\n    bb: Box[Box[Named]] = Box(Box(A()))\n    print(bb)\n",
    );
}

const NESTED_CTOR_HINT_PRELUDE: &str = "protocol Named:\n    fn name(self) -> str\n\nstruct A:\n    fn name(self) -> str:\n        return \"a\"\n\nstruct Box[T]:\n    v: T\n\nstruct Bag[T]:\n    items: List[T]\n\n";

#[test]
fn nested_ctor_hint_neighbours_accept() {
    ok(&format!(
        "{NESTED_CTOR_HINT_PRELUDE}fn main():\n    bb2: Box[Box[Named]] = Box[Box[Named]](Box(A()))\n    print(bb2)\n"
    ));
    ok(&format!(
        "{NESTED_CTOR_HINT_PRELUDE}fn main():\n    t: (Box[Named], int) = (Box(A()), 1)\n    print(t)\n"
    ));
    ok(&format!(
        "{NESTED_CTOR_HINT_PRELUDE}fn main():\n    o: List[Named]? = ?[A()]\n    print(o)\n"
    ));
    ok(&format!(
        "{NESTED_CTOR_HINT_PRELUDE}fn main():\n    c: Box[List[Named]] = Box([A()])\n    print(c)\n"
    ));
    ok(&format!(
        "{NESTED_CTOR_HINT_PRELUDE}fn main():\n    r: Box[Named]!str = ?Box(A())\n    print(r)\n"
    ));
    ok(&format!(
        "{NESTED_CTOR_HINT_PRELUDE}fn mk() -> Box[Box[Named]]:\n    return Box(Box(A()))\n\nfn main():\n    print(mk())\n"
    ));
    ok(&format!(
        "{NESTED_CTOR_HINT_PRELUDE}fn tk(b: Box[Box[Named]]) -> int:\n    return 1\n\nfn main():\n    print(tk(Box(Box(A()))))\n"
    ));
    ok(&format!(
        "{NESTED_CTOR_HINT_PRELUDE}fn main():\n    g: Bag[Named] = Bag([A()])\n    print(g)\n"
    ));
}

#[test]
fn nested_ctor_hint_keeps_invariance() {
    rejects(
        &format!(
            "{NESTED_CTOR_HINT_PRELUDE}fn main():\n    b := Box(A())\n    bb: Box[Box[Named]] = Box(b)\n    print(bb)\n"
        ),
        "cannot assign Box[Box[A]] to variable of type Box[Box[Named]]",
    );
    rejects(
        &format!(
            "{NESTED_CTOR_HINT_PRELUDE}fn main():\n    zs := [A()]\n    g: Bag[Named] = Bag(zs)\n    print(g)\n"
        ),
        "cannot assign Bag[A] to variable of type Bag[Named]",
    );
    rejects(
        "fn main():\n    o: float? = ?1\n    print(o)\n",
        "'?' value: expected float, found int",
    );
}

#[test]
fn reassignment_widens_at_declared_protocol_type() {
    ok(
        "protocol Named:\n    fn name(self) -> str\n\nstruct A:\n    fn name(self) -> str:\n        return \"a\"\n\nfn main():\n    l: List[Named] = [A()]\n    l = [A()]\n    print(l)\n",
    );
}

#[test]
fn reassignment_hint_neighbours_accept() {
    ok(
        "protocol Named:\n    fn name(self) -> str\nstruct A:\n    fn name(self) -> str:\n        return \"a\"\nll: List[List[Named]] = [[A()]]\nll[0] = [A()]\nstruct Box[T]:\n    v: T\nb: Box[Named] = Box(A())\nbb: Box[Box[Named]] = Box[Box[Named]](b)\nbb.v = Box(A())\n",
    );
    ok(
        "protocol Keyed:\n    fn hash(self) -> int\nstruct A:\n    n: int\n    fn hash(self) -> int:\n        return self.n\n    fn eq(self, other: A) -> bool:\n        return self.n == other.n\nstruct B:\n    n: int\n    fn hash(self) -> int:\n        return self.n\n    fn eq(self, other: B) -> bool:\n        return self.n == other.n\nm2: Map[Keyed, int] = {B(2): 5}\nm2 = {B(3): 6}\nfn f(m: Map[Keyed, int]) -> int:\n    return m.len()\nnested: Map[Keyed, Map[Keyed, int]] = {A(1): {B(2): 5}}\nnested[A(1)] = {B(2): 5}\n",
    );
    ok(
        "protocol Named:\n    fn name(self) -> str\nstruct A:\n    fn name(self) -> str:\n        return \"a\"\nl: List[Named] = []\nl = [A()]\n",
    );
    ok(
        "protocol Named:\n    fn name(self) -> str\nstruct A:\n    fn name(self) -> str:\n        return \"a\"\nstruct Box[T]:\n    v: T\nt: (Box[Named], int) = (Box(A()), 1)\nt = (Box(A()), 2)\n",
    );
}

#[test]
fn reassignment_hint_keeps_invariance() {
    rejects(
        "protocol Named:\n    fn name(self) -> str\nstruct A:\n    fn name(self) -> str:\n        return \"a\"\nzs := [A()]\nl: List[Named] = []\nl = zs\n",
        "cannot assign List[A] to List[Named]",
    );
    rejects(
        "l: List[float] = [1.5]\nl = [1, 2]\n",
        "list element: expected float, found int",
    );
}

#[test]
fn d3_float_collection_method_param_rejects_untyped_int() {
    rejects(
        "fn main():\n    l: List[float] = [1.5]\n    l.push(3)\n    print(l)\n",
        "write 1.0",
    );
    ok("fn main():\n    l: List[float] = [1.5]\n    l.push(3.0)\n    print(l)\n");
}

#[test]
fn d3_reassignment_float_rejects_untyped_int_literal() {
    rejects(
        "fn main():\n    x: float = 1.5\n    x = 1\n    print(x)\n",
        "write 1.0",
    );
    rejects(
        "struct S:\n    f: float\n    fn set_one(self):\n        self.f = 1\n\nfn main():\n    s := S(2.0)\n    s.set_one()\n    print(s.f)\n",
        "write 1.0",
    );
    rejects(
        "g: float = 1.5\n\nfn set_g():\n    g = 1\n\nfn main():\n    set_g()\n    print(g)\n",
        "write 1.0",
    );
    rejects(
        "fn main():\n    c: float = 1.5\n    fn bump():\n        c = 1\n    bump()\n    print(c)\n",
        "write 1.0",
    );
    rejects(
        "fn main():\n    xs: List[float] = [1.5]\n    xs[0] = 1\n    print(xs)\n",
        "write 1.0",
    );
    rejects(
        "fn main():\n    m: Map[str, float] = {}\n    m[\"a\"] = 1\n    print(m)\n",
        "write 1.0",
    );
}

#[test]
fn reassignment_float_widen_rejects_typed_int() {
    rejects(
        "fn main():\n    x: float = 1.5\n    i := 2\n    x = i\n    print(x)\n",
        "cannot assign int to float",
    );
    rejects(
        "fn main():\n    n: int = 1\n    n = 2.5\n    print(n)\n",
        "cannot assign float to int",
    );
}

#[test]
fn d3_collection_method_float_widen_rejects_untyped_int_literal() {
    rejects(
        "fn main():\n    l: List[float] = [1.5]\n    l.insert(0, 3)\n    print(l.contains(1))\n",
        "write 1.0",
    );
}

#[test]
fn collection_method_float_widen_rejects_typed_int() {
    rejects(
        "fn main():\n    l: List[float] = [1.5]\n    i := 3\n    l.push(i)\n    print(l)\n",
        "write 1.0",
    );
}

#[test]
fn collection_method_mismatch_note_does_not_claim_an_earlier_use() {
    rejects(
        "fn main():\n    l: List[float] = [1.5]\n    l.push(\"a\")\n    print(l)\n",
        "fixed where the binding is declared",
    );
}

#[test]
fn format_spec_mismatch_against_option_float_caught_at_check_time() {
    rejects(
        "fn main():\n    o: float? = ?1.5\n    print(\"{o:.2f}\")\n",
        "format spec",
    );
}

#[test]
fn format_spec_on_container_rejected_at_check() {
    rejects(
        "fn main():\n    xs := [1]\n    print(\"{xs:d}\")\n",
        "format spec: type 'd' not valid for a string (List[int] is formatted as its text form)",
    );
    rejects(
        "fn main():\n    n: int? = None\n    print(\"{n:+}\")\n",
        "sign '+' not allowed on a string",
    );
}

#[test]
fn format_spec_on_container_width_still_accepted() {
    ok("fn main():\n    o: float? = ?1.5\n    print(\"{o:>12}\")\n");
    ok("fn show[T](v: T) -> str:\n    return \"{v:.2f}\"\n\nfn main():\n    print(show(1.5))\n");
}

const GENERIC_CTOR_WIDEN_PRELUDE: &str = "import Comparable from std.cmp\nstruct Pair[T]:\n    a: T\n    b: T\n\nstruct Box[T]:\n    v: T\n\nenum E[T]:\n    V(T, T)\n\nfn id[T](x: T) -> T:\n    return x\n\nfn mx[T: Comparable](a: T, b: T) -> T:\n    if a > b:\n        return a\n    return b\n\n";

#[test]
fn d3_generic_ctor_numeric_widen_neighbours_rejected() {
    for body in [
        "p := Pair[float](1, 2.5)\n    print(p)",
        "r: Pair[float] = Pair(1, 2)\n    print(r)",
        "e := E.V(1, 2.5)\n    print(e)",
        "f: E[float] = E.V(1, 2)\n    print(f)",
        "bf: Box[float] = Box(1)\n    print(bf)",
        "y := id[float](1)\n    print(y)",
        "z := mx[float](1, 2)\n    print(z)",
    ] {
        rejects(
            &format!("{GENERIC_CTOR_WIDEN_PRELUDE}fn main():\n    {body}\n"),
            "write 1.0",
        );
    }
    ok(&format!(
        "{GENERIC_CTOR_WIDEN_PRELUDE}fn main():\n    p := Pair[float](1.0, 2.5)\n    print(p)\n"
    ));
}

#[test]
fn generic_ctor_numeric_widen_neighbours_reject() {
    rejects(
        &format!(
            "{GENERIC_CTOR_WIDEN_PRELUDE}fn main():\n    b: Box[str] = Box(1)\n    print(b)\n"
        ),
        "cannot assign Box[int] to variable of type Box[str]",
    );
    rejects(
        &format!("{GENERIC_CTOR_WIDEN_PRELUDE}fn main():\n    print(mx(1, \"a\"))\n"),
        "argument to 'mx' has type str",
    );
    rejects(
        &format!(
            "{GENERIC_CTOR_WIDEN_PRELUDE}fn main():\n    i := 1\n    r: Pair[float] = Pair(i, 2)\n    print(r)\n"
        ),
        "cannot assign Pair[int] to variable of type Pair[float]",
    );
    rejects(
        &format!("{GENERIC_CTOR_WIDEN_PRELUDE}fn main():\n    y: float = id(1)\n    print(y)\n"),
        "cannot assign int to variable of type float",
    );
    rejects(
        &format!("{GENERIC_CTOR_WIDEN_PRELUDE}fn main():\n    print(mx[int](1, 2.5))\n"),
        "argument to 'mx' has type float, expected int",
    );
    rejects(
        &format!(
            "{GENERIC_CTOR_WIDEN_PRELUDE}enum F:\n    V(float)\n\nfn main():\n    print(F.V(1))\n"
        ),
        "argument 1 of 'V': expected float, found int",
    );
}

#[test]
fn int_never_widens_into_float_declaration_slot() {
    rejects("x: float = 1\nprint(x)\n", "write 1.0");
}

#[test]
fn int_never_widens_into_float_list_extend_slot() {
    rejects(
        "fn main():\n    l: List[float] = [1.5]\n    l.extend([3])\n",
        "write 1.0",
    );
}

#[test]
fn int_never_widens_at_every_sink() {
    let boxed = "struct Box[T]:\n    v: T\n\n";
    let lf = "    l: List[float] = [1.5]\n";
    let progs: Vec<String> = vec![
        "x: float = 1\nprint(x)\n".into(),
        "fn main():\n    x: float = 1.0\n    x = 1\n    print(x)\n".into(),
        "fn main():\n    x: float = 1.0\n    y: float = 1.0\n    x, y = 1, 2\n    print(x + y)\n"
            .into(),
        format!("{boxed}fn g() -> Box[float]: Box(1)\n"),
        format!("fn main():\n{lf}    l.push(3)\n"),
        format!("fn main():\n{lf}    l.extend([3])\n"),
        format!("fn main():\n{lf}    l += [3]\n"),
        "fn main():\n    ch := Channel[float](1)\n    ch.send(1)\n".into(),
        "fn mx[T](a: T, b: T) -> T: a\n\nfn main():\n    print(mx(1, 2.5))\n".into(),
        "fn id[T](x: T) -> T: x\n\nfn main():\n    print(id[float](1))\n".into(),
        "struct S:\n    a: float\n\nfn main():\n    print(S(1).a)\n".into(),
        "fn main():\n    m: Map[str, float] = {\"a\": 1}\n    print(m)\n".into(),
        "enum E:\n    V(float)\n\nfn main():\n    print(E.V(1))\n".into(),
        "fn main():\n    o: float? = None\n    x: float = o ?? 1\n    print(x)\n".into(),
        "import Iterator from std.iter\nfn gen() -> Iterator[float]:\n    yield 1\n".into(),
        "fn main():\n    l: List[float] = [i for i in 0..3]\n    print(l)\n".into(),
        "fn g(a: float = 3) -> float: a\n".into(),
        "struct S:\n    a: float = 3\n".into(),
        "fn main():\n    g := fn() -> float: 3\n    print(g())\n".into(),
        "fn f() -> float: 3\n".into(),
        "fn f() -> float:\n    return 3\n".into(),
        "fn main():\n    x: float? = 1\n    print(x)\n".into(),
        "fn main():\n    x := if true: 1 else: 2.5\n    print(x)\n".into(),
        "fn main():\n    x := match 1:\n        1: 2\n        _: 2.5\n    print(x)\n".into(),
        "fn main():\n    x := [1, 2.5]\n    print(x)\n".into(),
        "fn main():\n    m := {\"a\": 1, \"b\": 2.5}\n    print(m)\n".into(),
        "fn main():\n    l: List[float] = [1.5] + [2]\n    print(l)\n".into(),
        "fn main():\n    m: Map[str, float] = {\"a\": 1.5}\n    m[\"b\"] = 2\n".into(),
        format!("fn main():\n{lf}    l[0] = 2\n"),
        "struct S:\n    a: float\n\nfn main():\n    s := S(1.0)\n    s.a = 2\n".into(),
        "fn main():\n    i := 3\n    x: float = i\n    print(x)\n".into(),
        "fn main():\n    x := [1.5, 2.5].fold(0, fn(a: float, b: float) -> float: a + b)\n    print(x)\n"
            .into(),
    ];
    for src in &progs {
        let errs = check_src(src);
        assert!(
            errs.iter().any(|e| e.message.contains("write 1.0")),
            "expected a `write 1.0` error for:\n{src}\ngot: {errs:?}"
        );
    }
    // An explicit `?1` names its payload: the mismatch is reported on the `?` value.
    rejects(
        "fn main():\n    x: float? = ?1\n    print(x)\n",
        "'?' value: expected float, found int — write 1.0",
    );
    rejects(
        "fn main():\n    r: float!str = ?1\n    print(r)\n",
        "'?' value: expected float, found int — write 1.0",
    );
    // The native std types resolve only through the module graph.
    entry_rejects(
        "import std.math\n\nfn main():\n    print(math.sqrt(16))\n",
        "write 1.0",
    );
    entry_rejects(
        "import Shared from std.concurrency\n\nfn main():\n    s := Shared[float](1)\n    print(s)\n",
        "write 1.0",
    );
    entry_rejects(
        "import Atomic from std.concurrency\n\nfn main():\n    a := Atomic[float](1)\n    print(a)\n",
        "write 1.0",
    );
}

#[test]
fn int_never_widens_dot_zero_spellings_check() {
    let progs: Vec<String> = vec![
        "x: float = 1.0\nprint(x)\n".into(),
        "fn main():\n    l: List[float] = [1.5]\n    l.push(3.0)\n".into(),
        "fn mx[T](a: T, b: T) -> T: a\n\nfn main():\n    print(mx(1.0, 2.5))\n".into(),
        "fn main():\n    ch := Channel[float](1)\n    ch.send(1.0)\n".into(),
        "struct Box[T]:\n    v: T\n\nfn g() -> Box[float]: Box(1.0)\n".into(),
        "fn main():\n    x := [1.0, 2.5]\n    print(x)\n".into(),
        "fn main():\n    x: Any = if true: 1 else: 2.5\n    print(x)\n".into(),
        "fn main():\n    f := 1.5\n    f += 1\n    print(f)\n".into(),
        "fn main():\n    i := 3\n    print(i * 1.5)\n    print(2 * 1.5)\n    print(i < 3.5)\n"
            .into(),
    ];
    for src in &progs {
        ok(src);
    }
}

#[test]
fn holed_string_pattern_is_a_compile_error() {
    // W14-22: `"{x}"` in pattern position was accepted and silently never matched. The check is
    // in the PARSER now (a pattern is decoded like the same literal), so parse the source directly.
    // The malformed `{` fails in the lexer now, so both phases report as one message.
    #[derive(Debug)]
    struct Rejected {
        message: String,
    }
    fn parse_src(src: &str) -> Result<crate::ast::Module, Rejected> {
        let tokens = lexer::tokenize(src).map_err(|e| Rejected { message: e.message })?;
        parser::parse(tokens).map_err(|e| Rejected { message: e.message })
    }
    fn prog(pat: &str) -> String {
        format!(
            "fn main():\n    match \"hi\":\n        {pat}: print(\"a\")\n        _: print(\"b\")\n"
        )
    }
    match parse_src(&prog("\"{x}\"")) {
        Ok(_) => panic!("expected an error for a holed string pattern, got no errors"),
        Err(e) => assert!(
            e.message.contains("a string pattern cannot interpolate"),
            "got: {}",
            e.message
        ),
    }
    // A malformed `{` reports the same interpolation error as the expression form.
    match parse_src(&prog("\"{\"")) {
        Ok(_) => panic!("expected an error for an unterminated '{{' pattern"),
        Err(e) => assert!(e.message.contains("unterminated '{'"), "got: {}", e.message),
    }
    // Not holes: an all-digit hole is literal text (DEC-018), `{{x}}` is literal braces.
    parse_src(&prog("\"{4}\"")).expect("an all-digit hole pattern is literal text");
    parse_src(&prog("\"{{x}}\"")).expect("a doubled-brace pattern is literal braces");
}

#[test]
fn a_tuple_scrutinee_catch_all_binding_is_irrefutable() {
    // W14-36 + DEC-107: a bare name is a whole-value catch-all on a tuple scrutinee too.
    ok(
        "fn main():\n    t := (1, 2)\n    match t:\n        (0, y): print(y)\n        rest: print(rest)\nmain()\n",
    );
    // A variant name cannot bind a tuple.
    rejects(
        "fn main():\n    t := (1, 2)\n    match t:\n        (0, y): print(y)\n        None: print(0)\nmain()\n",
        "is a variant name",
    );
    // A guarded catch-all covers nothing (DEC-017/065/076).
    rejects(
        "fn main():\n    t := (1, 2)\n    match t:\n        (0, y): print(y)\n        rest if rest.0 > 0: print(rest)\nmain()\n",
        "non-exhaustive",
    );
}

// TICKET-142 (W14-19): a numeric format spec on a concrete struct is a compile error, like the containers.
#[test]
fn format_spec_on_concrete_struct_rejected_at_check() {
    rejects(
        "struct P:\n    x: int\n\nfn main():\n    p := P(1)\n    print(\"{p:d}\")\n",
        "format spec: type 'd' not valid for a string",
    );
}

// TICKET-142 (W14-32): `_ := expr` discards at module top level, any number of times, any types.
#[test]
fn blank_identifier_redeclared_at_top_level_with_different_types() {
    ok(
        "fn f() -> int?:\n    return None\n\nfn g() -> str?:\n    return None\n\n_ := f()\n_ := g()\n",
    );
}

// TICKET-142 (W14-19): every other concrete non-numeric type renders as its text form, so a numeric
// spec on it is a compile error (checked against the string rules), while an alignment spec is fine.
#[test]
fn format_spec_on_enum_fn_bytes_shared_rejected_at_check() {
    let needle = "format spec: type 'd' not valid for a string";
    rejects("enum E:\n    A\n\ne := E.A\nprint(\"{e:d}\")\n", needle);
    rejects("fn f() -> int:\n    return 1\n\nprint(\"{f:d}\")\n", needle);
    rejects("b := b\"ab\"\nprint(\"{b:d}\")\n", needle);
    entry_rejects(
        "import Shared from std.concurrency\ns := Shared[int](1)\nprint(\"{s:d}\")\n",
        needle,
    );
    ok("enum E:\n    A\n\ne := E.A\nprint(\"{e:>5}\")\n");
}

// TICKET-142 (W14-32): `_` is the blank identifier in `:=`, `=` and destructuring, at every scope.
#[test]
fn blank_identifier_discards_in_every_position() {
    let decls = "fn f() -> int?:\n    return None\n\nfn g() -> str?:\n    return None\n\n";
    ok(&format!(
        "{decls}fn main():\n    _ := f()\n    _ := g()\n    _ = f()\n    _ = g()\n    fn inner() -> int:\n        _ = g()\n        return 1\n    a, _ := (1, \"x\")\n    b, _ := (\"y\", 2)\n    print(inner() + a)\n    print(b)\n"
    ));
    ok(&format!("{decls}_ = f()\n"));
}

// TICKET-142 (W14-32): `_` is never declared, so it cannot be read; a loop variable named `_` binds.
#[test]
fn blank_identifier_is_not_readable() {
    rejects("_ := 5\nprint(_)\n", "cannot use '_' as a value");
    rejects(
        "for _ in 0..2:\n    print(_)\n",
        "cannot use '_' as a value",
    );
}

// TICKET-261: a grouped blank `(_)` is the blank at a `:=` and a wait arm; it binds nothing.
#[test]
fn grouped_blank_binds_nothing() {
    rejects("(_) := 7\nprint(_)\n", "cannot use '_' as a value");
    let wait = |target: &str| {
        format!(
            "fn f(ch: Channel[int]):\n    wait:\n        {target} := ch.recv():\n            print(_)\n"
        )
    };
    rejects(&wait("(_)"), "cannot use '_' as a value");
    rejects(&wait("_"), "cannot use '_' as a value");
}

// TICKET-261: a typed blank `_: T = e` is a `Pattern::Wildcard` target and still checks `e` against `T`.
#[test]
fn typed_blank_let_checks_its_annotation() {
    rejects(
        "_: int = \"s\"\n",
        "cannot assign str to variable of type int",
    );
    ok("_: int = 1\n_: str = \"x\"\n");
}

// TICKET-261: one declare policy for the leaves of a binding target. A nested `Unknown` leaves
// the proven leaves' types in place at every site, and a leaf still meets the const check.
#[test]
fn nested_unknown_walrus_keeps_the_proven_leaf_types() {
    rejects(
        "fn f(u):\n    a, (b, c) := (1, u)\n    print(a + \"s\", b, c)\n",
        "int",
    );
}

#[test]
fn nested_unknown_for_keeps_the_proven_leaf_types() {
    rejects(
        "fn g(u):\n    for a, (b, c) in [(1, u)]:\n        print(a + \"s\", b, c)\n",
        "int",
    );
}

#[test]
fn nested_unknown_walrus_meets_the_const_check() {
    rejects(
        "fn h(u):\n    X: const int = 1\n    X, (b, c) := (2, u)\n    print(b, c)\n",
        "cannot re-declare const binding 'X'",
    );
}

#[test]
fn nested_unknown_top_level_keeps_the_proven_leaf_types() {
    rejects(
        "fn id(x):\n    return x\nv := id(3)\na, (b, c) := (1, v)\nprint(a + \"s\", b, c)\n",
        "int",
    );
}

#[test]
fn nested_wait_arm_keeps_the_leaf_types() {
    rejects(
        "fn w(ch: Channel[(int, (int, int))]):\n    wait:\n        a, (b, c) := ch.recv():\n            print(a + \"s\", b, c)\n",
        "int",
    );
}

#[test]
fn nested_unknown_comprehension_keeps_the_proven_leaf_types() {
    rejects(
        "fn k(u):\n    print([a + \"s\" for a, (b, c) in [(1, u)]])\n",
        "int",
    );
}

// TICKET-142 (W14-33): an all-constant int expression that overflows i64 is a compile error.
#[test]
fn constant_int_overflow_rejected_at_check() {
    let needle = "does not fit in int";
    rejects("print(9223372036854775807 + 1)\n", needle);
    rejects("print(9223372036854775807 * 2)\n", needle);
    rejects("print(-(-9223372036854775807 - 1))\n", needle);
    rejects("x := 1\nprint(x + (9223372036854775807 + 1))\n", needle);
    rejects(
        "fn f(a: int) -> int:\n    return a\ny := f(9223372036854775807 * 2) + 1\n",
        needle,
    );
    rejects(
        "fn main():\n    print(9223372036854775807 + 1)\nmain()\n",
        needle,
    );
    rejects(
        "fn app[T](f: fn() -> T) -> T:\n    return f()\nprint(app(fn() -> int: 9223372036854775807 + 1))\n",
        needle,
    );
    rejects(
        "fn id[T](x: T) -> T:\n    return x\nprint(id(9223372036854775807 + 1))\n",
        needle,
    );
    // Exactly one diagnostic even where the checker infers the expression more than once.
    let errs = check_src("fn main():\n    print(9223372036854775807 + 1)\nmain()\n");
    assert_eq!(
        errs.iter().filter(|e| e.message.contains(needle)).count(),
        1,
        "got: {errs:?}"
    );
    let errs = check_src("print(1 + (9223372036854775807 + 1))\n");
    assert_eq!(
        errs.iter().filter(|e| e.message.contains(needle)).count(),
        1,
        "got: {errs:?}"
    );
    ok("print(9223372036854775807 - 1)\n");
    ok("x := 9223372036854775807\nprint(x + 1)\n");
    ok("z := 0\nprint(1 / z)\n");
}

// TICKET-142 (W14-33): the constant-overflow scan runs once per maximal arithmetic tree, so it is
// linear on a left chain. One scan of a 3000-`+` chain visits 6001 nodes; a per-node rescan visits
// about 4.5 million. Do not raise the bound: it is what tells the two apart.
#[test]
fn constant_int_overflow_scan_is_linear() {
    fn visits(src: String) -> usize {
        let tokens = lexer::tokenize(&src).expect("lex should succeed");
        let module = parser::parse(tokens).expect("parse should succeed");
        crate::on_frontend_stack_scoped(move || {
            let mut c = Checker::new();
            c.seed_native_prelude_sigs();
            c.check_module(&module.stmts, None, &[], &[]);
            c.const_scan_visits
        })
    }
    let typed = visits(format!("x := 1\ny := x{}\n", " + 1".repeat(3000)));
    assert!(typed < 7000, "non-constant chain scanned {typed} nodes");
    let constant = visits(format!("y := 1{}\n", " + 1".repeat(3000)));
    assert!(constant < 7000, "constant chain scanned {constant} nodes");
}

// TICKET-142 (W14-33): `Channel(n)` takes its element type from the annotation / param slot.
#[test]
fn channel_ctor_takes_element_type_from_annotation_and_param() {
    ok("ch: Channel[float] = Channel(1)\n");
    ok("fn f(c: Channel[int]):\n    pass\nf(Channel(1))\n");
    rejects("c := Channel(1)\n", "needs an element type");
}

// TICKET-142 (W14-33): a nested `fn`'s defaults apply at its call sites and through a value alias.
#[test]
fn nested_fn_default_applies_at_call() {
    ok(
        "fn outer() -> int:\n    fn f(x: int, y: int = 3) -> int:\n        return x + y\n    g := f\n    return f(1) + g(1, 2)\n",
    );
}

// TICKET-142 (W14-33): a nested fn's default is evaluated in MODULE scope (the compiler's prologue
// hides the frame's locals), so a default reading an enclosing local/param is a check error.
#[test]
fn nested_fn_default_cannot_read_enclosing_locals() {
    let msg = "a nested fn's default cannot read the enclosing fn's locals";
    // (a) a param
    rejects(
        "fn outer(n: int) -> int:\n    fn f(x: int = n) -> int:\n        return x\n    return f()\nprint(outer(5))\n",
        msg,
    );
    // (b) a local
    rejects(
        "fn outer() -> int:\n    m := 4\n    fn f(x: int = m + 1) -> int:\n        return x\n    return f()\nprint(outer())\n",
        msg,
    );
    // (c) a local shadowing a module global (the prologue would silently read the global)
    rejects(
        "K := 7\nfn outer() -> int:\n    K := 100\n    fn f(x: int = K) -> int:\n        return x\n    return f()\nprint(outer())\n",
        msg,
    );
    // (d) a sibling nested fn
    rejects(
        "fn outer() -> int:\n    fn one() -> int:\n        return 1\n    fn f(x: int = one()) -> int:\n        return x\n    return f()\nprint(outer())\n",
        msg,
    );
    // (e) an interpolation hole
    rejects(
        "fn outer(n: int) -> str:\n    fn f(s: str = \"n={n}\") -> str:\n        return s\n    return f()\nprint(outer(1))\n",
        msg,
    );
}

#[test]
fn nested_fn_default_reads_module_global() {
    ok(
        "K := 7\nfn outer() -> int:\n    fn f(x: int = K, y: int = 2) -> int:\n        return x + y\n    g := f\n    return f() + f(1) + g() + g(1, 1)\nprint(outer())\n",
    );
    ok(
        "fn outer() -> int:\n    fn f(xs: List[int] = [], n: int = \"ab\".len()) -> int:\n        return xs.len() + n\n    return f()\nprint(outer())\n",
    );
}

// A lambda cannot declare a default, so it keeps exact arity and today's message.
#[test]
fn lambda_call_keeps_exact_arity() {
    rejects(
        "h := fn(x: int) -> int: x\nprint(h())\n",
        "'h' expects 1 argument(s), got 0",
    );
}

// TICKET-145 (W14): diagnostics that name the wrong thing.
#[test]
fn inline_if_else_body_in_fn_is_accepted() {
    let src = "fn a(n: int) -> int: if n > 0: 1 else: 2\nfn main():\n    print(a(1))\n";
    let tokens = lexer::tokenize(src).expect("lex should succeed");
    let parsed = parser::parse(tokens);
    assert!(
        parsed.is_ok(),
        "inline if/else fn body rejected: {:?}",
        parsed.err()
    );
}

#[test]
fn double_question_type_error_does_not_leak_token_name() {
    let src = "fn main():\n    y: int?? = ?None\n";
    let tokens = lexer::tokenize(src).expect("lex should succeed");
    let msg = format!("{:?}", parser::parse(tokens).err());
    assert!(
        !msg.contains("questionquestion"),
        "token name leaked: {msg}"
    );
}

#[test]
fn question_op_in_unknown_return_fn_does_not_print_bare_question_mark_type() {
    let src = "fn p(n: int) -> int?:\n    return ?n\nfn main():\n    f := fn(n: int): p(n)? + 1\n    print(f(1))\n";
    let errs = check_src(src);
    assert!(
        !errs.iter().any(|e| e.message.contains("returns ?,")),
        "unknown type printed as '?': {errs:?}"
    );
}

#[test]
fn inline_if_without_else_in_fn_body_is_rejected() {
    let tokens = lexer::tokenize("fn a(n: int): if n > 0: 1\n").expect("lex should succeed");
    let err = parser::parse(tokens).expect_err("else-less inline if must not parse");
    assert!(
        err.message.contains("expected 'else'"),
        "wrong message: {err:?}"
    );
}

#[test]
fn inline_nested_if_statement_block_is_still_rejected() {
    let tokens = lexer::tokenize("fn main():\n    if true: if false: print(1)\n")
        .expect("lex should succeed");
    // An `if` after an inline `:` is the one-line if-expression (TICKET-260), so it needs its `else`.
    let err = parser::parse(tokens).expect_err("nested inline block must not parse");
    assert!(
        err.message.contains("expected 'else'"),
        "wrong message: {err:?}"
    );
}

#[test]
fn double_question_type_suffix_is_a_nested_optional() {
    ok("fn main():\n    y: int?? = ?None\n    print(y)\n");
}

#[test]
fn question_op_in_undeclared_return_closure_names_the_missing_return_type() {
    let src = "fn p(n: int) -> int?:\n    return ?n\nfn main():\n    f := fn(n: int): p(n)? + 1\n    print(f(1))\n";
    rejects(
        src,
        "'?' used in a function whose return type is not declared",
    );
}

/// TICKET-146 (W14-23): a tuple, `List` and `Option` are ordered lexicographically when every
/// element type is `Comparable`; the checker must accept `<` on them and `.sort()` of a tuple list.
#[test]
fn comparable_tuple_list_option_ok() {
    ok("print((1, 2) < (1, 3))\n");
    ok("print([1] < [2])\n");
    rejects("print(?1 < ?2)\n", "cannot compare");
    ok("xs := [(2, \"b\"), (1, \"z\")]\nxs.sort()\n");
    rejects("a: int? = None\nprint(a < ?0)\n", "cannot compare");
    ok("xs := [(1, \"a\")]\nxs.sort()\n");
    ok("xs := [(1, 2)]\nprint(xs.min())\n");
    ok("import Comparable from std.cmp\nfn f[T: Comparable](a: T):\n    pass\nf((1, \"a\"))\n");
    ok(
        "import Comparable from std.cmp\nfn g[T: Comparable](xs: List[T], ys: List[T]) -> bool:\n    return xs < ys\n",
    );
}

/// TICKET-146: the grant is exactly "every element type is Comparable" — a non-orderable element
/// (a plain struct, a bool) or an unordered container (`Result`, `Map`, `Set`) stays rejected.
#[test]
fn comparable_tuple_list_option_non_comparable_element_rejected() {
    let plain = "struct Plain:\n    n: int\n";
    rejects(
        &format!("{plain}print((1, Plain(1)) < (1, Plain(2)))\n"),
        "cannot compare",
    );
    rejects("print([{1: 2}] < [{1: 3}])\n", "cannot compare");
    rejects(
        "a: bool? = true\nb: bool? = false\nprint(a < b)\n",
        "cannot compare",
    );
    rejects(
        "a: int!str = 1\nb: int!str = 2\nprint(a < b)\n",
        "cannot compare",
    );
    rejects("print({1: 2} < {1: 3})\n", "cannot compare");
    rejects(
        "print(Set.from_iter([1]) < Set.from_iter([2]))\n",
        "cannot compare",
    );
    rejects(
        &format!("{plain}xs := [(1, Plain(1))]\nxs.sort()\n"),
        "does not satisfy Comparable",
    );
    rejects(
        "xs := [[{1: 2}], [{1: 3}]]\nxs.min()\n",
        "does not satisfy Comparable",
    );
}

/// A protocol VALUE whose protocol requires a `Self`-taking method (here via the `Comparable` embed)
/// erases which type it holds, so two of them must not be ordered as a tuple/List/Option element
/// either: `lt(A(1), B("x"))` would hand a `B` to `A.compare`. Base rejects the bare `a < b` too.
#[test]
fn comparable_tuple_list_option_protocol_value_element_rejected() {
    let ord2 =
        "import Comparable from std.cmp\nprotocol Ord2:\n    Comparable\n    fn tag(self) -> str\n";
    rejects(
        &format!("{ord2}fn lt(a: Ord2, b: Ord2) -> bool:\n    return (a, 0) < (b, 0)\n"),
        "cannot compare",
    );
    rejects(
        &format!("{ord2}fn lt(a: Ord2, b: Ord2) -> bool:\n    return [a] < [b]\n"),
        "cannot compare",
    );
    rejects(
        &format!("{ord2}fn s(xs: List[(Ord2, int)]):\n    xs.sort()\n"),
        "does not satisfy Comparable",
    );
    // Nested: the element check recurses through the inner tuple.
    rejects(
        &format!("{ord2}fn lt(a: Ord2, b: Ord2) -> bool:\n    return [(a, 0)] < [(b, 0)]\n"),
        "cannot compare",
    );
}
