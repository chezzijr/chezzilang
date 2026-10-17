//! Type-checker tests. Each `*_rejected` test pins a real bug class the checker must catch (the
//! "red" that drove the code); the `*_ok` tests guard against false positives on valid programs.

use super::*;
use crate::test_support::TmpDir;
use crate::{lexer, parser};

/// The text of `tyvar::ERR_VALUE_NEEDS_TYPE`, as a literal: this file must also compile against a
/// base checker that has no such const (the pipeline gate runs the repro test there).
const ERR_VALUE_NEEDS_TYPE_TEXT: &str = "a `!` value needs its type from an annotation: add `-> T!E` to the function, or annotate the binding, e.g. `w: int!str = !e`";

/// Type-check a source string, returning the collected errors (empty = clean).
fn check_src(src: &str) -> Vec<CheckError> {
    let tokens = lexer::tokenize(src).expect("lex should succeed");
    let module = parser::parse(tokens).expect("parse should succeed");
    // A program with an `import` goes through the module graph, so `import Add from std.ops` binds
    // the protocol as it does for `chezzi check`. A program the resolver rejects stays on the
    // single-module path (its test pins that path's diagnostic).
    let imports = module
        .stmts
        .iter()
        .any(|s| matches!(s.kind, crate::ast::StmtKind::Import(_)));
    let errs = match imports.then(|| graph_check(src)).flatten() {
        Some(errs) => errs,
        None => match check(&module) {
            Ok(()) => Vec::new(),
            Err(e) => e,
        },
    };
    assert_no_removed_spelling(src, &errs);
    errs
}

/// [`check_entry`] without the panic: `None` when the resolver rejects the program.
fn graph_check(src: &str) -> Option<Vec<CheckError>> {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).ok()?;
    Some(match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    })
}

/// The result and the warnings of one graph check.
type GraphDiags = (Result<(), Vec<CheckError>>, Vec<CheckError>);

/// [`graph_check`] for diagnostics: the result and the warnings, `None` when the resolver rejects.
fn graph_diags(src: &str) -> Option<GraphDiags> {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).ok()?;
    Some(check_graph_diags(&graph, None))
}

/// TICKET-228 (D6) — a diagnostic names a carrier by its type (`int?`, `int!str`) or its pattern
/// (`?_`, `!_`), never by a long name the user cannot write. When no word of `src` is one of the
/// five removed names, no `message` or `help` may hold one. No allow-list: fix the message.
fn assert_no_removed_spelling(src: &str, diags: &[CheckError]) {
    const REMOVED: [&str; 5] = ["Option", "Result", "Some", "Ok", "Err"];
    fn words(text: &str) -> impl Iterator<Item = &str> {
        text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
    }
    if words(src).any(|w| REMOVED.contains(&w)) {
        return;
    }
    for d in diags {
        for text in [Some(d.message.as_str()), d.help.as_deref()]
            .into_iter()
            .flatten()
        {
            if let Some(w) = words(text).find(|w| REMOVED.contains(w)) {
                panic!("a diagnostic names the removed spelling `{w}`: {text:?}\nsource:\n{src}");
            }
        }
    }
}

/// Assert the source type-checks clean.
fn ok(src: &str) {
    let errs = check_src(src);
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
}

/// TICKET-265 — a std protocol other than the prelude's binds only through an import. A `rejects*`
/// helper panics when an error says `unknown name '<Protocol>'` (or `unknown type`, `unknown generic
/// type`, `unknown protocol`) for such a protocol: the program forgot its import, so a needle that
/// matches some other error proves nothing. A needle that itself says `unknown` pins that error.
fn assert_no_missing_std_import(src: &str, errs: &[CheckError], needle: &str) {
    if needle.contains("unknown") {
        return;
    }
    const HEADS: [&str; 4] = [
        "unknown name '",
        "unknown type '",
        "unknown generic type '",
        "unknown protocol '",
    ];
    for e in errs {
        // A module error carries the prefix `in module 'm': `.
        let msg = match e.message.strip_prefix("in module '") {
            Some(rest) => rest.split_once("': ").map_or(rest, |(_, m)| m),
            None => e.message.as_str(),
        };
        let Some(name) = HEADS
            .iter()
            .find_map(|h| msg.strip_prefix(h))
            .and_then(|rest| rest.split('\'').next())
        else {
            continue;
        };
        let owned_by_import = is_std_protocol(name) && !is_global_protocol(name);
        assert!(
            !owned_by_import,
            "program names std protocol '{name}' without an import: {}\nsource:\n{src}",
            e.message
        );
    }
}

/// Assert the source produces at least one error whose message contains `needle`.
fn rejects(src: &str, needle: &str) {
    let errs = check_src(src);
    assert_no_missing_std_import(src, &errs, needle);
    assert!(
        errs.iter().any(|e| e.message.contains(needle)),
        "expected an error containing {needle:?}, got: {errs:?}"
    );
}

/// The first error the lexer or the parser reports for `src`, as `(line, col, message)`. A hole is
/// lexed and parsed with its literal, so a malformed hole fails here, before the checker runs.
fn front_err(src: &str) -> (usize, usize, String) {
    match lexer::tokenize(src) {
        Err(e) => (e.line, e.col, e.message),
        Ok(tokens) => match parser::parse(tokens) {
            Err(e) => (e.span.line as usize, e.span.col as usize, e.message),
            Ok(_) => panic!("expected a lex or parse error, got none for {src:?}"),
        },
    }
}

/// Assert the lexer or the parser rejects `src` with a message containing `needle`.
fn front_rejects(src: &str, needle: &str) {
    let (_, _, message) = front_err(src);
    assert!(
        message.contains(needle),
        "expected an error containing {needle:?}, got: {message:?}"
    );
}

/// Assert the source fails to PARSE with a message containing `needle`.
fn parse_rejects(src: &str, needle: &str) {
    let tokens = lexer::tokenize(src).expect("lex should succeed");
    let errs: Vec<String> = match parser::parse(tokens) {
        Ok(_) => Vec::new(),
        Err(e) => vec![e.message],
    };
    assert!(
        errs.iter().any(|m| m.contains(needle)),
        "expected an error containing {needle:?}, got: {errs:?}"
    );
}

/// Like `rejects`, plus asserting the SAME error's `help` contains `help_needle`.
fn rejects_help(src: &str, msg_needle: &str, help_needle: &str) {
    let errs = check_src(src);
    assert_no_missing_std_import(src, &errs, msg_needle);
    assert!(
        errs.iter().any(|e| e.message.contains(msg_needle)
            && e.help.as_deref().is_some_and(|h| h.contains(help_needle))),
        "expected an error containing {msg_needle:?} with help containing {help_needle:?}, got: {errs:?}"
    );
}

fn rejects_help_at(src: &str, msg_needle: &str, help_needle: &str, line: u32) {
    let errs = check_src(src);
    assert_no_missing_std_import(src, &errs, msg_needle);
    assert!(
        errs.iter().any(|e| e.message.contains(msg_needle)
            && e.help.as_deref().is_some_and(|h| h.contains(help_needle))
            && e.span.line == line),
        "expected a task-side error at line {line} with help containing {help_needle:?}, got: {errs:?}"
    );
}

/// Type-check a source string, returning the collected WARNINGS (non-fatal diagnostics).
fn warn_src(src: &str) -> (Vec<CheckError>, Vec<CheckError>) {
    let tokens = lexer::tokenize(src).expect("lex should succeed");
    let module = parser::parse(tokens).expect("parse should succeed");
    // Same routing as `check_src`: an `import` binds a std protocol only through the module graph.
    let imports = module
        .stmts
        .iter()
        .any(|s| matches!(s.kind, crate::ast::StmtKind::Import(_)));
    let graphed = imports.then(|| graph_diags(src)).flatten();
    let (res, warns) = graphed.unwrap_or_else(|| check_diags(&module));
    let errs = res.err().unwrap_or_default();
    assert_no_removed_spelling(src, &errs);
    assert_no_removed_spelling(src, &warns);
    (errs, warns)
}

/// Assert the source type-checks CLEAN and emits a warning containing `needle`. The clean assertion
/// is load-bearing: without it the helper cannot tell a warning from an error, and every rule built
/// on it would pass while emitting a hard error.
fn warns(src: &str, needle: &str) {
    let (errs, warns) = warn_src(src);
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
    assert!(
        warns.iter().any(|w| w.message.contains(needle)),
        "expected a warning containing {needle:?}, got: {warns:?}"
    );
}

/// Assert the source type-checks clean and emits NO warnings — the false-positive guard for a rule
/// added via [`warns`].
fn no_warn(src: &str) {
    let (errs, warns) = warn_src(src);
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
    assert!(warns.is_empty(), "expected no warnings, got: {warns:?}");
}

fn entry_ok(src: &str) {
    let errs = check_entry(src);
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
}

fn entry_rejects(src: &str, needle: &str) {
    let errs = check_entry(src);
    assert_no_missing_std_import(src, &errs, needle);
    assert!(
        errs.iter().any(|e| e.message.contains(needle)),
        "expected an error containing {needle:?}, got: {errs:?}"
    );
}

/// Type-check an entry program that may import native std modules (resolved via `build_graph`).
fn check_entry(src: &str) -> Vec<CheckError> {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    }
}

fn files_ok(files: &[(&str, &str)]) {
    let errs = check_files(files);
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
}

fn files_reject(files: &[(&str, &str)], needle: &str) {
    let errs = check_files(files);
    assert!(
        errs.iter().any(|e| e.message.contains(needle)),
        "expected an error containing {needle:?}, got: {errs:?}"
    );
}

/// Assert the desugared source type-checks clean.
fn ok_desugared(src: &str) {
    let errs = check_desugared(src);
    assert!(errs.is_empty(), "expected no type errors, got: {errs:?}");
}

/// Assert the desugared source produces an error containing `needle`.
fn rejects_desugared(src: &str, needle: &str) {
    let errs = check_desugared(src);
    assert_no_missing_std_import(src, &errs, needle);
    assert!(
        errs.iter().any(|e| e.message.contains(needle)),
        "expected an error containing {needle:?}, got: {errs:?}"
    );
}

/// Type-check a source string after running the desugar pass (default validation, provider
/// synthesis, full-path folding; call binding and `?.`/`??` carrier lowering are the CHECKER's job). Production always desugars before the checker (`resolver::build_graph`);
/// these tests otherwise bypass it. Returns the collected errors.
fn check_desugared(src: &str) -> Vec<CheckError> {
    let tokens = lexer::tokenize(src).expect("lex should succeed");
    let mut module = parser::parse(tokens).expect("parse should succeed");
    // An `import` binds a std protocol only through the module graph, which desugars too.
    if module
        .stmts
        .iter()
        .any(|s| matches!(s.kind, crate::ast::StmtKind::Import(_)))
    {
        let errs = check_entry(src);
        assert_no_removed_spelling(src, &errs);
        return errs;
    }
    crate::desugar::run_standalone(&mut module).expect("desugar should succeed");
    let errs = match check(&module) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    };
    assert_no_removed_spelling(src, &errs);
    errs
}

/// [`no_warn`], but through `build_graph` — the only wire that resolves a native std import, so it is
/// what a rule whose negative cases need `Shared`/`RwShared`/`Atomic` has to use.
fn entry_no_warn(src: &str) {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let (res, warns) = check_graph_diags(&graph, None);
    assert!(res.is_ok(), "expected no type errors, got: {res:?}");
    assert!(warns.is_empty(), "expected no warnings, got: {warns:?}");
}

/// Build a multi-file graph from (rel, src) pairs (first must be `main.chz`) and check it.
fn check_files(files: &[(&str, &str)]) -> Vec<CheckError> {
    let t = TmpDir::new();
    let mut entry = None;
    for (rel, src) in files {
        if let Some(parent) = std::path::Path::new(rel).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(t.0.join(parent)).unwrap();
        }
        let p = t.write(rel, src);
        if *rel == "main.chz" {
            entry = Some(p);
        }
    }
    let graph =
        crate::resolver::build_graph(&entry.expect("a main.chz")).expect("resolve should succeed");
    match check_graph(&graph) {
        Ok(()) => Vec::new(),
        Err(e) => e,
    }
}

/// `rejects`, but through `check_entry` (the program imports a native std module).
fn rejects_entry(src: &str, needle: &str) {
    let errs = check_entry(src);
    assert_no_missing_std_import(src, &errs, needle);
    assert!(
        errs.iter().any(|e| e.message.contains(needle)),
        "expected an error containing {needle:?}, got: {errs:?}"
    );
}

/// Phase 5a-containers — a `Checker` after a trivial graph check, with the always-linked prelude's
/// `List`/`Map`/`Set` method tables (harvested into `container_seeds`) re-seeded into `self.structs`.
/// Lets a test drive `native_handle_method("List"/"Map"/"Set", …)` exactly as the `Ty::List`/`Ty::Map`/
/// `Ty::Set` dispatch arms do.
#[cfg(test)]
fn prelude_container_checker() -> Checker {
    let t = TmpDir::new();
    let entry = t.write("main.chz", "fn main(): print(1)\n");
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    let mut c = Checker::new();
    c.run_graph_pass(&graph, false);
    assert!(c.errors.is_empty(), "graph check errored: {:?}", c.errors);
    assert!(
        c.container_seeds.contains_key("List")
            && c.container_seeds.contains_key("Map")
            && c.container_seeds.contains_key("Set")
            && c.container_seeds.contains_key("Channel")
            && c.container_seeds.contains_key("str")
            && c.container_seeds.contains_key("bytes")
            && c.container_seeds.contains_key("bytearray"),
        "prelude List/Map/Set/Channel/str/bytes/bytearray container seeds must be harvested"
    );
    // container_seeds is populated; re-seed it into self.structs for the method-table lookup.
    c.seed_stdlib_structs();
    c
}

/// The effective ModuleSig of native module `std.<module>` produced by a full graph check that
/// `import std.<module>` — i.e. the sig the CLI actually uses (harvested-from-file for a file-backed
/// module + `attach_native_module_metadata`), NOT the raw `native_module_sig` table. `module` is the
/// bare submodule name, e.g. "math" / "io" / "regex".
#[cfg(test)]
fn native_module_sig_via_graph(module: &str) -> ModuleSig {
    let t = TmpDir::new();
    let entry = t.write(
        "main.chz",
        &format!("import std.{module}\nfn main(): print(1)\n"),
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    let mut c = Checker::new();
    c.run_graph_pass(&graph, false);
    assert!(c.errors.is_empty(), "graph check errored: {:?}", c.errors);
    let id = graph
        .modules
        .iter()
        .find(|m| m.dotted == ["std", module])
        .unwrap_or_else(|| panic!("std.{module} in graph"))
        .id
        .clone();
    c.module_sigs
        .get(&id)
        .unwrap_or_else(|| panic!("std.{module} ModuleSig"))
        .clone()
}

/// Type-check an entry program and report how many [`Checker::enter_eq_obligation`] entries the
/// `Eq`-growth walk made -- the counted-work replacement for a wall-clock bound (TICKET-059). Inlines
/// [`check_graph_diags`]'s body inside its own `on_frontend_stack_scoped` closure, the same workaround
/// `stack_probe_frontend_walker_depth` uses for `checker::check`: the entry counter is thread-local,
/// and that fn spawns a fresh 1 GiB thread whose thread-locals start at zero, so reading the counter on
/// the test's own thread would always measure 0.
fn eq_obligations_for(src: &str) -> (Vec<CheckError>, u64) {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    crate::on_frontend_stack_scoped(move || {
        Checker::reset_eq_obligation_entries();
        let mut c = Checker::new();
        c.run_graph_pass(&graph, false);
        (c.errors, Checker::eq_obligation_entries())
    })
}

/// TICKET-257 — one sample receiver per kind for the operator/bound grid.
struct GridSample {
    prelude: &'static str,
    lit: &'static str,
    /// Needles that must pass `in` / `Contains[TY]`: (literal, type).
    items: &'static [(&'static str, &'static str)],
    /// One needle that must fail.
    bad_item: Option<(&'static str, &'static str)>,
    /// What `for v in a` yields.
    elem: Option<&'static str>,
    /// (key literal, K, V) of `a[k]`.
    index: Option<(&'static str, &'static str, &'static str)>,
    /// A value literal for `a[k] = v`.
    set_val: Option<&'static str>,
    /// The result type of `a[0..1]`.
    slice: Option<&'static str>,
}

/// Exhaustive over `RecvKind` (no wildcard): a new kind fails the build until it has a sample.
fn grid_samples(k: super::proto::RecvKind) -> Vec<GridSample> {
    use super::proto::RecvKind;
    const CONC: &str = "import std.concurrency\n";
    match k {
        RecvKind::Int => vec![GridSample {
            lit: "7",
            ..GRID_BARE
        }],
        RecvKind::Float => vec![GridSample {
            lit: "1.5",
            ..GRID_BARE
        }],
        RecvKind::Bool => vec![GridSample {
            lit: "true",
            ..GRID_BARE
        }],
        RecvKind::Str => vec![GridSample {
            lit: r#""abc""#,
            items: &[(r#""b""#, "str")],
            bad_item: Some(("98", "int")),
            elem: Some("str"),
            index: Some(("0", "int", "str")),
            slice: Some("str"),
            ..GRID_BARE
        }],
        RecvKind::Bytes => vec![GridSample {
            lit: r#"b"abc""#,
            items: &[
                ("98", "int"),
                (r#"b"b""#, "bytes"),
                (r#"bytearray(b"b")"#, "bytearray"),
            ],
            bad_item: Some((r#""b""#, "str")),
            elem: Some("int"),
            index: Some(("0", "int", "int")),
            slice: Some("bytes"),
            ..GRID_BARE
        }],
        RecvKind::ByteArray => vec![GridSample {
            lit: r#"bytearray(b"abc")"#,
            items: &[
                ("98", "int"),
                (r#"b"bc""#, "bytes"),
                (r#"bytearray(b"b")"#, "bytearray"),
            ],
            bad_item: Some((r#""b""#, "str")),
            elem: Some("int"),
            index: Some(("0", "int", "int")),
            set_val: Some("1"),
            slice: Some("bytearray"),
            ..GRID_BARE
        }],
        RecvKind::List => vec![GridSample {
            lit: "[1, 2]",
            items: &[("1", "int")],
            bad_item: Some((r#""x""#, "str")),
            elem: Some("int"),
            index: Some(("0", "int", "int")),
            set_val: Some("5"),
            slice: Some("List[int]"),
            ..GRID_BARE
        }],
        RecvKind::Set => vec![GridSample {
            lit: "{1, 2}",
            items: &[("1", "int")],
            bad_item: Some((r#""x""#, "str")),
            elem: Some("int"),
            ..GRID_BARE
        }],
        RecvKind::Map => vec![GridSample {
            lit: r#"{"a": 1}"#,
            items: &[(r#""a""#, "str")],
            bad_item: Some(("1", "int")),
            elem: Some("(str, int)"),
            index: Some((r#""a""#, "str", "int")),
            set_val: Some("5"),
            ..GRID_BARE
        }],
        RecvKind::Tuple => vec![GridSample {
            lit: "(1, 2)",
            ..GRID_BARE
        }],
        RecvKind::Struct => vec![
            GridSample {
                prelude: "struct P:\n    x: int\n",
                lit: "P(1)",
                ..GRID_BARE
            },
            GridSample {
                lit: "0..3",
                items: &[("1", "int")],
                bad_item: Some((r#""x""#, "str")),
                elem: Some("int"),
                ..GRID_BARE
            },
        ],
        RecvKind::Cursor => vec![GridSample {
            lit: "[1].iter()",
            elem: Some("int"),
            ..GRID_BARE
        }],
        RecvKind::Enum => vec![GridSample {
            prelude: "enum En:\n    A\n    B\n",
            lit: "En.A",
            ..GRID_BARE
        }],
        RecvKind::Option => vec![GridSample {
            prelude: "fn ho() -> int?:\n    return 1\n",
            lit: "ho()",
            ..GRID_BARE
        }],
        RecvKind::Result => vec![GridSample {
            prelude: "fn hr() -> int!str:\n    return 1\n",
            lit: "hr()",
            ..GRID_BARE
        }],
        RecvKind::Func => vec![GridSample {
            prelude: "fn g(x: int) -> int:\n    return x\n",
            lit: "g",
            ..GRID_BARE
        }],
        RecvKind::Protocol => vec![GridSample {
            prelude: "protocol Sized_:\n    fn size(self) -> int\nstruct Tag:\n    n: int\n    fn size(self) -> int:\n        return self.n\nfn mkp() -> Sized_:\n    return Tag(1)\n",
            lit: "mkp()",
            ..GRID_BARE
        }],
        RecvKind::Channel => vec![GridSample {
            prelude: CONC,
            lit: "Channel[int](1)",
            elem: Some("int"),
            ..GRID_BARE
        }],
        RecvKind::Shared => vec![GridSample {
            prelude: CONC,
            lit: "Shared(1)",
            ..GRID_BARE
        }],
        RecvKind::RwShared => vec![GridSample {
            prelude: CONC,
            lit: "RwShared(1)",
            ..GRID_BARE
        }],
        RecvKind::Atomic => vec![GridSample {
            prelude: CONC,
            lit: "Atomic(1)",
            ..GRID_BARE
        }],
    }
}

/// [`entry_no_warn`]'s positive twin: clean, and a warning containing `needle`.
fn entry_warns(src: &str, needle: &str) {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let (res, warns) = check_graph_diags(&graph, None);
    assert!(res.is_ok(), "expected no type errors, got: {res:?}");
    assert!(
        warns.iter().any(|w| w.message.contains(needle)),
        "expected a warning containing {needle:?}, got: {warns:?}"
    );
}

/// [`entry_warns`], but the program must carry EXACTLY ONE warning. A `Map`/`List` payload's `remove`/`pop`
/// returns a carrier, so the W8-2 discarded-`Option` rule would add a second warning to the same statement.
fn entry_warns_once(src: &str, needle: &str) {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve should succeed");
    let (res, warns) = check_graph_diags(&graph, None);
    assert!(res.is_ok(), "expected no type errors, got: {res:?}");
    assert_eq!(
        warns.len(),
        1,
        "expected exactly one warning, got: {warns:?}"
    );
    assert!(
        warns[0].message.contains(needle),
        "expected the warning to contain {needle:?}, got: {warns:?}"
    );
}

const GRID_BARE: GridSample = GridSample {
    prelude: "",
    lit: "",
    items: &[],
    bad_item: None,
    elem: None,
    index: None,
    set_val: None,
    slice: None,
};

mod basics;
mod builtins_misc;
mod carriers_control;
mod coercion_assign;
mod concurrency;
mod core_methods;
mod empty_collections;
mod ffi;
mod fn_values;
mod generics;
mod late_bounds;
mod names;
mod none_carriers;
mod protocols_eq;
mod receiverless_widening;
mod soundness;
mod statements;
mod static_turbofish;
mod std_modules;
mod std_protocol_imports;
mod temporaries;
mod unify_rules;
mod unused_bindings;
mod warnings;
mod witnesses;
