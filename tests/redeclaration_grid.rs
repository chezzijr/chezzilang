//! One answer per (binder kind × prior binding × scope) for a second binding of a name. The
//! expected answer of each cell is derived from the rule, never listed by hand:
//!
//! - A module binds each name once (owner decision 2026-10-16; Go: `redeclared in this block`, Rust:
//!   E0428 / E0255). A type (`struct`, `enum`, `type`, `protocol`, a type import) and a value (`fn`,
//!   `extern fn`, `:=`, a value import) never share a name, because Chezzi uses a type name as a
//!   value (`E(1)`, `E.A`), so it is callable.
//! - A second declaration of a name is an error: "is already imported" when the prior is an import,
//!   else "is already defined". Imports and top-level fns are hoisted, so source order does not
//!   matter.
//! - A top-level `:=` is a declaration, so a name is bound once whatever the types; `=` reassigns.
//! - A builtin is shadowed, and an inner scope (a fn body) shadows everything (all three ancestors).
//! - A re-export is no member: `import abs from lib.hrs` where `hrs` only imports `abs` (Go has no
//!   re-export; Rust E0603).
//!
//! Every cell names `abs`, so an `extern "libc"` binder resolves a real symbol and the run is real.
//! An import row with a `use_line` reads the prior inside a fn called before the binder: the
//! program runs, so a type-only import that holds a slot (`'None' is not callable`) fails the cell.
//! The ticket repro and the prelude cell run in the same test. Python re-binds every pair, but a
//! dynamically typed language is no oracle for a static name rule.

use std::path::Path;
use std::process::Command;

#[derive(Clone, Copy, PartialEq)]
enum Origin {
    Builtin,
    Import,
    Own,
}

/// A binding of `name` before the binder: how to write it, whether `import abs from lib.<home>`
/// reaches a member, and, for a value, an expression of its type.
struct Prior {
    tag: &'static str,
    name: &'static str,
    origin: Origin,
    header: &'static str,
    /// `false` for a re-export: the home module only imports the name, so it is no member.
    member: bool,
    /// A statement that reads the prior, and the text it prints.
    use_line: Option<(&'static str, &'static str)>,
    same_type: Option<&'static str>,
}

const LIB: &[(&str, &str)] = &[
    ("lib/f.chz", "fn abs(x: int) -> int:\n    return x\n"),
    ("lib/v.chz", "abs := 5\n"),
    ("lib/t.chz", "struct abs:\n    a: int\n"),
    ("lib/abs.chz", "K := 1\n"),
    ("lib/hc.chz", "abs: const int = 5\n"),
    (
        "lib/hx.chz",
        "extern \"libc\":\n    fn abs(x: int) -> int\n",
    ),
    ("lib/he.chz", "enum abs:\n    Z\n"),
    ("lib/ha.chz", "type abs = int\n"),
    ("lib/hp.chz", "protocol abs:\n    fn z(self) -> int\n"),
    ("lib/hrs.chz", "import abs from lib.t\n"),
    ("lib/hrf.chz", "import abs from lib.f\n"),
];

const PRIORS: &[Prior] = &[
    Prior {
        tag: "import-fn",
        name: "abs",
        origin: Origin::Import,
        header: "import abs from lib.f\n",
        member: true,
        use_line: Some(("print(abs(2))", "2")),
        same_type: Some("fn(x: int) -> int: x"),
    },
    Prior {
        tag: "import-value",
        name: "abs",
        origin: Origin::Import,
        header: "import abs from lib.v\n",
        member: true,
        use_line: Some(("print(abs)", "5")),
        same_type: Some("7"),
    },
    Prior {
        tag: "import-const",
        name: "abs",
        origin: Origin::Import,
        header: "import abs from lib.hc\n",
        member: true,
        use_line: Some(("print(abs)", "5")),
        same_type: Some("7"),
    },
    Prior {
        tag: "import-extern",
        name: "abs",
        origin: Origin::Import,
        header: "import abs from lib.hx\n",
        member: true,
        use_line: Some(("print(abs(-2))", "2")),
        same_type: Some("fn(x: int) -> int: x"),
    },
    Prior {
        tag: "import-type",
        name: "abs",
        origin: Origin::Import,
        header: "import abs from lib.t\n",
        member: true,
        use_line: Some(("print(abs(a=1))", "abs(a=1)")),
        same_type: None,
    },
    Prior {
        tag: "import-enum",
        name: "abs",
        origin: Origin::Import,
        header: "import abs from lib.he\n",
        member: true,
        use_line: Some(("print(abs.Z)", "Z")),
        same_type: None,
    },
    Prior {
        tag: "import-alias",
        name: "abs",
        origin: Origin::Import,
        header: "import abs from lib.ha\n",
        member: true,
        use_line: Some(("x: abs = 3\n    print(x)", "3")),
        same_type: None,
    },
    Prior {
        tag: "import-protocol",
        name: "abs",
        origin: Origin::Import,
        header: "import abs from lib.hp\n",
        member: true,
        use_line: Some(("print(1)", "1")),
        same_type: None,
    },
    Prior {
        tag: "import-module",
        name: "abs",
        origin: Origin::Import,
        header: "import lib.abs\n",
        member: true,
        use_line: None,
        same_type: None,
    },
    Prior {
        tag: "import-variant",
        name: "abs",
        origin: Origin::Import,
        header: "import abs from Color\nenum Color:\n    abs\n    B\n",
        member: true,
        use_line: None,
        same_type: Some("Color.B"),
    },
    Prior {
        tag: "reexport-struct",
        name: "abs",
        origin: Origin::Import,
        header: "import abs from lib.hrs\n",
        member: false,
        use_line: None,
        same_type: None,
    },
    Prior {
        tag: "reexport-fn",
        name: "abs",
        origin: Origin::Import,
        header: "import abs from lib.hrf\n",
        member: false,
        use_line: None,
        same_type: None,
    },
    Prior {
        tag: "own-fn",
        name: "abs",
        origin: Origin::Own,
        header: "fn abs(x: int) -> int:\n    return x\n",
        member: true,
        use_line: None,
        same_type: Some("fn(x: int) -> int: x"),
    },
    Prior {
        tag: "own-extern",
        name: "abs",
        origin: Origin::Own,
        header: "extern \"libc\":\n    fn abs(x: int) -> int\n",
        member: true,
        use_line: None,
        same_type: Some("fn(x: int) -> int: x"),
    },
    Prior {
        tag: "own-value",
        name: "abs",
        origin: Origin::Own,
        header: "abs := 5\n",
        member: true,
        use_line: None,
        same_type: Some("7"),
    },
    Prior {
        tag: "own-struct",
        name: "abs",
        origin: Origin::Own,
        header: "struct abs:\n    a: int\n",
        member: true,
        use_line: None,
        same_type: None,
    },
    Prior {
        tag: "own-enum",
        name: "abs",
        origin: Origin::Own,
        header: "enum abs:\n    A\n",
        member: true,
        use_line: None,
        same_type: None,
    },
    Prior {
        tag: "own-alias",
        name: "abs",
        origin: Origin::Own,
        header: "type abs = int\n",
        member: true,
        use_line: None,
        same_type: None,
    },
    Prior {
        tag: "own-protocol",
        name: "abs",
        origin: Origin::Own,
        header: "protocol abs:\n    fn a(self) -> int\n",
        member: true,
        use_line: None,
        same_type: None,
    },
    // `ord` is a builtin fn and `str` a builtin type; neither is a libc symbol, so an extern binder
    // of those names reaches the extern and stops at its `dlsym`.
    Prior {
        tag: "builtin-fn",
        name: "ord",
        origin: Origin::Builtin,
        header: "",
        member: true,
        use_line: None,
        same_type: None,
    },
    Prior {
        tag: "builtin-type",
        name: "str",
        origin: Origin::Builtin,
        header: "",
        member: true,
        use_line: None,
        same_type: None,
    },
];

/// The enum and `ALL` come from one list, so a new variant is in `ALL`; every decision below is an
/// exhaustive `match`, so the build fails until it decides the variant.
macro_rules! binders {
    ($($v:ident),+ $(,)?) => {
        #[derive(Clone, Copy, PartialEq, Debug)]
        enum Binder {
            $($v),+
        }
        impl Binder {
            const ALL: &'static [Binder] = &[$(Binder::$v),+];
        }
    };
}

binders!(
    Fn, Extern, Struct, Enum, Alias, Protocol, LetSame, LetOther, LocalLet, NestedFn, Param
);

impl Binder {
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

    /// Does the binder bind a module name? An inner-scope binder does not.
    fn binds_module_name(self) -> bool {
        match self {
            Binder::Fn
            | Binder::Extern
            | Binder::Struct
            | Binder::Enum
            | Binder::Alias
            | Binder::Protocol
            | Binder::LetSame
            | Binder::LetOther => true,
            Binder::LocalLet | Binder::NestedFn | Binder::Param => false,
        }
    }
}

/// `None`: the program runs. `Some(needle)`: the check rejects it with that text.
fn expected(p: &Prior, b: Binder) -> Option<&'static str> {
    if !p.member {
        Some("has no member 'abs'")
    } else if p.origin == Origin::Builtin || !b.binds_module_name() {
        None
    } else if p.origin == Origin::Import {
        Some("is already imported")
    } else {
        Some("is already defined")
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
        for &b in Binder::ALL {
            let Some(binder) = b.source(p.name, p.same_type) else {
                continue;
            };
            cells += 1;
            let reader = match p.use_line {
                Some((stmt, _)) => format!("fn use_prior():\n    {stmt}\nuse_prior()\n"),
                None => String::new(),
            };
            let src = format!("{}{reader}{binder}print(\"end\")\n", p.header);
            let (ok, text) = run(&dir, &src);
            let want = expected(p, b);
            let pass = match want {
                None => {
                    let printed = p.use_line.is_none_or(|(_, out)| text.contains(out));
                    (ok && text.ends_with("end\n") && printed)
                        || (b == Binder::Extern
                            && p.name != "abs"
                            && text.contains(&format!("symbol '{}' not found", p.name)))
                }
                Some(needle) => !ok && text.contains("type error") && text.contains(needle),
            };
            if !pass {
                wrong.push(format!(
                    "{} x {:?}: want {want:?}, got {}",
                    p.tag,
                    b,
                    text.lines().next().unwrap_or("")
                ));
            }
        }
    }
    // The ticket repro: a type-only import, a read inside a fn, a later `:=` of the same name.
    std::fs::write(dir.join("lib2.chz"), "type Count = int\n").unwrap();
    let repro = "import Count from lib2\nfn g() -> int:\n    return Count(3)\nprint(g())\nCount := fn(n: int) -> int: n * 10\n";
    let (ok, text) = run(&dir, repro);
    cells += 1;
    if ok || !text.contains("type error") || !text.contains("'Count' is already imported") {
        wrong.push(format!(
            "ticket repro: want 'Count' is already imported, got {}",
            text.lines().next().unwrap_or("")
        ));
    }
    // The prelude is no module binding: a variant import of the prelude's name shadows it.
    let (ok, text) = run(&dir, "enum E:\n    Some\nimport Some from E\nprint(Some)\n");
    cells += 1;
    if !ok || text != "Some\n" {
        wrong.push(format!("prelude cell: want `Some`, got {text:?}"));
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
