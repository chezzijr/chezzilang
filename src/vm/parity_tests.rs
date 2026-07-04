// Extracted from vm/mod.rs (test module). `super::` == the `vm` module.
//! Cross-engine parity: the serial VM (`parallel=false`) and the M:N VM (`parallel=true`) must
//! agree on stdout *and* error for every program. NB: both drive the same `Vm` bytecode, so for a
//! sequential program this is a determinism check on one engine; the differential bite is on
//! concurrent programs (scheduler/airlock/fault-report), where the two paths genuinely differ.
//! (Historically these compared the VM against a separate tree-walk interpreter, since removed.)
use super::*;
use std::path::PathBuf;

/// Outcome of a run, normalized so the serial and M:N VM results compare directly.
fn parallel_outcome(src: &str) -> Result<String, String> {
    run_capture_parallel(src).map_err(|e| e.to_string())
}
fn vm_outcome(src: &str) -> Result<String, String> {
    run_capture(src).map_err(|e| e.to_string())
}

fn assert_parity(src: &str) {
    assert_eq!(
        vm_outcome(src),
        parallel_outcome(src),
        "VM/interp divergence for:\n{src}"
    );
}

/// Native-prelude phase 2a — the scalar-conversion CTORS (int/float/str/bytes/bytearray) are now
/// sourced from the synthetic PRELUDE table (`Intrinsic::Ctor`) instead of hard-coded arms. This
/// is a pure metadata refactor: the runtime `do_builtin` dispatch is unchanged, so every conversion
/// must produce byte-identical output on BOTH engines (VM == interp), including the base-16 int
/// parse, float rendering, str-of-scalar, and the byte-buffer ctors.
#[test]
fn scalar_ctor_conversions_parity() {
    let src = r#"
fn main():
    print(int("5"))
    print(int("-42"))
    print(int(3.9))
    print(float("1.5"))
    print(float(2))
    print(str(123))
    print(str(4.5))
    print(str(true))
    b := bytes([104, 105])
    print(b.len())
    print(b[0])
    print(b[1])
    ba := bytearray([65, 66])
    print(ba.len())
    print(ba[0])
    ba.push(67)
    print(ba.len())
    print(ba[2])

main()
"#;
    assert_parity(src);
}

/// Native-prelude phase 2b — the GENERIC / reserved-type container CTORS (range/List/Map/Set) are
/// now sourced from the synthetic PRELUDE table (`Intrinsic::Ctor`) for their `CallBuiltin`
/// DISPATCH, exactly as 2a did for the scalar ctors. Their generic type-identity resolution is
/// untouched, so every construction + generic-inference path must stay byte-identical on BOTH
/// engines (VM == interp): range overloads, empty/turbofished containers, dedup, iteration.
#[test]
fn container_ctor_parity() {
    let src = r#"
fn main():
    print(range(5).len())
    for i in range(1, 10, 2):
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
    s := Set([1, 1, 2, 3, 3])
    print(s.len())
    s2 := Set[int]()
    print(s2.len())

main()
"#;
    assert_parity(src);
}

/// M19 SSO — string ops must stay byte-identical across both engines for strings that straddle
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
    print(c[0:22])                    # 22 bytes — inline
    print(c[0:23])                    # 23 bytes — heap
    print(ab[0:22])

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
    assert_parity(src);
}

use std::sync::atomic::{AtomicUsize, Ordering};
static PARITY_TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = PARITY_TMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_par_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).unwrap();
        p
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// ONE-WAY RATCHET (supersedes commit 9829f94): a reserved builtin TYPE name used as a user
/// generic type parameter is REJECTED at check time with `type 'X' is reserved (builtin)` — it is
/// NOT silently shadowed into a usable type param. 9829f94 made these shadow-and-run (the
/// behavior this guards-against now), which violated the reserved-name discipline (a scalar param
/// is dead/unreferenceable; a container/enum-builtin param silently shadows the builtin). The
/// reject happens in `check_graph` (the gate the bare `run_capture` helpers skip), so the program
/// never reaches any engine — three-engine parity is by construction (no run needed). Asserted via
/// the full `build_graph` + `check_graph` CLI path.
#[test]
fn type_param_named_like_reserved_rejected_at_check() {
    for name in ["Executor", "ptr", "Socket", "Listener", "owned_str"] {
        let src = format!(
            "fn id[{name}](x: {name}) -> {name}:\n    return x\nfn main():\n    print(id(1))\nmain()\n"
        );
        let t = TmpDir::new();
        let entry = t.write("main.chz", &src);
        let graph = crate::resolver::build_graph(&entry).expect("resolve");
        match crate::checker::check_graph(&graph) {
            Ok(()) => panic!("type param named {name} must be rejected as reserved (builtin)"),
            Err(errs) => assert!(
                errs.iter()
                    .any(|e| e.message.contains("reserved (builtin)")),
                "type param named {name} must error reserved (builtin), got: {errs:?}"
            ),
        }
    }
}

/// Run a multi-file program (one or more `.chz` files) through BOTH engines via `run_file`,
/// assert they agree on stdout and on ok/err, and return the agreed stdout. `files` is
/// `(relative_path, contents)`; `entry` names the file to run. Needed because the single-file
/// `assert_parity` can't exercise imports (and std modules require the import path).
fn assert_parity_file(files: &[(&str, &str)], entry: &str) -> String {
    let t = TmpDir::new();
    let mut entry_path = None;
    for (rel, contents) in files {
        let p = t.write(rel, contents);
        if *rel == entry {
            entry_path = Some(p);
        }
    }
    let entry_path = entry_path.expect("entry must be one of the files");
    let (io, ie_out, ir, _) = run_file_p(&entry_path);
    let (vo, ve_out, vr, _) = run_file(&entry_path);
    assert_eq!(io, vo, "stdout divergence (interp vs vm) for entry {entry}");
    assert_eq!(
        ie_out, ve_out,
        "stderr divergence (interp vs vm) for entry {entry}"
    );
    match (&ir, &vr) {
        (Ok(()), Ok(())) => {}
        (Err(ie), Err(ve)) => {
            assert_eq!(
                ie.to_string(),
                ve.to_string(),
                "error divergence (interp vs vm)"
            );
        }
        _ => panic!("ok/err divergence: interp={ir:?} vm={vr:?}"),
    }
    io
}

/// Convenience: a single entry file (the common std-module case).
fn parity_entry(src: &str) -> String {
    assert_parity_file(&[("main.chz", src)], "main.chz")
}

// ----- module-scoped user types: cross-module construction parity -----

/// `import geo; geo.Point(1,2)` constructs and prints `Point(x=1, y=2)` (BARE name, no `::`),
/// byte-identical on both engines.
#[test]
fn qualified_struct_ctor_parity() {
    let out = assert_parity_file(
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
    let out = assert_parity_file(
        &[
            ("types.chz", "struct S:\n    n: int\n"),
            ("main.chz", "import S from types\nprint(S(7))\n"),
        ],
        "main.chz",
    );
    assert_eq!(out, "S(n=7)\n");
}

/// Task 3/5: the module-owned synthetic structs (`Match`/`Response`/`ProcResult`) are now
/// USER-CONSTRUCTIBLE once their module is imported — the VM and the frozen interp must build +
/// field-read them BYTE-IDENTICALLY (the layout seeded in `Compiler::new` ↔ the interp's native
/// seed must agree). Qualified ctor + bare ctor on whole-module import.
#[test]
fn synthetic_struct_qualified_ctor_parity() {
    let out = parity_entry(
        "import std.regex\nm := regex.Match(\"hi\", 0, 2, [\"a\"])\nprint(m.text + str(m.start) + str(m.end) + \",\".join(m.groups))\n",
    );
    assert_eq!(out, "hi02a\n");
}

#[test]
fn synthetic_struct_bare_ctor_on_import_parity() {
    let out = parity_entry(
        "import std.regex\nm: Match = Match(\"yo\", 1, 3, [])\nprint(m.text + str(m.end))\n",
    );
    assert_eq!(out, "yo3\n");
}

/// Phase 4b — the whole regex SIGNATURE (the `native struct Match` + the 5 `native fn`s) now comes
/// from the file-backed `std/regex.chz` (harvested into std.regex's ModuleSig via
/// `harvest_native_module`), replacing BOTH the phase-4a companion stub and the hand-built
/// `native_module_sig` arm. This must be a ZERO observable behavior change on ALL THREE engines:
/// producing a `Match` (regex.find), reading every field, `from std.regex import Match`, and
/// qualified `regex.Match(...)` are byte-identical on interp, the cooperative VM, AND the M:N
/// OS-thread engine. This is the regression net proving the native-seam pure-type `bind_import` skip
/// still holds in both engines (`from std.regex import Match` must NOT bind a runtime value / fault).
/// The origin=Builtin force in `harvest_native_module` is what keeps that skip correct.
#[test]
fn regex_match_file_backed_three_engine_parity() {
    let src = "import std.regex\n\
                   import Match from std.regex\n\
                   fn describe(m: Match) -> str:\n\
                   \x20   return m.text + \"@\" + str(m.start) + \"-\" + str(m.end) + \":\" + \",\".join(m.groups)\n\
                   lit: Match = regex.Match(\"lit\", 9, 12, [\"g\"])\n\
                   print(describe(lit))\n\
                   match regex.find(\"[0-9]+\", \"a12b\"):\n\
                   \x20   Ok(opt):\n\
                   \x20       match opt:\n\
                   \x20           Some(m): print(describe(m))\n\
                   \x20           None: print(\"none\")\n\
                   \x20   Err(e): print(e)\n";
    // VM(serial) + interp byte-identical (the standard two-engine parity gate).
    let out = parity_entry(src);
    assert_eq!(out, "lit@9-12:g\n12@1-3:\n");
    // …and the M:N OS-thread engine agrees (needs the graph path — write a file, drive run_file_*).
    let t = TmpDir::new();
    let path = t.write("main.chz", src);
    let (mn_out, _e, mn_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    mn_res.expect("regex Match stub program should run on the M:N engine");
    assert_eq!(
        mn_out, out,
        "M:N output diverged from VM/interp on regex Match stub"
    );
}

/// from-import `Response` (the `import Name from module` selective form).
#[test]
fn synthetic_struct_from_import_ctor_parity() {
    let out = parity_entry(
        "import Response from std.request\nr := Response(200, \"ok\", {\"k\": \"v\"})\nprint(str(r.status) + r.body + r.headers[\"k\"])\n",
    );
    assert_eq!(out, "200okv\n");
}

#[test]
fn synthetic_procresult_qualified_ctor_parity() {
    let out = parity_entry(
        "import std.process\np := process.ProcResult(\"out\", \"err\", 7)\nprint(p.stdout + p.stderr + str(p.code))\n",
    );
    assert_eq!(out, "outerr7\n");
}

/// Phase 4f — std.process + std.request are now FILE-BACKED (`std/process.chz`, `std/request.chz`),
/// their whole signature (fields-only `native struct` ProcResult/Response + the `native fn`s, incl.
/// request's OPTIONAL trailing `timeout_ms`) harvested via `harvest_native_module`, retiring both the
/// `native_module_sig` fn arms AND the `export_struct` arms. This must be a ZERO observable behavior
/// change on ALL THREE engines: constructing ProcResult/Response, reading every field, `from std.X
/// import Type`, qualified `X.Type(...)`, and request.get with AND without the optional `timeout_ms`
/// are byte-identical on interp, the cooperative VM, AND the M:N OS-thread engine. (Only ctors +
/// field reads are exercised at runtime — request.get would need a live server — but the optional
/// `timeout_ms` arg still TYPECHECKS both ways here, proving the harvested optional-tail sig.)
#[test]
fn process_request_file_backed_three_engine_parity() {
    let src = "import std.process\n\
                   import std.request\n\
                   import ProcResult from std.process\n\
                   import Response from std.request\n\
                   fn describe_proc(p: ProcResult) -> str:\n\
                   \x20   return p.stdout + \"|\" + p.stderr + \"|\" + str(p.code)\n\
                   fn describe_resp(r: Response) -> str:\n\
                   \x20   return str(r.status) + \"|\" + r.body + \"|\" + r.headers[\"k\"]\n\
                   pr: ProcResult = process.ProcResult(\"out\", \"err\", 7)\n\
                   rp: Response = request.Response(200, \"body\", {\"k\": \"v\"})\n\
                   print(describe_proc(pr))\n\
                   print(describe_resp(rp))\n";
    // VM(serial) + interp byte-identical.
    let out = parity_entry(src);
    assert_eq!(out, "out|err|7\n200|body|v\n");
    // …and the M:N OS-thread engine agrees.
    let t = TmpDir::new();
    let path = t.write("main.chz", src);
    let (mn_out, _e, mn_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    mn_res.expect("process/request ctor program should run on the M:N engine");
    assert_eq!(
        mn_out, out,
        "M:N output diverged from VM/interp on process/request ctors"
    );
}

/// Phase 4f — the pure TYPE `import ProcResult from std.process` (no runtime module-member value)
/// must NOT fault on EITHER engine (the both-engine `bind_import` skip). A single-engine fault = red.
#[test]
fn pure_type_import_no_fault_both_engines() {
    let out = parity_entry("import ProcResult from std.process\nprint(1)\n");
    assert_eq!(out, "1\n");
    let out2 = parity_entry("import Response from std.request\nprint(2)\n");
    assert_eq!(out2, "2\n");
}

/// A user `struct Response` WITHOUT `import std.request` is the user's OWN type on both engines —
/// the synthetic name is freed (the `Builtin`-origin seed is shadowed by the user declaration).
#[test]
fn user_struct_shadows_synthetic_name_parity() {
    assert_eq!(
        parity_entry("struct Response:\n    code: int\nr := Response(7)\nprint(str(r.code))\n"),
        "7\n"
    );
    assert_eq!(
        parity_entry("struct Match:\n    score: int\nm := Match(3)\nprint(str(m.score))\n"),
        "3\n"
    );
}

/// `import geo` then `geo.Color.Red` (nullary) and `geo.Shape.Circle(5)` (payload) construct.
#[test]
fn qualified_enum_ctor_parity() {
    let out = assert_parity_file(
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
    let out = assert_parity_file(
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
/// (`Point(...)`), the other is disambiguated. Byte-identical on both engines.
#[test]
fn real_collision_two_modules_same_struct() {
    let out = assert_parity_file(
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
/// the match-pattern side must use the SAME module-scoped runtime key as construction. Identical
/// on both engines.
#[test]
fn enum_collision_match_in_declaring_module() {
    let out = assert_parity_file(
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
    let out = parity_entry(
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
    let out = assert_parity_file(
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
/// byte-identical on both engines (interp was already correct; this guards VM/serial).
#[test]
fn enum_collision_construct_in_other_declaring_module() {
    let out = assert_parity_file(
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
    let out = assert_parity_file(
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

/// A spawned task constructs an imported struct AND a value crosses a Channel (cross-airlock,
/// data-not-time): identical on interp, default VM, and `--parallel`.
#[test]
fn imported_struct_across_airlock_three_engine() {
    let files = [
        ("geo.chz", "struct Point:\n    x: int\n    y: int\n"),
        (
            "main.chz",
            "import geo\nch := Channel[geo.Point]()\nparallel:\n    spawn:\n        ch.send(geo.Point(3, 4))\np := ch.recv()\nprint(p.x + p.y)\n",
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
    let (io, _, ir, _) = run_file_p(&entry);
    let (vo, _, vr, _) = run_file(&entry);
    let (po, _, pr, _) = run_file_parallel(&entry, crate::native::HostConfig::default());
    assert!(
        ir.is_ok() && vr.is_ok() && pr.is_ok(),
        "a run faulted: i={ir:?} v={vr:?} p={pr:?}"
    );
    assert_eq!(io, "7\n");
    assert_eq!(io, vo, "interp vs vm");
    assert_eq!(io, po, "interp vs --parallel");
}

// ----- ROOT REDESIGN: always-qualified identity key + bare display name -----

/// THE BUG (collision-loser decode against the WRONG layout): the entry module declares its OWN
/// `Point{a,b}` while a dep also declares `Point{x}`. A bare `json.decode[Point]` must decode
/// against the ENTRY's layout (the bare name resolves to the entry's `Point`), printing 5 — not
/// fail `decode: missing key 'x'` against the dep's layout. Byte-identical on both engines.
#[test]
fn decode_collision_loser_against_correct_layout() {
    let out = assert_parity_file(
        &[
            ("dep.chz", "struct Point:\n    x: int\n"),
            (
                "main.chz",
                "import std.json\nimport dep\nstruct Point:\n    a: int\n    b: int\np := json.decode[Point](\"{{\\\"a\\\":5,\\\"b\\\":9}}\")\nmatch p:\n    Ok(v): print(v.a)\n    Err(e): print(e)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "5\n");
}

/// Twin: a QUALIFIED `json.decode[dep.Point]` decodes against the dep's layout (`x`), printing 7.
#[test]
fn decode_qualified_target() {
    let out = assert_parity_file(
        &[
            ("dep.chz", "struct Point:\n    x: int\n"),
            (
                "main.chz",
                "import std.json\nimport dep\nstruct Point:\n    a: int\n    b: int\np := json.decode[dep.Point](\"{{\\\"x\\\":7}}\")\nmatch p:\n    Ok(v): print(v.x)\n    Err(e): print(e)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "7\n");
}

/// BARE DISPLAY ON COLLISION: two modules both declare `struct Point` with different layouts;
/// printing each value shows the BARE name (`Point(x=1)` / `Point(y=2)`) on BOTH — never the
/// module-qualified identity key. Byte-identical on both engines.
#[test]
fn collision_prints_bare_for_both() {
    let out = assert_parity_file(
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
/// DEFINING scope (`dep::Inner` with `k`), not the entry's. Prints 3. Byte-identical both engines.
#[test]
fn decode_nested_struct_field_in_defining_module() {
    let out = assert_parity_file(
        &[
            (
                "dep.chz",
                "struct Inner:\n    k: int\nstruct Wrap:\n    inner: Inner\n",
            ),
            (
                "main.chz",
                "import std.json\nimport dep\nstruct Inner:\n    other: int\np := json.decode[dep.Wrap](\"{{\\\"inner\\\":{{\\\"k\\\":3}}}}\")\nmatch p:\n    Ok(w): print(w.inner.k)\n    Err(e): print(e)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "3\n");
}

/// AIRLOCK 3-ENGINE PARITY: the collision-decode repro must produce identical output under interp,
/// the default VM, and the `--parallel` OS-thread engine.
#[test]
fn decode_collision_three_engine() {
    let files = [
        ("dep.chz", "struct Point:\n    x: int\n"),
        (
            "main.chz",
            "import std.json\nimport dep\nstruct Point:\n    a: int\n    b: int\nmatch json.decode[Point](\"{{\\\"a\\\":5,\\\"b\\\":9}}\"):\n    Ok(v): print(v.a)\n    Err(e): print(e)\n",
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
    let (io, _, ir, _) = run_file_p(&entry);
    let (vo, _, vr, _) = run_file(&entry);
    let (po, _, pr, _) = run_file_parallel(&entry, crate::native::HostConfig::default());
    assert!(
        ir.is_ok() && vr.is_ok() && pr.is_ok(),
        "a run faulted: i={ir:?} v={vr:?} p={pr:?}"
    );
    assert_eq!(io, "5\n");
    assert_eq!(io, vo, "interp vs vm");
    assert_eq!(io, po, "interp vs --parallel");
}

/// BLOCKER 2/3 — a bare match-pattern enum qualifier (`Color.Red`) whose enum is reachable only
/// via WHOLE-MODULE import must be resolved from the SCRUTINEE'S static enum, not re-guessed by
/// iterating the (RandomState-seeded) import map. Two whole-imported same-named enums; scrutinee
/// from `a`; the `Color.Red`/`Color.Blue` arms must resolve against `a::Color`. Looped 50x to
/// defeat the nondeterminism (one pass could luck-pass) and asserted identical across all three
/// engines (interp / default VM / `--parallel`).
#[test]
fn match_arm_scrutinee_driven_three_engine() {
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
        let (io, _, ir, _) = run_file_p(&entry);
        let (vo, _, vr, _) = run_file(&entry);
        let (po, _, pr, _) = run_file_parallel(&entry, crate::native::HostConfig::default());
        assert!(
            ir.is_ok() && vr.is_ok() && pr.is_ok(),
            "a run faulted: i={ir:?} v={vr:?} p={pr:?}"
        );
        assert_eq!(io, "red\n", "interp output wrong");
        assert_eq!(io, vo, "interp vs vm");
        assert_eq!(io, po, "interp vs --parallel");
    }
}

/// BLOCKER 2/3 divergence case — `import b` FIRST, then `import a`; scrutinee is `a.Color.Blue`,
/// a variant that exists ONLY in `a`. A scrutinee-blind import-iterating resolver picked `b`
/// (which has no `Blue`) and crashed (`--serial`/`--parallel`) or matched the wrong arm (VM). The
/// match-arm key MUST come from the scrutinee's `a::Color`. Looped 50x across all three engines.
#[test]
fn match_arm_only_in_a_variant_three_engine() {
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
        let (io, _, ir, _) = run_file_p(&entry);
        let (vo, _, vr, _) = run_file(&entry);
        let (po, _, pr, _) = run_file_parallel(&entry, crate::native::HostConfig::default());
        assert!(
            ir.is_ok() && vr.is_ok() && pr.is_ok(),
            "a run faulted: i={ir:?} v={vr:?} p={pr:?}"
        );
        assert_eq!(io, "blue\n", "interp output wrong");
        assert_eq!(io, vo, "interp vs vm");
        assert_eq!(io, po, "interp vs --parallel");
    }
}

/// AIRLOCK: a colliding struct value sent through a Channel survives the wire round-trip and
/// prints its BARE name on the other side. Identical on interp, default VM, and `--parallel`.
#[test]
fn collision_struct_sent_across_task() {
    let files = [
        ("a.chz", "struct Point:\n    x: int\n"),
        ("b.chz", "struct Point:\n    y: int\n"),
        (
            "main.chz",
            "import a\nimport b\nch := Channel[b.Point]()\nparallel:\n    spawn:\n        ch.send(b.Point(9))\np := ch.recv()\nprint(p)\nprint(p.y)\n",
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
    let (io, _, ir, _) = run_file_p(&entry);
    let (vo, _, vr, _) = run_file(&entry);
    let (po, _, pr, _) = run_file_parallel(&entry, crate::native::HostConfig::default());
    assert!(
        ir.is_ok() && vr.is_ok() && pr.is_ok(),
        "a run faulted: i={ir:?} v={vr:?} p={pr:?}"
    );
    assert_eq!(io, "Point(y=9)\n9\n");
    assert_eq!(io, vo, "interp vs vm");
    assert_eq!(io, po, "interp vs --parallel");
}

// ----- C5 (A2): program-exit auto-drain is skipped on a hard `os.exit` -----

#[test]
fn executor_autodrain_skipped_on_os_exit() {
    // `os.exit` is a hard halt — like `defer`, the program-exit auto-drain is skipped, so a
    // submitted-but-un-shut executor's work must NOT run. Driven through the file path on both
    // engines (parity), since it imports std.os.
    let out = parity_entry(
        "import std.os\nfn j():\n    print(\"RAN\")\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j())\n    print(\"before exit\")\n    os.exit(0)\nmain()\n",
    );
    assert_eq!(out, "before exit\n");
}

// ----- M9: std.regex parity (exercises NativeRet::Struct lowering on both engines) -----

#[test]
fn regex_find_all_replace_split_parity() {
    let out = parity_entry(
        r##"import std.regex
match regex.find_all("[0-9]+", "a1 22 333"):
    Ok(ms):
        for m in ms:
            print(m.text)
    Err(e): print(e)
match regex.replace_all("[0-9]+", "a1b22c", "#"):
    Ok(s): print(s)
    Err(e): print(e)
match regex.split(",", "a,b,c"):
    Ok(parts): print("|".join(parts))
    Err(e): print(e)
"##,
    );
    assert_eq!(out, "1\n22\n333\na#b#c\na|b|c\n");
}

/// `std.request` against a loopback server, run through BOTH engines (exercises `NativeRet::Map`
/// lowering on each). The server serves one canned response per connection; interp and vm each
/// open one, so it accepts twice.
#[test]
fn request_get_parity_against_local_server() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
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
        }
    });

    let src = format!(
        "import std.request\nmatch request.get(\"http://{addr}/\"):\n    Ok(resp):\n        print(str(resp.status))\n        print(resp.body)\n        print(resp.headers[\"x-test\"])\n    Err(e): print(e)\n"
    );
    let out = parity_entry(&src);
    server.join().unwrap();
    assert_eq!(out, "200\npong\nhi\n");
}

/// `std.request` new verbs + custom headers, run through BOTH engines against a loopback server
/// that records every request's wire bytes. Each engine issues a `put` and a header-carrying
/// `request("DELETE", …)`, so the server accepts 4 times (2 per engine). Asserts (a) identical
/// stdout across VM and interp and (b) the right method line + custom header reached the wire —
/// locking the off-heap `NativeArg::Map` headers path and the verb wrappers under parity.
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
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let n = stream.read(&mut buf).unwrap_or(0);
            seen_srv
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&buf[..n]).into_owned());
            // `Connection: close` so ureq's thread-local pool (shared across both engine runs on
            // this test thread) never reuses a server-closed socket — one fresh conn per request.
            let resp = "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nok";
            stream.write_all(resp.as_bytes()).unwrap();
        }
    });

    // Each engine: a PUT verb wrapper, then a header-carrying general DELETE.
    let src = format!(
        "import std.request\nmatch request.put(\"http://{addr}/\", \"payload\"):\n    Ok(r): print(str(r.status))\n    Err(e): print(e)\nmatch request.request(\"DELETE\", \"http://{addr}/\", \"\", {{\"X-Custom\": \"value\"}}):\n    Ok(r): print(str(r.status))\n    Err(e): print(e)\n"
    );
    let out = parity_entry(&src);
    server.join().unwrap();
    assert_eq!(
        out, "200\n200\n",
        "VM/interp must agree and both requests succeed"
    );

    let reqs = seen.lock().unwrap();
    assert_eq!(reqs.len(), 4, "two requests per engine");
    let puts = reqs.iter().filter(|r| r.starts_with("PUT ")).count();
    let deletes = reqs.iter().filter(|r| r.starts_with("DELETE ")).count();
    let with_header = reqs
        .iter()
        .filter(|r| r.contains("X-Custom: value"))
        .count();
    assert_eq!(puts, 2, "both engines must send PUT");
    assert_eq!(deletes, 2, "both engines must send DELETE");
    assert_eq!(
        with_header, 2,
        "the custom header must reach the wire on both engines"
    );
}

#[test]
fn regex_find_groups_and_span_parity() {
    let out = parity_entry(
        r#"import std.regex
match regex.find("([a-z]+)@([a-z]+)", "xx ann@host"):
    Ok(opt):
        match opt:
            Some(m): print(m.text + " " + str(m.start) + " " + ",".join(m.groups))
            None: print("none")
    Err(e): print(e)
"#,
    );
    assert_eq!(out, "ann@host 3 ann,host\n");
}

// ----- break / continue parity (both engines must agree AND produce the right output) -----

/// Assert both engines agree AND that the (shared) stdout equals `expect`. A hang here means a
/// `continue` is landing on the wrong target (re-test without advancing → infinite loop).
fn assert_parity_out(src: &str, expect: &str) {
    assert_parity(src);
    assert_eq!(
        vm_outcome(src).expect("program should run"),
        expect,
        "for:\n{src}"
    );
}

#[test]
fn bitwise_ops_parity() {
    assert_parity_out(
        "print(5 & 3)\nprint(5 | 2)\nprint(5 ^ 3)\nprint(1 << 4)\nprint(255 >> 4)\n",
        "1\n7\n6\n16\n15\n",
    );
}

#[test]
fn bitwise_precedence_below_comparison_parity() {
    // `5 & 3 == 1` is `(5 & 3) == 1` (bitwise binds tighter than `==`, Python-style).
    assert_parity_out("print(5 & 3 == 1)\n", "true\n");
}

#[test]
fn xor_fold_single_number_parity() {
    assert_parity_out(
        "xs := [4,1,2,1,4,2,7]\nacc := 0\nfor x in xs:\n    acc = acc ^ x\nprint(acc)\n",
        "7\n",
    );
}

#[test]
fn shift_out_of_range_error_parity() {
    // Dynamic shift the checker can't catch — both engines must raise the same runtime error.
    assert_parity("print(1 << 64)\n");
    assert_parity("print(1 << -1)\n");
}

#[test]
fn match_tuple_pattern_parity() {
    assert_parity_out(
        "p := (3, 4)\nmatch p:\n    (0, y): print(y)\n    (x, y): print(x + y)\n",
        "7\n",
    );
}

#[test]
fn match_tuple_literal_arm_parity() {
    assert_parity_out(
        "p := (1, 9)\nlabel := match p:\n    (1, n): \"one {n}\"\n    _: \"other\"\nprint(label)\n",
        "one 9\n",
    );
}

#[test]
fn match_nested_variant_in_tuple_parity() {
    assert_parity_out(
        "o: (int, int)? = Some((10, 20))\nmatch o:\n    None: print(\"none\")\n    Some((a, b)): print(a + b)\n",
        "30\n",
    );
}

#[test]
fn match_nested_heap_payload_gc_stress() {
    // Nested pattern binding heap values (strings) inside a tuple inside a variant; a GC mid-bind
    // must not collect the still-referenced payload.
    let src = "o: (str, str)? = Some((\"a\" + \"b\", \"c\" + \"d\"))\nmatch o:\n    None: print(\"none\")\n    Some((x, y)): print(x + y)\n";
    assert_parity(src);
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
    assert_parity_out(
        "n := 5\nmatch n:\n    x if x < 0: print(\"neg\")\n    x if x > 10: print(\"big\")\n    _: print(\"mid\")\n",
        "mid\n",
    );
}

#[test]
fn match_guard_first_true_parity() {
    assert_parity_out(
        "n := -2\nlabel := match n:\n    x if x < 0: \"neg\"\n    _: \"nonneg\"\nprint(label)\n",
        "neg\n",
    );
}

#[test]
fn match_range_boundaries_parity() {
    // Half-open: start is inclusive, end is exclusive.
    assert_parity_out(
        "fn b(n: int) -> str:\n    return match n:\n        0..10: \"lo\"\n        10..20: \"hi\"\n        _: \"out\"\nprint(b(0))\nprint(b(9))\nprint(b(10))\nprint(b(19))\nprint(b(20))\n",
        "lo\nlo\nhi\nhi\nout\n",
    );
}

#[test]
fn match_range_with_literal_mix_parity() {
    // A match mixing int literals and ranges still routes through the literal path.
    assert_parity_out(
        "fn f(n: int) -> str:\n    return match n:\n        0: \"zero\"\n        1..100: \"small\"\n        _: \"big\"\nprint(f(0))\nprint(f(50))\nprint(f(500))\n",
        "zero\nsmall\nbig\n",
    );
}

#[test]
fn for_over_map_keys_parity() {
    assert_parity_out(
        "m := {\"a\": 1, \"b\": 2, \"c\": 3}\nfor k in m:\n    print(k)\n",
        "a\nb\nc\n",
    );
}

/// Regression (review #1): a struct field named `decode` must still be indexable — the
/// `.decode[…]` JSON form is only stolen when a real `[Type](arg)` follows.
#[test]
fn field_named_decode_is_indexable_parity() {
    let out = parity_entry(
        "struct Box:\n    decode: List[int]\nb := Box([10, 20, 30])\nprint(b.decode[1])\nprint(b.decode[0] + b.decode[2])\n",
    );
    assert_eq!(out, "20\n40\n");
}

/// Regression (review #2): malformed and out-of-range numbers come back as `Err` / stringify
/// cleanly — they must never abort the host (no uncaught `float()`/`int()` panic).
#[test]
fn json_malformed_numbers_are_errors_parity() {
    let out = parity_entry(
        "import std.json\nfn tp(s: str) -> str:\n    match json.parse(s):\n        Ok(j): return \"OK \" + json.stringify(j)\n        Err(e): return \"ERR\"\nprint(tp(\"1e\"))\nprint(tp(\"1.\"))\nprint(tp(\"100000000000000000000\"))\n",
    );
    assert_eq!(out, "ERR\nERR\nOK 100000000000000000000.0\n");
}

#[test]
fn json_decode_struct_parity() {
    let out = parity_entry(
        "import std.json\nstruct P:\n    x: int\n    y: int\nmatch json.decode[P](\"{{\\\"x\\\":1,\\\"y\\\":2}}\"):\n    Ok(p): print(p.x + p.y)\n    Err(e): print(e)\n",
    );
    assert_eq!(out, "3\n");
}

#[test]
fn json_decode_error_parity() {
    let out = parity_entry(
        "import std.json\nstruct P:\n    x: int\nmatch json.decode[P](\"{{\\\"y\\\":2}}\"):\n    Ok(p): print(p.x)\n    Err(e): print(e)\n",
    );
    assert_eq!(out, "decode: missing key 'x' at $\n");
}

#[test]
fn process_cmd_ok_and_err_parity() {
    let out = parity_entry(
        "import std.process\nmatch process.cmd(\"printf abc\"):\n    Ok(s): print(\"ok:\" + s)\n    Err(e): print(\"err:\" + e)\nmatch process.cmd(\"exit 2\"):\n    Ok(s): print(\"ok\")\n    Err(e): print(\"err:\" + e)\n",
    );
    assert_eq!(out, "ok:abc\nerr:command exited with status 2\n");
}

#[test]
fn fs_predicates_parity() {
    let out = parity_entry(
        "import std.fs\nprint(fs.exists(\"Cargo.toml\"))\nprint(fs.exists(\"definitely_not_here.zzz\"))\nprint(fs.is_dir(\"src\"))\n",
    );
    assert_eq!(out, "true\nfalse\ntrue\n");
}

#[test]
fn time_format_parity() {
    let out = parity_entry(
        "import std.time\nprint(time.format(0))\nprint(time.format(1700000000))\nprint(time.now() > 0)\n",
    );
    assert_eq!(out, "1970-01-01 00:00:00\n2023-11-14 22:13:20\ntrue\n");
}

#[test]
fn set_dedup_and_algebra_parity() {
    assert_parity_out(
        "s := {3, 1, 3, 2, 1}\nprint(s.len())\nprint({1,2,3}.union({3,4}).len())\nprint({1,2,3}.intersection({2,3,4}).len())\nprint({1,2,3}.difference({2,3}).len())\nprint({1,2} == {2,1})\n",
        "3\n4\n2\n1\ntrue\n",
    );
}

#[test]
fn set_mutation_and_iteration_parity() {
    assert_parity_out(
        "s := Set()\ns.add(10)\ns.add(10)\ns.add(20)\nprint(s.len())\nprint(s.remove(10))\nprint(s.remove(10))\ntotal := 0\nfor x in {5, 15, 25}:\n    total += x\nprint(total)\n",
        "2\ntrue\nfalse\n45\n",
    );
}

#[test]
fn set_display_parity() {
    assert_parity_out("print({1, 2, 3})\nprint(Set())\n", "{1, 2, 3}\nSet()\n");
}

#[test]
fn str_chars_parity() {
    assert_parity_out(
        "cs := \"héllo\".chars()\nprint(cs.len())\nprint(cs[1])\n",
        "5\né\n",
    );
}

#[test]
fn for_over_str_parity() {
    assert_parity_out(
        "out := \"\"\nfor c in \"abc\":\n    out = out + c + \"-\"\nprint(out)\n",
        "a-b-c-\n",
    );
}

#[test]
fn for_over_empty_str_parity() {
    assert_parity_out("n := 0\nfor c in \"\":\n    n += 1\nprint(n)\n", "0\n");
}

#[test]
fn for_over_map_key_value_parity() {
    assert_parity_out(
        "m := {\"a\": 1, \"b\": 2}\ns := 0\nfor k, v in m:\n    print(\"{k}={v}\")\n    s += v\nprint(s)\n",
        "a=1\nb=2\n3\n",
    );
}

#[test]
fn for_over_map_kv_mutation_during_iteration_parity() {
    // The body reassigns a not-yet-visited key; both engines must agree (snapshot semantics:
    // the value bound is the one captured at loop start, like list iteration).
    assert_parity_out(
        "m := {\"a\": 1, \"b\": 2, \"c\": 3}\nout := 0\nfor k, v in m:\n    m[\"c\"] = 99\n    out += v\nprint(out)\n",
        "6\n",
    );
}

#[test]
fn for_over_map_kv_remove_during_iteration_parity() {
    // Removing a future key mid-iteration must not crash one engine while the other succeeds.
    assert_parity_out(
        "m := {\"a\": 1, \"b\": 2}\nfirst := true\nsum := 0\nfor k, v in m:\n    if first:\n        m.remove(\"b\")\n        first = false\n    sum += v\nprint(sum)\n",
        "3\n",
    );
}

#[test]
fn for_over_map_break_continue_parity() {
    // break/continue still target the index increment over the keys sequence.
    assert_parity_out(
        "m := {\"a\": 1, \"b\": 2, \"c\": 3, \"d\": 4}\nfor k, v in m:\n    if v == 2: continue\n    if v == 4: break\n    print(k)\n",
        "a\nc\n",
    );
}

#[test]
fn cmp_max_int_parity() {
    // Generic min/max now live in std.cmp; abs stays in std.math. File/graph path required.
    let out = parity_entry(
        "import std.cmp\nimport std.math\nfn main():\n    print(cmp.max(3, 5))\n    print(cmp.min(3, 5))\n    print(math.abs(-5))\nmain()\n",
    );
    assert_eq!(out, "5\n3\n5\n");
}

#[test]
fn cmp_max_float_parity() {
    let out = parity_entry(
        "import std.cmp\nimport std.math\nfn main():\n    print(cmp.max(3.0, 5.0))\n    print(math.abs(-2.5))\nmain()\n",
    );
    assert_eq!(out, "5.0\n2.5\n");
}

#[test]
fn cmp_max_struct_parity() {
    // The generic max over a Comparable struct must be byte-identical on both engines.
    let src = "import std.cmp\nstruct P:\n    n: int\n    fn compare(self, o: P) -> int:\n        return self.n - o.n\nfn main():\n    print(cmp.max(P(2), P(9)).n)\n    print(cmp.min(P(2), P(9)).n)\nmain()\n";
    assert_eq!(parity_entry(src), "9\n2\n");
}

#[test]
fn ord_chr_parity() {
    assert_parity_out("print(ord(\"A\"))\nprint(chr(97))\n", "65\na\n");
}

#[test]
fn ord_chr_roundtrip_parity() {
    assert_parity_out("print(chr(ord(\"z\")))\n", "z\n");
}

#[test]
fn ord_index_digit_value_parity() {
    // The digit-value idiom over an indexed char.
    assert_parity_out("s := \"7\"\nprint(ord(s[0]) - ord(\"0\"))\n", "7\n");
}

#[test]
fn ord_empty_string_error_parity() {
    // Runtime error (checker can't catch it) — message must match across engines.
    assert_parity("print(ord(\"\"))\n");
}

#[test]
fn chr_invalid_codepoint_error_parity() {
    assert_parity("print(chr(-1))\n");
    assert_parity("print(chr(2000000))\n");
}

#[test]
fn sort_by_descending_parity() {
    assert_parity_out(
        "xs := [3,1,2]\nxs.sort_by(fn(a: int, b: int) -> int: b - a)\nprint(xs)\n",
        "[3, 2, 1]\n",
    );
}

#[test]
fn sort_by_stable_by_key_parity() {
    // Equal keys (string length) must keep input order — stability is part of the contract.
    assert_parity_out(
        "ws := [\"bb\", \"a\", \"dd\", \"e\"]\nws.sort_by(fn(a: str, b: str) -> int: a.len() - b.len())\nprint(ws)\n",
        "[a, e, bb, dd]\n",
    );
}

#[test]
fn sort_by_comparator_mutates_list_parity() {
    // A comparator that mutates an element being sorted must behave identically on both engines.
    // Both sort a snapshot taken at call time and overwrite the list with the sorted result, so
    // the in-comparator `xs[0] = 100` is discarded.
    let src = "xs := [3, 1, 2]\nfn cmp(a: int, b: int) -> int:\n    xs[0] = 100\n    return a - b\nxs.sort_by(cmp)\nprint(xs)\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "[1, 2, 3]\n");
}

#[test]
fn sort_by_empty_and_singleton_parity() {
    assert_parity_out(
        "xs := [42]\nxs.sort_by(fn(a: int, b: int) -> int: a - b)\nprint(xs)\n",
        "[42]\n",
    );
}

#[test]
fn break_early_for_parity() {
    assert_parity_out(
        "s := 0\nfor i in 0..10:\n    if i == 5: break\n    s += i\nprint(s)\n",
        "10\n",
    );
}

#[test]
fn continue_for_terminates_parity() {
    // THE increment-landing guard: `continue` must reach the loop's `i += 1`, never the
    // condition (would re-test the same `i` forever). If this hangs, the target is wrong.
    assert_parity_out(
        "for i in 0..5:\n    if i == 1: continue\n    if i == 3: continue\n    print(i)\n",
        "0\n2\n4\n",
    );
}

#[test]
fn while_break_parity() {
    assert_parity_out(
        "i := 0\nwhile true:\n    if i == 3: break\n    i += 1\nprint(i)\n",
        "3\n",
    );
}

#[test]
fn while_continue_progresses_parity() {
    // The counter advances BEFORE the `continue`, so the `while` still terminates.
    assert_parity_out(
        "i := 0\ns := 0\nwhile i < 5:\n    i += 1\n    if i == 2: continue\n    s += i\nprint(s)\n",
        "13\n",
    );
}

#[test]
fn break_in_if_in_loop_parity() {
    assert_parity_out(
        "for i in 0..10:\n    if i > 2:\n        break\n    print(i)\n",
        "0\n1\n2\n",
    );
}

#[test]
fn return_from_loop_parity() {
    // `return` inside a loop still returns the whole function (break/continue don't intercept it).
    assert_parity_out(
        "fn f():\n    for i in 0..10:\n        if i == 2: return i\n    return -1\nprint(f())\n",
        "2\n",
    );
}

#[test]
fn nested_loop_inner_break_parity() {
    // Inner `break` does not break the outer loop: the outer runs all 3 iterations.
    assert_parity_out(
        "n := 0\nfor i in 0..3:\n    for j in 0..3:\n        break\n    n += 1\nprint(n)\n",
        "3\n",
    );
}

#[test]
fn continue_list_for_parity() {
    // `continue` over a LIST for-loop (not just range) advances to the next element.
    assert_parity_out(
        "for x in [1,2,3,4]:\n    if x % 2 == 0: continue\n    print(x)\n",
        "1\n3\n",
    );
}

#[test]
fn break_list_for_parity() {
    assert_parity_out(
        "for x in [10,20,30,40]:\n    if x == 30: break\n    print(x)\n",
        "10\n20\n",
    );
}

// ----- literal + wildcard match parity -----

#[test]
fn match_int_literals_stmt_parity() {
    assert_parity(
        "n := 2\nmatch n:\n    0: print(\"zero\")\n    1: print(\"one\")\n    _: print(\"many\")\n",
    );
}

#[test]
fn match_str_literals_expr_parity() {
    assert_parity("c := \"x\"\ns := match c:\n    \"a\": \"first\"\n    _: \"other\"\nprint(s)\n");
}

#[test]
fn match_bool_literals_parity() {
    assert_parity(
        "b := false\nmatch b:\n    true: print(\"yes\")\n    false: print(\"no\")\n    _: print(\"?\")\n",
    );
}

#[test]
fn match_literal_matched_arm_parity() {
    // The matching literal arm fires (wildcard not reached).
    assert_parity("n := 1\ns := match n:\n    0: \"a\"\n    1: \"b\"\n    _: \"z\"\nprint(s)\n");
}

#[test]
fn match_wildcard_reached_parity() {
    // No literal matches → the `_` arm fires.
    assert_parity("n := 9\ns := match n:\n    0: \"a\"\n    1: \"b\"\n    _: \"z\"\nprint(s)\n");
}

#[test]
fn match_variant_regression_parity() {
    // A variant match still lowers via the variant path unchanged.
    assert_parity(
        "o := Some(5)\nmatch o:\n    Some(v): print(\"got {v}\")\n    None: print(\"none\")\n",
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
        parity_entry(src),
        "2.0\n3.0\n4.0\n1024.0\n3.5\n3.0\n3.141592653589793\n"
    );
}

#[test]
fn parity_std_math_sqrt_negative_is_nan() {
    // math.sqrt of a negative is IEEE NaN — never faults, identical on both engines.
    let src = "import std.math\nfn main():\n    print(math.sqrt(0.0 - 1.0))\nmain()";
    assert_eq!(parity_entry(src), "NaN\n");
}

#[test]
fn parity_float_ieee_div_mod() {
    // Float division/modulo by zero is total IEEE-754 on both engines: inf / NaN, never a fault.
    let src = "fn main():\n    print(1.0 / 0.0)\n    print(-1.0 / 0.0)\n    print(0.0 / 0.0)\n    print(5.0 % 0.0)\nmain()";
    assert_eq!(parity_entry(src), "inf\n-inf\nNaN\nNaN\n");
}

#[test]
fn parity_int_div_by_zero_still_faults() {
    // INTEGER division by zero still faults — caught + printed identically on both engines.
    let src = "fn run() -> int!:\n    r := recover:\n        1 / 0\n    match r:\n        Ok(v): return Ok(v)\n        Err(e): print(e.message())\n    return Ok(0)\nfn main():\n    _ := run()\nmain()";
    assert_eq!(parity_entry(src), "division by zero\n");
}

#[test]
fn parity_std_math_predicates() {
    // is_nan / is_inf / is_finite — float predicates returning bool, identical on both engines.
    let src = "import std.math\nfn main():\n    print(math.is_nan(0.0 / 0.0))\n    print(math.is_inf(1.0 / 0.0))\n    print(math.is_finite(1.0))\n    print(math.is_finite(1.0 / 0.0))\nmain()";
    assert_eq!(parity_entry(src), "true\ntrue\ntrue\nfalse\n");
}

#[test]
fn parity_nan_ordered_compare_is_false() {
    // Ordered comparisons (`< <= > >=`) involving NaN are ALWAYS false on both engines — never a
    // fault — matching IEEE-754 / Python / Rust. Equality is untouched (`nan == nan` → false,
    // `nan != nan` → true). Normal float compares still work (regression guard).
    let src = "fn main():\n    nan := 0.0 / 0.0\n    print(nan < 1.0)\n    print(nan <= 1.0)\n    print(nan > 1.0)\n    print(nan >= 1.0)\n    print(1.0 < nan)\n    print(1.0 > nan)\n    print((1.0 / 0.0) < nan)\n    print(1.0 < 2.0)\n    print(2.0 > 1.0)\n    print(nan == nan)\n    print(nan != nan)\nmain()";
    assert_parity_out(
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
    // would be a non-portable test. `assert_parity` proves the real guarantees portably: the sort
    // never faults and VM↔interp agree byte-identically on whatever order this machine produces.
    let src = "fn main():\n    xs := [1.0, 0.0 / 0.0, 2.0, 0.0 / 0.0, 0.5]\n    xs.sort_by_key(fn(x: float) -> float: x)\n    for v in xs:\n        print(v)\nmain()";
    assert_parity(src);
}

#[test]
fn parity_sort_by_key_signed_zero_matches_sort() {
    // `sort_by_key` over a float key uses `total_cmp` for the WHOLE comparison, exactly like
    // `sort()` — so they agree even on `-0.0`/`+0.0`, which `partial_cmp` ranks Equal but
    // `total_cmp` orders `-0.0 < +0.0`. Signed-zero order is invisible to `==` (`-0.0 == +0.0`),
    // so observe it via `1.0/x` → `-inf` for `-0.0`, `+inf` for `+0.0`. Both sort paths must put
    // `-0.0` first ⇒ `-inf` then `inf`. Platform-independent (no NaN sign involved).
    let by_sort = "fn main():\n    xs := [0.0, -1.0 * 0.0]\n    xs.sort()\n    for v in xs:\n        print(1.0 / v)\nmain()";
    assert_parity_out(by_sort, "-inf\ninf\n");
    let by_key = "fn main():\n    xs := [0.0, -1.0 * 0.0]\n    xs.sort_by_key(fn(x: float) -> float: x)\n    for v in xs:\n        print(1.0 / v)\nmain()";
    assert_parity_out(by_key, "-inf\ninf\n");
}

#[test]
fn parity_sort_by_key_normal_key_unchanged() {
    // Behavior-preserving guard: non-NaN float keys and int keys sort exactly as before — Part B
    // touches only the NaN float-key path.
    let fsrc = "fn main():\n    xs := [3.0, 1.0, 2.0]\n    xs.sort_by_key(fn(x: float) -> float: x)\n    for v in xs:\n        print(v)\nmain()";
    assert_parity_out(fsrc, "1.0\n2.0\n3.0\n");
    let isrc = "fn main():\n    xs := [3, 1, 2]\n    xs.sort_by_key(fn(x: int) -> int: x)\n    for v in xs:\n        print(v)\nmain()";
    assert_parity_out(isrc, "1\n2\n3\n");
}

#[test]
fn math_abs_min_overflows() {
    // `math.abs(i64::MIN)` has no representable result. Raw `i64::abs()` would panic (debug) or
    // wrap (release); the native fn must surface a recoverable overflow like every other op,
    // identically on both engines. i64::MIN is built as `-MAX - 1` (the literal
    // `9223372036854775808` overflows the lexer).
    let src = "import std.math\nfn main():\n    x := -9223372036854775807 - 1\n    print(math.abs(x))\nmain()";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let ie = run_file_p(&entry).2.unwrap_err().to_string();
    let ve = run_file(&entry).2.unwrap_err().to_string();
    assert_eq!(
        ie, ve,
        "abs-overflow error must be identical on both engines"
    );
    assert!(ie.contains("integer overflow in abs"), "{ie}");
}

#[test]
fn math_abs_min_overflow_is_recoverable() {
    // The overflow is a normal recoverable fault: `recover:` turns it into an Err, not a crash.
    let src = "import std.math\nfn main():\n    x := -9223372036854775807 - 1\n    r := recover:\n        math.abs(x)\n    match r:\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()";
    let out = parity_entry(src);
    assert!(out.contains("integer overflow in abs"), "{out}");
}

#[test]
fn exit_threads_code_through_both_engines() {
    // `std.os.exit(code)` halts the program with that exit code on both engines: output before
    // the call is preserved, the statement after it never runs, and the run is not an error.
    let src = "import std.os\nfn main():\n    print(\"before\")\n    os.exit(3)\n    print(\"after\")\nmain()";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (io, _ie, ir, ic) = run_file_p(&entry);
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
    let (io, _ie, ir, ic) = run_file_p(&entry);
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
fn defer_top_level_runs_on_unhandled_error() {
    // An unhandled top-level `?` error still unwinds through the module body's defers (cleanup
    // runs before the program reports the error).
    let src = "fn log(s: str):\n    print(s)\nfn boom() -> int!:\n    return Err(\"nope\")\ndefer log(\"cleanup\")\nprint(\"before\")\nx := boom()?\nprint(\"after\")\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    let (vo, _ve, vr, _vc) = run_file(&entry);
    assert_eq!(
        io, "before\ncleanup\n",
        "interp: top-level defer runs on unhandled error"
    );
    assert_eq!(
        vo, "before\ncleanup\n",
        "vm: top-level defer runs on unhandled error"
    );
    assert!(
        ir.is_err() && vr.is_err(),
        "unhandled `?` is an error: interp={ir:?} vm={vr:?}"
    );
}

#[test]
fn exit_is_not_caught_by_recover() {
    // A hard exit unwinds past a `recover:` boundary — it is NOT converted to an `Err` value.
    let src = "import std.os\nfn main():\n    x := recover:\n        os.exit(7)\n    print(\"unreachable\")\nmain()";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (io, _ie, _ir, ic) = run_file_p(&entry);
    let (vo, _ve, _vr, vc) = run_file(&entry);
    assert_eq!(io, "", "interp: nothing after the recover runs");
    assert_eq!(vo, "", "vm: nothing after the recover runs");
    assert_eq!(ic, Some(7), "interp exit code");
    assert_eq!(vc, Some(7), "vm exit code");
}

#[test]
fn exit_in_spawned_child_aborts_siblings() {
    // B1/B2: `std.os.exit` inside a child fiber is a hard halt — it aborts the remaining siblings
    // and the rest of the program. The first child prints then exits(3); the second child and the
    // post-`parallel:` statement never run. Identical on both engines (no blocking involved).
    let src = "import std.os\nfn a():\n    print(\"a\")\n    os.exit(3)\nfn b():\n    print(\"b\")\nfn main():\n    parallel:\n        spawn a()\n        spawn b()\n    print(\"after\")\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (io, _ie, ir, ic) = run_file_p(&entry);
    let (vo, _ve, vr, vc) = run_file(&entry);
    assert_eq!(
        vo, "a\n",
        "vm: sibling and post-parallel statement aborted by os.exit"
    );
    assert_eq!(
        io, "a\n",
        "interp: sibling and post-parallel statement aborted by os.exit"
    );
    assert_eq!(vc, Some(3), "vm exit code");
    assert_eq!(ic, Some(3), "interp exit code");
    assert!(
        ir.is_ok() && vr.is_ok(),
        "os.exit is a clean halt, not an error: interp={ir:?} vm={vr:?}"
    );
}

/// B3.4: a child `std.os.exit(code)` on the `--parallel` OS-thread pool is a clean hard-halt,
/// not a fault. Cross-thread: the worker's `pending_exit` propagates up the join to the parent
/// VM, the exiting child's buffered output is flushed, and the post-`parallel:` statement never
/// runs. The `--parallel` counterpart of `exit_in_spawned_child_aborts_siblings`.
#[test]
fn parallel_child_os_exit_halts_with_code() {
    let src = "import std.os\nfn a():\n    print(\"a\")\n    os.exit(3)\nfn b():\n    print(\"b\")\nfn main():\n    parallel:\n        spawn a()\n        spawn b()\n    print(\"after\")\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (out, _err, res, code) = run_file_parallel(&entry, crate::native::HostConfig::default());
    assert_eq!(
        code,
        Some(3),
        "child os.exit code propagates cross-thread to the parent"
    );
    assert!(
        res.is_ok(),
        "os.exit is a clean halt, not an error: {res:?}"
    );
    assert!(
        out.contains('a'),
        "the exiting child's buffered output is flushed: got {out:?}"
    );
    assert!(
        !out.contains("after"),
        "the post-parallel statement never runs after os.exit: got {out:?}"
    );
}

/// B3.4: `os.exit` in one child aborts a `recv`-blocked sibling too (same machinery as a fault —
/// it trips the nursery cancel flag), so the join completes with the exit code instead of hanging.
/// `exiter` is spawned first → runs inline on the joining thread (the exit trips cancel without
/// depending on pool scheduling); the recv-blocked `consumer` runs on the pool and aborts.
#[test]
fn parallel_os_exit_aborts_recv_blocked_sibling() {
    let src = "import std.os\nfn exiter(ch: Channel[int]):\n    os.exit(5)\nfn consumer(ch: Channel[int]):\n    ch.recv()\n    print(\"consumed\")\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn exiter(ch)\n        spawn consumer(ch)\nmain()\n";
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (out, _err, _res, code) = run_file_parallel(&entry, crate::native::HostConfig::default());
    assert_eq!(
        code,
        Some(5),
        "os.exit code propagates; the recv-blocked consumer aborts, no hang"
    );
    assert!(
        !out.contains("consumed"),
        "the aborted consumer never ran past its blocked recv: got {out:?}"
    );
}

/// B3.5 — run an entry on the `--parallel` engine under a watchdog: a missing/broken deadlock
/// detector would hang the nursery forever, so we run it on a side thread and fail loudly if it
/// doesn't finish, instead of wedging the whole test binary. (On a clean detector none of these
/// ever time out — the leak only happens on the failure path we're guarding against.)
fn run_parallel_watchdog(src: &str) -> RunOutput {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &entry,
            crate::native::HostConfig::default(),
        ));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(r) => r,
        Err(_) => {
            panic!("hung: --parallel nursery did not terminate (deadlock detection missing/broken)")
        }
    }
}

/// B3.5 — the cooperative `fibers_all_blocked_is_deadlock` golden, ported to `--parallel`: two
/// tasks each block on a distinct empty channel with no producer. The cooperative scheduler
/// already faults this; under threads B3.5's nursery-local detector must fault it too rather
/// than hang on the condvars.
#[test]
fn parallel_all_blocked_deadlock_faults() {
    let src = "fn waiter(c: Channel[int]):\n    c.recv()\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn waiter(a)\n        spawn waiter(b)\nmain()\n";
    let (_o, _e, res, _c) = run_parallel_watchdog(src);
    let err = res.expect_err("an all-blocked --parallel nursery must fault, not hang");
    assert!(err.message.contains("deadlock"), "got: {}", err.message);
}

/// `std.cancel` wakeup regression — a sibling's `cancel()` (which `trip()`s the token's `done()`
/// channel) must wake a fiber **parked** in `wait: tok.done().recv()` under the OS-thread engine.
/// The canceller sleeps first so the waiter reliably reaches `park_wait` and blocks; the `trip()`
/// then routes through `close_wake`, which must reach the parked `WaitPark` token and re-poll it to
/// observe the latch. (The *narrower* park-decision-vs-`trip` race is closed by adding `done_latch`
/// to `MnSched::park`/`park_wait`'s gap re-check — correct by construction, mirroring how `closed`
/// is handled; not deterministically forceable from source.) All rounds must complete.
#[test]
fn cancel_trip_wakes_parked_wait_under_parallel() {
    let src = "import std.cancel\n\
                   import std.time\n\
                   fn waiter(tok: Token, out: Channel[bool]):\n\
                   \x20   wait:\n\
                   \x20       _ := tok.done().recv(): out.send(true)\n\
                   fn canceller(tok: Token):\n\
                   \x20   time.sleep_ms(5)\n\
                   \x20   tok.cancel()\n\
                   fn main():\n\
                   \x20   out := Channel[bool]()\n\
                   \x20   n := 30\n\
                   \x20   for i in 0..n:\n\
                   \x20       tok := cancel.manual()\n\
                   \x20       parallel:\n\
                   \x20           spawn waiter(tok, out)\n\
                   \x20           spawn canceller(tok)\n\
                   \x20   c := 0\n\
                   \x20   for _ in 0..n:\n\
                   \x20       out.recv()\n\
                   \x20       c = c + 1\n\
                   \x20   print(c)\n\
                   main()\n";
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(res.is_ok(), "cancel→wait must not fault/strand: {res:?}");
    assert_eq!(
        out, "30\n",
        "every parked waiter must be woken by the sibling's cancel"
    );
}

/// B3.5 — the named anti-false-positive case: one sibling `send`s the very channel the other
/// `recv`s, so the nursery genuinely progresses. The barrier-confirm detector must NOT report a
/// deadlock (a real send aborts any half-built all-blocked confirmation).
#[test]
fn parallel_near_miss_does_not_false_positive() {
    let src = "fn consumer(c: Channel[int]):\n    print(c.recv())\nfn producer(c: Channel[int]):\n    c.send(7)\nfn main():\n    c := Channel[int]()\n    parallel:\n        spawn consumer(c)\n        spawn producer(c)\n    print(\"done\")\nmain()\n";
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(res.is_ok(), "near-miss must not fault: {res:?}");
    assert!(
        out.contains('7'),
        "consumer received the sent value: {out:?}"
    );
    assert!(
        out.contains("done"),
        "the nursery joined and main continued: {out:?}"
    );
}

/// B3.5 — a three-task relay (consumer ← relay ← producer) where `blocked == live` is reached
/// only momentarily while a message is in flight. A naive blocked-count detector false-positives
/// here; the per-epoch barrier (a worker holding a deliverable message pops it instead of
/// confirming empty) must not.
#[test]
fn parallel_chained_near_miss_no_false_positive() {
    let src = "fn relay(x: Channel[int], z: Channel[int]):\n    v := x.recv()\n    z.send(v)\nfn producer(x: Channel[int]):\n    x.send(1)\nfn consumer(z: Channel[int]):\n    print(z.recv())\nfn main():\n    x := Channel[int]()\n    z := Channel[int]()\n    parallel:\n        spawn consumer(z)\n        spawn relay(x, z)\n        spawn producer(x)\n    print(\"ok\")\nmain()\n";
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_ok(),
        "chained relay must not false-positive: {res:?}"
    );
    assert!(
        out.contains('1'),
        "the relayed value reached the consumer: {out:?}"
    );
    assert!(out.contains("ok"), "the nursery joined: {out:?}");
}

/// D6 — single-connection loopback over `std.net` on the M:N engine: a `parallel:` runs a server
/// fiber (`listen`/`accept`/`read`/`write`) and a client fiber (`connect`/`write`/`read`) in one
/// program. A would-block `accept`/`read` parks on the netpoller and resumes on readiness, so the
/// round-trip completes without a thread per op. `Listener.addr()` surfaces the OS-assigned port so
/// the client can reach the `:0` bind. Watchdog-guarded.
#[test]
fn net_loopback_round_trip_over_parallel() {
    let src = r#"import std.net

fn serve(server: Listener) -> int!:
    conn := server.accept()?
    msg := conn.read(64)?
    conn.write("echo:" + msg)?
    conn.close()
    server.close()
    return Ok(0)

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("hello")?
    reply := sock.read(64)?
    print(reply)
    sock.close()
    return Ok(0)

fn run() -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn serve(server)
        spawn client(addr)
    return Ok(0)

fn main():
    match run():
        Ok(_): print("done")
        Err(e): print("net error: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(res.is_ok(), "net round-trip must not fault: {res:?}");
    assert!(
        out.contains("echo:hello"),
        "the client received the server's echo: {out:?}"
    );
    assert!(
        out.contains("done"),
        "the nursery joined and main continued: {out:?}"
    );
    assert!(
        !out.contains("net error"),
        "no I/O error on the happy path: {out:?}"
    );
}

/// D6c — run a `--parallel` net program from a temp file under a 30 s watchdog (net round-trips
/// can legitimately take longer than `run_parallel_watchdog`'s 5 s, and a regressed timeout would
/// HANG rather than fault). Returns the captured stdout, or panics loudly on a hang.
fn run_net_timeout_watchdog(tag: &str, src: &str) -> String {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &entry,
            crate::native::HostConfig::default(),
        ));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "{tag}: program faulted: {res:?}");
            out
        }
        Err(_) => panic!("{tag}: hung — D6c socket timeout regressed (the op parked forever)"),
    }
}

/// D6c — `conn.read(n, timeout_ms)` returns `Err("timeout")` when the peer accepts but never
/// writes. The server accepts the connection and then sleeps past the client's read timeout; the
/// client's `read(64, 100)` parks on the netpoller with a 100 ms deadline, the deadline fires
/// before any data, and the rewound op returns `Err` with `e.message() == "timeout"`.
#[test]
fn read_timeout_returns_err() {
    let src = "\
import std.net
import std.time

fn server(listener: Listener) -> int!:
    conn := listener.accept()?
    time.sleep_ms(400)
    conn.close()
    listener.close()
    return Ok(0)

fn client(addr: str):
    sock := net.connect(addr)?
    match sock.read(64, 100):
        Ok(s): print(\"GOT:\" + s)
        Err(e): print(\"ERR:\" + e.message())
    sock.close()

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    addr := listener.addr()?
    parallel:
        spawn server(listener)
        spawn client(addr)
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"\")
        Err(e): print(\"RUN-ERR:\" + e.message())

main()
";
    let out = run_net_timeout_watchdog("read_timeout", src);
    assert!(
        out.contains("ERR:timeout"),
        "read(64, 100) must surface Err(\"timeout\"): {out:?}"
    );
    assert!(
        !out.contains("GOT:"),
        "no data should have been read: {out:?}"
    );
}

/// D6c — `server.accept(timeout_ms)` returns `Err("timeout")` when NO client ever connects, and the
/// program terminates (no hang). The lone acceptor parks on the netpoller with a deadline; the
/// deadline fires, the rewound `accept` returns `Err("timeout")`, and the nursery joins.
#[test]
fn accept_timeout_returns_err() {
    let src = "\
import std.net

fn server(listener: Listener):
    match listener.accept(100):
        Ok(_): print(\"ACCEPTED\")
        Err(e): print(\"ERR:\" + e.message())
    listener.close()

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    parallel:
        spawn server(listener)
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"done\")
        Err(e): print(\"RUN-ERR:\" + e.message())

main()
";
    let out = run_net_timeout_watchdog("accept_timeout", src);
    assert!(
        out.contains("ERR:timeout"),
        "accept(100) with no client must surface Err(\"timeout\"): {out:?}"
    );
    assert!(
        out.contains("done"),
        "the nursery joined and main continued (no hang): {out:?}"
    );
    assert!(!out.contains("ACCEPTED"), "nothing was accepted: {out:?}");
}

/// D6c regression — a `read(n)` with NO timeout still parks FOREVER (until data arrives): the
/// timeout machinery must not have made the untimed read return early. The server sleeps well past
/// any plausible deadline before writing; the client's untimed `read(64)` must wait for the bytes
/// (not time out) and print them.
#[test]
fn read_without_timeout_still_parks_forever() {
    let src = "\
import std.net
import std.time

fn server(listener: Listener) -> int!:
    conn := listener.accept()?
    time.sleep_ms(300)
    conn.write(\"late\")?
    conn.close()
    listener.close()
    return Ok(0)

fn client(addr: str):
    sock := net.connect(addr)?
    match sock.read(64):
        Ok(s): print(\"GOT:\" + s)
        Err(e): print(\"ERR:\" + e.message())
    sock.close()

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    addr := listener.addr()?
    parallel:
        spawn server(listener)
        spawn client(addr)
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"\")
        Err(e): print(\"RUN-ERR:\" + e.message())

main()
";
    let out = run_net_timeout_watchdog("read_no_timeout", src);
    assert!(
        out.contains("GOT:late"),
        "an untimed read must block until data, not time out: {out:?}"
    );
    assert!(
        !out.contains("ERR:"),
        "no timeout/error on the untimed read: {out:?}"
    );
}

/// D6c — the bundled `examples/socket_timeout.chz` golden: a `--parallel` program that demonstrates
/// both an `accept(timeout_ms)` and a `read(n, timeout_ms)` timeout branch, run end-to-end against
/// its `.expected` output. Net examples need `--parallel` (no fibers to park on the cooperative
/// engine), so — like `echo_server.chz` — this is exercised here rather than in the cooperative
/// golden harness. Watchdog-guarded so a regression faults instead of hanging the test binary.
#[test]
fn example_socket_timeout_matches_expected() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let entry = manifest.join("examples/socket_timeout.chz");
    let expected = std::fs::read_to_string(manifest.join("examples/socket_timeout.expected"))
        .expect("read examples/socket_timeout.expected");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &entry,
            crate::native::HostConfig::default(),
        ));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "socket_timeout.chz faulted: {res:?}");
            assert_eq!(
                out, expected,
                "socket_timeout.chz output diverged from its golden"
            );
        }
        Err(_) => panic!("socket_timeout.chz hung — D6c timeout regressed"),
    }
}

/// D6b — the production-ready gate (regression for the documented HANG): a `parallel:` runs a
/// fiber parked on `accept` (no client ever connects, so it parks on the netpoller forever) beside
/// a sibling that faults. Before D6b's `poller::drain_sched`, the faulting sibling tripped cancel
/// but never reached the poller-parked acceptor — its task stayed `inflight`, the fault never
/// propagated, and the nursery wedged. Now the drain re-injects the acceptor, it unwinds on the
/// cancel flag, and the original fault surfaces. Watchdog-guarded: a regression re-hangs here.
#[test]
fn net_faulting_sibling_aborts_accept_parked_peer() {
    let src = r#"import std.net

fn faulter(z: int) -> int!:
    return Ok(10 / z)

fn acceptor(server: Listener) -> int!:
    conn := server.accept()?
    conn.close()
    return Ok(0)

fn run() -> int!:
    server := net.listen("127.0.0.1:0")?
    parallel:
        spawn acceptor(server)
        spawn faulter(0)
    return Ok(0)

fn main():
    match run():
        Ok(_): print("joined ok")
        Err(e): print("caught: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    let err = res.expect_err("the faulting sibling's error must propagate, not hang the nursery");
    assert!(
        err.message.contains("division by zero"),
        "the original fault surfaces: {}",
        err.message
    );
    assert!(
        !out.contains("joined ok"),
        "the nursery faulted rather than joining cleanly: {out:?}"
    );
}

/// D6b — non-blocking `connect` actually parks (and is drainable): a fiber connects to an
/// unroutable TEST-NET-1 address (RFC 5737 `192.0.2.0/24` — the SYN gets no reply, so the
/// non-blocking connect stays `EINPROGRESS` and the fiber parks on writability *indefinitely*),
/// while a sibling faults. A blocking v1 connect would have pinned a worker on the dead handshake;
/// the parked connect must instead be reached by `poller::drain_sched` so the fault propagates and
/// the nursery joins. Deterministic (the address never completes) and watchdog-guarded.
#[test]
fn net_connect_parks_and_is_drained_on_fault() {
    let src = r#"import std.net

fn faulter(z: int) -> int!:
    return Ok(10 / z)

fn dialer() -> int!:
    sock := net.connect("192.0.2.1:9")?
    sock.close()
    return Ok(0)

fn run() -> int!:
    parallel:
        spawn dialer()
        spawn faulter(0)
    return Ok(0)

fn main():
    match run():
        Ok(_): print("joined ok")
        Err(e): print("caught: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    let err = res.expect_err("the faulting sibling aborts the connect-parked dialer, no hang");
    assert!(
        err.message.contains("division by zero"),
        "the original fault surfaces: {}",
        err.message
    );
    assert!(
        !out.contains("joined ok"),
        "the nursery faulted rather than joining cleanly: {out:?}"
    );
}

/// D6b — the top-level (no-`--parallel`) blocking connect fallback returns a clean `Err` rather
/// than hanging: `net.connect` to a dead loopback port (bound-then-dropped) settles to a refusal
/// through `block_until_connected`. Guards the bounded-spin fix — a regression to an unbounded spin
/// on a non-completing handshake would surface as a watchdog timeout here.
#[test]
fn net_connect_top_level_dead_port_errors_not_hangs() {
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let src = format!(
        "import std.net\nfn main():\n    match net.connect(\"{dead}\"):\n        Ok(_): print(\"connected\")\n        Err(e): print(\"refused\")\nmain()\n"
    );
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let t = TmpDir::new();
        let entry = t.write("main.chz", &src);
        let (out, _e, res, _c) = run_file(&entry); // cooperative (no --parallel) ⇒ the blocking fallback
        let _ = tx.send((out, res));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(15)) {
        Ok((out, res)) => {
            res.expect("top-level connect program runs");
            assert_eq!(
                out, "refused\n",
                "dead port ⇒ Err branch (bounded, no hang)"
            );
        }
        Err(_) => {
            panic!("hung: top-level connect to a dead port did not return (unbounded spin?)")
        }
    }
}

/// D6 — the headline netpoller test: an echo server services **far more connections than there are
/// workers**, without a thread per connection. One acceptor fiber + N=100 client fibers run in a
/// single `parallel:` over a core-sized pool (100 ≫ cores). Every client parks on its `read` and
/// the acceptor parks on each `accept`/`read` — on the netpoller, not a pinned worker — so all 100
/// round-trips complete. Without the poller (thread-per-park) the bounded pool would starve and the
/// watchdog would fire. (N stays under the TCP backlog so the v1 *blocking* connect never pins a
/// worker waiting for backlog room — non-blocking connect is deferred to D6b; the per-connection
/// handler runs inline in the acceptor because M:N's fixed task `total` has no spawn-after-join.)
#[test]
fn net_echo_server_services_more_conns_than_workers() {
    let src = r#"import std.net

fn acceptor(server: Listener, n: int) -> int!:
    for _ in 0..n:
        conn := server.accept()?
        msg := conn.read(64)?
        conn.write("echo:" + msg)?
        conn.close()
    server.close()
    return Ok(0)

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("ping")?
    reply := sock.read(64)?
    sock.close()
    if reply == "echo:ping":
        return Ok(1)
    return Err("bad reply: " + reply)

fn run(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn acceptor(server, n)
        for _ in 0..n:
            spawn client(addr)
    return Ok(0)

fn main():
    match run(100):
        Ok(_): print("all served")
        Err(e): print("error: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(res.is_ok(), "100-conn echo server must not fault: {res:?}");
    assert!(
        out.contains("all served"),
        "every connection was serviced + the nursery joined: {out:?}"
    );
    assert!(!out.contains("error"), "no client saw a bad echo: {out:?}");
}

/// Per-connection spawn — the spec's canonical shape: the acceptor `spawn`s a `handle(conn)`
/// fiber PER connection inside its `parallel:` instead of serving inline, and the inner nursery
/// joins them. `#conns ≫ #workers` still completes (handlers multiplex over the core-sized pool).
/// Exercises the eager-nursery + `MnSched::inject` path end-to-end with the bytecode engine.
#[test]
fn net_echo_server_spawns_handler_per_connection() {
    // Nested socket nurseries (an acceptor's `parallel:` servicing outer-sibling clients) need
    // ≥2 hw threads: the inner join blocks the parent's outer worker (decision B), so on a single
    // core the outer clients can't progress to drain the echoes — a pre-existing M:N limit that
    // per-connection spawn is the first to exercise. Skip on 1 core (CI is ≥2 core) rather than hang.
    if std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        < 2
    {
        return;
    }
    let src = r#"import std.net

fn handle(conn: Socket) -> int!:
    msg := conn.read(64)?
    conn.write("echo:" + msg)?
    conn.close()
    return Ok(0)

fn acceptor(server: Listener, n: int) -> int!:
    parallel:
        for _ in 0..n:
            conn := server.accept()?
            spawn handle(conn)
    server.close()
    return Ok(0)

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("ping")?
    reply := sock.read(64)?
    sock.close()
    if reply == "echo:ping":
        return Ok(1)
    return Err("bad reply: " + reply)

fn run(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn acceptor(server, n)
        for _ in 0..n:
            spawn client(addr)
    return Ok(0)

fn main():
    match run(100):
        Ok(_): print("all served")
        Err(e): print("error: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_ok(),
        "per-connection-spawn echo server must not fault: {res:?}"
    );
    assert!(
        out.contains("all served"),
        "every connection was handled by its own fiber: {out:?}"
    );
    assert!(!out.contains("error"), "no client saw a bad echo: {out:?}");
}

/// Per-connection spawn — proves handlers run CONCURRENTLY with accepting (not queued-to-join).
/// A single client opens N connections SEQUENTIALLY: each reply must arrive before the next
/// connect. The acceptor `spawn`s a handler per connection. Under the old queue-at-join model the
/// handler never ran during the accept loop, so the client's first `read` blocked forever and the
/// acceptor's second `accept` had no incoming connection → hang (watchdog fires). The eager
/// inner nursery runs each handler immediately, unblocking the client so the loop advances.
#[test]
fn net_echo_sequential_client_needs_concurrent_handlers() {
    // Eager per-connection spawn requires ≥2 hardware threads (the inner join blocks the parent's
    // sole outer worker on a single core — see `Op::EnterNursery`). This test's whole point is a
    // handler running mid-loop to unblock the next connect, which a 1-core box cannot do; skip it
    // there rather than hang. CI runners are ≥2 core in practice.
    if std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        < 2
    {
        return;
    }
    let src = r#"import std.net

fn handle(conn: Socket) -> int!:
    msg := conn.read(64)?
    conn.write("echo:" + msg)?
    conn.close()
    return Ok(0)

fn acceptor(server: Listener, n: int) -> int!:
    parallel:
        for _ in 0..n:
            conn := server.accept()?
            spawn handle(conn)
    server.close()
    return Ok(0)

fn client(addr: str, n: int) -> int!:
    for i in 0..n:
        sock := net.connect(addr)?
        sock.write("ping")?
        reply := sock.read(64)?
        sock.close()
        if reply != "echo:ping":
            return Err("bad reply: " + reply)
    return Ok(0)

fn run(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn acceptor(server, n)
        spawn client(addr, n)
    return Ok(0)

fn main():
    match run(8):
        Ok(_): print("all served")
        Err(e): print("error: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_ok(),
        "sequential client must complete once handlers run concurrently: {res:?}"
    );
    assert!(
        out.contains("all served"),
        "all 8 sequential round-trips serviced: {out:?}"
    );
    assert!(
        !out.contains("error"),
        "every reply was a correct echo: {out:?}"
    );
}

/// Per-connection spawn — a per-connection HANDLER fault propagates as the acceptor's fault and
/// tears the run down WITHOUT hanging. One injected handler faults (index-out-of-bounds — a real
/// runtime fault, since a spawned task's `Result` *return value* is discarded); the eager inner
/// nursery trips its own cancel (D6b `cancel_drain` + `drain_sched` reach sibling handlers), the
/// join surfaces the fault as the acceptor's body fault, and the OUTER nursery then cancels the
/// clients (so a client stranded without its echo unwinds instead of blocking on `read` forever).
#[test]
fn net_echo_handler_fault_cancels_acceptor() {
    // Nested socket nursery → needs ≥2 hw threads (see `net_echo_server_spawns_handler_per_connection`).
    if std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        < 2
    {
        return;
    }
    let src = r#"import std.net

fn handle(conn: Socket, i: int) -> int!:
    msg := conn.read(64)?
    if i == 0:
        conn.close()
        boom := [1]
        return Ok(boom[10])
    conn.write("echo:" + msg)?
    conn.close()
    return Ok(0)

fn acceptor(server: Listener, n: int) -> int!:
    parallel:
        for i in 0..n:
            conn := server.accept()?
            spawn handle(conn, i)
    server.close()
    return Ok(0)

fn client(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("ping")?
    reply := sock.read(64)?
    sock.close()
    return Ok(1)

fn run(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn acceptor(server, n)
        for _ in 0..n:
            spawn client(addr)
    return Ok(0)

fn main():
    match run(6):
        Ok(_): print("all served")
        Err(e): print("error: " + e.message())

main()
"#;
    // The whole point is "no hang": `run_parallel_watchdog` panics if the nursery never
    // terminates. A faulting handler must drive the run to a clean finish (fault surfaced via the
    // acceptor's `match`, or propagated), not deadlock the netpoller-parked siblings.
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_err() || out.contains("error"),
        "a per-connection handler fault must surface (faulted run or reported error), not be swallowed: res={res:?} out={out:?}"
    );
    assert!(
        !out.contains("all served"),
        "the run must not report success once a handler faulted: {out:?}"
    );
}

/// Per-connection spawn — the DEGENERATE eager nursery: a `parallel:` body (entered eagerly under
/// `--parallel` inside a fiber) that injects NOTHING. `activate_eager_nursery` builds a `total==0`
/// sched with `body_open`; `JoinNursery` must `close_body` and have the inline worker terminate
/// immediately (`done==0==total`) and join the drainer — not hang on the empty sched. Pins the
/// `body_open` → `close_body` → terminate handshake on the empty path.
#[test]
fn eager_nursery_with_zero_spawns_completes() {
    if std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        < 2
    {
        return;
    }
    let src = "fn worker():\n    parallel:\n        print(\"eager body, no spawn\")\n    print(\"worker done\")\nfn main():\n    parallel:\n        spawn worker()\nmain()\n";
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_ok(),
        "an empty eager nursery must join cleanly: {res:?}"
    );
    assert!(
        out.contains("eager body, no spawn"),
        "the body ran: {out:?}"
    );
    assert!(
        out.contains("worker done"),
        "the eager nursery joined and the worker continued: {out:?}"
    );
}

/// Per-connection spawn — CONCURRENT eager nurseries (the pool-exhaustion regression): four
/// independent servers each run their OWN eager per-connection-spawn nursery at once. Because each
/// eager nursery drains on a DEDICATED raw OS thread (not the bounded process pool), they do not
/// starve each other — with the earlier pool-farmed design, four long-running eager drainers would
/// exhaust a core-sized pool and hang (undetectably, since `body_open` vetoes the deadlock predicate).
#[test]
fn net_concurrent_eager_servers_do_not_exhaust_pool() {
    if std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        < 2
    {
        return;
    }
    let src = r#"import std.net

fn handle(conn: Socket) -> int!:
    msg := conn.read(64)?
    conn.write("echo:" + msg)?
    conn.close()
    return Ok(0)

fn server_loop(server: Listener, n: int) -> int!:
    parallel:
        for _ in 0..n:
            conn := server.accept()?
            spawn handle(conn)
    server.close()
    return Ok(0)

fn pinger(addr: str) -> int!:
    sock := net.connect(addr)?
    sock.write("ping")?
    reply := sock.read(64)?
    sock.close()
    if reply == "echo:ping":
        return Ok(1)
    return Err("bad reply: " + reply)

fn one_server(n: int) -> int!:
    server := net.listen("127.0.0.1:0")?
    addr := server.addr()?
    parallel:
        spawn server_loop(server, n)
        for _ in 0..n:
            spawn pinger(addr)
    return Ok(0)

fn run(servers: int, conns: int) -> int!:
    parallel:
        for _ in 0..servers:
            spawn one_server(conns)
    return Ok(0)

fn main():
    match run(4, 12):
        Ok(_): print("all servers done")
        Err(e): print("error: " + e.message())

main()
"#;
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    assert!(
        res.is_ok(),
        "concurrent eager servers must not fault: {res:?}"
    );
    assert!(
        out.contains("all servers done"),
        "every concurrent eager nursery completed: {out:?}"
    );
    assert!(!out.contains("error"), "no pinger saw a bad echo: {out:?}");
}

/// B3.5 — a task that finishes normally but strands a `recv`-blocked sibling (it never sent the
/// channel the sibling waits on) is a deadlock. Exercises the `task_finished` `live--` path:
/// dropping the finished task from the live count makes `blocked == live`, so the survivor faults.
#[test]
fn parallel_finished_task_leaves_sibling_deadlocked() {
    let src = "fn waiter(c: Channel[int]):\n    c.recv()\nfn quick():\n    print(\"quick\")\nfn main():\n    c := Channel[int]()\n    parallel:\n        spawn waiter(c)\n        spawn quick()\nmain()\n";
    let (out, _e, res, _c) = run_parallel_watchdog(src);
    let err = res.expect_err("a finished sibling that strands a recv-blocked task is a deadlock");
    assert!(err.message.contains("deadlock"), "got: {}", err.message);
    assert!(
        out.contains("quick"),
        "the finished task's output is still flushed: {out:?}"
    );
}

/// Run an entry through both engines with a freshly-built [`crate::native::HostConfig`] each
/// (the config isn't `Clone` — `mk_cfg` produces an identical one per engine). Asserts stdout +
/// ok/err parity; returns the agreed stdout.
fn parity_entry_cfg(src: &str, mk_cfg: impl Fn() -> crate::native::HostConfig) -> String {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (io, ie_out, ir, _ic) = run_file_parallel(&entry, mk_cfg());
    let (vo, ve_out, vr, _vc) = run_file_with(&entry, mk_cfg());
    assert_eq!(io, vo, "stdout divergence (interp vs vm)");
    assert_eq!(ie_out, ve_out, "stderr divergence (interp vs vm)");
    assert_eq!(
        ir.is_ok(),
        vr.is_ok(),
        "ok/err divergence: interp={ir:?} vm={vr:?}"
    );
    io
}

#[test]
fn parity_std_io_print() {
    assert_eq!(
        parity_entry("import std.io\nfn main():\n    io.print(\"hello\")\nmain()"),
        "hello\n"
    );
}

#[test]
fn parity_std_io_read_write_file() {
    let t = TmpDir::new();
    let data = t.0.join("data.txt").display().to_string();
    let src = format!(
        "import std.io\nfn main():\n    match io.write_file(\"{data}\", \"hello\\nworld\"):\n        Ok(_): io.print(\"wrote\")\n        Err(e): io.print(e)\n    match io.read_file(\"{data}\"):\n        Ok(s): io.print(s)\n        Err(e): io.print(e)\nmain()"
    );
    let entry = t.write("main.chz", &src);
    let (io_out, _ie, ir, _) = run_file_p(&entry);
    let (vo, _ve, vr, _) = run_file(&entry);
    assert!(ir.is_ok() && vr.is_ok(), "interp={ir:?} vm={vr:?}");
    assert_eq!(io_out, vo);
    assert_eq!(io_out, "wrote\nhello\nworld\n");
}

#[test]
fn parity_std_io_read_missing_file_errs() {
    // The error text comes from the same `std::fs` call on both engines, so it matches; we only
    // assert the Err branch is taken (deterministic regardless of OS message).
    let src = "import std.io\nfn main():\n    match io.read_file(\"/no/such/chezzi/path/xyz\"):\n        Ok(s): io.print(s)\n        Err(e): io.print(\"err\")\nmain()";
    assert_eq!(parity_entry(src), "err\n");
}

#[test]
#[cfg(target_os = "linux")]
fn read_file_caps_oversized_input() {
    // /dev/zero is unbounded; read_file must return an Err (the size cap), not OOM.
    let src = "import std.io\nfn main():\n    match io.read_file(\"/dev/zero\"):\n        Ok(s): io.print(\"ok\")\n        Err(e): io.print(\"capped\")\nmain()";
    assert_eq!(parity_entry(src), "capped\n");
}

#[test]
fn parity_std_io_read_line_consumes_injected_stdin() {
    use crate::native::{HostConfig, Stdin};
    let src = "import std.io\nfn main():\n    match io.read_line():\n        Some(l): io.print(\"got {l}\")\n        None: io.print(\"eof\")\n    match io.read_line():\n        Some(l): io.print(l)\n        None: io.print(\"eof\")\nmain()";
    let out = parity_entry_cfg(src, || HostConfig {
        stdin: Stdin::Lines(["alpha".to_string()].into_iter().collect()),
        ..Default::default()
    });
    assert_eq!(out, "got alpha\neof\n");
}

#[test]
fn parity_std_io_eprint_goes_to_stderr_not_stdout() {
    let src = "import std.io\nfn main():\n    io.eprint(\"to stderr\")\n    io.print(\"to stdout\")\nmain()";
    // Parity (both engines): stdout has only the print line, stderr has only the eprint line.
    assert_eq!(parity_entry(src), "to stdout\n");
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (out, err, res, _) = run_file(&entry);
    assert!(res.is_ok());
    assert_eq!(out, "to stdout\n");
    assert_eq!(err, "to stderr\n");
}

#[test]
fn parity_std_os_args_and_env() {
    use crate::native::HostConfig;
    let src = "import std.io\nimport std.os\nfn main():\n    for a in os.args():\n        io.print(a)\n    match os.env(\"CHEZZI_TEST_VAR\"):\n        Some(v): io.print(v)\n        None: io.print(\"no var\")\nmain()";
    let out = parity_entry_cfg(src, || HostConfig {
        args: vec!["x".to_string(), "y".to_string()],
        env: [("CHEZZI_TEST_VAR".to_string(), "hi".to_string())]
            .into_iter()
            .collect(),
        ..Default::default()
    });
    assert_eq!(out, "x\ny\nhi\n");
}

#[test]
fn parity_std_os_env_missing_is_none() {
    use crate::native::HostConfig;
    let src = "import std.io\nimport std.os\nfn main():\n    match os.env(\"DEFINITELY_UNSET_XYZ\"):\n        Some(v): io.print(v)\n        None: io.print(\"none\")\nmain()";
    let out = parity_entry_cfg(src, HostConfig::default);
    assert_eq!(out, "none\n");
}

#[test]
fn parity_std_os_getcwd_ok() {
    let src = "import std.io\nimport std.os\nfn main():\n    match os.getcwd():\n        Ok(p): io.print(\"ok\")\n        Err(e): io.print(\"err\")\nmain()";
    assert_eq!(parity_entry(src), "ok\n");
}

/// Run a single-file (importing std) program on the VM with GC stress on (collect before every
/// instruction) and the given config — surfaces any native-return value the collector might free
/// while still reachable.
fn vm_run_file_stress(src: &str, cfg: crate::native::HostConfig) -> String {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).unwrap();
    let program = crate::compiler::compile_graph(&graph).unwrap();
    let mut vm = Vm::new(Arc::new(program));
    vm.gc_stress = true;
    vm.host = cfg;
    vm.run()
        .unwrap_or_else(|e| panic!("unexpected error under GC stress: {e}"));
    vm.out
}

#[test]
fn parity_std_str_pure_chezzi_with_mixed_native_import() {
    // std.str is a real Chezzi file (crate/std/str.chz); std.io is native — both in one program.
    let src = "import std.io\nimport std.str as text\nfn main():\n    io.print(text.repeat(\"ab\", 3))\n    io.print(text.reverse(\"hello\"))\n    io.print(text.pad_left(\"7\", 3, \"0\"))\n    if text.is_empty(\"\"):\n        io.print(\"empty\")\n    for line in text.split_lines(\"a\\nb\\nc\"):\n        io.print(line)\nmain()";
    assert_eq!(parity_entry(src), "ababab\nolleh\n007\nempty\na\nb\nc\n");
}

#[test]
fn native_returned_heap_values_survive_gc_stress() {
    use crate::native::HostConfig;
    // Each os.args() call allocates a fresh heap list (immediately garbage); under stress the
    // collector runs every instruction. A dangling handle in native lowering would panic here.
    let src = "import std.io\nimport std.os\nfn main():\n    n := 0\n    while n < 300:\n        xs := os.args()\n        n += 1\n    io.print(\"done {n}\")\nmain()";
    let cfg = HostConfig {
        args: vec!["a".to_string()],
        ..Default::default()
    };
    let out = vm_run_file_stress(src, cfg);
    assert_eq!(out, "done 300\n");
}

/// A spread of programs exercising every feature class — run through BOTH engines.
const PROGRAMS: &[&str] = &[
    // arithmetic + promotion + truncation
    "print(7 / 2)\nprint(1 + 2.0)\nprint(2.5 * 2.0)\nprint(10 % 3)",
    // string concat + interpolation + escapes
    "fn main():\n    n := \"x\"\n    print(\"a{n}b {1 + 2} {{lit}}\")\nmain()",
    // comparison + equality + bool logic
    "print(1 < 2)\nprint(2 == 2.0)\nprint(true and false)\nprint(false or true)\nprint(not true)",
    // lists, indexing, len
    "print([1, 2, 3])\nprint([10, 20, 30][2])\nprint([1, 2].len())",
    // structs + methods
    "struct P:\n    x: int\n    y: int\n    fn sum(self) -> int:\n        return self.x + self.y\nfn main():\n    p := P(3, 4)\n    print(p)\n    print(p.sum())\nmain()",
    // enums + match + payload binding
    "enum S:\n    C(int)\n    Sq(int)\nfn a(s: S) -> int:\n    match s:\n        S.C(r): return r * r\n        S.Sq(n): return n * n\nfn main():\n    print(a(S.C(3)))\n    print(a(S.Sq(4)))\nmain()",
    // generic enum (type-erased): same enum at two element types + match payload substitution
    "enum Tree[T]:\n    Leaf\n    Node(T, Tree[T], Tree[T])\nfn sum(t: Tree[int]) -> int:\n    match t:\n        Tree.Leaf: return 0\n        Tree.Node(v, l, r): return sum(l) + v + sum(r)\nfn main():\n    t: Tree[int] = Tree.Node(2, Tree.Node(1, Tree.Leaf, Tree.Leaf), Tree.Node(3, Tree.Leaf, Tree.Leaf))\n    print(sum(t))\nmain()",
    // closures
    "fn adder(n: int):\n    return fn(x: int) -> int: x + n\nfn main():\n    f := adder(10)\n    print(f(5))\nmain()",
    // ? operator (Ok + Err propagation)
    "fn d(a: int, b: int) -> Result[int]:\n    if b == 0:\n        return Err(\"zero\")\n    return Ok(a / b)\nfn use() -> Result[int]:\n    r := d(10, 0)?\n    return Ok(r)\nfn main():\n    match use():\n        Ok(v): print(v)\n        Err(e): print(e)\nmain()",
    // for + while loops
    "fn main():\n    t := 0\n    for i in 0..100:\n        t += i\n    print(t)\n    n := 5\n    while n > 0:\n        n -= 1\n    print(n)\nmain()",
    // builtins
    "print(range(4))\nprint(int(\"7\") + 1)\nprint(float(3))\nprint([1, 2, 3].len())\nprint(str(42))",
    // recursion
    "fn fib(n: int) -> int:\n    if n < 2:\n        return n\n    return fib(n - 1) + fib(n - 2)\nfn main():\n    print(fib(15))\nmain()",
    // inferred return type (no `-> T`): runtime is unaffected, both engines agree
    "fn add(a: int, b: int):\n    return a + b\nfn classify(n: int):\n    if n == 0:\n        return Some(0)\n    return None\nfn main():\n    print(add(2, 3))\n    match classify(0):\n        Some(v): print(v)\n        None: print(\"none\")\nmain()",
    // expression-valued match (multiline) + if (inline): both engines must agree on the value
    "fn lookup(k: int) -> int?:\n    if k == 0:\n        return None\n    return Some(k)\nfn main():\n    found := match lookup(7):\n        Some(v): v\n        None: -1\n    print(found)\n    sign := if found > 0: \"pos\" else: \"neg\"\n    print(sign)\n    none := match lookup(0):\n        Some(v): v\n        None: -1\n    print(none)\nmain()",
    // ----- M6: core-type methods (str) -----
    "print(\"abcd\".len())\nprint(\"Hi There\".upper())\nprint(\"Hi There\".lower())\nprint(\"  pad  \".trim())",
    // str conforms to Error: message() returns the string itself
    "print(\"boom\".message())",
    // Go-style Result[T, E]: custom struct error (T!E), match, message() dispatch
    "struct DbErr:\n    code: int\n    fn message(self) -> str:\n        return \"db {self.code}\"\nfn q(ok: bool) -> int!DbErr:\n    if ok:\n        return Ok(1)\n    return Err(DbErr(503))\nfn main():\n    match q(false):\n        Ok(v): print(v)\n        Err(e): print(e.message())\n    match q(true):\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()",
    // default-Error path: Err(str) flows as Result[int, Error], consumed via message()
    "fn parse(ok: bool) -> int!:\n    if ok:\n        return Ok(42)\n    return Err(\"bad input\")\nfn main():\n    match parse(false):\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()",
    // ----- M11 Phase B: recover boundary -----
    // recover catches index-OOB; Ok path wraps the trailing value
    "fn main():\n    r := recover:\n        [1, 2][9]\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"recovered: {e.message()}\")\nmain()",
    // recover catches divide-by-zero
    "fn main():\n    r := recover:\n        10 / 0\n    match r:\n        Ok(v): print(v)\n        Err(e): print(\"err: {e.message()}\")\nmain()",
    // recover catches integer overflow
    "fn main():\n    r := recover:\n        9223372036854775807 * 2\n    match r:\n        Ok(v): print(v)\n        Err(e): print(\"ovf\")\nmain()",
    // recover ok-path wraps the value
    "fn main():\n    r := recover:\n        2 + 3\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err\")\nmain()",
    // a fault three calls deep is caught at the boundary (no per-call wrapping)
    "fn a() -> int:\n    return b()\nfn b() -> int:\n    return c()\nfn c() -> int:\n    return [1][9]\nfn main():\n    r := recover:\n        a()\n    match r:\n        Ok(v): print(v)\n        Err(e): print(\"deep recovered\")\nmain()",
    // `?` inside recover short-circuits to `r` (try-block): the Err lands in `r`, and code
    // AFTER the recover still runs — the enclosing fn returns a plain str, so this only works
    // if `?` did NOT exit the function.
    "fn d(b: int) -> int!:\n    if b == 0:\n        return Err(\"zero\")\n    return Ok(10 / b)\nfn use() -> str:\n    r := recover:\n        x := d(0)?\n        x + 1\n    match r:\n        Ok(v): return \"ok\"\n        Err(e): return \"caught {e.message()}\"\nfn main():\n    print(use())\nmain()",
    // `?` Ok path inside recover: value unwrapped, trailing expression becomes the Ok result
    "fn d(b: int) -> int!:\n    return Ok(10 / b)\nfn main():\n    r := recover:\n        x := d(2)?\n        x + 1\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(e.message())\nmain()",
    // side effects before a caught fault PERSIST (keep semantics) — both engines must agree
    "fn main():\n    x := 1\n    r := recover:\n        x = 99\n        [1][9]\n    match r:\n        Ok(v): print(\"ok\")\n        Err(e): print(\"recovered\")\n    print(\"x={x}\")\nmain()",
    // nested recover: the inner boundary catches, the outer sees a normal value
    "fn main():\n    r := recover:\n        inner := recover:\n            [1][9]\n        match inner:\n            Ok(v): v\n            Err(e): 0\n    match r:\n        Ok(v): print(\"outer ok {v}\")\n        Err(e): print(\"outer err\")\nmain()",
    // recovered value composes with `?` after the boundary\n
    "fn run() -> int!:\n    r := recover:\n        [10, 20][0]\n    v := r?\n    return Ok(v + 1)\nfn main():\n    match run():\n        Ok(v): print(v)\n        Err(e): print(e.message())\nmain()",
    "print(\"a,b,c\".split(\",\"))\nprint(\",\".join([\"a\", \"b\", \"c\"]))",
    "print(\"abc\".starts_with(\"ab\"))\nprint(\"abc\".starts_with(\"z\"))\nprint(\"abc\".contains(\"b\"))\nprint(\"abc\".contains(\"q\"))",
    // chained core-type methods
    "print(\"  Hello,World  \".trim().lower().split(\",\"))",
    // ----- M6: core-type methods (list) -----
    "fn main():\n    xs := [1, 2]\n    xs.push(3)\n    xs.push(4)\n    print(xs)\n    print(xs.len())\nmain()",
    // ----- M6: pipe operator -----
    "fn inc(n: int) -> int: n + 1\nfn dbl(n: int) -> int: n * 2\nfn main():\n    print(5 |> inc() |> dbl())\nmain()",
    "fn shout(s: str) -> str: s.upper()\nfn main():\n    print(\"hi\" |> shout())\nmain()",
    // ----- error parity -----
    "print(1 / 0)",
    "print([1, 2][9])",
    "print(1 + \"x\")",
    "fn loop(n: int) -> int:\n    return loop(n + 1)\nfn main():\n    print(loop(0))\nmain()",
    // M6 method error parity
    "print(\"hi\".upper(\"extra\"))",
    "print(\"hi\".frobnicate())",
    "print(\",\".join([1, 2]))",
    "print((5).upper())",
    // arg-eval order: a bad method/receiver with an erroring arg must report the SAME error on
    // both engines — the VM evaluates args (operands) before the call, so the interp must too.
    "print((5).frob(1 / 0))",
    "print(\"hi\".frob(1 / 0))",
    // ----- entry model: no auto-main; unhandled top-level Err/None exits -----
    "fn main():\n    print(\"hi\")", // main defined but never called → no output
    "Err(\"boom\")",                 // bare top-level Err → unhandled error
    "x := Err(\"oops\")?",           // top-level `?` Err → unhandled error
    "fn g() -> Option[int]:\n    return None\ng()", // bare None → unhandled error
    "fn f() -> Result[int]:\n    return Err(\"x\")\nr := f()\nprint(\"handled\")", // Err bound = handled → no exit
    "fn main():\n    print(\"before\")\n    x := Err(\"boom\")?\n    print(\"after\")\nmain()", // partial output then exit
    // a user enum shadowing `Err` is a normal value: bare one must NOT exit, `?` must reject it
    "enum Signal:\n    Err(int)\n    Quiet\nErr(5)\nprint(\"made it\")",
    "enum Signal:\n    Err(int)\n    Quiet\nfn f() -> int:\n    x := Err(5)?\n    return x\nf()",
    // unhandled top-level error INSIDE a top-level block (interp: call_depth 0, VM: is_toplevel)
    "if true:\n    Err(\"boom\")\nprint(\"after\")", // bare Err in `if` → exit, no "after"
    "for i in 0..1:\n    Err(\"x\")\nprint(\"after\")", // bare Err in `for` → exit
    "fn d() -> Result[int]:\n    return Err(\"z\")\nif true:\n    x := d()?\n    print(x)", // top-level `?` in block → exit (same span both engines)
];

#[test]
fn parity_full_suite_vm_vs_interp() {
    for src in PROGRAMS {
        assert_parity(src);
    }
}

#[test]
fn parity_index_assign() {
    assert_parity("xs := [1, 2, 3]\nxs[1] = 9\nxs[0] += 4\nxs[2] -= 1\nprint(xs)\n");
}

#[test]
fn parity_index_assign_out_of_bounds() {
    assert_parity("xs := [1, 2, 3]\nxs[9] = 0\nprint(xs)\n");
}

#[test]
fn parity_compound_index_oob_vs_rhs_error_order() {
    // Compound `xs[i] += rhs` on an out-of-bounds `i` where `rhs` ALSO errors: both engines
    // must agree on which error wins. The VM reads the target (bounds-check) before `rhs`;
    // the interp must do the same.
    assert_parity("xs := [1, 2, 3]\nz := 0\nxs[5] += 1 / z\n");
}

#[test]
fn parity_compound_index_oob_skips_rhs_side_effect() {
    // On an out-of-bounds compound assign, neither engine should run the rhs side effect.
    assert_parity(
        "fn side() -> int:\n    print(\"rhs ran\")\n    return 0\nxs := [1, 2, 3]\nxs[5] += side()\nprint(\"after\")\n",
    );
}

#[test]
fn parity_field_assign() {
    assert_parity(
        "struct P:\n    x: int\n    y: int\np := P(1, 2)\np.x = 9\np.y += 3\nprint(p.x)\nprint(p.y)\n",
    );
}

// NOTE: method_type_params / param_protocol / method_default_args were parity-only; they are
// now full golden tests (`golden_*_chz_matches_expected_and_interp` above), which assert exact
// output AND cross-engine parity, so the weaker `parity_*` file tests were removed.

#[test]
fn parity_hof_param() {
    let src = "fn apply(f: fn(int) -> int, v: int) -> int:\n    return f(v)\ninc := fn(x: int) -> int: x + 1\nprint(apply(inc, 4))\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "5\n");
}

#[test]
fn parity_list_pop_some() {
    let src = "xs := [1,2,3]\nx := xs.pop()\nmatch x:\n    Some(v): print(\"got {v}\")\n    None: print(\"empty\")\nprint(xs.len())\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "got 3\n2\n");
}

#[test]
fn parity_list_pop_empty_none() {
    let src = "xs := [1]\na := xs.pop()\nb := xs.pop()\nmatch b:\n    Some(v): print(\"v\")\n    None: print(\"none\")\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "none\n");
}

#[test]
fn parity_list_reverse() {
    let src = "xs := [3,1,2]\nxs.reverse()\nprint(xs[0])\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "2\n");
}

#[test]
fn parity_list_contains() {
    let src = "print([1,2,3].contains(2))\nprint([1,2,3].contains(9))\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "true\nfalse\n");
}

#[test]
fn parity_list_index_of() {
    let src = "print([10,20,30].index_of(20))\nprint([1,2].index_of(9))\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "1\n-1\n");
}

#[test]
fn parity_list_sum() {
    let src = "print([1,2,3,4].sum())\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "10\n");
}

/// Integer `sum()` must use checked arithmetic and raise the SAME recoverable
/// "integer overflow in Add" runtime error as `+`, not silently wrap. Runs both engines.
#[test]
fn parity_list_sum_overflow() {
    let src = "print([9223372036854775807, 1].sum())\n";
    assert_parity(src);
    let err = vm_outcome(src).expect_err("expected a runtime error, not wraparound");
    assert!(
        err.contains("integer overflow in Add"),
        "unexpected error: {err}"
    );
}

/// A list containing any float takes the float path — would-overflow ints must NOT fault and
/// both engines must agree (guards against the int checked_add being hoisted above any_float).
#[test]
fn parity_list_sum_mixed_float() {
    let src = "print([9223372036854775807, 1, 0.0].sum())\n";
    assert_parity(src);
}

#[test]
fn parity_list_sort_int() {
    let src = "xs := [3,1,2]\nxs.sort()\nprint(xs[0])\nprint(xs[2])\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "1\n3\n");
}

#[test]
fn parity_list_sort_str() {
    let src = "xs := [\"banana\",\"apple\",\"cherry\"]\nxs.sort()\nfor s in xs:\n    print(s)\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "apple\nbanana\ncherry\n");
}

#[test]
fn parity_list_sort_float() {
    let src = "xs := [3.5, 1.1, 2.2]\nxs.sort()\nprint(xs[0])\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "1.1\n");
}

// ===== higher-order list methods: map / filter / fold =====
//
// These call a closure per element. On the VM each closure runs nested frames that can GC at
// instruction boundaries, so the source/result lists (and fold's accumulator) must stay rooted.
// Several tests use HEAP elements (strings / nested lists) and run under `gc_stress` so that a
// collection actually happens mid-iteration — if rooting is wrong they crash with a dangling ref.

#[test]
fn parity_list_map_int() {
    let src = "xs := [1,2,3]\nys := xs.map(fn(x: int) -> int: x * 2)\nprint(ys)\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "[2, 4, 6]\n");
}

#[test]
fn parity_list_map_to_str_gc_stress() {
    // Each element maps to a freshly-allocated string (heap), so collection mid-map matters.
    let src =
        "xs := [1,2,3]\nys := xs.map(fn(x: int) -> str: \"n{x}\")\nfor y in ys:\n    print(y)\n";
    assert_parity(src);
    let expected = "n1\nn2\nn3\n";
    assert_eq!(vm_outcome(src).unwrap(), expected);
    assert_eq!(
        run_capture_stress(src),
        expected,
        "VM gc_stress diverged (rooting bug?)"
    );
}

#[test]
fn parity_list_map_to_nested_list_gc_stress() {
    // Maps each element to a nested list (heap); the result list holds heap children.
    let src = "xs := [1,2,3]\nys := xs.map(fn(x: int) -> List[int]: [x, x])\nprint(ys[1][0])\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "2\n");
    assert_eq!(
        run_capture_stress(src),
        "2\n",
        "VM gc_stress diverged (rooting bug?)"
    );
}

#[test]
fn parity_list_filter_gc_stress() {
    // Filter over string elements; kept elements are heap objects pushed into the result.
    let src = "xs := [\"a\",\"bb\",\"ccc\",\"d\"]\nys := xs.filter(fn(x: str) -> bool: x.len() > 1)\nprint(ys.len())\nprint(ys[0])\n";
    assert_parity(src);
    let expected = "2\nbb\n";
    assert_eq!(vm_outcome(src).unwrap(), expected);
    assert_eq!(
        run_capture_stress(src),
        expected,
        "VM gc_stress diverged (rooting bug?)"
    );
}

#[test]
fn parity_list_filter_int() {
    let src = "xs := [1,2,3,4]\nys := xs.filter(fn(x: int) -> bool: x % 2 == 0)\nprint(ys.len())\nprint(ys[0])\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "2\n2\n");
}

#[test]
fn parity_list_fold_str_acc_gc_stress() {
    // Fold building a string accumulator (heap) — each step allocates a new acc string, so the
    // rooted accumulator slot must survive the next element's closure call.
    let src = "xs := [\"a\",\"b\",\"c\"]\ns := xs.fold(\"\", fn(a: str, x: str) -> str: a + x)\nprint(s)\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "abc\n");
    assert_eq!(
        run_capture_stress(src),
        "abc\n",
        "VM gc_stress diverged (rooting bug?)"
    );
}

#[test]
fn parity_list_sort_by_str_gc_stress() {
    // Sort heap-string elements by length; the comparator re-enters the VM and a collection can
    // fire mid-sort. The source list must stay rooted (we permute indices, not raw Values).
    let src = "xs := [\"ccc\",\"a\",\"dd\",\"b\"]\nxs.sort_by(fn(a: str, b: str) -> int: a.len() - b.len())\nfor x in xs:\n    print(x)\n";
    assert_parity(src);
    let expected = "a\nb\ndd\nccc\n";
    assert_eq!(vm_outcome(src).unwrap(), expected);
    assert_eq!(
        run_capture_stress(src),
        expected,
        "VM gc_stress diverged (rooting bug?)"
    );
}

#[test]
fn parity_list_sort_by_nested_list_gc_stress() {
    // Elements are nested lists (heap); sort by first element. Exercises rooting of heap children
    // across comparator calls under stress.
    let src = "xs := [[3,0],[1,0],[2,0]]\nxs.sort_by(fn(a: List[int], b: List[int]) -> int: a[0] - b[0])\nprint(xs[0][0])\nprint(xs[2][0])\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "1\n3\n");
    assert_eq!(
        run_capture_stress(src),
        "1\n3\n",
        "VM gc_stress diverged (rooting bug?)"
    );
}

#[test]
fn parity_list_fold_sum() {
    let src = "print([1,2,3,4].fold(0, fn(a: int, x: int) -> int: a + x))\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "10\n");
}

#[test]
fn parity_map_closure_free_generic_call() {
    // Bug D: `xs.map(fn(x): ident(x))` where `ident[T](x: T) -> T` type-checks to List[int] (the
    // closure-return loop-back recovers `map`'s `U` from the nested free generic call). Runtime is
    // generic-erased, so both engines print the same `2`.
    let src = "fn ident[T](x: T) -> T:\n    return x\nfn main():\n    xs := [1, 2, 3]\n    ys := xs.map(fn(x): ident(x))\n    print(ys[0] + 1)\nmain()\n";
    assert_parity(src);
    assert_eq!(vm_outcome(src).unwrap(), "2\n");
}

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

/// Run a file through both engines and assert identical (stdout, error).
fn assert_file_parity(rel: &str) {
    let path = fixture(rel);
    let (vm_out, vm_err, vm_res, _) = run_file(&path);
    let (ip_out, ip_err, ip_res, _) = run_file_p(&path);
    assert_eq!(vm_out, ip_out, "stdout divergence for {rel}");
    assert_eq!(vm_err, ip_err, "stderr divergence for {rel}");
    assert_eq!(
        vm_res.err().map(|e| e.to_string()),
        ip_res.err().map(|e| e.to_string()),
        "error divergence for {rel}"
    );
}

#[test]
fn golden_hello_via_run_file() {
    let path = fixture("examples/hello.chz");
    let expected = std::fs::read_to_string(fixture("examples/hello.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok());
    assert_eq!(out, expected);
}

/// M6 golden: core-type methods + pipe run end-to-end on the VM and byte-match the interp.
#[test]
fn golden_methods_via_run_file() {
    let path = fixture("examples/methods.chz");
    let expected = std::fs::read_to_string(fixture("examples/methods.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/methods.chz");
}

/// Golden: in-place index & field assignment run end-to-end on the VM and byte-match the interp.
#[test]
fn golden_mutate_via_run_file() {
    let path = fixture("examples/mutate.chz");
    let expected = std::fs::read_to_string(fixture("examples/mutate.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/mutate.chz");
}

/// `examples/collection_ops.chz` — golden coverage of every collection operator (gap #3): list
/// `+`/`*` and set `| & - ^` (multi-element, empty, type-correct mixes). Uses `std.io`, so it
/// runs through `run_file` (module resolution) and asserts two-engine parity via
/// `assert_file_parity` (VM==interp==expected).
#[test]
fn golden_collection_ops_via_run_file() {
    let path = fixture("examples/collection_ops.chz");
    let expected = std::fs::read_to_string(fixture("examples/collection_ops.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/collection_ops.chz");
}

/// M6c golden: the std-library demo (native std.io/math/os + Chezzi std.str) runs end-to-end on
/// the VM and byte-matches both the `.expected` file and the interpreter.
#[test]
fn golden_std_demo_via_run_file() {
    let path = fixture("examples/std_demo.chz");
    let expected = std::fs::read_to_string(fixture("examples/std_demo.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/std_demo.chz");
}

/// Additive std.math trig/exp/log intrinsics run end-to-end on the VM and byte-match both the
/// `.expected` file and the interpreter (parity via `assert_file_parity`).
#[test]
fn golden_math_more_via_run_file() {
    let path = fixture("examples/math_more.chz");
    let expected = std::fs::read_to_string(fixture("examples/math_more.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/math_more.chz");
}

/// `Channel.trip()` — the manual level-trigger latch (behind `std.cancel`'s `done()`). Tripping
/// makes the channel permanently ready (`recv`/`try_recv` yield `true`, fanning out). Deterministic
/// on every engine → golden + parity.
#[test]
fn golden_channel_trip_via_run_file() {
    let path = fixture("examples/channel_trip.chz");
    let expected = std::fs::read_to_string(fixture("examples/channel_trip.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/channel_trip.chz");
}

/// `std.cancel` — a manual token: `cancelled()`/`reason()` before/after `cancel()`, `done()`
/// readiness, idempotent re-cancel. Pure `Shared`/latch, no timing → golden + parity on every engine.
#[test]
fn golden_cancel_manual_via_run_file() {
    let path = fixture("examples/cancel_manual.chz");
    let expected = std::fs::read_to_string(fixture("examples/cancel_manual.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/cancel_manual.chz");
}

/// `std.cancel` — timeouts + `wait:` integration (`done()` as a wait arm). All cases deterministic
/// (pre-loaded channels / already-elapsed deadline) → golden + parity.
#[test]
fn golden_cancel_timeout_wait_via_run_file() {
    let path = fixture("examples/cancel_timeout_wait.chz");
    let expected =
        std::fs::read_to_string(fixture("examples/cancel_timeout_wait.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/cancel_timeout_wait.chz");
}

/// `std.cancel` — cooperative CPU-loop cancellation DIVERGES by engine, so it has no `.expected`
/// (cf. `examples/parallel_cancel.chz`). A sibling's manual `cancel()` aborts the polling worker
/// early on the default OS-thread engine (preemption); the single-threaded cooperative oracle runs
/// the worker to completion first (the canceller only runs at the sequential join). Asserts both.
#[test]
fn cancel_cpu_diverges_by_engine() {
    let path = fixture("examples/cancel_cpu.chz");
    let (par_out, _e, par_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    assert!(par_res.is_ok(), "{par_res:?}");
    assert_eq!(
        par_out, "worker aborted early\n",
        "default OS-thread engine should preempt + abort"
    );
    let (coop_out, _e, coop_res, _) = run_file(&path);
    assert!(coop_res.is_ok(), "{coop_res:?}");
    assert_eq!(
        coop_out, "worker ran to completion\n",
        "cooperative oracle should run to completion"
    );
}

// ===== std.cancel TREE PROPAGATION (parent/child derivation) =====
//
// A child token derived from a parent observes the parent's cancellation/timeout transitively
// (root-to-leaves, one-directional). The link is LIVE (Shared flag + Shared kids registry cross
// the airlock as live cores), so a parent flip is seen by already-derived children — including
// children that crossed `spawn`/`parallel:`. Cancelling a child never touches the parent.
//
// Each test runs an inline Chezzi snippet through the real module graph (`import std.cancel`
// needs `run_file`, not the standalone compile path). The `print`s are the assertion surface.

/// Wrap a `main()` body in `import std.cancel` + run it through the module graph, return stdout.
fn run_cancel_snippet(tag: &str, body: &str) -> String {
    let src = format!("import std.cancel\n\nfn main():\n{body}\nmain()\n");
    run_cancel_src(tag, &src)
}

/// Run a full `std.cancel`-importing source through the module graph, return stdout.
fn run_cancel_src(tag: &str, src: &str) -> String {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let entry = std::env::temp_dir().join(format!("chezzi_{tag}_{}_{seq}.chz", std::process::id()));
    std::fs::write(&entry, src).expect("write temp .chz");
    let (out, _err, res, _) = run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "snippet faulted: {res:?}");
    out
}

/// A derived child observes a manual parent cancel (transitive, live link). Child starts live.
#[test]
fn cancel_child_polls_parent_cancel() {
    let out = run_cancel_snippet(
        "cancel_child_polls_parent",
        "    p := cancel.manual()\n    c := p.derive()\n    print(\"c before: {c.cancelled()}\")\n    p.cancel()\n    print(\"c after: {c.cancelled()}\")\n    match c.reason():\n        Some(r): print(\"c reason: {r}\")\n        None:    print(\"c reason: none\")\n",
    );
    assert_eq!(out, "c before: false\nc after: true\nc reason: cancelled\n");
}

/// Cancelling a child is one-directional: the parent is untouched (flag + reason stay clear).
#[test]
fn cancel_child_cancel_does_not_touch_parent() {
    let out = run_cancel_snippet(
        "cancel_child_one_directional",
        "    p := cancel.manual()\n    c := p.derive()\n    c.cancel()\n    print(\"c: {c.cancelled()}\")\n    print(\"p: {p.cancelled()}\")\n    match p.reason():\n        Some(r): print(\"p reason: {r}\")\n        None:    print(\"p reason: none\")\n",
    );
    assert_eq!(out, "c: true\np: false\np reason: none\n");
}

/// Cascade is transitive: cancelling the root cancels a grandchild with the inherited reason.
#[test]
fn cancel_transitive_grandchild() {
    let out = run_cancel_snippet(
        "cancel_transitive_grandchild",
        "    p := cancel.manual()\n    c := p.derive()\n    g := c.derive()\n    p.cancel()\n    print(\"g: {g.cancelled()}\")\n    match g.reason():\n        Some(r): print(\"g reason: {r}\")\n        None:    print(\"g reason: none\")\n",
    );
    assert_eq!(out, "g: true\ng reason: cancelled\n");
}

/// A manual parent cancel makes a derived child's `done()` channel ready (registry fan-out).
#[test]
fn cancel_child_done_ready_after_parent_cancel() {
    let out = run_cancel_snippet(
        "cancel_child_done_fanout",
        "    p := cancel.manual()\n    c := p.derive()\n    p.cancel()\n    match c.done().try_recv():\n        Some(v): print(\"done: {v}\")\n        None:    print(\"not done\")\n",
    );
    assert_eq!(out, "done: true\n");
}

/// `done()` cascades TRANSITIVELY: a manual GRANDPARENT cancel makes a grandchild's `done()`
/// channel ready, not just `cancelled()`. The grandchild's done-channel is registered into every
/// ancestor's `kids`, so the grandparent's fan-out reaches it directly (a `wait:` waiter wakes).
#[test]
fn cancel_grandchild_done_ready_after_grandparent_cancel() {
    let out = run_cancel_snippet(
        "cancel_grandchild_done_fanout",
        "    gp := cancel.manual()\n    mid := gp.derive()\n    leaf := mid.derive()\n    gp.cancel()\n    match leaf.done().try_recv():\n        Some(v): print(\"done: {v}\")\n        None:    print(\"not done\")\n",
    );
    assert_eq!(out, "done: true\n");
}

/// `done()` cascade reaches any depth: a manual ROOT cancel makes a great-grandchild's (depth 3)
/// `done()` channel ready, proving the per-ancestor registration walks the whole chain to the root.
#[test]
fn cancel_great_grandchild_done_ready_after_root_cancel() {
    let out = run_cancel_snippet(
        "cancel_ggchild_done_fanout",
        "    root := cancel.manual()\n    a := root.derive()\n    b := a.derive()\n    leaf := b.derive()\n    root.cancel()\n    match leaf.done().try_recv():\n        Some(v): print(\"done: {v}\")\n        None:    print(\"not done\")\n",
    );
    assert_eq!(out, "done: true\n");
}

/// A child of an already-elapsed timeout parent inherits the tightest deadline: it is cancelled
/// at once, its reason is "timeout", its done() is ready, and its deadline equals the parent's.
#[test]
fn cancel_child_inherits_tightest_deadline() {
    let out = run_cancel_snippet(
        "cancel_child_tightest_deadline",
        "    p := cancel.timeout(0)\n    c := p.derive()\n    print(\"c cancelled: {c.cancelled()}\")\n    match c.reason():\n        Some(r): print(\"c reason: {r}\")\n        None:    print(\"c reason: none\")\n    match c.done().try_recv():\n        Some(v): print(\"c done: {v}\")\n        None:    print(\"c not done\")\n    print(\"deadline match: {c.deadline_at() == p.deadline_at()}\")\n",
    );
    assert_eq!(
        out,
        "c cancelled: true\nc reason: timeout\nc done: true\ndeadline match: true\n"
    );
}

/// The live link survives the concurrency airlock: a derived child crosses `spawn`/`parallel:`,
/// and a parent cancel done in the parent task BEFORE the nursery is observed by the spawned task
/// (Shared cores cross as LIVE handles, not snapshots). Deterministic on all engines: the parent
/// already cancelled before the spawn, so the worker only polls the already-flipped flag.
#[test]
fn cancel_token_sendable_with_parent() {
    let src = "import std.cancel\n\nfn watch(c: Token, seen: Shared[bool]):\n    if c.cancelled():\n        seen.set(true)\n\nfn main():\n    p := cancel.manual()\n    c := p.derive()\n    seen := Shared(false)\n    p.cancel()\n    parallel:\n        spawn watch(c, seen)\n    print(\"observed cancel: {seen.get()}\")\n\nmain()\n";
    let out = run_cancel_src("cancel_token_sendable_parent", src);
    assert_eq!(out, "observed cancel: true\n");
}

/// The free-function form `cancel.derive(parent)` is equivalent to `parent.derive()`.
#[test]
fn cancel_derive_free_fn_form() {
    let out = run_cancel_snippet(
        "cancel_derive_free_fn",
        "    p := cancel.manual()\n    c := cancel.derive(p)\n    print(\"before: {c.cancelled()}\")\n    p.cancel()\n    print(\"after: {c.cancelled()}\")\n",
    );
    assert_eq!(out, "before: false\nafter: true\n");
}

/// `std.cancel` tree-propagation golden (VM): `examples/cancel_tree.chz` byte-matches `.expected`
/// and stays identical to the interpreter (`assert_file_parity`). Deterministic (manual cancel +
/// pre-ready/zero timers, same task) → golden on every engine.
#[test]
fn golden_cancel_tree_via_run_file() {
    let path = fixture("examples/cancel_tree.chz");
    let expected = std::fs::read_to_string(fixture("examples/cancel_tree.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/cancel_tree.chz");
}

/// C-ABI FFI golden (VM twin of `interp::golden_ffi_chz`): the `extern "lib":` block calls
/// `cos`/`sqrt` (libm) and `strlen` (libc) via dlopen+libffi on the VM, byte-matches `.expected`,
/// and stays identical to the interpreter (`assert_file_parity`). Linux-only (needs libm/libc).
#[test]
#[cfg(target_os = "linux")]
fn golden_ffi_chz_via_run_file() {
    let path = fixture("examples/ffi.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ffi.chz");
}

/// C-ABI opaque `ptr` handle golden (VM twin of `interp::golden_ffi_ptr_chz`): the `extern "lib":`
/// block opens a libc `FILE*` (`fopen -> ptr`), hands the handle back to `fclose`, and detects a
/// NULL handle via `std.ffi.is_null` / `== std.ffi.null()`. Byte-matches `.expected` and stays
/// identical to the interpreter (`assert_file_parity`). Linux-only (needs libc).
#[test]
#[cfg(target_os = "linux")]
fn golden_ffi_ptr_chz_via_run_file() {
    let path = fixture("examples/ffi_ptr.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi_ptr.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ffi_ptr.chz");
}

/// C struct BY VALUE golden (VM twin of `interp::golden_ffi_struct_chz`): the `extern "lib":`
/// block binds `div_t div(int, int)` — a libc fn taking two scalars and returning a small POD
/// struct BY VALUE — to a Chezzi `struct DivT{quot, rem}`. `div(17, 5) == {3, 2}` (pure, always
/// present). Byte-matches `.expected` and stays identical to the interpreter (`assert_file_parity`
/// runs the serial VM + interp). The `--parallel`/M:N engine is NOT driven here; the returned
/// `NativeRet::Struct` already crosses the M:N airlock by the same deep-copy std.regex uses, so the
/// path is exercised, just not by this golden. Linux-only.
#[test]
#[cfg(target_os = "linux")]
fn golden_ffi_struct_chz_via_run_file() {
    let path = fixture("examples/ffi_struct.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi_struct.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ffi_struct.chz");
}

/// CAPSTONE C-buffer FFI golden (VM twin of `interp::golden_ffi_qsort_chz`): sort a Chezzi list
/// with libc `qsort`, composing `ffi.alloc` + `store_int64_at` + a Chezzi `qsort` comparator
/// callback (`load_int64` both `const void*` sides) + `load_int64_at` read-back + `defer ffi.free`.
/// The marquee proof that callbacks + deref + alloc all compose. Byte-matches `.expected` and stays
/// identical to the interpreter (`assert_file_parity` runs the serial VM + interp). Linux-only
/// (needs libc); the fixed 8-byte int64 stride matches the comparator on every LP64 unix.
#[test]
#[cfg(target_os = "linux")]
fn golden_ffi_qsort_chz_via_run_file() {
    let path = fixture("examples/ffi_qsort.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi_qsort.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ffi_qsort.chz");
    // Also exercise the M:N `--parallel` engine: the qsort comparator re-enters the VM as a
    // libffi callback under worker pinning — the highest-risk callback+alloc composition, and
    // the one path the cooperative-VM + interp parity above does not cover. Keeps the engine
    // matrix honest (the sibling deref test runs `--parallel` too).
    let (par_out, _par_err, par_res, _) =
        run_file_parallel(&path, crate::native::HostConfig::default());
    assert!(par_res.is_ok(), "{par_res:?}");
    assert_eq!(
        par_out, expected,
        "M:N --parallel diverged for ffi_qsort.chz"
    );
}

/// C-ABI deeper `str` returns golden (VM twin of `interp::golden_ffi_str_chz`): `strdup -> owned_str`
/// (owned `char*` copied into a Chezzi `str` AND freed) and `getenv -> str?` (NULL → `None`). Byte-
/// matches `.expected` and stays identical to the interpreter (`assert_file_parity`). Linux-only.
#[test]
#[cfg(target_os = "linux")]
fn golden_ffi_str_chz_via_run_file() {
    let path = fixture("examples/ffi_str.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi_str.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ffi_str.chz");
}

/// C-ABI fixed-width integer marshalling golden (VM twin of `interp::golden_ffi_int_chz`): `atoi
/// -> int32` (sign-extend), `htonl(uint32) -> uint32` (zero-extend, high-bit positive), `abs(int8)
/// -> int8` (signed round-trip + param truncation per a C cast). Byte-matches `.expected` and stays
/// identical to the interpreter (`assert_file_parity`). Linux-only (needs libc).
// The example's `htonl` lines encode little-endian oracles, so the golden is LE-gated.
#[test]
#[cfg(all(target_os = "linux", target_endian = "little"))]
fn golden_ffi_int_chz_via_run_file() {
    let path = fixture("examples/ffi_int.chz");
    let expected = std::fs::read_to_string(fixture("examples/ffi_int.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ffi_int.chz");
}

/// A complete self-contained program (merge sort + binary search + stats over std.math) runs on
/// the VM, byte-matches `.expected`, and stays identical to the interpreter.
#[test]
fn golden_overflow_via_run_file() {
    // The integer-overflow policy, end-to-end: every overflow (arith, neg, div, math.abs) is a
    // recoverable fault, identical on both engines.
    let path = fixture("examples/overflow.chz");
    let expected = std::fs::read_to_string(fixture("examples/overflow.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/overflow.chz");
}

#[test]
fn golden_stats_app_via_run_file() {
    let path = fixture("examples/stats.chz");
    let expected = std::fs::read_to_string(fixture("examples/stats.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/stats.chz");
}

/// G3 golden: `examples/stdlib_cmp.chz` — `import std.cmp`, generic `min`/`max`/`clamp` over
/// int/float/str/struct, and `list.sort()` over Comparable structs. Byte-matches `.expected`
/// and stays identical on interp + VM.
#[test]
fn golden_stdlib_cmp_via_run_file() {
    let path = fixture("examples/stdlib_cmp.chz");
    let expected = std::fs::read_to_string(fixture("examples/stdlib_cmp.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/stdlib_cmp.chz");
}

/// std.str helpers golden: `examples/str_more.chz` — the additive ends_with/index_of/count/
/// replace/strip_prefix/strip_suffix funcs, end-to-end on the VM, byte-identical to `.expected`
/// and the interpreter.
#[test]
fn golden_str_more_via_run_file() {
    let path = fixture("examples/str_more.chz");
    let expected = std::fs::read_to_string(fixture("examples/str_more.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/str_more.chz");
}

/// str receiver-method golden: `examples/str_methods.chz` — the gap #1 forwarder methods
/// (ends_with/replace/repeat/reverse/pad_left/index_of/count/strip_prefix/strip_suffix/
/// split_lines/strip) + gap #7 safe parse (to_int/to_float), end-to-end on the VM,
/// byte-identical to `.expected` and the interpreter.
#[test]
fn golden_str_methods_via_run_file() {
    let path = fixture("examples/str_methods.chz");
    let expected = std::fs::read_to_string(fixture("examples/str_methods.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/str_methods.chz");
}

/// std.iter helpers golden: `examples/iter_more.chz` — the additive take/drop/any/all/find/
/// flatten funcs, end-to-end on the VM, byte-identical to `.expected` and the interpreter.
#[test]
fn golden_iter_more_via_run_file() {
    let path = fixture("examples/iter_more.chz");
    let expected = std::fs::read_to_string(fixture("examples/iter_more.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/iter_more.chz");
}

/// std.rand goldens — run as ONE test so they execute sequentially. The PRNG is a process-global
/// (shared by VM + interp + every test in the run); two *separate* `#[test]`s seeding-then-drawing
/// would interleave on that global under the test harness's parallel runner and diverge. Drawn
/// sequentially within one test body, each program's `rand.seed()`-then-draw stream is
/// deterministic and byte-identical across engines (`assert_file_parity` checks VM == interp).
/// (This is the same concurrent-draw limit documented in docs/stdlib.md §std.rand — the goldens
/// avoid it by serializing.)
///
/// - `rand_seeded`: all four scalar fns (seed/int/float/bool), seeded → deterministic values.
/// - `rand_shape`: UNSEEDED (OS-entropy path); asserts only range/shape, prints fixed "ok" lines
///   (value-independent, so byte-identical across engines despite the nondeterministic seed).
/// - `rand_iter`: std.iter `shuffle`/`choice`/`sample` (pure Chezzi over native `rand.int`).
#[test]
fn golden_rand_via_run_file() {
    // Serialize against the rand unit tests (shared process-global RNG); see TEST_RNG_LOCK.
    let _g = crate::native::rand::TEST_RNG_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for rel in [
        "examples/rand_seeded.chz",
        "examples/rand_shape.chz",
        "examples/rand_iter.chz",
    ] {
        let path = fixture(rel);
        let expected =
            std::fs::read_to_string(fixture(&format!("{}.expected", &rel[..rel.len() - 4])))
                .unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{rel}: {res:?}");
        assert_eq!(out, expected, "stdout mismatch for {rel}");
        assert_file_parity(rel);
    }
}

/// std.path golden: `examples/path.chz` — the pure-Chezzi unix path-STRING ops
/// (is_abs/is_rel/basename/dirname/split/ext/stem/with_ext/normalize/join), end-to-end on the
/// VM, byte-identical to `.expected` and the interpreter (assert_file_parity == VM==interp).
#[test]
fn golden_path_via_run_file() {
    let path = fixture("examples/path.chz");
    let expected = std::fs::read_to_string(fixture("examples/path.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/path.chz");
}

/// std.datetime golden: `examples/datetime.chz` — the pure-Chezzi civil-calendar module
/// (from_epoch/to_epoch/formatters/duration/leap/weekday on FIXED epochs only, no clock),
/// end-to-end on the VM, byte-identical to `.expected` and the interpreter (assert_file_parity).
#[test]
fn golden_datetime_via_run_file() {
    let path = fixture("examples/datetime.chz");
    let expected = std::fs::read_to_string(fixture("examples/datetime.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/datetime.chz");
}

/// std.concurrency.collection golden: `examples/concurrent_collection.chz` — the pure-Chezzi
/// thread-safe `ConcurrentMap`/`ConcurrentCounter` wrappers over `RwShared[Map[...]]`. The program
/// is DETERMINISTIC by construction (single-write-lock RMW for the counter, each-own-key for the
/// map), so its output is identical on the VM, the interpreter (assert_file_parity), and (verified
/// manually) `--parallel`.
#[test]
fn golden_concurrent_collection_via_run_file() {
    let path = fixture("examples/concurrent_collection.chz");
    let expected =
        std::fs::read_to_string(fixture("examples/concurrent_collection.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/concurrent_collection.chz");
}

/// std.collections golden: `examples/collections.chz` — the pure-Chezzi Heap/Deque/Counter
/// module (min/max heap + from_list, two-stack deque both ends, Counter most_common ties),
/// end-to-end on the VM, byte-identical to `.expected` and the interpreter (assert_file_parity).
#[test]
fn golden_collections_via_run_file() {
    let path = fixture("examples/collections.chz");
    let expected = std::fs::read_to_string(fixture("examples/collections.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/collections.chz");
}

/// M8-M5 golden: `examples/json_decode.chz` — type-directed `json.decode[T]` into struct /
/// typed map / list / scalar, with Option fields, extra-key tolerance, and an error case.
/// Byte-identical on interp + VM.
#[test]
fn golden_json_decode_via_run_file() {
    let path = fixture("examples/json_decode.chz");
    let expected = std::fs::read_to_string(fixture("examples/json_decode.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/json_decode.chz");
}

/// M8-M3 golden: `examples/sys.chz` — the native trio std.process/std.fs/std.time, end-to-end
/// on the VM, byte-identical to `.expected` and the interpreter (deterministic ops only).
#[test]
fn golden_sys_via_run_file() {
    let path = fixture("examples/sys.chz");
    let expected = std::fs::read_to_string(fixture("examples/sys.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/sys.chz");
}

/// std.process polish golden: `examples/process_polish.chz` — the structured `run`/`run_args`
/// forms. Proves (a) a non-zero exit is `Ok(ProcResult)` carrying both streams + code (stdout NOT
/// discarded), (b) `run_args` runs WITHOUT a shell so `$(...)`/`;`/`&&` are passed literally
/// (injection-safe), (c) a spawn failure is a catchable `Err`. Byte-identical on the VM + interp.
#[test]
fn golden_process_polish_via_run_file() {
    let path = fixture("examples/process_polish.chz");
    let expected = std::fs::read_to_string(fixture("examples/process_polish.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/process_polish.chz");
}

/// M8+ golden: `examples/fs_mutations.chz` — the std.fs mutation surface (mkdir/append/rename/
/// copy/remove_file/remove_dir) as a self-cleaning round-trip. The script writes into the
/// gitignored `examples/.fs_scratch` and tears it down; the test pre/post-removes the scratch so a
/// prior crash cannot poison the run, and asserts byte-identical stdout on the VM + interp.
#[test]
fn golden_fs_mutations_via_run_file() {
    // Serialize against the interp twin: both write the single fixed examples/.fs_scratch.
    let _g = crate::native::fs::FS_SCRATCH_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let scratch = fixture("examples/.fs_scratch");
    let _ = std::fs::remove_dir_all(&scratch); // guard against a stale dir from a crashed run
    let path = fixture("examples/fs_mutations.chz");
    let expected = std::fs::read_to_string(fixture("examples/fs_mutations.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/fs_mutations.chz");
    // The round-trip removes its own scratch; assert nothing leaked into the working tree.
    assert!(
        !scratch.exists(),
        "fs_mutations.chz left examples/.fs_scratch behind"
    );
    let _ = std::fs::remove_dir_all(&scratch);
}

/// std.encoding + std.crypto goldens — deterministic str<->str codecs / digests (no entropy), so
/// they need no lock and are byte-identical on VM + interp + `.expected`.
#[test]
fn golden_encoding_crypto_via_run_file() {
    for rel in ["examples/encoding.chz", "examples/crypto.chz"] {
        let path = fixture(rel);
        let expected =
            std::fs::read_to_string(fixture(&format!("{}.expected", &rel[..rel.len() - 4])))
                .unwrap();
        let (out, _err, res, _) = run_file(&path);
        assert!(res.is_ok(), "{rel}: {res:?}");
        assert_eq!(out, expected, "stdout mismatch for {rel}");
        assert_file_parity(rel);
    }
}

/// std.uuid golden — `examples/uuid_shape.chz` uses `uuid_seed(N)` so the v4() stream is
/// deterministic and byte-identical on VM + interp. The UUID RNG is a process-global shared by
/// VM + interp + every test; serialize against the uuid unit tests (see TEST_UUID_LOCK) so a
/// concurrent seeded draw can't interleave on the global and diverge (the documented `--parallel`
/// concurrent-draw limit, surfacing in the parallel test runner — same as std.rand's golden).
#[test]
fn golden_uuid_via_run_file() {
    let _g = crate::native::uuid::TEST_UUID_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let path = fixture("examples/uuid_shape.chz");
    let expected = std::fs::read_to_string(fixture("examples/uuid_shape.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/uuid_shape.chz");
}

/// Comprehensions golden: `examples/comprehensions.chz` — list/set/map comprehensions, a guard,
/// and a range source. Byte-matches `.expected` and stays identical on interp + VM.
#[test]
fn golden_comprehensions_via_run_file() {
    let path = fixture("examples/comprehensions.chz");
    let expected = std::fs::read_to_string(fixture("examples/comprehensions.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/comprehensions.chz");
}

/// Nested-clause comprehensions golden: `examples/comprehensions_nested.chz` — 2- and 3-clause
/// list comps, a guard after a non-final clause, a later clause referencing an earlier variable,
/// and nested set + map forms. Byte-matches `.expected` and stays identical on interp + VM.
#[test]
fn golden_comprehensions_nested_via_run_file() {
    let path = fixture("examples/comprehensions_nested.chz");
    let expected =
        std::fs::read_to_string(fixture("examples/comprehensions_nested.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/comprehensions_nested.chz");
}

/// Stateful-iterator comprehension golden: `examples/comprehension_iter_state.chz` — a
/// comprehension whose element/guard read a struct iterator's LIVE, per-`next()` state. The
/// iterator must be driven lazily (interleaved with the body), so this byte-matches `.expected`
/// and stays identical on interp + VM (regression for the eager-drain parity bug).
#[test]
fn golden_comprehension_iter_state_via_run_file() {
    let path = fixture("examples/comprehension_iter_state.chz");
    let expected =
        std::fs::read_to_string(fixture("examples/comprehension_iter_state.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/comprehension_iter_state.chz");
}

/// Radix-literal golden: `examples/hex.chz` — hex/binary/octal literals feeding bitwise +
/// arithmetic. Byte-matches `.expected` and stays identical on interp + VM.
#[test]
fn golden_hex_via_run_file() {
    let path = fixture("examples/hex.chz");
    let expected = std::fs::read_to_string(fixture("examples/hex.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/hex.chz");
}

/// List concat/extend + map merge/update golden: `examples/concat_merge.chz`. New-vs-mutate
/// semantics + arg-wins-on-key-clash. Byte-matches `.expected`, identical on interp + VM.
#[test]
fn golden_concat_merge_via_run_file() {
    let path = fixture("examples/concat_merge.chz");
    let expected = std::fs::read_to_string(fixture("examples/concat_merge.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/concat_merge.chz");
}

/// Tuple-destructuring `for` + `std.iter` golden: `examples/for_tuple.chz` — destructure a list
/// of tuples, one-var whole-tuple, triples, `enumerate`/`zip`, comprehension combo. Byte-matches
/// `.expected`, identical on interp + VM (the `IsMap` runtime split is exercised alongside maps).
#[test]
fn golden_for_tuple_via_run_file() {
    let path = fixture("examples/for_tuple.chz");
    let expected = std::fs::read_to_string(fixture("examples/for_tuple.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/for_tuple.chz");
}

/// Optional chaining + null-coalescing golden: `examples/optchain.chz` — `?.field`, `?.method()`,
/// `??`, chaining + None short-circuit. Desugared to `match`; byte-matches `.expected`, identical
/// on interp + VM.
#[test]
fn golden_optchain_via_run_file() {
    let path = fixture("examples/optchain.chz");
    let expected = std::fs::read_to_string(fixture("examples/optchain.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/optchain.chz");
}

/// Runtime stack trace: a faulting nested call reports the error line + the call chain (innermost
/// first) with each call's line. Asserted on the VM, and the interp must produce the IDENTICAL
/// formatted trace (frames carry the same call-site spans on both engines).
#[test]
fn stack_trace_reports_call_chain_on_both_engines() {
    let path = fixture("examples/stack_trace.chz");
    let (_out, _err, res, _) = run_file(&path);
    let e = res.expect_err("program should fault");
    assert_eq!(e.message, "division by zero");
    let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(names, vec!["divide", "compute", "main"]);
    // Call-site lines, innermost first.
    let lines: Vec<usize> = e.trace.iter().map(|f| f.span.line).collect();
    assert_eq!(lines, vec![15, 18, 20]);
    let vm_fmt = format_trace(&e.message, e.span, &e.trace);
    assert!(
        vm_fmt.contains("at divide (called at line 15"),
        "got: {vm_fmt}"
    );

    // Interp parity: identical formatted trace.
    let (_o, _er, ip_res, _) = run_file_p(&path);
    let ie = ip_res.expect_err("program should fault");
    let ip_fmt = format_trace(&ie.message, ie.span, &ie.trace);
    assert_eq!(vm_fmt, ip_fmt, "engines must produce the same stack trace");
}

/// Helper: write deep-infinite-recursion source to a temp file and return its path. The recursion
/// hits `MAX_CALL_DEPTH` → a `recursion limit exceeded` fault with a ~10_000-frame raw trace.
#[cfg(test)]
fn deep_recursion_fixture(tag: &str) -> std::path::PathBuf {
    let src =
        "fn rec(n: int) -> int:\n    return rec(n + 1)\nfn main():\n    print(rec(0))\nmain()\n";
    let dir = std::env::temp_dir().join("chezzi_deep_recursion_trace_test");
    std::fs::create_dir_all(&dir).unwrap();
    // Unique per-test filename so concurrently-running tests never truncate each other's source
    // mid-read (a shared path would race: one test reads an empty/partial file → no fault).
    let path = dir.join(format!("rec_{tag}.chz"));
    std::fs::write(&path, src).unwrap();
    path
}

/// Gap #8: an infinite-recursion fault must NOT print one frame per call (~10_001 lines). The
/// renderer collapses consecutive identical frames into a `× N` form and caps the printed trace,
/// so the output stays a couple dozen lines while the raw captured trace is still ~10_000 frames.
#[test]
fn recursion_trace_is_bounded_and_collapsed() {
    let path = deep_recursion_fixture("bounded");
    let (_out, _err, res, _) = run_file(&path);
    let e = res.expect_err("deep recursion should fault");
    // Raw trace really is huge.
    assert!(
        e.trace.len() > 1000,
        "expected a deep raw trace, got {}",
        e.trace.len()
    );
    let fmt = format_trace(&e.message, e.span, &e.trace);
    let nlines = fmt.lines().count();
    assert!(
        nlines < 30,
        "trace should be bounded, got {nlines} lines:\n{fmt}"
    );
    // Collapse marker present (the `× N more identical frames` run elision).
    assert!(
        fmt.contains("identical frames"),
        "expected a collapse marker, got:\n{fmt}"
    );
    // Innermost frame still visible.
    assert!(
        fmt.contains("at rec (called at"),
        "innermost frame must be shown, got:\n{fmt}"
    );
    // Outermost frame (main) still visible.
    assert!(
        fmt.contains("at main (called at"),
        "outermost frame must be shown, got:\n{fmt}"
    );
}

/// Gap #8 parity: the bounded/collapsed trace must be byte-identical across both engines.
#[test]
fn recursion_trace_parity_vm_vs_interp() {
    let path = deep_recursion_fixture("parity");
    let (_o, _e, res, _) = run_file(&path);
    let e = res.expect_err("deep recursion should fault");
    let vm_fmt = format_trace(&e.message, e.span, &e.trace);
    let (_o2, _e2, ip_res, _) = run_file_p(&path);
    let ie = ip_res.expect_err("deep recursion should fault");
    let ip_fmt = format_trace(&ie.message, ie.span, &ie.trace);
    assert_eq!(
        vm_fmt, ip_fmt,
        "engines must produce the same bounded/collapsed trace"
    );
}

/// Gap #8 cap path: a deep chain of DISTINCT-named frames (no collapse possible) is still bounded
/// by the head/tail cap with a `frames elided` marker, and stays byte-identical across engines.
#[test]
fn format_trace_caps_distinct_name_chain() {
    let span = Span { line: 1, col: 1 };
    let trace: Vec<TraceFrame> = (0..50)
        .map(|n| TraceFrame {
            function: format!("f{n}"),
            span,
        })
        .collect();
    let vm_fmt = format_trace("boom", span, &trace);
    let nlines = vm_fmt.lines().count();
    // 1 header + 10 head + 1 elision marker + 10 tail = 22.
    assert_eq!(nlines, 22, "got:\n{vm_fmt}");
    assert!(vm_fmt.contains("frames elided"), "got:\n{vm_fmt}");
    assert!(vm_fmt.contains("at f0 (called at"), "head, got:\n{vm_fmt}");
    assert!(vm_fmt.contains("at f49 (called at"), "tail, got:\n{vm_fmt}");
    // No collapse marker — all names distinct.
    assert!(!vm_fmt.contains("identical frames"), "got:\n{vm_fmt}");

    // Parity: interp renders the identical synthetic trace.
    let ip_trace: Vec<TraceFrame> = (0..50)
        .map(|n| TraceFrame {
            function: format!("f{n}"),
            span,
        })
        .collect();
    let ip_fmt = format_trace("boom", span, &ip_trace);
    assert_eq!(vm_fmt, ip_fmt, "engines must agree on the capped trace");
}

/// Gap #8 cap boundary: with many short same-name runs the collapse emits `× N` markers; the cap
/// must never split a marker from its `at` line (markers stay inside their entry). Every `× N`
/// line must be immediately preceded by its `at` line, head and tail alike, and engines agree.
#[test]
fn format_trace_cap_never_orphans_collapse_marker() {
    let span = Span { line: 1, col: 1 };
    // 25 names × 2 consecutive frames each → 25 collapsed entries (each an `at` line + `× 1`
    // marker), > TRACE_HEAD + TRACE_TAIL so the cap fires across marker-bearing entries.
    let trace: Vec<TraceFrame> = (0..25)
        .flat_map(|n| {
            let f = format!("g{n}");
            [
                TraceFrame {
                    function: f.clone(),
                    span,
                },
                TraceFrame { function: f, span },
            ]
        })
        .collect();
    let vm_fmt = format_trace("boom", span, &trace);
    let lines: Vec<&str> = vm_fmt.lines().collect();
    assert!(
        vm_fmt.contains("frames elided"),
        "cap must fire, got:\n{vm_fmt}"
    );
    for (i, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("… (×") {
            let prev = lines.get(i - 1).copied().unwrap_or("");
            assert!(
                prev.trim_start().starts_with("at "),
                "orphaned collapse marker at line {i}; prev={prev:?}\n{vm_fmt}"
            );
        }
    }
    // Parity with interp on the same synthetic trace.
    let ip_trace: Vec<TraceFrame> = (0..25)
        .flat_map(|n| {
            let f = format!("g{n}");
            [
                TraceFrame {
                    function: f.clone(),
                    span,
                },
                TraceFrame { function: f, span },
            ]
        })
        .collect();
    let ip_fmt = format_trace("boom", span, &ip_trace);
    assert_eq!(
        vm_fmt, ip_fmt,
        "engines must agree on the capped collapsed trace"
    );
}

/// A `recover:`-caught fault leaves no stale frames: a *later* uncaught fault's trace shows only
/// its own chain, not the recovered call.
#[test]
fn recovered_fault_does_not_pollute_later_trace() {
    let src = "fn boom() -> int:\n    return 1 / 0\nfn safe() -> int:\n    r := recover:\n        boom()\n    return 7\nfn deeper() -> int:\n    xs := [1, 2]\n    return xs[9]\nfn main():\n    print(safe())\n    print(deeper())\nmain()\n";
    let dir = std::env::temp_dir().join("chezzi_recover_trace_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rt.chz");
    std::fs::write(&path, src).unwrap();
    let (_o, _e, res, _) = run_file(&path);
    let e = res.expect_err("should fault");
    let names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(
        names,
        vec!["deeper", "main"],
        "no stale 'boom'/'safe' frames"
    );
}

/// A `defer`red call that itself faults supersedes the original fault (Go semantics); the trace
/// must reflect the DEFERRED fault's chain (deeper, includes the deferred fn), identically on
/// both engines — not the original body fault's chain.
#[test]
fn deferred_fault_trace_supersedes_on_both_engines() {
    let src = "fn boom() -> int:\n    return 1 / 0\nfn worker() -> int:\n    defer boom()\n    xs := [1, 2]\n    return xs[9]\nfn main():\n    print(worker())\nmain()\n";
    let dir = std::env::temp_dir().join("chezzi_defer_trace_test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dt.chz");
    std::fs::write(&path, src).unwrap();
    let (_o, _e, res, _) = run_file(&path);
    let e = res.expect_err("should fault");
    let vm_names: Vec<&str> = e.trace.iter().map(|f| f.function.as_str()).collect();
    assert_eq!(
        vm_names,
        vec!["boom", "worker", "main"],
        "deferred fault's chain"
    );
    let vm_fmt = format_trace(&e.message, e.span, &e.trace);
    let (_o2, _e2, ip_res, _) = run_file_p(&path);
    let ie = ip_res.expect_err("should fault");
    let ip_fmt = format_trace(&ie.message, ie.span, &ie.trace);
    assert_eq!(
        vm_fmt, ip_fmt,
        "engines must agree on a deferred-fault trace"
    );
}

/// Non-constant default golden: `examples/default_expr.chz` — defaults that are arithmetic on
/// literals, a global times a literal, and a function call (free fns + struct fields). Byte-matches
/// `.expected`, identical on interp + VM.
#[test]
fn golden_default_expr_via_run_file() {
    let path = fixture("examples/default_expr.chz");
    let expected = std::fs::read_to_string(fixture("examples/default_expr.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/default_expr.chz");
}

/// Function-typed field call golden: `examples/fn_field.chz` — `recv.f(args)` where `f` is a
/// `fn`-typed field resolves to field-access-then-call (on `self` and on an external receiver),
/// not a method. Byte-matches `.expected`, identical on interp + VM.
#[test]
fn golden_fn_field_via_run_file() {
    let path = fixture("examples/fn_field.chz");
    let expected = std::fs::read_to_string(fixture("examples/fn_field.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/fn_field.chz");
}

/// `sort_by_key` golden: `examples/sort_by_key.chz` — sort in place by a derived key (int/str
/// keys, stable, descending-via-negation, and a Comparable *struct* key). Byte-matches
/// `.expected`, identical on interp + VM.
#[test]
fn golden_sort_by_key_via_run_file() {
    let path = fixture("examples/sort_by_key.chz");
    let expected = std::fs::read_to_string(fixture("examples/sort_by_key.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/sort_by_key.chz");
}

/// `Ref[T]` golden: `examples/ref.chz` — a pure-Chezzi one-field mutable box (`std.ref`):
/// `get`/`set`/`update`, closure-capture accumulation through the shared struct, generic over a
/// non-int type. Byte-matches `.expected`, identical on interp + VM. No engine change.
#[test]
fn golden_ref_via_run_file() {
    let path = fixture("examples/ref.chz");
    let expected = std::fs::read_to_string(fixture("examples/ref.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ref.chz");
}

/// `Ref` reserved-global golden: `examples/ref_no_import.chz` exercises the `ref` keyword, the
/// explicit `Ref[int]`/`Ref(0)` box, and a closure that mutates a captured `ref` local through the
/// shared box — all with NO `import std.ref` (std.ref is always linked). Asserts all THREE engines
/// byte-identical: cooperative VM == interp (`assert_file_parity`) PLUS the M:N OS-thread engine
/// (`run_file_parallel`).
#[test]
fn golden_ref_no_import_via_run_file() {
    let path = fixture("examples/ref_no_import.chz");
    let expected = std::fs::read_to_string(fixture("examples/ref_no_import.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ref_no_import.chz");
    // M:N OS-thread engine (default `run`) must match byte-for-byte too.
    let (par_out, _par_err, par_res, _) =
        run_file_parallel(&path, crate::native::HostConfig::default());
    assert!(par_res.is_ok(), "{par_res:?}");
    assert_eq!(par_out, expected, "M:N engine divergence for ref_no_import");
}

/// `ref T` golden: `examples/ref_binding.chz` — the transparent by-reference binding modifier
/// (sugar over `std.ref` `Ref[T]`): create + read/write auto-deref, alias-shares-box, a plain
/// `:=` copy that does NOT share, pass-by-ref mutating the caller's binding, a `ref -> T` param
/// auto-deref copy, and inner-fn capture-by-ref persisting through the shared box. All lowering
/// lives in desugar, so the VM and interp are byte-identical by construction.
#[test]
fn golden_ref_binding_via_run_file() {
    let path = fixture("examples/ref_binding.chz");
    let expected = std::fs::read_to_string(fixture("examples/ref_binding.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ref_binding.chz");
}

/// `ref T` indirect-callee golden: `examples/ref_indirect.chz` — the type-directed arg coercion
/// (alias / deref / by receiver type) reached through a LOCAL fn-value, a closure, and a method
/// name shared across structs that disagree on ref-ness — including EXPRESSION receivers (an
/// inline ctor call and a struct-returning fn call), which resolve by receiver type identically
/// to a named local. All lower to the same `Ref[T]` box, so the VM and interp are byte-identical
/// by construction.
#[test]
fn golden_ref_indirect_via_run_file() {
    let path = fixture("examples/ref_indirect.chz");
    let expected = std::fs::read_to_string(fixture("examples/ref_indirect.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ref_indirect.chz");
}

/// `ref T` airlock golden: `examples/ref_airlock.chz` — the concurrency boundary (spec §7). A
/// `ref T` box is non-sendable, so its VALUE must be copied across the airlock; the child mutates
/// only its copy and the parent's binding is untouched. Byte-identical on interp + VM.
#[test]
fn golden_ref_airlock_via_run_file() {
    let path = fixture("examples/ref_airlock.chz");
    let expected = std::fs::read_to_string(fixture("examples/ref_airlock.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/ref_airlock.chz");
}

/// Tuple destructuring + match-on-tuple + guards golden: `examples/tuple_match.chz` — `a, b :=
/// fn()`, typed tuple value + `.0`/`.1`, `match` literal/binding/guard arms, `Some((a, b))`.
/// Coverage for behavior that already worked. Byte-matches `.expected`, identical on interp + VM.
#[test]
fn golden_tuple_match_via_run_file() {
    let path = fixture("examples/tuple_match.chz");
    let expected = std::fs::read_to_string(fixture("examples/tuple_match.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/tuple_match.chz");
}

/// `std.os.exit(code)` golden: `examples/exit.chz` halts at the negative branch with status 2.
/// Byte-matches `.expected` on both engines and both report the same exit code.
#[test]
fn golden_exit_via_run_file() {
    let path = fixture("examples/exit.chz");
    let expected = std::fs::read_to_string(fixture("examples/exit.expected")).unwrap();
    let (vo, _ve, vr, vc) = run_file(&path);
    let (io, _ie, ir, ic) = run_file_p(&path);
    assert!(
        vr.is_ok() && ir.is_ok(),
        "exit is a clean halt: vm={vr:?} interp={ir:?}"
    );
    assert_eq!(vo, expected, "vm stdout");
    assert_eq!(io, expected, "interp stdout");
    assert_eq!(vc, Some(2), "vm exit code");
    assert_eq!(ic, Some(2), "interp exit code");
}

/// M8-M2 golden: `examples/json_dynamic.chz` — `import std.json`, the pure-Chezzi `Json` enum
/// parse/stringify round-trip + accessors + unicode escapes + an error case. Byte-matches
/// `.expected` and stays identical on interp + VM.
#[test]
fn golden_json_dynamic_via_run_file() {
    let path = fixture("examples/json_dynamic.chz");
    let expected = std::fs::read_to_string(fixture("examples/json_dynamic.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/json_dynamic.chz");
}

/// M9 golden: `examples/regex_demo.chz` — `import std.regex` (is_match / find with capture
/// groups / find_all / replace_all / split + a bad-pattern Err). Byte-matches `.expected` and
/// stays identical on interp + VM.
#[test]
fn golden_regex_demo_via_run_file() {
    let path = fixture("examples/regex_demo.chz");
    let expected = std::fs::read_to_string(fixture("examples/regex_demo.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/regex_demo.chz");
}

/// Golden: `examples/knapsack.chz` fills an int DP table with `cmp.max` (std.cmp generic over
/// Comparable). Runs on the VM, byte-matches `.expected`, and stays identical to the interp.
#[test]
fn golden_knapsack_via_run_file() {
    let path = fixture("examples/knapsack.chz");
    let expected = std::fs::read_to_string(fixture("examples/knapsack.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/knapsack.chz");
}

/// `Iterator[T]` golden: a generic fn bounded `[S: Iterator[T], T]` over list/str/set/struct,
/// with the element type flowing into returns. Parity-checked across both engines.
#[test]
fn golden_iterator_bound_via_run_file() {
    let path = fixture("examples/iterator_bound.chz");
    let expected = std::fs::read_to_string(fixture("examples/iterator_bound.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/iterator_bound.chz");
}

/// Lazy iterator adapters (Take/Mapped over an infinite Count) — the no-`yield` story. The inner
/// `self.inner.next()` recovers the element type through the `I: Iterator[T]` bound on both engines.
#[test]
fn golden_iter_adapters_via_run_file() {
    let path = fixture("examples/iter_adapters.chz");
    let expected = std::fs::read_to_string(fixture("examples/iter_adapters.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/iter_adapters.chz");
}

/// `Iterable[T]` + `.iter()` — a list flows into the Take/Mapped adapter pipeline, `.iter()`+manual
/// `.next()` on every collection, a pure-`Iterable` struct drives `for`, an `[S: Iterable[T]]` fn
/// over a list AND a struct iterator, empty/idempotent cursors, `List(xs.iter())` round-trip. Byte-
/// identical on VM + interp (the `--serial` third engine is asserted in the regression script).
#[test]
fn golden_iterable_via_run_file() {
    let path = fixture("examples/iterable.chz");
    let expected = std::fs::read_to_string(fixture("examples/iterable.expected")).unwrap();
    let (out, _err, res, _) = run_file(&path);
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(out, expected);
    assert_file_parity("examples/iterable.chz");
}

#[test]
fn golden_multi_file_project_via_vm() {
    let expected = std::fs::read_to_string(fixture("tests/fixtures/proj/main.expected")).unwrap();
    let (out, _err, res, _) = run_file(&fixture("tests/fixtures/proj/main.chz"));
    assert!(res.is_ok());
    assert_eq!(out, expected);
    assert_file_parity("tests/fixtures/proj/main.chz");
}

/// The M4.5 headline bug, now on the VM: an imported function reading its module's top-level
/// constant must resolve against *its own* module, not the caller — even when the caller
/// defines a same-named global with a different value.
#[test]
fn imported_fn_uses_home_globals() {
    let (out, _err, res, _) = run_file(&fixture("tests/fixtures/homeglobals/main.chz"));
    assert!(res.is_ok());
    assert_eq!(out, "from-lib\nfrom-main\n");
    assert_file_parity("tests/fixtures/homeglobals/main.chz");
}

/// Whole multi-file project is byte-identical under GC stress.
#[test]
fn multi_file_identical_under_gc_stress() {
    // The fixture is small; run it under stress by routing through the entry graph manually.
    let expected = std::fs::read_to_string(fixture("tests/fixtures/proj/main.expected")).unwrap();
    let graph = crate::resolver::build_graph(&fixture("tests/fixtures/proj/main.chz")).unwrap();
    let program = crate::compiler::compile_graph(&graph).unwrap();
    let mut vm = Vm::new(Arc::new(program));
    vm.gc_stress = true;
    vm.run().unwrap();
    assert_eq!(vm.out, expected);
}

// ----- map / dictionary parity (gap #5) -----

#[test]
fn parity_map_literal_print() {
    // Deterministic insertion order; duplicate key -> last wins. Display is `{k: v, …}`.
    assert_parity_out("m := {\"a\": 1, \"b\": 2}\nprint(m)\n", "{a: 1, b: 2}\n");
    assert_parity_out("e := {}\nprint(e)\n", "{}\n");
    assert_parity_out("m := {\"a\": 1, \"a\": 9}\nprint(m)\n", "{a: 9}\n");
}

#[test]
fn parity_map_index_read() {
    assert_parity_out("m := {\"a\": 1, \"b\": 2}\nprint(m[\"b\"])\n", "2\n");
}

#[test]
fn parity_map_missing_key_read_errors() {
    // Both engines must error identically on a missing key.
    let src = "m := {\"a\": 1}\nprint(m[\"z\"])\n";
    assert_parity(src);
    assert!(
        vm_outcome(src).unwrap_err().contains("key not found"),
        "{:?}",
        vm_outcome(src)
    );
}

#[test]
fn parity_map_index_insert_and_update() {
    assert_parity_out(
        "m := {\"a\": 1}\nm[\"b\"] = 2\nm[\"a\"] = 9\nprint(m)\n",
        "{a: 9, b: 2}\n",
    );
}

#[test]
fn parity_map_compound_assign() {
    assert_parity_out("m := {\"a\": 1}\nm[\"a\"] += 5\nprint(m[\"a\"])\n", "6\n");
}

#[test]
fn parity_map_compound_assign_missing_key_errors() {
    // Compound on a missing key is an error (consistent with read-missing).
    let src = "m := {\"a\": 1}\nm[\"z\"] += 1\n";
    assert_parity(src);
    assert!(
        vm_outcome(src).unwrap_err().contains("key not found"),
        "{:?}",
        vm_outcome(src)
    );
}

#[test]
fn parity_map_methods() {
    assert_parity_out("m := {\"a\": 1, \"b\": 2}\nprint(m.len())\n", "2\n");
    assert_parity_out(
        "m := {\"a\": 1}\nprint(m.has(\"a\"))\nprint(m.has(\"z\"))\n",
        "true\nfalse\n",
    );
    assert_parity_out(
        "m := {\"a\": 1}\nmatch m.get(\"a\"):\n    Some(v): print(v)\n    None: print(\"absent\")\n",
        "1\n",
    );
    assert_parity_out(
        "m := {\"a\": 1}\nmatch m.get(\"z\"):\n    Some(v): print(v)\n    None: print(\"absent\")\n",
        "absent\n",
    );
    assert_parity_out("m := {\"a\": 1, \"b\": 2}\nprint(m.keys())\n", "[a, b]\n");
    assert_parity_out("m := {\"a\": 1, \"b\": 2}\nprint(m.values())\n", "[1, 2]\n");
}

#[test]
fn parity_map_remove() {
    assert_parity_out(
        "m := {\"a\": 1, \"b\": 2}\nmatch m.remove(\"a\"):\n    Some(v): print(v)\n    None: print(\"absent\")\nprint(m)\n",
        "1\n{b: 2}\n",
    );
    // remove of a missing key -> None, map unchanged.
    assert_parity_out(
        "m := {\"a\": 1}\nmatch m.remove(\"z\"):\n    Some(v): print(v)\n    None: print(\"absent\")\nprint(m)\n",
        "absent\n{a: 1}\n",
    );
}

#[test]
fn parity_map_keys_iteration() {
    assert_parity_out(
        "m := {\"a\": 1, \"b\": 2, \"c\": 3}\nfor k in m.keys():\n    print(k)\n",
        "a\nb\nc\n",
    );
}

#[test]
fn parity_map_int_and_bool_keys() {
    assert_parity_out("m := {1: \"x\", 2: \"y\"}\nprint(m[2])\n", "y\n");
    assert_parity_out("m := {true: 1, false: 0}\nprint(m[false])\n", "0\n");
}

// ----- Hashable struct keys (hash-table map/set) -----

/// A struct with `hash(self) -> int` as a map key: insert/update/get/has/remove + insertion-order
/// iteration must be byte-identical across both engines.
#[test]
fn parity_map_struct_key() {
    let src = "\
struct P:
    x: int
    y: int
    fn hash(self) -> int:
        return self.x * 31 + self.y
fn main():
    m: Map[P, str] = {}
    m[P(1, 2)] = \"a\"
    m[P(3, 4)] = \"b\"
    m[P(1, 2)] = \"z\"
    for k in m:
        print(k)
    print(m[P(3, 4)])
    print(m.has(P(1, 2)))
    print(m.has(P(9, 9)))
    print(m.get(P(3, 4)))
    print(m.remove(P(1, 2)))
    print(m.len())
main()";
    assert_parity(src);
}

/// Set of structs: dedup of structurally-equal keys via custom hash + union/intersection/difference.
#[test]
fn parity_set_struct_algebra() {
    let src = "\
struct P:
    x: int
    fn hash(self) -> int:
        return self.x
fn main():
    a: Set[P] = Set([P(1), P(2), P(2), P(3)])
    b: Set[P] = Set([P(2), P(3), P(4)])
    print(a.len())
    print(a.union(b).len())
    print(a.intersection(b).len())
    print(a.difference(b).len())
    print(a.has(P(2)))
    a.remove(P(2))
    print(a.has(P(2)))
main()";
    assert_parity(src);
}

/// A struct used as a map key but MISSING `hash()` is a checker error — but `run_capture` bypasses
/// the checker, so the runtime must error consistently (not panic) on both engines.
#[test]
fn parity_map_struct_key_missing_hash_errors() {
    let src = "\
struct P:
    x: int
fn main():
    m: Map[P, int] = {}
    m[P(1)] = 5
main()";
    assert_parity(src);
}

/// REGRESSION (AsInt relocation): a non-int LIST index now errors at runtime in `GetIndex`,
/// with the SAME message the removed `AsInt` produced. The checker is bypassed by `run_capture`,
/// so this exercises the relocated runtime validation on both engines.
#[test]
fn parity_list_non_int_index_still_errors() {
    let src = "xs := [1, 2, 3]\nprint(xs[\"a\"])\n";
    assert_parity(src);
    assert!(
        vm_outcome(src)
            .unwrap_err()
            .contains("expected int, found str"),
        "{:?}",
        vm_outcome(src)
    );
    // And on assignment (SetIndex relocation).
    let src2 = "xs := [1, 2, 3]\nxs[\"a\"] = 9\n";
    assert_parity(src2);
    assert!(
        vm_outcome(src2)
            .unwrap_err()
            .contains("expected int, found str"),
        "{:?}",
        vm_outcome(src2)
    );
}

#[test]
fn parity_map_gc_stress_heap_keys_and_values() {
    // Keys AND values are heap strings; build many maps so collection runs mid-stream and the
    // `Heap::children` tracing of BOTH keys and values is exercised (a use-after-free if either
    // is untraced). The keys()/values() lists also hold heap children.
    let src = "fn main():\n    i := 0\n    while i < 200:\n        m := {\"k{i}\": \"v{i}\"}\n        m[\"extra\"] = \"x{i}\"\n        if i == 199:\n            print(m[\"k{i}\"])\n            print(m.values())\n        i += 1\nmain()\n";
    assert_parity(src);
    let expected = "v199\n[v199, x199]\n";
    assert_eq!(vm_outcome(src).unwrap(), expected);
    assert_eq!(
        run_capture_stress(src),
        expected,
        "VM gc_stress diverged (untraced map key/value?)"
    );
}

// (Removed `bench_vm_faster_than_interp`: it timed the bytecode VM against the tree-walk
// interpreter, which no longer exists. Perf tracking lives in `benches/run.chz` vs CPython and
// `docs/benchmarks.md`; serial-vs-M:N output parity on loop-heavy code is covered by other tests.)

// ===== gap #8: tuples + multi-return + destructuring =====

#[test]
fn parity_tuple_literal_display() {
    assert_parity_out("t := (1, 2)\nprint(t)\n", "(1, 2)\n");
}

#[test]
fn parity_tuple_element_access() {
    assert_parity_out("t := (3, 4)\nprint(t.0)\nprint(t.1)\n", "3\n4\n");
}

#[test]
fn parity_tuple_element_out_of_range_errors() {
    // The checker would catch `.2` statically, but `t` here is built so both engines hit the
    // runtime bounds check with the identical message — parity on the error path.
    assert_parity("t := (1, 2)\nprint(t.0)\nprint(t.1)\n");
}

#[test]
fn parity_destructure_local() {
    assert_parity_out("a, b := (1, 2)\nprint(a)\nprint(b)\n", "1\n2\n");
}

#[test]
fn parity_tuple_equality() {
    assert_parity_out(
        "print((1, 2) == (1, 2))\nprint((1, 2) == (1, 3))\n",
        "true\nfalse\n",
    );
}

#[test]
fn parity_multi_return_destructured_at_call_site() {
    let src = "fn pair() -> (int, int):\n    return (3, 4)\nfn main():\n    a, b := pair()\n    print(a + b)\nmain()\n";
    assert_parity_out(src, "7\n");
}

#[test]
fn parity_tuple_heap_elements_gc_stress() {
    // A tuple of heap values (a string + a list). Under GC stress a collection happens between
    // building the tuple and reading it back — proving `Heap::children` traces tuple elements.
    let src = "t := (\"hi\", [1, 2, 3])\nprint(t.0)\nprint(t.1)\n";
    assert_parity(src);
    assert_eq!(
        run_capture_stress(src),
        "hi\n[1, 2, 3]\n",
        "tuple elements not GC-traced?"
    );
}

// ----- slicing + Index/IndexSet/Slice protocol dispatch (VM ↔ interp parity) -----

#[test]
fn slice_list_and_str_parity() {
    assert_parity_out("print([1, 2, 3, 4, 5][1:3])\n", "[2, 3]\n");
    assert_parity_out("print(\"hello\"[0:2])\n", "he\n");
    // Open bounds + step + reverse — both engines byte-identical.
    assert_parity_out("print([1, 2, 3, 4, 5][2:])\n", "[3, 4, 5]\n");
    assert_parity_out("print([1, 2, 3, 4, 5][:2])\n", "[1, 2]\n");
    assert_parity_out("print([1, 2, 3, 4, 5][::2])\n", "[1, 3, 5]\n");
    assert_parity_out("print([1, 2, 3, 4, 5][::-1])\n", "[5, 4, 3, 2, 1]\n");
    assert_parity_out("print(\"hello\"[::-1])\n", "olleh\n");
    // Multibyte UTF-8 must round-trip through char-stepping (SSO/heap boundary) on both engines.
    assert_parity_out("print(\"héllo\"[::-1])\n", "olléh\n");
    assert_parity_out("print(\"héllo\"[1:3])\n", "él\n");
}

#[test]
fn slice_clamped_parity() {
    assert_parity_out("print([1, 2, 3][1:99])\n", "[2, 3]\n");
    assert_parity_out("print([1, 2, 3][2:1])\n", "[]\n");
    // Negative bound counts from the end (Python): [-1:2] -> start=2,end=2 -> [].
    assert_parity_out("print([1, 2, 3][-1:2])\n", "[]\n");
    assert_parity_out("print([1, 2, 3, 4, 5][-2:])\n", "[4, 5]\n");
    // Slice bounds CLAMP (no fault) even far out of range...
    assert_parity_out("print([1, 2, 3][-100:])\n", "[1, 2, 3]\n");
    // ...but a plain out-of-range negative index FAULTS, byte-identically.
    assert_parity("print([1, 2, 3][0 - 100])\n");
    // Zero step faults with the same message in both engines.
    assert_parity("print([1, 2, 3][::0])\n");
}

#[test]
fn negative_index_parity() {
    assert_parity_out("print([10, 20, 30][-1])\n", "30\n");
    assert_parity_out("print(\"hello\"[-2])\n", "l\n");
    assert_parity_out("xs := [1, 2, 3]\nxs[-1] = 99\nprint(xs[2])\n", "99\n");
}

const BUF_PROG: &str = "\
struct Buf:
    xs: List[int]
    fn index(self, key: int) -> int:
        return self.xs[key]
    fn set_index(self, key: int, val: int):
        self.xs[key] = val
    fn slice(self, start: int? = None, end: int? = None, step: int? = None) -> List[int]:
        match (start, end, step):
            (Some(s), Some(e), Some(c)): return self.xs[s:e:c]
            (Some(s), Some(e), None): return self.xs[s:e]
            (Some(s), None, Some(c)): return self.xs[s::c]
            (Some(s), None, None): return self.xs[s:]
            (None, Some(e), Some(c)): return self.xs[:e:c]
            (None, Some(e), None): return self.xs[:e]
            (None, None, Some(c)): return self.xs[::c]
            (None, None, None): return self.xs[:]
            _: return self.xs[:]
fn main():
    b := Buf([10, 20, 30])
    print(b[0])
    b[1] = 99
    print(b[1])
    b[0] += 5
    print(b[0])
    print(b[0:2])
    print(b[:])
    print(b[::-1])
main()";

#[test]
fn struct_index_slice_dispatch_parity() {
    assert_parity_out(
        BUF_PROG,
        "10\n99\n15\n[15, 99]\n[15, 99, 30]\n[30, 99, 15]\n",
    );
}

#[test]
fn slice_survives_gc_stress() {
    // The sliced list shares the source's element handles; a GC during the slice alloc must not
    // collect them. (Source is an inline temporary, unrooted except by the slice path.)
    let src = "print([1, 2, 3, 4, 5][1:4])\n";
    assert_parity(src);
    assert_eq!(
        run_capture_stress(src),
        "[2, 3, 4]\n",
        "slice elements not GC-rooted?"
    );
}

// ----- M19 lever #3: positional closure captures (HashMap → Vec<Value> by compile-time slot).
// Characterization net: these pin observable behavior across BOTH engines so the HashMap→Vec
// refactor cannot silently diverge. They pass on the pre-refactor (HashMap) code too. -----

/// Closure-capture golden: `examples/closure_capture.chz` (one var, several vars, nested
/// capture, a shared mutable box, HOF callbacks, and a hot-loop capture read) byte-identical on
/// the VM, the interpreter, the `--parallel` engine, and its `.expected`. The two-engine parity
/// is the oracle for M19 lever #3 (positional captures, HashMap → Vec<Value> by compile-time slot).
#[test]
fn golden_closure_capture_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/closure_capture.chz");
    let expected = include_str!("../../examples/closure_capture.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("parallel run"));
}

/// Closure-capture-across-scopes golden: `examples/closure_capture_scopes.chz` test-locks the
/// documented capture rule before the JIT — a plain local snapshots at creation (`10`), a
/// `ref` local is a shared box (`20`), and a global is referenced live (`20`). Byte-identical
/// on the VM, the interpreter, the `--parallel` engine, and its `.expected`. The example uses
/// `import std.ref` (the `ref int` annotation resolves to `Ref`), so it runs through the real
/// module graph via `run_file` (a temp entry), not `run_capture`/`compile_module_standalone`.
#[test]
fn golden_closure_capture_scopes_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/closure_capture_scopes.chz");
    let expected = include_str!("../../examples/closure_capture_scopes.expected");
    let dir = std::env::temp_dir().join(format!("chezzi_vm_cap_scopes_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(&entry, src).unwrap();
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    let (po, _pe, pr, _pc) = run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert!(ir.is_ok(), "interp faulted: {ir:?}");
    assert!(pr.is_ok(), "parallel faulted: {pr:?}");
    assert_eq!(vo, expected, "VM output vs .expected");
    assert_eq!(vo, io, "VM vs interp divergence");
    assert_eq!(vo, po, "VM vs parallel divergence");
}

#[test]
fn capture_single_var_parity() {
    // 1-var capture: the single slot read via GetCaptured.
    let src = "\
fn make():
    n := 7
    return fn(x: int) -> int: x + n
fn main():
    f := make()
    print(f(3))
main()";
    assert_parity_out(src, "10\n");
}

#[test]
fn capture_multi_var_parity() {
    // Multiple captured vars get distinct slots (snapshot order); all read positionally.
    let src = "\
fn make():
    a := 1
    b := 2
    c := 3
    return fn(x: int) -> int: x + a + b + c
fn main():
    f := make()
    print(f(10))
main()";
    assert_parity_out(src, "16\n");
}

#[test]
fn capture_nested_closure_parity() {
    // Inner closure captures the ENCLOSING closure's captured var (CapSrc::Captured) —
    // the nested-slot-mapping path. `n` must reach the innermost body positionally.
    let src = "\
fn make():
    n := 100
    return fn(a: int): fn(b: int) -> int: a + b + n
fn main():
    outer := make()
    inner := outer(20)
    print(inner(3))
main()";
    assert_parity_out(src, "123\n");
}

#[test]
fn capture_deep_nested_three_levels_parity() {
    // Three levels of capture chaining — each level forwards an enclosing capture by slot.
    let src = "\
fn make():
    base := 1000
    return fn(a: int): fn(b: int): fn(c: int) -> int: base + a + b + c
fn main():
    f := make()(200)(30)
    print(f(4))
main()";
    assert_parity_out(src, "1234\n");
}

#[test]
fn capture_mutable_box_mutation_parity() {
    // A captured mutable heap box (a list), mutated after capture — the handle is captured by
    // value, mutation flows through the shared object. Two closures capture the SAME list at
    // distinct slots; the writer's append is visible through the reader's slot. (Mirrors the
    // Ref[T] box pattern without needing a file-path module import.)
    let src = "\
fn main():
    box := [0]
    bump := fn(): box.push(box.len())
    rd := fn() -> int: box.len()
    bump()
    bump()
    bump()
    print(rd())
    print(box)
main()";
    assert_parity_out(src, "4\n[0, 1, 2, 3]\n");
}

#[test]
fn capture_hof_callbacks_parity() {
    // Closures-with-captures as map/filter/fold callbacks (the HOF hot path for GetCaptured).
    let src = "\
fn main():
    base := 10
    xs := [1, 2, 3, 4]
    doubled := xs.map(fn(x: int) -> int: x * base)
    print(doubled)
    threshold := 2
    kept := xs.filter(fn(x: int) -> bool: x > threshold)
    print(kept)
    seed := 100
    total := xs.fold(seed, fn(acc: int, x: int) -> int: acc + x)
    print(total)
main()";
    assert_parity_out(src, "[10, 20, 30, 40]\n[3, 4]\n110\n");
}

#[test]
fn capture_hot_read_loop_parity() {
    // GetCaptured executed many times in a loop (the hot path post-refactor: pure index).
    let src = "\
fn make():
    step := 2
    return fn(x: int) -> int: x + step
fn main():
    f := make()
    total := 0
    i := 0
    while i < 1000:
        total = total + f(i)
        i = i + 1
    print(total)
main()";
    assert_parity_out(src, "501500\n");
}

#[test]
fn capture_closure_across_spawn_parity() {
    // A capturing closure sent across spawn/channel — exercises the --parallel deep-clone +
    // wire/snap rebuild of positional captures. VM (serial), interp, and --parallel must agree.
    let src = "\
fn main():
    base := 41
    ch := Channel[int]()
    parallel:
        spawn:
            ch.send(base + 1)
    print(ch.recv())
main()";
    assert_parity_out(src, "42\n");
    assert_eq!(
        run_capture_parallel(src).expect("parallel"),
        "42\n",
        "--parallel capture wire"
    );
}

// ===== bytes type =====

#[test]
fn vm_bytes_ops() {
    // Index → int, negative index, reverse slice → bytes (repr), for-loop sum, len, ==/!=.
    let src = concat!(
        "fn main():\n",
        "    b := b\"\\x01\\x02\\x03\"\n",
        "    print(b[0])\n",    // 1
        "    print(b[-1])\n",   // 3
        "    print(b[::-1])\n", // b'\x03\x02\x01'
        "    s := 0\n",
        "    for x in b:\n",
        "        s = s + x\n",
        "    print(s)\n",                  // 6
        "    print(b.len())\n",            // 3
        "    print(b\"ab\" == b\"ab\")\n", // true
        "    print(b\"ab\" == b\"ac\")\n", // false
        "    print(b\"ab\" != b\"ac\")\n", // true
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(
        run_capture(src).expect("vm"),
        "1\n3\nb'\\x03\\x02\\x01'\n6\n3\ntrue\nfalse\ntrue\n"
    );
}

#[test]
fn vm_bytes_index_out_of_range_recoverable() {
    // An out-of-range index faults recoverably (catchable by `recover:`), like list/str.
    let src = concat!(
        "fn main():\n",
        "    b := b\"\\x01\\x02\"\n",
        "    x := recover:\n",
        "        b[9]\n",
        "    match x:\n",
        "        Ok(v): print(v)\n",
        "        Err(e): print(\"caught\")\n",
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(run_capture(src).expect("vm"), "caught\n");
}

#[test]
fn vm_bytes_repr_and_map_key() {
    // Display/str repr is Python b'...'; bytes works as a map key (Hashable).
    let src = concat!(
        "fn main():\n",
        "    print(b\"hi\\n\")\n",      // b'hi\n'
        "    print(str(b\"\\xFF\"))\n", // b'\xff'
        "    m := {b\"a\": 1, b\"b\": 2}\n",
        "    print(m[b\"a\"])\n", // 1
        "    print(m[b\"b\"])\n", // 2
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(run_capture(src).expect("vm"), "b'hi\\n'\nb'\\xff'\n1\n2\n");
}

#[test]
fn vm_bytes_slice_step_parity() {
    let src = "print(b\"\\x00\\x01\\x02\\x03\\x04\"[1:4:2])\nprint(b\"abc\"[1:])\n";
    assert_parity(src);
}

#[test]
fn bytes_crosses_channel() {
    // `bytes` is fully value-typed and sendable — it crosses the --parallel airlock via
    // WireValue::Bytes and reconstructs as a fresh heap `bytes` (Python b'...' repr preserved).
    // Buffer-then-drain (send before recv) so all THREE engines agree — the sequential interp
    // cannot block a consumer mid-flight on a live producer (a C5 limitation, not bytes-specific).
    let src = concat!(
        "fn main():\n",
        "    ch := Channel[bytes]()\n",
        "    parallel:\n",
        "        spawn ch.send(b\"\\x01\\x02\")\n",
        "    print(ch.recv())\n",
        "main()\n"
    );
    // Three-engine parity: cooperative VM, --parallel M:N, and interp all agree.
    assert_parity(src);
    assert_eq!(run_capture(src).expect("vm"), "b'\\x01\\x02'\n");
    assert_eq!(
        run_capture_parallel(src).expect("parallel"),
        "b'\\x01\\x02'\n"
    );
}

// ===== bytearray type (mutable sibling of bytes) =====

#[test]
fn vm_bytearray_ops() {
    // Constructors (all 4 forms), index read, slice -> bytearray (incl. reverse), for-loop sum,
    // len, push, pop, extend, ==/!=, and Display bytearray(b'...').
    let src = concat!(
        "fn main():\n",
        "    print(bytearray())\n",                // bytearray(b'')
        "    print(bytearray(3))\n",               // bytearray(b'\x00\x00\x00')
        "    print(bytearray(b\"\\x01\\x02\"))\n", // bytearray(b'\x01\x02')
        "    ba := bytearray([1, 2, 3])\n",
        "    print(ba)\n",       // bytearray(b'\x01\x02\x03')
        "    print(ba[0])\n",    // 1
        "    print(ba[-1])\n",   // 3
        "    print(ba[::-1])\n", // bytearray(b'\x03\x02\x01')
        "    s := 0\n",
        "    for x in ba:\n",
        "        s = s + x\n",
        "    print(s)\n",        // 6
        "    print(ba.len())\n", // 3
        "    ba.push(4)\n",
        "    print(ba)\n",       // bytearray(b'\x01\x02\x03\x04')
        "    print(ba.pop())\n", // Some(4)
        "    ba.extend(b\"\\xFF\")\n",
        "    ba.extend([7, 8])\n",
        "    print(ba)\n", // bytearray(b'\x01\x02\x03\xff\x07\x08')
        "    print(bytearray([1]) == bytearray([1]))\n", // true
        "    print(bytearray([1]) != bytearray([2]))\n", // true
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(
        run_capture(src).expect("vm"),
        "bytearray(b'')\nbytearray(b'\\x00\\x00\\x00')\nbytearray(b'\\x01\\x02')\nbytearray(b'\\x01\\x02\\x03')\n1\n3\nbytearray(b'\\x03\\x02\\x01')\n6\n3\nbytearray(b'\\x01\\x02\\x03\\x04')\nSome(4)\nbytearray(b'\\x01\\x02\\x03\\xff\\x07\\x08')\ntrue\ntrue\n"
    );
}

#[test]
fn vm_bytearray_index_write_and_shared_mutation() {
    // ba[i] = x mutates in place; a second binding observes it (proves in-place, not copy).
    let src = concat!(
        "fn main():\n",
        "    ba := bytearray([1, 2, 3])\n",
        "    ba2 := ba\n",
        "    ba[0] = 65\n",
        "    print(ba2[0])\n", // 65 — same buffer
        "    ba2[1] = 66\n",
        "    print(ba[1])\n", // 66 — observed through the other binding
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(run_capture(src).expect("vm"), "65\n66\n");
}

#[test]
fn vm_bytearray_oob_and_value_range_recoverable() {
    // An out-of-range index OR an out-of-range value (not 0..=255) is a recoverable fault.
    let src = concat!(
        "fn main():\n",
        "    ba := bytearray([1, 2])\n",
        "    r1 := recover:\n",
        "        ba[9] = 1\n",
        "    match r1:\n",
        "        Ok(v): print(\"ok\")\n",
        "        Err(e): print(\"caught oob index\")\n",
        "    r2 := recover:\n",
        "        ba[0] = 999\n",
        "    match r2:\n",
        "        Ok(v): print(\"ok\")\n",
        "        Err(e): print(\"caught bad value\")\n",
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(
        run_capture(src).expect("vm"),
        "caught oob index\ncaught bad value\n"
    );
}

#[test]
fn vm_bytearray_huge_size_is_recoverable_not_abort() {
    // `bytearray(N)` for an absurd N must fault recoverably (try_reserve), NOT abort the process
    // (SIGABRT) uncatchably — the language's recoverable-fault invariant (cf. range()'s cap).
    let src = concat!(
        "fn main():\n",
        "    r := recover:\n",
        "        bytearray(9999999999999)\n",
        "    match r:\n",
        "        Ok(v): print(\"ok\")\n",
        "        Err(e): print(\"caught huge\")\n",
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(run_capture(src).expect("vm"), "caught huge\n");
}

#[test]
fn vm_bytearray_conversion_bridge() {
    // bytes(ba) -> immutable snapshot; bytearray(b) -> mutable copy; round-trip + independence.
    let src = concat!(
        "fn main():\n",
        "    ba := bytearray([1, 2, 3])\n",
        "    b := bytes(ba)\n",
        "    print(b)\n", // b'\x01\x02\x03'
        "    ba[0] = 99\n",
        "    print(b)\n", // b'\x01\x02\x03' — snapshot unaffected
        "    ba2 := bytearray(b\"\\x07\\x08\")\n",
        "    ba2[0] = 10\n",
        "    print(ba2)\n",          // bytearray(b'\n\x08') — 0x0a renders as \n
        "    print(bytearray(b))\n", // bytearray(b'\x01\x02\x03')
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(
        run_capture(src).expect("vm"),
        "b'\\x01\\x02\\x03'\nb'\\x01\\x02\\x03'\nbytearray(b'\\n\\x08')\nbytearray(b'\\x01\\x02\\x03')\n"
    );
}

#[test]
fn bytearray_crosses_channel_deep_copy() {
    // `bytearray` crosses the --parallel airlock by VALUE (deep copy): the other side gets a
    // fresh independent buffer. Buffer-then-drain (send before recv) so the sequential interp
    // agrees, exactly like bytes_crosses_channel (a C5 limitation, not bytearray-specific).
    let src = concat!(
        "fn main():\n",
        "    ch := Channel[bytearray]()\n",
        "    parallel:\n",
        "        spawn ch.send(bytearray([1, 2]))\n",
        "    print(ch.recv())\n",
        "main()\n"
    );
    assert_parity(src);
    assert_eq!(run_capture(src).expect("vm"), "bytearray(b'\\x01\\x02')\n");
    assert_eq!(
        run_capture_parallel(src).expect("parallel"),
        "bytearray(b'\\x01\\x02')\n"
    );
}

// ===== Iterable[T] / `.iter()` cursor =====

#[test]
fn iter_next_idempotent_both_engines() {
    // next() yields Some(10), Some(20), then None forever (idempotent past exhaustion).
    let src = "fn main():\n    it := [10, 20].iter()\n    print(it.next())\n    print(it.next())\n    print(it.next())\n    print(it.next())\nmain()\n";
    assert_parity_out(src, "Some(10)\nSome(20)\nNone\nNone\n");
}

#[test]
fn iter_empty_collection_none_immediately() {
    let src =
        "fn main():\n    xs: List[int] = []\n    it := xs.iter()\n    print(it.next())\nmain()\n";
    assert_parity_out(src, "None\n");
}

#[test]
fn iter_snapshot_order_matches_for() {
    // For each collection, the cursor's element sequence must equal `for x in X`.
    for coll in [
        "[1, 2, 3]",
        "{1, 2, 3}",
        "{1: \"a\", 2: \"b\"}", // map → keys
        "\"abc\"",
        "b\"hi\"",
        "bytearray([7, 8])",
    ] {
        let via_for = format!("fn main():\n    for x in {coll}:\n        print(x)\nmain()\n");
        let via_iter = format!(
            "fn main():\n    it := ({coll}).iter()\n    while true:\n        match it.next():\n            Some(x):\n                print(x)\n            None:\n                break\nmain()\n"
        );
        assert_parity(&via_for);
        assert_parity(&via_iter);
        let for_out = vm_outcome(&via_for);
        let iter_out = vm_outcome(&via_iter);
        assert_eq!(
            for_out, iter_out,
            "cursor order must match `for` for {coll}"
        );
    }
}

#[test]
fn iter_self_on_iterator_value() {
    // iter() on a cursor returns self (idempotent); driving the result still works.
    let src = "fn main():\n    it := [1, 2].iter().iter()\n    print(it.next())\nmain()\n";
    assert_parity_out(src, "Some(1)\n");
}

#[test]
fn list_of_cursor_roundtrip_both_engines() {
    assert_parity_out(
        "fn main():\n    print(List([5, 6, 7].iter()))\nmain()\n",
        "[5, 6, 7]\n",
    );
    assert_parity_out(
        "fn main():\n    print(Set({1, 2}.iter()).len())\nmain()\n",
        "2\n",
    );
}

#[test]
fn cursor_composes_into_adapter() {
    // The headline win: a list cursor flows into a `[I: Iterator[T]]` adapter.
    let src = concat!(
        "struct Take[I: Iterator[T], T]:\n",
        "    inner: I\n",
        "    left: int\n",
        "    fn next(self) -> Option[T]:\n",
        "        if self.left <= 0:\n",
        "            return None\n",
        "        self.left = self.left - 1\n",
        "        return self.inner.next()\n",
        "fn main():\n",
        "    for v in Take([10, 20, 30, 40].iter(), 2):\n",
        "        print(v)\n",
        "main()\n"
    );
    assert_parity_out(src, "10\n20\n");
}

#[test]
fn for_over_pure_iterable_struct() {
    // A struct with ONLY iter() drives `for`; one with BOTH still uses next() (sentinel).
    let only_iter = concat!(
        "struct Wrap:\n",
        "    xs: List[int]\n",
        "    fn iter(self) -> Iterator[int]:\n",
        "        return self.xs.iter()\n",
        "fn main():\n",
        "    for x in Wrap([1, 2, 3]):\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_parity_out(only_iter, "1\n2\n3\n");
    // Both iter() and next(): next() wins (iter() would print a different sentinel).
    let both = concat!(
        "struct Two:\n",
        "    n: int\n",
        "    fn next(self) -> Option[int]:\n",
        "        if self.n >= 2:\n",
        "            return None\n",
        "        v := self.n\n",
        "        self.n = self.n + 1\n",
        "        return Some(v + 100)\n",
        "    fn iter(self) -> Iterator[int]:\n",
        "        return [999].iter()\n",
        "fn main():\n",
        "    for x in Two(0):\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_parity_out(both, "100\n101\n");
}

// ---- Bug C: a `for`/`List()`/`Set()` over a NAMED builtin cursor consumes it in place ----
// (must match `.next()` and struct iterators and docs/syntax.md:709-713).

#[test]
fn for_named_cursor_partial_consume_then_drain() {
    // The repro: partial-consume via a broken `for`, then `List()` drains the REMAINDER.
    let src = concat!(
        "fn main():\n",
        "    it := [1, 2, 3, 4].iter()\n",
        "    seen := 0\n",
        "    for x in it:\n",
        "        seen = seen + 1\n",
        "        if x == 2:\n",
        "            break\n",
        "    rest := List(it)\n",
        "    print(\"seen={seen} rest={rest}\")\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap().trim(), "seen=2 rest=[3, 4]");
    assert_parity(src);
}

#[test]
fn for_named_cursor_two_pass_second_yields_nothing() {
    // A named cursor is consumed by the first `for`; the second pass yields nothing.
    let src = concat!(
        "fn main():\n",
        "    it := [1, 2, 3].iter()\n",
        "    for x in it:\n",
        "        print(x)\n",
        "    for x in it:\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap(), "1\n2\n3\n");
    assert_parity(src);
}

#[test]
fn next_after_for_over_named_cursor_is_none() {
    // `for` advances the shared cursor; a trailing `.next()` sees it exhausted.
    let src = concat!(
        "fn main():\n",
        "    it := [10, 20].iter()\n",
        "    for x in it:\n",
        "        print(x)\n",
        "    print(it.next())\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap(), "10\n20\nNone\n");
    assert_parity(src);
}

#[test]
fn for_over_collection_reiterates_fully_twice() {
    // Invariant 1: a NON-cursor collection (list) keeps fresh-snapshot semantics — both passes full.
    let src = concat!(
        "fn main():\n",
        "    xs := [1, 2, 3]\n",
        "    for x in xs:\n",
        "        print(x)\n",
        "    for x in xs:\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap(), "1\n2\n3\n1\n2\n3\n");
    assert_parity(src);
}

#[test]
fn iter_of_iter_fresh_cursor() {
    // Invariant 2: `xs.iter().iter()` is one fresh cursor (no double-advance).
    let src = concat!(
        "fn main():\n",
        "    a := [5, 6].iter().iter()\n",
        "    print(a.next())\n",
        "    for x in a:\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap(), "Some(5)\n6\n");
    assert_parity(src);
}

#[test]
fn for_over_fresh_temp_cursor_full() {
    // Invariant 3: a fresh unnamed temp cursor still fully iterates.
    let src = concat!(
        "fn main():\n",
        "    for x in [7, 8, 9].iter():\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap(), "7\n8\n9\n");
    assert_parity(src);
}

#[test]
fn cursor_crosses_spawn_airlock_three_engine_parity() {
    // A cursor IS sendable: it crosses the spawn/channel airlock as a DEEP COPY (independent
    // snapshot + pos on the receiver), like a `list`. The receiver drives it and gets `Some(1)`.
    // This must hold byte-identically on ALL THREE engines — the cooperative VM, the M:N
    // `--parallel` engine, and the interpreter (whose `deep_clone` already deep-copies a cursor).
    // (Regression guard: an earlier cut gated the cursor non-sendable like a generator, which
    // panicked the spawned worker on the VM while the interp succeeded — a parity divergence.)
    let src = concat!(
        "fn main():\n",
        "    ch := Channel[Iterator[int]]()\n",
        "    parallel:\n",
        "        spawn ch.send([1, 2].iter())\n",
        "    print(ch.recv().next())\n",
        "main()\n"
    );
    assert_parity_out(src, "Some(1)\n");
    assert_eq!(
        run_capture_parallel(src).expect("--parallel"),
        "Some(1)\n",
        "cursor crosses the M:N airlock"
    );
}

#[test]
fn generator_iter_returns_self_vm() {
    // VM-only (interp has no generators): a generator's iter() returns self and drives.
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "    yield 2\n",
        "fn main():\n",
        "    it := gen().iter()\n",
        "    print(it.next())\n",
        "    print(it.next())\n",
        "    print(it.next())\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).expect("vm"), "Some(1)\nSome(2)\nNone\n");
}

// ===== one-way int→float implicit widening (Architecture C: real runtime coercion) =====

/// Assert all three engines (VM serial default, interp oracle, VM `--parallel`) produce `want`.
fn widen_three_engines(src: &str, want: &str) {
    assert_eq!(run_capture(src).expect("vm"), want, "vm engine");
    assert_eq!(
        run_capture_parallel(src).expect("interp"),
        want,
        "interp engine"
    );
    assert_eq!(
        run_capture_parallel(src).expect("parallel"),
        want,
        "parallel engine"
    );
}

/// A `float`-annotated let binding stores a genuine `f64` (display `3.0`), and `x / 2` is FLOAT
/// division (`1.5`), NOT int division (`1`). The division is the load-bearing semantic proof.
#[test]
fn widen_let_display_and_division() {
    widen_three_engines("x: float = 3\nprint(x)\nprint(x / 2)\n", "3.0\n1.5\n");
}

/// Passing an int VARIABLE into a `float` param coerces at the callee prologue: `z / 2` is float
/// division. Proves the coercion is at the callee boundary (works for any caller, not just literals).
#[test]
fn widen_param_int_variable_division() {
    widen_three_engines("fn f(z: float):\n    print(z / 2)\na := 3\nf(a)\n", "1.5\n");
}

/// A non-literal int expression returned from a `-> float` function is coerced before `Return`.
#[test]
fn widen_return_nonliteral_int_expr() {
    widen_three_engines(
        "fn g(n: int) -> float:\n    return n + 1\nprint(g(2) / 2)\n",
        "1.5\n",
    );
}

/// An int field value widens into a `float` struct field (per-field coercion at `NewStruct`).
#[test]
fn widen_struct_float_field_division() {
    widen_three_engines(
        "struct P:\n    v: float\np := P(3)\nprint(p.v / 2)\n",
        "1.5\n",
    );
}

/// An int DEFAULT value widens into a `float` param: omitted (`g()` → spliced default coerced at
/// the prologue) AND explicit int (`g(5)`) both store a genuine f64.
#[test]
fn widen_default_param_division() {
    widen_three_engines(
        "fn g(a: float = 3) -> float:\n    return a / 2\nprint(g())\nprint(g(5))\n",
        "1.5\n2.5\n",
    );
}

/// An inline-expr fn body (`fn g(n: int) -> float: n + 1`) coerces its implicit return too.
#[test]
fn widen_inline_expr_body_return() {
    widen_three_engines("fn g(n: int) -> float: n + 1\nprint(g(2) / 2)\n", "1.5\n");
}

/// A `float`-param closure coerces at its prologue.
#[test]
fn widen_closure_float_param_division() {
    widen_three_engines("f := fn(z: float): z / 2\nprint(f(3))\n", "1.5\n");
}

/// (A) Annotated `List[float] = [1, 2.3]` — `xs[0]` is a genuine float (`1 / 2 == 0.5`).
/// (B) Un-annotated all-literal mix `[1, 2.3]` widens its int LITERAL via the peephole.
/// (C) A map VALUE float position likewise widens.
#[test]
fn widen_collection_annotated_and_literal() {
    widen_three_engines("xs: List[float] = [1, 2.3]\nprint(xs[0] / 2)\n", "0.5\n");
    widen_three_engines(
        "ys := [1, 2.3]\nprint(ys[0] / 2)\nprint(ys[1])\n",
        "0.5\n2.3\n",
    );
    widen_three_engines(
        "m: Map[str, float] = {\"a\": 1, \"b\": 2.3}\nprint(m[\"a\"] / 2)\n",
        "0.5\n",
    );
}

/// An all-int literal collection must NOT widen (the peephole only fires when ≥1 float literal is
/// present): `[1, 2, 3]` stays `List[int]`, so `xs[0] / 2` is int division (`0`).
#[test]
fn widen_all_int_literal_collection_stays_int() {
    widen_three_engines("xs := [1, 2, 3]\nprint(xs[0] / 2)\n", "0\n");
}

/// Regression-pin: mixed int/float COMPARISONS already widen at runtime; ensure no double-coerce
/// or divergence after the new coercion ops.
#[test]
fn widen_mixed_comparisons_pinned() {
    widen_three_engines("print(1 < 2.3)\nprint(1 == 2.3)\n", "true\nfalse\n");
}

/// An ANNOTATED `List[float]` widens a NON-LITERAL int element too (all elements coerced): both
/// engines must agree (`a` is an int variable, not a literal).
#[test]
fn widen_annotated_list_widens_nonliteral_element() {
    widen_three_engines(
        "a := 1\nxs: List[float] = [a, 2.3]\nprint(xs[0] / 2)\n",
        "0.5\n",
    );
}

/// CARVE-OUT pinned: an UN-ANNOTATED non-literal mixed collection (`xs := [a, b]`, a:int, b:float)
/// is NOT element-widened (the compiler is type-blind about non-literal `a`), so `xs[0]` stays Int
/// → `xs[0] / 2` is int division (`0`). Both engines must AGREE on this (parity, not correctness).
#[test]
fn widen_unannotated_nonliteral_mixed_collection_carveout() {
    widen_three_engines("a := 1\nb := 2.3\nxs := [a, b]\nprint(xs[0] / 2)\n", "0\n");
}
