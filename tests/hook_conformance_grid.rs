//! TICKET-250: the hook-conformance grid. A method is the hook for an operator exactly when it
//! satisfies the builtin protocol's requirement (`Checker::hook_method`). One generated program
//! per cell, run through the built `chezzi` binary: hook x shape x owner kind x use. The operator
//! and the bound `T: P` must give the same answer in every cell, and a method that is not the
//! hook must leave every use exactly as it is for the same owner with no such method.

use std::path::PathBuf;
use std::process::Command;

#[path = "support/grid_cell.rs"]
mod grid_cell;
#[path = "support/grid_tests.rs"]
#[macro_use]
mod grid_tests;

// One test per (owner kind, hook), per hook of the conditional and declaration-verdict families,
// and per remaining row group (TICKET-276); `every_grid_hook_has_a_test` ties the hook lists to
// `hooks()`.
grid_tests!(OWNERS x HOOK_NAMES: [r#struct, r#enum, gstruct, genum, zstruct] x [iter, next, index, set_index, slice, str, eq, hash, compare, add, sub, mul, div, r#mod, neg, contains, bitand, bitor, bitxor, shl, shr] => crate::assert_owner_hook);
grid_tests!(CONDITIONAL_HOOKS in conditional: [iter, next, index, set_index, slice, str, eq, hash, compare, add, sub, mul, div, r#mod, neg, contains, bitand, bitor, bitxor, shl, shr] => crate::assert_conditional_hook);
grid_tests!(DECLARATION_VERDICT_HOOKS in declaration_verdict: [iter, next, index, set_index, slice, str, eq, hash, compare, add, sub, mul, div, r#mod, neg, contains, bitand, bitor, bitxor, shl, shr] => crate::assert_declaration_verdict_hook);

/// The grid row of a hook, by its method name.
fn hook_named(name: &str) -> Hook {
    hooks()
        .into_iter()
        .find(|hook| hook.name == name)
        .unwrap_or_else(|| panic!("no grid row for hook {name}"))
}

/// Fails with every wrong row of one family, sorted.
fn assert_rows(what: &str, mut bad: Vec<String>) {
    bad.sort();
    assert!(
        bad.is_empty(),
        "{} {what} cells wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

fn m(sig: &str, body: &str) -> String {
    format!("    fn {sig}:\n        {body}\n")
}

const SHAPES: [&str; 6] = ["conf", "gen", "static", "wrongret", "extra", "generic"];

struct Hook {
    name: &'static str,
    /// The protocol, with the arguments the conforming shape satisfies.
    proto: &'static str,
    /// The slot's type when it is not `proto`: `Iterator[T]` in type position is the cursor
    /// itself, so a `next` owner is held in an `Iterable[T]` slot.
    slot_proto: Option<&'static str>,
    /// The import line that licenses `proto` in a bound or a slot.
    import: &'static str,
    /// The import line that licenses `slot_proto`.
    slot_import: Option<&'static str>,
    /// One method text per entry of `SHAPES`.
    shapes: [String; 6],
    op: &'static str,
    slot: &'static str,
    /// The intrinsic spelling `x.m(..)`, an ordinary method call.
    intr: &'static str,
    /// `v.m(..)` on a receiver `v` typed through the protocol (a bound or a slot).
    call: &'static str,
}

/// The row of one binary arithmetic hook: `fn <name>(self, other: Self) -> Self` behind `<sym>`.
fn arith(name: &'static str, proto: &'static str, sym: &str) -> Hook {
    let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    Hook {
        name,
        proto,
        slot_proto: None,
        import: leak(format!("import {proto} from std.ops\n")),
        slot_import: None,
        shapes: [
            m(&format!("{name}(self, other: Self) -> Self"), "return self"),
            m(
                &format!("{name}(self, other: Self) -> Iterator[int]"),
                "yield 1",
            ),
            m(&format!("{name}(a: int, other: int) -> int"), "return a"),
            m(&format!("{name}(self, other: Self) -> int"), "return 1"),
            m(
                &format!("{name}(self, other: Self, z: int) -> Self"),
                "return self",
            ),
            m(
                &format!("{name}[U](self, other: Self) -> Self"),
                "return self",
            ),
        ],
        op: leak(format!("z := x {sym} y\nprint(\"applied\")\n")),
        slot: "print(\"slot\")",
        intr: leak(format!("z := x.{name}(y)\nprint(\"applied\")\n")),
        call: leak(format!("z := v.{name}(v)\n    print(\"applied\")")),
    }
}

/// One row per entry of `chezzi::base::hooks::HOOKS`; `every_hook_row_has_a_grid_row` pins that.
fn hooks() -> Vec<Hook> {
    let idx = m("index(self, k: int) -> int", "return self.n");
    let eq = m("eq(self, other: Self) -> bool", "return true");
    vec![
        Hook {
            name: "iter",
            proto: "Iterable[int]",
            slot_proto: None,
            import: "import Iterable from std.iter\n",
            slot_import: None,
            shapes: [
                m("iter(self) -> Iterator[int]", "return [1, 2].iter()"),
                m("iter(self) -> Iterator[int]", "yield 1\n        yield 2"),
                m("iter(a: int) -> Iterator[int]", "return [a].iter()"),
                m("iter(self) -> int", "return 1"),
                m("iter(self, z: int) -> Iterator[int]", "return [z].iter()"),
                m("iter[U](self) -> Iterator[int]", "return [1].iter()"),
            ],
            op: "for q in x:\n    print(q)\n",
            slot: "for q in v:\n        print(q)",
            intr: "print(List.from_iter(x.iter()))\n",
            call: "print(List.from_iter(v.iter()))",
        },
        Hook {
            name: "next",
            proto: "Iterator[int]",
            slot_proto: Some("Iterable[int]"),
            import: "import Iterator from std.iter\n",
            slot_import: Some("import Iterable from std.iter\n"),
            shapes: [
                m("next(self) -> int?", "return None"),
                m("next(self) -> Iterator[int]", "yield 1"),
                m("next(a: int) -> int?", "return None"),
                m("next(self) -> int", "return 1"),
                m("next(self, z: int) -> int?", "return None"),
                m("next[U](self) -> int?", "return None"),
            ],
            op: "for q in x:\n    print(q)\nprint(\"done\")\n",
            slot: "for q in v:\n        print(q)\n    print(\"done\")",
            intr: "print(x.next())\n",
            call: "print(v.next())",
        },
        Hook {
            name: "index",
            proto: "Index[int, int]",
            slot_proto: None,
            import: "import Index from std.ops\n",
            slot_import: None,
            shapes: [
                m("index(self, k: int) -> int", "return k * 2"),
                m("index(self, k: int) -> Iterator[int]", "yield k"),
                m("index(a: int, k: int) -> int", "return a + k"),
                m("index(self, k: int) -> str", "return \"s\""),
                m("index(self, k: int, z: int) -> int", "return k"),
                m("index[U](self, k: int) -> int", "return k"),
            ],
            op: "print(x[2])\n",
            slot: "print(v[2])",
            intr: "print(x.index(2))\n",
            call: "print(v.index(2))",
        },
        Hook {
            name: "slice",
            proto: "Slice[int]",
            slot_proto: None,
            import: "import Slice from std.ops\n",
            slot_import: None,
            shapes: [
                m("slice(self, r: Range) -> int", "return 7"),
                m("slice(self, r: Range) -> Iterator[int]", "yield 7"),
                m("slice(r: Range) -> int", "return 7"),
                m("slice(self, r: Range) -> str", "return \"s\""),
                m("slice(self, r: Range, z: int) -> int", "return 7"),
                m("slice[U](self, r: Range) -> int", "return 7"),
            ],
            op: "print(x[1..2])\n",
            slot: "print(v[1..2])",
            intr: "print(x.slice(1..2))\n",
            call: "print(v.slice(1..2))",
        },
        Hook {
            name: "set_index",
            proto: "IndexSet[int, int]",
            slot_proto: None,
            import: "import IndexSet from std.ops\n",
            slot_import: None,
            shapes: [
                idx.clone() + &m("set_index(self, k: int, v: int)", "self.n = v"),
                idx.clone()
                    + &m(
                        "set_index(self, k: int, v: int)",
                        "self.n = v\n        yield 1",
                    ),
                idx.clone()
                    + &m(
                        "set_index(a: int, k: int, v: int)",
                        "print(\"static set\", a)",
                    ),
                idx.clone()
                    + &m(
                        "set_index(self, k: int, v: int) -> int",
                        "self.n = v\n        return 1",
                    ),
                idx.clone() + &m("set_index(self, k: int, v: int, z: int)", "self.n = v"),
                idx.clone() + &m("set_index[U](self, k: int, v: int)", "self.n = v"),
            ],
            op: "x[0] = 9\nprint(\"set\")\n",
            slot: "v[0] = 9\n    print(\"set\")",
            intr: "x.set_index(0, 9)\nprint(\"set\")\n",
            call: "v.set_index(0, 9)\n    print(\"set\")",
        },
        Hook {
            name: "str",
            proto: "Stringable",
            slot_proto: None,
            import: "import Stringable from std.string\n",
            slot_import: None,
            shapes: [
                m("str(self) -> str", "return \"HOOK\""),
                m("str(self) -> Iterator[str]", "yield \"G\""),
                m("str(a: int) -> str", "return \"STATIC\""),
                m("str(self) -> int", "return 5"),
                m("str(self, z: int) -> str", "return \"X\""),
                m("str[U](self) -> str", "return \"GEN\""),
            ],
            op: "print(x)\nprint(\"{x}\")\n",
            slot: "print(v)",
            intr: "print(x.str())\n",
            call: "print(v.str())",
        },
        Hook {
            name: "eq",
            proto: "Eq",
            slot_proto: None,
            import: "import Eq from std.cmp\n",
            slot_import: None,
            shapes: [
                m(
                    "eq(self, other: Self) -> bool",
                    "print(\"HOOK\")\n        return true",
                ),
                m("eq(self, other: Self) -> Iterator[bool]", "yield true"),
                m("eq(a: int, other: int) -> bool", "return true"),
                m("eq(self, other: Self) -> int", "return 1"),
                m("eq(self, other: Self, z: int) -> bool", "return true"),
                m("eq[U](self, other: Self) -> bool", "return true"),
            ],
            op: "print(x == y)\n",
            slot: "print(v == v)",
            intr: "print(x.eq(y))\n",
            call: "print(v.eq(v))",
        },
        Hook {
            name: "hash",
            proto: "Hashable",
            slot_proto: None,
            import: "import Hashable from std.cmp\n",
            slot_import: None,
            shapes: [
                m("hash(self) -> int", "return 1") + &eq,
                m("hash(self) -> Iterator[int]", "yield 1") + &eq,
                m("hash(a: int) -> int", "return 1") + &eq,
                m("hash(self) -> str", "return \"h\"") + &eq,
                m("hash(self, z: int) -> int", "return 1") + &eq,
                m("hash[U](self) -> int", "return 1") + &eq,
            ],
            op: "s := Set.from_iter([x, y])\nprint(s.len())\n",
            slot: "print(\"slot\")",
            intr: "print(x.hash())\n",
            call: "print(v.hash())",
        },
        Hook {
            name: "compare",
            proto: "Comparable",
            slot_proto: None,
            import: "import Comparable from std.cmp\n",
            slot_import: None,
            shapes: [
                m("compare(self, other: Self) -> int", "return 0 - 1"),
                m("compare(self, other: Self) -> Iterator[int]", "yield 1"),
                m("compare(a: int, other: int) -> int", "return 0 - 1"),
                m("compare(self, other: Self) -> bool", "return true"),
                m("compare(self, other: Self, z: int) -> int", "return 0 - 1"),
                m("compare[U](self, other: Self) -> int", "return 0 - 1"),
            ],
            op: "print(x < y)\n",
            slot: "print(\"slot\")",
            intr: "print(x.compare(y))\n",
            call: "print(v.compare(v))",
        },
        arith("add", "Add", "+"),
        arith("sub", "Sub", "-"),
        arith("mul", "Mul", "*"),
        arith("div", "Div", "/"),
        arith("mod", "Mod", "%"),
        arith("bitand", "BitAnd", "&"),
        arith("bitor", "BitOr", "|"),
        arith("bitxor", "BitXor", "^"),
        arith("shl", "Shl", "<<"),
        arith("shr", "Shr", ">>"),
        Hook {
            name: "neg",
            proto: "Neg",
            slot_proto: None,
            import: "import Neg from std.ops\n",
            slot_import: None,
            shapes: [
                m("neg(self) -> Self", "return self"),
                m("neg(self) -> Iterator[int]", "yield 1"),
                m("neg(a: int) -> int", "return 0 - a"),
                m("neg(self) -> int", "return 1"),
                m("neg(self, z: int) -> Self", "return self"),
                m("neg[U](self) -> Self", "return self"),
            ],
            op: "z := -x\nprint(\"negged\")\n",
            slot: "print(\"slot\")",
            intr: "z := x.neg()\nprint(\"negged\")\n",
            call: "z := v.neg()\n    print(\"negged\")",
        },
        Hook {
            name: "contains",
            proto: "Contains[int]",
            slot_proto: None,
            import: "import Contains from std.ops\n",
            slot_import: None,
            shapes: [
                m("contains(self, item: int) -> bool", "return item == 1"),
                m("contains(self, item: int) -> Iterator[bool]", "yield true"),
                m("contains(a: int, item: int) -> bool", "return true"),
                m("contains(self, item: int) -> int", "return 1"),
                m("contains(self, item: int, z: int) -> bool", "return true"),
                m("contains[U](self, item: int) -> bool", "return item == 1"),
            ],
            op: "print(1 in x)\n",
            slot: "print(1 in v)",
            intr: "print(x.contains(1))\n",
            call: "print(v.contains(1))",
        },
    ]
}

/// The protocol, with arguments, that `shape` of `hook` satisfies; `None` when the shape is not
/// the hook. A generator body or another return type still conforms where the protocol leaves the
/// return to a type argument (`Index[K, V]`, `Slice[R]`, `Iterable[T]`).
fn shape_proto(hook: &Hook, shape: &str) -> Option<&'static str> {
    match (hook.name, shape) {
        (_, "conf") | ("iter", "gen") => Some(hook.proto),
        ("index", "gen") => Some("Index[int, Iterator[int]]"),
        ("index", "wrongret") => Some("Index[int, str]"),
        ("slice", "gen") => Some("Slice[Iterator[int]]"),
        ("slice", "wrongret") => Some("Slice[str]"),
        _ => None,
    }
}

/// Does `shape` of `hook` satisfy the protocol requirement? `None`: the grid asserts agreement
/// only (a `set_index` generator has no declared return).
fn conforms(hook: &Hook, shape: &str) -> Option<bool> {
    match (hook.name, shape) {
        ("set_index", "gen") => None,
        _ => Some(shape_proto(hook, shape).is_some()),
    }
}

/// The declaration of owner `kind` with `methods`, and the bindings `x` and `y`.
fn owner(kind: &str, methods: &str) -> String {
    let fieldless = methods
        .replace("self.n = v", "print(\"set\", v)")
        .replace("return self.n", "return 3");
    match kind {
        "struct" => format!("struct X:\n    n: int\n{methods}x := X(1)\ny := X(2)\n"),
        "enum" => format!("enum X:\n    A(int)\n{fieldless}x := X.A(1)\ny := X.A(2)\n"),
        "gstruct" => format!("struct X[T]:\n    n: T\n{fieldless}x := X(1)\ny := X(2)\n"),
        "genum" => {
            format!("enum X[T]:\n    A(T)\n{fieldless}x: X[int] = X.A(1)\ny: X[int] = X.A(2)\n")
        }
        _ => format!("struct X:\n{fieldless}x := X()\ny := X()\n"),
    }
}

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("chezzi-hook-grid-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("create the grid directory");
    d
}

/// One run of a cell: did it exit with status zero, its stdout, and everything it printed.
#[derive(Clone, PartialEq)]
struct Outcome {
    ok: bool,
    stdout: String,
    text: String,
}

fn chezzi(sub: &str, name: &str, src: &str, threads: Option<&str>) -> Outcome {
    let path = dir().join(format!("{name}.chz"));
    std::fs::write(&path, src).expect("write the cell");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.arg(sub).arg(&path);
    match threads {
        Some(n) => cmd.env("CHEZZI_THREADS", n),
        None => cmd.env_remove("CHEZZI_THREADS"),
    };
    let out = cmd.output().expect("run chezzi");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let text = format!("{stdout}{}", String::from_utf8_lossy(&out.stderr));
    Outcome {
        ok: out.status.success(),
        stdout,
        text,
    }
}

/// `chezzi check <src>`: did it check clean, and everything it printed.
fn check(name: &str, src: &str) -> (bool, String) {
    let out = chezzi("check", name, src, None);
    (out.ok, out.text)
}

/// `chezzi run <src>` at `CHEZZI_THREADS` 1, 2 and default. The three runs must agree; a
/// disagreement is reported in `bad` and the first run is returned.
fn run(name: &str, src: &str, bad: &mut Vec<String>) -> Outcome {
    let src = format!("{src}print(\"END\")\n");
    let first = chezzi("run", name, &src, Some("1"));
    for threads in [Some("2"), None] {
        let again = chezzi("run", name, &src, threads);
        if again.ok != first.ok || again.stdout != first.stdout {
            bad.push(format!(
                "{name}: CHEZZI_THREADS=1 and {threads:?} disagree: {:?} vs {:?}",
                first.stdout, again.stdout
            ));
        }
    }
    // D1: a run that exits with status zero must reach its last statement.
    if first.ok && !first.stdout.ends_with("END\n") {
        bad.push(format!(
            "{name}: exits with status zero before its last statement: {:?}",
            first.stdout
        ));
    }
    first
}

const ITERATOR_IMPORT: &str = "import Iterator from std.iter\n";

/// The import lines of a program that spells `texts`: `import` licenses the hook's protocol, and
/// `Iterator` is added when a text spells the generator type.
fn import_lines(import: &str, texts: &[&str]) -> String {
    let iterator = !import.contains("Iterator") && texts.iter().any(|t| t.contains("Iterator["));
    format!("{import}{}", if iterator { ITERATOR_IMPORT } else { "" })
}

/// The uses of one owner: operator, bound `T: P`, protocol-typed slot, `v.m(..)` in a bounded
/// body, `v.m(..)` through a slot. Each program starts with the imports it spells.
fn uses(hook: &Hook, proto: &str, decl: &str) -> [(&'static str, String); 5] {
    let slot_ty = hook.slot_proto.unwrap_or(proto);
    let slot_import = hook.slot_import.unwrap_or(hook.import);
    let bound_head = import_lines(hook.import, &[decl, proto]);
    let slot_head = import_lines(slot_import, &[decl, slot_ty]);
    [
        (
            "bbody",
            format!(
                "{bound_head}{decl}fn bb[C: {proto}](v: C):\n    {}\nbb(x)\n",
                hook.call
            ),
        ),
        (
            "sbody",
            format!(
                "{slot_head}{decl}fn sb(v: {slot_ty}):\n    {}\nsb(x)\n",
                hook.call
            ),
        ),
        (
            "op",
            format!("{}{decl}{}", import_lines("", &[decl]), hook.op),
        ),
        (
            "bound",
            format!("{bound_head}{decl}fn b[C: {proto}](c: C):\n    print(\"bound ok\")\nb(x)\n"),
        ),
        (
            "slot",
            format!(
                "{slot_head}{decl}fn s(v: {slot_ty}):\n    {}\ns(x)\n",
                hook.slot
            ),
        ),
    ]
}

fn cell_failures(kind: &str, hook: &Hook, si: usize) -> Vec<String> {
    let shape = SHAPES[si];
    let mut bad = Vec::new();
    let verdict = conforms(hook, shape);
    let proto = shape_proto(hook, shape).unwrap_or(hook.proto);
    let decl = owner(kind, &hook.shapes[si]);
    // The same owner with no method of the hook's name.
    let renamed = hook.shapes[si]
        .replace(
            &format!("fn {}(", hook.name),
            &format!("fn zz_{}(", hook.name),
        )
        .replace(
            &format!("fn {}[", hook.name),
            &format!("fn zz_{}[", hook.name),
        );
    let none = owner(kind, &renamed);
    let cell = format!("{kind}|{}|{shape}", hook.name);
    // A requirement that names `Self` in an operand cannot be called through a slot.
    let self_operand = hook.shapes[0].contains("other: Self");
    let mut answers = Vec::new();
    for ((u, src), (_, src_none)) in uses(hook, proto, &decl)
        .into_iter()
        .zip(uses(hook, proto, &none))
    {
        let tag = format!("{kind}_{}_{shape}_{u}", hook.name);
        let got = run(&tag, &src, &mut bad);
        if matches!(u, "op" | "bound" | "slot") {
            answers.push((u, got.ok));
        }
        match verdict {
            Some(true) if u == "sbody" && self_operand => {}
            // `Iterator[T]` in type position is the cursor itself; no slot holds a `next` owner
            // and exposes `next`.
            _ if u == "sbody" && hook.name == "next" => {}
            Some(true) if !got.ok => bad.push(format!(
                "{cell}|{u}: a conforming hook is rejected: {}",
                got.text
            )),
            Some(true)
                if matches!(hook.name, "str" | "eq")
                    && u == "op"
                    && !got.stdout.contains("HOOK") =>
            {
                bad.push(format!("{cell}|{u}: the hook did not run: {}", got.text))
            }
            Some(false) => {
                let want = run(&format!("{tag}_none"), &src_none, &mut bad);
                if got.ok != want.ok || got.stdout != want.stdout {
                    bad.push(format!(
                        "{cell}|{u}: a non-hook gives {:?} (ok={}), the owner with no such method {:?} (ok={}): {}",
                        got.stdout, got.ok, want.stdout, want.ok, got.text
                    ));
                }
            }
            _ => {}
        }
    }
    // `print` accepts every value (default display), so for `str` the operator and the bound
    // agree on WHICH method runs, checked above, not on acceptance. `==` is total the same way
    // (structural), and there the three uses do agree.
    if hook.name != "str" && answers.iter().any(|(_, ok)| *ok != answers[0].1) {
        bad.push(format!("{cell}: the uses disagree: {answers:?}"));
    }
    // The intrinsic spelling is an ordinary method call, whatever the shape.
    let intr = run(
        &format!("{kind}_{}_{shape}_intr", hook.name),
        &format!("{}{decl}{}", import_lines("", &[&decl]), hook.intr),
        &mut bad,
    );
    let intr_ok = match shape {
        "conf" => intr.ok,
        "static" => intr.text.contains("is a static method of"),
        "extra" => intr.text.contains("expects"),
        _ => true,
    };
    if !intr_ok {
        bad.push(format!("{cell}|intr: {}", intr.text));
    }
    bad
}

/// Run-time rows beside the grid: `.iter()` typed through `Iterable` reads the hook table, and
/// never finds a declared method by name alone.
fn iter_run_failures() -> Vec<String> {
    let mut bad = Vec::new();
    let next = m("next(self) -> int?", "return None");
    let static_iter = m(
        "iter(k: int) -> int",
        "print(\"static iter ran\")\n        return k",
    );
    let str_iter = m(
        "iter(self) -> Iterator[str]",
        "return [\"a\", \"b\"].iter()",
    );
    let through = |elem: &str| {
        format!(
            "fn g(s: Iterable[{elem}]) -> List[{elem}]:\n    return List.from_iter(s.iter())\nfn b[S: Iterable[{elem}]](s: S) -> List[{elem}]:\n    return List.from_iter(s.iter())\nprint(g(c))\nprint(b(c))\n"
        )
    };
    let iterable = "import Iterable from std.iter\n";
    let both_imports = "import Iterator, Iterable from std.iter\n";
    for (kind, head, mk) in [
        ("struct", "struct C:\n    n: int\n", "c := C(2)\n"),
        ("enum", "enum C:\n    A(int)\n", "c := C.A(2)\n"),
    ] {
        for (row, imports, methods, body, want) in [
            (
                "next",
                iterable,
                next.clone(),
                through("int"),
                Some("[]\n[]\nEND\n"),
            ),
            (
                "next_static_iter",
                iterable,
                next.clone() + &static_iter,
                through("int"),
                Some("[]\n[]\nEND\n"),
            ),
            (
                "static_next",
                iterable,
                m("next(a: int) -> int?", "return None"),
                through("int"),
                None,
            ),
            (
                "next_str_iter",
                both_imports,
                next.clone() + &str_iter,
                through("str"),
                Some("['a', 'b']\n['a', 'b']\nEND\n"),
            ),
            (
                "concrete_both",
                ITERATOR_IMPORT,
                next.clone() + &str_iter,
                "print(List.from_iter(c.iter()))\n".to_string(),
                Some("['a', 'b']\nEND\n"),
            ),
        ] {
            let got = run(
                &format!("iter_{kind}_{row}"),
                &format!("{imports}{head}{methods}{mk}{body}"),
                &mut bad,
            );
            match want {
                Some(out) if got.stdout != out => bad.push(format!(
                    "s.iter()|{kind}|{row}: expected {out:?}, got: {}",
                    got.text
                )),
                None if got.ok => bad.push(format!(
                    "s.iter()|{kind}|{row}: a static `next` is iterable: {}",
                    got.text
                )),
                _ => {}
            }
        }
    }
    let slot = "import Iterable from std.iter\nfn g(s: Iterable[int]) -> List[int]:\n    return List.from_iter(s.iter())\nprint(g([1, 2]))\nprint(g(Set.from_iter([3])))\n";
    let got = run("iter_slot_builtin", slot, &mut bad);
    if got.stdout != "[1, 2]\n[3]\nEND\n" {
        bad.push(format!(
            "s.iter() on a builtin in an Iterable[int] slot: {}",
            got.text
        ));
    }
    bad
}

/// Check-time rows beside the grid: `.iter()` on a concrete declared receiver is an ordinary
/// method call, and on an `Iterable[T]` slot it is the protocol's requirement.
fn iter_call_failures() -> Vec<String> {
    let mut bad = Vec::new();
    let next = m("next(self) -> int?", "return None");
    let static_next = m("next(a: int) -> int?", "return None");
    let both = next.clone()
        + &m(
            "iter(self) -> Iterator[str]",
            "return [\"a\", \"b\"].iter()",
        );
    for (kind, head, mk) in [
        ("struct", "struct C:\n    n: int\n", "c := C(2)\n"),
        ("enum", "enum C:\n    A(int)\n", "c := C.A(2)\n"),
    ] {
        for (row, imports, methods, body, needle) in [
            (
                "next_only",
                "",
                &next,
                "it := c.iter()\nprint(it)\n",
                "has no method 'iter'",
            ),
            (
                "static_next",
                "",
                &static_next,
                "it := c.iter()\nprint(it)\n",
                "has no method 'iter'",
            ),
            (
                "both",
                ITERATOR_IMPORT,
                &both,
                "n: int? = c.iter().next()\nprint(n)\n",
                "cannot assign str? to variable of type int?",
            ),
        ] {
            let (ok, text) = check(
                &format!("conc_{kind}_{row}"),
                &format!("{imports}{head}{methods}{mk}{body}"),
            );
            if ok || !text.contains(needle) {
                bad.push(format!(
                    "concrete c.iter()|{kind}|{row}: expected {needle:?}, got: {text}"
                ));
            }
        }
    }
    let slot = "import Iterable from std.iter\nfn g(s: Iterable[int]) -> List[int]:\n    return List.from_iter(s.iter())\nprint(g([1, 2]))\nprint(g(Set.from_iter([3])))\n";
    let (ok, text) = check("slot_builtin", slot);
    if !ok {
        bad.push(format!("s.iter() on an Iterable[int] slot: {text}"));
    }
    bad
}

/// Rows beside the grid for `eq` and for a conditional hook.
fn eq_failures() -> Vec<String> {
    let mut bad = Vec::new();
    let user = |operand: &str| {
        format!(
            "protocol MyEq:\n    fn eq(self, {operand}: Self) -> bool\nstruct P:\n    v: int\n    fn eq(self, {operand}: P) -> bool:\n        return true\nfn f[T: MyEq](a: T, b: T) -> bool:\n    return a.eq(b)\nprint(f(P(1), P(2)), P(1) == P(2))\n"
        )
    };
    for (row, src, want) in [
        // A user protocol that declares its own `eq` dispatches `a.eq(b)` to the declared
        // method. `==` on a type whose `eq` misnames the operand is structural.
        ("user_protocol_eq_misnamed", user("o"), "true false\nEND\n"),
        // The same method with the operand named `other` is also the `Eq` hook.
        ("user_protocol_eq_named", user("other"), "true true\nEND\n"),
        // A conditional method is the hook for every instantiation the checker admits.
        (
            "conditional_hooks",
            "import Comparable, Eq from std.cmp\nstruct Box[T]:\n    v: T\n    fn compare(self, other: Box[T]) -> int where T: Comparable:\n        return self.v.compare(other.v)\n    fn eq(self, other: Box[T]) -> bool where T: Eq:\n        print(\"eq hook\")\n        return self.v == other.v\nprint(Box(1) < Box(2))\nprint(Box(1) == Box(1))\n"
                .to_string(),
            "true\neq hook\ntrue\nEND\n",
        ),
    ] {
        let got = run(&format!("eq_{row}"), &src, &mut bad);
        if got.stdout != want {
            bad.push(format!("eq|{row}: expected {want:?}, got: {}", got.text));
        }
    }
    // `StructuralEq` (D5): a payload reaches a generic `Atomic[T]` only through the sealed bound.
    // A type with the `eq` hook does not satisfy it. A type whose `eq` is not the hook does, and
    // then `==` and `cas` give the same answer.
    for (kind, decl, one, two) in [
        ("struct", "struct P:\n    v: int", "P(1)", "P(2)"),
        ("enum", "enum P:\n    A(int)", "P.A(1)", "P.A(2)"),
    ] {
        for (shape, sig, hook) in [
            ("conforming", "fn eq(self, other: P) -> bool", true),
            ("static", "fn eq(a: P, b: P) -> bool", false),
            ("misnamed", "fn eq(self, o: P) -> bool", false),
            ("generic", "fn eq[U](self, other: U) -> bool", false),
        ] {
            let src = format!(
                "import Atomic, StructuralEq from std.concurrency\n{decl}\n    {sig}:\n        return true\nfn mk[T: StructuralEq](v: T) -> Atomic[T]:\n    return Atomic(v)\na := mk({one})\nprint(a.load() == {two}, a.cas({two}, {two}))\n"
            );
            let got = run(&format!("eq_structural_{kind}_{shape}"), &src, &mut bad);
            let right = if hook {
                !got.ok && got.text.contains("does not satisfy StructuralEq")
            } else {
                got.stdout == "false false\nEND\n"
            };
            if !right {
                bad.push(format!("eq|StructuralEq|{kind}|{shape}: {}", got.text));
            }
        }
    }
    bad
}

/// A conditional hook (`fn m(..) where T: Comparable`) x {bound satisfied, bound not satisfied}
/// x generic owner. The hook table has one bit per declared type, so a conditional method is the
/// hook of every instantiation the checker admits. An instantiation that satisfies the bound
/// behaves as the unconditional conforming shape under every use. One that does not is a check
/// error under every use, and the error names the bound; no use falls back to the hookless
/// path, because the run time cannot tell the two instantiations apart.
///
/// `str` has no unsatisfied row: `print` accepts every value and has no check-time decider.
fn conditional_hook_failures(hook: &Hook) -> Vec<String> {
    let mut bad = Vec::new();
    let unsatisfied = |src: &str| {
        // `N` goes after the program's leading import lines.
        let imports_len: usize = src
            .lines()
            .take_while(|l| l.starts_with("import "))
            .map(|l| l.len() + 1)
            .sum();
        let (imports, rest) = src.split_at(imports_len);
        format!(
            "{imports}struct N:\n    w: int\n{}",
            rest.replace("X(1)", "X(N(1))")
                .replace("X(2)", "X(N(2))")
                .replace("X.A(1)", "X.A(N(1))")
                .replace("X.A(2)", "X.A(N(2))")
                .replace("X[int]", "X[N]")
        )
    };
    for kind in ["gstruct", "genum"] {
        let plain = owner(kind, &hook.shapes[0]);
        // Every method of the shape carries the bound (`set_index` comes with an `index`).
        let cond_methods: String = hook.shapes[0]
            .lines()
            .map(|line| match line.strip_suffix(':') {
                Some(sig) if line.starts_with("    fn ") => {
                    format!("{sig} where T: Comparable:\n")
                }
                _ => format!("{line}\n"),
            })
            .collect();
        let cond = owner(kind, &cond_methods);
        let cell = format!("{kind}|{}|where", hook.name);
        for ((u, src), (_, src_plain)) in uses(hook, hook.proto, &cond)
            .into_iter()
            .zip(uses(hook, hook.proto, &plain))
        {
            // The conditional program spells `where T: Comparable`; the `op` use imports nothing.
            let imports_comparable = src
                .lines()
                .any(|l| l.starts_with("import ") && l.contains("Comparable"));
            let cmp = if imports_comparable {
                ""
            } else {
                "import Comparable from std.cmp\n"
            };
            let src = format!("{cmp}{src}");
            let tag = format!("{kind}_{}_where_{u}", hook.name);
            let got = run(&format!("{tag}_sat"), &src, &mut bad);
            let want = run(&format!("{tag}_plain"), &src_plain, &mut bad);
            if got.ok != want.ok || got.stdout != want.stdout {
                bad.push(format!(
                        "{cell}|{u}: a satisfied bound gives {:?} (ok={}), the unconditional hook {:?} (ok={}): {}",
                        got.stdout, got.ok, want.stdout, want.ok, got.text
                    ));
            }
            if hook.name == "str" {
                continue;
            }
            let (clean, text) = check(&format!("{tag}_unsat"), &unsatisfied(&src));
            if clean {
                bad.push(format!(
                    "{cell}|{u}: an instantiation that fails the bound checks clean"
                ));
            } else if matches!(u, "op" | "bound") && !text.contains("N: Comparable") {
                bad.push(format!(
                    "{cell}|{u}: the error does not name the bound: {text}"
                ));
            }
        }
    }
    bad
}

/// The conditional-hook rows beside the per-hook grid: a conditional `iter` beside `next`, and a
/// zero-field struct with a conditional `hash`.
fn conditional_failures() -> Vec<String> {
    let mut bad = Vec::new();
    // A type with the `next` hook and a conditional `iter` hook of another element type. A
    // satisfied bound iterates through `iter`. An unsatisfied one is not iterable: `iter` wins
    // at run time, so no consumer may read the `next` elements.
    let both = m(
        "next(self) -> int?",
        "print(\"next ran\")\n        return None",
    ) + &m(
        "iter(self) -> Iterator[str] where T: Comparable",
        "return [\"a\"].iter()",
    );
    for (kind, head, mk) in [
        ("struct", "struct X[T]:\n    n: T\n", "X"),
        ("enum", "enum X[T]:\n    A(T)\n", "X.A"),
    ] {
        let plain_imports = "import Comparable from std.cmp\nimport Iterator from std.iter\n";
        let iterable_imports =
            "import Comparable from std.cmp\nimport Iterator, Iterable from std.iter\n";
        for (consumer, imports, tail) in [
            ("for", plain_imports, "for q in x:\n    print(q)\n"),
            ("list", plain_imports, "print(List.from_iter(x))\n"),
            ("comp", plain_imports, "print([q for q in x])\n"),
            (
                "bbody",
                iterable_imports,
                "fn w[S: Iterable[str]](s: S):\n    print(List.from_iter(s.iter()))\nw(x)\n",
            ),
            (
                "bbody_int",
                iterable_imports,
                "fn w[S: Iterable[int]](s: S):\n    print(List.from_iter(s.iter()))\nw(x)\n",
            ),
            (
                "iterator_bound",
                plain_imports,
                "fn w[S: Iterator[int]](s: S):\n    for q in s:\n        print(q)\nw(x)\n",
            ),
        ] {
            let tag = format!("where_both_{kind}_{consumer}");
            let unsat =
                format!("{imports}struct N:\n    w: int\n{head}{both}x := {mk}(N(1))\n{tail}");
            let (clean, text) = check(&format!("{tag}_unsat"), &unsat);
            if clean {
                bad.push(format!(
                    "{tag}: `iter` fails its bound and the type still iterates"
                ));
            } else if matches!(consumer, "for" | "list" | "comp")
                && !text.contains("method 'iter' requires N: Comparable")
            {
                bad.push(format!("{tag}: the error does not name the bound: {text}"));
            }
            let sat = format!("{imports}{head}{both}x := {mk}(1)\n{tail}");
            let got = run(&format!("{tag}_sat"), &sat, &mut bad);
            // The two rows that ask for `int` elements are check errors: `iter` yields `str`.
            let int_row = matches!(consumer, "bbody_int" | "iterator_bound");
            let right = if int_row {
                !got.ok && !got.stdout.contains("next ran")
            } else {
                got.ok && got.stdout.contains('a') && !got.stdout.contains("next ran")
            };
            if !right {
                bad.push(format!(
                    "{tag}: a satisfied bound must iterate through `iter`: {}",
                    got.text
                ));
            }
        }
    }
    // A zero-field struct with a conditional `hash`: the conditional `hash` decides the
    // instantiation, so an unsatisfied instantiation is not `Hashable`.
    let z = "import Comparable from std.cmp\nstruct Z[T]:\n    fn hash(self) -> int where T: Comparable:\n        print(\"hash ran\")\n        return 7\n    fn eq(self, other: Self) -> bool:\n        return true\n";
    let (clean, text) = check(
        "where_zero_field_unsat",
        &format!("{z}struct N:\n    w: int\ns: Set[Z[N]] = Set()\ns.insert(Z[N]())\n"),
    );
    if clean || !text.contains("N: Comparable") {
        bad.push(format!(
            "where_zero_field_unsat: expected an error that names the bound: {text}"
        ));
    }
    let got = run(
        "where_zero_field_sat",
        &format!("{z}s: Set[Z[int]] = Set()\ns.insert(Z[int]())\nprint(s.len())\n"),
        &mut bad,
    );
    if got.stdout != "hash ran\n1\nEND\n" {
        bad.push(format!("where_zero_field_sat: {}", got.text));
    }
    bad
}

/// One verdict per (declared type, hook), decided at the declaration. A method whose signature
/// conforms for one instantiation only (its return type is the owner's `T`, or its operand
/// names `X[int]`) is the hook of no instantiation: under every use, each instantiation behaves
/// as the same owner with no such method, at check and at run. `index`, `slice` and `set_index`
/// have no row: a return type `T` binds the protocol's own argument, so it conforms at the
/// declaration, and they take no `Self` operand. `neg` and the arithmetic hooks have no
/// `ret_t` row: no `T` equals `X[T]`.
fn declaration_verdict_hook_failures(hook: &Hook) -> Vec<String> {
    let mut bad = Vec::new();
    // (variant, signature, body, the instantiation the signature conforms for, another one)
    type Inst = (&'static str, &'static str);
    let ret_t = |sig: &str, conf: Inst, other: Inst| {
        (
            "ret_t",
            sig.to_string(),
            "return self.get()".to_string(),
            conf,
            other,
        )
    };
    let operand = |sig: String, body: &str| {
        (
            "operand",
            sig,
            body.to_string(),
            ("int", "1"),
            ("str", "\"s\""),
        )
    };
    let n = hook.name;
    let variants = match n {
        "iter" => vec![ret_t(
            "iter(self) -> T",
            ("Iterator[int]", "[1, 2].iter()"),
            ("int", "1"),
        )],
        "next" => vec![ret_t("next(self) -> T", ("int?", "None"), ("int", "1"))],
        "str" => vec![ret_t("str(self) -> T", ("str", "\"s\""), ("int", "1"))],
        "hash" => vec![ret_t("hash(self) -> T", ("int", "1"), ("str", "\"s\""))],
        "contains" => vec![ret_t(
            "contains(self, item: int) -> T",
            ("bool", "true"),
            ("int", "1"),
        )],
        "eq" => vec![
            ret_t("eq(self, other: Self) -> T", ("bool", "true"), ("int", "1")),
            operand("eq(self, other: X[int]) -> bool".to_string(), "return true"),
        ],
        "compare" => vec![
            ret_t(
                "compare(self, other: Self) -> T",
                ("int", "1"),
                ("str", "\"s\""),
            ),
            operand(
                "compare(self, other: X[int]) -> int".to_string(),
                "return -1",
            ),
        ],
        "neg" => vec![operand("neg(self) -> X[int]".to_string(), "return MK(1)")],
        "add" | "sub" | "mul" | "div" | "mod" | "bitand" | "bitor" | "bitxor" | "shl" | "shr" => {
            vec![operand(
                format!("{n}(self, other: X[int]) -> X[int]"),
                "return other",
            )]
        }
        "index" | "slice" | "set_index" => Vec::new(),
        other => panic!("no declaration-verdict row for hook `{other}`"),
    };
    for (variant, sig, body, conf, other) in variants {
        for (kind, head, mk) in [
            (
                "gstruct",
                "struct X[T]:\n    n: T\n    fn get(self) -> T:\n        return self.n\n",
                "X",
            ),
            (
                "genum",
                "enum X[T]:\n    A(T)\n    fn get(self) -> T:\n        match self:\n            X.A(v): return v\n",
                "X.A",
            ),
        ] {
            let method = m(&sig, &format!("print(\"HOOK RAN\")\n        {body}")).replace("MK", mk);
            for (inst, (ty, arg)) in [("conf", conf), ("other", other)] {
                let vals = format!("x: X[{ty}] = {mk}({arg})\ny: X[{ty}] = {mk}({arg})\n");
                let with = format!("{head}{method}{vals}");
                let without = format!("{head}{vals}");
                for ((u, src), (_, src_none)) in uses(hook, hook.proto, &with)
                    .into_iter()
                    .zip(uses(hook, hook.proto, &without))
                {
                    let tag = format!("decl_{kind}_{n}_{variant}_{inst}_{u}");
                    let cell = format!("{kind}|{n}|{variant}|{inst}|{u}");
                    let (clean, text) = check(&tag, &src);
                    let (clean_none, _) = check(&format!("{tag}_none"), &src_none);
                    let got = run(&tag, &src, &mut bad);
                    let want = run(&format!("{tag}_none"), &src_none, &mut bad);
                    if clean != clean_none || got.ok != want.ok || got.stdout != want.stdout {
                        bad.push(format!(
                            "{cell}: check clean={clean}, run ok={}, {:?}; the owner with no \
                                 such method: check clean={clean_none}, run ok={}, {:?}: {text}{}",
                            got.ok, got.stdout, want.ok, want.stdout, got.text
                        ));
                    }
                }
            }
        }
    }
    bad
}

/// The review's named programs for the declaration verdict, each at check and at run.
fn declaration_verdict_named_failures() -> Vec<String> {
    let mut bad = Vec::new();
    // The review's named programs, each at check and at run.
    for (name, src, want) in [
        (
            "G",
            "struct Key[T]:\n    v: T\n    fn hash(self) -> T:\n        return self.v\ns: Set[Key[int]] = Set()\ns.insert(Key(1))\nprint(s.len())\n",
            "Set element type must implement Hashable",
        ),
        (
            "D",
            "struct Box[T]:\n    v: T\n    fn compare(self, other: Box[int]) -> int: return -1\nprint(Box(1) < Box(2))\n",
            "cannot",
        ),
        (
            "E",
            "struct Box[T]:\n    v: T\n    fn add(self, other: Box[int]) -> Box[T]: return self\nprint(Box(1) + Box(2))\n",
            "cannot apply +",
        ),
        (
            "F",
            "struct Box[T]:\n    v: T\n    fn next(self) -> str?:\n        return None\n    fn iter(self) -> T:\n        return self.v\nfor q in Box([1, 2].iter()):\n    print(q + 1)\n",
            "cannot apply + to str and int",
        ),
    ] {
        let (clean, text) = check(&format!("decl_named_{name}"), src);
        let got = run(&format!("decl_named_{name}"), src, &mut bad);
        if clean || got.ok || !text.contains(want) {
            bad.push(format!(
                "decl_named_{name}: expected the check error `{want}` and no run: {text}{}",
                got.text
            ));
        }
    }
    bad
}

/// The `Iterator[T]` bound has one element type for a type: `s.next()`, `for q in s`, `List(s)`
/// and the recovery of `T` all read it. A declared type whose `next` and `iter` hooks yield
/// different element types does not satisfy `Iterator` (it still satisfies `Iterable`).
/// Grid: both hooks x {same element type, another} x bound x use x owner, at check and at run.
fn iterator_bound_failures() -> Vec<String> {
    let mut bad = Vec::new();
    // (owner kind, head, `next` element as declared, body of `next`, body of a same-type `iter`,
    //  the instantiations: (element type, value, printed element))
    type Inst = (&'static str, &'static str, &'static str);
    type Owner = (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static [Inst],
    );
    let owners: [Owner; 3] = [
        (
            "struct",
            "struct Both:\n    n: int\n",
            "int",
            "return ?self.n",
            "return [self.n].iter()",
            &[("int", "Both(5)", "5")],
        ),
        (
            "enum",
            "enum Both:\n    A(int)\n",
            "int",
            "return ?5",
            "return [5].iter()",
            &[("int", "Both.A(5)", "5")],
        ),
        (
            "gstruct",
            "struct Both[T]:\n    n: T\n",
            "T",
            "return ?self.n",
            "return [self.n].iter()",
            &[("int", "Both(5)", "5"), ("str", "Both(\"s\")", "'s'")],
        ),
    ];
    for (kind, head, next_elem, next_body, iter_body, insts) in owners {
        for same in [true, false] {
            let iter = if same {
                m(&format!("iter(self) -> Iterator[{next_elem}]"), iter_body)
            } else {
                m(
                    "iter(self) -> Iterator[str]",
                    "return [\"a\", \"b\"].iter()",
                )
            };
            let next = m(&format!("next(self) -> {next_elem}?"), next_body);
            for &(elem, value, shown) in insts {
                for bound in ["T", "int", "str"] {
                    let (params, e) = match bound {
                        "T" => ("[S: Iterator[T], T]".to_string(), "T"),
                        b => (format!("[S: Iterator[{b}]]"), b),
                    };
                    let uses = [
                        (
                            "next",
                            format!(
                                "fn f{params}(s: S) -> {e}?:\n    return s.next()\nmatch f({value}):\n    ?v: print([v])\n    None: print(\"none\")\n"
                            ),
                        ),
                        (
                            "for",
                            format!(
                                "fn f{params}(s: S) -> List[{e}]:\n    out: List[{e}] = []\n    for q in s:\n        out.push(q)\n    return out\nprint(f({value}))\n"
                            ),
                        ),
                        (
                            "list",
                            format!(
                                "fn f{params}(s: S) -> List[{e}]:\n    return List.from_iter(s)\nprint(f({value}))\n"
                            ),
                        ),
                    ];
                    for (u, f) in uses {
                        let which = if same { "same" } else { "other" };
                        let tag = format!("itb_{kind}_{which}_{elem}_{bound}_{u}");
                        let src = format!("{ITERATOR_IMPORT}{head}{next}{iter}{f}");
                        let (clean, text) = check(&tag, &src);
                        let got = run(&tag, &src, &mut bad);
                        // The two hooks disagree at the DECLARATION -> the one error that names
                        // both element types; else the bound's argument must be the element.
                        let want = if !same {
                            Err(format!(
                                "does not satisfy Iterator ('next' yields {next_elem}, 'iter' yields str)"
                            ))
                        } else if bound != "T" && bound != elem {
                            Err("does not match the declared element type".to_string())
                        } else {
                            Ok(format!("[{shown}]\nEND\n"))
                        };
                        let wrong = match &want {
                            Ok(out) => !clean || !got.ok || got.stdout != *out,
                            // A conflict is the ONE error of its program.
                            Err(msg) => {
                                clean
                                    || got.ok
                                    || !text.contains(msg.as_str())
                                    || (!same && !text.contains("chezzi: 1 type error"))
                            }
                        };
                        if wrong {
                            bad.push(format!(
                                "{tag}: expected {want:?}; check clean={clean}, run ok={}, {:?}: {text}",
                                got.ok, got.stdout
                            ));
                        }
                    }
                }
            }
        }
    }
    // The review's named programs. `Both`: `next` yields `int`, `iter` yields `str`.
    let both = "struct Both:\n    n: int\n    fn next(self) -> int?:\n        if self.n <= 0:\n            return None\n        self.n -= 1\n        return ?self.n\n    fn iter(self) -> Iterator[str]:\n        return [\"a\", \"b\"].iter()\n";
    let one_error = "does not satisfy Iterator ('next' yields int, 'iter' yields str)";
    for (name, imports, tail, want) in [
        // `iter` wins for `for` and `List()`, and the type still satisfies `Iterable[str]`.
        (
            "N0",
            "import Iterator, Iterable from std.iter\n",
            "for q in Both(2):\n    print(q)\nprint(List.from_iter(Both(2)))\nfn g[S: Iterable[T], T](s: S) -> List[T]:\n    return List.from_iter(s)\nprint(g(Both(2)))\n",
            Ok("a\nb\n['a', 'b']\n['a', 'b']\nEND\n"),
        ),
        (
            "N1",
            ITERATOR_IMPORT,
            "fn first[S: Iterator[T], T](s: S) -> T?:\n    return s.next()\nr := first(Both(2))\nmatch r:\n    ?v: print(v + \"!\")\n    None: print(\"none\")\n",
            Err(one_error),
        ),
        (
            "N2",
            ITERATOR_IMPORT,
            "fn w[S: Iterator[int]](s: S) -> int?:\n    return s.next()\nprint(w(Both(2)))\n",
            Err(one_error),
        ),
        (
            "N3",
            ITERATOR_IMPORT,
            "fn w[S: Iterator[str]](s: S):\n    for q in s:\n        print(q + \"!\")\nw(Both(2))\n",
            Err(one_error),
        ),
    ] {
        let tag = format!("itb_named_{name}");
        let src = format!("{imports}{both}{tail}");
        let (clean, text) = check(&tag, &src);
        let got = run(&tag, &src, &mut bad);
        let wrong = match want {
            Ok(out) => !clean || !got.ok || got.stdout != out,
            Err(msg) => {
                clean || got.ok || !text.contains(msg) || !text.contains("chezzi: 1 type error")
            }
        };
        if wrong {
            bad.push(format!(
                "{tag}: expected {want:?}; check clean={clean}, run ok={}, {:?}: {text}",
                got.ok, got.stdout
            ));
        }
    }
    bad
}

/// The grid is written from the checker's table: a hook added to `HOOKS` with no grid row, or a
/// grid row for a method `HOOKS` does not list, fails here.
#[test]
fn every_hook_row_has_a_grid_row() {
    let table: Vec<&str> = chezzi::base::hooks::HOOKS
        .iter()
        .map(|row| row.hook.method())
        .collect();
    let grid: Vec<&str> = hooks().iter().map(|hook| hook.name).collect();
    let mut sorted = (table.clone(), grid.clone());
    sorted.0.sort_unstable();
    sorted.1.sort_unstable();
    assert_eq!(sorted.0, sorted.1, "HOOKS: {table:?}, grid: {grid:?}");
}

/// One (owner kind, hook): every shape cell.
fn assert_owner_hook(kind: &str, name: &str) {
    let hook = hook_named(name);
    let mut bad = Vec::new();
    for si in 0..SHAPES.len() {
        bad.extend(cell_failures(kind, &hook, si));
    }
    assert_rows(&format!("{kind} x {name}"), bad);
}

/// One hook of the conditional-hook grid.
fn assert_conditional_hook(name: &str) {
    assert_rows(
        &format!("conditional {name}"),
        conditional_hook_failures(&hook_named(name)),
    );
}

/// One hook of the declaration-verdict grid. `index`, `slice` and `set_index` have no variant, so
/// their test checks zero rows; it keeps the list equal to `hooks()`.
fn assert_declaration_verdict_hook(name: &str) {
    assert_rows(
        &format!("declaration-verdict {name}"),
        declaration_verdict_hook_failures(&hook_named(name)),
    );
}

#[test]
fn iter_call_rows_agree() {
    assert_rows("iter call", iter_call_failures());
}

#[test]
fn iter_run_rows_agree() {
    assert_rows("iter run", iter_run_failures());
}

#[test]
fn eq_rows_agree() {
    assert_rows("eq", eq_failures());
}

#[test]
fn conditional_rows_agree() {
    assert_rows("conditional", conditional_failures());
}

#[test]
fn declaration_verdict_named_programs_agree() {
    assert_rows(
        "declaration-verdict named",
        declaration_verdict_named_failures(),
    );
}

#[test]
fn iterator_bound_rows_agree() {
    assert_rows("iterator bound", iterator_bound_failures());
}

/// Each hook list of the split grids names every hook of `hooks()` once, so a hook added to
/// `HOOKS` cannot go untested (with `every_hook_row_has_a_grid_row`, all three lists tie to it).
#[test]
fn every_grid_hook_has_a_test() {
    let mut want: Vec<&str> = hooks().iter().map(|hook| hook.name).collect();
    want.sort_unstable();
    for (list, rows) in [
        ("HOOK_NAMES", HOOK_NAMES),
        ("CONDITIONAL_HOOKS", CONDITIONAL_HOOKS),
        ("DECLARATION_VERDICT_HOOKS", DECLARATION_VERDICT_HOOKS),
    ] {
        let mut have: Vec<&str> = rows.to_vec();
        have.sort_unstable();
        assert_eq!(have, want, "{list} must list every hook of hooks()");
    }
}

/// TICKET-250 (D6) — `iterable_elem` is the one reader of "what does iterating this yield".
/// Every source of an iterand feeds every consumer, and each prints the same elements. No range
/// and no channel row: the owner is designing their rule separately.
#[test]
fn every_iterand_source_feeds_every_consumer() {
    const E: &str = "(int, int)";
    let decls = format!(
        "import Iterator, Iterable from std.iter\nstruct Bag:\n    xs: List[{E}]\n    fn iter(self) -> Iterator[{E}]:\n        return self.xs.iter()\nstruct Cur:\n    i: int\n    fn next(self) -> {E}?:\n        if self.i >= 2:\n            return None\n        self.i += 1\n        return (self.i, self.i * 10)\nfn drain[S: Iterable[T], T](s: S) -> List[T]:\n    return [q for q in s]\n"
    );
    const BAG: &str = "Bag([(1, 10), (2, 20)])";
    // (source, wrapper header or "" for a top-level use, the value the consumer iterates)
    let sources: [(&str, String, &str); 5] = [
        ("struct with iter", String::new(), BAG),
        ("struct with next", String::new(), "Cur(0)"),
        (
            "S: Iterable[T]",
            format!("fn w[S: Iterable[{E}]](s: S):\n"),
            BAG,
        ),
        (
            "S: Iterator[T]",
            format!("fn w[S: Iterator[{E}]](s: S):\n"),
            "Cur(0)",
        ),
        (
            "Iterable[T]-typed value",
            format!("fn w(s: Iterable[{E}]):\n"),
            BAG,
        ),
    ];
    let list = "[(1, 10), (2, 20)]\n";
    let consumers: [(&str, String, &str); 6] = [
        (
            "for",
            format!("t: List[{E}] = []\nfor q in SRC:\n    t.push(q)\nprint(t)\n"),
            list,
        ),
        (
            "comprehension",
            "print([q for q in SRC])\n".to_string(),
            list,
        ),
        ("List()", "print(List.from_iter(SRC))\n".to_string(), list),
        (
            "Set()",
            "print(Set.from_iter(SRC).len())\n".to_string(),
            "2\n",
        ),
        (
            "Map()",
            "print(Map.from_iter(SRC))\n".to_string(),
            "{1: 10, 2: 20}\n",
        ),
        ("generic inference", "print(drain(SRC))\n".to_string(), list),
    ];
    let mut bad = Vec::new();
    for (si, (source, wrapper, value)) in sources.iter().enumerate() {
        for (ci, (consumer, body, want)) in consumers.iter().enumerate() {
            let src = if wrapper.is_empty() {
                format!("{decls}{}", body.replace("SRC", value))
            } else {
                let inner: String = body
                    .replace("SRC", "s")
                    .lines()
                    .map(|l| format!("    {l}\n"))
                    .collect();
                format!("{decls}{wrapper}{inner}w({value})\n")
            };
            let got = run(&format!("iterand_{si}_{ci}"), &src, &mut bad);
            if got.stdout != format!("{want}END\n") {
                bad.push(format!(
                    "{source} -> {consumer}: expected {want:?}, got: {}",
                    got.text
                ));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} cell(s) wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// A run's text with the file name dropped and each call site cut to its line: the consumers of
/// one hook spell its call at one line but at different columns.
fn trace(name: &str, text: &str) -> String {
    let text = text.replace(
        &format!("{}:", dir().join(format!("{name}.chz")).display()),
        "",
    );
    text.lines()
        .map(|line| match line.split_once("(called at ") {
            Some((head, site)) => {
                let at = site.split(':').next().unwrap_or(site);
                format!("{head}(called at {at})\n")
            }
            None => format!("{line}\n"),
        })
        .collect()
}

/// A user hook runs on one of two paths: a `for` loop and a direct `v.m(..)` push the hook's
/// frame flat on the fiber (`do_method_call`), and `List(v)`, a comprehension and an operator
/// re-enter through `invoke_hook`. Each cell runs every consumer of a hook at
/// `CHEZZI_THREADS` 1, 2 and default (`run`), and every consumer must print the cell's one
/// golden for that hook: a fault inside the hook, a hook that recurses past the depth limit, a
/// channel `recv` fed by a spawned sibling, and a sibling's fault while the owner sleeps inside
/// the hook. Each consumer is one statement, so its call site has one line in every spelling.
#[test]
fn every_hook_consumer_gives_the_same_failure_path() {
    // The consumers of each hook. `V` is the cell's receiver value.
    let consumers: [(&str, &str, &str); 8] = [
        ("next", "for", "for x in V: print(x)"),
        ("next", "list", "for x in List.from_iter(V): print(x)"),
        ("next", "comp", "for x in [q for q in V]: print(x)"),
        ("next", "call", "print(V.next())"),
        ("eq", "op", "print(V == V)"),
        ("eq", "call", "print(V.eq(V))"),
        ("add", "op", "print((V + V).n)"),
        ("add", "call", "print(V.add(V).n)"),
    ];
    // (cell, declarations ending in `fn use_it():`, the line before the consumer, the receiver,
    //  the consumers it skips, the golden of each hook as (hook, golden))
    type Cell = (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static [&'static str],
        [(&'static str, &'static str); 3],
    );
    let fault_decl = "struct Bad:\n    n: int\n    fn next(self) -> int?:\n        xs := [1]\n        return xs[5]\n    fn eq(self, other: Bad) -> bool:\n        xs := [1]\n        return xs[5] == 0\n    fn add(self, other: Bad) -> Bad:\n        xs := [1]\n        return Bad(xs[5])\nfn use_it():\n";
    let depth_decl = "struct R:\n    n: int\n    fn next(self) -> int?:\n        return self.next()\n    fn eq(self, other: R) -> bool:\n        return self == other\n    fn add(self, other: R) -> R:\n        return self + other\nfn use_it():\n";
    let feed_decl = "fn take(c: Channel[int]) -> int:\n    v := c.recv() else: panic(\"c closed\")\n    return v\nstruct Feed:\n    c: Channel[int]\n    n: int\n    fn next(self) -> int?:\n        if self.n == 0:\n            return None\n        self.n = self.n - 1\n        return take(self.c)\n    fn eq(self, other: Feed) -> bool:\n        return take(self.c) + take(self.c) + take(self.c) == 3\n    fn add(self, other: Feed) -> Feed:\n        return Feed(self.c, take(self.c) + take(self.c) + take(self.c))\nc := Channel[int](0)\nfn use_it():\n";
    let slow_decl = "import std.time\nstruct Slow:\n    n: int\n    fn next(self) -> int?:\n        time.sleep_ms(200)\n        print(\"step\")\n        return None\n    fn eq(self, other: Slow) -> bool:\n        time.sleep_ms(200)\n        print(\"step\")\n        return true\n    fn add(self, other: Slow) -> Slow:\n        time.sleep_ms(200)\n        print(\"step\")\n        return self\nfn boom():\n    time.sleep_ms(50)\n    xs := [1]\n    print(xs[5])\nfn use_it():\n";
    let depth = |hook: &str, line: usize| -> String {
        format!(
            "runtime error ({line}:16): maximum call depth (10000) exceeded (infinite recursion?)\n  at {hook} (called at {line})\n  … (× 9998 more identical frames) …\n  at use_it (called at 12)\n"
        )
    };
    let depth_next = depth("next", 4);
    let depth_eq = depth("eq", 6);
    let depth_add = depth("add", 8);
    let boom = "runtime error (19:11): index 5 out of bounds (len 1)\n  at boom (called at 22)\n";
    let cells: [Cell; 4] = [
        (
            "fault",
            fault_decl,
            "    print(\"before\")\n",
            "Bad(1)",
            &[],
            [
                (
                    "next",
                    "before\nruntime error (5:16): index 5 out of bounds (len 1)\n  at next (called at 14)\n  at use_it (called at 15)\n",
                ),
                (
                    "eq",
                    "before\nruntime error (8:16): index 5 out of bounds (len 1)\n  at eq (called at 14)\n  at use_it (called at 15)\n",
                ),
                (
                    "add",
                    "before\nruntime error (11:20): index 5 out of bounds (len 1)\n  at add (called at 14)\n  at use_it (called at 15)\n",
                ),
            ],
        ),
        (
            "depth",
            depth_decl,
            "    print(\"before\")\n",
            "R(1)",
            &[],
            [
                (
                    "next",
                    Box::leak(format!("before\n{depth_next}").into_boxed_str()),
                ),
                (
                    "eq",
                    Box::leak(format!("before\n{depth_eq}").into_boxed_str()),
                ),
                (
                    "add",
                    Box::leak(format!("before\n{depth_add}").into_boxed_str()),
                ),
            ],
        ),
        // The sender is a sibling task, so the hook parks in `recv` mid-call. `v.next()` takes
        // one value of three and leaves the sender blocked; it has no row.
        (
            "channel",
            feed_decl,
            "    parallel:\n        spawn:\n            for i in 0..3:\n                c.send(i)\n",
            "Feed(c, 3)",
            &["next|call"],
            [
                ("next", "0\n1\n2\nEND\n"),
                ("eq", "true\nEND\n"),
                ("add", "3\nEND\n"),
            ],
        ),
        // The owner sleeps inside the hook when the sibling faults: the run reports the
        // sibling's fault, and neither the rest of the hook nor `after` prints.
        (
            "cancel",
            slow_decl,
            "    parallel:\n        spawn boom()\n",
            "Slow(1)",
            &[],
            [("next", boom), ("eq", boom), ("add", boom)],
        ),
    ];
    let mut bad = Vec::new();
    for (cell, decl, before, value, skip, goldens) in cells {
        for (hook, consumer, stmt) in consumers {
            if skip.contains(&format!("{hook}|{consumer}").as_str()) {
                continue;
            }
            let golden = goldens
                .iter()
                .find(|(h, _)| *h == hook)
                .map(|(_, g)| *g)
                .unwrap_or_else(|| panic!("cell {cell} has no golden for {hook}"));
            let indent = if before.contains("parallel:") {
                "        "
            } else {
                "    "
            };
            let after = if cell == "cancel" {
                format!("{indent}print(\"after\")\n")
            } else {
                String::new()
            };
            let src = format!(
                "{decl}{before}{indent}{}\n{after}use_it()\n",
                stmt.replace('V', value)
            );
            let tag = format!("hookpath_{cell}_{hook}_{consumer}");
            let got = run(&tag, &src, &mut bad);
            let text = trace(&tag, &got.text);
            if text != golden {
                bad.push(format!(
                    "{cell}|{hook}|{consumer}: expected {golden:?}, got {text:?}"
                ));
            }
        }
    }
    assert_rows("hook failure path", bad);
}

/// TICKET-250 — the near-miss warning is true where it fires. For every (hook, shape, owner)
/// cell: the checker warns `'m' on X is not the P hook` exactly when the method has a receiver,
/// the grid's table says it is not the hook, and the operator use still runs to its last
/// statement (so the operator skipped the method silently). The gate is measured per cell on the
/// built binary. A cell where the gate and the run disagree is fixed by declining the warning.
#[test]
fn a_warning_fires_exactly_where_the_operator_skips_the_method() {
    let hooks = hooks();
    let mut bad = Vec::new();
    let mut cells = 0;
    for &kind in OWNERS {
        for hook in &hooks {
            for (si, shape) in SHAPES.iter().enumerate() {
                let Some(conforming) = conforms(hook, shape) else {
                    continue;
                };
                cells += 1;
                let decl = owner(kind, &hook.shapes[si]);
                let src = format!("{}{decl}{}", import_lines("", &[&decl]), hook.op);
                let tag = format!("warn_{kind}_{}_{shape}", hook.name);
                let path = dir().join(format!("{tag}.chz"));
                std::fs::write(&path, &src).expect("write the cell");
                let checked = Command::new(env!("CARGO_BIN_EXE_chezzi"))
                    .arg("check")
                    .arg("--errors=json")
                    .arg(&path)
                    .output()
                    .expect("run chezzi");
                let report = String::from_utf8_lossy(&checked.stdout).to_string();
                let warned = report.contains(&format!("'{}' on X is not the", hook.name));
                let ran = chezzi("run", &tag, &format!("{src}print(\"END\")\n"), Some("2"));
                let skipped = ran.ok && ran.stdout.ends_with("END\n");
                let want = *shape != "static" && !conforming && skipped;
                if warned != want {
                    bad.push(format!(
                        "{kind}|{}|{shape}: warned={warned}, expected {want} (operator ran={skipped}): {report}",
                        hook.name
                    ));
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {cells} cells wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
