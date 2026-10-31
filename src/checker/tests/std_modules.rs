//! Checker tests: std.process/fs/time/regex/request/encoding/concurrency file-backed modules,
//! json.decode, set.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== M8-M3: native trio std.process / std.fs / std.time signatures =====

#[test]
fn native_process_cmd_returns_result_str() {
    entry_ok(
        "import std.process\nfn main():\n    match process.cmd(\"echo hi\"):\n        ?s: print(s)\n        !e: print(e)\n",
    );
}

#[test]
fn native_process_cmd_arg_must_be_str() {
    entry_rejects(
        "import std.process\nfn main():\n    print(process.cmd(5))\n",
        "argument 1 of 'cmd'",
    );
}

#[test]
fn native_process_run_returns_result_proc_result() {
    entry_ok(
        "import std.process\nfn main():\n    match process.run(\"echo hi\"):\n        ?r:\n            o: str = r.stdout\n            e: str = r.stderr\n            c: int = r.code\n            print(o + e + str(c))\n        !msg: print(msg)\n",
    );
}

#[test]
fn native_process_run_args_takes_prog_and_list_str() {
    entry_ok(
        "import std.process\nfn main():\n    match process.run_args(\"echo\", [\"a\", \"b\"]):\n        ?r: print(r.stdout + str(r.code))\n        !msg: print(msg)\n",
    );
}

#[test]
fn native_process_run_args_argv_must_be_list_str() {
    entry_rejects(
        "import std.process\nfn main():\n    print(process.run_args(\"echo\", \"notalist\"))\n",
        "argument 2 of 'run_args'",
    );
}

#[test]
fn list_comprehension_infers_element_type() {
    // `[x * 2 for x in [1, 2, 3]]` is a `List[int]` — the loop var binds to the list's element.
    ok("xs: List[int] = [x * 2 for x in [1, 2, 3]]\n");
}

#[test]
fn list_comprehension_wrong_element_type_rejected() {
    rejects("xs: List[str] = [x * 2 for x in [1, 2, 3]]\n", "List[int]");
}

#[test]
fn comprehension_guard_must_be_bool() {
    rejects(
        "xs := [x for x in [1, 2, 3] if x]\n",
        "comprehension guard must be bool",
    );
}

#[test]
fn list_comprehension_over_range_is_list_int() {
    ok("xs: List[int] = [x * x for x in 0..10]\n");
}

#[test]
fn set_comprehension_infers_element_type() {
    ok("s: Set[int] = {x for x in [1, 2, 3]}\n");
}

#[test]
fn map_comprehension_over_map_entries() {
    ok("src: Map[str, int] = {\"a\": 1}\nm: Map[str, int] = {k: v for k, v in src}\n");
}

#[test]
fn comprehension_var_out_of_scope_after() {
    // The loop variable is scoped to the comprehension; referencing it afterward is unknown.
    rejects("xs := [x for x in [1, 2, 3]]\nprint(x)\n", "x");
}

#[test]
fn nested_comp_later_clause_sees_earlier_var_typechecks() {
    // The second clause's iterable references the first clause's binding (a list-of-lists flatten).
    ok("ys: List[int] = [y for xs in [[1, 2], [3]] for y in xs]\n");
}

#[test]
fn nested_comp_two_clause_element_type() {
    ok("ps: List[int] = [x + y for x in [1, 2] for y in [10, 20]]\n");
}

#[test]
fn nested_comp_unbound_in_later_clause_errors() {
    // `zzz` is bound nowhere; a later clause's iterable must still resolve names normally.
    rejects(
        "ys := [y for x in [1, 2] for y in zzz]\n",
        "unknown name 'zzz'",
    );
}

#[test]
fn nested_comp_guard_after_nonfinal_clause_typechecks() {
    ok("ps: List[int] = [x * y for x in [1, 2, 3] if x > 0 for y in [10, 20]]\n");
}

#[test]
fn nested_comp_channel_in_later_clause_checks() {
    // A channel in a later clause is an iterable like any other.
    entry_ok("fn main():\n    ch := Channel[int]()\n    xs := [c for x in [1, 2] for c in ch]\n");
}

#[test]
fn native_os_exit_takes_int() {
    entry_ok("import std.os\nfn main():\n    os.exit(1)\n");
}

#[test]
fn native_os_exit_arg_must_be_int() {
    entry_rejects(
        "import std.os\nfn main():\n    os.exit(\"nope\")\n",
        "argument 1 of 'exit'",
    );
}

#[test]
fn native_fs_predicates_are_bool_and_size_is_result_int() {
    entry_ok(
        "import std.fs\nfn main():\n    b: bool = fs.is_file(\"x\")\n    e: bool = fs.exists(\"x\")\n    match fs.size(\"x\"):\n        ?n: print(str(n))\n        !m: print(m)\n",
    );
}

#[test]
fn native_fs_list_dir_returns_result_list_path() {
    // W7-8 — `list_dir` hands back `Result[List[path.Path]]`; `.str()` is the lossy display.
    entry_ok(
        "import std.fs\nimport std.path\nfn main():\n    match fs.list_dir(\".\"):\n        ?xs: print(xs[0].str())\n        !e: print(e)\n",
    );
}

#[test]
fn path_ctor_accepts_str_like_every_other_path_fn() {
    // Every free fn in std.path takes PathLike (str/bytes/bytearray/Path), but the struct's
    // auto-generated ctor demands `raw: bytes` exactly, so the static `Path.new` takes the same
    // literal that `path.join(["/a/b"])` and `path.is_abs("/a/b")` accept.
    entry_ok("import std.path\nfn main():\n    p := path.Path.new(\"/a/b\")\n    print(p.str())\n");
}

#[test]
fn path_ctor_still_takes_bytes() {
    // TICKET-029 — the fix must not break the pre-existing bytes spelling.
    entry_ok("import std.path\nfn main():\n    p := path.Path(b\"/a/b\")\n    print(p.str())\n");
}

#[test]
fn path_ctor_rejects_a_non_pathlike() {
    // `Path.new` names PathLike, not bytes, in its rejection.
    let errs = check_entry("import std.path\nfn main():\n    p := path.Path.new(3)\n");
    assert_eq!(errs.len(), 1, "expected one error, got: {errs:?}");
    assert!(
        errs[0].message.contains("argument 1 of 'new'")
            && errs[0].message.contains("expected PathLike"),
        "got: {}",
        errs[0].message
    );
}

#[test]
fn path_value_still_satisfies_pathlike() {
    // A Path built through the raw ctor satisfies PathLike structurally.
    entry_ok(
        "import std.path\nimport std.fs\nfn main():\n    p := path.Path(b\"/a\")\n    print(fs.exists(p))\n",
    );
}

#[test]
fn path_from_imported_ctor_is_usable_as_a_type() {
    // A from-imported `Path` binds the type (annotation) and its static `new`.
    entry_ok(
        "import Path from std.path\nfn main():\n    p: Path = Path.new(\"/a/b\")\n    print(p.bytes())\n",
    );
}

#[test]
fn path_from_import_still_reserves_the_name_against_a_user_struct() {
    // TICKET-029 — the Builtin-origin reservation must still be bound for a colliding from-import
    // even though std.path now also has a same-named fn.
    entry_rejects(
        "import Path from std.path\nstruct Path:\n    x: int\nfn main():\n    print(1)\nmain()\n",
        "'Path' is already imported",
    );
}

#[test]
fn native_fs_mutations_typecheck_as_result_nil() {
    entry_ok(
        "import std.fs\nfn main():\n    match fs.mkdir(\"d\"):\n        ?_: print(\"made\")\n        !e: print(e)\n    match fs.append(\"f\", \"x\"):\n        ?_: print(\"app\")\n        !e: print(e)\n    match fs.rename(\"a\", \"b\"):\n        ?_: print(\"ren\")\n        !e: print(e)\n    match fs.copy(\"a\", \"b\"):\n        ?_: print(\"cp\")\n        !e: print(e)\n    match fs.remove_file(\"f\"):\n        ?_: print(\"rmf\")\n        !e: print(e)\n    match fs.remove_dir(\"d\"):\n        ?_: print(\"rmd\")\n        !e: print(e)\n",
    );
}

#[test]
fn native_fs_mutation_wrong_arity_rejected() {
    entry_rejects(
        "import std.fs\nfn main():\n    print(str(fs.mkdir(\"d\", \"extra\")))\n",
        "mkdir",
    );
}

#[test]
fn native_fs_unknown_member_rejected() {
    entry_rejects(
        "import std.fs\nfn main():\n    print(fs.touch(\"x\"))\n",
        "has no member 'touch'",
    );
}

// ===== M9: std.regex (Match struct) =====

#[test]
fn native_regex_is_match_returns_result_bool() {
    entry_ok(
        "import std.regex\nfn main():\n    match regex.is_match(\"x\", \"xy\"):\n        ?b:\n            if b:\n                print(\"yes\")\n        !e: print(e)\n",
    );
}

#[test]
fn native_regex_find_returns_match_with_typed_fields() {
    entry_ok(
        "import std.regex\nfn main():\n    match regex.find(\"[0-9]+\", \"a12\"):\n        ?opt:\n            match opt:\n                ?m:\n                    t: str = m.text\n                    st: int = m.start\n                    g: List[str] = m.groups\n                    print(t + str(st) + \",\".join(g))\n                None: print(\"none\")\n        !e: print(e)\n",
    );
}

#[test]
fn native_regex_find_all_returns_result_list_match() {
    entry_ok(
        "import std.regex\nfn main():\n    match regex.find_all(\"[0-9]+\", \"1 2\"):\n        ?ms:\n            for m in ms:\n                print(m.text)\n        !e: print(e)\n",
    );
}

#[test]
fn native_regex_split_and_replace_all_return_strings() {
    entry_ok(
        "import std.regex\nfn main():\n    match regex.replace_all(\"a\", \"banana\", \"o\"):\n        ?s: print(s)\n        !e: print(e)\n    match regex.split(\",\", \"a,b\"):\n        ?xs: print(\"|\".join(xs))\n        !e: print(e)\n",
    );
}

#[test]
fn native_regex_match_unknown_field_rejected() {
    entry_rejects(
        "import std.regex\nfn main():\n    match regex.find(\"a\", \"a\"):\n        ?opt:\n            match opt:\n                ?m: print(m.nope)\n                None: print(\"\")\n        !e: print(e)\n",
        "has no field 'nope'",
    );
}

#[test]
fn native_regex_unknown_member_rejected() {
    entry_rejects(
        "import std.regex\nfn main():\n    print(regex.compile(\"x\"))\n",
        "has no member 'compile'",
    );
}

// ===== Phase 4b: std.regex is now a FILE-BACKED `std/regex.chz` whose `native struct Match` + 5
// `native fn`s are declared in-module. Both the phase-4a companion stub (std/regex.stub.chz) and the
// `native_module_sig` regex arm are RETIRED — the checker harvests the module's whole SIGNATURE (type +
// fns) from the parsed in-module decls via `harvest_native_module`. Behavior is byte-identical. =====

/// Parse the real `std/regex.chz` (the SIGNATURE source for phase 4b).
#[cfg(test)]
fn parse_regex_chz() -> crate::ast::Module {
    let path = crate::resolver::std_root().join("regex.chz");
    let src = std::fs::read_to_string(&path).expect("std/regex.chz must exist");
    let toks = crate::lexer::tokenize(&src).expect("std/regex.chz must lex");
    crate::parser::parse(toks).expect("std/regex.chz must parse")
}

/// The std.regex ModuleSig produced by a full graph check that `import std.regex`.
#[cfg(test)]
fn regex_module_sig_via_graph() -> ModuleSig {
    native_module_sig_via_graph("regex")
}

#[test]
fn harvest_native_method_own_type_params() {
    // A native method's own `[U]` lands on the harvested `FnSig.type_params`, and `U` resolves to
    // `Ty::Param("U")` in the method's params/ret (nested inside the struct's `[T]` scope).
    let src = "native struct Foo[T]:\n    native fn map[U](self, f: fn(T) -> U) -> List[U]\n";
    let module = parser::parse(lexer::tokenize(src).unwrap()).unwrap();
    let mut c = Checker::new();
    let info = c
        .harvest_native_struct_table(&module, "Foo")
        .expect("harvest Foo");
    let sig = info.methods.get("map").expect("Foo.map harvested");
    assert_eq!(sig.type_params.len(), 1, "method own [U] on the sig");
    assert_eq!(sig.type_params[0].name, "U");
    // `self` stripped → one param `fn(T) -> U`, with U as a `Ty::Param`.
    assert_eq!(sig.params.len(), 1);
    match &sig.params[0] {
        Ty::Func { params, ret, .. } => {
            assert_eq!(params, &vec![Ty::Param("T".to_string())]);
            assert_eq!(**ret, Ty::Param("U".to_string()));
        }
        other => panic!("expected fn param, got {other}"),
    }
    assert_eq!(sig.ret, Ty::list(Ty::Param("U".to_string())));
}

/// PROVENANCE — the whole regex signature (Match type + the 5 fns) comes from parsing
/// `std/regex.chz` (no Rust-side sig exists): a full graph check that `import std.regex` resolves both
/// the fns and `Match`.
#[test]
fn regex_sig_from_file_not_native_module_sig() {
    // A full graph check that imports std.regex still resolves regex.Match + typed fields.
    entry_ok(
        "import std.regex\nfn main():\n    m: regex.Match = regex.Match(\"x\", 0, 1, [])\n    t: str = m.text\n    s: int = m.start\n    g: List[str] = m.groups\n    print(t + str(s) + \",\".join(g))\n",
    );
}

/// The 6 regex fn FnSigs + the Match StructInfo in the module's ModuleSig must EXACTLY equal what
/// `native_module_sig` used to hand-build — byte-identical provenance move from the deleted arm to the
/// parsed `std/regex.chz`. (`params`/`ret` compared directly; labels/doc are surface-only.)
#[test]
fn regex_fn_sigs_exact() {
    let sig = regex_module_sig_via_graph();
    let m = || Ty::Struct("Match".to_string(), vec![]);
    let expected: Vec<(&str, Vec<Ty>, Ty)> = vec![
        ("is_match", vec![Ty::Str, Ty::Str], Ty::result(Ty::Bool)),
        ("find", vec![Ty::Str, Ty::Str], Ty::result(Ty::option(m()))),
        (
            "find_all",
            vec![Ty::Str, Ty::Str],
            Ty::result(Ty::list(m())),
        ),
        (
            "find_all_text",
            vec![Ty::Str, Ty::Str],
            Ty::result(Ty::list(Ty::Str)),
        ),
        (
            "replace_all",
            vec![Ty::Str, Ty::Str, Ty::Str],
            Ty::result(Ty::Str),
        ),
        (
            "split",
            vec![Ty::Str, Ty::Str],
            Ty::result(Ty::list(Ty::Str)),
        ),
    ];
    assert_eq!(
        sig.fns().count(),
        expected.len(),
        "std.regex must export exactly the 6 regex fns"
    );
    for (name, params, ret) in &expected {
        let fs = sig
            .certain_fn(name)
            .unwrap_or_else(|| panic!("std.regex ModuleSig missing fn '{name}'"));
        assert_eq!(&fs.params, params, "fn '{name}' params drifted");
        assert_eq!(&fs.ret, ret, "fn '{name}' return drifted");
        assert_eq!(fs.min_params, params.len(), "fn '{name}' arity drifted");
    }
    // Match's StructInfo comes from the in-module `native struct`, origin Builtin (load-bearing for the
    // both-engines pure-type `bind_import` skip).
    let mi = sig
        .struct_defs
        .get("Match")
        .expect("std.regex ModuleSig must export Match");
    assert_eq!(
        mi.fields,
        vec![
            ("text".to_string(), Ty::Str),
            ("start".to_string(), Ty::Int),
            ("end".to_string(), Ty::Int),
            ("groups".to_string(), Ty::list(Ty::Str)),
        ]
    );
    assert!(matches!(mi.origin, StructOrigin::Builtin));
    assert!(sig.types.contains("Match"));
}

/// DRIFT GUARD — the file-harvested Match StructInfo (fields, in positional order) must byte-match the
/// remaining hand-built layout copies. Field ORDER is load-bearing (positional across compiler
/// `Compiler::new`, interp finalize, `native/regex.rs` match_to_ret, and seed_stdlib_structs), so a
/// silent drift here would trap at runtime on a field read. Assemble the expected layout once and
/// compare both the file harvest and seed_stdlib_structs against it.
#[test]
fn regex_chz_match_matches_handbuilt_layouts() {
    let expected: Vec<(String, Ty)> = vec![
        ("text".into(), Ty::Str),
        ("start".into(), Ty::Int),
        ("end".into(), Ty::Int),
        ("groups".into(), Ty::list(Ty::Str)),
    ];
    // The in-module `native struct` harvest (from the real std/regex.chz).
    let mut c = Checker::new();
    let mut sig = ModuleSig::default();
    let ast = parse_regex_chz();
    c.harvest_native_module(&ast, &mut sig);
    let harvested = sig
        .struct_defs
        .get("Match")
        .expect("std/regex.chz must harvest a `Match` struct");
    assert_eq!(harvested.fields, expected, "harvested Match layout drifted");
    assert!(harvested.type_params.is_empty());
    assert!(harvested.methods.is_empty());
    assert!(matches!(harvested.origin, StructOrigin::Builtin));
    // The harvest must ALSO have populated the 6 fn sigs (whole-module signature source).
    assert_eq!(sig.fns().count(), 6, "std/regex.chz must harvest 6 fns");
    // seed_stdlib_structs's hand-built Match copy (globally-present layout) must agree.
    let seeded = c.structs.get("Match").expect("Match must be seeded");
    assert_eq!(
        seeded.fields, expected,
        "seeded Match layout drifted from std/regex.chz"
    );
    // The harvest must NOT leak `Match` into module-scoped `struct_names` (import-gating preserved):
    // the transient insert during fn-sig resolution is removed.
    assert!(
        !c.struct_names.contains("Match"),
        "harvest_native_module must not leave Match bare-visible (import-gating leak)"
    );
}

// ===== Phase 4f: std.process / std.request are FILE-BACKED (`std/process.chz`, `std/request.chz`)
// whose fields-only `native struct` (ProcResult / Response) + `native fn`s are declared in-module and
// harvested via `harvest_native_module`, retiring BOTH their `native_module_sig` fn arms AND their
// `export_struct` type arms. The one subtlety over regex: get/post/request carry an OPTIONAL trailing
// `timeout_ms: int = 0`, which harvest lowers to `min_params` from the trailing default. =====

/// Parse a native-module source string into an AST (the SIGNATURE source for a file-backed module).
#[cfg(test)]
fn parse_native_src(src: &str) -> crate::ast::Module {
    let toks = crate::lexer::tokenize(src).expect("native module src must lex");
    crate::parser::parse(toks).expect("native module src must parse")
}

/// Parse the real `std/process.chz` (the phase-4f SIGNATURE source).
#[cfg(test)]
fn parse_process_chz() -> crate::ast::Module {
    let path = crate::resolver::std_root().join("process.chz");
    let src = std::fs::read_to_string(&path).expect("std/process.chz must exist");
    parse_native_src(&src)
}

/// Parse the real `std/request.chz` (the phase-4f SIGNATURE source).
#[cfg(test)]
fn parse_request_chz() -> crate::ast::Module {
    let path = crate::resolver::std_root().join("request.chz");
    let src = std::fs::read_to_string(&path).expect("std/request.chz must exist");
    parse_native_src(&src)
}

/// HARVEST — a `native fn` whose trailing param carries a `= default` marker lowers to an
/// optional-tail FnSig (min_params = len - trailing-defaults). Byte-identical to the deleted
/// hand-built install for std.request's get/post/request.
#[test]
fn harvest_optional_tail_from_trailing_default() {
    let ast = parse_native_src("native fn f(a: str, b: int = 0) -> bool!\n");
    let mut c = Checker::new();
    let mut sig = ModuleSig::default();
    c.harvest_native_module(&ast, &mut sig);
    let fs = sig.certain_fn("f").expect("harvest must export fn f");
    assert_eq!(fs.params, vec![Ty::Str, Ty::Int], "params drifted");
    assert_eq!(fs.min_params, 1, "trailing default → min_params = len-1");
    assert_eq!(fs.params.len(), 2);
    // A fn with NO defaults stays plain (min_params == len).
    let ast2 = parse_native_src("native fn g(a: str, b: int) -> bool!\n");
    let mut sig2 = ModuleSig::default();
    c.harvest_native_module(&ast2, &mut sig2);
    let gs = sig2.certain_fn("g").unwrap();
    assert_eq!(gs.min_params, 2, "no defaults → min_params == len");
}

/// PROVENANCE + exact sigs — std.process's 5 fns + the `ProcResult` StructInfo come from the
/// file-backed `std/process.chz` (harvested), byte-identical to the deleted hand-built arm.
#[test]
fn process_fn_sigs_exact() {
    let sig = native_module_sig_via_graph("process");
    let proc = || Ty::Struct("ProcResult".to_string(), vec![]);
    let expected: Vec<(&str, Vec<Ty>, Ty, usize)> = vec![
        ("cmd", vec![Ty::Str], Ty::result(Ty::Str), 1),
        ("run", vec![Ty::Str], Ty::result(proc()), 1),
        (
            "run_args",
            vec![Ty::Str, Ty::list(Ty::Str)],
            Ty::result(proc()),
            2,
        ),
        // W6-4 — the byte-exact stdout twins of the two spawn forms.
        ("run_bytes", vec![Ty::Str], Ty::result(Ty::Bytes), 1),
        (
            "run_args_bytes",
            vec![Ty::Str, Ty::list(Ty::Str)],
            Ty::result(Ty::Bytes),
            2,
        ),
    ];
    assert_eq!(sig.fns().count(), expected.len(), "std.process fn count");
    for (name, params, ret, minp) in &expected {
        let fs = sig
            .certain_fn(name)
            .unwrap_or_else(|| panic!("std.process missing fn '{name}'"));
        assert_eq!(&fs.params, params, "fn '{name}' params drifted");
        assert_eq!(&fs.ret, ret, "fn '{name}' return drifted");
        assert_eq!(fs.min_params, *minp, "fn '{name}' arity drifted");
    }
    let mi = sig
        .struct_defs
        .get("ProcResult")
        .expect("std.process must export ProcResult");
    assert_eq!(
        mi.fields,
        vec![
            ("stdout".to_string(), Ty::Str),
            ("stderr".to_string(), Ty::Str),
            ("code".to_string(), Ty::Int),
        ]
    );
    assert!(matches!(mi.origin, StructOrigin::Builtin));
    assert!(sig.types.contains("ProcResult"));
}

/// PROVENANCE + exact sigs — std.request's 7 fns (incl. get/post/request's OPTIONAL trailing
/// `timeout_ms`) + the `Response` StructInfo come from the file-backed `std/request.chz`.
#[test]
fn request_fn_sigs_exact() {
    let sig = native_module_sig_via_graph("request");
    let resp = || Ty::Struct("Response".to_string(), vec![]);
    // (name, params, ret, min_params). get/post/request are optional-tail (min = len-1).
    let expected: Vec<(&str, Vec<Ty>, Ty, usize)> = vec![
        ("get", vec![Ty::Str, Ty::Int], Ty::result(resp()), 1),
        (
            "get_bytes",
            vec![Ty::Str, Ty::Int],
            Ty::result(Ty::Bytes),
            1,
        ),
        (
            "post",
            vec![Ty::Str, Ty::Str, Ty::Int],
            Ty::result(resp()),
            2,
        ),
        (
            "request",
            vec![
                Ty::Str,
                Ty::Str,
                Ty::Str,
                Ty::Map(Box::new(Ty::Str), Box::new(Ty::Str)),
                Ty::Int,
            ],
            Ty::result(resp()),
            4,
        ),
        ("put", vec![Ty::Str, Ty::Str], Ty::result(resp()), 2),
        ("patch", vec![Ty::Str, Ty::Str], Ty::result(resp()), 2),
        ("delete", vec![Ty::Str], Ty::result(resp()), 1),
        ("head", vec![Ty::Str], Ty::result(resp()), 1),
    ];
    assert_eq!(sig.fns().count(), expected.len(), "std.request fn count");
    for (name, params, ret, minp) in &expected {
        let fs = sig
            .certain_fn(name)
            .unwrap_or_else(|| panic!("std.request missing fn '{name}'"));
        assert_eq!(&fs.params, params, "fn '{name}' params drifted");
        assert_eq!(&fs.ret, ret, "fn '{name}' return drifted");
        assert_eq!(fs.min_params, *minp, "fn '{name}' arity drifted");
    }
    let ri = sig
        .struct_defs
        .get("Response")
        .expect("std.request must export Response");
    assert_eq!(
        ri.fields,
        vec![
            ("status".to_string(), Ty::Int),
            ("body".to_string(), Ty::Str),
            (
                "headers".to_string(),
                Ty::Map(Box::new(Ty::Str), Box::new(Ty::Str))
            ),
        ]
    );
    assert!(matches!(ri.origin, StructOrigin::Builtin));
    assert!(sig.types.contains("Response"));
}

/// Phase 4f — request.get/post/request accept the OPTIONAL trailing `timeout_ms` BOTH ways (the
/// harvested optional-tail sig, min_params = len-1). Byte-identical to the deleted
/// hand-built install.
#[test]
fn request_optional_timeout_arg_typechecks() {
    // With and without the trailing timeout_ms — both must check.
    entry_ok(
        "import std.request\nfn main():\n    a := request.get(\"http://x\")\n    b := request.get(\"http://x\", 500)\n    c := request.post(\"http://x\", \"b\")\n    d := request.post(\"http://x\", \"b\", 500)\n    e := request.request(\"GET\", \"http://x\", \"\", {\"h\": \"v\"})\n    f := request.request(\"GET\", \"http://x\", \"\", {\"h\": \"v\"}, 500)\n    print(\"ok\")\n",
    );
    // Too many args is still rejected (arity ceiling = params.len()).
    entry_rejects(
        "import std.request\nfn main():\n    x := request.get(\"http://x\", 1, 2)\n    print(\"x\")\n",
        "get",
    );
}

/// DRIFT GUARD — the file-harvested `ProcResult` StructInfo (fields, positional order) must byte-match
/// the remaining hand-built layout copies (seed_stdlib_structs + the runtime copies). A silent reorder
/// would trap at runtime on a field read.
#[test]
fn procresult_chz_matches_handbuilt_layouts() {
    let expected: Vec<(String, Ty)> = vec![
        ("stdout".into(), Ty::Str),
        ("stderr".into(), Ty::Str),
        ("code".into(), Ty::Int),
    ];
    let mut c = Checker::new();
    let mut sig = ModuleSig::default();
    let ast = parse_process_chz();
    c.harvest_native_module(&ast, &mut sig);
    let harvested = sig
        .struct_defs
        .get("ProcResult")
        .expect("std/process.chz must harvest a `ProcResult` struct");
    assert_eq!(harvested.fields, expected, "harvested ProcResult drifted");
    assert!(harvested.type_params.is_empty());
    assert!(harvested.methods.is_empty());
    assert!(matches!(harvested.origin, StructOrigin::Builtin));
    // 3 str-returning fns + the 2 W6-4 bytes twins (`run_bytes` / `run_args_bytes`).
    assert_eq!(sig.fns().count(), 5, "std/process.chz must harvest 5 fns");
    // seed_stdlib_structs's copy must agree.
    let seeded = c.structs.get("ProcResult").expect("ProcResult seeded");
    assert_eq!(seeded.fields, expected, "seeded ProcResult drifted");
    // Import-gating preserved: no bare-name leak.
    assert!(
        !c.struct_names.contains("ProcResult"),
        "harvest must not leave ProcResult bare-visible"
    );
}

/// DRIFT GUARD — the file-harvested `Response` StructInfo must byte-match the hand-built copies.
#[test]
fn response_chz_matches_handbuilt_layouts() {
    let expected: Vec<(String, Ty)> = vec![
        ("status".into(), Ty::Int),
        ("body".into(), Ty::Str),
        (
            "headers".into(),
            Ty::Map(Box::new(Ty::Str), Box::new(Ty::Str)),
        ),
    ];
    let mut c = Checker::new();
    let mut sig = ModuleSig::default();
    let ast = parse_request_chz();
    c.harvest_native_module(&ast, &mut sig);
    let harvested = sig
        .struct_defs
        .get("Response")
        .expect("std/request.chz must harvest a `Response` struct");
    assert_eq!(harvested.fields, expected, "harvested Response drifted");
    assert!(harvested.type_params.is_empty());
    assert!(harvested.methods.is_empty());
    assert!(matches!(harvested.origin, StructOrigin::Builtin));
    assert_eq!(sig.fns().count(), 8, "std/request.chz must harvest 8 fns");
    let seeded = c.structs.get("Response").expect("Response seeded");
    assert_eq!(seeded.fields, expected, "seeded Response drifted");
    assert!(
        !c.struct_names.contains("Response"),
        "harvest must not leave Response bare-visible"
    );
}

// ===== phase 4d: std.math/io/os/rand/fs are FILE-BACKED (native fn decls in std/<M>.chz) =====

/// The effective (graph-built) sigs for a representative fn of each migrated module must EXACTLY equal
/// what the deleted arm used to hand-build — byte-identical provenance move from arm → parsed .chz.
#[test]
fn math_io_os_rand_fs_representative_sigs_exact() {
    // (module, fn, params, ret)
    let math = native_module_sig_via_graph("math");
    let sqrt = math.certain_fn("sqrt").expect("math.sqrt");
    assert_eq!(sqrt.params, vec![Ty::Float]);
    assert_eq!(sqrt.ret, Ty::Float);
    let pow = math.certain_fn("pow").expect("math.pow");
    assert_eq!(pow.params, vec![Ty::Float, Ty::Float]);
    assert_eq!(pow.ret, Ty::Float);
    let is_nan = math.certain_fn("is_nan").expect("math.is_nan");
    assert_eq!(is_nan.ret, Ty::Bool);
    // math.pi / math.e are float module VALUES (not fns) — const lets harvested from std/math.chz.
    assert_eq!(math.value_ty("pi"), Some(&Ty::Float));
    assert_eq!(math.value_ty("e"), Some(&Ty::Float));
    // math.abs is a [T: Num] generic (TICKET-214): one declared sig, no side-set.
    let abs = math.certain_fn("abs").expect("math.abs");
    assert_eq!(abs.params, vec![Ty::Param("T".into())]);
    assert_eq!(abs.ret, Ty::Param("T".into()));
    assert_eq!(abs.type_params.len(), 1);
    assert!(abs.type_params[0].bounds.iter().any(|b| b.name == "Num"));
    // `divmod` is a BODIED Chezzi fn harvested as a module member alongside the native decls (the
    // hybrid native+Chezzi module form) — it counts as an exported fn.
    let divmod = math.certain_fn("divmod").expect("math.divmod");
    assert_eq!(divmod.params, vec![Ty::Int, Ty::Int]);
    assert_eq!(divmod.ret, Ty::Tuple(vec![Ty::Int, Ty::Int]));
    assert_eq!(
        math.fns().count(),
        32,
        "std.math must export exactly 32 fns (31 native + bodied `divmod`)"
    );

    /// W7-8 — the `PathLike` protocol `Ty`: the param type of every path-taking std fn.
    fn pathlike() -> Ty {
        Ty::Protocol("PathLike".into(), vec![])
    }
    let io = native_module_sig_via_graph("io");
    let print = io.certain_fn("print").expect("io.print");
    assert_eq!(print.params, vec![Ty::Str]);
    assert_eq!(print.ret, Ty::Nil, "io.print must return None, not Unknown");
    // W7-8 — every path param is `PathLike` now (a bare `str` literal still binds to it).
    let write_file = io.certain_fn("write_file").expect("io.write_file");
    assert_eq!(write_file.params, vec![pathlike(), Ty::Str]);
    assert_eq!(write_file.ret, Ty::result(Ty::Nil));
    assert_eq!(io.certain_fn("read_line").unwrap().ret, Ty::option(Ty::Str));
    assert_eq!(io.certain_fn("input").unwrap().params, vec![Ty::Str]);
    assert_eq!(io.certain_fn("input").unwrap().ret, Ty::option(Ty::Str));
    assert_eq!(io.certain_fn("flush").unwrap().ret, Ty::Nil);
    // R1 — the binary whole-file twins.
    let read_bytes = io.certain_fn("read_bytes").expect("io.read_bytes");
    assert_eq!(read_bytes.params, vec![pathlike()]);
    assert_eq!(read_bytes.ret, Ty::result(Ty::Bytes));
    let write_bytes = io.certain_fn("write_bytes").expect("io.write_bytes");
    assert_eq!(write_bytes.params, vec![pathlike(), Ty::Bytes]);
    assert_eq!(write_bytes.ret, Ty::result(Ty::Nil));
    // R2 — the Writer openers/handles. `create`/`append` -> Result[Writer]; `stdout`/`stderr` -> Writer;
    // `buffered(w, size = 8192)` -> Writer (optional-tail size ⇒ min_params 1).
    assert_eq!(io.certain_fn("create").unwrap().params, vec![pathlike()]);
    assert_eq!(io.certain_fn("create").unwrap().ret, Ty::result(Ty::Writer));
    assert_eq!(io.certain_fn("append").unwrap().ret, Ty::result(Ty::Writer));
    assert_eq!(io.certain_fn("stdout").unwrap().ret, Ty::Writer);
    let buffered = io.certain_fn("buffered").expect("io.buffered");
    assert_eq!(buffered.params, vec![Ty::Writer, Ty::Int]);
    assert_eq!(buffered.min_params, 1);
    assert_eq!(buffered.ret, Ty::Writer);
    // The Writer method table (harvested from the `native struct Writer`).
    let writer = io.struct_defs.get("Writer").expect("io Writer struct_def");
    assert_eq!(
        writer.methods.get("write").unwrap().ret,
        Ty::result(Ty::Int)
    );
    assert_eq!(
        writer.methods.get("close").unwrap().ret,
        Ty::result(Ty::Nil)
    );
    // R2b — the Reader opener + method table. `open` -> Result[Reader]; `read_line` -> Option[str];
    // `read_bytes(n)` -> Result[bytes]; `close` -> Result[nil].
    assert_eq!(io.certain_fn("open").unwrap().params, vec![pathlike()]);
    assert_eq!(io.certain_fn("open").unwrap().ret, Ty::result(Ty::Reader));
    let reader = io.struct_defs.get("Reader").expect("io Reader struct_def");
    assert_eq!(
        reader.methods.get("read_line").unwrap().ret,
        Ty::option(Ty::Str)
    );
    assert_eq!(
        reader.methods.get("read_bytes").unwrap().ret,
        Ty::result(Ty::Bytes)
    );
    assert_eq!(
        reader.methods.get("close").unwrap().ret,
        Ty::result(Ty::Nil)
    );
    assert_eq!(io.certain_fn("isatty").unwrap().params, Vec::<Ty>::new());
    assert_eq!(io.certain_fn("isatty").unwrap().ret, Ty::Bool);
    // 20 public fns; the 7 `_` byte-seam natives are internal to std and not exported (TICKET-243).
    assert_eq!(io.fns().count(), 20);

    let os = native_module_sig_via_graph("os");
    // W7-8 — `getcwd` hands back a `path.Path` (raw OS bytes), NOT a lossily-decoded `str`.
    assert_eq!(
        os.certain_fn("getcwd").unwrap().ret,
        Ty::result(Ty::Struct("Path".into(), vec![]))
    );
    assert_eq!(os.certain_fn("args").unwrap().ret, Ty::list(Ty::Str));
    assert_eq!(os.certain_fn("env").unwrap().ret, Ty::option(Ty::Str));
    let exit = os.certain_fn("exit").expect("os.exit");
    assert_eq!(exit.params, vec![Ty::Int]);
    assert_eq!(exit.ret, Ty::Nil);
    // gaps §6 system query + mutation fns.
    assert_eq!(os.certain_fn("getpid").unwrap().ret, Ty::Int);
    assert_eq!(os.certain_fn("platform").unwrap().ret, Ty::Str);
    assert_eq!(os.certain_fn("hostname").unwrap().ret, Ty::option(Ty::Str));
    assert_eq!(os.certain_fn("home_dir").unwrap().ret, Ty::option(Ty::Str));
    // W7-8 (review) — `temp_dir` hands back a `path.Path` too: `$TMPDIR` is raw OS bytes, and
    // decoding it left a path-RETURNING API through which a U+FFFD path was still constructible.
    assert_eq!(
        os.certain_fn("temp_dir").unwrap().ret,
        Ty::Struct("Path".into(), vec![])
    );
    assert_eq!(
        os.certain_fn("environ").unwrap().ret,
        Ty::map(Ty::Str, Ty::Str)
    );
    let setenv = os.certain_fn("setenv").expect("os.setenv");
    assert_eq!(setenv.params, vec![Ty::Str, Ty::Str]);
    assert_eq!(setenv.ret, Ty::Nil);
    assert_eq!(os.certain_fn("chdir").unwrap().ret, Ty::result(Ty::Nil));
    // 12 public fns; the 3 `_` byte-seam natives are internal to std and not exported (TICKET-243).
    assert_eq!(os.fns().count(), 12);

    let rand = native_module_sig_via_graph("rand");
    let ri = rand.certain_fn("int").expect("rand.int");
    assert_eq!(ri.params, vec![Ty::Int, Ty::Int]);
    assert_eq!(ri.ret, Ty::Int);
    assert_eq!(rand.certain_fn("float").unwrap().ret, Ty::Float);
    assert_eq!(rand.certain_fn("bool").unwrap().ret, Ty::Bool);
    assert_eq!(rand.certain_fn("seed").unwrap().ret, Ty::Nil);
    assert_eq!(rand.fns().count(), 4);

    let fs = native_module_sig_via_graph("fs");
    // W7-8 — `PathLike` in, `path.Path` out.
    let list_dir = fs.certain_fn("list_dir").expect("fs.list_dir");
    assert_eq!(list_dir.params, vec![pathlike()]);
    assert_eq!(
        list_dir.ret,
        Ty::result(Ty::list(Ty::Struct("Path".into(), vec![])))
    );
    assert_eq!(fs.certain_fn("exists").unwrap().ret, Ty::Bool);
    assert_eq!(fs.certain_fn("size").unwrap().ret, Ty::result(Ty::Int));
    assert_eq!(fs.certain_fn("mkdir").unwrap().ret, Ty::result(Ty::Nil));
    // fs-trio (fs grab-bag): canonicalize -> Result[str], chmod(str,int) -> Result[nil], atomic_write.
    assert_eq!(
        fs.certain_fn("canonicalize").unwrap().ret,
        Ty::result(Ty::Struct("Path".into(), vec![]))
    );
    assert_eq!(
        fs.certain_fn("chmod").unwrap().params,
        vec![pathlike(), Ty::Int]
    );
    assert_eq!(
        fs.certain_fn("atomic_write").unwrap().ret,
        Ty::result(Ty::Nil)
    );
    // --- fs.stat/fs.walk (gaps §6 metadata READ + recursive walk): 15 + stat + walk = 17.
    // (FileInfo is a native struct, not a function — not counted here.)
    // 17 public fns; the 17 `_` byte-seam natives and `_wrap_many` are internal to std and not
    // exported (TICKET-243).
    assert_eq!(fs.fns().count(), 17);
}

/// Hybrid native+Chezzi module: a BODIED `fn` (`math.divmod`) declared alongside the bodyless
/// `native fn`s in `std/math.chz` is harvested as a real module member — callable qualified, via
/// `import NAME from PATH`, and coexisting with a native sibling in the same module. (Its body's type
/// safety is guarded end-to-end in `tests/hybrid_native_module.rs`; here we pin that it resolves as a
/// member at all — the harvest PASS 2b + native-arm body-check wiring.)
#[test]
fn hybrid_native_module_bodied_fn_is_a_member() {
    entry_ok("import std.math\nx := math.divmod(17, 5)\nprint(x)\n");
    entry_ok("import divmod from std.math\nprint(divmod(20, 6))\n");
    // coexists with a native sibling fn (`gcd`) in the SAME module.
    entry_ok("import std.math\nprint(math.divmod(9, 2))\nprint(math.gcd(12, 18))\n");
}

/// Hover doc preserved after migration: math.sqrt (and an io/os fn) still carry the authored blurb via
/// `attach_native_module_metadata`, even though math/io/os fns are now harvested (absent from the raw
/// `native_module_sig`).
#[test]
fn math_io_os_fn_hover_doc_preserved() {
    let math = native_module_sig_via_graph("math");
    assert_eq!(
        math.certain_fn("sqrt").unwrap().doc.as_deref(),
        Some("square root (NaN for a negative argument)")
    );
    let io = native_module_sig_via_graph("io");
    assert_eq!(
        io.certain_fn("print").unwrap().doc.as_deref(),
        Some("write a line to stdout (with a trailing newline)")
    );
    let os = native_module_sig_via_graph("os");
    assert_eq!(
        os.certain_fn("getcwd").unwrap().doc.as_deref(),
        Some("the current working directory (may fail)")
    );
}

/// Runtime dispatch is UNTOUCHED by the refactor: the native member/const tables still carry the same
/// impls, name-keyed. (The `native fn` decl only feeds the front-end signature.)
#[test]
fn math_io_os_rand_fs_runtime_tables_unchanged() {
    assert_eq!(crate::native::native_members("std.math").len(), 31);
    // std.io: 9 original + R2's 5 Writer openers + R2b's `open` (all → intercepted) + read_all +
    // read_char (R2 grab-bag stdin twins) + 3 isatty TTY-detection variants (gaps §6) = 20.
    assert_eq!(crate::native::native_members("std.io").len(), 20);
    // std.os: 4 original (args/env/getcwd/exit) + gaps §6's 8 system fns
    // (getpid/platform/hostname/home_dir/temp_dir/environ/setenv/chdir) = 12.
    assert_eq!(crate::native::native_members("std.os").len(), 12);
    assert_eq!(crate::native::native_members("std.rand").len(), 4);
    // --- fs.stat/fs.walk (gaps §6): 15 original + stat + walk = 17 runtime members.
    assert_eq!(crate::native::native_members("std.fs").len(), 17);
    let math_sig = native_module_sig_via_graph("math");
    let mut consts: Vec<&str> = math_sig
        .members
        .iter()
        .filter(|(_, m)| m.is_const)
        .map(|(n, _)| n.as_str())
        .collect();
    consts.sort();
    assert_eq!(consts, vec!["e", "inf", "nan", "pi"]);
    assert!(crate::native::is_file_backed_native("std.math"));
    assert!(crate::native::is_file_backed_native("std.regex"));
    // std.net (4c-net) + std.ffi (4c-ffi) + std.concurrency (4c-concurrency) are FILE-BACKED now —
    // std.concurrency was the LAST virtual native std module, so EVERY native std module is now file-
    // backed (there is no virtual one left to assert `!is_file_backed_native` against).
    assert!(crate::native::is_file_backed_native("std.net"));
    assert!(crate::native::is_file_backed_native("std.ffi"));
    assert!(crate::native::is_file_backed_native("std.concurrency"));
}

/// Phase 4c-net — std.net is FILE-BACKED: its `Socket`/`Listener` native structs carry a harvested
/// METHOD table (the new native-method-binding capability) and its `connect`/`listen` free fns come
/// from `std/net.chz` (no Rust-side sig).
#[test]
fn net_sig_from_file_not_native_module_sig() {
    // The graph-built sig carries the free fns + both native types WITH their method tables.
    let sig = native_module_sig_via_graph("net");
    let connect = sig.certain_fn("connect").expect("net.connect");
    assert_eq!(connect.params, vec![Ty::Str]);
    assert_eq!(connect.ret, Ty::result(Ty::Socket));
    let listen = sig.certain_fn("listen").expect("net.listen");
    assert_eq!(listen.params, vec![Ty::Str]);
    assert_eq!(listen.ret, Ty::result(Ty::Listener));
    let socket = sig
        .struct_defs
        .get("Socket")
        .expect("net.Socket struct_def");
    let mut smeths: Vec<&str> = socket.methods.keys().map(String::as_str).collect();
    smeths.sort();
    assert_eq!(
        smeths,
        ["close", "read", "read_bytes", "write", "write_bytes"]
    );
    // R1 — the binary twins: `read_bytes -> Result[bytes]`, `write_bytes(bytes) -> Result[int]`.
    let read_bytes = socket.methods.get("read_bytes").unwrap();
    assert_eq!(read_bytes.params, vec![Ty::Int, Ty::Int]);
    assert_eq!(read_bytes.ret, Ty::result(Ty::Bytes));
    let write_bytes = socket.methods.get("write_bytes").unwrap();
    assert_eq!(write_bytes.params, vec![Ty::Bytes, Ty::Int]);
    assert_eq!(write_bytes.ret, Ty::result(Ty::Int));
    // read/write are optional-tail (the trailing `timeout_ms`); close is nil.
    let read = socket.methods.get("read").unwrap();
    assert_eq!(read.params, vec![Ty::Int, Ty::Int]);
    assert_eq!(read.ret, Ty::result(Ty::Str));
    assert_eq!(read.min_params, 1);
    let write = socket.methods.get("write").unwrap();
    assert_eq!(write.params, vec![Ty::Str, Ty::Int]);
    assert_eq!(write.ret, Ty::result(Ty::Int));
    assert_eq!(write.min_params, 1);
    let close = socket.methods.get("close").unwrap();
    assert_eq!(close.params, Vec::<Ty>::new());
    assert_eq!(close.ret, Ty::Nil);
    let listener = sig
        .struct_defs
        .get("Listener")
        .expect("net.Listener struct_def");
    let mut lmeths: Vec<&str> = listener.methods.keys().map(String::as_str).collect();
    lmeths.sort();
    assert_eq!(lmeths, ["accept", "addr", "close"]);
    let accept = listener.methods.get("accept").unwrap();
    assert_eq!(accept.params, vec![Ty::Int]);
    assert_eq!(accept.ret, Ty::result(Ty::Socket));
    assert_eq!(accept.min_params, 0);
    let addr = listener.methods.get("addr").unwrap();
    assert_eq!(addr.params, Vec::<Ty>::new());
    assert_eq!(addr.ret, Ty::result(Ty::Str));
}

// ===== M9: std.request (Response struct) =====

#[test]
fn native_request_get_returns_response_with_typed_fields() {
    entry_ok(
        "import std.request\nfn main():\n    match request.get(\"http://x\"):\n        ?resp:\n            st: int = resp.status\n            body: str = resp.body\n            h: Map[str, str] = resp.headers\n            print(body + str(st) + h[\"k\"])\n        !e: print(e)\n",
    );
}

#[test]
fn native_request_post_takes_url_and_body() {
    entry_ok(
        "import std.request\nfn main():\n    match request.post(\"http://x\", \"payload\"):\n        ?resp: print(str(resp.status))\n        !e: print(e)\n",
    );
}

#[test]
fn native_request_get_arg_must_be_str() {
    entry_rejects(
        "import std.request\nfn main():\n    print(request.get(5))\n",
        "argument 1 of 'get'",
    );
}

#[test]
fn native_request_response_unknown_field_rejected() {
    entry_rejects(
        "import std.request\nfn main():\n    match request.get(\"http://x\"):\n        ?resp: print(resp.nope)\n        !e: print(e)\n",
        "has no field 'nope'",
    );
}

// ===== Task 3/5: Match/Response/ProcResult are module-owned, not global-reserved =====

// (2) The bare TYPE NAME (annotation/construction) requires importing the owning module.
#[test]
fn bare_match_unknown_without_import() {
    entry_rejects(
        "fn main():\n    m: Match = Match(\"x\", 0, 1, [])\n    print(m.text)\n",
        "unknown type 'Match'",
    );
}

#[test]
fn bare_response_unknown_without_import() {
    entry_rejects(
        "fn main():\n    r: Response = Response(200, \"\", {})\n    print(str(r.status))\n",
        "unknown type 'Response'",
    );
}

#[test]
fn bare_procresult_unknown_without_import() {
    entry_rejects(
        "fn main():\n    p: ProcResult = ProcResult(\"\", \"\", 0)\n    print(str(p.code))\n",
        "unknown type 'ProcResult'",
    );
}

// A bare `FileInfo` without `import std.fs` gets the actionable import HINT (the types_by_name
// reverse-index 4th touch point — omitting it degrades to a generic "unknown type" with no hint).
#[test]
fn bare_fileinfo_hints_std_fs() {
    entry_rejects(
        "fn f(x: FileInfo):\n    print(\"hi\")\n",
        "import it from std.fs",
    );
}

// The unknown-type error hints the owning module.
#[test]
fn unknown_match_hint_points_at_module() {
    entry_rejects(
        "fn main():\n    m: Match = ProcResult(\"\", \"\", 0)\n    print(m.text)\n",
        "std.regex",
    );
}

// (3) `import std.regex` exposes `Match` bare for annotation AND qualified ctor `regex.Match(...)`.
#[test]
fn import_licenses_bare_match() {
    entry_ok(
        "import std.regex\nfn main():\n    m: regex.Match = regex.Match(\"x\", 0, 1, [])\n    print(m.text)\n",
    );
}

#[test]
fn import_licenses_bare_response() {
    entry_ok(
        "import std.request\nfn main():\n    r: request.Response = request.Response(200, \"ok\", {})\n    print(r.body)\n",
    );
}

#[test]
fn import_licenses_bare_procresult() {
    entry_ok(
        "import std.process\nfn main():\n    p: process.ProcResult = process.ProcResult(\"o\", \"e\", 0)\n    print(p.stdout)\n",
    );
}

// (4) `import Match from std.regex` (selective from-import) exposes the bare name.
#[test]
fn from_import_licenses_bare_match() {
    entry_ok(
        "import Match from std.regex\nfn main():\n    m: Match = Match(\"x\", 0, 1, [])\n    print(m.text)\n",
    );
}

#[test]
fn from_import_licenses_bare_response() {
    entry_ok(
        "import Response from std.request\nfn main():\n    r: Response = Response(200, \"ok\", {})\n    print(r.body)\n",
    );
}

// (5) The names are FREED for user types: a user `struct Response` (no import) is their own type.
#[test]
fn user_struct_response_without_import_ok() {
    entry_ok(
        "struct Response:\n    code: int\nfn main():\n    r := Response(7)\n    print(str(r.code))\n",
    );
}

#[test]
fn user_struct_match_without_import_ok() {
    entry_ok(
        "struct Match:\n    score: int\nfn main():\n    m := Match(3)\n    print(str(m.score))\n",
    );
}

// COLLISION: `import X from M` + a same-named user `struct X` (for the four native struct-modeled
// types Ref/Match/Response/ProcResult, and every other import-gated std struct) must be a CLEAN
// check-time `already defined` error — NEVER accept-then-trap at runtime. The import licenses the
// bare name as a Builtin-origin layout; a user `struct X` would overwrite the seed and carry the
// user layout while the runtime returns/constructs the native shape → field trap. These types are
// NOT reserved (a bare unimported `struct Match` is legal) — the import is an ordinary name
// collision, exactly like the enum/typealias siblings already report.
#[test]
fn import_plus_same_name_struct_decl_rejected() {
    // from-import form, the struct-modeled natives
    for (src, name) in [
        (
            "import Match from std.regex\nstruct Match:\n    v: int\nfn main():\n    print(1)\nmain()\n",
            "Match",
        ),
        (
            "import Response from std.request\nstruct Response:\n    v: int\nfn main():\n    print(1)\nmain()\n",
            "Response",
        ),
        (
            "import ProcResult from std.process\nstruct ProcResult:\n    v: int\nfn main():\n    print(1)\nmain()\n",
            "ProcResult",
        ),
    ] {
        let errs = check_entry(src);
        assert!(
            errs.iter().any(|e| e.message.contains("already imported")
                && e.message.contains(name)
                && !e.message.contains("reserved")),
            "expected `{name}` already-imported error, got: {errs:?}"
        );
    }
    // A whole-module import binds no bare name, so a local `struct Match` is its own type beside
    // `regex.Match` (`tests/chz/spec/std_module_names_test.chz` runs both).
    entry_ok("import std.regex\nstruct Match:\n    x: int\nfn main():\n    print(1)\nmain()\n");
}

// BOUNDARY: a similar-but-distinct user struct name stays legal even with the import present —
// the gate keys on the exact imported name, not a prefix/substring.
#[test]
fn import_does_not_over_reject_distinct_struct_name() {
    entry_ok(
        "import Match from std.regex\nstruct MatchBox:\n    v: int\nfn main():\n    b := MatchBox(5)\n    print(str(b.v))\nmain()\n",
    );
}

// BOUNDARY: module-owned intent preserved — the bare name is FREE for a user struct when the owning
// module is NOT imported (ProcResult lacked a bare-ok test; Ref/Match/Response covered elsewhere).
#[test]
fn bare_struct_procresult_without_import_ok() {
    entry_ok(
        "struct ProcResult:\n    x: int\nfn main():\n    r := ProcResult(5)\n    print(str(r.x))\nmain()\n",
    );
}

// BOUNDARY: `import Match as M` binds only `M` (not `Match`) into `imported_builtin_types`, so a
// same-named `struct Match` is a FREE name, not a collision — must stay accepted.
#[test]
fn import_alias_native_struct_same_name_ok() {
    entry_ok(
        "import Match as M from std.regex\nstruct Match:\n    v: int\nfn main():\n    print(str(Match(1).v))\nmain()\n",
    );
}

// PARITY TARGET: the enum sibling arm already reports `already defined` for the same import
// collision (it collides via `struct_names`) — pin it so the struct arm keeps matching it.
#[test]
fn import_plus_same_name_enum_rejected() {
    entry_rejects(
        "import Match from std.regex\nenum Match:\n    A\nfn main():\n    print(1)\nmain()\n",
        "'Match' is already imported",
    );
}

// PHASE-4b BUG: `harvest_native_module` resolves the regex fns' return types (which reference the
// import-gated `Match`, e.g. `find -> Result[Option[Match]]`) via `resolve_type` while running in the
// native-module arm WITHOUT `begin_module` — so `self.structs` is LEFTOVER from the previously-checked
// module. If a sibling user module declared a generic `struct Match[T]` (overwriting the seeded
// nparams-0 native Match), `resolve_type`'s struct arm fires a spurious `type 'Match' expects 1 type
// argument(s), got 0`, falsely rejecting a valid program. The harvest must resolve `Match` against its
// OWN native layout, immune to sibling-module / graph-order state (zero-observable-change contract).
#[test]
fn regex_harvest_immune_to_sibling_generic_match() {
    let t = TmpDir::new();
    // A user module legally declaring a generic `struct Match[T]` (it does NOT whole-module import
    // std.regex, so the decl is allowed). Checked immediately before the std.regex native arm.
    t.write("helper.chz", "struct Match[T]:\n    val: T\n");
    let entry = t.write(
        "main.chz",
        "import helper\nimport std.regex\nfn main():\n    match regex.find(\"a\", \"a\"):\n        ?opt: print(\"ok\")\n        !e: print(e)\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(
        errs.is_empty(),
        "regex harvest must be immune to a sibling `struct Match[T]`; got spurious errors: {errs:?}"
    );
}

// ===== Phase 4e — std.encoding / std.crypto / std.uuid / std.time file-backed native modules =====

/// PROVENANCE (TICKET-218) — no native module has a Rust-side sig: std.time's `timer` is an
/// ordinary member harvested from its `native fn` decl, and std.ffi's type exports come from
/// `std/ffi.chz`'s `native type` decls.
#[test]
fn enc_crypto_uuid_time_sig_from_file_not_native_module_sig() {
    let time = native_module_sig_via_graph("time");
    assert!(
        time.certain_fn("timer").is_some() && !time.types.contains("timer"),
        "timer is an ordinary std.time function"
    );
    let ffi = native_module_sig_via_graph("ffi");
    for tn in crate::native::ffi::declared_types()
        .iter()
        .map(|d| d.name.as_str())
    {
        assert!(
            ffi.types.contains(tn),
            "std.ffi must license its declared type `{tn}`"
        );
    }
}

/// EXACT SIGS — the harvested fn sigs must byte-match what `native_module_sig` used to hand-build.
#[test]
fn enc_fn_sigs_exact() {
    let sig = native_module_sig_via_graph("encoding");
    let expected: Vec<(&str, Vec<Ty>, Ty)> = vec![
        ("base64_encode", vec![Ty::Str], Ty::Str),
        ("base64_encode_url", vec![Ty::Str], Ty::Str),
        ("base64_decode", vec![Ty::Str], Ty::result(Ty::Str)),
        ("base64_decode_url", vec![Ty::Str], Ty::result(Ty::Str)),
        ("base64_encode_bytes", vec![Ty::Bytes], Ty::Str),
        ("base64_decode_bytes", vec![Ty::Str], Ty::result(Ty::Bytes)),
        ("hex_encode", vec![Ty::Str], Ty::Str),
        ("hex_decode", vec![Ty::Str], Ty::result(Ty::Str)),
        ("url_encode", vec![Ty::Str], Ty::Str),
        ("url_decode", vec![Ty::Str], Ty::result(Ty::Str)),
        (
            "query_encode",
            vec![Ty::Map(Box::new(Ty::Str), Box::new(Ty::Str))],
            Ty::Str,
        ),
        (
            "query_decode",
            vec![Ty::Str],
            Ty::Map(Box::new(Ty::Str), Box::new(Ty::Str)),
        ),
        (
            "url_parse",
            vec![Ty::Str],
            Ty::Map(Box::new(Ty::Str), Box::new(Ty::Str)),
        ),
    ];
    assert_eq!(sig.fns().count(), expected.len(), "std.encoding fn count");
    for (name, params, ret) in &expected {
        let fs = sig
            .certain_fn(name)
            .unwrap_or_else(|| panic!("std.encoding missing fn '{name}'"));
        assert_eq!(&fs.params, params, "fn '{name}' params drifted");
        assert_eq!(&fs.ret, ret, "fn '{name}' return drifted");
        assert_eq!(fs.min_params, params.len(), "fn '{name}' arity drifted");
    }
}

// ===== phase 4c-concurrency: std.concurrency is FILE-BACKED (native struct decls in std/concurrency.chz)

/// PROVENANCE — the whole std.concurrency signature (the four `Shared`/`RwShared`/`Atomic`/`Executor`
/// native structs WITH their harvested method tables) comes from parsing `std/concurrency.chz`: a full
/// graph check that `import std.concurrency` resolves all four types and their methods.
#[test]
fn concurrency_sig_from_file_not_native_module_sig() {
    // A full graph check harvests the four native structs with their method tables.
    let sig = native_module_sig_via_graph("concurrency");
    let expected: &[(&str, &[&str])] = &[
        ("Shared", &["get", "set", "update"]),
        ("RwShared", &["get", "set", "write", "read"]),
        (
            "Atomic",
            &["load", "store", "exchange", "cas", "add", "sub"],
        ),
        (
            "AtomicInt",
            &["load", "store", "exchange", "cas", "add", "sub"],
        ),
        ("Executor", &["submit", "shutdown", "cancel"]),
    ];
    for (tname, methods) in expected {
        let info = sig
            .struct_defs
            .get(*tname)
            .unwrap_or_else(|| panic!("std.concurrency must harvest native struct '{tname}'"));
        for m in *methods {
            assert!(
                info.methods.contains_key(*m),
                "std.concurrency '{tname}' must harvest method '{m}', got: {:?}",
                info.methods.keys().collect::<Vec<_>>()
            );
        }
    }
}

/// METADATA PORT (phase 4c-concurrency) — two method sigs a plain harvested sig cannot express are
/// re-attached in `attach_native_module_metadata`: `RwShared.read`'s closure param becomes `fn(T) -> ?`
/// (any return; R recovered at the call site). `Executor.submit[T]` (TICKET-289) takes the zero-arity `fn() -> T`
/// and returns `Task[T]`. The GENERIC `Shared[T]`/`RwShared[T]`/`Atomic[T]` method sigs carry `Ty::Param("T")` so
/// they substitute at each call site (the net-style native-method-binding capability extended to generics).
#[test]
fn concurrency_harvested_method_sigs_shape() {
    let sig = native_module_sig_via_graph("concurrency");
    // Shared[T].set(v: T) -> nil — the param is the box's type param, unsubstituted in the table.
    let shared = sig.struct_defs.get("Shared").expect("Shared");
    let set = shared.methods.get("set").expect("Shared.set");
    assert_eq!(set.params, vec![Ty::Param("T".into())], "Shared.set param");
    assert_eq!(set.ret, Ty::Nil, "Shared.set ret");
    // RwShared.read(f) — metadata retyped the closure param to fn(T) -> ? (any return).
    let rw = sig.struct_defs.get("RwShared").expect("RwShared");
    let read = rw.methods.get("read").expect("RwShared.read");
    match read.params.first() {
        Some(Ty::Func { params, ret, .. }) => {
            assert_eq!(params, &vec![Ty::Param("T".into())], "read closure param");
            assert_eq!(**ret, Ty::Unknown, "read closure return must be ? (any R)");
        }
        other => panic!("RwShared.read param 0 must be fn(T) -> ?, got: {other:?}"),
    }
    // Executor.submit(f) — metadata retyped the closure param to fn() -> ? (zero-arity, any return).
    let ex = sig.struct_defs.get("Executor").expect("Executor");
    let submit = ex.methods.get("submit").expect("Executor.submit");
    match submit.params.first() {
        Some(Ty::Func { params, ret, .. }) => {
            assert!(params.is_empty(), "submit closure must be zero-arity");
            assert_eq!(
                **ret,
                Ty::Param("T".into()),
                "submit closure return is the Task's T"
            );
        }
        other => panic!("Executor.submit param 0 must be fn() -> T, got: {other:?}"),
    }
}

/// Phase 4c-followup — native instance methods declare a leading bare `self` (mirroring user structs)
/// that harvest STRIPS: the harvested method-table sig must be byte-identical to the pre-`self`
/// spelling (behavior-preserving). Anti-launder: the stripped `self` must NOT surface as a leading
/// `Ty::Unknown` receiver param.
#[test]
fn native_method_harvest_strips_self() {
    // net.Socket.read(self, n: int, timeout_ms: int = 0) — self stripped ⇒ params == [int, int].
    let net = native_module_sig_via_graph("net");
    let read = net
        .struct_defs
        .get("Socket")
        .expect("Socket")
        .methods
        .get("read")
        .expect("Socket.read");
    assert_eq!(read.params, vec![Ty::Int, Ty::Int], "self must be stripped");
    assert_eq!(
        read.min_params, 1,
        "optional-tail count unaffected by strip"
    );
    assert!(
        !read.params.contains(&Ty::Unknown),
        "stripped self must not surface as a Ty::Unknown receiver"
    );
    // concurrency.Shared.get(self) -> T — self stripped ⇒ zero params, ret is the box's Ty::Param.
    let conc = native_module_sig_via_graph("concurrency");
    let get = conc
        .struct_defs
        .get("Shared")
        .expect("Shared")
        .methods
        .get("get")
        .expect("Shared.get");
    assert!(
        get.params.is_empty(),
        "self-only method harvests to zero params"
    );
    assert_eq!(get.ret, Ty::Param("T".into()), "Shared.get ret == T");
}

/// The harvested table substitutes `T` at each call site (net's capability extended to generics): a
/// wrong-typed `set`/`store` is rejected against the SUBSTITUTED element type, not the raw `Ty::Param`.
#[test]
fn concurrency_methods_resolve_via_harvested_table_with_subst() {
    // Shared[int].set("x") — the param `T` substitutes to `int`, so a str arg is rejected.
    entry_rejects(
        "import Shared from std.concurrency\nfn main():\n    s := Shared(0)\n    s.set(\"x\")\nmain()\n",
        "expected int",
    );
    // Shared[str].get() composes as a str (T substitutes to str).
    entry_ok(
        "import Shared from std.concurrency\nfn main():\n    s := Shared(\"hi\")\n    print(s.get() + \"!\")\nmain()\n",
    );
    // Atomic add/sub numeric gate residual survives the harvest (str box has no `add`).
    entry_rejects(
        "import Atomic from std.concurrency\nfn main():\n    a := Atomic(\"x\")\n    a.add(1)\nmain()\n",
        "no method 'add'",
    );
    // Shared[int].update(fn(x): x+1) — the leading `self` is stripped, so the update closure is the
    // only arg the sig expects; it still type-checks after the 4c-followup strip.
    entry_ok(
        "import Shared from std.concurrency\nfn main():\n    s := Shared(0)\n    s.update(fn(x): x+1)\nmain()\n",
    );
}

/// Executor.submit accepts a closure with ANY return type (its result is discarded) but STILL rejects a
/// wrong-arity closure — the metadata port types the param `fn() -> ?`, so a 1-arg closure is an error.
#[test]
fn executor_submit_accepts_any_return_rejects_arity() {
    entry_ok(
        "import Executor from std.concurrency\nfn main():\n    ex := Executor()\n    t := ex.submit(fn(): 42)\n    print(t.get())\n    ex.shutdown()\nmain()\n",
    );
    entry_rejects(
        "import Executor from std.concurrency\nfn main():\n    ex := Executor()\n    ex.submit(fn(x): x)\nmain()\n",
        "argument to 'submit'",
    );
}

/// EXACT SIGS (phase 4c) — the 68 harvested std.ffi fn sigs must byte-match what the deleted
/// `native_module_sig("std.ffi")` arm used to hand-build (the load_*/store_* for-loops, expanded here).
/// Also asserts the type-license tail (`ptr` + the 8 fixed-width names) survives in `sig.types`.
#[test]
fn ffi_fn_sigs_exact() {
    let sig = native_module_sig_via_graph("ffi");
    // Build the expected sigs the SAME way the old arm did, so this is a byte-for-byte provenance move.
    let mut expected: Vec<(String, Vec<Ty>, Ty)> = vec![
        ("null".to_string(), vec![], Ty::Ptr),
        ("is_null".to_string(), vec![Ty::Ptr], Ty::Bool),
    ];
    for (n, t) in [
        ("load_int", Ty::Int),
        ("load_int8", Ty::Int),
        ("load_int16", Ty::Int),
        ("load_int32", Ty::Int),
        ("load_int64", Ty::Int),
        ("load_uint8", Ty::Int),
        ("load_uint16", Ty::Int),
        ("load_uint32", Ty::Int),
        ("load_uint64", Ty::Int),
        ("load_float", Ty::Float),
        ("load_float32", Ty::Float),
        ("load_bool", Ty::Bool),
        ("load_ptr", Ty::Ptr),
        ("load_str", Ty::Str),
    ] {
        expected.push((n.to_string(), vec![Ty::Ptr], t.clone()));
        expected.push((format!("{n}_at"), vec![Ty::Ptr, Ty::Int], t));
    }
    for (n, v) in [
        ("store_int", Ty::Int),
        ("store_int8", super::sig::ffi_width_ty("int8")),
        ("store_int16", super::sig::ffi_width_ty("int16")),
        ("store_int32", super::sig::ffi_width_ty("int32")),
        ("store_int64", super::sig::ffi_width_ty("int64")),
        ("store_uint8", super::sig::ffi_width_ty("uint8")),
        ("store_uint16", super::sig::ffi_width_ty("uint16")),
        ("store_uint32", super::sig::ffi_width_ty("uint32")),
        ("store_uint64", super::sig::ffi_width_ty("uint64")),
        ("store_float", Ty::Float),
        ("store_float32", super::sig::ffi_width_ty("float32")),
        ("store_bool", Ty::Bool),
        ("store_ptr", Ty::Ptr),
    ] {
        expected.push((n.to_string(), vec![Ty::Ptr, v.clone()], Ty::Nil));
        expected.push((format!("{n}_at"), vec![Ty::Ptr, Ty::Int, v], Ty::Nil));
    }
    expected.push(("alloc".to_string(), vec![Ty::Int], Ty::Ptr));
    expected.push(("alloc_zeroed".to_string(), vec![Ty::Int], Ty::Ptr));
    expected.push(("free".to_string(), vec![Ty::Ptr], Ty::Nil));
    for w in [
        "int8", "int16", "int32", "int64", "uint8", "uint16", "uint32", "uint64",
    ] {
        expected.push((format!("cast_{w}"), vec![Ty::Int], Ty::Int));
    }
    expected.push(("cast_float32".to_string(), vec![Ty::Float], Ty::Float));

    assert_eq!(expected.len(), 68, "expected exactly 68 std.ffi fns");
    assert_eq!(
        sig.fns().count(),
        68,
        "std.ffi must harvest exactly 68 native fns from std/ffi.chz"
    );
    for (name, params, ret) in &expected {
        let fs = sig
            .certain_fn(name)
            .unwrap_or_else(|| panic!("std.ffi missing fn '{name}'"));
        assert_eq!(&fs.params, params, "fn '{name}' params drifted");
        assert_eq!(&fs.ret, ret, "fn '{name}' return drifted");
        assert_eq!(fs.min_params, params.len(), "fn '{name}' arity drifted");
    }
    // The type-license tail is UNCHANGED by the migration: `ptr` + the 8 fixed-width names survive.
    assert!(
        sig.types.contains("ptr"),
        "std.ffi must keep `ptr` licensed"
    );
    for tn in crate::native::ffi::declared_types()
        .iter()
        .map(|d| d.name.as_str())
    {
        assert!(
            sig.types.contains(tn),
            "std.ffi must keep the fixed-width type name `{tn}` licensed"
        );
    }
    // The runtime member table cross-checks the harvested surface 1:1.
    assert_eq!(
        crate::native::native_members("std.ffi").len(),
        68,
        "std.ffi runtime MEMBERS must match the 68 harvested fns"
    );
}

#[test]
fn crypto_fn_sigs_exact() {
    let sig = native_module_sig_via_graph("crypto");
    let expected: Vec<(&str, Vec<Ty>, Ty)> = vec![
        ("sha256", vec![Ty::Str], Ty::Str),
        ("sha256_bytes", vec![Ty::Bytes], Ty::Str),
        ("sha1", vec![Ty::Str], Ty::Str),
        ("sha1_bytes", vec![Ty::Bytes], Ty::Str),
        ("sha512", vec![Ty::Str], Ty::Str),
        ("sha512_bytes", vec![Ty::Bytes], Ty::Str),
        ("md5", vec![Ty::Str], Ty::Str),
        ("hmac_sha256", vec![Ty::Bytes, Ty::Bytes], Ty::Str),
        ("secure_bytes", vec![Ty::Int], Ty::Bytes),
        ("token_hex", vec![Ty::Int], Ty::Str),
    ];
    assert_eq!(sig.fns().count(), expected.len(), "std.crypto fn count");
    for (name, params, ret) in &expected {
        let fs = sig
            .certain_fn(name)
            .unwrap_or_else(|| panic!("std.crypto missing fn '{name}'"));
        assert_eq!(&fs.params, params, "fn '{name}' params drifted");
        assert_eq!(&fs.ret, ret, "fn '{name}' return drifted");
    }
}

#[test]
fn uuid_fn_sigs_exact() {
    let sig = native_module_sig_via_graph("uuid");
    let expected: Vec<(&str, Vec<Ty>, Ty)> = vec![
        ("v4", vec![], Ty::Str),
        ("uuid_seed", vec![Ty::Int], Ty::Nil),
    ];
    assert_eq!(sig.fns().count(), expected.len(), "std.uuid fn count");
    for (name, params, ret) in &expected {
        let fs = sig
            .certain_fn(name)
            .unwrap_or_else(|| panic!("std.uuid missing fn '{name}'"));
        assert_eq!(&fs.params, params, "fn '{name}' params drifted");
        assert_eq!(&fs.ret, ret, "fn '{name}' return drifted");
    }
}

#[test]
fn std_time_exports_now_ms_as_int() {
    entry_ok("import std.time\nt: int = time.now_ms()\nprint(t)\n");
}

#[test]
fn time_fn_sigs_exact() {
    let sig = native_module_sig_via_graph("time");
    let expected: Vec<(&str, Vec<Ty>, Ty)> = vec![
        ("now", vec![], Ty::Int),
        ("now_ms", vec![], Ty::Int),
        ("monotonic", vec![], Ty::Float),
        ("sleep_ms", vec![Ty::Int], Ty::Nil),
        ("format", vec![Ty::Int], Ty::Str),
        ("timer", vec![Ty::Int], Ty::channel(Ty::Bool)),
    ];
    assert_eq!(
        sig.fns().count(),
        expected.len(),
        "std.time must export exactly its 6 native fns"
    );
    for (name, params, ret) in &expected {
        let fs = sig
            .certain_fn(name)
            .unwrap_or_else(|| panic!("std.time missing fn '{name}'"));
        assert_eq!(&fs.params, params, "fn '{name}' params drifted");
        assert_eq!(&fs.ret, ret, "fn '{name}' return drifted");
    }
}

/// A `native fn` decl in a USER (non-stdlib) .chz file is still a clear error — the file-backed std
/// files take the native arm (bypassing check_module's guard), user files do not (phase-4e regression
/// guard, complementing `native_decl_in_user_file_rejected`).
#[test]
fn phase4e_user_file_native_fn_still_rejected() {
    entry_rejects(
        "native fn sha256(s: str) -> str\nfn main():\n    print(1)\nmain()\n",
        "native fn/ctor declarations are only allowed in standard-library modules",
    );
}

#[test]
fn native_time_now_is_int_monotonic_is_float() {
    entry_ok(
        "import std.time\nfn main():\n    t: int = time.now()\n    m: float = time.monotonic()\n    time.sleep_ms(0)\n    s: str = time.format(t)\n    print(s)\n",
    );
}

#[test]
fn native_time_format_arg_must_be_int() {
    entry_rejects(
        "import std.time\nfn main():\n    print(time.format(\"x\"))\n",
        "argument 1 of 'format'",
    );
}

// ===== M8-M5: type-directed json.decode[T] =====

#[test]
fn json_decode_into_struct_is_result_of_struct() {
    entry_ok(
        "import std.json\nstruct P:\n    x: int\n    y: int\nfn main():\n    match json.decode[P](\"x\"):\n        ?p: print(str(p.x))\n        !e: print(e)\n",
    );
}

#[test]
fn json_decode_into_typed_map_and_list() {
    entry_ok(
        "import std.json\nfn main():\n    a := json.decode[Map[str, int]](\"x\")\n    b := json.decode[List[float]](\"y\")\n    print(\"ok\")\n",
    );
}

#[test]
fn json_decode_scalar_result_type_flows() {
    entry_ok(
        "import std.json\nfn main():\n    match json.decode[int](\"3\"):\n        ?n: print(str(n + 1))\n        !e: print(e)\n",
    );
}

#[test]
fn json_decode_source_must_be_str() {
    entry_rejects(
        "import std.json\nfn main():\n    print(json.decode[int](5))\n",
        "decode source must be str",
    );
}

#[test]
fn json_decode_rejects_function_target() {
    entry_rejects(
        "import std.json\nfn main():\n    x := json.decode[fn(int) -> int](\"x\")\n",
        "cannot decode into",
    );
}

#[test]
fn json_decode_rejects_unknown_target_type() {
    entry_rejects(
        "import std.json\nfn main():\n    x := json.decode[Nope](\"x\")\n",
        "unknown type 'Nope'",
    );
}

#[test]
fn json_decode_rejects_recursive_struct() {
    entry_rejects(
        "import std.json\nstruct Node:\n    val: int\n    next: Node?\nfn main():\n    x := json.decode[Node](\"x\")\n",
        // W19 S2: the bare struct name, never the module-mangled key (`main::Node`).
        "recursive struct 'Node' is not decodable",
    );
}

#[test]
fn json_decode_rejects_map_with_non_str_key() {
    entry_rejects(
        "import std.json\nfn main():\n    x := json.decode[Map[int, int]](\"x\")\n",
        "map keys must be str",
    );
}

// ===== M8-M4: set type =====

#[test]
fn set_literal_infers_set_of_elem() {
    ok("s: Set[int] = {1, 2, 3}\nprint(s.len())\n");
}

#[test]
fn set_methods_typecheck() {
    ok(
        "s := {1, 2}\nb: bool = s.contains(1)\ns.insert(3)\nr: bool = s.remove(1)\nu: Set[int] = s.union({4})\nprint(u.len())\n",
    );
}

#[test]
fn set_builtin_empty_and_from_list() {
    ok("e: Set[str] = Set()\ne.insert(\"x\")\nf: Set[int] = Set([1, 1, 2])\nprint(f.len())\n");
}

#[test]
fn set_iteration_binds_element() {
    ok("for x in {1, 2, 3}:\n    y: int = x\n    print(y)\n");
}

#[test]
fn set_mixed_element_types_rejected() {
    rejects("s := {1, \"two\"}\n", "set elements differ");
}

#[test]
fn set_non_hashable_element_rejected() {
    rejects("s := {[1], [2]}\n", "must implement Hashable");
}

#[test]
fn set_union_arg_must_be_set() {
    rejects("s := {1, 2}\nx := s.union([3])\n", "argument 1 of 'union'");
}

#[test]
fn set_not_indexable() {
    rejects("s := {1, 2}\nx := s[0]\n", "cannot index into Set");
}

/// TICKET-265 — a global protocol (declared in the prelude) needs no import, so it gets no import
/// hint; a std protocol of another module keeps its hint.
#[test]
fn a_global_protocol_gets_no_import_hint() {
    let c = Checker::new();
    let globals: Vec<&str> = crate::checker::graph_pass::std_protocol_decls()
        .iter()
        .map(|p| p.name.as_str())
        .filter(|n| crate::checker::is_global_protocol(n))
        .collect();
    assert_eq!(globals, ["Any", "Error"]);
    for name in globals {
        assert_eq!(c.import_hint(name), "", "{name}");
    }
    assert_eq!(
        c.import_hint("Iterable"),
        "; import it from std.iter (`import Iterable from std.iter`)"
    );
}

// ===== TICKET-279: Deque, Heap and Counter are Iterable =====

#[test]
fn deque_is_iterable_in_for() {
    entry_ok(
        "import Deque from std.collections\nfn main():\n    d := Deque[int]([], [])\n    d.push_back(1)\n    for x in d:\n        n: int = x\n        print(n)\n",
    );
}

/// One row of the Iterable grid: what iterating one type yields, from the ancestor's run output.
struct IterRow {
    name: &'static str,
    /// Lines before `fn main():` (imports, declarations).
    head: &'static str,
    /// The statements that bind `v` inside `main`.
    make: &'static str,
    /// What one step yields, spelled as a type.
    elem: &'static str,
    /// `print` of the elements in iteration order, or of the sorted elements for an unordered row.
    want: &'static str,
    /// The ancestor fixes no order (Rust `BinaryHeap::iter`): each cell sorts what it got, so the
    /// cell checks the elements and their count, never their order.
    unordered: bool,
}

/// The builtin rows: one per `RecvKind` that an `Iterable` row of `INTRINSIC_PROTO_METHODS`
/// names. A struct row comes from `STD_ROWS` and `USER_ROWS`.
fn builtin_iterable_rows() -> Vec<IterRow> {
    use crate::checker::proto::{INTRINSIC_PROTO_METHODS, RecvKind};
    let row = |name, make, elem, want| IterRow {
        name,
        head: "",
        make,
        elem,
        want,
        unordered: false,
    };
    let mut rows = Vec::new();
    for (proto, _, kind) in INTRINSIC_PROTO_METHODS {
        if *proto != "Iterable" {
            continue;
        }
        let r = match kind {
            RecvKind::List => row("List", "v := [3, 1, 2]", "int", "[3, 1, 2]"),
            RecvKind::Set => row("Set", "v := {3, 1, 2}", "int", "[3, 1, 2]"),
            RecvKind::Map => row(
                "Map",
                "v := {'b': 2, 'a': 1}",
                "(str, int)",
                "[('b', 2), ('a', 1)]",
            ),
            RecvKind::Str => row("str", "v := 'ab'", "str", "['a', 'b']"),
            RecvKind::Bytes => row("bytes", "v := b'AB'", "int", "[65, 66]"),
            RecvKind::ByteArray => row("bytearray", "v := bytearray(b'AB')", "int", "[65, 66]"),
            RecvKind::Channel => row(
                "Channel",
                "v := Channel[int](2)\n    v.send(4)\n    v.send(5)\n    v.close()",
                "int",
                "[4, 5]",
            ),
            RecvKind::Cursor => row("Iterator", "v := [7, 8].iter()", "int", "[7, 8]"),
            // A struct yields through its own hook: the std and user rows cover it.
            RecvKind::Struct => continue,
            RecvKind::Int
            | RecvKind::Float
            | RecvKind::Bool
            | RecvKind::Tuple
            | RecvKind::Enum
            | RecvKind::Option
            | RecvKind::Result
            | RecvKind::Func
            | RecvKind::Protocol
            | RecvKind::Shared
            | RecvKind::RwShared
            | RecvKind::Atomic => panic!("an Iterable row names {kind:?}: give it a grid row"),
        };
        rows.push(r);
    }
    rows
}

/// Every std struct that declares an `iter` method, found by parsing `STD_FILES`.
fn std_structs_with_iter() -> Vec<String> {
    let mut names = Vec::new();
    for (_, src) in crate::native::std_embed::STD_FILES {
        let ast = crate::parser::parse(crate::lexer::tokenize(src).expect("std lexes"))
            .expect("std parses");
        for s in &ast.stmts {
            if let StmtKind::Struct { name, methods, .. } = &s.kind
                && methods.iter().any(|m| m.name == "iter")
            {
                names.push(name.clone());
            }
        }
    }
    names.sort();
    names
}

/// The std rows (owner decision 2026-10-16), in name order. Each order is the ancestor's run
/// output: Deque front to back (Python `list(deque)`, Rust `VecDeque::iter`), Heap in no order
/// (Rust `BinaryHeap::iter`, so its cells compare sorted contents), Counter as a Map, Range as
/// Python `range`.
const STD_ROWS: &[IterRow] = &[
    IterRow {
        name: "Counter",
        head: "import Counter from std.collections\n",
        make: "v: Counter[str] = Counter({})\n    v.add('b')\n    v.add('a')\n    v.add('b')",
        elem: "(str, int)",
        want: "[('b', 2), ('a', 1)]",
        unordered: false,
    },
    IterRow {
        name: "Deque",
        head: "import Deque from std.collections\n",
        make: "v := Deque[int]([], [])\n    v.push_back(2)\n    v.push_back(3)\n    v.push_front(1)\n    v.push_front(0)",
        elem: "int",
        want: "[0, 1, 2, 3]",
        unordered: false,
    },
    IterRow {
        name: "Heap",
        head: "import min_heap from std.collections\n",
        make: "v := min_heap()\n    v.push(5)\n    v.push(1)\n    v.push(3)",
        elem: "int",
        want: "[1, 3, 5]",
        unordered: true,
    },
    IterRow {
        name: "Range",
        head: "",
        make: "v := 1..4",
        elem: "int",
        want: "[1, 2, 3]",
        unordered: false,
    },
    // TICKET-288: a receiver yields `T!Lagged` per step, in send order, until the broadcast is closed.
    IterRow {
        name: "Receiver",
        head: "import Broadcast, Lagged from std.concurrency.broadcast\n",
        make: "b := Broadcast[int].new(4)\n    v := b.subscribe()\n    b.send(1)\n    b.send(2)\n    b.send(3)\n    b.close()",
        elem: "int!Lagged",
        want: "[1, 2, 3]",
        unordered: false,
    },
];

/// A user struct yields through its `iter` hook as a std struct does.
const USER_ROWS: &[IterRow] = &[IterRow {
    name: "user struct",
    head: "import Iterator from std.iter\nstruct Bag:\n    xs: List[(str, int)]\n\n    fn iter(self) -> Iterator[(str, int)]:\n        return self.xs.iter()\n",
    make: "v := Bag([('a', 1), ('b', 2)])",
    elem: "(str, int)",
    want: "[('a', 1), ('b', 2)]",
    unordered: false,
}];

/// TICKET-279: one answer to "what does `for x in v` yield", for every Iterable type x every
/// position (`for`, comprehension, `List(...)`, `v.iter()`, a tuple target). A cell is red when
/// the checker rejects it, or when its run prints other than the ancestor's elements.
#[test]
fn iterable_yields_grid() {
    let std_names: Vec<String> = STD_ROWS.iter().map(|r| r.name.to_string()).collect();
    let mut red: Vec<String> = Vec::new();
    let declared = std_structs_with_iter();
    if declared != std_names {
        red.push(format!(
            "std structs with `iter` {declared:?}, STD_ROWS {std_names:?}"
        ));
    }
    let rows = builtin_iterable_rows();
    for IterRow {
        name,
        head,
        make,
        elem,
        want,
        unordered,
    } in rows.iter().chain(STD_ROWS).chain(USER_ROWS)
    {
        let mut cells = vec![
            (
                "for",
                format!("    got: List[{elem}] = []\n    for x in v:\n        got.push(x)\n"),
            ),
            ("comprehension", "    got := [x for x in v]\n".to_string()),
            ("List()", "    got := List(v)\n".to_string()),
            ("v.iter()", "    got := List(v.iter())\n".to_string()),
        ];
        if elem.starts_with('(') {
            cells.push((
                "tuple target",
                format!(
                    "    got: List[{elem}] = []\n    for a, b in v:\n        got.push((a, b))\n"
                ),
            ));
        }
        let sort = if *unordered { "    got.sort()\n" } else { "" };
        for (pos, body) in cells {
            let src = format!("{head}fn main():\n    {make}\n{body}{sort}    print(got)\n");
            let errs = check_entry(&src);
            if let Some(e) = errs.first() {
                red.push(format!("{name} x {pos}: check: {}", e.message));
                continue;
            }
            match crate::compiler::run_for_tests::run_capture(&format!("{src}main()\n")) {
                Ok(out) if out.trim_end() == *want => {}
                Ok(out) => red.push(format!("{name} x {pos}: printed {out:?}, want {want:?}")),
                Err(e) => red.push(format!("{name} x {pos}: run: {e:?}")),
            }
        }
    }
    assert!(red.is_empty(), "red cells:\n{}", red.join("\n"));
}

/// TICKET-279: a type with an `iter` hook is iterable, so the `Iterator` bound refuses it with the
/// `for` / `.iter()` advice, as for a raw list. `iterable_elem` decides it (`satisfies.rs`).
#[test]
fn deque_against_an_iterator_bound_gets_the_iterate_advice() {
    rejects_entry(
        "import Deque from std.collections\nimport Iterator from std.iter\nfn first[S: Iterator[T], T](xs: S, d: T) -> T:\n    return d\nfn main():\n    d := Deque[int]([], [])\n    n := first(d, 0)\n    print(n)\n",
        "Iterate it with `for`",
    );
}

/// TICKET-288: `std.concurrency.broadcast` exports `Broadcast[T]` (tokio `broadcast`): `subscribe()` gives a
/// receiver that iterates with `for`, `send` never blocks, `close` ends the loops.
#[test]
fn broadcast_is_exported_by_std_concurrency() {
    ok(
        "import Broadcast from std.concurrency.broadcast\nfn main():\n    b := Broadcast[int].new(4)\n    rx := b.subscribe()\n    b.send(1)\n    b.close()\n    for v in rx:\n        print(v)\n",
    );
}
