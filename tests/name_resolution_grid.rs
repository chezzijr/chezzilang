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
        tag: "newtype",
        name: "N",
        decl: "newtype N = int\n",
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
    // Desugar's pre-check call binding (the TICKET-182 interim) sees this one.
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
        // A top-level `fn` may not take a builtin / reserved name (`is_reserved_name`).
        ("toplevel_fn", "builtin_fn" | "builtin_ctor" | "std_ctor") => Some("is reserved"),
        // Two top-level declarations of one name.
        ("toplevel_fn", "imported_fn" | "defaulted_fn") => Some("is already defined"),
        // A module global's type is frozen at its first declaration (the import).
        ("toplevel_let", "module") => Some("cannot re-declare module-level binding"),
        // A top-level `fn` over an imported module name: Go and Rust reject the redeclaration.
        // The message belongs to the Declarations family; the cell pins that it is rejected.
        ("toplevel_fn", "module") => Some("module math is not callable"),
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
    cells
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

/// DEC-029/055/172: a same-module top-level `fn S` over a struct, newtype or alias `S`.
fn same_module_fn_cells() -> Vec<Cell> {
    let p = "struct P:\n    x: int\n";
    let bn = "(n: int) -> str:\n    return \"B{n}\"\n";
    vec![
        one(
            "toplevel_fn/struct/raw_ctor_in_own_body",
            format!("{p}fn P(n: int) -> P:\n    return P(n)\nprint(P(4))\n"),
            Expect::Prints("P(x=4)".into()),
        ),
        one(
            "toplevel_fn/newtype/raw_ctor_in_own_body",
            "newtype N = int\nfn N(n: int) -> N:\n    return N(n)\nprint(N(4) == N(4))\n".into(),
            Expect::Prints("true".into()),
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
            "importer/newtype/qualified_call",
            "import lib\nprint(lib.N(4))\n",
            &format!("newtype N = int\nfn N{bn}"),
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

/// Cells desugar's pre-check call binding still gets wrong: the owner-accepted interim that
/// TICKET-182 removes. They must stay red here; when one turns green, move it back.
const INTERIM_182: &[&str] = &["nested_fn/defaulted_fn/call"];

/// TICKET-180 cells whose fix is a later step of that ticket (qualified heads, identifier reads,
/// pattern heads, alias bodies). The ticket is not done until this list is empty. They must stay
/// red here; when one turns green, delete it from the list.
const PENDING_180: &[&str] = &[
    "toplevel_let/struct/member",
    "import_alias/struct/member",
    "toplevel_let/newtype/member",
    "import_alias/newtype/member",
    "toplevel_let/enum/member",
    "import_alias/enum/member",
    "toplevel_let/alias/member",
    "import_alias/alias/member",
    "toplevel_let/imported_type/member",
    "import_alias/defaulted_fn/member",
    "importer/alias/k3_str_arg",
    "importer/alias/k3_int_arg",
    "p2/alias_of_ambiguous_module_type",
    "p2/generic_alias_turbofish",
];

#[test]
fn name_resolution_grid() {
    let root = std::env::temp_dir().join(format!("chezzi-name-grid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cells = grid();
    let mut fails = Vec::new();
    for (i, c) in cells.iter().enumerate() {
        let pinned = [INTERIM_182, PENDING_180]
            .iter()
            .any(|l| l.contains(&c.name.as_str()));
        match (run_cell(&root, i, c), pinned) {
            (Err(e), false) => fails.push(e),
            (Ok(()), true) => {
                fails.push(format!("{}: green; remove it from its pinned list", c.name))
            }
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
