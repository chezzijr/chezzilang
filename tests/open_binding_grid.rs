//! TICKET-238 -- the type of a value is known on the statement that creates it.
//! One program per cell: creation form x creation site. An untyped binder of an open form is
//! rejected on its own line with the one `cannot infer the ...` error; a site that supplies the
//! type on the same statement is accepted and RUN, with the type observed through a typed sink
//! (`fn sink(x: T)`) or a `match`. `?5` is `int?` at once, so it is accepted at every site.
//!
//! After the grid: every wave-22 Family A repro (rejected at its creation line, one error per
//! open binding), the same-statement solving cells, the direct `?x` readers, and the cascade
//! cells, which count the `error` objects of `chezzi check --errors=json`.

#[path = "support/grid_cell.rs"]
mod grid_cell;

use grid_cell::{Cell, Expect, run_cell};
use std::process::Command;

const PRELUDE: &str = "struct Cell[T]:
    v: T?
struct Box[T]:
    items: List[T]
    fn new() -> Box[T]:
        return Box([])
enum Bx[T]:
    Full(T)
    Empty
fn id[T](x: T) -> T:
    return x
fn ident[T](x: T) -> T:
    return x
";

const REJECT: &str = "cannot infer the";

/// (label, expression, typed spelling, what `print(E)` shows).
const FORMS: [(&str, &str, &str, &str); 14] = [
    ("[]", "[]", "List[int]", "[]"),
    ("{}", "{}", "Map[str, int]", "{}"),
    ("Set()", "Set()", "Set[int]", "Set()"),
    ("None", "None", "int?", "None"),
    ("[None]", "[None]", "List[int?]", "[None]"),
    ("[[]]", "[[]]", "List[List[int]]", "[[]]"),
    ("([], 1)", "([], 1)", "(List[int], int)", "([], 1)"),
    ("Cell(None)", "Cell(None)", "Cell[int]", "Cell(v=None)"),
    ("Box.new()", "Box.new()", "Box[int]", "Box(items=[])"),
    ("Bx.Empty", "Bx.Empty", "Bx[int]", "Empty"),
    ("id([])", "id([])", "List[int]", "[]"),
    ("ident", "ident", "fn(int) -> int", ""),
    ("?5", "?5", "int?", "5"),
    ("comprehension", "[x for x in []]", "List[int]", "[]"),
];

#[derive(Clone, Copy, PartialEq)]
enum Verdict {
    Reject,
    Sink,
    Print,
}

/// `top` goes after the prelude and `body` inside `fn main():`; an empty `body` is a top-level
/// cell. `read` is appended for `?5`, whose untyped cells are accepted and read by `match`.
struct Site {
    name: &'static str,
    verdict: Verdict,
    top: &'static str,
    body: &'static str,
    read: &'static str,
}

const SINK: &str = "fn sink(x: {T}):\n    print(\"ok\")\n";

const SITES: [Site; 13] = [
    Site {
        name: "untyped in a fn",
        verdict: Verdict::Reject,
        top: "",
        body: "    v := {E}\n",
        read: "    match v:\n        ?n: print(n + 1)\n        None: print(\"none\")\n",
    },
    Site {
        name: "untyped at top level",
        verdict: Verdict::Reject,
        top: "v := {E}\n",
        body: "",
        read: "match v:\n    ?n: print(n + 1)\n    None: print(\"none\")\n",
    },
    Site {
        name: "untyped in a loop",
        verdict: Verdict::Reject,
        top: "",
        body: "    for i in 0..1:\n        v := {E}\n",
        read: "        match v:\n            ?n: print(n + 1)\n            None: print(\"none\")\n",
    },
    Site {
        name: "untyped closure body",
        verdict: Verdict::Reject,
        top: "",
        body: "    g := fn(): {E}\n",
        read: "    match g():\n        ?n: print(n + 1)\n        None: print(\"none\")\n",
    },
    Site {
        name: "untyped generic result",
        verdict: Verdict::Reject,
        top: "",
        body: "    v := id({E})\n",
        read: "    match v:\n        ?n: print(n + 1)\n        None: print(\"none\")\n",
    },
    Site {
        name: "typed binding",
        verdict: Verdict::Sink,
        top: "",
        body: "    v: {T} = {E}\n    sink(v)\n",
        read: "",
    },
    Site {
        name: "typed argument",
        verdict: Verdict::Sink,
        top: "",
        body: "    sink({E})\n",
        read: "",
    },
    Site {
        name: "typed return",
        verdict: Verdict::Sink,
        top: "fn mk() -> {T}:\n    return {E}\n",
        body: "    sink(mk())\n",
        read: "",
    },
    Site {
        name: "field slot",
        verdict: Verdict::Sink,
        top: "struct H:\n    f: {T}\n",
        body: "    h := H({E})\n    h.f = {E}\n    sink(h.f)\n",
        read: "",
    },
    Site {
        name: "element slot",
        verdict: Verdict::Sink,
        top: "",
        body: "    hs: List[{T}] = [{E}]\n    hs[0] = {E}\n    sink(hs[0])\n",
        read: "",
    },
    Site {
        name: "default param",
        verdict: Verdict::Sink,
        top: "fn d(x: {T} = {E}):\n    sink(x)\n",
        body: "    d()\n",
        read: "",
    },
    Site {
        name: "typed closure",
        verdict: Verdict::Sink,
        top: "",
        body: "    g := fn() -> {T}: {E}\n    sink(g())\n",
        read: "",
    },
    Site {
        name: "consumed",
        verdict: Verdict::Print,
        top: "",
        body: "    print({E})\n",
        read: "",
    },
];

fn program(top: &str, body: &str) -> String {
    if body.is_empty() {
        format!("{PRELUDE}{top}")
    } else {
        format!("{PRELUDE}{top}fn main():\n{body}main()\n")
    }
}

fn cell(name: String, src: String, expect: Expect) -> Cell {
    Cell {
        name,
        files: vec![("main.chz".to_string(), src)],
        expect,
    }
}

fn grid_cells(out: &mut Vec<Cell>) {
    for (label, e, t, shown) in FORMS {
        for s in &SITES {
            if label == "comprehension" && s.name == "default param" {
                continue;
            }
            let fill = |x: &str| x.replace("{E}", e).replace("{T}", t);
            let is_opt = label == "?5";
            let mut top = fill(s.top);
            let mut body = fill(s.body);
            if s.verdict == Verdict::Sink {
                top = format!("{}{top}", fill(SINK));
            }
            let expect = match s.verdict {
                Verdict::Reject if is_opt => {
                    if body.is_empty() {
                        top.push_str(s.read);
                    } else {
                        body.push_str(s.read);
                    }
                    Expect::Prints("6".to_string())
                }
                Verdict::Reject if label == "ident" => Expect::Rejects("ident["),
                // A free closure's return is a signature: its own owner rejects the hole.
                Verdict::Reject if s.name == "untyped closure body" => {
                    Expect::Rejects("cannot infer return type of '<closure>'")
                }
                Verdict::Reject => Expect::Rejects(REJECT),
                Verdict::Sink => Expect::Prints("ok".to_string()),
                Verdict::Print if label == "ident" => Expect::Rejects("ident["),
                Verdict::Print => Expect::Prints(shown.to_string()),
            };
            out.push(cell(
                format!("{label} / {}", s.name),
                program(&top, &body),
                expect,
            ));
        }
    }
}

/// The wave-22 Family A repros, verbatim, with the number of open bindings each creates.
const FAMILY_A: [(&str, usize, &str); 19] = [
    (
        "f1",
        1,
        "fn main():\n    z := None\n    g := fn() -> str?: z\n    z = 7\n    match g():\n        ?s: print(s.upper())\n        None: print(\"none\")\nmain()\n",
    ),
    (
        "f1b",
        1,
        "fn main():\n    xs := []\n    fn g() -> List[str]: xs\n    xs.push(7)\n    print(g()[0].upper())\nmain()\n",
    ),
    (
        "f2",
        1,
        "fn main():\n    z := None\n    acc: List[int] = []\n    for _ in 0..2:\n        match z:\n            ?s: acc.push(s)\n            None: print(\"none\")\n        z = \"str\"\n    print(acc)\n    print(acc[0] + 1)\nmain()\n",
    ),
    (
        "f2b",
        1,
        "g := None\nfn show() -> str:\n    return match g:\n        ?s: s.upper()\n        None: \"none\"\nfn seta():\n    g = 7\nseta()\nprint(show())\n",
    ),
    (
        "f2c",
        1,
        "g := None\nfn seta():\n    g = 7\nfn show() -> str:\n    return match g:\n        ?s: s.upper()\n        None: \"none\"\nseta()\nprint(show())\n",
    ),
    (
        "f2d",
        1,
        "fn main():\n    xs := []\n    for _ in 0..2:\n        if xs.len() > 0:\n            print(xs[0].upper())\n        xs.push(7)\nmain()\n",
    ),
    (
        "f3",
        1,
        "fn id[T](x: T) -> T:\n    return x\nfn main():\n    xs := [None]\n    ys := id(xs)\n    ys.push(\"s\")\n    xs.push(7)\n    total := 0\n    for x in xs:\n        match x:\n            ?v: total += v\n            None: pass\n    print(total)\nmain()\n",
    ),
    (
        "f3b",
        1,
        "fn main():\n    xs := []\n    ys := [xs]\n    ys[0].push(\"s\")\n    xs.push(7)\n    zs: List[int] = xs\n    print(zs)\n    print(zs[0] + 1)\nmain()\n",
    ),
    (
        "f3s",
        1,
        "fn show(x: int?) -> str:\n    return match x:\n        ?v: \"int {v}\"\n        None: \"none\"\nfn main():\n    xs := [None]\n    ys := [xs]\n    ys[0].push(\"s\")\n    xs.push(7)\n    for x in xs:\n        print(show(x))\nmain()\n",
    ),
    (
        "f4",
        1,
        "struct Cell[T]:\n    v: T?\nfn main():\n    c := Cell(None)\n    a: Cell[int] = c\n    b: Cell[str] = c\n    a.v = 5\n    match b.v:\n        ?s: print(s.upper())\n        None: print(\"none\")\nmain()\n",
    ),
    (
        "f4b",
        1,
        "fn fill(c: (List[str], int)):\n    c.0.push(\"s\")\nfn first(c: (List[int], int)) -> int:\n    return c.0[0] + 1\nfn main():\n    c := ([], 1)\n    fill(c)\n    print(first(c))\nmain()\n",
    ),
    (
        "p5",
        1,
        "struct Box[T]:\n    items: List[T]\n    fn new() -> Box[T]:\n        return Box([])\n    fn of(x: T) -> Box[T]:\n        return Box([x])\n    fn add(self, x: T):\n        self.items.push(x)\nfn main():\n    v := Box.new()\n    v.add(1)\n    xs := [Box.new(), Box.of(1)]\n    print(v.items, xs.len())\nmain()\n",
    ),
    (
        "p5b",
        1,
        "struct Box[T]:\n    items: List[T]\n    fn new() -> Box[T]:\n        return Box([])\nfn main():\n    v := Box.new()\n    print(\"accepted\")\nmain()\n",
    ),
    (
        "h7a",
        1,
        "h := [None]\nfn r() -> str:\n    return h[1] ?? \"e\"\nh.push(2)\nx: str = r()\nprint(x)\nprint(x.upper())\n",
    ),
    (
        "h7b",
        1,
        "fn main():\n    h := [None]\n    f := fn() -> str: h[1] ?? \"e\"\n    h.push(2)\n    x: str = f()\n    print(x)\n    print(x + \"!\")\nmain()\n",
    ),
    (
        "h7c",
        1,
        "fn main():\n    h := [None]\n    for i in 0..2:\n        x: str = h[i] ?? \"e\"\n        print(x, x.len())\n        h.push(7)\nmain()\n",
    ),
    (
        "h7d",
        1,
        "fn main():\n    z := None\n    for i in 0..2:\n        x: str = z ?? \"e\"\n        print(x, x.len())\n        z = 7\nmain()\n",
    ),
    (
        "h7e",
        2,
        "struct B[T]:\n    v: T\nfn main():\n    b := B(None)\n    f := fn() -> str: b.v ?? \"d\"\n    b.v = 1\n    print(f().len())\n    m := {\"a\": None}\n    g := fn() -> str: m[\"a\"] ?? \"d\"\n    m[\"a\"] = 2.5\n    print(g().upper())\nmain()\n",
    ),
    (
        "h7f",
        1,
        "fn main():\n    h := []\n    f := fn() -> str: h[0]\n    h.push(2)\n    print(f().upper())\nmain()\n",
    ),
];

const APPLY: &str = "fn apply[T](f: fn(T) -> T, x: T) -> T:\n    return f(x)\n";
const FOLD: &str = "fn fold[T, A](xs: List[T], init: A, f: fn(A, T) -> A) -> A:\n    acc := init\n    for x in xs:\n        acc = f(acc, x)\n    return acc\n";
const TAG: &str = "fn tag[U](xs: List[U]) -> List[U]:\n    return xs\n";

fn in_main(top: &str, body: &str) -> String {
    format!("{top}fn main():\n{body}main()\n")
}

fn single_cells(out: &mut Vec<Cell>) {
    let p = |s: &str| Expect::Prints(s.to_string());
    for (name, _, src) in FAMILY_A {
        out.push(cell(
            format!("family A {name}"),
            src.to_string(),
            Expect::Rejects(REJECT),
        ));
    }
    let mut add =
        |name: &str, src: String, expect: Expect| out.push(cell(name.to_string(), src, expect));
    add(
        "(b) struct ctor with a closure",
        in_main(
            "struct M[A, B]:\n    a: A\n    f: fn(A) -> B\nfn sink(x: M[int, int]):\n    print(\"ok\")\n",
            "    m := M(1, fn(x): x * 2)\n    sink(m)\n",
        ),
        p("ok"),
    );
    add(
        "(c) closure in a bare T slot",
        in_main(
            "fn store[T](x: T) -> T:\n    return x\n",
            "    f := store(fn(a: int): a + 1)\n    print(f(2))\n",
        ),
        p("3"),
    );
    add(
        "(d) closure param typed by an open argument",
        in_main(APPLY, "    ys := apply(fn(a): a, [])\n    ys.push(\"s\")\n"),
        Expect::Rejects("cannot infer the element type of `a`"),
    );
    add(
        "(e) fold over an open accumulator",
        in_main(
            FOLD,
            "    s := fold([1, 2], [], fn(acc, x): acc + [x])\n    print(s)\n",
        ),
        Expect::Rejects("cannot infer the element type of `acc`"),
    );
    add(
        "(e) fold into a typed binding",
        in_main(
            FOLD,
            "    r: List[int] = fold([1, 2], [], fn(acc, x): acc + [x])\n    print(r)\n",
        ),
        p("[1, 2]"),
    );
    add(
        "(f) a lambda param does not close the statement frame",
        in_main(
            "fn mk() -> int!:\n    return 2\nfn pick(f: fn(int) -> int!) -> int!:\n    return f(1)\n",
            "    r := mk()\n    print(?2 == pick(fn(x: int): r))\n",
        ),
        p("true"),
    );
    add(
        "(g) match ?5",
        in_main(
            "",
            "    match ?5:\n        ?n: print(n + 1)\n        None: print(\"none\")\n",
        ),
        p("6"),
    );
    add("(g) ?5 ?? 1", in_main("", "    print(?5 ?? 1)\n"), p("5"));
    add(
        "(g) (?P(3))?.f",
        in_main("struct P:\n    f: int\n", "    print((?P(3))?.f)\n"),
        p("3"),
    );
    add(
        "(g) ?7 else",
        in_main("", "    w := ?7 else:\n        return\n    print(w)\n"),
        p("7"),
    );
    add(
        "(g) for x in [?5]",
        in_main("", "    for x in [?5]:\n        print(x ?? 0)\n"),
        p("5"),
    );
    add(
        "(l) free generic call binds nothing",
        in_main(TAG, "    xs := tag([])\n    print(xs)\n"),
        Expect::Rejects(REJECT),
    );
    add(
        "(l) free generic call consumed or typed",
        in_main(
            TAG,
            "    print(tag([]))\n    ys: List[int] = tag([])\n    print(ys)\n",
        ),
        p("[]\n[]"),
    );
    // No message prints a hole as a type: a pending `?x` reads as its default `T?`.
    add(
        "(p) None into an int",
        in_main("", "    a: int = None\n    print(a)\n"),
        Expect::Rejects("cannot assign None to variable of type int"),
    );
    add(
        "(p) 1 + ?5",
        in_main("", "    print(1 + ?5)\n"),
        Expect::Rejects("cannot apply + to int and int?"),
    );
    add(
        "(p) (?5).foo()",
        in_main("", "    print((?5).foo())\n"),
        Expect::Rejects("type int? has no method 'foo'"),
    );
    add(
        "(p) if ?5",
        in_main("", "    if ?5:\n        print(1)\n"),
        Expect::Rejects("if condition must be bool, found int?"),
    );
    add(
        "(n) None ?? 1",
        in_main("", "    print(None ?? 1)\n"),
        p("1"),
    );
    add(
        "(n) [None][0] ?? 2",
        in_main("", "    print([None][0] ?? 2)\n"),
        p("2"),
    );
}

/// (name, program, error count, names every one of which some error must hold in backticks).
fn counted_cells() -> Vec<(String, String, usize, Vec<&'static str>)> {
    let mut out: Vec<(String, String, usize, Vec<&'static str>)> = Vec::new();
    for (name, n, src) in FAMILY_A {
        out.push((format!("family A {name}"), src.to_string(), n, vec![]));
    }
    let cascade = "xs := []\nys := [xs]\nxs.push(1)\n";
    out.push((
        "(h) cascade in a fn".into(),
        in_main("", "    xs := []\n    ys := [xs]\n    xs.push(1)\n"),
        1,
        vec!["xs"],
    ));
    out.push((
        "(h) cascade at top level".into(),
        cascade.to_string(),
        1,
        vec!["xs"],
    ));
    out.push((
        "(i) generic fn value".into(),
        in_main(
            "fn ident[T](x: T) -> T:\n    return x\n",
            "    g := ident\n    print(g(5))\n",
        ),
        1,
        vec![],
    ));
    out.push((
        "(j) a nested read does not silence the outer binding".into(),
        in_main(
            "",
            "    r := recover:\n        for e in []:\n            print(e)\n        []\n    print(r)\n",
        ),
        1,
        vec!["r"],
    ));
    out.push((
        "(j) a nested poisoned read".into(),
        in_main(
            "",
            "    xs := []\n    r := recover:\n        print(xs)\n        []\n    print(r)\n",
        ),
        2,
        vec!["xs", "r"],
    ));
    // A poisoned read at nesting depth 2..4 below the statement that binds `r`: the read never
    // counts for `r`'s statement, at any depth.
    for (depth, body) in [
        (2, "        if true:\n            print(xs)\n"),
        (
            3,
            "        for i in 0..1:\n            if i == 0:\n                print(xs)\n",
        ),
        (
            4,
            "        for i in 0..1:\n            if i == 0:\n                if true:\n                    print(xs)\n",
        ),
    ] {
        out.push((
            format!("(j) a nested poisoned read at depth {depth}"),
            in_main(
                "",
                &format!("    xs := []\n    r := recover:\n{body}        []\n    print(r)\n"),
            ),
            2,
            vec!["xs", "r"],
        ));
    }
    for (label, body) in [
        ("directly", "            print(xs)\n"),
        (
            "under an if",
            "            if true:\n                print(xs)\n",
        ),
    ] {
        out.push((
            format!("(j) a poisoned read {label} in a nested fn body"),
            in_main(
                "",
                &format!(
                    "    xs := []\n    r := recover:\n        fn g() -> int:\n{body}            return 1\n        print(g())\n        []\n    print(r)\n"
                ),
            ),
            2,
            vec!["xs", "r"],
        ));
    }
    out.push((
        "(j) recover inside if inside for".into(),
        in_main(
            "",
            "    xs := []\n    for i in 0..1:\n        if i == 0:\n            r := recover:\n                print(xs)\n                []\n            print(r)\n",
        ),
        2,
        vec!["xs", "r"],
    ));
    out.push((
        "(k) uses of a rejected binding do not cascade".into(),
        in_main(
            "",
            "    xs := []\n    t := xs\n    us := [t]\n    match us:\n        (p, q): print(p)\n    print(us[\"k\"])\n    zs := []\n    print(zs)\n",
        ),
        2,
        vec!["xs", "zs"],
    ));
    out.push((
        "(m) inline-expr fn body".into(),
        format!("{APPLY}fn f() -> int: apply(fn(a): 1, []).len()\nprint(f())\n"),
        1,
        vec!["a"],
    ));
    out.push((
        "(o) top-level None".into(),
        "g := None\nprint(1)\n".into(),
        1,
        vec!["g"],
    ));
    out.push((
        "(o) top-level [None] read by a fn below".into(),
        "h := [None]\nfn r() -> str:\n    return h[1] ?? \"e\"\nprint(r())\n".into(),
        1,
        vec!["h"],
    ));
    out
}

/// The text of each `error` object of `chezzi check --errors=json`, from its message on.
fn check_errors(dir: &std::path::Path, src: &str) -> Vec<String> {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("main.chz"), src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["check", "--errors=json", "main.chz"])
        .current_dir(dir)
        .output()
        .expect("spawn chezzi");
    let text = String::from_utf8_lossy(&out.stdout);
    let key = r#""severity":"error","message":""#;
    text.split(key).skip(1).map(str::to_string).collect()
}

#[test]
fn open_binding_grid() {
    let root = std::env::temp_dir().join(format!("chezzi-open-binding-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut cells = Vec::new();
    grid_cells(&mut cells);
    single_cells(&mut cells);
    let mut fails = Vec::new();
    for (i, c) in cells.iter().enumerate() {
        if let Err(e) = run_cell(&root, i, c) {
            fails.push(e);
        }
        // No reject cell describes a hole with an internal token.
        if matches!(c.expect, Expect::Rejects(_)) {
            let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
                .args(["check", "main.chz"])
                .current_dir(root.join(format!("c{i}")))
                .output()
                .expect("spawn chezzi");
            let err = String::from_utf8_lossy(&out.stderr);
            if err.contains('\u{E000}') || err.contains("<unknown>") || err.contains("found _") {
                fails.push(format!("{}: a hole token in {err:?}", c.name));
            }
        }
    }
    let counted = counted_cells();
    let total = cells.len() + counted.len();
    for (i, (name, src, want, names)) in counted.into_iter().enumerate() {
        let errs = check_errors(&root.join(format!("n{i}")), &src);
        let named = names
            .iter()
            .all(|n| errs.iter().any(|m| m.contains(&format!("`{n}`"))));
        let all_hole = errs
            .iter()
            .all(|m| m.contains(REJECT) || m.contains("ident["));
        if errs.len() != want || !named || !all_hole {
            fails.push(format!(
                "{name}: want {want} error(s) naming {names:?}; got {errs:?}"
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    let list = fails.join(&String::from(char::from(10)));
    assert!(
        fails.is_empty(),
        "{} of {total} open-binding cells failed: {list}",
        fails.len()
    );
}
