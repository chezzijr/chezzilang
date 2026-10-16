//! TICKET-264: a function is a function. Every callee kind x every call path x every arity shape,
//! one generated program per cell, run through the built `chezzi` binary. The oracle is the TWIN:
//! the direct call of the same callee with the same arguments. A cell passes when its program and
//! its twin agree on the exit status, the stdout and the first error line (location stripped, the
//! written callee name substituted). A deviation is one of the rules E1-E5 below, each a language
//! decision, never a cell-by-cell list.
//!
//! Rows come from the seven kinds the ticket names (top fn, nested fn, method, lambda, builtin,
//! native fn, struct ctor), plus an extern C variadic. The builtin rows come from
//! `Builtin::ALL` filtered by `first_class()`, through the exhaustive `builtin_input`.

use chezzi::base::builtin::Builtin;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

const FN_VALUE_KW: &str = "keyword arguments through a function value need a binding";
const BOUND_METHOD: &str = "a bound method is not a value";
const IS_A_TYPE: &str = "is a type, not a value";
const EXPECTED_FN: &str = "expected fn(";

/// Cells red today, by name. A cell that turns green must leave the list.
const FN_VALUE_PINNED_RED: &[&str] = &[
    "builtin_chr/-/binding/named",
    "builtin_chr/-/defer/named",
    "builtin_chr/-/lib/named",
    "builtin_chr/-/slot/named",
    "builtin_chr/-/spawn/named",
    "builtin_ord/-/binding/named",
    "builtin_ord/-/defer/named",
    "builtin_ord/-/lib/named",
    "builtin_ord/-/slot/named",
    "builtin_ord/-/spawn/named",
    "builtin_panic/-/binding/named",
    "builtin_panic/-/defer/named",
    "builtin_panic/-/lib/named",
    "builtin_panic/-/slot/named",
    "builtin_panic/-/spawn/named",
    "builtin_print/-/binding/few",
    "builtin_print/-/binding/named",
    "builtin_print/-/binding/surplus",
    "builtin_print/-/call_result/exact",
    "builtin_print/-/call_result/few",
    "builtin_print/-/call_result/named",
    "builtin_print/-/call_result/surplus",
    "builtin_print/-/defer/few",
    "builtin_print/-/defer/named",
    "builtin_print/-/defer/surplus",
    "builtin_print/-/field/exact",
    "builtin_print/-/field/few",
    "builtin_print/-/field/named",
    "builtin_print/-/field/surplus",
    "builtin_print/-/hof_map/hof",
    "builtin_print/-/lib/few",
    "builtin_print/-/lib/named",
    "builtin_print/-/lib/surplus",
    "builtin_print/-/param/exact",
    "builtin_print/-/param/few",
    "builtin_print/-/param/named",
    "builtin_print/-/param/surplus",
    "builtin_print/-/slot/few",
    "builtin_print/-/slot/named",
    "builtin_print/-/slot/surplus",
    "builtin_print/-/spawn/few",
    "builtin_print/-/spawn/named",
    "builtin_print/-/spawn/surplus",
    "extern_snprintf/-/binding/surplus",
    "extern_snprintf/-/call_result/exact",
    "extern_snprintf/-/call_result/few",
    "extern_snprintf/-/call_result/surplus",
    "extern_snprintf/-/defer/surplus",
    "extern_snprintf/-/field/exact",
    "extern_snprintf/-/field/few",
    "extern_snprintf/-/field/surplus",
    "extern_snprintf/-/param/exact",
    "extern_snprintf/-/param/few",
    "extern_snprintf/-/param/surplus",
    "extern_snprintf/-/slot/surplus",
    "extern_snprintf/-/spawn/surplus",
    "native_sqrt/-/binding/named",
    "native_sqrt/-/defer/named",
    "native_sqrt/-/spawn/named",
    "nested_fn/var/field/exact",
    "nested_fn/var/field/few",
    "nested_fn/var/field/named",
    "nested_fn/var/field/surplus",
    "nested_fn/var/param/exact",
    "nested_fn/var/param/few",
    "nested_fn/var/param/named",
    "nested_fn/var/param/surplus",
    "nested_fn/var/slot/exact",
    "nested_fn/var/slot/surplus",
    "static_method/var/call_result/exact",
    "static_method/var/call_result/few",
    "static_method/var/call_result/named",
    "static_method/var/call_result/surplus",
    "static_method/var/field/exact",
    "static_method/var/field/few",
    "static_method/var/field/named",
    "static_method/var/field/surplus",
    "static_method/var/lib/exact",
    "static_method/var/lib/surplus",
    "static_method/var/param/exact",
    "static_method/var/param/few",
    "static_method/var/param/named",
    "static_method/var/param/surplus",
    "static_method/var/slot/exact",
    "static_method/var/slot/surplus",
    "top_fn/var/call_result/exact",
    "top_fn/var/call_result/few",
    "top_fn/var/call_result/named",
    "top_fn/var/call_result/surplus",
    "top_fn/var/field/exact",
    "top_fn/var/field/few",
    "top_fn/var/field/named",
    "top_fn/var/field/surplus",
    "top_fn/var/lib/exact",
    "top_fn/var/lib/surplus",
    "top_fn/var/param/exact",
    "top_fn/var/param/few",
    "top_fn/var/param/named",
    "top_fn/var/param/surplus",
    "top_fn/var/slot/exact",
    "top_fn/var/slot/surplus",
];

/// One parameter list a user-declared callee takes, with its arity shapes.
struct PSet {
    tag: &'static str,
    params: &'static str,
    ret: &'static str,
    ty: &'static str,
    /// `(shape tag, call arguments)`.
    shapes: &'static [(&'static str, &'static str)],
}

const ONE: PSet = PSet {
    tag: "one",
    params: "a: int",
    ret: "\"TAG:{a}\"",
    ty: "fn(int) -> str",
    shapes: &[
        ("few", ""),
        ("exact", "1"),
        ("many", "1, 2"),
        ("named", "a=1"),
    ],
};
const TWO: PSet = PSet {
    tag: "two",
    params: "a: int, b: int",
    ret: "\"TAG:{a},{b}\"",
    ty: "fn(int, int) -> str",
    shapes: &[
        ("few", "1"),
        ("exact", "1, 2"),
        ("many", "1, 2, 3"),
        ("named", "b=2, a=1"),
    ],
};
const DFLT: PSet = PSet {
    tag: "dflt",
    params: "a: int, b: int = 20",
    ret: "\"TAG:{a},{b}\"",
    ty: "fn(int, int) -> str",
    shapes: &[
        ("few", ""),
        ("omit", "1"),
        ("exact", "1, 2"),
        ("many", "1, 2, 3"),
    ],
};
const VAR: PSet = PSet {
    tag: "var",
    params: "a: int, ...rest: int",
    ret: "\"TAG:{a}+{rest.len()}\"",
    ty: "fn(int, ...int) -> str",
    shapes: &[
        ("few", ""),
        ("exact", "1"),
        ("surplus", "1, 2, 3"),
        ("named", "a=1"),
    ],
};

/// One callee: the program pieces that define it and how a call reads.
struct Callee {
    id: String,
    /// Module-level import lines.
    imports: String,
    /// Module-level declarations that define the callee.
    decls: String,
    /// Lines (4-space indented) inside `main` that define the callee (a nested fn).
    nested: String,
    callee: String,
    /// The callee's fn type as an annotation, and its return annotation.
    ty: String,
    ret: String,
    /// The callee returns nothing: a call is a statement.
    stmt: bool,
    shapes: Vec<(String, String)>,
    /// The element a `[..].map(callee)` call takes; `None` when the callee has no one-arg form.
    hof_arg: Option<String>,
    /// A lib module can hold the callee (`G := callee`).
    lib_ok: bool,
    /// E3 / E4: the callee is no value; every non-direct path rejects with this fragment.
    no_value: Option<&'static str>,
    /// The name an arity error gives the callee at its direct call.
    written: String,
}

fn fn_text(name: &str, tag: &str, ps: &PSet, indent: &str, recv: &str) -> String {
    let ret = ps.ret.replace("TAG", tag);
    format!(
        "{indent}fn {name}({recv}{}) -> str:\n{indent}    return {ret}\n",
        ps.params
    )
}

fn shapes_of(ps: &PSet) -> Vec<(String, String)> {
    ps.shapes
        .iter()
        .map(|(t, a)| (t.to_string(), a.to_string()))
        .collect()
}

fn user(id: &str, ps: &PSet, decls: String, callee: &str, written: &str) -> Callee {
    Callee {
        id: format!("{id}/{}", ps.tag),
        imports: String::new(),
        decls,
        nested: String::new(),
        callee: callee.to_string(),
        ty: ps.ty.to_string(),
        ret: "str".to_string(),
        stmt: false,
        shapes: shapes_of(ps),
        hof_arg: (ps.tag == "one").then(|| "1".to_string()),
        lib_ok: true,
        no_value: None,
        written: written.to_string(),
    }
}

/// What a first-class builtin takes: its fn type and its shapes. Exhaustive over `Builtin`, so a
/// new builtin forces a decision; every non-first-class variant answers `None`.
struct BuiltinInput {
    ty: &'static str,
    ret: &'static str,
    stmt: bool,
    shapes: &'static [(&'static str, &'static str)],
    hof_arg: Option<&'static str>,
}

fn builtin_input(b: Builtin) -> Option<BuiltinInput> {
    match b {
        Builtin::Print => Some(BuiltinInput {
            ty: "fn(...Any) -> None",
            ret: "",
            stmt: true,
            shapes: &[
                ("few", ""),
                ("exact", "1"),
                ("surplus", "1, 2"),
                ("named", "1, 2, sep=\"-\""),
            ],
            hof_arg: Some("1"),
        }),
        Builtin::Ord => Some(BuiltinInput {
            ty: "fn(str) -> int",
            ret: "int",
            stmt: false,
            shapes: &[
                ("few", ""),
                ("exact", "\"a\""),
                ("many", "\"a\", \"b\""),
                ("named", "c=\"a\""),
            ],
            hof_arg: Some("\"a\""),
        }),
        Builtin::Chr => Some(BuiltinInput {
            ty: "fn(int) -> str",
            ret: "str",
            stmt: false,
            shapes: &[
                ("few", ""),
                ("exact", "97"),
                ("many", "97, 98"),
                ("named", "i=97"),
            ],
            hof_arg: Some("97"),
        }),
        Builtin::Panic => Some(BuiltinInput {
            ty: "",
            ret: "",
            stmt: false,
            shapes: &[
                ("few", ""),
                ("exact", "\"boom\""),
                ("many", "\"a\", \"b\""),
                ("named", "msg=\"boom\""),
            ],
            hof_arg: Some("\"boom\""),
        }),
        Builtin::Int
        | Builtin::Float
        | Builtin::Bool
        | Builtin::Str
        | Builtin::Bytes
        | Builtin::ByteArray
        | Builtin::List
        | Builtin::Map
        | Builtin::Set
        | Builtin::Channel
        | Builtin::Shared
        | Builtin::RwShared
        | Builtin::Atomic
        | Builtin::AtomicInt
        | Builtin::Executor => None,
    }
}

fn builtin_name(b: Builtin) -> &'static str {
    match b {
        Builtin::Print => "print",
        Builtin::Ord => "ord",
        Builtin::Chr => "chr",
        Builtin::Panic => "panic",
        Builtin::Int
        | Builtin::Float
        | Builtin::Bool
        | Builtin::Str
        | Builtin::Bytes
        | Builtin::ByteArray
        | Builtin::List
        | Builtin::Map
        | Builtin::Set
        | Builtin::Channel
        | Builtin::Shared
        | Builtin::RwShared
        | Builtin::Atomic
        | Builtin::AtomicInt
        | Builtin::Executor => "",
    }
}

fn callees() -> Vec<Callee> {
    let mut v = Vec::new();
    for ps in [&ONE, &TWO, &DFLT, &VAR] {
        v.push(user("top_fn", ps, fn_text("k", "k", ps, "", ""), "k", "k"));
        let mut n = user("nested_fn", ps, String::new(), "k", "k");
        n.nested = fn_text("k", "k", ps, "    ", "");
        n.lib_ok = false;
        v.push(n);
        let decls = format!(
            "struct S:\n    x: int\n{}",
            fn_text("mk", "s", ps, "    ", "")
        );
        v.push(user("static_method", ps, decls, "S.mk", "mk"));
    }
    for (ps, lam) in [
        (&ONE, "fn(a: int) -> str: \"k:{a}\""),
        (&TWO, "fn(a: int, b: int) -> str: \"k:{a},{b}\""),
    ] {
        v.push(user("lambda", ps, format!("k := {lam}\n"), "k", "k"));
    }
    let mut inst = user(
        "instance_method",
        &ONE,
        "struct S:\n    x: int\n    fn k(self, a: int) -> str:\n        return \"k:{a}\"\n"
            .to_string(),
        "S(0).k",
        "k",
    );
    inst.no_value = Some(BOUND_METHOD);
    v.push(inst);
    let mut ctor = user(
        "ctor",
        &ONE,
        "struct P:\n    a: int\n".to_string(),
        "P",
        "P",
    );
    ctor.ty = "fn(int) -> P".to_string();
    ctor.ret = "P".to_string();
    ctor.no_value = Some(IS_A_TYPE);
    v.push(ctor);
    for b in Builtin::ALL {
        if let Some(i) = builtin_input(b) {
            v.push(Callee {
                id: format!("builtin_{}/-", builtin_name(b)),
                imports: String::new(),
                decls: String::new(),
                nested: String::new(),
                callee: builtin_name(b).to_string(),
                ty: i.ty.to_string(),
                ret: i.ret.to_string(),
                stmt: i.stmt,
                shapes: i
                    .shapes
                    .iter()
                    .map(|(t, a)| (t.to_string(), a.to_string()))
                    .collect(),
                hof_arg: i.hof_arg.map(str::to_string),
                lib_ok: true,
                no_value: None,
                written: builtin_name(b).to_string(),
            });
        }
    }
    v.push(Callee {
        id: "native_sqrt/-".to_string(),
        imports: "import std.math\n".to_string(),
        decls: String::new(),
        nested: String::new(),
        callee: "math.sqrt".to_string(),
        ty: "fn(float) -> float".to_string(),
        ret: "float".to_string(),
        stmt: false,
        shapes: [
            ("few", ""),
            ("exact", "4.0"),
            ("many", "4.0, 1.0"),
            ("named", "x=4.0"),
        ]
        .iter()
        .map(|(t, a)| (t.to_string(), a.to_string()))
        .collect(),
        hof_arg: Some("4.0".to_string()),
        lib_ok: true,
        no_value: None,
        written: "sqrt".to_string(),
    });
    v.push(Callee {
        id: "extern_snprintf/-".to_string(),
        imports: "import std.ffi\nimport int32 from std.ffi\n".to_string(),
        decls: "extern \"libc\":\n    fn snprintf(buf: ffi.ptr, n: int, fmt: str, ...) -> int32\nBUF := ffi.alloc(64)\n"
            .to_string(),
        nested: String::new(),
        callee: "snprintf".to_string(),
        ty: "fn(ffi.ptr, int, str, ...) -> int32".to_string(),
        ret: "int32".to_string(),
        stmt: false,
        shapes: [("few", "BUF, 64"), ("exact", "BUF, 64, \"plain\""), ("surplus", "BUF, 64, \"%s\", \"x\"")]
            .iter()
            .map(|(t, a)| (t.to_string(), a.to_string()))
            .collect(),
        hof_arg: None,
        lib_ok: false,
        no_value: None,
        written: "snprintf".to_string(),
    });
    v
}

#[derive(Clone, Copy, PartialEq)]
enum CallPath {
    Direct,
    Binding,
    Slot,
    Param,
    Field,
    Lib,
    CallResult,
    Defer,
    Spawn,
    HofMap,
    HofSubmit,
}

const PATHS: [CallPath; 11] = [
    CallPath::Direct,
    CallPath::Binding,
    CallPath::Slot,
    CallPath::Param,
    CallPath::Field,
    CallPath::Lib,
    CallPath::CallResult,
    CallPath::Defer,
    CallPath::Spawn,
    CallPath::HofMap,
    CallPath::HofSubmit,
];

impl CallPath {
    fn tag(self) -> &'static str {
        match self {
            CallPath::Direct => "direct",
            CallPath::Binding => "binding",
            CallPath::Slot => "slot",
            CallPath::Param => "param",
            CallPath::Field => "field",
            CallPath::Lib => "lib",
            CallPath::CallResult => "call_result",
            CallPath::Defer => "defer",
            CallPath::Spawn => "spawn",
            CallPath::HofMap => "hof_map",
            CallPath::HofSubmit => "hof_submit",
        }
    }
    /// The name an arity error gives the callee on this path.
    fn name(self, written: &str) -> String {
        match self {
            CallPath::Direct => written.to_string(),
            CallPath::Binding | CallPath::Defer | CallPath::Spawn => "h".to_string(),
            CallPath::Param => "k".to_string(),
            CallPath::Field => "f".to_string(),
            CallPath::Lib => "G".to_string(),
            CallPath::Slot | CallPath::CallResult | CallPath::HofMap | CallPath::HofSubmit => {
                "closure".to_string()
            }
        }
    }
}

enum Expect {
    /// Same exit status, stdout and first error line as the direct program.
    Twin(Vec<(String, String)>),
    Rejects(&'static str),
}

struct Cell {
    name: String,
    files: Vec<(String, String)>,
    expect: Expect,
    path_name: String,
    written: String,
}

fn program(c: &Callee, lib: Option<&str>, module_extra: &str, body: &str) -> Vec<(String, String)> {
    let mut main = String::new();
    main.push_str(&c.imports);
    if lib.is_some() {
        main.push_str("import lib\n");
    } else {
        main.push_str(&c.decls);
    }
    main.push_str(module_extra);
    main.push_str("fn main():\n");
    main.push_str(&c.nested);
    for l in body.lines() {
        main.push_str("    ");
        main.push_str(l);
        main.push('\n');
    }
    main.push_str("main()\n");
    let mut files = vec![("main.chz".to_string(), main)];
    if let Some(lib) = lib {
        files.push(("lib.chz".to_string(), lib.to_string()));
    }
    files
}

/// `print(expr)`, or `expr` for a callee that returns nothing.
fn emit(c: &Callee, expr: &str) -> String {
    if c.stmt {
        expr.to_string()
    } else {
        format!("print({expr})")
    }
}

fn cells() -> Vec<Cell> {
    let mut out = Vec::new();
    for c in callees() {
        let direct =
            |args: &str| program(&c, None, "", &emit(&c, &format!("{}({args})", c.callee)));
        for path in PATHS {
            let nested = !c.nested.is_empty();
            if nested && matches!(path, CallPath::Lib | CallPath::CallResult) {
                continue;
            }
            if path == CallPath::Lib && !c.lib_ok {
                continue;
            }
            // `panic` returns an unwritable type (`fn(str) -> ?`): no annotation names it.
            if c.ty.is_empty()
                && matches!(
                    path,
                    CallPath::Param | CallPath::Field | CallPath::CallResult
                )
            {
                continue;
            }
            let hof = matches!(path, CallPath::HofMap | CallPath::HofSubmit);
            let shapes: Vec<(String, String)> = if hof {
                vec![("hof".to_string(), c.hof_arg.clone().unwrap_or_default())]
            } else {
                c.shapes.clone()
            };
            if path == CallPath::HofMap && c.hof_arg.is_none() {
                continue;
            }
            for (shape, args) in shapes {
                let name = format!("{}/{}/{shape}", c.id, path.tag());
                let ty = &c.ty;
                let ret_sig = if c.stmt {
                    String::new()
                } else {
                    format!(" -> {}", c.ret)
                };
                let callee = &c.callee;
                let call_on = |v: &str| emit(&c, &format!("{v}({args})"));
                let (module_extra, body, lib): (String, String, Option<String>) = match path {
                    CallPath::Direct => {
                        (String::new(), emit(&c, &format!("{callee}({args})")), None)
                    }
                    CallPath::Binding => (
                        String::new(),
                        format!("h := {callee}\n{}", call_on("h")),
                        None,
                    ),
                    CallPath::Slot => (
                        String::new(),
                        format!("fs := [{callee}]\n{}", call_on("fs[0]")),
                        None,
                    ),
                    CallPath::Param => {
                        let ret = if c.stmt {
                            String::new()
                        } else {
                            "return ".to_string()
                        };
                        (
                            format!("fn ap(k: {ty}){ret_sig}:\n    {ret}k({args})\n"),
                            if c.stmt {
                                format!("ap({callee})")
                            } else {
                                format!("print(ap({callee}))")
                            },
                            None,
                        )
                    }
                    CallPath::Field => (
                        format!("struct W:\n    f: {ty}\n"),
                        emit(&c, &format!("W({callee}).f({args})")),
                        None,
                    ),
                    CallPath::Lib => (
                        String::new(),
                        emit(&c, &format!("lib.G({args})")),
                        Some(format!("{}{}G := {callee}\n", c.imports, c.decls)),
                    ),
                    CallPath::CallResult => (
                        format!("fn mk() -> {ty}:\n    return {callee}\n"),
                        emit(&c, &format!("mk()({args})")),
                        None,
                    ),
                    CallPath::Defer => (
                        String::new(),
                        format!(
                            "h := {callee}\ndefer {}",
                            if c.stmt {
                                format!("h({args})")
                            } else {
                                format!("print(h({args}))")
                            }
                        ),
                        None,
                    ),
                    CallPath::Spawn => (
                        String::new(),
                        format!(
                            "h := {callee}\nparallel:\n    spawn {}",
                            if c.stmt {
                                format!("h({args})")
                            } else {
                                format!("print(h({args}))")
                            }
                        ),
                        None,
                    ),
                    CallPath::HofMap => (
                        String::new(),
                        format!("print([{args}].map({callee}))"),
                        None,
                    ),
                    CallPath::HofSubmit => (
                        "import Executor from std.concurrency\n".to_string(),
                        format!("ex := Executor(2)\nex.submit({callee})\nex.shutdown()"),
                        None,
                    ),
                };
                let files = program(&c, lib.as_deref(), &module_extra, &body);
                let expect = expectation(&c, path, &shape, &args, &direct);
                out.push(Cell {
                    name,
                    files,
                    expect,
                    path_name: path.name(&c.written),
                    written: c.written.clone(),
                });
            }
        }
    }
    out
}

/// The cell's expected outcome: a rule (E1-E5) or its twin.
fn expectation(
    c: &Callee,
    path: CallPath,
    shape: &str,
    args: &str,
    direct: &dyn Fn(&str) -> Vec<(String, String)>,
) -> Expect {
    if path != CallPath::Direct
        && let Some(frag) = c.no_value
    {
        return Expect::Rejects(frag);
    }
    // E1: a keyword call through a callee that is not a certain binding (TICKET-139).
    let keyword = shape == "named";
    let certain = matches!(
        path,
        CallPath::Direct | CallPath::Binding | CallPath::Defer | CallPath::Spawn
    );
    if keyword && !certain {
        return Expect::Rejects(FN_VALUE_KW);
    }
    // E2: a defaulted call through an annotated fn type, which carries no defaults.
    if shape == "omit"
        && matches!(
            path,
            CallPath::Param | CallPath::Field | CallPath::CallResult
        )
    {
        return Expect::Rejects("argument(s), got 1");
    }
    // E5: the HOF's parameter type decides. `map` takes a plain `fn(T) -> U`, `submit` a `fn() -> T`.
    match path {
        CallPath::HofSubmit => return Expect::Rejects(EXPECTED_FN),
        CallPath::HofMap => {
            let one_param = !c.ty.contains("...") && !c.ty.contains(',');
            return if one_param {
                Expect::Twin(program(
                    c,
                    None,
                    "",
                    &format!("print([{}({args})])", c.callee),
                ))
            } else {
                Expect::Rejects(EXPECTED_FN)
            };
        }
        CallPath::Direct
        | CallPath::Binding
        | CallPath::Slot
        | CallPath::Param
        | CallPath::Field
        | CallPath::Lib
        | CallPath::CallResult
        | CallPath::Defer
        | CallPath::Spawn => {}
    }
    Expect::Twin(direct(args))
}

struct Out {
    ok: bool,
    stdout: String,
    first_err: String,
    stderr: String,
}

fn run_program(root: &Path, idx: usize, files: &[(String, String)]) -> Out {
    let dir: PathBuf = root.join(format!("c{idx}"));
    for (rel, src) in files {
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
    let stderr = String::from_utf8_lossy(&out.stderr);
    Out {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
        first_err: strip_loc(stderr.lines().next().unwrap_or("")),
        stderr: stderr.to_string(),
    }
}

/// An arity error reads one of three ways: `missing required argument 'a'` (a call plan miss, no
/// callee name), `'k' expects 2 argument(s), got 0` and `'k' expects 1–2 argument(s), got 0` (an
/// annotated fn type carries no defaults, E2). Every form is one arity rejection; the callee name
/// is checked wherever the message carries one. Any other line compares as written.
fn canon(line: &str, path_name: &str) -> String {
    if line.contains(" expects ") && line.contains(" argument(s), got ") {
        let named = format!("'{path_name}' expects ");
        if !line.contains(&named) {
            return format!("ARITY with the wrong callee name (want {path_name:?}): {line}");
        }
        return "ARITY".to_string();
    }
    if line.contains("missing required argument") {
        return "ARITY".to_string();
    }
    line.to_string()
}

/// Drop a `(file:line:col)` location from an error line.
fn strip_loc(line: &str) -> String {
    let mut s = line.to_string();
    while let Some(open) = s.find(" (") {
        let Some(close) = s[open..].find(')') else {
            break;
        };
        let inner = &s[open + 2..open + close];
        let tail: Vec<&str> = inner.rsplitn(3, ':').collect();
        if tail.len() == 3
            && tail[0].chars().all(|c| c.is_ascii_digit())
            && tail[1].chars().all(|c| c.is_ascii_digit())
        {
            s.replace_range(open..open + close + 1, "");
        } else {
            break;
        }
    }
    s
}

fn run_all(root: &Path, base: usize, programs: &[&Vec<(String, String)>]) -> Vec<Out> {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Out)>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= programs.len() {
                        break;
                    }
                    let o = run_program(root, base + i, programs[i]);
                    results.lock().unwrap().push((i, o));
                }
            });
        }
    });
    let mut r = results.into_inner().unwrap();
    r.sort_by_key(|(i, _)| *i);
    r.into_iter().map(|(_, o)| o).collect()
}

fn key(files: &[(String, String)]) -> String {
    files
        .iter()
        .map(|(n, s)| format!("{n}\n{s}"))
        .collect::<Vec<_>>()
        .join("\n--\n")
}

fn grid_fails(root: &Path, cells: &[Cell]) -> Vec<(String, String)> {
    let mut twin_ix: HashMap<String, usize> = HashMap::new();
    let mut twins: Vec<&Vec<(String, String)>> = Vec::new();
    for c in cells {
        if let Expect::Twin(f) = &c.expect {
            twin_ix.entry(key(f)).or_insert_with(|| {
                twins.push(f);
                twins.len() - 1
            });
        }
    }
    let twin_out = run_all(root, 0, &twins);
    let programs: Vec<&Vec<(String, String)>> = cells.iter().map(|c| &c.files).collect();
    let outs = run_all(root, twins.len(), &programs);
    let mut fails = Vec::new();
    for (c, got) in cells.iter().zip(&outs) {
        let verdict = match &c.expect {
            Expect::Rejects(frag) => {
                if !got.ok && got.stderr.contains(frag) {
                    Ok(())
                } else {
                    Err(format!(
                        "want rejects {frag:?}; got ok={} stdout {:?} err {:?}",
                        got.ok, got.stdout, got.first_err
                    ))
                }
            }
            Expect::Twin(f) => {
                let t = &twin_out[twin_ix[&key(f)]];
                let want_err = t
                    .first_err
                    .replace(&format!("'{}'", c.written), &format!("'{}'", c.path_name));
                let (want_err, got_err) = (
                    canon(&want_err, &c.path_name),
                    canon(&got.first_err, &c.path_name),
                );
                if t.ok == got.ok && t.stdout == got.stdout && want_err == got_err {
                    Ok(())
                } else {
                    Err(format!(
                        "twin ok={} stdout {:?} err {:?}; got ok={} stdout {:?} err {:?}",
                        t.ok, t.stdout, want_err, got.ok, got.stdout, got.first_err
                    ))
                }
            }
        };
        if let Err(e) = verdict {
            fails.push((c.name.clone(), e));
        }
    }
    fails
}

#[test]
fn builtin_input_covers_exactly_the_first_class_builtins() {
    for b in Builtin::ALL {
        assert_eq!(builtin_input(b).is_some(), b.first_class(), "{b:?}");
        assert_eq!(!builtin_name(b).is_empty(), b.first_class(), "{b:?}");
    }
}

#[test]
fn fn_value_call_grid() {
    let root = std::env::temp_dir().join(format!("chezzi-fn-value-grid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let cells = cells();
    let fails = grid_fails(&root, &cells);
    let _ = std::fs::remove_dir_all(&root);
    let mut bad = Vec::new();
    for (name, why) in &fails {
        if !FN_VALUE_PINNED_RED.contains(&name.as_str()) {
            bad.push(format!("{name}: {why}"));
        }
    }
    for p in FN_VALUE_PINNED_RED {
        if !fails.iter().any(|(n, _)| n == p) {
            bad.push(format!("{p}: green; remove it from FN_VALUE_PINNED_RED"));
        }
    }
    if std::env::var("GRID_DUMP").is_ok() {
        for (n, w) in &fails {
            eprintln!("RED {n} :: {w}");
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} fn-value cells failed:\n{}",
        bad.len(),
        cells.len(),
        bad.join("\n")
    );
}

/// The family of a red cell, from the measurements on the unfixed binary. A cell name is
/// `row/pset/path/shape`.
fn family(tag: &str) -> Option<&'static str> {
    let parts: Vec<&str> = tag.split('/').collect();
    let [row, pset, path, shape] = parts[..] else {
        return None;
    };
    // F1: `print` as a value loses its variadic shape.
    if row == "builtin_print" && path != "hof_map" {
        return Some("F1");
    }
    // F5: `map` over `print`.
    if row == "builtin_print" {
        return Some("F5");
    }
    // F3: a C variadic extern value keeps no varargs.
    if row == "extern_snprintf" {
        return Some("F3");
    }
    // F4: a keyword call through a builtin or native value.
    if shape == "named"
        && matches!(
            row,
            "builtin_ord" | "builtin_chr" | "builtin_panic" | "native_sqrt"
        )
    {
        return Some("F4");
    }
    // F2: a variadic value packs only through a certain binding.
    if pset == "var" {
        return Some("F2");
    }
    None
}

#[test]
fn fn_value_pinned_red_belongs_to_the_families() {
    let pinned: std::collections::BTreeSet<&str> = FN_VALUE_PINNED_RED.iter().copied().collect();
    for n in &pinned {
        assert!(family(n).is_some(), "pinned cell {n} belongs to no family");
    }
    for f in ["F1", "F2", "F3", "F4", "F5"] {
        assert!(
            pinned.iter().any(|n| family(n) == Some(f)),
            "family {f} names no pinned cell"
        );
    }
}
