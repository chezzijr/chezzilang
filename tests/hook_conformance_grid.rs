//! TICKET-250: the hook-conformance grid. A method is the hook for an operator exactly when it
//! satisfies the builtin protocol's requirement (`Checker::hook_method`). One generated program
//! per cell, run through the built `chezzi` binary: hook x shape x owner kind x use. The operator
//! and the bound `T: P` must give the same answer in every cell, and a method that is not the
//! hook must leave every use exactly as it is for the same owner with no such method.

use std::path::PathBuf;
use std::process::Command;

fn m(sig: &str, body: &str) -> String {
    format!("    fn {sig}:\n        {body}\n")
}

const SHAPES: [&str; 6] = ["conf", "gen", "static", "wrongret", "extra", "generic"];
const OWNERS: [&str; 5] = ["struct", "enum", "gstruct", "genum", "zstruct"];

struct Hook {
    name: &'static str,
    /// The protocol, with the arguments the conforming shape satisfies.
    proto: &'static str,
    /// The slot's type when it is not `proto`: `Iterator[T]` in type position is the cursor
    /// itself, so a `next` owner is held in an `Iterable[T]` slot.
    slot_proto: Option<&'static str>,
    /// One method text per entry of `SHAPES`.
    shapes: [String; 6],
    op: &'static str,
    slot: &'static str,
}

fn hooks() -> Vec<Hook> {
    let idx = m("index(self, k: int) -> int", "return self.n");
    let eq = m("eq(self, other: Self) -> bool", "return true");
    vec![
        Hook {
            name: "iter",
            proto: "Iterable[int]",
            slot_proto: None,
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
        },
        Hook {
            name: "next",
            proto: "Iterator[int]",
            slot_proto: Some("Iterable[int]"),
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
        },
        Hook {
            name: "index",
            proto: "Index[int, int]",
            slot_proto: None,
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
        },
        Hook {
            name: "slice",
            proto: "Slice[int]",
            slot_proto: None,
            shapes: [
                m(
                    "slice(self, start: int?, end: int?, step: int?) -> int",
                    "return 7",
                ),
                m(
                    "slice(self, start: int?, end: int?, step: int?) -> Iterator[int]",
                    "yield 7",
                ),
                m(
                    "slice(a: int?, start: int?, end: int?, step: int?) -> int",
                    "return 7",
                ),
                m(
                    "slice(self, start: int?, end: int?, step: int?) -> str",
                    "return \"s\"",
                ),
                m(
                    "slice(self, start: int?, end: int?, step: int?, z: int) -> int",
                    "return 7",
                ),
                m(
                    "slice[U](self, start: int?, end: int?, step: int?) -> int",
                    "return 7",
                ),
            ],
            op: "print(x[1:2])\n",
            slot: "print(v[1:2])",
        },
        Hook {
            name: "set_index",
            proto: "IndexSet[int, int]",
            slot_proto: None,
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
        },
        Hook {
            name: "hash",
            proto: "Hashable",
            slot_proto: None,
            shapes: [
                m("hash(self) -> int", "return 1") + &eq,
                m("hash(self) -> Iterator[int]", "yield 1") + &eq,
                m("hash(a: int) -> int", "return 1") + &eq,
                m("hash(self) -> str", "return \"h\"") + &eq,
                m("hash(self, z: int) -> int", "return 1") + &eq,
                m("hash[U](self) -> int", "return 1") + &eq,
            ],
            op: "s := Set([x, y])\nprint(s.len())\n",
            slot: "print(\"slot\")",
        },
        Hook {
            name: "compare",
            proto: "Comparable",
            slot_proto: None,
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
        },
        Hook {
            name: "add",
            proto: "Add",
            slot_proto: None,
            shapes: [
                m("add(self, other: Self) -> Self", "return self"),
                m("add(self, other: Self) -> Iterator[int]", "yield 1"),
                m("add(a: int, other: int) -> int", "return a + other"),
                m("add(self, other: Self) -> int", "return 1"),
                m("add(self, other: Self, z: int) -> Self", "return self"),
                m("add[U](self, other: Self) -> Self", "return self"),
            ],
            op: "z := x + y\nprint(\"added\")\n",
            slot: "print(\"slot\")",
        },
        Hook {
            name: "neg",
            proto: "Neg",
            slot_proto: None,
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
        },
        Hook {
            name: "contains",
            proto: "Contains[int]",
            slot_proto: None,
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

/// `chezzi check <src>`: did it check clean, and everything it printed.
fn check(name: &str, src: &str) -> (bool, String) {
    let path = dir().join(format!("{name}.chz"));
    std::fs::write(&path, src).expect("write the cell");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("check")
        .arg(&path)
        .output()
        .expect("run chezzi");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// The three uses of one owner: operator, bound `T: P`, protocol-typed slot.
fn uses(hook: &Hook, proto: &str, decl: &str) -> [(&'static str, String); 3] {
    [
        ("op", format!("{decl}{}", hook.op)),
        (
            "bound",
            format!("{decl}fn b[C: {proto}](c: C):\n    print(\"bound ok\")\nb(x)\n"),
        ),
        (
            "slot",
            format!(
                "{decl}fn s(v: {}):\n    {}\ns(x)\n",
                hook.slot_proto.unwrap_or(proto),
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
    let mut answers = Vec::new();
    for ((u, src), (_, src_none)) in uses(hook, proto, &decl)
        .into_iter()
        .zip(uses(hook, proto, &none))
    {
        let tag = format!("{kind}_{}_{shape}_{u}", hook.name);
        let (ok, text) = check(&tag, &src);
        answers.push((u, ok));
        match verdict {
            Some(true) if !ok => {
                bad.push(format!("{cell}|{u}: a conforming hook is rejected: {text}"))
            }
            Some(false) => {
                let (ok_none, _) = check(&format!("{tag}_none"), &src_none);
                if ok != ok_none {
                    bad.push(format!(
                        "{cell}|{u}: a non-hook answers {ok}, the owner with no such method {ok_none}: {text}"
                    ));
                }
            }
            _ => {}
        }
    }
    if answers.iter().any(|(_, ok)| *ok != answers[0].1) {
        bad.push(format!("{cell}: the uses disagree: {answers:?}"));
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
        for (row, methods, body, needle) in [
            (
                "next_only",
                &next,
                "it := c.iter()\nprint(it)\n",
                "has no method 'iter'",
            ),
            (
                "static_next",
                &static_next,
                "it := c.iter()\nprint(it)\n",
                "has no method 'iter'",
            ),
            (
                "both",
                &both,
                "n: int? = c.iter().next()\nprint(n)\n",
                "cannot assign str? to variable of type int?",
            ),
        ] {
            let (ok, text) = check(
                &format!("conc_{kind}_{row}"),
                &format!("{head}{methods}{mk}{body}"),
            );
            if ok || !text.contains(needle) {
                bad.push(format!(
                    "concrete c.iter()|{kind}|{row}: expected {needle:?}, got: {text}"
                ));
            }
        }
    }
    let slot = "fn g(s: Iterable[int]) -> List[int]:\n    return List(s.iter())\nprint(g([1, 2]))\nprint(g(Set([3])))\n";
    let (ok, text) = check("slot_builtin", slot);
    if !ok {
        bad.push(format!("s.iter() on an Iterable[int] slot: {text}"));
    }
    bad
}

#[test]
fn operator_and_bound_agree_in_every_cell() {
    let hooks = hooks();
    let mut cells: Vec<(&str, &Hook, usize)> = Vec::new();
    for kind in OWNERS {
        for hook in &hooks {
            for si in 0..SHAPES.len() {
                cells.push((kind, hook, si));
            }
        }
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let bad = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some((kind, hook, si)) = cells.get(i) else {
                        break;
                    };
                    let found = cell_failures(kind, hook, *si);
                    bad.lock().expect("grid lock").extend(found);
                }
            });
        }
    });
    let mut bad = bad.into_inner().expect("grid lock");
    bad.extend(iter_call_failures());
    bad.sort();
    assert!(
        bad.is_empty(),
        "{} of {} cells wrong:\n{}",
        bad.len(),
        cells.len(),
        bad.join("\n")
    );
}
