//! TICKET-180: the name-resolution grid. Every binder kind x every name kind it can shadow
//! x every position the name can be read in, one generated program per cell, run through the
//! built `chezzi` binary. The expected value is the CPython / Rust meaning: the innermost
//! binding wins, a same-module `fn S` replaces the ctor of `S` (DEC-029/055/172), and inside
//! `fn S`'s own body `S` is the raw ctor.
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

/// A name the binder shadows, and the declaration that puts it in scope.
struct Kind {
    tag: &'static str,
    name: &'static str,
    decl: &'static str,
    /// The argument the call cell passes (the defaulted-fn kind calls `f(1)`).
    arg: u32,
}

const KINDS: &[Kind] = &[
    Kind {
        tag: "builtin_fn",
        name: "ord",
        decl: "",
        arg: 4,
    },
    Kind {
        tag: "builtin_ctor",
        name: "Channel",
        decl: "",
        arg: 4,
    },
    Kind {
        tag: "std_ctor",
        name: "timer",
        decl: "import std.time\n",
        arg: 4,
    },
    Kind {
        tag: "struct",
        name: "P",
        decl: "struct P:\n    x: int\n",
        arg: 4,
    },
    Kind {
        tag: "enum",
        name: "E",
        decl: "enum E:\n    A\n",
        arg: 4,
    },
    Kind {
        tag: "alias",
        name: "Q",
        decl: "struct P:\n    x: int\ntype Q = P\n",
        arg: 4,
    },
    Kind {
        tag: "imported_fn",
        name: "f",
        decl: "import f from lib\n",
        arg: 4,
    },
    Kind {
        tag: "imported_type",
        name: "T",
        decl: "import T from lib\n",
        arg: 4,
    },
    Kind {
        tag: "module",
        name: "math",
        decl: "import std.math\n",
        arg: 4,
    },
    // A defaulted fn: the checker binds its omitted argument (TICKET-182).
    Kind {
        tag: "defaulted_fn",
        name: "f",
        decl: "fn f(a: int, b: int = 10) -> int:\n    return a + b\n",
        arg: 1,
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

/// The binder kinds that bind a VALUE; `None` when the binder cannot bind this position.
fn bind(binder: &str, n: &str, p: &Pos) -> Option<String> {
    let (ty, ret, zero, e, r) = (p.ty, p.ret, p.zero, &p.value, &p.read);
    Some(match binder {
        "param" => format!("fn g({n}: {ty}) -> {ret}:\n    return {r}\nprint(g({e}))\n"),
        "local" => format!("fn g() -> {ret}:\n    {n} := {e}\n    return {r}\nprint(g())\n"),
        "toplevel_let" => format!("{n} := {e}\nprint({r})\n"),
        "for_var" => format!("for {n} in [{e}]:\n    print({r})\n"),
        "match_binding" => {
            format!("match Some({e}):\n    Some({n}): print({r})\n    None: print(\"none\")\n")
        }
        "closure_param" => format!("print((fn({n}: {ty}) -> {ret}: {r})({e}))\n"),
        "comprehension_var" => format!("print([{r} for {n} in [{e}]][0])\n"),
        "wait_recv" => format!(
            "fn g() -> {ret}:\n    ch := Channel[{ty}](1)\n    ch.send({e})\n    out := {zero}\n    wait:\n        {n} := ch.recv(): out = {r}\n    return out\nprint(g())\n"
        ),
        "nested_fn" if p.tag == "call" => format!(
            "fn g() -> str:\n    fn {n}(n: int) -> str:\n        return \"B{{n}}\"\n    return {r}\nprint(g())\n"
        ),
        "toplevel_fn" if p.tag == "call" => {
            format!("fn {n}(n: int) -> str:\n    return \"B{{n}}\"\nprint({r})\n")
        }
        "import_alias" if p.tag == "member" => format!("import lib as {n}\nprint({r})\n"),
        _ => return None,
    })
}

const BINDERS: &[&str] = &[
    "param",
    "local",
    "toplevel_let",
    "for_var",
    "match_binding",
    "closure_param",
    "comprehension_var",
    "wait_recv",
    "nested_fn",
    "toplevel_fn",
    "import_alias",
];

/// Cells the language rejects by a named declaration rule, with the rule.
fn named_rejection(binder: &str, k: &Kind) -> Option<&'static str> {
    match (binder, k.tag) {
        // `is_reserved_alias_target` (src/checker/mod.rs): an import alias may not take a
        // builtin or reserved-type name.
        ("import_alias", "builtin_fn" | "builtin_ctor" | "std_ctor") => Some("is reserved"),
        // Two imports under one name.
        ("import_alias", "imported_fn" | "imported_type" | "module") => Some("already"),
        // A whole-module import and a same-module `fn` or type under one name (owner decisions
        // 2026-09-29; Go: `redeclared in this block`).
        ("import_alias", "struct" | "enum" | "alias" | "defaulted_fn") => {
            Some("is already imported")
        }
        // A top-level `fn` may not take a builtin / reserved name (`is_reserved_name`).
        ("toplevel_fn", "builtin_fn" | "builtin_ctor" | "std_ctor") => Some("is reserved"),
        // Two top-level declarations of one name.
        ("toplevel_fn", "imported_fn" | "defaulted_fn") => Some("is already defined"),
        // A module global's type is frozen at its first declaration (the import).
        ("toplevel_let", "module") => Some("cannot re-declare module-level binding"),
        // A top-level `fn` over an imported module name: the import and the fn would share one
        // runtime global slot; Go rejects the redeclaration.
        ("toplevel_fn", "module") => Some("is already imported"),
        _ => None,
    }
}

fn grid() -> Vec<Cell> {
    let mut cells = Vec::new();
    for k in KINDS {
        for p in positions(k) {
            for binder in BINDERS {
                let Some(body) = bind(binder, k.name, &p) else {
                    continue;
                };
                let src = format!("{}{S_DECL}{body}", k.decl);
                let expect = match named_rejection(binder, k) {
                    Some(frag) => Expect::Rejects(frag),
                    None => Expect::Prints(p.expect.clone()),
                };
                cells.push(Cell {
                    name: format!("{binder}/{}/{}", k.tag, p.tag),
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

const SHAPES_OUT: &str = "2\nL2\nT(x=3)\nS(k=4)\nRed\nVal(5)\nA\nB(6)\nBox(v=7)\nPair(a=8, b='p')\n9\n10\nw\n12\n13\n14\nL15\n18\n19\nOk(T(x=20))\nbody\nS22\nD21\nD23";

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

/// DEC-029/055/172: a same-module top-level `fn S` over a struct or alias `S`.
fn same_module_fn_cells() -> Vec<Cell> {
    let p = "struct P:\n    x: int\n";
    let bn = "(n: int) -> str:\n    return \"B{n}\"\n";
    vec![
        one(
            "toplevel_fn/struct/raw_ctor_in_own_body",
            format!("{p}fn P(n: int) -> P:\n    return P(n)\nprint(P(4))\n"),
            Expect::Prints("P(x=4)".into()),
        ),
        // K2: the recursive call inside `fn Q` is the fn, not the alias ctor.
        one(
            "toplevel_fn/alias/k2_recursive_call",
            format!(
                "{p}type Q = P\nfn Q(s: str) -> P:\n    if s == \"\":\n        return P(0)\n    return Q(s[1:])\nprint(Q(\"abc\"))\n"
            ),
            Expect::Prints("P(x=0)".into()),
        ),
        with_lib(
            "importer/struct/qualified_call",
            "import lib\nprint(lib.P(4))\n",
            &format!("{p}fn P{bn}"),
            Expect::Prints("B4".into()),
        ),
        with_lib(
            "importer/alias/qualified_call",
            "import lib\nprint(lib.Q(4))\n",
            &format!("{p}type Q = P\nfn Q{bn}"),
            Expect::Prints("B4".into()),
        ),
        // K3: `lib.Q` is `fn Q(s: str)`, so `lib.Q(5)` is a type error (Rust rejects `lib::Q(5)`).
        with_lib(
            "importer/alias/k3_str_arg",
            "import lib\nprint(lib.Q(\"abc\"))\n",
            &format!("{p}type Q = P\nfn Q(s: str) -> P:\n    return P(s.len())\n"),
            Expect::Prints("P(x=3)".into()),
        ),
        with_lib(
            "importer/alias/k3_int_arg",
            "import lib\nprint(lib.Q(5))\n",
            &format!("{p}type Q = P\nfn Q(s: str) -> P:\n    return P(s.len())\n"),
            Expect::Rejects("expected str, found int"),
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
}

const VV: &str = "V(A=4, k=4, pi=4)";

#[rustfmt::skip]
const FIELD_HEADS: &[FieldHead] = &[
    FieldHead { tag: "struct", name: "P", decl: "struct P:\n    x: int\n", member: "k", ty: "V", value: VV },
    FieldHead { tag: "enum", name: "E", decl: "enum E:\n    A\n", member: "A", ty: "V", value: VV },
    FieldHead { tag: "enum_alias", name: "R", decl: "enum E:\n    A\ntype R = E\n", member: "A", ty: "V", value: VV },
    FieldHead { tag: "alias", name: "Q", decl: "struct P:\n    x: int\ntype Q = P\n", member: "k", ty: "V", value: VV },
    FieldHead { tag: "imported_type", name: "T", decl: "import T from lib\n", member: "k", ty: "V", value: VV },
    FieldHead { tag: "module", name: "math", decl: "import std.math\n", member: "pi", ty: "V", value: VV },
    FieldHead { tag: "qualified_enum", name: "lib", decl: "import lib\n", member: "E.A", ty: "W", value: "W(E=V(A=4, k=4, pi=4))" },
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

fn field_bind(binder: &str, n: &str, ty: &str, e: &str, r: &str, ctx: &str) -> Option<String> {
    let b = |ind| field_block(r, ctx, ind);
    Some(match binder {
        "param" => format!("fn g({n}: {ty}):\n{}g({e})\n", b(4)),
        "local" => format!("fn g():\n    {n} := {e}\n{}g()\n", b(4)),
        "toplevel_let" => format!("{n} := {e}\n{}", b(0)),
        "for_var" => format!("for {n} in [{e}]:\n{}", b(4)),
        "match_binding" => format!(
            "match Some({e}):\n    Some({n}):\n{}    None: print(\"none\")\n",
            b(8)
        ),
        "wait_recv" => format!(
            "fn g():\n    ch := Channel[{ty}](1)\n    ch.send({e})\n    wait:\n        {n} := ch.recv():\n{}g()\n",
            b(12)
        ),
        "import_alias" => format!("import lib as {n}\n{}", b(0)),
        "closure_param" | "comprehension_var" => {
            // A closure body is one expression: only the `read` and `arg` contexts fit.
            let x = match ctx {
                "read" => r.to_string(),
                "arg" => format!("id({r})"),
                _ => return None,
            };
            if binder == "closure_param" {
                format!("print((fn({n}: {ty}) -> int: {x})({e}))\n")
            } else {
                format!("print([{x} for {n} in [{e}]][0])\n")
            }
        }
        _ => return None,
    })
}

fn field_rejection(binder: &str, tag: &str) -> Option<&'static str> {
    match (binder, tag) {
        ("toplevel_let", "module" | "qualified_enum") => {
            Some("cannot re-declare module-level binding")
        }
        ("import_alias", "module" | "qualified_enum" | "imported_type") => Some("already"),
        // A whole-module import and a same-module type under one name.
        ("import_alias", "struct" | "enum" | "enum_alias" | "alias") => Some("is already imported"),
        _ => None,
    }
}

/// The value-field read position: a value binder named like a type or module, read as `X.m`
/// (not called) in a print, an argument, a match scrutinee and a return.
fn field_cells() -> Vec<Cell> {
    let binders = [
        "param",
        "local",
        "toplevel_let",
        "for_var",
        "match_binding",
        "closure_param",
        "comprehension_var",
        "wait_recv",
        "import_alias",
    ];
    let mut cells = Vec::new();
    for h in FIELD_HEADS {
        for ctx in ["read", "arg", "scrutinee", "return"] {
            for binder in binders {
                let r = format!("{}.{}", h.name, h.member);
                let Some(body) = field_bind(binder, h.name, h.ty, h.value, &r, ctx) else {
                    continue;
                };
                let expect = match field_rejection(binder, h.tag) {
                    Some(frag) => Expect::Rejects(frag),
                    None => Expect::Prints("4".into()),
                };
                cells.push(with_lib(
                    &format!("{binder}/{}/field_{ctx}", h.tag),
                    &format!("{}{FIELD_DECLS}{body}", h.decl),
                    FIELD_LIB,
                    expect,
                ));
            }
        }
    }
    cells
}

const ORDER_PRE: &str = "struct P:\n    x: int\nenum E:\n    A\nstruct V:\n    A: int\n";

const ORDER_LIB: &str = "A := 4\n";

/// A top-level binding named like a builtin, a type or an enum, read in a fn body.
struct OrderHead {
    tag: &'static str,
    binding: &'static str,
    body: &'static [&'static str],
    ret: &'static str,
    out: &'static str,
    /// The typed-use cell's `y: <wrong> = g()` and the rejection it expects.
    wrong: &'static str,
    rejects: &'static str,
}

const ORD_BIND: &str = "ord := fn(s: str) -> int: 1000\n";
const P_BIND: &str = "P := fn(n: int) -> str: \"v{n}\"\n";
const INT_AS_STR: &str = "cannot assign int to variable of type str";
const STR_AS_INT: &str = "cannot assign str to variable of type int";

#[rustfmt::skip]
const ORDER_HEADS: &[OrderHead] = &[
    OrderHead { tag: "ord_call", binding: ORD_BIND, body: &["return ord(\"a\")"], ret: "int", out: "1000", wrong: "str", rejects: INT_AS_STR },
    OrderHead { tag: "ord_read", binding: ORD_BIND, body: &["h := ord", "return h(\"a\")"], ret: "int", out: "1000", wrong: "str", rejects: INT_AS_STR },
    OrderHead { tag: "P_call", binding: P_BIND, body: &["return P(4)"], ret: "str", out: "v4", wrong: "int", rejects: STR_AS_INT },
    OrderHead { tag: "P_read", binding: P_BIND, body: &["h := P", "return h(4)"], ret: "str", out: "v4", wrong: "int", rejects: STR_AS_INT },
    OrderHead { tag: "E_field", binding: "E := V(4)\n", body: &["return E.A"], ret: "int", out: "4", wrong: "str", rejects: INT_AS_STR },
    OrderHead { tag: "x_read", binding: "x := 5\n", body: &["return x"], ret: "int", out: "5", wrong: "str", rejects: INT_AS_STR },
];

/// `fn g` over the head's body, with an inferred or an annotated return.
fn order_fn(h: &OrderHead, annotated: bool) -> String {
    let sig = if annotated {
        format!("fn g() -> {}:\n", h.ret)
    } else {
        "fn g():\n".into()
    };
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
    let g_p = "fn g():\n    return P(4)\n";
    let e = "enum E:\n    A\n";
    let g_e = "fn g():\n    return E.A\n";
    let imp = "import lib as E\n";
    let raw = [
        (
            "swap/fn_first/call",
            format!("{p}{fn_p}{g_p}print(g())\n"),
            Expect::Prints("B4".into()),
        ),
        (
            "swap/fn_last/call",
            format!("{p}{g_p}{fn_p}print(g())\n"),
            Expect::Prints("B4".into()),
        ),
        // A whole-module import and a same-module type may not share a name, in either order.
        (
            "swap/import_first/enum_field",
            format!("{e}{imp}{g_e}print(g())\n"),
            Expect::Rejects("'E' is already imported"),
        ),
        (
            "swap/import_last/enum_field",
            format!("{e}{g_e}{imp}print(g())\n"),
            Expect::Rejects("'E' is already imported"),
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
            format!("{ORDER_PRE}print(E.A)\nE := V(4)\nprint(E.A)\n"),
            Expect::Prints("A\n4".into()),
        ),
        (
            "order/after/closure/ord_call",
            format!("f := fn() -> int: ord(\"a\")\n{ORD_BIND}print(f())\n"),
            Expect::Prints("1000".into()),
        ),
        (
            "order/inferred_return_type_of_a_global",
            "x := \"s\"\nfn f():\n    return x\ny: int = f()\nprint(y)\n".into(),
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
            "t187/from_import/alias_with_fn_twin",
            "import Q from lib\nq: Q = Q(\"ab\")\nprint(q.s.len())\n",
            "struct P:\n    s: str\ntype Q = P\nfn Q(s: str) -> P:\n    return P(s)\n",
            Expect::Prints("2".into()),
        ),
        one(
            "t187/generic_value/qualified/same_name",
            pick("T", "cmp.max"),
            Expect::Rejects("is generic and"),
        ),
        one(
            "t187/generic_value/qualified/other_name",
            pick("U", "cmp.max"),
            Expect::Rejects("is generic and"),
        ),
        one(
            "t187/generic_value/from_import/same_name",
            pick("T", "max").replace("import std.cmp", "import max from std.cmp"),
            Expect::Rejects("is generic and"),
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
            Expect::Prints("Ok(7)".into()),
        ),
        one(
            "t187/decode/json_alias",
            "import std.json as j\nprint(j.decode[List[int]](\"[1]\"))\n".into(),
            Expect::Prints("Ok([1])".into()),
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
