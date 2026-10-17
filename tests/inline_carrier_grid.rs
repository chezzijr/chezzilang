//! TICKET-248 — an optional built with no type hint, one program per (consumer, source).
//! The source is `?V` inline, `o` after `o := ?V`, or `o` after `o: P? = ?V`. The three sources
//! must agree in every consumer but the two joins with `!e`, where only the inline `?V` is still
//! open when its sibling is read. The extra rows hold the inline-only neighbours: joins in both
//! orders, an inline body, a `T!E` parameter, and the constant-width check under one carrier.

#[path = "support/grid_cell.rs"]
mod grid_cell;

use grid_cell::{Cell, Expect, run_grid};

const LOCATED: &str = "protocol Located:\n    fn line(self) -> int\nstruct Tok:\n    n: int\n    fn line(self) -> int:\n        return self.n\n";
const NC: &str = "struct NC:\n    n: int\n";
const UN1: &str =
    "fn un1[T](o: T?) -> List[T]:\n    match o:\n        ?v: return [v]\n        None: return []\n";

enum Out {
    /// All three sources print this.
    P(&'static str),
    /// All three sources are rejected with this fragment.
    R(&'static str),
    /// The inline source prints the first; the two named sources are rejected with the second.
    Join(&'static str, &'static str),
}

/// One consumer: `decls` at top level, `V` the payload, `P` its type, `{S}` in `body` the source.
struct Consumer {
    name: &'static str,
    decls: String,
    v: &'static str,
    p: &'static str,
    body: &'static str,
    out: Out,
}

fn consumer(
    name: &'static str,
    decls: &[&str],
    v: &'static str,
    p: &'static str,
    body: &'static str,
    out: Out,
) -> Consumer {
    Consumer {
        name,
        decls: decls.concat(),
        v,
        p,
        body,
        out,
    }
}

fn consumers() -> Vec<Consumer> {
    use Out::{Join, P, R};
    vec![
        consumer(
            "num",
            &["import Num from std.ops\nfn double[T: Num](x: T) -> T:\n    return x + x\n"],
            "5",
            "int",
            "print(double({S}))",
            R("type int? does not satisfy Num"),
        ),
        consumer(
            "user-protocol",
            &[
                LOCATED,
                "fn where_is[T: Located](x: T) -> int:\n    return x.line()\n",
            ],
            "Tok(7)",
            "Tok",
            "n: int = where_is({S})\nprint(n)",
            R("type Tok? does not satisfy Located"),
        ),
        consumer(
            "comparable",
            &[
                "import Comparable from std.cmp\n",
                NC,
                "fn mx[T: Comparable](a: T, b: T) -> T:\n    return if a < b: b else: a\n",
            ],
            "NC(1)",
            "NC",
            "print(mx({S}, {S}))",
            R("type NC? does not satisfy Comparable"),
        ),
        consumer(
            "hashable-eq",
            &[
                "import Eq, Hashable from std.cmp\nfn hs[T: Hashable + Eq](a: T) -> Set[T]:\n    return {a}\n",
            ],
            "1",
            "int",
            "print(hs({S}))",
            R("type int? does not satisfy Hashable"),
        ),
        consumer(
            "iterator",
            &[
                "import Iterator from std.iter\nfn total[I: Iterator[int]](it: I) -> int:\n    s := 0\n    for v in it:\n        s += v\n    return s\n",
            ],
            "[1, 2].iter()",
            "Iterator[int]",
            "print(total({S}))",
            R("type Iterator[int]? does not satisfy Iterator"),
        ),
        consumer(
            "struct-bound",
            &[
                "import Comparable from std.cmp\n",
                NC,
                "struct Ord[T: Comparable]:\n    v: T\n",
            ],
            "NC(1)",
            "NC",
            "w := Ord({S})\nprint(w)",
            R("type NC? does not satisfy Comparable"),
        ),
        consumer(
            "where-method",
            &[
                "import Num from std.ops\nstruct Bx[T]:\n    v: T\n    fn dbl(self) -> T where T: Num:\n        return self.v + self.v\n",
            ],
            "5",
            "int",
            "print(Bx({S}).dbl())",
            R("type int? does not satisfy Num"),
        ),
        consumer(
            "list-max",
            &[NC],
            "NC(2)",
            "NC",
            "print([{S}, {S}].max())",
            R("type NC? does not satisfy Comparable"),
        ),
        consumer("opt-param", &[UN1], "5", "int", "print(un1({S}))", P("[5]")),
        consumer(
            "list-opt-param",
            &[
                "fn firsts[T](xs: List[T?]) -> List[T]:\n    out: List[T] = []\n    for x in xs:\n        match x:\n            ?v: out.push(v)\n            None: pass\n    return out\n",
            ],
            "5",
            "int",
            "print(firsts([{S}, {S}]))",
            P("[5, 5]"),
        ),
        consumer(
            "struct-field",
            &["struct Cell[T]:\n    v: T?\n"],
            "5",
            "int",
            "print(Cell({S}))",
            P("Cell(v=5)"),
        ),
        consumer(
            "enum-payload",
            &["enum Tr[T]:\n    Leaf\n    Node(T?, int)\n"],
            "5",
            "int",
            "print(Tr.Node({S}, 1))",
            P("Node(5, 1)"),
        ),
        consumer(
            "lambda-receiver",
            &[],
            "5",
            "int",
            "print([{S}, {S}].map(fn(o): o ?? 0))",
            P("[5, 5]"),
        ),
        consumer(
            "lambda-argument",
            &["fn ap[T, U](x: T, f: fn(T) -> U) -> U:\n    return f(x)\n"],
            "5",
            "int",
            "print(ap({S}, fn(o): o ?? 0))",
            P("5"),
        ),
        consumer(
            "lambda-struct-method",
            &[
                "struct Box[T]:\n    v: T\n    fn map[U](self, f: fn(T) -> U) -> Box[U]:\n        return Box(f(self.v))\n",
            ],
            "5",
            "int",
            "print(Box({S}).map(fn(o): o ?? 0).v)",
            P("5"),
        ),
        consumer("any-let", &[], "5", "int", "x: Any = {S}\nprint(x)", P("5")),
        consumer(
            "any-param",
            &["fn show(x: Any) -> str:\n    return \"{x}\"\n"],
            "5",
            "int",
            "print(show({S}))",
            P("5"),
        ),
        consumer(
            "any-field",
            &["struct B:\n    v: Any\n"],
            "5",
            "int",
            "print(B({S}))",
            P("B(v=5)"),
        ),
        consumer(
            "any-list-push",
            &[],
            "5",
            "int",
            "xs: List[Any] = [1]\nxs.push({S})\nprint(xs)",
            P("[1, 5]"),
        ),
        consumer(
            "protocol-param",
            &[LOCATED, "fn at(x: Located) -> int:\n    return x.line()\n"],
            "Tok(7)",
            "Tok",
            "print(at({S}))",
            R("Tok? does not satisfy Located"),
        ),
        consumer(
            "format-spec",
            &[],
            "5",
            "int",
            "print(\"{{S}:05}\")",
            R("zero-pad '0' not allowed on a string (int? is formatted as its text form)"),
        ),
        consumer(
            "payload-mismatch",
            &["fn un[T](o: T?, d: T) -> T:\n    return o ?? d\n"],
            "5",
            "int",
            "print(un({S}, 1.5))",
            R("argument to 'un' has type float, expected int"),
        ),
        consumer(
            "join-ok-err",
            &[],
            "5",
            "int",
            "a := [{S}, !\"e\"]\nprint(a)",
            Join("[5, !'e']", "list elements differ: int? vs _!str"),
        ),
        consumer(
            "join-err-ok",
            &[],
            "5",
            "int",
            "a := [!\"e\", {S}]\nprint(a)",
            Join("[!'e', 5]", "list elements differ: _!str vs int?"),
        ),
    ]
}

fn main_only(name: String, src: String, expect: Expect) -> Cell {
    Cell {
        name,
        files: vec![("main.chz".to_string(), src)],
        expect,
    }
}

fn prints(s: &str) -> Expect {
    Expect::Prints(s.to_string())
}

fn source_cells(c: &Consumer) -> Vec<Cell> {
    let sources = [
        ("inline", String::new(), format!("?{}", c.v)),
        ("inferred", format!("o := ?{}\n", c.v), "o".to_string()),
        (
            "annotated",
            format!("o: {}? = ?{}\n", c.p, c.v),
            "o".to_string(),
        ),
    ];
    sources
        .into_iter()
        .map(|(source, pre, s)| {
            let src = format!("{}{pre}{}\n", c.decls, c.body.replace("{S}", &s));
            let expect = match (&c.out, source) {
                (Out::P(want), _) | (Out::Join(want, _), "inline") => prints(want),
                (Out::R(frag), _) | (Out::Join(_, frag), _) => Expect::Rejects(frag),
            };
            main_only(format!("{}/{source}", c.name), src, expect)
        })
        .collect()
}

/// `body` inside `fn main():`, after the top-level `decls`.
fn in_main(decls: &str, body: &[&str]) -> String {
    let lines: Vec<String> = body.iter().map(|l| format!("    {l}\n")).collect();
    format!("{decls}fn main():\n{}main()\n", lines.concat())
}

/// The inline-only rows of the ticket's second table.
fn extra_cells() -> Vec<Cell> {
    const INT8: &str = "import int8 from std.ffi\n";
    const TOO_WIDE: &str = "describes the C boundary only";
    let p = |name: &str, src: String, want: &str| main_only(name.to_string(), src, prints(want));
    let r = |name: &str, src: String, frag: &'static str| {
        main_only(name.to_string(), src, Expect::Rejects(frag))
    };
    vec![
        p(
            "join-list/ok-err",
            in_main("", &["a := [?1, !\"e\"]", "print(a)"]),
            "[1, !'e']",
        ),
        p(
            "join-list/err-ok",
            in_main("", &["a := [!\"e\", ?1]", "print(a)"]),
            "[!'e', 1]",
        ),
        p(
            "join-if/ok-err",
            in_main("", &["c := true", "b := if c: ?1 else: !\"e\"", "print(b)"]),
            "1",
        ),
        p(
            "join-if/err-ok",
            in_main("", &["c := true", "b := if c: !\"e\" else: ?1", "print(b)"]),
            "!e",
        ),
        p(
            "join-map/ok-err",
            in_main("", &["m := {\"a\": ?1, \"b\": !\"e\"}", "print(m)"]),
            "{'a': 1, 'b': !'e'}",
        ),
        p(
            "join-map/err-ok",
            in_main("", &["m := {\"a\": !\"e\", \"b\": ?1}", "print(m)"]),
            "{'a': !'e', 'b': 1}",
        ),
        p(
            "join-match/ok-err",
            in_main(
                "",
                &[
                    "c := true",
                    "d := match c:",
                    "    false: ?1",
                    "    true: !\"e\"",
                    "print(d)",
                ],
            ),
            "!e",
        ),
        p(
            "join-match/err-ok",
            in_main(
                "",
                &[
                    "c := true",
                    "d := match c:",
                    "    true: !\"e\"",
                    "    false: ?1",
                    "print(d)",
                ],
            ),
            "!e",
        ),
        p(
            "inline-body/err-ok",
            "fn re(c: bool): if c: !\"e\" else: ?1\nx := re(false)\nprint(x ?? 9)\n".to_string(),
            "1",
        ),
        p(
            "inline-body",
            "fn mk(): ?5\nprint(mk() ?? 0)\n".to_string(),
            "5",
        ),
        p(
            "result-param",
            "fn rp[T](r: T!str) -> List[T]:\n    match r:\n        ?v: return [v]\n        !e: return []\nprint(rp(?5))\n"
                .to_string(),
            "[5]",
        ),
        r(
            "result-param-open-error",
            "fn rp[T, E](r: T!E) -> int:\n    return 1\nprint(rp(?5))\n".to_string(),
            "argument to 'rp' has type int?, expected T!E",
        ),
        p(
            "tuple-opt-param",
            "fn tp[T](p: (T?, int)) -> List[T]:\n    return []\nprint(tp((?5, 1)))\n".to_string(),
            "[]",
        ),
        p("nested", format!("{UN1}print(un1(??5))\n"), "[5]"),
        p(
            "sibling-pins-result",
            in_main(
                "fn mv[T](a: List[T], b: List[T]) -> int:\n    return a.len() + b.len()\n",
                &["ys: List[int!str] = []", "print(mv([?5], ys))"],
            ),
            "1",
        ),
        p(
            "bare-T-annotation",
            in_main(
                "fn g[T](x: T) -> T:\n    return x\n",
                &["r: int!str = g(?5)", "print(r)"],
            ),
            "5",
        ),
        p(
            "lambda-later-result",
            in_main(
                "fn pk[T](x: T, f: fn(T) -> int, y: T) -> int:\n    return f(x)\n",
                &["r: int!str = ?1", "print(pk(?5, fn(o): 1, r))"],
            ),
            "1",
        ),
        r(
            "plain-sibling",
            "fn same[T](a: T, b: T) -> T:\n    return a\nprint(same(1, ?5))\n".to_string(),
            "argument to 'same' has type int?, expected int",
        ),
        r(
            "mismatch-if",
            in_main("", &["c := true", "x := if c: ?5 else: ?1.5", "print(x)"]),
            "'?' value: expected int, found float",
        ),
        r(
            "mismatch-list",
            in_main("", &["x := [?5, ?1.5]", "print(x)"]),
            "'?' value: expected int, found float",
        ),
        r(
            "arith",
            "print(1 + ?5)\n".to_string(),
            "cannot apply + to int and int?",
        ),
        p(
            "compare/result-fn",
            "fn f() -> int!:\n    return 2\nprint(?2 == f())\n".to_string(),
            "true",
        ),
        p(
            "compare/two-optionals",
            "print(?1 == ?1, ?1 == None)\n".to_string(),
            "true false",
        ),
        r(
            "h8/tuple-under-optional",
            format!("{INT8}t: (int8, int)? = (300, 1)\nprint(t)\n"),
            TOO_WIDE,
        ),
        r(
            "h8/tuple-under-result",
            format!("{INT8}t: (int8, int)!str = (300, 1)\nprint(t)\n"),
            TOO_WIDE,
        ),
        r(
            "h8/tuple-plain",
            format!("{INT8}t: (int8, int) = (300, 1)\nprint(t)\n"),
            TOO_WIDE,
        ),
        r(
            "h8/list-under-optional",
            format!("{INT8}t: List[int8]? = [300]\nprint(t)\n"),
            TOO_WIDE,
        ),
        r(
            "h8/tuple-under-optional-fits",
            format!("{INT8}t: (int8, int)? = (100, 1)\nprint(t)\n"),
            TOO_WIDE,
        ),
    ]
}

#[test]
fn inline_carrier_grid() {
    let mut cells: Vec<Cell> = consumers().iter().flat_map(source_cells).collect();
    cells.extend(extra_cells());
    run_grid("inline-carrier", &cells, &[]);
}
