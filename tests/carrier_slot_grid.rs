//! TICKET-227 (D3) — implicit wrap at every typed slot, one program per (carrier, slot, value).
//! Every accept cell RUNS and is read through a generated `fn show(x: C)` that matches each layer
//! by pattern (`?v`, `!e`, `None`) and uses the payload at its own type. Printed text is not the
//! oracle: a carrier prints as the user writes it, so `5` and `?5` print the same. A missed wrap
//! faults in `show` (`cannot match on int`); a double wrap faults there too (`cannot apply Add to
//! enum and int`).

#[path = "support/grid_cell.rs"]
mod grid_cell;

use grid_cell::{Cell, Expect, run_grid};

fn prints(s: &str) -> Expect {
    Expect::Prints(s.to_string())
}

fn cell(name: String, files: Vec<(&str, String)>, expect: Expect) -> Cell {
    Cell {
        name,
        files: files.into_iter().map(|(p, s)| (p.to_string(), s)).collect(),
        expect,
    }
}

fn main_only(name: String, src: String, expect: Expect) -> Cell {
    cell(name, vec![("main.chz", src)], expect)
}

/// One typed slot: `decls` go at top level, `body` inside `fn main():`. `{C}` is the carrier type,
/// `{v}` the value, `{SEED}` a value of type `{C}`. A multi-line value continues at the indent of
/// the line it lands on.
struct Slot {
    name: &'static str,
    decls: &'static str,
    body: &'static str,
    /// Fragment of the slot's own rejection of a value that does not fit.
    reject: &'static str,
    /// The statement-tail slots, where a `match` value is written.
    tail: bool,
}

// The `assignable` caller class (## Digest of TICKET-227) each row exercises — the completeness walk:
//   typed let                       -> typed let `sig.rs` (check_let)
//   assignment, multi-target        -> assignment (ident / tuple element)
//   field / list / map index assign -> assignment (field, index), index key
//   user index-set                  -> user index-set (set_index value)
//   positional / keyword / method / native method / channel send / wait send / Shared set -> arg
//   variadic arg                    -> list element (the synthesized variadic list)
//   generic arg / generic variant / generic static method -> generic arg
//   ctor field / variant payload    -> arg (ctor)
//   wait recv-assign                -> assignment (`wait` recv-assign, an ordinary assignment)
//   Shared / RwShared ctor          -> box ctor
//   list element / map value        -> list/set/map element
//   tuple element                   -> assignment of a tuple-typed let (tuple element hint)
//   list / map conversion           -> conversion ctor element
//   list / map comprehension        -> comprehension element (projected, DEC-032)
//   return / inline body / yield    -> return / inline body / yield
//   annotated closure / closure in a typed slot -> annotated closure / closure body slot
//   param default / field default   -> param default / field default
const SLOTS: &[Slot] = &[
    Slot {
        name: "typed_let",
        decls: "",
        body: "x: {C} = {v}\nshow(x)",
        reject: "cannot assign",
        tail: true,
    },
    Slot {
        name: "assign",
        decls: "",
        body: "x: {C} = {SEED}\nx = {v}\nshow(x)",
        reject: "cannot assign",
        tail: true,
    },
    Slot {
        name: "field_assign",
        decls: "struct S:\n    n: {C}\n",
        body: "s := S(n={SEED})\ns.n = {v}\nshow(s.n)",
        reject: "cannot assign",
        tail: true,
    },
    Slot {
        name: "list_index_assign",
        decls: "",
        body: "xs: List[{C}] = [{SEED}]\nxs[0] = {v}\nshow(xs[0])",
        reject: "cannot assign",
        tail: true,
    },
    Slot {
        name: "map_index_assign",
        decls: "",
        body: "m: Map[str, {C}] = {}\nm[\"k\"] = {v}\nshow(m[\"k\"])",
        reject: "cannot assign",
        tail: true,
    },
    Slot {
        name: "user_index_set",
        decls: "struct U:\n    val: {C}\n    fn index(self, k: int) -> {C}:\n        return self.val\n    fn set_index(self, k: int, v: {C}):\n        self.val = v\n",
        body: "u := U(val={SEED})\nu[0] = {v}\nshow(u[0])",
        reject: "cannot assign",
        tail: false,
    },
    Slot {
        name: "multi_assign",
        decls: "",
        body: "x: {C} = {SEED}\ny := 0\nx, y = {v}, 0\nshow(x)",
        reject: "cannot assign",
        tail: false,
    },
    Slot {
        name: "multi_index_assign",
        decls: "",
        body: "xs: List[{C}] = [{SEED}]\ny := 0\nxs[0], y = {v}, 0\nshow(xs[0])",
        reject: "cannot assign",
        tail: false,
    },
    Slot {
        name: "positional_arg",
        decls: "fn t(x: {C}):\n    show(x)\n",
        body: "t({v})",
        reject: "argument 1 of 't'",
        tail: false,
    },
    Slot {
        name: "keyword_arg",
        decls: "fn k(a: int, b: {C} = {SEED}):\n    show(b)\n",
        body: "k(0, b={v})",
        reject: "argument 2 of 'k'",
        tail: false,
    },
    Slot {
        name: "variadic_arg",
        decls: "fn va(...xs: {C}):\n    show(xs[0])\n",
        body: "va({v})",
        reject: "list element",
        tail: false,
    },
    Slot {
        name: "generic_arg",
        decls: "fn f[T](a: T, b: {C}):\n    show(b)\n",
        body: "f(0, {v})",
        reject: "argument to 'f'",
        tail: false,
    },
    Slot {
        name: "ctor_field",
        decls: "struct S:\n    n: {C}\n",
        body: "show(S(n={v}).n)",
        reject: "argument 1 of 'S'",
        tail: false,
    },
    Slot {
        name: "variant_payload",
        decls: "enum E:\n    V({C})\n",
        body: "match E.V({v}):\n    E.V(x):\n        show(x)",
        reject: "argument 1 of 'V'",
        tail: false,
    },
    Slot {
        name: "generic_variant_payload",
        decls: "enum G[T]:\n    V(T, {C})\n",
        body: "match G.V(0, {v}):\n    G.V(_, x):\n        show(x)",
        reject: "argument to 'V'",
        tail: false,
    },
    Slot {
        name: "method_arg",
        decls: "struct M:\n    k: int\n    fn put(self, x: {C}):\n        show(x)\n",
        body: "M(0).put({v})",
        reject: "argument 1 of 'put'",
        tail: false,
    },
    Slot {
        name: "native_method_arg",
        decls: "",
        body: "xs: List[{C}] = []\nxs.push({v})\nshow(xs[0])",
        reject: "argument 1 of 'push'",
        tail: false,
    },
    Slot {
        name: "generic_static_method_arg",
        decls: "struct H[T]:\n    v: T\n    fn mk(a: T, x: {C}):\n        show(x)\n",
        body: "H.mk(0, {v})",
        reject: "argument to 'mk'",
        tail: false,
    },
    Slot {
        name: "channel_send",
        decls: "",
        body: "ch := Channel[{C}](1)\nch.send({v})\nr := ch.recv() else: panic(\"closed\")\nshow(r)",
        reject: "argument 1 of 'send'",
        tail: false,
    },
    Slot {
        name: "wait_send_arm",
        decls: "",
        body: "ch := Channel[{C}](1)\nwait:\n    ch.send({v}):\n        r := ch.recv() else: panic(\"closed\")\n        show(r)",
        reject: "argument 1 of 'send'",
        tail: false,
    },
    Slot {
        name: "shared_set",
        decls: "import Shared from std.concurrency\n",
        body: "s := Shared[{C}]({SEED})\ns.set({v})\nshow(s.get())",
        reject: "argument 1 of 'set'",
        tail: false,
    },
    Slot {
        name: "shared_ctor",
        decls: "import Shared from std.concurrency\n",
        body: "s := Shared[{C}]({v})\nshow(s.get())",
        reject: "expected element type",
        tail: false,
    },
    Slot {
        name: "rwshared_ctor",
        decls: "import RwShared from std.concurrency\n",
        body: "s := RwShared[{C}]({v})\nshow(s.get())",
        reject: "expected element type",
        tail: false,
    },
    Slot {
        name: "list_element",
        decls: "",
        body: "xs: List[{C}] = [{v}]\nshow(xs[0])",
        reject: "list element",
        tail: false,
    },
    Slot {
        name: "map_value",
        decls: "",
        body: "m: Map[str, {C}] = {\"k\": {v}}\nshow(m[\"k\"])",
        reject: "map value",
        tail: false,
    },
    Slot {
        name: "tuple_element",
        decls: "",
        body: "t: ({C}, int) = ({v}, 0)\nshow(t.0)",
        reject: "cannot assign",
        tail: false,
    },
    Slot {
        name: "list_comprehension",
        decls: "",
        body: "xs: List[{C}] = [{v} for i in [0]]\nshow(xs[0])",
        reject: "cannot assign",
        tail: false,
    },
    Slot {
        name: "map_comprehension_value",
        decls: "",
        body: "m: Map[str, {C}] = {\"k\": {v} for i in [0]}\nshow(m[\"k\"])",
        reject: "cannot assign",
        tail: false,
    },
    Slot {
        name: "return",
        decls: "fn r() -> {C}:\n    return {v}\n",
        body: "show(r())",
        reject: "expected return type",
        tail: true,
    },
    Slot {
        name: "inline_body",
        decls: "fn r() -> {C}: {v}\n",
        body: "show(r())",
        reject: "expected return type",
        tail: false,
    },
    Slot {
        name: "yield",
        decls: "import Iterator from std.iter\nfn g() -> Iterator[{C}]:\n    yield {v}\n",
        body: "for y in g():\n    show(y)",
        reject: "expected yield type",
        tail: true,
    },
    Slot {
        name: "annotated_closure",
        decls: "",
        body: "k := fn() -> {C}: {v}\nshow(k())",
        reject: "closure body has type",
        tail: false,
    },
    Slot {
        name: "closure_in_typed_slot",
        decls: "fn g(k: fn() -> {C}):\n    show(k())\n",
        body: "g(fn(): {v})",
        reject: "argument 1 of 'g'",
        tail: false,
    },
    Slot {
        name: "param_default",
        decls: "fn dflt(x: {C} = {v}):\n    show(x)\n",
        body: "dflt()",
        reject: "default value for parameter",
        tail: false,
    },
    Slot {
        name: "field_default",
        decls: "struct D:\n    n: {C} = {v}\n",
        body: "show(D().n)",
        reject: "default value for field",
        tail: false,
    },
];

/// The structural readers, one per carrier type: each names every layer by pattern and uses the
/// innermost payload in an operation of its own type.
const SHOW_OPT: &str = "fn show(x: int?):\n    match x:\n        ?v:\n            print(\"some {v + 0}\")\n        None:\n            print(\"none\")\n";
const SHOW_RES: &str = "fn show(x: int!str):\n    match x:\n        ?v:\n            print(\"ok {v + 0}\")\n        !e:\n            print(\"err {e.upper()}\")\n";
const SHOW_VOID: &str = "fn show(x: None!str):\n    match x:\n        ?_:\n            print(\"ok\")\n        !e:\n            print(\"err {e.upper()}\")\n";
const SHOW_OPT2: &str = "fn show(x: int??):\n    match x:\n        ?(?v):\n            print(\"some some {v + 0}\")\n        ?None:\n            print(\"some none\")\n        None:\n            print(\"none\")\n";

/// How a value fares at a carrier: the text `show` prints, the slot's own rejection, or a named one.
#[derive(Clone, Copy)]
enum Out {
    Prints(&'static str),
    SlotRejects,
    Rejects(&'static str),
}

const HAVE_OPT: &str = "fn have(n: int) -> int?:\n    return n\n";
const HAVE_RES: &str = "fn have(n: int) -> int!str:\n    return n\n";
const HAVE_VOID: &str = "fn have() -> None!str:\n    return\n";

struct Carrier {
    ty: &'static str,
    /// The `fn show(x: C)` every accept cell of this carrier is read through.
    show: &'static str,
    /// A helper that returns an EXISTING carrier: `have(5)` for the value rows, `have()` for the
    /// void success. For `int??` it returns `int?`, one layer short of the slot.
    have: &'static str,
    seed: &'static str,
    /// `ALT` of the branch values: the other branch's carrier-shaped value.
    alt: &'static str,
    /// `(value, outcome)`, in the order 5, None, ?5, !"e", an existing carrier, `BR`.
    values: [(&'static str, Out); 6],
    /// The FIT6 of the branch shapes, and the text a branch shape prints (`None` = no shapes).
    shapes: Option<(&'static str, &'static str)>,
}

const BR_INCOMPATIBLE: Out = Out::Rejects("branches have incompatible types");

const CARRIERS: &[Carrier] = &[
    Carrier {
        ty: "int?",
        show: SHOW_OPT,
        have: HAVE_OPT,
        seed: "None",
        alt: "None",
        values: [
            ("5", Out::Prints("some 5")),
            ("None", Out::Prints("none")),
            ("?5", Out::Prints("some 5")),
            ("!\"e\"", Out::SlotRejects),
            ("have(5)", Out::Prints("some 5")),
            ("BR", Out::Prints("some 5")),
        ],
        shapes: Some(("have(6)", "some 5")),
    },
    Carrier {
        ty: "int!str",
        show: SHOW_RES,
        have: HAVE_RES,
        seed: "!\"s\"",
        alt: "!\"e\"",
        values: [
            ("5", Out::Prints("ok 5")),
            ("None", Out::SlotRejects),
            ("?5", Out::Prints("ok 5")),
            ("!\"e\"", Out::Prints("err E")),
            ("have(5)", Out::Prints("ok 5")),
            ("BR", Out::Prints("ok 5")),
        ],
        shapes: Some(("have(6)", "ok 5")),
    },
    Carrier {
        ty: "None!str",
        show: SHOW_VOID,
        have: HAVE_VOID,
        seed: "have()",
        alt: "!\"e\"",
        values: [
            ("5", Out::SlotRejects),
            ("None", Out::SlotRejects),
            ("?5", Out::Rejects("'?' value: expected")),
            ("!\"e\"", Out::Prints("err E")),
            ("have()", Out::Prints("ok")),
            ("BR", BR_INCOMPATIBLE),
        ],
        shapes: None,
    },
    Carrier {
        ty: "int??",
        show: SHOW_OPT2,
        have: HAVE_OPT,
        seed: "None",
        alt: "None",
        values: [
            ("5", Out::SlotRejects),
            ("None", Out::Prints("none")),
            ("?5", Out::Prints("some some 5")),
            ("!\"e\"", Out::SlotRejects),
            ("have(5)", Out::SlotRejects),
            ("BR", BR_INCOMPATIBLE),
        ],
        shapes: None,
    },
];

/// Substitute `{C}`, `{SEED}` and `{v}` into `tmpl`; a multi-line `v` continues at the indent of
/// the line it lands on.
fn fill(tmpl: &str, c: &Carrier, v: &str) -> String {
    tmpl.lines()
        .map(|line| {
            let line = line.replace("{C}", c.ty).replace("{SEED}", c.seed);
            if !line.contains("{v}") {
                return line;
            }
            let indent: String = line.chars().take_while(|ch| *ch == ' ').collect();
            let v = v.replace('\n', &format!("\n{indent}"));
            line.replace("{v}", &v)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A whole program: the branch flags, the carrier's `show`, the slot's declarations, and `main`.
fn program(slot: &Slot, c: &Carrier, v: &str, flags: (bool, bool)) -> String {
    let body = fill(slot.body, c, v)
        .lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    let decls = fill(slot.decls, c, v);
    format!(
        "c := {}\nd := {}\n{}{}{decls}\nfn main():\n{body}\nmain()\n",
        flags.0, flags.1, c.show, c.have
    )
}

fn expect(out: Out, slot: &Slot) -> Expect {
    match out {
        Out::Prints(s) => prints(s),
        Out::SlotRejects => Expect::Rejects(slot.reject),
        Out::Rejects(f) => Expect::Rejects(f),
    }
}

fn slot_cells(cells: &mut Vec<Cell>) {
    for slot in SLOTS {
        for c in CARRIERS {
            for (v, out) in c.values {
                let (src, flags) = match v {
                    "BR" => (format!("(if c: 5 else: {})", c.alt), (true, false)),
                    v => (format!("({v})"), (true, false)),
                };
                cells.push(main_only(
                    format!("{}/{}/{v}", slot.name, c.ty),
                    program(slot, c, &src, flags),
                    expect(out, slot),
                ));
            }
            if let Some((fit6, shown)) = c.shapes {
                let shapes = [
                    (
                        "ELIF",
                        format!("(if c: {fit6} elif d: 5 else: 7)"),
                        (false, true),
                    ),
                    (
                        "NEST_ELSE",
                        format!("(if c: {fit6} else: (if c: 7 else: 5))"),
                        (false, false),
                    ),
                    (
                        "NEST_THEN",
                        format!("(if c: (if c: 5 else: {}) else: 6)", c.alt),
                        (true, false),
                    ),
                ];
                for (name, src, flags) in shapes {
                    cells.push(main_only(
                        format!("{}/{}/{name}", slot.name, c.ty),
                        program(slot, c, &src, flags),
                        prints(shown),
                    ));
                }
                if slot.tail {
                    let src = format!("match c:\n    true: 5\n    false: {}", c.alt);
                    cells.push(main_only(
                        format!("{}/{}/MATCH", slot.name, c.ty),
                        program(slot, c, &src, (true, false)),
                        prints(shown),
                    ));
                }
            }
        }
    }
}

/// A `wait` arm binds `T!RecvError`; its element, unwrapped, is assigned into the target.
fn wait_recv_assign_cells(cells: &mut Vec<Cell>) {
    let slot = Slot {
        name: "wait_recv_assign",
        decls: "",
        body: "ch := Channel[int](1)\nch.send(5)\nx: {C} = {SEED}\nwait:\n    r := ch.recv():\n        v := r else: panic(\"ch closed\")\n        x = v\n        show(x)",
        reject: "cannot assign",
        tail: false,
    };
    for c in CARRIERS {
        let out = c.values[0].1;
        cells.push(main_only(
            format!("wait_recv_assign/{}", c.ty),
            program(&slot, c, "", (true, false)),
            expect(out, &slot),
        ));
    }
}

/// `T[C].from_iter([v])` is no typed slot: the argument's elements keep their own type, which must
/// be `C` itself, so a plain `5` never wraps into `int?` (Rust E0271 on the same program). A value
/// of type `C` reaches the conversion and runs. One row per carrier, in the order of
/// [`Carrier::values`] and then, for a carrier with branch shapes, `ELIF`, `NEST_ELSE`, `NEST_THEN`
/// (the last three outcomes of a carrier without shapes are unused).
fn conversion_cells(cells: &mut Vec<Cell>) {
    const ELEM: Out = Out::Rejects("iterator element type");
    const NEEDS_ANNOTATION: Out = Out::Rejects("a `!` value needs its type from an annotation");
    let rows: [(&str, [Out; 9]); 4] = [
        (
            "int?",
            [
                ELEM,
                Out::Prints("none"),
                Out::Prints("some 5"),
                ELEM,
                Out::Prints("some 5"),
                Out::Prints("some 5"),
                BR_INCOMPATIBLE,
                BR_INCOMPATIBLE,
                BR_INCOMPATIBLE,
            ],
        ),
        (
            "int!str",
            [
                ELEM,
                ELEM,
                Out::Prints("ok 5"),
                Out::Prints("err E"),
                Out::Prints("ok 5"),
                NEEDS_ANNOTATION,
                BR_INCOMPATIBLE,
                BR_INCOMPATIBLE,
                NEEDS_ANNOTATION,
            ],
        ),
        (
            "None!str",
            [
                ELEM,
                ELEM,
                Out::Rejects("'?' value: expected"),
                Out::Prints("err E"),
                Out::Prints("ok"),
                NEEDS_ANNOTATION,
                ELEM,
                ELEM,
                ELEM,
            ],
        ),
        (
            "int??",
            [
                ELEM,
                Out::Prints("none"),
                Out::Rejects("'?' value: expected int?, found int"),
                ELEM,
                ELEM,
                ELEM,
                ELEM,
                ELEM,
                ELEM,
            ],
        ),
    ];
    let forms = [
        (
            "list_conversion",
            "xs := List[{C}].from_iter([{v}])\nshow(xs[0])",
        ),
        (
            "map_conversion",
            "m := Map[str, {C}].from_iter([(\"k\", {v})])\nshow(m[\"k\"])",
        ),
    ];
    for (name, body) in forms {
        let slot = Slot {
            name,
            decls: "",
            body,
            reject: "iterator element type",
            tail: false,
        };
        for (ty, outs) in rows {
            let c = CARRIERS.iter().find(|c| c.ty == ty).expect("a carrier row");
            let mut srcs: Vec<(String, String, (bool, bool))> = c
                .values
                .iter()
                .map(|(v, _)| match *v {
                    "BR" => (
                        v.to_string(),
                        format!("(if c: 5 else: {})", c.alt),
                        (true, false),
                    ),
                    v => (v.to_string(), format!("({v})"), (true, false)),
                })
                .collect();
            if let Some((fit6, _)) = c.shapes {
                srcs.push((
                    "ELIF".into(),
                    format!("(if c: {fit6} elif d: 5 else: 7)"),
                    (false, true),
                ));
                srcs.push((
                    "NEST_ELSE".into(),
                    format!("(if c: {fit6} else: (if c: 7 else: 5))"),
                    (false, false),
                ));
                srcs.push((
                    "NEST_THEN".into(),
                    format!("(if c: (if c: 5 else: {}) else: 6)", c.alt),
                    (true, false),
                ));
            }
            for ((v, src, flags), out) in srcs.into_iter().zip(outs) {
                cells.push(main_only(
                    format!("{name}/{ty}/{v}"),
                    program(&slot, c, &src, flags),
                    expect(out, &slot),
                ));
            }
        }
    }
}

/// No carrier is `Hashable`: set elements and map keys reject before a wrap matters.
fn hashable_cells(cells: &mut Vec<Cell>) {
    // `Set.from_iter` rejects through its decl's `T: Hashable` bound; the typed slots through the
    // key check.
    let shapes = [
        (
            "s := Set[{C}].from_iter([{v}])",
            "does not satisfy Hashable",
        ),
        ("s: Set[{C}] = {{v}}", "must implement Hashable"),
        ("m: Map[{C}, str] = {{v}: \"a\"}", "must implement Hashable"),
        (
            "s: Set[{C}] = {{v} for i in [0]}",
            "must implement Hashable",
        ),
    ];
    for c in &CARRIERS[..2] {
        for v in ["5", "?5"] {
            for (i, (shape, needle)) in shapes.iter().enumerate() {
                let body = shape
                    .replace("{{v}", &format!("{{{v}"))
                    .replace("{C}", c.ty)
                    .replace("{v}", v);
                cells.push(main_only(
                    format!("hashable{i}/{}/{v}", c.ty),
                    format!("fn main():\n    {body}\nmain()\n"),
                    Expect::Rejects(needle),
                ));
            }
        }
    }
}

fn extra_cells(cells: &mut Vec<Cell>) {
    let m = |name: &str, src: &str, e: Expect| main_only(name.to_string(), format!("{src}\n"), e);
    // A cell read through `show`: the carrier's reader goes in front of the cell source.
    let ms = |name: &str, show: &str, src: &str, e: Expect| {
        main_only(name.to_string(), format!("{show}{src}\n"), e)
    };
    let r = Expect::Rejects;
    cells.extend([
        ms("q_default_optional", SHOW_OPT, "fn main():\n    y := ?5\n    show(y)\nmain()", prints("some 5")),
        ms("q_var_bound_unknown", SHOW_OPT, "fn main():\n    y := ?5\n    ys := [y]\n    ys = []\n    show(y)\n    print(ys)\nmain()", prints("some 5\n[]")),
        // TICKET-238: `z := ?5` is `int?` on its own line; no later slot re-reads it.
        ms("q_not_pinned_by_result", SHOW_RES, "fn take(r: int!str):\n    show(r)\nfn main():\n    z := ?5\n    take(z)\nmain()", r("expected int!str, found int?")),
        m("bang_not_pinned_by_return", "fn f() -> int!:\n    e := !\"disk\"\n    return e\nmatch f():\n    ?v:\n        print(\"ok {v + 0}\")\n    !e:\n        print(\"err {e.message()}\")", r("a `!` value needs its type from an annotation")),
        m("bang_unpinned_fn", "fn main():\n    w := !\"disk\"\n    print(w)\nmain()", r("a `!` value needs its type from an annotation")),
        m("bang_unpinned_top", "w := !\"disk\"\nprint(w)", r("a `!` value needs its type from an annotation")),
        m("bang_not_error", "fn f() -> int!:\n    return !5\nprint(f())", r("int does not satisfy Error")),
        ms("bang_whole_operand", SHOW_RES, "fn wrap(s: str) -> str:\n    return s + \"!\"\nfn f() -> int!str:\n    return !wrap(\"x\")\nshow(f())", prints("err X!")),
        ms("bang_in_list", SHOW_RES, "rs: List[int!str] = [1, !\"disk\", 3]\nfor r in rs:\n    show(r)", prints("ok 1\nerr DISK\nok 3")),
        ms("none_bang_falls_off", SHOW_VOID, "fn save(p: str) -> None!str:\n    if p == \"\":\n        return !\"empty\"\n    pass\nshow(save(\"a\"))", prints("ok")),
        m("none_bang_is_not_a_value", "fn save() -> None!str:\n    return\nfn main():\n    x := save()?\nmain()", r("cannot be used as a value")),
        m("generic_slot_declines", "fn f[T](x: T) -> T?:\n    return x\nprint(f(1))", r("expected return type")),
        m("no_int_float", "fn f() -> float?:\n    return 1\nprint(f())", r("expected return type")),
        ms("operands_never_wrap", SHOW_OPT, "x: int? = 1 + 2\nshow(x)", prints("some 3")),
        ms("default_list", SHOW_OPT, "fn f(xs: List[int?] = [5]):\n    show(xs[0])\nf()", prints("some 5")),
        ms("default_provider_param", SHOW_OPT, "fn g() -> int:\n    return 5\nfn f(x: int? = g()):\n    show(x)\nf()", prints("some 5")),
        ms("default_provider_field", SHOW_OPT, "fn g() -> int:\n    return 5\nstruct S:\n    n: int? = g()\nshow(S().n)", prints("some 5")),
        ms("default_q", SHOW_OPT, "fn f(x: int? = ?5):\n    show(x)\nf()", prints("some 5")),
        ms("default_bang", SHOW_RES, "fn f(x: int!str = !\"e\"):\n    show(x)\nf()", prints("err E")),
        m("interp_arg", "fn t(x: int?) -> str:\n    match x:\n        ?v:\n            return \"some {v + 0}\"\n        None:\n            return \"none\"\nprint(\"{t(5)}\")", prints("some 5")),
        ms("coalesce_option", SHOW_OPT, "o: int? = 5\nx: int? = o ?? 0\nshow(x)", prints("some 5")),
        ms("coalesce_result", SHOW_RES, "o: int? = 5\nx: int!str = o ?? 0\nshow(x)", prints("ok 5")),
        m("coalesce_none_arm", "fn f(o: int?) -> int?:\n    return o ?? None\nprint(f(None))", r("'??' sides have incompatible types")),
        m("comprehension_barrier", "ys: List[int] = [y for xs in [[1, 2], [3]] for y in xs]\nprint(ys)", prints("[1, 2, 3]")),
        m("inferred_return_not_a_slot", "fn f(c: bool): if c: 1 else: None\nprint(f(true))", prints("1")),
        m("tuple_call_does_not_split", "fn g() -> (int, int):\n    return (5, 0)\nx: int? = None\ny := 0\nx, y = g()\nprint(x)", r("cannot assign")),
    ]);
    // A default compiles as the declaration's node: called across a module boundary too.
    cells.push(cell(
        "default_list_from_lib".to_string(),
        vec![
            (
                "lib.chz",
                format!("{SHOW_OPT}fn f(xs: List[int?] = [5]):\n    show(xs[0])\n"),
            ),
            ("main.chz", "import lib\nlib.f()\n".to_string()),
        ],
        prints("some 5"),
    ));
    cells.push(cell(
        "default_from_lib".to_string(),
        vec![
            (
                "lib.chz",
                format!("{SHOW_OPT}fn f(x: int? = 5):\n    show(x)\n"),
            ),
            ("main.chz", "import lib\nlib.f()\n".to_string()),
        ],
        prints("some 5"),
    ));
}

#[test]
fn carrier_slot_grid() {
    let mut cells = Vec::new();
    slot_cells(&mut cells);
    conversion_cells(&mut cells);
    wait_recv_assign_cells(&mut cells);
    hashable_cells(&mut cells);
    extra_cells(&mut cells);
    run_grid("carrier-slot", &cells, &[]);
}

/// An `if` whose branches are a plain value and an unpinned `!e` (`if c: 5 else: !"e"`) has no
/// carrier type, and nothing wraps its `5`. A later compare in the same statement must not fill
/// its open success slot: a generic bound's element recovery, a list or `if` sibling of type
/// `T!E`. Each such cell rejects with the annotation error; the annotated binding wraps and runs.
/// The `?` rows pin the neighbour: `if c: 5 else: ?6` rejects at its own join today.
/// [`hint_source_cells`] adds the axis of where the expected type comes from.
#[test]
fn open_carrier_join_grid() {
    const NEEDS: &str = "a `!` value needs its type from an annotation: add `-> T!E` to the function, or annotate the binding, e.g. `w: int!str = !e`";
    const Q_JOIN: &str = "branches have incompatible types: int and int?";
    const DECLS: &str = "import Iterable from std.iter\nstruct Bx[T]:\n    v: List[T]\n    fn from2[S: Iterable[T]](xs: S) -> Bx[T]:\n        out := List[T]()\n        for x in xs:\n            out.push(x)\n        return Bx(out)\nfn f[S: Iterable[T], T](xs: S) -> List[T]:\n    out := List[T]()\n    for x in xs:\n        out.push(x)\n    return out\nc := true\n";
    // (carrier, its reader, the carrier branch beside a plain `5`, that join's rejection)
    let kinds: [(&str, &str, &str, &str); 2] = [
        ("int!str", SHOW_RES, "!\"e\"", NEEDS),
        ("int?", SHOW_OPT, "?6", Q_JOIN),
    ];
    // (site, program body with `{C}` / `{v}`, accepts)
    let sites: [(&str, &str, bool); 6] = [
        (
            "bound_static",
            "for x in Bx[{C}].from2([{v}]).v:\n    show(x)",
            false,
        ),
        (
            "bound_free_fn",
            "b: List[{C}] = f([{v}])\nfor x in b:\n    show(x)",
            false,
        ),
        (
            "list_sibling",
            "w: {C} = 9\nfor x in [{v}, w]:\n    show(x)",
            false,
        ),
        (
            "if_sibling",
            "w: {C} = 9\nx := if c: {v} else: w\nshow(x)",
            false,
        ),
        (
            "annotated",
            "xs: List[{C}] = [{v}]\nfor x in xs:\n    show(x)",
            true,
        ),
        ("unannotated", "x := {v}\nshow(x)", false),
    ];
    let mut cells = Vec::new();
    for (ty, show, carrier, reject) in kinds {
        let shapes = [
            ("if", format!("(if c: 5 else: {carrier})"), "", reject),
            (
                "nested_if",
                format!("(if c: (if c: 5 else: {carrier}) else: 6)"),
                "",
                reject,
            ),
            // A multi-line `match` is a block value: it reaches a slot through a binding.
            (
                "match",
                "m".to_string(),
                "m := match c:\n    true: 5\n    false: {K}\n",
                reject,
            ),
        ];
        for (shape, v, pre, shape_reject) in &shapes {
            for (site, body, accepts) in sites {
                let pre = pre.replace("{K}", carrier);
                let src = format!(
                    "{DECLS}{show}{pre}{}\n",
                    body.replace("{C}", ty).replace("{v}", v)
                );
                let expect = if accepts && pre.is_empty() {
                    prints(if ty == "int?" { "some 5" } else { "ok 5" })
                } else {
                    Expect::Rejects(shape_reject)
                };
                cells.push(main_only(format!("{ty}/{shape}/{site}"), src, expect));
            }
        }
        // The annotated twin of the `match` row: the written type wraps the plain arm.
        cells.push(main_only(
            format!("{ty}/match/annotated_binding"),
            format!(
                "{DECLS}{show}m: {ty} = match c:\n    true: 5\n    false: {carrier}\nfor x in Bx[{ty}].from2([m]).v:\n    show(x)\n"
            ),
            prints(if ty == "int?" { "some 5" } else { "ok 5" }),
        ));
    }
    hint_source_cells(&mut cells, DECLS, NEEDS);
    seed_join_and_report_cells(&mut cells);
    run_grid("open-carrier-join", &cells, &[]);
}

/// A seed joins arms that need no wrap as on base: `Any` and a protocol, under `==` and `in`, for
/// each arm (outputs from the base binary). The `!e` report lands on the `!e` arm in either order
/// and in a `match`, once per statement.
fn seed_join_and_report_cells(cells: &mut Vec<Cell>) {
    const P: &str = "protocol P:\n    fn n(self) -> int\nstruct A:\n    x: int\n    fn n(self) -> int:\n        return 1\nstruct B:\n    x: int\n    fn n(self) -> int:\n        return 2\n";
    let joins = [
        (
            "any_eq",
            "",
            "a: Any = 1\nprint((if c: 1 else: \"s\") == a)",
        ),
        (
            "any_in",
            "",
            "xs: List[Any] = [1]\nprint((if c: 1 else: \"s\") in xs)",
        ),
        (
            "proto_eq",
            P,
            "p: P = A(1)\nprint((if c: A(1) else: B(2)) == p)",
        ),
        (
            "proto_in",
            P,
            "ps: List[P] = [A(1)]\nprint((if c: A(1) else: B(2)) in ps)",
        ),
    ];
    for (name, decls, body) in joins {
        for c in ["true", "false"] {
            cells.push(main_only(
                format!("seed_join/{name}/{c}"),
                format!("{decls}c := {c}\n{body}\n"),
                prints(c),
            ));
        }
    }
    let m = |name: &str, src: &str, frag: &'static str| {
        main_only(
            name.to_string(),
            format!("c := true\n{src}\n"),
            Expect::Rejects(frag),
        )
    };
    cells.extend([
        m(
            "report/bang_first",
            "x := if c: !\"e\" else: 5\nprint(x)",
            "main.chz:2:12): a `!` value needs",
        ),
        m(
            "report/bang_second",
            "x := if c: 5 else: !\"e\"\nprint(x)",
            "main.chz:2:20): a `!` value needs",
        ),
        m(
            "report/match",
            "m := match c:\n    true: !\"e\"\n    false: 5\nprint(m)",
            "main.chz:3:11): a `!` value needs",
        ),
        m(
            "report/once",
            "xs := [if c: 5 else: !\"e\", !\"f\"]\nprint(xs)",
            "chezzi: 1 type error",
        ),
        // A seed never joins `int` and an unpinned `?6` (its default `int?`): no hint rejects it too.
        m(
            "seed_join/q_default",
            "o: int? = 5\nprint((if c: 5 else: ?6) == o)",
            "branches have incompatible types: int and int?",
        ),
    ]);
}

/// The hint-source axis of [`open_carrier_join_grid`]: one `if` of a plain `5` and a carrier arm
/// under each source of an expected type. An owned slot (binding, param, return, collection
/// element, an `Any` slot) wraps the `5` and prints its annotated twin's value. A seed (a generic
/// `T`, an operand of `==` / `!=` / `in`, under any outer type) wraps nothing: a `!e` arm rejects
/// with the annotation error, and a `None` arm takes its sibling's optional, as with no hint.
fn hint_source_cells(cells: &mut Vec<Cell>, decls: &str, needs: &'static str) {
    const G: &str = "fn g[T](x: T) -> T:\n    return x\n";
    // (carrier, its reader, the `if`, the generic-seed verdict, the `Any` slot output)
    let kinds: [(&str, &str, &str, Expect, &str); 2] = [
        (
            "int!str",
            SHOW_RES,
            "(if c: 5 else: !\"e\")",
            Expect::Rejects("branches have incompatible types: int and int!str"),
            "5",
        ),
        (
            "int?",
            SHOW_OPT,
            "(if c: 5 else: None)",
            prints("some 5"),
            "5",
        ),
    ];
    for (ty, show, v, seed, any) in kinds {
        let ok = if ty == "int?" { "some 5" } else { "ok 5" };
        let bang = ty == "int!str";
        // (source, program body with `{C}` / `{v}`, verdict)
        let op = |out: &'static str| {
            if bang {
                Expect::Rejects(needs)
            } else {
                prints(out)
            }
        };
        let sources: Vec<(&str, &str, Expect)> = vec![
            ("binding", "x: {C} = {v}\nshow(x)", prints(ok)),
            ("param", "show({v})", prints(ok)),
            (
                "declared_return",
                "fn k() -> {C}:\n    return {v}\nshow(k())",
                prints(ok),
            ),
            (
                "list_element",
                "xs: List[{C}] = [{v}]\nshow(xs[0])",
                prints(ok),
            ),
            ("any_slot", "x: Any = {v}\nprint(x)", prints(any)),
            ("generic_seed", "y: {C} = g({v})\nshow(y)", seed.clone()),
            (
                "eq_under_bool",
                "w: {C} = 5\nb: bool = {v} == w\nprint(b)",
                op("true"),
            ),
            ("eq_under_print", "w: {C} = 5\nprint({v} == w)", op("true")),
            (
                "eq_right_operand",
                "w: {C} = 5\nprint(w == {v})",
                op("true"),
            ),
            (
                "eq_unhinted",
                "w: {C} = 5\nb := {v} == w\nprint(b)",
                op("true"),
            ),
            (
                "ne_under_bool",
                "w: {C} = 5\nb: bool = {v} != w\nprint(b)",
                op("false"),
            ),
            (
                "in_under_print",
                "w: {C} = 5\nprint({v} in [w])",
                op("true"),
            ),
        ];
        for (source, body, expect) in sources {
            let src = format!(
                "{decls}{G}{show}{}\n",
                body.replace("{C}", ty).replace("{v}", v)
            );
            cells.push(main_only(format!("hint/{ty}/{source}"), src, expect));
        }
    }
}
