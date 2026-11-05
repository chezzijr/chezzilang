//! TICKET-265 — a std import checks and compiles only the std fns the program reaches. The module
//! graph owns the answer ([`crate::resolver::ModuleGraph::skips_fn_body`]); these tests pin it per
//! import shape, then read it back from the checker's body count and the compiled protos.

use super::*;

use crate::test_support::TmpDir;

fn graph_of(src: &str) -> crate::resolver::ModuleGraph {
    let t = TmpDir::new();
    t.write("chezzi.toml", "[project]\nname = \"g\"\n");
    let entry = t.write("main.chz", src);
    crate::resolver::build_graph(&entry).expect("graph should build")
}

fn module<'g>(
    graph: &'g crate::resolver::ModuleGraph,
    dotted: &[&str],
) -> &'g crate::resolver::LoadedModule {
    graph
        .modules
        .iter()
        .find(|m| m.dotted == dotted)
        .unwrap_or_else(|| panic!("the graph has no module {dotted:?}"))
}

/// The top-level fn names of `lm` the graph keeps (checks and compiles).
fn kept_fns(
    graph: &crate::resolver::ModuleGraph,
    lm: &crate::resolver::LoadedModule,
) -> Vec<String> {
    lm.ast
        .stmts
        .iter()
        .filter_map(|s| match &s.kind {
            StmtKind::Fn(decl) if !graph.skips_fn_body(lm, &decl.name) => Some(decl.name.clone()),
            _ => None,
        })
        .collect()
}

/// One row: a program, a std module, the fns of that module it must keep, and the fns it must skip.
struct Row {
    src: &'static str,
    module: &'static [&'static str],
    kept: &'static [&'static str],
    skipped: &'static [&'static str],
}

const ROWS: &[Row] = &[
    // from-import
    Row {
        src: "import is_empty from std.string\nprint(is_empty(\"\"))\n",
        module: &["std", "string"],
        kept: &["is_empty"],
        skipped: &["reverse", "title", "is_cased"],
    },
    // alias import, qualified call
    Row {
        src: "import std.string as s\nprint(s.reverse(\"ab\"))\n",
        module: &["std", "string"],
        kept: &["reverse"],
        skipped: &["is_empty", "title"],
    },
    // whole-module import, qualified call
    Row {
        src: "import std.string\nprint(string.is_empty(\"\"))\n",
        module: &["std", "string"],
        kept: &["is_empty"],
        skipped: &["reverse"],
    },
    // a name inside an f-string hole
    Row {
        src: "import std.string as s\nprint(\"{s.reverse(\"ab\")}\")\n",
        module: &["std", "string"],
        kept: &["reverse"],
        skipped: &["is_empty"],
    },
    // a name only in a comment reaches nothing
    Row {
        src: "import std.string as s\n# s.is_empty\nprint(1)\n",
        module: &["std", "string"],
        kept: &[],
        skipped: &["is_empty"],
    },
    // transitive, same module: `title` calls `is_cased`
    Row {
        src: "import std.string as s\nprint(s.title(\"ab\"))\n",
        module: &["std", "string"],
        kept: &["title", "is_cased"],
        skipped: &["is_empty"],
    },
    // transitive, across modules: `datetime.pad_year` calls `s.pad_left`
    Row {
        src: "import std.datetime as d\nprint(d.pad_year(5))\n",
        module: &["std", "string"],
        kept: &["pad_left"],
        skipped: &["is_empty"],
    },
    // a datetime fn that reaches no std.string fn
    Row {
        src: "import std.datetime as d\nprint(d.is_leap_year(2000))\n",
        module: &["std", "string"],
        kept: &[],
        skipped: &["pad_left"],
    },
    // the word names a fn of a module the program does not import: std.string stays unreached
    Row {
        src: "import std.datetime as d\nfn is_empty() -> bool:\n    return d.is_leap_year(2000)\nprint(is_empty())\n",
        module: &["std", "string"],
        kept: &[],
        skipped: &["is_empty"],
    },
    // `json.decode[T]` reads std.json's `parse` slot, a name the program never spells
    Row {
        src: "import std.json as json\nstruct P:\n    x: int\nmatch json.decode[P](\"{{\\\"x\\\": 1}}\"):\n    ?p: print(p.x)\n    !e: print(e)\n",
        module: &["std", "json"],
        kept: &["parse"],
        skipped: &["is_null"],
    },
    // a fn used as a value, never called by name
    Row {
        src: "import max from std.cmp\nf := max[int]\nprint(f(3, 4))\n",
        module: &["std", "cmp"],
        kept: &["max"],
        skipped: &["min", "clamp"],
    },
    // reached only from a method body: `Duration.to_string` calls `_fmt_pos`
    Row {
        src: "import std.duration as d\nprint(d.millis(1500).to_string())\n",
        module: &["std", "duration"],
        kept: &["millis", "_fmt_pos"],
        skipped: &["parse", "since"],
    },
    // a file-backed native module is not gated: `_run_outcome`, called only from the method
    // `Executor.submit`, is kept
    Row {
        src: "import std.concurrency\nex := concurrency.Executor()\nprint(ex.submit(fn() -> int: 1).get())\nex.shutdown()\n",
        module: &["std", "concurrency"],
        kept: &["_run_outcome"],
        skipped: &[],
    },
    // reached only from a closure: the fn `memoize1` returns calls `_memo_get`
    Row {
        src: "import std.memoize\nprint(memoize.memoize1(fn(k: int) -> int: k)(2))\n",
        module: &["std", "memoize"],
        kept: &["memoize1", "_memo_get"],
        skipped: &[],
    },
    // the same import with no call: the closure's callee stays unreached
    Row {
        src: "import std.memoize\nprint(1)\n",
        module: &["std", "memoize"],
        kept: &[],
        skipped: &["memoize1", "_memo_get"],
    },
];

#[test]
fn std_fn_reach_grid() {
    let mut red = Vec::new();
    for row in ROWS {
        let graph = graph_of(row.src);
        let lm = module(&graph, row.module);
        let kept = kept_fns(&graph, lm);
        for name in row.kept {
            if !kept.iter().any(|k| k == name) {
                red.push(format!("{:?}: {name} skipped; kept {kept:?}", row.src));
            }
        }
        for name in row.skipped {
            if kept.iter().any(|k| k == name) {
                red.push(format!("{:?}: {name} kept; kept {kept:?}", row.src));
            }
        }
    }
    assert!(
        red.is_empty(),
        "{} red cells:\n{}",
        red.len(),
        red.join("\n")
    );
}

#[test]
fn a_std_import_checks_only_the_reached_bodies() {
    let graph = graph_of("import is_empty from std.string\nprint(is_empty(\"\"))\n");
    let bodies = crate::checker::check_graph_counting_fn_bodies(&graph);
    assert_eq!(bodies, 1, "checked {bodies} fn bodies");
}

#[test]
fn a_std_import_compiles_only_the_reached_fns() {
    let graph = graph_of("import is_empty from std.string\nprint(is_empty(\"\"))\n");
    let program = compile_graph(&graph).expect("compile");
    let names: Vec<&str> = program
        .protos
        .iter()
        .map(|p| p.name.as_str())
        .filter(|n| !n.starts_with('<'))
        .collect();
    assert_eq!(names, ["is_empty"], "compiled protos {names:?}");
}

/// The entry module keeps every fn, a std file included: `chezzi check std/string.chz` checks
/// each body.
#[test]
fn a_std_entry_keeps_every_fn() {
    let path = crate::resolver::std_root().join("string.chz");
    let graph = crate::resolver::build_graph(&path).expect("graph should build");
    let lm = graph.modules.last().expect("the entry");
    let all = lm
        .ast
        .stmts
        .iter()
        .filter(|s| matches!(s.kind, StmtKind::Fn(_)))
        .count();
    assert_eq!(kept_fns(&graph, lm).len(), all);
}

/// A std body a program does not reach is no longer checked in that program, so each std file is
/// checked as an entry here: every body of every std file passes the checker.
#[test]
fn every_std_file_checks_clean_as_the_entry() {
    let mut files: Vec<_> = std::fs::read_dir(crate::resolver::std_root())
        .expect("std dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "chz"))
        .collect();
    files.sort();
    assert!(files.len() > 10, "found {} std files", files.len());
    for path in files {
        let graph = crate::resolver::build_graph(&path)
            .unwrap_or_else(|e| panic!("{}: {}", path.display(), e.message));
        let (res, _) = crate::checker::check_graph_diags(&graph, None);
        if let Err(errs) = res {
            panic!(
                "{}: {:?}",
                path.display(),
                errs.iter().map(|e| &e.message).collect::<Vec<_>>()
            );
        }
    }
}

/// A range literal counts only in a body the program reaches: `capitalize` slices with `s[0..1]`,
/// `is_empty` builds no range.
#[test]
fn a_range_in_a_skipped_std_body_records_no_range_use() {
    let skipped = graph_of("import std.string as s\nprint(s.is_empty(\"\"))\n");
    assert!(
        !skipped.prelude_used.uses(crate::resolver::RANGE_STRUCT),
        "a skipped body recorded a Range use"
    );
    let reached = graph_of("import std.string as s\nprint(s.capitalize(\"ab\"))\n");
    assert!(
        reached.prelude_used.uses(crate::resolver::RANGE_STRUCT),
        "a reached body recorded no Range use"
    );
    compile_graph(&reached).expect("compile");
}

/// The prelude's bodied native-struct methods (`int.from`, `int.try_from`) are checked and compiled
/// only for a graph whose sources name one ([`crate::resolver::PreludeUse::uses_native_bodies`]):
/// `print(1)` skips them, a member path (`int.from`, `T.from`) keeps them, and the kept program runs
/// them (`tests/chz/spec/builtin_from_test.chz`).
#[test]
fn prelude_native_bodies_are_kept_only_when_named() {
    let rows: [(&str, bool); 6] = [
        ("print(1)\n", false),
        // `from` the import keyword and `bytes.from_iter` name no bodied method.
        (
            "import is_empty from std.string\nprint(is_empty(\"\"))\n",
            false,
        ),
        ("print(bytes.from_iter([1]))\n", false),
        ("print(int.from(true))\n", true),
        ("print(float.try_from(\"1.5\") ?? 0.0)\n", true),
        (
            "import From from std.convert\nfn lift[T: From[bool]](b: bool) -> T:\n    return T.from(b)\nprint(lift[int](true))\n",
            true,
        ),
    ];
    for (src, kept) in rows {
        let graph = graph_of(src);
        let prelude = module(&graph, &["std", "prelude"]);
        assert_eq!(
            !graph.skips_prelude_native_bodies(prelude),
            kept,
            "{src:?}: kept the prelude's native-struct bodies"
        );
        let (res, _) = crate::checker::check_graph_diags(&graph, None);
        assert!(res.is_ok(), "{src:?}: {res:?}");
        let program = compile_graph(&graph).expect("compile");
        assert_eq!(
            program.native_methods.contains_key("int"),
            kept,
            "{src:?}: compiled int's bodied statics"
        );
    }
}
