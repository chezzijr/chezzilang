//! Checker tests: TICKET-187 family F: names, exports, method type params, defaults, extern
//! structs, generic value uses, prefix `!`.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== TICKET-187: Family F — names resolved outside the single sources =====

/// K6: a protocol call binds named args by the PROTOCOL's parameter names, so an
/// implementor that permutes them (`new, old`) must not conform (owner decision 2026-09-30).
#[test]
fn t187_protocol_impl_with_permuted_param_names_rejected() {
    rejects_desugared(
        "protocol Rep:\n    fn replace(self, old: str, new: str) -> str\nstruct Doc:\n    text: str\n    fn replace(self, new: str, old: str) -> str:\n        return self.text.replace(old, new)\nfn run(p: Rep) -> str:\n    return p.replace(old=\"a\", new=\"b\")\nprint(run(Doc(\"aaa\")))\n",
        "old",
    );
}

/// K9: `x.decode[T](s)` on a non-json receiver is an ordinary member call; `int` has no
/// `decode` method, so check must reject it (today: check OK, runtime "no method 'parse'").
#[test]
fn t187_decode_on_int_receiver_rejected() {
    rejects("n := 5\nprint(n.decode[int](\"7\"))\n", "decode");
}

/// G1: a caller's `T` and the imported generic's `T` must not merge by name; calling
/// `cmp.max` through a value with a non-Comparable `T` stays rejected like the direct call.
#[test]
fn t187_generic_fn_value_same_named_param_rejected() {
    rejects_entry(
        "import std.cmp\nstruct P:\n    x: int\nfn pick[T](a: T, b: T) -> T:\n    f := cmp.max\n    return f(a, b)\nfn main():\n    print(pick(P(1), P(2)).x)\n",
        // TICKET-238: the binding is rejected on its own line; no later call pins it.
        "'cmp.max' is generic and T is not determined here",
    );
}

/// The rejection of an unpinned imported generic value offers only advice that works. TICKET-197:
/// a value-position turbofish resolves on an imported fn (`cmp.max[int]`), so it is offered.
#[test]
fn t187_imported_generic_value_advice_offers_the_turbofish() {
    let errs = check_entry("import std.cmp\nf := cmp.max\n");
    let e = errs
        .iter()
        .find(|e| {
            e.message
                .contains("'cmp.max' is generic and T is not determined here")
        })
        .unwrap_or_else(|| panic!("expected the generic-value error, got: {errs:?}"));
    assert!(
        e.message.contains("instantiate it (`cmp.max[<T>]`)"),
        "{}",
        e.message
    );
    let errs = check_entry("import std.cmp\nf := cmp.max[int]\nprint(f(3, 9))\n");
    assert!(errs.is_empty(), "{errs:?}");
}

/// Owner answer 2(a): a one-parameter implementor that renames the parameter does not conform.
#[test]
fn t187_protocol_one_param_name_mismatch_rejected() {
    rejects_desugared(
        "protocol Sh:\n    fn put(self, item: int)\nstruct B:\n    fn put(self, x: int):\n        pass\ns: Sh = B()\n",
        "declares 'item'",
    );
}

/// Owner answer 2(a): prelude protocols take their names from `std/prelude.chz` (`other`).
#[test]
fn t187_prelude_protocol_name_mismatch_rejected() {
    rejects_desugared(
        "struct P:\n    x: int\n    fn compare(self, o: P) -> int:\n        return self.x - o.x\nfn lo[T: Comparable](a: T, b: T) -> T:\n    return a\nprint(lo(P(1), P(2)).x)\n",
        "declares 'other'",
    );
}

/// The K6 program with the protocol's own parameter order conforms.
#[test]
fn t187_matching_implementor_accepted() {
    ok_desugared(
        "protocol Rep:\n    fn replace(self, old: str, new: str) -> str\nstruct Doc:\n    text: str\n    fn replace(self, old: str, new: str) -> str:\n        return self.text.replace(old, new)\nfn run(p: Rep) -> str:\n    return p.replace(old=\"a\", new=\"b\")\nprint(run(Doc(\"aaa\")))\n",
    );
}

/// `str` conforms to `Rep`: its native `replace(self, old, new)` carries the same names.
#[test]
fn t187_builtin_conformer_binds_by_protocol_names() {
    ok_desugared(
        "protocol Rep:\n    fn replace(self, old: str, new: str) -> str\nfn run(p: Rep) -> str:\n    return p.replace(old=\"a\", new=\"b\")\nprint(run(\"aaa\"))\n",
    );
}

/// Owner answer 1(a): an implementor's default is not reachable through the protocol.
#[test]
fn t187_protocol_call_omitting_an_argument_is_an_arity_error() {
    rejects_desugared(
        "protocol P:\n    fn probe(self, n: int) -> int\nstruct S:\n    fn probe(self, n: int = 5) -> int:\n        return n\nfn f(p: P) -> int:\n    return p.probe()\nprint(f(S()))\n",
        "expects 1 argument",
    );
}

/// TICKET-189 A1: a spawned callee that REBINDS its parameter before writing it writes a fresh
/// local, not the parent's copy -- Go prints `3 [1]`. Layer A must drop the rebound parameter.
#[test]
fn spawn_callee_rebinding_its_param_before_writing_is_not_a_task_copy_write() {
    ok(
        "fn f5(ys: List[int], out: Channel[int]):\n    ys = [1, 2]\n    ys.push(3)\n    out.send(ys.len())\nfn main():\n    xs := [1]\n    out := Channel[int](1)\n    parallel:\n        spawn f5(xs, out)\n    print(out.recv())\n    print(xs)\nmain()\n",
    );
}

/// TICKET-189 G1: layer A's compile-time verdict for every non-generator spawn route × argument
/// source × callee body. A param is mapped through the checker's call plan (`bound_slots`), so a
/// keyword-bound named arg a callee writes is rejected, and a variadic pack, a literal default fill
/// or a param rebound before the write is not a task copy. Method, static and value callees decline
/// to layer C (the runtime fault, G2 in `tests/chz/spec/airlock_crossing_policy_grid_test.chz`).
#[test]
fn airlock_crossing_policy_grid_compile_cells() {
    const COPY: &str = "is this task's copy";
    let prelude = "import std.concurrency\n\
fn w(xs: List[int], out: Channel[int]):\n    xs.push(3)\n    out.send(xs.len())\n\
fn rb(ys: List[int], out: Channel[int]):\n    ys = [1, 2]\n    ys.push(3)\n    out.send(ys.len())\n\
fn rbc(ys: List[int], out: Channel[int], c: bool):\n    if c:\n        ys = [1]\n    ys.push(3)\n    out.send(ys.len())\n\
fn wr(ys: List[int], out: Channel[int]):\n    ys.push(3)\n    ys = [1]\n    out.send(ys.len())\n\
fn pk(out: Channel[int], ...rest: List[int]):\n    rest.push([9])\n    out.send(rest.len())\n\
fn df(out: Channel[int], acc: List[int] = []):\n    acc.push(3)\n    out.send(acc.len())\n\
struct R:\n    n: int\n\n    fn w(self, xs: List[int], out: Channel[int]):\n        xs.push(3)\n        out.send(xs.len())\n\n    fn rb(self, ys: List[int], out: Channel[int]):\n        ys = [1, 2]\n        ys.push(3)\n        out.send(ys.len())\n";
    let single = |cell: &str| {
        format!(
            "{prelude}fn main():\n    xs := [1]\n    out := Channel[int](8)\n    r := R(n=1)\n    g := w\n    parallel:\n        {cell}\n    print(xs)\n    print(r.n)\n    print(g)\nmain()\n"
        )
    };
    let lib = "fn w(xs: List[int], out: Channel[int]):\n    xs.push(3)\n    out.send(xs.len())\n\
fn dflt(out: Channel[int], acc: List[int] = []):\n    acc.push(3)\n    out.send(acc.len())\n\
fn pk(out: Channel[int], ...rest: List[int]):\n    rest.push([9])\n    out.send(rest.len())\n\
struct K:\n    n: int\n\n    fn w(xs: List[int], out: Channel[int]):\n        xs.push(3)\n        out.send(xs.len())\n";
    let graph = |cell: &str| {
        format!(
            "import std.concurrency\nimport g1lib as lib\nfn main():\n    xs := [1]\n    out := Channel[int](8)\n    parallel:\n        {cell}\n    print(xs)\nmain()\n"
        )
    };
    // (cell, graph program?, expected: None = ok, Some(needle) = rejected)
    let cells: &[(&str, bool, Option<&str>)] = &[
        ("spawn rb(xs, out)", false, None),
        ("spawn rbc(xs, out, true)", false, None),
        ("spawn wr(xs, out)", false, Some(COPY)),
        ("spawn pk(out, xs)", false, None),
        ("spawn w(out=out, xs=xs)", false, Some(COPY)),
        ("spawn w(out=out, xs=[1])", false, None),
        ("spawn df(out)", false, None),
        ("spawn w(xs, out)", false, Some(COPY)),
        ("spawn w([1], out)", false, None),
        ("spawn lib.w(xs, out)", true, Some(COPY)),
        ("spawn lib.w(out=out, xs=xs)", true, Some(COPY)),
        ("spawn lib.pk(out, xs)", true, None),
        ("spawn lib.dflt(out)", true, None),
        ("spawn lib.K.w(xs, out)", true, None),
        ("spawn r.w(xs, out)", false, None),
        ("spawn r.w(out=out, xs=xs)", false, None),
        ("spawn r.rb(xs, out)", false, None),
        ("spawn g(xs, out)", false, None),
        ("spawn: w(out=out, xs=xs)", false, Some(COPY)),
        ("spawn: w(xs, out)", false, Some(COPY)),
        ("spawn: pk(out, xs)", false, None),
        ("spawn: rb(xs, out)", false, None),
        ("spawn: lib.w(xs, out)", true, Some(COPY)),
    ];
    let mut wrong = Vec::new();
    for &(cell, is_graph, want) in cells {
        let errs = if is_graph {
            check_files(&[("g1lib.chz", lib), ("main.chz", &graph(cell))])
        } else {
            check_src(&single(cell))
        };
        let pass = match want {
            None => errs.is_empty(),
            Some(needle) => errs.iter().any(|e| e.message.contains(needle)),
        };
        if !pass {
            wrong.push(format!("cell `{cell}`: expected {want:?}, got: {errs:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} wrong cell(s):\n{}",
        wrong.len(),
        wrong.join("\n")
    );
    // Executor has no `map` (`std/concurrency.chz` declares submit/submit_result).
    files_reject(
        &[(
            "main.chz",
            "import std.concurrency\nfn main():\n    ex := Executor()\n    print(ex.map(fn(x: int) -> int: x, [1]))\nmain()\n",
        )],
        "has no method 'map'",
    );
}

/// TICKET-219: `submit_outcome(f, out)` is deleted. It sealed a caller's channel, which may be
/// full, unbuffered, or fanned in from several jobs; `submit_result` returns std's own channel.
#[test]
fn executor_has_no_submit_outcome() {
    files_reject(
        &[(
            "main.chz",
            "import std.concurrency\nfn main():\n    ex := Executor()\n    out := Channel[int!](1)\n    ex.submit_outcome(fn() -> int: 7, out)\nmain()\n",
        )],
        "has no method 'submit_outcome'",
    );
}

/// TICKET-196 K5x: a module that rebinds a fn name (`f := fn(b, a)`) exports the OLD fn's labels, so a
/// keyword call from another module binds the stale labels (CPython prints `12`, Chezzi `21`).
#[test]
fn imported_rebound_fn_denies_keyword_call_qualified() {
    files_reject(
        &[
            (
                "lib.chz",
                "fn f(a: int, b: int) -> str:\n    return \"{a}{b}\"\nf := fn(b: int, a: int) -> str: \"{a}{b}\"\n",
            ),
            ("main.chz", "import lib\nprint(lib.f(a=1, b=2))\n"),
        ],
        "keyword arguments through 'f' are ambiguous",
    );
}

/// TICKET-196 K5x, from-import call form.
#[test]
fn imported_rebound_fn_denies_keyword_call_from_import() {
    files_reject(
        &[
            (
                "lib.chz",
                "fn f(a: int, b: int) -> str:\n    return \"{a}{b}\"\nf := fn(b: int, a: int) -> str: \"{a}{b}\"\n",
            ),
            ("main.chz", "import f from lib\nprint(f(a=1, b=2))\n"),
        ],
        "keyword arguments through 'f' are ambiguous",
    );
}

/// TICKET-196 K6x: an imported const cannot be re-declared in the importer.
#[test]
fn imported_const_cannot_be_redeclared() {
    files_reject(
        &[
            ("lib.chz", "Y: const int = 5\n"),
            ("main.chz", "import Y from lib\nY := 7\nprint(Y)\n"),
        ],
        "cannot re-declare const binding 'Y'",
    );
}

/// TICKET-196 Family D2: a module exports ONE record per slot (`ModuleSig::members`). Every read path
/// of another module's name (qualified call, from-import call, qualified/from-import value, `defer` /
/// `spawn` / `spawn:` call forms, turbofish member, generic caller) x every declaration shape reads
/// the slot's final type, and labels / defaults / variadic / type params / witnesses / write summary
/// only from a name declared once as a fn. Each cell's same-module twin is pinned by DEC-186's cells.
#[test]
fn module_export_grid() {
    const FO: &str = "fn f(a: int, b: int = 5) -> str:\n    return \"{a}{b}\"\n";
    const LET_F: &str = "f := fn(b: int, a: int) -> str: \"{a}{b}\"\n";
    const VF: &str = "fn f(a: int, ...rest: int) -> str:\n    return \"{a}\"\nf := fn(a: int, b: int) -> str: \"{a}{b}\"\n";
    const GF: &str = "fn idt[T](x: T) -> T:\n    return x\nidt := fn(x: int) -> int: x + 1\n";
    const SF: &str = "struct S:\n    n: int\nfn S(n: int) -> int:\n    return n\n";
    const SL: &str = "struct S:\n    n: int\nS := fn(n: int) -> int: n * 100\n";
    const SN: &str = "struct S:\n    n: int\nS := 5\n";
    const LO: &str = "g := fn(a: int, b: int) -> int: a * 10 + b\n";
    const CO: &str = "Y: const int = 5\n";
    const PL: &str = "Y: int = 5\n";
    const VAL: &str = "need a binding that holds one known function";
    const COPY: &str = "is this task's copy";
    const WF: &str =
        "fn w(xs: List[int], out: Channel[int]):\n    xs.push(3)\n    out.send(xs.len())\n";
    const WN: &str = "fn w(xs: List[int], out: Channel[int]) -> int:\n    return xs.len()\nw := fn(xs: List[int], out: Channel[int]) -> int: wpush(xs)\nfn wpush(xs: List[int]) -> int:\n    xs.push(3)\n    return 0\n";
    const WL: &str = "protocol Zero:\n    fn zero() -> Self\nstruct N:\n    v: int\n\n    fn zero() -> N:\n        return N(v=0)\nfn mk[T: Zero](x: T) -> T:\n    return T.zero()\n";
    const RE_Y: &str = "cannot reassign 'Y' — it is declared const in module 'lib'";
    let ft = format!("{FO}{LET_F}");
    let lf = format!("{LET_F}{FO}");
    let sfl = format!("{SF}S := fn(m: int) -> int: m * 100\n");
    let wo = format!("{WF}w := fn(xs: List[int], out: Channel[int]) -> int: xs.len()\n");
    let wll = format!("{WL}mk := fn(x: N) -> N: N(v=1)\n");
    let amb = |x: &str| format!("keyword arguments through '{x}' are ambiguous");
    let konst = |x: &str| format!("cannot re-declare const binding '{x}'");
    let par = |i: &str, c: &str| {
        format!(
            "import std.concurrency\n{i}\nfn main():\n    xs := [1]\n    out := Channel[int](8)\n    parallel:\n        {c}\n    print(xs)\nmain()\n"
        )
    };
    let (amb_f, amb_s, konst_y, konst_pi) = (amb("f"), amb("S"), konst("Y"), konst("pi"));
    let cells: Vec<(&str, String, String, Option<&str>)> = vec![
        ("fn_only_all_paths", FO.into(), "import lib\nimport f from lib\nprint(lib.f(a=1, b=2))\nprint(f(b=2, a=1))\nprint(lib.f(1))\nprint(f(1))\nh := lib.f\nprint(h(a=1))\n".into(), None),
        ("fn_then_let_qualified_kw", ft.clone(), "import lib\nprint(lib.f(a=1, b=2))\n".into(), Some(&amb_f)),
        ("fn_then_let_from_kw", ft.clone(), "import f from lib\nprint(f(a=1, b=2))\n".into(), Some(&amb_f)),
        ("fn_then_let_defer_kw", ft.clone(), "import lib\nfn g():\n    defer lib.f(a=1, b=2)\ng()\n".into(), Some(&amb_f)),
        ("fn_then_let_spawn_kw", ft.clone(), "import f from lib\nfn g():\n    spawn f(a=1, b=2)\ng()\n".into(), Some(&amb_f)),
        ("fn_then_let_qualified_default", ft.clone(), "import lib\nprint(lib.f(1))\n".into(), Some("expects 2 argument(s), got 1")),
        ("fn_then_let_from_default", ft.clone(), "import f from lib\nprint(f(1))\n".into(), Some("expects 2 argument(s), got 1")),
        ("fn_then_let_qualified_value_kw", ft.clone(), "import lib\nh := lib.f\nprint(h(a=1, b=2))\n".into(), Some(VAL)),
        ("fn_then_let_from_value_kw", ft.clone(), "import f from lib\nh := f\nprint(h(a=1, b=2))\n".into(), Some(VAL)),
        ("fn_then_let_qualified_value_default", ft.clone(), "import lib\nh := lib.f\nprint(h(1))\n".into(), Some("expects 2 argument(s), got 1")),
        ("fn_then_let_positional", ft.clone(), "import lib\nimport f from lib\nprint(lib.f(1, 2))\nprint(f(1, 2))\n".into(), None),
        ("let_then_fn_qualified_kw", lf.clone(), "import lib\nprint(lib.f(a=1, b=2))\n".into(), Some(&amb_f)),
        ("let_then_fn_from_default", lf.clone(), "import f from lib\nprint(f(1))\n".into(), Some("expects 2 argument(s), got 1")),
        ("variadic_then_let_two", VF.into(), "import lib\nprint(lib.f(1, 2))\n".into(), None),
        ("variadic_then_let_qualified_three", VF.into(), "import lib\nprint(lib.f(1, 2, 3))\n".into(), Some("expects 2 argument(s), got 3")),
        ("variadic_then_let_from_three", VF.into(), "import f from lib\nprint(f(1, 2, 3))\n".into(), Some("expects 2 argument(s), got 3")),
        // Any error: the slot holds a non-generic fn.
        ("generic_then_let_turbofish", GF.into(), "import lib\nprint(lib.idt[int](1))\n".into(), Some("")),
        ("generic_then_let_plain", GF.into(), "import lib\nprint(lib.idt(1))\n".into(), None),
        ("struct_fn_qualified_kw", SF.into(), "import lib\nprint(lib.S(n=2))\n".into(), None),
        ("struct_fn_let_qualified_kw", sfl.clone(), "import lib\nprint(lib.S(n=2))\n".into(), Some(&amb_s)),
        ("struct_let_fn_calls_slot", SL.into(), "import lib\nx: int = lib.S(2)\nprint(x)\n".into(), None),
        ("struct_let_int_keeps_ctor", SN.into(), "import lib\nprint(lib.S(2))\n".into(), None),
        ("let_only_closure_qualified_kw", LO.into(), "import lib\nprint(lib.g(a=1, b=2))\n".into(), Some(VAL)),
        ("const_from_import_colon_eq", CO.into(), "import Y from lib\nY := 7\nprint(Y)\n".into(), Some(&konst_y)),
        ("const_from_import_typed_let", CO.into(), "import Y from lib\nY: int = 7\nprint(Y)\n".into(), Some(&konst_y)),
        ("const_from_import_fn", CO.into(), "import Y from lib\nfn Y() -> int:\n    return 1\n".into(), Some(&konst_y)),
        ("const_from_import_inner_shadow", CO.into(), "import Y from lib\nfn g():\n    Y := 3\n    print(Y)\ng()\n".into(), None),
        ("const_from_import_assign", CO.into(), "import Y from lib\nY = 7\n".into(), Some(RE_Y)),
        ("const_from_import_compound", CO.into(), "import Y from lib\nY += 1\n".into(), Some(RE_Y)),
        ("const_list_from_import_extend", "XS: const List[int] = [1]\n".into(), "import XS from lib\nXS += [2]\n".into(), Some("cannot reassign 'XS' — it is declared const in module 'lib'")),
        ("const_qualified_assign", CO.into(), "import lib\nlib.Y = 3\n".into(), Some(RE_Y)),
        ("plain_from_import_colon_eq", PL.into(), "import Y from lib\nY := 7\nprint(Y)\n".into(), None),
        ("native_const_from_import_colon_eq", PL.into(), "import pi from std.math\npi := 3.0\nprint(pi)\n".into(), Some(&konst_pi)),
        ("write_fn_only_qualified_spawn", WF.into(), par("import lib", "spawn lib.w(xs, out)"), Some(COPY)),
        ("write_fn_only_from_spawn_block", WF.into(), par("import w from lib", "spawn: w(xs, out)"), Some(COPY)),
        ("write_old_qualified_spawn", wo.clone(), par("import lib", "spawn lib.w(xs, out)"), None),
        ("write_old_from_spawn", wo.clone(), par("import w from lib", "spawn w(xs, out)"), None),
        ("write_old_qualified_spawn_block", wo.clone(), par("import lib", "spawn: lib.w(xs, out)"), None),
        // The runtime layer C fault owns this write; the checker declines, as for a same-module
        // value callee.
        ("write_new_qualified_spawn", WN.into(), par("import lib", "spawn lib.w(xs, out)"), None),
        ("write_new_from_spawn", WN.into(), par("import w from lib", "spawn w(xs, out)"), None),
        ("witness_fn_only_charges", WL.into(), "import lib\nfn g[T: lib.Zero](x: T) -> T:\n    return lib.mk(x)\nh := g\nprint(g(lib.N(v=5)).v)\n".into(), Some("cannot be used as a function value")),
        ("witness_then_let_calls_slot", wll.clone(), "import lib\nfn g[T: lib.Zero](x: T) -> T:\n    return lib.mk(x)\nprint(g(lib.N(v=5)).v)\n".into(), Some("found T")),
    ];
    let mut wrong = Vec::new();
    for (name, lib, main, want) in &cells {
        let errs = check_files(&[("lib.chz", lib), ("main.chz", main)]);
        let pass = match want {
            None => errs.is_empty(),
            Some(needle) => !errs.is_empty() && errs.iter().any(|e| e.message.contains(needle)),
        };
        if !pass {
            let msgs: Vec<&str> = errs.iter().map(|e| e.message.as_str()).collect();
            wrong.push(format!("cell {name}: want {want:?}, got {msgs:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} wrong cell(s):\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

#[test]
fn method_own_param_does_not_collide_with_receiver_param_by_name() {
    // `Box[U].pair[T](t)` is `(U, T)`; the receiver's `T` and the method's `U` must be substituted
    // once, keyed on the declaration, not re-substituted by bare name in a second pass.
    let src = "struct Box[T]:\n    v: T\n    fn pair[U](self, u: U) -> (T, U):\n        return (self.v, u)\nfn go2[T, U](b: Box[U], t: T) -> (T, T):\n    return b.pair(t)\n";
    let errs = check_src(src);
    assert!(
        !errs.is_empty(),
        "expected a type error for returning (U, T) as (T, T), got none"
    );
}

#[test]
fn fn_value_param_named_like_prelude_map_param_is_accepted() {
    ok("fn h[U, T](xs: List[U], f: fn(U) -> T) -> List[T]:\n    return xs.map(f)\n");
}

/// Run `(name, src, clean)` cells through `check_entry` (std imports resolve); fail listing every
/// cell whose verdict is wrong (`clean` = no error wanted).
fn assert_clean_grid(cells: &[(String, String, bool)]) {
    let cells: Vec<_> = cells
        .iter()
        .map(|(n, s, c)| (n.clone(), String::new(), s.clone(), *c))
        .collect();
    assert_clean_files_grid(&cells);
}

/// [`assert_clean_grid`] over a two-module graph: each cell is `(name, lib, main, clean)`, checked
/// with `lib.chz` beside the entry `main.chz`.
fn assert_clean_files_grid(cells: &[(String, String, String, bool)]) {
    let mut wrong = Vec::new();
    for (name, lib, main, clean) in cells {
        let errs = check_files(&[("lib.chz", lib), ("main.chz", main)]);
        if errs.is_empty() != *clean {
            let msgs: Vec<&str> = errs.iter().map(|e| e.message.as_str()).collect();
            wrong.push(format!("cell {name}: want clean={clean}, got {msgs:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} wrong cell(s):\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// TICKET-197 (N5): an operator or direct hook method conforms to its protocol's parameter names
/// (`Checker::hook_impl` asks `param_name_mismatch`), and the operator error names the misnamed
/// parameter. `==` is not governed by the name rule (DEC-187).
#[test]
fn hook_param_name_grid() {
    let arith = |m: &str, op: &str, p: &str| {
        (
            format!("    fn {m}(self, {p}: V) -> V:\n        return V(n=self.n + {p}.n)\n"),
            format!("x := V(1) {op} V(2)\nprint(x.n)\n"),
        )
    };
    // (hook, method text, use, misnamed parameter or None for the protocol's names)
    let mut cells: Vec<(String, String, String, Option<&str>)> = Vec::new();
    for (m, op) in [("add", "+"), ("sub", "-"), ("mul", "*")] {
        for p in ["o", "other"] {
            let (meth, use_) = arith(m, op, p);
            cells.push((m.to_string(), meth, use_, (p == "o").then_some("o")));
        }
    }
    for p in ["o", "other"] {
        cells.push((
            "compare".to_string(),
            format!("    fn eq(self, other: V) -> bool:\n        return self.n == other.n\n    fn compare(self, {p}: V) -> int:\n        return self.n - {p}.n\n"),
            "print(V(1) < V(2))\n".to_string(),
            (p == "o").then_some("o"),
        ));
    }
    for (p, bad) in [("i", true), ("k", false)] {
        cells.push((
            "index".to_string(),
            format!("    fn index(self, {p}: int) -> int:\n        return self.n + {p}\n"),
            "print(V(3)[2])\n".to_string(),
            bad.then_some("i"),
        ));
    }
    for (p, bad) in [("x", true), ("item", false)] {
        cells.push((
            "contains".to_string(),
            format!("    fn contains(self, {p}: int) -> bool:\n        return self.n == {p}\n"),
            "print(3 in V(3))\n".to_string(),
            bad.then_some("x"),
        ));
    }
    for (a, bad) in [("a", true), ("r", false)] {
        cells.push((
            "slice".to_string(),
            format!("    fn slice(self, {a}: Range) -> int:\n        return self.n\n"),
            "print(V(1)[1..2])\n".to_string(),
            bad.then_some("a"),
        ));
    }
    for ((k, v), bad) in [(("i", "val"), true), (("k", "v"), false)] {
        cells.push((
            "set_index".to_string(),
            format!("    fn index(self, k: int) -> int:\n        return self.n\n    fn set_index(self, {k}: int, {v}: int) -> None:\n        self.n = {v}\n"),
            "v := V(1)\nv[0] = 4\nprint(v.n)\n".to_string(),
            bad.then_some("i"),
        ));
    }
    cells.push((
        "eq".to_string(),
        "    fn eq(self, o: V) -> bool:\n        return self.n == o.n\n".to_string(),
        "print(V(1) == V(1))\n".to_string(),
        None,
    ));
    let mut wrong = Vec::new();
    for (hook, meth, use_, bad) in &cells {
        let src = format!("struct V:\n    n: int\n{meth}{use_}");
        let errs = check_src(&src);
        let msgs: Vec<&str> = errs.iter().map(|e| e.message.as_str()).collect();
        let verdict = match bad {
            None => errs.is_empty(),
            Some(p) => {
                let needle = format!("parameter 1 is named '{p}'");
                msgs.iter().any(|m| m.contains(&needle))
            }
        };
        if !verdict {
            wrong.push(format!("cell {hook} misnamed={bad:?}: got {msgs:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} wrong cell(s):\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// TICKET-197 (N4): a turbofish head is a generic fn value wherever `generic_fn_value_sig` finds
/// one: same module, from-import or `m.f`, in value, call, argument and parameter-default position.
/// A local that shadows the imported name is indexed as itself.
#[test]
fn fn_turbofish_head_grid() {
    const LIB: &str = "fn idt[T](x: T) -> T:\n    return x\n";
    let uses = |h: &str| {
        [
            ("value", format!("i := {h}[int]\nprint(i(4))\n")),
            ("call", format!("print({h}[int](4))\n")),
            ("argument", format!("print([1].map({h}[int]))\n")),
            (
                "default",
                format!(
                    "fn run(f: fn(int) -> int = {h}[int]) -> int:\n    return f(9)\nprint(run())\n"
                ),
            ),
        ]
    };
    let heads = [
        ("same_module", String::new(), LIB.to_string(), "idt"),
        (
            "from_import",
            LIB.to_string(),
            "import idt from lib\n".to_string(),
            "idt",
        ),
        (
            "module_member",
            LIB.to_string(),
            "import lib\n".to_string(),
            "lib.idt",
        ),
    ];
    let mut cells: Vec<(String, String, String, bool)> = Vec::new();
    for (hname, lib, prefix, head) in heads {
        for (pname, body) in uses(head) {
            cells.push((
                format!("{hname}_{pname}"),
                lib.clone(),
                format!("{prefix}{body}"),
                true,
            ));
        }
    }
    cells.push((
        "shadowed".to_string(),
        LIB.to_string(),
        "import idt from lib\nfn k(idt: List[int]) -> int:\n    return idt[0]\nprint(k([7]))\n"
            .to_string(),
        true,
    ));
    cells.push((
        "shadowed_module".to_string(),
        LIB.to_string(),
        "import lib\nstruct S:\n    idt: List[int]\nfn k(lib: S) -> int:\n    return lib.idt[0]\nprint(k(S(idt=[7])))\n"
            .to_string(),
        true,
    ));
    assert_clean_files_grid(&cells);
    // A turbofish carries one type argument (`f[int, str]` does not parse), so an imported head's
    // arity error is a two-parameter fn given one.
    let errs = check_files(&[
        ("lib.chz", "fn two[A, B](a: A, b: B) -> A:\n    return a\n"),
        ("main.chz", "import two from lib\ni := two[int]\n"),
    ]);
    assert!(
        errs.iter()
            .any(|e| e.message.contains("expects 2 type argument")),
        "{errs:?}"
    );
}

#[test]
fn method_type_param_capture_grid() {
    // A generic method's own type params and the receiver's type args are substituted with ONE
    // map (`instantiate_method`), so a caller param named like the method's never collapses into
    // it. Receivers x binding (arg, turbofish, hint) x caller names (disjoint, equal, swapped).
    const BOX: &str = "struct Box[T]:\n    v: T\n    fn pair[U](self, u: U) -> (T, U):\n        return (self.v, u)\n";
    const ENU: &str = "enum E[T]:\n    A(T)\n    fn pair[U](self, u: U) -> (T, U):\n        match self:\n            E.A(v): return (v, u)\n";
    let recvs = [
        ("struct", BOX, "Box[Q]", "(Q, P)"),
        ("enum", ENU, "E[Q]", "(Q, P)"),
    ];
    let mut cells: Vec<(String, String, bool)> = Vec::new();
    for (rname, decl, ty, want) in recvs {
        for (p, q) in [("A", "B"), ("T", "U"), ("U", "T")] {
            let ty = ty.replace('Q', q);
            let want = want.replace('Q', q).replace('P', p);
            let head = format!("{decl}fn go[{p}, {q}](b: {ty}, t: {p}) -> {want}:\n");
            let bodies = [
                ("arg", "    return b.pair(t)\n".to_string()),
                ("turbofish", format!("    return b.pair[{p}](t)\n")),
                ("hint", format!("    r: {want} = b.pair(t)\n    return r\n")),
            ];
            for (bname, body) in bodies {
                cells.push((
                    format!("{rname}_{p}{q}_{bname}"),
                    format!("{head}{body}"),
                    true,
                ));
            }
            let bad = format!(
                "{decl}fn go[{p}, {q}](b: {ty}, t: {p}) -> ({p}, {p}):\n    return b.pair(t)\n"
            );
            cells.push((format!("{rname}_{p}{q}_wrong_return"), bad, false));
        }
    }
    let natives = [
        (
            "list_map",
            "fn h[U, T](xs: List[U], f: fn(U) -> T) -> List[T]:\n    return xs.map(f)\n",
            true,
        ),
        (
            "list_fold",
            "fn h[U, T](xs: List[U], init: T, f: fn(T, U) -> T) -> T:\n    return xs.fold(init, f)\n",
            true,
        ),
        (
            "list_sort_by_key",
            "fn s[K](xs: List[K], f: fn(K) -> int):\n    xs.sort_by_key(f)\n",
            true,
        ),
        (
            "rwshared_fold",
            "import std.concurrency\nimport ConcurrentList from std.concurrency.collection\nfn g[R](s: ConcurrentList[R]) -> int:\n    return s.fold(0, fn(a: int, x: R) -> int: a + 1)\n",
            true,
        ),
        (
            "list_map_wrong_return",
            "fn h[U, T](xs: List[U], f: fn(U) -> T) -> List[U]:\n    return xs.map(f)\n",
            false,
        ),
    ];
    for (n, src, clean) in natives {
        cells.push((n.to_string(), src.to_string(), clean));
    }
    assert_clean_grid(&cells);
}

#[test]
fn fn_value_slot_grid() {
    // A fn value carries its declaration's call slots (`FnLabels.slots`), so a call through a value
    // certain to hold one fn fills defaults and packs a variadic exactly like a direct call. Shapes x
    // bindings (top-level, from-import, m.f, nested fn, turbofish) x calls (positional, omitted,
    // keyword).
    const WD: &str = "protocol Zero:\n    fn zero() -> Self\nstruct N:\n    v: int\n\n    fn zero() -> N:\n        return N(v=0)\n";
    const WF: &str = "fn v[T: Zero](x: T) -> T:\n    return T.zero()\n";
    let wl = format!("{WD}{WF}");
    let shapes: [(&str, &str, [&str; 3]); 5] = [
        (
            "d",
            "fn v(a: int, b: int = 10) -> int:\n    return a * 100 + b\n",
            ["w(1, 2)", "w(1)", "w(b=5, a=1)"],
        ),
        (
            "v",
            "fn v(a: int, ...rest: int) -> int:\n    return a * 100 + rest.len()\n",
            ["w(1, 2, 3)", "w(1)", "w(a=1)"],
        ),
        (
            "dv",
            "fn v(a: int, b: int = 10, ...rest: int) -> int:\n    return a * 100 + b + rest.len()\n",
            ["w(1, 2, 3)", "w(1)", "w(1, b=5)"],
        ),
        (
            "vk",
            "fn v(...xs: int, tail: int = 3) -> int:\n    return xs.len() * 100 + tail\n",
            ["w(1, 2)", "w()", "w(1, tail=9)"],
        ),
        (
            "dd",
            "fn v(a: int = 1, b: int = 2) -> int:\n    return a * 10 + b\n",
            ["w(1)", "w()", "w(b=5)"],
        ),
    ];
    let indent = |s: &str| s.lines().map(|l| format!("    {l}\n")).collect::<String>();
    let mut cells: Vec<(String, String, String, bool)> = Vec::new();
    for (sname, shape, calls) in shapes {
        for (cname, call) in ["positional", "omitted", "keyword"].iter().zip(calls) {
            let tf_shape = shape.replacen("fn v(", "fn v[T](x: T, ", 1);
            let tf_call = if call == "w()" {
                "w(\"x\")".to_string()
            } else {
                call.replacen("w(", "w(\"x\", ", 1)
            };
            let bindings = [
                (
                    "toplevel",
                    String::new(),
                    format!("{shape}w := v\nprint({call})\n"),
                ),
                (
                    "from_import",
                    shape.to_string(),
                    format!("import v from lib\nw := v\nprint({call})\n"),
                ),
                (
                    "module_member",
                    shape.to_string(),
                    format!("import lib\nw := lib.v\nprint({call})\n"),
                ),
                (
                    "nested",
                    String::new(),
                    format!(
                        "fn outer():\n{}    w := v\n    print({call})\nouter()\n",
                        indent(shape)
                    ),
                ),
                (
                    "turbofish",
                    String::new(),
                    format!("{tf_shape}w := v[str]\nprint({tf_call})\n"),
                ),
            ];
            for (bname, lib, main) in bindings {
                cells.push((format!("{sname}_{bname}_{cname}"), lib, main, true));
            }
        }
    }
    // A witness-taking fn is never a value, in any binding.
    let witness = [
        ("toplevel", String::new(), format!("{wl}w := v\n")),
        (
            "from_import",
            wl.clone(),
            "import v from lib\nw := v\n".to_string(),
        ),
        (
            "module_member",
            wl.clone(),
            "import lib\nw := lib.v\n".to_string(),
        ),
        (
            "nested",
            String::new(),
            format!("{WD}fn outer():\n{}    w := v\nouter()\n", indent(WF)),
        ),
    ];
    for (bname, lib, main) in witness {
        cells.push((format!("w_{bname}"), lib, main, false));
    }
    // An alias waits on its source: a write to EITHER name voids a keyword call through the alias.
    const VU: &str = "    fn v(a: int, b: int = 10) -> int:\n        return a * 100 + b\n    fn u(b: int, a: int = 10) -> int:\n        return a * 100 + b\n";
    cells.push((
        "ambiguous_alias_loop".to_string(),
        String::new(),
        format!("fn outer():\n{VU}    for _i in 0..2:\n        w := v\n        print(w(b=5, a=1))\n        v = u\nouter()\n"),
        false,
    ));
    cells.push((
        "ambiguous_alias_inner".to_string(),
        String::new(),
        format!("fn outer():\n{VU}    w := v\n    if true:\n        z := w\n        print(z(b=5, a=1))\n    w = u\nouter()\n"),
        false,
    ));
    assert_clean_files_grid(&cells);
}

#[test]
fn default_reads_enclosing_scope_not_same_named_param() {
    ok_desugared("k := 1\nfn f(k: int = k) -> int:\n    return k\nprint(f())\n");
}

/// TICKET-201: the field that owns a default is not a struct-body binder of it, so `k: int = k`
/// reads module `k` (CPython dataclass: `1`).
#[test]
fn n3_default_owner_is_not_its_own_class_body_binder() {
    ok_desugared("k := 1\nstruct S:\n    k: int = k\nprint(S().k)\n");
}

/// TICKET-201: a default runs in module scope, so an earlier parameter is not bound there.
#[test]
fn n3_default_param_reference_is_an_unknown_name() {
    rejects_desugared(
        "fn f(a: int, b: int = a) -> int:\n    return b\n",
        "unknown name 'a'",
    );
}

/// TICKET-201: a default naming an earlier parameter reads the module binding of that name.
#[test]
fn n3_default_reads_the_global_not_the_param() {
    ok_desugared("a := 100\nfn f(a: int, b: int = a) -> int:\n    return b\nprint(f(1))\n");
}

/// TICKET-201: CPython evaluates a default in the class body, where an earlier field or method
/// is bound; Chezzi runs it in module scope, so the read is declined, not silently rebound.
#[test]
fn n3_default_reading_a_class_body_binder_is_declined() {
    rejects_desugared(
        "a := 100\nstruct S:\n    a: int = 1\n    b: int = a\nprint(S().b)\n",
        "is bound in the body of 'S'",
    );
    rejects_desugared(
        "a := 100\nstruct S:\n    a: int = 1\n    fn m(self, x: int = a) -> int:\n        return x\nprint(S().m())\n",
        "is bound in the body of 'S'",
    );
}

/// TICKET-201 (S3): `Self` in a field default is an unknown name, never a callee-filled binder;
/// a ctor has no callee, so no fill or arity error may join the true diagnostic.
#[test]
fn n3_field_default_naming_self_reports_no_fill_error() {
    for fields in [
        "    n: int = Self.mk()\n    m: int = 2\n",
        "    m: int = 2\n    n: int = Self.mk()\n",
    ] {
        let src =
            format!("struct A:\n{fields}    fn mk() -> int:\n        return 41\nprint(A())\n");
        let errs = check_desugared(&src);
        // Exactly one: the default is typed at its declaration and again inlined at `A()`, and the
        // same diagnostic at the same span is one finding (`finish_diags`).
        assert_eq!(errs.len(), 1, "expected one error for {src:?}: {errs:?}");
        for e in &errs {
            assert!(
                e.message.contains("unknown name 'Self'")
                    && !e.message.contains("filled by the callee")
                    && !e.message.contains("expects"),
                "unexpected error for {src:?}: {errs:?}"
            );
        }
    }
}

#[test]
fn bound_type_arg_abstract_mismatch_rejected() {
    rejects(
        "protocol Conv[S]:\n    fn conv(self) -> S\nstruct I:\n    n: int\n    fn conv(self) -> str:\n        return \"i{self.n}\"\nfn need[S, B: Conv[S]](u: B, s: S) -> S:\n    return u.conv()\nfn go[X, Y: Conv[str]](y: Y, x: X) -> X:\n    return need(y, x)\nprint(go(I(n=1), 5) + 1)\n",
        "does not satisfy Conv[",
    );
}

/// TICKET-202 (Family G3): a bound's type arguments mean what they mean at the DECLARATION, and an
/// abstract argument satisfies `P[X]` only through its own bound `P[X]` for the same `X` (Rust
/// E0277, Go "does not satisfy"). One grid over every bound site x every argument kind; each wrong
/// cell is collected so a single run names all of them.
#[test]
fn bound_type_arg_grid() {
    const H: &str = "protocol Conv[S]:\n    fn conv(self) -> S\nstruct I:\n    n: int\n    fn conv(self) -> str:\n        return \"i{self.n}\"\nstruct J:\n    n: int\n    fn conv(self) -> int:\n        return 7\n";
    let mut wrong: Vec<String> = Vec::new();
    let mut cell = |name: String, src: String, yes: bool, needle: &str| {
        let errs: Vec<String> = check_src(&src)
            .into_iter()
            .filter(|e| e.severity == Severity::Error)
            .map(|e| e.message)
            .collect();
        let good = if yes {
            errs.is_empty()
        } else {
            errs.iter().any(|m| m.contains(needle))
        };
        if !good {
            wrong.push(format!("{name}: {errs:?}"));
        }
    };
    let callers: [(&str, &str, bool); 6] = [
        ("concrete_ok", "fn go(y: I, x: str) -> str", true),
        ("concrete_mismatch", "fn go(y: J, x: str) -> str", false),
        (
            "param_matching",
            "fn go[X, Y: Conv[X]](y: Y, x: X) -> X",
            true,
        ),
        (
            "param_mismatch",
            "fn go[X, Y: Conv[str]](y: Y, x: X) -> X",
            false,
        ),
        ("param_unbound", "fn go[X, Y](y: Y, x: X) -> X", false),
        (
            "param_same_name",
            "fn go[S, B: Conv[str]](y: B, x: S) -> S",
            false,
        ),
    ];
    let sites: [(&str, &str, &str); 7] = [
        (
            "fn_call",
            "fn need[S, B: Conv[S]](u: B, s: S) -> S:\n    return u.conv()\n",
            "need(y, x)",
        ),
        (
            "fn_where",
            "fn need[S, B](u: B, s: S) -> S where B: Conv[S]:\n    return u.conv()\n",
            "need(y, x)",
        ),
        (
            "static_call",
            "struct M:\n    n: int\n    fn mk[S, B: Conv[S]](u: B, s: S) -> S:\n        return u.conv()\n",
            "M.mk(y, x)",
        ),
        (
            "struct_ctor",
            "struct W[S, B: Conv[S]]:\n    s: S\n    b: B\n",
            "W(s=x, b=y).s",
        ),
        (
            "method_own",
            "struct Bx:\n    n: int\n    fn pick[S, B: Conv[S]](self, u: B, s: S) -> S:\n        return u.conv()\n",
            "Bx(n=0).pick(y, x)",
        ),
        (
            "method_recv",
            "struct Box[S]:\n    v: S\n    fn pick[B: Conv[S]](self, u: B) -> S:\n        return u.conv()\n",
            "Box(v=x).pick(y)",
        ),
        (
            "method_where",
            "struct Box[S]:\n    v: S\n    fn pick[B](self, u: B) -> S where B: Conv[S]:\n        return u.conv()\n",
            "Box(v=x).pick(y)",
        ),
    ];
    for (site, decls, call) in sites {
        for (arg, sig, yes) in callers {
            cell(
                format!("{site} x {arg}"),
                format!("{H}{decls}{sig}:\n    return {call}\n"),
                yes,
                "does not satisfy Conv",
            );
        }
    }
    // Conditional conformance: `Box[S]` provides `Show` only where `S: Conv[str]`.
    let cond = "protocol Show:\n    fn show(self) -> str\nstruct Box[S]:\n    v: S\n    fn show(self) -> str where S: Conv[str]:\n        return self.v.conv()\nfn sh[A: Show](a: A) -> str:\n    return a.show()\n";
    for (arg, sig, yes) in [
        ("concrete_ok", "fn go(y: I) -> str", true),
        ("concrete_mismatch", "fn go(y: J) -> str", false),
        ("param_matching", "fn go[Y: Conv[str]](y: Y) -> str", true),
        ("param_mismatch", "fn go[Y: Conv[int]](y: Y) -> str", false),
        ("param_unbound", "fn go[Y](y: Y) -> str", false),
        ("param_same_name", "fn go[S: Conv[int]](y: S) -> str", false),
    ] {
        cell(
            format!("cond_conf x {arg}"),
            format!("{H}{cond}{sig}:\n    return sh(Box(v=y))\n"),
            yes,
            "does not satisfy Show",
        );
    }
    // A type annotation applies the bound with the annotation's args.
    let w = "struct W[S, B: Conv[S]]:\n    s: S\n    b: B\n";
    cell(
        "annotation x W[str, I]".into(),
        format!("{H}{w}fn g(w: W[str, I]) -> str:\n    return w.s\n"),
        true,
        "",
    );
    cell(
        "annotation x W[int, I]".into(),
        format!("{H}{w}fn f(w: W[int, I]) -> int:\n    return w.s\n"),
        false,
        "does not satisfy Conv",
    );
    // A caller's type param named like a concrete type in the bound must not capture it.
    cell(
        "ctor_shadow".into(),
        format!(
            "{H}struct K:\n    n: int\n    fn conv(self) -> I:\n        return I(n=4)\nstruct W2[B: Conv[I]]:\n    b: B\nfn go[I](k: K, i: I) -> int:\n    return W2(b=k).b.conv().n\n"
        ),
        true,
        "",
    );
    // The bound reaches `Conv` only through a protocol embed line.
    let need = "fn need[S, B: Conv[S]](u: B, s: S) -> S:\n    return u.conv()\n";
    #[allow(clippy::type_complexity)]
    let embed_rows: [(&str, &str, &[(&str, &str, bool)]); 3] = [
        (
            "embed_param",
            "protocol PE[X]:\n    Conv[X]\n",
            &[
                ("concrete_ok", "fn go(y: I, x: str) -> str", true),
                ("concrete_mismatch", "fn go(y: J, x: str) -> str", false),
                (
                    "param_matching",
                    "fn go[X, Y: PE[X]](y: Y, x: X) -> X",
                    true,
                ),
                (
                    "param_mismatch",
                    "fn go[X, Y: PE[str]](y: Y, x: X) -> X",
                    false,
                ),
                ("param_unbound", "fn go[X, Y](y: Y, x: X) -> X", false),
                (
                    "param_same_name",
                    "fn go[S, B: PE[str]](y: B, x: S) -> S",
                    false,
                ),
            ],
        ),
        (
            "embed_concrete",
            "struct T:\n    n: int\nprotocol PT:\n    Conv[T]\n",
            &[
                ("concrete_ok", "fn go[Y: PT](y: Y, x: T) -> T", true),
                ("param_same_name", "fn go[T, Y: PT](y: Y, x: T) -> T", false),
            ],
        ),
        (
            "embed_nested",
            "protocol PL[X]:\n    Conv[List[X]]\n",
            &[
                (
                    "param_matching",
                    "fn go[X, Y: PL[X]](y: Y, x: List[X]) -> List[X]",
                    true,
                ),
                (
                    "param_mismatch",
                    "fn go[X, Y: PL[X]](y: Y, x: X) -> X",
                    false,
                ),
            ],
        ),
    ];
    for (row, decls, sigs) in embed_rows {
        for &(arg, sig, yes) in sigs {
            cell(
                format!("{row} x {arg}"),
                format!("{H}{decls}{need}{sig}:\n    return need(y, x)\n"),
                yes,
                "does not satisfy Conv",
            );
        }
    }
    // A static protocol requirement called through the bounded param.
    let mk = "protocol Mk[S]:\n    fn mk(s: S) -> Self\nstruct P:\n    n: int\n    fn mk(s: str) -> P:\n        return P(n=1)\nstruct Q:\n    n: int\n    fn mk(s: int) -> Q:\n        return Q(n=2)\n";
    let need_mk = "fn need[S, B: Mk[S]](y: B, s: S) -> B:\n    return B.mk(s)\n";
    for (arg, sig, yes) in [
        ("concrete_ok", "fn go(y: P, x: str) -> P", true),
        ("concrete_mismatch", "fn go(y: Q, x: str) -> Q", false),
        (
            "param_matching",
            "fn go[X, Y: Mk[X]](y: Y, x: X) -> Y",
            true,
        ),
        (
            "param_mismatch",
            "fn go[X, Y: Mk[str]](y: Y, x: X) -> Y",
            false,
        ),
        ("param_unbound", "fn go[X, Y](y: Y, x: X) -> Y", false),
        (
            "param_same_name",
            "fn go[S, B: Mk[str]](y: B, x: S) -> B",
            false,
        ),
    ] {
        cell(
            format!("witness_static x {arg}"),
            format!("{H}{mk}{need_mk}{sig}:\n    return need(y, x)\n"),
            yes,
            "does not satisfy Mk",
        );
    }
    // The in-scope bound read by a witness call in the body.
    cell(
        "witness_body x param_matching".into(),
        format!("{H}{mk}fn go[X, Y: Mk[X]](y: Y, x: X) -> Y:\n    return Y.mk(x)\n"),
        true,
        "",
    );
    cell(
        "witness_body x param_mismatch".into(),
        format!("{H}{mk}fn go[X, Y: Mk[str]](y: Y, x: X) -> Y:\n    return Y.mk(x)\n"),
        false,
        "expected str, found X",
    );
    // A generic struct's static method with a receiver-param bound.
    let bs = "struct Bs[S]:\n    v: S\n    fn mk[B: Conv[S]](u: B, s: S) -> S:\n        return u.conv()\n";
    for (arg, sig, yes) in callers {
        cell(
            format!("static_recv x {arg}"),
            format!("{H}{bs}{sig}:\n    return Bs.mk(y, x)\n"),
            yes,
            "does not satisfy Conv",
        );
    }
    // `S` is never inferred: the verdict must not depend on the caller's spelling.
    let by = "struct By[S]:\n    v: S\n    fn mk[B: Conv[S]](u: B) -> int:\n        return 1\n";
    for (arg, sig) in [
        ("caller_S", "fn go[S, Y: Conv[S]](y: Y) -> int"),
        ("caller_X", "fn go[X, Y: Conv[X]](y: Y) -> int"),
    ] {
        cell(
            format!("static_uninf x {arg}"),
            format!("{H}{by}{sig}:\n    return By.mk(y)\n"),
            true,
            "",
        );
    }
    cell(
        "static_turbo x str".into(),
        format!("{H}{by}fn go[Y: Conv[str]](y: Y) -> int:\n    return By[str].mk(y)\n"),
        true,
        "",
    );
    cell(
        "static_turbo x int".into(),
        format!("{H}{by}fn go[Y: Conv[str]](y: Y) -> int:\n    return By[int].mk(y)\n"),
        false,
        "does not satisfy Conv",
    );
    // A receiver `where` bound is substituted by the receiver map ONCE: a caller param named like a
    // receiver param (`A`) must not be rewritten a second time. One row per instance-method arm.
    let swap_hosts = [
        (
            "struct",
            "struct P[A, B]:\n    a: A\n    fn m(self, b: B) -> B where A: Conv[B]:\n        return b\n",
        ),
        (
            "enum",
            "enum P[A, B]:\n    V(A)\n    fn m(self, b: B) -> B where A: Conv[B]:\n        return b\n",
        ),
    ];
    for (host, decl) in swap_hosts {
        for (arg, sig, yes) in [
            (
                "param_matching",
                "fn go[A, Y: Conv[A]](p: P[Y, A], a: A) -> A",
                true,
            ),
            (
                "param_swapped",
                "fn go[A, Y: Conv[Y]](p: P[Y, A], a: A) -> A",
                false,
            ),
        ] {
            cell(
                format!("method_where_swap_{host} x {arg}"),
                format!("{H}{decl}{sig}:\n    return p.m(a)\n"),
                yes,
                "does not satisfy Conv",
            );
        }
    }
    // An embed arg may name a protocol declared later.
    cell(
        "embed_forward".into(),
        "protocol Conv[S]:\n    fn conv(self) -> S\nprotocol PE:\n    Conv[B]\nprotocol B:\n    fn b(self) -> int\nstruct Z:\n    n: int\n    fn b(self) -> int:\n        return self.n\nstruct Y:\n    n: int\n    fn conv(self) -> B:\n        return Z(n=3)\nfn use_it[T: PE](t: T) -> int:\n    return t.conv().b()\nprint(use_it(Y(n=0)))\n".into(),
        true,
        "",
    );
    assert!(
        wrong.is_empty(),
        "{} wrong cells:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// TICKET-202 (K3b): a method's bound `Conv[T]` names the RECEIVER's `T`, so `Box[str]` accepts an
/// `I` whose `conv` returns `str` (rustc compiles the same program).
#[test]
fn method_bound_reads_the_receiver_arg_ok() {
    ok(
        "protocol Conv[S]:\n    fn conv(self) -> S\nstruct I:\n    n: int\n    fn conv(self) -> str:\n        return \"i{self.n}\"\nstruct Box[T]:\n    v: T\n    fn pick[U: Conv[T]](self, u: U) -> T:\n        return u.conv()\nprint(Box(v=\"s\").pick(I(n=3)).upper())\n",
    );
}

/// TICKET-202 (K3): a caller's own `T` must not capture the method bound's `T`; `Box[int]` needs
/// `Conv[int]` and `I` provides `Conv[str]` (rustc E0277).
#[test]
fn method_bound_receiver_param_not_captured_rejected() {
    rejects(
        "protocol Conv[S]:\n    fn conv(self) -> S\nstruct I:\n    n: int\n    fn conv(self) -> str:\n        return \"i{self.n}\"\nstruct Box[T]:\n    v: T\n    fn pick[U: Conv[T]](self, u: U) -> T:\n        return u.conv()\nfn go[T](b: Box[int], u: I, t: T) -> int:\n    return b.pick(u)\nprint(go(Box(v=1), I(n=3), 0) + 1)\n",
        "does not satisfy Conv",
    );
}

/// TICKET-202 (k16/k2): `T: Conv[str]` forwarded as `B: Conv[A]` with `A := T` needs `Conv[T]`,
/// which `T` does not have; an abstract arg matches only by identity.
#[test]
fn bound_on_forwarded_param_by_identity_rejected() {
    rejects(
        "protocol Conv[S]:\n    fn conv(self) -> S\nstruct I:\n    n: int\n    fn conv(self) -> str:\n        return \"i{self.n}\"\nfn pick[A, B: Conv[A]](v: A, u: B) -> A:\n    return u.conv()\nfn go[T: Conv[str]](t: T) -> T:\n    return pick(t, t)\nr := go(I(n=3))\nprint(r)\n",
        "does not satisfy Conv[",
    );
}

#[test]
fn multi_arg_turbofish_is_a_value() {
    ok(
        "fn pair[A, B](a: A, b: B) -> (A, B):\n    return (a, b)\np := pair[str, int]\nprint(p(\"a\", 1))\n",
    );
}

/// TICKET-213: `Executor(n)` caps the running jobs; the checker accepts zero arguments or one `int`.
#[test]
fn executor_accepts_a_worker_limit() {
    entry_ok(
        "import std.concurrency\nfn main():\n    ex := Executor(2)\n    ex.shutdown()\nmain()\n",
    );
}

#[test]
fn executor_rejects_a_bad_worker_limit() {
    entry_rejects(
        "import std.concurrency\nex := Executor(\"x\")\n",
        "argument 1 of 'Executor': expected int, found str",
    );
    entry_rejects(
        "import std.concurrency\nex := Executor(1, 2)\n",
        "'Executor' expects 0–1 argument(s), got 2",
    );
    entry_ok("import std.concurrency\nex := Executor()\nex.shutdown()\n");
}

#[test]
fn instance_method_called_through_type_path() {
    ok(
        "struct P:\n    n: int\n    fn get(self) -> int:\n        return self.n\nprint(P.get(P(1)))\n",
    );
}

#[test]
fn unknown_type_head_reports_one_error() {
    let errs = check_src("x := Nope[int].make\n");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("unknown name 'Nope'")),
        "want unknown name 'Nope', got {errs:?}"
    );
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("unknown name 'int'")),
        "a type-shaped index after an errored head is not a value: {errs:?}"
    );
}

#[test]
fn native_math_abs_is_a_num_generic_value() {
    entry_ok("import std.math\nfn main():\n    print([-1, 2].map(math.abs))\n");
}

/// TICKET-214 grid: native module fns (math.abs, math.sign, json.decode) x position x type arg, plus Num satisfaction and the operators a T: Num body gets.
#[test]
fn native_fn_and_num_protocol_grid() {
    let num_struct = "struct S:\n    v: int\n    fn add(self, other: S) -> S:\n        return S(self.v + other.v)\n    fn sub(self, other: S) -> S:\n        return S(self.v - other.v)\n    fn mul(self, other: S) -> S:\n        return S(self.v * other.v)\n    fn div(self, other: S) -> S:\n        return S(self.v / other.v)\n    fn mod(self, other: S) -> S:\n        return S(self.v % other.v)\n    fn neg(self) -> S:\n        return S(-self.v)\n    fn compare(self, other: S) -> int:\n        return self.v - other.v\n    fn eq(self, other: S) -> bool:\n        return self.v == other.v\nfn twice[T: Num](x: T) -> T:\n    return x + x\nprint(twice(S(2)).v)";
    let cells: &[(&str, &str, Option<&str>)] = &[
        ("call_int", "print(math.abs(-3), math.sign(-3))", None),
        ("call_float", "print(math.abs(-2.5), math.sign(-2.5))", None),
        (
            "call_targ_int",
            "print(math.abs[int](-3), math.sign[int](-3))",
            None,
        ),
        (
            "call_targ_float",
            "print(math.abs[float](-2.5), math.sign[float](2.5))",
            None,
        ),
        (
            "let_missing",
            "g := math.abs",
            Some("'math.abs' is generic and T is not determined here"),
        ),
        (
            "let_missing_sign",
            "g := math.sign",
            Some("'math.sign' is generic and T is not determined here"),
        ),
        (
            "let_targ_int",
            "g := math.abs[int]\ns := math.sign[int]\nprint(g(-3), s(-3))",
            None,
        ),
        (
            "let_targ_float",
            "g := math.abs[float]\nprint(g(-2.5))",
            None,
        ),
        (
            "typed_let_int",
            "h: fn(int) -> int = math.abs\ns: fn(int) -> int = math.sign\nprint(h(-3), s(-3))",
            None,
        ),
        (
            "typed_let_float",
            "h: fn(float) -> float = math.abs\nprint(h(-2.5))",
            None,
        ),
        (
            "hof_int",
            "print([-1, 2].map(math.abs), [-3, 0, 2].map(math.sign))",
            None,
        ),
        ("hof_float", "print([-1.5, 2.0].map(math.abs))", None),
        ("hof_targ", "print([-1, 2].map(math.abs[int]))", None),
        (
            "paren_call",
            "print((math.abs)(-3), (math.sign)(-2.5))",
            None,
        ),
        (
            "caller_param",
            "fn app[U: Num](x: U) -> U:\n    f: fn(U) -> U = math.abs\n    return f(x)\nprint(app(-4), app(-1.5))",
            None,
        ),
        (
            "spawn_capture",
            "fn main():\n    g := math.abs[int]\n    parallel:\n        spawn:\n            print(g(-4))\nmain()",
            None,
        ),
        (
            "from_import",
            "import abs from std.math\nprint(abs(-3), [-1, 2].map(abs))",
            None,
        ),
        (
            "targ_str",
            "print(math.abs[str](\"x\"))",
            Some("type str does not satisfy Num"),
        ),
        (
            "arg_str",
            "print(math.abs(\"x\"))",
            Some("type str does not satisfy Num"),
        ),
        (
            "arg_bool",
            "print(math.abs(true))",
            Some("type bool does not satisfy Num"),
        ),
        (
            "num_int",
            "fn twice[T: Num](x: T) -> T:\n    return x + x\nprint(twice(2), twice(2.5))",
            None,
        ),
        (
            "num_str",
            "fn twice[T: Num](x: T) -> T:\n    return x + x\nprint(twice(\"a\"))",
            Some("type str does not satisfy Num"),
        ),
        (
            "num_bool",
            "fn twice[T: Num](x: T) -> T:\n    return x\nprint(twice(true))",
            Some("type bool does not satisfy Num"),
        ),
        (
            "num_struct",
            num_struct,
            Some("does not satisfy Num (only int and float do)"),
        ),
        (
            "num_ops",
            "fn ops[T: Num](a: T, b: T) -> T:\n    print(a + b, a - b, a * b, a / b, a % b, -a, a < b, a <= b, a > b, a >= b, a == b, a != b)\n    return a\nops(7, 2)\nops(7.5, 2.0)",
            None,
        ),
        (
            "num_mix",
            "fn both[T: Num](a: T, b: T) -> T:\n    return a\nprint(both(7, 2.0))",
            Some("argument to 'both' has type float, expected int"),
        ),
        (
            "num_lit",
            "fn inc[T: Num](x: T) -> T:\n    return x + 1",
            Some("cannot apply + to T and int"),
        ),
        (
            "num_not",
            "fn f[T: Num](x: T) -> bool:\n    return not x",
            Some("'not' expects bool, found T"),
        ),
        (
            "unsealed_param",
            "fn f[T: Arithmetic + Mod + Neg + Comparable](x: T) -> T:\n    return math.abs(x)",
            Some("type T does not satisfy Num"),
        ),
        ("num_value", "x: Num = 2.5\nprint(x)", None),
        (
            "num_value_abs",
            "x: Num = 2.5\nprint(math.abs(x))",
            Some("cannot use the protocol value Num as type parameter"),
        ),
        (
            "num_reserved",
            "enum Num:\n    A\n    B",
            Some("reserved (builtin)"),
        ),
        ("dec_call", "print(json.decode[int](\"5\"))", None),
        (
            "dec_call_no_targ",
            "print(json.decode(\"5\"))",
            Some("decode takes one type argument and one str argument"),
        ),
        (
            "dec_let",
            "d := json.decode[int]\nprint(d(\"5\"), d(\"x\"))",
            None,
        ),
        (
            "dec_let_float",
            "d := json.decode[float]\nprint(d(\"2.5\"))",
            None,
        ),
        (
            "dec_let_struct",
            "struct P:\n    x: int\n    y: int = 7\nd := json.decode[P]\nprint(d(\"{{}}\"))",
            None,
        ),
        (
            "dec_alias",
            "import std.json as j\nd := j.decode[int]\nprint(d(\"5\"))",
            None,
        ),
        (
            "dec_missing",
            "d := json.decode",
            Some("'json.decode' is generic and T is not determined here"),
        ),
        (
            "dec_typed",
            "d: fn(str) -> int! = json.decode\nprint(d(\"5\"))",
            None,
        ),
        (
            "dec_typed_targ",
            "d: fn(str) -> int! = json.decode[int]\nprint(d(\"5\"))",
            None,
        ),
        (
            "dec_typed_mismatch",
            "d: fn(str) -> str! = json.decode[int]",
            Some("cannot assign fn(str) -> int! to variable of type fn(str) -> str!"),
        ),
        (
            "dec_hof",
            "print([\"5\", \"x\"].map(json.decode[int]))",
            None,
        ),
        (
            "dec_hof_missing",
            "print([\"5\"].map(json.decode))",
            Some("'json.decode' is generic and T is not determined here"),
        ),
        (
            "dec_hof_empty",
            "print([].map(json.decode))",
            Some("'json.decode' is generic and T is not determined here"),
        ),
        (
            "dec_generic_callee",
            "fn app2[V](f: fn(str) -> V!, v: V) -> V!:\n    return f(\"5\")\nprint(app2(json.decode, 3))",
            None,
        ),
        (
            "dec_paren",
            "d := (json.decode[int])\nprint(d(\"5\"))",
            None,
        ),
        (
            "dec_caller_param",
            "fn app[U](s: str) -> U!:\n    f: fn(str) -> U! = json.decode\n    return f(s)",
            Some("decode: cannot decode into U"),
        ),
        (
            "dec_targ_param",
            "fn app[U](s: str) -> U!:\n    f := json.decode[U]\n    return f(s)",
            Some("decode: cannot decode into U"),
        ),
        (
            "dec_bad_target",
            "d := json.decode[Channel[int]]",
            Some("decode: cannot decode into Channel[int]"),
        ),
        (
            "dec_two_targs",
            "d := json.decode[int, str]",
            Some("'json.decode' expects 1 type argument(s), found 2"),
        ),
        (
            "dec_spawn",
            "fn main():\n    d := json.decode[int]\n    parallel:\n        spawn:\n            print(d(\"4\"))\nmain()",
            None,
        ),
        (
            "dec_global_in_fn",
            "d := json.decode[int]\nfn main():\n    print(d(\"5\"))\nmain()",
            None,
        ),
        (
            "dec_from_import",
            "import decode from std.json",
            Some("module 'std.json' has no member 'decode'"),
        ),
        (
            "dec_generic_t_slot",
            "fn id[T](x: T) -> T:\n    return x\nf := id(json.decode)",
            Some("'json.decode' is generic and T is not determined here"),
        ),
        (
            "dec_generic_t_slot_typed",
            "fn id[T](x: T) -> T:\n    return x\nf: fn(str) -> int! = id(json.decode)\nprint(f(\"5\"))",
            None,
        ),
        (
            "dec_some",
            "o := ?json.decode",
            Some("'json.decode' is generic and T is not determined here"),
        ),
        (
            "dec_some_typed",
            "o: (fn(str) -> int!)? = ?json.decode",
            None,
        ),
        (
            "dec_list",
            "xs := [json.decode]",
            Some("'json.decode' is generic and T is not determined here"),
        ),
        (
            "dec_unknown_slot",
            "fn app2[V](f: fn(str) -> V!, v: V) -> V!:\n    return f(\"5\")\nprint(app2(json.decode, []))",
            Some("'json.decode' is generic and T is not determined here"),
        ),
        (
            "dec_arity_slot",
            "fn k[U](f: fn(U, U) -> U) -> int:\n    return 1\nprint(k(json.decode))",
            Some("'json.decode' is generic and T is not determined here"),
        ),
    ];
    let mut red = Vec::new();
    for (name, body, want) in cells {
        let errs = check_entry(&format!("import std.math\nimport std.json\n{body}"));
        let pass = match want {
            None => errs.is_empty(),
            Some(n) => errs.iter().any(|e| e.message.contains(n)),
        };
        if !pass {
            red.push(format!("{name}: {errs:?}"));
        }
    }
    assert!(red.is_empty(), "red cells:\n{}", red.join("\n"));
}

#[test]
fn extern_nested_struct_by_value_is_marshallable() {
    // TICKET-217: a struct field whose type is another C-marshallable struct is accepted as an
    // extern param (raylib `Camera2D {Vector2 offset, Vector2 target, float rotation, float zoom}`).
    entry_ok(
        "import int32 from std.ffi\nstruct In:\n    a: int32\n    b: int32\nstruct Out:\n    i: In\n    c: int32\nextern \"libt.so\":\n    fn take(o: Out) -> int\n",
    );
}

#[test]
fn extern_float32_param_return_field_and_callback_typecheck() {
    // TICKET-217: `float32` is a C `float` width, usable as a param, return, struct field and
    // callback part; the program sees it as `float`.
    entry_ok(
        "import float32 from std.ffi\nstruct V:\n    x: float32\n    y: float32\n\
         extern \"libm\":\n    fn sqrtf(x: float32) -> float32\n    fn vf(v: V) -> V\n    \
         fn cb(x: float32, f: fn(float32) -> float32) -> float32\n\
         r: float = sqrtf(2.0)\n",
    );
}

#[test]
fn extern_float32_requires_import() {
    rejects(
        "extern \"libm\":\n    fn sqrtf(x: float32) -> float32\n",
        "float32",
    );
}

#[test]
fn extern_float32_alias_is_float_across_modules() {
    // TICKET-262: `type Real = float32` is a width type in an alias body, so the alias decl is a
    // rule-1 error, and the extern use of the exported alias cannot launder it. The remaining error
    // is the program's own `s: str = sqrtf(2.0)`, so the alias still reads as `float`.
    let t = TmpDir::new();
    t.write(
        "cdefs.chz",
        "import float32 from std.ffi\n\ntype Real = float32\n",
    );
    let entry = t.write(
        "main.chz",
        "import cdefs\nimport Real from cdefs\n\nextern \"libm\":\n    fn sqrtf(x: Real) -> cdefs.Real\n\
         \nr: float = sqrtf(2.0)\ns: str = sqrtf(2.0)\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let errs = match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert!(
        errs.iter()
            .any(|e| e.message.contains("describes the C boundary only")),
        "expected the rule-1 error for the alias body, got: {errs:?}"
    );
    assert!(
        errs.iter().any(|e| e
            .message
            .contains("cannot assign float to variable of type str")),
        "got: {errs:?}"
    );
}

#[test]
fn extern_nested_struct_depth_two_param_and_return_typecheck() {
    entry_ok(
        "import int32 from std.ffi\nstruct In:\n    a: int32\n    b: int32\nstruct Out:\n    i: In\n    c: int32\n\
         struct Deep:\n    o: Out\n    d: float\nextern \"libt.so\":\n    fn deep_sum(d: Deep) -> float\n    \
         fn deep_make(a: int32, b: int32, c: int32, d: float) -> Deep\n",
    );
}

#[test]
fn extern_recursive_struct_by_value_is_rejected() {
    for src in [
        "import int32 from std.ffi\nstruct Node:\n    v: int32\n    next: Node\n\
         extern \"libt.so\":\n    fn f(n: Node) -> int\n",
        "struct A:\n    b: B\nstruct B:\n    a: A\nextern \"libt.so\":\n    fn f(a: A) -> int\n",
    ] {
        let errs = check_entry(src);
        assert!(
            errs.iter().any(|e| e
                .message
                .contains("is recursively defined and cannot be C-marshallable")),
            "expected a recursive-struct reject for {src:?}, got: {errs:?}"
        );
    }
}

#[test]
fn extern_nested_str_field_names_the_inner_field() {
    let errs = check_entry(
        "import int32 from std.ffi\nstruct N:\n    name: str\nstruct O:\n    n: N\n    k: int32\n\
         extern \"libt.so\":\n    fn f(o: O) -> int\n",
    );
    assert!(
        errs.iter().any(|e| e.message.contains("field 'name'")),
        "expected the inner str field named, got: {errs:?}"
    );
}

#[test]
fn extern_c_variadic_call_typechecks() {
    entry_ok(
        "import std.ffi\nextern \"libc\":\n    fn printf(fmt: str, ...) -> int\n\
         printf(\"%ld %f %s %d %p\\n\", 1, 2.5, \"x\", true, ffi.null())\nprintf(\"plain\\n\")\n",
    );
}

#[test]
fn extern_c_variadic_rejects_non_scalar_varargs() {
    const HEAD: &str = "extern \"libc\":\n    fn printf(fmt: str, ...) -> int\n";
    for tail in [
        "struct P:\n    x: int\nprintf(\"x\", P(1))\n",
        "printf(\"x\", fn(n: int) -> int: n)\n",
        "s: str? = None\nprintf(\"x\", s)\n",
        "printf(\"x\", [1])\n",
    ] {
        let src = format!("{HEAD}{tail}");
        let errs = check_entry(&src);
        assert!(
            errs.iter().any(|e| e
                .message
                .contains("cannot be passed to a C variadic parameter")),
            "expected a vararg reject for {tail:?}, got: {errs:?}"
        );
    }
}

#[test]
fn extern_c_variadic_requires_the_fixed_args() {
    rejects_entry(
        "extern \"libc\":\n    fn printf(fmt: str, ...) -> int\nprintf()\n",
        "'printf' expects at least 1 argument(s), got 0",
    );
}

/// TICKET-218: a constant outside a C width's range is a compile error where the width is expected.
#[test]
fn ffi_width_constant_out_of_range_rejected() {
    rejects_entry(
        "import int8 from std.ffi\nstruct S:\n    q: int8\ns := S(1000)\nprint(s.q)\n",
        "does not fit int8 (-128..127)",
    );
    // A local annotation is a rule-1 error (TICKET-262), whatever the constant.
    rejects_entry(
        "import int8 from std.ffi\nx: int8 = 1000\nprint(x)\n",
        "describes the C boundary only",
    );
}

#[test]
fn type_arg_bracket_reads_with_the_type_grammar() {
    ok(
        "fn idt[T](x: T) -> T:\n    return x\nfn main():\n    g := idt[(int, str)]\n    print(g((1, \"a\")))\n",
    );
}

#[test]
fn generic_fn_value_pinned_by_sibling_at_coalesce_join() {
    ok(
        "fn inc(x: int) -> int:\n    return x + 1\nfn g[T](x: T) -> T:\n    return x\nfn main():\n    o: (fn(int) -> int)? = ?inc\n    print((o ?? g)(5))\n",
    );
}

/// TICKET-225 (R5, CK5) — a generic fn value read with no pin takes type variables, and a sibling at
/// a join pins them. Join x order (generic first / last) x sibling {`inc`, `(o ?? inc)`, `h`}. A
/// concrete sibling pins `g`; another unpinned generic `h` pins nothing, so its cells use the result
/// without a call and must still reject. Every set cell rejects `Hashable` first (no fn is Hashable).
#[test]
fn generic_value_join_grid() {
    let prelude = "struct Bx[T]:\n    v: T\n    fn add(self, other: Bx[T]) -> Bx[T]:\n        return other\nfn g[T](x: T) -> T:\n    return x\nfn h[T](x: T) -> T:\n    return x\nfn inc(x: int) -> int:\n    return x + 1\nfn main():\n    o: (fn(int) -> int)? = ?inc\n    c := true\n    d := false\n    k := 1\n";
    // `{P}` is `(5)` in a pinning cell and empty in an `h` cell. `set` marks the Hashable cells.
    let joins: &[(&str, &str, bool)] = &[
        ("if", "f := if c: {A} else: {B}\n    print(f{P})\n", false),
        (
            "elif",
            "f := if c: {A} elif d: {B} else: {B}\n    print(f{P})\n",
            false,
        ),
        (
            "match",
            "f := match k:\n        1: {A}\n        _: {B}\n    print(f{P})\n",
            false,
        ),
        ("list", "f := [{A}, {B}]\n    print(f[0]{P})\n", false),
        (
            "map",
            "f := {\"a\": {A}, \"b\": {B}}\n    print(f[\"a\"]{P})\n",
            false,
        ),
        ("==", "print({A} == {B})\n", false),
        ("list +", "f := [{A}] + [{B}]\n    print(f[0]{P})\n", false),
        (
            "list +=",
            "xs := [{A}]\n    xs += [{B}]\n    print(xs[0]{P})\n",
            false,
        ),
        ("in", "print({A} in [{B}])\n", false),
        ("Bx +", "print(((Bx({A}) + Bx({B})).v){P})\n", false),
        ("Bx ==", "print(Bx({A}) == Bx({B}))\n", false),
        ("set", "f := {{A}, {B}}\n    print(f)\n", true),
        ("set -", "print({{A}} - {{B}})\n", true),
        ("set |", "print({{A}} | {{B}})\n", true),
        ("set -=", "s := {{A}}\n    s -= {{B}}\n    print(s)\n", true),
        ("set |=", "s := {{A}}\n    s |= {{B}}\n    print(s)\n", true),
    ];
    let mut red: Vec<String> = Vec::new();
    for (join, body, set) in joins {
        for sib in ["inc", "(o ?? inc)", "h"] {
            for generic_first in [true, false] {
                let (a, b) = if generic_first {
                    ("g", sib)
                } else {
                    (sib, "g")
                };
                let p = if sib == "h" { "" } else { "(5)" };
                let src = format!(
                    "{prelude}    {}",
                    body.replace("{A}", a).replace("{B}", b).replace("{P}", p)
                );
                let errs = check_src(&src);
                let want = if *set {
                    Some("Hashable")
                } else if sib == "h" || (*join == "list +=" && generic_first) {
                    // TICKET-238: `xs := [g]` is not pinned by the later `+=`.
                    Some("not determined here")
                } else {
                    None
                };
                let cell = format!("{join} / {sib} / generic_first={generic_first}");
                match want {
                    None if !errs.is_empty() => red.push(format!("{cell}: want ok, got {errs:?}")),
                    Some(n) if !errs.iter().any(|e| e.message.contains(n)) => {
                        red.push(format!("{cell}: want {n:?}, got {errs:?}"))
                    }
                    _ => {}
                }
            }
        }
    }
    // `??` takes the generic last only: its lhs is the `Option`.
    let errs = check_src(&format!("{prelude}    print((o ?? g)(5))\n"));
    if !errs.is_empty() {
        red.push(format!("??: want ok, got {errs:?}"));
    }
    assert!(
        red.is_empty(),
        "{} red cells:\n{}",
        red.len(),
        red.join("\n")
    );
}

/// TICKET-225 (R5) — a let-bound generic fn value is pinned by any later use in its fn body, and is
/// rejected at the read's span only if its frame (one fn body, or one top-level statement) closes
/// with it unpinned.
#[test]
fn generic_value_later_use_grid() {
    let prelude = "fn g[T](x: T) -> T:\n    return x\nfn inc(x: int) -> int:\n    return x + 1\nfn take(p: fn(int) -> int) -> int:\n    return p(1)\n";
    // TICKET-238: a later use does not pin `f := g`. The read is typed where it is bound.
    let oks: &[&str] = &[
        "fn main():\n    f := g[int]\n    print(f(5))\n",
        "fn main():\n    p: fn(int) -> int = g\n    print(p(1))\n",
        "fn main():\n    print(take(g))\n",
    ];
    let mut red: Vec<String> = Vec::new();
    for body in oks {
        let errs = check_entry(&format!("{prelude}{body}"));
        if !errs.is_empty() {
            red.push(format!("{body}\n  want ok, got {errs:?}"));
        }
    }
    // (body, line of the read `f := ...` counted from the body's first line, 1-based)
    let rejects_at: &[(&str, u32)] = &[
        ("fn main():\n    f := g\n", 2),
        ("fn main():\n    f := g\n    print(f(5))\n", 2),
        ("fn main():\n    f := g\n    print(take(f))\n", 2),
        (
            "fn main():\n    f := g\n    p: fn(int) -> int = f\n    print(p(1))\n",
            2,
        ),
        ("fn main():\n    f := g\n    print(f == inc)\n", 2),
        (
            "fn mk2() -> fn(int) -> int:\n    f := g\n    return f\nfn main():\n    print(mk2()(5))\n",
            2,
        ),
        ("fn main():\n    f := g\n    print(f)\n", 2),
        (
            "fn mk[T](n: int) -> List[T]:\n    return []\nfn main():\n    f := mk\n    print(f(1))\n",
            4,
        ),
        ("f := g\nprint(f(5))\n", 1),
    ];
    let base = prelude.lines().count() as u32;
    for (body, line) in rejects_at {
        let errs = check_src(&format!("{prelude}{body}"));
        if !errs
            .iter()
            .any(|e| e.message.contains("not determined here") && e.span.line == base + line)
        {
            red.push(format!(
                "{body}\n  want 'not determined here' at line {}, got {errs:?}",
                base + line
            ));
        }
    }
    // A bounded generic read is rejected at its binding like any other.
    let lt = "struct P:\n    x: int\nfn lt[T: Comparable](a: T, b: T) -> bool:\n    return a < b\n";
    let errs = check_src(&format!(
        "{lt}fn main():\n    f := lt\n    print(f(P(1), P(2)))\n"
    ));
    if !errs
        .iter()
        .any(|e| e.message.contains("not determined here"))
    {
        red.push(format!("bound: want 'not determined here', got {errs:?}"));
    }
    assert!(
        red.is_empty(),
        "{} red cells:\n{}",
        red.len(),
        red.join("\n")
    );
}

/// TICKET-225 (R5, W2/FF1) — an untyped constant (a literal or a folded constant expression) meets
/// the slot it lands in, and is rejected iff its value lies outside the slot's width (Go:
/// `cannot use 1 << 8 (untyped int constant 256) as int8 value ... (overflows)`).
#[test]
fn untyped_constant_width_grid() {
    use crate::native::cffi::{CType, width_ctype};
    // (template, int-only). `{W}` is the width, `{K}` the constant.
    let slots: &[(&str, bool)] = &[
        ("x: {W} = {K}\nprint(x)\n", false),
        ("x: {W} = 0\nx = {K}\nprint(x)\n", false),
        ("fn take(x: {W}) -> None:\n    pass\ntake({K})\n", false),
        ("fn r() -> {W}:\n    return {K}\nprint(r())\n", false),
        ("struct S:\n    v: {W}\ns := S({K})\nprint(s)\n", false),
        (
            "struct Bx[T]:\n    v: T\nb: Bx[{W}] = Bx({K})\nprint(b)\n",
            false,
        ),
        (
            "enum E[T]:\n    A(T)\ne: E[{W}] = E.A({K})\nprint(e)\n",
            false,
        ),
        (
            "fn id[T](x: T) -> T:\n    return x\ny: {W} = id({K})\nprint(y)\n",
            false,
        ),
        ("m: Map[{W}, str] = {}\nm[{K}] = \"a\"\nprint(m)\n", true),
        ("m: Map[{W}, str] = {}\nprint(m[{K}])\n", true),
        ("l: List[{W}] = [{K}]\nprint(l)\n", false),
        ("o: {W}? = ?({K})\nprint(o)\n", false),
        ("c := true\nx: {W} = if c: {K} else: 0\nprint(x)\n", false),
    ];
    // (constant text, its folded value)
    let int_ks: &[(&str, i64)] = &[
        ("300", 300),
        ("1 << 8", 256),
        ("127 | (127 + 1)", 255),
        ("0 - 1", -1),
    ];
    let float_ks: &[(&str, f64)] = &[
        ("3e38 + 3e38", 6e38),
        ("-3e38 * 2.0", -6e38),
        ("1.5 + 1.5", 3.0),
    ];
    let mut red: Vec<String> = Vec::new();
    // TICKET-262: the struct field is the one slot here that is a C position, so only it meets its
    // constant; every other slot rejects the width type itself, whatever the constant.
    let rule1 = "describes the C boundary only";
    let at_c = |slot: &str| slot.starts_with("struct S:\n    v: {W}");
    let mut run = |w: &str, slot: &str, k: &str, want: Option<String>| {
        let want = if at_c(slot) {
            want
        } else {
            Some(rule1.to_string())
        };
        let body = slot.replace("{W}", w).replace("{K}", k);
        let src = format!("import std.ffi\nimport {w} from std.ffi\n{body}");
        let errs = check_entry(&src);
        match want {
            None if !errs.is_empty() => red.push(format!("{src}\n  want ok, got {errs:?}")),
            Some(n) if !errs.iter().any(|e| e.message.contains(n.as_str())) => {
                red.push(format!("{src}\n  want {n:?}, got {errs:?}"))
            }
            _ => {}
        }
    };
    for w in [
        "int8", "uint8", "int16", "uint16", "int32", "uint32", "uint64",
    ] {
        let ct = width_ctype(w).unwrap();
        for (slot, _) in slots {
            for (k, v) in int_ks {
                let want = (!ct.fits_int(*v, true))
                    .then(|| format!("constant {v} does not fit {w} {}", ct.range_text()));
                run(w, slot, k, want);
            }
        }
    }
    let ct = CType::Float32;
    for (slot, int_only) in slots {
        if *int_only {
            continue;
        }
        let slot = slot
            .replace("= 0\n", "= 0.0\n")
            .replace("else: 0\n", "else: 0.0\n");
        for (k, v) in float_ks {
            let want = (!ct.fits_f64(*v))
                .then(|| format!("constant {v:e} does not fit float32 {}", ct.range_text()));
            run("float32", &slot, k, want);
        }
    }
    // Neighbours: a non-constant int into a width slot is no constant error (the width type itself is
    // the rule-1 error, TICKET-262).
    for body in [
        "x: int8 = [300].len()\nprint(x)\n",
        "x: int8 = str(300).len()\nprint(x)\n",
        "fn n(a: int) -> int8:\n    return 0\nx: int8 = n(300)\nprint(x)\n",
        // An index subscript is an `int`, not the slot's width (Go: `var x int8 = xs[200]`).
        "xs: List[int8] = [1]\nx: int8 = xs[200]\nprint(x)\n",
        "xs: List[int8] = [1]\nfn take(v: int8) -> None:\n    pass\ntake(xs[200])\n",
        "xs: List[int8] = [1]\nfn r() -> int8:\n    return xs[200]\nprint(r())\n",
        "xs: List[int8] = [1]\nx: int8 = xs[200] + 1\nprint(x)\n",
        "xs: List[int8] = [1]\nx: int8 = xs[0..200][0]\nprint(x)\n",
        "s := \"abc\"\nn: int8 = s[200].len()\nprint(n)\n",
    ] {
        run("int8", body, "", None);
    }
    // Must still fail: a constant stored through an index, or assigned after an index read,
    // meets the element width.
    for body in [
        "xs: List[int8] = [1]\nxs[0] = 300\nprint(xs)\n",
        "xs: List[int8] = [1]\nx: int8 = xs[0]\nx = 300\nprint(x)\n",
    ] {
        let want = "constant 300 does not fit int8 (-128..127)".to_string();
        run("int8", body, "", Some(want));
    }
    assert!(
        red.is_empty(),
        "{} red cells:\n{}",
        red.len(),
        red.join("\n")
    );
}

#[test]
fn prefix_bang_builds_an_error_value() {
    ok("fn f() -> int!str:\n    return !\"disk\"\nfn main():\n    pass\n");
}

#[test]
fn prefix_bang_operand_must_satisfy_error() {
    rejects(
        "fn f() -> int!:\n    return !5\n",
        "int does not satisfy Error",
    );
}

#[test]
fn prefix_bang_applies_to_the_whole_postfix_operand() {
    use crate::ast::{ExprKind, StmtKind, UnaryOp};
    let m = parser::parse(lexer::tokenize("x := !a.b(c)\n").expect("lex")).expect("parse");
    let StmtKind::Let { value, .. } = &m.stmts[0].kind else {
        panic!("expected a let: {:?}", m.stmts[0]);
    };
    let ExprKind::Unary {
        op: UnaryOp::ErrVal,
        expr,
    } = &value.kind
    else {
        panic!("expected a prefix '!': {value:?}");
    };
    assert!(
        matches!(expr.kind, ExprKind::Call { .. }),
        "operand must be the whole call: {expr:?}"
    );
}

#[test]
fn none_is_the_void_type_name() {
    ok("fn log(m: str) -> None:\n    print(m)\nlog(\"hi\")\n");
    rejects(
        "fn log(m: str) -> None:\n    print(m)\nx := log(\"hi\")\n",
        "cannot be used as a value",
    );
    ok(
        "fn save(p: str) -> None!str:\n    if p == \"\":\n        return !\"empty\"\n    print(p)\n",
    );
    rejects(
        "fn save() -> None!str:\n    return\nx := save()?\n",
        "cannot be used as a value",
    );
}

#[test]
fn multi_target_assignment_wraps_each_element() {
    ok("x: int? = None\ny := 0\nx, y = 5, 0\n");
    rejects(
        "fn g() -> (int, int):\n    return (5, 0)\nx: int? = None\ny := 0\nx, y = g()\n",
        "cannot assign int to",
    );
}

#[test]
fn wait_recv_assign_wraps_into_an_optional_target() {
    ok(
        "fn main():\n    ch := Channel[int](1)\n    ch.send(5)\n    x: int? = None\n    wait:\n        x = ch.recv():\n            print(x)\n",
    );
}

#[test]
fn question_prefix_without_expected_type_defaults_to_optional() {
    ok("fn main():\n    y := ?5\n    z: int? = y\n");
}

#[test]
fn question_prefix_builds_the_expected_carrier() {
    ok("fn main():\n    x: int? = ?5\n    y: int!str = ?5\n    z: int?? = ?None\n");
    rejects(
        "fn main():\n    x: int? = ?\"s\"\n",
        "'?' value: expected int, found str",
    );
}

#[test]
fn bang_prefix_without_a_pinning_use_is_rejected() {
    rejects(
        "fn main():\n    w := !\"disk\"\n    print(w)\n",
        "a `!` value needs its type from an annotation",
    );
    rejects(
        "w := !\"disk\"\nprint(w)\n",
        "a `!` value needs its type from an annotation",
    );
}

#[test]
fn carrier_variants_resolve_like_user_enums() {
    ok(
        "fn main():\n    x := ?5\n    y: int? = None\n    z := ?1\n    print(x)\n    print(y)\n    print(z)\n",
    );
}

#[test]
fn enum_ty_and_as_enum_are_inverse_on_every_enum_shape() {
    let shapes = [
        ("Option", vec![Ty::Int], Ty::option(Ty::Int)),
        (
            "Result",
            vec![Ty::Int, Ty::Str],
            Ty::result_e(Ty::Int, Ty::Str),
        ),
        (
            "Opt1",
            vec![Ty::Int],
            Ty::Enum("Opt1".to_string(), vec![Ty::Int]),
        ),
        ("Color", vec![], Ty::Enum("Color".to_string(), vec![])),
    ];
    for (key, args, want) in shapes {
        let built = Ty::enum_ty(key.to_string(), args.clone());
        assert_eq!(built, want, "enum_ty({key})");
        let (k, a) = built.as_enum().expect("an enum shape");
        assert_eq!((k, a), (key, args), "as_enum({key})");
    }
    assert!(Ty::Int.as_enum().is_none());
    assert!(Ty::list(Ty::Int).as_enum().is_none());
}
