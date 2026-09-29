//! TICKET-182: the call-binding grid. Every callee kind x every argument shape x shadowing, one
//! generated program per cell, run through the built `chezzi` binary. The expected value is
//! CPython's binding: a keyword fills the parameter of that name, an omitted defaulted parameter
//! takes its default, and a callee name shadowed by a nearer binder binds against that binder.
//!
//! The checker decides which declaration a call binds against and records the argument slot plan
//! (`checker::CallPlanTable`); the compiler lowers from that plan. A red cell here means a second
//! binder has come back, or the plan and the lowering disagree.

use std::path::{Path, PathBuf};
use std::process::Command;

enum Expect {
    Prints(String),
    /// A program the language rejects by a named rule; the fragment is part of the message.
    Rejects(&'static str),
}

struct Cell {
    name: String,
    files: Vec<(String, String)>,
    expect: Expect,
}

/// Replace each `(key, value)` placeholder in `t`, in order.
fn sub(t: &str, pairs: &[(&str, &str)]) -> String {
    let mut s = t.to_string();
    for (k, v) in pairs {
        s = s.replace(k, v);
    }
    s
}

const PARAMS: &str = "a: int, b: int = 20, c: int = 30";
const PARAMS_DFLT_PARAM: &str = "a: int, b: int = a, c: int = 30";
const FIELDS: &str = "    a: int\n    b: int = 20\n    c: int = 30\n";
const FIELDS_DFLT_PARAM: &str = "    a: int\n    b: int = a\n    c: int = 30\n";
const RET: &str = "\"TAG:{a},{b},{c}\"";

/// The four fixed-arity shapes: call arguments and the bound `a,b,c`.
const SHAPES: &[(&str, &str, &str)] = &[
    ("pos", "1, 2, 3", "1,2,3"),
    ("kw_rev", "c=3, b=2, a=1", "1,2,3"),
    ("mixed", "1, c=3", "1,20,3"),
    ("omit", "1", "1,20,30"),
];

const DFLT_PARAM_REJECT: &str = "default value cannot reference parameter 'a'";
const DFLT_FIELD_REJECT: &str = "default value cannot reference field 'a'";
const CALLEE_FILLED_HOLE: &str =
    "is filled by the callee and can only be omitted from the END of a call";
const FN_VALUE_KW: &str = "keyword arguments through a function value need a binding";
const ONLY_SUPPORTED: &str = "named arguments are only supported on";

/// A fixed-arity callee kind: `files(params_or_fields, args)` builds the program.
struct Kind {
    tag: &'static str,
    /// Wraps the expected `a,b,c` into the printed line.
    wrap: fn(&str) -> String,
    /// `(declaration text, call args) -> files`. The declaration text is the params list, or the
    /// field block for a ctor kind.
    build: fn(&str, &str) -> Vec<(String, String)>,
    ctor: bool,
    /// Shapes this kind skips or expects to reject.
    exceptions: &'static [(&'static str, Option<&'static str>)],
}

fn main_only(src: String) -> Vec<(String, String)> {
    vec![("main.chz".to_string(), src)]
}

fn with_lib(lib_path: &str, lib: String, main: String) -> Vec<(String, String)> {
    vec![("main.chz".to_string(), main), (lib_path.to_string(), lib)]
}

fn fn_decl(name: &str, tag: &str, params: &str, indent: &str) -> String {
    let ret = sub(RET, &[("TAG", tag)]);
    format!("{indent}fn {name}({params}) -> str:\n{indent}    return {ret}\n")
}

const PRINT_S: &str = "print(\"{s.a},{s.b},{s.c}\")\n";

fn kinds() -> Vec<Kind> {
    fn tagged(t: &'static str) -> fn(&str) -> String {
        match t {
            "k" => |v| format!("k:{v}"),
            "g" => |v| format!("g:{v}"),
            "s" => |v| format!("s:{v}"),
            "m" => |v| format!("m:{v}"),
            "c" => |v| format!("c:{v}"),
            "opt" => |v| format!("Some('m:{v}')"),
            _ => |v| v.to_string(),
        }
    }
    vec![
        Kind {
            tag: "top_fn",
            wrap: tagged("k"),
            build: |p, a| main_only(format!("{}print(k({a}))\n", fn_decl("k", "k", p, ""))),
            ctor: false,
            exceptions: &[],
        },
        Kind {
            tag: "nested_fn",
            wrap: tagged("k"),
            build: |p, a| {
                main_only(format!(
                    "fn body() -> str:\n{}    return k({a})\nprint(body())\n",
                    fn_decl("k", "k", p, "    ")
                ))
            },
            ctor: false,
            exceptions: &[("mixed", Some(CALLEE_FILLED_HOLE))],
        },
        Kind {
            tag: "value_alias",
            wrap: tagged("k"),
            build: |p, a| {
                main_only(format!(
                    "{}k := top\nprint(k({a}))\n",
                    fn_decl("top", "k", p, "")
                ))
            },
            ctor: false,
            exceptions: &[("mixed", Some(CALLEE_FILLED_HOLE)), ("dflt_param", None)],
        },
        Kind {
            tag: "imported_fn",
            wrap: tagged("k"),
            build: |p, a| {
                with_lib(
                    "lib.chz",
                    fn_decl("k", "k", p, ""),
                    format!("import k from lib\nprint(k({a}))\n"),
                )
            },
            ctor: false,
            exceptions: &[],
        },
        Kind {
            tag: "qualified_fn",
            wrap: tagged("k"),
            build: |p, a| {
                with_lib(
                    "lib.chz",
                    fn_decl("k", "k", p, ""),
                    format!("import lib\nprint(lib.k({a}))\n"),
                )
            },
            ctor: false,
            exceptions: &[],
        },
        Kind {
            tag: "full_path_fn",
            wrap: tagged("k"),
            build: |p, a| {
                with_lib(
                    "pkg/lib.chz",
                    fn_decl("k", "k", p, ""),
                    format!("import pkg.lib\nprint(pkg.lib.k({a}))\n"),
                )
            },
            ctor: false,
            exceptions: &[],
        },
        Kind {
            tag: "struct_ctor",
            wrap: tagged(""),
            build: |f, a| main_only(format!("struct S:\n{f}s := S({a})\n{PRINT_S}")),
            ctor: true,
            exceptions: &[],
        },
        Kind {
            tag: "qualified_ctor",
            wrap: tagged(""),
            build: |f, a| {
                with_lib(
                    "lib.chz",
                    format!("struct S:\n{f}"),
                    format!("import lib\ns := lib.S({a})\n{PRINT_S}"),
                )
            },
            ctor: true,
            exceptions: &[],
        },
        Kind {
            tag: "alias_ctor",
            wrap: tagged(""),
            build: |f, a| main_only(format!("struct S:\n{f}type Q = S\ns := Q({a})\n{PRINT_S}")),
            ctor: true,
            exceptions: &[],
        },
        Kind {
            tag: "generic_fn",
            wrap: tagged("g"),
            build: |p, a| {
                let p = p.replacen("a: int", "a: T", 1);
                let ret = sub(RET, &[("TAG", "g")]);
                main_only(format!(
                    "fn k[T]({p}) -> str:\n    return {ret}\nprint(k({a}))\n"
                ))
            },
            ctor: false,
            exceptions: &[("dflt_param", None)],
        },
        Kind {
            tag: "generic_ctor",
            wrap: tagged(""),
            build: |f, a| {
                let f = f.replacen("a: int", "a: T", 1);
                main_only(format!("struct G[T]:\n{f}s := G({a})\n{PRINT_S}"))
            },
            ctor: true,
            exceptions: &[("dflt_param", None)],
        },
        Kind {
            tag: "static_method",
            wrap: tagged("s"),
            build: |p, a| {
                main_only(format!(
                    "struct S:\n    x: int\n{}print(S.mk({a}))\n",
                    fn_decl("mk", "s", p, "    ")
                ))
            },
            ctor: false,
            exceptions: &[],
        },
        Kind {
            tag: "qualified_static",
            wrap: tagged("s"),
            build: |p, a| {
                with_lib(
                    "lib.chz",
                    format!("struct S:\n    x: int\n{}", fn_decl("mk", "s", p, "    ")),
                    format!("import lib\nprint(lib.S.mk({a}))\n"),
                )
            },
            ctor: false,
            exceptions: &[],
        },
        Kind {
            tag: "instance_method",
            wrap: tagged("m"),
            build: |p, a| {
                main_only(format!(
                    "struct S:\n    x: int\n{}print(S(0).m({a}))\n",
                    fn_decl("m", "m", &format!("self, {p}"), "    ")
                ))
            },
            ctor: false,
            exceptions: &[],
        },
        Kind {
            tag: "enum_method",
            wrap: tagged("m"),
            build: |p, a| {
                main_only(format!(
                    "enum E:\n    A\n    B\n{}print(E.A.m({a}))\n",
                    fn_decl("m", "m", &format!("self, {p}"), "    ")
                ))
            },
            ctor: false,
            exceptions: &[],
        },
        Kind {
            tag: "opt_carrier_method",
            wrap: tagged("opt"),
            build: |p, a| {
                main_only(format!(
                    "struct S:\n    x: int\n{}o: S? = Some(S(0))\nprint(o?.m({a}))\n",
                    fn_decl("m", "m", &format!("self, {p}"), "    ")
                ))
            },
            ctor: false,
            exceptions: &[],
        },
        Kind {
            tag: "protocol_method",
            wrap: tagged("m"),
            build: |p, a| {
                main_only(format!(
                    "protocol P:\n    fn m(self, a: int, b: int, c: int) -> str\nstruct S:\n    x: int\n{}p: P = S(0)\nprint(p.m({a}))\n",
                    fn_decl("m", "m", &format!("self, {p}"), "    ")
                ))
            },
            ctor: false,
            exceptions: &[],
        },
        Kind {
            tag: "protocol_static_via_bound",
            wrap: tagged("s"),
            build: |p, a| {
                main_only(format!(
                    "protocol Mk:\n    fn mk(a: int, b: int, c: int) -> str\nstruct S:\n    x: int\n{}fn via[T: Mk]() -> str:\n    return T.mk({a})\nprint(via[S]())\n",
                    fn_decl("mk", "s", p, "    ")
                ))
            },
            ctor: false,
            exceptions: &[],
        },
    ]
}

fn fixed_cells() -> Vec<Cell> {
    let mut cells = Vec::new();
    for k in kinds() {
        let (params, dflt) = if k.ctor {
            (FIELDS, FIELDS_DFLT_PARAM)
        } else {
            (PARAMS, PARAMS_DFLT_PARAM)
        };
        let ex = |shape: &str| {
            k.exceptions
                .iter()
                .find(|(s, _)| *s == shape)
                .map(|(_, e)| *e)
        };
        for (shape, args, bound) in SHAPES {
            let expect = match ex(shape) {
                Some(Some(frag)) => Expect::Rejects(frag),
                Some(None) => continue,
                None => Expect::Prints((k.wrap)(bound)),
            };
            cells.push(Cell {
                name: format!("{}/{shape}", k.tag),
                files: (k.build)(params, args),
                expect,
            });
        }
        if ex("dflt_param").is_none() {
            cells.push(Cell {
                name: format!("{}/dflt_param", k.tag),
                files: (k.build)(dflt, "1"),
                expect: Expect::Rejects(if k.ctor {
                    DFLT_FIELD_REJECT
                } else {
                    DFLT_PARAM_REJECT
                }),
            });
        }
    }
    cells
}

fn cell(name: &str, src: &str, expect: Expect) -> Cell {
    Cell {
        name: name.to_string(),
        files: main_only(src.to_string()),
        expect,
    }
}

fn prints(s: &str) -> Expect {
    Expect::Prints(s.to_string())
}

/// Pre-existing callee-filled and value-call behaviour, kept.
fn value_cells() -> Vec<Cell> {
    let top = fn_decl("top", "k", PARAMS, "");
    let closure = "k := fn(a: int, b: int, c: int) -> str: \"c:{a},{b},{c}\"\n";
    let param = "fn body(k: fn(a: int, b: int, c: int) -> str) -> str:\n    return k(ARGS)\nfn top3(a: int, b: int, c: int) -> str:\n    return \"c:{a},{b},{c}\"\nprint(body(top3))\n";
    vec![
        cell(
            "value_alias/kw_prefix",
            &format!("{top}k := top\nprint(k(a=1, b=2))\n"),
            prints("k:1,2,30"),
        ),
        cell(
            "local_closure/pos",
            &format!("{closure}print(k(1, 2, 3))\n"),
            prints("c:1,2,3"),
        ),
        cell(
            "local_closure/kw_rev",
            &format!("{closure}print(k(c=3, b=2, a=1))\n"),
            prints("c:1,2,3"),
        ),
        cell(
            "local_closure/mixed",
            &format!("{closure}print(k(1, c=3))\n"),
            Expect::Rejects("missing required argument 'b'"),
        ),
        cell(
            "fn_param/pos",
            &sub(param, &[("ARGS", "1, 2, 3")]),
            prints("c:1,2,3"),
        ),
        cell(
            "fn_param/kw_rev",
            &sub(param, &[("ARGS", "c=3, b=2, a=1")]),
            Expect::Rejects(FN_VALUE_KW),
        ),
        cell(
            "newtype_ctor/pos",
            "newtype N = int\nprint(N(5))\n",
            prints("N(5)"),
        ),
        cell(
            "newtype_ctor/kw",
            "newtype N = int\nprint(N(x=5))\n",
            Expect::Rejects(ONLY_SUPPORTED),
        ),
    ]
}

fn variadic_cells() -> Vec<Cell> {
    let body = "    return \"TAG:{a}+{rest.len()}\"\n";
    let calls = |h: &str| format!("print({h}(1, 2, 3))\nprint({h}(a=1))\n");
    let f = sub(body, &[("TAG", "k")]);
    let s = sub(body, &[("TAG", "s")]).replace("    return", "        return");
    let m = sub(body, &[("TAG", "m")]).replace("    return", "        return");
    vec![
        cell(
            "top_fn/variadic",
            &format!("fn k(a: int, ...rest: int) -> str:\n{f}{}", calls("k")),
            prints("k:1+2\nk:1+0"),
        ),
        cell(
            "static_method/variadic",
            &format!(
                "struct S:\n    x: int\n    fn mk(a: int, ...rest: int) -> str:\n{s}{}",
                calls("S.mk")
            ),
            prints("s:1+2\ns:1+0"),
        ),
        cell(
            "instance_method/variadic",
            &format!(
                "struct S:\n    x: int\n    fn m(self, a: int, ...rest: int) -> str:\n{m}{}",
                calls("S(0).m")
            ),
            prints("m:1+2\nm:1+0"),
        ),
    ]
}

fn provider_ctor_cells() -> Vec<Cell> {
    let decl = "struct W[T]:\n    v: T\nfn mkl[T]() -> List[W[T]]:\n    xs: List[W[T]] = []\n    return xs\nstruct GP[T]:\n    a: int\n    items: List[W[T]] = mkl()\n";
    vec![
        cell(
            "generic_provider_ctor/turbofish_omit",
            &format!("{decl}g := GP[int](1)\nprint(\"{{g.a}},{{g.items.len()}}\")\n"),
            prints("1,0"),
        ),
        cell(
            "generic_provider_ctor/bare_omit",
            &format!("{decl}g := GP(1)\nprint(g.a)\n"),
            Expect::Rejects("can only be filled with explicit type arguments"),
        ),
    ]
}

/// A callee name shadowed by a nearer binder of a different signature binds against the binder.
fn shadow_cells() -> Vec<Cell> {
    // (row, outer declaration, lib file, callee name)
    let outers: &[(&str, String, Option<String>, &str)] = &[
        ("top_fn", fn_decl("k", "k", PARAMS, ""), None, "k"),
        (
            "imported_fn",
            "import k from lib\n".to_string(),
            Some(fn_decl("k", "k", PARAMS, "")),
            "k",
        ),
        ("struct_ctor", format!("struct S:\n{FIELDS}"), None, "S"),
        (
            "alias_ctor",
            format!("struct S:\n{FIELDS}type Q = S\n"),
            None,
            "Q",
        ),
    ];
    let mut cells = Vec::new();
    for (row, outer, lib, n) in outers {
        for (shape, args) in [("omit", "1"), ("kw", "n=1")] {
            let binders = [
                (
                    "shadow_nested_fn",
                    format!(
                        "fn body() -> str:\n    fn {n}(n: int) -> str:\n        return \"S{{n}}\"\n    return {n}({args})\nprint(body())\n"
                    ),
                    prints("S1"),
                ),
                (
                    "shadow_closure",
                    format!(
                        "fn body() -> str:\n    {n} := fn(n: int) -> str: \"S{{n}}\"\n    return {n}({args})\nprint(body())\n"
                    ),
                    prints("S1"),
                ),
                (
                    "shadow_param",
                    format!(
                        "fn body({n}: fn(n: int) -> str) -> str:\n    return {n}({args})\nprint(body(fn(n: int) -> str: \"S{{n}}\"))\n"
                    ),
                    if shape == "kw" {
                        Expect::Rejects(FN_VALUE_KW)
                    } else {
                        prints("S1")
                    },
                ),
            ];
            for (b, body, expect) in binders {
                let main = format!("{outer}{body}");
                let files = match lib {
                    Some(l) => with_lib("lib.chz", l.clone(), main),
                    None => main_only(main),
                };
                cells.push(Cell {
                    name: format!("{row}/{b}/{shape}"),
                    files,
                    expect,
                });
            }
        }
    }
    cells
}

/// `spawn`/`defer` evaluate a default fill at the statement, as Go evaluates `defer f(g())`.
fn stmt_fill_cells() -> Vec<Cell> {
    let decl = "fn note() -> int:\n    print(\"default ran\")\n    return 20\nfn k(a: int, b: int = note(), c: int = 30):\n    print(\"k:{a},{b},{c}\")\nfn ks(out: Channel[str], a: int, b: int = note(), c: int = 30):\n    out.send(\"k:{a},{b},{c}\")\n";
    let defer = |call: &str| {
        format!("{decl}fn body():\n    defer {call}\n    print(\"after defer\")\nbody()\n")
    };
    let spawn = |call: &str| {
        format!(
            "{decl}fn body():\n    ch := Channel[str](1)\n    parallel:\n        spawn {call}\n        print(\"after spawn\")\n        print(ch.recv())\nbody()\n"
        )
    };
    vec![
        cell(
            "stmt_fill/defer_omit",
            &defer("k(1)"),
            prints("default ran\nafter defer\nk:1,20,30"),
        ),
        cell(
            "stmt_fill/defer_kw",
            &defer("k(c=3, a=1)"),
            prints("default ran\nafter defer\nk:1,20,3"),
        ),
        cell(
            "stmt_fill/spawn_omit",
            &spawn("ks(ch, 1)"),
            prints("default ran\nafter spawn\nk:1,20,30"),
        ),
        cell(
            "stmt_fill/spawn_kw",
            &spawn("ks(ch, c=3, a=1)"),
            prints("default ran\nafter spawn\nk:1,20,3"),
        ),
    ]
}

/// A native, builtin or `extern` callee takes no named arguments, as CPython's `chr` does not.
fn native_cells() -> Vec<Cell> {
    let counter = "struct Counter:\n    n: int\n    fn add(self, amount: int = 1) -> int:\n        return self.n + amount\n";
    let sock = |call: &str| {
        format!(
            "import std.net\nfn f(sk: net.Socket) -> str:\n    r := {call}\n    return \"{{r}}\"\nprint(\"ok\")\n"
        )
    };
    let req = |call: &str| {
        format!(
            "import std.request\nfn f() -> str:\n    r := {call}\n    return \"{{r}}\"\nprint(\"ok\")\n"
        )
    };
    let fn_field = "struct H:\n    map: fn(int) -> int\nh := H(fn(x: int) -> int: x)\n";
    vec![
        cell(
            "builtin_receiver/pos",
            "s := Set([1, 2])\ns.add(3)\nprint(s.len())\n",
            prints("3"),
        ),
        cell(
            "builtin_receiver/kw",
            "s := Set([1, 2])\ns.add(x=3)\nprint(s.len())\n",
            Expect::Rejects("'add' takes no named arguments"),
        ),
        cell(
            "builtin_receiver_user_twin/kw",
            &format!("{counter}s := Set([1, 2])\ns.add(x=3)\nprint(s.len())\n"),
            Expect::Rejects("'add' takes no named arguments"),
        ),
        cell("native_method/pos", &sock("sk.read(10, 5)"), prints("ok")),
        cell(
            "native_method/kw",
            &sock("sk.read(10, timeout_ms=5)"),
            Expect::Rejects("'read' takes no named arguments"),
        ),
        cell(
            "native_module_fn/pos",
            &req("request.get(\"http://x\", 5)"),
            prints("ok"),
        ),
        cell(
            "native_module_fn/kw",
            &req("request.get(\"http://x\", timeout_ms=5)"),
            Expect::Rejects("'get' takes no named arguments"),
        ),
        cell("native_free_fn/pos", "print(chr(65))\n", prints("A")),
        cell(
            "native_free_fn/kw",
            "print(chr(65, n=66))\n",
            Expect::Rejects("'chr' takes no named arguments"),
        ),
        cell(
            "builtin_ctor/pos",
            "s: Set[int] = Set([1])\nprint(s.len())\n",
            prints("1"),
        ),
        cell(
            "builtin_ctor/kw",
            "s: Set[int] = Set(xs=[1])\nprint(s.len())\n",
            Expect::Rejects("'Set' takes no named arguments"),
        ),
        cell(
            "fn_field/pos",
            &format!("{fn_field}print(h.map(4))\n"),
            prints("4"),
        ),
        cell(
            "fn_field/kw",
            &format!("{fn_field}print(h.map(arg=4))\n"),
            Expect::Rejects(ONLY_SUPPORTED),
        ),
    ]
}

fn grid() -> Vec<Cell> {
    let mut cells = fixed_cells();
    cells.extend(value_cells());
    cells.extend(variadic_cells());
    cells.extend(provider_ctor_cells());
    cells.extend(shadow_cells());
    cells.extend(stmt_fill_cells());
    cells.extend(native_cells());
    cells
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

/// Cells red on the pre-TICKET-182 binary. They must stay red here; when one turns green, remove
/// it from the list.
const PINNED_RED: &[&str] = &[
    "nested_fn/mixed",
    "nested_fn/dflt_param",
    "value_alias/mixed",
    "local_closure/mixed",
    "newtype_ctor/kw",
    "top_fn/shadow_nested_fn/omit",
    "top_fn/shadow_nested_fn/kw",
    "imported_fn/shadow_nested_fn/omit",
    "imported_fn/shadow_nested_fn/kw",
    "struct_ctor/shadow_nested_fn/omit",
    "struct_ctor/shadow_nested_fn/kw",
    "alias_ctor/shadow_nested_fn/omit",
    "alias_ctor/shadow_nested_fn/kw",
    "builtin_receiver/kw",
    "builtin_receiver_user_twin/kw",
    "native_method/kw",
    "native_module_fn/kw",
    "native_free_fn/kw",
    "builtin_ctor/kw",
];

#[test]
fn call_binding_grid() {
    let root = std::env::temp_dir().join(format!("chezzi-call-grid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cells = grid();
    let mut fails = Vec::new();
    for (i, c) in cells.iter().enumerate() {
        let pinned = PINNED_RED.contains(&c.name.as_str());
        match (run_cell(&root, i, c), pinned) {
            (Err(e), false) => fails.push(e),
            (Ok(()), true) => fails.push(format!("{}: green; remove it from PINNED_RED", c.name)),
            _ => {}
        }
    }
    let _ = std::fs::remove_dir_all(&root);
    assert!(
        fails.is_empty(),
        "{} of {} call-binding cells failed:\n{}",
        fails.len(),
        cells.len(),
        fails.join("\n")
    );
}
