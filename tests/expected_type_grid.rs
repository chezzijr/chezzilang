//! TICKET-239 — the expected type of an expression has ONE channel, so every slot kind gives a
//! value the same verdict, at module scope and in a fn body. Three generated grids, one program
//! per cell, run through the built binary:
//!
//! 1. `expected_type_slot_grid` — slot kind x value x scope. Every accepted cell RUNS and is read
//!    through a `fn show(x: T)` that takes the stored value apart BY PATTERN (`?v`, `!e`, `None`,
//!    a call of the closure, a `len`), so a missed or doubled wrap faults instead of printing the
//!    same text.
//! 2. `join_grid` — construct x pair of sibling values. An accepted cell is two programs: a type
//!    probe (`zz: bool = x` names the joined type in its rejection) and a run.
//! 3. `void_chain_grid` — `x?.m()` where `m` returns nothing, receiver x position.

#[path = "support/grid_cell.rs"]
mod grid_cell;

use grid_cell::{Cell, Expect, run_grid};

fn main_only(name: String, src: String, expect: Expect) -> Cell {
    Cell {
        name,
        files: vec![("main.chz".to_string(), src)],
        expect,
    }
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

// ===== grid I: slot kind x value x scope =====

/// One typed slot. `decls` go at top level; `stmt` goes at top level or inside `fn main():`.
/// `{T}` is the slot type, `{v}` the value, `{SEED}` another value of type `{T}`. `stmt` hands
/// the stored value to `show`.
struct Slot {
    name: &'static str,
    decls: &'static str,
    stmt: &'static str,
}

const G: &str = "fn g[T](x: T) -> T:\n    return x\n";
const B: &str = "struct B[T]:\n    v: T\n";
const B_MK: &str = "struct B[T]:\n    v: T\n    fn mk(x: T) -> B[T]:\n        return B(x)\n";

const SLOTS: &[Slot] = &[
    Slot {
        name: "typed_binding",
        decls: "",
        stmt: "x: {T} = {v}\nshow(x)",
    },
    Slot {
        name: "fn_arg",
        decls: "fn take(x: {T}):\n    show(x)\n",
        stmt: "take({v})",
    },
    Slot {
        name: "generic_fn_turbofish",
        decls: G,
        stmt: "r := g[{T}]({v})\nshow(r)",
    },
    Slot {
        name: "generic_fn_inferred",
        decls: G,
        stmt: "r: {T} = g({v})\nshow(r)",
    },
    Slot {
        name: "struct_ctor",
        decls: "struct S:\n    v: {T}\n",
        stmt: "s := S({v})\nshow(s.v)",
    },
    Slot {
        name: "generic_ctor_type_args",
        decls: B,
        stmt: "b := B[{T}]({v})\nshow(b.v)",
    },
    Slot {
        name: "generic_ctor_annotation",
        decls: B,
        stmt: "b: B[{T}] = B({v})\nshow(b.v)",
    },
    Slot {
        name: "static_type_args",
        decls: B_MK,
        stmt: "b := B[{T}].mk({v})\nshow(b.v)",
    },
    Slot {
        name: "static_annotation",
        decls: B_MK,
        stmt: "b: B[{T}] = B.mk({v})\nshow(b.v)",
    },
    Slot {
        name: "non_generic_static",
        decls: "struct S:\n    v: {T}\n    fn mk(x: {T}) -> S:\n        return S(x)\n",
        stmt: "s := S.mk({v})\nshow(s.v)",
    },
    Slot {
        name: "method",
        decls: "struct M:\n    n: int\n    fn put(self, x: {T}):\n        show(x)\n",
        stmt: "m := M(0)\nm.put({v})",
    },
    Slot {
        name: "method_on_generic_receiver",
        decls: "struct C[T]:\n    v: T\n    fn put(self, x: T):\n        self.v = x\n",
        stmt: "c := C[{T}]({SEED})\nc.put({v})\nshow(c.v)",
    },
    Slot {
        name: "generic_method",
        decls: "struct M:\n    n: int\n    fn gm[U](self, x: U) -> U:\n        return x\n",
        stmt: "m := M(0)\nr := m.gm[{T}]({v})\nshow(r)",
    },
    Slot {
        name: "field_assign",
        decls: "struct S:\n    v: {T}\n",
        stmt: "s := S({SEED})\ns.v = {v}\nshow(s.v)",
    },
    Slot {
        name: "tuple_element",
        decls: "",
        stmt: "t: ({T}, int) = ({v}, 1)\nshow(t.0)",
    },
    Slot {
        name: "list_element",
        decls: "",
        stmt: "xs: List[{T}] = [{v}]\nshow(xs[0])",
    },
    Slot {
        name: "map_value",
        decls: "",
        stmt: "m: Map[str, {T}] = {\"k\": {v}}\nshow(m[\"k\"])",
    },
    Slot {
        name: "return",
        decls: "fn mk() -> {T}:\n    return {v}\n",
        stmt: "show(mk())",
    },
    Slot {
        name: "default_param",
        decls: "fn dp(x: {T} = {v}):\n    show(x)\n",
        stmt: "dp()",
    },
];

#[derive(Clone, Copy)]
enum Out {
    Prints(&'static str),
    Rejects(&'static str),
}

/// One value column: the slot type it is written into, and the uniform verdict for every slot.
struct Val {
    name: &'static str,
    ty: &'static str,
    v: &'static str,
    seed: &'static str,
    /// The `fn show(x: T)` every accepted cell of this column is read through.
    show: &'static str,
    out: Out,
}

const SHOW_OPT: &str = "fn show(x: int?):\n    match x:\n        ?v:\n            print(\"some {v + 0}\")\n        None:\n            print(\"none\")\n";
const SHOW_RES: &str = "fn show(x: int!str):\n    match x:\n        ?v:\n            print(\"ok {v + 0}\")\n        !e:\n            print(\"err {e.upper()}\")\n";
const SHOW_FN: &str = "fn show(f: fn(int) -> int):\n    print(f(4))\n";
const SHOW_OPT_FN: &str = "fn show(x: (fn(int) -> int)?):\n    match x:\n        ?f:\n            print(f(4))\n        None:\n            print(\"none\")\n";
const SHOW_W: &str = "fn show(x: int8):\n    print(x)\n";
const SHOW_OPT_W: &str = "fn show(x: int8?):\n    match x:\n        ?v:\n            print(\"some {v}\")\n        None:\n            print(\"none\")\n";
const SHOW_LIST: &str = "fn show(x: List[int]):\n    print(x.len())\n";
const SHOW_OPT_LIST: &str = "fn show(x: List[int]?):\n    match x:\n        ?v:\n            print(\"some {v.len()}\")\n        None:\n            print(\"none\")\n";

const WIDTH: Out = Out::Rejects("constant 300 does not fit int8");

const VALS: &[Val] = &[
    Val {
        name: "plain",
        ty: "int?",
        v: "5",
        seed: "None",
        show: SHOW_OPT,
        out: Out::Prints("some 5"),
    },
    Val {
        name: "present",
        ty: "int?",
        v: "?5",
        seed: "None",
        show: SHOW_OPT,
        out: Out::Prints("some 5"),
    },
    Val {
        name: "err",
        ty: "int!str",
        v: "!\"e\"",
        seed: "?0",
        show: SHOW_RES,
        out: Out::Prints("err E"),
    },
    Val {
        name: "none",
        ty: "int?",
        v: "None",
        seed: "?0",
        show: SHOW_OPT,
        out: Out::Prints("none"),
    },
    Val {
        name: "untyped_closure",
        ty: "fn(int) -> int",
        v: "fn(a): a + 1",
        seed: "fn(a: int) -> int: a",
        show: SHOW_FN,
        out: Out::Prints("5"),
    },
    Val {
        name: "untyped_closure_into_optional",
        ty: "(fn(int) -> int)?",
        v: "fn(a): a + 1",
        seed: "None",
        show: SHOW_OPT_FN,
        out: Out::Prints("5"),
    },
    Val {
        name: "typed_closure_into_optional",
        ty: "(fn(int) -> int)?",
        v: "fn(a: int) -> int: a + 1",
        seed: "None",
        show: SHOW_OPT_FN,
        out: Out::Prints("5"),
    },
    Val {
        name: "width",
        ty: "int8",
        v: "300",
        seed: "1",
        show: SHOW_W,
        out: WIDTH,
    },
    Val {
        name: "width_into_optional",
        ty: "int8?",
        v: "300",
        seed: "None",
        show: SHOW_OPT_W,
        out: WIDTH,
    },
    Val {
        name: "empty_list",
        ty: "List[int]",
        v: "[]",
        seed: "[1]",
        show: SHOW_LIST,
        out: Out::Prints("0"),
    },
    Val {
        name: "empty_list_into_optional",
        ty: "List[int]?",
        v: "[]",
        seed: "None",
        show: SHOW_OPT_LIST,
        out: Out::Prints("some 0"),
    },
];

fn slot_program(slot: &Slot, val: &Val, in_fn: bool) -> String {
    let fill = |s: &str| {
        s.replace("{T}", val.ty)
            .replace("{v}", val.v)
            .replace("{SEED}", val.seed)
    };
    let mut src = String::new();
    if val.ty.contains("int8") {
        src.push_str("import int8 from std.ffi\n");
    }
    src.push_str(val.show);
    src.push_str(&fill(slot.decls));
    let stmt = fill(slot.stmt);
    if in_fn {
        src.push_str("fn main():\n");
        for l in stmt.lines() {
            src.push_str("    ");
            src.push_str(l);
            src.push('\n');
        }
        src.push_str("main()\n");
    } else {
        src.push_str(&stmt);
        src.push('\n');
    }
    src
}

/// 19 slots x 2 scopes x 11 values. Red before TICKET-239 in 107 cells: an untyped closure into
/// `(fn)?` at every slot, a typed one at all but three, a plain value and `[]` into an optional at
/// the six generic slots, `!e` at five generic slots at module scope, and `300` at an annotation-
/// only generic slot (accepted, or rejected with the wrong text).
#[test]
fn expected_type_slot_grid() {
    let mut cells = Vec::new();
    for slot in SLOTS {
        for val in VALS {
            for (scope, in_fn) in [("module", false), ("fn_body", true)] {
                let expect = match val.out {
                    Out::Prints(s) => Expect::Prints(s.to_string()),
                    Out::Rejects(f) => Expect::Rejects(f),
                };
                cells.push(main_only(
                    format!("{}/{}/{scope}", slot.name, val.name),
                    slot_program(slot, val, in_fn),
                    expect,
                ));
            }
        }
    }
    assert_eq!(cells.len(), 19 * 11 * 2);
    run_grid("expected-type-slot", &cells);
}

// ===== grid II: construct x pair =====

/// The written-type note (owner, 2026-10-10): a carrier beside its plain payload does not join.
const NOTE: &str = "only under a written type";
const HASHABLE: &str = "set element type must implement Hashable";

/// What a pair of sibling values joins to, before the construct wraps it.
#[derive(Clone, Copy)]
enum Join {
    /// The joined element type.
    Ty(&'static str),
    /// No join; `Some(extra)` is a second fragment the message carries beside the construct's own.
    No(Option<&'static str>),
}

struct Pair {
    name: &'static str,
    vs: &'static [&'static str],
    /// The type of the `??` left operand that makes this pair; `None` when `??` cannot spell it.
    coalesce_lhs: Option<&'static str>,
    join: Join,
}

const PAIRS: &[Pair] = &[
    Pair {
        name: "T+T",
        vs: &["1", "2"],
        coalesce_lhs: Some("int?"),
        join: Join::Ty("int"),
    },
    Pair {
        name: "T?+T",
        vs: &["w", "5"],
        coalesce_lhs: Some("int??"),
        join: Join::No(Some(NOTE)),
    },
    // `??` has two operands: no three-operand cell.
    Pair {
        name: "T?+T+None",
        vs: &["w", "5", "None"],
        coalesce_lhs: None,
        join: Join::Ty("int?"),
    },
    Pair {
        name: "T!E+T!E",
        vs: &["a()", "b()"],
        coalesce_lhs: Some("(int!str)?"),
        join: Join::Ty("int!str"),
    },
    Pair {
        name: "T!E+T!F",
        vs: &["a()", "f()"],
        coalesce_lhs: Some("(int!str)?"),
        join: Join::No(None),
    },
    Pair {
        name: "T!E+T",
        vs: &["a()", "5"],
        coalesce_lhs: Some("(int!str)?"),
        join: Join::No(Some(NOTE)),
    },
    Pair {
        name: "int+float",
        vs: &["1", "2.0"],
        coalesce_lhs: Some("int?"),
        join: Join::No(Some("write 1.0")),
    },
    // The `??` payload is never a written `None`: no `None`-first cell.
    Pair {
        name: "None+T",
        vs: &["None", "5"],
        coalesce_lhs: None,
        join: Join::Ty("int?"),
    },
    Pair {
        name: "T!E+!e",
        vs: &["a()", "!\"e\""],
        coalesce_lhs: Some("(int!str)?"),
        join: Join::Ty("int!str"),
    },
    // The `??` payload is never a bare `!e`: no `!e`-first cell.
    Pair {
        name: "!e+T!E",
        vs: &["!\"e\"", "a()"],
        coalesce_lhs: None,
        join: Join::Ty("int!str"),
    },
];

const CONSTRUCTS: &[&str] = &["if", "match", "list", "set", "map", "??", "recover"];

/// The statements that bind `x` to the construct over the pair, or `None` when it has no spelling.
fn construct(k: &str, p: &Pair) -> Option<Vec<String>> {
    let (a, b) = (p.vs[0], p.vs[1]);
    let c = p.vs.get(2);
    Some(match k {
        "if" => match c {
            None => vec![format!("x := if c: {a} else: {b}")],
            Some(c) => vec![format!("x := if c: {a} elif n == 1: {b} else: {c}")],
        },
        "match" => {
            let mut s = vec!["x := match n:".to_string(), format!("    0: {a}")];
            match c {
                None => s.push(format!("    _: {b}")),
                Some(c) => {
                    s.push(format!("    1: {b}"));
                    s.push(format!("    _: {c}"));
                }
            }
            s
        }
        "list" => vec![format!("x := [{}]", p.vs.join(", "))],
        "set" => vec![format!("x := {{{}}}", p.vs.join(", "))],
        "map" => {
            let kv: Vec<String> =
                p.vs.iter()
                    .enumerate()
                    .map(|(i, v)| format!("\"k{i}\": {v}"))
                    .collect();
            vec![format!("x := {{{}}}", kv.join(", "))]
        }
        "??" => {
            let lhs = p.coalesce_lhs?;
            vec![format!("o: {lhs} = None"), format!("x := o ?? {b}")]
        }
        "recover" => {
            let mut s = vec![
                "x := recover:".to_string(),
                "    if c:".to_string(),
                format!("        {a}"),
            ];
            if let Some(c) = c {
                s.push("    elif n == 1:".to_string());
                s.push(format!("        {b}"));
                s.push("    else:".to_string());
                s.push(format!("        {c}"));
            } else {
                s.push("    else:".to_string());
                s.push(format!("        {b}"));
            }
            s
        }
        _ => unreachable!(),
    })
}

/// The verdict of construct `k` over pair `p`: the type `x` gets, or the fragments of its rejection.
fn verdict(k: &str, p: &Pair) -> Result<String, Vec<&'static str>> {
    let own = match k {
        "if" | "match" => "branches have incompatible types",
        "list" => "list elements differ",
        "map" => "map values differ",
        "??" => "'??' sides have incompatible types",
        _ => "",
    };
    match (k, p.join) {
        // A set of anything but plain ints is rejected at the element, whatever the pair joins to.
        ("set", _) if p.name != "T+T" => Err(match p.join {
            Join::No(Some(NOTE)) => vec![HASHABLE, NOTE],
            _ => vec![HASHABLE],
        }),
        ("set", Join::Ty(t)) => Ok(format!("Set[{t}]")),
        ("list", Join::Ty(t)) => Ok(format!("List[{t}]")),
        ("map", Join::Ty(t)) => Ok(format!("Map[str, {t}]")),
        // A `recover:` tail is `T!`; a failed join drops the value (DEC-054) and the type is `!`.
        ("recover", Join::Ty(t)) if t.contains('!') => Ok(format!("({t})!")),
        ("recover", Join::Ty(t)) => Ok(format!("{t}!")),
        ("recover", Join::No(_)) => Ok("!".to_string()),
        (_, Join::Ty(t)) => Ok(t.to_string()),
        (_, Join::No(extra)) => Err(std::iter::once(own).chain(extra).collect()),
    }
}

const JOIN_DECLS: &str = "fn a() -> int!str:\n    return !\"bad\"\nfn b() -> int!str:\n    return 5\nfn f() -> int!int:\n    return 5\n";

fn join_program(body: &[String], last: &str, in_fn: bool) -> String {
    let mut src = JOIN_DECLS.to_string();
    let (head, indent, tail) = if in_fn {
        (
            "fn main(c: bool, n: int):\n    w: int? = 5\n",
            "    ",
            "main(true, 0)\n",
        )
    } else {
        ("c := true\nn := 0\nw: int? = 5\n", "", "")
    };
    src.push_str(head);
    for l in body.iter().map(String::as_str).chain([last]) {
        src.push_str(indent);
        src.push_str(l);
        src.push('\n');
    }
    src.push_str(tail);
    src
}

/// 7 constructs x 10 pairs in a fn body, and the two `!e` pairs at module scope too. Red before
/// TICKET-239: `T!E + T!E` typed `int!` in `if` / `match` / `??`; `T!E + !e` rejected in `if` /
/// `match` / `??` and in every construct at module scope; no rejection carried the note or named
/// `??`.
#[test]
fn join_grid() {
    let mut cells = Vec::new();
    for k in CONSTRUCTS {
        for p in PAIRS {
            let Some(body) = construct(k, p) else {
                continue;
            };
            let scopes: &[(&str, bool)] = if p.name.contains("!e") {
                &[("fn_body", true), ("module", false)]
            } else {
                &[("fn_body", true)]
            };
            for (scope, in_fn) in scopes {
                let name = format!("{k}/{}/{scope}", p.name);
                match verdict(k, p) {
                    Ok(ty) => {
                        let probe = leak(format!("cannot assign {ty} to variable of type bool"));
                        cells.push(main_only(
                            format!("{name}/type"),
                            join_program(&body, "zz: bool = x", *in_fn),
                            Expect::Rejects(probe),
                        ));
                        cells.push(main_only(
                            format!("{name}/run"),
                            join_program(&body, "print(\"ran\")", *in_fn),
                            Expect::Prints("ran".to_string()),
                        ));
                    }
                    Err(frags) => {
                        for frag in frags {
                            cells.push(main_only(
                                format!("{name}/rejects {frag}"),
                                join_program(&body, "print(\"ran\")", *in_fn),
                                Expect::Rejects(frag),
                            ));
                        }
                    }
                }
            }
        }
    }
    run_grid("expected-type-join", &cells);
}

// ===== grid III: `x?.m()` where `m` returns nothing =====

const VOID_C: &str = "struct C:\n    n: int\n    fn bump(self):\n        self.n += 1\n";

/// receiver x position, each cell RUN and read through the receiver printed after the call; then
/// the three value positions, which stay the existing error. Red before TICKET-239 in every
/// position: `expression returns no value (None) and cannot be used as a value`.
#[test]
fn void_chain_grid() {
    let positions: &[(&str, &str)] = &[
        ("module_statement", "c: C? = {R}\nc?.bump()\nprint(c)\n"),
        (
            "fn_body_statement",
            "fn main():\n    c: C? = {R}\n    c?.bump()\n    print(c)\nmain()\n",
        ),
        (
            "inline_fn_body",
            "fn poke(c: C?): c?.bump()\nc: C? = {R}\npoke(c)\nprint(c)\n",
        ),
        (
            "closure_body",
            "c: C? = {R}\nf := fn(): c?.bump()\nf()\nprint(c)\n",
        ),
        ("if_tail", "c: C? = {R}\nif true: c?.bump()\nprint(c)\n"),
        (
            "match_arm_tail",
            "c: C? = {R}\nmatch 1:\n    1: c?.bump()\n    _: pass\nprint(c)\n",
        ),
        (
            "for_body",
            "xs: List[C?] = [{R}]\nfor x in xs: x?.bump()\nprint(xs[0])\n",
        ),
    ];
    let mut cells = Vec::new();
    for (recv, printed) in [("C(0)", "C(n=1)"), ("None", "None")] {
        for (pos, prog) in positions {
            cells.push(main_only(
                format!("{pos}/{recv}"),
                format!("{VOID_C}{}", prog.replace("{R}", recv)),
                Expect::Prints(printed.to_string()),
            ));
        }
    }
    for value_use in ["x := c?.bump()", "print(c?.bump())", "xs := [c?.bump()]"] {
        cells.push(main_only(
            format!("value_position/{value_use}"),
            format!("{VOID_C}c: C? = C(0)\n{value_use}\n"),
            Expect::Rejects("expression returns no value"),
        ));
    }
    run_grid("expected-type-void-chain", &cells);
}
