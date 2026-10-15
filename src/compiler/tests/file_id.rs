//! A1 — `ModuleProto` carries the module's [`crate::lexer::Span::file`] id, so a compiled `Program`
//! can map a span back to the file it came from (`Program::file_path`). Nothing observable changes
//! yet; these are the plumbing tests later tasks build on.

use super::*;

use crate::test_support::TmpDir;

/// Entry imports TWO modules (`a`, `b`) that each import a further module (`c`, `d`) — so the
/// resolver's DFS pre-order `file` id assignment (parent parses before its imports recurse,
/// `resolver::mod.rs`) genuinely disagrees with `graph.modules`' deps-first post-order (a child
/// is PUSHED to the order vec only after it returns, i.e. before its importer). A `modules[file -
/// 1]` indexing bug would silently pick the wrong module here.
fn build_diamond() -> crate::resolver::ModuleGraph {
    let t = TmpDir::new();
    t.write("chezzi.toml", "[project]\nname = \"g\"\n");
    t.write("c.chz", "fn fc() -> int:\n    return 1\n");
    t.write(
        "a.chz",
        "import c\nfn fa() -> int:\n    return c.fc() + 1\n",
    );
    t.write("d.chz", "fn fd() -> int:\n    return 1\n");
    t.write(
        "b.chz",
        "import d\nfn fb() -> int:\n    return d.fd() + 1\n",
    );
    let entry = t.write(
        "main.chz",
        "import a\nimport b\nfn main():\n    print(a.fa() + b.fb())\nmain()\n",
    );
    // `t` (and its files) must outlive the graph build, but the graph itself holds no file
    // handles — dropping `t` after `build_graph` returns is fine, same as the resolver's own
    // fixture tests.
    crate::resolver::build_graph(&entry).expect("graph should build")
}

/// TICKET-255 — a program that holds no range value must not compile the prelude's `Range`
/// methods. Every `chezzi run` pays for each proto compiled before the first user statement.
#[test]
fn program_without_a_range_compiles_no_prelude_fn_protos() {
    let t = TmpDir::new();
    t.write("chezzi.toml", "[project]\nname = \"g\"\n");
    let entry = t.write("main.chz", "print(1)\n");
    let graph = crate::resolver::build_graph(&entry).expect("graph should build");
    let program = compile_graph(&graph).expect("compile");
    let range_protos: Vec<&str> = program
        .protos
        .iter()
        .map(|p| p.name.as_str())
        .filter(|n| !n.starts_with('<'))
        .collect();
    assert!(
        range_protos.is_empty(),
        "print(1) compiled {} prelude fn protos: {range_protos:?}",
        range_protos.len()
    );
}

/// TICKET-255 — programs that build no `Range` value: a counting loop over a literal range takes the
/// counter path (`ast::counting_range`) and needs no struct.
const LAZY_ROWS: &[&str] = &[
    "print(1)\n",
    "for i in 0..3:\n    print(i)\n",
    "for i in 0..=3:\n    print(i)\n",
    "for i in 2..:\n    break\n",
    "for _ in 0..3:\n    print(1)\n",
    "n := 3\nwhile n > 0:\n    for i in 0..n:\n        print(i)\n    n -= 1\n",
];

/// TICKET-255 — programs that build a `Range` value: every one needs the prelude methods.
const RANGE_ROWS: &[&str] = &[
    "r := 0..3\nprint(r.len())\n",
    "xs := [1, 2, 3]\nprint(xs[0..2])\n",
    "print(Range(0, 3).len())\n",
    "print(\"{(0..3).len()}\")\n",
];

/// TICKET-255 — forms the walker may count either way; each must still compile.
const EITHER_ROWS: &[&str] = &[
    "print([x for x in 0..3])\n",
    "for i in (0..3):\n    print(i)\n",
];

fn graph_of(src: &str) -> crate::resolver::ModuleGraph {
    let t = TmpDir::new();
    t.write("chezzi.toml", "[project]\nname = \"g\"\n");
    let entry = t.write("main.chz", src);
    crate::resolver::build_graph(&entry).expect("graph should build")
}

fn prelude_range_methods(graph: &crate::resolver::ModuleGraph) -> Vec<String> {
    graph.modules[0]
        .ast
        .stmts
        .iter()
        .find_map(|s| match &s.kind {
            StmtKind::Struct { name, methods, .. } if name == crate::resolver::RANGE_STRUCT => {
                Some(methods.iter().map(|m| m.name.clone()).collect())
            }
            _ => None,
        })
        .expect("the prelude declares Range")
}

#[test]
fn range_free_graph_checks_no_prelude_fn_bodies() {
    for src in LAZY_ROWS {
        let graph = graph_of(src);
        assert!(!graph.range_used, "{src:?} recorded a Range use");
        let bodies = crate::checker::check_graph_counting_fn_bodies(&graph);
        assert_eq!(bodies, 0, "{src:?} checked {bodies} fn bodies");
    }
    for src in RANGE_ROWS {
        let graph = graph_of(src);
        assert!(graph.range_used, "{src:?} recorded no Range use");
        let bodies = crate::checker::check_graph_counting_fn_bodies(&graph);
        let methods = prelude_range_methods(&graph).len();
        assert!(
            bodies >= methods,
            "{src:?} checked {bodies} bodies, below {methods}"
        );
    }
}

#[test]
fn range_use_grid_matches_the_compiled_methods() {
    for src in LAZY_ROWS.iter().chain(RANGE_ROWS).chain(EITHER_ROWS) {
        let graph = graph_of(src);
        let program = compile_graph(&graph).unwrap_or_else(|e| panic!("{src:?}: {}", e.message));
        let have: Vec<&String> = program.structs[&program.range_key].methods.keys().collect();
        if LAZY_ROWS.contains(src) {
            assert!(have.is_empty(), "{src:?} compiled Range methods {have:?}");
        } else if RANGE_ROWS.contains(src) {
            for m in prelude_range_methods(&graph) {
                assert!(have.contains(&&m), "{src:?} lacks Range method {m}");
            }
        }
    }
}

#[test]
fn module_protos_carry_their_graph_file_ids() {
    let graph = build_diamond();
    assert!(graph.modules.len() >= 5, "expected the 5 written modules");
    let program = compile_graph(&graph).expect("compile");
    assert_eq!(program.modules.len(), graph.modules.len());

    let mut seen = std::collections::HashSet::new();
    for (i, lm) in graph.modules.iter().enumerate() {
        assert_eq!(
            program.modules[i].file,
            lm.file,
            "module {} (index {i}) lost its graph file id",
            lm.label()
        );
        assert_ne!(lm.file, 0, "module {} got sentinel file id 0", lm.label());
        assert!(
            seen.insert(lm.file),
            "file id {} is not unique across modules",
            lm.file
        );
    }
}

#[test]
fn file_path_resolves_by_id_not_by_index() {
    let graph = build_diamond();
    let program = compile_graph(&graph).expect("compile");

    for lm in &graph.modules {
        assert_eq!(
            program.file_path(lm.file),
            Some(lm.id.0.as_path()),
            "file_path({}) should resolve to {}'s path — a `modules[file - 1]` index bug \
                 returns some OTHER module's path here (pre-order file ids vs. post-order modules)",
            lm.file,
            lm.label()
        );
    }
    assert_eq!(program.file_path(0), None, "file id 0 must never resolve");
    assert_eq!(
        program.file_path(99999),
        None,
        "an unknown file id must resolve to None, not panic"
    );
}

#[test]
fn single_module_compile_has_no_file_id() {
    let tokens = crate::lexer::tokenize("fn main():\n    print(1)\nmain()\n").expect("lex");
    let module = crate::parser::parse(tokens).expect("parse");
    let program = compile_module_standalone(&module).expect("compile");

    assert_eq!(program.modules.len(), 1);
    assert_eq!(program.modules[0].file, 0);
    assert_eq!(program.file_path(0), None);
}
