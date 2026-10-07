//! TICKET-227 (D3) — implicit wrap at every typed slot, one program per (carrier, slot, value).
//! Every accept cell RUNS and checks the printed value: a seam whose value does not own its slot
//! turns a plain-`5` cell into a reject; a missed `compile_expr` hook prints `5` where `Some(5)`
//! is expected; a double wrap prints `Some(Some(5))`.

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
        body: "x: {C} = {v}\nprint(x)",
        reject: "cannot assign",
        tail: true,
    },
    Slot {
        name: "assign",
        decls: "",
        body: "x: {C} = {SEED}\nx = {v}\nprint(x)",
        reject: "cannot assign",
        tail: true,
    },
    Slot {
        name: "field_assign",
        decls: "struct S:\n    n: {C}\n",
        body: "s := S(n={SEED})\ns.n = {v}\nprint(s.n)",
        reject: "cannot assign",
        tail: true,
    },
    Slot {
        name: "list_index_assign",
        decls: "",
        body: "xs: List[{C}] = [{SEED}]\nxs[0] = {v}\nprint(xs[0])",
        reject: "cannot assign",
        tail: true,
    },
    Slot {
        name: "map_index_assign",
        decls: "",
        body: "m: Map[str, {C}] = {}\nm[\"k\"] = {v}\nprint(m[\"k\"])",
        reject: "cannot assign",
        tail: true,
    },
    Slot {
        name: "user_index_set",
        decls: "struct U:\n    val: {C}\n    fn index(self, k: int) -> {C}:\n        return self.val\n    fn set_index(self, k: int, v: {C}):\n        self.val = v\n",
        body: "u := U(val={SEED})\nu[0] = {v}\nprint(u[0])",
        reject: "cannot assign",
        tail: false,
    },
    Slot {
        name: "multi_assign",
        decls: "",
        body: "x: {C} = {SEED}\ny := 0\nx, y = {v}, 0\nprint(x)",
        reject: "cannot assign",
        tail: false,
    },
    Slot {
        name: "multi_index_assign",
        decls: "",
        body: "xs: List[{C}] = [{SEED}]\ny := 0\nxs[0], y = {v}, 0\nprint(xs[0])",
        reject: "cannot assign",
        tail: false,
    },
    Slot {
        name: "positional_arg",
        decls: "fn t(x: {C}):\n    print(x)\n",
        body: "t({v})",
        reject: "argument 1 of 't'",
        tail: false,
    },
    Slot {
        name: "keyword_arg",
        decls: "fn k(a: int, b: {C} = {SEED}):\n    print(b)\n",
        body: "k(0, b={v})",
        reject: "argument 2 of 'k'",
        tail: false,
    },
    Slot {
        name: "variadic_arg",
        decls: "fn va(...xs: {C}):\n    print(xs[0])\n",
        body: "va({v})",
        reject: "list element",
        tail: false,
    },
    Slot {
        name: "generic_arg",
        decls: "fn f[T](a: T, b: {C}):\n    print(b)\n",
        body: "f(0, {v})",
        reject: "argument to 'f'",
        tail: false,
    },
    Slot {
        name: "ctor_field",
        decls: "struct S:\n    n: {C}\n",
        body: "print(S(n={v}).n)",
        reject: "argument 1 of 'S'",
        tail: false,
    },
    Slot {
        name: "variant_payload",
        decls: "enum E:\n    V({C})\n",
        body: "match E.V({v}):\n    E.V(x):\n        print(x)",
        reject: "argument 1 of 'V'",
        tail: false,
    },
    Slot {
        name: "generic_variant_payload",
        decls: "enum G[T]:\n    V(T, {C})\n",
        body: "match G.V(0, {v}):\n    G.V(_, x):\n        print(x)",
        reject: "argument to 'V'",
        tail: false,
    },
    Slot {
        name: "method_arg",
        decls: "struct M:\n    k: int\n    fn put(self, x: {C}):\n        print(x)\n",
        body: "M(0).put({v})",
        reject: "argument 1 of 'put'",
        tail: false,
    },
    Slot {
        name: "native_method_arg",
        decls: "",
        body: "xs: List[{C}] = []\nxs.push({v})\nprint(xs[0])",
        reject: "argument 1 of 'push'",
        tail: false,
    },
    Slot {
        name: "generic_static_method_arg",
        decls: "struct H[T]:\n    v: T\n    fn mk(a: T, x: {C}):\n        print(x)\n",
        body: "H.mk(0, {v})",
        reject: "argument to 'mk'",
        tail: false,
    },
    Slot {
        name: "channel_send",
        decls: "",
        body: "ch := Channel[{C}](1)\nch.send({v})\nprint(ch.recv())",
        reject: "argument 1 of 'send'",
        tail: false,
    },
    Slot {
        name: "wait_send_arm",
        decls: "",
        body: "ch := Channel[{C}](1)\nwait:\n    ch.send({v}):\n        print(ch.recv())",
        reject: "argument 1 of 'send'",
        tail: false,
    },
    Slot {
        name: "shared_set",
        decls: "import Shared from std.concurrency\n",
        body: "s := Shared[{C}]({SEED})\ns.set({v})\nprint(s.get())",
        reject: "argument 1 of 'set'",
        tail: false,
    },
    Slot {
        name: "shared_ctor",
        decls: "import Shared from std.concurrency\n",
        body: "s := Shared[{C}]({v})\nprint(s.get())",
        reject: "expected element type",
        tail: false,
    },
    Slot {
        name: "rwshared_ctor",
        decls: "import RwShared from std.concurrency\n",
        body: "s := RwShared[{C}]({v})\nprint(s.get())",
        reject: "expected element type",
        tail: false,
    },
    Slot {
        name: "list_element",
        decls: "",
        body: "xs: List[{C}] = [{v}]\nprint(xs[0])",
        reject: "list element",
        tail: false,
    },
    Slot {
        name: "map_value",
        decls: "",
        body: "m: Map[str, {C}] = {\"k\": {v}}\nprint(m[\"k\"])",
        reject: "map value",
        tail: false,
    },
    Slot {
        name: "tuple_element",
        decls: "",
        body: "t: ({C}, int) = ({v}, 0)\nprint(t.0)",
        reject: "cannot assign",
        tail: false,
    },
    Slot {
        name: "list_conversion",
        decls: "",
        body: "xs := List[{C}]([{v}])\nprint(xs[0])",
        reject: "list element",
        tail: false,
    },
    Slot {
        name: "map_conversion",
        decls: "",
        body: "m := Map[str, {C}]([(\"k\", {v})])\nprint(m[\"k\"])",
        reject: "list element",
        tail: false,
    },
    Slot {
        name: "list_comprehension",
        decls: "",
        body: "xs: List[{C}] = [{v} for i in [0]]\nprint(xs[0])",
        reject: "cannot assign",
        tail: false,
    },
    Slot {
        name: "map_comprehension_value",
        decls: "",
        body: "m: Map[str, {C}] = {\"k\": {v} for i in [0]}\nprint(m[\"k\"])",
        reject: "cannot assign",
        tail: false,
    },
    Slot {
        name: "return",
        decls: "fn r() -> {C}:\n    return {v}\n",
        body: "print(r())",
        reject: "expected return type",
        tail: true,
    },
    Slot {
        name: "inline_body",
        decls: "fn r() -> {C}: {v}\n",
        body: "print(r())",
        reject: "expected return type",
        tail: false,
    },
    Slot {
        name: "yield",
        decls: "fn g() -> Iterator[{C}]:\n    yield {v}\n",
        body: "for y in g():\n    print(y)",
        reject: "expected yield type",
        tail: true,
    },
    Slot {
        name: "annotated_closure",
        decls: "",
        body: "k := fn() -> {C}: {v}\nprint(k())",
        reject: "closure body has type",
        tail: false,
    },
    Slot {
        name: "closure_in_typed_slot",
        decls: "fn g(k: fn() -> {C}):\n    print(k())\n",
        body: "g(fn(): {v})",
        reject: "argument 1 of 'g'",
        tail: false,
    },
    Slot {
        name: "param_default",
        decls: "fn dflt(x: {C} = {v}):\n    print(x)\n",
        body: "dflt()",
        reject: "default value for parameter",
        tail: false,
    },
    Slot {
        name: "field_default",
        decls: "struct D:\n    n: {C} = {v}\n",
        body: "print(D().n)",
        reject: "default value for field",
        tail: false,
    },
];

/// How a value fares at a carrier: the printed text, the slot's own rejection, or a named one.
#[derive(Clone, Copy)]
enum Out {
    Prints(&'static str),
    SlotRejects,
    Rejects(&'static str),
}

struct Carrier {
    ty: &'static str,
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
        seed: "None",
        alt: "None",
        values: [
            ("5", Out::Prints("Some(5)")),
            ("None", Out::Prints("None")),
            ("?5", Out::Prints("Some(5)")),
            ("!\"e\"", Out::SlotRejects),
            ("Some(5)", Out::Prints("Some(5)")),
            ("BR", Out::Prints("Some(5)")),
        ],
        shapes: Some(("Some(6)", "Some(5)")),
    },
    Carrier {
        ty: "int!str",
        seed: "Err(\"s\")",
        alt: "!\"e\"",
        values: [
            ("5", Out::Prints("Ok(5)")),
            ("None", Out::SlotRejects),
            ("?5", Out::Prints("Ok(5)")),
            ("!\"e\"", Out::Prints("Err('e')")),
            ("Ok(5)", Out::Prints("Ok(5)")),
            ("BR", Out::Prints("Ok(5)")),
        ],
        shapes: Some(("Ok(6)", "Ok(5)")),
    },
    Carrier {
        ty: "None!str",
        seed: "Ok()",
        alt: "!\"e\"",
        values: [
            ("5", Out::SlotRejects),
            ("None", Out::SlotRejects),
            ("?5", Out::Rejects("'?' value: expected")),
            ("!\"e\"", Out::Prints("Err('e')")),
            ("Ok()", Out::Prints("Ok(nil)")),
            ("BR", BR_INCOMPATIBLE),
        ],
        shapes: None,
    },
    Carrier {
        ty: "int??",
        seed: "None",
        alt: "None",
        values: [
            ("5", Out::SlotRejects),
            ("None", Out::Prints("None")),
            ("?5", Out::Prints("Some(Some(5))")),
            ("!\"e\"", Out::SlotRejects),
            ("Some(5)", Out::SlotRejects),
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

/// A whole program: the branch flags, the slot's declarations, and `main`.
fn program(slot: &Slot, c: &Carrier, v: &str, flags: (bool, bool)) -> String {
    let body = fill(slot.body, c, v)
        .lines()
        .map(|l| format!("    {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    let decls = fill(slot.decls, c, v);
    format!(
        "c := {}\nd := {}\n{decls}\nfn main():\n{body}\nmain()\n",
        flags.0, flags.1
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

/// A `wait` arm delivers its channel's plain element into the assigned target.
fn wait_recv_assign_cells(cells: &mut Vec<Cell>) {
    let slot = Slot {
        name: "wait_recv_assign",
        decls: "",
        body: "ch := Channel[int](1)\nch.send(5)\nx: {C} = {SEED}\nwait:\n    x = ch.recv():\n        print(x)",
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

/// No carrier is `Hashable`: set elements and map keys reject before a wrap matters.
fn hashable_cells(cells: &mut Vec<Cell>) {
    let shapes = [
        "s := Set[{C}]([{v}])",
        "s: Set[{C}] = {{v}}",
        "m: Map[{C}, str] = {{v}: \"a\"}",
        "s: Set[{C}] = {{v} for i in [0]}",
    ];
    for c in &CARRIERS[..2] {
        for v in ["5", c.values[4].0] {
            for (i, shape) in shapes.iter().enumerate() {
                let body = shape
                    .replace("{{v}", &format!("{{{v}"))
                    .replace("{C}", c.ty)
                    .replace("{v}", v);
                cells.push(main_only(
                    format!("hashable{i}/{}/{v}", c.ty),
                    format!("fn main():\n    {body}\nmain()\n"),
                    Expect::Rejects("must implement Hashable"),
                ));
            }
        }
    }
}

fn extra_cells(cells: &mut Vec<Cell>) {
    let m = |name: &str, src: &str, e: Expect| main_only(name.to_string(), format!("{src}\n"), e);
    let r = Expect::Rejects;
    cells.extend([
        m("q_default_optional", "fn main():\n    y := ?5\n    print(y)\nmain()", prints("Some(5)")),
        m("q_var_bound_unknown", "fn main():\n    y := ?5\n    ys := [y]\n    ys = []\n    print(y)\n    print(ys)\nmain()", prints("Some(5)\n[]")),
        m("q_pinned_by_result", "fn take(r: int!str):\n    print(r)\nfn main():\n    z := ?5\n    take(z)\nmain()", prints("Ok(5)")),
        m("bang_pinned_by_return", "fn f() -> int!:\n    e := !\"disk\"\n    return e\nprint(f())", prints("Err('disk')")),
        m("bang_unpinned_fn", "fn main():\n    w := !\"disk\"\n    print(w)\nmain()", r("cannot infer the success type")),
        m("bang_unpinned_top", "w := !\"disk\"\nprint(w)", r("cannot infer the success type")),
        m("bang_not_error", "fn f() -> int!:\n    return !5\nprint(f())", r("int does not satisfy Error")),
        m("bang_whole_operand", "fn wrap(s: str) -> str:\n    return s + \"!\"\nfn f() -> int!str:\n    return !wrap(\"x\")\nprint(f())", prints("Err('x!')")),
        m("bang_in_list", "rs: List[int!str] = [1, !\"disk\", 3]\nprint(rs)", prints("[Ok(1), Err('disk'), Ok(3)]")),
        m("none_bang_falls_off", "fn save(p: str) -> None!str:\n    if p == \"\":\n        return !\"empty\"\n    pass\nprint(save(\"a\"))", prints("Ok(nil)")),
        m("none_bang_is_not_a_value", "fn save() -> None!str:\n    return\nfn main():\n    x := save()?\nmain()", r("cannot be used as a value")),
        m("generic_slot_declines", "fn f[T](x: T) -> T?:\n    return x\nprint(f(1))", r("expected return type")),
        m("no_int_float", "fn f() -> float?:\n    return 1\nprint(f())", r("expected return type")),
        m("operands_never_wrap", "x: int? = 1 + 2\nprint(x)", prints("Some(3)")),
        m("carrier_payload_is_seed", "x: Option[Option[int]] = Some(5)\nprint(x)", r("cannot assign")),
        m("default_list", "fn f(xs: List[int?] = [5]):\n    print(xs)\nf()", prints("[Some(5)]")),
        m("default_provider_param", "fn g() -> int:\n    return 5\nfn f(x: int? = g()):\n    print(x)\nf()", prints("Some(5)")),
        m("default_provider_field", "fn g() -> int:\n    return 5\nstruct S:\n    n: int? = g()\nprint(S().n)", prints("Some(5)")),
        m("default_q", "fn f(x: int? = ?5):\n    print(x)\nf()", prints("Some(5)")),
        m("default_bang", "fn f(x: int!str = !\"e\"):\n    print(x)\nf()", prints("Err('e')")),
        m("interp_arg", "fn t(x: int?) -> int?:\n    return x\nprint(\"{t(5)}\")", prints("Some(5)")),
        m("pipe_arg", "fn t(x: int?) -> int?:\n    return x\nprint(5 |> t())", prints("Some(5)")),
        m("coalesce_option", "o: int? = Some(5)\nx: int? = o ?? 0\nprint(x)", prints("Some(5)")),
        m("coalesce_result", "o: int? = Some(5)\nx: int!str = o ?? 0\nprint(x)", prints("Ok(5)")),
        m("coalesce_none_arm", "fn f(o: int?) -> int?:\n    return o ?? None\nprint(f(None))", r("branches have incompatible types")),
        m("comprehension_barrier", "ys: List[int] = [y for xs in [[1, 2], [3]] for y in xs]\nprint(ys)", prints("[1, 2, 3]")),
        m("inferred_return_not_a_slot", "fn f(c: bool):\n    if c:\n        return Some(2)\n    return if c: 1 else: None\nprint(f(true))", r("branches have incompatible types")),
        m("tuple_call_does_not_split", "fn g() -> (int, int):\n    return (5, 0)\nx: int? = None\ny := 0\nx, y = g()\nprint(x)", r("cannot assign")),
    ]);
    // A default compiles as the declaration's node: called across a module boundary too.
    cells.push(cell(
        "default_list_from_lib".to_string(),
        vec![
            (
                "lib.chz",
                "fn f(xs: List[int?] = [5]):\n    print(xs)\n".to_string(),
            ),
            ("main.chz", "import lib\nlib.f()\n".to_string()),
        ],
        prints("[Some(5)]"),
    ));
    cells.push(cell(
        "default_from_lib".to_string(),
        vec![
            ("lib.chz", "fn f(x: int? = 5):\n    print(x)\n".to_string()),
            ("main.chz", "import lib\nlib.f()\n".to_string()),
        ],
        prints("Some(5)"),
    ));
}

#[test]
fn carrier_slot_grid() {
    let mut cells = Vec::new();
    slot_cells(&mut cells);
    wait_recv_assign_cells(&mut cells);
    hashable_cells(&mut cells);
    extra_cells(&mut cells);
    run_grid("carrier-slot", &cells);
}
