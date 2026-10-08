//! TICKET-210: one answer for what a fn-like path denotes, in every position. A call through a
//! path is the path's value applied (an instance method through its type takes its receiver
//! first, Rust's `Type::method(&v)`), a caller's type parameter pins a generic fn value like a
//! concrete type, a value head re-reads `head[k](args)` as index-then-call (Go, CPython), an alias
//! given type arguments is rejected by one rule, and a bound's instance method is reachable
//! through its type parameter (`T.get(v)`). One generated program per cell, run through the built
//! `chezzi` binary.

#[path = "support/grid_cell.rs"]
mod grid_cell;

use grid_cell::{Cell, Expect, run_grid};

const PRE: &str = "struct P:
    n: int
    fn get(self) -> int:
        return self.n
    fn set(self, v: int):
        self.n = v
    fn mk(n: int) -> P:
        return P(n)
struct Bx[T]:
    v: T
    fn make(v: T) -> Bx[T]:
        return Bx(v)
    fn get(self) -> T:
        return self.v
    fn put[U](self, u: U) -> U:
        return u
enum E:
    A(int)
    B
    fn tag(self) -> int:
        match self:
            E.A(n):
                return n
            E.B:
                return 0
enum R[T]:
    L(T)
    N
type BI = Bx[int]
type RI = R[int]
fn top(x: int) -> int:
    return x + 1
fn idt[T](x: T) -> T:
    return x
fn ap[A, B](f: fn(A) -> B, a: A) -> B:
    return f(a)
";

const LIB: &str = "struct Bx[T]:
    v: T
    fn get(self) -> T:
        return self.v
struct L:
    n: int
    fn get(self) -> int:
        return self.n
    fn bump(self, k: int = 1) -> int:
        return self.n + k
fn f(x: int) -> int:
    return x * 10
fn a(x: int) -> int:
    return x + 1
fn b(x: int) -> int:
    return x + 2
fs := [a, b]
fn pa(x: int):
    print(x + 1)
fn pb(x: int):
    print(x + 2)
ps := [pa, pb]
";

fn prints(s: &str) -> Expect {
    Expect::Prints(s.to_string())
}

/// `main.chz` = the shared prelude + `body`, beside `lib.chz`.
fn cell(name: String, body: &str, expect: Expect) -> Cell {
    Cell {
        name,
        files: vec![
            ("lib.chz".to_string(), LIB.to_string()),
            ("main.chz".to_string(), format!("{PRE}{body}\n")),
        ],
        expect,
    }
}

/// A program of `main.chz` alone, without the prelude.
fn bare(name: &str, src: &str, expect: Expect) -> Cell {
    Cell {
        name: name.to_string(),
        files: vec![
            ("lib.chz".to_string(), LIB.to_string()),
            ("main.chz".to_string(), format!("{src}\n")),
        ],
        expect,
    }
}

/// Path x position.
fn path_cells(out: &mut Vec<Cell>) {
    // (row, import line, path, args, fn type, suffix on the result, printed, positions)
    #[allow(clippy::type_complexity)]
    let rows: &[(&str, &str, &str, &str, &str, &str, &str, &str)] = &[
        ("plain", "", "top", "1", "fn(int) -> int", "", "2", "cltphs"),
        (
            "generic",
            "",
            "idt[int]",
            "3",
            "fn(int) -> int",
            "",
            "3",
            "cltphs",
        ),
        (
            "imported",
            "import f from lib",
            "f",
            "1",
            "fn(int) -> int",
            "",
            "10",
            "cltphs",
        ),
        (
            "modfn",
            "import lib",
            "lib.f",
            "2",
            "fn(int) -> int",
            "",
            "20",
            "cltphs",
        ),
        (
            "static",
            "",
            "P.mk",
            "4",
            "fn(int) -> P",
            ".n",
            "4",
            "cltphs",
        ),
        (
            "variant",
            "",
            "E.A",
            "5",
            "fn(int) -> E",
            ".tag()",
            "5",
            "cltphs",
        ),
        (
            "struct_get",
            "",
            "P.get",
            "P(6)",
            "fn(P) -> int",
            "",
            "6",
            "cltphs",
        ),
        (
            "generic_get",
            "",
            "Bx.get",
            "Bx(7)",
            "fn(Bx[int]) -> int",
            "",
            "7",
            "ctph",
        ),
        (
            "applied_get",
            "",
            "Bx[int].get",
            "Bx(7)",
            "fn(Bx[int]) -> int",
            "",
            "7",
            "cltphs",
        ),
        (
            "own_generic",
            "",
            "Bx[int].put[str]",
            "Bx(1), \"s\"",
            "fn(Bx[int], str) -> str",
            "",
            "s",
            "cltp",
        ),
        (
            "enum_tag",
            "",
            "E.tag",
            "E.A(8)",
            "fn(E) -> int",
            "",
            "8",
            "cltphs",
        ),
        (
            "qualified",
            "import lib",
            "lib.L.get",
            "lib.L(9)",
            "fn(lib.L) -> int",
            "",
            "9",
            "cltphs",
        ),
        (
            "qualified_generic",
            "import lib",
            "lib.Bx[int].get",
            "lib.Bx(9)",
            "fn(lib.Bx[int]) -> int",
            "",
            "9",
            "cltph",
        ),
        (
            "alias",
            "",
            "BI.get",
            "Bx(10)",
            "fn(Bx[int]) -> int",
            "",
            "10",
            "cltphs",
        ),
    ];
    for (row, imp, path, args, ty, suf, want, pos) in rows {
        for p in pos.chars() {
            let (pname, body) = match p {
                'c' => ("call", format!("print({path}({args}){suf})")),
                'l' => ("let", format!("g := {path}\nprint(g({args}){suf})")),
                't' => ("typed", format!("g: {ty} = {path}\nprint(g({args}){suf})")),
                'p' => ("paren", format!("print(({path})({args}){suf})")),
                'h' => ("hof", format!("print(ap({path}, {args}){suf})")),
                _ => (
                    "spawn",
                    format!(
                        "r := Channel[int](1)\nparallel:\n    spawn:\n        r.send({path}({args}){suf})\nprint(r.recv())"
                    ),
                ),
            };
            let body = if imp.is_empty() {
                body
            } else {
                format!("{imp}\n{body}")
            };
            out.push(cell(format!("{row}/{pname}"), &body, prints(want)));
        }
    }
}

/// Generic paths pinned to a caller's abstract param, under three spellings of that param.
fn abstract_cells(out: &mut Vec<Cell>) {
    for cp in ["V", "T", "U"] {
        let cases: &[(&str, &str)] = &[
            ("typed", "h: fn({cp}) -> {cp} = idt\n    return h(x)"),
            ("hof", "return ap(idt, x)"),
            ("map", "xs := [x]\n    return xs.map(idt)[0]"),
            ("hof_get", "return ap(Bx.get, Bx(x))"),
            (
                "typed_get",
                "h: fn(Bx[{cp}]) -> {cp} = Bx.get\n    return h(Bx(x))",
            ),
        ];
        for (n, b) in cases {
            let b = b.replace("{cp}", cp);
            let body =
                format!("fn o[{cp}](x: {cp}) -> {cp}:\n    {b}\nprint(o(1))\nprint(o(\"s\"))");
            out.push(cell(format!("abstract_{cp}/{n}"), &body, prints("1\ns")));
        }
    }
}

/// K3 runtime, misses, K4 and K6.
fn extra_cells(out: &mut Vec<Cell>) {
    out.push(cell(
        "k3/set_writes_receiver".into(),
        "p := P(1)\nP.set(p, 9)\nprint(p.n)",
        prints("9"),
    ));
    out.push(cell(
        "k3/qualified_named_arg".into(),
        "import lib\nprint(lib.L.bump(lib.L(1), k=2))",
        prints("3"),
    ));
    out.push(cell(
        "k3/no_receiver".into(),
        "print(P.get())",
        Expect::Rejects("'get' expects"),
    ));
    out.push(bare(
        "k3/native_method",
        "import std.io\nprint(io.Reader.close())",
        Expect::Rejects("call it on a value"),
    ));
    out.push(cell(
        "k3/miss".into(),
        "print(P.nope(P(1)))",
        Expect::Rejects("has no static method 'nope'"),
    ));
    out.push(cell(
        "k6/nongeneric_alias".into(),
        "type P2 = P\nprint(P2[int].get(P(1)))",
        Expect::Rejects("already fixes its type arguments"),
    ));
    out.push(cell(
        "k4/module_miss".into(),
        "import lib\nprint(lib.Nope[str](1))",
        Expect::Rejects("module 'lib' has no member 'Nope'"),
    ));
    // An unpinned generic alias takes its target's type arguments in an annotation, as in a
    // constructor call (`BB[int](9)`); a non-generic alias given any rejects.
    out.push(cell(
        "k6/unpinned_annotation".into(),
        "type BB = Bx\nx: BB[int] = Bx(1)\nprint(x.v + 1)",
        prints("2"),
    ));
    out.push(cell(
        "k6/unpinned_annotation_mismatch".into(),
        "type BB = Bx\nx: BB[int] = Bx(\"s\")\nprint(x.v)",
        Expect::Rejects("cannot assign"),
    ));
    out.push(cell(
        "k6/unpinned_annotation_arity".into(),
        "type BB = Bx\nx: BB[int, str] = Bx(1)\nprint(x.v)",
        Expect::Rejects("expects 1 type argument"),
    ));
    out.push(cell(
        "k6/nongeneric_annotation".into(),
        "type P2 = P\nx: P2[int] = P(1)\nprint(x.n)",
        Expect::Rejects("already fixes its type arguments"),
    ));
    for (n, b) in [
        ("static_call", "print(BI[str].make(\"s\").v)"),
        ("annotation", "x: BI[str] = Bx(1)\nprint(x.v)"),
        ("variant", "g := RI[str].L\nprint(g)"),
        ("bare", "g := RI[str]\nprint(g)"),
    ] {
        out.push(cell(
            format!("k6/{n}"),
            b,
            Expect::Rejects("already fixes its type arguments"),
        ));
    }
}

/// K5: a value head re-reads its call bracket as an index.
fn shadow_cells(out: &mut Vec<Cell>) {
    const AB: &str = "struct K:\n    n: int\nfn a(x: int) -> int:\n    return x + 1\nfn b(x: int) -> int:\n    return x + 2\n";
    let cases: &[(&str, &str, Expect)] = &[
        (
            "local",
            "fn run() -> int:\n    K := 1\n    fs := [a, b]\n    return fs[K](10)\nprint(run())",
            prints("12"),
        ),
        (
            "param",
            "fn run(K: int) -> int:\n    ps := [P(5), P(6)]\n    fs := [a, a]\n    return ps[K].get() + fs[K](0)\nprint(run(1))",
            prints("7"),
        ),
        (
            "global",
            "fs := [a, b]\ni := 1\nprint(fs[i](10))",
            prints("12"),
        ),
        (
            "local_named_like_generic_fn",
            "fn run() -> int:\n    idt := [a, b]\n    K := 0\n    return idt[K](1)\nprint(run())",
            prints("2"),
        ),
        ("generic_still_wins", "print(idt[int](3))", prints("3")),
        (
            "field",
            "struct H:\n    fs: List[fn(int) -> int]\nh := H([a, b])\nk := 1\nprint(h.fs[k](10))",
            prints("12"),
        ),
        (
            "deep_field",
            "struct H:\n    fs: List[fn(int) -> int]\nhs := [H([a, b])]\nk := 1\nprint(hs[0].fs[k](10))",
            prints("12"),
        ),
        (
            "module_global",
            "import lib\nk := 1\nprint(lib.fs[k](10))",
            prints("12"),
        ),
        (
            "defer_local",
            "fn pa(x: int):\n    print(x + 1)\nfn pb(x: int):\n    print(x + 2)\nfn run():\n    fs := [pa, pb]\n    k := 1\n    defer fs[k](1)\n    print(\"body\")\nrun()",
            prints("body\n3"),
        ),
        (
            "defer_field",
            "fn pa(x: int):\n    print(x + 1)\nfn pb(x: int):\n    print(x + 2)\nstruct H:\n    fs: List[fn(int) -> None]\nfn run():\n    h := H([pa, pb])\n    k := 1\n    defer h.fs[k](1)\n    print(\"body\")\nrun()",
            prints("body\n3"),
        ),
        (
            "defer_module_global",
            "import lib\nfn run():\n    k := 1\n    defer lib.ps[k](1)\n    print(\"body\")\nrun()",
            prints("body\n3"),
        ),
        (
            "spawn_local",
            "fn pa(x: int):\n    print(x + 1)\nfn pb(x: int):\n    print(x + 2)\nfs := [pa, pb]\nk := 1\nparallel:\n    spawn fs[k](10)\nprint(\"after\")",
            prints("12\nafter"),
        ),
        (
            "spawn_field",
            "fn pa(x: int):\n    print(x + 1)\nfn pb(x: int):\n    print(x + 2)\nstruct H:\n    fs: List[fn(int) -> None]\nh := H([pa, pb])\nk := 1\nparallel:\n    spawn h.fs[k](10)\nprint(\"after\")",
            prints("12\nafter"),
        ),
        (
            "spawn_module_global",
            "import lib\nk := 1\nparallel:\n    spawn lib.ps[k](10)\nprint(\"after\")",
            prints("12\nafter"),
        ),
        (
            "method_turbofish_wins",
            "struct W:\n    n: int\n    fn cast[U](self, u: U) -> U:\n        return u\nprint(W(1).cast[str](\"a\"))",
            prints("a"),
        ),
        (
            "fn_field_rejects",
            "struct H2:\n    f: fn(int) -> int\nh := H2(a)\nprint(h.f[int](3))",
            Expect::Rejects("cannot index into"),
        ),
    ];
    for (n, b, e) in cases {
        let e = match e {
            Expect::Prints(s) => Expect::Prints(s.clone()),
            Expect::Rejects(f) => Expect::Rejects(f),
        };
        out.push(cell(format!("k5/{n}"), &format!("{AB}{b}"), e));
    }
}

/// K2 decl-site defaults, hint positions and `hint_want`, each in two spellings of `ident`.
fn hint_cells(out: &mut Vec<Cell>) {
    for s in ["T", "U"] {
        let id = format!("fn ident[{s}](x: {s}) -> {s}:\n    return x\n");
        let cases: &[(&str, &str, &str)] = &[
            (
                "default_param",
                "fn g[U](x: U, f: fn(U) -> U = ident) -> U:\n    return f(x)\nprint(g(1))\nprint(g(\"s\"))",
                "1\ns",
            ),
            (
                "default_field",
                "struct H[U]:\n    n: U\n    f: fn(U) -> U = ident\nh := H[int](3)\nprint(h.f(h.n))",
                "3",
            ),
            (
                "default_method_param",
                "struct W[U]:\n    n: U\n    fn ap(self, f: fn(U) -> U = ident) -> U:\n        return f(self.n)\nprint(W(4).ap())",
                "4",
            ),
            (
                "reassign",
                "fn o[U](x: U) -> U:\n    h: fn(U) -> U = fn(y): y\n    h = ident\n    return h(x)\nprint(o(1))",
                "1",
            ),
            (
                "some",
                "fn o[U](x: U) -> U:\n    h: (fn(U) -> U)? = ?ident\n    match h:\n        ?f:\n            return f(x)\n        None:\n            return x\nprint(o(2))",
                "2",
            ),
            (
                "some_reassign",
                "fn o[U](x: U) -> U:\n    h: (fn(U) -> U)? = None\n    h = ?ident\n    match h:\n        ?f:\n            return f(x)\n        None:\n            return x\nprint(o(2))",
                "2",
            ),
            (
                "ok",
                "fn o[U](x: U) -> (fn(U) -> U)!str:\n    return ?ident\nmatch o(1):\n    ?f:\n        print(f(4))\n    !e:\n        print(e)",
                "4",
            ),
            (
                "tuple",
                "fn o[U](x: U) -> U:\n    t: (fn(U) -> U, int) = (ident, 1)\n    return t.0(x)\nprint(o(3))",
                "3",
            ),
            (
                "list_map_ctor",
                "struct H[V]:\n    f: fn(V) -> V\nfn o[U](x: U) -> U:\n    xs: List[fn(U) -> U] = [ident]\n    m: Map[str, fn(U) -> U] = {\"a\": ident}\n    h := H[U](ident)\n    return h.f(m[\"a\"](xs[0](x)))\nprint(o(5))",
                "5",
            ),
            (
                "concrete_reassign",
                "h: fn(int) -> int = ident\nh = ident\nprint(h(1))",
                "1",
            ),
            (
                "hint_want",
                "struct H[V]:\n    f: fn(V) -> V\nenum Q[V]:\n    A(fn(V) -> V)\n    B\nfn o[U](x: U) -> U:\n    h: H[U] = H(ident)\n    return h.f(x)\nfn o2[U](x: U) -> U:\n    r: Q[U] = Q.A(ident)\n    match r:\n        Q.A(g):\n            return g(x)\n        Q.B:\n            return x\nprint(o(7))\nprint(o2(8))",
                "7\n8",
            ),
        ];
        for (n, b, want) in cases {
            out.push(bare(
                &format!("k2_{s}/{n}"),
                &format!("{id}{b}"),
                prints(want),
            ));
        }
        out.push(bare(
            &format!("k2_{s}/two_default_rejects"),
            &format!(
                "fn two[{s}, B](a: {s}) -> {s}:\n    return a\nfn g[U](x: U, f: fn(U) -> U = two) -> U:\n    return f(x)\nprint(g(1))"
            ),
            Expect::Rejects("is generic and"),
        ));
    }
    out.push(bare(
        "k2/hint_want_control",
        "struct H[V]:\n    f: fn(V) -> V\nenum Q[V]:\n    A(fn(V) -> V)\n    B\nfn ident[T](x: T) -> T:\n    return x\nh: H[int] = H(ident)\nprint(h.f(5))\nr: Q[int] = Q.A(ident)\nmatch r:\n    Q.A(g):\n        print(g(6))\n    Q.B:\n        print(0)",
        prints("5\n6"),
    ));
    out.push(bare(
        "k2/typed_let_two_rejects",
        "fn two[A, B](a: A) -> A:\n    return a\nfn o[V](x: V) -> V:\n    h: fn(V) -> V = two\n    return h(x)\nprint(o(1))",
        Expect::Rejects("is generic and"),
    ));
    out.push(bare(
        "k2/tuple_two_rejects",
        "fn two[A, B](a: A) -> A:\n    return a\nfn o[U](x: U) -> U:\n    t: (fn(U) -> U, int) = (two, 1)\n    return t.0(x)\nprint(o(1))",
        Expect::Rejects("is generic and"),
    ));
}

/// A bound's instance method through its type parameter.
fn param_cells(out: &mut Vec<Cell>) {
    const G: &str = "protocol Getter:\n    fn get(self) -> int\nstruct P:\n    n: int\n    fn get(self) -> int:\n        return self.n\n    fn set(self, n: int):\n        self.n = n\n    fn mk() -> P:\n        return P(9)\nfn ap[A](f: fn(A) -> int, a: A) -> int:\n    return f(a)\n";
    let cases: &[(&str, &str, Expect)] = &[
        (
            "call",
            "fn f[T: Getter](v: T) -> int:\n    return T.get(v)\nprint(f(P(1)))",
            prints("1"),
        ),
        (
            "let",
            "fn f[T: Getter](v: T) -> int:\n    g := T.get\n    return g(v)\nprint(f(P(2)))",
            prints("2"),
        ),
        (
            "typed",
            "fn f[T: Getter](v: T) -> int:\n    g: fn(T) -> int = T.get\n    return g(v)\nprint(f(P(2)))",
            prints("2"),
        ),
        (
            "paren_hof",
            "fn f[T: Getter](v: T) -> int:\n    return (T.get)(v) + ap(T.get, v)\nprint(f(P(3)))",
            prints("6"),
        ),
        (
            "generic_struct_method",
            "struct Bx[T: Getter]:\n    v: T\n    fn via(self) -> int:\n        return T.get(self.v)\nprint(Bx(P(4)).via())",
            prints("4"),
        ),
        (
            "spawn",
            "fn f[T: Getter](v: T) -> int:\n    r := Channel[int](1)\n    parallel:\n        spawn:\n            r.send(T.get(v))\n    return r.recv()\nprint(f(P(5)))",
            prints("5"),
        ),
        (
            "defer",
            "protocol Setter:\n    fn set(self, n: int)\nfn f[T: Setter](v: T):\n    defer T.set(v, 9)\n    print(\"body\")\np := P(1)\nf(p)\nprint(p.n)",
            prints("body\n9"),
        ),
        (
            "static_guard",
            "protocol Mk:\n    fn mk() -> Self\nfn make[T: Mk]() -> T:\n    return T.mk()\nprint(make[P]().n)",
            prints("9"),
        ),
        (
            "miss_call",
            "fn f[T: Getter](v: T) -> int:\n    return T.nope(v)\nprint(f(P(1)))",
            Expect::Rejects("no bound on 'T' declares a static method 'nope'"),
        ),
        (
            "miss_value",
            "fn f[T: Getter](v: T) -> int:\n    g := T.nope\n    return 0\nprint(f(P(1)))",
            Expect::Rejects("a type parameter has no member 'nope'"),
        ),
    ];
    for (n, b, e) in cases {
        let e = match e {
            Expect::Prints(s) => Expect::Prints(s.clone()),
            Expect::Rejects(f) => Expect::Rejects(f),
        };
        out.push(bare(&format!("param/{n}"), &format!("{G}{b}"), e));
    }
}

#[test]
fn path_call_grid() {
    let mut cells = Vec::new();
    path_cells(&mut cells);
    abstract_cells(&mut cells);
    extra_cells(&mut cells);
    shadow_cells(&mut cells);
    hint_cells(&mut cells);
    param_cells(&mut cells);
    run_grid("path-call", &cells);
}

const GLIB: &str = "fn g[T](x: T) -> T:
    return x
fn f(x: int) -> int:
    return x
struct Bx[T]:
    v: T
    fn make(v: T) -> Bx[T]:
        return Bx(v=v)
    fn get(self) -> T:
        return self.v
enum E[T]:
    A(T)
    N
";

/// TICKET-222: a parenthesised path callee is the path value applied, for every head, with and
/// without type arguments (Rust `(i64::abs)(-6)`, `(g::<i64>)(6)` print `6 6`).
#[test]
fn parenthesised_turbofish_call_grid() {
    let pre = "import std.json
import std.math
import lib
import lib as L
import a.b
import Bx, E from lib
fn idt[T](x: T) -> T:
    return x
fn top(x: int) -> int:
    return x
type BX = Bx[int]
";
    // (head, path, argument, show of the result, printed)
    let heads = [
        ("local", "idt[int]", "6", "{}", "6"),
        ("mod", "lib.g[int]", "6", "{}", "6"),
        ("alias_mod", "L.g[int]", "6", "{}", "6"),
        ("full", "a.b.g[int]", "7", "{}", "7"),
        ("json", "json.decode[int]", "'3'", "{}", "3"),
        ("abs", "math.abs[int]", "-6", "{}", "6"),
        ("make", "Bx[int].make", "6", "{}.v", "6"),
        ("get", "Bx[int].get", "Bx[int](v=6)", "{}", "6"),
        ("variant", "E[int].A", "6", "{}", "A(6)"),
        ("alias", "BX.make", "6", "{}.v", "6"),
        ("plain", "top", "6", "{}", "6"),
        ("plain_mod", "lib.f", "5", "{}", "5"),
        ("plain_static", "lib.Bx[int].make", "6", "{}.v", "6"),
    ];
    let mut cells = Vec::new();
    for (h, path, arg, show, want) in heads {
        let rows = [
            ("paren_call", format!("r := ({path})({arg})")),
            ("paren_let", format!("v := ({path})\nr := v({arg})")),
        ];
        for (p, body) in rows {
            let r = show.replace("{}", "r");
            cells.push(Cell {
                name: format!("paren/{h}/{p}"),
                files: vec![
                    ("lib.chz".to_string(), GLIB.to_string()),
                    ("a/b.chz".to_string(), GLIB.to_string()),
                    ("main.chz".to_string(), format!("{pre}{body}\nprint({r})\n")),
                ],
                expect: prints(want),
            });
        }
    }
    run_grid("paren-turbofish-call", &cells);
}
