//! A protocol's method name has one owner, its `protocol` decl in `std/*.chz`. One grid: every std
//! protocol with a method x receiver kind (a user struct through the operator, a user struct
//! through the bound `T: P`, a builtin through the bound). Each cell generates its program from the
//! decl's own method names, so the method that the operator calls, the method that the bound
//! grants and the decl's method must be one name for the cell to print its expected output. A
//! method renamed in a std decl renames every cell; a second copy of the name anywhere in the
//! checker or the VM then fails a cell.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use chezzi::ast::StmtKind;
use chezzi::base::hooks::{HOOKS, Hook, HookRow, Unhooked};

/// One std protocol decl: its module, its own method names in decl order, and whether every own
/// method is static (no receiver).
struct Decl {
    module: String,
    methods: Vec<String>,
    is_static: bool,
}

/// Every `protocol` decl of the embedded std, by protocol name, read with the real parser.
fn std_decls() -> HashMap<String, Decl> {
    let mut out = HashMap::new();
    for (rel, src) in chezzi::native::std_embed::STD_FILES {
        let tokens = chezzi::lexer::tokenize(src).expect("a std file lexes");
        let module = chezzi::parser::parse(tokens).expect("a std file parses");
        let module_name = format!("std.{}", rel.trim_end_matches(".chz").replace('/', "."));
        for stmt in &module.stmts {
            if let StmtKind::Protocol { name, methods, .. } = &stmt.kind {
                out.insert(
                    name.clone(),
                    Decl {
                        module: module_name.clone(),
                        methods: methods.iter().map(|m| m.name.clone()).collect(),
                        is_static: methods.iter().all(|m| {
                            !m.params
                                .first()
                                .is_some_and(|p| chezzi::ast::is_self_name(&p.name))
                        }),
                    },
                );
            }
        }
    }
    out
}

/// What gives a std protocol its method: a `HOOKS` row, or an `Unhooked` protocol.
#[derive(Clone, Copy)]
enum Owner<'a> {
    Hook(&'a HookRow),
    Unhooked(Unhooked),
}

impl Owner<'_> {
    fn of(protocol: &str) -> Option<Owner<'static>> {
        HOOKS
            .iter()
            .find(|row| row.protocol == protocol)
            .map(Owner::Hook)
            .or_else(|| {
                Unhooked::ALL
                    .into_iter()
                    .find(|u| u.protocol() == protocol)
                    .map(Owner::Unhooked)
            })
    }

    fn method(self) -> &'static str {
        match self {
            Owner::Hook(row) => row.hook.method(),
            Owner::Unhooked(u) => u.method(),
        }
    }
}

/// One cell's program text and the stdout it must print.
struct Cell {
    name: String,
    src: String,
    want: String,
}

/// What one protocol contributes: the bound as a type parameter spells it, the methods a struct
/// `S(n: int)` declares to satisfy it, and per receiver kind the program body and its output.
struct Spec {
    bound: String,
    methods: String,
    /// The operator use on `S` values, and its output. `None` for a protocol no operator calls.
    op: Option<(String, String)>,
    /// The body of `via(a: T, b: T)`, and the call that hands it two `S` values, and the output.
    struct_call: (String, String, String),
    /// The same for a builtin receiver: the body, the call, the output.
    builtin_call: (String, String, String),
}

fn method(sig: &str, body: &str) -> String {
    format!("    fn {sig}:\n        {body}\n")
}

/// The spec of one protocol, with every method name read from its owner.
fn spec(protocol: &str, owner: Owner) -> Spec {
    let m = owner.method();
    let eq = Hook::Eq.method();
    let p = |body: &str| format!("print({body})");
    let arith = |sym: &str| Spec {
        bound: protocol.to_string(),
        methods: method(
            &format!("{m}(self, other: S) -> S"),
            "return S(self.n * 10 + other.n)",
        ),
        op: Some((p(&format!("S(1) {sym} S(2)")), "S(n=12)".into())),
        struct_call: (
            p(&format!("a.{m}(b)")),
            "S(1), S(2)".into(),
            "S(n=12)".into(),
        ),
        builtin_call: (p(&format!("a.{m}(b)")), "6, 2".into(), String::new()),
    };
    let row = match owner {
        Owner::Hook(row) => row,
        Owner::Unhooked(u) => return unhooked_spec(u, m),
    };
    fn unhooked_spec(u: Unhooked, m: &str) -> Spec {
        let p = |body: &str| format!("print({body})");
        match u {
            Unhooked::Error => Spec {
                bound: u.protocol().into(),
                methods: method(&format!("{m}(self) -> str"), "return \"S err\""),
                op: None,
                struct_call: (p(&format!("a.{m}()")), "S(1), S(2)".into(), "S err".into()),
                builtin_call: (
                    p(&format!("a.{m}()")),
                    "\"boom\", \"x\"".into(),
                    "boom".into(),
                ),
            },
            Unhooked::PathLike => Spec {
                bound: u.protocol().into(),
                methods: method(&format!("{m}(self) -> bytes"), "return b\"q\""),
                op: None,
                struct_call: (p(&format!("a.{m}()")), "S(1), S(2)".into(), "b'q'".into()),
                builtin_call: (p(&format!("a.{m}()")), "\"p\", \"x\"".into(), "b'p'".into()),
            },
        }
    }
    match row.hook {
        Hook::Iter => Spec {
            bound: "Iterable[int]".into(),
            methods: method(&format!("{m}(self) -> Iterator[int]"), "yield self.n"),
            op: Some(("for q in S(5):\n    print(q)".into(), "5".into())),
            struct_call: (
                p(&format!("List.from_iter(a.{m}())")),
                "S(5), S(6)".into(),
                "[5]".into(),
            ),
            builtin_call: (
                p(&format!("List.from_iter(a.{m}())")),
                "[5], [6]".into(),
                "[5]".into(),
            ),
        },
        Hook::Next => Spec {
            bound: "Iterator[int]".into(),
            methods: method(
                &format!("{m}(self) -> int?"),
                "if self.n == 0:\n            return None\n        self.n -= 1\n        return self.n",
            ),
            op: Some(("for q in S(2):\n    print(q)".into(), "1\n0".into())),
            struct_call: (p(&format!("a.{m}()")), "S(2), S(2)".into(), "1".into()),
            builtin_call: (
                p(&format!("a.{m}()")),
                format!("[5].{0}(), [6].{0}()", Hook::Iter.method()),
                "5".into(),
            ),
        },
        Hook::Index => Spec {
            bound: "Index[int, int]".into(),
            methods: method(&format!("{m}(self, k: int) -> int"), "return self.n + k"),
            op: Some((p("S(5)[2]"), "7".into())),
            struct_call: (p(&format!("a.{m}(2)")), "S(5), S(6)".into(), "7".into()),
            builtin_call: (p(&format!("a.{m}(1)")), "[5, 6], [7]".into(), "6".into()),
        },
        Hook::SetIndex => {
            // `IndexSet` embeds `Index`: the struct declares the embedded read hook too.
            let read = Hook::Index.method();
            Spec {
                bound: "IndexSet[int, int]".into(),
                methods: method(&format!("{read}(self, k: int) -> int"), "return self.n + k")
                    + &method(&format!("{m}(self, k: int, v: int)"), "self.n = v"),
                op: Some(("x := S(5)\nx[0] = 9\nprint(x[0])".into(), "9".into())),
                struct_call: (
                    format!("a.{m}(0, 9)\n    print(a.{read}(0))"),
                    "S(5), S(6)".into(),
                    "9".into(),
                ),
                builtin_call: (
                    format!("a.{m}(0, 9)\n    print(a.{read}(0))"),
                    "[5], [6]".into(),
                    "9".into(),
                ),
            }
        }
        Hook::Slice => Spec {
            bound: "Slice[List[int]]".into(),
            methods: method(
                &format!("{m}(self, r: Range) -> List[int]"),
                "return [self.n]",
            ),
            op: Some((p("S(5)[0..1]"), "[5]".into())),
            struct_call: (
                p(&format!("a.{m}(0..1)")),
                "S(5), S(6)".into(),
                "[5]".into(),
            ),
            builtin_call: (
                p(&format!("a.{m}(0..1)")),
                "[5, 6], [7]".into(),
                "[5]".into(),
            ),
        },
        Hook::Str => Spec {
            bound: "Stringable".into(),
            methods: method(&format!("{m}(self) -> str"), "return \"S!\""),
            op: Some((p("S(5)"), "S!".into())),
            struct_call: (p(&format!("a.{m}()")), "S(5), S(6)".into(), "S!".into()),
            builtin_call: (p(&format!("a.{m}()")), "5, 6".into(), "5".into()),
        },
        Hook::Eq => Spec {
            bound: "Eq".into(),
            methods: method(&format!("{m}(self, other: S) -> bool"), "return true"),
            op: Some((p("S(1) == S(2)"), "true".into())),
            struct_call: (p(&format!("a.{m}(b)")), "S(1), S(2)".into(), "true".into()),
            builtin_call: (p(&format!("a.{m}(b)")), "5, 6".into(), "false".into()),
        },
        Hook::Hash => Spec {
            bound: "Hashable".into(),
            methods: method(&format!("{eq}(self, other: S) -> bool"), "return true")
                + &method(&format!("{m}(self) -> int"), "return 0"),
            op: Some((p("{S(1), S(2)}.len()"), "1".into())),
            struct_call: (p(&format!("a.{m}()")), "S(1), S(2)".into(), "0".into()),
            builtin_call: (
                p(&format!("a.{m}() == b.{m}()")),
                "5, 5".into(),
                "true".into(),
            ),
        },
        Hook::Compare => Spec {
            bound: "Comparable".into(),
            methods: method(
                &format!("{m}(self, other: S) -> int"),
                "return other.n - self.n",
            ),
            op: Some((p("S(1) < S(2)"), "false".into())),
            struct_call: (p(&format!("a.{m}(b)")), "S(1), S(2)".into(), "1".into()),
            builtin_call: (p(&format!("a.{m}(b)")), "5, 6".into(), "-1".into()),
        },
        Hook::Neg => Spec {
            bound: "Neg".into(),
            methods: method(&format!("{m}(self) -> S"), "return S(0 - self.n)"),
            op: Some((p("-S(5)"), "S(n=-5)".into())),
            struct_call: (
                p(&format!("a.{m}()")),
                "S(5), S(6)".into(),
                "S(n=-5)".into(),
            ),
            builtin_call: (p(&format!("a.{m}()")), "5, 6".into(), "-5".into()),
        },
        Hook::Contains => Spec {
            bound: "Contains[int]".into(),
            methods: method(
                &format!("{m}(self, item: int) -> bool"),
                "return item == self.n",
            ),
            op: Some((p("5 in S(5)"), "true".into())),
            struct_call: (p(&format!("a.{m}(5)")), "S(5), S(6)".into(), "true".into()),
            builtin_call: (p(&format!("a.{m}(5)")), "[5], [6]".into(), "true".into()),
        },
        Hook::Add
        | Hook::Sub
        | Hook::Mul
        | Hook::Div
        | Hook::Mod
        | Hook::BitAnd
        | Hook::BitOr
        | Hook::BitXor
        | Hook::Shl
        | Hook::Shr => {
            let mut s = arith(row.operator);
            // The builtin answer is the operator's own answer on the same two ints.
            s.builtin_call.2 = int_op(row.operator, 6, 2);
            s
        }
    }
}

/// `a <op> b` on two ints, as the language computes it (int `/` floors, as `//` does in Python).
fn int_op(op: &str, a: i64, b: i64) -> String {
    match op {
        "+" => a + b,
        "-" => a - b,
        "*" => a * b,
        "/" => a.div_euclid(b),
        "%" => a.rem_euclid(b),
        "&" => a & b,
        "|" => a | b,
        "^" => a ^ b,
        "<<" => a << b,
        ">>" => a >> b,
        other => panic!("not a binary int operator: {other}"),
    }
    .to_string()
}

fn cells() -> Vec<Cell> {
    let decls = std_decls();
    let mut protocols: Vec<&String> = decls
        .iter()
        .filter(|(_, d)| !d.methods.is_empty())
        .map(|(name, _)| name)
        .collect();
    protocols.sort();
    let mut out = Vec::new();
    for protocol in protocols {
        let decl = &decls[protocol.as_str()];
        let Some(owner) = Owner::of(protocol) else {
            // A static requirement (`convert(x: S) -> Self`): no receiver, so no operator and no
            // builtin grant. The decl column fails any other protocol with no owner.
            continue;
        };
        let s = spec(protocol, owner);
        let import = if decl.module == "std.prelude" {
            String::new()
        } else {
            format!("import {protocol} from {}\n", decl.module)
        };
        // A method that returns a cursor names `Iterator` in its signature.
        let cursor = if s.methods.contains("Iterator[") && protocol != "Iterator" {
            "import Iterator from std.iter\n"
        } else {
            ""
        };
        let ty = format!("{cursor}struct S:\n    n: int\n{}", s.methods);
        let via = |body: &str| format!("fn via[T: {}](a: T, b: T):\n    {body}\n", s.bound);
        if let Some((op, want)) = &s.op {
            out.push(Cell {
                name: format!("{protocol}/struct/operator"),
                src: format!("{ty}\n{op}\n"),
                want: want.clone(),
            });
        }
        let (body, args, want) = &s.struct_call;
        out.push(Cell {
            name: format!("{protocol}/struct/bound"),
            src: format!("{import}{ty}\n{}via({args})\n", via(body)),
            want: want.clone(),
        });
        let (body, args, want) = &s.builtin_call;
        out.push(Cell {
            name: format!("{protocol}/builtin/bound"),
            src: format!("{import}{}via({args})\n", via(body)),
            want: want.clone(),
        });
    }
    out
}

fn run(name: &str, src: &str) -> (bool, String, String) {
    let dir = std::env::temp_dir().join(format!(
        "chezzi_protocol_method_grid_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create the cell dir");
    let path: PathBuf = dir.join(format!("{}.chz", name.replace('/', "_")));
    std::fs::write(&path, src).expect("write the cell");
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .output()
        .expect("run chezzi");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// The decl column: each `HOOKS` and `Unhooked` protocol's method is the one own method of its std
/// decl, and each other std protocol with a method has only static methods.
#[test]
fn every_hook_method_is_its_std_decls_method() {
    let decls = std_decls();
    let mut bad = Vec::new();
    let owned = HOOKS
        .iter()
        .map(|row| (row.protocol, row.hook.method()))
        .chain(
            Unhooked::ALL
                .into_iter()
                .map(|u| (u.protocol(), u.method())),
        );
    for (protocol, method) in owned {
        match decls.get(protocol) {
            Some(d) if d.methods == [method] => {}
            Some(d) => bad.push(format!(
                "{protocol}: the owner calls '{method}', the std decl declares {:?}",
                d.methods
            )),
            None => bad.push(format!("{protocol}: no std decl")),
        }
    }
    for (name, d) in &decls {
        if !d.methods.is_empty() && Owner::of(name).is_none() && !d.is_static {
            bad.push(format!(
                "std protocol {name} has an instance method and no hook or Unhooked owner"
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} decl cells wrong:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// The operator and bound columns, one generated program per cell.
#[test]
fn every_protocol_method_cell_calls_the_decls_name() {
    let mut bad = Vec::new();
    let cells = cells();
    for cell in &cells {
        let (ok, stdout, stderr) = run(&cell.name, &cell.src);
        if !ok || stdout != cell.want {
            bad.push(format!(
                "{}: want {:?}, got {:?} (ok={ok})\n{stderr}--- program ---\n{}",
                cell.name, cell.want, stdout, cell.src
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} cells wrong:\n{}",
        bad.len(),
        cells.len(),
        bad.join("\n")
    );
    eprintln!("protocol_method_grid: {} cells green", cells.len());
}
