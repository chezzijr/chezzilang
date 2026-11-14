//! One method set per type (wave 24 F4): a method answers the same in every use, whatever
//! declares it. Sources: a user method (`U.str`), a std native method (`str.upper`), an intrinsic
//! method (`str`/`eq`/`hash`/`compare`, the requirement of a std protocol a type satisfies with no
//! declaration behind it), a declared struct `copy`, an UNDECLARED struct `copy` (no such method:
//! a struct has no intrinsic `copy`), and `List.copy`. Uses: a direct call `v.m()`, a user-protocol
//! bound, a std-protocol bound, a bound method `v.m` (never a value), the type path `T.m` as a value
//! and called `T.m(v)`, the miss help, and the runtime result. Extra axes: function values compare
//! by identity everywhere (`==`, `contains`, `Atomic.cas`; a fn is not Hashable), the `RwShared`
//! miss help names the collection type that has the method, and `sum`/`contains` on an element
//! type with no `Add`/`Eq`.

use std::path::PathBuf;
use std::process::Command;

fn chezzi(tag: &str, cmd: &str, src: &str) -> (bool, String) {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("chezzi-mset-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.chz"), src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args([cmd, "main.chz"])
        .current_dir(&dir)
        .output()
        .expect("run chezzi");
    let _ = std::fs::remove_dir_all(&dir);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// The declarations every cell can read.
const PRELUDE: &str = "\
import Stringable from std.string
import Eq, Comparable, Hashable from std.cmp
protocol Show:
    fn str(self) -> str
protocol Same:
    fn eq(self, other: Self) -> bool
protocol Keyed:
    fn hash(self) -> int
protocol Ord3:
    fn compare(self, other: Self) -> int
protocol Up:
    fn upper(self) -> str
protocol Dup:
    fn copy(self) -> Self
struct P:
    x: int
struct U:
    x: int
    fn str(self) -> str:
        return \"U!\"
struct D:
    x: int
    fn copy(self) -> D:
        return D(self.x + 1)
fn show[T: Show](v: T) -> str:
    return v.str()
fn same[T: Same](a: T, b: T) -> bool:
    return a.eq(b)
fn keyed[T: Keyed](v: T) -> int:
    return v.hash()
fn ord3[T: Ord3](a: T, b: T) -> int:
    return a.compare(b)
fn up[T: Up](v: T) -> str:
    return v.upper()
fn twice[T: Dup](v: T) -> T:
    return v.copy()
fn sshow[T: Stringable](v: T) -> str:
    return v.str()
fn ssame[T: Eq](a: T, b: T) -> bool:
    return a.eq(b)
fn skeyed[T: Hashable](v: T) -> int:
    return v.hash()
fn sord[T: Comparable](a: T, b: T) -> int:
    return a.compare(b)
";

/// (statements, expected stdout). Each cell runs after [`PRELUDE`].
const ACCEPT: &[(&str, &str)] = &[
    // User method: direct, user bound, std bound, `T.m` value, `T.m(v)`.
    ("print(U(1).str())", "U!"),
    ("print(show(U(1)))", "U!"),
    ("print(sshow(U(1)))", "U!"),
    ("f := U.str\nprint(f(U(1)))", "U!"),
    ("print(U.str(U(1)))", "U!"),
    // Std native method: direct, user bound.
    ("print(\"ab\".upper())", "AB"),
    ("print(up(\"ab\"))", "AB"),
    // Intrinsic `str`: direct, user bound, std bound, on a scalar, a struct and a container.
    ("print((5).str())", "5"),
    ("print(\"a\".str())", "a"),
    ("print((1.5).str())", "1.5"),
    ("print(true.str())", "true"),
    ("print(P(1).str())", "P(x=1)"),
    ("print([1, 2].str())", "[1, 2]"),
    ("print((1, \"a\").str())", "(1, 'a')"),
    ("o: int? = ?3\nprint(o.str())", "3"),
    ("print(show(5))", "5"),
    ("print(show(P(1)))", "P(x=1)"),
    ("print(show([1]))", "[1]"),
    ("print(sshow(5))", "5"),
    ("print(sshow(P(1)))", "P(x=1)"),
    // Intrinsic `eq`: direct, user bound, std bound.
    ("print((5).eq(5))", "true"),
    ("print(P(1).eq(P(2)))", "false"),
    ("print([1].eq([1]))", "true"),
    ("print(same(P(1), P(1)))", "true"),
    ("print(same(5, 6))", "false"),
    ("print(ssame(P(1), P(1)))", "true"),
    // Intrinsic `hash`: direct, user bound, std bound.
    ("print((7).hash() == skeyed(7))", "true"),
    ("print(\"a\".hash() == keyed(\"a\"))", "true"),
    ("print((1, 2).hash() == skeyed((1, 2)))", "true"),
    // Intrinsic `compare`: direct, user bound, std bound.
    ("print((3).compare(4))", "-1"),
    ("print(\"b\".compare(\"a\"))", "1"),
    ("print([1, 2].compare([1, 2]))", "0"),
    ("print(ord3(3, 4))", "-1"),
    ("print(sord(3, 4))", "-1"),
    // A declared struct `copy`: direct, user bound, `T.m` value, `T.m(v)`.
    ("print(D(1).copy().x)", "2"),
    ("print(twice(D(1)).x)", "2"),
    ("f := D.copy\nprint(f(D(1)).x)", "2"),
    ("print(D.copy(D(1)).x)", "2"),
    // `List.copy` stays (shallow): direct, user bound.
    (
        "xs := [1, 2]\nys := xs.copy()\nys.push(3)\nprint(xs)",
        "[1, 2]",
    ),
    ("print(twice([1, 2]))", "[1, 2]"),
    // Every intrinsic row of a protocol with no type parameters is a method, operators included.
    ("print((5).add(3))", "8"),
    ("print((5).neg())", "-5"),
    ("print([1].add([2]))", "[1, 2]"),
    ("print({1, 2}.sub({1}))", "{2}"),
    ("print(\"a\".as_path())", "b'a'"),
    ("it := [1].iter()\nprint(it.eq(it))", "true"),
    ("f := fn(x: int) -> int: x\nprint(f.eq(f))", "true"),
    // A native handle is `Eq` by identity, as `==` compares it: `eq` is a direct method, and the
    // `where T: Eq` decls of `contains`/`index_of`/`dedup`/`unique` accept it.
    (
        "import Executor from std.concurrency\nex := Executor()\nex2 := Executor()\nprint(ex.eq(ex), ex.eq(ex2), ex == ex)",
        "true false true",
    ),
    (
        "import Executor from std.concurrency\nex := Executor()\nprint([ex].index_of(ex), [ex].contains(ex), [ex, ex].unique().len())",
        "0 true 1",
    ),
    (
        "import std.ffi\np := ffi.null()\nprint(p.eq(ffi.null()), [p].index_of(p))",
        "true 0",
    ),
    (
        "f := fn(x: int) -> int: x\nprint([f].index_of(f), [f, f].dedup().len())",
        "0 1",
    ),
    // A field named like an intrinsic method stays a field.
    ("struct F:\n    str: int\nprint(F(3).str)", "3"),
    // Function values compare by identity everywhere.
    ("f := fn(x: int) -> int: x\ng := f\nprint(f == g)", "true"),
    ("f := fn(x: int) -> int: x\nprint([f].contains(f))", "true"),
    (
        "fn mk() -> fn(int) -> int:\n    return fn(x: int) -> int: x\nprint(mk() == mk())",
        "false",
    ),
    (
        "import std.concurrency\nstruct Tag:\n    f: fn(int) -> int\nt := Tag(fn(x): x)\na := concurrency.Atomic(t)\nprint(a.cas(t, t))",
        "true",
    ),
    (
        "import std.concurrency\nstruct Tag:\n    f: fn(int) -> int\nt := Tag(fn(x): x)\na := concurrency.Atomic(t)\nprint(a.cas(Tag(fn(x): x), t))",
        "false",
    ),
    (
        "import std.concurrency\nf := fn(x: int) -> int: x\na := concurrency.Atomic(f)\nprint(a.cas(f, f))",
        "true",
    ),
];

#[test]
fn method_set_accept_cells() {
    let mut bad = Vec::new();
    for (i, (src, want)) in ACCEPT.iter().enumerate() {
        let (ok, got) = chezzi(&format!("acc{i}"), "run", &format!("{PRELUDE}{src}\n"));
        if !ok || got.trim_end() != *want {
            bad.push(format!("--- {src}\nwant: {want}\ngot:\n{got}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} cell(s) wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// (statements, a line the check error contains). Each cell runs after [`PRELUDE`].
const REJECT: &[(&str, &str)] = &[
    // An undeclared struct `copy` is no method in every use.
    ("print(P(1).copy().x)", "type P has no method 'copy'"),
    (
        "print(twice(P(1)).x)",
        "type P does not satisfy Dup (missing method 'copy')",
    ),
    ("f := P.copy", "type 'P' has no method 'copy'"),
    (
        "print(P.copy(P(1)).x)",
        "type 'P' has no static method 'copy'",
    ),
    ("p := P(1)\nf := p.copy", "type P has no field 'copy'"),
    // A bound method is never a value, whatever declares it.
    ("u := U(1)\nf := u.str", "'str' is a method"),
    ("p := P(1)\nf := p.str", "'str' is a method"),
    ("f := (5).str", "type int has no field 'str'"),
    // An intrinsic method has no proto, like a native one: the type path names the method.
    ("f := P.str", "'str' is an intrinsic method of 'P'"),
    ("print(P.str(P(1)))", "'str' is an intrinsic method of 'P'"),
    ("f := int.str", "'str' is an intrinsic method of 'int'"),
    (
        "f := str.upper",
        "'upper' is a method of the native type 'str'",
    ),
    // The miss help reads the one method set.
    ("print((5).strr())", "help: did you mean 'str'?"),
    ("print(P(1).eqq(P(1)))", "help: did you mean 'eq'?"),
    // A field named like an intrinsic method shadows it.
    (
        "struct F:\n    str: int\nprint(F(3).str())",
        "'str' is a field -- read it without parentheses",
    ),
    // An intrinsic method is only as wide as its protocol's grant.
    (
        "struct H:\n    x: int\nprint(H(1).hash())",
        "type H has no method 'hash'",
    ),
    (
        "print(P(1).compare(P(2)))",
        "type P has no method 'compare'",
    ),
    ("print(keyed(P(1)))", "type P does not satisfy Keyed"),
    ("print(ord3(P(1), P(2)))", "type P does not satisfy Ord3"),
    // A function value is not Hashable.
    (
        "f := fn(x: int) -> int: x\ns := {f}",
        "set element type must implement Hashable",
    ),
    // The `RwShared` miss help names the collection type that has the method.
    (
        "import RwShared from std.concurrency\nrw := RwShared({1: 2})\nprint(rw.has(1))",
        "help: use ConcurrentMap from std.concurrency.collection",
    ),
    (
        "import RwShared from std.concurrency\nrw := RwShared({1: 2})\nprint(rw.contains(1))",
        "help: use ConcurrentMap from std.concurrency.collection",
    ),
    (
        "import RwShared from std.concurrency\nrw := RwShared({1: 2})\nprint(rw.snapshot())",
        "help: use ConcurrentMap from std.concurrency.collection",
    ),
    (
        "import RwShared from std.concurrency\nrw := RwShared([1])\nprint(rw.fold(0, fn(a: int, x: int) -> int: a + x))",
        "help: use ConcurrentList from std.concurrency.collection",
    ),
    (
        "import RwShared from std.concurrency\nrw := RwShared({1})\nprint(rw.insert(1))",
        "help: use ConcurrentSet from std.concurrency.collection",
    ),
    // `sum` and `contains` on an element type with no `Add` / `Eq`.
    (
        "print([P(1)].sum())",
        "sum() requires a numeric list, found List[P]",
    ),
    (
        "struct Q:\n    x: int\nstruct Bx[T]:\n    v: T\n    fn eq(self, o: Bx[T]) -> bool where T: Comparable:\n        return true\nprint([Bx(Q(1))].contains(Bx(Q(1))))",
        "Bx[Q]'s `eq` requires Q: Comparable",
    ),
    (
        "struct Q:\n    x: int\nstruct Bx[T]:\n    v: T\n    fn eq(self, o: Bx[T]) -> bool where T: Comparable:\n        return true\nn := [Bx(Q(1))].index_of(Bx(Q(1)))",
        "type Bx[Q] does not satisfy Eq (Bx[Q]'s `eq` requires Q: Comparable)",
    ),
    (
        "fn g[T](xs: List[T], v: T) -> int:\n    return xs.index_of(v)",
        "type T does not satisfy Eq",
    ),
];

#[test]
fn method_set_reject_cells() {
    let mut bad = Vec::new();
    for (i, (src, want)) in REJECT.iter().enumerate() {
        let (ok, got) = chezzi(&format!("rej{i}"), "check", &format!("{PRELUDE}{src}\n"));
        if ok || !got.contains(want) {
            bad.push(format!("--- {src}\nwant: {want}\ngot:\n{got}"));
        }
    }
    assert!(
        bad.is_empty(),
        "{} cell(s) wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
