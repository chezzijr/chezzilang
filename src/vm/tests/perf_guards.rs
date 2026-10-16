//! VM tests: list methods, assert, M19 interning, FxHash, index specialization, inline caches,
//! quickening, polymorphic IC, native fn values.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

// ---- gaps.md §2 wave-1: List value/ergonomics methods ----

/// `remove_at` on a true out-of-range index faults byte-identically.
#[test]
fn remove_at_oob_faults_both_engines() {
    let src = "xs := [1, 2, 3]\nprint(\"before\")\nxs.remove_at(9)\n";
    assert_fault_parity(src, "before\n");
    assert_eq!(run_err(src), "index 9 out of bounds (len 3)");
}

/// `min`/`max` on an empty list answer `None` — NOT a fault. They return `Option[T]` like the
/// sibling `first`/`last`/`pop` (Rust's `Iterator::min`), so the program runs to completion.
#[test]
fn min_max_empty_is_none() {
    let src = "xs: List[int] = []\nprint(\"before\")\nprint(xs.min() == None)\nprint(xs.max() == None)\nprint(xs.min() ?? -1)\n";
    assert_mc_parity(src, "before\ntrue\ntrue\n-1\n");
}

/// `min_by`/`max_by` take a key extractor and return `Some(<the ELEMENT with the extremal key>)`
/// (first-seen tie).
#[test]
fn list_min_max_by_parity() {
    let src = "struct P:\n    k: int\n    tag: str\n\
               xs := [P(2, \"a\"), P(1, \"b\"), P(1, \"c\"), P(3, \"d\")]\n\
               fn key(p: P) -> int:\n    return p.k\n\
               match xs.min_by(key):\n    ?lo: print(lo.tag)\n    None: print(\"none\")\n\
               match xs.max_by(key):\n    ?hi: print(hi.tag)\n    None: print(\"none\")\n";
    assert_mc_parity(src, "b\nd\n");
}

/// …and `min_by`/`max_by` on an empty list answer `None` too, with no fault.
#[test]
fn min_max_by_empty_is_none() {
    let src = "xs: List[int] = []\nfn key(x: int) -> int:\n    return x\nprint(\"before\")\nprint(xs.min_by(key) == None)\nprint(xs.max_by(key) ?? -1)\n";
    assert_mc_parity(src, "before\ntrue\n-1\n");
}

/// `min`/`max` scan a SNAPSHOT: a Comparable-struct element's `compare` that SHRINKS the receiver
/// mid-scan must not index past the live list's new length (no OOB panic), matching `sort`/`min_by`.
/// Regression for `list_reduce_extreme` re-indexing the live source after user code mutated it.
#[test]
fn min_max_shrinking_comparator_no_panic() {
    // `compare` shrinks the module-global list it is invoked on (the receiver) each comparison; the
    // snapshot scan still visits all three original elements → min by x = 1,.
    let min_src = "struct Point:\n    x: int\n    fn compare(self, other: Point) -> int:\n        pts.remove_at(0)\n        return self.x - other.x\n    fn eq(self, other: Point) -> bool:\n        return self.x == other.x\n\
               pts: List[Point] = [Point(3), Point(1), Point(2)]\n\
               match pts.min():\n    ?p: print(p.x)\n    None: print(\"none\")\n";
    assert_mc_parity(min_src, "1\n");
    // Same for `max` (same `list_reduce_extreme` scan, is_max flipped) → max by x = 3.
    let max_src = "struct Q:\n    x: int\n    fn compare(self, other: Q) -> int:\n        qs.remove_at(0)\n        return self.x - other.x\n    fn eq(self, other: Q) -> bool:\n        return self.x == other.x\n\
               qs: List[Q] = [Q(3), Q(1), Q(2)]\n\
               match qs.max():\n    ?q: print(q.x)\n    None: print(\"none\")\n";
    assert_mc_parity(max_src, "3\n");
}

// ---- gaps.md §2 wave-2: List iter-ergonomics (unique/dedup/chunk/windows/take_while/drop_while/count/position) ----

/// `chunk`/`windows` with `n <= 0` fault byte-identically with a clear message.
#[test]
fn list_chunk_windows_bad_n_faults() {
    let c = "xs := [1, 2]\nprint(\"before\")\nxs.chunk(0)\n";
    assert_fault_parity(c, "before\n");
    assert_eq!(run_err(c), "chunk size must be positive, got 0");
    let w = "xs := [1, 2]\nprint(\"before\")\nxs.windows(-1)\n";
    assert_fault_parity(w, "before\n");
    assert_eq!(run_err(w), "window size must be positive, got -1");
}

/// The 4 predicate methods scan a SNAPSHOT: a predicate that SHRINKS the receiver mid-scan must not
/// index past the live list's new length (no OOB panic), matching `map`/`filter`/`min`. The predicate
/// sees the full original-length snapshot.
#[test]
fn list_predicate_shrinking_no_panic() {
    // count: pred pops the receiver on the first element; snapshot still visits all 4 → all x>0 → 4.
    let c = "xs := [1, 2, 3, 4]\nfn f(x: int) -> bool:\n    if x == 1:\n        xs.pop()\n    return x > 0\nprint(xs.count(f))\n";
    assert_mc_parity(c, "4\n");
    // position: pred shrinks the receiver; snapshot scan finds index 2 without OOB.
    let p = "xs := [1, 2, 3, 4]\nfn f(x: int) -> bool:\n    xs.pop()\n    return x == 3\nfn showpos(o: int?) -> int:\n    match o:\n        ?i: return i\n        None: return -1\nprint(showpos(xs.position(f)))\n";
    assert_mc_parity(p, "2\n");
    // take_while: pred shrinks the receiver; snapshot scan is unaffected.
    let t = "xs := [1, 2, 3, 4]\nfn f(x: int) -> bool:\n    xs.pop()\n    return x < 3\nprint(xs.take_while(f))\n";
    assert_mc_parity(t, "[1, 2]\n");
}

// ---- list HOF: callback that shrinks the receiver (snapshot semantics) ----

/// `map` iterates a SNAPSHOT of the receiver's elements at call time: a callback that pops the
/// receiver mid-iteration must not perturb the iteration sequence (no OOB panic), matching the
/// interpreter, comprehensions, and Python `map`. Regression for the OOB at list_hof's index.
#[test]
fn map_shrinking_callback_no_panic() {
    let src = "xs := [1, 2, 3, 4, 5]\nfn f(x: int) -> int:\n    xs.pop()\n    return x * 2\nys := xs.map(f)\nprint(ys)\n";
    assert_eq!(run(src), "[2, 4, 6, 8, 10]\n");
}

/// `filter` over a snapshot: a predicate that shrinks the receiver still tests every original
/// element. Original 1..5 → evens kept → `[2, 4]`.
#[test]
fn filter_shrinking_callback_no_panic() {
    let src = "xs := [1, 2, 3, 4, 5]\nfn p(x: int) -> bool:\n    xs.pop()\n    return x % 2 == 0\nprint(xs.filter(p))\n";
    assert_eq!(run(src), "[2, 4]\n");
}

/// `fold` over a snapshot: a callback that shrinks the receiver still folds all original
/// elements. Sum of 1..5 = 15.
#[test]
fn fold_shrinking_callback_no_panic() {
    let src = "xs := [1, 2, 3, 4, 5]\nfn g(acc: int, x: int) -> int:\n    xs.pop()\n    return acc + x\nprint(xs.fold(0, g))\n";
    assert_eq!(run(src), "15\n");
}

// ---- assert (Phase A) ----

#[test]
fn assert_true_does_not_fault() {
    assert_eq!(run("assert true\nprint(\"ok\")\n"), "ok\n");
    assert_eq!(run("assert 1 == 1, \"never\"\nprint(\"ok\")\n"), "ok\n");
}

#[test]
fn assert_false_faults_with_custom_msg_and_line() {
    // The assert is on line 2; the fault carries that line and the FORMATTED custom message
    // (`assertion failed: <msg>`), giving the failure context Python's `assert` does.
    let err = run_capture("print(\"a\")\nassert false, \"boom\"\n").unwrap_err();
    assert_eq!(err.message, "assertion failed: boom");
    assert_eq!(err.span.line, 2);
}

#[test]
fn assert_true_with_msg_does_not_evaluate_msg() {
    // A passing assert must NOT evaluate its message expression (laziness): the program runs on.
    assert_eq!(run("assert true, \"never\"\nprint(\"ok\")\n"), "ok\n");
}

#[test]
fn assert_false_default_message() {
    let err = run_capture("assert false\n").unwrap_err();
    assert_eq!(err.message, "assertion failed");
    assert_eq!(err.span.line, 1);
}

#[test]
fn assert_comparison_failure_shows_both_operands() {
    let err = run_capture("xs := [1, 2, 3]\nassert xs.len() == 4\n").unwrap_err();
    assert_eq!(err.message, "assertion failed: 3 == 4");
    assert_eq!(err.span.line, 2);
    assert!(err.is_assert);
}

#[test]
fn assert_comparison_failure_keeps_custom_msg() {
    let err = run_capture("x := 1\nassert x == 2, \"x must be two\"\n").unwrap_err();
    assert_eq!(err.message, "assertion failed: x must be two (1 == 2)");
}

#[test]
fn assert_comparison_renders_every_operator() {
    let cases = [
        ("assert 2 < 1\n", "assertion failed: 2 < 1"),
        ("assert 2 <= 1\n", "assertion failed: 2 <= 1"),
        ("assert 1 > 2\n", "assertion failed: 1 > 2"),
        ("assert 1 >= 2\n", "assertion failed: 1 >= 2"),
        ("assert 1 != 1\n", "assertion failed: 1 != 1"),
        (
            "assert \"ab\" == \"ac\"\n",
            "assertion failed: 'ab' == 'ac'",
        ),
    ];
    for (src, expect) in cases {
        assert_eq!(run_capture(src).unwrap_err().message, expect, "src: {src}");
    }
}

#[test]
fn assert_non_comparison_message_unchanged() {
    assert_eq!(
        run_capture("assert 4 in [1, 2, 3]\n").unwrap_err().message,
        "assertion failed"
    );
    assert_eq!(
        run_capture("assert false, \"boom\"\n").unwrap_err().message,
        "assertion failed: boom"
    );
}

// ---- M19 Phase 3: ConstStr interning + per-char alloc (correctness guards) ----

#[test]
fn interned_literal_repeated_pushes_render_identically() {
    // The same literal op pushed many times must render identically — interning must not change
    // the observed value (no identity operator exists, so aliasing is invisible).
    assert_eq!(
        run("i := 0\nwhile i < 3:\n    print(\"hi\")\n    i = i + 1\n"),
        "hi\nhi\nhi\n"
    );
}

#[test]
fn interned_fstring_literal_parts_in_loop() {
    // Interpolation literal chunks (`n=` / `!`) are ConstStr pushes repeated per iteration.
    assert_eq!(
        run("i := 0\nwhile i < 3:\n    print(\"n={i}!\")\n    i = i + 1\n"),
        "n=0!\nn=1!\nn=2!\n"
    );
}

#[test]
fn interned_literal_as_map_key_repeated() {
    // A literal reused as a map key: aliasing must preserve structural (by-content) map lookup.
    assert_eq!(
        run("m := {}\ni := 0\nwhile i < 3:\n    m[\"k\"] = i\n    i = i + 1\nprint(m[\"k\"])\n"),
        "2\n"
    );
}

#[test]
fn interned_strings_survive_gc_stress() {
    // Proves interned ConstStr objects are GC-rooted: collect-before-every-instruction must not
    // sweep a cached literal out from under a later push of the same op.
    let src = "i := 0\nout := \"\"\nwhile i < 50:\n    out = out + \"x\"\n    i = i + 1\nprint(out.len())\n";
    assert_eq!(run_capture_stress(src), run(src));
    assert_eq!(run(src), "50\n");
}

#[test]
fn path_values_survive_gc_stress() {
    // TICKET-215: the `fn_values` memo is a GC root — collect-before-every-instruction must not sweep
    // the one `P.mk` object, or the reused slot (here `xs` / `ys`) answers a later read. The
    // value goes through a helper so no local keeps it alive (`(P.mk)(i)` lowers to a direct call).
    let src = "struct P:\n    n: int\n    fn mk(n: int) -> P:\n        return P(n)\nenum E:\n    A(int)\n    B\nfn mkn(f: fn(int) -> P, n: int) -> int:\n    return f(n).n\nfn len1(f: fn(int) -> E, n: int) -> int:\n    return [f(n)].len()\ni := 0\ntotal := 0\nwhile i < 20:\n    a := mkn(P.mk, i)\n    xs := [i, i + 1]\n    b := len1(fn(n: int) -> E: E.A(n), i)\n    ys := [i]\n    total = total + a + xs[0] + b + ys.len() - 1\n    i = i + 1\nprint(total)\n";
    assert_eq!(run_capture_stress(src), "400\n");
}

#[test]
fn per_char_sites_render_unchanged() {
    // `for c in str`, string indexing, `chars()`, and `chr()` all build 1-char strs via the
    // single-allocation helper — output must stay byte-identical (same UTF-8).
    assert_eq!(
        run("for c in \"héllo\":\n    print(c)\n"),
        "h\né\nl\nl\no\n"
    );
    assert_eq!(run("s := \"héllo\"\nprint(s[1])\n"), "é\n");
    assert_eq!(
        run("for c in \"abc\".chars():\n    print(c)\n"),
        "a\nb\nc\n"
    );
    assert_eq!(run("print(chr(233))\n"), "é\n");
}

// ---- M19: FxHash map/set index hasher (correctness guards) ----
// The map/set `index` (cached-hash → positions) and `str_intern` swap SipHash for a cheap FxHash.
// The hasher only picks buckets; `values_equal` confirms every probe, so behavior must not change.

#[test]
fn fxhash_map_int_keys_insert_lookup_remove() {
    // Int keys hash straight to f64 bits, so this exercises only the index BuildHasher. Insert,
    // read, remove (rebuilds the index), then re-insert — all must agree with the interpreter.
    let src = "m := {}\ni := 0\nwhile i < 50:\n    m[i] = i * 2\n    i = i + 1\n\
                   m.remove(10)\nm.remove(20)\nm[10] = 999\n\
                   acc := 0\nfor k in m.keys():\n    acc = acc + m[k]\nprint(acc)\nprint(m.len())\n";
    // sum(2i, i in 0..50) = 2450; drop 20→-40, drop10 then re-add 10→999: 2450-20-40+999 = 3389
    assert_eq!(run_parity(src), "3389\n49\n");
}

#[test]
fn fxhash_map_str_keys() {
    // String keys hash by content (DefaultHasher, unchanged) then route through the index
    // BuildHasher (changed). Repeated-key updates must still land in the same entry.
    let src = concat!(
        "counts := {}\n",
        "for w in [\"a\", \"b\", \"a\", \"c\", \"a\", \"b\"]:\n",
        "    if counts.contains(w):\n",
        "        counts[w] = counts[w] + 1\n",
        "    else:\n",
        "        counts[w] = 1\n",
        "for k in counts.keys():\n",
        "    print(\"{k}={counts[k]}\")\n",
    );
    assert_eq!(run_parity(src), "a=3\nb=2\nc=1\n");
}

#[test]
fn fxhash_constant_hash_collision_still_resolves() {
    // A struct key whose hash() is constant forces every key into ONE index bucket. The probe
    // must still find the right entry via structural ==, regardless of the bucket hasher.
    let src = "struct K:\n    v: int\n    fn hash(self) -> int:\n        return 7\n\
                   m := {}\ni := 0\nwhile i < 30:\n    m[K(i)] = i\n    i = i + 1\n\
                   print(m[K(7)])\nprint(m[K(29)])\nprint(m.contains(K(30)))\nprint(m.len())\n";
    assert_eq!(run_parity(src), "7\n29\nfalse\n30\n");
}

#[test]
fn dense_index_collision_upgrade_parity() {
    // Two DISTINCT struct keys whose hash() is the same constant land in ONE index bucket. The
    // dense index must UPGRADE that bucket from a single inline position to hold BOTH positions
    // (the `Many` path) so each key reads back distinctly, and an absent third constant-hash key
    // still misses. A One-only / single-slot index would drop the second key and diverge.
    let src = "struct K:\n    v: int\n    fn hash(self) -> int:\n        return 42\n\
                   m := {}\nm[K(1)] = \"one\"\nm[K(2)] = \"two\"\n\
                   print(m[K(1)])\nprint(m[K(2)])\nprint(m.contains(K(3)))\nprint(m.len())\n";
    assert_eq!(run_parity(src), "one\ntwo\nfalse\n2\n");
}

#[test]
fn fxhash_set_dedup_and_ops() {
    // Set dedup + union/intersection/difference over the index hasher.
    let src = "a := Set([1, 2, 3, 2, 1])\nb := Set([3, 4, 5])\n\
                   print(a.len())\nprint(a.union(b).len())\nprint(a.intersection(b).len())\nprint(a.difference(b).len())\n";
    assert_eq!(run_parity(src), "3\n5\n1\n2\n");
}

// ---- M19 Tier-2: index-access specialization (behavior-preserving guards) ----
// The Int-key fast path in `get_index`/`set_index` (skips the rooting that protects a struct
// key's re-entrant hash) and the inline `GetIndex`/`SetIndex` dispatch are VM-only speedups, so
// every result + error string must stay byte-identical to what the general (pre-fast-path) match
// produces. `idx_parity` asserts the run against a literal `expected`: `Ok(stdout)` for a clean
// run, `Err(msg)` for a runtime fault (compared against the fault's `.message`, not its
// span-carrying `Display`, since the span isn't the contract being pinned here).
fn idx_parity(src: &str, expected: Result<&str, &str>) {
    match (run_capture(src), expected) {
        (Ok(out), Ok(want)) => assert_eq!(out, want),
        (Err(e), Err(want)) => assert_eq!(e.message, want),
        (Ok(out), Err(want)) => panic!("expected fault {want:?}, got ?{out:?}"),
        (Err(e), Ok(want)) => panic!("expected ?{want:?}, got fault {e:?}"),
    }
}

#[test]
fn idxspec_int_map_get_hit_and_miss() {
    // Int-key map read: a present key returns its value; an absent key faults "key not found".
    idx_parity(
        "m := {1: 10, 2: 20}\nprint(m[1])\nprint(m[2])\n",
        Ok("10\n20\n"),
    );
    idx_parity("m := {1: 10}\nprint(m[99])\n", Err("key not found: 99"));
}

#[test]
fn idxspec_int_map_set_overwrite_and_insert() {
    // Int-key map write: overwrite an existing entry, insert a new one; len + reads agree.
    idx_parity(
        "m := {1: 10}\nm[1] = 11\nm[2] = 20\nprint(m[1])\nprint(m[2])\nprint(m.len())\n",
        Ok("11\n20\n2\n"),
    );
}

#[test]
fn idxspec_int_list_get_set_in_bounds() {
    idx_parity(
        "xs := [5, 6, 7]\nprint(xs[0])\nxs[2] = 99\nprint(xs[2])\n",
        Ok("5\n99\n"),
    );
}

#[test]
fn idxspec_list_out_of_bounds_message_exact() {
    // Both get and set must surface the exact same bounds message through the fast path's fallback.
    idx_parity(
        "xs := [1, 2, 3]\nprint(xs[5])\n",
        Err("index 5 out of bounds (len 3)"),
    );
    idx_parity(
        "xs := [1, 2, 3]\nxs[5] = 0\n",
        Err("index 5 out of bounds (len 3)"),
    );
    // Negative indexing counts from the end (Python parity, docs/syntax.md); -1 on a 3-element
    // list is IN range (the last element), not a fault — verified against CPython.
    idx_parity("xs := [1, 2, 3]\nprint(xs[-1])\n", Ok("3\n"));
}

#[test]
fn idxspec_non_int_map_keys_via_fallback() {
    // Str + bool keys must NOT take the Int fast path — they route through the unchanged general
    // match (content/scalar hash). Output + a str-key miss message stay identical.
    idx_parity(
        "m := {\"a\": 1, \"b\": 2}\nprint(m[\"a\"])\nprint(m[\"b\"])\n",
        Ok("1\n2\n"),
    );
    idx_parity(
        "m := {true: 1, false: 0}\nprint(m[false])\nprint(m[true])\n",
        Ok("0\n1\n"),
    );
    idx_parity(
        "m := {\"a\": 1}\nprint(m[\"z\"])\n",
        Err("key not found: 'z'"),
    );
}

#[test]
fn idxspec_struct_index_protocol_via_fallback() {
    // THE TRAP: an Int key on a struct receiver must dispatch the `index`/`set_index` protocol,
    // NOT the List/Map Int fast path. The receiver kind (Struct) gates the fast path, not the key.
    let src = "struct Buf:\n    xs: List[int]\n    fn index(self, k: int) -> int:\n        return self.xs[k]\n    fn set_index(self, k: int, v: int):\n        self.xs[k] = v\n\
                   b := Buf([10, 20, 30])\nprint(b[0])\nb[1] = 99\nprint(b[1])\n";
    idx_parity(src, Ok("10\n99\n"));
}

#[test]
fn idxspec_int_float_key_collision_resolves() {
    // Int(3) and Float(3.0) hash identically (3.0.to_bits()) and are values_equal. The fast path
    // shortcuts only the HASH, never the candidates+values_equal probe, so a Float key inserted as
    // 3.0 is found by m[3] and vice-versa.
    //
    // DELIBERATE VM-level-only pin, not reachable through `chezzi run` — `idx_parity` calls
    // `run_capture`, which compiles via `compile_module_standalone` and so SKIPS the checker
    // (mod.rs:4693). That matters here because the real product can never build this map: `float`
    // does not implement `Hashable` (`float_map_key_annotation_rejected`, checker/tests.rs:9053,
    // rejects `Map[float, int]` itself with "must implement Hashable"), and even setting that aside,
    // indexing an int-keyed map with a float key is its own rejection
    // (`map_index_wrong_key_type_rejected`, checker/tests.rs:9047, "key"). Measured: `chezzi run` on
    // this exact source produces 3 "map key must be int, found float" errors. So no Chezzi-legal
    // program can ever put an Int key and a colliding Float key in the same map — this test exists
    // purely to pin the Rust-level hash-bucket invariant (same hash bits =/=> skip the eq probe)
    // that the fast path must preserve if that constraint is ever relaxed.
    idx_parity(
        "m := {}\nm[3] = \"int\"\nprint(m[3.0])\nm[3.0] = \"float\"\nprint(m[3])\nprint(m.len())\n",
        Ok("int\nfloat\n1\n"),
    );
}

// ---- M19 Phase 4: struct-field inline cache (correctness guards) ----

#[test]
fn nan_format_spec_sign_parity() {
    // A negative-signed NaN (0.0/0.0) must render `NaN` (not `-NaN`) through the format-spec
    // path on BOTH engines, matching the bare stringify path. Infinities keep their sign.
    let src = "n := 0.0 / 0.0\n\
                   print(\"{n:.2f}\")\nprint(\"{n:f}\")\nprint(\"{n:e}\")\n\
                   ninf := -1.0 / 0.0\nprint(\"{ninf:.2f}\")\n\
                   pinf := 1.0 / 0.0\nprint(\"{pinf:.2f}\")\n";
    assert_eq!(run_parity(src), "NaN\nNaN\nNaN\n-inf\ninf\n");
}

#[test]
fn neg_int_pattern_runtime_parity() {
    // A negative int literal pattern matches the negative value and nothing else; a
    // negative-bounded range pattern is half-open (`-10 <= v < -5`). byte-identical against the golden.
    let src = "fn classify(x: int) -> str:\n\
                   \x20   match x:\n\
                   \x20       -3: return \"neg3\"\n\
                   \x20       -10..-5: return \"lo\"\n\
                   \x20       _: return \"other\"\n\
                   print(classify(-3))\n\
                   print(classify(-7))\n\
                   print(classify(-5))\n\
                   print(classify(-10))\n\
                   print(classify(3))\n";
    assert_eq!(run_parity(src), "neg3\nlo\nother\nlo\nother\n");
}

#[test]
fn neg_pattern_with_guard_and_or_parity() {
    // Negatives compose with guards and or-patterns; this holds.
    let src = "fn f(x: int, flag: bool) -> str:\n\
                   \x20   match x:\n\
                   \x20       -3 if flag: return \"g\"\n\
                   \x20       -1 | -2: return \"or\"\n\
                   \x20       _: return \"_\"\n\
                   print(f(-3, true))\n\
                   print(f(-3, false))\n\
                   print(f(-1, false))\n\
                   print(f(-2, true))\n\
                   print(f(0, true))\n";
    assert_eq!(run_parity(src), "g\n_\nor\nor\n_\n");
}

#[test]
fn ic_deep_field_read() {
    // Read the LAST field of a 6-field struct in a loop: exercises the IC hit path past five
    // would-be name-probes. Cached idx must point at `f` every iteration.
    let src = "struct S:\n    a: int\n    b: int\n    c: int\n    d: int\n    e: int\n    f: int\n\
                   s := S(1, 2, 3, 4, 5, 6)\n\
                   i := 0\nacc := 0\nwhile i < 5:\n    acc = acc + s.f\n    i = i + 1\nprint(acc)\n";
    assert_eq!(run_parity(src), "30\n");
}

#[test]
fn ic_field_write_then_read() {
    // SetField IC: mutate `x` (plain) and `y` (compound) in a loop, then read both back. The
    // write cache and the read cache must agree on the field index.
    let src = "struct P:\n    x: int\n    y: int\n\
                   p := P(0, 0)\n\
                   i := 0\nwhile i < 4:\n    p.x = p.x + 2\n    p.y += 3\n    i = i + 1\n\
                   print(p.x)\nprint(p.y)\n";
    assert_eq!(run_parity(src), "8\n12\n");
}

#[test]
fn ic_distinct_layouts() {
    // Two structs whose shared field names sit at DIFFERENT indices (A{x,y} vs B{y,x}), read at
    // their own sites in one loop. A bug that confused per-site IC cells (bad id allocation, or a
    // hit that skipped the name re-verify) would return a wrong field; the verify keeps it sound.
    let src = "struct A:\n    x: int\n    y: int\nstruct B:\n    y: int\n    x: int\n\
                   a := A(1, 2)\nb := B(3, 4)\n\
                   i := 0\ns := 0\nwhile i < 3:\n    s = s + a.x + a.y + b.x + b.y\n    i = i + 1\nprint(s)\n";
    // per iter: a.x=1 a.y=2 b.x=4 b.y=3 => 10; *3 = 30
    assert_eq!(run_parity(src), "30\n");
}

#[test]
fn ic_self_field_method() {
    // `self.field` reads inside a method called in a loop — the hot OO path the IC targets.
    let src = "struct Counter:\n    n: int\n\n    fn get(self) -> int:\n        return self.n\n\
                   c := Counter(7)\n\
                   i := 0\nacc := 0\nwhile i < 5:\n    acc = acc + c.get()\n    i = i + 1\nprint(acc)\n";
    assert_eq!(run_parity(src), "35\n");
}

#[test]
fn ic_struct_under_parallel_engine() {
    // The IC lives on each worker `Vm` too; field-heavy code run on the real-thread engine must
    // produce the same output as the cooperative engine (the caches are per-Vm, self-verifying).
    let src = "struct Pt:\n    x: int\n    y: int\n\n    fn sum(self) -> int:\n        return self.x + self.y\n\
                   p := Pt(3, 4)\n\
                   acc := 0\ni := 0\nwhile i < 100:\n    acc = acc + p.sum()\n    p.x = p.x + 1\n    i = i + 1\n\
                   print(acc)\nprint(p.x)\n";
    // Hand-derived: p.x reads 3, 4, 5, .., 102 across the 100 iterations (incremented AFTER each
    // sum), so acc = sum_{k=0..99} ((3+k)+4) = 100*7 + (0+..+99) = 700 + 4950 = 5650; p.x ends at
    // 3+100 = 103. Pure Chezzi-arithmetic, no ancestor semantics involved.
    assert_eq!(run_capture(src).expect("parallel"), "5650\n103\n");
}

#[test]
fn ic_gc_stress_fields() {
    // Field reads under collect-before-every-instruction: cached indices stay valid because GC
    // never reorders a struct's `fields` Vec (and the IC holds indices, not GcRefs).
    let src = "struct V:\n    a: int\n    b: int\n    c: int\n\
                   v := V(10, 20, 30)\n\
                   i := 0\nacc := 0\nwhile i < 30:\n    acc = acc + v.a + v.b + v.c\n    i = i + 1\nprint(acc)\n";
    assert_eq!(run_capture_stress(src), run(src));
    assert_eq!(run_parity(src), "1800\n");
}

// ---- M19 Phase 5b: struct type-id guard on the field IC (correctness guards) ----
// The IC hit now guards on a numeric `tid` (== struct layout identity) instead of re-verifying
// the field name string. Soundness rests on: every distinct layout has a distinct `tid`, and the
// empty/sentinel `tid` never matches. These lock that behavior is unchanged.

#[test]
fn typeid_guard_distinct_layouts_keep_distinct_values() {
    // Same field names on two types at SWAPPED indices, read in a hot loop. With the tid guard,
    // each per-type site caches (tid, idx); a guard that ignored type identity (or stamped a
    // shared tid) would read the wrong slot. Values asserted, not just a sum, to pin the layout.
    let src = concat!(
        "struct A:\n    v: int\n    w: int\n",
        "struct B:\n    w: int\n    v: int\n",
        "a := A(1, 2)\n",
        "b := B(3, 4)\n",
        "i := 0\nout := 0\n",
        "while i < 4:\n    out = out + a.v * 1000 + a.w * 100 + b.v * 10 + b.w\n    i = i + 1\n",
        // per iter: a.v=1,a.w=2,b.v=4,b.w=3 -> 1000+200+40+3 = 1243 ; *4 = 4972
        "print(out)\n",
    );
    assert_eq!(run_parity(src), "4972\n");
}

#[test]
fn typeid_guard_struct_round_trips_through_channel() {
    // A struct sent across a Channel is serialized (to_wire) and rebuilt (from_wire) in the
    // receiver; from_wire must stamp a `tid` so the receiver's field IC stays sound. VM-only
    // (channels don't run under the frozen interp), so assert the VM output directly.
    let src = concat!(
        "struct Pt:\n    x: int\n    y: int\n",
        "fn worker(ch: Channel[Pt]):\n    p := ch.recv()\n    print(\"{p.x} {p.y}\")\n",
        "fn sender(ch: Channel[Pt]):\n    ch.send(Pt(10, 20))\n",
        "fn main():\n    ch := Channel[Pt]()\n    parallel:\n        spawn worker(ch)\n        spawn sender(ch)\n",
        "main()\n",
    );
    assert_eq!(run(src), "10 20\n");
}

// ---- M19 Tier-2: adaptive opcode quickening (PEP 659), v1 — binops — correctness guards ----
// The un-fused generic binop arms (`Add..GtEq` reached by stack operands; `Eq`/`NotEq` always)
// specialize to an int/int fast path behind a per-`Vm`, per-site (proto,ip) deopt guard. The
// side table holds only state bytes (no `GcRef`), so it is heap-independent — never swapped in
// `swap_ctx`, like `field_ic`/`method_ic`. Behaviour is byte-identical to the generic path.
// These guard the gotchas.

#[test]
fn free_test_fn_is_tagged_and_recorded() {
    // A free `test fn` is recorded in `program.tests` and its proto is tagged `is_test`; an
    // ordinary `fn` is neither.
    let src = "test fn t():\n    assert true\nfn helper():\n    return\n";
    let module = parser::parse(lexer::tokenize(src).unwrap()).unwrap();
    let program = crate::compiler::compile_module_standalone(&module).unwrap();
    assert_eq!(program.tests.len(), 1, "exactly one free test recorded");
    let (name, pid) = &program.tests[0];
    assert_eq!(name, "t");
    assert!(
        program.protos[*pid].is_test,
        "the test proto is tagged is_test"
    );
    // The `helper` proto is not a test.
    assert!(
        program.protos.iter().filter(|p| p.is_test).count() == 1,
        "only the test fn is tagged"
    );
}

#[test]
fn suite_is_discovered_with_thunk_and_methods() {
    // A struct with a `test fn` method is a suite with a `__new_` thunk + recorded test methods.
    let src = "struct S:\n    n: int = 7\n    test fn a(self):\n        assert self.n == 7\n    fn before_each(self):\n        return\n";
    let module = parser::parse(lexer::tokenize(src).unwrap()).unwrap();
    let program = crate::compiler::compile_module_standalone(&module).unwrap();
    assert_eq!(program.suites.len(), 1, "one suite discovered");
    let s = &program.suites[0];
    assert_eq!(s.name, "S");
    assert_eq!(
        s.tests.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        vec!["a"]
    );
    assert!(
        s.hooks.contains_key("before_each"),
        "before_each hook recorded"
    );
    // The thunk proto exists and the struct records its test methods. ROOT REDESIGN — the struct
    // table is keyed by the qualified IDENTITY KEY (`<main>::S` on the standalone path).
    assert!(s.new_thunk < program.protos.len());
    assert_eq!(
        program.structs["<main>::S"].test_methods,
        vec!["a".to_string()]
    );
}

#[test]
fn quicken_table_presized_and_based() {
    // White-box wiring: the per-`Vm` quicken side table has one state byte per program
    // instruction, and `quicken_base` is the prefix sum of per-proto code lengths so a site is
    // `quicken_base[pid] + ip` (mirrors `field_ic_sites`/`field_ic` presizing).
    let src = "i := 0\ntotal := 0\nwhile i < 5:\n    total = total + i * i\n    i = i + 1\nprint(total)\n";
    let tokens = lexer::tokenize(src).unwrap();
    let module = parser::parse(tokens).unwrap();
    let program = crate::compiler::compile_module_standalone(&module).unwrap();
    let vm = Vm::new(Arc::new(program.clone()));
    let total: usize = program.protos.iter().map(|p| p.code.len()).sum();
    assert_eq!(vm.quicken.len(), total, "one quicken cell per instruction");
    assert_eq!(
        vm.quicken_base.len(),
        program.protos.len(),
        "one base per proto"
    );
    // prefix-sum invariant: base[0]==0, base[k+1]==base[k]+len(proto[k])
    let mut acc = 0u32;
    for (pid, p) in program.protos.iter().enumerate() {
        assert_eq!(
            vm.quicken_base[pid], acc,
            "base[{pid}] is the running prefix sum"
        );
        acc += p.code.len() as u32;
    }
    // all cells start Cold (0)
    assert!(vm.quicken.iter().all(|&b| b == 0), "every site starts Cold");
}

#[test]
fn quicken_eq_uses_exact_i64_semantics() {
    // The quickened int `Eq`/`NotEq` fast path compares EXACT i64 (`x == y`), matching
    // `values_equal_guarded`'s exact `(Int,Int)` arm — Python parity. 2^53 vs 2^53+1 are DISTINCT
    // ints (they'd collide only under the old lossy `as_f64` compare), so `==` is FALSE and `!=` is
    // TRUE. Run it in a hot loop so the site warms past Cold into the specialized Int state — this
    // guards that the specialized path stays exact, not just the generic one.
    let src = "i := 0\nhits := 0\nwhile i < 3:\n    if 9007199254740992 == 9007199254740993:\n        hits = hits + 1\n    i = i + 1\nprint(hits)\n";
    assert_eq!(run_parity(src), "0\n");
    let src2 = "i := 0\nmiss := 0\nwhile i < 3:\n    if 9007199254740992 != 9007199254740993:\n        miss = miss + 1\n    i = i + 1\nprint(miss)\n";
    assert_eq!(run_parity(src2), "3\n");
}

#[test]
fn quicken_eq_small_ints_exact() {
    // Small ints (within f64 exact range) compare normally; loop warms the site to Int state.
    let src = "i := 0\nc := 0\nwhile i < 6:\n    if i == 3:\n        c = c + 100\n    if i != 3:\n        c = c + 1\n    i = i + 1\nprint(c)\n";
    // i==3 once (+100), i!=3 five times (+5) = 105
    assert_eq!(run_parity(src), "105\n");
}

#[test]
fn quicken_deopt_int_then_float_then_str() {
    // A single generic `+` site reached first with ints (warms to Int), then floats, then strings
    // (string concat) — each must deopt cleanly to the generic path and stay correct. The `+` of
    // two CALL results is a stack-operand Add (un-fused), exactly the quickening target.
    let src = "fn add2[T](a: T, b: T) -> T:\n    return a + b\n\
                   print(add2(add2(2, 3), add2(4, 5)))\n\
                   print(add2(1.5, 2.5))\n\
                   print(add2(\"ab\", \"cd\"))\n";
    // 5+9=14 ; 1.5+2.5=4.0 ; abcd
    assert_eq!(run_parity(src), "14\n4.0\nabcd\n");
}

#[test]
fn quicken_stack_arith_and_compare_int_fast_path() {
    // Stack-operand arith + ordered compare on ints (the un-fused generic arms). `(a*b) - (c+d)`
    // pushes intermediate results then operates on them — not a `local⊕local`/`local⊕const`
    // window, so it never fuses to a superinstruction and rides the quickened path instead.
    let src = "fn f(a: int, b: int, c: int, d: int) -> int:\n    return (a * b) - (c + d)\n\
                   i := 0\nacc := 0\nwhile i < 4:\n    if f(i + 2, i + 3, i, 1) > 0:\n        acc = acc + f(i + 2, i + 3, i, 1)\n    i = i + 1\nprint(acc)\n";
    // f = (i+2)(i+3) - (i+1): i=0:6-1=5; i=1:12-2=10; i=2:20-3=17; i=3:30-4=26 ; all >0 => 58
    assert_eq!(run_parity(src), "58\n");
}

#[test]
fn quicken_overflow_and_divzero_errors_match_generic() {
    // The quickened int fast path reuses `fast_int_bin`, so overflow / div-by-zero must raise the
    // SAME error as the generic `arith` path. Warm the site, then trip it.
    let dz = "i := 0\nwhile i < 1:\n    print(10 / (i - i))\n    i = i + 1\n";
    let err = run_capture(dz).unwrap_err().to_string();
    assert!(err.contains("division by zero"), "got: {err}");
    let mz = "i := 0\nwhile i < 1:\n    print(10 % (i - i))\n    i = i + 1\n";
    let err2 = run_capture(mz).unwrap_err().to_string();
    assert!(err2.contains("modulo by zero"), "got: {err2}");
}

// ---- M19 Phase 6: method-call inline cache (+ flatten) — correctness guards ----
// `Op::CallMethod` on a struct caches `(tid → proto, module_idx)` per call site, mirroring the
// field IC: a hit on a matching `tid` skips the `program.structs` clone + the name-keyed
// `def.methods` probe. The cell holds no `GcRef` (proto id + module_idx are heap-independent), so
// it is invisible to GC / snapshots / `swap_ctx` — sound across cooperative fibers and `--parallel`.

#[test]
fn method_ic_sites_allocated_and_vm_presized() {
    // White-box wiring: a program with struct-method calls allocates ≥1 method-IC site, and the
    // VM pre-sizes its per-`Vm` `method_ic` vector to match (mirrors `field_ic_sites`/`field_ic`).
    let src = "struct C:\n    n: int\n\n    fn g(self) -> int:\n        return self.n\nc := C(5)\nprint(c.g())\n";
    let tokens = lexer::tokenize(src).unwrap();
    let module = parser::parse(tokens).unwrap();
    let program = crate::compiler::compile_module_standalone(&module).unwrap();
    assert!(
        program.method_ic_sites >= 1,
        "expected ≥1 method-IC site, got {}",
        program.method_ic_sites
    );
    let vm = Vm::new(Arc::new(program.clone()));
    assert_eq!(vm.method_ic.len(), program.method_ic_sites as usize);
}

#[test]
fn method_ic_monomorphic_hot_loop() {
    // A struct method called in a hot loop — the IC hit path. The method DISPATCH (not a field
    // read) is what the method IC caches; the cached proto must be re-used every iteration.
    let src = "struct Acc:\n    n: int\n\n    fn add(self, k: int) -> int:\n        return self.n + k\n\
                   a := Acc(10)\ni := 0\nout := 0\nwhile i < 5:\n    out = out + a.add(i)\n    i = i + 1\nprint(out)\n";
    // per iter: 10 + i -> 10,11,12,13,14 = 60
    assert_eq!(run_parity(src), "60\n");
}

#[test]
fn method_ic_polymorphic_one_site_via_protocol() {
    // A protocol-bounded generic fn has ONE `CallMethod` site (type-erased body) reached by two
    // distinct struct types. A method-IC hit that ignored type identity would dispatch a stale
    // proto (Sq.area on a Rect); the `tid` guard forces a re-resolve on the type switch.
    let src = "protocol Shape:\n    fn area(self) -> int\n\
                   struct Sq:\n    s: int\n\n    fn area(self) -> int:\n        return self.s * self.s\n\
                   struct Rect:\n    w: int\n    h: int\n\n    fn area(self) -> int:\n        return self.w * self.h\n\
                   fn describe[S: Shape](x: S) -> int:\n    return x.area()\n\
                   i := 0\nout := 0\nwhile i < 4:\n    out = out + describe(Sq(3)) + describe(Rect(2, 5))\n    i = i + 1\nprint(out)\n";
    // per iter: 9 + 10 = 19 ; *4 = 76
    assert_eq!(run_parity(src), "76\n");
}

#[test]
fn method_ic_under_parallel_engine() {
    // The method IC lives on each worker `Vm`; method-heavy code on the real-thread engine must
    // match the cooperative engine (caches are per-Vm, tid-guarded, self-verifying).
    let src = "struct Pt:\n    x: int\n    y: int\n\n    fn sum(self) -> int:\n        return self.x + self.y\n\
                   p := Pt(3, 4)\nacc := 0\ni := 0\nwhile i < 100:\n    acc = acc + p.sum()\n    p.x = p.x + 1\n    i = i + 1\n\
                   print(acc)\nprint(p.x)\n";
    // Same arithmetic as `ic_struct_under_parallel_engine` (identical Pt(3,4)/sum/100-iter loop):
    // acc = 700 + 4950 = 5650, p.x ends at 103.
    assert_eq!(run_capture(src).expect("parallel"), "5650\n103\n");
}

#[test]
fn method_ic_gc_stress() {
    // Method dispatch under collect-before-every-instruction: the cached proto/module_idx stay
    // valid because they hold no GcRef and GC never reorders a struct's identity.
    let src = "struct Box:\n    v: int\n\n    fn doubled(self) -> int:\n        return self.v * 2\n\
                   b := Box(21)\ni := 0\nacc := 0\nwhile i < 30:\n    acc = acc + b.doubled()\n    i = i + 1\nprint(acc)\n";
    assert_eq!(run_capture_stress(src), run(src));
    assert_eq!(run_parity(src), "1260\n");
}

#[test]
fn method_ic_function_typed_field_not_cached() {
    // `recv.f(args)` where `f` is a function-typed FIELD (not a method) must keep dispatching via
    // `invoke_value` — the method IC must never cache it as a method proto.
    let src = "struct H:\n    op: fn(int) -> int\n\
                   double := fn(x: int) -> int: x * 2\nh := H(double)\n\
                   i := 0\nout := 0\nwhile i < 3:\n    out = out + h.op(i + 1)\n    i = i + 1\nprint(out)\n";
    // per iter: (i+1)*2 -> 2,4,6 = 12
    assert_eq!(run_parity(src), "12\n");
}

#[test]
fn method_ic_struct_method_shadowing_hof_name() {
    // The IC fast path sits BEFORE the list-HOF / core-type guards in `do_method_call`. A struct
    // whose own method is named `map` (a built-in list HOF name) must dispatch the STRUCT method,
    // never the list HOF — the `Obj::Struct` tid guard makes the collision impossible, this pins it.
    let src = "struct Grid:\n    n: int\n\n    fn map(self, k: int) -> int:\n        return self.n + k\n\
                   g := Grid(100)\ni := 0\nacc := 0\nwhile i < 3:\n    acc = acc + g.map(i)\n    i = i + 1\nprint(acc)\n";
    // 100+0 + 100+1 + 100+2 = 303
    assert_eq!(run_parity(src), "303\n");
}

#[test]
fn method_ic_flattened_method_with_defer_in_loop() {
    // A flattened method's `do_return` must drain the frame's `defer`s on the IC-hit path, every
    // iteration, AFTER the return value is captured (Go order) — pinned across repeated hits.
    let src = "fn note(id: int):\n    print(\"d{id}\")\n\
                   struct Logger:\n    id: int\n\n    fn work(self, n: int) -> int:\n        defer note(self.id)\n        return n * 2\n\
                   l := Logger(7)\ni := 0\nacc := 0\nwhile i < 3:\n    acc = acc + l.work(i)\n    i = i + 1\nprint(acc)\n";
    // each call: prints d7 (defer), returns n*2 -> 0,2,4 = 6
    assert_eq!(run_parity(src), "d7\nd7\nd7\n6\n");
}

#[test]
fn method_ic_uncaught_fault_on_hit_path() {
    // Warm the IC with a good call, then fault on a cached hit. The flattened/cached path must
    // produce the SAME uncaught-fault behavior (message + that the program errors) as a fresh
    // resolve (run_err asserts the VM error).
    let src = "struct Bomb:\n    n: int\n\n    fn blow(self, d: int) -> int:\n        return self.n / d\n\
                   b := Bomb(10)\nprint(b.blow(2))\nprint(b.blow(0))\n";
    let vm_err = run_err(src);
    let interp_err = match run_capture(src) {
        Ok(o) => panic!("expected interp error, got {o:?}"),
        Err(e) => e.message,
    };
    assert_eq!(
        vm_err, interp_err,
        "VM/interp must agree on the IC-hit-path fault message"
    );
    assert!(
        vm_err.contains("zero") || vm_err.contains("division"),
        "got: {vm_err}"
    );
}

#[test]
fn method_ic_survives_fiber_park_under_parallel() {
    // The per-`Vm` `method_ic` must stay intact across a `swap_ctx` (a fiber parks on `recv`, another
    // runs, the parked fiber resumes and makes a CACHED method call). The central liveness claim:
    // the cell holds no `GcRef`, so a context swap can't invalidate it. VM == `--parallel`.
    let src = "struct Acc:\n    base: int\n\n    fn fold_in(self, k: int) -> int:\n        return self.base + k\n\
                   fn consumer(ch: Channel[int]):\n    a := Acc(1000)\n    total := 0\n    i := 0\n    while i < 4:\n        v := ch.recv()\n        total = total + a.fold_in(v)\n        i = i + 1\n    print(total)\n\
                   fn producer(ch: Channel[int]):\n    i := 0\n    while i < 4:\n        ch.send(i)\n        i = i + 1\n\
                   fn main():\n    ch := Channel[int]()\n    parallel:\n        spawn consumer(ch)\n        spawn producer(ch)\nmain()\n";
    // total = 4*1000 + (0+1+2+3) = 4006
    assert_eq!(run(src), "4006\n");
}

// ---- M19 — N-way polymorphic method-call IC (CALLMETHOD ADAPTIVE) correctness guards ----
// The single `MethodIcCell` per site is widened to a small N-way poly cache: a megamorphic site
// (a `List[Shape]` walked at one `.area()` call) must HIT a way for each distinct receiver `tid`
// — never thrash the monomorphic refill, never dispatch a wrong body. Each way is tid+arity
// re-guarded on every hit (a way can never enter a frame with the wrong slot count or body). A
// 5th+ distinct tid sets a one-way sticky-generic bit so the site stops probing the ways and goes
// straight to the (clone-free) slow path, mirroring the binop quickening's `Q_GENERIC`.

#[test]
fn mega_dispatch_correctness_parity() {
    // A `Shape` protocol with `.area()` on FOUR distinct struct types (distinct layouts/tids).
    // A heterogeneous `List[Shape]` is walked at ONE call site, repeatedly (each type hit many
    // times). The right body must dispatch per type across repeated calls; byte-identical against the golden.
    let src = "protocol Shape:\n    fn area(self) -> int\n\
                   struct Sq:\n    s: int\n\n    fn area(self) -> int:\n        return self.s * self.s\n\
                   struct Rect:\n    w: int\n    h: int\n\n    fn area(self) -> int:\n        return self.w * self.h\n\
                   struct Tri:\n    b: int\n    hh: int\n\n    fn area(self) -> int:\n        return self.b * self.hh / 2\n\
                   struct Circ:\n    r: int\n\n    fn area(self) -> int:\n        return self.r + self.r\n\
                   fn total(shapes: List[Shape]) -> int:\n    acc := 0\n    for s in shapes:\n        acc = acc + s.area()\n    return acc\n\
                   shapes := []\nshapes.push(Sq(3))\nshapes.push(Rect(2, 4))\nshapes.push(Tri(4, 5))\nshapes.push(Circ(3))\n\
                   out := 0\ni := 0\nwhile i < 8:\n    out = out + total(shapes)\n    i = i + 1\nprint(out)\n";
    // per pass: 9 + 8 + 10 + 6 = 33 ; *8 = 264
    assert_eq!(run_parity(src), "264\n");
}

#[test]
fn poly_ic_all_ways_distinct_bodies() {
    // Four distinct tids each map to a DIFFERENT body returning a tid-distinguishing constant.
    // Every way fills, then each type is re-called: each must still return its OWN body's value,
    // not a stale neighbour way's. A broken way-match (compare only way[0].tid) returns way-0's
    // body for the 2nd/3rd/4th types → wrong sum → this fails.
    let src = "protocol Tag:\n    fn id(self) -> int\n\
                   struct A:\n    n: int\n\n    fn id(self) -> int:\n        return 1\n\
                   struct B:\n    n: int\n\n    fn id(self) -> int:\n        return 2\n\
                   struct C:\n    n: int\n\n    fn id(self) -> int:\n        return 4\n\
                   struct D:\n    n: int\n\n    fn id(self) -> int:\n        return 8\n\
                   fn sum(xs: List[Tag]) -> int:\n    acc := 0\n    for x in xs:\n        acc = acc + x.id()\n    return acc\n\
                   xs := []\nxs.push(A(0))\nxs.push(B(0))\nxs.push(C(0))\nxs.push(D(0))\n\
                   out := 0\ni := 0\nwhile i < 5:\n    out = out + sum(xs)\n    i = i + 1\nprint(out)\n";
    // per pass 1+2+4+8 = 15 ; *5 = 75 (a stale-way bug would smear the bits)
    assert_eq!(run_parity(src), "75\n");
}

#[test]
fn poly_ic_overflow_goes_sticky_generic() {
    // FIVE+ distinct struct types at ONE site overflow the N(=4)-way cache. The 5th+ must resolve
    // via the slow path (sticky-generic), never a wrong way. All five bodies must still dispatch
    // correctly across repeated calls; byte-identical against the golden.
    let src = "protocol Tag:\n    fn id(self) -> int\n\
                   struct A:\n    n: int\n\n    fn id(self) -> int:\n        return 1\n\
                   struct B:\n    n: int\n\n    fn id(self) -> int:\n        return 10\n\
                   struct C:\n    n: int\n\n    fn id(self) -> int:\n        return 100\n\
                   struct D:\n    n: int\n\n    fn id(self) -> int:\n        return 1000\n\
                   struct E:\n    n: int\n\n    fn id(self) -> int:\n        return 10000\n\
                   struct F:\n    n: int\n\n    fn id(self) -> int:\n        return 100000\n\
                   fn sum(xs: List[Tag]) -> int:\n    acc := 0\n    for x in xs:\n        acc = acc + x.id()\n    return acc\n\
                   xs := []\nxs.push(A(0))\nxs.push(B(0))\nxs.push(C(0))\nxs.push(D(0))\nxs.push(E(0))\nxs.push(F(0))\n\
                   out := 0\ni := 0\nwhile i < 6:\n    out = out + sum(xs)\n    i = i + 1\nprint(out)\n";
    // per pass 111111 ; *6 = 666666
    assert_eq!(run_parity(src), "666666\n");
}

#[test]
fn poly_ic_site_latches_sticky_on_5th_type() {
    // White-box: after a 5-type megamorphic site runs, the site's 4 ways are all occupied AND the
    // `sticky` latch is set (so further calls skip way-probing — the anti-thrash guarantee). Pins
    // the deopt mechanism directly, not just the output. A monomorphic site stays non-sticky.
    let src = "protocol Tag:\n    fn id(self) -> int\n\
                   struct A:\n    n: int\n\n    fn id(self) -> int:\n        return 1\n\
                   struct B:\n    n: int\n\n    fn id(self) -> int:\n        return 2\n\
                   struct C:\n    n: int\n\n    fn id(self) -> int:\n        return 3\n\
                   struct D:\n    n: int\n\n    fn id(self) -> int:\n        return 4\n\
                   struct E:\n    n: int\n\n    fn id(self) -> int:\n        return 5\n\
                   fn sum(xs: List[Tag]) -> int:\n    acc := 0\n    for x in xs:\n        acc = acc + x.id()\n    return acc\n\
                   xs := []\nxs.push(A(0))\nxs.push(B(0))\nxs.push(C(0))\nxs.push(D(0))\nxs.push(E(0))\n\
                   print(sum(xs))\n";
    let tokens = lexer::tokenize(src).unwrap();
    let module = parser::parse(tokens).unwrap();
    let program = crate::compiler::compile_module_standalone(&module).unwrap();
    let mut vm = Vm::new(Arc::new(program));
    vm.run().expect("run");
    assert_eq!(vm.out, b"15\n");
    // Exactly one method-call site (the `x.id()` in `sum`), and it must have gone sticky with all
    // 4 ways filled by the first four distinct tids.
    let sticky_sites = vm.method_ic.iter().filter(|s| s.sticky).count();
    assert_eq!(
        sticky_sites, 1,
        "expected the megamorphic `id` site to latch sticky"
    );
    let sticky = vm.method_ic.iter().find(|s| s.sticky).unwrap();
    assert!(
        sticky.ways.iter().all(|w| w.tid != TID_NONE),
        "all 4 ways should be occupied before sticky"
    );
}

#[test]
fn structdef_clone_free_slow_path_parity() {
    // A megamorphic site (>4 types → sticky-generic slow path) PLUS a function-typed FIELD call
    // `recv.f(args)` (the fields-fallback arm that previously read the cloned Obj). Removing the
    // per-miss StructDef clone must not break either. byte-identical against the golden.
    let src = "protocol Tag:\n    fn id(self) -> int\n\
                   struct A:\n    n: int\n\n    fn id(self) -> int:\n        return 1\n\
                   struct B:\n    n: int\n\n    fn id(self) -> int:\n        return 2\n\
                   struct C:\n    n: int\n\n    fn id(self) -> int:\n        return 3\n\
                   struct D:\n    n: int\n\n    fn id(self) -> int:\n        return 4\n\
                   struct E:\n    n: int\n\n    fn id(self) -> int:\n        return 5\n\
                   struct H:\n    op: fn(int) -> int\n\
                   fn sum(xs: List[Tag]) -> int:\n    acc := 0\n    for x in xs:\n        acc = acc + x.id()\n    return acc\n\
                   xs := []\nxs.push(A(0))\nxs.push(B(0))\nxs.push(C(0))\nxs.push(D(0))\nxs.push(E(0))\n\
                   double := fn(x: int) -> int: x * 2\nh := H(double)\n\
                   out := 0\ni := 0\nwhile i < 3:\n    out = out + sum(xs) + h.op(i)\n    i = i + 1\nprint(out)\n";
    // per pass sum=15 ; h.op(i)=2*i -> i=0:15+0, i=1:15+2, i=2:15+4 => 45+6 = 51
    assert_eq!(run_parity(src), "51\n");
}

#[test]
fn inlined_hot_ops_path_matches_step() {
    // M19 Phase 7 — `run_until` dispatches the hottest ops (GetLocal/SetLocal, the superinstrs,
    // Jump/JumpIfFalse, Call/Return) inline and delegates the tail to `step`. This hammers every
    // inlined op in one program (locals + `a+b`/`a+const`/`i+=1` superinstrs + a conditional +
    // a call + a return) and pins the result.
    let src = "fn f(a: int, b: int) -> int:\n    return a + b\n\
                   i := 0\nacc := 0\n\
                   while i < 20:\n    x := i * 2\n    if x % 3 == 0:\n        acc = acc + f(x, i)\n    else:\n        acc = acc + 1\n    i = i + 1\nprint(acc)\n";
    // x=0,3*?: i=0 x=0 x%3==0 acc+=f(0,0)=0; i=1 x=2 no acc+=1; i=2 x=4 no +1; i=3 x=6 yes +f(6,3)=9;
    // i=4 x=8 no +1; i=5 x=10 no +1; i=6 x=12 yes +f(12,6)=18; ... acc=202 (verified against CPython).
    assert_eq!(run(src), "202\n");
}

#[test]
fn method_call_flatten_deep_recursion_on_small_stack() {
    // Phase 6b: a recursive struct method must not consume host stack (frames live in the heap
    // `frames` Vec, executed by the running `run_until` — not a per-call Rust recursion). Survives
    // a host stack far below production `VM_STACK_BYTES`, like the plain-call flatten guarantee.
    let src = "struct R:\n    base: int\n\n    fn down(self, n: int) -> int:\n        if n == 0:\n            return self.base\n        return self.down(n - 1)\n\
                   r := R(99)\nprint(r.down(8000))\n";
    assert_eq!(
        run_capture_on_stack(src, 256 * 1024).expect("deep method recursion on small stack"),
        "99\n"
    );
}

#[test]
fn calls_preserve_arg_order_nesting_and_result_slot() {
    // P1 characterization: locks call semantics before the in-place-args refactor. The bugs an
    // in-place fast path could introduce are stack-position errors — wrong arg order, a stale
    // callee slot left under the result, or a misplaced return value in a larger expression.
    // Non-commutative op catches arg-order swaps; the nested/expression forms catch slot drift.
    assert_eq!(
        run("fn sub(a: int, b: int) -> int:\n    return a - b\nprint(sub(10, 3))\n"),
        "7\n"
    );
    assert_eq!(
        run(
            "fn sub(a: int, b: int) -> int:\n    return a - b\nprint(sub(sub(20, 5), sub(8, 3)))\n"
        ),
        "10\n"
    );
    assert_eq!(
        run("fn sub(a: int, b: int) -> int:\n    return a - b\nprint(sub(10, 3) * 2 + 1)\n"),
        "15\n"
    );
    // Zero-arg call returning a value; result used in an expression.
    assert_eq!(
        run("fn five() -> int:\n    return 5\nprint(five() + 1)\n"),
        "6\n"
    );
    // Recursion through the call path.
    assert_eq!(
        run(
            "fn fib(n: int) -> int:\n    if n < 2:\n        return n\n    return fib(n - 1) + fib(n - 2)\nprint(fib(10))\n"
        ),
        "55\n"
    );
    // Closure value called via a binding (the Closure arm of the fast path).
    assert_eq!(run("g := fn(x: int) -> int: x * 2\nprint(g(21))\n"), "42\n");
    // Closure capturing an outer binding, then called.
    assert_eq!(
        run("k := 100\nadd := fn(x: int) -> int: x + k\nprint(add(7))\n"),
        "107\n"
    );
    // HOF native (`map`) still routes through the Vec path in invoke_value — must stay correct.
    assert_eq!(
        run("print([1, 2, 3].map(fn(x: int) -> int: x + 1))\n"),
        "[2, 3, 4]\n"
    );
    // `defer` inside a called fn runs at that fn's exit (LIFO), not the caller's.
    assert_eq!(
        run(
            "fn log(s: str):\n    print(s)\nfn f():\n    defer log(\"a\")\n    defer log(\"b\")\n    log(\"body\")\nf()\nlog(\"after\")\n"
        ),
        "body\nb\na\nafter\n"
    );
}

#[test]
fn fstring_and_str_render_all_value_shapes() {
    // P2 characterization: locks the exact BuildStr / stringify output across every value
    // shape before the stringify-into-buffer refactor (separators, braces, nesting, hooks).
    // W8-1: `{1}` is a bare-digit hole, which now renders as literal text (see
    // `interpolation.rs`'s digit-hole rule), not as an interpolated `1`.
    assert_eq!(run("print(\"{1} {2.5} {true}\")\n"), "{1} 2.5 true\n");
    assert_eq!(run("x := 42\nprint(\"i={x}\")\n"), "i=42\n");
    assert_eq!(run("print(\"{[1, 2, 3]}\")\n"), "[1, 2, 3]\n");
    assert_eq!(run("print(\"{(1, 2)}\")\n"), "(1, 2)\n");
    assert_eq!(run("print(\"{[[1], [2, 3]]}\")\n"), "[[1], [2, 3]]\n");
    assert_eq!(
        run("m := {\"a\": 1, \"b\": 2}\nprint(\"{m}\")\n"),
        "{'a': 1, 'b': 2}\n"
    );
    assert_eq!(run("print(str({1, 2}))\n"), "{1, 2}\n");
    assert_eq!(run("s: Set[int] = Set()\nprint(str(s))\n"), "Set()\n");
    // Struct default repr + a multi-part f-string mixing literal text and several holes.
    assert_eq!(
        run("struct P:\n    x: int\n    y: int\nprint(\"p={P(3, 4)} end\")\n"),
        "p=P(x=3, y=4) end\n"
    );
    // `str(self)` protocol hook overrides the default repr inside interpolation.
    assert_eq!(
        run(
            "struct Pt:\n    x: int\n    fn str(self) -> str:\n        return \"<{self.x}>\"\nprint(\"v={Pt(7)}\")\n"
        ),
        "v=<7>\n"
    );
    // Enum nullary + payload variants.
    assert_eq!(
        run("enum E:\n    A\n    B(int, int)\nprint(\"{E.A} {E.B(1, 2)}\")\n"),
        "A B(1, 2)\n"
    );
}

#[test]
fn list_comprehension_maps_and_filters() {
    assert_eq!(run("print([x * 2 for x in [1, 2, 3]])\n"), "[2, 4, 6]\n");
    assert_eq!(
        run("print([x for x in [1, 2, 3, 4] if x % 2 == 0])\n"),
        "[2, 4]\n"
    );
}

#[test]
fn list_comprehension_over_range() {
    assert_eq!(run("print([x * x for x in 0..5])\n"), "[0, 1, 4, 9, 16]\n");
}

#[test]
fn vm_two_clause_list_comprehension() {
    // ys iterate inner-most for each x (Python order): (1,10),(1,20),(2,10),(2,20).
    assert_eq!(
        run("print([x + y for x in [1, 2] for y in [10, 20]])\n"),
        "[11, 21, 12, 22]\n"
    );
}

#[test]
fn vm_three_clause_list_comprehension() {
    assert_eq!(
        run("print([a * 100 + b * 10 + c for a in [1, 2] for b in [3] for c in [4, 5]])\n"),
        "[134, 135, 234, 235]\n"
    );
}

#[test]
fn vm_guard_after_nonfinal_clause() {
    // Only odd x survive (1, 3); each pairs with y in [10, 20].
    assert_eq!(
        run("print([x * y for x in 1..4 if x % 2 == 1 for y in [10, 20]])\n"),
        "[10, 20, 30, 60]\n"
    );
}

#[test]
fn vm_later_clause_references_earlier_var() {
    // Flatten a list-of-lists: second clause iterates the first clause's binding.
    assert_eq!(
        run("print([y for xs in [[1, 2], [3], [4, 5]] for y in xs])\n"),
        "[1, 2, 3, 4, 5]\n"
    );
}

#[test]
fn vm_nested_set_and_map_comprehension() {
    assert_eq!(
        run("print({x + y for x in [0, 3] for y in [0, 3]})\n"),
        "{0, 3, 6}\n"
    );
    assert_eq!(
        run("print({x * 10 + y: x + y for x in [1, 2] for y in [3]})\n"),
        "{13: 4, 23: 5}\n"
    );
}

/// A comprehension over a STATEFUL struct iterator must drive `next()` LAZILY — the element/guard
/// see the iterator's per-step state, exactly like a `for` statement and like the VM's
/// `compile_for`. The since-removed interp engine eagerly drained the iterator to `None` before
/// evaluating any element (so `c.n` read the fully-advanced field), diverging from the VM. This
/// asserts the lazy/interleaved result, for the single-clause AND the nested-clause shape.
#[test]
fn comprehension_stateful_struct_iterator_lazy_parity() {
    let src = "\
struct Counter:
    n: int
    fn next(self) -> int?:
        v := self.n
        self.n = self.n + 1
        if v >= 3:
            return None
        return ?v

fn main():
    c := Counter(0)
    print([x * 100 + c.n for x in c])
    c2 := Counter(0)
    print([x * 100 + c2.n for x in c2 for y in [0, 1]])

main()
";
    let vm = run_capture(src).expect("vm run");
    // Must be byte-identical (the hard rule).
    // And the canonical (lazy/interleaved) result: each element reads the just-advanced `n`.
    assert_eq!(
        vm, "[1, 102, 203]\n[1, 1, 102, 102, 203, 203]\n",
        "lazy interleaved iteration expected"
    );
}

// ----- M6c: native function values -----

/// A boxed `BigInt`/`FloatBox` must be observationally identical to the inline `Int`/`Float` of the
/// same value — display, hash (the f64-bits scheme, NOT `*n as u64`), equality, and ordering. These
/// variants are unreachable from real programs this phase, so this direct test is their only exercise.
#[test]
fn bigint_floatbox_behave_like_inline() {
    use std::cmp::Ordering;
    let mut vm = Vm::new(Arc::new(empty_program()));
    let bi = Value::obj(vm.heap.alloc(Obj::BigInt(5)));
    let bi2 = Value::obj(vm.heap.alloc(Obj::BigInt(5)));
    let fb = Value::obj(vm.heap.alloc(Obj::FloatBox(1.5)));
    let fb2 = Value::obj(vm.heap.alloc(Obj::FloatBox(1.5)));
    // Display matches the inline scalar.
    assert_eq!(vm.display(bi), "5");
    assert_eq!(vm.display(bi), vm.display(Value::int(5)));
    let ref15 = vm.box_float(1.5);
    assert_eq!(vm.display(fb), vm.display(ref15));
    // Hash matches the inline scalar (validates the canonical f64-bits scheme, not `*n as u64`).
    assert_eq!(vm.scalar_hash(bi), vm.scalar_hash(Value::int(5)));
    assert_eq!(vm.scalar_hash(fb), vm.scalar_hash(ref15));
    // Equality holds across two independent boxes of the same value.
    assert!(vm.values_equal(bi, bi2));
    assert!(vm.values_equal(fb, fb2));
    // Ordering: equal boxes compare Equal.
    assert_eq!(vm.value_order(bi, bi2), Ordering::Equal);
    assert_eq!(vm.value_order(fb, fb2), Ordering::Equal);
}

#[test]
fn vm_calls_native_fn_value() {
    use crate::native::{Host, HostError, NativeRet};
    fn add(h: &mut dyn Host) -> Result<NativeRet, HostError> {
        crate::native::expect_args(h, "add", 2)?;
        Ok(NativeRet::Int(h.arg_int(0)? + h.arg_int(1)?))
    }
    let mut vm = Vm::new(Arc::new(empty_program()));
    let h = vm.heap.alloc(Obj::Native {
        name: "add".into(),
        func: add,
        kind: crate::native::Kind::Inline,
    });
    vm.push(Value::obj(h));
    vm.push(Value::int(40));
    vm.push(Value::int(2));
    vm.do_call(2, Span::RUNTIME).unwrap();
    assert_eq!(vm.pop(), Value::int(42));
}

/// TICKET-226: `Vm::callable` is the one answer to "is this value callable", read by call, spawn,
/// `Executor.submit` and the entrypoint, and it agrees with `type_name`'s "function". The extern
/// (`Obj::Cffi`) cell is `tests/chz/spec/fn_value_grid_test.chz`'s; the exhaustive `match` in
/// `Vm::callable` (no `_` arm) is the completeness guard for a new `Obj` kind.
#[test]
fn callable_classifies_every_fn_kind() {
    use crate::native::{Host, HostError, NativeRet};
    fn nop(_: &mut dyn Host) -> Result<NativeRet, HostError> {
        Ok(NativeRet::Nil)
    }
    let mut vm = Vm::new(Arc::new(empty_program()));
    let home = vm.heap.alloc(Obj::Str("home".into()));
    let fns = [
        Obj::Func { proto: 0, home },
        Obj::Closure {
            proto: 0,
            captured: vec![],
            home,
        },
        Obj::Native {
            name: "n".into(),
            func: nop,
            kind: crate::native::Kind::Inline,
        },
        Obj::Builtin(crate::base::builtin::Builtin::Ord),
    ];
    for o in fns {
        let v = Value::obj(vm.heap.alloc(o));
        assert!(vm.callable(v).is_some(), "{} is callable", vm.type_name(v));
        assert_eq!(vm.type_name(v), "function");
    }
    let others = [
        Value::obj(vm.heap.alloc(Obj::Str("s".into()))),
        Value::obj(vm.heap.alloc(Obj::Bytes(Box::new([])))),
        Value::int(1),
        Value::nil(),
    ];
    for v in others {
        assert!(
            vm.callable(v).is_none(),
            "{} is not callable",
            vm.type_name(v)
        );
        assert_ne!(vm.type_name(v), "function");
    }
}

#[test]
fn guarded_restores_native_reentry_on_panic() {
    // Regression (FFI callbacks): a Rust panic re-entered through a native FFI callback is caught
    // one frame up by `callback_trampoline`'s `catch_unwind` and re-raised as a recoverable error.
    // `guarded` MUST restore `native_reentry` on that unwind — otherwise the counter leaks at +1
    // for the VM's lifetime, and every blocking op gated on `native_reentry == 0` silently
    // demotes/inlines instead of parking on `--parallel` after a recovered callback panic
    // (diverging from interp, which has no such counter). A `Drop` guard can't fix this (it would
    // alias `self` across the re-entry), so `guarded` catches + decrements + resumes the unwind.
    let mut vm = Vm::new(Arc::new(empty_program()));
    assert_eq!(vm.native_reentry, 0);
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {})); // silence the expected panic print
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        vm.guarded(|_vm| -> Result<(), RuntimeError> { panic!("callback boom") })
    }));
    std::panic::set_hook(prev);
    assert!(
        caught.is_err(),
        "the panic must propagate out of guarded unchanged"
    );
    assert_eq!(
        vm.native_reentry, 0,
        "native_reentry must return to its entry value after a panicking re-entry"
    );
}

#[test]
fn guarded_walk_restores_walk_base_on_panic() {
    // W8-43 twin of the above. `guarded_walk` sets `walk_base` (the structural depth the enclosing
    // native walk already consumed) around a protocol-hook re-entry. It CANNOT restore it with a
    // plain assignment after the call: `guarded` catches the unwind, decrements `native_reentry`,
    // and resumes the unwind FROM INSIDE ITSELF, so anything written after `guarded(...)` is
    // skipped on a panic. Panics really do traverse this seam (`callback_trampoline` converts an
    // FFI-callback panic into a recoverable error; `run_one_fiber` turns a worker panic into
    // `Disp::Finish` and keeps the shell `Vm` alive for the NEXT fiber) — a leaked `walk_base`
    // would make every later fiber's `==`/`str` fault spuriously at "maximum structural depth".
    let mut vm = Vm::new(Arc::new(empty_program()));
    assert_eq!(vm.walk_base, 0);
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {})); // silence the expected panic print
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        vm.guarded_walk(9_999, |_vm| -> Result<(), RuntimeError> {
            panic!("hook boom")
        })
    }));
    std::panic::set_hook(prev);
    assert!(
        caught.is_err(),
        "the panic must propagate out of guarded_walk unchanged"
    );
    assert_eq!(
        vm.walk_base, 0,
        "walk_base must return to its entry value after a panicking hook re-entry"
    );
    assert_eq!(
        vm.native_reentry, 0,
        "the inner guarded must still have restored native_reentry"
    );
}

#[test]
fn cursor_crosses_airlock_by_deep_copy() {
    // A cursor (`Obj::Iter`) IS sendable: `to_wire` deep-copies it (recursively wiring items +
    // carrying `pos`) into a `WireValue::Iter`, and `from_wire` rebuilds an INDEPENDENT cursor —
    // exactly how a `List` crosses, and matching the interpreter's `deep_clone`. (Earlier this was
    // gated non-sendable like a generator, which panicked `deep_clone`'s `.expect` and diverged VM
    // from interp; a cursor is plain data — a snapshot Vec + index — so it crosses by value.)
    let mut vm = Vm::new(Arc::new(empty_program()));
    let cursor = Value::obj(vm.heap.alloc(Obj::Iter {
        items: vec![Value::int(7), Value::int(8)],
        pos: 1,
    }));
    let wire = vm
        .to_wire(cursor)
        .expect("a cursor is sendable by deep copy");
    assert!(!wire.has_handle(), "a cursor of scalars carries no handle");
    match &wire {
        WireValue::Iter { items, pos, .. } => {
            assert_eq!(*pos, 1, "pos carried across the wire");
            assert_eq!(items.len(), 2, "both snapshot items wired");
        }
        other => panic!("expected WireValue::Iter, got {other:?}"),
    }
    // Rebuild on the heap: an independent cursor with the same items + pos.
    let rebuilt = vm.from_wire(wire);
    let Some(h) = rebuilt.as_obj() else {
        panic!("from_wire(cursor) must be a heap obj")
    };
    match vm.heap.get(h) {
        Obj::Iter { items, pos } => {
            assert_eq!(*pos, 1);
            assert_eq!(items, &vec![Value::int(7), Value::int(8)]);
        }
        _ => panic!("expected a rebuilt Obj::Iter"),
    }
}

#[test]
fn vm_native_str_return_lowers_to_heap_with_no_children() {
    use crate::native::{Host, HostError, NativeRet};
    fn greet(_h: &mut dyn Host) -> Result<NativeRet, HostError> {
        Ok(NativeRet::Str("hi".into()))
    }
    let mut vm = Vm::new(Arc::new(empty_program()));
    let nat = vm.heap.alloc(Obj::Native {
        name: "greet".into(),
        func: greet,
        kind: crate::native::Kind::Inline,
    });
    // A native fn handle has no GC children (guards the mark-phase claim).
    assert!(vm.heap.children(nat).is_empty());
    vm.push(Value::obj(nat));
    vm.do_call(0, Span::RUNTIME).unwrap();
    let result = vm.pop();
    assert_eq!(vm.display(result), "hi");
}
