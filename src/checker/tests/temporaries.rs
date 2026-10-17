//! Checker tests: TICKET-254 fresh temporaries, range subscripts, native container results, slice
//! hooks, recv kinds.
//! Split out of `src/checker/tests.rs`. `super::*` is the parent module.

use super::*;

// ===== TICKET-254: one answer for a mutating call on a fresh temporary, in every context =====

/// `(errors, warnings)` of a program, each as `message` plus `help`, through `build_graph` so that
/// `import std.concurrency` and a two-file program resolve. When `T254_DUMP` names a directory, the
/// program is also written there as `<name>.chz` (the run-time column of the grid is measured on the
/// built binary, not here).
fn t254_diags(name: &str, files: &[(&str, String)], main: &str) -> (Vec<String>, Vec<String>) {
    let t = TmpDir::new();
    for (file, body) in files {
        t.write(file, body);
    }
    let entry = t.write("main.chz", main);
    if let Ok(dir) = std::env::var("T254_DUMP") {
        std::fs::write(format!("{dir}/{name}.chz"), main).expect("dump");
        for (file, body) in files {
            std::fs::write(format!("{dir}/{file}"), body).expect("dump");
        }
    }
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let (res, warns) = check_graph_diags(&graph, None);
    let text = |d: &CheckError| format!("{} {}", d.message, d.help.clone().unwrap_or_default());
    (
        res.err().unwrap_or_default().iter().map(text).collect(),
        warns.iter().map(text).collect(),
    )
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum T254Ctx {
    Top,
    Fn,
    SpawnedFn,
    SpawnBlock,
    ExecutorJob,
    BoxClosure,
    ImportedFn,
}

impl T254Ctx {
    const ALL: [T254Ctx; 7] = [
        T254Ctx::Top,
        T254Ctx::Fn,
        T254Ctx::SpawnedFn,
        T254Ctx::SpawnBlock,
        T254Ctx::ExecutorJob,
        T254Ctx::BoxClosure,
        T254Ctx::ImportedFn,
    ];
}

struct T254Shape {
    name: &'static str,
    /// Declarations placed before the program.
    decls: &'static str,
    params: &'static str,
    args: &'static str,
    root: &'static str,
    /// The expression the program prints at the end: the root after the write.
    show: &'static str,
    rows: &'static [T254Row],
}

/// What a cell must show. The three columns are the warning, the check error and the root printed
/// after the write.
#[derive(Clone, Copy)]
enum T254Expect {
    /// The receiver is a fresh temporary: the warning (its text for a call, its text for a store),
    /// no error, the root unchanged.
    Warns(&'static str, &'static str),
    /// A fresh temporary the warning declines (a ceiling): no warning, no error, the root unchanged.
    Silent,
    /// A write that reaches the root, as a D4 control: no warning, the error of `t254_control_errors`.
    /// In a context that checks clean, the run follows `t254_control_faults`.
    Control,
    /// A write the checker cannot see reaches the root (a user `slice` hook that returns its own
    /// field): no warning, no error, and the run follows `t254_control_faults`: the D4 fault, never
    /// a silent lost write.
    Declined,
}

const T254_COPY_TEXT: T254Expect =
    T254Expect::Warns("changes a temporary copy", "changes a temporary copy");

/// `kind`: the native method table the receiver belongs to (`""` for none). Every method of it with
/// `crate::native::is_mutating_native_kind` true is a write of the row ([`t254_native_calls`]).
/// `writes`: the other statements (stores); `@` is the receiver, `$` the element value.
struct T254Row {
    name: &'static str,
    recv: &'static str,
    a: &'static str,
    expect: T254Expect,
    kind: &'static str,
    writes: &'static [&'static str],
}

const fn t254_row(
    name: &'static str,
    recv: &'static str,
    a: &'static str,
    expect: T254Expect,
    kind: &'static str,
    writes: &'static [&'static str],
) -> T254Row {
    T254Row {
        name,
        recv,
        a,
        expect,
        kind,
        writes,
    }
}

/// One call for each mutating native method of the four mutable containers. A mutating method of
/// the table with no entry here fails `t254_native_calls`.
const T254_CALLS: &[(&str, &str, &str)] = &[
    ("List", "push", "push($)"),
    ("List", "pop", "pop()"),
    ("List", "reverse", "reverse()"),
    ("List", "sort", "sort()"),
    ("List", "sort_by", "sort_by(fn(a, b): 0)"),
    ("List", "sort_by_key", "sort_by_key(fn(a): 0)"),
    ("List", "extend", "extend([$])"),
    ("List", "insert", "insert(0, $)"),
    ("List", "remove_at", "remove_at(0)"),
    ("Map", "remove", "remove(\"a\")"),
    ("Map", "update", "update({\"z\": $})"),
    ("Set", "add", "add($)"),
    ("Set", "remove", "remove($)"),
    ("bytearray", "push", "push($)"),
    ("bytearray", "pop", "pop()"),
    ("bytearray", "extend", "extend(b\"c\")"),
    ("bytearray", "reverse", "reverse()"),
];

/// The call templates of every method of `kind` that `is_mutating_native_kind` flags, read from the
/// prelude method table, so a new mutating method is a cell the day it lands.
fn t254_native_calls(kind: &str) -> Vec<String> {
    if kind.is_empty() {
        return Vec::new();
    }
    let c = prelude_container_checker();
    let mut methods: Vec<String> = c
        .structs
        .get(kind)
        .unwrap_or_else(|| panic!("seeded {kind} struct"))
        .methods
        .keys()
        .cloned()
        .collect();
    if kind == "bytearray" {
        methods.push("extend".to_string());
    }
    methods.sort();
    methods.dedup();
    methods
        .iter()
        .filter(|m| crate::native::is_mutating_native_kind(kind, m))
        .map(|m| {
            let (_, _, call) = T254_CALLS
                .iter()
                .find(|(k, name, _)| *k == kind && name == m)
                .unwrap_or_else(|| {
                    panic!("{kind}.{m} is a mutating method with no T254_CALLS row")
                });
            (*call).to_string()
        })
        .collect()
}

const T254_SHAPES: &[T254Shape] = &[
    T254Shape {
        name: "list",
        decls: "",
        params: "g: List[List[int]], r: Range",
        args: "g, r",
        root: "g := [[1], [2], [3]]\nr := 0..2\n",
        show: "g",
        rows: &[
            t254_row(
                "slice_lit",
                "g[0..2]",
                "[9]",
                T254_COPY_TEXT,
                "List",
                &["@[0] = $"],
            ),
            t254_row(
                "slice_var",
                "g[r]",
                "[9]",
                T254_COPY_TEXT,
                "List",
                &["@[0] = $"],
            ),
            t254_row(
                "slice_step",
                "g[Range(0, 3, 2)]",
                "[9]",
                T254_COPY_TEXT,
                "List",
                &["@[0] = $"],
            ),
            t254_row(
                "reversed",
                "g.reversed()",
                "[9]",
                T254_COPY_TEXT,
                "List",
                &["@[0] = $"],
            ),
            t254_row(
                "copy",
                "g.copy()",
                "[9]",
                T254_COPY_TEXT,
                "List",
                &["@[0] = $"],
            ),
            t254_row(
                "operator",
                "(g + [[4]])",
                "[9]",
                T254_COPY_TEXT,
                "List",
                &["@[0] = $"],
            ),
            t254_row(
                "comprehension",
                "[x for x in g]",
                "[9]",
                T254_COPY_TEXT,
                "List",
                &["@[0] = $"],
            ),
            t254_row(
                "literal",
                "[[1]]",
                "[9]",
                T254_COPY_TEXT,
                "List",
                &["@[0] = $"],
            ),
            // A builtin constructor call is not in `fresh_shape`: the warning declines (ceiling).
            t254_row(
                "ctor",
                "List([5])",
                "9",
                T254Expect::Silent,
                "List",
                &["@[0] = $"],
            ),
            t254_row(
                "index",
                "g[0]",
                "9",
                T254Expect::Control,
                "List",
                &["@[0] = $"],
            ),
            t254_row(
                "name",
                "g",
                "[9]",
                T254Expect::Control,
                "List",
                &["@[0] = $"],
            ),
        ],
    },
    T254Shape {
        name: "map",
        decls: "",
        params: "g: Map[str, int]",
        args: "g",
        root: "g := {\"a\": 1}\n",
        show: "g",
        rows: &[
            t254_row(
                "copy",
                "g.copy()",
                "2",
                T254_COPY_TEXT,
                "Map",
                &["@[\"z\"] = $"],
            ),
            t254_row(
                "literal",
                "({\"a\": 1})",
                "2",
                T254_COPY_TEXT,
                "Map",
                &["@[\"z\"] = $"],
            ),
            t254_row(
                "name",
                "g",
                "2",
                T254Expect::Control,
                "Map",
                &["@[\"z\"] = $"],
            ),
        ],
    },
    T254Shape {
        name: "set",
        decls: "",
        params: "g: Set[int]",
        args: "g",
        root: "g := {1, 2}\n",
        show: "g",
        rows: &[
            t254_row("copy", "g.copy()", "1", T254_COPY_TEXT, "Set", &[]),
            t254_row("name", "g", "1", T254Expect::Control, "Set", &[]),
        ],
    },
    T254Shape {
        name: "bytearray",
        decls: "",
        params: "g: bytearray",
        args: "g",
        root: "g := bytearray()\ng.push(97)\ng.push(98)\n",
        show: "g",
        rows: &[
            t254_row(
                "slice_lit",
                "g[0..1]",
                "1",
                T254_COPY_TEXT,
                "bytearray",
                &["@[0] = $"],
            ),
            t254_row(
                "copy",
                "g.copy()",
                "1",
                T254_COPY_TEXT,
                "bytearray",
                &["@[0] = $"],
            ),
            t254_row(
                "name",
                "g",
                "1",
                T254Expect::Control,
                "bytearray",
                &["@[0] = $"],
            ),
        ],
    },
    T254Shape {
        name: "struct",
        decls: "struct P:\n    x: int\n    items: List[int]\n",
        params: "g: P",
        args: "g",
        root: "g := P(1, [1])\n",
        show: "g",
        rows: &[
            t254_row(
                "ctor_store",
                "P(2, [2])",
                "5",
                T254_COPY_TEXT,
                "",
                &["@.x = $"],
            ),
            t254_row(
                "name_store",
                "g",
                "5",
                T254Expect::Control,
                "",
                &["@.x = $"],
            ),
            t254_row(
                "field_push",
                "g.items",
                "3",
                T254Expect::Control,
                "List",
                &[],
            ),
        ],
    },
    // A user `set_index` hook runs user code on a temporary: the warning declines.
    T254Shape {
        name: "set_index_hook",
        decls: "struct Log:\n    n: int\n    fn index(self, k: int) -> int:\n        return self.n\n    fn set_index(self, k: int, v: int):\n        print(\"set {k}={v}\")\n",
        params: "g: Log",
        args: "g",
        root: "g := Log(0)\n",
        show: "g.n",
        rows: &[
            t254_row(
                "ctor_store",
                "Log(0)",
                "5",
                T254Expect::Silent,
                "",
                &["@[0] = $"],
            ),
            t254_row(
                "name_store",
                "g",
                "5",
                T254Expect::Silent,
                "",
                &["@[0] = $"],
            ),
        ],
    },
    // A user `slice` hook may return a fresh list or its own field; the warning declines either way.
    T254Shape {
        name: "slice_hook_fresh",
        decls: "struct H:\n    data: List[int]\n    fn slice(self, r: Range) -> List[int]:\n        return [7, 8]\n",
        params: "g: H",
        args: "g",
        root: "g := H([1, 2, 3])\n",
        show: "g.data",
        rows: &[t254_row(
            "hook_fresh",
            "g[0..2]",
            "9",
            T254Expect::Silent,
            "List",
            &["@[0] = $"],
        )],
    },
    T254Shape {
        name: "slice_hook_alias",
        decls: "struct A:\n    data: List[int]\n    fn slice(self, r: Range) -> List[int]:\n        return self.data\n",
        params: "g: A",
        args: "g",
        root: "g := A([1, 2, 3])\n",
        show: "g.data",
        rows: &[t254_row(
            "hook_alias",
            "g[0..2]",
            "9",
            T254Expect::Declined,
            "List",
            &["@[0] = $"],
        )],
    },
    // A `Shared` read is a deep copy (TICKET-089): the box rows are rows of the same rule.
    T254Shape {
        name: "box",
        decls: "import std.concurrency\n",
        params: "g: Shared[List[int]]",
        args: "g",
        root: "import Shared from std.concurrency\ng := Shared([1, 2])\n",
        show: "g.get()",
        rows: &[t254_row(
            "box_read",
            "g.get()",
            "9",
            T254Expect::Warns(
                "mutates the copy returned by",
                "this write lands on the copy returned by",
            ),
            "List",
            &["@[0] = $"],
        )],
    },
];

/// The cells with no legal program: a struct declared in the entry file is not visible to `lib.chz`,
/// and the box shape imports `std.concurrency` itself, as the `Executor` and box-closure programs do.
fn t254_applies(shape: &T254Shape, ctx: T254Ctx) -> bool {
    match ctx {
        T254Ctx::ImportedFn => shape.decls.is_empty(),
        T254Ctx::ExecutorJob | T254Ctx::BoxClosure => !shape.decls.starts_with("import"),
        T254Ctx::Top | T254Ctx::Fn | T254Ctx::SpawnedFn | T254Ctx::SpawnBlock => true,
    }
}

fn t254_program(
    shape: &T254Shape,
    ctx: T254Ctx,
    stmt: &str,
) -> (Vec<(&'static str, String)>, String) {
    let (decls, params, args, root) = (shape.decls, shape.params, shape.args, shape.root);
    let show = shape.show;
    let work = format!("fn work({params}):\n    {stmt}\n");
    let mut files = Vec::new();
    let main = match ctx {
        T254Ctx::Top => format!("{decls}{root}{stmt}\nprint({show})\n"),
        T254Ctx::Fn => format!("{decls}{work}{root}work({args})\nprint({show})\n"),
        T254Ctx::SpawnedFn => {
            format!("{decls}{work}{root}parallel:\n    spawn work({args})\nprint({show})\n")
        }
        T254Ctx::SpawnBlock => {
            format!("{decls}{root}parallel:\n    spawn:\n        {stmt}\nprint({show})\n")
        }
        T254Ctx::ExecutorJob => format!(
            "import Executor from std.concurrency\n{decls}{work}{root}ex := Executor()\nex.submit(fn(): work({args}))\nex.shutdown()\nprint({show})\n"
        ),
        T254Ctx::BoxClosure => format!(
            "import Shared from std.concurrency\n{decls}{work}fn wrap({params}, x: int) -> int:\n    work({args})\n    return x\n{root}s := Shared(0)\ns.update(fn(x: int) -> int: wrap({args}, x))\nprint({show})\n"
        ),
        T254Ctx::ImportedFn => {
            files.push(("lib.chz", work));
            format!("import lib\n{root}parallel:\n    spawn lib.work({args})\nprint({show})\n")
        }
    };
    (files, main)
}

/// Does a D4 error ("this task's copy") belong to a plain-name or plain-index write in `ctx`?
/// Measured on the base binary with `T254_DUMP`: a spawned fn, a `spawn:` block and a fn of another
/// module are errors; a plain call, the top level, a box closure and an `Executor` job are not.
fn t254_control_errors(ctx: T254Ctx) -> bool {
    match ctx {
        T254Ctx::SpawnedFn | T254Ctx::SpawnBlock | T254Ctx::ImportedFn => true,
        T254Ctx::Top | T254Ctx::Fn | T254Ctx::ExecutorJob | T254Ctx::BoxClosure => false,
    }
}

/// A write that reaches the root, run in `ctx` where the program checks clean: the top level and a
/// plain call write the root; a task, an `Executor` job and a box closure run on a copy, and the run
/// ends with the D4 fault `this value is this task's copy`. Measured on the base binary: the same
/// table.
fn t254_control_faults(ctx: T254Ctx) -> bool {
    match ctx {
        T254Ctx::Top | T254Ctx::Fn => false,
        T254Ctx::SpawnedFn
        | T254Ctx::SpawnBlock
        | T254Ctx::ExecutorJob
        | T254Ctx::BoxClosure
        | T254Ctx::ImportedFn => true,
    }
}

/// The last line a program printed, or the fault it ended with. The program runs through
/// `run_file`, so the checker's hook and slice tables reach the compiler (`run_capture` has none).
fn t254_last_line(src: &str) -> String {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (out, _err, result, _code) = crate::compiler::run_for_tests::run_file(&entry);
    match result {
        Ok(()) => out.lines().last().unwrap_or_default().to_string(),
        Err(e) => format!("fault: {e}"),
    }
}

fn t254_any_temporary_warning(warns: &[String]) -> bool {
    warns.iter().any(|w| {
        w.contains("temporary copy")
            || w.contains("mutates the copy returned by")
            || w.contains("lands on the copy returned by")
    })
}

#[test]
fn fresh_temporary_grid() {
    let mut bad = Vec::new();
    for shape in T254_SHAPES {
        let baseline = t254_last_line(&format!(
            "{}{}print({})\n",
            shape.decls, shape.root, shape.show
        ));
        for row in shape.rows {
            let mut writes: Vec<(String, bool)> = t254_native_calls(row.kind)
                .into_iter()
                .map(|call| (format!("@.{call}"), true))
                .collect();
            writes.extend(row.writes.iter().map(|w| ((*w).to_string(), false)));
            for ctx in T254Ctx::ALL {
                if !t254_applies(shape, ctx) {
                    continue;
                }
                for (wi, (write, is_call)) in writes.iter().enumerate() {
                    let stmt = write.replace('@', row.recv).replace('$', row.a);
                    let cell = format!("{}_{}_{:?}_w{wi}", shape.name, row.name, ctx);
                    let (files, main) = t254_program(shape, ctx, &stmt);
                    let (errs, warns) = t254_diags(&cell, &files, &main);
                    let errored = errs.iter().any(|e| e.contains("this task's copy"));
                    let (want_warn, want_err) = match row.expect {
                        T254Expect::Warns(call, store) => {
                            let text = if *is_call { call } else { store };
                            (Some(text), false)
                        }
                        T254Expect::Silent | T254Expect::Declined => (None, false),
                        T254Expect::Control => (None, t254_control_errors(ctx)),
                    };
                    let warned = match want_warn {
                        Some(text) => warns.iter().any(|w| w.contains(text)),
                        None => t254_any_temporary_warning(&warns),
                    };
                    let stray = errs.len() != usize::from(want_err);
                    // The run-time column: a fresh or declined write leaves the root as it was.
                    let ran = (errs.is_empty() && ctx != T254Ctx::ImportedFn)
                        .then(|| t254_last_line(&main));
                    let run_bad = match (&ran, row.expect) {
                        (Some(got), T254Expect::Warns(..) | T254Expect::Silent) => *got != baseline,
                        (Some(got), T254Expect::Control | T254Expect::Declined) => {
                            let d4 = got.starts_with("fault:") && got.contains("this task's copy");
                            let other_fault = got.starts_with("fault:") && !d4;
                            d4 != t254_control_faults(ctx) || other_fault
                        }
                        (None, _) => false,
                    };
                    if warned != want_warn.is_some() || errored != want_err || stray || run_bad {
                        bad.push(format!(
                            "{cell}: stmt `{stmt}` warned={warned} (want {want_warn:?}) errored={errored} (want {want_err}) errs={errs:?} ran={ran:?} baseline={baseline:?}"
                        ));
                    }
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} red cells:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

#[derive(Clone, Copy, Debug)]
enum T254Owner {
    Struct,
    Enum,
    Generic,
    /// `G[K]` whose `index` takes its own parameter, used as `G[Range]`.
    Keyed,
}

#[derive(Clone, Copy, Debug)]
enum T254Declares {
    SliceOnly,
    IndexRangeOnly,
    Both,
    IndexIntAndSlice,
}

#[derive(Clone, Copy, Debug)]
enum T254Use {
    Literal,
    Variable,
    SliceBound,
    IndexBound,
}

#[test]
fn range_subscript_ambiguity_grid() {
    let owners = [
        T254Owner::Struct,
        T254Owner::Enum,
        T254Owner::Generic,
        T254Owner::Keyed,
    ];
    let declares = [
        T254Declares::SliceOnly,
        T254Declares::IndexRangeOnly,
        T254Declares::Both,
        T254Declares::IndexIntAndSlice,
    ];
    let uses = [
        T254Use::Literal,
        T254Use::Variable,
        T254Use::SliceBound,
        T254Use::IndexBound,
    ];
    let both_text = "a type takes a range as a slice or as a key, not both";
    let mut bad = Vec::new();
    for owner in owners {
        for decl in declares {
            for usage in uses {
                let (head, elem, key_int, make) = match owner {
                    T254Owner::Struct => (
                        "struct S:\n    data: List[int]\n",
                        "int",
                        "int",
                        "x := S([1, 2, 3])\n",
                    ),
                    T254Owner::Enum => (
                        "enum S:\n    A(List[int])\n    B\n",
                        "int",
                        "int",
                        "x := S.A([1, 2, 3])\n",
                    ),
                    T254Owner::Generic => (
                        "struct S[T]:\n    data: List[T]\n",
                        "T",
                        "int",
                        "x := S([1, 2, 3])\n",
                    ),
                    T254Owner::Keyed => (
                        "struct S[K]:\n    data: List[int]\n",
                        "int",
                        "K",
                        "x: S[Range] = S([1, 2, 3])\n",
                    ),
                };
                let make = if matches!(
                    (owner, decl),
                    (T254Owner::Keyed, T254Declares::IndexIntAndSlice)
                ) {
                    "x: S[int] = S([1, 2, 3])\n"
                } else {
                    make
                };
                // An enum has no field: its bodies build the result.
                let (slice_body, index_body) = match owner {
                    T254Owner::Enum => ("[1]", "1"),
                    T254Owner::Struct | T254Owner::Generic | T254Owner::Keyed => {
                        ("self.data[r]", "self.data[0]")
                    }
                };
                let slice = format!(
                    "    fn slice(self, r: Range) -> List[{elem}]:\n        return {slice_body}\n"
                );
                let index_range = format!(
                    "    fn index(self, k: {}) -> {elem}:\n        return {index_body}\n",
                    if matches!(owner, T254Owner::Keyed) {
                        "K"
                    } else {
                        "Range"
                    }
                );
                let index_int = format!(
                    "    fn index(self, k: {key_int}) -> {elem}:\n        return {index_body}\n"
                );
                let methods = match decl {
                    T254Declares::SliceOnly => slice,
                    T254Declares::IndexRangeOnly => index_range,
                    T254Declares::Both => format!("{slice}{index_range}"),
                    T254Declares::IndexIntAndSlice => format!("{slice}{index_int}"),
                };
                let tail = match usage {
                    T254Use::Literal => "print(x[1..3])\n".to_string(),
                    T254Use::Variable => "r := 1..3\nprint(x[r])\n".to_string(),
                    T254Use::SliceBound => {
                        "import Slice from std.ops\nfn f[C: Slice[R], R](c: C) -> R:\n    return c[1..2]\nprint(f(x))\n"
                            .to_string()
                    }
                    T254Use::IndexBound => {
                        "import Index from std.ops\nfn f[C: Index[Range, V], V](c: C) -> V:\n    return c[1..2]\nprint(f(x))\n"
                            .to_string()
                    }
                };
                let src = format!("{head}{methods}{make}{tail}");
                let errs = check_src(&src);
                let both_err = errs.iter().any(|e| e.message.contains(both_text));
                let any_err = !errs.is_empty();
                // The keyed owner declares `index(k: K)`: legal on its own, an error where `K` is a Range.
                let want_both = match (owner, decl, usage) {
                    (
                        T254Owner::Keyed,
                        T254Declares::Both,
                        T254Use::Literal | T254Use::Variable,
                    ) => true,
                    (
                        T254Owner::Keyed,
                        T254Declares::Both,
                        T254Use::SliceBound | T254Use::IndexBound,
                    ) => false,
                    (_, T254Declares::Both, _) => true,
                    (
                        _,
                        T254Declares::SliceOnly
                        | T254Declares::IndexRangeOnly
                        | T254Declares::IndexIntAndSlice,
                        _,
                    ) => false,
                };
                // Without `Both`, only a bound the type does not satisfy is an error.
                let want_other_err = match (decl, usage) {
                    (T254Declares::Both, _) => false,
                    (
                        T254Declares::SliceOnly | T254Declares::IndexIntAndSlice,
                        T254Use::IndexBound,
                    ) => true,
                    (T254Declares::IndexRangeOnly, T254Use::SliceBound) => true,
                    (T254Declares::SliceOnly | T254Declares::IndexIntAndSlice, _) => false,
                    (T254Declares::IndexRangeOnly, _) => false,
                };
                if both_err != want_both || (!want_both && any_err != want_other_err) {
                    bad.push(format!("{owner:?} {decl:?} {usage:?}: both_err={both_err} (want {want_both}) any_err={any_err} (want other {want_other_err}) errs={errs:?}\n{src}"));
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} red cells:\n{}",
        bad.len(),
        bad.join("\n")
    );
}

/// TICKET-254 — the `fresh_shape` row for a native method that returns a container reads the DECLARED
/// return type. That is sound only while no such method returns its receiver or a view of it
/// (DEC-190 item 6). Every native method of `List`/`Map`/`Set`/`bytearray` whose declared return type
/// is one of those four runs here: the program mutates the result and asserts the receiver printed
/// the same. A new method with such a return type and no row fails the completeness check first.
#[test]
fn native_container_results_are_new_objects() {
    // (kind, receiver setup, call, a mutation of the result `r` that changes it whenever `r` is
    // the receiver or shares its storage)
    let rows: &[(&str, &str, &str, &str)] = &[
        ("List", "x := [3, 1, 1, 2]", "reversed()", "r.pop()"),
        ("List", "x := [3, 1, 1, 2]", "concat([4])", "r.pop()"),
        ("List", "x := [3, 1, 1, 2]", "map(fn(a): a + 1)", "r.pop()"),
        (
            "List",
            "x := [3, 1, 1, 2]",
            "filter(fn(a): a > 0)",
            "r.pop()",
        ),
        ("List", "x := [3, 1, 1, 2]", "unique()", "r.pop()"),
        ("List", "x := [3, 1, 1, 2]", "dedup()", "r.pop()"),
        ("List", "x := [3, 1, 1, 2]", "chunk(2)", "r.pop()"),
        ("List", "x := [3, 1, 1, 2]", "windows(2)", "r.pop()"),
        (
            "List",
            "x := [3, 1, 1, 2]",
            "take_while(fn(a): a > 0)",
            "r.pop()",
        ),
        (
            "List",
            "x := [3, 1, 1, 2]",
            "drop_while(fn(a): a > 5)",
            "r.pop()",
        ),
        ("List", "x := [3, 1, 1, 2]", "copy()", "r.pop()"),
        ("Map", "x := {\"a\": 1, \"b\": 2}", "keys()", "r.pop()"),
        ("Map", "x := {\"a\": 1, \"b\": 2}", "values()", "r.pop()"),
        ("Map", "x := {\"a\": 1, \"b\": 2}", "items()", "r.pop()"),
        (
            "Map",
            "x := {\"a\": 1, \"b\": 2}",
            "merge({\"c\": 3})",
            "r.remove(\"a\")",
        ),
        (
            "Map",
            "x := {\"a\": 1, \"b\": 2}",
            "copy()",
            "r.remove(\"a\")",
        ),
        ("Set", "x := {1, 2}", "union({9})", "r.remove(1)"),
        ("Set", "x := {1, 2}", "intersection({1, 2})", "r.remove(1)"),
        ("Set", "x := {1, 2}", "difference({9})", "r.remove(1)"),
        ("Set", "x := {1, 2}", "copy()", "r.remove(1)"),
        (
            "bytearray",
            "x := bytearray()\nx.push(97)\nx.push(98)",
            "copy()",
            "r.pop()",
        ),
        (
            "bytearray",
            "x := bytearray()\nx.push(97)\nx.push(98)",
            "reversed()",
            "r.pop()",
        ),
        // A `str` receiver is immutable; its container results are new lists all the same.
        ("str", "x := \"a,b\"", "split(\",\")", "r.pop()"),
        ("str", "x := \"a,b\"", "chars()", "r.pop()"),
        ("str", "x := \"a\\nb\"", "split_lines()", "r.pop()"),
    ];
    let c = prelude_container_checker();
    let mut bad: Vec<String> = Vec::new();
    for kind in ["List", "Map", "Set", "bytearray", "str", "bytes"] {
        let info = c.structs.get(kind).expect("seeded container struct");
        for (name, sig) in &info.methods {
            let container = fn_writes::native_container(&sig.ret);
            let sampled = rows
                .iter()
                .any(|(k, _, call, _)| *k == kind && call.split('(').next() == Some(name.as_str()));
            if container && !sampled {
                bad.push(format!("{kind}.{name}: returns a container, has no row"));
            }
            if !container && sampled {
                bad.push(format!("{kind}.{name}: has a row, returns no container"));
            }
        }
    }
    for (kind, setup, call, mutate) in rows {
        let src = format!(
            "{setup}\nbefore := \"{{x}}\"\nr := x.{call}\n{mutate}\nprint(\"{{x}}\" == before)\n"
        );
        match crate::compiler::run_for_tests::run_capture(&src) {
            Ok(out) if out.trim() == "true" => {}
            Ok(out) => bad.push(format!("{kind}.{call}: the receiver changed: {out:?}")),
            Err(e) => bad.push(format!("{kind}.{call}: {}", e.message)),
        }
    }
    assert!(bad.is_empty(), "native container results: {bad:#?}");
}

/// TICKET-254 — a write whose LAST place link is a builtin slice changes a new container, so it is no
/// write to the root; a write that goes on through an element of the slice, or through a `Map` key
/// that is a `Range`, still reaches the root and stays a D4 error in a spawned fn.
#[test]
fn a_slice_is_no_place_but_an_element_of_it_and_a_range_key_are() {
    let spawn = |params: &str, root: &str, stmt: &str| {
        format!("fn work({params}):\n    {stmt}\n{root}parallel:\n    spawn work(g)\nprint(g)\n")
    };
    let list = ("g: List[List[int]]", "g := [[1], [2], [3]]\n");
    let map = ("g: Map[Range, List[int]]", "g := {0..3: [1]}\n");
    // (params and root, statement, is it a D4 error?)
    let cells = [
        (list, "g[0..2].push([9])", false),
        (list, "g[0..2][0] = [9]", false),
        (list, "g[0..2][0].push(9)", true),
        (list, "g[0].push(9)", true),
        (list, "g.push([9])", true),
        (map, "g[0..3].push(9)", true),
    ];
    for ((params, root), stmt, errors) in cells {
        let (errs, _) = t254_diags("slice_place", &[], &spawn(params, root, stmt));
        let got = errs.iter().any(|e| e.contains("this task's copy"));
        assert_eq!(got, errors, "`{stmt}`: errors={errs:?}");
    }
}

/// TICKET-254 ceiling, measured: a user `slice` hook may return its own field (`st.chz` prints
/// `[1, 2, 3, 9]` for the alias), so a mutating call on its result is not a proven temporary write.
/// The warning stays silent for a hook that returns a fresh list as well: it is not a builtin slice.
#[test]
fn a_user_slice_hook_result_is_no_fresh_temporary() {
    for body in ["return self.xs", "return [1, 2]"] {
        let src = format!(
            "struct S:\n    xs: List[int]\n    fn slice(self, r: Range) -> List[int]:\n        {body}\ns := S([1, 2, 3])\ns[0..2].push(9)\nprint(s.xs)\n"
        );
        no_warn(&src);
    }
}

/// TICKET-254 — the freshness of a subscript travels with the write summary, so a spawned callee in
/// ANOTHER module (a fn, or a method of a struct that module declares) is judged by the module that
/// typed it. A read of the record by key in the caller's module would miss it.
#[test]
fn a_slice_write_in_an_imported_callee_is_no_task_write() {
    let lib = "struct Board:\n    rows: List[List[int]]\n    fn poke(self):\n        self.rows[0..2].push([9])\n    fn real(self):\n        self.rows[0].push(9)\nfn work(g: List[List[int]]):\n    g[0..2].push([9])\nfn deep(g: List[List[int]]):\n    g[0..2][0].push(9)\n".to_string();
    let main = |call: &str| {
        format!(
            "import lib\nb := lib.Board([[1], [2], [3]])\ng := [[1], [2], [3]]\nparallel:\n    spawn {call}\nprint(g)\n"
        )
    };
    for (call, errors) in [
        ("lib.work(g)", false),
        ("b.poke()", false),
        ("lib.deep(g)", true),
    ] {
        let (errs, _) = t254_diags("imported_slice", &[("lib.chz", lib.clone())], &main(call));
        let got = errs.iter().any(|e| e.contains("this task's copy"));
        assert_eq!(got, errors, "`spawn {call}`: errors={errs:?}");
    }
}

/// TICKET-254 — `fn_writes::builtin_slice` (the types whose `x[a..b]` builds a new value) and
/// `slice_result` (the types that slice at all) name the same four built-in sequence types. A type
/// `slice_result` slices into itself that the classifier misses stays a place link: the old false
/// D4 error.
#[test]
fn builtin_slice_matches_what_slice_result_slices() {
    let c = Checker::new();
    let rows = [
        (Ty::List(Box::new(Ty::Int)), true),
        (Ty::Str, true),
        (Ty::Bytes, true),
        (Ty::ByteArray, true),
        (Ty::Map(Box::new(Ty::Str), Box::new(Ty::Int)), false),
        (Ty::Set(Box::new(Ty::Int)), false),
        (Ty::Int, false),
        (Ty::Tuple(vec![Ty::Int]), false),
    ];
    for (ty, builtin) in rows {
        assert_eq!(
            fn_writes::builtin_slice(&ty),
            builtin,
            "builtin_slice({ty})"
        );
        assert_eq!(
            c.slice_result(&ty) == Some(ty.clone()),
            builtin,
            "slice_result({ty})"
        );
    }
}

#[test]
fn builtin_list_satisfies_contains_bound_like_the_in_operator() {
    // U5: `2 in [1, 2]` type-checks, so `List[int]` must satisfy `Contains[int]`.
    ok("fn main():\n    print(2 in [1, 2])\n");
    ok(
        "import Contains from std.ops\nfn has[C: Contains[int]](c: C) -> bool:\n    return 2 in c\n\nfn main():\n    print(has([1, 2]))\n",
    );
}

/// TICKET-257 review — a type-parameter needle is not a wildcard: `x in s` with `x: T` faulted at run
/// time on `f("abc", 1)`, and base rejected it. A `T` needle still fits a container of `T`.
#[test]
fn type_param_needle_is_rejected_for_a_concrete_container() {
    rejects(
        "fn f[T](s: str, x: T) -> bool:\n    return x in s\n\nfn main():\n    print(f(\"abc\", 1))\n",
        "cannot test membership",
    );
    rejects(
        "fn g[T](b: bytes, x: T) -> bool:\n    return x in b\n\nfn main():\n    print(g(b\"abc\", \"b\"))\n",
        "cannot test membership",
    );
    rejects(
        "fn h[T](s: Set[int], x: T) -> bool:\n    return x in s\n\nfn main():\n    print(h({1}, 1))\n",
        "cannot test membership",
    );
    ok(
        "import Eq from std.cmp\nfn k[T: Eq](xs: List[T], x: T) -> bool:\n    return x in xs\n\nfn main():\n    print(k([1], 1))\n",
    );
    // A Set or Map probe hashes the needle: it must satisfy `Hashable`. Both programs faulted at run time.
    rejects(
        "fn f[T](s: Set[(int, int)], x: (T, int)) -> bool:\n    return x in s\n\nfn main():\n    print(f({(1, 2)}, ([1], 2)))\n",
        "cannot test membership",
    );
    rejects(
        "import Hashable from std.cmp\nfn h[T: Hashable](s: Set[T], x: List[int]) -> bool:\n    return x in s\n\nfn main():\n    print(h({\"a\"}, [1]))\n",
        "cannot test membership",
    );
    rejects(
        "fn m(d: Map[int, int], x: List[int]) -> bool:\n    return x in d\n\nfn main():\n    print(m({1: 2}, [1]))\n",
        "cannot test membership",
    );
    ok(
        "import Eq, Hashable from std.cmp\nfn f[T: Hashable + Eq](s: Set[(T, int)], x: (T, int)) -> bool:\n    return x in s\n\nfn main():\n    print(f({(1, 2)}, (1, 2)))\n",
    );
    ok(
        "import Eq, Hashable from std.cmp\nfn g[T: Hashable + Eq](s: List[(T, int)], x: (T, int)) -> bool:\n    return x in s\n\nfn main():\n    print(g([(1, 2)], (1, 2)))\n",
    );
    ok(
        "fn k(s: List[(int, int)], x: (int, int)) -> bool:\n    return x in s\n\nfn main():\n    print(k([(1, 2)], (1, 2)))\n",
    );
    // A List compares by `==`, so a `T` needle fits as `x == e` does; a str `in` is a substring test.
    ok(
        "fn f[T](xs: List[int], x: T) -> bool:\n    return x in xs\n\nfn main():\n    print(f([1], 1))\n",
    );
    rejects(
        "fn f[T](s: str, x: T) -> bool:\n    return x in s\n\nfn main():\n    print(f(\"abc\", \"a\"))\n",
        "cannot test membership",
    );
    // `float` is not `Hashable` (owner rule), so a Set or Map probe rejects it although `1 == 1.0`.
    rejects(
        "fn main():\n    print(1.0 in {1})\n",
        "cannot test membership",
    );
    rejects(
        "fn main():\n    print({1: \"a\"}.contains(1.0))\n",
        "cannot test membership",
    );
}

/// TICKET-257 — `RecvKind::ALL` lists every variant, in declaration order. The exhaustive `match`
/// has no wildcard, so a new variant fails the build here until it gets an index.
#[test]
fn recv_kind_all_lists_every_variant() {
    use super::proto::RecvKind;
    let index = |k: RecvKind| match k {
        RecvKind::Int => 0,
        RecvKind::Float => 1,
        RecvKind::Bool => 2,
        RecvKind::Str => 3,
        RecvKind::Bytes => 4,
        RecvKind::ByteArray => 5,
        RecvKind::List => 6,
        RecvKind::Map => 7,
        RecvKind::Set => 8,
        RecvKind::Tuple => 9,
        RecvKind::Struct => 10,
        RecvKind::Cursor => 11,
        RecvKind::Enum => 12,
        RecvKind::Option => 13,
        RecvKind::Result => 14,
        RecvKind::Func => 15,
        RecvKind::Protocol => 16,
        RecvKind::Channel => 17,
        RecvKind::Shared => 18,
        RecvKind::RwShared => 19,
        RecvKind::Atomic => 20,
    };
    let max = RecvKind::ALL.iter().map(|k| index(*k)).max().unwrap();
    assert_eq!(RecvKind::ALL.len(), max + 1, "ALL misses a variant");
    for k in RecvKind::ALL {
        assert_eq!(RecvKind::ALL[index(k)], k);
    }
}
