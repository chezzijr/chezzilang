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

/// Every std protocol, one bound per cell: without its import the bound is `unknown name`, with
/// the import from its owning module the bound checks. `Any` and `Error` check with no import.
#[test]
fn std_protocol_import_grid() {
    let protocols = std_protocols();
    assert!(
        protocols.len() >= 25,
        "the grid reads {} protocols from std/*.chz",
        protocols.len()
    );
    for (name, module, arity) in protocols {
        let bound = spelled(&name, arity);
        let body = format!("fn f[T: {bound}](x: T) -> int:\n    return 1\nprint(1)\n");
        if module == "std.prelude" {
            ok(&body);
            continue;
        }
        let errs = check_src(&body);
        assert!(
            errs.iter()
                .any(|e| e.message.contains(&format!("unknown name '{name}'"))),
            "'{name}' without an import: expected `unknown name '{name}'`, got {errs:?}"
        );
        ok(&format!("import {name} from {module}\n{body}"));
    }
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
        "import Hashable from std.cmp\nfn uniq[T: Hashable](xs: List[T]) -> int:\n    return Set(xs).len()\nprint(uniq([1, 1, 2]))\n",
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
        "fn uniq[T](xs: List[T]) -> int:\n    return Set(xs).len()\n",
        "Set element type must implement Hashable",
    );
    // Must still fail: `hash` alone does not make `Box[Tag]` a key when its `eq` needs `compare`.
    rejects(
        "import Comparable from std.cmp\nstruct Tag:\n    n: int\nstruct Box[T]:\n    val: T\n    fn hash(self) -> int:\n        return 0\n    fn eq(self, other: Box[T]) -> bool where T: Comparable:\n        return self.val.compare(other.val) == 0\nprint(Set([Box(Tag(1))]).len())\n",
        "Set element type must implement Hashable",
    );
}

/// An operator or a hook needs no import: the protocol name appears nowhere in the program.
#[test]
fn operators_need_no_import() {
    ok(
        "a := 1 + 2\nb := 3 < 4\nc := \"x\" == \"y\"\nxs := [1, 2, 3]\nd := xs[0]\nxs[1] = 5\ne := 2 in xs\nfor v in xs:\n    print(v)\nm := {1: 2}\nprint(a, b, c, d, e, str(m), \"{xs}\")\n",
    );
    ok(
        "struct P:\n    n: int\n    fn add(self, other: P) -> P:\n        return P(self.n + other.n)\n    fn str(self) -> str:\n        return \"P\"\nprint(P(1) + P(2))\n",
    );
}
