//! Checker tests: refine-on-first-use of empty collections, import name collisions, duplicate
//! binders.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ============================================================================
// refine-on-first-use: empty-collection element/key/value Unknown slots
// (the empty-slot half of the Ty::Unknown soundness family). An empty literal's
// element/key/value (or nullary-variant type arg / native None) is Unknown; the
// FIRST mutating op that supplies a concrete type re-pins the binding, so a later
// conflicting op is a normal error and the Hashable/float-key ban runs when the
// key/element becomes concrete. Refinement is BLOCK-LOCAL flow-sensitive: a
// refinement inside one branch arm does not leak into a sibling arm or post-branch.
// ============================================================================

// ---- step 1: straight-line refine pins the element, later conflict rejected ----

#[test]
fn empty_list_push_pins_element_then_mixed_rejected() {
    // x:=[]; x.push(1) pins List[int]; x.push("s") is then a normal mismatch.
    rejects(
        "fn main():\n x := []\n x.push(1)\n x.push(\"s\")\nmain()",
        "cannot infer the",
    );
}

#[test]
fn empty_list_of_none_then_conflicting_some_rejected() {
    // `[None]` is a list of an open `T?`; `push(?5)` pins `List[int?]`; `push(?"hi")` then
    // conflicts (`'?' value: expected int, found str`).
    rejects(
        "fn main():\n xs := [None]\n xs.push(?5)\n xs.push(?\"hi\")\nmain()",
        "cannot infer the",
    );
}

#[test]
fn empty_list_of_nullary_enum_then_conflicting_variant_rejected() {
    // [Box.Empty] is List[Box[Unknown]]; push(Box.Full("hi")) refines to List[Box[str]];
    // push(Box.Full(5)) then conflicts (nullary-variant producer).
    rejects(
        "enum Box[T]:\n Full(T)\n Empty\nfn main():\n xs := [Box.Empty]\n xs.push(Box.Full(\"hi\"))\n xs.push(Box.Full(5))\nmain()",
        "cannot infer the",
    );
}

// ---- step 2: insertion-site Hashable / float-key ban on empty {}/Set() ----

#[test]
fn empty_map_float_key_rejected() {
    // m:={}; m[1.5]="b" — float key must be rejected even though key type is Unknown.
    rejects(
        "fn main():\n m := {}\n m[1.5] = \"b\"\nmain()",
        "cannot infer the",
    );
}

#[test]
fn empty_set_float_and_nan_rejected() {
    // s:=Set(); s.add(1.5) and the inf-inf NaN add — both non-Hashable, rejected.
    rejects(
        "fn main():\n s := Set()\n s.add(1.5)\nmain()",
        "cannot infer the",
    );
    rejects(
        "fn main():\n big := 1e308\n inf := big * 10.0\n nan := inf - inf\n s := Set()\n s.add(nan)\nmain()",
        "cannot infer the",
    );
}

// ---- step 3: un-annotated heterogeneous struct list rejected with annotation hint ----

// ---- PART A: a never-constrained empty collection requires an annotation ----

#[test]
fn unconstrained_empty_list_rejected() {
    // `b := []` whose element type is NEVER inferred (only read) is now a static error.
    rejects(
        "fn main():\n b := []\n print(b)\nmain()",
        "cannot infer the",
    );
}

#[test]
fn unconstrained_empty_map_rejected() {
    rejects(
        "fn main():\n b := {}\n print(b)\nmain()",
        "cannot infer the",
    );
}

#[test]
fn unconstrained_empty_set_rejected() {
    rejects(
        "fn main():\n b := Set()\n print(b)\nmain()",
        "cannot infer the",
    );
}

#[test]
fn unconstrained_empty_at_module_level_rejected() {
    // top-level script binding, never constrained → error (caught at the module seam).
    rejects("b := []\nprint(b)\n", "cannot infer the");
}

// false-positive matrix: a typed sink unifies the Unknown away → NO error.

#[test]
fn typed_annotation_empty_list_ok() {
    ok("fn main():\n b: List[int] = []\n print(b)\nmain()");
}

#[test]
fn typed_annotation_empty_map_ok() {
    ok("fn main():\n m: Map[str, int] = {}\n print(m)\nmain()");
}

#[test]
fn typed_annotation_empty_set_ok() {
    ok("fn main():\n s: Set[int] = Set()\n print(s)\nmain()");
}

#[test]
fn typed_param_empty_arg_ok() {
    // `f([])` where the param is List[int] — the literal flows into a typed sink, no local site.
    ok("fn f(xs: List[int]):\n print(xs.len())\nfn main():\n f([])\nmain()");
}

#[test]
fn typed_return_empty_ok() {
    ok("fn g() -> List[int]:\n return []\nfn main():\n print(g().len())\nmain()");
}

#[test]
fn turbofish_empty_ctor_ok() {
    ok("fn main():\n b := List[int]()\n print(b)\nmain()");
}

#[test]
fn turbofish_empty_ctor_from_list_ok() {
    ok("fn main():\n b := List[int]([])\n print(b)\nmain()");
}

#[test]
fn empty_push_then_read_no_false_error() {
    // refine-on-first-use constrains the binding → no annotation required.
    ok("fn main():\n out: List[int] = []\n out.push(1)\n print(out)\nmain()");
}

// false-positive matrix #2 — a binding constrained by a CONCRETE value flowing into it (NOT just the
// two refine-on-first-use gates: push/add/insert/extend + index-assign) must NOT be flagged. These
// were rejected by the original impl (the pending-site drop wired only into the two refine gates).

#[test]
fn empty_then_plain_reassign_concrete_ok() {
    // `b: List[int] = []` then a whole-binding reassignment `b = [1, 2, 3]` determines the element type → no
    // annotation required (the binding IS constrained).
    ok("fn main():\n b: List[int] = []\n b = [1, 2, 3]\n print(b)\nmain()");
}

#[test]
fn empty_then_compound_assign_concrete_ok() {
    // `b: List[int] = []` then `b += [1, 2, 3]` (compound list-extend) constrains the element type.
    ok("fn main():\n b: List[int] = []\n b += [1, 2, 3]\n print(b)\nmain()");
}

#[test]
fn empty_then_tuple_assign_concrete_ok() {
    // tuple-assignment `a, b = [1], [2]` constrains both bindings (recurses into the Ident arm).
    ok(
        "fn main():\n a: List[int] = []\n b: List[int] = []\n a, b = [1], [2]\n print(a)\n print(b)\nmain()",
    );
}

#[test]
fn empty_then_reassign_from_call_ok() {
    // `result: List[int] = []` then `result = compute()` where `compute() -> List[int]` constrains it.
    ok(
        "fn compute() -> List[int]:\n return [1, 2]\nfn main():\n result: List[int] = []\n result = compute()\n print(result)\nmain()",
    );
}

#[test]
fn empty_binding_into_typed_param_ok() {
    // the spec's typed-parameter false-positive guard, one binding away: `f(b)` where the param is
    // `List[int]` constrains `b` (the direct-literal form `f([])` is covered separately above).
    ok("fn f(xs: List[int]):\n print(xs.len())\nfn main():\n b: List[int] = []\n f(b)\nmain()");
}

#[test]
fn empty_then_conditional_reassign_ok() {
    // 'declare empty, fill in a branch' idiom — the reassignment lives in an inner block but
    // constrains the fn-scope binding.
    ok("fn main():\n out: List[str] = []\n if true:\n  out = [\"x\"]\n print(out)\nmain()");
}

#[test]
fn empty_binding_into_typed_return_ok() {
    // the typed-return false-positive guard, one binding away: `b: List[int] = []` then `return b` where the
    // return type is `List[int]` constrains `b` (the direct-literal form `return []` is covered above).
    ok(
        "fn g() -> List[int]:\n b: List[int] = []\n return b\nfn main():\n print(g().len())\nmain()",
    );
}

#[test]
fn empty_then_reassign_still_empty_rejected() {
    // GUARD: reassigning ANOTHER empty literal does NOT constrain — still no element type → error.
    rejects(
        "fn main():\n b := []\n b = []\n print(b)\nmain()",
        "cannot infer the",
    );
}

#[test]
fn empty_into_typed_binding_value_ok() {
    // REGRESSION (bug #1): `b: List[int] = []` then `c: List[int] = b` — binding the empty into a
    // CONCRETE-typed annotated let constrains b's element type (the spec's typed-binding
    // false-positive guard, one binding away from `b: List[int] = []`). The annotated-let branch
    // must drop b's pending site; base accepts this.
    ok("fn main():\n b: List[int] = []\n c: List[int] = b\n print(c.len())\nmain()");
}

#[test]
fn empty_captured_then_push_ok() {
    // REGRESSION (bug #2): `acc := []` then a `spawn:` body that supplies the element via
    // `acc.push(1)` constrains acc — the capture early-return in the receiver refine must still drop
    // the pending annotation site. Base accepts this; the element type IS supplied (via push).
    rejects(
        "fn main():\n acc := []\n spawn:\n  acc.push(1)\n print(acc)\nmain()",
        "cannot infer the",
    );
}

// false-positive matrix #3 — an empty binding READ AS A VALUE that ESCAPES into another binding or
// structure (RHS of plain/field assign, untyped alias, or nested in a collection literal) is no
// longer provably-unconstrained and must NOT be flagged. These regressed when the feature landed:
// the drop-guard covered only typed sinks + the LHS-target of reassign, never the RHS source.

#[test]
fn empty_into_plain_assign_target_ok() {
    // REGRESSION: `c := [1]` (List[int]) then `c = b` (b: List[int] = []) flows b into a typed slot via plain
    // `=`. Sound (annotated `c: List[int] = b` accepts); must not error on b.
    ok("fn main():\n c := [1]\n b: List[int] = []\n c = b\n print(c)\nmain()");
}

#[test]
fn empty_into_field_assign_ok() {
    // REGRESSION: assigning an empty binding into a CONCRETE-typed struct field via `bx.items = b`
    // constrains b's element type — must not error on b.
    ok(
        "struct Box:\n items: List[int]\nfn main():\n bx := Box([1])\n b: List[int] = []\n bx.items = b\n print(bx.items)\nmain()",
    );
}

#[test]
fn empty_alias_then_push_ok() {
    // REGRESSION: `c := b` aliases the same list; `c.push(1)` establishes the element type. b escapes
    // into the alias, so its pending site must drop (annotated `b: List[int] = []` runs, prints [1]).
    ok("fn main():\n b: List[int] = []\n c := b\n c.push(1)\n print(b)\nmain()");
}

#[test]
fn empty_alias_both_unconstrained_still_rejected() {
    // GUARD (no new false-negative): aliasing drops the SOURCE site, but the alias `c` is itself an
    // unrefined empty — the requirement moves to c, it does not vanish. Still an error.
    rejects(
        "fn main():\n b := []\n c := b\n print(c)\nmain()",
        "cannot infer the",
    );
}

// ---- step 4: PERSISTENT refine-on-first-use — the first use pins the element/key/value type
// for the binding's whole scope, even across sibling STATEMENT branches/arms. Building a
// heterogeneous collection split across branches is now a type error, exactly like `[1, "s"]`. ----

#[test]
fn flow_sensitive_if_else_int_vs_str_rejects() {
    // First-use pin PERSISTS: the then-arm pins xs to List[int]; the else-arm's str push is a
    // cross-branch element-type conflict — rejected (a sound static over-approximation).
    rejects(
        "fn main():\n c := true\n xs := []\n if c:\n  xs.push(1)\n else:\n  xs.push(\"s\")\nmain()",
        "cannot infer the",
    );
}

#[test]
fn flow_sensitive_map_if_elif_rejects() {
    // The first arm pins cfg to Map[str,int]; the else-if writes a float value. float→int is the
    // LOSSY direction (NOT widened — consistent with one-way int→float widening) → rejected.
    rejects(
        "fn main():\n c := 1\n cfg := {}\n if c == 1:\n  cfg[\"x\"] = 1\n elif c == 2:\n  cfg[\"y\"] = 2.0\nmain()",
        "cannot infer the",
    );
}

#[test]
fn flow_sensitive_set_if_else_rejects() {
    // First-use pin persists across sibling arms: set pinned to Set[int], the str add is rejected.
    rejects(
        "fn main():\n c := true\n s := Set()\n if c:\n  s.add(1)\n else:\n  s.add(\"x\")\nmain()",
        "cannot infer the",
    );
}

// ---- step 5: an inner-block first-use pin PERSISTS; a later conflicting use is rejected ----

#[test]
fn refine_inside_block_persists_then_conflict_rejected() {
    // The if-arm's push(1) pins xs to List[int] for the whole scope (the pin is written to the
    // OWNING outer scope by the pin write and survives the block's `pop_scope`). The post-if push("s")
    // is therefore a real element-type conflict — rejected. (Persistent first-use pinning replaces
    // the old block-local "does not leak" design.)
    rejects(
        "fn main():\n xs := []\n if true:\n  xs.push(1)\n xs.push(\"s\")\nmain()",
        "cannot infer the",
    );
}

#[test]
fn refine_inside_block_on_outer_list_ok() {
    // the pin write targets the OWNING (outer) scope; a HOMOGENEOUS build across block boundary stays fine
    // — the persistent pin only rejects a CONFLICTING later use, not a matching one.
    ok("fn main():\n xs: List[int] = []\n if true:\n  xs.push(1)\n xs.push(2)\nmain()");
}

// ---- repros 2-5: persistent-pin rejections (single arm then concrete use, second-arm conflict,
// statement-match arm conflict, loop-body pin then post-loop conflict) ----

#[test]
fn refine_single_arm_then_concrete_use_rejects() {
    // One arm pins xs to List[int]; the annotated read `s: str = xs[0]` then mismatches.
    rejects(
        "fn main():\n c := true\n xs := []\n if c:\n  xs.push(1)\n s: str = xs[0]\nmain()",
        "cannot infer the",
    );
}

#[test]
fn refine_conflict_in_second_arm_rejects() {
    // Homogeneous first arm pins List[int]; the conflict lands in the SECOND (else-if) arm.
    rejects(
        "fn main():\n c := 1\n xs := []\n if c == 1:\n  xs.push(1)\n elif c == 2:\n  xs.push(\"s\")\nmain()",
        "cannot infer the",
    );
}

#[test]
fn refine_stmt_match_arm_conflict_rejects() {
    // Statement-`match` arms mirror if/else statements (Option B): the `1:` arm pins List[int],
    // the `_:` arm's str push is a hard cross-arm conflict.
    rejects(
        "fn main():\n c := 1\n xs := []\n match c:\n  1:\n   xs.push(1)\n  _:\n   xs.push(\"s\")\nmain()",
        "cannot infer the",
    );
}

#[test]
fn refine_loop_body_pin_then_post_loop_conflict_rejects() {
    // The for-loop body pins xs to List[int]; the post-loop str push conflicts. Accepts the
    // zero-trip / always-runs over-approximation by design (sound static over-approximation).
    rejects(
        "fn main():\n xs := []\n for i in [1,2]:\n  xs.push(i)\n xs.push(\"s\")\nmain()",
        "cannot infer the",
    );
    // while variant: condition guards the body, but the body's first-use pin still persists.
    rejects(
        "fn main():\n n := 3\n xs := []\n while n > 0:\n  xs.push(1)\n  n = n - 1\n xs.push(\"s\")\nmain()",
        "cannot infer the",
    );
}

#[test]
fn refine_zero_trip_loop_over_approximation_rejects() {
    // Zero-trip over-approximation made explicit: the loop body never runs at runtime, yet the
    // static pin still fires — `xs:=[]; for i in []: xs.push(1); xs.push("s")` REJECTS by design.
    rejects(
        "fn main():\n xs := []\n for i in []:\n  xs.push(1)\n xs.push(\"s\")\nmain()",
        "cannot infer the",
    );
}

// ---- step 6: invariants — never-refined empties, homogeneous builds, residual hole ----

#[test]
fn never_refined_empty_needs_annotation() {
    // PART A: a never-constrained empty (only read, never refined) now requires an annotation —
    // un-annotated it errors, and the annotated form is the escape hatch.
    rejects(
        "fn main():\n empty := {}\n print(empty)\nmain()",
        "cannot infer the",
    );
    ok(
        "fn main():\n empty: Map[str, int] = {}\n print(empty)\n xs: List[int] = []\n print(xs)\nmain()",
    );
}

#[test]
fn idiomatic_homogeneous_push_ok() {
    ok(
        "fn main():\n out: List[int] = []\n out.push(1)\n out.push(2)\n s: Set[str] = Set()\n s.add(\"a\")\n s.add(\"b\")\n m: Map[str, int] = {}\n m[\"k\"] = 1\n m[\"j\"] = 2\nmain()",
    );
}

#[test]
fn annotated_heterogeneous_list_ok() {
    // The intended escape hatch: an explicit List[Shape] annotation accepts mixed structs
    // sharing the protocol (refinement never engages on an already-concrete element type).
    ok(
        "protocol Shape:\n fn area(self) -> int\nstruct Sq:\n s: int\n fn area(self) -> int:\n  return self.s * self.s\nstruct Rect:\n w: int\n h: int\n fn area(self) -> int:\n  return self.w * self.h\nfn main():\n shapes: List[Shape] = []\n shapes.push(Sq(3))\n shapes.push(Rect(2, 4))\nmain()",
    );
}

#[test]
fn nonident_receiver_is_refined_through_its_root() {
    // A receiver that is a place (Index expr xss[0]) pins its root binding `xss`, so the mixed
    // push is rejected like the simple-variable one (TICKET-234).
    rejects(
        "fn main():\n xss := [[]]\n xss[0].push(1)\n xss[0].push(\"s\")\nmain()",
        "cannot infer the",
    );
}

// ---- step 7: golden-test checker-bypass fix — every shipped example type-checks ----

#[test]
fn all_shipped_examples_typecheck() {
    // The golden example tests drive run_capture, which BYPASSES the Checker — so a checker
    // regression on a shipped example would ship FALSELY GREEN. This routes every
    // examples/*.chz through the real checked path (build_graph + check_graph, mirroring
    // `chezzi check`) so example type-errors are caught from now on.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    // NO allow-list: every shipped example type-checks. (Two used to be exempt. `explicit_type_args.chz`
    // was wrongly rejected by a `name_is_generic` keying bug. `panic.chz` bound `Ok(v)` + `{v}` over a
    // `recover:` of a VOID call — that yields `Result[nil, Error]`, so the interpolation was a real
    // nil-in-value-position error, NOT the "bottom-typed `panic` in value position" the old comment
    // blamed: `x := if true: 1 else: panic(...)` checks clean on its own. Both fixed and verified here.)
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("examples dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("chz"))
        .collect();
    entries.sort();
    assert!(!entries.is_empty(), "no examples found in {dir:?}");
    let mut failures = Vec::new();
    for path in &entries {
        match crate::resolver::build_graph(path) {
            Ok(graph) => {
                if let Err(errs) = crate::checker::check_graph(&graph) {
                    failures.push(format!("{}: {:?}", path.display(), errs));
                }
            }
            Err(e) => failures.push(format!("{}: resolve/parse error: {e}", path.display())),
        }
    }
    assert!(
        failures.is_empty(),
        "these shipped examples fail `chezzi check`:\n{}",
        failures.join("\n")
    );
}

#[test]
fn all_std_files_standalone_typecheck() {
    // Std modules rely on stdlib AUTO-PRIVILEGE (e.g. `std/concurrency/collection.chz` uses bare
    // `RwShared[Map[K, V]]` field types with no `import` of its own). That privilege used to be
    // granted ONLY on the import path, so opening a std file directly (`chezzi check std/foo.chz`,
    // or an editor/LSP) reported phantom "unknown type 'RwShared'" errors. The path-aware
    // `LoadedModule::is_std` now grants it on the standalone-entry path too — assert every shipped
    // `std/**/*.chz` type-checks clean as an ENTRY (build_graph + check_graph, mirroring `chezzi check`).
    fn collect_chz(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("std dir readable").flatten() {
            let p = entry.path();
            if p.is_dir() {
                collect_chz(&p, out);
            } else if p.extension().and_then(|s| s.to_str()) == Some("chz") {
                out.push(p);
            }
        }
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("std");
    let mut entries = Vec::new();
    collect_chz(&dir, &mut entries);
    entries.sort();
    assert!(!entries.is_empty(), "no std files found in {dir:?}");
    let mut failures = Vec::new();
    for path in &entries {
        match crate::resolver::build_graph(path) {
            Ok(graph) => {
                if let Err(errs) = crate::checker::check_graph(&graph) {
                    failures.push(format!("{}: {:?}", path.display(), errs));
                }
            }
            Err(e) => failures.push(format!("{}: resolve/parse error: {e}", path.display())),
        }
    }
    assert!(
        failures.is_empty(),
        "these std files fail standalone `chezzi check`:\n{}",
        failures.join("\n")
    );
}

#[test]
fn all_bench_corpus_typecheck() {
    // The bench corpus `benches/chz/*.chz` is run by `benches/run.chz` via `chezzi run`, which does
    // a pre-run type check (src/main.rs) and BLOCKS execution on any type error. `cargo test` does
    // not otherwise exercise these files, so a stale lowercase `list[...]`/`map[...]`/`set[...]`
    // type annotation (now rejected after the hard list->List/map->Map/set->Set rename) would ship
    // FALSELY GREEN and break the documented perf-measurement entrypoint. Route every bench file
    // through the real checked path so such regressions are caught.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benches")
        .join("chz");
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("benches/chz dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("chz"))
        .collect();
    entries.sort();
    assert!(!entries.is_empty(), "no bench corpus found in {dir:?}");
    let mut failures = Vec::new();
    for path in &entries {
        match crate::resolver::build_graph(path) {
            Ok(graph) => {
                if let Err(errs) = crate::checker::check_graph(&graph) {
                    failures.push(format!("{}: {:?}", path.display(), errs));
                }
            }
            Err(e) => failures.push(format!("{}: resolve/parse error: {e}", path.display())),
        }
    }
    assert!(
        failures.is_empty(),
        "these bench corpus files fail `chezzi check`:\n{}",
        failures.join("\n")
    );
}

// === Import name collisions (soundness) + duplicate binder in one pattern ===

#[test]
fn same_named_types_from_two_modules_are_qualified_in_one_message() {
    // TICKET-149 (3): two DIFFERENT `Col`s in one message print module-qualified, as Go does
    // (`cannot use b.Red (constant of type b.Col) as a.Col value`).
    let errs = check_files(&[
        ("pkg/a.chz", "enum Col:\n    Red\n    Blue\n"),
        ("pkg/b.chz", "enum Col:\n    Red\n    Green\n"),
        (
            "main.chz",
            "import Col from pkg.a\nimport Col as BCol from pkg.b\nfn f(c: Col) -> int: 1\nprint(f(BCol.Red))\nc: Col = BCol.Red\nfn g() -> Col:\n    return BCol.Red\nxs: List[Col] = [BCol.Red]\nprint(Col.Red == BCol.Red)\n",
        ),
    ]);
    for want in [
        "argument 1 of 'f': expected a.Col, found b.Col",
        "cannot assign b.Col to variable of type a.Col",
        "expected return type a.Col, found b.Col",
        "list element: expected a.Col, found b.Col",
        "cannot compare a.Col and b.Col for equality",
    ] {
        assert!(
            errs.iter().any(|e| e.message.contains(want)),
            "expected a message containing {want:?}, got: {errs:?}"
        );
    }
}

#[test]
fn a_message_without_a_name_collision_stays_bare() {
    // Only ONE `Col` is in the message, so nothing is qualified.
    let errs = check_files(&[
        ("pkg/a.chz", "enum Col:\n    Red\n    Blue\n"),
        ("pkg/b.chz", "enum Col:\n    Red\n    Green\n"),
        (
            "main.chz",
            "import Col from pkg.a\nimport Col as BCol from pkg.b\nfn f(c: Col) -> int: 1\nprint(f(1))\n",
        ),
    ]);
    assert!(
        errs.iter()
            .any(|e| e.message == "argument 1 of 'f': expected Col, found int"),
        "got: {errs:?}"
    );
}

#[test]
fn same_named_types_whose_module_names_collide_use_the_full_path() {
    // `x.m` and `y.m` share the last segment `m`, so `m.Col` twice would still be ambiguous.
    let errs = check_files(&[
        ("x/m.chz", "enum Col:\n    Red\n"),
        ("y/m.chz", "enum Col:\n    Red\n"),
        (
            "main.chz",
            "import Col from x.m\nimport Col as YCol from y.m\nfn f(c: Col) -> int: 1\nprint(f(YCol.Red))\n",
        ),
    ]);
    assert!(
        errs.iter()
            .any(|e| e.message.contains("expected x.m.Col, found y.m.Col")),
        "got: {errs:?}"
    );
}

#[test]
fn extern_fn_reachable_as_module_member() {
    // TICKET-061 W10-14: an extern fn is a module-global callable (docs/syntax.md) and functions
    // export by default, but `capture_sig` has no `StmtKind::Extern` arm, so it never lands in
    // `sig.members` — an importer sees "module 'c' has no member 'strlen'" even though `c.chz`
    // declares `extern fn strlen`.
    files_ok(&[
        ("c.chz", "extern \"libc\":\n    fn strlen(s: str) -> int\n"),
        ("main.chz", "import c\nprint(c.strlen(\"abc\"))\n"),
    ]);
}

#[test]
fn extern_fn_reachable_via_from_import() {
    // TICKET-061 W10-14: the `import strlen from c` spelling must resolve too — same `capture_sig`
    // gap as the whole-module form above.
    files_ok(&[
        ("c.chz", "extern \"libc\":\n    fn strlen(s: str) -> int\n"),
        ("main.chz", "import strlen from c\nprint(strlen(\"abc\"))\n"),
    ]);
}

/// **The `Eq` walk must read the field/payload tables through the MISS-ONLY owning-module helpers
/// (gap #4).** A named-fn import (`import make from lib`) hands back a value whose `Ty::Struct` key
/// belongs to the DEFINING module and injects nothing into the importer's `structs` table, so the
/// bare-table read the first cut used found no fields, walked nothing, and GRANTED `Eq` to a type
/// whose field reaches an unmet `where` bound — check-clean, then `Tag has no 'compare'` at runtime.
///
/// The whole-module import of the identical program is the control: it always rejected, so the two
/// import forms disagreeing IS the bug. Both spellings must produce the same verdict AND the same
/// message. (`ok()`/`entry_ok()` cannot see this at all — it needs the real `check_graph` path.)
#[test]
fn eq_walk_sees_fields_through_every_import_form() {
    const LIB: &str = "\
struct Tag:
    n: int
struct Box[T]:
    val: T
    fn compare(self, other: Self) -> int where T: Comparable:
        return 0
    fn eq(self, other: Self) -> bool where T: Comparable:
        return self.compare(other) == 0
struct Holder:
    b: Box[Tag]
enum Carrier:
    Empty
    Has(Box[Tag])
fn make(n: int) -> Holder:
    return Holder(Box(Tag(n)))
fn carry(n: int) -> Carrier:
    return Carrier.Has(Box(Tag(n)))
";
    const NEEDLE: &str = "'s `eq` requires Tag: Comparable";
    for (label, main) in [
        (
            "named-fn import (struct field)",
            "import make from lib\nfn needs[U: Eq](a: U, b: U) -> bool:\n    return a == b\nprint(needs(make(1), make(2)))\n",
        ),
        (
            "whole-module import (struct field) — the control",
            "import lib\nfn needs[U: Eq](a: U, b: U) -> bool:\n    return a == b\nprint(needs(lib.make(1), lib.make(2)))\n",
        ),
        (
            "named-fn import (enum payload)",
            "import carry from lib\nfn needs[U: Eq](a: U, b: U) -> bool:\n    return a == b\nprint(needs(carry(1), carry(2)))\n",
        ),
        (
            "whole-module import (enum payload) — the control",
            "import lib\nfn needs[U: Eq](a: U, b: U) -> bool:\n    return a == b\nprint(needs(lib.carry(1), lib.carry(2)))\n",
        ),
    ] {
        let errs = check_files(&[("lib.chz", LIB), ("main.chz", main)]);
        assert!(
            errs.iter().any(|e| e.message.contains(NEEDLE)),
            "{label}: expected an error containing {NEEDLE:?}, got: {errs:?}"
        );
    }
    // …and a SOUND cross-module type is still granted through both forms (the fix must not simply
    // refuse everything it cannot see locally).
    const OK_LIB: &str = "struct P:\n    n: int\nfn mk(n: int) -> P:\n    return P(n)\n";
    files_ok(&[
        ("lib.chz", OK_LIB),
        (
            "main.chz",
            "import mk from lib\nfn needs[U: Eq](a: U, b: U) -> bool:\n    return a == b\nprint(needs(mk(1), mk(2)))\n",
        ),
    ]);
}

/// **The `Eq` cycle guard must key on the IDENTITY key, not the display name.** Two modules may each
/// declare a `struct H`; `Ty`'s `Display` renders both as `H` (deliberately — user-facing messages
/// show bare names), so a `Display`-keyed in-progress stack thinks `b.H` is already being proven the
/// moment `a.H` is, applies the coinductive assumption to a type that was never in progress, and
/// GRANTS `Eq` to one whose field reaches an unmet `where` bound. Check-clean, then
/// `Tag has no 'compare'` at runtime.
///
/// `Ty: PartialEq` compares the module-scoped key (`a::H`), which is why the guard holds the `Ty`
/// itself. **The NAME is the only variable**, and the two controls below prove it: renaming the
/// second struct, and collapsing the same shape into one module with distinct names, both rejected
/// even while the collision was being granted.
#[test]
fn the_eq_cycle_guard_keys_on_identity_not_display_name() {
    const A: &str = "\
struct Tag:
    n: int
struct Bad[T]:
    val: T
    fn compare(self, other: Self) -> int where T: Comparable:
        return 0
    fn eq(self, other: Self) -> bool where T: Comparable:
        return self.compare(other) == 0
struct H:
    v: Bad[Tag]
fn mk_a(n: int) -> H:
    return H(Bad(Tag(n)))
";
    let b = |name: &str| {
        format!(
            "import a\nstruct C[T]:\n    v: T\n    fn eq(self, other: Self) -> bool where T: Eq:\n        return self.v == other.v\nstruct {name}:\n    v: C[a.H]\nfn mk_b(n: int) -> {name}:\n    return {name}(C(a.mk_a(n)))\n"
        )
    };
    const MAIN: &str = "import b\nfn needs[U: Eq](x: U, y: U) -> bool:\n    return x == y\nprint(needs(b.mk_b(1), b.mk_b(2)))\n";
    // THE COLLISION: `b.H` shares a display name with `a.H`, which it reaches.
    files_reject(
        &[("a.chz", A), ("b.chz", &b("H")), ("main.chz", MAIN)],
        "does not satisfy Eq",
    );
    // CONTROL 1 — identical program, second struct renamed. Rejected even before the fix.
    files_reject(
        &[("a.chz", A), ("b.chz", &b("H2")), ("main.chz", MAIN)],
        "type H2 does not satisfy Eq (C[H]'s `eq` requires H: Eq)",
    );
    // CONTROL 2 — the same shape in ONE module with distinct names. Also always rejected.
    entry_rejects(
        &format!(
            "{A}struct C[T]:\n    v: T\n    fn eq(self, other: Self) -> bool where T: Eq:\n        return self.v == other.v\nstruct H2:\n    v: C[H]\nfn needs[U: Eq](x: U, y: U) -> bool:\n    return x == y\nfn use_it(p: H2, q: H2) -> bool:\n    return needs(p, q)\n"
        ),
        "type H2 does not satisfy Eq (C[H]'s `eq` requires H: Eq)",
    );
}

#[test]
fn struct_match_qualified_whole_module_ok() {
    // Bug #1: a struct reached via a WHOLE-module import (`import geo`, referenced `geo.Point`) is
    // only spellable qualified — the bare name `Point` is not in scope. The qualified struct pattern
    // `geo.Point(x, y)` must type-check (symmetric with qualified construction `geo.Point(3, 4)`).
    files_ok(&[
        ("geo.chz", "struct Point:\n    x: int\n    y: int\n"),
        (
            "main.chz",
            "import geo\nfn f(p: geo.Point) -> int:\n    match p:\n        geo.Point(x, y): return x + y\n",
        ),
    ]);
}

/// UNSOUND case: `import v from vmod` (a value) then `import v from fmod` (a fn) — checker resolved
/// `v` to the value, runtime binds the function → `v + 1` faulted at runtime. Must be a check error.
#[test]
fn import_value_then_fn_same_name_rejected() {
    files_reject(
        &[
            ("vmod.chz", "v := 42\n"),
            ("fmod.chz", "fn v() -> int:\n    return 7\n"),
            (
                "main.chz",
                "import v from vmod\nimport v from fmod\nfn main():\n    print(v + 1)\n",
            ),
        ],
        "is already imported",
    );
}

/// WRONG/SILENT case: two `fn f` from different modules, last-wins, no error. Must be a check error.
#[test]
fn duplicate_from_import_fn_rejected() {
    files_reject(
        &[
            ("lib.chz", "fn f() -> int:\n    return 1\n"),
            ("lib2.chz", "fn f() -> int:\n    return 99\n"),
            (
                "main.chz",
                "import f from lib\nimport f from lib2\nprint(f())\n",
            ),
        ],
        "is already imported",
    );
}

/// Two structs of the same name from different modules. Must be a check error.
#[test]
fn duplicate_from_import_struct_rejected() {
    files_reject(
        &[
            ("a.chz", "struct P:\n    x: int\n"),
            ("b.chz", "struct P:\n    y: int\n"),
            ("main.chz", "import P from a\nimport P from b\n"),
        ],
        "is already imported",
    );
}

/// Distinct from-imports must still pass.
#[test]
fn distinct_from_imports_ok() {
    files_ok(&[
        ("lib.chz", "fn f() -> int:\n    return 1\n"),
        ("lib2.chz", "fn g() -> int:\n    return 2\n"),
        (
            "main.chz",
            "import f from lib\nimport g from lib2\nprint(f() + g())\n",
        ),
    ]);
}

/// Two whole-module imports with distinct bind names (one aliased) must still pass.
#[test]
fn import_module_as_alias_ok() {
    files_ok(&[
        ("a.chz", "fn f() -> int:\n    return 1\n"),
        ("b.chz", "fn f() -> int:\n    return 2\n"),
        (
            "main.chz",
            "import a\nimport b as c\nprint(a.f() + c.f())\n",
        ),
    ]);
}

/// `import v as w` + `import f as w` (both alias to `w`) must collide on the bind name.
#[test]
fn import_alias_collision_rejected() {
    files_reject(
        &[
            ("vmod.chz", "v := 42\n"),
            ("fmod.chz", "fn f() -> int:\n    return 7\n"),
            (
                "main.chz",
                "import v as w from vmod\nimport f as w from fmod\n",
            ),
        ],
        "is already imported",
    );
}

/// A tuple pattern binding the same name twice must be a compile error.
#[test]
fn duplicate_tuple_pattern_binder_rejected() {
    rejects(
        "fn f(t: (int, int)) -> int:\n    match t:\n        (x, x): return x\n        _: return -1\n",
        "is bound more than once",
    );
}

/// An enum-variant payload binding the same name twice must be a compile error.
#[test]
fn duplicate_enum_payload_binder_rejected() {
    rejects(
        "enum E:\n    V(int, int)\nfn f(e: E) -> int:\n    match e:\n        E.V(a, a): return a\n        _: return -1\n",
        "is bound more than once",
    );
}

/// A nested duplicate binder (tuple inside a variant) must be caught too.
#[test]
fn duplicate_nested_pattern_binder_rejected() {
    rejects(
        "enum E:\n    V((int, int))\nfn f(e: E) -> int:\n    match e:\n        E.V((a, a)): return a\n        _: return -1\n",
        "is bound more than once",
    );
}

/// A binder shared between an OUTER slot and a nested OR-pattern is a single-pattern duplicate
/// (the or binds `x` and the outer tuple slot binds `x` → `x` twice on a matching path).
#[test]
fn duplicate_binder_across_outer_and_or_rejected() {
    rejects(
        "enum E:\n    A(int)\n    B(int)\nfn f(t: (int, E)) -> int:\n    match t:\n        (x, E.A(x) | E.B(x)): return 100\n        _: return -1\n",
        "is bound more than once",
    );
}

/// A duplicate INSIDE one or-alternative must still be caught.
#[test]
fn duplicate_binder_within_or_alternative_rejected() {
    rejects(
        "enum E:\n    V(int, int)\n    W(int)\nfn f(e: E) -> int:\n    match e:\n        E.V(a, a) | E.W(a): return a\n        _: return -1\n",
        "is bound more than once",
    );
}

/// Cross-alternative reuse of the SAME name (`A(x) | B(x)`) is NOT a duplicate — each alternative
/// is its own binding context (guards the or-fix against over-rejection).
#[test]
fn or_alternatives_reuse_binder_ok() {
    ok(
        "enum E:\n    A(int)\n    B(int)\nfn f(e: E) -> int:\n    match e:\n        E.A(x) | E.B(x): return x\n        _: return -1\n",
    );
}

/// `_` repeated in a pattern binds nothing → must still be OK.
#[test]
fn wildcard_repeated_in_pattern_ok() {
    ok("fn f(t: (int, int)) -> int:\n    match t:\n        (_, _): return 0\n");
}

/// The same binder name reused across SEPARATE arms is fine (distinct scopes).
#[test]
fn same_binder_across_arms_ok() {
    ok("fn f(t: int) -> int:\n    match t:\n        0: return 0\n        x: return x\n");
}

/// An or-pattern binding the same names across DIFFERENT alternatives is legal.
#[test]
fn or_pattern_same_names_across_alts_ok() {
    ok(
        "enum E:\n    A(int)\n    B(int)\nfn f(e: E) -> int:\n    match e:\n        E.A(x) | E.B(x): return x\n",
    );
}
