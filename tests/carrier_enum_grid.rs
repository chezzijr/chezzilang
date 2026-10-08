//! TICKET-229 (R3, R3b): `Option` / `Result` are the prelude's ordinary enums, and an enum variant
//! is a bare name exactly where an `import V from Enum` binds it. Every C cell runs twice: once
//! over two user enums shaped like the carriers (`Opt1[T]`, `Res2[T, E]`, with the same two variant
//! imports the prelude declares), and once over the carriers, by whole-word substitution of the
//! program AND its expectation. The user twin is the oracle the carrier twin must equal. The V
//! cells pin variant import itself. One generated program per cell, run through the built `chezzi`
//! binary.

#[path = "support/grid_cell.rs"]
mod grid_cell;

use grid_cell::{Cell, Expect, run_grid};

/// The two user enums, shaped like the prelude's carriers.
const ENUMS: &str = "enum Opt1[T]:
    Som(T)
    Non
enum Res2[T, E]:
    Okk(T)
    Errr(E)
";

/// The two variant imports, shaped like the prelude's.
const IMPORTS: &str = "import Som, Non from Opt1
import Okk, Errr from Res2
";

/// The user twin's spelling of each carrier name.
const TWINS: &[(&str, &str)] = &[
    ("Opt1", "Option"),
    ("Som", "Some"),
    ("Non", "None"),
    ("Res2", "Result"),
    ("Okk", "Ok"),
    ("Errr", "Err"),
];

/// `text` with every whole identifier that is a user-twin name replaced by its carrier name.
fn to_carrier(text: &str) -> String {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        let repl = TWINS.iter().find(|(u, _)| u == word).map(|(_, c)| *c);
        out.push_str(repl.unwrap_or(word));
        word.clear();
    };
    for c in text.chars() {
        if is_word(c) {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

enum Want {
    Prints(&'static str),
    Rejects(&'static str),
    /// A reject whose text names the type: the carrier twin prints its sugar (second field).
    RejectsAs(&'static str, &'static str),
}
use Want::{Prints, Rejects, RejectsAs};

fn expect_of(w: &Want, carrier: bool) -> Expect {
    let conv = |s: &str| {
        if carrier {
            to_carrier(s)
        } else {
            s.to_string()
        }
    };
    match w {
        Prints(s) => Expect::Prints(conv(s)),
        Rejects(s) => Expect::Rejects(Box::leak(conv(s).into_boxed_str())),
        RejectsAs(user, sugar) => Expect::Rejects(if carrier { sugar } else { user }),
    }
}

/// A twin cell: `main` after the preamble (user twin) or alone (carrier twin).
fn twin(cells: &mut Vec<Cell>, name: &str, main: &str, want: Want) {
    cells.push(Cell {
        name: format!("{name} user"),
        files: vec![("main.chz".to_string(), format!("{ENUMS}{IMPORTS}{main}"))],
        expect: expect_of(&want, false),
    });
    cells.push(Cell {
        name: format!("{name} carrier"),
        files: vec![("main.chz".to_string(), to_carrier(main))],
        expect: expect_of(&want, true),
    });
}

/// A twin cell whose enum lives behind `lib.chz`: the user twin's lib declares the two enums, the
/// carrier twin's lib holds only `lib_tail`. The main file has no preamble in either twin.
fn twin_lib(cells: &mut Vec<Cell>, name: &str, lib_tail: &str, main: &str, want: Want) {
    cells.push(Cell {
        name: format!("{name} user"),
        files: vec![
            ("main.chz".to_string(), main.to_string()),
            ("lib.chz".to_string(), format!("{ENUMS}{lib_tail}")),
        ],
        expect: expect_of(&want, false),
    });
    cells.push(Cell {
        name: format!("{name} carrier"),
        files: vec![
            ("main.chz".to_string(), to_carrier(main)),
            ("lib.chz".to_string(), to_carrier(lib_tail)),
        ],
        expect: expect_of(&want, true),
    });
}

/// A user-only cell: the files as written.
fn only(cells: &mut Vec<Cell>, name: &str, files: &[(&str, &str)], want: Want) {
    cells.push(Cell {
        name: name.to_string(),
        files: files
            .iter()
            .map(|(p, s)| (p.to_string(), s.to_string()))
            .collect(),
        expect: expect_of(&want, false),
    });
}

#[test]
fn carrier_enum_grid() {
    let mut cells = Vec::new();
    let c = &mut cells;

    // --- values -------------------------------------------------------------------------------
    twin(
        c,
        "C1 qualified call",
        "print(Opt1.Som(5), Res2.Okk(5), Res2.Errr(\"x\"))\n",
        Prints("Som(5) Okk(5) Errr('x')"),
    );
    twin(
        c,
        "C2 qualified nullary",
        "print(Opt1.Non)\n",
        Prints("Non"),
    );
    twin(
        c,
        "C3 type-applied",
        "print(Opt1[int].Som(5), Opt1[int].Non, Res2[int, str].Okk(5), Res2[int, str].Errr(\"x\"))\n",
        Prints("Som(5) Non Okk(5) Errr('x')"),
    );
    twin(
        c,
        "C4 bare imported",
        "print(Som(5), Non, Okk(5), Errr(\"x\"))\n",
        Prints("Som(5) Non Okk(5) Errr('x')"),
    );
    twin(
        c,
        "C5 HOF",
        "print([1, 2].map(Som), [1, 2].map(Opt1.Som), [1, 2].map(Opt1[int].Som))\n",
        Prints("[Som(1), Som(2)] [Som(1), Som(2)] [Som(1), Som(2)]"),
    );
    twin(
        c,
        "C6 annotated value",
        "f: fn(int) -> Opt1[int] = Som\nprint(f(3))\n",
        Prints("Som(3)"),
    );
    twin(
        c,
        "C7 pinned by a later use",
        "fn main():\n    f := Som\n    print(f(3))\nmain()\n",
        Prints("Som(3)"),
    );
    twin(
        c,
        "C8 unpinned value",
        "fn main():\n    f := Som\n    print(1)\nmain()\n",
        Rejects("is generic and T is not determined here"),
    );

    // --- patterns and exhaustiveness ----------------------------------------------------------
    twin(
        c,
        "C9 bare pattern",
        "v: Opt1[int] = Som(3)\nmatch v:\n    Som(n): print(n)\n    Non: print(0)\n",
        Prints("3"),
    );
    twin(
        c,
        "C10 qualified pattern",
        "v: Opt1[int] = Som(3)\nmatch v:\n    Opt1.Som(n): print(n)\n    Opt1.Non: print(0)\n",
        Prints("3"),
    );
    twin(
        c,
        "C11 two-parameter pattern",
        "r: Res2[int, str] = Errr(\"e\")\nmatch r:\n    Okk(n): print(n)\n    Errr(e): print(e)\n",
        Prints("e"),
    );
    twin(
        c,
        "C12 exhaustiveness",
        "v: Opt1[int] = Som(3)\nmatch v:\n    Som(n): print(n)\n",
        Rejects("non-exhaustive match on Opt1: missing Non"),
    );
    twin(
        c,
        "C13 exhaustiveness, two parameters",
        "r: Res2[int, str] = Okk(1)\nmatch r:\n    Okk(n): print(n)\n",
        Rejects("non-exhaustive match on Res2: missing Errr"),
    );
    twin(
        c,
        "C14 nested",
        "w: Opt1[Opt1[int]] = Som(Non)\nmatch w:\n    Som(Non): print(1)\n    Som(Som(n)): print(n)\n    Non: print(0)\n",
        Prints("1"),
    );

    // --- what must still fail -----------------------------------------------------------------
    twin(
        c,
        "C15 no int-to-float payload",
        "x: Opt1[float] = Som(1)\n",
        Rejects("write 1.0"),
    );
    twin(
        c,
        "C16 arity",
        "print(Som(1, 2))\n",
        Rejects("Som() expects 1 argument(s), got 2"),
    );
    twin(
        c,
        "C17 payload mismatch",
        "x: Opt1[int] = Som(\"s\")\n",
        RejectsAs(
            "cannot assign Opt1[str] to variable of type Opt1[int]",
            "cannot assign str? to variable of type int?",
        ),
    );
    twin(
        c,
        "C18 the payload hint is a seed only",
        "x: Opt1[Opt1[int]] = Som(5)\n",
        RejectsAs(
            "cannot assign Opt1[int] to variable of type Opt1[Opt1[int]]",
            "cannot assign int? to variable of type int??",
        ),
    );
    // Red when the bare-callee rule also takes a nullary variant (`Non() expects 0 argument(s)`).
    twin(
        c,
        "C30 a nullary variant is not callable",
        "x: Opt1[int] = Non(1)\n",
        Rejects("is not callable"),
    );
    twin(
        c,
        "C19 nested witness",
        "w: Opt1[Opt1[int]] = Som(Non)\nmatch w:\n    Som(Som(n)): print(n)\n    Non: print(0)\n",
        Rejects("pattern `Som(Non)` is not covered"),
    );
    twin(
        c,
        "C20 nested witness, two parameters",
        "r: Res2[Res2[int, str], str] = Okk(Errr(\"x\"))\nmatch r:\n    Okk(Okk(n)): print(n)\n    Errr(e): print(e)\n",
        Rejects("pattern `Okk(Errr(_))` is not covered"),
    );

    // --- a type alias of the enum is a path head ----------------------------------------------
    twin(
        c,
        "C21 alias value",
        "type F = Opt1[int]\nprint(F.Som(1))\n",
        Prints("Som(1)"),
    );
    twin(
        c,
        "C22 alias nullary",
        "type F = Opt1[int]\nprint(F.Non)\n",
        Prints("Non"),
    );
    twin(
        c,
        "C23 alias pattern",
        "type F = Opt1[int]\nv: F = Opt1.Som(3)\nmatch v:\n    F.Som(n): print(n)\n    F.Non: print(0)\n",
        Prints("3"),
    );
    twin(
        c,
        "C24 alias of a two-parameter enum",
        "type G = Res2[int, str]\nprint(G.Okk(1), G.Errr(\"x\"))\n",
        Prints("Okk(1) Errr('x')"),
    );
    twin(
        c,
        "C25 an alias pins its type argument",
        "type F = Opt1[int]\nx: F = F.Som(\"s\")\n",
        Rejects("has type str, expected int"),
    );
    twin(
        c,
        "C26 alias variant as a fn value",
        "type F = Opt1[int]\nprint([1, 2].map(F.Som))\nf := F.Som\nprint(f(3))\n",
        Prints("[Som(1), Som(2)]\nSom(3)"),
    );
    twin_lib(
        c,
        "C27 module-qualified alias",
        "type Tone = Opt1[int]\n",
        "import lib\nprint(lib.Tone.Som(1), lib.Tone.Non)\nv: lib.Tone = lib.Tone.Som(3)\nmatch v:\n    lib.Tone.Som(n): print(n)\n    lib.Tone.Non: print(0)\n",
        Prints("Som(1) Non\n3"),
    );
    twin_lib(
        c,
        "C28 from-imported alias",
        "type Tone = Opt1[int]\n",
        "import Tone from lib\nprint(Tone.Som(1), Tone.Non)\nv: Tone = Tone.Som(3)\nmatch v:\n    Tone.Som(n): print(n)\n    Tone.Non: print(0)\n",
        Prints("Som(1) Non\n3"),
    );
    twin(
        c,
        "C29 a protocol-bound miss reads the method table",
        "protocol Default:\n    fn default() -> Self\nfn mk[T: Default]() -> T:\n    return T.default()\nx := mk[Opt1[int]]()\n",
        Rejects("does not satisfy Default (missing method 'default')"),
    );

    // --- variant import (user enums only) -----------------------------------------------------
    let pre = |body: &str| format!("{ENUMS}{IMPORTS}{body}");
    let enums = |body: &str| format!("{ENUMS}{body}");
    const COLOR: &str = "enum Color:\n    Red\n    Green\n";
    only(
        c,
        "V1 a variant that is not imported stays qualified",
        &[("main.chz", &enums("print(Non)\n"))],
        Rejects("write it qualified as 'Opt1.Non'"),
    );
    only(
        c,
        "V2 a variant import and a same-named fn clash",
        &[(
            "main.chz",
            &enums("import Som from Opt1\nfn Som(x: int) -> int:\n    return x\n"),
        )],
        Rejects("'Som' is already imported"),
    );
    only(
        c,
        "V3 a local shadows an imported variant",
        &[(
            "main.chz",
            &pre("fn main():\n    Non := 5\n    print(Non)\nmain()\n"),
        )],
        Prints("5"),
    );
    only(
        c,
        "V4 variants of an enum from another module",
        &[
            (
                "main.chz",
                "import Color from lib\nimport Red, Green from Color\nprint(Red, Green)\n",
            ),
            ("lib.chz", COLOR),
        ],
        Prints("Red Green"),
    );
    only(
        c,
        "V5 unknown variant",
        &[(
            "main.chz",
            "enum Color:\n    Red\nimport Purple from Color\n",
        )],
        Rejects("enum 'Color' has no variant 'Purple'"),
    );
    only(
        c,
        "V6 an aliased variant import",
        &[(
            "main.chz",
            "enum Color:\n    Red\nimport Red as R from Color\nprint(R)\n",
        )],
        Prints("Red"),
    );
    only(
        c,
        "V7 a struct is not an import source",
        &[("main.chz", "struct S:\n    n: int\nimport X from S\n")],
        Rejects("cannot find module 'S'"),
    );
    only(
        c,
        "V8 a module that resolves wins over a from-imported name",
        &[
            (
                "main.chz",
                "import geo from lib\nimport Point from geo\nprint(geo(), Point(x=2).x)\n",
            ),
            ("lib.chz", "fn geo() -> int:\n    return 1\n"),
            ("geo.chz", "struct Point:\n    x: int\n"),
        ],
        Prints("1 2"),
    );
    only(
        c,
        "V9 a root module cannot capture the prelude's import",
        &[
            (
                "main.chz",
                "print(Some(1), None, Ok(2), Err(\"x\"))\nv: Option[int] = Some(3)\nmatch v:\n    Some(n): print(n)\n    None: print(0)\n",
            ),
            ("Option.chz", "fn helper() -> int:\n    return 7\n"),
            ("Result.chz", "fn helper() -> int:\n    return 8\n"),
        ],
        Prints("Some(1) None Ok(2) Err('x')\n3"),
    );
    only(
        c,
        "V10 a user fn shadows a prelude variant",
        &[(
            "main.chz",
            "fn Some(x: int) -> int:\n    return x + 1\nprint(Some(1))\n",
        )],
        Prints("2"),
    );
    only(
        c,
        "V11 a global and a local shadow a prelude variant",
        &[(
            "main.chz",
            "None := 5\nprint(None)\nfn main():\n    Ok := 3\n    print(Ok)\nmain()\n",
        )],
        Prints("5\n3"),
    );
    only(
        c,
        "V12 a user variant named like a carrier variant stays qualified",
        &[(
            "main.chz",
            "enum E:\n    Some(int)\n    Other\nprint(E.Some(1))\nv: Option[int] = Some(2)\nprint(v)\n",
        )],
        Prints("Some(1)\nSome(2)"),
    );
    only(
        c,
        "V13 an explicit import may not rebind a prelude variant name",
        &[(
            "main.chz",
            "enum Mine:\n    Some(int)\n    Nada\nimport Some, Nada from Mine\n",
        )],
        Rejects("'Some' is already imported from Option by the prelude"),
    );
    only(
        c,
        "V14 an inline None default crosses modules",
        &[
            ("main.chz", "import f from lib\nprint(f(), f(2))\n"),
            (
                "lib.chz",
                "fn f(x: int? = None) -> int:\n    return x ?? 7\n",
            ),
        ],
        Prints("7 2"),
    );
    only(
        c,
        "V15 None stays an inline default",
        &[(
            "main.chz",
            "fn conv[T](a: int, b: T? = None, c: int = 0) -> int:\n    return a + c\nprint(conv[int](1, c=2), conv[int](1))\n",
        )],
        Prints("3 1"),
    );
    only(
        c,
        "V16 a from-import and a variant import bind one name",
        &[
            (
                "main.chz",
                &format!("{COLOR}import Red from lib\nimport Red, Green from Color\n"),
            ),
            ("lib.chz", "fn Red() -> int:\n    return 1\n"),
        ],
        Rejects("'Red' is already imported"),
    );
    only(
        c,
        "V17 one variant imported twice",
        &[(
            "main.chz",
            &format!("{COLOR}import Red from Color\nimport Red from Color\n"),
        )],
        Rejects("'Red' is already imported"),
    );
    only(
        c,
        "V18 a reserved alias",
        &[(
            "main.chz",
            "enum Color:\n    Red\nimport Red as print from Color\n",
        )],
        Rejects("import alias 'print' is reserved (builtin)"),
    );
    only(
        c,
        "V19 a global shadows a variant import",
        &[(
            "main.chz",
            &format!("{COLOR}import Red from Color\nRed := 5\nprint(Red, Color.Red)\n"),
        )],
        Prints("5 Red"),
    );
    only(
        c,
        "V20 a witness for an enum that is not imported stays qualified",
        &[(
            "main.chz",
            &enums(
                "w: Opt1[Opt1[int]] = Opt1.Som(Opt1.Non)\nmatch w:\n    Opt1.Som(Opt1.Som(n)): print(n)\n    Opt1.Non: print(0)\n",
            ),
        )],
        Rejects("pattern `Opt1.Som(Opt1.Non)` is not covered"),
    );
    only(
        c,
        "V21 a witness for a partly imported enum stays qualified",
        &[(
            "main.chz",
            &enums(
                "import Non from Opt1\nw: Opt1[Opt1[int]] = Opt1.Som(Non)\nmatch w:\n    Opt1.Som(Opt1.Som(n)): print(n)\n    Non: print(0)\n",
            ),
        )],
        Rejects("pattern `Opt1.Som(Opt1.Non)` is not covered"),
    );

    run_grid("carrier-enum", &cells);
}
