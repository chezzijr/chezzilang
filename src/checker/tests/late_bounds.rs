//! Checker tests: std module sources, Shared read, optional bounds, late conformance, Atomic Eq,
//! range literals.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

/// Every `.chz` file under the std root except the prelude, as `(dotted module, source)`.
fn std_module_sources() -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, dotted: &str, out: &mut Vec<(String, String)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for p in entries {
            let stem = p.file_stem().unwrap().to_string_lossy().to_string();
            if p.is_dir() {
                walk(&p, &format!("{dotted}.{stem}"), out);
            } else if p.extension().is_some_and(|x| x == "chz") && stem != "prelude" {
                out.push((
                    format!("{dotted}.{stem}"),
                    std::fs::read_to_string(&p).unwrap(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(&crate::resolver::std_root(), "std", &mut out);
    out
}

/// The identifier a declaration line starts with, after its keyword prefix.
fn leading_ident(rest: &str) -> String {
    rest.chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

/// TICKET-243: a `_` name a std module declares is internal to std. The grid is every
/// `RWSHARED_VIEWS` primitive, every top-level `_` name of every std file, and every `_` method of
/// every std struct or enum, each rejected in a user module; the control cells stay accepted.
#[test]
fn std_internal_names_are_hidden_from_user_modules() {
    // (a) every container primitive: a user module rejects it and names the collection; a std
    // caller exists.
    let callers = [
        include_str!("../../../std/concurrency/collection.chz"),
        include_str!("../../../std/memoize.chz"),
    ];
    for (head, coll, names) in super::expr::RWSHARED_VIEWS {
        let init = match *head {
            "List" => "[1, 2]",
            "Map" => "{\"a\": 1}",
            "Set" => "{1, 2}",
            other => panic!("no initializer for head {other}"),
        };
        let help = format!("use {coll} from std.concurrency.collection");
        for name in *names {
            let src = format!(
                "import RwShared from std.concurrency\nfn main():\n    r := RwShared({init})\n    r.{name}()\nmain()\n"
            );
            let errs = check_entry(&src);
            assert!(
                errs.iter().any(|e| e.message.contains("has no method")
                    && e.help.as_deref() == Some(help.as_str())),
                "{head} r.{name}(): expected `has no method` with help {help:?}, got: {errs:?}"
            );
            let call = format!(".{name}(");
            assert!(
                callers.iter().any(|c| c.contains(&call)),
                "RWSHARED_VIEWS lists {name} for {head} but no std caller writes `{call}`"
            );
        }
    }

    // (b) every top-level `_` name and every `_` method of every std file.
    let mut members: Vec<(String, String)> = Vec::new();
    let mut methods = std::collections::BTreeSet::new();
    for (module, src) in std_module_sources() {
        let mut owner = String::new();
        for line in src.lines() {
            let body = line.trim_start();
            let indented = body.len() != line.len();
            if !indented {
                for kw in ["native struct ", "struct ", "enum "] {
                    if let Some(rest) = line.strip_prefix(kw) {
                        owner = leading_ident(rest);
                    }
                }
                for kw in [
                    "native fn _",
                    "fn _",
                    "struct _",
                    "enum _",
                    "protocol _",
                    "type _",
                ] {
                    if line.starts_with(kw) {
                        let name = leading_ident(&line[kw.len() - 1..]);
                        members.push((module.clone(), name));
                    }
                }
            } else {
                for kw in ["native fn _", "fn _"] {
                    if body.starts_with(kw) {
                        let name = leading_ident(&body[kw.len() - 1..]);
                        methods.insert((module.clone(), owner.clone(), name));
                    }
                }
            }
        }
    }
    for want in [
        ("std.concurrency", "_settle"),
        ("std.fs", "_exists"),
        ("std.concurrency.pmap", "_PmapCell"),
    ] {
        assert!(
            members
                .iter()
                .any(|(m, n)| (m.as_str(), n.as_str()) == want),
            "the std walk did not collect {want:?}"
        );
    }
    for (module, name) in &members {
        let errs = check_entry(&format!("import {name} from {module}\n"));
        let miss = format!("has no member '{name}'");
        assert!(
            errs.iter().any(|e| e.message.contains(&miss)),
            "import {name} from {module}: expected {miss:?}, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|e| e.message.contains("import it from")
                || e.help
                    .as_deref()
                    .is_some_and(|h| h.contains("import it from"))),
            "import {name} from {module}: the hint names a sealed type: {errs:?}"
        );
    }
    entry_rejects(
        "import std.concurrency\nconcurrency._settle(5, 1)\n",
        "module 'concurrency' has no member '_settle'",
    );
    let want_methods: std::collections::BTreeSet<(String, String, String)> = [
        ("std.cancel", "Token", "_mark"),
        ("std.concurrency", "Executor", "_submit_settled"),
    ]
    .iter()
    .map(|(m, o, n)| (m.to_string(), o.to_string(), n.to_string()))
    .collect();
    assert_eq!(
        methods, want_methods,
        "a std `_` method needs a call cell below before it joins this set"
    );
    entry_rejects(
        "import std.cancel\nt := cancel.manual()\nt._mark()\n",
        "type Token has no method '_mark'",
    );
    entry_ok("import std.cancel\nt := cancel.manual()\nt.cancel()\nassert t.cancelled()\n");
    entry_ok(
        "struct S:\n    n: int\n    fn _helper(self) -> int:\n        return self.n\nprint(S(1)._helper())\n",
    );
    entry_ok("import pmap from std.concurrency.pmap\nprint(pmap([1], fn(x: int) -> int: x))\n");
    // The seal is for std only: a USER module's `_` member and `_` type stay importable.
    let errs = check_files(&[
        (
            "lib.chz",
            "fn _secret() -> int:\n    return 7\nstruct _Hid:\n    n: int\n",
        ),
        (
            "main.chz",
            "import _secret, _Hid from lib\nimport lib\nprint(_secret(), lib._secret(), _Hid(2).n)\n",
        ),
    ]);
    assert!(errs.is_empty(), "a user module's `_` names: {errs:?}");
    // A native `_` method is no field either, and witnesses no user protocol.
    entry_rejects(
        "import RwShared from std.concurrency\nr := RwShared([1])\nf := r._len\n",
        "has no field '_len'",
    );
    entry_rejects(
        "import RwShared from std.concurrency\nprotocol P:\n    fn _len(self) -> int\nfn f[T: P](x: T) -> int:\n    return x._len()\nprint(f(RwShared([1])))\n",
        "does not satisfy P",
    );

    // (c) the native `_` method, and the public surface beside every sealed name.
    let errs = check_entry(
        "import Executor from std.concurrency\nex := Executor()\nch: Channel[int] = Channel(1)\nex._submit_settled(fn(): pass, ch)\n",
    );
    assert!(
        errs.iter()
            .any(|e| e.message.contains("has no method '_submit_settled'")),
        "{errs:?}"
    );
    assert!(
        !errs.iter().any(|e| e
            .help
            .as_deref()
            .is_some_and(|h| h.contains("_submit_settled"))),
        "the help names the hidden method: {errs:?}"
    );
    entry_ok(
        "import std.fs\nimport std.concurrency\nimport ConcurrentList from std.concurrency.collection\nprint(fs.exists(\"x\"))\nh := concurrency.Executor().submit_result(fn(): 4)\nprint(h)\nConcurrentList[int].new().push(1)\n",
    );
}

/// TICKET-246: `Shared[T]` gains `read(f)` (the closure runs inside the box heap, result copied out).
#[test]
fn shared_has_a_read_method() {
    entry_ok(
        "import Shared from std.concurrency\nfn main():\n    s := Shared([1, 2, 3])\n    n := s.read(fn(l: List[int]) -> int: l.len())\n    print(n)\nmain()\n",
    );
}

/// TICKET-246 (Airlock, fact A4) — every row of `CROSSING_CLOSURE_METHODS` gets the capture floor:
/// its closure runs in another heap, so a DIRECT write to a captured local is the D4 error at
/// check time. Grid: row x {direct write in a literal closure, direct write in a closure value,
/// write through a callee, read only, a closure that mutates only its parameter}.
///
/// The callee column is `ok` for every row: the floor reports a direct write only (measured,
/// Digest section 10); that shape is the run-time D4 fault, a cell of
/// `tests/chz/suites/box_grid_test.chz`. `RwShared.read` has no row (its closure runs in the
/// reader's heap), so a captured write there stays `ok`.
#[test]
fn every_crossing_closure_method_reports_a_captured_write() {
    // (owner, method, receiver setup, a call with `{F}` for the closure, the closure's parameter).
    let user_rows = [
        (
            "Executor",
            "submit",
            "r := Executor(1)",
            "r.submit({F})",
            "",
        ),
        (
            "Shared",
            "update",
            "r := Shared(1)",
            "r.update({F})",
            "x: int",
        ),
        (
            "Shared",
            "read",
            "r := Shared(1)",
            "_ := r.read({F})",
            "x: int",
        ),
        (
            "RwShared",
            "write",
            "r := RwShared(1)",
            "r.write({F})",
            "x: int",
        ),
    ];
    for (owner, method, ..) in user_rows {
        assert!(
            crate::base::crossing::CROSSING_CLOSURE_METHODS.contains(&(owner, method)),
            "{owner}.{method} has no row"
        );
    }
    // `_submit_settled` is std-internal: its row exists and its one caller checks clean.
    assert!(
        crate::base::crossing::CROSSING_CLOSURE_METHODS.contains(&("Executor", "_submit_settled"))
    );
    assert_eq!(
        crate::base::crossing::CROSSING_CLOSURE_METHODS.len(),
        user_rows.len() + 1
    );
    entry_ok(
        "import Executor from std.concurrency\nfn main():\n    ex := Executor(1)\n    ch := ex.submit_result(fn() -> int: 1)\n    print(ch.recv())\n    ex.shutdown()\nmain()\n",
    );
    for (owner, method, setup, call, param) in user_rows {
        let ret = if param.is_empty() { "None" } else { "int" };
        let tail = if param.is_empty() { "" } else { "x + " };
        let program = |pre: &str, f: &str| {
            format!(
                "import Atomic, AtomicInt, Executor, RwShared, Shared from std.concurrency\nfn keep(xs: List[int]) -> int:\n    xs.push(1)\n    return 1\nfn main():\n    seen: List[int] = []\n    {setup}\n{pre}    {}\n    print(seen)\nmain()\n",
                call.replace("{F}", f)
            )
        };
        let direct = format!("fn({param}): {tail}seen.remove_at(0)");
        let value = format!("    f := fn({param}) -> {ret}: {tail}seen.remove_at(0)\n");
        let value = if param.is_empty() {
            "    f := fn(): seen.push(1)\n".to_string()
        } else {
            value
        };
        let direct = if param.is_empty() {
            "fn(): seen.push(1)".to_string()
        } else {
            direct
        };
        let cell = |shape: &str, src: String, rejects: bool| {
            let errs = check_entry(&src);
            let hit = errs
                .iter()
                .any(|e| e.message.contains("is this task's copy"));
            if rejects {
                assert!(
                    hit,
                    "{owner}.{method} x {shape}: want the D4 error, got {errs:?}"
                );
            } else {
                assert!(
                    errs.is_empty(),
                    "{owner}.{method} x {shape}: want ok, got {errs:?}"
                );
            }
        };
        cell("literal direct write", program("", &direct), true);
        cell("closure value direct write", program(&value, "f"), true);
        let callee = if param.is_empty() {
            "fn(): keep(seen)".to_string()
        } else {
            format!("fn({param}): {tail}keep(seen)")
        };
        cell("write through a callee", program("", &callee), false);
        let read = if param.is_empty() {
            "fn(): print(seen.len())".to_string()
        } else {
            format!("fn({param}): {tail}seen.len()")
        };
        cell("read only", program("", &read), false);
    }
    // A closure that mutates only its PARAMETER is the point of the box: `ok`.
    entry_ok(
        "import RwShared, Shared from std.concurrency\nfn bump(xs: List[int]) -> List[int]:\n    xs.push(1)\n    return xs\nfn main():\n    s := Shared([1])\n    s.update(fn(xs): bump(xs))\n    w := RwShared([1])\n    w.write(fn(xs): bump(xs))\n    print(s.read(fn(xs): xs.len()), w.get())\nmain()\n",
    );
    // No row: `RwShared.read` runs its closure in the reader's heap, so a captured write is legal.
    entry_ok(
        "import RwShared from std.concurrency\nfn main():\n    seen: List[int] = []\n    w := RwShared(1)\n    n := w.read(fn(x: int) -> int: x + seen.remove_at(0))\n    print(n, seen)\nmain()\n",
    );
}

#[test]
fn inline_optional_never_satisfies_a_bound_through_its_payload() {
    let num = "import Num from std.ops\nfn double[T: Num](x: T) -> T:\n    return x + x\n";
    let loc = "protocol Located:\n    fn line(self) -> int\nstruct Tok:\n    n: int\n    fn line(self) -> int:\n        return self.n\nfn where_is[T: Located](x: T) -> int:\n    return x.line()\n";
    let cells: Vec<(&str, String)> = vec![
        ("num", format!("{num}print(double(?5))\n")),
        ("user-protocol", format!("{loc}n: int = where_is(?Tok(7))\n")),
        (
            "comparable",
            "import Comparable from std.cmp\nfn mx[T: Comparable](a: T, b: T) -> T:\n    return if a < b: b else: a\nstruct NC:\n    n: int\nprint(mx(?NC(1), ?NC(2)))\n".to_string(),
        ),
    ];
    let mut missed = Vec::new();
    for (name, src) in &cells {
        if !check_src(src)
            .iter()
            .any(|e| e.message.contains("does not satisfy"))
        {
            missed.push(*name);
        }
    }
    assert!(
        missed.is_empty(),
        "inline optional passed a bound unchecked in cells: {missed:?}"
    );
}

/// TICKET-250: a static method with a hook's name is not the hook, so the operator must reject it
/// exactly as the matching protocol bound does.
#[test]
fn static_method_is_not_a_hook_for_the_operator() {
    let cells = [
        (
            "index",
            r"fn index(a: int, k: int) -> int:
        return k",
            "print(c[2])",
        ),
        (
            "iter",
            r"fn iter(a: int) -> Iterator[int]:
        return [1].iter()",
            r"for x in c:
    print(x)",
        ),
        (
            "contains",
            r"fn contains(a: int, k: int) -> bool:
        return true",
            "print(2 in c)",
        ),
        (
            "set_index",
            r"fn set_index(a: int, k: int, v: int):
        return",
            "c[1] = 2",
        ),
    ];
    let mut accepted = Vec::new();
    for (hook, method, use_site) in cells {
        let src = format!(
            "import Iterator from std.iter\nstruct C:\n    n: int\n    {method}\nc := C(1)\n{use_site}\n"
        );
        if check_src(&src).is_empty() {
            accepted.push(hook);
        }
    }
    assert!(
        accepted.is_empty(),
        "operator accepted a static hook: {accepted:?}"
    );
}

/// TICKET-250 — a receiver-first method with a hook's name that `hook_verdict` rejects for its
/// shape warns, and only where the operator still accepts the type and silently skips the method
/// (`==`, `print`, `for` through the other iteration hook). A static
/// method never warns. A type the operator rejects never warns: that use is already a check error
/// carrying the reason, and a `Set`-like `fn add(self, item)` must stay silent.
#[test]
fn a_near_miss_hook_warns_only_where_the_operator_silently_skips_it() {
    let p = |m: &str| format!("struct P:\n    v: int\n{m}x := P(1)\nprint(x.v)\n");
    let cases = [
        (
            "    fn eq(self, o: P) -> bool:\n        return true\n",
            "'o'",
        ),
        (
            "    fn eq[U](self, o: U) -> bool:\n        return true\n",
            "'eq' on P is not the Eq hook",
        ),
        (
            "    fn eq(self, other: P) -> int:\n        return 1\n",
            "'eq' on P is not the Eq hook",
        ),
        (
            "    fn str(self) -> List[str]:\n        return [\"x\"]\n",
            "'str' on P is not the Stringable hook",
        ),
        (
            "    fn str[T](self) -> str:\n        return \"x\"\n",
            "'str' on P is not the Stringable hook",
        ),
    ];
    for (m, needle) in cases {
        warns(&p(m), needle);
    }
    // `Hashable` never warns (TICKET-280): the operator rejects every struct without the hook.
    no_warn("struct Z:\n    fn hash(self) -> str:\n        return \"h\"\nz := Z()\nprint(z)\n");
    warns(
        "import Iterator from std.iter\nstruct C:\n    n: int\n    fn next(self) -> int?:\n        return None\n    fn iter(self, k: int) -> Iterator[int]:\n        return [k].iter()\nc := C(1)\nprint(c.n)\n",
        "'iter' on C is not the Iterable hook",
    );
    for m in [
        "    fn eq(a: P, b: P) -> bool:\n        return true\n",
        "    fn str(reading: int) -> str:\n        return \"x\"\n",
        "    fn add(self, item: int):\n        pass\n",
        "    fn index(self, a: int, b: int) -> int:\n        return a + b\n",
        "    fn hash(self) -> str:\n        return \"h\"\n",
        "    fn eq(self, other: P) -> bool:\n        return true\n",
        "    fn str(self) -> str:\n        return \"x\"\n",
        "    fn compare(self, other: P) -> int:\n        return 0\n",
    ] {
        no_warn(&p(m));
    }
}

/// TICKET-250 (D4) — `hoist` defers every conformance question a type spelling asks until all
/// types of the module are inserted, so a type may ask it of itself or of a type declared later.
/// The negative rows prove a deferred obligation still runs.
#[test]
fn a_type_may_ask_conformance_of_a_type_declared_later() {
    let hash = "    fn hash(self) -> int:\n        return self.id\n    fn eq(self, other: Self) -> bool:\n        return self.id == other.id\n";
    // A method returns a `Set` of its own type.
    let own_set = |h: &str| {
        format!(
            "struct Node:\n    id: int\n{h}    fn peers(self) -> Set[Node]:\n        return Set([self, self])\nprint(Node(1).peers().len())\n"
        )
    };
    // A field is a `Map` keyed by a struct declared later.
    let later_key = |h: &str| {
        format!(
            "struct Reg:\n    m: Map[R, int]\nstruct R:\n    id: int\n{h}r := Reg({{}})\nprint(r.m.len())\n"
        )
    };
    ok(&own_set(hash));
    ok(&later_key(hash));
    // A bounded generic applied to the type being declared.
    ok(
        "protocol Named:\n    fn name(self) -> str\nstruct Box[T: Named]:\n    v: T\nstruct A:\n    nxt: Box[A]?\n    fn name(self) -> str:\n        return \"a\"\na := A(None)\nprint(a.name())\n",
    );
    rejects(&own_set(""), "Set element type must implement Hashable");
    rejects(&later_key(""), "Map key type must implement Hashable");
    rejects(
        "protocol Named:\n    fn name(self) -> str\nstruct Box[T: Named]:\n    v: T\nstruct A:\n    nxt: Box[A]?\na := A(None)\nprint(a)\n",
        "does not satisfy Named",
    );
    // A bounded type parameter keeps its bound when the obligation runs late.
    ok(
        "import Eq, Hashable from std.cmp\nstruct Reg[K: Hashable + Eq]:\n    m: Map[K, int]\nr: Reg[int] = Reg({})\nprint(r.m.len())\n",
    );
}

/// TICKET-250 — an `Atomic` payload is rejected only when its `eq` is the `Eq` hook
/// (`Checker::hook_method`). A static `eq`, one that misnames its operand, and one that returns
/// `int` are ordinary methods: the payload compares structurally, so `cas` and `==` agree.
#[test]
fn an_atomic_payload_with_a_non_hook_eq_is_legal() {
    for owner in ["struct P:\n    v: int\n", "enum P:\n    A(int)\n"] {
        let mk = if owner.starts_with("struct") {
            "P(1)"
        } else {
            "P.A(1)"
        };
        for eq in [
            "    fn eq(a: int, b: int) -> bool:\n        return true\n",
            "    fn eq(self, o: P) -> bool:\n        return true\n",
            "    fn eq(self, other: P) -> int:\n        return 1\n",
        ] {
            entry_ok(&format!(
                "import Atomic from std.concurrency\n{owner}{eq}a := Atomic({mk})\nprint(a.load() == {mk})\n"
            ));
        }
        // The hook itself is still refused.
        entry_rejects(
            &format!(
                "import Atomic from std.concurrency\n{owner}    fn eq(self, other: P) -> bool:\n        return true\na := Atomic({mk})\nprint(a.load() == {mk})\n"
            ),
            "payload defines its own 'eq'",
        );
    }
}

/// TICKET-250 — `self` is the receiver only as the first parameter of a method declared in a
/// struct, enum or protocol body, un-annotated or annotated with the owner, with no default.
/// Every other parameter named `self` is a declaration error. A cell the parser rejects is
/// skipped: the grid asks only that no cell yields a typed non-receiver `self`.
#[test]
fn self_is_the_receiver_only_as_a_methods_first_parameter() {
    const ERR: &str = "'self' is only allowed as the first parameter of a method declared in a struct, enum or protocol body";
    // (kind, program with `{P}` for the parameter list, owner spelling, is a method owner)
    let kinds: [(&str, &str, &str, bool); 7] = [
        (
            "struct method",
            "struct S:\n    v: int\n    fn m({P}) -> int:\n        return 1\n",
            "S",
            true,
        ),
        (
            "enum method",
            "enum E:\n    A\n    fn m({P}) -> int:\n        return 1\n",
            "E",
            true,
        ),
        (
            "protocol method",
            "protocol Pr:\n    fn m({P}) -> int\n",
            "Self",
            true,
        ),
        ("free fn", "fn m({P}) -> int:\n    return 1\n", "int", false),
        (
            "nested fn in a method",
            "struct S:\n    v: int\n    fn outer(self) -> int:\n        fn m({P}) -> int:\n            return 1\n        return self.v\n",
            "S",
            false,
        ),
        (
            "closure",
            "f := fn({P}) -> int: 1\nprint(f)\n",
            "int",
            false,
        ),
        (
            "extern fn",
            "extern \"libc\":\n    fn m({P}) -> int\n",
            "int",
            false,
        ),
    ];
    let mut bad: Vec<String> = Vec::new();
    for (kind, tmpl, owner, is_method) in kinds {
        for pos in ["first", "second", "absent", "twice"] {
            for ann in ["none", "owner", "other"] {
                for dflt in [false, true] {
                    let other = if owner.starts_with("int") {
                        "str"
                    } else {
                        "int"
                    };
                    let slf = match (ann, dflt) {
                        ("none", false) => "self".to_string(),
                        ("none", true) => "self = 1".to_string(),
                        ("owner", false) => format!("self: {owner}"),
                        ("owner", true) => format!("self: {owner} = 1"),
                        (_, false) => format!("self: {other}"),
                        (_, true) => format!("self: {other} = 1"),
                    };
                    let a = "a: int";
                    let params = match pos {
                        "first" => format!("{slf}, {a}"),
                        "second" => format!("{a}, {slf}"),
                        "absent" => a.to_string(),
                        _ => format!("{slf}, {slf}"),
                    };
                    let src = tmpl.replace("{P}", &params);
                    let receiver = is_method && pos == "first" && ann != "other" && !dflt;
                    let clean = receiver || pos == "absent";
                    let cell = format!("{kind} / {pos} / {ann} / default={dflt}");
                    let parsed = lexer::tokenize(&src)
                        .map_err(|e| format!("{e:?}"))
                        .and_then(|t| parser::parse(t).map_err(|e| format!("{e:?}")));
                    let errs: Vec<String> = match parsed {
                        Ok(module) => match check(&module) {
                            Ok(()) => Vec::new(),
                            Err(e) => e.into_iter().map(|e| e.message).collect(),
                        },
                        Err(parse) if clean => vec![format!("parse: {parse}")],
                        Err(_) => continue,
                    };
                    if clean && !errs.is_empty() {
                        bad.push(format!("{cell}: expected clean, got {errs:?}"));
                    }
                    if !clean && !errs.iter().any(|m| m.contains(ERR)) {
                        bad.push(format!("{cell}: expected the self error, got {errs:?}"));
                    }
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} cell(s) wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
    // S2-8: a free fn's `self` was an untyped parameter that took any argument.
    rejects("fn cast(self) -> int: self\nprint(cast(2.5))\n", ERR);
}

/// TICKET-250 (D5) — `Atomic[T]` over a bare type parameter must carry the sealed bound
/// `StructuralEq`, which `std.concurrency` declares and an import licenses. Without the bound a
/// payload with the `eq` hook reached `Atomic` by substitution: `==` said `true`, `cas` said `false`.
#[test]
fn an_atomic_payload_parameter_must_carry_structural_eq() {
    const MK: &str = "(v: T) -> Atomic[T]:\n    return Atomic(v)\n";
    const HOOKED: &str =
        "struct P:\n    n: int\n    fn eq(self, other: P) -> bool:\n        return true\n";
    const UNKNOWN: &str = "unknown name 'StructuralEq' in bound on 'T'; import it from std.concurrency (`import StructuralEq from std.concurrency`)";
    // The unbounded parameter is refused, in a signature and in a struct field.
    entry_rejects(
        &format!("import Atomic from std.concurrency\nfn mk[T]{MK}"),
        "Atomic[T] payload 'T' must carry the bound StructuralEq (import it from std.concurrency)",
    );
    entry_rejects(
        "import Atomic from std.concurrency\nstruct H[T]:\n    a: Atomic[T]\n",
        "payload 'T' must carry the bound StructuralEq",
    );
    // A parameter nested in the payload is the same hole.
    entry_rejects(
        "import Atomic from std.concurrency\nfn f[T](a: Atomic[List[T]]) -> int:\n    return 1\n",
        "payload 'T' must carry the bound StructuralEq",
    );
    // Another bound does not stand in for it.
    entry_rejects(
        &format!(
            "import Hashable from std.cmp\nimport Atomic from std.concurrency\nfn mk[T: Hashable]{MK}"
        ),
        "must carry the bound StructuralEq",
    );
    // The bound checks under both import forms and as a `where` clause.
    entry_ok(&format!(
        "import Atomic, StructuralEq from std.concurrency\nfn mk[T: StructuralEq]{MK}print(mk(3).load())\n"
    ));
    entry_ok(&format!(
        "import Atomic, StructuralEq from std.concurrency\nfn mk[T: StructuralEq]{MK}print(mk([1]).load())\n"
    ));
    entry_ok(
        "import Atomic, StructuralEq from std.concurrency\nstruct H[T: StructuralEq]:\n    a: Atomic[T]\nh := H(Atomic(1))\n",
    );
    // `Atomic` alone does not license the bound's name; neither does no import at all.
    entry_rejects(
        &format!("import Atomic from std.concurrency\nfn mk[T: StructuralEq]{MK}"),
        UNKNOWN,
    );
    rejects("fn f[T: StructuralEq](v: T) -> T:\n    return v\n", UNKNOWN);
    // A module's own `protocol StructuralEq` wins over the imported name.
    const OWN: &str = "import std.concurrency\nprotocol StructuralEq:\n    fn tag(self) -> int\nstruct Q:\n    fn tag(self) -> int:\n        return 7\nfn f[T: StructuralEq](v: T) -> int:\n    return v.tag()\n";
    entry_ok(&format!("{OWN}print(f(Q()))\n"));
    entry_rejects(
        &format!("{OWN}print(f(3))\n"),
        "does not satisfy StructuralEq",
    );
    // The bound is sealed: a type that reaches the `eq` hook does not satisfy it.
    for call in ["mk(P(1))", "mk([P(1)])", "mk((1, ?P(1)))"] {
        entry_rejects(
            &format!(
                "import Atomic, StructuralEq from std.concurrency\n{HOOKED}fn mk[T: StructuralEq]{MK}x := {call}\n"
            ),
            "does not satisfy StructuralEq",
        );
    }
    // A method named `eq` that is not the hook leaves the type structural: a legal payload, direct
    // and through the bound, for a struct and for an enum.
    for eq in [
        "fn eq(a: P, b: P) -> bool:\n        return true",
        "fn eq(self, o: P) -> bool:\n        return true",
        "fn eq(self, other: P) -> int:\n        return 1",
    ] {
        for (decl, val) in [
            ("struct P:\n    n: int", "P(1)"),
            ("enum P:\n    A(int)", "P.A(1)"),
        ] {
            entry_ok(&format!(
                "import Atomic, StructuralEq from std.concurrency\n{decl}\n    {eq}\nfn mk[T: StructuralEq]{MK}a := Atomic({val})\nb := mk({val})\n"
            ));
        }
    }
    // Must still fail: the direct spelling of a hooked payload.
    entry_rejects(
        &format!("import Atomic from std.concurrency\n{HOOKED}a := Atomic(P(1))\n"),
        "payload defines its own 'eq'",
    );
}

#[test]
fn range_is_a_value_bound_to_a_name_and_passed_as_iterable() {
    ok(
        "import Iterable from std.iter\nfn total(r: Iterable[int]) -> int:\n    return 0\nr := 0..3\nx := total(0..3)\n",
    );
}

#[test]
fn range_struct_is_in_scope_with_no_import() {
    ok("r := Range(0, 3)\nfn f(q: Range) -> int:\n    return q.step\n");
}

/// Each index is typed once: `xs[xs[xs[..i..]]]` costs one `index_value` entry per subscript, so
/// 24 levels cost at most twice 12 levels plus a constant. Typed twice per level it is 2^depth.
/// Counted, not timed (`no_new_rust_test_reads_a_wall_clock`).
#[test]
fn nested_subscripts_type_each_index_once() {
    let entries = |depth: usize| {
        let expr = format!("{}i{}", "xs[".repeat(depth), "]".repeat(depth));
        let src = format!("xs := [0]\ni := 0\nprint({expr})\n");
        let tokens = lexer::tokenize(&src).expect("lex should succeed");
        let module = parser::parse(tokens).expect("parse should succeed");
        let (res, entries) = check_counting_index_values(&module);
        assert!(res.is_ok(), "{depth} nested subscripts: {res:?}");
        entries
    };
    let (shallow, deep) = (entries(12), entries(24));
    assert!(
        deep <= 2 * shallow + 4,
        "12 nested subscripts made {shallow} index_value entries, 24 made {deep}"
    );
}

#[test]
fn a_literal_range_with_no_start_cannot_be_iterated() {
    rejects(
        "for i in ..5:\n    print(i)\n",
        "a range with no start cannot be iterated",
    );
    rejects(
        "xs := [i for i in ..5]\n",
        "a range with no start cannot be iterated",
    );
    // A value reaches the same rule at run time, not at check time.
    ok("r := ..5\nxs := 3 in r\n");
    // A literal with a start iterates, however open its end.
    ok("for i in 2..:\n    break\n");
}

#[test]
fn len_of_a_literal_range_with_a_missing_bound_is_a_check_error() {
    rejects(
        "n := (5..).len()\n",
        "len() needs a range with a start and an end",
    );
    rejects(
        "n := (..5).len()\n",
        "len() needs a range with a start and an end",
    );
    ok("n := (2..5).len()\n");
}

#[test]
fn a_list_built_from_an_endless_literal_range_type_checks() {
    // No collector tests for a missing end (owner answer A): `List(5..)` runs until memory ends, as
    // `List(g())` does for an endless generator.
    ok("xs := List(5..)\n");
}

#[test]
fn range_slice_and_range_index_on_one_type_is_a_declaration_error() {
    // TICKET-254 part A: a type takes a range as a slice or as a key, not both.
    rejects(
        "struct Both:\n    data: List[int]\n    fn slice(self, r: Range) -> List[int]:\n        return self.data[r]\n    fn index(self, k: Range) -> str:\n        return \"i\"\nb := Both([1, 2, 3, 4])\nprint(b[1..3])\n",
        "a type takes a range as a slice or as a key, not both",
    );
}

#[test]
fn mutating_call_on_a_builtin_slice_temporary_warns() {
    // TICKET-254 part B.1: `g[0..2]` builds a fresh list, so the push changes a dropped copy.
    warns(
        "g := [[1], [2], [3]]\ng[0..2].push([9])\nprint(g)\n",
        "changes a temporary copy",
    );
}
