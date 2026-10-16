//! One answer per (binder kind × prior binding × scope) for a second binding of a name. The
//! expected answer of each cell is derived from the rule, never listed by hand:
//!
//! - A module has two namespaces (Rust's): a type (`struct`, `enum`, `type`, `protocol`) and a value
//!   (`fn`, `extern fn`, `:=`) of one name coexist, and a call reaches the value (TICKET-029).
//! - A second declaration in a namespace a prior one binds is an error (Go: `redeclared in this
//!   block`; Rust: E0428 / E0255): "is already imported" when either is an import, else "is already
//!   defined". Imports and top-level fns are hoisted, so source order does not matter.
//! - A top-level `:=` of a value's name re-binds its slot, whose type is frozen at its first
//!   binding: the same type is legal (Python's re-binding), another type is rejected, wherever the
//!   readers sit. A variant import is a constant, so no let re-binds it (Rust E0530).
//! - A builtin is shadowed, and an inner scope (a fn body) shadows everything (all three ancestors).
//!
//! Every cell names `abs`, so an `extern "libc"` binder resolves a real symbol and the run is real.
//! Measured on the ancestors (2026-10-16): Go rejects every top-level pair (`x redeclared in this
//! block`, `F already declared through dot-import`); Rust rejects every same-namespace pair
//! (E0428/E0255) and accepts `enum N` / `type N` beside `fn N`; Python re-binds every pair.

use std::path::Path;
use std::process::Command;

#[derive(Clone, Copy, PartialEq)]
enum Origin {
    Builtin,
    Import,
    Own,
}

/// A binding of `abs` before the binder: how to write it, which namespaces it binds, and, for a
/// value, an expression of its type.
struct Prior {
    tag: &'static str,
    origin: Origin,
    header: &'static str,
    value: bool,
    ty: bool,
    same_type: Option<&'static str>,
    /// A constant (a variant import): no let re-binds it (Rust E0530).
    is_const: bool,
    /// A top-level `:=`: a hoisted binder (`fn`, `extern`) fills the slot first, so the let is the
    /// second binding and the let's rule judges the pair.
    is_let: bool,
}

const LIB: &[(&str, &str)] = &[
    ("lib/f.chz", "fn abs(x: int) -> int:\n    return x\n"),
    ("lib/v.chz", "abs := 5\n"),
    ("lib/t.chz", "struct abs:\n    a: int\n"),
    ("lib/abs.chz", "K := 1\n"),
];

const PRIORS: &[Prior] = &[
    Prior {
        tag: "import-fn",
        origin: Origin::Import,
        header: "import abs from lib.f\n",
        value: true,
        ty: false,
        same_type: Some("fn(x: int) -> int: x"),
        is_let: false,
        is_const: false,
    },
    Prior {
        tag: "import-value",
        origin: Origin::Import,
        header: "import abs from lib.v\n",
        value: true,
        ty: false,
        same_type: Some("7"),
        is_let: false,
        is_const: false,
    },
    Prior {
        tag: "import-type",
        origin: Origin::Import,
        header: "import abs from lib.t\n",
        value: false,
        ty: true,
        same_type: None,
        is_let: false,
        is_const: false,
    },
    Prior {
        tag: "import-module",
        origin: Origin::Import,
        header: "import lib.abs\n",
        value: true,
        ty: true,
        same_type: None,
        is_let: false,
        is_const: false,
    },
    Prior {
        tag: "import-variant",
        origin: Origin::Import,
        header: "import abs from Color\nenum Color:\n    abs\n    B\n",
        value: true,
        ty: true,
        same_type: Some("Color.B"),
        is_let: false,
        is_const: true,
    },
    Prior {
        tag: "own-fn",
        origin: Origin::Own,
        header: "fn abs(x: int) -> int:\n    return x\n",
        value: true,
        ty: false,
        same_type: Some("fn(x: int) -> int: x"),
        is_let: false,
        is_const: false,
    },
    Prior {
        tag: "own-extern",
        origin: Origin::Own,
        header: "extern \"libc\":\n    fn abs(x: int) -> int\n",
        value: true,
        ty: false,
        same_type: Some("fn(x: int) -> int: x"),
        is_let: false,
        is_const: false,
    },
    Prior {
        tag: "own-value",
        origin: Origin::Own,
        header: "abs := 5\n",
        value: true,
        ty: false,
        same_type: Some("7"),
        is_let: true,
        is_const: false,
    },
    Prior {
        tag: "own-struct",
        origin: Origin::Own,
        header: "struct abs:\n    a: int\n",
        value: false,
        ty: true,
        same_type: None,
        is_let: false,
        is_const: false,
    },
    Prior {
        tag: "own-enum",
        origin: Origin::Own,
        header: "enum abs:\n    A\n",
        value: false,
        ty: true,
        same_type: None,
        is_let: false,
        is_const: false,
    },
    Prior {
        tag: "own-alias",
        origin: Origin::Own,
        header: "type abs = int\n",
        value: false,
        ty: true,
        same_type: None,
        is_let: false,
        is_const: false,
    },
    Prior {
        tag: "own-protocol",
        origin: Origin::Own,
        header: "protocol abs:\n    fn a(self) -> int\n",
        value: false,
        ty: true,
        same_type: None,
        is_let: false,
        is_const: false,
    },
    // `ord` is a builtin fn and `str` a builtin type; neither is a libc symbol, so an extern binder
    // of those names reaches the extern and stops at its `dlsym`.
    Prior {
        tag: "builtin-fn",
        origin: Origin::Builtin,
        header: "",
        value: true,
        ty: false,
        same_type: None,
        is_let: false,
        is_const: false,
    },
    Prior {
        tag: "builtin-type",
        origin: Origin::Builtin,
        header: "",
        value: false,
        ty: true,
        same_type: None,
        is_let: false,
        is_const: false,
    },
];

#[derive(Clone, Copy, PartialEq)]
enum Binder {
    Fn,
    Extern,
    Struct,
    Enum,
    Alias,
    Protocol,
    LetSame,
    LetOther,
    LocalLet,
    NestedFn,
    Param,
}

const BINDERS: &[Binder] = &[
    Binder::Fn,
    Binder::Extern,
    Binder::Struct,
    Binder::Enum,
    Binder::Alias,
    Binder::Protocol,
    Binder::LetSame,
    Binder::LetOther,
    Binder::LocalLet,
    Binder::NestedFn,
    Binder::Param,
];

impl Binder {
    fn tag(self) -> &'static str {
        match self {
            Binder::Fn => "fn",
            Binder::Extern => "extern",
            Binder::Struct => "struct",
            Binder::Enum => "enum",
            Binder::Alias => "type",
            Binder::Protocol => "protocol",
            Binder::LetSame => ":=same",
            Binder::LetOther => ":=other",
            Binder::LocalLet => "local:=",
            Binder::NestedFn => "nested-fn",
            Binder::Param => "param",
        }
    }

    /// The binder's source for name `n`, or `None` when the cell has no program (a same-type let
    /// of a prior that holds no value of a writable type).
    fn source(self, n: &str, same_type: Option<&str>) -> Option<String> {
        Some(match self {
            Binder::Fn => format!("fn {n}(x: int) -> int:\n    return x\n"),
            Binder::Extern => format!("extern \"libc\":\n    fn {n}(x: int) -> int\n"),
            Binder::Struct => format!("struct {n}:\n    b: int\n"),
            Binder::Enum => format!("enum {n}:\n    Z\n"),
            Binder::Alias => format!("type {n} = str\n"),
            Binder::Protocol => format!("protocol {n}:\n    fn z(self) -> int\n"),
            Binder::LetSame => format!("{n} := {}\n", same_type?),
            Binder::LetOther => format!("{n} := \"s\"\n"),
            Binder::LocalLet => format!("fn g():\n    {n} := \"s\"\n    print({n})\ng()\n"),
            Binder::NestedFn => {
                format!(
                    "fn g():\n    fn {n}() -> str:\n        return \"s\"\n    print({n}())\ng()\n"
                )
            }
            Binder::Param => format!("fn g({n}: str):\n    print({n})\ng(\"s\")\n"),
        })
    }

    /// The namespaces a top-level binder binds: (value, type). An inner-scope binder binds none.
    fn namespaces(self) -> (bool, bool) {
        match self {
            Binder::Fn | Binder::Extern | Binder::LetSame | Binder::LetOther => (true, false),
            Binder::Struct | Binder::Enum | Binder::Alias | Binder::Protocol => (false, true),
            Binder::LocalLet | Binder::NestedFn | Binder::Param => (false, false),
        }
    }
}

/// `None`: the program runs. `Some(needle)`: the check rejects it with that text.
fn expected(p: &Prior, b: Binder) -> Option<&'static str> {
    let (bv, bt) = b.namespaces();
    if p.origin == Origin::Builtin || !((bv && p.value) || (bt && p.ty)) {
        return None;
    }
    match b {
        Binder::LetSame | Binder::LetOther if p.is_const => Some("cannot re-declare const binding"),
        Binder::LetSame => None,
        Binder::LetOther => Some("cannot re-declare module-level binding"),
        Binder::Fn | Binder::Extern if p.is_let => Some("cannot re-declare module-level binding"),
        Binder::Fn
        | Binder::Extern
        | Binder::Struct
        | Binder::Enum
        | Binder::Alias
        | Binder::Protocol
        | Binder::LocalLet
        | Binder::NestedFn
        | Binder::Param => Some(if p.origin == Origin::Import {
            "is already imported"
        } else {
            "is already defined"
        }),
    }
}

fn run(dir: &Path, src: &str) -> (bool, String) {
    std::fs::write(dir.join("main.chz"), src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", "main.chz"])
        .current_dir(dir)
        .output()
        .expect("run chezzi");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[test]
fn every_redeclaration_cell_has_the_derived_answer() {
    let dir = std::env::temp_dir().join(format!("chezzi-redeclare-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("lib")).unwrap();
    for (rel, src) in LIB {
        std::fs::write(dir.join(rel), src).unwrap();
    }
    let mut wrong = Vec::new();
    let mut cells = 0;
    for p in PRIORS {
        let name = match p.tag {
            "builtin-fn" => "ord",
            "builtin-type" => "str",
            _ => "abs",
        };
        for &b in BINDERS {
            let Some(binder) = b.source(name, p.same_type) else {
                continue;
            };
            cells += 1;
            let src = format!("{}{binder}print(\"end\")\n", p.header);
            let (ok, text) = run(&dir, &src);
            let want = expected(p, b);
            let pass = match want {
                None => {
                    (ok && text.ends_with("end\n"))
                        || (b == Binder::Extern
                            && name != "abs"
                            && text.contains(&format!("symbol '{name}' not found")))
                }
                Some(needle) => !ok && text.contains("type error") && text.contains(needle),
            };
            if !pass {
                wrong.push(format!(
                    "{} x {}: want {want:?}, got {}",
                    p.tag,
                    b.tag(),
                    text.lines().next().unwrap_or("")
                ));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(cells > 140, "the grid lost cells: {cells}");
    assert!(
        wrong.is_empty(),
        "{} wrong cell(s) of {cells}:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}
