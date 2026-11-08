//! Golden tests: native handles and SSO, existentials across tasks, named imports, cross-module types
//! and collisions, qualified decode, Executor submit, number parsing.
//! Split out of `src/vm/golden_tests.rs`. `super::*` is the parent module.

use super::*;

/// Native-prelude phase 2b — the GENERIC / reserved-type container CTORS (range/List/Map/Set) are
/// dispatched by their `Builtin` id (`Lowering::Call`) for their `CallBuiltin`
/// DISPATCH, exactly as 2a did for the scalar ctors. Their generic type-identity resolution is
/// untouched, so every construction + generic-inference path must stay byte-identical
/// against a literal golden: range overloads, empty/turbofished containers, dedup, iteration.
#[test]
fn container_ctor_parity() {
    let src = r#"
fn main():
    print(List.from_iter(0..5).len())
    for i in Range(1, 10, 2):
        print(i)
    xs := List[int]()
    xs.push(10)
    xs.push(20)
    print(xs.len())
    print(xs[0])
    ss := List[str]()
    ss.push("a")
    print(ss[0])
    ys := List()
    print(ys.len())
    m := Map()
    m["k"] = 1
    print(m["k"])
    m2 := Map[str, int]()
    print(m2.len())
    s := Set.from_iter([1, 1, 2, 3, 3])
    print(s.len())
    s2 := Set[int]()
    print(s2.len())

main()
"#;
    assert_golden_out(src, "5\n1\n3\n5\n7\n9\n2\n10\na\n0\n1\n0\n3\n0\n");
}

/// M19 SSO — string ops must stay byte-identical for strings that straddle
/// the `ChzStr` inline/heap boundary (`INLINE_CAP` = 22 bytes), including multi-byte UTF-8.
/// Exercises concat, split/join, indexing, iteration, `==`, `.chars()`, and string map keys.
#[test]
fn sso_boundary_string_ops_parity() {
    let src = r#"
fn main():
    a := "aaaaaaaaaaaaaaaaaaaaa"      # 21 bytes (inline)
    b := "bbbbbbbbbbbbbbbbbbbbbb"     # 22 bytes (inline boundary)
    c := "ccccccccccccccccccccccc"    # 23 bytes (heap)
    print(a.len())
    print(b.len())
    print(c.len())

    # concat crosses the boundary in both directions
    ab := a + b
    print(ab.len())
    print(ab)
    print(a + "z")                    # 22, still inline
    print(b + "z")                    # 23, spills to heap

    # equality across storage (built two ways)
    print(b == "b" + "bbbbbbbbbbbbbbbbbbbbb")
    print((a + b) == (a + b))

    # indexing + iteration over a heap-length string
    print(c[0])
    print(c[22])
    n := 0
    for ch in c:
        n += 1
    print(n)

    # slice producing results on both sides of the boundary (slice itself allocs a str)
    print(c[0..22])                    # 22 bytes — inline
    print(c[0..23])                    # 23 bytes — heap
    print(ab[0..22])

    # case-fold can change byte length, straddling the boundary either way
    print(b.upper())                  # 22 ascii → 22, inline
    print(c.lower())                  # 23 → 23, heap
    print("héllo-wörld-straße".upper())   # multibyte fold (ß→SS grows length)

    # split / join round trip straddling the boundary
    joined := "left-segment-twelve,right-side-thirteen"
    bits := joined.split(",")
    print(bits[0])
    print(bits[1])
    print(",".join(bits) == joined)

    # f-string interpolation growing past the boundary
    i := 0
    while i < 5:
        print("prefix-pad-prefix-pad-{i}")   # ~22-23 bytes, straddles
        i += 1

    # multi-byte UTF-8: short (inline by bytes) and long (heap)
    m := "héllo wörld"                # 13 bytes, 11 chars — inline
    print(m.len())
    print(m.chars().len())
    big := "ñññññññññññññññ"          # 15 chars × 2 bytes = 30 — heap
    print(big.len())
    print(big.chars().len())

    # string map keys straddling the boundary
    mm := {a: 1, c: 2}
    print(mm[a])
    print(mm[c])

main()
"#;
    assert_golden_out(
        src,
        "21\n22\n23\n43\naaaaaaaaaaaaaaaaaaaaabbbbbbbbbbbbbbbbbbbbbb\naaaaaaaaaaaaaaaaaaaaaz\nbbbbbbbbbbbbbbbbbbbbbbz\ntrue\ntrue\nc\nc\n23\ncccccccccccccccccccccc\nccccccccccccccccccccccc\naaaaaaaaaaaaaaaaaaaaab\nBBBBBBBBBBBBBBBBBBBBBB\nccccccccccccccccccccccc\nHÉLLO-WÖRLD-STRASSE\nleft-segment-twelve\nright-side-thirteen\ntrue\nprefix-pad-prefix-pad-0\nprefix-pad-prefix-pad-1\nprefix-pad-prefix-pad-2\nprefix-pad-prefix-pad-3\nprefix-pad-prefix-pad-4\n11\n11\n15\n15\n1\n2\n",
    );
}

// ----- Task 2 (option a): user protocol existentials cross tasks -----

/// A concrete struct widened to a user protocol existential rides a `Channel[Proto]` to a spawned
/// task, which calls a protocol method and prints — byte-identical. (Was checker-
/// rejected pre-change: `Channel[Drawable]` "must be sendable".)
#[test]
fn protocol_value_crosses_channel() {
    let src = "protocol Drawable:\n    fn draw(self) -> str\nstruct Sq:\n    s: int\n    fn draw(self) -> str:\n        return \"sq\"\nfn main():\n    ch := Channel[Drawable]()\n    d: Drawable = Sq(1)\n    ch.send(d)\n    parallel:\n        spawn:\n            d2 := ch.recv() else: panic(\"ch closed\")\n            print(d2.draw())\nmain()\n";
    assert_golden_out(src, "sq\n");
}

/// An FFI fn (`Cffi`, pure code) passed as a spawn fn-ARG reaches the task and computes
/// `cos(0.0) == 1.0`.
#[test]
fn ffi_handle_crosses_spawn_arg() {
    assert_golden_out(
        "extern \"libm.so.6\":\n    fn cos(x: float) -> float\nfn use_fn(g: fn(float) -> float):\n    print(g(0.0))\nf := cos\nparallel:\n    spawn use_fn(f)\n",
        "1.0\n",
    );
}

/// An FFI fn sent over a `Channel` crosses by handle and is callable after `recv` — `sqrt(9.0) == 3.0`,
/// byte-identical (previously rejected at the `channel_method` "send" value-store).
#[test]
fn ffi_handle_crosses_channel_send() {
    assert_golden_out(
        "extern \"libm.so.6\":\n    fn sqrt(x: float) -> float\nch := Channel[fn(float) -> float]()\nch.send(sqrt)\nf := ch.recv() else: panic(\"ch closed\")\nprint(f(9.0))\n",
        "3.0\n",
    );
}

/// An FFI fn stored in a `Shared` box at CONSTRUCTION crosses by handle and is callable after `get` —
/// `sqrt(16.0) == 4.0` (previously rejected at the ctor value-store).
#[test]
fn ffi_handle_crosses_shared_ctor() {
    assert_golden_out(
        "extern \"libm.so.6\":\n    fn sqrt(x: float) -> float\nimport Shared from std.concurrency\nbox := Shared(sqrt)\nprint(box.get()(16.0))\n",
        "4.0\n",
    );
}

/// An FFI fn stored via `Shared.set` — the STORE path distinct from the ctor — crosses by handle and
/// is callable after `get` — `sqrt(25.0) == 5.0` (previously rejected at the store).
#[test]
fn ffi_handle_crosses_shared_set() {
    assert_golden_out(
        "extern \"libm.so.6\":\n    fn sqrt(x: float) -> float\nimport Shared from std.concurrency\nbox := Shared(fn(x: float) -> float: x)\nbox.set(sqrt)\nprint(box.get()(25.0))\n",
        "5.0\n",
    );
}

/// The FFI-over-`Channel` send now SUCCEEDS (Ok), byte-identical: `recover:` sees an
/// `Ok` → prints `false`. (Was `true` when the send rejected; the flip proves the send accepts it.)
#[test]
fn ffi_handle_send_succeeds() {
    assert_golden_out(
        "extern \"libm.so.6\":\n    fn sqrt(x: float) -> float\nch := Channel[fn(float) -> float]()\nr := recover: ch.send(sqrt)\nmatch r:\n    ?v: print(\"false\")\n    !e: print(\"true\")\n",
        "false\n",
    );
}

/// MUST-PRESERVE: a legit `Shared` handle crosses a `Channel` (M3.2: the receiver gets the handle).
#[test]
fn positive_shared_handle_crosses_channel() {
    assert_golden_out(
        "import Shared from std.concurrency\ns := Shared(42)\nch := Channel[Shared[int]]()\nch.send(s)\nt := ch.recv() else: panic(\"ch closed\")\nprint(t.get())\n",
        "42\n",
    );
}

/// MUST-PRESERVE: a nested `Shared[Shared[int]]` still constructs (the inner handle stores into the
/// outer box) — the guard must not over-reject a shared-core store.
#[test]
fn positive_nested_shared_constructs() {
    assert_golden_out(
        "import Shared from std.concurrency\nprint(Shared(Shared(1)).get().get())\n",
        "1\n",
    );
}

// ----- named-factory-import member resolution: RUNTIME unaffected (gap #4) -----
// These runs bypass the checker (compile+run directly), so they pass pre- AND post-fix — they lock
// that the checker-only member-resolution fix leaves the runtime output byte-identical.
// The checker gate itself is covered by the `checker::tests::checker_named_*` battery.

/// A named import of a factory FUNCTION (not its return type) still constructs and dispatches the
/// returned struct's method at runtime → `11`, byte-identical.
#[test]
fn named_fn_import_factory_struct_method_runtime() {
    let out = golden_file_entry(
        &[
            (
                "lib.chz",
                "struct Widget:\n    n: int\n    fn bump(self) -> int:\n        return self.n + 1\nfn make() -> Widget:\n    return Widget(10)\n",
            ),
            (
                "main.chz",
                "import make from lib\nw := make()\nprint(w.bump())\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "11\n");
}

/// TICKET-023: a named import of a PROTOCOL binds at runtime. `golden_file_entry` runs no checker,
/// so this pins the slot-list half (a type member holds no slot): without it `Vm::bind_import` faults "module 'shapes'
/// has no member 'Drawable'" trying to bind a runtime global for a name with no runtime value.
#[test]
fn protocol_named_import_binds_at_runtime() {
    let out = golden_file_entry(
        &[
            (
                "shapes.chz",
                "protocol Drawable:\n    fn draw(self) -> str\n",
            ),
            (
                "main.chz",
                "import Drawable from shapes\nstruct Circle:\n    r: int\n    fn draw(self) -> str:\n        return \"circle\"\nfn render(d: Drawable) -> str:\n    return d.draw()\nprint(render(Circle(1)))\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "circle\n");
}

/// Stdlib: `import manual from std.cancel; manual().cancelled()` runs (documented
/// Token API reachable off a named-imported factory result).
#[test]
fn named_fn_import_stdlib_cancel_runtime() {
    let out = golden_entry("import manual from std.cancel\nt := manual()\nprint(t.cancelled())\n");
    assert_eq!(out, "false\n");
}

/// Stdlib (gaps.md cancel-derive fix): a derived token's `done()` must not fire before its
/// `cancelled()`/`reason()` flip — the Go-context invariant "once done() unblocks, the token is
/// cancelled". `derive()` truncated the child timer's remaining-ms toward zero, arming it up to ~1ms
/// early; the `+ 1` ms margin keeps done() at-or-after the absolute deadline. Deterministic
/// (the margin makes it load-independent): a task woken by `done()` reads `cancelled()==true`.
#[test]
fn derived_cancel_token_done_implies_cancelled_runtime() {
    let out = golden_entry(
        "import std.cancel\nc := cancel.timeout(10).derive()\n_ := c.done().recv()\nprint(\"{c.cancelled()} {c.reason()}\")\n",
    );
    assert_eq!(out, "true timeout\n");
}

/// Stdlib: `import min_heap from std.collections; min_heap().push(3)` runs.
#[test]
fn named_fn_import_stdlib_collections_runtime() {
    let out = golden_entry(
        "import min_heap from std.collections\nh := min_heap()\nh.push(3)\nprint(h.len())\n",
    );
    assert_eq!(out, "1\n");
}

/// gap #4 (satisfies path): a named-fn-imported factory result passed through a protocol-bounded
/// generic constructs and dispatches → `W10`. The checker gate is covered in
/// `checker::tests::named_fn_import_satisfies_protocol_*`; this locks the RUNTIME output byte-identical.
#[test]
fn named_fn_import_protocol_bound_runtime() {
    let out = golden_file_entry(
        &[
            (
                "lib.chz",
                "struct Widget:\n    n: int\n    fn describe(self) -> str:\n        return \"W{self.n}\"\nfn make() -> Widget:\n    return Widget(10)\n",
            ),
            (
                "main.chz",
                "import make from lib\nprotocol Describable:\n    fn describe(self) -> str\nfn show[T: Describable](x: T):\n    print(x.describe())\nshow(make())\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "W10\n");
}

// ----- module-scoped user types: cross-module construction parity -----

/// `import geo; geo.Point(1,2)` constructs and prints `Point(x=1, y=2)` (BARE name, no `::`),
/// byte-identical against a literal golden.
#[test]
fn qualified_struct_ctor_parity() {
    let out = golden_file_entry(
        &[
            ("geo.chz", "struct Point:\n    x: int\n    y: int\n"),
            (
                "main.chz",
                "import geo\np := geo.Point(1, 2)\nprint(p)\nprint(p.x)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "Point(x=1, y=2)\n1\n");
}

/// `import S from types` (struct) constructs bare.
#[test]
fn from_import_struct_ctor_parity() {
    let out = golden_file_entry(
        &[
            ("types.chz", "struct S:\n    n: int\n"),
            ("main.chz", "import S from types\nprint(S(7))\n"),
        ],
        "main.chz",
    );
    assert_eq!(out, "S(n=7)\n");
}

/// Task 3/5: the module-owned synthetic structs (`Match`/`Response`/`ProcResult`) are now
/// USER-CONSTRUCTIBLE once their module is imported — the VM must build + field-read them
/// correctly (the layout seeded in `Compiler::new`). Qualified ctor + bare ctor on whole-module
/// import.
#[test]
fn synthetic_struct_qualified_ctor_parity() {
    let out = golden_entry(
        "import std.regex\nm := regex.Match(\"hi\", 0, 2, [\"a\"])\nprint(m.text + \"{m.start}\" + \"{m.end}\" + \",\".join(m.groups))\n",
    );
    assert_eq!(out, "hi02a\n");
}

#[test]
fn synthetic_struct_bare_ctor_on_import_parity() {
    let out = golden_entry(
        "import Match from std.regex\nm: Match = Match(\"yo\", 1, 3, [])\nprint(m.text + \"{m.end}\")\n",
    );
    assert_eq!(out, "yo3\n");
}

/// `Match.start`/`.end` are CODEPOINT offsets, so slicing the subject by them reproduces `.text` on
/// non-ASCII input too (Chezzi slicing is codepoint-indexed; the regex crate's byte spans are
/// converted at the native seam). Byte-identical against a literal expectation (it printed "lo"
/// before).
#[test]
fn regex_offsets_are_codepoint_slicable_parity() {
    let src = "import std.regex\n\
                   s := \"héllo\"\n\
                   match regex.find(\"l+\", s):\n\
                   \x20   ?opt:\n\
                   \x20       match opt:\n\
                   \x20           ?m:\n\
                   \x20               print(s[m.start..m.end])\n\
                   \x20               print(\"{s[m.start..m.end] == m.text}\")\n\
                   \x20           None: print(\"none\")\n\
                   \x20   !e: print(e)\n";
    let t = TmpDir::new();
    let path = t.write("main.chz", src);
    let (out, _e, res, _) = run_file(&path);
    res.expect("regex codepoint-offset program should run");
    assert_eq!(out, "ll\ntrue\n");
}

/// Phase 4b — the whole regex SIGNATURE (the `native struct Match` + the 5 `native fn`s) now comes
/// from the file-backed `std/regex.chz` (harvested into std.regex's ModuleSig via
/// `harvest_native_module`), replacing BOTH the phase-4a companion stub and the hand-built
/// `native_module_sig` arm. This must be a ZERO observable behavior change: producing a `Match`
/// (regex.find), reading every field, `from std.regex import Match`, and qualified
/// `regex.Match(...)` stay byte-identical. This is the regression net proving the native-seam
/// pure-type `bind_import` skip still holds (`from std.regex import Match` must NOT bind a runtime
/// value / fault). The origin=Builtin force in `harvest_native_module` is what keeps that skip
/// correct.
#[test]
fn regex_match_file_backed_runs() {
    let src = "import std.regex\n\
                   import Match from std.regex\n\
                   fn describe(m: Match) -> str:\n\
                   \x20   return m.text + \"@\" + \"{m.start}\" + \"-\" + \"{m.end}\" + \":\" + \",\".join(m.groups)\n\
                   lit: Match = regex.Match(\"lit\", 9, 12, [\"g\"])\n\
                   print(describe(lit))\n\
                   match regex.find(\"[0-9]+\", \"a12b\"):\n\
                   \x20   ?opt:\n\
                   \x20       match opt:\n\
                   \x20           ?m: print(describe(m))\n\
                   \x20           None: print(\"none\")\n\
                   \x20   !e: print(e)\n";
    let t = TmpDir::new();
    let path = t.write("main.chz", src);
    let (out, _e, res, _) = run_file(&path);
    res.expect("regex Match stub program should run");
    assert_eq!(out, "lit@9-12:g\n12@1-3:\n");
}

/// from-import `Response` (the `import Name from module` selective form).
#[test]
fn synthetic_struct_from_import_ctor_parity() {
    let out = golden_entry(
        "import Response from std.request\nr := Response(200, \"ok\", {\"k\": \"v\"})\nprint(\"{r.status}\" + r.body + r.headers[\"k\"])\n",
    );
    assert_eq!(out, "200okv\n");
}

#[test]
fn synthetic_procresult_qualified_ctor_parity() {
    let out = golden_entry(
        "import std.process\np := process.ProcResult(\"out\", \"err\", 7)\nprint(p.stdout + p.stderr + \"{p.code}\")\n",
    );
    assert_eq!(out, "outerr7\n");
}

/// Phase 4f — std.process + std.request are now FILE-BACKED (`std/process.chz`, `std/request.chz`),
/// their whole signature (fields-only `native struct` ProcResult/Response + the `native fn`s, incl.
/// request's OPTIONAL trailing `timeout_ms`) harvested via `harvest_native_module`, retiring both the
/// `native_module_sig` fn arms AND the `export_struct` arms. This must be a ZERO observable behavior
/// change: constructing ProcResult/Response, reading every field, `from std.X import Type`,
/// qualified `X.Type(...)`, and request.get with AND without the optional `timeout_ms` stay
/// byte-identical. (Only ctors +
/// field reads are exercised at runtime — request.get would need a live server — but the optional
/// `timeout_ms` arg still TYPECHECKS both ways here, proving the harvested optional-tail sig.)
#[test]
fn process_request_file_backed_runs() {
    let src = "import std.process\n\
                   import std.request\n\
                   import ProcResult from std.process\n\
                   import Response from std.request\n\
                   fn describe_proc(p: ProcResult) -> str:\n\
                   \x20   return p.stdout + \"|\" + p.stderr + \"|\" + \"{p.code}\"\n\
                   fn describe_resp(r: Response) -> str:\n\
                   \x20   return \"{r.status}\" + \"|\" + r.body + \"|\" + r.headers[\"k\"]\n\
                   pr: ProcResult = process.ProcResult(\"out\", \"err\", 7)\n\
                   rp: Response = request.Response(200, \"body\", {\"k\": \"v\"})\n\
                   print(describe_proc(pr))\n\
                   print(describe_resp(rp))\n";
    let t = TmpDir::new();
    let path = t.write("main.chz", src);
    let (out, _e, res, _) = run_file(&path);
    res.expect("process/request ctor program should run");
    assert_eq!(out, "out|err|7\n200|body|v\n");
}

/// Phase 4f — the pure TYPE `import ProcResult from std.process` (no runtime module-member value)
/// must NOT fault (the `bind_import` skip for a pure type).
#[test]
fn pure_type_import_no_fault() {
    let out = golden_entry("import ProcResult from std.process\nprint(1)\n");
    assert_eq!(out, "1\n");
    let out2 = golden_entry("import Response from std.request\nprint(2)\n");
    assert_eq!(out2, "2\n");
}

/// A user `struct Response` WITHOUT `import std.request` is the user's OWN type —
/// the synthetic name is freed (the `Builtin`-origin seed is shadowed by the user declaration).
#[test]
fn user_struct_shadows_synthetic_name_parity() {
    assert_eq!(
        golden_entry("struct Response:\n    code: int\nr := Response(7)\nprint(\"{r.code}\")\n"),
        "7\n"
    );
    assert_eq!(
        golden_entry("struct Match:\n    score: int\nm := Match(3)\nprint(\"{m.score}\")\n"),
        "3\n"
    );
}

/// `import geo` then `geo.Color.Red` (nullary) and `geo.Shape.Circle(5)` (payload) construct.
#[test]
fn qualified_enum_ctor_parity() {
    let out = golden_file_entry(
        &[
            (
                "geo.chz",
                "enum Color:\n    Red\n    Green\nenum Shape:\n    Circle(int)\n",
            ),
            (
                "main.chz",
                "import geo\nc := geo.Color.Red\ns := geo.Shape.Circle(5)\nmatch c:\n    Color.Red: print(\"red\")\n    Color.Green: print(\"green\")\nmatch s:\n    Shape.Circle(r): print(r)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "red\n5\n");
}

/// `import Color from types` (enum) constructs bare.
#[test]
fn from_import_enum_ctor_parity() {
    let out = golden_file_entry(
        &[
            ("types.chz", "enum Color:\n    Red\n    Green\n"),
            (
                "main.chz",
                "import Color from types\nc := Color.Green\nmatch c:\n    Color.Red: print(\"r\")\n    Color.Green: print(\"g\")\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "g\n");
}

/// Two modules both declare `struct Point` with DIFFERENT layouts, both reachable in the entry
/// program: a REAL collision. Each constructs correctly; the entry/first keeps the bare key
/// (`Point(...)`), the other is disambiguated. Byte-identical against a literal golden.
#[test]
fn real_collision_two_modules_same_struct() {
    let out = golden_file_entry(
        &[
            ("a.chz", "struct Point:\n    x: int\n"),
            ("b.chz", "struct Point:\n    y: int\n    z: int\n"),
            (
                "main.chz",
                "import a\nimport b\npa := a.Point(1)\npb := b.Point(2, 3)\nprint(pa.x)\nprint(pb.y)\nprint(pb.z)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "1\n2\n3\n");
}

/// Blocker C: two modules declare `enum Color`, both reachable. `cb.classify` MATCHES on its own
/// `Color.Red`/`Color.Green`. The disambiguated enum's variants must MATCH (not just construct):
/// the match-pattern side must use the SAME module-scoped runtime key as construction.
#[test]
fn enum_collision_match_in_declaring_module() {
    let out = golden_file_entry(
        &[
            ("ca.chz", "enum Color:\n    Red\n    Green\n"),
            (
                "cb.chz",
                "enum Color:\n    Red\n    Green\nfn classify(c: Color) -> int:\n    return match c:\n        Color.Red: 1\n        Color.Green: 2\n",
            ),
            (
                "cmain.chz",
                "import ca\nimport cb\nprint(cb.classify(cb.Color.Red))\nprint(cb.classify(cb.Color.Green))\n",
            ),
        ],
        "cmain.chz",
    );
    assert_eq!(out, "1\n2\n");
}

/// Blocker C (no-collision twin): a SINGLE-module enum matched within its own declaring module
/// must stay byte-identical (the common-case key is BARE). Guards the match-side key resolution
/// from regressing the non-colliding path.
#[test]
fn enum_match_same_module_no_collision() {
    let out = golden_entry(
        "enum Color:\n    Red\n    Green\nfn classify(c: Color) -> int:\n    return match c:\n        Color.Red: 1\n        Color.Green: 2\nprint(classify(Color.Red))\nprint(classify(Color.Green))\n",
    );
    assert_eq!(out, "1\n2\n");
}

/// Regression: a `from`-imported FUNCTION named like SOME OTHER module's type must still bind +
/// call as a function — the from-import type-skip is keyed on the TARGET module's types, NOT a
/// program-wide type-name set, and the bare ctor only fires for a bare-VISIBLE type (not one
/// merely present in the global table). Without both gates, `Foo()` wrongly hit B's struct ctor.
#[test]
fn from_imported_fn_named_like_another_modules_type() {
    let out = golden_file_entry(
        &[
            ("a.chz", "fn Foo() -> int:\n    return 42\n"),
            ("b.chz", "struct Foo:\n    x: int\n"),
            ("main.chz", "import Foo from a\nimport b\nprint(Foo())\n"),
        ],
        "main.chz",
    );
    assert_eq!(out, "42\n");
}

/// Blocker C (variant-key double-resolution): a module BOTH imports a colliding enum (`win.E`,
/// the load-order winner keyed BARE) AND declares its OWN same-named loser enum (`mid::E`), then
/// QUALIFIED-constructs the imported one (`win.E.A`) and passes it to the importer's fn, which
/// MATCHES it in `win`'s context. The construction's variant_id must key on `win`'s E (the call
/// site's already-resolved key), NOT be re-derived from the currently-compiled module (`mid`)'s
/// `bare_types` — else the producer bakes `mid::E::A` and `win.pick`'s match never fires. Must be
/// byte-identical against the golden (this guards the VM).
#[test]
fn enum_collision_construct_in_other_declaring_module() {
    let out = golden_file_entry(
        &[
            (
                "win.chz",
                "enum E:\n    A\n    B\n\nfn pick(e: E) -> int:\n    match e:\n        E.A: return 1\n        E.B: return 2\n",
            ),
            (
                "mid.chz",
                "import win\n\nenum E:\n    A\n    B\n\nfn go() -> int:\n    return win.pick(win.E.A)\n",
            ),
            ("main.chz", "import win\nimport mid\nprint(mid.go())\n"),
        ],
        "main.chz",
    );
    assert_eq!(out, "1\n");
}

/// No-collision twin: same layout, but the constructing module does NOT declare its own `E`
/// (single declarer `win`, bare key). Proves the common path (qualified construct of an imported
/// enum from an importer) is untouched. Passes before AND after the fix (regression guard).
#[test]
fn enum_no_collision_construct_in_importing_module() {
    let out = golden_file_entry(
        &[
            (
                "win.chz",
                "enum E:\n    A\n    B\n\nfn pick(e: E) -> int:\n    match e:\n        E.A: return 1\n        E.B: return 2\n",
            ),
            (
                "mid.chz",
                "import win\n\nfn go() -> int:\n    return win.pick(win.E.A)\n",
            ),
            ("main.chz", "import win\nimport mid\nprint(mid.go())\n"),
        ],
        "main.chz",
    );
    assert_eq!(out, "1\n");
}

/// A spawned task constructs an imported struct AND a value crosses a Channel (data-not-time):
/// the run must produce `7`.
#[test]
fn imported_struct_crosses_channel_from_task() {
    let files = [
        ("geo.chz", "struct Point:\n    x: int\n    y: int\n"),
        (
            "main.chz",
            "import geo\nch := Channel[geo.Point]()\nparallel:\n    spawn:\n        ch.send(geo.Point(3, 4))\np := ch.recv() else: panic(\"ch closed\")\nprint(p.x + p.y)\n",
        ),
    ];
    let t = TmpDir::new();
    let mut entry = None;
    for (rel, c) in &files {
        let p = t.write(rel, c);
        if *rel == "main.chz" {
            entry = Some(p);
        }
    }
    let entry = entry.unwrap();
    let (out, _, res, _) = run_file(&entry);
    assert!(res.is_ok(), "the run faulted: {res:?}");
    assert_eq!(out, "7\n");
}

// ----- ROOT REDESIGN: always-qualified identity key + bare display name -----

/// THE BUG (collision-loser decode against the WRONG layout): the entry module declares its OWN
/// `Point{a,b}` while a dep also declares `Point{x}`. A bare `json.decode[Point]` must decode
/// against the ENTRY's layout (the bare name resolves to the entry's `Point`), printing 5 — not
/// fail `decode: missing key 'x'` against the dep's layout.
#[test]
fn decode_collision_loser_against_correct_layout() {
    let out = golden_file_entry(
        &[
            ("dep.chz", "struct Point:\n    x: int\n"),
            (
                "main.chz",
                "import std.json\nimport dep\nstruct Point:\n    a: int\n    b: int\np := json.decode[Point](\"{{\\\"a\\\":5,\\\"b\\\":9}}\")\nmatch p:\n    ?v: print(v.a)\n    !e: print(e)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "5\n");
}

/// Twin: a QUALIFIED `json.decode[dep.Point]` decodes against the dep's layout (`x`), printing 7.
#[test]
fn decode_qualified_target() {
    let out = golden_file_entry(
        &[
            ("dep.chz", "struct Point:\n    x: int\n"),
            (
                "main.chz",
                "import std.json\nimport dep\nstruct Point:\n    a: int\n    b: int\np := json.decode[dep.Point](\"{{\\\"x\\\":7}}\")\nmatch p:\n    ?v: print(v.x)\n    !e: print(e)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "7\n");
}

/// BARE DISPLAY ON COLLISION: two modules both declare `struct Point` with different layouts;
/// printing each value shows the BARE name (`Point(x=1)` / `Point(y=2)`) on BOTH — never the
/// module-qualified identity key.
#[test]
fn collision_prints_bare_for_both() {
    let out = golden_file_entry(
        &[
            ("a.chz", "struct Point:\n    x: int\n"),
            ("b.chz", "struct Point:\n    y: int\n"),
            (
                "main.chz",
                "import a\nimport b\nprint(a.Point(1))\nprint(b.Point(2))\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "Point(x=1)\nPoint(y=2)\n");
}

/// NESTED-COLLISION FIELD DECODE: `dep` has `Wrap{inner: Inner}` + `Inner{k}`; the entry declares
/// its OWN `Inner{other}`. `json.decode[dep.Wrap]` must expand the nested `Inner` field in dep's
/// DEFINING scope (`dep::Inner` with `k`), not the entry's. Prints 3.
#[test]
fn decode_nested_struct_field_in_defining_module() {
    let out = golden_file_entry(
        &[
            (
                "dep.chz",
                "struct Inner:\n    k: int\nstruct Wrap:\n    inner: Inner\n",
            ),
            (
                "main.chz",
                "import std.json\nimport dep\nstruct Inner:\n    other: int\np := json.decode[dep.Wrap](\"{{\\\"inner\\\":{{\\\"k\\\":3}}}}\")\nmatch p:\n    ?w: print(w.inner.k)\n    !e: print(e)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "3\n");
}

/// The collision-decode repro must produce `5`.
#[test]
fn decode_collision() {
    let files = [
        ("dep.chz", "struct Point:\n    x: int\n"),
        (
            "main.chz",
            "import std.json\nimport dep\nstruct Point:\n    a: int\n    b: int\nmatch json.decode[Point](\"{{\\\"a\\\":5,\\\"b\\\":9}}\"):\n    ?v: print(v.a)\n    !e: print(e)\n",
        ),
    ];
    let t = TmpDir::new();
    let mut entry = None;
    for (rel, c) in &files {
        let p = t.write(rel, c);
        if *rel == "main.chz" {
            entry = Some(p);
        }
    }
    let entry = entry.unwrap();
    let (out, _, res, _) = run_file(&entry);
    assert!(res.is_ok(), "the run faulted: {res:?}");
    assert_eq!(out, "5\n");
}

/// BLOCKER 2/3 — a bare match-pattern enum qualifier (`Color.Red`) whose enum is reachable only
/// via WHOLE-MODULE import must be resolved from the SCRUTINEE'S static enum, not re-guessed by
/// iterating the (RandomState-seeded) import map. Two whole-imported same-named enums; scrutinee
/// from `a`; the `Color.Red`/`Color.Blue` arms must resolve against `a::Color`. Looped 50x to
/// defeat the nondeterminism (one pass could luck-pass); must print `red` every time.
#[test]
fn match_arm_scrutinee_driven() {
    let files = [
        ("a.chz", "enum Color:\n    Red\n    Blue\n"),
        ("b.chz", "enum Color:\n    Red\n    Green\n"),
        (
            "main.chz",
            "import a\nimport b\nc := a.Color.Red\nmatch c:\n    Color.Red: print(\"red\")\n    Color.Blue: print(\"blue\")\n",
        ),
    ];
    let t = TmpDir::new();
    let mut entry = None;
    for (rel, c) in &files {
        let p = t.write(rel, c);
        if *rel == "main.chz" {
            entry = Some(p);
        }
    }
    let entry = entry.unwrap();
    for _ in 0..50 {
        let (out, _, res, _) = run_file(&entry);
        assert!(res.is_ok(), "the run faulted: {res:?}");
        assert_eq!(out, "red\n", "output wrong");
    }
}

/// BLOCKER 2/3 divergence case — `import b` FIRST, then `import a`; scrutinee is `a.Color.Blue`,
/// a variant that exists ONLY in `a`. A scrutinee-blind import-iterating resolver picked `b`
/// (which has no `Blue`) and either crashed or matched the wrong arm. The
/// match-arm key MUST come from the scrutinee's `a::Color`. Looped 50x to defeat nondeterminism.
#[test]
fn match_arm_only_in_a_variant() {
    let files = [
        ("a.chz", "enum Color:\n    Red\n    Blue\n"),
        ("b.chz", "enum Color:\n    Red\n    Green\n"),
        (
            "main.chz",
            "import b\nimport a\nc := a.Color.Blue\nmatch c:\n    Color.Red: print(\"red\")\n    Color.Blue: print(\"blue\")\n",
        ),
    ];
    let t = TmpDir::new();
    let mut entry = None;
    for (rel, c) in &files {
        let p = t.write(rel, c);
        if *rel == "main.chz" {
            entry = Some(p);
        }
    }
    let entry = entry.unwrap();
    for _ in 0..50 {
        let (out, _, res, _) = run_file(&entry);
        assert!(res.is_ok(), "the run faulted: {res:?}");
        assert_eq!(out, "blue\n", "output wrong");
    }
}

/// CROSSING: a colliding struct value sent through a Channel reaches the other side and
/// prints its BARE name there.
#[test]
fn collision_struct_sent_across_task() {
    let files = [
        ("a.chz", "struct Point:\n    x: int\n"),
        ("b.chz", "struct Point:\n    y: int\n"),
        (
            "main.chz",
            "import a\nimport b\nch := Channel[b.Point]()\nparallel:\n    spawn:\n        ch.send(b.Point(9))\np := ch.recv() else: panic(\"ch closed\")\nprint(p)\nprint(p.y)\n",
        ),
    ];
    let t = TmpDir::new();
    let mut entry = None;
    for (rel, c) in &files {
        let p = t.write(rel, c);
        if *rel == "main.chz" {
            entry = Some(p);
        }
    }
    let entry = entry.unwrap();
    let (out, _, res, _) = run_file(&entry);
    assert!(res.is_ok(), "the run faulted: {res:?}");
    assert_eq!(out, "Point(y=9)\n9\n");
}

// ----- C5 (A2): program-exit auto-drain is skipped on a hard `os.exit` -----

#[test]
fn executor_autodrain_skipped_on_os_exit() {
    // `os.exit` is a hard halt — like `defer`, the program-exit auto-drain is skipped, so a
    // submitted-but-un-shut executor's job is never joined. The job waits on a gate that `main`
    // never opens: an Executor job starts at `submit` (eager), so a job free to run could print
    // before the exit (Go: a goroutine's writes before `os.Exit` are on stdout), and since
    // TICKET-270 the captured sink keeps that output. A drain that joined the held job would end
    // in a deadlock fault, not a clean exit 0. Driven through the file path since it imports std.os.
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        "import Executor from std.concurrency\nimport std.os\ngate := Channel[int]()\nfn j():\n    _ := gate.recv()\n    print(\"RAN\")\nfn main():\n    ex := Executor()\n    ex.submit(j)\n    print(\"before exit\")\n    os.exit(0)\nmain()\n",
    );
    let (out, _err, result, code) = crate::vm::run_file_engine(
        &entry,
        crate::native::HostConfig::default(),
        None,
        None,
        false,
    );
    assert_eq!(captured(out), "before exit\n");
    assert!(result.is_ok(), "the exit is clean: {result:?}");
    assert_eq!(code, Some(0));
}

// ----- Executor.submit hands a job its closure BY HANDLE -----
// The job holds the submitted closure's captures by handle, like any value that crosses tasks.

#[test]
fn executor_submit_generator_capturing_closure_crosses_by_value() {
    // F3 path C: a submitted closure captures a LIVE local generator (`it`, Pending). M3.4: the job
    // drives the parent's generator by handle. WAS: this faulted the generator crossing.
    let src = "import Executor from std.concurrency\n\
fn gen() -> Iterator[int]:
    yield 1
fn main():
    it := gen()
    fn task():
        for x in it:
            print(x)
    ex := Executor()
    ex.submit(task)
    ex.shutdown()
main()";
    assert_eq!(golden_entry(src), "1\n");
}

#[test]
fn executor_submit_mutating_closure_lands_parity() {
    // Repro #3: a submitted closure captures + mutates a local list, observed after the drain. M3.4:
    // the job holds the submitter's `box` by handle, so its push lands (Go, a goroutine appends to
    // a captured slice through a pointer: `2`).
    let out = golden_entry(
        "import Executor from std.concurrency\nfn bump_box(b):\n    b.push(1)\nfn main():\n    box := [0]\n    ex := Executor()\n    ex.submit(fn(): bump_box(box))\n    ex.shutdown()\n    print(box.len())\nmain()\n",
    );
    assert_eq!(out, "2\n");
}

#[test]
fn executor_submit_module_global_inplace_mutation_lands_parity() {
    // Task 1 (Executor path) — a submitted closure that mutates a MODULE-GLOBAL aggregate in place
    // writes the run's one `xs` (M3.1). Go (go1.27.0, `m31go/rust`): `4`.
    let out = golden_entry(
        "import Executor from std.concurrency\nxs := [1, 2, 3]\nfn bump_xs():\n    xs.push(99)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): bump_xs())\n    ex.shutdown()\n    print(xs.len())\nmain()\n",
    );
    assert_eq!(out, "4\n");
}

#[test]
fn executor_submit_module_global_callee_reassign_lands_parity() {
    // Task 1 (Executor path) — a submitted closure whose free-fn callee REASSIGNS a module global
    // (`bump()` does `count = count + 1`). M3.1: both jobs write the run's one `count`;
    // `Executor(1)` runs them one after the other, so they never race. Go (go1.27.0, `m31go/rust`):
    // `2`.
    let out = golden_entry(
        "import Executor from std.concurrency\ncount := 0\nfn bump():\n    count = count + 1\nfn main():\n    ex := Executor(1)\n    ex.submit(fn(): bump())\n    ex.submit(fn(): bump())\n    ex.shutdown()\n    print(count)\nmain()\n",
    );
    assert_eq!(out, "2\n");
}

#[test]
fn executor_submit_atomic_visible_to_parent_parity() {
    // Task 1 (Executor escape hatch) — an `Atomic` module global is the run's one global (M3.1), so a
    // task-side `add` IS visible to the parent. It once guarded that the per-task snapshot kept the
    // `Arc` (trap #1) on the Executor path too.
    let out = golden_entry(
        "import Atomic, Executor from std.concurrency\na := Atomic(0)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): a.add(1))\n    ex.submit(fn(): a.add(1))\n    ex.shutdown()\n    print(a.load())\nmain()\n",
    );
    assert_eq!(out, "2\n");
}

#[test]
fn executor_submit_sendable_closure_runs_parity() {
    // Control: a captured `Channel` is a genuinely-shared handle (crosses as its shared Arc, NOT
    // deep-copied), so a value sent from inside the submitted job is visible to the parent's `recv`.
    // Must stay `7` — the by-value collapse must not over-isolate a shared handle.
    let out = golden_entry(
        "import Executor from std.concurrency\nfn main():\n    ch := Channel[int]()\n    ex := Executor()\n    ex.submit(fn(): ch.send(7))\n    ex.shutdown()\n    print(ch.recv())\nmain()\n",
    );
    assert_eq!(out, "7\n");
}

// ----- M9: std.regex parity (exercises NativeRet::Struct lowering) -----

#[test]
fn regex_find_all_replace_split_parity() {
    let out = golden_entry(
        r##"import std.regex
match regex.find_all("[0-9]+", "a1 22 333"):
    ?ms:
        for m in ms:
            print(m.text)
    !e: print(e)
match regex.replace_all("[0-9]+", "a1b22c", "#"):
    ?s: print(s)
    !e: print(e)
match regex.split(",", "a,b,c"):
    ?parts: print("|".join(parts))
    !e: print(e)
"##,
    );
    assert_eq!(out, "1\n22\n333\na#b#c\na|b|c\n");
}

/// `std.request` against a loopback server (exercises `NativeRet::Map` lowering). The server serves
/// one canned response for the run's one connection.
#[test]
fn request_get_parity_against_local_server() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let body = "pong";
        let resp = format!(
            "HTTP/1.1 200 OK\r\nX-Test: hi\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(resp.as_bytes()).unwrap();
    });

    let src = format!(
        "import std.request\nmatch request.get(\"http://{addr}/\"):\n    ?resp:\n        print(\"{{resp.status}}\")\n        print(resp.body)\n        print(resp.headers[\"x-test\"])\n    !e: print(e)\n"
    );
    let out = golden_entry(&src);
    server.join().unwrap();
    assert_eq!(out, "200\npong\nhi\n");
}

/// W14-30 — a response header sent more than once is JOINED with `, ` (Python `requests`, RFC 9110
/// list form), not reduced to its first value. `Set-Cookie: a=1` + `Set-Cookie: b=2` → `a=1, b=2`;
/// a single-valued header is untouched.
#[test]
fn request_joins_duplicate_headers_against_local_server() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let resp = "HTTP/1.1 200 OK\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nX-One: solo\r\nConnection: close\r\nContent-Length: 2\r\n\r\nok";
        stream.write_all(resp.as_bytes()).unwrap();
    });

    let src = format!(
        "import std.request\nmatch request.get(\"http://{addr}/\"):\n    ?resp:\n        print(resp.headers[\"set-cookie\"])\n        print(resp.headers[\"x-one\"])\n    !e: print(e)\n"
    );
    let out = golden_entry(&src);
    server.join().unwrap();
    assert_eq!(out, "a=1, b=2\nsolo\n");
}

/// W14-30b — a response header whose value holds a non-ASCII byte must still appear in
/// `resp.headers` (CPython `urllib` surfaces it; RFC 9110 field values are opaque bytes). The value
/// is decoded latin-1 (byte -> code point), so UTF-8 `café` on the wire reads back as 5 chars.
#[test]
fn request_keeps_non_ascii_header_against_local_server() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let resp = "HTTP/1.1 200 OK\r\nX-Ascii: ok\r\nX-Cafe: café\r\nConnection: close\r\nContent-Length: 2\r\n\r\nhi";
        stream.write_all(resp.as_bytes()).unwrap();
    });

    let src = format!(
        "import std.request\nmatch request.get(\"http://{addr}/\"):\n    ?resp:\n        print(resp.headers[\"x-ascii\"])\n        print(\"x-cafe\" in resp.headers)\n    !e: print(e)\n"
    );
    let out = golden_entry(&src);
    server.join().unwrap();
    assert_eq!(out, "ok\ntrue\n");
}

/// W14-30b — the Chezzi-visible half: a UTF-8 `café` header value reads back latin-1 decoded
/// (`cafÃ©`), as CPython's `urllib` does, not dropped and not replaced with U+FFFD.
#[test]
fn request_decodes_a_non_ascii_header_value_latin1() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let resp =
            "HTTP/1.1 200 OK\r\nX-Cafe: café\r\nConnection: close\r\nContent-Length: 2\r\n\r\nhi";
        stream.write_all(resp.as_bytes()).unwrap();
    });

    let src = format!(
        "import std.request\nmatch request.get(\"http://{addr}/\"):\n    ?resp: print(resp.headers[\"x-cafe\"])\n    !e: print(e)\n"
    );
    let out = golden_entry(&src);
    server.join().unwrap();
    assert_eq!(out, "cafÃ©\n");
}

/// `std.request` new verbs + custom headers, against a loopback server that records every request's
/// wire bytes. The run issues a `put` and a header-carrying `request("DELETE", …)`, so the
/// server accepts twice. Asserts (a) the expected stdout and (b) the right method line + custom
/// header reached the wire — locking the off-heap `NativeArg::Map` headers path and the verb
/// wrappers.
#[test]
fn request_verbs_and_headers_parity_against_local_server() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_srv = Arc::clone(&seen);
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).unwrap_or(0);
            seen_srv
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf[..n]).into_owned());
            // `Connection: close` so ureq's thread-local pool never reuses a server-closed socket —
            // one fresh conn per request.
            let resp = "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nok";
            stream.write_all(resp.as_bytes()).unwrap();
        }
    });

    // A PUT verb wrapper, then a header-carrying general DELETE.
    let src = format!(
        "import std.request\nmatch request.put(\"http://{addr}/\", \"payload\"):\n    ?r: print(\"{{r.status}}\")\n    !e: print(e)\nmatch request.request(\"DELETE\", \"http://{addr}/\", \"\", {{\"X-Custom\": \"value\"}}):\n    ?r: print(\"{{r.status}}\")\n    !e: print(e)\n"
    );
    let out = golden_entry(&src);
    server.join().unwrap();
    assert_eq!(out, "200\n200\n", "both requests must succeed");

    let reqs = seen.lock().unwrap();
    assert_eq!(reqs.len(), 2, "one PUT and one DELETE");
    let puts = reqs.iter().filter(|r| r.starts_with("PUT ")).count();
    let deletes = reqs.iter().filter(|r| r.starts_with("DELETE ")).count();
    let with_header = reqs
        .iter()
        // Lowercased on the wire: the `http` crate normalizes every `HeaderName` (RFC 9110 field
        // names are case-insensitive), so ureq 3 sends `x-custom`, not ureq 2's `X-Custom`.
        .filter(|r| r.contains("x-custom: value"))
        .count();
    assert_eq!(puts, 1, "must send PUT");
    assert_eq!(deletes, 1, "must send DELETE");
    assert_eq!(with_header, 1, "the custom header must reach the wire");
}

#[test]
fn regex_find_groups_and_span_parity() {
    let out = golden_entry(
        r#"import std.regex
match regex.find("([a-z]+)@([a-z]+)", "xx ann@host"):
    ?opt:
        match opt:
            ?m: print(m.text + " " + "{m.start}" + " " + ",".join(m.groups))
            None: print("none")
    !e: print(e)
"#,
    );
    assert_eq!(out, "ann@host 3 ann,host\n");
}

// ----- break / continue parity (must produce the right output) -----

// ----- Phase 0 scalar fills: the Result-returning str parses -----

#[test]
fn to_int_parity() {
    // `s.to_int() -> int!`: the error names the cause.
    let src = r#"
fn main():
    print("42".to_int())
    print("  7 ".to_int())
    print("x".to_int())

main()
"#;
    assert_golden_out(src, "42\n7\n!to_int: cannot parse 'x' as int\n");
}

#[test]
fn to_float_parity() {
    // `s.to_float() -> float!`: the error names the cause.
    let src = r#"
fn main():
    print("3.14".to_float())
    print("x".to_float())

main()
"#;
    assert_golden_out(src, "3.14\n!to_float: cannot parse 'x' as float\n");
}
