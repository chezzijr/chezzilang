//! TICKET-180: the name-resolution grid. Every binder kind x every name kind it can shadow
//! x every position the name can be read in, one generated program per cell, run through the
//! built `chezzi` binary. The expected value is the CPython / Rust meaning: the innermost
//! binding wins, and a module binds each name once, a type included (owner 2026-10-16), so a
//! same-module `fn S` beside a type `S` is a second binding.
//!
//! The checker decides what a name means and the compiler reads that decision
//! (`checker::ResolutionTable`). A red cell here means the two halves disagree again.

use std::path::{Path, PathBuf};
use std::process::Command;

enum Expect {
    Prints(String),
    /// A declaration the language rejects by a named rule; the fragment is part of the message.
    Rejects(&'static str),
}

struct Cell {
    name: String,
    files: Vec<(String, String)>,
    expect: Expect,
}

/// The module name a declaration binds; a type binds it too (owner 2026-10-16).
#[derive(Clone, Copy, PartialEq)]
enum Binds {
    /// No module name: a builtin, or a name that is a member of a module.
    Nothing,
    /// The name came from an import.
    Import,
    /// The module declares the name itself.
    Own,
}

/// A name the binder shadows, and the declaration that puts it in scope.
struct Kind {
    tag: &'static str,
    name: &'static str,
    decl: &'static str,
    /// The argument the call cell passes (the defaulted-fn kind calls `f(1)`).
    arg: u32,
    /// The module name the declaration binds, if any.
    binds: Binds,
}

const KINDS: &[Kind] = &[
    Kind {
        tag: "builtin_fn",
        name: "ord",
        decl: "",
        arg: 4,
        binds: Binds::Nothing,
    },
    Kind {
        tag: "builtin_ctor",
        name: "Channel",
        decl: "",
        arg: 4,
        binds: Binds::Nothing,
    },
    Kind {
        tag: "std_ctor",
        name: "timer",
        decl: "import std.time\n",
        arg: 4,
        binds: Binds::Nothing,
    },
    Kind {
        tag: "struct",
        name: "P",
        decl: "struct P:\n    x: int\n",
        arg: 4,
        binds: Binds::Own,
    },
    Kind {
        tag: "enum",
        name: "E",
        decl: "enum E:\n    A\n",
        arg: 4,
        binds: Binds::Own,
    },
    Kind {
        tag: "alias",
        name: "Q",
        decl: "struct P:\n    x: int\ntype Q = P\n",
        arg: 4,
        binds: Binds::Own,
    },
    Kind {
        tag: "imported_fn",
        name: "f",
        decl: "import f from lib\n",
        arg: 4,
        binds: Binds::Import,
    },
    Kind {
        tag: "imported_type",
        name: "T",
        decl: "import T from lib\n",
        arg: 4,
        binds: Binds::Import,
    },
    Kind {
        tag: "module",
        name: "math",
        decl: "import std.math\n",
        arg: 4,
        binds: Binds::Import,
    },
    // A defaulted fn: the checker binds its omitted argument (TICKET-182).
    Kind {
        tag: "defaulted_fn",
        name: "f",
        decl: "fn f(a: int, b: int = 10) -> int:\n    return a + b\n",
        arg: 1,
        binds: Binds::Own,
    },
];

const LIB: &str = "struct T:\n    x: int\nfn f(n: int) -> str:\n    return \"L{n}\"\nfn h(n: int) -> str:\n    return \"B{n}\"\n";

const S_DECL: &str =
    "struct S:\n    k: int\n    fn h(self, n: int) -> str:\n        return \"B{n}\"\n";

/// What the binder binds and how the cell reads it.
struct Pos {
    tag: &'static str,
    ty: &'static str,
    ret: &'static str,
    zero: &'static str,
    value: String,
    read: String,
    expect: String,
}

fn positions(k: &Kind) -> Vec<Pos> {
    let n = k.name;
    vec![
        Pos {
            tag: "call",
            ty: "fn(int) -> str",
            ret: "str",
            zero: "\"\"",
            value: "fn(n: int) -> str: \"B{n}\"".into(),
            read: format!("{n}({})", k.arg),
            expect: format!("B{}", k.arg),
        },
        Pos {
            tag: "value",
            ty: "int",
            ret: "int",
            zero: "0",
            value: "7".into(),
            read: n.into(),
            expect: "7".into(),
        },
        Pos {
            tag: "member",
            ty: "S",
            ret: "str",
            zero: "\"\"",
            value: "S(k=4)".into(),
            read: format!("{n}.h(4)"),
            expect: "B4".into(),
        },
    ]
}

/// The enum and `ALL` come from one list, so a new binder is in `ALL`; every decision below is an
/// exhaustive `match`, so the build fails until it decides the variant.
macro_rules! binders {
    ($($v:ident),+ $(,)?) => {
        #[derive(Clone, Copy, PartialEq)]
        enum Binder {
            $($v),+
        }
        impl Binder {
            const ALL: &'static [Binder] = &[$(Binder::$v),+];
        }
    };
}

binders!(
    Param,
    Local,
    ToplevelLet,
    ForVar,
    MatchBinding,
    ClosureParam,
    ComprehensionVar,
    WaitRecv,
    NestedFn,
    ToplevelFn,
    ImportAlias
);

impl Binder {
    fn tag(self) -> &'static str {
        match self {
            Binder::Param => "param",
            Binder::Local => "local",
            Binder::ToplevelLet => "toplevel_let",
            Binder::ForVar => "for_var",
            Binder::MatchBinding => "match_binding",
            Binder::ClosureParam => "closure_param",
            Binder::ComprehensionVar => "comprehension_var",
            Binder::WaitRecv => "wait_recv",
            Binder::NestedFn => "nested_fn",
            Binder::ToplevelFn => "toplevel_fn",
            Binder::ImportAlias => "import_alias",
        }
    }

    /// Does the binder declare a name of the MODULE (a top-level let, fn or import alias)? The
    /// other eight bind in a scope below it.
    fn binds_module_name(self) -> bool {
        match self {
            Binder::ToplevelLet | Binder::ToplevelFn | Binder::ImportAlias => true,
            Binder::Param
            | Binder::Local
            | Binder::ForVar
            | Binder::MatchBinding
            | Binder::ClosureParam
            | Binder::ComprehensionVar
            | Binder::WaitRecv
            | Binder::NestedFn => false,
        }
    }
}

/// The cell's expected rejection, from the one fact both sides carry: does the binder declare a
/// module name, and does the name already hold one. A module binds each name once.
fn rejection(binder: Binder, prior: Binds) -> Option<&'static str> {
    match (binder.binds_module_name(), prior) {
        (false, _) | (true, Binds::Nothing) => None,
        (true, Binds::Import) => Some("is already imported"),
        (true, Binds::Own) => Some("is already defined"),
    }
}

/// The binder kinds that bind a VALUE; `None` when the binder cannot bind this position.
fn bind(binder: Binder, n: &str, p: &Pos) -> Option<String> {
    let (ty, ret, zero, e, r) = (p.ty, p.ret, p.zero, &p.value, &p.read);
    match binder {
        Binder::Param => Some(format!(
            "fn g({n}: {ty}) -> {ret}:\n    return {r}\nprint(g({e}))\n"
        )),
        Binder::Local => Some(format!(
            "fn g() -> {ret}:\n    {n} := {e}\n    return {r}\nprint(g())\n"
        )),
        Binder::ToplevelLet => Some(format!("{n} := {e}\nprint({r})\n")),
        Binder::ForVar => Some(format!("for {n} in [{e}]:\n    print({r})\n")),
        Binder::MatchBinding => Some(format!(
            "match ?{e}:\n    ?{n}: print({r})\n    None: print(\"none\")\n"
        )),
        Binder::ClosureParam => Some(format!("print((fn({n}: {ty}) -> {ret}: {r})({e}))\n")),
        Binder::ComprehensionVar => Some(format!("print([{r} for {n} in [{e}]][0])\n")),
        Binder::WaitRecv => Some(format!(
            "fn g() -> {ret}:\n    ch := Channel[{ty}](1)\n    ch.send({e})\n    out := {zero}\n    wait:\n        {n} := ch.recv():\n            {n} := {n} else: panic(\"ch closed\")\n            out = {r}\n    return out\nprint(g())\n"
        )),
        Binder::NestedFn => (p.tag == "call").then(|| {
            format!(
                "fn g() -> str:\n    fn {n}(n: int) -> str:\n        return \"B{{n}}\"\n    return {r}\nprint(g())\n"
            )
        }),
        Binder::ToplevelFn => (p.tag == "call").then(|| {
            format!("fn {n}(n: int) -> str:\n    return \"B{{n}}\"\nprint({r})\n")
        }),
        Binder::ImportAlias => {
            (p.tag == "member").then(|| format!("import lib as {n}\nprint({r})\n"))
        }
    }
}

/// Cells the language rejects by a named declaration rule, with the rule.
fn named_rejection(b: Binder, k: &Kind) -> Option<&'static str> {
    rejection(b, k.binds)
}

fn grid() -> Vec<Cell> {
    let mut cells = Vec::new();
    for k in KINDS {
        for p in positions(k) {
            for &binder in Binder::ALL {
                let Some(body) = bind(binder, k.name, &p) else {
                    continue;
                };
                let src = format!("{}{S_DECL}{body}", k.decl);
                let expect = match named_rejection(binder, k) {
                    Some(frag) => Expect::Rejects(frag),
                    None => Expect::Prints(p.expect.clone()),
                };
                cells.push(Cell {
                    name: format!("{}/{}/{}", binder.tag(), k.tag, p.tag),
                    files: vec![("main.chz".into(), src), ("lib.chz".into(), LIB.into())],
                    expect,
                });
            }
        }
        // Pattern head: a bare catch-all arm named like the declaration binds the scrutinee.
        cells.push(Cell {
            name: format!("match_arm/{}/pattern", k.tag),
            files: vec![
                (
                    "main.chz".into(),
                    format!("{}match 5:\n    {n}: print({n})\n", k.decl, n = k.name),
                ),
                ("lib.chz".into(), LIB.into()),
            ],
            expect: Expect::Prints("5".into()),
        });
    }
    cells.extend(same_module_fn_cells());
    cells.extend(p2_cells());
    cells.extend(field_cells());
    cells.extend(order_cells());
    cells.extend(typed_use_cells());
    cells.extend(head_shape_cells());
    cells.extend(t187_cells());
    cells
}

const SHAPES_LIB: &str = r#"struct T:
    x: int
enum E:
    A
    B(int)
fn f(n: int) -> str:
    return "L{n}"
fn g[U](v: U) -> U:
    return v
struct Box[V]:
    v: V
    fn of(v: V) -> Box[V]:
        return Box(v)
fn say(n: int):
    print("D{n}")
"#;

const SHAPES_MAIN: &str = r#"import lib
import std.json
struct S:
    k: int
    fn make(k: int) -> S:
        return S(k)
    fn shout(n: int):
        print("S{n}")
    fn get(self) -> int:
        return self.k
enum C:
    Red
    Val(int)
struct Box[V]:
    v: V
    fn of(v: V) -> Box[V]:
        return Box(v)
    fn pick[W](v: V, w: W) -> W:
        return w
struct Pair[A, B]:
    a: A
    b: B
    fn of(a: A, b: B) -> Pair[A, B]:
        return Pair(a, b)
fn run_defer():
    defer lib.say(21)
    defer S.shout(22)
    print("body")
fn h(n: int) -> int:
    return n + 1
fn id[U](v: U) -> U:
    return v
print(h(1))
print(lib.f(2))
print(lib.T(3))
print(S.make(4))
print(C.Red)
print(C.Val(5))
print(lib.E.A)
print(lib.E.B(6))
print(Box[int].of(7))
print(Pair[int, str].of(8, "p"))
s := S(9)
print(s.get())
print(lib.g[int](10))
print(Box[int].pick[str](11, "w"))
print(S.make(12).k)
xs := [S(13)]
print(xs[0].k)
fi := id[int]
print(fi(14))
lf := lib.f
print(lf(15))
m := match C.Val(18):
    C.Val(n): n
    C.Red: 0
print(m)
n2 := match lib.E.B(19):
    lib.E.B(n): n
    _: 0
print(n2)
r := json.decode[lib.T]("{{\"x\": 20}}")
print(r)
run_defer()
parallel:
    spawn lib.say(23)
"#;

const SHAPES_OUT: &str = "2\nL2\nT(x=3)\nS(k=4)\nRed\nVal(5)\nA\nB(6)\nBox(v=7)\nPair(a=8, b='p')\n9\n10\nw\n12\n13\n14\nL15\n18\n19\nT(x=20)\nbody\nS22\nD21\nD23";

/// One program with every head position the compiler lowers as a name. A node the compiler reads
/// without a checker record turns it red with `internal: no name resolution recorded`.
fn head_shape_cells() -> Vec<Cell> {
    vec![with_lib(
        "shapes/every_head_position",
        SHAPES_MAIN,
        SHAPES_LIB,
        Expect::Prints(SHAPES_OUT.into()),
    )]
}

fn one(name: &str, main: String, expect: Expect) -> Cell {
    Cell {
        name: name.into(),
        files: vec![("main.chz".into(), main)],
        expect,
    }
}

fn with_lib(name: &str, main: &str, lib: &str, expect: Expect) -> Cell {
    Cell {
        name: name.into(),
        files: vec![
            ("main.chz".into(), main.into()),
            ("lib.chz".into(), lib.into()),
        ],
        expect,
    }
}

/// A same-module `fn S` beside a type `S` is a second binding (owner 2026-10-16).
fn same_module_fn_cells() -> Vec<Cell> {
    let p = "struct P:\n    x: int\n";
    let bn = "(n: int) -> str:\n    return \"B{n}\"\n";
    vec![
        one(
            "toplevel_fn/struct/raw_ctor_in_own_body",
            format!("{p}fn P(n: int) -> P:\n    return P(n)\nprint(P(4))\n"),
            Expect::Rejects("is already defined"),
        ),
        one(
            "toplevel_fn/struct/value_read_in_own_body",
            format!(
                "{p}fn P(x: int) -> P:\n    if x > 100:\n        return P(x=x)\n    f := P\n    return f(x + 100)\nprint(P(4))\n"
            ),
            Expect::Rejects("is already defined"),
        ),
        // A variant lives under its enum, so a top-level `fn` may share its name; the bare call
        // is the fn, as the bare read is (Rust: `fn Circle` beside `Shape::Circle` prints `1`).
        one(
            "toplevel_fn/variant_name/call",
            "enum Shape:\n    Circle(int)\nfn Circle() -> int:\n    return 1\nprint(Circle(), Shape.Circle(2))\n".into(),
            Expect::Prints("1 Circle(2)".into()),
        ),
        // `None` is a value, not a constructor (Rust E0618 `expected function, found Option<_>`).
        one(
            "builtin/none/called",
            "x := None(1)\n".into(),
            Expect::Rejects("is not callable"),
        ),
        one(
            "toplevel_fn/alias/k2_recursive_call",
            format!(
                "{p}type Q = P\nfn Q(s: str) -> P:\n    if s == \"\":\n        return P(0)\n    return Q(s[1..])\nprint(Q(\"abc\"))\n"
            ),
            Expect::Rejects("is already defined"),
        ),
        with_lib(
            "importer/struct/qualified_call",
            "import lib\nprint(lib.P(4))\n",
            &format!("{p}fn P{bn}"),
            Expect::Rejects("is already defined"),
        ),
        with_lib(
            "importer/alias/qualified_call",
            "import lib\nprint(lib.Q(4))\n",
            &format!("{p}type Q = P\nfn Q{bn}"),
            Expect::Rejects("is already defined"),
        ),
        with_lib(
            "importer/alias/k3_str_arg",
            "import lib\nprint(lib.Q(\"abc\"))\n",
            &format!("{p}type Q = P\nfn Q(s: str) -> P:\n    return P(s.len())\n"),
            Expect::Rejects("is already defined"),
        ),
        with_lib(
            "importer/alias/k3_int_arg",
            "import lib\nprint(lib.Q(5))\n",
            &format!("{p}type Q = P\nfn Q(s: str) -> P:\n    return P(s.len())\n"),
            Expect::Rejects("is already defined"),
        ),
    ]
}

fn p2_cells() -> Vec<Cell> {
    let b = "struct P:\n    x: int\n".to_string();
    vec![
        Cell {
            name: "p2/alias_of_ambiguous_module_type".into(),
            files: vec![
                (
                    "main.chz".into(),
                    "import x.b\nimport y.b\ntype R = b.P\nprint(R(1))\n".into(),
                ),
                ("x/b.chz".into(), b.clone()),
                ("y/b.chz".into(), b),
            ],
            expect: Expect::Rejects("'b' is ambiguous"),
        },
        one(
            "p2/generic_alias_turbofish",
            "struct Box[T]:\n    v: T\ntype BB = Box\nprint(BB[int](9))\n".into(),
            Expect::Prints("Box(v=9)".into()),
        ),
    ]
}

const FIELD_LIB: &str = "struct T:\n    x: int\nfn f(n: int) -> str:\n    return \"L{n}\"\nfn h(n: int) -> str:\n    return \"B{n}\"\nenum E:\n    A\nA := 4\nk := 4\n";

const FIELD_DECLS: &str = "struct V:\n    A: int\n    k: int\n    pi: int\nstruct W:\n    E: V\nfn id(v: int) -> int:\n    return v\n";

/// A head a value binder can hide in a value-field read `X.m`.
struct FieldHead {
    tag: &'static str,
    name: &'static str,
    decl: &'static str,
    member: &'static str,
    ty: &'static str,
    value: &'static str,
    /// The module name the head binds.
    binds: Binds,
}

const VV: &str = "V(A=4, k=4, pi=4)";

#[rustfmt::skip]
const FIELD_HEADS: &[FieldHead] = &[
    FieldHead { tag: "struct", name: "P", decl: "struct P:\n    x: int\n", member: "k", ty: "V", value: VV, binds: Binds::Own },
    FieldHead { tag: "enum", name: "E", decl: "enum E:\n    A\n", member: "A", ty: "V", value: VV, binds: Binds::Own },
    FieldHead { tag: "enum_alias", name: "R", decl: "enum E:\n    A\ntype R = E\n", member: "A", ty: "V", value: VV, binds: Binds::Own },
    FieldHead { tag: "alias", name: "Q", decl: "struct P:\n    x: int\ntype Q = P\n", member: "k", ty: "V", value: VV, binds: Binds::Own },
    FieldHead { tag: "imported_type", name: "T", decl: "import T from lib\n", member: "k", ty: "V", value: VV, binds: Binds::Import },
    FieldHead { tag: "module", name: "math", decl: "import std.math\n", member: "pi", ty: "V", value: VV, binds: Binds::Import },
    FieldHead { tag: "qualified_enum", name: "lib", decl: "import lib\n", member: "E.A", ty: "W", value: "W(E=V(A=4, k=4, pi=4))", binds: Binds::Import },
];

/// The read `r` in one context, every line indented `ind` spaces.
fn field_block(r: &str, ctx: &str, ind: usize) -> String {
    let lines: Vec<String> = match ctx {
        "read" => vec![format!("print({r})")],
        "arg" => vec![format!("print(id({r}))")],
        "scrutinee" => vec![
            format!("m := match {r}:"),
            "    v: v".into(),
            "print(m)".into(),
        ],
        _ => vec![
            "fn ret() -> int:".into(),
            format!("    return {r}"),
            "print(ret())".into(),
        ],
    };
    let pad = " ".repeat(ind);
    lines.iter().map(|l| format!("{pad}{l}\n")).collect()
}

fn field_bind(binder: Binder, n: &str, ty: &str, e: &str, r: &str, ctx: &str) -> Option<String> {
    let b = |ind| field_block(r, ctx, ind);
    // A closure body is one expression: only the `read` and `arg` contexts fit.
    let one_expr = || match ctx {
        "read" => Some(r.to_string()),
        "arg" => Some(format!("id({r})")),
        _ => None,
    };
    match binder {
        Binder::Param => Some(format!("fn g({n}: {ty}):\n{}g({e})\n", b(4))),
        Binder::Local => Some(format!("fn g():\n    {n} := {e}\n{}g()\n", b(4))),
        Binder::ToplevelLet => Some(format!("{n} := {e}\n{}", b(0))),
        Binder::ForVar => Some(format!("for {n} in [{e}]:\n{}", b(4))),
        Binder::MatchBinding => Some(format!(
            "match ?{e}:\n    ?{n}:\n{}    None: print(\"none\")\n",
            b(8)
        )),
        Binder::WaitRecv => Some(format!(
            "fn g():\n    ch := Channel[{ty}](1)\n    ch.send({e})\n    wait:\n        {n} := ch.recv():\n            {n} := {n} else: panic(\"ch closed\")\n{}g()\n",
            b(12)
        )),
        Binder::ImportAlias => Some(format!("import lib as {n}\n{}", b(0))),
        Binder::ClosureParam => {
            one_expr().map(|x| format!("print((fn({n}: {ty}) -> int: {x})({e}))\n"))
        }
        Binder::ComprehensionVar => {
            one_expr().map(|x| format!("print([{x} for {n} in [{e}]][0])\n"))
        }
        // A fn is no value binder of a field read.
        Binder::NestedFn | Binder::ToplevelFn => None,
    }
}

fn field_rejection(b: Binder, h: &FieldHead) -> Option<&'static str> {
    rejection(b, h.binds)
}

/// The value-field read position: a value binder named like a type or module, read as `X.m`
/// (not called) in a print, an argument, a match scrutinee and a return.
fn field_cells() -> Vec<Cell> {
    let mut cells = Vec::new();
    for h in FIELD_HEADS {
        for ctx in ["read", "arg", "scrutinee", "return"] {
            for &binder in Binder::ALL {
                let r = format!("{}.{}", h.name, h.member);
                let Some(body) = field_bind(binder, h.name, h.ty, h.value, &r, ctx) else {
                    continue;
                };
                let expect = match field_rejection(binder, h) {
                    Some(frag) => Expect::Rejects(frag),
                    None => Expect::Prints("4".into()),
                };
                cells.push(with_lib(
                    &format!("{}/{}/field_{ctx}", binder.tag(), h.tag),
                    &format!("{}{FIELD_DECLS}{body}", h.decl),
                    FIELD_LIB,
                    expect,
                ));
            }
        }
    }
    cells
}

const ORDER_PRE: &str = "struct V:\n    A: int\n";

const ORDER_LIB: &str = "A := 4\n";

/// A top-level binding named like a builtin, a type or an enum, read in a fn body.
struct OrderHead {
    tag: &'static str,
    binding: &'static str,
    body: &'static [&'static str],
    /// The same head as one expression, for the inline body whose type is inferred.
    inline: &'static str,
    ret: &'static str,
    out: &'static str,
    /// The typed-use cell's `y: <wrong> = g()` and the rejection it expects.
    wrong: &'static str,
    rejects: &'static str,
}

const ORD_BIND: &str = "ord := fn(s: str) -> int: 1000\n";
const P_BIND: &str = "p := fn(n: int) -> str: \"v{n}\"\n";
const INT_AS_STR: &str = "cannot assign int to variable of type str";
const STR_AS_INT: &str = "cannot assign str to variable of type int";

#[rustfmt::skip]
const ORDER_HEADS: &[OrderHead] = &[
    OrderHead { tag: "ord_call", binding: ORD_BIND, body: &["return ord(\"a\")"], inline: "ord(\"a\")", ret: "int", out: "1000", wrong: "str", rejects: INT_AS_STR },
    OrderHead { tag: "ord_read", binding: ORD_BIND, body: &["h := ord", "return h(\"a\")"], inline: "[ord][0](\"a\")", ret: "int", out: "1000", wrong: "str", rejects: INT_AS_STR },
    OrderHead { tag: "p_call", binding: P_BIND, body: &["return p(4)"], inline: "p(4)", ret: "str", out: "v4", wrong: "int", rejects: STR_AS_INT },
    OrderHead { tag: "p_read", binding: P_BIND, body: &["h := p", "return h(4)"], inline: "[p][0](4)", ret: "str", out: "v4", wrong: "int", rejects: STR_AS_INT },
    OrderHead { tag: "e_field", binding: "e := V(4)\n", body: &["return e.A"], inline: "e.A", ret: "int", out: "4", wrong: "str", rejects: INT_AS_STR },
    OrderHead { tag: "x_read", binding: "x := 5\n", body: &["return x"], inline: "x", ret: "int", out: "5", wrong: "str", rejects: INT_AS_STR },
];

/// `fn g` over the head: an inline expression body, whose type is inferred, or an annotated block
/// body.
fn order_fn(h: &OrderHead, annotated: bool) -> String {
    if !annotated {
        return format!("fn g(): {}\n", h.inline);
    }
    let sig = format!("fn g() -> {}:\n", h.ret);
    let body: String = h.body.iter().map(|l| format!("    {l}\n")).collect();
    format!("{sig}{body}")
}

fn order_pair(h: &OrderHead, before: bool, annotated: bool) -> String {
    let f = order_fn(h, annotated);
    if before {
        format!("{ORDER_PRE}{}{f}", h.binding)
    } else {
        format!("{ORDER_PRE}{f}{}", h.binding)
    }
}

/// Walk order (owner notes 06:58Z, 08:31Z): a fn body's head is the top-level binding whether
/// the binding is above or below the fn, and no answer depends on which checker walk decides it.
fn order_cells() -> Vec<Cell> {
    let mut cells = Vec::new();
    for h in ORDER_HEADS {
        for (ann, annotated) in [("inferred", false), ("annotated", true)] {
            for (order, before) in [("before", true), ("after", false)] {
                cells.push(with_lib(
                    &format!("order/{order}/{ann}/{}", h.tag),
                    &format!("{}print(g())\n", order_pair(h, before, annotated)),
                    ORDER_LIB,
                    Expect::Prints(h.out.into()),
                ));
            }
        }
    }
    let p = "struct P:\n    x: int\n";
    let fn_p = "fn P(n: int) -> str:\n    return \"B{n}\"\n";
    let g_p = "fn g(): P(4)\n";
    let e = "enum E:\n    A\n";
    let g_e = "fn g(): E.A\n";
    let imp = "import lib as E\n";
    let raw = [
        (
            "swap/fn_first/call",
            format!("{p}{fn_p}{g_p}print(g())\n"),
            Expect::Rejects("is already defined"),
        ),
        (
            "swap/fn_last/call",
            format!("{p}{g_p}{fn_p}print(g())\n"),
            Expect::Rejects("is already defined"),
        ),
        // A whole-module import and a same-module type may not share a name, in either order;
        // the enum comes first in both, so the error names it.
        (
            "swap/import_first/enum_field",
            format!("{e}{imp}{g_e}print(g())\n"),
            Expect::Rejects("type 'E' is already defined"),
        ),
        (
            "swap/import_last/enum_field",
            format!("{e}{g_e}{imp}print(g())\n"),
            Expect::Rejects("type 'E' is already defined"),
        ),
        // Top-level statements keep lexical order (owner 07:27Z, Q2 (b); CPython).
        (
            "toplevel/builtin_call_above_let",
            format!("print(ord(\"a\"))\n{ORD_BIND}print(ord(\"a\"))\n"),
            Expect::Prints("97\n1000".into()),
        ),
        (
            "toplevel/builtin_read_above_let",
            format!("f := ord\nprint(f(\"a\"))\n{ORD_BIND}print(ord(\"a\"))\n"),
            Expect::Prints("97\n1000".into()),
        ),
        (
            "toplevel/enum_field_above_let",
            format!("enum E:\n    A\n{ORDER_PRE}print(E.A)\nE := V(4)\nprint(E.A)\n"),
            Expect::Rejects("is already defined"),
        ),
        (
            "order/after/closure/ord_call",
            format!("f := fn() -> int: ord(\"a\")\n{ORD_BIND}print(f())\n"),
            Expect::Prints("1000".into()),
        ),
        (
            "order/inferred_return_type_of_a_global",
            "x := \"s\"\nfn f(): x\ny: int = f()\nprint(y)\n".into(),
            Expect::Rejects(STR_AS_INT),
        ),
    ];
    for (name, main, expect) in raw {
        cells.push(with_lib(name, &main, ORDER_LIB, expect));
    }
    cells
}

/// Typed use (plan-validation 08:28Z): an inferred `fn g` returns the global's type in both
/// orders, so a wrong annotation at the use site is rejected with the exact type.
fn typed_use_cells() -> Vec<Cell> {
    let mut cells = Vec::new();
    for h in ORDER_HEADS {
        for (order, before) in [("before", true), ("after", false)] {
            cells.push(with_lib(
                &format!("typed_use/{order}/{}", h.tag),
                &format!(
                    "{}y: {} = g()\nprint(y)\n",
                    order_pair(h, before, false),
                    h.wrong
                ),
                ORDER_LIB,
                Expect::Rejects(h.rejects),
            ));
        }
    }
    cells
}

fn files(name: &str, fs: &[(&str, &str)], expect: Expect) -> Cell {
    Cell {
        name: name.into(),
        files: fs
            .iter()
            .map(|(p, s)| (p.to_string(), s.to_string()))
            .collect(),
        expect,
    }
}

/// TICKET-187: type position (bound through a local or qualified alias, from-import of an alias
/// with a fn twin), generic fn value (qualified and from-import; same-named and differently-named
/// caller `T`; pinned by annotation; HOF argument), and `.decode[T]` on every receiver kind.
fn t187_cells() -> Vec<Cell> {
    let named = "protocol Named:\n    fn name(self) -> str\n";
    let show = "struct A:\n    fn name(self) -> str:\n        return \"a\"\nfn show[T: N](x: T) -> str:\n    return x.name()\nprint(show(A()))\n";
    let pick = |tp: &str, f: &str| {
        format!(
            "import std.cmp\nstruct P:\n    x: int\nfn pick[{tp}](a: {tp}, b: {tp}) -> {tp}:\n    f := {f}\n    return f(a, b)\nprint(pick(P(1), P(2)).x)\n"
        )
    };
    let user_decode =
        "struct S:\n    fn decode[T](self, s: str) -> str:\n        return \"user\"\n";
    vec![
        one(
            "t187/bound/local_alias",
            format!("{named}type N = Named\n{show}"),
            Expect::Prints("a".into()),
        ),
        files(
            "t187/bound/qualified_alias",
            &[
                (
                    "main.chz",
                    &format!("import named\ntype N = named.Named\n{show}"),
                ),
                ("named.chz", named),
            ],
            Expect::Prints("a".into()),
        ),
        with_lib(
            "t187/from_import/alias_ctor",
            "import Q from lib\nq: Q = Q(s=\"ab\")\nprint(q.s.len())\n",
            "struct P:\n    s: str\ntype Q = P\n",
            Expect::Prints("2".into()),
        ),
        one(
            "t187/generic_value/qualified/same_name",
            pick("T", "cmp.max"),
            // TICKET-238: the read is rejected where it is bound; no later call pins it.
            Expect::Rejects("is generic and T is not determined here"),
        ),
        one(
            "t187/generic_value/qualified/other_name",
            pick("U", "cmp.max"),
            Expect::Rejects("is generic and T is not determined here"),
        ),
        one(
            "t187/generic_value/from_import/same_name",
            pick("T", "max").replace("import std.cmp", "import max from std.cmp"),
            Expect::Rejects("is generic and T is not determined here"),
        ),
        one(
            "t187/generic_value/qualified/pinned",
            "import std.cmp\ng: fn(int, int) -> int = cmp.max\nprint(g(3, 4))\n".into(),
            Expect::Prints("4".into()),
        ),
        one(
            "t187/generic_value/from_import/pinned",
            "import max from std.cmp\ng: fn(int, int) -> int = max\nprint(g(3, 4))\n".into(),
            Expect::Prints("4".into()),
        ),
        one(
            "t187/generic_value/qualified/hof_arg",
            "import std.cmp\nprint([1, 5, 3].fold(0, cmp.max))\n".into(),
            Expect::Prints("5".into()),
        ),
        one(
            "t187/generic_value/from_import/hof_arg",
            "import max from std.cmp\nprint([1, 5, 3].fold(0, max))\n".into(),
            Expect::Prints("5".into()),
        ),
        one(
            "t187/decode/json_module",
            "import std.json\nprint(json.decode[int](\"7\"))\n".into(),
            Expect::Prints("7".into()),
        ),
        one(
            "t187/decode/json_alias",
            "import std.json as j\nprint(j.decode[List[int]](\"[1]\"))\n".into(),
            Expect::Prints("[1]".into()),
        ),
        one(
            "t187/decode/int_receiver",
            "n := 5\nprint(n.decode[int](\"7\"))\n".into(),
            Expect::Rejects("method 'decode' takes no type argument(s)"),
        ),
        one(
            "t187/decode/user_generic_method",
            format!("{user_decode}print(S().decode[int](\"7\"))\n"),
            Expect::Prints("user".into()),
        ),
        one(
            "t187/decode/local_shadows_json",
            format!(
                "import std.json\n{user_decode}fn main():\n    json := S()\n    print(json.decode[int](\"7\"))\nmain()\n"
            ),
            Expect::Prints("user".into()),
        ),
    ]
}

fn run_cell(root: &Path, idx: usize, c: &Cell) -> Result<(), String> {
    let dir: PathBuf = root.join(format!("c{idx}"));
    for (rel, src) in &c.files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, src).unwrap();
    }
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg("main.chz")
        .current_dir(&dir)
        .output()
        .expect("spawn chezzi");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let ok = match &c.expect {
        Expect::Prints(want) => out.status.success() && stdout.trim_end() == want,
        Expect::Rejects(frag) => !out.status.success() && stderr.contains(frag),
    };
    if ok {
        return Ok(());
    }
    let want = match &c.expect {
        Expect::Prints(w) => format!("prints {w:?}"),
        Expect::Rejects(f) => format!("rejects {f:?}"),
    };
    let first_err = stderr.lines().next().unwrap_or("");
    Err(format!(
        "{}: want {want}; got stdout {:?} stderr {first_err:?}",
        c.name,
        stdout.trim_end()
    ))
}

/// Cells red on the pre-TICKET-187 binary. They must stay red here; when one turns green, remove
/// it from the list.
const PINNED_RED: &[&str] = &[];

#[test]
fn name_resolution_grid() {
    let root = std::env::temp_dir().join(format!("chezzi-name-grid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cells = grid();
    let mut fails = Vec::new();
    for (i, c) in cells.iter().enumerate() {
        let pinned = PINNED_RED.contains(&c.name.as_str());
        match (run_cell(&root, i, c), pinned) {
            (Err(e), false) => fails.push(e),
            (Ok(()), true) => fails.push(format!("{}: green; remove it from PINNED_RED", c.name)),
            _ => {}
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        fails.is_empty(),
        "{} of {} name-resolution cells failed:\n{}",
        fails.len(),
        cells.len(),
        fails.join("\n")
    );
}
