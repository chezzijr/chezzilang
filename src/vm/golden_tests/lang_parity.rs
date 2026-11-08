//! Golden tests: bitwise ops, match, json, fs/time, sets, for over maps, sort and sort_by_key,
//! break/continue, generators.
//! Split out of `src/vm/golden_tests.rs`. `super::*` is the parent module.

use super::*;

#[test]
fn bitwise_ops_parity() {
    assert_golden_out(
        "print(5 & 3)\nprint(5 | 2)\nprint(5 ^ 3)\nprint(1 << 4)\nprint(255 >> 4)\n",
        "1\n7\n6\n16\n15\n",
    );
}

#[test]
fn bitwise_precedence_below_comparison_parity() {
    // `5 & 3 == 1` is `(5 & 3) == 1` (bitwise binds tighter than `==`, Python-style).
    assert_golden_out("print(5 & 3 == 1)\n", "true\n");
}

#[test]
fn xor_fold_single_number_parity() {
    assert_golden_out(
        "xs := [4,1,2,1,4,2,7]\nacc := 0\nfor x in xs:\n    acc = acc ^ x\nprint(acc)\n",
        "7\n",
    );
}

#[test]
fn shift_out_of_range_error_parity() {
    // Dynamic shift the checker can't catch — must raise the runtime error at run time.
    assert_eq!(
        vm_outcome("print(1 << 64)\n").unwrap_err(),
        "runtime error (line 1, col 7): shift amount 64 out of range (0..64)"
    );
    assert_eq!(
        vm_outcome("print(1 << -1)\n").unwrap_err(),
        "runtime error (line 1, col 7): shift amount -1 out of range (0..64)"
    );
}

#[test]
fn match_tuple_pattern_parity() {
    assert_golden_out(
        "p := (3, 4)\nmatch p:\n    (0, y): print(y)\n    (x, y): print(x + y)\n",
        "7\n",
    );
}

#[test]
fn match_tuple_literal_arm_parity() {
    assert_golden_out(
        "p := (1, 9)\nlabel := match p:\n    (1, n): \"one {n}\"\n    _: \"other\"\nprint(label)\n",
        "one 9\n",
    );
}

#[test]
fn match_nested_variant_in_tuple_parity() {
    assert_golden_out(
        "o: (int, int)? = ?(10, 20)\nmatch o:\n    None: print(\"none\")\n    ?(a, b): print(a + b)\n",
        "30\n",
    );
}

#[test]
fn match_nested_heap_payload_gc_stress() {
    // Nested pattern binding heap values (strings) inside a tuple inside a variant; a GC mid-bind
    // must not collect the still-referenced payload.
    let src = "o: (str, str)? = ?(\"a\" + \"b\", \"c\" + \"d\")\nmatch o:\n    None: print(\"none\")\n    ?(x, y): print(x + y)\n";
    assert_golden_out(src, "abcd\n");
    assert_eq!(vm_outcome(src).unwrap(), "abcd\n");
    assert_eq!(
        run_capture_stress(src),
        "abcd\n",
        "VM gc_stress diverged (rooting bug?)"
    );
}

#[test]
fn match_guard_fallthrough_parity() {
    // The first arm's pattern binds but its guard is false → fall through to the next arm.
    assert_golden_out(
        "n := 5\nmatch n:\n    x if x < 0: print(\"neg\")\n    x if x > 10: print(\"big\")\n    _: print(\"mid\")\n",
        "mid\n",
    );
}

#[test]
fn match_guard_first_true_parity() {
    assert_golden_out(
        "n := -2\nlabel := match n:\n    x if x < 0: \"neg\"\n    _: \"nonneg\"\nprint(label)\n",
        "neg\n",
    );
}

#[test]
fn match_range_boundaries_parity() {
    // Half-open: start is inclusive, end is exclusive.
    assert_golden_out(
        "fn b(n: int) -> str:\n    return match n:\n        0..10: \"lo\"\n        10..20: \"hi\"\n        _: \"out\"\nprint(b(0))\nprint(b(9))\nprint(b(10))\nprint(b(19))\nprint(b(20))\n",
        "lo\nlo\nhi\nhi\nout\n",
    );
}

#[test]
fn match_range_with_literal_mix_parity() {
    // A match mixing int literals and ranges still routes through the literal path.
    assert_golden_out(
        "fn f(n: int) -> str:\n    return match n:\n        0: \"zero\"\n        1..100: \"small\"\n        _: \"big\"\nprint(f(0))\nprint(f(50))\nprint(f(500))\n",
        "zero\nsmall\nbig\n",
    );
}

#[test]
fn for_over_map_keys_parity() {
    assert_golden_out(
        "m := {\"a\": 1, \"b\": 2, \"c\": 3}\nfor k in m.keys():\n    print(k)\n",
        "a\nb\nc\n",
    );
}

/// Regression (review #1): a struct field named `decode` must still be indexable — the
/// `.decode[…]` JSON form is only stolen when a real `[Type](arg)` follows.
#[test]
fn field_named_decode_is_indexable_parity() {
    let out = golden_entry(
        "struct Box:\n    decode: List[int]\nb := Box([10, 20, 30])\nprint(b.decode[1])\nprint(b.decode[0] + b.decode[2])\n",
    );
    assert_eq!(out, "20\n40\n");
}

/// Regression (review #2): malformed and out-of-range numbers come back as `Err` / stringify
/// cleanly — they must never abort the host (no uncaught `float()`/`int()` panic).
#[test]
fn json_malformed_numbers_are_errors_parity() {
    let out = golden_entry(
        "import std.json\nfn tp(s: str) -> str:\n    match json.parse(s):\n        ?j: return \"OK \" + json.stringify(j)\n        !e: return \"ERR\"\nprint(tp(\"1e\"))\nprint(tp(\"1.\"))\nprint(tp(\"100000000000000000000\"))\n",
    );
    assert_eq!(out, "ERR\nERR\nOK 1e+20\n");
}

/// BUG 1: `json.stringify` must `\u00XX`-escape control chars U+0000..U+001F (Go/RFC-8259
/// policy) instead of emitting the raw byte (invalid JSON). Round-trips back to the original
/// string via `json.parse`'s `\u` handling.
#[test]
fn json_control_char_escape_roundtrip_parity() {
    let out = golden_entry(
        "import std.json\ns := \"x\" + chr(1) + chr(31) + chr(0) + \"y\"\nout := json.stringify(json.Json.Str(s))\ncodes := List[int]()\nfor c in out.chars():\n    codes.push(ord(c))\nprint(codes)\nmatch json.parse(out):\n    ?j:\n        match j:\n            json.Json.Str(back):\n                if back == s:\n                    print(\"RT_OK\")\n                else:\n                    print(\"RT_FAIL\")\n            _: print(\"NOT_STR\")\n    !e: print(\"PARSE_ERR\")\n",
    );
    assert_eq!(
        out,
        "[34, 120, 92, 117, 48, 48, 48, 49, 92, 117, 48, 48, 49, 102, 92, 117, 48, 48, 48, 48, 121, 34]\nRT_OK\n"
    );
}

/// BUG 2: `json.parse` rejects leading-zero integers (RFC-8259 / Python `json.loads`): a `0`
/// immediately followed by another digit is an error. Lone `0`/`-0`/`0.5`/`0e1` stay valid.
#[test]
fn json_leading_zero_numbers_are_errors_parity() {
    let out = golden_entry(
        "import std.json\nfn tp(s: str) -> str:\n    match json.parse(s):\n        ?j: return \"OK \" + json.stringify(j)\n        !e: return \"ERR\"\nprint(tp(\"01\"))\nprint(tp(\"007\"))\nprint(tp(\"-01\"))\nprint(tp(\"01.5\"))\nprint(tp(\"08\"))\nprint(tp(\"0\"))\nprint(tp(\"-0\"))\nprint(tp(\"0.5\"))\nprint(tp(\"10\"))\nprint(tp(\"0e1\"))\n",
    );
    assert_eq!(
        out,
        "ERR\nERR\nERR\nERR\nERR\nOK 0\nOK 0\nOK 0.5\nOK 10\nOK 0.0\n"
    );
}

/// OPTIONAL: a RAW control char (e.g. a literal newline, ord 10) inside a JSON string literal is
/// rejected (RFC/Python); a proper `\n` ESCAPE sequence still parses — proving the `\` arm is
/// untouched.
#[test]
fn json_raw_control_char_in_string_is_error_parity() {
    let out = golden_entry(
        "import std.json\nfn tp(s: str) -> str:\n    match json.parse(s):\n        ?j: return \"OK\"\n        !e: return \"ERR\"\nraw := \"\\\"x\" + chr(10) + \"y\\\"\"\nprint(tp(raw))\nprint(tp(\"\\\"a\\\\nb\\\"\"))\n",
    );
    assert_eq!(out, "ERR\nOK\n");
}

/// Finding C: `json.stringify` FAULTS (recoverable, Go-style) on a non-finite `Json.Num`
/// (NaN / +inf / -inf) instead of emitting the invalid bare tokens `NaN`/`inf`/`-inf` — which are
/// not valid JSON and are rejected by Chezzi's own `json.parse`. The fault is catchable under
/// `recover:` with a byte-identical message.
#[test]
fn json_stringify_non_finite_faults_parity() {
    let out = golden_entry(
        "import std.json\nfn tp(x: float) -> str:\n    doc := json.Json.Obj({\"v\": json.Json.Num(x)})\n    r := recover:\n        json.stringify(doc)\n    match r:\n        ?s: return \"OK \" + s\n        !e: return e.message()\nprint(tp(1.0 / 0.0))\nprint(tp(-1.0 / 0.0))\nprint(tp(0.0 / 0.0))\n",
    );
    assert_eq!(
        out,
        "cannot serialize non-finite float to JSON\ncannot serialize non-finite float to JSON\ncannot serialize non-finite float to JSON\n"
    );
}

/// Float `str()`/`print()` crossover golden — scientific notation when the decimal exponent is
/// `< -4` or `>= 16`, otherwise fixed — asserted **against a hardcoded literal**.
///
/// Read the name carefully: this is NOT a CPython differential. It pins these eight values against a
/// string typed into this file, so it can only catch an engine divergence or a regression away from
/// the values below — never a divergence from real CPython. `W7-31`'s filing cited it as a
/// CPython-parity pin, and that is part of why `W7-32` (shortest-repr ties broken away from zero
/// instead of to even) stayed invisible. The actual CPython differentials for float formatting are
/// `fmtspec::tests::python_float_repr_and_e_spec` (a table whose every expected string is a real
/// `python3 repr()` run) and `tests/chz/spec/conversions_test.chz`.
#[test]
fn python_float_repr_str_parity() {
    let out = golden_entry(
        "print(\"{1e16}\")\nprint(1.5e300)\nprint(0.00001)\nprint(\"{0 - 2.5e-8}\")\nprint(1.0)\nprint(0.0001)\nprint(1e15)\nprint(1e100)\n",
    );
    assert_eq!(
        out,
        "1e+16\n1.5e+300\n1e-05\n-2.5e-08\n1.0\n0.0001\n1000000000000000.0\n1e+100\n"
    );
}

/// Finding C regression guard: FINITE floats (including large magnitudes OUTSIDE the ±9e15
/// int-collapse range like 1e300), whole-number floats, negatives, ints, strings and nested
/// Arr/Obj are COMPLETELY unaffected — they stringify byte-identically and round-trip through
/// `json.parse`. Proves the non-finite guard does NOT reuse the ±9e15 range check.
#[test]
fn json_stringify_finite_roundtrip_unchanged_parity() {
    let out = golden_entry(
        "import std.json\ndoc := json.Json.Arr([json.Json.Num(3.0), json.Json.Num(1.5), json.Json.Num(1e300), json.Json.Num(0.0 - 2.5), json.Json.Str(\"hi\"), json.Json.Obj({\"k\": json.Json.Num(42.0)})])\ns := json.stringify(doc)\nprint(s)\nmatch json.parse(s):\n    ?v: print(\"roundtrip \" + json.stringify(v))\n    !e: print(\"ERR \" + e.message())\n",
    );
    // 1e300 is FINITE and far outside the i64 window: it must stringify normally (via `str(f)`),
    // NOT fault. `str(1e300)` renders in scientific notation (CPython repr parity, exponent >= 16):
    // `1e+300`. Every `Json.Num` here is float-shaped, so `num_str` renders `str(f)` verbatim — a
    // `Num` always keeps its trailing `.0` (TICKET-013, W8-35's `Json.Int` split).
    let line = "[3.0,1.5,1e+300,-2.5,\"hi\",{\"k\":42.0}]".to_string();
    assert_eq!(out, format!("{line}\nroundtrip {line}\n"));
}

#[test]
fn json_decode_struct_parity() {
    let out = golden_entry(
        "import std.json\nstruct P:\n    x: int\n    y: int\nmatch json.decode[P](\"{{\\\"x\\\":1,\\\"y\\\":2}}\"):\n    ?p: print(p.x + p.y)\n    !e: print(e)\n",
    );
    assert_eq!(out, "3\n");
}

#[test]
fn json_decode_error_parity() {
    let out = golden_entry(
        "import std.json\nstruct P:\n    x: int\nmatch json.decode[P](\"{{\\\"y\\\":2}}\"):\n    ?p: print(p.x)\n    !e: print(e)\n",
    );
    assert_eq!(out, "decode: missing key 'x' at $\n");
}

/// `json.as_int` is a total `-> Option[int]`: an in-range finite value (incl. the i64::MAX/MIN
/// boundaries) returns `Some`, an out-of-i64-range-but-finite number returns `None` (never faults),
/// and a fractional number still truncates. (Non-finite numbers can no longer arrive here — `parse`
/// rejects `1e400` at the source, so that input takes the `PARSEERR` arm below.)
#[test]
fn json_as_int_out_of_range_parity() {
    let out = golden_entry(
        "import std.json\nfn a(s: str) -> str:\n    match json.parse(s):\n        ?j:\n            match json.as_int(j):\n                ?v: return \"SOME \" + \"{v}\"\n                None: return \"NONE\"\n        !e: return \"PARSEERR\"\nprint(a(\"9999999999999999999\"))\nprint(a(\"42\"))\nprint(a(\"9223372036854775807\"))\nprint(a(\"-9223372036854775808\"))\nprint(a(\"2.5\"))\nprint(a(\"1e400\"))\n",
    );
    assert_eq!(
        out,
        "NONE\nSOME 42\nSOME 9223372036854775807\nSOME -9223372036854775808\nSOME 2\nPARSEERR\n"
    );
}

/// `json.parse` follows the Go `encoding/json` decode policy: a numeral whose magnitude overflows
/// f64 to a non-finite value (`1e400` → +inf, `-1e400` → -inf, and the same inside an array) is
/// REJECTED with `Err` at parse time — parse never manufactures a `Json.Num(inf/-inf/NaN)` that its
/// own `stringify` would then refuse to serialize. Finite numbers (incl. underflow-to-0 `1e-400`)
/// still parse `Ok`.
#[test]
fn json_parse_rejects_non_finite_parity() {
    let out = golden_entry(
        "import std.json\nfn p(s: str) -> str:\n    match json.parse(s):\n        ?j: return \"OK\"\n        !e: return \"PARSEERR\"\nprint(p(\"1e400\"))\nprint(p(\"-1e400\"))\nprint(p(\"[1e400]\"))\nprint(p(\"1.5\"))\nprint(p(\"123\"))\nprint(p(\"1e-400\"))\n",
    );
    assert_eq!(out, "PARSEERR\nPARSEERR\nPARSEERR\nOK\nOK\nOK\n");
}

/// `str.split` honours the `pieces == separators + 1` invariant at *every* input including the
/// empty string: `"".split(",")` has zero separators, so it yields a one-element list holding the
/// empty string (length 1, `x[0] == ""`) — matching Python/Go/Rust/JS, never `[]`. (An empty list
/// and a list holding a single empty string both *render* as `[]` because `""` prints as nothing,
/// so this asserts the length + element rather than the debug rendering.)
#[test]
fn str_empty_split_returns_single_empty_element_parity() {
    let out = golden_entry(
        "x := \"\".split(\",\")\nprint(x.len())\nprint(x[0] == \"\")\nprint(\"\".split(\",\").len() == 1)\nprint(\"a,\".split(\",\").len())\n",
    );
    assert_eq!(out, "1\ntrue\ntrue\n2\n");
}

/// The i64::MIN boundary literal `-9223372036854775808` (previously unreachable — it lex-errored as
/// "number too large" because the positive magnitude 2^63 overflows i64 before the minus applies)
/// now evaluates to i64::MIN and behaves as an ordinary int under arithmetic and `match`.
#[test]
fn i64_min_literal_runs_parity() {
    let out = golden_entry(
        "print(-9223372036854775808)\nprint(-9223372036854775808 + 1)\nprint(-9223372036854775807 - 1 == -9223372036854775808)\nmatch -9223372036854775808:\n    -9223372036854775808: print(\"min\")\n    _: print(\"other\")\n",
    );
    assert_eq!(
        out,
        "-9223372036854775808\n-9223372036854775807\ntrue\nmin\n"
    );
}

/// `json.decode[int]` rejects out-of-range integers with `Err` (never silently saturates), while
/// the exact i64::MAX / i64::MIN boundaries still decode to `Ok`.
#[test]
fn json_decode_int_out_of_range_parity() {
    let out = golden_entry(
        "import std.json\nfn d(s: str) -> str:\n    match json.decode[int](s):\n        ?v: return \"OK \" + \"{v}\"\n        !e: return \"ERR\"\nprint(d(\"1000000000000000000000000000000\"))\nprint(d(\"-1000000000000000000000000000000\"))\nprint(d(\"18446744073709551615\"))\nprint(d(\"9223372036854775807\"))\nprint(d(\"-9223372036854775808\"))\nprint(d(\"42\"))\n",
    );
    assert_eq!(
        out,
        "ERR\nERR\nERR\nOK 9223372036854775807\nOK -9223372036854775808\nOK 42\n"
    );
}

/// Cross-site consistency: for the identical strictly-out-of-range input, `decode[int]` returns
/// `Err` and `as_int` returns `None` — both total, neither saturates, neither faults.
#[test]
fn json_int_boundary_consistency_parity() {
    let out = golden_entry(
        "import std.json\ns := \"9999999999999999999\"\nmatch json.decode[int](s):\n    ?v: print(\"decode OK\")\n    !e: print(\"decode ERR\")\nmatch json.parse(s):\n    ?j:\n        match json.as_int(j):\n            ?v: print(\"as_int SOME\")\n            None: print(\"as_int NONE\")\n    !e: print(\"parse ERR\")\n",
    );
    assert_eq!(out, "decode ERR\nas_int NONE\n");
}

#[test]
fn process_cmd_ok_and_err_parity() {
    let out = golden_entry(
        "import std.process\nmatch process.cmd(\"printf abc\"):\n    ?s: print(\"ok:\" + s)\n    !e: print(\"err:\" + e)\nmatch process.cmd(\"exit 2\"):\n    ?s: print(\"ok\")\n    !e: print(\"err:\" + e)\n",
    );
    assert_eq!(out, "ok:abc\nerr:command exited with status 2\n");
}

#[test]
fn fs_predicates_parity() {
    let out = golden_entry(
        "import std.fs\nprint(fs.exists(\"Cargo.toml\"))\nprint(fs.exists(\"definitely_not_here.zzz\"))\nprint(fs.is_dir(\"src\"))\n",
    );
    assert_eq!(out, "true\nfalse\ntrue\n");
}

/// gaps §6 — fs.stat/fs.walk + the FileInfo native struct. Proves the
/// reserved-type gate (`from fs import FileInfo`) + the importer slot list skipping a type member,
/// and that stat/walk read the real filesystem identically. Scratch tree is self-owned (file-dependent
/// size — mtime is NOT asserted). Walk order is deterministic (per-dir sort) → exact stdout match.
#[test]
fn fs_stat_walk_fileinfo_parity() {
    let scratch = TmpDir::new();
    scratch.write("root/a.txt", "hello\n"); // 6 bytes
    scratch.write("root/b.txt", "b");
    scratch.write("root/sub/c.txt", "c");
    let file = scratch.0.join("root/a.txt");
    let file = file.to_string_lossy();
    let root = scratch.0.join("root");
    let root = root.to_string_lossy();
    // `import FileInfo from std.fs` proves the reserved-type gate + the importer slot list skipping a type
    // (it runtime-traps if FileInfo is not registered). `import std.fs` licenses the
    // `fs.stat`/`fs.walk` qualified calls.
    let src = format!(
        "import std.fs\n\
         import FileInfo from std.fs\n\
         match fs.stat(\"{file}\"):\n\
         \x20   ?fi:\n\
         \x20       print(\"{{fi.size}}\")\n\
         \x20       print(\"{{fi.is_file}}\")\n\
         \x20       print(\"{{fi.is_dir}}\")\n\
         \x20   !e: print(\"staterr\")\n\
         match fs.walk(\"{root}\"):\n\
         \x20   ?xs:\n\
         \x20       for p in xs:\n\
         \x20           print(p)\n\
         \x20   !e: print(\"walkerr\")\n",
    );
    let out = golden_entry(&src);
    let expected = format!(
        "6\ntrue\nfalse\n{a}\n{b}\n{sub}\n{c}\n",
        a = scratch.0.join("root/a.txt").to_string_lossy(),
        b = scratch.0.join("root/b.txt").to_string_lossy(),
        sub = scratch.0.join("root/sub").to_string_lossy(),
        c = scratch.0.join("root/sub/c.txt").to_string_lossy(),
    );
    assert_eq!(out, expected);
}

#[test]
fn time_format_parity() {
    let out = golden_entry(
        "import std.time\nprint(time.format(0))\nprint(time.format(1700000000))\nprint(time.now() > 0)\n",
    );
    assert_eq!(out, "1970-01-01 00:00:00\n2023-11-14 22:13:20\ntrue\n");
}

#[test]
fn set_dedup_and_algebra_parity() {
    assert_golden_out(
        "s := {3, 1, 3, 2, 1}\nprint(s.len())\nprint({1,2,3}.union({3,4}).len())\nprint({1,2,3}.intersection({2,3,4}).len())\nprint({1,2,3}.difference({2,3}).len())\nprint({1,2} == {2,1})\n",
        "3\n4\n2\n1\ntrue\n",
    );
}

#[test]
fn set_mutation_and_iteration_parity() {
    assert_golden_out(
        "s := Set()\ns.insert(10)\ns.insert(10)\ns.insert(20)\nprint(s.len())\nprint(s.remove(10))\nprint(s.remove(10))\ntotal := 0\nfor x in {5, 15, 25}:\n    total += x\nprint(total)\n",
        "2\ntrue\nfalse\n45\n",
    );
}

#[test]
fn set_display_parity() {
    assert_golden_out("print({1, 2, 3})\nprint(Set())\n", "{1, 2, 3}\nSet()\n");
}

#[test]
fn str_chars_parity() {
    assert_golden_out(
        "cs := \"héllo\".chars()\nprint(cs.len())\nprint(cs[1])\n",
        "5\né\n",
    );
}

#[test]
fn for_over_str_parity() {
    assert_golden_out(
        "out := \"\"\nfor c in \"abc\":\n    out = out + c + \"-\"\nprint(out)\n",
        "a-b-c-\n",
    );
}

#[test]
fn for_over_empty_str_parity() {
    assert_golden_out("n := 0\nfor c in \"\":\n    n += 1\nprint(n)\n", "0\n");
}

#[test]
fn for_over_map_key_value_parity() {
    assert_golden_out(
        "m := {\"a\": 1, \"b\": 2}\ns := 0\nfor k, v in m:\n    print(\"{k}={v}\")\n    s += v\nprint(s)\n",
        "a=1\nb=2\n3\n",
    );
}

#[test]
fn for_over_map_kv_mutation_during_iteration_parity() {
    // The body reassigns a not-yet-visited key (snapshot semantics:
    // the value bound is the one captured at loop start, like list iteration).
    assert_golden_out(
        "m := {\"a\": 1, \"b\": 2, \"c\": 3}\nout := 0\nfor k, v in m:\n    m[\"c\"] = 99\n    out += v\nprint(out)\n",
        "6\n",
    );
}

#[test]
fn for_over_map_kv_remove_during_iteration_parity() {
    // Removing a future key mid-iteration must not crash one engine while the other succeeds.
    assert_golden_out(
        "m := {\"a\": 1, \"b\": 2}\nfirst := true\nsum := 0\nfor k, v in m:\n    if first:\n        m.remove(\"b\")\n        first = false\n    sum += v\nprint(sum)\n",
        "3\n",
    );
}

#[test]
fn for_over_map_break_continue_parity() {
    // break/continue still target the index increment over the keys sequence.
    assert_golden_out(
        "m := {\"a\": 1, \"b\": 2, \"c\": 3, \"d\": 4}\nfor k, v in m:\n    if v == 2: continue\n    if v == 4: break\n    print(k)\n",
        "a\nc\n",
    );
}

#[test]
fn cmp_max_int_parity() {
    // Generic min/max now live in std.cmp; abs stays in std.math. File/graph path required.
    let out = golden_entry(
        "import std.cmp\nimport std.math\nfn main():\n    print(cmp.max(3, 5))\n    print(cmp.min(3, 5))\n    print(math.abs(-5))\nmain()\n",
    );
    assert_eq!(out, "5\n3\n5\n");
}

#[test]
fn cmp_max_float_parity() {
    let out = golden_entry(
        "import std.cmp\nimport std.math\nfn main():\n    print(cmp.max(3.0, 5.0))\n    print(math.abs(-2.5))\nmain()\n",
    );
    assert_eq!(out, "5.0\n2.5\n");
}

#[test]
fn cmp_max_struct_parity() {
    // The generic max over a Comparable struct must be byte-identical.
    let src = "import std.cmp\nstruct P:\n    n: int\n    fn compare(self, other: P) -> int:\n        return self.n - other.n\n    fn eq(self, other: P) -> bool:\n        return self.n == other.n\nfn main():\n    print(cmp.max(P(2), P(9)).n)\n    print(cmp.min(P(2), P(9)).n)\nmain()\n";
    assert_eq!(golden_entry(src), "9\n2\n");
}

#[test]
fn ord_chr_parity() {
    assert_golden_out("print(ord(\"A\"))\nprint(chr(97))\n", "65\na\n");
}

#[test]
fn ord_chr_roundtrip_parity() {
    assert_golden_out("print(chr(ord(\"z\")))\n", "z\n");
}

#[test]
fn ord_index_digit_value_parity() {
    // The digit-value idiom over an indexed char.
    assert_golden_out("s := \"7\"\nprint(ord(s[0]) - ord(\"0\"))\n", "7\n");
}

#[test]
fn ord_empty_string_error_parity() {
    // Runtime error (checker can't catch it).
    assert_eq!(
        vm_outcome("print(ord(\"\"))\n").unwrap_err(),
        "runtime error (line 1, col 7): ord() of an empty string"
    );
}

#[test]
fn chr_invalid_codepoint_error_parity() {
    assert_eq!(
        vm_outcome("print(chr(-1))\n").unwrap_err(),
        "runtime error (line 1, col 7): chr(): -1 is not a valid Unicode codepoint"
    );
    assert_eq!(
        vm_outcome("print(chr(2000000))\n").unwrap_err(),
        "runtime error (line 1, col 7): chr(): 2000000 is not a valid Unicode codepoint"
    );
}

#[test]
fn sort_by_descending_parity() {
    assert_golden_out(
        "xs := [3,1,2]\nxs.sort_by(fn(a: int, b: int) -> int: b - a)\nprint(xs)\n",
        "[3, 2, 1]\n",
    );
}

#[test]
fn sort_by_stable_by_key_parity() {
    // Equal keys (string length) must keep input order — stability is part of the contract.
    assert_golden_out(
        "ws := [\"bb\", \"a\", \"dd\", \"e\"]\nws.sort_by(fn(a: str, b: str) -> int: a.len() - b.len())\nprint(ws)\n",
        "['a', 'e', 'bb', 'dd']\n",
    );
}

#[test]
fn sort_by_comparator_mutating_the_list_faults() {
    // The in-comparator `xs[0] = 100` is now detected against the rooted snapshot and faulted,
    // matching CPython's `ValueError: list modified during sort` — never discarded.
    let src = "xs := [3, 1, 2]\nfn cmp(a: int, b: int) -> int:\n    xs[0] = 100\n    return a - b\nxs.sort_by(cmp)\nprint(xs)\n";
    assert!(
        vm_outcome(src)
            .unwrap_err()
            .contains("list modified during 'sort_by'"),
        "{:?}",
        vm_outcome(src)
    );
}

#[test]
fn sort_by_empty_and_singleton_parity() {
    assert_golden_out(
        "xs := [42]\nxs.sort_by(fn(a: int, b: int) -> int: a - b)\nprint(xs)\n",
        "[42]\n",
    );
}

#[test]
fn break_early_for_parity() {
    assert_golden_out(
        "s := 0\nfor i in 0..10:\n    if i == 5: break\n    s += i\nprint(s)\n",
        "10\n",
    );
}

#[test]
fn continue_for_terminates_parity() {
    // THE increment-landing guard: `continue` must reach the loop's `i += 1`, never the
    // condition (would re-test the same `i` forever). If this hangs, the target is wrong.
    assert_golden_out(
        "for i in 0..5:\n    if i == 1: continue\n    if i == 3: continue\n    print(i)\n",
        "0\n2\n4\n",
    );
}

#[test]
fn while_break_parity() {
    assert_golden_out(
        "i := 0\nwhile true:\n    if i == 3: break\n    i += 1\nprint(i)\n",
        "3\n",
    );
}

#[test]
fn while_continue_progresses_parity() {
    // The counter advances BEFORE the `continue`, so the `while` still terminates.
    assert_golden_out(
        "i := 0\ns := 0\nwhile i < 5:\n    i += 1\n    if i == 2: continue\n    s += i\nprint(s)\n",
        "13\n",
    );
}

#[test]
fn break_in_if_in_loop_parity() {
    assert_golden_out(
        "for i in 0..10:\n    if i > 2:\n        break\n    print(i)\n",
        "0\n1\n2\n",
    );
}

#[test]
fn return_from_loop_parity() {
    // `return` inside a loop still returns the whole function (break/continue don't intercept it).
    assert_golden_out(
        "fn f() -> int:\n    for i in 0..10:\n        if i == 2: return i\n    return -1\nprint(f())\n",
        "2\n",
    );
}

#[test]
fn nested_loop_inner_break_parity() {
    // Inner `break` does not break the outer loop: the outer runs all 3 iterations.
    assert_golden_out(
        "n := 0\nfor i in 0..3:\n    for j in 0..3:\n        break\n    n += 1\nprint(n)\n",
        "3\n",
    );
}

#[test]
fn continue_list_for_parity() {
    // `continue` over a LIST for-loop (not just range) advances to the next element.
    assert_golden_out(
        "for x in [1,2,3,4]:\n    if x % 2 == 0: continue\n    print(x)\n",
        "1\n3\n",
    );
}

#[test]
fn break_list_for_parity() {
    assert_golden_out(
        "for x in [10,20,30,40]:\n    if x == 30: break\n    print(x)\n",
        "10\n20\n",
    );
}

// ----- literal + wildcard match parity -----

#[test]
fn match_int_literals_stmt_parity() {
    assert_golden_out(
        "n := 2\nmatch n:\n    0: print(\"zero\")\n    1: print(\"one\")\n    _: print(\"many\")\n",
        "many\n",
    );
}

#[test]
fn match_str_literals_expr_parity() {
    assert_golden_out(
        "c := \"x\"\ns := match c:\n    \"a\": \"first\"\n    _: \"other\"\nprint(s)\n",
        "other\n",
    );
}

#[test]
fn match_bool_literals_parity() {
    assert_golden_out(
        "b := false\nmatch b:\n    true: print(\"yes\")\n    false: print(\"no\")\n    _: print(\"?\")\n",
        "no\n",
    );
}

#[test]
fn match_literal_matched_arm_parity() {
    // The matching literal arm fires (wildcard not reached).
    assert_golden_out(
        "n := 1\ns := match n:\n    0: \"a\"\n    1: \"b\"\n    _: \"z\"\nprint(s)\n",
        "b\n",
    );
}

#[test]
fn match_wildcard_reached_parity() {
    // No literal matches → the `_` arm fires.
    assert_golden_out(
        "n := 9\ns := match n:\n    0: \"a\"\n    1: \"b\"\n    _: \"z\"\nprint(s)\n",
        "z\n",
    );
}

#[test]
fn match_variant_regression_parity() {
    // A variant match still lowers via the variant path unchanged.
    assert_golden_out(
        "o := ?5\nmatch o:\n    ?v: print(\"got {v}\")\n    None: print(\"none\")\n",
        "got 5\n",
    );
}

#[test]
fn parity_std_math() {
    let src = "\
import std.math
fn main():
    print(math.floor(2.7))
    print(math.ceil(2.1))
    print(math.sqrt(16.0))
    print(math.pow(2.0, 10.0))
    print(math.abs(0.0 - 3.5))
    print(math.round(2.5))
    print(math.pi)
main()";
    assert_eq!(
        golden_entry(src),
        "2.0\n3.0\n4.0\n1024.0\n3.5\n3.0\n3.141592653589793\n"
    );
}

#[test]
fn parity_std_math_sqrt_negative_is_nan() {
    // math.sqrt of a negative is IEEE NaN — never faults, identical.
    let src = "import std.math\nfn main():\n    print(math.sqrt(0.0 - 1.0))\nmain()";
    assert_eq!(golden_entry(src), "NaN\n");
}

#[test]
fn parity_float_ieee_div_mod() {
    // Float division/modulo by zero is total IEEE-754: inf / NaN, never a fault.
    let src = "fn main():\n    print(1.0 / 0.0)\n    print(-1.0 / 0.0)\n    print(0.0 / 0.0)\n    print(5.0 % 0.0)\nmain()";
    assert_eq!(golden_entry(src), "inf\n-inf\nNaN\nNaN\n");
}

#[test]
fn parity_int_div_by_zero_still_faults() {
    // INTEGER division by zero still faults — caught + printed identically.
    let src = "fn run() -> int!:\n    r := recover:\n        1 / 0\n    match r:\n        ?v: return ?v\n        !e: print(e.message())\n    return ?0\nfn main():\n    _ := run()\nmain()";
    assert_eq!(golden_entry(src), "division by zero\n");
}

#[test]
fn parity_large_int_equality_is_exact() {
    // Two distinct i64 values above 2^53 must NOT compare equal (Python parity). Previously both
    // engines compared ints via `as_f64`, so 2^62+1 and 2^62+2 (both round to 2^62 in f64) wrongly
    // compared EQUAL. Cross-type `1 == 1.0` must STILL be true (int/float interop preserved).
    let src = "fn main():\n    a := 4611686018427387905\n    b := 4611686018427387906\n    print(a == b)\n    print(a != b)\n    print(a == a)\n    print(1 == 1.0)\n    print(2 == 3)\nmain()";
    assert_golden_out(src, "false\ntrue\ntrue\ntrue\nfalse\n");
}

#[test]
fn parity_large_int_map_keys_distinct() {
    // Two distinct large ints are distinct map keys (they were collapsed to one when eq was f64).
    // `1` and `1.0` still collapse to a single key (cross-type numeric key equality preserved).
    let src = "fn main():\n    m := {4611686018427387905: 1, 4611686018427387906: 2}\n    print(m.len())\n    n := {1: 10, 1.0: 20}\n    print(n.len())\nmain()";
    assert_golden_out(src, "2\n1\n");
}

#[test]
fn recover_tail_match_value_catches_fault_is_err() {
    // Must-not-break: even though the block is now value-typed, a fault raised BEFORE the tail-match
    // is still caught and converted to `Err` (single-Result stack invariant preserved).
    let src = "fn main():\n    r := recover:\n        xs := [1, 2]\n        y := xs[9]\n        match y:\n            _: 100\n    match r:\n        ?v: print(\"ok={v}\")\n        !e: print(\"err\")\nmain()";
    assert_golden_out(src, "err\n");
}

#[test]
fn recover_tail_match_heterogeneous_arms_run_parity() {
    // REGRESSION GUARD: heterogeneous tail-match arms (str vs int) fall back to `Result[nil]` at the
    // checker but the compiler still compiles the tail as a value — whichever arm runs, its value is
    // `Ok`-wrapped and simply IGNORED by `Ok(_)`. The program must check + RUN identically
    // (the first cut of the feature rejected it at `check`).
    let src = "fn foo(cmd: str):\n    r := recover:\n        match cmd:\n            \"a\": \"hello\"\n            _: 42\n    match r:\n        ?_: print(\"done\")\n        !e: print(\"failed\")\nfoo(\"a\")";
    assert_golden_out(src, "done\n");
}

#[test]
fn recover_tail_if_heterogeneous_branches_run_parity() {
    // The `if/else` analog of the heterogeneous fall-back: runs, value ignored.
    let src = "fn foo(n: int):\n    r := recover:\n        if n == 0:\n            \"zero\"\n        else:\n            n\n    match r:\n        ?_: print(\"done\")\n        !e: print(\"failed\")\nfoo(0)";
    assert_golden_out(src, "done\n");
}

#[test]
fn parity_std_math_predicates() {
    // is_nan / is_inf / is_finite — float predicates returning bool, identical.
    let src = "import std.math\nfn main():\n    print(math.is_nan(0.0 / 0.0))\n    print(math.is_inf(1.0 / 0.0))\n    print(math.is_finite(1.0))\n    print(math.is_finite(1.0 / 0.0))\nmain()";
    assert_eq!(golden_entry(src), "true\ntrue\ntrue\nfalse\n");
}

#[test]
fn parity_nan_ordered_compare_is_false() {
    // Ordered comparisons (`< <= > >=`) involving NaN are ALWAYS false — never a
    // fault — matching IEEE-754 / Python / Rust. Equality is untouched (`nan == nan` → false,
    // `nan != nan` → true). Normal float compares still work (regression guard).
    let src = "fn main():\n    nan := 0.0 / 0.0\n    print(nan < 1.0)\n    print(nan <= 1.0)\n    print(nan > 1.0)\n    print(nan >= 1.0)\n    print(1.0 < nan)\n    print(1.0 > nan)\n    print((1.0 / 0.0) < nan)\n    print(1.0 < 2.0)\n    print(2.0 > 1.0)\n    print(nan == nan)\n    print(nan != nan)\nmain()";
    assert_golden_out(
        src,
        "false\nfalse\nfalse\nfalse\nfalse\nfalse\nfalse\ntrue\ntrue\nfalse\ntrue\n",
    );
}

#[test]
fn parity_sort_by_key_nan_float_key_deterministic() {
    // `sort_by_key` with a float key that can be NaN sorts deterministically (total order, NaN to
    // one end) instead of faulting — consistent with `sort()`. The SIGN of `0.0/0.0` (hence
    // whether NaN ranks at the front or back) is platform-dependent (negative on x86 SSE2,
    // possibly positive elsewhere), so we do NOT bake an absolute position into a golden — that
    // would be a non-portable test. `assert_golden_out` proves the real guarantees portably: the sort
    // never faults and VM↔interp agree byte-identically on whatever order this machine produces.
    let src = "fn main():\n    xs := [1.0, 0.0 / 0.0, 2.0, 0.0 / 0.0, 0.5]\n    xs.sort_by_key(fn(x: float) -> float: x)\n    for v in xs:\n        print(v)\nmain()";
    assert_golden_out(src, "NaN\nNaN\n0.5\n1.0\n2.0\n");
}

#[test]
fn sort_and_sort_by_key_keep_signed_zeros_in_input_order() {
    // `-0.0 == +0.0`, so both orderings treat the pair as EQUAL and the stable sort keeps input
    // order — CPython `sorted([0.0, -0.0])` is `[0.0, -0.0]`. Observe the sign via `1.0/x`
    // (`inf` for `+0.0`, `-inf` for `-0.0`): input `[0.0, -0.0]` must print `inf` then `-inf`,
    // and input `[-0.0, 0.0]` must print `-inf` then `inf`. Platform-independent (no NaN sign).
    let by_sort = "fn main():\n    xs := [0.0, -1.0 * 0.0]\n    xs.sort()\n    for v in xs:\n        print(1.0 / v)\nmain()";
    assert_golden_out(by_sort, "inf\n-inf\n");
    let by_key = "fn main():\n    xs := [0.0, -1.0 * 0.0]\n    xs.sort_by_key(fn(x: float) -> float: x)\n    for v in xs:\n        print(1.0 / v)\nmain()";
    assert_golden_out(by_key, "inf\n-inf\n");
    let flipped = "fn main():\n    xs := [-1.0 * 0.0, 0.0]\n    xs.sort()\n    for v in xs:\n        print(1.0 / v)\nmain()";
    assert_golden_out(flipped, "-inf\ninf\n");
}

#[test]
fn parity_sort_by_key_normal_key_unchanged() {
    // Behavior-preserving guard: non-NaN float keys and int keys sort exactly as before — Part B
    // touches only the NaN float-key path.
    let fsrc = "fn main():\n    xs := [3.0, 1.0, 2.0]\n    xs.sort_by_key(fn(x: float) -> float: x)\n    for v in xs:\n        print(v)\nmain()";
    assert_golden_out(fsrc, "1.0\n2.0\n3.0\n");
    let isrc = "fn main():\n    xs := [3, 1, 2]\n    xs.sort_by_key(fn(x: int) -> int: x)\n    for v in xs:\n        print(v)\nmain()";
    assert_golden_out(isrc, "1\n2\n3\n");
}

#[test]
fn math_abs_min_overflows() {
    // `math.abs(i64::MIN)` has no representable result. Raw `i64::abs()` would panic (debug) or
    // wrap (release); the native fn must surface a recoverable overflow like every other op,
    // identically. i64::MIN is built as `-MAX - 1` (the literal
    // `9223372036854775808` overflows the lexer).
    let src = "import std.math\nfn main():\n    x := -9223372036854775807 - 1\n    print(math.abs(x))\nmain()";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let ie = run_file(&entry).2.unwrap_err().to_string();
    let ve = run_file(&entry).2.unwrap_err().to_string();
    assert_eq!(ie, ve, "abs-overflow error must be identical");
    assert!(ie.contains("integer overflow in abs"), "{ie}");
}

#[test]
fn math_abs_min_overflow_is_recoverable() {
    // The overflow is a normal recoverable fault: `recover:` turns it into an Err, not a crash.
    let src = "import std.math\nfn main():\n    x := -9223372036854775807 - 1\n    r := recover:\n        math.abs(x)\n    match r:\n        ?v: print(v)\n        !e: print(e.message())\nmain()";
    let out = golden_entry(src);
    assert!(out.contains("integer overflow in abs"), "{out}");
}

#[test]
fn exit_threads_code_through_both_engines() {
    // `std.os.exit(code)` halts the program with that exit code: output before
    // the call is preserved, the statement after it never runs, and the run is not an error.
    let src = "import std.os\nfn main():\n    print(\"before\")\n    os.exit(3)\n    print(\"after\")\nmain()";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (io, _ie, ir, ic) = run_file(&entry);
    let (vo, _ve, vr, vc) = run_file(&entry);
    assert_eq!(io, "before\n", "interp stdout");
    assert_eq!(vo, "before\n", "vm stdout");
    assert_eq!(ic, Some(3), "interp exit code");
    assert_eq!(vc, Some(3), "vm exit code");
    assert!(
        ir.is_ok() && vr.is_ok(),
        "exit is not a runtime error: interp={ir:?} vm={vr:?}"
    );
}

#[test]
fn defer_top_level_skipped_by_os_exit() {
    // `std.os.exit` is a hard halt — top-level defers do NOT run through it (matches Go's
    // `os.Exit`, and the existing frame/recover bypass).
    let src = "import std.os\nfn log(s: str):\n    print(s)\ndefer log(\"cleanup\")\nprint(\"before\")\nos.exit(2)\nprint(\"after\")\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (io, _ie, ir, ic) = run_file(&entry);
    let (vo, _ve, vr, vc) = run_file(&entry);
    assert_eq!(io, "before\n", "interp: cleanup defer skipped by os.exit");
    assert_eq!(vo, "before\n", "vm: cleanup defer skipped by os.exit");
    assert_eq!(ic, Some(2), "interp exit code");
    assert_eq!(vc, Some(2), "vm exit code");
    assert!(
        ir.is_ok() && vr.is_ok(),
        "exit is not a runtime error: interp={ir:?} vm={vr:?}"
    );
}

#[test]
fn exit_negative_code_masks_to_255() {
    // `os.exit(code)` reports the LOW 8 BITS of `code` (POSIX `exit(3)`/bash/Python/Go). A NEGATIVE
    // code used to be clamped UP to 0 — i.e. `os.exit(-1)`, the canonical failure idiom, reported
    // SUCCESS to the shell/CI. The process-status side of this rule is pinned by tests/exit_status.rs;
    // this pins the in-VM code (including the cross-thread re-store in sched.rs).
    for (code, want) in [("-1", 255), ("300", 44), ("-256", 0), ("0", 0)] {
        let src = format!("import std.os\nfn main():\n    os.exit({code})\nmain()");
        let t = TmpDir::new();
        let entry = t.write("main.chz", &src);
        let (_io, _ie, ir, ic) = run_file(&entry);
        let (_vo, _ve, vr, vc) = run_file(&entry);
        assert_eq!(ic, Some(want), "M:N exit code for os.exit({code})");
        assert_eq!(vc, Some(want), "serial exit code for os.exit({code})");
        assert!(
            ir.is_ok() && vr.is_ok(),
            "exit is a clean halt, not an error"
        );
    }
}

// ----- re-entrant `.next()` on a currently-running generator -----

#[test]
fn generator_reentrant_next_faults() {
    // A `.next()` on a generator that is ALREADY RUNNING (re-entered from inside its own body) used
    // to hit the `GenState::Done` placeholder the resume path parks in the heap object while the
    // generator runs, and silently answered `None` — a live, non-exhausted generator reporting
    // EXHAUSTION, i.e. a silently-wrong `Option`. It must be a clean, recoverable fault instead
    // (Python: `ValueError: generator already executing`).
    let src = r#"
holder: List[Iterator[int]] = []

fn gen() -> Iterator[int]:
    yield 1
    print("reentrant: {holder[0].next()}")
    yield 2

fn main():
    g := gen()
    holder.push(g)
    for x in g:
        print(x)
main()
"#;
    assert_eq!(
        vm_outcome(src).unwrap_err(),
        "runtime error (line 6, col 24): generator already running",
        "for:\n{src}"
    );
    let e = vm_outcome(src).expect_err("a re-entrant resume must fault, not answer None");
    assert!(e.contains("generator already running"), "{e}");
    // The value the consumer already pulled is neither lost nor duplicated, and the bogus
    // `reentrant: None` never prints. (`run_program` keeps stdout across the fault.)
    let (out, res) = run_program(src);
    assert!(res.is_err(), "the run faults");
    assert_eq!(
        out, "1\n",
        "only the first yielded value, then the fault: {out:?}"
    );
}

#[test]
fn generator_reentrancy_fault_is_recoverable() {
    // The fault is an ordinary RuntimeError: catchable by `recover:`, never a host panic. Also
    // fires when the re-entrancy is a `for` over the SAME generator (the for-loop intrinsic routes
    // through the same resume path as an explicit `.next()`).
    let src = r#"
holder: List[Iterator[int]] = []

fn gen() -> Iterator[int]:
    yield 1
    for y in holder[0]:
        print("never: {y}")
    yield 2

fn main():
    g := gen()
    holder.push(g)
    x := recover:
        for v in g:
            print(v)
        0
    match x:
        ?_: print("no fault")
        !e: print("caught: {e.message()}")
    print("still alive")
main()
"#;
    assert_golden_out(src, "1\ncaught: generator already running\nstill alive\n");
    let out = run_capture(src).expect("the fault is CAUGHT — the program exits Ok, no host panic");
    assert!(out.contains("caught:"), "the fault is recoverable: {out:?}");
    assert!(
        out.contains("generator already running"),
        "the recovered error names the cause: {out:?}"
    );
    assert!(
        out.ends_with("still alive\n"),
        "execution continues: {out:?}"
    );
    assert!(
        !out.contains("never:"),
        "the re-entrant loop never yields: {out:?}"
    );
}

#[test]
fn a_running_generator_sent_over_a_channel_is_the_same_generator() {
    // A generator sent WHILE it is running: `Channel.send` moves the generator itself, so the receiver resumes the
    // one generator after it yields: `2`, then exhausted. Go (`rewrites.go`): a stateful closure
    // sent while it runs is the same closure, `orig 1` then `copy 2 false`.
    let src = r#"
struct H:
    g: Iterator[int]?
h := H(None)
ch := Channel[H](1)
fn gen() -> Iterator[int]:
    ch.send(h)
    yield 1
    yield 2
g := gen()
h.g = ?g
print("orig", g.next())
got := ch.recv() else: panic("ch closed")
match got.g:
    ?x: print("copy", x.next(), x.next())
    None: print("none")
"#;
    assert_golden_out(src, "orig 1\ncopy 2 None\n");
}

#[test]
fn a_running_generator_stored_in_an_atomic_is_the_same_generator() {
    // TICKET-041(a) / M3.2, second vehicle: `Atomic.store` holds the generator itself, exactly as
    // the channel vehicle above. Go (`rewrites.go`): an `atomic.Value` store, `orig 1` then
    // `copy 2 false`.
    let src = r#"
import Atomic from std.concurrency
struct H:
    g: Iterator[int]?
h := H(None)
a := Atomic(H(None))
fn gen() -> Iterator[int]:
    a.store(h)
    yield 1
    yield 2
g := gen()
h.g = ?g
print("orig", g.next())
match a.load().g:
    ?x: print("copy", x.next(), x.next())
    None: print("none")
"#;
    assert_golden_out(src, "orig 1\ncopy 2 None\n");
}
