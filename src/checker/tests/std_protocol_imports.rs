//! TICKET-265: the std protocols live in their std modules. A bound `[T: Add]` needs
//! `import Add from std.ops`; `Any` and `Error` stay global; an operator needs no import.

use super::*;
use crate::ast::StmtKind;

/// `(name, owning module, type-parameter count)` of every std protocol, read from the `protocol`
/// declarations in `std/*.chz` (the one source), never from a list kept here.
fn std_protocols() -> Vec<(String, String, usize)> {
    graph_pass::std_protocol_decls()
        .iter()
        .map(|p| {
            let StmtKind::Protocol { type_params, .. } = &p.stmt.kind else {
                panic!("std protocol '{}' is not a protocol statement", p.name);
            };
            (p.name.clone(), p.module.clone(), type_params.len())
        })
        .collect()
}

/// `P` for a plain protocol, `P[int, int]` for one with two type parameters.
fn spelled(name: &str, arity: usize) -> String {
    if arity == 0 {
        name.to_string()
    } else {
        format!("{name}[{}]", vec!["int"; arity].join(", "))
    }
}

/// The five positions the import gate changes, as `(label, program naming the protocol as X)`: a
/// bound, a where clause, an annotation, an alias body and an embed. `user_protocol_key` serves all.
fn positions(x: &str) -> [(&'static str, String); 5] {
    [
        ("p1 bound", format!("fn f[T: {x}](v: T):\n    pass\n")),
        (
            "p2 where clause",
            format!("fn f[T](v: T) -> int where T: {x}:\n    return 0\n"),
        ),
        (
            "p3 annotation",
            format!("fn g(v: {x}) -> int:\n    return 0\n"),
        ),
        (
            "p4 alias body",
            format!("type Al = {x}\nfn g(v: Al) -> int:\n    return 0\n"),
        ),
        (
            "p5 embed",
            format!("protocol P:\n    {x}\n    fn m(self) -> int\n"),
        ),
    ]
}

/// The messages of `src` that say a name is unknown.
fn unknown_errors(src: &str) -> Vec<String> {
    check_src(src)
        .into_iter()
        .map(|e| e.message)
        .filter(|m| m.contains("unknown"))
        .collect()
}

/// Every std protocol x every position x {no import, from-import, aliased import, qualified}: with
/// no import the name is unknown; each import form makes it known. `Any` and `Error` (global) are
/// known with no import. The cells come from the `protocol` declarations in `std/*.chz`.
#[test]
fn std_protocol_import_grid() {
    let protocols = std_protocols();
    assert!(
        protocols.len() >= 25,
        "the grid reads {} protocols from std/*.chz",
        protocols.len()
    );
    let mut red: Vec<String> = Vec::new();
    for (name, module, arity) in protocols {
        let last = module.rsplit('.').next().unwrap_or(&module).to_string();
        let plain = positions(&spelled(&name, arity));
        let aliased = positions(&spelled("Alias", arity));
        let qualified = positions(&spelled(&format!("{last}.{name}"), arity));
        for (i, (label, body)) in plain.iter().enumerate() {
            let got = unknown_errors(body);
            if module == "std.prelude" {
                if !got.is_empty() {
                    red.push(format!("{label} {name}: global, got {got:?}"));
                }
                continue;
            }
            // (i) no import: the name is reported, and a bound also names the import.
            let errs = check_src(body);
            if !errs
                .iter()
                .any(|e| e.message.contains(&format!("'{name}'")))
            {
                red.push(format!("{label} {name}: no import checked, got {errs:?}"));
            }
            let want = format!("unknown name '{name}'");
            let hint = format!("import {name} from {module}");
            if i == 0
                && !errs
                    .iter()
                    .any(|e| e.message.contains(&want) && e.message.contains(&hint))
            {
                red.push(format!(
                    "{label} {name}: want `{want}` and `{hint}`, got {errs:?}"
                ));
            }
            // (ii) from-import, (iii) aliased import, (iv) qualified spelling after `import module`.
            let cells = [
                (
                    "from-import",
                    format!("import {name} from {module}\n{body}"),
                ),
                (
                    "aliased import",
                    format!("import {name} as Alias from {module}\n{}", aliased[i].1),
                ),
                ("qualified", format!("import {module}\n{}", qualified[i].1)),
            ];
            for (how, src) in cells {
                let got = unknown_errors(&src);
                if !got.is_empty() {
                    red.push(format!("{label} {name} {how}: {got:?}"));
                }
            }
        }
    }
    assert!(red.is_empty(), "red cells:\n{}", red.join("\n"));
}

/// One import binding licenses both meanings of `Iterator` (the value type and the bound), under
/// the imported name or an alias. Two bounds through different spellings are one `Iterator`
/// bound twice. An aliased non-`Iterator` protocol resolves through the same binding.
#[test]
fn iterator_alias_licenses_the_bound() {
    ok(
        "import Iterator as It from std.iter\nfn f[S: It[int]](s: S):\n    pass\nfn gen() -> It[int]:\n    yield 1\nprint(1)\n",
    );
    rejects(
        "import std.iter\nimport Iterator as It from std.iter\nfn f[S: It[int] + iter.Iterator[int]](s: S):\n    pass\n",
        "'S' has more than one Iterator bound",
    );
    rejects(
        "import Iterator from std.iter\nimport Iterator as It from std.iter\nfn f[S: It[int] + Iterator[int]](s: S):\n    pass\n",
        "'S' has more than one Iterator bound",
    );
    ok(
        "import Add as A from std.ops\nfn f[T: A](a: T, b: T) -> T:\n    return a + b\nprint(f(1, 2))\n",
    );
}

/// Importing a protocol from a module that does not declare it is an error, in every cell.
#[test]
fn std_protocol_import_from_the_wrong_module_is_rejected() {
    for (name, module, _) in std_protocols() {
        if module == "std.prelude" {
            continue;
        }
        let wrong = if module == "std.cmp" {
            "std.ops"
        } else {
            "std.cmp"
        };
        let src = format!("import {name} from {wrong}\nprint(1)\n");
        let errs = check_src(&src);
        assert!(
            !errs.is_empty(),
            "`import {name} from {wrong}` checked, but {module} owns '{name}'"
        );
    }
}

/// `Iterator[T]` as a value type needs the import, like every std protocol.
#[test]
fn iterator_value_type_needs_import() {
    let body = "fn count(n: int) -> Iterator[int]:\n    i := 0\n    while i < n:\n        yield i\n        i += 1\nprint([x for x in count(3)])\n";
    rejects(body, "unknown generic type 'Iterator'");
    ok(&format!("import Iterator from std.iter\n{body}"));
}

/// The qualified spelling `iter.Iterator[int]` is the generator type, like the bare spelling.
#[test]
fn qualified_iterator_is_the_generator_type() {
    ok(
        "import std.iter as iter\n\nfn count(n: int) -> iter.Iterator[int]:\n    i := 0\n    while i < n:\n        yield i\n        i += 1\n\nprint([x for x in count(3)])\n",
    );
    ok(
        "import std.iter\n\nfn count(n: int) -> iter.Iterator[int]:\n    i := 0\n    while i < n:\n        yield i\n        i += 1\n\nprint([x for x in count(3)])\n",
    );
}

/// The arity of a builtin type reached by qualified path comes from the type itself: a wrong count
/// is reported for `iter.Iterator` as it is for `concurrency.Shared`.
#[test]
fn qualified_builtin_type_arity_is_checked() {
    rejects(
        "import std.iter\nfn f(x: iter.Iterator[int, int]) -> int:\n    return 0\n",
        "type 'iter.Iterator' expects 1 type argument(s), got 2",
    );
    rejects(
        "import std.concurrency\nfn f(x: concurrency.Shared[int, int]) -> int:\n    return 0\n",
        "type 'concurrency.Shared' expects 1 type argument(s), got 2",
    );
    rejects(
        "import std.net\nfn f(x: net.Socket[int]) -> int:\n    return 0\n",
        "type 'net.Socket' expects 0 type argument(s), got 1",
    );
}

/// A builtin type that is not a protocol keeps its import: `Socket` needs `import Socket from std.net`.
#[test]
fn non_protocol_builtin_type_still_needs_its_import() {
    rejects(
        "fn f(s: Socket) -> int:\n    return 1\nprint(1)\n",
        "unknown type 'Socket'",
    );
}

/// Owner decision 2: `Hashable` embeds `Eq`, so a `[T: Hashable]` body may compare `T` and build a
/// `Set[T]`/`Map[T, _]` with no `Eq` bound, and a type with a `hash` whose `eq` cannot hold is not
/// `Hashable`.
#[test]
fn hashable_bound_licenses_eq() {
    ok(
        "import Hashable from std.cmp\nfn same[T: Hashable](a: T, b: T) -> bool:\n    return a == b\nprint(same(1, 1))\n",
    );
    ok(
        "import Hashable from std.cmp\nfn uniq[T: Hashable](xs: List[T]) -> int:\n    return Set.from_iter(xs).len()\nprint(uniq([1, 1, 2]))\n",
    );
    ok(
        "import Hashable from std.cmp\nfn tally[T: Hashable](x: T) -> int:\n    m := {x: 1}\n    return m.len()\nprint(tally(\"a\"))\n",
    );
    // Must still fail: a bound-free `T` still cannot be compared or used as a key.
    rejects(
        "fn same[T](a: T, b: T) -> bool:\n    return a == b\n",
        "T is not bounded by Eq",
    );
    rejects(
        "fn uniq[T](xs: List[T]) -> int:\n    return Set.from_iter(xs).len()\n",
        "type T does not satisfy Hashable",
    );
    // Must still fail: `hash` alone does not make `Box[Tag]` a key when its `eq` needs `compare`.
    // The message names the `Eq` half, which is the half that fails.
    rejects(
        "import Comparable from std.cmp\nstruct Tag:\n    n: int\nstruct Box[T]:\n    val: T\n    fn hash(self) -> int:\n        return 0\n    fn eq(self, other: Box[T]) -> bool where T: Comparable:\n        return self.val.compare(other.val) == 0\nprint(Set.from_iter([Box(Tag(1))]).len())\n",
        "type Box[Tag] does not satisfy Hashable (Box[Tag]'s `eq` requires Tag: Comparable)",
    );
}

/// An operator or a hook needs no import: the protocol name appears nowhere in the program.
#[test]
fn operators_need_no_import() {
    ok(
        "a := 1 + 2\nb := 3 < 4\nc := \"x\" == \"y\"\nxs := [1, 2, 3]\nd := xs[0]\nxs[1] = 5\ne := 2 in xs\nfor v in xs:\n    print(v)\nm := {1: 2}\nprint(a, b, c, d, e, \"{m}\", \"{xs}\")\n",
    );
    ok(
        "struct P:\n    n: int\n    fn add(self, other: P) -> P:\n        return P(self.n + other.n)\n    fn str(self) -> str:\n        return \"P\"\nprint(P(1) + P(2))\n",
    );
}

/// `std_protocol_decls` reads the `protocol` statements the build script extracted from its parse
/// of each std file. It must equal what a full parse of every `STD_FILES` entry finds, spans
/// included: a std file's spans carry its id, `index + 1`.
#[test]
fn std_protocol_decls_equal_a_full_parse() {
    let mut full: Vec<(String, String, crate::ast::Stmt)> = Vec::new();
    for (index, (rel, src)) in crate::native::std_embed::STD_FILES.iter().enumerate() {
        let module = format!("std.{}", rel.trim_end_matches(".chz").replace('/', "."));
        let (ast, _) = crate::parser::parse_module_source(src, index as u32 + 1, &|_: &str| false)
            .expect("std parses");
        for s in &ast.stmts {
            if let StmtKind::Protocol { name, .. } = &s.kind {
                full.push((name.clone(), module.clone(), s.clone()));
            }
        }
    }
    let found: Vec<_> = graph_pass::std_protocol_decls()
        .iter()
        .map(|p| (p.name.clone(), p.module.clone(), p.stmt.clone()))
        .collect();
    assert_eq!(found, full);
}

/// A signature error in a struct or enum method points into that method's line, not at the type's
/// header line.
#[test]
fn a_method_signature_error_points_at_the_method() {
    for (owner, head) in [
        ("struct", "struct S:\n    v: int\n"),
        ("enum", "enum E:\n    A\n"),
    ] {
        for (what, sig) in [
            ("bound", "fn m[T: Hashable](self, x: T) -> int"),
            ("param type", "fn m(self, x: Nope) -> int"),
        ] {
            let src = format!("{head}    {sig}:\n        return 0\nprint(1)\n");
            let errs = check_src(&src);
            assert_eq!(errs.len(), 1, "{owner} {what}: {errs:?}");
            assert_eq!(errs[0].span.line, 3, "{owner} {what}: {}", errs[0].message);
        }
    }
}
