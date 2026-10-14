//! TICKET-258 (owner rule U15): `x OP= v` means `x = x OP v` for every operator, target shape and
//! type. One cell per compound operator x target shape x operand pair. Each cell builds two programs:
//! the compound statement and its desugar. Both print the target, an alias taken before the
//! statement, and the initial value. Each cell has two columns:
//!
//! 1. CHECK: `chezzi check` gives both programs the same verdict, both ok or both rejected. A
//!    compound-only type rule turns this column red.
//! 2. RUN: `chezzi run` gives both programs the same exit status and stdout, and the alias still
//!    prints as the initial value. A compound-only lowering turns this column red.
//!
//! The operator list is derived from the lexer and the parser: every `lexer::PUNCTUATION` token
//! that the parser reads as a compound `AssignOp`. A new compound operator joins the grid with no
//! edit here.

use std::path::PathBuf;
use std::process::Command;

use chezzi::ast::StmtKind;
use chezzi::lexer::{PUNCTUATION, tokenize};
use chezzi::parser::parse;

/// Every compound operator the parser accepts, as `(compound spelling, binary spelling)`.
fn compound_ops() -> Vec<(String, String)> {
    let mut ops = Vec::new();
    for t in PUNCTUATION {
        let Some(lexeme) = t.lexeme() else { continue };
        let Ok(tokens) = tokenize(&format!("x {lexeme} 1\n")) else {
            continue;
        };
        let Ok(module) = parse(tokens) else { continue };
        if let [stmt] = module.stmts.as_slice()
            && let StmtKind::Assign { op, .. } = &stmt.kind
            && op.to_binop().is_some()
        {
            let bin = lexeme
                .strip_suffix('=')
                .expect("a compound operator ends in `=`");
            ops.push((lexeme.to_string(), bin.to_string()));
        }
    }
    ops
}

/// One operand pair: the target's type, its initial value and the right-hand side.
struct Operands {
    name: &'static str,
    ty: &'static str,
    init: &'static str,
    rhs: &'static str,
}

const OPERANDS: &[Operands] = &[
    Operands {
        name: "int",
        ty: "int",
        init: "12",
        rhs: "3",
    },
    Operands {
        name: "int_float",
        ty: "int",
        init: "12",
        rhs: "2.5",
    },
    Operands {
        name: "int8_const",
        ty: "int8",
        init: "12",
        rhs: "300",
    },
    Operands {
        name: "float",
        ty: "float",
        init: "12.5",
        rhs: "2.5",
    },
    Operands {
        name: "str",
        ty: "str",
        init: "\"ab\"",
        rhs: "\"c\"",
    },
    Operands {
        name: "bytes",
        ty: "bytes",
        init: "b\"ab\"",
        rhs: "b\"c\"",
    },
    Operands {
        name: "list",
        ty: "List[int]",
        init: "[1, 2]",
        rhs: "[3]",
    },
    Operands {
        name: "list_int",
        ty: "List[int]",
        init: "[1, 2]",
        rhs: "2",
    },
    Operands {
        name: "set",
        ty: "Set[int]",
        init: "{1, 2}",
        rhs: "{2, 3}",
    },
    Operands {
        name: "map",
        ty: "Map[str, int]",
        init: "{\"a\": 1}",
        rhs: "{\"b\": 2}",
    },
    Operands {
        name: "struct",
        ty: "V",
        init: "V(12)",
        rhs: "V(3)",
    },
];

/// A user struct with every arithmetic operator hook. Each hook returns a new value.
const PRELUDE: &str = "import int8 from std.ffi
struct V:
    n: int
    fn add(self, other: V) -> V:
        return V(self.n + other.n)
    fn sub(self, other: V) -> V:
        return V(self.n - other.n)
    fn mul(self, other: V) -> V:
        return V(self.n * other.n)
    fn div(self, other: V) -> V:
        return V(self.n / other.n)
    fn mod(self, other: V) -> V:
        return V(self.n % other.n)
struct H[T]:
    v: T
";

const SHAPES: [&str; 8] = [
    "local",
    "global",
    "captured",
    "field",
    "index",
    "key",
    "hook_index",
    "hook_index_split",
];

/// The program for one cell. `stmt` is the compound statement or its desugar, written with `@` for
/// the target. `hook_index_split` ignores the operand type: its `index` returns `int` while its
/// `set_index` takes `float`, so it runs only with the `int_float` operand pair.
fn program(shape: &str, o: &Operands, stmt: &str) -> String {
    let (ty, init) = (o.ty, o.init);
    let (decls, setup, place) = match shape {
        "local" | "captured" => (String::new(), format!("    x: {ty} = {init}\n"), "x"),
        "global" => (format!("x: {ty} = {init}\n"), String::new(), "x"),
        "field" => (String::new(), format!("    h: H[{ty}] = H({init})\n"), "h.v"),
        "index" => (String::new(), format!("    xs: List[{ty}] = [{init}]\n"), "xs[0]"),
        "key" => (String::new(), format!("    m: Map[str, {ty}] = {{\"k\": {init}}}\n"), "m[\"k\"]"),
        "hook_index" => (
            format!(
                "struct Bag:\n    v: {ty}\n    fn index(self, k: int) -> {ty}:\n        return self.v\n    fn set_index(self, k: int, v: {ty}):\n        self.v = v\n"
            ),
            format!("    g := Bag({init})\n"),
            "g[0]",
        ),
        "hook_index_split" => (
            "struct Split:\n    f: float\n    fn index(self, k: int) -> int:\n        return int(self.f)\n    fn set_index(self, k: int, v: float):\n        self.f = v\n".to_string(),
            "    g := Split(12.0)\n".to_string(),
            "g[0]",
        ),
        other => panic!("unknown shape {other}"),
    };
    let stmt = stmt.replace('@', place);
    let write = if shape == "captured" {
        format!("    fn f():\n        {stmt}\n    f()\n")
    } else {
        format!("    {stmt}\n")
    };
    format!(
        "{PRELUDE}{decls}fn main():\n{setup}    alias := {place}\n{write}    print({place})\n    print(alias)\n    print({init})\nmain()\n"
    )
}

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "chezzi-compound-assign-grid-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&d).expect("create the cell directory");
    d
}

/// `chezzi <cmd> <src>`: the exit status, stdout and stderr.
fn chezzi(cmd: &str, name: &str, src: &str) -> (bool, String, String) {
    let path = dir().join(format!("{name}.chz"));
    std::fs::write(&path, src).expect("write the cell");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg(cmd)
        .arg(&path)
        .output()
        .expect("run chezzi");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn compound_assign_equals_its_desugar_in_every_cell() {
    let ops = compound_ops();
    assert!(
        ops.iter().any(|(c, _)| c == "+="),
        "the parser must accept `+=`: {ops:?}"
    );
    let mut bad = Vec::new();
    let mut defined = vec![0; SHAPES.len()];
    for (i, (compound, bin)) in ops.iter().enumerate() {
        for o in OPERANDS {
            for (s, shape) in SHAPES.iter().enumerate() {
                if *shape == "hook_index_split" && o.name != "int_float" {
                    continue;
                }
                let cell = format!("{shape}_{}_{i}", o.name);
                let a = program(shape, o, &format!("@ {compound} {}", o.rhs));
                let b = program(shape, o, &format!("@ = @ {bin} {}", o.rhs));
                let (a_chk, _, a_chk_err) = chezzi("check", &format!("{cell}_compound"), &a);
                let (b_chk, _, b_chk_err) = chezzi("check", &format!("{cell}_desugar"), &b);
                if a_chk != b_chk {
                    bad.push(format!(
                        "CHECK {shape} {} `{compound} {}`: compound ok={a_chk} err={a_chk_err:?}; desugar ok={b_chk} err={b_chk_err:?}",
                        o.ty, o.rhs
                    ));
                }
                let (a_ok, a_out, a_err) = chezzi("run", &format!("{cell}_compound"), &a);
                let (b_ok, b_out, b_err) = chezzi("run", &format!("{cell}_desugar"), &b);
                let lines: Vec<&str> = a_out.lines().collect();
                let alias_kept = !a_ok || (lines.len() == 3 && lines[1] == lines[2]);
                if a_ok != b_ok || a_out != b_out || !alias_kept {
                    bad.push(format!(
                        "RUN {shape} {} `{compound} {}`: compound ok={a_ok} out={a_out:?} err={a_err:?}; desugar ok={b_ok} out={b_out:?} err={b_err:?}",
                        o.ty, o.rhs
                    ));
                }
                if b_ok {
                    defined[s] += 1;
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(dir());
    // A shape whose every desugar cell fails proves nothing: its program template is broken.
    for (s, shape) in SHAPES.iter().enumerate() {
        assert!(
            defined[s] > 0,
            "no `{shape}` cell type-checks: the shape's template is broken"
        );
    }
    assert!(
        bad.is_empty(),
        "{} cell(s) differ from the desugar:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
