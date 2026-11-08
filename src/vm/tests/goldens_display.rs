//! VM tests: orphaned-example goldens, struct iterators, cyclic data, interpolation faults, print
//! display hook.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

// ----- golden coverage for the formerly-orphaned examples + the comprehensive torture
// programs (edge_cases / evaluator / ledger). Each pins exact output.

/// `examples/hof.chz` — a function-typed parameter applied to a closure.
#[test]
fn golden_hof_chz_matches_expected() {
    let src = include_str!("../../../examples/hof.chz");
    let expected = include_str!("../../../examples/hof.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// `examples/list_hof.chz` — `map`/`filter`/`fold`, incl. an element-type-changing map.
#[test]
fn golden_list_hof_chz_matches_expected() {
    let src = include_str!("../../../examples/list_hof.chz");
    let expected = include_str!("../../../examples/list_hof.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// `examples/list_hof_shrink.chz` — map/filter/fold iterate a snapshot, so a callback that
/// shrinks (or grows) the receiver does not perturb iteration (and never OOB-panics). Locks
/// byte-identical against its `.expected`: before the fix the VM panicked here.
#[test]
fn golden_list_hof_shrink_chz_matches_expected() {
    let src = include_str!("../../../examples/list_hof_shrink.chz");
    let expected = include_str!("../../../examples/list_hof_shrink.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// `examples/list_methods.chz` — pop/reverse/contains/index_of/sum + value/iter ergonomics
/// (min/max/first/last/reversed/insert/remove, unique/dedup/chunk/windows/take_while/drop_while/count/position).
#[test]
fn golden_list_methods_chz_matches_expected() {
    let src = include_str!("../../../examples/list_methods.chz");
    let expected = include_str!("../../../examples/list_methods.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// `examples/loops.chz` — break/continue across for-range, for-list, and while loops.
#[test]
fn golden_loops_chz_matches_expected() {
    let src = include_str!("../../../examples/loops.chz");
    let expected = include_str!("../../../examples/loops.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// `examples/match_value.chz` — `match` on int/str literals with `_`, stmt + expr forms.
#[test]
fn golden_match_value_chz_matches_expected() {
    let src = include_str!("../../../examples/match_value.chz");
    let expected = include_str!("../../../examples/match_value.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// `examples/pair.chz` — tuples, multi-return, destructuring let, `.0`/`.1` access.
#[test]
fn golden_pair_chz_matches_expected() {
    let src = include_str!("../../../examples/pair.chz");
    let expected = include_str!("../../../examples/pair.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// `examples/method_default_args.chz` — default + named args on methods (was parity-only).
#[test]
fn golden_method_default_args_chz_matches_expected() {
    let src = include_str!("../../../examples/method_default_args.chz");
    let expected = include_str!("../../../examples/method_default_args.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// `examples/method_type_params.chz` — a method's own `[U]` inferred per call (was parity-only).
#[test]
fn golden_method_type_params_chz_matches_expected() {
    let src = include_str!("../../../examples/method_type_params.chz");
    let expected = include_str!("../../../examples/method_type_params.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// `examples/param_protocol.chz` — a user-defined parameterized protocol bound (was parity-only).
#[test]
fn golden_param_protocol_chz_matches_expected() {
    let src = include_str!("../../../examples/param_protocol.chz");
    let expected = include_str!("../../../examples/param_protocol.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// `examples/edge_cases.chz` — torture test: arithmetic faults under `recover:`, int/float
/// boundaries, empty/nested collection printing, slice clamping, index faults, truthiness,
/// block-scoped shadowing, closure capture-by-value, defer LIFO, and comprehensions.
#[test]
fn golden_edge_cases_chz_matches_expected() {
    let src = include_str!("../../../examples/edge_cases.chz");
    let expected = include_str!("../../../examples/edge_cases.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Left-shift overflow is a recoverable fault (`integer overflow in Shl`), matching the
/// `+ - * / %` checked-arith policy — not a silent wrap to `i64::MIN`. Right shift and every
/// non-overflowing left shift (incl. negative operands that round-trip) stay byte-identical.
#[test]
fn shift_left_overflow_is_recoverable_fault() {
    // overflow → recoverable fault with the shared arith-overflow message (a bound left operand: a
    // constant `1 << 63` is a compile error since TICKET-225),
    for src in [
        "a := 1\nprint(a << 63)",
        "a := 3\nprint(a << 62)",
        "a := 2\nprint(a << 62)",
    ] {
        let vm = run_capture(src).expect_err("vm: shift overflow should fault");
        assert_eq!(vm.message, "integer overflow in Shl");
    }

    // non-overflowing shifts (incl. `-1 << 63 == i64::MIN` which round-trips, `>>` which
    // never overflows) must NOT fault
    for (src, want) in [
        ("print((0 - 1) << 63)", "-9223372036854775808\n"),
        ("print(1 << 62)", "4611686018427387904\n"),
        ("print(0 << 63)", "0\n"),
        ("print(1024 >> 2)", "256\n"),
        ("print((0 - 8) >> 1)", "-4\n"),
    ] {
        let vm = run_capture(src).expect("vm: non-overflowing shift");
        assert_eq!(vm, want, "vm mismatch for `{src}`");
    }
}

/// `pad_left` with an EMPTY `fill` used to LIVELOCK (the pad loop never grew the string), producing
/// zero output and no diagnostic. It must now raise a RECOVERABLE fault EAGERLY — before the
/// `width <= len` early-out, so the fault is not input-dependent —, for both the
/// native method and the `std.string` free fn.
#[test]
fn pad_left_empty_fill_is_recoverable_fault() {
    const MSG: &str = "pad_left: fill must not be empty";
    // Native method. Both a growing width and a width the receiver already exceeds (the eager rule:
    // `width <= len` does NOT excuse an empty fill), plus the degenerate `"".pad_left(0, "")`.
    for src in [
        r#"print("a".pad_left(5, ""))"#,
        r#"print("abc".pad_left(1, ""))"#,
        r#"print("".pad_left(0, ""))"#,
    ] {
        assert_eq!(
            run_capture(src)
                .expect_err("empty fill should fault")
                .message,
            MSG,
            "mismatch for `{src}`"
        );
    }

    // (The `std.string` free fn's identical fault needs the module-graph runner — it is asserted in
    // `golden_tests::std_str_pad_left_empty_fill_faults`.)

    // The fault is CATCHABLE by `recover:` (a recoverable fault, not a host panic).
    let rec = "fn main():\n\
               \x20   r := recover:\n\
               \x20       \"a\".pad_left(5, \"\")\n\
               \x20   match r:\n\
               \x20       ?v: print(\"ok {v}\")\n\
               \x20       !e: print(\"caught {e.message()}\")\n\
               main()\n";
    let want = format!("caught {MSG}\n");
    assert_eq!(run_capture(rec).expect("recover"), want);
}

/// A multi-character `fill` is a repeating cycle TRUNCATED to fit: the result is EXACTLY `width`
/// codepoints (it used to overshoot — `"a".pad_left(4, "xy")` produced the 5-char `"xyxya"`).
#[test]
fn pad_left_multi_char_fill_is_exactly_width() {
    for (src, want) in [
        (r#"print("a".pad_left(4, "xy"))"#, "xyxa\n"),
        (r#"print("ab".pad_left(7, "xyz"))"#, "xyzxyab\n"),
        (r#"print("a".pad_left(2, "xy"))"#, "xa\n"),
        // Single-char fill (the normal path) and the never-shrinks rule are unchanged.
        (r#"print("7".pad_left(3, "0"))"#, "007\n"),
        (r#"print("12345".pad_left(3, "0"))"#, "12345\n"),
        (r#"print("a".pad_left(-5, "0"))"#, "a\n"),
        // `width = i64::MIN` (reachable from safe source) must ALSO just return `s` — the `need`
        // subtraction must not overflow (debug: host panic; release: wraps to a huge positive
        // `need` and bogusly faults `string pad capacity overflow`).
        (r#"print("ab".pad_left(-9223372036854775808, "x"))"#, "ab\n"),
        (r#"print("".pad_left(-9223372036854775808, "x"))"#, "\n"),
    ] {
        assert_eq!(run_capture(src).expect("run"), want, "mismatch for `{src}`");
    }

    // (The `std.string` free fn is a byte-identical alias — asserted in
    // `golden_tests::std_str_pad_left_matches_native_method`.)
}

/// Padding counts CODEPOINTS, not bytes — a non-ASCII fill char counts as 1 and no mid-char slice
/// can panic.
#[test]
fn pad_left_codepoints_not_bytes() {
    for (call, want) in [
        (r#""é".pad_left(3, "ü")"#, "üüé\n"),
        (r#""a".pad_left(4, "日本")"#, "日本日a\n"),
        (r#""héllo".pad_left(3, "0")"#, "héllo\n"),
    ] {
        let src = format!("print({call})");
        assert_eq!(
            run_capture(&src).expect("run"),
            want,
            "mismatch for `{call}`"
        );
    }
}

/// A huge `width` routes through the same capacity guard as `repeat`: a recoverable fault, not an
/// OOM/abort. (Native only — the `std.string` free fn cannot probe the allocator; that is a documented
/// divergence, docs/reference/stdlib/.)
#[test]
fn pad_left_huge_width_is_recoverable_fault() {
    for src in [
        r#"print("a".pad_left(9223372036854775807, "x"))"#,
        r#"print("a".pad_left(100000000000000000, "x"))"#,
    ] {
        assert_eq!(
            run_capture(src).expect_err("huge pad should fault").message,
            "string pad capacity overflow",
            "mismatch for `{src}`"
        );
    }
}

/// `str.repeat(n)` with a huge `n` must raise a RECOVERABLE fault (not hard-panic the process via
/// Rust's `str::repeat` capacity-overflow abort),, matching the repo's
/// checked-overflow policy. Normal repeats still work and stay parity-equal.
#[test]
fn str_repeat_capacity_overflow_is_recoverable_fault() {
    // `expect_err` (not a process abort) proves the panic was converted to a fault.
    let src = r#"print("ab".repeat(9223372036854775807))"#;
    let vm = run_capture(src).expect_err("vm: repeat overflow should fault");
    assert_eq!(vm.message, "string repeat capacity overflow");

    // Huge-but-representable count: passes the `isize::MAX` byte guard but the allocation is
    // infeasible. Before the `try_reserve_exact` guard this ABORTED the process (SIGABRT).
    let huge = r#"print("ab".repeat(100000000000000000))"#;
    let vm = run_capture(huge).expect_err("vm: huge repeat should fault");
    assert_eq!(vm.message, "string repeat capacity overflow");

    // Empty receiver with a huge count: `total == 0` passes BOTH the byte guard and
    // `try_reserve_exact`, so the result must be "" produced INSTANTLY. A naive
    // `for _ in 0..n { out.push_str("") }` fill would loop ~1e17 times (an uncatchable hang);
    // `str::repeat` short-circuits the empty receiver. The `expect`/value assertion only
    // returns if no hang occurred.
    let empty_huge = r#"print("".repeat(100000000000000000))"#;
    assert_eq!(run_capture(empty_huge).expect("vm"), "\n");

    // Sane repeats are unaffected.
    let ok = r#"print("ab".repeat(3))"#;
    assert_eq!(run_capture(ok).expect("vm"), "ababab\n");
}

/// Collection operators (gap #3): list `+` (concat) / `*` (repeat) and set `| & - ^`
/// (union/intersection/difference/symmetric-difference). Value-correctness on the VM; the
/// golden test below pins the printed form. Set print preserves insertion order (mine-then-
/// other for union; mine-filtered for intersection/difference; mine-not-in-other then
/// other-not-in-mine for symmetric-difference).
#[test]
fn collection_operators_eval_correct() {
    // list concat
    assert_eq!(
        run_capture("print([1, 2] + [3, 4])").expect("vm"),
        "[1, 2, 3, 4]\n"
    );
    // empty-side concat keeps element type / values
    assert_eq!(run_capture("print([] + [1, 2])").expect("vm"), "[1, 2]\n");
    assert_eq!(run_capture("print([1, 2] + [])").expect("vm"), "[1, 2]\n");
    // list repeat is a method (`*` means multiply only)
    assert_eq!(
        run_capture("print([0].repeat(3))").expect("vm"),
        "[0, 0, 0]\n"
    );
    // zero / negative repeat → empty
    assert_eq!(run_capture("print([1].repeat(0))").expect("vm"), "[]\n");
    assert_eq!(run_capture("print([1].repeat(-2))").expect("vm"), "[]\n");
    // set algebra (insertion-order preserved)
    let setops = "a: Set[int] = {1, 2, 3}\nb: Set[int] = {2, 3, 4}\nprint(\"{a | b}\")\nprint(\"{a & b}\")\nprint(\"{a - b}\")\nprint(\"{a ^ b}\")\n";
    assert_eq!(
        run_capture(setops).expect("vm"),
        "{1, 2, 3, 4}\n{2, 3}\n{1}\n{1, 4}\n"
    );
}

/// Compound-assign forms of the collection operators must produce identical values to the binary
/// forms. Every compound form lowers through its plain binary opcode (owner rule U15).
#[test]
fn collection_compound_assign_eval_correct() {
    let cases = [
        ("xs := [1, 2]\nxs += [3, 4]\nprint(xs)", "[1, 2, 3, 4]\n"),
        (
            "a: Set[int] = {1, 2, 3}\nb: Set[int] = {2, 3, 4}\na |= b\nprint(\"{a}\")",
            "{1, 2, 3, 4}\n",
        ),
        (
            "a: Set[int] = {1, 2, 3}\nb: Set[int] = {2, 3, 4}\na &= b\nprint(\"{a}\")",
            "{2, 3}\n",
        ),
        (
            "a: Set[int] = {1, 2, 3}\nb: Set[int] = {2, 3, 4}\na -= b\nprint(\"{a}\")",
            "{1}\n",
        ),
        (
            "a: Set[int] = {1, 2, 3}\nb: Set[int] = {2, 3, 4}\na ^= b\nprint(\"{a}\")",
            "{1, 4}\n",
        ),
    ];
    for (src, want) in cases {
        assert_eq!(
            run_capture(src).expect("vm"),
            want,
            "vm mismatch for `{src}`"
        );
    }
}

/// `[0].repeat(n)` with a huge `n` must raise a RECOVERABLE fault, not abort the process via a Vec
/// capacity-overflow panic — same checked-overflow policy as `str.repeat`,.
#[test]
fn list_repeat_capacity_overflow_is_recoverable_fault() {
    let src = "print([0].repeat(9223372036854775807))";
    let vm = run_capture(src).expect_err("vm: list repeat overflow should fault");
    assert_eq!(vm.message, "list repeat capacity overflow");

    // Huge-but-representable count: passes the `isize::MAX` byte guard (1e17 * 16B = 1.6e18 <
    // isize::MAX ≈ 9.2e18) but the allocation itself is infeasible. Before the
    // `try_reserve_exact` guard this ABORTED the process (SIGABRT) instead of faulting.
    let huge = "print([0].repeat(100000000000000000))";
    let vm = run_capture(huge).expect_err("vm: huge list repeat should fault");
    assert_eq!(vm.message, "list repeat capacity overflow");

    // Boundary: small repeats still work.
    assert_eq!(
        run_capture("print([0].repeat(3))").expect("vm"),
        "[0, 0, 0]\n"
    );
}

/// `examples/evaluator.chz` — a full tokenizer + recursive-descent parser + AST evaluator with
/// `Result`/`?` error paths (bad char, unbalanced parens, trailing input, divide-by-zero).
#[test]
fn golden_evaluator_chz_matches_expected() {
    let src = include_str!("../../../examples/evaluator.chz");
    let expected = include_str!("../../../examples/evaluator.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// `examples/ledger.chz` — account ledger: a map of mutable structs, overdraft `Result`s, a
/// `defer` closing line, `sort_by` ranking, and guarded comprehensions.
#[test]
fn golden_ledger_chz_matches_expected() {
    let src = include_str!("../../../examples/ledger.chz");
    let expected = include_str!("../../../examples/ledger.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// M1 (tier-1) golden: `examples/string_iter.chz` (chars + iterable strings) byte-identical
/// against its `.expected`.
#[test]
fn golden_string_iter_chz_matches_expected() {
    let src = include_str!("../../../examples/string_iter.chz");
    let expected = include_str!("../../../examples/string_iter.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Default + named arguments on free functions: `examples/default_args.chz` byte-identical against
/// its `.expected`.
#[test]
fn golden_default_args_chz_matches_expected() {
    let src = include_str!("../../../examples/default_args.chz");
    let expected = include_str!("../../../examples/default_args.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Default + named arguments on struct constructors: `examples/named_struct.chz` byte-identical
/// against its `.expected`.
#[test]
fn golden_named_struct_chz_matches_expected() {
    let src = include_str!("../../../examples/named_struct.chz");
    let expected = include_str!("../../../examples/named_struct.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Gap #5 golden: `examples/map.chz` is byte-identical to its `.expected`.
#[test]
fn golden_map_chz_matches_expected() {
    let src = include_str!("../../../examples/map.chz");
    let expected = include_str!("../../../examples/map.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// M10-G1 golden: `examples/stringable.chz` (the `Stringable` protocol — `str(self)` dispatch
/// from print/str()/interpolation, nested too) byte-identical against its `.expected`.
#[test]
fn golden_stringable_chz_matches_expected() {
    let src = include_str!("../../../examples/stringable.chz");
    let expected = include_str!("../../../examples/stringable.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// M10-G3 golden: `examples/operators.chz` (operator overloading via `Add`/`Sub`/`Mul` + the
/// multi-bound `T: Add + Mul`) byte-identical against its `.expected`.
#[test]
fn golden_operators_chz_matches_expected() {
    let src = include_str!("../../../examples/operators.chz");
    let expected = include_str!("../../../examples/operators.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// M10-G3 golden: `examples/type_alias.chz` (transparent type aliases) byte-identical against its
/// `.expected`.
#[test]
fn golden_type_alias_chz_matches_expected() {
    let src = include_str!("../../../examples/type_alias.chz");
    let expected = include_str!("../../../examples/type_alias.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// G1 golden: `examples/generics.chz` (generics + structural `Comparable`) is byte-identical
/// against its `.expected`.
#[test]
fn golden_generics_chz_matches_expected() {
    let src = include_str!("../../../examples/generics.chz");
    let expected = include_str!("../../../examples/generics.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// G2 golden: generic structs are byte-identical against its `.expected`.
#[test]
fn golden_generic_structs_chz_matches_expected() {
    let src = include_str!("../../../examples/generic_structs.chz");
    let expected = include_str!("../../../examples/generic_structs.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Tier-2 golden: generic enums (Tree[T] / Either[A, B]) — byte-identical against `.expected`.
#[test]
fn golden_generic_enum_chz_matches_expected() {
    let src = include_str!("../../../examples/generic_enum.chz");
    let expected = include_str!("../../../examples/generic_enum.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Golden: real hash-table map/set with Hashable struct keys — byte-identical against
/// `.expected`.
#[test]
fn golden_hashmap_keys_chz_matches_expected() {
    let src = include_str!("../../../examples/hashmap_keys.chz");
    let expected = include_str!("../../../examples/hashmap_keys.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Tech-debt golden: `examples/explicit_type_args.chz` (explicit call-site type arguments on a
/// generic fn / struct / enum-variant constructor) byte-identical against `.expected`.
#[test]
fn golden_explicit_type_args_chz_matches_expected() {
    let src = include_str!("../../../examples/explicit_type_args.chz");
    let expected = include_str!("../../../examples/explicit_type_args.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Type-side declaration-site turbofish (PART 1): `examples/turbofish_type_args.chz` exercises
/// `Box[int].Has(5)`, nullary `Box[int].Empty`, the 2-param `Pair[int, str].Both(…)` multi-arg
/// carrier, and a generic static `Box[int].empty()`. The change is purely in the checker's
/// resolution + the value's inferred type args (runtime is type-erased), so the output
/// must be byte-identical to `.expected`. Also asserts it type-checks clean.
#[test]
fn golden_turbofish_type_args_chz_matches_expected() {
    let src = include_str!("../../../examples/turbofish_type_args.chz");
    let expected = include_str!("../../../examples/turbofish_type_args.expected");
    // The full type-checker accepts the program (the golden run path skips checking).
    let module = parser::parse(lexer::tokenize(src).expect("lex")).expect("parse");
    assert!(
        crate::checker::check(&module).is_ok(),
        "turbofish_type_args.chz must type-check clean"
    );
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected, "vm output drifted from .expected");
}

/// Member-side declaration-site turbofish (PART 2): `examples/turbofish_member_args.chz`
/// exercises a generic static method's OWN `[U]` inferred (`Box[int].make(5)`) AND via the
/// combined turbofish (`Box[int].make[str]("hi")` + the bare carrier `Box.make[str]`), an
/// instance method with multi type-arg + multi value-arg turbofish (`p.first[int, str](…)`),
/// AND the regression guard `arr[i].handlers[k](x)` / `arr[0].handlers[0](y)` (a fn-valued field
/// on an INDEXED receiver must stay ordinary index-then-call, not a member-turbofish). The change
/// is checker resolution + type-erased dispatch, so the output must be
/// byte-identical to `.expected`. Also asserts it type-checks clean.
#[test]
fn golden_turbofish_member_args_chz_matches_expected() {
    let src = include_str!("../../../examples/turbofish_member_args.chz");
    let expected = include_str!("../../../examples/turbofish_member_args.expected");
    let module = parser::parse(lexer::tokenize(src).expect("lex")).expect("parse");
    assert!(
        crate::checker::check(&module).is_ok(),
        "turbofish_member_args.chz must type-check clean"
    );
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected, "vm output drifted from .expected");
}

/// Tech-debt golden: `examples/set_eq.chz` (order-independent set equality incl. nested in a
/// struct/list) byte-identical against its `.expected`.
#[test]
fn golden_set_eq_chz_matches_expected() {
    let src = include_str!("../../../examples/set_eq.chz");
    let expected = include_str!("../../../examples/set_eq.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Golden: `examples/map_eq.chz` — map equality is order-independent (same key→value pairs
/// regardless of insertion order), incl. nested in a struct/list, byte-identical against its
/// `.expected`. Pins the fix that made map `==` consistent with set `==`.
#[test]
fn golden_map_eq_chz_matches_expected() {
    let src = include_str!("../../../examples/map_eq.chz");
    let expected = include_str!("../../../examples/map_eq.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Golden: `examples/cycle_guard.chz` — a cyclic data structure makes `print`/`==` a recoverable
/// `RuntimeError` (depth-guarded) instead of an uncatchable host stack overflow, and a
/// deep-but-acyclic structure still renders fine. Byte-identical against its `.expected`.
#[test]
fn golden_cycle_guard_chz_matches_expected() {
    let src = include_str!("../../../examples/cycle_guard.chz");
    let expected = include_str!("../../../examples/cycle_guard.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// A SELF-REFERENTIAL value crossing a task boundary (`spawn` arg) reaches the task by handle; no
/// walk of the cycle, no depth cap. The spawned task reads `a.val == 1` (prints `got 1`) and the
/// recover returns `done`.
#[test]
fn crossing_cyclic_struct_crosses() {
    let src = "\
struct Node:
    val: int
    next: List[Node]
fn use_it(n: Node):
    print(\"got {n.val}\")
fn main():
    a := Node(1, [])
    b := Node(2, [])
    a.next.push(b)
    b.next.push(a)
    r := recover:
        parallel:
            spawn use_it(a)
    match r:
        ?_:  print(\"ok: done\")
        !e: print(\"caught: {e.message()}\")
main()
";
    assert_mc_parity(src, "got 1\nok: done\n");
}

/// The same self-referential value crossing via `Channel.send` and `Shared(...)` (by handle) also
/// gets through. Regression lock on the other crossing entry points beyond a bare `spawn` arg.
#[test]
fn crossing_cyclic_via_channel_send_and_shared_crosses() {
    let chan_src = "\
import std.concurrency
struct Node:
    val: int
    next: List[Node]
fn main():
    a := Node(1, [])
    b := Node(2, [])
    a.next.push(b)
    b.next.push(a)
    r := recover:
        ch := Channel[Node]()
        ch.send(a)
    match r:
        ?_:  print(\"ok: done\")
        !e: print(\"caught: {e.message()}\")
main()
";
    assert_mc_parity(chan_src, "ok: done\n");
    let shared_src = "\
import Shared from std.concurrency
struct Node:
    val: int
    next: List[Node]
fn main():
    a := Node(1, [])
    b := Node(2, [])
    a.next.push(b)
    b.next.push(a)
    r := recover:
        s := Shared(a)
    match r:
        ?_:  print(\"ok: done\")
        !e: print(\"caught: {e.message()}\")
main()
";
    assert_mc_parity(shared_src, "ok: done\n");
}

/// A nested (local) recursive `fn` crossing a spawn has a self-referential capture graph (the
/// letrec self-cell: `Closure h -> captured[Cell] -> Cell.inner = h`). The task gets the
/// closure by handle (M3.4; the deleted wire form back-referenced the knot), so the recursive fn
/// computes correctly — `fact(5) == 120`.
#[test]
fn crossing_recursive_local_fn_round_trips() {
    let src = "\
fn main():
    fn fact(n: int) -> int:
        if n <= 1: return 1
        return n * fact(n - 1)
    ch := Channel[int]()
    parallel:
        spawn: ch.send(fact(5))
    print(\"ok: {ch.recv()}\")
main()
";
    assert_mc_parity(src, "ok: 120\n");
}

/// Memory-safety lock (it once guarded the deleted copy step's tie-the-knot rebuild): the same
/// recursive-fn crossing under GC STRESS. The closure/cell cycle must stay GC-traced — a dangling
/// `GcRef` would panic at the next safepoint ("dangling GcRef"). A GC pass runs before/after the spawn, then the result is used (`fact(6) == 720`).
#[test]
fn crossing_recursive_local_fn_round_trips_under_gc_stress() {
    let src = "\
fn main():
    fn fact(n: int) -> int:
        if n <= 1: return 1
        return n * fact(n - 1)
    junk := [1, 2, 3, 4, 5]
    ch := Channel[int]()
    parallel:
        spawn: ch.send(fact(6))
    got := ch.recv()
    more := [got, got]
    print(\"ok: {got}\")
main()
";
    assert_eq!(run_capture_stress(src), "ok: 720\n");
    assert_eq!(run_capture_stress(src), run(src));
}

/// Memory-safety lock (item A; it once guarded the deleted copy step's CONTAINER rebuild): a
/// self-referential `struct` crossing a spawn under GC STRESS. The struct/list cycle must stay
/// GC-traced — a dangling `GcRef` would panic at the next safepoint ("dangling GcRef"). A GC pass runs before/after the
/// spawn, then the crossed cyclic node is read (`a.val == 1`, and the cycle is intact: `a.next[0]` is
/// `b` and `b.next[0]` is `a`). Mirrors `crossing_recursive_local_fn_round_trips_under_gc_stress`.
#[test]
fn crossing_self_ref_struct_round_trips_under_gc_stress() {
    let src = "\
struct Node:
    val: int
    next: List[Node]
fn use_it(n: Node):
    print(\"ok: {n.val} {n.next[0].val} {n.next[0].next[0].val}\")
fn main():
    junk := [1, 2, 3, 4, 5]
    a := Node(1, [])
    b := Node(2, [])
    a.next.push(b)
    b.next.push(a)
    ch := Channel[int]()
    parallel:
        spawn use_it(a)
    more := [a, b]
main()
";
    assert_eq!(run_capture_stress(src), "ok: 1 2 1\n");
    assert_eq!(run_capture_stress(src), run(src));
}

/// W7-11 — an `RwShared` copy-out view of an element whose cycle closes through the ROOT container
/// used to ABORT THE HOST: the piece rebuild hit `from_wire_memo`'s
/// `.expect("a wire Backref always targets an already-reconstructed node id")` on a legal,
/// single-threaded, checker-clean program, while `get()` on the same box worked. No store-side
/// re-emission can cover it: the missing node is the ROOT container itself.
///
/// **This test's failure mode is a dead process, not a red assert** — which is exactly why it is in
/// Rust and not only in `tests/chz/`: a regression takes libtest down with it.
///
/// The expected output is CPython's, measured on the same shape:
/// ```text
/// b = copy.deepcopy(xs[0]); b.val = 42; b.next[0].val            -> 42
/// b.next[0].next[0].next[0].val                                  -> 42
/// ```
#[test]
fn rwshared_view_of_a_container_cycling_element_does_not_abort_the_host() {
    let src = "\
import RwShared from std.concurrency
import ConcurrentList, ConcurrentMap, ConcurrentSet from std.concurrency.collection
struct Node:
    val: int
    back: List[Node]
fn main():
    a := Node(1, [])
    xs := [a, Node(2, [])]
    a.back = xs
    s := ConcurrentList(RwShared(xs))
    print(\"get {s.inner.get()[0].val}\")
    match s.at(0):
        ?e:
            print(\"walk {e.back[0].back[0].back[0].val}\")
            e.val = 42
            print(\"identity {e.back[0].val} {e.back[0].back[0].back[0].val}\")
        None: print(\"WRONG: at(0) was None\")
    match s.at(9):
        ?v: print(\"WRONG: at(9) was ?{v.val}\")
        None: print(\"oob None\")
main()
";
    assert_eq!(
        super::golden_tests::golden_entry(src),
        "get 1\nwalk 1\nidentity 42 42\noob None\n"
    );
}

/// W7-11 under GC STRESS — the fallback rebuilds the WHOLE container and hands back one node out of
/// it, so the returned element's own cycle is the only thing keeping the rest reachable. If that
/// rooting were wrong, a collection between the rebuild and the caller's use would surface here
/// (dangling `GcRef` panic or a wrong value), not in the plain run above.
#[test]
fn rwshared_cyclic_view_round_trips_under_gc_stress() {
    let src = "\
import RwShared from std.concurrency
import ConcurrentList, ConcurrentMap, ConcurrentSet from std.concurrency.collection
struct Node:
    val: int
    back: List[Node]
fn main():
    junk := [1, 2, 3, 4, 5]
    a := Node(1, [])
    xs := [a, Node(2, [])]
    a.back = xs
    s := ConcurrentList(RwShared(xs))
    match s.at(0):
        ?e: print(\"ok: {e.val} {e.back[0].back[0].val} {e.back.len()}\")
        None: print(\"WRONG\")
    more := [a]
main()
";
    assert_eq!(
        super::golden_tests::vm_run_file_stress(src, crate::native::HostConfig::default()),
        "ok: 1 1 2\n"
    );
    assert_eq!(
        super::golden_tests::vm_run_file_stress(src, crate::native::HostConfig::default()),
        super::golden_tests::golden_entry(src)
    );
}

/// W7-4 REVIEW (perf cliff, regression lock) — an `RwShared` read VIEW must stay O(element), never
/// O(whole container). The first cut resolved a piece's cross-element `Backref` by rebuilding the
/// ENTIRE stored container once PER ELEMENT, so `for_each`/`fold`/`at` over a container of closures
/// sharing one binding went quadratic (measured on the pre-fix release binary: n=4000 → 3.7s, n=12000
/// → 34s, versus 0.02s before W7-4). M3.3: a view reads the live container by handle, so it rebuilds nothing
/// (M3.5 deleted the wire node counter with the wire form; this pins the result).
#[test]
fn rwshared_view_over_shared_bindings_is_not_quadratic() {
    let src = "\
import RwShared from std.concurrency
import ConcurrentList, ConcurrentMap, ConcurrentSet from std.concurrency.collection
fn main():
    n := 0
    fn inc() -> int:
        n = n + 1
        return n
    fs: List[fn() -> int] = [inc]
    for i in 0..3000:
        fs.push(inc)
    s := ConcurrentList(RwShared(fs))
    c := 0
    fn tick(f: fn() -> int):
        c = c + 1
    s.for_each(tick)
    print(c)
main()
";
    let out = super::golden_tests::golden_entry(src);
    assert_eq!(out, "3001\n");
}

/// TICKET-154 (W11-15) -- the CONTAINER twin of the cell test above. One `inner` list pushed three
/// thousand times into the stored list is an aliased store, so every element after the first is a
/// bare back-ref. M3.3: the walk reads the live container by handle.
#[test]
fn rwshared_for_each_over_a_dag_alias_is_not_quadratic() {
    let src = "\
import RwShared from std.concurrency
import ConcurrentList, ConcurrentMap, ConcurrentSet from std.concurrency.collection
fn main():
    inner := [1]
    fs: List[List[int]] = [inner]
    for i in 0..3000:
        fs.push(inner)
    s := ConcurrentList(RwShared(fs))
    c := 0
    fn tick(x: List[int]):
        c = c + 1
    s.for_each(tick)
    print(c)
main()
";
    let out = super::golden_tests::golden_entry(src);
    assert_eq!(out, "3001\n");
}

/// TICKET-154 (W11-15) -- the single-piece cost on an aliased store. Before M3.3 `at(1)` rebuilt the
/// whole root once per call. M3.3: `at` returns the live element by handle.
#[test]
fn rwshared_at_over_an_aliased_store_stays_under_its_ceiling() {
    let src = "\
import RwShared from std.concurrency
import ConcurrentList, ConcurrentMap, ConcurrentSet from std.concurrency.collection
fn main():
    inner := List.from_iter(0..5000)
    outer: List[List[int]] = [inner, inner]
    for k in 0..8:
        outer.push(List.from_iter(0..5000))
    s := ConcurrentList(RwShared(outer))
    total := 0
    for k in 0..5:
        match s.at(1):
            ?v: total = total + v.len()
            None: total = total + 0
    print(total)
main()
";
    let out = super::golden_tests::golden_entry(src);
    assert_eq!(out, "25000\n");
}

/// W13-9 / TICKET-119 — a module global that is a 5000-deep struct chain. A spawn used to snapshot
/// it and fault at the depth cap after an O(depth^2) walk. M3.1: a spawn copies no module global, so
/// the task runs.
#[test]
fn a_spawn_beside_a_deep_module_global_copies_nothing() {
    let src = "\
struct N:
    next: N?
fn mk() -> N:
    head := N(None)
    cur := head
    for i in 0..5000:
        n := N(None)
        cur.next = ?n
        cur = n
    return head
gl: N = mk()
fn main():
    r := recover:
        parallel:
            spawn:
                print(\"crossed\")
        0
    match r:
        ?v: print(\"ok\")
        !e: print(\"err: {e.message()}\")
main()
";
    let out = run_capture(src);
    let out = out.unwrap_or_else(|e| panic!("unexpected runtime error: {e}"));
    assert_eq!(out, "crossed\nok\n");
}

/// W13-9 / TICKET-119 — a module global that is a 5000-deep chain with a payload at every level. A
/// spawn used to snapshot it
/// and fault at the depth cap after an O(depth^2) walk. M3.1: a spawn copies no module global, so
/// the task runs.
#[test]
fn a_spawn_beside_a_deep_payload_module_global_copies_nothing() {
    let src = "\
struct N:
    tag: List[int]
    next: N?
fn mk() -> N:
    head := N([1], None)
    cur := head
    for i in 0..5000:
        n := N([i], None)
        cur.next = ?n
        cur = n
    return head
gl: N = mk()
fn main():
    r := recover:
        parallel:
            spawn:
                print(\"crossed\")
        0
    match r:
        ?v: print(\"ok\")
        !e: print(\"err: {e.message()}\")
main()
";
    let out = run_capture(src);
    let out = out.unwrap_or_else(|e| panic!("unexpected runtime error: {e}"));
    assert_eq!(out, "crossed\nok\n");
}

/// W13-9 / TICKET-119 — a module global that is 6000 nested closures. A spawn used to snapshot it
/// and fault at the depth cap after an O(depth^2) walk. M3.1: a spawn copies no module global, so
/// the task runs.
#[test]
fn a_spawn_beside_a_deep_closure_module_global_copies_nothing() {
    let src = "\
fn mk(k: int) -> fn() -> int:
    f := fn() -> int: 0
    for i in 0..k:
        inner := f
        f = fn() -> int: inner() + 1
    return f
gl: fn() -> int = mk(6000)
fn main():
    r := recover:
        parallel:
            spawn:
                print(\"crossed\")
        0
    match r:
        ?v: print(\"ok\")
        !e: print(\"err: {e.message()}\")
main()
";
    let out = run_capture(src);
    let out = out.unwrap_or_else(|e| panic!("unexpected runtime error: {e}"));
    assert_eq!(out, "crossed\nok\n");
}

/// TICKET-119 review finding -- the first doomed-skip design (`(node, shallowest depth)` alone,
/// dropped) is unsound: a KEPT sibling speculative attempt mints `m`, a node inside a doomed walk. A
/// later deep reach of `h` skips its own attempt, so the slow arms never Backref `m`, and the walk
/// runs past the cap. This program crosses on base (`crossed`/`ok`); the unsound skip faulted it with
/// the depth-exceeded error instead. `holder.x` reaches `h`'s 1000th descendant `m` directly at depth
/// 1 and mints it; `holder.y` then reaches `h` itself at a much deeper depth, doomed by the
/// generator's earlier depth-tripped attempt. `make` holds a generator in a reference cycle (`g`'s
/// pending argument `c` holds `g`). Every speculative attempt that reaches `g` fails, so each node of
/// `holder.y` takes a slow arm. Before TICKET-276 the program held a module value (`p := [k]`), which
/// `chezzi check` rejects.
#[test]
fn crossing_doomed_skip_keeps_a_sibling_backref_and_the_program_crosses() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_t119_review_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    let src = "import Iterator from std.iter\nstruct N:\n    next: N?\nstruct Last:\n    count: fn() -> int\n    tail: N\nstruct P:\n    next: P?\n    last: Last?\nstruct Holder:\n    gen: Iterator[int]\n    x: N\n    y: P\nstruct C:\n    g: Iterator[int]?\nfn spin(c: C) -> Iterator[int]:\n    yield 1\nfn make() -> fn() -> int:\n    c := C(None)\n    g := spin(c)\n    c.g = ?g\n    p := [g]\n    fn count() -> int:\n        return p.len()\n    return count\nfn mk(k: int) -> N:\n    head := N(None)\n    cur := head\n    for i in 0..k:\n        n := N(None)\n        cur.next = ?n\n        cur = n\n    return head\nfn chain_to(k: int, tail: N) -> N:\n    head := N(None)\n    cur := head\n    for i in 0..k:\n        n := N(None)\n        cur.next = ?n\n        cur = n\n    cur.next = ?tail\n    return head\nfn nth(h: N, i: int) -> N:\n    cur := h\n    for j in 0..i:\n        match cur.next:\n            ?n: cur = n\n            None: return cur\n    return cur\nfn drive(deep: N) -> Iterator[int]:\n    yield 1\n    match deep.next:\n        ?n: yield 2\n        None: yield 0\nfn build() -> Holder:\n    h := mk(3000)\n    y := P(None, ?Last(make(), h))\n    for i in 0..2600:\n        y = P(?y, None)\n    return Holder(drive(chain_to(2500, h)), nth(h, 1000), y)\nholder: Holder = build()\nstarted := holder.gen.next()\nfn main():\n    r := recover:\n        parallel:\n            spawn:\n                print(\"crossed\")\n        0\n    match r:\n        ?v: print(\"ok\")\n        !e: print(\"err: {e.message()}\")\nmain()\n";
    std::fs::write(&entry, src).unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    let checked = crate::checker::check_graph(&graph);
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    if let Err(errs) = checked {
        panic!("the program must type-check: {errs:?}");
    }
    assert!(vm_res.is_ok(), "unexpected top-level fault: {vm_res:?}");
    assert_eq!(
        vm_out, "crossed\nok\n",
        "a doomed-attempt skip lost a sibling's Backref and faulted a program that must cross"
    );
}

/// TICKET-119 review finding -- the same unsound design also fails on a cycle: the recording attempt
/// Backrefs `z` because `z` is on its `path`, but `w`'s own attempt lacks that state, re-enters `z` by
/// a shorter route and is kept. Skipping `w` loops the slow arms.
#[test]
fn crossing_cyclic_module_global_with_a_shorter_reentry_still_crosses() {
    let src = "\
struct G:
    a: G?
    b: G?
fn build() -> G:
    r := G(None, None)
    z := G(None, None)
    w := G(None, None)
    h := G(None, None)
    y := G(None, None)
    cur := y
    for i in 0..4950:
        n := G(None, None)
        cur.a = ?n
        cur = n
    r.a = ?z
    z.a = ?w
    z.b = ?y
    w.a = ?h
    h.a = ?z
    a1 := G(None, None)
    h.b = ?a1
    p := a1
    for i in 0..50:
        q := G(None, None)
        p.a = ?q
        p = q
    p.a = ?y
    return r
gl: G = build()
fn main():
    r := recover:
        parallel:
            spawn:
                print(\"crossed\")
        0
    match r:
        ?v: print(\"ok\")
        !e: print(\"err: {e.message()}\")
main()
";
    assert_eq!(run(src), "crossed\nok\n");
}

/// TICKET-119 — `holder.a` and `holder.b` alias the SAME `N` chain `h`, beside a parked generator
/// over a deeper chain to it. M3.1: the task reads the run's one `holder`, so the push through
/// `a` is what `b` reads. Go (go1.27.0, `m31go/rust`): `2`.
#[test]
fn two_fields_of_a_module_global_over_one_chain_are_one_object() {
    let src = "\
struct N:
    tag: List[int]
    next: N?
struct Holder:
    gen: Iterator[int]
    a: N
    b: N
fn mk(k: int) -> N:
    head := N([1], None)
    cur := head
    for i in 0..k:
        n := N([i], None)
        cur.next = ?n
        cur = n
    return head
fn chain_to(k: int, tail: N) -> N:
    head := N([0], None)
    cur := head
    for i in 0..k:
        n := N([i], None)
        cur.next = ?n
        cur = n
    cur.next = ?tail
    return head
fn drive(deep: N) -> Iterator[int]:
    yield 1
    yield deep.tag.len()
fn build() -> Holder:
    h := mk(3000)
    return Holder(drive(chain_to(2500, h)), h, h)
holder: Holder = build()
started := holder.gen.next()
fn main():
    parallel:
        spawn:
            holder.a.tag.push(9)
            print(\"{holder.b.tag.len()}\")
main()
";
    assert_eq!(run(src), "2\n");
}

/// TICKET-119 -- a module global just under `MAX_STRUCTURAL_DEPTH` must still cross without ever
/// hitting the depth-tripped recording path at all.
#[test]
fn crossing_module_global_just_under_the_depth_cap_still_crosses() {
    let src = "\
struct N:
    next: N?
fn mk() -> N:
    head := N(None)
    cur := head
    for i in 0..4500:
        n := N(None)
        cur.next = ?n
        cur = n
    return head
gl: N = mk()
fn main():
    r := recover:
        parallel:
            spawn:
                print(\"crossed\")
        0
    match r:
        ?v: print(\"ok\")
        !e: print(\"err: {e.message()}\")
main()
";
    assert_eq!(run(src), "crossed\nok\n");
}

/// TICKET-119 / DEC-100 identity lock: a module global aliased through two `List[int]` bindings is
/// one object (M3.1: the run's), beside a deep module global. Go (go1.27.0, `m31go/rust`): `2
/// true`.
#[test]
fn crossing_deep_module_global_alias_is_one_object() {
    let src = "\
struct N:
    next: N?
fn mk(k: int) -> N:
    head := N(None)
    cur := head
    for i in 0..k:
        n := N(None)
        cur.next = ?n
        cur = n
    return head
shared: List[int] = [1]
a: List[int] = shared
b: List[int] = shared
gl: N = mk(4000)
fn main():
    parallel:
        spawn:
            a.push(2)
            print(\"{b.len()} {gl.next != None}\")
main()
";
    assert_eq!(run(src), "2 true\n");
}

/// W7-4 memory-safety lock: two module globals over one captured local are one binding, and a task
/// that writes through one and reads through the other runs under GC STRESS with junk allocation
/// around it. M3.1: one cell for the run. Go (go1.27.0, `m31go/rust`): `2`.
#[test]
fn crossing_module_global_shared_binding_survives_gc_stress() {
    let src = "\
struct Ctr:
    inc: fn() -> None
    get: fn() -> int
fn make() -> Ctr:
    n := 0
    fn inc():
        n = n + 1
    fn get() -> int:
        return n
    return Ctr(inc, get)
c := make()
gi := c.inc
gg := c.get
pad := [1, 2, 3, 4, 5]
fn main():
    junk := [1, 2, 3]
    r := Channel[int]()
    parallel:
        spawn:
            gi()
            gi()
            r.send(gg())
    more := [junk, junk]
    print(\"ok: {r.recv()}\")
main()
";
    assert_eq!(run_capture_stress(src), "ok: 2\n");
    assert_eq!(run_capture_stress(src), run(src));
}

/// W7-4a — cell identity across MODULES. `k.C` holds two sibling closures over one factory-local
/// `n`; `l.GI` and `main.GG` are globals in two DIFFERENT modules pointing at them. M3.1: the task
/// calls the run's one closure pair, so its two `l.GI()` writes reach `GG()`. CPython and Go
/// (go1.27.0, `m31go/rust`): `2`.
#[test]
fn crossing_cross_module_shared_binding_is_one_cell() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_w74a_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("k.chz"),
        "\
struct Ctr:
    inc: fn() -> None
    get: fn() -> int
fn make() -> Ctr:
    n := 0
    fn inc():
        n = n + 1
    fn get() -> int:
        return n
    return Ctr(inc, get)
C := make()
",
    )
    .unwrap();
    std::fs::write(dir.join("l.chz"), "import k\nGI := k.C.inc\n").unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "\
import k
import l
GG := k.C.get
fn main():
    r := Channel[int]()
    parallel:
        spawn:
            l.GI()
            l.GI()
            r.send(GG())
    print(r.recv())
main()
",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    // Under GC stress the cell is reached only through the module globals of `k` and `l`.
    let (stress_out, _se, stress_res, _) = run_file_stress(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vm_res.is_ok(), "faulted: {vm_res:?}");
    assert!(stress_res.is_ok(), "gc-stress faulted: {stress_res:?}");
    assert_eq!(
        vm_out, "2\n",
        "cross-module sibling closures split their cell"
    );
    assert_eq!(stress_out, "2\n", "a module global's cell is not GC-rooted");
}

/// W7-4b — a module global whose cell holds a module handle (`p := [k]`). M3.1: the task calls the
/// run's one closure pair, so `GC()` reads both `GA()` writes. CPython: `3`; Go (go1.27.0,
/// `m31go/rust`): `3`.
#[test]
fn crossing_handle_bearing_cell_keeps_one_binding() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_w74b_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("k.chz"), "V := 41\n").unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "\
import k
struct Sw:
    add: fn() -> None
    count: fn() -> int
fn make() -> Sw:
    p := [k]
    fn add():
        p = p + [k]
    fn count() -> int:
        return p.len()
    return Sw(add, count)
C := make()
GA := C.add
GC := C.count
fn main():
    r := Channel[int]()
    parallel:
        spawn:
            GA()
            GA()
            r.send(GC())
    print(r.recv())
main()
",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (stress_out, _se, stress_res, _) = run_file_stress(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vm_res.is_ok(), "faulted: {vm_res:?}");
    assert!(stress_res.is_ok(), "gc-stress faulted: {stress_res:?}");
    assert_eq!(vm_out, "3\n", "a handle-bearing cell split its binding");
    assert_eq!(stress_out, "3\n", "…and under GC stress");
}

/// W7-4b's second, unplanned fix — a RECURSIVE local `fn` whose captures embed a module used to
/// abort the whole spawn with `maximum structural depth (10000) exceeded (cyclic data structure?)`.
/// The self-cell cycle reached the module snapshot's slow arm when the closure ALSO held a handle,
/// and that arm had no `Backref`, so the walk ran the cycle to the shared depth cap and "rejected
/// cleanly" — a fault on a program CPython runs fine (`m = bk` + the same recursive `down`,
/// printing `41`). M3.1 deleted the snapshot: the task reads the run's one global.
#[test]
fn crossing_handle_bearing_recursive_local_fn_round_trips() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_w74b_cyc_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("k.chz"), "V := 41\n").unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "\
import k
fn make() -> fn(int) -> int:
    m := k
    fn down(n: int) -> int:
        if n <= 0:
            return m.V
        return down(n - 1)
    return down
G := make()
fn main():
    r := Channel[int]()
    parallel:
        spawn:
            r.send(G(3))
    print(r.recv())
main()
",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vm_res.is_ok(), "faulted (depth cap is back?): {vm_res:?}");
    assert_eq!(vm_out, "41\n");
}

/// W7-4c — a task's captured `gg` (a crossing copy of the binding as of its spawn) and the module
/// global `I` that wrote the binding before that spawn agree on its VALUE: CPython measures `1`.
/// M3.1: the global is the run's one closure, and the capture is copied after `I()` ran.
#[test]
fn crossing_shared_cell_takes_the_spawn_time_value() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_w74c_val_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "\
struct Ctr:
    inc: fn() -> None
    get: fn() -> int
fn make() -> Ctr:
    n := 0
    fn inc():
        n = n + 1
    fn get() -> int:
        return n
    return Ctr(inc, get)
C := make()
I := C.inc
fn go():
    gg := C.get
    r := Channel[int](1)
    parallel:
        spawn:
            pass
        I()
        spawn:
            r.send(gg())
    print(r.recv())
go()
",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vm_res.is_ok(), "faulted: {vm_res:?}");
    assert_eq!(vm_out, "1\n", "the task kept a STALE cell value");
}

/// W7-4a — a DISCARDED speculative wire attempt once forged a `Backref` (wire form deleted, M3.5). Found by adversarial
/// review, and it was a live host PANIC, not a theoretical one: `main.chz` below aborted the task
/// with `CellLoad on a non-handle value`.
///
/// Mechanism: `H := (k, k.C.get)` embeds a module, so the tuple fails `to_snap`'s `!has_handle()`
/// fast lane — but the attempt ALREADY marked `k.C.get`'s cell as emitted, and that cell's id was
/// minted back when module `k` was walked. `try_wire_speculative`'s rollback pruned `emitted` by
/// `id >= mint_from`, which cannot see an id from an earlier module, so the marking survived a walk
/// whose output was thrown away. `GG := k.C.get` (the next global) then emitted `Backref(id)` into a
/// module that never wrote the definition. Ordering matters: the handle-bearing global must come
/// FIRST — move `H` below `GG` and it prints `7` either way.
#[test]
fn crossing_discarded_wire_attempt_does_not_forge_a_backref() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_w74a_spec_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("k.chz"),
        "\
struct Ctr:
    inc: fn() -> None
    get: fn() -> int
fn make() -> Ctr:
    n := 7
    fn inc():
        n = n + 1
    fn get() -> int:
        return n
    return Ctr(inc, get)
C := make()
",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "\
import k
H := (k, k.C.get)
GG := k.C.get
fn main():
    r := Channel[int]()
    parallel:
        spawn:
            r.send(GG())
    print(r.recv())
main()
",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "faulted — a discarded attempt forged a Backref: {vm_res:?}"
    );
    assert_eq!(vm_out, "7\n");
}

/// Control (regression lock, rc=0) — the SAME recursive `fn` HOISTED to module scope IS sendable: it
/// crosses as `Obj::Func` (no captures; recursion resolves via its home-global slot), never entering
/// the `Obj::Closure` serialization arm, so the new self-ref diagnostic never fires and the send works.
#[test]
fn crossing_module_global_recursive_fn_control_sends() {
    let src = "\
fn fact(n: int) -> int:
    if n <= 1: return 1
    return n * fact(n - 1)
fn main():
    ch := Channel[int]()
    parallel:
        spawn: ch.send(fact(5))
    print(ch.recv())
main()
";
    assert_mc_parity(src, "120\n");
}

/// A task reads a cyclic MODULE-GLOBAL value: the task reads the run's one global, so nothing walks
/// the cycle; the worker prints
/// `w`, then the recover returns `done`.
#[test]
fn crossing_cyclic_module_global_crosses_mn() {
    let src = "\
struct Node:
    val: int
    next: List[Node]
a := Node(1, [])
b := Node(2, [])
a.next.push(b)
b.next.push(a)
fn worker():
    print(\"w\")
fn main():
    r := recover:
        parallel:
            spawn worker()
    match r:
        ?_:  print(\"ok: done\")
        !e: print(\"caught: {e.message()}\")
main()
";
    let out = run_capture(src).expect("M:N run should not crash");
    assert_eq!(out, "w\nok: done\n");
}

/// Bug A guard-precision — a BREADTH-shaped acyclic value (~100k shallow siblings, structural depth
/// ~2) must cross a spawn fine (the deleted copy step's 10_000 depth cap counted nesting LEVELS, not
/// siblings), so a large-but-shallow sendable never false-trips. (A >10_000-DEEP acyclic nest errored
/// there — the false-positive the print/`==` path has — so this test is breadth-shaped.)
#[test]
fn crossing_wide_acyclic_crosses_fine() {
    let src = "\
fn use_it(xs: List[int]):
    print(\"len {xs.len()} first {xs[0]} last {xs[99999]}\")
fn main():
    xs := [0]
    for i in 1..100000:
        xs.push(i)
    parallel:
        spawn use_it(xs)
main()
";
    assert_mc_parity(src, "len 100000 first 0 last 99999\n");
}

/// Golden: `examples/crossing_cycle.chz`: a SELF-REFERENTIAL value reaches a task (`spawn` arg /
/// `Channel.send` / `Shared`) by handle (M3.4), and so does a wide acyclic list.
#[test]
fn golden_crossing_cycle_chz_matches_expected() {
    let src = include_str!("../../../examples/crossing_cycle.chz");
    let expected = include_str!("../../../examples/crossing_cycle.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Tech-debt: a `set` nested inside a struct / list must compare unordered
/// (top-level set `==` already did). A derived, order-sensitive `SetData::eq` would print `false`
/// for `W(Set([1,2,3])) == W(Set([3,2,1]))`.
#[test]
fn nested_set_equality_parity() {
    let src = "\
struct W:
    s: Set[int]
a := W(Set.from_iter([1, 2, 3]))
b := W(Set.from_iter([3, 2, 1]))
print(a == b)
print([Set.from_iter([1, 2])] == [Set.from_iter([2, 1])])
";
    let vm = run_capture(src).expect("vm");
    assert_eq!(vm, "true\ntrue\n");
}

#[test]
fn sort_over_comparable_structs_on_vm() {
    let src = "\
struct P:
    n: int
    t: str
    fn compare(self, other: P) -> int:
        return self.n - other.n
    fn eq(self, other: P) -> bool:
        return self.n == other.n
    fn show(self) -> str:
        return self.t + \"{self.n}\"
xs := [P(3, \"c\"), P(1, \"a\"), P(2, \"b\"), P(1, \"z\")]
xs.sort()
for x in xs:
    print(x.show())
";
    assert_eq!(run(src), "a1\nz1\nb2\nc3\n");
}

#[test]
fn struct_ordering_dispatches_to_compare_on_vm() {
    let src = "\
struct P:
    n: int
    fn compare(self, other: P) -> int:
        return self.n - other.n
    fn eq(self, other: P) -> bool:
        return self.n == other.n
print(P(1) < P(2))
print(P(2) < P(1))
print(P(5) >= P(5))
";
    assert_eq!(run(src), "true\nfalse\ntrue\n");
}

/// `Contains` operator protocol (L5): `x in some_struct` dispatches to a user
/// `contains(self, item) -> bool` method. A checker-only change would `check` OK
/// then trap at runtime in `op_contains`'s reject arm — so this RUN test is the safety net.
#[test]
fn contains_protocol_struct_dispatches() {
    let src = "\
struct Bag:
    items: List[int]
    fn contains(self, item: int) -> bool:
        for it in self.items:
            if it == item:
                return true
        return false
b := Bag([1, 2, 3])
print(2 in b)
print(9 in b)
";
    assert_mc_parity(src, "true\nfalse\n");
}

/// `Contains` on a generic `Box[T]` — the `contains` param type must be the INSTANTIATED `int`,
/// and the generic instantiation must lower to the method call.
#[test]
fn contains_generic_box_runs() {
    let src = "\
struct Box[T]:
    v: T
    fn contains(self, item: T) -> bool:
        return item == self.v
b := Box[int](2)
print(2 in b)
print(3 in b)
";
    assert_mc_parity(src, "true\nfalse\n");
}

/// `Contains` also dispatches on enums (protocol-satisfaction machinery already covers them).
#[test]
fn contains_protocol_enum_dispatches() {
    let src = "\
enum Dir:
    N
    S
    fn contains(self, item: int) -> bool:
        return item == 0
d := Dir.N
print(0 in d)
print(1 in d)
";
    assert_mc_parity(src, "true\nfalse\n");
}

/// `in` resolves through a `Contains[T]` generic bound — the exact analog of `<` through a
/// `Comparable` bound. `contains_item_ty` recovers the item type from the bound; at runtime the
/// value is a concrete monomorphized struct/enum, so it dispatches via `op_contains`.
#[test]
fn contains_through_generic_bound_runs() {
    let src = "\
struct Bag:
    xs: List[int]
    fn contains(self, item: int) -> bool:
        for e in self.xs:
            if e == item:
                return true
        return false
fn has[C: Contains[int]](c: C, n: int) -> bool:
    return n in c
print(has(Bag([4, 5, 6]), 5))
print(has(Bag([4, 5, 6]), 9))
";
    assert_mc_parity(src, "true\nfalse\n");
}

#[test]
fn primitive_compare_method_on_vm() {
    let src = "fn c[T: Comparable](a: T, b: T) -> int:\n    return a.compare(b)\nprint(c(2, 5))\nprint(c(5, 2))\n";
    assert_eq!(run(src), "-1\n1\n");
}

/// Scalars intrinsically satisfy `Stringable` (checker arm in `proto.rs`) AND the erased generic
/// body's `v.str()` is dispatched by the scalar `str` branch in `do_method_call` — a checker-only
/// change would type-OK then runtime-trap "type int has no method 'str'". Covers all four scalars
/// (int/float/bool/str; the T=str arm exercises the already-`Obj::Str` receiver re-alloc).
/// Byte-identical (both share `do_method_call`).
#[test]
fn primitive_str_method_on_vm() {
    let src = "fn show[T: Stringable](v: T) -> str:\n    return v.str()\nprint(show(5))\nprint(show(3.14))\nprint(show(true))\nprint(show(\"hi\"))\n";
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, "5\n3.14\ntrue\nhi\n");
}

/// Gap #11 golden: `examples/sort_by.chz` (custom comparators, stable order, tuple-field sort)
/// is byte-identical on the VM against its `.expected`.
#[test]
fn golden_sort_by_chz_matches_expected() {
    let src = include_str!("../../../examples/sort_by.chz");
    let expected = include_str!("../../../examples/sort_by.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Call-flattening guarantee: deep *plain-function* recursion no longer consumes host Rust stack
/// (frames live in the heap `frames` `Vec`, not via a per-call `run_until` recursion), so it runs
/// to completion on a stack far below the production 256 MiB `VM_STACK_BYTES`. Before flattening,
/// the VM recursed ~25 KiB of host stack per call and overflowed a 1 MiB stack (an uncatchable
/// abort). Depth stays well under `MAX_CALL_DEPTH` (10_000).
#[test]
fn deep_plain_recursion_runs_on_small_host_stack() {
    let src = "\
fn sum_to(n: int) -> int:
    if n <= 0:
        return 0
    return n + sum_to(n - 1)

print(sum_to(5000))
";
    let out = super::run_capture_on_stack(src, 1024 * 1024)
        .expect("deep plain recursion should run on a 1 MiB host stack after call-flattening");
    assert_eq!(out, "12502500\n");
}

/// M19 — guards the `run_until` per-entry program borrow (the hoisted `Arc::clone` →
/// raw-pointer) across the native-reentry paths that re-enter `run_until`: HOF callbacks
/// (`map`/`fold` closures), an operator-overload `compare` (`<` on a struct), and a
/// `defer` unwinding through a recursive call. If the raw pointer ever dangled across a
/// re-entry or resume, the output would drift from the literal golden.
#[test]
fn native_reentry_hof_compare_defer_parity() {
    let src = "\
struct P:
    v: int
    fn compare(self, other: P) -> int:
        return self.v - other.v
    fn eq(self, other: P) -> bool:
        return self.v == other.v

fn leave(n: int):
    print(\"leave {n}\")

fn rec(n: int) -> int:
    defer leave(n)
    if n <= 0:
        return 0
    doubled := [1, 2, 3].map(fn(x: int) -> int: x * n)
    s := doubled.fold(0, fn(a: int, x: int) -> int: a + x)
    if P(n) < P(n + 1):
        s = s + rec(n - 1)
    return s

print(rec(3))
";
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, "leave 0\nleave 1\nleave 2\nleave 3\n36\n");
}

/// Gap #10 golden: `examples/cipher.chz` (ord/chr — ROT13 + manual digit parsing) is
/// byte-identical against its `.expected`.
#[test]
fn golden_cipher_chz_matches_expected() {
    let src = include_str!("../../../examples/cipher.chz");
    let expected = include_str!("../../../examples/cipher.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Gap #14 (+ #11) golden: `examples/word_freq.chz` iterates a map with `for w, c in counts`
/// and ranks tuples with `sort_by`. Byte-identical against its `.expected`.
#[test]
fn golden_word_freq_chz_matches_expected() {
    let src = include_str!("../../../examples/word_freq.chz");
    let expected = include_str!("../../../examples/word_freq.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Gap #15 golden: `examples/match_nested.chz` (tuple patterns, nested `Some((a, b))`, nested
/// literals) is byte-identical against its `.expected`.
#[test]
fn golden_match_nested_chz_matches_expected() {
    let src = include_str!("../../../examples/match_nested.chz");
    let expected = include_str!("../../../examples/match_nested.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// L2 golden: `examples/match_struct.chz` (struct positional destructuring in `match` — single-arm
/// exhaustive, literal fields + a whole-value catch-all binding, a generic `Box[int]` field, and a
/// nested `Line(Point(..), Point(..))`) is byte-identical against its `.expected`.
#[test]
fn golden_match_struct_chz_matches_expected_and_parity() {
    let src = include_str!("../../../examples/match_struct.chz");
    let expected = include_str!("../../../examples/match_struct.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Match-guard golden: `examples/match_guard.chz` (`pattern if cond:` arms, expr + stmt forms)
/// is byte-identical against its `.expected`.
#[test]
fn golden_match_guard_chz_matches_expected() {
    let src = include_str!("../../../examples/match_guard.chz");
    let expected = include_str!("../../../examples/match_guard.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Range-pattern golden: `examples/match_range.chz` (half-open `start..end` int patterns) is
/// byte-identical against its `.expected`.
#[test]
fn golden_match_range_chz_matches_expected() {
    let src = include_str!("../../../examples/match_range.chz");
    let expected = include_str!("../../../examples/match_range.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Gap #13 golden: `examples/bits.chz` (`& | ^ << >>` — XOR-fold + bitmask) is byte-identical
/// against its `.expected`.
#[test]
fn golden_bits_chz_matches_expected() {
    let src = include_str!("../../../examples/bits.chz");
    let expected = include_str!("../../../examples/bits.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Round-2 probe goldens: recursive data-structure + evaluator programs that surfaced the
/// round-2 gaps. Byte-identical against a literal golden, and their `.expected`.
#[test]
fn golden_bst_chz_matches_expected() {
    let src = include_str!("../../../examples/bst.chz");
    let expected = include_str!("../../../examples/bst.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

#[test]
fn golden_linked_list_chz_matches_expected() {
    let src = include_str!("../../../examples/linked_list.chz");
    let expected = include_str!("../../../examples/linked_list.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

#[test]
fn golden_calc_chz_matches_expected() {
    let src = include_str!("../../../examples/calc.chz");
    let expected = include_str!("../../../examples/calc.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

// ----- struct iterator protocol (`for x in s` driven by `next(self) -> Option[T]`) -----

#[test]
fn for_over_struct_iterator_counts() {
    let src = "struct Counter:\n    n: int\n    limit: int\n    fn next(self) -> int?:\n        if self.n >= self.limit:\n            return None\n        v := self.n\n        self.n = self.n + 1\n        return ?v\nfn main():\n    for x in Counter(0, 5):\n        print(x)\nmain()\n";
    assert_eq!(run(src), "0\n1\n2\n3\n4\n");
}

#[test]
fn for_over_struct_iterator_break_lazy() {
    let src = "struct Fib:\n    a: int\n    b: int\n    fn next(self) -> int?:\n        v := self.a\n        nb := self.a + self.b\n        self.a = self.b\n        self.b = nb\n        return ?v\nfn main():\n    for x in Fib(0, 1):\n        if x > 10:\n            break\n        print(x)\nmain()\n";
    assert_eq!(run(src), "0\n1\n1\n2\n3\n5\n8\n");
}

/// Golden: the iterator example runs on the VM with exactly the expected output.
#[test]
fn golden_iterator_chz_matches_expected() {
    let src = include_str!("../../../examples/iterator.chz");
    let expected = include_str!("../../../examples/iterator.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

// ----- cyclic-data structural-depth guard + order-independent map == -----

#[test]
fn cyclic_print_renders_the_back_edge() {
    // W14-35c: CPython's dataclass repr of the same cycle is `Node(next=[Node(next=[...])])`.
    let src = "\
struct Node:
    next: List[Node]
a := Node([])
b := Node([])
a.next.push(b)
b.next.push(a)
print(a)
";
    assert_eq!(run(src), "Node(next=[Node(next=[...])])\n");
}

#[test]
fn cyclic_equality_errors_not_crashes() {
    let src = "\
struct Node:
    next: List[Node]
a := Node([])
b := Node([])
a.next.push(b)
b.next.push(a)
c := Node([])
d := Node([])
c.next.push(d)
d.next.push(c)
print(a == c)
";
    assert!(
        run_err(src).contains("maximum structural depth"),
        "expected structural-depth error"
    );
}

#[test]
fn cyclic_print_inside_recover_succeeds() {
    let src = "\
struct Node:
    next: List[Node]
a := Node([])
b := Node([])
a.next.push(b)
b.next.push(a)
r := recover:
    print(a)
match r:
    ?v: print(\"ok\")
    !e: print(\"caught: {e.message()}\")
";
    assert_eq!(run(src), "Node(next=[Node(next=[...])])\nok\n");
}

#[test]
fn deep_acyclic_structure_ok() {
    let src = "\
x := [0]
i := 0
while i < 100:
    x = [x]
    i = i + 1
y := [0]
j := 0
while j < 100:
    y = [y]
    j = j + 1
print(x == y)
";
    assert_eq!(run(src), "true\n");
}

// ---- Bug E: runtime faults inside string interpolation report the real source line ----

/// Assert a runtime fault reports `line == expected_line`.
fn assert_fault_line(src: &str, needle: &str, expected_line: u32) {
    let e = run_capture(src).unwrap_err();
    assert!(
        e.message.contains(needle),
        "expected message to contain {needle:?}, got {:?}",
        e.message
    );
    assert_eq!(
        e.span.line, expected_line,
        "expected line {expected_line}, got {} (col {})",
        e.span.line, e.span.col
    );
}

/// Like [`assert_fault_line`] but pins the COLUMN too — the axis M24-6 was about. A binary
/// expression's span is its LEFT OPERAND's first char (`cc := 10 / b` faults at the `1`), so the
/// expected column is hand-counted to there.
fn assert_fault_at(src: &str, needle: &str, expected: (u32, u32)) {
    assert_fault_line(src, needle, expected.0);
    let e = run_capture(src).unwrap_err();
    assert_eq!((e.span.line, e.span.col), expected);
}

#[test]
fn interpolation_div_by_zero_reports_real_line() {
    let src =
        "print(\"line 1\")\nprint(\"line 2\")\nb := 0\nmsg := \"result = {10 / b}\"\nprint(msg)\n";
    assert_fault_line(src, "division by zero", 4);
}

#[test]
fn interpolation_index_oob_reports_real_line() {
    let src = "print(\"line 1\")\nprint(\"line 2\")\nxs := [1, 2, 3]\nmsg := \"val = {xs[9]}\"\nprint(msg)\n";
    assert_fault_line(src, "index", 4);
}

#[test]
fn interpolation_overflow_reports_real_line() {
    let src = "print(\"line 1\")\nprint(\"line 2\")\nbig := 9223372036854775807\nmsg := \"val = {big * big}\"\nprint(msg)\n";
    assert_fault_line(src, "overflow", 4);
}

#[test]
fn interpolation_multiline_reports_the_fragments_real_line() {
    // The triple-quoted string OPENS on line 4, but the faulting fragment physically sits on line
    // 6 — and that is what gets reported, column included. This used to report the opening line
    // (4) and a column that kept counting from the literal's start, because `raw` is post-escape
    // and a `\n` escape was indistinguishable from a real newline. The lexer now carries a
    // content-index → source-position map, so the two ARE distinguishable (`docs/contributing/gaps.md` M24-6).
    //   line 6:  result = {10 / b}
    //   cols:    1234567890
    // `result = ` is 9 chars, `{` is col 10, so the `10` starts at col 11.
    let src = "print(\"line 1\")\nprint(\"line 2\")\nb := 0\nmsg := \"\"\"\nsome text\nresult = {10 / b}\n\"\"\"\nprint(msg)\n";
    assert_fault_at(src, "division by zero", (6, 11));
}

#[test]
fn interpolation_escape_newline_before_fragment_not_misattributed() {
    // A `\n` ESCAPE is NOT a source newline, so it must not shift the reported line — and now that
    // is proved rather than approximated: the map records where each content char physically is, so
    // the escape costs the fragment two columns (its two source chars) and zero lines.
    //   line 4:  msg := "a\nb {10 / b}"
    //   cols:    12345678901234
    // `"`=8, `a`=9, `\`=10, `n`=11, `b`=12, ` `=13, `{`=14, so the `10` starts at col 15.
    let src =
        "print(\"line 1\")\nprint(\"line 2\")\nb := 0\nmsg := \"a\\nb {10 / b}\"\nprint(msg)\n";
    assert_fault_at(src, "division by zero", (4, 15));
}

#[test]
fn interpolation_backstop_still_faults_for_generic_str() {
    // Static format-spec/value-type checking fires only for CONCRETE scalars; a generic body
    // `"{v:.2f}"` (v: T) passes check, so instantiating it with a str MUST still fault at RUNTIME
    // — proving the runtime backstop is intact.
    let src = "fn show[T](v: T) -> str:\n    return \"{v:.2f}\"\nfn main():\n    print(show(\"hi\"))\nmain()\n";
    let e = run_capture(src).unwrap_err().message;
    assert!(e.contains("type 'f' not valid for a string"), "{e}");
    // A valid concrete-float `{x:.2f}` still renders.
    let ok = "x: float = 3.14159\nprint(\"{x:.2f}\")\n";
    assert_eq!(run_capture(ok).unwrap(), "3.14\n");
}

#[test]
fn non_interpolation_fault_span_unchanged() {
    // A direct top-level fault (no interpolation) still reports its real position — proves
    // `origin: None` leaves normal lexing byte-identical. `c := 10 / b`: `c`=1 … `1`=6.
    let src = "print(\"line 1\")\nprint(\"line 2\")\nb := 0\nc := 10 / b\nprint(c)\n";
    assert_fault_at(src, "division by zero", (4, 6));
    // A literal-only string + a valid interpolation still runs with correct output.
    let ok = "x := 5\nprint(\"x = {x}, done\")\n";
    assert_eq!(run_capture(ok).unwrap(), "x = 5, done\n");
    assert_eq!(run_capture(ok).unwrap(), "x = 5, done\n");
}

// ---- Bug B: print's `str(self)` display hook is used only when it CONFORMS to Stringable ----
// (returns a `str`) — checked on the RETURNED VALUE, so an annotated / inferred / aliased str all
// work, while a non-`str` return falls back to the default repr instead of recursing forever
// (was an uncatchable stack-overflow SIGABRT on a check-accepted program).

#[test]
fn str_hook_nonstr_return_struct_falls_back_to_default_repr() {
    // The Bug B repro: `str(self) -> S` returns the struct itself. Pre-fix this recursed in the
    // native stringifier → SIGABRT (the test binary would ABORT). Now it falls back to `S(n=5)`.
    let src = "struct S:\n    n: int\n    fn str(self) -> S:\n        return self\nfn main(): print(S(5))\nmain()\n";
    assert_mc_parity(src, "S(n=5)\n");
}

#[test]
fn str_hook_nonstr_return_enum_falls_back() {
    let enum_src = "enum E:\n    A(int)\n    fn str(self) -> E:\n        return self\nfn main(): print(E.A(5))\nmain()\n";
    assert_mc_parity(enum_src, "A(5)\n");
}

#[test]
fn str_hook_used_when_it_returns_str_annotated_inferred_or_aliased() {
    // Annotated `-> str`.
    let annotated = "struct S:\n    n: int\n    fn str(self) -> str:\n        return \"A{self.n}\"\nfn main(): print(S(5))\nmain()\n";
    assert_mc_parity(annotated, "A5\n");
    // Inferred str (un-annotated) — a syntactic `-> str` gate would wrongly drop this; the
    // returned-value check keeps it working.
    let inferred = "struct S:\n    n: int\n    fn str(self) -> str:\n        return \"custom<{self.n}>\"\nfn main(): print(S(5))\nmain()\n";
    assert_mc_parity(inferred, "custom<5>\n");
    // A str type-alias return also conforms.
    let aliased = "type MyStr = str\nstruct S:\n    n: int\n    fn str(self) -> MyStr:\n        return \"hi{self.n}\"\nfn main(): print(S(5))\nmain()\n";
    assert_mc_parity(aliased, "hi5\n");
    // Same gate applies inside string interpolation.
    let interp = "struct S:\n    n: int\n    fn str(self) -> S:\n        return self\nfn main(): print(\"v={S(5)}\")\nmain()\n";
    assert_mc_parity(interp, "v=S(n=5)\n");
}

#[test]
fn str_hook_direct_call_returning_nonstr_still_works() {
    // `str` stays a normal user method: a direct `s.str()` returning the struct is unaffected by the
    // display-hook gate (no checker rejection, no runtime restriction).
    let src = "struct S:\n    n: int\n    fn str(self) -> S:\n        return self\nfn main():\n    x := S(5).str()\n    print(x.n)\nmain()\n";
    assert_mc_parity(src, "5\n");
}

#[test]
fn str_hook_nonstr_fallback_is_gc_safe_after_mutating_hook() {
    // TICKET-250 — a `str(self)` that does not return `str` is not the `Stringable` hook, so
    // `print` never runs it: the default repr shows the fields as they are, and the method's
    // side effects (the `tag` write) do not happen.
    let src = "struct S:\n    n: int\n    tag: List[int]\n    fn str(self) -> S:\n        self.tag = [9, 9, 9]\n        i := 0\n        acc := [0]\n        while i < 100000:\n            acc = [i]\n            i = i + 1\n        return self\nfn main(): print(S(5, [1, 2, 3]))\nmain()\n";
    assert_mc_parity(src, "S(n=5, tag=[1, 2, 3])\n");
}

// A bare same-module generic fn pinned through a builtin closure-result HOF slot (`.map`/`.fold`)
// must RUN correctly (runtime is generic-erased; the checker fix is what unblocks it).

#[test]
fn generic_fn_value_map_conv_runs() {
    // `.map(conv)` — conv's own T pinned = int from the element type; yields the string list.
    let src = "fn conv[T](x: T) -> str:\n    return \"{x}\"\nprint([1, 2, 3].map(conv))\n";
    assert_mc_parity(src, "['1', '2', '3']\n");
}

#[test]
fn generic_fn_value_map_ident_runs() {
    // `.map(ident)` — return-only T pinned = int; yields the int list unchanged.
    let src = "fn ident[T](x: T) -> T:\n    return x\nprint([1, 2, 3].map(ident))\n";
    assert_mc_parity(src, "[1, 2, 3]\n");
}

#[test]
fn generic_fn_value_fold_add_runs() {
    // `.fold(0, add)` — accumulator U pinned by init=int, then add's T pinned from fn(int,int)->int.
    let src = "fn add[T: Add](a: T, b: T) -> T:\n    return a + b\nprint([1, 2, 3].fold(0, add))\n";
    assert_mc_parity(src, "6\n");
}

#[test]
fn generic_fn_value_map_closure_still_runs() {
    // Regression: the unannotated-closure loop-back is untouched — still [2, 4, 6].
    let src = "print([1, 2, 3].map(fn(x): x * 2))\n";
    assert_mc_parity(src, "[2, 4, 6]\n");
}

#[test]
fn generic_fn_value_filter_keep_runs() {
    // Regression: a bare generic keep[T] into .filter still runs (concrete fn(int)->bool slot).
    let src = "fn keep[T](x: T) -> bool:\n    return true\nprint([1, 2, 3].filter(keep))\n";
    assert_mc_parity(src, "[1, 2, 3]\n");
}

#[test]
fn generic_fn_value_user_hof_and_turbofish_run() {
    // Regression: the pre-existing user-HOF pin and the turbofish workaround still run unchanged.
    let src = "fn conv[T](x: T) -> str:\n    return \"{x}\"\nfn mymap(xs: List[int], f: fn(int) -> str) -> List[str]:\n    return xs.map(f)\nprint(mymap([1, 2, 3], conv))\nprint([1, 2, 3].map(conv[int]))\n";
    assert_mc_parity(src, "['1', '2', '3']\n['1', '2', '3']\n");
}

// ── Parameterized protocols in value position (Q1) — goldens ──────────────────────

#[test]
fn param_protocol_value_arg_runs() {
    // A `Container[int]` parameter accepts a conforming struct; `c.get(0)` dispatches by name and
    // yields the recovered `int`.
    let src = "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    items: List[int]\n    fn get(self, i: int) -> int:\n        return self.items[i]\nfn f(c: Container[int]) -> int:\n    return c.get(0) + 1\nfn main():\n    print(f(Bag([41])))\nmain()\n";
    assert_mc_parity(src, "42\n");
}

#[test]
fn param_protocol_method_return_recovered_runs() {
    // DECISION-2 recovery: `x := c.get(0)` is typed `int` and used in int arithmetic; identical
    // stdout.
    let src = "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    fn get(self, i: int) -> int:\n        return 7\nfn f(c: Container[int]) -> int:\n    x := c.get(0)\n    return x * 6\nfn main():\n    print(f(Bag()))\nmain()\n";
    assert_mc_parity(src, "42\n");
}

#[test]
fn param_protocol_field_and_nesting_run() {
    // A `Container[int]` struct field and a `List[Container[int]]` param both witness + erase; the
    // stored values dispatch by name identically.
    let src = "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    n: int\n    fn get(self, i: int) -> int:\n        return self.n\nstruct Holder:\n    c: Container[int]\nfn sum2(xs: List[Container[int]]) -> int:\n    return xs[0].get(0) + xs[1].get(0)\nfn main():\n    h := Holder(Bag(10))\n    print(h.c.get(0))\n    print(sum2([Bag(20), Bag(30)]))\nmain()\n";
    assert_mc_parity(src, "10\n50\n");
}

#[test]
fn param_protocol_reassign_accept_runs() {
    // DECISION-3 reassignment write-site (accept path): reassigning a different conforming struct into
    // a `Container[int]` local witnesses at the reassign boundary, then dispatches by name identically.
    let src = "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct A:\n    fn get(self, i: int) -> int:\n        return 1\nstruct B:\n    fn get(self, i: int) -> int:\n        return 2\nfn f(c: Container[int]) -> int:\n    c = B()\n    return c.get(0)\nfn main():\n    print(f(A()))\nmain()\n";
    assert_mc_parity(src, "2\n");
}

#[test]
fn from_witness_runs() {
    // From slice 2 — a static-ctor witness of `[T: From[int]]` type-checks; the bound-check
    // runs at the call site then erases (the trivial body never calls `T.from` — that's slice 3), so
    // the program runs byte-identically.
    let src = "struct Port:\n    n: int\n    fn from(x: int) -> Port:\n        return Port(n=x)\nfn use2[T: From[int]](x: int) -> int:\n    return x\nfn main():\n    print(use2[Port](5))\nmain()\n";
    assert_mc_parity(src, "5\n");
}

#[test]
fn legal_deep_nested_pattern_type_checks_and_runs() {
    // VERIFY (guards against over-rejection): a legally-nested `?(...)` value and a matching deep
    // pattern must NOT be rejected by the new pattern-depth guard — it still type-checks, and the
    // checker's exhaustiveness + type walk and the VM matcher stay safe recursing to that depth.
    // Depth is capped by the VALUE expression itself: each `?(` nesting costs ~2 of the shared
    // `parser::MAX_DEPTH` budget in expression position (parse_expr_bp + parse_unary), so a nested
    // ctor value maxes out near MAX_DEPTH/2 — the pattern side (1 depth/level) is the looser
    // constraint. THIS `n` IS CALIBRATED TO `parser::MAX_DEPTH` and must move with it: 30 at the
    // shipped cap of 64. (It was cut to 20 while `Span` was briefly 24 bytes and the cap 48; `Span`
    // is now 12 bytes, the cap is 64 again, so 30 is restored.) 30 is deep enough to prove the
    // pattern walk is safe and shallow enough to stay legal on both axes.
    let n = 30;
    let value = format!("{}0{}", "?(".repeat(n), ")".repeat(n));
    let pattern = format!("{}x{}", "?(".repeat(n), ")".repeat(n));
    let src = format!("o := {value}\nr := match o:\n    {pattern}: x\n    _: -1\nprint(r)\n");
    assert_eq!(run_capture(&src).unwrap().trim(), "0");
}

/// Crash-safety over the ITERATIVE-chain / composed-nesting depth axis: a deep AST the parser
/// ACCEPTS (~12.5 k nodes here, against `parser::MAX_AST_DEPTH` = 16 000) must run
/// on the VM's dedicated stack without a host stack overflow. This is the regression guard for the
/// deep-`1+1+…` / `a.f.f…` SIGABRT: the parser cap keeps the depth bounded, and the large VM /
/// front-end stacks absorb the walk. Runs on the debug test-harness → the 384 MiB `VM_STACK_BYTES`
/// thread (via `run_capture`), the *smaller* of the two front-end stacks, so debug frames are the
/// worst case. A single flat chain just under the cap must also run and compute correctly.
#[test]
fn deep_accepted_chains_run_without_stack_overflow() {
    // ~12.5 k-deep AST: 25 nested parens (comfortably under the `parser::MAX_DEPTH` paren ceiling,
    // which each paren costs ~2 of), each adding a 499-fold `+0` chain onto the left spine. This is
    // the shape that COMPOSES the two depth axes — the folds land on top of the descent — and it is
    // the concrete floor W7-50 had to clear when it picked `parser::MAX_AST_DEPTH` = 16 000 over the
    // 8 000 first proposed. Note what 16 000 did NOT do: it NARROWED the worst accepted AST from
    // ~29 940 nodes (measured on `b1307258` — a paren level can spend BOTH fold loops, at ~998 nodes
    // per level) to ~15 968, deliberately, because 29 940 sat at only 1.11x the ~33 100-node cliff.
    // That ~15 968 held only per-`Parser` while an interpolated `{…}` fragment was built by a second
    // `Parser` that got a fresh 16 000, so depth summed across nesting levels. A hole is parsed by
    // the SAME `Parser` now (TICKET-252), so its depth and fold-depth carry on, and
    // `desugar::Walker::walk_expr` bounds the finished tree as the backstop. The bound is
    // UNCONDITIONAL as of W7-51: it needed both a second desugar pass and a spliced default
    // EXPRESSION to slip past, and neither exists.
    // Value is invariant (all `+0`) so the result is deterministic; the point is that
    // walking/compiling/running this depth does not abort. Assign then print separately so the
    // `print(...)` call wrapper doesn't eat into the paren budget. THE PAREN COUNT IS CALIBRATED TO
    // `parser::MAX_AST_DEPTH` (25 × 500 = 12 500, against 16 000) and must move with it.
    let mut inner = String::from("1");
    for _ in 0..25 {
        inner = format!("({inner}{})", "+0".repeat(499));
    }
    assert_eq!(
        run_capture(&format!("x := {inner}\nprint(x)\n"))
            .unwrap()
            .trim(),
        "1"
    );

    // A single flat chain just under the cap runs and computes the right sum.
    let flat = format!("print(1{})\n", "+1".repeat(400));
    assert_eq!(run_capture(&flat).unwrap().trim(), "401");
}

/// From overloads: a fault inside one `from` overload names its frame `from` in the trace,
/// never the internal overload name (`from$str`). The overload's own body runs: the int
/// overload would not fault.
#[test]
fn a_from_overload_frame_in_a_trace_reads_from() {
    let dir = std::env::temp_dir().join(format!("chezzi_from_ov_trace_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import From from std.convert\nstruct Rat:\n    n: int\n    fn from(x: int) -> Rat:\n        return Rat(x)\n    fn from(x: str) -> Rat:\n        return Rat([1][9])\nprint(Rat.from(\"a\").n)\n",
    )
    .unwrap();
    let (_o, _e, res, _c) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    let e = res.expect_err("the str overload faults");
    let trace = format_trace(&e);
    assert!(
        trace.contains("at from"),
        "expected a `from` frame, got:\n{trace}"
    );
    assert!(
        !trace.contains("from$"),
        "the internal overload name reached a trace:\n{trace}"
    );
}
