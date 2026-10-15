//! Checker: check entry points, hover, extern signatures and the call/compile table passes.
//! Split out of `src/checker/mod.rs`. `super::*` is the parent module.

use super::*;

/// Type-check a single parsed module (no imports). Retained as the unit-test entry point; the CLI
/// drives [`check_graph`] so single- and multi-file programs share one path.
///
/// Runs on [`crate::on_frontend_stack_scoped`]'s dedicated stack — see that fn's doc comment. This is
/// the sole reason `check_src`/`ok`/`rejects` and the ~4000 other Rust test-harness callers that go
/// through this fn are no longer the smallest stack in the tree: they now get the same 1 GiB frontend
/// stack production always had, by construction, instead of whatever the ambient test thread happens
/// to be sized at.
#[cfg(test)]
pub fn check(module: &crate::ast::Module) -> Result<(), Vec<CheckError>> {
    check_diags(module).0
}

/// [`check`] plus the non-fatal warnings the pass produced — the single-module twin of
/// [`check_graph_diags`]. Warnings never affect the `Result`.
#[cfg(test)]
pub fn check_diags(module: &crate::ast::Module) -> (Result<(), Vec<CheckError>>, Vec<CheckError>) {
    let (res, warnings, _, _) = check_diags_with(module, true);
    (res, warnings)
}

/// [`check`], plus the number of fn bodies the pass checked (TICKET-233;
/// `Checker::fn_body_checks`).
#[cfg(test)]
pub fn check_counting_fn_body_checks(
    module: &crate::ast::Module,
) -> (Result<(), Vec<CheckError>>, usize) {
    let (res, _, checks, _) = check_diags_with(module, true);
    (res, checks)
}

/// TICKET-255 — the number of fn bodies one checker pass over `graph` enters. The graph twin of
/// [`check_counting_fn_body_checks`]. The count runs on the checker's own thread, so it is read there.
#[cfg(test)]
pub fn check_graph_counting_fn_bodies(graph: &ModuleGraph) -> usize {
    crate::on_frontend_stack_scoped(move || {
        let mut c = Checker::new();
        c.run_graph_pass(graph, false);
        c.fn_body_checks
    })
}

/// [`check`], plus the number of `index_value` entries the pass made (TICKET-253;
/// `Checker::index_value_checks`): one per subscript when each index is typed once.
#[cfg(test)]
pub fn check_counting_index_values(
    module: &crate::ast::Module,
) -> (Result<(), Vec<CheckError>>, usize) {
    let (res, _, _, indexes) = check_diags_with(module, true);
    (res, indexes)
}

/// [`check_diags`] with the nested-fn return memo off (`Checker::ret_memo`), for the twin comparison
/// in `the_nested_fn_ret_memo_is_invisible_to_diagnostics_and_tables`.
#[cfg(test)]
pub fn check_diags_no_memo(
    module: &crate::ast::Module,
) -> (Result<(), Vec<CheckError>>, Vec<CheckError>) {
    let (res, warnings, _, _) = check_diags_with(module, false);
    (res, warnings)
}

impl Checker {
    /// THE end of a check: hand out the errors and warnings, each reported once. A position can be
    /// checked twice by design — a field default is typed at its declaration and again where a
    /// constructor call inlines it (DEC-094) — and the same diagnostic at the same span is then one
    /// finding, not two (W19 S3: `n: int = Self.mk()` printed `unknown name 'Self'` twice).
    fn finish_diags(&mut self) -> (Result<(), Vec<CheckError>>, Vec<CheckError>) {
        fn once(v: Vec<CheckError>) -> Vec<CheckError> {
            // ponytail: O(n^2) over the diagnostics of one run; a run reports a handful.
            let mut out: Vec<CheckError> = Vec::with_capacity(v.len());
            for e in v {
                if !out.contains(&e) {
                    out.push(e);
                }
            }
            out
        }
        let warnings = once(std::mem::take(&mut self.warnings));
        let errors = once(std::mem::take(&mut self.errors));
        let res = if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        };
        (res, warnings)
    }
}

#[cfg(test)]
pub(super) fn check_diags_with(
    module: &crate::ast::Module,
    memo_enabled: bool,
) -> (Result<(), Vec<CheckError>>, Vec<CheckError>, usize, usize) {
    crate::on_frontend_stack_scoped(move || {
        let mut c = Checker::new();
        c.memo_enabled = memo_enabled;
        // Single-module path (no graph): the always-linked std/prelude.chz was never hoisted, so seed
        // the eight migrated universe-builtin signatures from it directly (graph path hoists them
        // normally).
        c.seed_native_prelude_sigs();
        c.check_module(&module.stmts, None, &[]);
        c.check_provider_cycles();
        let (res, warnings) = c.finish_diags();
        (res, warnings, c.fn_body_checks, c.index_value_checks)
    })
}

/// Entry point for a multi-file program: type-check every module in the graph (dependencies
/// before dependents), accumulating all errors across all modules (Go-style). User types are
/// MODULE-SCOPED: a type declared in one module is private to it and visible elsewhere only via
/// import. The same type name may appear in several modules.
pub fn check_graph(graph: &ModuleGraph) -> Result<(), Vec<CheckError>> {
    check_graph_with_entry(graph, None)
}

/// [`check_graph`] for the manifest-entrypoint run (`chezzi run` with no file): `entry_fn` is the
/// `[project] entrypoint = "mod:fn"` function name, which the runtime invokes BY NAME with no
/// arguments. That is CLI state the checker cannot discover, and the only thing it changes is one
/// extra rejection (M24: such a function may not take a hidden type witness — see the entry-fn arm
/// in `run_graph_pass`). `None` ⇒ byte-identical to `check_graph`.
///
/// Runs on [`crate::on_frontend_stack_scoped`]'s dedicated stack — see that fn's doc comment. This is
/// the PRODUCTION entry point (`check_graph` delegates here), so this is where the CLI, LSP
/// diagnostics/hover, and `test_runner` all land; wrapping HERE rather than at each of those callers
/// makes the 1 GiB stack a structural guarantee every entry point gets by construction, not a
/// convention each future caller has to remember. Nesting (a caller that also wraps, e.g. the CLI) is
/// harmless — one extra thread.
pub fn check_graph_with_entry(
    graph: &ModuleGraph,
    entry_fn: Option<&str>,
) -> Result<(), Vec<CheckError>> {
    check_graph_diags(graph, entry_fn).0
}

/// [`check_graph_with_entry`] plus the non-fatal warnings the pass produced. Warnings never affect
/// the `Result` — a program with warnings and no errors returns `Ok(())`. The plain entry points
/// keep their exact signatures because ~113 test call sites depend on them; a consumer that wants to
/// SHOW warnings (the CLI, the LSP, the test runner) calls this one instead.
pub fn check_graph_diags(
    graph: &ModuleGraph,
    entry_fn: Option<&str>,
) -> (Result<(), Vec<CheckError>>, Vec<CheckError>) {
    crate::on_frontend_stack_scoped(move || {
        let mut c = Checker::new();
        c.entry_fn = entry_fn.map(str::to_string);
        c.run_graph_pass(graph, false);
        c.finish_diags()
    })
}

/// Classification of the symbol a hover landed on, returned alongside the inferred type display so
/// editor tooling can label it (`local`/`param`/`fn`/`field`/`struct`/literal). Secondary metadata —
/// the type string is the load-bearing payload. `Param` is PRODUCED at param-DECL hover sites (fn /
/// method / closure signature); a param's body-USE still reports `Local` (different span). `Other`
/// covers a leaf the classifier can't bucket.
// `Struct` is part of the public hover-kind contract but not yet produced (a struct-name hover
// currently resolves through other paths) — these variants are also constructed by editor consumers,
// not this crate's default `chezzi` bin (which compiles `checker` privately and reaches hover only
// via the lib's `editor`), so allow the bin-only dead-code lint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum HoverKind {
    Local,
    Param,
    Func,
    Field,
    Struct,
    /// A TYPE token inside an annotation (`x: Id`, a param/return/field type, a `let` annotation):
    /// the hovered name resolves through `resolve_type` and the display is the resolved `Ty`.
    Type,
    Literal,
    Other,
}

/// Editor-tooling introspection (LSP hover): re-run the SAME deps-first checking pass as
/// [`check_graph`] over `graph`, but with a single-position PROBE armed on the ENTRY module. Returns
/// `Some((type_display, kind))` for the leaf expression / binding / field-name whose anchor position
/// is the 1-based `(line, col)` token start — and ONLY when the whole program type-checks (mirrors
/// the "no type when the program doesn't check" contract). `None` otherwise. Behavior-preserving:
/// the probe only records a type already computed by the normal pass; it changes no checker decision.
///
/// Consumed by the lib's `editor::hover` (and through it the `chezzi-lsp` server); the default
/// `chezzi` bin compiles `checker` privately without reaching it, so allow that target's dead-code lint.
///
/// Runs on [`crate::on_frontend_stack_scoped`]'s dedicated stack, like every other fn here that drives
/// `run_graph_pass`. This one was left unwrapped and relied on `editor::hover` wrapping its caller
/// instead — a convention, not a guarantee, and the exact hole that already bit this repo once when
/// hover ran the whole checker on a ~2 MiB LSP tokio worker. Wrapping HERE is what makes the doc block
/// on `EQ_BOUNDS_MAX_NODES` ("every caller is on 1 GiB by construction") true rather than aspirational.
#[allow(dead_code)]
pub fn hover_type(
    graph: &ModuleGraph,
    line: u32,
    col: u32,
) -> Option<(String, HoverKind, Option<String>)> {
    crate::on_frontend_stack_scoped(move || {
        let mut c = Checker::new();
        c.hover_probe = Some((line, col));
        c.hover_entry = Some(graph.entry.clone());
        c.run_graph_pass(graph, false);
        if !c.errors.is_empty() {
            return None;
        }
        c.hover_result
            .take()
            .map(|(ty, kind, doc)| (ty.to_string(), kind, doc))
    })
}

/// FFI ROOT FIX (fix4): resolve the fully-resolved, width-bearing C signature of every `extern` fn
/// in the graph, each in its DEFINING module's import/alias scope — the SINGLE resolver both backends
/// consume so every alias spelling (local chain, named-import hop, qualified hop, mixed) resolves
/// collision-proof by construction. Runs the SAME module-scoped pass as [`check_graph`] (deps-first,
/// `begin_module` / `bind_import` / `module_sigs`) but harvests the extern table and ignores type
/// errors (the error gate is `check_graph`, run separately by the CLI). The returned table is keyed
/// by `(graph module index, fn name)`.
///
/// Runs on [`crate::on_frontend_stack_scoped`]'s dedicated stack — see that fn's doc comment, and
/// `EQ_BOUNDS_MAX_IN_PROGRESS`'s doc in `checker::eq` for why this one matters: `chezzi run`/
/// `chezzi test` call this from `compiler::compile_graph`, itself called from INSIDE the VM's own
/// `VM_STACK_BYTES` thread (384 MiB) — a smaller stack than the checker's other two entry points get,
/// so the `Eq` walk this pass runs (`run_graph_pass`) needs the same 1 GiB floor they do (W7-55
/// Important-3 follow-up).
pub fn resolve_extern_signatures(graph: &ModuleGraph) -> ExternTable {
    crate::on_frontend_stack_scoped(move || {
        let mut c = Checker::new();
        c.run_graph_pass(graph, true);
        std::mem::take(&mut c.extern_sigs)
    })
}

/// Resolve extern C signatures for a SINGLE-FILE (standalone, source-string) program, going through
/// the EXACT SAME [`resolve_extern_signatures`] pass as the multi-file CLI — so there is exactly ONE
/// extern-type resolver in the whole codebase and the backends never re-resolve. Wraps `stmts` in a
/// synthetic one-module [`ModuleGraph`]: id `<main>` (so `module_keys` yields `<main>`, matching the
/// backends' `STANDALONE_MODULE_KEY` struct-identity key), no imports, no native. Width names
/// (`int32`) resolve as reserved leaves; local aliases/structs resolve from the parsed stmts; no
/// qualified / named-import forms exist in single-file source, so nothing else is needed.
///
/// Test-only: the single-file standalone compile/run path (`compile_module_standalone`)
/// is `#[cfg(test)]`. The production CLI — single- AND multi-file — always goes
/// through `build_graph` → `resolve_extern_signatures`.
#[cfg(test)]
pub fn resolve_extern_signatures_standalone(stmts: &[Stmt]) -> ExternTable {
    let id = crate::resolver::ModuleId(std::path::PathBuf::from("<main>"));
    let graph = ModuleGraph {
        entry: id.clone(),
        range_used: true,
        modules: vec![crate::resolver::LoadedModule {
            id,
            dotted: Vec::new(),
            ast: crate::ast::Module {
                stmts: stmts.to_vec(),
            },
            file: 0,
            imports: Vec::new(),
            native: None,
        }],
    };
    resolve_extern_signatures(&graph)
}

/// Swift-style keyword arguments through a function VALUE: resolve every value call that carries
/// labels (`g(name="Bob")`) into a positional slot PERMUTATION, keyed `(graph module index, call
/// span)`. Runs the SAME deps-first checking pass as [`check_graph`] (full inference — the callee's
/// labelled `Ty::Func` must be known), but harvests the keyword table and ignores type errors (the
/// error gate is `check_graph`, run separately). Both backends consume the returned table to lower a
/// value+keyword call to a plain positional `Op::Call`, so the runtime ABI stays positional.
///
/// M24 rides the SAME pass and the SAME `harvest_keywords` licence (one extra deps-first check, not
/// two): the [`WitnessTable`] the static-witness lowering consumes comes back alongside.
///
/// W7-43 rides it too, as the third element: the [`CarrierTable`] telling the compiler which
/// lowering each `?.` carrier takes. Unlike the other two it is recorded UNCONDITIONALLY (not gated
/// on `harvest_keywords`) — a gate would be a second way for this pass and [`check_graph`] to
/// disagree about the same program.
/// TICKET-250 rides it as the last two elements: the [`HookTable`] and the [`HookCallTable`]. A
/// `.eq(x)` call typed through the `Eq` requirement is a `Hook::Eq` entry of the second, and the
/// compiler lowers it to `==`.
/// W7-49 rides it as the last element: the key CONFLICTS this pass refused to overwrite. This pass
/// discards its type errors, so a conflict cannot travel as one — the compiler turns the first into a
/// hard `CompileError`. See [`record_call_table_entry`].
/// Runs on [`crate::on_frontend_stack_scoped`]'s dedicated stack — same reason as
/// [`resolve_extern_signatures`]: this is the other checker pass `compiler::compile_graph` runs from
/// inside the VM's 384 MiB thread (W7-55 Important-3 follow-up).
pub fn resolve_call_tables(graph: &ModuleGraph) -> CallTables {
    resolve_call_tables_with(graph, true)
}

/// [`resolve_call_tables`] with the nested-fn return memo (`Checker::ret_memo`) switchable, so a test
/// can run the SAME graph both ways. Production always passes `true`.
pub(super) fn resolve_call_tables_with(graph: &ModuleGraph, memo_enabled: bool) -> CallTables {
    compile_tables_pass(graph, memo_enabled, false).1
}

/// TICKET-255 — the one checker pass `compile_graph` runs: the extern C signatures and every call table
/// together, where the compiler once ran two passes over the same graph. Production always passes
/// `true` for both flags through this entry.
pub fn resolve_compile_tables(graph: &ModuleGraph) -> (ExternTable, CallTables) {
    compile_tables_pass(graph, true, true)
}

pub(super) fn compile_tables_pass(
    graph: &ModuleGraph,
    memo_enabled: bool,
    harvest_externs: bool,
) -> (ExternTable, CallTables) {
    crate::on_frontend_stack_scoped(move || {
        let mut c = Checker::new();
        c.memo_enabled = memo_enabled;
        c.harvest_keywords = true;
        c.run_graph_pass(graph, harvest_externs);
        let externs = std::mem::take(&mut c.extern_sigs);
        let tables = (
            std::mem::take(&mut c.call_plans),
            std::mem::take(&mut c.witnesses),
            std::mem::take(&mut c.carriers),
            std::mem::take(&mut c.sum_seeds),
            std::mem::take(&mut c.ret_coerce),
            std::mem::take(&mut c.table_conflicts),
            std::mem::take(&mut c.crossings),
            std::mem::take(&mut c.resolutions),
            std::mem::take(&mut c.fall_off),
            std::mem::take(&mut c.gen_crossings),
            std::mem::take(&mut c.wraps),
            std::mem::take(&mut c.hook_table),
            std::mem::take(&mut c.hook_calls),
            std::mem::take(&mut c.field_widths),
        );
        (externs, tables)
    })
}
